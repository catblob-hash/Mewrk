//! Test stand-ins for the layers around a session: a driver that answers
//! without a device, driver factories that connect, refuse or wait, and a
//! registered machine that is never dialled.
//!
//! The session module keeps its own, richer fakes for its own tests; these are
//! for the registry, `preview::start`, the tool layer and the per-platform
//! tools, which need a session that reaches a phase and, for the platform
//! tools, a protocol's extension to script: [`FakeAndroid`] (a shell answered
//! by a closure, installs recorded), [`FakeIos`] (WebDriverAgent calls
//! answered by a closure) and [`FakeDesktop`] (a clipboard to set and read).

use std::{
    collections::BTreeMap,
    io::Read,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{
    driver::{
        AndroidControl, ClipboardChannel, ClipboardText, DesktopControl, Driver, DriverError,
        DriverResult, Frame, IosControl, Platform, ShellResult, UiNode, WdaRoute,
    },
    keys::Key,
    place::ControlMachine,
    session::ConnectFn,
};
use crate::{preview_remote::RemoteMachine, run_environment::ShellRunner};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A screen that accepts every input of its kind, and remembers it. Its picture
/// is one flat colour (or noise) with whatever a test painted over it, at a
/// size a test can change, as a turned phone or a resized desktop would.
pub(crate) struct FakeDriver {
    platform: Platform,
    size: Mutex<(u32, u32)>,
    seq: AtomicU64,
    closes: AtomicUsize,
    /// The element tree it answers `ui_tree` with; none (as RFB) when `None`.
    tree: Option<UiNode>,
    /// Every pixel its own colour rather than one flat colour: a picture no JPEG
    /// compresses well.
    noisy: bool,
    /// Rectangles painted over the picture, in screen pixels, in order.
    marks: Mutex<Vec<((u32, u32, u32, u32), [u8; 3])>>,
    /// The input it received, one line per call: `tap 540,1200 x1`.
    inputs: Mutex<Vec<String>>,
    /// Every [`Driver::frame`] call, as the `fresh` it asked for: a real phone's driver captures
    /// anew only for a fresh frame, so what a look asks for matters.
    frame_requests: Mutex<Vec<bool>>,
    /// How long a read of the element tree takes, as a real device's does: a read bounded by less
    /// ([`Driver::ui_tree_within`]) fails, as one that outlasts its timeout does.
    tree_read_takes: Mutex<Duration>,
    /// The timeouts the bounded reads of the element tree were given.
    tree_timeouts: Mutex<Vec<Duration>>,
    android: Option<FakeAndroid>,
    ios: Option<FakeIos>,
    desktop: Option<FakeDesktop>,
}

impl FakeDriver {
    pub(crate) fn new(platform: Platform, width: u32, height: u32) -> Arc<Self> {
        Arc::new(Self::plain(platform, width, height))
    }

    fn plain(platform: Platform, width: u32, height: u32) -> Self {
        Self {
            platform,
            size: Mutex::new((width, height)),
            seq: AtomicU64::new(0),
            closes: AtomicUsize::new(0),
            tree: None,
            noisy: false,
            marks: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            frame_requests: Mutex::new(Vec::new()),
            tree_read_takes: Mutex::new(Duration::ZERO),
            tree_timeouts: Mutex::new(Vec::new()),
            android: None,
            ios: None,
            desktop: None,
        }
    }

    /// An Android phone, 1080x2400.
    pub(crate) fn touch() -> Arc<Self> {
        Self::new(Platform::Android, 1080, 2400)
    }

    /// An Android phone, 1080x2400, whose element tree is `tree`.
    pub(crate) fn touch_with_tree(tree: UiNode) -> Arc<Self> {
        Arc::new(Self {
            tree: Some(tree),
            ..Self::plain(Platform::Android, 1080, 2400)
        })
    }

    /// A screen whose every pixel differs from its neighbours.
    pub(crate) fn noisy(platform: Platform, width: u32, height: u32) -> Arc<Self> {
        Arc::new(Self {
            noisy: true,
            ..Self::plain(platform, width, height)
        })
    }

    /// An Android phone, 1080x2400, whose adb operations are `android`'s.
    pub(crate) fn with_android(android: FakeAndroid) -> Arc<Self> {
        Arc::new(Self {
            android: Some(android),
            ..Self::plain(Platform::Android, 1080, 2400)
        })
    }

    /// An Android phone, 1080x2400, whose adb operations are `android`'s and whose element tree
    /// is `tree`: a device that can be both asked about its shell and looked at (a dialog's
    /// buttons are read off the tree).
    pub(crate) fn with_android_tree(android: FakeAndroid, tree: UiNode) -> Arc<Self> {
        Arc::new(Self {
            android: Some(android),
            tree: Some(tree),
            ..Self::plain(Platform::Android, 1080, 2400)
        })
    }

    /// An iPhone, 1170x2532 pixels, whose WebDriverAgent calls are `ios`'s.
    pub(crate) fn with_ios(ios: FakeIos) -> Arc<Self> {
        Arc::new(Self {
            ios: Some(ios),
            ..Self::plain(Platform::Ios, 1170, 2532)
        })
    }

    /// A desktop, 1920x1080, whose RFB extras are `desktop`'s.
    pub(crate) fn with_desktop(desktop: FakeDesktop) -> Arc<Self> {
        Arc::new(Self {
            desktop: Some(desktop),
            ..Self::plain(Platform::Desktop, 1920, 1080)
        })
    }

    /// The scripted adb operations; panics on a driver made without them.
    pub(crate) fn android_fake(&self) -> &FakeAndroid {
        self.android.as_ref().expect("a driver made with_android")
    }

    /// The scripted WebDriverAgent; panics on a driver made without it.
    pub(crate) fn ios_fake(&self) -> &FakeIos {
        self.ios.as_ref().expect("a driver made with_ios")
    }

    /// The scripted RFB extras; panics on a driver made without them.
    pub(crate) fn desktop_fake(&self) -> &FakeDesktop {
        self.desktop.as_ref().expect("a driver made with_desktop")
    }

    /// How many times the session closed it.
    pub(crate) fn closes(&self) -> usize {
        self.closes.load(Ordering::SeqCst)
    }

    /// The input it has received so far.
    pub(crate) fn inputs(&self) -> Vec<String> {
        lock(&self.inputs).clone()
    }

    /// Every frame asked for so far, in order: `true` for a fresh one.
    pub(crate) fn frame_requests(&self) -> Vec<bool> {
        lock(&self.frame_requests).clone()
    }

    /// From now on a read of the element tree takes `time`: a bounded read ([`Driver::ui_tree_within`])
    /// given less fails at once, as a device that outlasts the timeout does.
    pub(crate) fn tree_read_takes(&self, time: Duration) {
        *lock(&self.tree_read_takes) = time;
    }

    /// The timeouts the bounded reads of the element tree have been given so far.
    pub(crate) fn tree_timeouts(&self) -> Vec<Duration> {
        lock(&self.tree_timeouts).clone()
    }

    /// The screen is now `width × height`; what was painted on it stays.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        *lock(&self.size) = (width, height);
    }

    /// Paints `rgb` over the screen pixels from `(x, y)`, `width × height` of
    /// them (cut at the screen's edge). Later paint covers earlier.
    pub(crate) fn paint(&self, (x, y): (u32, u32), (width, height): (u32, u32), rgb: [u8; 3]) {
        lock(&self.marks).push(((x, y, width, height), rgb));
    }

    fn record(&self, line: String) {
        lock(&self.inputs).push(line);
    }

    fn pixels(&self, width: u32, height: u32) -> Vec<u8> {
        let count = (width * height) as usize;
        let mut pixels: Vec<u8> = if !self.noisy {
            [30u8, 60, 90, 255]
                .iter()
                .copied()
                .cycle()
                .take(count * 4)
                .collect()
        } else {
            (0..count as u32)
                .flat_map(|index| {
                    let mut hash = index.wrapping_mul(0x9e37_79b9);
                    hash ^= hash >> 15;
                    hash = hash.wrapping_mul(0x2c1b_3c6d);
                    hash ^= hash >> 12;
                    hash = hash.wrapping_mul(0x297a_2d39);
                    hash ^= hash >> 15;
                    let [r, g, b, _] = hash.to_le_bytes();
                    [r, g, b, 255]
                })
                .collect()
        };
        for &((x, y, mark_width, mark_height), [r, g, b]) in lock(&self.marks).iter() {
            let right = x.saturating_add(mark_width).min(width);
            let bottom = y.saturating_add(mark_height).min(height);
            for row in y.min(height)..bottom {
                for column in x.min(width)..right {
                    let at = ((row * width + column) * 4) as usize;
                    pixels[at..at + 4].copy_from_slice(&[r, g, b, 255]);
                }
            }
        }
        pixels
    }
}

impl Driver for FakeDriver {
    fn info(&self) -> super::driver::DriverInfo {
        let (width, height) = *lock(&self.size);
        super::driver::DriverInfo {
            platform: self.platform,
            label: "Fake Pixel".into(),
            width,
            height,
            has_ui_tree: self.tree.is_some(),
        }
    }

    fn frame(&self, fresh: bool) -> DriverResult<Frame> {
        lock(&self.frame_requests).push(fresh);
        let (width, height) = *lock(&self.size);
        Ok(Frame {
            width,
            height,
            rgba: Arc::from(self.pixels(width, height)),
            seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
            captured_at: Instant::now(),
        })
    }

    fn pointer(&self, x: u32, y: u32, buttons: u8) -> DriverResult<()> {
        if self.platform.is_touch() {
            return Err(DriverError::Unsupported("a touch screen".into()));
        }
        self.record(format!("pointer {x},{y} buttons {buttons}"));
        Ok(())
    }

    fn tap(&self, x: u32, y: u32, count: u8) -> DriverResult<()> {
        if !self.platform.is_touch() {
            return Err(DriverError::Unsupported("a pointer screen".into()));
        }
        self.record(format!("tap {x},{y} x{count}"));
        Ok(())
    }

    fn long_press(&self, x: u32, y: u32, ms: u32) -> DriverResult<()> {
        if !self.platform.is_touch() {
            return Err(DriverError::Unsupported("a pointer screen".into()));
        }
        self.record(format!("long_press {x},{y} {ms}ms"));
        Ok(())
    }

    fn swipe(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> DriverResult<()> {
        if !self.platform.is_touch() {
            return Err(DriverError::Unsupported("a pointer screen".into()));
        }
        self.record(format!(
            "swipe {},{} -> {},{} {ms}ms",
            from.0, from.1, to.0, to.1
        ));
        Ok(())
    }

    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
        self.record(format!("key {key:?} {down:?}"));
        Ok(())
    }

    fn text(&self, text: &str) -> DriverResult<()> {
        self.record(format!("text {text}"));
        Ok(())
    }

    fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
        Ok(self.tree.clone())
    }

    fn ui_tree_within(&self, timeout: Duration) -> DriverResult<Option<UiNode>> {
        lock(&self.tree_timeouts).push(timeout);
        if *lock(&self.tree_read_takes) > timeout {
            return Err(DriverError::Failed(
                "uiautomator did not finish reading the screen in the time this call had left."
                    .into(),
            ));
        }
        Ok(self.tree.clone())
    }

    fn close(&self) {
        self.closes.fetch_add(1, Ordering::SeqCst);
    }

    fn android(&self) -> Option<&dyn AndroidControl> {
        self.android
            .as_ref()
            .map(|android| android as &dyn AndroidControl)
    }

    fn ios(&self) -> Option<&dyn IosControl> {
        self.ios.as_ref().map(|ios| ios as &dyn IosControl)
    }

    fn desktop(&self) -> Option<&dyn DesktopControl> {
        self.desktop
            .as_ref()
            .map(|desktop| desktop as &dyn DesktopControl)
    }
}

/// One [`AndroidControl::shell`] call as the fake received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellCall {
    pub command: String,
    pub limit: usize,
    pub timeout: Duration,
}

/// One [`AndroidControl::install`] as the fake received it: every byte it read
/// off the stream, the size it was told, and the flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InstallCall {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub flags: Vec<String>,
    pub timeout: Duration,
}

type ShellScript = dyn Fn(&str) -> DriverResult<ShellResult> + Send + Sync;
type InstallScript = dyn Fn(&InstallCall) -> DriverResult<String> + Send + Sync;

/// An adb device's shell and package installer, answered by closures.
pub(crate) struct FakeAndroid {
    sdk: u32,
    shell: Box<ShellScript>,
    install: Box<InstallScript>,
    shells: Mutex<Vec<ShellCall>>,
    installs: Mutex<Vec<InstallCall>>,
}

impl FakeAndroid {
    /// A device on API level `sdk` whose shell answers each command with
    /// `shell(command)`, and whose installs succeed.
    pub(crate) fn new(
        sdk: u32,
        shell: impl Fn(&str) -> ShellResult + Send + Sync + 'static,
    ) -> Self {
        Self::scripted(sdk, move |command| Ok(shell(command)))
    }

    /// [`Self::new`] with a shell that can also fail as a driver does: a
    /// timeout (`Failed`), a device that went away (`Disconnected`).
    pub(crate) fn scripted(
        sdk: u32,
        shell: impl Fn(&str) -> DriverResult<ShellResult> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sdk,
            shell: Box::new(shell),
            install: Box::new(|_| Ok("Success".to_owned())),
            shells: Mutex::new(Vec::new()),
            installs: Mutex::new(Vec::new()),
        }
    }

    /// Installs answer `reply(install)` (pm's status line) instead of `Success`.
    pub(crate) fn installing(
        mut self,
        reply: impl Fn(&InstallCall) -> DriverResult<String> + Send + Sync + 'static,
    ) -> Self {
        self.install = Box::new(reply);
        self
    }

    /// A framed shell's answer: `stdout`, exit 0.
    pub(crate) fn out(stdout: &str) -> ShellResult {
        ShellResult {
            stdout: stdout.as_bytes().to_vec(),
            exit: Some(0),
            ..ShellResult::default()
        }
    }

    /// A framed shell's failure: `stderr`, exit `exit`.
    pub(crate) fn failing(stderr: &str, exit: u8) -> ShellResult {
        ShellResult {
            stderr: stderr.as_bytes().to_vec(),
            exit: Some(exit),
            ..ShellResult::default()
        }
    }

    /// Every command the shell was given, in order.
    pub(crate) fn commands(&self) -> Vec<String> {
        lock(&self.shells)
            .iter()
            .map(|call| call.command.clone())
            .collect()
    }

    pub(crate) fn shell_calls(&self) -> Vec<ShellCall> {
        lock(&self.shells).clone()
    }

    pub(crate) fn installs(&self) -> Vec<InstallCall> {
        lock(&self.installs).clone()
    }
}

/// `bytes` cut to `limit`, and whether that cut anything.
fn kept(mut bytes: Vec<u8>, limit: usize) -> (Vec<u8>, bool) {
    let cut = bytes.len() > limit;
    bytes.truncate(limit);
    (bytes, cut)
}

impl AndroidControl for FakeAndroid {
    fn sdk(&self) -> DriverResult<u32> {
        Ok(self.sdk)
    }

    fn shell(&self, command: &str, limit: usize, timeout: Duration) -> DriverResult<ShellResult> {
        lock(&self.shells).push(ShellCall {
            command: command.to_owned(),
            limit,
            timeout,
        });
        let result = (self.shell)(command)?;
        let (stdout, stdout_cut) = kept(result.stdout, limit);
        let (stderr, stderr_cut) = kept(result.stderr, limit);
        Ok(ShellResult {
            stdout,
            stderr,
            exit: result.exit,
            truncated: result.truncated || stdout_cut || stderr_cut,
            timed_out: result.timed_out,
        })
    }

    /// Reads the stream in 64 KiB chunks, looking at `cancelled` between them as
    /// the real one does; a stream shorter than `size` fails as pm would.
    fn install(
        &self,
        apk: &mut dyn Read,
        size: u64,
        flags: &[&str],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> DriverResult<String> {
        let mut bytes = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        while (bytes.len() as u64) < size {
            if cancelled() {
                return Err(DriverError::Failed(format!(
                    "the install was stopped after {} of {size} bytes",
                    bytes.len()
                )));
            }
            let want = chunk
                .len()
                .min(usize::try_from(size - bytes.len() as u64).unwrap_or(usize::MAX));
            let read = apk
                .read(&mut chunk[..want])
                .map_err(|error| DriverError::Failed(format!("could not read the APK: {error}")))?;
            if read == 0 {
                return Err(DriverError::Failed(format!(
                    "the APK ended after {} of {size} bytes",
                    bytes.len()
                )));
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        let install = InstallCall {
            bytes,
            size,
            flags: flags.iter().map(|flag| (*flag).to_owned()).collect(),
            timeout,
        };
        let reply = (self.install)(&install);
        lock(&self.installs).push(install);
        reply
    }
}

/// The session id a [`FakeIos`] puts in the routes it records.
pub(crate) const FAKE_WDA_SESSION: &str = "fake-session";

/// One [`IosControl::call`] as the fake received it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WdaCall {
    pub method: String,
    /// The path as WebDriverAgent would see it: `/session/fake-session/wda/apps/launch`,
    /// `/status`.
    pub path: String,
    pub body: Option<Value>,
    pub timeout: Duration,
}

type WdaScript = dyn Fn(&str, WdaRoute<'_>, Option<&Value>) -> DriverResult<Value> + Send + Sync;

/// WebDriverAgent, answered by a closure of `(method, route, body)`.
pub(crate) struct FakeIos {
    reply: Box<WdaScript>,
    calls: Mutex<Vec<WdaCall>>,
}

impl FakeIos {
    pub(crate) fn new(
        reply: impl Fn(&str, WdaRoute<'_>, Option<&Value>) -> DriverResult<Value>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        Self {
            reply: Box::new(reply),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn calls(&self) -> Vec<WdaCall> {
        lock(&self.calls).clone()
    }

    /// Every call as `METHOD path`.
    pub(crate) fn lines(&self) -> Vec<String> {
        lock(&self.calls)
            .iter()
            .map(|call| format!("{} {}", call.method, call.path))
            .collect()
    }
}

impl IosControl for FakeIos {
    fn call(
        &self,
        method: &str,
        route: WdaRoute<'_>,
        body: Option<&Value>,
        timeout: Duration,
    ) -> DriverResult<Value> {
        let path = match route {
            WdaRoute::Session(suffix) => format!("/session/{FAKE_WDA_SESSION}{suffix}"),
            WdaRoute::Root(path) => path.to_owned(),
        };
        lock(&self.calls).push(WdaCall {
            method: method.to_owned(),
            path,
            body: body.cloned(),
            timeout,
        });
        (self.reply)(method, route, body)
    }
}

/// A VNC server's clipboard: its channel, what it last sent, whether it is
/// Apple's — all settable — and every text put on it.
pub(crate) struct FakeDesktop {
    channel: Mutex<ClipboardChannel>,
    clipboard: Mutex<Option<ClipboardText>>,
    macos: AtomicBool,
    writes: Mutex<Vec<String>>,
    /// The timeout of each read.
    reads: Mutex<Vec<Duration>>,
    /// What every call answers instead, while set.
    failure: Mutex<Option<DriverError>>,
}

impl FakeDesktop {
    /// A server on `channel` with an empty clipboard, not Apple's.
    pub(crate) fn new(channel: ClipboardChannel) -> Self {
        Self {
            channel: Mutex::new(channel),
            clipboard: Mutex::new(None),
            macos: AtomicBool::new(false),
            writes: Mutex::new(Vec::new()),
            reads: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
        }
    }

    pub(crate) fn set_channel(&self, channel: ClipboardChannel) {
        *lock(&self.channel) = channel;
    }

    /// What the server's clipboard holds (as it sent it, `age` ago), or nothing.
    pub(crate) fn set_clipboard_text(&self, text: Option<&str>, age: Duration) {
        *lock(&self.clipboard) = text.map(|text| ClipboardText {
            text: text.to_owned(),
            received: Instant::now().checked_sub(age).unwrap_or_else(Instant::now),
        });
    }

    pub(crate) fn set_macos(&self, macos: bool) {
        self.macos.store(macos, Ordering::SeqCst);
    }

    /// Every call fails with `failure` while it is set.
    pub(crate) fn fail_with(&self, failure: Option<DriverError>) {
        *lock(&self.failure) = failure;
    }

    /// Every text put on the clipboard, in order.
    pub(crate) fn writes(&self) -> Vec<String> {
        lock(&self.writes).clone()
    }

    pub(crate) fn reads(&self) -> Vec<Duration> {
        lock(&self.reads).clone()
    }

    fn failure(&self) -> DriverResult<()> {
        match lock(&self.failure).clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl DesktopControl for FakeDesktop {
    fn clipboard_channel(&self) -> ClipboardChannel {
        *lock(&self.channel)
    }

    /// Refuses on a Latin-1 channel what Latin-1 cannot carry, as the trait
    /// says; the clipboard it reads back is only what a test set.
    fn set_clipboard(&self, text: &str) -> DriverResult<()> {
        self.failure()?;
        if self.clipboard_channel() == ClipboardChannel::Latin1
            && text.chars().any(|character| u32::from(character) > 0xff)
        {
            return Err(DriverError::Unsupported(
                "this VNC server takes only Latin-1 clipboard text".to_owned(),
            ));
        }
        lock(&self.writes).push(text.to_owned());
        Ok(())
    }

    fn clipboard(&self, timeout: Duration) -> DriverResult<Option<ClipboardText>> {
        self.failure()?;
        lock(&self.reads).push(timeout);
        Ok(lock(&self.clipboard).clone())
    }

    fn is_macos(&self) -> bool {
        self.macos.load(Ordering::SeqCst)
    }
}

/// Every connect answers with `driver`.
pub(crate) fn ready(driver: Arc<FakeDriver>) -> ConnectFn {
    Arc::new(move |_target, _connector, _auth, _events| Ok(driver.clone() as Arc<dyn Driver>))
}

/// Every connect fails with `message`, which ends a session's first connect as failed.
pub(crate) fn refused(message: &str) -> ConnectFn {
    let message = message.to_owned();
    Arc::new(move |_target, _connector, _auth, _events| Err(DriverError::Failed(message.clone())))
}

/// Holds connects until it is opened.
#[derive(Clone, Default)]
pub(crate) struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub(crate) fn open(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }

    fn wait(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut open = self.0 .0.lock().unwrap();
        while !*open && Instant::now() < deadline {
            open = self
                .0
                 .1
                .wait_timeout(open, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }
}

/// Every connect waits for `gate`, then answers with `driver`.
pub(crate) fn gated(driver: Arc<FakeDriver>, gate: Gate) -> ConnectFn {
    Arc::new(move |_target, _connector, _auth, _events| {
        gate.wait();
        Ok(driver.clone() as Arc<dyn Driver>)
    })
}

/// A registered SSH machine at `host` that nothing ever dials: a session's fake
/// driver never uses its connector.
pub(crate) fn machine(id: &str, name: &str, host: &str) -> ControlMachine {
    let runner = ShellRunner::Ssh {
        host: host.to_owned(),
        port: 0,
        identity_file: String::new(),
        env: BTreeMap::new(),
        agent_shell: Default::default(),
    };
    ControlMachine {
        id: id.to_owned(),
        name: name.to_owned(),
        host: host.to_owned(),
        port: 0,
        machine: Ok(RemoteMachine::new(
            runner,
            format!("ssh:{id}"),
            name.to_owned(),
        )),
    }
}
