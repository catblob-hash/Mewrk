//! The server picker's logic: the rows, the body state, and `usePreviewServers`, from upstream's
//! `PreviewPane.tsx`, as a state machine with no host in it.

use std::collections::HashSet;

use mewrk_client::{t, tf};

use crate::preview::*;

/// One row of the server picker: a configuration, joined to the process answering it if any.
#[derive(Clone, Debug, PartialEq)]
pub struct PreviewServerRow {
    pub name: String,
    pub port: u32,
    pub url: Option<String>,
    pub server: Option<PreviewServerSnapshot>,
    pub running: bool,
    pub starting: bool,
    /// A `url` with no command: nothing to run or stop, because the server is somebody else's.
    pub attach: bool,
    /// A phone, a desktop, a VM: its `url` is the viewer, it has no port, and the page opens as soon as
    /// there is an address.
    pub remote: bool,
    /// A remote session that gave up, kept until it is stopped or restarted.
    pub failed: bool,
    pub remote_control: Option<PreviewRemoteControlConfiguration>,
}

fn is_failed_remote_control(server: &PreviewServerSnapshot) -> bool {
    server.status == PreviewServerStatus::Failed && server.remote.is_some()
}

/// Joins `.mewrk/launch.json` to the running processes, configurations first and in file order, then
/// any running server the file no longer describes.
pub fn preview_server_rows(configurations: &[PreviewConfiguredServer], servers: &[PreviewServerSnapshot]) -> Vec<PreviewServerRow> {
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut rows = Vec::with_capacity(configurations.len());
    for configuration in configurations {
        let unclaimed = || servers.iter().filter(|server| server.name == configuration.name && !claimed.contains(server.handle.as_str()));
        let server = unclaimed()
            .find(|server| matches!(server.status, PreviewServerStatus::Running | PreviewServerStatus::Starting))
            // A dev server that failed is gone from the registry; a remote session that failed isn't.
            .or_else(|| unclaimed().find(|server| is_failed_remote_control(server)));
        if let Some(server) = server {
            claimed.insert(&server.handle);
        }
        let remote = configuration.remote_control.is_some() || server.is_some_and(|server| server.remote.is_some());
        let status = server.map(|server| server.status);
        rows.push(PreviewServerRow {
            name: configuration.name.clone(),
            port: if remote { 0 } else { server.map_or(configuration.port, |server| server.port) },
            url: server.and_then(|server| server.url.clone()).or_else(|| configuration.url.clone()),
            server: server.cloned(),
            running: status == Some(PreviewServerStatus::Running),
            starting: status == Some(PreviewServerStatus::Starting),
            attach: !remote && configuration.command.as_deref().is_none_or(str::is_empty) && configuration.url.as_deref().is_some_and(|url| !url.is_empty()),
            remote,
            failed: remote && status == Some(PreviewServerStatus::Failed),
            remote_control: configuration.remote_control.clone(),
        });
    }
    for server in servers {
        if claimed.contains(server.handle.as_str()) {
            continue;
        }
        if !matches!(server.status, PreviewServerStatus::Running | PreviewServerStatus::Starting) && !is_failed_remote_control(server) {
            continue;
        }
        let remote = server.remote.is_some();
        rows.push(PreviewServerRow {
            name: server.name.clone(),
            port: if remote { 0 } else { server.port },
            url: server.url.clone(),
            server: Some(server.clone()),
            running: server.status == PreviewServerStatus::Running,
            starting: server.status == PreviewServerStatus::Starting,
            attach: false,
            remote,
            failed: remote && server.status == PreviewServerStatus::Failed,
            remote_control: None,
        });
    }
    rows
}

/// Whether the pane can point its page at the row right now.
pub fn preview_row_openable(row: &PreviewServerRow) -> bool {
    if row.remote {
        return row.server.is_some() && row.url.is_some() && !row.failed;
    }
    row.running || row.attach
}

/// The platform a remote-control row's icon is drawn for; `None` for a dev server or attach row.
pub fn preview_row_platform(row: &PreviewServerRow) -> Option<RemoteControlPlatform> {
    if !row.remote {
        return None;
    }
    let remote = row.server.as_ref().and_then(|server| server.remote.as_ref());
    let protocol = row.remote_control.as_ref().map(|configuration| configuration.protocol).or(remote.map(|remote| remote.protocol));
    Some(remote.map(|remote| remote.platform).or(protocol.map(remote_control_platform_of)).unwrap_or(RemoteControlPlatform::Desktop))
}

/// `name:port` for a dev server; a remote-control target has no port.
pub fn preview_server_label(name: &str, port: u32) -> String {
    if port > 0 { format!("{name}:{port}") } else { name.to_owned() }
}

/// What a row shows beside its name.
pub fn preview_row_detail(row: &PreviewServerRow) -> String {
    let remote = row.server.as_ref().and_then(|server| server.remote.as_ref());
    if row.remote {
        let place = remote_control_detail(row.remote_control.as_ref(), remote).unwrap_or_default();
        let state = remote.map(|remote| remote.state).unwrap_or(if row.failed { RemoteControlState::Failed } else { RemoteControlState::Ready });
        return match (remote_control_state_label(state), place.is_empty()) {
            (Some(status), false) => format!("{place} · {status}"),
            (Some(status), true) => status.to_owned(),
            (None, _) => place,
        };
    }
    if !row.attach {
        return match row.server.as_ref().and_then(|server| server.machine.as_deref()) {
            Some(machine) => format!("{machine} :{}", row.port),
            None => format!(":{}", row.port),
        };
    }
    let address = preview_server_address(row.port, row.url.as_deref());
    match url::Url::parse(&address) {
        Ok(parsed) => match (parsed.host_str(), parsed.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => address,
        },
        Err(_) => address,
    }
}

/// Whether the page on screen came from this row's server.
pub fn preview_row_is_open(row: &PreviewServerRow, url: Option<&str>) -> bool {
    if row.remote {
        return row.url.as_deref().is_some_and(|address| preview_url_is_served_at_path(url, address));
    }
    preview_url_is_served_at(url, &preview_server_address(row.port, row.url.as_deref()))
}

/// Why a remote-control row failed, when the host said, and only for a row that did.
pub fn preview_row_failure(row: &PreviewServerRow) -> Option<String> {
    if !row.remote || !row.failed {
        return None;
    }
    row.server.as_ref()?.remote.as_ref()?.message.as_deref().map(str::trim).filter(|message| !message.is_empty()).map(str::to_owned)
}

/// The lower-cased names of the processes a listed remote-control row starts first: an emulator, a VM.
/// The port such a process is configured with is the device's, so the host's readiness probe holds it
/// `starting`, and it has no page to wait for, so it is no reason to cover the viewer.
pub fn preview_launch_sibling_names(rows: &[PreviewServerRow]) -> HashSet<String> {
    rows.iter()
        .filter(|row| row.remote)
        .filter_map(|row| row.remote_control.as_ref().and_then(|configuration| configuration.launch.clone()).or_else(|| row.server.as_ref()?.remote.as_ref()?.launch.clone()))
        .map(|launch| launch.to_lowercase())
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewStartError {
    pub name: String,
    pub message: String,
    pub remote: bool,
    /// The failed session's own id: entries that repeat a name share it, and this tells them apart.
    pub server_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewStopped {
    pub label: String,
    pub name: String,
    pub remote: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewBodyState {
    Page,
    Reading,
    NoConfig,
    StartPage,
    Starting,
    StartFailed { name: String, message: String, remote: bool },
    Stopped { label: String, remote: bool, reason: Option<String> },
}

/// Which of the source's body states the pane is in. A live page wins over the start page but not over
/// a failure or a start in flight: those are about what the user just did. `configuration_count` is
/// `None` until the file has been read, when "no dev server" would be a claim the list contradicts.
pub fn preview_body_state(url: &str, configuration_count: Option<usize>, rows: &[PreviewServerRow], pending_name: Option<&str>, start_error: Option<&PreviewStartError>, stopped: Option<&PreviewStopped>) -> PreviewBodyState {
    if let Some(error) = start_error {
        return PreviewBodyState::StartFailed { name: error.name.clone(), message: error.message.clone(), remote: error.remote };
    }
    let siblings = preview_launch_sibling_names(rows);
    if pending_name.is_some() || rows.iter().any(|row| row.starting && !row.remote && !siblings.contains(&row.name.to_lowercase())) {
        return PreviewBodyState::Starting;
    }
    if !url.is_empty() && url != "about:blank" {
        // A page that outlived its server was showing something else, so the card is what says it went.
        return match stopped {
            None => PreviewBodyState::Page,
            Some(stopped) => PreviewBodyState::Stopped { label: stopped.label.clone(), remote: stopped.remote, reason: stopped.reason.clone() },
        };
    }
    match configuration_count {
        None => PreviewBodyState::Reading,
        Some(0) => PreviewBodyState::NoConfig,
        Some(_) => PreviewBodyState::StartPage,
    }
}

/// The server whose page the pane is showing.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveServer {
    handle: String,
    /// What tells a restart of this entry from another entry that merely shares its name.
    server_id: String,
    name: String,
    port: u32,
    remote: bool,
    /// The reason the host last gave: its row is gone by the time the stopped card needs to say why.
    message: Option<String>,
}

fn trimmed_message(server: &PreviewServerSnapshot) -> Option<String> {
    server.remote.as_ref()?.message.as_deref().map(str::trim).filter(|message| !message.is_empty()).map(str::to_owned)
}

fn same_server_id(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn attach_row(attached: &PreviewAttachment) -> PreviewServerRow {
    PreviewServerRow { name: attached.name.clone(), port: attached.port, url: Some(attached.url.clone()), server: None, running: false, starting: false, attach: true, remote: false, failed: false, remote_control: None }
}

/// `.mewrk/launch.json`, the running-server list, and every start and stop the pane makes.
#[derive(Default)]
pub struct ServersCore {
    pub configurations: Option<PreviewConfigurationList>,
    pub servers: Vec<PreviewServerSnapshot>,
    /// Why the last read failed: for a workspace on another machine, usually the machine itself.
    pub unreachable: Option<String>,
    pub pending_name: Option<String>,
    pub start_error: Option<PreviewStartError>,
    pub stopped: Option<PreviewStopped>,
    active: Option<ActiveServer>,
    /// A remote server this pane started, whose page opens once the host says it answers.
    pending_open: Option<PreviewServerRow>,
    target_key: Option<String>,
}

impl ServersCore {
    /// Another workspace's answers are not this one's, not even for the tick it takes to replace them.
    pub fn set_target(&mut self, target: Option<&PreviewTarget>) {
        let key = target.map(preview_target_key);
        if key != self.target_key {
            *self = ServersCore { target_key: key, ..ServersCore::default() };
        }
    }

    pub fn rows(&self) -> Vec<PreviewServerRow> {
        let empty = Vec::new();
        preview_server_rows(self.configurations.as_ref().map_or(&empty, |list| &list.servers), &self.servers)
    }

    /// One poll's answers. Returns the row whose page should open now, if a start was waiting for it.
    pub fn poll_answered(&mut self, listed: PreviewConfigurationList, running: Vec<PreviewServerSnapshot>) -> Option<PreviewServerRow> {
        self.configurations = Some(listed);
        self.unreachable = None;
        // "Failed to connect" is about a session that is gone. When that entry is listed again and not
        // failed, something started it again, and the card would otherwise stay over the live viewer.
        if self.start_error.as_ref().is_some_and(|error| {
            error.remote
                && running.iter().any(|server| {
                    server.remote.is_some()
                        && server.status != PreviewServerStatus::Failed
                        && match &error.server_id {
                            Some(id) => same_server_id(&server.server_id, id),
                            None => server.name == error.name,
                        }
                })
        }) {
            self.start_error = None;
        }
        let mut ready = None;
        if let Some(pending) = self.pending_open.clone() {
            let handle = pending.server.as_ref().map(|server| server.handle.clone());
            let now = running.iter().find(|server| Some(&server.handle) == handle.as_ref());
            if now.is_none_or(|server| server.status == PreviewServerStatus::Running) {
                self.pending_open = None;
                // Gone before it answered: the stopped card says so, and there is nothing to open.
                if let Some(now) = now {
                    ready = Some(PreviewServerRow { server: Some(now.clone()), running: true, starting: false, ..pending });
                }
            }
        }
        if let Some(active) = self.active.clone() {
            match running.iter().find(|server| server.handle == active.handle) {
                Some(current) => {
                    // Whatever the host says now: a reason from while it was reconnecting is no reason
                    // for the session ending after it recovered.
                    self.active.as_mut().expect("just read").message = trimmed_message(current);
                }
                None => {
                    // A session started again replaces its row. That is the entry being restarted, not a
                    // screen that went away, so the pane follows it. By id: an entry repeating the name
                    // is another screen, and following it would hide this one's end.
                    let successor = active.remote.then(|| running.iter().find(|server| server.remote.is_some() && same_server_id(&server.server_id, &active.server_id))).flatten();
                    match successor {
                        Some(successor) => {
                            self.active = (successor.status != PreviewServerStatus::Failed).then(|| ActiveServer {
                                handle: successor.handle.clone(),
                                server_id: successor.server_id.clone(),
                                name: successor.name.clone(),
                                port: 0,
                                remote: true,
                                message: trimmed_message(successor),
                            });
                        }
                        None => {
                            self.active = None;
                            self.stopped = Some(PreviewStopped { label: preview_server_label(&active.name, active.port), name: active.name.clone(), remote: active.remote, reason: active.message.clone() });
                        }
                    }
                }
            }
        }
        self.servers = running;
        ready
    }

    /// A workspace that has gone away answers again on the next tick; the pane keeps what it has.
    pub fn poll_failed(&mut self, reason: String) {
        self.unreachable = Some(reason);
    }

    pub fn begin_run(&mut self, name: &str) {
        self.start_error = None;
        self.stopped = None;
        // Starting an entry replaces the row the pane was following for it, and the poll would read the
        // missing handle as that screen going away, over the viewer the start is about to open.
        if self.active.as_ref().is_some_and(|active| active.name == name) {
            self.active = None;
        }
        self.pending_name = Some(name.to_owned());
    }

    /// A start's answer. Returns the row whose page should open now.
    pub fn run_answered(&mut self, name: &str, outcome: Result<PreviewStartOutcome, String>) -> Option<PreviewServerRow> {
        self.pending_name = None;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(message) => {
                self.start_error = Some(PreviewStartError { name: name.to_owned(), message, remote: false, server_id: None });
                return None;
            }
        };
        let server = match outcome {
            // Nothing was started, so there is no process to claim.
            PreviewStartOutcome::Attached { attached } => return Some(attach_row(&attached)),
            PreviewStartOutcome::Started { server, .. } => server,
        };
        let remote = server.remote.is_some();
        let failed = remote && server.status == PreviewServerStatus::Failed;
        // A session that already failed opens no page, so there is no page to report the end of.
        if !failed {
            self.active = Some(ActiveServer { handle: server.handle.clone(), server_id: server.server_id.clone(), name: server.name.clone(), port: server.port, remote, message: trimmed_message(&server) });
        }
        self.servers.retain(|known| known.handle != server.handle);
        self.servers.push(server.clone());
        let configured = self.configurations.as_ref().and_then(|list| list.servers.iter().find(|entry| entry.name == server.name));
        let row = PreviewServerRow {
            name: server.name.clone(),
            port: if remote { 0 } else { server.port },
            // A remote row's address is the viewer the host just generated, never anything the file could spell.
            url: if remote { server.url.clone() } else { configured.and_then(|entry| entry.url.clone()) },
            server: Some(server.clone()),
            running: server.status == PreviewServerStatus::Running,
            starting: server.status == PreviewServerStatus::Starting,
            attach: false,
            remote,
            failed,
            remote_control: configured.and_then(|entry| entry.remote_control.clone()),
        };
        if remote {
            // The viewer is up before the device is, so the page opens now instead of waiting for
            // `running`, which `credentials` would make never come.
            if failed {
                self.start_error = Some(PreviewStartError { name: name.to_owned(), message: trimmed_message(&server).unwrap_or_default(), remote: true, server_id: Some(server.server_id.clone()) });
                return None;
            }
            if row.url.is_some() {
                // The poll may have reported the screen this start replaced as gone meanwhile.
                self.stopped = None;
                return Some(row);
            }
            return None;
        }
        if server.machine.is_some() && server.status == PreviewServerStatus::Starting {
            // A page opened now would land on a refused connection a round trip before it could have
            // answered. The starting card holds until the host says the server answers.
            self.pending_open = Some(row);
            return None;
        }
        Some(row)
    }

    /// The handle to stop for this row. The pane forgets it at once.
    pub fn stop(&mut self, row: &PreviewServerRow) -> Option<String> {
        let handle = row.server.as_ref()?.handle.clone();
        if self.active.as_ref().is_some_and(|active| active.handle == handle) {
            self.active = None;
        }
        self.servers.retain(|server| server.handle != handle);
        Some(handle)
    }

    pub fn stop_all(&mut self) -> Vec<String> {
        let live = self.rows().into_iter().filter_map(|row| row.server.map(|server| server.handle)).collect();
        self.active = None;
        self.servers.clear();
        live
    }

    /// Names the server whose page the pane is showing. Opening a server someone else started claims it
    /// exactly the way starting one does, and is the answer to an earlier start that failed.
    pub fn adopt(&mut self, row: &PreviewServerRow) {
        let Some(server) = &row.server else {
            return;
        };
        self.active = Some(ActiveServer { handle: server.handle.clone(), server_id: server.server_id.clone(), name: server.name.clone(), port: server.port, remote: row.remote, message: trimmed_message(server) });
        self.stopped = None;
        self.start_error = None;
    }

    pub fn dismiss_stopped(&mut self) {
        self.stopped = None;
    }

    /// The page this pane was following a server for has been closed, so there is nothing left to report on it.
    pub fn forget_page(&mut self) {
        self.active = None;
        self.pending_open = None;
        self.stopped = None;
        self.start_error = None;
    }

    /// The server the log drawer follows. Empty in the two states where its output matters most would
    /// be useless: while a start is in flight, and right after one has failed.
    pub fn log_row(&self) -> Option<PreviewServerRow> {
        let rows = self.rows();
        rows.iter().find(|row| row.running && row.server.is_some()).or_else(|| rows.iter().find(|row| row.starting && row.server.is_some())).or_else(|| rows.iter().find(|row| row.failed && row.server.is_some())).cloned()
    }
}

/// The empty log drawer's one line. "Waiting for output" asserts a server that is printing nothing.
pub fn log_empty_text(server_name: Option<&str>) -> String {
    match server_name {
        Some(name) => tf("等待 {name} 的输出…", "Waiting for output from {name}…", &[("name", &name)]),
        None => t("没有正在运行的开发服务器", "No dev server is running").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(name: &str, port: u32) -> PreviewConfiguredServer {
        PreviewConfiguredServer { name: name.into(), command: Some("npm".into()), port, url: None, remote_control: None }
    }

    fn attach_config(name: &str, url: &str) -> PreviewConfiguredServer {
        PreviewConfiguredServer { name: name.into(), command: None, port: 0, url: Some(url.into()), remote_control: None }
    }

    fn remote_config(name: &str, launch: Option<&str>) -> PreviewConfiguredServer {
        PreviewConfiguredServer {
            name: name.into(),
            command: None,
            port: 0,
            url: None,
            remote_control: Some(PreviewRemoteControlConfiguration { protocol: RemoteControlProtocol::Vnc, place: RemoteControlPlace::ThisComputer, port: 5900, device: None, username: None, launch: launch.map(str::to_owned), read_only: false }),
        }
    }

    fn snapshot(handle: &str, name: &str, status: PreviewServerStatus) -> PreviewServerSnapshot {
        PreviewServerSnapshot { handle: handle.into(), server_id: name.into(), name: name.into(), port: 5173, status, started_at: String::new(), cwd: String::new(), session_id: None, machine: None, url: None, workspace: None, remote: None }
    }

    fn remote_snapshot(handle: &str, server_id: &str, name: &str, status: PreviewServerStatus, url: Option<&str>, message: Option<&str>) -> PreviewServerSnapshot {
        PreviewServerSnapshot {
            url: url.map(str::to_owned),
            port: 0,
            server_id: server_id.into(),
            remote: Some(PreviewRemoteControl {
                protocol: RemoteControlProtocol::Vnc,
                platform: RemoteControlPlatform::Desktop,
                target: "mac:5900".into(),
                target_display: None,
                label: None,
                state: match status {
                    PreviewServerStatus::Failed => RemoteControlState::Failed,
                    PreviewServerStatus::Starting => RemoteControlState::Connecting,
                    _ => RemoteControlState::Ready,
                },
                read_only: false,
                launch: None,
                message: message.map(str::to_owned),
            }),
            ..snapshot(handle, name, status)
        }
    }

    fn list(servers: Vec<PreviewConfiguredServer>) -> PreviewConfigurationList {
        PreviewConfigurationList { servers, ..Default::default() }
    }

    fn started(server: PreviewServerSnapshot) -> Result<PreviewStartOutcome, String> {
        Ok(PreviewStartOutcome::Started { server, reused: false })
    }

    #[test]
    fn joins_launch_json_to_the_processes_answering_it() {
        let rows = preview_server_rows(&[config("web", 3000), attach_config("docs", "https://docs.example.com/x"), config("api", 4000)], &[snapshot("h1", "web", PreviewServerStatus::Running), snapshot("h2", "orphan", PreviewServerStatus::Running), snapshot("h3", "gone", PreviewServerStatus::Stopped)]);
        assert_eq!(rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), ["web", "docs", "api", "orphan"]);
        assert!(rows[0].running && rows[0].port == 5173);
        assert!(rows[1].attach && !rows[1].running);
        assert!(!rows[2].running && rows[2].port == 4000);
        assert_eq!(preview_row_detail(&rows[0]), ":5173");
        assert_eq!(preview_row_detail(&rows[1]), "docs.example.com");
    }

    #[test]
    fn a_repeated_name_takes_each_process_once() {
        let rows = preview_server_rows(&[config("dev", 1), config("dev", 2)], &[snapshot("a", "dev", PreviewServerStatus::Running), snapshot("b", "dev", PreviewServerStatus::Starting)]);
        assert_eq!(rows[0].server.as_ref().unwrap().handle, "a");
        assert_eq!(rows[1].server.as_ref().unwrap().handle, "b");
    }

    #[test]
    fn a_failed_session_keeps_its_row_and_a_dead_server_does_not() {
        let rows = preview_server_rows(&[remote_config("mac", None)], &[remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, Some("http://127.0.0.1:1/t/"), Some("  refused  "))]);
        assert!(rows[0].failed && rows[0].remote && rows[0].port == 0);
        assert!(!preview_row_openable(&rows[0]));
        assert_eq!(preview_row_failure(&rows[0]).as_deref(), Some("refused"));
        assert_eq!(preview_row_detail(&rows[0]), format!("VNC · {}:5900 · {}", t("本机", "this computer"), t("连接失败", "Failed")));
        assert!(preview_server_rows(&[], &[snapshot("h", "dead", PreviewServerStatus::Failed)]).is_empty());
    }

    #[test]
    fn a_row_opens_when_there_is_something_to_open() {
        let rows = preview_server_rows(&[config("web", 3000), attach_config("docs", "http://x/")], &[]);
        assert!(!preview_row_openable(&rows[0]));
        assert!(preview_row_openable(&rows[1]));
        let starting = preview_server_rows(&[remote_config("mac", None)], &[remote_snapshot("h", "mac", "mac", PreviewServerStatus::Starting, Some("http://127.0.0.1:1/t/"), None)]);
        assert!(preview_row_openable(&starting[0]), "a viewer opens while it connects");
    }

    #[test]
    fn a_row_is_open_by_origin_or_by_token_path() {
        let dev = preview_server_rows(&[config("web", 3000)], &[])[0].clone();
        assert!(preview_row_is_open(&dev, Some("http://localhost:3000/a")));
        let viewer = preview_server_rows(&[remote_config("mac", None)], &[remote_snapshot("h", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:1/t/"), None)])[0].clone();
        assert!(preview_row_is_open(&viewer, Some("http://127.0.0.1:1/t/x")));
        assert!(!preview_row_is_open(&viewer, Some("http://127.0.0.1:1/other/")));
    }

    #[test]
    fn orders_the_body_states_so_an_actions_report_is_never_hidden() {
        let error = PreviewStartError { name: "web".into(), message: "boom".into(), remote: false, server_id: None };
        let stopped = PreviewStopped { label: "web:1".into(), name: "web".into(), remote: false, reason: None };
        let state = |url: &str, count: Option<usize>, pending: Option<&str>, e: Option<&PreviewStartError>, s: Option<&PreviewStopped>| preview_body_state(url, count, &[], pending, e, s);
        assert!(matches!(state("http://x/", Some(1), None, Some(&error), None), PreviewBodyState::StartFailed { .. }));
        assert_eq!(state("http://x/", Some(1), Some("web"), None, None), PreviewBodyState::Starting);
        assert_eq!(state("http://x/", Some(1), None, None, None), PreviewBodyState::Page);
        assert!(matches!(state("http://x/", Some(1), None, None, Some(&stopped)), PreviewBodyState::Stopped { .. }));
        // No page under it: the start page is what belongs there, not a card about what was closed.
        assert_eq!(state("about:blank", Some(1), None, None, Some(&stopped)), PreviewBodyState::StartPage);
        assert_eq!(state("about:blank", None, None, None, None), PreviewBodyState::Reading);
        assert_eq!(state("about:blank", Some(0), None, None, None), PreviewBodyState::NoConfig);
    }

    #[test]
    fn keeps_the_viewer_on_screen_while_what_its_entry_starts_first_is_still_starting() {
        let rows = preview_server_rows(&[remote_config("mac", Some("VM-Proc")), config("vm-proc", 5900)], &[remote_snapshot("h", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:1/t/"), None), snapshot("v", "vm-proc", PreviewServerStatus::Starting)]);
        assert_eq!(preview_body_state("http://127.0.0.1:1/t/", Some(2), &rows, None, None, None), PreviewBodyState::Page);
        let plain = preview_server_rows(&[config("web", 1)], &[snapshot("w", "web", PreviewServerStatus::Starting)]);
        assert_eq!(preview_body_state("http://x/", Some(1), &plain, None, None, None), PreviewBodyState::Starting);
    }

    #[test]
    fn opens_a_server_on_another_machine_only_once_the_host_says_it_answers() {
        let mut core = ServersCore::default();
        core.begin_run("web");
        let mut remote = snapshot("h", "web", PreviewServerStatus::Starting);
        remote.machine = Some("devbox".into());
        assert_eq!(core.run_answered("web", started(remote.clone())), None, "held back while it starts");
        assert_eq!(core.poll_answered(list(vec![config("web", 1)]), vec![remote.clone()]), None);
        remote.status = PreviewServerStatus::Running;
        let ready = core.poll_answered(list(vec![config("web", 1)]), vec![remote]).expect("opens now");
        assert!(ready.running && !ready.starting);
    }

    #[test]
    fn a_server_gone_before_it_answered_opens_nothing() {
        let mut core = ServersCore::default();
        let mut remote = snapshot("h", "web", PreviewServerStatus::Starting);
        remote.machine = Some("devbox".into());
        core.begin_run("web");
        core.run_answered("web", started(remote));
        assert_eq!(core.poll_answered(list(vec![]), vec![]), None);
        assert!(core.stopped.is_some());
    }

    #[test]
    fn a_local_server_opens_as_soon_as_the_start_returns() {
        let mut core = ServersCore::default();
        core.begin_run("web");
        let row = core.run_answered("web", started(snapshot("h", "web", PreviewServerStatus::Running))).expect("a row");
        assert!(row.running);
        assert_eq!(core.pending_name, None);
    }

    #[test]
    fn an_attach_opens_and_claims_nothing() {
        let mut core = ServersCore::default();
        core.begin_run("docs");
        let row = core.run_answered("docs", Ok(PreviewStartOutcome::Attached { attached: PreviewAttachment { server_id: "docs".into(), name: "docs".into(), port: 0, url: "http://x/".into() } })).expect("a row");
        assert!(row.attach);
        assert_eq!(core.poll_answered(list(vec![]), vec![]), None);
        assert_eq!(core.stopped, None, "no process was ever there to stop");
    }

    #[test]
    fn reports_a_failed_start_with_the_hosts_diagnosis() {
        let mut core = ServersCore::default();
        core.begin_run("web");
        assert_eq!(core.run_answered("web", Err("spawn failed: npm".into())), None);
        assert_eq!(core.start_error.as_ref().unwrap().message, "spawn failed: npm");
        assert!(!core.start_error.as_ref().unwrap().remote);
        core.begin_run("web");
        assert_eq!(core.start_error, None, "a retry takes the card down");
    }

    #[test]
    fn offers_a_restart_when_the_server_behind_the_page_disappears() {
        let mut core = ServersCore::default();
        core.begin_run("web");
        core.run_answered("web", started(snapshot("h", "web", PreviewServerStatus::Running)));
        core.poll_answered(list(vec![config("web", 1)]), vec![snapshot("h", "web", PreviewServerStatus::Running)]);
        assert_eq!(core.stopped, None);
        core.poll_answered(list(vec![config("web", 1)]), vec![]);
        let stopped = core.stopped.clone().expect("the stopped card");
        assert_eq!(stopped.label, "web:5173");
        assert!(!stopped.remote);
        core.dismiss_stopped();
        assert_eq!(core.stopped, None);
    }

    #[test]
    fn stopping_it_yourself_is_not_a_disappearance() {
        let mut core = ServersCore::default();
        core.begin_run("web");
        let row = core.run_answered("web", started(snapshot("h", "web", PreviewServerStatus::Running))).unwrap();
        assert_eq!(core.stop(&row).as_deref(), Some("h"));
        core.poll_answered(list(vec![config("web", 1)]), vec![]);
        assert_eq!(core.stopped, None);
    }

    #[test]
    fn stops_every_server_from_the_groups_own_action() {
        let mut core = ServersCore::default();
        core.poll_answered(list(vec![config("a", 1), config("b", 2)]), vec![snapshot("h1", "a", PreviewServerStatus::Running), snapshot("h2", "b", PreviewServerStatus::Starting)]);
        assert_eq!(core.stop_all(), ["h1", "h2"]);
        assert!(core.servers.is_empty());
    }

    #[test]
    fn a_viewer_opens_at_once_at_the_address_the_host_returned() {
        let mut core = ServersCore::default();
        core.poll_answered(list(vec![remote_config("mac", None)]), vec![]);
        core.begin_run("mac");
        let row = core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Starting, Some("http://127.0.0.1:9/token/"), None))).expect("the viewer opens");
        assert_eq!(row.url.as_deref(), Some("http://127.0.0.1:9/token/"));
        assert_eq!(row.port, 0);
        assert!(row.remote_control.is_some());
    }

    #[test]
    fn a_start_that_came_back_failed_opens_no_viewer() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        assert_eq!(core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some(" refused ")))), None);
        let error = core.start_error.clone().unwrap();
        assert_eq!((error.message.as_str(), error.remote, error.server_id.as_deref()), ("refused", true, Some("mac")));
        assert_eq!(core.poll_answered(list(vec![]), vec![]), None);
        assert_eq!(core.stopped, None, "no page to report the end of");
    }

    #[test]
    fn the_failure_card_goes_when_the_entry_is_started_again_behind_it() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some("no"))));
        core.poll_answered(list(vec![]), vec![remote_snapshot("h2", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/t/"), None)]);
        assert_eq!(core.start_error, None);
    }

    #[test]
    fn the_failure_card_stays_when_only_an_entry_sharing_the_name_is_up() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some("no"))));
        core.poll_answered(list(vec![]), vec![remote_snapshot("h2", "mac-2", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/t/"), None)]);
        assert!(core.start_error.is_some());
    }

    #[test]
    fn the_failure_card_goes_when_another_page_is_opened_over_it() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some("no"))));
        let other = preview_server_rows(&[config("web", 1)], &[snapshot("w", "web", PreviewServerStatus::Running)])[0].clone();
        core.adopt(&other);
        assert_eq!(core.start_error, None);
    }

    #[test]
    fn the_replaced_row_is_not_reported_as_a_disconnect_over_the_restart() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h1", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/a/"), None)));
        // Restarted: the running row is deleted before it connects again.
        core.begin_run("mac");
        assert_eq!(core.poll_answered(list(vec![]), vec![]), None);
        assert_eq!(core.stopped, None);
    }

    #[test]
    fn follows_the_row_that_replaces_it_and_reports_that_ones_end_with_its_reason() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h1", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/a/"), None)));
        // The model's `preview_start` replaced the row.
        core.poll_answered(list(vec![]), vec![remote_snapshot("h2", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/b/"), Some("link lost"))]);
        assert_eq!(core.stopped, None);
        core.poll_answered(list(vec![]), vec![]);
        let stopped = core.stopped.clone().expect("the end of the row it followed");
        assert_eq!(stopped.reason.as_deref(), Some("link lost"));
        assert_eq!(stopped.label, "mac", "no port");
        assert!(stopped.remote);
    }

    #[test]
    fn reports_the_stop_instead_of_following_another_entrys_session() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h1", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/a/"), None)));
        core.poll_answered(list(vec![]), vec![remote_snapshot("h2", "mac-2", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/b/"), None)]);
        assert!(core.stopped.is_some());
    }

    #[test]
    fn a_reason_from_before_a_recovery_is_no_reason_for_the_end_after_it() {
        let mut core = ServersCore::default();
        core.begin_run("mac");
        core.run_answered("mac", started(remote_snapshot("h", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/a/"), Some("reconnecting"))));
        core.poll_answered(list(vec![]), vec![remote_snapshot("h", "mac", "mac", PreviewServerStatus::Running, Some("http://127.0.0.1:9/a/"), None)]);
        core.poll_answered(list(vec![]), vec![]);
        assert_eq!(core.stopped.unwrap().reason, None);
    }

    #[test]
    fn a_workspaces_answers_are_not_anothers() {
        let target = |id: &str| PreviewTarget { conversation_id: id.into(), ..Default::default() };
        let mut core = ServersCore::default();
        core.set_target(Some(&target("a")));
        core.poll_answered(list(vec![config("web", 1)]), vec![snapshot("h", "web", PreviewServerStatus::Running)]);
        core.set_target(Some(&target("a")));
        assert!(core.configurations.is_some(), "the same workspace keeps them");
        core.set_target(Some(&target("b")));
        assert!(core.configurations.is_none() && core.servers.is_empty());
    }

    #[test]
    fn an_unreachable_machine_keeps_what_it_had_and_says_why() {
        let mut core = ServersCore::default();
        core.poll_answered(list(vec![config("web", 1)]), vec![]);
        core.poll_failed("devbox is away".into());
        assert_eq!(core.unreachable.as_deref(), Some("devbox is away"));
        assert!(core.configurations.is_some());
        core.poll_answered(list(vec![config("web", 1)]), vec![]);
        assert_eq!(core.unreachable, None);
    }

    #[test]
    fn the_log_drawer_follows_a_failed_session() {
        let mut core = ServersCore::default();
        core.poll_answered(list(vec![remote_config("mac", None)]), vec![remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some("no"))]);
        assert_eq!(core.log_row().map(|row| row.name), Some("mac".to_owned()));
        core.poll_answered(list(vec![remote_config("mac", None), config("web", 1)]), vec![remote_snapshot("h", "mac", "mac", PreviewServerStatus::Failed, None, Some("no")), snapshot("w", "web", PreviewServerStatus::Running)]);
        assert_eq!(core.log_row().map(|row| row.name), Some("web".to_owned()), "a running server comes first");
    }

    #[test]
    fn the_snapshot_reads_what_the_host_writes() {
        let server: PreviewServerSnapshot = serde_json::from_value(json!({"handle": "h", "serverId": "dev", "name": "dev", "port": 5173, "status": "running", "startedAt": "x", "cwd": "/w", "machine": "devbox", "url": "http://127.0.0.1:1/", "remote": {"protocol": "vnc", "platform": "desktop", "target": "t", "state": "ready", "readOnly": true}})).unwrap();
        assert_eq!(server.machine.as_deref(), Some("devbox"));
        assert!(server.remote.unwrap().read_only);
    }
}
