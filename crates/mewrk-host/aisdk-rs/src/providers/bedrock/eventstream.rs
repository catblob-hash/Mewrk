//! AWS eventstream framing (`@smithy/eventstream-codec`) and `createAmazonBedrockEventStreamDecoder` with the
//! payload half of `createAmazonBedrockEventStreamResponseHandler`.
//!
//! A frame is `total length, header length, prelude CRC, headers, payload, message CRC`, big endian. Both CRC-32s
//! cover everything before them; the message CRC runs on from the prelude's.

use crate::error::AiError;
use crate::json::{Json, Obj};
use crate::sse::FrameDecoder;
use crate::stream_text::secure_json_parse;

const PRELUDE_LENGTH: usize = 8;
const CHECKSUM_LENGTH: usize = 4;
const MINIMUM_MESSAGE_LENGTH: usize = PRELUDE_LENGTH + CHECKSUM_LENGTH * 2;

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            bit += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// `Crc32`: CRC-32 (IEEE 802.3), resumable.
#[derive(Clone, Copy)]
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = CRC_TABLE[((self.0 ^ u32::from(*byte)) & 0xff) as usize] ^ (self.0 >> 8);
        }
    }

    fn digest(self) -> u32 {
        self.0 ^ 0xffff_ffff
    }
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// One header's value, tagged as `HeaderMarshaller` tags it.
#[derive(Clone, Debug, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Integer(i32),
    Long(i64),
    Binary(Vec<u8>),
    Str(String),
    /// Milliseconds since the epoch.
    Timestamp(i64),
    /// The sixteen bytes, as lower-case hex in the usual groups.
    Uuid(String),
}

/// One decoded message. Headers keep their wire order; a repeated name takes the last, as `out[name] = ...` does.
#[derive(Debug, PartialEq)]
pub struct Frame<'a> {
    pub headers: Vec<(String, HeaderValue)>,
    pub body: &'a [u8],
}

impl Frame<'_> {
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().rev().find(|(key, _)| key == name).map(|(_, value)| value)
    }

    fn header_str(&self, name: &str) -> Option<&str> {
        match self.header(name) {
            Some(HeaderValue::Str(text)) => Some(text),
            _ => None,
        }
    }
}

/// `splitMessage`, then `HeaderMarshaller.parse`: `bytes` is exactly one frame.
pub fn decode_frame(bytes: &[u8]) -> Result<Frame<'_>, String> {
    if bytes.len() < MINIMUM_MESSAGE_LENGTH {
        return Err("Provided message too short to accommodate event stream message overhead".to_owned());
    }
    if bytes.len() != be_u32(bytes, 0) as usize {
        return Err("Reported message length does not match received message length".to_owned());
    }
    let header_length = be_u32(bytes, 4) as usize;
    let expected_prelude = be_u32(bytes, PRELUDE_LENGTH);
    let expected_message = be_u32(bytes, bytes.len() - CHECKSUM_LENGTH);
    let mut checksum = Crc32::new();
    checksum.update(&bytes[..PRELUDE_LENGTH]);
    if expected_prelude != checksum.digest() {
        return Err(format!(
            "The prelude checksum specified in the message ({expected_prelude}) does not match the calculated CRC32 checksum ({})",
            checksum.digest()
        ));
    }
    checksum.update(&bytes[PRELUDE_LENGTH..bytes.len() - CHECKSUM_LENGTH]);
    if expected_message != checksum.digest() {
        return Err(format!("The message checksum ({}) did not match the expected value of {expected_message}", checksum.digest()));
    }
    let headers_start = PRELUDE_LENGTH + CHECKSUM_LENGTH;
    let body_end = bytes.len() - CHECKSUM_LENGTH;
    if header_length > body_end - headers_start {
        return Err(format!("Invalid DataView length {header_length}"));
    }
    let body_start = headers_start + header_length;
    Ok(Frame { headers: parse_headers(&bytes[headers_start..body_start])?, body: &bytes[body_start..body_end] })
}

/// What a `DataView` read past its end throws.
const OUT_OF_BOUNDS: &str = "Offset is outside the bounds of the DataView";

struct HeaderCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> HeaderCursor<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or(OUT_OF_BOUNDS)?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

fn parse_headers(bytes: &[u8]) -> Result<Vec<(String, HeaderValue)>, String> {
    let mut cursor = HeaderCursor { bytes, at: 0 };
    let mut out = Vec::new();
    while cursor.at < bytes.len() {
        let name_length = cursor.array::<1>()?[0] as usize;
        let name = String::from_utf8_lossy(cursor.take(name_length)?).into_owned();
        let value = match cursor.array::<1>()?[0] {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Byte(i8::from_be_bytes(cursor.array()?)),
            3 => HeaderValue::Short(i16::from_be_bytes(cursor.array()?)),
            4 => HeaderValue::Integer(i32::from_be_bytes(cursor.array()?)),
            5 => HeaderValue::Long(i64::from_be_bytes(cursor.array()?)),
            6 => {
                let length = u16::from_be_bytes(cursor.array()?) as usize;
                HeaderValue::Binary(cursor.take(length)?.to_vec())
            }
            7 => {
                let length = u16::from_be_bytes(cursor.array()?) as usize;
                HeaderValue::Str(String::from_utf8_lossy(cursor.take(length)?).into_owned())
            }
            8 => HeaderValue::Timestamp(i64::from_be_bytes(cursor.array()?)),
            9 => {
                let id = super::sigv4::hex(cursor.take(16)?);
                HeaderValue::Uuid(format!("{}-{}-{}-{}-{}", &id[..8], &id[8..12], &id[12..16], &id[16..20], &id[20..]))
            }
            _ => return Err("Unrecognized header type tag".to_owned()),
        };
        out.push((name, value));
    }
    Ok(out)
}

/// `EventStreamCodec.encode`: the inverse of [`decode_frame`]. Nothing in the provider sends frames; a test's mock
/// upstream builds them with this, and the unit tests pin it to the codec's own bytes.
pub fn encode_frame(headers: &[(&str, HeaderValue)], body: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        encoded.push(name.len() as u8);
        encoded.extend_from_slice(name.as_bytes());
        match value {
            HeaderValue::Bool(value) => encoded.push(u8::from(!*value)),
            HeaderValue::Byte(value) => {
                encoded.push(2);
                encoded.extend_from_slice(&value.to_be_bytes());
            }
            HeaderValue::Short(value) => {
                encoded.push(3);
                encoded.extend_from_slice(&value.to_be_bytes());
            }
            HeaderValue::Integer(value) => {
                encoded.push(4);
                encoded.extend_from_slice(&value.to_be_bytes());
            }
            HeaderValue::Long(value) => {
                encoded.push(5);
                encoded.extend_from_slice(&value.to_be_bytes());
            }
            HeaderValue::Binary(value) => {
                encoded.push(6);
                encoded.extend_from_slice(&(value.len() as u16).to_be_bytes());
                encoded.extend_from_slice(value);
            }
            HeaderValue::Str(value) => {
                encoded.push(7);
                encoded.extend_from_slice(&(value.len() as u16).to_be_bytes());
                encoded.extend_from_slice(value.as_bytes());
            }
            HeaderValue::Timestamp(value) => {
                encoded.push(8);
                encoded.extend_from_slice(&value.to_be_bytes());
            }
            HeaderValue::Uuid(value) => {
                encoded.push(9);
                let digits: Vec<u8> = value.bytes().filter(|byte| *byte != b'-').collect();
                for pair in digits.chunks(2) {
                    let text = std::str::from_utf8(pair).unwrap_or("00");
                    encoded.push(u8::from_str_radix(text, 16).unwrap_or(0));
                }
            }
        }
    }
    let length = encoded.len() + body.len() + MINIMUM_MESSAGE_LENGTH;
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&(length as u32).to_be_bytes());
    out.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    let mut checksum = Crc32::new();
    checksum.update(&out);
    out.extend_from_slice(&checksum.digest().to_be_bytes());
    out.extend_from_slice(&encoded);
    out.extend_from_slice(body);
    checksum.update(&out[PRELUDE_LENGTH..]);
    out.extend_from_slice(&checksum.digest().to_be_bytes());
    out
}

/// Decodes the response body into `{ [payloadType]: payload }` chunks; a chunk's validation against the stream
/// schema is the model's, which has the raw value to hand either way.
///
/// In upstream a frame that fails to decode throws inside a `TransformStream`, which errors the stream: the failure
/// is the last thing that comes out, and later bytes are ignored. A payload that is not JSON is a failed chunk, and
/// the frames after it still arrive.
#[derive(Default)]
pub struct EventStreamDecoder {
    buffer: Vec<u8>,
    failed: bool,
}

impl EventStreamDecoder {
    fn fail(&mut self, message: String, out: &mut Vec<Result<Json, AiError>>) {
        self.failed = true;
        self.buffer.clear();
        out.push(Err(AiError::Other(message)));
    }

    /// `processEvent` in the response handler. `Err` is a throw.
    fn on_frame(frame: &Frame<'_>, out: &mut Vec<Result<Json, AiError>>) -> Result<(), String> {
        let payload_type = match frame.header_str(":message-type") {
            Some("event") => frame.header_str(":event-type"),
            Some("exception") => frame.header_str(":exception-type"),
            _ => None,
        };
        let Some(payload_type) = payload_type else { return Ok(()) };
        // `new TextDecoder()`: invalid sequences become U+FFFD and a byte order mark goes.
        let lossy = String::from_utf8_lossy(frame.body);
        let text = lossy.strip_prefix('\u{feff}').unwrap_or(&lossy);
        let mut value = match secure_json_parse(text) {
            Ok(value) => value,
            Err(detail) => {
                out.push(Err(AiError::Other(format!("JSON parsing failed: Text: {text}.\nError message: SyntaxError: {detail}"))));
                return Ok(());
            }
        };
        match &mut value {
            // `delete (null).p` is a TypeError, which `processEvent` lets out.
            Json::Null => return Err("Cannot convert undefined or null to object".to_owned()),
            // The padding Bedrock adds to hide message sizes.
            Json::Obj(object) => {
                object.remove("p");
            }
            _ => {}
        }
        let mut wrapped = Obj::new();
        wrapped.insert(payload_type, value);
        out.push(Ok(Json::Obj(wrapped)));
        Ok(())
    }
}

impl FrameDecoder for EventStreamDecoder {
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<Result<Json, AiError>>) {
        if self.failed {
            return;
        }
        self.buffer.extend_from_slice(chunk);
        let buffer = std::mem::take(&mut self.buffer);
        let mut at = 0;
        while buffer.len() - at >= 4 {
            let total = be_u32(&buffer, at) as usize;
            if buffer.len() - at < total {
                break;
            }
            let handled = decode_frame(&buffer[at..at + total]).and_then(|frame| Self::on_frame(&frame, out));
            if let Err(message) = handled {
                self.fail(message, out);
                return;
            }
            at += total;
        }
        self.buffer = buffer;
        self.buffer.drain(..at);
    }

    fn finish(&mut self, out: &mut Vec<Result<Json, AiError>>) {
        if !self.failed && !self.buffer.is_empty() {
            let message = format!("Incomplete Amazon Bedrock event-stream frame: {} buffered bytes remain at end of stream.", self.buffer.len());
            self.fail(message, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every vector below was written by `@smithy/eventstream-codec`'s `EventStreamCodec.encode` under node.
    const EVENT: &str = "0000009e00000057fe1b824f0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a302c2264656c7461223a7b2274657874223a226869227d2c2270223a22616263227df1e89c75";
    const EXCEPTION: &str = "0000008800000061de0175f40d3a6d6573736167652d74797065070009657863657074696f6e0f3a657863657074696f6e2d747970650700137468726f74746c696e67457863657074696f6e0d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b226d657373616765223a22736c6f7720646f776e227d4e43b958";
    const EMPTY: &str = "000000100000000005c248eb7d98c8ff";
    const UNICODE: &str = "00000083000000365cde80b20d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74617b22636f6e74656e74426c6f636b496e646578223a312c2264656c7461223a7b2274657874223a2268c3a96c6c6f20e4bda0e5a5bd20f09f9982227d7da3889c8d";
    const ALL_TYPES: &str = "000000a80000007fe5cf679301740001660102627902fb02736803fb2e02696e040001e240026c6f05fffffffdb34fe916026269060004010203ff0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d7479706507000b6d65737361676553746f70027473080000018bcfe5687b02757509b5a9e7e01a2b4c3d8e4f0011223344557b2273746f70526561736f6e223a22656e645f7475726e227d9faf3fe4";

    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap()).collect()
    }

    fn string(text: &str) -> HeaderValue {
        HeaderValue::Str(text.to_owned())
    }

    fn all_types_headers() -> Vec<(&'static str, HeaderValue)> {
        vec![
            ("t", HeaderValue::Bool(true)),
            ("f", HeaderValue::Bool(false)),
            ("by", HeaderValue::Byte(-5)),
            ("sh", HeaderValue::Short(-1234)),
            ("in", HeaderValue::Integer(123_456)),
            ("lo", HeaderValue::Long(-9_876_543_210)),
            ("bi", HeaderValue::Binary(vec![1, 2, 3, 255])),
            (":message-type", string("event")),
            (":event-type", string("messageStop")),
            ("ts", HeaderValue::Timestamp(1_700_000_000_123)),
            ("uu", HeaderValue::Uuid("b5a9e7e0-1a2b-4c3d-8e4f-001122334455".to_owned())),
        ]
    }

    #[test]
    fn the_codec_output_decodes_with_every_header_type() {
        let raw = bytes(ALL_TYPES);
        let frame = decode_frame(&raw).unwrap();
        let expected: Vec<(String, HeaderValue)> = all_types_headers().into_iter().map(|(name, value)| (name.to_owned(), value)).collect();
        assert_eq!(frame.headers, expected);
        assert_eq!(frame.body, br#"{"stopReason":"end_turn"}"#);
    }

    #[test]
    fn the_encoder_matches_the_codec_byte_for_byte() {
        assert_eq!(encode_frame(&all_types_headers(), br#"{"stopReason":"end_turn"}"#), bytes(ALL_TYPES));
        assert_eq!(encode_frame(&[], b""), bytes(EMPTY));
        let event = [(":message-type", string("event")), (":event-type", string("contentBlockDelta")), (":content-type", string("application/json"))];
        assert_eq!(encode_frame(&event, br#"{"contentBlockIndex":0,"delta":{"text":"hi"},"p":"abc"}"#), bytes(EVENT));
        let exception = [(":message-type", string("exception")), (":exception-type", string("throttlingException")), (":content-type", string("application/json"))];
        assert_eq!(encode_frame(&exception, br#"{"message":"slow down"}"#), bytes(EXCEPTION));
    }

    fn feed_all(chunks: &[&[u8]]) -> Vec<Result<String, String>> {
        let mut decoder = EventStreamDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.feed(chunk, &mut out);
        }
        decoder.finish(&mut out);
        out.into_iter().map(|item| item.map(|json| json.to_text()).map_err(|error| error.message())).collect()
    }

    fn stream() -> Vec<u8> {
        // An empty frame has no message type, so it decodes to nothing.
        [bytes(EVENT), bytes(EMPTY), bytes(EXCEPTION), bytes(UNICODE), bytes(ALL_TYPES)].concat()
    }

    fn expected_chunks() -> Vec<Result<String, String>> {
        vec![
            // The `p` padding is gone.
            Ok(r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hi"}}}"#.to_owned()),
            Ok(r#"{"throttlingException":{"message":"slow down"}}"#.to_owned()),
            Ok("{\"contentBlockDelta\":{\"contentBlockIndex\":1,\"delta\":{\"text\":\"h\u{e9}llo \u{4f60}\u{597d} \u{1f642}\"}}}".to_owned()),
            Ok(r#"{"messageStop":{"stopReason":"end_turn"}}"#.to_owned()),
        ]
    }

    #[test]
    fn frames_survive_a_split_at_every_point() {
        let stream = stream();
        assert_eq!(feed_all(&[&stream]), expected_chunks());
        for split in 0..=stream.len() {
            assert_eq!(feed_all(&[&stream[..split], &stream[split..]]), expected_chunks(), "split at {split}");
        }
    }

    #[test]
    fn frames_survive_one_byte_at_a_time() {
        let stream = stream();
        let chunks: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(feed_all(&chunks), expected_chunks());
    }

    /// The messages are the real codec's, thrown on these same bytes under node.
    #[test]
    fn corruption_reports_what_the_codec_reports() {
        let cases: [(&str, &str); 7] = [
            (
                "0000009e00000057011b824f0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a302c2264656c7461223a7b2274657874223a226869227d2c2270223a22616263227df1e89c75",
                "The prelude checksum specified in the message (18580047) does not match the calculated CRC32 checksum (4263215695)",
            ),
            (
                "0000009e00000057fe1b824f0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a302c2264656c7461223a7b2274657874223a226869227d2c2270223a22616263227df1e89c74",
                "The message checksum (4058553461) did not match the expected value of 4058553460",
            ),
            (
                "0000009e00000057fe1b824f0d3a6d6573736167652d747970650700056576656e740b3a6576656e742d74797065070011636f6e74656e74426c6f636b44656c74610d3a636f6e74656e742d747970650700106170706c69636174696f6e2f6a736f6e7b22636f6e74656e74426c6f636b496e646578223a302c2264656c7461223a7b2274657874223a226869227d2c2270223a23616263227df1e89c75",
                "The message checksum (984895440) did not match the expected value of 4058553461",
            ),
            ("0000000c0000000000000000", "Provided message too short to accommodate event stream message overhead"),
            ("0000001500000003542b962101612a7b7d62dc340c", "Unrecognized header type tag"),
            ("00000012000003e8fafe22027b7d312c217a", "Invalid DataView length 1000"),
            ("0000001400000004f72f2a3201610700ca8c1f30", "Offset is outside the bounds of the DataView"),
        ];
        for (hex, message) in cases {
            assert_eq!(decode_frame(&bytes(hex)).unwrap_err(), message);
            // Through the stream decoder, at every split, with a frame ahead of it and one behind it that never arrives.
            let stream = [bytes(EVENT), bytes(hex), bytes(EVENT)].concat();
            let expected = vec![Ok(r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"hi"}}}"#.to_owned()), Err(message.to_owned())];
            for split in 0..=stream.len() {
                assert_eq!(feed_all(&[&stream[..split], &stream[split..]]), expected, "{message}: split at {split}");
            }
        }
    }

    #[test]
    fn a_truncated_stream_is_an_error_at_the_end() {
        let event = bytes(EVENT);
        let cut = &event[..event.len() - 7];
        assert_eq!(
            feed_all(&[cut]),
            vec![Err(format!("Incomplete Amazon Bedrock event-stream frame: {} buffered bytes remain at end of stream.", cut.len()))]
        );
        // Under four bytes, even the length is not known yet.
        assert_eq!(feed_all(&[&event[..3]]), vec![Err("Incomplete Amazon Bedrock event-stream frame: 3 buffered bytes remain at end of stream.".to_owned())]);
    }

    fn event_with(body: &[u8], message_type: &str) -> Vec<u8> {
        encode_frame(&[(":message-type", string(message_type)), (":event-type", string("messageStop")), (":exception-type", string("throttlingException"))], body)
    }

    #[test]
    fn a_message_type_selects_its_payload_type_and_others_are_dropped() {
        assert_eq!(feed_all(&[&event_with(b"{}", "event")]), vec![Ok(r#"{"messageStop":{}}"#.to_owned())]);
        assert_eq!(feed_all(&[&event_with(b"{}", "exception")]), vec![Ok(r#"{"throttlingException":{}}"#.to_owned())]);
        // `:message-type: error` is the wire's third kind, and upstream's handler has no branch for it.
        assert_eq!(feed_all(&[&event_with(b"not json", "error")]), vec![]);
        // Missing event type on an event.
        assert_eq!(feed_all(&[&encode_frame(&[(":message-type", string("event"))], b"not json")]), vec![]);
    }

    #[test]
    fn payloads_read_as_text_decoder_and_secure_json_parse_read_them() {
        // A byte order mark is not part of the text.
        assert_eq!(feed_all(&[&event_with(b"\xef\xbb\xbf{\"a\":1}", "event")]), vec![Ok(r#"{"messageStop":{"a":1}}"#.to_owned())]);
        // A payload that is not JSON is a failed chunk, not the end of the stream.
        let bad = [event_with(b"not json", "event"), event_with(b"{\"a\":1}", "event")].concat();
        let out = feed_all(&[&bad]);
        assert!(out[0].as_ref().unwrap_err().starts_with("JSON parsing failed: Text: not json.\nError message: SyntaxError: "), "{out:?}");
        assert_eq!(out[1], Ok(r#"{"messageStop":{"a":1}}"#.to_owned()));
        let proto = feed_all(&[&event_with(br#"{"__proto__":{}}"#, "event")]);
        assert_eq!(proto[0], Err("JSON parsing failed: Text: {\"__proto__\":{}}.\nError message: SyntaxError: Object contains forbidden prototype property".to_owned()));
        // `delete (null).p` throws, which ends the stream; a number has nothing to delete.
        let null = [event_with(b"null", "event"), event_with(b"{}", "event")].concat();
        assert_eq!(feed_all(&[&null]), vec![Err("Cannot convert undefined or null to object".to_owned())]);
        assert_eq!(feed_all(&[&event_with(b"5", "event")]), vec![Ok(r#"{"messageStop":5}"#.to_owned())]);
    }

    #[test]
    fn crc32_check_value() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.digest(), 0xcbf4_3926);
    }
}
