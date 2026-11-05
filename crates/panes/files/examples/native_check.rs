//! The system viewers on the main thread, where a test cannot put them: PDFKit's view with a real document, a web view with
//! scripts off loading a composed page, a click on a link in it deciding nothing by itself. Prints what they report.
//! `cargo run -p mewrk-pane-files --example native_check`

#[path = "../tests/rig/mod.rs"]
#[allow(dead_code)]
mod rig;

#[cfg(target_os = "macos")]
fn main() {
    use mewrk_pane_files::html_preview::{compose_document, prepare_html_preview};
    use mewrk_pane_files::native::*;
    use objc2::{rc::Retained, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::NSView;
    use objc2_foundation::{NSDate, NSPoint, NSRect, NSRunLoop, NSSize};
    use rig::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let mtm = MainThreadMarker::new().expect("the main thread");
    let parent: Retained<NSView> = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(800.0, 600.0)));
    let parent_ptr = Retained::as_ptr(&parent) as *mut std::ffi::c_void;
    let spin = |seconds: f64| {
        let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
        // SAFETY: running this thread's own run loop.
        unsafe { NSRunLoop::currentRunLoop().runUntilDate(&until) };
    };

    // PDF.
    let stream = "BT /F1 12 Tf 10 50 Td (Hello PDFKit) Tj ET";
    let objects = ["<< /Type /Catalog /Pages 2 0 R >>".to_owned(), "<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>".to_owned(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(), format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()), "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(), "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned()];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());
    let mut host = PdfHost::new(&pdf).expect("a document").expect("on the main thread");
    host.place(parent_ptr, fluent::rect(10.0, 10.0, 400.0, 300.0), false);
    spin(0.2);
    println!("pdf pages={} current={} scale={:.2}", host.page_count(), host.current_page(), host.scale());
    host.set_scale(2.0);
    println!("pdf after set_scale(2) scale={:.2}", host.scale());
    host.fit_width();
    spin(0.1);
    println!("pdf after fit scale={:.2}", host.scale());
    println!("pdf text {:?}", extract_pdf_text(&pdf).map(|e| e.text));

    // Web.
    let data = TempDir::new("native-check");
    let app = start_app(&data.0);
    let page = prepare_html_preview("<!doctype html><html><head><title>Composed</title><script>document.title='ran'</script></head><body><h1>Hi</h1><img src=\"https://x.example/a.png\" alt=\"remote\"><a id=l href=\"next.html#s\">next</a></body></html>");
    let html = compose_document(&page, &["h1{color:teal}".to_owned()], &BTreeMap::new(), &|k| k.to_owned());
    let mut web = WebHost::new(app.task(), "site/index.html", Arc::new(|_, url| println!("link {url}"))).expect("on the main thread");
    web.load(1, &html, "site/index.html");
    web.place(parent_ptr, fluent::rect(10.0, 320.0, 400.0, 200.0), false);
    let mut waited = 0;
    while web.is_loading() && waited < 50 {
        spin(0.1);
        waited += 1;
    }
    println!("web loading={} title={:?} (a page whose script ran would say 'ran')", web.is_loading(), web.title());
    app.host.finalize();
}

#[cfg(not(target_os = "macos"))]
fn main() {}
