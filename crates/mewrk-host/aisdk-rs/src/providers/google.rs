//! `@ai-sdk/google`: `createGoogle` and the streaming path of `GoogleLanguageModel`, which Vertex reuses.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;

use crate::api::{ErrorShape, VERSION_GOOGLE, combine_headers, generate_id, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::error::AiError;
use crate::http::{BoxFuture, Cancel, DynFetch, Headers};
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, ModelUsage, StreamPart, SupportedUrls};
use crate::prompt::{is_full_media_type, resolve_full_media_type, top_level_media_type};
use crate::reasoning::js_round;
use crate::stream_text::secure_json_parse;
use crate::{jarr, jobj};

use super::ProviderSettings;

const GOOGLE_API_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// `SKIP_THOUGHT_SIGNATURE_VALIDATOR`: what Google documents for replaying a `functionCall` whose original
/// signature is gone.
const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    let base_url = target.base_url.as_deref().map_or(GOOGLE_API_URL, without_trailing_slash).to_owned();
    let mut headers = Headers::new();
    headers.set("x-goog-api-key", &target.api_key);
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/google/{VERSION_GOOGLE}")]);
    let supported_urls = gemini_api_supported_urls(&base_url, &target.model_id);
    Ok(Arc::new(GoogleLanguageModel {
        provider: "google.generative-ai",
        model_id: target.model_id,
        base_url,
        headers: Box::new(headers),
        fetch: target.fetch,
        supported_urls,
    }))
}

/// `config.headers`, resolved once per call: Vertex's fetch an access token first.
pub trait ResolveHeaders: Send + Sync {
    fn resolve<'a>(&'a self, cancel: &'a Cancel) -> BoxFuture<'a, Result<Headers, AiError>>;
}

impl ResolveHeaders for Headers {
    fn resolve<'a>(&'a self, _cancel: &'a Cancel) -> BoxFuture<'a, Result<Headers, AiError>> {
        Box::pin(async move { Ok(self.clone()) })
    }
}

/// `GoogleLanguageModel`. `createGoogle` wraps it in `GoogleBatchLanguageModel`, which does not touch `doStream`.
pub struct GoogleLanguageModel {
    pub provider: &'static str,
    pub model_id: String,
    pub base_url: String,
    pub headers: Box<dyn ResolveHeaders>,
    pub fetch: DynFetch,
    pub supported_urls: SupportedUrls,
}

/// A pattern built from a configured base URL, kept for the process: `SupportedUrls` holds `'static` patterns,
/// and models are built per step while base URLs are few.
fn intern(pattern: String) -> &'static str {
    static SEEN: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner());
    if let Some(found) = seen.get(&pattern) {
        return found;
    }
    let leaked: &'static str = Box::leak(pattern.clone().into_boxed_str());
    seen.insert(pattern, leaked);
    leaked
}

/// `createGoogle`'s `supportedUrls`. `\w` is spelled out because the regex crate's is Unicode-aware and JavaScript's is not.
fn gemini_api_supported_urls(base_url: &str, model_id: &str) -> SupportedUrls {
    let mut urls: SupportedUrls = vec![
        ("*", intern(format!("^{base_url}/files/.*$"))),
        ("*", r"^https://(?:www\.)?youtube\.com/watch\?v=[0-9A-Za-z_-]+(?:&[0-9A-Za-z_=&.-]*)?$"),
        ("*", r"^https://youtu\.be/[0-9A-Za-z_-]+(?:\?[0-9A-Za-z_=&.-]*)?$"),
    ];
    static GEMINI: OnceLock<Regex> = OnceLock::new();
    static GEMINI_20: OnceLock<Regex> = OnceLock::new();
    // `supportsExternalFileUrls`.
    if pattern(&GEMINI, r"(^|/)gemini-").is_match(model_id) && !pattern(&GEMINI_20, r"(^|/)gemini-2\.0").is_match(model_id) {
        for media_type in [
            "text/html", "text/css", "text/plain", "text/xml", "text/csv", "text/rtf", "text/javascript", "application/json", "application/pdf", "image/bmp",
            "image/jpeg", "image/png", "image/webp", "video/mp4", "video/mpeg", "video/quicktime", "video/avi", "video/x-flv", "video/mpg", "video/webm",
            "video/wmv", "video/3gpp",
        ] {
            urls.push((media_type, r"^https://.*$"));
        }
    }
    urls
}

fn pattern(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("a literal pattern"))
}

// ------------------------------------------------------------------ Model capabilities

struct Capabilities {
    supports_gemini2_tools: bool,
    supports_file_search: bool,
    uses_gemini3_features: bool,
}

/// `getGoogleModelCapabilities`: unknown Gemini ids inherit the newest behaviour.
fn capabilities(model_id: &str) -> Capabilities {
    static GEMINI_1: OnceLock<Regex> = OnceLock::new();
    static GEMINI_2: OnceLock<Regex> = OnceLock::new();
    static GEMINI_25: OnceLock<Regex> = OnceLock::new();
    static GEMINI: OnceLock<Regex> = OnceLock::new();
    static GEMINI_PRO: OnceLock<Regex> = OnceLock::new();
    static ROBOTICS: OnceLock<Regex> = OnceLock::new();
    let is_gemini = pattern(&GEMINI, r"(?i)(^|/)gemini-").is_match(model_id);
    let is_gemini2 = pattern(&GEMINI_2, r"(?i)(^|/)gemini-2(?:[.-]|$)").is_match(model_id);
    let known_pre_gemini2 = pattern(&GEMINI_1, r"(?i)(^|/)gemini-1(?:[.-]|$)").is_match(model_id)
        || pattern(&GEMINI_PRO, r"(?i)(^|/)gemini-pro(?:-vision)?$").is_match(model_id)
        || pattern(&ROBOTICS, r"(?i)(^|/)gemini-robotics-er-1\.5(?:[.-]|$)").is_match(model_id);
    let uses_gemini3_features = is_gemini && !(known_pre_gemini2 || is_gemini2);
    Capabilities {
        supports_gemini2_tools: (is_gemini && !known_pre_gemini2) || model_id.to_lowercase().contains("nano-banana"),
        supports_file_search: pattern(&GEMINI_25, r"(?i)(^|/)gemini-2\.5(?:[.-]|$)").is_match(model_id) || uses_gemini3_features,
        uses_gemini3_features,
    }
}

// ------------------------------------------------------------------ Provider options

/// `googleLanguageModelOptions` after its schema: unknown keys gone, nested objects in the schema's key order.
#[derive(Default)]
struct GoogleOptions {
    response_modalities: Option<Json>,
    thinking_config: Option<Obj>,
    cached_content: Option<String>,
    safety_settings: Option<Json>,
    threshold: Option<String>,
    audio_timestamp: Option<bool>,
    labels: Option<Json>,
    media_resolution: Option<String>,
    image_config: Option<Obj>,
    retrieval_config: Option<Json>,
    stream_function_call_arguments: Option<bool>,
    service_tier: Option<String>,
    shared_request_type: Option<String>,
    request_type: Option<String>,
}

const THRESHOLDS: &[&str] = &["HARM_BLOCK_THRESHOLD_UNSPECIFIED", "BLOCK_LOW_AND_ABOVE", "BLOCK_MEDIUM_AND_ABOVE", "BLOCK_ONLY_HIGH", "BLOCK_NONE", "OFF"];

// `.optional()` takes `undefined` only, so a JSON `null` fails the schema like any other wrong type.
fn opt_enum(object: &Obj, key: &str, allowed: &[&str]) -> Result<Option<String>, ()> {
    match object.get(key) {
        None => Ok(None),
        Some(Json::Str(value)) if allowed.is_empty() || allowed.contains(&value.as_str()) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn opt_bool(object: &Obj, key: &str) -> Result<Option<bool>, ()> {
    match object.get(key) {
        None => Ok(None),
        Some(Json::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(()),
    }
}

fn opt_num(object: &Obj, key: &str) -> Result<Option<Json>, ()> {
    match object.get(key) {
        None => Ok(None),
        Some(value @ Json::Num(_)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn opt_obj<'a>(object: &'a Obj, key: &str) -> Result<Option<&'a Obj>, ()> {
    match object.get(key) {
        None => Ok(None),
        Some(Json::Obj(value)) => Ok(Some(value)),
        Some(_) => Err(()),
    }
}

fn read_options(object: &Obj) -> Result<GoogleOptions, ()> {
    let mut out = GoogleOptions::default();
    if let Some(value) = object.get("responseModalities") {
        let items = value.as_arr().ok_or(())?;
        if !items.iter().all(|m| matches!(m.as_str(), Some("TEXT" | "IMAGE"))) {
            return Err(());
        }
        out.response_modalities = Some(value.clone());
    }
    if let Some(thinking) = opt_obj(object, "thinkingConfig")? {
        let mut config = Obj::new();
        if let Some(budget) = opt_num(thinking, "thinkingBudget")? {
            config.insert("thinkingBudget", budget);
        }
        if let Some(include) = opt_bool(thinking, "includeThoughts")? {
            config.insert("includeThoughts", include);
        }
        if let Some(level) = opt_enum(thinking, "thinkingLevel", &["minimal", "low", "medium", "high"])? {
            config.insert("thinkingLevel", level);
        }
        out.thinking_config = Some(config);
    }
    out.cached_content = opt_enum(object, "cachedContent", &[])?;
    opt_bool(object, "structuredOutputs")?;
    if let Some(value) = object.get("safetySettings") {
        let mut settings = Vec::new();
        for item in value.as_arr().ok_or(())? {
            let item = item.as_obj().ok_or(())?;
            let category = opt_enum(
                item,
                "category",
                &["HARM_CATEGORY_UNSPECIFIED", "HARM_CATEGORY_HATE_SPEECH", "HARM_CATEGORY_DANGEROUS_CONTENT", "HARM_CATEGORY_HARASSMENT", "HARM_CATEGORY_SEXUALLY_EXPLICIT", "HARM_CATEGORY_CIVIC_INTEGRITY"],
            )?
            .ok_or(())?;
            let threshold = opt_enum(item, "threshold", THRESHOLDS)?.ok_or(())?;
            settings.push(jobj! { "category": category, "threshold": threshold });
        }
        out.safety_settings = Some(Json::Arr(settings));
    }
    out.threshold = opt_enum(object, "threshold", THRESHOLDS)?;
    out.audio_timestamp = opt_bool(object, "audioTimestamp")?;
    if let Some(value) = object.get("labels") {
        if !value.as_obj().ok_or(())?.iter().all(|(_, v)| v.is_str()) {
            return Err(());
        }
        out.labels = Some(value.clone());
    }
    out.media_resolution = opt_enum(object, "mediaResolution", &["MEDIA_RESOLUTION_UNSPECIFIED", "MEDIA_RESOLUTION_LOW", "MEDIA_RESOLUTION_MEDIUM", "MEDIA_RESOLUTION_HIGH"])?;
    if let Some(image) = opt_obj(object, "imageConfig")? {
        let mut config = Obj::new();
        for (key, allowed) in [
            ("aspectRatio", &["1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9", "1:8", "8:1", "1:4", "4:1"][..]),
            ("imageSize", &["1K", "2K", "4K", "512"][..]),
            ("personGeneration", &["PERSON_GENERATION_UNSPECIFIED", "ALLOW_ALL", "ALLOW_ADULT", "ALLOW_NONE"][..]),
            ("prominentPeople", &["PROMINENT_PEOPLE_UNSPECIFIED", "ALLOW_PROMINENT_PEOPLE", "BLOCK_PROMINENT_PEOPLE"][..]),
        ] {
            if let Some(value) = opt_enum(image, key, allowed)? {
                config.insert(key, value);
            }
        }
        if let Some(output) = opt_obj(image, "imageOutputOptions")? {
            config.insert(
                "imageOutputOptions",
                jobj! {
                    "mimeType"?: opt_enum(output, "mimeType", &["image/jpeg", "image/png"])?,
                    "compressionQuality"?: opt_num(output, "compressionQuality")?,
                },
            );
        }
        out.image_config = Some(config);
    }
    if let Some(retrieval) = opt_obj(object, "retrievalConfig")? {
        let lat_lng = match opt_obj(retrieval, "latLng")? {
            Some(position) => Some(jobj! {
                "latitude": opt_num(position, "latitude")?.ok_or(())?,
                "longitude": opt_num(position, "longitude")?.ok_or(())?,
            }),
            None => None,
        };
        out.retrieval_config = Some(jobj! { "latLng"?: lat_lng });
    }
    out.stream_function_call_arguments = opt_bool(object, "streamFunctionCallArguments")?;
    out.service_tier = opt_enum(object, "serviceTier", &["standard", "flex", "priority"])?;
    out.shared_request_type = opt_enum(object, "sharedRequestType", &["priority", "flex", "standard"])?;
    out.request_type = opt_enum(object, "requestType", &["shared"])?;
    Ok(out)
}

/// `parseProviderOptions`.
fn parse_provider_options(options: &CallOptions, name: &str) -> Result<Option<GoogleOptions>, AiError> {
    let Some(value) = options.provider_options.as_ref().and_then(|all| all.non_null(name)) else { return Ok(None) };
    value
        .as_obj()
        .ok_or(())
        .and_then(read_options)
        .map(Some)
        .map_err(|()| AiError::other(format!("invalid {name} provider options")))
}

// ------------------------------------------------------------------ Thinking

/// `resolveThinkingConfig`.
fn resolve_thinking_config(reasoning: Option<&str>, model_id: &str) -> Option<Obj> {
    let reasoning = reasoning.filter(|r| *r != "provider-default")?;
    let mut config = Obj::new();
    if capabilities(model_id).uses_gemini3_features && !model_id.contains("gemini-3-pro-image") {
        let minimum = minimum_thinking_level(model_id);
        let level = match reasoning {
            // Thinking cannot be fully disabled on Gemini 3.
            "none" | "minimal" => minimum,
            "low" => "low",
            "medium" => "medium",
            "high" | "xhigh" => "high",
            _ => return None,
        };
        config.insert("thinkingLevel", level);
        return Some(config);
    }
    if reasoning == "none" {
        config.insert("thinkingBudget", 0u64);
        return Some(config);
    }
    // `mapReasoningToProviderBudget` over Gemini 2.5's 65536 output tokens, with a floor of zero.
    let share = match reasoning {
        "minimal" => 0.02,
        "low" => 0.1,
        "medium" => 0.3,
        "high" => 0.6,
        "xhigh" => 0.9,
        _ => return None,
    };
    let id = model_id.to_lowercase();
    let ceiling = if id.contains("2.5-pro") || id.contains("gemini-3-pro-image") { 32_768 } else { 24_576 };
    config.insert("thinkingBudget", js_round(65_536.0 * share).min(ceiling));
    Some(config)
}

/// `getMinimumThinkingLevelForGemini3Model`.
fn minimum_thinking_level(model_id: &str) -> &'static str {
    static VERSION: OnceLock<Regex> = OnceLock::new();
    let name = model_id.rsplit('/').next().unwrap_or("").to_lowercase();
    if name == "gemini-flash-latest" {
        return "low";
    }
    // `^gemini-(\d+)\.(\d+)-flash(?:$|-(?!lite(?:-|$)))`; the regex crate has no lookahead.
    let Some(found) = pattern(&VERSION, r"^gemini-(\d+)\.(\d+)-flash(.*)$").captures(&name) else { return "minimal" };
    let rest = &found[3];
    let after_dash = rest.strip_prefix('-');
    let is_lite = after_dash.and_then(|r| r.strip_prefix("lite")).is_some_and(|r| r.is_empty() || r.starts_with('-'));
    if !(rest.is_empty() || (after_dash.is_some() && !is_lite)) {
        return "minimal";
    }
    let major: f64 = found[1].parse().unwrap_or(0.0);
    let minor: f64 = found[2].parse().unwrap_or(0.0);
    if major > 3.0 || (major == 3.0 && minor >= 7.0) { "low" } else { "minimal" }
}

// ------------------------------------------------------------------ JSON Schema to OpenAPI

enum SchemaError {
    /// `isRecursiveJSONSchemaReferenceError`: the tool falls back to `parametersJsonSchema`.
    Recursive,
    Other(AiError),
}

struct ReferenceContext<'a> {
    definitions: Option<&'a Json>,
    dollar_definitions: Option<&'a Json>,
    resolving: Vec<String>,
}

/// JavaScript truthiness.
pub(super) fn truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(v) => *v,
        Json::Num(n) => {
            let v = n.as_f64();
            v != 0.0 && !v.is_nan()
        }
        Json::Str(v) => !v.is_empty(),
        Json::Arr(_) | Json::Obj(_) => true,
    }
}

/// `convertJSONSchemaToOpenAPISchema`. `None` is `undefined`, which an empty root object schema becomes.
fn convert_json_schema(schema: &Json) -> Result<Option<Json>, SchemaError> {
    let mut context = ReferenceContext { definitions: schema.get("definitions"), dollar_definitions: schema.get("$defs"), resolving: Vec::new() };
    convert_definition(schema, true, &mut context)
}

fn convert_list(items: &[Json], context: &mut ReferenceContext<'_>) -> Result<Vec<Json>, SchemaError> {
    // `undefined` in an array stringifies as `null`.
    items.iter().map(|item| Ok(convert_definition(item, false, context)?.unwrap_or_default())).collect()
}

fn convert_definition(schema: &Json, is_root: bool, context: &mut ReferenceContext<'_>) -> Result<Option<Json>, SchemaError> {
    let object = match schema {
        Json::Null => return Ok(None),
        Json::Bool(_) => return Ok(Some(jobj! { "type": "boolean", "properties": Json::obj() })),
        Json::Obj(object) => object,
        // Destructuring anything else finds no keys.
        _ => return Ok(Some(Json::obj())),
    };
    if let Some(reference) = object.non_null("$ref") {
        return convert_reference(object, reference, is_root, context);
    }
    if is_empty_object_schema(object) {
        if is_root {
            return Ok(None);
        }
        return Ok(Some(match object.get("description").filter(|d| truthy(d)) {
            Some(description) => jobj! { "type": "object", "description": description },
            None => jobj! { "type": "object" },
        }));
    }

    let type_ = object.get("type");
    let mut result = Obj::new();
    for key in ["description", "required", "format"] {
        if let Some(value) = object.get(key).filter(|v| truthy(v)) {
            result.insert(key, value);
        }
    }
    if let Some(value) = type_.filter(|t| truthy(t)) {
        if let Some(types) = value.as_arr() {
            let is_null = |t: &&Json| t.as_str() == Some("null");
            let non_null: Vec<&Json> = types.iter().filter(|t| !is_null(t)).collect();
            if non_null.is_empty() {
                result.insert("type", "null");
            } else {
                result.insert("anyOf", non_null.iter().map(|t| jobj! { "type": *t }).collect::<Vec<_>>());
                if types.iter().any(|t| is_null(&t)) {
                    result.insert("nullable", true);
                }
            }
        } else {
            result.insert("type", value);
        }
    }

    let single_const;
    let values: Option<&[Json]> = match (object.non_null("enum"), object.get("const")) {
        (Some(values), _) => Some(values.as_arr().map_or(&[][..], Vec::as_slice)),
        (None, Some(value)) => {
            single_const = [value.clone()];
            Some(&single_const[..])
        }
        (None, None) => None,
    };
    if let Some(values) = values {
        add_enum_to_schema(values, type_, &mut result).map_err(SchemaError::Other)?;
    }

    if let Some(properties) = object.non_null("properties") {
        let mut converted = Obj::new();
        for (key, value) in properties.as_obj().map_or(&[][..], |o| o.0.as_slice()) {
            if let Some(value) = convert_definition(value, false, context)? {
                converted.insert(key.as_str(), value);
            }
        }
        result.insert("properties", converted);
    }
    if let Some(items) = object.get("items").filter(|i| truthy(i)) {
        match items.as_arr() {
            Some(list) => result.insert("items", convert_list(list, context)?),
            None => {
                if let Some(converted) = convert_definition(items, false, context)? {
                    result.insert("items", converted);
                }
            }
        }
    }
    if let Some(all_of) = object.get("allOf").filter(|v| truthy(v)) {
        result.insert("allOf", convert_list(all_of.as_arr().map_or(&[][..], Vec::as_slice), context)?);
    }
    if let Some(any_of) = object.get("anyOf").filter(|v| truthy(v)) {
        let list = any_of.as_arr().map_or(&[][..], Vec::as_slice);
        let is_null_schema = |schema: &Json| schema.str_at("type") == Some("null");
        if list.iter().any(is_null_schema) {
            let non_null: Vec<Json> = list.iter().filter(|s| !is_null_schema(s)).cloned().collect();
            if non_null.len() == 1 {
                // One non-null schema: convert it and make it nullable.
                if let Some(Json::Obj(converted)) = convert_definition(&non_null[0], false, context)? {
                    result.insert("nullable", true);
                    result.extend_from(&converted);
                }
            } else {
                result.insert("anyOf", convert_list(&non_null, context)?);
                result.insert("nullable", true);
            }
        } else {
            result.insert("anyOf", convert_list(list, context)?);
        }
    }
    if let Some(one_of) = object.get("oneOf").filter(|v| truthy(v)) {
        result.insert("oneOf", convert_list(one_of.as_arr().map_or(&[][..], Vec::as_slice), context)?);
    }
    if let Some(min_length) = object.get("minLength") {
        result.insert("minLength", min_length);
    }
    Ok(Some(Json::Obj(result)))
}

fn is_empty_object_schema(schema: &Obj) -> bool {
    schema.get("type").and_then(Json::as_str) == Some("object")
        && schema.non_null("properties").is_none_or(|p| p.as_obj().is_some_and(Obj::is_empty))
        && !schema.get("additionalProperties").is_some_and(truthy)
}

fn unsupported_reference() -> SchemaError {
    SchemaError::Other(AiError::other("Google schema conversion only supports references to direct children of root-level $defs or definitions."))
}

/// `decodeURIComponent`; `None` where it throws.
fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        let hex = std::str::from_utf8(bytes.get(at + 1..at + 3)?).ok()?;
        out.push(u8::from_str_radix(hex, 16).ok()?);
        at += 3;
    }
    String::from_utf8(out).ok()
}

/// `getReferencedDefinition`.
fn referenced_definition<'a>(reference: &Json, context: &ReferenceContext<'a>) -> Result<(&'a Json, String), SchemaError> {
    let fail = unsupported_reference;
    let text = reference.as_str().ok_or_else(fail)?;
    let (prefix, definitions) = if text.starts_with("#/$defs/") {
        ("#/$defs/", context.dollar_definitions)
    } else if text.starts_with("#/definitions/") {
        ("#/definitions/", context.definitions)
    } else {
        return Err(fail());
    };
    let encoded = &text[prefix.len()..];
    if encoded.is_empty() || encoded.contains('/') {
        return Err(fail());
    }
    let decoded = decode_uri_component(encoded).ok_or_else(fail)?;
    // `/~(?![01])/`.
    let bad_tilde = decoded.char_indices().any(|(at, c)| c == '~' && !matches!(decoded[at + 1..].chars().next(), Some('0' | '1')));
    if decoded.contains('/') || bad_tilde {
        return Err(fail());
    }
    // One pass over `~[01]`: `~01` is the literal `~1`.
    let name = decoded.replace("~1", "/").replace("~0", "~");
    let definition = definitions.and_then(Json::as_obj).and_then(|d| d.get(&name)).ok_or_else(fail)?;
    Ok((definition, format!("{prefix}{name}")))
}

/// `convertJSONSchemaReference`: inlines the definition, because the Gemini Developer API rejects `ref` and `defs`.
fn convert_reference(schema: &Obj, reference: &Json, is_root: bool, context: &mut ReferenceContext<'_>) -> Result<Option<Json>, SchemaError> {
    let (definition, key) = referenced_definition(reference, context)?;
    if context.resolving.contains(&key) {
        return Err(SchemaError::Recursive);
    }
    let mut sibling = schema.clone();
    sibling.remove("$ref");
    let resolved = match definition {
        Json::Bool(true) => Json::Obj(sibling),
        Json::Bool(false) => Json::Bool(false),
        other => {
            let mut merged = other.as_obj().cloned().unwrap_or_default();
            merged.extend_from(&sibling);
            Json::Obj(merged)
        }
    };
    context.resolving.push(key);
    let converted = convert_definition(&resolved, is_root, context);
    context.resolving.pop();
    converted
}

/// `addEnumToSchema`: Gemini uses `nullable` instead of a null enum member.
fn add_enum_to_schema(values: &[Json], type_: Option<&Json>, result: &mut Obj) -> Result<(), AiError> {
    let type_list = type_.and_then(Json::as_arr);
    let type_has = |name: &str| type_list.is_some_and(|list| list.iter().any(|t| t.as_str() == Some(name)));
    let nullable = type_has("null") || (type_.is_none() && values.iter().any(Json::is_null));
    let enum_values: Vec<&Json> = values.iter().filter(|v| !nullable || !v.is_null()).collect();
    if !values.is_empty() && values.iter().all(Json::is_null) {
        let type_allows_null = type_.is_none() || type_.and_then(Json::as_str) == Some("null") || type_has("null");
        if type_allows_null {
            result.insert("type", "null");
            if type_list.is_some() {
                result.remove("anyOf");
            }
            return Ok(());
        }
    }
    let Some(enum_type) = enum_type(&enum_values, type_) else {
        return Err(AiError::other("Google does not support this JSON Schema enum. Enum values must share one supported primitive type and match the schema type."));
    };
    result.insert("type", enum_type);
    // The earlier type-array conversion created `anyOf`; the enum gives one concrete value type.
    if type_list.is_some() {
        result.remove("anyOf");
    }
    if nullable {
        result.insert("nullable", true);
    }
    if enum_type == "string" {
        result.insert("enum", enum_values.iter().map(|v| (*v).clone()).collect::<Vec<_>>());
    } else {
        result.insert("format", "enum");
        result.insert("enum", enum_values.iter().map(|v| Json::from(v.to_text())).collect::<Vec<_>>());
    }
    Ok(())
}

/// `getEnumType`.
fn enum_type(values: &[&Json], type_: Option<&Json>) -> Option<&'static str> {
    if values.is_empty() {
        return None;
    }
    let allows = |name: &str| match type_ {
        None => true,
        Some(Json::Str(single)) => single == name,
        Some(Json::Arr(list)) => list.iter().any(|t| t.as_str() == Some(name)),
        Some(_) => false,
    };
    if allows("string") && values.iter().all(|v| v.is_str()) {
        return Some("string");
    }
    if (allows("number") || allows("integer")) && values.iter().all(|v| v.as_f64().is_some_and(f64::is_finite)) {
        if allows("number") {
            return Some("number");
        }
        if values.iter().all(|v| v.as_f64().is_some_and(|n| n.fract() == 0.0)) {
            return Some("integer");
        }
    }
    if allows("boolean") && values.iter().all(|v| v.as_bool().is_some()) {
        return Some("boolean");
    }
    None
}

// ------------------------------------------------------------------ Tools

/// `prepareFunctionDeclaration`.
fn function_declaration(tool: &Json) -> Result<Json, AiError> {
    let name = tool.get("name").cloned().unwrap_or_default();
    let description = tool.non_null("description").cloned().unwrap_or_else(|| Json::from(""));
    let schema = tool.get("inputSchema").unwrap_or(&Json::Null);
    match convert_json_schema(schema) {
        Ok(parameters) => Ok(jobj! { "name": name, "description": description, "parameters"?: parameters }),
        Err(SchemaError::Recursive) => Ok(jobj! { "name": name, "description": description, "parametersJsonSchema": schema }),
        Err(SchemaError::Other(error)) => Err(error),
    }
}

fn args_of(tool: &Json) -> Json {
    Json::Obj(tool.get("args").and_then(Json::as_obj).cloned().unwrap_or_default())
}

/// `prepareTools`. Tools a model cannot take are only warned about upstream, and warnings are only logged.
fn prepare_tools(tools: Option<&Vec<Json>>, tool_choice: &Json, model_id: &str, is_vertex: bool) -> Result<(Option<Vec<Json>>, Option<Json>), AiError> {
    let Some(tools) = tools.filter(|t| !t.is_empty()) else { return Ok((None, None)) };
    let caps = capabilities(model_id);
    let is_function = |tool: &&Json| tool.str_at("type") == Some("function");
    let has_function_tools = tools.iter().any(|t| is_function(&t));
    let has_provider_tools = tools.iter().any(|t| t.str_at("type") == Some("provider"));

    if has_provider_tools {
        let mut google_tools = Vec::new();
        for tool in tools.iter().filter(|t| t.str_at("type") == Some("provider")) {
            let gemini2 = caps.supports_gemini2_tools;
            match tool.str_at("id").unwrap_or("") {
                "google.google_search" if gemini2 => google_tools.push(jobj! { "googleSearch": args_of(tool) }),
                "google.enterprise_web_search" if gemini2 => google_tools.push(jobj! { "enterpriseWebSearch": Json::obj() }),
                "google.url_context" if gemini2 => google_tools.push(jobj! { "urlContext": Json::obj() }),
                "google.code_execution" if gemini2 => google_tools.push(jobj! { "codeExecution": Json::obj() }),
                "google.file_search" if caps.supports_file_search => google_tools.push(jobj! { "fileSearch": args_of(tool) }),
                "google.vertex_rag_store" if gemini2 => google_tools.push(jobj! {
                    "retrieval": jobj! {
                        "vertex_rag_store": jobj! {
                            "rag_resources": jobj! { "rag_corpus"?: tool.path(&["args", "ragCorpus"]) },
                            "similarity_top_k"?: tool.path(&["args", "topK"]),
                        },
                    },
                }),
                "google.google_maps" if gemini2 => google_tools.push(jobj! { "googleMaps": Json::obj() }),
                _ => {}
            }
        }
        if has_function_tools && caps.uses_gemini3_features && !google_tools.is_empty() {
            let declarations = tools.iter().filter(is_function).map(function_declaration).collect::<Result<Vec<_>, _>>()?;
            let calling = match tool_choice.str_at("type") {
                Some("none") => jobj! { "mode": "NONE" },
                Some("required") => jobj! { "mode": "ANY" },
                Some("tool") => jobj! { "mode": "ANY", "allowedFunctionNames": jarr![tool_choice.get("toolName").cloned().unwrap_or_default()] },
                _ => jobj! { "mode": "VALIDATED" },
            };
            let config = jobj! { "functionCallingConfig": calling, "includeServerSideToolInvocations"?: (!is_vertex).then_some(true) };
            google_tools.push(jobj! { "functionDeclarations": declarations });
            return Ok((Some(google_tools), Some(config)));
        }
        return Ok(((!google_tools.is_empty()).then_some(google_tools), None));
    }

    let declarations = tools.iter().filter(is_function).map(function_declaration).collect::<Result<Vec<_>, _>>()?;
    let has_strict_tools = tools.iter().any(|t| t.get("strict").and_then(Json::as_bool) == Some(true));
    let calling = match tool_choice.str_at("type") {
        Some("auto") => jobj! { "mode": if has_strict_tools { "VALIDATED" } else { "AUTO" } },
        Some("none") => jobj! { "mode": "NONE" },
        Some("required") => jobj! { "mode": "ANY" },
        Some("tool") => jobj! { "mode": "ANY", "allowedFunctionNames": jarr![tool_choice.get("toolName").cloned().unwrap_or_default()] },
        // `toolChoice == null`, which `streamText` never sends when there are tools.
        _ if has_strict_tools => jobj! { "mode": "VALIDATED" },
        _ => return Ok((Some(vec![jobj! { "functionDeclarations": declarations }]), None)),
    };
    Ok((Some(vec![jobj! { "functionDeclarations": declarations }]), Some(jobj! { "functionCallingConfig": calling })))
}

// ------------------------------------------------------------------ Prompt

struct PromptOptions {
    is_gemma: bool,
    is_gemini3: bool,
    names: &'static [&'static str],
    supports_function_response_parts: bool,
    include_function_call_ids: bool,
}

/// `String(value)`.
pub(super) fn js_string(value: &Json) -> String {
    match value {
        Json::Str(text) => text.clone(),
        Json::Obj(_) => "[object Object]".to_owned(),
        Json::Arr(items) => items.iter().map(|i| if i.is_null() { String::new() } else { js_string(i) }).collect::<Vec<_>>().join(","),
        other => other.to_text(),
    }
}

fn base64_of_text(text: &str) -> String {
    base64_simd::STANDARD.encode_to_string(text.as_bytes())
}

/// `resolveProviderReference`.
fn provider_reference(part: &Json) -> Result<Json, AiError> {
    let reference = part.path(&["data", "reference"]);
    if let Some(id) = reference.and_then(|r| r.non_null("google")) {
        return Ok(id.clone());
    }
    let available = reference.and_then(Json::as_obj).map_or_else(Vec::new, |r| r.keys().collect::<Vec<_>>()).join(", ");
    Err(AiError::other(format!("No provider reference found for provider 'google'. Available providers: {available}")))
}

/// `parseBase64DataUrl`: `^data:([^;,]+);base64,(.+)$`.
fn parse_base64_data_url(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix("data:")?;
    let end = rest.find([';', ','])?;
    let (media_type, rest) = rest.split_at(end);
    let data = rest.strip_prefix(";base64,")?;
    (!media_type.is_empty() && !data.is_empty()).then_some((media_type, data))
}

/// `appendToolResultParts`: Gemini 3 takes files inside the `functionResponse`.
fn append_tool_result_parts(parts: &mut Vec<Json>, tool_name: &Json, value: &[Json], call_id: Option<Json>) -> Result<(), AiError> {
    let mut response_parts = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for item in value {
        match item.str_at("type").unwrap_or("") {
            "text" => texts.push(item.str_at("text").unwrap_or("").to_owned()),
            "file" => match item.path(&["data", "type"]).and_then(Json::as_str) {
                Some("data") => response_parts.push(jobj! {
                    "inlineData": jobj! {
                        "mimeType": resolve_full_media_type(item)?,
                        "data": item.path(&["data", "data"]).cloned().unwrap_or_default(),
                    },
                }),
                Some("url") => {
                    // Only inline data is supported in a `functionResponse`.
                    match item.path(&["data", "url"]).and_then(Json::as_str).and_then(parse_base64_data_url) {
                        Some((media_type, data)) => response_parts.push(jobj! { "inlineData": jobj! { "mimeType": media_type, "data": data } }),
                        None => texts.push(item.to_text()),
                    }
                }
                _ => texts.push(item.to_text()),
            },
            _ => texts.push(item.to_text()),
        }
    }
    parts.push(jobj! {
        "functionResponse": jobj! {
            "id"?: call_id,
            "name": tool_name,
            "response": jobj! {
                "name": tool_name,
                "content": if texts.is_empty() { "Tool executed successfully.".to_owned() } else { texts.join("\n") },
            },
            "parts"?: (!response_parts.is_empty()).then_some(response_parts),
        },
    });
    Ok(())
}

/// `appendLegacyToolResultParts`: before Gemini 3 a file is its own top-level part.
fn append_legacy_tool_result_parts(parts: &mut Vec<Json>, tool_name: &Json, value: &[Json], call_id: Option<Json>) -> Result<(), AiError> {
    for item in value {
        match item.str_at("type").unwrap_or("") {
            "text" => parts.push(jobj! {
                "functionResponse": jobj! {
                    "id"?: call_id.clone(),
                    "name": tool_name,
                    "response": jobj! { "name": tool_name, "content": item.get("text").cloned().unwrap_or_default() },
                },
            }),
            "file" if item.path(&["data", "type"]).and_then(Json::as_str) == Some("data") => {
                let kind = if top_level_media_type(item.str_at("mediaType").unwrap_or("")) == "image" { "image" } else { "file" };
                parts.push(jobj! {
                    "inlineData": jobj! {
                        "mimeType": resolve_full_media_type(item)?,
                        "data": item.path(&["data", "data"]).cloned().unwrap_or_default(),
                    },
                });
                parts.push(jobj! { "text": format!("Tool executed successfully and returned this {kind} as a response") });
            }
            _ => parts.push(jobj! { "text": item.to_text() }),
        }
    }
    Ok(())
}

fn unsupported(functionality: &str) -> AiError {
    AiError::other(format!("'{functionality}' functionality not supported."))
}

/// `readProviderOpts`: this provider's names in order, then the other family's (gateway interop).
fn read_provider_options<'a>(part: &'a Json, names: &[&str]) -> Option<&'a Json> {
    let options = part.get("providerOptions");
    for name in names {
        if let Some(found) = options.and_then(|o| o.non_null(name)) {
            return Some(found);
        }
    }
    let options = options?;
    if names.contains(&"google") {
        options.non_null("googleVertex").or_else(|| options.non_null("vertex"))
    } else {
        options.non_null("google")
    }
}

fn string_option(options: Option<&Json>, key: &str) -> Option<String> {
    options.and_then(|o| o.non_null(key)).map(js_string)
}

/// One part of an assistant message, `None` where the map returns `undefined`.
fn assistant_part(part: &Json, is_vertex_like: bool, options: &PromptOptions, signed: &mut bool) -> Result<Option<Json>, AiError> {
    let provider_options = read_provider_options(part, options.names);
    let thought_signature = string_option(provider_options, "thoughtSignature");
    let thought_flag = (provider_options.and_then(|o| o.get("thought")).and_then(Json::as_bool) == Some(true)).then_some(true);
    let media_type = || part.get("mediaType").cloned().unwrap_or_default();
    let inline = || part.path(&["data", "data"]).cloned().unwrap_or_default();
    let data_kind = part.path(&["data", "type"]).and_then(Json::as_str).unwrap_or("");
    const FILE_URLS: &str = "File data URLs in assistant messages are not supported";
    match part.str_at("type").unwrap_or("") {
        "text" => {
            let text = part.str_at("text").unwrap_or("");
            Ok((!text.is_empty()).then(|| jobj! { "text": text, "thoughtSignature"?: thought_signature }))
        }
        "reasoning" => {
            let text = part.str_at("text").unwrap_or("");
            Ok((!text.is_empty()).then(|| jobj! { "text": text, "thought": true, "thoughtSignature"?: thought_signature }))
        }
        "reasoning-file" => match data_kind {
            "url" => Err(unsupported(FILE_URLS)),
            "data" => Ok(Some(jobj! {
                "inlineData": jobj! { "mimeType": media_type(), "data": inline() },
                "thought": true,
                "thoughtSignature"?: thought_signature,
            })),
            _ => Ok(None),
        },
        "file" => match data_kind {
            "url" => Err(unsupported(FILE_URLS)),
            "reference" => {
                if is_vertex_like {
                    return Err(unsupported("file parts with provider references"));
                }
                Ok(Some(jobj! {
                    "fileData": jobj! { "mimeType": media_type(), "fileUri": provider_reference(part)? },
                    "thought"?: thought_flag,
                    "thoughtSignature"?: thought_signature,
                }))
            }
            "text" => {
                let declared = part.str_at("mediaType").unwrap_or("");
                Ok(Some(jobj! {
                    "inlineData": jobj! {
                        "mimeType": if is_full_media_type(declared) { declared } else { "text/plain" },
                        "data": base64_of_text(part.path(&["data", "text"]).and_then(Json::as_str).unwrap_or("")),
                    },
                    "thought"?: thought_flag,
                    "thoughtSignature"?: thought_signature,
                }))
            }
            "data" => Ok(Some(jobj! {
                "inlineData": jobj! { "mimeType": media_type(), "data": inline() },
                "thought"?: thought_flag,
                "thoughtSignature"?: thought_signature,
            })),
            _ => Ok(None),
        },
        "tool-call" => {
            let server_id = string_option(provider_options, "serverToolCallId");
            let server_type = string_option(provider_options, "serverToolType");
            let is_server = server_id.is_some() && server_type.is_some();
            // Gemini 3 returns one signature for a parallel response, on its first function call; the later
            // calls legitimately have none.
            let skip_mitigation = !is_server && thought_signature.is_none() && *signed;
            let effective = thought_signature.clone().or_else(|| (options.is_gemini3 && !skip_mitigation).then(|| SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_owned()));
            if !is_server && thought_signature.is_some() {
                *signed = true;
            }
            if let (Some(id), Some(tool_type)) = (server_id, server_type) {
                let args = match part.get("input") {
                    Some(Json::Str(text)) => Some(secure_json_parse(text).map_err(AiError::other)?),
                    other => other.cloned(),
                };
                return Ok(Some(jobj! {
                    "toolCall": jobj! { "toolType": tool_type, "args"?: args, "id": id },
                    "thoughtSignature"?: effective,
                }));
            }
            let call_id = options.include_function_call_ids.then(|| part.non_null("toolCallId").cloned()).flatten();
            Ok(Some(jobj! {
                "functionCall": jobj! {
                    "id"?: call_id,
                    "name": part.get("toolName").cloned().unwrap_or_default(),
                    "args"?: part.get("input").cloned(),
                },
                "thoughtSignature"?: effective,
            }))
        }
        "tool-result" => {
            let server_id = string_option(provider_options, "serverToolCallId").filter(|s| !s.is_empty());
            let server_type = string_option(provider_options, "serverToolType").filter(|s| !s.is_empty());
            let (Some(id), Some(tool_type)) = (server_id, server_type) else { return Ok(None) };
            Ok(Some(jobj! {
                "toolResponse": jobj! { "toolType": tool_type, "response": json_output_or_empty(part), "id": id },
                "thoughtSignature"?: thought_signature,
            }))
        }
        _ => Ok(None),
    }
}

fn json_output_or_empty(part: &Json) -> Json {
    let output = part.get("output");
    if output.and_then(|o| o.str_at("type")) == Some("json") {
        output.and_then(|o| o.get("value")).cloned().unwrap_or_default()
    } else {
        Json::obj()
    }
}

/// `convertToGoogleMessages`: the `contents` and the `systemInstruction`.
fn convert_to_google_messages(prompt: &[Json], options: &PromptOptions) -> Result<(Vec<Json>, Option<Json>), AiError> {
    let mut system_parts: Vec<Json> = Vec::new();
    let mut contents: Vec<Json> = Vec::new();
    let mut system_allowed = true;
    let is_vertex_like = !options.names.contains(&"google");
    for message in prompt {
        let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        match message.str_at("role").unwrap_or("") {
            "system" => {
                if !system_allowed {
                    return Err(unsupported("system messages are only supported at the beginning of the conversation"));
                }
                system_parts.push(jobj! { "text": message.get("content").cloned().unwrap_or_default() });
            }
            "user" => {
                system_allowed = false;
                let mut parts = Vec::with_capacity(content.len());
                for part in content {
                    match part.str_at("type").unwrap_or("") {
                        "text" => parts.push(jobj! { "text": part.get("text").cloned().unwrap_or_default() }),
                        "file" => match part.path(&["data", "type"]).and_then(Json::as_str).unwrap_or("") {
                            "url" => parts.push(jobj! {
                                "fileData": jobj! {
                                    "mimeType": resolve_full_media_type(part)?,
                                    "fileUri": part.path(&["data", "url"]).cloned().unwrap_or_default(),
                                },
                            }),
                            "reference" => {
                                if is_vertex_like {
                                    return Err(unsupported("file parts with provider references"));
                                }
                                parts.push(jobj! {
                                    "fileData": jobj! { "mimeType": resolve_full_media_type(part)?, "fileUri": provider_reference(part)? },
                                });
                            }
                            "text" => {
                                let declared = part.str_at("mediaType").unwrap_or("");
                                parts.push(jobj! {
                                    "inlineData": jobj! {
                                        "mimeType": if is_full_media_type(declared) { declared } else { "text/plain" },
                                        "data": base64_of_text(part.path(&["data", "text"]).and_then(Json::as_str).unwrap_or("")),
                                    },
                                });
                            }
                            "data" => parts.push(jobj! {
                                "inlineData": jobj! {
                                    "mimeType": resolve_full_media_type(part)?,
                                    "data": part.path(&["data", "data"]).cloned().unwrap_or_default(),
                                },
                            }),
                            _ => {}
                        },
                        _ => {}
                    }
                }
                contents.push(jobj! { "role": "user", "parts": parts });
            }
            "assistant" => {
                system_allowed = false;
                let mut signed = false;
                let mut parts = Vec::with_capacity(content.len());
                for part in content {
                    if let Some(converted) = assistant_part(part, is_vertex_like, options, &mut signed)? {
                        parts.push(converted);
                    }
                }
                contents.push(jobj! { "role": "model", "parts": parts });
            }
            "tool" => {
                system_allowed = false;
                let mut parts = Vec::with_capacity(content.len());
                for part in content {
                    if part.str_at("type") == Some("tool-approval-response") {
                        continue;
                    }
                    let provider_options = read_provider_options(part, options.names);
                    let server_id = string_option(provider_options, "serverToolCallId").filter(|s| !s.is_empty());
                    let server_type = string_option(provider_options, "serverToolType").filter(|s| !s.is_empty());
                    if let (Some(id), Some(tool_type)) = (server_id, server_type) {
                        // A server tool's response belongs to the model turn before it.
                        if let Some(last) = contents.last_mut().filter(|c| c.str_at("role") == Some("model")) {
                            let response = jobj! {
                                "toolResponse": jobj! { "toolType": tool_type, "response": json_output_or_empty(part), "id": id },
                                "thoughtSignature"?: string_option(provider_options, "thoughtSignature"),
                            };
                            if let Some(list) = last.get_mut("parts").and_then(Json::as_arr_mut) {
                                list.push(response);
                            }
                            continue;
                        }
                    }
                    let tool_name = part.get("toolName").cloned().unwrap_or_default();
                    let call_id = options.include_function_call_ids.then(|| part.non_null("toolCallId").cloned()).flatten();
                    let output = part.get("output").unwrap_or(&Json::Null);
                    match output.str_at("type").unwrap_or("") {
                        "content" => {
                            let value = output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
                            if options.supports_function_response_parts {
                                append_tool_result_parts(&mut parts, &tool_name, value, call_id)?;
                            } else {
                                append_legacy_tool_result_parts(&mut parts, &tool_name, value, call_id)?;
                            }
                        }
                        kind => {
                            let result = if kind == "execution-denied" {
                                Some(output.non_null("reason").cloned().unwrap_or_else(|| Json::from("Tool call execution denied.")))
                            } else {
                                output.get("value").cloned()
                            };
                            parts.push(jobj! {
                                "functionResponse": jobj! {
                                    "id"?: call_id,
                                    "name": &tool_name,
                                    "response": jobj! { "name": &tool_name, "content"?: result },
                                },
                            });
                        }
                    }
                }
                contents.push(jobj! { "role": "user", "parts": parts });
            }
            _ => {}
        }
    }
    if options.is_gemma
        && !system_parts.is_empty()
        && let Some(parts) = contents.first_mut().filter(|c| c.str_at("role") == Some("user")).and_then(|first| first.get_mut("parts")).and_then(Json::as_arr_mut)
    {
        let text = system_parts.iter().map(|p| p.str_at("text").unwrap_or("")).collect::<Vec<_>>().join("\n\n");
        parts.insert(0, jobj! { "text": format!("{text}\n\n") });
    }
    let system_instruction = (!system_parts.is_empty() && !options.is_gemma).then(|| jobj! { "parts": system_parts });
    Ok((contents, system_instruction))
}

// ------------------------------------------------------------------ Request

struct Args {
    body: Json,
    names: &'static [&'static str],
    extra_headers: Headers,
}

const GOOGLE_NAMES: &[&str] = &["google"];
const VERTEX_NAMES: &[&str] = &["googleVertex", "vertex"];

impl GoogleLanguageModel {
    /// `getArgs`.
    fn get_args(&self, options: &CallOptions) -> Result<Args, AiError> {
        let names = if self.provider.contains("vertex") { VERTEX_NAMES } else { GOOGLE_NAMES };
        let mut google_options = None;
        for name in names {
            google_options = parse_provider_options(options, name)?;
            if google_options.is_some() {
                break;
            }
        }
        // A Vertex provider may receive its options under the `google` key.
        if google_options.is_none() && !names.contains(&"google") {
            google_options = parse_provider_options(options, "google")?;
        }
        let google_options = google_options.unwrap_or_default();
        let is_vertex = self.provider.starts_with("google.vertex.");

        let mut extra_headers = Headers::new();
        if is_vertex {
            if let Some(value) = &google_options.shared_request_type {
                extra_headers.set("x-vertex-ai-llm-shared-request-type", value);
            }
            if let Some(value) = &google_options.request_type {
                extra_headers.set("x-vertex-ai-llm-request-type", value);
            }
        }
        // `personGeneration`, `prominentPeople` and `imageOutputOptions` are Vertex-only; the Gemini API rejects them.
        let mut image_config = google_options.image_config;
        if let (Some(config), false) = (&mut image_config, is_vertex) {
            for key in ["personGeneration", "prominentPeople", "imageOutputOptions"] {
                config.remove(key);
            }
        }

        let is_gemma = self.model_id.to_lowercase().starts_with("gemma-");
        let uses_gemini3 = capabilities(&self.model_id).uses_gemini3_features;
        let (contents, system_instruction) = convert_to_google_messages(
            &options.prompt,
            &PromptOptions {
                is_gemma,
                is_gemini3: uses_gemini3,
                names,
                supports_function_response_parts: uses_gemini3,
                include_function_call_ids: !is_vertex,
            },
        )?;
        let (tools, google_tool_config) = prepare_tools(options.tools.as_ref(), &options.tool_choice, &self.model_id, is_vertex)?;

        let resolved_thinking = resolve_thinking_config(options.reasoning.as_deref(), &self.model_id);
        let thinking_config = if google_options.thinking_config.is_some() || resolved_thinking.is_some() {
            let mut merged = resolved_thinking.unwrap_or_default();
            if let Some(config) = &google_options.thinking_config {
                merged.extend_from(config);
            }
            Some(merged)
        } else {
            None
        };

        let stream_function_call_arguments = is_vertex && google_options.stream_function_call_arguments == Some(true);
        let safety_settings = google_options.safety_settings.or_else(|| {
            google_options.threshold.as_ref().map(|threshold| {
                Json::Arr(
                    ["HARM_CATEGORY_HATE_SPEECH", "HARM_CATEGORY_DANGEROUS_CONTENT", "HARM_CATEGORY_HARASSMENT", "HARM_CATEGORY_SEXUALLY_EXPLICIT"]
                        .iter()
                        .map(|category| jobj! { "category": *category, "threshold": threshold })
                        .collect(),
                )
            })
        });
        let tool_config = if google_tool_config.is_some() || stream_function_call_arguments || google_options.retrieval_config.is_some() {
            let mut config = google_tool_config.as_ref().and_then(Json::as_obj).cloned().unwrap_or_default();
            if stream_function_call_arguments {
                let mut calling = config.get("functionCallingConfig").and_then(Json::as_obj).cloned().unwrap_or_default();
                calling.insert("streamFunctionCallArguments", true);
                config.insert("functionCallingConfig", calling);
            }
            if let Some(retrieval) = google_options.retrieval_config {
                config.insert("retrievalConfig", retrieval);
            }
            Some(config)
        } else {
            None
        };

        let body = jobj! {
            "generationConfig": jobj! {
                "maxOutputTokens"?: options.max_output_tokens,
                "audioTimestamp"?: google_options.audio_timestamp.filter(|on| *on),
                "responseModalities"?: google_options.response_modalities,
                "thinkingConfig"?: thinking_config,
                "mediaResolution"?: google_options.media_resolution,
                "imageConfig"?: image_config,
            },
            "contents": contents,
            "systemInstruction"?: if is_gemma { None } else { system_instruction },
            "safetySettings"?: safety_settings,
            "tools"?: tools,
            "toolConfig"?: tool_config,
            "cachedContent"?: google_options.cached_content,
            "labels"?: google_options.labels,
            "serviceTier"?: if is_vertex { None } else { google_options.service_tier },
        };
        Ok(Args { body, names, extra_headers })
    }

    /// `doStream`.
    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let args = self.get_args(&options)?;
        let headers = combine_headers(&self.headers.resolve(&options.cancel).await?, &args.extra_headers);
        // `getModelPath`.
        let path = if self.model_id.contains('/') { self.model_id.clone() } else { format!("models/{}", self.model_id) };
        let url = format!("{}/{path}:streamGenerateContent?alt=sse", self.base_url);
        let response = post_json(&self.fetch, &url, headers, &args.body, &options.cancel, ERROR_SHAPE).await?;
        Ok(ModelStream::sse(response.body, Box::new(StreamState::new(args.names))))
    }
}

/// `googleFailedResponseHandler`, whose schema wants all of `code`, `message` and `status`.
const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| {
        let error = body.get("error")?;
        if !error.get("code").is_some_and(|c| c.is_null() || c.is_num()) || error.str_at("status").is_none() {
            return None;
        }
        if !error.get("details").is_none_or(|d| d.is_null() || d.is_arr()) {
            return None;
        }
        error.str_at("message").map(str::to_owned)
    },
    is_retryable: None,
};

impl LanguageModel for GoogleLanguageModel {
    fn provider(&self) -> &str {
        self.provider
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> SupportedUrls {
        self.supported_urls.clone()
    }

    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(self.stream(options))
    }
}

// ------------------------------------------------------------------ Streaming tool arguments

#[derive(Clone, Debug, PartialEq)]
enum Segment {
    Key(String),
    Index(usize),
}

struct StackEntry {
    segment: Segment,
    is_array: bool,
    child_count: usize,
}

/// Anything larger is a broken or hostile upstream, not an argument list: JavaScript's sparse arrays would
/// shrug it off, a dense `Vec` would not.
const MAX_ARRAY_INDEX: usize = 1 << 20;

/// `GoogleJSONAccumulator`: builds the arguments from `partialArgs` and a running JSON text whose deltas
/// concatenate to what `JSON.stringify` gives.
struct JsonAccumulator {
    accumulated: Json,
    json_text: String,
    path_stack: Vec<StackEntry>,
    /// A string value whose closing quote has not been emitted yet.
    string_open: bool,
}

/// `parsePath`: `recipe.ingredients[0].name`.
fn parse_path(raw: &str) -> Vec<Segment> {
    static INDEX: OnceLock<Regex> = OnceLock::new();
    let mut segments = Vec::new();
    for part in raw.split('.') {
        match part.find('[') {
            None => segments.push(Segment::Key(part.to_owned())),
            Some(at) => {
                if at > 0 {
                    segments.push(Segment::Key(part[..at].to_owned()));
                }
                for found in pattern(&INDEX, r"\[(\d+)\]").captures_iter(part) {
                    segments.push(Segment::Index(found[1].parse().unwrap_or(usize::MAX)));
                }
            }
        }
    }
    segments
}

fn nested_value<'a>(root: &'a Json, segments: &[Segment]) -> Option<&'a Json> {
    let mut current = root;
    for segment in segments {
        current = match (current, segment) {
            (Json::Obj(object), Segment::Key(key)) => object.get(key)?,
            (Json::Obj(object), Segment::Index(index)) => object.get(&index.to_string())?,
            // A hole reads as `undefined`, and holes are all a `null` element can be here in practice.
            (Json::Arr(items), Segment::Index(index)) => items.get(*index).filter(|item| !item.is_null())?,
            _ => return None,
        };
    }
    Some(current)
}

fn child_slot<'a>(parent: &'a mut Json, segment: &Segment) -> Option<&'a mut Json> {
    match (parent, segment) {
        (Json::Obj(object), Segment::Key(key)) => {
            if !object.contains_key(key) {
                object.insert(key.as_str(), Json::Null);
            }
            object.get_mut(key)
        }
        (Json::Obj(object), Segment::Index(index)) => {
            let key = index.to_string();
            if !object.contains_key(&key) {
                object.insert(key.as_str(), Json::Null);
            }
            object.get_mut(&key)
        }
        (Json::Arr(items), Segment::Index(index)) => {
            if items.len() <= *index {
                items.resize(*index + 1, Json::Null);
            }
            items.get_mut(*index)
        }
        _ => None,
    }
}

/// `setNestedValue`.
fn set_nested_value(root: &mut Json, segments: &[Segment], value: Json) {
    let Some((last, parents)) = segments.split_last() else { return };
    let mut current = root;
    for (at, segment) in parents.iter().enumerate() {
        let Some(slot) = child_slot(current, segment) else { return };
        if slot.is_null() {
            *slot = if matches!(segments[at + 1], Segment::Index(_)) { Json::Arr(Vec::new()) } else { Json::obj() };
        }
        current = slot;
    }
    if let Some(slot) = child_slot(current, last) {
        *slot = value;
    }
}

/// `existingValue + arg.stringValue`.
fn js_concat(existing: &Json, more: &str) -> Json {
    let head = match existing {
        Json::Null => "null".to_owned(),
        other => js_string(other),
    };
    Json::from(head + more)
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text.slice(units)`, counting UTF-16 code units.
fn utf16_slice_from(text: &str, units: usize) -> &str {
    let mut seen = 0;
    for (at, c) in text.char_indices() {
        if seen >= units {
            return &text[at..];
        }
        seen += c.len_utf16();
    }
    ""
}

impl JsonAccumulator {
    fn new() -> Self {
        Self { accumulated: Json::obj(), json_text: String::new(), path_stack: Vec::new(), string_open: false }
    }

    /// `processPartialArgs`: the text delta.
    fn process(&mut self, partial_args: &[Json]) -> String {
        let mut delta = String::new();
        for arg in partial_args {
            let path = arg.str_at("jsonPath").unwrap_or("");
            let raw = path.strip_prefix("$.").unwrap_or(path);
            if raw.is_empty() {
                continue;
            }
            let segments = parse_path(raw);
            if segments.iter().any(|s| matches!(s, Segment::Index(i) if *i > MAX_ARRAY_INDEX)) {
                continue;
            }
            let string_value = arg.non_null("stringValue").and_then(Json::as_str);
            if let (Some(text), Some(existing)) = (string_value, nested_value(&self.accumulated, &segments)) {
                let joined = js_concat(existing, text);
                set_nested_value(&mut self.accumulated, &segments, joined);
                let escaped = Json::from(text).to_text();
                delta.push_str(&escaped[1..escaped.len() - 1]);
                continue;
            }
            // `resolvePartialArgValue`.
            let value = arg.non_null("stringValue").or_else(|| arg.non_null("numberValue")).or_else(|| arg.non_null("boolValue"));
            let value = match value {
                Some(value) => value.clone(),
                None if arg.get("nullValue").is_some() => Json::Null,
                None => continue,
            };
            let json = value.to_text();
            set_nested_value(&mut self.accumulated, &segments, value);
            let open_string = string_value.is_some() && arg.get("willContinue").and_then(Json::as_bool) == Some(true);
            delta.push_str(&self.emit_navigation_to(&segments, open_string, &json));
        }
        self.json_text.push_str(&delta);
        delta
    }

    /// `finalize`: the final JSON and the delta that closes what is open.
    fn finalize(&self) -> (String, String) {
        let final_args = self.accumulated.to_text();
        let closing = utf16_slice_from(&final_args, utf16_len(&self.json_text)).to_owned();
        (final_args, closing)
    }

    /// `emitNavigationTo`.
    fn emit_navigation_to(&mut self, target: &[Segment], open_string: bool, value_json: &str) -> String {
        let mut fragment = String::new();
        if self.string_open {
            fragment.push('"');
            self.string_open = false;
        }
        if self.path_stack.is_empty() {
            self.path_stack.push(StackEntry { segment: Segment::Key(String::new()), is_array: false, child_count: 0 });
            fragment.push('{');
        }
        let Some((leaf, containers)) = target.split_last() else { return fragment };
        // `findCommonStackDepth`: the root is never popped.
        let mut common = 0;
        while common < containers.len() && common + 1 < self.path_stack.len() && self.path_stack[common + 1].segment == containers[common] {
            common += 1;
        }
        // `closeDownTo`.
        while self.path_stack.len() > common + 1 {
            if let Some(entry) = self.path_stack.pop() {
                fragment.push(if entry.is_array { ']' } else { '}' });
            }
        }
        // `openDownTo`.
        for at in self.path_stack.len() - 1..containers.len() {
            let parent = self.path_stack.last_mut().expect("the root is on the stack");
            if parent.child_count > 0 {
                fragment.push(',');
            }
            parent.child_count += 1;
            if let Segment::Key(key) = &containers[at] {
                fragment.push_str(&Json::from(key.as_str()).to_text());
                fragment.push(':');
            }
            let child = containers.get(at + 1).unwrap_or(leaf);
            let is_array = matches!(child, Segment::Index(_));
            fragment.push(if is_array { '[' } else { '{' });
            self.path_stack.push(StackEntry { segment: containers[at].clone(), is_array, child_count: 0 });
        }
        // `emitLeaf`.
        let container = self.path_stack.last_mut().expect("the root is on the stack");
        if container.child_count > 0 {
            fragment.push(',');
        }
        container.child_count += 1;
        if let Segment::Key(key) = leaf {
            fragment.push_str(&Json::from(key.as_str()).to_text());
            fragment.push(':');
        }
        if open_string {
            fragment.push_str(&value_json[..value_json.len() - 1]);
            self.string_open = true;
        } else {
            fragment.push_str(value_json);
        }
        fragment
    }
}

// ------------------------------------------------------------------ Chunk schema

fn nullish(object: &Json, key: &str, check: impl Fn(&Json) -> bool) -> bool {
    object.non_null(key).is_none_or(check)
}

fn nullish_str(object: &Json, key: &str) -> bool {
    nullish(object, key, Json::is_str)
}

fn nullish_num(object: &Json, key: &str) -> bool {
    nullish(object, key, Json::is_num)
}

fn nullish_bool(object: &Json, key: &str) -> bool {
    nullish(object, key, |v| v.as_bool().is_some())
}

fn nullish_arr(object: &Json, key: &str, check: impl Fn(&Json) -> bool) -> bool {
    nullish(object, key, |v| v.as_arr().is_some_and(|items| items.iter().all(&check)))
}

fn all_nullish_str(object: &Json, keys: &[&str]) -> bool {
    object.is_obj() && keys.iter().all(|key| nullish_str(object, key))
}

fn signature_ok(part: &Json) -> bool {
    nullish_str(part, "thoughtSignature")
}

fn partial_arg_ok(arg: &Json) -> bool {
    arg.str_at("jsonPath").is_some() && nullish_str(arg, "stringValue") && nullish_num(arg, "numberValue") && nullish_bool(arg, "boolValue") && nullish_bool(arg, "willContinue")
}

/// The first option of the part union that takes this part; `None` fails the chunk. Order matters: the last
/// option has only optional keys, so a malformed `functionCall` falls through to it and does nothing.
enum Part<'a> {
    FunctionCall(&'a Json),
    InlineData(&'a Json),
    ToolCall(&'a Json),
    ToolResponse(&'a Json),
    Plain,
}

fn part_kind(part: &Json) -> Option<Part<'_>> {
    if !part.is_obj() {
        return None;
    }
    if let Some(call) = part.get("functionCall").filter(|c| c.is_obj()) {
        let shape = all_nullish_str(call, &["id", "name"]) && nullish_arr(call, "partialArgs", partial_arg_ok) && nullish_bool(call, "willContinue");
        if shape && signature_ok(part) {
            return Some(Part::FunctionCall(call));
        }
    }
    if let Some(data) = part.get("inlineData")
        && data.str_at("mimeType").is_some()
        && data.str_at("data").is_some()
        && nullish_bool(part, "thought")
        && signature_ok(part)
    {
        return Some(Part::InlineData(data));
    }
    if let Some(call) = part.get("toolCall")
        && call.str_at("toolType").is_some()
        && call.str_at("id").is_some()
        && signature_ok(part)
    {
        return Some(Part::ToolCall(call));
    }
    if let Some(response) = part.get("toolResponse")
        && response.str_at("toolType").is_some()
        && response.str_at("id").is_some()
        && signature_ok(part)
    {
        return Some(Part::ToolResponse(response));
    }
    let plain = nullish(part, "executableCode", |c| c.str_at("language").is_some() && c.str_at("code").is_some())
        && nullish(part, "codeExecutionResult", |r| r.str_at("outcome").is_some() && nullish_str(r, "output"))
        && nullish_str(part, "text")
        && nullish_bool(part, "thought")
        && signature_ok(part);
    plain.then_some(Part::Plain)
}

fn safety_rating_ok(rating: &Json) -> bool {
    all_nullish_str(rating, &["category", "probability", "severity"]) && nullish_num(rating, "probabilityScore") && nullish_num(rating, "severityScore") && nullish_bool(rating, "blocked")
}

fn string_list_ok(object: &Json, key: &str) -> bool {
    nullish_arr(object, key, Json::is_str)
}

fn number_list_ok(object: &Json, key: &str) -> bool {
    nullish_arr(object, key, Json::is_num)
}

fn grounding_chunk_ok(chunk: &Json) -> bool {
    chunk.is_obj()
        && nullish(chunk, "web", |web| web.str_at("uri").is_some() && nullish_str(web, "title"))
        && nullish(chunk, "image", |image| image.str_at("sourceUri").is_some() && image.str_at("imageUri").is_some() && all_nullish_str(image, &["title", "domain"]))
        && nullish(chunk, "retrievedContext", |context| all_nullish_str(context, &["uri", "title", "text", "fileSearchStore"]))
        && nullish(chunk, "maps", |maps| all_nullish_str(maps, &["uri", "title", "text", "placeId"]))
}

fn grounding_support_ok(support: &Json) -> bool {
    support.is_obj()
        && nullish(support, "segment", |segment| segment.is_obj() && nullish_num(segment, "startIndex") && nullish_num(segment, "endIndex") && nullish_str(segment, "text"))
        && nullish_str(support, "segment_text")
        && number_list_ok(support, "groundingChunkIndices")
        && number_list_ok(support, "supportChunkIndices")
        && number_list_ok(support, "confidenceScores")
        && number_list_ok(support, "confidenceScore")
}

fn grounding_metadata_ok(metadata: &Json) -> bool {
    metadata.is_obj()
        && string_list_ok(metadata, "webSearchQueries")
        && string_list_ok(metadata, "imageSearchQueries")
        && string_list_ok(metadata, "retrievalQueries")
        && nullish(metadata, "searchEntryPoint", |entry| entry.str_at("renderedContent").is_some())
        && nullish_arr(metadata, "groundingChunks", grounding_chunk_ok)
        && nullish_arr(metadata, "groundingSupports", grounding_support_ok)
        && nullish(metadata, "retrievalMetadata", Json::is_obj)
}

fn usage_ok(usage: &Json) -> bool {
    let details_ok = |detail: &Json| detail.str_at("modality").is_some() && detail.get("tokenCount").is_some_and(Json::is_num);
    usage.is_obj()
        && ["cachedContentTokenCount", "thoughtsTokenCount", "promptTokenCount", "candidatesTokenCount", "totalTokenCount"].iter().all(|key| nullish_num(usage, key))
        && all_nullish_str(usage, &["trafficType", "serviceTier"])
        && nullish_arr(usage, "promptTokensDetails", details_ok)
        && nullish_arr(usage, "candidatesTokensDetails", details_ok)
}

fn candidate_ok(candidate: &Json) -> bool {
    candidate.is_obj()
        && nullish(candidate, "content", |content| content.is_obj() && nullish_arr(content, "parts", |part| part_kind(part).is_some()))
        && all_nullish_str(candidate, &["finishReason", "finishMessage"])
        && nullish_arr(candidate, "safetyRatings", safety_rating_ok)
        && nullish(candidate, "groundingMetadata", grounding_metadata_ok)
        && nullish(candidate, "urlContextMetadata", |context| {
            context.is_obj() && nullish_arr(context, "urlMetadata", |item| item.str_at("retrievedUrl").is_some() && item.str_at("urlRetrievalStatus").is_some())
        })
}

/// `chunkSchema`, without Zod's wording of the failure.
fn chunk_ok(value: &Json) -> bool {
    value.is_obj()
        && nullish_str(value, "responseId")
        && nullish_arr(value, "candidates", candidate_ok)
        && nullish(value, "usageMetadata", usage_ok)
        && nullish(value, "promptFeedback", |feedback| feedback.is_obj() && nullish_str(feedback, "blockReason") && nullish_arr(feedback, "safetyRatings", safety_rating_ok))
}

// ------------------------------------------------------------------ Stream

/// `mapGoogleFinishReason`.
fn map_finish_reason(reason: &str, has_tool_calls: bool) -> &'static str {
    match reason {
        "STOP" => {
            if has_tool_calls {
                "tool-calls"
            } else {
                "stop"
            }
        }
        "MAX_TOKENS" => "length",
        "IMAGE_SAFETY" | "RECITATION" | "SAFETY" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => "content-filter",
        "MALFORMED_FUNCTION_CALL" => "error",
        _ => "other",
    }
}

/// `convertGoogleUsage`.
fn convert_usage(usage: Option<&Json>) -> ModelUsage {
    let Some(usage) = usage else { return ModelUsage::default() };
    let count = |key: &str| usage.non_null(key).and_then(Json::as_u64).unwrap_or(0);
    let (prompt, candidates, cached, thoughts) = (count("promptTokenCount"), count("candidatesTokenCount"), count("cachedContentTokenCount"), count("thoughtsTokenCount"));
    ModelUsage {
        input_total: Some(prompt),
        input_no_cache: Some(prompt.saturating_sub(cached)),
        input_cache_read: Some(cached),
        input_cache_write: None,
        output_total: Some(candidates + thoughts),
        output_text: Some(candidates),
        output_reasoning: Some(thoughts),
        raw: Some(usage.clone()),
    }
}

/// `extractSources`, for the sources a stream emits: only URL ones, so the document branch is not built.
fn url_sources(grounding: Option<&Json>) -> Vec<(&str, Option<&Json>)> {
    let mut out = Vec::new();
    for chunk in grounding.and_then(|g| g.non_null("groundingChunks")).and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        let source = if let Some(web) = chunk.non_null("web") {
            web.str_at("uri").map(|uri| (uri, web.non_null("title")))
        } else if let Some(image) = chunk.non_null("image") {
            // Google requires attribution to the source URI, not the image's.
            image.str_at("sourceUri").map(|uri| (uri, image.non_null("title")))
        } else if let Some(context) = chunk.non_null("retrievedContext") {
            context.str_at("uri").filter(|uri| uri.starts_with("http://") || uri.starts_with("https://")).map(|uri| (uri, context.non_null("title")))
        } else if let Some(maps) = chunk.non_null("maps") {
            maps.str_at("uri").filter(|uri| !uri.is_empty()).map(|uri| (uri, maps.non_null("title")))
        } else {
            None
        };
        out.extend(source);
    }
    out
}

struct ActiveToolCall {
    tool_call_id: String,
    tool_name: String,
    accumulator: JsonAccumulator,
    provider_metadata: Option<Json>,
}

struct StreamState {
    names: &'static [&'static str],
    finish_reason: FinishReason,
    usage: Option<Json>,
    provider_metadata: Option<Json>,
    last_grounding_metadata: Json,
    last_url_context_metadata: Json,
    has_tool_calls: bool,
    has_emitted_response_metadata: bool,
    current_text_block: Option<String>,
    current_reasoning_block: Option<String>,
    block_counter: u64,
    emitted_source_urls: HashSet<String>,
    last_code_execution_call: Option<String>,
    last_server_tool_call: Option<String>,
    active_tool_calls: Vec<ActiveToolCall>,
}

fn truthy_str<'a>(object: &'a Json, key: &str) -> Option<&'a str> {
    object.str_at(key).filter(|text| !text.is_empty())
}

impl StreamState {
    fn new(names: &'static [&'static str]) -> Self {
        Self {
            names,
            finish_reason: FinishReason::other(),
            usage: None,
            provider_metadata: None,
            last_grounding_metadata: Json::Null,
            last_url_context_metadata: Json::Null,
            has_tool_calls: false,
            has_emitted_response_metadata: false,
            current_text_block: None,
            current_reasoning_block: None,
            block_counter: 0,
            emitted_source_urls: HashSet::new(),
            last_code_execution_call: None,
            last_server_tool_call: None,
            active_tool_calls: Vec::new(),
        }
    }

    /// `wrapProviderMetadata`: Vertex writes under both of its names.
    fn wrap(&self, payload: Json) -> Json {
        let mut out = Obj::new();
        for name in self.names {
            out.insert(*name, payload.clone());
        }
        Json::Obj(out)
    }

    fn signature_metadata(&self, part: &Json) -> Option<Json> {
        truthy_str(part, "thoughtSignature").map(|signature| self.wrap(jobj! { "thoughtSignature": signature }))
    }

    fn server_metadata(&self, part: &Json, call_id: &str, tool_type: &str) -> Json {
        self.wrap(jobj! {
            "thoughtSignature"?: truthy_str(part, "thoughtSignature"),
            "serverToolCallId": call_id,
            "serverToolType": tool_type,
        })
    }

    fn next_block(&mut self) -> String {
        let id = self.block_counter.to_string();
        self.block_counter += 1;
        id
    }

    fn end_text(&mut self, out: &mut Vec<StreamPart>) {
        if let Some(id) = self.current_text_block.take() {
            out.push(StreamPart::TextEnd { id, provider_metadata: None });
        }
    }

    fn end_reasoning(&mut self, out: &mut Vec<StreamPart>) {
        if let Some(id) = self.current_reasoning_block.take() {
            out.push(StreamPart::ReasoningEnd { id, provider_metadata: None });
        }
    }

    fn on_text(&mut self, part: &Json, out: &mut Vec<StreamPart>) {
        let Some(text) = part.non_null("text").and_then(Json::as_str) else { return };
        let metadata = self.signature_metadata(part);
        if text.is_empty() {
            // A signature can arrive on an empty text part after the text it signs.
            if let (Some(metadata), Some(id)) = (metadata, &self.current_text_block) {
                out.push(StreamPart::TextDelta { id: id.clone(), delta: String::new(), provider_metadata: Some(metadata) });
            }
        } else if part.get("thought").and_then(Json::as_bool) == Some(true) {
            self.end_text(out);
            let id = match &self.current_reasoning_block {
                Some(id) => id.clone(),
                None => {
                    let id = self.next_block();
                    self.current_reasoning_block = Some(id.clone());
                    out.push(StreamPart::ReasoningStart { id: id.clone(), provider_metadata: metadata.clone() });
                    id
                }
            };
            out.push(StreamPart::ReasoningDelta { id, delta: text.to_owned(), provider_metadata: metadata });
        } else {
            self.end_reasoning(out);
            let id = match &self.current_text_block {
                Some(id) => id.clone(),
                None => {
                    let id = self.next_block();
                    self.current_text_block = Some(id.clone());
                    out.push(StreamPart::TextStart { id: id.clone(), provider_metadata: metadata.clone() });
                    id
                }
            };
            out.push(StreamPart::TextDelta { id, delta: text.to_owned(), provider_metadata: metadata });
        }
    }

    fn on_ordered_part(&mut self, part: &Json, out: &mut Vec<StreamPart>) {
        match part_kind(part) {
            Some(Part::InlineData(data)) => {
                // Relevant for multimodal output models.
                self.end_text(out);
                self.end_reasoning(out);
                let media_type = data.str_at("mimeType").unwrap_or("").to_owned();
                let file_data = jobj! { "type": "data", "data": data.get("data").cloned().unwrap_or_default() };
                let provider_metadata = self.signature_metadata(part);
                if part.get("thought").and_then(Json::as_bool) == Some(true) {
                    out.push(StreamPart::ReasoningFile { media_type, data: file_data, provider_metadata });
                } else {
                    out.push(StreamPart::File { media_type, data: file_data, provider_metadata });
                }
            }
            Some(Part::ToolCall(call)) => {
                let id = truthy_str(call, "id").map_or_else(generate_id, str::to_owned);
                self.last_server_tool_call = Some(id.clone());
                let tool_type = call.str_at("toolType").unwrap_or("");
                out.push(StreamPart::ToolCall {
                    tool_call_id: id.clone(),
                    tool_name: format!("server:{tool_type}"),
                    input: call.non_null("args").map_or_else(|| "{}".to_owned(), Json::to_text),
                    provider_executed: Some(true),
                    dynamic: Some(true),
                    provider_metadata: Some(self.server_metadata(part, &id, tool_type)),
                });
            }
            Some(Part::ToolResponse(response)) => {
                let id = self.last_server_tool_call.take().or_else(|| truthy_str(response, "id").map(str::to_owned)).unwrap_or_else(generate_id);
                let tool_type = response.str_at("toolType").unwrap_or("");
                out.push(StreamPart::ToolResult {
                    tool_call_id: id.clone(),
                    tool_name: format!("server:{tool_type}"),
                    result: response.non_null("response").cloned().unwrap_or_else(Json::obj),
                    is_error: None,
                    dynamic: None,
                    provider_metadata: Some(self.server_metadata(part, &id, tool_type)),
                });
            }
            Some(Part::Plain) => {
                if let Some(code) = part.non_null("executableCode").filter(|c| truthy_str(c, "code").is_some()) {
                    let id = generate_id();
                    self.last_code_execution_call = Some(id.clone());
                    // `executableCode` after its schema: `language`, then `code`.
                    let input = jobj! { "language": code.get("language").cloned().unwrap_or_default(), "code": code.get("code").cloned().unwrap_or_default() };
                    out.push(StreamPart::ToolCall {
                        tool_call_id: id,
                        tool_name: "code_execution".to_owned(),
                        input: input.to_text(),
                        provider_executed: Some(true),
                        dynamic: None,
                        provider_metadata: None,
                    });
                } else if let Some(result) = part.non_null("codeExecutionResult") {
                    // Results belong to the most recent executable code part.
                    if let Some(id) = &self.last_code_execution_call {
                        out.push(StreamPart::ToolResult {
                            tool_call_id: id.clone(),
                            tool_name: "code_execution".to_owned(),
                            result: jobj! {
                                "outcome": result.get("outcome").cloned().unwrap_or_default(),
                                "output": result.non_null("output").cloned().unwrap_or_else(|| Json::from("")),
                            },
                            is_error: None,
                            dynamic: None,
                            provider_metadata: None,
                        });
                    }
                } else {
                    self.on_text(part, out);
                }
            }
            // Handled in the second loop, after everything else in the chunk.
            Some(Part::FunctionCall(_)) | None => {}
        }
    }

    /// `finishActiveStreamingToolCall`.
    fn finish_active_call(&mut self, out: &mut Vec<StreamPart>) {
        let Some(active) = self.active_tool_calls.pop() else { return };
        let (final_json, closing) = active.accumulator.finalize();
        if !closing.is_empty() {
            out.push(StreamPart::ToolInputDelta { id: active.tool_call_id.clone(), delta: closing });
        }
        out.push(StreamPart::ToolInputEnd { id: active.tool_call_id.clone() });
        out.push(StreamPart::ToolCall {
            tool_call_id: active.tool_call_id,
            tool_name: active.tool_name,
            input: final_json,
            provider_executed: None,
            dynamic: None,
            provider_metadata: active.provider_metadata,
        });
        self.has_tool_calls = true;
    }

    fn on_function_call(&mut self, part: &Json, call: &Json, out: &mut Vec<StreamPart>) {
        let metadata = self.signature_metadata(part);
        let name = call.non_null("name").and_then(Json::as_str);
        let args = call.non_null("args");
        let partial_args = call.non_null("partialArgs").and_then(Json::as_arr);
        let will_continue = call.non_null("willContinue").and_then(Json::as_bool);
        let call_id = || truthy_str(call, "id").map_or_else(generate_id, str::to_owned);
        let args_done = |partial: &[Json]| will_continue != Some(true) && partial.iter().all(|arg| arg.get("willContinue").and_then(Json::as_bool) != Some(true));

        if partial_args.is_some() || (name.is_some() && will_continue == Some(true)) {
            if let Some(name) = name {
                let tool_call_id = call_id();
                self.active_tool_calls.push(ActiveToolCall {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: name.to_owned(),
                    accumulator: JsonAccumulator::new(),
                    provider_metadata: metadata,
                });
                out.push(StreamPart::ToolInputStart { id: tool_call_id.clone(), tool_name: name.to_owned(), provider_executed: None, dynamic: None });
                if let Some(partial) = partial_args {
                    let active = self.active_tool_calls.last_mut().expect("just pushed");
                    let delta = active.accumulator.process(partial);
                    if !delta.is_empty() {
                        out.push(StreamPart::ToolInputDelta { id: tool_call_id, delta });
                    }
                    if args_done(partial) {
                        self.finish_active_call(out);
                    }
                }
            } else if let (Some(partial), Some(active)) = (partial_args, self.active_tool_calls.last_mut()) {
                let delta = active.accumulator.process(partial);
                if !delta.is_empty() {
                    out.push(StreamPart::ToolInputDelta { id: active.tool_call_id.clone(), delta });
                }
                if args_done(partial) {
                    self.finish_active_call(out);
                }
            }
        } else if name.is_none() && args.is_none() && will_continue.is_none() && !self.active_tool_calls.is_empty() {
            // The terminal chunk of a streamed call.
            self.finish_active_call(out);
        } else if let Some(name) = name {
            // A complete call, or a single-chunk call with no arguments at all, which has no input delta.
            let tool_call_id = call_id();
            out.push(StreamPart::ToolInputStart { id: tool_call_id.clone(), tool_name: name.to_owned(), provider_executed: None, dynamic: None });
            let input = match args {
                Some(Json::Str(text)) => text.clone(),
                Some(value) => value.to_text(),
                None => "{}".to_owned(),
            };
            if args.is_some() {
                out.push(StreamPart::ToolInputDelta { id: tool_call_id.clone(), delta: input.clone() });
            }
            out.push(StreamPart::ToolInputEnd { id: tool_call_id.clone() });
            out.push(StreamPart::ToolCall { tool_call_id, tool_name: name.to_owned(), input, provider_executed: None, dynamic: None, provider_metadata: metadata });
            self.has_tool_calls = true;
        }
    }

    fn on_value(&mut self, value: &Json, out: &mut Vec<StreamPart>) {
        if let (false, Some(id)) = (self.has_emitted_response_metadata, value.non_null("responseId").and_then(Json::as_str)) {
            self.has_emitted_response_metadata = true;
            out.push(StreamPart::ResponseMetadata { id: Some(id.to_owned()), model_id: None });
        }
        let usage_metadata = value.non_null("usageMetadata");
        if let Some(usage) = usage_metadata {
            self.usage = Some(usage.clone());
        }
        // Sometimes the API returns an empty candidates array.
        let Some(candidate) = value.non_null("candidates").and_then(Json::as_arr).and_then(|list| list.first()) else { return };
        let grounding = candidate.non_null("groundingMetadata");
        if let Some(metadata) = grounding {
            self.last_grounding_metadata = metadata.clone();
        }
        if let Some(metadata) = candidate.non_null("urlContextMetadata") {
            self.last_url_context_metadata = metadata.clone();
        }
        for (url, title) in url_sources(grounding) {
            if self.emitted_source_urls.insert(url.to_owned()) {
                out.push(StreamPart::Source(jobj! { "sourceType": "url", "id": generate_id(), "url": url, "title"?: title }));
            }
        }
        // Tool calls go first, because `hasToolCalls` decides the finish reason below.
        if let Some(parts) = candidate.path(&["content", "parts"]).and_then(Json::as_arr) {
            for part in parts {
                self.on_ordered_part(part, out);
            }
            for part in parts {
                if let Some(Part::FunctionCall(call)) = part_kind(part) {
                    self.on_function_call(part, call, out);
                }
            }
        }
        if let Some(reason) = candidate.non_null("finishReason").and_then(Json::as_str) {
            self.finish_reason = FinishReason::new(map_finish_reason(reason, self.has_tool_calls), Some(reason.to_owned()));
            let service_tier = self.usage.as_ref().and_then(|u| u.non_null("serviceTier")).cloned().unwrap_or_default();
            self.provider_metadata = Some(self.wrap(jobj! {
                "promptFeedback": value.non_null("promptFeedback").cloned().unwrap_or_default(),
                "groundingMetadata": self.last_grounding_metadata.clone(),
                "urlContextMetadata": self.last_url_context_metadata.clone(),
                "safetyRatings": candidate.non_null("safetyRatings").cloned().unwrap_or_default(),
                "usageMetadata": usage_metadata.cloned().unwrap_or_default(),
                "finishMessage": candidate.non_null("finishMessage").cloned().unwrap_or_default(),
                "serviceTier": service_tier,
            }));
        }
    }
}

impl ChunkTransform for StreamState {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        let value = match chunk {
            Ok(value) => value,
            Err(error) => {
                out.push(StreamPart::Error(error));
                return;
            }
        };
        if !chunk_ok(&value) {
            out.push(StreamPart::Error(AiError::validation(&value, "the chunk does not match the Google response schema")));
            return;
        }
        self.on_value(&value, out);
    }

    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        self.end_text(out);
        self.end_reasoning(out);
        out.push(StreamPart::Finish {
            finish_reason: self.finish_reason.clone(),
            usage: convert_usage(self.usage.as_ref()),
            provider_metadata: self.provider_metadata.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(text: &str) -> Option<String> {
        match convert_json_schema(&Json::parse(text).unwrap()) {
            Ok(converted) => converted.map(|c| c.to_text()),
            Err(SchemaError::Recursive) => Some("<recursive>".to_owned()),
            Err(SchemaError::Other(error)) => Some(format!("<error: {error}>")),
        }
    }

    #[test]
    fn capabilities_follow_the_generation() {
        let caps = |id| {
            let c = capabilities(id);
            (c.supports_gemini2_tools, c.supports_file_search, c.uses_gemini3_features)
        };
        assert_eq!(caps("gemini-3-pro-preview"), (true, true, true));
        assert_eq!(caps("gemini-next-thing"), (true, true, true));
        assert_eq!(caps("gemini-2.5-flash"), (true, true, false));
        assert_eq!(caps("gemini-2.0-flash"), (true, false, false));
        assert_eq!(caps("gemini-1.5-pro"), (false, false, false));
        assert_eq!(caps("publishers/google/models/gemini-pro"), (false, false, false));
        assert_eq!(caps("GEMINI-pro-vision"), (false, false, false));
        assert_eq!(caps("gemini-robotics-er-1.5-preview"), (false, false, false));
        assert_eq!(caps("nano-banana-pro-preview"), (true, false, false));
        assert_eq!(caps("gemma-3-1b-it"), (false, false, false));
    }

    #[test]
    fn thinking_by_generation() {
        let thinking = |level, id| resolve_thinking_config(Some(level), id).map(|c| Json::Obj(c).to_text());
        assert_eq!(thinking("high", "gemini-2.5-flash").as_deref(), Some(r#"{"thinkingBudget":24576}"#));
        assert_eq!(thinking("high", "gemini-2.5-pro").as_deref(), Some(r#"{"thinkingBudget":32768}"#));
        assert_eq!(thinking("medium", "gemini-2.5-flash").as_deref(), Some(r#"{"thinkingBudget":19661}"#));
        assert_eq!(thinking("low", "gemini-2.5-flash").as_deref(), Some(r#"{"thinkingBudget":6554}"#));
        assert_eq!(thinking("minimal", "gemini-2.5-flash").as_deref(), Some(r#"{"thinkingBudget":1311}"#));
        assert_eq!(thinking("none", "gemini-2.5-flash").as_deref(), Some(r#"{"thinkingBudget":0}"#));
        assert_eq!(thinking("xhigh", "gemini-3-pro-preview").as_deref(), Some(r#"{"thinkingLevel":"high"}"#));
        assert_eq!(thinking("none", "gemini-3-pro-preview").as_deref(), Some(r#"{"thinkingLevel":"minimal"}"#));
        assert_eq!(thinking("none", "gemini-3.7-flash").as_deref(), Some(r#"{"thinkingLevel":"low"}"#));
        assert_eq!(thinking("minimal", "gemini-flash-latest").as_deref(), Some(r#"{"thinkingLevel":"low"}"#));
        assert_eq!(thinking("minimal", "gemini-3.7-flash-lite").as_deref(), Some(r#"{"thinkingLevel":"minimal"}"#));
        assert_eq!(thinking("minimal", "gemini-3.7-flash-preview").as_deref(), Some(r#"{"thinkingLevel":"low"}"#));
        assert_eq!(thinking("minimal", "gemini-3.7-flashy").as_deref(), Some(r#"{"thinkingLevel":"minimal"}"#));
        assert_eq!(thinking("high", "gemini-3-pro-image-preview").as_deref(), Some(r#"{"thinkingBudget":32768}"#));
        assert!(resolve_thinking_config(Some("provider-default"), "gemini-3-pro-preview").is_none());
        assert!(resolve_thinking_config(None, "gemini-3-pro-preview").is_none());
    }

    #[test]
    fn schema_conversion_follows_the_sdk() {
        assert_eq!(schema(r#"{"type":"object"}"#), None);
        assert_eq!(schema(r#"{"type":"object","properties":{}}"#), None);
        assert_eq!(schema(r#"{"type":"object","properties":{"a":{"type":"object","description":"d"}}}"#).unwrap(), r#"{"type":"object","properties":{"a":{"type":"object","description":"d"}}}"#);
        assert_eq!(schema(r#"{"type":["string","null"],"description":"d"}"#).unwrap(), r#"{"description":"d","anyOf":[{"type":"string"}],"nullable":true}"#);
        assert_eq!(schema(r#"{"type":["string","null"],"enum":["a",null]}"#).unwrap(), r#"{"nullable":true,"type":"string","enum":["a"]}"#);
        assert_eq!(schema(r#"{"enum":[1,2.5]}"#).unwrap(), r#"{"type":"number","format":"enum","enum":["1","2.5"]}"#);
        assert_eq!(schema(r#"{"type":"integer","enum":[1,2]}"#).unwrap(), r#"{"type":"integer","format":"enum","enum":["1","2"]}"#);
        assert_eq!(schema(r#"{"const":null}"#).unwrap(), r#"{"type":"null"}"#);
        assert_eq!(schema(r#"{"const":"x"}"#).unwrap(), r#"{"type":"string","enum":["x"]}"#);
        assert!(schema(r#"{"enum":["a",1]}"#).unwrap().starts_with("<error: Google does not support"));
        assert_eq!(schema(r#"{"anyOf":[{"type":"string"},{"type":"null"}]}"#).unwrap(), r#"{"nullable":true,"type":"string"}"#);
        assert_eq!(schema(r#"{"anyOf":[{"type":"string"},{"type":"number"},{"type":"null"}]}"#).unwrap(), r#"{"anyOf":[{"type":"string"},{"type":"number"}],"nullable":true}"#);
        assert_eq!(schema(r#"{"items":true}"#).unwrap(), r#"{"items":{"type":"boolean","properties":{}}}"#);
        assert_eq!(schema(r##"{"$defs":{"a":{"type":"string"}},"type":"object","properties":{"x":{"$ref":"#/$defs/a","description":"over"}}}"##).unwrap(), r#"{"type":"object","properties":{"x":{"description":"over","type":"string"}}}"#);
        assert_eq!(schema(r##"{"definitions":{"a~/b":{"type":"string"}},"properties":{"x":{"$ref":"#/definitions/a~0~1b"}}}"##).unwrap(), r#"{"properties":{"x":{"type":"string"}}}"#);
        assert_eq!(schema(r##"{"$defs":{"n":{"properties":{"next":{"$ref":"#/$defs/n"}}}},"$ref":"#/$defs/n"}"##).unwrap(), "<recursive>");
        assert!(schema(r##"{"$ref":"#/other/a"}"##).unwrap().starts_with("<error: Google schema conversion only supports"));
    }

    #[test]
    fn tool_choice_and_strict_tools() {
        let function = jobj! { "type": "function", "name": "f", "description": "d", "inputSchema": jobj! { "type": "object" } };
        let strict = jobj! { "type": "function", "name": "f", "inputSchema": jobj! { "type": "object" }, "strict": true };
        let search = jobj! { "type": "provider", "name": "google_search", "id": "google.google_search", "args": jobj! { "timeRangeFilter": jobj! { "startTime": "a", "endTime": "b" } } };
        let config = |tools: Vec<Json>, choice: Json, model: &str, vertex: bool| {
            let (tools, config) = prepare_tools(Some(&tools), &choice, model, vertex).unwrap();
            (Json::Arr(tools.unwrap_or_default()).to_text(), config.map(|c| c.to_text()))
        };
        let m = "gemini-2.5-flash";
        let declarations = r#"[{"functionDeclarations":[{"name":"f","description":"d"}]}]"#;
        assert_eq!(config(vec![function.clone()], jobj! { "type": "auto" }, m, false), (declarations.to_owned(), Some(r#"{"functionCallingConfig":{"mode":"AUTO"}}"#.to_owned())));
        assert_eq!(config(vec![function.clone()], jobj! { "type": "none" }, m, false).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"NONE"}}"#));
        assert_eq!(config(vec![function.clone()], jobj! { "type": "required" }, m, false).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"ANY"}}"#));
        assert_eq!(config(vec![function.clone()], jobj! { "type": "tool", "toolName": "f" }, m, false).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["f"]}}"#));
        assert_eq!(config(vec![strict.clone()], jobj! { "type": "auto" }, m, false), (r#"[{"functionDeclarations":[{"name":"f","description":""}]}]"#.to_owned(), Some(r#"{"functionCallingConfig":{"mode":"VALIDATED"}}"#.to_owned())));

        let g3 = "gemini-3-pro-preview";
        let both = || vec![function.clone(), search.clone()];
        let searches = r#"{"googleSearch":{"timeRangeFilter":{"startTime":"a","endTime":"b"}}}"#;
        assert_eq!(config(both(), jobj! { "type": "auto" }, g3, false), (format!(r#"[{searches},{{"functionDeclarations":[{{"name":"f","description":"d"}}]}}]"#), Some(r#"{"functionCallingConfig":{"mode":"VALIDATED"},"includeServerSideToolInvocations":true}"#.to_owned())));
        assert_eq!(config(both(), jobj! { "type": "none" }, g3, true).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"NONE"}}"#));
        assert_eq!(config(both(), jobj! { "type": "required" }, g3, false).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"ANY"},"includeServerSideToolInvocations":true}"#));
        assert_eq!(config(both(), jobj! { "type": "tool", "toolName": "f" }, g3, true).1.as_deref(), Some(r#"{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["f"]}}"#));
        // Before Gemini 3 the function tools go, and a model with no search at all has no tools left.
        assert_eq!(config(both(), jobj! { "type": "auto" }, m, false), (format!("[{searches}]"), None));
        assert_eq!(config(both(), jobj! { "type": "auto" }, "gemini-1.5-pro", false), ("[]".to_owned(), None));
        assert!(prepare_tools(Some(&Vec::new()), &jobj! { "type": "auto" }, m, false).unwrap().0.is_none());
    }

    #[test]
    fn accumulator_deltas_concatenate_to_the_final_json() {
        let parse = |text: &str| Json::parse(text).unwrap();
        let mut accumulator = JsonAccumulator::new();
        let mut text = accumulator.process(parse(r#"[{"jsonPath":"$.brightness","numberValue":50}]"#).as_arr().unwrap());
        assert_eq!(text, r#"{"brightness":50"#);
        text.push_str(&accumulator.process(parse(r#"[{"jsonPath":"$.recipe.name","stringValue":"Las","willContinue":true}]"#).as_arr().unwrap()));
        text.push_str(&accumulator.process(parse(r#"[{"jsonPath":"$.recipe.name","stringValue":"a\"gna"}]"#).as_arr().unwrap()));
        text.push_str(&accumulator.process(parse(r#"[{"jsonPath":"$.recipe.steps[0].t","stringValue":"é"},{"jsonPath":"$.recipe.steps[1].t","boolValue":true},{"jsonPath":"$.none","nullValue":"NULL_VALUE"}]"#).as_arr().unwrap()));
        let (final_json, closing) = accumulator.finalize();
        assert_eq!(final_json, r#"{"brightness":50,"recipe":{"name":"Lasa\"gna","steps":[{"t":"é"},{"t":true}]},"none":null}"#);
        assert_eq!(closing, "}");
        text.push_str(&closing);
        assert_eq!(text, final_json);
    }

    #[test]
    fn the_part_union_falls_through_in_order() {
        let kind = |text: &str| match part_kind(&Json::parse(text).unwrap()) {
            Some(Part::FunctionCall(_)) => "functionCall",
            Some(Part::InlineData(_)) => "inlineData",
            Some(Part::ToolCall(_)) => "toolCall",
            Some(Part::ToolResponse(_)) => "toolResponse",
            Some(Part::Plain) => "plain",
            None => "invalid",
        };
        assert_eq!(kind(r#"{"functionCall":{"name":"a"}}"#), "functionCall");
        assert_eq!(kind(r#"{"functionCall":{"name":1},"text":"x"}"#), "plain");
        assert_eq!(kind(r#"{"inlineData":{"mimeType":"a/b","data":"x"},"thought":true}"#), "inlineData");
        assert_eq!(kind(r#"{"toolCall":{"toolType":"T"}}"#), "plain");
        assert_eq!(kind(r#"{"text":5}"#), "invalid");
        assert_eq!(kind(r#"{"executableCode":{"code":"x"}}"#), "invalid");
        assert_eq!(kind(r#"{}"#), "plain");
    }

    #[test]
    fn finish_reasons_and_usage_map() {
        assert_eq!(map_finish_reason("STOP", false), "stop");
        assert_eq!(map_finish_reason("STOP", true), "tool-calls");
        assert_eq!(map_finish_reason("MAX_TOKENS", true), "length");
        for reason in ["IMAGE_SAFETY", "RECITATION", "SAFETY", "BLOCKLIST", "PROHIBITED_CONTENT", "SPII"] {
            assert_eq!(map_finish_reason(reason, false), "content-filter");
        }
        assert_eq!(map_finish_reason("MALFORMED_FUNCTION_CALL", false), "error");
        assert_eq!(map_finish_reason("FINISH_REASON_UNSPECIFIED", false), "other");
        assert_eq!(map_finish_reason("NEW", false), "other");
        let usage = convert_usage(Some(&Json::parse(r#"{"promptTokenCount":10,"cachedContentTokenCount":4,"candidatesTokenCount":3,"thoughtsTokenCount":2}"#).unwrap()));
        assert_eq!((usage.input_total, usage.input_no_cache, usage.input_cache_read, usage.input_cache_write), (Some(10), Some(6), Some(4), None));
        assert_eq!((usage.output_total, usage.output_text, usage.output_reasoning), (Some(5), Some(3), Some(2)));
        assert_eq!(convert_usage(None), ModelUsage::default());
    }

    #[test]
    fn data_urls_parse_as_the_sdk_regex_does() {
        assert_eq!(parse_base64_data_url("data:image/png;base64,AAA"), Some(("image/png", "AAA")));
        assert_eq!(parse_base64_data_url("data:image/png,AAA"), None);
        assert_eq!(parse_base64_data_url("data:;base64,AAA"), None);
        assert_eq!(parse_base64_data_url("data:image/png;base64,"), None);
        assert_eq!(parse_base64_data_url("https://x/y"), None);
    }
}
