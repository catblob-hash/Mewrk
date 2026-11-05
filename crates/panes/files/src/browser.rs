//! The file pane's view of the machines it browses and the files on them (upstream's `lib/fileBrowser.ts`).
//!
//! Every path is absolute on its machine and spelled the way the host's file service spells it: `/` separated, a Windows
//! machine's with an upper-case drive (`C:/Users/dev`), where `/` alone is the list of that machine's drives. The host
//! normalizes; the renderer compares and prints.

use mewrk_client::model::{RunTarget, SshMachineConfig};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::LazyLock;

/// A machine the pane can browse: this computer (`None`), a WSL distribution, a registered SSH machine.
pub type BrowseMachine = Option<RunTarget>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowseEntryKind {
    Directory,
    File,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseEntry {
    pub name: String,
    /// The entry's own absolute path.
    pub path: String,
    pub kind: BrowseEntryKind,
    /// A symbolic link; `kind` is what it leads to.
    pub link: bool,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseListing {
    pub path: String,
    pub parent: Option<String>,
    /// Whether the machine spells paths the Windows way.
    pub windows: bool,
    pub entries: Vec<BrowseEntry>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseTextFile {
    pub path: String,
    pub content: String,
    pub truncated: bool,
    pub size: u64,
    pub binary: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseFileBytes {
    pub path: String,
    pub data: String,
    pub size: u64,
    pub too_large: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseSearchMatch {
    pub name: String,
    /// Relative to the searched directory, `/` separated.
    pub path: String,
    pub kind: BrowseEntryKind,
    pub positions: Vec<u32>,
    pub score: f64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseSearchResults {
    pub query: String,
    pub root: String,
    pub matches: Vec<BrowseSearchMatch>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowseProbeTarget {
    pub machine: BrowseMachine,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseProbeResult {
    /// Normalized on its machine: `~` expanded, `..` applied.
    pub path: String,
    pub kind: Option<BrowseEntryKind>,
    /// False when the machine could not be asked in time.
    pub reached: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenWithApp {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub default: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenWithChoices {
    pub apps: Vec<OpenWithApp>,
    /// Whether the system has a chooser of its own for everything else.
    pub chooser: bool,
}

/// Host-side cap; asking for more is clamped there.
pub const SEARCH_LIMIT: usize = 200;

// The argument shapes every `browse_*` command takes, kept in one place the way upstream's wrappers do.
pub fn list_args(machine: &BrowseMachine, path: &str) -> Value {
    json!({ "machine": machine, "path": path })
}

pub fn search_args(machine: &BrowseMachine, root: &str, query: &str) -> Value {
    json!({ "machine": machine, "root": root, "query": query, "limit": SEARCH_LIMIT })
}

pub fn probe_args(targets: &[BrowseProbeTarget], patient: bool) -> Value {
    json!({ "targets": targets, "patient": patient })
}

// ---------------------------------------------------------------- machines

/// A machine's identity: `local`, `wsl:<distro>` or `ssh:<id>`, as the host keys it.
pub fn machine_key(machine: &BrowseMachine) -> String {
    match machine {
        None => "local".to_owned(),
        Some(RunTarget::Wsl { distro }) => format!("wsl:{distro}"),
        Some(RunTarget::Ssh { machine_id }) => format!("ssh:{machine_id}"),
    }
}

pub fn same_machine(left: &BrowseMachine, right: &BrowseMachine) -> bool {
    machine_key(left) == machine_key(right)
}

/// A file somewhere: the key every cache in the pane is kept under.
pub fn location_key(machine: &BrowseMachine, path: &str) -> String {
    format!("{}\u{0}{path}", machine_key(machine))
}

/// The path half of a key.
pub fn key_path(key: &str) -> &str {
    key.split_once('\u{0}').map_or(key, |(_, path)| path)
}

/// The machine half of a key.
pub fn key_machine(key: &str) -> &str {
    key.split_once('\u{0}').map_or("", |(machine, _)| machine)
}

/// The machine a key names, or `None` when it names none (upstream's `machineFromKey`).
pub fn machine_from_key(key: Option<&str>) -> Option<BrowseMachine> {
    let key = key?;
    if key == "local" {
        return Some(None);
    }
    if let Some(distro) = key.strip_prefix("wsl:").filter(|d| !d.is_empty()) {
        return Some(Some(RunTarget::Wsl { distro: distro.to_owned() }));
    }
    if let Some(id) = key.strip_prefix("ssh:").filter(|d| !d.is_empty()) {
        return Some(Some(RunTarget::Ssh { machine_id: id.to_owned() }));
    }
    None
}

// ---------------------------------------------------------------- paths

/// Whether `path` is spelled the Windows way: a drive, then a separator or nothing.
pub fn is_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'/' || b[2] == b'\\')
}

fn is_drive_root(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() == 2 || (b.len() == 3 && b[2] == b'/')) && b[0].is_ascii_uppercase() && b[1] == b':'
}

/// A path in the host's spelling, for comparing one this renderer was handed (a workspace root as the document records
/// it, `C:\Projects\Mewrk`, or as the host canonicalized it, `\\?\C:\Projects\Mewrk`) with one the file service
/// answered. Only the spelling changes: `~` and `..` stay for the host.
pub fn browse_path(raw: &str) -> String {
    let mut path = raw.trim().to_owned();
    let b = path.as_bytes();
    if b.len() >= 4 && matches!(b[0], b'\\' | b'/') && matches!(b[1], b'\\' | b'/') && b[2] == b'?' && matches!(b[3], b'\\' | b'/') {
        path = path[4..].to_owned();
    }
    if is_windows_path(&path) {
        path = path.replace('\\', "/");
        path = format!("{}{}", path[..1].to_ascii_uppercase(), &path[1..]);
        if path.len() == 2 {
            path.push('/');
        }
    }
    // Runs of slashes collapse, except at the very start.
    let mut out = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    if let Some(first) = chars.next() {
        out.push(first);
    }
    let mut previous_slash = false;
    let mut first_char_slash_run = out == "/";
    for c in chars {
        if c == '/' {
            if previous_slash {
                continue;
            }
            // `(.)\/{2,}` needs a character before the run, so a leading run is left alone.
            if first_char_slash_run {
                out.push(c);
                continue;
            }
            previous_slash = true;
        } else {
            previous_slash = false;
            first_char_slash_run = false;
        }
        out.push(c);
    }
    let drive_root = out.len() == 3 && is_drive_root(&out);
    if out.len() > 1 && !drive_root {
        let trimmed = out.trim_end_matches('/');
        return if trimmed.is_empty() { "/".to_owned() } else { trimmed.to_owned() };
    }
    out
}

/// `dir` and `name` with one separator between them.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

/// Where "up" leads from `path`: `C:/` goes to the drives, `/` to the list of machines (`None`).
pub fn parent_path(path: &str) -> Option<String> {
    if path == "/" || path.is_empty() {
        return None;
    }
    if is_drive_root(path) {
        return Some("/".to_owned());
    }
    let trimmed = path.trim_end_matches('/');
    let cut = trimmed.rfind('/')?;
    if cut == 0 {
        return Some("/".to_owned());
    }
    let head = &trimmed[..cut];
    if is_drive_root(head) {
        return Some(format!("{head}/"));
    }
    Some(head.to_owned())
}

/// The last segment: a file's name, a drive's letter, `/` for a root.
pub fn browse_name(path: &str) -> String {
    if is_drive_root(path) {
        return path[..2].to_owned();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
}

/// Windows ignores case; everything else does not.
fn folded(path: &str) -> String {
    if is_windows_path(path) { path.to_lowercase() } else { path.to_owned() }
}

/// `path` relative to `root`, `""` for the root itself, or `None` when it is elsewhere.
pub fn relative_path(root: &str, path: &str) -> Option<String> {
    let base = folded(root);
    let base = base.trim_end_matches('/');
    let target = folded(path);
    if target == base || target == format!("{base}/") {
        return Some(String::new());
    }
    let prefix = format!("{base}/");
    if !target.starts_with(&prefix) {
        return None;
    }
    Some(path[prefix.len()..].to_owned())
}

/// Every directory between `root` (exclusive) and `path` (exclusive), outermost first.
pub fn browse_ancestors(root: &str, path: &str) -> Vec<String> {
    let Some(relative) = relative_path(root, path).filter(|r| !r.is_empty()) else { return Vec::new() };
    let segments: Vec<&str> = relative.split('/').collect();
    let mut ancestors = Vec::new();
    let mut current = root.to_owned();
    for segment in &segments[..segments.len() - 1] {
        current = join_path(&current, segment);
        ancestors.push(current.clone());
    }
    ancestors
}

/// Resolves a path written against `base` (a directory, absolute on the same machine). An absolute path stands alone;
/// `..` past the top stays at the top.
pub fn resolve_path(base: &str, written: &str) -> String {
    let raw = written.trim();
    let windows = is_windows_path(base) || is_windows_path(raw);
    let spelled = if windows { raw.replace('\\', "/") } else { raw.to_owned() };
    if spelled.starts_with('/') || is_windows_path(&spelled) || spelled.starts_with('~') {
        return browse_path(&spelled);
    }
    // A remote workspace may be recorded as `~/…`; its machine expands that.
    let home = base == "~" || base.starts_with("~/");
    let b = base.as_bytes();
    let drive = (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| &base[..2]);
    let start = if drive.is_some() { &base[2..] } else if home { &base[1..] } else { base };
    let mut segments: Vec<&str> = Vec::new();
    let joined = format!("{start}/{spelled}");
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let lead = drive.map(str::to_ascii_uppercase).unwrap_or_else(|| if home { "~".to_owned() } else { String::new() });
    format!("{lead}/{}", segments.join("/"))
}

/// A path as its machine writes it: backslashes on Windows.
pub fn native_spelling(path: &str) -> String {
    if is_windows_path(path) { path.replace('/', "\\") } else { path.to_owned() }
}

/// The top of the filesystem a path is on: `/`, or its drive.
pub fn filesystem_root(path: &str) -> String {
    if is_windows_path(path) { format!("{}/", &path[..2]) } else { "/".to_owned() }
}

// ---------------------------------------------------------------- addresses

/// What the address bar needs to know about the machines it can name.
#[derive(Clone, Debug, Default)]
pub struct AddressContext {
    pub ssh_machines: Vec<SshMachineConfig>,
    /// Whether this computer is Windows, which decides how its paths are printed.
    pub host_windows: bool,
}

/// How a location is written in the address bar, machine included: a path on this computer as the computer spells it,
/// `devbox:/srv/app` for an SSH machine (or `ssh://devbox:2222/srv/app` on a port of its own), and
/// `\\wsl.localhost\Ubuntu\home\dev` for a WSL distribution, which is how Windows itself names one.
pub fn format_address(machine: &BrowseMachine, path: &str, context: &AddressContext) -> String {
    match machine {
        None => {
            if context.host_windows && is_windows_path(path) { path.replace('/', "\\") } else { path.to_owned() }
        }
        Some(RunTarget::Wsl { distro }) => format!("\\\\wsl.localhost\\{distro}{}", path.replace('/', "\\")),
        Some(RunTarget::Ssh { machine_id }) => {
            let config = context.ssh_machines.iter().find(|m| m.id == *machine_id);
            let host = config.map_or(machine_id.as_str(), |c| c.host.as_str());
            let port = config.map_or(0, |c| c.port);
            if port != 0 && port != 22 {
                format!("ssh://{host}:{port}{}{path}", if path.starts_with('/') { "" } else { "/" })
            } else {
                format!("{host}:{path}")
            }
        }
    }
}

/// What an address names: the list of machines, a place on one, or nothing it can read.
#[derive(Clone, Debug, PartialEq)]
pub enum ParsedAddress {
    Machines,
    Location { machine: BrowseMachine, path: String },
    UnknownMachine { name: String },
}

/// An SSH machine the user wrote as `host`: its catalog address (`user@host`, `host`, an `~/.ssh/config` alias), the
/// host part of that address, or its name.
fn ssh_machine_named<'a>(written: &str, port: Option<u32>, machines: &'a [SshMachineConfig]) -> Option<&'a SshMachineConfig> {
    let wanted = written.trim().to_lowercase();
    let port_matches = |m: &SshMachineConfig| port.is_none_or(|p| (if m.port == 0 { 22 } else { m.port as u32 }) == p);
    let after_at = |s: &str| s.rsplit('@').next().unwrap_or("").to_owned();
    machines
        .iter()
        .find(|m| port_matches(m) && m.host.to_lowercase() == wanted)
        .or_else(|| machines.iter().find(|m| port_matches(m) && after_at(&m.host.to_lowercase()) == after_at(&wanted)))
        .or_else(|| machines.iter().find(|m| port_matches(m) && m.name.trim().to_lowercase() == wanted))
}

static WSL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)^(?:[\\/]{2}wsl(?:\.localhost|\$)[\\/]|wsl://)([^\\/]+)(.*)$").expect("regex"));
static SSH_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^ssh://([^/:]+(?:@[^/:]+)?)(?::(\d+))?(/.*)?$").expect("regex"));
static SCP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([^\s/\\:]{2,}|[^\s/\\:]+@[^\s/\\:]+):(.*)$").expect("regex"));

/// Reads what was typed into the address bar. A bare path is on this computer (the address bar prints this computer's
/// paths without a prefix, so that is how they are typed back) and a relative one is taken against `current`, the
/// directory the bar was showing.
pub fn parse_address(typed: &str, context: &AddressContext, current: Option<(&BrowseMachine, &str)>) -> ParsedAddress {
    let text = typed.trim();
    if text.is_empty() {
        return ParsedAddress::Machines;
    }
    if let Some(c) = WSL.captures(text) {
        let rest = c[2].replace('\\', "/");
        return ParsedAddress::Location { machine: Some(RunTarget::Wsl { distro: c[1].to_owned() }), path: if rest.is_empty() { "/".to_owned() } else { rest } };
    }
    if let Some(c) = SSH_URL.captures(text) {
        let port = c.get(2).and_then(|p| p.as_str().parse::<u32>().ok());
        let Some(machine) = ssh_machine_named(&c[1], port, &context.ssh_machines) else {
            return ParsedAddress::UnknownMachine { name: c[1].to_owned() };
        };
        // `ssh://host/~/notes` is the home-relative form scp and git write.
        let rest = c.get(3).map_or("/", |m| m.as_str());
        let path = if rest.starts_with("/~") { &rest[1..] } else { rest };
        return ParsedAddress::Location { machine: Some(RunTarget::Ssh { machine_id: machine.id.clone() }), path: if path.is_empty() { "~".to_owned() } else { path.to_owned() } };
    }
    if is_windows_path(text) {
        // A drive path on a page showing a Windows SSH machine stays on it.
        let machine = current.filter(|(m, p)| m.is_some() && is_windows_path(p)).and_then(|(m, _)| m.clone());
        return ParsedAddress::Location { machine, path: text.to_owned() };
    }
    // `host:path`, scp's form. A one-letter "host" is a drive, handled above.
    if let Some(c) = SCP.captures(text) {
        let Some(machine) = ssh_machine_named(&c[1], None, &context.ssh_machines) else {
            return ParsedAddress::UnknownMachine { name: c[1].to_owned() };
        };
        let path = if c[2].is_empty() { "~".to_owned() } else { c[2].to_owned() };
        return ParsedAddress::Location { machine: Some(RunTarget::Ssh { machine_id: machine.id.clone() }), path };
    }
    if text.starts_with('/') || text.starts_with('~') || (context.host_windows && text.starts_with('\\')) {
        return ParsedAddress::Location { machine: None, path: text.to_owned() };
    }
    if let Some((machine, path)) = current {
        return ParsedAddress::Location { machine: machine.clone(), path: resolve_path(path, text) };
    }
    ParsedAddress::Location { machine: None, path: text.to_owned() }
}

/// A machine's name in a menu or a tab: this computer, `WSL: Ubuntu`, an SSH machine's own name.
pub fn machine_label(machine: &BrowseMachine, ssh_machines: &[SshMachineConfig], this_computer: &str) -> String {
    match machine {
        None => this_computer.to_owned(),
        Some(RunTarget::Wsl { distro }) => format!("WSL: {distro}"),
        Some(RunTarget::Ssh { machine_id }) => ssh_machines.iter().find(|m| m.id == *machine_id).map_or_else(|| machine_id.clone(), |m| m.name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(id: &str, name: &str, host: &str, port: u16) -> SshMachineConfig {
        serde_json::from_value(json!({ "id": id, "name": name, "host": host, "port": port, "identityFile": "", "createdAt": "", "updatedAt": "" })).unwrap()
    }

    fn posix() -> AddressContext {
        AddressContext { ssh_machines: vec![ssh("ubuntu-id", "ubuntu", "dev@100.88.12.34", 0), ssh("win-id", "office-pc", "win", 0), ssh("lab-id", "lab", "lab.internal", 2222)], host_windows: false }
    }

    fn windows() -> AddressContext {
        AddressContext { host_windows: true, ..posix() }
    }

    fn s(id: &str) -> BrowseMachine {
        Some(RunTarget::Ssh { machine_id: id.to_owned() })
    }

    fn loc(machine: BrowseMachine, path: &str) -> ParsedAddress {
        ParsedAddress::Location { machine, path: path.to_owned() }
    }

    #[test]
    fn spells_a_path_the_way_the_hosts_file_service_does() {
        assert_eq!(browse_path("C:\\Projects\\Mewrk\\"), "C:/Projects/Mewrk");
        assert_eq!(browse_path("\\\\?\\c:\\Projects\\Mewrk"), "C:/Projects/Mewrk");
        assert_eq!(browse_path("c:"), "C:/");
        assert_eq!(browse_path("C:\\"), "C:/");
        assert_eq!(browse_path("/home/dev//mewrk/"), "/home/dev/mewrk");
        assert_eq!(browse_path("/"), "/");
        // A backslash is an ordinary character in a POSIX name.
        assert_eq!(browse_path("/srv/a\\b"), "/srv/a\\b");
    }

    #[test]
    fn goes_up_from_a_drive_to_the_drives_and_from_a_top_to_the_machines() {
        assert_eq!(parent_path("/home/dev").as_deref(), Some("/home"));
        assert_eq!(parent_path("/home").as_deref(), Some("/"));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path("C:/Users/dev").as_deref(), Some("C:/Users"));
        assert_eq!(parent_path("C:/Users").as_deref(), Some("C:/"));
        assert_eq!(parent_path("C:/").as_deref(), Some("/"));
    }

    #[test]
    fn names_the_last_segment_a_drive_by_its_letter() {
        assert_eq!(browse_name("/home/dev/notes.md"), "notes.md");
        assert_eq!(browse_name("C:/"), "C:");
        assert_eq!(browse_name("/"), "/");
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("C:/", "Users"), "C:/Users");
        assert_eq!(join_path("/home/dev", "a"), "/home/dev/a");
    }

    #[test]
    fn relates_a_path_to_a_root_ignoring_case_only_on_windows() {
        assert_eq!(relative_path("/home/dev/mewrk", "/home/dev/mewrk/src/App.tsx").as_deref(), Some("src/App.tsx"));
        assert_eq!(relative_path("/home/dev/mewrk", "/home/dev/mewrk").as_deref(), Some(""));
        assert_eq!(relative_path("/home/dev/mewrk", "/home/dev/mewrk-old/x"), None);
        assert_eq!(relative_path("/home/dev/Mewrk", "/home/dev/mewrk/x"), None);
        assert_eq!(relative_path("C:/Projects/Mewrk", "c:/projects/mewrk/src/x.ts").as_deref(), Some("src/x.ts"));
        assert_eq!(relative_path("/", "/etc/hosts").as_deref(), Some("etc/hosts"));
        assert_eq!(relative_path("C:/", "C:/Users").as_deref(), Some("Users"));
        assert_eq!(browse_ancestors("/w", "/w/a/b/c.ts"), ["/w/a", "/w/a/b"]);
        assert!(browse_ancestors("/w", "/elsewhere/c.ts").is_empty());
    }

    #[test]
    fn resolves_a_written_path_against_the_directory_it_was_written_in() {
        assert_eq!(resolve_path("/home/dev/mewrk", "src/App.tsx"), "/home/dev/mewrk/src/App.tsx");
        assert_eq!(resolve_path("/home/dev/mewrk", "../other/x"), "/home/dev/other/x");
        assert_eq!(resolve_path("/home/dev/mewrk", "/etc/hosts"), "/etc/hosts");
        assert_eq!(resolve_path("C:/Projects/Mewrk", "src\\x.ts"), "C:/Projects/Mewrk/src/x.ts");
        assert_eq!(resolve_path("~/mewrk", "src/x.ts"), "~/mewrk/src/x.ts");
        assert_eq!(resolve_path("/", "../../x"), "/x");
    }

    #[test]
    fn prints_this_computers_paths_bare_and_other_machines_with_their_identity() {
        assert_eq!(format_address(&None, "/home/dev", &posix()), "/home/dev");
        assert_eq!(format_address(&None, "C:/Users/dev", &windows()), "C:\\Users\\dev");
        assert_eq!(format_address(&s("ubuntu-id"), "/home/dev", &posix()), "dev@100.88.12.34:/home/dev");
        assert_eq!(format_address(&s("lab-id"), "/srv", &posix()), "ssh://lab.internal:2222/srv");
        assert_eq!(format_address(&Some(RunTarget::Wsl { distro: "Ubuntu".into() }), "/home/dev", &windows()), "\\\\wsl.localhost\\Ubuntu\\home\\dev");
    }

    #[test]
    fn reads_back_every_address_it_prints() {
        let cases = [
            (None, "/home/dev", posix()),
            (s("ubuntu-id"), "/home/dev", posix()),
            (s("lab-id"), "/srv", posix()),
            (s("win-id"), "C:/Users/dev", posix()),
            (Some(RunTarget::Wsl { distro: "Ubuntu".into() }), "/home/dev", windows()),
        ];
        for (machine, path, context) in cases {
            let printed = format_address(&machine, path, &context);
            assert_eq!(parse_address(&printed, &context, None), loc(machine, path), "{printed}");
        }
        assert_eq!(parse_address("C:\\Users\\dev", &windows(), None), loc(None, "C:\\Users\\dev"));
    }

    #[test]
    fn knows_an_ssh_machine_by_its_address_its_host_or_its_name() {
        let ubuntu = loc(s("ubuntu-id"), "/srv");
        let c = posix();
        assert_eq!(parse_address("dev@100.88.12.34:/srv", &c, None), ubuntu);
        assert_eq!(parse_address("100.88.12.34:/srv", &c, None), ubuntu);
        assert_eq!(parse_address("ubuntu:/srv", &c, None), ubuntu);
        assert_eq!(parse_address("ssh://dev@100.88.12.34/srv", &c, None), ubuntu);
        assert_eq!(parse_address("ubuntu:", &c, None), loc(s("ubuntu-id"), "~"));
        assert_eq!(parse_address("ssh://ubuntu/~/notes", &c, None), loc(s("ubuntu-id"), "~/notes"));
        assert_eq!(parse_address("nowhere:/srv", &c, None), ParsedAddress::UnknownMachine { name: "nowhere".into() });
        // The port picks between machines on one host.
        assert_eq!(parse_address("ssh://lab.internal:22/srv", &c, None), ParsedAddress::UnknownMachine { name: "lab.internal".into() });
    }

    #[test]
    fn takes_nothing_as_the_machines_and_a_relative_path_against_the_page() {
        let c = posix();
        assert_eq!(parse_address("  ", &c, None), ParsedAddress::Machines);
        let machine = s("ubuntu-id");
        assert_eq!(parse_address("src/../lib", &c, Some((&machine, "/srv/app"))), loc(machine.clone(), "/srv/app/lib"));
        assert_eq!(parse_address("/etc", &c, Some((&machine, "/srv/app"))), loc(None, "/etc"));
        assert_eq!(parse_address("\\\\wsl$\\Debian\\etc", &windows(), None), loc(Some(RunTarget::Wsl { distro: "Debian".into() }), "/etc"));
    }

    #[test]
    fn keys_machines_the_way_the_host_does() {
        assert_eq!(machine_key(&None), "local");
        assert_eq!(machine_key(&Some(RunTarget::Wsl { distro: "Ubuntu".into() })), "wsl:Ubuntu");
        assert_eq!(machine_key(&s("id")), "ssh:id");
        assert_eq!(machine_from_key(Some("local")), Some(None));
        assert_eq!(machine_from_key(Some("wsl:Ubuntu")), Some(Some(RunTarget::Wsl { distro: "Ubuntu".into() })));
        assert_eq!(machine_from_key(Some("elsewhere")), None);
    }
}
