//! What a remote-control protocol has to provide, and nothing more.
//!
//! A driver speaks one protocol (RFB, the adb server's, WebDriverAgent's HTTP)
//! to one screen. Everything a protocol does not decide lives a layer up, in
//! [`super::session`]: turning a pointer drag from the viewer into a swipe,
//! mapping the model's coordinate frame back onto the screen, numbering the
//! nodes of a UI tree, throttling frame pulls, logging. Keeping that out of here
//! is what lets three very different protocols answer the same tools the same
//! way.
//!
//! Every coordinate a driver sees is a **screen pixel**: a pixel of the picture
//! [`Driver::frame`] returns. A protocol that addresses its screen in some other
//! unit (WebDriverAgent's points) converts inside the driver.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use super::keys::{Key, KeyChord};

/// What kind of screen is on the other end. It decides which gestures exist:
/// a desktop has a mouse pointer and a keyboard, a phone has touches and a few
/// hardware buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Platform {
    Android,
    Ios,
    Desktop,
}

impl Platform {
    /// Whether the screen is driven by touches rather than a pointer.
    pub fn is_touch(self) -> bool {
        matches!(self, Self::Android | Self::Ios)
    }
}

/// What a connected driver knows about its screen. Cheap to ask; a driver
/// keeps it current as the screen rotates or resizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverInfo {
    pub platform: Platform,
    /// The device or desktop as the protocol names it: an adb model
    /// (`Pixel 8`), the RFB desktop name, WebDriverAgent's device name.
    pub label: String,
    /// Screen size in screen pixels. `(0, 0)` until the first frame is known.
    pub width: u32,
    pub height: u32,
    /// Whether [`Driver::ui_tree`] can answer at all.
    pub has_ui_tree: bool,
}

/// One picture of the screen.
///
/// `rgba` is tightly packed, four bytes per pixel, alpha always 255. It is
/// shared, not copied, between whoever asked for it: the model's screenshot,
/// the viewer's encoder and the frame cache all hold the same buffer.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
    /// Counts up every time the picture changes, so a viewer can tell a new
    /// frame from the one it already sent without comparing pixels.
    pub seq: u64,
    pub captured_at: Instant,
}

impl fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Frame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// What a viewer in this process is given: pixels, ready for the GPU, with no encoder on the way.
#[derive(Clone)]
pub enum ScreenUpdate {
    /// The whole picture. Its size need not be the screen's: a stream may scale.
    Full { width: u32, height: u32, rgba: Arc<[u8]> },
    /// One rectangle of the picture last given whole.
    Patch { x: u32, y: u32, width: u32, height: u32, rgba: Vec<u8> },
}

impl ScreenUpdate {
    pub fn bytes(&self) -> usize {
        match self {
            Self::Full { rgba, .. } => rgba.len(),
            Self::Patch { rgba, .. } => rgba.len(),
        }
    }
}

impl fmt::Debug for ScreenUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full { width, height, .. } => write!(formatter, "Full({width}x{height})"),
            Self::Patch { x, y, width, height, .. } => write!(formatter, "Patch({x},{y} {width}x{height})"),
        }
    }
}

/// A rectangle in screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The point a tap on this element lands on, or `None` for an element with
    /// no area (which nothing can tap).
    pub fn center(&self) -> Option<(u32, u32)> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let x = i64::from(self.x) + i64::from(self.width / 2);
        let y = i64::from(self.y) + i64::from(self.height / 2);
        Some((u32::try_from(x).ok()?, u32::try_from(y).ok()?))
    }
}

/// One element of a device's own UI tree (Android's uiautomator dump,
/// WebDriverAgent's `/source`), already reduced to what a snapshot prints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiNode {
    /// Short class or type: `Button`, `TextView`, `XCUIElementTypeButton` with
    /// its prefix dropped.
    pub role: String,
    /// What a person would read: text, else content description / label.
    pub name: String,
    pub value: Option<String>,
    /// The developer's identifier (`resource-id`, accessibility identifier).
    pub id: Option<String>,
    pub bounds: Option<Rect>,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
    /// The app the element belongs to (Android's `package`); `None` where the
    /// platform does not say (iOS).
    pub package: Option<String>,
    /// Whether the element itself takes a tap (Android's `clickable`); always
    /// `false` on iOS, which has no such flag.
    pub clickable: bool,
    /// The element's own text, apart from `name`, which falls back to the
    /// content description when the text is empty. Android only: an iOS field
    /// keeps its content in `value`.
    pub text: Option<String>,
    /// What a field shows while it is empty (Android's `hint`, iOS's
    /// `placeholderValue`).
    pub hint: Option<String>,
    /// Whether the field hides what is typed into it (a password field).
    pub password: bool,
    /// Whether the parser's length cap shortened `name`, `value`, `text` or
    /// `hint`.
    pub truncated: bool,
    /// Whether the parser changed `name`, `value`, `text` or `hint` — trimmed it, replaced
    /// control characters with spaces, or cut it — so that it is not the device's own text.
    /// Such a text reads the field well enough for a person, but it cannot be matched
    /// against the device's own (a predicate on it can find another field whose text is the
    /// cleaned one). Every truncated node is inexact.
    pub inexact: bool,
    pub children: Vec<UiNode>,
}

/// What reading a device's element tree is expected to take, on the slow side of what devices do
/// (a UI dump takes one to three seconds, `/source` a few on a busy screen): a call with less
/// than this left does not start a read it could not finish ([`Driver::ui_tree_within`]).
pub const UI_TREE_READ: Duration = Duration::from_secs(3);

/// Kept back from the time a call has left when a tree read is bounded by it: the read is asked
/// to end this much before the call's own limit, so that the answer is still the caller's.
pub const UI_TREE_MARGIN: Duration = Duration::from_millis(250);

/// Whether `character` is a bidirectional control or an invisible format character (U+200B to
/// U+200F, U+202A to U+202E, U+2060 to U+2069, U+FEFF): one a person reads in another order than
/// a machine does, or does not see at all. A device's own text is shown to the model and to the
/// user without them, and a link that carries them is not one to open.
pub fn is_hidden_format(character: char) -> bool {
    matches!(
        character,
        '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}'
    )
}

/// The time a tree read may take when the call has `remaining` of its time: all of it but
/// [`UI_TREE_MARGIN`].
pub fn ui_tree_timeout(remaining: Duration) -> Duration {
    remaining.saturating_sub(UI_TREE_MARGIN)
}

/// Why a driver call did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    /// This protocol or platform has no such action (a right-click on a
    /// phone). The text says what to do instead.
    Unsupported(String),
    /// The connection is gone; the session decides whether to reconnect.
    Disconnected(String),
    /// The server asked for credentials, or refused the ones given.
    /// `needs_username` when the only way in takes an account name (Apple
    /// Remote Desktop authentication), not just a password.
    Credentials {
        retry: bool,
        needs_username: bool,
        message: String,
    },
    /// Anything else the other end answered with.
    Failed(String),
}

impl DriverError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(message)
            | Self::Disconnected(message)
            | Self::Failed(message)
            | Self::Credentials { message, .. } => message,
        }
    }
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for DriverError {}

pub type DriverResult<T> = Result<T, DriverError>;

/// What a driver tells its session without being asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverEvent {
    /// The picture changed ([`Frame::seq`] moved on). Pushing drivers (RFB)
    /// send it on every completed update; pulling drivers after each capture.
    Frame,
    /// The screen's size changed: a rotation, an RFB DesktopSize.
    Resized { width: u32, height: u32 },
    /// The connection ended on its own. The text says why, for the log and the
    /// viewer; the session decides whether to reconnect.
    Disconnected(String),
    /// One line for the session's log: what was negotiated, what was retried.
    /// Never a credential.
    Log(String),
}

/// Where a driver sends its [`DriverEvent`]s. Called from the driver's own
/// threads; must not block.
pub type DriverEvents = std::sync::Arc<dyn Fn(DriverEvent) + Send + Sync>;

/// Mouse buttons as RFB's button mask numbers them: bit 0 left, bit 1 middle,
/// bit 2 right, bits 3-6 the wheel (up, down, left, right).
pub mod buttons {
    pub const LEFT: u8 = 1;
    pub const MIDDLE: u8 = 1 << 1;
    pub const RIGHT: u8 = 1 << 2;
    pub const WHEEL_UP: u8 = 1 << 3;
    pub const WHEEL_DOWN: u8 = 1 << 4;
    pub const WHEEL_LEFT: u8 = 1 << 5;
    pub const WHEEL_RIGHT: u8 = 1 << 6;
}

/// One protocol talking to one screen. Implementations are blocking and must be
/// safe to call from several threads at once (the viewer's input and the
/// model's tool call can overlap); a driver serialises its own wire.
///
/// The defaults answer [`DriverError::Unsupported`], so a desktop driver leaves
/// the touch methods alone and a touch driver leaves [`Driver::pointer`] alone.
pub trait Driver: Send + Sync {
    fn info(&self) -> DriverInfo;

    /// The current picture. A driver that is pushed frames (RFB) returns the
    /// latest one it has; a driver that pulls (adb, WebDriverAgent) captures a
    /// new one when `fresh` is true and may return its last one otherwise.
    fn frame(&self, fresh: bool) -> DriverResult<Frame>;

    /// A fresh picture for a caller about to place input by it: the model's
    /// screenshot, the session measuring the screen after the model's input.
    /// A driver that learns more than the picture's size from a capture
    /// (WebDriverAgent's window, in points) learns that again here instead of
    /// keeping it while the picture keeps its size — an iPad app going into
    /// Split View changes the window, not the screenshot. The viewer's frame
    /// pump uses [`Self::frame`], which may skip that. The default is a fresh
    /// [`Self::frame`].
    fn remeasure(&self) -> DriverResult<Frame> {
        self.frame(true)
    }

    /// A pushing driver that tracks what changed: the change since the last call, or `None`
    /// when there is none or the driver does not know. It may answer [`ScreenUpdate::Full`] when
    /// a change has made patches meaningless (a resize).
    fn take_patch(&self) -> Option<ScreenUpdate> {
        None
    }

    /// The whole picture for a viewer that is just arriving, forgetting the change it already
    /// holds, so that the next [`Self::take_patch`] follows it exactly. `None`: use [`Self::frame`].
    fn take_full(&self) -> Option<ScreenUpdate> {
        None
    }

    /// A pulling driver that can do better than one capture at a time: runs a stream of
    /// pictures into `sink` until `stop` answers true or the stream ends. It blocks, and
    /// [`Ok`] means it is fine to start another at once (`stop`, or a limit of the stream's own).
    /// [`DriverError::Unsupported`]: this driver, or this device, has none.
    fn stream(&self, _stop: &(dyn Fn() -> bool + Sync), _sink: &mut dyn FnMut(ScreenUpdate)) -> DriverResult<()> {
        Err(DriverError::Unsupported("this screen has no picture stream".to_owned()))
    }

    /// Moves the pointer to `(x, y)` with exactly `buttons` held
    /// ([`buttons`]). Desktop drivers only: every click, drag and wheel notch
    /// is a sequence of these.
    fn pointer(&self, _x: u32, _y: u32, _buttons: u8) -> DriverResult<()> {
        Err(DriverError::Unsupported(
            "this screen is driven by touches, not a pointer".to_owned(),
        ))
    }

    /// Touch drivers: `count` taps in quick succession at `(x, y)`.
    fn tap(&self, _x: u32, _y: u32, _count: u8) -> DriverResult<()> {
        Err(DriverError::Unsupported(
            "this screen has a pointer, not touches".to_owned(),
        ))
    }

    /// Touch drivers: a touch held for `ms` milliseconds.
    fn long_press(&self, _x: u32, _y: u32, _ms: u32) -> DriverResult<()> {
        Err(DriverError::Unsupported(
            "this screen has a pointer, not touches".to_owned(),
        ))
    }

    /// Touch drivers: a touch moved from `from` to `to` over `ms` milliseconds.
    fn swipe(&self, _from: (u32, u32), _to: (u32, u32), _ms: u32) -> DriverResult<()> {
        Err(DriverError::Unsupported(
            "this screen has a pointer, not touches".to_owned(),
        ))
    }

    /// A key. `down` is `Some(true)` / `Some(false)` for one edge of a key a
    /// desktop holds (the viewer forwards key-down and key-up separately), and
    /// `None` for a whole press. Touch drivers act on `None` and `Some(true)`
    /// and ignore `Some(false)`.
    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()>;

    /// A key with modifiers held. The default holds each modifier down in
    /// order, presses the key, and lets the modifiers up in reverse — what a
    /// desktop driver wants. A touch driver, which cannot hold a key, overrides
    /// it (adb's `input keycombination`) or refuses it.
    fn chord(&self, chord: &KeyChord) -> DriverResult<()> {
        let mut held = Vec::with_capacity(chord.modifiers.len());
        let mut result = Ok(());
        for modifier in &chord.modifiers {
            result = self.key(modifier, Some(true));
            if result.is_err() {
                break;
            }
            held.push(*modifier);
        }
        if result.is_ok() {
            result = self.key(&chord.key, None);
        }
        // Whatever went down comes up, even when the press failed.
        for modifier in held.iter().rev() {
            let released = self.key(modifier, Some(false));
            if result.is_ok() {
                result = released;
            }
        }
        result
    }

    /// Text typed into whatever has focus.
    fn text(&self, text: &str) -> DriverResult<()>;

    /// Whether [`Driver::text`] could type all of `text`, answered without
    /// typing anything. A session types long text in pieces, so that a stopped
    /// or timed-out call stops between them; it asks this first, so a character
    /// the driver refuses stops the text before its first piece rather than
    /// halfway through. The default accepts everything.
    fn check_text(&self, _text: &str) -> DriverResult<()> {
        Ok(())
    }

    /// The screen's UI tree, or `None` when this protocol has none (RFB).
    fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
        Ok(None)
    }

    /// [`Self::ui_tree`], bounded by `timeout`: a read that has not finished by then answers
    /// `Failed` instead of running on for the protocol's own (much longer) limit, past the end
    /// of the tool call that asked. A caller takes `timeout` from what is left of its call
    /// ([`ui_tree_timeout`]). The default is [`Self::ui_tree`], for a driver whose tree is not
    /// read over a wire.
    fn ui_tree_within(&self, _timeout: Duration) -> DriverResult<Option<UiNode>> {
        self.ui_tree()
    }

    /// Releases the connection. Further calls answer `Disconnected`.
    fn close(&self);

    /// The adb operations behind the Android tools; `None` on any other protocol.
    fn android(&self) -> Option<&dyn AndroidControl> {
        None
    }

    /// The WebDriverAgent calls behind the iOS tools; `None` on any other protocol.
    fn ios(&self) -> Option<&dyn IosControl> {
        None
    }

    /// The RFB extras behind the desktop tools; `None` on any other protocol.
    fn desktop(&self) -> Option<&dyn DesktopControl> {
        None
    }
}

// What follows is what one protocol can do beyond showing a screen and taking
// input: the per-platform tools (`android_*`, `ios_*`, `desktop_*`) are built
// on it. Each is reached from [`Driver`] by an accessor that only that
// protocol's driver answers, so a tool on the wrong screen is told so instead
// of being sent something the other end does not speak. Every call is bounded:
// it takes the time it may use, which the session takes from what is left of
// the tool call (`session::platform::Step`). A call that runs out of that time
// answers [`DriverError::Failed`]; only a device or server that is really gone
// answers [`DriverError::Disconnected`], which starts the session's reconnects.

/// One shell command's result, as data: a non-zero exit is not an error at this level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellResult {
    pub stdout: Vec<u8>,
    /// Empty when the device has no framed shell (stderr then arrives mixed into stdout).
    pub stderr: Vec<u8>,
    /// `None` without a framed shell (no exit status is reported) or when the stream ended early.
    pub exit: Option<u8>,
    /// Output was cut at the requested limit.
    pub truncated: bool,
    /// The command had not ended when its output stopped being read — its timeout passed, or it
    /// printed far more than is ever read — and what it printed until then is all there is;
    /// `exit` is then `None`.
    pub timed_out: bool,
}

/// The adb operations behind the Android tools.
pub trait AndroidControl: Send + Sync {
    /// `ro.build.version.sdk`, cached.
    fn sdk(&self) -> DriverResult<u32>;

    /// Runs `command` in the device shell (adbd runs it with `sh -c` as the shell user),
    /// keeping at most `limit` bytes of each stream, within `timeout`. Every value interpolated
    /// into `command` must already be quoted (`adb::input::shell_quote`).
    fn shell(&self, command: &str, limit: usize, timeout: Duration) -> DriverResult<ShellResult>;

    /// Streams an APK of exactly `size` bytes from `apk` into `cmd package install <flags> -S <size>`
    /// over the `exec:` service, checking `cancelled` between chunks (a cancelled install closes
    /// the stream, which makes pm fail it). `flags` are fixed option words ("-r", "-t", "-d", "-g").
    /// Returns pm's final status line ("Success" or "Failure [INSTALL_FAILED_...]").
    fn install(
        &self,
        apk: &mut dyn std::io::Read,
        size: u64,
        flags: &[&str],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> DriverResult<String>;
}

/// Where a WebDriverAgent call goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WdaRoute<'a> {
    /// `/session/<the driver's session id><suffix>`; `suffix` starts with `/`.
    Session(&'a str),
    /// A route outside any session; starts with `/`.
    Root(&'a str),
}

/// The WebDriverAgent calls behind the iOS tools.
pub trait IosControl: Send + Sync {
    /// One WebDriverAgent call (`GET`, `POST` or `DELETE`), returning the reply's `value`.
    /// A route this WebDriverAgent build lacks answers `Unsupported`; WebDriverAgent's own error
    /// answers `Failed` with its message; a timeout answers `Failed`. The session-replacement and
    /// session-id rules of the driver apply (never create a session while one exists).
    fn call(
        &self,
        method: &str,
        route: WdaRoute<'_>,
        body: Option<&serde_json::Value>,
        timeout: Duration,
    ) -> DriverResult<serde_json::Value>;
}

/// The clipboard channel a VNC server offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipboardChannel {
    /// The Extended Clipboard pseudo-encoding (0xC0A1E5CE): UTF-8 text, and requests.
    Extended,
    /// Classic ClientCutText/ServerCutText: Latin-1 only, and only what the server sends unasked.
    Latin1,
}

/// Clipboard text a VNC server sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardText {
    pub text: String,
    pub received: Instant,
}

/// The RFB extras behind the desktop tools.
pub trait DesktopControl: Send + Sync {
    fn clipboard_channel(&self) -> ClipboardChannel;

    /// Puts `text` on the server's clipboard. On a Latin-1 channel, text with any other character
    /// is refused (`Unsupported`) before anything is sent.
    fn set_clipboard(&self, text: &str) -> DriverResult<()>;

    /// The server's clipboard text: on the Extended channel asked for and awaited up to `timeout`;
    /// on Latin-1, the last text the server sent unasked this connection, if any. `None` only when
    /// the server answered that its clipboard is empty; a server that did not answer and sent no
    /// text before is `Unsupported`, whose message says why.
    fn clipboard(&self, timeout: Duration) -> DriverResult<Option<ClipboardText>>;

    /// Whether this is Apple's server (macOS Screen Sharing): security type 30 offered or
    /// version `RFB 003.889`. Decides the default paste chord.
    fn is_macos(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rect_taps_its_middle_and_an_empty_one_cannot_be_tapped() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 41,
        };
        assert_eq!(rect.center(), Some((60, 40)));
        assert_eq!(
            Rect {
                x: 10,
                y: 20,
                width: 0,
                height: 5
            }
            .center(),
            None
        );
        assert_eq!(
            Rect {
                x: -100,
                y: 0,
                width: 10,
                height: 10
            }
            .center(),
            None
        );
    }
}
