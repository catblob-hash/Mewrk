//! `postJsonToApi` and the small helpers around it (`@ai-sdk/provider-utils`).

use crate::error::{AiError, ApiCallError};
use crate::http::{Cancel, DynFetch, Headers, HttpRequest, HttpResponse};
use crate::json::Json;

/// Package versions the wire strings carry (`aisdk-service/package-lock.json`).
pub const VERSION_ANTHROPIC: &str = "4.0.44";
pub const VERSION_OPENAI: &str = "4.0.50";
pub const VERSION_AZURE: &str = "4.0.52";
pub const VERSION_OPENAI_COMPATIBLE: &str = "3.0.39";
pub const VERSION_GOOGLE: &str = "4.0.54";
pub const VERSION_VERTEX: &str = "5.0.66";
pub const VERSION_XAI: &str = "4.0.47";
pub const VERSION_BEDROCK: &str = "5.0.66";
const VERSION_PROVIDER_UTILS: &str = "5.0.32";

/// The runtime segment of the user agent. Upstream's is `runtime/node.js/22`; there is no Node here.
pub(crate) const RUNTIME_USER_AGENT: &str = "runtime/rust";

/// The only environment a provider may read.
///
/// The Node sidecar ran with a cleared environment holding these names and nothing else (`process.rs`'s
/// `INHERITED_ENV`), which is what kept unrelated local credentials away from a user-configured relay: an
/// `OPENAI_API_KEY` or `AWS_ACCESS_KEY_ID` in the host's own environment never reached a provider. In-process that
/// has to be a rule rather than a process boundary, so nothing in this crate calls `std::env::var` directly.
pub fn allowed_env(name: &str) -> Option<String> {
    INHERITED_ENV.contains(&name).then(|| std::env::var(name).ok()).flatten()
}

const INHERITED_ENV: &[&str] = &[
    "SystemRoot", "windir", "SystemDrive", "COMSPEC", "PATHEXT", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE",
    "TEMP", "TMP", "TMPDIR", "PATH", "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
    // Not on upstream's list, where it never reached `PluggableAuthClient`, so executables always stayed refused. It
    // holds no credential and only ever says yes when the user set it.
    "GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES",
];

/// Every variable [`allowed_env`] would answer, with its value.
pub fn inherited_env() -> Vec<(String, String)> {
    INHERITED_ENV.iter().filter_map(|name| std::env::var(name).ok().map(|value| ((*name).to_owned(), value))).collect()
}

/// `withUserAgentSuffix`: appended to whatever `user-agent` is there.
pub fn with_user_agent_suffix(mut headers: Headers, parts: &[&str]) -> Headers {
    let current = headers.get("user-agent").unwrap_or("").to_owned();
    let mut joined: Vec<&str> = Vec::new();
    if !current.is_empty() {
        joined.push(&current);
    }
    joined.extend(parts.iter().filter(|part| !part.is_empty()));
    let value = joined.join(" ");
    headers.set("user-agent", &value);
    headers
}

/// `{ ...base, ...overrides }` of two header sets.
pub fn combine_headers(base: &Headers, overrides: &Headers) -> Headers {
    let mut out = base.clone();
    for (k, v) in &overrides.0 {
        out.set(k, v);
    }
    out
}

/// The `headers` of a `StepRequest` as a provider's `options.headers`.
pub fn headers_from_json(headers: Option<&Json>) -> Headers {
    let mut out = Headers::new();
    if let Some(object) = headers.and_then(Json::as_obj) {
        for (k, v) in object.iter() {
            if let Some(v) = v.as_str() {
                out.set(k, v);
            }
        }
    }
    out
}

/// `withoutTrailingSlash`.
pub fn without_trailing_slash(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

/// `generateId`: 16 random characters of the SDK's alphabet.
pub fn generate_id() -> String {
    random_id(16)
}

pub fn random_id(size: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    (0..size).map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char).collect()
}

/// How a failed response becomes the error the step reports.
#[derive(Clone, Copy)]
pub struct ErrorShape {
    /// Reads the message out of a parsed body, `None` when the body does not match the provider's error
    /// schema (the message then falls back to the status text).
    pub message: fn(&Json) -> Option<String>,
    /// `isRetryable` when the provider overrides the class default.
    pub is_retryable: Option<fn(u16, Option<&Json>) -> bool>,
}

/// `createJsonErrorResponseHandler` over a failed response.
pub async fn failed_response(
    url: &str,
    mut response: HttpResponse,
    shape: ErrorShape,
) -> AiError {
    let status = response.status;
    let headers = response.headers.to_vec();
    let body = match response.body.read_text().await {
        Ok(body) => body,
        Err(AiError::Abort) => return AiError::Abort,
        Err(error) => {
            return AiError::api(ApiCallError {
                message: "Failed to process error response".to_owned(),
                url: url.to_owned(),
                status: Some(status),
                response_headers: headers,
                response_body: None,
                is_retryable: ApiCallError::default_retryable(Some(status)),
                data: Some(Json::from(error.message())),
            });
        }
    };
    let parsed = if body.trim().is_empty() { None } else { Json::parse(&body).ok() };
    let message = parsed.as_ref().and_then(shape.message);
    let matched = message.is_some();
    let is_retryable = match shape.is_retryable {
        Some(decide) => decide(status, parsed.as_ref().filter(|_| matched)),
        None => ApiCallError::default_retryable(Some(status)),
    };
    AiError::api(ApiCallError {
        message: message.unwrap_or_else(|| response.status_text.clone()),
        url: url.to_owned(),
        status: Some(status),
        response_headers: headers,
        response_body: Some(body),
        is_retryable,
        data: if matched { parsed } else { None },
    })
}

/// `postJsonToApi`: sends `body`, returning the response when it is `ok` and the mapped error otherwise.
///
/// The caller's `headers` must already carry the provider's `ai-sdk/<name>/<version>` user-agent suffix.
pub async fn post_json(
    fetch: &DynFetch,
    url: &str,
    headers: Headers,
    body: &Json,
    cancel: &Cancel,
    shape: ErrorShape,
) -> Result<HttpResponse, AiError> {
    let mut with_type = Headers::new();
    with_type.set("content-type", "application/json");
    let merged = combine_headers(&with_type, &headers);
    let merged = with_user_agent_suffix(
        merged,
        &[&format!("ai-sdk/provider-utils/{VERSION_PROVIDER_UTILS}"), RUNTIME_USER_AGENT],
    );
    let request = HttpRequest::post(url, merged, body.to_text(), cancel.clone());
    let response = fetch.fetch(request).await?;
    if !response.ok() {
        return Err(failed_response(url, response, shape).await);
    }
    Ok(response)
}
