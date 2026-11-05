//! GIF, APNG and animated WebP: decoded once into a flipbook, so that playing one is the same as playing anything else that is already
//! baked. Whole frames come out of the `image` crate's decoders, which do each format's own compositing.

use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

use image::{AnimationDecoder, Frames};

use crate::flipbook::{FlipWriter, Flipbook};
use crate::pixels::{area_resize, Pixels};

/// The most a decoded animation may take on disk. One that would take more is drawn smaller.
const FRAMES_BUDGET: u64 = 1 << 30;
/// The longest side kept: past this a background gains nothing.
const LONG_EDGE: u32 = 3840;
/// What browsers show a frame asking for next to no time for: such a GIF was never meant to run that fast.
const QUICK_FRAME_MS: u32 = 20;
const QUICK_FRAME_SHOWS_MS: u32 = 100;

/// Whether these first bytes are of a format that can be animated at all.
pub fn may_be_animated(head: &[u8]) -> bool {
    head.starts_with(b"GIF8") || head.starts_with(b"\x89PNG\r\n\x1a\n") || (head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP")
}

/// How many frames a GIF, APNG or WebP says it has, from its structure alone. `None` for anything else.
pub fn frame_count(bytes: &[u8]) -> Option<u32> {
    if bytes.starts_with(b"GIF8") {
        gif_frames(bytes)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(png_frames(bytes))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some(webp_frames(bytes))
    } else {
        None
    }
}

/// Walks a GIF's blocks and counts its images.
fn gif_frames(bytes: &[u8]) -> Option<u32> {
    let skip_table = |flags: u8, at: usize| if flags & 0x80 != 0 { at + 3 * (1usize << ((flags & 7) + 1)) } else { at };
    let skip_blocks = |mut at: usize| -> Option<usize> {
        loop {
            let length = *bytes.get(at)? as usize;
            at += 1;
            if length == 0 {
                return Some(at);
            }
            at += length;
        }
    };
    let mut at = skip_table(*bytes.get(10)?, 13);
    let mut count = 0;
    // A file cut off partway still shows what it has.
    loop {
        at = match bytes.get(at) {
            Some(0x21) => match skip_blocks(at + 2) {
                Some(next) => next,
                None => return Some(count),
            },
            Some(0x2C) => {
                count += 1;
                let Some(&flags) = bytes.get(at + 9) else { return Some(count) };
                match skip_blocks(skip_table(flags, at + 10) + 1) {
                    Some(next) => next,
                    None => return Some(count),
                }
            }
            _ => return Some(count),
        };
    }
}

/// `acTL`, which comes before any image data when there is one. No such chunk: one frame.
fn png_frames(bytes: &[u8]) -> u32 {
    let mut at = 8;
    while let Some(header) = bytes.get(at..at + 8) {
        let length = u32::from_be_bytes(header[..4].try_into().expect("four bytes")) as usize;
        match &header[4..8] {
            b"acTL" => return bytes.get(at + 8..at + 12).map_or(1, |raw| u32::from_be_bytes(raw.try_into().expect("four bytes"))),
            b"IDAT" => return 1,
            _ => at += length + 12,
        }
    }
    1
}

/// Each animation frame is an `ANMF` chunk; a still picture has none.
fn webp_frames(bytes: &[u8]) -> u32 {
    let mut at = 12;
    let mut count = 0;
    while let Some(header) = bytes.get(at..at + 8) {
        let length = u32::from_le_bytes(header[4..8].try_into().expect("four bytes")) as usize;
        if &header[..4] == b"ANMF" {
            count += 1;
        }
        // Chunks are padded to an even length.
        at += 8 + length + (length & 1);
    }
    count.max(1)
}

fn frames_of<'a>(path: &Path, kind: &[u8]) -> Result<Frames<'a>, String> {
    let open = || File::open(path).map(BufReader::new).map_err(|e| e.to_string());
    let unreadable = |_| "unreadable".to_owned();
    if kind.starts_with(b"GIF8") {
        Ok(image::codecs::gif::GifDecoder::new(open()?).map_err(unreadable)?.into_frames())
    } else if kind.starts_with(b"\x89PNG") {
        Ok(image::codecs::png::PngDecoder::new(open()?).map_err(unreadable)?.apng().map_err(unreadable)?.into_frames())
    } else {
        Ok(image::codecs::webp::WebPDecoder::new(open()?).map_err(unreadable)?.into_frames())
    }
}

fn head_of(path: &Path) -> Result<[u8; 12], String> {
    use std::io::Read;
    let mut head = [0u8; 12];
    File::open(path).and_then(|mut file| file.read_exact(&mut head)).map_err(|_| "unreadable".to_owned())?;
    Ok(head)
}

/// The first frame, for the library and for what shows until the frames are ready.
pub fn first_frame(path: &Path) -> Result<Pixels, String> {
    let frame = frames_of(path, &head_of(path)?)?.next().ok_or_else(|| "unreadable".to_owned())?.map_err(|_| "unreadable".to_owned())?;
    let buffer = frame.into_buffer();
    let (width, height) = buffer.dimensions();
    Ok(Pixels { width, height, data: buffer.into_raw() })
}

/// How large to keep frames of this size: small enough for the long edge and for the budget.
fn kept_size(width: u32, height: u32, frames: u32) -> (u32, u32) {
    let whole = width as f64 * height as f64 * 4.0 * f64::from(frames.max(1));
    let scale = (FRAMES_BUDGET as f64 / whole).sqrt().min(f64::from(LONG_EDGE) / f64::from(width.max(height))).min(1.0);
    if scale >= 1.0 {
        return (width, height);
    }
    (((f64::from(width) * scale).floor() as u32).max(1), ((f64::from(height) * scale).floor() as u32).max(1))
}

/// Decodes an animated picture into a flipbook at `out`, however long that takes. Call it off the UI thread.
pub fn decode_to_flipbook(source: &Path, out: &Path) -> Result<std::sync::Arc<Flipbook>, String> {
    let count = frame_count(&std::fs::read(source).map_err(|e| e.to_string())?).ok_or_else(|| "unreadable".to_owned())?;
    let first = first_frame(source)?;
    let (width, height) = kept_size(first.width, first.height, count);
    let mut writer = FlipWriter::create(out, width, height, count.max(1))?;
    let mut elapsed_us = 0u64;
    for frame in frames_of(source, &head_of(source)?)? {
        let Ok(frame) = frame else { break };
        let (numerator, denominator) = frame.delay().numer_denom_ms();
        let shown_ms = numerator / denominator.max(1);
        elapsed_us += u64::from(if shown_ms < QUICK_FRAME_MS { QUICK_FRAME_SHOWS_MS } else { shown_ms }) * 1000;
        let buffer = frame.into_buffer();
        if buffer.dimensions() == (width, height) {
            writer.push(buffer.as_raw(), elapsed_us);
        } else {
            let (frame_width, frame_height) = buffer.dimensions();
            let shrunk = area_resize(&Pixels { width: frame_width, height: frame_height, data: buffer.into_raw() }, width, height)?;
            writer.push(&shrunk.data, elapsed_us);
        }
    }
    writer.finish()
}

/// A GIF of solid frames, for tests here and elsewhere: `colors` frames, each `delay_ms` long.
pub fn test_gif(width: u16, height: u16, colors: &[[u8; 3]], delay_ms: u32) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(Cursor::new(&mut out));
        let frames = colors.iter().map(|&[r, g, b]| {
            let buffer = image::RgbaImage::from_pixel(u32::from(width), u32::from(height), image::Rgba([r, g, b, 255]));
            image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(delay_ms, 1))
        });
        encoder.encode_frames(frames).expect("a GIF writes to memory");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("mewrk-anim-{name}-{}-{:x}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn a_gifs_frames_are_counted_from_its_blocks_and_a_still_ones_is_one() {
        assert_eq!(frame_count(&test_gif(4, 4, &[[1, 2, 3], [200, 0, 0], [0, 200, 0]], 100)), Some(3));
        assert_eq!(frame_count(&test_gif(4, 4, &[[1, 2, 3]], 100)), Some(1));
        assert_eq!(frame_count(b"not a picture"), None);
        let gif = test_gif(4, 4, &[[1, 2, 3], [200, 0, 0]], 100);
        assert_eq!(frame_count(&gif[..gif.len() - 8]), Some(2), "a file cut short still counts what it has");
    }

    #[test]
    fn a_plain_png_is_one_frame_and_an_animated_one_says_how_many() {
        let mut still = Vec::new();
        image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut still), &[0u8; 16], 2, 2, image::ExtendedColorType::Rgba8).unwrap();
        assert_eq!(frame_count(&still), Some(1));
        let mut animated = still[..8 + 25].to_vec();
        animated.extend_from_slice(&[0, 0, 0, 8]);
        animated.extend_from_slice(b"acTL");
        animated.extend_from_slice(&[0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(frame_count(&animated), Some(7));
    }

    #[test]
    fn the_frames_of_a_gif_land_in_a_book_with_their_times() {
        let dir = folder("gif");
        let source = dir.join("a.gif");
        std::fs::write(&source, test_gif(8, 6, &[[255, 0, 0], [0, 255, 0], [0, 0, 255]], 50)).unwrap();
        assert_eq!(crate::classify(&source), Ok(crate::FileKind::Animated));
        let book = decode_to_flipbook(&source, &dir.join("a.flip")).unwrap();
        assert_eq!(book.size(), (8, 6));
        assert_eq!(book.count(), 3);
        assert_eq!(book.duration_us(), 150_000);
        assert_eq!(book.index_at(60_000), Some(1));
        let pixel = |index: u32| book.frame(index).unwrap().as_ref()[..4].to_vec();
        assert!(pixel(0)[0] > 200 && pixel(0)[1] < 50, "{:?}", pixel(0));
        assert!(pixel(1)[1] > 200 && pixel(1)[0] < 50, "{:?}", pixel(1));
        assert!(pixel(2)[2] > 200 && pixel(2)[1] < 50, "{:?}", pixel(2));
        assert_eq!(first_frame(&source).unwrap().data[..3], pixel(0)[..3]);
    }

    #[test]
    fn a_frame_asking_for_no_time_shows_for_as_long_as_browsers_show_it() {
        let dir = folder("quick");
        let source = dir.join("a.gif");
        std::fs::write(&source, test_gif(2, 2, &[[255, 0, 0], [0, 255, 0]], 0)).unwrap();
        assert_eq!(decode_to_flipbook(&source, &dir.join("a.flip")).unwrap().duration_us(), 200_000);
    }

    #[test]
    fn a_still_gif_is_a_picture_and_kept_frames_fit_the_budget() {
        let dir = folder("still");
        let source = dir.join("a.gif");
        std::fs::write(&source, test_gif(2, 2, &[[255, 0, 0]], 0)).unwrap();
        assert_eq!(crate::classify(&source), Ok(crate::FileKind::Picture));
        assert_eq!(kept_size(640, 480, 100), (640, 480));
        assert_eq!(kept_size(7680, 3840, 1), (3840, 1920));
        let (width, height) = kept_size(1920, 1080, 2000);
        assert!(u64::from(width) * u64::from(height) * 4 * 2000 <= FRAMES_BUDGET, "{width}x{height}");
    }
}
