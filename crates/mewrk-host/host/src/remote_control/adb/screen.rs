//! What `screencap -p` printed, as pixels.
//!
//! The PNG may not be all the command wrote: on a device with several displays
//! `screencap` prints a warning line, and over the older raw `exec:` service
//! that line lands in the same stream as the picture. Everything before the PNG
//! signature is therefore dropped. The decoder is told what the largest picture
//! may be before it allocates anything, since the size comes from the device.

use std::io::Cursor;

/// `\x89PNG\r\n\x1a\n`.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// The most RGBA bytes a screen picture may take (8192 x 4096).
const MAX_RGBA_BYTES: u64 = 128 * 1024 * 1024;
/// What the PNG decoder may allocate while decoding.
const MAX_DECODE_BYTES: usize = 192 * 1024 * 1024;

/// One decoded screen.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Picture {
    pub width: u32,
    pub height: u32,
    /// Four bytes per pixel, alpha 255.
    pub rgba: Vec<u8>,
}

/// Where the PNG starts in `output`.
fn png_start(output: &[u8]) -> Option<usize> {
    if output.starts_with(&SIGNATURE) {
        return Some(0);
    }
    output
        .windows(SIGNATURE.len())
        .position(|window| window == SIGNATURE)
}

/// The first line of what a failed `screencap` printed, for an error message.
fn describe_output(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(&output[..output.len().min(400)]);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.is_empty() {
        "nothing".to_owned()
    } else {
        format!(
            "\"{}\"",
            crate::preview_launch_config::sanitize_for_message(line)
        )
    }
}

/// Decodes the PNG in `output` into RGBA.
pub(super) fn decode(output: &[u8]) -> Result<Picture, String> {
    let start = png_start(output).ok_or_else(|| {
        format!(
            "screencap did not produce a PNG (it printed {}). The screen may be off or locked by a secure window; wake the device and try again.",
            describe_output(output)
        )
    })?;
    let mut decoder = png::Decoder::new(Cursor::new(&output[start..]));
    // Palettes, sub-byte grey and 16-bit samples all become 8 bits per channel.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODE_BYTES,
    });
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("screencap's PNG could not be read: {error}"))?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) * 4 > MAX_RGBA_BYTES {
        return Err(format!(
            "the device reported a {width}x{height} screen, which is outside what can be shown"
        ));
    }
    if reader.output_buffer_size() > MAX_DECODE_BYTES {
        return Err("screencap's PNG is larger than can be decoded".to_owned());
    }
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("screencap's PNG could not be decoded: {error}"))?;
    if info.bit_depth != png::BitDepth::Eight {
        return Err("screencap's PNG has an unsupported bit depth".to_owned());
    }
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err("screencap's PNG kept an unexpanded palette".to_owned())
        }
    };
    let row_bytes = width as usize * channels;
    if info.line_size < row_bytes || buffer.len() < info.line_size * height as usize {
        return Err("screencap's PNG is shorter than its header says".to_owned());
    }
    // Alpha starts opaque and stays so; only the colour channels are written.
    let out_row = width as usize * 4;
    let mut rgba = vec![255u8; out_row * height as usize];
    for (row, out) in buffer
        .chunks_exact(info.line_size)
        .zip(rgba.chunks_exact_mut(out_row))
    {
        let row = &row[..row_bytes];
        match channels {
            4 => {
                out.copy_from_slice(row);
                out.chunks_exact_mut(4).for_each(|pixel| pixel[3] = 255);
            }
            3 => {
                for (pixel, source) in out.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
                    pixel[..3].copy_from_slice(source);
                }
            }
            _ => {
                for (pixel, source) in out.chunks_exact_mut(4).zip(row.chunks_exact(channels)) {
                    pixel[..3].fill(source[0]);
                }
            }
        }
    }
    Ok(Picture {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A PNG of the given colour type and depth holding `data`, for tests.
    pub(in crate::remote_control::adb) fn encode(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        palette: Option<&[u8]>,
        data: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if let Some(palette) = palette {
                encoder.set_palette(palette.to_vec());
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
        }
        bytes
    }

    /// A `width` x `height` RGBA picture whose pixel at (x, y) is
    /// `(x, y, x + y, 255)`, and its PNG.
    pub(in crate::remote_control::adb) fn gradient(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[x as u8, y as u8, (x + y) as u8, 255]);
            }
        }
        let png = encode(
            width,
            height,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            None,
            &rgba,
        );
        (png, rgba)
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn push_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    }

    #[test]
    fn an_rgba_png_decodes_to_the_same_pixels() {
        let (png, rgba) = gradient(5, 7);
        let picture = decode(&png).unwrap();
        assert_eq!((picture.width, picture.height), (5, 7));
        assert_eq!(picture.rgba, rgba);
    }

    #[test]
    fn everything_before_the_signature_is_dropped() {
        let (png, rgba) = gradient(3, 3);
        let mut noisy =
            b"[Warning] Multiple displays were found, but no display id was specified!\n".to_vec();
        noisy.extend_from_slice(&png);
        assert_eq!(decode(&noisy).unwrap().rgba, rgba);
    }

    #[test]
    fn rgb_alpha_is_forced_opaque_and_grey_is_widened() {
        let rgb = encode(
            2,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            None,
            &[10, 20, 30, 40, 50, 60],
        );
        assert_eq!(
            decode(&rgb).unwrap().rgba,
            vec![10, 20, 30, 255, 40, 50, 60, 255]
        );

        let translucent = encode(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            None,
            &[1, 2, 3, 4],
        );
        assert_eq!(decode(&translucent).unwrap().rgba, vec![1, 2, 3, 255]);

        let grey = encode(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            None,
            &[7, 200],
        );
        assert_eq!(
            decode(&grey).unwrap().rgba,
            vec![7, 7, 7, 255, 200, 200, 200, 255]
        );

        let grey_alpha = encode(
            1,
            2,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            None,
            &[9, 100, 33, 0],
        );
        assert_eq!(
            decode(&grey_alpha).unwrap().rgba,
            vec![9, 9, 9, 255, 33, 33, 33, 255]
        );
    }

    #[test]
    fn palette_and_sub_byte_and_sixteen_bit_pictures_are_expanded() {
        let indexed = encode(
            3,
            1,
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            Some(&[255, 0, 0, 0, 255, 0, 0, 0, 255]),
            &[2, 0, 1],
        );
        assert_eq!(
            decode(&indexed).unwrap().rgba,
            vec![0, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 255]
        );

        // Four 2-bit palette entries packed into one byte per row.
        let packed = encode(
            4,
            1,
            png::ColorType::Indexed,
            png::BitDepth::Two,
            Some(&[0, 0, 0, 10, 10, 10, 20, 20, 20, 30, 30, 30]),
            &[0b00_01_10_11],
        );
        let picture = decode(&packed).unwrap();
        assert_eq!(
            picture.rgba,
            vec![0, 0, 0, 255, 10, 10, 10, 255, 20, 20, 20, 255, 30, 30, 30, 255]
        );

        // One-bit grey: 1 is white after expansion.
        let bilevel = encode(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            None,
            &[0b10_000000],
        );
        assert_eq!(
            decode(&bilevel).unwrap().rgba,
            vec![255, 255, 255, 255, 0, 0, 0, 255]
        );

        // 16-bit samples keep their high byte.
        let deep = encode(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            None,
            &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc],
        );
        assert_eq!(decode(&deep).unwrap().rgba, vec![0x12, 0x56, 0x9a, 255]);
    }

    #[test]
    fn output_without_a_png_says_what_it_printed() {
        let error = decode(b"/system/bin/sh: screencap: not found\n").unwrap_err();
        assert!(error.contains("did not produce a PNG"), "{error}");
        assert!(error.contains("screencap: not found"), "{error}");
        let empty = decode(b"").unwrap_err();
        assert!(empty.contains("printed nothing"), "{empty}");
    }

    #[test]
    fn a_damaged_or_oversized_png_is_an_error_not_a_crash() {
        let (png, _) = gradient(8, 8);
        let truncated = &png[..png.len() / 2];
        assert!(decode(truncated).is_err());
        let signature_only = SIGNATURE.to_vec();
        assert!(decode(&signature_only).is_err());

        // A header that promises a screen far larger than the cap, with a valid
        // checksum. The size is checked before any pixel is decoded, so the
        // missing picture data is never reached.
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&40_000u32.to_be_bytes());
        ihdr.extend_from_slice(&40_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut forged = SIGNATURE.to_vec();
        push_chunk(&mut forged, b"IHDR", &ihdr);
        push_chunk(&mut forged, b"IDAT", &[0x78, 0x9c, 0x01]);
        let error = decode(&forged).unwrap_err();
        assert!(!error.is_empty(), "{error}");
    }
}
