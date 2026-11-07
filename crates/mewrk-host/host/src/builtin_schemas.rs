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
//! `tool.<name>.defs.*`, the shared `tool.shell.*`, `tool.memory.*`,
//! `tool.preview.*` and `tool.screen.*`, and the dynamic `workspace`
//! parameter's `tool.param.*`), so a
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

/// Most actions one `preview_input` call may carry: the executor's own bound.
const PREVIEW_INPUT_MAX_ACTIONS: usize = crate::remote_control::session::MAX_ACTIONS;
/// The longest `ms` any action takes: a `long_press` or a `drag` may last the whole batch budget.
/// A `wait` is held to [`crate::remote_control::session::MAX_WAIT_MS`] by the executor, which one
/// flat schema cannot say per action, so the root description says it. The shortest is 1, which
/// every action but a `wait` demands — and a wait of nothing is not worth a call.
const PREVIEW_INPUT_MAX_MS: u64 = crate::remote_control::session::MAX_BATCH_MS;
/// The executor's bounds on the rest (`remote_control::session::parse_actions`), which it keeps
/// private; `preview_input_bounds_are_the_executors` holds these to them.
const PREVIEW_INPUT_MAX_COORDINATE: u32 = 100_000;
const PREVIEW_INPUT_MAX_NOTCHES: i32 = 50;
const PREVIEW_INPUT_MAX_CLICKS: u32 = 3;
const PREVIEW_INPUT_MAX_REPEAT: u32 = 50;
/// The longest `modifiers` string a click, press, drag or scroll takes: names joined with `+`.
const PREVIEW_INPUT_MAX_MODIFIERS: usize = 64;

// What the remote-control screen tools drive, as `tool.screen.param.server_id` says it
// (`{screen}`): one phrase per family of tools, the same eight tools share one key.
/// The screen `preview_zoom` and `preview_wait` act on: any remote-control screen.
const SCREEN_ANY: &str = "remote-control screen";
/// The screen `screen_app` and `screen_device` act on: a phone or tablet.
const SCREEN_PHONE: &str = "Android device, iPhone or iPad";
/// The screen `screen_clipboard` acts on: an iPhone, iPad or desktop.
const SCREEN_CLIPBOARD: &str = "iPhone, iPad or desktop";
/// The screen `screen_package`, `android_logcat` and `android_shell` act on.
const SCREEN_ANDROID: &str = "Android device";

/// The longest package name any Android tool takes, and what `pm` and `am` accept as one.
const ANDROID_PACKAGE_MAX: usize = 255;
/// The longest URL or deep link an app tool opens.
const SCREEN_URL_MAX: usize = 2048;
/// The longest text `screen_clipboard` writes: 100,000 characters. A desktop takes less
/// (`PREVIEW_INPUT_PASTE_MAX_TEXT_CHARS`, the same ceiling the desktop executor holds to),
/// because a VNC server drops clipboard text past 256 KiB; the description says so.
const SCREEN_CLIPBOARD_MAX: usize = 100_000;
/// The longest text a `preview_input` `type` action carries with `via` "paste": 60,000
/// characters, at most 240,001 bytes as UTF-8 with CRLF line ends and the NUL, within the
/// 256 KiB TigerVNC takes by default (`remote_control::vnc::MAX_SENT_CLIPBOARD`). The
/// default `via` "auto" keeps the executor's 2000
/// ([`crate::remote_control::session::MAX_TEXT_CHARS`]), which one flat schema cannot say per
/// action, so the root description says it.
const PREVIEW_INPUT_PASTE_MAX_TEXT_CHARS: usize = 60_000;
/// The largest screen coordinate or extent `preview_zoom` and `preview_wait` take, in the
/// frame of the latest screenshot; the same bound `preview_input` puts on a coordinate.
const SCREEN_MAX_COORDINATE: u32 = 100_000;

/// The `actions` items of `preview_input`: one flat object whose `action` picks which of the other
/// fields mean anything, so the wire shape stays a plain object rather than a `oneOf` union (the
/// `the_catalog_carries_every_preview_tool_and_no_multiplexed_browser_tool` rule). Which fields an
/// action needs is the root description's business and the executor's check; every bound the
/// schema states is the executor's, so a call the schema admits is never refused for a value it
/// allowed, and one a provider holds to the schema is never denied a value the executor takes.
fn preview_input_action_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["click", "long_press", "drag", "scroll", "move", "type", "key", "wait"]
            },
            "x": {"type": "number", "minimum": 0, "maximum": PREVIEW_INPUT_MAX_COORDINATE},
            "y": {"type": "number", "minimum": 0, "maximum": PREVIEW_INPUT_MAX_COORDINATE},
            "toX": {"type": "number", "minimum": 0, "maximum": PREVIEW_INPUT_MAX_COORDINATE},
            "toY": {"type": "number", "minimum": 0, "maximum": PREVIEW_INPUT_MAX_COORDINATE},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "count": {"type": "integer", "minimum": 1, "maximum": PREVIEW_INPUT_MAX_CLICKS},
            "ms": {"type": "integer", "minimum": 1, "maximum": PREVIEW_INPUT_MAX_MS},
            "dx": {
                "type": "integer",
                "minimum": -PREVIEW_INPUT_MAX_NOTCHES,
                "maximum": PREVIEW_INPUT_MAX_NOTCHES
            },
            "dy": {
                "type": "integer",
                "minimum": -PREVIEW_INPUT_MAX_NOTCHES,
                "maximum": PREVIEW_INPUT_MAX_NOTCHES
            },
            // The longest text any `type` takes: `via` "paste" puts it on a desktop's clipboard.
            // The default `via` "auto" is held to the executor's 2000 (`parse_actions`), which
            // the root description says; a flat schema cannot tie the bound to a sibling field.
            "text": {"type": "string", "maxLength": PREVIEW_INPUT_PASTE_MAX_TEXT_CHARS},
            "via": {"type": "string", "enum": ["auto", "paste"]},
            "chord": {"type": "string", "minLength": 1, "maxLength": 64},
            "key": {"type": "string", "maxLength": 64},
            "repeat": {"type": "integer", "minimum": 1, "maximum": PREVIEW_INPUT_MAX_REPEAT},
            // Held for the action on a click, press, drag or scroll of a desktop: modifier names
            // joined with `+`. A phone has no keyboard to hold them on, which the executor says.
            "modifiers": {"type": "string", "minLength": 1, "maxLength": PREVIEW_INPUT_MAX_MODIFIERS}
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

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

/// The `serverId` of the remote-control screen tools: which screen the tool can drive to act
/// on. Optional like the preview tools': the host finds the screen the preview pane shows,
/// else the only one connected that the tool can drive. `screen` is what those screens are
/// called ([`SCREEN_ANY`] and its siblings).
fn screen_server_id_prop(screen: &str, profile: &PromptProfile) -> Value {
    string_prop(
        &profile.render(PromptKey::ToolScreenParamServerId, &[("screen", screen)]),
        256,
    )
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
                "glob": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 1024,
                    "description": profile.text(PromptKey::ToolGrepParamGlob)
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "default": "files_with_matches",
                    "description": profile.text(PromptKey::ToolGrepParamOutputMode)
                },
                "case_sensitive": {
                    "type": "boolean",
                    "default": true,
                    "description": profile.text(PromptKey::ToolGrepParamCaseSensitive)
                },
                "context": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "description": profile.text(PromptKey::ToolGrepParamContext)
                },
                "before": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "description": profile.text(PromptKey::ToolGrepParamBefore)
                },
                "after": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 20,
                    "description": profile.text(PromptKey::ToolGrepParamAfter)
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
        // included; `preview_upload_image`, `preview_dialog` and `preview_input`
        // are Mewrk's own, because the source has no equivalent and dropping
        // them would drop the feature. Parameter descriptions are the source's words verbatim, including
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
                },
                "button": string_prop(profile.text(PromptKey::ToolPreviewDialogParamButton), 256)
            },
            "required": [],
            "additionalProperties": false
        }),
        // Mewrk's own, like the two above: the source drives pages by selector or uid and has
        // nothing that taps a pixel of a phone or a desktop. `actions` is required; an empty list
        // would do nothing, so `minItems` says so.
        "preview_input" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewInputDescription),
            "properties": {
                "serverId": string_prop(profile.text(PromptKey::ToolPreviewInputParamServerId), 256),
                "actions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": PREVIEW_INPUT_MAX_ACTIONS,
                    "items": preview_input_action_schema()
                },
                "screenshot": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolPreviewInputParamScreenshot)
                }
            },
            "required": ["actions"],
            "additionalProperties": false
        }),
        // ------------------------------------------------- Remote-control groups
        //
        // Mewrk's own, like `preview_input`: the tools of a screen `preview_start` connected,
        // which the source has nothing like. Six act on any remote-control screen or on a phone,
        // tablet or desktop among them (`preview_zoom`, `preview_wait`, `screen_app`,
        // `screen_device`, `screen_clipboard`, `screen_package`), two on an Android device alone;
        // each takes the `serverId` of a screen it can drive, optional because the host finds the
        // one the preview pane shows or the only one connected. Every bound is the executor's
        // (`remote_control::{android,ios,desktop}` and `tool_executor::screen_tools`), which parse
        // the same numbers; `screen_tool_schemas_state_the_specs_bounds` pins them. The
        // `workspace` parameter is `screen_package`'s alone, for the APK path
        // ([`takes_a_workspace`]).
        "preview_zoom" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewZoomDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_ANY, profile),
                "x": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewZoomParamX)
                },
                "y": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewZoomParamY)
                },
                "width": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewZoomParamWidth)
                },
                "height": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewZoomParamHeight)
                }
            },
            "required": ["x", "y", "width", "height"],
            "additionalProperties": false
        }),
        // The region is all four of `x`, `y`, `width` and `height` or none, which one flat
        // object cannot say; the description does, and the executor refuses a partial one.
        "preview_wait" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolPreviewWaitDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_ANY, profile),
                "until": {
                    "type": "string",
                    "enum": ["change", "still"],
                    "default": "change",
                    "description": profile.text(PromptKey::ToolPreviewWaitParamUntil)
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 100,
                    "maximum": 20000,
                    "default": 5000,
                    "description": profile.text(PromptKey::ToolPreviewWaitParamTimeout)
                },
                "x": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewWaitParamX)
                },
                "y": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewWaitParamY)
                },
                "width": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewWaitParamWidth)
                },
                "height": {
                    "type": "number",
                    "minimum": 1,
                    "maximum": SCREEN_MAX_COORDINATE,
                    "description": profile.text(PromptKey::ToolPreviewWaitParamHeight)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "screen_app" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolScreenAppDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_PHONE, profile),
                "action": {
                    "type": "string",
                    "enum": [
                        "list",
                        "foreground",
                        "info",
                        "launch",
                        "activate",
                        "stop",
                        "open_url"
                    ],
                    "default": "list",
                    "description": profile.text(PromptKey::ToolScreenAppParamAction)
                },
                "app": string_prop(profile.text(PromptKey::ToolScreenAppParamApp), 255),
                "url": string_prop(profile.text(PromptKey::ToolScreenAppParamUrl), SCREEN_URL_MAX),
                "filter": string_prop(profile.text(PromptKey::ToolScreenAppParamFilter), 200),
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "default": 200,
                    "description": profile.text(PromptKey::ToolScreenAppParamLimit)
                },
                "scope": {
                    "type": "string",
                    "enum": ["launchable", "third_party", "all"],
                    "default": "launchable",
                    "description": profile.text(PromptKey::ToolScreenAppParamScope)
                },
                "activity": string_prop(profile.text(PromptKey::ToolScreenAppParamActivity), 255),
                "restart": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolScreenAppParamRestart)
                },
                "arguments": {
                    "type": "array",
                    "maxItems": 32,
                    "items": {"type": "string", "maxLength": 1024},
                    "description": profile.text(PromptKey::ToolScreenAppParamArguments)
                },
                "environment": {
                    "type": "object",
                    "maxProperties": 32,
                    "additionalProperties": {"type": "string", "maxLength": 1024},
                    "description": profile.text(PromptKey::ToolScreenAppParamEnvironment)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        "screen_device" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolScreenDeviceDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_PHONE, profile),
                "action": {
                    "type": "string",
                    "enum": [
                        "info",
                        "lock",
                        "unlock",
                        "wake",
                        "rotate",
                        "appearance",
                        "animations",
                        "location",
                        "clear_location",
                        "notifications",
                        "quick_settings",
                        "collapse",
                        "stay_awake",
                        "dismiss_keyboard"
                    ],
                    "default": "info",
                    "description": profile.text(PromptKey::ToolScreenDeviceParamAction)
                },
                "value": string_prop(profile.text(PromptKey::ToolScreenDeviceParamValue), 32),
                "sections": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "uniqueItems": true,
                    "items": {
                        "type": "string",
                        "enum": ["device", "display", "power", "notifications"]
                    },
                    "description": profile.text(PromptKey::ToolScreenDeviceParamSections)
                },
                "latitude": {
                    "type": "number",
                    "minimum": -90,
                    "maximum": 90,
                    "description": profile.text(PromptKey::ToolScreenDeviceParamLatitude)
                },
                "longitude": {
                    "type": "number",
                    "minimum": -180,
                    "maximum": 180,
                    "description": profile.text(PromptKey::ToolScreenDeviceParamLongitude)
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        // No `minLength` on `text`: writing the empty string is how the clipboard is cleared.
        "screen_clipboard" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolScreenClipboardDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_CLIPBOARD, profile),
                "action": {
                    "type": "string",
                    "enum": ["read", "write"],
                    "description": profile.text(PromptKey::ToolScreenClipboardParamAction)
                },
                "text": {
                    "type": "string",
                    "maxLength": SCREEN_CLIPBOARD_MAX,
                    "description": profile.text(PromptKey::ToolScreenClipboardParamText)
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        "screen_package" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolScreenPackageDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_ANDROID, profile),
                "action": {
                    "type": "string",
                    "enum": ["install", "uninstall", "clear_data", "grant", "revoke"],
                    "description": profile.text(PromptKey::ToolScreenPackageParamAction)
                },
                "path": string_prop(profile.text(PromptKey::ToolScreenPackageParamPath), 4096),
                "app": string_prop(
                    profile.text(PromptKey::ToolScreenPackageParamApp),
                    ANDROID_PACKAGE_MAX
                ),
                "permission": string_prop(
                    profile.text(PromptKey::ToolScreenPackageParamPermission),
                    255
                ),
                "keep_data": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolScreenPackageParamKeepData)
                },
                "replace_existing": {
                    "type": "boolean",
                    "default": true,
                    "description": profile.text(PromptKey::ToolScreenPackageParamReplaceExisting)
                },
                "downgrade": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolScreenPackageParamDowngrade)
                },
                "grant_permissions": {
                    "type": "boolean",
                    "description": profile.text(PromptKey::ToolScreenPackageParamGrantPermissions)
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        "android_logcat" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolAndroidLogcatDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_ANDROID, profile),
                "lines": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2000,
                    "default": 200,
                    "description": profile.text(PromptKey::ToolAndroidLogcatParamLines)
                },
                "level": {
                    "type": "string",
                    "enum": ["V", "D", "I", "W", "E", "F"],
                    "description": profile.text(PromptKey::ToolAndroidLogcatParamLevel)
                },
                "tag": string_prop(profile.text(PromptKey::ToolAndroidLogcatParamTag), 128),
                "package": string_prop(
                    profile.text(PromptKey::ToolAndroidLogcatParamPackage),
                    ANDROID_PACKAGE_MAX
                ),
                "buffer": {
                    "type": "string",
                    "enum": ["main", "system", "crash", "events", "all"],
                    "description": profile.text(PromptKey::ToolAndroidLogcatParamBuffer)
                },
                "contains": string_prop(profile.text(PromptKey::ToolAndroidLogcatParamContains), 200)
            },
            "required": [],
            "additionalProperties": false
        }),
        "android_shell" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolAndroidShellDescription),
            "properties": {
                "serverId": screen_server_id_prop(SCREEN_ANDROID, profile),
                "command": string_prop(profile.text(PromptKey::ToolAndroidShellParamCommand), 8192),
                "timeout": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": 20000,
                    "default": 10000,
                    "description": profile.text(PromptKey::ToolAndroidShellParamTimeout)
                }
            },
            "required": ["command"],
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
        "task_stop" => json!({
            "type": "object",
            "description": root_description(name, variant, profile),
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 320,
                    "description": profile.text(PromptKey::ToolTaskStopParamTask)
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        // Claude Code's `SendMessage` shape: who, what, and an optional preview.
        // `to` is the name `agent_spawn` took, so it carries that pattern.
        "send_message" => json!({
            "type": "object",
            "description": profile.text(PromptKey::ToolSendMessageDescription),
            "properties": {
                "to": {
                    "type": "string",
                    "pattern": "^[a-z][a-z0-9_-]{0,31}$",
                    "description": profile.text(PromptKey::ToolSendMessageParamTo)
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 32768,
                    "description": profile.text(PromptKey::ToolSendMessageParamMessage)
                },
                "summary": {
                    "type": "string",
                    "maxLength": 200,
                    "description": profile.text(PromptKey::ToolSendMessageParamSummary)
                }
            },
            "required": ["to", "message"],
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
///
/// `screen_package` does too, for one argument: `install` reads an `.apk` from
/// the selected workspace by `path`, and a relative path means nothing until it
/// is known whose root it is relative to (an absolute one is still held to that
/// workspace's path guard). The rest of the screen and Android tools act on
/// a screen found by `serverId` and are absent for the page tools' reason; the
/// device a package tool acts on is not chosen by workspace, only the file is.
pub(crate) fn takes_a_workspace(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "ls" | "grep"
            | "find"
            | "read"
            | "write"
            | "edit"
            | "lsp"
            | "preview_start"
            | "screen_package"
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
        let schema = builtin_tool_schema(name, ToolVariant::Standard, &PromptProfile::builtin())
            .unwrap_or_else(|| panic!("no schema for {name}"));
        with_workspace_parameter(schema, name, workspaces, &PromptProfile::builtin())
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
            let why = PromptProfile::builtin().render(
                PromptKey::ToolParamWorkspaceShellSuffix,
                &[(
                    "shell",
                    crate::shell_backend::ShellBackend::WindowsPowerShell.display_name(),
                )],
            );
            assert!(
                description.contains("PowerShell") && description.ends_with(&why),
                "a narrowed list has to say why: {description}"
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
        assert_eq!(workspaces.default_address("powershell"), 2);
        // The built-in wording leaves the default to the schema's `default`; a
        // profile that names it in prose gets the same number filled in.
        let stating = PromptProfile::from_file(
            "stating".into(),
            "Stating".into(),
            crate::model::ResolvedLanguage::EnUs,
            [(PromptKey::ToolParamWorkspace, "Workspace number. Defaults to {default}.".to_owned())]
                .into(),
            Vec::new(),
        );
        let stated = with_workspace_parameter(
            builtin_tool_schema("powershell", ToolVariant::Standard, &stating).unwrap(),
            "powershell",
            &workspaces,
            &stating,
        );
        let description = stated["properties"]["workspace"]["description"].as_str().unwrap();
        assert!(description.contains("Defaults to 2."), "{description}");

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

        let spawn = agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin());
        assert_eq!(
            spawn["properties"]["agent_type"]["enum"],
            json!(["alpha", "zeta"])
        );
        // The `not` guard references `agent_type` and must remain with roles.
        assert!(spawn.get("not").is_some());

        let workflow = workflow_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin());
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
        let spawn = agent_spawn_schema_for_roles(&[], false, ToolVariant::Standard, &PromptProfile::builtin());
        assert!(spawn["properties"].get("agent_type").is_none(), "{spawn}");
        assert!(spawn.get("not").is_none(), "{spawn}");
        // Only the role selector is removed; other tool properties remain.
        assert!(spawn["properties"].get("prompt").is_some());
        assert!(spawn["properties"].get("schema").is_some());

        let workflow = workflow_schema_for_roles(&[], false, ToolVariant::Standard, &PromptProfile::builtin());
        assert!(workflow.get("$defs").is_none(), "{workflow}");
        let description = workflow["properties"]["script"]["description"]
            .as_str()
            .expect("script description");
        assert!(
            description.contains("no roles: any value throws"),
            "没有角色时要说清楚，而不是继续描述一个用不了的选项：{description}"
        );
    }

    /// The `skill` schema is fixed: it names no skill and lists no trigger, so
    /// selecting another skill mid-conversation leaves the tool set untouched.
    #[test]
    fn the_skill_schema_names_no_skill_and_lists_no_trigger() {
        let schema = builtin_tool_schema("skill", ToolVariant::Standard, &PromptProfile::builtin())
            .expect("skill has a static schema");

        assert!(
            schema["properties"]["name"].get("enum").is_none(),
            "{schema}"
        );
        let description = schema["description"].as_str().expect("skill description");
        assert!(
            !description.contains(PromptKey::SkillListingHeading.builtin_text()),
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
            agent_spawn_schema_for_roles(&names, true, ToolVariant::Standard, &PromptProfile::builtin());
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
            agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin());
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
            workflow_schema_for_roles(&names, required, ToolVariant::Standard, &PromptProfile::builtin())
                ["properties"]["script"]["description"]
                .as_str()
                .expect("script description")
                .to_owned()
        };

        let required = script_prose(true);
        assert!(required.contains("required on every agent() call"), "{required}");
        assert!(required.contains("$defs.agentType"), "{required}");
        assert!(
            required.contains("throws at the call"),
            "要说清楚强制发生在哪里：{required}"
        );
        assert!(
            required.contains("instead of yielding a null step"),
            "要否掉「大不了这一步是 null」这个读法：{required}"
        );
        // Required mode makes `opts` non-optional in the signature.
        assert!(required.contains("agent(prompt, opts) ->"), "{required}");

        let fallback = script_prose(false);
        assert!(!fallback.contains("required on every agent() call"), "{fallback}");
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
            &PromptProfile::builtin(),
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
            &PromptProfile::builtin(),
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
            append_role_descriptions(&mut schema, &roles, &PromptProfile::builtin());
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
            &PromptProfile::builtin(),
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
            agent_spawn_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin());
        let spawn_base = spawn["description"]
            .as_str()
            .expect("spawn description")
            .to_owned();
        append_role_descriptions(&mut spawn, &roles, &PromptProfile::builtin());
        let spawn_block = spawn["description"]
            .as_str()
            .expect("spawn description")
            .strip_prefix(&spawn_base)
            .expect("块必须追加在原描述之后")
            .to_owned();

        let mut workflow =
            workflow_schema_for_roles(&names, false, ToolVariant::Standard, &PromptProfile::builtin());
        let workflow_base = workflow["description"]
            .as_str()
            .expect("workflow description")
            .to_owned();
        append_role_descriptions(&mut workflow, &roles, &PromptProfile::builtin());
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
        let description = workflow_schema(None, false, ToolVariant::Standard, &PromptProfile::builtin())
            ["properties"]["script"]["description"]
            .as_str()
            .expect("script description")
            .to_owned();
        for needle in [
            "isolation",
            "worktree",
            "HEAD",
            "uncommitted",
            "kept if the step changed or committed anything",
            "Costs a full checkout per step",
            "git repository root",
        ] {
            assert!(description.contains(needle), "缺少 {needle}：{description}");
        }
    }

    #[test]
    fn every_public_tool_has_a_builtin_schema() {
        for tool in tool_catalog() {
            assert!(
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin()).is_some(),
                "{} lacks a hand-authored schema",
                tool.name
            );
        }
    }

    /// Every schema is a closed object whose root description is its tool's
    /// description key. The built-in profile leaves some of those empty on
    /// purpose; a profile that states what each operation is reaches every
    /// tool through that same slot.
    #[test]
    fn every_builtin_schema_is_a_closed_object_with_a_factual_description() {
        let key_of = |name: &str| {
            PromptKey::for_tool_description(name, ToolVariant::Standard)
                .unwrap_or_else(|| panic!("{name} has no description key"))
        };
        let stating = PromptProfile::from_file(
            "stating".into(),
            "Stating".into(),
            crate::model::ResolvedLanguage::EnUs,
            tool_catalog()
                .iter()
                .map(|tool| (key_of(&tool.name), format!("What {} does.", tool.name)))
                .collect(),
            Vec::new(),
        );
        let builtin = PromptProfile::builtin();
        for tool in tool_catalog() {
            for profile in [&builtin, &stating] {
                let schema = builtin_tool_schema(&tool.name, ToolVariant::Standard, profile).unwrap();
                assert_eq!(schema["type"], "object", "{}", tool.name);
                assert_eq!(
                    schema["description"].as_str(),
                    Some(profile.text(key_of(&tool.name))),
                    "{} schema must carry its description key",
                    tool.name
                );
                assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
            }
            let stated = builtin_tool_schema(&tool.name, ToolVariant::Standard, &stating).unwrap();
            assert_eq!(
                stated["description"],
                json!(format!("What {} does.", tool.name)),
                "{} schema must state what the operation is",
                tool.name
            );
        }
    }

    #[test]
    fn schema_properties_match_catalog_parameters() {
        for tool in tool_catalog() {
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin()).unwrap();
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

    /// The preview surface is eighteen independent tools, not one multiplexed one. This
    /// pins the whole set — including the Mewrk-only tools the source has no equivalent
    /// for (`preview_input`, `preview_zoom`, `preview_wait`) — so a tool cannot be dropped
    /// or renamed without a decision.
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
                "preview_input",
                "preview_zoom",
                "preview_wait",
            ]
        );
        assert!(!tool_catalog().iter().any(|tool| tool.name == "playwright"));
        for name in &names {
            let schema = builtin_tool_schema(name, ToolVariant::Standard, &PromptProfile::builtin()).unwrap();
            assert!(
                schema.get("oneOf").is_none(),
                "{name} must be a flat tool, not a discriminated union"
            );
        }
    }

    /// Every bound `preview_input`'s schema states is the executor's (`parse_actions`): the edge
    /// value is accepted there, and one past it refused — so a provider that enforces the schema
    /// never denies the model a value the executor takes, and the schema never invites one the
    /// executor refuses. `ms` is the one field whose bound depends on the action: the schema holds
    /// the widest (a press or a drag), and the executor holds `wait` to less, which the
    /// description says.
    #[test]
    fn preview_input_bounds_are_the_executors() {
        use crate::remote_control::session::parse_actions;
        let schema = builtin_tool_schema(
            "preview_input",
            ToolVariant::Standard,
            &PromptProfile::builtin(),
        )
        .unwrap();
        assert_eq!(
            schema["properties"]["actions"]["maxItems"],
            json!(crate::remote_control::session::MAX_ACTIONS)
        );
        let fields = &schema["properties"]["actions"]["items"]["properties"];
        let bound = |field: &str, edge: &str| fields[field][edge].as_f64().unwrap();
        let parses = |action: Value| parse_actions(&json!([action])).is_ok();

        for field in ["x", "y", "toX", "toY"] {
            assert_eq!(fields[field]["type"], "number", "{field}");
            let action = |value: f64| {
                let mut drag = json!({"action": "drag", "x": 1, "y": 1, "toX": 1, "toY": 1});
                drag[field] = json!(value);
                drag
            };
            assert!(parses(action(bound(field, "maximum"))), "{field}");
            assert!(!parses(action(bound(field, "maximum") + 1.0)), "{field}");
            assert!(parses(action(bound(field, "minimum"))), "{field}");
            assert!(!parses(action(bound(field, "minimum") - 1.0)), "{field}");
        }
        assert_eq!(fields["count"]["type"], "integer");
        let click = |count: f64| json!({"action": "click", "x": 1, "y": 1, "count": count});
        assert!(parses(click(bound("count", "maximum"))));
        assert!(!parses(click(bound("count", "maximum") + 1.0)));
        assert!(parses(click(bound("count", "minimum"))));
        assert!(!parses(click(bound("count", "minimum") - 1.0)));

        assert_eq!(fields["ms"]["type"], "integer");
        let (shortest, longest) = (bound("ms", "minimum"), bound("ms", "maximum"));
        for press in [
            json!({"action": "long_press", "x": 1, "y": 1}),
            json!({"action": "drag", "x": 1, "y": 1, "toX": 2, "toY": 2}),
        ] {
            let with = |ms: f64| {
                let mut action = press.clone();
                action["ms"] = json!(ms);
                action
            };
            assert!(parses(with(longest)), "{press}");
            assert!(!parses(with(longest + 1.0)), "{press}");
            assert!(parses(with(shortest)), "{press}");
            assert!(!parses(with(shortest - 1.0)), "{press}");
        }
        let wait = |ms: i64| json!({"action": "wait", "ms": ms});
        let max_wait = crate::remote_control::session::MAX_WAIT_MS;
        assert!(parses(wait(max_wait)));
        assert!(!parses(wait(max_wait + 1)));
        assert!(parses(wait(shortest as i64)));
        let description = schema["description"].as_str().unwrap();
        assert!(
            description.contains(&format!("wait {{ms: 1-{max_wait}}}")),
            "{description}"
        );

        for field in ["dx", "dy"] {
            assert_eq!(fields[field]["type"], "integer", "{field}");
            let scroll = |value: Value| {
                let mut action = json!({"action": "scroll", "x": 1, "y": 1});
                action[field] = value;
                action
            };
            assert!(parses(scroll(json!(bound(field, "maximum")))), "{field}");
            assert!(!parses(scroll(json!(bound(field, "maximum") + 1.0))), "{field}");
            assert!(parses(scroll(json!(bound(field, "minimum")))), "{field}");
            assert!(!parses(scroll(json!(bound(field, "minimum") - 1.0))), "{field}");
            assert!(!parses(scroll(json!(0.5))), "{field}");
        }
        assert_eq!(fields["repeat"]["type"], "integer");
        let key = |repeat: f64| json!({"action": "key", "key": "Enter", "repeat": repeat});
        assert!(parses(key(bound("repeat", "maximum"))));
        assert!(!parses(key(bound("repeat", "maximum") + 1.0)));
        assert!(parses(key(bound("repeat", "minimum"))));
        assert!(!parses(key(bound("repeat", "minimum") - 1.0)));

        // `text`: the flat schema holds the longest any `via` takes — a paste's, 60000 — and the
        // executor holds each `via` to its own at parse: `auto` (the default) to 2000, `paste` to
        // 60000. So the schema never invites a text the executor refuses for its `via`, and the
        // executor's two bounds are the numbers the description states.
        assert_eq!(fields["text"]["maxLength"], json!(60_000));
        assert_eq!(
            fields["text"]["maxLength"].as_u64().unwrap() as usize,
            crate::remote_control::desktop::MAX_TEXT_CHARS
        );
        assert_eq!(crate::remote_control::session::MAX_TEXT_CHARS, 2_000);
        let typed = |length: usize, via: Option<&str>| {
            let mut action = json!({"action": "type", "text": "a".repeat(length)});
            if let Some(via) = via {
                action["via"] = json!(via);
            }
            action
        };
        assert!(parses(typed(2_000, None)));
        assert!(!parses(typed(2_001, None)));
        assert!(parses(typed(2_000, Some("auto"))));
        assert!(!parses(typed(2_001, Some("auto"))));
        assert!(parses(typed(60_000, Some("paste"))));
        assert!(!parses(typed(60_001, Some("paste"))));

        // `modifiers` rides on the four actions that take a pointer (a desktop holds the keys; a
        // phone has none, which the executor says): modifier names joined with `+`, 64 characters
        // at most. The field is refused on every other action as any unknown one is.
        assert_eq!(fields["modifiers"]["type"], "string");
        assert_eq!(fields["modifiers"]["minLength"], json!(1));
        let longest_modifiers = fields["modifiers"]["maxLength"].as_u64().unwrap() as usize;
        assert_eq!(longest_modifiers, 64);
        for held in [
            json!({"action": "click", "x": 1, "y": 1}),
            json!({"action": "long_press", "x": 1, "y": 1}),
            json!({"action": "drag", "x": 1, "y": 1, "toX": 2, "toY": 2}),
            json!({"action": "scroll", "x": 1, "y": 1, "dy": 1}),
        ] {
            let with = |modifiers: &str| {
                let mut action = held.clone();
                action["modifiers"] = json!(modifiers);
                action
            };
            assert!(parses(with("shift")), "{held}");
            assert!(parses(with("ctrl+shift")), "{held}");
            assert!(parses(with("Cmd+Alt")), "{held}");
            assert!(!parses(with("")), "{held}");
            assert!(!parses(with(&"a".repeat(longest_modifiers + 1))), "{held}");
            assert!(!parses(with("shift+a")), "a key is not a modifier: {held}");
        }
        for other in [
            json!({"action": "move", "x": 1, "y": 1}),
            json!({"action": "type", "text": "a"}),
            json!({"action": "key", "key": "Enter"}),
            json!({"action": "wait", "ms": 10}),
        ] {
            let mut action = other.clone();
            action["modifiers"] = json!("shift");
            assert!(!parses(action), "{other}");
        }
        assert!(
            description.contains("click {x, y, button?, count?, modifiers?}")
                && description.contains("scroll {x, y, dx?, dy?, modifiers?}")
                && description.contains("held during the action (desktops only)"),
            "{description}"
        );
    }

    /// The Screen and Android groups are eight independent tools in two categories of their own,
    /// not one multiplexed tool. The two screen-wide ones (`preview_zoom`, `preview_wait`) carry
    /// the `preview_` prefix of the tools they sit next to; the other four of Screen are
    /// `screen_*` and the two of Android are `android_*`. This pins the whole set in the catalog's
    /// order, so a tool cannot be dropped, renamed or moved to another group without a decision;
    /// and what they share: a flat closed object whose `serverId` names a screen the tool can
    /// drive, is never required, and reads as the shared key renders it.
    #[test]
    fn the_catalog_carries_every_screen_and_android_tool() {
        use crate::model::ToolCategory;
        let profile = PromptProfile::builtin();
        // Every tool with the screens its `serverId` says it acts on.
        let groups: [(ToolCategory, &[(&str, &str)]); 2] = [
            (
                ToolCategory::Screen,
                &[
                    ("preview_zoom", "remote-control screen"),
                    ("preview_wait", "remote-control screen"),
                    ("screen_app", "Android device, iPhone or iPad"),
                    ("screen_device", "Android device, iPhone or iPad"),
                    ("screen_clipboard", "iPhone, iPad or desktop"),
                    ("screen_package", "Android device"),
                ],
            ),
            (
                ToolCategory::Android,
                &[
                    ("android_logcat", "Android device"),
                    ("android_shell", "Android device"),
                ],
            ),
        ];
        let mut seen = 0;
        for (category, expected) in groups {
            let names: Vec<String> = tool_catalog()
                .iter()
                .filter(|tool| tool.category == category)
                .map(|tool| tool.name.clone())
                .collect();
            assert_eq!(
                names,
                expected.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                "{category:?}"
            );
            for (name, screen) in expected {
                seen += 1;
                let schema = builtin_tool_schema(name, ToolVariant::Standard, &profile).unwrap();
                assert!(schema.get("oneOf").is_none(), "{name} must be a flat tool");
                assert_eq!(schema["additionalProperties"], json!(false), "{name}");
                let server_id = &schema["properties"]["serverId"];
                assert_eq!(server_id["type"], "string", "{name}");
                assert_eq!(server_id["minLength"], json!(1), "{name}");
                assert_eq!(server_id["maxLength"], json!(256), "{name}");
                assert_eq!(
                    server_id["description"],
                    json!(format!(
                        "Default: the {screen} the preview pane shows, else the only one connected."
                    )),
                    "{name}"
                );
                let required = schema["required"].as_array().expect("required");
                assert!(!required.iter().any(|entry| entry == "serverId"), "{name}");
            }
        }
        assert_eq!(seen, 8);
        // None of them is a page tool: they never take the page tools' `serverId` text.
        assert_ne!(
            profile.text(PromptKey::ToolScreenParamServerId),
            profile.text(PromptKey::ToolPreviewParamServerId)
        );
    }

    /// What the descriptions say the tools do on each kind of screen is what the executors do:
    /// a simulated location is an iPhone's or iPad's alone (an Android device refuses it), an
    /// alert's field is typed into by `prompt_text` on an iPhone or iPad only (an Android dialog's
    /// field is filled like any field), and an iPhone or iPad takes no modifier chords. The text
    /// is the prompt profile's; the behaviour is in `remote_control::{android, ios, session}`.
    #[test]
    fn the_descriptions_say_what_each_kind_of_screen_does_and_refuses() {
        let profile = PromptProfile::builtin();
        let description = |tool: &str| {
            builtin_tool_schema(tool, ToolVariant::Standard, &profile).unwrap()["description"]
                .as_str()
                .unwrap()
                .to_owned()
        };

        let device = description("screen_device");
        assert!(
            device.contains(
                "location with latitude and longitude and clear_location (iPhone/iPad, iOS 16.4+)"
            ),
            "{device}"
        );
        // …and the executor of an Android device does refuse both.
        for (action, input) in [
            (
                "location",
                json!({"action": "location", "latitude": 1.0, "longitude": 2.0}),
            ),
            ("clear_location", json!({"action": "clear_location"})),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let files = crate::tool_executor::screen_tools::CallFiles::local(
                workspace.path(),
                crate::path_guard::ExecutionScope::workspace_only(workspace.path()),
            );
            let refusal = crate::remote_control::android::AndroidCall::parse(
                "screen_device",
                input.as_object().unwrap(),
                &files,
            )
            .unwrap_err();
            assert!(refusal.contains("Android"), "{action}: {refusal}");
        }

        let dialog = description("preview_dialog");
        assert!(
            dialog.contains("prompt_text alone only types into the alert's field (iPhone/iPad"),
            "{dialog}"
        );
        let input = json!({"prompt_text": "x", "button": "OK"});
        let refusal = crate::remote_control::android::AndroidCall::dialog(input.as_object().unwrap())
            .unwrap_err();
        assert!(refusal.contains("prompt_text is for an iPhone or iPad"), "{refusal}");

        let input = description("preview_input");
        assert!(
            input.contains("iPhone/iPad refuse ctrl, cmd and mod chords"),
            "{input}"
        );
    }

    /// What each of the eight tools asks the model for, as the spec states it: the required
    /// parameters, and for every parameter its type and bounds (the description is the prompt
    /// profile's, not part of the shape). The executors parse the same numbers, which is why a
    /// change here is a change of `parse` there.
    #[test]
    fn screen_tool_schemas_state_the_specs_bounds() {
        let profile = PromptProfile::builtin();
        let schema = |name: &str| builtin_tool_schema(name, ToolVariant::Standard, &profile).unwrap();
        let shape = |tool: &str, parameter: &str| {
            let mut property = schema(tool)["properties"][parameter].clone();
            assert!(property.is_object(), "{tool}.{parameter} is not declared");
            property
                .as_object_mut()
                .unwrap()
                .remove("description")
                .unwrap_or_else(|| panic!("{tool}.{parameter} carries no description"));
            property
        };
        let text = |max: u64| json!({"type": "string", "minLength": 1, "maxLength": max});
        let free_text = |max: u64| json!({"type": "string", "maxLength": max});
        let choice = |values: Value| json!({"type": "string", "enum": values});
        let number = |min: i64, max: i64| json!({"type": "number", "minimum": min, "maximum": max});
        let integer = |min: i64, max: i64, default: i64| {
            json!({"type": "integer", "minimum": min, "maximum": max, "default": default})
        };
        let flag = json!({"type": "boolean"});

        let required = |tool: &str| -> Vec<String> {
            schema(tool)["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry.as_str().unwrap().to_owned())
                .collect()
        };
        for (tool, expected) in [
            ("preview_zoom", &["x", "y", "width", "height"][..]),
            ("preview_wait", &[][..]),
            ("screen_app", &[][..]),
            ("screen_device", &[][..]),
            ("screen_clipboard", &["action"][..]),
            ("screen_package", &["action"][..]),
            ("android_logcat", &[][..]),
            ("android_shell", &["command"][..]),
        ] {
            assert_eq!(required(tool), expected, "{tool}");
        }
        // The properties of each tool, exactly: nothing else is declared.
        for (tool, expected) in [
            ("preview_zoom", &["serverId", "x", "y", "width", "height"][..]),
            (
                "preview_wait",
                &["serverId", "until", "timeout", "x", "y", "width", "height"][..],
            ),
            (
                "screen_app",
                &[
                    "serverId",
                    "action",
                    "app",
                    "url",
                    "filter",
                    "limit",
                    "scope",
                    "activity",
                    "restart",
                    "arguments",
                    "environment",
                ][..],
            ),
            (
                "screen_device",
                &["serverId", "action", "value", "sections", "latitude", "longitude"][..],
            ),
            ("screen_clipboard", &["serverId", "action", "text"][..]),
            (
                "screen_package",
                &[
                    "serverId",
                    "action",
                    "path",
                    "app",
                    "permission",
                    "keep_data",
                    "replace_existing",
                    "downgrade",
                    "grant_permissions",
                ][..],
            ),
            (
                "android_logcat",
                &["serverId", "lines", "level", "tag", "package", "buffer", "contains"][..],
            ),
            ("android_shell", &["serverId", "command", "timeout"][..]),
        ] {
            let declared = properties(&schema(tool));
            let expected: BTreeSet<String> = expected.iter().map(|name| (*name).to_owned()).collect();
            assert_eq!(declared, expected, "{tool}");
        }

        // preview_zoom
        assert_eq!(shape("preview_zoom", "x"), number(0, 100_000));
        assert_eq!(shape("preview_zoom", "y"), number(0, 100_000));
        assert_eq!(shape("preview_zoom", "width"), number(1, 100_000));
        assert_eq!(shape("preview_zoom", "height"), number(1, 100_000));
        // preview_wait
        assert_eq!(
            shape("preview_wait", "until"),
            json!({"type": "string", "enum": ["change", "still"], "default": "change"})
        );
        assert_eq!(shape("preview_wait", "timeout"), integer(100, 20000, 5000));
        assert_eq!(shape("preview_wait", "x"), number(0, 100_000));
        assert_eq!(shape("preview_wait", "y"), number(0, 100_000));
        assert_eq!(shape("preview_wait", "width"), number(1, 100_000));
        assert_eq!(shape("preview_wait", "height"), number(1, 100_000));
        // screen_app
        assert_eq!(
            shape("screen_app", "action"),
            json!({
                "type": "string",
                "enum": ["list", "foreground", "info", "launch", "activate", "stop", "open_url"],
                "default": "list"
            })
        );
        assert_eq!(shape("screen_app", "app"), text(255));
        assert_eq!(shape("screen_app", "url"), text(2048));
        assert_eq!(shape("screen_app", "filter"), text(200));
        assert_eq!(shape("screen_app", "limit"), integer(1, 1000, 200));
        assert_eq!(
            shape("screen_app", "scope"),
            json!({
                "type": "string",
                "enum": ["launchable", "third_party", "all"],
                "default": "launchable"
            })
        );
        assert_eq!(shape("screen_app", "activity"), text(255));
        assert_eq!(shape("screen_app", "restart"), flag);
        assert_eq!(
            shape("screen_app", "arguments"),
            json!({"type": "array", "maxItems": 32, "items": {"type": "string", "maxLength": 1024}})
        );
        assert_eq!(
            shape("screen_app", "environment"),
            json!({
                "type": "object",
                "maxProperties": 32,
                "additionalProperties": {"type": "string", "maxLength": 1024}
            })
        );
        // screen_device
        assert_eq!(
            shape("screen_device", "action"),
            json!({
                "type": "string",
                "enum": [
                    "info",
                    "lock",
                    "unlock",
                    "wake",
                    "rotate",
                    "appearance",
                    "animations",
                    "location",
                    "clear_location",
                    "notifications",
                    "quick_settings",
                    "collapse",
                    "stay_awake",
                    "dismiss_keyboard"
                ],
                "default": "info"
            })
        );
        assert_eq!(shape("screen_device", "value"), text(32));
        assert_eq!(
            shape("screen_device", "sections"),
            json!({
                "type": "array",
                "minItems": 1,
                "maxItems": 4,
                "uniqueItems": true,
                "items": {"type": "string", "enum": ["device", "display", "power", "notifications"]}
            })
        );
        assert_eq!(shape("screen_device", "latitude"), number(-90, 90));
        assert_eq!(shape("screen_device", "longitude"), number(-180, 180));
        // screen_clipboard: no minLength on the text, writing nothing clears the clipboard.
        assert_eq!(shape("screen_clipboard", "action"), choice(json!(["read", "write"])));
        assert_eq!(shape("screen_clipboard", "text"), free_text(100_000));
        // screen_package: the Android package tool's schema, the package named `app`.
        assert_eq!(
            shape("screen_package", "action"),
            choice(json!(["install", "uninstall", "clear_data", "grant", "revoke"]))
        );
        assert_eq!(shape("screen_package", "path"), text(4096));
        assert_eq!(shape("screen_package", "app"), text(255));
        assert_eq!(shape("screen_package", "permission"), text(255));
        assert_eq!(shape("screen_package", "keep_data"), flag);
        assert_eq!(
            shape("screen_package", "replace_existing"),
            json!({"type": "boolean", "default": true})
        );
        assert_eq!(shape("screen_package", "downgrade"), flag);
        assert_eq!(shape("screen_package", "grant_permissions"), flag);
        // android_logcat
        assert_eq!(shape("android_logcat", "lines"), integer(1, 2000, 200));
        assert_eq!(
            shape("android_logcat", "level"),
            choice(json!(["V", "D", "I", "W", "E", "F"]))
        );
        assert_eq!(shape("android_logcat", "tag"), text(128));
        assert_eq!(shape("android_logcat", "package"), text(255));
        assert_eq!(
            shape("android_logcat", "buffer"),
            choice(json!(["main", "system", "crash", "events", "all"]))
        );
        assert_eq!(shape("android_logcat", "contains"), text(200));
        // android_shell
        assert_eq!(shape("android_shell", "command"), text(8192));
        assert_eq!(shape("android_shell", "timeout"), integer(1000, 20000, 10000));

        // preview_dialog answers a phone's alert by the label of a button.
        assert_eq!(shape("preview_dialog", "button"), text(256));
        // preview_input's `type` carries `via` and `chord`, and a text up to the paste ceiling.
        let schema = schema("preview_input");
        let fields = &schema["properties"]["actions"]["items"]["properties"];
        assert_eq!(fields["via"], json!({"type": "string", "enum": ["auto", "paste"]}));
        assert_eq!(
            fields["chord"],
            json!({"type": "string", "minLength": 1, "maxLength": 64})
        );
        assert_eq!(fields["text"], json!({"type": "string", "maxLength": 60_000}));
    }

    /// `screen_package install` reads an `.apk` out of a workspace by `path`, so it is the one
    /// tool of the two groups that names a workspace once there is a choice: the others find
    /// their screen by `serverId`, and no workspace changes which device they act on.
    #[test]
    fn only_screen_package_among_the_screen_tools_names_a_workspace() {
        use crate::model::ToolCategory;
        let workspaces = mixed_workspaces();
        let schema = schema_with_workspaces("screen_package", &workspaces);
        assert_eq!(schema["properties"]["workspace"]["enum"], json!([1, 2]));
        assert_eq!(schema["properties"]["workspace"]["default"], json!(1));
        assert_eq!(schema["required"], json!(["action"]));
        // One workspace is not a choice.
        let single = WorkspaceSet::local_root("C:/work/app");
        assert!(schema_with_workspaces("screen_package", &single)["properties"]["workspace"].is_null());
        for tool in tool_catalog() {
            if matches!(tool.category, ToolCategory::Android | ToolCategory::Screen)
                && tool.name != "screen_package"
            {
                assert!(
                    schema_with_workspaces(&tool.name, &workspaces)["properties"]["workspace"].is_null(),
                    "{} should not address a workspace",
                    tool.name
                );
                assert!(!takes_a_workspace(&tool.name), "{}", tool.name);
            }
        }
        assert!(takes_a_workspace("screen_package"));
        assert!(!addresses_a_preview_server("screen_package"));
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
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin()).unwrap();
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

    /// The launch.json template is owned by `preview_launch_config`.
    /// `preview_start`'s description quotes it, and the built-in profile must
    /// quote the same bytes — a paraphrase there would teach the model a file
    /// format the parser rejects. The prose around it is the description's own.
    #[test]
    fn preview_start_descriptions_quote_the_launch_json_format_verbatim() {
        let profile = PromptProfile::builtin();
        let text = profile.text(PromptKey::ToolPreviewStartDescription);
        assert!(
            text.contains(&format!(
                ":\n{}\n",
                crate::preview_launch_config::LAUNCH_JSON_FORMAT
            )),
            "{text}"
        );
    }

    /// Claude Code's `SendMessage` shape: who, what, and an optional preview.
    /// `to` carries the pattern `agent_spawn.name` does, because it is that name.
    #[test]
    fn send_message_takes_a_child_name_a_message_and_an_optional_summary() {
        let profile = PromptProfile::builtin();
        let schema = builtin_tool_schema("send_message", ToolVariant::Standard, &profile).unwrap();
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "description": profile.text(PromptKey::ToolSendMessageDescription),
                "properties": {
                    "to": {
                        "type": "string",
                        "pattern": "^[a-z][a-z0-9_-]{0,31}$",
                        "description": profile.text(PromptKey::ToolSendMessageParamTo)
                    },
                    "message": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 32768,
                        "description": profile.text(PromptKey::ToolSendMessageParamMessage)
                    },
                    "summary": {
                        "type": "string",
                        "maxLength": 200,
                        "description": profile.text(PromptKey::ToolSendMessageParamSummary)
                    }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            })
        );
        // The name pattern is the one `agent_spawn` enforces on `name`.
        let spawn = builtin_tool_schema("agent_spawn", ToolVariant::Standard, &profile).unwrap();
        assert_eq!(
            schema["properties"]["to"]["pattern"],
            spawn["properties"]["name"]["pattern"]
        );
        // And the host's own bound on the message is what `agent_spawn.prompt` has.
        assert_eq!(
            schema["properties"]["message"]["maxLength"],
            spawn["properties"]["prompt"]["maxLength"]
        );
    }

    #[test]
    fn required_entries_reference_declared_properties() {
        for tool in tool_catalog() {
            let schema =
                builtin_tool_schema(&tool.name, ToolVariant::Standard, &PromptProfile::builtin()).unwrap();
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

    /// The baseline of the built-in profile, which every user file falls
    /// back to.
    fn baselines() -> [(&'static str, String); 1] {
        [("builtin-tool-schemas.json", baseline_document(&PromptProfile::builtin()))]
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
        let schema = builtin_tool_schema("task_wait", ToolVariant::Standard, &PromptProfile::builtin()).unwrap();
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
