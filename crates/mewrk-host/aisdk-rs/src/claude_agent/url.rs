//! The part of the WHATWG URL parser (`new URL`) the family's decisions rest on: whether an endpoint is on this
//! machine, whether a search hit is a web address, and which fetched addresses name the same request.
//!
//! There is no URL dependency in this crate, and these three decisions are security decisions: reading the host of
//! `http://127.0.0.1@evil.example` or `http://evil.example\@127.0.0.1` the way the CLI's own parser reads it is the
//! whole point. Internationalized hosts are the one thing left out: a non-ASCII host is compared lowercased rather
//! than through IDNA, so a host spelled in Unicode and its `xn--` form count as two addresses.

use std::net::Ipv6Addr;

/// A parsed URL. `host` is empty for a non-special URL with no authority.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Url {
    pub scheme: String,
    pub userinfo: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
}

impl Url {
    /// `url.href` without its fragment.
    pub fn href(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if let Some(userinfo) = &self.userinfo {
            out.push_str(userinfo);
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(port) = self.port {
            out.push_str(&format!(":{port}"));
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        out
    }

    /// `url.protocol = "https:"`, which drops a port that is the new scheme's default.
    pub fn set_https(&mut self) {
        self.scheme = "https".to_owned();
        if self.port == Some(443) {
            self.port = None;
        }
    }
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

fn is_special(scheme: &str) -> bool {
    default_port(scheme).is_some() || scheme == "file"
}

/// `new URL(value)`, or `None` where it throws.
pub(super) fn parse(value: &str) -> Option<Url> {
    // Surrounding C0 controls and spaces go, tabs and newlines go wherever they are.
    let trimmed = value.trim_matches(|c: char| c <= ' ');
    let cleaned: String = trimmed.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let colon = cleaned.find(':')?;
    let (scheme, rest) = (&cleaned[..colon], &cleaned[colon + 1..]);
    let mut letters = scheme.chars();
    if !letters.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !letters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let special = is_special(&scheme);
    let is_slash = |c: char| c == '/' || (special && c == '\\');
    let rest = rest.split('#').next().unwrap_or(rest);
    let after_slashes = if special {
        rest.trim_start_matches(is_slash)
    } else if let Some(after) = rest.strip_prefix("//") {
        after
    } else {
        // An opaque or relative-to-nothing path: no host at all.
        let (path, query) = split_query(rest);
        return Some(Url {
            scheme,
            userinfo: None,
            host: String::new(),
            port: None,
            path: path.to_owned(),
            query: query.map(str::to_owned),
        });
    };
    let authority_end = after_slashes.find(|c: char| is_slash(c) || c == '?').unwrap_or(after_slashes.len());
    let (authority, tail) = after_slashes.split_at(authority_end);
    // The last `@` ends the credentials: `a@b@host` has the host `host`.
    let (userinfo, host_port) = match authority.rfind('@') {
        Some(at) => (Some(&authority[..at]), &authority[at + 1..]),
        None => (None, authority),
    };
    let (raw_host, raw_port) = split_port(host_port)?;
    let port = match raw_port {
        Some(digits) if !digits.is_empty() => {
            if !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(digits.parse::<u16>().ok()?)
        }
        _ => None,
    };
    let port = port.filter(|p| Some(*p) != default_port(&scheme));
    let host = parse_host(raw_host, special)?;
    if special && host.is_empty() && scheme != "file" {
        return None;
    }
    let (raw_path, raw_query) = split_query(tail);
    let path = normalize_path(raw_path, special);
    let query = raw_query.map(|query| encode(query, |c| matches!(c, ' ' | '"' | '#' | '<' | '>') || (special && c == '\'')));
    Some(Url {
        scheme,
        userinfo: userinfo.filter(|u| !u.is_empty()).map(|u| encode(u, |c| matches!(c, ' ' | '"' | '#' | '<' | '>' | '?' | '`' | '{' | '}' | '/' | ';' | '=' | '[' | '\\' | ']' | '^' | '|'))),
        host,
        port,
        path,
        query,
    })
}

fn split_query(text: &str) -> (&str, Option<&str>) {
    match text.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (text, None),
    }
}

/// Host and port text; a colon inside `[...]` belongs to the host. `None` for an unclosed bracket.
fn split_port(host_port: &str) -> Option<(&str, Option<&str>)> {
    if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        let (host, after) = host_port.split_at(close + 1);
        return match after.strip_prefix(':') {
            Some(port) => Some((host, Some(port))),
            None if after.is_empty() => Some((host, None)),
            None => None,
        };
    }
    Some(match host_port.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (host_port, None),
    })
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' && at + 2 < bytes.len() {
            let high = (bytes[at + 1] as char).to_digit(16);
            let low = (bytes[at + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                at += 3;
                continue;
            }
        }
        out.push(bytes[at]);
        at += 1;
    }
    String::from_utf8(out).ok()
}

fn parse_host(raw: &str, special: bool) -> Option<String> {
    if let Some(inner) = raw.strip_prefix('[') {
        let inner = inner.strip_suffix(']')?;
        let address: Ipv6Addr = inner.parse().ok()?;
        return Some(format!("[{address}]"));
    }
    if !special {
        return Some(raw.to_owned());
    }
    let decoded = percent_decode(raw)?;
    let lowered = if decoded.is_ascii() { decoded.to_ascii_lowercase() } else { decoded.to_lowercase() };
    if lowered.chars().any(|c| {
        c <= ' ' || c == '\u{7f}' || matches!(c, '#' | '/' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '^' | '|' | '%')
    }) {
        return None;
    }
    if ends_in_number(&lowered) {
        return parse_ipv4(&lowered);
    }
    Some(lowered)
}

fn ends_in_number(host: &str) -> bool {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    let Some(last) = parts.last() else { return false };
    if !last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    parse_ipv4_number(last).is_some() && (last.starts_with("0x") || last.starts_with("0X"))
}

fn parse_ipv4_number(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let (digits, radix) = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        (hex, 16)
    } else if text.len() >= 2 && text.starts_with('0') {
        (&text[1..], 8)
    } else {
        (text, 10)
    };
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, radix).ok()
}

/// The IPv4 parser: 1 to 4 numbers, each decimal, octal or hex, the last filling what remains.
fn parse_ipv4(host: &str) -> Option<String> {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let numbers: Option<Vec<u64>> = parts.iter().map(|part| parse_ipv4_number(part)).collect();
    let numbers = numbers?;
    let (last, leading) = numbers.split_last()?;
    if leading.iter().any(|n| *n > 255) || *last >= 256u64.pow((5 - numbers.len()) as u32) {
        return None;
    }
    let mut value = *last;
    for (index, number) in leading.iter().enumerate() {
        value += number * 256u64.pow((3 - index) as u32);
    }
    Some(format!("{}.{}.{}.{}", (value >> 24) & 255, (value >> 16) & 255, (value >> 8) & 255, value & 255))
}

fn encode(text: &str, in_set: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if !(' '..='~').contains(&c) || in_set(c) {
            let mut buffer = [0u8; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_single_dot(segment: &str) -> bool {
    segment == "." || segment.eq_ignore_ascii_case("%2e")
}

fn is_double_dot(segment: &str) -> bool {
    matches!(segment.to_ascii_lowercase().as_str(), ".." | ".%2e" | "%2e." | "%2e%2e")
}

/// Path segments with dot segments resolved and the path percent-encode set applied.
fn normalize_path(raw: &str, special: bool) -> String {
    if raw.is_empty() {
        return if special { "/".to_owned() } else { String::new() };
    }
    let mut out: Vec<String> = Vec::new();
    let body = raw.strip_prefix(['/', '\\']).unwrap_or(raw);
    let segments: Vec<&str> = if special { body.split(['/', '\\']).collect() } else { body.split('/').collect() };
    let count = segments.len();
    for (index, segment) in segments.into_iter().enumerate() {
        let last = index + 1 == count;
        if is_double_dot(segment) {
            out.pop();
            if last {
                out.push(String::new());
            }
        } else if is_single_dot(segment) {
            if last {
                out.push(String::new());
            }
        } else {
            out.push(encode(segment, |c| matches!(c, ' ' | '"' | '#' | '<' | '>' | '?' | '`' | '{' | '}' | '^')));
        }
    }
    format!("/{}", out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn href(value: &str) -> Option<String> {
        parse(value).map(|url| url.href())
    }

    #[test]
    fn spelling_normalizes_the_way_the_url_class_does() {
        assert_eq!(href("HTTP://EXAMPLE.com").as_deref(), Some("http://example.com/"));
        assert_eq!(href("https://example.com:443/a").as_deref(), Some("https://example.com/a"));
        assert_eq!(href("https://example.com:8443/a/../b/./c").as_deref(), Some("https://example.com:8443/b/c"));
        assert_eq!(href("https://example.com/a b?x y").as_deref(), Some("https://example.com/a%20b?x%20y"));
        assert_eq!(href("  https://example.com/a#frag  ").as_deref(), Some("https://example.com/a"));
        assert_eq!(href("https:\\\\example.com\\a").as_deref(), Some("https://example.com/a"));
        assert_eq!(href("https://exa\tmple.com/").as_deref(), Some("https://example.com/"));
    }

    #[test]
    fn what_the_url_class_refuses_is_refused() {
        for bad in ["", "example.com", "http://", "http://[::1", "http://a:99999/", "http://a:b/", "http://a b/", "1http://a/", "http://@/"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn hosts_read_the_way_the_cli_reads_them() {
        let host = |value: &str| parse(value).map(|url| url.host);
        assert_eq!(host("http://127.0.0.1@evil.example/").as_deref(), Some("evil.example"));
        assert_eq!(host("http://evil.example\\@127.0.0.1/").as_deref(), Some("evil.example"));
        assert_eq!(host("http://127.0.0.1.evil.example/").as_deref(), Some("127.0.0.1.evil.example"));
        assert_eq!(host("http://localhost:9/").as_deref(), Some("localhost"));
        assert_eq!(host("http://[0:0:0:0:0:0:0:1]:9/").as_deref(), Some("[::1]"));
        assert_eq!(host("http://127.1/").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("http://0x7f.0.0.1/").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("http://2130706433/").as_deref(), Some("127.0.0.1"));
        assert_eq!(host("http://256.0.0.1/"), None);
    }
}
