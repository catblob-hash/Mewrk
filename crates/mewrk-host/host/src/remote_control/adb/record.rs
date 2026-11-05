//! The phone's screen as video: `screenrecord` writing H.264 to its stdout, read over adb and decoded
//! by the Mac's hardware. `screencap` costs the phone half a second a picture, because it encodes a
//! PNG in software; this is the phone's own video encoder, at the screen's own frame rate.
//!
//! It needs nothing installed on the phone, which is why it is this and not a server pushed to it.
//! What that costs is three things the tool does not do, each made up for here:
//!
//! * It stops after three minutes on most Android versions. The stream ends cleanly and the session
//!   starts another, which shows as a moment without new frames.
//! * It writes nothing until the screen changes, so the first picture comes from one capture.
//! * It keeps its size when the phone turns and letterboxes the picture inside it. The captures that
//!   go on while frames flow notice, and the stream restarts at the new shape.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use super::{lock, AdbDriver};
use crate::remote_control::{
    driver::{DriverError, DriverResult, Frame, ScreenUpdate},
    picture_stream::{length_prefixed, AnnexB, Chunks, H264Decoder, Next, Pictures},
};

/// Enough for sharp text at a phone's resolution, and little enough for adb over Wi-Fi.
const BIT_RATE: u32 = 8_000_000;
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for more of a frame before taking what came as all of it. Frames that
/// follow one another never wait this long, since the next one's start code ends the one before;
/// this is for the last frame before the screen goes still, which nothing would ever end.
const FRAME_SETTLE: Duration = Duration::from_millis(20);
/// How often a stream with nothing to read looks up to see whether it should stop.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// How often, while frames are flowing, a capture checks which way up the phone is.
const SHAPE_CHECK: Duration = Duration::from_secs(2);
/// A long edge the encoder of every phone takes, for one that refused the screen's own size.
const FALLBACK_LONG_EDGE: u32 = 1280;
/// Bad frames before the first good one, past which this stream is not going to work.
const DECODE_FAILURES: u32 = 30;

const SHAPE_UNKNOWN: u8 = 0;
const SHAPE_TALL: u8 = 1;
const SHAPE_WIDE: u8 = 2;

fn shape(width: u32, height: u32) -> u8 {
    if width > height { SHAPE_WIDE } else { SHAPE_TALL }
}

/// `width × height` scaled to fit `long_edge`, in the even numbers an encoder wants.
fn fitted(width: u32, height: u32, long_edge: u32) -> (u32, u32) {
    let scale = f64::from(long_edge) / f64::from(width.max(height).max(1));
    let even = |side: u32| ((f64::from(side) * scale.min(1.0)).round() as u32 / 2 * 2).max(2);
    (even(width), even(height))
}

fn command(size: Option<(u32, u32)>) -> String {
    match size {
        Some((width, height)) => format!("screenrecord --output-format=h264 --bit-rate {BIT_RATE} --size {width}x{height} -"),
        None => format!("screenrecord --output-format=h264 --bit-rate {BIT_RATE} -"),
    }
}

struct Watch {
    /// A capture for a picture to show until the stream's first frame.
    first: Mutex<Option<Frame>>,
    /// The stream's shape, once it has shown a frame.
    stream_shape: AtomicU8,
    frames: AtomicU64,
    turned: AtomicBool,
    done: AtomicBool,
}

/// The captures beside the stream: the first picture, then the phone's shape while frames flow.
fn watch_shape(driver: &AdbDriver, watch: &Watch) {
    if let Ok(frame) = driver.capture(true) {
        *lock(&watch.first) = Some(frame);
    }
    let mut seen = watch.frames.load(Ordering::Acquire);
    let mut checked = Instant::now();
    while !watch.done.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(50));
        if checked.elapsed() < SHAPE_CHECK {
            continue;
        }
        checked = Instant::now();
        let frames = watch.frames.load(Ordering::Acquire);
        if frames == seen {
            continue;
        }
        seen = frames;
        if let Ok(frame) = driver.capture(true) {
            let stream = watch.stream_shape.load(Ordering::Acquire);
            if stream != SHAPE_UNKNOWN && stream != shape(frame.width, frame.height) {
                watch.turned.store(true, Ordering::Release);
            }
        }
    }
}

pub(super) fn stream(driver: &AdbDriver, stop: &(dyn Fn() -> bool + Sync), sink: &mut dyn FnMut(ScreenUpdate)) -> DriverResult<()> {
    let size = *lock(&driver.record_size);
    let connection = driver.server.exec_stream(&driver.serial, &command(size), OPEN_TIMEOUT).map_err(|error| driver.explain(error))?;
    let (reader, _writer, handle) = connection.split();
    let chunks = Chunks::new(reader, handle);
    let watch = Arc::new(Watch { first: Mutex::new(None), stream_shape: AtomicU8::new(SHAPE_UNKNOWN), frames: AtomicU64::new(0), turned: AtomicBool::new(false), done: AtomicBool::new(false) });
    thread::scope(|scope| {
        scope.spawn(|| watch_shape(driver, &watch));
        let result = read_frames(&chunks, &watch, stop, sink);
        watch.done.store(true, Ordering::Release);
        // The encoder turned the screen's own size down: the next try asks for less.
        if result.is_err() && watch.frames.load(Ordering::Acquire) == 0 && size.is_none() {
            if let Some(frame) = lock(&driver.last).as_ref() {
                *lock(&driver.record_size) = Some(fitted(frame.width, frame.height, FALLBACK_LONG_EDGE));
            }
        }
        result
    })
}

fn read_frames(chunks: &Chunks, watch: &Watch, stop: &(dyn Fn() -> bool + Sync), sink: &mut dyn FnMut(ScreenUpdate)) -> DriverResult<()> {
    let mut units = AnnexB::default();
    let mut pictures = Pictures::default();
    let mut decoder: Option<H264Decoder> = None;
    let mut unfinished = false;
    let mut failures = 0_u32;
    // The start of what it wrote, for the message when it writes no video: that is its complaint.
    let mut said: Vec<u8> = Vec::new();

    let mut show = |pictures: &Pictures, decoder: &mut Option<H264Decoder>, slices: Vec<Vec<u8>>, sink: &mut dyn FnMut(ScreenUpdate)| -> DriverResult<()> {
        let (Some(sps), Some(pps)) = (&pictures.sps, &pictures.pps) else {
            return Ok(());
        };
        if !decoder.as_ref().is_some_and(|decoder| decoder.is_for(sps, pps)) {
            *decoder = Some(H264Decoder::new(sps, pps).map_err(DriverError::Failed)?);
        }
        let mut sample = length_prefixed(&slices);
        match decoder.as_mut().expect("just made").decode(&mut sample) {
            Ok(Some(picture)) => {
                watch.stream_shape.store(shape(picture.width, picture.height), Ordering::Release);
                watch.frames.fetch_add(1, Ordering::AcqRel);
                sink(ScreenUpdate::Full { width: picture.width, height: picture.height, rgba: picture.rgba });
            }
            Ok(None) => {}
            // A damaged frame heals at the next key frame, unless no frame has worked yet.
            Err(error) => {
                failures += 1;
                if failures >= DECODE_FAILURES && watch.frames.load(Ordering::Acquire) == 0 {
                    return Err(DriverError::Failed(error));
                }
            }
        }
        Ok(())
    };

    loop {
        if stop() || watch.turned.load(Ordering::Acquire) {
            return Ok(());
        }
        if watch.frames.load(Ordering::Acquire) == 0 {
            if let Some(frame) = lock(&watch.first).take() {
                sink(ScreenUpdate::Full { width: frame.width, height: frame.height, rgba: frame.rgba });
            }
        }
        match chunks.next(if unfinished { FRAME_SETTLE } else { IDLE_WAIT }) {
            Next::Bytes(bytes) => {
                if said.len() < 512 && watch.frames.load(Ordering::Acquire) == 0 {
                    said.extend_from_slice(&bytes[..bytes.len().min(512)]);
                }
                units.push(&bytes);
                while let Some(unit) = units.next_unit() {
                    if let Some(slices) = pictures.push(unit) {
                        show(&pictures, &mut decoder, slices, sink)?;
                    }
                }
                unfinished = true;
            }
            Next::Quiet => {
                if std::mem::take(&mut unfinished) {
                    let last = units.flush().and_then(|unit| pictures.push(unit)).into_iter().chain(pictures.take());
                    for slices in last.collect::<Vec<_>>() {
                        show(&pictures, &mut decoder, slices, sink)?;
                    }
                }
            }
            Next::Ended => {
                // The tool's own end: what it wrote last is still a frame.
                let last = units.flush().and_then(|unit| pictures.push(unit)).into_iter().chain(pictures.take());
                for slices in last.collect::<Vec<_>>() {
                    show(&pictures, &mut decoder, slices, sink)?;
                }
                if watch.frames.load(Ordering::Acquire) > 0 {
                    return Ok(());
                }
                let printable: String = String::from_utf8_lossy(&said).chars().filter(|c| !c.is_control() || *c == ' ').take(200).collect();
                return Err(DriverError::Failed(if printable.trim().is_empty() { "screenrecord ended without writing any video".to_owned() } else { format!("screenrecord ended without writing any video: {}", printable.trim()) }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_size_falls_back_to_even_numbers_in_shape() {
        assert_eq!(fitted(1080, 2400, 1280), (576, 1280));
        assert_eq!(fitted(2400, 1080, 1280), (1280, 576));
        assert_eq!(fitted(1001, 777, 1280), (1000, 776), "smaller than the limit stays, but even");
    }

    #[test]
    fn the_command_writes_raw_h264_to_stdout() {
        assert_eq!(command(None), "screenrecord --output-format=h264 --bit-rate 8000000 -");
        assert_eq!(command(Some((720, 1600))), "screenrecord --output-format=h264 --bit-rate 8000000 --size 720x1600 -");
    }
}
