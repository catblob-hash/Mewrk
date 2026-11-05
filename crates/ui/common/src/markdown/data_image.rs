//! A picture in a `data:` address, for a message with no surface to resolve it: what the page's own `<img>` does upstream, where the
//! `img-src` of `tauri.conf.json` is `'self' data:`. Decoded once off the UI thread, and kept while something asks for it.

use super::MarkdownImage;
use fluent::el::repaint;
use fluent::gpu::{queue_image, release_image};
use fluent::text::hash_str;
use mewrk_client::Task;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

/// A long conversation's worth of pictures, and bounded.
const LIMIT: usize = 64;
/// No side of a decoded picture goes past this: it only ever draws in a column of text.
const MAX_SIDE: u32 = 4096;

#[derive(Clone, Copy)]
enum Entry {
    Pending,
    Ready(MarkdownImage),
    Failed,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<u64, (Entry, u64)>,
    clock: u64,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);

thread_local! {
    /// What gets a decoded picture a frame: a leaf's resolver holds no `App`, so the leaf leaves this here.
    static TASK: std::cell::RefCell<Option<Task>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn remember_task(app: &mewrk_client::App) {
    TASK.with(|task| {
        let mut task = task.borrow_mut();
        if task.is_none() {
            *task = Some(app.task());
        }
    });
}

/// What to draw for this address now, starting on it when nothing has yet. Anything that is not a `data:image/` address answers `None`
/// for good: the page's `img-src` refuses it, and its description stands in.
pub(super) fn picture(src: &str) -> Option<MarkdownImage> {
    let trimmed = src.trim();
    if !trimmed.get(..11).is_some_and(|head| head.eq_ignore_ascii_case("data:image/")) {
        return None;
    }
    let key = hash_str(trimmed);
    let mut cache = CACHE.lock().ok()?;
    cache.clock += 1;
    let clock = cache.clock;
    if let Some((entry, used)) = cache.entries.get_mut(&key) {
        *used = clock;
        return if let Entry::Ready(image) = *entry { Some(image) } else { None };
    }
    if cache.entries.len() >= LIMIT
        && let Some(oldest) = cache.entries.iter().filter(|(_, (entry, _))| !matches!(entry, Entry::Pending)).min_by_key(|(_, (_, used))| *used).map(|(k, _)| *k)
        && let Some((Entry::Ready(image), _)) = cache.entries.remove(&oldest)
    {
        release_image(image.id);
    }
    cache.entries.insert(key, (Entry::Pending, clock));
    drop(cache);
    let source = trimmed.to_owned();
    let task = TASK.with(|task| task.borrow().clone());
    let started = std::thread::Builder::new().name("markdown-picture".to_owned()).spawn(move || {
        let entry = decode(&source).map_or(Entry::Failed, |(rgba, width, height)| Entry::Ready(MarkdownImage { id: queue_image(rgba, width, height), size: (width, height) }));
        if let Ok(mut cache) = CACHE.lock()
            && let Some(slot) = cache.entries.get_mut(&key)
        {
            slot.0 = entry;
        }
        if let Some(task) = task {
            task.post(|_| repaint());
        }
    });
    if started.is_err()
        && let Ok(mut cache) = CACHE.lock()
    {
        cache.entries.insert(key, (Entry::Failed, clock));
    }
    None
}

/// Straight RGBA from the text of a `data:` address.
fn decode(source: &str) -> Option<(Vec<u8>, u32, u32)> {
    let (head, payload) = source.split_once(',')?;
    let media_type = head.strip_prefix("data:").or_else(|| head.get(5..))?.split(';').next()?.to_ascii_lowercase();
    let bytes = if head.to_ascii_lowercase().ends_with(";base64") {
        base64_simd::STANDARD.decode_to_vec(payload.split_whitespace().collect::<String>().as_bytes()).ok()?
    } else {
        crate::path_text::decode(payload).into_bytes()
    };
    if media_type == "image/svg+xml" {
        return decode_svg(&bytes);
    }
    let decoded = image::load_from_memory(&bytes).ok()?;
    let decoded = if decoded.width().max(decoded.height()) > MAX_SIDE { decoded.thumbnail(MAX_SIDE, MAX_SIDE) } else { decoded };
    let rgba = decoded.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some((rgba.into_raw(), width, height))
}

fn decode_svg(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let (width, height) = ((size.width().ceil() as u32).clamp(1, MAX_SIDE), (size.height().ceil() as u32).clamp(1, MAX_SIDE));
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    // It comes premultiplied, and the textures here are straight.
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Some((rgba, width, height))
}
