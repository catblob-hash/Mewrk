//! Where a remote-control service is, and how a driver reaches it.
//!
//! Every service a remote-control entry names is on the loopback of one
//! **place**: this computer, or an SSH machine the user registered. A driver
//! never sees a host name — it asks its [`Connector`] for a port, and the
//! connector dials `127.0.0.1:<port>` here, or has the machine's Mewrk agent
//! dial its own `127.0.0.1:<port>` and relay the bytes over the SSH link
//! ([`crate::preview_tunnel::connect`]). That restriction is the trust rule of
//! `docs/remote-control.md` §2.3: a repository's file can point a session at a
//! port, never at an address, so it cannot aim one at the LAN, at a cloud
//! metadata endpoint, or at a server that would harvest the password the user
//! types.
//!
//! A [`Connection`] is two halves and a handle rather than one stream, because
//! RFB reads on one thread while input is written from others, and because a
//! relay over the agent link has no socket to set a timeout on. The handle's
//! [`ConnectionHandle::set_deadline`] gives every protocol the same timeout
//! either way, and [`ConnectionHandle::close`] unblocks a reader on every
//! platform — which a socket `shutdown` alone does not on Windows, where a
//! `recv` pending on another thread keeps waiting.

use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::preview_remote::RemoteMachine;

/// How often a socket reader wakes to see whether it was closed or ran past its
/// deadline.
const SOCKET_POLL: Duration = Duration::from_millis(200);
/// How long one socket write may block before the peer counts as gone.
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(15);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ends a connection from any thread: shuts the socket, or ends the relay on the
/// machine. Idempotent.
#[derive(Clone)]
pub struct Closer(Arc<dyn Fn() + Send + Sync>);

impl Closer {
    pub fn new(close: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(close))
    }

    pub fn close(&self) {
        (self.0)();
    }
}

impl std::fmt::Debug for Closer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Closer")
    }
}

/// What a socket's reader checks between its short waits.
#[derive(Default)]
struct SocketState {
    closed: AtomicBool,
    deadline: Mutex<Option<Instant>>,
}

/// A socket reader that presents a blocking read, but wakes every
/// [`SOCKET_POLL`] to notice a close or a passed deadline.
struct SocketReader {
    stream: TcpStream,
    state: Arc<SocketState>,
}

impl Read for SocketReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.state.closed.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "the connection was closed",
                ));
            }
            match self.stream.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if lock(&self.state.deadline).is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the other end did not answer in time",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                // A socket shut down under a pending read reads as the end of the stream;
                // one closed here is reported as closed, so a protocol can tell it from a
                // server that hung up.
                Ok(0) if self.state.closed.load(Ordering::Acquire) => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "the connection was closed",
                    ));
                }
                other => return other,
            }
        }
    }
}

enum Limiter {
    Socket(Arc<SocketState>),
    /// A relay has no timeout of its own: a deadline closes it instead, so on a
    /// relay the bound covers the whole exchange rather than each read.
    Relay(Mutex<Option<Deadline>>),
}

/// Closing and bounding a connection, from any thread, after its halves have
/// gone their separate ways.
#[derive(Clone)]
pub struct ConnectionHandle {
    closer: Closer,
    limiter: Arc<Limiter>,
}

impl ConnectionHandle {
    /// Ends the connection. Any read blocked on it returns an error; so does any
    /// later read or write. Idempotent.
    pub fn close(&self) {
        if let Limiter::Socket(state) = self.limiter.as_ref() {
            state.closed.store(true, Ordering::Release);
        }
        self.closer.close();
    }

    /// Bounds what follows by `timeout`, or lifts the bound with `None`. Set it
    /// before an exchange and lift it after: a read past the bound fails with
    /// `TimedOut` on a socket, and the whole connection is closed on a relay.
    pub fn set_deadline(&self, timeout: Option<Duration>) {
        match self.limiter.as_ref() {
            Limiter::Socket(state) => {
                *lock(&state.deadline) = timeout.map(|timeout| Instant::now() + timeout);
            }
            Limiter::Relay(deadline) => {
                *lock(deadline) = timeout.map(|timeout| Deadline::arm(&self.closer, timeout));
            }
        }
    }
}

impl std::fmt::Debug for ConnectionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConnectionHandle")
    }
}

/// One open connection to a service. Dropping every part of it ends it: the
/// socket closes with its last clone, the relay with its last handle.
pub struct Connection {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    handle: ConnectionHandle,
}

impl Connection {
    /// A connection over a TCP socket of this computer.
    pub fn from_tcp(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(SOCKET_POLL))?;
        stream.set_write_timeout(Some(SOCKET_WRITE_TIMEOUT))?;
        let state = Arc::new(SocketState::default());
        let closing = stream.try_clone()?;
        Ok(Self {
            reader: Box::new(SocketReader {
                stream: stream.try_clone()?,
                state: state.clone(),
            }),
            writer: Box::new(stream),
            handle: ConnectionHandle {
                closer: Closer::new(move || {
                    let _ = closing.shutdown(Shutdown::Both);
                }),
                limiter: Arc::new(Limiter::Socket(state)),
            },
        })
    }

    /// A connection over any byte pipe that `closer` can end (a relay over the
    /// agent link). `closer` must make a blocked read return.
    pub fn from_parts(
        reader: Box<dyn Read + Send>,
        writer: Box<dyn Write + Send>,
        closer: Closer,
    ) -> Self {
        Self {
            reader,
            writer,
            handle: ConnectionHandle {
                closer,
                limiter: Arc::new(Limiter::Relay(Mutex::new(None))),
            },
        }
    }

    pub fn handle(&self) -> &ConnectionHandle {
        &self.handle
    }

    /// The reading half, the writing half and the handle, for a protocol that
    /// reads on a thread of its own.
    pub fn split(self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>, ConnectionHandle) {
        (self.reader, self.writer, self.handle)
    }

    /// Reads until the other end closes, refusing more than `limit` bytes.
    pub fn read_to_end_limited(&mut self, limit: usize) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let count = match self.reader.read(&mut chunk) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if count == 0 {
                return Ok(bytes);
            }
            if bytes.len() + count > limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the reply is larger than {limit} bytes"),
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
    }

    pub fn close(&self) {
        self.handle.close();
    }
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

impl Write for Connection {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Closes a connection unless disarmed in time. Dropping the guard disarms it.
pub struct Deadline {
    disarm: Option<mpsc::Sender<()>>,
}

impl Deadline {
    pub fn arm(closer: &Closer, timeout: Duration) -> Self {
        let (disarm, disarmed) = mpsc::channel::<()>();
        let closer = closer.clone();
        let _ = thread::Builder::new()
            .name("remote-control-deadline".into())
            .spawn(move || {
                if let Err(RecvTimeoutError::Timeout) = disarmed.recv_timeout(timeout) {
                    closer.close();
                }
            });
        Self {
            disarm: Some(disarm),
        }
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        if let Some(disarm) = self.disarm.take() {
            let _ = disarm.send(());
        }
    }
}

/// The place a service is on, as drivers reach it.
pub trait Connector: Send + Sync {
    /// Opens a TCP connection to `port` on the place's loopback.
    fn connect(&self, port: u16, timeout: Duration) -> Result<Connection, String>;
    /// The place in the words the model, the log and every error read: "this
    /// computer", the machine's name. Always English (`docs/remote-control.md`
    /// §2.5), whatever language the app shows.
    fn place_label(&self) -> &str;
    /// The place as the viewer page and the pane show it to the person, in the
    /// app's language ("本机"). Never put into model-facing text: that is
    /// [`Self::place_label`].
    fn display_label(&self) -> &str {
        self.place_label()
    }
    /// The place's stable identity, for device locks and stored credentials:
    /// `local`, or `ssh:<machine id>` — never a display name, which the user can
    /// change and need not keep unique.
    fn place_key(&self) -> &str;
    /// Whether the place is this computer.
    fn is_local(&self) -> bool;
    /// Whether whoever dials through this connector has given up on the
    /// connection being made: the session closed while its driver was still
    /// connecting. A driver that waits for something on its way in (a device
    /// still booting, a session WebDriverAgent has to create) looks between
    /// attempts and stops, so a stopped session does not keep polling a device
    /// or take WebDriverAgent's one session from the session that replaced it.
    /// It never refuses a connection: a driver that is closing still reaches its
    /// server (to end the session it made).
    fn cancelled(&self) -> bool {
        false
    }
}

/// What every model-facing text calls this computer.
pub const THIS_COMPUTER: &str = "this computer";

/// This computer's loopback.
#[derive(Clone, Debug)]
pub struct LocalConnector {
    /// The app's own word for it, for the viewer page only.
    display: String,
}

impl LocalConnector {
    pub fn new() -> Self {
        Self {
            display: crate::ui_text::pick("本机", THIS_COMPUTER).to_owned(),
        }
    }
}

impl Default for LocalConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for LocalConnector {
    fn connect(&self, port: u16, timeout: Duration) -> Result<Connection, String> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&address, timeout).map_err(|error| {
            match error.kind() {
                io::ErrorKind::ConnectionRefused => {
                    format!("nothing is listening on 127.0.0.1:{port} on this computer")
                }
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                    format!("127.0.0.1:{port} on this computer did not answer")
                }
                _ => format!("could not connect to 127.0.0.1:{port} on this computer: {error}"),
            }
        })?;
        Connection::from_tcp(stream)
            .map_err(|error| format!("could not use the connection to 127.0.0.1:{port}: {error}"))
    }

    fn place_label(&self) -> &str {
        THIS_COMPUTER
    }

    fn display_label(&self) -> &str {
        &self.display
    }

    fn place_key(&self) -> &str {
        "local"
    }

    fn is_local(&self) -> bool {
        true
    }
}

/// An SSH machine's loopback, through its Mewrk agent.
#[derive(Clone, Debug)]
pub struct MachineConnector {
    machine: RemoteMachine,
    key: String,
}

impl MachineConnector {
    /// `machine_id` is the machine's id in the user's machine list, which is
    /// what [`Connector::place_key`] reports.
    pub fn new(machine: RemoteMachine, machine_id: &str) -> Self {
        Self {
            machine,
            key: format!("ssh:{machine_id}"),
        }
    }
}

impl Connector for MachineConnector {
    fn connect(&self, port: u16, timeout: Duration) -> Result<Connection, String> {
        let machine = self.machine.clone();
        // The relay waits for the machine's link on its own; the deadline bounds the whole
        // exchange, a reconnecting link included.
        let (sender, receiver) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("remote-control-connect".into())
            .spawn(move || {
                let _ = sender.send(crate::preview_tunnel::connect(&machine, "127.0.0.1", port));
            });
        let (process, writer, reader) = match receiver.recv_timeout(timeout) {
            Ok(result) => result?,
            Err(_) => {
                return Err(format!(
                    "127.0.0.1:{port} on {} did not answer in time",
                    self.machine.label()
                ))
            }
        };
        let process = Arc::new(process);
        Ok(Connection::from_parts(
            Box::new(reader),
            Box::new(writer),
            Closer::new(move || {
                process.close_stdin();
                process.kill();
            }),
        ))
    }

    fn place_label(&self) -> &str {
        self.machine.label()
    }

    fn place_key(&self) -> &str {
        &self.key
    }

    fn is_local(&self) -> bool {
        false
    }
}

/// Whether `host` — an SSH machine's host as the user registered it — is this
/// computer: a loopback name or address, or an address one of this computer's
/// own interfaces holds. Such a machine is still dialled through its agent —
/// it may be a guest behind a port forward, with devices of its own — but VNC
/// on it is refused, so the rule that keeps a session off the user's own
/// desktop cannot be stepped around by registering `localhost` as a machine.
///
/// A host that does not resolve is not this computer as far as this check can
/// tell; the SSH connection to it would fail anyway.
pub fn names_this_computer(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    let addresses: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(address) => vec![address],
        Err(_) => (host, 22)
            .to_socket_addrs()
            .map(|resolved| resolved.map(|address| address.ip()).collect())
            .unwrap_or_default(),
    };
    addresses.into_iter().any(is_own_address)
}

/// Loopback, unspecified, or bindable here (which only an address of this
/// computer's own interfaces is).
pub fn is_own_address(address: IpAddr) -> bool {
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    if address.is_loopback() || address.is_unspecified() {
        return true;
    }
    UdpSocket::bind(SocketAddr::new(address, 0)).is_ok()
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn a_local_connection_reaches_a_loopback_port_and_closes_from_another_thread() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.write_all(b"hello").unwrap();
            // Holds the connection open; the client's closer ends it.
            let mut rest = Vec::new();
            let _ = stream.read_to_end(&mut rest);
        });
        let mut connection = LocalConnector::new()
            .connect(port, Duration::from_secs(5))
            .unwrap();
        let mut greeting = [0u8; 5];
        connection.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, b"hello");
        let (mut reader, _writer, handle) = connection.split();
        let reading = thread::spawn(move || {
            let mut byte = [0u8; 1];
            reader.read(&mut byte)
        });
        thread::sleep(Duration::from_millis(100));
        let started = std::time::Instant::now();
        handle.close();
        assert!(reading.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    #[test]
    fn a_refused_port_says_nothing_is_listening() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let error = LocalConnector::new()
            .connect(port, Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(error.contains("nothing is listening"), "{error}");
    }

    #[test]
    fn a_deadline_unblocks_a_read_that_waits_too_long() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(3));
            drop(stream);
        });
        let mut connection = LocalConnector::new()
            .connect(port, Duration::from_secs(5))
            .unwrap();
        let started = std::time::Instant::now();
        connection.handle().set_deadline(Some(Duration::from_millis(300)));
        let mut byte = [0u8; 1];
        let error = connection.read_exact(&mut byte).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
    }

    /// The model reads English whatever the app's language; only the person's page is localized.
    #[test]
    fn this_computer_is_english_for_the_model_and_localized_for_the_person() {
        use crate::{model::ResolvedLanguage, ui_text::with_language};
        let chinese = with_language(ResolvedLanguage::ZhCn, LocalConnector::new);
        assert_eq!(chinese.place_label(), "this computer");
        assert_eq!(chinese.display_label(), "本机");
        let english = with_language(ResolvedLanguage::EnUs, LocalConnector::new);
        assert_eq!(english.place_label(), "this computer");
        assert_eq!(english.display_label(), "this computer");
        assert!(!chinese.cancelled());
    }

    #[test]
    fn this_computer_is_recognised_by_name_and_address() {
        assert!(names_this_computer("localhost"));
        assert!(names_this_computer("LOCALHOST"));
        assert!(names_this_computer("127.0.0.1"));
        assert!(names_this_computer("127.0.0.2"));
        assert!(names_this_computer("::1"));
        assert!(names_this_computer("[::1]"));
        assert!(names_this_computer("::ffff:127.0.0.1"));
        assert!(names_this_computer("0.0.0.0"));
        // TEST-NET-3, which no interface of a test machine holds.
        assert!(!names_this_computer("203.0.113.7"));
    }
}
