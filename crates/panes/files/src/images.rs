//! Pictures from `data:` URLs, decoded off the UI thread and uploaded through fluent's image queue. Keyed by the URL's
//! hash and shared, so a picture a document shows twice is one texture, and a re-render costs a lookup.
//!
//! An animated GIF, WebP or APNG is that same texture with a thread writing its frames into it, so everything that draws a
//! `Picture` animates the way an `<img>` does upstream without knowing it. The thread decodes one frame at a time, one ahead
//! of the screen: a long animation costs one frame's memory, and one nothing is drawing costs nothing, because the next frame
//! waits until a view has asked for the picture since the last one.

use fluent::gpu::write_image;
use fluent::{queue_image, release_image, ImageId};
use image::metadata::LoopCount;
use image::{AnimationDecoder, Frames, ImageFormat};
use mewrk_client::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Picture {
    Pending,
    Ready { id: ImageId, width: u32, height: u32 },
    Failed,
}

struct Cached {
    picture: Picture,
    used: u64,
    /// Stops when the entry goes.
    animation: Option<Animation>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<u64, Cached>,
    clock: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Keeps a window's worth of pictures; a document with more re-decodes what scrolls back.
const CACHE_LIMIT: usize = 96;
/// Past this a picture is scaled down to fit: no GPU texture need be larger, and a texture this size is already 256 MB.
const MAX_SIDE: u32 = 8192;

/// RGBA pixels (straight alpha) from an encoded picture of a media type.
pub fn decode_picture(bytes: &[u8], media_type: &str) -> Option<(Vec<u8>, u32, u32)> {
    if media_type == "image/svg+xml" {
        return decode_svg(bytes);
    }
    let decoded = image::load_from_memory(bytes).ok().or_else(|| system_decode(bytes))?;
    let (w, h) = (decoded.width(), decoded.height());
    let decoded = if w.max(h) > MAX_SIDE { decoded.thumbnail(MAX_SIDE, MAX_SIDE) } else { decoded };
    let rgba = decoded.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

fn decode_svg(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    // Rasterized at its own size or a little more, and never with its scripts: usvg has no idea what a script is.
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let k = (2048.0 / size.width().max(size.height()).max(1.0)).clamp(1.0, 4.0);
    let (w, h) = (((size.width() * k).ceil() as u32).clamp(1, MAX_SIDE), ((size.height() * k).ceil() as u32).clamp(1, MAX_SIDE));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(k, k), &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Some((rgba, w, h))
}

/// HEIC, AVIF and the other formats only the system decodes. Upstream had them from WebKit, and this is the same decoder.
#[cfg(target_os = "macos")]
fn system_decode(bytes: &[u8]) -> Option<image::DynamicImage> {
    use objc2::AnyThread;
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSData;
    let data = NSData::with_bytes(bytes);
    let picture = NSImage::initWithData(NSImage::alloc(), &data)?;
    let tiff = picture.TIFFRepresentation()?;
    image::load_from_memory_with_format(&tiff.to_vec(), image::ImageFormat::Tiff).ok()
}

#[cfg(not(target_os = "macos"))]
fn system_decode(_bytes: &[u8]) -> Option<image::DynamicImage> {
    None
}

fn media_type_of(source: &str) -> &str {
    source.strip_prefix("data:").and_then(|s| s.split([';', ',']).next()).unwrap_or("")
}

/// The picture behind a `data:` URL: its texture when decoded, starting the decode when nobody has.
pub fn picture_for(app: &App, source: &Arc<str>) -> Picture {
    known_picture(source).unwrap_or_else(|| start_picture(app.task(), source))
}

/// The same from where only a `Task` reaches: a Markdown leaf's image resolver holds no `App`.
pub fn picture_for_task(task: &Task, source: &Arc<str>) -> Picture {
    known_picture(source).unwrap_or_else(|| start_picture(task.clone(), source))
}

fn known_picture(source: &Arc<str>) -> Option<Picture> {
    let key = fluent::text::hash_str(source);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.clock += 1;
        let clock = c.clock;
        c.entries.get_mut(&key).map(|e| {
            e.used = clock;
            if let Some(animation) = &e.animation {
                animation.shared.saw();
            }
            e.picture
        })
    })
}

fn start_picture(task: Task, source: &Arc<str>) -> Picture {
    let key = fluent::text::hash_str(source);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let clock = c.clock;
        c.entries.insert(key, Cached { picture: Picture::Pending, used: clock, animation: None });
        if c.entries.len() > CACHE_LIMIT {
            let oldest = c.entries.iter().filter(|(k, e)| **k != key && e.picture != Picture::Pending).min_by_key(|(_, e)| e.used).map(|(k, _)| *k);
            if let Some(entry) = oldest.and_then(|k| c.entries.remove(&k))
                && let Picture::Ready { id, .. } = entry.picture
            {
                release_image(id);
            }
        }
    });
    let source = source.clone();
    std::thread::Builder::new().name("picture".to_owned()).spawn(move || {
        let media_type = media_type_of(&source).to_owned();
        let bytes = crate::format::data_url_bytes(&source);
        let decoded = bytes.as_ref().and_then(|bytes| decode_picture(bytes, &media_type));
        let mut animation = None;
        let picture = match decoded {
            Some((rgba, width, height)) => {
                let id = queue_image(rgba, width, height);
                if let Some(bytes) = bytes.filter(|bytes| is_animated(bytes)) {
                    animation = Animation::start(bytes, id, (width, height), task.clone());
                }
                Picture::Ready { id, width, height }
            }
            None => Picture::Failed,
        };
        task.post(move |_| {
            CACHE.with(|c| {
                if let Some(e) = c.borrow_mut().entries.get_mut(&key) {
                    e.picture = picture;
                    e.animation = animation;
                }
            });
            repaint();
        });
    }).expect("thread");
    Picture::Pending
}

/// Whether a picture has a thread writing frames into it.
pub fn is_animating(source: &Arc<str>) -> bool {
    let key = fluent::text::hash_str(source);
    CACHE.with(|c| c.borrow().entries.get(&key).is_some_and(|e| e.animation.is_some()))
}

/// A program's icon in a menu, small and already decoded by the time the menu has it.
pub fn icon_from_data_url(source: &str) -> Option<ImageId> {
    let key = fluent::text::hash_str(source);
    if let Some(Picture::Ready { id, .. }) = CACHE.with(|c| c.borrow().entries.get(&key).map(|e| e.picture)) {
        return Some(id);
    }
    let bytes = crate::format::data_url_bytes(source)?;
    let (rgba, width, height) = decode_picture(&bytes, media_type_of(source))?;
    let id = queue_image(rgba, width, height);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let used = c.clock;
        c.entries.insert(key, Cached { picture: Picture::Ready { id, width, height }, used, animation: None });
    });
    Some(id)
}

// ---------------------------------------------------------------- animation

/// A browser's floor for a frame's delay. Anything this short is taken as a file with no timing of its own, and without the
/// floor it would flash as fast as frames can be drawn.
const SHORTEST_DELAY_MS: f64 = 10.0;
const DELAY_FOR_THE_SHORTEST: Duration = Duration::from_millis(100);

fn frame_delay(frame: &image::Frame) -> Duration {
    let (numerator, denominator) = frame.delay().numer_denom_ms();
    let ms = numerator as f64 / denominator.max(1) as f64;
    if ms <= SHORTEST_DELAY_MS { DELAY_FOR_THE_SHORTEST } else { Duration::from_secs_f64(ms / 1000.0) }
}

/// A file's frames from the start, read as they are asked for, and how many times it says to play them. `None` for a format
/// with no animation, or a PNG or WebP that says it has none.
pub fn animated_frames(bytes: Arc<[u8]>) -> Option<(Frames<'static>, LoopCount)> {
    let reader = || Cursor::new(bytes.clone());
    match image::guess_format(&bytes).ok()? {
        ImageFormat::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(reader()).ok()?;
            // `gif` answers the same for "loop forever" and for a file that never says, and a browser plays that one once.
            let says = bytes.windows(11).any(|w| w == b"NETSCAPE2.0" || w == b"ANIMEXTS1.0");
            let count = if says { decoder.loop_count() } else { LoopCount::Finite(std::num::NonZeroU32::MIN) };
            Some((decoder.into_frames(), count))
        }
        ImageFormat::WebP => {
            let decoder = image::codecs::webp::WebPDecoder::new(reader()).ok()?;
            if !decoder.has_animation() {
                return None;
            }
            let count = decoder.loop_count();
            Some((decoder.into_frames(), count))
        }
        ImageFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::new(reader()).ok()?;
            if !decoder.is_apng().ok()? {
                return None;
            }
            let decoder = decoder.apng().ok()?;
            let count = decoder.loop_count();
            Some((decoder.into_frames(), count))
        }
        _ => None,
    }
}

/// Whether a file has a second frame to show. Costs at most two frames, however long the file.
pub fn is_animated(bytes: &[u8]) -> bool {
    animated_frames(Arc::from(bytes)).is_some_and(|(frames, _)| frames.take(2).filter(Result::is_ok).count() == 2)
}

/// What the picture's thread and the views that draw the picture know of each other.
#[derive(Default)]
struct Watch {
    stop: AtomicBool,
    /// Bumped every time a view asks for the picture.
    seen: AtomicU64,
    thread: OnceLock<std::thread::Thread>,
}

impl Watch {
    fn saw(&self) {
        self.seen.fetch_add(1, Ordering::Relaxed);
        self.wake();
    }

    fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Waits for a moment to come, or answers `false` if the picture has gone.
    fn sleep_until(&self, until: Instant) -> bool {
        while !self.stopped() {
            let now = Instant::now();
            if now >= until {
                return true;
            }
            std::thread::park_timeout(until - now);
        }
        false
    }

    /// Waits for a view to ask for the picture again, or answers `false` if the picture has gone. A picture scrolled away, in a
    /// page not on show or in a window that draws nothing stops here, with its one decoded frame.
    fn until_seen_after(&self, seen: u64) -> bool {
        while !self.stopped() {
            if self.seen.load(Ordering::Relaxed) != seen {
                return true;
            }
            std::thread::park();
        }
        false
    }
}

struct Animation {
    shared: Arc<Watch>,
}

impl Animation {
    fn start(bytes: Vec<u8>, id: ImageId, (width, height): (u32, u32), task: Task) -> Option<Self> {
        let shared = Arc::new(Watch::default());
        let watch = shared.clone();
        std::thread::Builder::new()
            .name("animated picture".to_owned())
            .spawn(move || {
                let _ = watch.thread.set(std::thread::current());
                play(Arc::from(bytes), &watch, &fluent::reduced_motion, (width, height), &mut |rgba| {
                    write_image(id, 0, 0, width, height, rgba);
                    task.post(|_| repaint());
                });
            })
            .ok()?;
        Some(Self { shared })
    }
}

impl Drop for Animation {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake();
    }
}

/// Shows a file's frames in order at their own pace until it has played as often as it says, and then holds its last frame,
/// as a browser does. While `still` says motion should stop it holds the first frame instead, and picks up from the start when
/// that changes. `size` is the texture's: frames of another size are not written, so a picture scaled down to fit a texture
/// stays the still it was decoded as.
fn play(bytes: Arc<[u8]>, watch: &Watch, still: &dyn Fn() -> bool, size: (u32, u32), show: &mut dyn FnMut(Vec<u8>)) {
    /// How soon a held picture notices the setting has changed. The setting is read when the window comes back to the front,
    /// so this need not be quick.
    const STILL_CHECK: Duration = Duration::from_millis(500);
    let mut plays = 0u32;
    // Outside the loop: the last frame of a play is on screen for its own delay too.
    let mut due = Instant::now();
    'file: loop {
        let Some((mut frames, count)) = animated_frames(bytes.clone()) else { return };
        let mut holding = false;
        while let Some(Ok(frame)) = frames.next() {
            if frame.buffer().dimensions() != size {
                return;
            }
            let delay = frame_delay(&frame);
            if !watch.sleep_until(due) {
                return;
            }
            let seen = watch.seen.load(Ordering::Relaxed);
            show(frame.into_buffer().into_raw());
            due = Instant::now() + delay;
            if !watch.until_seen_after(seen) {
                return;
            }
            if still() {
                while still() {
                    if !watch.sleep_until(Instant::now() + STILL_CHECK) {
                        return;
                    }
                }
                holding = true;
                break;
            }
        }
        if holding {
            plays = 0;
            continue 'file;
        }
        plays += 1;
        if let LoopCount::Finite(times) = count
            && plays >= times.get()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_and_svg_to_straight_rgba() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let (rgba, w, h) = decode_picture(&png, "image/png").unwrap();
        assert_eq!((w, h, &rgba[..4]), (3, 2, &[10u8, 20, 30, 255][..]));
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="#ff0000"/></svg>"##;
        let (rgba, w, _) = decode_picture(svg, "image/svg+xml").unwrap();
        assert!(w >= 10);
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn says_so_when_bytes_are_not_a_picture() {
        assert!(decode_picture(b"nope", "image/png").is_none());
    }

    /// A real GIF, encoded here: `frames` frames of solid colour, each `delay_ms` long, looping as told.
    fn gif(frames: u8, delay_ms: u32, repeat: Option<image::codecs::gif::Repeat>) -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            if let Some(repeat) = repeat {
                encoder.set_repeat(repeat).unwrap();
            }
            for i in 0..frames {
                let shade = i.wrapping_mul(60);
                let pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([shade, 255 - shade, 0, 255]));
                encoder.encode_frame(image::Frame::from_parts(pixels, 0, 0, image::Delay::from_numer_denom_ms(delay_ms, 1))).unwrap();
            }
        }
        out
    }

    /// Plays on a thread and hands back what it showed, for as long as `drive` says.
    fn played(bytes: Vec<u8>, size: (u32, u32), still: fn() -> bool, drive: impl FnOnce(&Arc<Watch>, &std::sync::Mutex<Vec<Vec<u8>>>)) -> Vec<Vec<u8>> {
        let shown: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Default::default();
        let watch = Arc::new(Watch::default());
        let thread = {
            let (watch, shown) = (watch.clone(), shown.clone());
            std::thread::spawn(move || {
                let _ = watch.thread.set(std::thread::current());
                play(Arc::from(bytes), &watch, &still, size, &mut |rgba| shown.lock().unwrap().push(rgba));
            })
        };
        drive(&watch, &shown);
        watch.stop.store(true, Ordering::Relaxed);
        watch.wake();
        thread.join().unwrap();
        let out = shown.lock().unwrap().clone();
        out
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn red_of(frames: &[Vec<u8>]) -> Vec<u8> {
        frames.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn knows_a_real_animated_gif_from_a_still_picture() {
        assert!(is_animated(&gif(3, 20, None)));
        assert!(!is_animated(&gif(1, 20, None)), "one frame is a still");
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255])).write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        assert!(!is_animated(&png));
        assert!(!is_animated(b"nope"));
    }

    #[test]
    fn the_picture_itself_is_the_first_frame_of_a_gif() {
        let (rgba, w, h) = decode_picture(&gif(3, 20, None), "image/gif").unwrap();
        assert_eq!((w, h, rgba[0]), (4, 4, 0));
    }

    #[test]
    fn plays_a_gif_in_order_at_its_pace_and_loops_when_it_says_to() {
        let started = Instant::now();
        let frames = played(gif(3, 40, Some(image::codecs::gif::Repeat::Infinite)), (4, 4), || false, |watch, shown| {
            // A view draws every frame, as the window does.
            wait_for("five frames", || {
                watch.saw();
                shown.lock().unwrap().len() >= 5
            });
        });
        assert!(started.elapsed() >= Duration::from_millis(4 * 40 - 5), "{:?}", started.elapsed());
        // Its own colours, with the encoder's small error, round and round.
        let red = red_of(&frames);
        assert!(red[0] < 5 && (55..65).contains(&red[1]) && (115..125).contains(&red[2]) && red[3] < 5 && (55..65).contains(&red[4]), "{red:?}");
    }

    #[test]
    fn a_gif_that_never_says_to_loop_plays_once_and_holds_its_last_frame() {
        let frames = played(gif(3, 20, None), (4, 4), || false, |watch, shown| {
            wait_for("three frames", || {
                watch.saw();
                shown.lock().unwrap().len() >= 3
            });
            let at = Instant::now();
            while at.elapsed() < Duration::from_millis(100) {
                watch.saw();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn plays_as_often_as_a_gif_says() {
        let frames = played(gif(2, 20, Some(image::codecs::gif::Repeat::Finite(2))), (4, 4), || false, |watch, shown| {
            wait_for("both plays", || {
                watch.saw();
                shown.lock().unwrap().len() >= 4
            });
            let at = Instant::now();
            while at.elapsed() < Duration::from_millis(100) {
                watch.saw();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        assert_eq!(frames.len(), 4);
    }

    #[test]
    fn decodes_nothing_past_the_next_frame_while_no_view_is_asking() {
        // A hundred frames, and a view that never comes back after the first: anything decoded ahead would be shown here.
        let frames = played(gif(100, 20, Some(image::codecs::gif::Repeat::Infinite)), (4, 4), || false, |_, shown| {
            wait_for("the first frame", || !shown.lock().unwrap().is_empty());
            std::thread::sleep(Duration::from_millis(150));
        });
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn holds_the_first_frame_under_reduced_motion() {
        let frames = played(gif(3, 20, Some(image::codecs::gif::Repeat::Infinite)), (4, 4), || true, |watch, shown| {
            wait_for("the first frame", || {
                watch.saw();
                !shown.lock().unwrap().is_empty()
            });
            let at = Instant::now();
            while at.elapsed() < Duration::from_millis(120) {
                watch.saw();
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        assert_eq!(red_of(&frames), vec![0]);
    }

    #[test]
    fn a_frame_with_no_delay_of_its_own_gets_a_browsers_floor() {
        let frame = |ms| image::Frame::from_parts(image::RgbaImage::new(1, 1), 0, 0, image::Delay::from_numer_denom_ms(ms, 1));
        assert_eq!(frame_delay(&frame(0)), Duration::from_millis(100));
        assert_eq!(frame_delay(&frame(10)), Duration::from_millis(100));
        assert_eq!(frame_delay(&frame(20)), Duration::from_millis(20));
    }

    #[test]
    fn plays_a_real_animated_webp() {
        // Two lossless frames from the WebP encoder, wrapped as the container says: `VP8X` with its animation flag, `ANIM`,
        // and an `ANMF` around each frame's `VP8L` chunk.
        let lossless = |color: [u8; 4]| {
            let mut out = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut out).encode(&color.repeat(4), 2, 2, image::ExtendedColorType::Rgba8).unwrap();
            out[12..].to_vec()
        };
        let chunk = |kind: &[u8; 4], body: &[u8]| {
            let mut out = kind.to_vec();
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(0);
            }
            out
        };
        let frame = |color: [u8; 4]| {
            let mut body = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 30, 0, 0, 0];
            body.extend_from_slice(&lossless(color));
            chunk(b"ANMF", &body)
        };
        let mut file = b"WEBP".to_vec();
        file.extend_from_slice(&chunk(b"VP8X", &[0x12, 0, 0, 0, 1, 0, 0, 1, 0, 0]));
        file.extend_from_slice(&chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
        file.extend_from_slice(&frame([255, 0, 0, 255]));
        file.extend_from_slice(&frame([0, 0, 255, 255]));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(file.len() as u32).to_le_bytes());
        webp.extend_from_slice(&file);
        assert!(is_animated(&webp));
        let frames = played(webp, (2, 2), || false, |watch, shown| {
            wait_for("three frames", || {
                watch.saw();
                shown.lock().unwrap().len() >= 3
            });
        });
        // Composited over the canvas, which may round a channel by one.
        let close = |frame: &[u8], color: [u8; 4]| frame[..4].iter().zip(color).all(|(a, b)| a.abs_diff(b) <= 2);
        assert!(close(&frames[0], [255, 0, 0, 255]), "{:?}", &frames[0][..4]);
        assert!(close(&frames[1], [0, 0, 255, 255]), "{:?}", &frames[1][..4]);
        assert!(close(&frames[2], [255, 0, 0, 255]), "`ANIM` says to play forever: {:?}", &frames[2][..4]);
    }

    #[test]
    fn plays_a_real_apng() {
        // An APNG written chunk by chunk: two frames, red then blue, with `acTL`/`fcTL`/`fdAT` where the format puts them.
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            let mut crc = !0u32;
            for byte in kind.iter().chain(body) {
                crc ^= *byte as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
                }
            }
            out.extend_from_slice(&(!crc).to_be_bytes());
        }
        let raw = |color: [u8; 4]| miniz_oxide::deflate::compress_to_vec_zlib(&[&[0u8][..], &color, &color, &[0], &color, &color].concat(), 6);
        let control = |sequence: u32| {
            let mut body = sequence.to_be_bytes().to_vec();
            for v in [2u32, 2, 0, 0] {
                body.extend_from_slice(&v.to_be_bytes());
            }
            body.extend_from_slice(&[0, 20, 0, 100, 0, 0]);
            body
        };
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
        chunk(&mut png, b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]);
        chunk(&mut png, b"fcTL", &control(0));
        chunk(&mut png, b"IDAT", &raw([255, 0, 0, 255]));
        chunk(&mut png, b"fcTL", &control(1));
        let mut data = 2u32.to_be_bytes().to_vec();
        data.extend_from_slice(&raw([0, 0, 255, 255]));
        chunk(&mut png, b"fdAT", &data);
        chunk(&mut png, b"IEND", &[]);
        assert!(is_animated(&png));
        let frames = played(png, (2, 2), || false, |watch, shown| {
            wait_for("three frames", || {
                watch.saw();
                shown.lock().unwrap().len() >= 3
            });
        });
        assert_eq!(&frames[0][..4], &[255, 0, 0, 255]);
        assert_eq!(&frames[1][..4], &[0, 0, 255, 255]);
        assert_eq!(&frames[2][..4], &[255, 0, 0, 255], "`acTL` says to play forever");
    }
}
