//! The AI SDK sidecar (`mewrk-aisdk`) from Mewrk's channel, checked at every
//! start.
//!
//! The channel is keyed by the host↔sidecar protocol
//! ([`PROTOCOL_VERSION`]) as well as by the target triple:
//! `aisdk/p<protocol>/<triple>.json`. A host only ever looks at builds that
//! speak its own protocol, so publishing a sidecar for a newer Mewrk never
//! reaches an older one, and an older build left installed is never run by a
//! newer host.
//!
//! Every build is installed into a directory of its own,
//! `<components>/aisdk/<unpacked sha256>/mewrk-aisdk[.exe]`, and
//! `<components>/aisdk/current.json` (the build's pointer plus `installedAt`)
//! says which one is in use. Switching builds is rewriting that one file, so a
//! sidecar already running keeps its executable (Windows could not replace it
//! anyway) and the next one started uses the new build. Builds other than the
//! current one and the one being installed are removed by the next check.
//!
//! A check runs once per start, in the background (release builds; a
//! development build only when [`super::MIRROR_ENV`] points it at a channel).
//! A check that fails leaves the installed build in use; with nothing
//! installed, the first request for the sidecar starts the check again in the
//! background ([`ensure_binary`]), or joins the one still running — waiting
//! for it a bounded time, after which the request is told how far the
//! download has got and the download carries on.
//!
//! The installed sidecar is hashed against its record once per process before
//! it is first run ([`super::verified_file`]); one that does not match is
//! taken for not installed, so the next check downloads it again.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{fetch_pointer_from, install_files_from, Pointer};
use crate::aisdk::protocol::PROTOCOL_VERSION;
use crate::helper_model::download::Progress;
use crate::ui_text::ui_text;

/// The pointer's `component`.
const COMPONENT: &str = "aisdk";
/// The record of the build in use, beside the builds.
const CURRENT: &str = "current.json";
/// The platform this host runs, which is the build it needs.
const TRIPLE: &str = env!("MEWRK_TARGET_TRIPLE");
/// How long a request waits for the sidecar's first download before it is
/// told to come back: long enough for the download on most connections, short
/// enough not to look hung.
const DOWNLOAD_WAIT: Duration = Duration::from_secs(90);
/// How often a waiting request asks its caller whether to stop.
const STOP_PROBE: Duration = Duration::from_millis(500);

/// What the Updates page shows about the sidecar.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AisdkStatus {
    pub state: AisdkState,
    /// The protocol this host speaks (`crate::aisdk::protocol::PROTOCOL_VERSION`).
    pub protocol: u32,
    /// The installed build's `version` and `builtAt`.
    pub version: Option<String>,
    pub built_at: Option<String>,
    /// While downloading.
    pub received_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    /// The last check's or download's failure.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AisdkState {
    /// A development build runs the sidecar from the source tree.
    Development,
    Installed,
    Checking,
    Downloading,
    Failed,
    Missing,
}

/// `current.json`: the pointer of the build in use, and when it was installed.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Installed {
    #[serde(flatten)]
    pointer: Pointer,
    installed_at: String,
}

// ---------------------------------------------------------------------------
// The check in flight
// ---------------------------------------------------------------------------

/// The one check this process runs at a time, for [`status`] and for the
/// requests that wait on it.
struct Check {
    running: bool,
    /// Bytes downloaded so far, of how many, once the download has started.
    progress: Option<(u64, u64)>,
    /// The last finished check's failure; cleared when the next one starts.
    error: Option<String>,
}

static CHECK: Mutex<Check> = Mutex::new(Check { running: false, progress: None, error: None });
/// Signalled whenever a check ends.
static CHECK_ENDED: Condvar = Condvar::new();

fn lock_check() -> MutexGuard<'static, Check> {
    CHECK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The right to run the check, held by whoever runs it. Dropping it, however
/// the check ended (a panic included), wakes everyone waiting.
struct Claim {
    error: Option<String>,
}

impl Claim {
    /// Starts a check; the caller holds `check` and has seen none running.
    fn start(check: &mut Check) -> Self {
        *check = Check { running: true, progress: None, error: None };
        // What the record says if the check never gets to say anything else.
        Self { error: Some("the AI SDK component check ended unexpectedly".into()) }
    }

    /// `None` while another check runs.
    fn take() -> Option<Self> {
        let mut check = lock_check();
        (!check.running).then(|| Self::start(&mut check))
    }

    /// Runs the check against Mewrk's channel and records how it went.
    fn run(mut self) -> Result<(), String> {
        let result = live_site().and_then(|(base, dir)| {
            let site = Site { base: &base, dir: &dir, triple: TRIPLE };
            check(&site, &AtomicBool::new(false), &mut |progress| {
                lock_check().progress = Some((progress.received, progress.total));
            })
        });
        match &result {
            Ok(Checked::Current) => {}
            Ok(Checked::Installed) => eprintln!("[components] installed a new AI SDK sidecar build"),
            Err(error) => eprintln!("[components] AI SDK sidecar check failed: {error}"),
        }
        self.error = result.as_ref().err().cloned();
        result.map(|_| ())
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut check = lock_check();
        check.running = false;
        check.progress = None;
        check.error = self.error.take();
        CHECK_ENDED.notify_all();
    }
}

/// The channel and `<components>/aisdk`, once components have a home.
fn live_site() -> Result<(String, PathBuf), String> {
    let root = super::root().ok_or_else(|| {
        ui_text!("组件目录还没有设定", "The components folder has not been set up")
    })?;
    Ok((super::channel_base(), root.join(COMPONENT)))
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Starts the once-per-start channel check in the background: in release
/// builds always, in a development build only when [`super::MIRROR_ENV`] names
/// a channel to test against.
pub fn spawn_startup_check() {
    if cfg!(debug_assertions) && super::override_base(super::MIRROR_ENV).is_none() {
        return;
    }
    // Claimed before the thread starts, so a request arriving at once waits
    // for this check instead of starting a second one.
    if let Some(claim) = Claim::take() {
        run_in_background(claim);
    }
}

/// Runs a claimed check on a thread of its own.
fn run_in_background(claim: Claim) {
    let spawned = std::thread::Builder::new()
        .name("aisdk-component-check".into())
        .spawn(move || {
            let _ = claim.run();
        });
    if let Err(error) = spawned {
        // The claim went down with the closure, so nobody waits on it.
        eprintln!("[components] could not start the AI SDK sidecar check: {error}");
    }
}

/// The installed sidecar, when one is and it is the file that was installed.
///
/// Its size is checked on every call, its SHA-256 (against `current.json`)
/// once per process ([`super::verified_file`]): the components folder is the
/// user's to write, and a sidecar that is not the published build must not
/// run. One that does not match is not installed as far as this host is
/// concerned, so the next check downloads it again.
pub fn installed_binary() -> Option<PathBuf> {
    let (_, dir) = live_site().ok()?;
    verified_in(&dir, TRIPLE).map(|(_, path)| path)
}

/// How [`ensure_binary`] ended without a sidecar.
#[derive(Debug, PartialEq, Eq)]
pub enum NotReady {
    /// The caller's `stop` said to, with its reason.
    Stopped(String),
    /// None is installed and the download did not finish in time, or failed:
    /// what to tell the user.
    Failed(String),
}

/// The installed sidecar, or — when there is none — the one being downloaded,
/// waited for at most [`DOWNLOAD_WAIT`]. With no download running, one is
/// started in the background. A wait that runs out says how far the download
/// has got; the download carries on, so asking again shortly finds it.
///
/// `stop` is asked every half second while waiting, and ends the wait when it
/// answers an error (the user stopped the step). Callers must not hold
/// anything other steps or a save wait on while in here.
pub fn ensure_binary(stop: &dyn Fn() -> Result<(), String>) -> Result<PathBuf, NotReady> {
    if let Some(path) = installed_binary() {
        return Ok(path);
    }
    let started = {
        let mut check = lock_check();
        (!check.running).then(|| Claim::start(&mut check))
    };
    if let Some(claim) = started {
        run_in_background(claim);
    }
    // The check waited for is the attempt this request gets; a failed one is
    // retried by the next request, not straight away.
    let error = wait_for_check(&CHECK, &CHECK_ENDED, DOWNLOAD_WAIT, stop)?;
    installed_binary().ok_or_else(|| NotReady::Failed(not_ready(&error.unwrap_or_else(not_installed))))
}

/// Waits up to `wait` for the check `check` records to end, asking `stop`
/// every [`STOP_PROBE`]. Returns the ended check's failure, if it failed.
fn wait_for_check(
    check: &Mutex<Check>,
    ended: &Condvar,
    wait: Duration,
    stop: &dyn Fn() -> Result<(), String>,
) -> Result<Option<String>, NotReady> {
    let deadline = Instant::now() + wait;
    let mut state = check.lock().unwrap_or_else(PoisonError::into_inner);
    while state.running {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(NotReady::Failed(still_downloading(state.progress)));
        }
        state = ended.wait_timeout(state, left.min(STOP_PROBE)).unwrap_or_else(PoisonError::into_inner).0;
        if state.running {
            // Asked without the record held: the caller's sink may take a while.
            drop(state);
            stop().map_err(NotReady::Stopped)?;
            state = check.lock().unwrap_or_else(PoisonError::into_inner);
        }
    }
    Ok(state.error.clone())
}

pub fn status() -> AisdkStatus {
    let mut status = AisdkStatus {
        state: AisdkState::Missing,
        protocol: PROTOCOL_VERSION,
        version: None,
        built_at: None,
        received_bytes: None,
        total_bytes: None,
        error: None,
    };
    if runs_development_sidecar() {
        status.state = AisdkState::Development;
        return status;
    }
    let installed = live_site().ok().and_then(|(_, dir)| installed_in(&dir, TRIPLE));
    let (running, progress, error) = {
        let check = lock_check();
        (check.running, check.progress, check.error.clone())
    };
    status.state = match (running, progress, &installed, &error) {
        (true, Some(_), _, _) => AisdkState::Downloading,
        (true, None, _, _) => AisdkState::Checking,
        (false, _, Some(_), _) => AisdkState::Installed,
        (false, _, None, Some(_)) => AisdkState::Failed,
        (false, _, None, None) => AisdkState::Missing,
    };
    if let Some((installed, _)) = installed {
        status.version = Some(installed.pointer.version);
        status.built_at = installed.pointer.built_at;
    }
    if let Some((received, total)) = progress {
        status.received_bytes = Some(received);
        status.total_bytes = Some(total);
    }
    // With a build installed this is an update that failed, and the build in
    // use is still the installed one.
    status.error = error;
    status
}

/// The Claude Agent SDK version the installed sidecar was built against
/// (its pointer's `claudeAgentSdk`).
pub fn claude_agent_sdk_version() -> Option<String> {
    let (_, dir) = live_site().ok()?;
    let (installed, _) = installed_in(&dir, TRIPLE)?;
    claude_agent_sdk_of(&installed.pointer)
}

/// Whether the sidecar [`crate::aisdk::process`] starts is a development one:
/// `MEWRK_AISDK_BIN`, or a debug build's source tree.
fn runs_development_sidecar() -> bool {
    if std::env::var_os(crate::aisdk::process::BINARY_ENV).is_some() {
        return true;
    }
    #[cfg(debug_assertions)]
    if crate::aisdk::process::source_tree_sidecar().is_some() {
        return true;
    }
    false
}

fn claude_agent_sdk_of(pointer: &Pointer) -> Option<String> {
    pointer.extra.get("claudeAgentSdk")?.as_str().map(str::to_owned)
}

fn not_ready(reason: &str) -> String {
    ui_text!(
        "Mewrk 的 AI SDK 组件还没有下载好：{reason}",
        "Mewrk's AI SDK component is not downloaded yet: {reason}"
    )
}

fn not_installed() -> String {
    ui_text!("没有找到可用的已安装构建", "no usable build is installed")
}

/// What a request that stopped waiting for the download is told.
fn still_downloading(progress: Option<(u64, u64)>) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    match progress {
        Some((received, total)) => {
            let (received, total) = (received as f64 / MB, total as f64 / MB);
            ui_text!(
                "Mewrk 的 AI SDK 组件还在下载（{received:.1} / {total:.1} MB）。下载会在后台继续，请稍后重试。",
                "Mewrk's AI SDK component is still downloading ({received:.1} of {total:.1} MB). The download carries on in the background; try again in a moment."
            )
        }
        None => ui_text!(
            "Mewrk 的 AI SDK 组件还在检查和下载中。下载会在后台继续，请稍后重试。",
            "Mewrk's AI SDK component is still being checked for and downloaded. The download carries on in the background; try again in a moment."
        ),
    }
}

// ---------------------------------------------------------------------------
// The check itself
// ---------------------------------------------------------------------------

/// Where a check reads from and installs into: Mewrk's channel and
/// `<components>/aisdk` in the app, a local server and a temporary directory
/// in tests.
struct Site<'a> {
    /// The channel base, like [`super::CHANNEL`].
    base: &'a str,
    /// `<components>/aisdk`.
    dir: &'a Path,
    triple: &'a str,
}

/// How a successful check ended.
#[derive(Debug, PartialEq, Eq)]
enum Checked {
    /// The published build is the installed one.
    Current,
    /// The published build was installed and is now current.
    Installed,
}

/// `aisdk/p<protocol>`: the channel directory this host's builds are under.
fn channel_dir() -> String {
    format!("{COMPONENT}/p{PROTOCOL_VERSION}")
}

/// The sidecar's file name in a build for `triple`.
fn sidecar_name(triple: &str) -> &'static str {
    if triple.contains("-windows-") {
        "mewrk-aisdk.exe"
    } else {
        "mewrk-aisdk"
    }
}

/// Fetches the published pointer and, when it names a build other than the
/// installed one, installs that build and makes it current.
fn check(site: &Site, cancel: &AtomicBool, progress: &mut dyn FnMut(Progress)) -> Result<Checked, String> {
    let pointer_dir = channel_dir();
    let key = format!("{pointer_dir}/{}.json", site.triple);
    let pointer = fetch_pointer_from(site.base, &key, COMPONENT, site.triple)?.ok_or_else(|| {
        let triple = site.triple;
        ui_text!(
            "Mewrk 的发布渠道上没有协议 {PROTOCOL_VERSION}、{triple} 的构建",
            "Mewrk's channel has no build for protocol {PROTOCOL_VERSION} on {triple}"
        )
    })?;
    check_pointer(&pointer, site.triple)?;
    let current = installed_in(site.dir, site.triple);
    let id = pointer.id().to_owned();
    let current_id = current.as_ref().map(|(installed, _)| installed.pointer.id().to_owned());
    // Older builds go, but not the one in use (a sidecar may be running it)
    // nor this one's partial download, which resumes.
    super::prune_dir(site.dir, |name| name == CURRENT || name == id || current_id.as_deref() == Some(name));
    // The published build is installed, unless its file is no longer the one
    // installed: then it is downloaded again, over it.
    if current_id.as_deref() == Some(id.as_str()) && verified_in(site.dir, site.triple).is_some() {
        return Ok(Checked::Current);
    }
    install_files_from(site.base, &pointer_dir, &pointer, &site.dir.join(&id), cancel, progress)?;
    let installed = Installed {
        pointer,
        installed_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    super::write_json_atomic(&site.dir.join(CURRENT), &installed)?;
    Ok(Checked::Installed)
}

/// What the sidecar's pointer must say beyond [`Pointer::validate`]: this
/// host's protocol, and the sidecar as its first file.
fn check_pointer(pointer: &Pointer, triple: &str) -> Result<(), String> {
    let protocol = pointer.extra.get("protocol").and_then(serde_json::Value::as_u64);
    if protocol != Some(u64::from(PROTOCOL_VERSION)) {
        return Err(format!(
            "the {triple} sidecar pointer is for protocol {protocol:?}, not {PROTOCOL_VERSION}"
        ));
    }
    let name = sidecar_name(triple);
    if pointer.files[0].name != name {
        return Err(format!("the {triple} sidecar pointer names {:?}, not {name}", pointer.files[0].name));
    }
    Ok(())
}

/// The build `<dir>/current.json` names, and its sidecar, when it is for
/// `triple` and this host's protocol and the file is there.
fn installed_in(dir: &Path, triple: &str) -> Option<(Installed, PathBuf)> {
    let installed: Installed = super::read_json(&dir.join(CURRENT))?;
    // Validated as any pointer is: what it names stays inside `dir`.
    installed.pointer.validate(COMPONENT, triple).ok()?;
    check_pointer(&installed.pointer, triple).ok()?;
    let file = &installed.pointer.files[0];
    let path = dir.join(installed.pointer.id()).join(&file.name);
    let metadata = fs::metadata(&path).ok()?;
    (metadata.is_file() && metadata.len() == file.unpacked_size).then_some((installed, path))
}

/// [`installed_in`], when the sidecar also hashes to what its record says
/// (once per process: [`super::verified_file`]).
fn verified_in(dir: &Path, triple: &str) -> Option<(Installed, PathBuf)> {
    let (installed, path) = installed_in(dir, triple)?;
    if super::verified_file(&path, &installed.pointer.files[0].unpacked_sha256) {
        return Some((installed, path));
    }
    eprintln!(
        "[components] {} is not the AI SDK sidecar build that was installed; it will be downloaded again",
        path.display()
    );
    None
}

/// A channel of the tests' own, served over plain HTTP.
#[cfg(test)]
pub(super) mod test_channel {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};

    use crate::components::PointerFile;

    /// Serves `files` (channel path, like `aisdk/p17/x.json`, to body) from a
    /// local server; any other path is a 404. Returns its base URL and the
    /// paths asked for, in order.
    pub fn serve(files: HashMap<String, Vec<u8>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let base = format!("http://{}", listener.local_addr().expect("local address"));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(clone) = stream.try_clone() else { continue };
                let mut reader = BufReader::new(clone);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).map_or(true, |read| read == 0) || line.trim().is_empty() {
                        break;
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_owned();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = match files.get(&path) {
                    Some(body) => ("200 OK", body.as_slice()),
                    None => ("404 Not Found", &b""[..]),
                };
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        (base, asked)
    }

    pub fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    /// `bytes` gzipped as the channel publishes them under `<triple>/<gz sha256>/`,
    /// with its pointer entry.
    pub fn packed(triple: &str, name: &str, bytes: &[u8]) -> (PointerFile, Vec<u8>) {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).expect("gzip");
        let gz = encoder.finish().expect("gzip");
        let sha = sha256(&gz);
        let file = PointerFile {
            name: name.into(),
            path: format!("{triple}/{sha}/{name}.gz"),
            size: gz.len() as u64,
            sha256: sha,
            unpacked_size: bytes.len() as u64,
            unpacked_sha256: sha256(bytes),
        };
        (file, gz)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::test_channel::{packed, serve};
    use super::*;

    const LINUX: &str = "x86_64-unknown-linux-musl";

    /// A published sidecar build: its pointer's JSON and the channel files
    /// serving it.
    fn publish(triple: &str, bytes: &[u8], edit: impl FnOnce(&mut serde_json::Value)) -> HashMap<String, Vec<u8>> {
        let dir = channel_dir();
        let (file, gz) = packed(triple, sidecar_name(triple), bytes);
        let mut pointer = serde_json::json!({
            "schema": 1,
            "component": "aisdk",
            "triple": triple,
            "version": "1.2.4",
            "builtAt": "2026-10-07T08:00:00Z",
            "protocol": PROTOCOL_VERSION,
            "claudeAgentSdk": "0.3.284",
            "files": [file.clone()],
        });
        edit(&mut pointer);
        HashMap::from([
            (format!("{dir}/{triple}.json"), serde_json::to_vec(&pointer).unwrap()),
            (format!("{dir}/{}", file.path), gz),
        ])
    }

    fn run(base: &str, dir: &Path) -> Result<Checked, String> {
        check(&Site { base, dir, triple: LINUX }, &AtomicBool::new(false), &mut |_| {})
    }

    fn downloads(asked: &Mutex<Vec<String>>) -> usize {
        asked.lock().unwrap().iter().filter(|path| path.ends_with(".gz")).count()
    }

    #[test]
    fn a_published_build_is_installed_and_becomes_current() {
        let (base, _) = serve(publish(LINUX, b"sidecar one", |_| {}));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(&base, dir.path()), Ok(Checked::Installed));
        let (installed, path) = installed_in(dir.path(), LINUX).expect("installed");
        assert_eq!(fs::read(&path).unwrap(), b"sidecar one");
        assert_eq!(path.file_name().unwrap(), "mewrk-aisdk");
        assert_eq!(path.parent().unwrap().file_name().unwrap().to_str(), Some(installed.pointer.id()));
        assert_eq!(installed.pointer.version, "1.2.4");
        assert_eq!(claude_agent_sdk_of(&installed.pointer).as_deref(), Some("0.3.284"));
        assert!(!installed.installed_at.is_empty());
        assert!(!installed.pointer.extra.contains_key("installedAt"), "the record's own field stays its own");
        assert!(!path.parent().unwrap().join(".download").exists(), "the download is cleaned up");
        // Another protocol's host does not take it for its own.
        let mut record: serde_json::Value = super::super::read_json(&dir.path().join(CURRENT)).unwrap();
        record["protocol"] = serde_json::json!(PROTOCOL_VERSION + 1);
        super::super::write_json_atomic(&dir.path().join(CURRENT), &record).unwrap();
        assert!(installed_in(dir.path(), LINUX).is_none());
    }

    #[test]
    fn an_unchanged_pointer_downloads_nothing() {
        let (base, asked) = serve(publish(LINUX, b"sidecar one", |_| {}));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(&base, dir.path()), Ok(Checked::Installed));
        assert_eq!(downloads(&asked), 1);
        assert_eq!(run(&base, dir.path()), Ok(Checked::Current));
        assert_eq!(downloads(&asked), 1, "the installed build is not fetched again");
        assert_eq!(asked.lock().unwrap().len(), 3, "two pointers and one file");
    }

    #[test]
    fn a_new_build_replaces_the_current_one_which_stays_until_the_next_check() {
        let dir = tempfile::tempdir().unwrap();
        let (base, _) = serve(publish(LINUX, b"sidecar one", |_| {}));
        run(&base, dir.path()).unwrap();
        let (_, first) = installed_in(dir.path(), LINUX).unwrap();
        let (base, _) = serve(publish(LINUX, b"sidecar two", |_| {}));
        assert_eq!(run(&base, dir.path()), Ok(Checked::Installed));
        let (_, second) = installed_in(dir.path(), LINUX).unwrap();
        assert_eq!(fs::read(&second).unwrap(), b"sidecar two");
        assert!(first.is_file(), "a sidecar may still be running the previous build");
        assert_eq!(run(&base, dir.path()), Ok(Checked::Current));
        assert!(!first.exists(), "the next check removes it");
        assert!(second.is_file());
    }

    #[test]
    fn a_tampered_file_is_refused() {
        // The served file is not the one the pointer describes.
        let mut files = publish(LINUX, b"sidecar one", |_| {});
        let (_, other) = packed(LINUX, "mewrk-aisdk", b"something else");
        for (path, body) in files.iter_mut() {
            if path.ends_with(".gz") {
                *body = other.clone();
            }
        }
        let (base, _) = serve(files);
        let dir = tempfile::tempdir().unwrap();
        assert!(run(&base, dir.path()).is_err());
        assert!(installed_in(dir.path(), LINUX).is_none());
        assert!(!dir.path().join(CURRENT).exists());

        // The served file is the listed one, but unpacks to other bytes.
        let (base, _) = serve(publish(LINUX, b"sidecar one", |pointer| {
            pointer["files"][0]["unpackedSha256"] = serde_json::json!("0".repeat(64));
        }));
        let dir = tempfile::tempdir().unwrap();
        assert!(run(&base, dir.path()).is_err());
        assert!(!dir.path().join(CURRENT).exists());
    }

    #[test]
    fn a_pointer_for_another_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        // Another triple's pointer published at this triple's key.
        let mut files = publish("aarch64-apple-darwin", b"sidecar one", |_| {});
        let key = format!("{}/aarch64-apple-darwin.json", channel_dir());
        let pointer = files.remove(&key).unwrap();
        files.insert(format!("{}/{LINUX}.json", channel_dir()), pointer);
        let (base, asked) = serve(files);
        assert!(run(&base, dir.path()).is_err());
        assert_eq!(downloads(&asked), 0);

        // Another protocol's, or one that names a file other than the sidecar.
        for edit in [
            (|pointer: &mut serde_json::Value| pointer["protocol"] = serde_json::json!(PROTOCOL_VERSION - 1))
                as fn(&mut serde_json::Value),
            |pointer| pointer["files"][0]["name"] = serde_json::json!("main.mjs"),
        ] {
            let (base, asked) = serve(publish(LINUX, b"sidecar one", edit));
            assert!(run(&base, dir.path()).is_err());
            assert_eq!(downloads(&asked), 0);
        }
        assert!(installed_in(dir.path(), LINUX).is_none());
    }

    #[test]
    fn no_published_build_is_an_error_naming_the_protocol() {
        let (base, _) = serve(HashMap::new());
        let dir = tempfile::tempdir().unwrap();
        let error = run(&base, dir.path()).unwrap_err();
        assert!(error.contains(&PROTOCOL_VERSION.to_string()) && error.contains(LINUX), "{error}");
    }

    #[test]
    fn the_status_reports_the_protocol_this_host_speaks() {
        assert_eq!(status().protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn a_sidecar_that_is_not_the_installed_build_is_downloaded_again() {
        let (base, asked) = serve(publish(LINUX, b"sidecar one", |_| {}));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run(&base, dir.path()), Ok(Checked::Installed));
        let (_, path) = verified_in(dir.path(), LINUX).expect("installed and verified");
        // Other bytes of the same size, written later.
        fs::write(&path, b"sidecar 1!!").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(installed_in(dir.path(), LINUX).is_some(), "the size alone does not tell");
        assert!(verified_in(dir.path(), LINUX).is_none());
        // The next check takes it for missing and puts the build back.
        assert_eq!(run(&base, dir.path()), Ok(Checked::Installed));
        assert_eq!(downloads(&asked), 2);
        assert_eq!(fs::read(&path).unwrap(), b"sidecar one");
        assert!(verified_in(dir.path(), LINUX).is_some());
    }

    fn check_record(running: bool, progress: Option<(u64, u64)>, error: Option<&str>) -> Mutex<Check> {
        Mutex::new(Check { running, progress, error: error.map(str::to_owned) })
    }

    #[test]
    fn a_request_waits_for_the_download_a_bounded_time_and_hears_how_far_it_got() {
        let ended = Condvar::new();
        let check = check_record(true, Some((12 << 20, 31 << 20)), None);
        let started = Instant::now();
        let outcome = wait_for_check(&check, &ended, Duration::from_millis(300), &|| Ok(()));
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        let Err(NotReady::Failed(message)) = outcome else {
            panic!("expected a bounded wait to fail: {outcome:?}");
        };
        assert!(message.contains("12.0") && message.contains("31.0"), "{message}");

        // Nothing downloaded yet: still said, without numbers. A condvar waits with one mutex
        // only (macOS panics otherwise), so this record gets its own.
        let ended = Condvar::new();
        let check = check_record(true, None, None);
        assert!(matches!(
            wait_for_check(&check, &ended, Duration::from_millis(50), &|| Ok(())),
            Err(NotReady::Failed(_))
        ));
    }

    #[test]
    fn a_stopped_request_stops_waiting_for_the_download() {
        let ended = Condvar::new();
        let check = check_record(true, None, None);
        let started = Instant::now();
        let outcome = wait_for_check(&check, &ended, Duration::from_secs(60), &|| Err("stopped by the user".into()));
        assert_eq!(outcome, Err(NotReady::Stopped("stopped by the user".into())));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn a_request_gets_the_outcome_of_the_check_it_waited_for() {
        let ended = std::sync::Arc::new(Condvar::new());
        let check = std::sync::Arc::new(check_record(true, None, None));
        let finisher = {
            let (check, ended) = (check.clone(), ended.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                *check.lock().unwrap() = Check { running: false, progress: None, error: Some("offline".into()) };
                ended.notify_all();
            })
        };
        let outcome = wait_for_check(&check, &ended, Duration::from_secs(60), &|| Ok(()));
        finisher.join().unwrap();
        assert_eq!(outcome, Ok(Some("offline".into())));
        // One that has already ended is not waited for.
        let done = check_record(false, None, None);
        assert_eq!(wait_for_check(&done, &ended, Duration::from_secs(60), &|| Ok(())), Ok(None));
    }
}
