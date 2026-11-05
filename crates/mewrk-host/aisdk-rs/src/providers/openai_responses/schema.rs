//! The Zod schemas of `openai-responses-api.ts` and the Responses provider options, as data.
//!
//! The schemas do more than accept or refuse. A parsed chunk holds only the schema's keys, in the schema's order, and
//! parts of it reach the host verbatim (`annotations`, `sources`, an error's `responseBody`), so they are ported rather
//! than replaced by reads of the raw value.

use std::sync::OnceLock;

use crate::json::{Json, Obj};

/// A Zod schema, for the combinators these schemas use.
pub(super) enum S {
    /// `z.any()`, `z.unknown()`, `jsonValueSchema`.
    Any,
    Str,
    Num,
    /// `z.number().min(a).max(b)`.
    NumRange(f64, f64),
    Bool,
    Lit(&'static str),
    Enum(&'static [&'static str]),
    /// `.optional()`: `undefined`, not `null`.
    Optional(Box<S>),
    /// `.nullish()`.
    Nullish(Box<S>),
    /// `.nullable()`: `null`, not `undefined`.
    Nullable(Box<S>),
    Arr(Box<S>),
    /// `z.array(...).min(1)`.
    NonEmptyArr(Box<S>),
    /// `z.object`: unknown keys go, the schema's keys come out in the schema's order.
    Obj(Vec<(&'static str, S)>),
    /// `z.object(...).loose()`: unknown keys stay, after the schema's.
    Loose(Vec<(&'static str, S)>),
    Record(Box<S>),
    Union(Vec<S>),
    /// `z.discriminatedUnion('type', ...)` of `S::Obj`s.
    Disc(Vec<S>),
}

impl S {
    /// `schema.safeParse`: `Err` when the value fails, `Ok(None)` for an accepted `undefined`.
    pub(super) fn parse(&self, input: Option<&Json>) -> Result<Option<Json>, ()> {
        match self {
            S::Any => Ok(input.cloned()),
            S::Optional(inner) => match input {
                None => Ok(None),
                some => inner.parse(some),
            },
            S::Nullish(inner) => match input {
                None => Ok(None),
                Some(Json::Null) => Ok(Some(Json::Null)),
                some => inner.parse(some),
            },
            S::Nullable(inner) => match input {
                Some(Json::Null) => Ok(Some(Json::Null)),
                other => inner.parse(other),
            },
            other => {
                let value = input.ok_or(())?;
                other.parse_present(value).map(Some)
            }
        }
    }

    fn parse_present(&self, value: &Json) -> Result<Json, ()> {
        let ok = |accepted: bool| if accepted { Ok(value.clone()) } else { Err(()) };
        match self {
            S::Any | S::Optional(_) | S::Nullish(_) | S::Nullable(_) => self.parse(Some(value)).map(Option::unwrap_or_default),
            S::Str => ok(value.is_str()),
            S::Num => ok(value.is_num()),
            S::NumRange(min, max) => ok(value.as_f64().is_some_and(|n| n >= *min && n <= *max)),
            S::Bool => ok(value.as_bool().is_some()),
            S::Lit(literal) => ok(value.as_str() == Some(literal)),
            S::Enum(values) => ok(value.as_str().is_some_and(|v| values.contains(&v))),
            S::Arr(inner) => parse_array(inner, value),
            S::NonEmptyArr(inner) => {
                if value.as_arr().is_some_and(Vec::is_empty) {
                    return Err(());
                }
                parse_array(inner, value)
            }
            S::Obj(fields) => parse_fields(fields, value).map(Json::Obj),
            S::Loose(fields) => {
                let mut out = parse_fields(fields, value)?;
                for (key, entry) in value.as_obj().ok_or(())?.iter() {
                    if !fields.iter().any(|(name, _)| *name == key) {
                        out.insert(key, entry.clone());
                    }
                }
                Ok(Json::Obj(out))
            }
            S::Record(inner) => {
                let mut out = Obj::new();
                for (key, entry) in value.as_obj().ok_or(())?.iter() {
                    if let Some(parsed) = inner.parse(Some(entry))? {
                        out.insert(key, parsed);
                    }
                }
                Ok(Json::Obj(out))
            }
            S::Union(options) => options.iter().find_map(|option| option.parse_present(value).ok()).ok_or(()),
            S::Disc(options) => {
                let kind = value.str_at("type").ok_or(())?;
                let option = options.iter().find(|option| option.literal_type() == Some(kind)).ok_or(())?;
                option.parse_present(value)
            }
        }
    }

    fn literal_type(&self) -> Option<&'static str> {
        let S::Obj(fields) = self else { return None };
        match fields.iter().find(|(name, _)| *name == "type") {
            Some((_, S::Lit(literal))) => Some(literal),
            _ => None,
        }
    }
}

fn parse_array(inner: &S, value: &Json) -> Result<Json, ()> {
    let items = value.as_arr().ok_or(())?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(inner.parse(Some(item))?.unwrap_or_default());
    }
    Ok(Json::Arr(out))
}

fn parse_fields(fields: &[(&'static str, S)], value: &Json) -> Result<Obj, ()> {
    let input = value.as_obj().ok_or(())?;
    let mut out = Obj(Vec::with_capacity(fields.len()));
    for (name, schema) in fields {
        if let Some(parsed) = schema.parse(input.get(name))? {
            out.0.push(((*name).to_owned(), parsed));
        }
    }
    Ok(out)
}

fn opt(inner: S) -> S {
    S::Optional(Box::new(inner))
}

fn nullish(inner: S) -> S {
    S::Nullish(Box::new(inner))
}

fn nullable(inner: S) -> S {
    S::Nullable(Box::new(inner))
}

fn arr(inner: S) -> S {
    S::Arr(Box::new(inner))
}

fn record(inner: S) -> S {
    S::Record(Box::new(inner))
}

/// `z.object` from `(key, schema)` pairs.
macro_rules! obj {
    ($($key:literal : $schema:expr),* $(,)?) => {
        S::Obj(vec![$(($key, $schema)),*])
    };
}

const STATUS: &[&str] = &["in_progress", "completed", "incomplete"];
const EXECUTION: &[&str] = &["server", "client"];
const PHASE: &[&str] = &["commentary", "final_answer"];

fn usage_schema() -> S {
    obj! {
        "input_tokens": S::Num,
        "input_tokens_details": nullish(obj! {
            "cached_tokens": nullish(S::Num),
            "cache_write_tokens": nullish(S::Num),
            "orchestration_input_tokens": nullish(S::Num),
            "orchestration_input_cached_tokens": nullish(S::Num),
        }),
        "output_tokens": S::Num,
        "output_tokens_details": nullish(obj! {
            "reasoning_tokens": nullish(S::Num),
            "orchestration_output_tokens": nullish(S::Num),
        }),
    }
}

fn reasoning_schema() -> S {
    nullish(obj! { "context": nullish(S::Str) })
}

fn incomplete_details() -> S {
    nullish(obj! { "reason": S::Str })
}

fn lifecycle_response() -> S {
    obj! {
        "id": S::Str,
        "created_at": S::Num,
        "model": S::Str,
        "service_tier": nullish(S::Str),
    }
}

fn computer_action() -> S {
    let keys = || nullish(arr(S::Str));
    S::Disc(vec![
        obj! {
            "type": S::Lit("click"),
            "button": S::Enum(&["left", "right", "wheel", "back", "forward"]),
            "x": S::Num,
            "y": S::Num,
            "keys": keys(),
        },
        obj! { "type": S::Lit("double_click"), "x": S::Num, "y": S::Num, "keys": keys() },
        obj! {
            "type": S::Lit("drag"),
            "path": arr(obj! { "x": S::Num, "y": S::Num }),
            "keys": keys(),
        },
        obj! { "type": S::Lit("keypress"), "keys": arr(S::Str) },
        obj! { "type": S::Lit("move"), "x": S::Num, "y": S::Num, "keys": keys() },
        obj! { "type": S::Lit("screenshot") },
        obj! {
            "type": S::Lit("scroll"),
            "x": S::Num,
            "y": S::Num,
            "scroll_x": S::Num,
            "scroll_y": S::Num,
            "keys": keys(),
        },
        obj! { "type": S::Lit("type"), "text": S::Str },
        obj! { "type": S::Lit("wait") },
    ])
}

fn computer_call() -> S {
    obj! {
        "type": S::Lit("computer_call"),
        "id": S::Str,
        "call_id": nullish(S::Str),
        "status": S::Enum(STATUS),
        "action": nullish(computer_action()),
        "actions": nullish(arr(computer_action())),
        "pending_safety_checks": nullish(arr(obj! {
            "id": S::Str,
            "code": nullish(S::Str),
            "message": nullish(S::Str),
        })),
    }
}

fn tool_caller() -> S {
    S::Disc(vec![
        obj! { "type": S::Lit("direct") },
        obj! { "type": S::Lit("program"), "caller_id": S::Str },
    ])
}

fn program() -> S {
    obj! {
        "type": S::Lit("program"),
        "id": S::Str,
        "call_id": S::Str,
        "code": S::Str,
        "fingerprint": S::Str,
    }
}

fn program_output() -> S {
    obj! {
        "type": S::Lit("program_output"),
        "id": S::Str,
        "call_id": S::Str,
        "result": S::Str,
        "status": S::Enum(&["completed", "incomplete"]),
    }
}

fn local_shell_call() -> S {
    obj! {
        "type": S::Lit("local_shell_call"),
        "id": S::Str,
        "call_id": S::Str,
        "action": obj! {
            "type": S::Lit("exec"),
            "command": arr(S::Str),
            "timeout_ms": opt(S::Num),
            "user": opt(S::Str),
            "working_directory": opt(S::Str),
            "env": opt(record(S::Str)),
        },
    }
}

fn apply_patch_call() -> S {
    obj! {
        "type": S::Lit("apply_patch_call"),
        "id": S::Str,
        "call_id": S::Str,
        "status": S::Enum(&["in_progress", "completed"]),
        "operation": S::Disc(vec![
            obj! { "type": S::Lit("create_file"), "path": S::Str, "diff": S::Str },
            obj! { "type": S::Lit("delete_file"), "path": S::Str },
            obj! { "type": S::Lit("update_file"), "path": S::Str, "diff": S::Str },
        ]),
    }
}

fn shell_call() -> S {
    obj! {
        "type": S::Lit("shell_call"),
        "id": S::Str,
        "call_id": S::Str,
        "status": S::Enum(STATUS),
        "action": obj! { "commands": arr(S::Str) },
    }
}

fn shell_call_output() -> S {
    obj! {
        "type": S::Lit("shell_call_output"),
        "id": S::Str,
        "call_id": S::Str,
        "status": S::Enum(STATUS),
        "output": arr(obj! {
            "stdout": S::Str,
            "stderr": S::Str,
            "outcome": S::Disc(vec![
                obj! { "type": S::Lit("timeout") },
                obj! { "type": S::Lit("exit"), "exit_code": S::Num },
            ]),
        }),
    }
}

fn tool_search_call() -> S {
    obj! {
        "type": S::Lit("tool_search_call"),
        "id": S::Str,
        "execution": S::Enum(EXECUTION),
        "call_id": nullable(S::Str),
        "status": S::Enum(STATUS),
        "arguments": S::Any,
    }
}

fn tool_search_output() -> S {
    obj! {
        "type": S::Lit("tool_search_output"),
        "id": S::Str,
        "execution": S::Enum(EXECUTION),
        "call_id": nullable(S::Str),
        "status": S::Enum(STATUS),
        "tools": arr(record(opt(S::Any))),
    }
}

fn code_outputs() -> S {
    nullable(arr(S::Disc(vec![
        obj! { "type": S::Lit("logs"), "logs": S::Str },
        obj! { "type": S::Lit("image"), "url": S::Str },
    ])))
}

fn mcp_error() -> S {
    S::Union(vec![
        S::Str,
        S::Loose(vec![
            ("type", opt(S::Str)),
            ("code", opt(S::Union(vec![S::Num, S::Str]))),
            ("message", opt(S::Str)),
        ]),
    ])
}

fn added_item() -> S {
    S::Disc(vec![
        obj! { "type": S::Lit("message"), "id": S::Str, "phase": nullish(S::Enum(PHASE)) },
        obj! { "type": S::Lit("reasoning"), "id": S::Str, "encrypted_content": nullish(S::Str) },
        obj! {
            "type": S::Lit("function_call"),
            "id": S::Str,
            "call_id": S::Str,
            "name": S::Str,
            "arguments": S::Str,
            "namespace": nullish(S::Str),
            "caller": nullish(tool_caller()),
        },
        program(),
        program_output(),
        obj! { "type": S::Lit("web_search_call"), "id": S::Str, "status": S::Str },
        computer_call(),
        obj! { "type": S::Lit("file_search_call"), "id": S::Str },
        local_shell_call(),
        obj! { "type": S::Lit("image_generation_call"), "id": S::Str },
        obj! {
            "type": S::Lit("code_interpreter_call"),
            "id": S::Str,
            "container_id": S::Str,
            "code": nullable(S::Str),
            "outputs": code_outputs(),
            "status": S::Str,
        },
        obj! {
            "type": S::Lit("mcp_call"),
            "id": S::Str,
            "status": S::Str,
            "approval_request_id": nullish(S::Str),
        },
        obj! { "type": S::Lit("mcp_list_tools"), "id": S::Str },
        obj! { "type": S::Lit("mcp_approval_request"), "id": S::Str },
        apply_patch_call(),
        obj! {
            "type": S::Lit("custom_tool_call"),
            "id": S::Str,
            "call_id": S::Str,
            "name": S::Str,
            "input": S::Str,
        },
        shell_call(),
        obj! { "type": S::Lit("compaction"), "id": S::Str, "encrypted_content": nullish(S::Str) },
        shell_call_output(),
        tool_search_call(),
        tool_search_output(),
    ])
}

fn done_item() -> S {
    S::Disc(vec![
        obj! { "type": S::Lit("message"), "id": S::Str, "phase": nullish(S::Enum(PHASE)) },
        obj! { "type": S::Lit("reasoning"), "id": S::Str, "encrypted_content": nullish(S::Str) },
        obj! {
            "type": S::Lit("function_call"),
            "id": S::Str,
            "call_id": S::Str,
            "name": S::Str,
            "arguments": S::Str,
            "status": S::Enum(STATUS),
            "namespace": nullish(S::Str),
            "caller": nullish(tool_caller()),
        },
        program(),
        program_output(),
        obj! {
            "type": S::Lit("custom_tool_call"),
            "id": S::Str,
            "call_id": S::Str,
            "name": S::Str,
            "input": S::Str,
            "status": S::Lit("completed"),
        },
        obj! {
            "type": S::Lit("code_interpreter_call"),
            "id": S::Str,
            "code": nullable(S::Str),
            "container_id": S::Str,
            "outputs": code_outputs(),
        },
        obj! { "type": S::Lit("image_generation_call"), "id": S::Str, "result": S::Str },
        obj! {
            "type": S::Lit("web_search_call"),
            "id": S::Str,
            "status": S::Str,
            "action": nullish(S::Disc(vec![
                obj! {
                    "type": S::Lit("search"),
                    "query": nullish(S::Str),
                    "queries": nullish(arr(S::Str)),
                    "sources": nullish(arr(S::Disc(vec![
                        obj! { "type": S::Lit("url"), "url": S::Str },
                        obj! { "type": S::Lit("api"), "name": S::Str },
                    ]))),
                },
                obj! { "type": S::Lit("open_page"), "url": nullish(S::Str) },
                obj! { "type": S::Lit("find_in_page"), "url": nullish(S::Str), "pattern": nullish(S::Str) },
            ])),
        },
        obj! {
            "type": S::Lit("file_search_call"),
            "id": S::Str,
            "queries": arr(S::Str),
            "results": nullish(arr(obj! {
                "attributes": record(S::Union(vec![S::Str, S::Num, S::Bool])),
                "file_id": S::Str,
                "filename": S::Str,
                "score": S::Num,
                "text": S::Str,
            })),
        },
        local_shell_call(),
        computer_call(),
        obj! {
            "type": S::Lit("mcp_call"),
            "id": S::Str,
            "status": S::Str,
            "arguments": S::Str,
            "name": S::Str,
            "server_label": S::Str,
            "output": nullish(S::Str),
            "error": nullish(mcp_error()),
            "approval_request_id": nullish(S::Str),
        },
        obj! {
            "type": S::Lit("mcp_list_tools"),
            "id": S::Str,
            "server_label": S::Str,
            "tools": arr(obj! {
                "name": S::Str,
                "description": opt(S::Str),
                "input_schema": S::Any,
                "annotations": opt(record(S::Any)),
            }),
            "error": opt(mcp_error()),
        },
        obj! {
            "type": S::Lit("mcp_approval_request"),
            "id": S::Str,
            "server_label": S::Str,
            "name": S::Str,
            "arguments": S::Str,
            "approval_request_id": opt(S::Str),
        },
        apply_patch_call(),
        shell_call(),
        obj! { "type": S::Lit("compaction"), "id": S::Str, "encrypted_content": S::Str },
        shell_call_output(),
        tool_search_call(),
        tool_search_output(),
    ])
}

/// `openaiResponsesModeledOutputItemTypes`.
const MODELED_ITEM_TYPES: &[&str] = &[
    "apply_patch_call",
    "code_interpreter_call",
    "compaction",
    "computer_call",
    "custom_tool_call",
    "file_search_call",
    "function_call",
    "image_generation_call",
    "local_shell_call",
    "mcp_approval_request",
    "mcp_call",
    "mcp_list_tools",
    "message",
    "program",
    "program_output",
    "reasoning",
    "shell_call",
    "shell_call_output",
    "tool_search_call",
    "tool_search_output",
    "web_search_call",
];

/// The union's options, by chunk type. A type not listed is an `unknown_chunk`.
struct ChunkSchemas {
    text_delta: S,
    finished: S,
    failed: S,
    created: S,
    in_progress: S,
    item_added: S,
    item_done: S,
    arguments_delta: S,
    arguments_done: S,
    custom_input_delta: S,
    partial_image: S,
    code_delta: S,
    code_done: S,
    annotation: S,
    summary_part_added: S,
    summary_text_delta: S,
    summary_part_done: S,
    patch_delta: S,
    patch_done: S,
    error: S,
}

fn chunk_schemas() -> &'static ChunkSchemas {
    static SCHEMAS: OnceLock<ChunkSchemas> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        let indexed_delta = || obj! { "type": S::Any, "item_id": S::Str, "output_index": S::Num, "delta": S::Str };
        ChunkSchemas {
            text_delta: obj! {
                "type": S::Any,
                "item_id": S::Str,
                "output_index": nullish(S::Num),
                "delta": S::Str,
                "logprobs": nullish(arr(obj! {
                    "token": S::Str,
                    "logprob": S::Num,
                    "top_logprobs": arr(obj! { "token": S::Str, "logprob": S::Num }),
                })),
            },
            finished: obj! {
                "type": S::Any,
                "response": obj! {
                    "incomplete_details": incomplete_details(),
                    "usage": usage_schema(),
                    "reasoning": reasoning_schema(),
                    "service_tier": nullish(S::Str),
                },
            },
            failed: obj! {
                "type": S::Any,
                "sequence_number": S::Num,
                "response": obj! {
                    "error": nullish(obj! { "code": nullish(S::Str), "message": S::Str }),
                    "incomplete_details": incomplete_details(),
                    "usage": nullish(usage_schema()),
                    "reasoning": reasoning_schema(),
                    "service_tier": nullish(S::Str),
                },
            },
            created: obj! { "type": S::Any, "response": lifecycle_response() },
            in_progress: obj! { "type": S::Any, "response": lifecycle_response() },
            item_added: obj! { "type": S::Any, "output_index": S::Num, "item": added_item() },
            item_done: obj! { "type": S::Any, "output_index": S::Num, "item": done_item() },
            arguments_delta: indexed_delta(),
            arguments_done: obj! { "type": S::Any, "item_id": S::Str, "output_index": S::Num, "arguments": S::Str },
            custom_input_delta: indexed_delta(),
            partial_image: obj! { "type": S::Any, "item_id": S::Str, "output_index": S::Num, "partial_image_b64": S::Str },
            code_delta: indexed_delta(),
            code_done: obj! { "type": S::Any, "item_id": S::Str, "output_index": S::Num, "code": S::Str },
            annotation: obj! {
                "type": S::Any,
                "annotation": S::Disc(vec![
                    obj! {
                        "type": S::Lit("url_citation"),
                        "start_index": S::Num,
                        "end_index": S::Num,
                        "url": S::Str,
                        "title": S::Str,
                    },
                    obj! { "type": S::Lit("file_citation"), "file_id": S::Str, "filename": S::Str, "index": S::Num },
                    obj! {
                        "type": S::Lit("container_file_citation"),
                        "container_id": S::Str,
                        "file_id": S::Str,
                        "filename": S::Str,
                        "start_index": S::Num,
                        "end_index": S::Num,
                    },
                    obj! { "type": S::Lit("file_path"), "file_id": S::Str, "index": S::Num },
                ]),
            },
            summary_part_added: obj! { "type": S::Any, "item_id": S::Str, "output_index": nullish(S::Num), "summary_index": S::Num },
            summary_text_delta: obj! {
                "type": S::Any,
                "item_id": S::Str,
                "output_index": nullish(S::Num),
                "summary_index": S::Num,
                "delta": S::Str,
            },
            summary_part_done: obj! { "type": S::Any, "item_id": S::Str, "output_index": nullish(S::Num), "summary_index": S::Num },
            patch_delta: obj! {
                "type": S::Any,
                "item_id": S::Str,
                "output_index": S::Num,
                "delta": S::Str,
                "obfuscation": nullish(S::Str),
            },
            patch_done: obj! { "type": S::Any, "item_id": S::Str, "output_index": S::Num, "diff": S::Str },
            // The nested shape is what OpenAI really sent for an early `insufficient_quota`; the flat one is what
            // it documents.
            error: S::Union(vec![
                obj! {
                    "type": S::Any,
                    "sequence_number": S::Num,
                    "error": obj! {
                        "type": S::Str,
                        "code": S::Str,
                        "message": S::Str,
                        "param": nullish(S::Str),
                    },
                },
                obj! {
                    "type": S::Any,
                    "sequence_number": S::Num,
                    "code": nullish(S::Str),
                    "message": S::Str,
                    "param": nullish(S::Str),
                },
            ]),
        }
    })
}

/// What `safeParseJSON` with `openaiResponsesChunkSchema` makes of a JSON value.
pub(super) enum Parsed {
    /// `success: false`.
    Failed,
    /// The union's fallback: `unknown_chunk`.
    Unknown,
    Chunk(Json),
}

pub(super) fn parse_chunk(raw: &Json) -> Parsed {
    let Some(kind) = raw.str_at("type") else { return Parsed::Failed };
    let schemas = chunk_schemas();
    let schema = match kind {
        "response.output_text.delta" => &schemas.text_delta,
        "response.completed" | "response.incomplete" => &schemas.finished,
        "response.failed" => &schemas.failed,
        "response.created" => &schemas.created,
        "response.in_progress" => &schemas.in_progress,
        "response.output_item.added" => &schemas.item_added,
        "response.output_item.done" => &schemas.item_done,
        "response.function_call_arguments.delta" => &schemas.arguments_delta,
        "response.function_call_arguments.done" => &schemas.arguments_done,
        "response.custom_tool_call_input.delta" => &schemas.custom_input_delta,
        "response.image_generation_call.partial_image" => &schemas.partial_image,
        "response.code_interpreter_call_code.delta" => &schemas.code_delta,
        "response.code_interpreter_call_code.done" => &schemas.code_done,
        "response.output_text.annotation.added" => &schemas.annotation,
        "response.reasoning_summary_part.added" => &schemas.summary_part_added,
        "response.reasoning_summary_text.delta" => &schemas.summary_text_delta,
        "response.reasoning_summary_part.done" => &schemas.summary_part_done,
        "response.apply_patch_call_operation_diff.delta" => &schemas.patch_delta,
        "response.apply_patch_call_operation_diff.done" => &schemas.patch_done,
        "error" => &schemas.error,
        _ => return Parsed::Unknown,
    };
    if let Ok(parsed) = schema.parse_present(raw) {
        return Parsed::Chunk(parsed);
    }
    // `isModeledOpenAIResponsesChunk`: an output item of a type the SDK does not know yet is not a failure; an
    // item that is not even an item is.
    if matches!(kind, "response.output_item.added" | "response.output_item.done") {
        if let Some(item_type) = raw.get("item").and_then(|item| item.as_obj()).and_then(|item| item.get("type")).and_then(Json::as_str) {
            if !MODELED_ITEM_TYPES.contains(&item_type) {
                return Parsed::Unknown;
            }
        }
    }
    Parsed::Failed
}

/// `openaiLanguageModelResponsesOptionsSchema`.
pub(super) fn options_schema() -> &'static S {
    static SCHEMA: OnceLock<S> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        obj! {
            "conversation": nullish(S::Str),
            "include": nullish(arr(S::Enum(&[
                "reasoning.encrypted_content",
                "file_search_call.results",
                "web_search_call.results",
                "message.output_text.logprobs",
            ]))),
            "instructions": nullish(S::Str),
            "logprobs": opt(S::Union(vec![S::Bool, S::NumRange(1.0, 20.0)])),
            "maxToolCalls": nullish(S::Num),
            "metadata": nullish(S::Any),
            "parallelToolCalls": nullish(S::Bool),
            "previousResponseId": nullish(S::Str),
            "promptCacheKey": nullish(S::Str),
            "promptCacheOptions": opt(obj! {
                "mode": opt(S::Enum(&["implicit", "explicit"])),
                "ttl": opt(S::Lit("30m")),
            }),
            "promptCacheRetention": nullish(S::Enum(&["in_memory", "24h"])),
            "reasoningEffort": nullish(S::Str),
            "reasoningMode": opt(S::Enum(&["standard", "pro"])),
            "reasoningContext": opt(S::Enum(&["auto", "current_turn", "all_turns"])),
            "reasoningSummary": nullish(S::Str),
            "safetyIdentifier": nullish(S::Str),
            "serviceTier": nullish(S::Enum(&["auto", "flex", "priority", "fast", "default"])),
            "store": nullish(S::Bool),
            "passThroughUnsupportedFiles": opt(S::Bool),
            "strictJsonSchema": nullish(S::Bool),
            "textVerbosity": nullish(S::Enum(&["low", "medium", "high"])),
            "truncation": nullish(S::Enum(&["auto", "disabled"])),
            "user": nullish(S::Str),
            "systemMessageMode": opt(S::Enum(&["system", "developer", "remove"])),
            "forceReasoning": opt(S::Bool),
            "contextManagement": nullish(arr(obj! {
                "type": S::Lit("compaction"),
                "compactThreshold": S::Num,
            })),
            "compactionTrigger": opt(S::Bool),
            "allowedTools": opt(obj! {
                "toolNames": S::NonEmptyArr(Box::new(S::Str)),
                "mode": opt(S::Enum(&["auto", "required"])),
            }),
        }
    })
}

/// `openaiResponsesReasoningProviderOptionsSchema`.
pub(super) fn reasoning_options_schema() -> &'static S {
    static SCHEMA: OnceLock<S> = OnceLock::new();
    SCHEMA.get_or_init(|| obj! { "itemId": nullish(S::Str), "reasoningEncryptedContent": nullish(S::Str) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> String {
        match parse_chunk(&Json::parse(text).unwrap()) {
            Parsed::Failed => "failed".to_owned(),
            Parsed::Unknown => "unknown".to_owned(),
            Parsed::Chunk(value) => value.to_text(),
        }
    }

    #[test]
    fn a_parsed_chunk_has_the_schema_keys_in_the_schema_order() {
        assert_eq!(
            parsed(r#"{"extra":1,"annotation":{"title":"t","url":"u","end_index":2,"start_index":1,"type":"url_citation","x":0},"item_id":"i","type":"response.output_text.annotation.added"}"#),
            r#"{"type":"response.output_text.annotation.added","annotation":{"type":"url_citation","start_index":1,"end_index":2,"url":"u","title":"t"}}"#
        );
    }

    #[test]
    fn modeled_chunks_fail_and_unmodeled_ones_pass() {
        assert_eq!(parsed(r#"{"type":"response.content_part.added"}"#), "unknown");
        assert_eq!(parsed(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"brand_new"}}"#), "unknown");
        assert_eq!(parsed(r#"{"type":"response.output_item.added","output_index":0}"#), "failed");
        assert_eq!(parsed(r#"{"type":"response.output_item.added","item":{"type":"message","id":"m"}}"#), "failed");
        assert_eq!(parsed(r#"{"type":"response.completed","response":{}}"#), "failed");
        assert_eq!(parsed(r#"{"type":"response.failed","response":{"error":{"message":"m"}}}"#), "failed");
        assert_eq!(parsed(r#"{"type":7}"#), "failed");
        assert_eq!(parsed(r#"[1]"#), "failed");
    }

    #[test]
    fn the_two_error_shapes() {
        assert_eq!(
            parsed(r#"{"type":"error","sequence_number":1,"error":{"type":"t","code":"c","message":"m","x":1}}"#),
            r#"{"type":"error","sequence_number":1,"error":{"type":"t","code":"c","message":"m"}}"#
        );
        assert_eq!(parsed(r#"{"type":"error","sequence_number":1,"message":"m","code":null}"#), r#"{"type":"error","sequence_number":1,"code":null,"message":"m"}"#);
        assert_eq!(parsed(r#"{"type":"error","message":"m"}"#), "failed");
    }

    #[test]
    fn optional_refuses_null_where_nullish_takes_it() {
        let schema = options_schema();
        let check = |text: &str| schema.parse(Some(&Json::parse(text).unwrap())).is_ok();
        assert!(check(r#"{"store":null,"reasoningSummary":null,"unknown":{}}"#));
        assert!(!check(r#"{"forceReasoning":null}"#));
        assert!(!check(r#"{"include":["web_search_call.action.sources"]}"#));
        assert!(!check(r#"{"logprobs":21}"#));
        assert!(check(r#"{"logprobs":true}"#));
        assert!(!check(r#"{"allowedTools":{"toolNames":[]}}"#));
        assert!(!check(r#""text""#));
    }
}
