//! The adb server's wire protocol: a request is four lowercase hex digits of
//! length and then the service string; the answer is `OKAY` or `FAIL` and a
//! length-prefixed message. One connection carries one service, so every
//! command here opens its own connection and bounds it with a deadline —
//! nothing is held between calls, which is also why the driver has no
//! "disconnect" to survive.
//!
//! Device services go through the server: `host:transport:<serial>` switches
//! the connection to that device, after which the next request is a service of
//! the device's own adbd (`exec:`, `shell,v2,raw:`).
//!
//! Everything read from the other end is bounded. The server may be on another
//! machine (a relay over SSH), and a broken or hostile one must not be able to
//! make us allocate without limit or wait for ever.

use std::io::{self, Read, Write};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::preview_launch_config::sanitize_for_message;
use crate::remote_control::transport::{Connection, Connector};

/// How long a dial to the server may take.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A `host:` query is answered at once.
pub(super) const HOST_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest `shell_v2` packet we accept. adbd sends at most 64 KiB.
const MAX_PACKET: usize = 1 << 20;
/// How much of a command's stderr is kept. The rest is read and dropped.
const STDERR_LIMIT: usize = 64 * 1024;
/// How much a command may print past what [`Server::shell_capped`] keeps — read and dropped, so
/// that its exit status still arrives — before the connection is dropped instead.
const DRAIN_LIMIT: usize = 64 * 1024 * 1024;
/// pm's answer to a streamed install: a line, or a few.
const INSTALL_ANSWER_LIMIT: usize = 64 * 1024;
/// The APK is written in pieces of this size; the cancellation and the time are looked at
/// between them.
const INSTALL_CHUNK: usize = 64 * 1024;
/// How long a package installer that stopped reading the APK gets to say why.
const INSTALL_REFUSAL_WAIT: Duration = Duration::from_secs(5);
/// How often a cancel is looked for while pm works on the APK it has whole.
const INSTALL_ANSWER_POLL: Duration = Duration::from_millis(200);

const ID_STDOUT: u8 = 1;
const ID_STDERR: u8 = 2;
const ID_EXIT: u8 = 3;
const ID_CLOSE_STDIN: u8 = 4;

/// Why an exchange with the adb server did not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WireError {
    /// The connector could not reach the server's port; the text is its own.
    Unreachable(String),
    /// The server (or the device behind it) answered `FAIL`.
    Refused(String),
    /// What came back is not the adb protocol, or is more than we accept.
    Protocol(String),
    /// The exchange broke off or ran out of time.
    Io(String),
}

impl WireError {
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Unreachable(message)
            | Self::Refused(message)
            | Self::Protocol(message)
            | Self::Io(message) => message,
        }
    }

    /// The message as it may be shown: what the server wrote itself (a `FAIL`)
    /// is trimmed to one plain line, the rest is this module's own words.
    pub(super) fn display(&self) -> String {
        match self {
            Self::Refused(message) => sanitize_for_message(message),
            other => other.message().to_owned(),
        }
    }
}

fn io_error(what: &str, error: &io::Error) -> WireError {
    match error.kind() {
        io::ErrorKind::TimedOut => {
            WireError::Io(format!("the adb server did not answer in time ({what})"))
        }
        io::ErrorKind::UnexpectedEof => {
            WireError::Io(format!("the adb server closed the connection ({what})"))
        }
        _ => WireError::Io(format!("the adb connection failed ({what}): {error}")),
    }
}

/// What a command printed, and how it ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ShellOutput {
    pub stdout: Vec<u8>,
    /// Always empty for `exec:`, which has no separate stderr.
    pub stderr: Vec<u8>,
    /// The command's exit status; `None` for `exec:` and for a `shell_v2`
    /// stream that ended without one.
    pub exit: Option<u8>,
}

/// What a command printed, kept up to a limit rather than refused past it, and how it ended.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CappedOutput {
    pub output: ShellOutput,
    /// stdout or stderr printed more than the limit, and was cut there.
    pub truncated: bool,
    /// The command had not ended when the reading stopped: its time ran out, or it printed more
    /// than [`DRAIN_LIMIT`] past the limit. What was kept is all there is, without a status.
    pub unfinished: bool,
}

/// Why a streamed install has no answer from the device's package installer.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum InstallFailure {
    /// The exchange with the adb server failed.
    Wire(WireError),
    /// Stopped before the whole APK was sent — cancelled, or out of time. The connection was
    /// dropped with it, and the installer discards an APK that did not arrive whole.
    Stopped(String),
    /// The APK could not be read on this side.
    Source(String),
}

/// How a capped read stopped short.
enum ReadStop {
    /// The deadline passed before the command ended.
    TimedOut,
    Wire(WireError),
}

fn read_stop(what: &str, error: &io::Error) -> ReadStop {
    if error.kind() == io::ErrorKind::TimedOut {
        ReadStop::TimedOut
    } else {
        ReadStop::Wire(io_error(what, error))
    }
}

/// Appends what fits of `data` under `limit`; the rest counts as drained.
fn keep(into: &mut Vec<u8>, data: &[u8], limit: usize, truncated: &mut bool, drained: &mut usize) {
    let room = limit.saturating_sub(into.len());
    if data.len() > room {
        *truncated = true;
        *drained += data.len() - room;
    }
    into.extend_from_slice(&data[..data.len().min(room)]);
}

/// `exec:cmd package install <flags> -S <size>`: the streamed install `adb install` makes when
/// the device has the `cmd` feature. `-S` comes last, after the fixed option words.
pub(super) fn install_service(flags: &[&str], size: u64) -> String {
    let mut service = "exec:cmd package install".to_owned();
    for flag in flags {
        service.push(' ');
        service.push_str(flag);
    }
    service.push_str(&format!(" -S {size}"));
    service
}

/// pm's status line in what it printed: the last `Success` or `Failure [...]` line, else the last
/// line it printed at all.
fn install_answer(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|line| line.starts_with("Success") || line.starts_with("Failure"))
        .or_else(|| lines.last())
        .map(|line| (*line).to_owned())
}

/// The adb server of one place.
#[derive(Clone)]
pub(super) struct Server {
    connector: Arc<dyn Connector>,
    port: u16,
}

impl Server {
    pub(super) fn new(connector: Arc<dyn Connector>, port: u16) -> Self {
        Self { connector, port }
    }

    pub(super) fn port(&self) -> u16 {
        self.port
    }

    pub(super) fn connector(&self) -> &dyn Connector {
        self.connector.as_ref()
    }

    /// A fresh connection to the server, closed by `timeout` — counted from now, the dial
    /// included — whatever happens on it.
    fn open(&self, timeout: Duration) -> Result<Connection, WireError> {
        let started = Instant::now();
        let dial = CONNECT_TIMEOUT.min(timeout);
        let connection = match self.connector.connect(self.port, dial) {
            Ok(connection) => connection,
            // A dial cut short by what is left of the caller's time says nothing about whether
            // the server is there.
            Err(_) if dial < CONNECT_TIMEOUT && started.elapsed() >= dial => {
                return Err(WireError::Io(
                    "the adb server did not answer in time (connecting)".to_owned(),
                ))
            }
            Err(message) => return Err(WireError::Unreachable(message)),
        };
        let left = timeout.saturating_sub(started.elapsed());
        if left.is_zero() {
            connection.close();
            return Err(WireError::Io(
                "the adb server did not answer in time (connecting)".to_owned(),
            ));
        }
        connection.handle().set_deadline(Some(left));
        Ok(connection)
    }

    /// Sends a `host:` request and reads its length-prefixed answer.
    pub(super) fn query(&self, request: &str, timeout: Duration) -> Result<Vec<u8>, WireError> {
        let mut connection = self.open(timeout)?;
        send(&mut connection, request)?;
        read_status(&mut connection)?;
        read_chunk(&mut connection)
    }

    /// The server's protocol version (`host:version`), which doubles as the
    /// check that the port really is an adb server.
    pub(super) fn version(&self) -> Result<u32, WireError> {
        let payload = self.query("host:version", HOST_TIMEOUT)?;
        let text = String::from_utf8_lossy(&payload);
        if payload.is_empty() || payload.len() > 8 || !payload.iter().all(u8::is_ascii_hexdigit) {
            return Err(WireError::Protocol(format!(
                "port {} answered, but not like an adb server (version reply {:?})",
                self.port,
                text.chars().take(16).collect::<String>()
            )));
        }
        u32::from_str_radix(&text, 16).map_err(|_| {
            WireError::Protocol(format!("port {} sent an unreadable adb version", self.port))
        })
    }

    /// `host:devices-l`: one line per device, as `adb devices -l` prints them.
    pub(super) fn devices(&self) -> Result<String, WireError> {
        let payload = self.query("host:devices-l", HOST_TIMEOUT)?;
        Ok(String::from_utf8_lossy(&payload).into_owned())
    }

    /// The features adb knows `serial` to have (`shell_v2`, `cmd`, ...).
    pub(super) fn features(&self, serial: &str) -> Result<Vec<String>, WireError> {
        let payload = self.query(&format!("host-serial:{serial}:features"), HOST_TIMEOUT)?;
        Ok(String::from_utf8_lossy(&payload)
            .split(',')
            .map(str::trim)
            .filter(|feature| !feature.is_empty())
            .map(str::to_owned)
            .collect())
    }

    /// Runs `command` through the device's shell.
    ///
    /// With `shell_v2` the stream is framed (stdout, stderr and the exit status
    /// arrive apart, which matters for a PNG that stderr must not be mixed
    /// into); without it the older `exec:` service sends raw bytes, stderr
    /// among them. `limit` bounds stdout; `timeout` bounds the whole exchange.
    pub(super) fn shell(
        &self,
        serial: &str,
        command: &str,
        shell_v2: bool,
        limit: usize,
        timeout: Duration,
    ) -> Result<ShellOutput, WireError> {
        let mut connection = self.open(timeout)?;
        send(&mut connection, &format!("host:transport:{serial}"))?;
        read_status(&mut connection)?;
        if shell_v2 {
            send(&mut connection, &format!("shell,v2,raw:{command}"))?;
            read_status(&mut connection)?;
            // Nothing will be typed into the command: end its stdin now, so that
            // one that reads it does not wait for us.
            connection
                .write_all(&[ID_CLOSE_STDIN, 0, 0, 0, 0])
                .and_then(|()| connection.flush())
                .map_err(|error| io_error("closing the command's stdin", &error))?;
            read_packets(&mut connection, limit)
        } else {
            send(&mut connection, &format!("exec:{command}"))?;
            read_status(&mut connection)?;
            match connection.read_to_end_limited(limit) {
                Ok(stdout) => Ok(ShellOutput {
                    stdout,
                    ..ShellOutput::default()
                }),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => Err(
                    WireError::Protocol(format!("the command printed more than {limit} bytes")),
                ),
                Err(error) => Err(io_error("reading the command's output", &error)),
            }
        }
    }

    /// A command that goes on writing, its raw output as it comes. The connection has no deadline:
    /// closing it, through its handle, is what ends the wait.
    pub(super) fn exec_stream(&self, serial: &str, command: &str, timeout: Duration) -> Result<Connection, WireError> {
        let mut connection = self.open(timeout)?;
        send(&mut connection, &format!("host:transport:{serial}"))?;
        read_status(&mut connection)?;
        send(&mut connection, &format!("exec:{command}"))?;
        read_status(&mut connection)?;
        connection.handle().set_deadline(None);
        Ok(connection)
    }

    /// [`Self::shell`] for a command whose output may be long, and whose exit status is data
    /// (the Android tools): each stream is kept up to `limit` bytes, and what it prints beyond
    /// that is read and dropped — up to [`DRAIN_LIMIT`] — so the exit status still arrives. A
    /// command that has not ended by `timeout`, or that printed more than [`DRAIN_LIMIT`], is not
    /// an error: what was kept is the answer, marked [`CappedOutput::unfinished`] and without a
    /// status, and the caller decides what an unfinished command is worth.
    pub(super) fn shell_capped(
        &self,
        serial: &str,
        command: &str,
        shell_v2: bool,
        limit: usize,
        timeout: Duration,
    ) -> Result<CappedOutput, WireError> {
        let started = Instant::now();
        let mut connection = self.open(timeout)?;
        send(&mut connection, &format!("host:transport:{serial}"))?;
        read_status(&mut connection)?;
        let mut capped = CappedOutput::default();
        let read = if shell_v2 {
            send(&mut connection, &format!("shell,v2,raw:{command}"))?;
            read_status(&mut connection)?;
            connection
                .write_all(&[ID_CLOSE_STDIN, 0, 0, 0, 0])
                .and_then(|()| connection.flush())
                .map_err(|error| io_error("closing the command's stdin", &error))?;
            read_packets_capped(&mut connection, limit, &mut capped)
        } else {
            send(&mut connection, &format!("exec:{command}"))?;
            read_status(&mut connection)?;
            read_raw_capped(&mut connection, limit, &mut capped)
        };
        match read {
            // A relay has no read timeout of its own: its deadline closes it, which reads as
            // the end of the stream.
            Ok(()) if capped.output.exit.is_none() && started.elapsed() >= timeout => {
                capped.unfinished = true;
                Ok(capped)
            }
            Ok(()) => Ok(capped),
            Err(ReadStop::TimedOut) => {
                capped.unfinished = true;
                Ok(capped)
            }
            Err(ReadStop::Wire(error)) => Err(error),
        }
    }

    /// Streams an APK of exactly `size` bytes into the device's package installer, as `adb
    /// install` does with the `cmd` feature: `host:transport:<serial>`, then
    /// `exec:cmd package install <flags> -S <size>`, then the bytes, then pm's status line.
    ///
    /// The stream is never half-closed: the size tells pm where the APK ends. Between pieces of
    /// [`INSTALL_CHUNK`] bytes, `cancelled` and the time are looked at; either stops the install
    /// by dropping the connection, which makes pm fail it. Once the whole APK is sent, a cancel
    /// still ends the wait for pm's answer at once ([`wait_for_answer`]), though pm may finish
    /// that install. `flags` are the caller's checked option words.
    pub(super) fn install(
        &self,
        serial: &str,
        flags: &[&str],
        size: u64,
        apk: &mut dyn Read,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, InstallFailure> {
        let deadline = Instant::now() + timeout;
        let mut connection = self.open(timeout).map_err(InstallFailure::Wire)?;
        send(&mut connection, &format!("host:transport:{serial}")).map_err(InstallFailure::Wire)?;
        read_status(&mut connection).map_err(InstallFailure::Wire)?;
        send(&mut connection, &install_service(flags, size)).map_err(InstallFailure::Wire)?;
        read_status(&mut connection).map_err(InstallFailure::Wire)?;
        let mut chunk = vec![0u8; INSTALL_CHUNK];
        let mut sent = 0u64;
        while sent < size {
            if cancelled() {
                return Err(InstallFailure::Stopped(format!(
                    "the install was cancelled after {sent} of {size} bytes; the device discards an APK that did not arrive whole"
                )));
            }
            if Instant::now() >= deadline {
                return Err(InstallFailure::Stopped(format!(
                    "the install ran out of time after {sent} of {size} bytes; the device discards an APK that did not arrive whole"
                )));
            }
            let want = chunk
                .len()
                .min(usize::try_from(size - sent).unwrap_or(usize::MAX));
            let read = match apk.read(&mut chunk[..want]) {
                Ok(0) => {
                    return Err(InstallFailure::Source(format!(
                        "the APK ended after {sent} of {size} bytes; it changed while it was being sent"
                    )))
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(InstallFailure::Source(format!(
                        "could not read the APK: {error}"
                    )))
                }
            };
            if let Err(error) = connection.write_all(&chunk[..read]) {
                // An installer that gave up stops reading, and has said why before it did.
                connection.handle().set_deadline(Some(INSTALL_REFUSAL_WAIT));
                return match connection.read_to_end_limited(INSTALL_ANSWER_LIMIT) {
                    Ok(bytes) => install_answer(&bytes)
                        .ok_or_else(|| InstallFailure::Wire(io_error("streaming the APK", &error))),
                    Err(_) => Err(InstallFailure::Wire(io_error("streaming the APK", &error))),
                };
            }
            sent += read as u64;
        }
        connection
            .flush()
            .map_err(|error| InstallFailure::Wire(io_error("streaming the APK", &error)))?;
        // Over a relay a write only queues: a link that reconnects during the upload can lose
        // part of it without a write failing, and pm then waits for bytes that never come.
        let relay_note = if self.connector.is_local() {
            String::new()
        } else {
            format!(
                " (over the link to {}, part of the upload may also have been lost while the link reconnected)",
                sanitize_for_message(self.connector.place_label())
            )
        };
        let read = match wait_for_answer(connection, cancelled) {
            Some(read) => read,
            None => return Err(InstallFailure::Stopped(CANCELLED_AFTER_UPLOAD.to_owned())),
        };
        let late = Instant::now() >= deadline;
        let unanswered = || {
            InstallFailure::Wire(WireError::Io(format!(
                "the whole APK was sent, but the device's package installer did not answer within {} s; it may still finish, so look with screen_app info before installing again{relay_note}",
                timeout.as_secs_f64().ceil()
            )))
        };
        match read {
            Ok(bytes) => match install_answer(&bytes) {
                Some(answer) => Ok(answer),
                // A relay's deadline ends it, which reads as the end of the stream.
                None if late => Err(unanswered()),
                None => Err(InstallFailure::Wire(WireError::Protocol(format!(
                    "the device's package installer answered nothing{relay_note}"
                )))),
            },
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                Err(InstallFailure::Wire(WireError::Protocol(format!(
                    "the device's package installer answered more than {INSTALL_ANSWER_LIMIT} bytes"
                ))))
            }
            Err(error) if late || error.kind() == io::ErrorKind::TimedOut => Err(unanswered()),
            Err(error) => Err(InstallFailure::Wire(io_error(
                "waiting for the package installer's answer",
                &error,
            ))),
        }
    }
}

/// What a streamed install says when it is cancelled while pm works on an APK it has whole.
const CANCELLED_AFTER_UPLOAD: &str = "the install was cancelled after the whole APK was sent; the device may still install it, so look with screen_app info before installing again";

/// pm's answer to a streamed install, read to its end on a thread of its own: pm verifies and
/// optimizes the app after the last byte, which can take minutes, and a cancel meanwhile ends the
/// wait at once — the connection is closed, which also ends the reading thread — and answers
/// `None`. The connection's deadline bounds the read either way.
fn wait_for_answer(
    mut connection: Connection,
    cancelled: &dyn Fn() -> bool,
) -> Option<io::Result<Vec<u8>>> {
    let handle = connection.handle().clone();
    let (sender, answer) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("adb-install-answer".into())
        .spawn(move || {
            let _ = sender.send(connection.read_to_end_limited(INSTALL_ANSWER_LIMIT));
        });
    if let Err(error) = spawned {
        return Some(Err(error));
    }
    loop {
        match answer.recv_timeout(INSTALL_ANSWER_POLL) {
            Ok(read) => return Some(read),
            Err(mpsc::RecvTimeoutError::Timeout) if cancelled() => {
                handle.close();
                return None;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Some(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the wait for the answer broke off",
                )))
            }
        }
    }
}

/// [`read_packets`] keeping `limit` bytes of each stream and dropping the rest.
fn read_packets_capped(
    connection: &mut Connection,
    limit: usize,
    capped: &mut CappedOutput,
) -> Result<(), ReadStop> {
    const WHAT: &str = "reading the command's output";
    let mut drained = 0usize;
    loop {
        let mut id = [0u8; 1];
        loop {
            match connection.read(&mut id) {
                // The device closed the stream between packets.
                Ok(0) => return Ok(()),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(read_stop(WHAT, &error)),
            }
        }
        let mut length = [0u8; 4];
        connection
            .read_exact(&mut length)
            .map_err(|error| read_stop(WHAT, &error))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_PACKET {
            return Err(ReadStop::Wire(WireError::Protocol(
                "the device sent an oversized shell packet".to_owned(),
            )));
        }
        let mut data = vec![0u8; length];
        connection
            .read_exact(&mut data)
            .map_err(|error| read_stop(WHAT, &error))?;
        let output = &mut capped.output;
        match id[0] {
            ID_STDOUT => keep(
                &mut output.stdout,
                &data,
                limit,
                &mut capped.truncated,
                &mut drained,
            ),
            ID_STDERR => keep(
                &mut output.stderr,
                &data,
                limit,
                &mut capped.truncated,
                &mut drained,
            ),
            ID_EXIT => {
                output.exit = data.first().copied();
                return Ok(());
            }
            _ => {}
        }
        if drained > DRAIN_LIMIT {
            capped.unfinished = true;
            return Ok(());
        }
    }
}

/// The raw `exec:` stream, keeping `limit` bytes and dropping the rest.
fn read_raw_capped(
    connection: &mut Connection,
    limit: usize,
    capped: &mut CappedOutput,
) -> Result<(), ReadStop> {
    let mut chunk = vec![0u8; 64 * 1024];
    let mut drained = 0usize;
    loop {
        let count = match connection.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(read_stop("reading the command's output", &error)),
        };
        keep(
            &mut capped.output.stdout,
            &chunk[..count],
            limit,
            &mut capped.truncated,
            &mut drained,
        );
        if drained > DRAIN_LIMIT {
            capped.unfinished = true;
            return Ok(());
        }
    }
}

fn send(connection: &mut Connection, service: &str) -> Result<(), WireError> {
    if service.len() > 0xffff {
        return Err(WireError::Protocol(
            "the request is too long for the adb protocol".to_owned(),
        ));
    }
    let mut request = format!("{:04x}", service.len()).into_bytes();
    request.extend_from_slice(service.as_bytes());
    connection
        .write_all(&request)
        .and_then(|()| connection.flush())
        .map_err(|error| io_error("sending a request", &error))
}

fn read_status(connection: &mut Connection) -> Result<(), WireError> {
    let mut status = [0u8; 4];
    connection
        .read_exact(&mut status)
        .map_err(|error| io_error("waiting for OKAY", &error))?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let message = read_chunk(connection)?;
            Err(WireError::Refused(
                String::from_utf8_lossy(&message).trim().to_owned(),
            ))
        }
        other => Err(WireError::Protocol(format!(
            "the adb server answered {:?} instead of OKAY or FAIL",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Four hex digits of length, then that many bytes. The length field cannot
/// say more than 65535, so the buffer is bounded by the format itself.
fn read_chunk(connection: &mut Connection) -> Result<Vec<u8>, WireError> {
    let mut length = [0u8; 4];
    connection
        .read_exact(&mut length)
        .map_err(|error| io_error("reading a reply length", &error))?;
    if !length.iter().all(u8::is_ascii_hexdigit) {
        return Err(WireError::Protocol(
            "the adb server sent a reply length that is not hexadecimal".to_owned(),
        ));
    }
    let length = usize::from_str_radix(&String::from_utf8_lossy(&length), 16).unwrap_or(0);
    let mut payload = vec![0u8; length];
    connection
        .read_exact(&mut payload)
        .map_err(|error| io_error("reading a reply", &error))?;
    Ok(payload)
}

/// Reads `shell_v2` packets until the exit status or the end of the stream:
/// a byte of packet id (1 stdout, 2 stderr, 3 exit) and a little-endian u32
/// of length.
fn read_packets(connection: &mut Connection, limit: usize) -> Result<ShellOutput, WireError> {
    let mut output = ShellOutput::default();
    loop {
        let mut id = [0u8; 1];
        loop {
            match connection.read(&mut id) {
                // The device closed the stream between packets.
                Ok(0) => return Ok(output),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error("reading the command's output", &error)),
            }
        }
        let mut length = [0u8; 4];
        connection
            .read_exact(&mut length)
            .map_err(|error| io_error("reading the command's output", &error))?;
        let length = u32::from_le_bytes(length) as usize;
        if length > MAX_PACKET {
            return Err(WireError::Protocol(
                "the device sent an oversized shell packet".to_owned(),
            ));
        }
        let mut data = vec![0u8; length];
        connection
            .read_exact(&mut data)
            .map_err(|error| io_error("reading the command's output", &error))?;
        match id[0] {
            ID_STDOUT => {
                if output.stdout.len() + data.len() > limit {
                    return Err(WireError::Protocol(format!(
                        "the command printed more than {limit} bytes"
                    )));
                }
                output.stdout.extend_from_slice(&data);
            }
            ID_STDERR => {
                let room = STDERR_LIMIT.saturating_sub(output.stderr.len());
                output
                    .stderr
                    .extend_from_slice(&data[..data.len().min(room)]);
            }
            ID_EXIT => {
                output.exit = data.first().copied();
                return Ok(output);
            }
            _ => {}
        }
    }
}
