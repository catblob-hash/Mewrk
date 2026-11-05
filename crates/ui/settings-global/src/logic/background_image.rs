//! `lib/backgroundImage.ts`: imported window backgrounds, a picked picture cut into a ladder of sizes and kept by the host.
//!
//! Resizing is an exact area average in linear light, the arithmetic upstream runs so that fine detail neither aliases
//! nor darkens. Nothing is invented: a picture smaller than the window is shown at the size it is.

use std::path::Path;

/// Long-edge sizes of the ladder. The top is where a 6K window stops needing more.
const BACKGROUND_LADDER: [u32; 7] = [640, 1280, 1920, 2560, 3840, 5120, 7680];
const MAX_LONG_EDGE: u32 = BACKGROUND_LADDER[BACKGROUND_LADDER.len() - 1];
const JPEG_QUALITY: u8 = 90;

/// RGBA, row-major, not premultiplied.
#[derive(Clone)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

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

fn srgb_to_linear_table() -> [f32; 256] {
    let mut table = [0.0f32; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        let channel = value as f64 / 255.0;
        *slot = (if channel <= 0.04045 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }) as f32;
    }
    table
}

const LINEAR_STEPS: usize = 16_383;

fn linear_to_srgb_table() -> Vec<u8> {
    (0..=LINEAR_STEPS)
        .map(|step| {
            let linear = step as f64 / LINEAR_STEPS as f64;
            let channel = if linear <= 0.0031308 { linear * 12.92 } else { 1.055 * linear.powf(1.0 / 2.4) - 0.055 };
            (channel.clamp(0.0, 1.0) * 255.0).round() as u8
        })
        .collect()
}

/// For each output cell along one axis, the source cells it covers and how much of each. Weights of one cell sum to 1.
fn coverage(source: u32, target: u32) -> Vec<Vec<(u32, f32)>> {
    let scale = source as f64 / target as f64;
    (0..target)
        .map(|cell| {
            let start = cell as f64 * scale;
            let end = (((cell + 1) as f64) * scale).min(source as f64);
            let from = start.floor() as u32;
            let to = end.ceil().min(source as f64) as u32;
            (from..to).map(|i| (i, (((i + 1) as f64).min(end) - (i as f64).max(start)) as f32 / scale as f32)).collect()
        })
        .collect()
}

/// Shrinks `source` by averaging exactly the source area under each output pixel, on linear-light, alpha-premultiplied
/// values: a fine black-and-white pattern becomes the grey it looks like, and a transparent pixel's hidden colour does
/// not bleed into its neighbours.
pub fn area_resize(source: &Pixels, width: u32, height: u32) -> Result<Pixels, String> {
    if width > source.width || height > source.height {
        return Err("areaResize only shrinks".to_owned());
    }
    let to_linear = srgb_to_linear_table();
    let to_srgb = linear_to_srgb_table();
    let columns = coverage(source.width, width);
    let rows = coverage(source.height, height);
    let (w, sw) = (width as usize, source.width as usize);
    let mut output = vec![0u8; w * height as usize * 4];
    let mut accumulator = vec![0.0f32; w * 4];
    let mut linear_row = vec![0.0f32; sw * 4];
    let mut filtered_row = vec![0.0f32; w * 4];
    let mut filtered_index: i64 = -1;
    for (y, row) in rows.iter().enumerate() {
        accumulator.fill(0.0);
        for &(source_row, row_weight) in row {
            if filtered_index != source_row as i64 {
                let offset = source_row as usize * sw * 4;
                for x in 0..sw {
                    let pixel = &source.data[offset + x * 4..offset + x * 4 + 4];
                    let alpha = pixel[3] as f32 / 255.0;
                    linear_row[x * 4] = to_linear[pixel[0] as usize] * alpha;
                    linear_row[x * 4 + 1] = to_linear[pixel[1] as usize] * alpha;
                    linear_row[x * 4 + 2] = to_linear[pixel[2] as usize] * alpha;
                    linear_row[x * 4 + 3] = alpha;
                }
                for (cell, column) in columns.iter().enumerate() {
                    let mut sum = [0.0f32; 4];
                    for &(at, weight) in column {
                        for (c, s) in sum.iter_mut().enumerate() {
                            *s += linear_row[at as usize * 4 + c] * weight;
                        }
                    }
                    filtered_row[cell * 4..cell * 4 + 4].copy_from_slice(&sum);
                }
                filtered_index = source_row as i64;
            }
            for (a, f) in accumulator.iter_mut().zip(filtered_row.iter()) {
                *a += f * row_weight;
            }
        }
        let offset = y * w * 4;
        for x in 0..w {
            let alpha = accumulator[x * 4 + 3];
            if alpha <= 0.0 {
                continue;
            }
            let k = LINEAR_STEPS as f32 / alpha;
            for c in 0..3 {
                output[offset + x * 4 + c] = to_srgb[((accumulator[x * 4 + c] * k).round() as usize).min(LINEAR_STEPS)];
            }
            output[offset + x * 4 + 3] = (alpha.min(1.0) * 255.0).round() as u8;
        }
    }
    Ok(Pixels { width, height, data: output })
}

fn is_opaque(pixels: &Pixels) -> bool {
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
    let decoded = decode(bytes)?;
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
