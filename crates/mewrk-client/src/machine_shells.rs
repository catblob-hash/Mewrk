//! Shell backends per machine (`lib/machineShells.ts`).
//!
//! The host's `ShellBackend` and `MachineShells` aren't reachable from this crate, so they are mirrored here with the
//! same serde names; a value crosses into `model` types through JSON.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{AttachedWorkspace, ExecutionEnvironmentAssets, RunTarget};
use crate::workspaces::{run_env_key, TerminalShell};
use crate::App;

/// A shell Mewrk can run a command in. Mirrors the host's `ShellBackend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellBackend {
    Bash,
    Zsh,
    Sh,
    Pwsh,
    Powershell,
}

/// A machine's operating system. Mirrors the host's `MachineOs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineOs {
    Windows,
    Macos,
    Linux,
    Wsl,
}

/// One shell a probe found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectedShell {
    pub backend: ShellBackend,
    pub path: String,
}

/// What a probe learned about one machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineShells {
    pub os: MachineOs,
    pub shells: Vec<DetectedShell>,
    pub probed_at: String,
}

/// Every machine's last probe, by environment key.
pub type ShellProbes = BTreeMap<String, MachineShells>;

/// Shell backends per operating system, most preferred first. Mirrors `shell_backend::backends_for`: a combination is
/// listed only when Mewrk can run the shell tool and its own file-tool and language-server scripts through it, so a
/// shell absent here is never probed on that OS. The order is Mewrk's own and the only one.
pub fn shell_backends_by_os(os: MachineOs) -> &'static [ShellBackend] {
    use ShellBackend::*;
    match os {
        MachineOs::Windows => &[Pwsh, Powershell, Bash],
        MachineOs::Macos => &[Zsh, Bash, Sh],
        MachineOs::Linux | MachineOs::Wsl => &[Bash, Zsh, Sh],
    }
}

/// Every backend, in the order its tools are listed. Mirrors `ShellBackend::ALL`.
pub const SHELL_BACKENDS: [ShellBackend; 5] = [ShellBackend::Bash, ShellBackend::Zsh, ShellBackend::Sh, ShellBackend::Pwsh, ShellBackend::Powershell];

impl ShellBackend {
    /// The tool's name, which is also the wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Pwsh => "pwsh",
            Self::Powershell => "powershell",
        }
    }
}

/// `isShellBackend`: the backend a string names exactly, if any.
pub fn parse_shell_backend(value: &str) -> Option<ShellBackend> {
    SHELL_BACKENDS.into_iter().find(|backend| backend.as_str() == value)
}

/// The name a backend goes by in settings.
pub fn shell_backend_label(backend: ShellBackend) -> &'static str {
    match backend {
        ShellBackend::Bash => "Bash",
        ShellBackend::Zsh => "zsh",
        ShellBackend::Sh => "sh",
        ShellBackend::Pwsh => "PowerShell 7",
        ShellBackend::Powershell => "Windows PowerShell",
    }
}

/// The name an operating system goes by in settings.
pub fn machine_os_label(os: MachineOs) -> &'static str {
    match os {
        MachineOs::Windows => "Windows",
        MachineOs::Macos => "macOS",
        MachineOs::Linux => "Linux",
        MachineOs::Wsl => "WSL",
    }
}

/// The backend a shell tool runs in, or `None` for any other tool. Mirrors `ShellBackend::of_tool`.
pub fn backend_of_tool(tool_name: &str) -> Option<ShellBackend> {
    parse_shell_backend(tool_name)
}

/// Whether `os` runs `backend`.
pub fn is_registered(os: MachineOs, backend: ShellBackend) -> bool {
    shell_backends_by_os(os).contains(&backend)
}

/// The first backend in `os`'s priority order that `available` has: the agent shell a newly probed machine starts with,
/// and the shell tool a fresh install's presets turn on. Mirrors `shell_backend::preferred_backend`.
pub fn preferred_backend(os: MachineOs, available: &[ShellBackend]) -> Option<ShellBackend> {
    shell_backends_by_os(os).iter().find(|backend| available.contains(backend)).copied()
}

/// The OS a platform string (`navigator.platform`'s values) names, or `None` when it names none this table knows.
fn host_machine_os(platform: &str) -> Option<MachineOs> {
    let value = platform.trim().to_ascii_lowercase();
    if value.starts_with("win") {
        Some(MachineOs::Windows)
    } else if value.starts_with("mac") || value.starts_with("iphone") || value.starts_with("ipad") {
        Some(MachineOs::Macos)
    } else if value.starts_with("linux") {
        Some(MachineOs::Linux)
    } else {
        None
    }
}

/// What upstream reads from `navigator.platform`, for the functions here that take a platform.
pub fn host_platform() -> &'static str {
    if cfg!(windows) {
        "Win32"
    } else if cfg!(target_os = "macos") {
        "MacIntel"
    } else if cfg!(target_os = "linux") {
        "Linux x86_64"
    } else {
        ""
    }
}

/// A machine's OS and shells as far as they are known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownShells {
    pub os: Option<MachineOs>,
    pub backends: Vec<ShellBackend>,
}

/// A machine's OS and shells as far as they are known: its last probe, or, for a remote machine never probed, bash
/// alone, which is what every remote leg ran before machines had backends. Mirrors `machine_shells::known`.
///
/// This machine is probed at startup, and until that answer arrives its OS's registered backends stand in, from the
/// platform string; an unrecognized platform keeps every backend, because hiding a tool the host would run is the worse
/// mistake.
pub fn known_shells(machine: Option<&RunTarget>, probes: &ShellProbes, platform: &str) -> KnownShells {
    if let Some(probed) = probes.get(&run_env_key(machine)) {
        return KnownShells { os: Some(probed.os), backends: probed.shells.iter().map(|shell| shell.backend).collect() };
    }
    let Some(machine) = machine else {
        let os = host_machine_os(platform);
        return KnownShells { os, backends: os.map_or_else(|| SHELL_BACKENDS.to_vec(), |os| shell_backends_by_os(os).to_vec()) };
    };
    KnownShells { os: matches!(machine, RunTarget::Wsl { .. }).then_some(MachineOs::Wsl), backends: vec![ShellBackend::Bash] }
}

/// The shell backends the tool list offers for a set of workspaces: the union of their machines' backends, in tool
/// order. No workspace at all is this machine, as the host resolves it.
pub fn available_shell_backends(workspaces: &[AttachedWorkspace], probes: &ShellProbes, platform: &str) -> Vec<ShellBackend> {
    let mut found = Vec::new();
    if workspaces.is_empty() {
        found.extend(known_shells(None, probes, platform).backends);
    }
    for workspace in workspaces {
        found.extend(known_shells(workspace.machine.as_ref(), probes, platform).backends);
    }
    SHELL_BACKENDS.into_iter().filter(|backend| found.contains(backend)).collect()
}

/// The terminal shell a backend's terminal starts. Terminals are not split the way the shell tools are: PowerShell is
/// one terminal shell, which the host starts as `pwsh` where installed and as `powershell.exe` otherwise.
fn terminal_shell_of(backend: ShellBackend) -> TerminalShell {
    match backend {
        ShellBackend::Pwsh | ShellBackend::Powershell => TerminalShell::Powershell,
        ShellBackend::Bash => TerminalShell::Bash,
        ShellBackend::Zsh => TerminalShell::Zsh,
        ShellBackend::Sh => TerminalShell::Sh,
    }
}

/// Whether a terminal on `machine` can start `shell`. Mirrors the host's `terminal::TerminalLaunch`: this machine
/// refuses `sh`, whose line editor cannot hold a line for the Git mutex; a WSL distribution has no PowerShell; an SSH
/// machine runs whatever it has, PowerShell being its agent's own default on Windows.
fn terminal_can_start(machine: Option<&RunTarget>, shell: TerminalShell) -> bool {
    match machine {
        None => shell != TerminalShell::Sh,
        Some(RunTarget::Wsl { .. }) => shell != TerminalShell::Powershell,
        Some(RunTarget::Ssh { .. }) => true,
    }
}

/// The shells a terminal in a workspace on `machine` can start, most preferred first: the ones its probe found, in its
/// OS's priority order, that a terminal there can run. Both PowerShell editions are one terminal shell, listed once at
/// the place of the more preferred. The first is the one a terminal nobody chose a shell for starts. Empty when the
/// probe found none of them.
pub fn terminal_shells_for(machine: Option<&RunTarget>, probes: &ShellProbes, platform: &str) -> Vec<TerminalShell> {
    let known = known_shells(machine, probes, platform);
    let ranked: &[ShellBackend] = known.os.map_or(&SHELL_BACKENDS, shell_backends_by_os);
    let mut shells = Vec::new();
    for backend in ranked {
        if !known.backends.contains(backend) {
            continue;
        }
        let shell = terminal_shell_of(*backend);
        if !shells.contains(&shell) && terminal_can_start(machine, shell) {
            shells.push(shell);
        }
    }
    shells
}

/// The catalog with every shell tool whose backend is not in `backends` removed.
pub fn tools_for_shell_backends<T: Clone>(tools: &[T], name: impl Fn(&T) -> &str, backends: &[ShellBackend]) -> Vec<T> {
    tools.iter().filter(|tool| backend_of_tool(name(tool)).is_none_or(|backend| backends.contains(&backend))).cloned().collect()
}

/// The agent shell recorded in a machine's settings, if any.
fn configured_agent_shell(machine: &RunTarget, assets: &ExecutionEnvironmentAssets) -> Option<ShellBackend> {
    // The host's type can't be named here; its serde form is its name.
    fn read(value: Value) -> Option<ShellBackend> {
        serde_json::from_value(value).ok()
    }
    match machine {
        RunTarget::Wsl { distro } => assets.wsl_agent_shells.get(distro).and_then(|shell| read(json!(shell))),
        RunTarget::Ssh { machine_id } => assets.ssh_machines.iter().find(|entry| entry.id == *machine_id).and_then(|entry| read(json!(entry.agent_shell))),
    }
}

/// The agent shell a machine's scripts run in, as the host resolves it: the one its settings chose when the machine
/// still has it, else the first of its OS's priority order the probe found. `None` for this machine, which has none.
pub fn effective_agent_shell(machine: Option<&RunTarget>, assets: &ExecutionEnvironmentAssets, probes: &ShellProbes) -> Option<ShellBackend> {
    let machine = machine?;
    let configured = configured_agent_shell(machine, assets);
    let Some(probed) = probes.get(&run_env_key(Some(machine))) else {
        return Some(configured.unwrap_or(ShellBackend::Bash));
    };
    let available: Vec<ShellBackend> = probed.shells.iter().map(|shell| shell.backend).collect();
    if let Some(configured) = configured.filter(|configured| available.contains(configured)) {
        return Some(configured);
    }
    Some(preferred_backend(probed.os, &available).unwrap_or(ShellBackend::Bash))
}

/// Sets `machine`'s agent shell. `false` when nothing would change.
pub fn with_agent_shell(assets: &mut ExecutionEnvironmentAssets, machine: &RunTarget, backend: ShellBackend) -> bool {
    if configured_agent_shell(machine, assets) == Some(backend) {
        return false;
    }
    match machine {
        RunTarget::Wsl { distro } => {
            if let Ok(shell) = serde_json::from_value(json!(backend)) {
                assets.wsl_agent_shells.insert(distro.clone(), shell);
                return true;
            }
            false
        }
        RunTarget::Ssh { machine_id } => {
            let Some(entry) = assets.ssh_machines.iter_mut().find(|entry| entry.id == *machine_id) else { return false };
            entry.agent_shell = serde_json::from_value(json!(backend)).ok();
            entry.updated_at = crate::doc::now_iso();
            true
        }
    }
}

/// Records a machine's agent shell the first time it is probed: the first backend of its OS's priority order that it
/// has. A machine that already has a choice keeps it. `false` when nothing changed.
pub fn with_default_agent_shell(assets: &mut ExecutionEnvironmentAssets, key: &str, shells: &MachineShells) -> bool {
    let Some(machine) = machine_of_env_key(key) else { return false };
    if let RunTarget::Ssh { machine_id } = &machine {
        if !assets.ssh_machines.iter().any(|entry| entry.id == *machine_id) {
            return false;
        }
    }
    if configured_agent_shell(&machine, assets).is_some() {
        return false;
    }
    let available: Vec<ShellBackend> = shells.shells.iter().map(|shell| shell.backend).collect();
    match preferred_backend(shells.os, &available) {
        Some(preferred) => with_agent_shell(assets, &machine, preferred),
        None => false,
    }
}

/// The address an SSH machine is reached at: host, port and key, as one comparable value. `None` for any other machine,
/// and for an SSH machine the catalog no longer has. A machine keeps its id when its settings are edited, so this, not
/// the id, says whether a probe still describes it. Mirrors `machine_shells::Endpoint`.
pub fn ssh_endpoint(assets: Option<&ExecutionEnvironmentAssets>, machine: Option<&RunTarget>) -> Option<String> {
    let Some(RunTarget::Ssh { machine_id }) = machine else { return None };
    let config = assets?.ssh_machines.iter().find(|entry| entry.id == *machine_id)?;
    Some(json!([config.host, config.port, config.identity_file]).to_string())
}

/// Drops the answer kept under `key`. `false` when there was none.
pub fn without_probe(probes: &mut ShellProbes, key: &str) -> bool {
    probes.remove(key).is_some()
}

/// The machine an environment key names, or `None` for this machine and anything unrecognized.
fn machine_of_env_key(key: &str) -> Option<RunTarget> {
    if let Some(distro) = key.strip_prefix("wsl:").filter(|rest| !rest.is_empty()) {
        return Some(RunTarget::Wsl { distro: distro.to_owned() });
    }
    if let Some(machine_id) = key.strip_prefix("ssh:").filter(|rest| !rest.is_empty()) {
        return Some(RunTarget::Ssh { machine_id: machine_id.to_owned() });
    }
    None
}

#[derive(Default)]
struct MachineShellsState {
    probes: ShellProbes,
}

impl App {
    /// `machineShells`: every machine's last shell probe, by environment key.
    pub fn machine_shells(&mut self) -> &ShellProbes {
        &self.local::<MachineShellsState>().probes
    }

    /// `setMachineShells(withoutProbe(...))`: until `machine` answers again it reads as never probed, so a failing probe of a
    /// new endpoint does not leave the old answer in force.
    pub fn forget_machine_probe(&mut self, machine: &RunTarget) {
        without_probe(&mut self.local::<MachineShellsState>().probes, &run_env_key(Some(machine)));
    }

    /// Asks the host for what's already probed; a push event's answer wins over what this brings. Part of [`crate::doc::init`].
    pub(crate) fn init_machine_shells(&mut self) {
        self.invoke::<ShellProbes>("list_machine_shells", json!({}), |app, result| {
            if let Ok(probes) = result {
                let state = app.local::<MachineShellsState>();
                let mut merged = probes;
                merged.append(&mut state.probes);
                state.probes = merged;
                app.record_default_agent_shells();
            }
        });
        self.on_push_event(|app, event| {
            if event["type"] != "machineShellsChanged" {
                return;
            }
            let Some(key) = event["key"].as_str() else { return };
            let Ok(shells) = serde_json::from_value::<MachineShells>(event["shells"].clone()) else { return };
            app.local::<MachineShellsState>().probes.insert(key.to_owned(), shells);
            app.record_default_agent_shells();
        });
    }

    /// A machine's first probe records its agent shell: the first backend of its OS's priority list that it has. That
    /// is the priority list's only use: a machine that already has a choice keeps it whatever the list says later.
    pub(crate) fn record_default_agent_shells(&mut self) {
        // Taken out rather than cloned: this runs after every document change.
        let probes = std::mem::take(&mut self.local::<MachineShellsState>().probes);
        if probes.is_empty() {
            return;
        }
        self.update_document_if(|doc| {
            let mut changed = false;
            for (key, shells) in &probes {
                changed |= with_default_agent_shell(&mut doc.assets.execution_environments, key, shells);
            }
            changed
        });
        self.local::<MachineShellsState>().probes = probes;
    }

    /// `probeMachineShells`: probes one machine for its shell backends now; `None` is this machine.
    pub fn probe_machine_shells(&mut self, machine: Option<RunTarget>, done: impl FnOnce(&mut App, Result<MachineShells, String>) + Send + 'static) {
        self.invoke::<MachineShells>("probe_machine_shells", json!({ "machine": machine }), done);
    }

    /// `terminalShellsOn`: the shells a terminal on `machine` can start, as its probe found them, most preferred first.
    pub fn terminal_shells_on(&mut self, machine: Option<&RunTarget>) -> Vec<TerminalShell> {
        terminal_shells_for(machine, self.machine_shells(), host_platform())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SshMachineConfig;
    use ShellBackend::*;

    fn probe(os: MachineOs, backends: &[ShellBackend]) -> MachineShells {
        MachineShells { os, shells: backends.iter().map(|backend| DetectedShell { backend: *backend, path: format!("/bin/{}", backend.as_str()) }).collect(), probed_at: "2026-09-23T00:00:00Z".into() }
    }

    fn assets() -> ExecutionEnvironmentAssets {
        ExecutionEnvironmentAssets { ssh_machines: vec![SshMachineConfig { id: "m1".into(), name: "devbox".into(), host: "dev@devbox".into(), ..Default::default() }], ..Default::default() }
    }

    fn probes<const N: usize>(entries: [(&str, MachineShells); N]) -> ShellProbes {
        entries.into_iter().map(|(key, value)| (key.to_owned(), value)).collect()
    }

    fn ssh() -> RunTarget {
        RunTarget::Ssh { machine_id: "m1".into() }
    }

    fn wsl() -> RunTarget {
        RunTarget::Wsl { distro: "Ubuntu".into() }
    }

    fn workspace(machine: Option<RunTarget>, path: &str) -> AttachedWorkspace {
        AttachedWorkspace { machine, path: path.into() }
    }

    #[test]
    fn registers_no_shell_that_cannot_carry_mewrks_own_scripts() {
        assert_eq!(shell_backends_by_os(MachineOs::Windows), &[Pwsh, Powershell, Bash]);
        for os in [MachineOs::Wsl, MachineOs::Macos, MachineOs::Linux] {
            assert!(!shell_backends_by_os(os).contains(&Powershell));
            assert!(!shell_backends_by_os(os).contains(&Pwsh));
        }
    }

    #[test]
    fn maps_every_shell_tool_back_to_its_backend() {
        assert_eq!(backend_of_tool("zsh"), Some(Zsh));
        assert_eq!(backend_of_tool("pwsh"), Some(Pwsh));
        assert_eq!(backend_of_tool("powershell"), Some(Powershell));
        assert_eq!(backend_of_tool("read"), None);
        assert_eq!(backend_of_tool("shell"), None);
        // Only the exact tool names are shell tools.
        assert_eq!(backend_of_tool("PowerShell"), None);
        assert_eq!(backend_of_tool("powershell7"), None);
    }

    #[test]
    fn recognizes_both_powershell_editions_as_backends_and_nothing_else_spelled_like_them() {
        for backend in ["bash", "zsh", "sh", "pwsh", "powershell"] {
            assert!(parse_shell_backend(backend).is_some());
        }
        for value in ["fish", "PowerShell", "pwsh.exe", ""] {
            assert!(parse_shell_backend(value).is_none());
        }
    }

    #[test]
    fn names_the_two_powershell_editions_apart() {
        assert_eq!(shell_backend_label(Pwsh), "PowerShell 7");
        assert_eq!(shell_backend_label(Powershell), "Windows PowerShell");
        assert_eq!(shell_backend_label(Bash), "Bash");
        assert_eq!(shell_backend_label(Zsh), "zsh");
        assert_eq!(shell_backend_label(Sh), "sh");
    }

    #[test]
    fn ranks_each_oss_shells_in_mewrks_own_fixed_order() {
        assert_eq!(shell_backends_by_os(MachineOs::Linux), &[Bash, Zsh, Sh]);
        assert_eq!(shell_backends_by_os(MachineOs::Macos), &[Zsh, Bash, Sh]);
        assert_eq!(shell_backends_by_os(MachineOs::Wsl), &[Bash, Zsh, Sh]);
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash, Powershell, Pwsh]), Some(Pwsh));
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash, Powershell]), Some(Powershell));
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash, Pwsh]), Some(Pwsh));
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash]), Some(Bash));
        assert_eq!(preferred_backend(MachineOs::Macos, &[Sh, Bash]), Some(Bash));
        assert_eq!(preferred_backend(MachineOs::Macos, &[]), None);
    }

    #[test]
    fn lists_the_union_of_the_machines_probed_backends() {
        let windows = workspace(Some(ssh()), "C:/work");
        let local = workspace(None, "/Users/dev/app");
        let probes = probes([("local", probe(MachineOs::Macos, &[Zsh, Bash, Sh])), ("ssh:m1", probe(MachineOs::Windows, &[Powershell]))]);
        assert_eq!(available_shell_backends(&[local, windows.clone()], &probes, "MacIntel"), vec![Bash, Zsh, Sh, Powershell]);
        assert_eq!(available_shell_backends(&[windows], &probes, "MacIntel"), vec![Powershell]);
        let tools = ["read", "bash", "powershell", "zsh"];
        assert_eq!(tools_for_shell_backends(&tools, |tool| tool, &[Powershell]), vec!["read", "powershell"]);
    }

    #[test]
    fn offers_each_powershell_editions_tool_only_where_that_edition_was_found() {
        let windows = workspace(Some(ssh()), "C:/work");
        let both = probes([("ssh:m1", probe(MachineOs::Windows, &[Powershell, Pwsh, Bash]))]);
        // Tool order, not probe order: pwsh sits before powershell.
        assert_eq!(available_shell_backends(std::slice::from_ref(&windows), &both, "MacIntel"), vec![Bash, Pwsh, Powershell]);
        let only7 = probes([("ssh:m1", probe(MachineOs::Windows, &[Pwsh]))]);
        assert_eq!(available_shell_backends(&[windows], &only7, "MacIntel"), vec![Pwsh]);
        let tools = ["read", "bash", "pwsh", "powershell", "zsh"];
        assert_eq!(tools_for_shell_backends(&tools, |tool| tool, &[Pwsh]), vec!["read", "pwsh"]);
        assert_eq!(tools_for_shell_backends(&tools, |tool| tool, &[Powershell]), vec!["read", "powershell"]);
        assert_eq!(tools_for_shell_backends(&tools, |tool| tool, &[Bash, Pwsh, Powershell]), vec!["read", "bash", "pwsh", "powershell"]);
    }

    #[test]
    fn assumes_bash_on_a_remote_machine_nobody_has_probed_and_the_host_oss_shells_before_the_hosts_probe() {
        let none = ShellProbes::new();
        assert_eq!(known_shells(Some(&ssh()), &none, "Win32"), KnownShells { os: None, backends: vec![Bash] });
        assert_eq!(known_shells(Some(&wsl()), &none, "Win32").os, Some(MachineOs::Wsl));
        assert_eq!(known_shells(None, &none, "Win32").backends, vec![Pwsh, Powershell, Bash]);
        assert_eq!(known_shells(None, &none, "MacIntel").backends, vec![Zsh, Bash, Sh]);
        // An unknown platform keeps every tool rather than hiding one the host would run.
        assert_eq!(known_shells(None, &none, "").backends, vec![Bash, Zsh, Sh, Pwsh, Powershell]);
        // No workspace at all is this machine.
        assert_eq!(available_shell_backends(&[], &none, "Linux x86_64"), vec![Bash, Zsh, Sh]);
    }

    #[test]
    fn lists_the_machines_probed_shells_in_its_oss_priority_order() {
        use TerminalShell as T;
        let probes = probes([("local", probe(MachineOs::Macos, &[Zsh, Bash, Sh])), ("wsl:Ubuntu", probe(MachineOs::Wsl, &[Bash, Sh]))]);
        // This machine's sh is left out: its line editor cannot hold a line for the Git mutex.
        assert_eq!(terminal_shells_for(None, &probes, "MacIntel"), vec![T::Zsh, T::Bash]);
        // A shell the probe did not find is not offered, however high it ranks.
        assert_eq!(terminal_shells_for(Some(&wsl()), &probes, "Win32"), vec![T::Bash, T::Sh]);
    }

    #[test]
    fn offers_powershell_on_a_windows_host_and_a_windows_ssh_machine_never_in_wsl() {
        use TerminalShell as T;
        let probes = probes([("local", probe(MachineOs::Windows, &[Powershell, Bash])), ("ssh:m1", probe(MachineOs::Windows, &[Powershell, Bash])), ("wsl:Ubuntu", probe(MachineOs::Wsl, &[Bash]))]);
        assert_eq!(terminal_shells_for(None, &probes, "Win32"), vec![T::Powershell, T::Bash]);
        assert_eq!(terminal_shells_for(Some(&ssh()), &probes, "Win32"), vec![T::Powershell, T::Bash]);
        assert_eq!(terminal_shells_for(Some(&wsl()), &probes, "Win32"), vec![T::Bash]);
    }

    #[test]
    fn offers_one_powershell_terminal_however_many_editions_a_machine_has() {
        use TerminalShell as T;
        // The terminal is not split like the shell tools: it starts PowerShell 7 where installed and Windows PowerShell otherwise.
        let all = probes([("local", probe(MachineOs::Windows, &[Pwsh, Powershell, Bash])), ("ssh:m1", probe(MachineOs::Windows, &[Powershell, Pwsh, Bash]))]);
        assert_eq!(terminal_shells_for(None, &all, "Win32"), vec![T::Powershell, T::Bash]);
        assert_eq!(terminal_shells_for(Some(&ssh()), &all, "Win32"), vec![T::Powershell, T::Bash]);
        // Either edition alone is the one PowerShell terminal as well.
        assert_eq!(terminal_shells_for(None, &probes([("local", probe(MachineOs::Windows, &[Pwsh, Bash]))]), "Win32"), vec![T::Powershell, T::Bash]);
        assert_eq!(terminal_shells_for(None, &probes([("local", probe(MachineOs::Windows, &[Pwsh]))]), "Win32"), vec![T::Powershell]);
        assert_eq!(terminal_shells_for(None, &probes([("local", probe(MachineOs::Windows, &[Powershell]))]), "Win32"), vec![T::Powershell]);
    }

    #[test]
    fn keeps_the_priority_order_while_deduplicating_the_powershell_editions() {
        use TerminalShell as T;
        // An OS the renderer cannot name ranks every backend in tool order, where the two editions sit after bash:
        // PowerShell is listed once, last.
        assert_eq!(terminal_shells_for(None, &ShellProbes::new(), ""), vec![T::Bash, T::Zsh, T::Powershell]);
        // On Windows the editions outrank bash, so the one PowerShell leads.
        assert_eq!(terminal_shells_for(None, &probes([("local", probe(MachineOs::Windows, &[Bash, Powershell, Pwsh]))]), "Win32"), vec![T::Powershell, T::Bash]);
    }

    #[test]
    fn never_offers_a_powershell_terminal_in_wsl_whatever_the_probe_reports() {
        let odd = probes([("wsl:Ubuntu", probe(MachineOs::Wsl, &[Bash, Pwsh, Powershell]))]);
        assert_eq!(terminal_shells_for(Some(&wsl()), &odd, "Win32"), vec![TerminalShell::Bash]);
    }

    #[test]
    fn falls_back_to_what_is_assumed_of_a_machine_nobody_has_probed() {
        use TerminalShell as T;
        let none = ShellProbes::new();
        assert_eq!(terminal_shells_for(None, &none, "Win32"), vec![T::Powershell, T::Bash]);
        assert_eq!(terminal_shells_for(None, &none, "Linux x86_64"), vec![T::Bash, T::Zsh]);
        assert_eq!(terminal_shells_for(Some(&ssh()), &none, "MacIntel"), vec![T::Bash]);
    }

    #[test]
    fn offers_nothing_when_the_probe_found_nothing_a_terminal_can_start() {
        assert_eq!(terminal_shells_for(Some(&ssh()), &probes([("ssh:m1", probe(MachineOs::Linux, &[]))]), "MacIntel"), vec![]);
    }

    fn agent_shell_of_m1(assets: &ExecutionEnvironmentAssets) -> Option<ShellBackend> {
        configured_agent_shell(&ssh(), assets)
    }

    #[test]
    fn records_a_new_machines_first_probe_as_its_oss_most_preferred_backend_it_has() {
        let mut next = assets();
        assert!(with_default_agent_shell(&mut next, "ssh:m1", &probe(MachineOs::Linux, &[Sh, Zsh, Bash])));
        assert_eq!(agent_shell_of_m1(&next), Some(Bash));
        // A machine that already has a choice keeps it.
        assert!(!with_default_agent_shell(&mut next, "ssh:m1", &probe(MachineOs::Linux, &[Bash])));
        // This machine has no agent shell, and a deleted machine has nothing to record.
        let mut untouched = assets();
        assert!(!with_default_agent_shell(&mut untouched, "local", &probe(MachineOs::Macos, &[Zsh])));
        assert!(!with_default_agent_shell(&mut untouched, "ssh:gone", &probe(MachineOs::Linux, &[Bash])));
        assert_eq!(untouched, assets());
    }

    #[test]
    fn records_a_windows_machines_first_probe_as_powershell_7_when_it_has_it_else_windows_powershell() {
        for (found, expected) in [(&[Bash, Powershell, Pwsh][..], Pwsh), (&[Bash, Powershell][..], Powershell), (&[Bash][..], Bash)] {
            let mut next = assets();
            with_default_agent_shell(&mut next, "ssh:m1", &probe(MachineOs::Windows, found));
            assert_eq!(agent_shell_of_m1(&next), Some(expected));
        }
    }

    #[test]
    fn lets_either_powershell_edition_be_chosen_as_an_agent_shell() {
        let machine = ssh();
        let mut pwsh = assets();
        assert!(with_agent_shell(&mut pwsh, &machine, Pwsh));
        assert_eq!(agent_shell_of_m1(&pwsh), Some(Pwsh));
        // The same value again changes nothing.
        assert!(!with_agent_shell(&mut pwsh, &machine, Pwsh));
        let both = probes([("ssh:m1", probe(MachineOs::Windows, &[Pwsh, Powershell, Bash]))]);
        assert_eq!(effective_agent_shell(Some(&machine), &pwsh, &both), Some(Pwsh));
        // A saved Windows PowerShell choice is kept even though PowerShell 7 outranks it.
        let mut five = assets();
        with_agent_shell(&mut five, &machine, Powershell);
        assert_eq!(effective_agent_shell(Some(&machine), &five, &both), Some(Powershell));
        // PowerShell 7 uninstalled: the choice falls back by priority, to the other edition.
        assert_eq!(effective_agent_shell(Some(&machine), &pwsh, &probes([("ssh:m1", probe(MachineOs::Windows, &[Powershell, Bash]))])), Some(Powershell));
        // Neither edition left: bash.
        assert_eq!(effective_agent_shell(Some(&machine), &pwsh, &probes([("ssh:m1", probe(MachineOs::Windows, &[Bash]))])), Some(Bash));
        // Before any probe the configured choice stands.
        assert_eq!(effective_agent_shell(Some(&machine), &pwsh, &ShellProbes::new()), Some(Pwsh));
    }

    #[test]
    fn records_a_wsl_distributions_choice_by_name() {
        let mut next = assets();
        with_default_agent_shell(&mut next, "wsl:Ubuntu", &probe(MachineOs::Wsl, &[Bash, Sh]));
        assert_eq!(configured_agent_shell(&wsl(), &next), Some(Bash));
        with_agent_shell(&mut next, &wsl(), Sh);
        assert_eq!(configured_agent_shell(&wsl(), &next), Some(Sh));
    }

    #[test]
    fn resolves_a_choice_the_machine_no_longer_has_the_way_the_host_does() {
        let machine = ssh();
        let mut chosen = assets();
        with_agent_shell(&mut chosen, &machine, Zsh);
        assert_eq!(effective_agent_shell(Some(&machine), &chosen, &probes([("ssh:m1", probe(MachineOs::Linux, &[Zsh, Bash]))])), Some(Zsh));
        assert_eq!(effective_agent_shell(Some(&machine), &chosen, &probes([("ssh:m1", probe(MachineOs::Linux, &[Bash, Sh]))])), Some(Bash));
        assert_eq!(effective_agent_shell(Some(&machine), &chosen, &ShellProbes::new()), Some(Zsh));
        assert_eq!(effective_agent_shell(None, &chosen, &ShellProbes::new()), None);
    }

    #[test]
    fn is_told_apart_by_its_endpoint_not_its_id() {
        let machine = ssh();
        let before = ssh_endpoint(Some(&assets()), Some(&machine));
        assert!(before.is_some());
        assert_eq!(ssh_endpoint(Some(&assets()), Some(&machine)), before);
        let mut moved = assets();
        moved.ssh_machines[0].host = "dev@other".into();
        assert_ne!(ssh_endpoint(Some(&moved), Some(&machine)), before);
        let empty = ExecutionEnvironmentAssets::default();
        assert_eq!(ssh_endpoint(Some(&empty), Some(&machine)), None);
        assert_eq!(ssh_endpoint(Some(&assets()), Some(&wsl())), None);
        assert_eq!(ssh_endpoint(Some(&assets()), None), None);
    }

    #[test]
    fn loses_its_old_probe_so_it_reads_as_never_probed_until_the_new_address_answers() {
        let mut all = probes([("local", probe(MachineOs::Macos, &[Zsh])), ("ssh:m1", probe(MachineOs::Linux, &[Bash]))]);
        assert!(without_probe(&mut all, "ssh:m1"));
        assert_eq!(all.keys().collect::<Vec<_>>(), vec!["local"]);
        assert_eq!(known_shells(Some(&ssh()), &all, "MacIntel"), KnownShells { os: None, backends: vec![Bash] });
        assert!(!without_probe(&mut all, "ssh:m1"));
    }
}
