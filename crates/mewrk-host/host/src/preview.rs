//! The seam between `.mewrk/launch.json` and the dev servers it describes.
//!
//! [`crate::preview_launch_config`] reads the file and [`crate::preview_servers`]
//! owns the processes; neither knows the other exists. This module is the only
//! place that does, so the order a start has to follow — resolve the entry, hand
//! back an attach entry before anything else looks at it, decide whether a running
//! server already answers it, reserve capacity, reserve the port, rewrite the port
//! arguments, spawn — stays in one readable function instead of being spread
//! across Tauri commands and a future tool handler.
//!
//! A remote-control entry (`docs/remote-control.md`) takes its own path from the same
//! start: its place is resolved against a [`ControlEnvironment`] the caller builds, its
//! `launch` entry is started through the ordinary process path, and its session becomes a
//! registry row ([`start_control`]).
//!
//! Two voices meet here. Anything that came out of the two ported modules is the
//! Claude Code desktop app's own English, passed through byte for byte because
//! those strings are the diagnosis; anything this layer decides for itself is in
//! the host's ordinary voice, like every other command error.
//!
//! One deliberate departure from that byte-for-byte rule: the config path reads
//! `.mewrk/launch.json`, not the source's `.claude/launch.json`. The source's
//! path belongs to a different product that may be installed alongside this one,
//! so sharing it would have the two fight over one file.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    preview_launch_config::{self, LaunchConfigDiscovery, LaunchDiscovery, ServerConfig},
    preview_remote::RemoteMachine,
    preview_servers::{
        self, ControlLaunch, ControlRowSpec, ControlStart, PreviewLogQuery, PreviewServerConfig,
        PreviewServerRegistry, PreviewServerSnapshot, PreviewServerStatus, PreviewStartAction,
        RemoteServerHost,
    },
    remote_control::{
        place::{device_lock_key, ControlEnvironment},
        session::{Preflight, RemoteControlSession, SessionSpec},
        target::{ControlTarget, Protocol},
    },
};

/// How long `preview_start` waits for a remote-control session to be ready, to ask for a
/// sign-in, or to fail (`docs/remote-control.md` §3.2). Still connecting after that, it is
/// handed back as `starting` and keeps connecting in the background.
const CONTROL_SETTLE_WAIT: Duration = Duration::from_secs(30);

/// How often a remote-control start looks at the port of the `launch` entry it just started.
const LAUNCH_PORT_POLL: Duration = Duration::from_millis(300);

/// What `preview_logs` answers when the id addresses nothing.
pub const NO_SERVER_FOR_LOGS: &str =
    "No dev server is running. preview_logs takes a process serverId from preview_list.";

/// What `preview_start` says once it has pointed the preview at an attach entry's
/// url. The sentence, and the fact that it is the whole report, are the source's.
pub const ATTACHED_NOTICE: &str =
    "Attached the preview to the configured url; no process was started.";

/// What `preview_stop` answers when handed an attach entry's id.
pub fn no_stop_for_attachment(server_id: &str) -> String {
    format!(
        "\"{}\" attaches the preview to a url; no process was started for it, so there is nothing to stop.",
        preview_launch_config::sanitize_for_message(server_id)
    )
}

/// A repeated name's entry as the model addresses it: the name as the file writes it, and the
/// number its first start gave it.
fn numbered_server_id(name: &str, number: u32) -> String {
    format!("{name}-{number}")
}

/// Whether `server_id` is what `preview_start` answered for an entry of `list` that attaches
/// rather than runs: the entry's name, or — for a name the file repeats — the name numbered.
pub fn is_attach_entry(list: &PreviewConfigurationList, server_id: &str) -> bool {
    let repeated = |name: &str| {
        list.servers
            .iter()
            .filter(|server| preview_launch_config::names_match(&server.name, name))
            .count()
            > 1
    };
    list.servers
        .iter()
        .filter(|server| server.command.is_none() && server.url.is_some())
        .any(|server| {
            preview_launch_config::names_match(&server.name, server_id)
                || server_id.rsplit_once('-').is_some_and(|(base, number)| {
                    preview_launch_config::names_match(&server.name, base)
                        && number.parse::<u32>().is_ok_and(|number| number >= 1)
                        && repeated(&server.name)
                })
        })
}

/// Whether a `serverId` the model passed is `server_id`. Ids are names, and names are matched the
/// way `preview_start` matches them.
pub fn server_ids_match(server_id: &str, requested: &str) -> bool {
    preview_launch_config::names_match(server_id, requested)
}

/// The entry of `list` a server was started from: the one with its name, or — for a name the
/// file repeats — the one its number was given to in `worktree`.
pub fn configured_entry<'a>(
    registry: &PreviewServerRegistry,
    worktree: &Path,
    list: &'a PreviewConfigurationList,
    server: &PreviewServerSnapshot,
) -> Option<&'a PreviewConfiguredServer> {
    let siblings: Vec<&PreviewConfiguredServer> = list
        .servers
        .iter()
        .filter(|entry| preview_launch_config::names_match(&entry.name, &server.name))
        .collect();
    if siblings.len() < 2 {
        return siblings.first().copied();
    }
    let number = server.server_id.rsplit_once('-')?.1.parse::<usize>().ok()?;
    let occurrence = *registry
        .repeated_name_order(worktree, &siblings[0].name)
        .get(number.checked_sub(1)?)?;
    siblings.get(occurrence).copied()
}

/// What one of a conversation's workspaces configures, wherever it is. `None` when that cannot be
/// read right now: a workspace on WSL, or a machine that is away.
pub fn workspace_configurations(
    workspace: &crate::workspace_set::ResolvedWorkspace,
) -> Option<PreviewConfigurationList> {
    match &workspace.machine {
        None => Some(configurations(Path::new(&workspace.root))),
        Some(machine @ crate::model::RunTarget::Ssh { .. }) => remote_configurations(
            &crate::preview_remote::RemoteMachine::new(
                workspace.runner.clone(),
                crate::run_environment::env_key(Some(machine)),
                workspace.machine_label.clone(),
            )
            .with_patience(crate::preview_remote::POLL_PATIENCE),
            &workspace.root,
        )
        .ok(),
        Some(crate::model::RunTarget::Wsl { .. }) => None,
    }
}

/// [`workspace_configurations`] read now: for a workspace on an SSH machine, from the machine
/// rather than the pane's cache, and with the reason when it cannot be read. `None` for a
/// workspace on WSL, which has no previews.
pub fn workspace_configurations_fresh(
    workspace: &crate::workspace_set::ResolvedWorkspace,
) -> Option<Result<PreviewConfigurationList, String>> {
    match &workspace.machine {
        None => Some(Ok(configurations(Path::new(&workspace.root)))),
        Some(machine @ crate::model::RunTarget::Ssh { .. }) => Some(remote_configurations_fresh(
            &crate::preview_remote::RemoteMachine::new(
                workspace.runner.clone(),
                crate::run_environment::env_key(Some(machine)),
                workspace.machine_label.clone(),
            )
            .with_patience(crate::preview_remote::POLL_PATIENCE),
            &workspace.root,
        )),
        Some(crate::model::RunTarget::Wsl { .. }) => None,
    }
}

/// The registry key of one of a conversation's workspaces: its directory on this computer, or,
/// for one on an SSH machine, the machine-qualified key its servers are filed under there. `None`
/// for a workspace on WSL, which has no previews.
pub fn registry_key(
    workspace: &crate::workspace_set::ResolvedWorkspace,
) -> Option<std::path::PathBuf> {
    match &workspace.machine {
        None => Some(std::path::PathBuf::from(&workspace.root)),
        Some(machine @ crate::model::RunTarget::Ssh { .. }) => Some(
            crate::preview_remote::RemoteMachine::new(
                workspace.runner.clone(),
                crate::run_environment::env_key(Some(machine)),
                workspace.machine_label.clone(),
            )
            .worktree_key(&workspace.root),
        ),
        Some(crate::model::RunTarget::Wsl { .. }) => None,
    }
}

// ---------------------------------------------------------------------------
// Which entry a start addresses
// ---------------------------------------------------------------------------

/// One usable entry of a launch.json, as far as choosing which entry a start addresses needs it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryView<'a> {
    pub name: &'a str,
    /// Runs nothing and points the preview at a url: once attached, it has had its turn.
    pub attaches: bool,
}

impl<'a> EntryView<'a> {
    fn of_config(server: &'a ServerConfig) -> Self {
        Self {
            name: &server.name,
            attaches: server.command.is_none() && server.control.is_none(),
        }
    }

    fn of_listed(server: &'a PreviewConfiguredServer) -> Self {
        Self {
            name: &server.name,
            attaches: server.command.is_none() && server.remote_control.is_none(),
        }
    }
}

/// What the registry knows that decides which of a repeated name's entries a start addresses:
/// the order the name's entries were first started in the worktree
/// ([`PreviewServerRegistry::repeated_name_order`]), and the server ids running for the caller
/// ([`preview_servers::running_for_session`]).
pub(crate) struct RepeatFacts<'a> {
    pub order: &'a dyn Fn(&str) -> Vec<usize>,
    pub running: &'a [String],
}

/// How the entry a start picked is numbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Numbering {
    /// No other entry has its name: its id is its name.
    Unique,
    /// A numbered id the model passed, which this entry was given when it was first started.
    Known(u32),
    /// The `occurrence`th entry of a repeated name, keyed by the name of the entry at `key` (the
    /// first of them): numbered when the start runs, with the number it was given before or the
    /// next one ([`PreviewServerRegistry::number_repeated_name`]).
    Occurrence { key: usize, occurrence: usize },
}

/// What a start of one name addresses among a file's usable entries ([`pick_entry`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EntryPick {
    /// Entry `index`.
    One { index: usize, numbering: Numbering },
    /// One of these entries — which, only the registry's facts decide, and they were not given.
    AnyOf(Vec<usize>),
    /// A numbered id of a repeated name that no entry has been given yet. `first` is the first
    /// entry of the name, `siblings` how many entries share it.
    NotStarted { first: usize, siblings: usize },
    /// Nothing: `preview_launch_config::select_server` refuses the name, in its own words.
    Unselected,
}

impl EntryPick {
    /// Every entry this pick could start.
    pub(crate) fn candidates(&self) -> Vec<usize> {
        match self {
            Self::One { index, .. } => vec![*index],
            Self::AnyOf(indexes) => indexes.clone(),
            Self::NotStarted { .. } | Self::Unselected => Vec::new(),
        }
    }
}

/// Which entry a start of `requested` addresses — the one rule for it, shared by the start
/// ([`address_entry`]) and by everything that has to know beforehand what a start will do: the
/// approval of a remote-control entry (`security::remote_control_start_decision`) is only as good
/// as its agreement with the start on which entry that is.
///
/// Pure. `entries` are the file's usable entries in file order, `malformed` the names of the
/// entries it could not use. The name is trimmed, and an empty one names nothing, as the start
/// reads it. A name one entry has is that entry; a name the file repeats addresses the first of
/// its entries that is not running yet (an attach entry, once attached, has had its turn), and
/// once every one is, the one first started; a numbered id (`dev-2`) of a repeated name addresses
/// the entry its number was given to; anything else is the parser's selection, including its
/// guess that the only entry of a clean file is the one meant whatever it was called.
///
/// Without `facts` the repeated-name rules cannot be decided, and the answer is every entry they
/// could reach ([`EntryPick::AnyOf`]).
pub(crate) fn pick_entry(
    entries: &[EntryView<'_>],
    malformed: &[&str],
    requested: Option<&str>,
    facts: Option<&RepeatFacts<'_>>,
) -> EntryPick {
    let requested = requested.map(str::trim).filter(|name| !name.is_empty());
    let siblings_of = |name: &str| -> Vec<usize> {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| preview_launch_config::names_match(entry.name, name))
            .map(|(index, _)| index)
            .collect()
    };

    // A numbered id, unless an entry is literally called that.
    if let Some(requested) = requested.filter(|requested| siblings_of(requested).is_empty()) {
        if let Some((base, number)) = requested
            .rsplit_once('-')
            .and_then(|(base, number)| Some((base, number.parse::<u32>().ok()?)))
            .filter(|(_, number)| *number >= 1)
        {
            let siblings = siblings_of(base);
            if siblings.len() > 1 {
                let Some(facts) = facts else {
                    return EntryPick::AnyOf(siblings);
                };
                let order = (facts.order)(entries[siblings[0]].name);
                return match usize::try_from(number - 1)
                    .ok()
                    .and_then(|position| order.get(position))
                    .and_then(|occurrence| siblings.get(*occurrence))
                {
                    Some(index) => EntryPick::One {
                        index: *index,
                        numbering: Numbering::Known(number),
                    },
                    None => EntryPick::NotStarted {
                        first: siblings[0],
                        siblings: siblings.len(),
                    },
                };
            }
        }
    }

    let Some(selected) = select_entry(entries, malformed, requested) else {
        return EntryPick::Unselected;
    };
    let siblings = siblings_of(entries[selected].name);
    if siblings.len() < 2 {
        return EntryPick::One {
            index: selected,
            numbering: Numbering::Unique,
        };
    }
    let Some(facts) = facts else {
        return EntryPick::AnyOf(siblings);
    };
    let order = (facts.order)(entries[siblings[0]].name);
    let taken = |occurrence: usize| {
        let Some(position) = order.iter().position(|started| *started == occurrence) else {
            return false;
        };
        // An attach entry has no process to be running; once attached, it has had its turn. A
        // remote-control entry has no command either, but it is not attached: like a dev server it
        // is taken while it is running.
        if entries[siblings[occurrence]].attaches {
            return true;
        }
        let server_id = numbered_server_id(
            entries[siblings[occurrence]].name,
            u32::try_from(position + 1).unwrap_or(u32::MAX),
        );
        facts
            .running
            .iter()
            .any(|running| running.eq_ignore_ascii_case(&server_id))
    };
    let occurrence = (0..siblings.len())
        .find(|occurrence| !taken(*occurrence))
        .or_else(|| order.first().copied())
        .unwrap_or(0);
    EntryPick::One {
        index: siblings[occurrence],
        numbering: Numbering::Occurrence {
            key: siblings[0],
            occurrence,
        },
    }
}

/// The entry `preview_launch_config::select_server` selects, without its messages: with no name,
/// the only entry; a name's first entry; and for a name no entry has, the only entry of a file
/// with nothing malformed in it. `None` wherever it refuses.
fn select_entry(
    entries: &[EntryView<'_>],
    malformed: &[&str],
    requested: Option<&str>,
) -> Option<usize> {
    let Some(requested) = requested else {
        return (entries.len() == 1).then_some(0);
    };
    if let Some(index) = entries
        .iter()
        .position(|entry| preview_launch_config::names_match(entry.name, requested))
    {
        return Some(index);
    }
    if malformed
        .iter()
        .any(|name| preview_launch_config::names_match(name, requested))
    {
        return None;
    }
    (entries.len() == 1 && malformed.is_empty()).then_some(0)
}

/// [`pick_entry`] on a listed file, for a caller that cannot see the registry: every entry a
/// start of `requested` could pick.
pub fn start_candidates(list: &PreviewConfigurationList, requested: Option<&str>) -> Vec<usize> {
    let entries: Vec<EntryView<'_>> = list.servers.iter().map(EntryView::of_listed).collect();
    let malformed: Vec<&str> = list.malformed.iter().map(|entry| entry.name.as_str()).collect();
    pick_entry(&entries, &malformed, requested, None).candidates()
}

/// A remote-control entry as a start connects it: everything its approval card shows. A start of
/// a model's call connects an entry only when it is identical to one the person confirmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlEntryIdentity {
    pub name: String,
    pub target: ControlTarget,
    /// The entry its `launch` names, as the file configures it — command line, directory and
    /// variables included. `None` when it names none, or when the file has no single entry of that
    /// name (the start then refuses before anything runs).
    pub launch: Option<PreviewConfiguredServer>,
}

impl ControlEntryIdentity {
    /// Entry `index` of `servers`, when it is a remote-control entry.
    pub fn of(servers: &[PreviewConfiguredServer], index: usize) -> Option<Self> {
        let entry = servers.get(index)?;
        let target = entry.remote_control.clone()?;
        let launch = target.launch.as_deref().and_then(|launch| {
            let mut named = servers
                .iter()
                .filter(|server| preview_launch_config::names_match(&server.name, launch));
            match (named.next(), named.next()) {
                (Some(only), None) => Some(only.clone()),
                _ => None,
            }
        });
        Some(Self {
            name: entry.name.clone(),
            target,
            launch,
        })
    }
}

/// Which remote-control entries a start may connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlApproval {
    /// Whichever the name addresses: the person started it from the pane, or a caller that has no
    /// approval to hold it to.
    Person,
    /// A model's call: only the entries on the card the person confirmed for it. Empty when the
    /// call was not judged to start one at all.
    Confirmed(Vec<ControlEntryIdentity>),
}

impl ControlApproval {
    /// `Ok` when the start may connect `identity`, which it addresses as `server_id`.
    fn admits(&self, identity: &ControlEntryIdentity, server_id: &str) -> Result<(), String> {
        match self {
            Self::Person => Ok(()),
            Self::Confirmed(confirmed) if confirmed.contains(identity) => Ok(()),
            Self::Confirmed(_) => Err(format!(
                "\"{}\" is a remote-control entry, and this call was not confirmed for it as it is now: .mewrk/launch.json or the servers running changed after the call was checked, or the file could not be read then. Nothing was started or connected. Call preview_start again; connecting a screen is confirmed by the user every time.",
                preview_launch_config::sanitize_for_message(server_id)
            )),
        }
    }
}

/// What a `preview_start` call's workspace configures, read once when the run loop judges the
/// call — the reading its approval card is built from and its start is then held to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartReading {
    /// The file as the start reads it, and the entries the start would pick given the registry as
    /// it was then: one, or none when it would refuse the name.
    Read {
        list: PreviewConfigurationList,
        picks: Vec<usize>,
        /// The `launch` entries of the picked remote-control entries that were already running
        /// for the conversation then: the start hands such a process over rather than starting
        /// one, so the card shows what it was started from ([`RunningLaunch`]).
        running_launches: Vec<RunningLaunch>,
    },
    /// The workspace's machine could not be asked for its file. Nothing is confirmed from it.
    Unreadable(String),
}

/// A remote-control entry's `launch` entry that is running when the call is judged: what the start
/// finds instead of starting it, as it was started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningLaunch {
    /// The entry's name, as the file writes it.
    pub name: String,
    pub spawned: preview_servers::SpawnedCommand,
    /// Whether it was started from what the file gives the entry now. A start refuses one that
    /// was not ([`start_launch`]), so the card says so rather than describe it as what will run.
    pub as_configured: bool,
}

impl StartReading {
    /// The remote-control entries a card built from this reading confirms.
    pub fn confirmed_controls(&self) -> Vec<ControlEntryIdentity> {
        match self {
            Self::Read { list, picks, .. } => picks
                .iter()
                .filter_map(|index| ControlEntryIdentity::of(&list.servers, *index))
                .collect(),
            Self::Unreadable(_) => Vec::new(),
        }
    }
}

/// Reads what a `preview_start` of `requested` in `workspace` would start, for the run loop to
/// judge the call by: the file as the start reads it — fresh, never the pane's cached copy — and
/// the entry [`pick_entry`] picks with the registry as it is now, for `session_id`. `None` for a
/// workspace on WSL, which has no previews.
pub fn read_for_start(
    registry: &PreviewServerRegistry,
    workspace: &crate::workspace_set::ResolvedWorkspace,
    requested: &str,
    session_id: &str,
) -> Option<StartReading> {
    let (list, worktree) = match &workspace.machine {
        None => (
            Ok(configurations(Path::new(&workspace.root))),
            std::path::PathBuf::from(&workspace.root),
        ),
        Some(machine @ crate::model::RunTarget::Ssh { .. }) => {
            let remote = RemoteMachine::new(
                workspace.runner.clone(),
                crate::run_environment::env_key(Some(machine)),
                workspace.machine_label.clone(),
            )
            .with_patience(crate::preview_remote::POLL_PATIENCE);
            (
                remote_configurations_fresh(&remote, &workspace.root),
                remote.worktree_key(&workspace.root),
            )
        }
        Some(crate::model::RunTarget::Wsl { .. }) => return None,
    };
    let list = match list {
        Ok(list) => list,
        Err(error) => return Some(StartReading::Unreadable(error)),
    };
    let running_rows = preview_servers::running_for_session(
        &registry.servers_for_worktree(&worktree),
        Some(session_id),
    );
    let running: Vec<String> = running_rows
        .iter()
        .map(|server| server.server_id.clone())
        .collect();
    let order = |name: &str| registry.repeated_name_order(&worktree, name);
    let entries: Vec<EntryView<'_>> = list.servers.iter().map(EntryView::of_listed).collect();
    let malformed: Vec<&str> = list.malformed.iter().map(|entry| entry.name.as_str()).collect();
    let picks = pick_entry(
        &entries,
        &malformed,
        Some(requested),
        Some(&RepeatFacts {
            order: &order,
            running: &running,
        }),
    )
    .candidates();
    let running_launches = running_launches(registry, &list, &picks, &running_rows);
    Some(StartReading::Read {
        list,
        picks,
        running_launches,
    })
}

/// The `launch` entries of the remote-control entries among `picks` that `running` — the rows a
/// start of the conversation may reuse — has running, found the way the start finds them: by the
/// entry's name ([`preview_servers::decide_start_action`]).
fn running_launches(
    registry: &PreviewServerRegistry,
    list: &PreviewConfigurationList,
    picks: &[usize],
    running: &[PreviewServerSnapshot],
) -> Vec<RunningLaunch> {
    let mut found: Vec<RunningLaunch> = Vec::new();
    for sibling in picks
        .iter()
        .filter_map(|index| ControlEntryIdentity::of(&list.servers, *index))
        .filter_map(|identity| identity.launch)
    {
        if found
            .iter()
            .any(|known| preview_launch_config::names_match(&known.name, &sibling.name))
        {
            continue;
        }
        let Some(row) = running
            .iter()
            .find(|row| row.remote.is_none() && row.server_id.eq_ignore_ascii_case(&sibling.name))
        else {
            continue;
        };
        let Some(spawned) = registry.spawned(&row.handle) else {
            continue;
        };
        let as_configured =
            spawn_config_of(&sibling).is_some_and(|config| spawned.is_spawn_of(&config));
        found.push(RunningLaunch {
            name: sibling.name.clone(),
            spawned,
            as_configured,
        });
    }
    found
}

/// What a start of a listed entry hands the process path, before any port is chosen: the
/// listing's side of [`PreviewServerConfig::try_from`]. `None` for a port no process can have.
fn spawn_config_of(server: &PreviewConfiguredServer) -> Option<PreviewServerConfig> {
    Some(PreviewServerConfig {
        name: server.name.clone(),
        server_id: server.name.clone(),
        command: server.command.clone(),
        args: server.args.clone(),
        cwd: std::path::PathBuf::from(&server.cwd),
        port: u16::try_from(server.port).ok()?,
        env: server.env.clone(),
        auto_port: server.auto_port,
        url: server.url.clone(),
        readiness: preview_servers::Readiness::Port,
    })
}

/// One usable entry, as the pane lists it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewConfiguredServer {
    pub name: String,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub cwd: String,
    /// Still the parser's `u32`: a port extracted from a command line is not
    /// range-checked, and truncating it here would hide the mistake instead of
    /// showing it.
    pub port: u32,
    pub auto_port: Option<bool>,
    pub url: Option<String>,
    /// A remote-control entry's target: the screen the pane shows instead of a page. Such an
    /// entry has no `command` and no `url`, so it is not an attach entry either.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_control: Option<ControlTarget>,
    /// The variables the entry sets for its command. Never sent to the pane — a value may be a
    /// secret the file was given — but part of what a remote-control entry's approval card shows
    /// of its `launch` entry, and of what that approval is held to ([`ControlEntryIdentity`]).
    #[serde(skip)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewMalformedEntry {
    pub name: String,
    pub reason: String,
}

/// Everything `.mewrk/launch.json` says, including what is wrong with it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewConfigurationList {
    pub launch_json_path: String,
    pub servers: Vec<PreviewConfiguredServer>,
    pub malformed: Vec<PreviewMalformedEntry>,
    /// The full explanation of an unusable file. `None` when it is usable.
    pub problem: Option<String>,
    /// The wire reason behind `problem`.
    pub problem_reason: Option<String>,
}

/// A `.mewrk/launch.json` entry that names a url and no command: the server is
/// somebody else's, and the whole start is pointing the preview at it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewAttachment {
    /// The entry's name, numbered when the file repeats it — the id a process of it would have.
    /// Nothing answers to it but the page, so `preview_stop` and `preview_logs` refuse it.
    pub server_id: String,
    pub name: String,
    /// The parser's `u32`, unchecked, because nothing binds it: the url carries
    /// the address, and the field is reported only because the entry stated it.
    pub port: u32,
    pub url: String,
}

/// What a start produced.
///
/// Untagged so the attach form is recognised by the field that is only there for
/// it, the way the source's own receipt is: an attach has no process, and giving
/// it a snapshot with a `status` would tell the pane and `preview_list` that
/// something is running when nothing was started.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum PreviewStartOutcome {
    /// A process: spawned by this call, or already running and handed back.
    Server {
        server: PreviewServerSnapshot,
        reused: bool,
    },
    /// The attach form. Nothing was spawned and no port was reserved.
    Attached { attached: PreviewAttachment },
}

impl TryFrom<&ServerConfig> for PreviewServerConfig {
    type Error = String;

    /// The parser keeps `port` as a `u32` because `extractPortFromCommand` reads
    /// whatever digits a command line holds and the source range-checks only the
    /// explicit `port` field. Five digits are not a port, and quietly binding a
    /// different one would leave the server unreachable at the address its own
    /// command line hardcodes.
    fn try_from(config: &ServerConfig) -> Result<Self, Self::Error> {
        let port = u16::try_from(config.port).map_err(|_| {
            format!(
                "launch.json 配置「{}」的端口 {} 超出 0-65535",
                config.name, config.port
            )
        })?;
        Ok(Self {
            name: config.name.clone(),
            // The name as the file writes it; a start that finds it repeated numbers it.
            server_id: config.name.clone(),
            command: config.command.clone(),
            args: config.args.clone(),
            cwd: config.cwd.clone(),
            port,
            env: config.env.clone(),
            auto_port: config.auto_port,
            url: config.url.clone(),
            readiness: preview_servers::Readiness::Http,
        })
    }
}

/// One workspace of a conversation, as the renderer's preview pages address it: by
/// the number the model addresses it with, exactly as a terminal is opened. A page
/// belongs to one workspace; its start page reads that workspace's
/// `.mewrk/launch.json` and the servers it runs run on that workspace's machine.
#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PreviewTarget {
    /// The conversation; for the renderer's draft, the id it will materialize as.
    pub conversation_id: String,
    /// Only for the draft: the project it is aimed at.
    #[serde(default)]
    pub draft_workspace_id: Option<String>,
    /// 1-based. Absent is workspace 1 — or, where a command can span them, every workspace.
    #[serde(default)]
    pub workspace: Option<u32>,
}

/// Reads `.mewrk/launch.json` and reports what it configures.
///
/// Never fails: an absent, unreadable, or broken file is a result the pane has to
/// render, not an error it has to swallow.
pub fn configurations(workspace: &Path) -> PreviewConfigurationList {
    let discovery = LaunchConfigDiscovery::new(workspace);
    let found = discovery.discover(None);
    if let Some(warning) = preview_launch_config::discovery_warning(&found) {
        eprintln!("预览服务器配置（{}）：{warning}", workspace.display());
    }
    configuration_list(&discovery, &found, workspace)
}

/// [`configurations`] for a workspace on another machine: the file is read there,
/// and parsed here by the same parser. Only an unreachable machine is an error —
/// the file's own problems are still a result to render.
pub fn remote_configurations(
    machine: &crate::preview_remote::RemoteMachine,
    root: &str,
) -> Result<PreviewConfigurationList, String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    /// How long a read stands in for the next. The pane asks every second and a half for as long
    /// as it is open, and each read of another machine's file is a process started there; a file
    /// edited on the machine still shows within a few seconds.
    const FRESH_FOR: Duration = Duration::from_secs(5);
    type Cache = Mutex<HashMap<String, (Instant, PreviewConfigurationList)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{}\n{root}", machine.key());
    if let Some((read_at, list)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        if read_at.elapsed() < FRESH_FOR {
            return Ok(list.clone());
        }
    }
    let list = remote_configurations_fresh(machine, root)?;
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, (Instant::now(), list.clone()));
    Ok(list)
}

/// [`remote_configurations`] read from the machine now, never from the pane's few-seconds cache:
/// what a decision about a start is made from. A cached copy can predate an edit the start
/// itself will read.
pub fn remote_configurations_fresh(
    machine: &crate::preview_remote::RemoteMachine,
    root: &str,
) -> Result<PreviewConfigurationList, String> {
    let (discovery, found) = remote_discovery(machine, root)?;
    Ok(configuration_list(
        &discovery,
        &found,
        discovery.working_directory(),
    ))
}

/// Reads and parses a remote workspace's `launch.json`, keeping every entry.
fn remote_discovery(
    machine: &crate::preview_remote::RemoteMachine,
    root: &str,
) -> Result<
    (
        LaunchConfigDiscovery,
        preview_launch_config::LaunchDiscovery,
    ),
    String,
> {
    use crate::preview_remote::RemoteLaunchJsonContent;
    let read = machine.read_launch_json(root)?;
    let discovery = LaunchConfigDiscovery::new(&read.root);
    let found = match read.content {
        RemoteLaunchJsonContent::Read(source) => discovery.parse_source(&source, None),
        RemoteLaunchJsonContent::Missing => preview_launch_config::LaunchDiscovery::NotFound,
        RemoteLaunchJsonContent::Unreadable(message) => {
            preview_launch_config::LaunchDiscovery::Unreadable {
                code: "EACCES".to_owned(),
                message,
            }
        }
    };
    Ok((discovery, found))
}

fn configuration_list(
    discovery: &LaunchConfigDiscovery,
    found: &preview_launch_config::LaunchDiscovery,
    workspace: &Path,
) -> PreviewConfigurationList {
    let config = found.config();
    PreviewConfigurationList {
        launch_json_path: discovery.launch_json_path().to_string_lossy().into_owned(),
        servers: config
            .map(|config| config.servers.iter().map(configured_server).collect())
            .unwrap_or_default(),
        malformed: found
            .malformed()
            .iter()
            .map(|entry| PreviewMalformedEntry {
                name: entry.name.clone(),
                reason: entry.reason.clone(),
            })
            .collect(),
        problem: preview_launch_config::discovery_model_message(found, workspace),
        problem_reason: preview_launch_config::discovery_deny_reason(found)
            .map(|reason| reason.as_str().to_owned()),
    }
}

/// One parsed entry as the pane lists it — and as a start's approval is checked against it.
fn configured_server(server: &ServerConfig) -> PreviewConfiguredServer {
    PreviewConfiguredServer {
        name: server.name.clone(),
        command: server.command.clone(),
        args: server.args.clone(),
        cwd: server.cwd.to_string_lossy().into_owned(),
        port: server.port,
        auto_port: server.auto_port,
        url: server.url.clone(),
        remote_control: server.control.clone(),
        env: server.env.clone(),
    }
}

/// Where a start runs what it starts and files what it registers: a workspace on this computer,
/// or one on an SSH machine.
#[derive(Clone, Copy)]
enum Site<'a> {
    /// The workspace's directory is the registry key, and where the file's paths resolve.
    Local(&'a Path),
    /// `worktree` is the registry key ([`RemoteMachine::worktree_key`]); `display_root` the root
    /// as the machine resolved it, which snapshots report as `cwd`.
    Remote {
        machine: &'a RemoteMachine,
        worktree: &'a Path,
        display_root: &'a str,
    },
}

impl Site<'_> {
    fn worktree(&self) -> &Path {
        match self {
            Self::Local(workspace) => workspace,
            Self::Remote { worktree, .. } => worktree,
        }
    }

    fn display_cwd(&self) -> Option<String> {
        match self {
            Self::Local(_) => None,
            Self::Remote { display_root, .. } => Some((*display_root).to_owned()),
        }
    }

    fn machine(&self) -> Option<&RemoteMachine> {
        match self {
            Self::Local(_) => None,
            Self::Remote { machine, .. } => Some(machine),
        }
    }
}

/// Starts the server `name` addresses, hands back the one already answering it, or
/// resolves the attach entry the caller then points the preview at. A remote-control entry
/// connects its screen instead ([`start_control`]); `environment` is what its place is
/// resolved against.
///
/// Blocks for up to the registry's startup gate — or, for a remote-control entry, up to
/// [`CONTROL_SETTLE_WAIT`] — and spawns detached threads, so it must run on a blocking worker
/// rather than a runtime thread.
pub fn start(
    registry: &PreviewServerRegistry,
    workspace: &Path,
    requested_name: Option<&str>,
    session_id: Option<&str>,
    environment: &ControlEnvironment,
) -> Result<PreviewStartOutcome, String> {
    start_with_approval(
        registry,
        workspace,
        requested_name,
        session_id,
        environment,
        &ControlApproval::Person,
    )
}

/// [`start`] for a model's call: a remote-control entry is connected only when `approval` admits
/// it — the entry the person confirmed on the call's card, exactly as it was shown.
pub fn start_with_approval(
    registry: &PreviewServerRegistry,
    workspace: &Path,
    requested_name: Option<&str>,
    session_id: Option<&str>,
    environment: &ControlEnvironment,
    approval: &ControlApproval,
) -> Result<PreviewStartOutcome, String> {
    // A renderer sends an empty box as an empty string. Normalising once here is
    // what keeps `select_server` (which would look for a server literally named
    // "") and `decide_start_action` (which already treats "" as unnamed) agreeing
    // on whether the caller named anything.
    let requested_name = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty());

    // Deliberately a nameless discovery: `select_server` needs every sibling entry
    // and every malformed one to explain a name it cannot find.
    let discovery = LaunchConfigDiscovery::new(workspace).discover(None);
    let configured_server_count = discovery.config().map_or(0, |config| config.servers.len());
    let running =
        preview_servers::running_for_session(&registry.servers_for_worktree(workspace), session_id);
    let (selected, server_id) = address_entry(
        registry,
        workspace,
        &discovery,
        workspace,
        requested_name,
        &running,
        approval,
    )?;
    let site = Site::Local(workspace);

    // A remote-control entry names a screen, not a process: it never reaches the registry's
    // process start, its port is the service's and never reserved, and it has its own reuse and
    // its own cap. Decided first, like the attach form, for the same reason.
    if let Some(target) = &selected.control {
        return start_control(
            registry,
            site,
            &discovery,
            &selected,
            target,
            server_id,
            configured_server_count,
            session_id,
            environment,
        );
    }

    // Decided where the source decides it: before reuse, before capacity, and
    // before any port is reserved. An entry that names a url and no command
    // describes a server this application never started, so there is no process
    // to reuse and nothing to bind — the preview is simply pointed at the url.
    //
    // The two branches partition every entry the parser can produce, which only
    // keeps a command-less entry when it has a url. One with neither falls through
    // to the registry's own refusal, which is the right message for it.
    if let (None, Some(url)) = (selected.command.as_deref(), selected.url.as_deref()) {
        return attachment(server_id, &selected.name, url, selected.port)
            .map(|attached| PreviewStartOutcome::Attached { attached });
    }

    let mut config = PreviewServerConfig::try_from(&selected)?;
    config.server_id = server_id;
    start_process(
        registry,
        site,
        config,
        requested_name,
        configured_server_count,
        session_id,
    )
}

/// The process path of a start, after the entry is resolved: reuse, capacity, the port, the
/// spawn — here, or on the workspace's machine through its agent.
fn start_process(
    registry: &PreviewServerRegistry,
    site: Site<'_>,
    mut config: PreviewServerConfig,
    requested_name: Option<&str>,
    configured_server_count: usize,
    session_id: Option<&str>,
) -> Result<PreviewStartOutcome, String> {
    let turn = registry.start_turn(site.machine().map(RemoteMachine::key));
    let mut turn = Some(turn.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    // Read in this start's turn: what the caller read before it may have gained this very server.
    let running = preview_servers::running_for_session(
        &registry.servers_for_worktree(site.worktree()),
        session_id,
    );
    if let Some(reused) = reuse(
        registry,
        &running,
        &config,
        requested_name,
        configured_server_count,
    )? {
        return Ok(reused);
    }
    registry
        .ensure_capacity(site.worktree(), session_id)
        .map_err(|error| error.message)?;
    let port = match site {
        Site::Local(_) => registry.select_port(config.port, config.auto_port, session_id),
        Site::Remote { machine, .. } => {
            registry.select_remote_port(machine, config.port, config.auto_port, session_id)
        }
    }
    .map_err(|error| error.message)?;
    if port != config.port {
        config.args = preview_servers::rewrite_port_arguments(&config.args, port);
        config.port = port;
    }
    let (machine, worktree, display_root) = match site {
        Site::Local(workspace) => {
            return registry
                .start_with_retries(workspace, &config, session_id, turn)
                .map(|server| PreviewStartOutcome::Server {
                    server,
                    reused: false,
                })
                .map_err(|error| error.message);
        }
        Site::Remote {
            machine,
            worktree,
            display_root,
        } => (machine, worktree, display_root),
    };
    let host = crate::preview_remote::host(machine);
    let mut last = None;
    for attempt in 1..=preview_servers::MAX_SPAWN_ATTEMPTS {
        match registry.start_remote(
            worktree,
            display_root,
            host.clone(),
            &config,
            session_id,
            &mut turn,
        ) {
            Ok(server) => {
                return Ok(PreviewStartOutcome::Server {
                    server,
                    reused: false,
                })
            }
            Err(error)
                if !error.is_retryable() || attempt == preview_servers::MAX_SPAWN_ATTEMPTS =>
            {
                return Err(error.message)
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.map_or_else(
        || "Failed to start preview server after retries".to_owned(),
        |error| error.message,
    ))
}

/// Connects a remote-control entry's screen (`docs/remote-control.md` §2.3–§2.4, §3.4).
///
/// In order: this conversation's live row for the server id is handed back; the place is
/// resolved; VNC on a machine registered at this computer's own address is refused; the local
/// VNC rule is applied (a VNC server on this computer is connected to only when a `launch` entry
/// started it, on exactly that port); the device lock and the cap are
/// checked before anything starts; the `launch` entry is started — or found running — through
/// the ordinary process path, and given a moment to open its port
/// ([`ControlEnvironment::launch_port_wait`]); the row is registered and
/// the session started; and the session is given [`CONTROL_SETTLE_WAIT`] to be ready, to ask
/// for a sign-in or to fail. A failed session's row is kept, with its log, and handed back like
/// any other: the receipt says why it failed.
#[allow(clippy::too_many_arguments)]
fn start_control(
    registry: &PreviewServerRegistry,
    site: Site<'_>,
    discovery: &LaunchDiscovery,
    entry: &ServerConfig,
    target: &ControlTarget,
    server_id: String,
    configured_server_count: usize,
    session_id: Option<&str>,
    environment: &ControlEnvironment,
) -> Result<PreviewStartOutcome, String> {
    let worktree = site.worktree();
    if let Some(server) = registry.live_control(worktree, &server_id, session_id) {
        return Ok(PreviewStartOutcome::Server {
            server,
            reused: true,
        });
    }
    let name = preview_launch_config::sanitize_for_message(&entry.name);
    let place = environment.resolve(target, site.machine())?;
    // A machine registered at this computer's own address is dialled through its agent, as the
    // card names it, but it may be this computer under another name, and a VNC server there may
    // then be the user's own desktop. The local VNC rule cannot be applied to it — whether a
    // `launch` entry owns the port is asked of the machine's processes, not this computer's — so
    // VNC there is refused: VNC to this computer goes through an entry on a workspace here.
    if target.protocol == Protocol::Vnc && !place.is_this_computer() && place.may_be_this_computer()
    {
        return Err(format!(
            "\"{name}\" is a VNC screen on the SSH machine \"{}\", which is registered at an address of this computer ({}), so that VNC server may be this computer's own desktop. Mewrk does not connect to VNC on such a machine. To control a virtual machine on this computer over VNC, put the entry in a workspace on this computer, with \"launch\" naming the entry that starts the virtual machine. adb and wda screens on that machine are not affected.",
            preview_launch_config::sanitize_for_message(&place.label()),
            preview_launch_config::sanitize_for_message(
                place.this_computer_address().unwrap_or_default()
            )
        ));
    }
    let local_vnc = place.is_this_computer() && target.protocol == Protocol::Vnc;
    if local_vnc && target.launch.is_none() {
        return Err(format!(
            "\"{name}\" is a VNC screen on this computer, and a VNC server on this computer may be the user's own desktop. Mewrk connects to one only when the entry's \"launch\" names the entry that starts it (a virtual machine with \"-vnc 127.0.0.1:1\", for example), so the server is known to be that entry's. To control a virtual machine started on this computer, put the name of the entry that starts it in \"launch\"."
        ));
    }
    let sibling = match target.launch.as_deref() {
        None => None,
        Some(launch) => {
            let workspace_place = environment.workspace_place(site.machine());
            if place.key() != workspace_place.key() {
                return Err(format!(
                    "\"{name}\" launches \"{}\" first, which runs where the workspace is ({}), but its screen is on {}. Remove \"launch\", or \"thisComputer\".",
                    preview_launch_config::sanitize_for_message(launch),
                    workspace_place.label(),
                    place.label()
                ));
            }
            let sibling = launch_entry(discovery, &name, launch)?;
            if local_vnc {
                local_vnc_launch_rule(&name, sibling, target.port)?;
            }
            Some(sibling)
        }
    };
    let lock_key = device_lock_key(target, &place.key());
    registry.admit_control(worktree, &lock_key, session_id)?;

    let launched = match sibling {
        Some(sibling) => Some(start_launch(
            registry,
            site,
            &name,
            sibling,
            configured_server_count,
            session_id,
            environment.launch_port_wait,
        )?),
        None => None,
    };
    let preflight: Option<Preflight> = match (&launched, local_vnc) {
        (Some(launched), true) => {
            Some(registry.listener_owner_preflight(&launched.handle, target.port))
        }
        _ => None,
    };
    let started = registry.start_control(
        worktree,
        ControlRowSpec {
            name: entry.name.clone(),
            server_id: server_id.clone(),
            session_id: session_id.map(str::to_owned),
            display_cwd: site.display_cwd(),
            lock_key,
            launch: launched.clone(),
        },
        |hooks| {
            RemoteControlSession::start(SessionSpec {
                name: entry.name.clone(),
                server_id: server_id.clone(),
                target: target.clone(),
                connector: place.connector(),
                preflight,
                on_change: hooks.on_change,
                log: hooks.log,
                connect: environment.connect.clone(),
            })
        },
    );
    let server = match started {
        Ok(ControlStart::Started(server)) => server,
        Ok(ControlStart::Reused(server)) => {
            return Ok(PreviewStartOutcome::Server {
                server,
                reused: true,
            })
        }
        Err(error) => {
            // An entry this start launched for a screen it then could not connect would run on
            // with nothing to show for it.
            if let Some(launched) = launched.filter(|launched| !launched.was_running) {
                registry.stop(&launched.handle);
            }
            return Err(error);
        }
    };
    if let Some(session) = registry.control_session(&server.handle) {
        session.wait_settled(CONTROL_SETTLE_WAIT);
        registry.refresh_control(&server.handle);
    }
    Ok(PreviewStartOutcome::Server {
        server: registry.get(&server.handle).unwrap_or(server),
        reused: false,
    })
}

/// The entry a remote-control entry's `launch` names: the one entry of the file with that name,
/// which runs a command. Refused, in words the model can fix the file with, when it is missing
/// (with the reason, when the file has it as a malformed entry), repeated, a remote-control
/// entry, or an attach entry.
fn launch_entry<'a>(
    discovery: &'a LaunchDiscovery,
    name: &str,
    launch: &str,
) -> Result<&'a ServerConfig, String> {
    let shown = preview_launch_config::sanitize_for_message(launch);
    let servers = discovery
        .config()
        .map(|config| config.servers.as_slice())
        .unwrap_or_default();
    let matching: Vec<&ServerConfig> = servers
        .iter()
        .filter(|server| preview_launch_config::names_match(&server.name, launch))
        .collect();
    let needs = "\"launch\" must name an entry with a command (\"runtimeExecutable\" or \"program\") that starts the device or the virtual machine";
    match matching.as_slice() {
        [] => match discovery
            .malformed()
            .iter()
            .find(|malformed| preview_launch_config::names_match(&malformed.name, launch))
        {
            Some(malformed) => Err(format!(
                "\"{name}\" launches \"{shown}\" first, but that entry of .mewrk/launch.json could not be used: {}",
                malformed.reason
            )),
            None => Err(format!(
                "\"{name}\" launches \"{shown}\" first, but .mewrk/launch.json has no entry named \"{shown}\"; {needs}."
            )),
        },
        [sibling] if sibling.control.is_some() => Err(format!(
            "\"{name}\" launches \"{shown}\" first, but \"{shown}\" is a remote-control entry itself; {needs}."
        )),
        [sibling] if sibling.command.is_none() => Err(format!(
            "\"{name}\" launches \"{shown}\" first, but \"{shown}\" has no command to run (it attaches to a url); {needs}."
        )),
        [sibling] => Ok(sibling),
        several => Err(format!(
            "\"{name}\" launches \"{shown}\" first, but {} entries of .mewrk/launch.json are named \"{shown}\"; give the entry to start a name of its own.",
            several.len()
        )),
    }
}

/// The local VNC rule's demands on the `launch` entry (`docs/remote-control.md` §2.4): it opens
/// exactly the screen's port, and that port is never moved — so it is free when the entry starts
/// (the dev server's own port check), and whatever listens there later can be checked against
/// the entry's process tree.
fn local_vnc_launch_rule(name: &str, sibling: &ServerConfig, port: u16) -> Result<(), String> {
    let launch = preview_launch_config::sanitize_for_message(&sibling.name);
    if sibling.auto_port != Some(false) {
        return Err(format!(
            "\"{name}\" connects to a VNC server on this computer that \"{launch}\" has to open on port {port} itself. Set \"autoPort\": false on \"{launch}\", so its port is never moved: a VNC display number such as QEMU's \"-vnc 127.0.0.1:1\" cannot follow a moved port."
        ));
    }
    if sibling.port != u32::from(port) {
        return Err(format!(
            "\"{name}\" connects to port {port} on this computer, but \"{launch}\" is configured for port {}. Set \"port\" to the same number on both entries, so the port \"{launch}\" opens is the one Mewrk checks and connects to.",
            sibling.port
        ));
    }
    Ok(())
}

/// Starts a remote-control entry's `launch` entry through the ordinary process path, as a start
/// of that entry by name would — or finds it running — and, when it was just started, gives it
/// `wait` to open its port.
///
/// A running one is handed over only when it was started from exactly what the file gives the
/// entry now ([`preview_servers::SpawnedCommand::is_spawn_of`]). The approval card of a model's
/// start shows the file's command line, and the start connects only an entry identical to the one
/// confirmed ([`ControlEntryIdentity::launch`]), so that check is what makes the process on the
/// card the process that runs. Reuse alone goes by the name: a process started earlier under the
/// same name from another command line — one that shares the user's real desktop over VNC, say,
/// before the file was rewritten to start a virtual machine — would otherwise be handed over as if
/// it were the one the person confirmed. A start from the pane is held to the file the same way.
#[allow(clippy::too_many_arguments)]
fn start_launch(
    registry: &PreviewServerRegistry,
    site: Site<'_>,
    name: &str,
    sibling: &ServerConfig,
    configured_server_count: usize,
    session_id: Option<&str>,
    wait: Duration,
) -> Result<ControlLaunch, String> {
    let shown = preview_launch_config::sanitize_for_message(&sibling.name);
    let mut config = PreviewServerConfig::try_from(sibling)?;
    config.server_id = sibling.name.clone();
    // What it opens is a screen's service, not a page: it is up once its port takes a connection.
    config.readiness = preview_servers::Readiness::Port;
    let configured = config.clone();
    let outcome = start_process(
        registry,
        site,
        config,
        Some(&sibling.name),
        configured_server_count,
        session_id,
    )
    .map_err(|error| {
        format!("\"{name}\" launches \"{shown}\" first, and it did not start: {error}")
    })?;
    let PreviewStartOutcome::Server { server, reused } = outcome else {
        return Err(format!(
            "\"{name}\" launches \"{shown}\" first, but \"{shown}\" did not start a process."
        ));
    };
    if reused
        && !registry
            .spawned(&server.handle)
            .is_some_and(|spawned| spawned.is_spawn_of(&configured))
    {
        let running_id = preview_launch_config::sanitize_for_message(&server.server_id);
        return Err(format!(
            "\"{name}\" launches \"{shown}\" first, but the running \"{running_id}\" was started from a different command line than .mewrk/launch.json now gives \"{shown}\", so it is not the process this start is for. Nothing was connected. Stop it first (preview_stop with serverId \"{running_id}\"), then call preview_start again."
        ));
    }
    if !reused && server.port != 0 {
        let deadline = Instant::now() + wait;
        loop {
            let answers = match site {
                Site::Local(_) => preview_servers::port_answers_here(server.port),
                Site::Remote { machine, .. } => machine
                    .port_state(server.port)
                    .is_ok_and(|(_, listening)| listening),
            };
            if answers || Instant::now() >= deadline {
                break;
            }
            if registry.get(&server.handle).is_none() {
                return Err(format!(
                    "\"{name}\" launches \"{shown}\" first, and \"{shown}\" exited before it opened port {}.",
                    server.port
                ));
            }
            std::thread::sleep(LAUNCH_PORT_POLL);
        }
    }
    Ok(ControlLaunch {
        handle: server.handle,
        server_id: server.server_id,
        was_running: reused,
    })
}

/// The entry a start addresses, and the server id it answers to.
///
/// Which entry is [`pick_entry`]'s rule, with the registry's facts: `running` is the caller's
/// own, narrowed by [`preview_servers::running_for_session`]. A name the file repeats gets its
/// number here, the first time one of its entries is started — so a start refused below numbers
/// nothing.
///
/// A remote-control entry is addressed only when `approval` admits it: a model's call was
/// confirmed for the entry the run loop read when it judged the call, and a file edited since,
/// or a registry that moved the name onto another entry, must not connect a screen nobody
/// confirmed ([`ControlApproval`]).
///
/// `worktree` is the registry key, `working_directory` the directory the file's paths resolve
/// against — the same directory for a workspace on this computer, not for one on another machine.
fn address_entry(
    registry: &PreviewServerRegistry,
    worktree: &Path,
    discovery: &preview_launch_config::LaunchDiscovery,
    working_directory: &Path,
    requested_name: Option<&str>,
    running: &[PreviewServerSnapshot],
    approval: &ControlApproval,
) -> Result<(ServerConfig, String), String> {
    let servers = discovery
        .config()
        .map(|config| config.servers.as_slice())
        .unwrap_or_default();
    let entries: Vec<EntryView<'_>> = servers.iter().map(EntryView::of_config).collect();
    let malformed: Vec<&str> = discovery
        .malformed()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let running_ids: Vec<String> = running
        .iter()
        .map(|server| server.server_id.clone())
        .collect();
    let order = |name: &str| registry.repeated_name_order(worktree, name);
    let facts = RepeatFacts {
        order: &order,
        running: &running_ids,
    };
    let (index, numbering) = match pick_entry(&entries, &malformed, requested_name, Some(&facts)) {
        EntryPick::One { index, numbering } => (index, numbering),
        EntryPick::NotStarted { first, siblings } => {
            let name = preview_launch_config::sanitize_for_message(&servers[first].name);
            return Err(format!(
                "\"{}\" has not been started. .mewrk/launch.json has {siblings} servers named \"{name}\"; preview_start with name \"{name}\" starts the next one that is not running, and they are numbered in the order they are first started.",
                preview_launch_config::sanitize_for_message(
                    requested_name.map(str::trim).unwrap_or_default()
                ),
            ));
        }
        // The parser's refusal is the diagnosis, in its words.
        EntryPick::Unselected | EntryPick::AnyOf(_) => {
            return Err(
                match preview_launch_config::select_server(
                    discovery,
                    working_directory,
                    requested_name,
                ) {
                    Err(denial) => denial.message,
                    Ok(selected) => format!(
                        "\"{}\" could not be told apart from the other entries of .mewrk/launch.json. Pass the name of the entry to start.",
                        preview_launch_config::sanitize_for_message(&selected.name)
                    ),
                },
            )
        }
    };
    let entry = &servers[index];
    let server_id = |number: u32| numbered_server_id(&entry.name, number);
    if entry.control.is_some() {
        let listed: Vec<PreviewConfiguredServer> = servers.iter().map(configured_server).collect();
        let identity = ControlEntryIdentity::of(&listed, index)
            .expect("a remote-control entry has an identity");
        let shown = match numbering {
            Numbering::Known(number) => server_id(number),
            Numbering::Unique | Numbering::Occurrence { .. } => entry.name.clone(),
        };
        approval.admits(&identity, &shown)?;
    }
    let id = match numbering {
        Numbering::Unique => entry.name.clone(),
        Numbering::Known(number) => server_id(number),
        Numbering::Occurrence { key, occurrence } => server_id(registry.number_repeated_name(
            worktree,
            &servers[key].name,
            occurrence,
        )),
    };
    Ok((entry.clone(), id))
}

/// The running server a start hands back instead of spawning another, if there is one.
///
/// The decision is made from a snapshot. A server that died in between has to be
/// reported as gone: respawning it silently would answer a "reuse" with a different
/// process than the one the caller was told about.
fn reuse(
    registry: &PreviewServerRegistry,
    running: &[PreviewServerSnapshot],
    config: &PreviewServerConfig,
    requested_name: Option<&str>,
    configured_server_count: usize,
) -> Result<Option<PreviewStartOutcome>, String> {
    let PreviewStartAction::Reuse { server, .. } = preview_servers::decide_start_action(
        running,
        &config.server_id,
        config.port,
        requested_name,
        configured_server_count,
    ) else {
        return Ok(None);
    };
    match registry.get(&server.handle).filter(|live| {
        matches!(
            live.status,
            PreviewServerStatus::Running | PreviewServerStatus::Starting
        )
    }) {
        Some(server) => Ok(Some(PreviewStartOutcome::Server {
            server,
            reused: true,
        })),
        None => Err(preview_servers::dead_reuse_refusal_message(
            &server.server_id,
        )),
    }
}

/// [`start`] for a workspace on another machine, through its agent.
///
/// The same order, the same decisions and the same messages; what changes is where
/// each question is asked. The file is read there, the port is probed there, and
/// the server runs there. An attach entry's url is handed back as the file wrote
/// it: it names that machine's `localhost`, which is where a page of that
/// workspace reaches.
///
/// `root` is the workspace directory as the conversation records it, which is also
/// what the registry files the machine's servers under, so a listing never has to
/// ask the machine where its root resolves to.
pub fn start_remote(
    registry: &PreviewServerRegistry,
    machine: &RemoteMachine,
    root: &str,
    requested_name: Option<&str>,
    session_id: Option<&str>,
    environment: &ControlEnvironment,
) -> Result<PreviewStartOutcome, String> {
    start_remote_with_approval(
        registry,
        machine,
        root,
        requested_name,
        session_id,
        environment,
        &ControlApproval::Person,
    )
}

/// [`start_remote`] for a model's call; see [`start_with_approval`].
pub fn start_remote_with_approval(
    registry: &PreviewServerRegistry,
    machine: &RemoteMachine,
    root: &str,
    requested_name: Option<&str>,
    session_id: Option<&str>,
    environment: &ControlEnvironment,
    approval: &ControlApproval,
) -> Result<PreviewStartOutcome, String> {
    let requested_name = requested_name
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let (discovery, found) = remote_discovery(machine, root)?;
    let resolved_root = discovery.working_directory().to_path_buf();
    let configured_server_count = found.config().map_or(0, |config| config.servers.len());
    let worktree = machine.worktree_key(root);
    let running =
        preview_servers::running_for_session(&registry.servers_for_worktree(&worktree), session_id);
    let (selected, server_id) = address_entry(
        registry,
        &worktree,
        &found,
        &resolved_root,
        requested_name,
        &running,
        approval,
    )?;
    let display_root = resolved_root.to_string_lossy().into_owned();
    let site = Site::Remote {
        machine,
        worktree: &worktree,
        display_root: &display_root,
    };
    if let Some(target) = &selected.control {
        return start_control(
            registry,
            site,
            &found,
            &selected,
            target,
            server_id,
            configured_server_count,
            session_id,
            environment,
        );
    }
    if let (None, Some(url)) = (selected.command.as_deref(), selected.url.as_deref()) {
        return attachment(server_id, &selected.name, url, selected.port)
            .map(|attached| PreviewStartOutcome::Attached { attached });
    }

    let mut config = PreviewServerConfig::try_from(&selected)?;
    config.server_id = server_id;
    start_process(
        registry,
        site,
        config,
        requested_name,
        configured_server_count,
        session_id,
    )
}

/// The attach form's own checks, applied to an entry the parser already accepted.
///
/// The localhost rule is re-stated rather than assumed: it is the reason a config
/// file cannot aim the preview at an arbitrary local endpoint, and this is the
/// second module that would have to keep it. The source words the refusal as the
/// boundary violation it would be, because a file that reaches here has already
/// passed [`preview_launch_config::validate_url`].
fn attachment(
    server_id: String,
    name: &str,
    url: &str,
    port: u32,
) -> Result<PreviewAttachment, String> {
    if preview_launch_config::is_localhost_url(url)
        && !preview_launch_config::is_bare_localhost_origin(url)
    {
        return Err(format!(
            "\"{}\" has a localhost \"url\" with a path or query, which this build should have rejected at config parsing. Change \"url\" to the server's origin (for example \"https://localhost:8443/\") and report this message if the config file already looks correct.",
            preview_launch_config::sanitize_for_message(name)
        ));
    }
    Ok(PreviewAttachment {
        server_id,
        name: name.to_owned(),
        port,
        url: url.to_owned(),
    })
}

/// One server's buffered output, filtered and tail-sliced. `handle` is the registry's.
pub fn logs(registry: &PreviewServerRegistry, handle: &str, query: &PreviewLogQuery) -> String {
    if registry.get(handle).is_none() {
        return NO_SERVER_FOR_LOGS.to_owned();
    }
    preview_servers::render_preview_logs(&registry.logs(handle), query)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::*;

    fn entry(port: u32) -> ServerConfig {
        ServerConfig {
            name: "dev".to_owned(),
            command: Some("npm".to_owned()),
            args: vec!["run".to_owned(), "dev".to_owned()],
            cwd: PathBuf::from("/project"),
            port,
            env: BTreeMap::new(),
            auto_port: None,
            url: None,
            control: None,
        }
    }

    #[test]
    fn a_parsed_entry_becomes_a_registry_configuration() {
        let converted = PreviewServerConfig::try_from(&entry(5173)).unwrap();

        assert_eq!(converted.name, "dev");
        assert_eq!(converted.command.as_deref(), Some("npm"));
        assert_eq!(converted.args, vec!["run".to_owned(), "dev".to_owned()]);
        assert_eq!(converted.cwd, PathBuf::from("/project"));
        assert_eq!(converted.port, 5173);
        assert_eq!(converted.auto_port, None);
    }

    #[test]
    fn a_port_wider_than_a_port_is_refused_rather_than_truncated() {
        let error = PreviewServerConfig::try_from(&entry(99999)).unwrap_err();

        assert!(error.contains("99999"), "{error}");
        assert!(error.contains("dev"), "{error}");
        // Truncation is the failure this guards: 99999 & 0xFFFF is 34463.
        assert!(!error.contains("34463"), "{error}");
    }

    #[test]
    fn a_missing_launch_json_is_a_reported_problem_not_an_error() {
        let workspace = tempfile::tempdir().unwrap();

        let listed = configurations(workspace.path());

        assert!(listed.servers.is_empty());
        assert!(listed.malformed.is_empty());
        assert_eq!(
            listed.problem_reason.as_deref(),
            Some("launch_config_missing")
        );
        assert!(listed
            .problem
            .unwrap()
            .starts_with("No .mewrk/launch.json found."));
        assert!(listed.launch_json_path.ends_with("launch.json"));
    }

    #[test]
    fn a_usable_file_lists_its_servers() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{
  "version": "0.0.1",
  "configurations": [
    { "name": "web", "runtimeExecutable": "npm", "runtimeArgs": ["run", "dev"], "port": 5173 }
  ]
}"#,
        );

        let listed = configurations(workspace.path());

        assert_eq!(listed.servers.len(), 1);
        assert_eq!(listed.servers[0].name, "web");
        assert_eq!(listed.servers[0].port, 5173);
        assert_eq!(listed.servers[0].command.as_deref(), Some("npm"));
        assert_eq!(listed.problem, None);
        assert_eq!(listed.problem_reason, None);
    }

    #[test]
    fn logs_for_an_unknown_server_name_the_tool_that_supplies_the_id() {
        let registry = PreviewServerRegistry::default();

        assert_eq!(
            logs(&registry, "missing", &PreviewLogQuery::default()),
            NO_SERVER_FOR_LOGS
        );
    }

    #[test]
    fn starting_without_a_launch_json_reports_the_configs_own_refusal() {
        let workspace = tempfile::tempdir().unwrap();
        let registry = PreviewServerRegistry::default();

        let error = start(
            &registry,
            workspace.path(),
            Some("  "),
            None,
            &ControlEnvironment::this_computer(),
        )
        .unwrap_err();

        assert!(error.starts_with("No .mewrk/launch.json found."), "{error}");
        assert!(registry.servers().is_empty());
    }

    /// The whole chain the format notes promise: a `url` with no command parses,
    /// resolves, and starts — by attaching, without a process behind it.
    #[test]
    fn an_attach_entry_starts_by_resolving_to_its_url_and_nothing_else() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{
  "configurations": [
    { "name": "docs", "url": "https://example.com/docs" }
  ]
}"#,
        );
        let registry = PreviewServerRegistry::default();

        let listed = configurations(workspace.path());
        assert_eq!(listed.servers[0].command, None);
        assert_eq!(
            listed.servers[0].url.as_deref(),
            Some("https://example.com/docs")
        );

        let outcome = start(
            &registry,
            workspace.path(),
            Some("docs"),
            Some("chat-1"),
            &ControlEnvironment::this_computer(),
        )
        .unwrap();

        assert_eq!(
            outcome,
            PreviewStartOutcome::Attached {
                attached: PreviewAttachment {
                    server_id: "docs".to_owned(),
                    name: "docs".to_owned(),
                    // Non-localhost, so the entry states no port and none is invented.
                    port: 0,
                    url: "https://example.com/docs".to_owned(),
                }
            }
        );
        // Decided before capacity and before the port: nothing was reserved, and
        // above all nothing was spawned.
        assert!(registry.servers().is_empty());
    }

    /// The receipt's id is the entry's name, like a process's, but no process answers to
    /// it, and every id-taking preview surface has to keep answering that way.
    #[test]
    fn an_attach_receipt_answers_to_the_entry_name_and_no_process() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[{"name":"api","url":"http://localhost:8443"}]}"#,
        );
        let registry = PreviewServerRegistry::default();

        let PreviewStartOutcome::Attached { attached } = start(
            &registry,
            workspace.path(),
            Some("api"),
            Some("chat-2"),
            &ControlEnvironment::this_computer(),
        )
        .unwrap() else {
            panic!("a url with no command is an attach");
        };

        // A localhost url does supply the port, so the entry reports where it points.
        assert_eq!(attached.port, 8443);
        assert_eq!(attached.server_id, "api");
        let listed = configurations(workspace.path());
        assert!(is_attach_entry(&listed, "api"));
        assert!(is_attach_entry(&listed, "API"));
        // Only a repeated name is ever numbered.
        assert!(!is_attach_entry(&listed, "api-1"));
        assert!(!is_attach_entry(&listed, "web"));
        assert_eq!(
            logs(&registry, &attached.server_id, &PreviewLogQuery::default()),
            NO_SERVER_FOR_LOGS
        );
    }

    /// A remote-control entry names a screen: no command to spawn and no url to attach to. It
    /// reaches the pane with its target and is not an attach entry, whatever id is asked for.
    #[test]
    fn a_remote_control_entry_is_listed_with_its_target_and_is_not_an_attach_entry() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"docs","url":"https://example.com/docs"},
  {"name":"web","runtimeExecutable":"npm","port":5173},
  {"name":"phone","type":"remote-control","protocol":"adb","device":"emulator-5554","machine":"mac","readOnly":true}
]}"#,
        );

        let listed = configurations(workspace.path());

        assert_eq!(listed.servers.len(), 3);
        assert!(listed.malformed.is_empty());
        let phone = &listed.servers[2];
        assert_eq!(
            (phone.command.as_deref(), phone.url.as_deref()),
            (None, None)
        );
        assert!(phone.args.is_empty());
        assert_eq!(phone.port, 5037);
        let target = phone.remote_control.as_ref().unwrap();
        assert_eq!(
            target.protocol,
            crate::remote_control::target::Protocol::Adb
        );
        assert_eq!(target.device.as_deref(), Some("emulator-5554"));
        assert!(listed.servers[..2]
            .iter()
            .all(|server| server.remote_control.is_none()));

        assert!(is_attach_entry(&listed, "docs"));
        assert!(!is_attach_entry(&listed, "phone"));
        assert!(!is_attach_entry(&listed, "PHONE"));
        assert!(!is_attach_entry(&listed, "phone-1"));
        assert!(!is_attach_entry(&listed, "web"));

        // On the wire only the remote-control entry carries the field.
        let wire = listed
            .servers
            .iter()
            .map(|server| serde_json::to_value(server).unwrap())
            .collect::<Vec<_>>();
        assert!(wire[0].get("remoteControl").is_none());
        assert!(wire[1].get("remoteControl").is_none());
        assert_eq!(
            wire[2]["remoteControl"],
            serde_json::json!({
                "protocol": "adb",
                "place": { "kind": "machine", "name": "mac" },
                "port": 5037,
                "device": "emulator-5554",
                "username": null,
                "launch": null,
                "readOnly": true,
            })
        );
    }

    /// A repeated name numbers an attach entry once and moves on, because an attach has nothing
    /// that could be running. A remote-control entry is taken the way a dev server is: while its
    /// server id is running.
    #[test]
    fn a_repeated_remote_control_name_is_taken_while_running_not_once_numbered() {
        let working_directory = Path::new("/project");
        let discovery = LaunchConfigDiscovery::new(working_directory).parse_source(
            r#"{"configurations":[
  {"name":"screen","type":"remote-control","protocol":"adb"},
  {"name":"docs","url":"https://example.com/a"},
  {"name":"Docs","url":"https://example.com/b"},
  {"name":"Screen","type":"remote-control","protocol":"vnc","port":5901}
]}"#,
            None,
        );
        let registry = PreviewServerRegistry::default();
        let address = |name: &str, running: &[PreviewServerSnapshot]| {
            let (entry, server_id) = address_entry(
                &registry,
                working_directory,
                &discovery,
                working_directory,
                Some(name),
                running,
                &ControlApproval::Person,
            )
            .unwrap();
            (
                server_id,
                entry.control.map(|control| control.protocol.as_str()),
            )
        };
        let running = |server_id: &str| PreviewServerSnapshot {
            handle: "handle".to_owned(),
            server_id: server_id.to_owned(),
            name: "screen".to_owned(),
            port: 5037,
            status: PreviewServerStatus::Running,
            started_at: String::new(),
            cwd: "/project".to_owned(),
            session_id: None,
            machine: None,
            url: None,
            workspace: None,
            remote: None,
        };

        // Attach entries: numbered in turn, each had its turn once it was attached.
        assert_eq!(address("docs", &[]), ("docs-1".to_owned(), None));
        assert_eq!(address("docs", &[]), ("Docs-2".to_owned(), None));
        // Remote-control entries: nothing running, so the first is still the one that is free.
        assert_eq!(address("screen", &[]), ("screen-1".to_owned(), Some("adb")));
        assert_eq!(address("screen", &[]), ("screen-1".to_owned(), Some("adb")));
        // Running, it is taken and the name reaches the next.
        assert_eq!(
            address("screen", &[running("screen-1")]),
            ("Screen-2".to_owned(), Some("vnc"))
        );
        assert_eq!(
            address("screen-2", &[]),
            ("Screen-2".to_owned(), Some("vnc"))
        );
    }

    /// A file the parser would refuse can still be handed to the attach layer by a
    /// future caller, and the localhost rule is the one that must not be lost in
    /// the handover.
    #[test]
    fn attaching_refuses_a_localhost_url_that_carries_a_path() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[{"name":"admin","url":"http://localhost:9090/admin/wipe"}]}"#,
        );

        // The parser drops it first, so the entry never reaches the attach layer.
        let listed = configurations(workspace.path());
        assert!(listed.servers.is_empty());

        let refusal = attachment(
            "admin".to_owned(),
            "admin",
            "http://localhost:9090/admin/wipe",
            9090,
        )
        .unwrap_err();

        assert!(refusal.starts_with("\"admin\" has a localhost \"url\" with a path or query,"));
        assert!(refusal.contains("should have rejected at config parsing"));
        assert!(attachment("admin".to_owned(), "admin", "http://localhost:9090", 9090).is_ok());
    }

    fn attached_id(outcome: PreviewStartOutcome) -> (String, String) {
        let PreviewStartOutcome::Attached { attached } = outcome else {
            panic!("a url with no command is an attach");
        };
        (attached.server_id, attached.url)
    }

    /// A launch.json may repeat a name. Each of its entries is numbered in the order it is first
    /// started, the name alone reaches the next one not yet started, and a numbered id reaches
    /// the entry it was given to — while another name keeps its bare id.
    #[test]
    fn a_repeated_name_numbers_its_entries_in_the_order_they_are_first_started() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"docs","url":"https://example.com/a"},
  {"name":"guide","url":"https://example.com/guide"},
  {"name":"Docs","url":"https://example.com/b"}
]}"#,
        );
        let registry = PreviewServerRegistry::default();
        let start_named = |name: &str| {
            start(
                &registry,
                workspace.path(),
                Some(name),
                Some("chat-1"),
                &ControlEnvironment::this_computer(),
            )
        };

        assert_eq!(
            attached_id(start_named("guide").unwrap()),
            ("guide".to_owned(), "https://example.com/guide".to_owned())
        );
        // Not started yet, so there is nothing numbered 1 to address.
        let early = start_named("docs-1").unwrap_err();
        assert!(
            early.starts_with("\"docs-1\" has not been started."),
            "{early}"
        );
        assert!(early.contains("2 servers named \"docs\""), "{early}");

        assert_eq!(
            attached_id(start_named("docs").unwrap()),
            ("docs-1".to_owned(), "https://example.com/a".to_owned())
        );
        assert_eq!(
            attached_id(start_named("docs").unwrap()),
            ("Docs-2".to_owned(), "https://example.com/b".to_owned())
        );
        // Every entry has had its turn: the first one started answers again.
        assert_eq!(
            attached_id(start_named("docs").unwrap()),
            ("docs-1".to_owned(), "https://example.com/a".to_owned())
        );
        assert_eq!(
            attached_id(start_named("docs-2").unwrap()),
            ("Docs-2".to_owned(), "https://example.com/b".to_owned())
        );
        assert!(start_named("docs-3").is_err());
        assert!(is_attach_entry(&configurations(workspace.path()), "docs-2"));
        assert!(registry.servers().is_empty());
    }

    /// With processes behind them, the name starts whichever entry is not running, a running one
    /// is handed back only for its own id, and a stopped entry comes back under the id it had.
    #[cfg(unix)]
    #[test]
    fn a_repeated_name_starts_the_entry_that_is_not_running() {
        let workspace = tempfile::tempdir().unwrap();
        let entry = |label: &str| {
            format!(
                r#"{{"name":"dev","runtimeExecutable":"/bin/sh","runtimeArgs":["-c","sleep 30 # {label}"],"port":0,"autoPort":true}}"#
            )
        };
        write_launch_json(
            workspace.path(),
            &format!(r#"{{"configurations":[{},{}]}}"#, entry("a"), entry("b")),
        );
        let registry = PreviewServerRegistry::default();
        let start_named = |name: &str| match start(
            &registry,
            workspace.path(),
            Some(name),
            Some("chat-1"),
            &ControlEnvironment::this_computer(),
        )
        .unwrap()
        {
            PreviewStartOutcome::Server { server, reused } => (server, reused),
            PreviewStartOutcome::Attached { .. } => panic!("these entries run a command"),
        };

        let (first, reused) = start_named("dev");
        assert_eq!((first.server_id.as_str(), reused), ("dev-1", false));
        let (second, reused) = start_named("dev");
        assert_eq!((second.server_id.as_str(), reused), ("dev-2", false));
        let (again, reused) = start_named("dev-2");
        assert_eq!(
            (again.handle.as_str(), reused),
            (second.handle.as_str(), true)
        );

        assert!(registry.stop(&first.handle));
        let (restarted, reused) = start_named("dev");
        assert_eq!((restarted.server_id.as_str(), reused), ("dev-1", false));
        assert_ne!(restarted.handle, first.handle);
        registry.stop_all();
    }

    // ---- Remote-control entries ----------------------------------------------

    /// An environment whose sessions all connect a fake phone at once.
    fn fake_environment() -> ControlEnvironment {
        use crate::remote_control::fake;
        // The launch entries of these tests open no port; nothing is worth waiting for.
        ControlEnvironment {
            launch_port_wait: Duration::ZERO,
            ..ControlEnvironment::this_computer()
        }
        .with_connect(fake::ready(fake::FakeDriver::touch()))
    }

    fn server_of(outcome: PreviewStartOutcome) -> (PreviewServerSnapshot, bool) {
        match outcome {
            PreviewStartOutcome::Server { server, reused } => (server, reused),
            PreviewStartOutcome::Attached { .. } => {
                panic!("a remote-control entry is not attached")
            }
        }
    }

    /// The JSON for an entry that runs a command which outlives the startup gate and holds no
    /// port, named `name`.
    fn sleeper(name: &str, extra: &str) -> String {
        #[cfg(windows)]
        let (program, args) = (
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
                + "\\System32\\cmd.exe",
            vec!["/C", "ping -n 30 127.0.0.1"],
        );
        #[cfg(not(windows))]
        let (program, args) = ("/bin/sh".to_owned(), vec!["-c", "sleep 30"]);
        format!(
            r#"{{"name":"{name}","runtimeExecutable":{},"runtimeArgs":{},{extra}}}"#,
            serde_json::to_string(&program).unwrap(),
            serde_json::to_string(&args).unwrap()
        )
    }

    /// A remote-control entry connects through the registry: a row with no port and no
    /// machine, its viewer as `url`, its screen as `remote`, waited for until the session is
    /// ready — and the same name again hands the row back.
    #[test]
    fn a_remote_control_entry_connects_a_row_and_is_handed_back_again() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"pixel","type":"remote-control","protocol":"adb","device":"emulator-5554"}
]}"#,
        );
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();

        let (server, reused) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("pixel"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        assert!(!reused);
        assert_eq!(server.server_id, "pixel");
        assert_eq!(server.port, 0);
        assert_eq!(server.machine, None);
        assert_eq!(server.status, PreviewServerStatus::Running);
        assert!(server
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("http://127.0.0.1:")));
        let remote = server.remote.clone().unwrap();
        assert_eq!(
            remote.state,
            crate::remote_control::view::SessionPhase::Ready
        );
        assert!(
            remote.target.starts_with("emulator-5554 · "),
            "{}",
            remote.target
        );

        let (again, reused) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("PIXEL"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        assert!(reused);
        assert_eq!(again.handle, server.handle);
        assert_eq!(registry.servers().len(), 1);
        registry.stop_all();
    }

    /// `"machine"` names a registered SSH machine: the screen is there, the row is still this
    /// computer's (its viewer is), and a name nobody registered is refused with the names that
    /// are.
    #[test]
    fn a_remote_control_entry_on_a_registered_machine() {
        use crate::remote_control::fake;
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"iphone","type":"remote-control","protocol":"wda","machine":"lab"},
  {"name":"other","type":"remote-control","protocol":"wda","machine":"studio"}
]}"#,
        );
        let registry = PreviewServerRegistry::default();
        let environment = ControlEnvironment {
            machines: Ok(vec![fake::machine("m1", "Lab", "203.0.113.7")]),
            ..fake_environment()
        }
        .with_route_check(std::sync::Arc::new(|_| Ok(())));

        let (server, _) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("iphone"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        assert_eq!(server.machine, None);
        assert_eq!(server.remote.unwrap().target, "Lab · 127.0.0.1:8100");

        let error = start(
            &registry,
            workspace.path(),
            Some("other"),
            Some("chat-1"),
            &environment,
        )
        .unwrap_err();
        assert!(
            error.starts_with("No SSH machine named \"studio\" is registered in Mewrk."),
            "{error}"
        );
        assert!(error.contains("\"Lab\""), "{error}");
        registry.stop_all();
    }

    /// A machine registered at this computer's own address is dialled through its agent under its
    /// own key — never rerouted to this computer, which would reach another device than a
    /// forwarded guest's — so a phone there connects as that machine's. A VNC server there may be
    /// this computer's own desktop, on the default SSH port or any other, and is refused before
    /// anything starts.
    #[test]
    fn a_machine_at_this_computers_address_connects_phones_but_never_vnc() {
        use crate::remote_control::fake;
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"phone","type":"remote-control","protocol":"adb","device":"emulator-5554","machine":"self"},
  {"name":"desktop","type":"remote-control","protocol":"vnc","port":5900,"machine":"self"},
  {"name":"forwarded","type":"remote-control","protocol":"vnc","port":5901,"machine":"guest"}
]}"#,
        );
        let mut guest = fake::machine("m2", "guest", "holycat@127.0.0.1");
        guest.port = 2222;
        let registry = PreviewServerRegistry::default();
        let environment = ControlEnvironment {
            machines: Ok(vec![fake::machine("m1", "self", "localhost"), guest]),
            ..fake_environment()
        }
        .with_route_check(std::sync::Arc::new(|_| Ok(())));
        let start_named = |name: &str| {
            start(
                &registry,
                workspace.path(),
                Some(name),
                Some("chat-1"),
                &environment,
            )
        };

        let (phone, _) = server_of(start_named("phone").unwrap());
        let session = registry.control(&phone.handle).unwrap().session;
        assert!(
            session.lock_key().contains("|ssh:m1|"),
            "{}",
            session.lock_key()
        );
        assert!(phone
            .remote
            .unwrap()
            .target
            .starts_with("emulator-5554 · self"));

        for (name, machine, address) in [
            ("desktop", "self", "localhost:22"),
            ("forwarded", "guest", "127.0.0.1:2222"),
        ] {
            let refused = start_named(name).unwrap_err();
            assert!(
                refused.starts_with(&format!(
                    "\"{name}\" is a VNC screen on the SSH machine \"{machine}\", which is registered at an address of this computer ({address}), so that VNC server may be this computer's own desktop."
                )),
                "{refused}"
            );
        }
        // Only the phone: nothing was started for a refused one.
        assert_eq!(registry.servers().len(), 1);
        registry.stop_all();
    }

    /// `launch` starts its entry first through the ordinary process path, the screen's row
    /// remembers it, and stopping the screen leaves it running — so a second start finds it
    /// running instead of starting another.
    #[test]
    fn a_launch_entry_starts_first_and_outlives_the_screen() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            &format!(
                r#"{{"configurations":[
  {},
  {{"name":"emu","type":"remote-control","protocol":"adb","device":"emulator-5554","launch":"emu-proc"}}
]}}"#,
                sleeper("emu-proc", r#""port":0,"autoPort":true"#)
            ),
        );
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();

        let (screen, _) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("emu"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        let control = registry.control(&screen.handle).unwrap();
        let launch = control.launch.clone().expect("the launch entry");
        assert_eq!(launch.server_id, "emu-proc");
        assert!(!launch.was_running);
        let sibling = registry.get(&launch.handle).expect("the launch entry runs");
        assert!(sibling.remote.is_none());
        assert_eq!(sibling.session_id.as_deref(), Some("chat-1"));

        assert!(registry.stop(&screen.handle));
        assert!(registry.get(&launch.handle).is_some());
        let (again, reused) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("emu"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        assert!(!reused);
        let relaunch = registry.control(&again.handle).unwrap().launch.unwrap();
        assert_eq!(relaunch.handle, launch.handle);
        assert!(relaunch.was_running);
        registry.stop_all();
    }

    /// `launch` must name one entry of the file that runs a command; anything else is refused at
    /// the start, in words that say what to write, and starts nothing.
    #[test]
    fn a_launch_that_names_no_runnable_entry_is_refused() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            r#"{"configurations":[
  {"name":"docs","url":"https://example.com/docs"},
  {"name":"pixel","type":"remote-control","protocol":"adb"},
  {"name":"bad","runtimeExecutable":"npm","port":"x"},
  {"name":"missing","type":"remote-control","protocol":"adb","device":"a","launch":"nope"},
  {"name":"attach","type":"remote-control","protocol":"adb","device":"b","launch":"docs"},
  {"name":"control","type":"remote-control","protocol":"adb","device":"c","launch":"pixel"},
  {"name":"broken","type":"remote-control","protocol":"adb","device":"d","launch":"bad"}
]}"#,
        );
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();
        let refusal = |name: &str| {
            start(
                &registry,
                workspace.path(),
                Some(name),
                Some("chat-1"),
                &environment,
            )
            .unwrap_err()
        };

        let missing = refusal("missing");
        assert!(
            missing.starts_with("\"missing\" launches \"nope\" first, but .mewrk/launch.json has no entry named \"nope\""),
            "{missing}"
        );
        let attach = refusal("attach");
        assert!(
            attach.contains("\"docs\" has no command to run (it attaches to a url)"),
            "{attach}"
        );
        let control = refusal("control");
        assert!(
            control.contains("\"pixel\" is a remote-control entry itself"),
            "{control}"
        );
        for refusal in [&missing, &attach, &control] {
            assert!(
                refusal.contains("\"launch\" must name an entry with a command"),
                "{refusal}"
            );
        }
        let broken = refusal("broken");
        assert!(
            broken.starts_with("\"broken\" launches \"bad\" first, but that entry of .mewrk/launch.json could not be used: "),
            "{broken}"
        );
        assert!(broken.contains("\"port\""), "{broken}");
        assert!(registry.servers().is_empty());
    }

    /// A VNC server on this computer may be the user's desktop, so an entry reaches one only
    /// through a `launch` entry that opens exactly that port and never moves it. Each refusal
    /// comes before anything starts.
    #[test]
    fn a_vnc_screen_on_this_computer_needs_a_launch_entry_that_owns_the_port() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            &format!(
                r#"{{"configurations":[
  {},
  {},
  {},
  {{"name":"desktop","type":"remote-control","protocol":"vnc"}},
  {{"name":"loose","type":"remote-control","protocol":"vnc","port":5901,"launch":"vm-auto"}},
  {{"name":"moved","type":"remote-control","protocol":"vnc","port":5901,"launch":"vm-other"}},
  {{"name":"phone","type":"remote-control","protocol":"vnc","port":5901,"launch":"vm-unset"}}
]}}"#,
                sleeper("vm-auto", r#""port":5901,"autoPort":true"#),
                sleeper("vm-other", r#""port":5902,"autoPort":false"#),
                sleeper("vm-unset", r#""port":5901"#),
            ),
        );
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();
        let refusal = |name: &str| {
            start(
                &registry,
                workspace.path(),
                Some(name),
                Some("chat-1"),
                &environment,
            )
            .unwrap_err()
        };

        let desktop = refusal("desktop");
        assert!(
            desktop.starts_with("\"desktop\" is a VNC screen on this computer, and a VNC server on this computer may be the user's own desktop."),
            "{desktop}"
        );
        assert!(desktop.contains("\"launch\""), "{desktop}");
        for name in ["loose", "phone"] {
            let loose = refusal(name);
            assert!(
                loose.contains("Set \"autoPort\": false on \"vm-"),
                "{loose}"
            );
        }
        let moved = refusal("moved");
        assert!(
            moved.starts_with("\"moved\" connects to port 5901 on this computer, but \"vm-other\" is configured for port 5902."),
            "{moved}"
        );
        assert!(registry.servers().is_empty());
    }

    // ---- Which entry a start addresses -----------------------------------------

    fn view(name: &str) -> EntryView<'_> {
        EntryView {
            name,
            attaches: false,
        }
    }

    /// The one rule for which entry a name addresses, as the start applies it with the registry's
    /// facts and as a caller without them must assume it: the name trimmed and matched in any
    /// case, a repeated name's next free entry, a numbered id's own entry, and the parser's guess
    /// that the only entry of a clean file is the one meant.
    #[test]
    fn a_name_picks_its_entry_by_one_rule_with_or_without_the_registry() {
        let entries = [view("web"), view("dev"), view("Dev")];
        let malformed = ["broken"];
        let nothing: [String; 0] = [];
        let unnumbered = |_: &str| Vec::new();
        let fresh = RepeatFacts {
            order: &unnumbered,
            running: &nothing,
        };
        let first_started = |_: &str| vec![0];
        let running = ["dev-1".to_owned()];
        let first_running = RepeatFacts {
            order: &first_started,
            running: &running,
        };
        let both_started = |_: &str| vec![0, 1];
        let both = RepeatFacts {
            order: &both_started,
            running: &nothing,
        };
        let pick = |name: Option<&str>, facts: Option<&RepeatFacts<'_>>| {
            pick_entry(&entries, &malformed, name, facts)
        };
        let one = |index, numbering| EntryPick::One { index, numbering };

        for name in ["web", " web ", "WEB\t"] {
            assert_eq!(pick(Some(name), None), one(0, Numbering::Unique), "{name:?}");
        }
        for name in [None, Some(""), Some("  "), Some("nothing"), Some("broken")] {
            assert_eq!(pick(name, Some(&fresh)), EntryPick::Unselected, "{name:?}");
        }
        assert_eq!(
            pick(Some("dev "), Some(&fresh)),
            one(1, Numbering::Occurrence { key: 1, occurrence: 0 })
        );
        assert_eq!(
            pick(Some("dev"), Some(&first_running)),
            one(2, Numbering::Occurrence { key: 1, occurrence: 1 })
        );
        assert_eq!(pick(Some("dev"), None), EntryPick::AnyOf(vec![1, 2]));
        assert_eq!(pick(Some(" dev-2"), Some(&both)), one(2, Numbering::Known(2)));
        assert_eq!(
            pick(Some("dev-2"), Some(&first_running)),
            EntryPick::NotStarted {
                first: 1,
                siblings: 2
            }
        );
        assert_eq!(pick(Some("dev-2"), None), EntryPick::AnyOf(vec![1, 2]));
        // A numbered id of a name only one entry has is no number: the parser decides.
        assert_eq!(pick(Some("web-1"), Some(&fresh)), EntryPick::Unselected);

        // One usable entry and nothing malformed: any name, or none, is that entry.
        let only = [view("pixel")];
        for name in [None, Some("web"), Some("pixel-2")] {
            assert_eq!(
                pick_entry(&only, &[], name, None),
                one(0, Numbering::Unique),
                "{name:?}"
            );
        }
        assert_eq!(
            pick_entry(&only, &["broken"], Some("web"), None),
            EntryPick::Unselected
        );
    }

    /// The picker's selection is the parser's: wherever `select_server` picks an entry, the
    /// picker picks the same one, and wherever it refuses, the picker does too.
    #[test]
    fn the_picker_selects_exactly_what_the_parser_selects() {
        let files = [
            r#"{"configurations":[{"name":"web","runtimeExecutable":"npm"}]}"#,
            r#"{"configurations":[{"name":"pixel","type":"remote-control","protocol":"adb"}]}"#,
            r#"{"configurations":[{"name":"web","runtimeExecutable":"npm"},{"name":"broken","runtimeExecutable":3}]}"#,
            r#"{"configurations":[{"name":"web","runtimeExecutable":"npm"},{"name":"Pixel","type":"remote-control","protocol":"adb"},{"name":"docs","url":"https://example.com"}]}"#,
            r#"{"configurations":[{"name":"broken","runtimeExecutable":3}]}"#,
        ];
        let working_directory = Path::new("/project");
        for file in files {
            let discovery =
                LaunchConfigDiscovery::new(working_directory).parse_source(file, None);
            let servers = discovery
                .config()
                .map(|config| config.servers.clone())
                .unwrap_or_default();
            let entries: Vec<EntryView<'_>> = servers.iter().map(EntryView::of_config).collect();
            let malformed: Vec<&str> = discovery
                .malformed()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            for name in [
                None,
                Some("web"),
                Some("WEB"),
                Some("pixel"),
                Some("docs"),
                Some("broken"),
                Some("guess"),
            ] {
                let parser = preview_launch_config::select_server(&discovery, working_directory, name)
                    .ok()
                    .map(|selected| {
                        servers
                            .iter()
                            .position(|server| *server == selected)
                            .expect("the parser selects one of the file's entries")
                    });
                assert_eq!(
                    select_entry(&entries, &malformed, name),
                    parser,
                    "{file} / {name:?}"
                );
            }
        }
    }

    /// A start held to an approval connects a remote-control entry only when it is one the
    /// approval confirmed, as it is now — and a refused start numbers nothing. A dev server is
    /// started whatever the approval, as before.
    #[test]
    fn a_start_connects_a_remote_control_entry_only_as_it_was_confirmed() {
        let workspace = tempfile::tempdir().unwrap();
        let screen = r#"{"name":"dev","type":"remote-control","protocol":"adb","device":"emulator-5554"}"#;
        write_launch_json(
            workspace.path(),
            &format!(
                r#"{{"configurations":[{},{screen}]}}"#,
                sleeper("dev", r#""port":0,"autoPort":true"#)
            ),
        );
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();
        let local = crate::workspace_set::WorkspaceSet::local_root(
            workspace.path().to_string_lossy().into_owned(),
        );
        let resolved = local.select(None).unwrap();
        let read = || read_for_start(&registry, resolved, "dev", "chat-1").unwrap();
        let start_as = |approval: &ControlApproval| {
            start_with_approval(
                &registry,
                workspace.path(),
                Some("dev"),
                Some("chat-1"),
                &environment,
                approval,
            )
        };

        // Nothing runs: the name is the dev server's, which no card is needed for.
        let first = read();
        assert!(matches!(&first, StartReading::Read { picks, .. } if picks == &[0]));
        let approval = ControlApproval::Confirmed(first.confirmed_controls());
        assert_eq!(approval, ControlApproval::Confirmed(Vec::new()));
        let (dev, _) = server_of(start_as(&approval).unwrap());
        assert_eq!(dev.server_id, "dev-1");
        assert!(dev.remote.is_none());

        // With it running the same name reaches the screen, which that call was never confirmed
        // for: refused, and nothing is numbered for it.
        let refused = start_as(&approval).unwrap_err();
        assert!(refused.starts_with("\"dev\" is a remote-control entry"), "{refused}");
        assert_eq!(registry.repeated_name_order(workspace.path(), "dev"), [0]);
        assert_eq!(registry.servers().len(), 1);

        // Read again, the card is the screen's, and the start connects it.
        let second = read();
        assert!(matches!(&second, StartReading::Read { picks, .. } if picks == &[1]));
        let (screen_row, _) = server_of(
            start_as(&ControlApproval::Confirmed(second.confirmed_controls())).unwrap(),
        );
        assert_eq!(screen_row.server_id, "dev-2");
        assert!(screen_row.remote.is_some());
        registry.stop_all();
    }

    /// What a card showed of a `launch` entry is part of what was confirmed: the same screen with
    /// another command line behind it is another start.
    #[test]
    fn a_changed_launch_entry_is_not_what_was_confirmed() {
        let workspace = tempfile::tempdir().unwrap();
        let screen = r#"{"name":"emu","type":"remote-control","protocol":"adb","device":"emulator-5554","launch":"emu-proc"}"#;
        let file = |extra: &str| {
            format!(
                r#"{{"configurations":[{},{screen}]}}"#,
                sleeper("emu-proc", extra)
            )
        };
        write_launch_json(workspace.path(), &file(r#""port":0,"autoPort":true"#));
        let registry = PreviewServerRegistry::default();
        let local = crate::workspace_set::WorkspaceSet::local_root(
            workspace.path().to_string_lossy().into_owned(),
        );
        let confirmed = read_for_start(&registry, local.select(None).unwrap(), "emu", "chat-1")
            .unwrap()
            .confirmed_controls();
        assert_eq!(confirmed.len(), 1);
        assert!(confirmed[0].launch.is_some());

        write_launch_json(
            workspace.path(),
            &file(r#""port":0,"autoPort":true,"env":{"LD_PRELOAD":"/tmp/x.so"}"#),
        );
        let refused = start_with_approval(
            &registry,
            workspace.path(),
            Some("emu"),
            Some("chat-1"),
            &fake_environment(),
            &ControlApproval::Confirmed(confirmed),
        )
        .unwrap_err();
        assert!(refused.contains("was not confirmed for it"), "{refused}");
        assert!(registry.servers().is_empty());
    }

    /// A `launch` entry already running is handed over only when it was started from what the file
    /// gives it now. One started under the same name from another command line — before the file
    /// was rewritten — is refused, for a model's confirmed start as for the person's, and the card's
    /// reading says it is running and from what; once the file matches again it is reused.
    #[test]
    fn a_running_launch_entry_started_from_another_command_line_is_not_handed_over() {
        let workspace = tempfile::tempdir().unwrap();
        let screen = r#"{"name":"emu","type":"remote-control","protocol":"adb","device":"emulator-5554","launch":"emu-proc"}"#;
        let original = sleeper("emu-proc", r#""port":0,"autoPort":true"#);
        // The same entry, rewritten to run the same program with other words.
        let rewritten = original.replace(" 30", " 29");
        assert_ne!(rewritten, original);
        let file = |sibling: &str| format!(r#"{{"configurations":[{sibling},{screen}]}}"#);
        write_launch_json(workspace.path(), &file(&original));
        let registry = PreviewServerRegistry::default();
        let environment = fake_environment();
        let local = crate::workspace_set::WorkspaceSet::local_root(
            workspace.path().to_string_lossy().into_owned(),
        );
        let resolved = local.select(None).unwrap();

        // Started by its own name, as an ordinary dev server.
        let (sibling, _) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("emu-proc"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        let reading = read_for_start(&registry, resolved, "emu", "chat-1").unwrap();
        let StartReading::Read {
            running_launches, ..
        } = &reading
        else {
            panic!("the file is readable");
        };
        assert_eq!(running_launches.len(), 1);
        assert!(running_launches[0].as_configured);

        write_launch_json(workspace.path(), &file(&rewritten));
        let reading = read_for_start(&registry, resolved, "emu", "chat-1").unwrap();
        let StartReading::Read {
            running_launches, ..
        } = &reading
        else {
            panic!("the file is readable");
        };
        assert_eq!(running_launches.len(), 1);
        assert_eq!(running_launches[0].name, "emu-proc");
        assert!(!running_launches[0].as_configured);
        assert!(running_launches[0]
            .spawned
            .args
            .iter()
            .any(|arg| arg.contains(" 30")));

        let confirmed = ControlApproval::Confirmed(reading.confirmed_controls());
        for approval in [&confirmed, &ControlApproval::Person] {
            let refused = start_with_approval(
                &registry,
                workspace.path(),
                Some("emu"),
                Some("chat-1"),
                &environment,
                approval,
            )
            .unwrap_err();
            assert!(
                refused.starts_with("\"emu\" launches \"emu-proc\" first, but the running \"emu-proc\" was started from a different command line than .mewrk/launch.json now gives \"emu-proc\""),
                "{refused}"
            );
            assert!(
                refused.contains("preview_stop with serverId \"emu-proc\""),
                "{refused}"
            );
        }
        // Nothing was connected, and the running process was left alone.
        assert!(registry.controls().is_empty());
        assert!(registry.get(&sibling.handle).is_some());

        write_launch_json(workspace.path(), &file(&original));
        let (screen_row, _) = server_of(
            start(
                &registry,
                workspace.path(),
                Some("emu"),
                Some("chat-1"),
                &environment,
            )
            .unwrap(),
        );
        let launch = registry
            .control(&screen_row.handle)
            .unwrap()
            .launch
            .unwrap();
        assert_eq!(launch.handle, sibling.handle);
        assert!(launch.was_running);
        registry.stop_all();
    }

    /// Calls `start` for each of `names` at once, the way the pane, the model and a subagent can.
    #[cfg(unix)]
    fn start_at_once(
        registry: &PreviewServerRegistry,
        workspace: &Path,
        names: &[&str],
    ) -> Vec<Result<PreviewServerSnapshot, String>> {
        let barrier = std::sync::Barrier::new(names.len());
        std::thread::scope(|scope| {
            let calls: Vec<_> = names
                .iter()
                .map(|name| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        match start(
                            registry,
                            workspace,
                            Some(name),
                            Some("chat-1"),
                            &ControlEnvironment::this_computer(),
                        )? {
                            PreviewStartOutcome::Server { server, .. } => Ok(server),
                            PreviewStartOutcome::Attached { .. } => {
                                panic!("these entries run a command")
                            }
                        }
                    })
                })
                .collect();
            calls.into_iter().map(|call| call.join().unwrap()).collect()
        })
    }

    #[cfg(unix)]
    fn sleeper_entry(name: &str, port: u16, auto_port: &str) -> String {
        format!(
            r#"{{"name":"{name}","runtimeExecutable":"/bin/sh","runtimeArgs":["-c","sleep 30 # {name}"],"port":{port}{auto_port}}}"#
        )
    }

    #[cfg(unix)]
    fn free_port() -> u16 {
        std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Overlapping starts of one entry are one server: every `autoPort` answer hands the later
    /// calls the row of the first, rather than a second process or a port "in use by another
    /// process" that is this very server.
    #[cfg(unix)]
    #[test]
    fn overlapping_starts_of_one_entry_share_one_server() {
        std::thread::scope(|scope| {
            for auto_port in ["", r#","autoPort":false"#, r#","autoPort":true"#] {
                scope.spawn(move || {
                    let workspace = tempfile::tempdir().unwrap();
                    write_launch_json(
                        workspace.path(),
                        &format!(
                            r#"{{"configurations":[{}]}}"#,
                            sleeper_entry("web", free_port(), auto_port)
                        ),
                    );
                    let registry = PreviewServerRegistry::default();

                    let started = start_at_once(&registry, workspace.path(), &["web"; 4]);

                    let rows = registry.servers_for_worktree(workspace.path());
                    assert_eq!(rows.len(), 1, "{auto_port}: {started:?}");
                    for call in started {
                        assert_eq!(call.unwrap().handle, rows[0].handle, "{auto_port}");
                    }
                    registry.stop_all();
                });
            }
        });
    }

    /// Two entries asking for one port at once are told apart the way two started in turn are:
    /// with `autoPort` the later one moves, and without it the refusal names the preview server
    /// that holds the port.
    #[cfg(unix)]
    #[test]
    fn overlapping_starts_of_two_entries_never_take_one_port() {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let workspace = tempfile::tempdir().unwrap();
                let port = free_port();
                write_launch_json(
                    workspace.path(),
                    &format!(
                        r#"{{"configurations":[{},{}]}}"#,
                        sleeper_entry("a", port, r#","autoPort":true"#),
                        sleeper_entry("b", port, r#","autoPort":true"#)
                    ),
                );
                let registry = PreviewServerRegistry::default();

                let started = start_at_once(&registry, workspace.path(), &["a", "b"]);

                let ports: Vec<u16> = started
                    .into_iter()
                    .map(|call| call.unwrap().port)
                    .collect();
                assert_ne!(ports[0], ports[1]);
                assert!(ports.contains(&port), "{ports:?} left {port} unused");
                registry.stop_all();
            });
            scope.spawn(|| {
                let workspace = tempfile::tempdir().unwrap();
                let port = free_port();
                write_launch_json(
                    workspace.path(),
                    &format!(
                        r#"{{"configurations":[{},{}]}}"#,
                        sleeper_entry("a", port, r#","autoPort":false"#),
                        sleeper_entry("b", port, r#","autoPort":false"#)
                    ),
                );
                let registry = PreviewServerRegistry::default();

                let started = start_at_once(&registry, workspace.path(), &["a", "b"]);

                let refusals: Vec<&String> =
                    started.iter().filter_map(|call| call.as_ref().err()).collect();
                assert_eq!(refusals.len(), 1, "{started:?}");
                assert!(refusals[0].contains("in use by preview server"), "{}", refusals[0]);
                assert_eq!(registry.servers_for_worktree(workspace.path()).len(), 1);
                registry.stop_all();
            });
        });
    }

    /// The cap counts rows, so starts overlapping at the cap must see each other's.
    #[cfg(unix)]
    #[test]
    fn overlapping_starts_cannot_pass_the_cap_together() {
        let names = ["a", "b", "c", "d", "e", "f", "g"];
        let workspace = tempfile::tempdir().unwrap();
        let entries: Vec<String> = names
            .iter()
            .map(|name| sleeper_entry(name, 0, r#","autoPort":true"#))
            .collect();
        write_launch_json(
            workspace.path(),
            &format!(r#"{{"configurations":[{}]}}"#, entries.join(",")),
        );
        let registry = PreviewServerRegistry::default();

        let started = start_at_once(&registry, workspace.path(), &names);

        assert_eq!(
            registry.servers_for_worktree(workspace.path()).len(),
            preview_servers::MAX_SERVERS_PER_WORKTREE
        );
        assert_eq!(
            started.iter().filter(|call| call.is_err()).count(),
            names.len() - preview_servers::MAX_SERVERS_PER_WORKTREE,
            "{started:?}"
        );
        registry.stop_all();
    }

    /// A start's turn ends with its row, so a server waiting out its startup gate holds up no
    /// other start on the machine.
    #[cfg(unix)]
    #[test]
    fn a_start_gives_up_its_turn_before_its_startup_gate() {
        let workspace = tempfile::tempdir().unwrap();
        write_launch_json(
            workspace.path(),
            &format!(
                r#"{{"configurations":[{}]}}"#,
                sleeper_entry("web", 0, r#","autoPort":true"#)
            ),
        );
        let registry = PreviewServerRegistry::default();
        let turn = registry.start_turn(None);

        std::thread::scope(|scope| {
            let call = scope.spawn(|| start_at_once(&registry, workspace.path(), &["web"]));
            let free_while_gated = loop {
                assert!(!call.is_finished(), "the turn was not free during the gate");
                let row_up = !registry.servers_for_worktree(workspace.path()).is_empty();
                if row_up && turn.try_lock().is_ok() {
                    break !call.is_finished();
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(free_while_gated);
            assert!(call.join().unwrap().remove(0).is_ok());
        });
        registry.stop_all();
    }

    fn write_launch_json(workspace: &Path, source: &str) {
        let directory = workspace.join(".mewrk");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("launch.json"), source).unwrap();
    }
}
