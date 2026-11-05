//! In-app approval prompts for model tool calls.
//!
//! The native `MessageDialog` this replaces was modal to the whole OS and
//! carried no state beyond the single answer. Approval is now a card above the
//! composer, driven by two shapes of prompt that share one registry:
//!
//! * **Model prompts.** A worker thread blocks on a one-shot channel while the
//!   card travels to the renderer, which answers through `resolve_tool_prompt`
//!   and wakes the worker. The waiter may be the top-level run's own thread or
//!   a detached task worker; [`PromptOwner`] is what tells them apart.
//! * **Manual prompts.** The tool editor's own calls have no run to stream
//!   through, so `request_tool_approval` returns the prompt descriptor instead
//!   of blocking, and the nonce is minted only when the answer arrives. The
//!   classified request stays host-side across those two calls, so the renderer
//!   cannot widen between asking and answering.
//!
//! # A card is not owned by a turn
//!
//! Tasks outlive the turn that created them (S11), so a workflow step or a
//! subagent can request approval after the top-level round ends. Cards travel
//! the app push channel when no run stream is open, and the renderer draws them
//! exactly as it draws cards from a run stream.
//!
//! A run end retracts only the cards that the run itself raised
//! ([`ToolPromptRegistry::cancel_run_prompts`]); a task's card survives the turn
//! exactly as the task does.
//!
//! `Ok(false)` means the user said no. Every other ending — stopped, timed out,
//! or no channel to ask through — is an `Err` that identifies the actual
//! outcome, so an unseen question is never attributed to the user as a refusal.
//!
//! Two invariants keep the "always allow" affordance from becoming a hole:
//!
//! * It is offered per (conversation, tool name) only, so allowing `read`
//!   never allows `write`, and it never crosses into another conversation —
//!   not even one branched, forked or continued from it. It is saved with the
//!   conversation, so it outlives a restart and dies with the conversation.
//!   A screen tool is kept per screen, and one that both looks and acts per
//!   action too ([`allowance_key`]): allowing a phone's app list never allows
//!   launching apps on it, nor anything on another screen. A screen is named
//!   by the handle of its registry row, which a per-process counter mints, so
//!   such an allowance is kept in memory only ([`allowance_names_a_screen`]):
//!   a saved one would answer a different screen after a restart.
//! * A decision the security classifier marked `mandatory_prompt` — VM guest
//!   authorization, global memory mutation, an MCP server that declares it
//!   needs interaction — can neither offer nor consume an allowance. Those
//!   cross a boundary the conversation's security level does not describe, so
//!   every one of them asks again.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, RecvTimeoutError, SyncSender},
        Mutex,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::model::{JsonObject, ResolvedLanguage, ToolDescriptor, ToolExecutionRequest};
use crate::security::RiskLevel;
use crate::ui_text;

/// How long a model prompt may stay unanswered before it is denied. The native
/// dialog blocked forever; a renderer that navigated away or crashed would
/// otherwise pin a worker thread for the process lifetime.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Cancellation is polled rather than signalled, so a stopped run releases its
/// worker within this window instead of at the prompt timeout.
const CANCELLATION_POLL: Duration = Duration::from_millis(150);
/// Manual prompts have no blocked thread to notice they were abandoned, so they
/// are evicted on the next registry write instead.
const MANUAL_PROMPT_TTL: Duration = Duration::from_secs(30 * 60);
/// Maximum number of unanswered cards per conversation. A saturated conversation
/// must not block prompts in another conversation.
const MAX_PENDING_PROMPTS_PER_CONVERSATION: usize = 64;
/// The card is one or two lines above the composer, not a document viewer.
const MAX_SUMMARY_CHARS: usize = 240;
/// Feedback on a plan card is a paragraph of guidance, not a document. It is
/// quoted into a tool result, so it is bounded like every other model input.
const MAX_FEEDBACK_CHARS: usize = 4_000;

/// True for the tools whose whole argument is an arbitrary command line. The
/// user's instruction is explicit: a shell call is never blanket-allowed,
/// because "this class of call" is not a meaningful category when the class is
/// "run anything".
pub fn is_shell_tool(tool_name: &str) -> bool {
    crate::shell_backend::ShellBackend::of_tool(tool_name).is_some()
}

/// The tool that drives a remote-control screen. It is deliberately not a
/// [`crate::browser::PreviewTool`]: that enum is the page (CDP) tools, whose
/// calls pass the signed-in-page takeover gate, and a screen has no page to take
/// over. It is as unbounded as `preview_click` all the same, so no allowance
/// answers for it either.
const REMOTE_INPUT_TOOL: &str = "preview_input";

/// The screen tools that are as open-ended on a real device as `preview_input`: a line of the
/// device's shell. (An answer to whatever alert a phone shows — a permission, a payment, a
/// deletion — is `preview_dialog`'s, a page tool and so never allowed; text pasted into whatever
/// field has focus is `preview_input`'s.) The other screen tools may carry an allowance: each does
/// one thing a card can say in full, and the allowance holds for one screen
/// ([`allowance_key`]).
const OPEN_ENDED_SCREEN_TOOLS: [&str; 1] = ["android_shell"];

/// Tools that may never carry a standing allowance, whatever the level says.
///
/// Three classes, three different reasons:
///
/// * shell — see [`is_shell_tool`]: the class is "run anything".
/// * `preview_input` — see [`REMOTE_INPUT_TOOL`]: a tap or a keystroke on a
///   real phone or desktop is as open-ended as a shell line; and so are the
///   screen tools of [`OPEN_ENDED_SCREEN_TOOLS`].
/// * the preview page tools — they are the tools with an **unconditional human
///   gate** underneath the ordinary classifier. `browser_credential_takeover_denial`
///   asks the user directly before the Agent may act on a tab they signed in
///   to, and its comment says why a hook may not answer it: "this protects the
///   user's own signed-in session, and the user asked to be the one who
///   answers." An allowance recorded from any earlier preview card would
///   have answered that gate too, silently, because the registry keyed
///   allowances by tool name alone. The takeover prompt is raised through the
///   same `ask`, so the only place that distinction can live is here.
pub fn never_blanket_allowed(tool_name: &str) -> bool {
    is_shell_tool(tool_name)
        || tool_name == REMOTE_INPUT_TOOL
        || OPEN_ENDED_SCREEN_TOOLS.contains(&tool_name)
        || crate::browser::PreviewTool::from_tool_name(tool_name).is_some()
}

/// The screen tools whose standing allowance is kept per action, with each one's actions and the
/// one an absent `action` means: one tool both looks and acts — `screen_app` lists apps and
/// launches them, `screen_clipboard` reads (a password too) and writes — so an "always allow"
/// said to one action must not answer another without a card.
const PER_ACTION_ALLOWANCE_TOOLS: [(&str, &[&str], Option<&str>); 4] = [
    (
        "screen_app",
        &["list", "foreground", "info", "launch", "activate", "stop", "open_url"],
        Some("list"),
    ),
    (
        "screen_device",
        &[
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
            "dismiss_keyboard",
        ],
        Some("info"),
    ),
    ("screen_clipboard", &["read", "write"], None),
    (
        "screen_package",
        &["install", "uninstall", "clear_data", "grant", "revoke"],
        None,
    ),
];

/// The name a call's "always allow" answer is recorded and looked up under (and, for every key
/// that names no screen, saved under).
///
/// For a screen tool ([`crate::tool_executor::screen_tools::SCREEN_TOOLS`]) it names the screen
/// the call was judged for — `screen`, the handle of its registry row — so an allowance given for
/// one phone never answers for another, nor for a desktop, nor for the same entry started again
/// (a new row): `screen_app#launch@<handle>`, `preview_zoom@<handle>`. A handle is minted by a
/// per-process counter, so such an allowance lives in memory only
/// ([`allowance_names_a_screen`]) and ends with the process. The tools of
/// [`PER_ACTION_ALLOWANCE_TOOLS`] also name the call's action (an unknown one as `other`, kept
/// apart from every real one). A screen tool call judged for no screen gets its bare name, which
/// answers nothing and is recorded for nothing ([`allowance_is_keyed`]): the manual path, which
/// judges no target, keeps no allowance for these tools.
///
/// Every other tool is keyed by its name.
pub fn allowance_key(tool_name: &str, input: &JsonObject, screen: Option<&str>) -> String {
    if crate::tool_executor::screen_tools::screen_tool_platforms(tool_name).is_none() {
        return tool_name.to_owned();
    }
    let Some(screen) = screen else {
        return tool_name.to_owned();
    };
    match PER_ACTION_ALLOWANCE_TOOLS
        .iter()
        .find(|(name, ..)| *name == tool_name)
    {
        Some((_, actions, default)) => {
            let action = match input.get("action") {
                None | Some(Value::Null) => default.unwrap_or("other"),
                Some(Value::String(action)) => actions
                    .iter()
                    .copied()
                    .find(|known| *known == action)
                    .unwrap_or("other"),
                Some(_) => "other",
            };
            format!("{tool_name}#{action}@{screen}")
        }
        None => format!("{tool_name}@{screen}"),
    }
}

/// Whether an allowance may be recorded and looked up under `key`: not under a screen tool's bare
/// name, which [`allowance_key`] gives a call judged for no screen.
pub fn allowance_is_keyed(key: &str) -> bool {
    crate::tool_executor::screen_tools::screen_tool_platforms(key).is_none()
}

/// Whether `key` (an [`allowance_key`]) names a screen: `screen_app#launch@3`, `preview_zoom@3`.
///
/// The `@<handle>` is the registry row's handle, minted by a counter that starts again at every
/// start of the app, so the same key can mean another screen after a restart. An allowance under
/// such a key is therefore kept in memory only: [`ToolPromptRegistry::remember_allowance`] does not
/// save it, and reading the saved ones back skips any row that names a screen (an earlier build,
/// or a hand-edited store, may have written one).
pub fn allowance_names_a_screen(key: &str) -> bool {
    key.contains('@')
}

/// The tool's name as an approval card shows it, in the card's language.
///
/// A descriptor's label follows the prompt profile, which is the model's
/// language and can differ from the app's, so built-in tools are named from
/// the catalog in `language` instead; a tool outside it (an MCP tool) keeps its
/// own label. Writing global memory says that the memory is shared by every
/// project. The descriptor's [`ToolDescriptor::approval_note`] goes on the
/// line below.
pub fn card_label(descriptor: &ToolDescriptor, language: ResolvedLanguage) -> String {
    let name = match descriptor.name.as_str() {
        "create_global_memory" => ui_text::pick_for(
            language,
            "创建全局记忆（跨项目共享）",
            "Create global memory (shared across projects)",
        )
        .to_owned(),
        "edit_global_memory" => ui_text::pick_for(
            language,
            "编辑全局记忆（跨项目共享）",
            "Edit global memory (shared across projects)",
        )
        .to_owned(),
        name => crate::catalog::tool_label(name, language)
            .unwrap_or_else(|| descriptor.label.clone()),
    };
    match &descriptor.approval_note {
        Some(note) => format!("{name}\n{note}"),
        None => name,
    }
}

/// What the model wants to do, built from the call's own arguments, worded in
/// the card's language.
///
/// A command and a list of addresses are shown whole, line breaks kept: what
/// will run, and every page that will be read, is exactly what is being
/// approved, and a cut-off tail is where a surprise would hide. So are the
/// link a device's `open_url` opens and the APK path `install` streams. Every
/// other call is one line, cut to fit beside the card's reason.
///
/// `input` must already be the redacted projection (`public_tool_input`), so
/// this never has to know which keys are secret — it only has to be readable.
/// Unknown tools fall back to their most descriptive scalar argument rather
/// than dumping JSON, because a card that shows a wall of braces is a card
/// nobody reads before clicking.
pub fn summarize_tool_input(
    tool_name: &str,
    input: &JsonObject,
    language: ResolvedLanguage,
) -> String {
    summarize_tool_input_on(tool_name, input, language, None)
}

/// [`summarize_tool_input`] for a call judged to act on a remote-control screen: `screen` is how
/// the card names that screen — its server id and the device's own name (`pixel (Pixel 8)`) —
/// and leads the summary, so that of two phones the card says which one it is about. `None`
/// leads with the call's `serverId` when it names one, as [`summarize_tool_input`] does.
pub fn summarize_tool_input_on(
    tool_name: &str,
    input: &JsonObject,
    language: ResolvedLanguage,
    screen: Option<&str>,
) -> String {
    let screen_prefix = |input: &JsonObject| screen_prefix(input, screen);
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::trim);
    // Every shell backend's tool (bash, zsh, sh, PowerShell), not a list of
    // names: a backend added later is approved on the same terms. The
    // background marker rides in front of the command so the user approving
    // the card knows this call returns immediately and the command then runs
    // detached from the round.
    if crate::shell_backend::ShellBackend::of_tool(tool_name).is_some() {
        if let Some(command) = text("command").filter(|command| !command.is_empty()) {
            let command = if input.get("run_in_background") == Some(&Value::Bool(true)) {
                let marker = ui_text::pick_for(language, "[后台]", "[Background]");
                format!("{marker} {command}")
            } else {
                command.to_owned()
            };
            return escape_display_text(&command);
        }
    }
    // A device's shell line is approved as a command is: whole, line breaks kept.
    if tool_name == "android_shell" {
        if let Some(command) = text("command").filter(|command| !command.is_empty()) {
            return escape_display_text(&format!("{}{command}", screen_prefix(input)));
        }
    }
    // A link can lead anywhere on the device, and an APK's file name is the end of its path, so
    // the URL `open_url` opens and the APK `install` streams are shown whole, as a shell line is,
    // after the rest of the call, which stays bounded.
    if let Some((head, whole)) = screen_call_with_a_whole_target(tool_name, input, screen) {
        return format!(
            "{} {}",
            truncate_for_display(&escape_display_text(&head)),
            escape_display_text(whole)
        );
    }
    if let Some(summary) = summarize_screen_call(tool_name, input, language, screen) {
        return truncate_for_display(&escape_display_text(&summary));
    }
    if tool_name == "web_fetch" {
        let urls = input
            .get("urls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .collect::<Vec<_>>();
        if !urls.is_empty() {
            return escape_display_text(&urls.join("\n"));
        }
    }
    let summary = match tool_name {
        "read" | "write" | "edit" | "delete" | "list" => text("path").map(str::to_owned),
        "search" | "grep" => match (text("pattern"), text("path")) {
            (Some(pattern), Some(path)) => Some(format!("{pattern} · {path}")),
            (Some(pattern), None) => Some(pattern.to_owned()),
            _ => None,
        },
        // Use the workflow run name rather than its script body.
        "workflow" => text("name").map(str::to_owned),
        _ => None,
    };
    let summary = summary
        .filter(|value| !value.is_empty())
        .or_else(|| most_descriptive_scalar(input))
        .unwrap_or_default();
    truncate_for_display(&escape_display_text(&summary))
}

/// `<screen>: ` for a call judged to act on a screen (see [`summarize_tool_input_on`]); else
/// `<serverId>: ` when the call names one, else nothing — the card then means the screen the
/// preview shows, or the only one of its kind.
fn screen_prefix(input: &JsonObject, screen: Option<&str>) -> String {
    if let Some(screen) = screen.map(str::trim).filter(|screen| !screen.is_empty()) {
        return format!("{screen}: ");
    }
    input
        .get("serverId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| format!("{id}: "))
        .unwrap_or_default()
}

/// The screen calls whose card shows one argument whole: `screen_app` `open_url`'s URL and
/// `screen_package` `install`'s path. `Some((head, whole))`, the head being
/// `[<screen>: ]<action>[ <package or bundle id>]` — the app that opens the link comes before
/// it, so a long link cannot push it off the card.
fn screen_call_with_a_whole_target<'a>(
    tool_name: &str,
    input: &'a JsonObject,
    screen: Option<&str>,
) -> Option<(String, &'a str)> {
    let screen_prefix = |input: &JsonObject| screen_prefix(input, screen);
    let text = move |key: &str| -> Option<&'a str> {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let (action, app, whole) = match (tool_name, text("action")?) {
        ("screen_app", "open_url") => ("open_url", text("app"), text("url")?),
        ("screen_package", "install") => ("install", None, text("path")?),
        _ => return None,
    };
    let head = match app {
        Some(app) => format!("{}{action} {app}", screen_prefix(input)),
        None => format!("{}{action}", screen_prefix(input)),
    };
    Some((head, whole))
}

/// At most `max` characters of `value`, with `…` when it was longer.
fn first_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let kept: String = value.chars().take(max).collect();
    format!("{kept}…")
}

/// What a screen call will do, as `[<serverId>: ]<action> <what it acts on>`: the app, the URL,
/// the APK's path, the button, the region — for `preview_input` each action in turn, led by the
/// first `type` with via `"paste"`: its text (the first 100 characters), the shortcut that pastes
/// it, and that the clipboard keeps it ([`summarize_input_actions`]); for a `screen_app` launch
/// whether it restarts the app and how many arguments and environment variables it carries.
/// `None` for any other tool. (`android_shell` is shown
/// whole, as a command is, and so are an `open_url` link and an `install` path:
/// [`screen_call_with_a_whole_target`].)
fn summarize_screen_call(
    tool_name: &str,
    input: &JsonObject,
    language: ResolvedLanguage,
    screen: Option<&str>,
) -> Option<String> {
    let screen_prefix = |input: &JsonObject| screen_prefix(input, screen);
    let text = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let number = |key: &str| input.get(key).filter(|value| value.is_number());
    let action = text("action");
    let joined = |parts: &[Option<&str>]| -> String {
        parts
            .iter()
            .flatten()
            .copied()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let body = match tool_name {
        "screen_app" => match action.unwrap_or("list") {
            "list" => joined(&[Some("list"), text("filter").or(text("scope"))]),
            "open_url" => joined(&[action, text("app"), text("url")]),
            action => {
                let component = match (text("app"), text("activity")) {
                    (Some(app), Some(activity)) => Some(format!("{app}/{activity}")),
                    (app, _) => app.map(str::to_owned),
                };
                // What changes how the app starts is on the card too: a restart ends the
                // running app (and what it had unsaved), and launch arguments and environment
                // variables are the app's own inputs.
                let restart = (input.get("restart").and_then(Value::as_bool) == Some(true))
                    .then_some("(restart)");
                let counted = |key: &str, one: &str, many: &str| -> Option<String> {
                    let count = match input.get(key)? {
                        Value::Array(items) => items.len(),
                        Value::Object(items) => items.len(),
                        _ => return None,
                    };
                    match count {
                        0 => None,
                        1 => Some(format!("+1 {one}")),
                        count => Some(format!("+{count} {many}")),
                    }
                };
                let arguments = counted("arguments", "argument", "arguments");
                let environment =
                    counted("environment", "environment variable", "environment variables");
                joined(&[
                    Some(action),
                    component.as_deref(),
                    restart,
                    arguments.as_deref(),
                    environment.as_deref(),
                ])
            }
        },
        "screen_package" => match action {
            Some("install") => joined(&[action, text("path")]),
            _ => joined(&[action, text("app"), text("permission")]),
        },
        "android_logcat" => joined(&[
            Some("logcat"),
            text("package").or(text("tag")),
            text("buffer"),
            text("contains"),
        ]),
        "screen_device" => match action.unwrap_or("info") {
            "info" => {
                let sections = input
                    .get("sections")
                    .and_then(Value::as_array)
                    .map(|sections| {
                        sections
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|sections| !sections.is_empty());
                joined(&[Some("info"), sections.as_deref()])
            }
            // A device with a passcode stays locked until its owner unlocks it: the model
            // cannot, so the card says so before it is allowed.
            "lock" => format!(
                "lock {}",
                ui_text::pick_for(
                    language,
                    "（设备有锁屏密码时，之后需要你来解锁）",
                    "(if the device has a passcode, you will have to unlock it afterwards)"
                )
            ),
            action => {
                let place = match (number("latitude"), number("longitude")) {
                    (Some(latitude), Some(longitude)) => Some(format!("{latitude}, {longitude}")),
                    _ => None,
                };
                joined(&[Some(action), text("value"), place.as_deref()])
            }
        },
        "screen_clipboard" => match action {
            Some("write") => joined(&[
                action,
                input
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| first_chars(text, 200))
                    .as_deref(),
            ]),
            _ => joined(&[action]),
        },
        "preview_input" => summarize_input_actions(input, language)?,
        // The answer a dialog gets: the button pressed, or accept / dismiss — and the text typed
        // into its field.
        "preview_dialog" => {
            let answer = match (text("button"), input.get("accept").and_then(Value::as_bool)) {
                (Some(button), _) => format!("press \"{button}\""),
                (None, Some(false)) => "dismiss".to_owned(),
                (None, Some(true)) => "accept".to_owned(),
                (None, None) if text("prompt_text").is_some() => "type".to_owned(),
                (None, None) => "accept".to_owned(),
            };
            match input.get("prompt_text").and_then(Value::as_str) {
                Some(prompt) if !prompt.is_empty() => {
                    format!("{answer} \"{}\"", first_chars(prompt, 200))
                }
                _ => answer,
            }
        }
        "preview_zoom" => match (number("x"), number("y"), number("width"), number("height")) {
            (Some(x), Some(y), Some(width), Some(height)) => {
                format!("zoom {x},{y} {width}x{height}")
            }
            _ => "zoom".to_owned(),
        },
        "preview_wait" => joined(&[Some("wait until"), Some(text("until").unwrap_or("change"))]),
        _ => return None,
    };
    Some(format!("{}{}", screen_prefix(input), body.trim_end()))
}

/// How many characters of a pasted text the card's lead shows.
const PASTE_SHOWN_CHARS: usize = 100;

/// `preview_input`'s actions, one after another: `click 288,640 · type "hello" · key Enter`.
///
/// A batch with a `type` through the clipboard (`via` `"paste"`) is led by that paste's notice
/// and text: `paste via the clipboard (cmd+v; the clipboard keeps the text) "hello" — click 1,2 ·
/// paste`. It lands in whatever field has focus, and the text stays on the clipboard where any app
/// can read it, so the card says both first — a card is cut to fit, and the cut takes the end of
/// the list, never the note. In the list the first paste then shows as `paste`; a later one shows
/// its own text briefly (`paste "hello"`), since only the first has been shown whole. `None` when
/// there are no actions to show.
fn summarize_input_actions(input: &JsonObject, language: ResolvedLanguage) -> Option<String> {
    let actions = input
        .get("actions")
        .and_then(Value::as_array)?
        .iter()
        .take(20)
        .filter_map(Value::as_object)
        .collect::<Vec<_>>();
    let is_paste = |action: &&JsonObject| {
        action.get("action").and_then(Value::as_str).map(str::trim) == Some("type")
            && action.get("via").and_then(Value::as_str).map(str::trim) == Some("paste")
    };
    let pasted = |action: &JsonObject, max: usize| {
        action
            .get("text")
            .and_then(Value::as_str)
            .map(|text| first_chars(text, max))
            .unwrap_or_default()
    };
    let first_paste = actions.iter().position(|action| is_paste(action));
    let shown = actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let text = |key: &str| action.get(key).and_then(Value::as_str).map(str::trim);
            let at = |x: &str, y: &str| match (action.get(x), action.get(y)) {
                (Some(x), Some(y)) if x.is_number() && y.is_number() => format!(" {x},{y}"),
                _ => String::new(),
            };
            let kind = text("action").unwrap_or("?");
            match kind {
                "type" if is_paste(action) && Some(index) == first_paste => "paste".to_owned(),
                "type" if is_paste(action) => format!("paste \"{}\"", pasted(action, 80)),
                "type" => format!(
                    "type \"{}\"",
                    first_chars(action.get("text").and_then(Value::as_str).unwrap_or(""), 80)
                ),
                "key" => format!("key {}", text("key").unwrap_or("")),
                "drag" => format!("drag{} →{}", at("x", "y"), at("toX", "toY")),
                "wait" => match action.get("ms") {
                    Some(ms) if ms.is_number() => format!("wait {ms} ms"),
                    _ => "wait".to_owned(),
                },
                other => format!("{other}{}", at("x", "y")),
            }
        })
        .collect::<Vec<_>>();
    if shown.is_empty() {
        return None;
    }
    let list = shown.join(" · ");
    let Some(first) = first_paste.map(|index| actions[index]) else {
        return Some(list);
    };
    let pasted = pasted(first, PASTE_SHOWN_CHARS);
    let chord = first
        .get("chord")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|chord| !chord.is_empty());
    let lead = match (language, chord) {
        (ResolvedLanguage::ZhCn, Some(chord)) => {
            format!("经剪贴板粘贴（{chord}；剪贴板之后保留这段文字）\"{pasted}\"——")
        }
        (ResolvedLanguage::ZhCn, None) => {
            format!("经剪贴板粘贴（剪贴板之后保留这段文字）\"{pasted}\"——")
        }
        (_, Some(chord)) => format!(
            "paste via the clipboard ({chord}; the clipboard keeps the text) \"{pasted}\" — "
        ),
        (_, None) => {
            format!("paste via the clipboard (the clipboard keeps the text) \"{pasted}\" — ")
        }
    };
    Some(format!("{lead}{list}"))
}

/// Falls back to the longest string argument, then to any scalar. "Longest"
/// beats "first" because argument order is the model's, and the most
/// informative field is rarely the alphabetically first one.
fn most_descriptive_scalar(input: &JsonObject) -> Option<String> {
    input
        .values()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .max_by_key(|value| value.chars().count())
        .map(str::to_owned)
        .or_else(|| {
            input
                .values()
                .find(|value| value.is_number() || value.is_boolean())
                .map(ToString::to_string)
        })
}

fn truncate_for_display(value: &str) -> String {
    // Collapse whitespace: a multi-line command must not push the composer off
    // screen, and the card is not where a diff gets reviewed.
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= MAX_SUMMARY_CHARS {
        return single_line;
    }
    let kept = single_line
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect::<String>();
    format!("{kept}…")
}

/// Neutralizes the bidi and zero-width characters that could make a displayed
/// command read as something other than what will run. Mirrors
/// `escape_approval_display_text` in `lib.rs`, which guards the same class of
/// spoofing for the text the native dialog used to show.
pub(crate) fn escape_display_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        ) {
            escaped.push_str(&format!("\\u{{{:04X}}}", character as u32));
        } else {
            escaped.push(character);
        }
    }
    escaped
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPromptDecision {
    Deny,
    AllowOnce,
    /// Allow this tool name for the rest of this conversation. Never offered
    /// for shell or MCP tools, nor for any `mandatory_prompt` decision.
    AllowAlways,
}

impl ToolPromptDecision {
    pub fn allows(self) -> bool {
        !matches!(self, Self::Deny)
    }
}

/// Which question a card is asking. The two kinds share one registry and one
/// resolve command, but only `Tool` is about authorizing a side effect, so only
/// `Tool` participates in blanket allowances.
///
/// The renderer draws `PlanExit` differently: no "always" button, and a
/// feedback box on the denial path, because "not yet, because…" is the answer
/// that keeps a planning turn going.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    #[default]
    Tool,
    /// The model finished a plan and wants to start implementing.
    PlanExit,
    /// `ask_user`: the model is blocked on the user's answers to its questions.
    /// The card carries the questions and is answered with a
    /// [`QuestionResponse`] rather than a yes/no, and it never times out —
    /// like the terminal dialog it mirrors, it waits for the user.
    Question,
}

/// One answer to one card. `feedback` is the user's prose, which only the plan
/// cards collect; it travels with the decision so the blocked worker can put it
/// in the tool result the model reads next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptAnswer {
    pub decision: ToolPromptDecision,
    pub feedback: Option<String>,
    /// What the user did with a question card. Only `PromptKind::Question`
    /// cards are answered with one; see [`ToolPromptRegistry::resolve_question`].
    pub question: Option<QuestionResponse>,
}

impl PromptAnswer {
    pub fn new(decision: ToolPromptDecision) -> Self {
        Self {
            decision,
            feedback: None,
            question: None,
        }
    }
}

/// The renderer's answer to a question card, as the user left it.
///
/// `answers` and `notes` are index-aligned with the card's questions: slot `i`
/// belongs to question `i`, and an unanswered question is `None`. A
/// multi-select answer arrives already joined with `", "`, which is how the
/// model reads it. The host, not the renderer, turns this into the tool result
/// text, so the wording the model sees cannot be supplied by the page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionResponse {
    pub action: QuestionAction,
    #[serde(default)]
    pub answers: Vec<Option<String>>,
    /// The preview text of the option the user picked, per question.
    #[serde(default)]
    pub previews: Vec<Option<String>>,
    /// Free-form notes the user attached to a question.
    #[serde(default)]
    pub notes: Vec<Option<String>>,
}

/// How the user left the question card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionAction {
    /// Handed back their answers (possibly not every question) — from the
    /// card's own submit, or by sending a message from the composer with some
    /// of the card filled in.
    #[default]
    Submit,
    /// Closed the card without answering. On its own that ends the turn; when
    /// the user closed it by sending a composer message, the message follows
    /// the result into the next round instead.
    Close,
    /// "Chat about this": the questions are declined, with whatever was
    /// answered so far handed back so the model can clarify.
    Chat,
}

/// Normalizes renderer-supplied card feedback: trimmed, empty treated as
/// absent, and capped, because this text is quoted verbatim into the tool
/// result the model reads next.
pub fn sanitize_prompt_feedback(feedback: Option<String>) -> Option<String> {
    let feedback = feedback?;
    let trimmed = feedback.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_FEEDBACK_CHARS).collect())
}

/// What the renderer needs to draw one card. Serialized straight to the
/// manual-path command result and mirrored field-for-field by
/// `ModelStreamEvent::ToolApprovalRequested`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PendingToolPrompt {
    pub prompt_id: String,
    pub tool_name: String,
    /// Absent in cards persisted or sent before plan mode existed, which were
    /// all ordinary tool approvals.
    #[serde(default)]
    pub kind: PromptKind,
    pub label: String,
    pub summary: String,
    pub risk_level: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
    /// Machine-readable requester address, unlike `requester` which is
    /// display-escaped text. The addressable child name (`sourceAgent`) plus
    /// the parent-timeline call id of the child's turn (`sourceCallId`); both
    /// absent for the main session's own calls. The renderer uses them to open
    /// the requesting child's page when the card arrives — the streaming view
    /// of a workflow step is keyed by the call id, not the pool name the card
    /// displays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_call_id: Option<String>,
    pub allow_always_offered: bool,
    /// Whether this card appears regardless of the conversation's security
    /// level. `allow_always_offered` cannot answer that question: an ordinary
    /// shell or MCP call also declines blanket permission while still being an
    /// approval the level decides. Without this the user sees an identical card
    /// under full access and reasonably concludes the level is broken, when what
    /// they are looking at is a deliberate circuit breaker.
    #[serde(default)]
    pub mandatory: bool,
    /// The questions a `PromptKind::Question` card asks, as the model wrote
    /// them (the `questions` array of the `ask_user` input). Absent on every
    /// other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<Value>,
}

/// Why a prompt stopped waiting. Only `Answered` carries the user's intent.
#[derive(Debug)]
enum PromptOutcome {
    Answered(PromptAnswer),
    /// A flag the waiter asked to be watched went up: the run was stopped, or
    /// the task that raised the card was stopped from the sidebar.
    Cancelled,
    TimedOut,
    /// The registry entry disappeared without an answer — today only
    /// [`ToolPromptRegistry::cancel_run_prompts`] and `clear` do that.
    Retracted,
}

/// Who is blocked on a model prompt, which is what decides whether the end of a
/// turn is allowed to answer it.
///
/// A run end retracts only cards raised by that run. A detached task's card
/// remains pending until it receives an answer, its task stops, or it times out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptOwner {
    /// Raised on the thread of the named top-level model run.
    Run(String),
    /// Raised by a detached task worker (subagent, workflow step, workflow
    /// driver). Tasks outlive the turn that spawned them (S11), so no turn
    /// boundary retracts this card; only an answer, the task's own stop flag,
    /// or the timeout ends it.
    Task,
}

enum PendingPrompt {
    /// A blocked worker thread. The conversation is kept so one run ending
    /// cannot cancel a concurrent run's card — runs hold a shared lease, so
    /// more than one can be live at a time.
    Model {
        conversation_id: String,
        owner: PromptOwner,
        /// The card as the renderer would draw it. Held rather than rebuilt so
        /// a renderer that reloaded (or connected late) can be handed the open
        /// cards verbatim instead of leaving a worker blocked behind a card
        /// nobody can see.
        card: PendingToolPrompt,
        sender: SyncSender<PromptAnswer>,
    },
    /// A renderer-initiated tool-editor call. The classified request is held
    /// here rather than resent with the answer, so the renderer cannot widen
    /// the call between being asked about it and answering.
    Manual {
        request: Box<ToolExecutionRequest>,
        /// The risk the classifier gave this call when the card was raised. An
        /// "always" answer is recorded at this level, never at "any".
        risk_level: RiskLevel,
        opened_at: Instant,
    },
}

/// What `resolve` found. The manual arm hands the request back so the caller
/// can mint a nonce for exactly what was classified.
pub enum PromptResolution {
    Model,
    Manual {
        request: Box<ToolExecutionRequest>,
        decision: ToolPromptDecision,
    },
}

/// Identifies the conversation that owns a card for capacity and fairness.
fn prompt_conversation(prompt: &PendingPrompt) -> &str {
    match prompt {
        PendingPrompt::Model {
            conversation_id, ..
        } => conversation_id,
        PendingPrompt::Manual { request, .. } => &request.conversation_id,
    }
}

#[derive(Default)]
pub struct ToolPromptRegistry {
    pending: Mutex<HashMap<String, PendingPrompt>>,
    allowances: Mutex<StandingAllowances>,
}

/// What each conversation said "always" to: tool name → the risk level of the
/// call the user was looking at when they said it.
///
/// The risk level is part of the meaning. An allowance may answer only calls
/// no riskier than the one for which it was granted.
///
/// Saved in the conversation store, which deletes a conversation's rows with
/// the conversation; this is a cache of the rows read so far, filled one
/// conversation at a time as its calls are classified. Except for an allowance
/// that names a screen ([`allowance_names_a_screen`]), which lives here and
/// nowhere else, so it ends with the process.
#[derive(Default)]
struct StandingAllowances {
    store: Option<std::sync::Arc<crate::conversation_store::ConversationStore>>,
    loaded: HashMap<String, HashMap<String, RiskLevel>>,
}

impl StandingAllowances {
    fn of(&mut self, conversation_id: &str) -> &mut HashMap<String, RiskLevel> {
        let store = self.store.clone();
        self.loaded
            .entry(conversation_id.to_owned())
            .or_insert_with(|| {
                let Some(store) = store else {
                    return HashMap::new();
                };
                match store.tool_allowances(conversation_id) {
                    // A row that names a screen is skipped: its handle was minted by an
                    // earlier process and means nothing in this one.
                    Ok(rows) => rows
                        .into_iter()
                        .filter(|(tool, _)| !allowance_names_a_screen(tool))
                        .filter_map(|(tool, risk)| Some((tool, RiskLevel::from_slug(&risk)?)))
                        .collect(),
                    Err(error) => {
                        eprintln!("Could not read the saved \"always allow\" answers of {conversation_id}: {error}");
                        HashMap::new()
                    }
                }
            })
    }
}

impl ToolPromptRegistry {
    /// Saves "always allow" answers in `store` from now on, and reads earlier
    /// ones from it. Attached once the document's store is open; until then
    /// (and in tests that never attach one) answers last for the process.
    pub fn attach_allowance_store(
        &self,
        store: std::sync::Arc<crate::conversation_store::ConversationStore>,
    ) {
        let mut allowances = self
            .allowances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        allowances.store = Some(store);
        allowances.loaded.clear();
    }

    /// True when this conversation already blanket-allowed this tool name **at
    /// or above** `risk_level`. Callers must not consult this for a
    /// `mandatory_prompt` decision.
    ///
    /// The risk bound is what keeps "stop asking about this class of call" from
    /// meaning "stop asking about this tool, whatever it is about to touch".
    ///
    /// `tool_name` is the call's [`allowance_key`]: for a screen tool it names the screen (and the
    /// action), and its bare name is never allowed ([`allowance_is_keyed`]).
    pub fn is_always_allowed(
        &self,
        conversation_id: &str,
        tool_name: &str,
        risk_level: RiskLevel,
    ) -> bool {
        if !allowance_is_keyed(tool_name) {
            return false;
        }
        self.allowances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .of(conversation_id)
            .get(tool_name)
            .is_some_and(|granted| risk_level <= *granted)
    }

    /// Records an allowance, keeping the **highest** risk explicitly granted
    /// for the pair. Widening only ever happens by the user answering a card
    /// they were shown: a High call is only ever seen because the standing
    /// Medium grant did not cover it.
    ///
    /// `tool_name` is the call's [`allowance_key`]; a screen tool's bare
    /// name, which nothing would ever look up, is not recorded. One that names
    /// a screen is recorded in memory only ([`allowance_names_a_screen`]).
    fn remember_allowance(&self, conversation_id: &str, tool_name: &str, risk_level: RiskLevel) {
        if !allowance_is_keyed(tool_name) {
            return;
        }
        let mut allowances = self
            .allowances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let granted = *allowances
            .of(conversation_id)
            .entry(tool_name.to_owned())
            .and_modify(|granted| *granted = (*granted).max(risk_level))
            .or_insert(risk_level);
        if allowance_names_a_screen(tool_name) {
            return;
        }
        if let Some(store) = allowances.store.as_ref() {
            if let Err(error) = store.put_tool_allowance(conversation_id, tool_name, granted.slug()) {
                eprintln!("Could not save the \"always allow\" answer for {tool_name} in {conversation_id}: {error}");
            }
        }
    }

    /// Registers a prompt under a fresh id. Abandoned manual prompts are
    /// evicted here, which is the only moment anything sweeps them.
    ///
    /// `build` receives the minted id because a model prompt stores the card
    /// the renderer will draw, and the card carries its own id.
    ///
    /// Capacity is per conversation: only cards in this card's conversation
    /// count toward [`MAX_PENDING_PROMPTS_PER_CONVERSATION`].
    fn register(&self, build: impl FnOnce(&str) -> PendingPrompt) -> Result<String, String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        pending.retain(|_, entry| match entry {
            PendingPrompt::Manual { opened_at, .. } => {
                now.duration_since(*opened_at) < MANUAL_PROMPT_TTL
            }
            PendingPrompt::Model { .. } => true,
        });
        let id = loop {
            let candidate = Uuid::new_v4().to_string();
            if !pending.contains_key(&candidate) {
                break candidate;
            }
        };
        let prompt = build(&id);
        let conversation = prompt_conversation(&prompt).to_owned();
        if pending
            .values()
            .filter(|entry| prompt_conversation(entry) == conversation)
            .count()
            >= MAX_PENDING_PROMPTS_PER_CONVERSATION
        {
            return Err("Too many tool calls are awaiting confirmation; resolve existing confirmations first".into());
        }
        pending.insert(id.clone(), prompt);
        Ok(id)
    }

    fn close(&self, prompt_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(prompt_id);
    }

    /// Opens a manual prompt for a renderer-initiated call and returns the id
    /// the renderer must answer with. The request is retained until then.
    pub fn open_manual(
        &self,
        request: &ToolExecutionRequest,
        risk_level: RiskLevel,
    ) -> Result<String, String> {
        self.register(|_| PendingPrompt::Manual {
            request: Box::new(request.clone()),
            risk_level,
            opened_at: Instant::now(),
        })
    }

    /// Every waiting model card, paired with the conversation it belongs to.
    pub fn all_pending_cards(&self) -> Vec<(String, PendingToolPrompt)> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .filter_map(|entry| match entry {
                PendingPrompt::Model {
                    conversation_id,
                    card,
                    ..
                } => Some((conversation_id.clone(), card.clone())),
                PendingPrompt::Manual { .. } => None,
            })
            .collect()
    }

    /// Renderer-side resolution. Unknown ids are an error rather than a silent
    /// success so a double-click or a stale card cannot read as consent for
    /// whatever prompt happens to be open next.
    ///
    /// `feedback` is the prose the plan cards collect. The manual path has no
    /// card that asks for it and drops it.
    ///
    /// A question card only accepts `Deny` here — the user dismissing the
    /// questions. Answers go through [`resolve_question`](Self::resolve_question);
    /// a bare "allow" would hand the model an empty answer the user never gave.
    pub fn resolve(
        &self,
        prompt_id: &str,
        decision: ToolPromptDecision,
        feedback: Option<String>,
    ) -> Result<PromptResolution, String> {
        let entry = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let question = matches!(
                pending.get(prompt_id),
                Some(PendingPrompt::Model { card, .. }) if card.kind == PromptKind::Question
            );
            if question && decision.allows() {
                return Err("A question card is answered with answers, not an approval".into());
            }
            pending
                .remove(prompt_id)
                .ok_or_else(|| "This tool confirmation has ended or does not exist".to_owned())?
        };
        match entry {
            PendingPrompt::Model { sender, .. } => {
                // The receiver is gone only if the waiter already bailed out;
                // the prompt is over either way.
                let _ = sender.try_send(PromptAnswer {
                    decision,
                    feedback,
                    question: None,
                });
                Ok(PromptResolution::Model)
            }
            PendingPrompt::Manual {
                request,
                risk_level,
                ..
            } => {
                // A manual prompt is never mandatory — the classifier's
                // mandatory decisions all arise inside a model run — so an
                // "always" answer here is honoured, at the risk it was shown at.
                if decision == ToolPromptDecision::AllowAlways {
                    self.remember_allowance(
                        &request.conversation_id,
                        &allowance_key(&request.tool_name, &request.input, None),
                        risk_level,
                    );
                }
                Ok(PromptResolution::Manual { request, decision })
            }
        }
    }

    /// Answers a question card with what the user did in it.
    ///
    /// `Submit` wakes the worker with an allowing decision and `Chat` with a
    /// denying one; both carry the response, which is what the worker formats
    /// into the tool result. The answer must line up with the card's
    /// questions — one slot per question — so a stale page cannot answer a
    /// card whose questions it never saw.
    pub fn resolve_question(
        &self,
        prompt_id: &str,
        response: QuestionResponse,
    ) -> Result<(), String> {
        let entry = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(PendingPrompt::Model { card, .. }) = pending.get(prompt_id) else {
                return Err("This question has ended or does not exist".into());
            };
            if card.kind != PromptKind::Question {
                return Err("This card is not a question".into());
            }
            let count = card
                .questions
                .as_ref()
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let aligned = |slots: &[Option<String>]| slots.is_empty() || slots.len() == count;
            // A close carries no answers at all.
            let answers_aligned = if response.action == QuestionAction::Close {
                response.answers.is_empty() || response.answers.len() == count
            } else {
                response.answers.len() == count
            };
            if !answers_aligned
                || !aligned(&response.previews)
                || !aligned(&response.notes)
            {
                return Err("The answers do not match this card's questions".into());
            }
            pending
                .remove(prompt_id)
                .ok_or_else(|| "This question has ended or does not exist".to_owned())?
        };
        let PendingPrompt::Model { sender, .. } = entry else {
            unreachable!("checked above");
        };
        let decision = match response.action {
            QuestionAction::Submit => ToolPromptDecision::AllowOnce,
            QuestionAction::Close | QuestionAction::Chat => ToolPromptDecision::Deny,
        };
        let _ = sender.try_send(PromptAnswer {
            decision,
            feedback: None,
            question: Some(response),
        });
        Ok(())
    }

    /// Retracts the cards **one finished run** raised on its own thread.
    ///
    /// Scoped to `PromptOwner::Run(request_id)` on purpose. The conversation-wide
    /// version this replaced took a task's card down with the turn and caused a
    /// fabricated refusal. A task's card is retracted by its own stop flag or by an answer,
    /// never by a turn boundary. Concurrent runs, other conversations, and the
    /// renderer's own tool-editor prompts are left alone.
    pub fn cancel_run_prompts(&self, conversation_id: &str, request_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, entry| match entry {
                PendingPrompt::Model {
                    conversation_id: owner,
                    owner: PromptOwner::Run(run),
                    ..
                } => !(owner == conversation_id && run == request_id),
                _ => true,
            });
    }

    pub fn clear(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        // The rows went with the conversations the reset purged; only what
        // was read of them is dropped here.
        self.allowances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .loaded
            .clear();
    }

    /// The boolean projection of [`ask_answer`](Self::ask_answer), for the
    /// yes/no cards whose prose is never read. Production callers go through
    /// `api::ask_announced_prompt`, which needs the prose for the plan cards.
    #[cfg(test)]
    pub fn ask(
        &self,
        conversation_id: &str,
        owner: PromptOwner,
        mandatory: bool,
        risk_level: RiskLevel,
        cancellations: &[&AtomicBool],
        card: PendingToolPrompt,
        announce: impl FnOnce(&PendingToolPrompt) -> Result<(), String>,
    ) -> Result<bool, String> {
        self.ask_answer(
            conversation_id,
            owner,
            mandatory,
            risk_level,
            cancellations,
            card,
            announce,
        )
        .map(|answer| answer.decision.allows())
    }

    /// [`ask_answer_as`](Self::ask_answer_as) with the allowance kept under
    /// the card's tool name, for the tests of a tool whose allowance is not
    /// kept per action.
    #[cfg(test)]
    pub fn ask_answer(
        &self,
        conversation_id: &str,
        owner: PromptOwner,
        mandatory: bool,
        risk_level: RiskLevel,
        cancellations: &[&AtomicBool],
        card: PendingToolPrompt,
        announce: impl FnOnce(&PendingToolPrompt) -> Result<(), String>,
    ) -> Result<PromptAnswer, String> {
        let allowance = card.tool_name.clone();
        self.ask_answer_as(
            conversation_id,
            &allowance,
            owner,
            mandatory,
            risk_level,
            cancellations,
            card,
            announce,
        )
    }

    /// Opens a model prompt, hands the card to `announce`, and blocks until the
    /// renderer answers, one of `cancellations` goes up, or the prompt times
    /// out.
    ///
    /// `card.prompt_id` is ignored; the registry mints it and hands `announce`
    /// the finished card.
    ///
    /// `cancellations` is a list rather than one flag because a task worker
    /// watches two things at once: the run that happens to be live (so stopping
    /// generation retracts the card) and its own task stop flag (so the sidebar
    /// stop button releases it). A background task with no live run passes just
    /// its own flag — and a card raised with no flags at all is still bounded by
    /// `PROMPT_TIMEOUT`.
    ///
    /// `announce` is what puts the card on screen. If it fails, the prompt is
    /// closed immediately rather than waiting out a card nobody can see.
    ///
    /// Only a user's `Deny` returns an answer that does not allow. Stopped,
    /// timed out, and retracted are `Err`, because callers must not attribute a
    /// refusal to the user when the user did not answer. The user's prose comes
    /// back with the decision; only the plan cards have anything to do with it.
    ///
    /// Blanket allowances are a property of authorizing a *tool*, so a card
    /// whose `kind` is not `Tool` neither consults nor records one — a user who
    /// once said "always allow" for some tool has not thereby approved a plan,
    /// and approving one plan must not silently answer a later card.
    ///
    /// A standing allowance is looked up and recorded under `allowance`: the
    /// call's [`allowance_key`] — its tool name, or for a screen tool the
    /// name, the action and the screen — and the card's own tool name for
    /// every other card.
    pub fn ask_answer_as(
        &self,
        conversation_id: &str,
        allowance: &str,
        owner: PromptOwner,
        mandatory: bool,
        risk_level: RiskLevel,
        cancellations: &[&AtomicBool],
        card: PendingToolPrompt,
        announce: impl FnOnce(&PendingToolPrompt) -> Result<(), String>,
    ) -> Result<PromptAnswer, String> {
        let tool_name = card.tool_name.clone();
        let never_times_out = card.kind == PromptKind::Question;
        let blanket_allowable =
            card.kind == PromptKind::Tool && !mandatory && !never_blanket_allowed(&tool_name);
        if blanket_allowable && self.is_always_allowed(conversation_id, allowance, risk_level) {
            return Ok(PromptAnswer::new(ToolPromptDecision::AllowAlways));
        }
        let stopped = || {
            cancellations
                .iter()
                .any(|flag| flag.load(Ordering::Acquire))
        };
        // Asking at all is pointless once the waiter is already stopped, and it
        // would flash a card the renderer must then retract.
        if stopped() {
            return Err("This tool call was stopped before requesting confirmation".into());
        }
        let (sender, receiver) = sync_channel(1);
        let conversation = conversation_id.to_owned();
        let mut announced = None;
        let prompt_id = self.register(|id| {
            let card = PendingToolPrompt {
                prompt_id: id.to_owned(),
                ..card.clone()
            };
            announced = Some(card.clone());
            PendingPrompt::Model {
                conversation_id: conversation.clone(),
                owner,
                card,
                sender,
            }
        })?;
        let announced = announced.expect("register always runs the builder");
        if let Err(error) = announce(&announced) {
            self.close(&prompt_id);
            return Err(format!(
                "Could not display the tool confirmation card: {error}"
            ));
        }

        let mut waited = Duration::ZERO;
        let outcome = loop {
            if stopped() {
                break PromptOutcome::Cancelled;
            }
            match receiver.recv_timeout(CANCELLATION_POLL) {
                Ok(answer) => break PromptOutcome::Answered(answer),
                Err(RecvTimeoutError::Disconnected) => break PromptOutcome::Retracted,
                Err(RecvTimeoutError::Timeout) => {
                    waited += CANCELLATION_POLL;
                    // A question waits for its answer the way the terminal
                    // dialog does; the run's stop button is what releases it.
                    if !never_times_out && waited >= PROMPT_TIMEOUT {
                        break PromptOutcome::TimedOut;
                    }
                }
            }
        };
        self.close(&prompt_id);

        match outcome {
            PromptOutcome::Answered(answer) => {
                // Cancellation wins if it races with an Allow decision. Recheck
                // after receiving the decision so execution cannot proceed in a
                // cancelled run.
                if stopped() {
                    return Err("This tool call was stopped while awaiting confirmation".into());
                }
                // An allowance is only ever recorded for a decision the level,
                // not the tool, was allowed to settle.
                if blanket_allowable && answer.decision == ToolPromptDecision::AllowAlways {
                    self.remember_allowance(conversation_id, allowance, risk_level);
                }
                Ok(answer)
            }
            PromptOutcome::Cancelled => {
                Err("This tool call was stopped while awaiting confirmation".into())
            }
            PromptOutcome::Retracted => {
                Err("This tool call's confirmation card was retracted without authorization".into())
            }
            PromptOutcome::TimedOut => {
                Err("Tool confirmation timed out without a response; this call was denied".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::thread;

    fn registry() -> Arc<ToolPromptRegistry> {
        Arc::new(ToolPromptRegistry::default())
    }

    fn object(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap()
    }

    fn manual_request(conversation: &str, tool: &str) -> ToolExecutionRequest {
        ToolExecutionRequest {
            conversation_id: conversation.into(),
            workspace_path: "/workspace".into(),
            tool_name: tool.into(),
            input: object(json!({"path": "notes.md"})),
        }
    }

    /// The display half of a model prompt. `prompt_id` is whatever the registry
    /// mints, so the placeholder here is deliberately wrong.
    fn card(tool: &str) -> PendingToolPrompt {
        PendingToolPrompt {
            prompt_id: "minted-by-the-registry".into(),
            tool_name: tool.into(),
            kind: PromptKind::Tool,
            label: tool.into(),
            summary: "notes.md".into(),
            risk_level: "中".into(),
            reason: "测试".into(),
            requester: None,
            source_agent: None,
            source_call_id: None,
            allow_always_offered: true,
            mandatory: false,
            questions: None,
        }
    }

    /// Answers the prompt `ask` is about to open, retrying until the waiter has
    /// registered it. Returns a handle the test joins to surface panics.
    fn answer_next_prompt(
        registry: &Arc<ToolPromptRegistry>,
        decision: ToolPromptDecision,
    ) -> (SyncSender<String>, thread::JoinHandle<()>) {
        let answering = Arc::clone(registry);
        let (announce, announced) = sync_channel(1);
        let responder = thread::spawn(move || {
            let id: String = announced.recv().expect("prompt was never announced");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if answering.resolve(&id, decision, None).is_ok() {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("prompt never became resolvable");
        });
        (announce, responder)
    }

    /// The whole point of the card: the worker blocks, the renderer answers,
    /// and the answer — not a default — decides.
    #[test]
    fn a_renderer_decision_unblocks_the_waiting_worker() {
        for (decision, expected) in [
            (ToolPromptDecision::Deny, false),
            (ToolPromptDecision::AllowOnce, true),
            (ToolPromptDecision::AllowAlways, true),
        ] {
            let registry = registry();
            let (announce, responder) = answer_next_prompt(&registry, decision);
            let allowed = registry
                .ask(
                    "conversation-a",
                    PromptOwner::Run("run-1".into()),
                    false,
                    RiskLevel::Medium,
                    &[],
                    card("write"),
                    |prompt| {
                        announce.send(prompt.prompt_id.clone()).unwrap();
                        Ok(())
                    },
                )
                .unwrap();
            responder.join().unwrap();
            assert_eq!(allowed, expected, "{decision:?}");
        }
    }

    /// An answer is saved with its conversation: a later process reads it
    /// back, and it goes when the conversation is deleted.
    #[test]
    fn an_allowance_outlives_a_restart_and_dies_with_its_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::conversation_store::store_for(&directory.path().join("document.v1.json")).unwrap();
        let document = crate::catalog::default_document();
        let conversation = document.workspaces[0].conversations[0].clone();
        store.put_conversation(&document.workspaces[0].id, &conversation).unwrap();

        let registry = registry();
        registry.attach_allowance_store(store.clone());
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask(
                &conversation.id,
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();

        let restarted = ToolPromptRegistry::default();
        restarted.attach_allowance_store(store.clone());
        assert!(restarted.is_always_allowed(&conversation.id, "write", RiskLevel::Medium));
        assert!(!restarted.is_always_allowed(&conversation.id, "write", RiskLevel::High));

        store.delete_conversation(&conversation.id).unwrap();
        let after_delete = ToolPromptRegistry::default();
        after_delete.attach_allowance_store(store);
        assert!(!after_delete.is_always_allowed(&conversation.id, "write", RiskLevel::Medium));
    }

    /// A standing allowance covers only the same tool and conversation. Anything
    /// wider would authorize calls the user never saw.
    #[test]
    fn an_allowance_covers_only_its_own_tool_and_conversation() {
        let registry = registry();
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();

        assert!(registry.is_always_allowed("conversation-a", "write", RiskLevel::Medium));
        assert!(!registry.is_always_allowed("conversation-a", "bash", RiskLevel::Medium));
        assert!(!registry.is_always_allowed("conversation-b", "write", RiskLevel::Medium));

        // The allowance now answers without ever announcing a card.
        let allowed = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |_| panic!("an allowed tool must not open another prompt"),
            )
            .unwrap();
        assert!(allowed);
    }

    /// The user's prose is part of the answer, not a side channel: a denied
    /// plan card is only useful if what the user wants changed comes back with
    /// the refusal.
    #[test]
    fn a_plan_card_carries_the_users_prose_back_to_the_waiting_worker() {
        let registry = registry();
        let answering = Arc::clone(&registry);
        let (announce, announced) = sync_channel(1);
        let responder = thread::spawn(move || {
            let id: String = announced.recv().expect("prompt was never announced");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                // Padded and over-long prose is normalized on the way in, the
                // same way the command does it for the renderer.
                let feedback = sanitize_prompt_feedback(Some(format!(
                    "  start with the storage layer{}  ",
                    "!".repeat(MAX_FEEDBACK_CHARS)
                )));
                if answering
                    .resolve(&id, ToolPromptDecision::Deny, feedback.clone())
                    .is_ok()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("prompt never became resolvable");
        });
        let mut exit = card("exit_plan_mode");
        exit.kind = PromptKind::PlanExit;
        exit.mandatory = true;
        let answer = registry
            .ask_answer(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                true,
                RiskLevel::Low,
                &[],
                exit,
                |prompt| {
                    assert_eq!(prompt.kind, PromptKind::PlanExit);
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert_eq!(answer.decision, ToolPromptDecision::Deny);
        let feedback = answer.feedback.expect("the refusal carries its reason");
        assert!(feedback.starts_with("start with the storage layer"));
        assert_eq!(feedback.chars().count(), MAX_FEEDBACK_CHARS);

        // Nothing to say is `None`, not an empty string a caller would quote.
        assert_eq!(sanitize_prompt_feedback(Some("   ".into())), None);
        assert_eq!(sanitize_prompt_feedback(None), None);
    }

    /// Blanket allowances authorize a *tool*. Saying "always allow" to some
    /// tool has not approved a plan, and approving one plan must not answer a
    /// later card by itself.
    #[test]
    fn a_plan_card_neither_records_nor_consults_an_allowance() {
        let registry = registry();
        let plan_card = || {
            let mut exit = card("exit_plan_mode");
            exit.kind = PromptKind::PlanExit;
            exit
        };

        // `mandatory` is false here so the only thing keeping the allowance
        // machinery away is the card's kind.
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask_answer(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Low,
                &[],
                plan_card(),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(!registry.is_always_allowed("conversation-a", "exit_plan_mode", RiskLevel::Low));

        // Even with an allowance recorded for that name by an ordinary tool
        // card, the plan card still goes to the user.
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        let mut ordinary = card("exit_plan_mode");
        ordinary.kind = PromptKind::Tool;
        registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Low,
                &[],
                ordinary,
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(registry.is_always_allowed("conversation-a", "exit_plan_mode", RiskLevel::Low));

        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::Deny);
        let answer = registry
            .ask_answer(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Low,
                &[],
                plan_card(),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert_eq!(answer.decision, ToolPromptDecision::Deny);
    }

    /// A standing allowance covers a call class, not every use of that tool.
    /// A Medium in-workspace `write` allowance cannot answer a High `write`
    /// outside the workspace or application data directory.
    #[test]
    fn an_allowance_does_not_answer_a_riskier_call_of_the_same_tool() {
        let registry = registry();
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();

        // An allowance at the same or lower risk does not ask again.
        assert!(registry.is_always_allowed("conversation-a", "write", RiskLevel::Medium));
        assert!(registry.is_always_allowed("conversation-a", "write", RiskLevel::Low));
        // A higher-risk call must ask again.
        assert!(!registry.is_always_allowed("conversation-a", "write", RiskLevel::High));

        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::Deny);
        let mut asked = false;
        let allowed = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::High,
                &[],
                card("write"),
                |prompt| {
                    asked = true;
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(asked, "高风险的同名调用必须重新立卡");
        assert!(!allowed);

        // The allowance expands to High only after the user allows the High-risk card.
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::High,
                &[],
                card("write"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(registry.is_always_allowed("conversation-a", "write", RiskLevel::High));
    }

    /// The preview page tools have an unconditional human gate for signed-in tab takeover.
    /// Since it uses the same `ask` path, a standing allowance must never answer it.
    #[test]
    fn preview_page_tools_never_carry_a_standing_allowance() {
        let registry = registry();
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);
        registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::High,
                &[],
                card("preview_click"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();

        // An AllowAlways decision is never persisted for this tool.
        assert!(!registry.is_always_allowed("conversation-a", "preview_click", RiskLevel::High));
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::Deny);
        let mut asked = false;
        let allowed = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::High,
                &[],
                card("preview_click"),
                |prompt| {
                    asked = true;
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(asked, "The takeover gate must ask every time");
        assert!(!allowed);
        // Shell tools follow the same rule, and so does every other preview page tool. The four
        // host-side ones never touch a page, so they are ordinary cards.
        assert!(never_blanket_allowed("bash"));
        assert!(never_blanket_allowed("powershell"));
        assert!(never_blanket_allowed("preview_snapshot"));
        assert!(never_blanket_allowed("preview_upload_image"));
        // `preview_dialog` is a page tool, so it is held to this line on a phone's alert too.
        assert!(never_blanket_allowed("preview_dialog"));
        // The screen tool is not a page tool, and is held to the same line by name.
        assert!(crate::browser::PreviewTool::from_tool_name("preview_input").is_none());
        assert!(never_blanket_allowed("preview_input"));
        assert!(!never_blanket_allowed("preview_start"));
        assert!(!never_blanket_allowed("preview_logs"));
        assert!(!never_blanket_allowed("write"));
    }

    /// Answers "always allow" at `risk` to the card a call of `tool` judged under `key` raises,
    /// as the model's approval path does (`ask_answer_as`).
    fn grant_always(
        registry: &Arc<ToolPromptRegistry>,
        conversation: &str,
        tool: &str,
        key: &str,
        risk: RiskLevel,
    ) {
        let (announce, responder) = answer_next_prompt(registry, ToolPromptDecision::AllowAlways);
        let answer = registry
            .ask_answer_as(
                conversation,
                key,
                PromptOwner::Run("run-1".into()),
                false,
                risk,
                &[],
                card(tool),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert_eq!(answer.decision, ToolPromptDecision::AllowAlways, "{key}");
    }

    /// Whether a call of `tool` judged under `key` raises a card. The user denies the card when
    /// one is raised; a call a standing allowance covers is answered without one.
    fn raised_a_card(
        registry: &Arc<ToolPromptRegistry>,
        conversation: &str,
        tool: &str,
        key: &str,
        risk: RiskLevel,
    ) -> bool {
        let answering = Arc::clone(registry);
        let (announce, announced) = sync_channel::<String>(1);
        let responder = thread::spawn(move || {
            // The sender is dropped when no card was announced.
            let Ok(id) = announced.recv() else {
                return false;
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if answering
                    .resolve(&id, ToolPromptDecision::Deny, None)
                    .is_ok()
                {
                    return true;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("prompt never became resolvable");
        });
        let answer = registry
            .ask_answer_as(
                conversation,
                key,
                PromptOwner::Run("run-1".into()),
                false,
                risk,
                &[],
                card(tool),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        drop(announce);
        let raised = responder.join().unwrap();
        assert_eq!(answer.decision.allows(), !raised, "{key}");
        raised
    }

    /// A device's shell line is as open-ended as `preview_input`, so no allowance answers it. An
    /// answer to a phone's alert is `preview_dialog`'s, a page tool and so never allowed either,
    /// and a paste is `preview_input`'s; the other screen tools each do one thing a card says in
    /// full, and may carry an allowance — for one screen.
    #[test]
    fn the_open_ended_screen_tools_never_carry_a_standing_allowance() {
        use crate::tool_executor::screen_tools::SCREEN_TOOLS;
        assert_eq!(OPEN_ENDED_SCREEN_TOOLS, ["android_shell"]);
        let shell = object(json!({"command": "getprop"}));
        assert!(never_blanket_allowed("android_shell"));
        let key = allowance_key("android_shell", &shell, Some("h1"));
        assert_eq!(key, "android_shell@h1");
        let prompts = registry();
        grant_always(
            &prompts,
            "conversation-a",
            "android_shell",
            &key,
            RiskLevel::High,
        );
        assert!(!prompts.is_always_allowed("conversation-a", &key, RiskLevel::Low));
        assert!(raised_a_card(
            &prompts,
            "conversation-a",
            "android_shell",
            &key,
            RiskLevel::Low
        ));

        // `preview_dialog` is a page tool, never blanket-allowed by the page-tool rule wherever it
        // runs, on a screen too; `preview_input` is never allowed by name.
        assert!(crate::browser::PreviewTool::from_tool_name("preview_dialog").is_some());
        assert!(crate::browser::PreviewTool::from_tool_name("preview_input").is_none());
        for tool in ["preview_dialog", "preview_input"] {
            assert!(never_blanket_allowed(tool), "{tool}");
            // Neither is a screen tool, so a screen never turns its name into a key.
            assert_eq!(allowance_key(tool, &object(json!({})), Some("h1")), tool);
            for screen in [None, Some("h1")] {
                let prompts = registry();
                let key = allowance_key(tool, &object(json!({})), screen);
                grant_always(&prompts, "conversation-a", tool, &key, RiskLevel::High);
                assert!(
                    !prompts.is_always_allowed("conversation-a", &key, RiskLevel::Low),
                    "{tool}"
                );
                assert!(raised_a_card(
                    &prompts,
                    "conversation-a",
                    tool,
                    &key,
                    RiskLevel::Low
                ));
            }
        }

        // Every other screen tool may carry an allowance, `preview_zoom` and `preview_wait`
        // among them, which share the page tools' prefix and nothing else.
        for tool in SCREEN_TOOLS {
            assert_eq!(
                never_blanket_allowed(tool),
                OPEN_ENDED_SCREEN_TOOLS.contains(&tool),
                "{tool}"
            );
        }
        for tool in ["preview_zoom", "preview_wait"] {
            assert!(crate::browser::PreviewTool::from_tool_name(tool).is_none());
            assert!(!never_blanket_allowed(tool), "{tool}");
        }
    }

    /// A screen tool's allowance key names the screen and, for the tools that both look and act,
    /// the action; a call judged for no screen gets its bare name, which keeps nothing; every
    /// other tool keeps its plain name.
    #[test]
    fn a_screen_tools_allowance_key_names_its_screen_and_action() {
        use crate::tool_executor::screen_tools::{screen_tool_platforms, SCREEN_TOOLS};
        let key = |tool: &str, input: Value, screen: Option<&str>| {
            allowance_key(tool, &object(input), screen)
        };
        // Per action, with the default an absent action means; an unknown or non-string one is
        // `other`, kept apart from every real action.
        assert_eq!(
            key("screen_app", json!({}), Some("h1")),
            "screen_app#list@h1"
        );
        assert_eq!(
            key("screen_app", json!({"action": null}), Some("h1")),
            "screen_app#list@h1"
        );
        assert_eq!(
            key(
                "screen_app",
                json!({"action": "launch", "app": "com.foo"}),
                Some("h1")
            ),
            "screen_app#launch@h1"
        );
        assert_eq!(
            key(
                "screen_app",
                json!({"action": "open_url", "url": "x"}),
                Some("h1")
            ),
            "screen_app#open_url@h1"
        );
        assert_eq!(
            key("screen_app", json!({"action": "uninstall"}), Some("h1")),
            "screen_app#other@h1"
        );
        assert_eq!(
            key("screen_app", json!({"action": 3}), Some("h1")),
            "screen_app#other@h1"
        );
        assert_eq!(
            key("screen_app", json!({"action": "LAUNCH"}), Some("h1")),
            "screen_app#other@h1"
        );
        assert_eq!(
            key("screen_device", json!({}), Some("h1")),
            "screen_device#info@h1"
        );
        assert_eq!(
            key("screen_device", json!({"action": "lock"}), Some("h1")),
            "screen_device#lock@h1"
        );
        assert_eq!(
            key("screen_device", json!({"action": "reboot"}), Some("h1")),
            "screen_device#other@h1"
        );
        assert_eq!(
            key("screen_clipboard", json!({"action": "read"}), Some("h1")),
            "screen_clipboard#read@h1"
        );
        assert_eq!(
            key(
                "screen_clipboard",
                json!({"action": "write", "text": "x"}),
                Some("h1")
            ),
            "screen_clipboard#write@h1"
        );
        // A clipboard call without an action is one the executor refuses, and shares no key with
        // a read or a write.
        assert_eq!(
            key("screen_clipboard", json!({}), Some("h1")),
            "screen_clipboard#other@h1"
        );
        assert_eq!(
            key("screen_clipboard", json!({"action": "erase"}), Some("h1")),
            "screen_clipboard#other@h1"
        );
        assert_eq!(
            key(
                "screen_package",
                json!({"action": "install", "path": "a.apk"}),
                Some("h1")
            ),
            "screen_package#install@h1"
        );
        assert_eq!(
            key("screen_package", json!({}), Some("h1")),
            "screen_package#other@h1"
        );
        // The tools that do one thing are keyed by the screen alone.
        for tool in [
            "preview_zoom",
            "preview_wait",
            "android_logcat",
            "android_shell",
        ] {
            assert_eq!(
                key(tool, json!({"action": "read"}), Some("h1")),
                format!("{tool}@h1")
            );
        }
        // Another screen, another key; the same call on the same screen, the same key.
        assert_ne!(
            key("screen_clipboard", json!({"action": "read"}), Some("h1")),
            key("screen_clipboard", json!({"action": "read"}), Some("h2"))
        );
        assert_ne!(
            key("screen_clipboard", json!({"action": "read"}), Some("h1")),
            key("screen_clipboard", json!({"action": "write"}), Some("h1"))
        );

        // No screen, no key: the bare name, which is never remembered nor looked up.
        for tool in SCREEN_TOOLS {
            let bare = key(tool, json!({"action": "read"}), None);
            assert_eq!(bare, tool);
            assert!(!allowance_is_keyed(&bare), "{tool}");
            assert!(allowance_is_keyed(&key(
                tool,
                json!({"action": "read"}),
                Some("h1")
            )));
        }
        // Every other tool keeps its plain name, a screen or not.
        for tool in [
            "write",
            "bash",
            "preview_dialog",
            "preview_input",
            "screen_",
        ] {
            for screen in [None, Some("h1")] {
                assert_eq!(key(tool, json!({"action": "read"}), screen), tool);
            }
            assert!(allowance_is_keyed(tool), "{tool}");
        }

        // The per-action table names screen tools only, and its defaults are among its actions.
        for (tool, actions, default) in PER_ACTION_ALLOWANCE_TOOLS {
            assert!(screen_tool_platforms(tool).is_some(), "{tool}");
            assert!(!actions.contains(&"other"), "{tool}");
            if let Some(default) = default {
                assert!(actions.contains(&default), "{tool}");
            }
        }
    }

    /// The per-action table is the tools' schemas, written twice: each entry's actions are the
    /// schema's `action` enum in the same order, and its default is the schema's default (none
    /// when the schema has none). An action added to a tool without the table would be keyed as
    /// `other`, and share one allowance with every unknown action; a default that drifted would
    /// key a call that names no action under the wrong one.
    #[test]
    fn the_per_action_allowance_table_is_the_tool_schemas_actions_and_default() {
        let profile = crate::prompt_profile::PromptProfile::builtin();
        for (tool, actions, default) in PER_ACTION_ALLOWANCE_TOOLS {
            let schema = crate::builtin_schemas::builtin_tool_schema(
                tool,
                crate::tool_surface::ToolVariant::Standard,
                &profile,
            )
            .unwrap_or_else(|| panic!("{tool} has no schema"));
            let action = &schema["properties"]["action"];
            let in_schema: Vec<&str> = action["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{tool}: action has no enum"))
                .iter()
                .map(|value| value.as_str().expect("an action is a string"))
                .collect();
            assert_eq!(
                in_schema, actions,
                "{tool}: the actions, in the schema's order"
            );
            assert_eq!(
                action.get("default").and_then(Value::as_str),
                default,
                "{tool}: the default"
            );
            // A default that is not a string would read as none above.
            assert!(
                action.get("default").is_none_or(Value::is_string),
                "{tool}: {action}"
            );
        }
        // Every screen tool with an `action` is in the table, so none is keyed as one thing.
        for tool in crate::tool_executor::screen_tools::SCREEN_TOOLS {
            let schema = crate::builtin_schemas::builtin_tool_schema(
                tool,
                crate::tool_surface::ToolVariant::Standard,
                &profile,
            )
            .unwrap_or_else(|| panic!("{tool} has no schema"));
            assert_eq!(
                schema["properties"].get("action").is_some(),
                PER_ACTION_ALLOWANCE_TOOLS
                    .iter()
                    .any(|(name, ..)| *name == tool),
                "{tool}"
            );
        }
    }

    /// One screen tool both looks and acts (a clipboard reads — a password too — and writes; an
    /// app tool lists and launches), at one risk level, on any of several screens. Its standing
    /// allowance holds for the screen and the action it was given for, in the conversation it
    /// was given in: an "always allow" said to one never answers another without a card.
    #[test]
    fn a_screen_tools_allowance_covers_only_the_screen_and_action_it_was_given_for() {
        let read = object(json!({"action": "read"}));
        let write = object(json!({"action": "write", "text": "x"}));
        let launch = object(json!({"action": "launch", "app": "com.foo"}));
        let stop = object(json!({"action": "stop", "app": "com.foo"}));
        let list = object(json!({}));
        let info = object(json!({}));
        let lock = object(json!({"action": "lock"}));
        let grant = object(json!({"action": "grant", "app": "com.foo", "permission": "p"}));
        let revoke = object(json!({"action": "revoke", "app": "com.foo", "permission": "p"}));
        for (tool, granted, other) in [
            ("screen_clipboard", &read, &write),
            ("screen_clipboard", &write, &read),
            ("screen_app", &launch, &stop),
            ("screen_app", &list, &launch),
            ("screen_device", &info, &lock),
            ("screen_package", &grant, &revoke),
        ] {
            let prompts = registry();
            let granted_key = allowance_key(tool, granted, Some("h1"));
            let other_action = allowance_key(tool, other, Some("h1"));
            let other_screen = allowance_key(tool, granted, Some("h2"));
            assert_ne!(granted_key, other_action, "{tool}");
            assert_ne!(granted_key, other_screen, "{tool}");

            grant_always(
                &prompts,
                "conversation-a",
                tool,
                &granted_key,
                RiskLevel::High,
            );
            assert!(prompts.is_always_allowed("conversation-a", &granted_key, RiskLevel::High));
            assert!(prompts.is_always_allowed("conversation-a", &granted_key, RiskLevel::Low));
            for key in [
                &other_action,
                &other_screen,
                &allowance_key(tool, granted, None),
            ] {
                assert!(
                    !prompts.is_always_allowed("conversation-a", key, RiskLevel::Low),
                    "{key}"
                );
            }
            assert!(!prompts.is_always_allowed("conversation-b", &granted_key, RiskLevel::Low));

            // The same action on the same screen is answered without a card…
            assert!(
                !raised_a_card(
                    &prompts,
                    "conversation-a",
                    tool,
                    &granted_key,
                    RiskLevel::High
                ),
                "{tool}"
            );
            // …another action on it, the same action on another screen, and the same call in
            // another conversation each raise their own card, which the user answers.
            assert!(raised_a_card(
                &prompts,
                "conversation-a",
                tool,
                &other_action,
                RiskLevel::High
            ));
            assert!(raised_a_card(
                &prompts,
                "conversation-a",
                tool,
                &other_screen,
                RiskLevel::High
            ));
            assert!(raised_a_card(
                &prompts,
                "conversation-b",
                tool,
                &granted_key,
                RiskLevel::High
            ));
        }
    }

    /// A screen tool call judged for no screen has no key an allowance could be kept under: its
    /// bare name answers nothing on the model's path, and an "always allow" said to its card is
    /// recorded for no screen at all.
    #[test]
    fn a_screen_tool_judged_for_no_screen_is_neither_remembered_nor_answered() {
        use crate::tool_executor::screen_tools::SCREEN_TOOLS;
        let input = object(json!({"action": "read"}));
        for tool in SCREEN_TOOLS {
            let bare = allowance_key(tool, &input, None);
            assert_eq!(bare, tool);
            let prompts = registry();
            // Said "always" with no screen to say it for…
            grant_always(&prompts, "conversation-a", tool, &bare, RiskLevel::High);
            // …it covers nothing: not the bare name, nor any screen's key.
            assert!(
                !prompts.is_always_allowed("conversation-a", &bare, RiskLevel::Low),
                "{tool}"
            );
            assert!(!prompts.is_always_allowed(
                "conversation-a",
                &allowance_key(tool, &input, Some("h1")),
                RiskLevel::Low
            ));
            assert!(raised_a_card(
                &prompts,
                "conversation-a",
                tool,
                &bare,
                RiskLevel::Low
            ));

            // Recording and looking up a bare name directly does nothing either.
            prompts.remember_allowance("conversation-a", &bare, RiskLevel::High);
            assert!(
                !prompts.is_always_allowed("conversation-a", &bare, RiskLevel::Low),
                "{tool}"
            );
        }
    }

    /// The manual path judges no target, so a screen tool it approves has no screen to keep an
    /// allowance for: "always allow" there records nothing for it, while an ordinary tool's is
    /// recorded as before.
    #[test]
    fn the_manual_path_keeps_no_allowance_for_a_screen_tool() {
        use crate::tool_executor::screen_tools::SCREEN_TOOLS;
        for tool in SCREEN_TOOLS {
            let input = object(json!({"action": "read"}));
            let manual = registry();
            let request = ToolExecutionRequest {
                input: input.clone(),
                ..manual_request("conversation-a", tool)
            };
            let id = manual.open_manual(&request, RiskLevel::High).unwrap();
            let resolution = manual
                .resolve(&id, ToolPromptDecision::AllowAlways, None)
                .unwrap();
            assert!(
                matches!(
                    resolution,
                    PromptResolution::Manual {
                        decision: ToolPromptDecision::AllowAlways,
                        ..
                    }
                ),
                "{tool}: the answer itself still reaches the call"
            );
            for key in [
                allowance_key(tool, &input, None),
                allowance_key(tool, &input, Some("h1")),
                tool.to_owned(),
            ] {
                assert!(
                    !manual.is_always_allowed("conversation-a", &key, RiskLevel::Low),
                    "{tool}: {key}"
                );
            }
        }

        // An ordinary tool is remembered under its name, as before.
        let manual = registry();
        let id = manual
            .open_manual(
                &manual_request("conversation-a", "write"),
                RiskLevel::Medium,
            )
            .unwrap();
        manual
            .resolve(&id, ToolPromptDecision::AllowAlways, None)
            .unwrap();
        assert!(manual.is_always_allowed("conversation-a", "write", RiskLevel::Medium));
        assert!(!manual.is_always_allowed("conversation-a", "write", RiskLevel::High));
    }

    /// A screen is named by the handle of its registry row, which a per-process counter mints, so
    /// the same `@1` is another screen after a restart. An allowance under such a key therefore
    /// answers within the process, is never written to the store, and is not read back from it
    /// either (an earlier build, or a hand-edited store, may hold such a row); an allowance that
    /// names no screen, `write` here, is saved and survives as before.
    #[test]
    fn a_screen_tools_allowance_dies_with_the_process() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            crate::conversation_store::store_for(&directory.path().join("document.v1.json"))
                .unwrap();
        let document = crate::catalog::default_document();
        let conversation = document.workspaces[0].conversations[0].clone();
        store
            .put_conversation(&document.workspaces[0].id, &conversation)
            .unwrap();
        let screen_keys = [
            "screen_clipboard#read@1",
            "preview_zoom@1",
            "android_shell@1",
        ];
        let first = registry();
        first.attach_allowance_store(store.clone());
        grant_always(
            &first,
            &conversation.id,
            "screen_clipboard",
            "screen_clipboard#read@1",
            RiskLevel::High,
        );
        grant_always(
            &first,
            &conversation.id,
            "preview_zoom",
            "preview_zoom@1",
            RiskLevel::High,
        );
        grant_always(
            &first,
            &conversation.id,
            "write",
            "write",
            RiskLevel::Medium,
        );
        // Within the process the screen allowance answers, under its screen and action only.
        assert!(first.is_always_allowed(
            &conversation.id,
            "screen_clipboard#read@1",
            RiskLevel::High
        ));
        assert!(first.is_always_allowed(&conversation.id, "preview_zoom@1", RiskLevel::High));
        assert!(!first.is_always_allowed(
            &conversation.id,
            "screen_clipboard#read@2",
            RiskLevel::Low
        ));
        assert!(!first.is_always_allowed(
            &conversation.id,
            "screen_clipboard#write@1",
            RiskLevel::Low
        ));
        // Nothing that names a screen reached the store; the plain tool's allowance did.
        let saved = store.tool_allowances(&conversation.id).unwrap();
        assert!(
            saved.iter().all(|(key, _)| !key.contains('@')),
            "a screen's allowance was saved: {saved:?}"
        );
        assert!(saved.iter().any(|(key, _)| key == "write"), "{saved:?}");

        // Rows an earlier build, or a hand-edited store, may hold: a screen allowance that was
        // saved, and a screen tool's bare name.
        for stale in [
            "screen_clipboard#read@1",
            "preview_zoom@1",
            "android_shell@1",
            "screen_clipboard",
            "screen_app",
        ] {
            store
                .put_tool_allowance(&conversation.id, stale, RiskLevel::High.slug())
                .unwrap();
        }
        let restarted = registry();
        restarted.attach_allowance_store(store);
        // After a restart nothing answers for any screen…
        for key in screen_keys.into_iter().chain([
            "screen_clipboard#write@1",
            "screen_clipboard#read@2",
            "screen_clipboard#read",
            "screen_clipboard",
            "screen_app",
            "screen_app#list@1",
        ]) {
            assert!(
                !restarted.is_always_allowed(&conversation.id, key, RiskLevel::Low),
                "{key}"
            );
        }
        assert!(raised_a_card(
            &restarted,
            &conversation.id,
            "screen_clipboard",
            "screen_clipboard#read@1",
            RiskLevel::Low
        ));
        // …while an allowance that names no screen still does.
        assert!(restarted.is_always_allowed(&conversation.id, "write", RiskLevel::Medium));
        assert!(!restarted.is_always_allowed(&conversation.id, "write", RiskLevel::High));
    }

    /// A key names a screen when it carries the `@<handle>` of one.
    #[test]
    fn an_allowance_key_names_a_screen_when_it_carries_a_handle() {
        use crate::tool_executor::screen_tools::SCREEN_TOOLS;
        for tool in SCREEN_TOOLS {
            let input = object(json!({"action": "read"}));
            assert!(allowance_names_a_screen(&allowance_key(
                tool,
                &input,
                Some("7")
            )));
            assert!(!allowance_names_a_screen(&allowance_key(tool, &input, None)));
        }
        for tool in ["read", "write", "preview_click", "preview_input", "bash"] {
            assert!(!allowance_names_a_screen(tool), "{tool}");
        }
    }

    /// A screen tool's card names the screen the call was judged for — its server id and the
    /// device's own name — in place of the `serverId` the call carries, for every screen tool and
    /// the shell; a blank name is none; the name is escaped like the rest of the card; and a tool
    /// that is no screen tool ignores it.
    #[test]
    fn a_screen_card_leads_with_the_screen_it_was_judged_for() {
        let en = ResolvedLanguage::EnUs;
        let on = |tool: &str, input: Value, screen: Option<&str>| {
            summarize_tool_input_on(tool, &object(input), en, screen)
        };
        let launch = json!({"serverId": "other", "action": "launch", "app": "com.foo"});
        assert_eq!(
            on("screen_app", launch.clone(), Some("pixel (Pixel 8)")),
            "pixel (Pixel 8): launch com.foo"
        );
        // A call judged for no screen is led by what it names, or by nothing.
        assert_eq!(
            on("screen_app", launch.clone(), None),
            "other: launch com.foo"
        );
        assert_eq!(
            summarize_tool_input("screen_app", &object(launch.clone()), en),
            "other: launch com.foo"
        );
        assert_eq!(
            on(
                "screen_app",
                json!({"action": "launch", "app": "com.foo"}),
                None
            ),
            "launch com.foo"
        );
        for blank in ["", "  "] {
            assert_eq!(
                on("screen_app", launch.clone(), Some(blank)),
                "other: launch com.foo",
                "{blank:?}"
            );
        }

        for (tool, input) in [
            ("android_shell", json!({"command": "getprop"})),
            ("android_logcat", json!({})),
            (
                "preview_zoom",
                json!({"x": 1, "y": 2, "width": 3, "height": 4}),
            ),
            ("preview_wait", json!({})),
            ("screen_app", json!({})),
            (
                "screen_app",
                json!({"action": "open_url", "url": "myapp://x", "app": "com.foo"}),
            ),
            (
                "screen_package",
                json!({"action": "install", "path": "a.apk"}),
            ),
            (
                "screen_package",
                json!({"action": "revoke", "app": "com.foo"}),
            ),
            ("screen_device", json!({})),
            ("screen_device", json!({"action": "lock"})),
            ("screen_clipboard", json!({"action": "read"})),
            (
                "preview_input",
                json!({"actions": [{"action": "click", "x": 1, "y": 2}]}),
            ),
            ("preview_dialog", json!({"button": "Allow"})),
        ] {
            let mut input = input;
            input["serverId"] = json!("other");
            let card = on(tool, input.clone(), Some("pixel (Pixel 8)"));
            assert!(card.starts_with("pixel (Pixel 8): "), "{tool}: {card}");
            assert!(!card.contains("other"), "{tool}: {card}");
            let named = on(tool, input, None);
            assert!(named.starts_with("other: "), "{tool}: {named}");
        }

        // Neither the shell nor an ordinary tool is led by a screen it does not act on.
        assert_eq!(
            on("write", json!({"path": "notes.md"}), Some("pixel")),
            "notes.md"
        );
        assert_eq!(on("bash", json!({"command": "ls"}), Some("pixel")), "ls");

        // The device's own name is its own text: escaped, on every kind of card.
        let spoofed = "pixel (\u{202e}Pixel)";
        for (tool, input) in [
            ("screen_app", json!({"action": "launch", "app": "com.foo"})),
            ("android_shell", json!({"command": "ls"})),
            (
                "screen_app",
                json!({"action": "open_url", "url": "myapp://x"}),
            ),
        ] {
            let card = on(tool, input, Some(spoofed));
            assert!(card.contains("\\u{202E}"), "{tool}: {card}");
            assert!(!card.contains('\u{202e}'), "{tool}: {card}");
        }
    }

    /// A screen tool's card says what will happen: the action and what it acts on, the whole
    /// shell line, the close-up's region and what a wait waits for.
    #[test]
    fn a_screen_tools_card_says_what_it_will_do_and_where() {
        let en = ResolvedLanguage::EnUs;
        let summary = |tool: &str, input: Value| summarize_tool_input(tool, &object(input), en);
        let shell = "pm list packages -3\nam force-stop com.example.app";
        assert_eq!(
            summary(
                "android_shell",
                json!({"serverId": "pixel", "command": shell})
            ),
            format!("pixel: {shell}")
        );
        let long_shell = format!("echo {}", "x".repeat(MAX_SUMMARY_CHARS * 2));
        assert_eq!(
            summary("android_shell", json!({"command": long_shell})),
            long_shell
        );
        assert_eq!(
            summary(
                "android_logcat",
                json!({"package": "com.foo", "buffer": "crash"})
            ),
            "logcat com.foo crash"
        );
        assert_eq!(
            summary(
                "android_logcat",
                json!({"serverId": "pixel", "tag": "OkHttp", "contains": "401"})
            ),
            "pixel: logcat OkHttp 401"
        );
        assert_eq!(summary("android_logcat", json!({})), "logcat");
        assert_eq!(
            summary(
                "preview_zoom",
                json!({"x": 100, "y": 200, "width": 300, "height": 150})
            ),
            "zoom 100,200 300x150"
        );
        assert_eq!(
            summary(
                "preview_zoom",
                json!({"serverId": "mac", "x": 0.5, "y": 2, "width": 30, "height": 15})
            ),
            "mac: zoom 0.5,2 30x15"
        );
        // A region that is not four numbers is not shown as one.
        assert_eq!(summary("preview_zoom", json!({})), "zoom");
        assert_eq!(
            summary(
                "preview_zoom",
                json!({"x": "1", "y": 2, "width": 3, "height": 4})
            ),
            "zoom"
        );
        assert_eq!(
            summary("preview_wait", json!({"until": "still"})),
            "wait until still"
        );
        assert_eq!(summary("preview_wait", json!({})), "wait until change");
        assert_eq!(
            summary(
                "preview_wait",
                json!({"serverId": "mac", "until": "change"})
            ),
            "mac: wait until change"
        );
    }

    /// `screen_app`: the action (the list by default) and the app it acts on, with the filter or
    /// scope of a list.
    #[test]
    fn a_screen_apps_card_says_the_action_and_the_app() {
        let en = ResolvedLanguage::EnUs;
        let phone = Some("pixel (Pixel 8)");
        let card = |input: Value| summarize_tool_input_on("screen_app", &object(input), en, phone);
        assert_eq!(card(json!({})), "pixel (Pixel 8): list");
        assert_eq!(card(json!({"action": "list"})), "pixel (Pixel 8): list");
        assert_eq!(
            card(json!({"action": "list", "filter": "chrome"})),
            "pixel (Pixel 8): list chrome"
        );
        assert_eq!(card(json!({"scope": "user"})), "pixel (Pixel 8): list user");
        assert_eq!(
            card(json!({"filter": "chrome", "scope": "user"})),
            "pixel (Pixel 8): list chrome"
        );
        assert_eq!(
            card(json!({"action": "foreground"})),
            "pixel (Pixel 8): foreground"
        );
        assert_eq!(
            card(json!({"action": "info", "app": "com.foo"})),
            "pixel (Pixel 8): info com.foo"
        );
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo", "activity": ".Main"})),
            "pixel (Pixel 8): launch com.foo/.Main"
        );
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo"})),
            "pixel (Pixel 8): launch com.foo"
        );
        assert_eq!(
            card(json!({"action": "activate", "app": "com.apple.Preferences"})),
            "pixel (Pixel 8): activate com.apple.Preferences"
        );
        assert_eq!(
            card(json!({"action": "stop", "app": "com.foo"})),
            "pixel (Pixel 8): stop com.foo"
        );
        // An `open_url` without a link has nothing to show whole.
        assert_eq!(
            card(json!({"action": "open_url", "app": "com.foo"})),
            "pixel (Pixel 8): open_url com.foo"
        );
        // An action the executor will refuse is shown as the model gave it.
        assert_eq!(
            card(json!({"action": "uninstall", "app": "com.foo"})),
            "pixel (Pixel 8): uninstall com.foo"
        );
    }

    /// `screen_package`: the action and the app, the permission a grant or revoke is about, and
    /// the APK an install streams.
    #[test]
    fn a_screen_packages_card_says_the_action_the_app_and_the_permission() {
        let en = ResolvedLanguage::EnUs;
        let phone = Some("pixel (Pixel 8)");
        let card =
            |input: Value| summarize_tool_input_on("screen_package", &object(input), en, phone);
        assert_eq!(
            card(json!({"action": "install", "path": "build/app-debug.apk"})),
            "pixel (Pixel 8): install build/app-debug.apk"
        );
        assert_eq!(
            card(json!({"action": "install"})),
            "pixel (Pixel 8): install"
        );
        assert_eq!(
            card(json!({"action": "uninstall", "app": "com.foo"})),
            "pixel (Pixel 8): uninstall com.foo"
        );
        assert_eq!(
            card(json!({"action": "clear_data", "app": "com.foo"})),
            "pixel (Pixel 8): clear_data com.foo"
        );
        for action in ["grant", "revoke"] {
            assert_eq!(
                card(json!({
                    "action": action,
                    "app": "com.foo",
                    "permission": "android.permission.CAMERA"
                })),
                format!("pixel (Pixel 8): {action} com.foo android.permission.CAMERA")
            );
        }
        assert_eq!(
            summarize_tool_input(
                "screen_package",
                &object(json!({"action": "revoke", "app": "com.foo", "permission": "p"})),
                en
            ),
            "revoke com.foo p"
        );
    }

    /// `screen_device`: its state with the sections asked for, what a setting is set to, and for
    /// `lock` a note, in the card's language, that a device with a passcode stays locked until
    /// its owner unlocks it — the model cannot.
    #[test]
    fn a_screen_devices_card_says_what_it_sets_and_warns_of_a_lock() {
        let en = ResolvedLanguage::EnUs;
        let zh = ResolvedLanguage::ZhCn;
        let phone = Some("pixel (Pixel 8)");
        let card =
            |input: Value| summarize_tool_input_on("screen_device", &object(input), en, phone);
        assert_eq!(card(json!({})), "pixel (Pixel 8): info");
        assert_eq!(card(json!({"action": "info"})), "pixel (Pixel 8): info");
        assert_eq!(
            card(json!({"sections": ["power", "notifications"]})),
            "pixel (Pixel 8): info power, notifications"
        );
        assert_eq!(card(json!({"sections": []})), "pixel (Pixel 8): info");
        assert_eq!(card(json!({"action": "wake"})), "pixel (Pixel 8): wake");
        assert_eq!(
            card(json!({"action": "rotate", "value": "landscape_left"})),
            "pixel (Pixel 8): rotate landscape_left"
        );
        assert_eq!(
            card(json!({"action": "appearance", "value": "dark"})),
            "pixel (Pixel 8): appearance dark"
        );
        assert_eq!(
            card(json!({"action": "location", "latitude": 31.2, "longitude": 121.5})),
            "pixel (Pixel 8): location 31.2, 121.5"
        );
        // Half a position is not shown as one.
        assert_eq!(
            card(json!({"action": "location", "latitude": 31.2})),
            "pixel (Pixel 8): location"
        );
        assert_eq!(
            card(json!({"action": "clear_location"})),
            "pixel (Pixel 8): clear_location"
        );
        assert_eq!(card(json!({"action": "reboot"})), "pixel (Pixel 8): reboot");

        // The lock note, in both of the app's languages, and only on a lock.
        assert_eq!(
            card(json!({"action": "lock"})),
            "pixel (Pixel 8): lock (if the device has a passcode, you will have to unlock it afterwards)"
        );
        assert_eq!(
            summarize_tool_input_on(
                "screen_device",
                &object(json!({"action": "lock"})),
                zh,
                phone
            ),
            "pixel (Pixel 8): lock （设备有锁屏密码时，之后需要你来解锁）"
        );
        assert_eq!(
            summarize_tool_input("screen_device", &object(json!({"action": "lock"})), zh),
            "lock （设备有锁屏密码时，之后需要你来解锁）"
        );
        for action in ["unlock", "wake", "rotate", "stay_awake"] {
            assert!(
                !card(json!({"action": action})).contains("passcode"),
                "{action}"
            );
        }
    }

    /// `screen_clipboard`: a read is a bare read; a write shows the text (the first 200
    /// characters) that replaces what the device holds, and a spoofing character in it is
    /// escaped.
    #[test]
    fn a_screen_clipboards_card_says_the_action_and_the_text_written() {
        let en = ResolvedLanguage::EnUs;
        let summary = |input: Value| {
            summarize_tool_input_on("screen_clipboard", &object(input), en, Some("mac"))
        };
        assert_eq!(summary(json!({"action": "read"})), "mac: read");
        assert_eq!(
            summary(json!({"action": "write", "text": "code 1234"})),
            "mac: write code 1234"
        );
        assert_eq!(summary(json!({"action": "write"})), "mac: write");
        let long = "长".repeat(300);
        assert_eq!(
            summary(json!({"action": "write", "text": long})),
            format!("mac: write {}…", "长".repeat(200))
        );
        let spoofed = summary(json!({"action": "write", "text": "safe\u{202e}evil"}));
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
        assert_eq!(
            summarize_tool_input(
                "screen_clipboard",
                &object(json!({"action": "write", "text": "hi"})),
                ResolvedLanguage::ZhCn
            ),
            "write hi"
        );
    }

    /// `preview_input`'s card lists its actions one after another; a batch with a `type` through
    /// the clipboard is led by the first paste: that it goes through the clipboard, the shortcut
    /// that pastes it, that the clipboard keeps the text, and the text (its first 100 characters)
    /// — in the card's language — and then the list, in which that paste shows briefly.
    #[test]
    fn a_preview_input_card_lists_its_actions_and_what_a_paste_leaves() {
        let en = ResolvedLanguage::EnUs;
        let zh = ResolvedLanguage::ZhCn;
        let phone = Some("pixel (Pixel 8)");
        let card =
            |input: Value| summarize_tool_input_on("preview_input", &object(input), en, phone);
        assert_eq!(
            card(json!({"actions": [
                {"action": "click", "x": 288, "y": 640},
                {"action": "type", "text": "hello"},
                {"action": "key", "key": "Enter"},
            ]})),
            "pixel (Pixel 8): click 288,640 · type \"hello\" · key Enter"
        );
        assert_eq!(
            card(json!({"actions": [
                {"action": "drag", "x": 1, "y": 2, "toX": 30, "toY": 40},
                {"action": "wait", "ms": 500},
            ]})),
            "pixel (Pixel 8): drag 1,2 → 30,40 · wait 500 ms"
        );
        // Typing through keys is cut at 80 characters; no shortcut is involved.
        assert_eq!(
            card(json!({"actions": [{"action": "type", "text": "a".repeat(100)}]})),
            format!("pixel (Pixel 8): type \"{}…\"", "a".repeat(80))
        );
        assert_eq!(
            card(json!({"actions": [{"action": "type", "text": "hi", "via": "keys"}]})),
            "pixel (Pixel 8): type \"hi\""
        );

        // A paste leads the card: it goes through the clipboard, what it pastes, and that the
        // text stays; the list follows, and shows the paste as `paste`.
        let paste = json!({"actions": [
            {"action": "type", "text": "你好，世界", "via": "paste", "chord": "cmd+v"}
        ]});
        assert_eq!(
            card(paste.clone()),
            "pixel (Pixel 8): paste via the clipboard (cmd+v; the clipboard keeps the text) \"你好，世界\" — paste"
        );
        assert_eq!(
            summarize_tool_input_on("preview_input", &object(paste), zh, phone),
            "pixel (Pixel 8): 经剪贴板粘贴（cmd+v；剪贴板之后保留这段文字）\"你好，世界\"——paste"
        );
        assert_eq!(
            card(json!({"actions": [
                {"action": "click", "x": 1, "y": 2},
                {"action": "type", "text": "hi", "via": "paste"},
            ]})),
            "pixel (Pixel 8): paste via the clipboard (the clipboard keeps the text) \"hi\" — click 1,2 · paste"
        );
        assert_eq!(
            summarize_tool_input(
                "preview_input",
                &object(json!({"serverId": "mac", "actions": [
                    {"action": "type", "text": "hi", "via": "paste", "chord": "ctrl+v"}
                ]})),
                en
            ),
            "mac: paste via the clipboard (ctrl+v; the clipboard keeps the text) \"hi\" — paste"
        );
        // Only the first 100 characters of a long text are shown, and the cut is marked.
        let long = summarize_tool_input(
            "preview_input",
            &object(json!({"actions": [
                {"action": "type", "text": "长".repeat(300), "via": "paste"}
            ]})),
            en,
        );
        assert!(
            long.starts_with(&format!(
                "paste via the clipboard (the clipboard keeps the text) \"{}…\" — paste",
                "长".repeat(100)
            )),
            "{long}"
        );
        // However long the text, and with a screen and a shortcut before it, the card keeps
        // the note: the cut to fit takes the end of the list.
        let long_on_a_screen = card(json!({"actions": [
            {"action": "type", "text": "a".repeat(300), "via": "paste", "chord": "cmd+v"}
        ]}));
        assert!(
            long_on_a_screen.starts_with(
                "pixel (Pixel 8): paste via the clipboard (cmd+v; the clipboard keeps the text) \"aaa"
            ),
            "{long_on_a_screen}"
        );
        let spoofed = card(json!({"actions": [
            {"action": "type", "text": "safe\u{202e}evil", "via": "paste"}
        ]}));
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
    }

    /// Three typed texts of 80 characters fill the card on their own: a paste after them used to
    /// be cut off the end, notice and all. The paste leads the card now, so the cut takes the
    /// tail of the list instead; a later paste is listed with its own text, not only the first.
    #[test]
    fn a_paste_after_long_typed_text_is_not_cut_off_the_card() {
        let en = ResolvedLanguage::EnUs;
        let zh = ResolvedLanguage::ZhCn;
        let phone = Some("pixel (Pixel 8)");
        let long = |character: &str| character.repeat(80);
        let input = json!({"actions": [
            {"action": "type", "text": long("a")},
            {"action": "type", "text": long("b")},
            {"action": "type", "text": long("c")},
            {"action": "type", "text": "secret", "via": "paste", "chord": "cmd+v"},
        ]});
        for (language, lead) in [
            (
                en,
                "pixel (Pixel 8): paste via the clipboard (cmd+v; the clipboard keeps the text) \"secret\" — ",
            ),
            (
                zh,
                "pixel (Pixel 8): 经剪贴板粘贴（cmd+v；剪贴板之后保留这段文字）\"secret\"——",
            ),
        ] {
            let card =
                summarize_tool_input_on("preview_input", &object(input.clone()), language, phone);
            assert!(card.starts_with(lead), "{card}");
            // The list follows the notice and is what is cut: the card is bounded, the typed
            // text is cut part way, and the notice is whole.
            assert!(card.ends_with('…'), "{card}");
            assert!(card.chars().count() <= MAX_SUMMARY_CHARS + 1, "{card}");
            assert!(card.contains(&format!("type \"{}", long("a"))), "{card}");
            assert!(!card.contains(&long("c")), "{card}");
        }
        // Without the lead the paste is the tail of the list, and is what the old card lost.
        let without_the_lead = summarize_tool_input_on(
            "preview_input",
            &object(json!({"actions": [
                {"action": "type", "text": long("a")},
                {"action": "type", "text": long("b")},
                {"action": "type", "text": long("c")},
                {"action": "type", "text": "secret"},
            ]})),
            en,
            phone,
        );
        assert!(!without_the_lead.contains("secret"), "{without_the_lead}");

        // Two pastes: the first leads, and shows as `paste`; the second is listed with its text.
        let two = summarize_tool_input_on(
            "preview_input",
            &object(json!({"actions": [
                {"action": "type", "text": "first", "via": "paste"},
                {"action": "key", "key": "Enter"},
                {"action": "type", "text": "second", "via": "paste", "chord": "ctrl+v"},
            ]})),
            en,
            phone,
        );
        assert_eq!(
            two,
            "pixel (Pixel 8): paste via the clipboard (the clipboard keeps the text) \"first\" — paste · key Enter · paste \"second\""
        );
    }

    /// A launch's card says when it restarts the app (ending the running one) and how many launch
    /// arguments and environment variables it passes in: they change what the app does, and the
    /// card is what the user approves.
    #[test]
    fn a_launch_card_says_when_it_restarts_the_app_and_what_it_passes_in() {
        let en = ResolvedLanguage::EnUs;
        let zh = ResolvedLanguage::ZhCn;
        let phone = Some("iphone (iPhone 15)");
        let card = |input: Value| {
            summarize_tool_input_on("screen_app", &object(input), en, phone)
        };
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo"})),
            "iphone (iPhone 15): launch com.foo"
        );
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo", "restart": true})),
            "iphone (iPhone 15): launch com.foo (restart)"
        );
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo", "restart": false})),
            "iphone (iPhone 15): launch com.foo"
        );
        assert_eq!(
            card(json!({
                "action": "launch",
                "app": "com.foo",
                "arguments": ["-a", "-b"],
                "environment": {"A": "1", "B": "2", "C": "3"}
            })),
            "iphone (iPhone 15): launch com.foo +2 arguments +3 environment variables"
        );
        assert_eq!(
            card(json!({
                "action": "launch",
                "app": "com.foo",
                "restart": true,
                "arguments": ["-a"],
                "environment": {"A": "1"}
            })),
            "iphone (iPhone 15): launch com.foo (restart) +1 argument +1 environment variable"
        );
        // Nothing given is nothing said; the card is the same in either language.
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo", "arguments": [], "environment": {}})),
            "iphone (iPhone 15): launch com.foo"
        );
        assert_eq!(
            summarize_tool_input_on(
                "screen_app",
                &object(json!({"action": "activate", "app": "com.foo", "restart": true})),
                zh,
                phone
            ),
            "iphone (iPhone 15): activate com.foo (restart)"
        );
        // An Android launch names its activity as before.
        assert_eq!(
            card(json!({"action": "launch", "app": "com.foo", "activity": ".Main"})),
            "iphone (iPhone 15): launch com.foo/.Main"
        );
    }

    /// `preview_dialog`'s card says how the dialog is answered — the button pressed, accepting or
    /// dismissing it — and the text typed into its field; on a phone the card names the phone.
    #[test]
    fn a_dialog_card_says_how_the_dialog_is_answered() {
        let en = ResolvedLanguage::EnUs;
        let card = |input: Value, screen: Option<&str>| {
            summarize_tool_input_on("preview_dialog", &object(input), en, screen)
        };
        let phone = Some("iphone (iPhone 15)");
        assert_eq!(
            card(json!({"button": "Allow"}), phone),
            "iphone (iPhone 15): press \"Allow\""
        );
        assert_eq!(
            card(json!({"accept": true}), phone),
            "iphone (iPhone 15): accept"
        );
        assert_eq!(
            card(json!({"accept": false}), phone),
            "iphone (iPhone 15): dismiss"
        );
        assert_eq!(
            card(json!({"prompt_text": "hello"}), phone),
            "iphone (iPhone 15): type \"hello\""
        );
        assert_eq!(
            card(json!({"accept": true, "prompt_text": "hello"}), phone),
            "iphone (iPhone 15): accept \"hello\""
        );
        assert_eq!(
            card(json!({"button": "OK", "prompt_text": "x"}), phone),
            "iphone (iPhone 15): press \"OK\" \"x\""
        );
        // A named button wins over accept; accepting is what the page does with no answer.
        assert_eq!(
            card(json!({"button": "Cancel", "accept": true}), phone),
            "iphone (iPhone 15): press \"Cancel\""
        );
        assert_eq!(card(json!({}), None), "accept");
        assert_eq!(
            card(json!({"accept": true, "prompt_text": ""}), None),
            "accept"
        );
        // Named by its `serverId` when the call was not judged for a screen.
        assert_eq!(
            card(json!({"serverId": "iphone", "button": "Allow"}), None),
            "iphone: press \"Allow\""
        );
        // Only the first 200 characters of the text typed are shown.
        assert_eq!(
            card(
                json!({"accept": true, "prompt_text": "x".repeat(300)}),
                None
            ),
            format!("accept \"{}…\"", "x".repeat(200))
        );
        let spoofed = card(json!({"button": "A\u{202e}B"}), phone);
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
    }

    /// A link can lead anywhere on the device and an APK's name ends its path, so the card shows
    /// the whole URL `open_url` opens — after the app that opens it — and the whole path
    /// `install` streams, however long; the rest of the call stays bounded, and a spoofing
    /// character is still escaped.
    #[test]
    fn a_screen_cards_link_and_apk_path_are_shown_whole() {
        let en = ResolvedLanguage::EnUs;
        let phone = Some("pixel (Pixel 8)");
        let summary =
            |tool: &str, input: Value| summarize_tool_input_on(tool, &object(input), en, phone);
        let url = format!("myapp://orders/{}&do=delete", "7".repeat(300));
        assert!(url.chars().count() > MAX_SUMMARY_CHARS);
        assert_eq!(
            summary(
                "screen_app",
                json!({"action": "open_url", "url": url, "app": "com.bank"})
            ),
            format!("pixel (Pixel 8): open_url com.bank {url}")
        );
        assert_eq!(
            summary("screen_app", json!({"action": "open_url", "url": url})),
            format!("pixel (Pixel 8): open_url {url}")
        );
        // Judged for no screen, the call's own `serverId` leads, as before; with neither, no one.
        assert_eq!(
            summarize_tool_input(
                "screen_app",
                &object(json!({
                    "serverId": "iphone",
                    "action": "open_url",
                    "url": url,
                    "app": "com.apple.mobilesafari"
                })),
                en
            ),
            format!("iphone: open_url com.apple.mobilesafari {url}")
        );
        assert_eq!(
            summarize_tool_input(
                "screen_app",
                &object(json!({"action": "open_url", "url": url})),
                en
            ),
            format!("open_url {url}")
        );
        let path = format!("{}/release/app-release.apk", "build/".repeat(60));
        assert!(path.chars().count() > MAX_SUMMARY_CHARS);
        assert_eq!(
            summary("screen_package", json!({"action": "install", "path": path})),
            format!("pixel (Pixel 8): install {path}")
        );
        assert_eq!(
            summarize_tool_input(
                "screen_package",
                &object(json!({"action": "install", "path": path})),
                en
            ),
            format!("install {path}")
        );
        // What comes before the whole argument is still cut to fit.
        let app = format!("com.{}", "a".repeat(400));
        let long_head = summary(
            "screen_app",
            json!({"action": "open_url", "url": "myapp://x", "app": app}),
        );
        assert!(
            long_head.starts_with("pixel (Pixel 8): open_url com.aaa"),
            "{long_head}"
        );
        assert!(long_head.ends_with("… myapp://x"), "{long_head}");
        assert_eq!(
            long_head.chars().count(),
            MAX_SUMMARY_CHARS + "… myapp://x".chars().count()
        );
        let spoofed = summary(
            "screen_app",
            json!({"action": "open_url", "url": "https://bank.example/\u{202e}lmth.exe"}),
        );
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
        let spoofed = summary(
            "screen_package",
            json!({"action": "install", "path": "build/\u{202e}kpa.exe"}),
        );
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
        // Other actions of the same tools are bounded as before.
        let launched = summary(
            "screen_app",
            json!({"action": "launch", "app": format!("com.{}", "b".repeat(400))}),
        );
        assert_eq!(launched.chars().count(), MAX_SUMMARY_CHARS + 1);
    }

    /// A `mandatory_prompt` decision neither consumes nor records an allowance:
    /// global memory mutation asks every time, even in a conversation that
    /// blanket-allowed the same tool name.
    #[test]
    fn a_mandatory_prompt_ignores_allowances_in_both_directions() {
        let registry = registry();
        registry.remember_allowance("conversation-a", "create_global_memory", RiskLevel::High);
        let (announce, responder) = answer_next_prompt(&registry, ToolPromptDecision::AllowAlways);

        let mut announced_once = false;
        let allowed = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                true,
                RiskLevel::High,
                &[],
                card("create_global_memory"),
                |prompt| {
                    announced_once = true;
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap();
        responder.join().unwrap();
        assert!(announced_once, "强制确认必须仍然弹卡片");
        assert!(allowed);

        // And an "always" answer to a mandatory prompt is spent, not stored:
        // once the pre-seeded allowance is cleared, nothing rewrote it.
        registry.clear();
        assert!(!registry.is_always_allowed(
            "conversation-a",
            "create_global_memory",
            RiskLevel::High
        ));
    }

    /// A stopped run must release its worker instead of holding it until the
    /// prompt timeout — and the release is an error, not a denial. A denial
    /// must never be reported for a card the user never saw.
    #[test]
    fn cancellation_releases_the_waiter_without_claiming_the_user_refused() {
        let registry = registry();
        let cancellation = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancellation);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            flag.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let error = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[cancellation.as_ref()],
                card("write"),
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.contains("was stopped"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// A task worker watches two flags at once. Stopping the task alone — the
    /// sidebar stop button, with no run in sight — must release the card, or a
    /// background step would sit behind an unanswered prompt for half an hour.
    #[test]
    fn a_task_stop_flag_releases_a_card_raised_with_no_run() {
        let registry = registry();
        let task_stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&task_stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            flag.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let error = registry
            .ask(
                "conversation-a",
                PromptOwner::Task,
                false,
                RiskLevel::Medium,
                &[task_stop.as_ref()],
                card("write"),
                |_| Ok(()),
            )
            .unwrap_err();
        assert!(error.contains("was stopped"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Cancellation wins when it races with Allow. The waiter is already in a
    /// receive slice when cancellation is set and the renderer sends Allow, so
    /// the post-answer cancellation check must reject execution.
    #[test]
    fn allow_arriving_after_cancellation_is_refused() {
        let registry = registry();
        let cancellation = Arc::new(AtomicBool::new(false));
        let (announce, announced) = sync_channel(1);
        let asker = Arc::clone(&registry);
        let asker_cancellation = Arc::clone(&cancellation);
        let run = thread::spawn(move || {
            asker.ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[asker_cancellation.as_ref()],
                card("write"),
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
        });
        let prompt_id = announced.recv().unwrap();
        // Let the waiter enter a recv_timeout slice after its initial flag check.
        thread::sleep(Duration::from_millis(40));
        cancellation.store(true, Ordering::Release);
        registry
            .resolve(&prompt_id, ToolPromptDecision::AllowOnce, None)
            .unwrap();
        let outcome = run.join().unwrap();
        assert!(
            outcome.is_err(),
            "an Allow racing a cancelled run must not pass: {outcome:?}"
        );
    }

    /// Ending a run retracts the cards **that run** raised and nothing else.
    ///
    /// The three bystanders are the whole point: the renderer's own tool-editor
    /// prompt, a concurrent run in another conversation, and — the one this
    /// scoping was introduced for — a detached task's card in the *same*
    /// conversation. A background workflow step outlives the turn, so a turn
    /// boundary answering for it is a fabricated rejection.
    #[test]
    fn ending_a_run_retracts_only_the_cards_that_run_raised() {
        let registry = registry();
        let manual = registry
            .open_manual(&manual_request("conversation-a", "read"), RiskLevel::Low)
            .unwrap();

        // A second conversation is mid-prompt the whole time.
        let bystander = Arc::clone(&registry);
        let (bystander_announce, bystander_announced) = sync_channel(1);
        let bystander_run = thread::spawn(move || {
            bystander
                .ask(
                    "conversation-b",
                    PromptOwner::Run("run-2".into()),
                    false,
                    RiskLevel::Medium,
                    &[],
                    card("write"),
                    |prompt| {
                        bystander_announce.send(prompt.prompt_id.clone()).unwrap();
                        Ok(())
                    },
                )
                .unwrap()
        });
        let bystander_id: String = bystander_announced.recv().unwrap();

        // A background task in the SAME conversation as the run that is about
        // to end. Its card must survive.
        let task = Arc::clone(&registry);
        let (task_announce, task_announced) = sync_channel(1);
        let task_run = thread::spawn(move || {
            task.ask(
                "conversation-a",
                PromptOwner::Task,
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |prompt| {
                    task_announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
            .unwrap()
        });
        let task_id: String = task_announced.recv().unwrap();

        let cancelling = Arc::clone(&registry);
        let (announce, announced) = sync_channel(1);
        let canceller = thread::spawn(move || {
            let _: String = announced.recv().unwrap();
            thread::sleep(Duration::from_millis(30));
            cancelling.cancel_run_prompts("conversation-a", "run-1");
        });
        let outcome = registry.ask(
            "conversation-a",
            PromptOwner::Run("run-1".into()),
            false,
            RiskLevel::Medium,
            &[],
            card("write"),
            |prompt| {
                announce.send(prompt.prompt_id.clone()).unwrap();
                Ok(())
            },
        );
        canceller.join().unwrap();
        assert!(
            outcome.is_err(),
            "撤回的确认不是「用户拒绝」，必须报错：{outcome:?}"
        );

        // The task's card in the same conversation is untouched and still
        // answerable — this is the regression the scoping exists for.
        assert!(registry
            .resolve(&task_id, ToolPromptDecision::AllowOnce, None)
            .is_ok());
        assert!(task_run.join().unwrap());

        // The other conversation's card is still answerable and still blocking.
        assert!(registry
            .resolve(&bystander_id, ToolPromptDecision::AllowOnce, None)
            .is_ok());
        assert!(bystander_run.join().unwrap());

        // The manual prompt survived and still carries its classified request.
        match registry
            .resolve(&manual, ToolPromptDecision::AllowOnce, None)
            .unwrap()
        {
            PromptResolution::Manual { request, decision } => {
                assert_eq!(request.tool_name, "read");
                assert_eq!(decision, ToolPromptDecision::AllowOnce);
            }
            PromptResolution::Model => panic!("manual prompt resolved as a model prompt"),
        }
    }

    /// A renderer that reloaded has to be able to get the open cards back.
    /// Without this a task's card — which no longer dies with the turn — would
    /// be invisible until the 30-minute timeout.
    #[test]
    fn pending_cards_are_listable_for_a_renderer_that_reloaded() {
        let registry = registry();
        let asker = Arc::clone(&registry);
        let (announce, announced) = sync_channel(1);
        let waiting = thread::spawn(move || {
            asker.ask(
                "conversation-a",
                PromptOwner::Task,
                false,
                RiskLevel::Medium,
                &[],
                PendingToolPrompt {
                    requester: Some("ws1".into()),
                    ..card("write")
                },
                |prompt| {
                    announce.send(prompt.prompt_id.clone()).unwrap();
                    Ok(())
                },
            )
        });
        let prompt_id: String = announced.recv().unwrap();

        let listed = registry.all_pending_cards();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "conversation-a");
        assert_eq!(listed[0].1.prompt_id, prompt_id);
        assert_eq!(listed[0].1.tool_name, "write");
        assert_eq!(listed[0].1.requester.as_deref(), Some("ws1"));

        // Manual prompts are the renderer's own business and never listed here.
        registry
            .open_manual(&manual_request("conversation-a", "read"), RiskLevel::Low)
            .unwrap();
        assert_eq!(registry.all_pending_cards().len(), 1);

        registry
            .resolve(&prompt_id, ToolPromptDecision::AllowOnce, None)
            .unwrap();
        assert!(waiting.join().unwrap().unwrap());
        assert!(registry.all_pending_cards().is_empty());
    }

    /// A prompt that cannot be shown is an error, not a silent allow, and it
    /// must not leave a registration behind.
    #[test]
    fn an_unannounceable_prompt_fails_closed_and_leaves_nothing_pending() {
        let registry = registry();
        let error = registry
            .ask(
                "conversation-a",
                PromptOwner::Run("run-1".into()),
                false,
                RiskLevel::Medium,
                &[],
                card("write"),
                |_| Err("通道已关闭".into()),
            )
            .unwrap_err();
        assert!(error.contains("通道已关闭"), "{error}");
        assert!(registry
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty());
    }

    /// Resolving twice, or resolving an id that was never opened, is an error.
    /// A stale card must not be able to answer whatever prompt is open next.
    #[test]
    fn a_prompt_id_is_single_use() {
        let registry = registry();
        assert!(registry
            .resolve("never-opened", ToolPromptDecision::AllowOnce, None)
            .is_err());

        let id = registry
            .open_manual(&manual_request("conversation-a", "read"), RiskLevel::Low)
            .unwrap();
        assert!(registry
            .resolve(&id, ToolPromptDecision::AllowOnce, None)
            .is_ok());
        assert!(registry
            .resolve(&id, ToolPromptDecision::AllowOnce, None)
            .is_err());
    }

    /// The renderer may not swap the arguments between being asked about a
    /// call and answering: the request it is granted is the one that was
    /// classified, because the renderer never resends it.
    #[test]
    fn a_manual_grant_returns_the_request_that_was_classified() {
        let registry = registry();
        let mut original = manual_request("conversation-a", "write");
        original.input = object(json!({"path": "notes.md", "content": "original"}));
        let id = registry.open_manual(&original, RiskLevel::Medium).unwrap();

        match registry
            .resolve(&id, ToolPromptDecision::AllowOnce, None)
            .unwrap()
        {
            PromptResolution::Manual { request, .. } => assert_eq!(*request, original),
            PromptResolution::Model => panic!("manual prompt resolved as a model prompt"),
        }
    }

    /// The card describes the call in the caller's own terms rather than
    /// dumping JSON, and it never trusts the argument text to render safely.
    #[test]
    fn a_summary_names_the_operation_and_neutralizes_display_spoofing() {
        let zh = ResolvedLanguage::ZhCn;
        assert_eq!(
            summarize_tool_input("bash", &object(json!({"command": "rm -rf build"})), zh),
            "rm -rf build"
        );
        assert_eq!(
            summarize_tool_input(
                "write",
                &object(json!({"path": "src/main.rs", "content": "x"})),
                zh
            ),
            "src/main.rs"
        );
        assert_eq!(
            summarize_tool_input(
                "workflow",
                &object(json!({"scriptName": "audit", "scriptBytes": 91})),
                zh
            ),
            "audit"
        );
        // Unknown tools still say something: the longest string argument.
        assert_eq!(
            summarize_tool_input(
                "mcp__notes__append",
                &object(json!({"id": "n1", "body": "a longer body"})),
                zh
            ),
            "a longer body"
        );

        // A right-to-left override could otherwise make a displayed command
        // read as the reverse of what runs.
        let spoofed = summarize_tool_input(
            "bash",
            &object(json!({"command": "echo safe\u{202e}dangerous"})),
            zh,
        );
        assert!(spoofed.contains("\\u{202E}"), "{spoofed}");
        assert!(!spoofed.contains('\u{202e}'), "{spoofed}");
    }

    /// A pasted file has to be cut down to one bounded line beside the card's
    /// reason; a command is what is being approved, so it is shown whole.
    #[test]
    fn a_long_summary_is_cut_to_one_line_but_a_command_is_shown_whole() {
        let long = "x".repeat(MAX_SUMMARY_CHARS * 3);
        let summary = summarize_tool_input(
            "write",
            &object(json!({"path": long, "content": "x"})),
            ResolvedLanguage::ZhCn,
        );
        assert_eq!(summary.chars().count(), MAX_SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
        let flattened = summarize_tool_input(
            "mcp__notes__append",
            &object(json!({"body": "first\n  second\n\tthird"})),
            ResolvedLanguage::ZhCn,
        );
        assert_eq!(flattened, "first second third");

        let command = format!("cat <<'EOF' > a.txt\n{long}\nEOF");
        assert_eq!(
            summarize_tool_input("bash", &object(json!({"command": command})), ResolvedLanguage::ZhCn),
            command
        );
    }

    /// zsh and sh are shells like bash: the card shows their command, not the
    /// model's description of it, which is the longest string argument.
    #[test]
    fn every_shell_shows_its_command_and_its_background_marker() {
        for tool in ["bash", "zsh", "sh", "powershell"] {
            let input = object(json!({
                "command": "ls -la",
                "description": "List files in current directory",
                "run_in_background": true
            }));
            assert_eq!(
                summarize_tool_input(tool, &input, ResolvedLanguage::EnUs),
                "[Background] ls -la",
                "{tool}"
            );
        }
    }

    /// Fetching pages names every page it will read.
    #[test]
    fn a_fetch_lists_every_url() {
        let input = object(json!({"urls": ["https://example.com/a", " https://example.org/b "]}));
        assert_eq!(
            summarize_tool_input("web_fetch", &input, ResolvedLanguage::ZhCn),
            "https://example.com/a\nhttps://example.org/b"
        );
    }

    /// The background marker is part of the card, so it is worded in the
    /// card's language like the rest of it.
    #[test]
    fn a_background_command_is_marked_in_the_card_language() {
        let input = object(json!({"command": "npm run dev", "run_in_background": true}));
        assert_eq!(
            summarize_tool_input("bash", &input, ResolvedLanguage::ZhCn),
            "[后台] npm run dev"
        );
        assert_eq!(
            summarize_tool_input("powershell", &input, ResolvedLanguage::EnUs),
            "[Background] npm run dev"
        );
    }

    /// The card names a tool in the app's language whatever language the
    /// prompt profile minted its descriptor in; the profile only changes what
    /// the model reads.
    #[test]
    fn a_card_names_the_tool_in_the_card_language_not_the_profile_language() {
        for profile_language in [ResolvedLanguage::ZhCn, ResolvedLanguage::EnUs] {
            let catalog = crate::catalog::tool_catalog_for_language(profile_language);
            let read = catalog.iter().find(|tool| tool.name == "read").unwrap();
            assert_eq!(card_label(read, ResolvedLanguage::ZhCn), "读取文件");
            assert_eq!(card_label(read, ResolvedLanguage::EnUs), "Read file");

            let global = catalog
                .iter()
                .find(|tool| tool.name == "create_global_memory")
                .unwrap();
            assert_eq!(
                card_label(global, ResolvedLanguage::EnUs),
                "Create global memory (shared across projects)"
            );
            assert_eq!(
                card_label(global, ResolvedLanguage::ZhCn),
                "创建全局记忆（跨项目共享）"
            );
        }

        // A tool the catalog does not know keeps its own label, and a note
        // goes on the line below the name.
        let mut mcp = crate::catalog::tool_catalog()
            .into_iter()
            .find(|tool| tool.name == "read")
            .unwrap();
        mcp.name = "mcp__notes__append".into();
        mcp.label = "Append note".into();
        mcp.approval_note = Some("Hook guard: check first".into());
        assert_eq!(
            card_label(&mcp, ResolvedLanguage::ZhCn),
            "Append note\nHook guard: check first"
        );
    }

    /// The user's rule, in one assertion: a shell call is never blanket-allowed.
    #[test]
    fn both_shell_tools_are_excluded_from_blanket_allowance() {
        assert!(is_shell_tool("bash"));
        assert!(is_shell_tool("powershell"));
        assert!(!is_shell_tool("write"));
        assert!(!is_shell_tool("preview_click"));
    }

    /// Prompt capacity is per conversation: a saturated conversation blocks
    /// only its own cards, not a first approval in another conversation.
    #[test]
    fn one_conversations_full_prompt_quota_does_not_block_another_conversation() {
        let registry = registry();
        for _ in 0..MAX_PENDING_PROMPTS_PER_CONVERSATION {
            registry
                .open_manual(
                    &manual_request("conversation-noisy", "write"),
                    RiskLevel::Medium,
                )
                .expect("额度内的卡照常立起");
        }
        assert!(
            registry
                .open_manual(
                    &manual_request("conversation-noisy", "write"),
                    RiskLevel::Medium
                )
                .is_err(),
            "洪峰会话自己的额度已满"
        );
        assert!(
            registry
                .open_manual(
                    &manual_request("conversation-quiet", "write"),
                    RiskLevel::Medium
                )
                .is_ok(),
            "另一个会话的第一张卡不受洪峰会话额度的牵连"
        );
    }
}
