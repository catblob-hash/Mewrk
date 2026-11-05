//! Image and video input.
//!
//! The server decodes video itself (ffmpeg, 2 fps, 4 to 768 frames) and its vision encoder is serial and
//! blocks the decode loop for every slot, so the client's job is to send as few bytes and as few pixels
//! as it can and to reject what the server would refuse only after a long upload.

use std::path::Path;

use bytes::Bytes;

/// Images are re-encoded above this edge: the encoder's cost grows with pixels (360p about 30 ms, 720p
/// about 116 ms, per the server's own measurements) and every slot waits for it.
pub const DEFAULT_MAX_IMAGE_EDGE: u32 = 1280;
pub const MAX_VIDEO_BYTES: u64 = 512 << 20;
const SERVER_FPS: f64 = 2.0;
const MIN_FRAMES: f64 = 4.0;
const MAX_FRAMES: f64 = 768.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MediaInfo {
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub codec: String,
}

pub struct MediaInput {
    pub kind: MediaKind,
    pub mime: &'static str,
    pub b64: Bytes,
    pub raw_len: u64,
    pub info: MediaInfo,
    pub name: String,
    /// Set when the client re-encoded the input to reduce it.
    pub transcoded_from: Option<(u32, u32)>,
}

#[derive(Debug)]
pub struct MediaError(pub String);

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for MediaError {}

fn err(msg: impl Into<String>) -> MediaError {
    MediaError(msg.into())
}

fn to_b64(raw: &[u8]) -> Bytes {
    Bytes::from(base64_simd::STANDARD.encode_type::<Vec<u8>>(raw))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sniffed {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Webp,
    Mp4,
    Mov,
    Matroska,
    Avi,
    Unknown,
}

pub fn sniff(head: &[u8]) -> Sniffed {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Sniffed::Png
    } else if head.starts_with(b"\xff\xd8\xff") {
        Sniffed::Jpeg
    } else if head.starts_with(b"GIF8") {
        Sniffed::Gif
    } else if head.starts_with(b"BM") {
        Sniffed::Bmp
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Sniffed::Webp
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI " {
        Sniffed::Avi
    } else if head.starts_with(b"\x1a\x45\xdf\xa3") {
        Sniffed::Matroska
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        if &head[8..12] == b"qt  " { Sniffed::Mov } else { Sniffed::Mp4 }
    } else {
        Sniffed::Unknown
    }
}

impl MediaInput {
    /// Runs the CPU-heavy work (decode, resize, base64) on the blocking pool so it never delays a token.
    pub async fn from_path(path: impl AsRef<Path>) -> Result<Self, MediaError> {
        let path = path.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || Self::from_path_blocking(&path, DEFAULT_MAX_IMAGE_EDGE))
            .await
            .map_err(|e| err(format!("media task: {e}")))?
    }

    pub fn from_path_blocking(path: &Path, max_edge: u32) -> Result<Self, MediaError> {
        let meta = std::fs::metadata(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        if meta.len() > MAX_VIDEO_BYTES {
            return Err(err(format!("{}: {} MiB is over the {} MiB limit", path.display(), meta.len() >> 20, MAX_VIDEO_BYTES >> 20)));
        }
        let raw = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        Self::from_bytes(&raw, name, max_edge)
    }

    pub fn from_bytes(raw: &[u8], name: String, max_edge: u32) -> Result<Self, MediaError> {
        match sniff(raw) {
            Sniffed::Png => image_input(raw, "image/png", true, name, max_edge),
            Sniffed::Jpeg => image_input(raw, "image/jpeg", true, name, max_edge),
            Sniffed::Bmp => image_input(raw, "image/bmp", true, name, max_edge),
            Sniffed::Gif => image_input(raw, "image/gif", true, name, max_edge),
            // The server's stb_image cannot read WebP, so it always goes through the local decoder.
            Sniffed::Webp => image_input(raw, "image/webp", false, name, max_edge),
            Sniffed::Mp4 => video_input(raw, "video/mp4", name),
            Sniffed::Mov => video_input(raw, "video/quicktime", name),
            Sniffed::Matroska => video_input(raw, "video/webm", name),
            Sniffed::Avi => video_input(raw, "video/x-msvideo", name),
            Sniffed::Unknown => Err(err(format!("{name}: not a recognised image or video"))),
        }
    }

    /// Rough time the server's serial encoder will spend on this, in milliseconds. It need not be exact: it
    /// only tells a silent stream how long silence is normal while media is being encoded.
    pub fn encode_estimate_ms(&self) -> u64 {
        match self.kind {
            // Measured on the server: 720p takes about 116 ms, and cost follows pixel count.
            MediaKind::Image => ((self.info.width as u64 * self.info.height as u64) * 116 / (1280 * 720)).max(30),
            MediaKind::Video => {
                let frames = if self.info.duration_ms > 0 { (self.info.duration_ms as f64 / 1000.0 * SERVER_FPS) as u64 } else { 16 };
                // A 4 s 360p clip took 134 ms, or 17 ms per frame. Larger sources cost more to decode.
                frames.max(4) * 40
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(kind: MediaKind, mime: &'static str, b64: Bytes) -> Self {
        Self { kind, mime, raw_len: b64.len() as u64, b64, info: MediaInfo::default(), name: String::new(), transcoded_from: None }
    }
}

fn image_dimensions(raw: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(raw)).with_guessed_format().ok()?.into_dimensions().ok()
}

fn image_input(raw: &[u8], mime: &'static str, server_reads_it: bool, name: String, max_edge: u32) -> Result<MediaInput, MediaError> {
    let dims = image_dimensions(raw).ok_or_else(|| err(format!("{name}: unreadable image header")))?;
    let (w, h) = dims;
    if server_reads_it && w.max(h) <= max_edge {
        return Ok(MediaInput {
            kind: MediaKind::Image,
            mime,
            b64: to_b64(raw),
            raw_len: raw.len() as u64,
            info: MediaInfo { width: w, height: h, duration_ms: 0, codec: mime.trim_start_matches("image/").into() },
            name,
            transcoded_from: None,
        });
    }

    // Reduce: keep the aspect ratio and send JPEG, which is far smaller than PNG for photographs.
    let decoded = image::load_from_memory(raw).map_err(|e| err(format!("{name}: {e}")))?;
    let scale = (max_edge as f64 / w.max(h) as f64).min(1.0);
    let (nw, nh) = (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1));
    let rgb = decoded.to_rgb8();
    let out_rgb = if (nw, nh) == (w, h) { rgb } else { resize_rgb(&rgb, nw, nh)? };
    let mut jpeg = Vec::with_capacity((nw * nh / 4) as usize);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 88)
        .encode(&out_rgb, nw, nh, image::ExtendedColorType::Rgb8)
        .map_err(|e| err(format!("{name}: {e}")))?;
    Ok(MediaInput {
        kind: MediaKind::Image,
        mime: "image/jpeg",
        raw_len: jpeg.len() as u64,
        b64: to_b64(&jpeg),
        info: MediaInfo { width: nw, height: nh, duration_ms: 0, codec: "jpeg".into() },
        name,
        transcoded_from: Some((w, h)),
    })
}

fn resize_rgb(src: &image::RgbImage, nw: u32, nh: u32) -> Result<image::RgbImage, MediaError> {
    use fast_image_resize as fr;
    let src_view = fr::images::ImageRef::new(src.width(), src.height(), src.as_raw(), fr::PixelType::U8x3)
        .map_err(|e| err(format!("resize: {e}")))?;
    let mut dst = fr::images::Image::new(nw, nh, fr::PixelType::U8x3);
    fr::Resizer::new()
        .resize(&src_view, &mut dst, &fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Bilinear)))
        .map_err(|e| err(format!("resize: {e}")))?;
    image::RgbImage::from_raw(nw, nh, dst.into_vec()).ok_or_else(|| err("resize: size mismatch"))
}

fn video_input(raw: &[u8], mime: &'static str, name: String) -> Result<MediaInput, MediaError> {
    let info = if matches!(mime, "video/mp4" | "video/quicktime") { parse_mp4(raw).unwrap_or_default() } else { MediaInfo::default() };
    if info.duration_ms > 0 {
        let frames = info.duration_ms as f64 / 1000.0 * SERVER_FPS;
        if frames > MAX_FRAMES {
            return Err(err(format!(
                "{name}: {:.0} s is about {frames:.0} frames at {SERVER_FPS} fps, over the server's limit of {MAX_FRAMES}",
                info.duration_ms as f64 / 1000.0
            )));
        }
        if frames < MIN_FRAMES {
            return Err(err(format!(
                "{name}: {:.1} s is under the server's minimum of {MIN_FRAMES} frames at {SERVER_FPS} fps",
                info.duration_ms as f64 / 1000.0
            )));
        }
    }
    Ok(MediaInput {
        kind: MediaKind::Video,
        mime,
        b64: to_b64(raw),
        raw_len: raw.len() as u64,
        info,
        name,
        transcoded_from: None,
    })
}

// ---------------------------------------------------------------- MP4 metadata

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Iterates sibling ISO-BMFF boxes as (type, payload).
fn boxes(mut b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        let size32 = be32(b, 0)? as u64;
        let ty = b.get(4..8)?;
        let (header, size) = match size32 {
            0 => (8, b.len() as u64),
            1 => (16, be64(b, 8)?),
            s => (8, s),
        };
        if size < header as u64 || size > b.len() as u64 {
            return None;
        }
        let payload = &b[header..size as usize];
        let out = (ty, payload);
        b = &b[size as usize..];
        Some(out)
    })
}

/// Reads duration, dimensions and codec. It never decodes anything, so it costs nothing on large files.
pub fn parse_mp4(raw: &[u8]) -> Option<MediaInfo> {
    let (_, moov) = boxes(raw).find(|(ty, _)| *ty == b"moov")?;
    let mut info = MediaInfo::default();
    for (ty, payload) in boxes(moov) {
        match ty {
            b"mvhd" => {
                let version = *payload.first()?;
                let (timescale, duration) = if version == 1 {
                    (be32(payload, 20)? as u64, be64(payload, 24)?)
                } else {
                    (be32(payload, 12)? as u64, be32(payload, 16)? as u64)
                };
                // Both come straight from the file, so neither the division nor the product can be trusted.
                info.duration_ms = duration.saturating_mul(1000).checked_div(timescale).unwrap_or(0);
            }
            b"trak" => {
                let mut dims = (0, 0);
                let mut codec = None;
                for (ty, payload) in boxes(payload) {
                    match ty {
                        b"tkhd" => {
                            let version = *payload.first()?;
                            let at = if version == 1 { 88 } else { 76 };
                            dims = (be32(payload, at)? >> 16, be32(payload, at + 4)? >> 16);
                        }
                        b"mdia" => codec = video_codec(payload),
                        _ => {}
                    }
                }
                if let Some(codec) = codec {
                    info.width = dims.0;
                    info.height = dims.1;
                    info.codec = codec;
                }
            }
            _ => {}
        }
    }
    Some(info)
}

fn video_codec(mdia: &[u8]) -> Option<String> {
    let is_video = boxes(mdia).any(|(ty, payload)| ty == b"hdlr" && payload.get(8..12) == Some(b"vide"));
    if !is_video {
        return None;
    }
    let (_, minf) = boxes(mdia).find(|(ty, _)| *ty == b"minf")?;
    let (_, stbl) = boxes(minf).find(|(ty, _)| *ty == b"stbl")?;
    let (_, stsd) = boxes(stbl).find(|(ty, _)| *ty == b"stsd")?;
    let entry = stsd.get(8..)?;
    let (fourcc, _) = boxes(entry).next()?;
    Some(String::from_utf8_lossy(fourcc).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_box(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(ty);
        out.extend_from_slice(payload);
        out
    }

    fn mp4_with(duration_secs: u32) -> Vec<u8> {
        let mut mvhd = vec![0u8; 100];
        mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        mvhd[16..20].copy_from_slice(&(duration_secs * 1000).to_be_bytes());
        let mut tkhd = vec![0u8; 84];
        tkhd[76..80].copy_from_slice(&(640u32 << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&(360u32 << 16).to_be_bytes());
        let mut hdlr = vec![0u8; 24];
        hdlr[8..12].copy_from_slice(b"vide");
        let mut stsd = vec![0u8; 8];
        stsd.extend(make_box(b"avc1", &[0u8; 78]));
        let stbl = make_box(b"stbl", &make_box(b"stsd", &stsd));
        let minf = make_box(b"minf", &stbl);
        let mdia = make_box(b"mdia", &[make_box(b"hdlr", &hdlr), minf].concat());
        let trak = make_box(b"trak", &[make_box(b"tkhd", &tkhd), mdia].concat());
        let moov = make_box(b"moov", &[make_box(b"mvhd", &mvhd), trak].concat());
        let mut file = make_box(b"ftyp", b"isom\0\0\0\0isom");
        file.extend(moov);
        file
    }

    #[test]
    fn sniffs_real_magic_numbers() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nxxxx"), Sniffed::Png);
        assert_eq!(sniff(b"\xff\xd8\xff\xe0"), Sniffed::Jpeg);
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Sniffed::Webp);
        assert_eq!(sniff(b"\0\0\0\x20ftypisom\0\0"), Sniffed::Mp4);
        assert_eq!(sniff(b"\0\0\0\x14ftypqt  \0\0"), Sniffed::Mov);
        assert_eq!(sniff(b"\x1a\x45\xdf\xa3\x01"), Sniffed::Matroska);
        assert_eq!(sniff(b"hello world!"), Sniffed::Unknown);
    }

    #[test]
    fn parses_mp4_metadata() {
        let info = parse_mp4(&mp4_with(4)).unwrap();
        assert_eq!(info, MediaInfo { width: 640, height: 360, duration_ms: 4000, codec: "avc1".into() });
    }

    #[test]
    fn rejects_truncated_and_hostile_boxes_without_panicking() {
        let good = mp4_with(4);
        for cut in 0..good.len() {
            let _ = parse_mp4(&good[..cut]);
        }
        let mut hostile = good.clone();
        hostile[3] = 0xff; // box claims to be larger than the file
        let _ = parse_mp4(&hostile);
    }

    #[test]
    fn hostile_durations_do_not_overflow() {
        let mut file = mp4_with(4);
        // Version 1 mvhd: a 64-bit duration, with a timescale of 1.
        let at = file.windows(4).position(|w| w == b"mvhd").unwrap() + 4;
        file[at] = 1;
        file[at + 20..at + 24].copy_from_slice(&1u32.to_be_bytes());
        file[at + 24..at + 32].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(parse_mp4(&file).unwrap().duration_ms, u64::MAX);
        // And a timescale of zero.
        file[at + 20..at + 24].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(parse_mp4(&file).unwrap().duration_ms, 0);
    }

    #[test]
    fn enforces_the_servers_frame_limits_before_uploading() {
        assert!(MediaInput::from_bytes(&mp4_with(4), "ok.mp4".into(), 1280).is_ok());
        assert!(MediaInput::from_bytes(&mp4_with(1), "short.mp4".into(), 1280).is_err());
        assert!(MediaInput::from_bytes(&mp4_with(400), "long.mp4".into(), 1280).is_err());
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 251) as u8, (y % 251) as u8, 128]));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn small_images_pass_through_untouched() {
        let raw = png(64, 32);
        let m = MediaInput::from_bytes(&raw, "s.png".into(), 1280).unwrap();
        assert_eq!(m.mime, "image/png");
        assert_eq!(m.raw_len, raw.len() as u64);
        assert!(m.transcoded_from.is_none());
        assert_eq!(base64_simd::STANDARD.decode_to_vec(&m.b64).unwrap(), raw);
    }

    #[test]
    fn oversized_images_are_reduced_keeping_aspect() {
        let m = MediaInput::from_bytes(&png(4000, 2000), "b.png".into(), 1280).unwrap();
        assert_eq!(m.mime, "image/jpeg");
        assert_eq!((m.info.width, m.info.height), (1280, 640));
        assert_eq!(m.transcoded_from, Some((4000, 2000)));
        let decoded = image::load_from_memory(&base64_simd::STANDARD.decode_to_vec(&m.b64).unwrap()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1280, 640));
    }

    #[test]
    fn garbage_is_rejected_with_its_name() {
        let e = MediaInput::from_bytes(b"not media at all", "x.bin".into(), 1280).err().unwrap();
        assert!(e.0.contains("x.bin"));
    }
}
