//! An iOS screen over WebDriverAgent (appium/WebDriverAgent).
//!
//! WebDriverAgent (WDA) is the XCUITest runner Appium, go-ios and Xcode all
//! start on an iPhone, iPad or simulator; it serves a WebDriver-style HTTP API
//! on port 8100 of the device (a simulator's own loopback, or a real device's
//! through `iproxy`). Preparing it — signing, installing, starting, forwarding
//! the port — is the user's job; this driver only talks to one that is already
//! running (`docs/remote-control.md` §1).
//!
//! What the protocol makes this driver careful about:
//!
//! * **Sessions.** Creating a session makes WDA end every other one, so an
//!   Appium run that started WDA would lose its session the moment Mewrk
//!   connected, and take Mewrk's in turn. [`connect`] therefore reads
//!   `GET /status` first and *reuses* the session it names; it creates one only
//!   when there is none, and then deletes only that one on [`Driver::close`].
//!   A session id goes into URL paths, so it is validated before it is stored.
//! * **Two units.** Screenshots are in pixels, every gesture and the UI tree in
//!   points. The driver's coordinates are screenshot pixels (the
//!   [`super::driver`] rule); the pixel/point ratio is measured as screenshot
//!   width over window width rather than trusted from `/wda/screen`, and
//!   re-measured whenever the screenshot changes size (a rotation).
//! * **Swipes.** `/wda/dragfromtoforduration` presses and *holds* for its
//!   duration before dragging, which turns a scroll into a long press;
//!   `/wda/pressAndDragWithVelocity` moves at a speed instead and is used for
//!   swipes. Either gesture, or a tap, falls back to a W3C `/actions` pointer
//!   sequence when a WDA build answers that the route does not exist.
//! * **Keys.** WDA types text into the focused field and has a home button and
//!   two volume buttons; it has no arrows, Escape, modifiers or Power. Those
//!   answer [`DriverError::Unsupported`] with what to do instead.
//!
//! * **The iOS tools.** `ios_*` reach WebDriverAgent through [`IosControl::call`]:
//!   fixed route strings only (a route never comes from the model, and the few
//!   that could end WebDriverAgent or replace its session are refused), the same
//!   session rules as every other call, and one time limit for the whole call,
//!   a lost session replaced inside it, and waiting for another caller's
//!   replacement of it counted in. A call that outlives that limit
//!   ([`Bound::Call`]) answers [`DriverError::Failed`]: WebDriverAgent took the
//!   connection and not the request, as it does while an app blocks it, and
//!   that must not start a reconnect.
//!
//! Every request is a fresh short-lived connection through [`super::http`], so
//! there is no wire to serialise; the driver's state is behind short locks that
//! are never held across a request (except while a lost session is replaced, so
//! two failing calls do not both create one). Transport failures come back as
//! [`DriverError::Disconnected`] from the call that met them (a timeout of an
//! [`IosControl::call`] excepted); the driver never sends
//! [`DriverEvent::Disconnected`], because a poll that finds nothing is not an
//! event.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, TryLockError,
    },
    thread,
    time::{Duration, Instant},
};

use base64::{
    alphabet,
    engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig},
    Engine as _,
};
use serde_json::{json, Map, Value};

use super::{
    driver::{
        Driver, DriverError, DriverEvent, DriverEvents, DriverInfo, DriverResult, Frame,
        IosControl, Platform, Rect, ScreenUpdate, UiNode, WdaRoute,
    },
    http,
    keys::{Key, KeyChord, NamedKey},
    picture_stream::{decode_jpeg, Chunks, Jpegs, Next},
    transport::Connector,
};

/// `GET /status` answers at once or not at all.
const STATUS_TIMEOUT: Duration = Duration::from_secs(10);
/// Creating a session may wait for the foreground app to settle.
const SESSION_TIMEOUT: Duration = Duration::from_secs(60);
/// Cheap queries: the window size.
const QUERY_TIMEOUT: Duration = Duration::from_secs(20);
/// WebDriverAgent's MJPEG server, on the same place as its HTTP port.
const MJPEG_PORT: u16 = 9100;
/// What the MJPEG server is asked for. Its default is ten frames a second at a quarter of the quality.
const MJPEG_FRAMERATE: u32 = 30;
const MJPEG_QUALITY: u32 = 50;
const MJPEG_CONNECT: Duration = Duration::from_secs(3);
const MJPEG_SETTINGS_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a stream with nothing to read looks up to see whether it should stop.
const MJPEG_IDLE_WAIT: Duration = Duration::from_millis(100);

/// A screenshot is encoded on the device and can be several megabytes.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
/// `/source` walks the whole accessibility tree, which takes seconds on a busy screen.
const SOURCE_TIMEOUT: Duration = Duration::from_secs(60);
/// A gesture is performed on the device, which waits for the app to go idle first;
/// the gesture's own length is added.
const GESTURE_TIMEOUT: Duration = Duration::from_secs(30);
/// Ending a session we created is best effort and must not hold up closing.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a call bounded as a whole ([`Bound::Call`]) looks again for the right to replace a
/// lost session while another caller holds it.
const REPLACING_POLL: Duration = Duration::from_millis(50);
/// A request that failed this close to its timeout (or after it) ran out of time rather than
/// meeting a server that was gone: the clocks of the socket, the relay and this one differ by
/// about this much.
const TIMEOUT_SLACK: Duration = Duration::from_millis(150);
/// The one route with a query string a caller may name: the UI tree, which
/// [`Driver::ui_tree`] reads itself.
const SOURCE_ROUTE: &str = "/source?format=json";
/// The route that finds elements by a predicate: the one lookup the iOS tools make before clearing
/// a field (`/element/<id>/clear` is the other element route [`check_route`] takes).
const ELEMENTS_ROUTE: &str = "/elements";
/// The longest route [`IosControl::call`] takes. The longest fixed one is under 40 bytes.
const MAX_ROUTE_BYTES: usize = 96;

/// The longest a touch is held, whatever was asked.
const MAX_HOLD_MS: u32 = 60_000;
/// Text typed in one call. WDA types about 60 characters a second; this keeps one
/// call well inside a minute.
const MAX_TEXT_CHARS: usize = 2_000;

/// Pixels of a screenshot, at most. A phone is 4 million; this leaves room for any
/// tablet and refuses a PNG that would take a gigabyte to unpack.
const MAX_FRAME_PIXELS: u64 = 25_000_000;
const MAX_FRAME_SIDE: u32 = 16_384;
/// The decoder's own working-memory ceiling.
const PNG_WORKING_BYTES: usize = 192 * 1024 * 1024;

/// UI tree limits: a node count and a depth. serde_json already refuses JSON nested
/// deeper than 128, which is about 60 elements.
const MAX_TREE_NODES: usize = 5_000;
const MAX_TREE_DEPTH: usize = 64;
/// A label or value longer than this is cut. A snapshot goes to a model.
const MAX_TEXT_FIELD: usize = 300;

/// Pixels per point outside this range mean the screenshot and the window are not
/// of the same screen.
const MIN_RATIO: f64 = 0.25;
const MAX_RATIO: f64 = 16.0;
/// Horizontal and vertical ratios further apart than this mean the window is
/// not laid out like the screenshot — an app in Split View, Slide Over or Stage
/// Manager, or one turned relative to the screen. A full-screen app's ratios
/// agree to well under a percent (only the rounding of a side in points).
/// Input is refused while they disagree: every tap would land off target.
const MAX_SKEW: f64 = 0.03;
/// How old the last reading of the window may be before a capture or a touch
/// reads it again. An app goes into Split View, Slide Over or Stage Manager
/// without the screenshot changing size, so nothing else would notice; one
/// cheap request every few seconds, while frames are pulled or input placed,
/// catches it before many touches land off target.
const WINDOW_RECHECK: Duration = Duration::from_secs(3);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether `id` is something that can sit in a URL path: what WDA hands out is a
/// UUID, and nothing else is trusted to be spliced into one.
fn valid_session_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Whether `id` is an element id that can sit in a URL path: what WebDriverAgent hands out for an
/// element is a UUID, and nothing else is trusted to be spliced into one — the same strictness as
/// a session id.
pub(crate) fn valid_element_id(id: &str) -> bool {
    valid_session_id(id)
}

/// Connects to WebDriverAgent on `port` of the connector's place.
///
/// Fails with a sentence a model can act on when nothing answers, when what
/// answers is not WebDriverAgent, or when no usable session exists or can be
/// made. On success the driver already holds one screenshot, so
/// [`Driver::info`] knows the screen's size.
pub fn connect(
    connector: Arc<dyn Connector>,
    port: u16,
    events: DriverEvents,
) -> DriverResult<Arc<dyn Driver>> {
    let driver: Arc<dyn Driver> = Arc::new(open_driver(connector, port, events, WINDOW_RECHECK)?);
    Ok(driver)
}

/// [`connect`], with the window read again once its last reading is
/// `window_recheck` old (tests shorten it).
fn open_driver(
    connector: Arc<dyn Connector>,
    port: u16,
    events: DriverEvents,
    window_recheck: Duration,
) -> DriverResult<WdaDriver> {
    let target = format!("127.0.0.1:{port} on {}", connector.place_label());
    let status = read_status(connector.as_ref(), port)?;
    let label = label_from_status(&status.value);
    let (session, created) = match status.session_id {
        Some(id) => (Session { id, owned: false }, false),
        // Creating a session ends every other one WebDriverAgent has: a session
        // stopped while it connected must not take it from the one replacing it.
        None if connector.cancelled() => {
            return Err(DriverError::Failed(
                "the session was stopped while it was connecting".to_owned(),
            ))
        }
        None => (create_session(connector.as_ref(), port)?, true),
    };
    let driver = WdaDriver {
        connector,
        port,
        events,
        label,
        session: Mutex::new(session),
        replacing: Mutex::new(()),
        geometry: Mutex::new(Geometry::unknown()),
        window_read: Mutex::new(None),
        window_recheck,
        last_frame: Mutex::new(None),
        frame_seq: AtomicU64::new(0),
        unsupported: Mutex::new(HashSet::new()),
        closed: AtomicBool::new(false),
    };
    driver.log(format!(
        "WebDriverAgent answered on {target}: {}",
        driver.label
    ));
    driver.log(if created {
        "created a WebDriverAgent session (none existed)".to_owned()
    } else {
        "reusing the session WebDriverAgent already had, so other clients' sessions stay alive"
            .to_owned()
    });
    // One screenshot now, so the screen's size is known before anyone asks.
    if let Err(error) = driver.capture(false, true) {
        driver.close();
        return Err(DriverError::Failed(error.message().to_owned()));
    }
    let geometry = *lock(&driver.geometry);
    driver.log(geometry.describe());
    Ok(driver)
}

/// What `GET /status` told us.
struct Status {
    /// The `value` object.
    value: Map<String, Value>,
    /// The session WebDriverAgent already has, validated.
    session_id: Option<String>,
}

fn read_status(connector: &dyn Connector, port: u16) -> DriverResult<Status> {
    read_status_within(connector, port, STATUS_TIMEOUT)
}

/// [`read_status`], giving up after `timeout`.
fn read_status_within(
    connector: &dyn Connector,
    port: u16,
    timeout: Duration,
) -> DriverResult<Status> {
    let target = format!("127.0.0.1:{port} on {}", connector.place_label());
    let response = http::request(connector, port, "GET", "/status", None, timeout).map_err(
        |error| {
            DriverError::Failed(format!(
                "Could not reach WebDriverAgent: {error}. Start WebDriverAgent on the device (Xcode, Appium or go-ios) and, for a real device, forward its port (iproxy {port} 8100)."
            ))
        },
    )?;
    let not_wda = || {
        DriverError::Failed(format!(
            "{target} answered, but it is not WebDriverAgent (HTTP {}: {}).",
            response.status,
            response.snippet()
        ))
    };
    if !response.is_success() {
        return Err(not_wda());
    }
    let Ok(Value::Object(mut body)) = response.json() else {
        return Err(not_wda());
    };
    let session = body.remove("sessionId");
    let Some(Value::Object(value)) = body.remove("value") else {
        return Err(not_wda());
    };
    if !["ready", "build", "ios", "state"]
        .iter()
        .any(|key| value.contains_key(*key))
    {
        return Err(not_wda());
    }
    if value.get("ready") == Some(&Value::Bool(false)) {
        let message = value
            .get("message")
            .and_then(Value::as_str)
            .map(crate::preview_launch_config::sanitize_for_message)
            .unwrap_or_default();
        return Err(DriverError::Failed(format!(
            "WebDriverAgent on {target} is running but not ready yet ({message}). Try again in a moment."
        )));
    }
    let session_id = match session {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if valid_session_id(&id) => Some(id),
        Some(_) => {
            return Err(DriverError::Failed(format!(
                "WebDriverAgent on {target} reported a session id in an unexpected format; refusing to use it."
            )))
        }
    };
    Ok(Status { value, session_id })
}

/// A readable name for the device from `/status`: `iPhone (iOS 17.5)`.
fn label_from_status(value: &Map<String, Value>) -> String {
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(|text| clean_text(text, 40))
            .filter(|text| !text.is_empty())
    };
    let device =
        text(value.get("device")).map(|device| match device.to_ascii_lowercase().as_str() {
            "iphone" => "iPhone".to_owned(),
            "ipad" => "iPad".to_owned(),
            _ => device,
        });
    let os = value.get("os").and_then(Value::as_object).and_then(|os| {
        let name = text(os.get("name"))?;
        Some(match text(os.get("version")) {
            Some(version) => format!("{name} {version}"),
            None => name,
        })
    });
    match (device, os) {
        (Some(device), Some(os)) => format!("{device} ({os})"),
        (Some(device), None) => device,
        (None, Some(os)) => format!("{os} device"),
        (None, None) => "iOS device".to_owned(),
    }
}

/// `POST /session` with empty capabilities: no bundle id, so the session follows
/// whatever app is in the foreground.
fn create_session(connector: &dyn Connector, port: u16) -> DriverResult<Session> {
    create_session_within(connector, port, SESSION_TIMEOUT)
}

/// [`create_session`], giving up after `timeout`.
fn create_session_within(
    connector: &dyn Connector,
    port: u16,
    timeout: Duration,
) -> DriverResult<Session> {
    let body = json!({"capabilities": {"alwaysMatch": {}}});
    let response = http::request(connector, port, "POST", "/session", Some(&body), timeout)
        .map_err(|error| {
            DriverError::Failed(format!(
                "Could not create a WebDriverAgent session: {error}"
            ))
        })?;
    let json = response.json().ok();
    if !response.is_success() {
        let message = json
            .as_ref()
            .and_then(|json| json.pointer("/value/message"))
            .and_then(Value::as_str)
            .map(crate::preview_launch_config::sanitize_for_message)
            .unwrap_or_else(|| response.snippet());
        return Err(DriverError::Failed(format!(
            "WebDriverAgent would not create a session (HTTP {}): {message}",
            response.status
        )));
    }
    let id = json.as_ref().and_then(|json| {
        json.pointer("/value/sessionId")
            .or_else(|| json.get("sessionId"))
            .and_then(Value::as_str)
    });
    match id {
        Some(id) if valid_session_id(id) => Ok(Session {
            id: id.to_owned(),
            owned: true,
        }),
        _ => Err(DriverError::Failed(
            "WebDriverAgent created a session but did not return a usable session id.".to_owned(),
        )),
    }
}

struct Session {
    id: String,
    /// Whether this driver created it, and so may delete it.
    owned: bool,
}

/// The screen in both of its units.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    /// Screenshot size, pixels. `(0, 0)` before the first screenshot.
    pixels: (u32, u32),
    /// Window size, points.
    points: (f64, f64),
}

impl Geometry {
    fn unknown() -> Self {
        Self {
            pixels: (0, 0),
            points: (0.0, 0.0),
        }
    }

    fn ratio_x(&self) -> f64 {
        f64::from(self.pixels.0) / self.points.0
    }

    fn ratio_y(&self) -> f64 {
        f64::from(self.pixels.1) / self.points.1
    }

    /// How far apart the two axes' ratios are, as a fraction.
    fn skew(&self) -> f64 {
        let (x, y) = (self.ratio_x(), self.ratio_y());
        (x - y).abs() / x.max(y)
    }

    fn to_points(&self, x: u32, y: u32) -> (i64, i64) {
        (
            (f64::from(x) / self.ratio_x()).round() as i64,
            (f64::from(y) / self.ratio_y()).round() as i64,
        )
    }

    fn describe(&self) -> String {
        format!(
            "screen {}x{} px, window {}x{} points (x{:.2})",
            self.pixels.0,
            self.pixels.1,
            self.points.0,
            self.points.1,
            self.ratio_x()
        )
    }
}

/// How long a call to WebDriverAgent may take, and what a call that takes longer means.
#[derive(Clone, Copy)]
enum Bound {
    /// Each request may take this long, and a request that gets no answer means
    /// WebDriverAgent is gone: every call the driver makes for its own screen.
    Request(Duration),
    /// Everything one call does — a lost session replaced, the request repeated — fits in
    /// `timeout`, which ends at `deadline`; a call that outlives it is too slow, not
    /// disconnected: the server took the connection and not the request, as it does
    /// while an app blocks it ([`IosControl::call`]).
    Call {
        deadline: Instant,
        timeout: Duration,
    },
}

impl Bound {
    fn call(timeout: Duration) -> Self {
        Self::Call {
            deadline: Instant::now() + timeout,
            timeout,
        }
    }

    /// How long the next request may take.
    fn left(self) -> DriverResult<Duration> {
        match self {
            Self::Request(timeout) => Ok(timeout),
            Self::Call { deadline, timeout } => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    Err(DriverError::Failed(too_slow(timeout)))
                } else {
                    Ok(left)
                }
            }
        }
    }

    /// `limit`, or what is left of the call when that is less.
    fn within(self, limit: Duration) -> DriverResult<Duration> {
        match self {
            Self::Request(_) => Ok(limit),
            Self::Call { .. } => self.left().map(|left| left.min(limit)),
        }
    }
}

/// Whether a request that failed with `error` after `elapsed` ran out of its `timeout`.
fn is_timeout(error: &str, elapsed: Duration, timeout: Duration) -> bool {
    // The connection was never made: whatever is there did not answer at all.
    if error.ends_with("did not answer") {
        return false;
    }
    // A socket says so itself; a relay closes the connection when its deadline passes.
    error.contains("did not answer in time") || elapsed + TIMEOUT_SLACK >= timeout
}

/// What a call bounded as a whole answers when its time ran out while another caller was still
/// replacing the lost session. The call sent nothing but the request that found the session gone.
fn replacement_under_way() -> String {
    "Another call is re-establishing the WebDriverAgent session; try again. This call did nothing on the device."
        .to_owned()
}

fn too_slow(timeout: Duration) -> String {
    format!(
        "WebDriverAgent did not answer within {}; the device may be busy or an app may be blocking it, and the request may still complete.",
        seconds_text(timeout)
    )
}

/// `5 s`, or `0.2 s` for a time that is not whole seconds.
fn seconds_text(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis % 1000 == 0 {
        format!("{} s", millis / 1000)
    } else {
        format!("{:.1} s", millis as f64 / 1000.0)
    }
}

/// Why a call to a session route did not return a value.
enum CallFailure {
    /// WebDriverAgent has no such route (an older or trimmed build).
    NoSuchCommand(String),
    Failed(DriverError),
}

impl From<DriverError> for CallFailure {
    fn from(error: DriverError) -> Self {
        Self::Failed(error)
    }
}

impl From<CallFailure> for DriverError {
    fn from(failure: CallFailure) -> Self {
        match failure {
            CallFailure::NoSuchCommand(route) => DriverError::Unsupported(format!(
                "this WebDriverAgent build does not implement {route}."
            )),
            CallFailure::Failed(error) => error,
        }
    }
}

/// What a reply meant.
enum Outcome {
    Value(Value),
    InvalidSession,
    NoSuchCommand,
    Error(String),
}

fn interpret(response: &http::Response) -> Outcome {
    let json = response.json();
    if response.is_success() {
        return match json {
            Ok(mut json) => Outcome::Value(
                json.get_mut("value")
                    .map(Value::take)
                    .unwrap_or(Value::Null),
            ),
            Err(error) => {
                Outcome::Error(format!("WebDriverAgent sent an unreadable reply: {error}"))
            }
        };
    }
    let error = json
        .as_ref()
        .ok()
        .and_then(|json| json.pointer("/value/error"))
        .and_then(Value::as_str);
    let message = json
        .as_ref()
        .ok()
        .and_then(|json| json.pointer("/value/message"))
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .map(crate::preview_launch_config::sanitize_for_message);
    match error {
        Some("invalid session id") => Outcome::InvalidSession,
        Some("unknown command" | "unknown method") => Outcome::NoSuchCommand,
        // A 404 whose body is not WebDriverAgent's error format is the HTTP server's own
        // "no such route" page.
        None if matches!(response.status, 404 | 405 | 501) => Outcome::NoSuchCommand,
        _ => Outcome::Error(format!(
            "HTTP {}: {}",
            response.status,
            message
                .or_else(|| error.map(crate::preview_launch_config::sanitize_for_message))
                .unwrap_or_else(|| response.snippet())
        )),
    }
}

struct WdaDriver {
    connector: Arc<dyn Connector>,
    port: u16,
    events: DriverEvents,
    label: String,
    session: Mutex<Session>,
    /// Held while a lost session is replaced, so concurrent failures replace it once.
    replacing: Mutex<()>,
    geometry: Mutex<Geometry>,
    /// When the window size was last read.
    window_read: Mutex<Option<Instant>>,
    /// [`WINDOW_RECHECK`].
    window_recheck: Duration,
    last_frame: Mutex<Option<Frame>>,
    frame_seq: AtomicU64,
    /// Session routes this WebDriverAgent answered "no such command" to, so the
    /// W3C fallback is used directly the next time.
    unsupported: Mutex<HashSet<String>>,
    closed: AtomicBool,
}

impl WdaDriver {
    fn log(&self, line: String) {
        (self.events)(DriverEvent::Log(line));
    }

    fn ensure_open(&self) -> DriverResult<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(DriverError::Disconnected(
                "the WebDriverAgent connection was closed.".to_owned(),
            ));
        }
        Ok(())
    }

    /// One request, bounded by `bound`. A transport failure means WebDriverAgent is gone
    /// or not answering — except, for a call bounded as a whole ([`Bound::Call`]), when
    /// the failure is the time running out ([`is_timeout`]): that call was too slow, which
    /// says nothing about the connection.
    fn send(
        &self,
        bound: Bound,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> DriverResult<http::Response> {
        self.ensure_open()?;
        let timeout = bound.left()?;
        let started = Instant::now();
        http::request(self.connector.as_ref(), self.port, method, path, body, timeout).map_err(
            |error| match bound {
                Bound::Call { timeout: whole, .. }
                    if is_timeout(&error, started.elapsed(), timeout) =>
                {
                    DriverError::Failed(too_slow(whole))
                }
                _ => DriverError::Disconnected(format!(
                    "WebDriverAgent is not answering: {error}. Check that its runner is still running on the device."
                )),
            },
        )
    }

    /// A route that needs no session. The value of a successful reply.
    fn sessionless(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, CallFailure> {
        self.sessionless_bounded(Bound::Request(timeout), method, path, body)
    }

    fn sessionless_bounded(
        &self,
        bound: Bound,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, CallFailure> {
        let response = self.send(bound, method, path, body)?;
        match interpret(&response) {
            Outcome::Value(value) => Ok(value),
            Outcome::NoSuchCommand => Err(CallFailure::NoSuchCommand(path.to_owned())),
            Outcome::InvalidSession => Err(CallFailure::Failed(DriverError::Failed(
                "WebDriverAgent reported an invalid session for a call that needs none.".to_owned(),
            ))),
            Outcome::Error(message) => Err(CallFailure::Failed(DriverError::Failed(format!(
                "WebDriverAgent could not do {path}: {message}"
            )))),
        }
    }

    /// A session route (`/session/<id><suffix>`). If WebDriverAgent says the
    /// session is gone — another client took over, or the runner restarted — one
    /// replacement is made and the call is retried once.
    fn session_call(
        &self,
        method: &str,
        suffix: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, CallFailure> {
        self.session_call_bounded(Bound::Request(timeout), method, suffix, body)
    }

    fn session_call_bounded(
        &self,
        bound: Bound,
        method: &str,
        suffix: &str,
        body: Option<&Value>,
    ) -> Result<Value, CallFailure> {
        let mut replaced = false;
        loop {
            let id = lock(&self.session).id.clone();
            let path = format!("/session/{id}{suffix}");
            let response = self.send(bound, method, &path, body)?;
            match interpret(&response) {
                Outcome::Value(value) => return Ok(value),
                Outcome::InvalidSession if !replaced => {
                    replaced = true;
                    self.replace_session(&id, bound)?;
                }
                Outcome::InvalidSession => {
                    return Err(CallFailure::Failed(DriverError::Failed(
                        "WebDriverAgent keeps reporting that its session does not exist, even right after one was made."
                            .to_owned(),
                    )))
                }
                Outcome::NoSuchCommand => return Err(CallFailure::NoSuchCommand(suffix.to_owned())),
                Outcome::Error(message) => {
                    return Err(CallFailure::Failed(DriverError::Failed(format!(
                        "WebDriverAgent could not do {suffix}: {message}"
                    ))))
                }
            }
        }
    }

    /// Replaces the session `failed` with the one WebDriverAgent has now, or a new
    /// one. Looking first means a takeover by Appium is followed rather than fought:
    /// creating a session would end the one that took over. Looking and creating each
    /// take what is left of `bound`, and so does waiting for another caller's replacement
    /// to end ([`Self::start_replacing`]).
    fn replace_session(&self, failed: &str, bound: Bound) -> DriverResult<()> {
        let _only_one = self.start_replacing(bound)?;
        if lock(&self.session).id != failed {
            // Another call already replaced it.
            return Ok(());
        }
        self.ensure_open()?;
        let status = read_status_within(
            self.connector.as_ref(),
            self.port,
            bound.within(STATUS_TIMEOUT)?,
        )?;
        let (session, how) = match status.session_id {
            Some(id) if id != failed => (
                Session { id, owned: false },
                "following the session another client made",
            ),
            _ => (
                create_session_within(
                    self.connector.as_ref(),
                    self.port,
                    bound.within(SESSION_TIMEOUT)?,
                )?,
                "created a new session",
            ),
        };
        self.log(format!("the WebDriverAgent session was lost; {how}"));
        let owned = session.owned;
        let id = session.id.clone();
        *lock(&self.session) = session;
        if owned && self.closed.load(Ordering::Acquire) {
            // Closed while the replacement was being made: nobody will use it.
            self.delete_session(&id);
        }
        Ok(())
    }

    /// The right to replace the session, which one caller holds at a time. Another caller's
    /// replacement may take up to 70 seconds ([`STATUS_TIMEOUT`] for the status read, then
    /// [`SESSION_TIMEOUT`] for a session created while the app settles); a request of the
    /// driver's own ([`Bound::Request`]) waits it out, but a call bounded as a whole
    /// ([`Bound::Call`]) waits only for what it has left, and then says another call is at it
    /// instead of holding the screen's input lock past its time.
    fn start_replacing(&self, bound: Bound) -> DriverResult<MutexGuard<'_, ()>> {
        if matches!(bound, Bound::Request(_)) {
            return Ok(lock(&self.replacing));
        }
        loop {
            match self.replacing.try_lock() {
                Ok(only_one) => return Ok(only_one),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {}
            }
            let left = bound
                .left()
                .map_err(|_| DriverError::Failed(replacement_under_way()))?;
            thread::sleep(left.min(REPLACING_POLL));
        }
    }

    fn delete_session(&self, id: &str) {
        let _ = http::request(
            self.connector.as_ref(),
            self.port,
            "DELETE",
            &format!("/session/{id}"),
            None,
            CLOSE_TIMEOUT,
        );
    }

    /// Takes a screenshot, learns or re-learns the screen's geometry from it, and
    /// caches it. `announce` sends the [`DriverEvent`]s; the capture made inside
    /// [`connect`] stays quiet because nobody holds the driver yet. `remeasure`
    /// reads the window again whatever it last was ([`Driver::remeasure`]).
    fn capture(&self, announce: bool, remeasure: bool) -> DriverResult<Frame> {
        // The picture is of the screen when it was asked for; the rest is encoding
        // and transfer. A caller comparing it with the time of an input needs that.
        let asked = Instant::now();
        let value = self.sessionless("GET", "/screenshot", None, SCREENSHOT_TIMEOUT)?;
        let text = value.as_str().ok_or_else(|| {
            DriverError::Failed("WebDriverAgent's screenshot reply holds no image.".to_owned())
        })?;
        let png = decode_base64(text)?;
        let (width, height, rgba) = decode_png(&png)?;
        self.learn_screen_size(width, height, announce, remeasure)?;
        let seq = self.frame_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let frame = Frame {
            width,
            height,
            rgba: Arc::from(rgba),
            seq,
            captured_at: asked,
        };
        *lock(&self.last_frame) = Some(frame.clone());
        if announce {
            (self.events)(DriverEvent::Frame);
        }
        Ok(frame)
    }

    /// The window size in points changes with the screenshot's size (a rotation), so
    /// it is read again then — and at every capture while the two disagree in
    /// proportion ([`MAX_SKEW`]), since an app leaving Split View changes its window
    /// without changing the screenshot. Going into Split View, Slide Over or Stage
    /// Manager does not change the screenshot either, so the window is also read
    /// again when asked to (`remeasure`: a capture the model's input will be
    /// placed by) and once its last reading is [`WINDOW_RECHECK`] old.
    fn learn_screen_size(
        &self,
        width: u32,
        height: u32,
        announce: bool,
        remeasure: bool,
    ) -> DriverResult<()> {
        let known = *lock(&self.geometry);
        let needed = known.pixels != (width, height) || known.skew() > MAX_SKEW;
        if !needed && !remeasure && !self.window_due() {
            return Ok(());
        }
        let read = self
            .session_call("GET", "/window/size", None, QUERY_TIMEOUT)
            .map_err(DriverError::from);
        *lock(&self.window_read) = Some(Instant::now());
        let value = match read {
            Ok(value) => value,
            // Only a check-up of a window that still fits the picture failed: the
            // picture is good and the geometry as good as it was, so neither is
            // thrown away for it. A lost connection is still a lost connection.
            Err(error) if !needed && !matches!(error, DriverError::Disconnected(_)) => {
                self.log(format!(
                    "could not read the window size again, so the last one is kept: {}",
                    error.message()
                ));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let points = window_points(&value)?;
        let geometry = Geometry {
            pixels: (width, height),
            points,
        };
        let (ratio_x, ratio_y) = (geometry.ratio_x(), geometry.ratio_y());
        if !(MIN_RATIO..=MAX_RATIO).contains(&ratio_x)
            || !(MIN_RATIO..=MAX_RATIO).contains(&ratio_y)
        {
            return Err(DriverError::Failed(format!(
                "WebDriverAgent's screenshot ({width}x{height} pixels) does not fit its window ({}x{} points), so taps cannot be placed.",
                points.0, points.1
            )));
        }
        *lock(&self.geometry) = geometry;
        if geometry == known {
            return Ok(());
        }
        if announce {
            self.log(geometry.describe());
            if known.pixels != (width, height) {
                (self.events)(DriverEvent::Resized { width, height });
            }
        }
        if geometry.skew() > MAX_SKEW {
            self.log(format!(
                "the screenshot and the window have different proportions ({ratio_x:.2} and {ratio_y:.2} pixels per point): the app is windowed or turned, and input is refused until they agree"
            ));
        } else if known.pixels == (width, height) {
            self.log("the window is laid out like the screenshot again; input works".to_owned());
        }
        Ok(())
    }

    /// The geometry a gesture is placed with. Refused while the window and the
    /// screenshot disagree in proportion: the pixel/point ratio would differ
    /// per axis, and WebDriverAgent places a windowed app's taps from the app
    /// window's corner, not the screen's, so every touch would land somewhere
    /// the model did not aim.
    fn input_geometry(&self) -> DriverResult<Geometry> {
        let known = self.geometry()?;
        // The window can change with nothing captured since (the person puts the
        // app into Split View on the device itself): an old reading is checked
        // before a touch is placed by it.
        if self.window_due() {
            self.learn_screen_size(known.pixels.0, known.pixels.1, true, false)?;
        }
        let geometry = self.geometry()?;
        if geometry.skew() > MAX_SKEW {
            return Err(DriverError::Failed(format!(
                "the app's window ({}x{} points) is not laid out like the screenshot ({}x{} pixels): it is in Split View, Slide Over or Stage Manager, or turned relative to the screen, so a touch cannot be placed. Bring the app full screen or turn the device back, then take a new preview_screenshot or preview_snapshot",
                geometry.points.0, geometry.points.1, geometry.pixels.0, geometry.pixels.1
            )));
        }
        Ok(geometry)
    }

    /// Whether the window's last reading is old enough to read it again.
    fn window_due(&self) -> bool {
        lock(&self.window_read).map_or(true, |at| at.elapsed() >= self.window_recheck)
    }

    fn geometry(&self) -> DriverResult<Geometry> {
        let geometry = *lock(&self.geometry);
        if geometry.pixels == (0, 0) {
            return Err(DriverError::Failed(
                "the screen's size is not known yet; take a screenshot first.".to_owned(),
            ));
        }
        Ok(geometry)
    }

    /// Runs a pointer gesture through its WebDriverAgent route, or through W3C
    /// `/actions` when the route does not exist (and remembers that it does not).
    fn gesture(
        &self,
        route: &str,
        body: Value,
        fallback: impl FnOnce() -> Value,
        timeout: Duration,
    ) -> DriverResult<()> {
        if !lock(&self.unsupported).contains(route) {
            match self.session_call("POST", route, Some(&body), timeout) {
                Ok(_) => return Ok(()),
                Err(CallFailure::NoSuchCommand(_)) => {
                    lock(&self.unsupported).insert(route.to_owned());
                    self.log(format!(
                        "this WebDriverAgent has no {route}; using W3C pointer actions instead"
                    ));
                }
                Err(CallFailure::Failed(error)) => return Err(error),
            }
        }
        match self.session_call("POST", "/actions", Some(&fallback()), timeout) {
            Ok(_) => Ok(()),
            Err(CallFailure::NoSuchCommand(_)) => Err(DriverError::Unsupported(format!(
                "this WebDriverAgent supports neither {route} nor W3C pointer actions."
            ))),
            Err(CallFailure::Failed(error)) => Err(error),
        }
    }

    /// `GET /source`, which has what `bound` allows to answer (the walk of the accessibility
    /// tree is what takes the time), converted to a [`UiNode`] tree.
    fn source_tree(&self, bound: Bound) -> DriverResult<Option<UiNode>> {
        self.ensure_open()?;
        let value = self
            .sessionless_bounded(bound, "GET", SOURCE_ROUTE, None)
            .map_err(DriverError::from)?;
        let geometry = self.geometry()?;
        let mut budget = MAX_TREE_NODES;
        convert_node(&value, &geometry, 0, &mut budget, true)
            .map(Some)
            .ok_or_else(|| {
                DriverError::Failed(
                    "WebDriverAgent returned no UI tree for this screen.".to_owned(),
                )
            })
    }

    fn type_text(&self, text: &str) -> DriverResult<()> {
        if text.is_empty() {
            return Ok(());
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(DriverError::Failed(format!(
                "that text is longer than {MAX_TEXT_CHARS} characters; type it in pieces."
            )));
        }
        let characters: Vec<String> = text.chars().map(String::from).collect();
        let timeout = GESTURE_TIMEOUT + Duration::from_millis(50 * characters.len() as u64);
        self.session_call(
            "POST",
            "/wda/keys",
            Some(&json!({"value": characters})),
            timeout,
        )
        .map(|_| ())
        .map_err(|failure| match failure {
            CallFailure::Failed(DriverError::Failed(message)) => DriverError::Failed(format!(
                "{message} (WebDriverAgent types only into a focused text field; tap one first.)"
            )),
            other => other.into(),
        })
    }

    fn press_button(&self, name: &str) -> DriverResult<()> {
        self.session_call(
            "POST",
            "/wda/pressButton",
            Some(&json!({"name": name})),
            GESTURE_TIMEOUT,
        )
        .map(|_| ())
        .map_err(DriverError::from)
    }
}

fn window_points(value: &Value) -> DriverResult<(f64, f64)> {
    let side = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_f64)
            .filter(|side| side.is_finite() && *side > 0.0 && *side <= 100_000.0)
    };
    match (side("width"), side("height")) {
        (Some(width), Some(height)) => Ok((width, height)),
        _ => Err(DriverError::Failed(
            "WebDriverAgent's window size is missing or not a positive size.".to_owned(),
        )),
    }
}

fn decode_base64(text: &str) -> DriverResult<Vec<u8>> {
    // WebDriverAgent writes plain base64, but intermediaries (and Appium's own
    // proxies) have been seen to wrap it in lines, and padding is not worth failing on.
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let decoded = if text.bytes().any(|byte| byte.is_ascii_whitespace()) {
        let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        engine.decode(compact)
    } else {
        engine.decode(text)
    };
    decoded.map_err(|_| {
        DriverError::Failed("WebDriverAgent's screenshot is not valid base64.".to_owned())
    })
}

/// A PNG as the tightly packed opaque RGBA a [`Frame`] holds.
fn decode_png(bytes: &[u8]) -> DriverResult<(u32, u32, Vec<u8>)> {
    let broken = |what: &str| {
        DriverError::Failed(format!(
            "WebDriverAgent's screenshot is not a usable PNG: {what}."
        ))
    };
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut limits = png::Limits::default();
    limits.bytes = PNG_WORKING_BYTES;
    decoder.set_limits(limits);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder
        .read_info()
        .map_err(|error| broken(&error.to_string()))?;
    let (width, height) = reader.info().size();
    if width == 0
        || height == 0
        || width > MAX_FRAME_SIDE
        || height > MAX_FRAME_SIDE
        || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
    {
        return Err(broken(&format!(
            "its size {width}x{height} is out of range"
        )));
    }
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| broken(&error.to_string()))?;
    if info.bit_depth != png::BitDepth::Eight || info.width != width || info.height != height {
        return Err(broken("its pixel format is unexpected"));
    }
    buffer.truncate(info.buffer_size());
    let pixel_count = width as usize * height as usize;
    let rgba = match info.color_type {
        // The usual case: the buffer already is the picture and only needs to be made opaque.
        png::ColorType::Rgba => {
            for pixel in buffer.chunks_exact_mut(4) {
                pixel[3] = 255;
            }
            buffer
        }
        png::ColorType::Rgb => {
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            for pixel in buffer.chunks_exact(3) {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
            rgba
        }
        png::ColorType::Grayscale => {
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            for value in &buffer {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
            rgba
        }
        png::ColorType::GrayscaleAlpha => {
            let mut rgba = Vec::with_capacity(pixel_count * 4);
            for pixel in buffer.chunks_exact(2) {
                rgba.extend_from_slice(&[pixel[0], pixel[0], pixel[0], 255]);
            }
            rgba
        }
        png::ColorType::Indexed => return Err(broken("its palette was not expanded")),
    };
    if rgba.len() != pixel_count * 4 {
        return Err(broken("its pixel data is the wrong length"));
    }
    Ok((width, height, rgba))
}

/// W3C pointer actions for one touch.
fn pointer_sequence(steps: Vec<Value>) -> Value {
    json!({"actions": [{
        "type": "pointer",
        "id": "finger1",
        "parameters": {"pointerType": "touch"},
        "actions": steps,
    }]})
}

fn pointer_move(x: i64, y: i64, duration_ms: u32) -> Value {
    json!({"type": "pointerMove", "duration": duration_ms, "x": x, "y": y, "origin": "viewport"})
}

fn pointer_down() -> Value {
    json!({"type": "pointerDown", "button": 0})
}

fn pointer_up() -> Value {
    json!({"type": "pointerUp", "button": 0})
}

fn pause(duration_ms: u32) -> Value {
    json!({"type": "pause", "duration": duration_ms})
}

/// Between the taps of a multi-tap and between a press and its release.
const TAP_GAP_MS: u32 = 40;

fn tap_steps(x: i64, y: i64, count: u8) -> Vec<Value> {
    let mut steps = vec![pointer_move(x, y, 0)];
    for tap in 0..count {
        if tap > 0 {
            steps.push(pause(TAP_GAP_MS));
        }
        steps.push(pointer_down());
        steps.push(pause(TAP_GAP_MS));
        steps.push(pointer_up());
    }
    steps
}

impl Driver for WdaDriver {
    fn info(&self) -> DriverInfo {
        let geometry = *lock(&self.geometry);
        DriverInfo {
            platform: Platform::Ios,
            label: self.label.clone(),
            width: geometry.pixels.0,
            height: geometry.pixels.1,
            has_ui_tree: true,
        }
    }

    fn frame(&self, fresh: bool) -> DriverResult<Frame> {
        self.ensure_open()?;
        if !fresh {
            if let Some(frame) = lock(&self.last_frame).clone() {
                return Ok(frame);
            }
        }
        self.capture(true, false)
    }

    /// WebDriverAgent's own MJPEG stream: JPEGs off the device as it takes them, against the base64
    /// PNG of a screenshot. Its port has to be reachable next to the HTTP one, as `iproxy` forwards
    /// either; where it is not, the captures go on as before.
    fn stream(&self, stop: &(dyn Fn() -> bool + Sync), sink: &mut dyn FnMut(ScreenUpdate)) -> DriverResult<()> {
        use std::io::Write as _;
        self.ensure_open()?;
        // Older runners have no such settings, and that only makes the stream slower.
        let settings = serde_json::json!({ "settings": { "mjpegServerFramerate": MJPEG_FRAMERATE, "mjpegServerScreenshotQuality": MJPEG_QUALITY, "mjpegScalingFactor": 100 } });
        let _ = self.session_call("POST", "/appium/settings", Some(&settings), MJPEG_SETTINGS_TIMEOUT);
        let mut connection = self
            .connector
            .connect(MJPEG_PORT, MJPEG_CONNECT)
            .map_err(|error| DriverError::Unsupported(format!("WebDriverAgent's picture stream is not reachable on port {MJPEG_PORT}: {error}")))?;
        connection.handle().set_deadline(None);
        connection
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .and_then(|()| connection.flush())
            .map_err(|error| DriverError::Unsupported(format!("WebDriverAgent's picture stream could not be asked for: {error}")))?;
        // Something to show until the stream's first frame.
        if let Some(frame) = lock(&self.last_frame).clone() {
            sink(ScreenUpdate::Full { width: frame.width, height: frame.height, rgba: frame.rgba });
        }
        let (reader, _writer, handle) = connection.split();
        let chunks = Chunks::new(reader, handle);
        let mut jpegs = Jpegs::default();
        let mut frames = 0_u64;
        while !stop() {
            match chunks.next(MJPEG_IDLE_WAIT) {
                Next::Bytes(bytes) => {
                    jpegs.push(&bytes).map_err(DriverError::Failed)?;
                    if let Some(jpeg) = jpegs.newest() {
                        // One bad frame is not the stream's end.
                        if let Ok((width, height, pixels)) = decode_jpeg(&jpeg) {
                            frames += 1;
                            sink(ScreenUpdate::Full { width, height, rgba: Arc::from(pixels) });
                        }
                    }
                }
                Next::Quiet => {}
                Next::Ended if frames == 0 => return Err(DriverError::Unsupported("WebDriverAgent's picture stream ended without a frame".to_owned())),
                Next::Ended => return Ok(()),
            }
        }
        Ok(())
    }

    fn remeasure(&self) -> DriverResult<Frame> {
        self.ensure_open()?;
        self.capture(true, true)
    }

    fn tap(&self, x: u32, y: u32, count: u8) -> DriverResult<()> {
        self.ensure_open()?;
        let geometry = self.input_geometry()?;
        let (px, py) = geometry.to_points(x, y);
        let point = json!({"x": px, "y": py});
        match count {
            0 | 1 => self.gesture(
                "/wda/tap",
                point,
                || pointer_sequence(tap_steps(px, py, 1)),
                GESTURE_TIMEOUT,
            ),
            2 => self.gesture(
                "/wda/doubleTap",
                point,
                || pointer_sequence(tap_steps(px, py, 2)),
                GESTURE_TIMEOUT,
            ),
            // WebDriverAgent has no triple tap that works at a point, so three taps in
            // quick succession; each is a request, so this is as fast as the link.
            3 => {
                for _ in 0..3 {
                    self.gesture(
                        "/wda/tap",
                        point.clone(),
                        || pointer_sequence(tap_steps(px, py, 1)),
                        GESTURE_TIMEOUT,
                    )?;
                }
                Ok(())
            }
            _ => Err(DriverError::Unsupported(
                "a touch screen takes one to three taps at a time.".to_owned(),
            )),
        }
    }

    fn long_press(&self, x: u32, y: u32, ms: u32) -> DriverResult<()> {
        self.ensure_open()?;
        let geometry = self.input_geometry()?;
        let (px, py) = geometry.to_points(x, y);
        let ms = ms.min(MAX_HOLD_MS);
        self.gesture(
            "/wda/touchAndHold",
            json!({"x": px, "y": py, "duration": f64::from(ms) / 1000.0}),
            || {
                pointer_sequence(vec![
                    pointer_move(px, py, 0),
                    pointer_down(),
                    pause(ms),
                    pointer_up(),
                ])
            },
            GESTURE_TIMEOUT + Duration::from_millis(u64::from(ms)),
        )
    }

    fn swipe(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> DriverResult<()> {
        self.ensure_open()?;
        let geometry = self.input_geometry()?;
        let (from_x, from_y) = geometry.to_points(from.0, from.1);
        let (to_x, to_y) = geometry.to_points(to.0, to.1);
        let ms = ms.clamp(1, MAX_HOLD_MS);
        if (from_x, from_y) == (to_x, to_y) {
            // A drag that goes nowhere is a press held for as long as it was asked to last.
            return self.long_press(from.0, from.1, ms);
        }
        let distance = ((to_x - from_x) as f64).hypot((to_y - from_y) as f64);
        // pressAndDragWithVelocity covers the distance at a speed in points per second.
        let seconds = f64::from(ms.max(50)) / 1000.0;
        let velocity = (distance / seconds).max(1.0);
        self.gesture(
            "/wda/pressAndDragWithVelocity",
            json!({
                "fromX": from_x,
                "fromY": from_y,
                "toX": to_x,
                "toY": to_y,
                "pressDuration": 0.05,
                "holdDuration": 0,
                "velocity": velocity,
            }),
            || {
                pointer_sequence(vec![
                    pointer_move(from_x, from_y, 0),
                    pointer_down(),
                    pause(50),
                    pointer_move(to_x, to_y, ms),
                    pointer_up(),
                ])
            },
            GESTURE_TIMEOUT + Duration::from_millis(u64::from(ms)),
        )
    }

    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
        // A phone's key has no separate release.
        if down == Some(false) {
            return Ok(());
        }
        self.ensure_open()?;
        match key {
            Key::Char(character) => self.type_text(&character.to_string()),
            Key::Named(named) => match named {
                NamedKey::Enter => self.type_text("\n"),
                NamedKey::Backspace => self.type_text("\u{8}"),
                NamedKey::Tab => self.type_text("\t"),
                NamedKey::Space => self.type_text(" "),
                NamedKey::HomeScreen => self
                    .sessionless("POST", "/wda/homescreen", None, GESTURE_TIMEOUT)
                    .map(|_| ())
                    .map_err(DriverError::from),
                NamedKey::VolumeUp => self.press_button("volumeUp"),
                NamedKey::VolumeDown => self.press_button("volumeDown"),
                NamedKey::Power => Err(DriverError::Unsupported(
                    "WebDriverAgent cannot press the power button.".to_owned(),
                )),
                NamedKey::Back | NamedKey::AppSwitch => Err(DriverError::Unsupported(
                    "an iPhone has no Back or app-switch button; use the on-screen controls or swipe, and key \"Home\" for the home screen.".to_owned(),
                )),
                NamedKey::Shift | NamedKey::Control | NamedKey::Alt | NamedKey::Meta | NamedKey::CapsLock => {
                    Err(DriverError::Unsupported(
                        "WebDriverAgent cannot hold modifier keys.".to_owned(),
                    ))
                }
                other => Err(DriverError::Unsupported(format!(
                    "WebDriverAgent can type text, Enter, Backspace and Tab, and press Home and the volume buttons; it cannot press {other:?}. Use the on-screen controls or swipes."
                ))),
            },
        }
    }

    fn chord(&self, chord: &KeyChord) -> DriverResult<()> {
        if !chord.modifiers.is_empty() {
            return Err(DriverError::Unsupported(
                "WebDriverAgent cannot press keys with modifiers held (no shortcuts); type text or use the on-screen controls.".to_owned(),
            ));
        }
        self.key(&chord.key, None)
    }

    fn text(&self, text: &str) -> DriverResult<()> {
        self.ensure_open()?;
        self.type_text(text)
    }

    fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
        self.source_tree(Bound::Request(SOURCE_TIMEOUT))
    }

    /// Bounded as a whole call is ([`Bound::Call`]): a read that outlasts `timeout` is too slow,
    /// not a WebDriverAgent that is gone.
    fn ui_tree_within(&self, timeout: Duration) -> DriverResult<Option<UiNode>> {
        self.source_tree(Bound::call(timeout.min(SOURCE_TIMEOUT)))
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (id, owned) = {
            let session = lock(&self.session);
            (session.id.clone(), session.owned)
        };
        // A session another client made is theirs to end.
        if owned {
            self.delete_session(&id);
        }
    }

    fn ios(&self) -> Option<&dyn IosControl> {
        Some(self)
    }
}

impl IosControl for WdaDriver {
    /// A route is one of the fixed strings the iOS tools name, never anything the model
    /// wrote: [`check_route`] refuses the rest, and the few routes that would end
    /// WebDriverAgent or replace its session. A session route follows the driver's
    /// session rules (a lost one is replaced once, another client's followed, none created
    /// while one exists); `timeout` bounds the call as a whole, replacement included, and
    /// a call that outlives it answers `Failed` — the connection is not in doubt.
    fn call(
        &self,
        method: &str,
        route: WdaRoute<'_>,
        body: Option<&Value>,
        timeout: Duration,
    ) -> DriverResult<Value> {
        if !matches!(method, "GET" | "POST" | "DELETE") {
            return Err(DriverError::Failed(format!(
                "WebDriverAgent calls are GET, POST or DELETE, not \"{}\".",
                crate::preview_launch_config::sanitize_for_message(method)
            )));
        }
        self.ensure_open()?;
        let bound = Bound::call(timeout);
        match route {
            WdaRoute::Session(suffix) => {
                check_route(suffix, false)?;
                self.session_call_bounded(bound, method, suffix, body)
            }
            WdaRoute::Root(path) => {
                check_route(path, true)?;
                self.sessionless_bounded(bound, method, path, body)
            }
        }
        .map_err(DriverError::from)
    }
}

/// Refuses a route that is not a fixed one: it starts with `/`, is made of letters,
/// digits and `/ _ - .` only, has no `..` or `//`, and carries no query or fragment
/// ([`SOURCE_ROUTE`] is the one fixed query). A route outside any session may not be
/// `/session` or below it (creating a session would end every other client's, and
/// `DELETE /session/<id>` ends one), and `/wda/shutdown` stops WebDriverAgent whichever
/// way it is reached. The element routes are two: [`ELEMENTS_ROUTE`], and `/element/<id>/clear`
/// with an id that is [`valid_element_id`]; any other route that starts `/element` is refused.
fn check_route(route: &str, outside_a_session: bool) -> DriverResult<()> {
    let plain = route.len() >= 2
        && route.len() <= MAX_ROUTE_BYTES
        && route.starts_with('/')
        && !route.contains("..")
        && !route.contains("//")
        && route
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'-' | b'.'));
    let session = outside_a_session && (route == "/session" || route.starts_with("/session/"));
    let shutdown = route.ends_with("/wda/shutdown");
    let element = !route.starts_with("/element")
        || route == ELEMENTS_ROUTE
        || route
            .strip_prefix("/element/")
            .and_then(|rest| rest.strip_suffix("/clear"))
            .is_some_and(valid_element_id);
    if (plain || route == SOURCE_ROUTE) && !session && !shutdown && element {
        return Ok(());
    }
    Err(DriverError::Failed(format!(
        "refusing the WebDriverAgent route \"{}\": only the fixed routes of the iOS tools are called.",
        crate::preview_launch_config::sanitize_for_message(route)
    )))
}

/// Collapses control characters to spaces and cuts to `limit` characters.
fn clean_text(text: &str, limit: usize) -> String {
    clean_text_flagged(text, limit).text
}

/// A text after [`clean_text`], and what the cleaning did to it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cleaned {
    text: String,
    /// The cut shortened the text.
    truncated: bool,
    /// The text is not the one the device gave: trimmed, control characters replaced by
    /// spaces, or cut. (A cut text is also altered.)
    altered: bool,
}

/// [`clean_text`], also saying what it changed.
fn clean_text_flagged(text: &str, limit: usize) -> Cleaned {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= limit {
        return Cleaned {
            altered: trimmed != text,
            text: trimmed.to_owned(),
            truncated: false,
        };
    }
    let mut cut: String = trimmed.chars().take(limit).collect();
    cut.push('…');
    Cleaned {
        text: cut,
        truncated: true,
        altered: true,
    }
}

/// WebDriverAgent writes flags as the strings `"1"`/`"0"`; other servers use
/// booleans or numbers.
fn flag(value: Option<&Value>, default: bool) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => {
            !(text.is_empty() || text == "0" || text.eq_ignore_ascii_case("false"))
        }
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        _ => default,
    }
}

fn text_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    text_field_flagged(object, key).map(|field| field.text)
}

/// [`text_field`], also saying what the cleaning and the length cap did to the text.
fn text_field_flagged(object: &Map<String, Value>, key: &str) -> Option<Cleaned> {
    raw_text_field(object, key).filter(|field| !field.text.is_empty())
}

/// The text of `key` as [`text_field_flagged`] reads it, but kept when cleaning left nothing: a
/// text of spaces and control characters is no text in a tree, yet it was not the device's
/// empty one either ([`Cleaned::altered`]).
fn raw_text_field(object: &Map<String, Value>, key: &str) -> Option<Cleaned> {
    Some(match object.get(key)? {
        Value::String(text) => clean_text_flagged(text, MAX_TEXT_FIELD),
        Value::Number(number) => Cleaned {
            text: number.to_string(),
            truncated: false,
            altered: false,
        },
        Value::Bool(flag) => Cleaned {
            text: flag.to_string(),
            truncated: false,
            altered: false,
        },
        _ => return None,
    })
}

/// A rect in points as a rect in screenshot pixels. Both edges are rounded, not the
/// size, so neighbouring elements keep sharing an edge.
fn pixel_rect(rect: &Value, geometry: &Geometry) -> Option<Rect> {
    let number = |key: &str| {
        rect.get(key)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite())
    };
    let (x, y, width, height) = (
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    );
    let (ratio_x, ratio_y) = (geometry.ratio_x(), geometry.ratio_y());
    let edge = |value: f64| value.round().clamp(-1_000_000.0, 1_000_000.0) as i64;
    let (left, top) = (edge(x * ratio_x), edge(y * ratio_y));
    let (right, bottom) = (edge((x + width) * ratio_x), edge((y + height) * ratio_y));
    Some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left).max(0) as u32,
        height: (bottom - top).max(0) as u32,
    })
}

/// One element of `/source?format=json`. Invisible elements are dropped with their
/// subtrees (the root is kept, whatever it says), and the tree is cut at
/// [`MAX_TREE_NODES`] elements and [`MAX_TREE_DEPTH`] levels.
fn convert_node(
    node: &Value,
    geometry: &Geometry,
    depth: usize,
    budget: &mut usize,
    is_root: bool,
) -> Option<UiNode> {
    let object = node.as_object()?;
    if !is_root && !flag(object.get("isVisible"), true) {
        return None;
    }
    if *budget == 0 {
        return None;
    }
    *budget -= 1;

    let role = object
        .get("type")
        .and_then(Value::as_str)
        .map(|kind| kind.strip_prefix("XCUIElementType").unwrap_or(kind))
        .map(|kind| clean_text(kind, 60))
        .filter(|kind| !kind.is_empty())
        .unwrap_or_else(|| "Other".to_owned());
    let label_raw = raw_text_field(object, "label");
    let name_raw = raw_text_field(object, "name");
    let value_raw = raw_text_field(object, "value");
    let hint_raw = raw_text_field(object, "placeholderValue");
    // Whether cleaning changed any text the node carries (even one it then left empty): a
    // matcher built from them would not be the device's own.
    let inexact = [&label_raw, &name_raw, &value_raw, &hint_raw]
        .into_iter()
        .any(|field| field.as_ref().is_some_and(|field| field.altered));
    let present = |field: Option<Cleaned>| field.filter(|field| !field.text.is_empty());
    let label = present(label_raw);
    let name_field = present(name_raw);
    let (name, name_truncated) = label
        .clone()
        .or_else(|| name_field.clone())
        .map(|field| (field.text, field.truncated))
        .unwrap_or_default();
    // The accessibility identifier: `rawIdentifier` where WebDriverAgent sends it,
    // else the `name` field when it says something the label does not.
    let id = text_field(object, "rawIdentifier")
        .into_iter()
        .chain(name_field.map(|field| field.text))
        .find(|candidate| *candidate != name);
    let value = present(value_raw);
    let hint = present(hint_raw);
    let truncated = name_truncated
        || value.as_ref().is_some_and(|field| field.truncated)
        || hint.as_ref().is_some_and(|field| field.truncated);
    let password = role == "SecureTextField";
    let mut children = Vec::new();
    if depth < MAX_TREE_DEPTH {
        if let Some(Value::Array(items)) = object.get("children") {
            for child in items {
                if *budget == 0 {
                    break;
                }
                if let Some(child) = convert_node(child, geometry, depth + 1, budget, false) {
                    children.push(child);
                }
            }
        }
    }
    Some(UiNode {
        role,
        name,
        value: value.map(|field| field.text),
        id,
        bounds: object
            .get("rect")
            .and_then(|rect| pixel_rect(rect, geometry)),
        enabled: flag(object.get("isEnabled"), true),
        focused: flag(object.get("isFocused"), false),
        selected: flag(object.get("isSelected"), false),
        package: None,
        clickable: false,
        // An iOS field keeps its content in `value`.
        text: None,
        hint: hint.map(|field| field.text),
        password,
        truncated,
        inexact: inexact || truncated,
        children,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::{Read, Write},
        net::{Ipv4Addr, TcpListener, TcpStream},
        thread,
    };

    use base64::engine::general_purpose::STANDARD;

    use super::*;
    use crate::remote_control::transport::LocalConnector;

    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        path: String,
        body: Option<Value>,
    }

    /// What the fake WebDriverAgent will answer, and what it was asked.
    struct FakeState {
        /// The session WebDriverAgent has right now.
        session: Option<String>,
        created: u32,
        forced_session_id: Option<String>,
        create_reply_top_level: bool,
        window: (f64, f64),
        screenshot: String,
        source: Value,
        status_override: Option<(u16, String)>,
        /// Session routes answered "unknown command".
        missing: HashSet<String>,
        /// Session routes answered with an error: status, error name, message.
        failing: HashMap<String, (u16, &'static str, &'static str)>,
        /// Routes outside any session answered with an error, by path.
        root_failing: HashMap<String, (u16, &'static str, &'static str)>,
        /// What a route answers instead of null, by `"METHOD route"` (a session route by its
        /// suffix). A route outside any session that is not set (or built in) is not there:
        /// the HTTP server's own 404.
        values: HashMap<String, Value>,
        /// Requests whose path ends with the key are answered only after this long.
        delays: Vec<(String, Duration)>,
        recorded: Vec<Recorded>,
    }

    impl FakeState {
        fn new() -> Self {
            Self {
                session: None,
                created: 0,
                forced_session_id: None,
                create_reply_top_level: false,
                window: (100.0, 200.0),
                screenshot: png_base64(200, 400),
                source: Value::Null,
                status_override: None,
                missing: HashSet::new(),
                failing: HashMap::new(),
                root_failing: HashMap::new(),
                values: HashMap::new(),
                delays: Vec::new(),
                recorded: Vec::new(),
            }
        }
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let mut data = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&[x as u8, y as u8, 7, 255]);
            }
        }
        writer.write_image_data(&data).unwrap();
        writer.finish().unwrap();
        out
    }

    fn png_base64(width: u32, height: u32) -> String {
        STANDARD.encode(png_bytes(width, height))
    }

    fn reply(value: Value, session: &Option<String>) -> (u16, String) {
        (
            200,
            json!({"value": value, "sessionId": session}).to_string(),
        )
    }

    fn error_reply(status: u16, error: &str, message: &str) -> (u16, String) {
        (
            status,
            json!({"value": {"error": error, "message": message, "traceback": ""}}).to_string(),
        )
    }

    fn respond(
        state: &mut FakeState,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, String) {
        state.recorded.push(Recorded {
            method: method.to_owned(),
            path: path.to_owned(),
            body,
        });
        let session = state.session.clone();
        match (method, path) {
            ("GET", "/status") => {
                if let Some((status, body)) = &state.status_override {
                    return (*status, body.clone());
                }
                return reply(
                    json!({
                        "ready": true,
                        "message": "WebDriverAgent is ready to accept commands",
                        "state": "success",
                        "os": {"name": "iOS", "version": "17.5"},
                        "ios": {"ip": "192.168.1.5"},
                        "build": {"time": "now"},
                        "device": "iphone",
                    }),
                    &session,
                );
            }
            ("POST", "/session") => {
                state.created += 1;
                let id = state
                    .forced_session_id
                    .clone()
                    .unwrap_or_else(|| format!("00000000-0000-0000-0000-{:012}", state.created));
                state.session = Some(id.clone());
                let body = if state.create_reply_top_level {
                    json!({"value": null, "sessionId": id})
                } else {
                    json!({"value": {"sessionId": id, "capabilities": {}}, "sessionId": id})
                };
                return (200, body.to_string());
            }
            ("GET", "/screenshot") => {
                return reply(Value::String(state.screenshot.clone()), &session)
            }
            ("GET", "/source?format=json") => return reply(state.source.clone(), &session),
            ("POST", "/wda/homescreen") => return reply(Value::Null, &session),
            _ => {}
        }
        if !path.starts_with("/session/") {
            if let Some((status, error, message)) = state.root_failing.get(path) {
                return error_reply(*status, error, message);
            }
            if let Some(value) = state.values.get(&format!("{method} {path}")) {
                return reply(value.clone(), &session);
            }
        }
        if let Some(rest) = path.strip_prefix("/session/") {
            let (id, suffix) = match rest.find('/') {
                Some(at) => (&rest[..at], &rest[at..]),
                None => (rest, ""),
            };
            if method == "DELETE" && suffix.is_empty() {
                if state.session.as_deref() == Some(id) {
                    state.session = None;
                }
                return reply(Value::Null, &state.session.clone());
            }
            if state.session.as_deref() != Some(id) {
                return error_reply(404, "invalid session id", "Session does not exist");
            }
            if state.missing.contains(suffix) {
                return error_reply(404, "unknown command", "Unhandled endpoint");
            }
            if let Some((status, error, message)) = state.failing.get(suffix) {
                return error_reply(*status, error, message);
            }
            if method == "GET" && suffix == "/window/size" {
                let (width, height) = state.window;
                return reply(json!({"width": width, "height": height}), &session);
            }
            if let Some(value) = state.values.get(&format!("{method} {suffix}")) {
                return reply(value.clone(), &session);
            }
            return reply(Value::Null, &session);
        }
        (404, "<html><body>404 Not Found</body></html>".to_owned())
    }

    fn serve_connection(mut stream: TcpStream, state: &Mutex<FakeState>) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => request.push(byte[0]),
                _ => return,
            }
        }
        let head = String::from_utf8_lossy(&request).into_owned();
        let mut first = head.lines().next().unwrap_or_default().split_whitespace();
        let method = first.next().unwrap_or_default().to_owned();
        let path = first.next().unwrap_or_default().to_owned();
        let length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if stream.read_exact(&mut body).is_err() {
            return;
        }
        let body = serde_json::from_slice::<Value>(&body).ok();
        let delay = lock(state)
            .delays
            .iter()
            .find(|(end, _)| path.ends_with(end.as_str()))
            .map(|(_, delay)| *delay);
        if let Some(delay) = delay {
            thread::sleep(delay);
        }
        let (status, text) = respond(&mut lock(state), &method, &path, body);
        let _ = write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        );
        let _ = stream.flush();
    }

    struct FakeWda {
        port: u16,
        state: Arc<Mutex<FakeState>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FakeWda {
        fn start(configure: impl FnOnce(&mut FakeState)) -> Self {
            let mut state = FakeState::new();
            configure(&mut state);
            let state = Arc::new(Mutex::new(state));
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (state, stop) = (state.clone(), stop.clone());
                thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => serve_connection(stream, &state),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(3));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };
            Self {
                port,
                state,
                stop,
                thread: Some(thread),
            }
        }

        fn requests(&self) -> Vec<Recorded> {
            lock(&self.state).recorded.clone()
        }

        /// `"METHOD /path"` of every request so far.
        fn lines(&self) -> Vec<String> {
            self.requests()
                .into_iter()
                .map(|request| format!("{} {}", request.method, request.path))
                .collect()
        }

        fn clear(&self) {
            lock(&self.state).recorded.clear();
        }

        fn set(&self, change: impl FnOnce(&mut FakeState)) {
            change(&mut lock(&self.state));
        }

        /// The bodies of the requests whose path ends with `suffix`.
        fn bodies(&self, suffix: &str) -> Vec<Value> {
            self.requests()
                .into_iter()
                .filter(|request| request.path.ends_with(suffix))
                .filter_map(|request| request.body)
                .collect()
        }

        fn count(&self, suffix: &str) -> usize {
            self.requests()
                .iter()
                .filter(|request| request.path.ends_with(suffix))
                .count()
        }

        fn deletes(&self) -> Vec<String> {
            self.lines()
                .into_iter()
                .filter(|line| line.starts_with("DELETE"))
                .collect()
        }
    }

    impl Drop for FakeWda {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    type EventLog = Arc<Mutex<Vec<DriverEvent>>>;

    fn open(fake: &FakeWda) -> (Arc<dyn Driver>, EventLog) {
        let log: EventLog = Arc::default();
        let sink = log.clone();
        let events: DriverEvents = Arc::new(move |event| lock(&sink).push(event));
        let driver = connect(Arc::new(LocalConnector::new()), fake.port, events)
            .expect("connect to the fake WebDriverAgent");
        (driver, log)
    }

    /// [`open`], with the window read again once its reading is `recheck` old.
    fn open_with_recheck(fake: &FakeWda, recheck: Duration) -> (WdaDriver, EventLog) {
        let log: EventLog = Arc::default();
        let sink = log.clone();
        let events: DriverEvents = Arc::new(move |event| lock(&sink).push(event));
        let driver = open_driver(Arc::new(LocalConnector::new()), fake.port, events, recheck)
            .expect("connect to the fake WebDriverAgent");
        (driver, log)
    }

    fn connect_error(fake: &FakeWda) -> DriverError {
        let events: DriverEvents = Arc::new(|_| {});
        match connect(Arc::new(LocalConnector::new()), fake.port, events) {
            Ok(_) => panic!("connect should have failed"),
            Err(error) => error,
        }
    }

    const EXISTING: &str = "AAAAAAAA-0000-4000-8000-000000000001";

    fn with_session(state: &mut FakeState) {
        state.session = Some(EXISTING.to_owned());
    }

    fn logs(log: &EventLog) -> Vec<String> {
        lock(log)
            .iter()
            .filter_map(|event| match event {
                DriverEvent::Log(line) => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_existing_session_is_reused_and_never_deleted() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        assert_eq!(
            fake.lines(),
            vec![
                "GET /status".to_owned(),
                "GET /screenshot".to_owned(),
                format!("GET /session/{EXISTING}/window/size"),
            ]
        );
        assert!(logs(&log).iter().any(|line| line.contains("reusing")));
        driver.close();
        assert!(fake.deletes().is_empty(), "{:?}", fake.lines());
        assert_eq!(lock(&fake.state).session.as_deref(), Some(EXISTING));
    }

    #[test]
    fn a_session_is_created_when_none_exists_and_only_that_one_is_deleted() {
        let fake = FakeWda::start(|_| {});
        let (driver, log) = open(&fake);
        let created = lock(&fake.state)
            .session
            .clone()
            .expect("a session was created");
        let creation = fake
            .requests()
            .into_iter()
            .find(|request| request.method == "POST" && request.path == "/session")
            .expect("POST /session");
        assert_eq!(
            creation.body,
            Some(json!({"capabilities": {"alwaysMatch": {}}}))
        );
        assert!(logs(&log).iter().any(|line| line.contains("created")));
        assert!(fake
            .lines()
            .contains(&format!("GET /session/{created}/window/size")));
        driver.close();
        driver.close();
        assert_eq!(fake.deletes(), vec![format!("DELETE /session/{created}")]);
        // After close, nothing more goes out.
        fake.clear();
        let error = driver.tap(1, 1, 1).unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn a_session_id_may_come_from_the_top_level_of_the_reply() {
        let fake = FakeWda::start(|state| state.create_reply_top_level = true);
        let (_driver, _) = open(&fake);
        let created = lock(&fake.state).session.clone().unwrap();
        assert!(fake
            .lines()
            .contains(&format!("GET /session/{created}/window/size")));
    }

    #[test]
    fn something_that_is_not_webdriveragent_is_refused() {
        for (status, body) in [
            (200, r#"{"value":{"hello":"world"}}"#),
            (200, r#"{"value":"ready"}"#),
            (200, "<html>It works</html>"),
            (404, "<html>Not here</html>"),
        ] {
            let fake = FakeWda::start(|state| {
                state.status_override = Some((status, body.to_owned()));
            });
            let error = connect_error(&fake);
            assert!(
                matches!(&error, DriverError::Failed(message) if message.contains("is not WebDriverAgent")),
                "{status} {body}: {error:?}"
            );
            // Nothing past /status was attempted.
            assert_eq!(fake.lines(), vec!["GET /status".to_owned()]);
        }
    }

    #[test]
    fn a_port_with_nothing_on_it_says_how_to_start_webdriveragent() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let events: DriverEvents = Arc::new(|_| {});
        let error = match connect(Arc::new(LocalConnector::new()), port, events) {
            Ok(_) => panic!("nothing is listening"),
            Err(error) => error,
        };
        assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
        assert!(
            error.message().contains("nothing is listening"),
            "{error:?}"
        );
        assert!(error.message().contains("iproxy"), "{error:?}");
    }

    #[test]
    fn a_not_ready_webdriveragent_is_reported_as_such() {
        let fake = FakeWda::start(|state| {
            state.status_override = Some((
                200,
                r#"{"value":{"ready":false,"message":"still starting"},"sessionId":null}"#
                    .to_owned(),
            ));
        });
        let error = connect_error(&fake);
        assert!(error.message().contains("not ready yet"), "{error:?}");
        assert!(error.message().contains("still starting"), "{error:?}");
    }

    #[test]
    fn a_session_id_that_does_not_look_like_one_never_reaches_a_path() {
        let too_long = "a".repeat(65);
        for hostile in [
            "../../wda/shutdown",
            "a b",
            "x/y",
            "",
            "\u{e9}",
            too_long.as_str(),
        ] {
            let status = json!({"value": {"ready": true}, "sessionId": hostile}).to_string();
            let fake = FakeWda::start(|state| state.status_override = Some((200, status)));
            let error = connect_error(&fake);
            assert!(
                matches!(&error, DriverError::Failed(message) if message.contains("unexpected format")),
                "{hostile:?}: {error:?}"
            );
            assert_eq!(fake.lines(), vec!["GET /status".to_owned()]);
        }
        // A number is not an id either.
        let fake = FakeWda::start(|state| {
            state.status_override =
                Some((200, r#"{"value":{"ready":true},"sessionId":12}"#.to_owned()));
        });
        assert!(connect_error(&fake).message().contains("unexpected format"));

        // Nor is one WebDriverAgent hands out when asked to create a session.
        let fake = FakeWda::start(|state| state.forced_session_id = Some("evil/../id".to_owned()));
        let error = connect_error(&fake);
        assert!(error.message().contains("usable session id"), "{error:?}");
        assert!(
            fake.lines().iter().all(|line| !line.contains("evil")),
            "{:?}",
            fake.lines()
        );
    }

    #[test]
    fn screenshots_become_rgba_and_taps_are_converted_from_pixels_to_points() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        let info = driver.info();
        assert_eq!(info.platform, Platform::Ios);
        assert_eq!((info.width, info.height), (200, 400));
        assert!(info.has_ui_tree);
        assert_eq!(info.label, "iPhone (iOS 17.5)");

        // The one taken while connecting is what a non-fresh frame returns.
        let cached = driver.frame(false).unwrap();
        assert_eq!((cached.width, cached.height, cached.seq), (200, 400, 1));
        assert_eq!(fake.count("/screenshot"), 1);
        assert_eq!(cached.rgba.len(), 200 * 400 * 4);
        assert_eq!(&cached.rgba[0..4], &[0, 0, 7, 255]);
        let at = |x: usize, y: usize| &cached.rgba[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4];
        assert_eq!(at(5, 0), &[5, 0, 7, 255]);
        assert_eq!(at(3, 9), &[3, 9, 7, 255]);

        let fresh = driver.frame(true).unwrap();
        assert_eq!(fresh.seq, 2);
        assert_eq!(fake.count("/screenshot"), 2);
        assert_eq!(
            lock(&log)
                .iter()
                .filter(|event| **event == DriverEvent::Frame)
                .count(),
            1,
            "only captures after connect are announced"
        );

        fake.clear();
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(
            fake.lines(),
            vec![format!("POST /session/{EXISTING}/wda/tap")]
        );
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 50, "y": 50})]);
        // Half a point rounds away from zero.
        fake.clear();
        driver.tap(101, 199, 1).unwrap();
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 51, "y": 100})]);
    }

    #[test]
    fn taps_swipes_and_holds_use_the_routes_wda_documents() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.clear();

        driver.tap(100, 100, 2).unwrap();
        assert_eq!(
            fake.bodies("/wda/doubleTap"),
            vec![json!({"x": 50, "y": 50})]
        );

        fake.clear();
        driver.tap(100, 100, 3).unwrap();
        assert_eq!(fake.count("/wda/tap"), 3);

        fake.clear();
        driver.long_press(100, 100, 800).unwrap();
        assert_eq!(
            fake.bodies("/wda/touchAndHold"),
            vec![json!({"x": 50, "y": 50, "duration": 0.8})]
        );

        // 200 pixels is 100 points; over half a second that is 200 points a second.
        fake.clear();
        driver.swipe((100, 100), (100, 300), 500).unwrap();
        let bodies = fake.bodies("/wda/pressAndDragWithVelocity");
        assert_eq!(bodies.len(), 1);
        let body = &bodies[0];
        assert_eq!(body["fromX"], json!(50));
        assert_eq!(body["fromY"], json!(50));
        assert_eq!(body["toX"], json!(50));
        assert_eq!(body["toY"], json!(150));
        assert_eq!(body["pressDuration"], json!(0.05));
        assert_eq!(body["holdDuration"], json!(0));
        assert_eq!(body["velocity"].as_f64(), Some(200.0));
        assert!(
            !fake
                .lines()
                .iter()
                .any(|line| line.contains("dragfromtoforduration")),
            "dragfromtoforduration presses and holds first"
        );

        // A drag that goes nowhere is a held press.
        fake.clear();
        driver.swipe((100, 100), (99, 99), 300).unwrap();
        assert_eq!(
            fake.bodies("/wda/touchAndHold"),
            vec![json!({"x": 50, "y": 50, "duration": 0.3})]
        );

        let error = driver.tap(1, 1, 4).unwrap_err();
        assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
        // No pointer on a phone.
        let error = driver.pointer(1, 1, 0).unwrap_err();
        assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
    }

    #[test]
    fn a_missing_route_falls_back_to_w3c_actions_and_is_remembered() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.missing.insert("/wda/tap".to_owned());
            state
                .missing
                .insert("/wda/pressAndDragWithVelocity".to_owned());
            state.missing.insert("/wda/touchAndHold".to_owned());
        });
        let (driver, log) = open(&fake);
        fake.clear();

        driver.tap(100, 100, 1).unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/tap"),
                format!("POST /session/{EXISTING}/actions"),
            ]
        );
        let actions = &fake.bodies("/actions")[0];
        let pointer = &actions["actions"][0];
        assert_eq!(pointer["type"], json!("pointer"));
        assert_eq!(pointer["parameters"], json!({"pointerType": "touch"}));
        let kinds: Vec<&str> = pointer["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["pointerMove", "pointerDown", "pause", "pointerUp"]);
        assert_eq!(pointer["actions"][0]["x"], json!(50));
        assert_eq!(pointer["actions"][0]["y"], json!(50));
        assert_eq!(pointer["actions"][0]["origin"], json!("viewport"));

        // The second tap does not probe the missing route again.
        fake.clear();
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(
            fake.lines(),
            vec![format!("POST /session/{EXISTING}/actions")]
        );
        assert!(logs(&log).iter().any(|line| line.contains("W3C")));

        fake.clear();
        driver.swipe((100, 100), (100, 300), 500).unwrap();
        let actions = &fake.bodies("/actions")[0];
        let steps = actions["actions"][0]["actions"].as_array().unwrap();
        let kinds: Vec<&str> = steps
            .iter()
            .map(|step| step["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "pointerMove",
                "pointerDown",
                "pause",
                "pointerMove",
                "pointerUp"
            ]
        );
        assert_eq!(steps[3]["duration"], json!(500));
        assert_eq!(steps[3]["x"], json!(50));
        assert_eq!(steps[3]["y"], json!(150));

        fake.clear();
        driver.long_press(100, 100, 700).unwrap();
        let actions = &fake.bodies("/actions")[0];
        let steps = actions["actions"][0]["actions"].as_array().unwrap();
        assert_eq!(steps[2], json!({"type": "pause", "duration": 700}));

        // Without W3C actions either there is nothing left to try.
        fake.set(|state| {
            state.missing.insert("/actions".to_owned());
        });
        let error = driver.tap(100, 100, 1).unwrap_err();
        assert!(
            matches!(&error, DriverError::Unsupported(message) if message.contains("neither")),
            "{error:?}"
        );
    }

    #[test]
    fn a_session_another_client_replaced_is_followed_not_fought() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        // Appium starts its own session.
        fake.set(|state| state.session = Some("BBBBBBBB-0000-4000-8000-000000000002".to_owned()));
        fake.clear();

        driver.tap(100, 100, 1).unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/tap"),
                "GET /status".to_owned(),
                "POST /session/BBBBBBBB-0000-4000-8000-000000000002/wda/tap".to_owned(),
            ]
        );
        assert!(logs(&log).iter().any(|line| line.contains("following")));
        // It is theirs, so closing leaves it.
        driver.close();
        assert!(fake.deletes().is_empty());
    }

    #[test]
    fn a_lost_session_is_recreated_once_and_that_one_is_ours_to_delete() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.set(|state| state.session = None);
        fake.clear();

        driver.tap(100, 100, 1).unwrap();
        let created = lock(&fake.state).session.clone().unwrap();
        assert_ne!(created, EXISTING);
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/tap"),
                "GET /status".to_owned(),
                "POST /session".to_owned(),
                format!("POST /session/{created}/wda/tap"),
            ]
        );
        // Later calls use the new session straight away.
        fake.clear();
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(
            fake.lines(),
            vec![format!("POST /session/{created}/wda/tap")]
        );

        driver.close();
        assert_eq!(fake.deletes(), vec![format!("DELETE /session/{created}")]);
    }

    #[test]
    fn calls_that_lose_the_session_together_replace_it_once() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.set(|state| state.session = None);
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let driver = driver.clone();
                thread::spawn(move || driver.tap(100, 100, 1))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(
            lock(&fake.state).created,
            1,
            "a second session would have ended the first: {:?}",
            fake.lines()
        );
    }

    #[test]
    fn a_session_that_stays_invalid_is_an_error_not_a_loop() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        // The route insists the session does not exist, even right after one was made.
        fake.set(|state| {
            state.failing.insert(
                "/wda/tap".to_owned(),
                (404, "invalid session id", "Session does not exist"),
            );
        });
        fake.clear();
        let error = driver.tap(100, 100, 1).unwrap_err();
        assert!(error.message().contains("keeps reporting"), "{error:?}");
        assert_eq!(fake.count("/wda/tap"), 2, "{:?}", fake.lines());
        assert_eq!(fake.count("/session"), 1, "one replacement, not a loop");
    }

    #[test]
    fn keys_map_to_what_webdriveragent_can_type_and_press() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);

        for (key, typed) in [
            (Key::Named(NamedKey::Enter), "\n"),
            (Key::Named(NamedKey::Backspace), "\u{8}"),
            (Key::Named(NamedKey::Tab), "\t"),
            (Key::Named(NamedKey::Space), " "),
            (Key::Char('q'), "q"),
            (Key::Char('\u{e9}'), "\u{e9}"),
        ] {
            fake.clear();
            driver.key(&key, None).unwrap();
            assert_eq!(
                fake.bodies("/wda/keys"),
                vec![json!({"value": [typed]})],
                "{key:?}"
            );
        }

        fake.clear();
        driver.key(&Key::Named(NamedKey::HomeScreen), None).unwrap();
        assert_eq!(fake.lines(), vec!["POST /wda/homescreen".to_owned()]);

        fake.clear();
        driver
            .key(&Key::Named(NamedKey::VolumeUp), Some(true))
            .unwrap();
        driver.key(&Key::Named(NamedKey::VolumeDown), None).unwrap();
        assert_eq!(
            fake.bodies("/wda/pressButton"),
            vec![json!({"name": "volumeUp"}), json!({"name": "volumeDown"})]
        );

        // Releasing a key is nothing on a phone.
        fake.clear();
        driver.key(&Key::Char('a'), Some(false)).unwrap();
        driver
            .key(&Key::Named(NamedKey::HomeScreen), Some(false))
            .unwrap();
        assert!(fake.requests().is_empty());

        // The rest is refused without asking the device.
        for key in [
            NamedKey::Escape,
            NamedKey::Up,
            NamedKey::Delete,
            NamedKey::F(5),
            NamedKey::Back,
            NamedKey::AppSwitch,
            NamedKey::Power,
            NamedKey::Shift,
            NamedKey::Meta,
            NamedKey::Home,
        ] {
            let error = driver.key(&Key::Named(key), None).unwrap_err();
            assert!(
                matches!(error, DriverError::Unsupported(_)),
                "{key:?}: {error:?}"
            );
        }
        assert!(fake.requests().is_empty(), "{:?}", fake.lines());

        // Only a chord with no modifier is a key.
        let error = driver
            .chord(&KeyChord {
                modifiers: vec![Key::Named(NamedKey::Control)],
                key: Key::Char('c'),
            })
            .unwrap_err();
        assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
        assert!(fake.requests().is_empty());
        driver
            .chord(&KeyChord {
                modifiers: vec![],
                key: Key::Named(NamedKey::Enter),
            })
            .unwrap();
        assert_eq!(fake.bodies("/wda/keys"), vec![json!({"value": ["\n"]})]);
    }

    #[test]
    fn text_is_sent_one_character_per_element_and_needs_a_focused_field() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.clear();
        driver.text("h\u{e9}\u{4e2d}\n\u{1f600}").unwrap();
        assert_eq!(
            fake.bodies("/wda/keys"),
            vec![json!({"value": ["h", "\u{e9}", "\u{4e2d}", "\n", "\u{1f600}"]})]
        );

        fake.clear();
        driver.text("").unwrap();
        assert!(fake.requests().is_empty());

        let error = driver.text(&"x".repeat(MAX_TEXT_CHARS + 1)).unwrap_err();
        assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
        assert!(fake.requests().is_empty());

        fake.set(|state| {
            state.failing.insert(
                "/wda/keys".to_owned(),
                (
                    400,
                    "invalid element state",
                    "Keyboard is not present on screen",
                ),
            );
        });
        let error = driver.text("hello").unwrap_err();
        assert!(
            error.message().contains("Keyboard is not present"),
            "{error:?}"
        );
        assert!(error.message().contains("focused text field"), "{error:?}");
    }

    #[test]
    fn a_rotation_is_noticed_from_the_screenshot_and_announced() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        assert_eq!(fake.count("/window/size"), 1);

        // The same frame size again asks nothing more.
        driver.frame(true).unwrap();
        assert_eq!(fake.count("/window/size"), 1);

        fake.set(|state| {
            state.screenshot = png_base64(400, 200);
            state.window = (200.0, 100.0);
        });
        let frame = driver.frame(true).unwrap();
        assert_eq!((frame.width, frame.height), (400, 200));
        assert_eq!(fake.count("/window/size"), 2);
        assert_eq!((driver.info().width, driver.info().height), (400, 200));
        assert!(lock(&log).contains(&DriverEvent::Resized {
            width: 400,
            height: 200
        }));

        fake.clear();
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 50, "y": 50})]);
    }

    #[test]
    fn a_screenshot_that_does_not_fit_the_window_is_refused() {
        // 200 pixels across 5 points is 40 pixels per point: not the same screen.
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.window = (5.0, 10.0);
        });
        let error = connect_error(&fake);
        assert!(
            error.message().contains("taps cannot be placed"),
            "{error:?}"
        );
        // A session it found is left alone.
        assert!(fake.deletes().is_empty());

        // One it made for this attempt is cleaned up.
        let fake = FakeWda::start(|state| {
            state.window = (5.0, 10.0);
        });
        let error = connect_error(&fake);
        assert!(
            error.message().contains("taps cannot be placed"),
            "{error:?}"
        );
        assert_eq!(fake.deletes().len(), 1, "{:?}", fake.lines());
    }

    /// An iPad app in Split View: the screenshot is the whole screen, the window half of it.
    /// Taps would land off target, so they are refused until the app is full screen again.
    #[test]
    fn a_window_that_is_not_laid_out_like_the_screenshot_refuses_input() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.window = (50.0, 200.0);
        });
        let (driver, log) = open(&fake);
        fake.clear();
        for error in [
            driver.tap(10, 10, 1).unwrap_err(),
            driver.long_press(10, 10, 500).unwrap_err(),
            driver.swipe((10, 10), (10, 100), 300).unwrap_err(),
        ] {
            assert!(
                error.message().contains("not laid out like the screenshot"),
                "{error:?}"
            );
        }
        assert!(fake.lines().is_empty(), "{:?}", fake.lines());
        assert!(logs(&log)
            .iter()
            .any(|line| line.contains("input is refused")));

        // The app goes full screen; the screenshot keeps its size, the window is read again.
        fake.set(|state| state.window = (100.0, 200.0));
        driver.frame(true).unwrap();
        assert_eq!(fake.count("/window/size"), 1);
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 50, "y": 50})]);
        // And not again while they agree.
        driver.frame(true).unwrap();
        assert_eq!(fake.count("/window/size"), 1);
    }

    /// The app goes into Split View after a full-screen window was learned. The screenshot keeps
    /// its size, so a viewer's frame right after the last reading does not notice; the model's
    /// own look reads the window again, and its taps are refused instead of landing off target.
    #[test]
    fn split_view_entered_mid_session_is_noticed_by_the_models_next_look() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.clear();
        fake.set(|state| state.window = (50.0, 200.0));
        driver.frame(true).unwrap();
        assert_eq!(fake.count("/window/size"), 0);

        let frame = driver.remeasure().unwrap();
        assert_eq!((frame.width, frame.height), (200, 400));
        assert_eq!(fake.count("/window/size"), 1);
        let error = driver.tap(10, 10, 1).unwrap_err();
        assert!(
            error.message().contains("not laid out like the screenshot"),
            "{error:?}"
        );
        assert!(fake.bodies("/wda/tap").is_empty());

        fake.set(|state| state.window = (100.0, 200.0));
        driver.remeasure().unwrap();
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 50, "y": 50})]);
    }

    /// With no look from the model, an old reading of the window is read again: by the next
    /// viewer frame, and by a touch when nothing captured since (the person changed the layout on
    /// the device). A check-up that fails keeps the window it had.
    #[test]
    fn an_old_window_reading_is_read_again_by_frames_and_touches() {
        let fake = FakeWda::start(with_session);
        let recheck = Duration::from_millis(40);
        let (driver, log) = open_with_recheck(&fake, recheck);
        fake.clear();
        fake.set(|state| state.window = (50.0, 200.0));
        thread::sleep(recheck * 2);
        driver.frame(true).unwrap();
        assert_eq!(fake.count("/window/size"), 1);
        let error = driver.tap(10, 10, 1).unwrap_err();
        assert!(
            error.message().contains("not laid out like the screenshot"),
            "{error:?}"
        );

        fake.set(|state| state.window = (100.0, 200.0));
        thread::sleep(recheck * 2);
        driver.tap(100, 100, 1).unwrap();
        assert_eq!(fake.bodies("/wda/tap"), vec![json!({"x": 50, "y": 50})]);

        fake.set(|state| {
            state.failing.insert(
                "/window/size".to_owned(),
                (500, "unknown error", "the app is busy"),
            );
        });
        thread::sleep(recheck * 2);
        let frame = driver.frame(true).unwrap();
        assert_eq!((frame.width, frame.height), (200, 400));
        driver.tap(100, 100, 1).unwrap();
        assert!(
            logs(&log)
                .iter()
                .any(|line| line.contains("the last one is kept")),
            "{:?}",
            logs(&log)
        );
    }

    #[test]
    fn wda_error_messages_reach_the_caller() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.failing.insert(
                "/wda/tap".to_owned(),
                (400, "invalid argument", "x is out of range"),
            );
        });
        let (driver, _) = open(&fake);
        let error = driver.tap(100, 100, 1).unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(error.message().contains("x is out of range"), "{error:?}");
        assert!(error.message().contains("HTTP 400"), "{error:?}");
    }

    #[test]
    fn losing_the_server_is_a_disconnection_not_a_failure() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        drop(fake);
        let error = driver.tap(100, 100, 1).unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
        let error = driver.frame(true).unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
        // The last frame is still there for whoever wants it.
        assert!(driver.frame(false).is_ok());
    }

    fn geometry_2x() -> Geometry {
        Geometry {
            pixels: (200, 400),
            points: (100.0, 200.0),
        }
    }

    #[test]
    fn the_ui_tree_is_converted_to_pixels_and_prefixes_are_dropped() {
        let source = json!({
            "type": "XCUIElementTypeApplication",
            "name": "Settings",
            "label": "Settings",
            "rect": {"x": 0, "y": 0, "width": 100, "height": 200},
            "isEnabled": "1",
            "isVisible": "1",
            "children": [{
                "type": "Window",
                "rect": {"x": 0, "y": 0, "width": 100, "height": 200},
                "isEnabled": "1",
                "isVisible": "1",
                "children": [
                    {
                        "type": "XCUIElementTypeButton",
                        "label": "Wi-Fi",
                        "name": "wifi-row",
                        "rawIdentifier": "wifi-row",
                        "value": "Off",
                        "rect": {"x": 10, "y": 20, "width": 30, "height": 40},
                        "isEnabled": "0",
                        "isVisible": "1",
                        "isFocused": "1"
                    },
                    {
                        "type": "Button",
                        "label": "Hidden",
                        "rect": {"x": 0, "y": 0, "width": 10, "height": 10},
                        "isVisible": "0",
                        "children": [{"type": "Button", "label": "child of hidden", "isVisible": "1"}]
                    },
                    {
                        "type": "StaticText",
                        "name": "Only a name",
                        "rect": {"x": 1.25, "y": 2.5, "width": 3, "height": 1},
                        "isVisible": true,
                        "isEnabled": true
                    },
                    {"type": "Other", "label": "Same", "name": "Same", "isVisible": "1"},
                    {"type": "Switch", "label": "Line\nbreak", "name": "toggle", "value": 1, "isVisible": "1", "isEnabled": "0"}
                ]
            }]
        });
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.source = source;
        });
        let (driver, _) = open(&fake);
        fake.clear();
        let tree = driver.ui_tree().unwrap().expect("a tree");
        assert_eq!(fake.lines(), vec!["GET /source?format=json".to_owned()]);

        assert_eq!(tree.role, "Application");
        assert_eq!(tree.name, "Settings");
        assert_eq!(
            tree.bounds,
            Some(Rect {
                x: 0,
                y: 0,
                width: 200,
                height: 400
            })
        );
        assert!(tree.enabled);
        let window = &tree.children[0];
        assert_eq!(window.role, "Window");
        assert_eq!(window.name, "");
        assert_eq!(
            window.children.len(),
            4,
            "the hidden button and its child are gone"
        );

        let button = &window.children[0];
        assert_eq!(button.role, "Button");
        assert_eq!(button.name, "Wi-Fi");
        assert_eq!(button.id.as_deref(), Some("wifi-row"));
        assert_eq!(button.value.as_deref(), Some("Off"));
        assert_eq!(
            button.bounds,
            Some(Rect {
                x: 20,
                y: 40,
                width: 60,
                height: 80
            })
        );
        assert!(!button.enabled);
        assert!(button.focused);
        assert!(!button.selected);
        assert_eq!(button.bounds.unwrap().center(), Some((50, 80)));

        let text = &window.children[1];
        assert_eq!(text.role, "StaticText");
        assert_eq!(text.name, "Only a name");
        assert_eq!(text.id, None, "the name is already what is shown");
        assert!(text.enabled);
        // 1.25 pt * 2 = 2.5 px rounds to 3; the right edge (4.25 pt) is 8.5 px, which is 9.
        assert_eq!(text.bounds.map(|rect| (rect.x, rect.width)), Some((3, 6)));

        assert_eq!(
            window.children[2].id, None,
            "a name equal to the label is no identifier"
        );

        let switch = &window.children[3];
        assert_eq!(switch.name, "Line break");
        assert_eq!(switch.id.as_deref(), Some("toggle"));
        assert_eq!(switch.value.as_deref(), Some("1"));
        assert!(!switch.enabled);
    }

    #[test]
    fn a_missing_tree_is_an_error_not_no_tree() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.source = Value::Null;
        });
        let (driver, _) = open(&fake);
        let error = driver.ui_tree().unwrap_err();
        assert!(error.message().contains("no UI tree"), "{error:?}");
    }

    #[test]
    fn the_ui_tree_is_cut_at_a_node_count_and_a_depth() {
        let geometry = geometry_2x();
        let wide = json!({
            "type": "Other",
            "children": (0..MAX_TREE_NODES + 100)
                .map(|number| json!({"type": "Button", "label": format!("b{number}")}))
                .collect::<Vec<_>>(),
        });
        let mut budget = MAX_TREE_NODES;
        let tree = convert_node(&wide, &geometry, 0, &mut budget, true).unwrap();
        assert_eq!(tree.children.len(), MAX_TREE_NODES - 1);
        assert_eq!(budget, 0);

        let mut deep = json!({"type": "Other", "label": "leaf"});
        for _ in 0..MAX_TREE_DEPTH + 10 {
            deep = json!({"type": "Other", "children": [deep]});
        }
        let mut budget = MAX_TREE_NODES;
        let mut node = convert_node(&deep, &geometry, 0, &mut budget, true).unwrap();
        let mut levels = 1;
        while let Some(child) = node.children.pop() {
            levels += 1;
            node = child;
        }
        assert_eq!(levels, MAX_TREE_DEPTH + 1);

        // Long labels are cut; control characters do not survive.
        let long =
            json!({"type": "Other", "label": "x".repeat(MAX_TEXT_FIELD * 3), "value": "a\u{7}b"});
        let mut budget = 10;
        let node = convert_node(&long, &geometry, 0, &mut budget, true).unwrap();
        assert_eq!(node.name.chars().count(), MAX_TEXT_FIELD + 1);
        assert!(node.name.ends_with('\u{2026}'));
        assert_eq!(node.value.as_deref(), Some("a b"));
    }

    #[test]
    fn a_node_carries_its_placeholder_password_flag_and_whether_it_was_cut() {
        let geometry = geometry_2x();
        let convert = |json: Value| {
            let mut budget = 10;
            convert_node(&json, &geometry, 0, &mut budget, true).unwrap()
        };

        let field = convert(json!({
            "type": "XCUIElementTypeSecureTextField",
            "label": "Password",
            "placeholderValue": "Required",
            "value": "••••",
        }));
        assert_eq!(field.role, "SecureTextField");
        assert!(field.password);
        assert_eq!(field.hint.as_deref(), Some("Required"));
        assert_eq!(field.value.as_deref(), Some("••••"));
        assert!(!field.truncated);
        assert_eq!(field.package, None);
        assert!(!field.clickable);
        assert_eq!(field.text, None);

        let plain = convert(json!({"type": "XCUIElementTypeTextField", "value": "hi"}));
        assert!(!plain.password);
        assert_eq!(plain.hint, None);
        assert!(!plain.truncated);

        let long_value = "v".repeat(MAX_TEXT_FIELD + 50);
        let long = convert(json!({
            "type": "XCUIElementTypeTextField",
            "label": "Notes",
            "value": long_value,
        }));
        assert!(long.truncated);
        assert_eq!(long.name, "Notes");
        let value = long.value.as_deref().unwrap();
        assert_eq!(value.chars().count(), MAX_TEXT_FIELD + 1);
        assert!(value.ends_with('\u{2026}'));

        // A value exactly at the cap is whole, and so is not flagged.
        let exact = convert(
            json!({"type": "XCUIElementTypeTextField", "value": "v".repeat(MAX_TEXT_FIELD)}),
        );
        assert!(!exact.truncated);
        assert_eq!(exact.value.unwrap().chars().count(), MAX_TEXT_FIELD);

        let long_hint = convert(json!({
            "type": "XCUIElementTypeTextField",
            "placeholderValue": "h".repeat(MAX_TEXT_FIELD * 2),
        }));
        assert!(long_hint.truncated);
        assert_eq!(long_hint.hint.unwrap().chars().count(), MAX_TEXT_FIELD + 1);

        let long_name = convert(
            json!({"type": "XCUIElementTypeButton", "label": "n".repeat(MAX_TEXT_FIELD + 1)}),
        );
        assert!(long_name.truncated);
    }

    /// The parser trims texts and turns control characters into spaces, so what a node says is
    /// not always what the device holds: such a node is flagged inexact (and a text cut short
    /// is inexact too), while a text it left as it was is not.
    #[test]
    fn a_node_is_inexact_when_cleaning_changed_a_text_it_carries() {
        let geometry = geometry_2x();
        let convert = |json: Value| {
            let mut budget = 10;
            convert_node(&json, &geometry, 0, &mut budget, true).unwrap()
        };
        let field = |extra: Value| {
            let mut node = json!({"type": "XCUIElementTypeTextField", "label": "Name"});
            node.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            convert(node)
        };

        // Nothing to clean: the device's own text, spaces inside it included.
        let exact = field(json!({"value": "John Smith", "placeholderValue": "Your name"}));
        assert!(!exact.inexact);
        assert_eq!(exact.value.as_deref(), Some("John Smith"));
        assert!(!field(json!({"value": 42})).inexact);
        assert!(!field(json!({})).inexact);

        // Trailing and leading spaces are trimmed: "John " reads as "John".
        let trailing = field(json!({"value": "John "}));
        assert_eq!(trailing.value.as_deref(), Some("John"));
        assert!(trailing.inexact && !trailing.truncated);
        assert!(field(json!({"value": " John"})).inexact);

        // Tabs and line breaks become spaces.
        let tabs = field(json!({"value": "a\tb\nc"}));
        assert_eq!(tabs.value.as_deref(), Some("a b c"));
        assert!(tabs.inexact);

        // The label and the placeholder count as well as the value.
        assert!(field(json!({"label": "Na\u{7}me"})).inexact);
        assert!(field(json!({"placeholderValue": "Your name\n"})).inexact);

        // A text of nothing but spaces and control characters is no value in the tree, but it
        // was not the device's empty text either.
        let blank = field(json!({"value": " \t "}));
        assert_eq!(blank.value, None);
        assert!(blank.inexact);

        // A text cut short is inexact; so every truncated node is.
        let long = field(json!({"value": "v".repeat(MAX_TEXT_FIELD + 1)}));
        assert!(long.truncated && long.inexact);
        let long_name = convert(
            json!({"type": "XCUIElementTypeButton", "label": "n".repeat(MAX_TEXT_FIELD + 1)}),
        );
        assert!(long_name.truncated && long_name.inexact);
    }

    /// A tree read bounded by a timeout gives up when the device is slower than that, long
    /// before WebDriverAgent's own limit, and leaves the connection as it was.
    #[test]
    fn a_tree_read_within_a_timeout_fails_when_the_device_is_slower() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.source = json!({"type": "XCUIElementTypeApplication", "name": "Settings"});
            state
                .delays
                .push(("/source?format=json".to_owned(), Duration::from_millis(1500)));
        });
        let (driver, log) = open(&fake);
        let started = Instant::now();
        let error = driver
            .ui_tree_within(Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );
        assert!(!logs(&log).iter().any(|line| line.contains("lost")));

        // The same read with the time it needs, and the unbounded one, both go through.
        fake.set(|state| state.delays.clear());
        let tree = driver
            .ui_tree_within(Duration::from_secs(5))
            .unwrap()
            .expect("a tree");
        assert_eq!(tree.name, "Settings");
        assert!(driver.ui_tree().unwrap().is_some());
    }

    #[test]
    fn labels_come_from_the_status_when_it_says_anything() {
        let object = |json: Value| match json {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        assert_eq!(
            label_from_status(&object(json!({"ready": true}))),
            "iOS device"
        );
        assert_eq!(
            label_from_status(&object(
                json!({"device": "ipad", "os": {"name": "iPadOS", "version": "18.1"}})
            )),
            "iPad (iPadOS 18.1)"
        );
        assert_eq!(
            label_from_status(&object(json!({"device": "iphone"}))),
            "iPhone"
        );
        assert_eq!(
            label_from_status(&object(json!({"os": {"name": "iOS"}}))),
            "iOS device"
        );
        assert_eq!(
            label_from_status(&object(json!({"device": "Vision\nPro"}))),
            "Vision Pro"
        );
    }

    #[test]
    fn base64_and_png_problems_are_failures_not_panics() {
        assert!(decode_base64("not base64 !!!").is_err());
        // Wrapped lines and missing padding are tolerated.
        let wrapped = STANDARD.encode(b"hello world").replace("bG8g", "bG8g\n");
        assert_eq!(decode_base64(&wrapped).unwrap(), b"hello world");
        assert_eq!(decode_base64("aGk").unwrap(), b"hi");

        assert!(decode_png(b"definitely not a png").is_err());
        let mut truncated = png_bytes(20, 20);
        truncated.truncate(truncated.len() / 2);
        assert!(decode_png(&truncated).is_err());
        let (width, height, rgba) = decode_png(&png_bytes(3, 2)).unwrap();
        assert_eq!((width, height, rgba.len()), (3, 2, 24));
        assert!(rgba.chunks(4).all(|pixel| pixel[3] == 255));

        // Grey is spread over the colour channels.
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 2, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[10, 200]).unwrap();
            writer.finish().unwrap();
        }
        let (_, _, rgba) = decode_png(&out).unwrap();
        assert_eq!(rgba, vec![10, 10, 10, 255, 200, 200, 200, 255]);
    }

    #[test]
    fn a_screenshot_reply_without_an_image_is_a_failure() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.set(|state| state.screenshot = String::new());
        let error = driver.frame(true).unwrap_err();
        assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    }

    #[test]
    fn session_ids_are_checked_for_shape() {
        assert!(valid_session_id("0F3A9C1E-77B2-4F0B-9E57-1A2B3C4D5E6F"));
        assert!(valid_session_id("abc123"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("a/b"));
        assert!(!valid_session_id("a b"));
        assert!(!valid_session_id("a?x=1"));
        assert!(!valid_session_id(&"a".repeat(65)));
        assert!(valid_session_id(&"a".repeat(64)));
    }

    // ---- the calls behind the iOS tools ----

    const OTHER: &str = "BBBBBBBB-0000-4000-8000-000000000002";
    const FIVE_SECONDS: Duration = Duration::from_secs(5);

    fn control(driver: &Arc<dyn Driver>) -> &dyn IosControl {
        driver.ios().expect("WebDriverAgent has the iOS calls")
    }

    #[test]
    fn only_webdriveragent_has_the_ios_calls_and_they_reach_session_and_root_routes() {
        assert!(crate::remote_control::fake::FakeDriver::touch()
            .ios()
            .is_none());
        let fake = FakeWda::start(|state| {
            with_session(state);
            state
                .values
                .insert("GET /wda/locked".to_owned(), json!(true));
            state
                .values
                .insert("GET /orientation".to_owned(), json!("PORTRAIT"));
        });
        let (driver, _) = open(&fake);
        let control = control(&driver);
        fake.clear();

        let locked = control
            .call("GET", WdaRoute::Root("/wda/locked"), None, FIVE_SECONDS)
            .unwrap();
        assert_eq!(locked, json!(true));
        let orientation = control
            .call("GET", WdaRoute::Session("/orientation"), None, FIVE_SECONDS)
            .unwrap();
        assert_eq!(orientation, json!("PORTRAIT"));
        let launch = json!({"bundleId": "com.apple.Preferences", "arguments": ["-x"]});
        let launched = control
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/launch"),
                Some(&launch),
                FIVE_SECONDS,
            )
            .unwrap();
        assert_eq!(launched, Value::Null);
        control
            .call(
                "DELETE",
                WdaRoute::Session("/wda/simulatedLocation"),
                None,
                FIVE_SECONDS,
            )
            .unwrap();
        control
            .call(
                "POST",
                WdaRoute::Root("/wda/homescreen"),
                None,
                FIVE_SECONDS,
            )
            .unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                "GET /wda/locked".to_owned(),
                format!("GET /session/{EXISTING}/orientation"),
                format!("POST /session/{EXISTING}/wda/apps/launch"),
                format!("DELETE /session/{EXISTING}/wda/simulatedLocation"),
                "POST /wda/homescreen".to_owned(),
            ]
        );
        assert_eq!(fake.bodies("/wda/apps/launch"), vec![launch]);
    }

    #[test]
    fn a_lost_session_is_replaced_through_call_as_it_is_for_input() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        let control = control(&driver);
        let activate = json!({"bundleId": "com.apple.mobilesafari"});
        let call = || {
            control.call(
                "POST",
                WdaRoute::Session("/wda/apps/activate"),
                Some(&activate),
                FIVE_SECONDS,
            )
        };

        // Another client made its own session: it is followed, and none is created.
        fake.set(|state| state.session = Some(OTHER.to_owned()));
        fake.clear();
        call().unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/apps/activate"),
                "GET /status".to_owned(),
                format!("POST /session/{OTHER}/wda/apps/activate"),
            ]
        );
        assert_eq!(lock(&fake.state).created, 0);
        assert!(logs(&log).iter().any(|line| line.contains("following")));

        // None exists any more: one is made, and it is ours to delete.
        fake.set(|state| state.session = None);
        fake.clear();
        call().unwrap();
        let created = lock(&fake.state).session.clone().unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{OTHER}/wda/apps/activate"),
                "GET /status".to_owned(),
                "POST /session".to_owned(),
                format!("POST /session/{created}/wda/apps/activate"),
            ]
        );
        assert_eq!(lock(&fake.state).created, 1);
        driver.close();
        assert_eq!(fake.deletes(), vec![format!("DELETE /session/{created}")]);
    }

    #[test]
    fn a_session_that_stays_invalid_is_an_error_through_call_too() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.set(|state| {
            state.failing.insert(
                "/wda/apps/activate".to_owned(),
                (404, "invalid session id", "Session does not exist"),
            );
        });
        let error = control(&driver)
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/activate"),
                Some(&json!({"bundleId": "a.b"})),
                FIVE_SECONDS,
            )
            .unwrap_err();
        assert!(error.message().contains("keeps reporting"), "{error:?}");
        // WebDriverAgent still named the failed session as the active one, so the driver made its
        // one replacement, once, and then gave up.
        assert_eq!(lock(&fake.state).created, 1, "{:?}", fake.lines());
    }

    #[test]
    fn a_route_the_build_lacks_is_unsupported_and_its_own_error_is_a_failure() {
        let fake = FakeWda::start(|state| {
            with_session(state);
            state.missing.insert("/wda/apps/state".to_owned());
            state.failing.insert(
                "/wda/apps/launch".to_owned(),
                (500, "unknown error", "Failed to launch\nnoise"),
            );
            state.root_failing.insert(
                "/wda/unlock".to_owned(),
                (500, "unknown error", "Unlock timed out"),
            );
        });
        let (driver, _) = open(&fake);
        let control = control(&driver);

        // A session route answered "unknown command".
        let error = control
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/state"),
                Some(&json!({"bundleId": "a.b"})),
                FIVE_SECONDS,
            )
            .unwrap_err();
        assert!(
            matches!(&error, DriverError::Unsupported(message) if message.contains("/wda/apps/state")),
            "{error:?}"
        );
        // A root route the server does not have is the HTTP server's own 404 page.
        let error = control
            .call("POST", WdaRoute::Root("/wda/lock"), None, FIVE_SECONDS)
            .unwrap_err();
        assert!(
            matches!(&error, DriverError::Unsupported(message) if message.contains("/wda/lock")),
            "{error:?}"
        );

        // Its own errors keep their message, on one line.
        let error = control
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/launch"),
                Some(&json!({"bundleId": "a.b"})),
                FIVE_SECONDS,
            )
            .unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(error.message().contains("HTTP 500"), "{error:?}");
        assert!(error.message().contains("Failed to launch"), "{error:?}");
        assert!(!error.message().contains("noise"), "{error:?}");
        let error = control
            .call("POST", WdaRoute::Root("/wda/unlock"), None, FIVE_SECONDS)
            .unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(error.message().contains("Unlock timed out"), "{error:?}");
        // Neither started a session or touched the one there is.
        assert_eq!(lock(&fake.state).created, 0);
        assert_eq!(lock(&fake.state).session.as_deref(), Some(EXISTING));
    }

    #[test]
    fn a_call_that_outlives_its_timeout_fails_and_the_connection_is_not_lost() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open(&fake);
        let control = control(&driver);
        fake.set(|state| {
            state
                .delays
                .push(("/wda/apps/launch".to_owned(), Duration::from_millis(1500)));
        });
        fake.clear();

        let started = Instant::now();
        let error = control
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/launch"),
                Some(&json!({"bundleId": "a.b"})),
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(
            error.message().contains("did not answer within 0.3 s"),
            "{error:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1000),
            "{:?}",
            started.elapsed()
        );

        // The session was not replaced, nothing was created, and the driver still answers.
        control
            .call("GET", WdaRoute::Session("/orientation"), None, FIVE_SECONDS)
            .unwrap();
        assert_eq!(lock(&fake.state).created, 0);
        assert_eq!(lock(&fake.state).session.as_deref(), Some(EXISTING));
        let lines = fake.lines();
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("/status") && line != "POST /session"),
            "{lines:?}"
        );
        assert!(!logs(&log).iter().any(|line| line.contains("lost")));
        driver.frame(true).unwrap();
        driver.tap(100, 100, 1).unwrap();
    }

    #[test]
    fn replacing_a_lost_session_takes_only_the_time_the_call_has() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        fake.set(|state| {
            state.session = None;
            state
                .delays
                .push(("/status".to_owned(), Duration::from_millis(1800)));
        });
        let started = Instant::now();
        let error = control(&driver)
            .call(
                "POST",
                WdaRoute::Session("/wda/apps/activate"),
                Some(&json!({"bundleId": "a.b"})),
                Duration::from_millis(300),
            )
            .unwrap_err();
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1400),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_call_waits_for_another_callers_replacement_only_as_long_as_it_has() {
        let fake = FakeWda::start(with_session);
        let (driver, log) = open_with_recheck(&fake, WINDOW_RECHECK);
        let activate = json!({"bundleId": "a.b"});
        fake.set(|state| state.session = None);
        fake.clear();

        // Another caller (the viewer's capture, a tap) is replacing the lost session, which
        // can take 70 seconds; this call's time is under half a second. The test lets go of the
        // replacement after three seconds whatever happens, so a call that waits for it fails
        // the test instead of hanging it.
        let (finished_in_time, waited, outcome) = thread::scope(|scope| {
            let replacing = lock(&driver.replacing);
            let started = Instant::now();
            let calling = scope.spawn(|| {
                IosControl::call(
                    &driver,
                    "POST",
                    WdaRoute::Session("/wda/apps/activate"),
                    Some(&activate),
                    Duration::from_millis(400),
                )
            });
            while !calling.is_finished() && started.elapsed() < Duration::from_secs(3) {
                thread::sleep(Duration::from_millis(10));
            }
            let finished_in_time = calling.is_finished();
            let waited = started.elapsed();
            drop(replacing);
            (finished_in_time, waited, calling.join().unwrap())
        });
        assert!(
            finished_in_time,
            "the call waited for another caller's replacement past its own time ({waited:?})"
        );
        let error = outcome.unwrap_err();

        // A failure of the call, not a lost connection and not a "took too long to answer".
        assert!(matches!(&error, DriverError::Failed(_)), "{error:?}");
        assert!(
            error
                .message()
                .contains("Another call is re-establishing the WebDriverAgent session; try again"),
            "{error:?}"
        );
        assert!(!error.message().contains("did not answer"), "{error:?}");
        assert!(waited >= Duration::from_millis(350), "{waited:?}");
        assert!(waited < Duration::from_millis(1500), "{waited:?}");
        // Nothing but the request that found the session gone was sent, and no session made.
        assert_eq!(
            fake.lines(),
            vec![format!("POST /session/{EXISTING}/wda/apps/activate")]
        );
        assert_eq!(lock(&fake.state).created, 0);
        assert!(!logs(&log).iter().any(|line| line.contains("lost")));

        // The right to replace was only waited for, never lost: the next call does it.
        IosControl::call(
            &driver,
            "POST",
            WdaRoute::Session("/wda/apps/activate"),
            Some(&activate),
            FIVE_SECONDS,
        )
        .unwrap();
        assert_eq!(lock(&fake.state).created, 1);
    }

    #[test]
    fn a_call_that_waited_for_a_replacement_uses_the_session_it_made() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open_with_recheck(&fake, WINDOW_RECHECK);
        let activate = json!({"bundleId": "a.b"});
        fake.set(|state| state.session = Some(OTHER.to_owned()));
        fake.clear();

        let result = thread::scope(|scope| {
            // The other caller follows the session another client made, a moment from now.
            let replacing = lock(&driver.replacing);
            let waiting = scope.spawn(|| {
                IosControl::call(
                    &driver,
                    "POST",
                    WdaRoute::Session("/wda/apps/activate"),
                    Some(&activate),
                    FIVE_SECONDS,
                )
            });
            thread::sleep(Duration::from_millis(250));
            *lock(&driver.session) = Session {
                id: OTHER.to_owned(),
                owned: false,
            };
            drop(replacing);
            waiting.join().unwrap()
        });
        result.unwrap();

        // It asked once with the lost session and once with the new one, and replaced nothing.
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/apps/activate"),
                format!("POST /session/{OTHER}/wda/apps/activate"),
            ]
        );
        assert_eq!(lock(&fake.state).created, 0);
    }

    #[test]
    fn a_driver_request_still_waits_out_a_replacement() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open_with_recheck(&fake, WINDOW_RECHECK);
        fake.set(|state| state.session = Some(OTHER.to_owned()));
        fake.clear();

        let result = thread::scope(|scope| {
            let replacing = lock(&driver.replacing);
            let waiting = scope.spawn(|| driver.tap(100, 100, 1));
            // Longer than any call of an iOS tool would wait.
            thread::sleep(Duration::from_millis(600));
            *lock(&driver.session) = Session {
                id: OTHER.to_owned(),
                owned: false,
            };
            drop(replacing);
            waiting.join().unwrap()
        });
        result.unwrap();
        assert_eq!(
            fake.lines(),
            vec![
                format!("POST /session/{EXISTING}/wda/tap"),
                format!("POST /session/{OTHER}/wda/tap"),
            ]
        );
    }

    #[test]
    fn a_server_that_is_gone_is_still_a_disconnection() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        drop(fake);
        let error = control(&driver)
            .call("GET", WdaRoute::Root("/wda/locked"), None, FIVE_SECONDS)
            .unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
        let error = control(&driver)
            .call("GET", WdaRoute::Session("/orientation"), None, FIVE_SECONDS)
            .unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");

        // And a closed driver says so without asking anyone.
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        driver.close();
        fake.clear();
        let error = control(&driver)
            .call("GET", WdaRoute::Root("/wda/locked"), None, FIVE_SECONDS)
            .unwrap_err();
        assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn only_fixed_routes_and_plain_methods_are_called() {
        let fake = FakeWda::start(with_session);
        let (driver, _) = open(&fake);
        let control = control(&driver);
        fake.clear();
        let too_long = format!("/{}", "a".repeat(MAX_ROUTE_BYTES));
        for route in [
            WdaRoute::Root("/session"),
            WdaRoute::Root("/session/AAAA"),
            WdaRoute::Root("/session/AAAA/wda/apps/launch"),
            WdaRoute::Root("/wda/shutdown"),
            WdaRoute::Session("/wda/shutdown"),
            WdaRoute::Root("wda/locked"),
            WdaRoute::Root(""),
            WdaRoute::Root("/"),
            WdaRoute::Root("/../wda/shutdown"),
            WdaRoute::Root("/wda/../x"),
            WdaRoute::Root("/wda//locked"),
            WdaRoute::Root("/wda/locked?x=1"),
            WdaRoute::Root("/wda/locked#x"),
            WdaRoute::Root("/wda/locked "),
            WdaRoute::Root("/wda/locked\r\nHost: evil"),
            WdaRoute::Root("/%2e%2e/x"),
            WdaRoute::Root("/source?format=xml"),
            WdaRoute::Root(&too_long),
            WdaRoute::Session(""),
            WdaRoute::Session("orientation"),
            WdaRoute::Session("/a b"),
            WdaRoute::Session("/orientation?x=1"),
            WdaRoute::Session("/\u{e9}"),
        ] {
            let error = control.call("GET", route, None, FIVE_SECONDS).unwrap_err();
            assert!(
                matches!(&error, DriverError::Failed(message) if message.contains("refusing")),
                "{route:?}: {error:?}"
            );
        }
        for method in ["PUT", "get", "", "GET /x", "POST\r\n"] {
            let error = control
                .call(method, WdaRoute::Root("/wda/locked"), None, FIVE_SECONDS)
                .unwrap_err();
            assert!(
                matches!(&error, DriverError::Failed(_)),
                "{method:?}: {error:?}"
            );
        }
        assert!(fake.requests().is_empty(), "{:?}", fake.lines());
        assert_eq!(lock(&fake.state).created, 0);

        // The tree's query is the one fixed query.
        fake.set(|state| state.source = json!({"type": "Application"}));
        let tree = control
            .call("GET", WdaRoute::Root(SOURCE_ROUTE), None, FIVE_SECONDS)
            .unwrap();
        assert_eq!(tree, json!({"type": "Application"}));
        assert_eq!(fake.lines(), vec![format!("GET {SOURCE_ROUTE}")]);
    }

    /// The iOS tools find a field with `/elements` and clear it with `/element/<id>/clear`; those
    /// are the only element routes, and the id in the second is held to the strictness of a
    /// session id.
    #[test]
    fn the_element_routes_are_the_two_the_ios_tools_call() {
        let uuid = "5F1B0A2C-3D4E-4F50-8A61-7B8C9D0E1F2A";
        for outside_a_session in [false, true] {
            for route in [
                "/elements".to_owned(),
                format!("/element/{uuid}/clear"),
                "/element/abc123/clear".to_owned(),
                format!("/element/{}/clear", "a".repeat(64)),
            ] {
                assert!(
                    check_route(&route, outside_a_session).is_ok(),
                    "{route} outside a session: {outside_a_session}"
                );
            }
            for route in [
                "/element".to_owned(),
                "/element/".to_owned(),
                "/element/clear".to_owned(),
                "/element//clear".to_owned(),
                "/element/a b/clear".to_owned(),
                "/element/a.b/clear".to_owned(),
                "/element/a_b/clear".to_owned(),
                "/element/../clear".to_owned(),
                "/element/%2e%2e/clear".to_owned(),
                "/element/a?b/clear".to_owned(),
                format!("/element/{uuid}/clear/more"),
                format!("/element/{uuid}/click"),
                format!("/element/{uuid}/value"),
                format!("/element/{uuid}"),
                format!("/element/{}/clear", "a".repeat(65)),
                "/elementary".to_owned(),
                "/elements/x".to_owned(),
                "/elements?using=x".to_owned(),
            ] {
                assert!(
                    matches!(
                        check_route(&route, outside_a_session),
                        Err(DriverError::Failed(message)) if message.contains("refusing")
                    ),
                    "{route} outside a session: {outside_a_session}"
                );
            }
        }
        assert!(valid_element_id(uuid));
        for bad in ["", "a b", "a/b", "a.b", "\u{e9}"] {
            assert!(!valid_element_id(bad), "{bad:?}");
        }
        assert!(!valid_element_id(&"a".repeat(65)));
    }

    #[test]
    fn a_timeout_is_told_from_a_gone_server_by_the_clock_and_the_message() {
        let timeout = Duration::from_secs(10);
        let early = Duration::from_millis(40);
        // The socket's own words.
        assert!(is_timeout(
            "127.0.0.1:8100 on this computer did not answer in time",
            early,
            timeout
        ));
        // The relay closes the connection when its deadline passes.
        assert!(is_timeout(
            "the connection to 127.0.0.1:8100 on mac was closed",
            timeout,
            timeout
        ));
        assert!(is_timeout(
            "the connection to 127.0.0.1:8100 on mac was closed",
            timeout - Duration::from_millis(100),
            timeout
        ));
        // Gone at once, or never connected: not a timeout.
        assert!(!is_timeout(
            "the connection to 127.0.0.1:8100 on mac was closed",
            early,
            timeout
        ));
        assert!(!is_timeout(
            "nothing is listening on 127.0.0.1:8100 on this computer",
            early,
            timeout
        ));
        assert!(!is_timeout(
            "127.0.0.1:8100 on this computer did not answer",
            timeout,
            timeout
        ));
        assert_eq!(seconds_text(Duration::from_secs(7)), "7 s");
        assert_eq!(seconds_text(Duration::from_millis(300)), "0.3 s");
        assert_eq!(seconds_text(Duration::from_millis(1500)), "1.5 s");
        assert!(too_slow(Duration::from_secs(20)).contains("within 20 s"));
    }
}
