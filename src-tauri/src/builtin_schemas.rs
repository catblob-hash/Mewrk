//! Authoritative model-visible JSON Schemas for built-in tools.
//!
//! Tool descriptions are empty in seeds unless supplied by a global
//! `~/.mewrk/tool-descriptions` file; schemas define what a tool is.
//!
//! Express parameter and cross-field boundaries with JSON Schema keywords. Only limits
//! JSON Schema cannot represent, such as byte limits or cross-field arithmetic, belong
//! in description prose. Numeric bounds must match host validation, not UI help text.
//! Description prose remains English regardless of UI locale.
//!
//! Descriptions are not authored in this module: root descriptions and every
//! parameter, nested item and `$defs` description are keys of the prompt-profile
//! registry (`tool.<name>.description`, `tool.<name>.param.<path>`,
//! `tool.<name>.defs.*`, the shared `tool.shell.*`, `tool.memory.*` and
//! `tool.preview.*`, and the dynamic `workspace` parameter's `tool.param.*`), so a
//! profile can reword any of them. The only prose left in code is a value handed to
//! a template as a placeholder, such as the lead phrase of a preview `serverId`.
//! `every_model_visible_description_comes_from_the_prompt_profile` pins this.
//!
//! `api.rs::tool_schema` checks descriptor-provided `input_schema` (MCP and
//! `structured_output`), this module, legacy `memory_*` schemas, then generic parameter
//! derivation.

use serde_json::{json, Value};

use crate::agents::{
    MAX_WAIT_AGENT_NAMES, WAIT_DEFAULT_TIMEOUT_SECONDS, WAIT_MAX_TIMEOUT_SECONDS,
    WAIT_MIN_TIMEOUT_SECONDS,
};
use crate::prompt_profile::{PromptKey, PromptProfile};
use crate::tool_surface::ToolVariant;
use crate::workspace_set::WorkspaceSet;
use workflow_core::MAX_SCRIPT_BYTES;

/// The `timeout` parameter text (`tool.shell.param.timeout`), rendered from the
/// constants that actually bound it so the schema cannot drift from the clamp.
/// The root description of `variant` of the built-in tool `tool`: that
/// variant's own key ([`PromptKey::for_tool_description`]).
fn root_description<'p>(tool: &str, variant: ToolVariant, profile: &'p PromptProfile) -> &'p str {
    let key = PromptKey::for_tool_description(tool, variant)
        .or_else(|| PromptKey::for_tool_description(tool, ToolVariant::Standard))
        .unwrap_or_else(|| panic!("{tool} is a built-in tool without a description key"));
    profile.text(key)
}

/// `run_in_background`'s description, shared by every shell tool: a child
/// agent's background command ends with its final reply, a top-level one
/// outlives the turn.
fn shell_run_in_background_description(variant: ToolVariant, profile: &PromptProfile) -> &str {
    profile.text(match variant {
        ToolVariant::Child => PromptKey::ToolShellChildParamRunInBackground,
        _ => PromptKey::ToolShellParamRunInBackground,
    })
}

fn shell_timeout_parameter_description(profile: &PromptProfile) -> String {
    profile.render(
        PromptKey::ToolShellParamTimeout,
        &[
            (
                "default_ms",
                &crate::tool_executor::SHELL_DEFAULT_TIMEOUT.as_millis().to_string(),
            ),
            (
                "max_ms",
                &crate::tool_executor::SHELL_MAX_TIMEOUT.as_millis().to_string(),
            ),
        ],
    )
}

/// Line ceiling both preview log tools clamp to (`preview_servers::MAX_LOG_LINES`),
/// so it is also the largest `lines` that changes anything.
const PREVIEW_MAX_LOG_LINES: u32 = 200;

/// Largest emulated viewport edge in CSS pixels. The source validates 1..=9999 and
/// names the range in its own refusal.
const PREVIEW_MAX_VIEWPORT: u32 = 9999;

fn string_prop(description: &str, max_length: usize) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": max_length,
        "description": description
    })
}

/// Which server (and therefore which page) a preview tool acts on.
///
/// Optional, deliberately, against the source schema: Claude Code marks it required
/// yet its own dispatcher documents the absent case — explicit id, else the first
/// running server for the worktree, else the session's preview page. In Mewrk the
/// preview page can exist with no dev server behind it, so a required parameter with
/// a documented absent case would be a lie.
fn server_id_prop(profile: &PromptProfile) -> Value {
    string_prop(profile.text(PromptKey::ToolPreviewParamServerId), 256)
}

/// The element a click or fill acts on, named by the uid `preview_snapshot` printed for it. The
/// other way to name it is `selector`, so neither is required.
fn snapshot_uid_prop(description: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "description": description
    })
}

/// The `serverId` of a tool that acts on one server. It says what the id is, because
/// `preview_start` does not hand it back: it is the name the model started the server by.
fn named_server_id_prop(description: &str, profile: &PromptProfile) -> Value {
    string_prop(
        &profile.render(
            PromptKey::ToolPreviewParamNamedServerId,
            &[("description", description)],
        ),
        256,
    )
}

/// The model-visible schema of `variant` of the built-in tool `name`, or `None`
/// for a name that is no built-in tool.
///
/// A tool whose contract a run property changes is a different tool in each
/// variant (see `crate::tool_surface`), so its arm branches on `variant`
/// wherever a description or a parameter differs, and every variant's text is
/// its own key. A variant the tool does not have is a caller's bug; it renders
/// the standard one.
pub(crate) fn builtin_tool_schema(
    name: &str,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Option<Value> {
    debug_assert!(
        crate::tool_surface::variants_of(name).contains(&variant),
        "{name} has no {variant:?} variant"
    );
    let schema = match name {
        // ---------------------------------------------------------------- Files
        "ls" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolLsDescription),
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "default": ".",
                    "description": profile.text(PromptKey::ToolLsParamPath)
                },
                "depth": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 8,
                    "default": 1,
                    "description": profile.text(PromptKey::ToolLsParamDepth)
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        "grep" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolGrepDescription),
            "properties": {
                "pattern": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolGrepParamPattern)
                },
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "default": ".",
                    "description": profile.text(PromptKey::ToolGrepParamPath)
                },
                "case_sensitive": {
                    "type": "boolean",
                    "default": false,
                    "description": profile.text(PromptKey::ToolGrepParamCaseSensitive)
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 250,
                    "description": profile.text(PromptKey::ToolGrepParamLimit)
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100000,
                    "default": 0,
                    "description": profile.text(PromptKey::ToolGrepParamOffset)
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        "find" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolFindDescription),
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "description": profile.text(PromptKey::ToolFindParamQuery)
                },
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "default": ".",
                    "description": profile.text(PromptKey::ToolFindParamPath)
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "read" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolReadParamPath)
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 1,
                    "description": profile.text(match variant {
                        ToolVariant::TextOnly => PromptKey::ToolReadTextOnlyParamStartLine,
                        _ => PromptKey::ToolReadParamStartLine,
                    })
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": profile.text(match variant {
                        ToolVariant::TextOnly => PromptKey::ToolReadTextOnlyParamEndLine,
                        _ => PromptKey::ToolReadParamEndLine,
                    })
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        "lsp" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolLspDescription),
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": [
                        "goToDefinition",
                        "findReferences",
                        "hover",
                        "documentSymbol",
                        "workspaceSymbol",
                        "goToImplementation",
                        "prepareCallHierarchy",
                        "incomingCalls",
                        "outgoingCalls"
                    ],
                    "description": profile.text(PromptKey::ToolLspParamOperation)
                },
                "filePath": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolLspParamFilePath)
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": profile.text(PromptKey::ToolLspParamLine)
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": profile.text(PromptKey::ToolLspParamCharacter)
                },
                "query": {
                    "type": "string",
                    "maxLength": 1024,
                    "description": profile.text(PromptKey::ToolLspParamQuery)
                }
            },
            "required": ["operation", "filePath", "line", "character"],
            "additionalProperties": false
        }),
        "write" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolWriteParamPath)
                },
                "content": {
                    "type": "string",
                    "description": profile.text(PromptKey::ToolWriteParamContent)
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        "edit" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolEditParamPath)
                },
                "find": {
                    "type": "string",
                    "minLength": 1,
                    "description": profile.text(PromptKey::ToolEditParamFind)
                },
                "replace": {
                    "type": "string",
                    "description": profile.text(PromptKey::ToolEditParamReplace)
                },
                "replace_all": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolEditParamReplaceAll)
                }
            },
            "required": ["path", "find", "replace"],
            "additionalProperties": false
        }),
        // ---------------------------------------------------------------- Commands
        "pwsh" => shell_command_schema(
            root_description(name, variant, profile),
            profile.text(PromptKey::ToolPwshParamCommand),
            variant,
            profile,
        ),
        "powershell" => shell_command_schema(
            root_description(name, variant, profile),
            profile.text(PromptKey::ToolPowershellParamCommand),
            variant,
            profile,
        ),
        "bash" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "command": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 65536,
                    "description": profile.text(PromptKey::ToolBashParamCommand)
                },
                "description": {
                    "type": "string",
                    "description": profile.text(PromptKey::ToolShellParamDescription)
                },
                "timeout": {
                    "type": "number",
                    "description": shell_timeout_parameter_description(profile)
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": shell_run_in_background_description(variant, profile)
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        "zsh" => shell_command_schema(
            root_description(name, variant, profile),
            profile.text(PromptKey::ToolZshParamCommand),
            variant,
            profile,
        ),
        "sh" => shell_command_schema(
            root_description(name, variant, profile),
            profile.text(PromptKey::ToolShParamCommand),
            variant,
            profile,
        ),

        //
        // The schemas mirror Cherry Studio's `shared/ai/builtinTools.ts`: `web_search`
        // accepts one self-contained query, `web_fetch` accepts absolute URLs, and both
        // return a result list with per-call IDs usable as `[cite:id]` references.
        "web_search" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": crate::api::WEB_SEARCH_MIN_QUERY,
                    "maxLength": crate::api::WEB_SEARCH_MAX_QUERY,
                    "description": profile.text(PromptKey::ToolWebSearchParamQuery)
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        "web_fetch" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolWebFetchDescription),
            "properties": {
                "urls": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": crate::model::MAX_SEARCH_INPUTS,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        // Do not use `format: "uri"`: strict OpenAI-compatible upstreams
                        // reject the whole request. The host validates absolute http(s) URLs.
                        "description": profile.text(PromptKey::ToolWebFetchParamUrlsItem)
                    },
                    "description": profile.text(PromptKey::ToolWebFetchParamUrls)
                }
            },
            "required": ["urls"],
            "additionalProperties": false
        }),
        // ------------------------------------------------------------- Preview
        //
        // Thirteen of these are Claude Code's preview tools, schemas and prose
        // included; `preview_upload_image` and `preview_dialog` are Mewrk's own,
        // because the source has no equivalent and dropping them would drop the
        // feature. Parameter descriptions are the source's words verbatim, including
        // `scale`'s closing sentence about `preview_click` — with `.mewrk/launch.json`
        // standing in for the source's `.claude/launch.json`, whose path belongs to a
        // product that may be installed alongside this one.
        "preview_start" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewStartDescription),
            "properties": {
                "name": string_prop(profile.text(PromptKey::ToolPreviewStartParamName), 256)
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        "preview_stop" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewStopDescription),
            "properties": {
                "serverId": named_server_id_prop("Server ID to stop", profile)
            },
            "required": ["serverId"],
            "additionalProperties": false
        }),
        "preview_list" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewListDescription),
            "properties": {},
            "additionalProperties": false
        }),
        "preview_logs" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewLogsDescription),
            "properties": {
                "serverId": named_server_id_prop("Server ID", profile),
                "level": {
                    "type": "string",
                    "enum": ["all", "error"],
                    "description": profile.text(PromptKey::ToolPreviewLogsParamLevel)
                },
                "lines": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": PREVIEW_MAX_LOG_LINES,
                    "description": profile.text(PromptKey::ToolPreviewLogsParamLines)
                },
                "search": string_prop(
                    profile.text(PromptKey::ToolPreviewLogsParamSearch),
                    512
                )
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_console_logs" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewConsoleLogsDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "level": {
                    "type": "string",
                    "enum": ["all", "error", "warn"],
                    "description": profile.text(PromptKey::ToolPreviewConsoleLogsParamLevel)
                },
                "lines": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": PREVIEW_MAX_LOG_LINES,
                    "description": profile.text(PromptKey::ToolPreviewConsoleLogsParamLines)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_screenshot" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewScreenshotDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "scale": {
                    "type": "number",
                    "minimum": 0.1,
                    "maximum": 1,
                    "description": profile.text(PromptKey::ToolPreviewScreenshotParamScale)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_snapshot" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewSnapshotDescription),
            "properties": {
                "serverId": server_id_prop(profile)
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_inspect" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewInspectDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "selector": string_prop(profile.text(PromptKey::ToolPreviewInspectParamSelector), 2048),
                "styles": {
                    "type": "array",
                    "maxItems": 64,
                    "items": {"type": "string", "minLength": 1, "maxLength": 128},
                    "description": profile.text(PromptKey::ToolPreviewInspectParamStyles)
                }
            },
            "required": ["selector"],
            "additionalProperties": false
        }),
        "preview_click" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewClickDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "selector": string_prop(profile.text(PromptKey::ToolPreviewClickParamSelector), 2048),
                "uid": snapshot_uid_prop(
                    profile.text(PromptKey::ToolPreviewClickParamUid)
                ),
                "doubleClick": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolPreviewClickParamDoubleClick)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_fill" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewFillDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "selector": string_prop(profile.text(PromptKey::ToolPreviewFillParamSelector), 2048),
                "uid": snapshot_uid_prop(
                    profile.text(PromptKey::ToolPreviewFillParamUid)
                ),
                // No `minLength`: filling with the empty string is how a field is cleared.
                "value": {
                    "type": "string",
                    "maxLength": 32768,
                    "description": profile.text(PromptKey::ToolPreviewFillParamValue)
                }
            },
            "required": ["value"],
            "additionalProperties": false
        }),
        "preview_eval" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewEvalDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "expression": string_prop(
                    profile.text(PromptKey::ToolPreviewEvalParamExpression),
                    65536
                )
            },
            "required": ["expression"],
            "additionalProperties": false
        }),
        "preview_network" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewNetworkDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "filter": {
                    "type": "string",
                    "enum": ["all", "failed"],
                    "description": profile.text(PromptKey::ToolPreviewNetworkParamFilter)
                },
                "requestId": string_prop(
                    profile.text(PromptKey::ToolPreviewNetworkParamRequestId),
                    256
                )
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_resize" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewResizeDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "preset": {
                    "type": "string",
                    "enum": ["mobile", "tablet", "desktop"],
                    "description": profile.text(PromptKey::ToolPreviewResizeParamPreset)
                },
                "width": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": PREVIEW_MAX_VIEWPORT,
                    "description": profile.text(PromptKey::ToolPreviewResizeParamWidth)
                },
                "height": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": PREVIEW_MAX_VIEWPORT,
                    "description": profile.text(PromptKey::ToolPreviewResizeParamHeight)
                },
                "colorScheme": {
                    "type": "string",
                    "enum": ["light", "dark"],
                    "description": profile.text(PromptKey::ToolPreviewResizeParamColorScheme)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "preview_upload_image" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewUploadImageDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "image_id": string_prop(
                    profile.text(PromptKey::ToolPreviewUploadImageParamImageId),
                    128
                ),
                "selector": string_prop(
                    profile.text(PromptKey::ToolPreviewUploadImageParamSelector),
                    2048
                ),
                "filename": string_prop(
                    profile.text(PromptKey::ToolPreviewUploadImageParamFilename),
                    128
                )
            },
            "required": ["image_id"],
            "additionalProperties": false
        }),
        "preview_dialog" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewDialogDescription),
            "properties": {
                "serverId": server_id_prop(profile),
                "accept": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolPreviewDialogParamAccept)
                },
                // No `minLength`: an empty answer is what a prompt dialog's own default is.
                "prompt_text": {
                    "type": "string",
                    "maxLength": 4096,
                    "description": profile.text(PromptKey::ToolPreviewDialogParamPromptText)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "agent_spawn" => agent_spawn_schema(None, false, variant, profile),
        "task_wait" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "tasks": {
                    "type": "array",
                    "maxItems": MAX_WAIT_AGENT_NAMES,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 320,
                        "description": profile.text(PromptKey::ToolTaskWaitParamTasksItem)
                    },
                    "description": profile.text(match variant {
                        ToolVariant::Child => PromptKey::ToolTaskWaitChildParamTasks,
                        _ => PromptKey::ToolTaskWaitParamTasks,
                    })
                },
                "timeout_seconds": {
                    "type": "integer",
                    "minimum": WAIT_MIN_TIMEOUT_SECONDS,
                    "maximum": WAIT_MAX_TIMEOUT_SECONDS,
                    "default": WAIT_DEFAULT_TIMEOUT_SECONDS,
                    "description": profile.text(match variant {
                        ToolVariant::Child => PromptKey::ToolTaskWaitChildParamTimeoutSeconds,
                        _ => PromptKey::ToolTaskWaitParamTimeoutSeconds,
                    })
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "task_list" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolTaskListDescription),
            "properties": {},
            "additionalProperties": false
        }),
        // One argument, always empty: the host's fabricated calls carry it, so
        // the call the model reads matches this schema, and an endpoint that
        // mishandles a tool without parameters has one to see.
        "box" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolBoxDescription),
            "properties": {
                crate::wire_history::BOX_INPUT_KEY: {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 0,
                    "description": profile.text(PromptKey::ToolBoxParamNone)
                }
            },
            "required": [crate::wire_history::BOX_INPUT_KEY],
            "additionalProperties": false
        }),
        // ---------------------------------------------------------- Long-term memory
        "read_global_memory" => memory_read_schema(
            profile.text(PromptKey::ToolReadGlobalMemoryDescription),
            profile,
        ),
        "read_project_memory" => memory_read_schema(
            profile.text(PromptKey::ToolReadProjectMemoryDescription),
            profile,
        ),
        "create_global_memory" => memory_create_schema(
            profile.text(PromptKey::ToolCreateGlobalMemoryDescription),
            profile,
        ),
        "create_project_memory" => memory_create_schema(
            profile.text(PromptKey::ToolCreateProjectMemoryDescription),
            profile,
        ),
        "edit_global_memory" => memory_edit_schema(
            profile.text(PromptKey::ToolEditGlobalMemoryDescription),
            profile,
        ),
        "edit_project_memory" => memory_edit_schema(
            profile.text(PromptKey::ToolEditProjectMemoryDescription),
            profile,
        ),
        // ------------------------------------------------------------ User interaction
        "ask_user" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolAskUserDescription),
            // Claude Code's AskUserQuestion input schema, descriptions verbatim.
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": profile.text(PromptKey::ToolAskUserParamQuestions),
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": profile.text(PromptKey::ToolAskUserParamQuestionsQuestion)
                            },
                            "header": {
                                "type": "string",
                                "description": profile.text(PromptKey::ToolAskUserParamQuestionsHeader)
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "description": profile.text(PromptKey::ToolAskUserParamQuestionsOptions),
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": profile.text(PromptKey::ToolAskUserParamQuestionsOptionsLabel)
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": profile.text(PromptKey::ToolAskUserParamQuestionsOptionsDescription)
                                        },
                                        "preview": {
                                            "type": "string",
                                            "description": profile.text(PromptKey::ToolAskUserParamQuestionsOptionsPreview)
                                        }
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false,
                                "description": profile.text(PromptKey::ToolAskUserParamQuestionsMultiSelect)
                            }
                        },
                        "required": ["question", "header", "options", "multiSelect"],
                        "additionalProperties": false
                    }
                },
                "answers": {
                    "type": "object",
                    "description": profile.text(PromptKey::ToolAskUserParamAnswers),
                    "additionalProperties": {"type": "string"}
                },
                "annotations": {
                    "type": "object",
                    "description": profile.text(PromptKey::ToolAskUserParamAnnotations),
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "preview": {
                                "type": "string",
                                "description": profile.text(PromptKey::ToolAskUserParamAnnotationsPreview)
                            },
                            "notes": {
                                "type": "string",
                                "description": profile.text(PromptKey::ToolAskUserParamAnnotationsNotes)
                            }
                        },
                        "additionalProperties": false
                    }
                },
                "metadata": {
                    "type": "object",
                    "description": profile.text(PromptKey::ToolAskUserParamMetadata),
                    "properties": {
                        "source": {
                            "type": "string",
                            "description": profile.text(PromptKey::ToolAskUserParamMetadataSource)
                        }
                    },
                    "additionalProperties": false
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
        "fork" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolForkDescription),
            "properties": {
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 32768,
                    "description": profile.text(PromptKey::ToolForkParamPrompt)
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        }),
        // ------------------------------------------------------------ Plan mode
        // Flat rather than an action `oneOf`: the Claude Agent provider publishes
        // host tools through MCP, and the plan must be writable there.
        "plan" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPlanDescription),
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["write", "read"],
                    "description": profile.text(PromptKey::ToolPlanParamAction)
                },
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 200000,
                    "description": profile.text(PromptKey::ToolPlanParamContent)
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        // A request, not a payload: the plan it acts on is the stored document,
        // and the answer is the user's.
        "exit_plan_mode" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolExitPlanModeDescription),
            "properties": {},
            "additionalProperties": false
        }),
        // ------------------------------------------------------------ Handoff
        // The memory tools' shape, pointed at the conversation's handoff
        // notebook; the index line is the `description`, as in MEMORY.md.
        "read_handoff_note" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolReadHandoffNoteDescription),
            "properties": {
                "name": memory_document_name(
                    profile.text(PromptKey::ToolReadHandoffNoteParamName)
                )
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        "create_handoff_note" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolCreateHandoffNoteDescription),
            "properties": {
                "name": memory_document_name(
                    profile.text(PromptKey::ToolCreateHandoffNoteParamName)
                ),
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "description": profile.text(PromptKey::ToolCreateHandoffNoteParamContent)
                },
                "description": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 300,
                    "description": profile.text(PromptKey::ToolCreateHandoffNoteParamDescription)
                }
            },
            "required": ["name", "content", "description"],
            "additionalProperties": false
        }),
        "edit_handoff_note" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolEditHandoffNoteDescription),
            "properties": {
                "name": memory_document_name(profile.text(PromptKey::ToolEditHandoffNoteParamName)),
                "old_text": {
                    "type": "string",
                    "minLength": 1,
                    "description": profile.text(PromptKey::ToolEditHandoffNoteParamOldText)
                },
                "new_text": {
                    "type": "string",
                    "description": profile.text(PromptKey::ToolEditHandoffNoteParamNewText)
                },
                "description": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 300,
                    "description": profile.text(PromptKey::ToolEditHandoffNoteParamDescription)
                }
            },
            "required": ["name", "old_text", "new_text", "description"],
            "additionalProperties": false
        }),
        // No arguments on purpose: the notes are the whole handoff.
        "handoff" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolHandoffDescription),
            "properties": {},
            "additionalProperties": false
        }),
        // ------------------------------------------------------------ Workflow
        // The static baseline is the permissive form used for golden-file and catalog
        // comparisons; host-generated per-run schemas apply any narrowing.
        "workflow" => workflow_schema(None, false, variant, profile),
        // ------------------------------------------------------------ Skills
        // The static baseline has no `enum`: no available skills and unknown available
        // skills are different claims, and unavailable documentation yields this schema.
        "skill" => skill_schema(profile),
        "tool_search" => tool_search_schema(profile),
        _ => return None,
    };
    Some(schema)
}

/// The schema of a shell backend's command tool: the shape `bash` spells out
/// in full, for every other backend.
fn shell_command_schema(
    description: &str,
    command_description: &str,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "command": {
                "type": "string",
                "minLength": 1,
                "maxLength": 65536,
                "description": command_description
            },
            "description": {
                "type": "string",
                "description": profile.text(PromptKey::ToolShellParamDescription)
            },
            "timeout": {
                "type": "number",
                "description": shell_timeout_parameter_description(profile)
            },
            "run_in_background": {
                "type": "boolean",
                "description": shell_run_in_background_description(variant, profile)
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// Whether a tool's arguments name a place — a path to act on, or a command
/// whose working directory and machine follow from where it runs.
///
/// `preview_start` does: which workspace's `.mewrk/launch.json` it reads
/// decides where the server runs — on that workspace's machine, an SSH machine
/// included. So do the tools that address one server
/// ([`addresses_a_preview_server`]). The page tools, the browser and memory
/// tools are absent on purpose: a conversation has one page, and a Markdown
/// store the host owns is nowhere in particular, so a workspace number would
/// advertise a choice that changes nothing.
pub(crate) fn takes_a_workspace(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "ls" | "grep" | "find" | "read" | "write" | "edit" | "lsp" | "preview_start"
    ) || addresses_a_preview_server(tool_name)
        || crate::shell_backend::ShellBackend::of_tool(tool_name).is_some()
}

/// The project memory tools, which take a `workspace` parameter of their own
/// when the conversation has more than one workspace: they name whose memory,
/// not where a call acts, so they are not among [`takes_a_workspace`].
pub(crate) fn names_a_project_memory(tool_name: &str) -> bool {
    crate::mewrk_memory::PROJECT_MEMORY_TOOL_NAMES.contains(&tool_name)
}

/// The tools that act on one dev server by its `serverId`. The id is the
/// server's launch.json name, which two workspaces can share, so the workspace
/// completes the address — but only when the name alone does not, which is why
/// their parameter has no default.
pub(crate) fn addresses_a_preview_server(tool_name: &str) -> bool {
    matches!(tool_name, "preview_stop" | "preview_logs")
}

/// Adds the `workspace` parameter naming which workspace a call acts in.
///
/// The parameter appears only when the conversation has more than one workspace.
/// With a single workspace there is nothing to choose: every call lands there,
/// and an enum of one value would be a question whose answer is already known —
/// tokens spent, and one more thing for a model to get wrong.
///
/// `enum` rather than a free integer because the allowed set is per tool. A
/// shell tool runs in one backend, and a machine has only the backends its
/// probe found, so each shell tool lists only the workspaces on machines that
/// have its shell; a `zsh` call naming a Windows workspace is not a call the
/// host could honour, and refusing it in the schema is cheaper than refusing it
/// in a tool result.
///
/// The parameter's prose comes from the run's prompt profile
/// (`tool.param.workspace*`).
pub(crate) fn with_workspace_parameter(
    mut schema: Value,
    tool_name: &str,
    workspaces: &WorkspaceSet,
    profile: &PromptProfile,
) -> Value {
    if workspaces.len() < 2 {
        return schema;
    }
    if names_a_project_memory(tool_name) {
        // Every workspace keeps its own project memory in its own folder.
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert(
                "workspace".to_owned(),
                json!({
                    "type": "integer",
                    "enum": workspaces.addresses(),
                    "default": 1,
                    "description": profile.text(PromptKey::ToolParamWorkspaceProjectMemory)
                }),
            );
        }
        return schema;
    }
    if !takes_a_workspace(tool_name) {
        return schema;
    }
    let backend = crate::shell_backend::ShellBackend::of_tool(tool_name);
    let addresses = match backend {
        Some(backend) => workspaces.shell_addresses(backend),
        None => workspaces.addresses(),
    };
    // No address means the tool has nowhere to run at all, which withdraws it
    // from the request entirely; an empty enum here would be an unsatisfiable
    // schema on a tool the model can still see.
    if addresses.is_empty() {
        return schema;
    }
    // The executor's default for a call that names none, so the number the
    // schema promises is the one the call lands on.
    let default = workspaces.default_address(tool_name);
    let addresses_a_server = addresses_a_preview_server(tool_name);
    let mut description = if addresses_a_server {
        profile.text(PromptKey::ToolParamWorkspaceServer).to_owned()
    } else {
        profile.render(
            PromptKey::ToolParamWorkspace,
            &[("default", &default.to_string())],
        )
    };
    if let Some(backend) = backend.filter(|_| addresses.len() < workspaces.len()) {
        description.push_str(&profile.render(
            PromptKey::ToolParamWorkspaceShellSuffix,
            &[("shell", backend.display_name())],
        ));
    }
    let Some(properties) = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
    else {
        return schema;
    };
    let mut parameter = json!({
        "type": "integer",
        "enum": addresses,
        "default": default,
        "description": description
    });
    if addresses_a_server {
        // Absent means "whichever workspace runs it", not workspace 1.
        if let Some(parameter) = parameter.as_object_mut() {
            parameter.remove("default");
        }
    }
    properties.insert("workspace".to_owned(), parameter);
    schema
}

fn memory_document_name(description: &str) -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": 120,
        "pattern": "^[^/\\\\]+$",
        "description": description
    })
}

fn memory_read_schema(description: &str, profile: &PromptProfile) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "name": memory_document_name(
                profile.text(PromptKey::ToolMemoryParamReadName)
            )
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

fn memory_create_schema(description: &str, profile: &PromptProfile) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "name": memory_document_name(
                profile.text(PromptKey::ToolMemoryParamCreateName)
            ),
            "content": {
                "type": "string",
                "minLength": 1,
                "description": profile.text(PromptKey::ToolMemoryParamCreateContent)
            },
            "description": {
                "type": "string",
                "minLength": 1,
                "maxLength": 300,
                "description": profile.text(PromptKey::ToolMemoryParamCreateDescription)
            }
        },
        "required": ["name", "content", "description"],
        "additionalProperties": false
    })
}

fn memory_edit_schema(description: &str, profile: &PromptProfile) -> Value {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "name": memory_document_name(
                profile.text(PromptKey::ToolMemoryParamEditName)
            ),
            "old_text": {
                "type": "string",
                "minLength": 1,
                "description": profile.text(PromptKey::ToolMemoryParamEditOldText)
            },
            "new_text": {
                "type": "string",
                "description": profile.text(PromptKey::ToolMemoryParamEditNewText)
            },
            "description": {
                "type": "string",
                "minLength": 1,
                "maxLength": 300,
                "description": profile.text(PromptKey::ToolMemoryParamEditDescription)
            }
        },
        "required": ["name", "old_text", "new_text", "description"],
        "additionalProperties": false
    })
}

/// Model-visible `workflow` schema: script body and optional parameters.
///
/// `roles` holds the named agents available for this run. `None` yields the static
/// baseline; `Some` yields a host-generated run-specific schema.
///
/// `role_required` controls whether every step must name a role. Because `agentType`
/// occurs inside JavaScript source, JSON Schema cannot enforce it; the prose must state
/// that violations throw synchronously in `workflow-script::issue_step`.
///
/// Step subagents inherit no conversation history, so the script API requires complete
/// behavioral descriptions. Limits come directly from `workflow-core` constants.
///
/// Role names use `$defs` as machine-readable legal values. It remains intentionally
/// unreferenced because the Responses adapter sends `strict: false`.
fn workflow_schema(
    roles: Option<&[String]>,
    role_required: bool,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Value {
    let mut schema = json!({
        "type": "object",
        "description": root_description("workflow", variant, profile),
        "properties": {
            "script": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_SCRIPT_BYTES,
                "description": workflow_script_description(roles, role_required, profile)
            },
            "name": {
                "type": "string",
                "pattern": "^[a-z][a-z0-9_-]{0,31}$",
                "description": profile.text(PromptKey::ToolWorkflowParamName)
            },
            "args": {
                "description": profile.text(PromptKey::ToolWorkflowParamArgs)
            },
            "token_budget": {
                "type": "integer",
                "minimum": 1,
                "description": profile.text(PromptKey::ToolWorkflowParamTokenBudget)
            },
            "resume_run_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 128,
                "pattern": "^[A-Za-z0-9_-]+$",
                "description": profile.text(PromptKey::ToolWorkflowParamResumeRunId)
            }
        },
        "required": ["name"],
        "additionalProperties": false,
    });
    if let Some(names) = roles.filter(|names| !names.is_empty()) {
        schema["$defs"] = json!({
            "agentType": {
                "description": profile.text(PromptKey::ToolWorkflowDefsAgentType),
                "enum": names
            }
        });
    }
    schema
}

/// Build the `workflow.script` description. Only the role clause varies by `roles`
/// and `role_required`.
///
/// Required-role prose must state that invalid `agentType` values throw synchronously,
/// because the value appears in JavaScript source beyond JSON Schema enforcement.
fn workflow_script_description(
    roles: Option<&[String]>,
    role_required: bool,
    profile: &PromptProfile,
) -> String {
    let agent_type_clause = profile.text(match (roles, role_required) {
        // The static baseline has no run-specific `$defs`.
        (None, _) => PromptKey::ToolWorkflowParamScriptAgentTypeUnresolved,
        // With no legal values, `agentType` remains optional.
        (Some([]), _) => PromptKey::ToolWorkflowParamScriptAgentTypeNone,
        (Some(_), false) => PromptKey::ToolWorkflowParamScriptAgentTypeOptional,
        (Some(_), true) => PromptKey::ToolWorkflowParamScriptAgentTypeRequired,
    });
    // The signature is an API shape, not prose, so it stays in code.
    let signature = if role_required {
        "- agent(prompt, opts) -> Promise<any>"
    } else {
        "- agent(prompt, opts?) -> Promise<any>"
    };
    profile.render(
        PromptKey::ToolWorkflowParamScript,
        &[
            ("signature", signature),
            ("agent_type_clause", agent_type_clause),
        ],
    )
}

/// Model-visible `agent_spawn` schema. `roles` has the same meaning as in
/// [`workflow_schema`].
///
/// When roles exist, `agent_type` uses an `enum`; when none exist, remove both the
/// unusable property and its `not` guard.
///
/// When `role_required`, `agent_type` is required and `context` is removed because
/// `context: "conversation"` and a named role are mutually exclusive across the host
/// validation layers. Fallback mode retains conversation context.
fn agent_spawn_schema(
    roles: Option<&[String]>,
    role_required: bool,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Value {
    let mut schema = json!({
        "type": "object",
        "description": root_description("agent_spawn", variant, profile),
        "properties": {
            "prompt": {
                "type": "string",
                "minLength": 1,
                "maxLength": 32768,
                "description": profile.text(PromptKey::ToolAgentSpawnParamPrompt)
            },
            "agent_type": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": profile.text(PromptKey::ToolAgentSpawnParamAgentType)
            },
            "name": {
                "type": "string",
                "pattern": "^[a-z][a-z0-9_-]{0,31}$",
                "description": profile.text(PromptKey::ToolAgentSpawnParamName)
            },
            "label": {
                "type": "string",
                "minLength": 1,
                "maxLength": 80,
                "description": profile.text(PromptKey::ToolAgentSpawnParamLabel)
            },
            "context": {
                "type": "string",
                "enum": ["none", "conversation"],
                "default": "none",
                "description": profile.text(PromptKey::ToolAgentSpawnParamContext)
            },
            "schema": {
                "type": "object",
                "description": profile.text(PromptKey::ToolAgentSpawnParamSchema)
            }
        },
        "required": ["prompt", "name"],
        "not": {
            "allOf": [
                {"required": ["agent_type"]},
                {"required": ["context"], "properties": {"context": {"const": "conversation"}}}
            ]
        },
        "additionalProperties": false
    });
    match roles {
        None => {}
        Some([]) => {
            schema["properties"]
                .as_object_mut()
                .expect("agent_spawn properties object")
                .remove("agent_type");
            // The guard references `agent_type`; remove it with the absent property.
            schema
                .as_object_mut()
                .expect("agent_spawn schema object")
                .remove("not");
        }
        Some(names) => {
            schema["properties"]["agent_type"] = json!({
                "type": "string",
                "enum": names,
                "description": profile.text(PromptKey::ToolAgentSpawnParamAgentTypeRoles)
            });
            if role_required {
                schema["required"] = json!(["prompt", "name", "agent_type"]);
                // Required `agent_type` makes `context: "conversation"` impossible, so
                // remove both `context` and its dedicated mutual-exclusion guard.
                schema["properties"]
                    .as_object_mut()
                    .expect("agent_spawn properties object")
                    .remove("context");
                schema
                    .as_object_mut()
                    .expect("agent_spawn schema object")
                    .remove("not");
            }
        }
    }
    schema
}

/// Run-specific `agent_spawn` schema. See [`agent_spawn_schema`].
pub(crate) fn agent_spawn_schema_for_roles(
    names: &[String],
    role_required: bool,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Value {
    agent_spawn_schema(Some(names), role_required, variant, profile)
}

/// Run-specific `workflow` schema. See [`workflow_schema`].
pub(crate) fn workflow_schema_for_roles(
    names: &[String],
    role_required: bool,
    variant: ToolVariant,
    profile: &PromptProfile,
) -> Value {
    workflow_schema(Some(names), role_required, variant, profile)
}

/// Removes every empty `description` string from a built-in tool's schema,
/// root included.
///
/// A profile says nothing about a tool or a parameter by giving it an empty
/// text; on the wire that is no `description` key at all rather than an empty
/// one, so a concise schema costs only what it says. Applied after every
/// rule the host appends to a description, so a description that only gained
/// a rule keeps it. Only string values are touched: a parameter that is itself
/// named `description` is an object schema and stays.
pub(crate) fn without_empty_descriptions(mut schema: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(map) => {
                if matches!(map.get("description"), Some(Value::String(text)) if text.is_empty()) {
                    map.remove("description");
                }
                for child in map.values_mut() {
                    strip(child);
                }
            }
            Value::Array(items) => {
                for item in items {
                    strip(item);
                }
            }
            _ => {}
        }
    }
    strip(&mut schema);
    schema
}

/// Model-visible name and user-provided description of an available role.
///
/// Names are represented separately as schema `enum` or `$defs` values. The host must
/// use the summary of the highest-priority applicable definition so a shadowed role's
/// description is never advertised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentRoleSummary {
    pub name: String,
    pub description: String,
}

/// Append role descriptions to a completed tool schema's top-level `description`.
///
/// Built-in tool prose resides in this field, while `ToolDescriptor.description` is
/// reserved for user overrides. Omit roles with blank descriptions and omit the block
/// entirely when no entries remain. Preserve nonblank text verbatim, including multiline
/// content; blankness follows `trim().is_empty()`. The heading and row shape come
/// from the run's prompt profile (`role.listing_heading` / `role.listing_row`).
pub(crate) fn append_role_descriptions(
    schema: &mut Value,
    roles: &[AgentRoleSummary],
    profile: &PromptProfile,
) {
    let lines = roles
        .iter()
        .filter(|role| !role.description.trim().is_empty())
        .map(|role| {
            profile.render(
                PromptKey::RoleListingRow,
                &[("name", &role.name), ("description", &role.description)],
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return;
    }
    let Some(description) = schema["description"].as_str() else {
        return;
    };
    let block = format!(
        "{description}\n\n{}\n{}",
        profile.text(PromptKey::RoleListingHeading),
        lines.join("\n")
    );
    schema["description"] = json!(block);
}

/// Schema of the child-only `subagent_update` tool, with its prose from the
/// run's prompt profile. It reaches the wire as a descriptor's own schema,
/// which the step builder passes through untouched, so a description the
/// profile leaves empty is dropped here instead.
pub(crate) fn subagent_update_schema(profile: &PromptProfile) -> Value {
    without_empty_descriptions(json!({
        "type": "object",
        "description": profile.text(PromptKey::SubagentUpdateToolDescription),
        "properties": {
            "message": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4000,
                "description": profile.text(PromptKey::SubagentUpdateMessageDescription)
            }
        },
        "required": ["message"],
        "additionalProperties": false
    }))
}

/// Model-visible `skill` schema.
///
/// It says nothing about which skills exist, and that is deliberate. A schema
/// is part of the tool set, and the tool set is re-declared on every request:
/// an `enum` of skill names would change the moment the user selected another
/// skill, mid-transcript, on protocols that will not take a changed tool set at
/// all. So the names — and what each one is for — live in the conversation's
/// own context, where a later addition is an appended message rather than a
/// rewrite. Prose comes from the profile (`skill.tool_description`,
/// `skill.name_description`); an unknown name is answered by
/// `api::run_skill_tool`, which names the ones this conversation has.
fn skill_schema(profile: &PromptProfile) -> Value {
    json!({
        "type": "object",
        "description": profile.text(PromptKey::SkillToolDescription),
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1,
                "maxLength": 240,
                "description": profile.text(PromptKey::SkillNameDescription)
            }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

/// Model-visible `tool_search` schema.
///
/// It names no tool, for the same reason [`skill_schema`] names no skill: a
/// schema is part of the tool set, the tool set is re-declared on every
/// request, and an `enum` of deferred tool names would move the moment a server
/// answered differently. The names live in the run's own announcement context,
/// where a later MCP server is an appended message rather than a rewrite, and a
/// name this run does not hold is answered by `api::run_tool_search_tool`.
fn tool_search_schema(profile: &PromptProfile) -> Value {
    json!({
        "type": "object",
        "description": profile.text(PromptKey::ToolSearchToolDescription),
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "maxLength": 2048,
                "description": profile.text(PromptKey::ToolSearchQueryDescription)
            },
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 50,
                "default": 5,
                "description": profile.text(PromptKey::ToolSearchMaxResultsDescription)
            }
        },
        "required": ["query"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::tool_catalog;
    use crate::model::{AttachedWorkspace, ExecutionEnvironmentAssets, RunTarget, SshMachineConfig};
    use std::collections::BTreeSet;

    /// A set with the host workspace first and an SSH one second, which is the
    /// shape every rule about the parameter turns on.
    fn mixed_workspaces() -> WorkspaceSet {
        let assets = ExecutionEnvironmentAssets {
            ssh_machines: vec![SshMachineConfig {
                id: "m1".into(),
                name: "devbox".into(),
                host: "user@devbox".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        WorkspaceSet::resolve(
            &assets,
            &AttachedWorkspace {
                machine: None,
                path: "C:/work/app".into(),
            },
            &[AttachedWorkspace {
                machine: Some(RunTarget::Ssh {
                    machine_id: "m1".into(),
                }),
                path: "~/services".into(),
            }],
        )
        .unwrap()
    }

    fn schema_with_workspaces(name: &str, workspaces: &WorkspaceSet) -> Value {
        let schema = builtin_tool_schema(name, ToolVariant::Standard, &PromptProfile::builtin_english())
            .unwrap_or_else(|| panic!("no schema for {name}"));
        with_workspace_parameter(schema, name, workspaces, &PromptProfile::builtin_english())
    }

    /// One workspace is not a choice, so the parameter is not a question worth
    /// asking — and an enum of one value is a token cost with no answer in it.
    #[test]
    fn a_single_workspace_adds_no_parameter() {
        let single = WorkspaceSet::local_root("C:/work/app");
        for name in ["read", "bash", "powershell", "grep"] {
            assert!(
                !properties(&schema_with_workspaces(name, &single)).contains("workspace"),
                "{name} should carry no workspace parameter"
            );
        }
    }

    #[test]
    fn every_path_and_shell_tool_gains_the_parameter_once_there_is_a_choice() {
        let workspaces = mixed_workspaces();
        for name in ["ls", "grep", "find", "read", "write", "edit", "lsp", "bash"] {
            let schema = schema_with_workspaces(name, &workspaces);
            assert!(
                properties(&schema).contains("workspace"),
                "{name} should address a workspace"
            );
            assert_eq!(schema["properties"]["workspace"]["enum"], json!([1, 2]));
            assert_eq!(schema["properties"]["workspace"]["default"], json!(1));
        }
    }

    /// The page tools, memory and orchestration tools are host subsystems rather
    /// than things that happen in a directory, so a number would advertise a
    /// choice that changes nothing.
    #[test]
    fn tools_that_do_not_act_in_a_directory_are_left_alone() {
        let workspaces = mixed_workspaces();
        for name in ["preview_list", "preview_screenshot", "ask_user"] {
            let schema = schema_with_workspaces(name, &workspaces);
            assert!(
                schema["properties"]["workspace"].is_null(),
                "{name} should not address a workspace: {schema}"
            );
        }
    }

    /// `preview_start` does name one: whose `.mewrk/launch.json` it reads, and
    /// so on which machine the server runs — an SSH machine's included.
    #[test]
    fn preview_start_names_the_workspace_whose_server_it_runs() {
        let workspaces = mixed_workspaces();
        let schema = schema_with_workspaces("preview_start", &workspaces);
        assert_eq!(schema["properties"]["workspace"]["enum"], json!([1, 2]));
        assert_eq!(schema["required"], json!(["name"]));
    }

    /// A server id is a launch.json name, which two workspaces can share, so the
    /// tools that address one server take the workspace too — with no default,
    /// because the name alone is enough whenever only one workspace runs it.
    #[test]
    fn tools_that_address_a_server_take_its_workspace_without_a_default() {
        let workspaces = mixed_workspaces();
        for name in ["preview_stop", "preview_logs"] {
            let schema = schema_with_workspaces(name, &workspaces);
            let parameter = &schema["properties"]["workspace"];
            assert_eq!(parameter["enum"], json!([1, 2]), "{name}");
            assert!(parameter["default"].is_null(), "{name}: {parameter}");
            assert!(
                parameter["description"]
                    .as_str()
                    .unwrap()
                    .contains("more than one workspace"),
                "{name}: {parameter}"
            );
        }
        let single = WorkspaceSet::local_root("C:/work/app");
        assert!(
            schema_with_workspaces("preview_stop", &single)["properties"]["workspace"].is_null()
        );
    }

    /// A shell tool lists only the workspaces whose machine has its shell: the
    /// unprobed SSH machine here is assumed to have bash alone, so `powershell`
    /// can name at most the host's workspace, and `bash` names both.
    #[test]
    fn a_shell_tool_lists_only_the_workspaces_whose_machine_has_its_shell() {
        let workspaces = mixed_workspaces();
        let schema = schema_with_workspaces("powershell", &workspaces);
        let host_has_powershell = crate::machine_shells::local()
            .get(crate::shell_backend::ShellBackend::WindowsPowerShell)
            .is_some();
        if host_has_powershell {
            assert_eq!(schema["properties"]["workspace"]["enum"], json!([1]));
            let description = schema["properties"]["workspace"]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(
                description.contains("PowerShell") && description.contains("another shell tool"),
                "a narrowed list has to say why and where to go instead: {description}"
            );
        } else {
            // No machine here has PowerShell, so the tool is withdrawn from the
            // request and never reaches this function.
            assert!(!properties(&schema).contains("workspace"));
        }
        let bash = schema_with_workspaces("bash", &workspaces);
        if crate::machine_shells::local()
            .get(crate::shell_backend::ShellBackend::Bash)
            .is_some()
        {
            assert_eq!(bash["properties"]["workspace"]["enum"], json!([1, 2]));
        }
    }

    /// A shell tool that workspace 1 cannot run defaults to the first
    /// workspace that can — the one the executor picks for a call that names
    /// none — and lists only the workspaces that can.
    #[test]
    fn a_shell_tool_defaults_to_the_first_workspace_that_has_its_shell() {
        use crate::machine_shells::{seed_for_test, DetectedShell, Endpoint, MachineShells};
        use crate::shell_backend::{MachineOs, ShellBackend};
        let machine = |id: &str| SshMachineConfig {
            id: id.into(),
            name: id.into(),
            host: format!("user@{id}"),
            ..Default::default()
        };
        let assets = ExecutionEnvironmentAssets {
            ssh_machines: vec![machine("schema-linux"), machine("schema-windows")],
            ..Default::default()
        };
        let probed = |os, shells: &[(ShellBackend, &str)]| MachineShells {
            os,
            shells: shells
                .iter()
                .map(|(backend, path)| DetectedShell { backend: *backend, path: (*path).into() })
                .collect(),
            probed_at: String::new(),
        };
        seed_for_test(
            "ssh:schema-linux",
            Some(Endpoint::of_machine(&assets.ssh_machines[0])),
            probed(MachineOs::Linux, &[(ShellBackend::Bash, "/bin/bash")]),
        );
        seed_for_test(
            "ssh:schema-windows",
            Some(Endpoint::of_machine(&assets.ssh_machines[1])),
            probed(
                MachineOs::Windows,
                &[
                    (ShellBackend::WindowsPowerShell, "powershell.exe"),
                    (ShellBackend::Bash, "bash.exe"),
                ],
            ),
        );
        let on = |id: &str, path: &str| AttachedWorkspace {
            machine: Some(RunTarget::Ssh { machine_id: id.into() }),
            path: path.into(),
        };
        let workspaces = WorkspaceSet::resolve(
            &assets,
            &on("schema-linux", "~/app"),
            &[on("schema-windows", "C:/work")],
        )
        .unwrap();

        let powershell = &schema_with_workspaces("powershell", &workspaces)["properties"]["workspace"];
        assert_eq!(powershell["enum"], json!([2]));
        assert_eq!(powershell["default"], json!(2));
        let description = powershell["description"].as_str().unwrap();
        assert!(description.contains("Defaults to 2."), "{description}");
        assert_eq!(workspaces.default_address("powershell"), 2);

        let bash = &schema_with_workspaces("bash", &workspaces)["properties"]["workspace"];
        assert_eq!(bash["enum"], json!([1, 2]));
        assert_eq!(bash["default"], json!(1));

        // That Windows machine has no PowerShell 7, so `pwsh` has nowhere to
        // run: 5.1 does not stand in for it.
        let pwsh = &schema_with_workspaces("pwsh", &workspaces)["properties"];
        assert!(pwsh.get("workspace").is_none(), "{pwsh}");
        assert!(workspaces.shell_addresses(ShellBackend::Pwsh).is_empty());
    }

    fn properties(schema: &Value) -> BTreeSet<String> {
        schema["properties"]
            .as_object()
            .expect("schema properties object")
            .keys()
            .cloned()
            .collect()
    }

    /// Role names must be machine-readable in both role-naming tool schemas:
    /// `agent_spawn` uses `agent_type.enum`; `workflow` uses `$defs.agentType` because
    /// `agentType` occurs in JavaScript source.
    #[test]
    fn configured_role_names_become_enum_values_in_both_role_naming_tools() {
        let names = vec!["alpha".to_owned(), "zeta".to_owned()];

        let spawn = agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin_english());
        assert_eq!(
            spawn["properties"]["agent_type"]["enum"],
            json!(["alpha", "zeta"])
        );
        // The `not` guard references `agent_type` and must remain with roles.
        assert!(spawn.get("not").is_some());

        let workflow = workflow_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin_english());
        assert_eq!(
            workflow["$defs"]["agentType"]["enum"],
            json!(["alpha", "zeta"])
        );
        // The script prose points to `$defs` rather than repeating role names.
        let description = workflow["properties"]["script"]["description"]
            .as_str()
            .expect("script description");
        assert!(description.contains("$defs.agentType"), "{description}");
        assert!(
            !description.contains("alpha"),
            "名字只该出现在 enum 里：{description}"
        );
    }

    /// Without roles, remove `agent_type` and its guard because the field has no legal
    /// value.
    #[test]
    fn a_conversation_without_roles_loses_the_agent_type_field_entirely() {
        let spawn = agent_spawn_schema_for_roles(&[], false, ToolVariant::Standard, &PromptProfile::builtin_english());
        assert!(spawn["properties"].get("agent_type").is_none(), "{spawn}");
        assert!(spawn.get("not").is_none(), "{spawn}");
        // Only the role selector is removed; other tool properties remain.
        assert!(spawn["properties"].get("prompt").is_some());
        assert!(spawn["properties"].get("schema").is_some());

        let workflow = workflow_schema_for_roles(&[], false, ToolVariant::Standard, &PromptProfile::builtin_english());
        assert!(workflow.get("$defs").is_none(), "{workflow}");
        let description = workflow["properties"]["script"]["description"]
            .as_str()
            .expect("script description");
        assert!(
            description.contains("no role is available to this conversation"),
            "没有角色时要说清楚，而不是继续描述一个用不了的选项：{description}"
        );
    }

    /// The `skill` schema is fixed: it names no skill and lists no trigger, so
    /// selecting another skill mid-conversation leaves the tool set untouched.
    #[test]
    fn the_skill_schema_names_no_skill_and_lists_no_trigger() {
        let schema = builtin_tool_schema("skill", ToolVariant::Standard, &PromptProfile::builtin_english())
            .expect("skill has a static schema");

        assert!(
            schema["properties"]["name"].get("enum").is_none(),
            "{schema}"
        );
        let description = schema["description"].as_str().expect("skill description");
        assert!(
            !description.contains(PromptKey::SkillListingHeading.builtin_en()),
            "{description}"
        );
        // Nor may the parameter's own prose promise a listing the schema does
        // not carry: `capabilities::runtime_context` puts it in the context.
        let name_description = schema["properties"]["name"]["description"]
            .as_str()
            .expect("name description");
        assert!(!name_description.contains("enum"), "{name_description}");
        // The static baseline must retain its complete property surface for golden and
        // catalog consistency comparisons.
        assert_eq!(schema["required"], json!(["name"]));
    }

    /// Required-role mode requires `agent_type` and removes `context` with its `not`
    /// guard because conversation context and named roles are mutually exclusive.
    ///
    #[test]
    fn requiring_a_role_puts_agent_type_in_required_and_removes_context() {
        let names = vec!["alpha".to_owned()];

        let required =
            agent_spawn_schema_for_roles(&names, true, ToolVariant::Standard, &PromptProfile::builtin_english());
        assert_eq!(
            required["required"],
            json!(["prompt", "name", "agent_type"])
        );
        assert!(
            required["properties"].get("context").is_none(),
            "{required}"
        );
        assert!(required.get("not").is_none(), "{required}");
        // Required mode retains the role enum; it narrows requiredness, not choices.
        assert_eq!(
            required["properties"]["agent_type"]["enum"],
            json!(["alpha"])
        );
        // Other properties remain.
        for key in ["prompt", "name", "label", "schema"] {
            assert!(
                required["properties"].get(key).is_some(),
                "{key}: {required}"
            );
        }

        let fallback =
            agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin_english());
        // Fallback relaxes only role selection; `prompt` and generated task address
        // `name` are required in both modes.
        assert_eq!(fallback["required"], json!(["prompt", "name"]));
        assert!(fallback["properties"].get("context").is_some());
        assert!(fallback.get("not").is_some());
    }

    /// Because `agentType` exists in JavaScript source beyond schema enforcement,
    /// required-role prose must state both the rule and synchronous failure behavior.
    #[test]
    fn requiring_a_role_states_both_the_rule_and_its_enforcement_in_the_script_prose() {
        let names = vec!["alpha".to_owned()];
        let script_prose = |required: bool| {
            workflow_schema_for_roles(&names, required, ToolVariant::Standard, &PromptProfile::builtin_english())
                ["properties"]["script"]["description"]
                .as_str()
                .expect("script description")
                .to_owned()
        };

        let required = script_prose(true);
        assert!(required.contains("REQUIRED"), "{required}");
        assert!(required.contains("$defs.agentType"), "{required}");
        assert!(
            required.contains("throws synchronously"),
            "要说清楚强制发生在哪里：{required}"
        );
        assert!(
            required.contains("NOT a step that resolves to null"),
            "要否掉「大不了这一步是 null」这个读法：{required}"
        );
        // Required mode makes `opts` non-optional in the signature.
        assert!(required.contains("agent(prompt, opts) ->"), "{required}");

        let fallback = script_prose(false);
        assert!(!fallback.contains("REQUIRED"), "{fallback}");
        assert!(fallback.contains("agent(prompt, opts?) ->"), "{fallback}");
    }

    /// Role descriptions append a titled `- <name>: <description>` list without
    /// modifying the original description.
    #[test]
    fn role_descriptions_append_a_titled_list_after_the_original_description() {
        let mut schema = json!({"description": "Tool prose.", "type": "object"});
        append_role_descriptions(
            &mut schema,
            &[
                role("reviewer", "对抗式审查：负责证伪既有结论。"),
                role("researcher", "深入检索与资料汇总。"),
            ],
            &PromptProfile::builtin_english(),
        );
        assert_eq!(
            schema["description"],
            json!(
                "Tool prose.\n\nAvailable agent types:\n\
                 - reviewer: 对抗式审查：负责证伪既有结论。\n\
                 - researcher: 深入检索与资料汇总。"
            )
        );
        // No schema key outside the description changes.
        assert_eq!(schema["type"], json!("object"));
    }

    /// Omit roles with blank descriptions; whitespace-only descriptions are blank.
    #[test]
    fn a_role_without_a_description_contributes_no_line() {
        let mut schema = json!({"description": "Tool prose."});
        append_role_descriptions(
            &mut schema,
            &[
                role("quiet", ""),
                role("spacey", "   \n  "),
                role("loud", "说点什么。"),
            ],
            &PromptProfile::builtin_english(),
        );
        assert_eq!(
            schema["description"],
            json!("Tool prose.\n\nAvailable agent types:\n- loud: 说点什么。")
        );
    }

    /// Omit the role block when no description produces an entry.
    #[test]
    fn an_all_silent_role_set_leaves_the_description_untouched() {
        for roles in [vec![], vec![role("quiet", ""), role("also-quiet", "  ")]] {
            let mut schema = json!({"description": "Tool prose."});
            append_role_descriptions(&mut schema, &roles, &PromptProfile::builtin_english());
            assert_eq!(schema["description"], json!("Tool prose."), "{roles:?}");
        }
    }

    /// Preserve multiline descriptions verbatim without trimming, wrapping, escaping,
    /// or rendering.
    #[test]
    fn a_multi_line_description_is_written_through_verbatim() {
        let mut schema = json!({"description": "Tool prose."});
        append_role_descriptions(
            &mut schema,
            &[role("multi", "第一行。\n第二行 - 带个横线。\n\n第四行。")],
            &PromptProfile::builtin_english(),
        );
        assert_eq!(
            schema["description"],
            json!(
                "Tool prose.\n\nAvailable agent types:\n\
                 - multi: 第一行。\n第二行 - 带个横线。\n\n第四行。"
            )
        );
    }

    /// Both role-naming tools share the same renderer, so their appended blocks must
    /// be byte-identical.
    #[test]
    fn both_role_naming_tools_receive_a_byte_identical_block() {
        let names = vec!["reviewer".to_owned()];
        let roles = vec![role("reviewer", "证伪既有结论。")];

        let mut spawn =
            agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin_english());
        let spawn_base = spawn["description"]
            .as_str()
            .expect("spawn description")
            .to_owned();
        append_role_descriptions(&mut spawn, &roles, &PromptProfile::builtin_english());
        let spawn_block = spawn["description"]
            .as_str()
            .expect("spawn description")
            .strip_prefix(&spawn_base)
            .expect("块必须追加在原描述之后")
            .to_owned();

        let mut workflow =
            workflow_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin_english());
        let workflow_base = workflow["description"]
            .as_str()
            .expect("workflow description")
            .to_owned();
        append_role_descriptions(&mut workflow, &roles, &PromptProfile::builtin_english());
        let workflow_block = workflow["description"]
            .as_str()
            .expect("workflow description")
            .strip_prefix(&workflow_base)
            .expect("块必须追加在原描述之后")
            .to_owned();

        assert_eq!(spawn_block, workflow_block);
        assert_eq!(
            spawn_block,
            "\n\nAvailable agent types:\n- reviewer: 证伪既有结论。"
        );
        // The machine-readable enum remains alongside the description block.
        assert_eq!(
            spawn["properties"]["agent_type"]["enum"],
            json!(["reviewer"])
        );
        assert_eq!(workflow["$defs"]["agentType"]["enum"], json!(["reviewer"]));
    }

    fn role(name: &str, description: &str) -> AgentRoleSummary {
        AgentRoleSummary {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    /// The script description must fully state isolation behavior: its sole value,
    /// committed-tree boundary, retention rule, and cost.
    #[test]
    fn the_script_description_states_what_isolation_actually_does() {
        let description = workflow_schema(None, false, ToolVariant::Standard, &PromptProfile::builtin_english())
            ["properties"]["script"]["description"]
            .as_str()
            .expect("script description")
            .to_owned();
        for needle in [
            "isolation",
            "worktree",
            "HEAD",
            "uncommitted",
            "EXPENSIVE",
            "git repository root",
        ] {
            assert!(description.contains(needle), "缺少 {needle}：{description}");
        }
    }

    #[test]
    fn every_public_tool_has_a_builtin_schema() {
        for tool in tool_catalog() {
            assert!(
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin_english()).is_some(),
                "{} lacks a hand-authored schema",
                tool.name
            );
        }
    }

    #[test]
    fn every_builtin_schema_is_a_closed_object_with_a_factual_description() {
        for tool in tool_catalog() {
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
            assert_eq!(schema["type"], "object", "{}", tool.name);
            let description = schema["description"].as_str().unwrap_or_default();
            assert!(
                !description.trim().is_empty(),
                "{} schema must state what the operation is",
                tool.name
            );
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
        }
    }

    #[test]
    fn schema_properties_match_catalog_parameters() {
        for tool in tool_catalog() {
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
            let schema_properties = properties(&schema);
            let parameters: BTreeSet<String> = tool
                .parameters
                .iter()
                .map(|parameter| parameter.name.clone())
                .collect();
            match tool.name.as_str() {
                // Permission-component fields are not Composer parameters and therefore
                // are intentionally absent from the wire schema.
                "ask_user" => {
                    assert!(schema_properties.is_superset(&parameters), "ask_user");
                }
                _ => {
                    assert_eq!(
                        schema_properties, parameters,
                        "{}: schema properties and catalog parameters drifted",
                        tool.name
                    );
                }
            }
        }
    }

    /// The preview surface is fifteen independent tools, not one multiplexed one. This
    /// pins the whole set — including the two Mewrk-only tools the source has no
    /// equivalent for — so a tool cannot be dropped or renamed without a decision.
    #[test]
    fn the_catalog_carries_every_preview_tool_and_no_multiplexed_browser_tool() {
        let names: Vec<String> = tool_catalog()
            .iter()
            .map(|tool| tool.name.clone())
            .filter(|name| name.starts_with("preview_"))
            .collect();
        assert_eq!(
            names,
            [
                "preview_start",
                "preview_stop",
                "preview_list",
                "preview_logs",
                "preview_console_logs",
                "preview_screenshot",
                "preview_snapshot",
                "preview_inspect",
                "preview_click",
                "preview_fill",
                "preview_eval",
                "preview_network",
                "preview_resize",
                "preview_upload_image",
                "preview_dialog",
            ]
        );
        assert!(!tool_catalog().iter().any(|tool| tool.name == "playwright"));
        for name in &names {
            let schema = builtin_tool_schema(name, ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
            assert!(
                schema.get("oneOf").is_none(),
                "{name} must be a flat tool, not a discriminated union"
            );
        }
    }

    /// `serverId` is optional everywhere it appears, against the source schema, because the
    /// dispatcher resolves it when absent. `preview_stop` is the exception: there is nothing
    /// to fall back to when the caller does not say which server to kill.
    #[test]
    fn server_id_is_optional_on_every_preview_tool_except_stop() {
        for tool in tool_catalog() {
            if !tool.name.starts_with("preview_") {
                continue;
            }
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
            if schema["properties"].get("serverId").is_none() {
                assert!(
                    tool.name == "preview_start" || tool.name == "preview_list",
                    "{} needs a serverId",
                    tool.name
                );
                continue;
            }
            let required: Vec<&str> = schema["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|entry| entry.as_str().expect("required entry"))
                .collect();
            assert_eq!(
                required.contains(&"serverId"),
                tool.name == "preview_stop",
                "{} serverId requiredness",
                tool.name
            );
        }
    }

    /// The launch.json template and its prose are one text, owned by
    /// `preview_launch_config`. `preview_start`'s description quotes them, and the
    /// built-in profile must quote the same bytes — a paraphrase there would teach the
    /// model a file format the parser rejects.
    #[test]
    fn preview_start_descriptions_quote_the_launch_json_format_verbatim() {
        let english = format!(
            "Start a dev server by name from .mewrk/launch.json. If .mewrk/launch.json doesn't exist, create it first with this format:\n{}\n{} Reuses the server if already running.{{?@shell}} ALWAYS use this instead of a shell command for running servers.{{/}} If the deliverable is already published as an Artifact, update the Artifact instead of starting a server to show it.",
            crate::preview_launch_config::LAUNCH_JSON_FORMAT,
            crate::preview_launch_config::LAUNCH_JSON_FORMAT_NOTES
        );
        assert_eq!(
            PromptProfile::builtin_english().text(PromptKey::ToolPreviewStartDescription),
            english
        );
    }

    #[test]
    fn required_entries_reference_declared_properties() {
        for tool in tool_catalog() {
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
            let schema_properties = properties(&schema);
            if let Some(required) = schema["required"].as_array() {
                for entry in required {
                    let name = entry.as_str().expect("required entry string");
                    assert!(
                        schema_properties.contains(name),
                        "{}: required {name} is not a declared property",
                        tool.name
                    );
                }
            }
        }
    }

    /// Sole authoritative generator for `docs/context-injections/builtin-tool-schemas.json`.
    ///
    /// Golden-file comparison keeps the design baseline current without parsing `json!`.
    ///
    /// Every variant of every tool is in it (`crate::tool_surface`): a tool a
    /// run property turns into another tool has one entry per variant, each
    /// naming its `variant`, the standard one first.
    fn baseline_document(profile: &PromptProfile) -> String {
        let catalog = tool_catalog();
        // Rendered as a run offering every tool sees it, so each sentence about
        // a sibling is in the baseline; the markers themselves are in the
        // prompt-profile export.
        let offered = crate::tool_mentions::OfferedTools::everything();
        let tools: Vec<Value> = catalog
            .iter()
            .flat_map(|tool| {
                let variants = crate::tool_surface::variants_of(&tool.name);
                let offered = &offered;
                variants.iter().map(move |variant| {
                    let mut schema =
                        builtin_tool_schema(&tool.name, *variant, profile).expect("public tool schema");
                    crate::tool_mentions::resolve_schema(&mut schema, offered);
                    let schema = without_empty_descriptions(schema);
                    if variants.len() == 1 {
                        json!({ "name": tool.name, "schema": schema })
                    } else {
                        json!({ "name": tool.name, "variant": variant.id(), "schema": schema })
                    }
                })
            })
            .collect();
        let document = json!({
            "kind": "mewrk-builtin-tool-schema-baseline",
            "note": "Model-visible parameter schemas (context layer 2: what things are, with every boundary as a JSON Schema keyword), one entry per variant of a tool whose contract a run property changes (src-tauri/src/tool_surface.rs), rendered as a run that offers every tool sees them (sentences about a sibling a run lacks are dropped there: src-tauri/src/tool_mentions.rs). Generated by builtin_schemas.rs tests; regenerate with: cargo test --lib -- builtin_schemas::tests::regenerate_builtin_schema_baseline --ignored",
            "source": "src-tauri/src/builtin_schemas.rs::builtin_tool_schema",
            "toolCount": catalog.len(),
            "variantCount": tools.len(),
            "tools": tools,
            "internalTools": {
                "subagent_update": without_empty_descriptions(subagent_update_schema(profile)),
            },
        });
        let mut rendered = serde_json::to_string_pretty(&document).expect("baseline JSON");
        rendered.push('\n');
        rendered
    }

    /// One baseline per built-in profile: the guided one, which every user
    /// file falls back to, and the concise one.
    fn baselines() -> [(&'static str, String); 2] {
        [
            ("builtin-tool-schemas.json", baseline_document(&PromptProfile::builtin_english())),
            ("builtin-tool-schemas.concise.json", baseline_document(&PromptProfile::builtin_concise())),
        ]
    }

    fn baseline_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../docs/context-injections")
            .join(name)
    }

    #[test]
    fn builtin_schema_baseline_is_current() {
        for (name, expected) in baselines() {
            let current = std::fs::read_to_string(baseline_path(name)).unwrap_or_default();
            assert!(
                current == expected,
                "docs/context-injections/{name} 已过期；运行\n  cargo test --lib -- builtin_schemas::tests::regenerate_builtin_schema_baseline --ignored\n重新生成后一并提交"
            );
        }
    }

    #[test]
    #[ignore = "writes the design baselines under docs/; run explicitly to regenerate"]
    fn regenerate_builtin_schema_baseline() {
        for (name, contents) in baselines() {
            std::fs::write(baseline_path(name), contents).expect("write baseline");
        }
    }

    #[test]
    fn empty_descriptions_leave_the_wire_but_a_parameter_named_description_stays() {
        let schema = without_empty_descriptions(json!({
            "type": "object",
            "description": "",
            "properties": {
                "description": { "type": "string", "description": "" },
                "path": { "type": "string", "description": "kept" },
                "items": { "type": "array", "items": { "type": "string", "description": "" } }
            }
        }));
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string" },
                    "path": { "type": "string", "description": "kept" },
                    "items": { "type": "array", "items": { "type": "string" } }
                }
            })
        );
    }

    #[test]
    fn task_wait_bounds_track_agents_constants() {
        let schema = builtin_tool_schema("task_wait", ToolVariant::Standard, &PromptProfile::builtin_english()).unwrap();
        let timeout = &schema["properties"]["timeout_seconds"];
        assert_eq!(timeout["minimum"], json!(WAIT_MIN_TIMEOUT_SECONDS));
        assert_eq!(timeout["maximum"], json!(WAIT_MAX_TIMEOUT_SECONDS));
        assert_eq!(timeout["default"], json!(WAIT_DEFAULT_TIMEOUT_SECONDS));
        assert_eq!(
            schema["properties"]["tasks"]["maxItems"],
            json!(MAX_WAIT_AGENT_NAMES)
        );
    }

    /// Every description a built-in schema shows the model — root, parameter, nested
    /// item and `$defs` alike, and the `workspace` parameter — is a key of the prompt
    /// profile, so a profile can reword any of them. Overriding every key with its own
    /// id proves no hard-coded English literal is left behind: a description that is
    /// not one of those markers is prose the registry cannot reach. The reverse holds
    /// too: every parameter or `$defs` key is reached by some schema, so none is dead.
    #[test]
    fn every_model_visible_description_comes_from_the_prompt_profile() {
        // A text keeps its placeholders, so a key rendered into another (the workflow
        // script into its `agentType` clause) leaves both markers in the output.
        let overrides = PromptKey::ALL
            .iter()
            .map(|key| {
                let placeholders = key
                    .placeholders()
                    .iter()
                    .map(|placeholder| format!("{{{placeholder}}}"))
                    .collect::<String>();
                (*key, format!("[profile:{}]{placeholders}", key.id()))
            })
            .collect();
        let profile = PromptProfile::from_file(
            "marked".into(),
            "Marked".into(),
            crate::model::ResolvedLanguage::EnUs,
            overrides,
            Vec::new(),
        );

        fn visit(
            value: &Value,
            path: &str,
            reached: &mut BTreeSet<String>,
            unmarked: &mut Vec<String>,
        ) {
            match value {
                Value::Object(map) => {
                    if let Some(description) = map.get("description").and_then(Value::as_str) {
                        if description.starts_with("[profile:") {
                            for (start, marker) in description.match_indices("[profile:") {
                                let rest = &description[start + marker.len()..];
                                if let Some(end) = rest.find(']') {
                                    reached.insert(rest[..end].to_owned());
                                }
                            }
                        } else {
                            unmarked.push(format!("{path}: {description}"));
                        }
                    }
                    for (key, child) in map {
                        visit(child, &format!("{path}.{key}"), reached, unmarked);
                    }
                }
                Value::Array(items) => {
                    for (index, child) in items.iter().enumerate() {
                        visit(child, &format!("{path}[{index}]"), reached, unmarked);
                    }
                }
                _ => {}
            }
        }

        let mixed = mixed_workspaces();
        let roles = vec!["alpha".to_owned()];
        let mut schemas: Vec<(String, Value)> = Vec::new();
        for tool in tool_catalog() {
            // Every variant: a variant's text is a key of its own, which only
            // that variant's schema reaches.
            for variant in crate::tool_surface::variants_of(&tool.name) {
                let schema = builtin_tool_schema(&tool.name, *variant, &profile).unwrap();
                schemas.push((
                    format!("{}[{}]+workspaces", tool.name, variant.id()),
                    with_workspace_parameter(schema.clone(), &tool.name, &mixed, &profile),
                ));
                schemas.push((format!("{}[{}]", tool.name, variant.id()), schema));
            }
        }
        for name in crate::mewrk_memory::PROJECT_MEMORY_TOOL_NAMES {
            let schema = builtin_tool_schema(name, ToolVariant::Standard, &profile).unwrap();
            schemas.push((
                format!("{name}+workspaces"),
                with_workspace_parameter(schema, name, &mixed, &profile),
            ));
        }
        for name in ["skill", "tool_search"] {
            schemas.push((name.to_owned(), builtin_tool_schema(name, ToolVariant::Standard, &profile).unwrap()));
        }
        for variant in [ToolVariant::Standard, ToolVariant::Async] {
            for required in [false, true] {
                schemas.push((
                    format!("agent_spawn[{}](roles, required={required})", variant.id()),
                    agent_spawn_schema_for_roles(&roles, required, variant, &profile),
                ));
                schemas.push((
                    format!("workflow[{}](roles, required={required})", variant.id()),
                    workflow_schema_for_roles(&roles, required, variant, &profile),
                ));
            }
            schemas.push((
                format!("agent_spawn[{}](no roles)", variant.id()),
                agent_spawn_schema_for_roles(&[], false, variant, &profile),
            ));
            schemas.push((
                format!("workflow[{}](no roles)", variant.id()),
                workflow_schema_for_roles(&[], false, variant, &profile),
            ));
        }
        schemas.push(("subagent_update".to_owned(), subagent_update_schema(&profile)));

        let mut reached = BTreeSet::new();
        let mut unmarked = Vec::new();
        for (name, schema) in &schemas {
            visit(schema, name, &mut reached, &mut unmarked);
        }
        assert!(
            unmarked.is_empty(),
            "descriptions the prompt profile cannot reach:\n{}",
            unmarked.join("\n")
        );
        // The shell suffix only appears where a machine lacks the shell, which depends
        // on what this host has installed; the other `workspace` keys do not.
        let reached_only_on_some_hosts = ["tool.param.workspace_shell_suffix"];
        // Root descriptions count too, every variant's included: a variant key
        // no schema reaches is a variant the code forgot to branch to.
        let dead: Vec<&str> = PromptKey::ALL
            .iter()
            .map(|key| key.id())
            .filter(|id| {
                id.starts_with("tool.")
                    && (id.contains(".param.") || id.contains(".defs.") || id.ends_with(".description"))
            })
            .filter(|id| !reached_only_on_some_hosts.contains(id) && !reached.contains(*id))
            .collect();
        assert!(dead.is_empty(), "schema keys no built-in schema uses: {dead:?}");
    }
}
