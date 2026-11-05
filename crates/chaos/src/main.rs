//! `chaosgw`: HTTPS gateway, mock server and network lab.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chaos::gateway::{self, Faults, GatewayConfig, SocketTuning};
use chaos::{cert, mock, netlab};

const USAGE: &str = "\
chaosgw serve --listen ADDR --upstream ADDR --cert-dir DIR [--hosts a,b] [--netns NS] [--plain]
              [--reset-per-mille N] [--stall-per-mille N --stall-ms MS] [--503-per-mille N]
              [--cc NAME] [--thin] [--relay ADDR] [--relay-netns NS] [--drop-to-sudo-user]
chaosgw mock --listen ADDR [--token-us N] [--first-byte-ms N] [--reasoning N] [--tools]
chaosgw netlab up|down|list
chaosgw netlab set PROFILE
chaosgw netlab show";

struct Args(Vec<String>);

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
    fn num<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.value(name) {
            Some(v) => v.parse().map_err(|_| format!("{name}: bad value {v}")),
            None => Ok(default),
        }
    }
    fn required(&self, name: &str) -> Result<&str, String> {
        self.value(name).ok_or_else(|| format!("{name} is required\n\n{USAGE}"))
    }
}

/// Loads the identity from `dir`, or creates it there. The CA goes to `ca.pem` for clients to trust.
fn load_identity(dir: &PathBuf, hosts: &[String]) -> Result<cert::Identity, String> {
    let (ca, crt, key) = (dir.join("ca.pem"), dir.join("server.der"), dir.join("server.key.der"));
    if let (Ok(ca_pem), Ok(cert_der), Ok(key_pkcs8)) = (std::fs::read_to_string(&ca), std::fs::read(&crt), std::fs::read(&key)) {
        return Ok(cert::Identity { ca_pem, cert_der, key_pkcs8 });
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let id = cert::generate(hosts)?;
    std::fs::write(&ca, &id.ca_pem).map_err(|e| e.to_string())?;
    std::fs::write(&crt, &id.cert_der).map_err(|e| e.to_string())?;
    std::fs::write(&key, &id.key_pkcs8).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    Ok(id)
}

/// Binds in `netns` if one is given. Must happen before the Tokio runtime is relevant to the socket.
fn bind(addr: SocketAddr, netns: Option<&str>) -> Result<std::net::TcpListener, String> {
    let do_bind = move || std::net::TcpListener::bind(addr).map_err(|e| format!("bind {addr}: {e}"));
    let listener = match netns {
        Some(ns) => netlab::in_namespace(ns, do_bind)??,
        None => do_bind()?,
    };
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    Ok(listener)
}

/// After the namespace work is done there is no reason to keep running as root.
#[cfg(unix)]
fn drop_to_sudo_user() -> Result<(), String> {
    let id = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok());
    let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) else {
        return Ok(());
    };
    // SAFETY: plain credential syscalls with no pointers beyond a null list of length 0.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0 {
            return Err(format!("dropping privileges: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

fn serve(args: &Args) -> Result<(), String> {
    let listen: SocketAddr = args.required("--listen")?.parse().map_err(|e| format!("--listen: {e}"))?;
    let upstream: SocketAddr = args.required("--upstream")?.parse().map_err(|e| format!("--upstream: {e}"))?;
    let netns = args.value("--netns");
    let listener = bind(listen, netns)?;

    let relay = match args.value("--relay") {
        Some(addr) => {
            let addr: SocketAddr = addr.parse().map_err(|e| format!("--relay: {e}"))?;
            Some(bind(addr, None)?)
        }
        None => None,
    };
    // Relay sockets have to be born in the client namespace, so a thread stays there making them.
    let socket_factory = match args.value("--relay-netns") {
        Some(ns) => {
            let (tx, rx) = std::sync::mpsc::channel::<std::sync::mpsc::Sender<std::io::Result<socket_in_ns::Raw>>>();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let ns = ns.to_owned();
            std::thread::spawn(move || socket_in_ns::factory(&ns, rx, entered_tx));
            // Privileges are dropped right after this, and entering a namespace needs them.
            entered_rx.recv().map_err(|_| "socket factory stopped".to_owned())??;
            Some(tx)
        }
        None => None,
    };

    #[cfg(unix)]
    if args.flag("--drop-to-sudo-user") {
        drop_to_sudo_user()?;
    }

    // After the drop, so that the key and certificate files belong to the user and not to root.
    let tls = if args.flag("--plain") {
        None
    } else {
        let hosts: Vec<String> = args.value("--hosts").unwrap_or("localhost,127.0.0.1").split(',').map(str::to_owned).collect();
        let dir = PathBuf::from(args.required("--cert-dir")?);
        Some(cert::server_config(&load_identity(&dir, &hosts)?)?)
    };

    let cfg = GatewayConfig {
        upstream,
        faults: Faults {
            reset_per_mille: args.num("--reset-per-mille", 0)?,
            stall_per_mille: args.num("--stall-per-mille", 0)?,
            stall_ms: args.num("--stall-ms", 3000)?,
            http503_per_mille: args.num("--503-per-mille", 0)?,
        },
        tuning: SocketTuning { congestion: args.value("--cc").map(str::to_owned), thin_linear_timeouts: args.flag("--thin") },
        seed: args.num("--seed", 1)?,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener).map_err(|e| e.to_string())?;
        let target = listener.local_addr().map_err(|e| e.to_string())?;
        let (stats, _task) = gateway::spawn(listener, tls, cfg);
        eprintln!("gateway listening on {target}");
        if let Some(relay) = relay {
            let relay = tokio::net::TcpListener::from_std(relay).map_err(|e| e.to_string())?;
            eprintln!("relay listening on {} -> {target}", relay.local_addr().map_err(|e| e.to_string())?);
            let factory = socket_factory.map(Arc::new);
            gateway::spawn_relay(relay, target, move || match &factory {
                Some(factory) => socket_in_ns::request(factory),
                None => tokio::net::TcpSocket::new_v4(),
            });
        }
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                _ = tick.tick() => eprintln!("{}", stats.to_json()),
                _ = tokio::signal::ctrl_c() => return Ok::<(), String>(()),
            }
        }
    })
}

/// Sockets born in another namespace, handed to whichever thread needs them.
mod socket_in_ns {
    use std::sync::mpsc::{channel, Receiver, Sender};

    pub type Raw = std::os::fd::OwnedFd;

    pub fn factory(ns: &str, requests: Receiver<Sender<std::io::Result<Raw>>>, entered: Sender<Result<(), String>>) {
        let path = format!("/var/run/netns/{ns}");
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(e) => {
                    let _ = entered.send(Err(format!("{path}: {e}")));
                    return;
                }
            };
            // SAFETY: valid fd, and this thread exists only to live in that namespace.
            if unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                let _ = entered.send(Err(format!("setns {path}: {}", std::io::Error::last_os_error())));
                return;
            }
        }
        let _ = (&path, entered.send(Ok(())));
        for reply in requests {
            let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).and_then(|s| {
                s.set_nonblocking(true)?;
                Ok(Raw::from(s))
            });
            let _ = reply.send(socket);
        }
    }

    pub fn request(factory: &Sender<Sender<std::io::Result<Raw>>>) -> std::io::Result<tokio::net::TcpSocket> {
        let (tx, rx) = channel();
        factory.send(tx).map_err(|_| std::io::Error::other("socket factory stopped"))?;
        let raw = rx.recv().map_err(|_| std::io::Error::other("socket factory stopped"))??;
        Ok(tokio::net::TcpSocket::from_std_stream(std::net::TcpStream::from(raw)))
    }
}

fn run_mock(args: &Args) -> Result<(), String> {
    let listen: SocketAddr = args.required("--listen")?.parse().map_err(|e| format!("--listen: {e}"))?;
    let cfg = mock::MockConfig {
        token_interval: Duration::from_micros(args.num("--token-us", 0)?),
        first_byte_delay: Duration::from_millis(args.num("--first-byte-ms", 0)?),
        reasoning_tokens: args.num("--reasoning", 0)?,
        tools: args.flag("--tools"),
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(listen).await.map_err(|e| e.to_string())?;
        let server = mock::MockServer::spawn_on(listener, cfg).map_err(|e| e.to_string())?;
        eprintln!("mock listening on {}", server.addr);
        let _ = tokio::signal::ctrl_c().await;
        Ok(())
    })
}

fn run_netlab(args: &Args) -> Result<(), String> {
    match args.0.first().map(String::as_str) {
        Some("up") => netlab::up(),
        Some("down") => netlab::down(),
        Some("list") => {
            for p in netlab::PROFILES {
                println!("{:8} {}", p.name, p.summary);
            }
            Ok(())
        }
        Some("set") => {
            let name = args.0.get(1).ok_or("netlab set PROFILE")?;
            let p = netlab::profile(name).ok_or_else(|| format!("unknown profile {name}"))?;
            netlab::apply_profile(p)?;
            println!("{}: {}", p.name, p.summary);
            Ok(())
        }
        Some("show") => {
            for client_side in [true, false] {
                let side = if client_side { "client" } else { "server" };
                println!("{side}: {:?} {:?}", netlab::qdisc_counters(client_side)?, netlab::tcp_counters(client_side)?);
            }
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    let command = raw.remove(0);
    let args = Args(raw);
    let result = match command.as_str() {
        "serve" => serve(&args),
        "mock" => run_mock(&args),
        "netlab" => run_netlab(&args),
        _ => Err(USAGE.into()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
