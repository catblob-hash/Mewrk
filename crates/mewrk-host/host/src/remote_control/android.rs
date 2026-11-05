//! What the screen tools do on an Android device connected over adb: `screen_app`,
//! `screen_device` and `screen_package`, `preview_dialog`'s button press, and the platform's own
//! `android_logcat` and `android_shell`.
//!
//! Each call is parsed here ([`AndroidCall::parse`], [`AndroidCall::dialog`]) before anything
//! reaches the device: every parameter of the tool's schema, with the schema's bounds
//! (`builtin_schemas.rs` states the same numbers); a parameter the tool does not have refused by
//! name, one that does not apply to the chosen action refused with the action named, one that is
//! for another kind of screen (an iPhone's launch arguments) refused with that said, and every
//! value that ends up in a command checked against what it may be — a package name, an activity
//! class, a permission, a log tag, a URL without spaces. A call declares what it does to the
//! screen ([`AndroidCall::effect`], which decides the input lock and the read-only rule in
//! `session::platform`) and how long it may take ([`AndroidCall::time`]), and runs on the
//! session's driver through [`Driver::android`] ([`AndroidControl`]): device-shell commands, and
//! the streamed install. The dialog showing is read from the driver's element tree, the same
//! `uiautomator dump` a snapshot reads, and a button is pressed with a tap on its centre.
//!
//! The commands are built by small pure functions, so what reaches the
//! device's `sh -c` can be read off in tests: every value in them is one
//! single-quoted word ([`shell_quote`]) even after it was validated, and every
//! number is formatted from an integer. What comes back is read by parsers as
//! small, written for the formats of the Android versions in the field, which
//! differ (`dumpsys window windows` before Android 10 and `dumpsys window
//! displays` since; `firstInstallTime` per package, then per user). Every read
//! is bounded — a log of at most 10000 lines, 1 MiB of a package's dump, 30
//! notifications — and whatever the device printed reaches the model with its
//! control characters taken out.
//!
//! An operation of several commands asks its [`Step`] before each one, so a
//! stopped or out-of-time call ends between commands, never inside one it
//! could not finish, and says what it had done.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{Seek, SeekFrom},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    model::JsonObject,
    preview_launch_config::sanitize_for_message,
    remote_control::{
        adb::shell_quote,
        driver::{
            is_hidden_format, ui_tree_timeout, AndroidControl, Driver, DriverError, ShellResult,
            UiNode, UI_TREE_READ,
        },
        session::{Effect, OpError, Step},
    },
    tool_executor::screen_tools::{action_in, one_of, CallFiles, LocalFile, SCREEN_CALL_TIME},
};

mod apk;
#[cfg(test)]
mod tests;

/// `screen_package` `install` streams a whole APK, which can take minutes:
/// its deadline and the host's envelope around it.
const INSTALL_TIME: (Duration, Duration) = (Duration::from_secs(300), Duration::from_secs(305));

// What the schemas leave to the executor: the defaults, and the longest package name.
const PACKAGE_MAX: usize = 255;
const LIST_LIMIT_DEFAULT: u64 = 200;
const LOG_LINES_DEFAULT: u64 = 200;
const SHELL_TIMEOUT_DEFAULT_MS: u64 = 10_000;

/// What `android_shell` keeps of each of its command's streams.
const SHELL_KEEP: usize = 32 * 1024;
/// A list the device prints: its packages, its launcher activities.
const LIST_READ: usize = 1024 * 1024;
/// An answer of a line or a few.
const SHORT_READ: usize = 64 * 1024;
/// `dumpsys package <pkg>`, at most.
const PACKAGE_DUMP_READ: usize = 1024 * 1024;
/// `dumpsys window`, `power`, `notification`, `getprop`: long on a busy device.
const DUMP_READ: usize = 4 * 1024 * 1024;
const LOG_READ: usize = 8 * 1024 * 1024;
/// The newest log lines searched when a filter picks lines out of them (`package` by uid, `tag`,
/// `level`, `contains`): logcat counts its `-t` before it applies them.
const LOG_FETCH_MAX: u64 = 10_000;
/// A log line is cut here.
const LOG_LINE_CHARS: usize = 1000;
const NOTIFICATIONS_SHOWN: usize = 30;
const NOTIFICATION_CHARS: usize = 300;
const PERMISSIONS_SHOWN: usize = 100;
/// The largest APK `install` streams; pm's `-S` is a byte count the device takes as an int64,
/// but nothing that large is an app.
const APK_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A command that answers at once may take this long at most (a JVM start on an old device).
const COMMAND_TIME: Duration = Duration::from_secs(15);
/// A dump of a system service, or the log.
const DUMP_TIME: Duration = Duration::from_secs(20);
/// Less than this left of the call, and no command is started: it could not come back.
const MIN_COMMAND_TIME: Duration = Duration::from_millis(300);
/// While an app is coming up the window manager may name no focus at all; it is asked again
/// for this long.
const FOCUS_PATIENCE: Duration = Duration::from_secs(2);
const FOCUS_POLL: Duration = Duration::from_millis(250);
/// How long the keyguard gets to go after `wm dismiss-keyguard`.
const UNLOCK_SETTLE: Duration = Duration::from_millis(500);
/// How long the keyguard gets to come up after the sleep key.
const LOCK_SETTLE: Duration = Duration::from_millis(500);
/// How long the input method gets to hide after Back.
const KEYBOARD_SETTLE: Duration = Duration::from_millis(300);
/// How long a dialog gets to go (or the next one to come up) after one of its buttons was pressed.
const DIALOG_SETTLE: Duration = Duration::from_millis(400);
/// The most buttons of a dialog an answer lists.
const BUTTONS_SHOWN: usize = 24;

/// `cmd package query-activities` arrived with Android 7.1.
const QUERY_ACTIVITIES_MIN_SDK: u32 = 25;
/// `cmd` itself arrived with Android 7.0.
const CMD_MIN_SDK: u32 = 24;
/// Android 10 moved the focus out of `dumpsys window windows` into `dumpsys window displays`,
/// and added `cmd window user-rotation`.
const DISPLAYS_DUMP_MIN_SDK: u32 = 29;
const USER_ROTATION_MIN_SDK: u32 = 29;
/// `logcat --uid` arrived with Android 11.
const LOGCAT_UID_MIN_SDK: u32 = 30;
/// Android 9 made pm replace an installed app by default (`-r` is ignored); `-R` refuses to.
const INSTALL_NO_REPLACE_MIN_SDK: u32 = 28;

/// The line that starts each part of a command of several ([`marked`]).
const MARK: &str = "@@mewrk:";

// What reading each part of `screen_device` `info` is called, in its answers.
const DEVICE_READ: &str = "reading the device's properties";
const DISPLAY_READ: &str = "reading the display's state";
const POWER_READ: &str = "reading the power state";
const NOTIFICATIONS_READ: &str = "reading the notifications";
const DIALOG_READ: &str = "reading the dialog showing";

const LAUNCHERS_COMMAND: &str = "cmd package query-activities --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER";
const POLICY_COMMAND: &str = "dumpsys window policy";
const NOT_ANDROID: &str = "This screen is not an Android device.";
const LOCKED_WITH_CREDENTIAL: &str = "The device is locked with a PIN, pattern or password. Ask the user to unlock it; never type a credential.";
const NATURAL_ORIENTATION_UNKNOWN: &str = "Could not read the display's size (wm size), so its natural orientation, which the device's rotation numbers count from, is unknown. Nothing was changed.";
const NO_FRAMED_SHELL: &str =
    "exit status unknown (this device has no framed shell; stderr is mixed into stdout)";
const STOPPED_STATUS: &str = "exit status unknown (the command had not ended when it was stopped)";

// ------------------------------------------------------------- parameters

/// `screen_app`'s actions; the first is the default.
const APP_ACTIONS: &[&str] = &[
    "list",
    "foreground",
    "info",
    "launch",
    "activate",
    "stop",
    "open_url",
];
const PACKAGE_ACTIONS: &[&str] = &["install", "uninstall", "clear_data", "grant", "revoke"];
/// `screen_device`'s actions; the first is the default.
const DEVICE_ACTIONS: &[&str] = &[
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
];
const SCOPES: &[&str] = &["launchable", "third_party", "all"];
const LEVELS: &[&str] = &["V", "D", "I", "W", "E", "F"];
const BUFFERS: &[&str] = &["main", "system", "crash", "events", "all"];
const SECTIONS: &[&str] = &["device", "display", "power", "notifications"];
/// `rotate`'s values, by how the device is turned from portrait.
const ROTATIONS: &[&str] = &[
    "portrait",
    "landscape_left",
    "landscape_right",
    "portrait_upside_down",
    "auto",
];
const SWITCHES: &[&str] = &["on", "off"];
const APPEARANCES: &[&str] = &["light", "dark", "auto"];

/// What the parameters that are an iPhone's alone (`screen_app` `arguments` and `environment`)
/// are answered with on an Android device.
const ARGUMENTS_NOT_HERE: &str = "arguments is for an iPhone or iPad; an Android app is started with launch (activity picks the screen to start) or open_url (a link for an app to open).";
const ENVIRONMENT_NOT_HERE: &str = "environment is for an iPhone or iPad; an Android app has no launch environment to set. Use launch with activity to start a screen of it, or open_url to hand it a link.";
/// `screen_device` `location` and `clear_location`: an emulator's location is set through its
/// console (`adb emu geo fix`), a channel this build does not use, and a real device has none.
const LOCATION_NOT_HERE: &str = "A simulated location on an Android device needs the emulator console, which this build does not use; set it in the emulator's own location controls instead.";
const CLEAR_LOCATION_NOT_HERE: &str = "There is no simulated location to clear on an Android device: setting one needs the emulator console, which this build does not use. Reset it in the emulator's own location controls instead.";
/// `preview_dialog` on an Android device presses a button by its label.
const ACCEPT_NOT_HERE: &str = "An Android dialog is answered by its button's label: pass button; screen_device info lists the buttons.";
const PROMPT_TEXT_NOT_HERE: &str = "preview_dialog prompt_text is for an iPhone or iPad; the text field of an Android dialog is filled with preview_fill (find it with preview_snapshot), and its button is pressed with button.";

/// What a parameter holds, with the schema's bounds.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// The tool's `action`, which [`action_in`] reads.
    Action,
    /// A string of 1 to this many characters.
    Text(usize),
    /// A whole number in this range.
    Integer(u64, u64),
    /// A number in this range, not necessarily whole.
    Number(i32, i32),
    Flag,
    /// One of these strings.
    Choice(&'static [&'static str]),
    /// `screen_device`'s `sections`: 1 to 4 of [`SECTIONS`], each at most once.
    Sections,
    /// A parameter the schema has for another kind of screen: whatever it holds, a call that
    /// gives it is refused with this.
    NotHere(&'static str),
}

/// One parameter of a tool, and the actions it applies to (`None`: every one, or a tool
/// without actions).
#[derive(Clone, Copy, Debug)]
struct Param {
    key: &'static str,
    kind: Kind,
    actions: Option<&'static [&'static str]>,
}

const fn param(key: &'static str, kind: Kind, actions: Option<&'static [&'static str]>) -> Param {
    Param { key, kind, actions }
}

const APP_PARAMS: &[Param] = &[
    param("action", Kind::Action, None),
    param(
        "app",
        Kind::Text(PACKAGE_MAX),
        Some(&["info", "launch", "activate", "stop", "open_url"]),
    ),
    param("url", Kind::Text(2048), Some(&["open_url"])),
    param("filter", Kind::Text(200), Some(&["list"])),
    param("limit", Kind::Integer(1, 1000), Some(&["list"])),
    param("scope", Kind::Choice(SCOPES), Some(&["list"])),
    param("activity", Kind::Text(255), Some(&["launch"])),
    param("restart", Kind::Flag, Some(&["launch"])),
    param("arguments", Kind::NotHere(ARGUMENTS_NOT_HERE), None),
    param("environment", Kind::NotHere(ENVIRONMENT_NOT_HERE), None),
];
const PACKAGE_PARAMS: &[Param] = &[
    param("action", Kind::Action, None),
    param("path", Kind::Text(4096), Some(&["install"])),
    param(
        "app",
        Kind::Text(PACKAGE_MAX),
        Some(&["uninstall", "clear_data", "grant", "revoke"]),
    ),
    param("permission", Kind::Text(255), Some(&["grant", "revoke"])),
    param("keep_data", Kind::Flag, Some(&["uninstall"])),
    param("replace_existing", Kind::Flag, Some(&["install"])),
    param("downgrade", Kind::Flag, Some(&["install"])),
    param("grant_permissions", Kind::Flag, Some(&["install"])),
];
const LOGCAT_PARAMS: &[Param] = &[
    param("lines", Kind::Integer(1, 2000), None),
    param("level", Kind::Choice(LEVELS), None),
    param("tag", Kind::Text(128), None),
    param("package", Kind::Text(PACKAGE_MAX), None),
    param("buffer", Kind::Choice(BUFFERS), None),
    param("contains", Kind::Text(200), None),
];
const DEVICE_PARAMS: &[Param] = &[
    param("action", Kind::Action, None),
    param(
        "value",
        Kind::Text(32),
        Some(&["rotate", "appearance", "animations", "stay_awake"]),
    ),
    param("sections", Kind::Sections, Some(&["info"])),
    param("latitude", Kind::Number(-90, 90), Some(&["location"])),
    param("longitude", Kind::Number(-180, 180), Some(&["location"])),
];
const SHELL_PARAMS: &[Param] = &[
    param("command", Kind::Text(8192), None),
    param("timeout", Kind::Integer(1000, 20_000), None),
];
/// `preview_dialog` as an Android device takes it: the button to press.
const DIALOG_PARAMS: &[Param] = &[
    param("accept", Kind::NotHere(ACCEPT_NOT_HERE), None),
    param("prompt_text", Kind::NotHere(PROMPT_TEXT_NOT_HERE), None),
    param("button", Kind::Text(256), None),
];

fn params(tool: &str) -> &'static [Param] {
    match tool {
        "screen_app" => APP_PARAMS,
        "screen_package" => PACKAGE_PARAMS,
        "screen_device" => DEVICE_PARAMS,
        "android_logcat" => LOGCAT_PARAMS,
        "preview_dialog" => DIALOG_PARAMS,
        _ => SHELL_PARAMS,
    }
}

/// The `action` values each tool takes, and whether it must be given.
fn actions(tool: &str) -> Option<(&'static [&'static str], bool)> {
    match tool {
        "screen_app" => Some((APP_ACTIONS, false)),
        "screen_package" => Some((PACKAGE_ACTIONS, true)),
        "screen_device" => Some((DEVICE_ACTIONS, false)),
        _ => None,
    }
}

fn text_bound(key: &str, max: usize) -> String {
    format!("\"{key}\" must be a string of 1 to {max} characters.")
}

fn integer_bound(key: &str, min: u64, max: u64) -> String {
    format!("\"{key}\" must be a whole number from {min} to {max}.")
}

fn number_bound(key: &str, min: i32, max: i32) -> String {
    format!("\"{key}\" must be a number from {min} to {max}.")
}

fn flag_bound(key: &str) -> String {
    format!("\"{key}\" must be true or false.")
}

fn choice_bound(key: &str, values: &[&str], got: &Value) -> String {
    match got.as_str() {
        Some(got) => format!(
            "\"{key}\" must be {}, not \"{}\".",
            one_of(values),
            sanitize_for_message(got)
        ),
        None => format!("\"{key}\" must be {}.", one_of(values)),
    }
}

fn sections_bound() -> String {
    format!(
        "\"sections\" must be a list of 1 to 4 of {}, each at most once.",
        and_list(SECTIONS)
    )
}

/// `values` as a sentence's list: `a, b and c`.
fn and_list(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A whole number, also when JSON wrote it as `200.0`.
fn whole(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.fract() == 0.0 && *number >= 0.0 && *number <= 1e15)
            .map(|number| number as u64)
    })
}

fn sections_of(value: &Value) -> Result<Vec<Section>, String> {
    let items = value.as_array().ok_or_else(sections_bound)?;
    if items.is_empty() || items.len() > SECTIONS.len() {
        return Err(sections_bound());
    }
    let mut sections = Vec::with_capacity(items.len());
    for item in items {
        let section = match item.as_str() {
            Some("device") => Section::Device,
            Some("display") => Section::Display,
            Some("power") => Section::Power,
            Some("notifications") => Section::Notifications,
            _ => return Err(sections_bound()),
        };
        if sections.contains(&section) {
            return Err(sections_bound());
        }
        sections.push(section);
    }
    Ok(sections)
}

/// A call's arguments, its keys already known to be the tool's.
struct Args<'a> {
    tool: &'static str,
    input: &'a JsonObject,
    params: &'static [Param],
}

impl<'a> Args<'a> {
    /// Refuses a key the tool does not have. `serverId` is every screen tool's (the tool layer
    /// reads it), and `workspace` is `screen_package`'s when the conversation has several
    /// (the host reads it, for the APK's path).
    fn new(tool: &'static str, input: &'a JsonObject) -> Result<Self, String> {
        let params = params(tool);
        let unknown = input.keys().find(|key| {
            let key = key.as_str();
            !(key == "serverId"
                || (tool == "screen_package" && key == "workspace")
                || params.iter().any(|param| param.key == key))
        });
        if let Some(key) = unknown {
            let mut taken = vec!["serverId"];
            taken.extend(params.iter().map(|param| param.key));
            return Err(format!(
                "{tool} has no parameter \"{}\"; it takes {}.",
                sanitize_for_message(key),
                and_list(&taken)
            ));
        }
        Ok(Self {
            tool,
            input,
            params,
        })
    }

    /// The value of `key`, when it is there and not `null`.
    fn value(&self, key: &str) -> Option<&'a Value> {
        self.input.get(key).filter(|value| !value.is_null())
    }

    /// A parameter for another kind of screen is refused first, whatever it holds; then every
    /// given parameter is of its kind and within its bounds.
    fn check(&self) -> Result<(), String> {
        for param in self.params {
            if let Kind::NotHere(refusal) = param.kind {
                if self.value(param.key).is_some() {
                    return Err(refusal.to_owned());
                }
            }
        }
        for param in self.params {
            let Some(value) = self.value(param.key) else {
                continue;
            };
            let fits = match param.kind {
                Kind::Action | Kind::NotHere(_) => true,
                Kind::Text(max) => value
                    .as_str()
                    .is_some_and(|text| (1..=max).contains(&text.chars().count())),
                Kind::Integer(min, max) => whole(value).is_some_and(|n| (min..=max).contains(&n)),
                Kind::Number(min, max) => value.as_f64().is_some_and(|n| {
                    n.is_finite() && (f64::from(min)..=f64::from(max)).contains(&n)
                }),
                Kind::Flag => value.is_boolean(),
                Kind::Choice(values) => value.as_str().is_some_and(|text| values.contains(&text)),
                Kind::Sections => {
                    sections_of(value)?;
                    true
                }
            };
            if !fits {
                return Err(match param.kind {
                    Kind::Text(max) => text_bound(param.key, max),
                    Kind::Integer(min, max) => integer_bound(param.key, min, max),
                    Kind::Number(min, max) => number_bound(param.key, min, max),
                    Kind::Flag => flag_bound(param.key),
                    Kind::Choice(values) => choice_bound(param.key, values, value),
                    Kind::Action | Kind::NotHere(_) | Kind::Sections => sections_bound(),
                });
            }
        }
        Ok(())
    }

    /// Refuses a parameter that does not apply to `action`.
    fn applies(&self, action: &str) -> Result<(), String> {
        for param in self.params {
            if let Some(actions) = param.actions {
                if self.value(param.key).is_some() && !actions.contains(&action) {
                    return Err(format!(
                        "\"{}\" does not apply to {} {action}; it is for {}.",
                        param.key,
                        self.tool,
                        one_of(actions)
                    ));
                }
            }
        }
        Ok(())
    }

    fn text(&self, key: &str) -> Option<&'a str> {
        self.value(key).and_then(Value::as_str)
    }

    fn integer(&self, key: &str) -> Option<u64> {
        self.value(key).and_then(whole)
    }

    fn flag(&self, key: &str) -> Option<bool> {
        self.value(key).and_then(Value::as_bool)
    }

    fn choice(&self, key: &str) -> Option<&'static str> {
        let given = self.text(key)?;
        self.params.iter().find_map(|param| match param.kind {
            Kind::Choice(values) if param.key == key => {
                values.iter().copied().find(|value| *value == given)
            }
            _ => None,
        })
    }

    /// `key`, which `action` (or the tool) cannot do without; `hint` says what it is.
    fn required(&self, key: &str, action: Option<&str>, hint: &str) -> Result<&'a str, String> {
        self.text(key).ok_or_else(|| match action {
            Some(action) => format!("{} {action} needs \"{key}\": {hint}", self.tool),
            None => format!("{} needs \"{key}\": {hint}", self.tool),
        })
    }
}

/// `^[A-Za-z][A-Za-z0-9_]*(\.[A-Za-z0-9_]+)+$`: two or more parts, the first starting with a
/// letter.
fn is_package(value: &str) -> bool {
    let word = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    };
    let mut parts = value.split('.');
    let first = parts.next().unwrap_or_default();
    let mut rest = 0usize;
    for part in parts {
        if !word(part) {
            return false;
        }
        rest += 1;
    }
    word(first) && first.starts_with(|character: char| character.is_ascii_alphabetic()) && rest > 0
}

/// `^[A-Za-z0-9_.$]+$`: `.MainActivity`, `com.example.app.MainActivity`, `Outer$Inner`.
fn is_activity(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '$')
        })
}

/// `^[A-Za-z0-9_.]+$`.
fn is_permission(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.'))
}

/// `^[^-\s:*'"\\][^\s:*'"\\]*$`: one word that cannot change a logcat filter spec, nor be read
/// as one of logcat's options.
fn is_tag(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.chars().any(|character| {
            character.is_whitespace() || matches!(character, ':' | '*' | '\'' | '"' | '\\')
        })
}

/// A link has no whitespace or control character, and none of the bidirectional controls and
/// zero-width characters ([`is_hidden_format`]) that make a link read as another than it is.
fn is_url(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|character| {
            character.is_whitespace() || character.is_control() || is_hidden_format(character)
        })
}

/// `value`, which `key` (`app`, or `android_logcat`'s `package`) holds, as a package name.
fn valid_package(key: &str, value: &str) -> Result<String, String> {
    if is_package(value) {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "\"{key}\" must be a package name such as com.example.app (two or more dot-separated parts of letters, digits and underscores, starting with a letter), not \"{}\".",
            sanitize_for_message(value)
        ))
    }
}

fn valid_activity(value: &str) -> Result<String, String> {
    if is_activity(value) {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "\"activity\" must be an activity class such as .MainActivity or com.example.app.MainActivity (letters, digits, _, . and $), not \"{}\".",
            sanitize_for_message(value)
        ))
    }
}

fn valid_permission(value: &str) -> Result<String, String> {
    if is_permission(value) {
        Ok(value.to_owned())
    } else {
        Err(format!(
            "\"permission\" must be a permission name such as android.permission.CAMERA (letters, digits, _ and .), not \"{}\".",
            sanitize_for_message(value)
        ))
    }
}

fn valid_tag(value: &str) -> Result<String, String> {
    if is_tag(value) {
        Ok(value.to_owned())
    } else {
        Err("\"tag\" must be one log tag, without spaces, colons, asterisks, quotes or backslashes, and not starting with -.".to_owned())
    }
}

fn valid_url(value: &str) -> Result<String, String> {
    if is_url(value) {
        Ok(value.to_owned())
    } else {
        Err("\"url\" must have no spaces, control characters, or invisible or text-direction characters; percent-encode them (a space is %20).".to_owned())
    }
}

// ------------------------------------------------------------- the calls

/// One call of an Android tool, parsed.
#[derive(Debug)]
pub(crate) struct AndroidCall {
    /// For the session's error prefix, and for the budget's refusals: "launching com.foo".
    what: String,
    call: Call,
}

#[derive(Debug)]
enum Call {
    Apps(Apps),
    App(App),
    Package(Package),
    Logcat(Logcat),
    Info(Vec<Section>),
    Settings(Setting),
    /// `preview_dialog`: the label of the button to press.
    Dialog {
        button: String,
    },
    Shell {
        command: String,
        timeout: Duration,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Launchable,
    ThirdParty,
    All,
}

/// What `screen_app` reads: the Look half of it.
#[derive(Debug)]
enum Apps {
    List {
        scope: Scope,
        filter: Option<String>,
        limit: usize,
    },
    Foreground,
    Info {
        package: String,
    },
}

/// What `screen_app` does to an app: the Act half of it.
#[derive(Debug)]
enum App {
    Launch {
        package: String,
        activity: Option<String>,
        restart: bool,
    },
    /// Brings a running app to the front: on Android that is a launch.
    Activate {
        package: String,
    },
    Stop {
        package: String,
    },
    OpenUrl {
        url: String,
        package: Option<String>,
    },
}

/// The APK an install streams: the open file (proved to be the path that was checked), its
/// name and size.
#[derive(Debug)]
struct Apk {
    file: File,
    name: String,
    size: u64,
}

#[derive(Debug)]
enum Package {
    Install {
        apk: Apk,
        /// Whether an installed version may be replaced; the option word for it depends on the
        /// device's Android version.
        replace_existing: bool,
        /// pm's other option words, in order: `-t`, `-d`, `-g`.
        flags: Vec<&'static str>,
    },
    Uninstall {
        package: String,
        keep_data: bool,
    },
    ClearData {
        package: String,
    },
    Permission {
        grant: bool,
        package: String,
        permission: String,
    },
}

#[derive(Debug)]
struct Logcat {
    lines: u64,
    level: Option<&'static str>,
    tag: Option<String>,
    package: Option<String>,
    buffer: Option<&'static str>,
    contains: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Device,
    Display,
    Power,
    Notifications,
}

/// How the device is turned, from portrait: what `screen_device` `rotate` is told, whatever the
/// display's natural orientation is ([`surface_rotation`] converts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Orientation {
    Portrait,
    /// Turned a quarter counter-clockwise from portrait.
    LandscapeLeft,
    /// Turned a quarter clockwise from portrait.
    LandscapeRight,
    PortraitUpsideDown,
}

impl Orientation {
    fn name(self) -> &'static str {
        match self {
            Self::Portrait => ROTATIONS[0],
            Self::LandscapeLeft => ROTATIONS[1],
            Self::LandscapeRight => ROTATIONS[2],
            Self::PortraitUpsideDown => ROTATIONS[3],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setting {
    Wake,
    /// The sleep key; whether that locks is the device's setting, which the answer reads.
    Lock,
    Unlock,
    /// `None` is auto-rotate.
    Rotate(Option<Orientation>),
    /// `cmd uimode night`'s word: `yes`, `no` or `auto`.
    Appearance(&'static str),
    Animations(bool),
    /// `cmd statusbar`'s subcommand.
    Shade(&'static str),
    StayAwake(bool),
    DismissKeyboard,
}

impl Setting {
    fn what(self) -> &'static str {
        match self {
            Self::Wake => "waking the device",
            Self::Lock => "putting the device to sleep",
            Self::Unlock => "unlocking the device",
            Self::Rotate(_) => "rotating the screen",
            Self::Appearance(_) => "switching appearance",
            Self::Animations(_) => "switching animations",
            Self::Shade("expand-notifications") => "opening the notification shade",
            Self::Shade("expand-settings") => "opening quick settings",
            Self::Shade(_) => "collapsing the notification shade",
            Self::StayAwake(_) => "switching stay-awake",
            Self::DismissKeyboard => "dismissing the keyboard",
        }
    }
}

impl Call {
    fn what(&self) -> String {
        match self {
            Self::Apps(Apps::List { .. }) => "listing the apps".to_owned(),
            Self::Apps(Apps::Foreground) => "reading the app in front".to_owned(),
            Self::Apps(Apps::Info { package }) => format!("reading {package}'s details"),
            Self::App(App::Launch { package, .. }) => format!("launching {package}"),
            Self::App(App::Activate { package }) => format!("activating {package}"),
            Self::App(App::Stop { package }) => format!("stopping {package}"),
            Self::App(App::OpenUrl { url, .. }) => format!("opening {}", brief(url, 80)),
            Self::Package(Package::Install { apk, .. }) => format!("installing {}", apk.name),
            Self::Package(Package::Uninstall { package, .. }) => format!("uninstalling {package}"),
            Self::Package(Package::ClearData { package }) => format!("erasing {package}'s data"),
            Self::Package(Package::Permission {
                grant: true,
                package,
                permission,
            }) => format!("granting {permission} to {package}"),
            Self::Package(Package::Permission {
                package,
                permission,
                ..
            }) => format!("revoking {permission} from {package}"),
            Self::Logcat(_) => "reading the log".to_owned(),
            Self::Info(_) => "reading the device's state".to_owned(),
            Self::Settings(setting) => setting.what().to_owned(),
            Self::Dialog { button } => {
                format!("pressing \"{}\"", brief(&sanitize_for_message(button), 80))
            }
            Self::Shell { .. } => "running the shell command".to_owned(),
        }
    }
}

const PACKAGE_HINT: &str = "the package name, such as com.example.app.";

/// What `screen_app` reads, when `action` is one of the three that only look.
fn parse_apps(args: &Args<'_>, action: &str) -> Result<Apps, String> {
    Ok(match action {
        "foreground" => Apps::Foreground,
        "info" => Apps::Info {
            package: valid_package(
                "app",
                args.required(
                    "app",
                    Some("info"),
                    "the package name, such as com.android.settings.",
                )?,
            )?,
        },
        _ => Apps::List {
            scope: match args.choice("scope") {
                Some("third_party") => Scope::ThirdParty,
                Some("all") => Scope::All,
                _ => Scope::Launchable,
            },
            filter: args.text("filter").map(str::to_owned),
            limit: args.integer("limit").unwrap_or(LIST_LIMIT_DEFAULT) as usize,
        },
    })
}

/// What `screen_app` does, when `action` is one of the four that act.
fn parse_app(args: &Args<'_>, action: &str) -> Result<App, String> {
    let package = || valid_package("app", args.required("app", Some(action), PACKAGE_HINT)?);
    Ok(match action {
        "launch" => App::Launch {
            package: package()?,
            activity: args.text("activity").map(valid_activity).transpose()?,
            restart: args.flag("restart").unwrap_or(false),
        },
        "activate" => App::Activate {
            package: package()?,
        },
        "stop" => App::Stop {
            package: package()?,
        },
        _ => App::OpenUrl {
            url: valid_url(args.required("url", Some(action), "the URL or deep link to open.")?)?,
            package: args
                .text("app")
                .map(|app| valid_package("app", app))
                .transpose()?,
        },
    })
}

fn parse_package(args: &Args<'_>, action: &str, files: &CallFiles) -> Result<Package, String> {
    let package = || -> Result<String, String> {
        valid_package("app", args.required("app", Some(action), PACKAGE_HINT)?)
    };
    Ok(match action {
        "install" => {
            let path = args.required(
                "path",
                Some(action),
                "the .apk file, relative to the workspace or absolute.",
            )?;
            let mut flags = Vec::with_capacity(3);
            // Test-only builds (a debug build from Android Studio) are what an agent installs most.
            flags.push("-t");
            if args.flag("downgrade").unwrap_or(false) {
                flags.push("-d");
            }
            if args.flag("grant_permissions").unwrap_or(false) {
                flags.push("-g");
            }
            Package::Install {
                apk: open_apk(files, path)?,
                replace_existing: args.flag("replace_existing").unwrap_or(true),
                flags,
            }
        }
        "uninstall" => Package::Uninstall {
            package: package()?,
            keep_data: args.flag("keep_data").unwrap_or(false),
        },
        "clear_data" => Package::ClearData {
            package: package()?,
        },
        _ => Package::Permission {
            grant: action == "grant",
            package: package()?,
            permission: valid_permission(args.required(
                "permission",
                Some(action),
                "the permission, such as android.permission.POST_NOTIFICATIONS.",
            )?)?,
        },
    })
}

/// The APK `requested` names, opened the way `read` opens a file (the call's workspace and scope,
/// this computer only): a regular file ending in `.apk`, not empty, at most 2 GiB.
fn open_apk(files: &CallFiles, requested: &str) -> Result<Apk, String> {
    let LocalFile { file, path, size } = files.open_local_file(requested)?;
    let name = path
        .file_name()
        .map(|name| clean_line(&name.to_string_lossy()))
        .unwrap_or_else(|| clean_line(requested));
    if !name.to_ascii_lowercase().ends_with(".apk") {
        return Err(format!(
            "{} is not an .apk file; screen_package install takes an APK.",
            sanitize_for_message(&name)
        ));
    }
    if size == 0 {
        return Err(format!("{} is empty.", sanitize_for_message(&name)));
    }
    if size > APK_MAX_BYTES {
        return Err(format!(
            "{} is {}; screen_package install streams at most 2 GiB.",
            sanitize_for_message(&name),
            human_size(size)
        ));
    }
    Ok(Apk { file, name, size })
}

fn parse_logcat(args: &Args<'_>) -> Result<Logcat, String> {
    Ok(Logcat {
        lines: args.integer("lines").unwrap_or(LOG_LINES_DEFAULT),
        level: args.choice("level"),
        tag: args.text("tag").map(valid_tag).transpose()?,
        package: args
            .text("package")
            .map(|package| valid_package("package", package))
            .transpose()?,
        buffer: args.choice("buffer"),
        contains: args.text("contains").map(str::to_owned),
    })
}

fn parse_info(args: &Args<'_>) -> Result<Vec<Section>, String> {
    let mut sections = match args.value("sections") {
        Some(value) => sections_of(value)?,
        None => vec![Section::Device, Section::Display, Section::Power],
    };
    sections.sort();
    Ok(sections)
}

/// The `screen_device` actions that change the device. A simulated location is refused here:
/// it takes the emulator console, which this build does not use.
fn parse_settings(args: &Args<'_>, action: &str) -> Result<Setting, String> {
    let value = |values: &'static [&'static str]| -> Result<&'static str, String> {
        let given = args.text("value").ok_or_else(|| {
            format!(
                "screen_device {action} needs \"value\": {}.",
                one_of(values)
            )
        })?;
        values
            .iter()
            .copied()
            .find(|value| *value == given)
            .ok_or_else(|| {
                format!(
                    "\"value\" for {action} must be {}, not \"{}\".",
                    one_of(values),
                    sanitize_for_message(given)
                )
            })
    };
    Ok(match action {
        "wake" => Setting::Wake,
        "lock" => Setting::Lock,
        "unlock" => Setting::Unlock,
        "rotate" => Setting::Rotate(match value(ROTATIONS)? {
            "portrait" => Some(Orientation::Portrait),
            "landscape_left" => Some(Orientation::LandscapeLeft),
            "landscape_right" => Some(Orientation::LandscapeRight),
            "portrait_upside_down" => Some(Orientation::PortraitUpsideDown),
            _ => None,
        }),
        "appearance" => Setting::Appearance(match value(APPEARANCES)? {
            "dark" => "yes",
            "light" => "no",
            _ => "auto",
        }),
        "animations" => Setting::Animations(value(SWITCHES)? == "on"),
        "location" => return Err(LOCATION_NOT_HERE.to_owned()),
        "clear_location" => return Err(CLEAR_LOCATION_NOT_HERE.to_owned()),
        "notifications" => Setting::Shade("expand-notifications"),
        "quick_settings" => Setting::Shade("expand-settings"),
        "collapse" => Setting::Shade("collapse"),
        "dismiss_keyboard" => Setting::DismissKeyboard,
        _ => Setting::StayAwake(value(SWITCHES)? == "on"),
    })
}

fn parse_shell(args: &Args<'_>) -> Result<Call, String> {
    Ok(Call::Shell {
        command: args
            .required("command", None, "the shell command line.")?
            .to_owned(),
        timeout: Duration::from_millis(args.integer("timeout").unwrap_or(SHELL_TIMEOUT_DEFAULT_MS)),
    })
}

impl AndroidCall {
    /// Parses one call of `tool` from its JSON input; every bound in the schema is checked here
    /// with the same numbers. `files` resolves file paths (`screen_package` `install`).
    pub(crate) fn parse(tool: &str, input: &JsonObject, files: &CallFiles) -> Result<Self, String> {
        let tool = match tool {
            "screen_app" => "screen_app",
            "screen_device" => "screen_device",
            "screen_package" => "screen_package",
            "android_logcat" => "android_logcat",
            "android_shell" => "android_shell",
            _ => return Err(format!("{tool} is not an Android tool.")),
        };
        let args = Args::new(tool, input)?;
        let action = match actions(tool) {
            Some((values, required)) => action_in(input, values, required)?,
            None => None,
        };
        args.check()?;
        if let Some(action) = action {
            args.applies(action)?;
        }
        let call = match (tool, action) {
            ("screen_app", Some(action @ ("list" | "foreground" | "info"))) => {
                Call::Apps(parse_apps(&args, action)?)
            }
            ("screen_app", Some(action)) => Call::App(parse_app(&args, action)?),
            ("screen_package", Some(action)) => Call::Package(parse_package(&args, action, files)?),
            ("screen_device", Some("info")) => Call::Info(parse_info(&args)?),
            ("screen_device", Some(action)) => Call::Settings(parse_settings(&args, action)?),
            ("android_logcat", _) => Call::Logcat(parse_logcat(&args)?),
            ("android_shell", _) => parse_shell(&args)?,
            // Every tool with actions has a default, so there is always one.
            _ => return Err(format!("{tool} needs an action.")),
        };
        Ok(Self {
            what: call.what(),
            call,
        })
    }

    /// `preview_dialog` on an Android device: presses the button of the dialog showing whose label
    /// is `button`, when exactly one is. The dialog's buttons are read from the element tree
    /// ([`dialog_buttons`]). Only `button` answers an Android dialog: `accept` is a guess at a
    /// position, and a text field is filled with `preview_fill`.
    pub(crate) fn dialog(input: &JsonObject) -> Result<Self, String> {
        let args = Args::new("preview_dialog", input)?;
        args.check()?;
        let button = args
            .required(
                "button",
                None,
                "the exact label of the button to press; screen_device info lists the buttons.",
            )?
            .to_owned();
        let call = Call::Dialog { button };
        Ok(Self {
            what: call.what(),
            call,
        })
    }

    pub(crate) fn effect(&self) -> Effect {
        match &self.call {
            Call::Apps(_) | Call::Logcat(_) | Call::Info(_) => Effect::Look,
            Call::Package(_) => Effect::Background,
            // screen_app's launch, activate, stop and open_url, screen_device's changes, a dialog
            // button and android_shell: they change what the screen shows, or (a shell line)
            // nothing says they do not.
            Call::App(_) | Call::Settings(_) | Call::Dialog { .. } | Call::Shell { .. } => {
                Effect::Act
            }
        }
    }

    /// The call's deadline and envelope (default: `SCREEN_CALL_DEADLINE` / `PREVIEW_TOOL_TIMEOUT`).
    pub(crate) fn time(&self) -> (Duration, Duration) {
        match self.call {
            Call::Package(Package::Install { .. }) => INSTALL_TIME,
            _ => SCREEN_CALL_TIME,
        }
    }

    /// For the session's error prefix and logs: "launching com.foo".
    pub(crate) fn what(&self) -> String {
        self.what.clone()
    }

    pub(crate) fn run(&self, driver: &dyn Driver, step: &Step<'_>) -> Result<String, OpError> {
        let android = driver
            .android()
            .ok_or_else(|| OpError::Said(NOT_ANDROID.to_owned()))?;
        let label = clean_line(&driver.info().label);
        let device = Device {
            android,
            driver,
            step,
            label: if label.is_empty() {
                "the device".to_owned()
            } else {
                label
            },
        };
        match &self.call {
            Call::Apps(Apps::List {
                scope,
                filter,
                limit,
            }) => device.apps(*scope, filter.as_deref(), *limit),
            Call::Apps(Apps::Foreground) => device.foreground(),
            Call::Apps(Apps::Info { package }) => device.app_info(package),
            Call::App(App::Launch {
                package,
                activity,
                restart,
            }) => device.launch(package, activity.as_deref(), *restart),
            Call::App(App::Activate { package }) => device.launch(package, None, false),
            Call::App(App::Stop { package }) => device.stop(package),
            Call::App(App::OpenUrl { url, package }) => device.open_url(url, package.as_deref()),
            Call::Package(Package::Install {
                apk,
                replace_existing,
                flags,
            }) => device.install(apk, *replace_existing, flags),
            Call::Package(Package::Uninstall { package, keep_data }) => {
                device.uninstall(package, *keep_data)
            }
            Call::Package(Package::ClearData { package }) => device.clear_data(package),
            Call::Package(Package::Permission {
                grant,
                package,
                permission,
            }) => device.permission(*grant, package, permission),
            Call::Logcat(query) => device.logcat(query),
            Call::Info(sections) => device.info(sections),
            Call::Settings(setting) => device.setting(*setting),
            Call::Dialog { button } => device.press(button),
            Call::Shell { command, timeout } => device.shell_command(command, *timeout),
        }
    }
}

// ------------------------------------------------------------- running them

/// The device a run talks to, and the time it has.
struct Device<'a> {
    android: &'a dyn AndroidControl,
    /// The same device as a screen: its element tree, and its taps.
    driver: &'a dyn Driver,
    step: &'a Step<'a>,
    /// The device as its model names it, for the answers.
    label: String,
}

/// `error` of the step `what`, said after what was already done: "Force-stopped com.foo.
/// Cancelled before …", "Force-stopped com.foo. Then finding its launcher activity failed: …". A
/// device that went away stays the driver's error, so that the session reconnects.
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

impl Device<'_> {
    /// Runs `command` (`what` names it for the budget's refusals: "reading the log"), keeping
    /// `limit` bytes of each stream, within the call's time and at most `most`.
    fn shell(
        &self,
        what: &str,
        command: &str,
        limit: usize,
        most: Duration,
    ) -> Result<ShellResult, OpError> {
        self.step.proceed(what)?;
        if !self.step.fits(MIN_COMMAND_TIME) {
            return Err(OpError::Said(format!(
                "Ran out of this call's time before {what}."
            )));
        }
        let timeout = self.step.remaining().min(most);
        let result = self.android.shell(command, limit, timeout)?;
        // What a read that never ended printed is no answer, unless it was cut anyway.
        if result.timed_out && !result.truncated {
            return Err(OpError::Driver(DriverError::Failed(format!(
                "the command did not end within {} ms",
                timeout.as_millis()
            ))));
        }
        Ok(result)
    }

    /// What `command` printed, as text.
    fn output(&self, what: &str, command: &str, limit: usize) -> Result<String, OpError> {
        let most = if limit > SHORT_READ {
            DUMP_TIME
        } else {
            COMMAND_TIME
        };
        Ok(printed(&self.shell(what, command, limit, most)?))
    }

    /// A command that changes something and prints nothing (or a line) when it works; its exit
    /// status, else what it printed, says whether it did.
    fn change(&self, what: &str, command: &str) -> Result<String, OpError> {
        let result = self.shell(what, command, SHORT_READ, COMMAND_TIME)?;
        let text = printed(&result);
        match refusal(&result, &text) {
            Some(problem) => Err(OpError::Said(format!(
                "{} failed: {problem}",
                capitalized(what)
            ))),
            None => Ok(text),
        }
    }

    fn sdk(&self) -> Result<u32, OpError> {
        Ok(self.android.sdk()?)
    }

    /// Refuses a package the device does not have: `am force-stop` "succeeds" for any name.
    fn ensure_installed(&self, package: &str) -> Result<(), OpError> {
        let text = self.output(
            &format!("checking that {package} is installed"),
            &pm_path_command(package),
            SHORT_READ,
        )?;
        if text
            .lines()
            .any(|line| line.trim_start().starts_with("package:"))
        {
            Ok(())
        } else {
            Err(OpError::Said(format!("{package} is not installed.")))
        }
    }

    // ------------------------------------------------------- screen_app: look

    fn apps(&self, scope: Scope, filter: Option<&str>, limit: usize) -> Result<String, OpError> {
        let mut note = None;
        let (kind, mut entries, cut) = match scope {
            Scope::Launchable => match self.launchers()? {
                Some((launchers, cut)) => (
                    "Launchable apps",
                    launchers
                        .into_iter()
                        .map(|(package, component)| format!("{package}  {component}"))
                        .collect::<Vec<_>>(),
                    cut,
                ),
                None => {
                    note = Some("This Android version cannot list launcher activities (cmd package query-activities needs Android 7.1), so these are its third-party packages instead.");
                    let (packages, cut) = self.packages(true)?;
                    ("Third-party packages", packages, cut)
                }
            },
            Scope::ThirdParty => {
                let (packages, cut) = self.packages(true)?;
                ("Third-party packages", packages, cut)
            }
            Scope::All => {
                let (packages, cut) = self.packages(false)?;
                ("Installed packages", packages, cut)
            }
        };
        if let Some(filter) = filter {
            let needle = filter.to_lowercase();
            entries.retain(|entry| entry.to_lowercase().contains(&needle));
        }
        let total = entries.len();
        entries.truncate(limit);
        let matching = filter
            .map(|filter| format!(" containing \"{}\"", clean_line(filter)))
            .unwrap_or_default();
        let mut lines = Vec::with_capacity(entries.len() + 3);
        lines.extend(note.map(str::to_owned));
        lines.push(format!(
            "{kind} on {}{matching}: {total} (showing {}).",
            self.label,
            entries.len()
        ));
        lines.extend(entries);
        if cut {
            lines.push("(The device's list was longer than 1 MiB and was cut there.)".to_owned());
        }
        Ok(lines.join("\n"))
    }

    /// The launcher activities, one per package; `None` where the device cannot list them.
    fn launchers(&self) -> Result<Option<(Vec<(String, String)>, bool)>, OpError> {
        if self.sdk()? < QUERY_ACTIVITIES_MIN_SDK {
            return Ok(None);
        }
        let result = self.shell(
            "listing the launchable apps",
            LAUNCHERS_COMMAND,
            LIST_READ,
            COMMAND_TIME,
        )?;
        let text = printed(&result);
        let launchers = parse_launchers(&text);
        if launchers.is_empty() && cannot_query(&result, &text) {
            return Ok(None);
        }
        Ok(Some((launchers, result.truncated)))
    }

    fn packages(&self, third_party: bool) -> Result<(Vec<String>, bool), OpError> {
        let result = self.shell(
            "listing the packages",
            list_packages_command(third_party),
            LIST_READ,
            COMMAND_TIME,
        )?;
        let text = printed(&result);
        let packages = parse_package_list(&text);
        if packages.is_empty() {
            if let Some(problem) = refusal(&result, &text) {
                return Err(OpError::Said(format!(
                    "The device could not list its packages: {problem}"
                )));
            }
        }
        Ok((packages, result.truncated))
    }

    fn foreground(&self) -> Result<String, OpError> {
        let command = marked(&[
            ("window", window_dump_command(self.sdk()?)),
            ("policy", POLICY_COMMAND),
        ]);
        let started = Instant::now();
        loop {
            let text = self.output("reading the app in front", &command, DUMP_READ)?;
            let parts = sections(&text);
            let focus = parse_focus(parts.get("window").map_or("", String::as_str));
            let nothing = focus.window.is_none() && focus.app.is_none();
            if !nothing || started.elapsed() >= FOCUS_PATIENCE || !self.step.pause(FOCUS_POLL) {
                if nothing {
                    // Cut short: say so rather than report nothing in front.
                    self.step.proceed("reading the app in front again")?;
                }
                let locked = lock_screen(parts.get("policy").map_or("", String::as_str));
                return Ok(foreground_report(&focus, locked));
            }
        }
    }

    fn app_info(&self, package: &str) -> Result<String, OpError> {
        self.ensure_installed(package)?;
        let pidof = self.output(
            &format!("checking whether {package} runs"),
            &pidof_command(package),
            SHORT_READ,
        )?;
        let running = match parse_pids(&pidof) {
            pids if !pids.is_empty() => format!(
                "{} {}",
                if pids.len() == 1 { "pid" } else { "pids" },
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ if pidof.contains("not found") => "unknown".to_owned(),
            _ => "no".to_owned(),
        };
        let dump = self.shell(
            &format!("reading {package}'s details"),
            &dumpsys_package_command(package),
            PACKAGE_DUMP_READ,
            DUMP_TIME,
        )?;
        let facts = parse_package_dump(&printed(&dump), package).unwrap_or_default();
        let mut report = package_report(package, &facts, &running);
        if dump.truncated {
            report.push_str(
                "\n(Its dump was longer than 1 MiB; what follows the first MiB was not read.)",
            );
        }
        Ok(report)
    }

    // ------------------------------------------------------ screen_app: act

    fn launch(
        &self,
        package: &str,
        activity: Option<&str>,
        restart: bool,
    ) -> Result<String, OpError> {
        self.ensure_installed(package)?;
        let done = if restart {
            self.change(
                &format!("force-stopping {package}"),
                &force_stop_command(package),
            )?;
            Some(format!("Force-stopped {package}"))
        } else {
            None
        };
        let done = done.as_deref();
        let component = match activity {
            Some(activity) => Some(format!("{package}/{activity}")),
            None => self.launcher_of(package).map_err(|error| {
                after(
                    done,
                    &format!("finding {package}'s launcher activity"),
                    error,
                )
            })?,
        };
        let launched = |rest: &str| match done {
            Some(done) => format!("{done} and launched {rest}"),
            None => format!("Launched {rest}"),
        };
        let Some(component) = component else {
            // No single launcher activity to name: monkey picks the launcher the way the home
            // screen would. `--pct-syskeys 0`: an emulator without hardware keys refuses it otherwise.
            let what = format!("launching {package}");
            let text = self
                .output(&what, &monkey_command(package), SHORT_READ)
                .map_err(|error| after(done, &what, error))?;
            if text.contains("No activities found") || text.contains("monkey aborted") {
                return Err(after(
                    done,
                    &what,
                    OpError::Said(format!(
                        "{package} has no launcher activity; pass activity to start one of its activities."
                    )),
                ));
            }
            return Ok(launched(&format!("{package} (its launcher activity).")));
        };
        let what = format!("launching {component}");
        let result = self
            .shell(
                &what,
                &start_component_command(&component),
                SHORT_READ,
                COMMAND_TIME,
            )
            .map_err(|error| after(done, &what, error))?;
        match parse_am_start(&result) {
            Started::Done => Ok(launched(&format!("{component}."))),
            Started::AlreadyFront => Ok(format!(
                "{component} was already running; brought it to the front."
            )),
            // The component may be the device's own words (the launcher it resolved).
            Started::Failed(line) => Err(after(
                done,
                &what,
                OpError::Said(format!(
                    "Could not launch {}: {line}",
                    sanitize_for_message(&component)
                )),
            )),
        }
    }

    /// The package's launcher activity, as `pkg/.Activity`; `None` when the device names none,
    /// or several (the chooser, `ResolverActivity`), or cannot be asked (no `cmd` before 7.0).
    fn launcher_of(&self, package: &str) -> Result<Option<String>, OpError> {
        if self.sdk()? < CMD_MIN_SDK {
            return Ok(None);
        }
        let text = self.output(
            &format!("finding {package}'s launcher activity"),
            &resolve_launcher_command(package),
            SHORT_READ,
        )?;
        Ok(parse_resolved_activity(&text))
    }

    fn stop(&self, package: &str) -> Result<String, OpError> {
        self.ensure_installed(package)?;
        self.change(&format!("stopping {package}"), &force_stop_command(package))?;
        Ok(format!("Stopped {package}."))
    }

    fn open_url(&self, url: &str, package: Option<&str>) -> Result<String, OpError> {
        if let Some(package) = package {
            self.ensure_installed(package)?;
        }
        let result = self.shell(
            &format!("opening {}", brief(url, 80)),
            &open_url_command(url, package),
            SHORT_READ,
            COMMAND_TIME,
        )?;
        match parse_am_start(&result) {
            Started::Failed(line) if line.to_ascii_lowercase().contains("unable to resolve") => {
                Err(OpError::Said(match package {
                    Some(package) => format!("{package} does not handle this URL."),
                    None => "No app on the device handles this URL.".to_owned(),
                }))
            }
            Started::Failed(line) => Err(OpError::Said(format!("Could not open the URL: {line}"))),
            Started::Done | Started::AlreadyFront => Ok(match package {
                Some(package) => format!("Opened {url} in {package}."),
                None => format!("Opened {url}."),
            }),
        }
    }

    // ---------------------------------------------------------- screen_package

    fn install(
        &self,
        apk: &Apk,
        replace_existing: bool,
        flags: &[&'static str],
    ) -> Result<String, OpError> {
        let what = format!("installing {}", apk.name);
        self.step.proceed(&what)?;
        // `&File` reads and seeks too: the call holds the file it checked.
        let mut reader: &File = &apk.file;
        let mut words: Vec<&'static str> = Vec::with_capacity(flags.len() + 1);
        if replace_existing {
            // Android 9 and later replace by default and ignore it; before, it is what allows it.
            words.push("-r");
        } else {
            // An app that is installed already is refused before the APK is sent. pm refuses too:
            // without `-r` before Android 9, and with `-R` since (it replaces by default there),
            // for an app that appeared meanwhile, or one whose package could not be read here.
            if let Some(package) =
                apk::package_name(&mut reader, apk.size).filter(|name| is_package(name))
            {
                let text = self.output(
                    &format!("checking whether {package} is installed"),
                    &pm_path_command(&package),
                    SHORT_READ,
                )?;
                if text
                    .lines()
                    .any(|line| line.trim_start().starts_with("package:"))
                {
                    return Err(OpError::Said(format!(
                        "{package} is installed already and replace_existing is false, so {} was not sent. Leave replace_existing at true to replace it, or uninstall {package} first.",
                        apk.name
                    )));
                }
            }
            if self.sdk()? >= INSTALL_NO_REPLACE_MIN_SDK {
                words.push("-R");
            }
        }
        words.extend_from_slice(flags);
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| OpError::Said(format!("Could not read {}: {error}", apk.name)))?;
        let cancelled = || self.step.cancelled();
        match self.android.install(
            &mut reader,
            apk.size,
            &words,
            self.step.remaining(),
            &cancelled,
        ) {
            Ok(answer) if answer.trim_start().starts_with("Success") => Ok(format!(
                "Installed {} ({}) on {}.",
                apk.name,
                human_size(apk.size),
                self.label
            )),
            Ok(answer) => Err(OpError::Said(format!(
                "The device did not install {}: {}{}",
                apk.name,
                sanitize_for_message(answer.trim()),
                install_hint(&answer)
            ))),
            Err(DriverError::Unsupported(message)) => Err(OpError::Said(message)),
            Err(error) => Err(error.into()),
        }
    }

    fn uninstall(&self, package: &str, keep_data: bool) -> Result<String, OpError> {
        self.ensure_installed(package)?;
        let sdk = self.sdk()?;
        let text = self.output(
            &format!("uninstalling {package}"),
            &uninstall_command(sdk, package, keep_data),
            SHORT_READ,
        )?;
        match status_line(&text) {
            Some(status) if status.starts_with("Success") => Ok(if keep_data {
                format!("Uninstalled {package} and kept its data and cache.")
            } else {
                format!("Uninstalled {package}.")
            }),
            Some(status) => Err(OpError::Said(format!(
                "The device did not uninstall {package}: {status}"
            ))),
            None => Err(OpError::Said(format!(
                "The device did not say whether it uninstalled {package}{}",
                printed_tail(&text)
            ))),
        }
    }

    fn clear_data(&self, package: &str) -> Result<String, OpError> {
        self.ensure_installed(package)?;
        let text = self.output(
            &format!("erasing {package}'s data"),
            &clear_command(package),
            SHORT_READ,
        )?;
        match status_line(&text) {
            Some(status) if status.starts_with("Success") => {
                Ok(format!("Erased all data of {package}."))
            }
            Some(status) => Err(OpError::Said(format!(
                "The device did not erase {package}'s data: {status}"
            ))),
            None => Err(OpError::Said(format!(
                "The device did not say whether it erased {package}'s data{}",
                printed_tail(&text)
            ))),
        }
    }

    fn permission(&self, grant: bool, package: &str, permission: &str) -> Result<String, OpError> {
        let (verb, word, preposition) = if grant {
            ("granting", "Granted", "to")
        } else {
            ("revoking", "Revoked", "from")
        };
        let what = format!("{verb} {permission} {preposition} {package}");
        let result = self.shell(
            &what,
            &permission_command(grant, package, permission),
            SHORT_READ,
            COMMAND_TIME,
        )?;
        match refusal(&result, &printed(&result)) {
            Some(problem) => Err(OpError::Said(format!(
                "Could not {} {permission} {preposition} {package}: {problem}",
                if grant { "grant" } else { "revoke" }
            ))),
            None => Ok(format!("{word} {permission} {preposition} {package}.")),
        }
    }

    // -------------------------------------------------------- android_logcat

    fn logcat(&self, query: &Logcat) -> Result<String, OpError> {
        let sdk = self.sdk()?;
        let source = match &query.package {
            None => None,
            Some(package) if sdk >= LOGCAT_UID_MIN_SDK => {
                let text = self.output(
                    &format!("finding {package}'s user id"),
                    &uid_command(package),
                    SHORT_READ,
                )?;
                let uid = parse_uid(&text, package)
                    .ok_or_else(|| OpError::Said(format!("{package} is not installed.")))?;
                Some(LogSource::Uid(uid))
            }
            Some(package) => {
                let text = self.output(
                    &format!("finding {package}'s process"),
                    &pidof_command(package),
                    SHORT_READ,
                )?;
                let pid = parse_pids(&text).first().copied().ok_or_else(|| {
                    OpError::Said(format!(
                        "{package} is not running, so its lines cannot be told apart on this Android version; launch it first, or read buffer crash without package."
                    ))
                })?;
                Some(LogSource::Pid(pid))
            }
        };
        // logd counts `-t` against the buffers and a pid only; logcat applies a uid, a tag and a
        // level afterwards, and `contains` is applied here. With any of them, the newest
        // LOG_FETCH_MAX lines are searched, and the last `lines` that match are kept.
        let searched = matches!(source, Some(LogSource::Uid(_)))
            || query.tag.is_some()
            || query.level.is_some_and(|level| level != "V")
            || query.contains.is_some();
        let fetch = if searched {
            LOG_FETCH_MAX.max(query.lines)
        } else {
            query.lines
        };
        let command = logcat_command(
            fetch,
            query.buffer,
            source,
            query.tag.as_deref(),
            query.level,
        );
        let result = self.shell("reading the log", &command, LOG_READ, DUMP_TIME)?;
        if let Some(code) = result.exit.filter(|code| *code != 0) {
            let text = printed(&result);
            let said = text
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(sanitize_for_message)
                .unwrap_or_else(|| format!("exit status {code}"));
            return Err(OpError::Said(format!("logcat failed: {said}")));
        }
        Ok(log_report(
            &clean(&result.stdout),
            query,
            source,
            result.truncated,
            searched.then_some(fetch),
        ))
    }

    // ------------------------------------------------- screen_device: info

    /// The sections asked for, and then the dialog showing, if any.
    fn info(&self, sections: &[Section]) -> Result<String, OpError> {
        let mut blocks: Vec<String> = Vec::with_capacity(sections.len() + 1);
        let reads = sections
            .iter()
            .map(Some)
            .chain(std::iter::once(None::<&Section>));
        for section in reads {
            let (block, what) = match section {
                Some(Section::Device) => (self.device_block(), DEVICE_READ),
                Some(Section::Display) => (self.display_block(), DISPLAY_READ),
                Some(Section::Power) => (self.power_block(), POWER_READ),
                Some(Section::Notifications) => (self.notification_block(), NOTIFICATIONS_READ),
                None => (self.dialog_block(), DIALOG_READ),
            };
            match block {
                Ok(block) => blocks.push(block),
                // Stopped between sections, or one that failed: what was read is still worth
                // having. A device that went away is the session's to handle.
                Err(OpError::Said(message)) if !blocks.is_empty() => {
                    blocks.push(format!("({message})"));
                    break;
                }
                Err(OpError::Driver(error))
                    if !blocks.is_empty() && !matches!(error, DriverError::Disconnected(_)) =>
                {
                    blocks.push(format!(
                        "({} failed: {}.)",
                        capitalized(what),
                        error.message().trim_end_matches('.')
                    ));
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(blocks.join("\n"))
    }

    fn device_block(&self) -> Result<String, OpError> {
        let text = self.output(DEVICE_READ, "getprop", DUMP_READ)?;
        Ok(device_report(&parse_getprop(&text)))
    }

    fn display_block(&self) -> Result<String, OpError> {
        let text = self.output(DISPLAY_READ, &display_command(), SHORT_READ)?;
        Ok(display_report(&sections(&text)))
    }

    fn power_block(&self) -> Result<String, OpError> {
        let text = self.output(POWER_READ, &power_command(), DUMP_READ)?;
        Ok(power_report(&sections(&text)))
    }

    fn notification_block(&self) -> Result<String, OpError> {
        let text = self.output(
            NOTIFICATIONS_READ,
            "dumpsys notification --noredact",
            DUMP_READ,
        )?;
        Ok(notification_report(&parse_notifications(&text)))
    }

    /// The dialog showing and its buttons ([`dialog_buttons`]), from the element tree.
    fn dialog_block(&self) -> Result<String, OpError> {
        Ok(match self.tree(DIALOG_READ)? {
            Some(root) => dialog_report(&dialog_buttons(&root)),
            None => "dialog: unknown (this screen has no element tree)".to_owned(),
        })
    }

    /// The screen's element tree, within the call's time: not started with less left than a
    /// dump is expected to take ([`UI_TREE_READ`]), and ended at what is left when the device is
    /// slower ([`Driver::ui_tree_within`]).
    fn tree(&self, what: &str) -> Result<Option<UiNode>, OpError> {
        self.step.proceed(what)?;
        if !self.step.fits(UI_TREE_READ) {
            return Err(OpError::Said(format!(
                "Ran out of this call's time before {what}."
            )));
        }
        Ok(self
            .driver
            .ui_tree_within(ui_tree_timeout(self.step.remaining()))?)
    }

    // ------------------------------------------------ screen_device: change

    fn setting(&self, setting: Setting) -> Result<String, OpError> {
        let what = setting.what();
        match setting {
            Setting::Wake => {
                let before = self.wakefulness()?;
                self.change(what, "input keyevent 224")?;
                Ok(match before.as_deref() {
                    Some("Awake") => "The device was awake already.".to_owned(),
                    Some(before) => {
                        format!("Woke the device; it was {}.", before.to_ascii_lowercase())
                    }
                    None => "Woke the device.".to_owned(),
                })
            }
            Setting::Lock => self.lock(),
            Setting::Unlock => self.unlock(),
            Setting::Rotate(target) => {
                let before = sections(&self.output(
                    "reading the rotation",
                    &marked(&[
                        ("size", "wm size"),
                        (
                            "accelerometer",
                            "settings get system accelerometer_rotation",
                        ),
                        ("rotation", "settings get system user_rotation"),
                    ]),
                    SHORT_READ,
                )?);
                let natural = natural_landscape(before.get("size").map_or("", String::as_str));
                let was = rotation_name(
                    setting_of(&before, "accelerometer"),
                    setting_of(&before, "rotation"),
                    natural,
                );
                let command = match target {
                    None => rotate_command(self.sdk()?, None),
                    Some(orientation) => {
                        let landscape = natural
                            .ok_or_else(|| OpError::Said(NATURAL_ORIENTATION_UNKNOWN.to_owned()))?;
                        rotate_command(self.sdk()?, Some(surface_rotation(orientation, landscape)))
                    }
                };
                self.change(what, &command)?;
                let now = match target {
                    Some(orientation) => format!("{} (locked)", orientation.name()),
                    None => "auto-rotate".to_owned(),
                };
                Ok(format!("The rotation is now {now}; it was {was}."))
            }
            Setting::Appearance(word) => {
                let was = night_mode(&self.output(
                    "reading the appearance",
                    "cmd uimode night",
                    SHORT_READ,
                )?);
                self.change(what, &format!("cmd uimode night {}", shell_quote(word)))?;
                let now = match word {
                    "yes" => "dark",
                    "no" => "light",
                    _ => "auto",
                };
                Ok(format!("Appearance is now {now}; it was {was}."))
            }
            Setting::DismissKeyboard => self.dismiss_keyboard(),
            Setting::Animations(on) => {
                let before = sections(&self.output(
                    "reading the animation scales",
                    &marked(&ANIMATION_SCALES),
                    SHORT_READ,
                )?);
                let was: Vec<String> = ANIMATION_SCALES
                    .iter()
                    .map(|(scale, _)| scale_value(setting_of(&before, scale)))
                    .collect();
                self.change(what, &animations_command(on))?;
                Ok(format!(
                    "Animations are now {} (window, transition and animator duration scales {}); they were {}.",
                    if on { "on" } else { "off" },
                    if on { 1 } else { 0 },
                    and_list(&was.iter().map(String::as_str).collect::<Vec<_>>())
                ))
            }
            Setting::Shade(subcommand) => {
                self.change(what, &format!("cmd statusbar {}", shell_quote(subcommand)))?;
                Ok(match subcommand {
                    "expand-notifications" => "Opened the notification shade.",
                    "expand-settings" => "Opened quick settings.",
                    _ => "Collapsed the notification shade.",
                }
                .to_owned())
            }
            Setting::StayAwake(on) => {
                let before = self.output(
                    "reading stay-awake",
                    "settings get global stay_on_while_plugged_in",
                    SHORT_READ,
                )?;
                let was = match before.trim() {
                    "0" | "null" | "" => "off",
                    other if other.chars().all(|character| character.is_ascii_digit()) => "on",
                    _ => "unknown",
                };
                self.change(
                    what,
                    if on {
                        "svc power stayon true"
                    } else {
                        "svc power stayon false"
                    },
                )?;
                Ok(format!(
                    "Staying awake while charging is now {}; it was {was}.",
                    if on { "on" } else { "off" }
                ))
            }
        }
    }

    /// `dumpsys power`'s wakefulness: `Awake`, `Asleep`, `Dreaming` or `Dozing`.
    fn wakefulness(&self) -> Result<Option<String>, OpError> {
        let text = self.output(
            "reading whether the screen is on",
            "dumpsys power",
            DUMP_READ,
        )?;
        Ok(parse_power(&text).wakefulness)
    }

    fn lock_state(&self, what: &str) -> Result<Option<bool>, OpError> {
        Ok(lock_screen(&self.output(
            what,
            POLICY_COMMAND,
            DUMP_READ,
        )?))
    }

    /// Wakes the device, and dismisses a lock screen that has no credential. A PIN, pattern or
    /// password is the person's to enter: the keyguard stays, and the answer says so.
    fn unlock(&self) -> Result<String, OpError> {
        self.change("waking the device", "input keyevent 224")?;
        let done = Some("Woke the device");
        let reading = "reading the lock screen";
        if self
            .lock_state(reading)
            .map_err(|error| after(done, reading, error))?
            == Some(false)
        {
            return Ok("The device is awake and its lock screen is not showing.".to_owned());
        }
        let dismissing = "dismissing the lock screen";
        let dismissed = self
            .shell(dismissing, "wm dismiss-keyguard", SHORT_READ, COMMAND_TIME)
            .map_err(|error| after(done, dismissing, error))?;
        if refusal(&dismissed, &printed(&dismissed)).is_some() {
            // An Android without `wm dismiss-keyguard`: the menu key dismisses an insecure keyguard.
            self.change(dismissing, "input keyevent 82")
                .map_err(|error| after(done, dismissing, error))?;
        }
        self.step.pause(UNLOCK_SETTLE);
        let again = "reading the lock screen again";
        match self.lock_state(again).map_err(|error| {
            after(
                Some("Asked the device to dismiss its lock screen"),
                again,
                error,
            )
        })? {
            Some(true) => Err(OpError::Said(LOCKED_WITH_CREDENTIAL.to_owned())),
            Some(false) => Ok("Unlocked the device.".to_owned()),
            None => Ok("Asked the device to dismiss its lock screen; whether it is gone could not be read. Take a screenshot to check.".to_owned()),
        }
    }

    /// Sends the sleep key, and then says what the device made of it. Whether sleeping locks is
    /// the device's own setting (the power button locking at once, the lock timeout), so the
    /// answer reads the screen and the lock screen once rather than assume.
    fn lock(&self) -> Result<String, OpError> {
        let before = self.wakefulness()?;
        self.change(Setting::Lock.what(), "input keyevent 223")?;
        let done = match before.as_deref() {
            Some("Asleep") => "The device was asleep already".to_owned(),
            Some(before) => format!(
                "Put the device to sleep; it was {}",
                before.to_ascii_lowercase()
            ),
            None => "Put the device to sleep".to_owned(),
        };
        // The lock screen comes up a moment after the screen goes off.
        self.step.pause(LOCK_SETTLE);
        let reading = "reading the lock screen";
        let text = self
            .output(
                reading,
                &marked(&[("power", "dumpsys power"), ("policy", POLICY_COMMAND)]),
                DUMP_READ,
            )
            .map_err(|error| after(Some(&done), reading, error))?;
        let parts = sections(&text);
        let part = |name: &str| parts.get(name).map_or("", String::as_str);
        Ok(format!(
            "{done}. {}",
            lock_report(&parse_power(part("power")), lock_screen(part("policy")))
        ))
    }

    /// Presses Back when the input method shows, and only then: Back with no keyboard up would
    /// leave the app's screen.
    fn dismiss_keyboard(&self) -> Result<String, OpError> {
        let reading = "reading whether the keyboard is showing";
        match self.keyboard_shown(reading)? {
            Some(false) => Ok("No keyboard was showing; nothing was pressed.".to_owned()),
            None => Err(OpError::Said(
                "Could not tell whether the keyboard is showing (this Android's input method dump says nothing about it), so Back was not pressed. If a keyboard is up, press the Back key with preview_input.".to_owned(),
            )),
            Some(true) => {
                self.change("pressing Back", "input keyevent 4")?;
                let done = Some("Pressed Back with the keyboard showing");
                self.step.pause(KEYBOARD_SETTLE);
                let again = "reading whether the keyboard is showing again";
                Ok(
                    match self
                        .keyboard_shown(again)
                        .map_err(|error| after(done, again, error))?
                    {
                        Some(false) => "The keyboard was showing; pressed Back and it is gone.",
                        Some(true) => "The keyboard was showing; pressed Back, but it is still showing.",
                        None => "The keyboard was showing; pressed Back.",
                    }
                    .to_owned(),
                )
            }
        }
    }

    /// Whether the input method shows, from `dumpsys input_method`'s `mInputShown`.
    fn keyboard_shown(&self, what: &str) -> Result<Option<bool>, OpError> {
        Ok(input_shown(&self.output(
            what,
            "dumpsys input_method",
            DUMP_READ,
        )?))
    }

    // ------------------------------------------------------- preview_dialog

    /// Presses the button of the dialog showing that `wanted` names: its label, when exactly one
    /// button has it ([`pick_button`]). Then reads the screen once more, because another dialog
    /// often follows (a permission, then the next permission).
    fn press(&self, wanted: &str) -> Result<String, OpError> {
        let Some(root) = self.tree(DIALOG_READ)? else {
            return Err(OpError::Said(
                "This screen has no element tree, so the buttons of its dialog cannot be read. Take a preview_screenshot and press one with preview_input.".to_owned(),
            ));
        };
        let buttons = dialog_buttons(&root);
        if buttons.is_empty() {
            return Err(OpError::Said(format!(
                "No dialog is showing on \"{}\".",
                sanitize_for_message(&self.label)
            )));
        }
        let button = pick_button(&buttons, wanted).map_err(OpError::Said)?;
        let shown = shown_label(&button.label);
        let (x, y) = button.center.ok_or_else(|| {
            OpError::Said(format!(
                "The button \"{shown}\" has no area on the screen, so it cannot be pressed."
            ))
        })?;
        let pressing = format!("pressing \"{shown}\"");
        self.step.proceed(&pressing)?;
        if !self.step.fits(MIN_COMMAND_TIME) {
            return Err(OpError::Said(format!(
                "Ran out of this call's time before {pressing}."
            )));
        }
        self.driver.tap(x, y, 1)?;
        let mut said = format!("Pressed \"{shown}\".");
        if self.step.pause(DIALOG_SETTLE) && self.step.fits(UI_TREE_READ) {
            match self
                .driver
                .ui_tree_within(ui_tree_timeout(self.step.remaining()))
            {
                Ok(Some(root)) => {
                    let next = dialog_buttons(&root);
                    if !next.is_empty() {
                        // Without the dialog's text, the same buttons may be the same dialog or
                        // the next one of its kind: the answer does not claim which.
                        let lead = if labels(&next) == labels(&buttons) {
                            "A dialog with the same buttons is showing"
                        } else {
                            "Another dialog is showing"
                        };
                        said.push_str(&format!("\n{lead}; buttons: {}.", label_list(&next)));
                    }
                }
                // A device that went away is the session's to handle; any other trouble reading
                // is only the courtesy of this answer going without.
                Err(error @ DriverError::Disconnected(_)) => return Err(error.into()),
                Ok(None) | Err(_) => {}
            }
        }
        Ok(said)
    }

    // --------------------------------------------------------- android_shell

    fn shell_command(&self, command: &str, timeout: Duration) -> Result<String, OpError> {
        let what = "running the shell command";
        self.step.proceed(what)?;
        if !self.step.fits(MIN_COMMAND_TIME) {
            return Err(OpError::Said(format!(
                "Ran out of this call's time before {what}."
            )));
        }
        let timeout = timeout.min(self.step.remaining());
        let started = Instant::now();
        match self.android.shell(command, SHELL_KEEP, timeout) {
            // Stopped when its time ran out (or once it had printed far more than is read).
            Ok(result) if result.timed_out => Err(OpError::Said(stopped_report(
                &result,
                started.elapsed().min(timeout),
            ))),
            Ok(result) => Ok(shell_report(&result)),
            // A driver whose timeout is a failure has no output to give with it.
            Err(DriverError::Failed(_)) if started.elapsed() >= timeout => {
                Err(OpError::Said(format!(
                    "Stopped after {} ms: the command did not end.",
                    timeout.as_millis()
                )))
            }
            Err(error) => Err(error.into()),
        }
    }
}

// ------------------------------------------------------------- commands

/// Commands run one after another in one shell, each after a line that names it ([`sections`]
/// reads them apart): one round trip for what would otherwise be several. Each part's stderr goes
/// with its stdout, so that a framed shell, which keeps the streams apart, does not leave one
/// part's complaint at the end of the last part. Every part is one simple command.
fn marked(parts: &[(&str, &str)]) -> String {
    parts
        .iter()
        .map(|(name, command)| format!("echo {MARK}{name}; {command} 2>&1"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn list_packages_command(third_party: bool) -> &'static str {
    if third_party {
        "pm list packages -3"
    } else {
        "pm list packages"
    }
}

fn pm_path_command(package: &str) -> String {
    format!("pm path {}", shell_quote(package))
}

fn pidof_command(package: &str) -> String {
    format!("pidof {}", shell_quote(package))
}

fn dumpsys_package_command(package: &str) -> String {
    format!("dumpsys package {}", shell_quote(package))
}

fn window_dump_command(sdk: u32) -> &'static str {
    if sdk >= DISPLAYS_DUMP_MIN_SDK {
        "dumpsys window displays"
    } else {
        "dumpsys window windows"
    }
}

fn resolve_launcher_command(package: &str) -> String {
    format!(
        "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {}",
        shell_quote(package)
    )
}

fn monkey_command(package: &str) -> String {
    format!(
        "monkey -p {} --pct-syskeys 0 -c android.intent.category.LAUNCHER 1",
        shell_quote(package)
    )
}

fn start_component_command(component: &str) -> String {
    format!(
        "am start -n {} -a android.intent.action.MAIN -c android.intent.category.LAUNCHER",
        shell_quote(component)
    )
}

fn force_stop_command(package: &str) -> String {
    format!("am force-stop {}", shell_quote(package))
}

fn open_url_command(url: &str, package: Option<&str>) -> String {
    let mut command = format!(
        "am start -a android.intent.action.VIEW -d {}",
        shell_quote(url)
    );
    if let Some(package) = package {
        command.push(' ');
        command.push_str(&shell_quote(package));
    }
    command
}

fn uninstall_command(sdk: u32, package: &str, keep_data: bool) -> String {
    format!(
        "{} uninstall {}{}",
        if sdk >= CMD_MIN_SDK {
            "cmd package"
        } else {
            "pm"
        },
        if keep_data { "-k " } else { "" },
        shell_quote(package)
    )
}

fn clear_command(package: &str) -> String {
    format!("pm clear {}", shell_quote(package))
}

fn permission_command(grant: bool, package: &str, permission: &str) -> String {
    format!(
        "pm {} {} {}",
        if grant { "grant" } else { "revoke" },
        shell_quote(package),
        shell_quote(permission)
    )
}

fn uid_command(package: &str) -> String {
    format!("cmd package list packages -U {}", shell_quote(package))
}

/// Whose log lines `android_logcat` keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogSource {
    Uid(u32),
    Pid(u32),
}

/// `logcat -d -v threadtime -t <N> [-b <buf>] [--uid=<uid> | --pid=<pid>] [-- <tag>:<L> *:S | --
/// *:<L>]`. `--` ends logcat's options (its getopt stops there on every version), so a filter
/// spec is never read as one even if a check let one through.
fn logcat_command(
    lines: u64,
    buffer: Option<&str>,
    source: Option<LogSource>,
    tag: Option<&str>,
    level: Option<&str>,
) -> String {
    let mut command = format!("logcat -d -v threadtime -t {lines}");
    if let Some(buffer) = buffer {
        command.push_str(" -b ");
        command.push_str(&shell_quote(buffer));
    }
    match source {
        Some(LogSource::Uid(uid)) => command.push_str(&format!(" --uid={uid}")),
        Some(LogSource::Pid(pid)) => command.push_str(&format!(" --pid={pid}")),
        None => {}
    }
    match (tag, level) {
        (Some(tag), level) => {
            command.push_str(" -- ");
            command.push_str(&shell_quote(&format!("{tag}:{}", level.unwrap_or("V"))));
            command.push(' ');
            command.push_str(&shell_quote("*:S"));
        }
        (None, Some(level)) => {
            command.push_str(" -- ");
            command.push_str(&shell_quote(&format!("*:{level}")));
        }
        (None, None) => {}
    }
    command
}

fn display_command() -> String {
    marked(&[
        ("size", "wm size"),
        ("density", "wm density"),
        ("rotation", "settings get system user_rotation"),
        (
            "accelerometer",
            "settings get system accelerometer_rotation",
        ),
        ("night", "cmd uimode night"),
        ("animation", "settings get global window_animation_scale"),
    ])
}

fn power_command() -> String {
    marked(&[
        ("battery", "dumpsys battery"),
        ("power", "dumpsys power"),
        ("policy", POLICY_COMMAND),
    ])
}

/// The three animation scales: the name of each one's part of a [`marked`] read, and the read.
const ANIMATION_SCALES: [(&str, &str); 3] = [
    ("window", "settings get global window_animation_scale"),
    (
        "transition",
        "settings get global transition_animation_scale",
    ),
    ("animator", "settings get global animator_duration_scale"),
];

fn animations_command(on: bool) -> String {
    let value = if on { 1 } else { 0 };
    format!(
        "settings put global window_animation_scale {value}; settings put global transition_animation_scale {value}; settings put global animator_duration_scale {value}"
    )
}

/// Android 10 and later lock or free the rotation with `cmd window user-rotation`; before, the
/// two settings it writes are written directly.
fn rotate_command(sdk: u32, rotation: Option<u8>) -> String {
    match (sdk >= USER_ROTATION_MIN_SDK, rotation) {
        (true, Some(rotation)) => format!("cmd window user-rotation lock {rotation}"),
        (true, None) => "cmd window user-rotation free".to_owned(),
        (false, Some(rotation)) => format!(
            "settings put system accelerometer_rotation 0; settings put system user_rotation {rotation}"
        ),
        (false, None) => "settings put system accelerometer_rotation 1".to_owned(),
    }
}

// ------------------------------------------------------------- reading what came back

/// Device bytes as text a model may read: invalid UTF-8 replaced, every control character but
/// newline and tab taken out.
fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

/// One value the device printed, on one line.
fn clean_line(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// What the device printed: stdout, then stderr (empty without a framed shell, which mixes it in).
fn printed(result: &ShellResult) -> String {
    let mut text = clean(&result.stdout);
    if !result.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&clean(&result.stderr));
    }
    text
}

/// `": <first line>."` of what was printed, or `"."` when nothing was.
fn printed_tail(text: &str) -> String {
    match text.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => format!(": {}", sanitize_for_message(line)),
        None => ".".to_owned(),
    }
}

/// At most `chars` characters of `text`, with an ellipsis when cut.
fn brief(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        let mut cut: String = text.chars().take(chars).collect();
        cut.push('…');
        cut
    }
}

fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// `12.3 MB`, in powers of a thousand.
fn human_size(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes < 1000 {
        format!("{bytes} bytes")
    } else if value < 1e6 {
        format!("{:.1} KB", value / 1e3)
    } else if value < 1e9 {
        format!("{:.1} MB", value / 1e6)
    } else {
        format!("{:.1} GB", value / 1e9)
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The line of what a command printed that says what went wrong: an exception's message first,
/// else the first line that reads like a complaint.
fn complaint(text: &str) -> Option<String> {
    // A service without a shell command answers the last one, and exits 0 (Binder's default).
    const MARKERS: [&str; 10] = [
        "Exception",
        "Error",
        "Failure",
        "nknown command",
        "nknown option",
        "not found",
        "Can't find service",
        "Usage:",
        "usage:",
        "No shell command implementation",
    ];
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .find(|line| line.contains("Exception: "))
        .or_else(|| {
            lines
                .iter()
                .find(|line| MARKERS.iter().any(|marker| line.contains(marker)))
        })
        .map(|line| sanitize_for_message(line))
}

/// Why a command that should print nothing did not work, if it did not: what it printed that
/// reads like an error, or a non-zero exit status.
fn refusal(result: &ShellResult, text: &str) -> Option<String> {
    complaint(text).or_else(|| match result.exit {
        Some(code) if code != 0 => Some(
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(sanitize_for_message)
                .unwrap_or_else(|| format!("exit status {code}")),
        ),
        _ => None,
    })
}

/// pm's `Success` / `Failure [...]` / `Failed` line.
fn status_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("Success") || line.starts_with("Failure") || line.starts_with("Failed")
        })
        .last()
        .map(sanitize_for_message)
}

/// Whether `cmd package query-activities` is missing on this device (rather than listing nothing).
fn cannot_query(result: &ShellResult, text: &str) -> bool {
    result.exit.is_some_and(|code| code != 0)
        || text.contains("nknown command")
        || text.contains("Error")
}

/// What each part of a [`marked`] command printed, by name. A marker is a whole line of its own
/// at the start of a line, as `echo` prints it; the dumps between them indent what they print,
/// so a window title or a notification that holds a marker's text does not start a part.
fn sections(text: &str) -> BTreeMap<String, String> {
    let mut parts: BTreeMap<String, String> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let marker = line
            .trim_end()
            .strip_prefix(MARK)
            .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase()));
        if let Some(name) = marker {
            current = Some(name.to_owned());
            parts.entry(name.to_owned()).or_default();
            continue;
        }
        if let Some(part) = current.as_ref().and_then(|name| parts.get_mut(name)) {
            part.push_str(line);
            part.push('\n');
        }
    }
    parts
}

/// A `settings get` answer of a [`marked`] read: `None` for `null` (never set) or nothing.
fn setting_of<'a>(parts: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    parts
        .get(name)
        .map(|part| part.trim())
        .filter(|value| !value.is_empty() && *value != "null")
}

/// One component per package from `cmd package query-activities --brief`: its lines are
/// `priority=… preferredOrder=…` and `com.android.settings/.Settings`. The first component of
/// each package is kept; sorted by package.
fn parse_launchers(text: &str) -> Vec<(String, String)> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.contains(char::is_whitespace) {
            continue;
        }
        let Some((package, class)) = line.split_once('/') else {
            continue;
        };
        if !is_package(package) || class.is_empty() {
            continue;
        }
        found
            .entry(package.to_owned())
            .or_insert_with(|| line.to_owned());
    }
    found.into_iter().collect()
}

/// `package:<name>` lines (`pm list packages`; with `-U`, `package:<name> uid:<n>`), sorted.
fn parse_package_list(text: &str) -> Vec<String> {
    let mut packages: Vec<String> = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|name| is_package(name))
        .map(str::to_owned)
        .collect();
    packages.sort();
    packages.dedup();
    packages
}

/// `pidof`'s answer: the numbers it printed.
fn parse_pids(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .map_while(|word| word.parse::<u32>().ok())
        .collect()
}

/// The uid of `package` from `cmd package list packages -U <pkg>`, whose filter is a substring:
/// `package:com.foo uid:10234` among `package:com.foo.bar uid:…`.
fn parse_uid(text: &str, package: &str) -> Option<u32> {
    let wanted = format!("package:{package}");
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next() != Some(wanted.as_str()) {
            return None;
        }
        words.find_map(|word| {
            let digits: String = word
                .strip_prefix("uid:")?
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
    })
}

/// The launcher `cmd package resolve-activity --brief` names: its last line with a `/`. The
/// chooser (`ResolverActivity`, when several match) and "No activity found" are none.
fn parse_resolved_activity(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| line.contains('/') && !line.contains(char::is_whitespace))
        .last()?;
    (!line.contains("ResolverActivity")).then(|| line.to_owned())
}

/// What `am start` said: it exits 0 when it failed, so its words decide.
#[derive(Debug, PartialEq, Eq)]
enum Started {
    Done,
    /// "Warning: Activity not started, its current task has been brought to the front".
    AlreadyFront,
    Failed(String),
}

/// The lines `am start` prints of its own on the way (`Starting: Intent { … }`, and `-W`'s
/// report): they echo the intent, whose component or link may hold any word.
fn am_echo(line: &str) -> bool {
    [
        "Starting: ",
        "Status:",
        "Activity:",
        "ThisTime:",
        "TotalTime:",
        "WaitTime:",
        "LaunchState:",
        "Complete",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn parse_am_start(result: &ShellResult) -> Started {
    let text = printed(result);
    let all: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let lines: Vec<&str> = all.iter().copied().filter(|line| !am_echo(line)).collect();
    if let Some(line) = lines
        .iter()
        .find(|line| line.to_ascii_lowercase().contains("unable to resolve"))
    {
        return Started::Failed(sanitize_for_message(line));
    }
    let complaint = |line: &&&str| {
        ["Error:", "Exception", "java.", "Security exception"]
            .iter()
            .any(|prefix| line.starts_with(prefix))
    };
    if let Some(line) = lines
        .iter()
        .find(|line| line.contains("Exception: "))
        .or_else(|| lines.iter().find(complaint))
        .or_else(|| lines.iter().find(|line| line.starts_with("Error")))
    {
        return Started::Failed(sanitize_for_message(line));
    }
    if let Some(code) = result.exit.filter(|code| *code != 0) {
        return Started::Failed(
            lines
                .first()
                .map(|line| sanitize_for_message(line))
                .unwrap_or_else(|| format!("exit status {code}")),
        );
    }
    if lines
        .iter()
        .any(|line| line.starts_with("Warning: Activity not started"))
    {
        Started::AlreadyFront
    } else {
        Started::Done
    }
}

/// What the window manager says has focus: the focused window's title (`mCurrentFocus`) and the
/// focused app's component (`mFocusedApp`).
#[derive(Debug, Default, PartialEq, Eq)]
struct Focus {
    window: Option<String>,
    app: Option<String>,
}

/// The first non-null `mCurrentFocus` and `mFocusedApp` of `dumpsys window displays` (Android
/// 10+, one pair per display) or `dumpsys window windows` (before).
fn parse_focus(text: &str) -> Focus {
    let mut focus = Focus::default();
    for line in text.lines() {
        if focus.window.is_none() {
            if let Some((_, value)) = line.split_once("mCurrentFocus=") {
                focus.window = window_title(value);
            }
        }
        if focus.app.is_none() {
            if let Some((_, value)) = line.split_once("mFocusedApp=") {
                focus.app = focused_component(value);
            }
        }
    }
    focus
}

/// `Window{8f1b2a3 u0 com.foo/com.foo.Main}` → `com.foo/com.foo.Main`; `Window{… u0
/// NotificationShade}` → `NotificationShade`; `null` → none.
fn window_title(value: &str) -> Option<String> {
    let inner = value.trim().strip_prefix("Window{")?;
    let inner = &inner[..inner.rfind('}')?];
    let mut words: Vec<&str> = inner.split_whitespace().skip(1).collect();
    if words.first().is_some_and(|word| {
        word.strip_prefix('u')
            .is_some_and(|user| !user.is_empty() && user.chars().all(|c| c.is_ascii_digit()))
    }) {
        words.remove(0);
    }
    let title = clean_line(&words.join(" "));
    (!title.is_empty()).then_some(title)
}

/// The component in `ActivityRecord{2c5e0b1 u0 com.foo/.Main t17}` (Android 10+), also inside
/// `AppWindowToken{… token=Token{… ActivityRecord{… u0 com.foo/.Main t2}}}` (before).
fn focused_component(value: &str) -> Option<String> {
    let (_, record) = value.split_once("ActivityRecord{")?;
    record
        .split_whitespace()
        .map(|word| word.trim_end_matches('}'))
        .find(|word| word.contains('/'))
        .map(clean_line)
}

/// `pkg/pkg.Main` as Android writes it short: `pkg/.Main`.
fn short_component(component: &str) -> String {
    match component.split_once('/') {
        Some((package, class)) => match class.strip_prefix(package) {
            Some(rest) if rest.starts_with('.') => format!("{package}/{rest}"),
            _ => component.to_owned(),
        },
        None => component.to_owned(),
    }
}

/// Whether the lock screen shows, from `dumpsys window policy`: `mShowingLockscreen` (Android
/// 9 and before), `isStatusBarKeyguard`, the keyguard delegate's `showing=` and its state
/// monitor's `mIsShowing=` (Android 10+). `None` when it says none of them.
fn lock_screen(policy: &str) -> Option<bool> {
    const SHOWING: [&str; 5] = [
        "mShowingLockscreen=true",
        "mDreamingLockscreen=true",
        "isStatusBarKeyguard=true",
        "KeyguardShowing=true",
        "mIsShowing=true",
    ];
    const HIDDEN: [&str; 4] = [
        "mShowingLockscreen=false",
        "isStatusBarKeyguard=false",
        "KeyguardShowing=false",
        "mIsShowing=false",
    ];
    let mut seen = None;
    let mut delegate = false;
    for line in policy.lines() {
        let line = line.trim();
        if line.starts_with("KeyguardServiceDelegate") {
            delegate = true;
            continue;
        }
        if delegate {
            match line {
                "showing=true" => return Some(true),
                "showing=false" => seen = Some(false),
                _ => {}
            }
        }
        for word in line.split_whitespace() {
            if SHOWING.iter().any(|pattern| word.ends_with(pattern)) {
                return Some(true);
            }
            if HIDDEN.iter().any(|pattern| word.ends_with(pattern)) {
                seen = Some(false);
            }
        }
    }
    seen
}

/// `screen_app` `foreground`'s answer.
fn foreground_report(focus: &Focus, locked: Option<bool>) -> String {
    let window_component = focus
        .window
        .as_deref()
        .and_then(|title| title.split_whitespace().find(|word| word.contains('/')));
    let app = window_component
        .map(str::to_owned)
        .or_else(|| focus.app.clone());
    let mut lines = vec![match &app {
        Some(component) => format!("In front: {}", short_component(component)),
        None => "No app is in front: the window manager names no focused app.".to_owned(),
    }];
    if window_component.is_none() {
        if let Some(title) = &focus.window {
            lines.push(format!("Focused window: {title}"));
        }
    }
    let keyguard = focus
        .window
        .as_deref()
        .is_some_and(|title| title.contains("Keyguard"));
    if locked == Some(true) || keyguard {
        lines.push("The lock screen is showing.".to_owned());
    }
    lines.join("\n")
}

/// What `dumpsys package <pkg>` says of one package.
#[derive(Debug, Default, PartialEq, Eq)]
struct PackageFacts {
    version_name: Option<String>,
    version_code: Option<String>,
    target_sdk: Option<String>,
    installed: Option<String>,
    updated: Option<String>,
    installer: Option<String>,
    granted: Vec<String>,
    denied: Vec<String>,
}

/// The first `Package [<pkg>]` block of `dumpsys package <pkg>` (an updated system app has a
/// second, its factory version, under "Hidden system packages"), and its user 0 runtime
/// permissions. Android 11 moved `firstInstallTime` into the user's part; both are read.
fn parse_package_dump(text: &str, package: &str) -> Option<PackageFacts> {
    let lines: Vec<&str> = text.lines().collect();
    let header = format!("Package [{package}]");
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with(&header))?;
    let depth = indent(lines[start]);
    let block: Vec<&str> = lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| line.trim().is_empty() || indent(line) > depth)
        .collect();
    let mut facts = PackageFacts::default();
    let first = |slot: &mut Option<String>, value: &str| {
        let value = clean_line(value);
        if slot.is_none() && !value.is_empty() && value != "null" {
            *slot = Some(value);
        }
    };
    for line in &block {
        let line = line.trim();
        for word in line.split_whitespace() {
            if let Some(value) = word.strip_prefix("versionCode=") {
                first(&mut facts.version_code, value);
            } else if let Some(value) = word.strip_prefix("targetSdk=") {
                first(&mut facts.target_sdk, value);
            }
        }
        if let Some(value) = line.strip_prefix("versionName=") {
            first(&mut facts.version_name, value);
        } else if let Some(value) = line.strip_prefix("firstInstallTime=") {
            first(&mut facts.installed, value);
        } else if let Some(value) = line.strip_prefix("lastUpdateTime=") {
            first(&mut facts.updated, value);
        } else if let Some(value) = line.strip_prefix("installerPackageName=") {
            first(
                &mut facts.installer,
                value.split_whitespace().next().unwrap_or_default(),
            );
        }
    }
    // The runtime permissions of user 0, else of the package when no user part is printed.
    let scope: &[&str] = match block
        .iter()
        .position(|line| line.trim_start().starts_with("User 0:"))
    {
        Some(user) => {
            let depth = indent(block[user]);
            let end = block[user + 1..]
                .iter()
                .position(|line| !line.trim().is_empty() && indent(line) <= depth)
                .map_or(block.len(), |offset| user + 1 + offset);
            &block[user + 1..end]
        }
        None => &block,
    };
    if let Some(at) = scope
        .iter()
        .position(|line| line.trim() == "runtime permissions:")
    {
        let depth = indent(scope[at]);
        for line in scope[at + 1..]
            .iter()
            .take_while(|line| !line.trim().is_empty() && indent(line) > depth)
        {
            let Some((name, state)) = line.trim().split_once(": ") else {
                continue;
            };
            if !is_permission(name) {
                continue;
            }
            if state.starts_with("granted=true") {
                facts.granted.push(name.to_owned());
            } else if state.starts_with("granted=false") {
                facts.denied.push(name.to_owned());
            }
        }
    }
    Some(facts)
}

/// `screen_app` `info`'s answer.
fn package_report(package: &str, facts: &PackageFacts, running: &str) -> String {
    let known = |value: &Option<String>| value.clone().unwrap_or_else(|| "unknown".to_owned());
    let version = match (&facts.version_name, &facts.version_code) {
        (Some(name), Some(code)) => format!("{name} (versionCode {code})"),
        (Some(name), None) => name.clone(),
        (None, Some(code)) => format!("unknown (versionCode {code})"),
        (None, None) => "unknown".to_owned(),
    };
    let total = facts.granted.len() + facts.denied.len();
    let mut shown = 0usize;
    let mut take = |names: &[String]| -> Vec<String> {
        let room = PERMISSIONS_SHOWN.saturating_sub(shown);
        let kept: Vec<String> = names.iter().take(room).cloned().collect();
        shown += kept.len();
        kept
    };
    let granted = take(&facts.granted);
    let denied = take(&facts.denied);
    let mut groups = Vec::new();
    if !granted.is_empty() {
        groups.push(format!("granted {}", granted.join(", ")));
    }
    if !denied.is_empty() {
        groups.push(format!("denied {}", denied.join(", ")));
    }
    let mut permissions = if groups.is_empty() {
        "none".to_owned()
    } else {
        groups.join("; ")
    };
    if total > shown {
        permissions.push_str(&format!(" (the first {shown} of {total})"));
    }
    [
        format!("package: {package}"),
        format!("version: {version}"),
        format!("target SDK: {}", known(&facts.target_sdk)),
        format!("installed: {}", known(&facts.installed)),
        format!("updated: {}", known(&facts.updated)),
        format!(
            "installer: {}",
            facts
                .installer
                .clone()
                .unwrap_or_else(|| "none (adb or the system)".to_owned())
        ),
        format!("running: {running}"),
        format!("runtime permissions: {permissions}"),
    ]
    .join("\n")
}

/// pm's failures an agent can do something about, and what.
fn install_hint(answer: &str) -> &'static str {
    const HINTS: [(&str, &str); 8] = [
        ("INSTALL_FAILED_VERSION_DOWNGRADE", " The installed version is newer: pass downgrade true (debuggable apps only), or uninstall it first."),
        ("INSTALL_FAILED_ALREADY_EXISTS", " It is installed already: leave replace_existing at true to replace it."),
        ("INSTALL_FAILED_UPDATE_INCOMPATIBLE", " The installed app is signed with another key: uninstall it first (which erases its data)."),
        ("INSTALL_FAILED_INSUFFICIENT_STORAGE", " The device is short of storage."),
        ("INSTALL_FAILED_DEPRECATED_SDK_VERSION", " This Android refuses apps with so old a target SDK."),
        ("INSTALL_FAILED_OLDER_SDK", " The app needs a newer Android than the device runs."),
        ("INSTALL_FAILED_NO_MATCHING_ABIS", " The APK has no native code for this device's CPU."),
        ("INSTALL_FAILED_USER_RESTRICTED", " The device does not allow installs over adb; the user has to allow them in its developer options."),
    ];
    HINTS
        .iter()
        .find(|(code, _)| answer.contains(code))
        .map_or("", |(_, hint)| hint)
}

/// `android_logcat`'s answer: a header that says what was read, then the lines, oldest first.
/// `searched` is how many of the newest lines the filters were applied to, when they were: fewer
/// matches than asked for then says that older lines were not looked at.
fn log_report(
    text: &str,
    query: &Logcat,
    source: Option<LogSource>,
    truncated: bool,
    searched: Option<u64>,
) -> String {
    let mut lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with("--------- "))
        .collect();
    if let Some(needle) = &query.contains {
        let needle = needle.to_lowercase();
        lines.retain(|line| line.to_lowercase().contains(&needle));
    }
    let keep = usize::try_from(query.lines).unwrap_or(usize::MAX);
    if lines.len() > keep {
        lines.drain(..lines.len() - keep);
    }
    let window = match searched {
        Some(searched) if lines.len() < keep => {
            format!("\n(Only the newest {searched} lines of the log were searched.)")
        }
        _ => String::new(),
    };
    if lines.is_empty() {
        return format!("No log lines matched.{window}");
    }
    let mut filters = String::new();
    if let Some(level) = query.level.filter(|level| *level != "V") {
        filters.push_str(&format!(", level {level} and above"));
    }
    if let Some(tag) = &query.tag {
        filters.push_str(&format!(", tag {tag}"));
    }
    match (&query.package, source) {
        (Some(package), Some(LogSource::Uid(uid))) => {
            filters.push_str(&format!(", app {package} (uid {uid})"))
        }
        (Some(package), Some(LogSource::Pid(pid))) => {
            filters.push_str(&format!(", app {package} (pid {pid})"))
        }
        _ => {}
    }
    if let Some(needle) = &query.contains {
        filters.push_str(&format!(", containing \"{}\"", clean_line(needle)));
    }
    let buffers = match query.buffer {
        Some("all") => "all buffers".to_owned(),
        Some(buffer) => format!("buffer {buffer}"),
        None => "default buffers".to_owned(),
    };
    let mut report = format!(
        "logcat {buffers}, last {} line{}{filters}:",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" }
    );
    for line in lines {
        report.push('\n');
        report.push_str(&brief(line.trim_end(), LOG_LINE_CHARS));
    }
    if truncated {
        report.push_str("\n(The log was longer than 8 MiB; its newest lines were not read. Ask for fewer lines, or narrow it with package, tag or level.)");
    }
    report.push_str(&window);
    report
}

/// `getprop`'s `[key]: [value]` lines.
fn parse_getprop(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix('[')?;
            let (key, rest) = line.split_once("]: [")?;
            let value = rest.strip_suffix(']').unwrap_or(rest);
            Some((key.to_owned(), clean_line(value)))
        })
        .collect()
}

/// `screen_device` `info`'s device block.
fn device_report(props: &BTreeMap<String, String>) -> String {
    let get = |key: &str| props.get(key).filter(|value| !value.is_empty()).cloned();
    let or_unknown = |value: Option<String>| value.unwrap_or_else(|| "unknown".to_owned());
    let android = match (get("ro.build.version.release"), get("ro.build.version.sdk")) {
        (Some(release), Some(sdk)) => format!("{release} (SDK {sdk})"),
        (Some(release), None) => release,
        (None, Some(sdk)) => format!("unknown (SDK {sdk})"),
        (None, None) => "unknown".to_owned(),
    };
    let emulator = match get("ro.boot.qemu.avd_name") {
        Some(avd) => format!("yes (AVD {avd})"),
        None if get("ro.kernel.qemu").as_deref() == Some("1")
            || get("ro.boot.qemu").as_deref() == Some("1")
            || matches!(get("ro.hardware").as_deref(), Some("ranchu" | "goldfish")) =>
        {
            "yes".to_owned()
        }
        None => "no".to_owned(),
    };
    [
        "device:".to_owned(),
        format!("  maker: {}", or_unknown(get("ro.product.manufacturer"))),
        format!("  model: {}", or_unknown(get("ro.product.model"))),
        format!("  Android: {android}"),
        format!(
            "  security patch: {}",
            or_unknown(get("ro.build.version.security_patch"))
        ),
        format!("  ABI: {}", or_unknown(get("ro.product.cpu.abi"))),
        format!(
            "  locale: {}",
            or_unknown(get("persist.sys.locale").or_else(|| get("ro.product.locale")))
        ),
        format!("  emulator: {emulator}"),
    ]
    .join("\n")
}

/// `Physical size: 1080x2400` and `Override size: 720x1600` (`wm size`; `wm density` likewise).
fn physical_and_override(text: &str) -> (Option<String>, Option<String>) {
    let value = |prefix: &str| {
        text.lines().find_map(|line| {
            line.trim()
                .strip_prefix(prefix)
                .and_then(|rest| rest.split_once(':'))
                .map(|(_, value)| clean_line(value))
        })
    };
    (value("Physical"), value("Override"))
}

/// Whether the display's natural orientation is landscape, from `wm size`'s answer: its size is
/// the one rotation 0 draws in, whatever way the device is turned (the override when one is set,
/// as that is what the window manager rotates). A square display counts as portrait. `None` when
/// the answer holds no size.
///
/// The rotation numbers (`Surface.ROTATION_*`, what `user_rotation` holds) count from this
/// orientation, and by the turn of the *drawn graphics*, which is the opposite of the device's own
/// turn (`Display.getRotation()` in the Android reference: a device turned 90 degrees
/// counter-clockwise reads `ROTATION_90`). So on a display whose natural orientation is portrait,
/// portrait is 0, a device turned a quarter counter-clockwise (landscape_left) is 1, upside down is
/// 2 and turned a quarter clockwise (landscape_right) is 3. On a natural-landscape display (many
/// tablets) portrait is the turn of 3 and the others follow from it: AOSP's
/// `DisplayRotation.configure` sets the portrait rotation to `ROTATION_270` and the upside-down one
/// to `ROTATION_90` there, the landscape one to `ROTATION_0` and the seascape one to
/// `ROTATION_180`; a device that sets `config_reverseDefaultRotation` swaps the first two, which
/// is rare and not read here. Not checked on a real device.
fn natural_landscape(size: &str) -> Option<bool> {
    let (physical, overridden) = physical_and_override(size);
    let used = overridden.or(physical)?;
    let (width, height) = used.split_once('x')?;
    Some(width.trim().parse::<u32>().ok()? > height.trim().parse::<u32>().ok()?)
}

/// The rotation number that shows the device turned as `orientation` says (see
/// [`natural_landscape`]).
fn surface_rotation(orientation: Orientation, natural_landscape: bool) -> u8 {
    let portrait = if natural_landscape { 3 } else { 0 };
    let turned = match orientation {
        Orientation::Portrait => 0,
        Orientation::LandscapeLeft => 1,
        Orientation::PortraitUpsideDown => 2,
        Orientation::LandscapeRight => 3,
    };
    (portrait + turned) % 4
}

/// [`surface_rotation`] the other way: how the device is turned at rotation number `rotation`.
fn orientation_of(rotation: u8, natural_landscape: bool) -> Orientation {
    let portrait = if natural_landscape { 3 } else { 0 };
    match (rotation + 4 - portrait) % 4 {
        0 => Orientation::Portrait,
        1 => Orientation::LandscapeLeft,
        2 => Orientation::PortraitUpsideDown,
        _ => Orientation::LandscapeRight,
    }
}

/// A rotation of `user_rotation` (0-3) by the names `rotate` takes, or auto-rotate. `landscape`
/// is whether the display's natural orientation is ([`natural_landscape`]).
fn rotation_name(
    accelerometer: Option<&str>,
    rotation: Option<&str>,
    landscape: Option<bool>,
) -> String {
    if accelerometer == Some("1") {
        return "auto-rotate".to_owned();
    }
    match (
        rotation.and_then(|rotation| rotation.parse::<u8>().ok()),
        landscape,
    ) {
        (Some(rotation), Some(landscape)) if rotation < 4 => {
            format!("{} (locked)", orientation_of(rotation, landscape).name())
        }
        _ => "unknown".to_owned(),
    }
}

/// `cmd uimode night`'s `Night mode: no` as `light`.
fn night_mode(text: &str) -> &'static str {
    let mode = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Night mode:"))
        .map(str::trim);
    match mode {
        Some("yes") => "dark",
        Some("no") => "light",
        Some("auto") => "auto",
        Some(other) if other.starts_with("custom") => "custom",
        _ => "unknown",
    }
}

/// An animation scale, `null` (never set) being its default 1.
fn scale_value(value: Option<&str>) -> String {
    match value {
        Some(value) => clean_line(value),
        None => "1 (default)".to_owned(),
    }
}

/// `screen_device` `info`'s display block, from [`display_command`]'s parts.
fn display_report(parts: &BTreeMap<String, String>) -> String {
    let empty = String::new();
    let part = |name: &str| parts.get(name).unwrap_or(&empty).as_str();
    let pair = |(physical, overridden): (Option<String>, Option<String>), unit: &str| match (
        physical, overridden,
    ) {
        (Some(physical), Some(overridden)) => format!("{physical}{unit} (override {overridden})"),
        (Some(physical), None) => format!("{physical}{unit}"),
        (None, Some(overridden)) => format!("override {overridden}"),
        (None, None) => "unknown".to_owned(),
    };
    let auto = match setting_of(parts, "accelerometer") {
        Some("1") => "on",
        Some("0") => "off",
        _ => "unknown",
    };
    let locked = match (
        setting_of(parts, "rotation").and_then(|value| value.parse::<u8>().ok()),
        natural_landscape(part("size")),
    ) {
        (Some(rotation), Some(landscape)) if rotation < 4 => {
            orientation_of(rotation, landscape).name().to_owned()
        }
        _ => "unknown".to_owned(),
    };
    // `user_rotation` is the rotation a lock holds; while auto-rotate is on the sensor decides.
    let rotation = if auto == "on" {
        format!("auto (when locked: {locked})")
    } else {
        locked
    };
    [
        "display:".to_owned(),
        format!("  size: {}", pair(physical_and_override(part("size")), "")),
        format!(
            "  density: {}",
            pair(physical_and_override(part("density")), " dpi")
        ),
        format!("  rotation: {rotation}"),
        format!("  auto-rotate: {auto}"),
        format!("  appearance: {}", night_mode(part("night"))),
        format!(
            "  animation scale: {}",
            scale_value(setting_of(parts, "animation"))
        ),
    ]
    .join("\n")
}

/// `dumpsys battery`'s `key: value` lines.
#[derive(Debug, Default, PartialEq, Eq)]
struct Battery {
    level: Option<u32>,
    scale: Option<u32>,
    status: Option<u32>,
    sources: Vec<&'static str>,
    /// Whether any `… powered:` line was there at all.
    known_sources: bool,
}

fn parse_battery(text: &str) -> Battery {
    let mut battery = Battery::default();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "level" => battery.level = value.parse().ok(),
            "scale" => battery.scale = value.parse().ok(),
            "status" => battery.status = value.parse().ok(),
            source @ ("AC powered" | "USB powered" | "Wireless powered" | "Dock powered") => {
                battery.known_sources = true;
                if value == "true" {
                    battery.sources.push(match source {
                        "AC powered" => "AC",
                        "USB powered" => "USB",
                        "Wireless powered" => "wireless",
                        _ => "dock",
                    });
                }
            }
            _ => {}
        }
    }
    battery
}

/// `dumpsys power`'s wakefulness and display state.
#[derive(Debug, Default, PartialEq, Eq)]
struct Power {
    wakefulness: Option<String>,
    display: Option<String>,
}

fn parse_power(text: &str) -> Power {
    let mut power = Power::default();
    for line in text.lines() {
        for word in line.split_whitespace() {
            if power.wakefulness.is_none() {
                if let Some(value) = word
                    .strip_prefix("mWakefulness=")
                    .or_else(|| word.strip_prefix("getWakefulnessLocked()="))
                {
                    power.wakefulness = Some(clean_line(value));
                }
            }
        }
        if power.display.is_none() {
            if let Some((_, state)) = line.split_once("Display Power: state=") {
                power.display = state.split_whitespace().next().map(clean_line);
            }
        }
    }
    power
}

/// `screen_device` `info`'s power block, from [`power_command`]'s parts.
fn power_report(parts: &BTreeMap<String, String>) -> String {
    let empty = String::new();
    let part = |name: &str| parts.get(name).unwrap_or(&empty).as_str();
    let battery = parse_battery(part("battery"));
    let level = match (battery.level, battery.scale) {
        (Some(level), Some(scale)) if scale > 0 && scale != 100 => {
            format!("{}%", level * 100 / scale)
        }
        (Some(level), _) => format!("{level}%"),
        _ => "unknown".to_owned(),
    };
    let status = match battery.status {
        Some(2) => " (charging)",
        Some(3) => " (discharging)",
        Some(4) => " (not charging)",
        Some(5) => " (full)",
        _ => "",
    };
    let plugged = if !battery.sources.is_empty() {
        format!("yes ({})", battery.sources.join(", "))
    } else if battery.known_sources {
        "no".to_owned()
    } else {
        "unknown".to_owned()
    };
    let power = parse_power(part("power"));
    let screen = match (power.display.as_deref(), power.wakefulness.as_deref()) {
        (Some("ON"), _) => "on".to_owned(),
        (Some("OFF"), _) => "off".to_owned(),
        (Some(state), _) => format!("{} (always-on display)", state.to_ascii_lowercase()),
        (None, Some("Awake")) => "on".to_owned(),
        (None, Some(_)) => "off".to_owned(),
        (None, None) => "unknown".to_owned(),
    };
    let lock = match lock_screen(part("policy")) {
        Some(true) => "showing",
        Some(false) => "not showing",
        None => "unknown",
    };
    [
        "power:".to_owned(),
        format!("  battery: {level}{status}"),
        format!("  plugged in: {plugged}"),
        format!("  screen: {screen}"),
        format!(
            "  wakefulness: {}",
            power.wakefulness.unwrap_or_else(|| "unknown".to_owned())
        ),
        format!("  lock screen: {lock}"),
    ]
    .join("\n")
}

/// One notification of `dumpsys notification --noredact`.
#[derive(Debug, PartialEq, Eq)]
struct Notice {
    package: String,
    title: Option<String>,
    text: Option<String>,
}

/// The extra of a notification that is being read, line by line: its value may go on over
/// several lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Extra {
    Title,
    Text,
}

/// One `NotificationRecord` of the dump, while it is read.
struct Record {
    depth: usize,
    notice: Notice,
    /// The indent of its `extras={` line, while inside that block.
    extras: Option<usize>,
    /// The title or text whose value is being read, and that value so far.
    value: Option<(Extra, String)>,
}

impl Record {
    /// The value being read is complete.
    fn settle(&mut self) {
        if let Some((extra, value)) = self.value.take() {
            let slot = match extra {
                Extra::Title => &mut self.notice.title,
                Extra::Text => &mut self.notice.text,
            };
            *slot = extra_value(&value);
        }
    }

    /// `key=value`, the start of an extra.
    fn extra(&mut self, line: &str) {
        self.settle();
        let wanted = [
            ("android.title=", Extra::Title, self.notice.title.is_none()),
            ("android.text=", Extra::Text, self.notice.text.is_none()),
        ];
        for (prefix, extra, open) in wanted {
            if let Some(value) = line.strip_prefix(prefix).filter(|_| open) {
                self.value = Some((extra, value.to_owned()));
            }
        }
    }

    fn finish(mut self) -> Notice {
        self.settle();
        self.notice
    }
}

/// A line of the dump's own structure after the active list, at the list's own indent: a
/// section (`Lights List:`) or a field of the manager (`mArchive=…`, `mUseAttentionLight=…`) —
/// not the text of an extra.
fn structural(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.ends_with(':')
        || trimmed.strip_prefix('m').is_some_and(|rest| {
            rest.starts_with(|character: char| character.is_ascii_uppercase())
                && rest
                    .find(|character| character == '=' || character == ':')
                    .is_some_and(|at| {
                        rest[..at]
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '_')
                    })
        })
}

/// `key=…` of an extra, as `NotificationRecord`'s dump prints it inside `extras={`.
fn is_extra_key(line: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| !key.is_empty() && !key.contains(char::is_whitespace))
}

/// The `NotificationRecord(…: pkg=<pkg> …)` blocks of the active list ("Notification List:",
/// when the dump has one) and their `android.title=` and `android.text=` extras. An extra's value
/// is printed raw, so a text of several lines goes on at the start of the next lines: inside a
/// record's `extras={ … }` a line is a new extra only where the dump puts one, two spaces in, and
/// the list ends only at a line of the dump's own structure.
fn parse_notifications(text: &str) -> Vec<Notice> {
    let lines: Vec<&str> = text.lines().collect();
    let (start, list_depth) = match lines
        .iter()
        .position(|line| line.trim() == "Notification List:")
    {
        Some(at) => (at + 1, Some(indent(lines[at]))),
        None => (0, None),
    };
    let mut notices = Vec::new();
    let mut current: Option<Record> = None;
    for line in &lines[start..] {
        let trimmed = line.trim();
        let depth = indent(line);
        if let Some(record) = current.as_mut() {
            if let Some(extras) = record.extras {
                if trimmed == "}" && depth == extras {
                    record.settle();
                    record.extras = None;
                } else if depth == extras + 2 && is_extra_key(trimmed) {
                    record.extra(trimmed);
                } else if let Some((_, value)) = record.value.as_mut() {
                    value.push('\n');
                    value.push_str(line);
                }
                continue;
            }
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("NotificationRecord(") {
            notices.extend(current.take().map(Record::finish));
            let package = trimmed
                .split_whitespace()
                .find_map(|word| word.strip_prefix("pkg="))
                .map_or_else(|| "unknown".to_owned(), clean_line);
            current = Some(Record {
                depth,
                notice: Notice {
                    package,
                    title: None,
                    text: None,
                },
                extras: None,
                value: None,
            });
            continue;
        }
        match list_depth {
            Some(list) if depth <= list => {
                if depth == list && structural(line) {
                    break;
                }
                // The rest of a value printed raw (a ticker text of several lines).
                continue;
            }
            None if current.as_ref().is_some_and(|record| depth <= record.depth) => {
                notices.extend(current.take().map(Record::finish));
                continue;
            }
            _ => {}
        }
        let Some(record) = current.as_mut() else {
            continue;
        };
        if depth <= record.depth {
            continue;
        }
        if trimmed == "extras={" {
            record.extras = Some(depth);
            continue;
        }
        // A dump without the block: the extras on lines of their own.
        if record.notice.title.is_none() {
            if let Some(value) = trimmed.strip_prefix("android.title=") {
                record.notice.title = extra_value(value);
            }
        }
        if record.notice.text.is_none() {
            if let Some(value) = trimmed.strip_prefix("android.text=") {
                record.notice.text = extra_value(value);
            }
        }
    }
    notices.extend(current.map(Record::finish));
    notices
}

/// An extra as the dump prints it: `String (Hello)`, `SpannableString (Hi)`, `null`, or redacted
/// `String [length=5]`.
fn extra_value(value: &str) -> Option<String> {
    let value = value.trim();
    if value == "null" || value.is_empty() {
        return None;
    }
    if let Some((_, inner)) = value.split_once(" (") {
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        return Some(brief(&clean_line(inner), NOTIFICATION_CHARS));
    }
    if value.contains("[length=") {
        return Some("(hidden by this Android version)".to_owned());
    }
    Some(brief(&clean_line(value), NOTIFICATION_CHARS))
}

/// `screen_device` `info`'s notifications block.
fn notification_report(notices: &[Notice]) -> String {
    if notices.is_empty() {
        return "notifications: none showing".to_owned();
    }
    let mut lines = vec![if notices.len() > NOTIFICATIONS_SHOWN {
        format!(
            "notifications: {} showing (the first {NOTIFICATIONS_SHOWN} listed)",
            notices.len()
        )
    } else {
        format!("notifications: {} showing", notices.len())
    }];
    for notice in notices.iter().take(NOTIFICATIONS_SHOWN) {
        let mut parts = Vec::new();
        if let Some(title) = &notice.title {
            parts.push(format!("title \"{title}\""));
        }
        if let Some(text) = &notice.text {
            parts.push(format!("text \"{text}\""));
        }
        lines.push(format!(
            "  {}: {}",
            notice.package,
            if parts.is_empty() {
                "no title or text".to_owned()
            } else {
                parts.join(", ")
            }
        ));
    }
    lines.join("\n")
}

/// What `screen_device` `lock` says the sleep key did: whether the screen went off, and whether
/// the lock screen is up. Both are the device's, read once, [`LOCK_SETTLE`] after the key: a
/// device may lock later than that, so a lock screen not showing then is what was seen, not a
/// claim that the device never locks.
fn lock_report(power: &Power, locked: Option<bool>) -> String {
    let off = match (power.display.as_deref(), power.wakefulness.as_deref()) {
        (Some("ON"), _) | (_, Some("Awake")) => Some(false),
        (Some("OFF"), _) | (_, Some("Asleep")) => Some(true),
        _ => None,
    };
    match (off, locked) {
        (Some(true), Some(true)) => "The screen is off and locked.",
        (Some(true), Some(false)) => "The screen is off; the lock screen was not showing half a second later.",
        (Some(true), None) => "The screen is off; whether the lock screen is showing could not be read.",
        (Some(false), _) => "The screen is still on after the sleep key; something may be keeping the device awake.",
        (None, Some(true)) => "The lock screen is showing; whether the screen is off could not be read.",
        (None, Some(false)) => "The lock screen is not showing; whether the screen is off could not be read.",
        (None, None) => "Neither whether the screen is off nor whether the lock screen is showing could be read.",
    }
    .to_owned()
}

/// Whether the input method shows, from `dumpsys input_method`: its `mInputShown`, a field of
/// every Android version from 9 to 15 and later (AOSP's `InputMethodManagerService.dump`, which
/// prints it with `mShowRequested` and others on one line up to Android 13, and
/// `ImeVisibilityStateComputer.dump`, on a line of its own, since 14). Showing anywhere in the
/// dump is showing; `None` where it holds no such field (an Android before 9, or a vendor's).
fn input_shown(dump: &str) -> Option<bool> {
    let mut seen = None;
    for word in dump.split_whitespace() {
        match word {
            "mInputShown=true" => return Some(true),
            "mInputShown=false" => seen = Some(false),
            _ => {}
        }
    }
    seen
}

/// One button of the dialog showing: its label as the device wrote it, and where a tap on it
/// lands (`None` for one with no area).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DialogButton {
    label: String,
    center: Option<(u32, u32)>,
}

/// The resource ids of an `AlertDialog`'s positive, negative and neutral buttons.
const DIALOG_BUTTON_IDS: [&str; 3] = [
    "android:id/button1",
    "android:id/button2",
    "android:id/button3",
];
/// The apps that draw the system's permission dialogs, whose buttons carry ids of their own
/// (`permission_allow_button`, `permission_deny_button`, …) that differ from Android to Android.
const PERMISSION_CONTROLLERS: [&str; 2] = [
    "com.android.permissioncontroller",
    "com.google.android.permissioncontroller",
];

/// The buttons of the dialog showing, in the order of the element tree: the nodes with a dialog
/// button's resource id, and the clickable nodes of the permission controller. A button's label is
/// its text, else its description; one with neither cannot be named, so it is left out. A node
/// inside a button of the same label is that button, not a second one. Another app's own dialog
/// with other ids is not found, and neither is told apart from the permission controller's own
/// screens, which draw the same kind of nodes.
fn dialog_buttons(root: &UiNode) -> Vec<DialogButton> {
    /// `enclosing` is the label of the button the node is inside of, if it is inside one.
    fn collect<'a>(node: &'a UiNode, found: &mut Vec<DialogButton>, enclosing: Option<&'a str>) {
        let by_id = node
            .id
            .as_deref()
            .is_some_and(|id| DIALOG_BUTTON_IDS.contains(&id));
        let by_app = node.clickable
            && node
                .package
                .as_deref()
                .is_some_and(|package| PERMISSION_CONTROLLERS.contains(&package));
        let mut inside = enclosing;
        if by_id || by_app {
            let label = node
                .text
                .as_deref()
                .filter(|text| !text.trim().is_empty())
                .unwrap_or(&node.name);
            if !label.trim().is_empty() {
                if enclosing != Some(label) {
                    found.push(DialogButton {
                        label: label.to_owned(),
                        center: node.bounds.and_then(|bounds| bounds.center()),
                    });
                }
                inside = Some(label);
            }
        }
        for child in &node.children {
            collect(child, found, inside);
        }
    }
    let mut found = Vec::new();
    collect(root, &mut found, None);
    found
}

/// A button label as an answer says it: one line, without quote-like or invisible characters.
fn shown_label(label: &str) -> String {
    sanitize_for_message(label)
}

fn labels(buttons: &[DialogButton]) -> Vec<&str> {
    buttons.iter().map(|button| button.label.as_str()).collect()
}

/// `"Allow", "Don't allow"`: the labels of up to [`BUTTONS_SHOWN`] buttons, each in quotes (a
/// shown label holds none of its own).
fn label_list(buttons: &[DialogButton]) -> String {
    let mut list = buttons
        .iter()
        .take(BUTTONS_SHOWN)
        .map(|button| format!("\"{}\"", shown_label(&button.label)))
        .collect::<Vec<_>>()
        .join(", ");
    if buttons.len() > BUTTONS_SHOWN {
        list.push_str(&format!(" and {} more", buttons.len() - BUTTONS_SHOWN));
    }
    list
}

/// `screen_device` `info`'s dialog line.
fn dialog_report(buttons: &[DialogButton]) -> String {
    if buttons.is_empty() {
        "dialog: none showing".to_owned()
    } else {
        format!("dialog: showing; buttons: {}", label_list(buttons))
    }
}

/// The one button `wanted` names. A label matches as the device wrote it, else as an answer lists
/// it (one line, a typographic apostrophe read as an ASCII one and the mark of a change at its
/// end), else as that without the mark — what a model writes who copied a label out of an answer
/// or off a screenshot. The first of these that matches any button decides, and it must match
/// exactly one; otherwise nothing is pressed, and the answer lists the buttons.
fn pick_button<'a>(buttons: &'a [DialogButton], wanted: &str) -> Result<&'a DialogButton, String> {
    fn unmarked(shown: &str) -> &str {
        shown.strip_suffix('\u{2026}').unwrap_or(shown)
    }
    let wanted_shown = shown_label(wanted);
    let tiers: [&dyn Fn(&DialogButton) -> bool; 3] = [
        &|button| button.label == wanted,
        &|button| shown_label(&button.label) == wanted,
        &|button| {
            let shown = shown_label(&button.label);
            unmarked(&shown) == unmarked(&wanted_shown)
        },
    ];
    for tier in tiers {
        let found: Vec<&DialogButton> = buttons.iter().filter(|button| tier(button)).collect();
        match found.as_slice() {
            [] => continue,
            [only] => return Ok(only),
            _ => {
                return Err(format!(
                    "Several buttons of the dialog are labelled \"{wanted_shown}\", so none was pressed. The buttons showing: {}. Take a preview_snapshot and press the one you mean with preview_click.",
                    label_list(buttons)
                ))
            }
        }
    }
    Err(format!(
        "No button of the dialog is labelled \"{wanted_shown}\". The buttons showing: {}.",
        label_list(buttons)
    ))
}

/// `android_shell`'s answer: the exit status, then each stream that printed anything.
fn shell_report(result: &ShellResult) -> String {
    let mut report = match result.exit {
        Some(code) => format!("exit {code}"),
        None if result.timed_out => STOPPED_STATUS.to_owned(),
        None => NO_FRAMED_SHELL.to_owned(),
    };
    match shell_streams(result) {
        Some(streams) => report.push_str(&streams),
        None => report.push_str("\n(no output)"),
    }
    report
}

/// `android_shell`'s answer for a command that had not ended when it was stopped, `after` into
/// it: that, and what it had printed by then.
fn stopped_report(result: &ShellResult, after: Duration) -> String {
    let mut report = format!(
        "Stopped after {} ms: the command did not end.",
        after.as_millis()
    );
    if let Some(streams) = shell_streams(result) {
        report.push_str(" Output so far:");
        report.push_str(&streams);
    }
    report
}

/// Each stream that printed anything, as a block on lines of its own, and the cut when there was
/// one; `None` when nothing was printed.
fn shell_streams(result: &ShellResult) -> Option<String> {
    let block = |name: &str, bytes: &[u8]| -> Option<String> {
        let text = clean(bytes);
        let text = text.trim_end_matches('\n');
        (!text.is_empty()).then(|| format!("\n{name}:\n{text}"))
    };
    let stdout = block("stdout", &result.stdout);
    let stderr = block("stderr", &result.stderr);
    if stdout.is_none() && stderr.is_none() {
        return None;
    }
    let mut streams: String = stdout.into_iter().chain(stderr).collect();
    if result.truncated {
        streams.push_str("\n(cut at 32 KiB)");
    }
    Some(streams)
}
