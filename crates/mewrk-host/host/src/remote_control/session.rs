//! One connected remote screen, from `preview_start` to `preview_stop`.
//!
//! A session owns everything a protocol does not decide (`docs/remote-control.md`
//! §3.2): the connection's state and its reconnects, signing in through the
//! viewer page, the frames the page streams, the coordinate frame the model
//! works in, the gestures `preview_input` names, the uids of a snapshot, and
//! the one input lock the model and the person at the pane share. A
//! [`Driver`] underneath only speaks its protocol.
//!
//! A session has two threads of its own. The **supervisor** connects, asks
//! for credentials, waits for the connection to end and reconnects with
//! backoff; nothing else ever calls the driver factory. The **pump** turns the
//! screen into the viewer's JPEGs, and only while a visible page is streaming:
//! a pulling driver (adb, WebDriverAgent) is asked for a picture about twice a
//! second, a pushing one (RFB) is encoded when it says the picture changed, at
//! most ten times a second, once for every page. Tool calls and the viewer's
//! input run on their callers' threads and take the driver as it is once they
//! hold the input lock; the model's input and the person's never interleave,
//! because both hold that one lock, and the model lets go of whatever the
//! person's pages held down before its batch starts.
//!
//! A tool call runs inside a [`CallContext`]: a deadline its answer must beat
//! and a flag for a stopped turn. Every wait (a reconnect, the input lock, a
//! settling screen) is bounded by it, nothing is started that cannot end before
//! it, and a batch that runs out of time says which actions ran.
//!
//! The model never sees the viewer's address, a credential or anything typed
//! into the sign-in form; neither does the log.

use std::{
    borrow::Cow,
    cell::Cell,
    collections::HashMap,
    fmt::{self, Write as _},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex, MutexGuard, TryLockError, Weak,
    },
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use super::{
    credentials::{self, CredentialStore, KeyringStore},
    desktop::{self, PasteChord},
    driver::{
        buttons, is_hidden_format, ui_tree_timeout, AndroidControl, Driver, DriverError,
        DriverEvent, DriverEvents, DriverInfo, DriverResult, Platform, Rect, ScreenUpdate, UiNode,
        UI_TREE_READ,
    },
    frame::{self, FrameHub, LiveScreen, Work},
    ios,
    keys::{self, Key, KeyChord, NamedKey},
    snapshot::{self, UidTarget},
    target::{self, ControlTarget, Protocol},
    transport::Connector,
    view::{
        CredentialRequest, PointerPhase, ScreenSize, SessionPhase, ViewerBackend,
        ViewerCredentials, ViewerEvent, ViewerFrame, ViewerState,
    },
    viewer::Viewer,
    vnc::VncAuth,
};
use crate::preview_launch_config::sanitize_for_message;

/// What the per-platform tools run through: effects and locks, a call's time,
/// `preview_zoom`'s close-up and `preview_wait`'s watch.
mod platform;

pub use platform::{Effect, FrameRegion, OpError, Step, WaitUntil, ZoomShot};

/// Waits between reconnect attempts; their count is the number of attempts.
const RECONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];
/// How long a tool call waits for a session that is not ready yet.
const READY_WAIT: Duration = Duration::from_secs(10);
/// How long a model input waits for another batch (a subagent's, the person's
/// swipe) to let go of the input lock. Inside the tool call's 30 s envelope.
const INPUT_LOCK_WAIT: Duration = Duration::from_secs(25);
/// A phone animates after a touch; a screenshot taken sooner shows the screen
/// halfway there.
const TOUCH_SETTLE: Duration = Duration::from_millis(300);
/// Between the clicks of a double or triple click: well inside every desktop's
/// double-click interval, long enough for a server to see two clicks.
const CLICK_GAP: Duration = Duration::from_millis(80);
/// Between a pulling driver's captures for the viewer: about two frames a
/// second, since a capture itself takes a few hundred milliseconds.
const PULL_PAUSE: Duration = Duration::from_millis(300);
/// The viewer's stream runs at most ten frames a second.
const ENCODE_INTERVAL: Duration = Duration::from_millis(100);
/// After the tap that focuses a field and before typing into it.
const FILL_FOCUS: Duration = Duration::from_millis(300);
/// The Android version from which `input keycombination` sends its keys with the Ctrl state
/// that makes `CTRL_LEFT A` select all: Android 12 injects them without it.
const REPLACE_MIN_SDK: u32 = 33;
/// What replacing an Android field's text is expected to take, step by step, on the slow
/// side of what devices do (a UI dump takes one to three seconds): the call must have it left
/// before the first deletion, so a field is never emptied and then left unfilled.
const REPLACE_TAP: Duration = Duration::from_millis(600);
/// Reading the element tree once more (the expected time of [`UI_TREE_READ`]).
const REPLACE_READ: Duration = UI_TREE_READ;
const REPLACE_KEYS: Duration = Duration::from_secs(1);
/// What clearing an iPhone or iPad field takes the same way: WebDriverAgent finds it by its text
/// (a walk of the accessibility tree, seconds on a busy screen) and types its backspaces.
const REPLACE_IOS_CLEAR: Duration = Duration::from_secs(5);
/// Typing into an Android field (`input text`), per piece of [`TEXT_PIECE_CHARS`] characters.
const REPLACE_TYPE_PIECE: Duration = Duration::from_millis(600);
/// How fast WebDriverAgent types into an iPhone or iPad field: about this many characters a
/// second (`wda.rs` sizes a typing call's timeout by it), far slower than `input text`.
const REPLACE_IOS_CHARS_PER_SECOND: u64 = 60;
/// Characters of a field's old text that an answer quotes.
const FIELD_TEXT_SHOWN: usize = 60;
/// How often the pump re-checks for work when nothing woke it.
const PUMP_IDLE: Duration = Duration::from_secs(1);
/// The pause between captures while a viewer in this process watches. A capture over adb or from
/// WebDriverAgent takes hundreds of milliseconds by itself; this only leaves the device room to
/// breathe between them.
const LIVE_PULL_PAUSE: Duration = Duration::from_millis(10);
/// Between one stream ending and the next, so one that dies at once does not spin.
const STREAM_RETRY: Duration = Duration::from_millis(100);
/// A stream that lasted this long has shown it works, whatever ended it.
const STREAM_PROVEN: Duration = Duration::from_secs(5);
/// Streams that may fail in a row before capture takes over for this connection.
const STREAM_ATTEMPTS: u32 = 3;
/// An entry with `launch` may be an emulator still booting.
const ADB_BOOT_WAIT: Duration = Duration::from_secs(180);
/// How much of a tool call's time is kept back from its work: the answer still
/// has to be put together and handed over before the caller stops waiting.
pub const CALL_MARGIN: Duration = Duration::from_secs(2);
/// The time the methods without a [`CallContext`] give themselves. The tool
/// layer calls the `*_within` methods; these stay for tests, which have no
/// deadline of their own.
#[cfg(test)]
const LEGACY_CALL: Duration = Duration::from_secs(25);
/// How often a wait that a tool call can cancel looks at its flag.
const WAIT_SLICE: Duration = Duration::from_millis(50);
/// How long one page's input batch waits for another page's to finish.
/// Batches are short; a page whose batch waits longer is dropped like one the
/// model's batch overlapped.
const VIEWER_LOCK_WAIT: Duration = Duration::from_secs(5);
/// How long a page that went away waits to let go of what it held. Only a
/// desktop has keys and buttons held, and its batches are fast; the wait is on
/// the ending stream's own thread.
const RELEASE_LOCK_WAIT: Duration = Duration::from_secs(30);
/// Typing longer than this goes in pieces of this many characters, so a
/// cancelled or timed-out call stops between them instead of typing on.
const TEXT_PIECE_CHARS: usize = 100;
/// The longest a page's own clock may say a touch was held.
const MAX_PAGE_HOLD: Duration = Duration::from_secs(60);
/// Characters of the reason a row shows ([`RemoteSummary::message`]).
const MAX_SUMMARY_MESSAGE_CHARS: usize = 300;

/// `preview_input` limits (`docs/remote-control.md` §4.1).
pub const MAX_ACTIONS: usize = 20;
pub const MAX_BATCH_MS: u64 = 20_000;
pub const MAX_WAIT_MS: i64 = 5_000;
const MAX_CLICKS: i64 = 3;
const MAX_REPEAT: i64 = 50;
const MAX_NOTCHES: i64 = 50;
/// Characters of one `type` action with `via` "auto", and of `preview_fill`'s value on a
/// screen. The `preview_input` root description states the same bound (`builtin_schemas.rs`);
/// its flat schema bounds `text` by the longest any `via` takes
/// ([`desktop::MAX_TEXT_CHARS`], which `via` "paste" is held to).
pub const MAX_TEXT_CHARS: usize = 2_000;
/// Characters of a `chord`. The `preview_input` schema states the same bound
/// (`builtin_schemas.rs`).
const MAX_CHORD_CHARS: usize = 64;
const MAX_COORDINATE: f64 = 100_000.0;
const DEFAULT_LONG_PRESS_MS: i64 = 800;
const DEFAULT_DRAG_MS: i64 = 300;
/// A touch screen scrolls by swiping; one swipe per direction takes this long,
/// and counts toward the batch's time budget.
const SCROLL_SWIPE_MS: u32 = 300;
/// Pointer moves a desktop drag is cut into, so the server sees motion rather
/// than a jump (some only start a drag once the pointer moves while pressed).
const DRAG_STEPS: u32 = 10;
const ACTION_NAMES: &str = "click, long_press, drag, scroll, move, type, key, wait";
/// Characters of a `modifiers` field. The `preview_input` schema states the
/// same bound (`builtin_schemas.rs`).
const MAX_MODIFIERS_CHARS: usize = 64;
/// Why a phone is never asked to hold a modifier.
const MODIFIERS_NEED_A_DESKTOP: &str =
    "modifiers need a desktop: a phone has no keyboard to hold them on.";

/// A touch from the viewer that moved less than this share of the picture on
/// either axis is a tap or a long press, not a swipe.
const TOUCH_SLOP: f64 = 0.02;
const TOUCH_LONG_PRESS: Duration = Duration::from_millis(500);
const TOUCH_LONG_PRESS_MAX_MS: u32 = 5_000;
const TOUCH_SWIPE_MIN_MS: u32 = 100;
const TOUCH_SWIPE_MAX_MS: u32 = 2_000;
/// Wheel notches one viewer event may carry.
const MAX_WHEEL_NOTCHES: f64 = 20.0;
/// Pages whose input state a session tracks, and keys one page may hold down.
const MAX_PAGES: usize = 16;
const MAX_HELD_KEYS: usize = 32;
const MAX_PASSWORD_BYTES: usize = 1024;
/// Characters of one log line.
const MAX_LOG_CHARS: usize = 500;
/// Characters of a device label, a place or an entry name in a message.
const MAX_LABEL_CHARS: usize = 80;
/// The same failed viewer input is logged at most this often.
const VIEWER_ERROR_QUIET: Duration = Duration::from_secs(10);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connects a driver: the target, where it is, the sign-in to offer (VNC
/// only), and where the driver sends its events. The connector answers
/// [`Connector::cancelled`] once the session closed, so a factory still waiting
/// (an emulator booting) gives up.
pub type ConnectFn = Arc<
    dyn Fn(
            &ControlTarget,
            Arc<dyn Connector>,
            Option<VncAuth>,
            DriverEvents,
        ) -> DriverResult<Arc<dyn Driver>>
        + Send
        + Sync,
>;

/// Runs before every connect; an error ends the session with that message.
pub type Preflight = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// What a session is started with.
#[derive(Clone)]
pub struct SessionSpec {
    /// The entry's name.
    pub name: String,
    pub server_id: String,
    pub target: ControlTarget,
    pub connector: Arc<dyn Connector>,
    /// Runs before every connect, the first and each reconnect (the ownership
    /// rule of §2.4). `Err` ends the session as failed with that message.
    pub preflight: Option<Preflight>,
    /// Called (from any thread, must not block) whenever [`RemoteControlSession::summary`]
    /// would change.
    pub on_change: Arc<dyn Fn() + Send + Sync>,
    /// One log line for the row's log ring. Never credentials, tokens or the
    /// viewer's address.
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    /// The driver factory; [`default_connect`] when `None`. Tests plug a fake.
    pub connect: Option<ConnectFn>,
}

/// The real drivers, by protocol.
pub fn default_connect() -> ConnectFn {
    Arc::new(|target, connector, auth, events| match target.protocol {
        Protocol::Vnc => {
            super::vnc::connect(connector, target.port, auth.unwrap_or_default(), events)
        }
        Protocol::Adb => super::adb::connect(
            connector,
            target.port,
            target.device.clone(),
            target.launch.as_ref().map(|_| ADB_BOOT_WAIT),
            events,
        ),
        Protocol::Wda => super::wda::connect(connector, target.port, events),
    })
}

/// The picture `preview_screenshot` returns: a JPEG of exactly the coordinate
/// frame, and the screen it was taken from.
#[derive(Clone)]
pub struct ModelScreenshot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub screen_width: u32,
    pub screen_height: u32,
}

impl fmt::Debug for ModelScreenshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelScreenshot")
            .field("bytes", &self.jpeg.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("screen_width", &self.screen_width)
            .field("screen_height", &self.screen_height)
            .finish()
    }
}

/// What a tool call allows a session: when its answer must be back, and
/// whether its turn or task was stopped. The `*_within` methods finish before
/// `deadline` — less [`CALL_MARGIN`] for the answer's own way back — or stop
/// before starting what could not, and say how far they got; `cancelled` is
/// looked at between actions and during every wait.
pub struct CallContext<'a> {
    pub deadline: Instant,
    pub cancelled: &'a dyn Fn() -> bool,
}

impl CallContext<'_> {
    fn budget(&self) -> Budget<'_> {
        Budget {
            cancelled: self.cancelled,
            limit: self
                .deadline
                .checked_sub(CALL_MARGIN)
                .unwrap_or(self.deadline),
        }
    }
}

/// The context the methods without one run in: [`LEGACY_CALL`] from now.
#[cfg(test)]
fn legacy_call(cancelled: &dyn Fn() -> bool) -> CallContext<'_> {
    CallContext {
        deadline: Instant::now() + LEGACY_CALL,
        cancelled,
    }
}

/// Why work stopped before it was done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Halt {
    Cancelled,
    OutOfTime,
}

impl Halt {
    fn word(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::OutOfTime => "out of time",
        }
    }
}

/// A [`CallContext`] as work checks it: stop when cancelled, or at `limit`.
#[derive(Clone, Copy)]
struct Budget<'a> {
    cancelled: &'a dyn Fn() -> bool,
    limit: Instant,
}

impl Budget<'_> {
    /// No limit worth the name, for the viewer's own gestures.
    fn open() -> Budget<'static> {
        Budget {
            cancelled: &|| false,
            limit: Instant::now() + Duration::from_secs(24 * 60 * 60),
        }
    }

    fn halt(&self) -> Option<Halt> {
        if (self.cancelled)() {
            Some(Halt::Cancelled)
        } else if Instant::now() >= self.limit {
            Some(Halt::OutOfTime)
        } else {
            None
        }
    }

    /// Whether something taking `needed` would end before the limit.
    fn fits(&self, needed: Duration) -> bool {
        Instant::now() + needed < self.limit
    }

    fn remaining(&self) -> Duration {
        self.limit.saturating_duration_since(Instant::now())
    }

    /// `Ok` unless the call was cancelled or is out of time, in which case
    /// `what` did not start.
    fn proceed(&self, what: &str) -> Result<(), String> {
        match self.halt() {
            None => Ok(()),
            Some(Halt::Cancelled) => Err(format!("Cancelled before {what}.")),
            Some(Halt::OutOfTime) => Err(format!(
                "Ran out of time before {what}: waiting for the screen used up this call's time. Try again."
            )),
        }
    }
}

/// What a `preview_input` batch did: one line per action that ran, the last
/// one saying why the batch stopped when it did not finish.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputReport {
    pub lines: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn mask(self) -> u8 {
        match self {
            Self::Left => buttons::LEFT,
            Self::Right => buttons::RIGHT,
            Self::Middle => buttons::MIDDLE,
        }
    }
}

/// One `preview_input` action. Coordinates are pixels of the coordinate frame.
/// `modifiers` are the keys held down for the whole of a pointer action, in
/// the order they go down (a desktop's shift-click, ctrl-drag, ctrl-wheel);
/// none, mostly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputAction {
    Click {
        x: u32,
        y: u32,
        button: MouseButton,
        count: u8,
        modifiers: Vec<Key>,
    },
    LongPress {
        x: u32,
        y: u32,
        ms: u32,
        modifiers: Vec<Key>,
    },
    Drag {
        x: u32,
        y: u32,
        to_x: u32,
        to_y: u32,
        ms: u32,
        modifiers: Vec<Key>,
    },
    /// Wheel notches; positive shows more content below / to the right.
    Scroll {
        x: u32,
        y: u32,
        dx: i32,
        dy: i32,
        modifiers: Vec<Key>,
    },
    Move {
        x: u32,
        y: u32,
    },
    Type {
        text: String,
        via: TypeVia,
    },
    Key {
        key: String,
        repeat: u32,
    },
    Wait {
        ms: u32,
    },
}

/// How a `type` action puts its text in (`type`'s `via`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeVia {
    /// `"auto"`: the screen's own way of typing — key presses on a desktop, `input text` on
    /// Android, WebDriverAgent's keys on an iPhone. Never touches the clipboard.
    Auto,
    /// `"paste"` (desktops only): the text goes on the desktop's clipboard and the shortcut
    /// pastes it in — `chord` as the model wrote it, the desktop's default when `None`. The
    /// clipboard keeps the text.
    Paste { chord: Option<PasteChord> },
}

impl InputAction {
    /// What the action costs of the batch's time budget.
    fn budget_ms(&self) -> u64 {
        match self {
            Self::LongPress { ms, .. } | Self::Drag { ms, .. } | Self::Wait { ms } => {
                u64::from(*ms)
            }
            Self::Scroll { dx, dy, .. } => {
                u64::from(SCROLL_SWIPE_MS) * (u64::from(*dx != 0) + u64::from(*dy != 0))
            }
            _ => 0,
        }
    }

    /// What the action needs to end inside the call's time, which is more than its share of
    /// the batch's budget ([`Self::budget_ms`]) where it pauses on its own: a paste waits for
    /// the server to take the clipboard before it presses its shortcut.
    fn needs_ms(&self) -> u64 {
        let pause = match self {
            Self::Type {
                via: TypeVia::Paste { .. },
                ..
            } => desktop::PASTE_PAUSE,
            _ => Duration::ZERO,
        };
        self.budget_ms() + u64::try_from(pause.as_millis()).unwrap_or(0)
    }

    /// Whether the action names `mod`, the screen system's shortcut key, anywhere: a held
    /// modifier, a key's chord, a paste's shortcut.
    fn uses_mod(&self) -> bool {
        let mod_key = Key::Named(NamedKey::Mod);
        match self {
            Self::Key { key, .. } => {
                keys::parse_chord(key, Platform::Desktop).is_ok_and(|chord| chord.uses_mod())
            }
            Self::Type {
                via: TypeVia::Paste {
                    chord: Some((_, chord)),
                },
                ..
            } => chord.uses_mod(),
            _ => self.modifiers().contains(&mod_key),
        }
    }

    /// The action with `mod` written as `primary` (`ctrl` or `cmd`) everywhere it appears.
    fn with_mod_as(&self, primary: NamedKey) -> Self {
        let mut action = self.clone();
        match &mut action {
            Self::Click { modifiers, .. }
            | Self::LongPress { modifiers, .. }
            | Self::Drag { modifiers, .. }
            | Self::Scroll { modifiers, .. } => *modifiers = keys::mod_as(modifiers, primary),
            Self::Key { key, .. } => *key = keys::replace_mod(key, primary),
            Self::Type {
                via:
                    TypeVia::Paste {
                        chord: Some((shown, chord)),
                    },
                ..
            } => {
                *shown = keys::replace_mod(shown, primary);
                *chord = chord.with_mod_as(primary);
            }
            Self::Move { .. } | Self::Type { .. } | Self::Wait { .. } => {}
        }
        action
    }

    fn point(&self) -> Option<(u32, u32)> {
        match *self {
            Self::Click { x, y, .. }
            | Self::LongPress { x, y, .. }
            | Self::Drag { x, y, .. }
            | Self::Scroll { x, y, .. }
            | Self::Move { x, y } => Some((x, y)),
            Self::Type { .. } | Self::Key { .. } | Self::Wait { .. } => None,
        }
    }

    /// The keys held down during the action; empty for most.
    fn modifiers(&self) -> &[Key] {
        match self {
            Self::Click { modifiers, .. }
            | Self::LongPress { modifiers, .. }
            | Self::Drag { modifiers, .. }
            | Self::Scroll { modifiers, .. } => modifiers,
            Self::Move { .. } | Self::Type { .. } | Self::Key { .. } | Self::Wait { .. } => &[],
        }
    }

    /// The action's name as the model writes it.
    fn name(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::LongPress { .. } => "long_press",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::Move { .. } => "move",
            Self::Type { .. } => "type",
            Self::Key { .. } => "key",
            Self::Wait { .. } => "wait",
        }
    }

    /// The action as a report line names it: `click (288, 640)`, and with
    /// modifiers `click (288, 640) holding ctrl+shift`.
    fn describe(&self) -> String {
        let line = self.describe_action();
        let held: Vec<&str> = self
            .modifiers()
            .iter()
            .filter_map(|key| keys::modifier_name(*key))
            .collect();
        if held.is_empty() {
            line
        } else {
            format!("{line} holding {}", held.join("+"))
        }
    }

    fn describe_action(&self) -> String {
        match self {
            Self::Click {
                x,
                y,
                button,
                count,
                ..
            } => {
                let times = match count {
                    2 => "double ",
                    3 => "triple ",
                    _ => "",
                };
                let which = match button {
                    MouseButton::Left => "",
                    MouseButton::Right => "right ",
                    MouseButton::Middle => "middle ",
                };
                format!("{times}{which}click ({x}, {y})")
            }
            Self::LongPress { x, y, ms, .. } => format!("long_press ({x}, {y}) for {ms} ms"),
            Self::Drag {
                x,
                y,
                to_x,
                to_y,
                ms,
                ..
            } => format!("drag ({x}, {y}) to ({to_x}, {to_y}) over {ms} ms"),
            Self::Scroll { x, y, dx, dy, .. } => {
                let mut line = format!("scroll ({x}, {y})");
                if *dx != 0 {
                    let _ = write!(line, " dx {dx}");
                }
                if *dy != 0 {
                    let _ = write!(line, " dy {dy}");
                }
                line
            }
            Self::Move { x, y } => format!("move ({x}, {y})"),
            Self::Type { text, via } => {
                let shown: String = text.chars().take(40).collect();
                let how = match via {
                    TypeVia::Auto => "",
                    TypeVia::Paste { .. } => " via paste",
                };
                if shown.len() < text.len() {
                    format!(
                        "type {shown:?}... ({} characters){how}",
                        text.chars().count()
                    )
                } else {
                    format!("type {shown:?}{how}")
                }
            }
            Self::Key { key, repeat } if *repeat > 1 => {
                format!("key {} x{repeat}", one_line(key, 60))
            }
            Self::Key { key, .. } => format!("key {}", one_line(key, 60)),
            Self::Wait { ms } => format!("wait {ms} ms"),
        }
    }
}

/// Reads `preview_input`'s `actions` array, with errors that say what to fix.
pub fn parse_actions(value: &Value) -> Result<Vec<InputAction>, String> {
    let Some(items) = value.as_array() else {
        return Err(
            "\"actions\" must be an array of actions, e.g. [{\"action\": \"click\", \"x\": 288, \"y\": 640}]."
                .to_owned(),
        );
    };
    if items.is_empty() {
        return Err("\"actions\" is empty; give at least one action.".to_owned());
    }
    if items.len() > MAX_ACTIONS {
        return Err(format!(
            "\"actions\" has {} actions; at most {MAX_ACTIONS} fit in one preview_input call. Split the batch.",
            items.len()
        ));
    }
    let actions = items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_action(index + 1, item))
        .collect::<Result<Vec<_>, _>>()?;
    let total: u64 = actions.iter().map(InputAction::budget_ms).sum();
    if total > MAX_BATCH_MS {
        return Err(format!(
            "The batch would take about {total} ms (waits, long_press and drag durations, and {SCROLL_SWIPE_MS} ms per scroll direction); the limit is {MAX_BATCH_MS} ms per preview_input call. Split it into several calls."
        ));
    }
    Ok(actions)
}

fn parse_action(number: usize, item: &Value) -> Result<InputAction, String> {
    let Some(object) = item.as_object() else {
        return Err(format!(
            "Action {number} must be an object such as {{\"action\": \"click\", \"x\": 288, \"y\": 640}}."
        ));
    };
    let name = match object.get("action") {
        Some(Value::String(name)) => name.as_str(),
        Some(_) => {
            return Err(format!(
                "Action {number}: \"action\" must be a string, one of {ACTION_NAMES}."
            ))
        }
        None => {
            return Err(format!(
                "Action {number} has no \"action\"; give one of {ACTION_NAMES}."
            ))
        }
    };
    let fields = Fields {
        number,
        name,
        object,
    };
    Ok(match name {
        "click" => {
            fields.only(&["x", "y", "button", "count", "modifiers"])?;
            let button = match object.get("button") {
                None | Some(Value::Null) => MouseButton::Left,
                Some(Value::String(button)) if button == "left" => MouseButton::Left,
                Some(Value::String(button)) if button == "right" => MouseButton::Right,
                Some(Value::String(button)) if button == "middle" => MouseButton::Middle,
                Some(_) => {
                    return Err(
                        fields.error("\"button\" must be \"left\", \"right\" or \"middle\".")
                    )
                }
            };
            InputAction::Click {
                x: fields.coordinate("x")?,
                y: fields.coordinate("y")?,
                button,
                count: fields.whole("count", 1, MAX_CLICKS, Some(1))? as u8,
                modifiers: fields.modifiers()?,
            }
        }
        "long_press" => {
            fields.only(&["x", "y", "ms", "modifiers"])?;
            InputAction::LongPress {
                x: fields.coordinate("x")?,
                y: fields.coordinate("y")?,
                ms: fields.whole("ms", 1, MAX_BATCH_MS as i64, Some(DEFAULT_LONG_PRESS_MS))? as u32,
                modifiers: fields.modifiers()?,
            }
        }
        "drag" => {
            fields.only(&["x", "y", "toX", "toY", "ms", "modifiers"])?;
            InputAction::Drag {
                x: fields.coordinate("x")?,
                y: fields.coordinate("y")?,
                to_x: fields.coordinate("toX")?,
                to_y: fields.coordinate("toY")?,
                ms: fields.whole("ms", 1, MAX_BATCH_MS as i64, Some(DEFAULT_DRAG_MS))? as u32,
                modifiers: fields.modifiers()?,
            }
        }
        "scroll" => {
            fields.only(&["x", "y", "dx", "dy", "modifiers"])?;
            let dx = fields.whole("dx", -MAX_NOTCHES, MAX_NOTCHES, Some(0))? as i32;
            let dy = fields.whole("dy", -MAX_NOTCHES, MAX_NOTCHES, Some(0))? as i32;
            if dx == 0 && dy == 0 {
                return Err(fields.error(
                    "give \"dy\" (wheel notches, positive scrolls down) or \"dx\" (positive scrolls right).",
                ));
            }
            InputAction::Scroll {
                x: fields.coordinate("x")?,
                y: fields.coordinate("y")?,
                dx,
                dy,
                modifiers: fields.modifiers()?,
            }
        }
        "move" => {
            fields.only(&["x", "y"])?;
            InputAction::Move {
                x: fields.coordinate("x")?,
                y: fields.coordinate("y")?,
            }
        }
        "type" => {
            fields.only(&["text", "via", "chord"])?;
            let text = fields.text("text")?;
            if text.is_empty() {
                return Err(fields.error("\"text\" is empty."));
            }
            let paste = match object.get("via") {
                None | Some(Value::Null) => false,
                Some(Value::String(via)) if via == "auto" => false,
                Some(Value::String(via)) if via == "paste" => true,
                Some(_) => return Err(fields.error("\"via\" must be \"auto\" or \"paste\".")),
            };
            // The flat schema bounds `text` by the longest any `via` takes (a paste's), and each
            // `via` is held to its own here, before anything runs: `auto` to
            // [`MAX_TEXT_CHARS`], `paste` to what a desktop's clipboard takes.
            let longest = if paste {
                desktop::MAX_TEXT_CHARS
            } else {
                MAX_TEXT_CHARS
            };
            if text.chars().count() > longest {
                return Err(fields.error(too_long(paste)));
            }
            let via = if paste {
                TypeVia::Paste {
                    chord: fields.chord()?,
                }
            } else {
                if object.get("chord").is_some_and(|chord| !chord.is_null()) {
                    return Err(fields.error(
                        "\"chord\" is the shortcut that pastes the text, so it goes with via \"paste\"; leave it out, or add via \"paste\".",
                    ));
                }
                TypeVia::Auto
            };
            InputAction::Type {
                text: text.to_owned(),
                via,
            }
        }
        "key" => {
            fields.only(&["key", "repeat"])?;
            let key = fields.text("key")?;
            // The names do not depend on the platform; only what Home means does,
            // and that is settled when the key is pressed.
            keys::parse_chord(key, Platform::Desktop).map_err(|error| fields.error(error))?;
            InputAction::Key {
                key: key.trim().to_owned(),
                repeat: fields.whole("repeat", 1, MAX_REPEAT, Some(1))? as u32,
            }
        }
        "wait" => {
            fields.only(&["ms"])?;
            InputAction::Wait {
                ms: fields.whole("ms", 0, MAX_WAIT_MS, None)? as u32,
            }
        }
        other => {
            return Err(format!(
                "Action {number}: unknown action \"{}\". Actions: {ACTION_NAMES}.",
                sanitize_for_message(other)
            ))
        }
    })
}

/// One action's fields, for reading them with errors that name the action.
struct Fields<'a> {
    number: usize,
    name: &'a str,
    object: &'a Map<String, Value>,
}

/// What a `type` action over its limit is refused with. `via` "auto" is held to
/// [`MAX_TEXT_CHARS`], and says that a desktop takes more through `via` "paste"; a paste is held
/// to what a desktop's clipboard takes ([`desktop::MAX_TEXT_CHARS`]).
fn too_long(paste: bool) -> String {
    if paste {
        format!(
            "\"text\" is longer than {} characters; split it over several type actions.",
            desktop::MAX_TEXT_CHARS
        )
    } else {
        format!(
            "\"text\" is longer than {MAX_TEXT_CHARS} characters; split it over several type actions, or on a desktop use via \"paste\", which takes up to {} characters.",
            desktop::MAX_TEXT_CHARS
        )
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

impl Fields<'_> {
    fn error(&self, message: impl fmt::Display) -> String {
        format!("Action {} ({}): {message}", self.number, self.name)
    }

    fn only(&self, allowed: &[&str]) -> Result<(), String> {
        match self
            .object
            .keys()
            .find(|key| key.as_str() != "action" && !allowed.contains(&key.as_str()))
        {
            Some(unknown) => Err(self.error(format_args!(
                "unknown field \"{}\"; {} takes {}.",
                sanitize_for_message(unknown),
                self.name,
                allowed.join(", ")
            ))),
            None => Ok(()),
        }
    }

    /// A pixel of the coordinate frame: a non-negative number, rounded.
    fn coordinate(&self, key: &str) -> Result<u32, String> {
        match self.object.get(key) {
            None | Some(Value::Null) => Err(self.error(format_args!(
                "\"{key}\" is required: a pixel of the coordinate frame (the last preview_screenshot, or the @x,y boxes of preview_snapshot), counted from its top-left corner."
            ))),
            Some(Value::Number(number)) => match number.as_f64() {
                Some(value) if value.is_finite() && (0.0..=MAX_COORDINATE).contains(&value) => {
                    Ok(value.round() as u32)
                }
                _ => Err(self.error(format_args!(
                    "\"{key}\" must be a non-negative number of pixels; got {number}."
                ))),
            },
            Some(other) => Err(self.error(format_args!(
                "\"{key}\" must be a number of pixels, not {}.",
                json_type(other)
            ))),
        }
    }

    /// A whole number in `min..=max`, or `default` when absent.
    fn whole(&self, key: &str, min: i64, max: i64, default: Option<i64>) -> Result<i64, String> {
        let out_of_range = |shown: &dyn fmt::Display| {
            self.error(format_args!(
                "\"{key}\" must be a whole number from {min} to {max}; got {shown}."
            ))
        };
        match self.object.get(key) {
            None | Some(Value::Null) => {
                default.ok_or_else(|| self.error(format_args!("\"{key}\" is required.")))
            }
            Some(Value::Number(number)) => match number.as_f64() {
                Some(value)
                    if value.is_finite()
                        && value.fract() == 0.0
                        && (min as f64..=max as f64).contains(&value) =>
                {
                    Ok(value as i64)
                }
                _ => Err(out_of_range(number)),
            },
            Some(other) => Err(out_of_range(&json_type(other))),
        }
    }

    fn text(&self, key: &str) -> Result<&str, String> {
        match self.object.get(key) {
            Some(Value::String(text)) => Ok(text),
            None | Some(Value::Null) => Err(self.error(format_args!("\"{key}\" is required."))),
            Some(other) => Err(self.error(format_args!(
                "\"{key}\" must be a string, not {}.",
                json_type(other)
            ))),
        }
    }

    /// A paste's `chord`: a shortcut of keys a desktop has (`"ctrl+shift+v"`), at most
    /// [`MAX_CHORD_CHARS`] characters; none when absent, which is the desktop's default.
    /// Whether the screen is a desktop is for the batch to say, which knows the screen.
    fn chord(&self) -> Result<Option<PasteChord>, String> {
        let text = match self.object.get("chord") {
            None | Some(Value::Null) => return Ok(None),
            Some(Value::String(text)) => text,
            Some(other) => {
                return Err(self.error(format_args!(
                    "\"chord\" must be a string such as \"ctrl+v\", not {}.",
                    json_type(other)
                )))
            }
        };
        let length = text.chars().count();
        if length > MAX_CHORD_CHARS {
            return Err(self.error(format_args!(
                "\"chord\" must be 1-{MAX_CHORD_CHARS} characters, such as \"ctrl+v\"; got {length}."
            )));
        }
        let chord = desktop::paste_chord(text).map_err(|error| self.error(error))?;
        Ok(Some((sanitize_for_message(text.trim()), chord)))
    }

    /// `modifiers`: modifier names joined with `+` (`"shift"`, `"ctrl+shift"`),
    /// 1-64 characters; none when absent. Whether the screen has a keyboard to
    /// hold them on is for the batch to say, which knows the screen.
    fn modifiers(&self) -> Result<Vec<Key>, String> {
        let text = match self.object.get("modifiers") {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::String(text)) => text,
            Some(other) => {
                return Err(self.error(format_args!(
                    "\"modifiers\" must be a string such as \"shift\" or \"ctrl+shift\", not {}.",
                    json_type(other)
                )))
            }
        };
        let length = text.chars().count();
        if length == 0 || length > MAX_MODIFIERS_CHARS {
            return Err(self.error(format_args!(
                "\"modifiers\" must be 1-{MAX_MODIFIERS_CHARS} characters, such as \"shift\" or \"ctrl+shift\"; got {length}."
            )));
        }
        keys::parse_modifiers(text).map_err(|error| self.error(error))
    }
}

/// The row's `remote` field (`docs/remote-control.md` §3.4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSummary {
    pub protocol: Protocol,
    pub platform: Platform,
    /// Where it is, in the English every model-facing text uses
    /// (`docs/remote-control.md` §2.5): `emulator-5554 · this computer`,
    /// `mac · 127.0.0.1:5900`. `preview_list` shows it.
    pub target: String,
    /// The same, in the app's language (`emulator-5554 · 本机`), for the pane's
    /// row. Display only: never put into anything the model reads.
    pub target_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenSize>,
    pub state: SessionPhase,
    pub read_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
    /// Why it failed, while `failed`; why the connection was lost, while
    /// `reconnecting`. One line, for the row to show. Never a credential, a
    /// token or the viewer's address: it is what drivers and servers said about
    /// the connection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Tunables; tests shrink them.
#[derive(Clone, Debug)]
struct Timing {
    backoff: Vec<Duration>,
    ready_wait: Duration,
    input_lock_wait: Duration,
    touch_settle: Duration,
    click_gap: Duration,
    pull_pause: Duration,
    encode_interval: Duration,
    fill_focus: Duration,
    replace: ReplaceEstimates,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff: RECONNECT_BACKOFF.to_vec(),
            ready_wait: READY_WAIT,
            input_lock_wait: INPUT_LOCK_WAIT,
            touch_settle: TOUCH_SETTLE,
            click_gap: CLICK_GAP,
            pull_pause: PULL_PAUSE,
            encode_interval: ENCODE_INTERVAL,
            fill_focus: FILL_FOCUS,
            replace: ReplaceEstimates::default(),
        }
    }
}

/// What the steps of replacing a field's text are expected to take (the `REPLACE_*` constants),
/// which a call must have left before it deletes anything. Kept apart from the real time the
/// steps take so that a test can shrink them.
#[derive(Clone, Copy, Debug)]
struct ReplaceEstimates {
    tap: Duration,
    read: Duration,
    keys: Duration,
    ios_clear: Duration,
    /// Android typing, per piece of [`TEXT_PIECE_CHARS`] characters.
    type_piece: Duration,
    ios_chars_per_second: u64,
}

impl Default for ReplaceEstimates {
    fn default() -> Self {
        Self {
            tap: REPLACE_TAP,
            read: REPLACE_READ,
            keys: REPLACE_KEYS,
            ios_clear: REPLACE_IOS_CLEAR,
            type_piece: REPLACE_TYPE_PIECE,
            ios_chars_per_second: REPLACE_IOS_CHARS_PER_SECOND,
        }
    }
}

impl ReplaceEstimates {
    /// Typing `chars` characters into an Android field: `input text` goes in pieces.
    fn android_typing(&self, chars: usize) -> Duration {
        let pieces = u32::try_from(chars.div_ceil(TEXT_PIECE_CHARS)).unwrap_or(u32::MAX);
        self.type_piece.saturating_mul(pieces)
    }

    /// Typing `chars` characters into an iPhone or iPad field, which WebDriverAgent types at
    /// about [`REPLACE_IOS_CHARS_PER_SECOND`], rounded up to a whole millisecond.
    fn ios_typing(&self, chars: usize) -> Duration {
        let rate = self.ios_chars_per_second.max(1);
        let chars = u64::try_from(chars).unwrap_or(u64::MAX);
        Duration::from_millis(chars.saturating_mul(1000).div_ceil(rate))
    }
}

/// A connected driver and which connection of the session it is, so an event
/// or an error from a connection that has since been replaced changes nothing.
#[derive(Clone)]
struct Live {
    driver: Arc<dyn Driver>,
    connection: u64,
}

/// A sign-in from the viewer's form. The password is wiped when it drops.
struct Answer {
    username: Option<String>,
    password: Zeroizing<String>,
    remember: bool,
}

/// Where the password a connect attempt offered came from.
enum AuthSource {
    Nothing,
    /// What the person typed this session.
    Typed,
    /// The credential store, under this account.
    Stored(String),
}

struct State {
    phase: SessionPhase,
    /// Why it failed, why it is reconnecting, why the sign-in was refused.
    message: Option<String>,
    failure: Option<String>,
    closed: bool,
    live: Option<Live>,
    /// The connection most recently started; events of older ones are stale.
    connection: u64,
    /// A connection that ended, and why.
    lost: Option<(u64, String)>,
    /// The screen as last known: label, platform, size.
    info: Option<DriverInfo>,
    request: Option<CredentialRequest>,
    /// A sign-in submitted and not yet tried.
    answer: Option<Answer>,
    /// The sign-in in use, kept for reconnects.
    auth: Option<Answer>,
    /// An account name typed into the form, to prefill it next time.
    typed_username: Option<String>,
    /// A stored password the server refused: not offered again this session.
    refused_account: Option<String>,
    /// The coordinate frame of the last screenshot or snapshot the model got.
    last_shown: Option<(u32, u32)>,
    uids: HashMap<u64, UidTarget>,
    /// The screen size the uids' centres were measured on.
    uids_screen: (u32, u32),
    next_uid: u64,
    last_input: Option<Instant>,
}

/// What one viewer page has held down or started, so its gestures can be
/// finished and its keys let go.
#[derive(Default)]
struct PageInput {
    /// RFB mask of the buttons held.
    buttons: u8,
    position: (u32, u32),
    /// Keys down, by `KeyboardEvent.code`: a key comes up as what went down,
    /// even when Shift changed its `key` in between. On a touch screen only the
    /// modifiers, which are held here rather than on the device.
    keys: HashMap<String, Key>,
    /// Wheel notches not yet whole.
    wheel: (f64, f64),
    touch: Option<TouchStart>,
}

/// What a page held when it was let go of: the buttons, where, and the keys.
type Held = (u8, (u32, u32), Vec<Key>);

impl PageInput {
    /// Forgets everything this page holds or has started, and returns what was
    /// down on the device, if anything was.
    fn let_go(&mut self) -> Option<Held> {
        self.touch = None;
        self.wheel = (0.0, 0.0);
        let buttons = std::mem::take(&mut self.buttons);
        let keys: Vec<Key> = self.keys.drain().map(|(_, key)| key).collect();
        (buttons != 0 || !keys.is_empty()).then_some((buttons, self.position, keys))
    }
}

/// Lets go of what a page held on a desktop: the buttons come up where the
/// pointer is, then each key.
fn release_held(driver: &dyn Driver, (buttons, (x, y), keys): Held) -> DriverResult<()> {
    let mut result = Ok(());
    if buttons != 0 {
        result = driver.pointer(x, y, 0);
    }
    for key in keys {
        let released = driver.key(&key, Some(false));
        if result.is_ok() {
            result = released;
        }
    }
    result
}

struct TouchStart {
    /// Where it went down, as fractions of the picture.
    from: (f64, f64),
    /// When it reached the host, and when it happened on the page's clock if
    /// the page said so.
    started: Instant,
    page_at: Option<f64>,
    moved: bool,
}

impl TouchStart {
    /// How long the finger was down, on the page's clock when both ends carry
    /// it: the down and the up of a touch can reach the host in one batch,
    /// queued behind a slow gesture, and arrival would make a long press a tap.
    fn held_until(&self, page_at: Option<f64>) -> Duration {
        match (self.page_at, page_at) {
            // Milliseconds as microseconds, rounded: no float error in a whole number.
            (Some(down), Some(up)) => Duration::from_micros(
                ((up - down).clamp(0.0, MAX_PAGE_HOLD.as_secs_f64() * 1000.0) * 1000.0).round()
                    as u64,
            ),
            _ => self.started.elapsed(),
        }
    }
}

/// A gesture synthesised from a touch on the viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TouchGesture {
    Tap((u32, u32)),
    LongPress((u32, u32), u32),
    Swipe((u32, u32), (u32, u32), u32),
}

struct Inner {
    name: String,
    server_id: String,
    target: ControlTarget,
    connector: Arc<dyn Connector>,
    /// What the driver factory dials through: [`Self::connector`], answering
    /// [`Connector::cancelled`] once the session is closed.
    dialler: Arc<dyn Connector>,
    /// Set when the session closes; what `dialler` reports.
    over: Arc<AtomicBool>,
    preflight: Option<Preflight>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    connect: ConnectFn,
    store: Arc<dyn CredentialStore>,
    timing: Timing,
    viewer: Mutex<Option<Viewer>>,
    state: Mutex<State>,
    /// Phase changes, answers, lost connections, closing.
    changed: Condvar,
    frames: FrameHub,
    /// Held by whoever is driving: a model batch, or one viewer request.
    input: Mutex<()>,
    model_active: AtomicBool,
    pages: Mutex<HashMap<String, PageInput>>,
    last_viewer_error: Mutex<Option<(String, Instant)>>,
    last_frame_error: Mutex<Option<String>>,
}

/// One remote screen. Dropping the last handle closes it.
pub struct RemoteControlSession {
    inner: Arc<Inner>,
}

impl Drop for RemoteControlSession {
    fn drop(&mut self) {
        self.inner.shut(None);
    }
}

impl RemoteControlSession {
    /// Starts the viewer and the connection, and returns at once; the session
    /// connects in the background ([`Self::wait_settled`]).
    pub fn start(spec: SessionSpec) -> Result<Arc<Self>, String> {
        Self::start_with(spec, Arc::new(KeyringStore), Timing::default())
    }

    fn start_with(
        spec: SessionSpec,
        store: Arc<dyn CredentialStore>,
        timing: Timing,
    ) -> Result<Arc<Self>, String> {
        let SessionSpec {
            name,
            server_id,
            target,
            connector,
            preflight,
            on_change,
            log,
            connect,
        } = spec;
        let over = Arc::new(AtomicBool::new(false));
        let dialler: Arc<dyn Connector> = Arc::new(SessionConnector {
            place: connector.clone(),
            over: over.clone(),
        });
        let inner = Arc::new(Inner {
            name,
            server_id,
            target,
            connector,
            dialler,
            over,
            preflight,
            on_change,
            log,
            connect: connect.unwrap_or_else(default_connect),
            store,
            timing,
            viewer: Mutex::new(None),
            state: Mutex::new(State {
                phase: SessionPhase::Connecting,
                message: None,
                failure: None,
                closed: false,
                live: None,
                connection: 0,
                lost: None,
                info: None,
                request: None,
                answer: None,
                auth: None,
                typed_username: None,
                refused_account: None,
                last_shown: None,
                uids: HashMap::new(),
                uids_screen: (0, 0),
                next_uid: snapshot::FIRST_UID,
                last_input: None,
            }),
            changed: Condvar::new(),
            frames: FrameHub::new(),
            input: Mutex::new(()),
            model_active: AtomicBool::new(false),
            pages: Mutex::new(HashMap::new()),
            last_viewer_error: Mutex::new(None),
            last_frame_error: Mutex::new(None),
        });
        let weak: Weak<Inner> = Arc::downgrade(&inner);
        let backend: Weak<dyn ViewerBackend> = weak;
        let viewer = Viewer::start(backend).map_err(|error| {
            format!("Could not open the preview pane's page for this screen: {error}")
        })?;
        *lock(&inner.viewer) = Some(viewer);
        let session = Arc::new(Self { inner });
        let supervisor = session.inner.clone();
        thread::Builder::new()
            .name("remote-control-session".into())
            .spawn(move || supervise(&supervisor))
            .map_err(|error| format!("Could not start the remote-control session: {error}"))?;
        let pump = session.inner.clone();
        thread::Builder::new()
            .name("remote-control-frames".into())
            .spawn(move || pump_frames(&pump))
            .map_err(|error| format!("Could not start the remote-control session: {error}"))?;
        Ok(session)
    }

    /// Blocks until the session is ready, needs the person to sign in or has
    /// failed, or `timeout` passes; returns the phase it is in.
    pub fn wait_settled(&self, timeout: Duration) -> SessionPhase {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.inner.state);
        loop {
            if state.closed
                || matches!(
                    state.phase,
                    SessionPhase::Ready | SessionPhase::Credentials | SessionPhase::Failed
                )
            {
                return state.phase;
            }
            let now = Instant::now();
            if now >= deadline {
                return state.phase;
            }
            state = self
                .inner
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    pub fn phase(&self) -> SessionPhase {
        lock(&self.inner.state).phase
    }

    /// Why the session failed, once it has.
    #[cfg(test)]
    pub fn failure(&self) -> Option<String> {
        lock(&self.inner.state).failure.clone()
    }

    pub fn summary(&self) -> RemoteSummary {
        let inner = &self.inner;
        let state = lock(&inner.state);
        let info = state.info.as_ref();
        RemoteSummary {
            protocol: inner.target.protocol,
            platform: info.map_or(inner.target.protocol.platform(), |info| info.platform),
            target: inner.target_label(inner.connector.place_label()),
            target_display: inner.target_label(inner.connector.display_label()),
            label: info
                .map(|info| one_line(&info.label, MAX_LABEL_CHARS))
                .filter(|label| !label.trim().is_empty()),
            screen: info
                .filter(|info| info.width > 0 && info.height > 0)
                .map(|info| ScreenSize {
                    width: info.width,
                    height: info.height,
                }),
            state: state.phase,
            read_only: inner.target.read_only,
            launch: inner.target.launch.clone(),
            message: match state.phase {
                SessionPhase::Failed => state.failure.as_deref(),
                SessionPhase::Reconnecting => state.message.as_deref(),
                _ => None,
            }
            .map(|text| one_line(text, MAX_SUMMARY_MESSAGE_CHARS))
            .filter(|text| !text.is_empty()),
        }
    }

    /// A viewer in this process starts watching. `wake` runs, on another thread, whenever there is
    /// something new to take; frames flow, and the screen stays busy, only until [`Self::unwatch`].
    pub fn watch(&self, wake: Box<dyn Fn() + Send + Sync>) -> Arc<LiveScreen> {
        self.inner.frames.watch(wake)
    }

    pub fn unwatch(&self, screen: &Arc<LiveScreen>) {
        self.inner.frames.unwatch(screen);
    }

    /// The state a viewer draws: the same a viewer page polls for.
    pub fn viewer_state(&self) -> ViewerState {
        ViewerBackend::state(self.inner.as_ref())
    }

    /// What a person did on a viewer in this process. `viewer` names it, as a page's id does. This
    /// waits for the screen, so not on a thread that draws.
    pub fn viewer_input(&self, viewer: &str, events: Vec<ViewerEvent>) {
        self.inner.viewer_input(viewer, events);
    }

    /// A viewer in this process went away with something held.
    pub fn release_viewer(&self, viewer: &str) {
        self.inner.release_page(viewer);
    }

    /// The sign-in form's answer. This may wait for the screen too.
    pub fn answer_credentials(&self, credentials: ViewerCredentials) {
        self.inner.answer(credentials);
    }

    /// The viewer page's address, for the preview pane only.
    pub fn viewer_url(&self) -> String {
        lock(&self.inner.viewer)
            .as_ref()
            .map(Viewer::url)
            .unwrap_or_default()
    }

    /// Whether a page at `page_url` is this session's viewer.
    pub fn shows(&self, page_url: &str) -> bool {
        lock(&self.inner.viewer)
            .as_ref()
            .is_some_and(|viewer| viewer.shows(page_url))
    }

    /// The device lock's key: `<protocol>|<place key>|<port>|<device or ''>`. Tests hold it
    /// to [`super::place::device_lock_key`], which the registry uses before a session exists.
    #[cfg(test)]
    pub fn lock_key(&self) -> String {
        let target = &self.inner.target;
        format!(
            "{}|{}|{}|{}",
            target.protocol.as_str(),
            self.inner.connector.place_key(),
            target.port,
            target.device.as_deref().unwrap_or("")
        )
    }

    /// What `preview_start` tells the model: what it reached, the screen and
    /// the coordinate frame, which tools apply — or that the person has to sign
    /// in, or that it is still connecting. Never the viewer's address.
    pub fn receipt(&self) -> String {
        let inner = &self.inner;
        let (phase, info, message, failure, request) = {
            let state = lock(&inner.state);
            (
                state.phase,
                state.info.clone(),
                state.message.clone(),
                state.failure.clone(),
                state.request.clone(),
            )
        };
        let what = inner.describe_target();
        let platform = info
            .as_ref()
            .map_or(inner.target.protocol.platform(), |info| info.platform);
        let mut text = String::new();
        match phase {
            SessionPhase::Ready => {
                let _ = write!(text, "Connected to {what}");
                if let Some(label) = info
                    .as_ref()
                    .map(|info| one_line(&info.label, MAX_LABEL_CHARS))
                    .filter(|label| !label.trim().is_empty())
                {
                    let _ = write!(text, " ({label})");
                }
                text.push('.');
                if let Some(info) = info
                    .as_ref()
                    .filter(|info| info.width > 0 && info.height > 0)
                {
                    let (width, height) = frame::model_frame(info.width, info.height);
                    let _ = write!(
                        text,
                        " The screen is {}x{} pixels; preview_screenshot shows it as a {width}x{height} image, and every x/y for preview_input is a pixel of that image (coordinate frame {width}x{height}).",
                        info.width, info.height
                    );
                }
                const TOUCH_TOOLS: &str = " Use preview_screenshot to look, preview_snapshot for the element tree (uids for preview_click and preview_fill, boxes in the same coordinate frame), and preview_input to tap, long-press, swipe, scroll, type and press keys";
                match platform {
                    Platform::Android => {
                        text.push_str(TOUCH_TOOLS);
                        text.push_str(" (Back, Home for the home screen, AppSwitch, Enter...).");
                    }
                    // WebDriverAgent has no Back or app-switch button to press.
                    Platform::Ios => {
                        text.push_str(TOUCH_TOOLS);
                        text.push_str(" (Home for the home screen, VolumeUp, VolumeDown, Enter, Backspace). To go back, use the app's own controls or swipe in from the left edge.");
                    }
                    Platform::Desktop => text.push_str(" Use preview_screenshot to look and preview_input to click, drag, scroll, type and press keys; this screen has no element tree, so act by coordinates rather than preview_snapshot."),
                }
                // Worded to stay true whether or not the platform's tool group is offered: the
                // session cannot know which tools the model has.
                text.push_str(match platform {
                    Platform::Android => " When the screen tools are on, screen_app, screen_device, screen_package, preview_zoom and preview_wait also act on this device, and android_logcat and android_shell when the Android tools are on.",
                    Platform::Ios => " When the screen tools are on, screen_app, screen_device, screen_clipboard, preview_zoom and preview_wait also act on this device.",
                    Platform::Desktop => " When the screen tools are on, screen_clipboard, preview_zoom and preview_wait also act on it. Text its keys cannot type goes through preview_input's type with via \"paste\".",
                });
                if inner.target.read_only {
                    text.push_str(" It is read-only: preview_input, preview_click, preview_fill, preview_dialog and every other tool that changes the device are refused; looking still works.");
                }
            }
            SessionPhase::Credentials => {
                let needs_username = request
                    .as_ref()
                    .is_some_and(|request| request.needs_username);
                let _ = write!(
                    text,
                    "Reached {what}, which asks for {}. The user must sign in in the preview pane; ask them to, then take a preview_screenshot once they have. The password never passes through you.",
                    if needs_username {
                        "an account name and password"
                    } else {
                        "a password"
                    }
                );
                if request.as_ref().is_some_and(|request| request.retry) {
                    text.push_str(" The last password was refused.");
                }
            }
            SessionPhase::Connecting => {
                let _ = write!(text, "Still connecting to {what}");
                if inner.target.protocol == Protocol::Adb && inner.target.launch.is_some() {
                    text.push_str(", waiting for the device to come online and finish booting");
                }
                let _ = write!(
                    text,
                    "; it keeps trying in the background. Take a preview_screenshot in a few seconds; {}.",
                    inner.logs_hint("the progress")
                );
            }
            SessionPhase::Reconnecting => {
                let _ = write!(
                    text,
                    "The connection to {what} was lost ({}); it is reconnecting in the background. Take a preview_screenshot in a few seconds; {}.",
                    message.as_deref().map_or_else(|| "no reason given".to_owned(), |message| one_line(message, MAX_LOG_CHARS)),
                    inner.logs_hint("the progress")
                );
            }
            SessionPhase::Failed => {
                let _ = write!(
                    text,
                    "Could not connect to {what}: {} {}.",
                    sentence(failure.as_deref().unwrap_or("the connection failed")),
                    inner.logs_hint("the connection log")
                );
            }
        }
        text
    }

    /// A fresh picture of the screen, scaled to the coordinate frame.
    #[cfg(test)]
    pub fn screenshot(&self) -> Result<ModelScreenshot, String> {
        self.screenshot_within(&legacy_call(&|| false))
    }

    /// `Self::screenshot` inside `call`: the capture starts only if the
    /// screen was ready in time.
    pub fn screenshot_within(&self, call: &CallContext) -> Result<ModelScreenshot, String> {
        let inner = &self.inner;
        let budget = call.budget();
        let live = inner.ready_within(&budget)?;
        inner.settle(&live, &budget);
        budget.proceed("taking the screenshot")?;
        let picture = live
            .driver
            .remeasure()
            .map_err(|error| inner.failed_call(&live, error, "Could not capture the screen"))?;
        let (jpeg, width, height) = frame::encode_for_model(&picture)
            .map_err(|error| format!("Could not capture the screen: {error}"))?;
        inner.note_size(picture.width, picture.height);
        inner.shown((width, height));
        Ok(ModelScreenshot {
            jpeg,
            width,
            height,
            screen_width: picture.width,
            screen_height: picture.height,
        })
    }

    /// The screen's element tree with uids, or why there is none.
    #[cfg(test)]
    pub fn snapshot(&self) -> Result<String, String> {
        self.snapshot_within(&legacy_call(&|| false))
    }

    /// `Self::snapshot` inside `call`.
    pub fn snapshot_within(&self, call: &CallContext) -> Result<String, String> {
        let inner = &self.inner;
        let budget = call.budget();
        let live = inner.ready_within(&budget)?;
        if !live.driver.info().has_ui_tree {
            return Err(snapshot::NO_TREE.to_owned());
        }
        inner.settle(&live, &budget);
        // Before the tree: a pulling driver learns a turned screen from a capture,
        // and WebDriverAgent converts the tree's points with what it learned.
        let mut screen = inner.current_screen(&live, &budget)?;
        let read_tree = || -> Result<UiNode, String> {
            budget.proceed("reading the element tree")?;
            live.driver
                .ui_tree()
                .map_err(|error| {
                    inner.failed_call(&live, error, "Could not read the screen's element tree")
                })?
                .ok_or_else(|| snapshot::NO_TREE.to_owned())
        };
        let mut tree = read_tree()?;
        // A capture after the last input is trusted, but one taken while the
        // screen was still turning (preview_input's own screenshot, about 300 ms
        // after the tap; an Android activity takes longer to come back) has the
        // old size. A tree wider or taller than that screen was read after the
        // turn: the screen is measured once more, and the tree read again if
        // that changed it, since WebDriverAgent converts it with what it measured.
        if reaches_past(&tree, screen) {
            budget.proceed("measuring the screen")?;
            let measured = inner.measure(&live)?;
            if measured != screen {
                screen = measured;
                tree = read_tree()?;
            }
        }
        let frame = frame::model_frame(screen.0, screen.1);
        let mut state = lock(&inner.state);
        let rendered = snapshot::render(&tree, state.next_uid, screen, frame);
        state.next_uid = rendered.next_uid;
        state.uids = rendered.uids.into_iter().collect();
        state.uids_screen = screen;
        state.last_shown = Some(frame);
        Ok(rendered.text)
    }

    /// Clicks (taps) the middle of an element of the latest snapshot.
    #[cfg(test)]
    pub fn click_uid(&self, uid: u64, double: bool) -> Result<String, String> {
        self.click_uid_within(uid, double, &legacy_call(&|| false))
    }

    /// `Self::click_uid` inside `call`: nothing is clicked once it is
    /// cancelled or out of time.
    pub fn click_uid_within(
        &self,
        uid: u64,
        double: bool,
        call: &CallContext,
    ) -> Result<String, String> {
        let inner = &self.inner;
        inner.refuse_read_only()?;
        let budget = call.budget();
        let (_input, live) = inner.take_screen(&budget)?;
        let (screen, frame) = inner.check_frame(&live, &budget)?;
        let (target, point) = inner.uid_point(uid, screen, &live)?;
        budget.proceed("clicking; nothing was clicked")?;
        let gesture = inner.gesture(&live, screen, frame, budget);
        let result = gesture.click_at(point, MouseButton::Left, if double { 2 } else { 1 });
        inner.touched();
        result.map_err(|error| inner.failed_call(&live, error, "The click failed"))?;
        let (x, y) =
            frame::screen_to_frame((i64::from(point.0), i64::from(point.1)), screen, frame);
        Ok(format!(
            "{} [{uid}] {} at ({x}, {y}).",
            if double { "Double-clicked" } else { "Clicked" },
            describe_uid(&target)
        ))
    }

    /// Fills an element of the latest snapshot: clicks it to focus it, then types `value`. On
    /// an Android or an iPhone or iPad field that replaces its text, as a page's field is —
    /// once the text is known and can be removed ([`Inner::replace_field`],
    /// [`Inner::replace_ios_field`]); on any other screen the field's existing text is kept,
    /// since there is no general "clear this field".
    #[cfg(test)]
    pub fn fill_uid(&self, uid: u64, value: &str) -> Result<String, String> {
        self.fill_uid_within(uid, value, &legacy_call(&|| false))
    }

    /// `Self::fill_uid` inside `call`. Long text is typed in pieces; a call
    /// cancelled or out of time stops between them and says how much was typed.
    pub fn fill_uid_within(
        &self,
        uid: u64,
        value: &str,
        call: &CallContext,
    ) -> Result<String, String> {
        let inner = &self.inner;
        inner.refuse_read_only()?;
        if value.chars().count() > MAX_TEXT_CHARS {
            return Err(format!(
                "\"value\" is longer than {MAX_TEXT_CHARS} characters; type the rest with preview_input."
            ));
        }
        let budget = call.budget();
        let (_input, live) = inner.take_screen(&budget)?;
        let (screen, frame) = inner.check_frame(&live, &budget)?;
        let (target, point) = inner.uid_point(uid, screen, &live)?;
        // What cannot be typed at all is refused before the field is touched.
        if !value.is_empty() {
            live.driver
                .check_text(value)
                .map_err(|error| format!("Nothing was clicked or typed: {error}"))?;
        }
        // An Android or iOS field is replaced. (Only a driver without its platform's extension,
        // a fake, has no version to ask and no WebDriverAgent to clear with: it keeps the old
        // text like the screens below.)
        let platform = live.driver.info().platform;
        let fill = Fill {
            uid,
            target: &target,
            point,
            value,
            screen,
            frame,
        };
        if platform == Platform::Android {
            if let Some(android) = live.driver.android() {
                return inner.replace_field(&live, android, fill, budget);
            }
        }
        if platform == Platform::Ios && live.driver.ios().is_some() {
            return inner.replace_ios_field(&live, fill, budget);
        }
        budget.proceed("clicking the field; nothing was clicked or typed")?;
        let gesture = inner.gesture(&live, screen, frame, budget);
        let clicked = gesture.click_at(point, MouseButton::Left, 1);
        inner.touched();
        clicked.map_err(|error| {
            inner.failed_call(&live, error, "The click that focuses the field failed")
        })?;
        let described = describe_uid(&target);
        let total = value.chars().count();
        if !value.is_empty() {
            inner.type_into_field(
                &live,
                &gesture,
                &format!("Clicked [{uid}] {described}"),
                value,
            )?;
        }
        Ok(format!(
            "Clicked [{uid}] {described} and typed {total} characters; text already in the field was kept."
        ))
    }

    /// Runs a `preview_input` batch. `Err` only when it could not start; an
    /// action that fails is reported on its line and ends the batch.
    #[cfg(test)]
    pub fn run_input(
        &self,
        actions: &[InputAction],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<InputReport, String> {
        self.run_input_within(actions, &legacy_call(cancelled))
    }

    /// `Self::run_input` inside `call`. No action starts once the call is
    /// cancelled or would end past the deadline; the report then ends with the
    /// line that says so, after the lines of the actions that ran.
    pub fn run_input_within(
        &self,
        actions: &[InputAction],
        call: &CallContext,
    ) -> Result<InputReport, String> {
        let inner = &self.inner;
        inner.refuse_read_only()?;
        if actions.is_empty() {
            return Err("\"actions\" is empty; give at least one action.".to_owned());
        }
        // Refused whole, before any action runs and before the person's held
        // keys are let go of: the batch was written for a keyboard.
        if self.platform().is_touch() {
            if let Some((index, action)) = actions
                .iter()
                .enumerate()
                .find(|(_, action)| !action.modifiers().is_empty())
            {
                return Err(format!(
                    "Action {} ({}): {MODIFIERS_NEED_A_DESKTOP}",
                    index + 1,
                    action.name()
                ));
            }
        }
        let budget = call.budget();
        let (_input, live) = inner.take_screen(&budget)?;
        // With the lock held and the connection as it now is, before the first action: what the
        // screen cannot take is refused for the whole batch, so a refusal sends nothing, and
        // `mod` becomes the key this screen's system means by it.
        let resolved = inner.check_batch(&live, actions)?;
        let actions: &[InputAction] = &resolved;
        let (screen, frame) = inner.check_frame(&live, &budget)?;
        let gesture = inner.gesture(&live, screen, frame, budget);
        let total = actions.len();
        let mut lines = Vec::with_capacity(total + 1);
        for (index, action) in actions.iter().enumerate() {
            let number = index + 1;
            let rest = not_run(number + 1, total);
            let halt = budget.halt().or_else(|| {
                // Never start what could not end in time.
                (!budget.fits(Duration::from_millis(action.needs_ms()))).then_some(Halt::OutOfTime)
            });
            match halt {
                Some(Halt::Cancelled) => {
                    lines.push(format!(
                        "{number}. {}: cancelled before it ran{rest}",
                        action.describe()
                    ));
                    break;
                }
                Some(Halt::OutOfTime) => {
                    lines.push(out_of_time_after(index, total));
                    break;
                }
                None => {}
            }
            if action.point().is_some() {
                if let Err(message) = inner.same_size(&live, screen) {
                    lines.push(format!(
                        "{number}. {}: not run: {message}{rest}",
                        action.describe()
                    ));
                    break;
                }
            }
            let result = gesture.perform(action);
            inner.touched();
            let error = match result {
                Ok(()) => {
                    // A paste says what it did and what the clipboard holds now.
                    let done = gesture.note.take().unwrap_or_else(|| "ok".to_owned());
                    lines.push(format!("{number}. {}: {done}", action.describe()));
                    continue;
                }
                Err(error) => error,
            };
            inner.note_lost(&live, &error);
            let typed = gesture
                .typed
                .take()
                .filter(|(done, _)| *done > 0)
                .map(|(done, all)| format!(" after typing {done} of {all} characters"))
                .unwrap_or_default();
            lines.push(match gesture.halted.take() {
                Some(halt) => format!(
                    "{number}. {}: stopped: {}{typed}{rest}",
                    action.describe(),
                    halt.word()
                ),
                None => format!(
                    "{number}. {}: failed{typed}: {error}{rest}",
                    action.describe()
                ),
            });
            break;
        }
        Ok(InputReport { lines })
    }

    /// Disconnects, stops the viewer and forgets the password. Idempotent.
    pub fn close(&self) {
        self.inner.shut(None);
    }

    /// Ends the session as failed with `reason` and closes it, in one step:
    /// what the registry does when something the screen depends on is gone —
    /// the entry its `launch` started has stopped. Unlike [`Self::close`], the
    /// failure stays readable: the row is kept as failed, and every tool call on
    /// it answers with `reason` instead of a reconnect that cannot succeed. A
    /// session that had already failed keeps its first reason.
    pub fn end(&self, reason: &str) {
        self.inner.shut(Some(reason.to_owned()));
    }
}

/// Why a phone is never asked to paste, and what to do there instead.
fn paste_needs_a_desktop(platform: Platform) -> String {
    let instead = match platform {
        Platform::Android => "type the text with via \"auto\" (printable ASCII only)",
        _ => "type the text with via \"auto\", or put it on the clipboard with screen_clipboard and paste it in the app",
    };
    format!("via \"paste\" needs a desktop: a phone's clipboard is out of reach here; {instead}.")
}

/// The key `mod` is on this screen's system, or why that cannot be said: Android's shortcuts
/// use ctrl, Apple's VNC server (macOS Screen Sharing) uses cmd, and no other desktop
/// identifies its system over VNC.
fn mod_key(driver: &dyn Driver, platform: Platform) -> Result<NamedKey, String> {
    match platform {
        Platform::Android => Ok(NamedKey::Control),
        Platform::Ios => Err(
            "an iPhone or iPad takes no modifier chords through WebDriverAgent; use the on-screen controls."
                .to_owned(),
        ),
        Platform::Desktop => {
            if driver.desktop().is_some_and(|desktop| desktop.is_macos()) {
                Ok(NamedKey::Meta)
            } else {
                Err("mod is cmd on macOS Screen Sharing and ctrl on Android; this desktop's system is unknown, so name ctrl or cmd.".to_owned())
            }
        }
    }
}

/// The tail of a report line for the actions after the one that stopped the
/// batch: ` (action 3 did not run)`, ` (actions 3-5 did not run)`, or nothing.
fn not_run(first: usize, total: usize) -> String {
    if first > total {
        String::new()
    } else if first == total {
        format!(" (action {total} did not run)")
    } else {
        format!(" (actions {first}-{total} did not run)")
    }
}

/// The report line of a batch that ran out of time after `done` actions.
fn out_of_time_after(done: usize, total: usize) -> String {
    let rest = not_run(done + 1, total);
    if done == 0 {
        format!("stopped: out of time before action 1{rest}")
    } else {
        format!("stopped: out of time after action {done}{rest}")
    }
}

/// The session's connector as its driver factory dials through it: the place's
/// own, which also answers [`Connector::cancelled`] once the session closed.
struct SessionConnector {
    place: Arc<dyn Connector>,
    over: Arc<AtomicBool>,
}

impl Connector for SessionConnector {
    fn connect(
        &self,
        port: u16,
        timeout: Duration,
    ) -> Result<super::transport::Connection, String> {
        self.place.connect(port, timeout)
    }

    fn place_label(&self) -> &str {
        self.place.place_label()
    }

    fn display_label(&self) -> &str {
        self.place.display_label()
    }

    fn place_key(&self) -> &str {
        self.place.place_key()
    }

    fn is_local(&self) -> bool {
        self.place.is_local()
    }

    fn cancelled(&self) -> bool {
        self.over.load(Ordering::Acquire) || self.place.cancelled()
    }
}

/// `text` on one line and at most `max_chars` characters: what a device or a
/// machine calls itself is shown to the model, and must not start a line of
/// its own there. Bidirectional controls and zero-width characters
/// ([`is_hidden_format`]) are dropped: they reorder or hide what a person reads.
fn one_line(text: &str, max_chars: usize) -> String {
    let visible: String = text
        .chars()
        .filter(|character| !is_hidden_format(*character))
        .collect();
    let mut line: String = visible
        .trim()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars + 1)
        .collect();
    if line.chars().count() > max_chars {
        line = line.chars().take(max_chars).collect();
        line.push_str("...");
    }
    line
}

/// `text` as a sentence: ending in a full stop.
fn sentence(text: &str) -> String {
    let trimmed = text.trim_end();
    if trimmed.ends_with(['.', '!', '?']) {
        trimmed.to_owned()
    } else {
        format!("{trimmed}.")
    }
}

fn describe_uid(target: &UidTarget) -> String {
    if target.name.is_empty() {
        target.role.clone()
    } else {
        format!("{} \"{}\"", target.role, target.name)
    }
}

/// A field's text as an answer quotes it: on one line, cut short, with its length.
fn quoted_text(text: &str) -> String {
    format!(
        "\"{}\" ({})",
        one_line(text, FIELD_TEXT_SHOWN),
        desktop::characters(text.chars().count())
    )
}

/// What `preview_fill` is filling: the field's uid and the element it named, the point to
/// tap, the value, and the screen and coordinate frame they are measured in.
struct Fill<'a> {
    uid: u64,
    target: &'a UidTarget,
    point: (u32, u32),
    value: &'a str,
    screen: (u32, u32),
    frame: (u32, u32),
}

/// What an iPhone or iPad field holds as the screen shows it now: its text (`None` when empty or
/// only its placeholder) and its label, which with the role and the text finds it again.
struct IosField {
    text: Option<String>,
    label: Option<String>,
}

/// The element of `tree` that has the focus and is the one `target` named — the same role and
/// the same box — or why not, in words that continue "but …".
fn focused_field<'t>(tree: &'t UiNode, target: &UidTarget) -> Result<&'t UiNode, String> {
    let mut focused = Vec::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if node.focused {
            focused.push(node);
        }
        pending.extend(node.children.iter());
    }
    let is_the_field = |node: &&UiNode| {
        node.bounds.is_some() && node.bounds == target.bounds && node.role.trim() == target.role
    };
    let mut matching = focused.iter().copied().filter(is_the_field);
    match (matching.next(), matching.next()) {
        (Some(field), None) => Ok(field),
        (Some(_), Some(_)) => Err(
            "more than one focused element has that role and box, so it is not known which is the field"
                .to_owned(),
        ),
        (None, _) => Err(match focused.first() {
            Some(other) => format!(
                "tapping it did not focus it (the focused element is {} \"{}\", or the field moved; take a new preview_snapshot)",
                one_line(&other.role, 60),
                one_line(&other.name, MAX_LABEL_CHARS)
            ),
            None => "tapping it did not focus it (nothing on the screen has the focus)".to_owned(),
        }),
    }
}

/// The one element of `tree` that is the one `target` named — the same role and the same box —
/// focused or not (WebDriverAgent's tree seldom marks an iOS field focused), or why not, as a
/// clause that follows a colon.
fn element_named<'t>(tree: &'t UiNode, target: &UidTarget) -> Result<&'t UiNode, String> {
    let Some(bounds) = target.bounds else {
        return Err(
            "the snapshot did not record the field's box, so it cannot be found again".to_owned(),
        );
    };
    let mut found = Vec::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        if node.bounds == Some(bounds) && node.role.trim() == target.role {
            found.push(node);
        }
        pending.extend(node.children.iter());
    }
    match found.as_slice() {
        [field] => Ok(field),
        [] => Err(
            "the field is not on the screen where the snapshot saw it (the screen moved or changed since; take a new preview_snapshot)"
                .to_owned(),
        ),
        _ => Err(
            "more than one element has the field's role and box, so it is not known which is the field"
                .to_owned(),
        ),
    }
}

impl Inner {
    fn log_line(&self, line: &str) {
        let cleaned: String = line
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(MAX_LOG_CHARS)
            .collect();
        (self.log)(&cleaned);
    }

    fn notify_change(&self) {
        (self.on_change)();
    }

    /// The target with its place called `place`: `emulator-5554 · this
    /// computer`, `mac · 127.0.0.1:5900`. The connector's place label for
    /// what the model reads, its display label for the pane.
    fn target_label(&self, place: &str) -> String {
        let port = self.target.port;
        match (&self.target.device, self.target.protocol) {
            (Some(device), Protocol::Adb) if port == Protocol::Adb.default_port() => {
                format!("{device} · {place}")
            }
            (Some(device), Protocol::Adb) => format!("{device} · {place} · 127.0.0.1:{port}"),
            _ => format!("{place} · 127.0.0.1:{port}"),
        }
    }

    /// The target in a sentence for the model.
    fn describe_target(&self) -> String {
        let place = one_line(self.connector.place_label(), MAX_LABEL_CHARS);
        let port = self.target.port;
        match self.target.protocol {
            Protocol::Vnc => format!("the VNC server on {place} (127.0.0.1:{port})"),
            Protocol::Adb => match &self.target.device {
                Some(device) => format!(
                    "the Android device {} through the adb server on {place} (127.0.0.1:{port})",
                    one_line(device, MAX_LABEL_CHARS)
                ),
                None => {
                    format!("the Android device of the adb server on {place} (127.0.0.1:{port})")
                }
            },
            Protocol::Wda => format!("WebDriverAgent (iOS) on {place} (127.0.0.1:{port})"),
        }
    }

    /// `preview_logs with serverId "<row>" shows <what>`: `preview_logs` without
    /// a serverId reads some other row (the `launch` sibling, started first)
    /// and skips a failed one.
    fn logs_hint(&self, what: &str) -> String {
        format!(
            "preview_logs with serverId \"{}\" shows {what}",
            one_line(&self.server_id, MAX_LABEL_CHARS)
        )
    }

    fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// Whether the session will never connect again: closed, or failed. Failed
    /// is final — nothing the supervisor or a late connect does changes it.
    fn is_over(&self) -> bool {
        over(&lock(&self.state))
    }

    /// Sleeps unless the session closes or fails first; whether it may go on.
    fn pause(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut state = lock(&self.state);
        while !over(&state) {
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        false
    }

    /// Moves to `phase` and tells everyone — unless the session is over.
    fn set_phase(&self, phase: SessionPhase, message: Option<String>, log: &str) {
        {
            let mut state = lock(&self.state);
            if over(&state) {
                return;
            }
            state.phase = phase;
            state.message = message;
        }
        self.changed.notify_all();
        self.log_line(log);
        self.notify_change();
    }

    /// Ends the session as failed. The viewer stays up, so the person reads why.
    fn fail(&self, reason: String) {
        let live = {
            let mut state = lock(&self.state);
            if over(&state) {
                return;
            }
            mark_failed(&mut state, &reason);
            state.live.take()
        };
        self.frames.set_pulling(false);
        if let Some(live) = live {
            live.driver.close();
        }
        self.changed.notify_all();
        self.log_line(&format!("failed: {reason}"));
        self.notify_change();
    }

    /// Closes the session — failing it first with `failure`, in the same step,
    /// so nothing can come between the two and leave a closed session that
    /// reads as connecting or connected.
    fn shut(&self, failure: Option<String>) {
        let (live, failed) = {
            let mut state = lock(&self.state);
            if state.closed {
                return;
            }
            let failed = match failure {
                Some(reason) if state.phase != SessionPhase::Failed => {
                    mark_failed(&mut state, &reason);
                    Some(reason)
                }
                _ => None,
            };
            state.closed = true;
            // Dropping them wipes the passwords.
            state.auth = None;
            state.answer = None;
            state.uids.clear();
            (state.live.take(), failed)
        };
        self.over.store(true, Ordering::Release);
        self.changed.notify_all();
        self.frames.close();
        if let Some(live) = live {
            live.driver.close();
        }
        if let Some(viewer) = lock(&self.viewer).as_ref() {
            viewer.stop();
        }
        // Never held across a driver call, so this does not wait on one.
        lock(&self.pages).clear();
        if let Some(reason) = &failed {
            self.log_line(&format!("failed: {reason}"));
        }
        self.log_line("disconnected: the session was stopped");
        if failed.is_some() {
            self.notify_change();
        }
    }

    /// The events callback for connection `connection`. It holds the session
    /// weakly: the driver is the session's, not the other way round.
    fn events(self: &Arc<Self>, connection: u64) -> DriverEvents {
        let weak = Arc::downgrade(self);
        Arc::new(move |event| {
            if let Some(inner) = weak.upgrade() {
                inner.on_event(connection, event);
            }
        })
    }

    fn on_event(&self, connection: u64, event: DriverEvent) {
        match event {
            DriverEvent::Frame => self.frames.mark_dirty(),
            DriverEvent::Resized { width, height } => {
                let changed = {
                    let mut state = lock(&self.state);
                    let current = state.connection == connection;
                    match state.info.as_mut() {
                        Some(info) if current && (info.width, info.height) != (width, height) => {
                            info.width = width;
                            info.height = height;
                            true
                        }
                        _ => false,
                    }
                };
                if changed {
                    self.log_line(&format!("the screen is now {width}x{height}"));
                    self.notify_change();
                }
            }
            DriverEvent::Disconnected(reason) => self.lost(connection, reason),
            DriverEvent::Log(line) => self.log_line(&line),
        }
    }

    /// Connection `connection` ended. Only the newest connection counts.
    fn lost(&self, connection: u64, reason: String) {
        {
            let mut state = lock(&self.state);
            if over(&state) || state.connection != connection || state.lost.is_some() {
                return;
            }
            state.lost = Some((connection, reason));
        }
        self.changed.notify_all();
    }

    /// A driver call that answered `Disconnected` means the connection is gone,
    /// whether or not the driver said so on its own.
    fn note_lost(&self, live: &Live, error: &DriverError) {
        if let DriverError::Disconnected(reason) = error {
            self.lost(live.connection, reason.clone());
        }
    }

    fn failed_call(&self, live: &Live, error: DriverError, what: &str) -> String {
        self.note_lost(live, &error);
        format!("{what}: {error}")
    }

    /// The sign-in to offer the next VNC connect, and where it came from: what
    /// the person typed this session, else what the credential store holds.
    fn vnc_auth(&self) -> (VncAuth, AuthSource) {
        let (username, refused) = {
            let state = lock(&self.state);
            if let Some(answer) = &state.auth {
                let auth = VncAuth {
                    username: answer
                        .username
                        .clone()
                        .or_else(|| self.target.username.clone()),
                    password: Some(answer.password.clone()),
                };
                return (auth, AuthSource::Typed);
            }
            (
                self.target
                    .username
                    .clone()
                    .or_else(|| state.typed_username.clone()),
                state.refused_account.clone(),
            )
        };
        let account = credentials::account(
            self.connector.place_key(),
            self.target.port,
            username.as_deref(),
        );
        if refused.as_deref() == Some(account.as_str()) {
            return (
                VncAuth {
                    username,
                    password: None,
                },
                AuthSource::Nothing,
            );
        }
        match self.store.get(&account) {
            Ok(Some(password)) => (
                VncAuth {
                    username,
                    password: Some(password),
                },
                AuthSource::Stored(account),
            ),
            Ok(None) => (
                VncAuth {
                    username,
                    password: None,
                },
                AuthSource::Nothing,
            ),
            Err(error) => {
                self.log_line(&error);
                (
                    VncAuth {
                        username,
                        password: None,
                    },
                    AuthSource::Nothing,
                )
            }
        }
    }

    /// The server refused what `source` offered: never offer it again.
    fn refused(&self, source: &AuthSource) {
        let mut state = lock(&self.state);
        match source {
            AuthSource::Typed => state.auth = None,
            AuthSource::Stored(account) => state.refused_account = Some(account.clone()),
            AuthSource::Nothing => {}
        }
    }

    /// Shows the sign-in form and blocks until the person answers or the
    /// session closes (`false`).
    fn ask_credentials(
        &self,
        retry: bool,
        needs_username: bool,
        message: String,
        reconnecting: bool,
    ) -> bool {
        {
            let mut state = lock(&self.state);
            if over(&state) {
                return false;
            }
            let username = self
                .target
                .username
                .clone()
                .or_else(|| state.typed_username.clone());
            state.request = Some(CredentialRequest {
                username,
                needs_username,
                retry,
            });
            state.phase = SessionPhase::Credentials;
            state.message = Some(message);
        }
        self.changed.notify_all();
        self.log_line(if retry {
            "the server refused the password; waiting for the user to sign in in the preview pane"
        } else {
            "the server asks for a password; waiting for the user to sign in in the preview pane"
        });
        self.notify_change();
        {
            let mut state = lock(&self.state);
            loop {
                if over(&state) {
                    return false;
                }
                if let Some(answer) = state.answer.take() {
                    state.auth = Some(answer);
                    break;
                }
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            state.request = None;
            state.message = None;
            state.phase = if reconnecting {
                SessionPhase::Reconnecting
            } else {
                SessionPhase::Connecting
            };
        }
        self.changed.notify_all();
        self.log_line("signing in with the password from the preview pane");
        self.notify_change();
        true
    }

    /// The person submitted the sign-in form.
    fn answer(&self, credentials: ViewerCredentials) {
        let ViewerCredentials {
            username,
            password,
            remember,
        } = credentials;
        let username = username
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        {
            let mut state = lock(&self.state);
            if state.closed || state.phase != SessionPhase::Credentials {
                return;
            }
            let needs_username = state
                .request
                .as_ref()
                .is_some_and(|request| request.needs_username);
            let problem = if username
                .as_deref()
                .is_some_and(|name| !target::is_plausible_username(name))
            {
                Some("The account name is not valid: at most 63 bytes and no control characters.")
            } else if password.len() > MAX_PASSWORD_BYTES {
                Some("The password is too long.")
            } else if needs_username && username.is_none() && self.target.username.is_none() {
                Some("This server needs an account name as well as the password.")
            } else {
                None
            };
            if let Some(problem) = problem {
                state.message = Some(problem.to_owned());
                return;
            }
            if username.is_some() {
                state.typed_username = username.clone();
            }
            state.answer = Some(Answer {
                username,
                password,
                remember,
            });
        }
        self.changed.notify_all();
    }

    /// A connect succeeded: make it the live connection. `false` when the
    /// session closed or failed meanwhile (the caller closes the driver).
    fn install(&self, driver: &Arc<dyn Driver>, connection: u64) -> bool {
        let info = driver.info();
        {
            let mut state = lock(&self.state);
            if over(&state) {
                return false;
            }
            state.live = Some(Live {
                driver: driver.clone(),
                connection,
            });
            state.phase = SessionPhase::Ready;
            state.message = None;
            state.request = None;
            state.failure = None;
            state.info = Some(info.clone());
        }
        self.frames.set_pulling(info.platform.is_touch());
        self.frames.mark_dirty();
        self.changed.notify_all();
        let mut line = format!("connected to {}", self.describe_target());
        if !info.label.trim().is_empty() {
            let _ = write!(line, " ({})", info.label);
        }
        if info.width > 0 && info.height > 0 {
            let _ = write!(line, ", screen {}x{}", info.width, info.height);
        }
        self.log_line(&line);
        self.notify_change();
        true
    }

    /// After a successful sign-in: remembers the password when asked to, and
    /// forgets a stored one the server had refused when the person chose not
    /// to remember the new one.
    fn signed_in(&self, source: &AuthSource) {
        if !matches!(source, AuthSource::Typed) {
            return;
        }
        let (save, stale) = {
            let mut state = lock(&self.state);
            let username = self.target.username.clone();
            let stale = state.refused_account.take();
            match state.auth.as_mut() {
                Some(answer) if answer.remember => {
                    // Saved once; a reconnect does not write it again.
                    answer.remember = false;
                    let account = credentials::account(
                        self.connector.place_key(),
                        self.target.port,
                        answer.username.as_deref().or(username.as_deref()),
                    );
                    (Some((account, answer.password.clone())), stale)
                }
                _ => (None, stale),
            }
        };
        match save {
            Some((account, password)) => match self.store.set(&account, &password) {
                Ok(()) => self.log_line("saved the password in the system credential store"),
                Err(error) => self.log_line(&error),
            },
            None => {
                if let Some(account) = stale {
                    if let Err(error) = self.store.delete(&account) {
                        self.log_line(&error);
                    }
                }
            }
        }
    }

    /// Blocks while connection `connection` is live; why it ended, or `None`
    /// when the session closed or failed. The ended driver is closed and dropped.
    fn wait_lost(&self, connection: u64) -> Option<String> {
        let (reason, live) = {
            let mut state = lock(&self.state);
            loop {
                if over(&state) {
                    return None;
                }
                if let Some((lost, _)) = &state.lost {
                    if *lost == connection {
                        break;
                    }
                }
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let reason = state
                .lost
                .take()
                .map(|(_, reason)| reason)
                .unwrap_or_default();
            let live = state.live.take_if(|live| live.connection == connection);
            (reason, live)
        };
        self.frames.set_pulling(false);
        if let Some(live) = live {
            live.driver.close();
        }
        Some(reason)
    }

    /// Waits up to the ready wait, and never past the call's limit, for a
    /// usable connection.
    fn ready_within(&self, budget: &Budget) -> Result<Live, String> {
        let deadline = (Instant::now() + self.timing.ready_wait).min(budget.limit);
        let mut state = lock(&self.state);
        loop {
            // A session ended as failed ([`RemoteControlSession::end`]) is closed too; why it
            // failed is the answer, not that it was stopped.
            if state.closed && state.phase != SessionPhase::Failed {
                return Err(
                    "This remote-control session was stopped; run preview_start again.".to_owned(),
                );
            }
            match state.phase {
                SessionPhase::Ready => {
                    if let Some(live) = state.live.clone() {
                        return Ok(live);
                    }
                }
                SessionPhase::Failed => {
                    return Err(format!(
                        "The remote-control session \"{}\" failed: {} Run preview_start again to retry; {}.",
                        one_line(&self.name, MAX_LABEL_CHARS),
                        sentence(state.failure.as_deref().unwrap_or("the connection failed")),
                        self.logs_hint("its connection log")
                    ));
                }
                _ => {}
            }
            if (budget.cancelled)() {
                return Err(
                    "Cancelled while waiting for the remote screen to be ready.".to_owned(),
                );
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, (deadline - now).min(WAIT_SLICE))
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
        Err(match state.phase {
            SessionPhase::Credentials => "The remote screen is waiting for the user to sign in in the preview pane. Ask them to enter the password there, then try again.".to_owned(),
            SessionPhase::Reconnecting => format!(
                "The remote screen lost its connection ({}) and is reconnecting; try again in a few seconds. {}.",
                state.message.as_deref().map_or_else(|| "no reason given".to_owned(), |message| one_line(message, MAX_LOG_CHARS)),
                self.logs_hint("the progress")
            ),
            _ => format!(
                "The remote screen is still connecting; try again in a few seconds. {}.",
                self.logs_hint("the progress")
            ),
        })
    }

    fn refuse_read_only(&self) -> Result<(), String> {
        if self.target.read_only {
            // A desktop has no element tree, so preview_snapshot is not offered there.
            let looking = match self.target.protocol.platform() {
                Platform::Desktop => "preview_screenshot, preview_zoom, preview_wait",
                Platform::Android | Platform::Ios => {
                    "preview_screenshot, preview_snapshot, preview_zoom, preview_wait"
                }
            };
            return Err(format!(
                "\"{}\" is read-only (\"readOnly\": true in .mewrk/launch.json): input and changes to the device are refused. Looking still works: {looking} and the screen tools that only read.",
                one_line(&self.name, MAX_LABEL_CHARS)
            ));
        }
        Ok(())
    }

    /// Takes the input lock for the model, waiting for another batch to end —
    /// up to the input lock wait, never past the call's limit.
    fn lock_input(&self, budget: &Budget) -> Result<MutexGuard<'_, ()>, String> {
        let deadline = (Instant::now() + self.timing.input_lock_wait).min(budget.limit);
        loop {
            match self.input.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if (budget.cancelled)() {
                return Err(
                    "Cancelled while waiting for another input on this screen to finish."
                        .to_owned(),
                );
            }
            if Instant::now() >= deadline {
                return Err("Another input batch is still running on this screen; try again when it finishes.".to_owned());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The screen, for the model's input: the input lock, and the connection as
    /// it is once the lock is held — the one the batch waited behind may have
    /// outlived a reconnect, and its driver with it. Whatever the person's pages
    /// hold down comes up first, so the model's typing does not land on a held
    /// Ctrl and its first click does not end the person's drag at its point.
    fn take_screen(&self, budget: &Budget) -> Result<(InputGuard<'_>, Live), String> {
        // Answer a failed or stopped session before queueing for the lock.
        self.ready_within(budget)?;
        let held = self.lock_input(budget)?;
        let live = self.ready_within(budget)?;
        self.let_go_of_pages(&live);
        // Only now: a page that sees this knows its holds were let go of.
        self.model_active.store(true, Ordering::Release);
        Ok((
            InputGuard {
                _held: held,
                active: &self.model_active,
            },
            live,
        ))
    }

    /// Lets go of every key and button the viewer's pages hold, and forgets
    /// their unfinished touches. Called with the input lock held.
    fn let_go_of_pages(&self, live: &Live) {
        let held: Vec<Held> = lock(&self.pages)
            .values_mut()
            .filter_map(PageInput::let_go)
            .collect();
        // A touch screen holds nothing: a touch is sent whole when it lifts, and
        // its modifiers never reach the device.
        if live.driver.info().platform.is_touch() {
            return;
        }
        for page in held {
            if let Err(error) = release_held(live.driver.as_ref(), page) {
                self.note_lost(live, &error);
            }
        }
    }

    fn touched(&self) {
        lock(&self.state).last_input = Some(Instant::now());
    }

    /// A touch screen still animating from the last input is given its
    /// moment before it is looked at — what is left of the call's time at most.
    fn settle(&self, live: &Live, budget: &Budget) {
        if !live.driver.info().platform.is_touch() {
            return;
        }
        let since = lock(&self.state).last_input.map(|at| at.elapsed());
        if let Some(rest) = since.and_then(|since| self.timing.touch_settle.checked_sub(since)) {
            thread::sleep(rest.min(budget.remaining()));
        }
    }

    /// What a `preview_input` batch is refused for before its first action, asked with the input
    /// lock held and the connection as it now is: a `type` via "paste" on a screen that is not
    /// a desktop whose clipboard takes the text, and `mod` on a screen that cannot say what it
    /// means. (The lengths of a `type`'s text are held at parse, [`parse_actions`], which needs
    /// no screen.) A refusal names the action, and nothing has been sent. Otherwise the batch
    /// comes back with `mod` written as the key this screen's system uses for its shortcuts.
    fn check_batch<'a>(
        &self,
        live: &Live,
        actions: &'a [InputAction],
    ) -> Result<Cow<'a, [InputAction]>, String> {
        let driver = live.driver.as_ref();
        let platform = driver.info().platform;
        let mut primary = None;
        for (index, action) in actions.iter().enumerate() {
            let refuse =
                |message: &str| format!("Action {} ({}): {message}", index + 1, action.name());
            if let InputAction::Type {
                text,
                via: TypeVia::Paste { .. },
            } = action
            {
                if platform.is_touch() {
                    return Err(refuse(&paste_needs_a_desktop(platform)));
                }
                let Some(desktop) = driver.desktop() else {
                    return Err(refuse(
                        "via \"paste\" needs a desktop connected over VNC; this screen is not one.",
                    ));
                };
                if let Some(why) = desktop::paste_refusal(desktop, text) {
                    return Err(refuse(&why));
                }
            }
            if primary.is_none() && action.uses_mod() {
                primary = Some(mod_key(driver, platform).map_err(|why| refuse(&why))?);
            }
        }
        Ok(match primary {
            Some(primary) => Cow::Owned(
                actions
                    .iter()
                    .map(|action| action.with_mod_as(primary))
                    .collect(),
            ),
            None => Cow::Borrowed(actions),
        })
    }

    /// What the model looks at again after the screen changed: a screenshot,
    /// or on a screen with an element tree either one — a model that cannot
    /// see images has only the snapshot.
    fn look_again(&self, live: &Live) -> &'static str {
        if live.driver.info().has_ui_tree {
            "preview_screenshot or preview_snapshot"
        } else {
            "preview_screenshot"
        }
    }

    /// The screen's size in pixels, from the driver.
    fn screen_size(&self, live: &Live) -> Result<(u32, u32), String> {
        let info = live.driver.info();
        if info.width > 0 && info.height > 0 {
            return Ok((info.width, info.height));
        }
        let picture = live
            .driver
            .frame(false)
            .map_err(|error| self.failed_call(live, error, "Could not read the screen's size"))?;
        if picture.width == 0 || picture.height == 0 {
            return Err(
                "The screen's size is not known yet; take a preview_screenshot first.".to_owned(),
            );
        }
        Ok((picture.width, picture.height))
    }

    /// The screen's size as it is now. A pulling driver (adb, WebDriverAgent)
    /// knows its size only from its last capture, and nothing captures while no
    /// viewer watches — so a phone turned by the model's own tap would keep its
    /// old size, every box and uid would be measured on it, and nothing the
    /// model could call would notice. A capture older than the last input is
    /// therefore not trusted: a fresh one is taken. One taken after the input
    /// but before a turn finished is, and only the tree shows it
    /// ([`RemoteControlSession::snapshot_within`] measures again then). A
    /// pushing driver (RFB) is told of every resize.
    fn current_screen(&self, live: &Live, budget: &Budget) -> Result<(u32, u32), String> {
        let info = live.driver.info();
        if info.platform.is_touch() {
            let last_input = lock(&self.state).last_input;
            let stale = match live.driver.frame(false) {
                Ok(last) => last_input.is_some_and(|input| last.captured_at < input),
                Err(_) => true,
            };
            if stale || info.width == 0 || info.height == 0 {
                budget.proceed("measuring the screen")?;
                return self.measure(live);
            }
        }
        self.screen_size(live)
    }

    /// The screen's size from a fresh capture, with everything else the driver
    /// learns of the screen's layout learned again ([`Driver::remeasure`]).
    fn measure(&self, live: &Live) -> Result<(u32, u32), String> {
        let picture = live
            .driver
            .remeasure()
            .map_err(|error| self.failed_call(live, error, "Could not read the screen's size"))?;
        if picture.width > 0 && picture.height > 0 {
            self.note_size(picture.width, picture.height);
            return Ok((picture.width, picture.height));
        }
        self.screen_size(live)
    }

    /// The screen and the coordinate frame input is read in. Refuses when the
    /// screen changed size since the model last looked: its coordinates (and
    /// uids) are for a picture that no longer exists.
    fn check_frame(
        &self,
        live: &Live,
        budget: &Budget,
    ) -> Result<((u32, u32), (u32, u32)), String> {
        let screen = self.current_screen(live, budget)?;
        let current = frame::model_frame(screen.0, screen.1);
        let look = self.look_again(live);
        let mut state = lock(&self.state);
        match state.last_shown {
            Some(shown) if shown != current => {
                state.uids.clear();
                Err(format!(
                    "The screen changed size: it is {}x{} pixels now (coordinate frame {}x{}), but the last {look} was {}x{}. Take a new {look} first.",
                    screen.0, screen.1, current.0, current.1, shown.0, shown.1
                ))
            }
            _ => Ok((screen, current)),
        }
    }

    /// Between the actions of a batch: whether the screen kept its size.
    fn same_size(&self, live: &Live, screen: (u32, u32)) -> Result<(), String> {
        let info = live.driver.info();
        if info.width == 0 || info.height == 0 || (info.width, info.height) == screen {
            return Ok(());
        }
        lock(&self.state).uids.clear();
        Err(format!(
            "the screen changed size to {}x{} pixels; take a new {}",
            info.width,
            info.height,
            self.look_again(live)
        ))
    }

    /// The model was just shown a picture in coordinate frame `frame`.
    fn shown(&self, frame: (u32, u32)) {
        let mut state = lock(&self.state);
        if state.last_shown != Some(frame) {
            state.uids.clear();
        }
        state.last_shown = Some(frame);
    }

    /// A captured frame's size, which may say the screen turned before an event did.
    fn note_size(&self, width: u32, height: u32) {
        let changed = {
            let mut state = lock(&self.state);
            match state.info.as_mut() {
                Some(info) if (info.width, info.height) != (width, height) => {
                    info.width = width;
                    info.height = height;
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.notify_change();
        }
    }

    fn uid_point(
        &self,
        uid: u64,
        screen: (u32, u32),
        live: &Live,
    ) -> Result<(UidTarget, (u32, u32)), String> {
        let (target, measured_on) = {
            let state = lock(&self.state);
            (state.uids.get(&uid).cloned(), state.uids_screen)
        };
        let target = target.ok_or_else(|| {
            format!("uid {uid} is not in the latest preview_snapshot of this screen; take a new preview_snapshot.")
        })?;
        // A screen that kept its coordinate frame can still have changed size
        // (a desktop resized to the same proportions): the centres are stale.
        if measured_on != screen {
            lock(&self.state).uids.clear();
            return Err(format!(
                "The screen changed size since the last preview_snapshot ({}x{} pixels now); take a new preview_snapshot.",
                screen.0, screen.1
            ));
        }
        let Some((x, y)) = target.center else {
            return Err(format!(
                "[{uid}] {} has no area on the screen to click; use preview_input at a point read off a {}.",
                describe_uid(&target),
                if live.driver.info().has_ui_tree {
                    "preview_screenshot, or off the @x,y box of an element near it in preview_snapshot"
                } else {
                    "preview_screenshot"
                }
            ));
        };
        // Off the screen, or the screen turned after the snapshot measured it:
        // either one measures it again.
        if x >= screen.0 || y >= screen.1 {
            return Err(format!(
                "[{uid}] {} is off the screen ({}x{} pixels); scroll it into view, or let a turning screen finish, then take a new {}.",
                describe_uid(&target),
                screen.0,
                screen.1,
                self.look_again(live)
            ));
        }
        Ok((target, (x, y)))
    }

    fn gesture<'a>(
        &self,
        live: &'a Live,
        screen: (u32, u32),
        frame: (u32, u32),
        budget: Budget<'a>,
    ) -> Gesture<'a> {
        Gesture::new(
            live.driver.as_ref(),
            screen,
            frame,
            self.timing.click_gap,
            budget,
        )
    }

    /// Types `value` into the field just clicked, once the focus has settled. `lead` is how
    /// the answer begins ("Clicked [3] EditText"); when typing stops short, the error says how
    /// much went in, and why.
    fn type_into_field(
        &self,
        live: &Live,
        gesture: &Gesture<'_>,
        lead: &str,
        value: &str,
    ) -> Result<(), String> {
        let total = value.chars().count();
        let typed = gesture
            .hold(self.timing.fill_focus)
            .and_then(|()| gesture.type_text(value));
        self.touched();
        let Err(error) = typed else {
            return Ok(());
        };
        self.note_lost(live, &error);
        let done = gesture.typed.take().map_or(0, |(done, _)| done);
        Err(match gesture.halted.take() {
            Some(halt) => format!(
                "{lead} and typed {done} of {total} characters, then the call was {}; the rest was not typed.",
                match halt {
                    Halt::Cancelled => "cancelled",
                    Halt::OutOfTime => "out of time",
                }
            ),
            None if done > 0 => format!(
                "{lead} and typed {done} of {total} characters, then typing failed: {error}"
            ),
            None => format!("{lead}, but typing failed: {error}"),
        })
    }

    /// `preview_fill` on an Android field: its text is replaced, as a page's field is. It is
    /// done only when everything about the field is known, and every check comes before the
    /// first deletion — a refusal there leaves the field as it was, and says so:
    ///
    /// * the version is Android 13 or later: `input keycombination` injects its keys without
    ///   the Ctrl state before that, so an app may not take the chord for select-all;
    /// * the time left covers clearing the field *and* typing into it, which would otherwise
    ///   leave a field emptied and not filled;
    /// * the field is tapped and the element tree read again: the element that has the focus
    ///   is the one the uid named (same role and box), and its text is known — not hidden (a
    ///   password field), not cut short by the reading, and a text that is only the field's
    ///   hint counts as none;
    /// * the call was not stopped during that read (a stopped call deletes nothing).
    ///
    /// Then, if there is text, select-all (`CTRL_LEFT A`) and delete (`DEL`), a third read to
    /// confirm the field is empty, and only then the typing. `input keycombination` reports
    /// success whether or not the app took the chord, which is why the confirmation reads the
    /// tree rather than trusting the command.
    ///
    /// Stock `uiautomator` dumps carry no `hint` attribute: an empty field shows its hint as
    /// its text. So the confirming read is told by what it shows: nothing, or a text that is
    /// not the one that was there, means the field is empty (and what it shows is its hint);
    /// the same text again may be the hint of a field that was empty all along or the keys an
    /// app ignored, and the dump cannot say which, so nothing is typed over it. (A text that
    /// is part of the old one is a deletion that stopped half way: refused as well.)
    fn replace_field(
        &self,
        live: &Live,
        android: &dyn AndroidControl,
        fill: Fill<'_>,
        budget: Budget<'_>,
    ) -> Result<String, String> {
        let Fill {
            uid,
            target,
            point,
            value,
            screen,
            frame,
        } = fill;
        let described = describe_uid(target);
        let total = value.chars().count();
        let estimates = self.timing.replace;
        let sdk = android.sdk().map_err(|error| {
            self.failed_call(live, error, "Could not read the device's Android version")
        })?;
        if sdk < REPLACE_MIN_SDK {
            return Err(format!(
                "Nothing was clicked, deleted or typed: replacing a field's text needs Android 13 or later here (this device is API {sdk}; older versions ignore the select-all chord's Ctrl). The field was left as it was; tap it with preview_click and add to its text with preview_input type instead."
            ));
        }
        let typing = estimates.android_typing(total);
        let needed = estimates.tap + estimates.read * 2 + estimates.keys + typing;
        if !budget.fits(needed) {
            return Err(format!(
                "Nothing was clicked, deleted or typed: replacing the field's text takes about {} s (tapping it, reading the screen twice, clearing it and typing {}) and this call has {} s left. The field was left as it was; type less, or try again.",
                needed.as_secs_f64().ceil(),
                desktop::characters(total),
                budget.remaining().as_secs_f64().floor()
            ));
        }
        budget.proceed("clicking the field; nothing was clicked, deleted or typed")?;
        let gesture = self.gesture(live, screen, frame, budget);
        let clicked = gesture.click_at(point, MouseButton::Left, 1);
        self.touched();
        clicked.map_err(|error| {
            self.failed_call(live, error, "The click that focuses the field failed")
        })?;
        let stopped = |halt: Halt| {
            format!(
                "Clicked [{uid}] {described}, then the call was {}; its text was left as it was and nothing was typed.",
                halt.word()
            )
        };
        let refused = |why: &str| {
            format!(
                "Clicked [{uid}] {described}, but {why}; its text was left as it was and nothing was typed."
            )
        };
        gesture
            .hold(self.timing.fill_focus)
            .map_err(|_| stopped(gesture.halted.take().unwrap_or(Halt::OutOfTime)))?;
        let before = self
            .read_field(live, target, &budget)
            .map_err(|why| refused(&why))?;
        let mut removed = None;
        if let Some(old) = before {
            if !budget.fits(estimates.keys + estimates.read + typing) {
                return Err(refused(
                    "there is not enough of this call's time left to clear it and type",
                ));
            }
            // The read above is the slow part: a call stopped during it deletes nothing.
            if let Some(halt) = budget.halt() {
                return Err(stopped(halt));
            }
            let control = Key::Named(NamedKey::Control);
            let selected = live.driver.chord(&KeyChord {
                modifiers: vec![control],
                key: Key::Char('a'),
            });
            self.touched();
            if let Err(error) = selected {
                self.note_lost(live, &error);
                return Err(refused(&format!("selecting its text failed: {error}")));
            }
            let deleted = live.driver.key(&Key::Named(NamedKey::Backspace), None);
            self.touched();
            if let Err(error) = deleted {
                self.note_lost(live, &error);
                return Err(format!(
                    "Clicked [{uid}] {described} and selected its text, but deleting it failed: {error}; nothing was typed. Its old text may still be there."
                ));
            }
            // What the device did with the keys is read, not assumed.
            if gesture.hold(self.timing.fill_focus).is_err() {
                return Err(format!(
                    "Clicked [{uid}] {described} and deleted its text, then the call was {}; nothing was typed, and it was not checked that the field is empty.",
                    gesture.halted.take().map_or("out of time", Halt::word)
                ));
            }
            match self.read_field(live, target, &budget) {
                // Nothing left, or only the hint attribute the dump does have.
                Ok(None) => {}
                // The same text again: the keys did nothing, or the field was empty and this is
                // its hint. Nothing in the dump tells which.
                Ok(Some(rest)) if rest == old => {
                    return Err(format!(
                        "Clicked [{uid}] {described} and selected its text and pressed delete, but it still shows {}: that may be its hint (the field is empty) or the app took no notice of the keys, and the screen's element tree cannot tell which. Nothing was typed. To fill a field that is empty, tap it with preview_click and type with preview_input.",
                        quoted_text(&rest)
                    ))
                }
                // Part of the old text: the deletion stopped half way.
                Ok(Some(rest)) if old.contains(rest.as_str()) => {
                    return Err(format!(
                        "Clicked [{uid}] {described} and selected its text and pressed delete, but it still holds {}, part of its old text: the app took only some of the keys. Nothing was typed.",
                        quoted_text(&rest)
                    ))
                }
                // Another text: the field is empty and shows its hint as its text.
                Ok(Some(_)) => {}
                Err(why) => {
                    return Err(format!(
                        "Clicked [{uid}] {described} and deleted its text, but {why}, so it is not known to be empty. Nothing was typed."
                    ))
                }
            }
            removed = Some(old);
        }
        let lead = match &removed {
            Some(old) => format!(
                "Clicked [{uid}] {described}, removed its old text {}",
                quoted_text(old)
            ),
            None => format!("Clicked [{uid}] {described}"),
        };
        if !value.is_empty() {
            self.type_into_field(live, &gesture, &lead, value)?;
        }
        Ok(match (removed.is_some(), total) {
            (true, 0) => format!("{lead}; typed nothing."),
            (true, _) => format!("{lead} and typed {total} characters."),
            (false, 0) => format!("{lead}; the field was already empty."),
            (false, _) => {
                format!("{lead} and typed {total} characters; the field was already empty.")
            }
        })
    }

    /// The screen's element tree, read for a fill within what is left of the call: refused, in
    /// words that continue "but …" or follow a colon, when the call was stopped or is out of
    /// time, when less is left than a read is expected to take (the read would run on past the
    /// call), or when the screen has none or the read failed. The read itself is bounded by what
    /// is left ([`Driver::ui_tree_within`]). `of` is what the tree is read for ("it", "the
    /// field").
    fn read_fill_tree(
        &self,
        live: &Live,
        budget: &Budget<'_>,
        of: &str,
    ) -> Result<UiNode, String> {
        match budget.halt() {
            Some(Halt::Cancelled) => return Err("the call was cancelled".to_owned()),
            Some(Halt::OutOfTime) => return Err("the call ran out of time".to_owned()),
            None => {}
        }
        if !budget.fits(self.timing.replace.read) {
            return Err("the call has too little time left to read the screen again".to_owned());
        }
        match live
            .driver
            .ui_tree_within(ui_tree_timeout(budget.remaining()))
        {
            Ok(Some(tree)) => Ok(tree),
            Ok(None) => Err(format!(
                "this screen has no element tree to read {of} from"
            )),
            Err(error) => {
                self.note_lost(live, &error);
                Err(format!(
                    "reading the screen's element tree failed: {}",
                    error.message().trim_end().trim_end_matches('.')
                ))
            }
        }
    }

    /// The text of the field `target` names, read from the screen as it is now: `None` when it
    /// is empty or shows only its hint. Refused, in words that continue "but …", when the
    /// element that has the focus is not that field (not the same role and box: the tap did
    /// not focus it, or the screen moved), or its text is not all there to be replaced — a
    /// password field's is hidden, a long one is cut short by the reading.
    fn read_field(
        &self,
        live: &Live,
        target: &UidTarget,
        budget: &Budget<'_>,
    ) -> Result<Option<String>, String> {
        let tree = self.read_fill_tree(live, budget, "it")?;
        let field = focused_field(&tree, target)?;
        if field.password {
            return Err(
                "it is a password field, whose text cannot be read, so it cannot be replaced"
                    .to_owned(),
            );
        }
        if field.truncated {
            return Err(
                "its text is longer than the reading of the screen keeps, so it cannot be replaced without losing what is not shown"
                    .to_owned(),
            );
        }
        let text = field.text.as_deref().unwrap_or("");
        // A field with nothing in it may show its hint as its text.
        let empty = text.is_empty() || field.hint.as_deref() == Some(text);
        Ok((!empty).then(|| text.to_owned()))
    }

    /// `preview_fill` on an iPhone or iPad field: its text is replaced, as a page's field is. As
    /// on Android ([`Self::replace_field`]) it is done only when everything about the field is
    /// known, and every check comes before the first change — a refusal there leaves the field as
    /// it was, and says so:
    ///
    /// * the element is a text field, a text view or a search field: not a password field, whose
    ///   text is hidden and is the person's to enter, and not anything that holds no text;
    /// * the time left covers reading the screen twice, clearing the field, tapping it and
    ///   typing (WebDriverAgent types about [`REPLACE_IOS_CHARS_PER_SECOND`] characters a
    ///   second), which would otherwise leave a field emptied and not filled;
    /// * the element tree is read again and the element the uid named — the same role and box;
    ///   it need not have the focus, which WebDriverAgent seldom reports — has its text all
    ///   known (not cut short by the reading, and not changed by it: a text the reading trimmed
    ///   or cleaned of control characters is not the device's own, and finding the field by it
    ///   could find another), a value that is only its placeholder counting as none.
    ///
    /// Then, if there is text, [`ios::clear_field`] finds the field by its role, text and label
    /// (exactly one element must match) and clears it with WebDriverAgent's own clear; the time
    /// left is checked again, for the second read, the tap and the typing; a second read confirms
    /// the field is empty; and only then it is tapped, which gives it the keyboard, and typed
    /// into. WebDriverAgent answers its clear whether or not the app took it, which is why the
    /// confirmation reads the tree.
    fn replace_ios_field(
        &self,
        live: &Live,
        fill: Fill<'_>,
        budget: Budget<'_>,
    ) -> Result<String, String> {
        let Fill {
            uid,
            target,
            point,
            value,
            screen,
            frame,
        } = fill;
        let described = describe_uid(target);
        let total = value.chars().count();
        let estimates = self.timing.replace;
        let untouched = |why: &str| {
            format!(
                "Nothing was clicked, cleared or typed on [{uid}] {described}: {why}. The field was left as it was."
            )
        };
        match target.role.as_str() {
            "TextField" | "TextView" | "SearchField" => {}
            "SecureTextField" => {
                return Err(untouched(
                    "it is a password field, whose text is hidden and is the user's to enter, so it is not replaced; ask the user to fill it",
                ))
            }
            _ => {
                return Err(untouched(
                    "it is not a text field, text view or search field, so it holds no text to replace; tap it with preview_click, or type into the field that has the focus with preview_input",
                ))
            }
        }
        let typing = estimates.ios_typing(total);
        let needed = estimates.read * 2 + estimates.ios_clear + estimates.tap + typing;
        if !budget.fits(needed) {
            return Err(untouched(&format!(
                "replacing its text takes about {} s (reading the screen twice, clearing the field, tapping it and typing {}) and this call has {} s left; type less, or try again",
                needed.as_secs_f64().ceil(),
                desktop::characters(total),
                budget.remaining().as_secs_f64().floor()
            )));
        }
        let before = self
            .read_ios_field(live, target, &budget)
            .map_err(|why| untouched(&why))?;
        let gesture = self.gesture(live, screen, frame, budget);
        let mut removed = None;
        if let Some(old) = before.text {
            if !budget.fits(estimates.ios_clear + estimates.read + estimates.tap + typing) {
                return Err(untouched(
                    "there is not enough of this call's time left to clear it, check it and type",
                ));
            }
            // Every `Said` of a refused `clear_field` is a refusal before the clear was asked
            // for; only a driver's own error leaves it unknown what the device did.
            let cleared = ios::clear_field(
                live.driver.as_ref(),
                &Step::of(budget),
                &target.role,
                &old,
                before.label.as_deref(),
            );
            self.touched();
            match cleared {
                Ok(()) => {}
                Err(OpError::Said(said)) => {
                    return Err(format!(
                        "Could not clear [{uid}] {described}: {} Nothing was typed, and the field was left as it was.",
                        sentence(&said)
                    ))
                }
                Err(OpError::Driver(error)) => {
                    self.note_lost(live, &error);
                    return Err(format!(
                        "Clearing [{uid}] {described} failed: {error}; nothing was typed. Its old text may still be there."
                    ));
                }
            }
            // The clear took what it took: the second read, the tap and the typing must still
            // fit in what is left, or the field would be left empty and not filled.
            if !budget.fits(estimates.read + estimates.tap + typing) {
                return Err(format!(
                    "Asked WebDriverAgent to clear [{uid}] {described} (its old text was {}), then this call had too little time left to read it again, tap it and type {}: the field is empty now unless the app took no notice of the clear, and nothing was typed. Fill it again.",
                    quoted_text(&old),
                    desktop::characters(total)
                ));
            }
            // What the device did with the clear is read, not assumed.
            if gesture.hold(self.timing.fill_focus).is_err() {
                return Err(format!(
                    "Asked WebDriverAgent to clear [{uid}] {described}, then the call was {}; it was not checked that the field is empty, and nothing was typed.",
                    gesture.halted.take().map_or("out of time", Halt::word)
                ));
            }
            match self.read_ios_field(live, target, &budget) {
                Ok(IosField { text: None, .. }) => {}
                Ok(IosField {
                    text: Some(rest), ..
                }) => {
                    return Err(format!(
                        "Asked WebDriverAgent to clear [{uid}] {described}, but it still holds {}: its old text may still be there. Nothing was typed.",
                        quoted_text(&rest)
                    ))
                }
                Err(why) => {
                    return Err(format!(
                        "Asked WebDriverAgent to clear [{uid}] {described}, but {why}, so it is not known to be empty and its old text may still be there. Nothing was typed."
                    ))
                }
            }
            removed = Some(format!(
                "Removed the old text {} from [{uid}] {described}",
                quoted_text(&old)
            ));
        }
        match (budget.halt(), &removed) {
            (None, _) => {}
            (Some(halt), Some(removed)) => {
                return Err(format!(
                    "{removed}, then the call was {}; it was not clicked and nothing was typed.",
                    halt.word()
                ))
            }
            (Some(_), None) => {
                budget.proceed("clicking the field; nothing was clicked, cleared or typed")?
            }
        }
        let clicked = gesture.click_at(point, MouseButton::Left, 1);
        self.touched();
        if let Err(error) = clicked {
            let Some(removed) = &removed else {
                return Err(self.failed_call(
                    live,
                    error,
                    "The click that focuses the field failed",
                ));
            };
            self.note_lost(live, &error);
            return Err(format!(
                "{removed}, but the click that focuses the field failed: {error}; nothing was typed."
            ));
        }
        let lead = match &removed {
            Some(removed) => format!("{removed}, then clicked it"),
            None => format!("Clicked [{uid}] {described}"),
        };
        if !value.is_empty() {
            self.type_into_field(live, &gesture, &lead, value)?;
        }
        let typed = desktop::characters(total);
        Ok(match (removed.is_some(), total) {
            (true, 0) => format!("{lead}; typed nothing."),
            (true, _) => format!("{lead} and typed {typed}."),
            (false, 0) => format!("{lead}; the field was already empty."),
            (false, _) => format!("{lead} and typed {typed}; the field was already empty."),
        })
    }

    /// The text of the iPhone or iPad field `target` names, read from the screen as it is now,
    /// with the label to find it by: the text is `None` when the field is empty or shows only
    /// its placeholder (WebDriverAgent gives an empty field's placeholder as its value). Refused,
    /// as a clause that follows a colon, when the element cannot be found by its role and box
    /// (it need not have the focus), or its text is not all there to be replaced — a password
    /// field's is hidden, a long one is cut short by the reading, and one the reading changed
    /// (trimmed, or its control characters made spaces) is not the device's own, so a search by
    /// it could find another field.
    fn read_ios_field(
        &self,
        live: &Live,
        target: &UidTarget,
        budget: &Budget<'_>,
    ) -> Result<IosField, String> {
        let tree = self.read_fill_tree(live, budget, "the field")?;
        let field = element_named(&tree, target)?;
        if field.password {
            return Err(
                "the field is a password field, whose text cannot be read, so it cannot be replaced"
                    .to_owned(),
            );
        }
        if field.truncated {
            return Err(
                "the field's text is longer than the reading of the screen keeps, so it cannot be replaced without losing what is not shown"
                    .to_owned(),
            );
        }
        if field.inexact {
            return Err(
                "its text has characters (or spacing) the reading of the screen cannot keep exactly, so it cannot be matched safely"
                    .to_owned(),
            );
        }
        let text = field.value.as_deref().unwrap_or("");
        // An empty field shows its placeholder as its value.
        let empty = text.is_empty() || field.hint.as_deref() == Some(text);
        Ok(IosField {
            text: (!empty).then(|| text.to_owned()),
            label: (!field.name.is_empty()).then(|| field.name.clone()),
        })
    }

    fn live(&self) -> Option<Live> {
        let state = lock(&self.state);
        if state.phase == SessionPhase::Ready {
            state.live.clone()
        } else {
            None
        }
    }

    /// A captured frame for the viewers in this process, unless they already have it.
    fn publish_live_frame(&self, live: &Live, picture: &super::driver::Frame) {
        if self.frames.live_wants(live.connection, picture.seq) || self.frames.take_live_wants_full() {
            self.note_size(picture.width, picture.height);
            self.frames.publish_live(&ScreenUpdate::Full { width: picture.width, height: picture.height, rgba: picture.rgba.clone() });
        }
    }

    /// Hands a captured frame to whoever is watching: pixels to a viewer in this process, and a
    /// JPEG, unless it already has it, to a page.
    fn publish(&self, live: &Live, picture: &super::driver::Frame) {
        if self.frames.live_watching() {
            self.publish_live_frame(live, picture);
            if !self.frames.page_watching() {
                return;
            }
        }
        if !self.frames.wants(live.connection, picture.seq) {
            return;
        }
        self.note_size(picture.width, picture.height);
        match frame::encode_for_viewer(picture) {
            Ok((jpeg, width, height)) => {
                self.frames
                    .publish(live.connection, picture.seq, jpeg, width, height);
                *lock(&self.last_frame_error) = None;
            }
            Err(error) => self.frame_error(error),
        }
    }

    /// Logs a frame failure once, not once per frame.
    fn frame_error(&self, error: String) {
        let mut last = lock(&self.last_frame_error);
        if last.as_deref() != Some(error.as_str()) {
            *last = Some(error.clone());
            drop(last);
            self.log_line(&format!("could not update the preview: {error}"));
        }
    }

    /// The input lock for a page's batch, or `None` when the batch is to be
    /// dropped: the model's batch has the screen (and already let go of what
    /// this page held; the page says so from `model_active`), or another
    /// page's batch did not end in time, or the session closed.
    fn lock_for_page(&self, patience: Duration) -> Option<MutexGuard<'_, ()>> {
        let deadline = Instant::now() + patience;
        loop {
            match self.input.try_lock() {
                Ok(guard) => return Some(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            if self.model_active.load(Ordering::Acquire)
                || Instant::now() >= deadline
                || self.is_closed()
            {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn viewer_input(&self, page: &str, events: Vec<ViewerEvent>) {
        if self.target.read_only || events.is_empty() {
            return;
        }
        let Some(_held) = self.lock_for_page(VIEWER_LOCK_WAIT) else {
            return;
        };
        // The connection as it is now that this page has the screen.
        let Some(live) = self.live() else {
            return;
        };
        let info = live.driver.info();
        let screen = (info.width, info.height);
        if screen.0 == 0 || screen.1 == 0 {
            return;
        }
        // Out of the map while the driver works, so closing the session (which
        // clears it) never waits for a gesture; the input lock keeps everything
        // else that touches a page's state out meanwhile.
        let mut input = {
            let mut pages = lock(&self.pages);
            match pages.remove(page) {
                Some(input) => input,
                None if pages.len() >= MAX_PAGES => return,
                None => PageInput::default(),
            }
        };
        for event in events {
            let result = if info.platform.is_touch() {
                touch_event(live.driver.as_ref(), info.platform, &mut input, screen, event)
            } else {
                desktop_event(live.driver.as_ref(), info.platform, &mut input, screen, event)
            };
            if let Err(error) = result {
                self.note_lost(&live, &error);
                self.viewer_error(&error);
                if matches!(error, DriverError::Disconnected(_)) {
                    break;
                }
            }
        }
        if !self.is_closed() {
            lock(&self.pages).insert(page.to_owned(), input);
        }
        self.touched();
    }

    fn viewer_error(&self, error: &DriverError) {
        let message = error.to_string();
        let mut last = lock(&self.last_viewer_error);
        let quiet = last.as_ref().is_some_and(|(previous, at)| {
            *previous == message && at.elapsed() < VIEWER_ERROR_QUIET
        });
        if !quiet {
            *last = Some((message.clone(), Instant::now()));
            drop(last);
            self.log_line(&format!("input from the preview pane failed: {message}"));
        }
    }

    /// A page went away: whatever it held down comes up — under the input
    /// lock, so the release never lands in the middle of the model's drag. When
    /// the model's batch has the screen, it let go of this page's holds as it
    /// took it, and there is nothing left to send.
    fn release_page(&self, page: &str) {
        let held = self.lock_for_page(RELEASE_LOCK_WAIT);
        let Some(mut input) = lock(&self.pages).remove(page) else {
            return;
        };
        let (Some(_held), Some(live)) = (held, self.live()) else {
            return;
        };
        if live.driver.info().platform.is_touch() {
            return;
        }
        if let Some(page) = input.let_go() {
            if let Err(error) = release_held(live.driver.as_ref(), page) {
                self.note_lost(&live, &error);
            }
        }
    }
}

/// Whether a session will never connect again: closed, or failed.
fn over(state: &State) -> bool {
    state.closed || state.phase == SessionPhase::Failed
}

/// Records the failure. Nothing reconnects after it, so the password the
/// person typed for reconnects goes now, not when the failed row is finally
/// stopped — which may be never.
fn mark_failed(state: &mut State, reason: &str) {
    state.phase = SessionPhase::Failed;
    state.failure = Some(reason.to_owned());
    state.message = Some(reason.to_owned());
    state.request = None;
    state.lost = None;
    state.auth = None;
    state.answer = None;
    state.uids.clear();
}

/// The model's hold on the input lock; the viewer shows `model_active` while
/// it lasts.
struct InputGuard<'a> {
    _held: MutexGuard<'a, ()>,
    active: &'a AtomicBool,
}

impl Drop for InputGuard<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

impl ViewerBackend for Inner {
    fn state(&self) -> ViewerState {
        let state = lock(&self.state);
        let info = state.info.as_ref();
        ViewerState {
            name: self.name.clone(),
            protocol: self.target.protocol,
            platform: info.map_or(self.target.protocol.platform(), |info| info.platform),
            // The page is the person's, in the app's language.
            endpoint: format!("{} · {}", self.connector.display_label(), self.target.port),
            label: info
                .map(|info| info.label.clone())
                .filter(|label| !label.trim().is_empty()),
            phase: state.phase,
            message: state.message.clone(),
            screen: info
                .filter(|info| info.width > 0 && info.height > 0)
                .map(|info| ScreenSize {
                    width: info.width,
                    height: info.height,
                }),
            read_only: self.target.read_only,
            credentials: if state.phase == SessionPhase::Credentials {
                state.request.clone()
            } else {
                None
            },
            model_active: self.model_active.load(Ordering::Acquire),
        }
    }

    fn next_frame(&self, after: u64, timeout: Duration) -> Option<ViewerFrame> {
        self.frames.next_frame(after, timeout)
    }

    fn latest_frame(&self) -> Option<ViewerFrame> {
        self.frames.latest()
    }

    fn input(&self, viewer: &str, events: Vec<ViewerEvent>) {
        self.viewer_input(viewer, events);
    }

    fn credentials(&self, credentials: ViewerCredentials) {
        self.answer(credentials);
    }

    fn set_viewer_visible(&self, viewer: &str, visible: bool) {
        self.frames.set_visible(viewer, visible);
    }

    fn viewer_gone(&self, viewer: &str) {
        self.frames.viewer_gone(viewer);
    }

    fn release_viewer(&self, viewer: &str) {
        self.release_page(viewer);
    }
}

/// The supervisor: connects, signs in, waits for the connection to end, and
/// reconnects with backoff until it gives up or the session closes.
fn supervise(inner: &Arc<Inner>) {
    // Why the last live connection ended, while reconnecting.
    let mut lost: Option<String> = None;
    let mut attempts = 0usize;
    let mut wait = Duration::ZERO;
    inner.log_line(&format!("connecting to {}", inner.describe_target()));
    loop {
        if !wait.is_zero() && !inner.pause(wait) {
            return;
        }
        wait = Duration::ZERO;
        if inner.is_over() {
            return;
        }
        if let Some(preflight) = &inner.preflight {
            if let Err(message) = preflight() {
                inner.fail(message);
                return;
            }
        }
        let (auth, source) = if inner.target.protocol == Protocol::Vnc {
            let (auth, source) = inner.vnc_auth();
            (Some(auth), source)
        } else {
            (None, AuthSource::Nothing)
        };
        let connection = {
            let mut state = lock(&inner.state);
            state.connection += 1;
            state.lost = None;
            state.connection
        };
        let events = inner.events(connection);
        let result = (inner.connect)(&inner.target, inner.dialler.clone(), auth, events);
        match result {
            Ok(driver) => {
                if !inner.install(&driver, connection) {
                    driver.close();
                    return;
                }
                inner.signed_in(&source);
                attempts = 0;
                let Some(reason) = inner.wait_lost(connection) else {
                    return;
                };
                lost = Some(reason);
                match reconnect_delay(inner, &mut attempts, lost.as_deref(), None) {
                    Some(delay) => wait = delay,
                    None => return,
                }
            }
            Err(DriverError::Credentials {
                retry,
                needs_username,
                message,
            }) => {
                inner.refused(&source);
                let retry = retry || !matches!(source, AuthSource::Nothing);
                if !inner.ask_credentials(retry, needs_username, message, lost.is_some()) {
                    return;
                }
            }
            Err(error) => {
                if lost.is_none() {
                    inner.fail(error.message().to_owned());
                    return;
                }
                inner.log_line(&format!("reconnect attempt {attempts} failed: {error}"));
                match reconnect_delay(inner, &mut attempts, lost.as_deref(), Some(error.message()))
                {
                    Some(delay) => wait = delay,
                    None => return,
                }
            }
        }
    }
}

/// The wait before the next reconnect attempt, entering `Reconnecting`; or
/// `None` after the last attempt, having failed the session.
fn reconnect_delay(
    inner: &Inner,
    attempts: &mut usize,
    lost: Option<&str>,
    last_error: Option<&str>,
) -> Option<Duration> {
    let reason = lost.unwrap_or("the connection ended");
    let Some(&delay) = inner.timing.backoff.get(*attempts) else {
        let mut failure = format!(
            "Lost the connection ({reason}) and could not reconnect after {} attempts",
            inner.timing.backoff.len()
        );
        if let Some(error) = last_error {
            let _ = write!(failure, "; the last one said: {error}");
        }
        failure.push('.');
        inner.fail(failure);
        return None;
    };
    *attempts += 1;
    inner.set_phase(
        SessionPhase::Reconnecting,
        Some(reason.to_owned()),
        &format!(
            "lost the connection ({reason}); reconnecting in {} ms (attempt {} of {})",
            delay.as_millis(),
            attempts,
            inner.timing.backoff.len()
        ),
    );
    Some(delay)
}

/// The pump: produces the frames while someone watches. A viewer in this process takes pixels
/// straight from the driver; a page takes JPEGs, encoded only for it.
fn pump_frames(inner: &Arc<Inner>) {
    let mut last_encode: Option<Instant> = None;
    // The connection whose driver has no working stream, and how many have failed in a row.
    let mut no_stream: Option<u64> = None;
    let mut stream_failures = 0_u32;
    loop {
        let Some(work) = inner.frames.wait_for_work(PUMP_IDLE) else {
            continue;
        };
        if work == Work::Closed {
            return;
        }
        let Some(live) = inner.live() else {
            // Watched, but nothing is connected: wait for a connection rather
            // than spin on the flag.
            if !inner.frames.pause(inner.timing.pull_pause) {
                return;
            }
            continue;
        };
        match work {
            Work::Pull => {
                if inner.frames.live_watching() && no_stream != Some(live.connection) {
                    let began = Instant::now();
                    let stop = || !inner.frames.live_watching() || inner.live().is_none_or(|now| now.connection != live.connection);
                    let result = live.driver.stream(&stop, &mut |update| inner.frames.publish_live(&update));
                    match result {
                        Ok(()) => stream_failures = 0,
                        Err(DriverError::Unsupported(_)) => no_stream = Some(live.connection),
                        Err(error) => {
                            // One that ran for a while and ended is worth starting again; one that
                            // cannot get going leaves the picture to the captures below.
                            stream_failures = if began.elapsed() < STREAM_PROVEN { stream_failures + 1 } else { 1 };
                            inner.log_line(&format!("the picture stream stopped: {error}"));
                            if stream_failures >= STREAM_ATTEMPTS {
                                inner.log_line("going on with one capture at a time");
                                no_stream = Some(live.connection);
                            }
                        }
                    }
                    if no_stream != Some(live.connection) {
                        if !inner.frames.pause(STREAM_RETRY) {
                            return;
                        }
                        continue;
                    }
                }
                match live.driver.frame(true) {
                    Ok(picture) => inner.publish(&live, &picture),
                    Err(error) => {
                        inner.note_lost(&live, &error);
                        inner.frame_error(error.to_string());
                    }
                }
                // A capture is its own pace limit; the pause is what a page's JPEGs ask for.
                if !inner.frames.pause(if inner.frames.live_watching() { LIVE_PULL_PAUSE.min(inner.timing.pull_pause) } else { inner.timing.pull_pause }) {
                    return;
                }
            }
            Work::Encode => {
                if inner.frames.live_watching() {
                    let update = if inner.frames.take_live_wants_full() { live.driver.take_full() } else { live.driver.take_patch() };
                    match update {
                        Some(update) => inner.frames.publish_live(&update),
                        // A driver that tracks nothing: the whole picture, if it is a new one.
                        None => {
                            if let Ok(picture) = live.driver.frame(false) {
                                inner.publish_live_frame(&live, &picture);
                            }
                        }
                    }
                    if !inner.frames.page_watching() {
                        continue;
                    }
                    // A page's JPEGs never hold a viewer's pixels back: the frame after next will do.
                    if last_encode.is_some_and(|at| at.elapsed() < inner.timing.encode_interval) {
                        continue;
                    }
                } else if let Some(rest) = last_encode
                    .and_then(|at| inner.timing.encode_interval.checked_sub(at.elapsed()))
                {
                    if !inner.frames.pause(rest) {
                        return;
                    }
                }
                last_encode = Some(Instant::now());
                match live.driver.frame(false) {
                    Ok(picture) => inner.publish(&live, &picture),
                    Err(error) => {
                        inner.note_lost(&live, &error);
                        inner.frame_error(error.to_string());
                    }
                }
            }
            Work::Closed => return,
        }
    }
}

/// The model's gestures, on one driver, in one coordinate frame, inside one
/// call's budget.
struct Gesture<'a> {
    driver: &'a dyn Driver,
    platform: Platform,
    screen: (u32, u32),
    frame: (u32, u32),
    click_gap: Duration,
    budget: Budget<'a>,
    /// Why the last gesture stopped before it was done, when it did.
    halted: Cell<Option<Halt>>,
    /// How much of the last `type` was typed — characters done and in all —
    /// when it did not all go.
    typed: Cell<Option<(usize, usize)>>,
    /// What the last gesture says of how it went, where "ok" would leave out something the
    /// model needs to know: a paste's shortcut and what the clipboard holds now.
    note: Cell<Option<String>>,
}

impl<'a> Gesture<'a> {
    fn new(
        driver: &'a dyn Driver,
        screen: (u32, u32),
        frame: (u32, u32),
        click_gap: Duration,
        budget: Budget<'a>,
    ) -> Self {
        Self {
            driver,
            platform: driver.info().platform,
            screen,
            frame,
            click_gap,
            budget,
            halted: Cell::new(None),
            typed: Cell::new(None),
            note: Cell::new(None),
        }
    }

    /// A pixel of the coordinate frame on the screen. A point past the
    /// picture's edge is a mistake worth saying, not a click to clamp.
    fn point(&self, x: u32, y: u32) -> DriverResult<(u32, u32)> {
        if x > self.frame.0 || y > self.frame.1 {
            return Err(DriverError::Failed(format!(
                "({x}, {y}) is outside the {}x{} coordinate frame",
                self.frame.0, self.frame.1
            )));
        }
        Ok(frame::frame_to_screen((x, y), self.frame, self.screen))
    }

    /// Remembers why the gesture stopped, and says it.
    fn stopped(&self, halt: Halt) -> DriverError {
        self.halted.set(Some(halt));
        DriverError::Failed(halt.word().to_owned())
    }

    /// Sleeps `duration` in short steps; stops early when the call is cancelled
    /// or runs out of time.
    fn hold(&self, duration: Duration) -> DriverResult<()> {
        let deadline = Instant::now() + duration;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            if let Some(halt) = self.budget.halt() {
                return Err(self.stopped(halt));
            }
            thread::sleep((deadline - now).min(WAIT_SLICE));
        }
    }

    /// Types `text`; long text in pieces of [`TEXT_PIECE_CHARS`], stopping
    /// between them when the call is cancelled or out of time. What the driver
    /// cannot type at all is refused before the first piece.
    fn type_text(&self, text: &str) -> DriverResult<()> {
        let pieces = text_pieces(text, TEXT_PIECE_CHARS);
        if pieces.len() <= 1 {
            return self.driver.text(text);
        }
        self.driver.check_text(text)?;
        let total = text.chars().count();
        let mut done = 0;
        for piece in pieces {
            if let Some(halt) = self.budget.halt() {
                self.typed.set(Some((done, total)));
                return Err(self.stopped(halt));
            }
            if let Err(error) = self.driver.text(piece) {
                self.typed.set(Some((done, total)));
                return Err(error);
            }
            done += piece.chars().count();
        }
        Ok(())
    }

    /// `type` with `via` "paste": the text onto the desktop's clipboard, a pause, the shortcut
    /// ([`desktop::paste`]), all inside this batch's lock and time. Said in the batch's line.
    fn paste_text(&self, text: &str, chord: Option<&PasteChord>) -> DriverResult<()> {
        let desktop = self.driver.desktop().ok_or_else(|| {
            DriverError::Unsupported("this screen is not a desktop connected over VNC".to_owned())
        })?;
        match desktop::paste(self.driver, desktop, &Step::of(self.budget), text, chord) {
            Ok(shown) => {
                self.note.set(Some(format!(
                    "pasted {} with {shown} (the desktop's clipboard now holds them)",
                    desktop::characters(text.chars().count())
                )));
                Ok(())
            }
            Err(OpError::Driver(error)) => Err(error),
            Err(OpError::Said(message)) => Err(DriverError::Failed(message)),
        }
    }

    fn perform(&self, action: &InputAction) -> DriverResult<()> {
        self.halted.set(None);
        self.typed.set(None);
        self.note.set(None);
        let modifiers = action.modifiers();
        if modifiers.is_empty() {
            return self.perform_plain(action);
        }
        if self.platform.is_touch() {
            return Err(DriverError::Unsupported(
                MODIFIERS_NEED_A_DESKTOP.trim_end_matches('.').to_owned(),
            ));
        }
        // A point off the frame is refused before any key goes down.
        if let Some((x, y)) = action.point() {
            self.point(x, y)?;
        }
        if let InputAction::Drag { to_x, to_y, .. } = action {
            self.point(*to_x, *to_y)?;
        }
        self.holding(modifiers, || self.perform_plain(action))
    }

    /// Runs `act` with `modifiers` held: each goes down in order before it and
    /// comes up in reverse after — also when `act` fails or is cut short, and
    /// when a later one fails to go down (those already down come up).
    fn holding(
        &self,
        modifiers: &[Key],
        act: impl FnOnce() -> DriverResult<()>,
    ) -> DriverResult<()> {
        let mut down = Vec::with_capacity(modifiers.len());
        let mut result = Ok(());
        for modifier in modifiers {
            if let Err(error) = self.driver.key(modifier, Some(true)) {
                result = Err(error);
                break;
            }
            down.push(*modifier);
        }
        if result.is_ok() {
            result = act();
        }
        for modifier in down.iter().rev() {
            let released = self.driver.key(modifier, Some(false));
            if result.is_ok() {
                result = released;
            }
        }
        result
    }

    fn perform_plain(&self, action: &InputAction) -> DriverResult<()> {
        let touch = self.platform.is_touch();
        match action {
            &InputAction::Click {
                x,
                y,
                button,
                count,
                ..
            } => {
                let point = self.point(x, y)?;
                self.click_at(point, button, count)
            }
            &InputAction::LongPress { x, y, ms, .. } => {
                let (x, y) = self.point(x, y)?;
                if touch {
                    return self.driver.long_press(x, y, ms);
                }
                self.driver.pointer(x, y, 0)?;
                self.driver.pointer(x, y, buttons::LEFT)?;
                let held = self.hold(Duration::from_millis(u64::from(ms)));
                // The button comes up whether or not the hold was cut short.
                self.driver.pointer(x, y, 0)?;
                held
            }
            &InputAction::Drag {
                x,
                y,
                to_x,
                to_y,
                ms,
                ..
            } => {
                let from = self.point(x, y)?;
                let to = self.point(to_x, to_y)?;
                if touch {
                    return self.driver.swipe(from, to, ms);
                }
                self.desktop_drag(from, to, ms)
            }
            &InputAction::Scroll { x, y, dx, dy, .. } => {
                let point = self.point(x, y)?;
                if touch {
                    self.touch_scroll(point, dx, dy)
                } else {
                    desktop_scroll(self.driver, point, 0, dx, dy)
                }
            }
            &InputAction::Move { x, y } => {
                let (x, y) = self.point(x, y)?;
                if touch {
                    return Err(DriverError::Unsupported(
                        "a touch screen has no pointer to hover; move is for desktops".to_owned(),
                    ));
                }
                self.driver.pointer(x, y, 0)
            }
            InputAction::Type {
                text,
                via: TypeVia::Auto,
            } => self.type_text(text),
            InputAction::Type {
                text,
                via: TypeVia::Paste { chord },
            } => self.paste_text(text, chord.as_ref()),
            InputAction::Key { key, repeat } => {
                let chord = keys::parse_chord(key, self.platform).map_err(DriverError::Failed)?;
                for index in 0..*repeat {
                    if index > 0 {
                        if let Some(halt) = self.budget.halt() {
                            return Err(self.stopped(halt));
                        }
                    }
                    if chord.modifiers.is_empty() {
                        self.driver.key(&chord.key, None)?;
                    } else {
                        self.driver.chord(&chord)?;
                    }
                }
                Ok(())
            }
            &InputAction::Wait { ms } => self.hold(Duration::from_millis(u64::from(ms))),
        }
    }

    /// A click at a screen pixel: taps on a touch screen; on a desktop, the
    /// pointer moves there, then each click presses and releases the button.
    fn click_at(&self, (x, y): (u32, u32), button: MouseButton, count: u8) -> DriverResult<()> {
        if self.platform.is_touch() {
            if button != MouseButton::Left {
                return Err(DriverError::Unsupported(
                    "a touch screen has no right or middle button; use long_press for a context menu"
                        .to_owned(),
                ));
            }
            return self.driver.tap(x, y, count);
        }
        let mask = button.mask();
        self.driver.pointer(x, y, 0)?;
        for index in 0..count {
            if index > 0 {
                thread::sleep(self.click_gap);
            }
            self.driver.pointer(x, y, mask)?;
            self.driver.pointer(x, y, 0)?;
        }
        Ok(())
    }

    /// Press, [`DRAG_STEPS`] moves spread over `ms`, release at the end. The
    /// button comes up even when cancelled halfway.
    fn desktop_drag(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> DriverResult<()> {
        self.driver.pointer(from.0, from.1, 0)?;
        self.driver.pointer(from.0, from.1, buttons::LEFT)?;
        let step = Duration::from_millis(u64::from(ms / DRAG_STEPS));
        let mut at = from;
        for index in 1..=DRAG_STEPS {
            if let Err(error) = self.hold(step) {
                self.driver.pointer(at.0, at.1, 0)?;
                return Err(error);
            }
            at = lerp(from, to, f64::from(index) / f64::from(DRAG_STEPS));
            if let Err(error) = self.driver.pointer(at.0, at.1, buttons::LEFT) {
                let _ = self.driver.pointer(at.0, at.1, 0);
                return Err(error);
            }
        }
        self.driver.pointer(to.0, to.1, 0)
    }

    /// Scrolling a touch screen is swiping the other way: to see what is
    /// below, the finger moves up. One notch is an eighth of the screen.
    fn touch_scroll(&self, from: (u32, u32), dx: i32, dy: i32) -> DriverResult<()> {
        let axis = |start: u32, notches: i32, size: u32| -> u32 {
            let travel = f64::from(notches) * f64::from(size) / 8.0;
            (f64::from(start) - travel)
                .round()
                .clamp(0.0, f64::from(size.saturating_sub(1))) as u32
        };
        let no_room = || {
            let (x, y) = frame::screen_to_frame(
                (i64::from(from.0), i64::from(from.1)),
                self.screen,
                self.frame,
            );
            DriverError::Failed(format!(
                "there is no room to swipe from ({x}, {y}) toward that edge; start the scroll nearer the middle of what should move"
            ))
        };
        if dy != 0 {
            let to = (from.0, axis(from.1, dy, self.screen.1));
            if to.1 == from.1 {
                return Err(no_room());
            }
            self.driver.swipe(from, to, SCROLL_SWIPE_MS)?;
        }
        if dx != 0 {
            if let Some(halt) = self.budget.halt() {
                return Err(self.stopped(halt));
            }
            let to = (axis(from.0, dx, self.screen.0), from.1);
            if to.0 == from.0 {
                return Err(no_room());
            }
            self.driver.swipe(from, to, SCROLL_SWIPE_MS)?;
        }
        Ok(())
    }
}

/// `text` cut into pieces of at most `size` characters, never between a
/// carriage return and the line feed after it (one Enter, not two).
fn text_pieces(text: &str, size: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut count = 0;
    let mut previous = None;
    for (at, character) in text.char_indices() {
        if count >= size && !(previous == Some('\r') && character == '\n') {
            pieces.push(&text[start..at]);
            start = at;
            count = 0;
        }
        count += 1;
        previous = Some(character);
    }
    if start < text.len() {
        pieces.push(&text[start..]);
    }
    pieces
}

fn lerp(from: (u32, u32), to: (u32, u32), share: f64) -> (u32, u32) {
    let axis =
        |a: u32, b: u32| (f64::from(a) + (f64::from(b) - f64::from(a)) * share).round() as u32;
    (axis(from.0, to.0), axis(from.1, to.1))
}

/// Wheel notches on a desktop: each is the wheel's button pressed and
/// released, with whatever buttons are `held` kept down.
fn desktop_scroll(
    driver: &dyn Driver,
    (x, y): (u32, u32),
    held: u8,
    dx: i32,
    dy: i32,
) -> DriverResult<()> {
    let notches = [
        (dy, buttons::WHEEL_DOWN, buttons::WHEEL_UP),
        (dx, buttons::WHEEL_RIGHT, buttons::WHEEL_LEFT),
    ];
    for (count, positive, negative) in notches {
        let wheel = if count > 0 { positive } else { negative };
        for _ in 0..count.unsigned_abs() {
            driver.pointer(x, y, held | wheel)?;
            driver.pointer(x, y, held)?;
        }
    }
    Ok(())
}

/// DOM `PointerEvent.buttons` (1 left, 2 right, 4 middle) as an RFB mask
/// (1 left, 2 middle, 4 right). Back and forward have no RFB bit.
fn dom_buttons_to_rfb(dom: u8) -> u8 {
    let mut mask = 0;
    if dom & 1 != 0 {
        mask |= buttons::LEFT;
    }
    if dom & 2 != 0 {
        mask |= buttons::RIGHT;
    }
    if dom & 4 != 0 {
        mask |= buttons::MIDDLE;
    }
    mask
}

/// A point of the viewer's picture, as fractions, on the screen.
fn fraction_point((x, y): (f64, f64), (width, height): (u32, u32)) -> (u32, u32) {
    let axis = |value: f64, size: u32| {
        if !value.is_finite() || size == 0 {
            return 0;
        }
        ((value.clamp(0.0, 1.0) * f64::from(size)).floor() as u32).min(size - 1)
    };
    (axis(x, width), axis(y, height))
}

fn within_slop(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < TOUCH_SLOP && (a.1 - b.1).abs() < TOUCH_SLOP
}

/// What a touch on the viewer was, once it lifted: a tap when it stayed put
/// briefly, a long press when it stayed put longer, else a swipe from where it
/// went down to where it came up, as fast as the person moved.
fn touch_gesture(
    start: (f64, f64),
    end: (f64, f64),
    moved: bool,
    held: Duration,
    screen: (u32, u32),
) -> TouchGesture {
    let from = fraction_point(start, screen);
    let held_ms = u32::try_from(held.as_millis()).unwrap_or(u32::MAX);
    if !moved && within_slop(start, end) {
        if held < TOUCH_LONG_PRESS {
            return TouchGesture::Tap(from);
        }
        return TouchGesture::LongPress(from, held_ms.min(TOUCH_LONG_PRESS_MAX_MS));
    }
    TouchGesture::Swipe(
        from,
        fraction_point(end, screen),
        held_ms.clamp(TOUCH_SWIPE_MIN_MS, TOUCH_SWIPE_MAX_MS),
    )
}

/// Pixels a tree's outer edge may stand past the screen before the screen is
/// taken to have turned: WebDriverAgent rounds points to pixels. A turn moves
/// the edge by hundreds.
const TREE_EDGE_SLACK: i64 = 2;

/// Whether the tree's outermost elements — the root, or the windows a root
/// without bounds holds — reach past a screen of `screen` pixels. Inner
/// elements may (a list scrolled past the bottom); a window cannot.
fn reaches_past(tree: &UiNode, screen: (u32, u32)) -> bool {
    let past = |bounds: &Rect| {
        i64::from(bounds.x) + i64::from(bounds.width) > i64::from(screen.0) + TREE_EDGE_SLACK
            || i64::from(bounds.y) + i64::from(bounds.height)
                > i64::from(screen.1) + TREE_EDGE_SLACK
    };
    match &tree.bounds {
        Some(bounds) => past(bounds),
        None => tree
            .children
            .iter()
            .filter_map(|child| child.bounds.as_ref())
            .any(past),
    }
}

/// A key name from the viewer's toolbar, or a chord.
fn press_named(driver: &dyn Driver, name: &str, platform: Platform) -> DriverResult<()> {
    let chord = keys::parse_chord(name, platform).map_err(DriverError::Failed)?;
    if chord.modifiers.is_empty() {
        driver.key(&chord.key, None)
    } else {
        driver.chord(&chord)
    }
}

/// Accumulates wheel deltas into whole notches, which it returns.
fn whole_notches(accumulated: &mut f64, delta: f64) -> i32 {
    if delta.is_finite() {
        *accumulated += delta.clamp(-MAX_WHEEL_NOTCHES, MAX_WHEEL_NOTCHES);
    }
    *accumulated = accumulated.clamp(-MAX_WHEEL_NOTCHES, MAX_WHEEL_NOTCHES);
    let whole = accumulated.trunc();
    *accumulated -= whole;
    whole as i32
}

/// A key from the viewer on a touch screen. A phone has no keyboard to hold
/// keys on: a character is typed as text (adb's key events have no capitals
/// and no punctuation, and the page's own text events never come for keys it
/// sends), modifiers are only remembered, and a key pressed while Ctrl, Alt or
/// Cmd is held goes as a combination — unless the layout composed it
/// ([`composed_by_layout`]), when the Ctrl and Alt are the layout's and it is
/// typed. Other keys are whole presses.
fn touch_key(
    driver: &dyn Driver,
    input: &mut PageInput,
    key: &str,
    code: String,
    down: bool,
    alt_graph: bool,
) -> DriverResult<()> {
    let slot = if code.is_empty() { key.to_owned() } else { code };
    // AltGr itself is remembered as the Alt it stands in for, so a page that
    // does not send `altGraph` still has its characters typed. Only here: a
    // desktop is sent the keys as they are.
    let pressed = if key == "AltGraph" {
        Some(Key::Named(NamedKey::Alt))
    } else {
        keys::from_dom_key(key)
    };
    let Some(pressed) = pressed else {
        return Ok(());
    };
    if pressed.is_modifier() {
        if !down {
            input.keys.remove(&slot);
        } else if input.keys.len() < MAX_HELD_KEYS || input.keys.contains_key(&slot) {
            input.keys.insert(slot, pressed);
        }
        return Ok(());
    }
    if !down {
        return Ok(());
    }
    let mut modifiers: Vec<Key> = input.keys.values().copied().collect();
    modifiers.sort_by_key(|modifier| match modifier {
        Key::Named(NamedKey::Control) => 0,
        Key::Named(NamedKey::Alt) => 1,
        Key::Named(NamedKey::Shift) => 2,
        _ => 3,
    });
    modifiers.dedup();
    if composed_by_layout(pressed, &modifiers, alt_graph) {
        modifiers
            .retain(|modifier| !matches!(modifier, Key::Named(NamedKey::Control | NamedKey::Alt)));
    }
    let shortcut = modifiers
        .iter()
        .any(|modifier| *modifier != Key::Named(NamedKey::Shift));
    if shortcut {
        // Shift is in the chord; the key is the one under it.
        let key = match pressed {
            Key::Char(character) => Key::Char(character.to_ascii_lowercase()),
            named => named,
        };
        return driver.chord(&KeyChord { modifiers, key });
    }
    match pressed {
        // Shift already made it the character it is.
        Key::Char(character) => driver.text(&character.to_string()),
        named => driver.key(&named, None),
    }
}

/// Whether the Control and Alt held with `pressed` are the keyboard layout's
/// way to a character rather than a shortcut. AltGr (`alt_graph`, which the
/// page reads off the key-down) is exactly that; Windows reports it as a
/// Control press of its own besides. A page that does not say is judged by
/// the key: Alt held (AltGr's own press counts as one) without Cmd, and a
/// character that is no ASCII letter or digit (`@`, `€`, `{`, `ą`), is what
/// AltGr — or a Mac's Option, which
/// composes the same way (`@` is Option+L on a German Mac) — gives, while
/// Ctrl+Alt+T is still a shortcut. A phone can type the one and not chord the
/// other: adb has no key code for `@`, and WebDriverAgent no chords at all.
fn composed_by_layout(pressed: Key, modifiers: &[Key], alt_graph: bool) -> bool {
    if alt_graph {
        return true;
    }
    let Key::Char(character) = pressed else {
        return false;
    };
    !character.is_ascii_alphanumeric()
        && modifiers.contains(&Key::Named(NamedKey::Alt))
        && !modifiers.contains(&Key::Named(NamedKey::Meta))
}

fn text_event(driver: &dyn Driver, text: &str) -> DriverResult<()> {
    if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return Ok(());
    }
    driver.text(text)
}

/// One viewer event on a desktop: the pointer and keys go through as they
/// are, edge by edge.
fn desktop_event(
    driver: &dyn Driver,
    platform: Platform,
    input: &mut PageInput,
    screen: (u32, u32),
    event: ViewerEvent,
) -> DriverResult<()> {
    match event {
        ViewerEvent::Pointer {
            phase,
            x,
            y,
            buttons: held,
            ..
        } => {
            let (x, y) = fraction_point((x, y), screen);
            let mask = if phase == PointerPhase::Cancel {
                0
            } else {
                dom_buttons_to_rfb(held)
            };
            input.buttons = mask;
            input.position = (x, y);
            driver.pointer(x, y, mask)
        }
        ViewerEvent::Wheel { x, y, dx, dy } => {
            let point = fraction_point((x, y), screen);
            input.position = point;
            let notches_y = whole_notches(&mut input.wheel.1, dy);
            let notches_x = whole_notches(&mut input.wheel.0, dx);
            desktop_scroll(driver, point, input.buttons, notches_x, notches_y)
        }
        ViewerEvent::Key {
            key, code, down, ..
        } => {
            let slot = if code.is_empty() { key.clone() } else { code };
            if down {
                let Some(pressed) = keys::from_dom_key(&key) else {
                    return Ok(());
                };
                if input.keys.len() < MAX_HELD_KEYS || input.keys.contains_key(&slot) {
                    input.keys.insert(slot, pressed);
                }
                driver.key(&pressed, Some(true))
            } else {
                match input
                    .keys
                    .remove(&slot)
                    .or_else(|| keys::from_dom_key(&key))
                {
                    Some(released) => driver.key(&released, Some(false)),
                    None => Ok(()),
                }
            }
        }
        ViewerEvent::Text { text } => text_event(driver, &text),
        ViewerEvent::Button { name } => press_named(driver, &name, platform),
    }
}

/// One viewer event on a touch screen. A touch is only known once it lifts,
/// so a gesture is synthesised then ([`touch_gesture`]), timed by the page's
/// clock when the page sends it; keys are whole presses.
fn touch_event(
    driver: &dyn Driver,
    platform: Platform,
    input: &mut PageInput,
    screen: (u32, u32),
    event: ViewerEvent,
) -> DriverResult<()> {
    match event {
        ViewerEvent::Pointer {
            phase, x, y, at, ..
        } => match phase {
            PointerPhase::Down => {
                input.touch = Some(TouchStart {
                    from: (x, y),
                    started: Instant::now(),
                    page_at: at,
                    moved: false,
                });
                Ok(())
            }
            PointerPhase::Move => {
                if let Some(touch) = input.touch.as_mut() {
                    if !within_slop(touch.from, (x, y)) {
                        touch.moved = true;
                    }
                }
                Ok(())
            }
            PointerPhase::Up => {
                let Some(touch) = input.touch.take() else {
                    return Ok(());
                };
                match touch_gesture(
                    touch.from,
                    (x, y),
                    touch.moved,
                    touch.held_until(at),
                    screen,
                ) {
                    TouchGesture::Tap((x, y)) => driver.tap(x, y, 1),
                    TouchGesture::LongPress((x, y), ms) => driver.long_press(x, y, ms),
                    TouchGesture::Swipe(from, to, ms) => driver.swipe(from, to, ms),
                }
            }
            PointerPhase::Cancel => {
                input.touch = None;
                Ok(())
            }
        },
        ViewerEvent::Wheel { x, y, dx, dy } => {
            // A mouse wheel over a phone scrolls it the way preview_input does.
            let notches_y = whole_notches(&mut input.wheel.1, dy);
            let notches_x = whole_notches(&mut input.wheel.0, dx);
            if notches_x == 0 && notches_y == 0 {
                return Ok(());
            }
            let gesture = Gesture::new(driver, screen, screen, CLICK_GAP, Budget::open());
            gesture.touch_scroll(fraction_point((x, y), screen), notches_x, notches_y)
        }
        ViewerEvent::Key {
            key,
            code,
            down,
            alt_graph,
        } => touch_key(driver, input, &key, code, down, alt_graph),
        ViewerEvent::Text { text } => text_event(driver, &text),
        ViewerEvent::Button { name } => press_named(driver, &name, platform),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    use serde_json::json;

    use super::*;
    use crate::remote_control::{
        credentials::tests::MemoryStore,
        driver::{ClipboardChannel, DesktopControl, Frame, IosControl, Rect, UiNode, WdaRoute},
        fake::{FakeAndroid, FakeDesktop},
        keys::{KeyChord, NamedKey},
        target::TargetPlace,
        transport::Connection,
    };

    /// A place that is never dialled: the fake drivers do not use it.
    struct FakeConnector;

    impl Connector for FakeConnector {
        fn connect(&self, port: u16, _timeout: Duration) -> Result<Connection, String> {
            Err(format!("nothing is listening on 127.0.0.1:{port} on lab"))
        }

        fn place_label(&self) -> &str {
            "lab"
        }

        /// What the person's page calls it, unlike what the model reads.
        fn display_label(&self) -> &str {
            "实验室"
        }

        fn place_key(&self) -> &str {
            "ssh:m1"
        }

        fn is_local(&self) -> bool {
            false
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Pointer(u32, u32, u8),
        Tap(u32, u32, u8),
        LongPress(u32, u32, u32),
        Swipe((u32, u32), (u32, u32), u32),
        Key(Key, Option<bool>),
        Chord(KeyChord),
        Text(String),
        /// A WebDriverAgent call of an iPhone that has one: `POST session:/elements`, with its body.
        Wda(String, Option<Value>),
        Close,
    }

    /// The WebDriverAgent of an iPhone that has one: `POST /elements` finds the elements of the
    /// screen that its predicate (type, value and perhaps label) matches, comparing with what the
    /// device itself holds; whether the clear it is then asked for empties that text field; and
    /// the error that clear answers instead.
    struct FakeWda {
        /// What `POST /elements` answers instead of evaluating its predicate.
        found: Mutex<Option<Value>>,
        clearing: AtomicBool,
        clear_error: Mutex<Option<DriverError>>,
        /// How long the clear takes.
        clear_takes: Mutex<Duration>,
        /// What the device holds in the element with this box where that is not what the tree
        /// says: the tree's texts are trimmed and cleaned of control characters by the parser.
        device_values: Mutex<Vec<(Rect, String)>>,
        /// The element ids handed out by the last search, with the box of the element each names.
        handed_out: Mutex<Vec<(String, Rect)>>,
    }

    /// Records every call; desktop or touch by its platform, like the real
    /// drivers.
    struct FakeDriver {
        platform: Platform,
        size: Mutex<(u32, u32)>,
        calls: Mutex<Vec<Call>>,
        /// What the screen shows; a test changes it as a turned screen would.
        tree: Mutex<Option<UiNode>>,
        tree_reads: AtomicUsize,
        seq: AtomicU64,
        captures: AtomicUsize,
        /// Like adb and WebDriverAgent: the size it reports is its last capture's,
        /// and an unfresh frame is that capture.
        pulling: bool,
        last: Mutex<Option<Frame>>,
        /// How long each touch, swipe and text call takes, as a slow device's.
        stall: Mutex<Duration>,
        /// The adb operations of a phone that has them: the version it runs.
        android: Option<FakeAndroid>,
        /// The RFB extras of a desktop that has them: its clipboard, whether it is Apple's.
        desktop: Option<FakeDesktop>,
        /// Whether Backspace empties the field that has the focus, as it does on a device that
        /// took the select-all chord before it.
        deleting: AtomicBool,
        /// Whether Backspace takes the last character of the focused field's text only, as it
        /// does on a device that did not take the select-all chord.
        backspace_takes_one: AtomicBool,
        /// What an emptied field shows as its text: its hint, which a stock `uiautomator` dump
        /// does not give as an attribute. A field that has a hint attribute shows that.
        unseen_hint: Mutex<Option<String>>,
        /// Called on every read of the element tree with the number of reads so far, after it was
        /// counted and before it answers.
        on_tree_read: Mutex<Option<Box<dyn Fn(usize) + Send + Sync>>>,
        /// How long a read of the element tree takes: a read bounded by less fails.
        tree_read_takes: Mutex<Duration>,
        /// The timeouts the bounded reads were given.
        tree_timeouts: Mutex<Vec<Duration>>,
        /// Every frame asked for, as the `fresh` it asked for.
        frame_requests: Mutex<Vec<bool>>,
        /// The WebDriverAgent of an iPhone that has one.
        wda: Option<FakeWda>,
    }

    impl FakeDriver {
        fn new(platform: Platform, width: u32, height: u32) -> Arc<Self> {
            Self::with_tree(platform, width, height, None)
        }

        fn with_tree(
            platform: Platform,
            width: u32,
            height: u32,
            tree: Option<UiNode>,
        ) -> Arc<Self> {
            Arc::new(Self::plain(platform, width, height, tree))
        }

        fn plain(platform: Platform, width: u32, height: u32, tree: Option<UiNode>) -> Self {
            Self {
                platform,
                size: Mutex::new((width, height)),
                calls: Mutex::new(Vec::new()),
                tree: Mutex::new(tree),
                tree_reads: AtomicUsize::new(0),
                seq: AtomicU64::new(0),
                captures: AtomicUsize::new(0),
                pulling: false,
                last: Mutex::new(None),
                stall: Mutex::new(Duration::ZERO),
                android: None,
                desktop: None,
                deleting: AtomicBool::new(false),
                backspace_takes_one: AtomicBool::new(false),
                unseen_hint: Mutex::new(None),
                on_tree_read: Mutex::new(None),
                tree_read_takes: Mutex::new(Duration::ZERO),
                tree_timeouts: Mutex::new(Vec::new()),
                frame_requests: Mutex::new(Vec::new()),
                wda: None,
            }
        }

        /// An iPhone whose screen shows `tree` and whose WebDriverAgent finds the elements of it
        /// that a search's predicate matches (the element ids are [`IOS_FIELD_ID`] for the first,
        /// then [`IOS_OTHER_ID`] ones); its clear empties that text field (the field then shows
        /// its placeholder, as a real one does) when `clearing`.
        fn ios_with(tree: UiNode, clearing: bool) -> Arc<Self> {
            Arc::new(Self {
                wda: Some(FakeWda {
                    found: Mutex::new(None),
                    clearing: AtomicBool::new(clearing),
                    clear_error: Mutex::new(None),
                    clear_takes: Mutex::new(Duration::ZERO),
                    device_values: Mutex::new(Vec::new()),
                    handed_out: Mutex::new(Vec::new()),
                }),
                ..Self::plain(Platform::Ios, 1170, 2532, Some(tree))
            })
        }

        /// What the WebDriverAgent's `POST /elements` answers from now on, whatever the screen
        /// shows.
        fn wda_finds(&self, found: Value) {
            *self.wda.as_ref().unwrap().found.lock().unwrap() = Some(found);
        }

        /// What its `/clear` answers from now on instead of clearing.
        fn wda_fails_to_clear(&self, error: DriverError) {
            *self.wda.as_ref().unwrap().clear_error.lock().unwrap() = Some(error);
        }

        /// How long its `/clear` takes from now on.
        fn wda_clear_takes(&self, time: Duration) {
            *self.wda.as_ref().unwrap().clear_takes.lock().unwrap() = time;
        }

        /// The device holds `value` in the element with this box, whatever the tree says of it.
        fn wda_holds(&self, bounds: Rect, value: &str) {
            self.wda
                .as_ref()
                .unwrap()
                .device_values
                .lock()
                .unwrap()
                .push((bounds, value.to_owned()));
        }

        /// What a search by `predicate` finds on the screen as it is now: the elements of the
        /// type, with the value and perhaps the label the predicate names, in the order the tree
        /// lists them. `None` for a predicate other than the ones the fill sends.
        fn wda_search(&self, predicate: &str) -> Option<Vec<(String, Rect)>> {
            fn literal(text: &str) -> Option<(String, &str)> {
                let mut characters = text.strip_prefix('\'')?.chars();
                let mut value = String::new();
                loop {
                    match characters.next()? {
                        '\\' => value.push(characters.next()?),
                        '\'' => return Some((value, characters.as_str())),
                        other => value.push(other),
                    }
                }
            }
            fn walk<'a>(node: &'a UiNode, into: &mut Vec<&'a UiNode>) {
                into.push(node);
                node.children.iter().for_each(|child| walk(child, into));
            }
            let (kind, rest) = literal(predicate.strip_prefix("type == ")?)?;
            let (value, rest) = literal(rest.strip_prefix(" AND value == ")?)?;
            let label: Option<String> = match rest.strip_prefix(" AND label == ") {
                Some(rest) => {
                    let (label, rest) = literal(rest)?;
                    if !rest.is_empty() {
                        return None;
                    }
                    Some(label)
                }
                None if rest.is_empty() => None,
                None => return None,
            };
            let wda = self.wda.as_ref()?;
            let held = wda.device_values.lock().unwrap().clone();
            let tree = self.tree.lock().unwrap().clone()?;
            let mut nodes = Vec::new();
            walk(&tree, &mut nodes);
            let found = nodes
                .into_iter()
                .filter(|node| format!("XCUIElementType{}", node.role) == kind)
                .filter(|node| {
                    let device = node
                        .bounds
                        .and_then(|bounds| {
                            held.iter()
                                .find(|(at, _)| *at == bounds)
                                .map(|(_, value)| value.clone())
                        })
                        .or_else(|| node.value.clone())
                        .unwrap_or_default();
                    device == value
                })
                .filter(|node| label.as_ref().map_or(true, |label| node.name == *label))
                .filter_map(|node| node.bounds)
                .enumerate()
                .map(|(index, bounds)| {
                    let id = if index == 0 {
                        IOS_FIELD_ID.to_owned()
                    } else {
                        format!("{IOS_OTHER_ID}{index}")
                    };
                    (id, bounds)
                })
                .collect();
            Some(found)
        }

        /// What WebDriverAgent's clear does to the text field with this box: nothing is left of
        /// its text, and it shows its placeholder as its value.
        fn clear_the_text_field(&self, bounds: Rect) {
            fn clear(node: &mut UiNode, bounds: Rect) {
                if node.bounds == Some(bounds)
                    && matches!(node.role.as_str(), "TextField" | "TextView" | "SearchField")
                {
                    node.value = node.hint.clone();
                }
                node.children.iter_mut().for_each(|child| clear(child, bounds));
            }
            if let Some(tree) = self.tree.lock().unwrap().as_mut() {
                clear(tree, bounds);
            }
        }

        /// An Android phone running API level `sdk`, whose screen shows `tree`; Backspace
        /// empties its focused field when `deleting`.
        fn android_with(sdk: u32, tree: UiNode, deleting: bool) -> Arc<Self> {
            Arc::new(Self {
                android: Some(FakeAndroid::new(sdk, |_| FakeAndroid::out(""))),
                deleting: AtomicBool::new(deleting),
                ..Self::plain(Platform::Android, 1080, 2400, Some(tree))
            })
        }

        /// An emptied field shows `hint` as its text (a stock dump has no attribute for it), or
        /// nothing.
        fn empty_fields_show(&self, hint: Option<&str>) {
            *self.unseen_hint.lock().unwrap() = hint.map(str::to_owned);
        }

        /// Backspace takes one character off the focused field, not all of its text.
        fn backspace_takes_one_character(&self) {
            self.backspace_takes_one.store(true, Ordering::SeqCst);
        }

        /// Runs `hook` with the number of reads so far at every read of the element tree.
        fn on_tree_read(&self, hook: impl Fn(usize) + Send + Sync + 'static) {
            *self.on_tree_read.lock().unwrap() = Some(Box::new(hook));
        }

        /// A read of the element tree takes `time`: one bounded by less fails.
        fn tree_read_takes(&self, time: Duration) {
            *self.tree_read_takes.lock().unwrap() = time;
        }

        fn tree_timeouts(&self) -> Vec<Duration> {
            self.tree_timeouts.lock().unwrap().clone()
        }

        fn frame_requests(&self) -> Vec<bool> {
            self.frame_requests.lock().unwrap().clone()
        }

        /// One read of the element tree, counted.
        fn read_tree(&self) -> Option<UiNode> {
            let reads = self.tree_reads.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some(hook) = self.on_tree_read.lock().unwrap().as_ref() {
                hook(reads);
            }
            self.tree.lock().unwrap().clone()
        }

        /// A desktop whose clipboard takes `channel`, Apple's server when `macos`.
        fn vnc_with(channel: ClipboardChannel, macos: bool) -> Arc<Self> {
            let desktop = FakeDesktop::new(channel);
            desktop.set_macos(macos);
            Arc::new(Self {
                desktop: Some(desktop),
                ..Self::plain(Platform::Desktop, 1920, 1080, None)
            })
        }

        /// A phone that knows its size only from its captures, captured once at
        /// connect as the real drivers do.
        fn pulling(platform: Platform, width: u32, height: u32, tree: Option<UiNode>) -> Arc<Self> {
            let driver = Arc::new(Self {
                pulling: true,
                ..Self::plain(platform, width, height, tree)
            });
            driver.capture();
            driver
        }

        fn capture(&self) -> Frame {
            self.captures.fetch_add(1, Ordering::SeqCst);
            let (width, height) = *self.size.lock().unwrap();
            let rgba: Vec<u8> = [40u8, 90, 200, 255].repeat((width * height) as usize);
            let frame = Frame {
                width,
                height,
                rgba: Arc::from(rgba),
                seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
                captured_at: Instant::now(),
            };
            *self.last.lock().unwrap() = Some(frame.clone());
            frame
        }

        fn slow_down(&self, each: Duration) {
            *self.stall.lock().unwrap() = each;
        }

        fn stalling(&self) {
            let stall = *self.stall.lock().unwrap();
            if !stall.is_zero() {
                thread::sleep(stall);
            }
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }

        fn resize(&self, width: u32, height: u32) {
            *self.size.lock().unwrap() = (width, height);
        }

        fn show(&self, tree: UiNode) {
            *self.tree.lock().unwrap() = Some(tree);
        }

        fn touch(&self) -> DriverResult<()> {
            if self.platform.is_touch() {
                Ok(())
            } else {
                Err(DriverError::Unsupported(
                    "this screen has a pointer, not touches".into(),
                ))
            }
        }
    }

    impl Driver for FakeDriver {
        fn info(&self) -> DriverInfo {
            let (width, height) = if self.pulling {
                self.last
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or((0, 0), |frame| (frame.width, frame.height))
            } else {
                *self.size.lock().unwrap()
            };
            DriverInfo {
                platform: self.platform,
                label: "Fake \"Pixel\"".into(),
                width,
                height,
                has_ui_tree: self.tree.lock().unwrap().is_some(),
            }
        }

        fn frame(&self, fresh: bool) -> DriverResult<Frame> {
            self.frame_requests.lock().unwrap().push(fresh);
            if self.pulling {
                if !fresh {
                    if let Some(frame) = self.last.lock().unwrap().clone() {
                        return Ok(frame);
                    }
                }
                return Ok(self.capture());
            }
            if fresh {
                self.captures.fetch_add(1, Ordering::SeqCst);
            }
            let (width, height) = *self.size.lock().unwrap();
            let rgba: Vec<u8> = [40u8, 90, 200, 255].repeat((width * height) as usize);
            Ok(Frame {
                width,
                height,
                rgba: Arc::from(rgba),
                seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
                captured_at: Instant::now(),
            })
        }

        fn pointer(&self, x: u32, y: u32, mask: u8) -> DriverResult<()> {
            if self.platform.is_touch() {
                return Err(DriverError::Unsupported(
                    "this screen is driven by touches, not a pointer".into(),
                ));
            }
            self.record(Call::Pointer(x, y, mask));
            Ok(())
        }

        fn tap(&self, x: u32, y: u32, count: u8) -> DriverResult<()> {
            self.touch()?;
            self.stalling();
            self.record(Call::Tap(x, y, count));
            Ok(())
        }

        fn long_press(&self, x: u32, y: u32, ms: u32) -> DriverResult<()> {
            self.touch()?;
            self.stalling();
            self.record(Call::LongPress(x, y, ms));
            Ok(())
        }

        fn swipe(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> DriverResult<()> {
            self.touch()?;
            self.stalling();
            self.record(Call::Swipe(from, to, ms));
            Ok(())
        }

        fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
            self.record(Call::Key(*key, down));
            if *key == Key::Named(NamedKey::Backspace) {
                if self.deleting.load(Ordering::SeqCst) {
                    self.empty_the_focused_field();
                } else if self.backspace_takes_one.load(Ordering::SeqCst) {
                    self.take_a_character_off_the_focused_field();
                }
            }
            Ok(())
        }

        fn chord(&self, chord: &KeyChord) -> DriverResult<()> {
            self.record(Call::Chord(chord.clone()));
            Ok(())
        }

        fn text(&self, text: &str) -> DriverResult<()> {
            if text.contains('\u{7}') {
                return Err(DriverError::Unsupported("no key types U+0007".into()));
            }
            self.stalling();
            self.record(Call::Text(text.to_owned()));
            Ok(())
        }

        fn check_text(&self, text: &str) -> DriverResult<()> {
            if text.contains('\u{7}') {
                return Err(DriverError::Unsupported("no key types U+0007".into()));
            }
            Ok(())
        }

        fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
            Ok(self.read_tree())
        }

        fn ui_tree_within(&self, timeout: Duration) -> DriverResult<Option<UiNode>> {
            self.tree_timeouts.lock().unwrap().push(timeout);
            if *self.tree_read_takes.lock().unwrap() > timeout {
                self.tree_reads.fetch_add(1, Ordering::SeqCst);
                return Err(DriverError::Failed(
                    "uiautomator did not finish reading the screen in the time this call had left."
                        .into(),
                ));
            }
            Ok(self.read_tree())
        }

        fn close(&self) {
            self.record(Call::Close);
        }

        fn android(&self) -> Option<&dyn AndroidControl> {
            self.android
                .as_ref()
                .map(|android| android as &dyn AndroidControl)
        }

        fn desktop(&self) -> Option<&dyn DesktopControl> {
            self.desktop
                .as_ref()
                .map(|desktop| desktop as &dyn DesktopControl)
        }

        fn ios(&self) -> Option<&dyn IosControl> {
            self.wda.as_ref().map(|_| self as &dyn IosControl)
        }
    }

    /// The element id the fake WebDriverAgent gives the first element a search finds.
    const IOS_FIELD_ID: &str = "5F1B0A2C-3D4E-4F50-8A61-7B8C9D0E1F2A";
    /// The start of the ids it gives the others, followed by their number in the list.
    const IOS_OTHER_ID: &str = "9A8B7C6D-5E4F-4A3B-9C2D-1E0F9A8B7C6";

    /// Answers only what filling a field asks of WebDriverAgent — `POST /elements` and an
    /// element's `/clear` — and records every call. A search is answered from the screen: the
    /// elements its predicate matches, as the device itself would compare their texts.
    impl IosControl for FakeDriver {
        fn call(
            &self,
            method: &str,
            route: WdaRoute<'_>,
            body: Option<&Value>,
            _timeout: Duration,
        ) -> DriverResult<Value> {
            let key = match route {
                WdaRoute::Session(suffix) => format!("{method} session:{suffix}"),
                WdaRoute::Root(path) => format!("{method} {path}"),
            };
            self.record(Call::Wda(key.clone(), body.cloned()));
            let Some(wda) = &self.wda else {
                return Err(DriverError::Unsupported("no WebDriverAgent here".into()));
            };
            if key == "POST session:/elements" {
                if let Some(found) = wda.found.lock().unwrap().clone() {
                    return Ok(found);
                }
                let predicate = body
                    .and_then(|body| body.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Some(found) = self.wda_search(predicate) else {
                    return Err(DriverError::Failed(format!(
                        "the fake WebDriverAgent does not know the predicate {predicate:?}"
                    )));
                };
                *wda.handed_out.lock().unwrap() = found.clone();
                return Ok(Value::Array(
                    found
                        .into_iter()
                        .map(|(id, _)| {
                            json!({"element-6066-11e4-a52e-4f735466cecf": id.clone(), "ELEMENT": id})
                        })
                        .collect(),
                ));
            }
            if let Some(id) = key
                .strip_prefix("POST session:/element/")
                .and_then(|rest| rest.strip_suffix("/clear"))
            {
                let takes = *wda.clear_takes.lock().unwrap();
                if !takes.is_zero() {
                    thread::sleep(takes);
                }
                if let Some(error) = wda.clear_error.lock().unwrap().clone() {
                    return Err(error);
                }
                if wda.clearing.load(Ordering::SeqCst) {
                    let bounds = wda
                        .handed_out
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(handed, _)| handed.as_str() == id)
                        .map(|(_, bounds)| *bounds);
                    if let Some(bounds) = bounds {
                        self.clear_the_text_field(bounds);
                    }
                }
                return Ok(Value::Null);
            }
            Err(DriverError::Unsupported(format!(
                "this WebDriverAgent build does not implement {key}."
            )))
        }
    }

    impl FakeDriver {
        /// What Backspace does on a device that took select-all: the focused field is empty, and
        /// shows its hint as its text where it has one: the dump's attribute, else the one a
        /// stock dump does not give ([`FakeDriver::empty_fields_show`]).
        fn empty_the_focused_field(&self) {
            fn empty(node: &mut UiNode, unseen: &Option<String>) {
                if node.focused {
                    node.text = node.hint.clone().or_else(|| unseen.clone());
                }
                node.children.iter_mut().for_each(|child| empty(child, unseen));
            }
            let unseen = self.unseen_hint.lock().unwrap().clone();
            if let Some(tree) = self.tree.lock().unwrap().as_mut() {
                empty(tree, &unseen);
            }
        }

        /// What Backspace does on a device that did not take select-all: the character before
        /// the cursor, the last one of the focused field's text, goes.
        fn take_a_character_off_the_focused_field(&self) {
            fn shorten(node: &mut UiNode) {
                if node.focused {
                    if let Some(text) = node.text.as_mut() {
                        text.pop();
                    }
                }
                node.children.iter_mut().for_each(shorten);
            }
            if let Some(tree) = self.tree.lock().unwrap().as_mut() {
                shorten(tree);
            }
        }
    }

    fn fast() -> Timing {
        Timing {
            backoff: vec![Duration::from_millis(10); 5],
            ready_wait: Duration::from_secs(3),
            input_lock_wait: Duration::from_secs(2),
            touch_settle: Duration::from_millis(1),
            click_gap: Duration::from_millis(1),
            pull_pause: Duration::from_millis(5),
            encode_interval: Duration::from_millis(1),
            fill_focus: Duration::from_millis(1),
            replace: ReplaceEstimates::default(),
        }
    }

    fn control(protocol: Protocol) -> ControlTarget {
        ControlTarget {
            protocol,
            place: TargetPlace::Machine("lab".into()),
            port: protocol.default_port(),
            device: None,
            username: None,
            launch: None,
            read_only: false,
        }
    }

    /// What one connect attempt is answered with, given the attempt's number
    /// and the sign-in offered.
    type Script = dyn Fn(usize, Option<&VncAuth>) -> DriverResult<Arc<FakeDriver>> + Send + Sync;

    struct Harness {
        session: Arc<RemoteControlSession>,
        logs: Arc<Mutex<Vec<String>>>,
        changes: Arc<AtomicUsize>,
        /// Each connect's events callback, in order.
        events: Arc<Mutex<Vec<DriverEvents>>>,
        auths: Arc<Mutex<Vec<VncAuth>>>,
        store: Arc<MemoryStore>,
    }

    impl Harness {
        fn backend(&self) -> &dyn ViewerBackend {
            self.session.inner.as_ref()
        }

        fn emit(&self, connect: usize, event: DriverEvent) {
            let events = self.events.lock().unwrap()[connect].clone();
            events(event);
        }

        fn logs(&self) -> String {
            self.logs.lock().unwrap().join("\n")
        }
    }

    fn start(
        target: ControlTarget,
        preflight: Option<Preflight>,
        store: Arc<MemoryStore>,
        script: impl Fn(usize, Option<&VncAuth>) -> DriverResult<Arc<FakeDriver>>
            + Send
            + Sync
            + 'static,
    ) -> Harness {
        start_timed(target, preflight, store, fast(), script)
    }

    fn start_timed(
        target: ControlTarget,
        preflight: Option<Preflight>,
        store: Arc<MemoryStore>,
        timing: Timing,
        script: impl Fn(usize, Option<&VncAuth>) -> DriverResult<Arc<FakeDriver>>
            + Send
            + Sync
            + 'static,
    ) -> Harness {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let changes = Arc::new(AtomicUsize::new(0));
        let events: Arc<Mutex<Vec<DriverEvents>>> = Arc::new(Mutex::new(Vec::new()));
        let auths = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Script> = Arc::new(script);
        let connect: ConnectFn = {
            let events = events.clone();
            let auths = auths.clone();
            Arc::new(move |_target, _connector, auth, driver_events| {
                let attempt = {
                    let mut events = events.lock().unwrap();
                    events.push(driver_events);
                    events.len() - 1
                };
                if let Some(auth) = &auth {
                    auths.lock().unwrap().push(auth.clone());
                }
                script(attempt, auth.as_ref()).map(|driver| driver as Arc<dyn Driver>)
            })
        };
        let spec = SessionSpec {
            name: "pixel".into(),
            server_id: "pixel".into(),
            target,
            connector: Arc::new(FakeConnector),
            preflight,
            on_change: {
                let changes = changes.clone();
                Arc::new(move || {
                    changes.fetch_add(1, Ordering::SeqCst);
                })
            },
            log: {
                let logs = logs.clone();
                Arc::new(move |line: &str| logs.lock().unwrap().push(line.to_owned()))
            },
            connect: Some(connect),
        };
        let session = RemoteControlSession::start_with(spec, store.clone(), timing).unwrap();
        Harness {
            session,
            logs,
            changes,
            events,
            auths,
            store,
        }
    }

    fn connected(target: ControlTarget, driver: Arc<FakeDriver>) -> Harness {
        let harness = start(target, None, Arc::default(), move |_, _| Ok(driver.clone()));
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        harness
    }

    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn actions(value: Value) -> Vec<InputAction> {
        parse_actions(&value).unwrap()
    }

    fn pointer(phase: PointerPhase, x: f64, y: f64, buttons: u8) -> ViewerEvent {
        ViewerEvent::Pointer {
            phase,
            x,
            y,
            buttons,
            at: None,
        }
    }

    /// A pointer event the page stamped with its own clock.
    fn pointer_at(phase: PointerPhase, x: f64, y: f64, at: f64) -> ViewerEvent {
        ViewerEvent::Pointer {
            phase,
            x,
            y,
            buttons: u8::from(phase != PointerPhase::Up),
            at: Some(at),
        }
    }

    fn gesture(driver: &FakeDriver, screen: (u32, u32), frame: (u32, u32)) -> Gesture<'_> {
        Gesture::new(
            driver,
            screen,
            frame,
            Duration::from_millis(1),
            Budget::open(),
        )
    }

    fn sign_in(username: Option<&str>, password: &str, remember: bool) -> ViewerCredentials {
        ViewerCredentials {
            username: username.map(str::to_owned),
            password: Zeroizing::new(password.to_owned()),
            remember,
        }
    }

    fn key(name: &str, code: &str, down: bool) -> ViewerEvent {
        ViewerEvent::Key {
            key: name.into(),
            code: code.into(),
            down,
            alt_graph: false,
        }
    }

    /// A key-down with AltGr held, as the page reports it.
    fn alt_graph_key(name: &str, code: &str) -> ViewerEvent {
        ViewerEvent::Key {
            key: name.into(),
            code: code.into(),
            down: true,
            alt_graph: true,
        }
    }

    fn wheel(x: f64, y: f64, dy: f64) -> ViewerEvent {
        ViewerEvent::Wheel { x, y, dx: 0.0, dy }
    }

    #[test]
    fn a_vnc_sign_in_waits_for_the_pane_retries_and_remembers_the_password() {
        let mut target = control(Protocol::Vnc);
        target.username = Some("holycat".into());
        let driver = FakeDriver::new(Platform::Desktop, 1920, 1080);
        let harness = start(target, None, Arc::default(), move |_, auth| {
            match auth.and_then(|auth| auth.password.as_deref().map(String::as_str)) {
                Some("right") => Ok(driver.clone()),
                Some(_) => Err(DriverError::Credentials {
                    retry: true,
                    needs_username: true,
                    message: "The server refused the account name or password.".into(),
                }),
                None => Err(DriverError::Credentials {
                    retry: false,
                    needs_username: true,
                    message: "The server asks for an account name and password.".into(),
                }),
            }
        });
        let session = &harness.session;
        assert_eq!(
            session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Credentials
        );
        // The person's page names the place in the app's language; the model reads English.
        let state = harness.backend().state();
        assert_eq!(state.endpoint, "实验室 · 5900");
        assert_eq!(
            state.credentials,
            Some(CredentialRequest {
                username: Some("holycat".into()),
                needs_username: true,
                retry: false
            })
        );
        let receipt = session.receipt();
        assert!(receipt.contains("sign in in the preview pane"), "{receipt}");
        assert!(
            receipt.contains("an account name and password"),
            "{receipt}"
        );
        assert_eq!(session.summary().state, SessionPhase::Credentials);
        let waiting = session
            .run_input(&actions(json!([{"action": "wait", "ms": 1}])), &|| false)
            .unwrap_err();
        assert!(
            waiting.contains("waiting for the user to sign in"),
            "{waiting}"
        );

        harness
            .backend()
            .credentials(sign_in(Some("holycat"), "wrong", true));
        eventually("the refusal", || {
            harness
                .backend()
                .state()
                .credentials
                .is_some_and(|request| request.retry)
        });
        harness
            .backend()
            .credentials(sign_in(Some("holycat"), "right", true));
        eventually("ready", || session.phase() == SessionPhase::Ready);
        let passwords: Vec<Option<String>> = harness
            .auths
            .lock()
            .unwrap()
            .iter()
            .map(|auth| auth.password.as_ref().map(|password| password.to_string()))
            .collect();
        assert_eq!(
            passwords,
            [None, Some("wrong".to_owned()), Some("right".to_owned())]
        );
        assert!(harness
            .auths
            .lock()
            .unwrap()
            .iter()
            .all(|auth| auth.username.as_deref() == Some("holycat")));
        eventually("the saved password", || {
            harness
                .store
                .entries
                .lock()
                .unwrap()
                .get("vnc|ssh:m1|5900|holycat")
                .map(String::as_str)
                == Some("right")
        });
        let receipt = session.receipt();
        assert!(
            receipt.starts_with(
                "Connected to the VNC server on lab (127.0.0.1:5900) (Fake \"Pixel\")."
            ),
            "{receipt}"
        );
        assert!(receipt.contains("1920x1080 pixels"), "{receipt}");
        assert!(receipt.contains("1280x720 image"), "{receipt}");
        assert!(!receipt.contains("http"), "{receipt}");
        let logs = harness.logs();
        assert!(!logs.contains("right") && !logs.contains("wrong"), "{logs}");
        assert!(logs.contains("saved the password"), "{logs}");
        assert!(harness.changes.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn a_stored_password_signs_in_without_asking() {
        let store = Arc::new(MemoryStore::default());
        store
            .entries
            .lock()
            .unwrap()
            .insert("vnc|ssh:m1|5900|".into(), "kept".into());
        let driver = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = start(
            control(Protocol::Vnc),
            None,
            store,
            move |_, auth| match auth.and_then(|auth| auth.password.as_deref().map(String::as_str))
            {
                Some("kept") => Ok(driver.clone()),
                _ => Err(DriverError::Credentials {
                    retry: false,
                    needs_username: false,
                    message: "VNC authentication is required.".into(),
                }),
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        assert_eq!(harness.auths.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_refused_stored_password_asks_and_is_forgotten_when_not_remembered() {
        let store = Arc::new(MemoryStore::default());
        store
            .entries
            .lock()
            .unwrap()
            .insert("vnc|ssh:m1|5900|".into(), "stale".into());
        let driver = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = start(
            control(Protocol::Vnc),
            None,
            store,
            move |_, auth| match auth.and_then(|auth| auth.password.as_deref().map(String::as_str))
            {
                Some("fresh") => Ok(driver.clone()),
                Some(_) => Err(DriverError::Credentials {
                    retry: true,
                    needs_username: false,
                    message: "The server refused the password.".into(),
                }),
                None => Err(DriverError::Credentials {
                    retry: false,
                    needs_username: false,
                    message: "VNC authentication is required.".into(),
                }),
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Credentials
        );
        assert!(harness.backend().state().credentials.unwrap().retry);
        harness.backend().credentials(sign_in(None, "fresh", false));
        eventually("ready", || harness.session.phase() == SessionPhase::Ready);
        eventually("the stale password gone", || {
            harness.store.entries.lock().unwrap().is_empty()
        });
    }

    #[test]
    fn a_lost_connection_reconnects_and_a_failing_preflight_ends_the_session() {
        let preflights = Arc::new(AtomicUsize::new(0));
        let preflight: Preflight = {
            let preflights = preflights.clone();
            Arc::new(move || {
                if preflights.fetch_add(1, Ordering::SeqCst) < 2 {
                    Ok(())
                } else {
                    Err("The virtual machine \"vm-proc\" is no longer running.".into())
                }
            })
        };
        let first = FakeDriver::new(Platform::Desktop, 1024, 768);
        let second = FakeDriver::new(Platform::Desktop, 1024, 768);
        let drivers = [first.clone(), second.clone()];
        let harness = start(
            control(Protocol::Vnc),
            Some(preflight),
            Arc::default(),
            move |attempt, _| {
                drivers
                    .get(attempt)
                    .cloned()
                    .ok_or_else(|| DriverError::Failed("unexpected connect".into()))
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        harness.emit(
            0,
            DriverEvent::Disconnected("the server closed the connection".into()),
        );
        eventually("the reconnect", || {
            harness.events.lock().unwrap().len() == 2
        });
        eventually("ready again", || {
            harness.session.phase() == SessionPhase::Ready
        });
        assert!(first.take().contains(&Call::Close));
        // An event from the old connection changes nothing.
        harness.emit(0, DriverEvent::Disconnected("late".into()));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(harness.session.phase(), SessionPhase::Ready);

        harness.emit(
            1,
            DriverEvent::Disconnected("the server closed the connection".into()),
        );
        eventually("the failure", || {
            harness.session.phase() == SessionPhase::Failed
        });
        let failure = harness.session.failure().unwrap();
        assert!(failure.contains("no longer running"), "{failure}");
        assert_eq!(harness.events.lock().unwrap().len(), 2);
        assert!(second.take().contains(&Call::Close));
        let logs = harness.logs();
        assert!(
            logs.contains("lost the connection (the server closed the connection); reconnecting"),
            "{logs}"
        );
        let error = harness.session.screenshot().unwrap_err();
        assert!(
            error.contains("failed") && error.contains("no longer running"),
            "{error}"
        );
        let receipt = harness.session.receipt();
        assert!(receipt.starts_with("Could not connect to"), "{receipt}");
    }

    #[test]
    fn reconnecting_gives_up_after_five_attempts() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = start(
            control(Protocol::Adb),
            None,
            Arc::default(),
            move |attempt, auth| {
                assert!(auth.is_none(), "only VNC is offered a sign-in");
                if attempt == 0 {
                    Ok(driver.clone())
                } else {
                    Err(DriverError::Failed("the device is offline".into()))
                }
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        harness.emit(0, DriverEvent::Disconnected("the device went away".into()));
        eventually("the failure", || {
            harness.session.phase() == SessionPhase::Failed
        });
        assert_eq!(harness.events.lock().unwrap().len(), 6);
        let failure = harness.session.failure().unwrap();
        assert!(failure.contains("after 5 attempts"), "{failure}");
        assert!(failure.contains("the device is offline"), "{failure}");
    }

    #[test]
    fn a_first_connect_that_fails_fails_the_session_with_its_reason() {
        let harness = start(control(Protocol::Wda), None, Arc::default(), |_, _| {
            Err(DriverError::Failed(
                "nothing is listening on 127.0.0.1:8100 on lab".into(),
            ))
        });
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Failed
        );
        assert_eq!(
            harness.session.failure().as_deref(),
            Some("nothing is listening on 127.0.0.1:8100 on lab")
        );
        assert_eq!(
            harness.session.receipt(),
            "Could not connect to WebDriverAgent (iOS) on lab (127.0.0.1:8100): nothing is listening on 127.0.0.1:8100 on lab. preview_logs with serverId \"pixel\" shows the connection log."
        );
    }

    #[test]
    fn the_summary_and_lock_key_name_the_target() {
        let mut target = control(Protocol::Adb);
        target.device = Some("emulator-5554".into());
        target.launch = Some("emu-proc".into());
        let harness = connected(target, FakeDriver::new(Platform::Android, 1080, 2400));
        let summary = serde_json::to_value(harness.session.summary()).unwrap();
        assert_eq!(
            summary,
            json!({
                "protocol": "adb",
                "platform": "android",
                "target": "emulator-5554 · lab",
                "targetDisplay": "emulator-5554 · 实验室",
                "label": "Fake \"Pixel\"",
                "screen": {"width": 1080, "height": 2400},
                "state": "ready",
                "readOnly": false,
                "launch": "emu-proc"
            })
        );
        assert_eq!(harness.session.lock_key(), "adb|ssh:m1|5037|emulator-5554");
        let receipt = harness.session.receipt();
        assert!(receipt.contains("576x1280 image"), "{receipt}");
        assert!(receipt.contains("preview_snapshot"), "{receipt}");
        let vnc = connected(
            control(Protocol::Vnc),
            FakeDriver::new(Platform::Desktop, 800, 600),
        );
        assert_eq!(vnc.session.summary().target, "lab · 127.0.0.1:5900");
        // The pane's row is in the app's language; the model's stays English.
        assert_eq!(
            vnc.session.summary().target_display,
            "实验室 · 127.0.0.1:5900"
        );
        assert_eq!(vnc.session.lock_key(), "vnc|ssh:m1|5900|");
        assert!(vnc.session.receipt().contains("no element tree"));
    }

    #[test]
    fn input_maps_the_coordinate_frame_and_refuses_a_screen_that_changed_size() {
        let driver = FakeDriver::with_tree(
            Platform::Android,
            1080,
            2400,
            Some(UiNode {
                role: "Button".into(),
                name: "OK".into(),
                bounds: Some(Rect {
                    x: 440,
                    y: 1100,
                    width: 200,
                    height: 200,
                }),
                ..UiNode::default()
            }),
        );
        let harness = connected(control(Protocol::Adb), driver.clone());
        let session = &harness.session;
        let shot = session.screenshot().unwrap();
        assert_eq!((shot.width, shot.height), (576, 1280));
        assert_eq!((shot.screen_width, shot.screen_height), (1080, 2400));
        let report = session
            .run_input(
                &actions(json!([{"action": "click", "x": 288, "y": 640}])),
                &|| false,
            )
            .unwrap();
        assert_eq!(report.lines, ["1. click (288, 640): ok"]);
        assert_eq!(driver.take(), [Call::Tap(540, 1200, 1)]);

        let snapshot = session.snapshot().unwrap();
        assert!(
            snapshot.contains("[1000001] Button: \"OK\" @235,587 107x107"),
            "{snapshot}"
        );
        assert_eq!(
            session.click_uid(1_000_001, true).unwrap(),
            "Double-clicked [1000001] Button \"OK\" at (288, 640)."
        );
        assert_eq!(driver.take(), [Call::Tap(540, 1200, 2)]);

        // The phone turns: the next input is refused until the model looks again.
        driver.resize(2400, 1080);
        harness.emit(
            0,
            DriverEvent::Resized {
                width: 2400,
                height: 1080,
            },
        );
        let error = session
            .run_input(
                &actions(json!([{"action": "click", "x": 10, "y": 10}])),
                &|| false,
            )
            .unwrap_err();
        assert!(
            error.contains("changed size") && error.contains("576x1280"),
            "{error}"
        );
        assert!(session
            .click_uid(1_000_001, false)
            .unwrap_err()
            .contains("changed size"));
        assert!(driver.take().is_empty());
        assert_eq!(
            session.summary().screen,
            Some(ScreenSize {
                width: 2400,
                height: 1080
            })
        );
        let shot = session.screenshot().unwrap();
        assert_eq!((shot.width, shot.height), (1280, 576));
        // The uids of the old picture are gone with it.
        let error = session.click_uid(1_000_001, false).unwrap_err();
        assert!(
            error.contains("not in the latest preview_snapshot"),
            "{error}"
        );
        let report = session
            .run_input(
                &actions(json!([{"action": "click", "x": 1280, "y": 576}])),
                &|| false,
            )
            .unwrap();
        assert_eq!(report.lines, ["1. click (1280, 576): ok"]);
        assert_eq!(driver.take(), [Call::Tap(2399, 1079, 1)]);
        let report = session
            .run_input(
                &actions(json!([
                    {"action": "click", "x": 1281, "y": 0},
                    {"action": "wait", "ms": 1}
                ])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            ["1. click (1281, 0): failed: (1281, 0) is outside the 1280x576 coordinate frame (action 2 did not run)"]
        );
    }

    #[test]
    fn snapshots_number_from_a_million_and_one_and_replace_the_last_ones_uids() {
        let element = |role: &str, name: &str, x: i32, y: i32| UiNode {
            role: role.into(),
            name: name.into(),
            bounds: Some(Rect {
                x,
                y,
                width: 100,
                height: 40,
            }),
            ..UiNode::default()
        };
        let tree = UiNode {
            role: "FrameLayout".into(),
            children: vec![
                element("EditText", "Search", 0, 0),
                element("Button", "Off screen", 500, 900),
            ],
            ..UiNode::default()
        };
        let driver = FakeDriver::with_tree(Platform::Ios, 400, 800, Some(tree));
        let harness = connected(control(Protocol::Wda), driver.clone());
        let session = &harness.session;
        let first = session.snapshot().unwrap();
        assert!(
            first.contains("[1000001] EditText: \"Search\" @0,0 100x40"),
            "{first}"
        );
        let second = session.snapshot().unwrap();
        assert!(second.contains("[1000003] EditText"), "{second}");
        assert!(session
            .click_uid(1_000_001, false)
            .unwrap_err()
            .contains("not in the latest"));
        assert!(session
            .click_uid(1_000_004, false)
            .unwrap_err()
            .contains("off the screen"));
        let filled = session.fill_uid(1_000_003, "cats").unwrap();
        assert_eq!(
            filled,
            "Clicked [1000003] EditText \"Search\" and typed 4 characters; text already in the field was kept."
        );
        assert_eq!(
            driver.take(),
            [Call::Tap(50, 20, 1), Call::Text("cats".into())]
        );

        // A screen that grows in proportion keeps its coordinate frame, but the
        // centres measured on it are stale all the same.
        let tablet = FakeDriver::with_tree(
            Platform::Ios,
            1000,
            2000,
            Some(element("Button", "OK", 10, 10)),
        );
        let harness = connected(control(Protocol::Wda), tablet.clone());
        assert!(harness
            .session
            .snapshot()
            .unwrap()
            .contains("[1000001] Button"));
        tablet.resize(1500, 3000);
        let error = harness.session.click_uid(1_000_001, false).unwrap_err();
        assert!(
            error.contains("changed size since the last preview_snapshot"),
            "{error}"
        );
        assert!(tablet.take().is_empty());

        let vnc = connected(
            control(Protocol::Vnc),
            FakeDriver::new(Platform::Desktop, 800, 600),
        );
        assert_eq!(vnc.session.snapshot().unwrap_err(), snapshot::NO_TREE);
    }

    #[test]
    fn actions_are_read_with_errors_that_say_what_to_fix() {
        let parsed = actions(json!([
            {"action": "click", "x": 10.4, "y": 20.6, "button": "right", "count": 2},
            {"action": "long_press", "x": 1, "y": 2},
            {"action": "drag", "x": 1, "y": 2, "toX": 3, "toY": 4},
            {"action": "scroll", "x": 5, "y": 6, "dy": -3},
            {"action": "move", "x": 0, "y": 0},
            {"action": "type", "text": "hello"},
            {"action": "key", "key": "ctrl+c", "repeat": 3},
            {"action": "wait", "ms": 500}
        ]));
        assert_eq!(
            parsed,
            [
                InputAction::Click {
                    x: 10,
                    y: 21,
                    button: MouseButton::Right,
                    count: 2,
                    modifiers: vec![]
                },
                InputAction::LongPress {
                    x: 1,
                    y: 2,
                    ms: 800,
                    modifiers: vec![]
                },
                InputAction::Drag {
                    x: 1,
                    y: 2,
                    to_x: 3,
                    to_y: 4,
                    ms: 300,
                    modifiers: vec![]
                },
                InputAction::Scroll {
                    x: 5,
                    y: 6,
                    dx: 0,
                    dy: -3,
                    modifiers: vec![]
                },
                InputAction::Move { x: 0, y: 0 },
                InputAction::Type {
                    text: "hello".into(),
                    via: TypeVia::Auto
                },
                InputAction::Key {
                    key: "ctrl+c".into(),
                    repeat: 3
                },
                InputAction::Wait { ms: 500 },
            ]
        );
        assert_eq!(parsed[0].describe(), "double right click (10, 21)");

        let error = |value: Value| parse_actions(&value).unwrap_err();
        let cases = [
            (json!({"action": "click"}), "\"actions\" must be an array"),
            (json!([]), "\"actions\" is empty"),
            (
                json!([{"action": "tap", "x": 1, "y": 1}]),
                "Action 1: unknown action \"tap\". Actions: click, long_press",
            ),
            (json!([{"x": 1}]), "Action 1 has no \"action\""),
            (
                json!([{"action": "click", "y": 1}]),
                "Action 1 (click): \"x\" is required",
            ),
            (
                json!([{"action": "click", "x": -1, "y": 1}]),
                "\"x\" must be a non-negative number of pixels; got -1",
            ),
            (
                json!([{"action": "click", "x": "1", "y": 1}]),
                "\"x\" must be a number of pixels, not a string",
            ),
            (
                json!([{"action": "click", "x": 1, "y": 1, "count": 4}]),
                "\"count\" must be a whole number from 1 to 3; got 4",
            ),
            (
                json!([{"action": "click", "x": 1, "y": 1, "button": "back"}]),
                "\"button\" must be \"left\", \"right\" or \"middle\"",
            ),
            (
                json!([{"action": "drag", "x": 1, "y": 1, "to_x": 2, "to_y": 2}]),
                "unknown field \"to_x\"; drag takes x, y, toX, toY, ms",
            ),
            (json!([{"action": "scroll", "x": 1, "y": 1}]), "give \"dy\""),
            (json!([{"action": "type", "text": ""}]), "\"text\" is empty"),
            (
                json!([{"action": "key", "key": "hyper+x"}]),
                "Unknown key \"hyper\"",
            ),
            (
                json!([{"action": "key", "key": "a", "repeat": 51}]),
                "\"repeat\" must be a whole number from 1 to 50",
            ),
            (
                json!([{"action": "wait"}]),
                "Action 1 (wait): \"ms\" is required",
            ),
            (
                json!([{"action": "wait", "ms": 5001}]),
                "\"ms\" must be a whole number from 0 to 5000",
            ),
            (json!([{"action": "wait", "ms": 1.5}]), "got 1.5"),
            (json!([1]), "Action 1 must be an object"),
        ];
        for (value, expected) in cases {
            let message = error(value);
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
        let many: Vec<Value> = (0..21)
            .map(|_| json!({"action": "wait", "ms": 1}))
            .collect();
        assert!(error(Value::Array(many)).contains("at most 20"));
        let long: Vec<Value> = (0..6)
            .map(|_| json!({"action": "wait", "ms": 4000}))
            .collect();
        assert!(error(Value::Array(long)).contains("the limit is 20000 ms"));
        let mut slow: Vec<Value> = (0..4)
            .map(|_| json!({"action": "wait", "ms": 4900}))
            .collect();
        slow.push(json!({"action": "scroll", "x": 1, "y": 1, "dx": 1, "dy": 1}));
        assert!(error(Value::Array(slow)).contains("about 20200 ms"));
    }

    #[test]
    fn desktop_gestures_are_pointer_sequences() {
        let driver = FakeDriver::new(Platform::Desktop, 200, 200);
        let desktop = gesture(&driver, (200, 200), (200, 200));
        let run = |value: Value| {
            for action in actions(value) {
                desktop.perform(&action).unwrap();
            }
            driver.take()
        };
        assert_eq!(
            run(json!([{"action": "click", "x": 10, "y": 20, "count": 2}])),
            [
                Call::Pointer(10, 20, 0),
                Call::Pointer(10, 20, 1),
                Call::Pointer(10, 20, 0),
                Call::Pointer(10, 20, 1),
                Call::Pointer(10, 20, 0)
            ]
        );
        assert_eq!(
            run(json!([{"action": "click", "x": 1, "y": 2, "button": "right"}])),
            [
                Call::Pointer(1, 2, 0),
                Call::Pointer(1, 2, 4),
                Call::Pointer(1, 2, 0)
            ]
        );
        assert_eq!(
            run(json!([{"action": "click", "x": 1, "y": 2, "button": "middle"}]))[1],
            Call::Pointer(1, 2, 2)
        );
        let drag = run(json!([
            {"action": "drag", "x": 10, "y": 10, "toX": 110, "toY": 60, "ms": 10}
        ]));
        assert_eq!(drag.len(), 13);
        assert_eq!(
            drag[..2],
            [Call::Pointer(10, 10, 0), Call::Pointer(10, 10, 1)]
        );
        assert_eq!(drag[6], Call::Pointer(60, 35, 1));
        assert_eq!(drag[11], Call::Pointer(110, 60, 1));
        assert_eq!(drag[12], Call::Pointer(110, 60, 0));
        assert_eq!(
            run(json!([{"action": "scroll", "x": 5, "y": 6, "dy": 2}])),
            [
                Call::Pointer(5, 6, 16),
                Call::Pointer(5, 6, 0),
                Call::Pointer(5, 6, 16),
                Call::Pointer(5, 6, 0)
            ]
        );
        assert_eq!(
            run(json!([{"action": "scroll", "x": 5, "y": 6, "dy": -1, "dx": 1}])),
            [
                Call::Pointer(5, 6, 8),
                Call::Pointer(5, 6, 0),
                Call::Pointer(5, 6, 64),
                Call::Pointer(5, 6, 0)
            ]
        );
        assert_eq!(
            run(json!([{"action": "scroll", "x": 5, "y": 6, "dx": -1}])),
            [Call::Pointer(5, 6, 32), Call::Pointer(5, 6, 0)]
        );
        assert_eq!(
            run(json!([{"action": "move", "x": 7, "y": 8}])),
            [Call::Pointer(7, 8, 0)]
        );
        assert_eq!(
            run(json!([{"action": "long_press", "x": 7, "y": 8, "ms": 5}])),
            [
                Call::Pointer(7, 8, 0),
                Call::Pointer(7, 8, 1),
                Call::Pointer(7, 8, 0)
            ]
        );
        assert_eq!(
            run(json!([
                {"action": "key", "key": "Enter", "repeat": 2},
                {"action": "key", "key": "ctrl+c"},
                {"action": "key", "key": "Home"},
                {"action": "type", "text": "hi"}
            ])),
            [
                Call::Key(Key::Named(NamedKey::Enter), None),
                Call::Key(Key::Named(NamedKey::Enter), None),
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Control)],
                    key: Key::Char('c')
                }),
                Call::Key(Key::Named(NamedKey::Home), None),
                Call::Text("hi".into())
            ]
        );
        // The coordinate frame maps back onto a bigger screen.
        let retina = FakeDriver::new(Platform::Desktop, 2880, 1800);
        gesture(&retina, (2880, 1800), (1280, 800))
            .perform(&InputAction::Move { x: 640, y: 400 })
            .unwrap();
        assert_eq!(retina.take(), [Call::Pointer(1440, 900, 0)]);
    }

    #[test]
    fn modifiers_ride_on_the_four_pointer_actions_and_name_only_modifiers() {
        let parsed = actions(json!([
            {"action": "click", "x": 10, "y": 20, "modifiers": "Ctrl+Shift"},
            {"action": "long_press", "x": 1, "y": 2, "modifiers": "alt"},
            {"action": "drag", "x": 1, "y": 2, "toX": 3, "toY": 4, "modifiers": "cmd+option"},
            {"action": "scroll", "x": 5, "y": 6, "dy": 2, "modifiers": "command"},
            {"action": "click", "x": 7, "y": 8, "modifiers": null}
        ]));
        let held: Vec<&[Key]> = parsed.iter().map(InputAction::modifiers).collect();
        let named = |keys: &[NamedKey]| keys.iter().map(|key| Key::Named(*key)).collect::<Vec<_>>();
        assert_eq!(held[0], named(&[NamedKey::Control, NamedKey::Shift]));
        assert_eq!(held[1], named(&[NamedKey::Alt]));
        assert_eq!(held[2], named(&[NamedKey::Meta, NamedKey::Alt]));
        assert_eq!(held[3], named(&[NamedKey::Meta]));
        assert!(held[4].is_empty());
        let lines: Vec<String> = parsed.iter().map(InputAction::describe).collect();
        assert_eq!(
            lines,
            [
                "click (10, 20) holding ctrl+shift",
                "long_press (1, 2) for 800 ms holding alt",
                "drag (1, 2) to (3, 4) over 300 ms holding cmd+alt",
                "scroll (5, 6) dy 2 holding cmd",
                "click (7, 8)"
            ]
        );

        let error = |value: Value| parse_actions(&value).unwrap_err();
        let cases = [
            (
                json!([{"action": "click", "x": 1, "y": 1, "modifiers": "shift+a"}]),
                "Action 1 (click): \"a\" is not a modifier; modifiers are shift, ctrl, alt",
            ),
            (
                json!([{"action": "drag", "x": 1, "y": 1, "toX": 2, "toY": 2, "modifiers": "ctrl+"}]),
                "Action 1 (drag): \"ctrl+\" has an empty name",
            ),
            (
                json!([{"action": "scroll", "x": 1, "y": 1, "dy": 1, "modifiers": ""}]),
                "Action 1 (scroll): \"modifiers\" must be 1-64 characters",
            ),
            (
                json!([{"action": "long_press", "x": 1, "y": 1, "modifiers": "s".repeat(65)}]),
                "\"modifiers\" must be 1-64 characters, such as \"shift\" or \"ctrl+shift\"; got 65.",
            ),
            (
                json!([{"action": "click", "x": 1, "y": 1, "modifiers": ["shift"]}]),
                "\"modifiers\" must be a string such as \"shift\" or \"ctrl+shift\", not an array.",
            ),
            (
                json!([{"action": "move", "x": 1, "y": 1, "modifiers": "shift"}]),
                "Action 1 (move): unknown field \"modifiers\"; move takes x, y.",
            ),
            (
                json!([{"action": "key", "key": "a", "modifiers": "shift"}]),
                "unknown field \"modifiers\"; key takes key, repeat.",
            ),
            (
                json!([{"action": "type", "text": "a", "modifiers": "shift"}]),
                "unknown field \"modifiers\"; type takes text, via, chord.",
            ),
            (
                json!([{"action": "wait", "ms": 1, "modifiers": "shift"}]),
                "unknown field \"modifiers\"; wait takes ms.",
            ),
        ];
        for (value, expected) in cases {
            let message = error(value);
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
        }
    }

    /// Held modifiers go down in order before the pointer moves and come up in reverse after
    /// it is done: a shift-click, a ctrl+alt drag, a cmd-wheel, a held long press.
    #[test]
    fn a_desktop_holds_the_modifiers_around_the_action() {
        let driver = FakeDriver::new(Platform::Desktop, 200, 200);
        let desktop = gesture(&driver, (200, 200), (200, 200));
        let run = |value: Value| {
            for action in actions(value) {
                desktop.perform(&action).unwrap();
            }
            driver.take()
        };
        let down = |key: NamedKey| Call::Key(Key::Named(key), Some(true));
        let up = |key: NamedKey| Call::Key(Key::Named(key), Some(false));
        assert_eq!(
            run(json!([{"action": "click", "x": 10, "y": 20, "modifiers": "shift"}])),
            [
                down(NamedKey::Shift),
                Call::Pointer(10, 20, 0),
                Call::Pointer(10, 20, 1),
                Call::Pointer(10, 20, 0),
                up(NamedKey::Shift)
            ]
        );
        let drag = run(json!([{
            "action": "drag", "x": 10, "y": 10, "toX": 110, "toY": 60, "ms": 10,
            "modifiers": "ctrl+alt"
        }]));
        assert_eq!(drag.len(), 2 + 13 + 2);
        assert_eq!(
            drag[..3],
            [
                down(NamedKey::Control),
                down(NamedKey::Alt),
                Call::Pointer(10, 10, 0)
            ]
        );
        assert_eq!(drag[14], Call::Pointer(110, 60, 0));
        assert_eq!(drag[15..], [up(NamedKey::Alt), up(NamedKey::Control)]);
        assert_eq!(
            run(json!([{"action": "scroll", "x": 5, "y": 6, "dy": 1, "modifiers": "cmd"}])),
            [
                down(NamedKey::Meta),
                Call::Pointer(5, 6, 16),
                Call::Pointer(5, 6, 0),
                up(NamedKey::Meta)
            ]
        );
        assert_eq!(
            run(json!([
                {"action": "long_press", "x": 7, "y": 8, "ms": 5, "modifiers": "shift+ctrl"}
            ])),
            [
                down(NamedKey::Shift),
                down(NamedKey::Control),
                Call::Pointer(7, 8, 0),
                Call::Pointer(7, 8, 1),
                Call::Pointer(7, 8, 0),
                up(NamedKey::Control),
                up(NamedKey::Shift)
            ]
        );
        // A point off the frame is refused before any key goes down.
        for value in [
            json!([{"action": "click", "x": 201, "y": 1, "modifiers": "shift"}]),
            json!([{"action": "drag", "x": 1, "y": 1, "toX": 1, "toY": 300, "modifiers": "shift"}]),
        ] {
            let action = &actions(value)[0];
            assert!(desktop.perform(action).is_err());
            assert!(driver.take().is_empty());
        }
    }

    /// What goes down comes up when the action fails: a long press and a drag cut short.
    #[test]
    fn held_modifiers_come_up_when_the_action_is_cut_short() {
        let driver = FakeDriver::new(Platform::Desktop, 200, 200);
        let stopped = Gesture::new(
            driver.as_ref(),
            (200, 200),
            (200, 200),
            Duration::from_millis(1),
            Budget {
                cancelled: &|| true,
                limit: Instant::now() + Duration::from_secs(60),
            },
        );
        let held = &actions(json!([
            {"action": "long_press", "x": 7, "y": 8, "ms": 2000, "modifiers": "shift"}
        ]))[0];
        assert!(stopped.perform(held).is_err());
        assert_eq!(stopped.halted.get(), Some(Halt::Cancelled));
        assert_eq!(
            driver.take(),
            [
                Call::Key(Key::Named(NamedKey::Shift), Some(true)),
                Call::Pointer(7, 8, 0),
                Call::Pointer(7, 8, 1),
                Call::Pointer(7, 8, 0),
                Call::Key(Key::Named(NamedKey::Shift), Some(false))
            ]
        );
        let dragged = &actions(json!([
            {"action": "drag", "x": 1, "y": 1, "toX": 100, "toY": 1, "ms": 2000, "modifiers": "ctrl"}
        ]))[0];
        assert!(stopped.perform(dragged).is_err());
        let calls = driver.take();
        assert_eq!(
            calls.first(),
            Some(&Call::Key(Key::Named(NamedKey::Control), Some(true)))
        );
        assert_eq!(calls[calls.len() - 2], Call::Pointer(1, 1, 0));
        assert_eq!(
            calls.last(),
            Some(&Call::Key(Key::Named(NamedKey::Control), Some(false)))
        );

        // A batch stopped halfway through a held action lets go of its modifiers too.
        let driver = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let started = Instant::now();
        let cancelled = move || started.elapsed() > Duration::from_millis(300);
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "long_press", "x": 4, "y": 4, "ms": 3000, "modifiers": "cmd+shift"},
                    {"action": "click", "x": 1, "y": 1}
                ])),
                &cancelled,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            ["1. long_press (4, 4) for 3000 ms holding cmd+shift: stopped: cancelled (action 2 did not run)"]
        );
        let calls: Vec<Call> = driver
            .take()
            .into_iter()
            .filter(|call| matches!(call, Call::Key(..) | Call::Pointer(..)))
            .collect();
        assert_eq!(
            calls,
            [
                Call::Key(Key::Named(NamedKey::Meta), Some(true)),
                Call::Key(Key::Named(NamedKey::Shift), Some(true)),
                Call::Pointer(4, 4, 0),
                Call::Pointer(4, 4, 1),
                Call::Pointer(4, 4, 0),
                Call::Key(Key::Named(NamedKey::Shift), Some(false)),
                Call::Key(Key::Named(NamedKey::Meta), Some(false))
            ]
        );
    }

    /// A phone has no keyboard to hold modifiers on: the batch is refused before any of it runs.
    #[test]
    fn a_touch_screen_refuses_modifiers_before_anything_runs() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let error = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "click", "x": 1, "y": 1},
                    {"action": "scroll", "x": 500, "y": 1200, "dy": 1, "modifiers": "ctrl"}
                ])),
                &|| false,
            )
            .unwrap_err();
        assert_eq!(
            error,
            "Action 2 (scroll): modifiers need a desktop: a phone has no keyboard to hold them on."
        );
        assert!(driver.take().is_empty());
        // And a gesture asked directly says the same.
        let touch = gesture(&driver, (1080, 2400), (1080, 2400));
        let action =
            &actions(json!([{"action": "click", "x": 1, "y": 1, "modifiers": "shift"}]))[0];
        assert_eq!(
            touch.perform(action).unwrap_err(),
            DriverError::Unsupported(
                "modifiers need a desktop: a phone has no keyboard to hold them on".into()
            )
        );
        assert!(driver.take().is_empty());
    }

    #[test]
    fn touch_gestures_are_taps_and_swipes() {
        let driver = FakeDriver::new(Platform::Android, 1000, 2000);
        let touch = gesture(&driver, (1000, 2000), (1000, 2000));
        let run = |value: Value| -> Result<Vec<Call>, String> {
            for action in actions(value) {
                touch.perform(&action).map_err(|error| error.to_string())?;
            }
            Ok(driver.take())
        };
        assert_eq!(
            run(json!([{"action": "click", "x": 10, "y": 20, "count": 2}])).unwrap(),
            [Call::Tap(10, 20, 2)]
        );
        let error =
            run(json!([{"action": "click", "x": 10, "y": 20, "button": "right"}])).unwrap_err();
        assert!(error.contains("long_press"), "{error}");
        assert_eq!(
            run(json!([{"action": "long_press", "x": 1, "y": 2}])).unwrap(),
            [Call::LongPress(1, 2, 800)]
        );
        assert_eq!(
            run(json!([
                {"action": "drag", "x": 1, "y": 2, "toX": 3, "toY": 400, "ms": 250}
            ]))
            .unwrap(),
            [Call::Swipe((1, 2), (3, 400), 250)]
        );
        // Scrolling down drags the content up: the finger moves up an eighth a notch.
        assert_eq!(
            run(json!([{"action": "scroll", "x": 500, "y": 1200, "dy": 2}])).unwrap(),
            [Call::Swipe((500, 1200), (500, 700), 300)]
        );
        assert_eq!(
            run(json!([{"action": "scroll", "x": 500, "y": 1200, "dy": -1, "dx": 1}])).unwrap(),
            [
                Call::Swipe((500, 1200), (500, 1450), 300),
                Call::Swipe((500, 1200), (375, 1200), 300)
            ]
        );
        assert_eq!(
            run(json!([{"action": "scroll", "x": 500, "y": 1200, "dy": 20}])).unwrap(),
            [Call::Swipe((500, 1200), (500, 0), 300)]
        );
        let error = run(json!([{"action": "scroll", "x": 500, "y": 0, "dy": 1}])).unwrap_err();
        assert!(error.contains("no room"), "{error}");
        let error = run(json!([{"action": "move", "x": 1, "y": 1}])).unwrap_err();
        assert!(error.contains("hover"), "{error}");
        assert_eq!(
            run(json!([{"action": "key", "key": "Home"}])).unwrap(),
            [Call::Key(Key::Named(NamedKey::HomeScreen), None)]
        );
    }

    #[test]
    fn a_batch_reports_each_action_and_stops_at_the_first_failure() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "click", "x": 288, "y": 640},
                    {"action": "type", "text": "hello"},
                    {"action": "move", "x": 1, "y": 1},
                    {"action": "key", "key": "Enter"},
                    {"action": "wait", "ms": 1}
                ])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            [
                "1. click (288, 640): ok",
                "2. type \"hello\": ok",
                "3. move (1, 1): failed: a touch screen has no pointer to hover; move is for desktops (actions 4-5 did not run)"
            ]
        );
        let report = harness
            .session
            .run_input(&actions(json!([{"action": "wait", "ms": 1}])), &|| true)
            .unwrap();
        assert_eq!(report.lines, ["1. wait 1 ms: cancelled before it ran"]);
    }

    #[test]
    fn a_read_only_screen_refuses_every_input() {
        let mut target = control(Protocol::Adb);
        target.read_only = true;
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(target, driver.clone());
        let session = &harness.session;
        let error = session
            .run_input(
                &actions(json!([{"action": "click", "x": 1, "y": 1}])),
                &|| false,
            )
            .unwrap_err();
        assert!(error.contains("read-only"), "{error}");
        // The refusal says what still works, the screen tools that only read among it.
        assert!(
            error.ends_with(": input and changes to the device are refused. Looking still works: preview_screenshot, preview_snapshot, preview_zoom, preview_wait and the screen tools that only read."),
            "{error}"
        );
        assert!(
            session.receipt().contains(" It is read-only: preview_input, preview_click, preview_fill, preview_dialog and every other tool that changes the device are refused; looking still works."),
            "{}",
            session.receipt()
        );
        // A desktop has no element tree, so its refusal does not offer preview_snapshot.
        let mut desk = control(Protocol::Vnc);
        desk.read_only = true;
        let desk = connected(desk, FakeDriver::new(Platform::Desktop, 800, 600));
        let refused = desk
            .session
            .run_input(
                &actions(json!([{"action": "click", "x": 1, "y": 1}])),
                &|| false,
            )
            .unwrap_err();
        assert!(
            refused.ends_with(": input and changes to the device are refused. Looking still works: preview_screenshot, preview_zoom, preview_wait and the screen tools that only read."),
            "{refused}"
        );
        assert!(session
            .click_uid(1_000_001, false)
            .unwrap_err()
            .contains("read-only"));
        assert!(session
            .fill_uid(1_000_001, "x")
            .unwrap_err()
            .contains("read-only"));
        harness.backend().input(
            "page",
            vec![
                pointer(PointerPhase::Down, 0.5, 0.5, 1),
                pointer(PointerPhase::Up, 0.5, 0.5, 0),
                ViewerEvent::Text { text: "x".into() },
            ],
        );
        assert!(driver.take().is_empty());
        assert!(session.screenshot().is_ok());
        assert!(session.receipt().contains("read-only"));
        assert!(harness.backend().state().read_only);
    }

    #[test]
    fn a_touch_on_the_viewer_becomes_a_tap_a_long_press_or_a_swipe() {
        let screen = (1080, 2400);
        assert_eq!(
            touch_gesture(
                (0.5, 0.5),
                (0.505, 0.5),
                false,
                Duration::from_millis(120),
                screen
            ),
            TouchGesture::Tap((540, 1200))
        );
        assert_eq!(
            touch_gesture(
                (0.5, 0.5),
                (0.5, 0.5),
                false,
                Duration::from_millis(900),
                screen
            ),
            TouchGesture::LongPress((540, 1200), 900)
        );
        assert_eq!(
            touch_gesture(
                (0.5, 0.5),
                (0.5, 0.5),
                false,
                Duration::from_secs(60),
                screen
            ),
            TouchGesture::LongPress((540, 1200), 5000)
        );
        assert_eq!(
            touch_gesture(
                (0.5, 0.8),
                (0.5, 0.3),
                true,
                Duration::from_millis(250),
                screen
            ),
            TouchGesture::Swipe((540, 1920), (540, 720), 250)
        );
        // A flick shorter than any swipe a device takes is stretched to one.
        assert_eq!(
            touch_gesture(
                (0.5, 0.8),
                (0.5, 0.3),
                true,
                Duration::from_millis(5),
                screen
            ),
            TouchGesture::Swipe((540, 1920), (540, 720), 100)
        );
        // Moving away and back is still a swipe, not a tap.
        assert!(matches!(
            touch_gesture(
                (0.5, 0.5),
                (0.5, 0.5),
                true,
                Duration::from_millis(100),
                screen
            ),
            TouchGesture::Swipe(..)
        ));
        assert_eq!(fraction_point((1.0, -0.5), screen), (1079, 0));
        assert_eq!(fraction_point((f64::NAN, 0.25), screen), (0, 600));

        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let backend = harness.backend();
        backend.input(
            "page",
            vec![
                pointer(PointerPhase::Down, 0.5, 0.5, 1),
                pointer(PointerPhase::Up, 0.5, 0.5, 0),
            ],
        );
        assert_eq!(driver.take(), [Call::Tap(540, 1200, 1)]);
        backend.input("page", vec![pointer(PointerPhase::Down, 0.25, 0.25, 1)]);
        thread::sleep(Duration::from_millis(550));
        backend.input("page", vec![pointer(PointerPhase::Up, 0.25, 0.25, 0)]);
        match driver.take().as_slice() {
            [Call::LongPress(270, 600, ms)] => assert!((550..2000).contains(ms), "{ms}"),
            other => panic!("expected a long press, got {other:?}"),
        }
        backend.input(
            "page",
            vec![
                pointer(PointerPhase::Down, 0.5, 0.8, 1),
                pointer(PointerPhase::Move, 0.5, 0.5, 1),
                pointer(PointerPhase::Up, 0.5, 0.3, 0),
            ],
        );
        assert_eq!(driver.take(), [Call::Swipe((540, 1920), (540, 720), 100)]);
        backend.input(
            "page",
            vec![
                key("Enter", "Enter", true),
                key("Enter", "Enter", false),
                ViewerEvent::Button {
                    name: "Home".into(),
                },
                wheel(0.5, 0.5, 1.0),
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Key(Key::Named(NamedKey::Enter), None),
                Call::Key(Key::Named(NamedKey::HomeScreen), None),
                Call::Swipe((540, 1200), (540, 900), 300)
            ]
        );
    }

    #[test]
    fn dom_buttons_become_rfb_buttons() {
        assert_eq!(dom_buttons_to_rfb(0), 0);
        assert_eq!(dom_buttons_to_rfb(1), buttons::LEFT);
        assert_eq!(dom_buttons_to_rfb(2), buttons::RIGHT);
        assert_eq!(dom_buttons_to_rfb(4), buttons::MIDDLE);
        assert_eq!(dom_buttons_to_rfb(3), buttons::LEFT | buttons::RIGHT);
        assert_eq!(dom_buttons_to_rfb(8 | 16), 0);
    }

    #[test]
    fn the_viewer_drives_a_desktop_edge_by_edge_and_lets_go_when_it_leaves() {
        let driver = FakeDriver::new(Platform::Desktop, 1920, 1080);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let backend = harness.backend();
        backend.input(
            "page",
            vec![
                pointer(PointerPhase::Down, 0.25, 0.5, 2),
                wheel(0.25, 0.5, 1.5),
                pointer(PointerPhase::Up, 0.25, 0.5, 0),
                wheel(0.25, 0.5, 0.5),
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Pointer(480, 540, 4),
                Call::Pointer(480, 540, 4 | 16),
                Call::Pointer(480, 540, 4),
                Call::Pointer(480, 540, 0),
                Call::Pointer(480, 540, 16),
                Call::Pointer(480, 540, 0)
            ]
        );
        // A key comes up as what went down, though Shift changed its name.
        backend.input(
            "page",
            vec![
                key("a", "KeyA", true),
                key("A", "KeyA", false),
                key("Shift", "ShiftLeft", true),
                pointer(PointerPhase::Down, 0.5, 0.5, 1),
                ViewerEvent::Button {
                    name: "ctrl+alt+Delete".into(),
                },
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Key(Key::Char('a'), Some(true)),
                Call::Key(Key::Char('a'), Some(false)),
                Call::Key(Key::Named(NamedKey::Shift), Some(true)),
                Call::Pointer(960, 540, 1),
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Alt)],
                    key: Key::Named(NamedKey::Delete)
                })
            ]
        );
        backend.viewer_gone("page");
        assert!(driver.take().is_empty(), "the stream's end itself sends nothing");
        backend.release_viewer("page");
        assert_eq!(
            driver.take(),
            [
                Call::Pointer(960, 540, 0),
                Call::Key(Key::Named(NamedKey::Shift), Some(false))
            ]
        );
    }

    #[test]
    fn the_viewer_is_ignored_while_the_model_holds_the_screen() {
        let driver = FakeDriver::new(Platform::Desktop, 100, 100);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let session = harness.session.clone();
        let batch = thread::spawn(move || {
            session
                .run_input(&actions(json!([{"action": "wait", "ms": 400}])), &|| false)
                .unwrap()
        });
        eventually("the model's batch", || {
            harness.backend().state().model_active
        });
        harness
            .backend()
            .input("page", vec![pointer(PointerPhase::Move, 0.5, 0.5, 0)]);
        assert_eq!(batch.join().unwrap().lines, ["1. wait 400 ms: ok"]);
        assert!(driver.take().is_empty());
        assert!(!harness.backend().state().model_active);
        harness
            .backend()
            .input("page", vec![pointer(PointerPhase::Move, 0.5, 0.5, 0)]);
        assert_eq!(driver.take(), [Call::Pointer(50, 50, 0)]);
    }

    #[test]
    fn frames_flow_to_a_visible_page_only() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let backend = harness.backend();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(driver.captures.load(Ordering::SeqCst), 0);
        assert!(backend.latest_frame().is_none());
        backend.set_viewer_visible("page", true);
        let frame = backend
            .next_frame(0, Duration::from_secs(5))
            .expect("a frame");
        assert_eq!((frame.width, frame.height), (720, 1600));
        let next = backend
            .next_frame(frame.seq, Duration::from_secs(5))
            .expect("another frame");
        assert!(next.seq > frame.seq);
        backend.viewer_gone("page");
        thread::sleep(Duration::from_millis(50));
        let captured = driver.captures.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(driver.captures.load(Ordering::SeqCst), captured);

        // A pushing driver is encoded when it says the picture changed.
        let desktop = FakeDriver::new(Platform::Desktop, 1920, 1080);
        let harness = connected(control(Protocol::Vnc), desktop.clone());
        let backend = harness.backend();
        backend.set_viewer_visible("page", true);
        let first = backend
            .next_frame(0, Duration::from_secs(5))
            .expect("the first frame");
        assert_eq!((first.width, first.height), (1600, 900));
        harness.emit(0, DriverEvent::Frame);
        let second = backend
            .next_frame(first.seq, Duration::from_secs(5))
            .expect("the update");
        assert_eq!(second.seq, first.seq + 1);
        assert_eq!(desktop.captures.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn closing_disconnects_and_stops_answering() {
        let driver = FakeDriver::new(Platform::Desktop, 100, 100);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        harness.session.close();
        assert!(driver.take().contains(&Call::Close));
        let error = harness.session.screenshot().unwrap_err();
        assert!(error.contains("stopped"), "{error}");
        assert!(harness
            .backend()
            .next_frame(0, Duration::from_secs(5))
            .is_none());
        harness.session.close();
    }

    // ------------------------------------------------------------ lifecycle

    /// Failed is final: a supervisor that wakes after the failure (the closed connection's
    /// reader reports the close at once) cannot move the session back to reconnecting, install
    /// a late connect or ask for a password, and ending it leaves it failed and closed together.
    #[test]
    fn a_failed_session_stays_failed_whatever_comes_after() {
        let driver = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let inner = &harness.session.inner;
        inner.fail("the virtual machine stopped".into());
        inner.set_phase(SessionPhase::Reconnecting, Some("late".into()), "late");
        let late: Arc<dyn Driver> = FakeDriver::new(Platform::Desktop, 800, 600);
        assert!(!inner.install(&late, 99));
        assert!(!inner.ask_credentials(false, false, "late".into(), true));
        assert_eq!(harness.session.phase(), SessionPhase::Failed);
        let summary = harness.session.summary();
        assert_eq!(summary.state, SessionPhase::Failed);
        assert_eq!(
            summary.message.as_deref(),
            Some("the virtual machine stopped")
        );

        // A session ended while ready: failed and closed in one step, whatever the reader says.
        let other = FakeDriver::new(Platform::Desktop, 800, 600);
        let ended = connected(control(Protocol::Vnc), other.clone());
        ended.session.end("the entry \"vm-proc\" stopped");
        ended.emit(
            0,
            DriverEvent::Disconnected("the VNC connection was closed".into()),
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(ended.session.phase(), SessionPhase::Failed);
        assert!(ended.session.inner.is_closed());
        assert_eq!(ended.events.lock().unwrap().len(), 1, "no reconnect");
        let error = ended.session.screenshot().unwrap_err();
        assert!(error.contains("\"vm-proc\" stopped"), "{error}");
        assert!(
            error.contains("preview_logs with serverId \"pixel\""),
            "{error}"
        );
        // A second end keeps the first reason.
        ended.session.end("something else");
        assert_eq!(
            ended.session.failure().as_deref(),
            Some("the entry \"vm-proc\" stopped")
        );
    }

    /// A session that gave up reconnecting forgets the password typed for its reconnects
    /// instead of keeping it for as long as the failed row is kept.
    #[test]
    fn a_failed_session_forgets_the_typed_password() {
        let driver = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = start(
            control(Protocol::Vnc),
            None,
            Arc::default(),
            move |attempt, auth| {
                let password = auth.and_then(|auth| auth.password.as_deref().map(String::as_str));
                match (attempt, password) {
                    (0, None) => Err(DriverError::Credentials {
                        retry: false,
                        needs_username: false,
                        message: "VNC authentication is required.".into(),
                    }),
                    (1, Some("typed")) => Ok(driver.clone()),
                    _ => Err(DriverError::Failed("the server is gone".into())),
                }
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Credentials
        );
        harness.backend().credentials(sign_in(None, "typed", false));
        eventually("ready", || harness.session.phase() == SessionPhase::Ready);
        assert!(lock(&harness.session.inner.state).auth.is_some());
        harness.emit(1, DriverEvent::Disconnected("the server went away".into()));
        eventually("the failure", || {
            harness.session.phase() == SessionPhase::Failed
        });
        let state = lock(&harness.session.inner.state);
        assert!(state.auth.is_none() && state.answer.is_none());
    }

    /// A session stopped while its driver is still on its way in (an emulator booting) tells
    /// the driver, which gives up instead of polling on beside the session that replaced it.
    #[test]
    fn closing_a_session_stops_a_connect_still_under_way() {
        let gave_up = Arc::new(AtomicBool::new(false));
        let connect: ConnectFn = {
            let gave_up = gave_up.clone();
            Arc::new(move |_target, connector, _auth, _events| {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !connector.cancelled() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                gave_up.store(connector.cancelled(), Ordering::SeqCst);
                Err(DriverError::Failed("stopped while waiting".into()))
            })
        };
        let session = RemoteControlSession::start_with(
            SessionSpec {
                name: "pixel".into(),
                server_id: "pixel".into(),
                target: control(Protocol::Adb),
                connector: Arc::new(FakeConnector),
                preflight: None,
                on_change: Arc::new(|| {}),
                log: Arc::new(|_| {}),
                connect: Some(connect),
            },
            Arc::new(MemoryStore::default()),
            fast(),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(!gave_up.load(Ordering::SeqCst));
        session.close();
        eventually("the connect to give up", || gave_up.load(Ordering::SeqCst));
    }

    /// Stopping a session does not wait for a gesture the pane is in the middle of.
    #[test]
    fn closing_does_not_wait_for_a_viewer_gesture_in_flight() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        driver.slow_down(Duration::from_millis(1500));
        let gesture = {
            let session = harness.session.clone();
            thread::spawn(move || {
                session.inner.viewer_input(
                    "page",
                    vec![
                        pointer(PointerPhase::Down, 0.5, 0.5, 1),
                        pointer(PointerPhase::Up, 0.5, 0.5, 0),
                    ],
                );
            })
        };
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        harness.session.close();
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "closing waited {:?}",
            started.elapsed()
        );
        gesture.join().unwrap();
    }

    // -------------------------------------------------------- the input lock

    /// The person holds Ctrl and the mouse button in the pane when the model's batch starts:
    /// both come up first, so the model's typing is not Ctrl+letters and its click does not end
    /// the person's drag at the model's point.
    #[test]
    fn the_models_batch_lets_go_of_what_the_person_holds() {
        let driver = FakeDriver::new(Platform::Desktop, 100, 100);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let backend = harness.backend();
        backend.input(
            "page",
            vec![
                key("Control", "ControlLeft", true),
                pointer(PointerPhase::Down, 0.5, 0.5, 1),
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Key(Key::Named(NamedKey::Control), Some(true)),
                Call::Pointer(50, 50, 1)
            ]
        );
        let report = harness
            .session
            .run_input(&actions(json!([{"action": "type", "text": "hi"}])), &|| false)
            .unwrap();
        assert_eq!(report.lines, ["1. type \"hi\": ok"]);
        assert_eq!(
            driver.take(),
            [
                Call::Pointer(50, 50, 0),
                Call::Key(Key::Named(NamedKey::Control), Some(false)),
                Call::Text("hi".into())
            ]
        );
        // Nothing is held any more: the page leaving sends nothing.
        backend.viewer_gone("page");
        backend.release_viewer("page");
        assert!(driver.take().is_empty());
    }

    /// A page that leaves while the model drags lets go of nothing in the middle of the drag:
    /// the model let go of the page's keys as it took the screen.
    #[test]
    fn a_page_leaving_mid_batch_does_not_cut_into_the_models_drag() {
        let driver = FakeDriver::new(Platform::Desktop, 100, 100);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        harness
            .backend()
            .input("page", vec![key("Shift", "ShiftLeft", true)]);
        driver.take();
        let session = harness.session.clone();
        let batch = thread::spawn(move || {
            session
                .run_input(
                    &actions(json!([
                        {"action": "drag", "x": 10, "y": 10, "toX": 90, "toY": 90, "ms": 400}
                    ])),
                    &|| false,
                )
                .unwrap()
        });
        eventually("the model's batch", || {
            harness.backend().state().model_active
        });
        let started = Instant::now();
        harness.backend().viewer_gone("page");
        harness.backend().release_viewer("page");
        assert!(started.elapsed() < Duration::from_millis(200));
        assert_eq!(batch.join().unwrap().lines.len(), 1);
        let calls = driver.take();
        assert_eq!(
            calls[0],
            Call::Key(Key::Named(NamedKey::Shift), Some(false))
        );
        let releases = calls
            .iter()
            .filter(|call| matches!(call, Call::Pointer(_, _, 0)))
            .count();
        assert_eq!(releases, 2, "{calls:?}");
        assert_eq!(calls.last(), Some(&Call::Pointer(90, 90, 0)));
    }

    /// A batch that waited for the screen while the connection was replaced runs on the new
    /// connection, not on the closed driver it would have taken before waiting.
    #[test]
    fn a_batch_that_waited_through_a_reconnect_uses_the_new_connection() {
        let first = FakeDriver::new(Platform::Android, 1080, 2400);
        let second = FakeDriver::new(Platform::Android, 1080, 2400);
        let drivers = [first.clone(), second.clone()];
        let harness = start(
            control(Protocol::Adb),
            None,
            Arc::default(),
            move |attempt, _| {
                drivers
                    .get(attempt)
                    .cloned()
                    .ok_or_else(|| DriverError::Failed("unexpected connect".into()))
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        let holder = {
            let session = harness.session.clone();
            thread::spawn(move || {
                session
                    .run_input(&actions(json!([{"action": "wait", "ms": 600}])), &|| false)
                    .unwrap()
            })
        };
        eventually("the first batch", || {
            harness.backend().state().model_active
        });
        let waiter = {
            let session = harness.session.clone();
            thread::spawn(move || {
                session
                    .run_input(
                        &actions(json!([{"action": "click", "x": 288, "y": 640}])),
                        &|| false,
                    )
                    .unwrap()
            })
        };
        thread::sleep(Duration::from_millis(50));
        harness.emit(0, DriverEvent::Disconnected("the device went away".into()));
        holder.join().unwrap();
        assert_eq!(waiter.join().unwrap().lines, ["1. click (288, 640): ok"]);
        assert_eq!(second.take(), [Call::Tap(540, 1200, 1)]);
        assert!(!first
            .take()
            .iter()
            .any(|call| matches!(call, Call::Tap(..))));
    }

    // --------------------------------------------------------- the deadline

    /// A batch that cannot finish before its call's deadline runs what fits and says where it
    /// stopped; nothing starts that would end past the deadline.
    #[test]
    fn a_batch_stops_in_time_and_reports_what_ran() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let session = &harness.session;
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(500),
            cancelled: &never,
        };
        let report = session
            .run_input_within(
                &actions(json!([
                    {"action": "click", "x": 1, "y": 1},
                    {"action": "wait", "ms": 300},
                    {"action": "wait", "ms": 300},
                    {"action": "click", "x": 2, "y": 2}
                ])),
                &call,
            )
            .unwrap();
        assert!(Instant::now() < call.deadline);
        assert_eq!(
            report.lines,
            [
                "1. click (1, 1): ok",
                "2. wait 300 ms: ok",
                "stopped: out of time after action 2 (actions 3-4 did not run)"
            ]
        );
        assert_eq!(driver.take(), [Call::Tap(2, 2, 1)]);

        // Long typing on a slow phone stops between pieces, and says how far it got.
        driver.slow_down(Duration::from_millis(150));
        let text = "a".repeat(1000);
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(400),
            cancelled: &never,
        };
        let report = session
            .run_input_within(
                &[InputAction::Type {
                    text: text.clone(),
                    via: TypeVia::Auto,
                }],
                &call,
            )
            .unwrap();
        assert!(Instant::now() < call.deadline);
        let line = &report.lines[0];
        assert!(
            line.contains("stopped: out of time after typing") && line.ends_with("of 1000 characters"),
            "{line}"
        );
        let typed: usize = driver
            .take()
            .iter()
            .map(|call| match call {
                Call::Text(piece) => piece.len(),
                _ => 0,
            })
            .sum();
        assert!(typed > 0 && typed < 1000 && line.contains(&format!("typing {typed} of")));
        driver.slow_down(Duration::ZERO);

        // No time left at all: nothing runs, and the screenshot is not taken.
        let call = CallContext {
            deadline: Instant::now() + Duration::from_millis(100),
            cancelled: &never,
        };
        let report = session
            .run_input_within(&actions(json!([{"action": "click", "x": 1, "y": 1}])), &call)
            .unwrap();
        assert_eq!(
            report.lines,
            ["stopped: out of time before action 1 (action 1 did not run)"]
        );
        let error = session.screenshot_within(&call).unwrap_err();
        assert!(error.starts_with("Ran out of time"), "{error}");
        assert!(driver.take().is_empty());
    }

    /// Waiting for another batch is bounded by the call's deadline, not only by the lock wait.
    #[test]
    fn waiting_for_the_screen_ends_with_the_calls_time() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let holder = {
            let session = harness.session.clone();
            thread::spawn(move || {
                session
                    .run_input(&actions(json!([{"action": "wait", "ms": 1500}])), &|| false)
                    .unwrap()
            })
        };
        eventually("the first batch", || {
            harness.backend().state().model_active
        });
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(300),
            cancelled: &never,
        };
        let error = harness
            .session
            .run_input_within(&actions(json!([{"action": "click", "x": 1, "y": 1}])), &call)
            .unwrap_err();
        assert!(Instant::now() < call.deadline);
        assert!(error.contains("still running"), "{error}");
        holder.join().unwrap();
        assert!(driver.take().is_empty());
    }

    /// A stopped turn stops a wait halfway, keeps a uid click from happening, and stops typing
    /// between pieces, saying how much went in.
    #[test]
    fn a_cancelled_call_stops_waits_clicks_and_typing() {
        let driver = FakeDriver::with_tree(
            Platform::Android,
            1080,
            2400,
            Some(UiNode {
                role: "EditText".into(),
                name: "Search".into(),
                bounds: Some(Rect {
                    x: 0,
                    y: 0,
                    width: 1080,
                    height: 200,
                }),
                ..UiNode::default()
            }),
        );
        let harness = connected(control(Protocol::Adb), driver.clone());
        let session = &harness.session;
        let flag = Arc::new(AtomicBool::new(false));
        let raise = |after: Duration| {
            let flag = flag.clone();
            flag.store(false, Ordering::SeqCst);
            thread::spawn(move || {
                thread::sleep(after);
                flag.store(true, Ordering::SeqCst);
            })
        };
        let cancelled = || flag.load(Ordering::SeqCst);
        let call = CallContext {
            deadline: Instant::now() + Duration::from_secs(20),
            cancelled: &cancelled,
        };
        let raising = raise(Duration::from_millis(400));
        let started = Instant::now();
        let report = session
            .run_input_within(
                &actions(json!([
                    {"action": "wait", "ms": 3000},
                    {"action": "click", "x": 1, "y": 1}
                ])),
                &call,
            )
            .unwrap();
        raising.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            report.lines,
            ["1. wait 3000 ms: stopped: cancelled (action 2 did not run)"]
        );

        session.snapshot().unwrap();
        let error = session
            .click_uid_within(1_000_001, false, &call)
            .unwrap_err();
        assert!(error.starts_with("Cancelled before clicking"), "{error}");
        assert!(driver.take().is_empty());

        driver.slow_down(Duration::from_millis(150));
        let raising = raise(Duration::from_millis(600));
        let error = session
            .fill_uid_within(1_000_001, &"b".repeat(1000), &call)
            .unwrap_err();
        raising.join().unwrap();
        assert!(
            error.contains("typed") && error.contains("of 1000 characters, then the call was cancelled"),
            "{error}"
        );
        assert_eq!(driver.take()[0], Call::Tap(540, 100, 1));

        // Text the device cannot type is refused before the field is even clicked.
        flag.store(false, Ordering::SeqCst);
        driver.slow_down(Duration::ZERO);
        let error = session
            .fill_uid_within(1_000_001, &format!("{}\u{7}", "c".repeat(300)), &call)
            .unwrap_err();
        assert!(error.contains("Nothing was clicked or typed"), "{error}");
        assert!(driver.take().is_empty());
    }

    // ------------------------------------------------- what the model reads

    /// The pane is hidden, so nothing pulls frames, and the model's own tap turns the phone. Input
    /// in the old frame is refused in words a model without images can act on, and a snapshot
    /// measures the turned screen.
    #[test]
    fn a_phone_turned_by_input_is_measured_again_without_a_viewer() {
        let driver = FakeDriver::pulling(
            Platform::Android,
            1080,
            2400,
            Some(UiNode {
                role: "Button".into(),
                name: "OK".into(),
                bounds: Some(Rect {
                    x: 2000,
                    y: 500,
                    width: 200,
                    height: 100,
                }),
                ..UiNode::default()
            }),
        );
        let harness = connected(control(Protocol::Adb), driver.clone());
        let session = &harness.session;
        session.screenshot().unwrap();
        session
            .run_input(
                &actions(json!([{"action": "click", "x": 288, "y": 640}])),
                &|| false,
            )
            .unwrap();
        driver.take();
        // The tap turned it; nothing has captured since.
        driver.resize(2400, 1080);
        let error = session
            .run_input(
                &actions(json!([{"action": "click", "x": 10, "y": 10}])),
                &|| false,
            )
            .unwrap_err();
        assert!(
            error.contains("2400x1080") && error.contains("Take a new preview_screenshot or preview_snapshot"),
            "{error}"
        );
        let snapshot = session.snapshot().unwrap();
        assert!(snapshot.starts_with("Screen 1280x576"), "{snapshot}");
        assert!(snapshot.contains("[1000001] Button: \"OK\""), "{snapshot}");
        assert_eq!(
            session.summary().screen,
            Some(ScreenSize {
                width: 2400,
                height: 1080
            })
        );
        session.click_uid(1_000_001, false).unwrap();
        assert_eq!(driver.take(), [Call::Tap(2100, 550, 1)]);
    }

    /// preview_input's own screenshot can be taken while the phone is still turning: it comes after
    /// the tap, so it is trusted, but it has the old size. The tree read after the turn is wider
    /// than that screen, so the snapshot measures the screen again (and reads the tree again on
    /// it), and its uids land where they are. An element really off the screen says how to look
    /// again.
    #[test]
    fn a_snapshot_wider_than_a_capture_taken_mid_turn_measures_the_screen_again() {
        let window = |width: u32, height: u32, children: Vec<UiNode>| UiNode {
            role: "FrameLayout".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width,
                height,
            }),
            children,
            ..UiNode::default()
        };
        let button = |name: &str, x: i32| UiNode {
            role: "Button".into(),
            name: name.into(),
            bounds: Some(Rect {
                x,
                y: 500,
                width: 200,
                height: 100,
            }),
            ..UiNode::default()
        };
        let driver = FakeDriver::pulling(
            Platform::Android,
            1080,
            2400,
            Some(window(1080, 2400, Vec::new())),
        );
        let harness = connected(control(Protocol::Adb), driver.clone());
        let session = &harness.session;
        session.screenshot().unwrap();
        session
            .run_input(
                &actions(json!([{"action": "click", "x": 288, "y": 640}])),
                &|| false,
            )
            .unwrap();
        // The screenshot preview_input takes after the tap, before the turn finished.
        session.screenshot().unwrap();
        driver.take();
        // The turn finishes; nothing captures.
        driver.resize(2400, 1080);
        driver.show(window(
            2400,
            1080,
            vec![button("OK", 2000), button("Past the edge", 2500)],
        ));
        let reads = driver.tree_reads.load(Ordering::SeqCst);
        let snapshot = session.snapshot().unwrap();
        assert!(snapshot.starts_with("Screen 1280x576"), "{snapshot}");
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), reads + 2);
        assert_eq!(
            session.summary().screen,
            Some(ScreenSize {
                width: 2400,
                height: 1080
            })
        );
        let uid = |label: &str| -> u64 {
            let line = snapshot
                .lines()
                .find(|line| line.contains(label))
                .unwrap_or_else(|| panic!("{label} in {snapshot}"));
            line[line.find('[').unwrap() + 1..line.find(']').unwrap()]
                .parse()
                .unwrap()
        };
        session.click_uid(uid("\"OK\""), false).unwrap();
        assert_eq!(driver.take(), [Call::Tap(2100, 550, 1)]);
        let error = session.click_uid(uid("Past the edge"), false).unwrap_err();
        assert!(
            error.contains("off the screen (2400x1080 pixels)")
                && error.contains("take a new preview_screenshot or preview_snapshot"),
            "{error}"
        );
        assert!(driver.take().is_empty());

        // A tree that fits the screen is read once, with no capture of its own.
        driver.show(window(2400, 1080, vec![button("OK", 2000)]));
        session.screenshot().unwrap();
        let (reads, captures) = (
            driver.tree_reads.load(Ordering::SeqCst),
            driver.captures.load(Ordering::SeqCst),
        );
        session.snapshot().unwrap();
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), reads + 1);
        assert_eq!(driver.captures.load(Ordering::SeqCst), captures);
    }

    /// Hints name the row, since preview_logs without a serverId reads another; an iPhone's
    /// receipt offers only the keys it has; the model reads the place in English.
    #[test]
    fn receipts_name_the_row_and_the_keys_each_phone_has() {
        let slow = FakeDriver::new(Platform::Android, 1080, 2400);
        let connecting = start(control(Protocol::Adb), None, Arc::default(), move |_, _| {
            thread::sleep(Duration::from_millis(500));
            Ok(slow.clone())
        });
        let receipt = connecting.session.receipt();
        assert!(receipt.starts_with("Still connecting"), "{receipt}");
        assert!(
            receipt.contains("preview_logs with serverId \"pixel\" shows the progress"),
            "{receipt}"
        );

        let android = connected(
            control(Protocol::Adb),
            FakeDriver::new(Platform::Android, 1080, 2400),
        );
        let receipt = android.session.receipt();
        assert!(receipt.contains("Back") && receipt.contains("AppSwitch"), "{receipt}");
        // Each receipt names the screen tools that reach its platform, in words true whether
        // they are on or not.
        assert!(
            receipt.ends_with(" When the screen tools are on, screen_app, screen_device, screen_package, preview_zoom and preview_wait also act on this device, and android_logcat and android_shell when the Android tools are on."),
            "{receipt}"
        );
        assert!(!receipt.contains("read-only"), "{receipt}");
        let desk = connected(
            control(Protocol::Vnc),
            FakeDriver::new(Platform::Desktop, 800, 600),
        );
        let receipt = desk.session.receipt();
        assert!(
            receipt.ends_with(" When the screen tools are on, screen_clipboard, preview_zoom and preview_wait also act on it. Text its keys cannot type goes through preview_input's type with via \"paste\"."),
            "{receipt}"
        );
        let iphone = connected(
            control(Protocol::Wda),
            FakeDriver::new(Platform::Ios, 1170, 2532),
        );
        let receipt = iphone.session.receipt();
        assert!(
            receipt.ends_with(" When the screen tools are on, screen_app, screen_device, screen_clipboard, preview_zoom and preview_wait also act on this device."),
            "{receipt}"
        );
        assert!(
            !receipt.contains("Back,") && !receipt.contains("Back ") && !receipt.contains("AppSwitch"),
            "{receipt}"
        );
        assert!(receipt.contains("Home for the home screen"), "{receipt}");
        assert!(receipt.contains("on lab (127.0.0.1:8100)"), "{receipt}");
        assert_eq!(iphone.session.summary().target, "lab · 127.0.0.1:8100");
    }

    /// The row says why it is reconnecting, then why it failed.
    #[test]
    fn the_summary_carries_the_reason_while_reconnecting_and_once_failed() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = start(
            control(Protocol::Adb),
            None,
            Arc::default(),
            move |attempt, _| {
                if attempt == 0 {
                    Ok(driver.clone())
                } else {
                    thread::sleep(Duration::from_millis(300));
                    Err(DriverError::Failed("the device is offline".into()))
                }
            },
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        assert_eq!(harness.session.summary().message, None);
        harness.emit(0, DriverEvent::Disconnected("the device went away".into()));
        eventually("reconnecting", || {
            harness.session.phase() == SessionPhase::Reconnecting
        });
        assert_eq!(
            harness.session.summary().message.as_deref(),
            Some("the device went away")
        );
        let receipt = harness.session.receipt();
        assert!(
            receipt.contains("preview_logs with serverId \"pixel\""),
            "{receipt}"
        );
        eventually("the failure", || {
            harness.session.phase() == SessionPhase::Failed
        });
        let message = harness.session.summary().message.unwrap();
        assert!(message.contains("after 5 attempts"), "{message}");
        let summary = serde_json::to_value(harness.session.summary()).unwrap();
        assert_eq!(summary["message"], json!(message));
    }

    // ---------------------------------------------------- the person's input

    /// A desktop keyboard typing into a phone's pane: characters go as text (capitals and
    /// punctuation included), a lone Shift sends nothing, and Ctrl+key is a combination.
    #[test]
    fn typing_in_the_pane_reaches_a_phone_as_text() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        harness.backend().input(
            "page",
            vec![
                key("Shift", "ShiftLeft", true),
                key("H", "KeyH", true),
                key("H", "KeyH", false),
                key("Shift", "ShiftLeft", false),
                key("i", "KeyI", true),
                key("i", "KeyI", false),
                key(",", "Comma", true),
                key(" ", "Space", true),
                key("!", "Digit1", true),
                key("Enter", "Enter", true),
                key("Control", "ControlLeft", true),
                key("a", "KeyA", true),
                key("Control", "ControlLeft", false),
                key("b", "KeyB", true),
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Text("H".into()),
                Call::Text("i".into()),
                Call::Text(",".into()),
                Call::Key(Key::Named(NamedKey::Space), None),
                Call::Text("!".into()),
                Call::Key(Key::Named(NamedKey::Enter), None),
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Control)],
                    key: Key::Char('a')
                }),
                Call::Text("b".into()),
            ]
        );
    }

    /// AltGr on a German, French or Polish layout makes characters, not shortcuts, and Windows
    /// reports a Control press with it. Those characters reach a phone as text, from a page that
    /// says AltGr is held and from one that does not; Ctrl+Alt with a letter is still a
    /// combination.
    #[test]
    fn altgr_characters_reach_a_phone_as_text() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        harness.backend().input(
            "page",
            vec![
                // Windows: Control, then AltGraph, then the character, flagged by the page.
                key("Control", "ControlLeft", true),
                key("AltGraph", "AltRight", true),
                alt_graph_key("@", "KeyQ"),
                key("@", "KeyQ", false),
                alt_graph_key("€", "KeyE"),
                alt_graph_key("Enter", "Enter"),
                key("AltGraph", "AltRight", false),
                key("Control", "ControlLeft", false),
                // A page that does not send altGraph. Linux: AltGraph alone.
                key("AltGraph", "AltRight", true),
                key("{", "Digit7", true),
                key("AltGraph", "AltRight", false),
                // Windows: Control and AltGraph.
                key("Control", "ControlLeft", true),
                key("AltGraph", "AltRight", true),
                key("ą", "KeyA", true),
                key("AltGraph", "AltRight", false),
                key("Control", "ControlLeft", false),
                // A Mac's Option composes the same way.
                key("Alt", "AltLeft", true),
                key("@", "KeyL", true),
                key("Alt", "AltLeft", false),
                // Ctrl+Alt+T is a shortcut.
                key("Control", "ControlLeft", true),
                key("Alt", "AltLeft", true),
                key("t", "KeyT", true),
                key("Alt", "AltLeft", false),
                key("Control", "ControlLeft", false),
                key("x", "KeyX", true),
            ],
        );
        assert_eq!(
            driver.take(),
            [
                Call::Text("@".into()),
                Call::Text("€".into()),
                Call::Key(Key::Named(NamedKey::Enter), None),
                Call::Text("{".into()),
                Call::Text("ą".into()),
                Call::Text("@".into()),
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Alt)],
                    key: Key::Char('t')
                }),
                Call::Text("x".into()),
            ]
        );
    }

    /// The page's down and up can arrive in one batch, queued behind a slow gesture: the hold is
    /// what the page's clock says, not what arrival says.
    #[test]
    fn a_touch_is_timed_by_the_pages_clock_when_it_sends_one() {
        let driver = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), driver.clone());
        let backend = harness.backend();
        backend.input(
            "page",
            vec![
                pointer_at(PointerPhase::Down, 0.5, 0.5, 1000.0),
                pointer_at(PointerPhase::Up, 0.5, 0.5, 1800.0),
            ],
        );
        assert_eq!(driver.take(), [Call::LongPress(540, 1200, 800)]);
        backend.input(
            "page",
            vec![
                pointer_at(PointerPhase::Down, 0.5, 0.8, 5000.0),
                pointer_at(PointerPhase::Move, 0.5, 0.5, 5300.0),
                pointer_at(PointerPhase::Up, 0.5, 0.3, 5600.0),
            ],
        );
        assert_eq!(
            driver.take(),
            [Call::Swipe((540, 1920), (540, 720), 600)]
        );
        // A clock that ran backwards is no hold at all.
        backend.input(
            "page",
            vec![
                pointer_at(PointerPhase::Down, 0.5, 0.5, 900.0),
                pointer_at(PointerPhase::Up, 0.5, 0.5, 100.0),
            ],
        );
        assert_eq!(driver.take(), [Call::Tap(540, 1200, 1)]);
    }

    #[test]
    fn text_is_cut_into_pieces_but_never_between_a_carriage_return_and_its_line_feed() {
        assert_eq!(text_pieces("abcdef", 2), ["ab", "cd", "ef"]);
        assert_eq!(text_pieces("a\r\nb", 2), ["a\r\n", "b"]);
        assert_eq!(text_pieces("中文字", 2), ["中文", "字"]);
        assert_eq!(text_pieces("", 2), Vec::<&str>::new());
        assert_eq!(not_run(3, 5), " (actions 3-5 did not run)");
        assert_eq!(not_run(5, 5), " (action 5 did not run)");
        assert_eq!(not_run(6, 5), "");
    }

    // ---------------------------------------- type's via and chord, and mod, in a batch

    /// `type` reads `via` ("auto" by default) and, with "paste", the shortcut; each way has its
    /// own limit on the text, and `chord` goes only with "paste".
    #[test]
    fn type_reads_its_via_and_chord_with_the_limits_of_each_way() {
        let type_with = |extra: Value| {
            let mut action = json!({"action": "type", "text": "hello"});
            for (key, value) in extra.as_object().unwrap() {
                action[key] = value.clone();
            }
            parse_actions(&json!([action]))
        };
        let error = |extra: Value| type_with(extra).unwrap_err();
        let auto = InputAction::Type {
            text: "hello".into(),
            via: TypeVia::Auto,
        };
        for extra in [json!({}), json!({"via": "auto"}), json!({"via": null})] {
            assert_eq!(type_with(extra).unwrap(), [auto.clone()]);
        }
        assert_eq!(
            type_with(json!({"via": "paste"})).unwrap(),
            [InputAction::Type {
                text: "hello".into(),
                via: TypeVia::Paste { chord: None }
            }]
        );
        // The shortcut is read as a key's chord, and kept as the model wrote it.
        let pasted = type_with(json!({"via": "paste", "chord": " Ctrl+Shift+V "})).unwrap();
        assert_eq!(
            pasted[0],
            InputAction::Type {
                text: "hello".into(),
                via: TypeVia::Paste {
                    chord: Some((
                        "Ctrl+Shift+V".into(),
                        keys::parse_chord("ctrl+shift+V", Platform::Desktop).unwrap()
                    ))
                }
            }
        );
        assert_eq!(pasted[0].describe(), "type \"hello\" via paste");
        assert!(type_with(json!({"via": "paste", "chord": "mod+v"})).is_ok());

        // Refusals name the action and the parameter.
        for via in [json!("clipboard"), json!(1), json!("Paste")] {
            assert_eq!(
                error(json!({"via": via})),
                "Action 1 (type): \"via\" must be \"auto\" or \"paste\"."
            );
        }
        for extra in [
            json!({"chord": "ctrl+v"}),
            json!({"via": "auto", "chord": "ctrl+v"}),
        ] {
            assert_eq!(
                error(extra),
                "Action 1 (type): \"chord\" is the shortcut that pastes the text, so it goes with via \"paste\"; leave it out, or add via \"paste\"."
            );
        }
        let chord_error = |chord: Value| error(json!({"via": "paste", "chord": chord}));
        assert!(chord_error(json!(7)).contains("\"chord\" must be a string"));
        assert!(chord_error(json!("  ")).contains("Action 1 (type): \"chord\" is empty"));
        assert!(chord_error(json!("hyper+v"))
            .contains("\"chord\" is not a shortcut: Unknown key \"hyper\""));
        assert!(chord_error(json!("ctrl+Back")).contains("a phone button"));
        assert!(chord_error(json!("v".repeat(65))).contains("must be 1-64 characters"));
        assert!(
            type_with(json!({"via": "paste", "chord": format!("{}v", " ".repeat(63))})).is_ok()
        );
        assert!(error(json!({"clipboard": true})).contains("type takes text, via, chord"));
        let empty = parse_actions(&json!([{"action": "type", "text": "", "via": "paste"}]));
        assert!(empty.unwrap_err().contains("\"text\" is empty"));

        // `via` "auto" takes 2000 characters, held at parse (it needs no screen), and says where
        // longer text goes; the schema's own bound is 60000, which `via` "paste" takes.
        let long = |length: usize, via: &str| {
            parse_actions(&json!([{"action": "type", "text": "中".repeat(length), "via": via}]))
        };
        let too_long_for_auto = "Action 1 (type): \"text\" is longer than 2000 characters; split it over several type actions, or on a desktop use via \"paste\", which takes up to 60000 characters.";
        assert!(long(2_000, "auto").is_ok());
        for length in [2_001, 60_000, 60_001] {
            assert_eq!(long(length, "auto").unwrap_err(), too_long_for_auto);
        }
        // No `via` is `auto`.
        for action in [
            json!({"action": "type", "text": "a".repeat(2_001)}),
            json!({"action": "type", "text": "a".repeat(2_001), "via": null}),
        ] {
            assert_eq!(
                parse_actions(&json!([action])).unwrap_err(),
                too_long_for_auto
            );
        }
        assert!(long(60_000, "paste").is_ok());
        assert_eq!(
            long(60_001, "paste").unwrap_err(),
            "Action 1 (type): \"text\" is longer than 60000 characters; split it over several type actions."
        );
        // The longest paste in the worst characters fits what a VNC server takes.
        assert_eq!(
            parse_actions(
                &json!([{"action": "type", "text": "😀".repeat(60_000), "via": "paste"}])
            )
            .unwrap()
            .len(),
            1
        );
    }

    /// What a batch cannot do on this screen is refused whole, with the lock held and before the
    /// first action: nothing is sent, not even the actions before the one at fault.
    #[test]
    fn a_batch_the_screen_cannot_take_is_refused_whole_before_its_first_action() {
        let batch = |action: Value| actions(json!([{"action": "key", "key": "Enter"}, action]));
        let paste = |text: &str| json!({"action": "type", "text": text, "via": "paste"});

        // `via` "auto" is held to 2000 characters when the batch is read — on every screen, and
        // before any screen is asked, so not even the action before it runs — and 2000 go in.
        assert_eq!(
            parse_actions(&json!([
                {"action": "key", "key": "Enter"},
                {"action": "type", "text": "a".repeat(2_001)}
            ]))
            .unwrap_err(),
            "Action 2 (type): \"text\" is longer than 2000 characters; split it over several type actions, or on a desktop use via \"paste\", which takes up to 60000 characters."
        );
        for (platform, protocol) in [
            (Platform::Android, Protocol::Adb),
            (Platform::Desktop, Protocol::Vnc),
        ] {
            let driver = FakeDriver::new(platform, 800, 600);
            let harness = connected(control(protocol), driver.clone());
            let report = harness
                .session
                .run_input(
                    &batch(json!({"action": "type", "text": "a".repeat(2_000)})),
                    &|| false,
                )
                .unwrap();
            assert_eq!(report.lines.len(), 2, "{:?}", report.lines);
        }

        // A phone's clipboard is out of reach of a type: it says what to do instead.
        let android = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), android.clone());
        let refused = harness
            .session
            .run_input(&batch(paste("hello")), &|| false)
            .unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (type): via \"paste\" needs a desktop: a phone's clipboard is out of reach here; type the text with via \"auto\" (printable ASCII only)."
        );
        assert!(android.take().is_empty());
        let iphone = FakeDriver::new(Platform::Ios, 1170, 2532);
        let harness = connected(control(Protocol::Wda), iphone.clone());
        let refused = harness
            .session
            .run_input(&batch(paste("hello")), &|| false)
            .unwrap_err();
        assert!(
            refused.starts_with("Action 2 (type): via \"paste\" needs a desktop: a phone's clipboard is out of reach here;")
                && refused.contains("screen_clipboard"),
            "{refused}"
        );
        assert!(iphone.take().is_empty());

        // A Latin-1 clipboard names the character it cannot carry; nothing was written.
        let latin1 = FakeDriver::vnc_with(ClipboardChannel::Latin1, false);
        let harness = connected(control(Protocol::Vnc), latin1.clone());
        let refused = harness
            .session
            .run_input(&batch(paste("café 😀")), &|| false)
            .unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (type): This VNC server takes only Latin-1 clipboard text (it lacks the Extended Clipboard extension), so text with \"😀\" cannot go on its clipboard. Type it with via \"auto\" instead, as key presses, which not every server takes."
        );
        assert!(latin1.take().is_empty());
        assert!(latin1.desktop.as_ref().unwrap().writes().is_empty());
        // The same text is fine where the channel carries it, and Latin-1 text on Latin-1.
        let report = harness
            .session
            .run_input(&batch(paste("café")), &|| false)
            .unwrap();
        assert!(report.lines[1].contains("pasted 4 characters with ctrl+v"));

        // Past what a VNC server's clipboard takes: refused, though the parser admits it.
        let extended = FakeDriver::vnc_with(ClipboardChannel::Extended, false);
        let harness = connected(control(Protocol::Vnc), extended.clone());
        let too_much = [
            actions(json!([{"action": "key", "key": "Enter"}]))[0].clone(),
            InputAction::Type {
                text: "a".repeat(60_001),
                via: TypeVia::Paste { chord: None },
            },
        ];
        let refused = harness.session.run_input(&too_much, &|| false).unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (type): \"text\" has 60001 characters; at most 60000 fit on a desktop's clipboard."
        );
        assert!(extended.take().is_empty());

        // A desktop whose driver offers no clipboard cannot paste.
        let bare = FakeDriver::new(Platform::Desktop, 800, 600);
        let harness = connected(control(Protocol::Vnc), bare.clone());
        let refused = harness
            .session
            .run_input(&batch(paste("hello")), &|| false)
            .unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (type): via \"paste\" needs a desktop connected over VNC; this screen is not one."
        );
        assert!(bare.take().is_empty());
    }

    /// A paste writes the clipboard, waits for the server, and presses the desktop's shortcut —
    /// all inside the batch's lock, in the batch's order — and its line says what the clipboard
    /// holds now.
    #[test]
    fn a_paste_writes_the_clipboard_then_presses_the_default_shortcut() {
        let ctrl_v = KeyChord {
            modifiers: vec![Key::Named(NamedKey::Control)],
            key: Key::Char('v'),
        };
        let driver = FakeDriver::vnc_with(ClipboardChannel::Extended, false);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let started = Instant::now();
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "key", "key": "Escape"},
                    {"action": "type", "text": "你好, world", "via": "paste"},
                    {"action": "type", "text": "tail"}
                ])),
                &|| false,
            )
            .unwrap();
        assert!(started.elapsed() >= desktop::PASTE_PAUSE);
        assert_eq!(
            report.lines,
            [
                "1. key Escape: ok",
                "2. type \"你好, world\" via paste: pasted 9 characters with ctrl+v (the desktop's clipboard now holds them)",
                "3. type \"tail\": ok"
            ]
        );
        assert_eq!(driver.desktop.as_ref().unwrap().writes(), ["你好, world"]);
        assert_eq!(
            driver.take(),
            [
                Call::Key(Key::Named(NamedKey::Escape), None),
                Call::Chord(ctrl_v),
                Call::Text("tail".into())
            ]
        );

        // Apple's server pastes with Command; a chord the model names is pressed as written.
        let mac = FakeDriver::vnc_with(ClipboardChannel::Latin1, true);
        let harness = connected(control(Protocol::Vnc), mac.clone());
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "type", "text": "ok", "via": "paste"},
                    {"action": "type", "text": "ls", "via": "paste", "chord": "Ctrl+Shift+V"},
                    {"action": "type", "text": "x", "via": "paste", "chord": "Insert"}
                ])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            [
                "1. type \"ok\" via paste: pasted 2 characters with cmd+v (the desktop's clipboard now holds them)",
                "2. type \"ls\" via paste: pasted 2 characters with Ctrl+Shift+V (the desktop's clipboard now holds them)",
                "3. type \"x\" via paste: pasted 1 character with Insert (the desktop's clipboard now holds them)"
            ]
        );
        assert_eq!(
            mac.take(),
            [
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Meta)],
                    key: Key::Char('v')
                }),
                Call::Chord(KeyChord {
                    modifiers: vec![Key::Named(NamedKey::Control), Key::Named(NamedKey::Shift)],
                    key: Key::Char('V')
                }),
                Call::Key(Key::Named(NamedKey::Insert), None)
            ]
        );
        assert_eq!(mac.desktop.as_ref().unwrap().writes(), ["ok", "ls", "x"]);

        // A clipboard that took nothing is no paste: the shortcut is never pressed.
        let failing = FakeDriver::vnc_with(ClipboardChannel::Extended, false);
        failing
            .desktop
            .as_ref()
            .unwrap()
            .fail_with(Some(DriverError::Failed("the server said no".into())));
        let harness = connected(control(Protocol::Vnc), failing.clone());
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "type", "text": "abc", "via": "paste"},
                    {"action": "key", "key": "Enter"}
                ])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            ["1. type \"abc\" via paste: failed: the server said no (action 2 did not run)"]
        );
        assert!(failing.take().is_empty());
    }

    /// The paste runs inside the batch's lock and time: another batch cannot come between its
    /// text and its shortcut, and a call with less left than the pause needs does not begin it.
    #[test]
    fn a_paste_holds_the_input_lock_and_the_calls_time() {
        let driver = FakeDriver::vnc_with(ClipboardChannel::Extended, false);
        let harness = connected(control(Protocol::Vnc), driver.clone());
        let session = harness.session.clone();
        let running = thread::spawn(move || {
            session
                .run_input(
                    &actions(json!([
                        {"action": "wait", "ms": 200},
                        {"action": "type", "text": "held", "via": "paste"}
                    ])),
                    &|| false,
                )
                .unwrap()
        });
        eventually("the batch", || harness.backend().state().model_active);
        assert!(harness.session.inner.input.try_lock().is_err());
        let report = running.join().unwrap();
        assert!(
            report.lines[1].contains("pasted 4 characters"),
            "{:?}",
            report.lines
        );
        assert!(harness.session.inner.input.try_lock().is_ok());

        // With less than the pause left, the paste does not start: the clipboard keeps its
        // text, and the report says where the batch stopped.
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(100),
            cancelled: &never,
        };
        let report = harness
            .session
            .run_input_within(
                &actions(json!([{"action": "type", "text": "late", "via": "paste"}])),
                &call,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            ["stopped: out of time before action 1 (action 1 did not run)"]
        );
        assert_eq!(driver.desktop.as_ref().unwrap().writes(), ["held"]);
    }

    /// `mod` is cmd on Apple's VNC server and ctrl on Android, decided with the lock held and
    /// the connection as it is; any other screen is refused before the first action, naming
    /// the action.
    #[test]
    fn mod_is_resolved_for_the_screen_before_the_first_action() {
        let key_of = |modifiers: &[NamedKey], key: Key| {
            Call::Chord(KeyChord {
                modifiers: modifiers.iter().map(|key| Key::Named(*key)).collect(),
                key,
            })
        };
        // Android: ctrl.
        let phone = FakeDriver::new(Platform::Android, 1080, 2400);
        let harness = connected(control(Protocol::Adb), phone.clone());
        let report = harness
            .session
            .run_input(
                &actions(json!([{"action": "key", "key": "mod+a"}, {"action": "key", "key": "MOD+shift+Z"}])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            ["1. key ctrl+a: ok", "2. key ctrl+shift+Z: ok"]
        );
        assert_eq!(
            phone.take(),
            [
                key_of(&[NamedKey::Control], Key::Char('a')),
                key_of(&[NamedKey::Control, NamedKey::Shift], Key::Char('Z'))
            ]
        );

        // Apple's VNC server: cmd — in a key, in the modifiers held for a click, and in a paste.
        let mac = FakeDriver::vnc_with(ClipboardChannel::Extended, true);
        let harness = connected(control(Protocol::Vnc), mac.clone());
        let report = harness
            .session
            .run_input(
                &actions(json!([
                    {"action": "key", "key": "mod+c"},
                    {"action": "click", "x": 10, "y": 10, "modifiers": "mod+shift"},
                    {"action": "type", "text": "p", "via": "paste", "chord": "Mod+v"}
                ])),
                &|| false,
            )
            .unwrap();
        assert_eq!(
            report.lines,
            [
                "1. key cmd+c: ok",
                "2. click (10, 10) holding cmd+shift: ok",
                "3. type \"p\" via paste: pasted 1 character with cmd+v (the desktop's clipboard now holds them)"
            ]
        );
        let calls = mac.take();
        assert_eq!(calls[0], key_of(&[NamedKey::Meta], Key::Char('c')));
        assert!(calls.contains(&Call::Key(Key::Named(NamedKey::Meta), Some(true))));
        assert!(calls.contains(&Call::Key(Key::Named(NamedKey::Shift), Some(true))));
        assert_eq!(
            *calls.last().unwrap(),
            key_of(&[NamedKey::Meta], Key::Char('v'))
        );
        assert!(!format!("{calls:?}").contains("Mod"));

        // Any other desktop does not say what its system is: refused, before the actions that
        // do not need it too. The model is told to name the key.
        let other = FakeDriver::vnc_with(ClipboardChannel::Extended, false);
        let harness = connected(control(Protocol::Vnc), other.clone());
        let refused = harness
            .session
            .run_input(
                &actions(
                    json!([{"action": "key", "key": "Enter"}, {"action": "key", "key": "mod+c"}]),
                ),
                &|| false,
            )
            .unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (key): mod is cmd on macOS Screen Sharing and ctrl on Android; this desktop's system is unknown, so name ctrl or cmd."
        );
        assert!(other.take().is_empty());
        let refused = harness
            .session
            .run_input(
                &actions(json!([{"action": "click", "x": 1, "y": 1, "modifiers": "mod"}])),
                &|| false,
            )
            .unwrap_err();
        assert!(
            refused.starts_with("Action 1 (click): mod is cmd on macOS"),
            "{refused}"
        );
        let refused = harness
            .session
            .run_input(
                &actions(
                    json!([{"action": "type", "text": "p", "via": "paste", "chord": "mod+v"}]),
                ),
                &|| false,
            )
            .unwrap_err();
        assert!(
            refused.starts_with("Action 1 (type): mod is cmd on macOS"),
            "{refused}"
        );
        assert!(other.take().is_empty());
        assert!(other.desktop.as_ref().unwrap().writes().is_empty());
        // Named keys are fine anywhere.
        harness
            .session
            .run_input(
                &actions(json!([{"action": "key", "key": "ctrl+c"}])),
                &|| false,
            )
            .unwrap();

        // An iPhone takes no chords through WebDriverAgent, mod or not.
        let iphone = FakeDriver::new(Platform::Ios, 1170, 2532);
        let harness = connected(control(Protocol::Wda), iphone.clone());
        let refused = harness
            .session
            .run_input(
                &actions(
                    json!([{"action": "key", "key": "Home"}, {"action": "key", "key": "mod+a"}]),
                ),
                &|| false,
            )
            .unwrap_err();
        assert_eq!(
            refused,
            "Action 2 (key): an iPhone or iPad takes no modifier chords through WebDriverAgent; use the on-screen controls."
        );
        assert!(iphone.take().is_empty());
    }

    // ----------------------------------------------- preview_fill replaces on Android

    /// A phone screen with one field, `EditText "Search"`, holding `text` and having the focus
    /// when `focused`.
    fn field_screen(text: Option<&str>, focused: bool) -> UiNode {
        UiNode {
            role: "FrameLayout".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1080,
                height: 2400,
            }),
            children: vec![UiNode {
                role: "EditText".into(),
                name: "Search".into(),
                text: text.map(str::to_owned),
                bounds: Some(Rect {
                    x: 100,
                    y: 300,
                    width: 880,
                    height: 120,
                }),
                focused,
                enabled: true,
                ..UiNode::default()
            }],
            ..UiNode::default()
        }
    }

    /// A phone on API level `sdk` showing the field, with its snapshot taken (the field is
    /// `[1000001]`, its middle at (540, 360)).
    fn field_phone(
        sdk: u32,
        deleting: bool,
        tweak: impl FnOnce(&mut UiNode),
    ) -> (Arc<FakeDriver>, Harness) {
        let mut tree = field_screen(Some("old text"), true);
        tweak(&mut tree.children[0]);
        let driver = FakeDriver::android_with(sdk, tree, deleting);
        // As a stock dump of a real device: no `hint` attribute, and an emptied field shows its
        // hint as its text.
        driver.empty_fields_show(Some("Search here"));
        let harness = connected(control(Protocol::Adb), driver.clone());
        harness.session.snapshot().unwrap();
        assert!(driver.take().is_empty());
        (driver, harness)
    }

    const FIELD_TAP: Call = Call::Tap(540, 360, 1);

    fn select_all() -> Call {
        Call::Chord(KeyChord {
            modifiers: vec![Key::Named(NamedKey::Control)],
            key: Key::Char('a'),
        })
    }

    fn delete() -> Call {
        Call::Key(Key::Named(NamedKey::Backspace), None)
    }

    /// An Android field's text is replaced: tapped, confirmed to be the focused field, read,
    /// selected and deleted, confirmed empty, and only then typed into. The answer says what
    /// was removed.
    #[test]
    fn a_fill_on_android_replaces_the_text_of_the_field() {
        for sdk in [33, 34] {
            let (driver, harness) = field_phone(sdk, true, |_| {});
            let answer = harness.session.fill_uid(1_000_001, "new").unwrap();
            assert_eq!(
                answer,
                "Clicked [1000001] EditText \"Search\", removed its old text \"old text\" (8 characters) and typed 3 characters."
            );
            assert_eq!(
                driver.take(),
                [FIELD_TAP, select_all(), delete(), Call::Text("new".into())],
                "API {sdk}"
            );
            // The snapshot, the read after the tap, the read that confirms the field is empty.
            assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 3);
        }

        // Clearing is a fill of nothing.
        let (driver, harness) = field_phone(34, true, |_| {});
        assert_eq!(
            harness.session.fill_uid(1_000_001, "").unwrap(),
            "Clicked [1000001] EditText \"Search\", removed its old text \"old text\" (8 characters); typed nothing."
        );
        assert_eq!(driver.take(), [FIELD_TAP, select_all(), delete()]);

        // A field with nothing in it — no text, or only its hint shown as text — is not cleared.
        for tweak in [
            Box::new(|field: &mut UiNode| field.text = None) as Box<dyn FnOnce(&mut UiNode)>,
            Box::new(|field: &mut UiNode| {
                field.text = Some("Search here".into());
                field.hint = Some("Search here".into());
            }),
        ] {
            let (driver, harness) = field_phone(34, true, tweak);
            assert_eq!(
                harness.session.fill_uid(1_000_001, "new").unwrap(),
                "Clicked [1000001] EditText \"Search\" and typed 3 characters; the field was already empty."
            );
            assert_eq!(driver.take(), [FIELD_TAP, Call::Text("new".into())]);
        }
        // A hint is not the text when the field holds something else.
        let (driver, harness) =
            field_phone(34, true, |field| field.hint = Some("Search here".into()));
        assert!(harness
            .session
            .fill_uid(1_000_001, "n")
            .unwrap()
            .contains("removed its old text \"old text\""));
        assert_eq!(driver.take().len(), 4);
    }

    /// Everything that cannot be known or done is refused before a character is deleted, and
    /// the answer says the field was left as it was.
    #[test]
    fn a_fill_on_android_refuses_before_deleting_anything_it_cannot_replace() {
        // Android 12 and older ignore the Ctrl of the select-all chord: refused before the tap.
        let (driver, harness) = field_phone(32, true, |_| {});
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("replacing a field's text needs Android 13 or later here")
                && refused.contains("older versions ignore the select-all chord's Ctrl")
                && refused.contains("The field was left as it was"),
            "{refused}"
        );
        assert!(driver.take().is_empty());

        // Text the device cannot type is refused first, before the tap too.
        let (driver, harness) = field_phone(34, true, |_| {});
        let refused = harness.session.fill_uid(1_000_001, "caf\u{7}").unwrap_err();
        assert!(
            refused.starts_with("Nothing was clicked or typed"),
            "{refused}"
        );
        assert!(driver.take().is_empty());

        // The tap did not focus the field: nothing has the focus, or another element does, or
        // the field moved.
        let (driver, harness) = field_phone(34, true, |field| field.focused = false);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            "Clicked [1000001] EditText \"Search\", but tapping it did not focus it (nothing on the screen has the focus); its text was left as it was and nothing was typed."
        );
        assert_eq!(driver.take(), [FIELD_TAP]);
        let (driver, harness) = field_phone(34, true, |_| {});
        let mut moved = field_screen(Some("old text"), true);
        moved.children[0].bounds = Some(Rect {
            x: 100,
            y: 700,
            width: 880,
            height: 120,
        });
        driver.show(moved);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("tapping it did not focus it (the focused element is EditText \"Search\", or the field moved"),
            "{refused}"
        );
        assert!(refused.ends_with("its text was left as it was and nothing was typed."));
        assert_eq!(driver.take(), [FIELD_TAP]);
        let (driver, harness) = field_phone(34, true, |_| {});
        let mut other = field_screen(Some("old text"), true);
        other.children[0].role = "Button".into();
        driver.show(other);
        assert!(harness
            .session
            .fill_uid(1_000_001, "new")
            .unwrap_err()
            .contains("the focused element is Button \"Search\""));
        assert_eq!(driver.take(), [FIELD_TAP]);

        // A text that is not all known cannot be replaced: cut short by the reading, or hidden.
        let (driver, harness) = field_phone(34, true, |field| field.truncated = true);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("longer than the reading of the screen keeps")
                && refused.ends_with("its text was left as it was and nothing was typed."),
            "{refused}"
        );
        assert_eq!(driver.take(), [FIELD_TAP]);
        let (driver, harness) = field_phone(34, true, |field| {
            field.password = true;
            field.text = Some("••••••••".into());
        });
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(refused.contains("it is a password field"), "{refused}");
        assert_eq!(driver.take(), [FIELD_TAP]);

        // A device that took no notice of the keys leaves the text there: nothing is typed over it.
        let (driver, harness) = field_phone(34, false, |_| {});
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("it still shows \"old text\" (8 characters)")
                && refused.ends_with(
                    "To fill a field that is empty, tap it with preview_click and type with preview_input."
                ),
            "{refused}"
        );
        assert_eq!(driver.take(), [FIELD_TAP, select_all(), delete()]);

        // Not enough time to clear the field and type into it: nothing is touched. (A fill needs
        // a few seconds on a slow device; this call has one.)
        let (driver, harness) = field_phone(34, true, |_| {});
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_secs(1),
            cancelled: &never,
        };
        let refused = harness
            .session
            .fill_uid_within(1_000_001, &"n".repeat(500), &call)
            .unwrap_err();
        assert!(
            refused.starts_with(
                "Nothing was clicked, deleted or typed: replacing the field's text takes about "
            ) && refused.contains("typing 500 characters")
                && refused.contains("The field was left as it was"),
            "{refused}"
        );
        assert!(driver.take().is_empty());
        // The same fill with the time it needs goes through.
        assert!(harness
            .session
            .fill_uid(1_000_001, &"n".repeat(500))
            .is_ok());
    }

    /// A stock `uiautomator` dump has no `hint` attribute: an empty field shows its hint as its
    /// text. So the read that confirms the deletion is told by what it shows: nothing, or a text
    /// that is not the old one, is an empty field (typed into); the old text again is a field
    /// whose keys were ignored or one that was empty all along, and nothing is typed over it; a
    /// part of the old text is a deletion that stopped half way.
    #[test]
    fn a_fill_on_android_tells_an_emptied_field_from_one_that_kept_its_text() {
        let answer = "Clicked [1000001] EditText \"Search\", removed its old text \"old text\" (8 characters) and typed 3 characters.";

        // The field shows nothing once emptied: cleared.
        let (driver, harness) = field_phone(34, true, |_| {});
        driver.empty_fields_show(None);
        assert_eq!(harness.session.fill_uid(1_000_001, "new").unwrap(), answer);
        assert_eq!(
            driver.take(),
            [FIELD_TAP, select_all(), delete(), Call::Text("new".into())]
        );

        // It shows its hint as its text, which is not the old text: the field is empty.
        let (driver, harness) = field_phone(34, true, |_| {});
        assert_eq!(harness.session.fill_uid(1_000_001, "new").unwrap(), answer);
        assert_eq!(
            driver.take(),
            [FIELD_TAP, select_all(), delete(), Call::Text("new".into())]
        );

        // The text it shows is the old text again: the keys were ignored…
        let ambiguous = "Clicked [1000001] EditText \"Search\" and selected its text and pressed delete, but it still shows \"old text\" (8 characters): that may be its hint (the field is empty) or the app took no notice of the keys, and the screen's element tree cannot tell which. Nothing was typed. To fill a field that is empty, tap it with preview_click and type with preview_input.";
        let (driver, harness) = field_phone(34, false, |_| {});
        assert_eq!(
            harness.session.fill_uid(1_000_001, "new").unwrap_err(),
            ambiguous
        );
        assert_eq!(driver.take(), [FIELD_TAP, select_all(), delete()]);

        // …or the field was empty all along and this is its hint (no attribute says so).
        let (driver, harness) = field_phone(34, true, |field| {
            field.text = Some("Search here".into());
        });
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            ambiguous.replace("\"old text\" (8 characters)", "\"Search here\" (11 characters)")
        );
        assert_eq!(driver.take(), [FIELD_TAP, select_all(), delete()]);

        // A dump that does give the hint says so: the text equal to it is no text, and the
        // field is typed into without being cleared.
        let (driver, harness) = field_phone(34, true, |field| {
            field.text = Some("Search here".into());
            field.hint = Some("Search here".into());
        });
        assert_eq!(
            harness.session.fill_uid(1_000_001, "new").unwrap(),
            "Clicked [1000001] EditText \"Search\" and typed 3 characters; the field was already empty."
        );
        assert_eq!(driver.take(), [FIELD_TAP, Call::Text("new".into())]);

        // Part of the old text left: the deletion stopped half way, which is not an empty field.
        let (driver, harness) = field_phone(34, false, |_| {});
        driver.backspace_takes_one_character();
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            "Clicked [1000001] EditText \"Search\" and selected its text and pressed delete, but it still holds \"old tex\" (7 characters), part of its old text: the app took only some of the keys. Nothing was typed."
        );
        assert_eq!(driver.take(), [FIELD_TAP, select_all(), delete()]);
    }

    /// What a device holds in a field is quoted in the answer without the text-direction controls
    /// and zero-width characters that reorder or hide what a person reads there; the length it
    /// states is the field's own.
    #[test]
    fn a_fills_answer_quotes_the_old_text_without_hidden_format_characters() {
        assert_eq!(
            one_line("a\u{202e}b\u{200b}c\u{2066}d\u{2069}\u{feff}e\u{200f}", 20),
            "abcde"
        );
        assert_eq!(one_line("\u{200b} x \u{2060}", 20), "x");
        assert_eq!(one_line("tab\tbed\n", 20), "tab bed");
        assert_eq!(
            quoted_text("pay\u{202e}evil"),
            "\"payevil\" (8 characters)"
        );

        let (driver, harness) = field_phone(34, true, |field| {
            field.text = Some("old\u{202e}text\u{200b}".into());
        });
        let answer = harness.session.fill_uid(1_000_001, "new").unwrap();
        assert_eq!(
            answer,
            "Clicked [1000001] EditText \"Search\", removed its old text \"oldtext\" (9 characters) and typed 3 characters."
        );
        assert_eq!(driver.take().len(), 4);

        // The same on an iPhone.
        let (driver, harness) =
            ios_field_phone("TextField", Some("it\u{202e}s old"), None, true);
        let answer = harness.session.fill_uid(1_000_001, "new").unwrap();
        assert!(
            answer.starts_with("Removed the old text \"its old\" (8 characters) from"),
            "{answer}"
        );
        assert_eq!(driver.take().len(), 4);
    }

    /// A call stopped while the field was being read deletes nothing: the read is the slow part,
    /// and it is over before the first key is sent.
    #[test]
    fn a_fill_on_android_stopped_during_the_read_deletes_nothing() {
        let (driver, harness) = field_phone(34, true, |_| {});
        let stop = Arc::new(AtomicBool::new(false));
        let raise = stop.clone();
        // The snapshot was the first read; the fill's own is the second.
        driver.on_tree_read(move |reads| {
            if reads == 2 {
                raise.store(true, Ordering::SeqCst);
            }
        });
        let stopped = {
            let stop = stop.clone();
            move || stop.load(Ordering::SeqCst)
        };
        let call = CallContext {
            deadline: Instant::now() + Duration::from_secs(60),
            cancelled: &stopped,
        };
        let refused = harness
            .session
            .fill_uid_within(1_000_001, "new", &call)
            .unwrap_err();
        assert_eq!(
            refused,
            "Clicked [1000001] EditText \"Search\", then the call was cancelled; its text was left as it was and nothing was typed."
        );
        assert_eq!(driver.take(), [FIELD_TAP]);
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 2);
    }

    /// The reads a fill makes are bounded by the time the call has left, so a slow device cannot
    /// carry a read on past the call; and a read is not started with less left than it is
    /// expected to take.
    #[test]
    fn a_fills_reads_are_bounded_by_the_time_the_call_has_left() {
        let never = || false;
        let within = |seconds: u64| CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_secs(seconds),
            cancelled: &never,
        };
        // Android: each read is given what is left less a margin, never more.
        let (driver, harness) = field_phone(34, true, |_| {});
        let call = within(20);
        harness
            .session
            .fill_uid_within(1_000_001, "new", &call)
            .unwrap();
        let timeouts = driver.tree_timeouts();
        assert_eq!(timeouts.len(), 2, "{timeouts:?}");
        for timeout in &timeouts {
            assert!(
                *timeout <= Duration::from_secs(20) - crate::remote_control::driver::UI_TREE_MARGIN
                    && *timeout > Duration::from_secs(17),
                "{timeouts:?}"
            );
        }
        assert!(timeouts[1] <= timeouts[0], "{timeouts:?}");

        // A device whose read outlasts what is left fails the read, and the field is as it was.
        let (driver, harness) = field_phone(34, true, |_| {});
        driver.tree_read_takes(Duration::from_secs(60));
        let call = within(20);
        let refused = harness
            .session
            .fill_uid_within(1_000_001, "new", &call)
            .unwrap_err();
        assert!(
            refused.starts_with("Clicked [1000001] EditText \"Search\", but reading the screen's element tree failed: ")
                && refused.ends_with("its text was left as it was and nothing was typed."),
            "{refused}"
        );
        assert_eq!(driver.take(), [FIELD_TAP]);

        // With less left than a read is expected to take, none is started.
        let (driver, harness) = field_phone(34, true, |_| {});
        let live = harness.session.inner.live().unwrap();
        let target = harness
            .session
            .inner
            .state
            .lock()
            .unwrap()
            .uids
            .get(&1_000_001)
            .cloned()
            .unwrap();
        let reads = driver.tree_reads.load(Ordering::SeqCst);
        let almost = Budget {
            cancelled: &never,
            limit: Instant::now() + UI_TREE_READ - Duration::from_millis(200),
        };
        assert_eq!(
            harness.session.inner.read_field(&live, &target, &almost),
            Err("the call has too little time left to read the screen again".to_owned())
        );
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), reads);
        assert!(driver.tree_timeouts().is_empty());
        let enough = Budget {
            cancelled: &never,
            limit: Instant::now() + UI_TREE_READ + Duration::from_secs(1),
        };
        assert_eq!(
            harness.session.inner.read_field(&live, &target, &enough),
            Ok(Some("old text".to_owned()))
        );
        assert_eq!(driver.tree_timeouts().len(), 1);

        // An iPhone's reads are bounded the same way.
        let driver = FakeDriver::ios_with(ios_field_screen("TextField", Some("old"), None), true);
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        driver.tree_read_takes(Duration::from_secs(60));
        let call = within(25);
        let refused = harness
            .session
            .fill_uid_within(1_000_001, "new", &call)
            .unwrap_err();
        assert!(
            refused.starts_with(
                "Nothing was clicked, cleared or typed on [1000001] TextField \"Name\": reading the screen's element tree failed: "
            ) && refused.ends_with("The field was left as it was."),
            "{refused}"
        );
        assert!(driver.take().is_empty());
        assert_eq!(driver.tree_timeouts().len(), 1);
    }

    /// A screen whose driver has no WebDriverAgent to clear a field with (a fake, or any
    /// protocol with no "clear this field") keeps the text the field had.
    #[test]
    fn a_fill_off_android_and_without_a_webdriveragent_still_keeps_the_fields_text() {
        let driver = FakeDriver::with_tree(
            Platform::Ios,
            1170,
            2532,
            Some(field_screen(Some("old text"), true)),
        );
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        let answer = harness.session.fill_uid(1_000_001, "new").unwrap();
        assert!(
            answer.ends_with("text already in the field was kept."),
            "{answer}"
        );
        assert!(matches!(
            driver.take().as_slice(),
            [Call::Tap(..), Call::Text(_)]
        ));
    }

    // ----------------------------------------------- preview_fill replaces on an iPhone

    /// A phone screen with one field of `role`, `TextField "Name"` and so on, holding `value`
    /// with `hint` for its placeholder, not focused (WebDriverAgent seldom says a field is).
    fn ios_field_screen(role: &str, value: Option<&str>, hint: Option<&str>) -> UiNode {
        UiNode {
            role: "Other".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1170,
                height: 2532,
            }),
            children: vec![UiNode {
                role: role.into(),
                name: "Name".into(),
                value: value.map(str::to_owned),
                hint: hint.map(str::to_owned),
                bounds: Some(Rect {
                    x: 100,
                    y: 300,
                    width: 880,
                    height: 120,
                }),
                enabled: true,
                ..UiNode::default()
            }],
            ..UiNode::default()
        }
    }

    /// An iPhone showing the field, whose clear works when `clearing`, with its snapshot taken
    /// (the field is `[1000001]`, its middle at (540, 360)).
    fn ios_field_phone(
        role: &str,
        value: Option<&str>,
        hint: Option<&str>,
        clearing: bool,
    ) -> (Arc<FakeDriver>, Harness) {
        let driver = FakeDriver::ios_with(ios_field_screen(role, value, hint), clearing);
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        assert!(driver.take().is_empty());
        (driver, harness)
    }

    /// The search `POST /elements` is asked for the field `TextField "Name"` holding `value`.
    fn ios_find(role: &str, value: &str) -> Call {
        Call::Wda(
            "POST session:/elements".into(),
            Some(json!({
                "using": "predicate string",
                "value": format!("type == 'XCUIElementType{role}' AND value == {value} AND label == 'Name'")
            })),
        )
    }

    fn ios_clear() -> Call {
        Call::Wda(
            format!("POST session:/element/{IOS_FIELD_ID}/clear"),
            Some(json!({})),
        )
    }

    /// An iPhone field's text is replaced: the tree is read again and the field found by its
    /// role and box (it need not have the focus), WebDriverAgent finds it by its text, with the
    /// text escaped for a predicate, and clears it, the tree is read once more to see it empty,
    /// and only then is it tapped and typed into. The answer says what was removed.
    #[test]
    fn a_fill_on_an_iphone_replaces_the_text_of_the_field() {
        for role in ["TextField", "TextView", "SearchField"] {
            let (driver, harness) =
                ios_field_phone(role, Some("it's old"), Some("Your name"), true);
            let answer = harness.session.fill_uid(1_000_001, "new").unwrap();
            assert_eq!(
                answer,
                format!("Removed the old text \"it's old\" (8 characters) from [1000001] {role} \"Name\", then clicked it and typed 3 characters.")
            );
            assert_eq!(
                driver.take(),
                [
                    ios_find(role, "'it\\'s old'"),
                    ios_clear(),
                    Call::Tap(540, 360, 1),
                    Call::Text("new".into())
                ],
                "{role}"
            );
            // The snapshot, the read before the clear, the read that confirms the field is empty.
            assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 3);
        }

        // Clearing is a fill of nothing: the field is tapped, and nothing is typed.
        let (driver, harness) = ios_field_phone("TextField", Some("old"), None, true);
        assert_eq!(
            harness.session.fill_uid(1_000_001, "").unwrap(),
            "Removed the old text \"old\" (3 characters) from [1000001] TextField \"Name\", then clicked it; typed nothing."
        );
        assert_eq!(
            driver.take(),
            [
                ios_find("TextField", "'old'"),
                ios_clear(),
                Call::Tap(540, 360, 1)
            ]
        );

        // The same text, a single character, and a long one: all of it is said once and typed.
        let (driver, harness) = ios_field_phone("TextField", Some("x"), None, true);
        assert_eq!(
            harness.session.fill_uid(1_000_001, "y").unwrap(),
            "Removed the old text \"x\" (1 character) from [1000001] TextField \"Name\", then clicked it and typed 1 character."
        );
        assert_eq!(driver.take().len(), 4);

        // A hint is not the text when the field holds something else: it is still cleared.
        let (driver, harness) = ios_field_phone("TextField", Some("old"), Some("Name here"), true);
        assert!(harness
            .session
            .fill_uid(1_000_001, "n")
            .unwrap()
            .starts_with("Removed the old text \"old\""));
        assert_eq!(driver.take().len(), 4);
    }

    /// A field with nothing in it — no value, or only its placeholder shown as the value — is not
    /// cleared: it is tapped and typed into, and WebDriverAgent is not asked for anything.
    #[test]
    fn a_fill_on_an_iphone_with_nothing_in_the_field_just_types() {
        for (value, hint) in [
            (None, None),
            (None, Some("Your name")),
            (Some("Your name"), Some("Your name")),
        ] {
            let (driver, harness) = ios_field_phone("TextField", value, hint, true);
            assert_eq!(
                harness.session.fill_uid(1_000_001, "new").unwrap(),
                "Clicked [1000001] TextField \"Name\" and typed 3 characters; the field was already empty.",
                "{value:?} {hint:?}"
            );
            assert_eq!(
                driver.take(),
                [Call::Tap(540, 360, 1), Call::Text("new".into())]
            );
            // The snapshot and the read before.
            assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 2);
        }
        let (driver, harness) =
            ios_field_phone("SearchField", Some("Search"), Some("Search"), true);
        assert_eq!(
            harness.session.fill_uid(1_000_001, "").unwrap(),
            "Clicked [1000001] SearchField \"Name\"; the field was already empty."
        );
        assert_eq!(driver.take(), [Call::Tap(540, 360, 1)]);
    }

    /// Everything that cannot be known or done is refused before anything reaches the device:
    /// no tap, no search, no clear, no key. The answer says the field was left as it was.
    #[test]
    fn a_fill_on_an_iphone_refuses_before_touching_a_field_it_cannot_replace() {
        let untouched = "The field was left as it was.";

        // A password field is the user's to fill, and its text cannot be read to be replaced.
        let (driver, harness) = ios_field_phone("SecureTextField", Some("hunter2"), None, true);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            format!("Nothing was clicked, cleared or typed on [1000001] SecureTextField \"Name\": it is a password field, whose text is hidden and is the user's to enter, so it is not replaced; ask the user to fill it. {untouched}")
        );
        assert!(driver.take().is_empty());
        // Even a text field that the tree flags as hiding its text (the reading is what is
        // trusted, not the role alone).
        let driver = FakeDriver::ios_with(
            {
                let mut screen = ios_field_screen("TextField", Some("••••"), None);
                screen.children[0].password = true;
                screen
            },
            true,
        );
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("the field is a password field, whose text cannot be read")
                && refused.ends_with(untouched),
            "{refused}"
        );
        assert!(driver.take().is_empty());

        // Not a field that holds text: it says what it is.
        for role in ["Button", "StaticText", "Other", "Switch"] {
            let (driver, harness) = ios_field_phone(role, Some("old"), None, true);
            let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
            assert!(
                refused.starts_with(&format!(
                    "Nothing was clicked, cleared or typed on [1000001] {role} \"Name\": it is not a text field, text view or search field"
                )) && refused.ends_with(untouched),
                "{role}: {refused}"
            );
            assert!(driver.take().is_empty(), "{role}");
            // Not even the tree was read again.
            assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 1, "{role}");
        }

        // A text that is cut short by the reading is not all known.
        let driver = FakeDriver::ios_with(
            {
                let mut screen = ios_field_screen("TextField", Some("long text"), None);
                screen.children[0].truncated = true;
                screen
            },
            true,
        );
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("longer than the reading of the screen keeps")
                && refused.ends_with(untouched),
            "{refused}"
        );
        assert!(driver.take().is_empty());

        // Text the driver cannot type is refused first, before the tree is read again.
        let (driver, harness) = ios_field_phone("TextField", Some("old"), None, true);
        let refused = harness.session.fill_uid(1_000_001, "caf\u{7}").unwrap_err();
        assert!(
            refused.starts_with("Nothing was clicked or typed"),
            "{refused}"
        );
        assert!(driver.take().is_empty());
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 1);

        // The field is not where the snapshot saw it, or its box names two elements.
        let (driver, harness) = ios_field_phone("TextField", Some("old"), None, true);
        let mut moved = ios_field_screen("TextField", Some("old"), None);
        moved.children[0].bounds = Some(Rect {
            x: 100,
            y: 700,
            width: 880,
            height: 120,
        });
        driver.show(moved);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("the field is not on the screen where the snapshot saw it")
                && refused.ends_with(untouched),
            "{refused}"
        );
        assert!(driver.take().is_empty());
        let mut twice = ios_field_screen("TextField", Some("old"), None);
        let copy = twice.children[0].clone();
        twice.children.push(copy);
        driver.show(twice);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("more than one element has the field's role and box")
                && refused.ends_with(untouched),
            "{refused}"
        );
        assert!(driver.take().is_empty());

        // Not enough time to read twice, clear the field, tap it and type into it: nothing is
        // touched. (A fill needs seconds on a slow device; this call has one.)
        let (driver, harness) = ios_field_phone("TextField", Some("old"), None, true);
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_secs(1),
            cancelled: &never,
        };
        let refused = harness
            .session
            .fill_uid_within(1_000_001, &"n".repeat(500), &call)
            .unwrap_err();
        assert!(
            refused.starts_with(
                "Nothing was clicked, cleared or typed on [1000001] TextField \"Name\": replacing its text takes about "
            ) && refused.contains("reading the screen twice, clearing the field, tapping it and typing 500 characters")
                && refused.ends_with(&format!("type less, or try again. {untouched}")),
            "{refused}"
        );
        assert!(driver.take().is_empty());
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 1);
        // The same fill with the time it needs goes through.
        assert!(harness
            .session
            .fill_uid(1_000_001, &"n".repeat(500))
            .is_ok());
    }

    /// A clear that WebDriverAgent could not do, or that the app took no notice of, leaves the old
    /// text where it is, and nothing is typed over it.
    #[test]
    fn a_fill_on_an_iphone_types_nothing_over_text_that_was_not_cleared() {
        // WebDriverAgent answered the clear, and the field still holds its text.
        let (driver, harness) = ios_field_phone("TextField", Some("old text"), None, false);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            "Asked WebDriverAgent to clear [1000001] TextField \"Name\", but it still holds \"old text\" (8 characters): its old text may still be there. Nothing was typed."
        );
        assert_eq!(
            driver.take(),
            [ios_find("TextField", "'old text'"), ios_clear()]
        );

        // The clear failed on the device: it may have taken part of the text.
        let (driver, harness) = ios_field_phone("TextField", Some("old text"), None, true);
        driver.wda_fails_to_clear(DriverError::Failed("the clear timed out".into()));
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            "Clearing [1000001] TextField \"Name\" failed: the clear timed out; nothing was typed. Its old text may still be there."
        );
        assert_eq!(driver.take().len(), 2);

        // No element has the text, or several do: nothing was cleared, and it says so.
        let (driver, harness) = ios_field_phone("TextField", Some("old text"), None, true);
        driver.wda_finds(json!([]));
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert_eq!(
            refused,
            "Could not clear [1000001] TextField \"Name\": No field on the screen has exactly that text, so nothing was cleared. The screen may have changed since the snapshot: take a new preview_snapshot. Nothing was typed, and the field was left as it was."
        );
        assert_eq!(driver.take(), [ios_find("TextField", "'old text'")]);
        driver.wda_finds(json!([
            {"element-6066-11e4-a52e-4f735466cecf": IOS_FIELD_ID},
            {"element-6066-11e4-a52e-4f735466cecf": "9A8B7C6D-5E4F-4A3B-9C2D-1E0F9A8B7C6D"}
        ]));
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("2 fields on the screen have exactly that text")
                && refused.ends_with("Nothing was typed, and the field was left as it was."),
            "{refused}"
        );
        assert_eq!(driver.take().len(), 1);

        // Text a predicate cannot match (a line break) is refused by the clear before a call.
        let (driver, harness) = ios_field_phone("TextView", Some("two\nlines"), None, true);
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(
            refused.contains("which a search by its text cannot match")
                && refused.ends_with("Nothing was typed, and the field was left as it was."),
            "{refused}"
        );
        assert!(driver.take().is_empty());
    }

    /// The reading of the screen trims texts and turns control characters into spaces. A field
    /// whose text it changed is not found by that text: a search for "John" finds the field that
    /// holds exactly "John", and clearing that one would clear another field than the one filled.
    /// So it is refused, before anything is searched or cleared.
    #[test]
    fn a_fill_on_an_iphone_refuses_a_field_whose_text_the_reading_changed() {
        let box_of = |y: i32| Rect {
            x: 100,
            y,
            width: 880,
            height: 120,
        };
        let field = |name: &str, y: i32, shown: &str, inexact: bool| UiNode {
            role: "TextField".into(),
            name: name.into(),
            value: Some(shown.into()),
            bounds: Some(box_of(y)),
            enabled: true,
            inexact,
            ..UiNode::default()
        };
        let screen = |first: UiNode, second: UiNode| UiNode {
            role: "Other".into(),
            bounds: Some(Rect {
                x: 0,
                y: 0,
                width: 1170,
                height: 2532,
            }),
            children: vec![first, second],
            ..UiNode::default()
        };
        let untouched = "its text has characters (or spacing) the reading of the screen cannot keep exactly, so it cannot be matched safely. The field was left as it was.";

        // Field A holds "John " (the reading says "John"); field B holds "John". Filling A
        // would find B by "John" and clear it.
        let driver = FakeDriver::ios_with(
            screen(
                field("First", 300, "John", true),
                field("First", 600, "John", false),
            ),
            true,
        );
        driver.wda_holds(box_of(300), "John ");
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        let uids = lock(&harness.session.inner.state).uids.clone();
        let uid_of = |y: i32| {
            *uids
                .iter()
                .find(|(_, target)| target.bounds == Some(box_of(y)))
                .map(|(uid, _)| uid)
                .unwrap()
        };
        let refused = harness
            .session
            .fill_uid(uid_of(300), "Jane")
            .unwrap_err();
        assert_eq!(
            refused,
            format!(
                "Nothing was clicked, cleared or typed on [{}] TextField \"First\": {untouched}",
                uid_of(300)
            )
        );
        // Not searched, not cleared, not tapped, and field B still holds its text.
        assert!(driver.take().is_empty());
        let tree = driver.tree.lock().unwrap().clone().unwrap();
        assert_eq!(tree.children[1].value.as_deref(), Some("John"));
        // The other field, whose reading is exact, is the one that is replaced.
        assert!(harness.session.fill_uid(uid_of(600), "Jane").is_ok());

        // Tabs and line breaks become spaces in the reading: the device holds "a\tb".
        let driver = FakeDriver::ios_with(
            screen(
                field("Notes", 300, "a b", true),
                field("Other", 600, "", false),
            ),
            true,
        );
        driver.wda_holds(box_of(300), "a\tb");
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        let uid = *lock(&harness.session.inner.state)
            .uids
            .iter()
            .find(|(_, target)| target.bounds == Some(box_of(300)))
            .map(|(uid, _)| uid)
            .unwrap();
        let refused = harness.session.fill_uid(uid, "x").unwrap_err();
        assert_eq!(
            refused,
            format!(
                "Nothing was clicked, cleared or typed on [{uid}] TextField \"Notes\": {untouched}"
            )
        );
        assert!(driver.take().is_empty());

        // An empty field whose placeholder was trimmed is refused too: what the reading says is
        // empty may hold text that equals the placeholder only after trimming.
        let driver = FakeDriver::ios_with(
            {
                let mut screen = ios_field_screen("TextField", Some("Your name"), Some("Your name"));
                screen.children[0].inexact = true;
                screen
            },
            true,
        );
        let harness = connected(control(Protocol::Wda), driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        assert!(harness
            .session
            .fill_uid(1_000_001, "x")
            .unwrap_err()
            .ends_with(untouched));
        assert!(driver.take().is_empty());
    }

    /// The fake WebDriverAgent finds what the device itself holds, as the real one does: with the
    /// predicate the fill sends, and not an element it does not match.
    #[test]
    fn the_fake_webdriveragent_evaluates_the_predicate_it_is_sent() {
        let driver = FakeDriver::ios_with(ios_field_screen("TextField", Some("old"), None), true);
        let find = |predicate: &str| -> Vec<String> {
            let found = driver
                .call(
                    "POST",
                    WdaRoute::Session("/elements"),
                    Some(&json!({"using": "predicate string", "value": predicate})),
                    Duration::from_secs(1),
                )
                .unwrap();
            found
                .as_array()
                .unwrap()
                .iter()
                .map(|element| {
                    element["element-6066-11e4-a52e-4f735466cecf"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .collect()
        };
        assert_eq!(
            find("type == 'XCUIElementTypeTextField' AND value == 'old' AND label == 'Name'"),
            [IOS_FIELD_ID]
        );
        assert_eq!(
            find("type == 'XCUIElementTypeTextField' AND value == 'old'"),
            [IOS_FIELD_ID]
        );
        for predicate in [
            "type == 'XCUIElementTypeTextField' AND value == 'older' AND label == 'Name'",
            "type == 'XCUIElementTypeTextField' AND value == 'old' AND label == 'Nom'",
            "type == 'XCUIElementTypeTextView' AND value == 'old' AND label == 'Name'",
            "type == 'XCUIElementTypeTextField' AND value == 'ol\\'d'",
        ] {
            assert!(find(predicate).is_empty(), "{predicate}");
        }
        // What the device holds is what matches, not what the tree says.
        driver.wda_holds(
            Rect {
                x: 100,
                y: 300,
                width: 880,
                height: 120,
            },
            "old ",
        );
        assert!(find("type == 'XCUIElementTypeTextField' AND value == 'old'").is_empty());
        assert_eq!(
            find("type == 'XCUIElementTypeTextField' AND value == 'old '"),
            [IOS_FIELD_ID]
        );
    }

    /// An iPhone field is typed at about 60 characters a second, not the hundreds an Android
    /// `input text` takes: a long text is refused before anything is cleared when the call has
    /// not the time to type it, where an estimate for Android would have let it through.
    #[test]
    fn a_fill_on_an_iphone_counts_the_time_typing_takes_before_it_clears_anything() {
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_secs(27),
            cancelled: &never,
        };
        let (driver, harness) = ios_field_phone("TextField", Some("old"), None, true);
        let refused = harness
            .session
            .fill_uid_within(1_000_001, &"n".repeat(2000), &call)
            .unwrap_err();
        assert!(
            refused.starts_with(
                "Nothing was clicked, cleared or typed on [1000001] TextField \"Name\": replacing its text takes about "
            ) && refused.contains("typing 2000 characters")
                && refused.ends_with("type less, or try again. The field was left as it was."),
            "{refused}"
        );
        // 2000 characters at 60 a second is 34 s: well over what is left.
        let seconds: u64 = refused
            .split("takes about ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|seconds| seconds.parse().ok())
            .unwrap();
        assert!(seconds >= 45, "{refused}");
        assert!(driver.take().is_empty());
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 1);

        // What a call has the time to type is replaced: 300 characters, 5 s of typing.
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_secs(27),
            cancelled: &never,
        };
        assert!(harness
            .session
            .fill_uid_within(1_000_001, &"n".repeat(300), &call)
            .is_ok());

        // The estimates are the session's own tunables: typing counted at the rate they state.
        let estimates = ReplaceEstimates::default();
        assert_eq!(estimates.ios_typing(0), Duration::ZERO);
        assert_eq!(estimates.ios_typing(60), Duration::from_secs(1));
        assert_eq!(estimates.ios_typing(61), Duration::from_millis(1017));
        assert_eq!(estimates.ios_typing(2000), Duration::from_millis(33_334));
        assert_eq!(estimates.android_typing(0), Duration::ZERO);
        assert_eq!(estimates.android_typing(100), REPLACE_TYPE_PIECE);
        assert_eq!(estimates.android_typing(101), REPLACE_TYPE_PIECE * 2);
        assert!(estimates.ios_typing(100) > estimates.android_typing(100));
    }

    /// The clear took time: what is left is checked again for the second read, the tap and the
    /// typing, and a call that cannot do all three says the field is empty now and that nothing
    /// was typed, rather than going on to type into a field it cannot finish.
    #[test]
    fn a_fill_on_an_iphone_checks_the_time_left_again_after_the_clear() {
        // The estimates shrunk a hundredfold, so that a clear that takes real time can use them
        // up: reading 30 ms, tapping 6 ms, clearing 50 ms, typing 600 characters a second.
        let replace = ReplaceEstimates {
            tap: Duration::from_millis(6),
            read: Duration::from_millis(30),
            keys: Duration::from_millis(10),
            ios_clear: Duration::from_millis(50),
            type_piece: Duration::from_millis(6),
            ios_chars_per_second: 600,
        };
        let timing = Timing {
            replace,
            ..fast()
        };
        let driver = FakeDriver::ios_with(ios_field_screen("TextField", Some("old"), None), true);
        let shared = driver.clone();
        let harness = start_timed(
            control(Protocol::Wda),
            None,
            Arc::default(),
            timing,
            move |_, _| Ok(shared.clone()),
        );
        assert_eq!(
            harness.session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        harness.session.snapshot().unwrap();
        driver.take();
        // 300 characters take 500 ms to type at that rate: the call is given the time for the
        // whole fill (about 616 ms) and a little more, then the clear takes 1.5 s of it.
        driver.wda_clear_takes(Duration::from_millis(1500));
        let never = || false;
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(2000),
            cancelled: &never,
        };
        let value = "n".repeat(300);
        let refused = harness
            .session
            .fill_uid_within(1_000_001, &value, &call)
            .unwrap_err();
        assert_eq!(
            refused,
            "Asked WebDriverAgent to clear [1000001] TextField \"Name\" (its old text was \"old\" (3 characters)), then this call had too little time left to read it again, tap it and type 300 characters: the field is empty now unless the app took no notice of the clear, and nothing was typed. Fill it again."
        );
        // The search and the clear were made; the field was not read again, tapped or typed into.
        let calls = driver.take();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(matches!(&calls[0], Call::Wda(route, _) if route == "POST session:/elements"));
        assert!(matches!(&calls[1], Call::Wda(route, _) if route.ends_with("/clear")));
        assert_eq!(driver.tree_reads.load(Ordering::SeqCst), 2);

        // With the time, the same fill goes through.
        driver.wda_clear_takes(Duration::ZERO);
        driver.show(ios_field_screen("TextField", Some("old"), None));
        let call = CallContext {
            deadline: Instant::now() + CALL_MARGIN + Duration::from_millis(2000),
            cancelled: &never,
        };
        assert!(harness
            .session
            .fill_uid_within(1_000_001, &value, &call)
            .is_ok());
    }

    /// A read-only entry refuses a fill before anything else, an iPhone included.
    #[test]
    fn a_fill_on_a_read_only_iphone_is_refused() {
        let mut target = control(Protocol::Wda);
        target.read_only = true;
        let driver = FakeDriver::ios_with(ios_field_screen("TextField", Some("old"), None), true);
        let harness = connected(target, driver.clone());
        harness.session.snapshot().unwrap();
        driver.take();
        let refused = harness.session.fill_uid(1_000_001, "new").unwrap_err();
        assert!(refused.contains("is read-only"), "{refused}");
        assert!(driver.take().is_empty());
    }
}
