//! Pictures of a remote screen, sized and encoded for whoever looks at them.
//!
//! Two audiences see a screen, with different needs. The viewer page streams
//! it continuously, so its frames are encoded fast — a box filter down to a
//! long edge of [`VIEWER_LONG_EDGE`], JPEG quality [`VIEWER_JPEG_QUALITY`] —
//! and **once**, however many pages are watching ([`FrameHub`]). The model sees
//! one picture per tool call and works in its pixels, so that picture is
//! resampled carefully (Lanczos-3, the prompt-image pipeline's resampler) to
//! the coordinate frame of [`model_frame`].
//!
//! The coordinate frame is the contract between the screenshot and
//! `preview_input`: `scale = min(1, 1280 / max(w, h))`, `round(w·scale) ×
//! round(h·scale)`. Every x/y the model writes is a pixel of that picture, and
//! the session maps it back onto the screen (`docs/remote-control.md` §3.2).

use std::{
    collections::HashSet,
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use super::{
    driver::{Frame, ScreenUpdate},
    view::ViewerFrame,
};

/// The long edge of a frame streamed to the viewer page. The pane is rarely
/// wider than this, and a 4K desktop at full size would cost four times the
/// encoding for nothing the pane can show.
pub const VIEWER_LONG_EDGE: u32 = 1600;
/// JPEG quality of the viewer's stream: legible text, small parts.
pub const VIEWER_JPEG_QUALITY: u8 = 70;
/// The long edge of the model's coordinate frame.
pub const MODEL_LONG_EDGE: u32 = 1280;
/// JPEG quality of the model's screenshot, a little above the stream's because
/// the model reads small text off it.
pub const MODEL_JPEG_QUALITY: u8 = 75;
/// How long after a stream last waited for a frame it still counts as open. The
/// viewer server asks again at once after each frame, and at least once a
/// second for its keepalive, so a stream that has not asked for this long has
/// ended without saying so.
const STREAM_GRACE: Duration = Duration::from_secs(3);
/// Pages a hub tracks as visible. A real pane has one or two; the bound keeps
/// a page that invents ids from growing the set.
const MAX_VISIBLE_PAGES: usize = 32;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `width × height` scaled so its long edge is at most `long_edge`, never up.
/// Each side is at least one pixel.
pub fn fit_long_edge(width: u32, height: u32, long_edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= long_edge || long == 0 {
        return (width, height);
    }
    let scale = f64::from(long_edge) / f64::from(long);
    let side = |value: u32| ((f64::from(value) * scale).round() as u32).max(1);
    (side(width), side(height))
}

/// The model's coordinate frame for a screen of `width × height` pixels.
pub fn model_frame(width: u32, height: u32) -> (u32, u32) {
    fit_long_edge(width, height, MODEL_LONG_EDGE)
}

/// A pixel of the coordinate frame `frame` mapped onto a screen of `screen`
/// pixels, clamped onto the screen.
pub fn frame_to_screen(point: (u32, u32), frame: (u32, u32), screen: (u32, u32)) -> (u32, u32) {
    let axis = |value: u32, frame: u32, screen: u32| {
        if frame == 0 || screen == 0 {
            return 0;
        }
        let mapped = (f64::from(value) * f64::from(screen) / f64::from(frame)).round() as u32;
        mapped.min(screen - 1)
    };
    (
        axis(point.0, frame.0, screen.0),
        axis(point.1, frame.1, screen.1),
    )
}

/// A screen pixel mapped into the coordinate frame `frame`.
pub fn screen_to_frame(point: (i64, i64), screen: (u32, u32), frame: (u32, u32)) -> (i64, i64) {
    let axis = |value: i64, screen: u32, frame: u32| {
        if screen == 0 {
            return value;
        }
        (value as f64 * f64::from(frame) / f64::from(screen)).round() as i64
    };
    (
        axis(point.0, screen.0, frame.0),
        axis(point.1, screen.1, frame.1),
    )
}

/// Checks that a driver's frame is what [`Frame`] promises: a non-empty
/// picture whose buffer holds exactly four bytes per pixel.
pub fn check_frame(frame: &Frame) -> Result<(), String> {
    let expected = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|pixels| pixels.checked_mul(4));
    if frame.width == 0 || frame.height == 0 || expected != Some(frame.rgba.len()) {
        return Err(format!(
            "the screen sent a {}x{} picture whose pixel data does not match its size",
            frame.width, frame.height
        ));
    }
    Ok(())
}

/// The viewer's JPEG of `frame`: box-filtered down to [`VIEWER_LONG_EDGE`],
/// quality [`VIEWER_JPEG_QUALITY`]. Returns the bytes and their size.
pub fn encode_for_viewer(frame: &Frame) -> Result<(Vec<u8>, u32, u32), String> {
    check_frame(frame)?;
    let (width, height) = fit_long_edge(frame.width, frame.height, VIEWER_LONG_EDGE);
    let jpeg = if (width, height) == (frame.width, frame.height) {
        encode_jpeg(
            width,
            height,
            &frame.rgba,
            jpeg_encoder::ColorType::Rgba,
            VIEWER_JPEG_QUALITY,
        )?
    } else {
        let rgb = box_downscale_rgb(&frame.rgba, frame.width, frame.height, width, height);
        encode_jpeg(
            width,
            height,
            &rgb,
            jpeg_encoder::ColorType::Rgb,
            VIEWER_JPEG_QUALITY,
        )?
    };
    Ok((jpeg, width, height))
}

/// The qualities the model's JPEG steps down through when it is over the
/// prompt pipeline's byte budget. A busy desktop or a photo at 1280 px can be,
/// at [`MODEL_JPEG_QUALITY`].
const MODEL_FALLBACK_QUALITIES: [u8; 7] = [60, 45, 32, 22, 14, 8, 1];

// The coordinate frame has to fit the prompt pipeline's sides, or the picture
// the model reads would be scaled away from the frame its x/y are read in.
const _: () = assert!(MODEL_LONG_EDGE <= crate::image_attachments::MODEL_IMAGE_MAX_SIDE);

/// The model's JPEG of `frame`, resampled to its coordinate frame with the
/// prompt-image pipeline's Lanczos-3, quality [`MODEL_JPEG_QUALITY`]. Returns
/// the bytes and the frame's size.
///
/// The JPEG is within the prompt pipeline's byte budget
/// ([`crate::image_attachments::MODEL_IMAGE_BYTE_BUDGET`]): a heavier one is
/// re-encoded here at lower qualities, never at a smaller size. Left to the
/// pipeline, a picture over budget could come back smaller than the frame,
/// and every coordinate the model read off it would land somewhere else.
pub fn encode_for_model(frame: &Frame) -> Result<(Vec<u8>, u32, u32), String> {
    check_frame(frame)?;
    let (width, height) = model_frame(frame.width, frame.height);
    let pixels =
        crate::image_attachments::resize(&frame.rgba, frame.width, frame.height, width, height);
    let jpeg = encode_model_jpeg(&pixels, width, height)?;
    Ok((jpeg, width, height))
}

/// A close-up for the model (`preview_zoom`): the screen pixels of `area`
/// resampled with the screenshot's Lanczos-3 to exactly `size` — larger than
/// `area` when it is a small one — and encoded as the screenshot is, within the
/// prompt pipeline's sides and byte budget and never at a smaller size: the
/// caller maps the close-up's pixels back onto the frame by its size.
pub fn encode_area_for_model(
    frame: &Frame,
    area: super::driver::Rect,
    (width, height): (u32, u32),
) -> Result<Vec<u8>, String> {
    check_frame(frame)?;
    let (left, top) = (
        u32::try_from(area.x).unwrap_or(u32::MAX),
        u32::try_from(area.y).unwrap_or(u32::MAX),
    );
    let inside = left
        .checked_add(area.width)
        .is_some_and(|right| right <= frame.width)
        && top
            .checked_add(area.height)
            .is_some_and(|bottom| bottom <= frame.height);
    if area.width == 0 || area.height == 0 || !inside {
        return Err(format!(
            "the {}x{} area at ({}, {}) is not inside the {}x{} screen",
            area.width, area.height, area.x, area.y, frame.width, frame.height
        ));
    }
    let max_side = crate::image_attachments::MODEL_IMAGE_MAX_SIDE;
    if width == 0 || height == 0 || width.max(height) > max_side {
        return Err(format!(
            "a {width}x{height} close-up does not fit the model's {max_side}-pixel sides"
        ));
    }
    let row = area.width as usize * 4;
    let mut crop = Vec::with_capacity(row * area.height as usize);
    for line in top..top + area.height {
        let start = (line as usize * frame.width as usize + left as usize) * 4;
        crop.extend_from_slice(&frame.rgba[start..start + row]);
    }
    let pixels = crate::image_attachments::resize(&crop, area.width, area.height, width, height);
    encode_model_jpeg(&pixels, width, height)
}

/// The model's JPEG of `width × height` opaque RGBA `pixels`: quality
/// [`MODEL_JPEG_QUALITY`], stepping down through the fallback qualities until it
/// is within the prompt pipeline's byte budget.
fn encode_model_jpeg(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let budget = crate::image_attachments::MODEL_IMAGE_BYTE_BUDGET;
    let encode = |quality| {
        encode_jpeg(
            width,
            height,
            pixels,
            jpeg_encoder::ColorType::Rgba,
            quality,
        )
    };
    let mut jpeg = encode(MODEL_JPEG_QUALITY)?;
    for quality in MODEL_FALLBACK_QUALITIES {
        if jpeg.len() <= budget {
            break;
        }
        jpeg = encode(quality)?;
    }
    if jpeg.len() > budget {
        return Err(format!(
            "a {width}x{height} picture of the screen does not fit in {budget} bytes at any JPEG quality"
        ));
    }
    Ok(jpeg)
}

/// A baseline JPEG with the standard Huffman tables: one interleaved scan,
/// which every decoder reads (see `image_attachments`' encoder for why the
/// optimized tables are avoided).
fn encode_jpeg(
    width: u32,
    height: u32,
    pixels: &[u8],
    color: jpeg_encoder::ColorType,
    quality: u8,
) -> Result<Vec<u8>, String> {
    let too_large = || format!("a {width}x{height} picture is too large for a JPEG");
    let jpeg_width = u16::try_from(width).map_err(|_| too_large())?;
    let jpeg_height = u16::try_from(height).map_err(|_| too_large())?;
    let mut bytes = Vec::new();
    jpeg_encoder::Encoder::new(&mut bytes, quality)
        .encode(pixels, jpeg_width, jpeg_height, color)
        .map_err(|error| format!("could not encode the screen as JPEG: {error}"))?;
    Ok(bytes)
}

/// Fixed-point unit of the box filter's weights: every target pixel's weights
/// sum to exactly this.
const WEIGHT_ONE: u32 = 1 << 16;

/// For each target index, the first source index it covers and the weights of
/// the source pixels from there on: the share of the target pixel's span each
/// one covers (area averaging).
fn box_taps(source: usize, target: usize) -> Vec<(usize, Vec<u32>)> {
    let scale = source as f64 / target as f64;
    (0..target)
        .map(|index| {
            let start = index as f64 * scale;
            let end = ((index + 1) as f64 * scale).min(source as f64);
            let first = (start.floor() as usize).min(source - 1);
            let last = (end.ceil() as usize).clamp(first + 1, source);
            let span = (end - start).max(f64::EPSILON);
            let mut weights: Vec<u32> = (first..last)
                .map(|position| {
                    let covered = end.min((position + 1) as f64) - start.max(position as f64);
                    (covered.max(0.0) / span * f64::from(WEIGHT_ONE)).round() as u32
                })
                .collect();
            // Rounding leaves the sum a little off; the largest share absorbs it, so a
            // flat colour stays exactly that colour.
            let sum: u32 = weights.iter().sum();
            if let Some(largest) = (0..weights.len()).max_by_key(|&at| weights[at]) {
                weights[largest] = (weights[largest] + WEIGHT_ONE).saturating_sub(sum);
            }
            (first, weights)
        })
        .collect()
}

/// Downscales opaque RGBA to RGB by area averaging, in fixed point. Fast
/// enough for a 1920×1080 desktop at the stream's ten frames a second; the
/// model's picture uses the slower, sharper resampler instead.
///
/// Each target row is the weighted sum of the source rows it covers, each of
/// those filtered horizontally first. A boundary row shared by two target rows
/// is filtered twice, which costs less than keeping a window of filtered rows.
pub fn box_downscale_rgb(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_width: u32,
    target_height: u32,
) -> Vec<u8> {
    let (width, height) = (width as usize, height as usize);
    let (target_width, target_height) = (target_width as usize, target_height as usize);
    if width == 0 || height == 0 || target_width == 0 || target_height == 0 {
        return Vec::new();
    }
    let columns = box_taps(width, target_width);
    let rows = box_taps(height, target_height);
    // A filtered row holds each channel in 8.8 fixed point: at most 255 << 8.
    let mut filtered = vec![0_u16; target_width * 3];
    // A weighted sum of those: (255 << 8) * WEIGHT_ONE at most, which only just
    // fits a u32; a u64 leaves the rounding of the weights no way to overflow.
    let mut accumulated = vec![0_u64; target_width * 3];
    let mut output = Vec::with_capacity(target_width * target_height * 3);
    for (first_row, row_weights) in &rows {
        accumulated.fill(0);
        for (offset, &row_weight) in row_weights.iter().enumerate() {
            let row = first_row + offset;
            let source = &rgba[row * width * 4..(row + 1) * width * 4];
            for ((first_column, weights), out) in columns.iter().zip(filtered.chunks_exact_mut(3)) {
                let mut sums = [0_u32; 3];
                for (index, &weight) in weights.iter().enumerate() {
                    let pixel = &source[(first_column + index) * 4..][..3];
                    sums[0] += u32::from(pixel[0]) * weight;
                    sums[1] += u32::from(pixel[1]) * weight;
                    sums[2] += u32::from(pixel[2]) * weight;
                }
                for (target, sum) in out.iter_mut().zip(sums) {
                    *target = ((sum + (1 << 7)) >> 8) as u16;
                }
            }
            for (sum, &value) in accumulated.iter_mut().zip(&filtered) {
                *sum += u64::from(value) * u64::from(row_weight);
            }
        }
        output.extend(
            accumulated
                .iter()
                .map(|&sum| ((sum + (1 << 23)) >> 24).min(255) as u8),
        );
    }
    output
}

/// What the hub's pump should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// A pulling driver should capture a frame: someone is watching.
    Pull,
    /// A pushing driver's picture changed and someone is watching: encode it.
    Encode,
    Closed,
}

/// The most of pixels a viewer that has not drawn yet is left holding, in patches. A viewer this
/// far behind gets the whole picture instead: a window that is not drawing must not cost memory.
const LIVE_INBOX_LIMIT: usize = 128 * 1024 * 1024;

/// One viewer in this process (the preview pane, drawing with the GPU): what it has not drawn yet.
pub struct LiveScreen {
    inbox: Mutex<Vec<ScreenUpdate>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl LiveScreen {
    fn new(wake: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { inbox: Mutex::new(Vec::new()), wake })
    }

    /// What arrived since the last call, in order. A whole picture replaces all before it.
    pub fn take(&self) -> Vec<ScreenUpdate> {
        std::mem::take(&mut *lock(&self.inbox))
    }

    /// Takes an update and answers whether there is still room for what follows.
    fn deliver(&self, update: &ScreenUpdate) -> bool {
        let fits = {
            let mut inbox = lock(&self.inbox);
            if matches!(update, ScreenUpdate::Full { .. }) {
                inbox.clear();
                inbox.push(update.clone());
                true
            } else if inbox.iter().map(ScreenUpdate::bytes).sum::<usize>() + update.bytes() > LIVE_INBOX_LIMIT {
                // Nothing here can be built on any more.
                inbox.clear();
                false
            } else {
                inbox.push(update.clone());
                true
            }
        };
        if fits {
            (self.wake)();
        }
        fits
    }
}

struct HubState {
    latest: Option<ViewerFrame>,
    /// The seq handed to viewers. It counts up for the session's whole life, so
    /// a reconnected driver that numbers its frames from zero again does not
    /// leave a viewer waiting for a seq above the old connection's.
    seq: u64,
    /// The driver frame `latest` was encoded from: connection and its seq.
    source: Option<(u64, u64)>,
    /// A pushed frame arrived that nobody has taken yet.
    dirty: bool,
    /// Viewers in this process. They are watching for as long as they are here.
    live: Vec<Arc<LiveScreen>>,
    /// Someone among them has no picture to build patches on.
    live_wants_full: bool,
    /// The newest whole picture, for a viewer arriving while a stream has nothing new to say.
    live_last_full: Option<ScreenUpdate>,
    /// The driver frame last given whole to them.
    live_source: Option<(u64, u64)>,
    /// The connected driver pulls (adb, WebDriverAgent): capture while watched.
    pulling: bool,
    visible: HashSet<String>,
    /// Streams blocked in [`FrameHub::next_frame`] right now.
    waiting: usize,
    last_wait: Option<Instant>,
    closed: bool,
}

impl HubState {
    fn page_watching(&self) -> bool {
        !self.visible.is_empty()
            && (self.waiting > 0 || self.last_wait.is_some_and(|at| at.elapsed() < STREAM_GRACE))
    }

    fn watched(&self) -> bool {
        !self.live.is_empty() || self.page_watching()
    }
}

/// The encoded frames of one session, shared by every viewer page, and the
/// bookkeeping that decides whether frames are worth producing at all: only a
/// visible page with an open stream counts (a hidden pane would otherwise keep
/// a phone capturing and this computer encoding for nobody).
pub struct FrameHub {
    state: Mutex<HubState>,
    /// A new frame was published, or the hub closed.
    published: Condvar,
    /// The pump has something to do, or the hub closed.
    work: Condvar,
}

impl Default for FrameHub {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameHub {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HubState {
                latest: None,
                seq: 0,
                source: None,
                dirty: false,
                live: Vec::new(),
                live_wants_full: false,
                live_last_full: None,
                live_source: None,
                pulling: false,
                visible: HashSet::new(),
                waiting: 0,
                last_wait: None,
                closed: false,
            }),
            published: Condvar::new(),
            work: Condvar::new(),
        }
    }

    /// Blocks up to `timeout` for something to do; `None` when nothing came.
    pub fn wait_for_work(&self, timeout: Duration) -> Option<Work> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        loop {
            if state.closed {
                return Some(Work::Closed);
            }
            if state.watched() {
                if state.pulling {
                    return Some(Work::Pull);
                }
                if state.dirty || state.live_wants_full {
                    state.dirty = false;
                    return Some(Work::Encode);
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            state = self
                .work
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    /// Sleeps `duration` unless the hub closes first. Returns whether it is
    /// still open.
    pub fn pause(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut state = lock(&self.state);
        while !state.closed {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            state = self
                .work
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        false
    }

    /// A pushing driver has a new picture.
    pub fn mark_dirty(&self) {
        lock(&self.state).dirty = true;
        self.work.notify_all();
    }

    /// Whether the connected driver pulls its frames (`false` while none is
    /// connected).
    pub fn set_pulling(&self, pulling: bool) {
        let mut state = lock(&self.state);
        state.pulling = pulling;
        if !pulling {
            state.dirty = true;
        }
        drop(state);
        self.work.notify_all();
    }

    pub fn set_visible(&self, viewer: &str, visible: bool) {
        let mut state = lock(&self.state);
        if visible {
            if state.visible.len() < MAX_VISIBLE_PAGES || state.visible.contains(viewer) {
                state.visible.insert(viewer.to_owned());
            }
        } else {
            state.visible.remove(viewer);
        }
        drop(state);
        self.work.notify_all();
    }

    pub fn viewer_gone(&self, viewer: &str) {
        self.set_visible(viewer, false);
    }

    /// Whether frame `frame_seq` of connection `connection` is newer than what
    /// the hub already has, so encoding it is not wasted.
    pub fn wants(&self, connection: u64, frame_seq: u64) -> bool {
        lock(&self.state).source != Some((connection, frame_seq))
    }

    /// A viewer in this process starts watching: frames flow until [`Self::unwatch`]. `wake` is
    /// called, from the pump's thread, whenever it has something new to [`LiveScreen::take`].
    pub fn watch(&self, wake: Box<dyn Fn() + Send + Sync>) -> Arc<LiveScreen> {
        let screen = LiveScreen::new(wake);
        let mut state = lock(&self.state);
        if let Some(full) = &state.live_last_full {
            screen.deliver(full);
        }
        state.live.push(screen.clone());
        state.live_wants_full = true;
        state.live_source = None;
        drop(state);
        self.work.notify_all();
        screen
    }

    pub fn unwatch(&self, screen: &Arc<LiveScreen>) {
        let mut state = lock(&self.state);
        state.live.retain(|live| !Arc::ptr_eq(live, screen));
        // Nobody is left to arrive into it, and a phone's picture is ten megabytes.
        if state.live.is_empty() {
            state.live_last_full = None;
        }
        drop(state);
        self.work.notify_all();
    }

    pub fn live_watching(&self) -> bool {
        !lock(&self.state).live.is_empty()
    }

    pub fn page_watching(&self) -> bool {
        lock(&self.state).page_watching()
    }

    /// Whether the next update has to be a whole picture. Asking answers it.
    pub fn take_live_wants_full(&self) -> bool {
        std::mem::take(&mut lock(&self.state).live_wants_full)
    }

    /// Hands an update to every viewer in this process.
    pub fn publish_live(&self, update: &ScreenUpdate) {
        let live = {
            let mut state = lock(&self.state);
            if matches!(update, ScreenUpdate::Full { .. }) {
                state.live_last_full = Some(update.clone());
            } else {
                // The picture has moved on from the last whole one.
                state.live_last_full = None;
            }
            state.live.clone()
        };
        let mut overflowed = false;
        for screen in &live {
            overflowed |= !screen.deliver(update);
        }
        if overflowed {
            lock(&self.state).live_wants_full = true;
            self.work.notify_all();
        }
    }

    /// Whether frame `frame_seq` of connection `connection` is new to the viewers in this process.
    /// Asking remembers it.
    pub fn live_wants(&self, connection: u64, frame_seq: u64) -> bool {
        let mut state = lock(&self.state);
        if state.live_source == Some((connection, frame_seq)) {
            return false;
        }
        state.live_source = Some((connection, frame_seq));
        true
    }

    /// Stores an encoded frame and wakes every stream waiting for one.
    pub fn publish(&self, connection: u64, frame_seq: u64, jpeg: Vec<u8>, width: u32, height: u32) {
        // The size travels in the JPEG; the frame records it for tests only.
        #[cfg(not(test))]
        let _ = (width, height);
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.seq += 1;
        state.source = Some((connection, frame_seq));
        state.latest = Some(ViewerFrame {
            jpeg: Arc::from(jpeg),
            #[cfg(test)]
            width,
            #[cfg(test)]
            height,
            seq: state.seq,
        });
        drop(state);
        self.published.notify_all();
    }

    pub fn latest(&self) -> Option<ViewerFrame> {
        lock(&self.state).latest.clone()
    }

    /// Blocks up to `timeout` for a frame whose seq is above `after`. A stream
    /// waiting here is what makes the session produce frames, so the pump is
    /// woken on the way in.
    pub fn next_frame(&self, after: u64, timeout: Duration) -> Option<ViewerFrame> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.state);
        state.waiting += 1;
        state.last_wait = Some(Instant::now());
        self.work.notify_all();
        let found = loop {
            if let Some(frame) = state.latest.as_ref().filter(|frame| frame.seq > after) {
                break Some(frame.clone());
            }
            let now = Instant::now();
            if state.closed || now >= deadline {
                break None;
            }
            state = self
                .published
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        };
        state.waiting = state.waiting.saturating_sub(1);
        state.last_wait = Some(Instant::now());
        found
    }

    /// Ends the hub: the pump stops and every waiting stream returns.
    pub fn close(&self) {
        lock(&self.state).closed = true;
        self.published.notify_all();
        self.work.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    fn frame(width: u32, height: u32, color: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let [r, g, b] = color(x, y);
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        Frame {
            width,
            height,
            rgba: Arc::from(rgba),
            seq: 1,
            captured_at: Instant::now(),
        }
    }

    fn decode(jpeg: &[u8]) -> (usize, usize, Vec<u8>) {
        use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(jpeg, options);
        let pixels = decoder.decode().expect("a valid JPEG");
        let (width, height) = decoder.dimensions().expect("dimensions");
        (width, height, pixels)
    }

    #[test]
    fn the_model_frame_caps_the_long_edge_at_1280_and_never_scales_up() {
        assert_eq!(model_frame(1080, 2400), (576, 1280));
        assert_eq!(model_frame(2880, 1800), (1280, 800));
        assert_eq!(model_frame(800, 600), (800, 600));
        assert_eq!(model_frame(1280, 720), (1280, 720));
        assert_eq!(model_frame(0, 0), (0, 0));
        assert_eq!(fit_long_edge(1920, 1080, VIEWER_LONG_EDGE), (1600, 900));
        assert_eq!(fit_long_edge(100_000, 1, VIEWER_LONG_EDGE), (1600, 1));
    }

    #[test]
    fn frame_pixels_map_back_onto_the_screen_and_clamp_at_its_edge() {
        let screen = (1080, 2400);
        let frame = model_frame(1080, 2400);
        assert_eq!(frame_to_screen((288, 640), frame, screen), (540, 1200));
        assert_eq!(frame_to_screen((0, 0), frame, screen), (0, 0));
        assert_eq!(frame_to_screen((576, 1280), frame, screen), (1079, 2399));
        assert_eq!(
            frame_to_screen((400, 300), (800, 600), (800, 600)),
            (400, 300)
        );
        assert_eq!(screen_to_frame((540, 1200), screen, frame), (288, 640));
        assert_eq!(screen_to_frame((-90, 0), screen, frame), (-48, 0));
    }

    #[test]
    fn box_filtering_keeps_a_flat_colour_and_averages_a_checkerboard() {
        let flat = frame(1920, 1080, |_, _| [12, 200, 77]);
        let rgb = box_downscale_rgb(&flat.rgba, 1920, 1080, 1600, 900);
        assert_eq!(rgb.len(), 1600 * 900 * 3);
        assert!(rgb.chunks_exact(3).all(|pixel| pixel == [12, 200, 77]));

        let checker = frame(8, 8, |x, y| {
            if (x + y) % 2 == 0 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        });
        let rgb = box_downscale_rgb(&checker.rgba, 8, 8, 4, 4);
        assert!(
            rgb.iter().all(|&value| (127..=128).contains(&value)),
            "{rgb:?}"
        );
    }

    #[test]
    fn the_viewers_jpeg_is_downscaled_to_1600_and_decodes() {
        let picture = frame(1920, 1080, |x, y| [(x % 256) as u8, (y % 256) as u8, 90]);
        let (jpeg, width, height) = encode_for_viewer(&picture).unwrap();
        assert_eq!((width, height), (1600, 900));
        let (decoded_width, decoded_height, pixels) = decode(&jpeg);
        assert_eq!((decoded_width, decoded_height), (1600, 900));
        assert_eq!(pixels.len(), 1600 * 900 * 3);

        let small = frame(640, 480, |_, _| [10, 20, 30]);
        let (jpeg, width, height) = encode_for_viewer(&small).unwrap();
        assert_eq!((width, height), (640, 480));
        let (_, _, pixels) = decode(&jpeg);
        let pixel = &pixels[..3];
        assert!(
            pixel
                .iter()
                .zip([10, 20, 30])
                .all(|(&got, want)| got.abs_diff(want) <= 3),
            "{pixel:?}"
        );
    }

    #[test]
    fn the_models_jpeg_is_the_coordinate_frame() {
        let picture = frame(
            1080,
            2400,
            |x, _| if x < 540 { [255, 0, 0] } else { [0, 0, 255] },
        );
        let (jpeg, width, height) = encode_for_model(&picture).unwrap();
        assert_eq!((width, height), (576, 1280));
        let (decoded_width, decoded_height, pixels) = decode(&jpeg);
        assert_eq!((decoded_width, decoded_height), (576, 1280));
        // Left half red, right half blue, after resampling and compression.
        let at = |x: usize, y: usize| &pixels[(y * 576 + x) * 3..][..3];
        assert!(at(100, 640)[0] > 200 && at(100, 640)[2] < 60);
        assert!(at(500, 640)[2] > 200 && at(500, 640)[0] < 60);
    }

    /// A picture too busy for the byte budget at the usual quality is
    /// re-encoded at a lower one, at the same size: the coordinate frame.
    #[test]
    fn a_noisy_screen_fits_the_byte_budget_at_the_size_of_the_frame() {
        // Every pixel its own colour: the worst a JPEG can be asked to hold.
        let picture = frame(1280, 800, |x, y| {
            let mut hash = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
            hash ^= hash >> 15;
            hash = hash.wrapping_mul(0x2c1b_3c6d);
            hash ^= hash >> 12;
            hash = hash.wrapping_mul(0x297a_2d39);
            hash ^= hash >> 15;
            let [r, g, b, _] = hash.to_le_bytes();
            [r, g, b]
        });
        let budget = crate::image_attachments::MODEL_IMAGE_BYTE_BUDGET;
        let (width, height) = model_frame(1280, 800);
        let pixels = crate::image_attachments::resize(&picture.rgba, 1280, 800, width, height);
        let usual = encode_jpeg(
            width,
            height,
            &pixels,
            jpeg_encoder::ColorType::Rgba,
            MODEL_JPEG_QUALITY,
        )
        .unwrap();
        assert!(usual.len() > budget, "{} bytes", usual.len());

        let (jpeg, encoded_width, encoded_height) = encode_for_model(&picture).unwrap();
        assert_eq!((encoded_width, encoded_height), (1280, 800));
        assert!(jpeg.len() <= budget, "{} bytes", jpeg.len());
        let (decoded_width, decoded_height, _) = decode(&jpeg);
        assert_eq!((decoded_width, decoded_height), (1280, 800));
    }

    /// A close-up is the area's own pixels, enlarged to exactly the size asked for; an area off
    /// the screen, or a size past the model's sides, is refused.
    #[test]
    fn a_close_up_is_its_area_at_exactly_its_size() {
        let picture = frame(400, 200, |x, y| match (x < 200, y < 100) {
            (true, true) => [255, 0, 0],
            (false, true) => [0, 255, 0],
            (true, false) => [0, 0, 255],
            (false, false) => [255, 255, 255],
        });
        let area = super::super::driver::Rect {
            x: 220,
            y: 10,
            width: 100,
            height: 50,
        };
        let jpeg = encode_area_for_model(&picture, area, (200, 100)).unwrap();
        let (width, height, pixels) = decode(&jpeg);
        assert_eq!((width, height), (200, 100));
        // Wholly inside the green quadrant.
        for (x, y) in [(5usize, 5usize), (100, 50), (195, 95)] {
            let pixel = &pixels[(y * 200 + x) * 3..][..3];
            assert!(
                pixel[1] > 220 && pixel[0] < 40 && pixel[2] < 40,
                "{pixel:?}"
            );
        }
        let past = super::super::driver::Rect {
            x: 350,
            y: 0,
            width: 100,
            height: 10,
        };
        assert!(encode_area_for_model(&picture, past, (100, 10)).is_err());
        let empty = super::super::driver::Rect { width: 0, ..area };
        assert!(encode_area_for_model(&picture, empty, (10, 10)).is_err());
        assert!(encode_area_for_model(&picture, area, (5_000, 10)).is_err());
        assert!(encode_area_for_model(&picture, area, (0, 10)).is_err());
    }

    #[test]
    fn a_frame_whose_buffer_does_not_match_its_size_is_refused() {
        let mut broken = frame(4, 4, |_, _| [0, 0, 0]);
        broken.height = 5;
        assert!(encode_for_viewer(&broken).is_err());
        assert!(encode_for_model(&broken).is_err());
        broken.width = 0;
        assert!(check_frame(&broken).is_err());
    }

    #[test]
    fn the_hub_shares_one_frame_and_wakes_a_waiting_stream() {
        let hub = Arc::new(FrameHub::new());
        assert!(hub.latest().is_none());
        assert!(hub.next_frame(0, Duration::from_millis(20)).is_none());
        let waiting = {
            let hub = hub.clone();
            thread::spawn(move || hub.next_frame(0, Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(50));
        assert!(hub.wants(1, 7));
        hub.publish(1, 7, vec![1, 2, 3], 10, 20);
        let frame = waiting.join().unwrap().expect("a frame");
        assert_eq!((frame.seq, frame.width, frame.height), (1, 10, 20));
        assert!(!hub.wants(1, 7));
        assert!(hub.wants(2, 7));
        // A reconnected driver's seq starts over; the viewer's still counts up.
        hub.publish(2, 1, vec![4], 10, 20);
        assert_eq!(hub.next_frame(1, Duration::from_millis(10)).unwrap().seq, 2);
        assert_eq!(hub.latest().unwrap().seq, 2);
    }

    #[test]
    fn work_comes_only_while_a_visible_page_has_a_stream() {
        let hub = Arc::new(FrameHub::new());
        hub.set_pulling(true);
        assert_eq!(hub.wait_for_work(Duration::from_millis(20)), None);
        hub.set_visible("page", true);
        // Visible, but no stream has asked for a frame yet.
        assert_eq!(hub.wait_for_work(Duration::from_millis(20)), None);
        let stream = {
            let hub = hub.clone();
            thread::spawn(move || hub.next_frame(0, Duration::from_millis(300)))
        };
        assert_eq!(hub.wait_for_work(Duration::from_secs(2)), Some(Work::Pull));
        stream.join().unwrap();
        hub.set_pulling(false);
        // Not pulling: work only when a pushed frame is waiting.
        assert_eq!(
            hub.wait_for_work(Duration::from_millis(20)),
            Some(Work::Encode)
        );
        assert_eq!(hub.wait_for_work(Duration::from_millis(20)), None);
        hub.mark_dirty();
        assert_eq!(
            hub.wait_for_work(Duration::from_millis(20)),
            Some(Work::Encode)
        );
        hub.viewer_gone("page");
        hub.mark_dirty();
        assert_eq!(hub.wait_for_work(Duration::from_millis(20)), None);
        hub.close();
        assert_eq!(
            hub.wait_for_work(Duration::from_secs(1)),
            Some(Work::Closed)
        );
        assert!(!hub.pause(Duration::from_secs(1)));
    }
}
