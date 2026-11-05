//! The iOS side of the screen tools: `screen_app`, `screen_device` and `screen_clipboard`, and
//! `preview_dialog` ([`IosCall::dialog`]), on an iPhone or iPad reached through WebDriverAgent.
//!
//! Each call is parsed here ([`IosCall::parse`]) before anything reaches the
//! device, declares what it does to the screen ([`IosCall::effect`], which
//! decides the input lock and the read-only rule in `session::platform`) and
//! how long it may take ([`IosCall::time`]), and runs on the session's driver
//! through [`Driver::ios`] ([`IosControl`]): WebDriverAgent routes inside the
//! session the driver already has, never a new one.
//!
//! What this module is careful about:
//!
//! * **Routes are fixed.** Every route is a literal in this file; nothing the
//!   model wrote becomes part of one. Its values travel in JSON bodies, built by
//!   the `*_body` functions, after the bundle id and variable names they hold
//!   were checked. The one id spliced into a route is an element's, which the
//!   device returned and which is format-checked first ([`clear_field`]).
//! * **Time.** Each WebDriverAgent call gets the shorter of its route's own
//!   limit and what is left of the tool call ([`Wda::ask`]); a step that no
//!   longer fits is not started, and an operation that stops says what it had
//!   already done.
//! * **Answers are plain text, a fact a line.** Whatever the device sent has its
//!   control and format characters dropped and is cut ([`plain`], [`one_line`]); in an
//!   error message it goes through `sanitize_for_message` instead, which also turns
//!   quote-like characters into plain quotes and keeps to one line.
//! * **The person's secrets stay theirs.** `screen_device` unlock never types a
//!   passcode: a device that stays locked answers with who has to unlock it. A password
//!   field is never cleared ([`clear_field`]).
//! * **What only Android has is refused by name** before anything is sent, with what an
//!   iPhone or iPad does instead.

use std::{collections::HashSet, sync::OnceLock, time::Duration};

use base64::{
    alphabet,
    engine::{
        general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD},
        DecodePaddingMode,
    },
    Engine as _,
};
use regex::Regex;
use serde_json::{json, Map, Value};

use crate::{
    model::JsonObject,
    preview_launch_config::sanitize_for_message,
    remote_control::{
        driver::{Driver, DriverError, IosControl, WdaRoute},
        session::{Effect, OpError, Step},
        wda::valid_element_id,
    },
    tool_executor::screen_tools::{action_in, one_of, CallFiles, SCREEN_CALL_TIME},
};

#[cfg(test)]
mod tests;

// The bounds of the schemas (`builtin_schemas.rs`). `tests::parse_takes_the_schemas_bounds`
// walks each schema and holds both to the same numbers.
const APP_MAX: usize = 255;
const URL_MAX: usize = 2048;
const FILTER_MAX: usize = 200;
const LIMIT_MIN: u64 = 1;
const LIMIT_MAX: u64 = 1000;
const LIMIT_DEFAULT: usize = 200;
const ARGUMENTS_MAX: usize = 32;
const ARGUMENT_MAX: usize = 1024;
const ENVIRONMENT_MAX: usize = 32;
const ENVIRONMENT_VALUE_MAX: usize = 1024;
const BUTTON_MAX: usize = 256;
const PROMPT_TEXT_MAX: usize = 4096;
const VALUE_MAX: usize = 32;
const CLIPBOARD_MAX: usize = 100_000;

/// Launching, bringing to the front, quitting and opening a URL wait for the app to settle.
const APP_CALL: Duration = Duration::from_secs(20);
/// A question WebDriverAgent answers at once.
const READ_CALL: Duration = Duration::from_secs(5);
/// Pressing an alert's button, or typing into its field.
const ALERT_CALL: Duration = Duration::from_secs(10);
/// How fast WebDriverAgent types: about this many characters a second (`wda.rs`).
const TYPING_CHARS_PER_SECOND: usize = 60;
/// The most characters of `prompt_text` an iPhone or iPad is given: what WebDriverAgent types in
/// [`ALERT_CALL`] with a sixth of it to spare. Longer is refused before anything is sent (the
/// call would be cut off part way, and the field left with half the text); the schema's own
/// bound ([`PROMPT_TEXT_MAX`]) is the page's.
const PROMPT_TEXT_TYPABLE: usize =
    ALERT_CALL.as_secs() as usize * TYPING_CHARS_PER_SECOND * 5 / 6;
/// Orientation, appearance, keyboard, simulated location.
const DEVICE_CALL: Duration = Duration::from_secs(10);
/// WebDriverAgent waits up to 5 s for the screen to lock.
const LOCK_CALL: Duration = Duration::from_secs(10);
/// It presses Home twice, a second apart, and waits up to 5 s for the screen to unlock.
const UNLOCK_CALL: Duration = Duration::from_secs(14);
const PASTEBOARD_WRITE_CALL: Duration = Duration::from_secs(8);
/// WebDriverAgent itself waits up to 30 s on the system's paste prompt; this call does not.
const PASTEBOARD_READ_CALL: Duration = Duration::from_secs(20);
/// Finding elements by a predicate walks the screen's accessibility tree, which takes seconds on
/// a busy one.
const FIND_CALL: Duration = Duration::from_secs(20);
/// Clearing a field types its backspaces on the device.
const CLEAR_CALL: Duration = Duration::from_secs(15);
/// How long after opening a URL an alert asking to open the app may take to appear.
const URL_SETTLE: Duration = Duration::from_millis(600);
/// How long after answering an alert the next one may take to appear.
const ALERT_SETTLE: Duration = Duration::from_millis(400);
/// A call with less time than this left is not started: it could not be answered.
const MIN_CALL: Duration = Duration::from_millis(250);

/// What of an alert, a label or a device's field is shown, in characters.
const ALERT_SHOWN: usize = 300;
const LABEL_SHOWN: usize = 100;
const FIELD_SHOWN: usize = 80;
const URL_SHOWN: usize = 200;
/// At most this many buttons of an alert are listed.
const BUTTONS_SHOWN: usize = 24;
/// The clipboard is shown up to this many bytes.
const CLIPBOARD_SHOWN: usize = 64 * 1024;

/// `com.apple.springboard` is what is in front on the home screen, the lock screen and
/// beneath a system alert.
const SPRINGBOARD: &str = "com.apple.springboard";

const NO_ALERT: &str = "No alert is showing that WebDriverAgent can see. If one is on the screen, tap its button by its position instead.";
const PASSCODE: &str =
    "The device is locked with a passcode. Ask the user to unlock it; never type a passcode.";
const NO_DISMISS_KEY: &str =
    "There is no key to dismiss this keyboard; tap outside the field instead.";
const NOT_AN_IPHONE: &str = "this screen is not an iPhone or iPad";
/// What `list` says beside the apps: WebDriverAgent has no list of what is installed.
const RUNNING_ONLY: &str = "Only apps running now are listed; an iPhone or iPad does not tell WebDriverAgent what is installed.";
const NEEDS_AN_ANSWER: &str = "preview_dialog needs button (the exact label of the button to press), accept (true or false) or prompt_text (to type into the alert's field).";

/// The keys WebDriverAgent may tap to dismiss a keyboard. It taps the first whose label or
/// identifier matches; phones often have none.
const DISMISS_KEYS: [&str; 7] = [
    "Done",
    "done",
    "Return",
    "return",
    "Search",
    "Go",
    "Hide keyboard",
];

/// One call of an iOS tool, parsed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IosCall {
    op: Op,
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    /// `screen_app` list: the running apps whose bundle id contains `filter`, at most `limit`.
    Apps {
        filter: Option<String>,
        limit: usize,
    },
    Foreground,
    AppState(String),
    Launch {
        app: String,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
        restart: bool,
    },
    Activate(String),
    Stop(String),
    OpenUrl {
        url: String,
        app: Option<String>,
    },
    DeviceInfo(Vec<Section>),
    Lock,
    Unlock,
    Rotate(Orientation),
    Appearance(Appearance),
    DismissKeyboard,
    Location {
        latitude: f64,
        longitude: f64,
    },
    ClearLocation,
    ReadClipboard,
    WriteClipboard(String),
    Dialog(Dialog),
}

/// `preview_dialog` on an iPhone or iPad: text for the alert's field, an answer, or both (typed
/// first). An answer-less dialog only types.
#[derive(Clone, Debug, PartialEq)]
struct Dialog {
    prompt_text: Option<String>,
    answer: Option<Answer>,
}

#[derive(Clone, Debug, PartialEq)]
enum Answer {
    /// The button with exactly this label.
    Button(String),
    /// `accept: true`: the button WebDriverAgent picks by position.
    Accept,
    /// `accept: false`.
    Dismiss,
}

/// The sections of `screen_device` info an iPhone or iPad has, in the order they are told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Device,
    Display,
    Power,
}

/// The sections of `screen_device` info the schema lists; the fourth is Android's alone.
const SECTIONS: [&str; 4] = ["device", "display", "power", "notifications"];

impl Section {
    fn word(self) -> &'static str {
        match self {
            Self::Device => SECTIONS[0],
            Self::Display => SECTIONS[1],
            Self::Power => SECTIONS[2],
        }
    }
}

/// How the device is turned, by the words `screen_device` rotate takes: the way the DEVICE is
/// turned from portrait, whichever way an app draws.
///
/// Apple's `UIDeviceOrientationLandscapeLeft` is "the device is in landscape mode, with the
/// device held upright and the home button on the right side": turned a quarter counter-clockwise
/// from portrait. WebDriverAgent's `POST /orientation` takes `LANDSCAPE` for it and
/// `UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT` for `UIDeviceOrientationLandscapeRight` (turned
/// clockwise): `FBOrientationCommands.m` in appium/WebDriverAgent maps
/// `FBWDOrientationValues.landscapeLeft` ("LANDSCAPE") to `UIDeviceOrientationLandscapeLeft` and
/// `landscapeRight` to `UIDeviceOrientationLandscapeRight`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Orientation {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
    PortraitUpsideDown,
}

/// The `value`s `screen_device` rotate takes, in the order the schema's description lists them.
const ORIENTATIONS: [&str; 4] = [
    "portrait",
    "landscape_left",
    "landscape_right",
    "portrait_upside_down",
];

impl Orientation {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "portrait" => Some(Self::Portrait),
            "landscape_left" => Some(Self::LandscapeLeft),
            "landscape_right" => Some(Self::LandscapeRight),
            "portrait_upside_down" => Some(Self::PortraitUpsideDown),
            _ => None,
        }
    }

    /// As the tool says it.
    fn word(self) -> &'static str {
        match self {
            Self::Portrait => ORIENTATIONS[0],
            Self::LandscapeLeft => ORIENTATIONS[1],
            Self::LandscapeRight => ORIENTATIONS[2],
            Self::PortraitUpsideDown => ORIENTATIONS[3],
        }
    }

    /// As WebDriverAgent names it.
    fn wda(self) -> &'static str {
        match self {
            Self::Portrait => "PORTRAIT",
            Self::LandscapeLeft => "LANDSCAPE",
            Self::LandscapeRight => "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT",
            Self::PortraitUpsideDown => "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN",
        }
    }

    /// The orientation WebDriverAgent's name stands for: [`Self::wda`] the other way round. Its
    /// `GET /orientation` answers with one of these four names, or "Unknown orientation"; a
    /// current build folds them into `PORTRAIT` and `LANDSCAPE` (its `_wdOrientationsMapping`),
    /// which is why [`orientation_text`] asks `GET /rotation` first.
    fn from_wda(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "PORTRAIT" => Some(Self::Portrait),
            "LANDSCAPE" => Some(Self::LandscapeLeft),
            "UIA_DEVICE_ORIENTATION_LANDSCAPERIGHT" => Some(Self::LandscapeRight),
            "UIA_DEVICE_ORIENTATION_PORTRAIT_UPSIDEDOWN" => Some(Self::PortraitUpsideDown),
            _ => None,
        }
    }

    /// The orientation `GET /rotation`'s `z` stands for. WebDriverAgent's `fb_rotationMapping`
    /// (`XCUIDevice+FBRotation.m`) gives portrait 0, landscape right 90, upside down 180 and
    /// landscape left 270; a device it cannot place answers -1.
    fn from_degrees(degrees: f64) -> Option<Self> {
        match degrees {
            degrees if degrees == 0.0 => Some(Self::Portrait),
            degrees if degrees == 90.0 => Some(Self::LandscapeRight),
            degrees if degrees == 180.0 => Some(Self::PortraitUpsideDown),
            degrees if degrees == 270.0 => Some(Self::LandscapeLeft),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// The `action` values each tool takes.
fn actions(tool: &str) -> Option<&'static [&'static str]> {
    match tool {
        "screen_app" => Some(&[
            "list",
            "foreground",
            "info",
            "launch",
            "activate",
            "stop",
            "open_url",
        ]),
        "screen_device" => Some(&[
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
        ]),
        "screen_clipboard" => Some(&["read", "write"]),
        _ => None,
    }
}

/// Whether the tool's `action` must be given: only `screen_clipboard`'s must. `screen_app` lists
/// and `screen_device` reports when it is left out.
fn action_required(tool: &str) -> bool {
    tool == "screen_clipboard"
}

/// A parameter of a tool, and the actions it is for (none listed: every call of the tool).
struct Param {
    name: &'static str,
    actions: &'static [&'static str],
    /// Set for a parameter only an Android device has: what this screen does instead.
    android_only: Option<&'static str>,
}

const fn param(name: &'static str, actions: &'static [&'static str]) -> Param {
    Param {
        name,
        actions,
        android_only: None,
    }
}

const fn android(name: &'static str, instead: &'static str) -> Param {
    Param {
        name,
        actions: &[],
        android_only: Some(instead),
    }
}

const APP_PARAMS: &[Param] = &[
    param("app", &["info", "launch", "activate", "stop", "open_url"]),
    param("url", &["open_url"]),
    param("filter", &["list"]),
    param("limit", &["list"]),
    android(
        "scope",
        "WebDriverAgent lists only the apps running now, whatever the scope. Leave it out.",
    ),
    android(
        "activity",
        "an app starts from its bundle id alone. Leave it out.",
    ),
    param("restart", &["launch"]),
    param("arguments", &["launch"]),
    param("environment", &["launch"]),
];

const DEVICE_PARAMS: &[Param] = &[
    param("value", &["rotate", "appearance"]),
    param("sections", &["info"]),
    param("latitude", &["location"]),
    param("longitude", &["location"]),
];

const CLIPBOARD_PARAMS: &[Param] = &[param("text", &["write"])];

const DIALOG_PARAMS: &[Param] = &[
    param("accept", &[]),
    param("prompt_text", &[]),
    param("button", &[]),
];

/// Every parameter of `tool` other than `serverId` and `action`, in the schema's order.
fn params(tool: &str) -> &'static [Param] {
    match tool {
        "screen_app" => APP_PARAMS,
        "screen_device" => DEVICE_PARAMS,
        "screen_clipboard" => CLIPBOARD_PARAMS,
        "preview_dialog" => DIALOG_PARAMS,
        _ => &[],
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// An argument given: present and not `null`.
fn given(input: &JsonObject, key: &str) -> bool {
    input.get(key).is_some_and(|value| !value.is_null())
}

/// Refuses an argument the tool does not have, naming the ones it does.
fn refuse_unknown(tool: &str, input: &JsonObject) -> Result<(), String> {
    let with_action = actions(tool).is_some();
    let known = || {
        let mut names = vec!["serverId"];
        if with_action {
            names.push("action");
        }
        names.extend(params(tool).iter().map(|param| param.name));
        names
    };
    for key in input.keys() {
        let ours = key == "serverId"
            || (with_action && key == "action")
            || params(tool).iter().any(|param| param.name == key);
        if !ours {
            return Err(format!(
                "{tool} has no parameter \"{}\"; it takes {}.",
                sanitize_for_message(key),
                and_list(&known())
            ));
        }
    }
    Ok(())
}

/// Refuses a parameter only an Android device has, saying so and what to do instead.
fn refuse_android_only(tool: &str, input: &JsonObject) -> Result<(), String> {
    for param in params(tool) {
        if let Some(instead) = param.android_only {
            if given(input, param.name) {
                return Err(format!(
                    "\"{}\" is Android-only, and this screen is an iPhone or iPad: {instead}",
                    param.name
                ));
            }
        }
    }
    Ok(())
}

/// Refuses an action only an Android device has: the action, what this screen is, and what to use
/// instead.
fn refuse_android_only_action(tool: &str, action: Option<&str>) -> Result<(), String> {
    let (Some(action), "screen_device") = (action, tool) else {
        return Ok(());
    };
    let instead = match action {
        "wake" => {
            return Err("An iPhone or iPad has no wake on its own; unlock wakes it and swipes the lock screen away (only without a passcode).".to_owned())
        }
        "animations" => "WebDriverAgent cannot change its animation speed. preview_wait waits for an animation to end.",
        "stay_awake" => "WebDriverAgent cannot keep its screen on. Unlock it again when it sleeps.",
        "notifications" => "WebDriverAgent cannot read or open its notifications in this build. Swipe down from the top edge of the screen with preview_input.",
        "quick_settings" => "WebDriverAgent cannot open Control Center in this build. Swipe down from the top-right corner with preview_input (up from the bottom edge on an iPhone with a Home button).",
        "collapse" => "it has no shade for WebDriverAgent to collapse. Swipe up from the bottom edge with preview_input, or press the Home key.",
        _ => return Ok(()),
    };
    Err(format!(
        "screen_device {action} is Android-only, and this screen is an iPhone or iPad: {instead}"
    ))
}

/// Refuses an argument that is for other actions than the one chosen.
fn refuse_inapplicable(tool: &str, action: Option<&str>, input: &JsonObject) -> Result<(), String> {
    let Some(action) = action else {
        return Ok(());
    };
    for param in params(tool) {
        if given(input, param.name) && !param.actions.is_empty() && !param.actions.contains(&action)
        {
            return Err(format!(
                "\"{}\" does not apply to {tool} {action}; it is for {}.",
                param.name,
                and_list(param.actions)
            ));
        }
    }
    Ok(())
}

/// The bounds in a sentence: `"url" must be 1 to 2048 characters.`
fn length_message(key: &str, min: usize, max: usize) -> String {
    if min == 0 {
        format!("\"{key}\" must be at most {max} characters.")
    } else {
        format!("\"{key}\" must be {min} to {max} characters.")
    }
}

/// A string argument of `min` to `max` characters; `None` when it is absent (or `null`).
fn text_arg(
    input: &JsonObject,
    key: &str,
    min: usize,
    max: usize,
) -> Result<Option<String>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let length = text.chars().count();
            if length < min || length > max {
                Err(length_message(key, min, max))
            } else {
                Ok(Some(text.clone()))
            }
        }
        Some(_) => Err(format!("\"{key}\" must be a string.")),
    }
}

/// A true-or-false argument; `None` when it is absent (or `null`).
fn flag_arg(input: &JsonObject, key: &str) -> Result<Option<bool>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("\"{key}\" must be true or false.")),
    }
}

/// A bundle id: letters, digits, dots and hyphens.
fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

/// The name of an environment variable: a letter or underscore, then letters, digits, underscores.
fn valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn app_arg(input: &JsonObject) -> Result<Option<String>, String> {
    let Some(id) = text_arg(input, "app", 1, APP_MAX)? else {
        return Ok(None);
    };
    if !valid_bundle_id(&id) {
        return Err(
            "\"app\" must be a bundle id such as com.apple.Preferences: letters, digits, dots and hyphens."
                .to_owned(),
        );
    }
    Ok(Some(id))
}

fn limit_arg(input: &JsonObject) -> Result<usize, String> {
    let message = || format!("\"limit\" must be a whole number from {LIMIT_MIN} to {LIMIT_MAX}.");
    match input.get("limit") {
        None | Some(Value::Null) => Ok(LIMIT_DEFAULT),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| {
                value.fract() == 0.0 && (LIMIT_MIN as f64..=LIMIT_MAX as f64).contains(value)
            })
            .map(|value| value as usize)
            .ok_or_else(message),
        Some(_) => Err(message()),
    }
}

fn arguments_arg(input: &JsonObject) -> Result<Vec<String>, String> {
    let message = || {
        format!("\"arguments\" must be an array of at most {ARGUMENTS_MAX} strings of at most {ARGUMENT_MAX} characters each.")
    };
    match input.get("arguments") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) if items.len() <= ARGUMENTS_MAX => items
            .iter()
            .map(|item| match item {
                Value::String(text) if text.chars().count() <= ARGUMENT_MAX => Ok(text.clone()),
                _ => Err(message()),
            })
            .collect(),
        Some(_) => Err(message()),
    }
}

fn environment_arg(input: &JsonObject) -> Result<Vec<(String, String)>, String> {
    let message = || {
        format!("\"environment\" must be an object of at most {ENVIRONMENT_MAX} variables, each a string of at most {ENVIRONMENT_VALUE_MAX} characters.")
    };
    match input.get("environment") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(variables)) if variables.len() <= ENVIRONMENT_MAX => variables
            .iter()
            .map(|(name, value)| {
                if !valid_variable_name(name) {
                    return Err(format!(
                        "The environment variable name \"{}\" is not valid: use letters, digits and underscores, and do not start with a digit.",
                        sanitize_for_message(name)
                    ));
                }
                match value {
                    Value::String(text) if text.chars().count() <= ENVIRONMENT_VALUE_MAX => {
                        Ok((name.clone(), text.clone()))
                    }
                    _ => Err(message()),
                }
            })
            .collect(),
        Some(_) => Err(message()),
    }
}

/// A number argument from `-limit` to `limit`; `None` when it is absent (or `null`).
fn degrees_arg(input: &JsonObject, key: &str, limit: f64) -> Result<Option<f64>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| value.is_finite() && value.abs() <= limit)
            .map(Some)
            .ok_or_else(|| format!("\"{key}\" must be a number from -{limit} to {limit}.")),
        Some(_) => Err(format!(
            "\"{key}\" must be a number from -{limit} to {limit}."
        )),
    }
}

fn sections_bound() -> String {
    format!(
        "\"sections\" must be a list of 1 to {} of {}, each at most once.",
        SECTIONS.len(),
        and_list(&SECTIONS)
    )
}

/// `info`'s `sections`: the ones an iPhone or iPad has, in the order they are told; all three when
/// none is given.
fn sections_arg(input: &JsonObject) -> Result<Vec<Section>, String> {
    let items = match input.get("sections") {
        None | Some(Value::Null) => {
            return Ok(vec![Section::Device, Section::Display, Section::Power])
        }
        Some(Value::Array(items)) if !items.is_empty() && items.len() <= SECTIONS.len() => items,
        Some(_) => return Err(sections_bound()),
    };
    let mut sections = Vec::with_capacity(items.len());
    for item in items {
        let section = match item.as_str() {
            Some("device") => Section::Device,
            Some("display") => Section::Display,
            Some("power") => Section::Power,
            Some("notifications") => {
                return Err("\"notifications\" is Android-only, and this screen is an iPhone or iPad: WebDriverAgent cannot read its notifications. Ask for device, display or power.".to_owned())
            }
            _ => return Err(sections_bound()),
        };
        if sections.contains(&section) {
            return Err(sections_bound());
        }
        sections.push(section);
    }
    sections.sort();
    Ok(sections)
}

fn parse_app(action: &str, input: &JsonObject) -> Result<Op, String> {
    let app = app_arg(input)?;
    let needed = || {
        app.clone().ok_or_else(|| {
            format!("screen_app {action} needs \"app\", the app's bundle id such as com.apple.Preferences.")
        })
    };
    Ok(match action {
        "list" => Op::Apps {
            filter: text_arg(input, "filter", 1, FILTER_MAX)?,
            limit: limit_arg(input)?,
        },
        "foreground" => Op::Foreground,
        "info" => Op::AppState(needed()?),
        "launch" => Op::Launch {
            app: needed()?,
            arguments: arguments_arg(input)?,
            environment: environment_arg(input)?,
            restart: flag_arg(input, "restart")?.unwrap_or(false),
        },
        "activate" => Op::Activate(needed()?),
        "stop" => Op::Stop(needed()?),
        _ => {
            let url = text_arg(input, "url", 1, URL_MAX)?.ok_or_else(|| {
                "screen_app open_url needs \"url\", the URL or deep link to open.".to_owned()
            })?;
            Op::OpenUrl { url, app }
        }
    })
}

fn parse_device(action: &str, input: &JsonObject) -> Result<Op, String> {
    let value = text_arg(input, "value", 1, VALUE_MAX)?;
    let latitude = degrees_arg(input, "latitude", 90.0)?;
    let longitude = degrees_arg(input, "longitude", 180.0)?;
    Ok(match action {
        "info" => Op::DeviceInfo(sections_arg(input)?),
        "lock" => Op::Lock,
        "unlock" => Op::Unlock,
        "rotate" => {
            let value = value.ok_or_else(|| {
                format!(
                    "screen_device rotate needs \"value\": {}.",
                    one_of(&ORIENTATIONS)
                )
            })?;
            Op::Rotate(Orientation::parse(&value).ok_or_else(|| rotation_refusal(&value))?)
        }
        "appearance" => {
            let value = value.ok_or_else(|| {
                "screen_device appearance needs \"value\": light or dark.".to_owned()
            })?;
            Op::Appearance(Appearance::parse(&value).ok_or_else(|| appearance_refusal(&value))?)
        }
        "dismiss_keyboard" => Op::DismissKeyboard,
        "location" => {
            match (latitude, longitude) {
                (Some(latitude), Some(longitude)) => Op::Location {
                    latitude,
                    longitude,
                },
                _ => return Err(
                    "screen_device location needs both \"latitude\" and \"longitude\", in degrees."
                        .to_owned(),
                ),
            }
        }
        _ => Op::ClearLocation,
    })
}

/// Why a `rotate` value is not one: `auto` and `landscape` are the two the model is likeliest to
/// try, and each has its own answer.
fn rotation_refusal(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => format!(
            "\"auto\" is Android-only, and this screen is an iPhone or iPad: it has no auto-rotate for WebDriverAgent to switch on. Give {}.",
            one_of(&ORIENTATIONS)
        ),
        "landscape" => format!(
            "\"landscape\" does not say which way the device is turned. Give landscape_left (a quarter counter-clockwise from portrait, Home button on the right) or landscape_right (clockwise), not \"{}\".",
            sanitize_for_message(value)
        ),
        _ => format!(
            "\"value\" for screen_device rotate must be {}, not \"{}\".",
            one_of(&ORIENTATIONS),
            sanitize_for_message(value)
        ),
    }
}

fn appearance_refusal(value: &str) -> String {
    if value.trim().eq_ignore_ascii_case("auto") {
        return "\"auto\" is Android-only, and this screen is an iPhone or iPad: its appearance is light or dark, and WebDriverAgent cannot set it to follow the time of day. Give light or dark.".to_owned();
    }
    format!(
        "\"value\" for screen_device appearance must be light or dark, not \"{}\".",
        sanitize_for_message(value)
    )
}

fn parse_clipboard(action: &str, input: &JsonObject) -> Result<Op, String> {
    Ok(match action {
        "read" => Op::ReadClipboard,
        _ => Op::WriteClipboard(text_arg(input, "text", 0, CLIPBOARD_MAX)?.ok_or_else(|| {
            "screen_clipboard write needs \"text\", the text to put on the clipboard (empty to clear it)."
                .to_owned()
        })?),
    })
}

impl IosCall {
    /// Parses one call of `tool` from its JSON input; every bound in the schema is checked here
    /// with the same numbers, an argument that is for another action is refused, and so is one
    /// the tool does not have (`serverId` is the tool layer's, and is let through). What only an
    /// Android device has is refused by name, with what an iPhone or iPad does instead.
    pub(crate) fn parse(
        tool: &str,
        input: &JsonObject,
        _files: &CallFiles,
    ) -> Result<Self, String> {
        let tool = match tool {
            "screen_app" => "screen_app",
            "screen_device" => "screen_device",
            "screen_clipboard" => "screen_clipboard",
            other => return Err(format!("{other} is not an iOS tool.")),
        };
        refuse_unknown(tool, input)?;
        refuse_android_only(tool, input)?;
        let action = match actions(tool) {
            Some(values) => action_in(input, values, action_required(tool))?,
            None => None,
        };
        refuse_android_only_action(tool, action)?;
        refuse_inapplicable(tool, action, input)?;
        let action = action.unwrap_or_default();
        let op = match tool {
            "screen_app" => parse_app(action, input)?,
            "screen_device" => parse_device(action, input)?,
            _ => parse_clipboard(action, input)?,
        };
        Ok(Self { op })
    }

    /// `preview_dialog` on an iPhone or iPad: `button` presses the button with exactly that label,
    /// `accept` accepts or dismisses by position, and `prompt_text` types into the alert's field,
    /// first when an answer is given too and alone (the alert stays) when not.
    pub(crate) fn dialog(input: &JsonObject) -> Result<Self, String> {
        const TOOL: &str = "preview_dialog";
        refuse_unknown(TOOL, input)?;
        let button = text_arg(input, "button", 1, BUTTON_MAX)?;
        let prompt_text = text_arg(input, "prompt_text", 0, PROMPT_TEXT_MAX)?;
        if let Some(text) = &prompt_text {
            let length = text.chars().count();
            if length > PROMPT_TEXT_TYPABLE {
                return Err(format!(
                    "\"prompt_text\" is {length} characters, and WebDriverAgent types about {TYPING_CHARS_PER_SECOND} characters a second: an iPhone or iPad takes at most {PROMPT_TEXT_TYPABLE} here, within this call's {} s. Nothing was typed or pressed, and the alert is still showing. Tap the alert's field to focus it, then type the longer text with preview_input type instead.",
                    ALERT_CALL.as_secs()
                ));
            }
        }
        let accept = flag_arg(input, "accept")?;
        let answer = match (button, accept) {
            (Some(_), Some(_)) => {
                return Err("preview_dialog takes button or accept, not both: button presses the button with exactly that label, accept guesses one by position.".to_owned())
            }
            (Some(label), None) => Some(Answer::Button(label)),
            (None, Some(true)) => Some(Answer::Accept),
            (None, Some(false)) => Some(Answer::Dismiss),
            (None, None) => None,
        };
        match (&prompt_text, &answer) {
            (None, None) => return Err(NEEDS_AN_ANSWER.to_owned()),
            (Some(_), Some(Answer::Dismiss)) => {
                return Err("\"prompt_text\" does not apply to preview_dialog with accept false: dismissing an alert does not use the text. Give accept true or a button to type it and answer, or neither to only type it.".to_owned())
            }
            _ => {}
        }
        Ok(Self {
            op: Op::Dialog(Dialog {
                prompt_text,
                answer,
            }),
        })
    }

    /// Only the reads are looks. Everything else is an act, a clipboard write included: it
    /// changes the device and not what its screen shows, but text put on the clipboard is there
    /// to be pasted by the next input, so it holds the screen's input lock and cannot land between
    /// another caller's write and their paste.
    pub(crate) fn effect(&self) -> Effect {
        match &self.op {
            Op::Apps { .. }
            | Op::Foreground
            | Op::AppState(_)
            | Op::DeviceInfo(_)
            | Op::ReadClipboard => Effect::Look,
            _ => Effect::Act,
        }
    }

    /// The call's deadline and envelope (default: `SCREEN_CALL_DEADLINE` / `PREVIEW_TOOL_TIMEOUT`).
    pub(crate) fn time(&self) -> (Duration, Duration) {
        SCREEN_CALL_TIME
    }

    /// For the session's error prefix and logs: "launching com.apple.Preferences".
    pub(crate) fn what(&self) -> String {
        match &self.op {
            Op::Apps { .. } => "listing the running apps".to_owned(),
            Op::Foreground => "reading the app in front".to_owned(),
            Op::AppState(app) => format!("reading the state of {app}"),
            Op::Launch {
                app, restart: true, ..
            } => format!("restarting {app}"),
            Op::Launch { app, .. } => format!("launching {app}"),
            Op::Activate(app) => format!("bringing {app} to the front"),
            Op::Stop(app) => format!("quitting {app}"),
            Op::OpenUrl { .. } => "opening a URL".to_owned(),
            Op::DeviceInfo(_) => "reading the device's state".to_owned(),
            Op::Dialog(Dialog { answer: None, .. }) => "typing into the alert".to_owned(),
            Op::Dialog(_) => "answering the alert".to_owned(),
            Op::Lock => "locking the device".to_owned(),
            Op::Unlock => "unlocking the device".to_owned(),
            Op::Rotate(_) => "rotating the device".to_owned(),
            Op::Appearance(_) => "changing the appearance".to_owned(),
            Op::DismissKeyboard => "dismissing the keyboard".to_owned(),
            Op::Location { .. } => "setting the simulated location".to_owned(),
            Op::ClearLocation => "clearing the simulated location".to_owned(),
            Op::ReadClipboard => "reading the clipboard".to_owned(),
            Op::WriteClipboard(_) => "writing the clipboard".to_owned(),
        }
    }

    pub(crate) fn run(&self, driver: &dyn Driver, step: &Step<'_>) -> Result<String, OpError> {
        let Some(control) = driver.ios() else {
            return Err(OpError::Said(NOT_AN_IPHONE.to_owned()));
        };
        let wda = Wda { control, step };
        match &self.op {
            Op::Apps { filter, limit } => list_apps(&wda, filter.as_deref(), *limit),
            Op::Foreground => foreground(&wda),
            Op::AppState(app) => app_state(&wda, app),
            Op::Launch {
                app,
                arguments,
                environment,
                restart,
            } => launch(&wda, app, arguments, environment, *restart),
            Op::Activate(app) => activate(&wda, app),
            Op::Stop(app) => stop(&wda, app),
            Op::OpenUrl { url, app } => open_url(&wda, url, app.as_deref()),
            Op::DeviceInfo(sections) => info(&wda, sections),
            Op::Dialog(dialog) => answer_dialog(&wda, dialog),
            Op::Lock => lock(&wda),
            Op::Unlock => unlock(&wda),
            Op::Rotate(orientation) => rotate(&wda, *orientation),
            Op::Appearance(appearance) => set_appearance(&wda, *appearance),
            Op::DismissKeyboard => dismiss_keyboard(&wda),
            Op::Location {
                latitude,
                longitude,
            } => set_location(&wda, *latitude, *longitude),
            Op::ClearLocation => clear_location(&wda),
            Op::ReadClipboard => read_clipboard(&wda),
            Op::WriteClipboard(text) => write_clipboard(&wda, text),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

fn bundle_body(bundle_id: &str) -> Value {
    json!({"bundleId": bundle_id})
}

/// `arguments` and `environment` go only when there are some: they apply to a fresh start.
fn launch_body(bundle_id: &str, arguments: &[String], environment: &[(String, String)]) -> Value {
    let mut body = bundle_body(bundle_id);
    if !arguments.is_empty() {
        body["arguments"] = json!(arguments);
    }
    if !environment.is_empty() {
        let variables: Map<String, Value> = environment
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        body["environment"] = Value::Object(variables);
    }
    body
}

fn url_body(url: &str, bundle_id: Option<&str>) -> Value {
    let mut body = json!({"url": url});
    if let Some(bundle_id) = bundle_id {
        body["bundleId"] = json!(bundle_id);
    }
    body
}

fn orientation_body(orientation: Orientation) -> Value {
    json!({"orientation": orientation.wda()})
}

fn appearance_body(appearance: Appearance) -> Value {
    json!({"name": appearance.word()})
}

fn keyboard_body() -> Value {
    json!({"keyNames": DISMISS_KEYS})
}

fn location_body(latitude: f64, longitude: f64) -> Value {
    json!({"latitude": latitude, "longitude": longitude})
}

fn alert_name_body(label: &str) -> Value {
    json!({"name": label})
}

fn alert_text_body(text: &str) -> Value {
    json!({"value": text})
}

/// The pasteboard takes its content as base64 of the bytes.
fn pasteboard_write_body(text: &str) -> Value {
    json!({"content": STANDARD.encode(text.as_bytes()), "contentType": "plaintext"})
}

fn pasteboard_read_body() -> Value {
    json!({"contentType": "plaintext"})
}

/// A search for elements by a predicate (`POST /elements`).
fn find_body(predicate: &str) -> Value {
    json!({"using": "predicate string", "value": predicate})
}

// ---------------------------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------------------------

/// WebDriverAgent as one operation sees it: calls inside the operation's time.
struct Wda<'a> {
    control: &'a dyn IosControl,
    step: &'a Step<'a>,
}

fn route_path(route: WdaRoute<'_>) -> &str {
    match route {
        WdaRoute::Session(path) | WdaRoute::Root(path) => path,
    }
}

/// A route the build lacks, in the model's terms: appium/WebDriverAgent 6 (2024) has every route
/// the iOS tools call.
fn missing_route(route: &str) -> String {
    format!("This WebDriverAgent build has no {route}; it is too old (appium/WebDriverAgent 6 or newer has it).")
}

impl Wda<'_> {
    /// One WebDriverAgent call in the shorter of `natural` and what is left of the tool call.
    /// An operation that was stopped, or is out of time, does not start it.
    fn ask(
        &self,
        method: &str,
        route: WdaRoute<'_>,
        body: Option<&Value>,
        natural: Duration,
    ) -> Result<Value, OpError> {
        self.step
            .proceed("asking the device")
            .map_err(OpError::Said)?;
        let timeout = natural.min(self.step.remaining());
        if timeout < MIN_CALL {
            return Err(OpError::Said(
                "Ran out of this call's time before asking the device.".to_owned(),
            ));
        }
        self.control
            .call(method, route, body, timeout)
            .map_err(OpError::Driver)
    }

    fn get(&self, route: WdaRoute<'_>, natural: Duration) -> Result<Value, OpError> {
        self.ask("GET", route, None, natural)
    }

    /// [`Self::ask`] for a call the operation cannot do without: a route the build lacks is said
    /// in the model's terms ([`missing_route`]) instead of as the driver words it.
    fn require(
        &self,
        method: &str,
        route: WdaRoute<'_>,
        body: Option<&Value>,
        natural: Duration,
    ) -> Result<Value, OpError> {
        let path = route_path(route);
        unsupported_said(self.ask(method, route, body, natural), path)
    }
}

/// An `Unsupported` answer as the operation's own sentence about `route`.
fn unsupported_said(result: Result<Value, OpError>, route: &str) -> Result<Value, OpError> {
    match result {
        Err(OpError::Driver(DriverError::Unsupported(_))) => {
            Err(OpError::Said(missing_route(route)))
        }
        other => other,
    }
}

/// `error` of the step `what`, said after what was already done: "Quit com.foo. Then launching it
/// failed: …". A device that went away stays the driver's error, so that the session reconnects.
fn after(done: Option<&str>, what: &str, error: OpError) -> OpError {
    match (done, error) {
        (Some(done), OpError::Said(message)) => OpError::Said(format!("{done}. {message}")),
        (Some(done), OpError::Driver(error)) if !matches!(error, DriverError::Disconnected(_)) => {
            OpError::Said(format!(
                "{done}. Then {what} failed: {}.",
                error.message().trim_end_matches('.')
            ))
        }
        (_, error) => error,
    }
}

// ---------------------------------------------------------------------------------------------
// Device text
// ---------------------------------------------------------------------------------------------

/// Text a device sent, for the model: control characters other than line breaks and tabs are
/// dropped, and so are the format characters (Unicode category Cf: bidirectional overrides and
/// isolates, zero-width and joiner characters, tag characters), which show nothing of their own
/// but can reorder what is around them or carry text no one sees.
fn plain(text: &str) -> String {
    static FORMAT: OnceLock<Regex> = OnceLock::new();
    let visible: String = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    FORMAT
        .get_or_init(|| Regex::new(r"\p{Cf}").expect("valid format character regex"))
        .replace_all(&visible, "")
        .into_owned()
}

/// `text` cut to `limit` characters, with an ellipsis when it was cut.
fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(limit).collect();
    cut.push('\u{2026}');
    cut
}

/// [`plain`] on one line (runs of white space, line breaks included, become one space), cut.
fn one_line(text: &str, limit: usize) -> String {
    let text = plain(text);
    cut(
        &text.split_whitespace().collect::<Vec<_>>().join(" "),
        limit,
    )
}

/// A string field of a reply, cleaned; `None` when it is absent, not text, or empty.
fn field(value: Option<&Value>, limit: usize) -> Option<String> {
    let text = one_line(value?.as_str()?, limit);
    (!text.is_empty()).then_some(text)
}

/// Degrees as a short number: `52.52`, not `52.520000`.
fn degrees_text(degrees: f64) -> String {
    let text = format!("{degrees:.6}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-0" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

// ---------------------------------------------------------------------------------------------
// screen_app
// ---------------------------------------------------------------------------------------------

/// `GET /wda/apps/list`: WebDriverAgent lists the apps that are active now, as `{pid, bundleId}`,
/// and nothing it is not running. The answer says so.
fn list_apps(wda: &Wda<'_>, filter: Option<&str>, limit: usize) -> Result<String, OpError> {
    let reply = wda.require("GET", WdaRoute::Session("/wda/apps/list"), None, READ_CALL)?;
    let entries: &[Value] = match &reply {
        Value::Array(entries) => entries,
        Value::Null => &[],
        _ => {
            return Err(OpError::Said(
                "WebDriverAgent's list of running apps is not a list.".to_owned(),
            ))
        }
    };
    let wanted = filter.map(str::to_lowercase);
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for entry in entries {
        let Some(id) = field(entry.get("bundleId"), APP_MAX) else {
            continue;
        };
        if let Some(wanted) = &wanted {
            if !id.to_lowercase().contains(wanted.as_str()) {
                continue;
            }
        }
        if seen.insert(id.clone()) {
            ids.push(id);
        }
    }
    let shown = ids.len().min(limit);
    let subject = match filter {
        Some(filter) => format!("Running apps matching \"{}\"", sanitize_for_message(filter)),
        None => "Running apps".to_owned(),
    };
    let mut said = format!("{subject}: {} (showing {shown}). {RUNNING_ONLY}", ids.len());
    for id in ids.iter().take(limit) {
        said.push('\n');
        said.push_str(id);
    }
    Ok(said)
}

/// The app in front and whether the lock screen shows, each best effort: it fails only when
/// neither could be read.
fn foreground(wda: &Wda<'_>) -> Result<String, OpError> {
    let mut probe = Probe::new(wda);
    let front = probe.get(WdaRoute::Root("/wda/activeAppInfo"));
    let locked = probe.get(WdaRoute::Root("/wda/locked"));
    probe.verdict("screen_app foreground")?;
    let mut lines = vec![
        info_line("app in front", front.as_ref().and_then(front_text)),
        info_line("lock state", lock_text(locked.as_ref())),
    ];
    if let Some(stop) = &probe.stopped {
        lines.push(stop.clone());
    }
    Ok(lines.join("\n"))
}

fn app_state(wda: &Wda<'_>, app: &str) -> Result<String, OpError> {
    let state = wda.require(
        "POST",
        WdaRoute::Session("/wda/apps/state"),
        Some(&bundle_body(app)),
        READ_CALL,
    )?;
    let named = app_state_name(&state);
    let mut said = info_line(&format!("app {app}"), named.map(str::to_owned));
    // WebDriverAgent answers "not running" for an app that is not installed as well.
    if named == Some("not running") {
        said.push_str(" (WebDriverAgent cannot tell this from an app that is not installed)");
    }
    Ok(said)
}

fn launch(
    wda: &Wda<'_>,
    app: &str,
    arguments: &[String],
    environment: &[(String, String)],
    restart: bool,
) -> Result<String, OpError> {
    // `restart` quits the app first, so that what is launched is a fresh start and the arguments
    // and environment apply.
    let was_running = if restart { Some(quit(wda, app)?) } else { None };
    let done = was_running.map(|running| {
        if running {
            format!("Quit {app}")
        } else {
            format!("{app} was not running")
        }
    });
    let body = launch_body(app, arguments, environment);
    wda.require(
        "POST",
        WdaRoute::Session("/wda/apps/launch"),
        Some(&body),
        APP_CALL,
    )
    .map_err(|error| after(done.as_deref(), "launching it", error))?;
    Ok(match was_running {
        None => format!("Launched {app}."),
        Some(true) => format!("Restarted {app}: quit it and launched it again."),
        Some(false) => format!("{app} was not running; launched it."),
    })
}

fn activate(wda: &Wda<'_>, app: &str) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Session("/wda/apps/activate"),
        Some(&bundle_body(app)),
        APP_CALL,
    )?;
    Ok(format!("Brought {app} to the front."))
}

/// Quits `app` and says whether it was running. WebDriverAgent answers whether it was; a build
/// that answers nothing is taken to have quit it.
fn quit(wda: &Wda<'_>, app: &str) -> Result<bool, OpError> {
    let was_running = wda.require(
        "POST",
        WdaRoute::Session("/wda/apps/terminate"),
        Some(&bundle_body(app)),
        APP_CALL,
    )?;
    Ok(was_running != Value::Bool(false))
}

fn stop(wda: &Wda<'_>, app: &str) -> Result<String, OpError> {
    Ok(if quit(wda, app)? {
        format!("Quit {app}.")
    } else {
        format!("{app} was not running.")
    })
}

fn open_url(wda: &Wda<'_>, url: &str, app: Option<&str>) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Session("/url"),
        Some(&url_body(url, app)),
        APP_CALL,
    )?;
    let mut said = format!("Opened {}.", one_line(url, URL_SHOWN));
    // The system may ask whether to open the app, and nothing else says so.
    if !wda.step.pause(URL_SETTLE) {
        said.push_str("\nNo time was left to check for an alert.");
        return Ok(said);
    }
    if let Some(alert) = read_alert_quietly(wda) {
        said.push('\n');
        said.push_str(&alert_sentence("An alert is showing", &alert));
    }
    Ok(said)
}

// ---------------------------------------------------------------------------------------------
// Alerts (preview_dialog)
// ---------------------------------------------------------------------------------------------

/// An alert as WebDriverAgent shows it: the text, and the labels of its buttons when they could
/// be read. Both as the device sent them.
#[derive(Clone, Debug, PartialEq)]
struct Alert {
    text: String,
    buttons: Option<Vec<String>>,
}

/// An alert's text: WebDriverAgent joins its lines with newlines. A reply that is no text at
/// all (WebDriverAgent answers `null` to commands that return nothing) is no alert.
fn alert_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(lines) => Some(
            lines
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// An alert's button labels; `None` when the reply lists none.
fn alert_buttons(value: &Value) -> Option<Vec<String>> {
    let labels: Vec<String> = value
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|label| !label.is_empty())
        .take(BUTTONS_SHOWN)
        .map(str::to_owned)
        .collect();
    (!labels.is_empty()).then_some(labels)
}

/// Whether a failed call says no alert is showing: WebDriverAgent answers "no such alert" with
/// HTTP 404 and W3C's words for it.
fn is_no_alert(error: &DriverError) -> bool {
    let DriverError::Failed(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("no such alert")
        || message.contains("modal dialog")
        || message.contains("http 404")
}

/// A button label as shown to the model.
fn shown_label(label: &str) -> String {
    one_line(label, LABEL_SHOWN)
}

/// `Cancel, Open`; a label with a comma in it is quoted so the list stays unambiguous.
fn label_list(labels: &[String]) -> String {
    list_labels(labels, shown_label)
}

/// [`label_list`] for an error message, where device text goes through
/// [`sanitize_for_message`] (no quote-like characters, no invisible ones, one line); a quoted
/// label cannot hold a quote, so the list stays unambiguous.
fn label_list_in_error(labels: &[String]) -> String {
    list_labels(labels, sanitize_for_message)
}

fn list_labels(labels: &[String], show: impl Fn(&str) -> String) -> String {
    labels
        .iter()
        .map(|label| {
            let shown = show(label);
            if shown.contains(',') {
                format!("\"{shown}\"")
            } else {
                shown
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// `An alert is showing: "Allow Paste?". Buttons: Don't Allow, Allow.`
fn alert_sentence(lead: &str, alert: &Alert) -> String {
    let text = one_line(&alert.text, ALERT_SHOWN);
    let head = if text.is_empty() {
        format!("{lead}: no text.")
    } else {
        format!("{lead}: \"{text}\".")
    };
    match &alert.buttons {
        Some(buttons) => format!("{head} Buttons: {}.", label_list(buttons)),
        None => format!("{head} Its buttons could not be read."),
    }
}

/// The alert showing, if any. A device that answers that none is showing is `Ok(None)`; a build
/// without the route, or a lost connection, is an error. The buttons are read best effort.
fn read_alert(wda: &Wda<'_>) -> Result<Option<Alert>, OpError> {
    let text = match unsupported_said(
        wda.get(WdaRoute::Session("/alert/text"), READ_CALL),
        "/alert/text",
    ) {
        Ok(value) => match alert_text(&value) {
            Some(text) => text,
            None => return Ok(None),
        },
        Err(OpError::Driver(error)) if is_no_alert(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let buttons = match wda.get(WdaRoute::Session("/wda/alert/buttons"), READ_CALL) {
        Ok(value) => alert_buttons(&value),
        Err(OpError::Driver(error @ DriverError::Disconnected(_))) => {
            return Err(OpError::Driver(error))
        }
        Err(_) => None,
    };
    Ok(Some(Alert { text, buttons }))
}

/// [`read_alert`] for a look that is only a courtesy to the call's answer: whatever goes wrong,
/// the answer is the one without it.
fn read_alert_quietly(wda: &Wda<'_>) -> Option<Alert> {
    read_alert(wda).ok().flatten()
}

/// A label as an error message says it ([`sanitize_for_message`]), without the ellipsis that
/// marks it as changed: what a model that copies a label out of an error writes in `button`.
fn as_the_error_says(label: &str) -> String {
    let said = sanitize_for_message(label);
    match said.strip_suffix('\u{2026}') {
        Some(unmarked) => unmarked.to_owned(),
        None => said,
    }
}

/// The button the model named, as the device labels it: its label as the device sent it or as
/// [`label_list`] shows it, or else as an error message shows it (the typographic apostrophe of
/// "Don\u{2019}t Allow" reads as an ASCII one there, and a model copies what it was told). Either
/// must pick exactly one button: of several with the same label none is pressed, and the answer
/// lists them. Without a list of buttons to check against, the label is taken as given and the
/// device decides.
fn pick_button(alert: &Alert, label: &str) -> Result<String, OpError> {
    let Some(buttons) = &alert.buttons else {
        return Ok(label.to_owned());
    };
    // The label as the device sent it, or as `label_list` shows it: it must match exactly one
    // button. With two buttons of the same label, which one the model means is not known, and
    // pressing the first could answer a permission or a deletion the other way.
    let exact: Vec<&String> = buttons
        .iter()
        .filter(|button| button.as_str() == label || shown_label(button) == label)
        .collect();
    match exact.as_slice() {
        [] => {}
        [only] => return Ok((*only).clone()),
        _ => {
            return Err(OpError::Said(format!(
                "Several buttons of the alert \"{}\" are labelled \"{}\", so none was pressed; its buttons: {}. Take a preview_snapshot and press the one you mean with preview_click.",
                sanitize_for_message(&alert.text),
                sanitize_for_message(label),
                label_list_in_error(buttons)
            )))
        }
    }
    // Else as an error message shows it, when that picks exactly one.
    let wanted = as_the_error_says(label);
    let mut like = buttons
        .iter()
        .filter(|button| !wanted.is_empty() && as_the_error_says(button) == wanted);
    if let (Some(button), None) = (like.next(), like.next()) {
        return Ok(button.clone());
    }
    // Everything the device sent is cleaned for an error, and what the model wrote is quoted back
    // the same way, so no quote or line break of either can reshape the sentence.
    Err(OpError::Said(format!(
        "No button \"{}\" on the alert \"{}\"; its buttons: {}.",
        sanitize_for_message(label),
        sanitize_for_message(&alert.text),
        label_list_in_error(buttons)
    )))
}

/// ` on "Allow Paste?"`, or nothing for an alert with no text.
fn on_alert(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!(" on \"{text}\"")
    }
}

/// ` "Allow Paste?"`, or nothing for an alert with no text: `the alert "Allow Paste?"`.
fn named(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else {
        format!(" \"{text}\"")
    }
}

/// Which button `/alert/accept` (`accept`) or `/alert/dismiss` pressed, as far as it can be
/// told. WebDriverAgent (`FBAlert.m`) presses the last button of an alert to accept and the
/// first to dismiss, and the other way round for an action sheet; the alert's type is not told,
/// so this is the alert's reading, said as a guess. A list of buttons cut at [`BUTTONS_SHOWN`] has
/// no known last one.
fn guessed_button(alert: &Alert, accept: bool) -> String {
    let (place, button) = match (&alert.buttons, accept) {
        (Some(buttons), true) if buttons.len() < BUTTONS_SHOWN => ("last", buttons.last()),
        (Some(buttons), false) => ("first", buttons.first()),
        _ => ("", None),
    };
    match button {
        Some(button) => format!("the {place} one, most likely \"{}\"", shown_label(button)),
        None => "its buttons could not be read in full, so which one was pressed is not known"
            .to_owned(),
    }
}

/// How the dialog is answered, once the button is settled.
enum Pressing<'a> {
    /// The button the model named by `asked`, which the device labels `device`.
    Button { asked: &'a str, device: String },
    /// By position, as `accept` or `dismiss`.
    Position { accept: bool },
}

fn answer_dialog(wda: &Wda<'_>, dialog: &Dialog) -> Result<String, OpError> {
    let alert = read_alert(wda)?.ok_or_else(|| OpError::Said(NO_ALERT.to_owned()))?;
    let text = one_line(&alert.text, ALERT_SHOWN);
    // Chosen before anything is typed: a label the alert does not have must not leave text in
    // its field.
    let pressing = match &dialog.answer {
        Some(Answer::Button(asked)) => Some(Pressing::Button {
            asked,
            device: pick_button(&alert, asked)?,
        }),
        Some(Answer::Accept) => Some(Pressing::Position { accept: true }),
        Some(Answer::Dismiss) => Some(Pressing::Position { accept: false }),
        None => None,
    };
    let typed = match &dialog.prompt_text {
        Some(typed) => {
            wda.require(
                "POST",
                WdaRoute::Session("/alert/text"),
                Some(&alert_text_body(typed)),
                ALERT_CALL,
            )?;
            Some(format!(
                "Typed {} characters into the alert{}",
                typed.chars().count(),
                named(&text)
            ))
        }
        None => None,
    };
    let Some(pressing) = pressing else {
        // The alert stays: its field is filled, and a button still has to answer it.
        let mut said = typed.map(|typed| format!("{typed}.")).unwrap_or_default();
        if let Some(buttons) = &alert.buttons {
            said.push_str(&format!(" Buttons: {}.", label_list(buttons)));
        }
        return Ok(said);
    };
    let (pressed, answered) = match &pressing {
        Pressing::Button { asked, device } => (
            wda.require(
                "POST",
                WdaRoute::Session("/alert/accept"),
                Some(&alert_name_body(device)),
                ALERT_CALL,
            ),
            format!(
                "Pressed \"{}\"{}.",
                one_line(asked, LABEL_SHOWN),
                on_alert(&text)
            ),
        ),
        Pressing::Position { accept } => (
            wda.require(
                "POST",
                WdaRoute::Session(if *accept {
                    "/alert/accept"
                } else {
                    "/alert/dismiss"
                }),
                Some(&json!({})),
                ALERT_CALL,
            ),
            format!(
                "{} the alert{}. The button was guessed by position: {}.",
                if *accept { "Accepted" } else { "Dismissed" },
                named(&text),
                guessed_button(&alert, *accept)
            ),
        ),
    };
    pressed.map_err(|error| after(typed.as_deref(), "answering it", error))?;
    let mut done = match typed {
        Some(typed) => format!("{typed}. {answered}"),
        None => answered,
    };
    // Another alert often follows (a permission, then the next permission).
    if wda.step.pause(ALERT_SETTLE) {
        if let Some(next) = read_alert_quietly(wda) {
            let lead = if next == alert {
                "The same alert is still showing"
            } else {
                "Another alert is showing"
            };
            done.push('\n');
            done.push_str(&alert_sentence(lead, &next));
        }
    }
    Ok(done)
}

// ---------------------------------------------------------------------------------------------
// screen_device
// ---------------------------------------------------------------------------------------------

fn lock(wda: &Wda<'_>) -> Result<String, OpError> {
    wda.require("POST", WdaRoute::Root("/wda/lock"), None, LOCK_CALL)?;
    Ok("Locked the device.".to_owned())
}

/// Whether `/wda/locked` says the device is locked.
fn is_locked(value: &Value) -> bool {
    value.as_bool() == Some(true)
}

fn unlock(wda: &Wda<'_>) -> Result<String, OpError> {
    let sent = unsupported_said(
        wda.ask("POST", WdaRoute::Root("/wda/unlock"), None, UNLOCK_CALL),
        "/wda/unlock",
    );
    // Only WebDriverAgent saying no is worth looking past: it also says no to a passcode.
    if let Err(error @ (OpError::Said(_) | OpError::Driver(DriverError::Disconnected(_)))) = sent {
        return Err(error);
    }
    let locked = wda.get(WdaRoute::Root("/wda/locked"), READ_CALL);
    match (locked, sent) {
        (Ok(locked), _) if is_locked(&locked) => Err(OpError::Said(PASSCODE.to_owned())),
        (Ok(_), _) => Ok("The device is unlocked.".to_owned()),
        (Err(error @ OpError::Driver(DriverError::Disconnected(_))), _) => Err(error),
        (Err(_), Ok(_)) => Ok(
            "Sent the unlock, but the lock state could not be read; look at the screen to see whether it worked."
                .to_owned(),
        ),
        (Err(_), Err(error)) => Err(error),
    }
}

fn rotate(wda: &Wda<'_>, orientation: Orientation) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Session("/orientation"),
        Some(&orientation_body(orientation)),
        DEVICE_CALL,
    )?;
    Ok(format!("Rotated to {}.", orientation.word()))
}

fn set_appearance(wda: &Wda<'_>, appearance: Appearance) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Root("/wda/device/appearance"),
        Some(&appearance_body(appearance)),
        DEVICE_CALL,
    )?;
    Ok(format!("Set the appearance to {}.", appearance.word()))
}

fn dismiss_keyboard(wda: &Wda<'_>) -> Result<String, OpError> {
    let route = "/wda/keyboard/dismiss";
    let result = unsupported_said(
        wda.ask(
            "POST",
            WdaRoute::Session(route),
            Some(&keyboard_body()),
            DEVICE_CALL,
        ),
        route,
    );
    match result {
        Ok(_) => Ok("Dismissed the keyboard.".to_owned()),
        // A phone's keyboard often has no key for it, and WebDriverAgent says so.
        Err(OpError::Driver(DriverError::Failed(message)))
            if message
                .to_ascii_lowercase()
                .contains("dismiss the keyboard") =>
        {
            Err(OpError::Said(NO_DISMISS_KEY.to_owned()))
        }
        Err(error) => Err(error),
    }
}

fn set_location(wda: &Wda<'_>, latitude: f64, longitude: f64) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Session("/wda/simulatedLocation"),
        Some(&location_body(latitude, longitude)),
        DEVICE_CALL,
    )?;
    Ok(format!(
        "Set the simulated location to {}, {}.",
        degrees_text(latitude),
        degrees_text(longitude)
    ))
}

fn clear_location(wda: &Wda<'_>) -> Result<String, OpError> {
    wda.require(
        "DELETE",
        WdaRoute::Session("/wda/simulatedLocation"),
        None,
        DEVICE_CALL,
    )?;
    Ok("Cleared the simulated location.".to_owned())
}

// ---------------------------------------------------------------------------------------------
// screen_clipboard
// ---------------------------------------------------------------------------------------------

fn write_clipboard(wda: &Wda<'_>, text: &str) -> Result<String, OpError> {
    wda.require(
        "POST",
        WdaRoute::Root("/wda/setPasteboard"),
        Some(&pasteboard_write_body(text)),
        PASTEBOARD_WRITE_CALL,
    )?;
    Ok(match text.chars().count() {
        0 => "Emptied the clipboard.".to_owned(),
        count => format!("Put {count} characters on the clipboard."),
    })
}

/// A pasteboard reply: base64 of the text's bytes (an empty string when there is nothing),
/// decoded as UTF-8 with what is not text replaced. Line breaks and tabs stay; other control
/// characters go.
fn pasteboard_text(value: &Value) -> Result<String, String> {
    let encoded = match value {
        Value::String(text) => text,
        Value::Null => return Ok(String::new()),
        _ => return Err("WebDriverAgent's clipboard reply is not text.".to_owned()),
    };
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let compact: String = encoded
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    let bytes = engine
        .decode(compact)
        .map_err(|_| "WebDriverAgent's clipboard reply is not valid base64.".to_owned())?;
    Ok(plain(&String::from_utf8_lossy(&bytes)))
}

/// `text` cut to at most `limit` bytes at a character boundary, and whether it was cut.
fn clipped(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn read_clipboard(wda: &Wda<'_>) -> Result<String, OpError> {
    let value = wda.require(
        "POST",
        WdaRoute::Root("/wda/getPasteboard"),
        Some(&pasteboard_read_body()),
        PASTEBOARD_READ_CALL,
    )?;
    let text = pasteboard_text(&value).map_err(OpError::Said)?;
    let characters = text.chars().count();
    if characters == 0 {
        return Ok("The clipboard is empty (on a real device this can also mean WebDriverAgent could not read it).".to_owned());
    }
    let (shown, was_cut) = clipped(&text, CLIPBOARD_SHOWN);
    let head = if was_cut {
        format!("Clipboard ({characters} characters; the first {CLIPBOARD_SHOWN} bytes are shown):")
    } else {
        format!("Clipboard ({characters} characters):")
    };
    Ok(format!("{head}\n{shown}"))
}

// ---------------------------------------------------------------------------------------------
// screen_device info
// ---------------------------------------------------------------------------------------------

/// What `info` asks, as the device answered. A piece the device did not answer is `None`.
#[derive(Default)]
struct Readings {
    status: Option<Value>,
    device: Option<Value>,
    front: Option<Value>,
    locked: Option<Value>,
    orientation: Option<Value>,
    rotation: Option<Value>,
    battery: Option<Value>,
    /// `Some(None)`: asked, and no alert is showing.
    alert: Option<Option<Alert>>,
}

/// `iOS 17.5`, from `/status`.
fn system_text(status: &Value) -> Option<String> {
    let os = status.get("os")?;
    let version = field(os.get("version"), FIELD_SHOWN)?;
    Some(match field(os.get("name"), FIELD_SHOWN) {
        Some(name) => format!("{name} {version}"),
        None => version,
    })
}

fn battery_text(battery: &Value) -> Option<String> {
    let level = battery
        .get("level")
        .and_then(Value::as_f64)
        .filter(|level| (0.0..=1.0).contains(level))
        .map(|level| format!("{}%", (level * 100.0).round() as i64));
    let state = match battery.get("state").and_then(Value::as_f64) {
        Some(state) if state == 1.0 => Some("unplugged"),
        Some(state) if state == 2.0 => Some("charging"),
        Some(state) if state == 3.0 => Some("full"),
        _ => None,
    };
    match (level, state) {
        (Some(level), Some(state)) => Some(format!("{level}, {state}")),
        (Some(level), None) => Some(level),
        (None, Some(state)) => Some(format!("level unknown, {state}")),
        (None, None) => None,
    }
}

/// The app in front, from `/wda/activeAppInfo`.
fn front_text(front: &Value) -> Option<String> {
    let bundle_id = field(front.get("bundleId"), FIELD_SHOWN);
    let name = field(front.get("name"), FIELD_SHOWN);
    match (name, bundle_id) {
        (_, Some(bundle_id)) if bundle_id == SPRINGBOARD => {
            Some(format!("home screen or system UI ({SPRINGBOARD})"))
        }
        (Some(name), Some(bundle_id)) => Some(format!("{name} ({bundle_id})")),
        (None, Some(bundle_id)) => Some(bundle_id),
        (Some(name), None) => Some(name),
        (None, None) => None,
    }
}

/// What `/wda/apps/state` counts (XCUIApplicationState): 1 not running, 2 running in the
/// background and suspended, 3 running in the background, 4 running in the foreground.
fn app_state_name(state: &Value) -> Option<&'static str> {
    match state.as_f64()? as i64 {
        1 => Some("not running"),
        2 => Some("background (suspended)"),
        3 => Some("background"),
        4 => Some("foreground"),
        _ => None,
    }
}

/// `locked` or `unlocked`, from `/wda/locked`.
fn lock_text(locked: Option<&Value>) -> Option<String> {
    locked
        .and_then(Value::as_bool)
        .map(|locked| String::from(if locked { "locked" } else { "unlocked" }))
}

/// The device's orientation in the words `screen_device` rotate takes, which WebDriverAgent's own
/// names are not. `GET /rotation` is asked first: it says which way (a current build's
/// `GET /orientation` folds upside down into `PORTRAIT` and both landscapes into `LANDSCAPE`).
/// Without it, the name `GET /orientation` gave is read as WebDriverAgent sets it
/// ([`Orientation::from_wda`]); a name that is none of the four is shown as it came, in lower case.
fn orientation_text(orientation: Option<&Value>, rotation: Option<&Value>) -> Option<String> {
    if let Some(precise) = rotation
        .and_then(|rotation| rotation.get("z"))
        .and_then(Value::as_f64)
        .and_then(Orientation::from_degrees)
    {
        return Some(precise.word().to_owned());
    }
    let name = field(orientation, FIELD_SHOWN)?;
    Some(match Orientation::from_wda(&name) {
        Some(orientation) => orientation.word().to_owned(),
        None => name.to_lowercase(),
    })
}

fn info_line(label: &str, value: Option<String>) -> String {
    format!("{label}: {}", value.unwrap_or_else(|| "unknown".to_owned()))
}

impl Readings {
    /// One section, as `device:` and then its lines indented.
    fn section(&self, section: Section) -> String {
        let device = |key: &str| {
            self.device
                .as_ref()
                .and_then(|device| field(device.get(key), FIELD_SHOWN))
        };
        let lines = match section {
            Section::Device => vec![
                info_line("name", device("name")),
                info_line("model", device("model")),
                info_line("system", self.status.as_ref().and_then(system_text)),
                info_line(
                    "kind",
                    self.device
                        .as_ref()
                        .and_then(|device| device.get("isSimulator"))
                        .and_then(Value::as_bool)
                        .map(|simulator| {
                            String::from(if simulator {
                                "simulator"
                            } else {
                                "real device"
                            })
                        }),
                ),
                info_line("locale", device("currentLocale")),
                info_line("time zone", device("timeZone")),
            ],
            Section::Display => vec![
                info_line(
                    "appearance",
                    device("userInterfaceStyle")
                        .map(|style| style.to_lowercase())
                        .filter(|style| matches!(style.as_str(), "light" | "dark")),
                ),
                info_line(
                    "orientation",
                    orientation_text(self.orientation.as_ref(), self.rotation.as_ref()),
                ),
                info_line("app in front", self.front.as_ref().and_then(front_text)),
            ],
            Section::Power => vec![
                info_line("lock state", lock_text(self.locked.as_ref())),
                info_line("battery", self.battery.as_ref().and_then(battery_text)),
            ],
        };
        let mut block = format!("{}:", section.word());
        for line in lines {
            block.push_str("\n  ");
            block.push_str(&line);
        }
        block
    }

    /// The alert showing, with its buttons: told whichever sections were asked for.
    fn alert_lines(&self) -> Vec<String> {
        match &self.alert {
            None => vec!["alert: unknown".to_owned()],
            Some(None) => vec!["alert: none".to_owned()],
            Some(Some(alert)) => {
                let text = one_line(&alert.text, ALERT_SHOWN);
                vec![
                    if text.is_empty() {
                        "alert: showing, with no text".to_owned()
                    } else {
                        format!("alert: \"{text}\"")
                    },
                    info_line("alert buttons", alert.buttons.as_deref().map(label_list)),
                ]
            }
        }
    }
}

/// The questions `info` asks one after another, and how they went. A question that fails
/// leaves its piece unknown; a lost connection or an operation out of time ends the asking.
struct Probe<'a> {
    wda: &'a Wda<'a>,
    answered: usize,
    failures: Vec<DriverError>,
    gone: Option<DriverError>,
    stopped: Option<String>,
}

impl<'a> Probe<'a> {
    fn new(wda: &'a Wda<'a>) -> Self {
        Self {
            wda,
            answered: 0,
            failures: Vec::new(),
            gone: None,
            stopped: None,
        }
    }

    fn over(&self) -> bool {
        self.gone.is_some() || self.stopped.is_some()
    }

    /// The reply, or `None` with the failure kept.
    fn ask(&mut self, method: &str, route: WdaRoute<'_>, body: Option<&Value>) -> Option<Value> {
        if self.over() {
            return None;
        }
        match self.wda.ask(method, route, body, READ_CALL) {
            Ok(value) => {
                self.answered += 1;
                Some(value)
            }
            Err(OpError::Driver(error @ DriverError::Disconnected(_))) => {
                self.gone = Some(error);
                None
            }
            Err(OpError::Driver(error)) => {
                self.failures.push(error);
                None
            }
            Err(OpError::Said(stop)) => {
                self.stopped = Some(stop);
                None
            }
        }
    }

    fn get(&mut self, route: WdaRoute<'_>) -> Option<Value> {
        self.ask("GET", route, None)
    }

    /// The alert showing: `Some(None)` when none is, `None` when that could not be told.
    fn alert(&mut self) -> Option<Option<Alert>> {
        if self.over() {
            return None;
        }
        match self.wda.get(WdaRoute::Session("/alert/text"), READ_CALL) {
            Ok(value) => {
                self.answered += 1;
                let Some(text) = alert_text(&value) else {
                    return Some(None);
                };
                let buttons = self
                    .get(WdaRoute::Session("/wda/alert/buttons"))
                    .and_then(|value| alert_buttons(&value));
                Some(Some(Alert { text, buttons }))
            }
            // Nothing is showing is an answer.
            Err(OpError::Driver(error)) if is_no_alert(&error) => {
                self.answered += 1;
                Some(None)
            }
            Err(OpError::Driver(error @ DriverError::Disconnected(_))) => {
                self.gone = Some(error);
                None
            }
            Err(OpError::Driver(error)) => {
                self.failures.push(error);
                None
            }
            Err(OpError::Said(stop)) => {
                self.stopped = Some(stop);
                None
            }
        }
    }

    /// Fine unless nothing at all answered, which says why. `reads` names the call whose
    /// routes were tried: "screen_device info".
    fn verdict(&self, reads: &str) -> Result<(), OpError> {
        if self.answered > 0 {
            return Ok(());
        }
        if let Some(gone) = &self.gone {
            return Err(OpError::Driver(gone.clone()));
        }
        if let Some(stop) = &self.stopped {
            return Err(OpError::Said(stop.clone()));
        }
        match self
            .failures
            .iter()
            .find(|error| !matches!(error, DriverError::Unsupported(_)))
        {
            Some(error) => Err(OpError::Driver(error.clone())),
            None => Err(OpError::Said(format!(
                "This WebDriverAgent build has none of the routes {reads} reads; it is too old (appium/WebDriverAgent 6 or newer has them)."
            ))),
        }
    }
}

fn info(wda: &Wda<'_>, sections: &[Section]) -> Result<String, OpError> {
    let wants = |section: Section| sections.contains(&section);
    let mut probe = Probe::new(wda);
    let mut readings = Readings::default();
    if wants(Section::Device) {
        readings.status = probe.get(WdaRoute::Root("/status"));
    }
    // The device's own report holds the appearance as well as the name and the model.
    if wants(Section::Device) || wants(Section::Display) {
        readings.device = probe.get(WdaRoute::Root("/wda/device/info"));
    }
    if wants(Section::Display) {
        readings.orientation = probe.get(WdaRoute::Session("/orientation"));
        readings.rotation = probe.get(WdaRoute::Session("/rotation"));
        readings.front = probe.get(WdaRoute::Root("/wda/activeAppInfo"));
    }
    if wants(Section::Power) {
        readings.locked = probe.get(WdaRoute::Root("/wda/locked"));
        readings.battery = probe.get(WdaRoute::Session("/wda/batteryInfo"));
    }
    readings.alert = probe.alert();
    probe.verdict("screen_device info")?;
    let mut lines: Vec<String> = sections
        .iter()
        .map(|section| readings.section(*section))
        .collect();
    lines.extend(readings.alert_lines());
    // Lines may be unknown because the call stopped, and then it says so.
    if let Some(stop) = &probe.stopped {
        lines.push(stop.clone());
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------------------------
// Clearing a text field
// ---------------------------------------------------------------------------------------------

/// The key under which W3C WebDriver (and WebDriverAgent) name an element in a reply; older
/// builds also use `ELEMENT`.
const W3C_ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

/// A literal in an NSPredicate format string: single-quoted, with a backslash or a single quote
/// inside it escaped by a backslash.
fn predicate_literal(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('\'');
    for character in text.chars() {
        if matches!(character, '\\' | '\'') {
            literal.push('\\');
        }
        literal.push(character);
    }
    literal.push('\'');
    literal
}

/// The predicate that finds the field `role` whose text is `value` (and whose label is `label`,
/// when one is given); `role` is already one of the three a field can have.
fn field_predicate(role: &str, value: &str, label: Option<&str>) -> String {
    let mut predicate = format!(
        "type == 'XCUIElementType{role}' AND value == {}",
        predicate_literal(value)
    );
    if let Some(label) = label {
        predicate.push_str(&format!(" AND label == {}", predicate_literal(label)));
    }
    predicate
}

/// What is wrong with asking for this field before anything is sent: only a text field, a text
/// view or a search field has text to clear this way, a password field never, and text with
/// control characters cannot be told in a predicate.
fn check_field(role: &str, value: &str, label: Option<&str>) -> Result<(), OpError> {
    match role {
        "TextField" | "TextView" | "SearchField" => {}
        "SecureTextField" => {
            return Err(OpError::Said(
                "That is a password field. Its text is hidden, so it cannot be found to clear, and a password is the user's to enter; ask the user to clear it."
                    .to_owned(),
            ))
        }
        other => {
            return Err(OpError::Said(format!(
                "Only a text field, text view or search field can be cleared this way, not a {}.",
                sanitize_for_message(other)
            )))
        }
    }
    if value.is_empty() {
        return Err(OpError::Said(
            "The field's text is empty: there is nothing to clear, and nothing to find it by."
                .to_owned(),
        ));
    }
    if value.chars().any(char::is_control)
        || label.is_some_and(|label| label.chars().any(char::is_control))
    {
        return Err(OpError::Said(
            "The field's text has line breaks or other control characters, which a search by its text cannot match, so it was not cleared."
                .to_owned(),
        ));
    }
    Ok(())
}

/// The element a search found, when it found exactly one: its id, format-checked.
fn only_element(found: &Value) -> Result<String, OpError> {
    let Value::Array(elements) = found else {
        return Err(OpError::Said(
            "WebDriverAgent's answer to the search was not a list of elements; nothing was cleared."
                .to_owned(),
        ));
    };
    match elements.as_slice() {
        [] => Err(OpError::Said(
            "No field on the screen has exactly that text, so nothing was cleared. The screen may have changed since the snapshot: take a new preview_snapshot."
                .to_owned(),
        )),
        [element] => {
            let id = element
                .get(W3C_ELEMENT_KEY)
                .or_else(|| element.get("ELEMENT"))
                .and_then(Value::as_str);
            match id {
                Some(id) if valid_element_id(id) => Ok(id.to_owned()),
                _ => Err(OpError::Said(
                    "WebDriverAgent found the field but named it in a way that cannot be used safely in a route, so nothing was cleared."
                        .to_owned(),
                )),
            }
        }
        several => Err(OpError::Said(format!(
            "{} fields on the screen have exactly that text, so which one to clear is not certain and nothing was cleared. Tap the one you mean and press Backspace with preview_input instead.",
            several.len()
        ))),
    }
}

/// Clears the text field of `role` (`TextField`, `TextView` or `SearchField`: the element type
/// without its `XCUIElementType` prefix) whose current text is `value` and, when given, whose
/// label is `label`, through WebDriverAgent's own clear. It is found with `POST /elements` by a
/// predicate on its type, value and label, each literal escaped, and **exactly one** element must
/// match: none or several is said and nothing is cleared. The id the device returns is
/// format-checked before it is spliced into `/element/<id>/clear`.
///
/// For `preview_fill` replacing what a field holds ([`crate::remote_control::session`]'s
/// `replace_ios_field`), which has read `value` from the tree and checked it is whole. A password
/// field is refused. Every refusal that is an [`OpError::Said`] is made before the clear was asked
/// for, so nothing was cleared; only a driver's own error leaves it unknown what the device did.
pub(crate) fn clear_field(
    driver: &dyn Driver,
    step: &Step<'_>,
    role: &str,
    value: &str,
    label: Option<&str>,
) -> Result<(), OpError> {
    check_field(role, value, label)?;
    let Some(control) = driver.ios() else {
        return Err(OpError::Said(NOT_AN_IPHONE.to_owned()));
    };
    let wda = Wda { control, step };
    let found = wda.require(
        "POST",
        WdaRoute::Session("/elements"),
        Some(&find_body(&field_predicate(role, value, label))),
        FIND_CALL,
    )?;
    let id = only_element(&found)?;
    let route = format!("/element/{id}/clear");
    wda.require(
        "POST",
        WdaRoute::Session(&route),
        Some(&json!({})),
        CLEAR_CALL,
    )?;
    Ok(())
}
