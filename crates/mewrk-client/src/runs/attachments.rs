//! Port of `lib/fileAttachments.ts`, the part a send needs, and the budgets of `lib/fileBudget.ts` and `lib/imageBudget.ts`.
//!
//! Everything a user can attach to a message, as one gesture. A picked, pasted or dropped file is sorted by what its
//! bytes are — never by its name, which says nothing reliable about a `Makefile` or a `.ts` Windows files under video —
//! and goes one of three ways: an image joins the message's images, a PDF is read for its text, and a UTF-8 text file
//! is kept as it is. Anything else is turned away with a reason, because a model that reads text has no use for it.

use super::context_tokens::estimate_tokens;
use crate::model::{FileAttachment, ImageAttachment};

pub const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_IMAGE_ATTACHMENT_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_FILE_ATTACHMENT_TEXT_BYTES: u64 = 512 * 1024;
pub const MAX_TEXT_FILE_UPLOAD_BYTES: u64 = 262_144;
pub const MAX_FILE_ATTACHMENT_PDF_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_TEXT_FILE_TOKENS: u64 = 25_000;
pub const TRUNCATED_TEXT_FILE_LINES: usize = 2_000;
pub const MAX_MESSAGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentRejectionReason {
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
    /// Taking it would put the message's attachments past [`MAX_MESSAGE_ATTACHMENT_BYTES`].
    MessageTooLarge,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentRejection {
    /// The file turned away; absent when only a count is known.
    pub name: Option<String>,
    pub reason: AttachmentRejectionReason,
}

/// A file offered to the composer: upstream's `File`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerFile {
    pub name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

impl ComposerFile {
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }
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

fn is_image_bytes(bytes: &[u8]) -> bool {
    const SIGNATURES: [&[u8]; 4] = [&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a], &[0xff, 0xd8, 0xff], b"GIF87a", b"GIF89a"];
    SIGNATURES.iter().any(|signature| bytes.starts_with(signature)) || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// `%PDF-` anywhere in the first KiB, which is where the format allows the header to be.
fn is_pdf_bytes(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(1024)].windows(5).any(|window| window == b"%PDF-")
}

/// The file's text, or `None` when it is not text.
///
/// UTF-8 (with or without a byte-order mark) is taken as it is; UTF-16 is taken
/// only with its byte-order mark, since without one it cannot be told from
/// binary. A NUL anywhere means binary, as it does to git.
pub fn decode_attachment_text(bytes: &[u8]) -> Option<String> {
    let units = |rest: &[u8], big: bool| -> Option<String> {
        if rest.len() % 2 != 0 {
            return None;
        }
        let units: Vec<u16> = rest.chunks_exact(2).map(|pair| if big { u16::from_be_bytes([pair[0], pair[1]]) } else { u16::from_le_bytes([pair[0], pair[1]]) }).collect();
        String::from_utf16(&units).ok()
    };
    let text = if bytes.starts_with(&[0xff, 0xfe]) {
        units(&bytes[2..], false)?
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        units(&bytes[2..], true)?
    } else {
        std::str::from_utf8(bytes).ok()?.to_owned()
    };
    if text.contains('\0') {
        return None;
    }
    Some(text.strip_prefix('\u{feff}').map(str::to_owned).unwrap_or(text))
}

pub fn sniff_attachment_bytes(bytes: &[u8]) -> ByteSniff {
    if bytes.is_empty() {
        return ByteSniff::Empty;
    }
    if is_image_bytes(bytes) {
        return ByteSniff::Image;
    }
    if is_pdf_bytes(bytes) {
        return ByteSniff::Pdf;
    }
    if decode_attachment_text(bytes).is_none() { ByteSniff::Binary } else { ByteSniff::Text }
}

/// The largest file of any kind this intake reads before it knows the kind.
const MAX_UNSNIFFED_BYTES: u64 = if MAX_IMAGE_ATTACHMENT_BYTES > MAX_FILE_ATTACHMENT_PDF_BYTES { MAX_IMAGE_ATTACHMENT_BYTES } else { MAX_FILE_ATTACHMENT_PDF_BYTES };

/// What of a text file the model reads, by Claude Code's Read rule (mirrors Rust
/// `file_attachments::ModelText`): the whole text within
/// [`MAX_TEXT_FILE_TOKENS`], else its first [`TRUNCATED_TEXT_FILE_LINES`]
/// lines when those are within it, else `None` — the file is not attached.
pub fn text_for_model(text: &str) -> Option<(&str, bool)> {
    if estimate_tokens(text) <= MAX_TEXT_FILE_TOKENS {
        return Some((text, false));
    }
    let mut end = None;
    let mut from = 0;
    for _ in 0..TRUNCATED_TEXT_FILE_LINES {
        let at = text[from..].find('\n')? + from;
        end = Some(at);
        from = at + 1;
    }
    let head = &text[..end?];
    (estimate_tokens(head) <= MAX_TEXT_FILE_TOKENS).then_some((head, true))
}

fn is_image_media_type(media_type: &str) -> bool {
    matches!(media_type.to_ascii_lowercase().as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp")
}

/// Admits files to one message while its attachments stay within [`MAX_MESSAGE_ATTACHMENT_BYTES`], in the order they
/// are offered: one that would go past it is turned away, and a smaller one after it may still fit.
#[derive(Clone, Copy, Debug)]
pub struct MessageAttachmentRoom(u64);

impl MessageAttachmentRoom {
    pub fn new(images: &[ImageAttachment], files: &[FileAttachment]) -> Self {
        let taken: u64 = images.iter().map(|image| image.bytes).sum::<u64>() + files.iter().map(|file| file.bytes).sum::<u64>();
        Self(MAX_MESSAGE_ATTACHMENT_BYTES.saturating_sub(taken))
    }

    pub fn fits(&mut self, size: u64) -> bool {
        if size > self.0 {
            return false;
        }
        self.0 -= size;
        true
    }
}

/// How one offered file is to be handled, decided before anything is uploaded.
#[derive(Debug, PartialEq, Eq)]
pub enum Intake {
    Image(ComposerFile),
    Document { file: ComposerFile, kind: DocumentKind },
    Rejected(AttachmentRejection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Pdf,
    Text,
}

/// Sorts a batch by what each file is (the first half of `intakeAttachments`). `images_allowed` is whether this
/// message's model can see images; `room` counts what the message already carries.
pub fn sort_attachments(files: Vec<ComposerFile>, images_allowed: bool, mut room: MessageAttachmentRoom) -> Vec<Intake> {
    let name_of = |file: &ComposerFile| if file.name.is_empty() { None } else { Some(file.name.clone()) };
    files
        .into_iter()
        .map(|file| {
            let name = name_of(&file);
            let size = file.size();
            let reject = |reason| Intake::Rejected(AttachmentRejection { name: name.clone(), reason });
            if size == 0 {
                return reject(AttachmentRejectionReason::Empty);
            }
            let image = |file: ComposerFile, room: &mut MessageAttachmentRoom| {
                if !images_allowed {
                    reject(AttachmentRejectionReason::ImageInputUnavailable)
                } else if size > MAX_IMAGE_ATTACHMENT_BYTES {
                    reject(AttachmentRejectionReason::TooLarge)
                } else if !room.fits(size) {
                    reject(AttachmentRejectionReason::MessageTooLarge)
                } else {
                    Intake::Image(file)
                }
            };
            // A file that says it is a picture goes the way pictures always have; the
            // host decodes it and is the one to refuse it if it is not.
            if is_image_media_type(&file.media_type) {
                return image(file, &mut room);
            }
            if size > MAX_UNSNIFFED_BYTES {
                return reject(AttachmentRejectionReason::TooLarge);
            }
            match sniff_attachment_bytes(&file.bytes) {
                ByteSniff::Image => image(file, &mut room),
                ByteSniff::Pdf if size > MAX_FILE_ATTACHMENT_PDF_BYTES => reject(AttachmentRejectionReason::TooLarge),
                kind @ (ByteSniff::Pdf | ByteSniff::Text) => {
                    if room.fits(size) {
                        Intake::Document { file, kind: if kind == ByteSniff::Pdf { DocumentKind::Pdf } else { DocumentKind::Text } }
                    } else {
                        reject(AttachmentRejectionReason::MessageTooLarge)
                    }
                }
                ByteSniff::Empty => reject(AttachmentRejectionReason::Empty),
                ByteSniff::Binary => reject(AttachmentRejectionReason::Unsupported),
            }
        })
        .collect()
}

/// A document read and checked, ready to upload, with the tokens its text will cost.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadDocument {
    pub name: String,
    pub bytes: Vec<u8>,
    pub format: &'static str,
    pub extracted: Option<ExtractedPdfText>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedPdfText {
    pub text: String,
    pub pages: u32,
}

/// Why a PDF has no text to give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PdfError {
    WithoutText,
    Password,
    Unreadable,
}

/// `extractPdfText`: `[Page n]\ntext` per page, which is what the model reads. Upstream runs pdf.js in the renderer; here it
/// is the system's own reader.
#[cfg(target_os = "macos")]
pub fn extract_pdf_text(bytes: &[u8]) -> Result<ExtractedPdfText, PdfError> {
    use objc2::AnyThread;
    use objc2_foundation::NSData;
    use objc2_pdf_kit::PDFDocument;
    let data = NSData::with_bytes(bytes);
    // SAFETY: the data is a valid NSData, and PDFDocument copes with any bytes by answering nil.
    let document = unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }.ok_or(PdfError::Unreadable)?;
    // SAFETY: plain property read.
    if unsafe { document.isLocked() } {
        return Err(PdfError::Password);
    }
    // SAFETY: plain reads on a document this function owns.
    let pages = unsafe { document.pageCount() };
    let mut text = String::new();
    for index in 0..pages {
        // SAFETY: `index` is within the document's pages.
        let page = unsafe { document.pageAtIndex(index) };
        // SAFETY: plain property read.
        let content = page.and_then(|p| unsafe { p.string() }).map_or(String::new(), |s| s.to_string());
        if content.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("[Page {}]\n{}", index + 1, content.trim()));
    }
    if text.is_empty() {
        return Err(PdfError::WithoutText);
    }
    Ok(ExtractedPdfText { text, pages: pages as u32 })
}

/// No PDF reader is wired on this platform yet, so a PDF is turned away as unreadable rather than attached without the
/// text the model would read.
#[cfg(not(target_os = "macos"))]
pub fn extract_pdf_text(_bytes: &[u8]) -> Result<ExtractedPdfText, PdfError> {
    Err(PdfError::Unreadable)
}

/// `readDocument`: checks a document before upload.
pub fn read_document(file: ComposerFile, kind: DocumentKind) -> Result<ReadDocument, AttachmentRejection> {
    let trimmed = file.name.trim();
    let name = if trimmed.is_empty() { if kind == DocumentKind::Pdf { "document.pdf" } else { "text.txt" }.to_owned() } else { trimmed.to_owned() };
    let reject = |reason| AttachmentRejection { name: Some(name.clone()), reason };
    match kind {
        DocumentKind::Text => {
            let Some(text) = decode_attachment_text(&file.bytes) else { return Err(reject(AttachmentRejectionReason::Unsupported)) };
            // UTF-16 is stored as the UTF-8 the model will read, so the host sees one encoding.
            let utf8 = if matches!(file.bytes.first(), Some(0xff | 0xfe)) { text.clone().into_bytes() } else { file.bytes };
            if utf8.len() as u64 > MAX_TEXT_FILE_UPLOAD_BYTES {
                return Err(reject(AttachmentRejectionReason::TooLarge));
            }
            if text.trim().is_empty() {
                return Err(reject(AttachmentRejectionReason::Empty));
            }
            // A long file is sent as its first lines; the budget counts what is sent.
            if text_for_model(&text).is_none() {
                return Err(reject(AttachmentRejectionReason::TooLong));
            }
            Ok(ReadDocument { bytes: utf8, format: "text", extracted: None, name })
        }
        DocumentKind::Pdf => {
            let extracted = extract_pdf_text(&file.bytes).map_err(|error| {
                reject(match error {
                    PdfError::WithoutText => AttachmentRejectionReason::PdfWithoutText,
                    PdfError::Password => AttachmentRejectionReason::PdfPassword,
                    PdfError::Unreadable => AttachmentRejectionReason::PdfUnreadable,
                })
            })?;
            if extracted.text.len() as u64 > MAX_FILE_ATTACHMENT_TEXT_BYTES {
                return Err(reject(AttachmentRejectionReason::TooLarge));
            }
            Ok(ReadDocument { bytes: file.bytes, format: "pdf", extracted: Some(extracted), name })
        }
    }
}

/// Merges newly attached files into a message's list, by id, keeping order.
pub fn merge_file_attachments(current: &[FileAttachment], added: &[FileAttachment]) -> Vec<FileAttachment> {
    let mut merged = current.to_vec();
    for file in added {
        if !merged.iter().any(|known| known.id == file.id) {
            merged.push(file.clone());
        }
    }
    merged
}

/// `bytesToBase64`.
pub fn bytes_to_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(TABLE[(group >> 18) as usize & 63] as char);
        out.push(TABLE[(group >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(group >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[group as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, media_type: &str, bytes: &[u8]) -> ComposerFile {
        ComposerFile { name: name.into(), media_type: media_type.into(), bytes: bytes.to_vec() }
    }

    #[test]
    fn sniffs_by_bytes_not_by_name() {
        assert_eq!(sniff_attachment_bytes(b""), ByteSniff::Empty);
        assert_eq!(sniff_attachment_bytes(&[0xff, 0xd8, 0xff, 0x00]), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes(b"RIFF1234WEBPxxxx"), ByteSniff::Image);
        assert_eq!(sniff_attachment_bytes(b"\n\n%PDF-1.7"), ByteSniff::Pdf);
        assert_eq!(sniff_attachment_bytes("文本".as_bytes()), ByteSniff::Text);
        assert_eq!(sniff_attachment_bytes(b"a\0b"), ByteSniff::Binary);
        assert_eq!(sniff_attachment_bytes(&[0xff, 0xfe, b'a', 0]), ByteSniff::Text);
    }

    #[test]
    fn sorts_a_batch_and_keeps_later_files_that_still_fit() {
        let intake = sort_attachments(
            vec![file("empty", "", b""), file("a.png", "image/png", b"x"), file("a.txt", "", b"hello"), file("a.bin", "", b"a\0b")],
            false,
            MessageAttachmentRoom::new(&[], &[]),
        );
        assert!(matches!(&intake[0], Intake::Rejected(AttachmentRejection { reason: AttachmentRejectionReason::Empty, .. })));
        assert!(matches!(&intake[1], Intake::Rejected(AttachmentRejection { reason: AttachmentRejectionReason::ImageInputUnavailable, .. })));
        assert!(matches!(&intake[2], Intake::Document { kind: DocumentKind::Text, .. }));
        assert!(matches!(&intake[3], Intake::Rejected(AttachmentRejection { reason: AttachmentRejectionReason::Unsupported, .. })));
        let mut room = MessageAttachmentRoom(10);
        assert!(!room.fits(11));
        assert!(room.fits(10));
    }

    #[test]
    fn a_long_text_file_goes_as_its_first_lines_or_not_at_all() {
        let lines = "a\n".repeat(TRUNCATED_TEXT_FILE_LINES + 200_000);
        let (head, truncated) = text_for_model(&lines).expect("the first lines fit");
        assert!(truncated);
        assert_eq!(head.lines().count(), TRUNCATED_TEXT_FILE_LINES);
        let one_line = "a".repeat(200_000);
        assert_eq!(text_for_model(&one_line), None);
    }

    #[test]
    fn a_pdf_reports_that_it_cannot_be_read_rather_than_attaching_without_text() {
        let rejected = read_document(file("a.pdf", "", b"%PDF-1.7"), DocumentKind::Pdf).unwrap_err();
        assert_eq!(rejected.reason, AttachmentRejectionReason::PdfUnreadable);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pdf_with_text_comes_back_with_that_text_page_by_page() {
        // One page, one line, from nothing but the file format.
        let stream = "BT /F1 18 Tf 20 100 Td (Hello) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", index + 1).as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes());

        let document = read_document(file("a.pdf", "application/pdf", &pdf), DocumentKind::Pdf).unwrap();
        assert_eq!(document.format, "pdf");
        let extracted = document.extracted.unwrap();
        assert_eq!(extracted.pages, 1);
        assert!(extracted.text.starts_with("[Page 1]\n") && extracted.text.contains("Hello"), "{extracted:?}");
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(bytes_to_base64(b""), "");
        assert_eq!(bytes_to_base64(b"f"), "Zg==");
        assert_eq!(bytes_to_base64(b"fo"), "Zm8=");
        assert_eq!(bytes_to_base64(b"foo"), "Zm9v");
        assert_eq!(bytes_to_base64(&[0xfb, 0xff]), "+/8=");
    }
}
