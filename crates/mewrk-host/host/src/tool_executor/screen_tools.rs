//! The screen tools: what a remote-control screen can do beyond the page tools —
//! `preview_zoom` and `preview_wait` on any screen, `screen_app`, `screen_device`,
//! `screen_clipboard` and `screen_package` on the kinds of screen each reaches, and
//! `android_logcat` / `android_shell` on an Android device (`docs/remote-control-unified-tools.md`).
//!
//! Every one of them is a bounded, synchronous call on one screen's session:
//!
//! 1. its screen is resolved ([`preview_target::resolve_screen_target`]: a screen of a kind the
//!    tool reaches, [`screen_tool_platforms`], never the page) and held to what the call was
//!    judged for when it was approved ([`preview_target::target_at_dispatch`]), exactly as for
//!    `preview_input`;
//! 2. its arguments are parsed — by the module of that screen's platform
//!    (`remote_control::{android, ios, desktop}`), or here for the two the session does itself
//!    (`preview_zoom`, `preview_wait`) — before anything reaches the screen;
//! 3. it runs on its own thread under the call's deadline and the host's wall clock
//!    ([`preview_target::on_screen_within`]), through the session's
//!    [`RemoteControlSession::operate_within`] — which takes the input lock, refuses a read-only
//!    entry, or neither, by the call's [`Effect`].
//!
//! `preview_dialog` on a phone or tablet ([`run_dialog_on_screen`]) is the page tool's screen
//! side, run the same way.
//!
//! Nothing here, nor in what the platforms answer, ever carries the viewer page's address.

use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::Utc;
use serde_json::{json, Value};

use super::{
    preview_target::{
        on_screen_within, target_at_dispatch, PreviewTarget, RemoteScreen, SCREEN_CALL_DEADLINE,
    },
    Outcome, PREVIEW_TOOL_TIMEOUT,
};
use crate::{
    cancel::CancelSignal,
    file_sandbox::FileSandbox,
    image_attachments::ImageAttachmentStore,
    model::{JsonObject, ToolExecutionRequest},
    path_guard::{secure_open_existing_file_with_scope, ExecutionScope},
    remote_control::{
        android::AndroidCall,
        desktop::DesktopCall,
        driver::{Driver, Platform},
        ios::IosCall,
        session::{Effect, FrameRegion, OpError, RemoteControlSession, Step, WaitUntil, ZoomShot},
    },
    state::AppState,
    workspace_set::ResolvedWorkspace,
};

/// Every screen tool, in the catalog's order.
pub(crate) const SCREEN_TOOLS: [&str; 8] = [
    "preview_zoom",
    "preview_wait",
    "screen_app",
    "screen_device",
    "screen_clipboard",
    "screen_package",
    "android_logcat",
    "android_shell",
];

/// The kinds of screen each screen tool reaches, in [`SCREEN_TOOLS`]' order. These are exactly the
/// platforms where it is implemented: a screen of any other kind is not one the tool can find, so a
/// call never resolves to a screen that could only refuse it — and never after it was approved.
const SCREEN_TOOL_PLATFORMS: [(&str, &[Platform]); 8] = [
    ("preview_zoom", &[Platform::Android, Platform::Ios, Platform::Desktop]),
    ("preview_wait", &[Platform::Android, Platform::Ios, Platform::Desktop]),
    ("screen_app", &[Platform::Android, Platform::Ios]),
    ("screen_device", &[Platform::Android, Platform::Ios]),
    ("screen_clipboard", &[Platform::Ios, Platform::Desktop]),
    ("screen_package", &[Platform::Android]),
    ("android_logcat", &[Platform::Android]),
    ("android_shell", &[Platform::Android]),
];

/// The kinds of screen `preview_dialog` answers a dialog on, besides the page.
pub(crate) const DIALOG_SCREEN_PLATFORMS: &[Platform] = &[Platform::Android, Platform::Ios];

/// The parameters `preview_dialog` takes on a screen, as its schema states them.
const DIALOG_PARAMETERS: &[&str] = &["serverId", "accept", "prompt_text", "button"];

/// What `preview_dialog` answers on a phone or tablet when the call says nothing to answer with:
/// there, unlike on the page, accepting is never the default.
pub(super) const DIALOG_NEEDS_AN_ANSWER: &str = "On a phone or tablet preview_dialog has no default answer: give button (the exact label of the button to press) or accept (true or false). prompt_text alone only types into the alert's field.";

/// The time a screen tool's call gets unless it says otherwise: its deadline inside the session
/// ([`SCREEN_CALL_DEADLINE`]) and the host's wall clock around it ([`PREVIEW_TOOL_TIMEOUT`]).
pub(crate) const SCREEN_CALL_TIME: (Duration, Duration) =
    (SCREEN_CALL_DEADLINE, PREVIEW_TOOL_TIMEOUT);

/// The coordinate bounds of `preview_zoom` and `preview_wait`, as their schemas state them.
const MAX_COORDINATE: f64 = 100_000.0;
/// `preview_wait`'s `timeout`, in milliseconds: its bounds and its default.
const WAIT_TIMEOUT_MS: (u64, u64, u64) = (100, 20_000, 5_000);
/// What a close-up says beside its picture about where its pixels are.
const ZOOM_NOTE: &str = "close-up only: x/y for preview_input stay in the full screenshot's frame (frame x = region.x + image x × region.width / image.width)";

/// The kinds of screen `name` reaches, when it is a screen tool.
pub(crate) fn screen_tool_platforms(name: &str) -> Option<&'static [Platform]> {
    // The two tables share their order (a test holds them to it).
    let index = SCREEN_TOOLS.iter().position(|tool| *tool == name)?;
    Some(SCREEN_TOOL_PLATFORMS[index].1)
}

/// `values` as a sentence's list: `a, b or c`.
pub(crate) fn one_of(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// A call's `action`, which must be one of `values`. Absent (or `null`), it is an error when
/// `required`, else the first of `values` — the default.
pub(crate) fn action_in(
    input: &JsonObject,
    values: &'static [&'static str],
    required: bool,
) -> Result<Option<&'static str>, String> {
    match input.get("action") {
        None | Some(Value::Null) if required => {
            Err(format!("\"action\" is required: {}.", one_of(values)))
        }
        None | Some(Value::Null) => Ok(values.first().copied()),
        Some(Value::String(action)) => values
            .iter()
            .find(|value| **value == action)
            .map(|value| Some(*value))
            .ok_or_else(|| {
                format!(
                    "\"action\" must be {}, not \"{}\".",
                    one_of(values),
                    crate::preview_launch_config::sanitize_for_message(action)
                )
            }),
        Some(_) => Err(format!("\"action\" must be a string: {}.", one_of(values))),
    }
}

/// What a screen tool may read of the conversation's files, decided by the host the way the
/// `read` tool's are: the call's workspace (relative paths resolve against its root), the scope
/// the classifier gave the call ([`ExecutionScope`]; for `screen_package`, the one screen tool
/// that reads a file, it is unrestricted but denies the host's protected stores at every level,
/// exactly as `read`'s does — `security::classify`), and the workspace's sandbox. Only files of
/// **this computer**: a screen tool streams what it reads to a device from here, and a workspace
/// on another machine has its files there.
#[derive(Clone, Debug)]
pub(crate) struct CallFiles {
    workspace: ResolvedWorkspace,
    scope: ExecutionScope,
}

/// A file [`CallFiles::open_local_file`] opened: the handle, proved to be the canonical path that
/// was checked, and its size.
#[derive(Debug)]
pub(crate) struct LocalFile {
    pub file: File,
    pub path: PathBuf,
    pub size: u64,
}

impl CallFiles {
    pub(crate) fn new(workspace: &ResolvedWorkspace, scope: &ExecutionScope) -> Self {
        Self {
            workspace: workspace.clone(),
            scope: scope.clone(),
        }
    }

    /// The files of a workspace of this computer at `root`, within `scope`: for tests of a
    /// platform's `parse`.
    #[cfg(test)]
    pub(crate) fn local(root: &Path, scope: ExecutionScope) -> Self {
        let workspaces =
            crate::workspace_set::WorkspaceSet::local_root(root.to_string_lossy().into_owned());
        Self::new(workspaces.select(None).expect("workspace 1"), &scope)
    }

    /// The machine the workspace is on, when that is not this computer.
    pub(crate) fn remote_machine(&self) -> Option<&str> {
        (!self.workspace.is_local()).then(|| {
            let label = self.workspace.machine_label.trim();
            if label.is_empty() {
                "another machine"
            } else {
                label
            }
        })
    }

    /// Opens the existing regular file `requested` names — relative to the workspace, or absolute
    /// — through the path guard `read` uses: inside the call's scope, through no link swapped in
    /// on the way, and allowed by the workspace's sandbox. On a workspace of another machine it
    /// refuses: the one file a screen tool reads is the APK `screen_package` `install` streams to
    /// a device from this computer, so the refusal says what to do instead.
    pub(crate) fn open_local_file(&self, requested: &str) -> Result<LocalFile, String> {
        if let Some(machine) = self.remote_machine() {
            return Err(format!(
                "screen_package install reads the APK from this computer; this workspace is on {machine}. Copy the APK to this computer, or run `adb install` in that machine's shell."
            ));
        }
        let (file, path) = secure_open_existing_file_with_scope(
            Path::new(&self.workspace.root),
            requested,
            &self.scope,
        )?;
        if let Some(sandbox) = FileSandbox::of(&self.workspace)? {
            sandbox.check_read(&path)?;
        }
        let metadata = file.metadata().map_err(|error| {
            format!("Could not read the metadata of {}: {error}", path.display())
        })?;
        if !metadata.is_file() {
            return Err(format!("{} is not a regular file.", path.display()));
        }
        Ok(LocalFile {
            file,
            size: metadata.len(),
            path,
        })
    }
}

/// What the tool layer needs of a platform's parsed call; the three platforms' call types each
/// have these as their own methods.
trait PlatformCall: Send + 'static {
    fn effect(&self) -> Effect;
    fn time(&self) -> (Duration, Duration);
    fn what(&self) -> String;
    fn run(&self, driver: &dyn Driver, step: &Step<'_>) -> Result<String, OpError>;
}

macro_rules! platform_call {
    ($call:ty) => {
        impl PlatformCall for $call {
            fn effect(&self) -> Effect {
                <$call>::effect(self)
            }

            fn time(&self) -> (Duration, Duration) {
                <$call>::time(self)
            }

            fn what(&self) -> String {
                <$call>::what(self)
            }

            fn run(&self, driver: &dyn Driver, step: &Step<'_>) -> Result<String, OpError> {
                <$call>::run(self, driver, step)
            }
        }
    };
}

platform_call!(AndroidCall);
platform_call!(IosCall);
platform_call!(DesktopCall);

/// One screen tool call (see the module documentation). `files` is what `screen_package`
/// `install` may read.
pub(super) fn run_screen_tool(
    request: &ToolExecutionRequest,
    state: &AppState,
    attachment_store: Option<&ImageAttachmentStore>,
    cancel: &CancelSignal,
    call_id: Option<&str>,
    files: &CallFiles,
) -> Result<Outcome, String> {
    let tool = request.tool_name.as_str();
    let input = &request.input;
    match tool {
        "preview_zoom" => {
            only_parameters(tool, input, ZOOM_PARAMETERS)?;
            let region = zoom_region(input)?;
            let screen = screen_for(request, state, call_id)?;
            let shot = on_screen(
                tool,
                &screen,
                cancel,
                SCREEN_CALL_TIME,
                move |session, call| session.zoom_within(region, call),
            )?;
            zoom_outcome(&shot, attachment_store)
        }
        "preview_wait" => {
            only_parameters(tool, input, WAIT_PARAMETERS)?;
            let (until, timeout, region) = wait_arguments(input)?;
            let screen = screen_for(request, state, call_id)?;
            on_screen(
                tool,
                &screen,
                cancel,
                SCREEN_CALL_TIME,
                move |session, call| session.wait_within(until, timeout, region, call),
            )
            .map(Outcome::success)
        }
        _ => {
            let platforms =
                screen_tool_platforms(tool).ok_or_else(|| format!("Unknown tool: {tool}"))?;
            // The screen decides which platform parses the call, so it is resolved first.
            let screen = screen_for(request, state, call_id)?;
            match screen.session.platform() {
                // Resolution only finds screens of the tool's kinds.
                platform if !platforms.contains(&platform) => Err(format!(
                    "{tool} does not act on \"{}\"'s kind of screen.",
                    screen.server_id
                )),
                Platform::Android => {
                    run_call(AndroidCall::parse(tool, input, files)?, tool, &screen, cancel)
                }
                Platform::Ios => {
                    run_call(IosCall::parse(tool, input, files)?, tool, &screen, cancel)
                }
                Platform::Desktop => {
                    run_call(DesktopCall::parse(tool, input, files)?, tool, &screen, cancel)
                }
            }
        }
    }
}

/// `preview_dialog` on a remote-control screen: the alert or dialog a phone or tablet shows,
/// answered by its platform as an act — it holds the input lock and a read-only entry refuses it.
/// Unlike on the page, accepting is never the default here: the call says what to answer with.
pub(super) fn run_dialog_on_screen(
    request: &ToolExecutionRequest,
    screen: &RemoteScreen,
    cancel: &CancelSignal,
) -> Result<Outcome, String> {
    const TOOL: &str = "preview_dialog";
    let input = &request.input;
    only_parameters(TOOL, input, DIALOG_PARAMETERS)?;
    let given = |key: &str| input.get(key).is_some_and(|value| !value.is_null());
    if !given("button") && !given("accept") && !given("prompt_text") {
        return Err(DIALOG_NEEDS_AN_ANSWER.to_owned());
    }
    match screen.session.platform() {
        Platform::Android => run_call(AndroidCall::dialog(input)?, TOOL, screen, cancel),
        Platform::Ios => run_call(IosCall::dialog(input)?, TOOL, screen, cancel),
        Platform::Desktop => Err(no_dialogs_on_a_desktop(&screen.server_id)),
    }
}

/// What `preview_dialog` answers on a desktop, which it answers no dialogs on.
pub(super) fn no_dialogs_on_a_desktop(server_id: &str) -> String {
    format!(
        "\"{server_id}\" is a desktop, and preview_dialog answers dialogs on the web page and on phones and tablets only. Look at a desktop's dialog with preview_screenshot and answer it with preview_input."
    )
}

/// A platform's call on its screen, in the call's own time.
fn run_call(
    call: impl PlatformCall,
    tool: &str,
    screen: &RemoteScreen,
    cancel: &CancelSignal,
) -> Result<Outcome, String> {
    let time = call.time();
    on_screen(tool, screen, cancel, time, move |session, context| {
        let what = call.what();
        session.operate_within(call.effect(), &what, context, |driver, step| {
            call.run(driver, step)
        })
    })
    .map(Outcome::success)
}

/// [`on_screen_within`] with a call's `(deadline, envelope)`.
fn on_screen<T: Send + 'static>(
    tool: &str,
    screen: &RemoteScreen,
    cancel: &CancelSignal,
    (deadline, envelope): (Duration, Duration),
    work: impl FnOnce(
            &RemoteControlSession,
            &crate::remote_control::session::CallContext<'_>,
        ) -> Result<T, String>
        + Send
        + 'static,
) -> Result<T, String> {
    on_screen_within(tool, screen, cancel, deadline, envelope, work)
}

/// The screen the call acts on, held to what it was judged for.
fn screen_for(
    request: &ToolExecutionRequest,
    state: &AppState,
    call_id: Option<&str>,
) -> Result<RemoteScreen, String> {
    match target_at_dispatch(request, state, call_id)? {
        PreviewTarget::Remote(screen) => Ok(screen),
        // A screen tool resolves to a screen or to nothing (`resolve_screen_target`).
        PreviewTarget::Web(_) => Err(format!(
            "{} acts on remote-control screens, not on the web page.",
            request.tool_name
        )),
    }
}

/// The parameters `preview_zoom` takes, as its schema states them.
const ZOOM_PARAMETERS: &[&str] = &["serverId", "x", "y", "width", "height"];
/// The parameters `preview_wait` takes, as its schema states them.
const WAIT_PARAMETERS: &[&str] = &["serverId", "until", "timeout", "x", "y", "width", "height"];

/// Refuses a key `tool` does not take, by name and with the ones it does, as the platforms' own
/// parsers do: a misspelt `timeout_ms` must not silently become the default.
fn only_parameters(tool: &str, input: &JsonObject, known: &[&str]) -> Result<(), String> {
    let Some(key) = input.keys().find(|key| !known.contains(&key.as_str())) else {
        return Ok(());
    };
    let (last, rest) = known.split_last().expect("a tool takes some parameter");
    Err(format!(
        "{tool} has no parameter \"{}\"; it takes {} and {last}.",
        crate::preview_launch_config::sanitize_for_message(key),
        rest.join(", ")
    ))
}

/// A number argument within `min..=max`; `None` when it is absent (or `null`).
fn bounded_number(
    input: &JsonObject,
    key: &str,
    min: f64,
    max: f64,
) -> Result<Option<f64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| (min..=max).contains(value))
            .map(Some)
            .ok_or_else(|| format!("\"{key}\" must be a number from {min} to {max}.")),
        Some(_) => Err(format!("\"{key}\" must be a number from {min} to {max}.")),
    }
}

/// A region of the latest screenshot's frame: `x`, `y` from 0, `width`, `height` from 1, each at
/// most [`MAX_COORDINATE`]. `None` when none of the four is given; all four or none.
fn frame_region(input: &JsonObject) -> Result<Option<FrameRegion>, String> {
    let x = bounded_number(input, "x", 0.0, MAX_COORDINATE)?;
    let y = bounded_number(input, "y", 0.0, MAX_COORDINATE)?;
    let width = bounded_number(input, "width", 1.0, MAX_COORDINATE)?;
    let height = bounded_number(input, "height", 1.0, MAX_COORDINATE)?;
    match (x, y, width, height) {
        (Some(x), Some(y), Some(width), Some(height)) => Ok(Some(FrameRegion {
            x,
            y,
            width,
            height,
        })),
        (None, None, None, None) => Ok(None),
        _ => Err("Give x, y, width and height together, or none of them.".to_owned()),
    }
}

/// `preview_zoom`'s region, all four numbers required.
fn zoom_region(input: &JsonObject) -> Result<FrameRegion, String> {
    frame_region(input)?.ok_or_else(|| {
        "x, y, width and height are required: the region to zoom into, in the latest screenshot's frame.".to_owned()
    })
}

/// `preview_wait`'s `until` (default `change`), `timeout` in milliseconds (default 5000) and the
/// region it watches, if any.
fn wait_arguments(
    input: &JsonObject,
) -> Result<(WaitUntil, Duration, Option<FrameRegion>), String> {
    let until = match input.get("until") {
        None | Some(Value::Null) => WaitUntil::Change,
        Some(Value::String(until)) if until == "change" => WaitUntil::Change,
        Some(Value::String(until)) if until == "still" => WaitUntil::Still,
        Some(_) => return Err("\"until\" must be change or still.".to_owned()),
    };
    let (min, max, default) = WAIT_TIMEOUT_MS;
    let timeout = match input.get("timeout") {
        None | Some(Value::Null) => default,
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| value.fract() == 0.0 && (min as f64..=max as f64).contains(value))
            .map(|value| value as u64)
            .ok_or_else(|| {
                format!("\"timeout\" must be a whole number of milliseconds from {min} to {max}.")
            })?,
        Some(_) => {
            return Err(format!(
                "\"timeout\" must be a whole number of milliseconds from {min} to {max}."
            ))
        }
    };
    Ok((until, Duration::from_millis(timeout), frame_region(input)?))
}

/// A close-up as the model receives it: a conversation image attachment, and beside it where its
/// pixels are in the full screenshot's frame. Like a screenshot, it is delivered only at exactly
/// its own size, since that mapping is read off its size.
fn zoom_outcome(
    shot: &ZoomShot,
    attachment_store: Option<&ImageAttachmentStore>,
) -> Result<Outcome, String> {
    let store = attachment_store.ok_or_else(|| {
        "preview_zoom needs the conversation's attachment store to hand back pixels".to_owned()
    })?;
    let name = format!("zoom-{}", Utc::now().format("%Y%m%dT%H%M%S%.3fZ"));
    let image = store.import_compressed(&name, &shot.jpeg)?;
    if (image.width, image.height) != (shot.width, shot.height) {
        return Err(format!(
            "The close-up came out {}x{} instead of {}x{}, so it was not delivered: what is read off it would map back to the wrong place. Call preview_zoom again.",
            image.width, image.height, shot.width, shot.height
        ));
    }
    let whole = |value: f64| value.round() as i64;
    let text = json!({
        "region": {
            "x": whole(shot.region.x),
            "y": whole(shot.region.y),
            "width": whole(shot.region.width),
            "height": whole(shot.region.height),
        },
        "image": {"width": shot.width, "height": shot.height},
        "zoom": (shot.zoom * 100.0).round() / 100.0,
        "note": ZOOM_NOTE,
    });
    let text = serde_json::to_string_pretty(&text)
        .map_err(|error| format!("Failed to encode the close-up's result: {error}"))?;
    Ok(Outcome::success_with_images(text, vec![image]))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap_or_default()
    }

    /// Every screen tool reaches the kinds of screen the M0 table gives it, in the catalog's order,
    /// and resolves its screen; nothing else is a screen tool.
    #[test]
    fn every_screen_tool_reaches_its_kinds_of_screen_and_nothing_else_does() {
        use Platform::{Android, Desktop, Ios};
        let listed: Vec<&str> = SCREEN_TOOL_PLATFORMS.iter().map(|(tool, _)| *tool).collect();
        assert_eq!(listed, SCREEN_TOOLS.to_vec());
        let expected: [(&str, &[Platform]); 8] = [
            ("preview_zoom", &[Android, Ios, Desktop]),
            ("preview_wait", &[Android, Ios, Desktop]),
            ("screen_app", &[Android, Ios]),
            ("screen_device", &[Android, Ios]),
            ("screen_clipboard", &[Ios, Desktop]),
            ("screen_package", &[Android]),
            ("android_logcat", &[Android]),
            ("android_shell", &[Android]),
        ];
        for (tool, platforms) in expected {
            assert_eq!(screen_tool_platforms(tool), Some(platforms), "{tool}");
            assert!(
                crate::tool_executor::preview_target::resolves_a_target(tool),
                "{tool} resolves its screen"
            );
        }
        for other in [
            "preview_input",
            "preview_screenshot",
            "preview_dialog",
            "bash",
            "screen_",
        ] {
            assert_eq!(screen_tool_platforms(other), None, "{other}");
        }
        assert_eq!(DIALOG_SCREEN_PLATFORMS, &[Android, Ios]);
    }

    #[test]
    fn an_action_is_one_of_its_values_and_a_missing_one_is_said() {
        const VALUES: &[&str] = &["read", "write"];
        assert_eq!(
            action_in(&object(json!({"action": "write"})), VALUES, true),
            Ok(Some("write"))
        );
        assert_eq!(
            action_in(&object(json!({})), VALUES, false),
            Ok(Some("read"))
        );
        assert_eq!(
            action_in(&object(json!({})), VALUES, true),
            Err("\"action\" is required: read or write.".to_owned())
        );
        assert_eq!(
            action_in(&object(json!({"action": "erase"})), VALUES, true),
            Err("\"action\" must be read or write, not \"erase\".".to_owned())
        );
        assert!(action_in(&object(json!({"action": 1})), VALUES, true).is_err());
        assert_eq!(one_of(&["a", "b", "c"]), "a, b or c");
    }

    /// The bounds the schemas state: x and y from 0, width and height from 1, up to 100000; a
    /// zoom needs all four, a wait all four or none.
    #[test]
    fn regions_and_waits_are_read_within_their_schemas_bounds() {
        assert_eq!(
            zoom_region(&object(
                json!({"x": 0, "y": 10.5, "width": 1, "height": 100000})
            )),
            Ok(FrameRegion {
                x: 0.0,
                y: 10.5,
                width: 1.0,
                height: 100_000.0
            })
        );
        for bad in [
            json!({"x": -1, "y": 0, "width": 10, "height": 10}),
            json!({"x": 0, "y": 0, "width": 0, "height": 10}),
            json!({"x": 0, "y": 0, "width": 10, "height": 100001}),
            json!({"x": "1", "y": 0, "width": 10, "height": 10}),
            json!({"x": 0, "y": 0, "width": 10}),
            json!({}),
        ] {
            assert!(zoom_region(&object(bad.clone())).is_err(), "{bad}");
        }

        let (until, timeout, region) = wait_arguments(&object(json!({}))).unwrap();
        assert_eq!(until, WaitUntil::Change);
        assert_eq!(timeout, Duration::from_millis(5_000));
        assert_eq!(region, None);
        let (until, timeout, region) = wait_arguments(&object(
            json!({"until": "still", "timeout": 100, "x": 1, "y": 2, "width": 3, "height": 4}),
        ))
        .unwrap();
        assert_eq!(until, WaitUntil::Still);
        assert_eq!(timeout, Duration::from_millis(100));
        assert!(region.is_some());
        assert_eq!(
            wait_arguments(&object(json!({"timeout": 20000.0})))
                .unwrap()
                .1,
            Duration::from_millis(20_000)
        );
        for bad in [
            json!({"until": "stable"}),
            json!({"timeout": 99}),
            json!({"timeout": 20001}),
            json!({"timeout": 150.5}),
            json!({"x": 1, "y": 2}),
        ] {
            assert!(wait_arguments(&object(bad.clone())).is_err(), "{bad}");
        }
    }

    /// `preview_zoom` and `preview_wait` take exactly the parameters their schemas state, and
    /// refuse any other by name — a misspelt `timeout_ms` would otherwise wait the default — as
    /// the platforms' own parsers do.
    #[test]
    fn zoom_and_wait_take_exactly_their_schemas_parameters() {
        let profile = crate::prompt_profile::PromptProfile::builtin();
        for (tool, known) in [
            ("preview_zoom", ZOOM_PARAMETERS),
            ("preview_wait", WAIT_PARAMETERS),
        ] {
            let schema = crate::builtin_schemas::builtin_tool_schema(
                tool,
                crate::tool_surface::ToolVariant::Standard,
                &profile,
            )
            .unwrap();
            let mut stated: Vec<&str> = schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            stated.sort_unstable();
            let mut taken = known.to_vec();
            taken.sort_unstable();
            assert_eq!(stated, taken, "{tool}");
            let mut every = object(json!({"x": 1, "y": 2, "width": 3, "height": 4}));
            for key in known {
                every.entry(*key).or_insert(json!("desktop"));
            }
            assert_eq!(only_parameters(tool, &every, known), Ok(()), "{tool}");
        }
        assert_eq!(
            only_parameters(
                "preview_wait",
                &object(json!({"until": "still", "timeout_ms": 15000})),
                WAIT_PARAMETERS
            ),
            Err("preview_wait has no parameter \"timeout_ms\"; it takes serverId, until, timeout, x, y, width and height.".to_owned())
        );
        assert_eq!(
            only_parameters(
                "preview_zoom",
                &object(json!({"x": 1, "y": 1, "width": 9, "height": 9, "region": {}})),
                ZOOM_PARAMETERS
            ),
            Err("preview_zoom has no parameter \"region\"; it takes serverId, x, y, width and height.".to_owned())
        );
    }

    /// The APK `install` reads is a file of this computer, found the way `read` finds one; a
    /// workspace on another machine is refused with what to do instead.
    #[test]
    fn a_call_reads_only_local_files_inside_its_scope() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("app-debug.apk"), b"PK\x03\x04apk").unwrap();
        std::fs::write(outside.path().join("other.apk"), b"PK").unwrap();
        let files = CallFiles::local(
            workspace.path(),
            ExecutionScope::workspace_only(workspace.path()),
        );
        assert_eq!(files.remote_machine(), None);
        let opened = files.open_local_file("app-debug.apk").unwrap();
        assert_eq!(opened.size, 7);
        assert!(opened.path.ends_with("app-debug.apk"));
        assert!(files
            .open_local_file(&outside.path().join("other.apk").to_string_lossy())
            .is_err());
        assert!(files.open_local_file("missing.apk").is_err());
        assert!(files.open_local_file(".").is_err());

        let mut remote = files.workspace.clone();
        remote.machine = Some(crate::model::RunTarget::Ssh {
            machine_id: "m1".into(),
        });
        remote.machine_label = "mac".into();
        let remote = CallFiles::new(&remote, &ExecutionScope::Unrestricted);
        assert_eq!(remote.remote_machine(), Some("mac"));
        assert_eq!(
            remote.open_local_file("app-debug.apk").unwrap_err(),
            "screen_package install reads the APK from this computer; this workspace is on mac. Copy the APK to this computer, or run `adb install` in that machine's shell."
        );
    }
}
