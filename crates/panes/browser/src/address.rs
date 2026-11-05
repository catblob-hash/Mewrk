//! What the address bar accepts, from upstream's `BrowserPanel.tsx`.

use mewrk_client::t;
use url::Url;

/// `encodeURIComponent`.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `^(?:\[[\da-f:.]+\]|[^\s/?#:@[\]]+):\d+(?:[/?#]|$)`, case-insensitively: a port says it is a host.
/// `example.com:8080` otherwise reads like a scheme followed by an opaque path.
fn looks_like_host_and_port(value: &str) -> bool {
    let (host, rest) = if let Some(after) = value.strip_prefix('[') {
        let Some(end) = after.find(']') else { return false };
        let inner = &after[..end];
        if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
            return false;
        }
        (true, &after[end + 1..])
    } else {
        let end = value.find(|c: char| c.is_whitespace() || "/?#:@[]".contains(c)).unwrap_or(value.len());
        (end > 0, &value[end..])
    };
    if !host {
        return false;
    }
    let Some(port) = rest.strip_prefix(':') else { return false };
    let digits = port.find(|c: char| !c.is_ascii_digit()).unwrap_or(port.len());
    if digits == 0 {
        return false;
    }
    match port[digits..].chars().next() {
        None => true,
        Some(next) => "/?#".contains(next),
    }
}

/// `^[a-z][a-z\d+.-]*:`, case-insensitively.
fn has_scheme(value: &str) -> bool {
    let mut chars = value.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || "+.-".contains(c)) {
            return false;
        }
    }
    false
}

/// A typed address as what the page should load, or why not.
pub fn normalize_browser_address(input: &str) -> Result<String, String> {
    let value = input.trim();
    if value.is_empty() {
        return Err(t("请输入网址或搜索内容", "Enter a URL or search query").to_owned());
    }
    if value.eq_ignore_ascii_case("about:blank") {
        return Ok("about:blank".to_owned());
    }
    let host_and_port = looks_like_host_and_port(value);
    if !host_and_port && has_scheme(value) {
        let url = Url::parse(value).map_err(|error| error.to_string())?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(t("侧栏浏览器只支持 HTTP(S) 地址", "The sidebar browser only supports HTTP(S) addresses").to_owned());
        }
        return Ok(url.to_string());
    }
    let search = format!("https://www.google.com/search?q={}", encode_uri_component(value));
    if value.chars().any(char::is_whitespace) {
        return Ok(search);
    }
    let Ok(parsed) = Url::parse(&format!("http://{value}")) else {
        return Ok(search);
    };
    let hostname = parsed.host_str().unwrap_or_default().to_owned();
    if !host_and_port && !value.contains('.') && hostname != "localhost" && !hostname.starts_with('[') {
        return Ok(search);
    }
    // Dev servers on this machine and the LAN rarely have certificates; everything else gets TLS.
    Ok(format!("{}://{value}", if is_local_or_private_host(&hostname) { "http" } else { "https" }))
}

/// Whether a URL hostname names this machine or a private network.
pub fn is_local_or_private_host(hostname: &str) -> bool {
    let lower = hostname.to_ascii_lowercase();
    let host = lower.strip_suffix('.').unwrap_or(&lower);
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|part| (1..=3).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_digit())) {
        let first: u32 = parts[0].parse().unwrap_or(0);
        let second: u32 = parts[1].parse().unwrap_or(0);
        return first == 127 || first == 10 || (first == 172 && (16..=31).contains(&second)) || (first == 192 && second == 168) || (first == 169 && second == 254);
    }
    if let Some(ipv6) = host.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
        let hex = |index: usize| ipv6.as_bytes().get(index).is_some_and(u8::is_ascii_hexdigit);
        let unique_local = ipv6.starts_with('f') && matches!(ipv6.as_bytes().get(1), Some(b'c' | b'd')) && hex(2) && hex(3) && ipv6.as_bytes().get(4) == Some(&b':');
        let link_local = ipv6.starts_with("fe") && matches!(ipv6.as_bytes().get(2), Some(b'8' | b'9' | b'a' | b'b')) && hex(3) && ipv6.as_bytes().get(4) == Some(&b':');
        return ipv6 == "::1" || unique_local || link_local;
    }
    false
}

/// `host:port` and `pathname+search+hash`, the two halves the address chip prints separately.
pub fn split_browser_address(url: &str) -> Option<(String, String)> {
    if url.is_empty() || url.eq_ignore_ascii_case("about:blank") {
        return None;
    }
    let Ok(parsed) = Url::parse(url) else {
        return Some((url.to_owned(), String::new()));
    };
    let host = match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        (None, _) => String::new(),
    };
    let mut path = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        path.push('?');
        path.push_str(query);
    }
    if let Some(fragment) = parsed.fragment() {
        path.push('#');
        path.push_str(fragment);
    }
    Some((host, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses_without_accepting_active_content_schemes() {
        assert_eq!(normalize_browser_address("example.com").unwrap(), "https://example.com");
        assert_eq!(normalize_browser_address("localhost:3000/path").unwrap(), "http://localhost:3000/path");
        assert_eq!(normalize_browser_address("search terms").unwrap(), "https://www.google.com/search?q=search%20terms");
        assert!(normalize_browser_address("javascript:alert(1)").unwrap_err().contains("HTTP"));
        assert!(normalize_browser_address("  ").is_err());
        assert_eq!(normalize_browser_address("ABOUT:BLANK").unwrap(), "about:blank");
    }

    #[test]
    fn opens_host_port_without_a_scheme_local_and_private_over_http() {
        for (typed, expected) in [
            ("myapp.localhost:3000", "http://myapp.localhost:3000"),
            ("example.com:8080", "https://example.com:8080"),
            ("example.com:8080/docs?x=1", "https://example.com:8080/docs?x=1"),
            ("192.168.1.5:3000", "http://192.168.1.5:3000"),
            ("10.0.0.7", "http://10.0.0.7"),
            ("172.20.1.1:8000", "http://172.20.1.1:8000"),
            ("172.32.1.1", "https://172.32.1.1"),
            ("printer.local", "http://printer.local"),
            ("127.0.0.1:5173", "http://127.0.0.1:5173"),
            ("localhost", "http://localhost"),
            ("[::1]:8080", "http://[::1]:8080"),
            ("8.8.8.8", "https://8.8.8.8"),
            ("http://example.com:8080/", "http://example.com:8080/"),
            ("router", "https://www.google.com/search?q=router"),
        ] {
            assert_eq!(normalize_browser_address(typed).unwrap(), expected, "{typed}");
        }
        assert!(normalize_browser_address("mailto:someone@example.com").unwrap_err().contains("HTTP"));
    }

    #[test]
    fn recognizes_local_and_private_hosts() {
        for host in ["localhost", "a.localhost", "box.local", "127.8.0.1", "10.1.2.3", "172.16.0.1", "172.31.255.1", "192.168.0.1", "169.254.1.1", "[::1]", "[fd00::1]", "[fe80::1]", "LOCALHOST."] {
            assert!(is_local_or_private_host(host), "{host}");
        }
        for host in ["example.com", "local.example.com", "172.15.0.1", "172.32.0.1", "11.0.0.1", "[2001:db8::1]"] {
            assert!(!is_local_or_private_host(host), "{host}");
        }
    }

    #[test]
    fn splits_the_address_chip_into_its_halves() {
        assert_eq!(split_browser_address("about:blank"), None);
        assert_eq!(split_browser_address(""), None);
        assert_eq!(split_browser_address("http://localhost:5173/inbox?tab=1#top"), Some(("localhost:5173".to_owned(), "/inbox?tab=1#top".to_owned())));
        assert_eq!(split_browser_address("not a url"), Some(("not a url".to_owned(), String::new())));
    }
}
