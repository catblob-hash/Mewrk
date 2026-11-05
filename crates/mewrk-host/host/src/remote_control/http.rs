//! A small blocking HTTP/1.1 client over a [`Connector`].
//!
//! WebDriverAgent is plain HTTP on the loopback of the place its device is
//! behind, and "the loopback of the place" may be an SSH machine's, reached
//! through a relay of its Mewrk agent rather than a socket of this process. A
//! general HTTP client cannot be handed such a pipe, and a proxy setting would
//! let a request leave the one loopback a remote-control entry is allowed to
//! name, so this module speaks the little HTTP that WDA needs over a
//! [`Connection`] itself:
//!
//! * one request per connection (`Connection: close`), JSON bodies;
//! * a reply body by `Content-Length`, by chunks, or until the server closes;
//! * redirects are never followed — a 3xx is just a status — so a reply cannot
//!   send a request anywhere else;
//! * everything is bounded. The reply head and body have size limits, and the
//!   whole exchange, connecting included, has one deadline that holds against a
//!   server that answers a byte at a time as well as one that does not answer.

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    time::{Duration, Instant},
};

use serde_json::Value;

use super::transport::{Connection, Connector};

/// The longest reply head (status line and headers) accepted.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// The largest reply body accepted. A full-resolution WDA screenshot is a few
/// megabytes of base64 inside JSON; this leaves room for a very large tablet
/// and nothing for a server that streams forever.
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// The largest request body sent. Callers send small JSON commands.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// How many `1xx` interim replies are skipped before the final one.
const MAX_INTERIM_REPLIES: usize = 8;
/// The longest chunk-size line (a size and perhaps extensions).
const MAX_CHUNK_LINE: usize = 1024;

#[derive(Clone, Copy)]
struct Limits {
    head: usize,
    body: usize,
}

const DEFAULT_LIMITS: Limits = Limits {
    head: MAX_HEAD_BYTES,
    body: MAX_BODY_BYTES,
};

/// What the server answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as JSON. The error never repeats the body, which a broken or
    /// hostile server chose; [`Response::snippet`] is the way to show some of it.
    pub fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.body).map_err(|error| {
            format!(
                "the reply (HTTP {}) is not valid JSON: {error}",
                self.status
            )
        })
    }

    /// The start of the body as one line of text, for a message about a reply
    /// that was not what was expected.
    pub fn snippet(&self) -> String {
        let head = &self.body[..self.body.len().min(200)];
        crate::preview_launch_config::sanitize_for_message(&String::from_utf8_lossy(head))
    }
}

/// Sends one request to `port` on the connector's place and reads the whole
/// reply. `timeout` bounds the exchange from the first connection attempt to the
/// last byte of the body.
///
/// `path` must be an origin-form path such as `/status` or `/source?format=json`
/// (it starts with `/` and holds only printable ASCII); the caller percent-encodes
/// whatever it appends. `body` is sent as JSON; a request without one carries
/// `Content-Length: 0`.
pub fn request(
    connector: &dyn Connector,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Response, String> {
    exchange(connector, port, method, path, body, timeout, DEFAULT_LIMITS)
}

fn exchange(
    connector: &dyn Connector,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
    limits: Limits,
) -> Result<Response, String> {
    let wire = encode_request(port, method, path, body)?;
    let deadline = Instant::now() + timeout;
    let target = format!("127.0.0.1:{port} on {}", connector.place_label());
    let mut connection = connector.connect(port, timeout)?;
    let handle = connection.handle().clone();
    handle.set_deadline(Some(deadline.saturating_duration_since(Instant::now())));
    let result = run(
        &mut connection,
        &wire,
        &target,
        deadline,
        limits,
        method == "HEAD",
    );
    // The reply is complete or abandoned either way, and `Connection: close` means the
    // server is done with this socket; lifting the deadline first stops a relay's timer.
    handle.set_deadline(None);
    handle.close();
    result
}

fn encode_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Vec<u8>, String> {
    if method.is_empty() || !method.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err(format!(
            "\"{}\" is not an HTTP method",
            crate::preview_launch_config::sanitize_for_message(method)
        ));
    }
    validate_path(path)?;
    let payload = match body {
        Some(value) => serde_json::to_vec(value)
            .map_err(|error| format!("the request body is not valid JSON: {error}"))?,
        None => Vec::new(),
    };
    if payload.len() > MAX_REQUEST_BYTES {
        return Err(format!(
            "the request body is larger than {MAX_REQUEST_BYTES} bytes"
        ));
    }
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUser-Agent: Mewrk\r\nAccept: application/json\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        payload.len()
    )
    .into_bytes();
    wire.extend_from_slice(&payload);
    Ok(wire)
}

/// A request line is built by joining strings, so the path must not be able to
/// end it early: no space, no control character, no `#`, nothing outside
/// printable ASCII.
fn validate_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err("an HTTP path must start with '/'".to_owned());
    }
    if path.len() > 2048 {
        return Err("an HTTP path may be at most 2048 bytes".to_owned());
    }
    if !path
        .bytes()
        .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'#')
    {
        return Err(
            "an HTTP path may hold only printable ASCII without spaces or '#'; percent-encode the rest"
                .to_owned(),
        );
    }
    Ok(())
}

/// Reads fail with `TimedOut` once the deadline has passed, even when the
/// server keeps the socket busy with a trickle of bytes — the connection's own
/// deadline only fires when a read finds nothing to read.
struct DeadlineRead<'a> {
    inner: &'a mut Connection,
    deadline: Instant,
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the reply did not finish in time",
            ));
        }
        self.inner.read(buffer)
    }
}

fn run(
    connection: &mut Connection,
    wire: &[u8],
    target: &str,
    deadline: Instant,
    limits: Limits,
    head_only: bool,
) -> Result<Response, String> {
    connection
        .write_all(wire)
        .and_then(|()| connection.flush())
        .map_err(|error| io_failure(target, "send the request to", &error))?;
    let mut reader = BufReader::with_capacity(
        32 * 1024,
        DeadlineRead {
            inner: connection,
            deadline,
        },
    );
    read_response(&mut reader, target, limits, head_only)
}

fn io_failure(target: &str, doing: &str, error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::TimedOut => format!("{target} did not answer in time"),
        io::ErrorKind::ConnectionAborted => format!("the connection to {target} was closed"),
        _ => format!("could not {doing} {target}: {error}"),
    }
}

/// What the reply head said that decides how its body is read.
struct Head {
    status: u16,
    content_length: Option<u64>,
    chunked: bool,
}

fn read_response<R: BufRead>(
    reader: &mut R,
    target: &str,
    limits: Limits,
    head_only: bool,
) -> Result<Response, String> {
    let mut interim = 0;
    let head = loop {
        let head = read_head(reader, target, limits.head)?;
        // A `100 Continue` is never asked for here, but a server may send one anyway; the
        // answer is the next head. `101` switches protocols, which this client never does.
        if (100..200).contains(&head.status) && head.status != 101 {
            interim += 1;
            if interim > MAX_INTERIM_REPLIES {
                return Err(format!("{target} sent too many interim replies"));
            }
            continue;
        }
        break head;
    };
    let bodiless = head_only || head.status == 204 || head.status == 304 || head.status == 101;
    let body = if bodiless {
        Vec::new()
    } else if head.chunked {
        read_chunked(reader, target, limits)?
    } else if let Some(length) = head.content_length {
        read_counted(reader, target, length, limits)?
    } else {
        read_until_close(reader, target, limits)?
    };
    Ok(Response {
        status: head.status,
        body,
    })
}

/// One line without its terminator. `None` when the server closed before sending
/// a byte of it. A line past `budget`, or one the server cut short, is an error.
fn read_line<R: BufRead>(
    reader: &mut R,
    budget: &mut usize,
    target: &str,
) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    let allowed = u64::try_from(*budget).unwrap_or(u64::MAX).saturating_add(1);
    (&mut *reader)
        .take(allowed)
        .read_until(b'\n', &mut line)
        .map_err(|error| io_failure(target, "read the reply from", &error))?;
    if line.is_empty() {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') {
        return Err(if line.len() > *budget {
            format!("{target} sent a line longer than the limit")
        } else {
            format!("{target} closed the connection in the middle of its reply")
        });
    }
    if line.len() > *budget {
        return Err(format!("{target} sent a line longer than the limit"));
    }
    *budget -= line.len();
    line.pop();
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(line))
}

fn read_head<R: BufRead>(reader: &mut R, target: &str, limit: usize) -> Result<Head, String> {
    let mut budget = limit;
    let status_line = read_line(reader, &mut budget, target)?
        .ok_or_else(|| format!("{target} closed the connection without answering"))?;
    let status_line = String::from_utf8_lossy(&status_line);
    let mut parts = status_line.split_whitespace();
    let version = parts.next().unwrap_or_default();
    let status = parts
        .next()
        .filter(|code| code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|code| code.parse::<u16>().ok());
    let status = match status {
        Some(status) if version.starts_with("HTTP/1.") => status,
        _ => {
            return Err(format!(
                "{target} did not answer with HTTP: {}",
                crate::preview_launch_config::sanitize_for_message(&status_line)
            ))
        }
    };

    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    loop {
        let line = read_line(reader, &mut budget, target)?
            .ok_or_else(|| format!("{target} closed the connection in the middle of its reply"))?;
        if line.is_empty() {
            break;
        }
        let line = String::from_utf8_lossy(&line);
        // An obsolete folded continuation line carries nothing this client reads.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("{target} sent an invalid Content-Length"));
            }
            let length = value
                .parse::<u64>()
                .map_err(|_| format!("{target} sent an invalid Content-Length"))?;
            if content_length.is_some_and(|earlier| earlier != length) {
                return Err(format!("{target} sent conflicting Content-Length headers"));
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked wins only as the last coding, as the RFC has it.
            chunked = value
                .rsplit(',')
                .next()
                .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"));
        }
    }
    Ok(Head {
        status,
        content_length,
        chunked,
    })
}

fn too_large(target: &str, limit: usize) -> String {
    format!("{target} sent a reply larger than {limit} bytes")
}

fn read_counted<R: BufRead>(
    reader: &mut R,
    target: &str,
    length: u64,
    limits: Limits,
) -> Result<Vec<u8>, String> {
    if length > limits.body as u64 {
        return Err(too_large(target, limits.body));
    }
    // Grows with what arrives rather than with what the server claims.
    let mut body = Vec::new();
    (&mut *reader)
        .take(length)
        .read_to_end(&mut body)
        .map_err(|error| io_failure(target, "read the reply from", &error))?;
    if body.len() as u64 != length {
        return Err(format!(
            "{target} closed the connection before the whole reply arrived ({} of {length} bytes)",
            body.len()
        ));
    }
    Ok(body)
}

fn read_until_close<R: BufRead>(
    reader: &mut R,
    target: &str,
    limits: Limits,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let result = (&mut *reader)
        .take(limits.body as u64 + 1)
        .read_to_end(&mut body);
    match result {
        Ok(_) => {}
        // A server that resets the socket instead of closing it has still said all it
        // will; with no length to check against, what arrived is the body.
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Err(error) => return Err(io_failure(target, "read the reply from", &error)),
    }
    if body.len() > limits.body {
        return Err(too_large(target, limits.body));
    }
    Ok(body)
}

fn read_chunked<R: BufRead>(
    reader: &mut R,
    target: &str,
    limits: Limits,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let mut budget = MAX_CHUNK_LINE;
        let line = read_line(reader, &mut budget, target)?
            .ok_or_else(|| format!("{target} closed the connection in the middle of its reply"))?;
        let line = String::from_utf8_lossy(&line);
        let digits = line.split(';').next().unwrap_or_default().trim();
        if digits.is_empty()
            || digits.len() > 16
            || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(format!("{target} sent a malformed chunk size"));
        }
        let size = u64::from_str_radix(digits, 16)
            .map_err(|_| format!("{target} sent a malformed chunk size"))?;
        if size == 0 {
            // Trailers, if any, end at an empty line.
            let mut budget = limits.head;
            loop {
                let trailer = read_line(reader, &mut budget, target)?.ok_or_else(|| {
                    format!("{target} closed the connection in the middle of its reply")
                })?;
                if trailer.is_empty() {
                    return Ok(body);
                }
            }
        }
        if (body.len() as u64).saturating_add(size) > limits.body as u64 {
            return Err(too_large(target, limits.body));
        }
        let before = body.len();
        (&mut *reader)
            .take(size)
            .read_to_end(&mut body)
            .map_err(|error| io_failure(target, "read the reply from", &error))?;
        if (body.len() - before) as u64 != size {
            return Err(format!(
                "{target} closed the connection before the whole reply arrived"
            ));
        }
        let mut budget = 2;
        let end = read_line(reader, &mut budget, target)?
            .ok_or_else(|| format!("{target} closed the connection in the middle of its reply"))?;
        if !end.is_empty() {
            return Err(format!("{target} sent a malformed chunk"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, TcpListener, TcpStream},
        thread,
    };

    use serde_json::json;

    use super::*;
    use crate::remote_control::transport::LocalConnector;

    const QUICK: Duration = Duration::from_secs(5);

    /// Reads one request (head and the body its Content-Length announces).
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                return request;
            }
            request.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).unwrap();
        request.extend_from_slice(&body);
        request
    }

    /// A one-connection server: reads the request, hands the stream and the
    /// request bytes to `script`, and finishes when the script does, returning
    /// the request it saw.
    fn serve(
        script: impl FnOnce(&mut TcpStream, &[u8]) + Send + 'static,
    ) -> (u16, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            script(&mut stream, &request);
            request
        });
        (port, handle)
    }

    /// Waits for the client to hang up, so the script outlives the exchange.
    fn wait_for_hangup(stream: &mut TcpStream) {
        let mut sink = [0u8; 256];
        while matches!(stream.read(&mut sink), Ok(count) if count > 0) {}
    }

    fn get(port: u16, path: &str) -> Result<Response, String> {
        request(&LocalConnector::new(), port, "GET", path, None, QUICK)
    }

    fn small_limits() -> Limits {
        Limits {
            head: 256,
            body: 16,
        }
    }

    #[test]
    fn a_counted_reply_is_read_and_the_request_carries_the_required_headers() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
                )
                .unwrap();
            wait_for_hangup(stream);
        });
        let body = json!({"capabilities": {"a": [1, 2]}});
        let response = request(
            &LocalConnector::new(),
            port,
            "POST",
            "/session",
            Some(&body),
            QUICK,
        )
        .unwrap();
        assert_eq!(response.status, 200);
        assert!(response.is_success());
        assert_eq!(response.body, b"{\"ok\":true}");
        assert_eq!(response.json().unwrap()["ok"], json!(true));
        let sent = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(sent.starts_with("POST /session HTTP/1.1\r\n"), "{sent}");
        assert!(
            sent.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "{sent}"
        );
        assert!(sent.contains("Connection: close\r\n"), "{sent}");
        assert!(
            sent.contains("Content-Type: application/json\r\n"),
            "{sent}"
        );
        assert!(sent.contains("Content-Length: 28\r\n"), "{sent}");
        assert!(sent.ends_with("{\"capabilities\":{\"a\":[1,2]}}"), "{sent}");
    }

    #[test]
    fn a_get_without_a_body_announces_an_empty_one() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]")
                .unwrap();
            wait_for_hangup(stream);
        });
        let response = get(port, "/source?format=json").unwrap();
        assert_eq!(response.body, b"[]");
        let sent = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(
            sent.starts_with("GET /source?format=json HTTP/1.1\r\n"),
            "{sent}"
        );
        assert!(sent.contains("Content-Length: 0\r\n"), "{sent}");
    }

    #[test]
    fn a_chunked_reply_is_reassembled_with_extensions_and_trailers() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n5;name=value\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: 1\r\n\r\n",
                )
                .unwrap();
            wait_for_hangup(stream);
        });
        let response = get(port, "/chunked").unwrap();
        assert_eq!(response.body, b"hello world");
        server.join().unwrap();
    }

    #[test]
    fn a_reply_without_a_length_runs_until_the_server_closes() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(b"HTTP/1.0 200 OK\r\n\r\nuntil the end")
                .unwrap();
            // Dropping the stream closes it.
        });
        let response = get(port, "/close").unwrap();
        assert_eq!(response.body, b"until the end");
        server.join().unwrap();
    }

    #[test]
    fn a_reply_with_no_body_status_does_not_wait_for_a_close() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\n\r\n")
                .unwrap();
            wait_for_hangup(stream);
        });
        let started = Instant::now();
        let response = get(port, "/empty").unwrap();
        assert_eq!(response.status, 204);
        assert!(response.body.is_empty());
        assert!(started.elapsed() < Duration::from_secs(3));
        server.join().unwrap();
    }

    #[test]
    fn interim_replies_are_skipped() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(
                    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok",
                )
                .unwrap();
            wait_for_hangup(stream);
        });
        let response = get(port, "/interim").unwrap();
        assert_eq!(
            (response.status, response.body.as_slice()),
            (201, &b"ok"[..])
        );
        server.join().unwrap();
    }

    #[test]
    fn a_redirect_is_a_status_and_is_not_followed() {
        // The listener accepts once; following the Location would have nowhere to go.
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\n\r\n",
                )
                .unwrap();
            wait_for_hangup(stream);
        });
        let response = get(port, "/moved").unwrap();
        assert_eq!(response.status, 302);
        assert!(!response.is_success());
        server.join().unwrap();
    }

    #[test]
    fn an_error_status_still_returns_its_body() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 40\r\n\r\n{\"value\":{\"error\":\"invalid session id\"}}\n",
                )
                .unwrap();
            wait_for_hangup(stream);
        });
        let response = get(port, "/session/x/window/size").unwrap();
        assert_eq!(response.status, 404);
        assert_eq!(
            response.json().unwrap()["value"]["error"],
            json!("invalid session id")
        );
        server.join().unwrap();
    }

    fn exchange_with(port: u16, limits: Limits, timeout: Duration) -> Result<Response, String> {
        exchange(
            &LocalConnector::new(),
            port,
            "GET",
            "/x",
            None,
            timeout,
            limits,
        )
    }

    #[test]
    fn replies_past_the_limits_are_refused() {
        // Announced larger than the body limit: refused before reading it.
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .unwrap();
            wait_for_hangup(stream);
        });
        let error = exchange_with(port, small_limits(), QUICK).unwrap_err();
        assert!(error.contains("larger than 16 bytes"), "{error}");
        server.join().unwrap();

        // Streamed until close, with no length to refuse up front.
        let (port, server) = serve(|stream, _| {
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
            let _ = stream.write_all(&[b'x'; 200]);
        });
        let error = exchange_with(port, small_limits(), QUICK).unwrap_err();
        assert!(error.contains("larger than 16 bytes"), "{error}");
        server.join().unwrap();

        // Chunks that add up past the limit.
        let (port, server) = serve(|stream, _| {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nA\r\n0123456789\r\nA\r\n0123456789\r\n0\r\n\r\n",
            );
            wait_for_hangup(stream);
        });
        let error = exchange_with(port, small_limits(), QUICK).unwrap_err();
        assert!(error.contains("larger than 16 bytes"), "{error}");
        server.join().unwrap();

        // A head that never ends.
        let (port, server) = serve(|stream, _| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n");
            let _ = stream.write_all(format!("X-Pad: {}\r\n", "p".repeat(600)).as_bytes());
            wait_for_hangup(stream);
        });
        let error = exchange_with(port, small_limits(), QUICK).unwrap_err();
        assert!(error.contains("longer than the limit"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_reply_cut_short_is_an_error_not_a_short_body() {
        let (port, server) = serve(|stream, _| {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabcd")
                .unwrap();
        });
        let error = get(port, "/short").unwrap_err();
        assert!(error.contains("before the whole reply"), "{error}");
        server.join().unwrap();

        let (port, server) = serve(|stream, _| {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Le").unwrap();
        });
        let error = get(port, "/cut").unwrap_err();
        assert!(error.contains("middle of its reply"), "{error}");
        server.join().unwrap();

        let (port, server) = serve(|stream, _| {
            stream.write_all(b"not http at all\r\n\r\n").unwrap();
        });
        let error = get(port, "/other").unwrap_err();
        assert!(error.contains("did not answer with HTTP"), "{error}");
        server.join().unwrap();

        let (port, server) = serve(|_stream, _| {});
        let error = get(port, "/silent").unwrap_err();
        assert!(error.contains("without answering"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn a_server_that_never_answers_times_out() {
        let (port, server) = serve(|stream, _| wait_for_hangup(stream));
        let started = Instant::now();
        let error = exchange_with(port, DEFAULT_LIMITS, Duration::from_millis(400)).unwrap_err();
        assert!(error.contains("did not answer in time"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        server.join().unwrap();
    }

    #[test]
    fn a_server_that_trickles_bytes_cannot_hold_the_exchange_open() {
        let (port, server) = serve(|stream, _| {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n");
            // A header byte at a time, forever: no read ever finds the socket idle.
            for _ in 0..200 {
                if stream.write_all(b"x").is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let error = exchange_with(port, DEFAULT_LIMITS, Duration::from_millis(500)).unwrap_err();
        assert!(error.contains("did not answer in time"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
        server.join().unwrap();
    }

    #[test]
    fn a_bad_method_or_path_never_reaches_the_wire() {
        let connector = LocalConnector::new();
        // Nothing listens on port 1; failing validation first is what these check.
        for path in [
            "status",
            "",
            "/with space",
            "/line\r\nbreak",
            "/frag#ment",
            "/caf\u{e9}",
        ] {
            let error = request(&connector, 1, "GET", path, None, QUICK).unwrap_err();
            assert!(error.contains("HTTP path"), "{path:?}: {error}");
        }
        let error = request(&connector, 1, "GE T", "/ok", None, QUICK).unwrap_err();
        assert!(error.contains("not an HTTP method"), "{error}");
        let error = request(&connector, 1, "get", "/ok", None, QUICK).unwrap_err();
        assert!(error.contains("not an HTTP method"), "{error}");
    }

    #[test]
    fn a_refused_port_reports_the_connectors_own_message() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = get(port, "/status").unwrap_err();
        assert!(error.contains("nothing is listening"), "{error}");
    }

    #[test]
    fn json_errors_do_not_echo_the_body() {
        let response = Response {
            status: 200,
            body: b"<html>secret-looking text</html>".to_vec(),
        };
        let error = response.json().unwrap_err();
        assert!(!error.contains("secret-looking"), "{error}");
        assert!(error.contains("HTTP 200"), "{error}");
        assert!(response.snippet().contains("secret-looking"));
    }
}
