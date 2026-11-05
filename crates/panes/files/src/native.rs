//! The system's own viewers as native child views kept over a rectangle: PDFKit for PDFs, WKWebView with scripts off for
//! HTML. A native view sits above everything the GPU draws, so it steps aside while a menu or dialog is up, and its
//! floating controls go below it rather than over it. `native_view` is null when headless, and every caller falls back
//! to a card.

use mewrk_client::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub enum PdfError {
    Password,
    /// A PDF that holds no text: a scan.
    WithoutText,
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedPdfText {
    pub text: String,
    pub pages: u32,
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use crate::links::external_http_url;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSData, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
    use objc2_pdf_kit::{PDFDocument, PDFView};
    use objc2_web_kit::{WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationType, WKWebView, WKWebViewConfiguration};
    use std::ffi::c_void;

    /// Keeps a native view over a rectangle, touching it only when something changed.
    fn place(view: &NSView, parent: *mut c_void, r: Rect, covered: bool, last: &mut [f64; 4], hidden: &mut bool) {
        let parent_view = parent as *mut NSView;
        if parent_view.is_null() {
            return;
        }
        // SAFETY: `parent` is the live content view of the window, and views run on the main thread that owns it.
        let parent = unsafe { &*parent_view };
        // SAFETY: an object identity read.
        if unsafe { view.superview() }.as_deref().is_none_or(|s| !std::ptr::eq(s, parent)) {
            parent.addSubview(view);
        }
        let (parent_h, flipped) = (parent.frame().size.height, parent.isFlipped());
        let (x, w, h) = (r.x as f64, r.w as f64, r.h as f64);
        let y = if flipped { r.y as f64 } else { parent_h - r.y as f64 - h };
        let frame = [x, y, w, h];
        if frame != *last {
            *last = frame;
            view.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
        }
        if covered != *hidden {
            *hidden = covered;
            view.setHidden(covered);
        }
    }

    // ------------------------------------------------------------ PDF

    pub struct PdfHost {
        view: Retained<PDFView>,
        document: Retained<PDFDocument>,
        last: [f64; 4],
        hidden: bool,
    }

    impl Drop for PdfHost {
        fn drop(&mut self) {
            self.view.removeFromSuperview();
        }
    }

    /// Reads a PDF's bytes, saying what stands in the way.
    pub fn open_pdf_document(bytes: &[u8]) -> Result<Retained<PDFDocument>, PdfError> {
        let data = NSData::with_bytes(bytes);
        // SAFETY: the data is a valid NSData, and PDFDocument copes with any bytes by answering nil.
        let document = unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }.ok_or_else(|| PdfError::Unreadable(t("这不是有效的 PDF", "This is not a valid PDF").to_owned()))?;
        // SAFETY: plain property read.
        if unsafe { document.isLocked() } {
            return Err(PdfError::Password);
        }
        Ok(document)
    }

    pub fn pdf_page_count(bytes: &[u8]) -> Result<usize, PdfError> {
        let document = open_pdf_document(bytes)?;
        // SAFETY: plain property read.
        Ok(unsafe { document.pageCount() })
    }

    impl PdfHost {
        /// `None` off the main thread, which is only ever a test.
        pub fn new(bytes: &[u8]) -> Result<Option<Self>, PdfError> {
            let document = open_pdf_document(bytes)?;
            let Some(mtm) = MainThreadMarker::new() else { return Ok(None) };
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0));
            // SAFETY: created and used on the main thread; the document stays alive in this struct.
            let view = unsafe {
                let view = PDFView::initWithFrame(PDFView::alloc(mtm), frame);
                view.setDocument(Some(&document));
                view.setAutoScales(true);
                view
            };
            view.setHidden(true);
            Ok(Some(Self { view, document, last: [0.0; 4], hidden: true }))
        }

        pub fn place(&mut self, parent: *mut c_void, r: Rect, covered: bool) {
            place(&self.view, parent, r, covered, &mut self.last, &mut self.hidden);
        }

        pub fn page_count(&self) -> usize {
            // SAFETY: plain property read.
            unsafe { self.document.pageCount() }
        }

        /// 1-based.
        pub fn current_page(&self) -> usize {
            // SAFETY: plain property reads on this thread's objects.
            unsafe { self.view.currentPage().map_or(1, |p| self.document.indexForPage(&p) + 1) }
        }

        pub fn scale(&self) -> f64 {
            // SAFETY: plain property read.
            unsafe { self.view.scaleFactor() }
        }

        pub fn set_scale(&self, scale: f64) {
            // SAFETY: plain property writes on this thread's objects.
            unsafe {
                self.view.setAutoScales(false);
                self.view.setScaleFactor(scale);
            }
        }

        pub fn fit_width(&self) {
            // SAFETY: plain property write.
            unsafe { self.view.setAutoScales(true) };
        }
    }

    // ------------------------------------------------------------ web

    const BASE: &str = "mewrk-preview://page/";

    /// Where a link in a page goes: the app decides, since nothing in the page navigates on its own.
    pub type LinkHandler = Arc<dyn Fn(&mut App, &str) + Send + Sync>;

    struct DelegateIvars {
        task: Task,
        on_link: LinkHandler,
        own: String,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = DelegateIvars]
        struct PreviewDelegate;

        unsafe impl NSObjectProtocol for PreviewDelegate {}

        unsafe impl WKNavigationDelegate for PreviewDelegate {
            #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
            fn decide(&self, _web: &WKWebView, action: &WKNavigationAction, handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>) {
                // SAFETY: reads on objects WebKit hands to this call, on the main thread.
                let (kind, url) = unsafe { (action.navigationType(), action.request().URL().and_then(|u| u.absoluteString()).map(|s| s.to_string())) };
                let url = url.unwrap_or_default();
                let ivars = self.ivars();
                // The first load, and a fragment inside the page itself, stay in the page.
                let same_page = url.split('#').next() == Some(ivars.own.as_str());
                if kind != WKNavigationType::LinkActivated || same_page {
                    let allowed = kind != WKNavigationType::LinkActivated && url.starts_with(BASE) || same_page || url == "about:blank";
                    handler.call((if allowed { WKNavigationActionPolicy::Allow } else { WKNavigationActionPolicy::Cancel },));
                    return;
                }
                handler.call((WKNavigationActionPolicy::Cancel,));
                let on_link = ivars.on_link.clone();
                ivars.task.post(move |app| on_link(app, &url));
            }
        }
    );

    /// What a page link in a preview names, against the document root: `/docs/x.md`, or the web as it is.
    pub fn preview_link(url: &str) -> String {
        match url.strip_prefix(BASE) {
            Some(rest) => format!("/{rest}"),
            None => external_http_url(url).unwrap_or_else(|| url.to_owned()),
        }
    }

    pub struct WebHost {
        view: Retained<WKWebView>,
        _delegate: Retained<PreviewDelegate>,
        last: [f64; 4],
        hidden: bool,
        loaded: u64,
    }

    impl Drop for WebHost {
        fn drop(&mut self) {
            self.view.removeFromSuperview();
        }
    }

    impl WebHost {
        pub fn new(task: Task, document_path: &str, on_link: LinkHandler) -> Option<Self> {
            let mtm = MainThreadMarker::new()?;
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(10.0, 10.0));
            let own = format!("{BASE}{document_path}");
            let delegate: Retained<PreviewDelegate> = {
                let this = PreviewDelegate::alloc(mtm).set_ivars(DelegateIvars { task, on_link, own });
                // SAFETY: plain NSObject init of a class this module defined.
                unsafe { msg_send![super(this), init] }
            };
            // SAFETY: created and used on the main thread; the delegate outlives the view's use of it in this struct.
            let view = unsafe {
                let config = WKWebViewConfiguration::new(mtm);
                config.defaultWebpagePreferences().setAllowsContentJavaScript(false);
                let view = WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config);
                view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                view
            };
            view.setHidden(true);
            Some(Self { view, _delegate: delegate, last: [0.0; 4], hidden: true, loaded: 0 })
        }

        /// Shows a composed document, once per version.
        pub fn load(&mut self, version: u64, html: &str, document_path: &str) {
            if self.loaded == version {
                return;
            }
            self.loaded = version;
            let base = NSURL::URLWithString(&NSString::from_str(&format!("{BASE}{document_path}")));
            // SAFETY: on the main thread, with objects made for this call.
            let _ = unsafe { self.view.loadHTMLString_baseURL(&NSString::from_str(html), base.as_deref()) };
        }

        pub fn place(&mut self, parent: *mut c_void, r: Rect, covered: bool) {
            place(&self.view, parent, r, covered, &mut self.last, &mut self.hidden);
        }

        /// What the page calls itself, once it has loaded.
        pub fn title(&self) -> Option<String> {
            // SAFETY: plain property read on this thread's object.
            unsafe { self.view.title() }.map(|t| t.to_string())
        }

        pub fn is_loading(&self) -> bool {
            // SAFETY: plain property read on this thread's object.
            unsafe { self.view.isLoading() }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The smallest PDF with text PDFKit will open, assembled with a correct cross-reference table.
        pub(crate) fn tiny_pdf(text: &str) -> Vec<u8> {
            let stream = format!("BT /F1 12 Tf 10 50 Td ({text}) Tj ET");
            let objects = [
                "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
                format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            ];
            let mut out = b"%PDF-1.4\n".to_vec();
            let mut offsets = Vec::new();
            for (i, body) in objects.iter().enumerate() {
                offsets.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
            }
            let xref = out.len();
            out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
            for o in offsets {
                out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
            out
        }

        #[test]
        fn reads_a_real_pdf_through_pdfkit() {
            let pdf = tiny_pdf("Hello");
            assert_eq!(pdf_page_count(&pdf), Ok(1));
            let extracted = extract_pdf_text(&pdf).unwrap();
            assert_eq!(extracted.pages, 1);
            assert!(extracted.text.starts_with("[Page 1]\n") && extracted.text.contains("Hello"), "{extracted:?}");
        }

        #[test]
        fn says_so_when_bytes_are_not_a_pdf() {
            assert!(matches!(pdf_page_count(b"not a pdf"), Err(PdfError::Unreadable(_))));
        }

        #[test]
        fn a_link_in_a_page_reads_against_the_document_root() {
            assert_eq!(preview_link("mewrk-preview://page/docs/next.html#top"), "/docs/next.html#top");
            assert_eq!(preview_link("https://example.com/a"), "https://example.com/a");
        }
    }
}

/// `[Page n]\ntext` per page: what upstream's `extractPdfText` gives the model.
pub fn extract_pdf_text(bytes: &[u8]) -> Result<ExtractedPdfText, PdfError> {
    use mewrk_client::runs::attachments as client;
    match client::extract_pdf_text(bytes) {
        Ok(extracted) => Ok(ExtractedPdfText { text: extracted.text, pages: extracted.pages }),
        Err(client::PdfError::Password) => Err(PdfError::Password),
        Err(client::PdfError::WithoutText) => Err(PdfError::WithoutText),
        Err(client::PdfError::Unreadable) => Err(PdfError::Unreadable(t("这不是有效的 PDF", "This is not a valid PDF").to_owned())),
    }
}
