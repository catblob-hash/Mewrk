//! The parts of Mewrk its installer does not carry: each is fetched when it is
//! first needed and updated on a schedule of its own.
//!
//! - [`aisdk`]: the AI SDK sidecar (`mewrk-aisdk`), from Mewrk's own channel,
//!   checked once at every start.
//! - [`remote_agents`]: the agent builds for machines other than this computer
//!   (SSH machines, WSL), fetched the first time such a machine needs one.
//! - [`claude_agent`]: the Claude Agent SDK and the Claude Code CLI it drives,
//!   from npm, installed and updated by the user on the provider page.
//!
//! # Mewrk's own channel
//!
//! Mewrk's components are published by `scripts/publish-components.mjs` to the
//! R2 bucket behind `https://dl.mewrk.dev`, under `components/`. Every build is
//! described by a small *pointer* file and stored gzipped under the SHA-256 of
//! its compressed bytes, so a published file never changes and the CDN may keep
//! it for good while the pointer, which does change, is kept for a minute:
//!
//! ```text
//! components/aisdk/p<protocol>/<triple>.json                     pointer
//! components/aisdk/p<protocol>/<triple>/<sha256>/mewrk-aisdk.exe.gz
//! components/remote-agent/<source id>/<triple>.json              pointer
//! components/remote-agent/<source id>/<triple>/<sha256>/mewrk-remote.gz
//! ```
//!
//! A pointer's `files[].path` is relative to the directory the pointer is in.
//! Trust is the app updater's: HTTPS to Mewrk's own host, every file checked
//! against the SHA-256 the pointer lists, compressed and unpacked; an agent is
//! further checked against the source identity compiled into this host
//! (`remote_link::Build::read`).
//!
//! `MEWRK_COMPONENTS_MIRROR` replaces `https://dl.mewrk.dev/components` with
//! another base serving the same layout; development and tests serve
//! components from it. A release build takes it only over HTTPS, or plain HTTP
//! to this computer itself ([`override_base`]).
//!
//! Installed components live under `<app local data>/components/`, beside the
//! local model's files: per user, writable, and outside the per-machine install
//! directory the installer owns. Because that folder is the user's to write,
//! an executable there is hashed once per process before it is first run
//! ([`verified_file`]), against the digest recorded when it was installed.

pub mod aisdk;
pub mod claude_agent;
pub mod remote_agents;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::helper_model::download::{self, Progress, RemoteFile, Source};
use crate::ui_text::ui_text;

/// Where Mewrk's own components are published.
pub const CHANNEL: &str = "https://dl.mewrk.dev/components";
/// A base URL that replaces [`CHANNEL`].
pub const MIRROR_ENV: &str = "MEWRK_COMPONENTS_MIRROR";
/// The pointer format this host reads. A pointer of a newer schema is one this
/// host does not understand and is treated as absent.
pub const POINTER_SCHEMA: u32 = 1;
/// No pointer is anywhere near this; anything larger is not one.
const POINTER_LIMIT: usize = 64 * 1024;
/// No component file is anywhere near this (the sidecar is ~92 MiB unpacked).
const FILE_LIMIT: u64 = 1 << 30;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Records where installed components live: `<local_data_dir>/components`.
/// Called once at startup, before anything asks for a component; later calls
/// are ignored.
pub fn initialize(local_data_dir: &Path) {
    let _ = ROOT.set(local_data_dir.join("components"));
}

/// `<app local data>/components`, once [`initialize`] has run.
pub fn root() -> Option<&'static Path> {
    ROOT.get().map(PathBuf::as_path)
}

/// The base every channel path is under: [`CHANNEL`], or [`MIRROR_ENV`].
pub fn channel_base() -> String {
    override_base(MIRROR_ENV).unwrap_or_else(|| CHANNEL.to_owned())
}

/// The base URL the environment variable `name` sets in place of a built-in
/// one ([`MIRROR_ENV`], `claude_agent::REGISTRY_ENV`), without a trailing
/// slash; `None` when it sets none.
///
/// What comes from there is trusted as the built-in host is, so a release
/// build takes only an HTTPS base, or a plain HTTP one on this computer (a
/// test server); anything else is ignored, said on stderr. Development builds
/// take any.
pub fn override_base(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let base = value.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    if !acceptable_override(base, !cfg!(debug_assertions)) {
        eprintln!(
            "[components] ignoring {name}={base}: a release build takes only https://, or http:// to this computer"
        );
        return None;
    }
    Some(base.to_owned())
}

/// Whether `base` may replace a built-in host: in a `release` build, an HTTPS
/// URL or a plain HTTP one whose host is a loopback address.
fn acceptable_override(base: &str, release: bool) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" if !release => true,
        "http" => match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        },
        _ => false,
    }
}

/// One build of a component on Mewrk's channel.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Pointer {
    pub schema: u32,
    /// `aisdk` or `remote-agent`.
    pub component: String,
    /// The Rust target triple the build runs on.
    pub triple: String,
    /// For people: the Mewrk version the build was published with.
    pub version: String,
    /// RFC 3339, for people.
    #[serde(default)]
    pub built_at: Option<String>,
    pub files: Vec<PointerFile>,
    /// What else the component's own pointer says (the sidecar's `protocol`
    /// and `claudeAgentSdk`, an agent's `source`), read by that component.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PointerFile {
    /// The installed file's name: `mewrk-aisdk.exe`, `srt-win.exe`.
    pub name: String,
    /// The gzipped file, relative to the pointer's directory.
    pub path: String,
    /// Of the gzipped file.
    pub size: u64,
    pub sha256: String,
    pub unpacked_size: u64,
    pub unpacked_sha256: String,
}

impl Pointer {
    /// Refuses a pointer that names files outside its own directory, that is
    /// not for `component`/`triple`, or that this host cannot read.
    pub fn validate(&self, component: &str, triple: &str) -> Result<(), String> {
        if self.schema != POINTER_SCHEMA {
            return Err(format!("pointer schema {} is not {POINTER_SCHEMA}", self.schema));
        }
        if self.component != component || self.triple != triple {
            return Err(format!(
                "pointer is for {}/{}, not {component}/{triple}",
                self.component, self.triple
            ));
        }
        if self.files.is_empty() {
            return Err("pointer lists no files".into());
        }
        // Unique however the file system folds case: on Windows and macOS two
        // names differing only in case are one file.
        let mut names = std::collections::HashSet::new();
        for file in &self.files {
            if !is_plain_name(&file.name) || !names.insert(file.name.to_ascii_lowercase()) {
                return Err(format!("pointer file name {:?} is not a plain, unique name", file.name));
            }
            if !is_relative_path(&file.path) {
                return Err(format!("pointer path {:?} is not inside its directory", file.path));
            }
            if !is_sha256(&file.sha256) || !is_sha256(&file.unpacked_sha256) {
                return Err(format!("pointer digests of {} are not SHA-256", file.name));
            }
            if file.size == 0 || file.size > FILE_LIMIT || file.unpacked_size == 0 || file.unpacked_size > FILE_LIMIT {
                return Err(format!("pointer sizes of {} are out of range", file.name));
            }
        }
        Ok(())
    }

    /// The SHA-256 that names this build: its first file's, unpacked.
    pub fn id(&self) -> &str {
        &self.files[0].unpacked_sha256
    }
}

/// A file name with no directory in it: `[A-Za-z0-9._-]`, not starting with a dot.
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// `a/b/c` made of plain names only.
fn is_relative_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= 512 && path.split('/').all(is_plain_name)
}

pub fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// A client for pointers and registry documents: small answers, short waits,
/// HTTPS-only redirects.
pub fn small_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("Mewrk/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 5 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| ui_text!("无法创建下载客户端: {error}", "Could not set up the download client: {error}"))
}

/// GETs a JSON document of at most `limit` bytes. `Ok(None)` when the server
/// says there is none (404). Any other refusal — a 403 from a firewall's
/// challenge or a bucket's permissions — is an error naming the status: taking
/// it for "nothing published" would hide what is wrong.
pub fn fetch_json<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
    accept: Option<&str>,
    limit: usize,
) -> Result<Option<T>, String> {
    let mut request = client.get(url);
    if let Some(accept) = accept {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let response = request
        .send()
        .map_err(|error| ui_text!("无法连接 {host}: {error}", "Could not connect to {host}: {error}"))?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(ui_text!("{host} 返回 HTTP {status}", "{host} answered HTTP {status}"));
    }
    let mut body = Vec::new();
    response
        .take(limit as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|error| ui_text!("读取 {host} 的应答失败: {error}", "Could not read {host}'s answer: {error}"))?;
    if body.len() > limit {
        return Err(ui_text!("{host} 的应答过大", "{host}'s answer is too large"));
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| ui_text!("{host} 的应答无法解析: {error}", "Could not parse {host}'s answer: {error}"))
}

/// The pointer at `<base>/<key>`, if one is published and this host can read
/// it. `base` is [`channel_base`] (tests serve a channel of their own); `key`
/// is like `aisdk/p17/x86_64-pc-windows-msvc.json`.
pub fn fetch_pointer_from(base: &str, key: &str, component: &str, triple: &str) -> Result<Option<Pointer>, String> {
    let client = small_client()?;
    let url = format!("{base}/{key}");
    let Some(pointer) = fetch_json::<Pointer>(&client, &url, None, POINTER_LIMIT)? else {
        return Ok(None);
    };
    if pointer.schema != POINTER_SCHEMA {
        eprintln!("[components] {url}: schema {} is not {POINTER_SCHEMA}; ignoring it", pointer.schema);
        return Ok(None);
    }
    pointer.validate(component, triple).map_err(|error| format!("{url}: {error}"))?;
    Ok(Some(pointer))
}

/// Installs every file of `pointer` into `dest`, a directory of this build's
/// own: each gzipped file is downloaded (resumably, into `dest/.download`),
/// checked, unpacked beside it as `<name>`, checked again and made executable.
/// A file already in `dest` with the right size and SHA-256 is kept.
/// `base` is [`channel_base`] (tests serve a channel of their own);
/// `pointer_dir` is the channel key of the pointer's directory, like
/// `aisdk/p17`.
pub fn install_files_from(
    base: &str,
    pointer_dir: &str,
    pointer: &Pointer,
    dest: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|error| folder_failed(dest, error))?;
    let pending: Vec<&PointerFile> = pointer
        .files
        .iter()
        .filter(|file| !is_installed(&dest.join(&file.name), file.unpacked_size, &file.unpacked_sha256))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let staging = dest.join(".download");
    let remote: Vec<RemoteFile> = pending
        .iter()
        .map(|file| RemoteFile {
            remote: format!("{pointer_dir}/{}", file.path),
            local: format!("{}.gz", file.name),
            size: file.size,
            sha256: file.sha256.clone(),
            url: None,
        })
        .collect();
    download::download(&staging, &remote, &Source::at("mewrk", base), cancel, progress)?;
    for file in pending {
        let packed = staging.join(format!("{}.gz", file.name));
        let target = dest.join(&file.name);
        gunzip_verified(&packed, &target, file.unpacked_size, &file.unpacked_sha256)?;
    }
    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

/// Whether `path` holds exactly `size` bytes hashing to `sha256`.
pub fn is_installed(path: &Path, size: u64, sha256: &str) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() == size) && verified_file(path, sha256)
}

/// The files this process has hashed, with the size and modification time
/// they had then and the digest they hashed to.
static VERIFIED: Mutex<Option<HashMap<PathBuf, (u64, SystemTime, String)>>> = Mutex::new(None);

/// The size and modification time of the file at `path`.
fn stamp(path: &Path) -> Option<(u64, SystemTime)> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?)).filter(|_| metadata.is_file())
}

/// Whether the file at `path` hashes to `sha256` (lowercase hex).
///
/// An executable in the components folder, which the user's own programs can
/// write, is checked with this before it is run. The file is hashed once per
/// process — the sidecar is ~92 MiB, the Claude Code CLI ~250 MiB — and again
/// only when its size or modification time has changed since.
pub fn verified_file(path: &Path, sha256: &str) -> bool {
    let Some(before) = stamp(path) else {
        return false;
    };
    let known = VERIFIED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|verified| verified.get(path).cloned());
    if let Some((size, modified, digest)) = known {
        if (size, modified) == before {
            return digest == sha256;
        }
    }
    let Ok(digest) = download::sha256_file(path) else {
        return false;
    };
    // Kept only when the file did not change while it was read.
    if stamp(path) != Some(before) {
        return false;
    }
    let matches = digest == sha256;
    VERIFIED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), (before.0, before.1, digest));
    matches
}

/// Records that the file at `path`, as it is now, hashes to `sha256`: the
/// installer has just checked the bytes it wrote there, so their first use
/// need not hash them again.
pub fn remember_verified(path: &Path, sha256: &str) {
    if let Some((size, modified)) = stamp(path) {
        VERIFIED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .insert(path.to_path_buf(), (size, modified, sha256.to_owned()));
    }
}

/// How long [`with_patience`] keeps trying.
const PATIENCE: Duration = if cfg!(test) { Duration::from_millis(600) } else { Duration::from_secs(15) };

/// Runs `attempt` — moving or removing a file or directory just written —
/// until it succeeds, for up to ~15 s, waiting longer between tries each
/// time. On Windows a virus scanner or the search indexer opens a new
/// executable as soon as it appears and holds it for a moment, and a rename
/// or a removal fails while it does. An error no wait would change — the
/// source is gone (`NotFound`), the destination is taken (`AlreadyExists`) —
/// ends it at once.
pub fn with_patience<T>(mut attempt: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let until = Instant::now() + PATIENCE;
    let mut delay = Duration::from_millis(50);
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists)
                    || Instant::now() + delay > until =>
            {
                return Err(error);
            }
            Err(_) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Unpacks the gzip file `packed` to `target` through a temporary name,
/// refusing anything but `size` bytes hashing to `sha256`, and makes the result
/// executable.
pub fn gunzip_verified(packed: &Path, target: &Path, size: u64, sha256: &str) -> Result<(), String> {
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let partial = target.with_file_name(format!(".{name}.partial"));
    let result = (|| {
        let input = File::open(packed).map_err(|error| write_failed(&name, error))?;
        let mut decoder = flate2::read::GzDecoder::new(std::io::BufReader::new(input)).take(size + 1);
        let mut output = File::create(&partial).map_err(|error| write_failed(&name, error))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut written = 0u64;
        loop {
            let read = decoder
                .read(&mut buffer)
                .map_err(|error| ui_text!("{name} 解压失败: {error}", "Could not unpack {name}: {error}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read]).map_err(|error| write_failed(&name, error))?;
            written += read as u64;
        }
        output.sync_all().map_err(|error| write_failed(&name, error))?;
        drop(output);
        let digest: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        if written != size || digest != sha256 {
            return Err(ui_text!(
                "{name} 校验失败（解压后的大小或 sha256 不符），请重试",
                "{name} failed verification (its unpacked size or sha256 does not match); try again"
            ));
        }
        make_executable(&partial);
        replace_file(&partial, target).map_err(|error| {
            ui_text!("无法放置 {name}: {error}", "Could not move {name} into place: {error}")
        })?;
        remember_verified(target, sha256);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// Moves `from` over `to`, patiently ([`with_patience`]). A file still running
/// (Windows) cannot be replaced; that is the caller's to avoid by giving every
/// build a directory of its own.
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    with_patience(|| match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) if to.exists() && from.exists() => {
            fs::remove_file(to)?;
            fs::rename(from, to)
        }
        Err(error) => Err(error),
    })
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

/// Reads a JSON file this host wrote; `None` when absent or unreadable.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Writes `value` to `path` through a temporary file, so a reader sees the old
/// document or the new one, never half of either.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path.parent().ok_or_else(|| format!("{} has no directory", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| folder_failed(parent, error))?;
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let partial = parent.join(format!(".{name}.{}.partial", std::process::id()));
    let text = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    fs::write(&partial, text).map_err(|error| write_failed(&name, error))?;
    replace_file(&partial, path).map_err(|error| {
        let _ = fs::remove_file(&partial);
        write_failed(&name, error)
    })
}

/// Removes every entry of `dir` whose name `keep` refuses. Entries in use
/// (a running executable on Windows) stay until a later start.
pub fn prune_dir(dir: &Path, keep: impl Fn(&str) -> bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep(&name) {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
        if let Err(error) = removed {
            eprintln!("[components] could not remove {}: {error}", path.display());
        }
    }
}

fn folder_failed(dir: &Path, error: std::io::Error) -> String {
    let dir = dir.display();
    ui_text!("无法创建目录 {dir}: {error}", "Could not create the folder {dir}: {error}")
}

fn write_failed(name: &str, error: std::io::Error) -> String {
    ui_text!("无法写入 {name}: {error}", "Could not write {name}: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer() -> Pointer {
        serde_json::from_value(serde_json::json!({
            "schema": 1,
            "component": "aisdk",
            "triple": "x86_64-pc-windows-msvc",
            "version": "1.2.4",
            "builtAt": "2026-10-07T00:00:00Z",
            "protocol": 17,
            "files": [{
                "name": "mewrk-aisdk.exe",
                "path": "x86_64-pc-windows-msvc/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/mewrk-aisdk.exe.gz",
                "size": 10,
                "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "unpackedSize": 20,
                "unpackedSha256": "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
            }]
        }))
        .expect("pointer parses")
    }

    #[test]
    fn a_pointer_keeps_what_its_component_adds() {
        let pointer = pointer();
        assert_eq!(pointer.extra.get("protocol"), Some(&serde_json::json!(17)));
        assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_ok());
        assert_eq!(pointer.id(), "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210");
    }

    #[test]
    fn a_pointer_cannot_reach_outside_its_directory_or_be_for_another_build() {
        assert!(pointer().validate("aisdk", "aarch64-apple-darwin").is_err());
        assert!(pointer().validate("remote-agent", "x86_64-pc-windows-msvc").is_err());
        for path in ["../x.gz", "a/../x.gz", "/x.gz", "a//x.gz", ".hidden/x.gz", "a\\x.gz", ""] {
            let mut pointer = pointer();
            pointer.files[0].path = path.into();
            assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_err(), "{path}");
        }
        for name in ["", "..", ".x", "a/b", "a\\b", "C:x"] {
            let mut pointer = pointer();
            pointer.files[0].name = name.into();
            assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_err(), "{name}");
        }
        let mut pointer = pointer();
        pointer.files[0].sha256 = "ABC".into();
        assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_err());
    }

    #[test]
    fn two_files_whose_names_differ_only_in_case_are_one_file() {
        let mut pointer = pointer();
        let mut twin = pointer.files[0].clone();
        twin.name = "MEWRK-AISDK.EXE".into();
        pointer.files.push(twin.clone());
        assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_err());
        pointer.files[1].name = "srt-win.exe".into();
        assert!(pointer.validate("aisdk", "x86_64-pc-windows-msvc").is_ok());
    }

    #[test]
    fn a_release_takes_only_a_secure_or_local_override() {
        let secure_or_local =
            ["https://mirror.example/components", "http://127.0.0.1:8080", "http://[::1]:9", "http://localhost:3000"];
        for base in secure_or_local {
            assert!(acceptable_override(base, true), "{base}");
        }
        let elsewhere = [
            "http://mirror.example",
            "http://10.0.0.1",
            "http://127.0.0.1.example.com",
            "file:///c:/x",
            "ftp://x",
            "not a url",
        ];
        for base in elsewhere {
            assert!(!acceptable_override(base, true), "{base}");
        }
        // A development build points it wherever its developer likes.
        assert!(acceptable_override("http://mirror.example", false));
        assert!(!acceptable_override("file:///c:/x", false));
    }

    /// A one-request server answering `status` to whatever is asked.
    fn answer_once(status: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_owned();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());
            }
        });
        base
    }

    #[test]
    fn only_a_404_means_nothing_is_published() {
        let client = small_client().unwrap();
        let fetch = |status: &str| {
            let url = format!("{}/x.json", answer_once(status));
            fetch_json::<serde_json::Value>(&client, &url, None, 1024)
        };
        assert_eq!(fetch("404 Not Found"), Ok(None));
        let refused = fetch("403 Forbidden").unwrap_err();
        assert!(refused.contains("403"), "{refused}");
    }

    #[test]
    fn patience_outlasts_a_passing_failure_but_not_a_missing_source() {
        let mut tries = 0;
        let value = with_patience(|| {
            tries += 1;
            if tries < 3 {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            } else {
                Ok(tries)
            }
        });
        assert_eq!(value.unwrap(), 3);

        let mut tries = 0;
        let missing = with_patience(|| -> std::io::Result<()> {
            tries += 1;
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        });
        assert!(missing.is_err());
        assert_eq!(tries, 1, "a missing source is not waited for");

        let started = Instant::now();
        let held = with_patience(|| -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert!(held.is_err());
        assert!(started.elapsed() < PATIENCE + Duration::from_secs(2), "{:?}", started.elapsed());
    }

    #[test]
    fn a_file_is_hashed_again_once_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        fs::write(&path, b"the real tool").unwrap();
        let sha = format!("{:x}", Sha256::digest(b"the real tool"));
        assert!(verified_file(&path, &sha));
        assert!(verified_file(&path, &sha), "the second answer comes from what was recorded");
        assert!(!verified_file(&path, &"0".repeat(64)));
        // Replaced by other bytes of another size: refused.
        fs::write(&path, b"a tampered tool!").unwrap();
        assert!(!verified_file(&path, &sha));
        assert!(!verified_file(&dir.path().join("missing"), &sha));
    }

    #[test]
    fn unpacking_refuses_bytes_other_than_the_pointer_says() {
        let dir = tempfile::tempdir().expect("temp dir");
        let packed = dir.path().join("x.gz");
        let mut encoder = flate2::write::GzEncoder::new(File::create(&packed).unwrap(), flate2::Compression::default());
        encoder.write_all(b"hello agent").unwrap();
        encoder.finish().unwrap();
        let sha = format!("{:x}", Sha256::digest(b"hello agent"));
        let target = dir.path().join("agent");
        assert!(gunzip_verified(&packed, &target, 11, &"0".repeat(64)).is_err());
        assert!(!target.exists());
        assert!(gunzip_verified(&packed, &target, 10, &sha).is_err());
        gunzip_verified(&packed, &target, 11, &sha).expect("unpacks");
        assert_eq!(fs::read(&target).unwrap(), b"hello agent");
        assert!(is_installed(&target, 11, &sha));
    }
}
