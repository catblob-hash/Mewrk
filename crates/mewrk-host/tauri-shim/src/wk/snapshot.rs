//! `Page.captureScreenshot`: `takeSnapshot`, and the image as the bytes the protocol promises.
//!
//! `takeSnapshot` only paints what lies inside the view: a rectangle reaching past it comes back the
//! right size and blank there. What goes beyond the viewport is therefore drawn by `createPDF`, which
//! paints the document itself without resizing anything, and rasterized here. Growing the view
//! instead would fire `resize`, stretch everything in `vh` and show on top of the client's UI.

use std::sync::Arc;

use base64::Engine as _;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, Message};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSDeviceRGBColorSpace, NSGraphicsContext, NSImage, NSImageCompressionFactor, NSImageRep, NSPDFImageRep};
use objc2_foundation::{NSData, NSDictionary, NSError, NSNumber, NSPoint, NSRect, NSSize};
use objc2_web_kit::{WKPDFConfiguration, WKSnapshotConfiguration, WKWebView};
use serde_json::{json, Value};

use super::cdp::Done;
use super::{native_web, post_main, PageShared};

/// A rectangle of the page in CSS pixels relative to the viewport, and the factor to scale it by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CaptureRect {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) width: f64,
    pub(super) height: f64,
    pub(super) scale: f64,
    /// How far the document is scrolled, which is what makes `x` and `y` view coordinates.
    pub(super) scroll_x: f64,
    pub(super) scroll_y: f64,
}

impl CaptureRect {
    /// Whether any of it lies outside a view `width` by `height`, where `takeSnapshot` paints nothing.
    fn reaches_past(&self, width: f64, height: f64) -> bool {
        // A rounding's worth of slack: a clip computed from fractional layout may overhang by less
        // than a pixel, and the slower path would be wasted on it.
        const SLACK: f64 = 1.0;
        self.x < -SLACK || self.y < -SLACK || self.x + self.width > width + SLACK || self.y + self.height > height + SLACK
    }
}

/// Where the protocol's `clip` lands on the view. A clip is in document coordinates, and a capture
/// that didn't name one is the viewport, or the whole document when it asked to go beyond it.
pub(super) fn capture_rect(params: &Value, metrics: &Value) -> Option<CaptureRect> {
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_f64);
    let scroll_x = metrics.pointer("/cssVisualViewport/pageX").and_then(Value::as_f64).unwrap_or(0.0);
    let scroll_y = metrics.pointer("/cssVisualViewport/pageY").and_then(Value::as_f64).unwrap_or(0.0);
    if let Some(clip) = params.get("clip") {
        return Some(CaptureRect {
            x: number(clip, "x").unwrap_or(0.0) - scroll_x,
            y: number(clip, "y").unwrap_or(0.0) - scroll_y,
            width: number(clip, "width")?,
            height: number(clip, "height")?,
            scale: number(clip, "scale").unwrap_or(1.0),
            scroll_x,
            scroll_y,
        });
    }
    if params.get("captureBeyondViewport").and_then(Value::as_bool) == Some(true) {
        let content = metrics.get("cssContentSize")?;
        return Some(CaptureRect {
            x: -scroll_x,
            y: -scroll_y,
            width: number(content, "width")?,
            height: number(content, "height")?,
            scale: 1.0,
            scroll_x,
            scroll_y,
        });
    }
    None
}

pub(super) fn capture(shared: &Arc<PageShared>, params: Value, rect: Option<CaptureRect>, done: Done) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err("captures run on the main thread".into()));
    };
    let Some(web) = native_web(&shared.label) else {
        return done(Err("the page is closed".into()));
    };
    let jpeg = params.get("format").and_then(Value::as_str) == Some("jpeg");
    let quality = params.get("quality").and_then(Value::as_f64).unwrap_or(80.0) / 100.0;
    // SAFETY: on the main thread, with a live view and a configuration made here.
    let configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
    // SAFETY: a property read on the main thread.
    let zoom = unsafe { web.pageZoom() };
    if let Some(rect) = rect {
        let bounds = web.bounds().size;
        if rect.reaches_past(bounds.width / zoom, bounds.height / zoom) {
            return capture_document(&web, rect, zoom, jpeg, quality, done);
        }
        let frame = NSRect::new(NSPoint::new(rect.x * zoom, rect.y * zoom), NSSize::new(rect.width * zoom, rect.height * zoom));
        // SAFETY: plain property writes on an object owned here.
        unsafe {
            configuration.setRect(frame);
            if rect.scale < 1.0 {
                configuration.setSnapshotWidth(Some(&NSNumber::new_f64(rect.width * rect.scale)));
            }
        }
    }
    // Not after screen updates: those never come for a page that isn't being drawn, and a parked or
    // occluded page is exactly the one the model is working on. WebKit paints the snapshot itself.
    // SAFETY: as above.
    unsafe { configuration.setAfterScreenUpdates(false) };
    let done = std::sync::Mutex::new(Some(done));
    let handler = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
        let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        // SAFETY: WebKit passes live objects or nil for the duration of the call.
        let (image, error) = unsafe { (image.as_ref(), error.as_ref()) };
        let Some(image) = image else {
            let message = error.map_or_else(|| "WebKit returned no snapshot".to_owned(), |error| error.localizedDescription().to_string());
            return done(Err(message));
        };
        let Some(tiff) = image.TIFFRepresentation() else {
            return done(Err("WebKit's snapshot has no pixels".into()));
        };
        // Encoding a retina page takes tens of milliseconds, which are the UI's if spent here. The
        // pane's warm frames ask for one of these every second.
        let tiff = ImmutableData(tiff);
        std::thread::spawn(move || {
            let tiff = tiff;
            let encoded = encode(&tiff.0, jpeg, quality);
            post_main(move || done(encoded));
        });
    });
    // SAFETY: on the main thread; WebKit copies the block.
    unsafe { web.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &handler) };
}

/// `rect` of the document, as drawn by `createPDF`.
fn capture_document(web: &WKWebView, rect: CaptureRect, zoom: f64, jpeg: bool, quality: f64, done: Done) {
    let Some(mtm) = MainThreadMarker::new() else {
        return done(Err("captures run on the main thread".into()));
    };
    let backing = web.window().map_or(1.0, |window| window.backingScaleFactor());
    // SAFETY: on the main thread, with a configuration made here.
    let configuration = unsafe { WKPDFConfiguration::new(mtm) };
    let frame = NSRect::new(
        NSPoint::new((rect.x + rect.scroll_x) * zoom, (rect.y + rect.scroll_y) * zoom),
        NSSize::new(rect.width * zoom, rect.height * zoom),
    );
    // SAFETY: a property write on an object owned here.
    unsafe { configuration.setRect(frame) };
    let done = std::sync::Mutex::new(Some(done));
    let width = (rect.width * rect.scale * backing).round().max(1.0) as isize;
    let height = (rect.height * rect.scale * backing).round().max(1.0) as isize;
    let handler = RcBlock::new(move |pdf: *mut NSData, error: *mut NSError| {
        let Some(done) = done.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        // SAFETY: WebKit passes live objects or nil for the duration of the call.
        let (pdf, error) = unsafe { (pdf.as_ref(), error.as_ref()) };
        let Some(pdf) = pdf else {
            let message = error.map_or_else(|| "WebKit drew no document".to_owned(), |error| error.localizedDescription().to_string());
            return done(Err(message));
        };
        let pdf = ImmutableData(pdf.retain());
        // Rasterizing a whole document is the slowest thing a capture does.
        std::thread::spawn(move || {
            let pdf = pdf;
            let encoded = rasterize(&pdf.0, width, height, jpeg, quality);
            post_main(move || done(encoded));
        });
    });
    // SAFETY: on the main thread; WebKit copies the block.
    unsafe { web.createPDFWithConfiguration_completionHandler(Some(&configuration), &handler) };
}

/// The PDF's first page, `width` by `height` device pixels, encoded as a capture.
fn rasterize(pdf: &NSData, width: isize, height: isize, jpeg: bool, quality: f64) -> Result<String, String> {
    let page = NSPDFImageRep::imageRepWithData(pdf).ok_or_else(|| "could not read WebKit's document".to_owned())?;
    // SAFETY: an extern static of AppKit's. The bitmap owns its planes (null asks it to allocate), and
    // every other argument describes 8-bit RGBA at a row size it works out itself. The context is this
    // thread's own and is restored before the bitmap is read.
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            0,
            0,
        )
    }
    .ok_or_else(|| format!("could not allocate a {width} by {height} capture"))?;
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap).ok_or_else(|| "could not draw into a capture".to_owned())?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    let drawn = NSImageRep::drawInRect(&page, NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width as f64, height as f64)));
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    if !drawn {
        return Err("could not draw WebKit's document".into());
    }
    represent(&bitmap, jpeg, quality)
}

/// A TIFF or PDF on its way to the encoding thread.
struct ImmutableData(Retained<NSData>);

// SAFETY: `NSData` is immutable, which Foundation documents as safe to use from any thread, and
// nothing else holds this one: `TIFFRepresentation` made it for this call.
unsafe impl Send for ImmutableData {}

fn encode(tiff: &NSData, jpeg: bool, quality: f64) -> Result<String, String> {
    let bitmap: Retained<NSBitmapImageRep> = NSBitmapImageRep::imageRepWithData(tiff).ok_or_else(|| "could not read WebKit's snapshot".to_owned())?;
    represent(&bitmap, jpeg, quality)
}

fn represent(bitmap: &NSBitmapImageRep, jpeg: bool, quality: f64) -> Result<String, String> {
    // SAFETY: a documented conversion on an object this thread owns; bitmap image reps may be used
    // off the main thread as long as one thread has them at a time.
    let bytes = unsafe {
        if jpeg {
            let number = NSNumber::new_f64(quality.clamp(0.0, 1.0));
            let properties = NSDictionary::from_slices(&[NSImageCompressionFactor], &[number.as_ref()]);
            bitmap.representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties)
        } else {
            bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        }
    }
    .ok_or_else(|| "could not encode WebKit's snapshot".to_owned())?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes.to_vec());
    Ok(json!({ "data": data }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clip_is_in_document_coordinates_and_the_view_is_not() {
        let metrics = json!({"cssVisualViewport": {"pageX": 10.0, "pageY": 200.0}, "cssContentSize": {"width": 1000.0, "height": 3000.0}});
        let clip = capture_rect(&json!({"clip": {"x": 20, "y": 300, "width": 50, "height": 60, "scale": 0.5}}), &metrics).unwrap();
        assert_eq!((clip.x, clip.y, clip.width, clip.height, clip.scale), (10.0, 100.0, 50.0, 60.0, 0.5));
        assert_eq!((clip.scroll_x, clip.scroll_y), (10.0, 200.0));
        let full = capture_rect(&json!({"captureBeyondViewport": true}), &metrics).unwrap();
        assert_eq!((full.x, full.y, full.width, full.height, full.scale), (-10.0, -200.0, 1000.0, 3000.0, 1.0));
        assert_eq!(capture_rect(&json!({"format": "png"}), &metrics), None);
    }

    #[test]
    fn only_what_reaches_past_the_view_takes_the_slow_path() {
        let rect = |x, y, width, height| CaptureRect { x, y, width, height, scale: 1.0, scroll_x: 0.0, scroll_y: 0.0 };
        assert!(!rect(0.0, 0.0, 1000.0, 760.0).reaches_past(1000.0, 760.0));
        assert!(!rect(0.4, 0.0, 1000.0, 760.4).reaches_past(1000.0, 760.0));
        assert!(rect(0.0, 0.0, 1000.0, 3000.0).reaches_past(1000.0, 760.0));
        assert!(rect(0.0, -200.0, 100.0, 100.0).reaches_past(1000.0, 760.0));
    }
}
