//! The DevTools protocol, as far as `browser.rs` speaks it, answered by WebKit.
//!
//! [`DEVTOOLS_METHODS`] is every method the host calls and how close this gets, so what is lost is
//! written in one place and a test can hold the host to it. A method not listed answers with an
//! error the model can read, immediately: nothing here may leave a caller waiting.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::{
    NSArray, NSDate, NSDictionary, NSError, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires, NSHTTPCookieName, NSHTTPCookiePath,
    NSHTTPCookieSameSiteLax, NSHTTPCookieSameSitePolicy, NSHTTPCookieSameSiteStrict, NSHTTPCookieSecure, NSHTTPCookieValue, NSJSONSerialization,
    NSJSONWritingOptions, NSNull, NSNumber, NSPoint, NSProcessInfo, NSString, NSURL,
};
use objc2_web_kit::{WKContentWorld, WKWebView, WKWebsiteDataStore};
use serde_json::{json, Map, Value};
use zeroize::Zeroize;

use super::snapshot;
use super::{lock, native_store, native_web, post_main_after, reinstall_user_scripts, Emulation, PageShared, NATIVE};

pub(super) type Done = Box<dyn FnOnce(Result<String, String>) + Send>;

/// The fixed id of the only frame the protocol addresses here.
const MAIN_FRAME_ID: &str = "wk-main";
/// What a captured file may weigh on its way into a page's memory, by way of a `data` string.
const MAX_UPLOAD_BYTES: u64 = 64 * 1024 * 1024;
/// A real `mouseUp` takes a moment to come back from the web process. How long to look, and how often.
const TRUSTED_CLICK_POLLS: [Duration; 3] = [Duration::from_millis(40), Duration::from_millis(80), Duration::from_millis(160)];
/// `Browser.getVersion`'s product: only ever read for the mobile user agent the host builds, which
/// claims Android Chrome by design, and a real WebKit version there would be a version Chrome never had.
const REPORTED_PRODUCT: &str = "WebKit-as-Chrome/132.0.0.0";

/// How close a method gets to what Chromium does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// The same answer, from the matching WebKit API or a stateless no-op.
    Exact,
    /// The same shape from another source; the third column says what is lost.
    Approximated,
    /// An error saying so.
    Unavailable,
}

/// Every DevTools method the host calls, then what the host listens for.
pub const DEVTOOLS_METHODS: &[(&str, Support, &str)] = &[
    ("Accessibility.enable", Support::Exact, "no state to switch on"),
    ("Accessibility.disable", Support::Exact, "no state to switch off"),
    ("Accessibility.getFullAXTree", Support::Approximated, "computed in the page from ARIA and HTML, not WebKit's real tree: a heuristic accessible name, no cross-origin frames"),
    ("Browser.getVersion", Support::Approximated, "reports a fixed Chrome-shaped product, used only for the emulated mobile user agent"),
    ("CSS.enable", Support::Exact, "no state to switch on"),
    ("CSS.disable", Support::Exact, "no state to switch off"),
    ("CSS.getComputedStyleForNode", Support::Exact, "getComputedStyle; WebKit's own property list, so names differ where engines do"),
    ("DOM.enable", Support::Exact, "no state to switch on"),
    ("DOM.disable", Support::Exact, "no state to switch off"),
    ("DOM.describeNode", Support::Exact, "from the node itself"),
    ("DOM.getBoxModel", Support::Approximated, "computed from the bounding rect and computed box sizes: a rectangle, so a transformed element's quad is lost"),
    ("DOM.getDocument", Support::Exact, "from the document itself"),
    ("DOM.pushNodesByBackendIdsToFrontend", Support::Exact, "node ids and backend ids are one id here"),
    ("DOM.querySelector", Support::Exact, "querySelector"),
    ("DOM.resolveNode", Support::Exact, "a handle in the page's world"),
    ("DOM.setFileInputFiles", Support::Approximated, "an open chooser takes the files exactly; otherwise the bytes are read here and set through DataTransfer, so a large file costs page memory (64 MiB limit)"),
    ("Emulation.clearDeviceMetricsOverride", Support::Exact, "takes everything below back"),
    ("Emulation.setDeviceMetricsOverride", Support::Approximated, "the CSS viewport is exact (view size plus page zoom), devicePixelRatio and screen are script overrides, a meta viewport is not honoured on the Mac"),
    ("Emulation.setEmitTouchEventsForMouse", Support::Approximated, "plain events named touchstart/touchend: TouchEvent doesn't exist on the Mac"),
    ("Emulation.setEmulatedMedia", Support::Exact, "prefers-color-scheme through the view's appearance, media through mediaType; any other feature is an error"),
    ("Emulation.setTouchEmulationEnabled", Support::Approximated, "maxTouchPoints and ontouchstart as script overrides; matchMedia(pointer) still says mouse"),
    ("Emulation.setUserAgentOverride", Support::Approximated, "customUserAgent is exact; userAgentMetadata is lost, WebKit has no client hints"),
    ("Input.dispatchMouseEvent", Support::Approximated, "real NSEvents to the view, isTrusted and default actions included, with script events as the fallback when the view can't take them; left and right buttons and moves only, and mouseWheel is an error (the host sends only left presses and releases)"),
    ("Network.enable", Support::Exact, "the hooks are always on"),
    ("Network.clearBrowserCache", Support::Exact, "WKWebsiteDataStore"),
    ("Network.clearBrowserCookies", Support::Exact, "WKHTTPCookieStore"),
    ("Network.getAllCookies", Support::Exact, "WKHTTPCookieStore; priority, source scheme and partition aren't kept by WebKit, so they read as defaults"),
    ("Network.getCookies", Support::Exact, "all cookies, filtered by the same domain, path and secure rules"),
    ("Network.getResponseBody", Support::Approximated, "what the page's own fetch and XHR read, up to 1 MiB, newest 64; a document's or a subresource's body is unavailable"),
    ("Network.setCookies", Support::Exact, "WKHTTPCookieStore"),
    ("Overlay.enable", Support::Exact, "no state to switch on"),
    ("Overlay.disable", Support::Exact, "no state to switch off"),
    ("Overlay.hideHighlight", Support::Exact, "hides the picker's box"),
    ("Overlay.setInspectMode", Support::Approximated, "an injected overlay, drawn in the page; picks in a cross-origin frame select the frame"),
    ("Page.enable", Support::Exact, "the delegates are always on"),
    ("Page.bringToFront", Support::Exact, "a page in the window is already at the front"),
    ("Page.captureScreenshot", Support::Approximated, "takeSnapshot is exact for what is in the view, parked or occluded included; beyond the viewport it is drawn by createPDF, where canvas, WebGL and video may not paint"),
    ("Page.getFrameTree", Support::Approximated, "the main frame only: WebKit doesn't enumerate frames"),
    ("Page.getLayoutMetrics", Support::Exact, "visualViewport and the document's size"),
    ("Page.getNavigationHistory", Support::Exact, "backForwardList"),
    ("Page.setInterceptFileChooserDialog", Support::Approximated, "the chooser is held by the UI delegate; which input opened it comes from the click, when a click did"),
    ("Runtime.callFunctionOn", Support::Exact, "in the page's world"),
    ("Runtime.evaluate", Support::Exact, "callAsyncJavaScript, and evaluateJavaScript for what only parses as statements, where a promise isn't awaited"),
    ("Runtime.releaseObject", Support::Exact, "drops the page-side handle"),
    ("Storage.clearDataForOrigin", Support::Approximated, "clears the tab's whole store, which holds at least that origin"),
    ("Storage.getCookies", Support::Exact, "as Network.getAllCookies"),
    ("Storage.setCookies", Support::Exact, "as Network.setCookies"),
    ("Network.requestWillBeSent", Support::Approximated, "documents from the navigation delegate, fetch and XHR from hooks, subresources only once they finished; WebSockets and media ranges aren't seen"),
    ("Network.responseReceived", Support::Approximated, "as above; a subresource's status needs a WebKit that reports responseStatus, else it reads 200"),
    ("Network.loadingFinished", Support::Approximated, "as above"),
    ("Network.loadingFailed", Support::Approximated, "as above, with script's error text rather than net:: codes"),
    ("Page.domContentEventFired", Support::Exact, "DOMContentLoaded, reported by the page"),
    ("Page.loadEventFired", Support::Exact, "didFinishNavigation"),
    ("Page.frameNavigated", Support::Exact, "didCommitNavigation, main frame only"),
    ("Page.fileChooserOpened", Support::Approximated, "as Page.setInterceptFileChooserDialog"),
    ("Overlay.inspectNodeRequested", Support::Approximated, "as Overlay.setInspectMode"),
    ("Overlay.inspectModeCanceled", Support::Approximated, "as Overlay.setInspectMode: Escape, in the page"),
];

fn ok(done: Done, value: Value) {
    done(Ok(value.to_string()))
}

fn page_world(mtm: MainThreadMarker) -> Retained<WKContentWorld> {
    // SAFETY: a constructor on the main thread.
    unsafe { WKContentWorld::pageWorld(mtm) }
}

fn ns_error_text(error: &NSError) -> String {
    let info = error.userInfo();
    let detail = info.objectForKey(&NSString::from_str("WKJavaScriptExceptionMessage")).and_then(|message| message.downcast::<NSString>().ok());
    detail.map_or_else(|| error.localizedDescription().to_string(), |message| message.to_string())
}

/// Runs `body` as an async function in the page's world, and hands back the string it returned.
///
/// Dialogs block this: WebKit queues the call until the page is free, and the host's own timeout and
/// modal check decide what to tell its caller meanwhile, as they do on Chromium.
fn call_async(web: &WKWebView, body: &str, done: impl FnOnce(Result<String, String>) + 'static) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err("scripts run on the main thread".into()));
    };
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        // SAFETY: WebKit passes live objects or nil for the duration of the call.
        let (value, error) = unsafe { (value.as_ref(), error.as_ref()) };
        if let Some(error) = error {
            return done(Err(ns_error_text(error)));
        }
        let text = value.and_then(|value| value.downcast_ref::<NSString>()).map_or_else(|| "null".to_owned(), |text| text.to_string());
        done(Ok(text));
    });
    // SAFETY: on the main thread with a live view; WebKit copies the block.
    unsafe {
        web.callAsyncJavaScript_arguments_inFrame_inContentWorld_completionHandler(&NSString::from_str(body), None, None, &page_world(mtm), Some(&handler));
    }
}

/// `window.__mewrkWk.<expression>` as a protocol result: the expression's value, as JSON, or an error.
fn hook(web: &WKWebView, expression: &str, done: Done) {
    hook_value(web, expression, move |result| done(result.map(|value| value.to_string())));
}

/// The same, with the value in hand.
fn hook_value(web: &WKWebView, expression: &str, done: impl FnOnce(Result<Value, String>) + 'static) {
    let body = format!(
        r#"const __wk = window.__mewrkWk;
if (!__wk) throw new Error("the page's bootstrap has not run");
try {{ return JSON.stringify({{ value: __wk.{expression} }}); }} catch (__error) {{ return JSON.stringify({{ error: String(__error && __error.message || __error) }}); }}"#
    );
    call_async(web, &body, move |result| {
        let outcome = result.and_then(|raw| {
            let mut parsed: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            if let Some(error) = parsed.get("error").and_then(Value::as_str) {
                return Err(error.to_owned());
            }
            Ok(parsed.get_mut("value").map_or(Value::Null, Value::take))
        });
        done(outcome);
    });
}

fn js_string(value: &str) -> String {
    Value::from(value).to_string()
}

fn id_param(params: &Value) -> Option<i64> {
    params.get("backendNodeId").or_else(|| params.get("nodeId")).and_then(Value::as_i64)
}

/// `Runtime.evaluate`.
///
/// `callAsyncJavaScript` is what can await a promise, but it takes a function body, so a script that
/// is statements rather than an expression doesn't parse there. Everything that runs inside is caught
/// by the wrapper, so an error reaching the handler means it never started, and the plain evaluation
/// that takes statements (and gives the completion value `Runtime.evaluate` promises) can't run it
/// twice. Going through `eval` instead would be refused by a page's own Content-Security-Policy.
fn evaluate(web: Retained<WKWebView>, params: &Value, done: Done) {
    let expression = params.get("expression").and_then(Value::as_str).unwrap_or_default().to_owned();
    let by_value = params.get("returnByValue").and_then(Value::as_bool).unwrap_or(false);
    let awaits = params.get("awaitPromise").and_then(Value::as_bool).unwrap_or(false);
    let wait = if awaits { "await " } else { "" };
    let body = format!(
        "const __wk = window.__mewrkWk;\ntry {{\nconst __value = {wait}(\n{expression}\n);\nreturn __wk ? __wk.encode(__value, {by_value}) : JSON.stringify({{ result: {{ type: typeof __value, value: __value }} }});\n}} catch (__error) {{\nreturn __wk ? __wk.encodeError(__error) : JSON.stringify({{ exceptionDetails: {{ text: String(__error) }} }});\n}}"
    );
    let retry = web.clone();
    call_async(&web, &body, move |result| match result {
        Ok(text) => done(Ok(text)),
        Err(_) => evaluate_statements(&retry, &expression, by_value, done),
    });
}

/// What `Runtime.evaluate` says about what a completion value turned out to be.
fn evaluate_statements(web: &WKWebView, script: &str, by_value: bool, done: Done) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err("scripts run on the main thread".into()));
    };
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(move |value: *mut AnyObject, error: *mut NSError| {
        let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        // SAFETY: WebKit passes live objects or nil for the duration of the call.
        let (value, error) = unsafe { (value.as_ref(), error.as_ref()) };
        if let Some(error) = error {
            // Unsupported result type (5): it ran, it just can't say what it got.
            let unsupported = error.code() == 5;
            if unsupported {
                return ok(done, json!({"result": {"type": "object", "className": "Object"}}));
            }
            let description = ns_error_text(error);
            return ok(done, json!({"result": {"type": "object", "subtype": "error"}, "exceptionDetails": {"text": "Uncaught", "exception": {"type": "object", "subtype": "error", "description": description}}}));
        }
        let converted = value.map_or(Value::Null, ns_to_json);
        let result = match value {
            None => json!({"type": "undefined"}),
            Some(_) if by_value => json!({"type": json_type(&converted), "value": converted}),
            Some(_) => json!({"type": json_type(&converted), "value": converted}),
        };
        ok(done, json!({ "result": result }));
    });
    // SAFETY: on the main thread with a live view; WebKit copies the block.
    unsafe {
        web.evaluateJavaScript_inFrame_inContentWorld_completionHandler(&NSString::from_str(script), None, &page_world(mtm), Some(&handler));
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        _ => "object",
    }
}

/// What WebKit converted a value to, back to JSON.
fn ns_to_json(value: &AnyObject) -> Value {
    if let Some(text) = value.downcast_ref::<NSString>() {
        return Value::String(text.to_string());
    }
    if value.downcast_ref::<NSNull>().is_some() {
        return Value::Null;
    }
    if let Some(number) = value.downcast_ref::<NSNumber>() {
        // SAFETY: `objCType` is a valid C string for the object's life.
        let kind = unsafe { *number.objCType().as_ptr() } as u8;
        if kind == b'c' || kind == b'B' {
            return Value::Bool(number.as_bool());
        }
        let value = number.as_f64();
        // What `JSON.stringify` would have written: an integer has no fraction.
        if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
            return Value::from(value as i64);
        }
        return serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number);
    }
    // SAFETY: a class method taking any object; it reports what it can't write through `Err`.
    let data = unsafe { NSJSONSerialization::dataWithJSONObject_options_error(value, NSJSONWritingOptions::empty()) };
    data.ok().and_then(|data| serde_json::from_slice(&data.to_vec()).ok()).unwrap_or(Value::Null)
}

/// `Runtime.callFunctionOn`. The function's text is pasted in rather than compiled by `new Function`,
/// for the same Content-Security-Policy reason as `evaluate`.
fn call_function_on(web: &WKWebView, params: &Value, done: Done) {
    let Some(object_id) = params.get("objectId").and_then(Value::as_str) else {
        return done(Err("Runtime.callFunctionOn needs an objectId".into()));
    };
    let declaration = params.get("functionDeclaration").and_then(Value::as_str).unwrap_or_default();
    let by_value = params.get("returnByValue").and_then(Value::as_bool).unwrap_or(false);
    let awaits = params.get("awaitPromise").and_then(Value::as_bool).unwrap_or(false);
    let wait = if awaits { "await " } else { "" };
    let object = js_string(object_id);
    let body = format!(
        "const __wk = window.__mewrkWk;\ntry {{\nconst __target = __wk.object({object});\nconst __function = (\n{declaration}\n);\nconst __value = {wait}__function.call(__target);\nreturn __wk.encode(__value, {by_value});\n}} catch (__error) {{\nreturn __wk.encodeError(__error);\n}}"
    );
    call_async(web, &body, done);
}

// ----- cookies -------------------------------------------------------------------------------------

/// A cookie in `Network.Cookie`'s shape, with the fields WebKit has no notion of at Chromium's
/// defaults: they aren't lost, because no WebKit cookie ever had anything else.
pub(super) fn cookie_json(cookie: &NSHTTPCookie) -> Value {
    let name = cookie.name().to_string();
    let value = cookie.value().to_string();
    let session = cookie.isSessionOnly();
    let expires = if session { -1.0 } else { cookie.expiresDate().map_or(-1.0, |date| date.timeIntervalSince1970()) };
    let mut out = Map::new();
    out.insert("size".to_owned(), json!(name.len() + value.len()));
    out.insert("name".to_owned(), json!(name));
    out.insert("value".to_owned(), json!(value));
    out.insert("domain".to_owned(), json!(cookie.domain().to_string()));
    out.insert("path".to_owned(), json!(cookie.path().to_string()));
    out.insert("expires".to_owned(), json!(expires));
    out.insert("httpOnly".to_owned(), json!(cookie.isHTTPOnly()));
    out.insert("secure".to_owned(), json!(cookie.isSecure()));
    out.insert("session".to_owned(), json!(session));
    out.insert("priority".to_owned(), json!("Medium"));
    out.insert("sourceScheme".to_owned(), json!("Unset"));
    out.insert("sourcePort".to_owned(), json!(-1));
    if let Some(policy) = cookie.sameSitePolicy() {
        let policy = policy.to_string().to_ascii_lowercase();
        if policy == "lax" {
            out.insert("sameSite".to_owned(), json!("Lax"));
        } else if policy == "strict" {
            out.insert("sameSite".to_owned(), json!("Strict"));
        }
    }
    Value::Object(out)
}

/// RFC 6265's matching, for `Network.getCookies`.
pub(super) fn cookie_applies(domain: &str, path: &str, secure: bool, url: &url::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    let domain_matches = match domain.strip_prefix('.') {
        Some(bare) => host == bare || host.ends_with(&format!(".{bare}")),
        None => host == domain,
    };
    let request_path = url.path();
    let path_matches =
        request_path == path || (request_path.starts_with(path) && (path.ends_with('/') || request_path[path.len()..].starts_with('/')));
    domain_matches && path_matches && (!secure || url.scheme() == "https")
}

fn all_cookies(store: &WKWebsiteDataStore, done: impl FnOnce(Vec<Value>) + 'static) {
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(move |cookies: std::ptr::NonNull<NSArray<NSHTTPCookie>>| {
        let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        // SAFETY: WebKit passes a live array for the duration of the call.
        let cookies = unsafe { cookies.as_ref() };
        done(cookies.iter().map(|cookie| cookie_json(&cookie)).collect());
    });
    // SAFETY: on the main thread; WebKit copies the block.
    unsafe { store.httpCookieStore().getAllCookies(&handler) };
}

fn set_cookie_property(names: &mut Vec<&'static NSString>, values: &mut Vec<Retained<AnyObject>>, name: &'static NSString, value: Retained<AnyObject>) {
    names.push(name);
    values.push(value);
}

/// An `NSHTTPCookie` from `Network.CookieParam`. `HttpOnly` has no public property key; it's the key
/// `properties` itself reports for such a cookie, and what restoring one round trips through.
fn cookie_from_param(param: &Value) -> Option<Retained<NSHTTPCookie>> {
    let text = |key: &str| param.get(key).and_then(Value::as_str);
    let url = text("url").and_then(|url| url::Url::parse(url).ok());
    let domain = text("domain").map(str::to_owned).or_else(|| url.as_ref()?.host_str().map(str::to_owned))?;
    let path = text("path").map_or_else(|| "/".to_owned(), str::to_owned);
    let mut names: Vec<&'static NSString> = Vec::new();
    let mut values: Vec<Retained<AnyObject>> = Vec::new();
    let string = |value: &str| -> Retained<AnyObject> { NSString::from_str(value).into() };
    // SAFETY: extern statics of Foundation's, valid for the process.
    unsafe {
        set_cookie_property(&mut names, &mut values, NSHTTPCookieName, string(text("name")?));
        set_cookie_property(&mut names, &mut values, NSHTTPCookieValue, string(text("value").unwrap_or_default()));
        set_cookie_property(&mut names, &mut values, NSHTTPCookieDomain, string(&domain));
        set_cookie_property(&mut names, &mut values, NSHTTPCookiePath, string(&path));
        if param.get("secure").and_then(Value::as_bool) == Some(true) {
            set_cookie_property(&mut names, &mut values, NSHTTPCookieSecure, string("TRUE"));
        }
        if let Some(expires) = param.get("expires").and_then(Value::as_f64).filter(|expires| *expires > 0.0) {
            let date: Retained<AnyObject> = NSDate::dateWithTimeIntervalSince1970(expires).into();
            set_cookie_property(&mut names, &mut values, NSHTTPCookieExpires, date);
        }
        match text("sameSite") {
            Some("Lax") => set_cookie_property(&mut names, &mut values, NSHTTPCookieSameSitePolicy, string(&NSHTTPCookieSameSiteLax.to_string())),
            Some("Strict") => set_cookie_property(&mut names, &mut values, NSHTTPCookieSameSitePolicy, string(&NSHTTPCookieSameSiteStrict.to_string())),
            _ => {}
        }
    }
    let http_only_key = NSString::from_str("HttpOnly");
    let mut key_refs: Vec<&NSString> = names.clone();
    if param.get("httpOnly").and_then(Value::as_bool) == Some(true) {
        key_refs.push(&http_only_key);
        values.push(string("TRUE"));
    }
    let value_refs: Vec<&AnyObject> = values.iter().map(|value| &**value).collect();
    let properties = NSDictionary::from_slices(&key_refs, &value_refs);
    // SAFETY: the dictionary's keys and values are the documented property types; nil means refused.
    unsafe { NSHTTPCookie::cookieWithProperties(&properties) }
}

/// `Storage.setCookies`. The store takes them all at once and answers once, so a failure of the
/// parameters is known here and a failure of the store shows when the caller reads them back, which
/// is what the host's cold-resume verification does.
fn set_cookies(store: &WKWebsiteDataStore, params: &Value, done: Done) {
    let cookies: Vec<Retained<NSHTTPCookie>> = match params.get("cookies").and_then(Value::as_array) {
        Some(params) => {
            let mut cookies = Vec::with_capacity(params.len());
            for param in params {
                match cookie_from_param(param) {
                    Some(cookie) => cookies.push(cookie),
                    None => return done(Err(format!("WebKit refused the cookie {}", param.get("name").and_then(Value::as_str).unwrap_or("?")))),
                }
            }
            cookies
        }
        None => return done(Err("setCookies needs a cookies array".into())),
    };
    let array = NSArray::from_retained_slice(&cookies);
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(move || {
        if let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() {
            ok(done, json!({}));
        }
    });
    // SAFETY: on the main thread; WebKit copies the block.
    unsafe { store.httpCookieStore().setCookies_completionHandler(&array, Some(&handler)) };
}

/// Website data of `types`, or every type there is.
fn clear_data(store: &WKWebsiteDataStore, types: Option<&[&'static NSString]>, done: Done) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err("data is cleared on the main thread".into()));
    };
    // SAFETY: class methods on the main thread.
    let set = match types {
        Some(types) => objc2_foundation::NSSet::from_slice(types),
        None => unsafe { WKWebsiteDataStore::allWebsiteDataTypes(mtm) },
    };
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(move || {
        if let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() {
            ok(done, json!({}));
        }
    });
    // SAFETY: on the main thread; WebKit copies the block.
    unsafe { store.removeDataOfTypes_modifiedSince_completionHandler(&set, &NSDate::distantPast(), &handler) };
}

// ----- input -----------------------------------------------------------------------------------------

static NEXT_EVENT_NUMBER: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(1);

/// Sends `kind` to the view directly, as AppKit would after hit-testing it. Posting to the window
/// server instead would need the app active and the view on screen, and would land on whatever is
/// under that point on the person's screen: a parked page has to take a click and nobody else may.
fn send_mouse_event(web: &WKWebView, kind: NSEventType, button: &str, x: f64, y: f64, clicks: isize) -> bool {
    let Some(window) = web.window() else {
        return false;
    };
    // SAFETY: a property read on the main thread.
    let zoom = unsafe { web.pageZoom() };
    let local = NSPoint::new(x * zoom, if web.isFlipped() { y * zoom } else { web.bounds().size.height - y * zoom });
    let location = web.convertPoint_toView(local, None);
    let number = NEXT_EVENT_NUMBER.fetch_add(1, Ordering::Relaxed);
    let time = NSProcessInfo::processInfo().systemUptime();
    let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,
        location,
        NSEventModifierFlags::empty(),
        time,
        window.windowNumber(),
        None,
        number,
        clicks,
        if kind == NSEventType::LeftMouseUp || kind == NSEventType::RightMouseUp { 0.0 } else { 1.0 },
    );
    let Some(event) = event else {
        return false;
    };
    match (kind, button) {
        (NSEventType::LeftMouseDown, _) => web.mouseDown(&event),
        (NSEventType::LeftMouseUp, _) => web.mouseUp(&event),
        (NSEventType::RightMouseDown, _) => web.rightMouseDown(&event),
        (NSEventType::RightMouseUp, _) => web.rightMouseUp(&event),
        (NSEventType::MouseMoved, _) => web.mouseMoved(&event),
        _ => return false,
    }
    true
}

fn dispatch_mouse(shared: &Arc<PageShared>, web: Retained<WKWebView>, params: &Value, done: Done) {
    let kind = params.get("type").and_then(Value::as_str).unwrap_or_default();
    let x = params.get("x").and_then(Value::as_f64).unwrap_or(0.0);
    let y = params.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    let button = params.get("button").and_then(Value::as_str).unwrap_or("left").to_owned();
    let clicks = params.get("clickCount").and_then(Value::as_i64).unwrap_or(1) as isize;
    let right = button == "right";
    match kind {
        "mouseMoved" => {
            send_mouse_event(&web, NSEventType::MouseMoved, &button, x, y, 0);
            ok(done, json!({}));
        }
        "mousePressed" if button == "left" || right => {
            // The count before the press is what tells the release whether anything arrived.
            let label = shared.label.clone();
            let sent = web.clone();
            hook_value(&web, "trustedMouseUps()", move |count| {
                let before = count.ok().and_then(|value| value.as_u64()).unwrap_or(0);
                MOUSE_BASELINE.with(|baseline| baseline.borrow_mut().insert(label.clone(), before));
                send_mouse_event(&sent, if right { NSEventType::RightMouseDown } else { NSEventType::LeftMouseDown }, &button, x, y, clicks);
                ok(done, json!({}));
            });
        }
        "mouseReleased" if button == "left" || right => {
            send_mouse_event(&web, if right { NSEventType::RightMouseUp } else { NSEventType::LeftMouseUp }, &button, x, y, clicks);
            if right {
                return ok(done, json!({}));
            }
            let before = MOUSE_BASELINE.with(|baseline| baseline.borrow_mut().remove(&shared.label)).unwrap_or(0);
            confirm_click(shared.label.clone(), before, x, y, clicks, 0, done);
        }
        "mouseWheel" => done(Err("Input.dispatchMouseEvent mouseWheel is not available on the WKWebView page engine".into())),
        other => done(Err(format!("Input.dispatchMouseEvent {other} with the {button} button is not available on the WKWebView page engine"))),
    }
}

thread_local! {
    static MOUSE_BASELINE: std::cell::RefCell<std::collections::HashMap<String, u64>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Whether the release reached the page, by the page's own count of trusted events. If it never does
/// the click still has to happen, as script events, or the tool would report a click nothing saw.
fn confirm_click(label: String, before: u64, x: f64, y: f64, clicks: isize, attempt: usize, done: Done) {
    let Some(delay) = TRUSTED_CLICK_POLLS.get(attempt).copied() else {
        let Some(web) = native_web(&label) else {
            return done(Err("the page is closed".into()));
        };
        return hook_value(&web, &format!("syntheticClick({x}, {y}, {clicks})"), move |result| {
            done(result.map(|_| json!({}).to_string()));
        });
    };
    post_main_after(delay, move || {
        let Some(web) = native_web(&label) else {
            return done(Err("the page is closed".into()));
        };
        hook_value(&web, "trustedMouseUps()", move |count| {
            match count {
                Ok(value) if value.as_u64().unwrap_or(0) > before => ok(done, json!({})),
                _ => confirm_click(label, before, x, y, clicks, attempt + 1, done),
            }
        });
    });
}

// ----- files ------------------------------------------------------------------------------------------

/// `DOM.setFileInputFiles`. A chooser the page is waiting in takes the paths themselves.
fn set_file_input_files(shared: &Arc<PageShared>, web: Retained<WKWebView>, params: &Value, done: Done) {
    let files: Vec<String> = params.get("files").and_then(Value::as_array).map_or_else(Vec::new, |files| files.iter().filter_map(Value::as_str).map(str::to_owned).collect());
    let chooser = NATIVE.with(|native| native.borrow_mut().get_mut(&shared.label).and_then(|page| page.file_chooser.take()));
    if let Some(chooser) = chooser {
        let urls: Vec<Retained<NSURL>> = files.iter().map(|file| NSURL::fileURLWithPath(&NSString::from_str(file))).collect();
        let array = NSArray::from_retained_slice(&urls);
        chooser.call((Retained::as_ptr(&array) as *mut NSArray<NSURL>,));
        return ok(done, json!({}));
    }
    let target = match params.get("objectId").and_then(Value::as_str) {
        Some(object_id) => object_id.to_owned(),
        None => match id_param(params) {
            Some(id) => format!("n{id}"),
            None => return done(Err("DOM.setFileInputFiles needs a target".into())),
        },
    };
    // Reading a file can take far longer than a frame. The page then gets it as text, because that is
    // all that crosses into a page's script.
    let label = shared.label.clone();
    let _ = web;
    std::thread::spawn(move || {
        let encoded = encode_files(&files);
        super::post_main(move || {
            let payload = match encoded {
                Ok(payload) => payload,
                Err(error) => return done(Err(error)),
            };
            let Some(web) = native_web(&label) else {
                return done(Err("the page is closed".into()));
            };
            hook(&web, &format!("setFiles({}, {payload})", js_string(&target)), done);
        });
    });
}

fn encode_files(files: &[String]) -> Result<String, String> {
    let mut total = 0;
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let metadata = std::fs::metadata(file).map_err(|error| format!("could not read {file}: {error}"))?;
        total += metadata.len();
        if total > MAX_UPLOAD_BYTES {
            return Err(format!("the files exceed the {} MiB this page engine can hand to a page", MAX_UPLOAD_BYTES / (1024 * 1024)));
        }
        let bytes = std::fs::read(file).map_err(|error| format!("could not read {file}: {error}"))?;
        let name = std::path::Path::new(file).file_name().and_then(|name| name.to_str()).unwrap_or("file");
        let modified = metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |since| since.as_millis() as u64);
        entries.push(json!({
            "name": name,
            "type": super::delegate::preview_mime_type(std::path::Path::new(file)),
            "lastModified": modified,
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        }));
    }
    Ok(Value::Array(entries).to_string())
}

pub(super) fn file_chooser_opened(shared: &Arc<PageShared>, mode: &'static str) {
    let Some(web) = native_web(&shared.label) else {
        return;
    };
    let label = shared.label.clone();
    hook_value(&web, "lastFileInput()", move |input| {
        let found = NATIVE.with(|native| native.borrow().contains_key(&label));
        if !found {
            return;
        }
        let pages = super::pages();
        let Some(shared) = pages.as_ref().and_then(|pages| pages.get(&label).cloned()) else {
            return;
        };
        drop(pages);
        let mut params = json!({"mode": mode, "frameId": MAIN_FRAME_ID});
        if let Some(id) = input.ok().and_then(|value| value.as_i64()) {
            params["backendNodeId"] = json!(id);
        }
        shared.event("Page.fileChooserOpened", params);
    });
}

// ----- emulation ---------------------------------------------------------------------------------------

/// What `hooks.js`' `emulate` is told, which is also what a new document is told through a user script.
fn emulation_config(emulation: &Emulation) -> Value {
    json!({
        "deviceScaleFactor": if emulation.device_scale_factor > 0.0 { Some(emulation.device_scale_factor) } else { None },
        "screenWidth": emulation.viewport.filter(|_| emulation.mobile).map(|(width, _)| width),
        "screenHeight": emulation.viewport.filter(|_| emulation.mobile).map(|(_, height)| height),
        "maxTouchPoints": emulation.max_touch_points,
        "emitTouchEvents": emulation.emit_touch_events,
    })
}

/// What every new document is told beyond the page's own scripts, in the order it has to know it.
pub(super) const LOOPBACK_SCRIPT: &str = "window.__mewrkWk && window.__mewrkWk.setLoopbackAliased(true);";

fn document_scripts(shared: &PageShared) -> Vec<String> {
    let mut scripts = Vec::new();
    if shared.proxied.load(Ordering::SeqCst) {
        scripts.push(LOOPBACK_SCRIPT.to_owned());
    }
    let emulation = *lock(&shared.emulation);
    if emulation != Emulation::default() {
        scripts.push(format!("window.__mewrkWk && window.__mewrkWk.emulate({});", emulation_config(&emulation)));
    }
    scripts
}

/// Applies the emulation state: the view's frame now, the script to this document and the next ones.
fn apply_emulation(shared: &Arc<PageShared>, web: &WKWebView, done: Done) {
    let emulation = *lock(&shared.emulation);
    super::place_view(web, shared);
    let config = emulation_config(&emulation);
    reinstall_user_scripts(&shared.label, &document_scripts(shared));
    hook(web, &format!("emulate({config})"), Box::new(move |result| done(result.map(|_| "{}".to_owned()))));
}

/// The page's network changed: its scripts follow now, and new documents from the start.
pub(super) fn network_changed(shared: &Arc<PageShared>) {
    reinstall_user_scripts(&shared.label, &document_scripts(shared));
    if let Some(web) = native_web(&shared.label) {
        let on = shared.proxied.load(Ordering::SeqCst);
        hook_value(&web, &format!("setLoopbackAliased({on})"), |_| {});
    }
}

fn set_emulated_media(web: &WKWebView, params: &Value, done: Done) {
    if let Some(media) = params.get("media").and_then(Value::as_str) {
        // SAFETY: a property write on the main thread.
        let media = (!media.is_empty()).then(|| NSString::from_str(media));
        unsafe { web.setMediaType(media.as_deref()) };
    }
    let features = params.get("features").and_then(Value::as_array).cloned().unwrap_or_default();
    for feature in &features {
        match (feature.get("name").and_then(Value::as_str), feature.get("value").and_then(Value::as_str)) {
            (Some("prefers-color-scheme"), Some(scheme)) => {
                // SAFETY: extern statics of AppKit's; a property write on the main thread.
                let appearance = unsafe {
                    match scheme {
                        "dark" => NSAppearance::appearanceNamed(NSAppearanceNameDarkAqua),
                        "light" => NSAppearance::appearanceNamed(NSAppearanceNameAqua),
                        _ => None,
                    }
                };
                web.setAppearance(appearance.as_deref());
            }
            (Some(name), _) => return done(Err(format!("Emulation.setEmulatedMedia {name} is not available on the WKWebView page engine"))),
            _ => {}
        }
    }
    ok(done, json!({}));
}

// ----- the table ------------------------------------------------------------------------------------------

/// Arms what has to be put back when a new document comes up.
pub(super) fn document_committed(shared: &Arc<PageShared>, web: &WKWebView) {
    let config = lock(&shared.inspect_config).clone();
    if let Some(config) = config {
        hook_value(web, &format!("pickerArm({config})"), |_| {});
    }
}

/// Answers one method. Runs on the main thread, and may complete later, on the main thread too.
pub(super) fn dispatch(shared: &Arc<PageShared>, method: &str, raw_params: &str, done: Done) {
    let Some(web) = native_web(&shared.label) else {
        return done(Err("the page is closed".into()));
    };
    let mut params: Value = match serde_json::from_str(raw_params) {
        Ok(params) => params,
        Err(error) => return done(Err(format!("{method}: invalid parameters: {error}"))),
    };
    dispatch_parsed(shared, web, method, &params, done);
    // The parameters of `setCookies` carry HttpOnly values.
    wipe(&mut params);
}

fn wipe(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(wipe),
        Value::Object(fields) => fields.values_mut().for_each(wipe),
        _ => {}
    }
}

fn dispatch_parsed(shared: &Arc<PageShared>, web: Retained<WKWebView>, method: &str, params: &Value, done: Done) {
    match method {
        // Domains with nothing to switch: what they would report is always being watched here.
        "Accessibility.enable" | "Accessibility.disable" | "CSS.enable" | "CSS.disable" | "DOM.enable" | "DOM.disable" | "Network.enable" | "Overlay.enable"
        | "Overlay.disable" | "Page.enable" | "Page.bringToFront" => ok(done, json!({})),

        "Runtime.evaluate" => evaluate(web, params, done),
        "Runtime.callFunctionOn" => call_function_on(&web, params, done),
        "Runtime.releaseObject" => {
            let object_id = params.get("objectId").and_then(Value::as_str).unwrap_or_default();
            hook(&web, &format!("release({})", js_string(object_id)), Box::new(move |_| ok(done, json!({}))));
        }

        "Accessibility.getFullAXTree" => hook(&web, "axTree()", done),

        "DOM.getDocument" => hook(&web, "describe(window.__mewrkWk.nodeId(document), 0)", Box::new(move |result| {
            done(result.and_then(|node| {
                let node: Value = serde_json::from_str(&node).map_err(|error| error.to_string())?;
                Ok(json!({ "root": node }).to_string())
            }))
        })),
        "DOM.querySelector" => {
            let Some(id) = id_param(params) else {
                return done(Err("DOM.querySelector needs a nodeId".into()));
            };
            let selector = params.get("selector").and_then(Value::as_str).unwrap_or_default();
            hook(&web, &format!("querySelector({id}, {})", js_string(selector)), done);
        }
        "DOM.describeNode" => {
            let Some(id) = id_param(params) else {
                return done(Err("DOM.describeNode needs a node".into()));
            };
            let depth = params.get("depth").and_then(Value::as_i64).unwrap_or(0);
            hook(&web, &format!("describe({id}, {depth})"), Box::new(move |result| {
                done(result.and_then(|node| {
                    let node: Value = serde_json::from_str(&node).map_err(|error| error.to_string())?;
                    Ok(json!({ "node": node }).to_string())
                }))
            }));
        }
        "DOM.getBoxModel" => {
            let Some(id) = id_param(params) else {
                return done(Err("DOM.getBoxModel needs a node".into()));
            };
            hook(&web, &format!("boxModel({id})"), done);
        }
        "DOM.pushNodesByBackendIdsToFrontend" => {
            let ids = params.get("backendNodeIds").cloned().unwrap_or_else(|| json!([]));
            hook(&web, &format!("pushNodes({ids})"), done);
        }
        "DOM.resolveNode" => {
            let Some(id) = id_param(params) else {
                return done(Err("DOM.resolveNode needs a node".into()));
            };
            let object_id = format!("n{id}");
            hook_value(&web, &format!("nodeById({id}) ? 1 : 0"), move |result| match result {
                Ok(_) => ok(done, json!({"object": {"type": "object", "subtype": "node", "objectId": object_id}})),
                Err(error) => done(Err(error)),
            });
        }
        "DOM.setFileInputFiles" => set_file_input_files(shared, web, params, done),

        "CSS.getComputedStyleForNode" => {
            let Some(id) = id_param(params) else {
                return done(Err("CSS.getComputedStyleForNode needs a node".into()));
            };
            hook(&web, &format!("computedStyle({id})"), done);
        }

        "Overlay.setInspectMode" => {
            let mode = params.get("mode").and_then(Value::as_str).unwrap_or("none");
            if mode == "none" {
                *lock(&shared.inspect_config) = None;
                return hook(&web, "pickerDisarm()", done);
            }
            if mode != "searchForNode" {
                return done(Err(format!("Overlay.setInspectMode {mode} is not available on the WKWebView page engine")));
            }
            let config = params.get("highlightConfig").cloned().unwrap_or_else(|| json!({}));
            *lock(&shared.inspect_config) = Some(config.clone());
            hook(&web, &format!("pickerArm({config})"), done);
        }
        "Overlay.hideHighlight" => hook(&web, "hideHighlight()", done),

        "Input.dispatchMouseEvent" => dispatch_mouse(shared, web, params, done),

        "Page.getFrameTree" => {
            let url = shared.public_url(&super::current_url(&web));
            ok(done, json!({"frameTree": {"frame": {"id": MAIN_FRAME_ID, "loaderId": "wk", "url": url, "securityOrigin": "", "mimeType": "text/html"}}}));
        }
        "Page.getLayoutMetrics" => hook(&web, "layoutMetrics()", done),
        "Page.getNavigationHistory" => ok(done, navigation_history(shared, &web)),
        "Page.setInterceptFileChooserDialog" => {
            shared.intercept_file_chooser.store(params.get("enabled").and_then(Value::as_bool).unwrap_or(false), Ordering::SeqCst);
            ok(done, json!({}));
        }
        "Page.captureScreenshot" => {
            let shared = shared.clone();
            let params = params.clone();
            let needs_metrics = params.get("clip").is_some() || params.get("captureBeyondViewport").and_then(Value::as_bool) == Some(true);
            if !needs_metrics {
                return snapshot::capture(&shared, params, None, done);
            }
            hook_value(&web, "layoutMetrics()", move |metrics| match metrics {
                Ok(metrics) => {
                    let rect = snapshot::capture_rect(&params, &metrics);
                    snapshot::capture(&shared, params, rect, done);
                }
                Err(error) => done(Err(error)),
            });
        }

        "Emulation.setEmulatedMedia" => set_emulated_media(&web, params, done),
        "Emulation.setDeviceMetricsOverride" => {
            let width = params.get("width").and_then(Value::as_f64).unwrap_or(0.0);
            let height = params.get("height").and_then(Value::as_f64).unwrap_or(0.0);
            {
                let mut emulation = lock(&shared.emulation);
                emulation.viewport = (width > 0.0 && height > 0.0).then_some((width, height));
                emulation.device_scale_factor = params.get("deviceScaleFactor").and_then(Value::as_f64).unwrap_or(0.0);
                emulation.mobile = params.get("mobile").and_then(Value::as_bool).unwrap_or(false);
            }
            apply_emulation(shared, &web, done);
        }
        "Emulation.clearDeviceMetricsOverride" => {
            {
                let mut emulation = lock(&shared.emulation);
                emulation.viewport = None;
                emulation.device_scale_factor = 0.0;
                emulation.mobile = false;
            }
            apply_emulation(shared, &web, done);
        }
        "Emulation.setUserAgentOverride" => {
            let agent = params.get("userAgent").and_then(Value::as_str).unwrap_or_default();
            // SAFETY: a property write on the main thread.
            let agent = (!agent.is_empty()).then(|| NSString::from_str(agent));
            unsafe { web.setCustomUserAgent(agent.as_deref()) };
            ok(done, json!({}));
        }
        "Emulation.setTouchEmulationEnabled" => {
            let enabled = params.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let points = params.get("maxTouchPoints").and_then(Value::as_u64).unwrap_or(1) as u32;
            lock(&shared.emulation).max_touch_points = if enabled { points } else { 0 };
            apply_emulation(shared, &web, done);
        }
        "Emulation.setEmitTouchEventsForMouse" => {
            lock(&shared.emulation).emit_touch_events = params.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            apply_emulation(shared, &web, done);
        }

        "Network.getResponseBody" => {
            let request_id = params.get("requestId").and_then(Value::as_str).unwrap_or_default();
            hook(&web, &format!("responseBody({})", js_string(request_id)), done);
        }
        "Network.getCookies" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            let urls: Vec<url::Url> = params.get("urls").and_then(Value::as_array).map_or_else(Vec::new, |urls| urls.iter().filter_map(Value::as_str).filter_map(|url| url::Url::parse(url).ok()).collect());
            all_cookies(&store, move |cookies| {
                let cookies: Vec<Value> = cookies.into_iter().filter(|cookie| {
                    let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or_default();
                    let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
                    let secure = cookie.get("secure").and_then(Value::as_bool).unwrap_or(false);
                    urls.iter().any(|url| cookie_applies(domain, path, secure, url))
                }).collect();
                ok(done, json!({ "cookies": cookies }));
            });
        }
        "Network.getAllCookies" | "Storage.getCookies" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            all_cookies(&store, move |cookies| ok(done, json!({ "cookies": cookies })));
        }
        "Network.setCookies" | "Storage.setCookies" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            set_cookies(&store, params, done);
        }
        "Network.clearBrowserCookies" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            // SAFETY: extern statics of WebKit's.
            let types = unsafe { [objc2_web_kit::WKWebsiteDataTypeCookies] };
            clear_data(&store, Some(&types), done);
        }
        "Network.clearBrowserCache" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            // SAFETY: extern statics of WebKit's.
            let types = unsafe { [objc2_web_kit::WKWebsiteDataTypeDiskCache, objc2_web_kit::WKWebsiteDataTypeMemoryCache] };
            clear_data(&store, Some(&types), done);
        }
        "Storage.clearDataForOrigin" => {
            let Some(store) = native_store(&shared.label) else {
                return done(Err("the page is closed".into()));
            };
            clear_data(&store, None, done);
        }

        "Browser.getVersion" => ok(done, json!({"protocolVersion": "1.3", "product": REPORTED_PRODUCT, "revision": "", "userAgent": "", "jsVersion": ""})),

        other => done(Err(format!("{other} is not available on the WKWebView page engine"))),
    }
}

fn navigation_history(shared: &PageShared, web: &WKWebView) -> Value {
    // SAFETY: property reads on the main thread.
    unsafe {
        let list = web.backForwardList();
        let item_json = |id: usize, item: &objc2_web_kit::WKBackForwardListItem| {
            let url = item.URL().absoluteString().map_or_else(String::new, |url| url.to_string());
            let url = shared.public_url(&url);
            let title = item.title().map_or_else(String::new, |title| title.to_string());
            json!({"id": id, "url": url, "userTypedURL": url, "title": title, "transitionType": "link"})
        };
        let mut entries = Vec::new();
        for item in list.backList().iter() {
            entries.push(item_json(entries.len(), &item));
        }
        let current_index = entries.len();
        match list.currentItem() {
            Some(item) => entries.push(item_json(current_index, &item)),
            None => entries.push(json!({"id": 0, "url": super::BLANK_URL, "userTypedURL": super::BLANK_URL, "title": "", "transitionType": "link"})),
        }
        for item in list.forwardList().iter() {
            entries.push(item_json(entries.len(), &item));
        }
        json!({ "currentIndex": current_index, "entries": entries })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    /// Every `"Domain.method"` written in the host, so that one upstream adds can't slip through.
    fn methods_written_in(source: &str, into: &mut BTreeSet<String>) {
        let bytes = source.as_bytes();
        let mut index = 0;
        while let Some(found) = source[index..].find('"') {
            let start = index + found + 1;
            let Some(length) = source[start..].find('"') else { break };
            let text = &source[start..start + length];
            let domains = ["Accessibility", "Browser", "CSS", "DOM", "Emulation", "Input", "Network", "Overlay", "Page", "Runtime", "Storage"];
            if let Some((domain, name)) = text.split_once('.') {
                let is_method = domains.contains(&domain)
                    && name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                    && name.chars().all(|c| c.is_ascii_alphanumeric());
                if is_method {
                    into.insert(text.to_owned());
                }
            }
            index = start + length + 1;
            if index >= bytes.len() {
                break;
            }
        }
    }

    #[test]
    fn the_table_answers_every_method_the_host_calls() {
        let mut host = BTreeSet::new();
        methods_written_in(include_str!("../../../host/src/browser.rs"), &mut host);
        methods_written_in(include_str!("../../../host/src/browser_profile_data.rs"), &mut host);
        let known: HashSet<&str> = DEVTOOLS_METHODS.iter().map(|(method, _, _)| *method).collect();
        let missing: Vec<&String> = host.iter().filter(|method| !known.contains(method.as_str())).collect();
        assert!(missing.is_empty(), "the host calls DevTools methods this engine has no line for: {missing:?}");
    }

    #[test]
    fn nothing_listed_is_unavailable_without_saying_what_is_lost() {
        for (method, support, note) in DEVTOOLS_METHODS {
            assert!(!note.is_empty(), "{method} says nothing");
            if *support == Support::Unavailable {
                assert!(note.len() > 8, "{method} doesn't say what is lost");
            }
        }
    }

    #[test]
    fn cookies_match_by_the_rfc_rules() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        assert!(cookie_applies(".example.com", "/", false, &url("https://www.example.com/a")));
        assert!(cookie_applies(".example.com", "/", false, &url("https://example.com/")));
        assert!(!cookie_applies(".example.com", "/", false, &url("https://badexample.com/")));
        assert!(cookie_applies("example.com", "/", false, &url("http://example.com/")));
        assert!(!cookie_applies("example.com", "/", false, &url("http://www.example.com/")));
        assert!(!cookie_applies("example.com", "/", true, &url("http://example.com/")));
        assert!(cookie_applies("example.com", "/app", false, &url("http://example.com/app/x")));
        assert!(cookie_applies("example.com", "/app/", false, &url("http://example.com/app/x")));
        assert!(!cookie_applies("example.com", "/app", false, &url("http://example.com/application")));
    }

    #[test]
    fn emulation_says_nothing_until_asked() {
        let config = emulation_config(&Emulation::default());
        assert_eq!(config["deviceScaleFactor"], Value::Null);
        assert_eq!(config["screenWidth"], Value::Null);
        assert_eq!(config["maxTouchPoints"], 0);
        let phone = Emulation { viewport: Some((375.0, 812.0)), device_scale_factor: 2.0, mobile: true, max_touch_points: 5, emit_touch_events: true };
        let config = emulation_config(&phone);
        assert_eq!(config["screenWidth"], 375.0);
        assert_eq!(config["deviceScaleFactor"], 2.0);
    }
}
