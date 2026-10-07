//! The Claude Agent SDK and the Claude Code CLI it drives, installed from npm
//! and updated by the user on the Claude Agent provider page.
//!
//! Mewrk carries neither. The SDK (`@anthropic-ai/claude-agent-sdk`, the
//! JavaScript the sidecar loads from disk) and the CLI (the SDK's platform
//! package, `@anthropic-ai/claude-agent-sdk-<platform>`, one native
//! `claude[.exe]`) are fetched when the user installs them and replaced when
//! the user updates them; both packages are published together under one
//! version, so they cannot disagree.
//!
//! Which releases are offered is the sidecar's to say. It is built against one
//! SDK release (its pointer's `claudeAgentSdk`, else the pin this host was
//! built with), and only that release's caret range — `^0.3.284` — keeps the
//! interface the sidecar was written for. Prereleases are never offered.
//!
//! # Where from
//!
//! `registry.npmjs.org`, then `registry.npmmirror.com` (the mainland China
//! mirror, serving the same tarballs) when the first cannot be reached or is
//! failing: a refusal or a missing version is an answer, not a reason to ask
//! the mirror. `MEWRK_NPM_REGISTRY` replaces both (a release build takes only
//! an HTTPS registry, or one on this computer: [`super::override_base`]).
//!
//! # Trust
//!
//! Whichever registry answers, what it says of a version is believed only
//! when npm signed it: every version document must carry a valid
//! `dist.signatures` entry by npm's registry key ([`NPM_KEY_SPKI`]) over
//! `<name>@<version>:<dist.integrity>`, which the mirror passes on unchanged.
//! Each tarball is then checked against that SHA-512 (`dist.integrity`), the
//! CLI against the SHA-256 and size the SDK's own `manifest.json` lists for
//! this platform, and the SDK's `package.json` must name the version asked
//! for. The CLI's SHA-256 is recorded in `install.json`, and the installed CLI
//! is hashed against it once per process before it is first run
//! ([`super::verified_file`]).
//!
//! # Layout
//!
//! ```text
//! <components>/claude-agent/current.json          {"sdkVersion": "0.3.292"}
//! <components>/claude-agent/0.3.292/sdk/…          the SDK tarball's package/
//! <components>/claude-agent/0.3.292/claude[.exe]
//! <components>/claude-agent/0.3.292/install.json   {sdkVersion, claudeCodeVersion, claudeSha256, installedAt}
//! <components>/claude-agent/.download/             tarballs while installing
//! ```
//!
//! A version is unpacked into a staging directory and renamed into place, and
//! `current.json` switches only once everything checked out, so a version half
//! installed is never current. The previous version stays — a CLI still
//! running may be using it — as does every version this process has run, and
//! older ones are removed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};
use semver::{Comparator, Op, Prerelease, Version, VersionReq};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use crate::host_platform::host_platform;
use crate::ui_text::ui_text;

/// The npm registries tried in order.
pub const REGISTRIES: &[&str] = &["https://registry.npmjs.org", "https://registry.npmmirror.com"];
/// A registry base URL that replaces [`REGISTRIES`].
pub const REGISTRY_ENV: &str = "MEWRK_NPM_REGISTRY";
/// The key npm signs every version it publishes with, as
/// <https://registry.npmjs.org/-/npm/v1/keys> lists it (the one without an
/// expiry; the other expired on 2025-01-29): its id, and the ECDSA P-256 public
/// key as base64 SubjectPublicKeyInfo DER.
const NPM_KEY_ID: &str = "SHA256:DhQ8wR5APBvFHLF/+Tc+AYvPOdTpcIDqOhxsBHRwC7U";
const NPM_KEY_SPKI: &str =
    "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEY6Ya7W++7aUPzvMTrezH6Ycx3c+HOKYCcNGybJZSCJq/fd7Qa8uuAKtdIkUQtQiEKERhAmE5lMMJhP8OkDOa2g==";
/// The SDK's package; the CLI's is this plus `-<platform>`.
const SDK_PACKAGE: &str = "@anthropic-ai/claude-agent-sdk";
/// The directory under the components root.
const DIR: &str = "claude-agent";
const CURRENT: &str = "current.json";
const INSTALL_RECORD: &str = "install.json";
const DOWNLOADS: &str = ".download";
const STAGING_PREFIX: &str = ".staging-";
/// The abbreviated packument: every version, without READMEs.
const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";
const TAGS_LIMIT: usize = 64 * 1024;
const VERSION_DOC_LIMIT: usize = 1 << 20;
/// The SDK's abbreviated packument is ~400 KB with ~300 versions.
const PACKUMENT_LIMIT: usize = 32 << 20;
/// The CLI's tarball is ~80–100 MB.
const TARBALL_LIMIT: u64 = 512 << 20;
/// The SDK unpacks to ~5 MB.
const SDK_UNPACKED_LIMIT: u64 = 128 << 20;
const SDK_ENTRY_LIMIT: usize = 10_000;
/// The CLI unpacks to ~250 MB.
const CLI_LIMIT: u64 = 1 << 30;
/// Failed attempts in a row, with no byte gained, before a tarball gives up
/// on its registry.
const ATTEMPTS: u32 = 4;
/// The first wait before a retry; it doubles each time.
const RETRY_DELAY: Duration = if cfg!(test) { Duration::from_millis(10) } else { Duration::from_secs(1) };
/// The blocking client applies its timeout to each read, so this is how long
/// a transfer may go without a byte before it counts as dropped.
const STALL: Duration = Duration::from_secs(60);
const CHUNK: usize = 256 * 1024;
/// What a cancelled install ends with. Never shown: a cancel is not a failure.
const CANCELLED: &str = "cancelled";

/// Overrides the CLI in development, for driving another build against the
/// fixtures. Debug builds only.
#[cfg(debug_assertions)]
const EXECUTABLE_ENV: &str = "MEWRK_CLAUDE_BIN";

/// The SDK and the CLI a Claude Agent step runs with.
// The versions are read where a development build reports its source-tree copy.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[derive(Clone, Debug)]
pub struct Runtime {
    /// `claude[.exe]`.
    pub cli: PathBuf,
    /// The SDK's root entry, `sdk.mjs`, which the sidecar loads from disk.
    pub sdk_entry: PathBuf,
    pub sdk_version: String,
    pub claude_code_version: Option<String>,
}

/// The installed SDK and CLI (in a development build, the source tree's when
/// none is installed).
///
/// An installed version outside the range the sidecar takes ([`floor`]) is
/// refused, as is a CLI that is no longer the file that was installed: what
/// the user is told says how to put either right.
pub fn runtime() -> Result<Runtime, String> {
    let dir = super::root().map(|root| root.join(DIR));
    let runtime = runtime_in(dir.as_deref(), &floor())?;
    #[cfg(debug_assertions)]
    let runtime = with_executable_override(runtime)?;
    Ok(runtime)
}

/// [`runtime`] from `dir` (`<components>/claude-agent`), for a sidecar built
/// against `floor`.
fn runtime_in(dir: Option<&Path>, floor: &Version) -> Result<Runtime, String> {
    let Some((root, record)) = dir.and_then(current_install) else {
        #[cfg(debug_assertions)]
        if let Some(runtime) = development_runtime() {
            return Ok(runtime);
        }
        return Err(not_installed());
    };
    if !is_compatible(&record.sdk_version, floor) {
        let (installed, range) = (&record.sdk_version, compatible_range(floor));
        return Err(ui_text!(
            "已安装的 Claude Agent 组件是 {installed} 版，而这个版本的 Mewrk 的 AI SDK 组件需要 {range}：请在「设置 → 提供商 → 模型提供商 → Claude Agent」里更新它们。",
            "The installed Claude Agent components are version {installed}, and this Mewrk's AI SDK component needs {range}: update them under Settings → Providers → Model providers → Claude Agent."
        ));
    }
    if !cli_verified(&root, &record) {
        let path = root.join(cli_name());
        let path = path.display();
        return Err(ui_text!(
            "已安装的 Claude Code（{path}）与安装时记录的不一致，可能被改动过，Mewrk 不会运行它：请在「设置 → 提供商 → 模型提供商 → Claude Agent」里重新安装 Claude Agent 组件。",
            "The installed Claude Code ({path}) is not the file that was installed and may have been altered, so Mewrk will not run it: install the Claude Agent components again under Settings → Providers → Model providers → Claude Agent."
        ));
    }
    remember_in_use(&root);
    Ok(Runtime {
        cli: root.join(cli_name()),
        sdk_entry: root.join("sdk").join("sdk.mjs"),
        sdk_version: record.sdk_version,
        claude_code_version: record.claude_code_version,
    })
}

/// Whether [`runtime`] has something to run.
pub fn is_installed() -> bool {
    runtime().is_ok()
}

/// What every Claude Agent action says while there is nothing to run.
pub fn not_installed() -> String {
    ui_text!(
        "Claude Agent 组件还没有安装：在「设置 → 提供商 → 模型提供商 → Claude Agent」里安装。",
        "The Claude Agent components are not installed yet: install them under Settings → Providers → Model providers → Claude Agent."
    )
}

/// The version directories [`runtime`] has handed out in this process. A
/// session keeps running the CLI it started with for as long as it lasts, so
/// none of these is removed while this process runs ([`switch_to`]); a switch
/// after the next start removes them.
static IN_USE: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

fn remember_in_use(root: &Path) {
    IN_USE.lock().unwrap_or_else(PoisonError::into_inner).insert(root.to_path_buf());
}

fn in_use(root: &Path) -> bool {
    IN_USE.lock().unwrap_or_else(PoisonError::into_inner).contains(root)
}

/// The npm platform of this build, as the CLI's package names it: `win32-x64`,
/// `darwin-arm64`, `linux-x64-musl`… A musl build runs the musl CLI: the
/// target environment, not the machine, decides which C library is there.
pub fn platform_tag() -> String {
    let arch = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };
    let libc = if cfg!(target_env = "musl") { "-musl" } else { "" };
    format!("{}-{arch}{libc}", host_platform().npm_platform_tag())
}

/// The CLI's file name here. Only the native build: the npm `claude.cmd` and
/// `cli.js` shims need a Node runtime the single-file sidecar cannot provide.
fn cli_name() -> String {
    format!("claude{}", host_platform().executable_suffix())
}

fn cli_package(platform: &str) -> String {
    format!("{SDK_PACKAGE}-{platform}")
}

/// What the provider page shows.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAgentComponentStatus {
    pub installed: Option<InstalledVersion>,
    /// The caret range the sidecar takes: `^0.3.284`.
    pub compatible: String,
    /// The newest compatible release on npm. `None` when not checked (this
    /// run has not asked yet) or when the check failed.
    pub latest: Option<AvailableVersion>,
    /// Why the check failed, for people.
    pub latest_error: Option<String>,
    /// The newest release outside [`Self::compatible`], when newer than
    /// [`Self::latest`]: it needs a newer Mewrk.
    pub newer_incompatible: Option<String>,
    /// Installed, and [`Self::latest`] is newer — or the installed version is
    /// not compatible and `latest` is (the "update" may then be to an older
    /// version: the newest this Mewrk takes).
    pub update_available: bool,
    pub task: Option<TaskStatus>,
    /// The last install's failure; cleared when another starts.
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledVersion {
    pub sdk_version: String,
    pub claude_code_version: Option<String>,
    pub source: InstalledSource,
    /// RFC 3339; `None` for the source tree.
    pub installed_at: Option<String>,
    /// Whether the version is in [`ClaudeAgentComponentStatus::compatible`]:
    /// one that is not cannot run until it is updated. Always true for the
    /// source tree, which is what the sidecar was built against.
    pub compatible: bool,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InstalledSource {
    /// Installed from npm on the provider page.
    Installed,
    /// A development build running the source tree's `node_modules`.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    Development,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableVersion {
    pub sdk_version: String,
    pub claude_code_version: Option<String>,
}

/// The install in progress.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub action: TaskAction,
    /// Empty until the newest compatible release is known, when the install
    /// was not asked for a version and nothing was checked before.
    pub sdk_version: String,
    pub phase: Phase,
    pub received_bytes: u64,
    /// Known once the server has said: the CLI's size first, the SDK's added
    /// when its download starts.
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TaskAction {
    Install,
    Update,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    /// Asking npm which version, and for its tarballs.
    Resolving,
    Downloading,
    /// Unpacking into staging, checking the CLI and the SDK's own version.
    Verifying,
    /// Moving the version into place and switching to it.
    Installing,
}

/// `current.json`.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentRecord {
    sdk_version: String,
}

/// `<version>/install.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallRecord {
    sdk_version: String,
    #[serde(default)]
    claude_code_version: Option<String>,
    /// The SHA-256 of `claude[.exe]` as installed, which it is checked against
    /// before it runs. A record without it is of a CLI nothing vouches for.
    #[serde(default)]
    claude_sha256: Option<String>,
    #[serde(default)]
    installed_at: Option<String>,
}

/// A package's `package.json`, as far as it is read.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackageJson {
    name: String,
    version: String,
    #[serde(default)]
    claude_code_version: Option<String>,
}

/// The SDK's `manifest.json`: every platform's CLI build.
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    platforms: BTreeMap<String, ManifestBuild>,
}

#[derive(Debug, Deserialize)]
struct ManifestBuild {
    /// SHA-256 of the unpacked executable.
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

/// One version of one package, as the registry describes it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionDoc {
    name: String,
    version: String,
    #[serde(default)]
    claude_code_version: Option<String>,
    dist: Dist,
}

#[derive(Debug, Deserialize)]
struct Dist {
    tarball: String,
    #[serde(default)]
    integrity: Option<String>,
    /// npm's signatures over `<name>@<version>:<integrity>`.
    #[serde(default)]
    signatures: Vec<DistSignature>,
}

#[derive(Debug, Deserialize)]
struct DistSignature {
    keyid: String,
    /// Base64 of the DER-encoded ECDSA signature.
    sig: String,
}

/// The abbreviated packument, as far as it is read.
#[derive(Debug, Deserialize)]
struct Packument {
    #[serde(default)]
    versions: BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Versions

/// A release version: no prerelease, no build metadata.
fn parse_release(text: &str) -> Option<Version> {
    let version = Version::parse(text.trim()).ok()?;
    (version.pre.is_empty() && version.build.is_empty()).then_some(version)
}

/// `^<floor>`.
fn compatible_range(floor: &Version) -> VersionReq {
    VersionReq {
        comparators: vec![Comparator {
            op: Op::Caret,
            major: floor.major,
            minor: Some(floor.minor),
            patch: Some(floor.patch),
            pre: Prerelease::EMPTY,
        }],
    }
}

/// The SDK release the sidecar was built against: the installed sidecar's,
/// else the one this host was built with.
fn floor() -> Version {
    super::aisdk::claude_agent_sdk_version().as_deref().and_then(parse_release).unwrap_or_else(pinned_floor)
}

fn pinned_floor() -> Version {
    parse_release(env!("MEWRK_CLAUDE_AGENT_SDK_VERSION")).unwrap_or(Version::new(0, 0, 0))
}

/// What npm has for this host: the newest compatible release, and the newest
/// release beyond it that this host cannot take.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Selection {
    latest: Option<Version>,
    newer_incompatible: Option<Version>,
}

fn select(versions: &[Version], floor: &Version) -> Selection {
    let range = compatible_range(floor);
    let releases = || versions.iter().filter(|version| version.pre.is_empty());
    let latest = releases().filter(|version| range.matches(version)).max().cloned();
    let newer_incompatible = releases()
        .filter(|version| !range.matches(version))
        .max()
        .filter(|version| *version > latest.as_ref().unwrap_or(floor))
        .cloned();
    Selection { latest, newer_incompatible }
}

/// Whether the installed `version` is one a sidecar built against `floor` takes.
fn is_compatible(version: &str, floor: &Version) -> bool {
    Version::parse(version).is_ok_and(|version| compatible_range(floor).matches(&version))
}

/// `text` as a release in the compatible range, or why not.
fn compatible_version(text: &str, floor: &Version) -> Result<Version, String> {
    let range = compatible_range(floor);
    let text = text.trim();
    match parse_release(text) {
        Some(version) if range.matches(&version) => Ok(version),
        Some(_) => Err(ui_text!(
            "Claude Agent SDK {text} 与这个版本的 Mewrk 不兼容（需要 {range}）",
            "Claude Agent SDK {text} does not work with this Mewrk (it needs {range})"
        )),
        None => Err(ui_text!(
            "{text} 不是 Claude Agent SDK 的正式版本号",
            "{text} is not a Claude Agent SDK release version"
        )),
    }
}

// ---------------------------------------------------------------------------
// npm

/// How a request to a registry failed.
#[derive(Debug)]
enum Failure {
    /// The registry could not be reached, timed out or is failing (5xx):
    /// another registry may do better.
    Unreachable(String),
    /// An answer, or a failure no other registry would change.
    Fatal(String),
}

/// A key registry documents are signed with.
#[derive(Clone, Debug)]
struct RegistryKey {
    /// As `dist.signatures[].keyid` names it.
    id: String,
    /// The P-256 public key: an uncompressed point, 65 bytes.
    point: Vec<u8>,
}

impl RegistryKey {
    /// npm's ([`NPM_KEY_SPKI`]). The SubjectPublicKeyInfo of a P-256 key ends
    /// with its point.
    fn npm() -> Self {
        let spki = base64::engine::general_purpose::STANDARD.decode(NPM_KEY_SPKI).expect("npm's key is base64");
        Self { id: NPM_KEY_ID.to_owned(), point: spki[spki.len() - 65..].to_vec() }
    }
}

#[derive(Clone, Debug)]
struct Npm {
    registries: Vec<String>,
    /// Whose signature makes a version document believed: npm's, always,
    /// whatever the registries; tests sign their fixtures with a key of their
    /// own.
    keys: Vec<RegistryKey>,
}

impl Npm {
    fn from_env() -> Self {
        let registries = match super::override_base(REGISTRY_ENV) {
            Some(base) => vec![base],
            None => REGISTRIES.iter().map(|base| (*base).to_owned()).collect(),
        };
        Self { registries, keys: vec![RegistryKey::npm()] }
    }

    /// Runs `attempt` against each registry in turn until one answers.
    fn first<T>(&self, mut attempt: impl FnMut(&Registry) -> Result<T, Failure>) -> Result<T, String> {
        let mut last = None;
        for base in &self.registries {
            match attempt(&Registry { base, keys: &self.keys }) {
                Ok(value) => return Ok(value),
                Err(Failure::Unreachable(error)) => {
                    eprintln!("[claude-agent] {base}: {error}");
                    last = Some(error);
                }
                Err(Failure::Fatal(error)) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| ui_text!("没有可用的 npm 源", "No npm registry is configured")))
    }
}

struct Registry<'a> {
    base: &'a str,
    /// [`Npm::keys`].
    keys: &'a [RegistryKey],
}

impl Registry<'_> {
    /// `<base>/<scope>%2f<name>`, the spelling npm itself uses.
    fn package_url(&self, package: &str) -> String {
        format!("{}/{}", self.base, package.replace('/', "%2f"))
    }

    fn latest_tag(&self, client: &reqwest::blocking::Client) -> Result<Option<Version>, Failure> {
        let url = format!("{}/-/package/{}/dist-tags", self.base, SDK_PACKAGE.replace('/', "%2f"));
        let tags: BTreeMap<String, Value> =
            get_json(client, &url, None, TAGS_LIMIT)?.ok_or_else(|| not_on_npm(SDK_PACKAGE))?;
        Ok(tags.get("latest").and_then(Value::as_str).and_then(parse_release))
    }

    /// Every release of the SDK that is not deprecated.
    fn releases(&self, client: &reqwest::blocking::Client) -> Result<Vec<Version>, Failure> {
        let url = self.package_url(SDK_PACKAGE);
        let packument: Packument =
            get_json(client, &url, Some(ABBREVIATED), PACKUMENT_LIMIT)?.ok_or_else(|| not_on_npm(SDK_PACKAGE))?;
        Ok(packument
            .versions
            .iter()
            // npm writes the reason; an empty one (or `false`) is a deprecation undone.
            .filter(|(_, doc)| {
                doc.get("deprecated")
                    .is_none_or(|reason| reason.is_null() || reason == false || reason.as_str() == Some(""))
            })
            .filter_map(|(version, _)| parse_release(version))
            .collect())
    }

    fn version_doc(
        &self,
        client: &reqwest::blocking::Client,
        package: &str,
        version: &Version,
    ) -> Result<VersionDoc, Failure> {
        let url = format!("{}/{version}", self.package_url(package));
        let doc: VersionDoc = get_json(client, &url, None, VERSION_DOC_LIMIT)?
            .ok_or_else(|| not_on_npm(&format!("{package}@{version}")))?;
        if doc.name != package || doc.version != version.to_string() {
            let (name, found) = (&doc.name, &doc.version);
            return Err(Failure::Fatal(ui_text!(
                "npm 源为 {package}@{version} 返回了 {name}@{found}",
                "The npm registry answered {name}@{found} for {package}@{version}"
            )));
        }
        if !signed_by(&doc, self.keys) {
            let host = host_of(self.base);
            return Err(Failure::Fatal(ui_text!(
                "{host} 给出的 {package}@{version} 没有 npm 的有效签名，Mewrk 不会使用它",
                "What {host} says of {package}@{version} carries no valid npm signature, so Mewrk will not use it"
            )));
        }
        Ok(doc)
    }

    /// The newest compatible release and what is beyond it. `dist-tags.latest`
    /// answers when it is compatible; the whole version list is read only when
    /// it is not.
    fn select(&self, client: &reqwest::blocking::Client, floor: &Version) -> Result<Selection, Failure> {
        if let Some(latest) = self.latest_tag(client)? {
            if compatible_range(floor).matches(&latest) {
                return Ok(Selection { latest: Some(latest), newer_incompatible: None });
            }
        }
        Ok(select(&self.releases(client)?, floor))
    }

    /// [`Self::select`] with the newest compatible release's Claude Code version.
    fn check(&self, client: &reqwest::blocking::Client, floor: &Version) -> Result<LatestCheck, Failure> {
        let selection = self.select(client, floor)?;
        let latest = match &selection.latest {
            Some(version) => Some(AvailableVersion {
                sdk_version: version.to_string(),
                claude_code_version: self.version_doc(client, SDK_PACKAGE, version)?.claude_code_version,
            }),
            None => None,
        };
        Ok(LatestCheck { latest, newer_incompatible: selection.newer_incompatible.map(|version| version.to_string()) })
    }
}

fn not_on_npm(what: &str) -> Failure {
    Failure::Fatal(ui_text!("npm 上没有 {what}", "{what} is not on npm"))
}

/// Whether `doc` carries a valid signature by one of `keys` over
/// `<name>@<version>:<dist.integrity>`: what npm signs for every version it
/// publishes, so a registry passing it on (a mirror) cannot change the
/// tarball's digest without it showing.
fn signed_by(doc: &VersionDoc, keys: &[RegistryKey]) -> bool {
    let Some(integrity) = doc.dist.integrity.as_deref() else {
        return false;
    };
    let message = format!("{}@{}:{integrity}", doc.name, doc.version);
    doc.dist.signatures.iter().any(|signature| {
        let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(signature.sig.trim()) else {
            return false;
        };
        keys.iter().filter(|key| key.id == signature.keyid).any(|key| {
            UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &key.point).verify(message.as_bytes(), &sig).is_ok()
        })
    })
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default()
}

/// A status worth asking again, or asking another registry: the server is
/// failing or overloaded rather than answering.
fn is_passing(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
}

/// `components::fetch_json`, telling a registry that cannot be reached from
/// one that answered. `Ok(None)` for a 404.
fn get_json<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
    accept: Option<&str>,
    limit: usize,
) -> Result<Option<T>, Failure> {
    let host = host_of(url);
    let mut request = client.get(url);
    if let Some(accept) = accept {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    let response = request.send().map_err(|error| {
        Failure::Unreachable(ui_text!("无法连接 {host}: {error}", "Could not connect to {host}: {error}"))
    })?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        let error = ui_text!("{host} 返回 HTTP {status}", "{host} answered HTTP {status}");
        return Err(if is_passing(status) { Failure::Unreachable(error) } else { Failure::Fatal(error) });
    }
    let mut body = Vec::new();
    response.take(limit as u64 + 1).read_to_end(&mut body).map_err(|error| {
        Failure::Unreachable(ui_text!("读取 {host} 的应答失败: {error}", "Could not read {host}'s answer: {error}"))
    })?;
    if body.len() > limit {
        return Err(Failure::Fatal(ui_text!("{host} 的应答过大", "{host}'s answer is too large")));
    }
    serde_json::from_slice(&body).map(Some).map_err(|error| {
        Failure::Fatal(ui_text!("{host} 的应答无法解析: {error}", "Could not parse {host}'s answer: {error}"))
    })
}

/// A client for tarballs: no limit on the whole transfer, only on silence;
/// redirects (npmmirror sends tarballs to its CDN) stay on HTTPS.
fn download_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(STALL)
        .user_agent(concat!("Mewrk/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() > 10 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|error| ui_text!("无法创建下载客户端: {error}", "Could not set up the download client: {error}"))
}

/// A tarball must come over HTTPS — or, from a registry that is itself plain
/// HTTP (a test registry on this machine), from that same origin.
fn check_tarball_url(registry: &str, url: &str) -> Result<(), Failure> {
    let parsed = reqwest::Url::parse(url).ok();
    let base = reqwest::Url::parse(registry).ok();
    let allowed = parsed.as_ref().is_some_and(|parsed| {
        parsed.scheme() == "https"
            || base.as_ref().is_some_and(|base| {
                base.scheme() == parsed.scheme()
                    && base.host_str() == parsed.host_str()
                    && base.port_or_known_default() == parsed.port_or_known_default()
            })
    });
    if allowed {
        Ok(())
    } else {
        Err(Failure::Fatal(ui_text!(
            "npm 源给出的下载地址不是 HTTPS: {url}",
            "The npm registry gave a download address that is not HTTPS: {url}"
        )))
    }
}

/// The SHA-512 of an npm `dist.integrity` (`sha512-<base64>`; any other
/// algorithm listed beside it is ignored).
fn sha512_of(integrity: Option<&str>) -> Option<Vec<u8>> {
    integrity?
        .split_whitespace()
        .find_map(|item| item.strip_prefix("sha512-"))
        .and_then(|digest| base64::engine::general_purpose::STANDARD.decode(digest.split('?').next()?).ok())
        .filter(|digest| digest.len() == 64)
}

fn sha512_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha512::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_vec())
}

fn part_len(part: &Path) -> u64 {
    fs::metadata(part).map(|metadata| metadata.len()).unwrap_or(0)
}

/// Sleeps `delay`, or until cancelled.
fn wait(delay: Duration, cancel: &AtomicBool) -> Result<(), Failure> {
    let until = Instant::now() + delay;
    while Instant::now() < until {
        if cancel.load(Ordering::Acquire) {
            return Err(Failure::Fatal(CANCELLED.into()));
        }
        std::thread::sleep(Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())));
    }
    Ok(())
}

/// How one attempt at a tarball failed.
enum Attempt {
    /// Worth another attempt at the same registry: the connection failed,
    /// dropped or stalled, or the server had a passing error.
    Retry(String),
    Fatal(String),
}

/// Downloads `url` to `target`, refusing anything whose SHA-512 is not
/// `sha512`. The bytes are hashed as they arrive. A dropped transfer is tried
/// again, continuing where it stopped when the server takes a range; one that
/// keeps failing is [`Failure::Unreachable`], and what arrived stays in
/// `<target>.part` for the next registry or the next install to continue. A
/// `target` already there with the right digest is kept.
/// `progress(received, total)` follows the transfer.
fn fetch_tarball(
    client: &reqwest::blocking::Client,
    url: &str,
    target: &Path,
    sha512: &[u8],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), Failure> {
    let name = target.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    if target.is_file() {
        if sha512_file(target).is_ok_and(|digest| digest == sha512) {
            let size = part_len(target);
            progress(size, Some(size));
            return Ok(());
        }
        let _ = fs::remove_file(target);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| Failure::Fatal(folder_failed(parent, error)))?;
    }
    let part = target.with_file_name(format!("{name}.part"));
    let mut failures = 0;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Failure::Fatal(CANCELLED.into()));
        }
        let before = part_len(&part);
        match fetch_once(client, url, &name, &part, sha512, cancel, progress) {
            Ok(()) => break,
            Err(Attempt::Retry(error)) => {
                failures = if part_len(&part) > before { 1 } else { failures + 1 };
                if failures >= ATTEMPTS {
                    return Err(Failure::Unreachable(error));
                }
                eprintln!("[claude-agent] {url}: {error}; retrying");
                wait(RETRY_DELAY * 2u32.pow(failures - 1), cancel)?;
            }
            Err(Attempt::Fatal(error)) => return Err(Failure::Fatal(error)),
        }
    }
    super::with_patience(|| fs::rename(&part, target)).map_err(|error| {
        Failure::Fatal(ui_text!("无法放置 {name}: {error}", "Could not move {name} into place: {error}"))
    })
}

fn fetch_once(
    client: &reqwest::blocking::Client,
    url: &str,
    name: &str,
    part: &Path,
    sha512: &[u8],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<(), Attempt> {
    use reqwest::StatusCode;
    let mut have = part_len(part);
    let mut hasher = Sha512::new();
    if have > TARBALL_LIMIT {
        let _ = fs::remove_file(part);
        have = 0;
    }
    if have > 0 {
        // What arrived before is part of the digest too.
        let primed = File::open(part).and_then(|mut file| {
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    return Ok(());
                }
                hasher.update(&buffer[..read]);
            }
        });
        if primed.is_err() {
            let _ = fs::remove_file(part);
            have = 0;
            hasher = Sha512::new();
        }
    }
    let resumed = have > 0;
    let mut request = client.get(url);
    if resumed {
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let host = host_of(url);
    let mut response = request.send().map_err(|error| {
        Attempt::Retry(ui_text!("无法连接 {host}: {error}", "Could not connect to {host}: {error}"))
    })?;
    let status = response.status();
    match status {
        // The whole file, whether or not a range was asked for.
        StatusCode::OK => {
            have = 0;
            hasher = Sha512::new();
        }
        StatusCode::PARTIAL_CONTENT if resumed => {
            let start = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("bytes ")?.split('-').next()?.trim().parse::<u64>().ok());
            if start != Some(have) {
                let _ = fs::remove_file(part);
                return Err(Attempt::Retry(ui_text!(
                    "{name} 续传位置不符，重新下载",
                    "{name} resumed at the wrong place; downloading it again"
                )));
            }
        }
        StatusCode::RANGE_NOT_SATISFIABLE if resumed => {
            let _ = fs::remove_file(part);
            return Err(Attempt::Retry(ui_text!(
                "{name} 无法续传，重新下载",
                "{name} cannot be resumed; downloading it again"
            )));
        }
        _ => {
            let error = ui_text!("下载 {name} 失败：HTTP {status}", "Downloading {name} failed: HTTP {status}");
            return Err(if is_passing(status) { Attempt::Retry(error) } else { Attempt::Fatal(error) });
        }
    }
    let total = response.content_length().map(|length| have + length);
    if total.is_some_and(|total| total > TARBALL_LIMIT) {
        return Err(Attempt::Fatal(ui_text!("{name} 过大", "{name} is too large")));
    }
    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .append(have > 0)
        .truncate(have == 0)
        .open(part)
        .map_err(|error| Attempt::Fatal(write_failed(name, error)))?;
    progress(have, total);
    let mut buffer = vec![0u8; CHUNK];
    let mut received = have;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Attempt::Fatal(CANCELLED.into()));
        }
        let read = match response.read(&mut buffer) {
            Ok(read) => read,
            // What arrived stays in the part; the retry continues from there.
            Err(error) => {
                return Err(Attempt::Retry(ui_text!("下载中断: {error}", "The download was interrupted: {error}")));
            }
        };
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read]).map_err(|error| Attempt::Fatal(write_failed(name, error)))?;
        hasher.update(&buffer[..read]);
        received += read as u64;
        if received > TARBALL_LIMIT {
            drop(out);
            let _ = fs::remove_file(part);
            return Err(Attempt::Fatal(ui_text!("{name} 过大", "{name} is too large")));
        }
        progress(received, total);
    }
    out.sync_all().map_err(|error| Attempt::Fatal(write_failed(name, error)))?;
    drop(out);
    if total.is_some_and(|total| received < total) {
        return Err(Attempt::Retry(ui_text!("{name} 的连接提前结束", "The connection for {name} ended early")));
    }
    if hasher.finalize().as_slice() != sha512 {
        let _ = fs::remove_file(part);
        // Bytes kept from an earlier transfer may be what is wrong; one
        // download from the start settles it.
        return Err(if resumed {
            Attempt::Retry(ui_text!(
                "{name} 校验失败（sha512 不符），重新下载",
                "{name} failed verification (its sha512 does not match); downloading it again"
            ))
        } else {
            Attempt::Fatal(ui_text!(
                "{name} 校验失败（sha512 与 npm 上登记的不符），请重试",
                "{name} failed verification (its sha512 does not match what npm lists); try again"
            ))
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unpacking

/// The path of a tarball entry under `package/`, as plain segments; `None`
/// for anything that could land elsewhere: outside `package/`, absolute,
/// `..`, a backslash or a drive, an NTFS stream (`:`), a Windows device name.
/// `package/` itself is `Some(vec![])`.
fn package_path(raw: &[u8]) -> Option<Vec<&str>> {
    let text = std::str::from_utf8(raw).ok()?;
    let rest = text.strip_prefix("package")?;
    let rest = match rest {
        "" | "/" => return Some(Vec::new()),
        _ => rest.strip_prefix('/')?,
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let segments: Vec<&str> = rest.split('/').collect();
    segments.iter().all(|segment| is_safe_segment(segment)).then_some(segments)
}

fn is_safe_segment(segment: &str) -> bool {
    const DEVICES: &[&str] = &["con", "prn", "aux", "nul", "conin$", "conout$"];
    let stem = segment.split('.').next().unwrap_or_default().to_ascii_lowercase();
    let device = DEVICES.contains(&stem.as_str())
        || ((stem.starts_with("com") || stem.starts_with("lpt"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && !segment.ends_with('.')
        && !segment.ends_with(' ')
        && !device
        && segment.chars().all(|character| {
            !character.is_control() && !matches!(character, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        })
}

fn open_tarball(tarball: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<BufReader<File>>>, String> {
    let file = File::open(tarball).map_err(|error| unreadable_tarball(tarball, error))?;
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file))))
}

fn unreadable_tarball(tarball: &Path, error: impl std::fmt::Display) -> String {
    let name = tarball.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    ui_text!("无法读取 {name}: {error}", "Could not read {name}: {error}")
}

fn refused_entry(tarball: &Path, raw: &[u8]) -> String {
    let name = tarball.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let entry = String::from_utf8_lossy(raw);
    ui_text!(
        "{name} 里有不能安装的条目 {entry:?}（不是 package/ 下的普通文件）",
        "{name} holds an entry that cannot be installed, {entry:?} (not a plain file under package/)"
    )
}

/// Unpacks the SDK tarball's `package/` into `dest`: regular files and
/// directories only, every path checked by [`package_path`], within
/// [`SDK_UNPACKED_LIMIT`].
fn unpack_sdk(tarball: &Path, dest: &Path, cancel: &AtomicBool) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|error| folder_failed(dest, error))?;
    let mut archive = open_tarball(tarball)?;
    let entries = archive.entries().map_err(|error| unreadable_tarball(tarball, error))?;
    let (mut count, mut unpacked) = (0usize, 0u64);
    for entry in entries {
        if cancel.load(Ordering::Acquire) {
            return Err(CANCELLED.into());
        }
        let mut entry = entry.map_err(|error| unreadable_tarball(tarball, error))?;
        count += 1;
        if count > SDK_ENTRY_LIMIT {
            return Err(unreadable_tarball(tarball, "too many entries"));
        }
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::XGlobalHeader
                | tar::EntryType::XHeader
                | tar::EntryType::GNULongName
                | tar::EntryType::GNULongLink
        ) {
            continue;
        }
        let raw = entry.path_bytes().into_owned();
        let segments = package_path(&raw).ok_or_else(|| refused_entry(tarball, &raw))?;
        let path = segments.iter().fold(dest.to_path_buf(), |path, segment| path.join(segment));
        match kind {
            tar::EntryType::Directory => fs::create_dir_all(&path).map_err(|error| folder_failed(&path, error))?,
            tar::EntryType::Regular | tar::EntryType::Continuous if !segments.is_empty() => {
                let size = entry.header().size().map_err(|error| unreadable_tarball(tarball, error))?;
                unpacked += size;
                if unpacked > SDK_UNPACKED_LIMIT {
                    return Err(unreadable_tarball(tarball, "unpacks too large"));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|error| folder_failed(parent, error))?;
                }
                let file_name = segments.join("/");
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|error| write_failed(&file_name, error))?;
                let copied = std::io::copy(&mut (&mut entry).take(size), &mut out)
                    .map_err(|error| unreadable_tarball(tarball, error))?;
                if copied != size {
                    return Err(unreadable_tarball(tarball, "truncated"));
                }
            }
            _ => return Err(refused_entry(tarball, &raw)),
        }
    }
    Ok(())
}

/// Unpacks `package/claude[.exe]` out of the CLI tarball into `target`, made
/// executable. Returns its size and SHA-256. Nothing else of the package is
/// needed, so nothing else is written.
fn unpack_cli(tarball: &Path, target: &Path, cancel: &AtomicBool) -> Result<(u64, String), String> {
    let wanted = format!("package/{}", cli_name());
    let mut archive = open_tarball(tarball)?;
    let entries = archive.entries().map_err(|error| unreadable_tarball(tarball, error))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| unreadable_tarball(tarball, error))?;
        let raw = entry.path_bytes().into_owned();
        if raw != wanted.as_bytes() {
            continue;
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(refused_entry(tarball, &raw));
        }
        let size = entry.header().size().map_err(|error| unreadable_tarball(tarball, error))?;
        if size == 0 || size > CLI_LIMIT {
            return Err(unreadable_tarball(tarball, "the executable's size is out of range"));
        }
        let name = cli_name();
        let mut out =
            OpenOptions::new().write(true).create_new(true).open(target).map_err(|error| write_failed(&name, error))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; CHUNK];
        let mut written = 0u64;
        let mut body = (&mut entry).take(size);
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(CANCELLED.into());
            }
            let read = body.read(&mut buffer).map_err(|error| unreadable_tarball(tarball, error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read]).map_err(|error| write_failed(&name, error))?;
            written += read as u64;
        }
        out.sync_all().map_err(|error| write_failed(&name, error))?;
        drop(out);
        if written != size {
            return Err(unreadable_tarball(tarball, "truncated"));
        }
        make_executable(target);
        let digest = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
        return Ok((written, digest));
    }
    let name = cli_name();
    Err(unreadable_tarball(tarball, format!("no package/{name}")))
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

// ---------------------------------------------------------------------------
// The installed versions

/// A version directory's name: a release version, nothing that could be a path.
fn version_dir(dir: &Path, version: &str) -> Option<PathBuf> {
    let parsed = parse_release(version)?;
    (parsed.to_string() == version).then(|| dir.join(version))
}

/// The record of `version` under `dir` when that version is complete.
fn installed_at(dir: &Path, version: &str) -> Option<(PathBuf, InstallRecord)> {
    let root = version_dir(dir, version)?;
    let record: InstallRecord = super::read_json(&root.join(INSTALL_RECORD))?;
    let complete =
        record.sdk_version == version && root.join("sdk").join("sdk.mjs").is_file() && root.join(cli_name()).is_file();
    complete.then_some((root, record))
}

fn current_version(dir: &Path) -> Option<String> {
    super::read_json::<CurrentRecord>(&dir.join(CURRENT)).map(|record| record.sdk_version)
}

/// The current version under `dir` (`<components>/claude-agent`), when it is
/// complete: its directory and its record.
fn current_install(dir: &Path) -> Option<(PathBuf, InstallRecord)> {
    installed_at(dir, &current_version(dir)?)
}

/// Whether the CLI of the version installed at `root` is the file its record
/// says was installed ([`super::verified_file`]: hashed once per process).
fn cli_verified(root: &Path, record: &InstallRecord) -> bool {
    record
        .claude_sha256
        .as_deref()
        .is_some_and(|sha256| super::verified_file(&root.join(cli_name()), sha256))
}

/// The current version under `dir`, as on disk: neither checked against the
/// sidecar's range or its record, nor remembered as in use.
#[cfg(test)]
fn installed_runtime(dir: &Path) -> Option<Runtime> {
    let (root, record) = current_install(dir)?;
    Some(Runtime {
        cli: root.join(cli_name()),
        sdk_entry: root.join("sdk").join("sdk.mjs"),
        sdk_version: record.sdk_version,
        claude_code_version: record.claude_code_version,
    })
}

/// The source tree's SDK and CLI: what `npm install` in `aisdk-service/` put
/// under `node_modules`, the SDK's own platform package included.
///
/// Debug builds only, because it embeds `CARGO_MANIFEST_DIR`.
#[cfg(debug_assertions)]
pub(crate) fn development_runtime() -> Option<Runtime> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?;
    let modules = repo.join("aisdk-service").join("node_modules").join("@anthropic-ai");
    let sdk = modules.join("claude-agent-sdk");
    let entry = sdk.join("sdk.mjs");
    let cli = modules.join(format!("claude-agent-sdk-{}", platform_tag())).join(cli_name());
    if !entry.is_file() || !cli.is_file() {
        return None;
    }
    let package: PackageJson = super::read_json(&sdk.join("package.json"))?;
    Some(Runtime {
        cli,
        sdk_entry: entry,
        sdk_version: package.version,
        claude_code_version: package.claude_code_version,
    })
}

/// [`EXECUTABLE_ENV`] in place of the CLI.
#[cfg(debug_assertions)]
fn with_executable_override(runtime: Runtime) -> Result<Runtime, String> {
    let Ok(value) = std::env::var(EXECUTABLE_ENV) else {
        return Ok(runtime);
    };
    let cli = PathBuf::from(value);
    if !cli.is_file() {
        let path = cli.display();
        return Err(ui_text!(
            "{EXECUTABLE_ENV} 指向的 Claude Code 不存在：{path}",
            "The Claude Code that {EXECUTABLE_ENV} names does not exist: {path}"
        ));
    }
    Ok(Runtime { cli, ..runtime })
}

/// What the provider page shows as installed. A version whose CLI is not the
/// file that was installed is not: installing it again is how that is put
/// right, and the page offers an install for what is not installed.
fn installed_version(dir: Option<&Path>, floor: &Version) -> Option<InstalledVersion> {
    if let Some((_, record)) = dir.and_then(current_install).filter(|(root, record)| cli_verified(root, record)) {
        return Some(InstalledVersion {
            compatible: is_compatible(&record.sdk_version, floor),
            sdk_version: record.sdk_version,
            claude_code_version: record.claude_code_version,
            source: InstalledSource::Installed,
            installed_at: record.installed_at,
        });
    }
    #[cfg(debug_assertions)]
    if let Some(runtime) = development_runtime() {
        return Some(InstalledVersion {
            sdk_version: runtime.sdk_version,
            claude_code_version: runtime.claude_code_version,
            source: InstalledSource::Development,
            installed_at: None,
            compatible: true,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// Status

/// What npm said last, for the floor it was asked about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LatestCheck {
    latest: Option<AvailableVersion>,
    newer_incompatible: Option<String>,
}

/// The last check of this run. A poll that does not check again (while an
/// install runs) still says what the page was told.
static LATEST: Mutex<Option<(Version, LatestCheck)>> = Mutex::new(None);

fn latest_cache() -> MutexGuard<'static, Option<(Version, LatestCheck)>> {
    LATEST.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The provider page's view. `check_latest` asks npm for the newest
/// compatible release (blocking, a few seconds at most); without it the
/// answer is what is on disk, the install in progress and the last check.
pub fn status(check_latest: bool) -> ClaudeAgentComponentStatus {
    let dir = super::root().map(|root| root.join(DIR));
    status_with(dir.as_deref(), &Npm::from_env(), &floor(), check_latest)
}

fn status_with(dir: Option<&Path>, npm: &Npm, floor: &Version, check_latest: bool) -> ClaudeAgentComponentStatus {
    let compatible = compatible_range(floor).to_string();
    let (check, latest_error) = if check_latest {
        match check(npm, floor) {
            Ok(check) => {
                *latest_cache() = Some((floor.clone(), check.clone()));
                (Some(check), None)
            }
            Err(error) => {
                *latest_cache() = None;
                (None, Some(error))
            }
        }
    } else {
        let cached = latest_cache().as_ref().filter(|(checked, _)| checked == floor).map(|(_, check)| check.clone());
        (cached, None)
    };
    let (latest, newer_incompatible) = check.map_or((None, None), |check| (check.latest, check.newer_incompatible));
    let latest_error = latest_error.or_else(|| {
        (check_latest && latest.is_none()).then(|| {
            ui_text!("npm 上没有与 {compatible} 兼容的版本", "npm has no release compatible with {compatible}")
        })
    });
    let installed = installed_version(dir, floor);
    let update_available = match (&installed, &latest) {
        // What is installed cannot run: the newest compatible release is the
        // update, newer or not.
        (Some(installed), Some(latest)) if !installed.compatible => installed.sdk_version != latest.sdk_version,
        (Some(installed), Some(latest)) => {
            match (Version::parse(&installed.sdk_version), Version::parse(&latest.sdk_version)) {
                (Ok(installed), Ok(latest)) => latest > installed,
                _ => false,
            }
        }
        _ => false,
    };
    let (task, last_error) = {
        let task = task();
        (task.running.clone(), task.last_error.clone())
    };
    ClaudeAgentComponentStatus {
        installed,
        compatible,
        latest,
        latest_error,
        newer_incompatible,
        update_available,
        task,
        last_error,
    }
}

fn check(npm: &Npm, floor: &Version) -> Result<LatestCheck, String> {
    let client = super::small_client()?;
    npm.first(|registry| registry.check(&client, floor)).map_err(|error| {
        ui_text!(
            "无法从 npm 检查 Claude Agent SDK 的版本: {error}",
            "Could not check npm for Claude Agent SDK releases: {error}"
        )
    })
}

// ---------------------------------------------------------------------------
// Installing

/// Where and from what an install works. Everything an install reads from
/// the process (the components root, the registry override, the sidecar's
/// SDK release, the platform) is gathered here first, so tests can supply
/// their own.
struct Context {
    /// `<components>/claude-agent`.
    dir: PathBuf,
    npm: Npm,
    floor: Version,
    /// The CLI package's platform: [`platform_tag`].
    platform: String,
}

impl Context {
    fn current() -> Result<Self, String> {
        let root =
            super::root().ok_or_else(|| ui_text!("组件目录尚未就绪", "The components folder is not set up yet"))?;
        Ok(Self { dir: root.join(DIR), npm: Npm::from_env(), floor: floor(), platform: platform_tag() })
    }
}

/// What an install reports as it goes.
#[derive(Debug, PartialEq, Eq)]
enum Progress {
    /// The version being installed, once known.
    Version(String),
    Phase(Phase),
    /// Bytes received of both tarballs, and their size as far as known.
    Bytes(u64, Option<u64>),
}

/// Installs `requested` (else the newest compatible release) under
/// `ctx.dir` and makes it current. A version already installed there is
/// switched to without downloading anything.
fn install(
    ctx: &Context,
    requested: Option<&str>,
    cancel: &AtomicBool,
    report: &mut dyn FnMut(Progress),
) -> Result<(), String> {
    report(Progress::Phase(Phase::Resolving));
    let requested = requested.map(|text| compatible_version(text, &ctx.floor)).transpose()?;
    fs::create_dir_all(&ctx.dir).map_err(|error| folder_failed(&ctx.dir, error))?;
    // A staging directory left by an install that never finished.
    super::prune_dir(&ctx.dir, |name| !name.starts_with(STAGING_PREFIX));
    let downloads = ctx.dir.join(DOWNLOADS);
    let client = super::small_client()?;
    let tarballs = download_client()?;
    let cli_package = cli_package(&ctx.platform);
    let fetched = ctx.npm.first(|registry| {
        let version = match &requested {
            Some(version) => version.clone(),
            None => registry.select(&client, &ctx.floor)?.latest.ok_or_else(|| {
                let range = compatible_range(&ctx.floor);
                Failure::Fatal(ui_text!(
                    "npm 上没有与 {range} 兼容的版本",
                    "npm has no release compatible with {range}"
                ))
            })?,
        };
        report(Progress::Version(version.to_string()));
        // Installed, and still the files that were: switching to it is all.
        // Anything else of it on disk is replaced.
        let installed = installed_at(&ctx.dir, &version.to_string());
        if installed.is_some_and(|(root, record)| cli_verified(&root, &record)) {
            return Ok(Fetched::Installed(version));
        }
        let sdk = registry.version_doc(&client, SDK_PACKAGE, &version)?;
        let cli = registry.version_doc(&client, &cli_package, &version)?;
        let mut digests = Vec::new();
        for doc in [&cli, &sdk] {
            check_tarball_url(registry.base, &doc.dist.tarball)?;
            let name = &doc.name;
            digests.push(sha512_of(doc.dist.integrity.as_deref()).ok_or_else(|| {
                Failure::Fatal(ui_text!(
                    "npm 没有给出 {name}@{version} 的 sha512 校验值",
                    "npm lists no sha512 for {name}@{version}"
                ))
            })?);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Failure::Fatal(CANCELLED.into()));
        }
        report(Progress::Phase(Phase::Downloading));
        // The CLI first: it is nearly all of the bytes, so the total is close
        // to right from the start.
        let cli_tarball = downloads.join(format!("claude-agent-sdk-{}-{version}.tgz", ctx.platform));
        fetch_tarball(&tarballs, &cli.dist.tarball, &cli_tarball, &digests[0], cancel, &mut |received, total| {
            report(Progress::Bytes(received, total))
        })?;
        let done = part_len(&cli_tarball);
        let sdk_tarball = downloads.join(format!("claude-agent-sdk-{version}.tgz"));
        fetch_tarball(&tarballs, &sdk.dist.tarball, &sdk_tarball, &digests[1], cancel, &mut |received, total| {
            report(Progress::Bytes(done + received, total.map(|total| done + total)))
        })?;
        Ok(Fetched::Downloaded { version, sdk_tarball, cli_tarball, claude_code_version: sdk.claude_code_version })
    })?;
    let (version, sdk_tarball, cli_tarball, listed_claude_code) = match fetched {
        Fetched::Installed(version) => {
            report(Progress::Phase(Phase::Installing));
            return switch_to(&ctx.dir, &version.to_string());
        }
        Fetched::Downloaded { version, sdk_tarball, cli_tarball, claude_code_version } => {
            (version, sdk_tarball, cli_tarball, claude_code_version)
        }
    };
    report(Progress::Phase(Phase::Verifying));
    let name = version.to_string();
    let staging = ctx.dir.join(format!("{STAGING_PREFIX}{name}"));
    let cli_sha256 = match stage(ctx, &name, &staging, &sdk_tarball, &cli_tarball, listed_claude_code, cancel) {
        Ok(cli_sha256) => cli_sha256,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            // Tarballs that passed npm's digest but not the checks after it
            // would only fail the same way again; a cancelled one is kept for
            // the next attempt.
            if error != CANCELLED {
                let _ = fs::remove_file(&sdk_tarball);
                let _ = fs::remove_file(&cli_tarball);
            }
            return Err(error);
        }
    };
    report(Progress::Phase(Phase::Installing));
    let target = ctx.dir.join(&name);
    if let Err(error) = place(&staging, &target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    // Hashed as it was unpacked; its first run need not hash it again.
    super::remember_verified(&target.join(cli_name()), &cli_sha256);
    switch_to(&ctx.dir, &name)
}

/// What [`install`] got from npm.
enum Fetched {
    /// The version is installed already: switching to it is all there is.
    Installed(Version),
    Downloaded {
        version: Version,
        sdk_tarball: PathBuf,
        cli_tarball: PathBuf,
        /// The registry's `claudeCodeVersion`, in case the SDK's own
        /// `package.json` does not say.
        claude_code_version: Option<String>,
    },
}

/// Unpacks and checks both tarballs into `staging`, and records the version.
/// Returns the CLI's SHA-256.
fn stage(
    ctx: &Context,
    version: &str,
    staging: &Path,
    sdk_tarball: &Path,
    cli_tarball: &Path,
    listed_claude_code: Option<String>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let sdk_dir = staging.join("sdk");
    unpack_sdk(sdk_tarball, &sdk_dir, cancel)?;
    let package: PackageJson = super::read_json(&sdk_dir.join("package.json")).ok_or_else(|| {
        ui_text!("Claude Agent SDK 的 package.json 无法读取", "The Claude Agent SDK's package.json cannot be read")
    })?;
    if package.name != SDK_PACKAGE || package.version != version {
        let (name, found) = (&package.name, &package.version);
        return Err(ui_text!(
            "下载到的是 {name}@{found}，不是 {SDK_PACKAGE}@{version}",
            "What was downloaded is {name}@{found}, not {SDK_PACKAGE}@{version}"
        ));
    }
    if !sdk_dir.join("sdk.mjs").is_file() {
        return Err(ui_text!("Claude Agent SDK {version} 里没有 sdk.mjs", "Claude Agent SDK {version} has no sdk.mjs"));
    }
    let cli = staging.join(cli_name());
    let (size, sha256) = unpack_cli(cli_tarball, &cli, cancel)?;
    // The SDK lists every platform's build; a manifest without this platform
    // (or without a manifest) leaves the npm integrity as the check.
    let manifest: Option<Manifest> = super::read_json(&sdk_dir.join("manifest.json"));
    if let Some(build) = manifest.as_ref().and_then(|manifest| manifest.platforms.get(&ctx.platform)) {
        let checksum_differs = build.checksum.as_ref().is_some_and(|checksum| !checksum.eq_ignore_ascii_case(&sha256));
        if checksum_differs || build.size.is_some_and(|listed| listed != size) {
            let platform = &ctx.platform;
            return Err(ui_text!(
                "Claude Code（{platform}）与 Claude Agent SDK {version} 清单里登记的不符",
                "The Claude Code for {platform} does not match what Claude Agent SDK {version}'s manifest lists"
            ));
        }
    }
    let record = InstallRecord {
        sdk_version: version.to_owned(),
        claude_code_version: package.claude_code_version.or(listed_claude_code),
        claude_sha256: Some(sha256.clone()),
        installed_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    };
    super::write_json_atomic(&staging.join(INSTALL_RECORD), &record)?;
    Ok(sha256)
}

/// Renames the staged version into place, patiently ([`super::with_patience`]:
/// a virus scanner may hold the new executable for a moment). A directory
/// already there is not a usable install of the version (that is switched to
/// instead) and is replaced — unless files in it are in use, which only a
/// Claude Code still running from it does: then restarting Mewrk, which ends
/// every session it started, is what frees it.
fn place(staging: &Path, target: &Path) -> Result<(), String> {
    let shown = target.display().to_string();
    if target.exists() {
        super::with_patience(|| fs::remove_dir_all(target)).map_err(|error| {
            ui_text!(
                "无法替换 {shown}，其中的文件正在被使用（{error}）：请重启 Mewrk 后再安装。",
                "Could not replace {shown}: files in it are in use ({error}). Restart Mewrk and install again."
            )
        })?;
    }
    super::with_patience(|| fs::rename(staging, target))
        .map_err(|error| ui_text!("无法放置 {shown}: {error}", "Could not move {shown} into place: {error}"))
}

/// Makes `version` current, then removes every version but it, the one it
/// replaces and those this process has run ([`IN_USE`]), with the downloads
/// and anything left half done. Switching to the version already current (a
/// reinstall) says nothing of which version came before it, so then every
/// version stays.
fn switch_to(dir: &Path, version: &str) -> Result<(), String> {
    let previous = current_version(dir);
    let unchanged = previous.as_deref() == Some(version);
    if !unchanged {
        super::write_json_atomic(&dir.join(CURRENT), &CurrentRecord { sdk_version: version.to_owned() })?;
    }
    let keep = |name: &str| {
        name == CURRENT
            || name == version
            || previous.as_deref() == Some(name)
            || (unchanged && version_dir(dir, name).is_some())
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if keep(&name) || in_use(&path) {
            continue;
        }
        if let Err(error) = remove_version(&path) {
            eprintln!("[claude-agent] could not remove {}: {error}", path.display());
        }
    }
    Ok(())
}

/// Removes an entry of `<components>/claude-agent`. A version directory goes
/// CLI first: a CLI still running (Windows) cannot be removed, and then the
/// rest of its version is left whole for a later switch, rather than half
/// deleted under it.
fn remove_version(path: &Path) -> std::io::Result<()> {
    if !path.is_dir() {
        return fs::remove_file(path);
    }
    match fs::remove_file(path.join(cli_name())) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::remove_dir_all(path)
}

// ---------------------------------------------------------------------------
// The install task

struct Task {
    running: Option<TaskStatus>,
    cancel: Option<Arc<AtomicBool>>,
    last_error: Option<String>,
}

static TASK: Mutex<Task> = Mutex::new(Task { running: None, cancel: None, last_error: None });

fn task() -> MutexGuard<'static, Task> {
    TASK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts installing `version` (else the newest compatible release) on a
/// thread of its own; [`status`] follows it. Refused while another runs.
pub fn start_install(version: Option<String>) -> Result<(), String> {
    start_install_with(Context::current()?, version)
}

fn start_install_with(ctx: Context, version: Option<String>) -> Result<(), String> {
    let requested = version.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());
    if let Some(text) = &requested {
        compatible_version(text, &ctx.floor)?;
    }
    let action = if current_install(&ctx.dir).is_some() { TaskAction::Update } else { TaskAction::Install };
    let hint = requested.clone().or_else(|| {
        latest_cache()
            .as_ref()
            .filter(|(checked, _)| *checked == ctx.floor)
            .and_then(|(_, check)| check.latest.as_ref().map(|latest| latest.sdk_version.clone()))
    });
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut task = task();
        if task.running.is_some() {
            return Err(ui_text!(
                "Claude Agent 组件正在安装，请等它结束或先取消",
                "The Claude Agent components are already being installed; wait for it or cancel it first"
            ));
        }
        task.running = Some(TaskStatus {
            action,
            sdk_version: hint.unwrap_or_default(),
            phase: Phase::Resolving,
            received_bytes: 0,
            total_bytes: None,
        });
        task.cancel = Some(Arc::clone(&cancel));
        task.last_error = None;
    }
    let spawned = std::thread::Builder::new().name("claude-agent-install".into()).spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            install(&ctx, requested.as_deref(), &cancel, &mut follow)
        }))
        .unwrap_or_else(|_| Err(ui_text!("安装意外中止", "The install stopped unexpectedly")));
        let mut task = task();
        task.running = None;
        task.cancel = None;
        match result {
            Ok(()) => {}
            Err(error) if error == CANCELLED => {}
            Err(error) => {
                eprintln!("[claude-agent] install failed: {error}");
                task.last_error = Some(error);
            }
        }
    });
    if let Err(error) = spawned {
        let mut task = task();
        task.running = None;
        task.cancel = None;
        return Err(ui_text!("无法开始安装: {error}", "Could not start the install: {error}"));
    }
    Ok(())
}

/// Records an install's progress for [`status`].
fn follow(progress: Progress) {
    let mut task = task();
    let Some(running) = task.running.as_mut() else {
        return;
    };
    match progress {
        Progress::Version(version) => running.sdk_version = version,
        Progress::Phase(phase) => running.phase = phase,
        Progress::Bytes(received, total) => {
            running.received_bytes = received;
            running.total_bytes = total;
        }
    }
}

/// Stops the install in progress, if any. What was downloaded stays for the
/// next attempt; nothing half unpacked does.
pub fn cancel_install() {
    if let Some(cancel) = &task().cancel {
        cancel.store(true, Ordering::Release);
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
    use std::collections::HashMap;
    use std::io::BufRead as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Condvar;

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use serde_json::json;

    use super::*;

    const FLOOR: &str = "0.3.284";

    #[derive(Clone)]
    enum Reply {
        Body(Vec<u8>),
        Status(u16),
        /// The body, once the gate opens.
        Gated(Vec<u8>, Arc<Gate>),
    }

    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
        }
    }

    /// A registry on this machine: answers each path from its routes, 404
    /// for anything else, one connection per request.
    struct Fixture {
        base: String,
        routes: Arc<Mutex<HashMap<String, Reply>>>,
        hits: Arc<Mutex<Vec<String>>>,
    }

    impl Fixture {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes = Arc::new(Mutex::new(HashMap::new()));
            let hits = Arc::new(Mutex::new(Vec::new()));
            let (served, seen) = (Arc::clone(&routes), Arc::clone(&hits));
            std::thread::spawn(move || {
                for stream in listener.incoming().map_while(Result::ok) {
                    let (routes, hits) = (Arc::clone(&served), Arc::clone(&seen));
                    std::thread::spawn(move || answer(stream, &routes, &hits));
                }
            });
            Self { base, routes, hits }
        }

        fn route(&self, path: &str, reply: Reply) {
            self.routes.lock().unwrap().insert(path.to_owned(), reply);
        }

        fn json(&self, path: &str, value: Value) {
            self.route(path, Reply::Body(serde_json::to_vec(&value).unwrap()));
        }

        fn hits(&self, prefix: &str) -> usize {
            self.hits.lock().unwrap().iter().filter(|path| path.starts_with(prefix)).count()
        }
    }

    fn answer(mut stream: TcpStream, routes: &Mutex<HashMap<String, Reply>>, hits: &Mutex<Vec<String>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(read) if read > 0 && !line.trim().is_empty() => continue,
                _ => break,
            }
        }
        let path = request_line.split_whitespace().nth(1).unwrap_or_default().replace("%2f", "/").replace("%2F", "/");
        hits.lock().unwrap().push(path.clone());
        let reply = routes.lock().unwrap().get(&path).cloned().unwrap_or(Reply::Status(404));
        let (status, body) = match reply {
            Reply::Body(body) => ("200 OK".to_owned(), body),
            Reply::Status(code) => (format!("{code} Fixture"), Vec::new()),
            Reply::Gated(body, gate) => {
                gate.wait();
                ("200 OK".to_owned(), body)
            }
        };
        let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
    }

    /// The key the fixtures' version documents are signed with, standing in
    /// for npm's.
    fn signer() -> &'static ring::signature::EcdsaKeyPair {
        static SIGNER: std::sync::OnceLock<ring::signature::EcdsaKeyPair> = std::sync::OnceLock::new();
        SIGNER.get_or_init(|| generate_key())
    }

    fn generate_key() -> ring::signature::EcdsaKeyPair {
        use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};
        let random = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &random).unwrap();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &random).unwrap()
    }

    const TEST_KEY_ID: &str = "SHA256:the-tests-own-registry-key";

    fn test_key() -> RegistryKey {
        use ring::signature::KeyPair as _;
        RegistryKey { id: TEST_KEY_ID.into(), point: signer().public_key().as_ref().to_vec() }
    }

    /// Registries whose documents are believed when signed by [`signer`].
    fn registries(bases: &[&str]) -> Npm {
        Npm { registries: bases.iter().map(|base| (*base).to_owned()).collect(), keys: vec![test_key()] }
    }

    /// A signature over what npm signs of `doc`, by `key`.
    fn signature_of(doc: &Value, key: &ring::signature::EcdsaKeyPair) -> String {
        let text = |value: &Value| value.as_str().unwrap().to_owned();
        let message = format!("{}@{}:{}", text(&doc["name"]), text(&doc["version"]), text(&doc["dist"]["integrity"]));
        let signature = key.sign(&ring::rand::SystemRandom::new(), message.as_bytes()).unwrap();
        base64::engine::general_purpose::STANDARD.encode(signature.as_ref())
    }

    /// `doc` as npm publishes it: signed.
    fn signed(mut doc: Value) -> Value {
        let sig = signature_of(&doc, signer());
        doc["dist"]["signatures"] = json!([{ "keyid": TEST_KEY_ID, "sig": sig }]);
        doc
    }

    fn context(fixture: &Fixture, root: &Path) -> Context {
        Context {
            dir: root.join(DIR),
            npm: registries(&[fixture.base.as_str()]),
            floor: Version::parse(FLOOR).unwrap(),
            platform: platform_tag(),
        }
    }

    type Tar = tar::Builder<GzEncoder<Vec<u8>>>;

    fn gzip_tar(build: impl FnOnce(&mut Tar)) -> Vec<u8> {
        let mut builder = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        build(&mut builder);
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn add_file(builder: &mut Tar, path: &str, body: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, body).unwrap();
    }

    /// An entry named as given, past the checks `append_data` makes.
    fn add_raw(builder: &mut Tar, name: &[u8], kind: tar::EntryType) {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_mode(0o644);
        header.set_entry_type(kind);
        let body: &[u8] = if kind == tar::EntryType::Symlink {
            header.set_link_name("../../../evil.txt").unwrap();
            b""
        } else {
            b"evil"
        };
        header.set_size(body.len() as u64);
        header.set_cksum();
        builder.append(&header, body).unwrap();
    }

    /// Bytes that tell releases apart and do not compress.
    fn cli_bytes(version: &str, len: usize) -> Vec<u8> {
        let mut state =
            version.bytes().fold(0x9e37_79b9_7f4a_7c15u64, |state, byte| state.rotate_left(5) ^ u64::from(byte));
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn integrity(bytes: &[u8]) -> String {
        format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes)))
    }

    fn code_version(version: &str) -> String {
        version.replacen("0.3.", "2.1.", 1)
    }

    /// The SDK's tarball, its manifest listing `listed` as this platform's CLI.
    fn sdk_tarball(version: &str, listed: &[u8], extra: impl FnOnce(&mut Tar)) -> Vec<u8> {
        gzip_tar(|builder| {
            let package =
                json!({ "name": SDK_PACKAGE, "version": version, "claudeCodeVersion": code_version(version) });
            add_file(builder, "package/package.json", package.to_string().as_bytes());
            add_file(builder, "package/sdk.mjs", b"export function query() {}\n");
            add_file(builder, "package/lib/types.d.ts", b"export {};\n");
            let manifest = json!({
                "version": code_version(version),
                "platforms": {
                    platform_tag(): { "binary": cli_name(), "checksum": sha256_hex(listed), "size": listed.len() },
                    "plan9-mips": { "binary": "claude", "checksum": "00", "size": 1 }
                }
            });
            add_file(builder, "package/manifest.json", manifest.to_string().as_bytes());
            extra(builder);
        })
    }

    fn cli_tarball(cli: &[u8]) -> Vec<u8> {
        gzip_tar(|builder| {
            add_file(builder, "package/package.json", br#"{"name":"cli"}"#);
            add_file(builder, &format!("package/{}", cli_name()), cli);
            add_file(builder, "package/README.md", b"the CLI");
        })
    }

    fn publish_with(
        fixture: &Fixture,
        version: &str,
        sdk: &[u8],
        cli: &[u8],
        sdk_integrity: String,
        cli_integrity: String,
    ) {
        let (base, platform) = (&fixture.base, platform_tag());
        fixture.json(
            &format!("/{SDK_PACKAGE}/{version}"),
            signed(json!({
                "name": SDK_PACKAGE,
                "version": version,
                "claudeCodeVersion": code_version(version),
                "dist": { "tarball": format!("{base}/tarballs/sdk-{version}.tgz"), "integrity": sdk_integrity }
            })),
        );
        fixture.json(
            &format!("/{SDK_PACKAGE}-{platform}/{version}"),
            signed(json!({
                "name": format!("{SDK_PACKAGE}-{platform}"),
                "version": version,
                "dist": { "tarball": format!("{base}/tarballs/cli-{version}.tgz"), "integrity": cli_integrity }
            })),
        );
        fixture.route(&format!("/tarballs/sdk-{version}.tgz"), Reply::Body(sdk.to_vec()));
        fixture.route(&format!("/tarballs/cli-{version}.tgz"), Reply::Body(cli.to_vec()));
    }

    fn publish(fixture: &Fixture, version: &str, sdk: &[u8], cli: &[u8]) {
        publish_with(fixture, version, sdk, cli, integrity(sdk), integrity(cli));
    }

    /// Publishes a consistent release; returns its CLI's bytes.
    fn release(fixture: &Fixture, version: &str) -> Vec<u8> {
        let cli = cli_bytes(version, 4096);
        publish(fixture, version, &sdk_tarball(version, &cli, |_| {}), &cli_tarball(&cli));
        cli
    }

    fn tag_latest(fixture: &Fixture, version: &str) {
        fixture.json(&format!("/-/package/{SDK_PACKAGE}/dist-tags"), json!({ "latest": version }));
    }

    fn no_leftovers(dir: &Path) {
        let names: Vec<String> = fs::read_dir(dir)
            .map(|entries| entries.flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        assert!(names.iter().all(|name| !name.starts_with(STAGING_PREFIX)), "{names:?}");
    }

    fn quiet(_: Progress) {}

    #[test]
    fn an_install_checks_both_tarballs_and_makes_the_version_current() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let cli = release(&fixture, "0.3.290");
        tag_latest(&fixture, "0.3.290");

        let mut events = Vec::new();
        install(&ctx, None, &AtomicBool::new(false), &mut |progress| events.push(progress)).expect("installs");

        let runtime = installed_runtime(&ctx.dir).expect("0.3.290 is current");
        assert_eq!(runtime.sdk_version, "0.3.290");
        assert_eq!(runtime.claude_code_version.as_deref(), Some("2.1.290"));
        assert_eq!(runtime.cli, ctx.dir.join("0.3.290").join(cli_name()));
        assert_eq!(fs::read(&runtime.cli).unwrap(), cli);
        assert_eq!(runtime.sdk_entry, ctx.dir.join("0.3.290").join("sdk").join("sdk.mjs"));
        assert!(ctx.dir.join("0.3.290/sdk/lib/types.d.ts").is_file());
        let record: InstallRecord = super::super::read_json(&ctx.dir.join("0.3.290").join(INSTALL_RECORD)).unwrap();
        assert!(record.installed_at.is_some_and(|at| at.ends_with('Z')));
        assert!(!ctx.dir.join(DOWNLOADS).exists(), "the tarballs go once installed");
        no_leftovers(&ctx.dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&runtime.cli).unwrap().permissions().mode() & 0o777, 0o755);
        }

        assert!(events.contains(&Progress::Version("0.3.290".into())));
        let phases: Vec<Phase> = events
            .iter()
            .filter_map(|event| match event {
                Progress::Phase(phase) => Some(*phase),
                _ => None,
            })
            .collect();
        assert_eq!(phases, [Phase::Resolving, Phase::Downloading, Phase::Verifying, Phase::Installing]);
        let last = events.iter().rev().find_map(|event| match event {
            Progress::Bytes(received, total) => Some((*received, *total)),
            _ => None,
        });
        let (received, total) = last.expect("bytes were reported");
        assert_eq!(Some(received), total);

        let status = status_with(Some(&ctx.dir), &ctx.npm, &ctx.floor, true);
        let installed = status.installed.clone().expect("installed");
        assert_eq!((installed.sdk_version.as_str(), installed.source), ("0.3.290", InstalledSource::Installed));
        assert_eq!(status.latest.as_ref().map(|latest| latest.sdk_version.as_str()), Some("0.3.290"));
        assert!(!status.update_available);
        let value = serde_json::to_value(&status).unwrap();
        for key in [
            "installed",
            "compatible",
            "latest",
            "latestError",
            "newerIncompatible",
            "updateAvailable",
            "task",
            "lastError",
        ] {
            assert!(value.get(key).is_some(), "{key} 必须出现（null 也要出现）");
        }
        assert_eq!(value["compatible"], "^0.3.284");
        assert_eq!(value["installed"]["source"], "installed");
        assert_eq!(value["installed"]["claudeCodeVersion"], "2.1.290");
        assert_eq!(value["installed"]["compatible"], true);
        assert_eq!(value["latest"]["sdkVersion"], "0.3.290");
        let record: Value = super::super::read_json(&ctx.dir.join("0.3.290").join(INSTALL_RECORD)).unwrap();
        assert_eq!(record["claudeSha256"], sha256_hex(&cli), "the CLI's digest is recorded as installed");

        // Two more releases: the oldest goes, the one before current stays.
        release(&fixture, "0.3.291");
        release(&fixture, "0.3.292");
        install(&ctx, Some("0.3.291"), &AtomicBool::new(false), &mut quiet).unwrap();
        install(&ctx, Some("0.3.292"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.292"));
        assert!(ctx.dir.join("0.3.291").is_dir());
        assert!(!ctx.dir.join("0.3.290").exists());
        tag_latest(&fixture, "0.3.292");
        assert!(!status_with(Some(&ctx.dir), &ctx.npm, &ctx.floor, true).update_available);

        // Going back to the version kept downloads nothing.
        let fetched = fixture.hits("/tarballs/");
        install(&ctx, Some("0.3.291"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.291"));
        assert_eq!(fixture.hits("/tarballs/"), fetched);
        assert!(ctx.dir.join("0.3.292").is_dir(), "the version it replaced stays");
        assert!(status_with(Some(&ctx.dir), &ctx.npm, &ctx.floor, true).update_available);
    }

    #[test]
    fn a_tarball_that_fails_verification_is_refused_and_the_current_version_stays() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        release(&fixture, "0.3.290");
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();

        // npm lists another digest for the CLI's tarball.
        let cli = cli_bytes("0.3.291", 4096);
        let (sdk, cli_tgz) = (sdk_tarball("0.3.291", &cli, |_| {}), cli_tarball(&cli));
        publish_with(&fixture, "0.3.291", &sdk, &cli_tgz, integrity(&sdk), integrity(b"another tarball"));
        let error = install(&ctx, Some("0.3.291"), &AtomicBool::new(false), &mut quiet).unwrap_err();
        assert!(error.contains("sha512"), "{error}");
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.290"));
        assert!(!ctx.dir.join("0.3.291").exists());
        let part = ctx.dir.join(DOWNLOADS).join(format!("claude-agent-sdk-{}-0.3.291.tgz.part", platform_tag()));
        assert!(!part.exists(), "bytes that failed are not kept");

        // The SDK's manifest lists another build of the CLI for this platform.
        let cli = cli_bytes("0.3.292", 4096);
        publish(&fixture, "0.3.292", &sdk_tarball("0.3.292", b"another build", |_| {}), &cli_tarball(&cli));
        let error = install(&ctx, Some("0.3.292"), &AtomicBool::new(false), &mut quiet).unwrap_err();
        assert!(error.contains("清单"), "{error}");
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.290"));
        assert!(!ctx.dir.join("0.3.292").exists());
        no_leftovers(&ctx.dir);

        // An SDK whose package.json names another version.
        let cli = cli_bytes("0.3.293", 4096);
        publish(&fixture, "0.3.293", &sdk_tarball("0.3.299", &cli, |_| {}), &cli_tarball(&cli));
        let error = install(&ctx, Some("0.3.293"), &AtomicBool::new(false), &mut quiet).unwrap_err();
        assert!(error.contains("0.3.299"), "{error}");
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.290"));

        // Outside the range, or a prerelease: refused before npm is asked.
        let asked = fixture.hits("/");
        for version in ["0.4.0", "0.3.283", "0.3.300-beta.1", "latest", "../0.3.290"] {
            assert!(install(&ctx, Some(version), &AtomicBool::new(false), &mut quiet).is_err(), "{version}");
        }
        assert_eq!(fixture.hits("/"), asked);
        assert_eq!(installed_runtime(&ctx.dir).map(|runtime| runtime.sdk_version).as_deref(), Some("0.3.290"));
    }

    #[test]
    fn an_entry_that_could_land_outside_the_package_is_refused() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let cases: [(&[u8], tar::EntryType); 5] = [
            (b"package/../evil.txt", tar::EntryType::Regular),
            (b"/evil.txt", tar::EntryType::Regular),
            (b"package/lib/../../evil.txt", tar::EntryType::Regular),
            (b"package/link", tar::EntryType::Symlink),
            (b"package/sdk.mjs:stream", tar::EntryType::Regular),
        ];
        for (index, (name, kind)) in cases.into_iter().enumerate() {
            let version = format!("0.3.{}", 300 + index);
            let cli = cli_bytes(&version, 1024);
            let sdk = sdk_tarball(&version, &cli, |builder| add_raw(builder, name, kind));
            publish(&fixture, &version, &sdk, &cli_tarball(&cli));
            let error = install(&ctx, Some(&version), &AtomicBool::new(false), &mut quiet).unwrap_err();
            let shown = String::from_utf8_lossy(name);
            assert!(error.contains("package/"), "{shown}: {error}");
            assert!(current_version(&ctx.dir).is_none(), "{shown}");
            assert!(!ctx.dir.join(&version).exists(), "{shown}");
            assert!(!ctx.dir.join("evil.txt").exists() && !root.path().join("evil.txt").exists(), "{shown}");
            no_leftovers(&ctx.dir);
        }
    }

    #[test]
    fn package_paths_stay_inside_the_package() {
        assert_eq!(package_path(b"package/sdk.mjs"), Some(vec!["sdk.mjs"]));
        assert_eq!(package_path(b"package/lib/x.d.ts"), Some(vec!["lib", "x.d.ts"]));
        assert_eq!(package_path(b"package/lib/"), Some(vec!["lib"]));
        assert_eq!(package_path(b"package/"), Some(Vec::<&str>::new()));
        assert_eq!(package_path(b"package/console.log"), Some(vec!["console.log"]));
        for bad in [
            "package/../x",
            "/package/x",
            "x/sdk.mjs",
            "packagex/y",
            "package//x",
            "package/./x",
            "package/a\\..\\..\\x",
            "package/C:x",
            "package/con",
            "package/NUL.txt",
            "package/com1",
            "package/x.",
            "package/x ",
            "package/a\u{1}b",
        ] {
            assert!(package_path(bad.as_bytes()).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_newest_compatible_release_is_offered_and_prereleases_never() {
        let floor = Version::parse(FLOOR).unwrap();
        let versions = |list: &[&str]| list.iter().map(|text| Version::parse(text).unwrap()).collect::<Vec<_>>();
        let found = select(
            &versions(&["0.2.99", "0.3.284", "0.3.290", "0.3.291-beta.1", "0.4.0", "0.4.1-rc.1", "1.0.0-alpha"]),
            &floor,
        );
        assert_eq!(found.latest, Some(Version::new(0, 3, 290)));
        assert_eq!(found.newer_incompatible, Some(Version::new(0, 4, 0)));
        let found = select(&versions(&["0.3.284", "0.3.285"]), &floor);
        assert_eq!((found.latest, found.newer_incompatible), (Some(Version::new(0, 3, 285)), None));
        // A registry behind the sidecar: nothing to offer, nothing beyond.
        let found = select(&versions(&["0.2.99", "0.3.100"]), &floor);
        assert_eq!((found.latest, found.newer_incompatible), (None, None));
        let found = select(&versions(&["0.2.99", "0.4.0"]), &floor);
        assert_eq!((found.latest, found.newer_incompatible), (None, Some(Version::new(0, 4, 0))));

        assert_eq!(compatible_range(&floor).to_string(), "^0.3.284");
        assert!(compatible_version("0.3.300", &floor).is_ok());
        assert!(compatible_version("0.3.300-beta.1", &floor).is_err());
        assert!(compatible_version("0.3.300+build", &floor).is_err());
        assert!(compatible_version("0.4.0", &floor).is_err());
        assert!(compatible_version("0.3.200", &floor).is_err());
    }

    #[test]
    fn the_version_list_is_read_only_when_latest_is_not_compatible() {
        let fixture = Fixture::start();
        let npm = registries(&[fixture.base.as_str()]);
        let floor = Version::parse(FLOOR).unwrap();
        release(&fixture, "0.3.289");
        tag_latest(&fixture, "0.4.0");
        fixture.json(
            &format!("/{SDK_PACKAGE}"),
            json!({ "name": SDK_PACKAGE, "versions": {
                "0.2.1": {}, "0.3.284": {}, "0.3.289": {}, "0.3.290": { "deprecated": "broken" },
                "0.3.291-beta.1": {}, "0.4.0": {}
            }}),
        );
        let status = status_with(None, &npm, &floor, true);
        assert_eq!(
            status.latest,
            Some(AvailableVersion { sdk_version: "0.3.289".into(), claude_code_version: Some("2.1.289".into()) })
        );
        assert_eq!(status.newer_incompatible.as_deref(), Some("0.4.0"));
        assert_eq!(status.latest_error, None);
        let list = format!("/{SDK_PACKAGE}");
        let listed = || fixture.hits.lock().unwrap().iter().filter(|path| **path == list).count();
        assert_eq!(listed(), 1);

        tag_latest(&fixture, "0.3.289");
        let status = status_with(None, &npm, &floor, true);
        assert_eq!(status.latest.map(|latest| latest.sdk_version).as_deref(), Some("0.3.289"));
        assert_eq!(status.newer_incompatible, None);
        assert_eq!(listed(), 1, "a compatible latest tag answers without the list");

        // Nothing compatible: said, not left blank.
        tag_latest(&fixture, "0.2.1");
        fixture.json(&list, json!({ "versions": { "0.2.1": {} } }));
        let status = status_with(None, &npm, &floor, true);
        assert!(status.latest.is_none());
        assert!(status.latest_error.is_some_and(|error| error.contains("^0.3.284")));
    }

    #[test]
    fn a_registry_that_cannot_be_reached_hands_over_and_one_that_answers_is_believed() {
        let dead = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let failing = Fixture::start();
        failing.route(&format!("/-/package/{SDK_PACKAGE}/dist-tags"), Reply::Status(503));
        let live = Fixture::start();
        release(&live, "0.3.290");
        tag_latest(&live, "0.3.290");
        let floor = Version::parse(FLOOR).unwrap();

        let npm = registries(&[dead.as_str(), failing.base.as_str(), live.base.as_str()]);
        let status = status_with(None, &npm, &floor, true);
        assert_eq!(status.latest.map(|latest| latest.sdk_version).as_deref(), Some("0.3.290"));
        assert_eq!(failing.hits("/"), 1);

        // "No such package" is an answer: the next registry is not asked.
        let missing = Fixture::start();
        let npm = registries(&[missing.base.as_str(), live.base.as_str()]);
        let asked = live.hits("/");
        let status = status_with(None, &npm, &floor, true);
        assert!(status.latest.is_none());
        assert!(status.latest_error.is_some());
        assert_eq!(live.hits("/"), asked);

        // An install takes the same way round.
        let root = tempfile::tempdir().unwrap();
        let ctx = Context {
            npm: registries(&[failing.base.as_str(), live.base.as_str()]),
            ..context(&live, root.path())
        };
        install(&ctx, None, &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.290"));
    }

    #[test]
    fn a_cancelled_install_leaves_nothing_half_done() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let cli = cli_bytes("0.3.290", 3 << 20);
        publish(&fixture, "0.3.290", &sdk_tarball("0.3.290", &cli, |_| {}), &cli_tarball(&cli));

        let cancel = AtomicBool::new(false);
        let error = install(&ctx, Some("0.3.290"), &cancel, &mut |progress| {
            if matches!(progress, Progress::Bytes(received, _) if received > 0) {
                cancel.store(true, Ordering::Release);
            }
        })
        .unwrap_err();
        assert_eq!(error, CANCELLED);
        assert!(current_version(&ctx.dir).is_none());
        assert!(!ctx.dir.join("0.3.290").exists());
        no_leftovers(&ctx.dir);

        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).expect("the next attempt finishes");
        assert_eq!(fs::read(installed_runtime(&ctx.dir).unwrap().cli).unwrap(), cli);
    }

    fn wait_for(what: &str, done: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < until, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The only test that runs the global task.
    #[test]
    fn one_install_runs_at_a_time_and_a_cancel_is_not_a_failure() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let cli = cli_bytes("0.3.290", 4096);
        publish(&fixture, "0.3.290", &sdk_tarball("0.3.290", &cli, |_| {}), &cli_tarball(&cli));
        let gate = Arc::new(Gate::default());
        fixture.route("/tarballs/cli-0.3.290.tgz", Reply::Gated(cli_tarball(&cli), Arc::clone(&gate)));

        start_install_with(context(&fixture, root.path()), Some("0.3.290".into())).expect("starts");
        wait_for("the download", || task().running.as_ref().is_some_and(|running| running.phase == Phase::Downloading));
        let running = task().running.clone().unwrap();
        assert_eq!((running.action, running.sdk_version.as_str()), (TaskAction::Install, "0.3.290"));
        let second = start_install_with(context(&fixture, root.path()), None).unwrap_err();
        assert!(second.contains("正在安装"), "{second}");

        cancel_install();
        gate.open();
        wait_for("the install to end", || task().running.is_none());
        assert_eq!(task().last_error, None);
        assert!(current_version(&root.path().join(DIR)).is_none());
    }

    #[test]
    fn integrity_reads_the_sha512_of_an_npm_listing() {
        let digest = Sha512::digest(b"x").to_vec();
        let listed = integrity(b"x");
        assert_eq!(sha512_of(Some(&listed)), Some(digest.clone()));
        assert_eq!(sha512_of(Some(&format!("sha1-abc= {listed}"))), Some(digest));
        assert_eq!(sha512_of(Some("sha1-abc=")), None);
        assert_eq!(sha512_of(Some("sha512-bm90IGEgZGlnZXN0")), None);
        assert_eq!(sha512_of(None), None);
    }

    #[test]
    fn a_tarball_comes_over_https_or_from_a_plain_registry_itself() {
        let allowed = |registry: &str, url: &str| check_tarball_url(registry, url).is_ok();
        assert!(allowed("https://registry.npmjs.org", "https://cdn.example/x.tgz"));
        assert!(!allowed("https://registry.npmjs.org", "http://registry.npmjs.org/x.tgz"));
        assert!(allowed("http://127.0.0.1:9", "http://127.0.0.1:9/x.tgz"));
        assert!(!allowed("http://127.0.0.1:9", "http://127.0.0.1:10/x.tgz"));
        assert!(!allowed("http://127.0.0.1:9", "file:///x.tgz"));
    }

    /// What npm itself signed for `@anthropic-ai/claude-agent-sdk@0.3.284`
    /// (`dist.signatures`, as registry.npmjs.org and registry.npmmirror.com
    /// both serve it), checked with the key this host carries.
    #[test]
    fn npms_own_key_verifies_what_npm_signed() {
        let doc: VersionDoc = serde_json::from_value(json!({
            "name": SDK_PACKAGE,
            "version": "0.3.284",
            "dist": {
                "tarball": "https://registry.npmjs.org/@anthropic-ai/claude-agent-sdk/-/claude-agent-sdk-0.3.284.tgz",
                "integrity": "sha512-NSoJwEq6nFSf8dtaacYx37QdGgqApI3eHFUlxclMLYi8irb6ZJwEaUnPnLUCyAyg9W/tMkgZC0GXWLRCc01I0w==",
                "signatures": [{
                    "keyid": NPM_KEY_ID,
                    "sig": "MEYCIQCfAYrIt9KuS55SWqemgG+65Gh6WKVwPyegn1fBZ5ihQwIhAM3m5emgeXxyEtsev4x9f0B/4nAXAKmCUL29kiITEopQ"
                }]
            }
        }))
        .unwrap();
        let npm = RegistryKey::npm();
        assert_eq!(npm.point.len(), 65);
        assert_eq!(npm.point[0], 4, "an uncompressed point");
        assert!(signed_by(&doc, &[npm.clone()]));
        // The same signature over another tarball's digest, or by another key, is no signature.
        let mut forged: VersionDoc = serde_json::from_value(json!({
            "name": SDK_PACKAGE,
            "version": "0.3.284",
            "dist": { "tarball": "x", "integrity": integrity(b"another tarball"), "signatures": [{
                "keyid": NPM_KEY_ID,
                "sig": "MEYCIQCfAYrIt9KuS55SWqemgG+65Gh6WKVwPyegn1fBZ5ihQwIhAM3m5emgeXxyEtsev4x9f0B/4nAXAKmCUL29kiITEopQ"
            }]}
        }))
        .unwrap();
        assert!(!signed_by(&forged, &[npm.clone()]));
        assert!(!signed_by(&doc, &[test_key()]));
        forged.dist.integrity = doc.dist.integrity.clone();
        forged.version = "0.3.285".into();
        assert!(!signed_by(&forged, &[npm]));
    }

    #[test]
    fn a_version_npm_did_not_sign_is_refused() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let (base, platform) = (fixture.base.clone(), platform_tag());
        let stranger = generate_key();
        let cases: [(&str, fn(Value, &ring::signature::EcdsaKeyPair) -> Value); 5] = [
            // No signature at all.
            ("0.3.290", |doc, _| doc),
            // Signed by a key that is not the registry's, under the registry key's id.
            ("0.3.291", |mut doc, stranger| {
                let sig = signature_of(&doc, stranger);
                doc["dist"]["signatures"] = json!([{ "keyid": TEST_KEY_ID, "sig": sig }]);
                doc
            }),
            // Signed by the registry's key under another id.
            ("0.3.292", |mut doc, _| {
                let sig = signature_of(&doc, signer());
                doc["dist"]["signatures"] = json!([{ "keyid": "SHA256:another", "sig": sig }]);
                doc
            }),
            // A genuine signature over another digest.
            ("0.3.293", |doc, _| {
                let mut doc = signed(doc);
                doc["dist"]["integrity"] = json!(integrity(b"a tampered tarball"));
                doc
            }),
            // Not base64.
            ("0.3.294", |mut doc, _| {
                doc["dist"]["signatures"] = json!([{ "keyid": TEST_KEY_ID, "sig": "%%%" }]);
                doc
            }),
        ];
        for (version, sign) in cases {
            let cli = cli_bytes(version, 1024);
            let (sdk, cli_tgz) = (sdk_tarball(version, &cli, |_| {}), cli_tarball(&cli));
            publish(&fixture, version, &sdk, &cli_tgz);
            // The CLI's document as the registry serves it, signed (or not) as the case says.
            let doc = json!({
                "name": format!("{SDK_PACKAGE}-{platform}"),
                "version": version,
                "dist": { "tarball": format!("{base}/tarballs/cli-{version}.tgz"), "integrity": integrity(&cli_tgz) }
            });
            fixture.json(&format!("/{SDK_PACKAGE}-{platform}/{version}"), sign(doc, &stranger));
            let fetched = fixture.hits("/tarballs/");
            let error = install(&ctx, Some(version), &AtomicBool::new(false), &mut quiet).unwrap_err();
            assert!(error.contains("签名") || error.contains("signature"), "{version}: {error}");
            assert_eq!(fixture.hits("/tarballs/"), fetched, "{version}: nothing is downloaded");
            assert!(current_version(&ctx.dir).is_none());
        }

        // The latest check believes the registry no more than an install does.
        tag_latest(&fixture, "0.3.295");
        let cli = cli_bytes("0.3.295", 1024);
        publish(&fixture, "0.3.295", &sdk_tarball("0.3.295", &cli, |_| {}), &cli_tarball(&cli));
        let path = format!("/{SDK_PACKAGE}/0.3.295");
        let Some(Reply::Body(body)) = fixture.routes.lock().unwrap().get(&path).cloned() else {
            panic!("0.3.295 is published");
        };
        let mut doc: Value = serde_json::from_slice(&body).unwrap();
        doc["dist"].as_object_mut().unwrap().remove("signatures");
        fixture.json(&path, doc);
        let status = status_with(None, &ctx.npm, &ctx.floor, true);
        assert!(status.latest.is_none());
        assert!(status.latest_error.is_some_and(|error| error.contains("签名") || error.contains("signature")));
    }

    #[test]
    fn an_installed_version_outside_the_range_is_refused_and_offered_an_update() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        release(&fixture, "0.3.290");
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(runtime_in(Some(&ctx.dir), &ctx.floor).unwrap().sdk_version, "0.3.290");

        // A sidecar built against a newer SDK release no longer takes it.
        let newer = Version::parse("0.3.295").unwrap();
        let error = runtime_in(Some(&ctx.dir), &newer).unwrap_err();
        assert!(error.contains("0.3.290") && error.contains("^0.3.295"), "{error}");
        release(&fixture, "0.3.296");
        tag_latest(&fixture, "0.3.296");
        let status = status_with(Some(&ctx.dir), &ctx.npm, &newer, true);
        let installed = status.installed.clone().expect("still shown as installed");
        assert!(!installed.compatible);
        assert!(status.update_available);
        assert_eq!(serde_json::to_value(&status).unwrap()["installed"]["compatible"], false);

        // Nor does one built against an older release take what is newer than its range: the
        // newest compatible release is the update, though it is older.
        let older = Version::parse("0.2.9").unwrap();
        tag_latest(&fixture, "0.2.12");
        release(&fixture, "0.2.12");
        let status = status_with(Some(&ctx.dir), &ctx.npm, &older, true);
        assert_eq!(status.latest.map(|latest| latest.sdk_version).as_deref(), Some("0.2.12"));
        assert!(status.update_available);
        assert!(runtime_in(Some(&ctx.dir), &older).is_err());
    }

    /// Other bytes of the same size, written after the CLI was installed.
    fn tamper(cli: &Path) {
        let mut bytes = fs::read(cli).unwrap();
        bytes[0] ^= 0xff;
        fs::write(cli, bytes).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        fs::File::options().write(true).open(cli).unwrap().set_modified(later).unwrap();
    }

    #[test]
    fn a_cli_that_is_not_the_installed_file_is_refused_and_installed_again() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let cli = release(&fixture, "0.3.290");
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();
        let runtime = runtime_in(Some(&ctx.dir), &ctx.floor).unwrap();
        tamper(&runtime.cli);
        let error = runtime_in(Some(&ctx.dir), &ctx.floor).unwrap_err();
        assert!(error.contains("Claude Code") && error.contains("Claude Agent"), "{error}");
        // The page offers to install it, which puts the real file back.
        let shown = installed_version(Some(&ctx.dir), &ctx.floor);
        assert!(shown.is_none_or(|shown| shown.source == InstalledSource::Development));
        let fetched = fixture.hits("/tarballs/");
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert!(fixture.hits("/tarballs/") > fetched, "downloaded again, not switched to");
        assert_eq!(fs::read(&runtime.cli).unwrap(), cli);
        assert!(runtime_in(Some(&ctx.dir), &ctx.floor).is_ok());

        // A record that never said what was installed vouches for nothing.
        let record_path = ctx.dir.join("0.3.290").join(INSTALL_RECORD);
        let mut record: Value = super::super::read_json(&record_path).unwrap();
        record.as_object_mut().unwrap().remove("claudeSha256");
        super::super::write_json_atomic(&record_path, &record).unwrap();
        assert!(runtime_in(Some(&ctx.dir), &ctx.floor).is_err());
    }

    #[test]
    fn a_switch_keeps_the_version_before_and_every_version_this_process_ran() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        for version in ["0.3.290", "0.3.291", "0.3.292", "0.3.293"] {
            release(&fixture, version);
        }
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();
        install(&ctx, Some("0.3.291"), &AtomicBool::new(false), &mut quiet).unwrap();
        // Reinstalling the current version says nothing of which came before it.
        install(&ctx, Some("0.3.291"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert!(ctx.dir.join("0.3.290").is_dir(), "the real previous version stays");
        // A session started now keeps running 0.3.291 however often the version changes.
        runtime_in(Some(&ctx.dir), &ctx.floor).unwrap();
        install(&ctx, Some("0.3.292"), &AtomicBool::new(false), &mut quiet).unwrap();
        install(&ctx, Some("0.3.293"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(current_version(&ctx.dir).as_deref(), Some("0.3.293"));
        assert!(ctx.dir.join("0.3.292").is_dir(), "the version before");
        assert!(ctx.dir.join("0.3.291").join(cli_name()).is_file(), "the version a session runs");
        assert!(!ctx.dir.join("0.3.290").exists());
    }

    #[test]
    fn a_leftover_directory_of_the_version_is_replaced() {
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        let cli = release(&fixture, "0.3.290");
        // What a removal that stopped halfway leaves: no record, no SDK.
        let leftover = ctx.dir.join("0.3.290");
        fs::create_dir_all(&leftover).unwrap();
        fs::write(leftover.join("stray.txt"), b"left over").unwrap();
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap();
        assert_eq!(fs::read(leftover.join(cli_name())).unwrap(), cli);
        assert!(!leftover.join("stray.txt").exists());
    }

    /// A Claude Code still running from a leftover directory holds its files: the install says to
    /// restart Mewrk, which ends every session it started.
    #[cfg(windows)]
    #[test]
    fn a_leftover_directory_in_use_asks_for_a_restart() {
        use std::os::windows::fs::OpenOptionsExt as _;
        let fixture = Fixture::start();
        let root = tempfile::tempdir().unwrap();
        let ctx = context(&fixture, root.path());
        release(&fixture, "0.3.290");
        let leftover = ctx.dir.join("0.3.290");
        fs::create_dir_all(&leftover).unwrap();
        let held = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .share_mode(0)
            .open(leftover.join(cli_name()))
            .unwrap();
        let error = install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).unwrap_err();
        assert!(error.contains("重启") || error.contains("Restart"), "{error}");
        assert!(current_version(&ctx.dir).is_none());
        no_leftovers(&ctx.dir);
        drop(held);
        install(&ctx, Some("0.3.290"), &AtomicBool::new(false), &mut quiet).expect("installs once the files are free");
    }

    #[test]
    fn the_pinned_floor_is_a_release() {
        assert_eq!(pinned_floor().to_string(), env!("MEWRK_CLAUDE_AGENT_SDK_VERSION"));
        assert!(platform_tag().starts_with(host_platform().npm_platform_tag()));
    }

    /// What `stage` relies on, against the real packages: the SDK's manifest
    /// lists the SHA-256 and size of the unpacked CLI in its own platform
    /// package, and the source tree holds the SDK this host was built with.
    #[cfg(debug_assertions)]
    #[test]
    fn the_source_tree_cli_is_the_build_its_sdk_lists() {
        let runtime = development_runtime().expect("aisdk-service/node_modules 里应有 Agent SDK 和它的平台包");
        assert_eq!(runtime.sdk_version, env!("MEWRK_CLAUDE_AGENT_SDK_VERSION"));
        let manifest: Manifest =
            super::super::read_json(&runtime.sdk_entry.with_file_name("manifest.json")).expect("manifest.json");
        let build = &manifest.platforms[&platform_tag()];
        assert_eq!(build.size, Some(fs::metadata(&runtime.cli).unwrap().len()));
        let digest = crate::helper_model::download::sha256_file(&runtime.cli).unwrap();
        assert_eq!(build.checksum.as_deref(), Some(digest.as_str()));
    }

    /// The whole install against the real registries (`MEWRK_NPM_REGISTRY`
    /// honoured): ~100 MB, so only on request,
    /// `cargo test --lib components::claude_agent -- --ignored`.
    #[test]
    #[ignore = "downloads the Claude Agent SDK and its CLI from npm"]
    fn installs_the_newest_compatible_release_from_npm() {
        let root = tempfile::tempdir().unwrap();
        let ctx =
            Context { dir: root.path().join(DIR), npm: Npm::from_env(), floor: pinned_floor(), platform: platform_tag() };
        let status = status_with(Some(&ctx.dir), &ctx.npm, &ctx.floor, true);
        let latest = status.latest.expect("npm has a compatible release");
        install(&ctx, None, &AtomicBool::new(false), &mut quiet).expect("installs from npm");
        let runtime = installed_runtime(&ctx.dir).expect("installed");
        assert_eq!(runtime.sdk_version, latest.sdk_version);
        assert_eq!(runtime.claude_code_version, latest.claude_code_version);
        let output = std::process::Command::new(&runtime.cli)
            .arg("--version")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .output()
            .expect("runs");
        let reported = String::from_utf8_lossy(&output.stdout);
        assert!(reported.trim().starts_with(latest.claude_code_version.as_deref().unwrap_or("?")), "{reported}");
    }
}
