//! Backgrounds that move.
//!
//! A video is played by the system's hardware decoder, and each frame reaches the GPU without a copy. An animated picture is decoded
//! once into a [`flipbook::Flipbook`], a file of whole frames that playing only maps and uploads. The glass baked from either is a
//! flipbook too, and a still picture's glass is one with a single frame, so there is one way to keep a bake and one way to show it.

pub mod animated;
pub mod flipbook;
pub mod pixels;
pub mod player;
pub mod video;

pub use flipbook::{FlipWriter, Flipbook};
pub use pixels::{area_resize, Pixels};
pub use player::{Player, Tick};
pub use video::VideoInfo;

use std::path::Path;

/// What a picked file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Picture,
    /// A GIF, APNG or WebP with more than one frame.
    Animated,
    Video,
}

/// The extensions of what [`video`] may be able to read, for the file dialog.
pub const VIDEO_EXTENSIONS: &[&str] = if cfg!(target_os = "macos") { &["mp4", "m4v", "mov"] } else { &[] };

/// Decides by the file's first bytes and not its name, except for a QuickTime file, which can open with any of a few atoms.
pub fn classify(path: &Path) -> Result<FileKind, String> {
    use std::io::Read;
    let mut head = [0u8; 12];
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let read = file.read(&mut head).map_err(|e| e.to_string())?;
    let head = &head[..read];
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        // An iPhone's photos come in this same box: its brand says which it is.
        let picture = matches!(&head[8..12], b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" | b"mif1" | b"msf1" | b"avif" | b"avis");
        return Ok(if picture { FileKind::Picture } else { FileKind::Video });
    }
    if head.len() >= 8 && matches!(&head[4..8], b"moov" | b"mdat" | b"wide" | b"free" | b"skip") && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mov")) {
        return Ok(FileKind::Video);
    }
    if animated::may_be_animated(head) {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        if animated::frame_count(&bytes).is_some_and(|count| count > 1) {
            return Ok(FileKind::Animated);
        }
    }
    Ok(FileKind::Picture)
}

/// Keeps a long job out of the way of the window, and of the video on screen.
pub fn lower_this_threads_priority() {
    #[cfg(target_os = "macos")]
    // SAFETY: it only changes the calling thread's own class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_what_its_first_bytes_say_and_an_iphones_photo_is_no_video() {
        let dir = std::env::temp_dir().join(format!("mewrk-classify-{}-{:x}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            classify(&path).unwrap()
        };
        assert_eq!(file("a.heic", b"\0\0\0\x18ftypheic\0\0\0\0mif1"), FileKind::Picture);
        assert_eq!(file("a.avif", b"\0\0\0\x18ftypavif\0\0\0\0mif1"), FileKind::Picture);
        assert_eq!(file("a.mp4", b"\0\0\0\x18ftypisom\0\0\0\0mp41"), FileKind::Video);
        assert_eq!(file("a.mov", b"\0\0\0\x14ftypqt  \0\0\0\0qt  "), FileKind::Video);
        assert_eq!(file("old.mov", b"\0\0\0\x08wide\0\0\0\0mdat"), FileKind::Video);
        assert_eq!(file("old.bin", b"\0\0\0\x08wide\0\0\0\0mdat"), FileKind::Picture, "only a .mov may open that way");
        assert_eq!(file("note.txt", b"just some words here"), FileKind::Picture);
        assert_eq!(file("tiny", b"x"), FileKind::Picture);
    }
}
