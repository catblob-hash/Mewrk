//! The clipboard over RFB: the classic cut-text messages, Latin-1 only, and
//! the Extended Clipboard pseudo-encoding (0xC0A1E5CE), which carries UTF-8 and
//! lets a client ask for the server's clipboard (research-rfb §2; rfbproto,
//! "Extended Clipboard Pseudo-Encoding", first implemented by UltraVNC).
//!
//! Both use the same two messages, ServerCutText (3) and ClientCutText (6),
//! whose length is read as signed: zero or more is the classic message, that
//! many bytes of ISO 8859-1 with LF line ends; a negative one is the extended
//! message, `abs(length)` bytes that start with a u32 of flags — the formats
//! (bit 0 text; RTF, HTML, DIB and files above it) and one action: caps,
//! request, peek, notify or provide. A provide's payload is a zlib stream that
//! holds, for each format flagged in ascending bit order, a u32 size and that
//! many bytes; text is UTF-8 with CRLF line ends and a terminating NUL that the
//! size counts.
//!
//! Servers differ in what they send (research-rfb §2.3), and this side is
//! lenient where they do: a provide's stream need not end, since TigerVNC
//! flushes it with Z_SYNC_FLUSH and never finishes it; its text need not end
//! in a NUL, since QEMU's does not; and its line ends become LF. What this side
//! sends is the strict form every server read accepts: a finished zlib stream,
//! CRLF line ends, the NUL counted.

use std::io::Write;

use flate2::{write::ZlibEncoder, Compression, Decompress, FlushDecompress, Status};

/// The text format, the lowest of the format bits (0-15).
pub const TEXT: u32 = 1;
/// The sender lists the formats and actions it takes, and after the flags one
/// u32 per format: the most of it the other side may send unasked.
pub const CAPS: u32 = 1 << 24;
/// The sender asks for the other side's clipboard in the formats flagged.
pub const REQUEST: u32 = 1 << 25;
/// The sender asks which formats the other side's clipboard holds.
pub const PEEK: u32 = 1 << 26;
/// The sender's clipboard changed and holds the formats flagged (none: it is
/// empty).
pub const NOTIFY: u32 = 1 << 27;
/// The sender's clipboard itself, in the formats flagged.
pub const PROVIDE: u32 = 1 << 28;
/// The action bits.
pub const ACTIONS: u32 = 0xff00_0000;

/// The start of an extended ClientCutText of `body` bytes after the length.
fn extended_header(body: usize) -> Vec<u8> {
    let mut message = Vec::with_capacity(8 + body);
    message.extend_from_slice(&[6, 0, 0, 0]);
    // Every body this side sends is far below 2 GiB.
    message.extend_from_slice(&(-(body as i32)).to_be_bytes());
    message
}

/// The client's caps, sent in answer to each of the server's: text only, with
/// request, peek, notify and provide, and nothing sent unasked (a size of 0:
/// the server announces a change and the client asks for it). TigerVNC assumes
/// a client takes no clipboard actions at all until it has these
/// (research-rfb §2.2, §7.1).
/// `06 00 00 00 FF FF FF F8 1F 00 00 01 00 00 00 00`.
pub fn client_caps() -> Vec<u8> {
    let mut message = extended_header(8);
    message.extend_from_slice(&(CAPS | REQUEST | PEEK | NOTIFY | PROVIDE | TEXT).to_be_bytes());
    message.extend_from_slice(&0u32.to_be_bytes());
    message
}

/// A request, peek or notify for the text format: `06 00 00 00 FF FF FF FC`
/// and the flags (`02 00 00 01` for a request, `08 00 00 01` for a notify).
pub fn client_action(action: u32) -> Vec<u8> {
    let mut message = extended_header(4);
    message.extend_from_slice(&(action | TEXT).to_be_bytes());
    message
}

/// `text` with CRLF line ends, as a provide carries it.
fn with_crlf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The size a provide of `text` declares, before compression: its UTF-8 with
/// CRLF line ends, and the NUL.
pub fn provided_size(text: &str) -> usize {
    with_crlf(text).len() + 1
}

/// A provide of `text`: UTF-8 with CRLF line ends and a terminating NUL that
/// its size counts, in one finished zlib stream (Z_FINISH), which every server
/// read takes; neatvnc drops a text that does not end in NUL.
pub fn client_provide(text: &str) -> Vec<u8> {
    let crlf = with_crlf(text);
    let mut payload = Vec::with_capacity(crlf.len() + 5);
    payload.extend_from_slice(&((crlf.len() + 1) as u32).to_be_bytes());
    payload.extend_from_slice(crlf.as_bytes());
    payload.push(0);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    // Writing to and finishing an encoder over a Vec cannot fail.
    let compressed = encoder
        .write_all(&payload)
        .and_then(|()| encoder.finish())
        .unwrap_or_default();
    let mut message = extended_header(4 + compressed.len());
    message.extend_from_slice(&(PROVIDE | TEXT).to_be_bytes());
    message.extend_from_slice(&compressed);
    message
}

/// The first character of `text` that Latin-1 cannot carry, if any.
pub fn beyond_latin1(text: &str) -> Option<char> {
    text.chars().find(|character| u32::from(*character) > 0xff)
}

/// A classic ClientCutText of `text`, in Latin-1 with LF line ends; `None`
/// when a character is beyond Latin-1.
pub fn latin1_cut_text(text: &str) -> Option<Vec<u8>> {
    if beyond_latin1(text).is_some() {
        return None;
    }
    let lf = text.replace("\r\n", "\n");
    let bytes: Vec<u8> = lf.chars().map(|character| character as u8).collect();
    let mut message = Vec::with_capacity(8 + bytes.len());
    message.extend_from_slice(&[6, 0, 0, 0]);
    message.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    message.extend_from_slice(&bytes);
    Some(message)
}

/// The text of a classic ServerCutText: Latin-1, line ends made LF.
pub fn latin1_text(bytes: &[u8]) -> String {
    let text: String = bytes.iter().map(|byte| char::from(*byte)).collect();
    text.replace("\r\n", "\n")
}

/// A provide's text, from the zlib stream after its flags: `None` when the
/// provide carries no text. At most `keep` bytes of the text are inflated and
/// kept; the stream is read without needing its end, so TigerVNC's
/// sync-flushed one reads as well as a finished one. One trailing NUL goes
/// (QEMU sends none), invalid UTF-8 is replaced, and CRLF becomes LF.
pub fn provided_text(flags: u32, compressed: &[u8], keep: usize) -> Result<Option<String>, String> {
    if flags & TEXT == 0 {
        return Ok(None);
    }
    // Text is the lowest format bit, so it comes first in the stream: its size,
    // then its bytes. Nothing after them is inflated.
    let size = |inflated: &[u8]| -> Option<usize> {
        let declared: [u8; 4] = inflated.get(..4)?.try_into().ok()?;
        Some(u32::from_be_bytes(declared) as usize)
    };
    let wanted = |inflated: &[u8]| size(inflated).map(|size| 4 + size.min(keep));
    let mut inflate = Decompress::new(true);
    let mut inflated: Vec<u8> = Vec::new();
    let mut input = compressed;
    let mut chunk = vec![0u8; 32 * 1024];
    loop {
        if wanted(&inflated).is_some_and(|wanted| inflated.len() >= wanted) {
            break;
        }
        let (read_before, written_before) = (inflate.total_in(), inflate.total_out());
        let status = inflate
            .decompress(input, &mut chunk, FlushDecompress::Sync)
            .map_err(|error| format!("its compressed data is corrupt ({error})"))?;
        let read = (inflate.total_in() - read_before) as usize;
        let written = (inflate.total_out() - written_before) as usize;
        inflated.extend_from_slice(&chunk[..written]);
        input = &input[read..];
        if status == Status::StreamEnd || (read == 0 && written == 0) {
            break;
        }
    }
    let (Some(size), Some(wanted)) = (size(&inflated), wanted(&inflated)) else {
        return Err("its text is shorter than its size says".to_owned());
    };
    if inflated.len() < wanted {
        return Err("its text is shorter than its size says".to_owned());
    }
    let text = &inflated[4..wanted];
    let text = if size <= keep {
        String::from_utf8_lossy(text.strip_suffix(&[0]).unwrap_or(text)).into_owned()
    } else {
        // Cut, perhaps inside a character: its first bytes are dropped, not
        // shown as U+FFFD.
        let valid = match std::str::from_utf8(text) {
            Ok(_) => text.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => text.len(),
        };
        String::from_utf8_lossy(&text[..valid]).into_owned()
    };
    Ok(Some(text.replace("\r\n", "\n")))
}

#[cfg(test)]
mod tests {
    use flate2::{Compress, FlushCompress};

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The bytes of research-rfb §2.2, checked there against an independent Python encoder.
    #[test]
    fn the_client_messages_are_the_specs_bytes() {
        assert_eq!(
            hex(&client_caps()),
            "06 00 00 00 FF FF FF F8 1F 00 00 01 00 00 00 00"
        );
        assert_eq!(
            hex(&client_action(NOTIFY)),
            "06 00 00 00 FF FF FF FC 08 00 00 01"
        );
        assert_eq!(
            hex(&client_action(REQUEST)),
            "06 00 00 00 FF FF FF FC 02 00 00 01"
        );
        let provide = client_provide("a\nb");
        assert_eq!(hex(&provide[..4]), "06 00 00 00");
        let length = i32::from_be_bytes(provide[4..8].try_into().unwrap());
        assert_eq!(length.unsigned_abs() as usize, provide.len() - 8);
        assert_eq!(hex(&provide[8..12]), "10 00 00 01");
        // A finished stream: it inflates to its end.
        let mut inflate = Decompress::new(true);
        let mut payload = Vec::with_capacity(64);
        let status = inflate
            .decompress_vec(&provide[12..], &mut payload, FlushDecompress::Finish)
            .unwrap();
        assert_eq!(status, Status::StreamEnd);
        assert_eq!(payload, [0, 0, 0, 5, b'a', b'\r', b'\n', b'b', 0]);
        // The size counts what goes: CRLF line ends, the NUL, UTF-8 bytes.
        assert_eq!(provided_size("a\nb"), 5);
        assert_eq!(provided_size("a\r\nb\r\r\n"), 8);
        assert_eq!(provided_size(""), 1);
        assert_eq!(provided_size("中😀"), 8);
    }

    #[test]
    fn a_sync_flushed_stream_without_its_end_inflates() {
        let mut payload = 12u32.to_be_bytes().to_vec();
        payload.extend_from_slice(b"tiger\r\nvnc!\0");
        let mut compress = Compress::new(Compression::default(), true);
        let mut compressed = Vec::with_capacity(256);
        compress
            .compress_vec(&payload, &mut compressed, FlushCompress::Sync)
            .unwrap();
        assert_eq!(
            provided_text(PROVIDE | TEXT, &compressed, 1024).unwrap(),
            Some("tiger\nvnc!".to_owned())
        );
        // Kept to its first bytes when it is longer than what is kept.
        assert_eq!(
            provided_text(PROVIDE | TEXT, &compressed, 5).unwrap(),
            Some("tiger".to_owned())
        );
        // A finished stream (QEMU's), whose text has no NUL.
        let mut payload = 6u32.to_be_bytes().to_vec();
        payload.extend_from_slice(b"a\r\nb\r\n");
        let mut compress = Compress::new(Compression::default(), true);
        let mut finished = Vec::with_capacity(256);
        compress
            .compress_vec(&payload, &mut finished, FlushCompress::Finish)
            .unwrap();
        assert_eq!(
            provided_text(PROVIDE | TEXT, &finished, 1024).unwrap(),
            Some("a\nb\n".to_owned())
        );
        // A provide of another format carries no text; a cut stream is refused.
        assert_eq!(provided_text(PROVIDE | 2, &compressed, 1024).unwrap(), None);
        assert!(provided_text(PROVIDE | TEXT, &compressed[..4], 1024).is_err());
        assert!(provided_text(PROVIDE | TEXT, b"not zlib at all", 1024).is_err());
    }

    #[test]
    fn latin1_goes_both_ways_with_lf() {
        assert_eq!(
            latin1_cut_text("café\r\nx").unwrap(),
            [6, 0, 0, 0, 0, 0, 0, 6, b'c', b'a', b'f', 0xe9, b'\n', b'x']
        );
        assert_eq!(latin1_cut_text("中"), None);
        assert_eq!(beyond_latin1("ÿ ok 中 €"), Some('中'));
        assert_eq!(latin1_text(&[b'h', 0xe9, b'\r', b'\n', b'!']), "hé\n!");
    }
}
