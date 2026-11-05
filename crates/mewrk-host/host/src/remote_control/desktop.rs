//! The desktop side of `screen_clipboard`, and the paste a `preview_input` batch makes with
//! `type`'s `via` "paste", on a desktop or virtual machine connected over VNC.
//! (`preview_zoom` and `preview_wait` work on the session's own frames and
//! coordinate frame, so they are the session's: `session::platform`.)
//!
//! A clipboard call is parsed here ([`DesktopCall::parse`]) before anything reaches the
//! server — every parameter with the bounds its schema states (`builtin_schemas.rs`; a
//! desktop's clipboard takes less than the schema's 100,000 characters, which are an
//! iPhone's) — declares what it does to the screen ([`DesktopCall::effect`], which decides
//! the input lock and the read-only rule in `session::platform`) and how long it may take
//! ([`DesktopCall::time`]), and runs on the session's driver through [`Driver::desktop`]
//! ([`DesktopControl`]). A paste is a step of a `preview_input` batch: the session asks what
//! can be asked of it before the batch's first action ([`paste_refusal`]) and runs it at its
//! turn, under the batch's input lock ([`paste`]), pressing its shortcut with
//! [`Driver::chord`].
//!
//! The clipboard is the VNC server's: any text when the server has the
//! Extended Clipboard, Latin-1 only when it has just the classic cut text
//! (`vnc::clipboard`). Text that Latin-1 cannot carry is refused here, before
//! anything is sent, so a paste never presses its shortcut over a clipboard
//! that did not take the text. A clipboard write holds the input lock as a
//! paste does, so it never lands between a paste's text and its shortcut.

use std::time::Duration;

use serde_json::Value;

use crate::{
    model::JsonObject,
    preview_launch_config::sanitize_for_message,
    remote_control::{
        driver::{ClipboardChannel, DesktopControl, Driver, DriverError, Platform},
        keys::{self, Key, KeyChord, NamedKey},
        session::{Effect, OpError, Step},
    },
    tool_executor::screen_tools::{action_in, CallFiles, SCREEN_CALL_TIME},
};

/// The longest text a desktop's clipboard takes, in characters: what `screen_clipboard`
/// writes there and what a `preview_input` paste puts on it (the paste's schema bound,
/// `builtin_schemas::PREVIEW_INPUT_PASTE_MAX_TEXT_CHARS`; `screen_clipboard`'s own schema
/// bound is the iPhone's 100,000). At most 4 bytes each as UTF-8 with CRLF line ends, so
/// 240,001 bytes with the NUL, within what every VNC server takes
/// (`vnc::MAX_SENT_CLIPBOARD`, which the driver checks).
pub(crate) const MAX_TEXT_CHARS: usize = 60_000;
/// How long a read waits for the server to send its clipboard.
const READ_WAIT: Duration = Duration::from_secs(2);
/// Between putting the text on the clipboard and pressing the shortcut, for
/// the server to take the clipboard over: TigerVNC claims the selection, QEMU
/// tells the guest's agent (research-rfb §7.2).
pub(crate) const PASTE_PAUSE: Duration = Duration::from_millis(150);
/// The most of the clipboard a read shows, in bytes of UTF-8.
const MAX_SHOWN_BYTES: usize = 64 * 1024;
/// What a refusal of text for the clipboard offers instead, inside a `preview_input` batch.
const TYPE_INSTEAD: &str =
    " Type it with via \"auto\" instead, as key presses, which not every server takes.";

/// One call of `screen_clipboard` on a desktop, parsed.
pub(crate) struct DesktopCall {
    op: Op,
}

enum Op {
    /// `screen_clipboard` `read`.
    Read,
    /// `screen_clipboard` `write`.
    Write { text: String },
}

/// A paste shortcut as the model wrote it (sanitised, for the answers) with the chord it
/// reads as.
pub(crate) type PasteChord = (String, KeyChord);

/// `values` as a sentence's list: `a, b and c`.
fn all_of(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Refuses any parameter but `allowed`. `serverId` is the tool layer's, and
/// always allowed.
fn only(input: &JsonObject, tool: &str, allowed: &[&str]) -> Result<(), String> {
    match input
        .keys()
        .find(|key| key.as_str() != "serverId" && !allowed.contains(&key.as_str()))
    {
        Some(unknown) => {
            let mut takes = vec!["serverId"];
            takes.extend_from_slice(allowed);
            Err(format!(
                "Unknown parameter \"{}\"; {tool} takes {}.",
                sanitize_for_message(unknown),
                all_of(&takes)
            ))
        }
        None => Ok(()),
    }
}

/// An optional string of at most `max` characters; `None` when absent (or
/// `null`).
fn optional_string<'a>(
    input: &'a JsonObject,
    key: &str,
    max: usize,
    example: &str,
) -> Result<Option<&'a str>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let count = text.chars().count();
            if count > max {
                return Err(format!(
                    "\"{key}\" has {count} characters; at most {max} fit in one call."
                ));
            }
            Ok(Some(text))
        }
        Some(_) => Err(format!("\"{key}\" must be a string{example}.")),
    }
}

/// `count` characters, as a sentence says it.
pub(crate) fn characters(count: usize) -> String {
    if count == 1 {
        "1 character".to_owned()
    } else {
        format!("{count} characters")
    }
}

/// A character as an error shows it: itself in quotes, or its code point when
/// it would not show (a space, a control or format character).
fn shown_character(character: char) -> String {
    let alone = character.to_string();
    if character.is_whitespace() || sanitize_for_message(&alone) != alone {
        format!("U+{:04X}", u32::from(character))
    } else {
        format!("\"{alone}\"")
    }
}

/// The paste shortcut: a chord of keys a desktop has, read with the same
/// names as `preview_input`'s `key`.
pub(crate) fn paste_chord(text: &str) -> Result<KeyChord, String> {
    if text.trim().is_empty() {
        return Err(
            "\"chord\" is empty; give a shortcut such as ctrl+v, or leave it out for the desktop's default."
                .to_owned(),
        );
    }
    let chord = keys::parse_chord(text, Platform::Desktop)
        .map_err(|error| format!("\"chord\" is not a shortcut: {error}"))?;
    // `mod` has no keysym of its own: the batch says which key it is once it holds the screen.
    if chord.key != Key::Named(NamedKey::Mod) && keys::keysym(chord.key).is_none() {
        return Err(format!(
            "\"chord\" ends in \"{}\", a phone button; a desktop has no such key. Give a shortcut such as ctrl+v.",
            sanitize_for_message(text.rsplit('+').next().unwrap_or(text).trim())
        ));
    }
    Ok(chord)
}

/// What a desktop pastes with when the call names no shortcut: Command-V on
/// macOS Screen Sharing (whose Command is Super_L, which `cmd` sends), Ctrl-V
/// everywhere else.
fn default_chord(macos: bool) -> PasteChord {
    let (name, modifier) = if macos {
        ("cmd+v", NamedKey::Meta)
    } else {
        ("ctrl+v", NamedKey::Control)
    };
    (
        name.to_owned(),
        KeyChord {
            modifiers: vec![Key::Named(modifier)],
            key: Key::Char('v'),
        },
    )
}

/// The server's channel, refusing first what it cannot carry: on a Latin-1
/// channel, text with any other character goes nowhere, and says which.
fn channel_for(
    desktop: &dyn DesktopControl,
    text: &str,
    instead: &str,
) -> Result<ClipboardChannel, OpError> {
    let channel = desktop.clipboard_channel();
    if channel == ClipboardChannel::Latin1 {
        if let Some(character) = text.chars().find(|character| u32::from(*character) > 0xff) {
            return Err(OpError::Said(format!(
                "This VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), so text with {} cannot go on its clipboard.{instead}",
                shown_character(character)
            )));
        }
    }
    Ok(channel)
}

/// Why `text` cannot be pasted on this server, when it cannot: it is past what a VNC server's
/// clipboard takes, or it has a character the server's channel cannot carry (named). A
/// `preview_input` batch asks this of every paste before its first action, so a refusal
/// sends nothing.
pub(crate) fn paste_refusal(desktop: &dyn DesktopControl, text: &str) -> Option<String> {
    let count = text.chars().count();
    if count > MAX_TEXT_CHARS {
        return Some(format!(
            "\"text\" has {count} characters; at most {MAX_TEXT_CHARS} fit on a desktop's clipboard."
        ));
    }
    match channel_for(desktop, text, TYPE_INSTEAD) {
        Err(OpError::Said(message)) => Some(message),
        _ => None,
    }
}

/// How long ago `elapsed` was, as a clipboard read says it.
fn ago(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 1 {
        "just now".to_owned()
    } else if seconds < 120 {
        format!("{seconds} s ago")
    } else if seconds < 2 * 60 * 60 {
        format!("{} min ago", seconds / 60)
    } else {
        format!("{} h ago", seconds / 3600)
    }
}

/// The clipboard text as a read shows it: no control characters but line
/// feeds and tabs, at most [`MAX_SHOWN_BYTES`].
fn shown_text(text: &str) -> (String, usize, bool) {
    let cleaned: String = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect();
    let count = cleaned.chars().count();
    if cleaned.len() <= MAX_SHOWN_BYTES {
        return (cleaned, count, false);
    }
    let mut end = MAX_SHOWN_BYTES;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    (cleaned[..end].to_owned(), count, true)
}

impl DesktopCall {
    /// Parses one call of `tool` from its JSON input; every bound in the schema is checked here
    /// with the same numbers — but for `text`, which a desktop's clipboard takes
    /// [`MAX_TEXT_CHARS`] of where the schema allows an iPhone's 100,000: past that it is
    /// refused here, before anything is sent.
    pub(crate) fn parse(
        tool: &str,
        input: &JsonObject,
        _files: &CallFiles,
    ) -> Result<Self, String> {
        let op = match tool {
            "screen_clipboard" => {
                only(input, tool, &["action", "text"])?;
                let action = action_in(input, &["read", "write"], true)?;
                let text = optional_string(input, "text", MAX_TEXT_CHARS, "")?;
                match (action, text) {
                    (Some("read"), None) => Op::Read,
                    (Some("read"), Some(_)) => {
                        return Err(
                            "\"text\" does not apply to read; leave it out, or use action write to put text on the clipboard."
                                .to_owned(),
                        )
                    }
                    (_, Some(text)) => Op::Write {
                        text: text.to_owned(),
                    },
                    (_, None) => {
                        return Err(
                            "\"text\" is required for write: the text to put on the clipboard (an empty one clears it)."
                                .to_owned(),
                        )
                    }
                }
            }
            other => {
                return Err(format!(
                    "{} is not a desktop tool this module runs.",
                    sanitize_for_message(other)
                ))
            }
        };
        Ok(Self { op })
    }

    pub(crate) fn effect(&self) -> Effect {
        match self.op {
            Op::Read => Effect::Look,
            // A clipboard write changes no picture, but holds the input lock: a
            // paste puts its text on the clipboard and presses the shortcut
            // under that lock, and a write in between would paste other text.
            Op::Write { .. } => Effect::Act,
        }
    }

    /// The call's deadline and envelope (default: `SCREEN_CALL_DEADLINE` / `PREVIEW_TOOL_TIMEOUT`).
    pub(crate) fn time(&self) -> (Duration, Duration) {
        SCREEN_CALL_TIME
    }

    /// For the session's error prefix and logs: "putting 42 characters on the desktop's clipboard".
    pub(crate) fn what(&self) -> String {
        match &self.op {
            Op::Read => "reading the desktop's clipboard".to_owned(),
            Op::Write { text } => format!(
                "putting {} on the desktop's clipboard",
                characters(text.chars().count())
            ),
        }
    }

    pub(crate) fn run(&self, driver: &dyn Driver, step: &Step<'_>) -> Result<String, OpError> {
        let desktop = driver.desktop().ok_or_else(|| {
            OpError::Said("this screen is not a desktop connected over VNC".to_owned())
        })?;
        match &self.op {
            Op::Read => read(desktop, step),
            Op::Write { text } => {
                let channel = channel_for(desktop, text, "")?;
                step.proceed("putting the text on the desktop's clipboard")?;
                desktop.set_clipboard(text)?;
                Ok(format!(
                    "Put {} on the desktop's clipboard ({}).",
                    characters(text.chars().count()),
                    channel_name(channel)
                ))
            }
        }
    }
}

/// The channel as the answers name it.
fn channel_name(channel: ClipboardChannel) -> &'static str {
    match channel {
        ClipboardChannel::Extended => "UTF-8, Extended Clipboard",
        ClipboardChannel::Latin1 => "Latin-1",
    }
}

/// A driver's reason as a whole answer: its first letter capitalised, and a
/// full stop at its end.
fn sentence(reason: &str) -> String {
    let mut characters = reason.trim().chars();
    let mut answer: String = match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    };
    if !answer.ends_with('.') {
        answer.push('.');
    }
    answer
}

/// `screen_clipboard` `read`: the server's text, waited for at most
/// [`READ_WAIT`] and never past the call's time. "Empty" only when the server
/// said so: one that did not answer (`Unsupported`, whose text says why) is
/// answered with that.
fn read(desktop: &dyn DesktopControl, step: &Step<'_>) -> Result<String, OpError> {
    let clipboard = match desktop.clipboard(READ_WAIT.min(step.remaining())) {
        Ok(clipboard) => clipboard,
        Err(DriverError::Unsupported(why)) => return Ok(sentence(&why)),
        Err(error) => return Err(OpError::Driver(error)),
    };
    match clipboard.filter(|clipboard| !clipboard.text.is_empty()) {
        Some(clipboard) => {
            let (shown, count, cut) = shown_text(&clipboard.text);
            let mut answer = format!(
                "Clipboard ({}, received {}; {}):\n{shown}",
                characters(count),
                ago(clipboard.received.elapsed()),
                channel_name(desktop.clipboard_channel())
            );
            if cut {
                answer.push_str("\n(cut at 64 KiB)");
            }
            Ok(answer)
        }
        None => Ok(match desktop.clipboard_channel() {
            ClipboardChannel::Extended => "The clipboard is empty.".to_owned(),
            ClipboardChannel::Latin1 => "This server has not sent its clipboard since the connection was made; it sends it when its clipboard changes, so copy the text there again and read once more.".to_owned(),
        }),
    }
}

/// A paste (`preview_input` `type` with `via` "paste"): the text onto the clipboard, a pause for
/// the server to take it, then the shortcut — `chord` as the model wrote it, else the desktop's
/// default. Returns the shortcut as the answer names it. What cannot go on the clipboard is
/// refused before anything is sent; a call stopped after the text went says the shortcut was
/// not pressed.
pub(crate) fn paste(
    driver: &dyn Driver,
    desktop: &dyn DesktopControl,
    step: &Step<'_>,
    text: &str,
    chord: Option<&PasteChord>,
) -> Result<String, OpError> {
    channel_for(desktop, text, TYPE_INSTEAD)?;
    let (shown, chord) = match chord {
        Some(chord) => chord.clone(),
        None => default_chord(desktop.is_macos()),
    };
    step.proceed("putting the text on the desktop's clipboard")?;
    // Never a clipboard changed for a shortcut there is no time left to press.
    if !step.fits(PASTE_PAUSE) {
        return Err(OpError::Said(
            "Ran out of this call's time before putting the text on the desktop's clipboard."
                .to_owned(),
        ));
    }
    desktop.set_clipboard(text)?;
    if !step.pause(PASTE_PAUSE) {
        let stopped = if step.cancelled() {
            "Cancelled"
        } else {
            "Ran out of this call's time"
        };
        return Err(OpError::Said(format!(
            "{stopped} after putting the text on the desktop's clipboard, before pressing {shown}: nothing was pasted."
        )));
    }
    let pressed = if chord.modifiers.is_empty() {
        driver.key(&chord.key, None)
    } else {
        driver.chord(&chord)
    };
    match pressed {
        Ok(()) => Ok(shown),
        // The session reconnects on this one.
        Err(error @ DriverError::Disconnected(_)) => Err(OpError::Driver(error)),
        Err(error) => Err(OpError::Said(format!(
            "Put the text on the desktop's clipboard, but pressing {shown} failed: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;
    use crate::{
        builtin_schemas::builtin_tool_schema,
        path_guard::ExecutionScope,
        prompt_profile::PromptProfile,
        remote_control::fake::{FakeDesktop, FakeDriver},
        tool_surface::ToolVariant,
    };

    const TOOL: &str = "screen_clipboard";

    fn files() -> CallFiles {
        CallFiles::local(&std::env::temp_dir(), ExecutionScope::Unrestricted)
    }

    fn parse(tool: &str, input: Value) -> Result<DesktopCall, String> {
        DesktopCall::parse(tool, input.as_object().unwrap(), &files())
    }

    fn desktop(channel: ClipboardChannel) -> std::sync::Arc<FakeDriver> {
        FakeDriver::with_desktop(FakeDesktop::new(channel))
    }

    fn run(call: &DesktopCall, driver: &FakeDriver) -> Result<String, OpError> {
        let never = || false;
        call.run(driver, &Step::for_tests(&never, Duration::from_secs(20)))
    }

    fn said(result: Result<String, OpError>) -> String {
        match result {
            Err(OpError::Said(message)) => message,
            other => panic!("expected a refusal in the call's own words, got {other:?}"),
        }
    }

    /// A paste of `text` with `chord` (as the model writes it) on `driver`, in `within` of time.
    fn pasted(
        driver: &FakeDriver,
        text: &str,
        chord: Option<&str>,
        within: Duration,
    ) -> Result<String, OpError> {
        let never = || false;
        let chord = chord.map(|chord| (chord.trim().to_owned(), paste_chord(chord).unwrap()));
        paste(
            driver,
            driver.desktop().unwrap(),
            &Step::for_tests(&never, within),
            text,
            chord.as_ref(),
        )
    }

    /// Every parameter the schema states is taken with exactly its bounds: the edges, and
    /// refused one past them; each enum value, and refused another. `text` is the one bound
    /// that is narrower here than in the schema: a desktop's clipboard takes 60,000 characters
    /// where an iPhone's takes the schema's 100,000.
    #[test]
    fn parse_takes_exactly_what_the_schema_states() {
        let profile = PromptProfile::builtin();
        let schema = builtin_tool_schema(TOOL, ToolVariant::Standard, &profile).unwrap();
        let properties = schema["properties"].as_object().unwrap();
        let base = json!({"action": "write", "text": "x"});
        let with = |key: &str, value: Value| {
            let mut input = base.clone();
            input[key] = value;
            parse(TOOL, input)
        };
        for (key, property) in properties {
            match key.as_str() {
                "serverId" => assert!(with(key, json!("desktop")).is_ok()),
                "action" => {
                    for value in property["enum"].as_array().unwrap() {
                        let mut input = json!({"action": value});
                        if value == "write" {
                            input["text"] = json!("x");
                        }
                        assert!(parse(TOOL, input).is_ok(), "{value}");
                    }
                    assert!(with(key, json!("copy")).is_err());
                }
                "text" => {
                    let schema_longest = property["maxLength"].as_u64().unwrap() as usize;
                    assert_eq!(schema_longest, 100_000);
                    assert!(schema_longest > MAX_TEXT_CHARS);
                    assert!(property.get("minLength").is_none(), "an empty text clears");
                    assert!(with(key, json!("中".repeat(MAX_TEXT_CHARS))).is_ok());
                    let error = with(key, json!("中".repeat(MAX_TEXT_CHARS + 1)))
                        .err()
                        .unwrap();
                    assert_eq!(
                        error,
                        "\"text\" has 60001 characters; at most 60000 fit in one call."
                    );
                    assert!(with(key, json!("")).is_ok());
                    assert!(with(key, json!(7)).is_err());
                }
                other => panic!("{TOOL} states {other}, which parse does not know"),
            }
        }
        let error = with("verbose", json!(true)).err().unwrap();
        assert!(error.contains("Unknown parameter \"verbose\""), "{error}");
        assert!(error.contains("serverId"), "{error}");
    }

    /// The longest text a desktop takes fits what every VNC server takes, whatever its
    /// characters: none is more than 4 bytes as UTF-8 with CRLF line ends (an LF becomes 2), and
    /// the provide counts one NUL more.
    #[test]
    fn the_longest_text_fits_what_every_vnc_server_takes() {
        let worst = "😀".repeat(MAX_TEXT_CHARS);
        assert!(parse(TOOL, json!({"action": "write", "text": worst})).is_ok());
        let bytes = worst.len() + 1;
        assert_eq!(bytes, 4 * MAX_TEXT_CHARS + 1);
        assert!(
            bytes <= crate::remote_control::vnc::MAX_SENT_CLIPBOARD,
            "{MAX_TEXT_CHARS} characters can be {bytes} bytes"
        );
    }

    #[test]
    fn parse_says_what_is_missing_and_what_does_not_apply() {
        let error = |tool: &str, input: Value| parse(tool, input).err().unwrap();
        assert_eq!(
            error(TOOL, json!({})),
            "\"action\" is required: read or write."
        );
        assert!(
            error(TOOL, json!({"action": "write"})).starts_with("\"text\" is required for write")
        );
        assert!(error(TOOL, json!({"action": "read", "text": "x"}))
            .starts_with("\"text\" does not apply to read"));
        assert_eq!(
            error(TOOL, json!({"action": "read", "chord": "ctrl+v"})),
            "Unknown parameter \"chord\"; screen_clipboard takes serverId, action and text."
        );
        assert!(parse(TOOL, json!({"action": "read", "text": null})).is_ok());
        // The other platforms' tools are not this module's.
        for other in ["android_shell", "screen_app"] {
            assert!(
                error(other, json!({"action": "read"})).contains("not a desktop tool"),
                "{other}"
            );
        }
    }

    #[test]
    fn effects_and_what_each_call_does() {
        let read = parse(TOOL, json!({"action": "read"})).unwrap();
        let write = parse(TOOL, json!({"action": "write", "text": "中文"})).unwrap();
        assert_eq!(read.effect(), Effect::Look);
        // Under the input lock, as a paste is: never between a paste's text and its shortcut.
        assert_eq!(write.effect(), Effect::Act);
        assert_eq!(read.what(), "reading the desktop's clipboard");
        assert_eq!(
            write.what(),
            "putting 2 characters on the desktop's clipboard"
        );
        assert_eq!(read.time(), SCREEN_CALL_TIME);
    }

    #[test]
    fn a_write_says_how_the_text_went_and_latin1_refuses_before_sending() {
        let driver = desktop(ClipboardChannel::Extended);
        let write = |text: &str| parse(TOOL, json!({"action": "write", "text": text})).unwrap();
        assert_eq!(
            run(&write("中文 ok"), &driver).unwrap(),
            "Put 5 characters on the desktop's clipboard (UTF-8, Extended Clipboard)."
        );
        assert_eq!(
            run(&write(""), &driver).unwrap(),
            "Put 0 characters on the desktop's clipboard (UTF-8, Extended Clipboard)."
        );

        driver.desktop_fake().set_channel(ClipboardChannel::Latin1);
        assert_eq!(
            run(&write("café"), &driver).unwrap(),
            "Put 4 characters on the desktop's clipboard (Latin-1)."
        );
        assert_eq!(
            said(run(&write("ok 中"), &driver)),
            "This VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), so text with \"中\" cannot go on its clipboard."
        );
        assert!(said(run(&write("a\u{2028}b"), &driver)).contains("text with U+2028 cannot"));
        assert_eq!(driver.desktop_fake().writes(), ["中文 ok", "", "café"]);
        assert!(driver.inputs().is_empty());
    }

    #[test]
    fn a_read_shows_the_text_and_its_age_or_why_there_is_none() {
        let driver = desktop(ClipboardChannel::Extended);
        let fake = driver.desktop_fake();
        let read = parse(TOOL, json!({"action": "read"})).unwrap();
        fake.set_clipboard_text(Some("line 1\nline\t2\u{7}\r"), Duration::from_secs(3));
        assert_eq!(
            run(&read, &driver).unwrap(),
            "Clipboard (13 characters, received 3 s ago; UTF-8, Extended Clipboard):\nline 1\nline\t2"
        );
        fake.set_clipboard_text(Some("x"), Duration::ZERO);
        assert_eq!(
            run(&read, &driver).unwrap(),
            "Clipboard (1 character, received just now; UTF-8, Extended Clipboard):\nx"
        );
        fake.set_clipboard_text(Some("y"), Duration::from_secs(600));
        assert!(run(&read, &driver).unwrap().starts_with(
            "Clipboard (1 character, received 10 min ago; UTF-8, Extended Clipboard):"
        ));
        fake.set_clipboard_text(Some(&"é".repeat(40_000)), Duration::ZERO);
        let long = run(&read, &driver).unwrap();
        assert!(
            long.starts_with("Clipboard (40000 characters"),
            "{}",
            &long[..60]
        );
        assert!(long.ends_with("\n(cut at 64 KiB)"));
        assert!(long.len() < 64 * 1024 + 100);

        fake.set_clipboard_text(None, Duration::ZERO);
        assert_eq!(run(&read, &driver).unwrap(), "The clipboard is empty.");
        fake.set_clipboard_text(Some(""), Duration::ZERO);
        assert_eq!(run(&read, &driver).unwrap(), "The clipboard is empty.");

        // A server that did not answer is not an empty clipboard: the driver's reason is the
        // answer. Any other failure stays the driver's error.
        fake.fail_with(Some(DriverError::Unsupported(
            "this VNC server did not answer within 2 s when asked for its clipboard. Its clipboard may be empty (some servers then send nothing), or out of its reach".to_owned(),
        )));
        assert_eq!(
            run(&read, &driver).unwrap(),
            "This VNC server did not answer within 2 s when asked for its clipboard. Its clipboard may be empty (some servers then send nothing), or out of its reach."
        );
        fake.fail_with(Some(DriverError::Failed("the server said no".to_owned())));
        assert_eq!(
            run(&read, &driver).unwrap_err(),
            OpError::Driver(DriverError::Failed("the server said no".to_owned()))
        );
        fake.fail_with(None);

        fake.set_channel(ClipboardChannel::Latin1);
        fake.set_clipboard_text(Some("café"), Duration::ZERO);
        assert_eq!(
            run(&read, &driver).unwrap(),
            "Clipboard (4 characters, received just now; Latin-1):\ncafé"
        );
        fake.set_clipboard_text(None, Duration::ZERO);
        assert_eq!(
            run(&read, &driver).unwrap(),
            "This server has not sent its clipboard since the connection was made; it sends it when its clipboard changes, so copy the text there again and read once more."
        );
        // Each read waited the two seconds a server gets to answer.
        assert!(fake
            .reads()
            .iter()
            .all(|timeout| *timeout == Duration::from_secs(2)));

        // Never longer than the call has left.
        let never = || false;
        run(&read, &driver).unwrap();
        read.run(
            driver.as_ref(),
            &Step::for_tests(&never, Duration::from_millis(500)),
        )
        .unwrap();
        assert!(*fake.reads().last().unwrap() <= Duration::from_millis(500));
    }

    #[test]
    fn a_paste_sets_the_clipboard_then_presses_the_desktops_shortcut() {
        let driver = desktop(ClipboardChannel::Extended);
        let long = Duration::from_secs(20);
        let started = Instant::now();
        assert_eq!(
            pasted(&driver, "你好, world", None, long).unwrap(),
            "ctrl+v"
        );
        assert!(started.elapsed() >= PASTE_PAUSE);
        assert_eq!(driver.desktop_fake().writes(), ["你好, world"]);
        assert_eq!(
            driver.inputs(),
            [
                "key Named(Control) Some(true)",
                "key Char('v') None",
                "key Named(Control) Some(false)"
            ]
        );

        let mac = desktop(ClipboardChannel::Latin1);
        mac.desktop_fake().set_macos(true);
        assert_eq!(pasted(&mac, "ok", None, long).unwrap(), "cmd+v");
        assert_eq!(mac.inputs()[0], "key Named(Meta) Some(true)");

        let terminal = desktop(ClipboardChannel::Extended);
        pasted(&terminal, "ls", Some("Ctrl+Shift+V"), long).unwrap();
        assert_eq!(
            terminal.inputs(),
            [
                "key Named(Control) Some(true)",
                "key Named(Shift) Some(true)",
                "key Char('V') None",
                "key Named(Shift) Some(false)",
                "key Named(Control) Some(false)"
            ]
        );
        let insert = desktop(ClipboardChannel::Extended);
        assert_eq!(
            pasted(&insert, "ls", Some(" shift+Insert "), long).unwrap(),
            "shift+Insert"
        );
        let bare = desktop(ClipboardChannel::Extended);
        pasted(&bare, "ls", Some("Insert"), long).unwrap();
        assert_eq!(bare.inputs(), ["key Named(Insert) None"]);
    }

    #[test]
    fn a_paste_refuses_before_sending_and_says_how_far_it_got() {
        let latin1 = desktop(ClipboardChannel::Latin1);
        let long = Duration::from_secs(20);
        let refusal = said(pasted(&latin1, "naïve 😀", None, long));
        assert!(
            refusal.starts_with("This VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), so text with \"😀\" cannot go on its clipboard. Type it with via \"auto\""),
            "{refusal}"
        );
        assert!(latin1.desktop_fake().writes().is_empty());
        assert!(latin1.inputs().is_empty());

        // Stopped during the pause: the text went, the shortcut did not.
        let driver = desktop(ClipboardChannel::Extended);
        let stop = Instant::now() + Duration::from_millis(40);
        let cancelled = move || Instant::now() >= stop;
        let error = paste(
            driver.as_ref(),
            driver.desktop().unwrap(),
            &Step::for_tests(&cancelled, long),
            "abc",
            None,
        );
        assert_eq!(
            said(error),
            "Cancelled after putting the text on the desktop's clipboard, before pressing ctrl+v: nothing was pasted."
        );
        assert_eq!(driver.desktop_fake().writes(), ["abc"]);
        assert!(driver.inputs().is_empty());
        // Out of the call's time before it began: nothing at all.
        let error = pasted(&driver, "abc", None, Duration::ZERO);
        assert!(said(error).starts_with("Ran out of this call's time before putting the text"));
        // Or with less left than the pause before the shortcut needs.
        let error = pasted(&driver, "abc", None, Duration::from_millis(100));
        assert_eq!(
            said(error),
            "Ran out of this call's time before putting the text on the desktop's clipboard."
        );
        assert_eq!(driver.desktop_fake().writes().len(), 1);

        // A driver's error stays one, so the session can tell a lost connection.
        let gone = desktop(ClipboardChannel::Extended);
        gone.desktop_fake()
            .fail_with(Some(DriverError::Disconnected(
                "the VNC server closed the connection".into(),
            )));
        assert_eq!(
            pasted(&gone, "abc", None, long).unwrap_err(),
            OpError::Driver(DriverError::Disconnected(
                "the VNC server closed the connection".into()
            ))
        );
        assert!(gone.inputs().is_empty());
    }

    /// What a batch asks of a paste before its first action: the text goes on this server's
    /// clipboard, or the refusal names why — and the character.
    #[test]
    fn a_paste_is_refused_up_front_when_the_clipboard_cannot_take_the_text() {
        let extended = desktop(ClipboardChannel::Extended);
        let server = extended.desktop().unwrap();
        assert_eq!(paste_refusal(server, "中文 😀"), None);
        assert_eq!(paste_refusal(server, &"a".repeat(MAX_TEXT_CHARS)), None);
        assert_eq!(
            paste_refusal(server, &"a".repeat(MAX_TEXT_CHARS + 1)).unwrap(),
            "\"text\" has 60001 characters; at most 60000 fit on a desktop's clipboard."
        );

        let latin1 = desktop(ClipboardChannel::Latin1);
        let server = latin1.desktop().unwrap();
        assert_eq!(paste_refusal(server, "café"), None);
        let refusal = paste_refusal(server, "ok 中").unwrap();
        assert_eq!(
            refusal,
            "This VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), so text with \"中\" cannot go on its clipboard. Type it with via \"auto\" instead, as key presses, which not every server takes."
        );
        assert!(paste_refusal(server, "a\u{2028}b")
            .unwrap()
            .contains("text with U+2028 cannot"));
        assert!(latin1.desktop_fake().writes().is_empty());
        assert!(latin1.inputs().is_empty());
    }

    #[test]
    fn a_paste_shortcut_is_a_chord_a_desktop_has() {
        assert_eq!(
            paste_chord("ctrl+shift+v").unwrap().modifiers,
            vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Shift)]
        );
        assert!(paste_chord("mod+v").unwrap().uses_mod());
        assert!(paste_chord("  ")
            .unwrap_err()
            .starts_with("\"chord\" is empty"));
        assert!(paste_chord("hyper+v")
            .unwrap_err()
            .starts_with("\"chord\" is not a shortcut: Unknown key \"hyper\""));
        assert!(paste_chord("ctrl+Back")
            .unwrap_err()
            .contains("a phone button"));
    }

    #[test]
    fn a_screen_that_is_not_a_vnc_desktop_is_refused() {
        let call = parse(TOOL, json!({"action": "read"})).unwrap();
        assert_eq!(
            said(run(&call, &FakeDriver::touch())),
            "this screen is not a desktop connected over VNC"
        );
    }
}
