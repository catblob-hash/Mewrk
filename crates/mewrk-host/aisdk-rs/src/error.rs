//! Failures of one step and their classification (`main.ts::classify`).
//!
//! The host's retry loop relies solely on [`StepError::kind`]: `transient` retries with backoff and
//! `permanent` fails at once. The error variants keep what the SDK's error classes carried, so the same
//! duck-typed decisions can be made in one place.

use crate::jobj;
use crate::json::Json;

/// `AI_APICallError`: the request reached an upstream that refused it, or never connected.
#[derive(Clone, Debug)]
pub struct ApiCallError {
    pub message: String,
    pub url: String,
    pub status: Option<u16>,
    pub response_headers: Vec<(String, String)>,
    pub response_body: Option<String>,
    pub is_retryable: bool,
    pub data: Option<Json>,
}

impl ApiCallError {
    /// `isRetryable` as the class defaults it from the status.
    pub fn default_retryable(status: Option<u16>) -> bool {
        matches!(status, Some(code) if code == 408 || code == 409 || code == 429 || code >= 500)
    }
}

/// `AI_StreamProviderError`: an error the upstream reported after the stream started.
#[derive(Clone, Debug)]
pub struct StreamProviderError {
    pub message: String,
    pub type_: Option<String>,
    pub code: Option<Json>,
    pub status: Option<u16>,
    pub is_retryable: bool,
    pub data: Option<Json>,
}

#[derive(Clone, Debug)]
pub enum AiError {
    /// The step's cancel fired (`AbortError`).
    Abort,
    Api(Box<ApiCallError>),
    StreamProvider(Box<StreamProviderError>),
    /// A transport failure outside the request's own handling, carrying the code undici would give it
    /// (`cause.code`), read on the way through `retryableCauseCode`.
    Network { code: &'static str, message: String },
    /// Everything else: a thrown `Error` with no retry semantics, a validation failure.
    Other(String),
}

impl AiError {
    pub fn other(message: impl Into<String>) -> Self {
        AiError::Other(message.into())
    }

    pub fn api(api: ApiCallError) -> Self {
        AiError::Api(Box::new(api))
    }

    /// `TypeValidationError`: a chunk that fails the SDK's schema for it.
    pub fn validation(value: &Json, detail: &str) -> Self {
        AiError::Other(format!(
            "Type validation failed: Value: {}.\nError message: {detail}",
            value.to_text()
        ))
    }

    pub fn message(&self) -> String {
        match self {
            AiError::Abort => "This operation was aborted".to_owned(),
            AiError::Api(e) => e.message.clone(),
            AiError::StreamProvider(e) => e.message.clone(),
            AiError::Network { message, .. } => message.clone(),
            AiError::Other(message) => message.clone(),
        }
    }

    /// `error.toString()` as `getErrorMessage` renders a thrown error, with its class's prefix.
    pub fn display_for_tool_error(&self) -> String {
        match self {
            AiError::Abort => "AbortError: This operation was aborted".to_owned(),
            AiError::Api(e) => format!("AI_APICallError: {}", e.message),
            AiError::StreamProvider(e) => format!("AI_StreamProviderError: {}", e.message),
            AiError::Network { message, .. } => format!("TypeError: {message}"),
            AiError::Other(message) => message.clone(),
        }
    }
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for AiError {}

/// What the host's error frame carries.
#[derive(Clone, Debug, PartialEq)]
pub struct StepError {
    pub kind: &'static str,
    pub message: String,
    pub status: Option<u16>,
    pub retry_after_ms: Option<u64>,
}

impl StepError {
    pub fn to_json(&self) -> Json {
        jobj! {
            "kind": self.kind,
            "message": self.message.as_str(),
            "status"?: self.status.map(|s| s as u64),
            "retryAfterMs"?: self.retry_after_ms,
        }
    }
}

/// Retry-After lower bound, in non-negative integer milliseconds.
pub fn retry_after_ms(headers: &[(String, String)]) -> Option<u64> {
    let raw = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))?
        .1
        .trim();
    if raw.is_empty() {
        return None;
    }
    // Do not let a calendar parse read an invalid numeric hint as a year.
    let numeric = !raw.is_empty()
        && raw.bytes().next().is_some_and(|b| b.is_ascii_digit())
        && raw.split_once('.').map_or(raw.bytes().all(|b| b.is_ascii_digit()), |(whole, frac)| {
            whole.bytes().all(|b| b.is_ascii_digit()) && !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit())
        });
    let value = if numeric {
        raw.parse::<f64>().ok()? * 1000.0
    } else if raw.bytes().any(|b| b.is_ascii_alphabetic()) {
        http_date_ms(raw)? - now_ms() as f64
    } else {
        return None;
    };
    if !value.is_finite() || value > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(value.ceil().max(0.0) as u64)
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Milliseconds since the epoch of an RFC 7231 date (`Sun, 06 Nov 1994 08:49:37 GMT`) or the bare form
/// `Date.parse` also takes for a server that drops the weekday. `None` for anything else.
fn http_date_ms(raw: &str) -> Option<f64> {
    let text = raw.split_once(", ").map_or(raw, |(_, rest)| rest).trim();
    let mut parts = text.split_ascii_whitespace();
    let (first, second) = (parts.next()?, parts.next()?);
    // `06 Nov 1994 …` or `Nov 06 1994 …`.
    let (day, month) = match first.parse::<u32>() {
        Ok(day) => (day, second),
        Err(_) => (second.parse().ok()?, first),
    };
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|name| month.len() >= 3 && month[..3].eq_ignore_ascii_case(name))? as u32
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next().unwrap_or("00:00:00").split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next().unwrap_or("0").parse().ok()?;
    let second: i64 = clock.next().unwrap_or("0").parse().ok()?;
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + hour * 3600 + minute * 60 + second) * 1000) as f64)
}

/// Retryable stream `code` and `type` variants: rate limits, overload, server errors and timeouts. Not
/// `invalid_*`, `*_not_found` or auth failures.
fn stream_facet_is_retryable(facets: &str) -> bool {
    let text = facets.to_ascii_lowercase();
    [
        "overload",
        "server_error",
        "service_unavailable",
        "timeout",
        "temporar",
        "too_many",
    ]
    .iter()
    .any(|needle| text.contains(needle))
        || contains_loose(&text, "rate", "limit")
        || contains_loose(&text, "try", "again")
}

/// `first.?second`: the two words with at most one character between them.
fn contains_loose(text: &str, first: &str, second: &str) -> bool {
    text.match_indices(first).any(|(at, _)| {
        let rest = &text[at + first.len()..];
        rest.starts_with(second)
            || rest.char_indices().nth(1).is_some_and(|(i, _)| rest[i..].starts_with(second))
    })
}

pub fn classify(error: &AiError) -> StepError {
    match error {
        AiError::Abort => StepError {
            kind: "cancelled",
            message: "请求已取消".to_owned(),
            status: None,
            retry_after_ms: None,
        },
        AiError::StreamProvider(stream) => {
            // Stream errors arrive after HTTP 200, so retryability cannot come from a status code. The
            // known transient code and type forms are transient; validation and auth stay permanent.
            if !stream.is_retryable {
                let mut facets = Vec::new();
                if let Some(Json::Str(code)) = &stream.code {
                    facets.push(code.as_str());
                }
                if let Some(type_) = &stream.type_ {
                    facets.push(type_.as_str());
                }
                if stream_facet_is_retryable(&facets.join(" ")) {
                    return StepError {
                        kind: "transient",
                        message: stream.message.clone(),
                        status: stream.status,
                        retry_after_ms: None,
                    };
                }
            }
            StepError {
                kind: if stream.is_retryable { "transient" } else { "permanent" },
                message: stream.message.clone(),
                status: stream.status,
                retry_after_ms: None,
            }
        }
        AiError::Api(api) => StepError {
            kind: if api.is_retryable { "transient" } else { "permanent" },
            message: api.message.clone(),
            status: api.status,
            retry_after_ms: retry_after_ms(&api.response_headers),
        },
        AiError::Network { code, message } => StepError {
            kind: "transient",
            message: format!("{code}: {message}"),
            status: None,
            retry_after_ms: None,
        },
        AiError::Other(message) => StepError {
            kind: "permanent",
            message: message.clone(),
            status: None,
            retry_after_ms: None,
        },
    }
}

// ---------------------------------------------------------------- Secrets (`error-redaction.ts`)

fn is_secret_header(name: &str) -> bool {
    ["authorization", "proxy-authorization", "x-api-key", "api-key", "x-goog-api-key"]
        .iter()
        .any(|known| name.eq_ignore_ascii_case(known))
}

/// Request-local credentials only; successful model and tool content is never rewritten.
pub fn secrets_of(request: &Json) -> Vec<String> {
    let mut values: Vec<String> = vec![request.str_at("apiKey").unwrap_or("").to_owned()];
    if let Some(headers) = request.get("headers").and_then(Json::as_obj) {
        for (name, value) in headers.iter() {
            let Some(value) = value.as_str() else { continue };
            if is_secret_header(name) {
                values.push(value.to_owned());
                let lower = value.to_ascii_lowercase();
                if lower.starts_with("bearer") || lower.starts_with("basic") {
                    if let Some((scheme, rest)) = value.split_once(char::is_whitespace) {
                        if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("basic") {
                            values.push(rest.trim_start().to_owned());
                        }
                    }
                }
            }
        }
    }
    // The claude-agent family carries its only credential, a local test stub's key, inside `agent.env`,
    // and a stub that echoes it in a 401 would otherwise reach the host verbatim.
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if let Some(value) = request.path(&["agent", "env", name]).and_then(Json::as_str) {
            values.push(value.to_owned());
        }
    }
    let mut out: Vec<String> = Vec::new();
    for value in values {
        for candidate in [value.as_str(), value.trim()] {
            if !candidate.is_empty() && !out.iter().any(|seen| seen == candidate) {
                out.push(candidate.to_owned());
            }
        }
    }
    // Longest first, so a key that contains another is redacted whole.
    out.sort_by_key(|value| std::cmp::Reverse(value.len()));
    out
}

/// One pass, so a short key overlapping the replacement marker cannot redact the marker.
pub fn redact_secrets(text: &str, secrets: &[String]) -> String {
    if secrets.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    'outer: while at < text.len() {
        for secret in secrets {
            if text[at..].starts_with(secret.as_str()) {
                out.push_str("[redacted]");
                at += secret.len();
                continue 'outer;
            }
        }
        let c = text[at..].chars().next().unwrap_or('\0');
        out.push(c);
        at += c.len_utf8();
    }
    out
}

pub fn redact_error(mut error: StepError, secrets: &[String]) -> StepError {
    error.message = redact_secrets(&error.message, secrets);
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: Option<u16>, headers: &[(&str, &str)]) -> AiError {
        AiError::api(ApiCallError {
            message: "boom".into(),
            url: "http://x".into(),
            status,
            response_headers: headers.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect(),
            response_body: None,
            is_retryable: ApiCallError::default_retryable(status),
            data: None,
        })
    }

    #[test]
    fn statuses_classify_as_the_error_class_defaults() {
        assert_eq!(classify(&api(Some(429), &[])).kind, "transient");
        assert_eq!(classify(&api(Some(529), &[])).kind, "transient");
        assert_eq!(classify(&api(Some(400), &[])).kind, "permanent");
        assert_eq!(classify(&api(Some(401), &[])).kind, "permanent");
        assert_eq!(classify(&AiError::Abort).kind, "cancelled");
    }

    #[test]
    fn retry_after_reads_seconds_and_dates_but_not_numeric_junk() {
        assert_eq!(classify(&api(Some(429), &[("Retry-After", "7")])).retry_after_ms, Some(7000));
        assert_eq!(classify(&api(Some(429), &[("retry-after", "1.5")])).retry_after_ms, Some(1500));
        assert_eq!(classify(&api(Some(429), &[("retry-after", "0")])).retry_after_ms, Some(0));
        assert_eq!(classify(&api(Some(429), &[("retry-after", "-1")])).retry_after_ms, None);
        assert_eq!(classify(&api(Some(429), &[("retry-after", "2030")])).retry_after_ms, Some(2_030_000));
        // A date in the past is a zero wait, a garbage word nothing.
        assert_eq!(classify(&api(Some(429), &[("retry-after", "Sun, 06 Nov 1994 08:49:37 GMT")])).retry_after_ms, Some(0));
        assert_eq!(classify(&api(Some(429), &[("retry-after", "soon")])).retry_after_ms, None);
    }

    #[test]
    fn known_dates_parse_to_the_right_instant() {
        assert_eq!(http_date_ms("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777_000.0));
    }

    #[test]
    fn stream_errors_read_their_code_and_type() {
        let stream = |type_: &str, code: Option<&str>| {
            AiError::StreamProvider(Box::new(StreamProviderError {
                message: "m".into(),
                type_: Some(type_.into()),
                code: code.map(Json::from),
                status: None,
                is_retryable: false,
                data: None,
            }))
        };
        assert_eq!(classify(&stream("rate_limit_exceeded", None)).kind, "transient");
        assert_eq!(classify(&stream("x", Some("ratelimit"))).kind, "transient");
        assert_eq!(classify(&stream("overloaded_error", None)).kind, "transient");
        assert_eq!(classify(&stream("invalid_request_error", None)).kind, "permanent");
        assert_eq!(classify(&stream("model_not_found", None)).kind, "permanent");
        assert_eq!(classify(&stream("x", Some("please_try_again"))).kind, "transient");
    }

    #[test]
    fn secrets_are_redacted_longest_first_in_one_pass() {
        let request = jobj! {
            "apiKey": " sk-abc ",
            "headers": jobj! { "Authorization": "Bearer tok123", "X-Other": "keep" },
            "agent": jobj! { "env": jobj! { "ANTHROPIC_API_KEY": "stub-key" } },
        };
        let secrets = secrets_of(&request);
        let out = redact_secrets("a sk-abc b Bearer tok123 c tok123 d keep e stub-key", &secrets);
        // The key with its surrounding spaces is the longest secret, and goes first.
        assert_eq!(out, "a[redacted]b [redacted] c [redacted] d keep e [redacted]");
    }
}
