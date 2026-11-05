//! `amazon-bedrock-sigv4-fetch.ts` and the `AwsV4Signer` it calls (`aws4fetch`): the two ways a Bedrock request
//! authenticates, as `fetch` wrappers.
//!
//! The signer canonicalizes what `new URL()` and `new Headers()` would have handed it, so both are ported as far as
//! a request URL can reach them.

use graviola::hashing::hmac::Hmac;
use graviola::hashing::{Hash, Sha256};

use crate::api::{RUNTIME_USER_AGENT, VERSION_BEDROCK, combine_headers, with_user_agent_suffix};
use crate::error::{AiError, now_ms};
use crate::http::{BoxFuture, DynFetch, Fetch, Headers, HttpRequest, HttpResponse};

/// `AmazonBedrockCredentials`.
#[derive(Clone)]
pub struct Credentials {
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// `getCredentials`, resolved per request as upstream does: a missing setting fails the call, not the provider.
pub type CredentialSource = Box<dyn Fn() -> Result<Credentials, AiError> + Send + Sync>;
/// The signing time, as `AwsV4Signer`'s `datetime`.
pub type Clock = Box<dyn Fn() -> String + Send + Sync>;

/// `UNSIGNABLE_HEADERS`.
const UNSIGNABLE_HEADERS: [&str; 9] =
    ["authorization", "content-type", "content-length", "user-agent", "presigned-expires", "expect", "x-amzn-trace-id", "range", "connection"];

/// One `new AwsV4Signer({...})`.
pub struct SignInput<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a Headers,
    pub body: &'a str,
    pub credentials: &'a Credentials,
    pub service: &'a str,
    /// `20150830T123600Z`.
    pub datetime: &'a str,
}

/// What `signer.sign()` leaves behind.
pub struct Signed {
    /// The input's headers without `host`, with `x-amz-date`, `x-amz-security-token` and `authorization` added.
    pub headers: Headers,
    pub canonical: String,
    pub signature: String,
}

fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new(key);
    mac.update(message);
    mac.finish().as_ref().to_vec()
}

fn sha256_hex(message: &[u8]) -> String {
    hex(Sha256::hash(message).as_ref())
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

/// `kSigning`, the chain under `signature()`.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let date_key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    hmac(&service_key, b"aws4_request")
}

/// `new AwsV4Signer(...)` then `.sign()`, for header signing (`signQuery` is never used here).
pub fn sign(input: &SignInput<'_>) -> Result<Signed, AiError> {
    let url = ParsedUrl::parse(input.url).ok_or_else(|| AiError::other("Invalid URL"))?;
    let credentials = input.credentials;

    // `new Headers(entries)`: values lose their surrounding whitespace.
    let mut headers = Headers::new();
    for (name, value) in &input.headers.0 {
        headers.set(name, value.trim_matches([' ', '\t', '\r', '\n']));
    }
    headers.delete("host");
    headers.set("x-amz-date", input.datetime);
    if let Some(token) = credentials.session_token.as_deref().filter(|token| !token.is_empty()) {
        headers.set("x-amz-security-token", token);
    }

    let mut signable: Vec<&str> = vec!["host"];
    signable.extend(headers.0.iter().map(|(name, _)| name.as_str()).filter(|name| !UNSIGNABLE_HEADERS.contains(name)));
    signable.sort_unstable();
    let signed_headers = signable.join(";");
    let canonical_headers = signable
        .iter()
        .map(|name| {
            let value = if *name == "host" { url.host.clone() } else { collapse_whitespace(headers.get(name).unwrap_or("")) };
            format!("{name}:{value}")
        })
        .collect::<Vec<_>>()
        .join("\n");

    let date = input.datetime.get(..8).unwrap_or(input.datetime);
    let credential_string = format!("{date}/{}/{}/aws4_request", credentials.region, input.service);

    let collapsed = collapse_slashes(&url.path);
    let encoded_path = encode_rfc3986(&encode_uri_component(&collapsed).replace("%2F", "/"));
    let encoded_search = canonical_query(url.query.as_deref());
    let body_hash = match headers.get("x-amz-content-sha256").filter(|value| !value.is_empty()) {
        Some(preset) => preset.to_owned(),
        None => sha256_hex(input.body.as_bytes()),
    };
    let canonical = [
        input.method.to_ascii_uppercase().as_str(),
        encoded_path.as_str(),
        encoded_search.as_str(),
        format!("{canonical_headers}\n").as_str(),
        signed_headers.as_str(),
        body_hash.as_str(),
    ]
    .join("\n");

    let string_to_sign = format!("AWS4-HMAC-SHA256\n{}\n{credential_string}\n{}", input.datetime, sha256_hex(canonical.as_bytes()));
    let key = signing_key(&credentials.secret_access_key, date, &credentials.region, input.service);
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
    headers.set(
        "authorization",
        &format!("AWS4-HMAC-SHA256 Credential={}/{credential_string}, SignedHeaders={signed_headers}, Signature={signature}", credentials.access_key_id),
    );
    Ok(Signed { headers, canonical, signature })
}

/// `new Date().toISOString().replace(/[:-]|\.\d{3}/g, '')`.
pub fn amz_datetime(epoch_ms: u64) -> String {
    let seconds = epoch_ms / 1000;
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z", rest / 3600, rest / 60 % 60, rest % 60)
}

// ------------------------------------------------------------------ `fetch` wrappers

fn bedrock_user_agent() -> String {
    format!("ai-sdk/amazon-bedrock/{VERSION_BEDROCK}")
}

/// `createSigV4FetchFunction`.
pub struct SigV4Fetch {
    pub credentials: CredentialSource,
    pub clock: Clock,
    pub inner: DynFetch,
}

impl SigV4Fetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        // The user agent's suffix goes on a second time here, on top of the one `postJsonToApi` gave it: upstream's
        // provider header and this wrapper each add it.
        let headers = with_user_agent_suffix(std::mem::take(&mut request.headers), &[&bedrock_user_agent(), RUNTIME_USER_AGENT]);
        let body = request.body.as_deref().unwrap_or("");
        if !request.method.eq_ignore_ascii_case("POST") || body.is_empty() {
            request.headers = headers;
            return self.inner.fetch(request).await;
        }
        let credentials = (self.credentials)()?;
        let datetime = (self.clock)();
        let signed = sign(&SignInput {
            method: "POST",
            url: &request.url,
            headers: &headers,
            body,
            credentials: &credentials,
            service: "bedrock",
            datetime: &datetime,
        })?;
        request.headers = combine_headers(&headers, &signed.headers);
        self.inner.fetch(request).await
    }
}

impl Fetch for SigV4Fetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// The wall clock `AwsV4Signer` reads when it is given no `datetime`.
pub fn system_clock() -> Clock {
    Box::new(|| amz_datetime(now_ms()))
}

/// `createApiKeyFetchFunction`.
pub struct ApiKeyFetch {
    pub api_key: String,
    pub inner: DynFetch,
}

impl ApiKeyFetch {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        let mut headers = with_user_agent_suffix(std::mem::take(&mut request.headers), &[&bedrock_user_agent(), RUNTIME_USER_AGENT]);
        // `combineHeaders(lowercased, { Authorization })` keeps both spellings as separate keys of a plain object,
        // and `fetch`'s `new Headers(...)` then appends them: a caller's `authorization` survives, comma-joined.
        let bearer = format!("Bearer {}", self.api_key);
        let value = match headers.get("authorization") {
            Some(existing) => format!("{existing}, {bearer}"),
            None => bearer,
        };
        headers.set("authorization", &value);
        request.headers = headers;
        self.inner.fetch(request).await
    }
}

impl Fetch for ApiKeyFetch {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

// ------------------------------------------------------------------ `new URL()`, as far as signing reads it

struct ParsedUrl {
    /// `url.host`: lower case, without a default port.
    host: String,
    /// `url.pathname`.
    path: String,
    /// `url.search` without its `?`.
    query: Option<String>,
}

impl ParsedUrl {
    fn parse(raw: &str) -> Option<Self> {
        // Leading and trailing C0 controls and spaces go, and tabs and newlines anywhere.
        let cleaned: String = raw.trim_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
        let (scheme, rest) = cleaned.split_once(':')?;
        let mut chars = scheme.chars();
        if !chars.next()?.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return None;
        }
        let scheme = scheme.to_ascii_lowercase();
        let rest = rest.trim_start_matches(['/', '\\']);
        let authority_end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(authority_end);
        let host_and_port = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        if host_and_port.is_empty() {
            return None;
        }
        let tail = tail.split_once('#').map_or(tail, |(before, _)| before);
        let (path, query) = match tail.split_once('?') {
            Some((path, query)) => (path, Some(query.to_owned())),
            None => (tail, None),
        };
        Some(Self { host: normalize_host(&scheme, host_and_port), path: normalize_path(path), query })
    }
}

/// `new URL(url).host`.
pub fn url_host(url: &str) -> Option<String> {
    ParsedUrl::parse(url).map(|parsed| parsed.host)
}

fn normalize_host(scheme: &str, host_and_port: &str) -> String {
    let lowered = host_and_port.to_ascii_lowercase();
    // A colon inside an IPv6 literal's brackets is not a port.
    let split = lowered.rfind(':').filter(|at| !lowered[*at..].contains(']'));
    let Some(at) = split else { return lowered };
    let (host, port) = (&lowered[..at], &lowered[at + 1..]);
    let default = match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    };
    match port.parse::<u32>() {
        Ok(number) if Some(number) == default => host.to_owned(),
        Ok(number) => format!("{host}:{number}"),
        Err(_) => host.to_owned(),
    }
}

/// The URL standard's path state for a special URL: `\` is `/`, dot segments resolve (`%2e` counts as a dot), and
/// the path percent-encode set is encoded.
fn normalize_path(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut segments: Vec<String> = Vec::new();
    let parts: Vec<&str> = unified.strip_prefix('/').unwrap_or(&unified).split('/').collect();
    let count = parts.len();
    for (at, part) in parts.into_iter().enumerate() {
        let last = at + 1 == count;
        let lowered = part.to_ascii_lowercase();
        if matches!(lowered.as_str(), ".." | ".%2e" | "%2e." | "%2e%2e") {
            segments.pop();
            if last {
                segments.push(String::new());
            }
        } else if matches!(lowered.as_str(), "." | "%2e") {
            if last {
                segments.push(String::new());
            }
        } else {
            segments.push(encode_path_segment(part));
        }
    }
    format!("/{}", segments.join("/"))
}

fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for c in segment.chars() {
        let encode = c <= '\u{1f}' || c >= '\u{7f}' || matches!(c, ' ' | '"' | '#' | '<' | '>' | '?' | '`' | '{' | '}');
        if encode {
            let mut buffer = [0u8; 4];
            for byte in c.encode_utf8(&mut buffer).as_bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `url.pathname.replace(/\/+/g, '/')`.
fn collapse_slashes(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if c != '/' || !out.ends_with('/') {
            out.push(c);
        }
    }
    out
}

/// `.replace(/\s+/g, ' ')`, with JavaScript's `\s`.
fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if super::is_js_whitespace(c) {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

/// `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `encodeRfc3986`.
fn encode_rfc3986(encoded: &str) -> String {
    let mut out = String::with_capacity(encoded.len());
    for c in encoded.chars() {
        match c {
            '!' | '\'' | '(' | ')' | '*' => out.push_str(&format!("%{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

fn percent_decode_form(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() && bytes[at + 1].is_ascii_hexdigit() && bytes[at + 2].is_ascii_hexdigit() => {
                let high = (bytes[at + 1] as char).to_digit(16).unwrap_or(0) as u8;
                let low = (bytes[at + 2] as char).to_digit(16).unwrap_or(0) as u8;
                out.push(high * 16 + low);
                at += 2;
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `encodedSearch`: `url.searchParams`, encoded again and sorted, without empty names.
fn canonical_query(query: Option<&str>) -> String {
    let Some(query) = query else { return String::new() };
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode_form(name), percent_decode_form(value))
        })
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| (encode_rfc3986(&encode_uri_component(&name)), encode_rfc3986(&encode_uri_component(&value))))
        .collect();
    pairs.sort();
    pairs.into_iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn credentials(region: &str, token: Option<&str>) -> Credentials {
        Credentials {
            region: region.to_owned(),
            access_key_id: ACCESS_KEY.to_owned(),
            secret_access_key: SECRET.to_owned(),
            session_token: token.map(str::to_owned),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run(method: &str, url: &str, headers: &[(&str, &str)], body: &str, service: &str, region: &str, token: Option<&str>, datetime: &str) -> Signed {
        sign(&SignInput {
            method,
            url,
            headers: &Headers::from_pairs(headers.iter().copied()),
            body,
            credentials: &credentials(region, token),
            service,
            datetime,
        })
        .unwrap()
    }

    /// The AWS documentation's derivation example ("Examples of how to derive a signing key").
    #[test]
    fn signing_key_of_the_published_example() {
        assert_eq!(hex(&signing_key(SECRET, "20120215", "us-east-1", "iam")), "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d");
        assert_eq!(hex(&signing_key(SECRET, "20150830", "us-east-1", "iam")), "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9");
    }

    /// The AWS SigV4 test suite's `get-vanilla` and `post-vanilla`.
    #[test]
    fn published_vanilla_vectors() {
        let get = run("GET", "https://example.amazonaws.com/", &[], "", "service", "us-east-1", None, "20150830T123600Z");
        assert_eq!(get.canonical, "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(
            get.headers.get("authorization"),
            Some("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31")
        );
        let post = run("POST", "https://example.amazonaws.com/", &[], "", "service", "us-east-1", None, "20150830T123600Z");
        assert_eq!(
            post.headers.get("authorization"),
            Some("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b")
        );
        assert_eq!(post.headers.get("x-amz-date"), Some("20150830T123600Z"));
    }

    /// Everything below came out of `aws4fetch`'s `AwsV4Signer` under node, given the same inputs.
    #[test]
    fn aws4fetch_vector_bedrock_body_is_hashed_as_utf8() {
        let signed = run(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-sonnet-4-5-20250929-v1%3A0/converse-stream",
            &[("content-type", "application/json"), ("user-agent", "ai-sdk/amazon-bedrock/5.0.66")],
            "{\"system\":[],\"messages\":[{\"role\":\"user\",\"content\":[{\"text\":\"h\u{e9}llo \u{4f60}\u{597d}\"}]}]}",
            "bedrock",
            "us-east-1",
            None,
            "20260102T030405Z",
        );
        assert_eq!(signed.canonical, "POST\n/model/anthropic.claude-sonnet-4-5-20250929-v1%253A0/converse-stream\n\nhost:bedrock-runtime.us-east-1.amazonaws.com\nx-amz-date:20260102T030405Z\n\nhost;x-amz-date\nfd76e2cfe1953868a3b271c2d990019366e5191486cd9cf58bc334f41a20ca20");
        assert_eq!(signed.signature, "3647f403cbab32ee876a4a5c1afc623600e300542842901dbb96048391150e21");
        assert_eq!(signed.headers.get("content-type"), Some("application/json"));
    }

    #[test]
    fn aws4fetch_vector_session_token() {
        let signed = run(
            "POST",
            "https://bedrock-runtime.eu-west-1.amazonaws.com/model/m/converse-stream",
            &[("content-type", "application/json")],
            "{}",
            "bedrock",
            "eu-west-1",
            Some("TOKEN/with+chars=="),
            "20260102T030405Z",
        );
        assert_eq!(signed.canonical, "POST\n/model/m/converse-stream\n\nhost:bedrock-runtime.eu-west-1.amazonaws.com\nx-amz-date:20260102T030405Z\nx-amz-security-token:TOKEN/with+chars==\n\nhost;x-amz-date;x-amz-security-token\n44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
        assert_eq!(signed.signature, "a17422d787b044d780d8e8ee114b32d59400e773f9b974a4142fd5db438c7b56");
        assert_eq!(signed.headers.get("x-amz-security-token"), Some("TOKEN/with+chars=="));
    }

    #[test]
    fn aws4fetch_vector_custom_headers_and_path_normalization() {
        let signed = run(
            "POST",
            "http://LOCALHOST:8080//a//b/../c/model/x%2Fy!z*(1)/converse-stream",
            &[
                ("X-Extra", "  spaced   out  value "),
                ("x-amz-meta", "a\t\tb"),
                ("Authorization", "nope"),
                ("Range", "bytes=0-1"),
                ("Expect", "100-continue"),
                ("X-Amzn-Trace-Id", "skip"),
                ("Connection", "keep-alive"),
                ("Content-Length", "1"),
                ("presigned-expires", "z"),
                ("Host", "other.example"),
            ],
            "x",
            "bedrock",
            "us-west-2",
            None,
            "20260102T030405Z",
        );
        assert_eq!(signed.canonical, "POST\n/a/c/model/x%252Fy%21z%2A%281%29/converse-stream\n\nhost:localhost:8080\nx-amz-date:20260102T030405Z\nx-amz-meta:a b\nx-extra:spaced out value\n\nhost;x-amz-date;x-amz-meta;x-extra\n2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881");
        assert_eq!(signed.signature, "fed931ee95039e385bcb5e0a95af693011ae0dfe402bcf778bcde582dae19962");
        // The value on the wire keeps its inner runs; only the canonical form collapses them.
        assert_eq!(signed.headers.get("x-extra"), Some("spaced   out  value"));
        assert_eq!(signed.headers.get("host"), None);
    }

    #[test]
    fn aws4fetch_vector_default_ports_query_and_fragment() {
        let port = |url: &str| run("POST", url, &[], "x", "bedrock", "r", None, "20260102T030405Z").signature;
        assert_eq!(port("https://h.example:443/p"), "5a1fbcb8700eba170a5ac12007c57e5b695c92bbea5b56282c2fce411a62be7d");
        assert_eq!(port("http://h.example:80/p"), "5a1fbcb8700eba170a5ac12007c57e5b695c92bbea5b56282c2fce411a62be7d");
        assert_eq!(port("https://user:pw@h.example/p#frag"), "5a1fbcb8700eba170a5ac12007c57e5b695c92bbea5b56282c2fce411a62be7d");
        let query = run("POST", "https://h.example/p?b=2&a=1&a=0&c=x+y&d=%E4%BD%A0&e&=skip&f=a%2Fb!", &[], "x", "bedrock", "r", None, "20260102T030405Z");
        assert_eq!(query.canonical.split('\n').nth(2), Some("a=0&a=1&b=2&c=x%20y&d=%E4%BD%A0&e=&f=a%2Fb%21"));
        assert_eq!(query.signature, "3cfde5fb6967aa252b554f122991dc94ddad960e431ca76bd0efd306497a3f62");
    }

    #[test]
    fn aws4fetch_vector_unicode_path_dots_and_preset_hash() {
        let unicode = run("POST", "https://h.example/\u{4f60}\u{597d} x/a\"b<c>/d`e{f}^g|h", &[], "x", "bedrock", "r", None, "20260102T030405Z");
        assert_eq!(unicode.canonical.split('\n').nth(1), Some("/%25E4%25BD%25A0%25E5%25A5%25BD%2520x/a%2522b%253Cc%253E/d%2560e%257Bf%257D%5Eg%7Ch"));
        assert_eq!(unicode.signature, "f00a31310fb0dd0245f04aba6879cb87d8f46b72bbb00945def581e9727185d8");
        let dots = run("POST", "https://h.example/a/./b/%2e%2E/c/..", &[], "x", "bedrock", "r", None, "20260102T030405Z");
        assert_eq!(dots.canonical.split('\n').nth(1), Some("/a/"));
        assert_eq!(dots.signature, "231168b9c3a22904a1df3c4e4f0672bdcaa8b613e7c362bd9c35c88a7cbab194");
        let preset = run("POST", "https://h.example/p", &[("X-Amz-Content-Sha256", "preset")], "x", "bedrock", "r", None, "20260102T030405Z");
        assert_eq!(preset.canonical, "POST\n/p\n\nhost:h.example\nx-amz-content-sha256:preset\nx-amz-date:20260102T030405Z\n\nhost;x-amz-content-sha256;x-amz-date\npreset");
        assert_eq!(preset.signature, "079c842d446c33cd0e9ca5ade90044761f145ba2f3b17e8f745e15dd0292303f");
    }

    #[test]
    fn datetime_matches_to_iso_string() {
        // `new Date(1700000000123).toISOString()` is `2023-11-14T22:13:20.123Z`.
        assert_eq!(amz_datetime(1_700_000_000_123), "20231114T221320Z");
        assert_eq!(amz_datetime(0), "19700101T000000Z");
        // A leap day, and the last second of a year.
        assert_eq!(amz_datetime(1_709_164_800_000), "20240229T000000Z");
        assert_eq!(amz_datetime(1_735_689_599_999), "20241231T235959Z");
    }

    #[test]
    fn uri_component_leaves_what_javascript_leaves() {
        assert_eq!(encode_uri_component("us.anthropic.claude-opus-5:0/x y!~*'()_-"), "us.anthropic.claude-opus-5%3A0%2Fx%20y!~*'()_-");
        assert_eq!(encode_uri_component("\u{4f60}"), "%E4%BD%A0");
    }
}
