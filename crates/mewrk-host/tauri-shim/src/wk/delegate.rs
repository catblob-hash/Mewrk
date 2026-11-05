//! What WebKit calls back on: navigation policy and load events, script dialogs and file choosers,
//! the messages `hooks.js` posts, and the folder behind a local file's preview.
//!
//! Nothing in here may panic: a panic across these Objective-C frames aborts the process.

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_foundation::{
    ns_string, NSArray, NSData, NSDictionary, NSError, NSHTTPURLResponse, NSString, NSURL, NSURLResponse, NSURLErrorCancelled,
    NSURLErrorDomain, NSURLErrorFailingURLErrorKey,
};
use objc2_web_kit::{
    WKFrameInfo, WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationResponse,
    WKNavigationResponsePolicy, WKOpenPanelParameters, WKScriptMessage, WKScriptMessageHandler, WKUIDelegate, WKURLSchemeHandler,
    WKURLSchemeTask, WKUserContentController, WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};
use serde_json::{json, Value};

use super::{current_url, lock, post_main, HeldDialog, PageDialog, PageLoadEvent, PageShared, NATIVE};

/// `WebKitErrorFrameLoadInterruptedByPolicyChange`: the response policy turned a load down.
const FRAME_LOAD_INTERRUPTED: isize = 102;
/// The Cmd key, by `NSEventModifierFlags`, and the middle button: both open a new tab in a browser.
const COMMAND_FLAG: usize = 1 << 20;
const MIDDLE_BUTTON: isize = 2;
/// What a message from `hooks.js` may be. The largest real one is a long URL.
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
/// The only events a page may report about itself.
const PAGE_REPORTED_EVENTS: [&str; 6] = [
    "Network.requestWillBeSent",
    "Network.responseReceived",
    "Network.loadingFinished",
    "Network.loadingFailed",
    "Page.domContentEventFired",
    "Overlay.inspectNodeRequested",
];
const PICKER_CANCEL_EVENT: &str = "Overlay.inspectModeCanceled";

fn ns_url_string(url: Option<Retained<NSURL>>) -> String {
    url.and_then(|url| url.absoluteString()).map_or_else(String::new, |url| url.to_string())
}

/// `<`, `>` and `&` for a message put into a failure page.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The document a failed main-frame load leaves behind, so the model has something to read and the
/// person something to see: Chromium's own error page, in effect. The app's language isn't known to
/// the page engine, so it says both.
pub(super) fn failure_page(url: &str, error: &str) -> String {
    let host = url::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_else(|| url.to_owned());
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{host}</title><meta name="color-scheme" content="light dark">
<style>body{{font:14px -apple-system,sans-serif;margin:0;display:flex;align-items:center;justify-content:center;height:100vh}}
main{{max-width:30rem;padding:1.5rem}}h1{{font-size:1.1rem;font-weight:600;margin:0 0 .5rem}}p{{margin:.25rem 0;opacity:.7;overflow-wrap:anywhere}}</style></head>
<body><main><h1>无法打开此页面 · This page can't be opened</h1><p>{url}</p><p>{error}</p></main></body></html>"#,
        host = escape_html(&host),
        url = escape_html(url),
        error = escape_html(error),
    )
}

pub(super) struct Ivars {
    shared: Arc<PageShared>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MewrkPageDelegate"]
    #[ivars = Ivars]
    pub(super) struct PageDelegate;

    unsafe impl NSObjectProtocol for PageDelegate {}

    unsafe impl WKNavigationDelegate for PageDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_action(&self, _web: &WKWebView, action: &WKNavigationAction, decide: &DynBlock<dyn Fn(WKNavigationActionPolicy)>) {
            decide.call((self.action_policy(action),));
        }

        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn decide_response(&self, _web: &WKWebView, response: &WKNavigationResponse, decide: &DynBlock<dyn Fn(WKNavigationResponsePolicy)>) {
            decide.call((self.response_policy(response),));
        }

        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.navigation_started(web);
        }

        #[unsafe(method(webView:didCommitNavigation:))]
        fn did_commit(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.navigation_committed(web);
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web: &WKWebView, _navigation: Option<&WKNavigation>) {
            self.navigation_finished(web);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, web: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.navigation_failed(web, error);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, web: &WKWebView, _navigation: Option<&WKNavigation>, error: &NSError) {
            self.navigation_failed(web, error);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn process_terminated(&self, _web: &WKWebView) {
            self.ivars().shared.crashed();
        }
    }

    unsafe impl WKUIDelegate for PageDelegate {
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(
            &self,
            _web: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            // SAFETY: property reads on the main thread.
            let url = ns_url_string(unsafe { action.request() }.URL());
            let shared = &self.ivars().shared;
            shared.report_new_window(&shared.public_url(&url));
            std::ptr::null_mut()
        }

        #[unsafe(method(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn alert(&self, _web: &WKWebView, message: &NSString, frame: &WKFrameInfo, done: &DynBlock<dyn Fn()>) {
            self.dialog("alert", message, None, frame, HeldDialog::Alert(done.copy()));
        }

        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn confirm(&self, _web: &WKWebView, message: &NSString, frame: &WKFrameInfo, done: &DynBlock<dyn Fn(Bool)>) {
            self.dialog("confirm", message, None, frame, HeldDialog::Confirm(done.copy()));
        }

        #[unsafe(method(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:))]
        fn prompt(&self, _web: &WKWebView, message: &NSString, default_text: Option<&NSString>, frame: &WKFrameInfo, done: &DynBlock<dyn Fn(*mut NSString)>) {
            self.dialog("prompt", message, default_text, frame, HeldDialog::Prompt(done.copy()));
        }

        #[unsafe(method(webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:))]
        fn open_panel(&self, _web: &WKWebView, parameters: &WKOpenPanelParameters, _frame: &WKFrameInfo, done: &DynBlock<dyn Fn(*mut NSArray<NSURL>)>) {
            self.file_chooser(parameters, done.copy());
        }
    }

    unsafe impl WKScriptMessageHandler for PageDelegate {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn script_message(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            self.page_message(message);
        }
    }

    unsafe impl WKURLSchemeHandler for PageDelegate {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start_task(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            self.scheme_task_started(task);
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop_task(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            let key = task as *const _ as *const c_void as usize;
            let label = self.ivars().shared.label.clone();
            NATIVE.with(|native| {
                if let Some(page) = native.borrow_mut().get_mut(&label) {
                    page.scheme_tasks.remove(&key);
                }
            });
        }
    }

    impl PageDelegate {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn observe_value(&self, _key_path: *mut NSString, object: *mut AnyObject, _change: *mut AnyObject, _context: *mut c_void) {
            // SAFETY: the only observation registered is the view's `title`, on the main thread.
            let Some(web) = (unsafe { object.as_ref() }).and_then(|object| object.downcast_ref::<WKWebView>()) else {
                return;
            };
            let title = unsafe { web.title() }.map_or_else(String::new, |title| title.to_string());
            self.ivars().shared.title_changed(title);
        }
    }
);

impl PageDelegate {
    pub(super) fn new(shared: Arc<PageShared>, mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { shared });
        // SAFETY: `NSObject`'s designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// Whether the main frame may go where it was just asked to. Every path a load can be turned down
    /// by goes through here, so nothing the host forbids runs a request.
    fn action_policy(&self, action: &WKNavigationAction) -> WKNavigationActionPolicy {
        let shared = &self.ivars().shared;
        // SAFETY: property reads on the main thread.
        let request = unsafe { action.request() };
        let raw = ns_url_string(request.URL());
        let url = shared.public_url(&raw);
        let Some(target) = (unsafe { action.targetFrame() }) else {
            // No frame to load in: `target=_blank` and friends.
            shared.report_new_window(&url);
            return WKNavigationActionPolicy::Cancel;
        };
        // SAFETY: as above.
        if !unsafe { target.isMainFrame() } {
            return WKNavigationActionPolicy::Allow;
        }
        // SAFETY: as above.
        if unsafe { action.shouldPerformDownload() } {
            // Downloads are denied for every page.
            let _ = shared.allows_download();
            return WKNavigationActionPolicy::Cancel;
        }
        // SAFETY: as above.
        let (button, flags) = unsafe { (action.buttonNumber(), action.modifierFlags().0) };
        if button == MIDDLE_BUTTON || (flags & COMMAND_FLAG != 0 && button == 0) {
            shared.report_new_window(&url);
            return WKNavigationActionPolicy::Cancel;
        }
        if !shared.allows_navigation(&url) {
            return WKNavigationActionPolicy::Cancel;
        }
        // A link to a mapped host: the policy has said yes, but only the scheme handler can serve it.
        if let Ok(parsed) = url::Url::parse(&raw) {
            let engine = shared.engine_url(&parsed);
            if engine != raw {
                let label = shared.label.clone();
                post_main(move || {
                    if let Some(web) = super::native_web(&label) {
                        let _ = super::load(&web, &engine);
                    }
                });
                return WKNavigationActionPolicy::Cancel;
            }
        }
        // SAFETY: as above.
        let method = request.HTTPMethod().map_or_else(|| "GET".to_owned(), |method| method.to_string());
        *lock(&shared.navigation_method) = method;
        WKNavigationActionPolicy::Allow
    }

    fn response_policy(&self, response: &WKNavigationResponse) -> WKNavigationResponsePolicy {
        let shared = &self.ivars().shared;
        // SAFETY: property reads on the main thread.
        if !unsafe { response.isForMainFrame() } {
            return WKNavigationResponsePolicy::Allow;
        }
        // SAFETY: as above.
        let inner: Retained<NSURLResponse> = unsafe { response.response() };
        let request_id = lock(&shared.document_request).clone();
        if let Some(request_id) = request_id {
            let (status, status_text) = match inner.downcast_ref::<NSHTTPURLResponse>() {
                Some(http) => {
                    let code = http.statusCode();
                    (code, NSHTTPURLResponse::localizedStringForStatusCode(code).to_string())
                }
                None => (200, String::new()),
            };
            let url = shared.public_url(&ns_url_string(inner.URL()));
            shared.event(
                "Network.responseReceived",
                json!({"requestId": request_id, "type": "Document", "frameId": "wk-main", "response": {"url": url, "status": status, "statusText": status_text}}),
            );
        }
        // SAFETY: as above.
        if unsafe { response.canShowMIMEType() } {
            return WKNavigationResponsePolicy::Allow;
        }
        // Something only a download could show. Every download is refused; the handler is still asked,
        // because it is what checks the page's authority.
        if shared.allows_download() {
            return WKNavigationResponsePolicy::Allow;
        }
        WKNavigationResponsePolicy::Cancel
    }

    fn navigation_started(&self, web: &WKWebView) {
        let shared = &self.ivars().shared;
        let url = current_url(web);
        if !url.starts_with("about:") {
            let number = shared.next_document.fetch_add(1, Ordering::SeqCst);
            let request_id = format!("doc.{number}");
            let method = lock(&shared.navigation_method).clone();
            shared.event(
                "Network.requestWillBeSent",
                json!({"requestId": request_id, "type": "Document", "frameId": "wk-main", "request": {"url": shared.public_url(&url), "method": method}}),
            );
            *lock(&shared.document_request) = Some(request_id);
        }
        shared.page_load(&url, PageLoadEvent::Started);
    }

    fn navigation_committed(&self, web: &WKWebView) {
        let shared = &self.ivars().shared;
        let url = shared.public_url(&current_url(web));
        // Whatever the previous document had going went with it.
        let abandoned: HashSet<String> = std::mem::take(&mut *lock(&shared.open_requests));
        for request_id in abandoned {
            shared.event("Network.loadingFailed", json!({"requestId": request_id, "errorText": "net::ERR_ABORTED", "canceled": true}));
        }
        shared.event("Page.frameNavigated", json!({"frame": {"id": "wk-main", "loaderId": "wk", "url": url, "securityOrigin": "", "mimeType": ""}}));
        // A file chooser belongs to the document that opened it.
        let label = shared.label.clone();
        let chooser = NATIVE.with(|native| native.borrow_mut().get_mut(&label).and_then(|page| page.file_chooser.take()));
        if let Some(chooser) = chooser {
            chooser.call((std::ptr::null_mut(),));
        }
        super::cdp::document_committed(shared, web);
    }

    fn navigation_finished(&self, web: &WKWebView) {
        let shared = &self.ivars().shared;
        let url = current_url(web);
        *lock(&shared.showing_failure_for) = None;
        if let Some(request_id) = lock(&shared.document_request).take() {
            shared.event("Network.loadingFinished", json!({"requestId": request_id}));
        }
        shared.event("Page.loadEventFired", json!({"timestamp": 0}));
        shared.page_load(&url, PageLoadEvent::Finished);
    }

    fn navigation_failed(&self, web: &WKWebView, error: &NSError) {
        let shared = &self.ivars().shared;
        let code = error.code();
        let domain = error.domain().to_string();
        // A newer load took over, or the page was told to stop: not a failure, and whoever did it
        // has already said what the state is.
        // SAFETY: an extern static of Foundation's.
        let cancelled = domain == unsafe { NSURLErrorDomain }.to_string() && code == NSURLErrorCancelled;
        if cancelled {
            return;
        }
        let failed_url = failing_url(error).unwrap_or_else(|| current_url(web));
        let description = error.localizedDescription().to_string();
        if let Some(request_id) = lock(&shared.document_request).take() {
            shared.event("Network.loadingFailed", json!({"requestId": request_id, "errorText": description, "type": "Document"}));
        }
        // The response policy said no, and the page stays where it was.
        if code == FRAME_LOAD_INTERRUPTED {
            shared.page_load(&current_url(web), PageLoadEvent::Finished);
            return;
        }
        // A failed load finishes on the address that failed, as it does on WebView2.
        let already_shown = lock(&shared.showing_failure_for).replace(failed_url.clone()).is_some();
        if !already_shown && self.show_failure_page(web, &failed_url, &description) {
            return;
        }
        *lock(&shared.showing_failure_for) = None;
        shared.page_load(&failed_url, PageLoadEvent::Finished);
    }

    /// Chromium shows an error document at the failed address. `_loadAlternateHTMLString` is the SPI
    /// Safari does the same with, and the one that doesn't add a history entry for the failure.
    fn show_failure_page(&self, web: &WKWebView, url: &str, description: &str) -> bool {
        let selector = sel!(_loadAlternateHTMLString:baseURL:forUnreachableURL:);
        let Some(base) = NSURL::URLWithString(&NSString::from_str(url)) else {
            return false;
        };
        if !web.respondsToSelector(selector) {
            return false;
        }
        let html = NSString::from_str(&failure_page(url, description));
        // SAFETY: guarded above; on the main thread with objects that live through the call.
        unsafe {
            let _: () = msg_send![web, _loadAlternateHTMLString: &*html, baseURL: &*base, forUnreachableURL: &*base];
        }
        true
    }

    fn dialog(&self, kind: &'static str, message: &NSString, default_text: Option<&NSString>, frame: &WKFrameInfo, held: HeldDialog) {
        // SAFETY: property reads on the main thread.
        let url = ns_url_string(unsafe { frame.request() }.URL());
        let shared = &self.ivars().shared;
        shared.open_dialog(
            PageDialog {
                kind,
                message: message.to_string(),
                default_text: default_text.map_or_else(String::new, |text| text.to_string()),
                url: shared.public_url(&url),
            },
            held,
        );
    }

    /// Nothing is shown to the person: as with `Page.setInterceptFileChooserDialog`, the chooser waits
    /// for `DOM.setFileInputFiles`. Without interception it is a plain cancel; the system's panel would
    /// open over a window the client owns, for a page it knows nothing about.
    fn file_chooser(&self, parameters: &WKOpenPanelParameters, done: RcBlock<dyn Fn(*mut NSArray<NSURL>)>) {
        let shared = &self.ivars().shared;
        if !shared.intercept_file_chooser.load(Ordering::SeqCst) {
            done.call((std::ptr::null_mut(),));
            return;
        }
        // SAFETY: a property read on the main thread.
        let mode = if unsafe { parameters.allowsMultipleSelection() } { "selectMultiple" } else { "selectSingle" };
        let label = shared.label.clone();
        let replaced = NATIVE.with(|native| native.borrow_mut().get_mut(&label).and_then(|page| page.file_chooser.replace(done)));
        if let Some(replaced) = replaced {
            replaced.call((std::ptr::null_mut(),));
        }
        super::cdp::file_chooser_opened(shared, mode);
    }

    fn page_message(&self, message: &WKScriptMessage) {
        let shared = &self.ivars().shared;
        // SAFETY: a property read on the main thread.
        let body = unsafe { message.body() };
        let Some(text) = body.downcast_ref::<NSString>().map(|text| text.to_string()) else {
            return;
        };
        if text.len() > MAX_MESSAGE_BYTES {
            return;
        }
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        if !PAGE_REPORTED_EVENTS.contains(&method) && method != PICKER_CANCEL_EVENT {
            return;
        }
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        if let Some(request_id) = params.get("requestId").and_then(Value::as_str) {
            let mut open = lock(&shared.open_requests);
            match method {
                "Network.requestWillBeSent" => {
                    open.insert(request_id.to_owned());
                }
                "Network.loadingFinished" | "Network.loadingFailed" => {
                    open.remove(request_id);
                }
                _ => {}
            }
        }
        shared.event(method, params);
    }

    fn scheme_task_started(&self, task: &ProtocolObject<dyn WKURLSchemeTask>) {
        let shared = &self.ivars().shared;
        // SAFETY: property reads on the main thread.
        let raw = ns_url_string(unsafe { task.request() }.URL());
        let key = task as *const _ as *const c_void as usize;
        let label = shared.label.clone();
        let retained = task.retain();
        NATIVE.with(|native| {
            if let Some(page) = native.borrow_mut().get_mut(&label) {
                page.scheme_tasks.insert(key, retained);
            }
        });
        let folder = url::Url::parse(&raw).ok().and_then(|url| {
            let host = url.host_str()?.to_owned();
            Some((lock(&shared.virtual_hosts).get(&host).cloned()?, url.path().to_owned()))
        });
        std::thread::spawn(move || {
            let served = folder.and_then(|(folder, path)| {
                let file = resolve_preview_file(&folder, &path)?;
                let bytes = std::fs::read(&file).ok()?;
                Some((bytes, preview_mime_type(&file)))
            });
            post_main(move || finish_scheme_task(&label, key, &raw, served));
        });
    }
}

fn finish_scheme_task(label: &str, key: usize, raw: &str, served: Option<(Vec<u8>, &'static str)>) {
    // Stopped in the meantime: touching it now would raise an exception.
    let task = NATIVE.with(|native| native.borrow_mut().get_mut(label).and_then(|page| page.scheme_tasks.remove(&key)));
    let Some(task) = task else {
        return;
    };
    let Some(url) = NSURL::URLWithString(&NSString::from_str(raw)) else {
        return;
    };
    let (status, mime, bytes) = match served {
        Some((bytes, mime)) => (200, mime, bytes),
        None => (404, "text/plain", b"Not found".to_vec()),
    };
    let headers = NSDictionary::from_retained_objects(
        &[ns_string!("Content-Type"), ns_string!("Content-Length")],
        &[NSString::from_str(mime), NSString::from_str(&bytes.len().to_string())],
    );
    let response = NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(NSHTTPURLResponse::alloc(), &url, status, Some(ns_string!("HTTP/1.1")), Some(&headers));
    let Some(response) = response else {
        return;
    };
    // SAFETY: the task is live (it was still registered) and these are its documented steps in order.
    unsafe {
        task.didReceiveResponse(&response);
        task.didReceiveData(&NSData::with_bytes(&bytes));
        task.didFinish();
    }
}

fn failing_url(error: &NSError) -> Option<String> {
    let info = error.userInfo();
    // SAFETY: an extern static of Foundation's; the value under this key is documented as a URL.
    let key = unsafe { NSURLErrorFailingURLErrorKey };
    let value = info.objectForKey(key)?;
    value.downcast_ref::<NSURL>().and_then(|url| url.absoluteString()).map(|url| url.to_string())
}

/// Serves a file below the mapped folder, or nothing. The path is resolved component by component and
/// re-checked after canonicalisation, so neither `..` nor a symlink can reach outside the folder.
pub(super) fn resolve_preview_file(folder: &Path, url_path: &str) -> Option<PathBuf> {
    let mut path = folder.to_path_buf();
    for segment in url_path.split('/').filter(|segment| !segment.is_empty()) {
        let segment = percent_decode(segment)?;
        if segment == "." || segment == ".." || segment.contains('/') || segment.contains('\0') {
            return None;
        }
        path.push(segment);
    }
    let root = std::fs::canonicalize(folder).ok()?;
    let resolved = std::fs::canonicalize(&path).ok()?;
    (resolved.starts_with(&root) && resolved.is_file()).then_some(resolved)
}

fn percent_decode(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

pub(super) fn preview_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html" | "htm") => "text/html",
        Some("css") => "text/css",
        Some("js" | "mjs") => "text/javascript",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "log") => "text/plain",
        Some("xml") => "application/xml",
        Some("wasm") => "application/wasm",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_files_never_leave_the_mapped_folder() {
        let root = std::env::temp_dir().join(format!("mewrk-wk-preview-{}", std::process::id()));
        let folder = root.join("site");
        std::fs::create_dir_all(folder.join("assets")).unwrap();
        std::fs::write(folder.join("index.html"), "<p>hi</p>").unwrap();
        std::fs::write(folder.join("assets/app.js"), "1").unwrap();
        std::fs::write(root.join("secret.txt"), "no").unwrap();

        assert!(resolve_preview_file(&folder, "/index.html").is_some());
        assert!(resolve_preview_file(&folder, "/assets/app.js").is_some());
        assert!(resolve_preview_file(&folder, "/assets%2Fapp.js").is_none());
        assert!(resolve_preview_file(&folder, "/../secret.txt").is_none());
        assert!(resolve_preview_file(&folder, "/%2e%2e/secret.txt").is_none());
        assert!(resolve_preview_file(&folder, "/assets").is_none());
        std::os::unix::fs::symlink(root.join("secret.txt"), folder.join("link")).unwrap();
        assert!(resolve_preview_file(&folder, "/link").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preview_mime_types_follow_the_extension() {
        assert_eq!(preview_mime_type(Path::new("a/Index.HTML")), "text/html");
        assert_eq!(preview_mime_type(Path::new("a.bin")), "application/octet-stream");
    }

    #[test]
    fn a_failure_page_cannot_be_written_by_the_address_it_failed_at() {
        let page = failure_page("https://a.test/<script>alert(1)</script>", "<b>no</b>");
        assert!(!page.contains("<script>"));
        assert!(!page.contains("<b>no"));
    }
}
