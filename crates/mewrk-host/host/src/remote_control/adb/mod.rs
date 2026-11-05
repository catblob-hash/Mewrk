//! Android over the adb server's protocol: `screencap` for the picture,
//! `input` for touches and keys, `uiautomator dump` for the element tree.
//!
//! The adb **server** (normally `127.0.0.1:5037` of the place) speaks a plain
//! ASCII protocol, so no `adb` binary is involved once it runs — the driver
//! talks to the port through its [`Connector`], which is also how it reaches the
//! server of an SSH machine. The binary is run in exactly one situation: this
//! computer, and a connection to the port was *refused*, so that
//! `adb start-server` can bring the server up ([`server`]).
//!
//! adb has no persistent connection to hold. Every command opens its own
//! ([`wire`]), so the driver keeps no socket between calls and a command that
//! finds its device gone is what reports the disconnect
//! ([`DriverEvent::Disconnected`]).
//!
//! The device is chosen without guessing ([`devices`]) and never connected: a
//! wireless serial must already be in the server's list. Input is built as
//! device-shell command strings ([`input`]); the only free text in them, the
//! text being typed, is always one single-quoted shell word.

mod devices;
mod input;
#[cfg(target_os = "macos")]
mod record;
mod screen;
mod server;
mod uiautomator;
mod wire;

#[cfg(test)]
mod tests;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use super::{
    driver::{
        AndroidControl, Driver, DriverError, DriverEvent, DriverEvents, DriverInfo, DriverResult,
        Frame, Platform, ScreenUpdate, ShellResult, UiNode,
    },
    keys::{self, Key, KeyChord},
    target::is_plausible_serial,
    transport::Connector,
};
use crate::preview_launch_config::sanitize_for_message;
use devices::{parse_devices, DeviceEntry, Unavailable};
pub(crate) use input::shell_quote;
use wire::{CappedOutput, InstallFailure, Server, ShellOutput, WireError, HOST_TIMEOUT};

/// An `input` command takes this long at most (a process start on an old
/// device, and the adb round trip); a gesture's own duration is added.
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);
/// Extra time for typing, which is several `input` calls in one command.
const TEXT_EXTRA: Duration = Duration::from_secs(30);
const SCREENCAP_TIMEOUT: Duration = Duration::from_secs(30);
const DUMP_TIMEOUT: Duration = Duration::from_secs(30);
/// What a command that prints a line or two may print.
const SMALL_OUTPUT: usize = 64 * 1024;
/// `getprop ro.build.version.sdk`: no JVM to start, so it answers at once.
const SDK_TIMEOUT: Duration = Duration::from_secs(5);
/// Less than this left of a failed call, and the device list is not asked why it failed.
const EXPLAIN_MIN: Duration = Duration::from_millis(200);
const SCREENCAP_LIMIT: usize = 64 * 1024 * 1024;
const DUMP_LIMIT: usize = 8 * 1024 * 1024;
/// Less than this left of a bounded dump ([`AdbDriver::dump_tree`]), and no attempt is made: a
/// `uiautomator dump` takes longer than that to start.
const DUMP_MIN_ATTEMPT: Duration = Duration::from_millis(500);
/// Gesture durations beyond this are clamped, so a bad number cannot hold a
/// connection open for ever.
const MAX_GESTURE_MS: u32 = 60_000;
/// Attempts at a dump the device answered with "not idle yet".
const DUMP_ATTEMPTS: usize = 3;
#[cfg(not(test))]
const DUMP_RETRY_DELAY: Duration = Duration::from_millis(400);
#[cfg(test)]
const DUMP_RETRY_DELAY: Duration = Duration::from_millis(10);
/// How often a device that is still coming up is looked at again.
#[cfg(not(test))]
const BOOT_POLL: Duration = Duration::from_secs(1);
#[cfg(test)]
const BOOT_POLL: Duration = Duration::from_millis(50);
/// How long a just-started adb server may take to answer.
#[cfg(not(test))]
const SERVER_STARTS_WITHIN: Duration = Duration::from_secs(10);
#[cfg(test)]
const SERVER_STARTS_WITHIN: Duration = Duration::from_millis(600);
const SERVER_START_POLL: Duration = Duration::from_millis(100);
/// `input keycombination` arrived with Android 12.
const KEYCOMBINATION_MIN_SDK: u32 = 31;
/// The option words a streamed install passes on to pm, and nothing else. `-R` (Android 9 and
/// later) refuses to replace an installed app, which pm otherwise does by default there.
const INSTALL_FLAGS: [&str; 5] = ["-r", "-R", "-t", "-d", "-g"];
const NO_STREAMED_INSTALL: &str =
    "This device's Android is too old for streamed installs (it needs Android 7.0).";

const CLOSED: &str = "the Android connection was closed";
const DUMP_TTY: &str = "uiautomator dump /dev/tty";
/// For shells that have no `/dev/tty` (a raw exec stream has no terminal):
/// dump to a file the shell user may write, print it, remove it.
const DUMP_FILE: &str = "uiautomator dump /data/local/tmp/mewrk-ui.xml && cat /data/local/tmp/mewrk-ui.xml; rm -f /data/local/tmp/mewrk-ui.xml";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Connects to the adb server on `port` of the connector's place and picks the
/// device: `device` when given, else the only one online. `boot_wait` (an entry
/// with `launch`, whose emulator may still be booting) waits that long for the
/// device to appear and finish booting.
pub fn connect(
    connector: Arc<dyn Connector>,
    port: u16,
    device: Option<String>,
    boot_wait: Option<Duration>,
    events: DriverEvents,
) -> DriverResult<Arc<dyn Driver>> {
    connect_with(
        connector,
        port,
        device,
        boot_wait,
        events,
        &server::start_server,
    )
}

/// [`connect`], with what starts the adb server passed in so that a test can
/// prove it is (or is not) called.
fn connect_with(
    connector: Arc<dyn Connector>,
    port: u16,
    device: Option<String>,
    boot_wait: Option<Duration>,
    events: DriverEvents,
    start_server: &dyn Fn(u16) -> Result<(), String>,
) -> DriverResult<Arc<dyn Driver>> {
    if let Some(serial) = &device {
        if !is_plausible_serial(serial) {
            return Err(DriverError::Failed(format!(
                "\"{}\" is not a device serial adb could have printed.",
                sanitize_for_message(serial)
            )));
        }
    }
    let server = Server::new(connector, port);
    ensure_server(&server, &events, start_server)?;
    let entry = wait_for_device(&server, device.as_deref(), boot_wait, &events)?;

    let features = match server.features(&entry.serial) {
        Ok(features) => Some(features),
        Err(error) => {
            events(DriverEvent::Log(format!(
                "adb could not list the device's features ({}); using the plain exec service",
                error.display()
            )));
            None
        }
    };
    let shell_v2 = features
        .as_ref()
        .is_some_and(|features| features.iter().any(|feature| feature == "shell_v2"));
    let driver = AdbDriver {
        label: entry.label(),
        serial: entry.serial.clone(),
        server,
        shell_v2,
        events: events.clone(),
        closed: AtomicBool::new(false),
        // "Not known to be up yet": a failure before the first success is the
        // connect's own error, not a disconnect to announce.
        gone: AtomicBool::new(true),
        frame_gate: Mutex::new(()),
        last: Mutex::new(None),
        sdk: Mutex::new(None),
            #[cfg(target_os = "macos")]
            record_size: Mutex::new(None),
        features: Mutex::new(features),
        dump: Mutex::new(DumpVia::Tty),
    };
    // The size has to be known before the session can lay out its coordinates.
    let first = driver.capture(false)?;
    // The Android version decides several tools' commands: read it now, while there is time,
    // rather than inside a call that has little left. A device that cannot say is asked again
    // when it is needed.
    if let Err(error) = driver.sdk() {
        events(DriverEvent::Log(format!(
            "adb could not read the device's Android version yet ({error})"
        )));
    }
    events(DriverEvent::Log(format!(
        "Android device {} ({}) on {}: screen {}x{}, {}",
        sanitize_for_message(&driver.serial),
        sanitize_for_message(&driver.label),
        sanitize_for_message(driver.server.connector().place_label()),
        first.width,
        first.height,
        if shell_v2 { "shell_v2" } else { "exec" }
    )));
    Ok(Arc::new(driver))
}

/// Whether `message` (a connector's) says the port refused the connection:
/// nothing is listening, which is the only thing that justifies running `adb`.
fn is_refused(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("nothing is listening") || lower.contains("refused")
}

/// Makes sure an adb server answers on the port, starting one only when the
/// place is this computer and the connection was refused.
fn ensure_server(
    server: &Server,
    events: &DriverEvents,
    start_server: &dyn Fn(u16) -> Result<(), String>,
) -> DriverResult<()> {
    let place = sanitize_for_message(server.connector().place_label());
    let port = server.port();
    let announce = |version: u32| {
        events(DriverEvent::Log(format!(
            "adb server on port {port} of {place} answers (protocol version {version})"
        )));
    };
    let unreachable = match server.version() {
        Ok(version) => {
            announce(version);
            return Ok(());
        }
        Err(WireError::Unreachable(message)) => message,
        Err(other) => {
            return Err(DriverError::Failed(format!(
                "The adb server on port {port} of {place} did not answer properly: {}",
                other.display()
            )))
        }
    };
    if !server.connector().is_local() {
        return Err(DriverError::Failed(format!(
            "No adb server is reachable on port {port} of {place} ({}). Run `adb start-server` on that machine, then try again.",
            sanitize_for_message(&unreachable)
        )));
    }
    if !is_refused(&unreachable) {
        // Something may be listening and slow; running adb against it could
        // replace it. Report instead.
        return Err(DriverError::Failed(format!(
            "The adb server on port {port} of this computer is not usable: {}",
            sanitize_for_message(&unreachable)
        )));
    }
    events(DriverEvent::Log(format!(
        "no adb server is listening on port {port}; starting one"
    )));
    start_server(port).map_err(DriverError::Failed)?;
    let deadline = Instant::now() + SERVER_STARTS_WITHIN;
    loop {
        match server.version() {
            Ok(version) => {
                announce(version);
                return Ok(());
            }
            Err(WireError::Unreachable(_)) if Instant::now() < deadline => {
                thread::sleep(SERVER_START_POLL);
            }
            Err(error) => {
                return Err(DriverError::Failed(format!(
                    "`adb start-server` ran, but port {port} still does not answer: {}",
                    error.display()
                )));
            }
        }
    }
}

/// One look at the device list (and, when waiting for a boot, at the boot
/// flag): the device, or why there is none yet.
fn look_for_device(
    server: &Server,
    wanted: Option<&str>,
    check_boot: bool,
) -> Result<DeviceEntry, Unavailable> {
    let still_possible = |message: String| Unavailable {
        message,
        may_resolve: true,
    };
    let listing = server.devices().map_err(|error| {
        still_possible(format!(
            "The adb server could not list devices: {}",
            error.display()
        ))
    })?;
    let entry = devices::select(
        &parse_devices(&listing),
        wanted,
        server.connector().place_label(),
    )?;
    if !is_plausible_serial(&entry.serial) {
        return Err(Unavailable {
            message: format!(
                "adb reported a device serial that is not accepted: \"{}\".",
                sanitize_for_message(&entry.serial)
            ),
            may_resolve: false,
        });
    }
    if check_boot {
        let booted = server
            .shell(
                &entry.serial,
                "getprop sys.boot_completed",
                false,
                SMALL_OUTPUT,
                SHELL_TIMEOUT,
            )
            .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "1");
        match booted {
            Ok(true) => {}
            Ok(false) => {
                return Err(still_possible(format!(
                    "Android device \"{}\" is still booting.",
                    sanitize_for_message(&entry.serial)
                )))
            }
            Err(error) => {
                return Err(still_possible(format!(
                    "Android device \"{}\" is not answering yet: {}",
                    sanitize_for_message(&entry.serial),
                    error.display()
                )))
            }
        }
    }
    Ok(entry)
}

/// The device to drive. With `boot_wait` the device may not exist yet or may
/// still be starting; it is looked for every second until it is online and has
/// finished booting, or the time is up.
fn wait_for_device(
    server: &Server,
    wanted: Option<&str>,
    boot_wait: Option<Duration>,
    events: &DriverEvents,
) -> DriverResult<DeviceEntry> {
    let started = Instant::now();
    let mut announced = false;
    loop {
        // A session stopped while its emulator boots does not keep polling for
        // the rest of the boot wait, beside the session that replaced it.
        if server.connector().cancelled() {
            return Err(DriverError::Failed(
                "the session was stopped while it waited for the device".to_owned(),
            ));
        }
        let problem = match look_for_device(server, wanted, boot_wait.is_some()) {
            Ok(entry) => return Ok(entry),
            Err(problem) => problem,
        };
        let Some(patience) = boot_wait else {
            return Err(DriverError::Failed(problem.message));
        };
        let waited = started.elapsed();
        if !problem.may_resolve || waited >= patience {
            let note = if problem.may_resolve {
                format!(" Waited {} seconds for it.", patience.as_secs())
            } else {
                String::new()
            };
            return Err(DriverError::Failed(format!("{}{note}", problem.message)));
        }
        if !announced {
            announced = true;
            events(DriverEvent::Log(format!(
                "waiting up to {} seconds for the device to come up: {}",
                patience.as_secs(),
                problem.message
            )));
        }
        thread::sleep(BOOT_POLL.min(patience - waited));
    }
}

/// How the element tree is dumped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DumpVia {
    /// To `/dev/tty`: no file, but only a shell with a terminal has one.
    Tty,
    /// To a file in `/data/local/tmp`, printed and removed.
    File,
}

struct AdbDriver {
    server: Server,
    serial: String,
    label: String,
    /// Whether the device speaks the framed shell protocol, which keeps stderr
    /// out of a screenshot and reports exit statuses.
    shell_v2: bool,
    events: DriverEvents,
    closed: AtomicBool,
    /// Whether the device was last found missing, so that one outage is
    /// announced once and a recovery re-arms it.
    gone: AtomicBool,
    /// One screen capture at a time; a waiter takes the one that finished.
    frame_gate: Mutex<()>,
    last: Mutex<Option<Frame>>,
    /// `ro.build.version.sdk`, asked once.
    sdk: Mutex<Option<u32>>,
    /// The size `screenrecord` is asked for, after its encoder refused the screen's own.
    #[cfg(target_os = "macos")]
    record_size: Mutex<Option<(u32, u32)>>,
    /// What adb says the device supports (`shell_v2`, `cmd`, ...), read when it connected, or
    /// again when that failed.
    features: Mutex<Option<Vec<String>>>,
    dump: Mutex<DumpVia>,
}

impl AdbDriver {
    fn ensure_open(&self) -> DriverResult<()> {
        if self.closed.load(Ordering::Acquire) {
            Err(DriverError::Disconnected(CLOSED.to_owned()))
        } else {
            Ok(())
        }
    }

    /// The device's UI tree through `uiautomator dump`. With `within`, the whole read — retries
    /// and the fall back to a file included — has that long, each attempt only what is left of
    /// it (at most [`DUMP_TIMEOUT`]); without, each attempt has [`DUMP_TIMEOUT`].
    fn dump_tree(&self, within: Option<Duration>) -> DriverResult<Option<UiNode>> {
        self.ensure_open()?;
        let until = within.map(|within| Instant::now() + within);
        let mut via = *lock(&self.dump);
        let mut transient = 0;
        let problem = loop {
            let command = match via {
                DumpVia::Tty => DUMP_TTY,
                DumpVia::File => DUMP_FILE,
            };
            let attempt = match until {
                None => DUMP_TIMEOUT,
                Some(until) => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left < DUMP_MIN_ATTEMPT {
                        return Err(DriverError::Failed(
                            "uiautomator did not finish reading the screen in the time this call had left. Try again.".to_owned(),
                        ));
                    }
                    left.min(DUMP_TIMEOUT)
                }
            };
            let output = self.run_until(command, DUMP_LIMIT, attempt, until)?;
            // Over a framed shell the tool's complaints are on stderr, over exec
            // they are in the stream; read both.
            let mut printed = output.stdout;
            printed.extend_from_slice(&output.stderr);
            if let Some(xml) = uiautomator::extract_hierarchy(&printed) {
                let root = uiautomator::parse(&xml).map_err(|message| {
                    DriverError::Failed(format!(
                        "The device's UI dump could not be read: {message}."
                    ))
                })?;
                *lock(&self.dump) = via;
                return Ok(Some(root));
            }
            let text = String::from_utf8_lossy(&printed);
            if uiautomator::is_transient(&text) {
                transient += 1;
                if transient < DUMP_ATTEMPTS {
                    thread::sleep(DUMP_RETRY_DELAY);
                    continue;
                }
                break first_line(&printed);
            }
            if via == DumpVia::Tty {
                via = DumpVia::File;
                continue;
            }
            break first_line(&printed);
        };
        Err(DriverError::Failed(format!(
            "uiautomator could not read the screen ({}). Try again in a moment; the screen may still be changing, show a secure window, or another automation tool may be holding the accessibility service.",
            if problem.is_empty() { "it printed nothing".to_owned() } else { problem }
        )))
    }

    /// Runs one command in the device's shell.
    fn run(&self, command: &str, limit: usize, timeout: Duration) -> DriverResult<ShellOutput> {
        self.run_until(command, limit, timeout, None)
    }

    /// [`Self::run`], whose failure is explained within `until` ([`Self::explain_until`]).
    fn run_until(
        &self,
        command: &str,
        limit: usize,
        timeout: Duration,
        until: Option<Instant>,
    ) -> DriverResult<ShellOutput> {
        self.ensure_open()?;
        match self
            .server
            .shell(&self.serial, command, self.shell_v2, limit, timeout)
        {
            Ok(output) => {
                self.gone.store(false, Ordering::Release);
                Ok(output)
            }
            Err(error) => Err(self.explain_until(error, until)),
        }
    }

    /// Turns a failed exchange into the driver's error, and into a
    /// [`DriverEvent::Disconnected`] when the cause is that the device (or the
    /// server) is no longer there.
    fn explain(&self, error: WireError) -> DriverError {
        self.explain_until(error, None)
    }

    /// [`Self::explain`] for a call that had to be over by `until`: the device list that tells a
    /// broken exchange from a vanished device is asked only within what is left of it, and not at
    /// all when nothing is (the next call finds a missing device then).
    fn explain_until(&self, error: WireError, until: Option<Instant>) -> DriverError {
        let ask_within = match until {
            None => HOST_TIMEOUT,
            Some(until) => until
                .saturating_duration_since(Instant::now())
                .min(HOST_TIMEOUT),
        };
        match &error {
            WireError::Unreachable(message) => self.lost(format!(
                "The adb server is no longer reachable: {}",
                sanitize_for_message(message)
            )),
            WireError::Refused(message) if device_is_missing(message) => self.lost(format!(
                "Android device \"{}\" is gone: {}",
                sanitize_for_message(&self.serial),
                sanitize_for_message(message)
            )),
            WireError::Refused(message) => DriverError::Failed(format!(
                "adb refused the command: {}",
                sanitize_for_message(message)
            )),
            WireError::Protocol(message) | WireError::Io(message) if ask_within < EXPLAIN_MIN => {
                DriverError::Failed(message.clone())
            }
            WireError::Protocol(message) | WireError::Io(message) => {
                // The exchange broke; if the device is no longer listed that is why.
                let state = self
                    .server
                    .query("host:devices-l", ask_within)
                    .map(|payload| {
                        parse_devices(&String::from_utf8_lossy(&payload))
                            .into_iter()
                            .find(|device| device.serial == self.serial)
                    });
                match state {
                    Ok(Some(device)) if device.is_online() => DriverError::Failed(message.clone()),
                    Ok(Some(device)) => self.lost(format!(
                        "Android device \"{}\" is {}.",
                        sanitize_for_message(&self.serial),
                        sanitize_for_message(&device.state)
                    )),
                    Ok(None) => self.lost(format!(
                        "Android device \"{}\" is no longer in adb's device list.",
                        sanitize_for_message(&self.serial)
                    )),
                    // A list that did not come in the little time left says nothing either way.
                    Err(WireError::Io(_)) if ask_within < HOST_TIMEOUT => {
                        DriverError::Failed(message.clone())
                    }
                    Err(_) => self.lost(format!("The adb server stopped answering: {message}")),
                }
            }
        }
    }

    fn lost(&self, reason: String) -> DriverError {
        if !self.gone.swap(true, Ordering::AcqRel) {
            (self.events)(DriverEvent::Disconnected(reason.clone()));
        }
        DriverError::Disconnected(reason)
    }

    /// Runs one input command and checks that the device took it.
    fn input(&self, command: &str, extra: Duration) -> DriverResult<()> {
        let output = self.run(command, SMALL_OUTPUT, SHELL_TIMEOUT + extra)?;
        check_input(&output)
    }

    /// A new picture, or the one another thread took while this one waited —
    /// only if that one *began* after this call did.
    ///
    /// A frame's `captured_at` is when its screencap was asked for, not when
    /// the PNG arrived: `screencap` reads the display first and spends the rest
    /// of its half second encoding and sending. A capture the viewer's pump began
    /// before the model's tap landed shows the screen before the tap however late
    /// it finishes, so a caller that asked after the tap waits it out and takes
    /// its own.
    fn capture(&self, announce: bool) -> DriverResult<Frame> {
        let asked = Instant::now();
        let _gate = lock(&self.frame_gate);
        if let Some(frame) = lock(&self.last).as_ref() {
            if frame.captured_at >= asked {
                return Ok(frame.clone());
            }
        }
        let started = Instant::now();
        let output = self.run("screencap -p", SCREENCAP_LIMIT, SCREENCAP_TIMEOUT)?;
        // A failed screencap explains itself on stderr when the streams are apart.
        let evidence = if output.stdout.is_empty() {
            &output.stderr
        } else {
            &output.stdout
        };
        let picture = screen::decode(evidence).map_err(DriverError::Failed)?;
        let (frame, resized) = {
            let mut last = lock(&self.last);
            let resized = last.as_ref().is_some_and(|previous| {
                (previous.width, previous.height) != (picture.width, picture.height)
            });
            let frame = Frame {
                width: picture.width,
                height: picture.height,
                rgba: Arc::from(picture.rgba),
                seq: last.as_ref().map_or(1, |previous| previous.seq + 1),
                captured_at: started,
            };
            *last = Some(frame.clone());
            (frame, resized)
        };
        if announce {
            if resized {
                (self.events)(DriverEvent::Resized {
                    width: frame.width,
                    height: frame.height,
                });
            }
            (self.events)(DriverEvent::Frame);
        }
        Ok(frame)
    }

    fn sdk(&self) -> DriverResult<u32> {
        if let Some(sdk) = *lock(&self.sdk) {
            return Ok(sdk);
        }
        let output = self.run_until(
            "getprop ro.build.version.sdk",
            SMALL_OUTPUT,
            SDK_TIMEOUT,
            Some(Instant::now() + SDK_TIMEOUT),
        )?;
        let sdk = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|_| {
                DriverError::Failed("Could not read the Android version of the device.".to_owned())
            })?;
        *lock(&self.sdk) = Some(sdk);
        Ok(sdk)
    }

    /// Whether adb lists `feature` for the device.
    fn has_feature(&self, feature: &str) -> DriverResult<bool> {
        if let Some(features) = lock(&self.features).as_ref() {
            return Ok(features.iter().any(|known| known == feature));
        }
        let features = self
            .server
            .features(&self.serial)
            .map_err(|error| self.explain(error))?;
        let has = features.iter().any(|known| known == feature);
        *lock(&self.features) = Some(features);
        Ok(has)
    }
}

/// Whether a `FAIL` from the server says the device is not there to be driven.
fn device_is_missing(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    ["not found", "offline", "unauthorized", "no devices"]
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// What `input` printed, shortened to one line for an error message.
fn first_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(2000)]);
    sanitize_for_message(text.trim())
}

/// Whether the device took an input command. `input` prints nothing when it
/// worked; with a framed shell the exit status says so, and without one the
/// output is all there is to go on.
fn check_input(output: &ShellOutput) -> DriverResult<()> {
    let printed = if output.stderr.is_empty() {
        first_line(&output.stdout)
    } else {
        first_line(&output.stderr)
    };
    match output.exit {
        Some(0) => Ok(()),
        Some(status) => Err(DriverError::Failed(format!(
            "The device rejected the input command (exit status {status}){}",
            if printed.is_empty() {
                ".".to_owned()
            } else {
                format!(": {printed}")
            }
        ))),
        None => {
            let looks_failed = [
                "Exception",
                "Error",
                "error:",
                "not found",
                "Usage:",
                "Killed",
            ]
            .iter()
            .any(|marker| {
                String::from_utf8_lossy(&output.stdout).contains(marker)
                    || String::from_utf8_lossy(&output.stderr).contains(marker)
            });
            if looks_failed {
                Err(DriverError::Failed(format!(
                    "The device rejected the input command: {printed}"
                )))
            } else {
                Ok(())
            }
        }
    }
}

/// Android's key code for `key`, or the reason there is none.
fn key_code(key: &Key) -> DriverResult<u32> {
    match key {
        // The code table folds case; a key press of a capital is not a lowercase one.
        Key::Char(character) if character.is_ascii_uppercase() => Err(DriverError::Unsupported(
            format!(
                "Android key events cannot press a capital \"{character}\"; use the type action to enter capital letters."
            ),
        )),
        Key::Char(character) => keys::android_keycode(*key).ok_or_else(|| {
            DriverError::Unsupported(format!(
                "Android has no key code for \"{}\"; use the type action to enter it.",
                sanitize_for_message(&character.to_string())
            ))
        }),
        Key::Named(named) => keys::android_keycode(*key).ok_or_else(|| {
            DriverError::Unsupported(format!("Android has no key code for {named:?}."))
        }),
    }
}

impl Driver for AdbDriver {
    fn info(&self) -> DriverInfo {
        let (width, height) = lock(&self.last)
            .as_ref()
            .map_or((0, 0), |frame| (frame.width, frame.height));
        DriverInfo {
            platform: Platform::Android,
            label: self.label.clone(),
            width,
            height,
            has_ui_tree: true,
        }
    }

    fn frame(&self, fresh: bool) -> DriverResult<Frame> {
        self.ensure_open()?;
        if !fresh {
            if let Some(frame) = lock(&self.last).clone() {
                return Ok(frame);
            }
        }
        self.capture(true)
    }

    /// `screenrecord`'s H.264, off the phone's own encoder, through the Mac's own decoder.
    #[cfg(target_os = "macos")]
    fn stream(&self, stop: &(dyn Fn() -> bool + Sync), sink: &mut dyn FnMut(ScreenUpdate)) -> DriverResult<()> {
        self.ensure_open()?;
        record::stream(self, stop, sink)
    }

    fn tap(&self, x: u32, y: u32, count: u8) -> DriverResult<()> {
        self.input(&input::tap_command(x, y, count), Duration::ZERO)
    }

    fn long_press(&self, x: u32, y: u32, ms: u32) -> DriverResult<()> {
        let ms = ms.min(MAX_GESTURE_MS);
        self.input(
            &input::long_press_command(x, y, ms),
            Duration::from_millis(u64::from(ms)),
        )
    }

    fn swipe(&self, from: (u32, u32), to: (u32, u32), ms: u32) -> DriverResult<()> {
        let ms = ms.min(MAX_GESTURE_MS);
        self.input(
            &input::swipe_command(from, to, ms),
            Duration::from_millis(u64::from(ms)),
        )
    }

    fn key(&self, key: &Key, down: Option<bool>) -> DriverResult<()> {
        self.ensure_open()?;
        // A phone cannot hold a key; the press is the whole key-down.
        if down == Some(false) {
            return Ok(());
        }
        self.input(&input::keyevent_command(key_code(key)?), Duration::ZERO)
    }

    fn chord(&self, chord: &KeyChord) -> DriverResult<()> {
        if chord.modifiers.is_empty() {
            return self.key(&chord.key, None);
        }
        self.ensure_open()?;
        let codes = chord
            .modifiers
            .iter()
            .chain(std::iter::once(&chord.key))
            .map(key_code)
            .collect::<DriverResult<Vec<u32>>>()?;
        let sdk = self.sdk()?;
        if sdk < KEYCOMBINATION_MIN_SDK {
            return Err(DriverError::Unsupported(format!(
                "Key combinations need Android 12 (API 31) or newer, and this device runs API {sdk}. Press the keys one at a time instead, or use the type action for text."
            )));
        }
        self.input(&input::keycombination_command(&codes), Duration::ZERO)
    }

    fn text(&self, text: &str) -> DriverResult<()> {
        self.ensure_open()?;
        for command in input::text_commands(text)? {
            self.input(&command, TEXT_EXTRA)?;
        }
        Ok(())
    }

    fn check_text(&self, text: &str) -> DriverResult<()> {
        input::text_commands(text).map(|_| ())
    }

    fn ui_tree(&self) -> DriverResult<Option<UiNode>> {
        self.dump_tree(None)
    }

    fn ui_tree_within(&self, timeout: Duration) -> DriverResult<Option<UiNode>> {
        self.dump_tree(Some(timeout))
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn android(&self) -> Option<&dyn AndroidControl> {
        Some(self)
    }
}

/// The Android tools' commands: unlike input, a command that fails is not this layer's error —
/// its exit status and what it printed are the answer — and its output is cut at the caller's
/// limit rather than refused past it.
impl AndroidControl for AdbDriver {
    fn sdk(&self) -> DriverResult<u32> {
        self.ensure_open()?;
        AdbDriver::sdk(self)
    }

    /// A framed stream always ends with the command's exit status; one that ends before it, in
    /// time, was cut by the device or the server going away, which the device list then tells.
    fn shell(&self, command: &str, limit: usize, timeout: Duration) -> DriverResult<ShellResult> {
        self.ensure_open()?;
        let until = Instant::now() + timeout;
        match self
            .server
            .shell_capped(&self.serial, command, self.shell_v2, limit, timeout)
        {
            Ok(capped) if self.shell_v2 && capped.output.exit.is_none() && !capped.unfinished => {
                Err(self.explain_until(
                    WireError::Io(
                        "the device ended the command's output without its exit status".to_owned(),
                    ),
                    Some(until),
                ))
            }
            Ok(CappedOutput {
                output,
                truncated,
                unfinished,
            }) => {
                self.gone.store(false, Ordering::Release);
                Ok(ShellResult {
                    stdout: output.stdout,
                    stderr: output.stderr,
                    exit: output.exit,
                    truncated,
                    timed_out: unfinished,
                })
            }
            Err(error) => Err(self.explain_until(error, Some(until))),
        }
    }

    /// The streamed install of `adb install`, which needs the device's `cmd` (Android 7.0);
    /// nothing is sent to a device without it.
    fn install(
        &self,
        apk: &mut dyn std::io::Read,
        size: u64,
        flags: &[&str],
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> DriverResult<String> {
        self.ensure_open()?;
        if let Some(flag) = flags.iter().find(|flag| !INSTALL_FLAGS.contains(flag)) {
            return Err(DriverError::Failed(format!(
                "\"{}\" is not an install option this driver passes on to the device.",
                sanitize_for_message(flag)
            )));
        }
        if !self.has_feature("cmd")? {
            return Err(DriverError::Unsupported(NO_STREAMED_INSTALL.to_owned()));
        }
        let until = Instant::now() + timeout;
        match self
            .server
            .install(&self.serial, flags, size, apk, timeout, cancelled)
        {
            Ok(answer) => {
                self.gone.store(false, Ordering::Release);
                Ok(answer)
            }
            Err(InstallFailure::Wire(error)) => Err(self.explain_until(error, Some(until))),
            Err(InstallFailure::Stopped(message) | InstallFailure::Source(message)) => {
                Err(DriverError::Failed(message))
            }
        }
    }
}
