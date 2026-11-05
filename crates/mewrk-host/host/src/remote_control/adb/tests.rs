//! The driver against an in-process fake adb server.
//!
//! The fake speaks the framing the real one does — four hex digits of length
//! and a service, `OKAY`/`FAIL`, `host:transport:<serial>` followed by a device
//! service whose reply is raw (`exec:`) or framed (`shell,v2,raw:`) — over a
//! loopback socket, so the driver goes through the real [`LocalConnector`]. It
//! records every service string it receives, which is how the tests see exactly
//! what would have run on a device.

use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::*;
use crate::remote_control::{
    driver::{buttons, Rect},
    keys::{parse_chord, NamedKey},
    transport::{Connection, LocalConnector},
};

const SERIAL: &str = "R5CT1234ABC";
const PHONE: &str =
    "R5CT1234ABC            device usb:1-1 product:shiba model:Pixel_8 device:shiba transport_id:3\n";
const FEATURES_V2: &str = "shell_v2,cmd,stat_v2,ls_v2";
const FEATURES_OLD: &str = "cmd,stat_v2";
const WARNING: &str = "[Warning] Multiple displays were found, but no display id was specified! Defaulting to the first display found.\n";
const DUMP_XML: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" resource-id="" class="android.widget.FrameLayout" package="com.example" content-desc="" checkable="false" checked="false" clickable="false" enabled="true" focusable="false" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,0][1080,2400]"><node index="0" text="Tom &amp; Jerry" resource-id="com.example:id/title" class="android.widget.TextView" package="com.example" content-desc="" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[100,200][500,260]" /><node index="1" text="" resource-id="" class="android.widget.ImageButton" package="com.example" content-desc="Back &quot;up&quot;" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="false" scrollable="false" long-clickable="false" password="false" selected="false" bounds="[0,60][120,180]" /></node></hierarchy>"#;

fn gradient(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
    screen::tests::gradient(width, height)
}

// ---------------------------------------------------------------- the fake

/// What a fake device's shell answers to one command.
#[derive(Clone, Default)]
struct Reply {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: u8,
    /// A framed stream that ends before its exit packet, as when adbd or the device goes away.
    no_exit: bool,
}

impl Reply {
    fn out(bytes: impl AsRef<[u8]>) -> Self {
        Self {
            stdout: bytes.as_ref().to_vec(),
            ..Self::default()
        }
    }

    fn failing(stderr: &str, exit: u8) -> Self {
        Self {
            stderr: stderr.as_bytes().to_vec(),
            exit,
            ..Self::default()
        }
    }
}

type ShellFn = dyn Fn(&str) -> Reply + Send + Sync;
type DevicesFn = dyn Fn(usize) -> String + Send + Sync;
/// pm's answer to a streamed install, given its command and the bytes that arrived.
type InstallFn = dyn Fn(&str, &[u8]) -> String + Send + Sync;

struct Script {
    version: String,
    /// The `host:devices-l` payload, given how many listings came before.
    devices: Box<DevicesFn>,
    features: String,
    shell: Box<ShellFn>,
    install: Box<InstallFn>,
    /// Hang up right after the transport switch, as a device that vanishes does.
    hang_up: Arc<AtomicBool>,
}

impl Script {
    fn phone(shell: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        Self {
            version: "0029".to_owned(),
            devices: Box::new(|_| PHONE.to_owned()),
            features: FEATURES_V2.to_owned(),
            shell: Box::new(shell),
            install: Box::new(|_, _| "Success\n".to_owned()),
            hang_up: Arc::default(),
        }
    }

    fn install(mut self, answer: impl Fn(&str, &[u8]) -> String + Send + Sync + 'static) -> Self {
        self.install = Box::new(answer);
        self
    }

    fn devices(mut self, devices: impl Fn(usize) -> String + Send + Sync + 'static) -> Self {
        self.devices = Box::new(devices);
        self
    }

    fn features(mut self, features: &str) -> Self {
        self.features = features.to_owned();
        self
    }
}

/// A phone that answers the commands a connect needs.
fn phone_shell(png: Vec<u8>, sdk: u32) -> impl Fn(&str) -> Reply + Send + Sync + 'static {
    move |command| match command {
        "screencap -p" => Reply::out(&png),
        "getprop ro.build.version.sdk" => Reply::out(format!("{sdk}\n")),
        "getprop sys.boot_completed" => Reply::out("1\n"),
        _ => Reply::default(),
    }
}

/// What a streamed install sent: the command, the bytes that arrived, and whether they were the
/// whole `-S` size (or the client went away first).
#[derive(Clone, Debug)]
struct Body {
    command: String,
    bytes: Vec<u8>,
    whole: bool,
}

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<String>>,
    device_lists: AtomicUsize,
    /// Framed commands whose stdin the client closed.
    stdin_closed: AtomicUsize,
    bodies: Mutex<Vec<Body>>,
}

impl Recorder {
    fn record(&self, request: &str) {
        self.requests.lock().unwrap().push(request.to_owned());
    }
}

struct FakeAdb {
    port: u16,
    recorder: Arc<Recorder>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl FakeAdb {
    fn start(script: Script) -> Self {
        let script = Arc::new(script);
        Self::start_raw(move |stream, recorder| {
            let _ = serve(stream, recorder, &script);
        })
    }

    /// A server whose connections are handled by `handler`, one thread each.
    fn start_raw(handler: impl Fn(TcpStream, &Recorder) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorder = Arc::new(Recorder::default());
        let stop = Arc::new(AtomicBool::new(false));
        let handler = Arc::new(handler);
        let accept = {
            let (recorder, stop) = (recorder.clone(), stop.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let (handler, recorder) = (handler.clone(), recorder.clone());
                    thread::spawn(move || handler(stream, &recorder));
                }
            })
        };
        Self {
            port,
            recorder,
            stop,
            accept: Some(accept),
        }
    }

    fn requests(&self) -> Vec<String> {
        self.recorder.requests.lock().unwrap().clone()
    }

    /// The device-shell commands received, in order.
    fn commands(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|request| {
                request
                    .strip_prefix("shell,v2,raw:")
                    .or_else(|| request.strip_prefix("exec:"))
                    .map(str::to_owned)
            })
            .collect()
    }
}

impl Drop for FakeAdb {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<String> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = usize::from_str_radix(&String::from_utf8_lossy(&length), 16)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut request = vec![0u8; length];
    stream.read_exact(&mut request)?;
    Ok(String::from_utf8_lossy(&request).into_owned())
}

fn write_chunk(stream: &mut TcpStream, payload: &str) -> io::Result<()> {
    stream.write_all(format!("OKAY{:04x}{payload}", payload.len()).as_bytes())
}

fn write_fail(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    stream.write_all(format!("FAIL{:04x}{message}", message.len()).as_bytes())
}

fn write_packet(stream: &mut TcpStream, id: u8, data: &[u8]) -> io::Result<()> {
    let mut packet = vec![id];
    packet.extend_from_slice(&(data.len() as u32).to_le_bytes());
    packet.extend_from_slice(data);
    stream.write_all(&packet)
}

fn serve(mut stream: TcpStream, recorder: &Recorder, script: &Script) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = read_request(&mut stream)?;
    recorder.record(&request);
    if request == "host:version" {
        return write_chunk(&mut stream, &script.version);
    }
    if request == "host:devices-l" {
        let listed = recorder.device_lists.fetch_add(1, Ordering::SeqCst);
        return write_chunk(&mut stream, &(script.devices)(listed));
    }
    if request.starts_with("host-serial:") && request.ends_with(":features") {
        return write_chunk(&mut stream, &script.features);
    }
    let Some(serial) = request.strip_prefix("host:transport:") else {
        return write_fail(&mut stream, "unknown host service");
    };
    // The device is reachable if the listing the client last saw has it online.
    let seen = recorder
        .device_lists
        .load(Ordering::SeqCst)
        .saturating_sub(1);
    let online = (script.devices)(seen).lines().any(|line| {
        let mut tokens = line.split_whitespace();
        tokens.next() == Some(serial) && tokens.next() == Some("device")
    });
    if !online {
        return write_fail(&mut stream, &format!("device '{serial}' not found"));
    }
    stream.write_all(b"OKAY")?;
    if script.hang_up.load(Ordering::SeqCst) {
        return Ok(());
    }
    let service = read_request(&mut stream)?;
    recorder.record(&service);
    if let Some(command) = service.strip_prefix("shell,v2,raw:") {
        stream.write_all(b"OKAY")?;
        // The client ends the command's stdin before anything else.
        let mut packet = [0u8; 5];
        stream.read_exact(&mut packet)?;
        if packet == [4, 0, 0, 0, 0] {
            recorder.stdin_closed.fetch_add(1, Ordering::SeqCst);
        }
        let reply = (script.shell)(command);
        if !reply.stderr.is_empty() {
            write_packet(&mut stream, 2, &reply.stderr)?;
        }
        // Several packets, so the demultiplexer is exercised across boundaries.
        for chunk in reply.stdout.chunks(700) {
            write_packet(&mut stream, 1, chunk)?;
        }
        if !reply.no_exit {
            write_packet(&mut stream, 3, &[reply.exit])?;
        }
    } else if let Some(command) = service.strip_prefix("exec:cmd package install") {
        // `adb install`'s streamed install: exactly `-S` bytes follow on the same stream, and
        // pm answers once it has them all; a client that leaves early gets nothing.
        stream.write_all(b"OKAY")?;
        let command = format!("cmd package install{command}");
        let size: usize = command
            .split_whitespace()
            .skip_while(|word| *word != "-S")
            .nth(1)
            .and_then(|size| size.parse().ok())
            .unwrap_or(0);
        let mut bytes = Vec::with_capacity(size);
        let mut chunk = [0u8; 8192];
        while bytes.len() < size {
            let want = (size - bytes.len()).min(chunk.len());
            match stream.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break,
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            }
        }
        let whole = bytes.len() == size;
        let answer = whole.then(|| (script.install)(&command, &bytes));
        recorder.bodies.lock().unwrap().push(Body {
            command,
            bytes,
            whole,
        });
        if let Some(answer) = answer {
            stream.write_all(answer.as_bytes())?;
        }
    } else if let Some(command) = service.strip_prefix("exec:") {
        stream.write_all(b"OKAY")?;
        let reply = (script.shell)(command);
        // stderr is on the same stream, and comes out when it is printed.
        stream.write_all(&reply.stderr)?;
        stream.write_all(&reply.stdout)?;
    } else {
        return write_fail(&mut stream, "unknown device service");
    }
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

// ------------------------------------------------------------ the harness

type Events = Arc<Mutex<Vec<DriverEvent>>>;

fn sink() -> (DriverEvents, Events) {
    let events: Events = Arc::default();
    let writer = events.clone();
    let callback: DriverEvents = Arc::new(move |event| writer.lock().unwrap().push(event));
    (callback, events)
}

/// Everything but the log lines, in order.
fn signals(events: &Events) -> Vec<DriverEvent> {
    events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| !matches!(event, DriverEvent::Log(_)))
        .cloned()
        .collect()
}

fn logged(events: &Events) -> String {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            DriverEvent::Log(line) => Some(line.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Connected {
    driver: DriverResult<Arc<dyn Driver>>,
    events: Events,
    /// How many times the harness was asked to start adb.
    starts: Arc<AtomicUsize>,
}

impl Connected {
    fn driver(self) -> Arc<dyn Driver> {
        match self.driver {
            Ok(driver) => driver,
            Err(error) => panic!("expected a connection, got: {error}"),
        }
    }

    fn error(&self) -> DriverError {
        match &self.driver {
            Ok(_) => panic!("expected an error, but it connected"),
            Err(error) => error.clone(),
        }
    }
}

fn connect_through(
    connector: Arc<dyn Connector>,
    port: u16,
    device: Option<&str>,
    boot_wait: Option<Duration>,
    start: impl Fn(u16) -> Result<(), String>,
) -> Connected {
    let (events, log) = sink();
    let starts = Arc::new(AtomicUsize::new(0));
    let counter = starts.clone();
    let driver = connect_with(
        connector,
        port,
        device.map(str::to_owned),
        boot_wait,
        events,
        &move |port| {
            counter.fetch_add(1, Ordering::SeqCst);
            start(port)
        },
    );
    Connected {
        driver,
        events: log,
        starts,
    }
}

/// Connects over the real local connector; adb must not be started.
fn connect_local(port: u16, device: Option<&str>, boot_wait: Option<Duration>) -> Connected {
    connect_through(
        Arc::new(LocalConnector::new()),
        port,
        device,
        boot_wait,
        |_| Err("the test did not expect adb to be started".to_owned()),
    )
}

struct Setup {
    fake: FakeAdb,
    driver: Arc<dyn Driver>,
    events: Events,
}

fn setup(script: Script) -> Setup {
    let fake = FakeAdb::start(script);
    let connected = connect_local(fake.port, None, None);
    let events = connected.events.clone();
    Setup {
        driver: connected.driver(),
        events,
        fake,
    }
}

fn phone(sdk: u32, features: &str) -> Setup {
    setup(Script::phone(phone_shell(gradient(4, 6).0, sdk)).features(features))
}

/// A connector that refuses until `up` is set, and then dials the real port.
struct GatedConnector {
    inner: LocalConnector,
    up: Arc<AtomicBool>,
    local: bool,
    label: &'static str,
    refusal: &'static str,
}

impl Connector for GatedConnector {
    fn connect(&self, port: u16, timeout: Duration) -> Result<Connection, String> {
        if self.up.load(Ordering::SeqCst) {
            self.inner.connect(port, timeout)
        } else {
            Err(self.refusal.to_owned())
        }
    }

    fn place_label(&self) -> &str {
        self.label
    }

    fn place_key(&self) -> &str {
        if self.local {
            "local"
        } else {
            "ssh:test"
        }
    }

    fn is_local(&self) -> bool {
        self.local
    }
}

const REFUSED: &str = "nothing is listening on 127.0.0.1:5037 on this computer";

fn gated(
    up: &Arc<AtomicBool>,
    local: bool,
    label: &'static str,
    refusal: &'static str,
) -> Arc<GatedConnector> {
    Arc::new(GatedConnector {
        inner: LocalConnector::new(),
        up: up.clone(),
        local,
        label,
        refusal,
    })
}

fn texts(commands: &[String]) -> Vec<&str> {
    commands.iter().map(String::as_str).collect()
}

// ----------------------------------------------------- choosing the device

#[test]
fn a_single_online_device_is_chosen_and_adb_connect_is_never_sent() {
    let (png, _) = gradient(4, 6);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)));
    let connected = connect_local(fake.port, None, None);
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    let events = connected.events.clone();
    let driver = connected.driver();

    let requests = fake.requests();
    assert_eq!(requests[0], "host:version");
    assert_eq!(requests[1], "host:devices-l");
    assert!(requests.contains(&format!("host-serial:{SERIAL}:features")));
    assert!(requests.contains(&format!("host:transport:{SERIAL}")));
    assert!(requests.contains(&"shell,v2,raw:screencap -p".to_owned()));
    // Every framed command had its stdin closed: the first picture, and the Android version,
    // which a connect reads while it has the time.
    assert_eq!(fake.recorder.stdin_closed.load(Ordering::SeqCst), 2);
    assert!(requests.contains(&"shell,v2,raw:getprop ro.build.version.sdk".to_owned()));
    for request in &requests {
        assert!(
            !request.starts_with("host:connect")
                && !request.starts_with("host:pair")
                && !request.starts_with("host:disconnect"),
            "{request}"
        );
    }

    let info = driver.info();
    assert_eq!(info.platform, Platform::Android);
    assert_eq!(info.label, "Pixel 8");
    assert_eq!((info.width, info.height), (4, 6));
    assert!(info.has_ui_tree);
    let log = logged(&events);
    assert!(log.contains("version 41"), "{log}");
    assert!(
        log.contains("Pixel 8") && log.contains("4x6") && log.contains("shell_v2"),
        "{log}"
    );
    // Nothing is announced for the capture that connecting itself made.
    assert!(signals(&events).is_empty());
}

fn connect_failure(listing: &'static str, device: Option<&str>) -> (String, Vec<String>) {
    let (png, _) = gradient(2, 2);
    let fake =
        FakeAdb::start(Script::phone(phone_shell(png, 33)).devices(move |_| listing.to_owned()));
    let connected = connect_local(fake.port, device, None);
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    let message = connected.error().message().to_owned();
    (message, fake.requests())
}

#[test]
fn without_a_serial_there_must_be_exactly_one_online_device() {
    let (message, requests) = connect_failure("", None);
    assert!(message.contains("No Android device is online"), "{message}");
    assert!(
        requests.iter().all(|request| request.starts_with("host:")),
        "{requests:?}"
    );
    assert!(!requests
        .iter()
        .any(|request| request.starts_with("host:transport")));

    let (message, _) = connect_failure("ZX1 offline\nZX2 unauthorized\n", None);
    assert!(
        message.contains("ZX1 (offline)") && message.contains("ZX2 (unauthorized)"),
        "{message}"
    );

    let (message, requests) = connect_failure("A1 device\nB2 device\n", None);
    assert!(message.contains("More than one"), "{message}");
    assert!(
        message.contains("A1 (device)") && message.contains("B2 (device)"),
        "{message}"
    );
    assert!(!requests
        .iter()
        .any(|request| request.starts_with("host:transport")));
}

#[test]
fn a_named_serial_must_be_listed_and_authorised() {
    let (message, _) = connect_failure("R5CT unauthorized transport_id:1\n", Some("R5CT"));
    assert!(message.contains("unauthorized"), "{message}");
    assert!(
        message.contains("accept the") && message.contains("USB debugging"),
        "{message}"
    );

    let (message, _) = connect_failure("R5CT offline\n", Some("R5CT"));
    assert!(message.contains("offline"), "{message}");

    let (message, requests) = connect_failure("A1 device\nB2 device\n", Some("ghost"));
    assert!(message.contains("\"ghost\" is not in"), "{message}");
    assert!(message.contains("A1 (device), B2 (device)"), "{message}");
    assert!(!requests
        .iter()
        .any(|request| request.starts_with("host:connect")));

    let (message, requests) = connect_failure("A1 device\n", Some("10.0.0.5:5555"));
    assert!(message.contains("adb connect"), "{message}");
    assert!(
        !requests
            .iter()
            .any(|request| request.starts_with("host:connect")),
        "{requests:?}"
    );
}

#[test]
fn a_named_serial_among_several_is_the_one_driven() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(
        Script::phone(phone_shell(png, 33))
            .devices(|_| "A1 device model:One\nB2 device model:Two\n".to_owned()),
    );
    let connected = connect_local(fake.port, Some("B2"), None);
    let driver = connected.driver();
    assert_eq!(driver.info().label, "Two");
    let requests = fake.requests();
    assert!(requests.contains(&"host:transport:B2".to_owned()));
    assert!(!requests.contains(&"host:transport:A1".to_owned()));
}

#[test]
fn a_serial_that_could_not_be_one_is_refused_before_anything_is_sent() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)));
    let connected = connect_local(fake.port, Some("x; reboot"), None);
    assert!(connected.error().message().contains("not a device serial"));
    assert!(fake.requests().is_empty());
}

// ------------------------------------------------------------ boot waiting

#[test]
fn boot_wait_polls_until_the_device_is_listed_and_has_booted() {
    let (png, _) = gradient(4, 6);
    let boot_checks = Arc::new(AtomicUsize::new(0));
    let counted = boot_checks.clone();
    let fake = FakeAdb::start(
        Script::phone(move |command| match command {
            "getprop sys.boot_completed" => {
                let seen = counted.fetch_add(1, Ordering::SeqCst);
                Reply::out(if seen < 2 { "0\n" } else { "1\n" })
            }
            other => phone_shell(png.clone(), 33)(other),
        })
        .devices(|listed| {
            if listed < 2 {
                String::new()
            } else {
                PHONE.to_owned()
            }
        }),
    );
    let started = Instant::now();
    let connected = connect_local(fake.port, Some(SERIAL), Some(Duration::from_secs(20)));
    let events = connected.events.clone();
    let driver = connected.driver();
    assert!(started.elapsed() < Duration::from_secs(10));
    // Absent twice, then listed but booting twice, then up.
    assert_eq!(boot_checks.load(Ordering::SeqCst), 3);
    let listings = fake
        .requests()
        .iter()
        .filter(|request| *request == "host:devices-l")
        .count();
    assert_eq!(listings, 5);
    assert!(
        logged(&events).contains("waiting up to 20 seconds"),
        "{}",
        logged(&events)
    );
    assert_eq!(driver.info().width, 4);
}

/// This computer's loopback, until the session it serves is stopped.
struct Stoppable {
    inner: LocalConnector,
    stopped: Arc<AtomicBool>,
}

impl Connector for Stoppable {
    fn connect(&self, port: u16, timeout: Duration) -> Result<Connection, String> {
        self.inner.connect(port, timeout)
    }

    fn place_label(&self) -> &str {
        "this computer"
    }

    fn place_key(&self) -> &str {
        "local"
    }

    fn is_local(&self) -> bool {
        true
    }

    fn cancelled(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
}

/// A session stopped while its emulator boots does not keep polling for the rest of the
/// boot wait, beside the session that replaced it.
#[test]
fn boot_wait_ends_when_the_session_is_stopped() {
    let (png, _) = gradient(4, 6);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)).devices(|_| String::new()));
    let stopped = Arc::new(AtomicBool::new(false));
    let stopping = {
        let stopped = stopped.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            stopped.store(true, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    let connected = connect_through(
        Arc::new(Stoppable {
            inner: LocalConnector::new(),
            stopped,
        }),
        fake.port,
        Some(SERIAL),
        Some(Duration::from_secs(60)),
        |_| Err("the test did not expect adb to be started".to_owned()),
    );
    stopping.join().unwrap();
    let message = connected.error().message().to_owned();
    assert!(message.contains("stopped"), "{message}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn boot_wait_gives_up_when_the_device_never_finishes_booting() {
    let (png, _) = gradient(4, 6);
    let fake = FakeAdb::start(Script::phone(move |command| match command {
        "getprop sys.boot_completed" => Reply::out("0\n"),
        other => phone_shell(png.clone(), 33)(other),
    }));
    let started = Instant::now();
    let connected = connect_local(fake.port, None, Some(Duration::from_millis(300)));
    let message = connected.error().message().to_owned();
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(message.contains("still booting"), "{message}");
    assert!(message.contains("Waited"), "{message}");
}

#[test]
fn boot_wait_does_not_wait_for_a_problem_time_cannot_fix() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(
        Script::phone(phone_shell(png, 33)).devices(|_| "A1 device\nB2 device\n".to_owned()),
    );
    let started = Instant::now();
    let connected = connect_local(fake.port, None, Some(Duration::from_secs(30)));
    assert!(connected.error().message().contains("More than one"));
    assert!(started.elapsed() < Duration::from_secs(5));
}

// ----------------------------------------------------------- the adb server

#[test]
fn adb_is_never_run_while_a_server_answers() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)));
    let up = Arc::new(AtomicBool::new(true));
    let connected = connect_through(
        gated(&up, true, "this computer", REFUSED),
        fake.port,
        None,
        None,
        |_| Ok(()),
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    connected.driver();
}

#[test]
fn a_refused_port_on_this_computer_starts_the_server_once_and_retries() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)));
    let up = Arc::new(AtomicBool::new(false));
    let raised = up.clone();
    let connected = connect_through(
        gated(&up, true, "this computer", REFUSED),
        fake.port,
        None,
        None,
        move |_| {
            raised.store(true, Ordering::SeqCst);
            Ok(())
        },
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 1);
    let events = connected.events.clone();
    connected.driver();
    assert!(
        logged(&events).contains("starting one"),
        "{}",
        logged(&events)
    );
}

#[test]
fn a_machine_without_a_server_is_told_to_start_it_there() {
    let up = Arc::new(AtomicBool::new(false));
    let connected = connect_through(
        gated(&up, false, "mac", "connection refused"),
        5037,
        None,
        None,
        |_| Ok(()),
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    let message = connected.error().message().to_owned();
    assert!(
        message.contains("`adb start-server` on that machine"),
        "{message}"
    );
    assert!(message.contains("mac"), "{message}");
}

#[test]
fn adb_is_not_started_for_a_port_that_is_slow_rather_than_refused() {
    let up = Arc::new(AtomicBool::new(false));
    let connected = connect_through(
        gated(
            &up,
            true,
            "this computer",
            "127.0.0.1:5037 on this computer did not answer",
        ),
        5037,
        None,
        None,
        |_| Ok(()),
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    assert!(connected.error().message().contains("not usable"));
}

#[test]
fn a_failed_start_and_a_server_that_stays_silent_are_both_reported() {
    let up = Arc::new(AtomicBool::new(false));
    let connected = connect_through(
        gated(&up, true, "this computer", REFUSED),
        5037,
        None,
        None,
        |_| Err("adb was not found on PATH".to_owned()),
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 1);
    assert!(connected.error().message().contains("adb was not found"));

    let connected = connect_through(
        gated(&up, true, "this computer", REFUSED),
        5037,
        None,
        None,
        |_| Ok(()),
    );
    assert_eq!(connected.starts.load(Ordering::SeqCst), 1);
    assert!(connected
        .error()
        .message()
        .contains("still does not answer"));
}

#[test]
fn a_port_that_is_not_an_adb_server_is_refused_without_running_adb() {
    let impostor = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
    });
    let connected = connect_local(impostor.port, None, None);
    assert_eq!(connected.starts.load(Ordering::SeqCst), 0);
    let message = connected.error().message().to_owned();
    assert!(message.contains("did not answer properly"), "{message}");

    // Right framing, wrong content: the version must be hex.
    let odd = FakeAdb::start(Script {
        version: "hello".to_owned(),
        ..Script::phone(|_| Reply::default())
    });
    let message = connect_local(odd.port, None, None)
        .error()
        .message()
        .to_owned();
    assert!(message.contains("not like an adb server"), "{message}");
}

// ------------------------------------------------------------------ frames

fn warned_capture(features: &str) {
    let (png, rgba) = gradient(4, 6);
    let mut mixed = WARNING.as_bytes().to_vec();
    mixed.extend_from_slice(&png);
    let framed = features == FEATURES_V2;
    let shell = move |command: &str| match command {
        // Over exec the warning is in the one stream, before the picture; over
        // the framed shell it arrives on stderr.
        "screencap -p" if framed => Reply {
            stdout: png.clone(),
            stderr: WARNING.as_bytes().to_vec(),
            exit: 0,
            ..Reply::default()
        },
        "screencap -p" => Reply::out(&mixed),
        _ => Reply::default(),
    };
    let setup = setup(Script::phone(shell).features(features));
    let frame = setup.driver.frame(true).unwrap();
    assert_eq!((frame.width, frame.height), (4, 6));
    assert_eq!(frame.rgba.as_ref(), rgba.as_slice());
    assert_eq!(frame.seq, 2);
    let request = if framed {
        "shell,v2,raw:screencap -p"
    } else {
        "exec:screencap -p"
    };
    assert!(setup.fake.requests().contains(&request.to_owned()));
}

#[test]
fn a_warning_line_before_the_png_over_exec_is_cut_away() {
    warned_capture(FEATURES_OLD);
}

#[test]
fn over_the_framed_shell_stdout_and_stderr_arrive_apart() {
    warned_capture(FEATURES_V2);
}

#[test]
fn frames_count_up_and_a_new_size_is_announced_before_the_frame() {
    let (portrait, _) = gradient(4, 6);
    let (landscape, landscape_rgba) = gradient(6, 4);
    let rotated = Arc::new(AtomicBool::new(false));
    let flag = rotated.clone();
    let setup = setup(Script::phone(move |command| match command {
        "screencap -p" if flag.load(Ordering::SeqCst) => Reply::out(&landscape),
        "screencap -p" => Reply::out(&portrait),
        _ => Reply::default(),
    }));
    let captures = |setup: &Setup| {
        setup
            .fake
            .commands()
            .iter()
            .filter(|command| *command == "screencap -p")
            .count()
    };
    assert_eq!(captures(&setup), 1);

    let again = setup.driver.frame(true).unwrap();
    assert_eq!(again.seq, 2);
    assert_eq!(signals(&setup.events), vec![DriverEvent::Frame]);

    rotated.store(true, Ordering::SeqCst);
    let turned = setup.driver.frame(true).unwrap();
    assert_eq!((turned.width, turned.height, turned.seq), (6, 4, 3));
    assert_eq!(turned.rgba.as_ref(), landscape_rgba.as_slice());
    assert_eq!(
        signals(&setup.events),
        vec![
            DriverEvent::Frame,
            DriverEvent::Resized {
                width: 6,
                height: 4
            },
            DriverEvent::Frame
        ]
    );
    let info = setup.driver.info();
    assert_eq!((info.width, info.height), (6, 4));

    // Asking for the last frame does not touch the device.
    let before = captures(&setup);
    let cached = setup.driver.frame(false).unwrap();
    assert_eq!(cached.seq, 3);
    assert_eq!(captures(&setup), before);
}

#[test]
fn a_screencap_that_prints_no_picture_is_an_error_that_says_what_it_printed() {
    let (png, _) = gradient(2, 2);
    let broken = Arc::new(AtomicBool::new(false));
    let flag = broken.clone();
    let setup = setup(Script::phone(move |command| match command {
        "screencap -p" if flag.load(Ordering::SeqCst) => {
            Reply::failing("screencap: no such display", 1)
        }
        other => phone_shell(png.clone(), 33)(other),
    }));
    broken.store(true, Ordering::SeqCst);
    let error = setup.driver.frame(true).unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        error.message().contains("screencap: no such display"),
        "{error}"
    );
}

#[test]
fn simultaneous_requests_for_a_fresh_frame_share_one_capture() {
    let (png, _) = gradient(4, 6);
    let setup = setup(Script::phone(move |command| match command {
        "screencap -p" => {
            thread::sleep(Duration::from_millis(300));
            Reply::out(&png)
        }
        _ => Reply::default(),
    }));
    let before = setup
        .fake
        .commands()
        .iter()
        .filter(|command| *command == "screencap -p")
        .count();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (driver, barrier) = (setup.driver.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                driver.frame(true).unwrap().seq
            })
        })
        .collect();
    let seqs: Vec<u64> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let captured = setup
        .fake
        .commands()
        .iter()
        .filter(|command| *command == "screencap -p")
        .count()
        - before;
    // Whoever waited for the gate took the picture finished meanwhile; with no
    // sharing at all it would be four.
    assert!(captured < 4, "{captured} captures for 4 requests");
    assert!(seqs.iter().all(|seq| *seq >= 2), "{seqs:?}");
}

/// The viewer's pump began a capture just before the model's tap landed; the model's
/// screenshot, asked for after the tap, must not be handed that picture of the screen before
/// it, however late it finishes.
#[test]
fn a_capture_begun_before_the_request_is_not_handed_to_it() {
    let (png, _) = gradient(4, 6);
    let setup = setup(Script::phone(move |command| match command {
        "screencap -p" => {
            thread::sleep(Duration::from_millis(400));
            Reply::out(&png)
        }
        _ => Reply::default(),
    }));
    let captures = || {
        setup
            .fake
            .commands()
            .iter()
            .filter(|command| *command == "screencap -p")
            .count()
    };
    let before = captures();
    let pump = {
        let driver = setup.driver.clone();
        thread::spawn(move || driver.frame(true).unwrap())
    };
    // The pump's screencap is under way when the model asks.
    let deadline = Instant::now() + Duration::from_secs(5);
    while captures() == before {
        assert!(Instant::now() < deadline, "the pump never captured");
        thread::sleep(Duration::from_millis(5));
    }
    thread::sleep(Duration::from_millis(50));
    let asked = Instant::now();
    let model = setup.driver.frame(true).unwrap();
    let pumped = pump.join().unwrap();
    assert_eq!(captures() - before, 2);
    assert!(model.seq > pumped.seq, "{} vs {}", model.seq, pumped.seq);
    assert!(model.captured_at >= asked);
    assert!(pumped.captured_at < asked);
}

// ------------------------------------------------------------------- input

#[test]
fn gestures_and_keys_become_exactly_these_shell_commands() {
    for features in [FEATURES_V2, FEATURES_OLD] {
        let setup = phone(33, features);
        let driver = &setup.driver;
        let before = setup.fake.commands().len();

        driver.tap(10, 20, 1).unwrap();
        driver.tap(10, 20, 2).unwrap();
        driver.tap(10, 20, 3).unwrap();
        driver.long_press(5, 6, 800).unwrap();
        driver.swipe((1, 2), (3, 4), 300).unwrap();
        driver.key(&Key::Named(NamedKey::Enter), None).unwrap();
        driver.key(&Key::Named(NamedKey::Back), Some(true)).unwrap();
        // A phone cannot hold a key: the release is nothing.
        driver
            .key(&Key::Named(NamedKey::Back), Some(false))
            .unwrap();
        driver.key(&Key::Char('a'), None).unwrap();
        driver.key(&Key::Named(NamedKey::Escape), None).unwrap();
        driver
            .chord(&parse_chord("ctrl+c", Platform::Android).unwrap())
            .unwrap();
        // A chord without modifiers is just a key; "Home" is the home screen.
        driver
            .chord(&parse_chord("Home", Platform::Android).unwrap())
            .unwrap();
        driver
            .chord(&parse_chord("ctrl+shift+t", Platform::Android).unwrap())
            .unwrap();

        let commands = setup.fake.commands();
        assert_eq!(
            texts(&commands[before..]),
            vec![
                "input tap 10 20",
                "input tap 10 20; input tap 10 20",
                "input tap 10 20; input tap 10 20; input tap 10 20",
                "input swipe 5 6 5 6 800",
                "input swipe 1 2 3 4 300",
                "input keyevent 66",
                "input keyevent 4",
                "input keyevent 29",
                "input keyevent 4",
                "input keycombination 113 31",
                "input keyevent 3",
                "input keycombination 113 59 48",
            ],
            "{features}"
        );
    }
}

#[test]
fn keys_with_no_android_code_are_refused_and_send_nothing() {
    let setup = phone(33, FEATURES_V2);
    let before = setup.fake.commands().len();
    for key in [Key::Char('?'), Key::Char('A'), Key::Char('\u{e9}')] {
        let error = setup.driver.key(&key, None).unwrap_err();
        assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
        assert!(error.message().contains("type action"), "{error}");
    }
    let error = setup.driver.pointer(1, 2, buttons::LEFT).unwrap_err();
    assert!(matches!(error, DriverError::Unsupported(_)));
    assert_eq!(setup.fake.commands().len(), before);
}

#[test]
fn key_combinations_need_android_12() {
    let setup = phone(30, FEATURES_V2);
    let chord = parse_chord("ctrl+c", Platform::Android).unwrap();
    let error = setup.driver.chord(&chord).unwrap_err();
    assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
    assert!(
        error.message().contains("Android 12") && error.message().contains("API 30"),
        "{error}"
    );
    // The answer is remembered; plain keys still work.
    let error = setup.driver.chord(&chord).unwrap_err();
    assert!(matches!(error, DriverError::Unsupported(_)));
    setup
        .driver
        .key(&Key::Named(NamedKey::Enter), None)
        .unwrap();
    let commands = setup.fake.commands();
    assert_eq!(
        commands
            .iter()
            .filter(|command| command.starts_with("getprop"))
            .count(),
        1
    );
    assert!(!commands
        .iter()
        .any(|command| command.contains("keycombination")));
}

#[test]
fn typed_text_reaches_the_device_as_one_quoted_word() {
    let setup = phone(33, FEATURES_V2);
    let cases: &[(&str, &str)] = &[
        ("hello world", "input text 'hello%sworld'"),
        ("a;b", "input text 'a;b'"),
        ("a$(b)c", "input text 'a$(b)c'"),
        ("a`b`c", "input text 'a`b`c'"),
        ("a|b&c", "input text 'a|b&c'"),
        ("a<b>c", "input text 'a<b>c'"),
        ("say \"hi\"", r#"input text 'say%s"hi"'"#),
        ("it's", r"input text 'it'\''s'"),
        (r"back\slash", r"input text 'back\slash'"),
        ("100%s", "input text '100%'; input text 's'"),
        (
            "$(reboot); echo 'hi' %s",
            r"input text '$(reboot);%secho%s'\''hi'\''%s%'; input text 's'",
        ),
        (
            "line one\nline two",
            "input text 'line%sone'; input keyevent 66; input text 'line%stwo'",
        ),
    ];
    for (text, expected) in cases {
        let before = setup.fake.commands().len();
        setup.driver.text(text).unwrap();
        let commands = setup.fake.commands();
        assert_eq!(texts(&commands[before..]), vec![*expected], "{text:?}");
    }
}

#[test]
fn text_the_device_cannot_type_is_refused_before_anything_is_sent() {
    let setup = phone(33, FEATURES_OLD);
    let before = setup.fake.commands().len();
    let error = setup.driver.text("caf\u{e9}").unwrap_err();
    assert!(matches!(error, DriverError::Unsupported(_)), "{error:?}");
    assert!(
        error.message().contains("adb can only type ASCII text"),
        "{error}"
    );
    assert!(error.message().contains("paste"), "{error}");
    let error = setup.driver.text(&"x".repeat(6000)).unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    setup.driver.text("").unwrap();
    assert_eq!(setup.fake.commands().len(), before);
}

#[test]
fn a_command_the_device_rejects_is_reported_with_what_it_said() {
    let (png, _) = gradient(2, 2);
    let framed = Script::phone(move |command| {
        match command {
        "input tap 9 9" => Reply::failing(
            "java.lang.SecurityException: Injecting input events requires INJECT_EVENTS\n\tat android.os.Foo",
            255,
        ),
        other => phone_shell(png.clone(), 33)(other),
    }
    });
    let setup = setup(framed);
    let error = setup.driver.tap(9, 9, 1).unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(error.message().contains("exit status 255"), "{error}");
    assert!(error.message().contains("INJECT_EVENTS"), "{error}");
    assert!(!error.message().contains('\n'), "{error}");
    setup.driver.tap(1, 1, 1).unwrap();

    // Over exec there is no exit status; an exception in the output says it.
    let (png, _) = gradient(2, 2);
    let raw = Script::phone(move |command| match command {
        "input tap 9 9" => Reply::out(
            "Exception occurred while executing 'tap':\njava.lang.IllegalArgumentException\n",
        ),
        other => phone_shell(png.clone(), 33)(other),
    })
    .features(FEATURES_OLD);
    let setup = self::setup(raw);
    let error = setup.driver.tap(9, 9, 1).unwrap_err();
    assert!(
        error.message().contains("rejected") && error.message().contains("Exception occurred"),
        "{error}"
    );
    setup.driver.tap(1, 1, 1).unwrap();
}

// ---------------------------------------------------------------- ui tree

fn dump_script(shell: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Setup {
    let (png, _) = gradient(2, 2);
    setup(Script::phone(move |command| {
        if command.starts_with("uiautomator") {
            shell(command)
        } else {
            phone_shell(png.clone(), 33)(command)
        }
    }))
}

fn dump_commands(setup: &Setup) -> Vec<String> {
    setup
        .fake
        .commands()
        .into_iter()
        .filter(|command| command.starts_with("uiautomator"))
        .collect()
}

#[test]
fn the_ui_tree_is_parsed_from_the_tty_dump_with_its_trailer_cut_off() {
    let setup = dump_script(|command| {
        assert_eq!(command, DUMP_TTY);
        Reply::out(format!("{DUMP_XML}\nUI hierchary dumped to: /dev/tty\n"))
    });
    let root = setup.driver.ui_tree().unwrap().expect("a tree");
    assert_eq!(root.role, "FrameLayout");
    assert_eq!(root.children.len(), 2);
    assert_eq!(root.children[0].name, "Tom & Jerry");
    assert_eq!(root.children[0].id.as_deref(), Some("com.example:id/title"));
    assert_eq!(root.children[1].role, "ImageButton");
    assert_eq!(root.children[1].name, "Back \"up\"");
    assert_eq!(
        root.children[1].bounds,
        Some(Rect {
            x: 0,
            y: 60,
            width: 120,
            height: 120
        })
    );
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY]);
}

#[test]
fn a_shell_without_a_tty_falls_back_to_a_file_and_remembers_it() {
    let setup = dump_script(|command| match command {
        DUMP_TTY => {
            Reply::out("java.io.FileNotFoundException: /dev/tty (No such device or address)\n")
        }
        DUMP_FILE => Reply::out(DUMP_XML),
        other => panic!("{other}"),
    });
    assert!(setup.driver.ui_tree().unwrap().is_some());
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY, DUMP_FILE]);
    assert!(setup.driver.ui_tree().unwrap().is_some());
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY, DUMP_FILE, DUMP_FILE]);
}

#[test]
fn a_dump_the_device_was_not_ready_for_is_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let setup = dump_script(move |_| {
        if counted.fetch_add(1, Ordering::SeqCst) < 2 {
            Reply::out("ERROR: null root node returned by UiTestAutomationBridge.\n")
        } else {
            Reply::out(DUMP_XML)
        }
    });
    assert!(setup.driver.ui_tree().unwrap().is_some());
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY; 3]);
}

#[test]
fn a_screen_that_never_settles_gives_up_after_three_attempts() {
    // Over the framed shell the complaint is on stderr.
    let setup = dump_script(|_| Reply::failing("ERROR: could not get idle state.\n", 1));
    let error = setup.driver.ui_tree().unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        error
            .message()
            .contains("uiautomator could not read the screen"),
        "{error}"
    );
    assert!(
        error.message().contains("could not get idle state"),
        "{error}"
    );
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY; 3]);
}

#[test]
fn a_dump_that_is_not_xml_in_either_mode_is_an_error() {
    let setup = dump_script(|_| Reply::out("something unrelated\n"));
    let error = setup.driver.ui_tree().unwrap_err();
    assert!(error.message().contains("something unrelated"), "{error}");
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY, DUMP_FILE]);
}

#[test]
fn a_dump_read_within_a_timeout_stops_at_it_instead_of_at_the_dumps_own_limit() {
    // A dump that outlasts what the call has left ends at that, not after thirty seconds.
    let slow = dump_script(|_| {
        thread::sleep(Duration::from_secs(2));
        Reply::out(DUMP_XML)
    });
    let started = Instant::now();
    let error = slow
        .driver
        .ui_tree_within(Duration::from_millis(800))
        .unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1800),
        "{:?}",
        started.elapsed()
    );

    // With too little left to run a dump at all, none is started.
    let setup = dump_script(|_| Reply::out(DUMP_XML));
    let error = setup
        .driver
        .ui_tree_within(Duration::from_millis(100))
        .unwrap_err();
    assert!(
        error.message().contains("did not finish reading the screen"),
        "{error}"
    );
    assert!(dump_commands(&setup).is_empty());

    // With the time it needs, it reads the tree as ui_tree does.
    let root = setup
        .driver
        .ui_tree_within(Duration::from_secs(10))
        .unwrap()
        .expect("a tree");
    assert_eq!(root.children.len(), 2);
    assert_eq!(dump_commands(&setup), vec![DUMP_TTY]);
}

// ----------------------------------------------------- disconnect and close

#[test]
fn a_vanished_device_is_announced_once_per_outage() {
    let (png, _) = gradient(2, 2);
    let gone = Arc::new(AtomicBool::new(false));
    let flag = gone.clone();
    let setup = setup(Script::phone(phone_shell(png, 33)).devices(move |_| {
        if flag.load(Ordering::SeqCst) {
            String::new()
        } else {
            PHONE.to_owned()
        }
    }));
    setup.driver.tap(1, 1, 1).unwrap();
    assert!(signals(&setup.events).is_empty());

    gone.store(true, Ordering::SeqCst);
    let error = setup.driver.tap(1, 1, 1).unwrap_err();
    assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
    assert!(
        error.message().contains(SERIAL) && error.message().contains("not found"),
        "{error}"
    );
    let error = setup.driver.frame(true).unwrap_err();
    assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
    let disconnects = |events: &Events| {
        signals(events)
            .iter()
            .filter(|event| matches!(event, DriverEvent::Disconnected(_)))
            .count()
    };
    assert_eq!(disconnects(&setup.events), 1);

    // It came back: the driver works again, and the next outage is announced.
    gone.store(false, Ordering::SeqCst);
    setup.driver.tap(1, 1, 1).unwrap();
    gone.store(true, Ordering::SeqCst);
    assert!(setup.driver.tap(1, 1, 1).is_err());
    assert_eq!(disconnects(&setup.events), 2);
}

#[test]
fn a_stream_that_breaks_is_a_disconnect_only_if_the_device_is_gone() {
    let (png, _) = gradient(2, 2);
    let script = Script::phone(phone_shell(png, 33));
    let hang_up = script.hang_up.clone();
    let setup = setup(script);

    // The connection breaks but the device is still listed: just a failure.
    hang_up.store(true, Ordering::SeqCst);
    let error = setup.driver.tap(1, 1, 1).unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(signals(&setup.events).is_empty());
}

#[test]
fn a_stream_that_breaks_while_the_device_has_left_the_list_is_a_disconnect() {
    let (png, _) = gradient(2, 2);
    // Listed when the client connects; gone from every listing after that. The
    // transport switch still works (the fake judges by the listing the client
    // last saw), then the stream is cut, and the check that follows finds the
    // device missing.
    let script = Script::phone(phone_shell(png, 33)).devices(|listed| {
        if listed == 0 {
            PHONE.to_owned()
        } else {
            String::new()
        }
    });
    let hang_up = script.hang_up.clone();
    let setup = setup(script);
    hang_up.store(true, Ordering::SeqCst);
    let error = setup.driver.tap(1, 1, 1).unwrap_err();
    assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
    assert!(
        error.message().contains("no longer in adb's device list"),
        "{error}"
    );
    assert_eq!(
        signals(&setup.events)
            .iter()
            .filter(|event| matches!(event, DriverEvent::Disconnected(_)))
            .count(),
        1
    );
}

#[test]
fn a_server_that_went_away_is_a_disconnect() {
    let setup = phone(33, FEATURES_V2);
    let Setup {
        fake,
        driver,
        events,
    } = setup;
    drop(fake);
    let error = driver.tap(1, 1, 1).unwrap_err();
    assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
    assert!(error.message().contains("adb server"), "{error}");
    assert_eq!(
        signals(&events)
            .iter()
            .filter(|event| matches!(event, DriverEvent::Disconnected(_)))
            .count(),
        1
    );
}

#[test]
fn a_closed_driver_answers_disconnected_and_sends_nothing() {
    let setup = phone(33, FEATURES_V2);
    let before = setup.fake.requests().len();
    setup.driver.close();
    setup.driver.close();
    let results = [
        setup.driver.frame(true).map(drop),
        setup.driver.frame(false).map(drop),
        setup.driver.tap(1, 1, 1),
        setup.driver.long_press(1, 1, 10),
        setup.driver.swipe((1, 1), (2, 2), 10),
        setup.driver.key(&Key::Named(NamedKey::Enter), None),
        setup.driver.key(&Key::Named(NamedKey::Enter), Some(false)),
        setup
            .driver
            .chord(&parse_chord("ctrl+c", Platform::Android).unwrap()),
        setup.driver.text("x"),
        setup.driver.ui_tree().map(drop),
    ];
    for result in results {
        assert!(
            matches!(result, Err(DriverError::Disconnected(_))),
            "{result:?}"
        );
    }
    assert_eq!(setup.fake.requests().len(), before);
    assert!(signals(&setup.events).is_empty());
    // What was known stays known.
    assert_eq!(setup.driver.info().width, 4);
}

// ------------------------------------------------------- the wire, bounded

fn local_server(port: u16) -> Server {
    Server::new(Arc::new(LocalConnector::new()), port)
}

#[test]
fn command_output_is_bounded_by_the_limit() {
    let fake = FakeAdb::start(Script::phone(|_| Reply::out(vec![b'x'; 5000])));
    let server = local_server(fake.port);
    for framed in [false, true] {
        let error = server
            .shell(SERIAL, "big", framed, 1000, Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(error, WireError::Protocol(_)), "{error:?}");
        assert!(
            error.message().contains("more than 1000 bytes"),
            "{error:?}"
        );
        let all = server
            .shell(SERIAL, "big", framed, 5000, Duration::from_secs(5))
            .unwrap();
        assert_eq!(all.stdout.len(), 5000);
    }
}

#[test]
fn an_answer_that_never_comes_ends_at_the_deadline() {
    let silent = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
        thread::sleep(Duration::from_secs(4));
    });
    let started = Instant::now();
    let error = local_server(silent.port)
        .query("host:version", Duration::from_millis(300))
        .unwrap_err();
    assert!(matches!(error, WireError::Io(_)), "{error:?}");
    assert!(error.message().contains("in time"), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn fail_replies_and_garbled_framing_are_told_apart() {
    let refusing = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
        let _ = write_fail(&mut stream, "no luck");
    });
    let error = local_server(refusing.port)
        .query("host:anything", Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(error, WireError::Refused("no luck".to_owned()));

    let garbled = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
        let _ = stream.write_all(b"OKAYzzzz");
    });
    let error = local_server(garbled.port)
        .query("host:anything", Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Protocol(_)), "{error:?}");

    let wrong = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
        let _ = stream.write_all(b"HTTP/1.1");
    });
    let error = local_server(wrong.port)
        .query("host:anything", Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Protocol(_)), "{error:?}");

    let hung_up = FakeAdb::start_raw(|mut stream, _| {
        let _ = read_request(&mut stream);
    });
    let error = local_server(hung_up.port)
        .query("host:anything", Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Io(_)), "{error:?}");
}

/// A server that switches to a device and then does `then` with the stream.
fn device_stream(then: impl Fn(&mut TcpStream) + Send + Sync + 'static) -> FakeAdb {
    FakeAdb::start_raw(move |mut stream, _| {
        if read_request(&mut stream).is_err() {
            return;
        }
        let _ = stream.write_all(b"OKAY");
        if read_request(&mut stream).is_err() {
            return;
        }
        let _ = stream.write_all(b"OKAY");
        // The close-stdin packet the framed shell starts with.
        let mut close_stdin = [0u8; 5];
        let _ = stream.read_exact(&mut close_stdin);
        then(&mut stream);
        let _ = stream.shutdown(Shutdown::Both);
    })
}

#[test]
fn a_hostile_shell_stream_cannot_make_us_allocate_or_hang() {
    let oversized = device_stream(|stream| {
        let _ = stream.write_all(&[1, 0xff, 0xff, 0xff, 0xff]);
    });
    let error = local_server(oversized.port)
        .shell(SERIAL, "x", true, 1000, Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Protocol(_)), "{error:?}");
    assert!(error.message().contains("oversized"), "{error:?}");

    // A stream that ends without an exit packet keeps what it sent.
    let cut_short = device_stream(|stream| {
        let _ = write_packet(stream, 1, b"hi");
        let _ = write_packet(stream, 9, b"an id nobody knows");
        let _ = write_packet(stream, 2, b"warn");
    });
    let output = local_server(cut_short.port)
        .shell(SERIAL, "x", true, 1000, Duration::from_secs(5))
        .unwrap();
    assert_eq!(output.stdout, b"hi");
    assert_eq!(output.stderr, b"warn");
    assert_eq!(output.exit, None);

    // A packet cut in the middle is an error, not a short read taken as complete.
    let torn = device_stream(|stream| {
        let _ = stream.write_all(&[1, 10, 0, 0, 0, b'a', b'b']);
    });
    let error = local_server(torn.port)
        .shell(SERIAL, "x", true, 1000, Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Io(_)), "{error:?}");
}

#[test]
fn stderr_kept_from_a_noisy_command_is_capped() {
    let noisy = device_stream(|stream| {
        for _ in 0..200 {
            let _ = write_packet(stream, 2, &[b'e'; 8192]);
        }
        let _ = write_packet(stream, 1, b"out");
        let _ = write_packet(stream, 3, &[7]);
    });
    let output = local_server(noisy.port)
        .shell(SERIAL, "x", true, 1000, Duration::from_secs(10))
        .unwrap();
    assert_eq!(output.stderr.len(), 64 * 1024);
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.exit, Some(7));
}

// ----------------------------------------------------------- small helpers

#[test]
fn refusals_are_recognised_by_their_words() {
    assert!(is_refused(
        "nothing is listening on 127.0.0.1:5037 on this computer"
    ));
    assert!(is_refused("Connection refused (os error 111)"));
    assert!(!is_refused(
        "127.0.0.1:5037 on this computer did not answer"
    ));
}

#[test]
fn a_missing_device_is_recognised_in_the_servers_words() {
    assert!(device_is_missing("device 'R5CT' not found"));
    assert!(device_is_missing("device offline"));
    assert!(device_is_missing(
        "device unauthorized.\nThis adbd's $ADB_VENDOR_KEYS is not set"
    ));
    assert!(device_is_missing("no devices/emulators found"));
    assert!(!device_is_missing("more than one device/emulator"));
}

#[test]
fn input_outcomes_are_read_from_the_exit_status_or_the_output() {
    let ok = ShellOutput {
        exit: Some(0),
        ..ShellOutput::default()
    };
    assert!(check_input(&ok).is_ok());
    let bad = ShellOutput {
        exit: Some(1),
        stderr: b"Error: nope\nmore".to_vec(),
        ..ShellOutput::default()
    };
    let error = check_input(&bad).unwrap_err();
    assert!(
        error.message().contains("exit status 1") && error.message().contains("Error: nope"),
        "{error}"
    );
    assert!(!error.message().contains("more"), "{error}");
    // No status (exec): silence is success, a stack trace is not.
    assert!(check_input(&ShellOutput::default()).is_ok());
    assert!(check_input(&ShellOutput::out(b"INFO: fine\n")).is_ok());
    assert!(check_input(&ShellOutput::out(b"java.lang.RuntimeException: boom\n")).is_err());
}

impl ShellOutput {
    fn out(bytes: &[u8]) -> Self {
        Self {
            stdout: bytes.to_vec(),
            ..Self::default()
        }
    }
}

// --------------------------------------------- the Android tools' operations

fn android(setup: &Setup) -> &dyn AndroidControl {
    setup
        .driver
        .android()
        .expect("an adb driver answers the Android tools")
}

/// No request of these tests ever connects, pairs or disconnects a device: adb's own `connect`
/// is the user's to run.
fn never_connects(fake: &FakeAdb) {
    for request in fake.requests() {
        assert!(
            !request.starts_with("host:connect")
                && !request.starts_with("host:pair")
                && !request.starts_with("host:disconnect"),
            "{request}"
        );
    }
}

#[test]
fn a_command_that_fails_is_data_with_its_exit_status() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(move |command| match command {
        "ls /nope; echo done" => Reply {
            stdout: b"done\n".to_vec(),
            stderr: b"ls: /nope: No such file or directory\n".to_vec(),
            exit: 1,
            ..Reply::default()
        },
        other => phone_shell(png.clone(), 33)(other),
    }));
    let result = android(&setup)
        .shell("ls /nope; echo done", 1024, Duration::from_secs(5))
        .unwrap();
    assert_eq!(
        result,
        ShellResult {
            stdout: b"done\n".to_vec(),
            stderr: b"ls: /nope: No such file or directory\n".to_vec(),
            exit: Some(1),
            truncated: false,
            timed_out: false,
        }
    );
    assert!(setup
        .fake
        .requests()
        .contains(&"shell,v2,raw:ls /nope; echo done".to_owned()));
    // The version is asked once, and shared with the key combinations' check.
    assert_eq!(android(&setup).sdk().unwrap(), 33);
    assert_eq!(android(&setup).sdk().unwrap(), 33);
    assert_eq!(
        setup
            .fake
            .commands()
            .iter()
            .filter(|command| *command == "getprop ro.build.version.sdk")
            .count(),
        1
    );
    assert!(signals(&setup.events).is_empty());
    never_connects(&setup.fake);
}

#[test]
fn without_a_framed_shell_the_streams_arrive_mixed_and_without_a_status() {
    let (png, _) = gradient(2, 2);
    let setup = setup(
        Script::phone(move |command| match command {
            "pm path com.x.y" => Reply::failing("Error: package not found\n", 1),
            other => phone_shell(png.clone(), 30)(other),
        })
        .features(FEATURES_OLD),
    );
    let result = android(&setup)
        .shell("pm path com.x.y", 1024, Duration::from_secs(5))
        .unwrap();
    assert_eq!(result.stdout, b"Error: package not found\n");
    assert!(result.stderr.is_empty());
    assert_eq!(result.exit, None);
    assert!(!result.truncated);
    assert!(setup
        .fake
        .requests()
        .contains(&"exec:pm path com.x.y".to_owned()));
}

#[test]
fn output_past_the_limit_is_cut_and_the_status_still_arrives() {
    for features in [FEATURES_V2, FEATURES_OLD] {
        let (png, _) = gradient(2, 2);
        let setup = setup(
            Script::phone(move |command| match command {
                "big" => Reply {
                    stdout: vec![b'x'; 50_000],
                    stderr: vec![b'e'; 3_000],
                    exit: 4,
                    ..Reply::default()
                },
                other => phone_shell(png.clone(), 33)(other),
            })
            .features(features),
        );
        let result = android(&setup)
            .shell("big", 1000, Duration::from_secs(5))
            .unwrap();
        assert!(result.truncated, "{features}");
        assert_eq!(result.stdout.len(), 1000, "{features}");
        if features == FEATURES_V2 {
            assert_eq!(result.stderr, vec![b'e'; 1000]);
            assert_eq!(result.stdout, vec![b'x'; 1000]);
            assert_eq!(result.exit, Some(4));
        } else {
            // Over exec, stderr came first in the one stream.
            assert_eq!(result.stdout, vec![b'e'; 1000]);
            assert_eq!(result.exit, None);
        }
        let all = android(&setup)
            .shell("big", 60_000, Duration::from_secs(5))
            .unwrap();
        assert!(!all.truncated, "{features}");
    }
}

#[test]
fn a_command_that_does_not_end_in_time_is_stopped_not_a_disconnect() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(move |command| match command {
        "sleep 3" => {
            thread::sleep(Duration::from_secs(2));
            Reply::default()
        }
        other => phone_shell(png.clone(), 33)(other),
    }));
    let lists = setup.fake.recorder.device_lists.load(Ordering::SeqCst);
    let started = Instant::now();
    let result = android(&setup)
        .shell("sleep 3", 1000, Duration::from_millis(300))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert!(result.timed_out);
    assert_eq!(result.exit, None);
    assert!(signals(&setup.events).is_empty());
    // Nothing broke, so the device list was not asked why.
    assert_eq!(
        setup.fake.recorder.device_lists.load(Ordering::SeqCst),
        lists
    );
}

/// Why a call broke is asked of the device list only within what is left of the call; a list
/// that does not come in that time says nothing, so the device is not declared gone for it.
#[test]
fn a_broken_call_is_explained_within_its_own_time() {
    let (png, _) = gradient(2, 2);
    let script = Script::phone(phone_shell(png, 33)).devices(|listed| {
        if listed >= 1 {
            thread::sleep(Duration::from_secs(3));
        }
        PHONE.to_owned()
    });
    let hang_up = script.hang_up.clone();
    let setup = setup(script);
    hang_up.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let error = android(&setup)
        .shell("ls", 1000, Duration::from_millis(800))
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "{:?}",
        started.elapsed()
    );
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(signals(&setup.events).is_empty());
}

#[test]
fn a_framed_stream_that_ends_without_its_status_is_an_error() {
    let (png, _) = gradient(2, 2);
    // Listed for the connect and the first check; gone from the list after that.
    let setup = setup(
        Script::phone(move |command| match command {
            "cut" => Reply {
                stdout: b"half".to_vec(),
                no_exit: true,
                ..Reply::default()
            },
            other => phone_shell(png.clone(), 33)(other),
        })
        .devices(|listed| {
            if listed < 2 {
                PHONE.to_owned()
            } else {
                String::new()
            }
        }),
    );
    // Not a command without a status ("no framed shell"): the device or the server cut it.
    let error = android(&setup)
        .shell("cut", 1000, Duration::from_secs(5))
        .unwrap_err();
    assert_eq!(
        error,
        DriverError::Failed(
            "the device ended the command's output without its exit status".to_owned()
        )
    );
    assert!(signals(&setup.events).is_empty());
    let error = android(&setup)
        .shell("cut", 1000, Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, DriverError::Disconnected(_)), "{error:?}");
}

#[test]
fn a_capped_read_keeps_what_it_has_when_a_cut_command_never_ends() {
    let endless = device_stream(|stream| {
        for _ in 0..20 {
            let _ = write_packet(stream, 1, &[b'y'; 4096]);
        }
        thread::sleep(Duration::from_secs(3));
    });
    let capped = local_server(endless.port)
        .shell_capped(SERIAL, "yes", true, 1000, Duration::from_millis(400))
        .unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.output.stdout.len(), 1000);
    assert_eq!(capped.output.exit, None);
    assert!(capped.unfinished);

    // One that printed nothing and never ended is stopped the same way: nothing kept.
    let silent = device_stream(|_| thread::sleep(Duration::from_secs(3)));
    let started = Instant::now();
    let stopped = local_server(silent.port)
        .shell_capped(SERIAL, "sleep 9", true, 1000, Duration::from_millis(400))
        .unwrap();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert!(stopped.unfinished && !stopped.truncated);
    assert!(stopped.output.stdout.is_empty());
    assert_eq!(stopped.output.exit, None);

    let oversized = device_stream(|stream| {
        let _ = stream.write_all(&[2, 0xff, 0xff, 0xff, 0x7f]);
    });
    let error = local_server(oversized.port)
        .shell_capped(SERIAL, "x", true, 1000, Duration::from_secs(5))
        .unwrap_err();
    assert!(matches!(error, WireError::Protocol(_)), "{error:?}");
}

fn bodies(fake: &FakeAdb) -> Vec<Body> {
    fake.recorder.bodies.lock().unwrap().clone()
}

#[test]
fn an_apk_streams_into_cmd_package_install_and_pms_line_is_the_answer() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(phone_shell(png, 33)).install(|command, _| {
        if command.contains(" -d ") {
            "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]\n".to_owned()
        } else {
            "Success\n".to_owned()
        }
    }));
    let apk: Vec<u8> = (0..300_000u32)
        .map(|n| (n.wrapping_mul(7) % 256) as u8)
        .collect();
    let answer = android(&setup)
        .install(
            &mut apk.as_slice(),
            apk.len() as u64,
            &["-r", "-t", "-g"],
            Duration::from_secs(10),
            &|| false,
        )
        .unwrap();
    assert_eq!(answer, "Success");
    let sent = bodies(&setup.fake);
    assert_eq!(sent[0].command, "cmd package install -r -t -g -S 300000");
    assert!(sent[0].whole);
    assert_eq!(sent[0].bytes, apk);
    assert!(setup
        .fake
        .requests()
        .contains(&format!("host:transport:{SERIAL}")));
    assert!(setup
        .fake
        .requests()
        .contains(&"exec:cmd package install -r -t -g -S 300000".to_owned()));

    let answer = android(&setup)
        .install(
            &mut apk.as_slice(),
            apk.len() as u64,
            &["-r", "-t", "-d"],
            Duration::from_secs(10),
            &|| false,
        )
        .unwrap();
    assert_eq!(answer, "Failure [INSTALL_FAILED_VERSION_DOWNGRADE]");
    // An APK shorter than the size it was announced with is this side's error.
    let error = android(&setup)
        .install(
            &mut &apk[..10],
            20,
            &["-t"],
            Duration::from_secs(10),
            &|| false,
        )
        .unwrap_err();
    assert!(
        error.message().contains("ended after 10 of 20 bytes"),
        "{error}"
    );
    assert!(signals(&setup.events).is_empty());
    never_connects(&setup.fake);
}

#[test]
fn a_cancelled_install_drops_the_connection_mid_stream() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(phone_shell(png, 33)));
    let apk = vec![0x5au8; 1024 * 1024];
    let checks = AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, Ordering::SeqCst) >= 2;
    let error = android(&setup)
        .install(
            &mut apk.as_slice(),
            apk.len() as u64,
            &["-r", "-t"],
            Duration::from_secs(10),
            &cancelled,
        )
        .unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        error
            .message()
            .contains("cancelled after 131072 of 1048576 bytes"),
        "{error}"
    );
    // The fake saw the stream end before the size: the connection was dropped, not held.
    let deadline = Instant::now() + Duration::from_secs(5);
    while bodies(&setup.fake).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the fake never saw the stream end"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let sent = &bodies(&setup.fake)[0];
    assert!(!sent.whole);
    assert_eq!(sent.bytes.len(), 131_072);
    assert!(signals(&setup.events).is_empty());
}

#[test]
fn an_install_that_gets_no_answer_in_time_fails_without_a_disconnect() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(phone_shell(png, 33)).install(|_, _| {
        thread::sleep(Duration::from_secs(3));
        "Success\n".to_owned()
    }));
    let started = Instant::now();
    let error = android(&setup)
        .install(
            &mut &b"PK\x03\x04"[..],
            4,
            &["-t"],
            Duration::from_millis(500),
            &|| false,
        )
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        error.message().contains("the whole APK was sent")
            && error.message().contains("look with screen_app info"),
        "{error}"
    );
    assert!(signals(&setup.events).is_empty());
}

#[test]
fn an_install_cancelled_while_pm_works_ends_at_once() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(phone_shell(png, 33)).install(|_, _| {
        thread::sleep(Duration::from_secs(4));
        "Success\n".to_owned()
    }));
    let apk = vec![0x5au8; 100_000];
    // Not cancelled while the APK is sent (two pieces); cancelled once pm has it whole.
    let checks = AtomicUsize::new(0);
    let cancelled = || checks.fetch_add(1, Ordering::SeqCst) >= 2;
    let started = Instant::now();
    let error = android(&setup)
        .install(
            &mut apk.as_slice(),
            apk.len() as u64,
            &["-R", "-t"],
            Duration::from_secs(30),
            &cancelled,
        )
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        error,
        DriverError::Failed("the install was cancelled after the whole APK was sent; the device may still install it, so look with screen_app info before installing again".to_owned())
    );
    // pm had the whole APK (the fake records it once its own answer is written).
    let deadline = Instant::now() + Duration::from_secs(8);
    while bodies(&setup.fake).is_empty() {
        assert!(Instant::now() < deadline, "the fake never finished");
        thread::sleep(Duration::from_millis(20));
    }
    let sent = bodies(&setup.fake);
    assert!(sent[0].whole);
    assert_eq!(sent[0].command, "cmd package install -R -t -S 100000");
    assert!(signals(&setup.events).is_empty());
}

/// A dial the caller's time cuts short is that call running out of time, not a server that is
/// gone; and the deadline counts from before the dial.
#[test]
fn the_dial_counts_against_the_callers_time() {
    struct Slow {
        dials: Mutex<Vec<Duration>>,
        delay: Duration,
    }
    impl Connector for Slow {
        fn connect(&self, _port: u16, timeout: Duration) -> Result<Connection, String> {
            self.dials.lock().unwrap().push(timeout);
            thread::sleep(self.delay.min(timeout));
            Err("127.0.0.1:5037 on build-box did not answer in time".to_owned())
        }
        fn place_label(&self) -> &str {
            "build-box"
        }
        fn place_key(&self) -> &str {
            "ssh:test"
        }
        fn is_local(&self) -> bool {
            false
        }
    }
    let slow = Arc::new(Slow {
        dials: Mutex::default(),
        delay: Duration::from_secs(10),
    });
    let server = Server::new(slow.clone(), 5037);
    let started = Instant::now();
    let error = server
        .query("host:devices-l", Duration::from_millis(300))
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(
        error,
        WireError::Io("the adb server did not answer in time (connecting)".to_owned())
    );
    assert_eq!(slow.dials.lock().unwrap()[0], Duration::from_millis(300));
    // A refusal is the server's absence whatever the time.
    let refusing = Arc::new(Slow {
        dials: Mutex::default(),
        delay: Duration::ZERO,
    });
    let error = Server::new(refusing.clone(), 5037)
        .query("host:devices-l", Duration::from_millis(300))
        .unwrap_err();
    assert!(matches!(error, WireError::Unreachable(_)), "{error:?}");
    let error = Server::new(refusing.clone(), 5037)
        .query("host:devices-l", HOST_TIMEOUT)
        .unwrap_err();
    assert!(matches!(error, WireError::Unreachable(_)), "{error:?}");
    assert_eq!(refusing.dials.lock().unwrap()[1], wire::CONNECT_TIMEOUT);
}

/// Over a relay an upload can lose bytes while the link reconnects without any write failing:
/// an install that then gets no answer says that may be why.
#[test]
fn an_unanswered_install_over_a_relay_says_the_link_may_have_lost_part_of_it() {
    let (png, _) = gradient(2, 2);
    let fake = FakeAdb::start(Script::phone(phone_shell(png, 33)).install(|_, _| {
        thread::sleep(Duration::from_secs(3));
        "Success\n".to_owned()
    }));
    let up = Arc::new(AtomicBool::new(true));
    let driver = connect_through(
        gated(&up, false, "build-box", REFUSED),
        fake.port,
        None,
        None,
        |_| Err("not here".to_owned()),
    )
    .driver();
    let lists = fake.recorder.device_lists.load(Ordering::SeqCst);
    let error = driver
        .android()
        .unwrap()
        .install(
            &mut &b"PK\x03\x04"[..],
            4,
            &["-t"],
            Duration::from_millis(500),
            &|| false,
        )
        .unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(
        error.message().contains("did not answer within")
            && error.message().ends_with(" (over the link to build-box, part of the upload may also have been lost while the link reconnected)"),
        "{error}"
    );
    // The call's time was up: the device list was not asked why.
    assert_eq!(fake.recorder.device_lists.load(Ordering::SeqCst), lists);
}

#[test]
fn a_device_without_cmd_is_sent_no_install() {
    let (png, _) = gradient(2, 2);
    let setup = setup(Script::phone(phone_shell(png, 23)).features("shell_v2,stat_v2"));
    let error = android(&setup)
        .install(
            &mut &b"PK"[..],
            2,
            &["-r", "-t"],
            Duration::from_secs(5),
            &|| false,
        )
        .unwrap_err();
    assert_eq!(
        error,
        DriverError::Unsupported(
            "This device's Android is too old for streamed installs (it needs Android 7.0)."
                .to_owned()
        )
    );
    // Only the fixed option words are passed on.
    let error = android(&setup)
        .install(
            &mut &b"PK"[..],
            2,
            &["-r", "--user 0; reboot"],
            Duration::from_secs(5),
            &|| false,
        )
        .unwrap_err();
    assert!(matches!(error, DriverError::Failed(_)), "{error:?}");
    assert!(setup
        .fake
        .requests()
        .iter()
        .all(|request| !request.starts_with("exec:cmd package install")));
    assert!(bodies(&setup.fake).is_empty());
    never_connects(&setup.fake);
}

#[test]
fn a_closed_driver_runs_no_android_operation() {
    let setup = phone(33, FEATURES_V2);
    let before = setup.fake.requests().len();
    setup.driver.close();
    let android = android(&setup);
    assert!(matches!(android.sdk(), Err(DriverError::Disconnected(_))));
    assert!(matches!(
        android.shell("ls", 10, Duration::from_secs(1)),
        Err(DriverError::Disconnected(_))
    ));
    assert!(matches!(
        android.install(&mut &b"PK"[..], 2, &["-t"], Duration::from_secs(1), &|| {
            false
        }),
        Err(DriverError::Disconnected(_))
    ));
    assert_eq!(setup.fake.requests().len(), before);
}

#[test]
fn the_install_service_puts_the_size_last() {
    assert_eq!(
        wire::install_service(&["-r", "-t", "-d", "-g"], 12_345),
        "exec:cmd package install -r -t -d -g -S 12345"
    );
    assert_eq!(
        wire::install_service(&[], 1),
        "exec:cmd package install -S 1"
    );
}

// ---------------------------------------------------------------- the picture stream

/// A phone's `screenrecord`: real H.264, from this Mac's encoder, in the bytes the tool writes.
#[cfg(target_os = "macos")]
#[test]
fn the_stream_shows_a_capture_first_and_then_decoded_video_until_it_is_stopped() {
    use crate::remote_control::picture_stream::test_stream;
    let video = test_stream(320, 240, &[[220, 30, 30], [30, 200, 40], [30, 40, 220]]);
    let (png, _) = gradient(4, 6);
    let shell = phone_shell(png, 33);
    let served = video.clone();
    let fake = FakeAdb::start(Script::phone(move |command: &str| {
        if command.starts_with("screenrecord") {
            Reply::out(&served)
        } else {
            shell(command)
        }
    }));
    let connected = connect_local(fake.port, None, None);
    let driver = connected.driver();
    let seen = Mutex::new(Vec::new());
    // The fake closes the connection after the bytes: the tool's own time limit.
    let result = driver.stream(&|| false, &mut |update| {
        if let ScreenUpdate::Full { width, height, .. } = update {
            seen.lock().unwrap().push((width, height));
        }
    });
    result.unwrap();
    let seen = seen.into_inner().unwrap();
    assert!(seen.iter().filter(|size| **size == (320, 240)).count() >= 3, "{seen:?}");
    let asked: Vec<_> = fake.requests().into_iter().filter(|request| request.contains("screenrecord")).collect();
    assert_eq!(asked, ["exec:screenrecord --output-format=h264 --bit-rate 8000000 -"]);
}

/// A phone whose tool will not run says so, and the next try asks for less.
#[cfg(target_os = "macos")]
#[test]
fn a_stream_that_writes_no_video_says_why_and_asks_for_a_smaller_size_next() {
    let (png, _) = gradient(1080, 2400);
    let shell = phone_shell(png, 33);
    let fake = FakeAdb::start(Script::phone(move |command: &str| {
        if command.starts_with("screenrecord") {
            Reply::out("ERROR: unable to configure video/avc codec at 1080x2400 (err=-19)\n")
        } else {
            shell(command)
        }
    }));
    let connected = connect_local(fake.port, None, None);
    let driver = connected.driver();
    let error = driver.stream(&|| false, &mut |_| {}).unwrap_err();
    assert!(error.message().contains("unable to configure"), "{error}");
    let _ = driver.stream(&|| false, &mut |_| {});
    let asked: Vec<_> = fake.requests().into_iter().filter(|request| request.contains("screenrecord")).collect();
    assert_eq!(asked.len(), 2);
    assert!(asked[1].contains("--size 576x1280"), "{asked:?}");
}
