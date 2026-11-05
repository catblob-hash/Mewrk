//! The text half of `read`: a window of lines out of a file of any size.
//!
//! Claude Code's `Read` does not fail on a file that is not valid UTF-8, does
//! not refuse a file for being large, and answers a range of a big file by
//! streaming to it. This is the same, shared by the host leg and the remote one
//! (which holds the bytes it was sent in memory and reads them through the same
//! code), so a file reads the same on whichever machine it is on.
//!
//! The bytes are decoded as they stream by, never held whole:
//!
//! * A UTF-16 byte-order mark (either order) selects UTF-16, decoded to UTF-8 on
//!   the fly. Anything else is UTF-8, decoded lossily line by line: an invalid
//!   byte becomes U+FFFD, and the result says the file cannot be edited
//!   (`edit` reads strict UTF-8 and would refuse it).
//! * A NUL in the first [`SNIFF_BYTES`] of anything not UTF-16 is a binary
//!   file, refused outright instead of poured into the context. This is the
//!   rule `grep` already skips binary files by.
//! * Lines before the range are counted and dropped, the range is kept, and
//!   lines after it are only counted — and only when the file is small enough
//!   for that to be cheap ([`COUNT_SCAN_LIMIT`]).
//! * One read never returns more than [`READ_DEFAULT_LINES`] lines (or
//!   [`READ_MAX_LINES`] when the call names an end) and [`READ_MAX_BYTES`] bytes;
//!   a read that stops short says where to continue. A line that alone is more
//!   than a read can return is cut and says how much was left out.

use std::borrow::Cow;
use std::io::{self, BufRead, BufReader, Chain, Cursor, Read};

use crate::prompt_profile::{PromptKey, PromptProfile};

/// Lines a `read` without `end_line` returns: Claude Code's default.
pub(crate) const READ_DEFAULT_LINES: usize = 2_000;
/// Most lines one `read` returns, whatever range it asked for.
pub(crate) const READ_MAX_LINES: u64 = 5_001;
/// Bytes of text one `read` returns, whatever its range; a range past it is
/// cut at a line boundary and says where to continue.
pub(crate) const READ_MAX_BYTES: usize = 60 * 1024;
/// How much of the start of a file is looked at for a NUL, the figure `grep`
/// uses to skip binary files — and `grep` reads its files with the same
/// [`read_line`], so a file is cut into lines the same way by both.
pub(crate) const SNIFF_BYTES: usize = 8 * 1024;
/// Files up to this size are scanned to their end when a read stops short, so
/// the continuation note can name the total. Scanning is a pass over the file at
/// disk speed — tens of milliseconds at this size — but a log of many gigabytes
/// is not worth that for a number, and its note says the total was not counted.
pub(crate) const COUNT_SCAN_LIMIT: u64 = 64 * 1024 * 1024;
/// The most bytes a read goes through to reach its first line. A range this
/// deep into a file is what `tail` and `sed` are for, and without a limit a
/// request for line 10,000,000,000 would read to the end of whatever it was
/// given.
const SKIP_SCAN_LIMIT: u64 = 1024 * 1024 * 1024;
/// Room kept below the byte budget for the marker that closes a cut line.
const CUT_MARKER_RESERVE: usize = 128;
/// How much of the stream is buffered at a time.
pub(crate) const IO_CHUNK: usize = 64 * 1024;

/// What a text `read` shows for its range.
pub(crate) struct TextRead {
    pub output: String,
    /// The file's whole text, normalized as the read record keeps it, when this
    /// read put all of it in front of the model: no range, nothing cut, nothing
    /// lossy. Only such a read can vouch for the file.
    pub whole: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
        }
    }
}

/// Reads the window `start_line..=end_line` (1-based; `end_line` is
/// `u64::MAX` when the call did not bound it) of the text in `source`, the
/// whole file from its first byte.
///
/// `size` is the file's length in bytes when it is known; it only decides
/// whether the total line count is worth a pass over the rest of the file.
/// `path` is for the refusal of a binary file.
pub(crate) fn read_lines<R: Read>(
    source: R,
    size: Option<u64>,
    path: &str,
    start_line: u64,
    end_line: u64,
    profile: &PromptProfile,
) -> Result<TextRead, String> {
    read_stream(source, size, path, start_line, end_line, None, profile)
}

/// The most lines one read of `start_line..=end_line` returns: the default
/// length when the call named no end, otherwise the range, never more than
/// [`READ_MAX_LINES`].
pub(crate) fn lines_per_read(start_line: u64, end_line: u64) -> u64 {
    if end_line == u64::MAX {
        READ_DEFAULT_LINES as u64
    } else {
        (end_line - (start_line - 1)).min(READ_MAX_LINES)
    }
}

/// What a machine that cut the lines out of a big file itself says about the
/// text it sent.
pub(crate) struct Window {
    /// The file's line number of the first line sent.
    pub base_line: u64,
    /// The file's line count, when the machine counted it.
    pub total: Option<u64>,
    /// Whether the file goes on after what was sent: more lines than the
    /// machine was asked for, or the bytes it was allowed to send ran out.
    pub more_after: bool,
}

/// [`read_lines`] for text that is not the file from its start but the part of
/// it a remote machine cut out ([`Window`]). The sniffing for binary data is the
/// machine's to do, as the head of the file is not what arrives.
pub(crate) fn read_window<R: Read>(
    source: R,
    path: &str,
    start_line: u64,
    end_line: u64,
    window: Window,
    profile: &PromptProfile,
) -> Result<TextRead, String> {
    read_stream(source, None, path, start_line, end_line, Some(window), profile)
}

fn read_stream<R: Read>(
    source: R,
    size: Option<u64>,
    path: &str,
    start_line: u64,
    end_line: u64,
    window: Option<Window>,
    profile: &PromptProfile,
) -> Result<TextRead, String> {
    let io_error = |error: io::Error| format!("Failed to read text file {path}: {error}");
    // The head of the file is what says what it is. A part of a big file that a
    // machine cut out is not the head: the machine looked there already.
    let (mut reader, encoding) = open_text(source, path, window.is_none(), profile)?;
    let reader: &mut dyn BufRead = &mut *reader;
    let unbounded = end_line == u64::MAX;
    let whole_candidate = window.is_none() && start_line == 1 && unbounded;

    // Lines of the file before the first one shown, and lines of the stream to
    // go through to reach it: the same, unless the stream starts partway in.
    let before = start_line - 1;
    let start_index = start_line - window.as_ref().map_or(1, |window| window.base_line);
    let mut scanned = 0_u64;
    let (skipped, dangling) =
        skip_lines(reader, start_index, &mut scanned).map_err(io_error)?;
    if scanned > SKIP_SCAN_LIMIT {
        return Err(format!(
            "Line {start_line} is more than {} GiB into the file. Use a shell command such as tail or sed to reach it.",
            SKIP_SCAN_LIMIT / 1024 / 1024 / 1024
        ));
    }
    if skipped < start_index {
        // The file ended inside the lines being skipped.
        return Ok(nothing(skipped + u64::from(dangling), whole_candidate, profile));
    }

    let lines_wanted = lines_per_read(start_line, end_line);
    let mut shown: Vec<String> = Vec::new();
    let mut bytes_used = 0_usize;
    let mut normalized = String::new();
    let mut lossy = false;
    let mut cut = false;
    let mut at_eof = false;
    // A line that was read and did not fit is not shown, but it is a line of
    // the file when the rest is counted.
    let mut unfit = 0_u64;
    while (shown.len() as u64) < lines_wanted {
        let Some(line) = read_line(reader, READ_MAX_BYTES).map_err(io_error)? else {
            at_eof = true;
            break;
        };
        let text = match std::str::from_utf8(&line.bytes) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => {
                lossy = true;
                String::from_utf8_lossy(&line.bytes)
            }
        };
        let fits_alone = line.dropped_bytes == 0 && text.len() < READ_MAX_BYTES;
        if fits_alone {
            let cost = text.len() + 1;
            if bytes_used + cost > READ_MAX_BYTES {
                unfit = 1;
                break;
            }
            bytes_used += cost;
            if whole_candidate {
                normalized.push_str(&text);
                if line.ended {
                    normalized.push('\n');
                }
            }
            shown.push(text.into_owned());
        } else if shown.is_empty() {
            // Longer than a whole read can return: cut to the budget and say
            // how much was left, rather than refuse the file for one line.
            let room = READ_MAX_BYTES - 1 - CUT_MARKER_RESERVE;
            let mut end = room.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let omitted = text[end..].chars().count() as u64 + line.dropped_chars;
            let mut shown_line = text[..end].to_owned();
            shown_line.push_str(&profile.render(
                PromptKey::ToolReadLineTruncated,
                &[("omitted", &omitted.to_string())],
            ));
            shown.push(shown_line);
            bytes_used = READ_MAX_BYTES;
            cut = true;
        } else {
            // It would fit a read of its own, or be cut in one: leave it for
            // the read that continues here.
            unfit = 1;
            break;
        }
    }
    if shown.is_empty() {
        // Nothing at the start line: the file has fewer lines than that.
        let total = window.as_ref().and_then(|window| window.total).unwrap_or(before);
        return Ok(nothing(total, whole_candidate, profile));
    }

    let emitted = shown.len() as u64;
    // Done, or stopped by a cap? A read that reached the end the call asked
    // for is done wherever the file goes on; otherwise the file's end decides.
    let reached_asked_end = !unbounded && emitted == end_line - before;
    if !at_eof && unfit == 0 && !reached_asked_end {
        at_eof = reader.fill_buf().map_err(io_error)?.is_empty();
    }
    // A machine that sent a part of the file says whether the file goes on.
    let more_after = window.as_ref().is_some_and(|window| window.more_after);
    let stopped_early = (!at_eof || more_after) && !reached_asked_end;

    let mut output = shown.join("\n");
    if lossy {
        output.push_str(profile.text(PromptKey::ToolReadLossyNote));
    }
    if encoding != Encoding::Utf8 {
        output.push_str(&profile.render(
            PromptKey::ToolReadUtf16Note,
            &[("encoding", encoding.label())],
        ));
    }
    if stopped_early {
        let last = before + emitted;
        let from = start_line.to_string();
        let to = last.to_string();
        let next = (last + 1).to_string();
        // What is left is counted by going through it, which only a stream
        // that is the file itself, and small, is worth; a machine's part of a
        // big file brings the total it counted, or none.
        let total = match &window {
            Some(window) => window.total,
            None if size.is_none_or(|size| size <= COUNT_SCAN_LIMIT) => {
                Some(last + unfit + count_lines(reader).map_err(io_error)?)
            }
            None => None,
        };
        if let Some(total) = total {
            output.push_str(&profile.render(
                PromptKey::ToolReadLimit,
                &[
                    ("from", &from),
                    ("to", &to),
                    ("total", &total.to_string()),
                    ("next", &next),
                ],
            ));
        } else {
            output.push_str(&profile.render(
                PromptKey::ToolReadLimitUncounted,
                &[("from", &from), ("to", &to), ("next", &next)],
            ));
        }
    }
    let whole = (whole_candidate
        && !stopped_early
        && !lossy
        && !cut
        && encoding == Encoding::Utf8)
        .then(|| crate::file_read_state::normalize_text(&normalized));
    Ok(TextRead { output, whole })
}

/// The answer for a range that starts past the end of a file of `total` lines.
fn nothing(total: u64, whole_candidate: bool, profile: &PromptProfile) -> TextRead {
    if total == 0 {
        // An empty file read without a range is a full read of it.
        return TextRead {
            output: profile.text(PromptKey::ToolReadEmptyFile).to_owned(),
            whole: whole_candidate.then(String::new),
        };
    }
    TextRead {
        output: profile.render(
            PromptKey::ToolReadRangeOutOfBounds,
            &[("total", &total.to_string())],
        ),
        whole: None,
    }
}

/// The decoded stream, and what it was decoded from. Refuses binary data, when
/// `sniff` says the stream starts the file.
fn open_text<'a, R: Read + 'a>(
    mut source: R,
    path: &str,
    sniff: bool,
    profile: &PromptProfile,
) -> Result<(Box<dyn BufRead + 'a>, Encoding), String> {
    if !sniff {
        return Ok((Box::new(BufReader::with_capacity(IO_CHUNK, source)), Encoding::Utf8));
    }
    let mut head = Vec::with_capacity(SNIFF_BYTES);
    (&mut source)
        .take(SNIFF_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|error| format!("Failed to read text file {path}: {error}"))?;
    let (encoding, mark) = match head.as_slice() {
        [0xFF, 0xFE, ..] => (Encoding::Utf16Le, 2),
        [0xFE, 0xFF, ..] => (Encoding::Utf16Be, 2),
        _ => (Encoding::Utf8, 0),
    };
    // UTF-16 text is full of NULs; the mark is what says it is text.
    if encoding == Encoding::Utf8 && head.contains(&0) {
        return Err(profile.render(PromptKey::ToolReadBinary, &[("path", path)]));
    }
    let mut head = Cursor::new(head);
    head.set_position(mark);
    let joined: Chain<Cursor<Vec<u8>>, R> = head.chain(source);
    Ok(match encoding {
        Encoding::Utf8 => (Box::new(BufReader::with_capacity(IO_CHUNK, joined)), encoding),
        Encoding::Utf16Le | Encoding::Utf16Be => (
            Box::new(BufReader::with_capacity(
                IO_CHUNK,
                Utf16Decoder::new(joined, encoding == Encoding::Utf16Be),
            )),
            encoding,
        ),
    })
}

/// One line of the stream, at most `cap` bytes of it kept.
pub(crate) struct Line {
    pub(crate) bytes: Vec<u8>,
    /// Bytes past `cap`, which were read and dropped.
    pub(crate) dropped_bytes: u64,
    /// Characters among them (counted by their first bytes).
    pub(crate) dropped_chars: u64,
    /// Whether the line ended in a newline rather than in the end of the file.
    pub(crate) ended: bool,
}

/// The next line without its terminator (a `\r` before the `\n` goes with it,
/// as `str::lines` does it), or `None` at the end of the stream. Memory is
/// bounded by `cap` whatever the line's length.
pub(crate) fn read_line(reader: &mut dyn BufRead, cap: usize) -> io::Result<Option<Line>> {
    let mut line = Line {
        bytes: Vec::new(),
        dropped_bytes: 0,
        dropped_chars: 0,
        ended: false,
    };
    let mut any = false;
    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        any = true;
        let (take, found) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(index) => (index, true),
            None => (chunk.len(), false),
        };
        let keep = cap.saturating_sub(line.bytes.len()).min(take);
        line.bytes.extend_from_slice(&chunk[..keep]);
        line.dropped_bytes += (take - keep) as u64;
        line.dropped_chars += count_characters(&chunk[keep..take]);
        reader.consume(take + usize::from(found));
        if found {
            line.ended = true;
            break;
        }
    }
    if !any {
        return Ok(None);
    }
    if line.dropped_bytes > 0 {
        drop_partial_character(&mut line);
    } else if line.ended && line.bytes.last() == Some(&b'\r') {
        line.bytes.pop();
    }
    Ok(Some(line))
}

/// Characters in `bytes`, counted by the bytes that start one.
fn count_characters(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|byte| (**byte & 0xC0) != 0x80).count() as u64
}

/// A cut line must not end halfway through a character: that half would read
/// as U+FFFD and make a valid file look lossy.
fn drop_partial_character(line: &mut Line) {
    let length = line.bytes.len();
    if length == 0 {
        return;
    }
    let mut start = length;
    while start > 0 && length - start < 4 {
        start -= 1;
        if (line.bytes[start] & 0xC0) != 0x80 {
            break;
        }
    }
    let needs = match line.bytes.get(start).copied() {
        Some(0xC0..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF7) => 4,
        _ => 1,
    };
    if length - start < needs {
        line.dropped_bytes += (length - start) as u64;
        line.dropped_chars += 1;
        line.bytes.truncate(start);
    }
}

/// Discards `count` lines. Returns how many it found before the end of the
/// stream and whether the stream ended in a line with no terminator (one more
/// line than were counted). `scanned` is the number of bytes gone through.
fn skip_lines(
    reader: &mut dyn BufRead,
    count: u64,
    scanned: &mut u64,
) -> io::Result<(u64, bool)> {
    let mut skipped = 0_u64;
    let mut dangling = false;
    while skipped < count {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let mut used = 0;
        while skipped < count && used < chunk.len() {
            match chunk[used..].iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    used += index + 1;
                    skipped += 1;
                    dangling = false;
                }
                None => {
                    used = chunk.len();
                    dangling = true;
                }
            }
        }
        reader.consume(used);
        *scanned += used as u64;
        if *scanned > SKIP_SCAN_LIMIT {
            break;
        }
    }
    Ok((skipped, dangling))
}

/// The lines left in the stream.
fn count_lines(reader: &mut dyn BufRead) -> io::Result<u64> {
    let mut lines = 0_u64;
    // A stream that ends on a newline (or is empty) has no unfinished line.
    let mut last = b'\n';
    loop {
        let chunk = reader.fill_buf()?;
        let Some(final_byte) = chunk.last().copied() else {
            break;
        };
        lines += chunk.iter().filter(|byte| **byte == b'\n').count() as u64;
        last = final_byte;
        let length = chunk.len();
        reader.consume(length);
    }
    Ok(lines + u64::from(last != b'\n'))
}

/// UTF-16 read as UTF-8, a chunk at a time. A lone surrogate, or a stray last
/// byte, becomes U+FFFD, as lossy decoding does for UTF-8.
struct Utf16Decoder<R: Read> {
    inner: R,
    big_endian: bool,
    odd_byte: Option<u8>,
    high_surrogate: Option<u16>,
    decoded: Vec<u8>,
    position: usize,
    finished: bool,
}

impl<R: Read> Utf16Decoder<R> {
    fn new(inner: R, big_endian: bool) -> Self {
        Self {
            inner,
            big_endian,
            odd_byte: None,
            high_surrogate: None,
            decoded: Vec::new(),
            position: 0,
            finished: false,
        }
    }

    fn push(&mut self, character: char) {
        let mut buffer = [0_u8; 4];
        self.decoded
            .extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
    }

    fn push_unit(&mut self, unit: u16) {
        match self.high_surrogate.take() {
            Some(high) if (0xDC00..=0xDFFF).contains(&unit) => {
                let scalar = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                self.push(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            Some(_) => {
                self.push(char::REPLACEMENT_CHARACTER);
                self.push_unit(unit);
            }
            None => match unit {
                0xD800..=0xDBFF => self.high_surrogate = Some(unit),
                0xDC00..=0xDFFF => self.push(char::REPLACEMENT_CHARACTER),
                _ => self.push(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)),
            },
        }
    }

    /// Decodes one more chunk of the source into `decoded`.
    fn refill(&mut self) -> io::Result<()> {
        self.decoded.clear();
        self.position = 0;
        let mut chunk = [0_u8; 8192];
        let read = self.inner.read(&mut chunk)?;
        if read == 0 {
            self.finished = true;
            if self.high_surrogate.take().is_some() || self.odd_byte.take().is_some() {
                self.push(char::REPLACEMENT_CHARACTER);
            }
            return Ok(());
        }
        for &byte in &chunk[..read] {
            match self.odd_byte.take() {
                None => self.odd_byte = Some(byte),
                Some(first) => {
                    let pair = [first, byte];
                    let unit = if self.big_endian {
                        u16::from_be_bytes(pair)
                    } else {
                        u16::from_le_bytes(pair)
                    };
                    self.push_unit(unit);
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Utf16Decoder<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.position >= self.decoded.len() {
            if self.finished {
                return Ok(0);
            }
            self.refill()?;
        }
        let available = &self.decoded[self.position..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.position += count;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn english() -> PromptProfile {
        PromptProfile::builtin()
    }

    fn read(bytes: &[u8], start: u64, end: u64) -> Result<TextRead, String> {
        read_lines(bytes, Some(bytes.len() as u64), "f.txt", start, end, &english())
    }

    fn numbered(count: usize) -> Vec<u8> {
        (1..=count).map(|n| format!("line {n}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn a_whole_small_file_reads_as_one_record() {
        let read = read(b"alpha\r\nbeta\ngamma", 1, u64::MAX).unwrap();
        assert_eq!(read.output, "alpha\nbeta\ngamma");
        // The record is the normalized text, including whether it ended in a
        // newline, so the edit gate hashes what is on disk.
        assert_eq!(read.whole.as_deref(), Some("alpha\nbeta\ngamma"));
        let read = self::read(b"alpha\nbeta\n", 1, u64::MAX).unwrap();
        assert_eq!(read.whole.as_deref(), Some("alpha\nbeta\n"));
    }

    #[test]
    fn an_empty_file_and_a_range_past_the_end_are_told_apart() {
        let empty = read(b"", 1, u64::MAX).unwrap();
        assert_eq!(empty.output, english().text(PromptKey::ToolReadEmptyFile));
        assert_eq!(empty.whole.as_deref(), Some(""));
        let ranged = read(b"", 3, 9).unwrap();
        assert_eq!(ranged.output, english().text(PromptKey::ToolReadEmptyFile));
        assert!(ranged.whole.is_none(), "a slice of nothing vouches for nothing");

        let past = read(b"one\ntwo\n", 5, u64::MAX).unwrap();
        assert!(past.output.contains("only 2 lines"), "{}", past.output);
        assert!(past.whole.is_none());
        // Exactly one past the last line is past it too.
        assert!(read(b"one\ntwo\n", 3, u64::MAX).unwrap().output.contains("only 2 lines"));
        assert!(read(b"one\ntwo", 3, u64::MAX).unwrap().output.contains("only 2 lines"));
        assert_eq!(read(b"one\ntwo", 2, u64::MAX).unwrap().output, "two");
    }

    #[test]
    fn a_range_selects_lines_without_noting_the_rest() {
        let bytes = numbered(10);
        let read = read(&bytes, 3, 5).unwrap();
        assert_eq!(read.output, "line 3\nline 4\nline 5");
        assert!(read.whole.is_none());
    }

    #[test]
    fn the_default_length_stops_short_and_names_the_continuation_and_the_total() {
        let bytes = numbered(READ_DEFAULT_LINES + 50);
        let read = read(&bytes, 1, u64::MAX).unwrap();
        assert!(read.output.starts_with("line 1\n"));
        assert!(read.output.contains(&format!("line {READ_DEFAULT_LINES}\n")));
        assert!(
            read.output.ends_with(&format!(
                "… showing lines 1–{READ_DEFAULT_LINES} of {}. Continue with start_line={}.",
                READ_DEFAULT_LINES + 50,
                READ_DEFAULT_LINES + 1
            )),
            "{}",
            &read.output[read.output.len() - 120..]
        );
        assert!(read.whole.is_none());
        // Exactly the default length is the whole file: nothing to continue.
        let exact = numbered(READ_DEFAULT_LINES);
        let read = self::read(&exact, 1, u64::MAX).unwrap();
        assert!(!read.output.contains("showing lines"));
        assert!(read.whole.is_some());
    }

    #[test]
    fn a_named_end_reads_up_to_five_thousand_and_one_lines() {
        let bytes = numbered(7000);
        let read = read(&bytes, 1, 6500).unwrap();
        assert!(read.output.contains("line 5001\n"));
        assert!(!read.output.contains("line 5002\n"), "{}", &read.output[read.output.len() - 200..]);
        assert!(read.output.contains("showing lines 1–5001 of 7000"));
        // A range that ends inside the cap is not "stopped early" even though
        // the file goes on.
        let read = self::read(&bytes, 10, 20).unwrap();
        assert!(!read.output.contains("showing lines"));
    }

    #[test]
    fn the_byte_budget_stops_before_the_line_that_would_overrun_it() {
        let long = "x".repeat(20 * 1024);
        let text = format!("{long}\n{long}\n{long}\n{long}\n");
        let read = read(text.as_bytes(), 1, u64::MAX).unwrap();
        assert!(read.output.contains("showing lines 1–2 of 4"), "{}", &read.output[read.output.len() - 100..]);
        assert!(read.output.ends_with("start_line=3."));
        assert!(read.whole.is_none());
        // The continuation starts at the line that did not fit.
        let next = self::read(text.as_bytes(), 3, u64::MAX).unwrap();
        assert!(!next.output.contains("showing lines"), "two lines fit");
    }

    #[test]
    fn a_line_longer_than_any_read_is_cut_with_a_marker_and_the_next_lines_still_read() {
        let giant = "é".repeat(200_000);
        let text = format!("first\n{giant}\nlast\n");
        // Not first in its read: left for the read that continues there.
        let head = read(text.as_bytes(), 1, u64::MAX).unwrap();
        assert!(head.output.starts_with("first\n"));
        assert!(head.output.contains("showing lines 1–1 of 3"), "{}", head.output.len());
        assert!(head.output.ends_with("start_line=2."));
        // First in its read: cut inside the budget, with the omitted count.
        let cut = self::read(text.as_bytes(), 2, 2).unwrap();
        assert!(cut.output.len() < READ_MAX_BYTES, "{}", cut.output.len());
        assert!(cut.output.contains("line cut:"), "{}", &cut.output[cut.output.len() - 80..]);
        let shown = cut.output.split(" … [line cut").next().unwrap();
        assert!(shown.chars().all(|c| c == 'é'), "cut on a character boundary");
        let omitted = 200_000 - shown.chars().count();
        assert!(cut.output.contains(&format!("{omitted} more characters")), "{}", &cut.output[cut.output.len() - 80..]);
        assert!(cut.whole.is_none());
        // And the line after it reads as usual.
        assert_eq!(self::read(text.as_bytes(), 3, u64::MAX).unwrap().output, "last");
    }

    #[test]
    fn a_giant_line_costs_memory_for_its_cap_and_not_its_length() {
        // 40 MiB of one line, produced on demand.
        struct Endless(u64);
        impl Read for Endless {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = buffer.len().min(self.0 as usize);
                buffer[..count].fill(b'a');
                self.0 -= count as u64;
                Ok(count)
            }
        }
        let read = read_lines(Endless(40 * 1024 * 1024), None, "f", 1, u64::MAX, &english()).unwrap();
        assert!(read.output.len() < READ_MAX_BYTES);
        assert!(read.output.contains("more characters not shown"));
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily_and_says_edit_cannot_touch_it() {
        let read = read(b"caf\xe9 au lait\nplain\n", 1, u64::MAX).unwrap();
        assert!(read.output.starts_with("caf\u{fffd} au lait\nplain"), "{}", read.output);
        assert!(read.output.ends_with("`edit` cannot modify it.)"), "{}", read.output);
        assert!(read.whole.is_none(), "a lossy read cannot vouch for the bytes");
        // Valid UTF-8 says nothing.
        assert!(!self::read("café\n".as_bytes(), 1, u64::MAX).unwrap().output.contains("lossily"));
    }

    #[test]
    fn utf16_with_a_byte_order_mark_decodes_in_both_orders() {
        let text = "héllo 😀\nsecond line\n";
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        for unit in text.encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
            be.extend_from_slice(&unit.to_be_bytes());
        }
        for (bytes, label) in [(le, "UTF-16LE"), (be, "UTF-16BE")] {
            let read = read(&bytes, 1, u64::MAX).unwrap();
            assert!(read.output.starts_with("héllo 😀\nsecond line"), "{}", read.output);
            assert!(read.output.contains(&format!("This file is {label} text")), "{}", read.output);
            assert!(read.output.ends_with("`edit` cannot modify it.)"));
            assert!(read.whole.is_none(), "edit cannot rewrite it, so no full record");
            let ranged = self::read(&bytes, 2, 2).unwrap();
            assert!(ranged.output.starts_with("second line"), "{}", ranged.output);
        }
        // A lone surrogate and a stray last byte are replaced, not fatal.
        let broken = [0xFF, 0xFE, 0x00, 0xD8, b'a', 0x00, 0x41];
        let read = read(&broken, 1, u64::MAX).unwrap();
        assert!(read.output.starts_with("\u{fffd}a\u{fffd}"), "{:?}", read.output);
    }

    #[test]
    fn a_nul_in_the_first_kilobytes_makes_it_binary() {
        let mut bytes = b"MZ\x90\x00\x03\x00".to_vec();
        bytes.extend_from_slice(&[b'a'; 100]);
        let refused = read(&bytes, 1, u64::MAX).err().expect("refused");
        assert!(refused.starts_with("This tool cannot read binary files: f.txt"), "{refused}");
        // Past the sniffed prefix it is text as far as the tool can tell.
        let mut late = vec![b'a'; SNIFF_BYTES + 10];
        late.push(0);
        assert!(read(&late, 1, u64::MAX).is_ok());
        // UTF-16 text is all NULs and is not binary.
        assert!(read(&[0xFF, 0xFE, b'a', 0], 1, u64::MAX).is_ok());
    }

    #[test]
    fn a_part_cut_out_by_a_machine_is_not_sniffed_as_the_head_of_a_file() {
        // A NUL, or bytes that look like a UTF-16 mark, in the middle of a file
        // say nothing about the file: the machine looked at its head already.
        let mut part = b"\xff\xfe stray bytes\nsecond\0 line\nthird\n".to_vec();
        part.extend_from_slice(b"fourth\n");
        let read = read_window(
            &part[..],
            "f.txt",
            6000,
            6100,
            Window {
                base_line: 6000,
                total: Some(9000),
                more_after: true,
            },
            &english(),
        )
        .unwrap();
        assert!(read.output.starts_with("\u{fffd}\u{fffd} stray bytes\nsecond\0 line\nthird\nfourth"), "{:?}", read.output);
        // The hint is in the file's own line numbers and uses the machine's count.
        assert!(
            read.output.ends_with("… showing lines 6000–6003 of 9000. Continue with start_line=6004."),
            "{:?}",
            read.output
        );
        assert!(read.whole.is_none());
        // Past the end, the machine's total is the answer.
        let past = read_window(
            &b""[..],
            "f.txt",
            9500,
            9600,
            Window {
                base_line: 9500,
                total: Some(9000),
                more_after: false,
            },
            &english(),
        )
        .unwrap();
        assert!(past.output.contains("only 9000 lines"), "{}", past.output);
    }

    #[test]
    fn a_big_file_is_read_in_a_range_without_counting_when_it_is_too_large_to_count() {
        // The size only changes the note: a large file says its total was not
        // counted, a small one counts.
        let bytes = numbered(READ_DEFAULT_LINES + 5);
        let uncounted = read_lines(
            &bytes[..],
            Some(COUNT_SCAN_LIMIT + 1),
            "f",
            1,
            u64::MAX,
            &english(),
        )
        .unwrap();
        assert!(
            uncounted.output.ends_with(&format!(
                "… showing lines 1–{READ_DEFAULT_LINES}; the total line count was not computed because the file is large. Continue with start_line={}.",
                READ_DEFAULT_LINES + 1
            )),
            "{}",
            &uncounted.output[uncounted.output.len() - 160..]
        );
    }

    #[test]
    fn a_start_deep_into_the_file_skips_without_keeping_what_it_skips() {
        let bytes = numbered(100_000);
        let read = read(&bytes, 99_990, u64::MAX).unwrap();
        assert!(read.output.starts_with("line 99990\n"));
        assert!(read.output.ends_with("line 100000"));
        assert!(!read.output.contains("showing lines"));
    }

    #[test]
    fn crlf_and_a_lone_carriage_return_split_like_str_lines() {
        let text = "a\r\nb\rc\r\n\r\nd\r";
        let expected = text.lines().collect::<Vec<_>>().join("\n");
        let read = read(text.as_bytes(), 1, u64::MAX).unwrap();
        assert_eq!(read.output, expected);
        assert_eq!(read.whole.as_deref(), Some("a\nb\rc\n\nd\r"));
    }

    #[test]
    fn the_total_counts_a_last_line_without_a_newline_and_the_line_that_did_not_fit() {
        let long = "y".repeat(40 * 1024);
        let text = format!("{long}\n{long}\ntail");
        let read = read(text.as_bytes(), 1, u64::MAX).unwrap();
        assert!(read.output.contains("showing lines 1–1 of 3"), "{}", &read.output[read.output.len() - 90..]);
    }
}
