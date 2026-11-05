//! Incremental Server-Sent Events framing.
//!
//! TLS records and TCP segments cut an event stream at arbitrary byte offsets, and on a lossy link they
//! do so constantly. The parser must produce the same events for every possible chunking, and in the
//! common case (a whole event inside one chunk) it hands out a slice of that chunk without copying.

use memchr::memchr;

/// Events larger than this are a protocol violation, not a big token.
const MAX_EVENT: usize = 16 << 20;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SseEvent<'a> {
    Data(&'a [u8]),
    /// llama-server reports mid-stream failures in an `error:` field rather than in `data:`.
    Error(&'a [u8]),
}

#[derive(Debug, PartialEq, Eq)]
pub struct EventTooLarge;

#[derive(Default)]
pub struct SseParser {
    /// Unterminated line carried over from the previous chunk.
    line: Vec<u8>,
    /// Field value of the event being assembled, when it could not stay borrowed from the chunk.
    data: Vec<u8>,
    has_data: bool,
    is_error: bool,
}

/// A field line, split into what the parser cares about.
fn field(line: &[u8]) -> Option<(bool, &[u8])> {
    let (is_error, rest) = if let Some(rest) = line.strip_prefix(b"data:") {
        (false, rest)
    } else {
        let rest = line.strip_prefix(b"error:")?;
        (true, rest)
    };
    Some((is_error, rest.strip_prefix(b" ").unwrap_or(rest)))
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk and calls `on` for every event completed by it.
    pub fn feed(&mut self, mut chunk: &[u8], on: &mut impl FnMut(SseEvent<'_>)) -> Result<(), EventTooLarge> {
        // Value of the current event while it still lives entirely inside `chunk`.
        let mut borrowed: Option<(bool, &[u8])> = None;

        while let Some(nl) = memchr(b'\n', chunk) {
            let (raw, rest) = chunk.split_at(nl);
            chunk = &rest[1..];

            if self.line.is_empty() {
                let line = raw.strip_suffix(b"\r").unwrap_or(raw);
                if line.is_empty() {
                    if let Some((is_error, value)) = borrowed.take() {
                        on(if is_error { SseEvent::Error(value) } else { SseEvent::Data(value) });
                    } else {
                        self.dispatch_owned(on);
                    }
                } else if let Some((is_error, value)) = field(line) {
                    if self.has_data {
                        self.push_owned(is_error, value)?;
                    } else if let Some((prev_error, prev)) = borrowed.take() {
                        // Second line of a multi-line event: only now does it need a buffer.
                        self.push_owned(prev_error, prev)?;
                        self.push_owned(is_error, value)?;
                    } else {
                        borrowed = Some((is_error, value));
                    }
                }
            } else {
                self.line.extend_from_slice(raw);
                let mut line = std::mem::take(&mut self.line);
                let trimmed = line.strip_suffix(b"\r").unwrap_or(&line);
                if trimmed.is_empty() {
                    self.dispatch_owned(on);
                } else if let Some((is_error, value)) = field(trimmed) {
                    self.push_owned(is_error, value)?;
                }
                line.clear();
                self.line = line; // keep the allocation
            }
        }

        if let Some((is_error, value)) = borrowed {
            self.push_owned(is_error, value)?;
        }
        if self.line.len() + chunk.len() > MAX_EVENT {
            return Err(EventTooLarge);
        }
        self.line.extend_from_slice(chunk);
        Ok(())
    }

    fn push_owned(&mut self, is_error: bool, value: &[u8]) -> Result<(), EventTooLarge> {
        if self.data.len() + value.len() > MAX_EVENT {
            return Err(EventTooLarge);
        }
        if self.has_data {
            self.data.push(b'\n');
        }
        self.data.extend_from_slice(value);
        self.has_data = true;
        self.is_error |= is_error;
        Ok(())
    }

    fn dispatch_owned(&mut self, on: &mut impl FnMut(SseEvent<'_>)) {
        if self.has_data {
            on(if self.is_error { SseEvent::Error(&self.data) } else { SseEvent::Data(&self.data) });
            self.data.clear();
            self.has_data = false;
            self.is_error = false;
        }
    }

    /// Discards partial state, for reuse on a new connection.
    pub fn reset(&mut self) {
        self.line.clear();
        self.data.clear();
        self.has_data = false;
        self.is_error = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<(bool, Vec<u8>)> {
        let mut parser = SseParser::new();
        let mut out = Vec::new();
        for chunk in chunks {
            parser
                .feed(chunk, &mut |event| match event {
                    SseEvent::Data(d) => out.push((false, d.to_vec())),
                    SseEvent::Error(d) => out.push((true, d.to_vec())),
                })
                .unwrap();
        }
        out
    }

    const STREAM: &[u8] = b"data: {\"a\":1}\n\ndata:{\"b\":2}\r\n\r\n: comment\nevent: x\ndata: l1\ndata: l2\n\nerror: {\"e\":3}\n\ndata: [DONE]\n\n";

    fn expected() -> Vec<(bool, Vec<u8>)> {
        vec![
            (false, b"{\"a\":1}".to_vec()),
            (false, b"{\"b\":2}".to_vec()),
            (false, b"l1\nl2".to_vec()),
            (true, b"{\"e\":3}".to_vec()),
            (false, b"[DONE]".to_vec()),
        ]
    }

    #[test]
    fn whole_stream_in_one_chunk() {
        assert_eq!(collect(&[STREAM]), expected());
    }

    #[test]
    fn every_two_way_split_gives_the_same_events() {
        for cut in 0..=STREAM.len() {
            let (a, b) = STREAM.split_at(cut);
            assert_eq!(collect(&[a, b]), expected(), "cut at {cut}");
        }
    }

    #[test]
    fn every_three_way_split_gives_the_same_events() {
        for i in 0..=STREAM.len() {
            for j in i..=STREAM.len() {
                assert_eq!(collect(&[&STREAM[..i], &STREAM[i..j], &STREAM[j..]]), expected(), "cuts {i},{j}");
            }
        }
    }

    #[test]
    fn byte_at_a_time() {
        let chunks: Vec<&[u8]> = STREAM.chunks(1).collect();
        assert_eq!(collect(&chunks), expected());
    }

    #[test]
    fn single_chunk_events_are_not_copied() {
        let chunk = b"data: abc\n\n";
        let mut parser = SseParser::new();
        let mut same_memory = false;
        parser
            .feed(chunk, &mut |event| {
                if let SseEvent::Data(d) = event {
                    same_memory = d.as_ptr() == chunk[6..].as_ptr();
                }
            })
            .unwrap();
        assert!(same_memory);
    }

    #[test]
    fn oversized_event_is_rejected_instead_of_buffered_forever() {
        let mut parser = SseParser::new();
        let big = vec![b'x'; MAX_EVENT + 1];
        assert_eq!(parser.feed(&big, &mut |_| {}), Err(EventTooLarge));
    }
}
