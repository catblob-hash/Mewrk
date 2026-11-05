//! Everything a user can attach to a message, as one gesture (upstream's `lib/fileAttachments.ts`). A picked, pasted or
//! dropped file is sorted by what its bytes are, never by its name, and goes one of three ways: an image joins the
//! message's images, a PDF is read for its text, and a UTF-8 text file is kept as it is. Anything else is turned away
//! with a reason, because a model that reads text has no use for it.

use crate::native::{ExtractedPdfText, PdfError};
use mewrk_client::model::{FileAttachment, ImageAttachment};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

pub const MAX_FILE_ATTACHMENT_TEXT_BYTES: usize = 512 * 1024;
pub const MAX_TEXT_FILE_UPLOAD_BYTES: u64 = 262_144;
pub const MAX_FILE_ATTACHMENT_PDF_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_TEXT_FILE_TOKENS: u64 = 25_000;
pub const TRUNCATED_TEXT_FILE_LINES: usize = 2_000;
pub const MAX_MESSAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Pdf,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RejectionReason {
    Directory,
    Empty,
    Unsupported,
    TooLarge,
    /// A text file still over the token cap in its first lines.
    TooLong,
    ImageInputUnavailable,
    ImageRejected,
    PdfWithoutText,
    PdfPassword,
    PdfUnreadable,
    /// Taking it would put the message's attachments past the limit.
    MessageTooLarge,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttachmentRejection {
    /// The file turned away; absent when only a count is known.
    pub name: Option<String>,
    pub reason: RejectionReason,
}

fn rejected(name: Option<&str>, reason: RejectionReason) -> AttachmentRejection {
    AttachmentRejection { name: name.filter(|n| !n.is_empty()).map(str::to_owned), reason }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AttachmentIntakeResult {
    pub images: Vec<ImageAttachment>,
    pub files: Vec<FileAttachment>,
    pub rejected: Vec<AttachmentRejection>,
}

/// What the first bytes of a file say it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteSniff {
    Image,
    Pdf,
    Text,
    Binary,
    Empty,
}

fn is_image_bytes(b: &[u8]) -> bool {
    b.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) || b.starts_with(&[0xff, 0xd8, 0xff]) || b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") || (b.starts_with(b"RIFF") && b.len() >= 12 && &b[8..12] == b"WEBP")
}

/// `%PDF-` anywhere in the first KiB, which is where the format allows the header to be.
fn is_pdf_bytes(b: &[u8]) -> bool {
    b[..b.len().min(1024)].windows(5).any(|w| w == b"%PDF-")
}

/// The file's text, or `None` when it is not text. UTF-8 (with or without a byte-order mark) is taken as it is; UTF-16 only
/// with its byte-order mark, since without one it cannot be told from binary. A NUL anywhere means binary, as to git.
pub fn decode_attachment_text(b: &[u8]) -> Option<String> {
    let wide = |rest: &[u8], big: bool| -> Option<String> {
        if rest.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }).collect();
        String::from_utf16(&units).ok()
    };
    let text = if let Some(rest) = b.strip_prefix(&[0xff, 0xfe]) {
        wide(rest, false)?
    } else if let Some(rest) = b.strip_prefix(&[0xfe, 0xff]) {
        wide(rest, true)?
    } else {
        std::str::from_utf8(b).ok()?.to_owned()
    };
    if text.contains('\u{0}') {
        return None;
    }
    Some(text.strip_prefix('\u{FEFF}').map(str::to_owned).unwrap_or(text))
}

pub fn sniff_attachment_bytes(b: &[u8]) -> ByteSniff {
    if b.is_empty() {
        ByteSniff::Empty
    } else if is_image_bytes(b) {
        ByteSniff::Image
    } else if is_pdf_bytes(b) {
        ByteSniff::Pdf
    } else if decode_attachment_text(b).is_none() {
        ByteSniff::Binary
    } else {
        ByteSniff::Text
    }
}

/// The verdict on one item of a drag still in the air, before anything is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DragVerdict {
    Image,
    Pdf,
    Text,
    Unknown,
    Directory,
    Unsupported,
    Empty,
    TooLarge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DragItem {
    pub name: String,
    pub verdict: DragVerdict,
    /// The native path, when the drag came from the operating system through the host.
    pub path: Option<String>,
}

fn size_fits(kind: AttachmentKind, size: u64) -> bool {
    size <= match kind {
        AttachmentKind::Image => MAX_IMAGE_ATTACHMENT_BYTES,
        AttachmentKind::Pdf => MAX_FILE_ATTACHMENT_PDF_BYTES,
        AttachmentKind::Text => MAX_TEXT_FILE_UPLOAD_BYTES,
    }
}

/// The largest file of any kind this intake reads before it knows the kind.
const MAX_UNSNIFFED_BYTES: u64 = if MAX_IMAGE_ATTACHMENT_BYTES > MAX_FILE_ATTACHMENT_PDF_BYTES { MAX_IMAGE_ATTACHMENT_BYTES } else { MAX_FILE_ATTACHMENT_PDF_BYTES };

/// `lib/contextTokens.ts`: a character's weight by whether it is ASCII.
pub fn estimate_tokens(text: &str) -> u64 {
    let (mut ascii, mut wide) = (0u64, 0u64);
    for c in text.chars() {
        if (c as u32) <= 0x7f {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    (ascii as f64 / 4.0 + wide as f64 / 1.6).ceil() as u64
}

/// What of a text file the model reads, by Claude Code's Read rule: the whole text within the token cap, else its first
/// lines when those are within it, else `None`: the file is not attached.
pub fn text_for_model(text: &str) -> Option<(String, bool)> {
    if estimate_tokens(text) <= MAX_TEXT_FILE_TOKENS {
        return Some((text.to_owned(), false));
    }
    let mut end = None;
    let mut at = 0;
    for _ in 0..TRUNCATED_TEXT_FILE_LINES {
        let found = text[at..].find('\n')?;
        end = Some(at + found);
        at += found + 1;
    }
    let head = &text[..end?];
    (estimate_tokens(head) <= MAX_TEXT_FILE_TOKENS).then(|| (head.to_owned(), true))
}

/// What the host could tell about one path in a native drag, before anything is dropped.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedPathProbe {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub size: u64,
    pub sniff: String,
}

/// The host looked at the path itself: it knows a folder from a file, and what the file starts with.
pub fn drag_item_from_probe(probe: &DroppedPathProbe) -> DragItem {
    let verdict = match (probe.kind.as_str(), probe.sniff.as_str()) {
        ("directory", _) => DragVerdict::Directory,
        ("file", "image") => if size_fits(AttachmentKind::Image, probe.size) { DragVerdict::Image } else { DragVerdict::TooLarge },
        ("file", "pdf") => if size_fits(AttachmentKind::Pdf, probe.size) { DragVerdict::Pdf } else { DragVerdict::TooLarge },
        ("file", "text") => if size_fits(AttachmentKind::Text, probe.size) { DragVerdict::Text } else { DragVerdict::TooLarge },
        ("file", "empty") => DragVerdict::Empty,
        _ => DragVerdict::Unsupported,
    };
    DragItem { name: probe.name.clone(), verdict, path: Some(probe.path.clone()) }
}

const IMAGE_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
const TEXT_MEDIA_TYPES: &[&str] = &["application/json", "application/ld+json", "application/xml", "application/javascript", "application/typescript", "application/x-sh", "application/x-httpd-php", "application/x-yaml", "application/yaml", "application/toml", "application/sql", "application/x-ipynb+json", "image/svg+xml"];

/// A drag that names only each file's media type: what the type rules out is ruled out; a missing or unfamiliar type waits
/// for the drop.
pub fn drag_item_from_media_type(kind: &str) -> DragItem {
    let n = kind.trim().to_lowercase();
    let verdict = if IMAGE_MEDIA_TYPES.contains(&n.as_str()) {
        DragVerdict::Image
    } else if n == "application/pdf" {
        DragVerdict::Pdf
    } else if n.starts_with("text/") || TEXT_MEDIA_TYPES.contains(&n.as_str()) {
        DragVerdict::Text
    } else if n.is_empty() || n == "application/octet-stream" || n == "video/mp2t" {
        // Windows names TypeScript files after MPEG transport streams.
        DragVerdict::Unknown
    } else {
        DragVerdict::Unsupported
    };
    DragItem { name: String::new(), verdict, path: None }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DragRefusal {
    Verdict(DragVerdict),
    ImageInputUnavailable,
}

/// What a drop zone would make of a drag, given whether it can take images.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DragSummary {
    pub accepted: usize,
    /// Items that might be accepted once read.
    pub undecided: usize,
    pub rejected: Vec<(DragRefusal, usize)>,
}

pub fn summarize_drag(items: &[DragItem], image_input: bool) -> DragSummary {
    let mut out = DragSummary::default();
    let mut refuse = |r: DragRefusal| match out.rejected.iter_mut().find(|(k, _)| *k == r) {
        Some((_, n)) => *n += 1,
        None => out.rejected.push((r, 1)),
    };
    let (mut accepted, mut undecided) = (0, 0);
    for item in items {
        match item.verdict {
            DragVerdict::Image if !image_input => refuse(DragRefusal::ImageInputUnavailable),
            DragVerdict::Image | DragVerdict::Pdf | DragVerdict::Text => accepted += 1,
            DragVerdict::Unknown => undecided += 1,
            v => refuse(DragRefusal::Verdict(v)),
        }
    }
    out.accepted = accepted;
    out.undecided = undecided;
    out
}

/// Items a drop turns away before reading anything, as the rejections the notice lists.
pub fn rejections_for_drag_items(items: &[DragItem], image_input: bool) -> Vec<AttachmentRejection> {
    items.iter().filter_map(|item| {
        let reason = match item.verdict {
            DragVerdict::Image if !image_input => RejectionReason::ImageInputUnavailable,
            DragVerdict::Directory => RejectionReason::Directory,
            DragVerdict::Unsupported => RejectionReason::Unsupported,
            DragVerdict::Empty => RejectionReason::Empty,
            DragVerdict::TooLarge => RejectionReason::TooLarge,
            _ => return None,
        };
        Some(rejected(Some(&item.name), reason))
    }).collect()
}

/// One file offered to a message. `media_type` and `size` are what a picker reports; `bytes` is what it holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IncomingFile {
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub bytes: Vec<u8>,
}

impl IncomingFile {
    pub fn new(name: &str, bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        Self { name: name.to_owned(), media_type: String::new(), size: bytes.len() as u64, bytes }
    }

    pub fn typed(name: &str, media_type: &str, bytes: impl Into<Vec<u8>>) -> Self {
        Self { media_type: media_type.to_owned(), ..Self::new(name, bytes) }
    }
}

/// Admits files to one message while its attachments stay within the limit, in the order they are offered: one that would
/// go past it is turned away, and a smaller one after it may still fit.
pub struct AttachmentRoom(u64);

impl AttachmentRoom {
    pub fn new(images: &[ImageAttachment], files: &[FileAttachment]) -> Self {
        let used: u64 = images.iter().map(|i| i.bytes).sum::<u64>() + files.iter().map(|f| f.bytes).sum::<u64>();
        Self(MAX_MESSAGE_ATTACHMENT_BYTES.saturating_sub(used))
    }

    pub fn fits(&mut self, size: u64) -> bool {
        if size > self.0 {
            return false;
        }
        self.0 -= size;
        true
    }
}

/// The ways out of the intake, kept apart so that it is the same code with the host behind it and with a test.
pub trait IntakePorts {
    /// Takes the image files and returns those it attached. `None` when the model this message goes to cannot see images.
    fn add_images(&mut self, files: &[IncomingFile]) -> Option<Vec<ImageAttachment>>;
    fn can_take_images(&self) -> bool;
    fn extract_pdf(&mut self, bytes: &[u8]) -> Result<ExtractedPdfText, PdfError>;
    fn upload(&mut self, name: &str, bytes: &[u8], format: &str, extracted: Option<&ExtractedPdfText>) -> Result<FileAttachment, String>;
}

/// The host's upload and PDFKit's text, from a task thread.
pub struct HostPorts<'a> {
    pub task: &'a mewrk_client::Task,
    pub add_images: Option<Box<dyn FnMut(&[IncomingFile]) -> Vec<ImageAttachment> + 'a>>,
}

impl IntakePorts for HostPorts<'_> {
    fn add_images(&mut self, files: &[IncomingFile]) -> Option<Vec<ImageAttachment>> {
        self.add_images.as_mut().map(|f| f(files))
    }

    fn can_take_images(&self) -> bool {
        self.add_images.is_some()
    }

    fn extract_pdf(&mut self, bytes: &[u8]) -> Result<ExtractedPdfText, PdfError> {
        crate::native::extract_pdf_text(bytes)
    }

    fn upload(&mut self, name: &str, bytes: &[u8], format: &str, extracted: Option<&ExtractedPdfText>) -> Result<FileAttachment, String> {
        self.task.invoke("file_attachment_upload", json!({ "name": name, "data": crate::format::base64_encode(bytes), "format": format, "text": extracted.map(|e| e.text.as_str()), "pages": extracted.map(|e| e.pages) }))
    }
}

struct ReadDocument {
    name: String,
    bytes: Vec<u8>,
    format: &'static str,
    extracted: Option<ExtractedPdfText>,
}

fn read_document(ports: &mut dyn IntakePorts, file: &IncomingFile, kind: AttachmentKind) -> Result<ReadDocument, AttachmentRejection> {
    let trimmed = file.name.trim();
    let name = if trimmed.is_empty() { if kind == AttachmentKind::Pdf { "document.pdf" } else { "text.txt" } } else { trimmed }.to_owned();
    if kind == AttachmentKind::Text {
        let Some(text) = decode_attachment_text(&file.bytes) else { return Err(rejected(Some(&name), RejectionReason::Unsupported)) };
        // UTF-16 is stored as the UTF-8 the model will read, so the host sees one encoding.
        let utf8 = if matches!(file.bytes.first(), Some(0xff | 0xfe)) { text.clone().into_bytes() } else { file.bytes.clone() };
        if utf8.len() as u64 > MAX_TEXT_FILE_UPLOAD_BYTES {
            return Err(rejected(Some(&name), RejectionReason::TooLarge));
        }
        if text.trim().is_empty() {
            return Err(rejected(Some(&name), RejectionReason::Empty));
        }
        // A long file is sent as its first lines; the budget counts what is sent.
        if text_for_model(&text).is_none() {
            return Err(rejected(Some(&name), RejectionReason::TooLong));
        }
        return Ok(ReadDocument { name, bytes: utf8, format: "text", extracted: None });
    }
    let extracted = match ports.extract_pdf(&file.bytes) {
        Ok(e) => e,
        Err(PdfError::WithoutText) => return Err(rejected(Some(&name), RejectionReason::PdfWithoutText)),
        Err(PdfError::Password) => return Err(rejected(Some(&name), RejectionReason::PdfPassword)),
        Err(PdfError::Unreadable(_)) => return Err(rejected(Some(&name), RejectionReason::PdfUnreadable)),
    };
    if extracted.text.len() > MAX_FILE_ATTACHMENT_TEXT_BYTES {
        return Err(rejected(Some(&name), RejectionReason::TooLarge));
    }
    Ok(ReadDocument { name, bytes: file.bytes.clone(), format: "pdf", extracted: Some(extracted) })
}

/// Attaches a batch of files to one message. Files already on the message, and files that turn out to be the same bytes as
/// one of them, are not attached twice: a duplicate is not an error worth a notice.
pub fn intake_attachments(files: &[IncomingFile], ports: &mut dyn IntakePorts, existing_files: &[FileAttachment], existing_images: &[ImageAttachment], pre_rejected: &[AttachmentRejection]) -> AttachmentIntakeResult {
    let mut out = AttachmentIntakeResult { rejected: pre_rejected.to_vec(), ..Default::default() };
    let mut room = AttachmentRoom::new(existing_images, existing_files);
    let mut image_files: Vec<IncomingFile> = Vec::new();
    let mut documents: Vec<(&IncomingFile, AttachmentKind)> = Vec::new();
    for file in files {
        let name = Some(file.name.as_str());
        if file.size == 0 {
            out.rejected.push(rejected(name, RejectionReason::Empty));
            continue;
        }
        let mut image_path = |room: &mut AttachmentRoom, out: &mut AttachmentIntakeResult, ports: &dyn IntakePorts| {
            if !ports.can_take_images() {
                out.rejected.push(rejected(name, RejectionReason::ImageInputUnavailable));
            } else if !size_fits(AttachmentKind::Image, file.size) {
                out.rejected.push(rejected(name, RejectionReason::TooLarge));
            } else if !room.fits(file.size) {
                out.rejected.push(rejected(name, RejectionReason::MessageTooLarge));
            } else {
                image_files.push(file.clone());
            }
        };
        // A file that says it is a picture goes the way pictures always have; the host decodes it and is the one to refuse.
        if IMAGE_MEDIA_TYPES.contains(&file.media_type.to_lowercase().as_str()) {
            image_path(&mut room, &mut out, &*ports);
            continue;
        }
        if file.size > MAX_UNSNIFFED_BYTES {
            out.rejected.push(rejected(name, RejectionReason::TooLarge));
            continue;
        }
        match sniff_attachment_bytes(&file.bytes) {
            ByteSniff::Image => image_path(&mut room, &mut out, &*ports),
            ByteSniff::Pdf if !size_fits(AttachmentKind::Pdf, file.size) => out.rejected.push(rejected(name, RejectionReason::TooLarge)),
            sniff @ (ByteSniff::Pdf | ByteSniff::Text) => {
                if room.fits(file.size) {
                    documents.push((file, if sniff == ByteSniff::Pdf { AttachmentKind::Pdf } else { AttachmentKind::Text }));
                } else {
                    out.rejected.push(rejected(name, RejectionReason::MessageTooLarge));
                }
            }
            ByteSniff::Empty => out.rejected.push(rejected(name, RejectionReason::Empty)),
            ByteSniff::Binary => out.rejected.push(rejected(name, RejectionReason::Unsupported)),
        }
    }
    if !image_files.is_empty()
        && let Some(images) = ports.add_images(&image_files)
    {
        // The image path drops what it cannot take without saying which; the count is what is left to report.
        for _ in images.len()..image_files.len() {
            out.rejected.push(rejected(None, RejectionReason::ImageRejected));
        }
        out.images = images;
    }
    let mut uploads = Vec::new();
    for (file, kind) in documents {
        match read_document(ports, file, kind) {
            Ok(doc) => uploads.push(doc),
            Err(r) => out.rejected.push(r),
        }
    }
    let mut known: HashSet<String> = existing_files.iter().map(|f| f.id.clone()).collect();
    for doc in uploads {
        match ports.upload(&doc.name, &doc.bytes, doc.format, doc.extracted.as_ref()) {
            Ok(file) => {
                if known.insert(file.id.clone()) {
                    out.files.push(file);
                }
            }
            Err(_) => out.rejected.push(rejected(Some(&doc.name), RejectionReason::Failed)),
        }
    }
    out
}

/// Merges newly attached files into a message's list, by id, keeping order.
pub fn merge_file_attachments(current: &[FileAttachment], added: &[FileAttachment]) -> Vec<FileAttachment> {
    let mut known: HashSet<&str> = current.iter().map(|f| f.id.as_str()).collect();
    let mut merged = current.to_vec();
    for file in added {
        if known.insert(file.id.as_str()) {
            merged.push(file.clone());
        }
    }
    merged
}

/// A file of the last native drop, read from the host.
pub fn read_dropped_file(task: &mewrk_client::Task, path: &str) -> Result<IncomingFile, String> {
    #[derive(Deserialize)]
    struct Dropped {
        name: String,
        data: String,
    }
    let dropped: Dropped = task.invoke("dropped_file_read", json!({ "path": path }))?;
    let bytes = base64_simd::STANDARD.decode_to_vec(dropped.data.as_bytes()).map_err(|e| e.to_string())?;
    Ok(IncomingFile::new(&dropped.name, bytes))
}

/// Classifies the paths of the drag in progress. The host answers only for paths its own window saw enter or drop.
pub fn probe_dropped_paths(task: &mewrk_client::Task, paths: &[String]) -> Result<Vec<DroppedPathProbe>, String> {
    task.invoke("dropped_paths_probe", json!({ "paths": paths }))
}

pub fn file_attachment_args(file_id: &str) -> Value {
    json!({ "fileId": file_id })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn stored(name: &str, id: &str) -> FileAttachment {
        serde_json::from_value(json!({ "id": format!("{id:0<64}"), "name": name, "format": "text", "bytes": 10, "tokens": 3 })).unwrap()
    }

    fn image(id: &str, bytes: u64) -> ImageAttachment {
        serde_json::from_value(json!({ "id": id.repeat(64), "name": "a.png", "mime": "image/png", "width": 1, "height": 1, "bytes": bytes, "shortId": 1 })).unwrap()
    }

    #[derive(Default)]
    struct Ports {
        images: Option<Vec<ImageAttachment>>,
        asked_images: Vec<String>,
        pdf: Vec<Result<ExtractedPdfText, PdfError>>,
        extracted_calls: usize,
        uploads: Vec<(String, Vec<u8>, String, Option<ExtractedPdfText>)>,
    }

    impl IntakePorts for Ports {
        fn add_images(&mut self, files: &[IncomingFile]) -> Option<Vec<ImageAttachment>> {
            self.asked_images = files.iter().map(|f| f.name.clone()).collect();
            self.images.clone()
        }
        fn can_take_images(&self) -> bool {
            self.images.is_some()
        }
        fn extract_pdf(&mut self, _: &[u8]) -> Result<ExtractedPdfText, PdfError> {
            self.extracted_calls += 1;
            self.pdf.remove(0)
        }
        fn upload(&mut self, name: &str, bytes: &[u8], format: &str, extracted: Option<&ExtractedPdfText>) -> Result<FileAttachment, String> {
            self.uploads.push((name.to_owned(), bytes.to_vec(), format.to_owned(), extracted.cloned()));
            let mut file = stored(name, name);
            file.bytes = bytes.len() as u64;
            Ok(file)
        }
    }

    fn names(files: &[FileAttachment]) -> Vec<&str> {
        files.iter().map(|f| f.name.as_str()).collect()
    }

    fn r(name: &str, reason: RejectionReason) -> AttachmentRejection {
        AttachmentRejection { name: Some(name.to_owned()), reason }
    }

    #[test]
    fn tells_pictures_pdfs_and_text_apart_by_their_bytes() {
        assert_eq!(sniff_attachment_bytes(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0]), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes(&[0xff, 0xd8, 0xff, 0xe0]), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes(b"GIF89a..."), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes(b"RIFF\0\0\0\0WEBPVP8 "), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes("%PDF-1.7\n%âãÏÓ".as_bytes()), ByteSniff::Pdf);
        // The header may sit after some junk, within the first KiB.
        assert_eq!(sniff_attachment_bytes(format!("{}%PDF-1.4", " ".repeat(100)).as_bytes()), ByteSniff::Pdf);
        assert_eq!(sniff_attachment_bytes(b"export const answer = 42;\n"), ByteSniff::Text);
        assert_eq!(sniff_attachment_bytes("中文说明".as_bytes()), ByteSniff::Text);
    }

    #[test]
    fn calls_a_nul_or_broken_utf8_binary_and_nothing_at_all_empty() {
        assert_eq!(sniff_attachment_bytes(&[0x50, 0x4b, 0x03, 0x04, 0x00]), ByteSniff::Binary);
        assert_eq!(sniff_attachment_bytes(&[0xc3, 0x28]), ByteSniff::Binary);
        assert_eq!(sniff_attachment_bytes(&[]), ByteSniff::Empty);
    }

    #[test]
    fn drops_a_utf8_mark_and_reads_utf16_only_with_its_mark() {
        assert_eq!(decode_attachment_text(&[0xef, 0xbb, 0xbf, 0x68, 0x69]).as_deref(), Some("hi"));
        assert_eq!(decode_attachment_text(&[0xff, 0xfe, 0x68, 0x00, 0x69, 0x00]).as_deref(), Some("hi"));
        assert_eq!(decode_attachment_text(&[0xfe, 0xff, 0x00, 0x68, 0x00, 0x69]).as_deref(), Some("hi"));
        assert_eq!(decode_attachment_text(&[0x68, 0x00, 0x69, 0x00]), None);
    }

    fn probe(name: &str, kind: &str, size: u64, sniff: &str) -> DragVerdict {
        drag_item_from_probe(&DroppedPathProbe { path: format!("/{name}"), name: name.to_owned(), kind: kind.to_owned(), size, sniff: sniff.to_owned() }).verdict
    }

    #[test]
    fn takes_the_hosts_word_on_folders_sizes_and_what_a_file_starts_with() {
        assert_eq!(probe("a", "directory", 0, "none"), DragVerdict::Directory);
        assert_eq!(probe("b.md", "file", 10, "text"), DragVerdict::Text);
        assert_eq!(MAX_TEXT_FILE_UPLOAD_BYTES, 262_144);
        assert_eq!(probe("big", "file", MAX_TEXT_FILE_UPLOAD_BYTES + 1, "text"), DragVerdict::TooLarge);
        assert_eq!(probe("t", "file", MAX_TEXT_FILE_UPLOAD_BYTES, "text"), DragVerdict::Text);
        assert_eq!(probe("p", "file", MAX_FILE_ATTACHMENT_PDF_BYTES, "pdf"), DragVerdict::Pdf);
        assert_eq!(probe("q", "file", MAX_FILE_ATTACHMENT_PDF_BYTES + 1, "pdf"), DragVerdict::TooLarge);
        assert_eq!(probe("i", "file", MAX_IMAGE_ATTACHMENT_BYTES, "image"), DragVerdict::Image);
        assert_eq!(probe("c.zip", "file", 10, "binary"), DragVerdict::Unsupported);
        assert_eq!(probe("d", "file", 0, "empty"), DragVerdict::Empty);
    }

    #[test]
    fn reads_what_it_can_from_a_browsers_media_types_and_waits_on_the_rest() {
        let v = |t: &str| drag_item_from_media_type(t).verdict;
        assert_eq!(v("image/png"), DragVerdict::Image);
        assert_eq!(v("application/pdf"), DragVerdict::Pdf);
        assert_eq!(v("text/markdown"), DragVerdict::Text);
        assert_eq!(v("application/json"), DragVerdict::Text);
        assert_eq!(v(""), DragVerdict::Unknown);
        assert_eq!(v("video/mp2t"), DragVerdict::Unknown);
        assert_eq!(v("application/zip"), DragVerdict::Unsupported);
    }

    #[test]
    fn counts_a_picture_as_refused_where_the_model_has_no_image_input() {
        let item = |name: &str, verdict| DragItem { name: name.to_owned(), verdict, path: None };
        let items = [item("a.png", DragVerdict::Image), item("b.md", DragVerdict::Text), item("dir", DragVerdict::Directory), item("", DragVerdict::Unknown)];
        assert_eq!(summarize_drag(&items, true), DragSummary { accepted: 2, undecided: 1, rejected: vec![(DragRefusal::Verdict(DragVerdict::Directory), 1)] });
        assert_eq!(summarize_drag(&items, false), DragSummary { accepted: 1, undecided: 1, rejected: vec![(DragRefusal::ImageInputUnavailable, 1), (DragRefusal::Verdict(DragVerdict::Directory), 1)] });
        assert_eq!(rejections_for_drag_items(&items, false), [r("a.png", RejectionReason::ImageInputUnavailable), r("dir", RejectionReason::Directory)]);
    }

    #[test]
    fn sends_a_text_within_the_token_cap_whole_and_cuts_a_longer_one_to_its_first_lines() {
        let text = vec!["z"; 5_000].join("\n");
        assert_eq!(text_for_model(&text), Some((text.clone(), false)));
        let lines: Vec<String> = (0..3_000).map(|i| format!("{:05}{}", i + 1, "x".repeat(35))).collect();
        let long = lines.join("\n");
        assert!(estimate_tokens(&long) > MAX_TEXT_FILE_TOKENS);
        assert_eq!(text_for_model(&long), Some((lines[..2_000].join("\n"), true)));
    }

    #[test]
    fn gives_up_on_one_still_over_the_cap_in_those_lines() {
        assert_eq!(text_for_model(&vec!["y".repeat(80); 1_500].join("\n")), None);
        assert_eq!(text_for_model(&"w".repeat(120_000)), None);
    }

    #[test]
    fn stores_a_text_file_as_utf8_and_a_pdf_with_its_text() {
        let mut ports = Ports { pdf: vec![Ok(ExtractedPdfText { text: "[Page 1]\nhello".into(), pages: 1 })], ..Default::default() };
        let result = intake_attachments(&[IncomingFile::new("wide.txt", vec![0xff, 0xfe, 0x68, 0x00, 0x69, 0x00]), IncomingFile::typed("paper.pdf", "application/pdf", b"%PDF-1.7 body".to_vec())], &mut ports, &[], &[], &[]);
        assert!(result.rejected.is_empty());
        assert_eq!(names(&result.files), ["wide.txt", "paper.pdf"]);
        assert_eq!(ports.uploads[0].1, b"hi");
        assert_eq!(ports.uploads[0].2, "text");
        assert_eq!(ports.uploads[1].2, "pdf");
        assert_eq!(ports.uploads[1].3, Some(ExtractedPdfText { text: "[Page 1]\nhello".into(), pages: 1 }));
    }

    #[test]
    fn gives_each_refused_file_its_reason() {
        let mut ports = Ports { pdf: vec![Err(PdfError::WithoutText), Err(PdfError::Password)], ..Default::default() };
        let huge = "a\n".repeat(MAX_TEXT_FILE_UPLOAD_BYTES as usize / 2 + 1);
        let result = intake_attachments(&[
            IncomingFile::new("bundle.zip", vec![0x50, 0x4b, 0x03, 0x04, 0x00]),
            IncomingFile::new("empty.txt", Vec::new()),
            IncomingFile::new("scan.pdf", b"%PDF-1.4".to_vec()),
            IncomingFile::new("locked.pdf", b"%PDF-1.4".to_vec()),
            IncomingFile::new("huge.log", huge.into_bytes()),
            IncomingFile::typed("photo.png", "image/png", vec![1]),
        ], &mut ports, &[], &[], &[r("folder", RejectionReason::Directory)]);
        assert!(result.files.is_empty());
        assert_eq!(result.rejected, [
            r("folder", RejectionReason::Directory),
            r("bundle.zip", RejectionReason::Unsupported),
            r("empty.txt", RejectionReason::Empty),
            r("photo.png", RejectionReason::ImageInputUnavailable),
            r("scan.pdf", RejectionReason::PdfWithoutText),
            r("locked.pdf", RejectionReason::PdfPassword),
            r("huge.log", RejectionReason::TooLarge),
        ]);
        assert!(ports.uploads.is_empty());
    }

    #[test]
    fn sends_a_long_text_file_as_its_first_lines_and_refuses_one_that_is_still_too_long() {
        let mut ports = Ports::default();
        let result = intake_attachments(&[IncomingFile::new("big.log", vec!["x".repeat(40); 3_000].join("\n").into_bytes()), IncomingFile::new("wide.csv", vec!["y".repeat(80); 1_500].join("\n").into_bytes())], &mut ports, &[], &[], &[]);
        assert_eq!(names(&result.files), ["big.log"]);
        assert_eq!(result.rejected, [r("wide.csv", RejectionReason::TooLong)]);
    }

    #[test]
    fn turns_away_a_picture_or_pdf_too_large_to_hand_over_by_name() {
        let mut ports = Ports { images: Some(Vec::new()), ..Default::default() };
        let mut photo = IncomingFile::typed("photo.jpg", "image/jpeg", vec![1]);
        photo.size = MAX_IMAGE_ATTACHMENT_BYTES + 1;
        let mut pdf = b"%PDF-1.7 ".to_vec();
        pdf.resize(MAX_FILE_ATTACHMENT_PDF_BYTES as usize + 9, 0);
        let result = intake_attachments(&[photo, IncomingFile::new("big.pdf", pdf)], &mut ports, &[], &[], &[]);
        assert!(ports.asked_images.is_empty());
        assert_eq!(result.rejected, [r("photo.jpg", RejectionReason::TooLarge), r("big.pdf", RejectionReason::TooLarge)]);
        assert_eq!(ports.extracted_calls, 0);
    }

    #[test]
    fn hands_pictures_to_the_image_path_and_reports_the_ones_it_did_not_take() {
        let accepted = image("i", 1);
        let mut ports = Ports { images: Some(vec![accepted.clone()]), ..Default::default() };
        let result = intake_attachments(&[IncomingFile::typed("a.png", "image/png", vec![1]), IncomingFile::new("unlabelled", vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])], &mut ports, &[], &[], &[]);
        assert_eq!(ports.asked_images, ["a.png", "unlabelled"]);
        assert_eq!(result.images, [accepted]);
        assert_eq!(result.rejected, [AttachmentRejection { name: None, reason: RejectionReason::ImageRejected }]);
    }

    #[test]
    fn takes_any_number_of_files_and_drops_a_file_it_already_has() {
        let existing: Vec<FileAttachment> = (0..20).map(|i| stored(&format!("f{i}.txt"), &format!("f{i}.txt"))).collect();
        let added: Vec<IncomingFile> = (0..25).map(|i| IncomingFile::new(&format!("n{i}.txt"), format!("n{i}").into_bytes())).collect();
        let mut all = vec![IncomingFile::new("f0.txt", b"again".to_vec())];
        all.extend(added.iter().cloned());
        let result = intake_attachments(&all, &mut Ports::default(), &existing, &[], &[]);
        assert_eq!(names(&result.files), added.iter().map(|f| f.name.as_str()).collect::<Vec<_>>());
        assert!(result.rejected.is_empty());
        assert_eq!(merge_file_attachments(&existing, &result.files).len(), 45);
    }

    #[test]
    fn keeps_one_messages_attachments_within_32_mib_in_the_order_they_come() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(MAX_MESSAGE_ATTACHMENT_BYTES, 32 * MIB);
        let sized = |mut f: IncomingFile, size: u64| {
            f.size = size;
            f
        };
        let photo_image = image("p", 1);
        let mut ports = Ports { images: Some(vec![photo_image.clone()]), ..Default::default() };
        let mut old = stored("old.pdf", "old.pdf");
        old.bytes = 10 * MIB;
        // 2 MiB of images and 10 MiB of files already on the message leave 20 MiB.
        let result = intake_attachments(&[sized(IncomingFile::typed("photo.png", "image/png", vec![1]), 15 * MIB), sized(IncomingFile::new("report.pdf", b"%PDF-1.7 ".to_vec()), 10 * MIB), IncomingFile::new("notes.txt", b"notes".to_vec())], &mut ports, &[old], &[image("o", 2 * MIB)], &[]);
        // The photo fits (5 MiB left), the report does not, and the small note still does.
        assert_eq!(ports.asked_images, ["photo.png"]);
        assert_eq!(names(&result.files), ["notes.txt"]);
        assert_eq!(result.rejected, [r("report.pdf", RejectionReason::MessageTooLarge)]);
        assert_eq!(ports.extracted_calls, 0);
    }

    #[test]
    fn takes_a_messages_files_however_much_text_they_come_to() {
        let mut heavy = stored("heavy.txt", "heavy.txt");
        heavy.tokens = 400_000;
        let result = intake_attachments(&[IncomingFile::new("tiny.txt", b"tiny".to_vec()), IncomingFile::new("more.txt", "x".repeat(400).into_bytes())], &mut Ports::default(), &[heavy], &[], &[]);
        assert_eq!(names(&result.files), ["tiny.txt", "more.txt"]);
        assert!(result.rejected.is_empty());
    }
}
