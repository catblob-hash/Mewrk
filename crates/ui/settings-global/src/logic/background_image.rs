//! `lib/backgroundImage.ts`: imported window backgrounds, a picked picture cut into a ladder of sizes and kept by the host.
//!
//! Resizing is an exact area average in linear light, the arithmetic upstream runs so that fine detail neither aliases
//! nor darkens. Nothing is invented: a picture smaller than the window is shown at the size it is.

use std::path::Path;

pub use mewrk_motion::pixels::{area_resize, Pixels};

/// Long-edge sizes of the ladder. The top is where a 6K window stops needing more.
const BACKGROUND_LADDER: [u32; 7] = [640, 1280, 1920, 2560, 3840, 5120, 7680];
const MAX_LONG_EDGE: u32 = BACKGROUND_LADDER[BACKGROUND_LADDER.len() - 1];
const JPEG_QUALITY: u8 = 90;

pub struct EncodedTier {
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

fn round(v: f64) -> u32 {
    v.round() as u32
}

/// Sizes of every tier for a picture of this size, largest first.
pub fn background_ladder(width: u32, height: u32) -> Vec<(u32, u32)> {
    let long_edge = width.max(height) as f64;
    let top_scale = (MAX_LONG_EDGE as f64 / long_edge).min(1.0);
    let top = (round(width as f64 * top_scale).max(1), round(height as f64 * top_scale).max(1));
    let top_long = top.0.max(top.1) as f64;
    let mut sizes = vec![top];
    for &edge in BACKGROUND_LADDER.iter().rev() {
        // A rung within a tenth of the one above it would be a second copy of it.
        if edge as f64 * 1.1 > top_long {
            continue;
        }
        let scale = edge as f64 / long_edge;
        sizes.push((round(width as f64 * scale).max(1), round(height as f64 * scale).max(1)));
    }
    sizes
}

/// Whether a tier already covers a window of this many device pixels.
pub fn tier_covers(tier: (u32, u32), width: f64, height: f64) -> bool {
    tier.0 as f64 >= width.round() && tier.1 as f64 >= height.round()
}

/// The size a picture of this size has once shrunk to just cover a window of this many device pixels, never above its own.
pub fn cover_size(picture: (u32, u32), width: f64, height: f64) -> (u32, u32) {
    let scale = (width / f64::from(picture.0.max(1))).max(height / f64::from(picture.1.max(1))).min(1.0);
    let side = |side: u32| ((f64::from(side) * scale).ceil() as u32).clamp(1, side.max(1));
    (side(picture.0), side(picture.1))
}

/// A picture shrunk to just cover the window, or as it is when it is no larger. The tier that covers a window can be half again as wide
/// and as tall, and the built-in pictures are 3840 wide for any window: on the GPU, a 1440x900 window's backdrop took 35 MiB where 20 do.
pub fn cover_window(pixels: Pixels, width: f64, height: f64) -> Result<Pixels, String> {
    let (w, h) = cover_size((pixels.width, pixels.height), width, height);
    if (w, h) == (pixels.width, pixels.height) {
        return Ok(pixels);
    }
    area_resize(&pixels, w, h)
}

pub fn is_opaque(pixels: &Pixels) -> bool {
    pixels.data.chunks_exact(4).all(|p| p[3] == 255)
}

fn encode(pixels: &Pixels, opaque: bool) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    if opaque {
        let rgb: Vec<u8> = pixels.data.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY).write_image(&rgb, pixels.width, pixels.height, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())?;
    } else {
        image::codecs::png::PngEncoder::new(&mut out).write_image(&pixels.data, pixels.width, pixels.height, image::ExtendedColorType::Rgba8).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Decodes a file, following its EXIF orientation. The message is `unreadable` for what `image` has no decoder for.
pub fn decode(bytes: &[u8]) -> Result<Pixels, String> {
    use image::ImageDecoder;
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|_| "unreadable".to_owned())?;
    let mut decoder = reader.into_decoder().map_err(|_| "unreadable".to_owned())?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut decoded = image::DynamicImage::from_decoder(decoder).map_err(|_| "unreadable".to_owned())?;
    decoded.apply_orientation(orientation);
    let rgba = decoded.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(Pixels { width, height, data: rgba.into_raw() })
}

/// Every tier of a picture, smallest first. Each is shrunk from the one above it.
pub fn build_background_tiers(bytes: &[u8]) -> Result<Vec<EncodedTier>, String> {
    tiers_of(decode(bytes)?)
}

/// The same for pixels already in memory: the first frame of something that moves.
pub fn tiers_of(decoded: Pixels) -> Result<Vec<EncodedTier>, String> {
    let ladder = background_ladder(decoded.width, decoded.height);
    let top = ladder[0];
    let top_pixels = if (decoded.width, decoded.height) == top { decoded } else { area_resize(&decoded, top.0, top.1)? };
    let opaque = is_opaque(&top_pixels);
    let mut tiers = Vec::new();
    let mut previous = top_pixels;
    for &(w, h) in &ladder {
        if (w, h) != (previous.width, previous.height) {
            previous = area_resize(&previous, w, h)?;
        }
        tiers.push(EncodedTier { width: w, height: h, bytes: encode(&previous, opaque)? });
    }
    tiers.reverse();
    Ok(tiers)
}

/// The bytes of a picked file, or why not. Separate so that a test can import from a path it wrote.
pub fn read_picture(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if image::guess_format(&bytes).is_ok() {
        return Ok(bytes);
    }
    // No pure-Rust decoder reads HEIC, which an iPhone's photos arrive in, and upstream gets it from the browser engine. `sips` ships with
    // macOS and reads what ImageIO reads. Anywhere else, or when it can't, the bytes go on and fail as unreadable.
    Ok(converted_with_sips(path).unwrap_or(bytes))
}

#[cfg(target_os = "macos")]
fn converted_with_sips(path: &Path) -> Option<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("mewrk-background-{}-{:016x}.png", std::process::id(), fastrand::u64(..)));
    let converted = std::process::Command::new("/usr/bin/sips").args(["-s", "format", "png"]).arg(path).arg("--out").arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    let bytes = if converted { std::fs::read(&out).ok() } else { None };
    let _ = std::fs::remove_file(&out);
    bytes
}

#[cfg(not(target_os = "macos"))]
fn converted_with_sips(_path: &Path) -> Option<Vec<u8>> {
    None
}

pub fn base64_encode(bytes: &[u8]) -> String {
    base64_simd::STANDARD.encode_to_string(bytes)
}

/// The payload of a `data:` URL, as pixels.
pub fn decode_data_url(data_url: &str) -> Result<Pixels, String> {
    let (_, payload) = data_url.split_once(";base64,").ok_or_else(|| "not a data URL".to_owned())?;
    let bytes = base64_simd::STANDARD.decode_to_vec(payload).map_err(|e| e.to_string())?;
    decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_is_kept_just_large_enough_to_cover_the_window() {
        assert_eq!(cover_size((3840, 2400), 2880.0, 1800.0), (2880, 1800));
        assert_eq!(cover_size((3840, 2160), 2880.0, 1800.0), (3200, 1800));
        assert_eq!(cover_size((3840, 2400), 5120.0, 2880.0), (3840, 2400), "never above its own size");
        for (window, picture) in [((2881.4, 1799.6), (3840, 2400)), ((1000.5, 333.3), (1921, 1081)), ((640.0, 2000.0), (7680, 4320))] {
            let size = cover_size(picture, window.0, window.1);
            assert!(tier_covers(size, window.0, window.1), "{size:?} covers {window:?}");
        }
        let pixels = Pixels { width: 8, height: 4, data: vec![255; 8 * 4 * 4] };
        let fitted = cover_window(pixels, 4.0, 1.0).unwrap();
        assert_eq!((fitted.width, fitted.height), (4, 2));
        assert!(is_opaque(&fitted));
    }

    #[test]
    fn what_cannot_be_read_goes_on_as_it_is_and_fails_as_unreadable_later() {
        let path = std::env::temp_dir().join(format!("mewrk-not-a-picture-{}.heic", std::process::id()));
        std::fs::write(&path, b"not a picture").unwrap();
        let bytes = read_picture(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes, b"not a picture");
        assert_eq!(decode(&bytes).err().as_deref(), Some("unreadable"));
    }

    // backgroundImage.test.ts
    #[test]
    fn the_ladder_has_a_rung_for_every_size_below_the_picture() {
        assert_eq!(background_ladder(3840, 2160), vec![(3840, 2160), (2560, 1440), (1920, 1080), (1280, 720), (640, 360)]);
    }

    #[test]
    fn a_small_picture_stays_its_size_and_a_huge_one_is_capped() {
        assert_eq!(background_ladder(100, 50), vec![(100, 50)]);
        assert_eq!(background_ladder(16000, 8000)[0], (7680, 3840));
    }

    #[test]
    fn a_rung_within_a_tenth_of_the_top_is_skipped() {
        let ladder = background_ladder(2000, 1000);
        assert_eq!(ladder[0], (2000, 1000));
        assert_eq!(ladder[1], (1280, 640));
    }

    #[test]
    fn an_exact_half_keeps_the_grey_a_checkerboard_looks_like() {
        let mut data = Vec::new();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let v = if (x + y) % 2 == 0 { 255 } else { 0 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let out = area_resize(&Pixels { width: 4, height: 4, data }, 2, 2).unwrap();
        // Linear-light average of black and white: sRGB 188, not the 128 a gamma-space average darkens it to.
        assert!((out.data[0] as i32 - 188).abs() <= 1, "{}", out.data[0]);
        assert_eq!(out.data[3], 255);
    }

    #[test]
    fn a_transparent_pixels_hidden_colour_does_not_bleed() {
        let data = vec![255, 0, 0, 0, 0, 255, 0, 255];
        let out = area_resize(&Pixels { width: 2, height: 1, data }, 1, 1).unwrap();
        assert_eq!(out.data[0], 0);
        assert_eq!(out.data[1], 255);
        assert_eq!(out.data[3], 128);
    }

    #[test]
    fn growing_is_refused() {
        assert!(area_resize(&Pixels { width: 1, height: 1, data: vec![0; 4] }, 2, 2).is_err());
    }

    #[test]
    fn tiers_round_trip_through_the_encoders_smallest_first() {
        let width = 800;
        let height = 400;
        let data: Vec<u8> = (0..width * height).flat_map(|i| [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8, 255]).collect();
        let png = encode(&Pixels { width, height, data }, false).unwrap();
        let tiers = build_background_tiers(&png).unwrap();
        assert_eq!(tiers.iter().map(|t| (t.width, t.height)).collect::<Vec<_>>(), vec![(640, 320), (800, 400)]);
        // Opaque pictures go out as JPEG.
        assert_eq!(&tiers[0].bytes[..2], &[0xFF, 0xD8]);
        assert!(decode(b"not a picture").is_err_and(|e| e == "unreadable"));
    }
}
