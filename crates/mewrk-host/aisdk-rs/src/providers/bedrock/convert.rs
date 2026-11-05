//! `convert-to-amazon-bedrock-chat-messages.ts` and `amazon-bedrock-prepare-tools.ts`.

use super::schema::{self, Schema};
use super::{js_trim, js_truthy, normalize_tool_call_id, parse_provider_options, unsupported};
use crate::error::AiError;
use crate::jobj;
use crate::json::Json;
use crate::prompt::{is_full_media_type, resolve_full_media_type, top_level_media_type};

fn image_format(media_type: &str) -> Result<&'static str, AiError> {
    Ok(match media_type {
        "image/jpeg" => "jpeg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => return Err(AiError::other(format!("Unsupported image mime type: {media_type}, expected one of: image/jpeg, image/png, image/gif, image/webp"))),
    })
}

fn document_format(media_type: &str) -> Result<&'static str, AiError> {
    Ok(match media_type {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => {
            return Err(AiError::other(format!(
                "Unsupported file mime type: {media_type}, expected one of: application/pdf, text/csv, application/msword, application/vnd.openxmlformats-officedocument.wordprocessingml.document, application/vnd.ms-excel, application/vnd.openxmlformats-officedocument.spreadsheetml.sheet, text/html, text/plain, text/markdown"
            )));
        }
    })
}

fn video_format(media_type: &str) -> Result<&'static str, AiError> {
    Ok(match media_type {
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-flv" => "flv",
        "video/mpeg" => "mpeg",
        "video/mpg" => "mpg",
        "video/wmv" | "video/x-ms-wmv" => "wmv",
        "video/3gpp" => "three_gp",
        _ => {
            return Err(AiError::other(format!(
                "Unsupported video mime type: {media_type}, expected one of: video/x-matroska, video/quicktime, video/mp4, video/webm, video/x-flv, video/mpeg, video/mpg, video/wmv, video/x-ms-wmv, video/3gpp"
            )));
        }
    })
}

fn get_cache_point(options: Option<&Json>) -> Option<Json> {
    let options = options?;
    let config = options.path(&["amazonBedrock", "cachePoint"]).filter(|c| !c.is_null()).or_else(|| options.path(&["bedrock", "cachePoint"]))?;
    js_truthy(config).then(|| jobj! { "cachePoint": config.clone() })
}

fn push_cache_point(content: &mut Vec<Json>, options: Option<&Json>) {
    if let Some(point) = get_cache_point(options) {
        content.push(point);
    }
}

/// `sanitizeToolName`.
fn sanitize_tool_name(name: &str) -> String {
    let kept: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
    if kept.is_empty() { "_".to_owned() } else { kept }
}

/// `stripFileExtension`.
fn strip_file_extension(filename: &str) -> &str {
    filename.split_once('.').map_or(filename, |(stem, _)| stem)
}

/// `url.protocol === 's3:'`.
fn is_s3_url(data: &Json) -> bool {
    data.str_at("url").and_then(|url| url.split_once(':')).is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("s3"))
}

/// `url.toString()` of an `s3:` URL: the scheme is the part a non-special URL lower-cases, and the path takes the
/// path percent-encode set.
fn s3_url_text(data: &Json) -> String {
    let url = data.str_at("url").unwrap_or("");
    let Some((scheme, rest)) = url.split_once(':') else { return url.to_owned() };
    let mut out = format!("{}:", scheme.to_ascii_lowercase());
    for c in rest.chars() {
        if c <= ' ' || c >= '\u{7f}' || matches!(c, '"' | '<' | '>' | '`' | '{' | '}') {
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

/// `getAmazonBedrockMediaSource`.
fn media_source(data: &Json, functionality: &str) -> Result<Json, AiError> {
    match data.str_at("type") {
        Some("data") => Ok(jobj! { "bytes": data.get("data").cloned().unwrap_or_default() }),
        _ if is_s3_url(data) => Ok(jobj! { "s3Location": jobj! { "uri": s3_url_text(data) } }),
        _ => Err(unsupported(functionality)),
    }
}

fn image_block(data: &Json, media_type: &str, functionality: &str) -> Result<Json, AiError> {
    Ok(jobj! { "image": jobj! { "format": image_format(media_type)?, "source": media_source(data, functionality)? } })
}

fn video_block(data: &Json, media_type: &str, functionality: &str) -> Result<Json, AiError> {
    Ok(jobj! { "video": jobj! { "format": video_format(media_type)?, "source": media_source(data, functionality)? } })
}

/// A provider-options lookup that prefers `amazonBedrock` over the legacy `bedrock` key.
fn part_options(options: Option<&Json>, schema: &'static Schema) -> Result<Option<Json>, AiError> {
    if let Some(parsed) = parse_provider_options("amazonBedrock", options, schema)? {
        return Ok(Some(parsed));
    }
    parse_provider_options("bedrock", options, schema)
}

fn citations_enabled(part: &Json) -> Result<bool, AiError> {
    let parsed = part_options(part.get("providerOptions"), &schema::FILE_PART_OPTIONS)?;
    Ok(parsed.and_then(|o| o.path(&["citations", "enabled"]).and_then(Json::as_bool)).unwrap_or(false))
}

fn guard_content(options: Option<&Json>) -> bool {
    options.and_then(|o| o.get("guardContent")).and_then(Json::as_bool).unwrap_or(false)
}

struct DocumentNames(u64);

impl DocumentNames {
    /// `part.filename ? stripFileExtension(part.filename) : generateDocumentName()`.
    fn for_part(&mut self, part: &Json) -> String {
        match part.str_at("filename").filter(|name| !name.is_empty()) {
            Some(filename) => strip_file_extension(filename).to_owned(),
            None => {
                self.0 += 1;
                format!("document-{}", self.0)
            }
        }
    }
}

fn document_block(format: &str, name: String, bytes: Json, citations: bool) -> Json {
    jobj! {
        "document": jobj! {
            "format": format,
            "name": name,
            "source": jobj! { "bytes": bytes },
            "citations"?: citations.then(|| jobj! { "enabled": true }),
        },
    }
}

fn user_file_block(part: &Json, names: &mut DocumentNames) -> Result<Json, AiError> {
    let data = part.get("data").cloned().unwrap_or_default();
    match data.str_at("type").unwrap_or("") {
        "reference" => Err(unsupported("file parts with provider references")),
        "url" => {
            if !is_s3_url(&data) {
                return Err(unsupported("File URL data"));
            }
            let media_type = resolve_full_media_type(part)?;
            match top_level_media_type(&media_type) {
                "image" => image_block(&data, &media_type, "File URL data"),
                "video" => video_block(&data, &media_type, "File URL data"),
                _ => Err(unsupported("File URL data")),
            }
        }
        "text" => {
            let declared = part.str_at("mediaType").unwrap_or("");
            let media_type = if is_full_media_type(declared) { declared } else { "text/plain" };
            let citations = citations_enabled(part)?;
            let format = document_format(media_type)?;
            let text = data.str_at("text").unwrap_or("");
            Ok(document_block(format, names.for_part(part), Json::from(base64_simd::STANDARD.encode_to_string(text.as_bytes())), citations))
        }
        _ => {
            let media_type = resolve_full_media_type(part)?;
            match top_level_media_type(&media_type) {
                "image" => {
                    let options = part_options(part.get("providerOptions"), &schema::IMAGE_PART_OPTIONS)?;
                    let block = image_block(&data, &media_type, "File URL data")?;
                    Ok(if guard_content(options.as_ref()) { jobj! { "guardContent": block } } else { block })
                }
                "video" => video_block(&data, &media_type, "File URL data"),
                _ => {
                    let citations = citations_enabled(part)?;
                    let format = document_format(&media_type)?;
                    Ok(document_block(format, names.for_part(part), data.get("data").cloned().unwrap_or_default(), citations))
                }
            }
        }
    }
}

fn tool_result_content(output: &Json, names: &mut DocumentNames) -> Result<Vec<Json>, AiError> {
    match output.str_at("type").unwrap_or("") {
        "content" => {
            let mut blocks = Vec::new();
            for item in output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                blocks.push(tool_result_item(item, names)?);
            }
            Ok(blocks)
        }
        "text" | "error-text" => Ok(vec![jobj! { "text": output.get("value").cloned().unwrap_or_default() }]),
        "execution-denied" => Ok(vec![jobj! { "text": output.non_null("reason").cloned().unwrap_or_else(|| Json::from("Tool call execution denied.")) }]),
        _ => Ok(vec![jobj! { "text"?: output.get("value").map(Json::to_text) }]),
    }
}

fn tool_result_item(item: &Json, names: &mut DocumentNames) -> Result<Json, AiError> {
    match item.str_at("type").unwrap_or("") {
        "text" => Ok(jobj! { "text": item.get("text").cloned().unwrap_or_default() }),
        "file" => {
            let data = item.get("data").cloned().unwrap_or_default();
            let data_type = data.str_at("type").unwrap_or("");
            let functionality = format!("tool result file data of type \"{data_type}\"");
            // Upstream lets an `s3:` URL through here, but it would have to be a `URL` object: a frame's JSON can only
            // hold a string, which the SDK's `ModelMessage[]` schema refuses long before this point. So none arrives.
            if data_type != "data" {
                return Err(unsupported(&functionality));
            }
            let media_type = resolve_full_media_type(item)?;
            match top_level_media_type(&media_type) {
                "image" => image_block(&data, &media_type, &functionality),
                "video" => video_block(&data, &media_type, &functionality),
                _ => {
                    let citations = citations_enabled(item)?;
                    let format = document_format(&media_type)?;
                    Ok(document_block(format, names.for_part(item), data.get("data").cloned().unwrap_or_default(), citations))
                }
            }
        }
        other => Err(unsupported(&format!("unsupported tool content part type: {other}"))),
    }
}

/// `toBedrockToolInput`: Bedrock requires an object.
fn tool_input(input: Option<&Json>) -> Json {
    match input {
        Some(value) if value.is_obj() => value.clone(),
        other => jobj! { "rawInvalidInput"?: other.cloned() },
    }
}

fn user_message_content(message: &Json, is_mistral: bool, content: &mut Vec<Json>, names: &mut DocumentNames) -> Result<(), AiError> {
    let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
    match message.str_at("role") {
        Some("user") => {
            for part in parts {
                let options = part.get("providerOptions");
                match part.str_at("type") {
                    Some("text") => {
                        let text_options = part_options(options, &schema::TEXT_PART_OPTIONS)?;
                        let text = part.get("text").cloned().unwrap_or_default();
                        if guard_content(text_options.as_ref()) {
                            content.push(jobj! {
                                "guardContent": jobj! {
                                    "text": jobj! {
                                        "text": text,
                                        "qualifiers"?: text_options.as_ref().and_then(|o| o.get("guardContentQualifiers")).cloned(),
                                    },
                                },
                            });
                        } else {
                            content.push(jobj! { "text": text });
                        }
                    }
                    Some("file") => content.push(user_file_block(part, names)?),
                    _ => {}
                }
                push_cache_point(content, options);
            }
        }
        _ => {
            for part in parts {
                if part.str_at("type") == Some("tool-approval-response") {
                    continue;
                }
                let output = part.get("output").cloned().unwrap_or_default();
                let result = tool_result_content(&output, names)?;
                content.push(jobj! {
                    "toolResult": jobj! {
                        "toolUseId": normalize_tool_call_id(part.str_at("toolCallId").unwrap_or(""), is_mistral),
                        "content": result,
                    },
                });
                push_cache_point(content, part.get("providerOptions"));
            }
        }
    }
    push_cache_point(content, message.get("providerOptions"));
    Ok(())
}

fn assistant_message_content(message: &Json, is_mistral: bool, is_last: bool, content: &mut Vec<Json>) -> Result<(), AiError> {
    let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
    let has_reasoning = parts.iter().any(|part| part.str_at("type") == Some("reasoning"));
    for (at, part) in parts.iter().enumerate() {
        let options = part.get("providerOptions");
        match part.str_at("type") {
            Some("text") => {
                let text = part.str_at("text").unwrap_or("");
                // Empty text blocks go unless reasoning blocks are present.
                if !js_trim(text).is_empty() || has_reasoning {
                    // Bedrock does not allow trailing whitespace in a pre-filled assistant response.
                    let wire = if is_last && at + 1 == parts.len() { js_trim(text) } else { text };
                    content.push(jobj! { "text": wire });
                }
            }
            Some("reasoning") => {
                let metadata = part_options(options, &schema::REASONING_METADATA)?;
                let field = |key: &str| metadata.as_ref().and_then(|m| m.get(key)).cloned();
                if let Some(signature) = field("signature") {
                    // No trimming: the signature validates the exact original bytes.
                    content.push(jobj! {
                        "reasoningContent": jobj! {
                            "reasoningText": jobj! { "text": part.get("text").cloned().unwrap_or_default(), "signature": signature },
                        },
                    });
                } else if let Some(redacted) = field("redactedContent") {
                    content.push(jobj! { "reasoningContent": jobj! { "redactedContent": redacted } });
                } else if let Some(data) = field("redactedData") {
                    content.push(jobj! { "reasoningContent": jobj! { "redactedReasoning": jobj! { "data": data } } });
                }
                // Unsigned reasoning is intentionally not replayed: some models return reasoning without a
                // signature, and sending it back can leak it into the visible response.
            }
            Some("tool-call") => content.push(jobj! {
                "toolUse": jobj! {
                    "toolUseId": normalize_tool_call_id(part.str_at("toolCallId").unwrap_or(""), is_mistral),
                    "name": sanitize_tool_name(part.str_at("toolName").unwrap_or("")),
                    "input": tool_input(part.get("input")),
                },
            }),
            _ => {}
        }
        push_cache_point(content, options);
    }
    push_cache_point(content, message.get("providerOptions"));
    Ok(())
}

/// `convertToAmazonBedrockChatMessages`: `(system, messages)`.
pub(super) fn convert_messages(prompt: &[Json], is_mistral: bool) -> Result<(Vec<Json>, Vec<Json>), AiError> {
    // `groupIntoBlocks`: runs of one kind, with `tool` counted as `user`.
    let kind_of = |message: &Json| match message.str_at("role") {
        Some("user") | Some("tool") => "user",
        Some("assistant") => "assistant",
        _ => "system",
    };
    let mut blocks: Vec<(&str, Vec<&Json>)> = Vec::new();
    for message in prompt {
        let kind = kind_of(message);
        match blocks.last_mut() {
            Some((current, list)) if *current == kind => list.push(message),
            _ => blocks.push((kind, vec![message])),
        }
    }

    let mut system = Vec::new();
    let mut messages = Vec::new();
    let mut names = DocumentNames(0);
    let block_count = blocks.len();
    for (i, (kind, block)) in blocks.into_iter().enumerate() {
        match kind {
            "system" => {
                if !messages.is_empty() {
                    return Err(unsupported("Multiple system messages that are separated by user/assistant messages"));
                }
                for message in block {
                    system.push(jobj! { "text": message.get("content").cloned().unwrap_or_default() });
                    if let Some(point) = get_cache_point(message.get("providerOptions")) {
                        system.push(point);
                    }
                }
            }
            "user" => {
                let mut content = Vec::new();
                for message in block {
                    user_message_content(message, is_mistral, &mut content, &mut names)?;
                }
                messages.push(jobj! { "role": "user", "content": content });
            }
            _ => {
                let mut content = Vec::new();
                let count = block.len();
                for (j, message) in block.into_iter().enumerate() {
                    assistant_message_content(message, is_mistral, i + 1 == block_count && j + 1 == count, &mut content)?;
                }
                if !content.is_empty() {
                    messages.push(jobj! { "role": "assistant", "content": content });
                }
            }
        }
    }
    Ok((system, messages))
}

/// What `prepareTools` returns, without the warnings.
#[derive(Default)]
pub(super) struct PreparedTools {
    /// `toolConfig.tools`, `toolConfig.toolChoice`.
    pub tools: Vec<Json>,
    pub tool_choice: Option<Json>,
    /// `additionalTools.tool_choice`.
    pub additional_tool_choice: Option<Json>,
    pub betas: Vec<String>,
}

/// `asSchema(factory({}).inputSchema).jsonSchema`, for the factories this crate's callers use (`search.rs`). Other
/// Anthropic tools are never declared here; an id it does not know is left out, with upstream's warning unlogged.
fn provider_tool_input_schema(id: &str) -> Option<Json> {
    let field = match id {
        "anthropic.web_search_20260209" => "query",
        "anthropic.web_fetch_20250910" | "anthropic.web_fetch_20260209" => "url",
        _ => return None,
    };
    let mut properties = crate::json::Obj::new();
    properties.insert(field, jobj! { "type": "string" });
    Some(jobj! {
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": properties,
        "required": vec![Json::from(field)],
        "additionalProperties": false,
    })
}

/// `@ai-sdk/anthropic`'s `prepareTools`, for the two things Bedrock takes from it: the betas and the tool choice.
fn anthropic_tool_choice(tool_choice: &Json, disable_parallel: Option<bool>) -> Option<Json> {
    match tool_choice.str_at("type").unwrap_or("auto") {
        "required" => Some(jobj! { "type": "any", "disable_parallel_tool_use"?: disable_parallel }),
        "none" => None,
        "tool" => Some(jobj! {
            "type": "tool",
            "name": tool_choice.get("toolName").cloned().unwrap_or_default(),
            "disable_parallel_tool_use"?: disable_parallel,
        }),
        _ => Some(jobj! { "type": "auto", "disable_parallel_tool_use"?: disable_parallel }),
    }
}

/// `prepareTools`.
pub(super) fn prepare_tools(tools: Option<&Vec<Json>>, tool_choice: &Json, model_id: &str, disable_parallel: Option<bool>) -> PreparedTools {
    let mut prepared = PreparedTools::default();
    let Some(tools) = tools.filter(|t| !t.is_empty()) else { return prepared };
    // `web_search_20250305` is not supported on Bedrock: it goes, with only a warning.
    let supported: Vec<&Json> = tools
        .iter()
        .filter(|tool| !(tool.str_at("type") == Some("provider") && tool.str_at("id") == Some("anthropic.web_search_20250305")))
        .collect();
    if supported.is_empty() {
        return prepared;
    }

    // Narrower than the chat model's check: the dot is part of it.
    let is_anthropic = model_id.contains("anthropic.");
    let provider_tools: Vec<&&Json> = supported.iter().filter(|t| t.str_at("type") == Some("provider")).collect();
    let function_tools: Vec<&&Json> = supported.iter().filter(|t| t.str_at("type") == Some("function")).collect();
    let using_anthropic_tools = is_anthropic && !provider_tools.is_empty();
    let choice_type = tool_choice.str_at("type");

    if using_anthropic_tools {
        for tool in &provider_tools {
            let beta = match tool.str_at("id") {
                Some("anthropic.web_fetch_20250910") => "web-fetch-2025-09-10",
                Some("anthropic.web_fetch_20260209" | "anthropic.web_search_20260209") => "code-execution-web-tools-2026-02-09",
                _ => continue,
            };
            if !prepared.betas.iter().any(|known| known == beta) {
                prepared.betas.push(beta.to_owned());
            }
        }
        // Only the tool choice goes into the additional fields; the definitions stay in `toolConfig`.
        prepared.additional_tool_choice = anthropic_tool_choice(tool_choice, disable_parallel);
        for tool in &provider_tools {
            if let Some(schema) = tool.str_at("id").and_then(provider_tool_input_schema) {
                prepared.tools.push(jobj! {
                    "toolSpec": jobj! {
                        "name": tool.get("name").cloned().unwrap_or_default(),
                        "inputSchema": jobj! { "json": schema },
                    },
                });
            }
        }
    }

    let named = tool_choice.str_at("toolName").filter(|_| choice_type == Some("tool"));
    let strict_supported = super::supports_strict_tools(model_id);
    for tool in function_tools {
        if named.is_some_and(|name| tool.str_at("name") != Some(name)) {
            continue;
        }
        // `tool.description?.trim() !== ''`, with a null here being the `undefined` the host left out.
        let description = tool.non_null("description").filter(|d| d.as_str().is_none_or(|text| !js_trim(text).is_empty()));
        prepared.tools.push(jobj! {
            "toolSpec": jobj! {
                "name": tool.get("name").cloned().unwrap_or_default(),
                "description"?: description.cloned(),
                "strict"?: tool.non_null("strict").filter(|_| strict_supported).cloned(),
                "inputSchema": jobj! { "json": tool.get("inputSchema").cloned().unwrap_or_default() },
            },
        });
    }

    if is_anthropic && !using_anthropic_tools && disable_parallel == Some(true) && !prepared.tools.is_empty() && choice_type != Some("none") {
        prepared.additional_tool_choice = Some(match choice_type {
            Some("required") => jobj! { "type": "any", "disable_parallel_tool_use": true },
            Some("tool") => jobj! { "type": "tool", "name": tool_choice.get("toolName").cloned().unwrap_or_default(), "disable_parallel_tool_use": true },
            _ => jobj! { "type": "auto", "disable_parallel_tool_use": true },
        });
    }

    // The standard choice applies only without Anthropic provider tools.
    if !using_anthropic_tools && prepared.additional_tool_choice.is_none() && !prepared.tools.is_empty() {
        match choice_type {
            Some("required") => prepared.tool_choice = Some(jobj! { "any": Json::obj() }),
            Some("none") => prepared.tools.clear(),
            Some("tool") => prepared.tool_choice = Some(jobj! { "tool": jobj! { "name": tool_choice.get("toolName").cloned().unwrap_or_default() } }),
            _ => prepared.tool_choice = Some(jobj! { "auto": Json::obj() }),
        }
    }
    prepared
}
