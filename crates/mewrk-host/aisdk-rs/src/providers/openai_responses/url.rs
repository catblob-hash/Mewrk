//! `new URL(url)` and `url.toString()` for `http(s)` URLs, for the few places the SDK sends a URL through them.

/// A URL split the way the WHATWG parser splits an `http(s)` one.
pub(in crate::providers) struct ParsedUrl {
    scheme: String,
    userinfo: Option<String>,
    pub(in crate::providers) host: String,
    port: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

fn percent_encode_into(out: &mut String, text: &str, keep: impl Fn(u8) -> bool) {
    for &byte in text.as_bytes() {
        if keep(byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
}

/// The path percent-encode set: what `new URL` escapes in a path.
fn path_keeps(byte: u8) -> bool {
    byte > 0x20 && byte < 0x7f && !matches!(byte, b'"' | b'#' | b'<' | b'>' | b'?' | b'`' | b'{' | b'}')
}

/// The special-query percent-encode set.
fn query_keeps(byte: u8) -> bool {
    byte > 0x20 && byte < 0x7f && !matches!(byte, b'"' | b'#' | b'<' | b'>' | b'\'')
}

/// The fragment percent-encode set.
fn fragment_keeps(byte: u8) -> bool {
    byte > 0x20 && byte < 0x7f && !matches!(byte, b'"' | b'<' | b'>' | b'`')
}

/// `new URL(url).toString()` for an `http(s)` URL; anything else stays as it is, since the hostname rules below are
/// only those of the special schemes.
pub(super) fn normalized_http_url(url: &str) -> Option<String> {
    parse_url(url).filter(|parsed| matches!(parsed.scheme.as_str(), "http" | "https")).map(|parsed| parsed.href())
}

/// `new URL(url)`, for the shapes a validated base URL can take. `None` is the `TypeError: Invalid URL`.
pub(in crate::providers) fn parse_url(url: &str) -> Option<ParsedUrl> {
    let url = url.trim_matches(|c: char| c <= ' ');
    let (scheme, rest) = url.split_once(':')?;
    let mut scheme_chars = scheme.chars();
    if !scheme_chars.next()?.is_ascii_alphabetic() || !scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let (rest, fragment) = match rest.split_once('#') {
        Some((before, after)) => (before, Some(after.to_owned())),
        None => (rest, None),
    };
    let (rest, query) = match rest.split_once('?') {
        Some((before, after)) => (before, Some(after.to_owned())),
        None => (rest, None),
    };
    // A backslash is a slash up to there, and only up to there.
    let rest = rest.replace('\\', "/");
    let rest = rest.trim_start_matches('/');
    let (authority, path) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let (userinfo, host_port) = match authority.rsplit_once('@') {
        Some((userinfo, host_port)) => (Some(userinfo.to_owned()), host_port),
        None => (None, authority),
    };
    let (host, port) = if host_port.starts_with('[') {
        let end = host_port.find(']')?;
        let (host, after) = host_port.split_at(end + 1);
        (host, after.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port.filter(|p| !p.is_empty()) {
        None => None,
        Some(port) => {
            let number: u16 = port.parse().ok()?;
            let default = match scheme.as_str() {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            };
            (default != Some(number)).then(|| number.to_string())
        }
    };
    Some(ParsedUrl {
        scheme,
        userinfo,
        host: host.to_ascii_lowercase(),
        port,
        path: normalize_path(path),
        query,
        fragment,
    })
}

/// Dot segments resolved, the path percent-encode set escaped, `%` kept as it is.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<String> = Vec::new();
    let raw: Vec<&str> = path.split('/').skip(1).collect();
    for (at, segment) in raw.iter().enumerate() {
        let last = at + 1 == raw.len();
        match segment.to_ascii_lowercase().as_str() {
            "." | "%2e" => {
                if last {
                    segments.push(String::new());
                }
            }
            ".." | ".%2e" | "%2e." | "%2e%2e" => {
                segments.pop();
                if last {
                    segments.push(String::new());
                }
            }
            _ => {
                let mut encoded = String::new();
                percent_encode_into(&mut encoded, segment, path_keeps);
                segments.push(encoded);
            }
        }
    }
    format!("/{}", segments.join("/"))
}

impl ParsedUrl {
    pub(in crate::providers) fn href(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if let Some(userinfo) = &self.userinfo {
            percent_encode_into(&mut out, userinfo, |byte| {
                path_keeps(byte) && !matches!(byte, b'/' | b';' | b'=' | b'@' | b'[' | b'\\' | b']' | b'^' | b'|')
            });
            out.push('@');
        }
        out.push_str(&self.host);
        if let Some(port) = &self.port {
            out.push(':');
            out.push_str(port);
        }
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            percent_encode_into(&mut out, query, query_keeps);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            percent_encode_into(&mut out, fragment, fragment_keeps);
        }
        out
    }

    /// `url.searchParams.set(name, value)`: the whole query is reserialized as form data.
    pub(in crate::providers) fn set_search_param(&mut self, name: &str, value: &str) {
        let mut pairs: Vec<(String, String)> = self
            .query
            .as_deref()
            .unwrap_or("")
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, entry) = pair.split_once('=').unwrap_or((pair, ""));
                (form_decode(key), form_decode(entry))
            })
            .collect();
        match pairs.iter().position(|(key, _)| key == name) {
            Some(at) => {
                pairs[at].1 = value.to_owned();
                let mut seen = false;
                pairs.retain(|(key, _)| {
                    let duplicate = key == name && seen;
                    seen |= key == name;
                    !duplicate
                });
            }
            None => pairs.push((name.to_owned(), value.to_owned())),
        }
        let serialized: Vec<String> = pairs.iter().map(|(key, entry)| format!("{}={}", form_encode(key), form_encode(entry))).collect();
        self.query = Some(serialized.join("&"));
    }
}

fn form_encode(text: &str) -> String {
    let mut out = String::new();
    for &byte in text.as_bytes() {
        match byte {
            b' ' => out.push('+'),
            b'*' | b'-' | b'.' | b'_' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn form_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = (bytes[at] == b'%').then(|| text.get(at + 1..at + 3).and_then(|h| u8::from_str_radix(h, 16).ok())).flatten();
        if let Some(decoded) = hex {
            out.push(decoded);
            at += 3;
            continue;
        }
        out.push(if bytes[at] == b'+' { b' ' } else { bytes[at] });
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each expectation is what `new URL(url).href` gave under Node 22.
    #[test]
    fn hrefs_match_node() {
        let cases = [
            ("https://X.example/a b.png", Some("https://x.example/a%20b.png")),
            ("HTTP://x.example:80", Some("http://x.example/")),
            ("https://x.example:443/", Some("https://x.example/")),
            ("http://x.example:8080/p?q=1#f", Some("http://x.example:8080/p?q=1#f")),
            ("https://x.example", Some("https://x.example/")),
            ("https://x.example/a|b^c[d]\\e", Some("https://x.example/a|b^c[d]/e")),
            ("https://x.example/%7e/%zz/\u{e9}", Some("https://x.example/%7e/%zz/%C3%A9")),
            ("https://x.example/a?b='c d\"e<f>g`h{i}j|k", Some("https://x.example/a?b=%27c%20d%22e%3Cf%3Eg`h{i}j|k")),
            ("https://x.example/./a/../b/.", Some("https://x.example/b/")),
            ("https://x.example/a/%2e%2E/b", Some("https://x.example/b")),
            ("https://user:p w@x.example/", Some("https://user:p%20w@x.example/")),
            ("https://x.example/?", Some("https://x.example/?")),
            ("https://x.example/#", Some("https://x.example/#")),
            ("https://x.example//", Some("https://x.example//")),
            ("http://[::1]:9/x", Some("http://[::1]:9/x")),
            ("https://x.example/a;b=c,d:e@f&g+h$i!j*k(l)m~n", Some("https://x.example/a;b=c,d:e@f&g+h$i!j*k(l)m~n")),
            ("https://x.example/{}`\"<>", Some("https://x.example/%7B%7D%60%22%3C%3E")),
            ("https://x.example/a#b c", Some("https://x.example/a#b%20c")),
            ("https://x.example:0/", Some("https://x.example:0/")),
            ("https://x.example:65536/", None),
            ("https://x.example:abc/", None),
            ("https://1.2.3.4/x", Some("https://1.2.3.4/x")),
            ("https://x.example./a", Some("https://x.example./a")),
        ];
        for (url, expected) in cases {
            assert_eq!(normalized_http_url(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn other_schemes_are_left_alone() {
        assert_eq!(normalized_http_url("s3://Bucket/a"), None);
        assert_eq!(normalized_http_url("nonsense"), None);
    }
}
