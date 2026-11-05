//! `web_fetch` and `web_search`.
//!
//! Over hyper directly rather than `agent_rt::Transport`, which was made for one thing: it hides a
//! response's headers, so a redirect cannot be followed or a content type read, and it sends every
//! request with `Accept: text/event-stream`, which is what some sites answer with a 406.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, HOST, LOCATION, USER_AGENT};
use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::{Tool, ToolCtx, ToolFuture, ToolResult, future};
use crate::model::PermissionMode;
use crate::prompt::text;

const MAX_REDIRECTS: usize = 5;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGE_CHARS: usize = 200_000;
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_URLS: usize = 20;
const WEB_RESULT_MAX_TITLE: usize = 2_048;
const WEB_RESULT_MAX_CONTENT: usize = 64 * 1024;
/// Mewrk's default for how much of one fetched page reaches the model, in its estimate of tokens.
const FETCH_TOKEN_CAP: usize = 2_000;
/// Browser-like, since some sites return an empty shell to anything else.
const PAGE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
const EXA_MCP_TIMEOUT: Duration = Duration::from_secs(25);
const SEARCH_MIN_QUERY: usize = 2;
const SEARCH_MAX_QUERY: usize = 200;
const SEARCH_RESULTS: u32 = 5;

pub(super) fn register(v: &mut Vec<Arc<dyn Tool>>) {
    v.push(Arc::new(WebFetch));
    v.push(Arc::new(WebSearch));
}

// ---------------------------------------------------------------- urls

#[derive(Clone, Debug, PartialEq, Eq)]
struct Url {
    tls: bool,
    host: String,
    port: u16,
    /// With the query, percent-encoded, and never empty.
    path: String,
}

impl Url {
    fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (tls, rest) = if let Some(rest) = strip_prefix_ignore_case(raw, "https://") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(raw, "http://") {
            (false, rest)
        } else {
            return Err(format!("Only HTTP(S) URLs can be fetched: {raw}"));
        };
        let without_fragment = rest.split('#').next().unwrap_or(rest);
        let (authority, path) = match without_fragment.find(['/', '?']) {
            Some(at) => (&without_fragment[..at], &without_fragment[at..]),
            None => (without_fragment, ""),
        };
        if authority.contains('@') {
            return Err("Fetch URL must not include a username or password".to_owned());
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, tail) = bracketed.split_once(']').ok_or_else(|| format!("Invalid fetch URL: {raw}"))?;
            (host.to_owned(), tail.strip_prefix(':').map(str::to_owned))
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            (host.to_owned(), Some(port.to_owned()))
        } else {
            (authority.to_owned(), None)
        };
        if host.is_empty() {
            return Err(format!("Fetch URL has no host: {raw}"));
        }
        if !host.is_ascii() {
            return Err(format!("Fetch URL has a non-ASCII host, which this client cannot resolve; use its punycode form: {raw}"));
        }
        let port = match port.filter(|p| !p.is_empty()) {
            Some(p) => p.parse().map_err(|_| format!("Invalid fetch URL: {raw}"))?,
            None if tls => 443,
            None => 80,
        };
        Ok(Self { tls, host: host.to_ascii_lowercase(), port, path: encode_path(if path.is_empty() { "/" } else { path }) })
    }

    fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        if self.port == if self.tls { 443 } else { 80 } { host } else { format!("{host}:{}", self.port) }
    }

    fn shown(&self) -> String {
        format!("{}://{}{}", if self.tls { "https" } else { "http" }, self.authority(), self.path)
    }

    /// Where a `Location` value leads from here.
    fn join(&self, location: &str) -> Result<Self, String> {
        let location = location.trim();
        if strip_prefix_ignore_case(location, "http://").is_some() || strip_prefix_ignore_case(location, "https://").is_some() {
            return Self::parse(location);
        }
        if let Some(rest) = location.strip_prefix("//") {
            return Self::parse(&format!("{}://{rest}", if self.tls { "https" } else { "http" }));
        }
        let (own_path, _) = self.path.split_once('?').unwrap_or((&self.path, ""));
        let (target, query) = match location.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (location, None),
        };
        let target = target.split('#').next().unwrap_or(target);
        let base = if let Some(absolute) = target.strip_prefix('/') {
            format!("/{absolute}")
        } else if target.is_empty() {
            own_path.to_owned()
        } else {
            let directory = own_path.rsplit_once('/').map_or("", |(d, _)| d);
            format!("{directory}/{target}")
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in base.split('/').skip(1) {
            match part {
                "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        let mut path = format!("/{}", parts.join("/"));
        if let Some(query) = query {
            path.push('?');
            path.push_str(query);
        }
        Ok(Self { path: encode_path(&path), ..self.clone() })
    }
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &value[prefix.len()..])
}

/// What `http::Uri` will not take: anything past ASCII, and spaces and quotes a page's link may still hold.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_graphic() && !matches!(byte, b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn is_private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_broadcast() || v4.is_documentation() || v4.is_unspecified() || v4.is_multicast()
                // CGNAT and benchmarking ranges are not publicly routable.
                || (a == 100 && (64..128).contains(&b))
                || (a == 198 && (18..20).contains(&b))
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v6.to_ipv4_mapped().is_some_and(|m| is_private_address(IpAddr::V4(m)))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Refuse every private address: the rule for any URL the user did not name.
    Public,
    /// Accept one the call named directly, as reaching a dev server on this machine is the point of such a call. Never for a redirect: the page chose
    /// those.
    NamedLocal,
    #[cfg(test)]
    Anywhere,
}

impl Reach {
    /// What a redirect from a target reached this way may reach.
    fn for_redirect(self) -> Self {
        match self {
            #[cfg(test)]
            Self::Anywhere => Self::Anywhere,
            _ => Self::Public,
        }
    }
}

/// Rejects the whole target if any address it resolves to is private. Choosing one public address would let a rebinding attacker choose the
/// connection target.
async fn resolve_target(url: &Url, reach: Reach) -> Result<SocketAddr, String> {
    let shown = url.shown();
    let candidates: Vec<SocketAddr> = match url.host.parse::<IpAddr>() {
        Ok(address) => vec![SocketAddr::new(address, url.port)],
        Err(_) => tokio::net::lookup_host((url.host.as_str(), url.port)).await.map_err(|e| format!("Could not resolve {}: {e}", url.host))?.collect(),
    };
    let first = *candidates.first().ok_or_else(|| format!("{shown} did not resolve to any address"))?;
    if reach == Reach::Public
        && let Some(private) = candidates.iter().find(|a| is_private_address(a.ip()))
    {
        return Err(format!("Refused to fetch a target resolving to a private-network address ({}): {shown}", private.ip()));
    }
    Ok(first)
}

// ---------------------------------------------------------------- one http exchange

fn tls_connector() -> &'static Result<TlsConnector, String> {
    static CONNECTOR: OnceLock<Result<TlsConnector, String>> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut config = ClientConfig::builder_with_provider(rustls_graviola::default_provider().into())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(TlsConnector::from(Arc::new(config)))
    })
}

struct Exchange {
    status: StatusCode,
    location: Option<String>,
    content_type: String,
    body: Vec<u8>,
}

async fn send<T>(io: T, request: Request<Full<Bytes>>, limit: usize) -> Result<Exchange, String>
where
    T: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, connection) = http1::handshake(io).await.map_err(|e| e.to_string())?;
    let task = tokio::spawn(async move {
        let _ = connection.await;
    });
    let result = async {
        let response = sender.send_request(request).await.map_err(|e| e.to_string())?;
        let status = response.status();
        let headers = response.headers();
        let location = headers.get(LOCATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
        if let Some(encoding) = headers.get(CONTENT_ENCODING).and_then(|v| v.to_str().ok()).filter(|e| !e.eq_ignore_ascii_case("identity")) {
            return Err(format!("the server sent {encoding}-compressed content, which was not asked for and cannot be read here"));
        }
        let mut body = Vec::new();
        let mut incoming = response.into_body();
        // A page past the cap is cut, not failed: its start is what the model reads anyway.
        while body.len() < limit {
            match incoming.frame().await {
                None => break,
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        body.extend_from_slice(&data[..data.len().min(limit - body.len())]);
                    }
                }
            }
        }
        Ok(Exchange { status, location, content_type, body })
    }
    .await;
    task.abort();
    result
}

/// One request on a fresh connection to an address already approved. The body is read within the deadline too: a slow page must not hold a turn for
/// ever.
async fn exchange(
    url: &Url,
    address: SocketAddr,
    method: Method,
    headers: &[(http::HeaderName, &str)],
    payload: Bytes,
    deadline: Instant,
    limit: usize,
) -> Result<Exchange, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT.min(remaining), TcpStream::connect(address))
        .await
        .map_err(|_| "the connection timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    let _ = tcp.set_nodelay(true);
    let mut request = Request::builder().method(method).uri(url.path.as_str()).header(HOST, url.authority());
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    let request = request.body(Full::new(payload)).map_err(|e| e.to_string())?;
    let work = async {
        if url.tls {
            let connector = tls_connector().as_ref().map_err(Clone::clone)?;
            let name = ServerName::try_from(url.host.clone()).map_err(|e| e.to_string())?;
            let stream = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
            send(TokioIo::new(stream), request, limit).await
        } else {
            send(TokioIo::new(tcp), request, limit).await
        }
    };
    tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), work).await.map_err(|_| "the request timed out".to_owned())?
}

#[cfg_attr(test, derive(Debug))]
struct Page {
    url: String,
    content_type: String,
    body: Vec<u8>,
}

/// A GET, following redirects by hand so that every hop gets the same checks.
async fn fetch(target: &str, reach: Reach) -> Result<Page, String> {
    let mut url = Url::parse(target)?;
    let deadline = Instant::now() + PAGE_TIMEOUT;
    let mut hop_reach = reach;
    for _ in 0..=MAX_REDIRECTS {
        let address = resolve_target(&url, hop_reach).await?;
        hop_reach = hop_reach.for_redirect();
        let shown = url.shown();
        let headers = [(USER_AGENT, PAGE_USER_AGENT), (ACCEPT, "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.8"), (ACCEPT_ENCODING, "identity")];
        let response = exchange(&url, address, Method::GET, &headers, Bytes::new(), deadline, MAX_PAGE_BYTES).await.map_err(|e| format!("Fetch {shown} failed: {e}"))?;
        if response.status.is_redirection() && response.status != StatusCode::NOT_MODIFIED {
            let location = response.location.ok_or_else(|| format!("{shown} returned {} without a Location header", response.status))?;
            url = url.join(&location)?;
            continue;
        }
        if !response.status.is_success() {
            return Err(format!("Fetch {shown} returned {}", response.status));
        }
        return Ok(Page { url: shown, content_type: response.content_type, body: response.body });
    }
    Err(format!("Fetch redirects exceeded {MAX_REDIRECTS} hops: {target}"))
}

// ---------------------------------------------------------------- html to text

/// Subtrees that cannot hold readable text.
const DROPPED: [&str; 6] = ["script", "style", "noscript", "svg", "iframe", "template"];
const BLOCKS: [&str; 22] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "div",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "tr",
];

/// The offset of `</name` from `from` on, whatever its case.
fn find_close(lower: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut at = from;
    while let Some(i) = memchr::memchr(b'<', &lower[at..]) {
        let start = at + i;
        if lower.get(start + 1) == Some(&b'/') && lower.get(start + 2..start + 2 + name.len()) == Some(name) {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

/// One pass over the bytes rather than one per element kind, so a page of ten thousand tags costs what a page of ten does per byte.
fn html_to_text(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(html.len() / 2);
    let mut title = String::new();
    let mut pos = 0;
    while let Some(i) = memchr::memchr(b'<', &bytes[pos..]) {
        let open = pos + i;
        out.push_str(&html[pos..open]);
        if bytes[open..].starts_with(b"<!--") {
            match lower[open + 4..].find("-->") {
                Some(end) => pos = open + 4 + end + 3,
                // An unclosed comment swallows the rest.
                None => return (decode_entities(&title), collapse(&decode_entities(&out))),
            }
            continue;
        }
        let Some(end) = memchr::memchr(b'>', &bytes[open..]).map(|e| open + e) else { break };
        let tag = &lower[open + 1..end];
        let name: String = tag.trim_start_matches('/').chars().take_while(char::is_ascii_alphanumeric).collect();
        let closing = tag.starts_with('/');
        pos = end + 1;
        if closing {
            if BLOCKS.contains(&name.as_str()) {
                out.push('\n');
            }
            continue;
        }
        let self_closed = tag.ends_with('/');
        if DROPPED.contains(&name.as_str()) {
            out.push('\n');
            if self_closed {
                continue;
            }
            match find_close(bytes, pos, name.as_bytes()) {
                Some(close) => {
                    pos = memchr::memchr(b'>', &bytes[close..]).map_or(bytes.len(), |e| close + e + 1);
                }
                None => return (decode_entities(&title), collapse(&decode_entities(&out))),
            }
            continue;
        }
        if name == "title"
            && let Some(close) = find_close(bytes, pos, b"title")
        {
            if title.is_empty() {
                title = html[pos..close].trim().to_owned();
            }
            pos = memchr::memchr(b'>', &bytes[close..]).map_or(bytes.len(), |e| close + e + 1);
            continue;
        }
        if BLOCKS.contains(&name.as_str()) {
            out.push('\n');
        }
    }
    if pos < html.len() && !bytes[pos..].contains(&b'<') {
        out.push_str(&html[pos..]);
    }
    (decode_entities(&title), collapse(&decode_entities(&out)))
}

fn decode_entities(source: &str) -> String {
    if !source.contains('&') {
        return source.to_owned();
    }
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let window_end = (1..=tail.len().min(12)).rev().find(|e| tail.is_char_boundary(*e)).unwrap_or(0);
        let decoded = tail[..window_end].find(';').and_then(|end| {
            let entity = &tail[1..end];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                _ => entity
                    .strip_prefix('#')
                    .and_then(|number| match number.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse::<u32>().ok(),
                    })
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn collapse(content: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() && lines.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        lines.push(trimmed);
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let joined = lines.join("\n");
    match joined.char_indices().nth(MAX_PAGE_CHARS) {
        Some((end, _)) => joined[..end].to_owned(),
        None => joined,
    }
}

/// Token budget by the estimate the rest of Mewrk uses: ASCII at 1/4 token, anything else at 1/1.6.
fn slice_by_tokens(content: &str, limit: usize) -> &str {
    let (mut ascii, mut wide) = (0u64, 0u64);
    for (index, c) in content.char_indices() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
        if ((ascii as f64 / 4.0) + (wide as f64 / 1.6)).ceil() as usize > limit {
            return &content[..index];
        }
    }
    content
}

fn readable(page: &Page) -> Result<(String, String), String> {
    if memchr::memchr(0, &page.body[..page.body.len().min(8192)]).is_some() {
        return Err(format!("{} has no extractable content", page.url));
    }
    let source = String::from_utf8_lossy(&page.body);
    let kind = page.content_type.to_ascii_lowercase();
    // A code file or a JSON answer is what it is, and its angle brackets are not tags.
    let plain = !kind.is_empty() && !kind.contains("html") && !kind.contains("xml");
    let (title, content) = if plain { (String::new(), collapse(&source)) } else { html_to_text(&source) };
    if content.trim().is_empty() {
        return Err(format!("{} has no extractable content", page.url));
    }
    let title = if title.trim().is_empty() { page.url.clone() } else { title };
    let sliced = slice_by_tokens(&content, FETCH_TOKEN_CAP);
    let content = if sliced.len() < content.len() { format!("{sliced}...") } else { content };
    Ok((title, content))
}

/// Web text reaches the context as data. Control and direction-changing characters go, and a line that claims to be a role is marked if the profile
/// says how.
fn sanitize(value: &str, max_chars: usize) -> String {
    let marker = text("web.untrusted_marker");
    let mut out = String::new();
    for (index, line) in value.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let line: String = line.chars().filter(|c| !c.is_control() && !matches!(*c, '\u{061c}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}')).collect();
        let probe = line.trim_start().to_ascii_lowercase();
        if ["system:", "developer:", "human:", "assistant:", "tool:", "<system-reminder", "<task-notification", "[system]", "ignore previous", "ignore all previous"]
            .iter()
            .any(|p| probe.starts_with(p))
        {
            out.push_str(marker);
        }
        out.push_str(&line);
        if out.len() >= max_chars {
            break;
        }
    }
    match out.char_indices().nth(max_chars) {
        Some((end, _)) => out[..end].to_owned(),
        None => out,
    }
}

fn results_output(items: &[(String, String, String)]) -> String {
    let prefix = format!("{:08x}", fastrand::u32(..));
    let results: Vec<Value> = items
        .iter()
        .enumerate()
        .map(|(i, (title, url, content))| {
            json!({
                "id": format!("{prefix}-{}", i + 1),
                "title": sanitize(title, WEB_RESULT_MAX_TITLE),
                "url": sanitize(url, WEB_RESULT_MAX_TITLE),
                "content": sanitize(content, WEB_RESULT_MAX_CONTENT),
            })
        })
        .collect();
    let mut envelope = json!({"results": results, "untrustedWebContent": true});
    let notice = text("web.results_notice");
    if !notice.is_empty() {
        envelope["notice"] = Value::String(notice.to_owned());
    }
    serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| "{\"results\":[]}".to_owned())
}

// ---------------------------------------------------------------- web_fetch

fn parse_urls(args: &Value) -> Result<Vec<String>, String> {
    let raw = args.get("urls").and_then(Value::as_array).ok_or("web_fetch.urls must be an array of strings")?;
    let mut urls = Vec::with_capacity(raw.len());
    for entry in raw {
        let url = entry.as_str().ok_or("Each web_fetch.urls entry must be a string")?.trim();
        if !url.is_empty() {
            urls.push(url.to_owned());
        }
    }
    if urls.is_empty() {
        return Err("At least one URL is required".to_owned());
    }
    if urls.len() > MAX_URLS {
        return Err(format!("At most {MAX_URLS} URLs can be fetched at once"));
    }
    let invalid: Vec<&str> = urls.iter().filter(|u| Url::parse(u).is_err()).map(String::as_str).collect();
    if !invalid.is_empty() {
        return Err(format!("Not an absolute HTTP(S) URL: {}", invalid.join(", ")));
    }
    Ok(urls)
}

async fn fetch_all(urls: Vec<String>, reach: Reach) -> Result<String, String> {
    let mut handles = Vec::with_capacity(urls.len());
    for url in urls {
        handles.push(tokio::spawn(async move {
            let page = fetch(&url, reach).await?;
            let (title, content) = readable(&page)?;
            Ok::<_, String>((title, page.url, content))
        }));
    }
    let mut items = Vec::new();
    let mut first_error = None;
    for handle in handles {
        match handle.await {
            Ok(Ok(item)) => items.push(item),
            Ok(Err(e)) => {
                first_error.get_or_insert(e);
            }
            Err(e) => {
                first_error.get_or_insert(e.to_string());
            }
        }
    }
    // What came through is returned; only when nothing did is the first failure the answer.
    if items.is_empty() {
        return Err(format!("Web fetch failed: {}", first_error.unwrap_or_else(|| "no results".to_owned())));
    }
    Ok(results_output(&items))
}

struct WebFetch;

impl Tool for WebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }
    fn dangerous(&self, _: &Value) -> bool {
        true
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        args.get("urls").and_then(Value::as_array).map_or_else(String::new, |urls| urls.iter().filter_map(Value::as_str).map(str::trim).collect::<Vec<_>>().join("\n"))
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let urls = parse_urls(&args)?;
            // Only where nothing asks does a call get to reach this machine's own network.
            let reach = if ctx.spec.settings.permission == PermissionMode::FullAccess { Reach::NamedLocal } else { Reach::Public };
            tokio::select! {
                r = fetch_all(urls, reach) => r.map(ToolResult::ok),
                () = ctx.run.cancel.cancelled() => Err("The run was stopped before the fetch finished.".to_owned()),
            }
        })
    }
}

// ---------------------------------------------------------------- web_search

fn parse_query(args: &Value) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or("web_search.query must be a non-empty string: use one self-contained query without pronouns that refer to earlier context")?;
    let length = query.chars().count();
    if length < SEARCH_MIN_QUERY {
        return Err(format!("web_search.query must be at least {SEARCH_MIN_QUERY} characters"));
    }
    if length > SEARCH_MAX_QUERY {
        return Err(format!("web_search.query exceeds the {SEARCH_MAX_QUERY}-character limit; split a long question into multiple searches"));
    }
    Ok(query.to_owned())
}

fn mcp_content_text(payload: &str) -> Option<String> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let joined = value
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|i| i.get("text").and_then(Value::as_str))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!joined.is_empty()).then_some(joined)
}

/// Blank-line separated `Title:`, `URL:` and `Text:` blocks, out of SSE lines or a plain JSON body.
fn parse_exa_payload(body: &str) -> Vec<(String, String, String)> {
    let mut chunks: Vec<String> =
        body.lines().filter_map(|l| l.strip_prefix("data: ")).map(str::trim).filter(|p| !p.is_empty() && *p != "[DONE]").filter_map(mcp_content_text).collect();
    if chunks.is_empty()
        && let Some(found) = mcp_content_text(body)
    {
        chunks.push(found);
    }
    if chunks.is_empty() && body.contains("Title:") {
        chunks.push(body.to_owned());
    }
    parse_exa_blocks(&chunks.join("\n\n"))
}

/// A block opens at its `Title:`, and its text runs from `Text:` or `Highlights:` to the next one, blank lines and all: the service has changed that
/// label before.
fn parse_exa_blocks(raw: &str) -> Vec<(String, String, String)> {
    let mut items: Vec<(String, String, String)> = Vec::new();
    let mut in_text = false;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("Title:") {
            items.push((rest.trim().to_owned(), String::new(), String::new()));
            in_text = false;
        } else if in_text {
            if let Some((_, _, content)) = items.last_mut() {
                content.push('\n');
                content.push_str(line);
            }
        } else if let Some(rest) = line.strip_prefix("URL:") {
            if items.is_empty() {
                items.push(Default::default());
            }
            if let Some((_, url, _)) = items.last_mut() {
                *url = rest.trim().to_owned();
            }
        } else if let Some(rest) = line.strip_prefix("Text:").or_else(|| line.strip_prefix("Highlights:")) {
            if items.is_empty() {
                items.push(Default::default());
            }
            if let Some((_, _, content)) = items.last_mut() {
                *content = rest.trim().to_owned();
            }
            in_text = true;
        }
    }
    for (_, _, content) in &mut items {
        *content = content.trim().to_owned();
    }
    items
}

async fn exa_search(endpoint: &str, reach: Reach, query: &str) -> Result<String, String> {
    let url = Url::parse(endpoint)?;
    let address = resolve_target(&url, reach).await?;
    let payload = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "web_search_exa", "arguments": {"query": query, "type": "auto", "livecrawl": "fallback", "numResults": SEARCH_RESULTS}},
    });
    let headers = [(ACCEPT, "application/json, text/event-stream"), (CONTENT_TYPE, "application/json"), (USER_AGENT, PAGE_USER_AGENT), (ACCEPT_ENCODING, "identity")];
    let deadline = Instant::now() + EXA_MCP_TIMEOUT;
    let response = exchange(&url, address, Method::POST, &headers, Bytes::from(payload.to_string()), deadline, MAX_PAGE_BYTES)
        .await
        .map_err(|e| format!("Web search failed: {e}"))?;
    let body = String::from_utf8_lossy(&response.body);
    if !response.status.is_success() {
        return Err(format!("Web search failed: the search service returned {}: {}", response.status, body.chars().take(300).collect::<String>()));
    }
    let items = parse_exa_payload(&body);
    Ok(results_output(&items.into_iter().take(SEARCH_RESULTS as usize).collect::<Vec<_>>()))
}

struct WebSearch;

impl Tool for WebSearch {
    fn name(&self) -> &'static str {
        "web_search"
    }
    fn dangerous(&self, _: &Value) -> bool {
        true
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        args.get("query").and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let query = parse_query(&args)?;
            tokio::select! {
                r = exa_search(EXA_MCP_URL, Reach::Public, &query) => r.map(ToolResult::ok),
                () = ctx.run.cancel.cancelled() => Err("The run was stopped before the search finished.".to_owned()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A tiny server: `route` maps a request line's path to a raw response.
    async fn serve(route: fn(&str) -> String) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut seen = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => seen.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&seen).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let _ = socket.write_all(route(&path).as_bytes()).await;
                });
            }
        });
        port
    }

    fn ok(content_type: &str, body: &str) -> String {
        format!("HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    }

    fn redirect(to: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nlocation: {to}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
    }

    /// Over the real network, so left to be run by hand: `cargo test -p mewrk-core live_web -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn live_web() {
        let out = fetch_all(vec!["https://example.com/".to_owned()], Reach::Public).await.unwrap();
        assert!(out.contains("Example Domain"), "{out}");
        let out = exa_search(EXA_MCP_URL, Reach::Public, "rust regex crate").await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert!(parsed["results"].as_array().is_some_and(|r| !r.is_empty()), "{out}");
        assert!(parsed["results"][0]["content"].as_str().is_some_and(|c| !c.is_empty()), "{out}");
    }

    #[test]
    fn urls_parse_and_join() {
        let url = Url::parse("HTTPS://Example.com/a/b?x=1#frag").unwrap();
        assert_eq!(url, Url { tls: true, host: "example.com".into(), port: 443, path: "/a/b?x=1".into() });
        assert_eq!(url.shown(), "https://example.com/a/b?x=1");
        assert_eq!(Url::parse("http://h:8080").unwrap().shown(), "http://h:8080/");
        assert_eq!(Url::parse("http://[::1]:9/x").unwrap().host, "::1");
        assert_eq!(Url::parse("http://h/a b/世").unwrap().path, "/a%20b/%E4%B8%96");
        for bad in ["ftp://x", "example.com", "http://", "http://u:p@host/", "http://h:notaport/"] {
            assert!(Url::parse(bad).is_err(), "{bad}");
        }
        let base = Url::parse("https://h/a/b/c?q=1").unwrap();
        assert_eq!(base.join("/x").unwrap().shown(), "https://h/x");
        assert_eq!(base.join("d").unwrap().shown(), "https://h/a/b/d");
        assert_eq!(base.join("../d?z=2").unwrap().shown(), "https://h/a/d?z=2");
        assert_eq!(base.join("//other/p").unwrap().shown(), "https://other/p");
        assert_eq!(base.join("http://else/").unwrap().shown(), "http://else/");
    }

    #[test]
    fn html_becomes_text() {
        let html = "<!doctype html><html><head><title>A &amp; B</title><style>p{}</style></head><body>\n<!-- gone --><h1>Head</h1><p>One <b>bold</b> &lt;tag&gt;.</p><SCRIPT type=x>nope()</SCRIPT><div>Two&nbsp;&#x41;&#66;</div><scriptish>kept</scriptish><svg/>Tail</body></html>";
        let (title, content) = html_to_text(html);
        assert_eq!(title, "A & B");
        assert_eq!(content, "Head\n\nOne bold <tag>.\n\nTwo AB\nkept\nTail");
        // An unclosed script swallows the rest, and an implicit `</head>` loses nothing.
        assert_eq!(html_to_text("<p>a</p><script>b").1, "a");
        assert_eq!(html_to_text("<head><meta x><title>t</title><p>body").1, "body");
        assert_eq!(html_to_text("plain & simple").1, "plain & simple");
        assert_eq!(decode_entities("&#xZZ; &bogus; 世&amp;"), "&#xZZ; &bogus; 世&");
    }

    #[test]
    fn plain_text_keeps_its_angle_brackets() {
        let page = Page { url: "u".into(), content_type: "text/plain; charset=utf-8".into(), body: b"Vec<u8> and <T>\n\n\n\nnext".to_vec() };
        let (title, content) = readable(&page).unwrap();
        assert_eq!(title, "u");
        assert_eq!(content, "Vec<u8> and <T>\n\nnext");
        let binary = Page { url: "u".into(), content_type: "application/pdf".into(), body: b"%PDF\0\0".to_vec() };
        assert!(readable(&binary).unwrap_err().contains("no extractable content"));
    }

    #[test]
    fn a_page_is_cut_to_mewrks_budget() {
        let long = "word ".repeat(10_000);
        let page = Page { url: "u".into(), content_type: "text/plain".into(), body: long.clone().into_bytes() };
        let (_, content) = readable(&page).unwrap();
        assert!(content.ends_with("...") && content.len() < long.len());
        assert_eq!(slice_by_tokens("abcd", 1), "abcd");
        assert_eq!(slice_by_tokens("abcde", 1), "abcd");
    }

    #[test]
    fn private_addresses_are_private() {
        for (address, private) in [
            ("127.0.0.1", true),
            ("10.1.2.3", true),
            ("192.168.0.1", true),
            ("169.254.1.1", true),
            ("100.64.0.1", true),
            ("198.18.0.1", true),
            ("8.8.8.8", false),
            ("::1", true),
            ("fe80::1", true),
            ("fc00::1", true),
            ("::ffff:127.0.0.1", true),
            ("2001:4860:4860::8888", false),
        ] {
            assert_eq!(is_private_address(address.parse().unwrap()), private, "{address}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_are_followed_and_every_hop_is_checked() {
        let port = serve(|path| match path {
            "/start" => redirect("/middle?x=1"),
            "/middle?x=1" => redirect("end"),
            "/end" => ok("text/html", "<title>T</title><p>arrived</p>"),
            "/loop" => redirect("/loop"),
            "/nowhere" => "HTTP/1.1 302 Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
            "/chunked" => {
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n".to_owned()
            }
            "/gz" => "HTTP/1.1 200 OK\r\ncontent-encoding: gzip\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
            _ => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
        })
        .await;
        let base = format!("http://127.0.0.1:{port}");
        let page = fetch(&format!("{base}/start"), Reach::Anywhere).await.unwrap();
        assert_eq!(page.url, format!("{base}/end"));
        assert_eq!(readable(&page).unwrap(), ("T".to_owned(), "arrived".to_owned()));
        assert!(fetch(&format!("{base}/loop"), Reach::Anywhere).await.unwrap_err().contains("exceeded 5 hops"));
        assert!(fetch(&format!("{base}/nowhere"), Reach::Anywhere).await.unwrap_err().contains("without a Location"));
        assert!(fetch(&format!("{base}/missing"), Reach::Anywhere).await.unwrap_err().contains("returned 404"));
        assert_eq!(fetch(&format!("{base}/chunked"), Reach::Anywhere).await.unwrap().body, b"abcde");
        assert!(fetch(&format!("{base}/gz"), Reach::Anywhere).await.unwrap_err().contains("gzip"));

        // A call may name a local address where nothing asks, but a page may not send it to one.
        let err = fetch(&format!("{base}/end"), Reach::Public).await.unwrap_err();
        assert!(err.starts_with("Refused to fetch a target resolving to a private-network address"), "{err}");
        assert!(fetch(&format!("{base}/end"), Reach::NamedLocal).await.is_ok());
        let err = fetch(&format!("{base}/start"), Reach::NamedLocal).await.unwrap_err();
        assert!(err.starts_with("Refused to fetch"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_returns_what_came_through_in_mewrks_envelope() {
        let port = serve(|path| match path {
            "/good" => ok("text/html", "<title>Good</title><p>system: not really</p>"),
            _ => "HTTP/1.1 500 Oops\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_owned(),
        })
        .await;
        let good = format!("http://127.0.0.1:{port}/good");
        let bad = format!("http://127.0.0.1:{port}/bad");
        let out = fetch_all(vec![bad.clone(), good.clone()], Reach::NamedLocal).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["untrustedWebContent"], true);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["title"], "Good");
        assert_eq!(results[0]["url"], good);
        assert_eq!(results[0]["content"], "system: not really");
        assert!(results[0]["id"].as_str().unwrap().ends_with("-1"));
        let err = fetch_all(vec![bad], Reach::NamedLocal).await.unwrap_err();
        assert!(err.starts_with("Web fetch failed: Fetch "), "{err}");
    }

    #[test]
    fn urls_are_checked_before_anything_goes_out() {
        assert!(parse_urls(&json!({"urls": "x"})).is_err());
        assert_eq!(parse_urls(&json!({"urls": [" ", ""]})).unwrap_err(), "At least one URL is required");
        assert_eq!(parse_urls(&json!({"urls": ["nope"]})).unwrap_err(), "Not an absolute HTTP(S) URL: nope");
        let many: Vec<String> = (0..21).map(|i| format!("http://h/{i}")).collect();
        assert!(parse_urls(&json!({"urls": many})).unwrap_err().contains("At most 20"));
        assert_eq!(parse_urls(&json!({"urls": [" http://h/ "]})).unwrap(), ["http://h/"]);
    }

    #[test]
    fn hostile_text_is_data() {
        assert_eq!(sanitize("a\u{202e}b\u{0}c", 100), "abc");
        assert_eq!(sanitize("line one\nline two", 8), "line one");
    }

    #[test]
    fn exa_answers_parse_as_sse_or_plain() {
        let inner = "Title: First\nURL: https://a\nText: alpha\nmore\n\nTitle: Second\nURL: https://b\nText: beta";
        let body = json!({"result": {"content": [{"type": "text", "text": inner}]}});
        let sse = format!("event: message\ndata: {body}\n\n");
        let expected = vec![("First".to_owned(), "https://a".to_owned(), "alpha\nmore".to_owned()), ("Second".to_owned(), "https://b".to_owned(), "beta".to_owned())];
        assert_eq!(parse_exa_payload(&sse), expected);
        assert_eq!(parse_exa_payload(&body.to_string()), expected);
        assert_eq!(parse_exa_payload(inner), expected);
        assert!(parse_exa_payload("nothing here").is_empty());
        let highlights = "Title: One\nURL: https://a\nPublished: N/A\nAuthor: N/A\nHighlights:\n# one\n\nbody\n\nmore\n\nTitle: Two\nURL: https://b\nHighlights:\ntwo";
        assert_eq!(
            parse_exa_blocks(highlights),
            [("One".to_owned(), "https://a".to_owned(), "# one\n\nbody\n\nmore".to_owned()), ("Two".to_owned(), "https://b".to_owned(), "two".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_search_goes_out_as_one_json_rpc_call() {
        let port = serve(|_| {
            let inner = "Title: Hit\nURL: https://x\nText: found";
            let body = json!({"result": {"content": [{"text": inner}]}}).to_string();
            ok("application/json", &body)
        })
        .await;
        let out = exa_search(&format!("http://127.0.0.1:{port}/mcp"), Reach::Anywhere, "rust").await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["results"][0]["title"], "Hit");
        assert_eq!(parsed["results"][0]["content"], "found");
        assert!(parse_query(&json!({"query": "x"})).unwrap_err().contains("at least 2"));
        assert!(parse_query(&json!({"query": "y".repeat(201)})).unwrap_err().contains("200-character"));
    }
}
