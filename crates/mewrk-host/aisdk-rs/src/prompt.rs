//! `standardizePrompt` and `convertToLanguageModelPrompt`: `ModelMessage[]` to `LanguageModelV4Prompt`.

use std::collections::{HashMap, HashSet};

use crate::error::AiError;
use crate::http::{Cancel, HttpRequest, NetFetch};
use crate::jobj;
use crate::json::{Json, Obj};

/// `mergeObjects`: deep merge; arrays and primitives are replaced.
pub fn merge_objects(base: Option<&Json>, overrides: Option<&Json>) -> Option<Json> {
    match (base, overrides) {
        (None, None) => None,
        (None, Some(o)) => Some(o.clone()),
        (Some(b), None) => Some(b.clone()),
        (Some(b), Some(o)) => {
            let (Some(b_obj), Some(o_obj)) = (b.as_obj(), o.as_obj()) else {
                return Some(o.clone());
            };
            let mut out = b_obj.clone();
            for (key, value) in o_obj.iter() {
                if key == "__proto__" || key == "constructor" || key == "prototype" {
                    continue;
                }
                match (out.get(key), value) {
                    (Some(existing @ Json::Obj(_)), Json::Obj(_)) => {
                        let merged = merge_objects(Some(existing), Some(value));
                        out.insert(key, merged.unwrap_or(Json::Null));
                    }
                    _ => out.insert(key, value.clone()),
                }
            }
            Some(Json::Obj(out))
        }
    }
}

/// One `FilePart.data` value in the tagged form providers read.
struct FileData {
    data: Json,
    media_type: Option<String>,
}

fn is_data_like_url(text: &str) -> Option<&str> {
    // `new URL(text)` succeeds on anything with a scheme. Only a scheme is checked: base64 data contains
    // no `:`, and that is the case the distinction exists for.
    let colon = text.find(':')?;
    let scheme = &text[..colon];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'))
        .then_some(scheme)
}

fn url_to_file_data(url: &str) -> Result<FileData, AiError> {
    if url.len() >= 5 && url[..5].eq_ignore_ascii_case("data:") {
        // `splitDataUrl`: comma-separated, media type before the first `;`.
        let mut split = url.split(',');
        let header = split.next().unwrap_or("");
        let content = split.next();
        let media = header.split(';').next().and_then(|h| h.split(':').nth(1));
        return match (media, content) {
            (Some(media), Some(content)) => Ok(FileData {
                data: jobj! { "type": "data", "data": content },
                media_type: Some(media.to_owned()),
            }),
            _ => Err(AiError::other(format!("Invalid data URL format in content {url}"))),
        };
    }
    Ok(FileData { data: jobj! { "type": "url", "url": url }, media_type: None })
}

/// `convertToLanguageModelV4FilePart`.
fn convert_file_data(content: &Json) -> Result<FileData, AiError> {
    if let Some(kind) = content.str_at("type") {
        match kind {
            "data" => {
                if let Some(text) = content.str_at("data") {
                    if text.starts_with("data:") {
                        return Err(AiError::other(
                            "Data URLs are not valid inline data. Pass them as { type: \"url\", url } instead.",
                        ));
                    }
                }
                return Ok(FileData {
                    data: jobj! { "type": "data", "data": content.get("data").cloned().unwrap_or_default() },
                    media_type: None,
                });
            }
            "url" => return url_to_file_data(content.str_at("url").unwrap_or("")),
            "reference" => {
                return Ok(FileData {
                    data: jobj! { "type": "reference", "reference": content.get("reference").cloned().unwrap_or_default() },
                    media_type: None,
                });
            }
            "text" => {
                return Ok(FileData {
                    data: jobj! { "type": "text", "text": content.get("text").cloned().unwrap_or_default() },
                    media_type: None,
                });
            }
            _ => {}
        }
    }
    if let Some(text) = content.as_str() {
        if is_data_like_url(text).is_some() {
            return url_to_file_data(text);
        }
        return Ok(FileData { data: jobj! { "type": "data", "data": text }, media_type: None });
    }
    // A provider reference: `{ openai: "file-…" }`.
    if content.is_obj() {
        return Ok(FileData { data: jobj! { "type": "reference", "reference": content.clone() }, media_type: None });
    }
    Err(AiError::other("Invalid data content. Expected a base64 string, a URL or a tagged file value."))
}

// ------------------------------------------------------------------ Media types

const IMAGE_SIGNATURES: &[(&str, &[Option<u8>])] = &[
    ("image/gif", &[Some(0x47), Some(0x49), Some(0x46)]),
    ("image/png", &[Some(0x89), Some(0x50), Some(0x4e), Some(0x47)]),
    ("image/jpeg", &[Some(0xff), Some(0xd8)]),
    ("image/webp", &[Some(0x52), Some(0x49), Some(0x46), Some(0x46), None, None, None, None, Some(0x57), Some(0x45), Some(0x42), Some(0x50)]),
    ("image/bmp", &[Some(0x42), Some(0x4d)]),
    ("image/tiff", &[Some(0x49), Some(0x49), Some(0x2a), Some(0x00)]),
    ("image/tiff", &[Some(0x4d), Some(0x4d), Some(0x00), Some(0x2a)]),
    ("image/avif", &[Some(0x00), Some(0x00), Some(0x00), Some(0x20), Some(0x66), Some(0x74), Some(0x79), Some(0x70), Some(0x61), Some(0x76), Some(0x69), Some(0x66)]),
    ("image/heic", &[Some(0x00), Some(0x00), Some(0x00), Some(0x20), Some(0x66), Some(0x74), Some(0x79), Some(0x70), Some(0x68), Some(0x65), Some(0x69), Some(0x63)]),
];

const DOCUMENT_SIGNATURES: &[(&str, &[Option<u8>])] = &[("application/pdf", &[Some(0x25), Some(0x50), Some(0x44), Some(0x46)])];

const AUDIO_SIGNATURES: &[(&str, &[Option<u8>])] = &[
    ("audio/mpeg", &[Some(0xff), Some(0xfb)]),
    ("audio/mpeg", &[Some(0xff), Some(0xfa)]),
    ("audio/mpeg", &[Some(0xff), Some(0xf3)]),
    ("audio/mpeg", &[Some(0xff), Some(0xf2)]),
    ("audio/mpeg", &[Some(0xff), Some(0xe3)]),
    ("audio/mpeg", &[Some(0xff), Some(0xe2)]),
    ("audio/wav", &[Some(0x52), Some(0x49), Some(0x46), Some(0x46), None, None, None, None, Some(0x57), Some(0x41), Some(0x56), Some(0x45)]),
    ("audio/ogg", &[Some(0x4f), Some(0x67), Some(0x67), Some(0x53)]),
    ("audio/flac", &[Some(0x66), Some(0x4c), Some(0x61), Some(0x43)]),
    ("audio/aac", &[Some(0x40), Some(0x15), Some(0x00), Some(0x00)]),
    ("audio/webm", &[Some(0x1a), Some(0x45), Some(0xdf), Some(0xa3)]),
    ("audio/mp4", &[Some(0x00), Some(0x00), Some(0x00), None, Some(0x66), Some(0x74), Some(0x79), Some(0x70)]),
];

const VIDEO_SIGNATURES: &[(&str, &[Option<u8>])] = &[
    ("video/mp4", &[Some(0x00), Some(0x00), Some(0x00), None, Some(0x66), Some(0x74), Some(0x79), Some(0x70)]),
    ("video/webm", &[Some(0x1a), Some(0x45), Some(0xdf), Some(0xa3)]),
    ("video/quicktime", &[Some(0x00), Some(0x00), Some(0x00), Some(0x14), Some(0x66), Some(0x74), Some(0x79), Some(0x70), Some(0x71), Some(0x74)]),
    ("video/x-msvideo", &[Some(0x52), Some(0x49), Some(0x46), Some(0x46)]),
];

fn base64_prefix(text: &str, max_bytes: usize) -> Vec<u8> {
    let chars = max_bytes.div_ceil(3) * 4;
    let head = &text.as_bytes()[..text.len().min(chars)];
    let cleaned: Vec<u8> = head.iter().copied().filter(|b| !b.is_ascii_whitespace()).map(|b| match b {
        b'-' => b'+',
        b'_' => b'/',
        b => b,
    }).collect();
    let usable = cleaned.len() - cleaned.len() % 4;
    let mut out = base64_simd::STANDARD.decode_to_vec(&cleaned[..usable]).or_else(|_| base64_simd::STANDARD_NO_PAD.decode_to_vec(&cleaned[..usable])).unwrap_or_default();
    out.truncate(max_bytes);
    out
}

/// `detectMediaType` over inline base64.
pub fn detect_media_type(data: &str, top_level: &str) -> Option<&'static str> {
    let table = match top_level {
        "image" => IMAGE_SIGNATURES,
        "audio" => AUDIO_SIGNATURES,
        "video" => VIDEO_SIGNATURES,
        "application" => DOCUMENT_SIGNATURES,
        _ => return None,
    };
    let mut bytes = base64_prefix(data, 18);
    // ID3v2-tagged MP3s carry the audio frame after the tag.
    if bytes.len() > 10 && bytes[0] == 0x49 && bytes[1] == 0x44 && bytes[2] == 0x33 {
        let long = base64_prefix(data, 128 * 1024 + 12);
        let size = ((long[6] as usize & 0x7f) << 21) | ((long[7] as usize & 0x7f) << 14) | ((long[8] as usize & 0x7f) << 7) | (long[9] as usize & 0x7f);
        bytes = long.get(size + 10..).map(<[u8]>::to_vec).unwrap_or_default();
    }
    table.iter().find_map(|(media, prefix)| {
        (bytes.len() >= prefix.len() && prefix.iter().enumerate().all(|(i, b)| b.is_none_or(|b| bytes[i] == b))).then_some(*media)
    })
}

pub fn top_level_media_type(media_type: &str) -> &str {
    media_type.split_once('/').map_or(media_type, |(top, _)| top)
}

pub fn is_full_media_type(media_type: &str) -> bool {
    media_type.split_once('/').is_some_and(|(_, sub)| !sub.is_empty() && sub != "*")
}

/// `resolveFullMediaType` for a file part as the conversion leaves it: `{ mediaType, data: {type,…} }`.
pub fn resolve_full_media_type(part: &Json) -> Result<String, AiError> {
    let media_type = part.str_at("mediaType").unwrap_or("");
    if is_full_media_type(media_type) {
        return Ok(media_type.to_owned());
    }
    if part.path(&["data", "type"]).and_then(Json::as_str) == Some("data") {
        if let Some(detected) = part.path(&["data", "data"]).and_then(Json::as_str).and_then(|d| detect_media_type(d, top_level_media_type(media_type))) {
            return Ok(detected.to_owned());
        }
        return Err(AiError::other(format!(
            "'file of media type \"{media_type}\" must specify subtype since it could not be auto-detected' functionality not supported."
        )));
    }
    Err(AiError::other(format!(
        "'file of media type \"{media_type}\" must specify subtype since it is not passed as inline bytes' functionality not supported."
    )))
}

// ------------------------------------------------------------------ Downloads

/// `isUrlSupported`.
fn url_supported(supported: &[(&'static str, &'static str)], media_type: &str, url: &str) -> bool {
    let url = url.to_ascii_lowercase();
    let media_type = media_type.to_ascii_lowercase();
    let top_only = !media_type.contains('/');
    supported.iter().any(|(key, pattern)| {
        let key = key.to_ascii_lowercase();
        let prefix = if key == "*" || key == "*/*" { String::new() } else { key.replacen('*', "", 1) };
        let matches = prefix.is_empty()
            || if top_only { format!("{media_type}/") == prefix } else { media_type.starts_with(&prefix) };
        matches && regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&url))
    })
}

/// `validateDownloadUrl`: refuses private and internal addresses before anything connects.
fn validate_download_url(url: &str) -> Result<(), AiError> {
    // `DownloadError` is given its `message` here, so its `Failed to download` default does not apply.
    let refuse = |reason: String| Err(AiError::other(reason));
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        let scheme = url.split_once(':').map_or("", |(scheme, _)| scheme).to_ascii_lowercase();
        return refuse(format!("URL scheme must be http, https, or data, got {scheme}:"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("").to_ascii_lowercase()
    } else {
        authority.rsplit_once(':').map_or(authority, |(host, _)| host).to_ascii_lowercase()
    };
    let host = host.trim_end_matches('.').to_owned();
    if host.is_empty() {
        return refuse("URL must have a hostname".to_owned());
    }
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".localhost") {
        return refuse(format!("URL with hostname {host} is not allowed"));
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let shown = if ip.is_ipv6() { format!("IPv6 address [{host}]") } else { format!("IP address {host}") };
        let private = match ip {
            std::net::IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
                    || a == 0 || (a == 100 && (64..128).contains(&b))
            }
            std::net::IpAddr::V6(v6) => {
                let first = v6.segments()[0];
                v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
                    || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private() || v4.is_loopback() || v4.is_link_local())
            }
        };
        if private {
            return refuse(format!("URL with {shown} is not allowed"));
        }
    }
    Ok(())
}

const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;

/// `download`: an asset the model cannot fetch itself. Redirects are re-validated hop by hop.
async fn download(url: &str, cancel: &Cancel) -> Result<(Vec<u8>, Option<String>), AiError> {
    let fetch = NetFetch;
    let mut at = url.to_owned();
    for _ in 0..10 {
        validate_download_url(&at)?;
        let request = HttpRequest { method: "GET", url: at.clone(), headers: Default::default(), body: None, ca_roots: None, cancel: cancel.clone() };
        let mut response = crate::http::Fetch::fetch(&fetch, request).await?;
        if (300..400).contains(&response.status) {
            let Some(location) = response.headers.get("location").map(str::to_owned) else {
                return Err(AiError::other(format!("Failed to download {url}: {} {}", response.status, response.status_text)));
            };
            at = join_url(&at, &location);
            continue;
        }
        if !response.ok() {
            return Err(AiError::other(format!("Failed to download {url}: {} {}", response.status, response.status_text)));
        }
        let media = response.headers.get("content-type").map(str::to_owned);
        let mut out = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await? {
            out.extend_from_slice(&chunk);
            if out.len() > MAX_DOWNLOAD_BYTES {
                return Err(AiError::other(format!("Download of {url} exceeded maximum size of {MAX_DOWNLOAD_BYTES} bytes.")));
            }
        }
        return Ok((out, media));
    }
    Err(AiError::other(format!("Failed to download {url}: Too many redirects.")))
}

fn join_url(base: &str, location: &str) -> String {
    if location.contains("://") {
        return location.to_owned();
    }
    let scheme_end = base.find("://").map_or(0, |at| at + 3);
    let path_start = base[scheme_end..].find('/').map_or(base.len(), |at| at + scheme_end);
    if location.starts_with('/') {
        return format!("{}{location}", &base[..path_start]);
    }
    let last = base.rfind('/').filter(|at| *at >= path_start).unwrap_or(base.len());
    format!("{}/{location}", &base[..last])
}

type Downloads = HashMap<String, (String, Option<String>)>;

/// `downloadAssets`: every file URL the model does not take, fetched up front.
async fn download_assets(messages: &[Json], supported: &[(&'static str, &'static str)], cancel: &Cancel) -> Result<Downloads, AiError> {
    let mut wanted: Vec<(Option<String>, Json)> = Vec::new();
    for message in messages {
        let role = message.str_at("role").unwrap_or("");
        let Some(content) = message.get("content").and_then(Json::as_arr) else { continue };
        match role {
            "user" => {
                for part in content {
                    if part.str_at("type") == Some("image") {
                        wanted.push((Some(part.str_at("mediaType").unwrap_or("image").to_owned()), part.get("image").cloned().unwrap_or_default()));
                    } else if part.str_at("type") == Some("file") {
                        wanted.push((part.str_at("mediaType").map(str::to_owned), part.get("data").cloned().unwrap_or_default()));
                    }
                }
            }
            "tool" | "assistant" => {
                for part in content {
                    if part.str_at("type") != Some("tool-result") {
                        continue;
                    }
                    if part.path(&["output", "type"]).and_then(Json::as_str) != Some("content") {
                        continue;
                    }
                    if let Some(items) = part.path(&["output", "value"]).and_then(Json::as_arr) {
                        for item in items {
                            if item.str_at("type") == Some("file") {
                                wanted.push((item.str_at("mediaType").map(str::to_owned), item.get("data").cloned().unwrap_or_default()));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Downloads::new();
    for (media_type, data) in wanted {
        let converted = convert_file_data(&data)?;
        if converted.data.str_at("type") != Some("url") {
            continue;
        }
        let url = converted.data.str_at("url").unwrap_or("").to_owned();
        if media_type.as_deref().is_some_and(|m| url_supported(supported, m, &url)) || out.contains_key(&url) {
            continue;
        }
        let (bytes, media) = download(&url, cancel).await?;
        out.insert(url, (base64_simd::STANDARD.encode_to_string(&bytes), media));
    }
    Ok(out)
}

// ------------------------------------------------------------------ Conversion

/// `convertPartToLanguageModelPart`, for a `file` part.
fn convert_file_part(part: &Json, downloads: &Downloads) -> Result<Json, AiError> {
    let source = if part.str_at("type") == Some("image") { part.get("image") } else { part.get("data") };
    let converted = convert_file_data(source.unwrap_or(&Json::Null))?;
    let declared = if part.str_at("type") == Some("image") {
        Some(part.str_at("mediaType").unwrap_or("image").to_owned())
    } else {
        part.str_at("mediaType").map(str::to_owned)
    };
    let mut media_type = converted.media_type.or(declared);
    let mut data = converted.data;
    if data.str_at("type") == Some("url") {
        if let Some((bytes, downloaded_media)) = data.str_at("url").and_then(|u| downloads.get(u)) {
            data = jobj! { "type": "data", "data": bytes.as_str() };
            if let Some(downloaded_media) = downloaded_media {
                if media_type.as_deref().is_none_or(|m| !is_full_media_type(m)) {
                    media_type = Some(downloaded_media.clone());
                }
            }
        }
    }
    if data.str_at("type") == Some("data") {
        if let Some(found) = data.str_at("data").and_then(|d| detect_media_type(d, "image")) {
            media_type = Some(found.to_owned());
        }
    }
    let Some(media_type) = media_type else {
        return Err(AiError::other("Media type is missing for file part"));
    };
    Ok(jobj! {
        "type": "file",
        "mediaType": media_type,
        "filename"?: part.get("filename").cloned(),
        "data": data,
        "providerOptions"?: part.get("providerOptions").cloned(),
    })
}

fn media_type_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = path.split_once("://").map_or(path, |(_, rest)| rest.split_once('/').map_or("", |(_, p)| p));
    let extension = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "heic" => "image/heic",
        "bmp" => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn file_id_reference(file_id: &Json, provider: &str) -> Json {
    if file_id.is_obj() {
        return file_id.clone();
    }
    let mut reference = Obj::new();
    reference.insert(provider, file_id.clone());
    Json::Obj(reference)
}

/// `mapToolResultOutput`, the legacy tool-output forms included.
fn map_tool_output(output: &Json, provider: &str, downloads: &Downloads) -> Result<Json, AiError> {
    if output.str_at("type") != Some("content") {
        return Ok(output.clone());
    }
    let mut value = Vec::new();
    let options = |item: &Json| item.get("providerOptions").cloned();
    for item in output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        let mapped = match item.str_at("type").unwrap_or("") {
            "file" => convert_file_part(item, downloads)?,
            "file-data" => jobj! {
                "type": "file",
                "data": jobj! { "type": "data", "data": item.get("data").cloned().unwrap_or_default() },
                "filename"?: item.get("filename").cloned(),
                "mediaType": item.get("mediaType").cloned().unwrap_or_default(),
                "providerOptions"?: options(item),
            },
            "file-url" => {
                let url = item.str_at("url").unwrap_or("");
                let media = item.str_at("mediaType").map_or_else(|| media_type_from_url(url), str::to_owned);
                jobj! { "type": "file", "data": jobj! { "type": "url", "url": url }, "mediaType": media, "providerOptions"?: options(item) }
            }
            "file-id" => jobj! {
                "type": "file",
                "data": jobj! { "type": "reference", "reference": file_id_reference(item.get("fileId").unwrap_or(&Json::Null), provider) },
                "mediaType": "application",
                "providerOptions"?: options(item),
            },
            "file-reference" => jobj! {
                "type": "file",
                "data": jobj! { "type": "reference", "reference": item.get("providerReference").cloned().unwrap_or_default() },
                "mediaType": "application",
                "providerOptions"?: options(item),
            },
            "image-data" => jobj! {
                "type": "file",
                "data": jobj! { "type": "data", "data": item.get("data").cloned().unwrap_or_default() },
                "mediaType": item.get("mediaType").cloned().unwrap_or_default(),
                "providerOptions"?: options(item),
            },
            "image-url" => jobj! {
                "type": "file",
                "data": jobj! { "type": "url", "url": item.get("url").cloned().unwrap_or_default() },
                "mediaType": "image",
                "providerOptions"?: options(item),
            },
            "image-file-id" => jobj! {
                "type": "file",
                "data": jobj! { "type": "reference", "reference": file_id_reference(item.get("fileId").unwrap_or(&Json::Null), provider) },
                "mediaType": "image",
                "providerOptions"?: options(item),
            },
            "image-file-reference" => jobj! {
                "type": "file",
                "data": jobj! { "type": "reference", "reference": item.get("providerReference").cloned().unwrap_or_default() },
                "mediaType": "image",
                "providerOptions"?: options(item),
            },
            _ => item.clone(),
        };
        value.push(mapped);
    }
    Ok(jobj! { "type": "content", "value": value })
}

fn convert_message(message: &Json, provider: &str, downloads: &Downloads) -> Result<Json, AiError> {
    let role = message.str_at("role").unwrap_or("");
    let options = message.get("providerOptions").cloned();
    match role {
        "system" => Ok(jobj! {
            "role": "system",
            "content": message.get("content").cloned().unwrap_or_default(),
            "providerOptions"?: options,
        }),
        "user" | "assistant" => {
            if let Some(text) = message.get("content").and_then(Json::as_str) {
                return Ok(jobj! {
                    "role": role,
                    "content": vec![jobj! { "type": "text", "text": text }],
                    "providerOptions"?: options,
                });
            }
            let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
            let mut content = Vec::with_capacity(parts.len());
            for part in parts {
                let part_options = part.get("providerOptions").cloned();
                let kind = part.str_at("type").unwrap_or("");
                if role == "user" {
                    match kind {
                        "text" => {
                            let text = part.str_at("text").unwrap_or("");
                            if !text.is_empty() {
                                content.push(jobj! { "type": "text", "text": text, "providerOptions"?: part_options });
                            }
                        }
                        "image" | "file" => content.push(convert_file_part(part, downloads)?),
                        _ => return Err(AiError::other("The messages do not match the ModelMessage[] schema.")),
                    }
                    continue;
                }
                match kind {
                    "text" => {
                        let text = part.str_at("text").unwrap_or("");
                        if text.is_empty() && part_options.is_none() {
                            continue;
                        }
                        content.push(jobj! { "type": "text", "text": text, "providerOptions"?: part_options });
                    }
                    "tool-approval-request" => {}
                    "custom" => content.push(jobj! {
                        "type": "custom",
                        "kind": part.get("kind").cloned().unwrap_or_default(),
                        "providerOptions"?: part_options,
                    }),
                    "file" => {
                        let converted = convert_file_data(part.get("data").unwrap_or(&Json::Null))?;
                        let media = converted.media_type.or_else(|| part.str_at("mediaType").map(str::to_owned));
                        content.push(jobj! {
                            "type": "file",
                            "data": converted.data,
                            "filename"?: part.get("filename").cloned(),
                            "mediaType"?: media,
                            "providerOptions"?: part_options,
                        });
                    }
                    "reasoning" => content.push(jobj! {
                        "type": "reasoning",
                        "text": part.get("text").cloned().unwrap_or_default(),
                        "providerOptions"?: part_options,
                    }),
                    "reasoning-file" => {
                        let converted = convert_file_data(part.get("data").unwrap_or(&Json::Null))?;
                        let kind = converted.data.str_at("type").unwrap_or("");
                        if kind != "data" && kind != "url" {
                            return Err(AiError::other(format!("Unsupported reasoning-file data type: {kind}")));
                        }
                        let media = converted.media_type.or_else(|| part.str_at("mediaType").map(str::to_owned));
                        content.push(jobj! {
                            "type": "reasoning-file",
                            "data": converted.data,
                            "mediaType"?: media,
                            "providerOptions"?: part_options,
                        });
                    }
                    "tool-call" => content.push(jobj! {
                        "type": "tool-call",
                        "toolCallId": part.get("toolCallId").cloned().unwrap_or_default(),
                        "toolName": part.get("toolName").cloned().unwrap_or_default(),
                        "input": part.get("input").cloned().unwrap_or_default(),
                        "providerExecuted"?: part.get("providerExecuted").cloned(),
                        "providerOptions"?: part_options,
                    }),
                    "tool-result" => content.push(jobj! {
                        "type": "tool-result",
                        "toolCallId": part.get("toolCallId").cloned().unwrap_or_default(),
                        "toolName": part.get("toolName").cloned().unwrap_or_default(),
                        "output": map_tool_output(part.get("output").unwrap_or(&Json::Null), provider, downloads)?,
                        "providerOptions"?: part_options,
                    }),
                    _ => return Err(AiError::other("The messages do not match the ModelMessage[] schema.")),
                }
            }
            Ok(jobj! { "role": role, "content": content, "providerOptions"?: options })
        }
        "tool" => {
            let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
            let mut content = Vec::with_capacity(parts.len());
            for part in parts {
                match part.str_at("type").unwrap_or("") {
                    "tool-result" => content.push(jobj! {
                        "type": "tool-result",
                        "toolCallId": part.get("toolCallId").cloned().unwrap_or_default(),
                        "toolName": part.get("toolName").cloned().unwrap_or_default(),
                        "output": map_tool_output(part.get("output").unwrap_or(&Json::Null), provider, downloads)?,
                        "providerOptions"?: part.get("providerOptions").cloned(),
                    }),
                    "tool-approval-response" if part.get("providerExecuted").and_then(Json::as_bool) == Some(true) => {
                        content.push(jobj! {
                            "type": "tool-approval-response",
                            "approvalId": part.get("approvalId").cloned().unwrap_or_default(),
                            "approved": part.get("approved").cloned().unwrap_or_default(),
                            "reason"?: part.get("reason").cloned(),
                        });
                    }
                    "tool-approval-response" => {}
                    _ => return Err(AiError::other("The messages do not match the ModelMessage[] schema.")),
                }
            }
            Ok(jobj! { "role": "tool", "content": content, "providerOptions"?: options })
        }
        other => Err(AiError::other(format!("Invalid message role: '{other}'"))),
    }
}

/// `standardizePrompt`, for `system` + `messages`.
pub fn standardize_prompt(messages: &[Json], allow_system_in_messages: bool) -> Result<(), AiError> {
    let invalid = |detail: &str| Err(AiError::other(format!("Invalid prompt: {detail}")));
    if messages.is_empty() {
        return invalid("messages must not be empty");
    }
    if !allow_system_in_messages && messages.iter().any(|m| m.str_at("role") == Some("system")) {
        return invalid("System messages are not allowed in the prompt or messages fields. Use the instructions option instead.");
    }
    for message in messages {
        let shape_ok = match message.str_at("role") {
            Some("system") => message.get("content").is_some_and(Json::is_str),
            Some("user") | Some("assistant") => {
                message.get("content").is_some_and(|c| c.is_str() || c.is_arr())
            }
            Some("tool") => message.get("content").is_some_and(Json::is_arr),
            _ => false,
        };
        if !shape_ok {
            return invalid("The messages do not match the ModelMessage[] schema.");
        }
    }
    Ok(())
}

/// `convertToLanguageModelPrompt`.
pub async fn convert_prompt(
    system: Option<&str>,
    messages: &[Json],
    supported_urls: &[(&'static str, &'static str)],
    provider: &str,
    cancel: &Cancel,
) -> Result<Vec<Json>, AiError> {
    let downloads = download_assets(messages, supported_urls, cancel).await?;

    let mut approval_ids: HashMap<String, String> = HashMap::new();
    for message in messages {
        if message.str_at("role") != Some("assistant") {
            continue;
        }
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") == Some("tool-approval-request") {
                if let (Some(a), Some(t)) = (part.str_at("approvalId"), part.str_at("toolCallId")) {
                    approval_ids.insert(a.to_owned(), t.to_owned());
                }
            }
        }
    }
    let mut approved: HashSet<String> = HashSet::new();
    for message in messages {
        if message.str_at("role") != Some("tool") {
            continue;
        }
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") == Some("tool-approval-response") {
                if let Some(id) = part.str_at("approvalId").and_then(|a| approval_ids.get(a)) {
                    approved.insert(id.clone());
                }
            }
        }
    }

    let mut converted: Vec<Json> = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = system {
        converted.push(jobj! { "role": "system", "content": system });
    }
    for message in messages {
        converted.push(convert_message(message, provider, &downloads)?);
    }

    // Consecutive tool messages become one.
    let mut combined: Vec<Json> = Vec::with_capacity(converted.len());
    for message in converted {
        if message.str_at("role") != Some("tool") {
            combined.push(message);
            continue;
        }
        match combined.last_mut() {
            Some(last) if last.str_at("role") == Some("tool") => {
                let last_options = last.get("providerOptions").cloned();
                let last_obj = last.as_obj_mut().expect("a message is an object");
                if let (Some(content), Some(options)) = (last_obj.get_mut("content").and_then(Json::as_arr_mut), &last_options) {
                    if let Some(part) = content.last_mut() {
                        let merged = merge_objects(Some(options), part.get("providerOptions"));
                        if let (Some(obj), Some(merged)) = (part.as_obj_mut(), merged) {
                            obj.insert("providerOptions", merged);
                        }
                    }
                }
                let new_content = message.get("content").and_then(Json::as_arr).cloned().unwrap_or_default();
                if let Some(content) = last_obj.get_mut("content").and_then(Json::as_arr_mut) {
                    content.extend(new_content);
                }
                match message.get("providerOptions") {
                    Some(options) => last_obj.insert("providerOptions", options.clone()),
                    None => {
                        last_obj.remove("providerOptions");
                    }
                }
            }
            _ => combined.push(message),
        }
    }

    // A client call needs its result before the conversation goes on.
    let mut open: Vec<String> = Vec::new();
    let missing = |open: &mut Vec<String>| -> Result<(), AiError> {
        open.retain(|id| !approved.contains(id));
        if open.is_empty() {
            return Ok(());
        }
        let (verb, noun) = if open.len() > 1 { ("Tool results are", "tool calls") } else { ("Tool result is", "tool call") };
        Err(AiError::other(format!("{verb} missing for {noun} {}.", open.join(", "))))
    };
    for message in &combined {
        let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        match message.str_at("role").unwrap_or("") {
            "assistant" => {
                for part in content {
                    if part.str_at("type") == Some("tool-call") && part.get("providerExecuted").and_then(Json::as_bool) != Some(true) {
                        if let Some(id) = part.str_at("toolCallId") {
                            if !open.iter().any(|seen| seen == id) {
                                open.push(id.to_owned());
                            }
                        }
                    }
                }
            }
            "tool" => {
                for part in content {
                    if part.str_at("type") == Some("tool-result") {
                        if let Some(id) = part.str_at("toolCallId") {
                            open.retain(|seen| seen != id);
                        }
                    }
                }
            }
            _ => missing(&mut open)?,
        }
    }
    missing(&mut open)?;

    combined.retain(|m| m.str_at("role") != Some("tool") || m.get("content").and_then(Json::as_arr).is_some_and(|c| !c.is_empty()));
    Ok(combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jarr;

    #[test]
    fn deep_merge_replaces_arrays_and_keeps_order() {
        let base = Json::parse(r#"{"a":{"x":1,"y":[1]},"b":2}"#).unwrap();
        let over = Json::parse(r#"{"a":{"y":[2],"z":3},"c":4}"#).unwrap();
        assert_eq!(merge_objects(Some(&base), Some(&over)).unwrap().to_text(), r#"{"a":{"x":1,"y":[2],"z":3},"b":2,"c":4}"#);
    }

    #[test]
    fn media_types_sniff_from_inline_bytes() {
        assert_eq!(detect_media_type("iVBORw0KGgo=", "image"), Some("image/png"));
        assert_eq!(detect_media_type("JVBERi0xLjQ=", "application"), Some("application/pdf"));
        assert_eq!(detect_media_type("aGVsbG8=", "image"), None);
    }

    #[tokio::test]
    async fn a_missing_tool_result_fails_the_prompt() {
        let messages = vec![
            jobj! { "role": "user", "content": "q" },
            jobj! { "role": "assistant", "content": jarr![jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "t", "input": jobj! {} }] },
            jobj! { "role": "user", "content": "next" },
        ];
        let error = convert_prompt(None, &messages, &[], "anthropic", &Cancel::new()).await.unwrap_err();
        assert!(error.message().contains("c1"), "{error}");
    }

    #[tokio::test]
    async fn consecutive_tool_messages_combine_and_empty_text_goes() {
        let messages = vec![
            jobj! { "role": "user", "content": jarr![jobj! { "type": "text", "text": "" }, jobj! { "type": "text", "text": "q" }] },
            jobj! { "role": "assistant", "content": jarr![
                jobj! { "type": "tool-call", "toolCallId": "a", "toolName": "t", "input": jobj! {} },
                jobj! { "type": "tool-call", "toolCallId": "b", "toolName": "t", "input": jobj! {} },
            ] },
            jobj! { "role": "tool", "content": jarr![jobj! { "type": "tool-result", "toolCallId": "a", "toolName": "t", "output": jobj! { "type": "text", "value": "1" } }] },
            jobj! { "role": "tool", "content": jarr![jobj! { "type": "tool-result", "toolCallId": "b", "toolName": "t", "output": jobj! { "type": "text", "value": "2" } }] },
        ];
        let prompt = convert_prompt(Some("S"), &messages, &[], "anthropic", &Cancel::new()).await.unwrap();
        assert_eq!(prompt.len(), 4);
        assert_eq!(prompt[1].get("content").unwrap().as_arr().unwrap().len(), 1);
        assert_eq!(prompt[3].get("content").unwrap().as_arr().unwrap().len(), 2);
    }
}
