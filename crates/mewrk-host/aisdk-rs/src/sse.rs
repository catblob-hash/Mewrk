//! SSE on both sides of the SDK: the dialect layers' line rewriter (`sse.ts`), and the event parser and JSON
//! decoder the SDK's `createEventSourceResponseHandler` runs on what comes out of it.

use bytes::Bytes;
use memchr::memchr;

use crate::error::AiError;
use crate::http::{Body, BoxFuture, ChunkMap, DynFetch, Fetch, HttpRequest, HttpResponse};
use crate::json::Json;

/// What a rewriter does to one line. `Keep` is the no-op path: the original bytes go through.
pub enum Rewrite {
    Keep,
    Drop,
    Replace(String),
    /// The line cannot be accepted: the stream errors, as a `throw` inside a `TransformStream` does.
    Fail(String),
}

pub type LineRewrite = Box<dyn FnMut(&str) -> Rewrite + Send>;
pub type MakeRewrite = Box<dyn Fn() -> LineRewrite + Send + Sync>;
pub type PatchRequest = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Wraps a `fetch` to apply a per-response SSE line rewriter (`sseDialectFetch`).
///
/// Only bodies change; headers and status do not. Non-SSE responses pass through. CRLF delimiters survive.
pub struct SseDialectFetch {
    pub make_rewrite: MakeRewrite,
    pub patch_request: Option<PatchRequest>,
    pub inner: DynFetch,
}

impl SseDialectFetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        if let (Some(patch), Some(body)) = (&self.patch_request, &request.body) {
            if let Some(patched) = patch(body) {
                request.body = Some(patched);
            }
        }
        let mut response = self.inner.fetch(request).await?;
        let content_type = response.headers.get("content-type").unwrap_or("");
        if !content_type.contains("text/event-stream") {
            return Ok(response);
        }
        let inner = std::mem::replace(&mut response.body, Body::empty());
        response.body = Body::mapped(Box::new(RewriteBody::new(inner, (self.make_rewrite)())));
        Ok(response)
    }
}

impl Fetch for SseDialectFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

pub struct RewriteBody {
    inner: Body,
    rewrite: LineRewrite,
    pending: Vec<u8>,
    finished: bool,
    failed: Option<AiError>,
}

impl RewriteBody {
    pub fn new(inner: Body, rewrite: LineRewrite) -> Self {
        Self { inner, rewrite, pending: Vec::new(), finished: false, failed: None }
    }

    async fn pull(&mut self) -> Result<Option<Bytes>, AiError> {
        loop {
            if let Some(error) = self.failed.take() {
                self.finished = true;
                return Err(error);
            }
            if self.finished {
                return Ok(None);
            }
            let chunk = self.inner.next_chunk().await?;
            let Some(chunk) = chunk else {
                self.finished = true;
                if self.pending.is_empty() {
                    return Ok(None);
                }
                let tail = std::mem::take(&mut self.pending);
                let line = String::from_utf8_lossy(&tail);
                return Ok(match (self.rewrite)(&line) {
                    Rewrite::Keep => Some(Bytes::from(tail)),
                    Rewrite::Drop => None,
                    Rewrite::Replace(text) => Some(Bytes::from(text)),
                    Rewrite::Fail(message) => return Err(AiError::Other(message)),
                });
            };
            let mut out: Vec<u8> = Vec::with_capacity(chunk.len());
            let mut rest: &[u8] = &chunk;
            while let Some(index) = memchr(b'\n', rest) {
                let (head, tail) = rest.split_at(index);
                let joined;
                let raw: &[u8] = if self.pending.is_empty() {
                    head
                } else {
                    self.pending.extend_from_slice(head);
                    joined = std::mem::take(&mut self.pending);
                    &joined
                };
                rest = &tail[1..];
                let carriage = raw.last() == Some(&b'\r');
                let body = if carriage { &raw[..raw.len() - 1] } else { raw };
                let line = String::from_utf8_lossy(body);
                match (self.rewrite)(&line) {
                    Rewrite::Keep => {
                        out.extend_from_slice(raw);
                        out.push(b'\n');
                    }
                    Rewrite::Drop => {}
                    Rewrite::Replace(text) => {
                        out.extend_from_slice(text.as_bytes());
                        if carriage {
                            out.push(b'\r');
                        }
                        out.push(b'\n');
                    }
                    Rewrite::Fail(message) => {
                        self.failed = Some(AiError::Other(message));
                        rest = &[];
                        break;
                    }
                }
            }
            self.pending.extend_from_slice(rest);
            if !out.is_empty() {
                return Ok(Some(Bytes::from(out)));
            }
        }
    }
}

impl ChunkMap for RewriteBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, AiError>> {
        Box::pin(self.pull())
    }
}

/// One server-sent event's fields that the SDK reads. `event` and `id` are parsed and ignored there.
#[derive(Default)]
pub struct EventSourceParser {
    /// Bytes after the last newline.
    line: Vec<u8>,
    data: String,
    has_data: bool,
    /// The previous chunk ended in `\r`, so a leading `\n` belongs to it.
    skip_lf: bool,
    bom_checked: bool,
}

impl EventSourceParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one chunk; `on` receives each complete event's `data`.
    pub fn feed(&mut self, chunk: &[u8], on: &mut dyn FnMut(String)) {
        let mut rest = chunk;
        if !self.bom_checked && !rest.is_empty() {
            self.bom_checked = true;
            rest = rest.strip_prefix(b"\xef\xbb\xbf").unwrap_or(rest);
        }
        if self.skip_lf && !rest.is_empty() {
            self.skip_lf = false;
            if rest[0] == b'\n' {
                rest = &rest[1..];
            }
        }
        while let Some(at) = rest.iter().position(|b| *b == b'\n' || *b == b'\r') {
            let terminator = rest[at];
            self.line.extend_from_slice(&rest[..at]);
            let line = std::mem::take(&mut self.line);
            self.line_done(&line, on);
            rest = &rest[at + 1..];
            if terminator == b'\r' {
                match rest.first() {
                    Some(b'\n') => rest = &rest[1..],
                    None => self.skip_lf = true,
                    _ => {}
                }
            }
        }
        self.line.extend_from_slice(rest);
    }

    fn line_done(&mut self, line: &[u8], on: &mut dyn FnMut(String)) {
        if line.is_empty() {
            if self.has_data {
                let data = std::mem::take(&mut self.data);
                self.has_data = false;
                on(data);
            }
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (field, value) = match memchr(b':', line) {
            Some(at) => {
                let value = &line[at + 1..];
                (&line[..at], value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line, &line[line.len()..]),
        };
        if field == b"data" {
            if self.has_data {
                self.data.push('\n');
            }
            self.data.push_str(&String::from_utf8_lossy(value));
            self.has_data = true;
        }
    }
}

/// A byte stream decoded into JSON chunks. SSE here, AWS eventstream on Bedrock.
pub trait FrameDecoder: Send {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<Result<Json, AiError>>);
    /// The stream ended: anything still buffered.
    fn finish(&mut self, _out: &mut Vec<Result<Json, AiError>>) {}
}

/// `parseJsonEventStream`: `[DONE]` is dropped, everything else is parsed. A frame that does not parse is a
/// failed chunk rather than the end of the stream, which is what the SDK hands its transformers too.
#[derive(Default)]
pub struct SseJsonDecoder {
    parser: EventSourceParser,
}

impl FrameDecoder for SseJsonDecoder {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<Result<Json, AiError>>) {
        self.parser.feed(chunk, &mut |data| {
            if data == "[DONE]" {
                return;
            }
            out.push(crate::stream_text::secure_json_parse(&data).map_err(|detail| {
                AiError::Other(format!("JSON parsing failed: Text: {data}.\nError message: SyntaxError: {detail}"))
            }));
        });
    }

    fn finish(&mut self, out: &mut Vec<Result<Json, AiError>>) {
        // `eventsource-parser` only dispatches on a blank line: an unterminated trailing event is dropped.
        let _ = out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = EventSourceParser::new();
        let mut out = Vec::new();
        for chunk in chunks {
            parser.feed(chunk, &mut |data| out.push(data));
        }
        out
    }

    #[test]
    fn frames_survive_every_chunking() {
        let stream = b"event: x\r\ndata: {\"a\":1}\r\n\r\n: comment\n\ndata:{\"b\":2}\ndata: 3\n\nid: 1\n\n";
        let whole = events(&[stream]);
        assert_eq!(whole, ["{\"a\":1}", "{\"b\":2}\n3"]);
        for split in 0..stream.len() {
            assert_eq!(events(&[&stream[..split], &stream[split..]]), whole, "split at {split}");
        }
    }

    #[test]
    fn an_unterminated_last_event_is_not_dispatched() {
        assert!(events(&[b"data: {\"a\":1}\n"]).is_empty());
    }

    #[tokio::test]
    async fn rewriter_keeps_crlf_and_chunk_boundaries() {
        struct Chunks(Vec<&'static [u8]>);
        impl ChunkMap for Chunks {
            fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, AiError>> {
                let next = if self.0.is_empty() { None } else { Some(Bytes::from_static(self.0.remove(0))) };
                Box::pin(async move { Ok(next) })
            }
        }
        let body = Body::mapped(Box::new(Chunks(vec![b"a\r\nb", b"b\ndrop\nreplace\r\ntail"])));
        let rewrite: LineRewrite = Box::new(|line| match line {
            "drop" => Rewrite::Drop,
            "replace" => Rewrite::Replace("new".to_owned()),
            _ => Rewrite::Keep,
        });
        let mut wrapped = Body::mapped(Box::new(RewriteBody::new(body, rewrite)));
        let text = wrapped.read_text().await.unwrap();
        assert_eq!(text, "a\r\nbb\nnew\r\ntail");
    }
}
