//! The Zod schemas the Bedrock package parses its stream chunks and options with, as data.
//!
//! The shape of these schemas does real work on Bedrock: `z.object` strips unknown keys, so an exception or event
//! this package does not name is silently a no-op; a delta's `z.union` keeps only the first matching member; and a
//! delta of an unknown kind fails the whole chunk. Interpreting the same schema reproduces all of that, including
//! the text of a validation error, which is `JSON.stringify(issues, null, 2)`.

use crate::jobj;
use crate::json::{Json, Obj};

pub enum Schema {
    /// `z.unknown()`, `z.any()`, and `z.json()` over what `JSON.parse` already produced.
    Any,
    Number,
    Str,
    Bool,
    Enum(&'static [&'static str]),
    /// `z.record(z.string(), ...)` of anything.
    Record,
    Array(&'static Schema),
    /// Known keys in schema order, and with `catchall` the rest in input order. Without it the rest go.
    Object { keys: &'static [(&'static str, Schema)], catchall: bool },
    Union(&'static [Schema]),
    Optional(&'static Schema),
    Nullish(&'static Schema),
}

use Schema::{Any, Array, Bool, Enum, Nullish, Number, Object, Optional, Record, Str, Union};

const fn object(keys: &'static [(&'static str, Schema)]) -> Schema {
    Object { keys, catchall: false }
}

const fn open_object(keys: &'static [(&'static str, Schema)]) -> Schema {
    Object { keys, catchall: true }
}

/// `BEDROCK_STOP_REASONS`.
const STOP_REASONS: &[&str] =
    &["stop", "stop_sequence", "end_turn", "length", "max_tokens", "content-filter", "content_filtered", "guardrail_intervened", "tool-calls", "tool_use"];

/// `AmazonBedrockToolUseSchema`.
const TOOL_USE: Schema = object(&[("toolUseId", Str), ("name", Str), ("input", Optional(&Any))]);

/// `AmazonBedrockAdditionalModelResponseFieldsSchema`.
const ADDITIONAL_RESPONSE_FIELDS: Schema = open_object(&[("delta", Nullish(&object(&[("stop_sequence", Nullish(&Str))])))]);

/// `AmazonBedrockCacheDetailSchema`.
const CACHE_DETAIL: Schema = open_object(&[("inputTokens", Number), ("ttl", Str)]);

/// `AmazonBedrockStreamSchema`.
pub static STREAM_CHUNK: Schema = object(&[
    (
        "contentBlockDelta",
        Nullish(&object(&[
            ("contentBlockIndex", Number),
            (
                "delta",
                Nullish(&Union(&[
                    object(&[("text", Str)]),
                    object(&[("toolUse", object(&[("input", Str)]))]),
                    object(&[("reasoningContent", object(&[("text", Str)]))]),
                    object(&[("reasoningContent", object(&[("signature", Str)]))]),
                    object(&[("reasoningContent", object(&[("data", Str)]))]),
                    object(&[("reasoningContent", object(&[("redactedContent", Str)]))]),
                ])),
            ),
        ])),
    ),
    ("contentBlockStart", Nullish(&object(&[("contentBlockIndex", Number), ("start", Nullish(&object(&[("toolUse", Nullish(&TOOL_USE))])))]))),
    ("contentBlockStop", Nullish(&object(&[("contentBlockIndex", Number)]))),
    ("internalServerException", Nullish(&Record)),
    (
        "messageStop",
        Nullish(&object(&[
            ("additionalModelResponseFields", Nullish(&ADDITIONAL_RESPONSE_FIELDS)),
            ("stopReason", Union(&[Enum(STOP_REASONS), Str])),
        ])),
    ),
    (
        "metadata",
        Nullish(&object(&[
            ("trace", Nullish(&Any)),
            ("performanceConfig", Nullish(&object(&[("latency", Str)]))),
            ("serviceTier", Nullish(&object(&[("type", Str)]))),
            (
                "usage",
                Nullish(&open_object(&[
                    ("cacheReadInputTokens", Nullish(&Number)),
                    ("cacheWriteInputTokens", Nullish(&Number)),
                    ("cacheDetails", Nullish(&Array(&CACHE_DETAIL))),
                    ("inputTokens", Number),
                    ("outputTokens", Number),
                    ("totalTokens", Optional(&Number)),
                ])),
            ),
        ])),
    ),
    ("modelStreamErrorException", Nullish(&Record)),
    ("serviceUnavailableException", Nullish(&Record)),
    ("throttlingException", Nullish(&Record)),
    ("validationException", Nullish(&Record)),
]);

/// `amazonBedrockLanguageModelChatOptions`.
pub static CHAT_OPTIONS: Schema = object(&[
    ("additionalModelRequestFields", Optional(&Record)),
    (
        "reasoningConfig",
        Optional(&object(&[
            ("type", Optional(&Enum(&["enabled", "disabled", "adaptive"]))),
            ("budgetTokens", Optional(&Number)),
            ("maxReasoningEffort", Optional(&Enum(&["low", "medium", "high", "xhigh", "max"]))),
            ("display", Optional(&Enum(&["omitted", "summarized"]))),
        ])),
    ),
    ("anthropicBeta", Optional(&Array(&Str))),
    ("serviceTier", Optional(&Enum(&["reserved", "priority", "default", "flex"]))),
]);

/// `anthropicProviderOptions` of the chat model.
pub static ANTHROPIC_OPTIONS: Schema = object(&[("disableParallelToolUse", Optional(&Bool))]);

/// `amazonBedrockFilePartProviderOptions`.
pub static FILE_PART_OPTIONS: Schema = object(&[("citations", Optional(&object(&[("enabled", Bool)])))]);

/// `amazonBedrockTextPartProviderOptions`.
pub static TEXT_PART_OPTIONS: Schema = object(&[
    ("guardContent", Optional(&Bool)),
    ("guardContentQualifiers", Optional(&Array(&Enum(&["grounding_source", "query", "guard_content"])))),
]);

/// `amazonBedrockImagePartProviderOptions`.
pub static IMAGE_PART_OPTIONS: Schema = object(&[("guardContent", Optional(&Bool))]);

/// `amazonBedrockReasoningMetadataSchema`.
pub static REASONING_METADATA: Schema = object(&[("signature", Optional(&Str)), ("redactedData", Optional(&Str)), ("redactedContent", Optional(&Str))]);

/// A Zod issue on its way out: its leading fields, the path built up as parents prefix it, and its message.
struct Issue {
    fields: Json,
    path: Vec<Json>,
    message: String,
}

impl Issue {
    /// `finalizeIssue`: `path` and `message` follow the fields the check set.
    fn finalized(self) -> Json {
        let mut object = self.fields.as_obj().cloned().unwrap_or_default();
        object.insert("path", Json::Arr(self.path));
        object.insert("message", self.message);
        Json::Obj(object)
    }

    fn prefixed(mut self, segment: Json) -> Self {
        self.path.insert(0, segment);
        self
    }
}

/// `parsedType`, for a value the input may also not have.
fn received(value: Option<&Json>) -> &'static str {
    match value {
        None => "undefined",
        Some(Json::Null) => "null",
        Some(Json::Bool(_)) => "boolean",
        Some(Json::Num(_)) => "number",
        Some(Json::Str(_)) => "string",
        Some(Json::Arr(_)) => "array",
        Some(Json::Obj(_)) => "object",
    }
}

fn invalid_type(expected: &str, value: Option<&Json>) -> Issue {
    Issue {
        fields: jobj! { "expected": expected, "code": "invalid_type" },
        path: Vec::new(),
        message: format!("Invalid input: expected {expected}, received {}", received(value)),
    }
}

fn run(schema: &Schema, value: Option<&Json>, issues: &mut Vec<Issue>) -> Option<Json> {
    let mut wrong = |expected: &str| {
        issues.push(invalid_type(expected, value));
        None
    };
    match schema {
        Any => value.cloned(),
        Number => match value {
            Some(Json::Num(_)) => value.cloned(),
            _ => wrong("number"),
        },
        Str => match value {
            Some(Json::Str(_)) => value.cloned(),
            _ => wrong("string"),
        },
        Bool => match value {
            Some(Json::Bool(_)) => value.cloned(),
            _ => wrong("boolean"),
        },
        Enum(values) => {
            if value.and_then(Json::as_str).is_some_and(|text| values.contains(&text)) {
                return value.cloned();
            }
            let joined = values.iter().map(|option| format!("\"{option}\"")).collect::<Vec<_>>().join("|");
            issues.push(Issue {
                fields: jobj! { "code": "invalid_value", "values": values.iter().map(|option| Json::from(*option)).collect::<Vec<_>>() },
                path: Vec::new(),
                message: format!("Invalid option: expected one of {joined}"),
            });
            None
        }
        Record => match value {
            Some(Json::Obj(_)) => value.cloned(),
            _ => wrong("record"),
        },
        Array(element) => {
            let Some(Json::Arr(items)) = value else { return wrong("array") };
            let mut out = Vec::with_capacity(items.len());
            for (at, item) in items.iter().enumerate() {
                let mut nested = Vec::new();
                let parsed = run(element, Some(item), &mut nested);
                issues.extend(nested.into_iter().map(|issue| issue.prefixed(Json::from(at))));
                out.push(parsed.unwrap_or(Json::Null));
            }
            Some(Json::Arr(out))
        }
        Object { keys, catchall } => {
            let Some(Json::Obj(input)) = value else { return wrong("object") };
            let mut out = Obj::new();
            for (key, field) in *keys {
                let mut nested = Vec::new();
                let parsed = run(field, input.get(key), &mut nested);
                issues.extend(nested.into_iter().map(|issue| issue.prefixed(Json::from(*key))));
                if let Some(parsed) = parsed {
                    out.insert(*key, parsed);
                }
            }
            if *catchall {
                for (key, item) in input.iter() {
                    if !keys.iter().any(|(known, _)| *known == key) {
                        out.insert(key, item.clone());
                    }
                }
            }
            Some(Json::Obj(out))
        }
        Union(options) => {
            let mut failed = Vec::with_capacity(options.len());
            for option in *options {
                let mut nested = Vec::new();
                let parsed = run(option, value, &mut nested);
                if nested.is_empty() {
                    return parsed;
                }
                failed.push(Json::Arr(nested.into_iter().map(Issue::finalized).collect()));
            }
            issues.push(Issue {
                fields: jobj! { "code": "invalid_union", "errors": failed },
                path: Vec::new(),
                message: "Invalid input".to_owned(),
            });
            None
        }
        Optional(inner) => {
            value?;
            run(inner, value, issues)
        }
        Nullish(inner) => match value {
            None => None,
            Some(Json::Null) => Some(Json::Null),
            Some(_) => run(inner, value, issues),
        },
    }
}

/// `safeParse`: the stripped value, or the text of the `ZodError`.
pub fn validate(schema: &Schema, value: &Json) -> Result<Json, String> {
    let mut issues = Vec::new();
    let parsed = run(schema, Some(value), &mut issues);
    if issues.is_empty() {
        return Ok(parsed.unwrap_or(Json::Null));
    }
    let list = Json::Arr(issues.into_iter().map(Issue::finalized).collect());
    Err(serde_json::to_string_pretty(&list).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<String, String> {
        validate(&STREAM_CHUNK, &Json::parse(text).unwrap()).map(|value| value.to_text())
    }

    #[test]
    fn unknown_keys_strip_and_unions_keep_their_first_member() {
        assert_eq!(parse(r#"{"ThrottlingException":{"message":"m"}}"#).unwrap(), "{}");
        assert_eq!(parse(r#"{"messageStart":{"role":"assistant"}}"#).unwrap(), "{}");
        assert_eq!(
            parse(r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"a","toolUse":{"input":"b"}},"extra":1}}"#).unwrap(),
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"text":"a"}}}"#
        );
        assert_eq!(
            parse(r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"signature":"s","text":"t"}}}}"#).unwrap(),
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"text":"t"}}}}"#
        );
    }

    #[test]
    fn catchall_keeps_extras_after_the_known_keys() {
        assert_eq!(
            parse(r#"{"metadata":{"usage":{"future":1,"outputTokens":2,"inputTokens":1,"cacheDetails":[{"x":1,"ttl":"5m","inputTokens":3}]},"metrics":{"latencyMs":1}}}"#).unwrap(),
            r#"{"metadata":{"usage":{"cacheDetails":[{"inputTokens":3,"ttl":"5m","x":1}],"inputTokens":1,"outputTokens":2,"future":1}}}"#
        );
    }

    #[test]
    fn nulls_pass_a_nullish_field() {
        assert_eq!(parse(r#"{"contentBlockStop":null}"#).unwrap(), r#"{"contentBlockStop":null}"#);
        assert_eq!(
            parse(r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":null}}"#).unwrap(),
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":null}}"#
        );
    }

    #[test]
    fn issues_render_as_zod_renders_them() {
        assert_eq!(
            parse(r#"{"contentBlockStop":{}}"#).unwrap_err(),
            "[\n  {\n    \"expected\": \"number\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"contentBlockStop\",\n      \"contentBlockIndex\"\n    ],\n    \"message\": \"Invalid input: expected number, received undefined\"\n  }\n]"
        );
    }

    #[test]
    fn option_schemas_reject_the_wrong_type() {
        let check = |schema: &Schema, text: &str| validate(schema, &Json::parse(text).unwrap()).is_ok();
        assert!(check(&CHAT_OPTIONS, r#"{"guardrailConfig":{"x":1},"reasoningConfig":{"type":"adaptive","maxReasoningEffort":"max"}}"#));
        assert!(!check(&CHAT_OPTIONS, r#"{"reasoningConfig":{"type":"sometimes"}}"#));
        assert!(!check(&CHAT_OPTIONS, r#"{"reasoningConfig":null}"#));
        assert!(!check(&CHAT_OPTIONS, r#"{"serviceTier":"gold"}"#));
        assert!(!check(&CHAT_OPTIONS, r#"[]"#));
        assert!(!check(&ANTHROPIC_OPTIONS, r#"{"disableParallelToolUse":"yes"}"#));
        assert!(!check(&REASONING_METADATA, r#"{"signature":5}"#));
        assert!(check(&REASONING_METADATA, r#"{"signature":"s","other":5}"#));
    }
}
