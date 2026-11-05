//! Dev servers and remote screens as the pane sees them: upstream's `lib/preview.ts`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use mewrk_client::t;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

/// One workspace of a conversation, by the number the model addresses it with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTarget {
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_workspace_id: Option<String>,
    /// 1-based. Absent is workspace 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<u32>,
}

pub fn preview_target_key(target: &PreviewTarget) -> String {
    format!("{}\n{}\n{}", target.conversation_id, target.draft_workspace_id.as_deref().unwrap_or(""), target.workspace.unwrap_or(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreviewServerStatus {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlProtocol {
    Vnc,
    Adb,
    Wda,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlPlatform {
    Android,
    Ios,
    Desktop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RemoteControlPlace {
    Workspace,
    ThisComputer,
    Machine { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteControlState {
    Connecting,
    Credentials,
    Ready,
    Reconnecting,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRemoteControlConfiguration {
    pub protocol: RemoteControlProtocol,
    pub place: RemoteControlPlace,
    pub port: u32,
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub launch: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRemoteControl {
    pub protocol: RemoteControlProtocol,
    pub platform: RemoteControlPlatform,
    pub target: String,
    #[serde(default)]
    pub target_display: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub state: RemoteControlState,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub launch: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewConfiguredServer {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    /// Still the parser's `u32`: a port read out of a command line is not range-checked.
    #[serde(default)]
    pub port: u32,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub remote_control: Option<PreviewRemoteControlConfiguration>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewConfigurationList {
    #[serde(default)]
    pub launch_json_path: String,
    #[serde(default)]
    pub servers: Vec<PreviewConfiguredServer>,
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewServerSnapshot {
    pub handle: String,
    pub server_id: String,
    pub name: String,
    #[serde(default)]
    pub port: u32,
    pub status: PreviewServerStatus,
    /// When it started, which the tasks pane's row counts its uptime from.
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub workspace: Option<u32>,
    #[serde(default)]
    pub remote: Option<PreviewRemoteControl>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewAttachment {
    pub server_id: String,
    pub name: String,
    pub port: u32,
    pub url: String,
}

/// Untagged, like the host's own enum: an attach started no process, so it has no snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PreviewStartOutcome {
    Attached { attached: PreviewAttachment },
    Started { server: PreviewServerSnapshot, #[serde(default)] reused: bool },
}

/// The host's own line ceiling (`preview_servers::MAX_LOG_LINES`).
pub const PREVIEW_MAX_LOG_LINES: u32 = 200;
pub const PREVIEW_LOG_POLL_INTERVAL: Duration = Duration::from_millis(1000);
pub const PREVIEW_SERVER_POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// The arguments of `preview_list_configurations` and `preview_list_servers`.
pub fn target_args(target: &PreviewTarget) -> Value {
    json!({ "target": target })
}

pub fn start_args(target: &PreviewTarget, name: Option<&str>) -> Value {
    json!({ "target": target, "name": name })
}

pub fn stop_args(handle: &str) -> Value {
    json!({ "handle": handle })
}

pub fn logs_args(handle: &str, lines: u32) -> Value {
    json!({ "handle": handle, "errorsOnly": Value::Null, "search": Value::Null, "lines": lines })
}

/// Where a configured or running server answers.
pub fn preview_server_address(port: u32, url: Option<&str>) -> String {
    match url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(explicit) => explicit.to_owned(),
        None => format!("http://localhost:{port}"),
    }
}

/// `URL.origin` as a comparable string, for the schemes that have one.
fn origin_of(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let origin = parsed.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// Whether a page at `url` is served by a server answering at `address`. Origin equality, not string
/// equality: a route, a query, a trailing slash are still that server's page.
pub fn preview_url_is_served_at(url: Option<&str>, address: &str) -> bool {
    let Some(page) = url.map(str::trim).filter(|page| !page.is_empty() && *page != "about:blank") else {
        return false;
    };
    match (origin_of(page), origin_of(address)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// Whether a page at `url` is a page of the viewer at `address`: the same origin *and* under its path.
/// A viewer's port can be handed to a later session, so the origin alone can't say which device it is.
pub fn preview_url_is_served_at_path(url: Option<&str>, address: &str) -> bool {
    let Some(page) = url.map(str::trim).filter(|page| !page.is_empty() && *page != "about:blank") else {
        return false;
    };
    let (Ok(served), Ok(shown)) = (Url::parse(address), Url::parse(page)) else {
        return false;
    };
    if served.origin() != shown.origin() {
        return false;
    }
    let prefix = if served.path().ends_with('/') { served.path().to_owned() } else { format!("{}/", served.path()) };
    shown.path().starts_with(&prefix) || format!("{}/", shown.path()) == prefix
}

/// The one pairing rule for every place that joins a page to a server row.
pub fn preview_page_is_served_by(url: Option<&str>, server: &PreviewServerSnapshot) -> bool {
    if server.remote.is_none() {
        return preview_url_is_served_at(url, &preview_server_address(server.port, server.url.as_deref()));
    }
    match server.url.as_deref().map(str::trim).filter(|address| !address.is_empty()) {
        Some(address) => preview_url_is_served_at_path(url, address),
        None => false,
    }
}

/// The remote-control row whose viewer the page at `url` is showing, if it is showing one.
pub fn preview_viewer_server_of_page<'a>(url: Option<&str>, servers: &'a [PreviewServerSnapshot]) -> Option<&'a PreviewServerSnapshot> {
    servers.iter().find(|server| server.remote.is_some() && preview_page_is_served_by(url, server))
}

/// Whether the page showing `url`, on machine `page_machine`, is the view of `server`. A dev server is
/// paired by origin and machine: the same address on two machines is two servers. A viewer is always
/// on this computer, so it pairs by origin and token path alone.
pub fn page_is_served_by_server(url: Option<&str>, page_machine: &str, server: &PreviewServerSnapshot, server_machine: &str) -> bool {
    if server.remote.is_some() {
        return preview_page_is_served_by(url, server);
    }
    preview_page_is_served_by(url, server) && page_machine == server_machine
}

/// Whether two page addresses are the same document's, spelled however the host spells it.
pub fn preview_addresses_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (Url::parse(left), Url::parse(right)) {
        (Ok(left), Ok(right)) => left.as_str() == right.as_str(),
        _ => false,
    }
}

// ----- the mark and the mirror: per page, because a page outlives the pane that showed it ---------------

/// One document of a page's session history, as the pane remembers it. The host says only whether Back
/// and Forward exist, and the pane has to know before the move which side the entry belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct PageHistoryEntry {
    pub url: String,
    pub viewer: bool,
    /// Until this time the document may still be redirected. A time rather than a flag, because a page
    /// that never reports its load finished must not stay "loading" for good.
    settle_by: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct PageHistoryStatus<'a> {
    pub url: &'a str,
    pub loading: Option<bool>,
    pub can_go_back: Option<bool>,
    pub can_go_forward: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct PageHistory {
    entries: Vec<PageHistoryEntry>,
    index: usize,
}

const PAGE_HISTORY_LIMIT: usize = 200;
const PAGE_HISTORY_SETTLE: Duration = Duration::from_millis(5000);

/// Everything `preview.ts` kept at module level, by session.
#[derive(Default)]
pub struct PageMemory {
    viewer_network_sessions: std::collections::HashSet<String>,
    histories: HashMap<String, PageHistory>,
    pane_closings: u64,
    theme_resynced_at: HashMap<String, u64>,
}

static MEMORY: LazyLock<Mutex<PageMemory>> = LazyLock::new(Mutex::default);

fn memory() -> std::sync::MutexGuard<'static, PageMemory> {
    MEMORY.lock().unwrap_or_else(|p| p.into_inner())
}

fn history_entry(url: &str, viewer: bool, loading: Option<bool>) -> PageHistoryEntry {
    PageHistoryEntry { url: url.to_owned(), viewer, settle_by: (loading == Some(true)).then(|| Instant::now() + PAGE_HISTORY_SETTLE) }
}

fn entry_is_settling(entry: &PageHistoryEntry) -> bool {
    entry.settle_by.is_some_and(|by| Instant::now() < by)
}

impl PageHistory {
    fn push(&mut self, entry: PageHistoryEntry) {
        self.entries.truncate(self.index + 1);
        self.entries.push(entry);
        self.index = self.entries.len() - 1;
        if self.entries.len() > PAGE_HISTORY_LIMIT {
            self.entries.remove(0);
            self.index -= 1;
        }
    }

    fn flags_agree(&self, status: &PageHistoryStatus<'_>) -> bool {
        let (Some(back), Some(forward)) = (status.can_go_back, status.can_go_forward) else {
            return true;
        };
        (self.index > 0) == back && (self.index + 1 < self.entries.len()) == forward
    }
}

/// Records that the page is on this computer's network for the sake of a viewer.
pub fn note_viewer_network_cleared(session_id: &str) {
    memory().viewer_network_sessions.insert(session_id.to_owned());
}

/// Whether the page was cleared for a viewer, forgetting the mark: the caller rebinds it now.
pub fn take_viewer_network_cleared(session_id: &str) -> bool {
    memory().viewer_network_sessions.remove(session_id)
}

/// Forgets the mirror: the page's history was thrown away (a page taken down and reopened).
pub fn reset_page_history(session_id: &str) {
    memory().histories.remove(session_id);
}

/// Follows the page the host reports. A change of address nobody announced is a history move or a
/// navigation the page made itself, and a page still loading may yet redirect. The mirror trusts the
/// host over itself: one that disagrees with `canGoBack`/`canGoForward` starts over.
pub fn observe_page_history(session_id: &str, status: &PageHistoryStatus<'_>, is_viewer: &dyn Fn(&str) -> bool) {
    let mut memory = memory();
    let Some(history) = memory.histories.get_mut(session_id) else {
        memory.histories.insert(
            session_id.to_owned(),
            PageHistory { entries: vec![history_entry(status.url, is_viewer(status.url), status.loading)], index: 0 },
        );
        return;
    };
    let index = history.index;
    let before_matches = index > 0 && preview_addresses_match(&history.entries[index - 1].url, status.url);
    let after_matches = history.entries.get(index + 1).is_some_and(|after| preview_addresses_match(&after.url, status.url));
    let settling = entry_is_settling(&history.entries[index]);
    if preview_addresses_match(&history.entries[index].url, status.url) {
        let here = &mut history.entries[index];
        here.viewer = here.viewer || is_viewer(status.url);
        if status.loading != Some(true) {
            here.settle_by = None;
        }
    } else if settling && before_matches {
        // The page has not left the previous document yet; the status is only behind.
    } else if settling {
        let here = &mut history.entries[index];
        here.url = status.url.to_owned();
        here.viewer = here.viewer || is_viewer(status.url);
        if status.loading != Some(true) {
            here.settle_by = None;
        }
    } else if before_matches {
        history.index -= 1;
    } else if after_matches {
        history.index += 1;
    } else {
        history.push(history_entry(status.url, is_viewer(status.url), status.loading));
    }
    let current = &history.entries[history.index];
    if !(status.loading == Some(true) && entry_is_settling(current)) && !history.flags_agree(status) {
        let mut only = current.clone();
        only.settle_by = None;
        *history = PageHistory { entries: vec![only], index: 0 };
    }
}

/// Records a navigation the pane itself made: always a new entry, unlike an address seen on the poll.
pub fn note_page_navigation(session_id: &str, status: &PageHistoryStatus<'_>, viewer: bool) {
    let entry = history_entry(status.url, viewer, status.loading);
    let mut memory = memory();
    let Some(history) = memory.histories.get_mut(session_id) else {
        memory.histories.insert(session_id.to_owned(), PageHistory { entries: vec![entry], index: 0 });
        return;
    };
    let here = &mut history.entries[history.index];
    if preview_addresses_match(&here.url, status.url) {
        here.viewer = here.viewer || viewer;
        return;
    }
    history.push(entry);
}

/// The entry Back or Forward would land on, when the mirror is sure where the page stands.
pub fn peek_page_history(session_id: &str, back: bool, status: &PageHistoryStatus<'_>, is_viewer: &dyn Fn(&str) -> bool) -> Option<(String, bool)> {
    let memory = memory();
    let history = memory.histories.get(session_id)?;
    let here = history.entries.get(history.index)?;
    if status.loading == Some(true) && entry_is_settling(here) {
        return None;
    }
    if !preview_addresses_match(&here.url, status.url) || !history.flags_agree(status) {
        return None;
    }
    let index = if back { history.index.checked_sub(1)? } else { history.index + 1 };
    let entry = history.entries.get(index)?;
    Some((entry.url.clone(), entry.viewer || is_viewer(&entry.url)))
}

/// Whether the page showing `url` is a remote-control viewer, by the live rows or by the mirror.
pub fn page_shows_viewer(session_id: &str, url: &str, is_viewer: &dyn Fn(&str) -> bool) -> bool {
    if is_viewer(url) {
        return true;
    }
    let memory = memory();
    memory.histories.get(session_id).and_then(|history| history.entries.get(history.index)).is_some_and(|here| here.viewer && preview_addresses_match(&here.url, url))
}

/// Records that a preview pane closed: every page it showed takes the app's theme afresh next time.
pub fn note_preview_pane_closed() {
    memory().pane_closings += 1;
}

/// Whether the pane showing this page is the first to show it since a pane closed. Asking answers it.
pub fn take_preview_theme_resync(session_id: &str) -> bool {
    let mut memory = memory();
    let closings = memory.pane_closings;
    if memory.theme_resynced_at.get(session_id) == Some(&closings) {
        return false;
    }
    memory.theme_resynced_at.insert(session_id.to_owned(), closings);
    true
}

// ----- wording ----------------------------------------------------------------------------------------

pub fn remote_control_protocol_label(protocol: RemoteControlProtocol) -> &'static str {
    match protocol {
        RemoteControlProtocol::Adb => "Android",
        RemoteControlProtocol::Wda => "iOS",
        RemoteControlProtocol::Vnc => "VNC",
    }
}

pub fn remote_control_platform_of(protocol: RemoteControlProtocol) -> RemoteControlPlatform {
    match protocol {
        RemoteControlProtocol::Adb => RemoteControlPlatform::Android,
        RemoteControlProtocol::Wda => RemoteControlPlatform::Ios,
        RemoteControlProtocol::Vnc => RemoteControlPlatform::Desktop,
    }
}

pub fn remote_control_place_label(place: &RemoteControlPlace) -> String {
    match place {
        RemoteControlPlace::Machine { name } => name.clone(),
        RemoteControlPlace::ThisComputer => t("本机", "this computer").to_owned(),
        RemoteControlPlace::Workspace => t("工作区", "workspace").to_owned(),
    }
}

pub fn remote_control_state_label(state: RemoteControlState) -> Option<&'static str> {
    match state {
        RemoteControlState::Connecting => Some(t("正在连接", "Connecting")),
        RemoteControlState::Credentials => Some(t("等待登录", "Sign-in needed")),
        RemoteControlState::Reconnecting => Some(t("正在重连", "Reconnecting")),
        RemoteControlState::Failed => Some(t("连接失败", "Failed")),
        RemoteControlState::Ready => None,
    }
}

/// What a remote-control row shows beside its name, never with a `:0` port.
pub fn remote_control_detail(configuration: Option<&PreviewRemoteControlConfiguration>, remote: Option<&PreviewRemoteControl>) -> Option<String> {
    if let Some(configuration) = configuration {
        let label = remote_control_protocol_label(configuration.protocol);
        let place = remote_control_place_label(&configuration.place);
        return Some(match configuration.protocol {
            RemoteControlProtocol::Adb => format!("{label} · {}", configuration.device.clone().unwrap_or(place)),
            RemoteControlProtocol::Wda => format!("{label} · {place}"),
            RemoteControlProtocol::Vnc => format!("{label} · {place}:{}", configuration.port),
        });
    }
    let remote = remote?;
    let target = remote.target_display.as_deref().map(str::trim).filter(|text| !text.is_empty()).unwrap_or(&remote.target);
    Some(format!("{} · {target}", remote_control_protocol_label(remote.protocol)))
}

/// The whole-buffer replies the host sends instead of output: sentences, not log lines.
pub const PREVIEW_EMPTY_LOG_REPLIES: [&str; 3] = ["No logs yet.", "No server errors found.", "No dev server is running. preview_logs takes a process serverId from preview_list."];

/// Splits one `preview_server_logs` reply into drawer lines. Empty for every "nothing yet" reply.
pub fn preview_log_lines(rendered: &str) -> Vec<String> {
    let text = rendered.replace("\r\n", "\n");
    let reply = text.trim();
    let searched = reply.starts_with("No logs matching \"") && reply.ends_with("\".");
    if reply.is_empty() || PREVIEW_EMPTY_LOG_REPLIES.contains(&reply) || searched {
        return Vec::new();
    }
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewLogSeverity {
    Error,
    Warn,
}

/// Claude Code's own classifier: the first 200 characters, uppercased, decide the colour.
pub fn preview_log_severity(line: &str) -> Option<PreviewLogSeverity> {
    let head: String = line.chars().take(200).collect::<String>().to_uppercase();
    if head.contains("ERROR") || head.contains("ERR!") || head.contains("FATAL") || head.contains("FAIL") {
        return Some(PreviewLogSeverity::Error);
    }
    head.contains("WARN").then_some(PreviewLogSeverity::Warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(url: Option<&str>, remote: bool) -> PreviewServerSnapshot {
        PreviewServerSnapshot {
            handle: "h".into(),
            server_id: "dev".into(),
            name: "dev".into(),
            port: 5173,
            status: PreviewServerStatus::Running,
            started_at: String::new(),
            cwd: String::new(),
            session_id: None,
            machine: None,
            url: url.map(str::to_owned),
            workspace: None,
            remote: remote.then(|| PreviewRemoteControl {
                protocol: RemoteControlProtocol::Vnc,
                platform: RemoteControlPlatform::Desktop,
                target: "mac:5900".into(),
                target_display: None,
                label: None,
                state: RemoteControlState::Ready,
                read_only: false,
                launch: None,
                message: None,
            }),
        }
    }

    #[test]
    fn a_page_is_served_at_an_origin_not_a_string() {
        assert!(preview_url_is_served_at(Some("http://localhost:5173/a/b?c=1"), "http://localhost:5173"));
        assert!(!preview_url_is_served_at(Some("http://localhost:5174/"), "http://localhost:5173"));
        assert!(!preview_url_is_served_at(Some("about:blank"), "http://localhost:5173"));
        assert!(!preview_url_is_served_at(None, "http://localhost:5173"));
        assert!(!preview_url_is_served_at(Some("not a url"), "not a url"));
    }

    #[test]
    fn a_viewer_is_its_token_path_on_a_path_boundary() {
        let address = "http://127.0.0.1:5000/abc/";
        assert!(preview_url_is_served_at_path(Some("http://127.0.0.1:5000/abc/"), address));
        assert!(preview_url_is_served_at_path(Some("http://127.0.0.1:5000/abc/state"), address));
        assert!(preview_url_is_served_at_path(Some("http://127.0.0.1:5000/abc"), address));
        assert!(!preview_url_is_served_at_path(Some("http://127.0.0.1:5000/abcd/"), address));
        assert!(!preview_url_is_served_at_path(Some("http://127.0.0.1:5001/abc/"), address));
    }

    #[test]
    fn pairing_a_page_with_a_server() {
        assert!(preview_page_is_served_by(Some("http://localhost:5173/x"), &server(None, false)));
        assert!(!preview_page_is_served_by(Some("http://localhost:5173/x"), &server(None, true)));
        let viewer = server(Some("http://127.0.0.1:5000/abc/"), true);
        assert!(preview_page_is_served_by(Some("http://127.0.0.1:5000/abc/"), &viewer));
        assert!(!preview_page_is_served_by(Some("http://127.0.0.1:5000/other/"), &viewer));
        let servers = [server(None, false), viewer.clone()];
        assert_eq!(preview_viewer_server_of_page(Some("http://127.0.0.1:5000/abc/x"), &servers), Some(&viewer));
        assert_eq!(preview_viewer_server_of_page(Some("http://localhost:5173/"), &servers), None);
        // The same address on two machines is two servers; a viewer has no machine.
        assert!(page_is_served_by_server(Some("http://localhost:5173/"), "ssh:a", &server(None, false), "ssh:a"));
        assert!(!page_is_served_by_server(Some("http://localhost:5173/"), "ssh:a", &server(None, false), "local"));
        assert!(page_is_served_by_server(Some("http://127.0.0.1:5000/abc/"), "ssh:a", &viewer, "local"));
    }

    #[test]
    fn the_viewer_mark_is_taken_once() {
        note_viewer_network_cleared("mark-1");
        assert!(take_viewer_network_cleared("mark-1"));
        assert!(!take_viewer_network_cleared("mark-1"));
    }

    fn status(url: &str, loading: bool, back: bool, forward: bool) -> PageHistoryStatus<'_> {
        PageHistoryStatus { url, loading: Some(loading), can_go_back: Some(back), can_go_forward: Some(forward) }
    }

    #[test]
    fn the_mirror_follows_a_navigation_and_a_history_move() {
        let viewer = |url: &str| url.contains("viewer");
        let session = "mirror-1";
        reset_page_history(session);
        observe_page_history(session, &status("http://a/", false, false, false), &viewer);
        note_page_navigation(session, &status("http://viewer/", false, true, false), true);
        assert_eq!(peek_page_history(session, true, &status("http://viewer/", false, true, false), &viewer), Some(("http://a/".to_owned(), false)));
        assert_eq!(peek_page_history(session, false, &status("http://viewer/", false, true, false), &viewer), None);
        // Back, seen on the poll.
        observe_page_history(session, &status("http://a/", false, false, true), &viewer);
        assert_eq!(peek_page_history(session, false, &status("http://a/", false, false, true), &viewer), Some(("http://viewer/".to_owned(), true)));
        assert!(page_shows_viewer(session, "http://viewer/x", &viewer));
    }

    #[test]
    fn the_mirror_trusts_the_host_over_itself() {
        let viewer = |_: &str| false;
        let session = "mirror-2";
        reset_page_history(session);
        observe_page_history(session, &status("http://a/", false, false, false), &viewer);
        note_page_navigation(session, &status("http://b/", false, true, false), false);
        // The host says there is no back: the mirror starts over from the page on screen.
        observe_page_history(session, &status("http://b/", false, false, false), &viewer);
        assert_eq!(peek_page_history(session, true, &status("http://b/", false, true, false), &viewer), None);
    }

    #[test]
    fn a_loading_document_may_still_redirect() {
        let viewer = |_: &str| false;
        let session = "mirror-3";
        reset_page_history(session);
        observe_page_history(session, &status("http://a/", false, false, false), &viewer);
        note_page_navigation(session, &status("http://b/", true, true, false), false);
        // Still loading: can't say, and the redirect replaces the entry rather than adding one.
        assert_eq!(peek_page_history(session, true, &status("http://b/", true, true, false), &viewer), None);
        observe_page_history(session, &status("http://c/", false, true, false), &viewer);
        assert_eq!(peek_page_history(session, true, &status("http://c/", false, true, false), &viewer), Some(("http://a/".to_owned(), false)));
    }

    #[test]
    fn a_theme_resync_is_answered_once_per_closing() {
        let session = "theme-1";
        assert!(take_preview_theme_resync(session));
        assert!(!take_preview_theme_resync(session));
        note_preview_pane_closed();
        assert!(take_preview_theme_resync(session));
        assert!(!take_preview_theme_resync(session));
    }

    #[test]
    fn details_never_print_a_zero_port() {
        let configuration = |protocol, device: Option<&str>| PreviewRemoteControlConfiguration {
            protocol,
            place: RemoteControlPlace::Machine { name: "mac".into() },
            port: 5900,
            device: device.map(str::to_owned),
            username: None,
            launch: None,
            read_only: false,
        };
        assert_eq!(remote_control_detail(Some(&configuration(RemoteControlProtocol::Vnc, None)), None).as_deref(), Some("VNC · mac:5900"));
        assert_eq!(remote_control_detail(Some(&configuration(RemoteControlProtocol::Adb, Some("emulator-5554"))), None).as_deref(), Some("Android · emulator-5554"));
        assert_eq!(remote_control_detail(Some(&configuration(RemoteControlProtocol::Wda, None)), None).as_deref(), Some("iOS · mac"));
        let snapshot = server(None, true).remote.unwrap();
        assert_eq!(remote_control_detail(None, Some(&snapshot)).as_deref(), Some("VNC · mac:5900"));
        assert_eq!(remote_control_detail(None, None), None);
    }

    #[test]
    fn log_replies_are_not_log_lines() {
        for reply in PREVIEW_EMPTY_LOG_REPLIES {
            assert!(preview_log_lines(reply).is_empty());
        }
        assert!(preview_log_lines("No logs matching \"x\".").is_empty());
        assert!(preview_log_lines("  \n").is_empty());
        assert_eq!(preview_log_lines("one\r\ntwo\n\n"), ["one", "two"]);
    }

    #[test]
    fn the_host_still_writes_the_replies_this_recognises() {
        let logs = include_str!("../../../mewrk-host/host/src/preview_servers.rs");
        assert!(logs.contains("No logs yet.") && logs.contains("No server errors found."));
        assert!(logs.contains(&format!("MAX_LOG_LINES: u32 = {PREVIEW_MAX_LOG_LINES}")));
        assert!(include_str!("../../../mewrk-host/host/src/preview.rs").contains(PREVIEW_EMPTY_LOG_REPLIES[2]));
    }

    #[test]
    fn severity_reads_the_head_of_the_line() {
        assert_eq!(preview_log_severity("npm ERR! nope"), Some(PreviewLogSeverity::Error));
        assert_eq!(preview_log_severity("Build failed"), Some(PreviewLogSeverity::Error));
        assert_eq!(preview_log_severity("warning: careful"), Some(PreviewLogSeverity::Warn));
        assert_eq!(preview_log_severity("ready in 300ms"), None);
        assert_eq!(preview_log_severity(&format!("{}error", "x".repeat(200))), None);
    }

    #[test]
    fn a_start_outcome_is_told_apart_by_its_member() {
        let attached: PreviewStartOutcome = serde_json::from_value(json!({"attached": {"serverId": "a", "name": "a", "port": 0, "url": "http://x/"}})).unwrap();
        assert!(matches!(attached, PreviewStartOutcome::Attached { .. }));
        let started: PreviewStartOutcome = serde_json::from_value(json!({"server": {"handle": "h", "serverId": "a", "name": "a", "port": 1, "status": "running"}, "reused": false})).unwrap();
        assert!(matches!(started, PreviewStartOutcome::Started { .. }));
    }
}
