//! Where a remote-control entry's service is, and what `preview_start` resolves
//! that with (`docs/remote-control.md` §2.3).
//!
//! The file names a place three ways — the workspace's, `"thisComputer"`, or a
//! registered SSH machine by its display name — and a session needs one
//! concrete answer: this computer's loopback, or one machine's loopback reached
//! through its Mewrk agent. [`ControlEnvironment`] is everything that answer
//! depends on that the file cannot say: the user's machine list, how a
//! machine's link is checked, and (for tests) which drivers connect. The
//! callers build it from the application's state; `preview::start` and
//! `preview::start_remote` resolve against it.
//!
//! Two keys derive from the resolved place and must agree wherever they are
//! computed: the place key (`local`, `ssh:<machine id>` — never a display name,
//! which the user can change and need not keep unique) and the device lock key
//! built on it ([`device_lock_key`]).

use std::{sync::Arc, time::Duration};

use crate::{
    model::{ExecutionEnvironmentAssets, RunTarget, SshMachineConfig},
    preview_launch_config::sanitize_for_message,
    preview_remote::RemoteMachine,
    run_environment::{self, ShellRunner},
};

use super::{
    session::ConnectFn,
    target::{ControlTarget, TargetPlace},
    transport::{names_this_computer, Connector, LocalConnector, MachineConnector},
};

/// A resolved place: where a session's connector dials.
#[derive(Clone, Debug)]
pub enum ControlPlace {
    ThisComputer,
    /// A registered SSH machine, reached through its agent — always, whatever its host: a
    /// machine registered at this computer's own address may be a guest behind a forward, with
    /// its own devices, and dialling this computer's loopback instead would drive a device other
    /// than the one the card names, under the wrong place key.
    Machine {
        machine: RemoteMachine,
        id: String,
        /// Its SSH address as registered (`host:port`), when the host names this computer
        /// (`localhost`, a loopback address, one of this computer's own) — on whatever port: it
        /// may be this computer under another name ([`ControlPlace::may_be_this_computer`]).
        this_computer_address: Option<String>,
    },
}

impl ControlPlace {
    /// The place's stable identity: `local`, or `ssh:<machine id>` — what
    /// [`Connector::place_key`] reports for the connector of the same place.
    pub fn key(&self) -> String {
        match self {
            Self::ThisComputer => "local".to_owned(),
            Self::Machine { id, .. } => format!("ssh:{id}"),
        }
    }

    pub fn is_this_computer(&self) -> bool {
        matches!(self, Self::ThisComputer)
    }

    /// Whether this place may be this computer reached under another name: a machine whose SSH
    /// host is an address of this computer. Nothing here can tell a second SSH server of this
    /// computer's own, or a forward that loops back, from a guest behind a forward — that needs an
    /// identity the agent would report, which it does not — so a session there is dialled through
    /// the machine's agent, as the card names it, and a VNC server there is refused: it may be the
    /// user's own desktop, which only a VNC entry on this computer with a `launch` entry that owns
    /// the port may reach (`docs/remote-control.md` §2.4). Phones are unaffected: the card names
    /// the device.
    pub fn may_be_this_computer(&self) -> bool {
        match self {
            Self::ThisComputer => true,
            Self::Machine {
                this_computer_address,
                ..
            } => this_computer_address.is_some(),
        }
    }

    /// The SSH address a machine that may be this computer is registered at, for a message.
    pub fn this_computer_address(&self) -> Option<&str> {
        match self {
            Self::ThisComputer => None,
            Self::Machine {
                this_computer_address,
                ..
            } => this_computer_address.as_deref(),
        }
    }

    /// The place as a person knows it, for a message.
    pub fn label(&self) -> String {
        match self {
            Self::ThisComputer => "this computer".to_owned(),
            Self::Machine { machine, .. } => machine.label().to_owned(),
        }
    }

    /// The connector a session at this place dials through.
    pub fn connector(&self) -> Arc<dyn Connector> {
        match self {
            Self::ThisComputer => Arc::new(LocalConnector::new()),
            Self::Machine { machine, id, .. } => {
                Arc::new(MachineConnector::new(machine.clone(), id))
            }
        }
    }
}

/// The device lock's key for `target` at the place `place_key` names:
/// `<protocol>|<place key>|<port>|<device or ''>`. The same text as
/// `RemoteControlSession::lock_key` gives once the session
/// exists; the registry needs it before, to refuse a second session on the
/// same screen without creating one first.
pub fn device_lock_key(target: &ControlTarget, place_key: &str) -> String {
    format!(
        "{}|{}|{}|{}",
        target.protocol.as_str(),
        place_key,
        target.port,
        target.device.as_deref().unwrap_or("")
    )
}

/// One SSH machine of the user's machine list, as `"machine"` addresses it.
#[derive(Clone, Debug)]
pub struct ControlMachine {
    /// The machine's id in the list: what its place key is made of.
    pub id: String,
    /// Its display name, which `"machine"` matches case-insensitively.
    pub name: String,
    /// The SSH host as registered, to tell a machine that may be this computer
    /// ([`ControlPlace::may_be_this_computer`]).
    pub host: String,
    /// The SSH port as registered; `0` is SSH's default, 22. It does not tell
    /// this computer from a guest behind a forward on its loopback, so nothing
    /// decides by it; it is part of the address a message names.
    pub port: u16,
    /// How the preview reaches it, or why it cannot (a machine with no host).
    pub machine: Result<RemoteMachine, String>,
}

impl ControlMachine {
    /// A machine of the list, with the runner its catalog entry resolves to.
    pub fn from_config(assets: &ExecutionEnvironmentAssets, config: &SshMachineConfig) -> Self {
        let target = RunTarget::Ssh {
            machine_id: config.id.clone(),
        };
        let label = if config.name.trim().is_empty() {
            "the remote machine".to_owned()
        } else {
            config.name.clone()
        };
        Self {
            id: config.id.clone(),
            name: config.name.clone(),
            host: config.host.clone(),
            port: config.port,
            machine: run_environment::resolve_shell_runner(assets, Some(&target), None).map(
                |runner| RemoteMachine::new(runner, run_environment::env_key(Some(&target)), label),
            ),
        }
    }

    /// The machine a workspace on another machine is on, as the preview
    /// already reaches it: its id is in its environment key, its host in its
    /// runner.
    pub fn of_workspace(machine: &RemoteMachine) -> Self {
        let (host, port) = match machine.runner() {
            ShellRunner::Ssh { host, port, .. } => (host.clone(), *port),
            _ => (String::new(), 0),
        };
        Self {
            id: machine
                .key()
                .strip_prefix("ssh:")
                .unwrap_or(machine.key())
                .to_owned(),
            name: machine.label().to_owned(),
            host,
            port,
            machine: Ok(machine.clone()),
        }
    }
}

/// How long a start waits, by default, for a `launch` entry it just started to open its port
/// before it connects. A virtual machine or an emulator opens its port within seconds; one that
/// has not by then is connected to anyway, and the session says what it found.
pub const LAUNCH_PORT_WAIT: Duration = Duration::from_secs(30);

/// Checks that a machine routes through Mewrk's agent before a session is
/// pointed at it. The real one waits for the link like every other preview
/// operation on a machine; tests replace it.
pub type RouteCheck = Arc<dyn Fn(&RemoteMachine) -> Result<(), String> + Send + Sync>;

/// What a remote-control start resolves places with: the user's machine list,
/// how a machine's link is checked, and the driver factory.
#[derive(Clone)]
pub struct ControlEnvironment {
    /// The registered SSH machines, or why they could not be read.
    pub machines: Result<Vec<ControlMachine>, String>,
    pub route_check: RouteCheck,
    /// The driver factory for every session started here; the real drivers
    /// when `None`. Tests plug a fake one in.
    pub connect: Option<ConnectFn>,
    /// How long a start waits for a `launch` entry it started to open its port
    /// ([`LAUNCH_PORT_WAIT`]; tests whose entries open none shorten it).
    pub launch_port_wait: Duration,
}

impl std::fmt::Debug for ControlEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlEnvironment")
            .field("machines", &self.machines)
            .field("connect", &self.connect.as_ref().map(|_| "custom"))
            .field("launch_port_wait", &self.launch_port_wait)
            .finish_non_exhaustive()
    }
}

/// A machine is reachable for remote control only through its agent: a relay
/// to its loopback needs `net connect` there.
fn agent_route(machine: &RemoteMachine) -> Result<(), String> {
    machine.link().map(|_| ())
}

impl ControlEnvironment {
    /// No machines registered: places resolve to this computer or to the
    /// workspace's own machine only.
    pub fn this_computer() -> Self {
        Self {
            machines: Ok(Vec::new()),
            route_check: Arc::new(agent_route),
            connect: None,
            launch_port_wait: LAUNCH_PORT_WAIT,
        }
    }

    /// The user's machine list as the application's settings hold it.
    pub fn from_assets(assets: &ExecutionEnvironmentAssets) -> Self {
        Self {
            machines: Ok(assets
                .ssh_machines
                .iter()
                .map(|config| ControlMachine::from_config(assets, config))
                .collect()),
            ..Self::this_computer()
        }
    }

    /// The machine list could not be read; a `"machine"` entry says why.
    pub fn without_machines(error: String) -> Self {
        Self {
            machines: Err(error),
            ..Self::this_computer()
        }
    }

    /// The same environment with `connect` as every session's driver factory.
    #[cfg(test)]
    pub fn with_connect(mut self, connect: ConnectFn) -> Self {
        self.connect = Some(connect);
        self
    }

    /// The same environment with `route_check` standing in for the link check.
    #[cfg(test)]
    pub fn with_route_check(mut self, route_check: RouteCheck) -> Self {
        self.route_check = route_check;
        self
    }

    /// Where the workspace is: this computer, or `workspace`'s machine — that
    /// machine even when its host is this computer's own address, the way the
    /// workspace itself reaches it. No link check.
    pub fn workspace_place(&self, workspace: Option<&RemoteMachine>) -> ControlPlace {
        match workspace {
            None => ControlPlace::ThisComputer,
            Some(machine) => {
                let machine = ControlMachine::of_workspace(machine);
                let this_computer_address = this_computer_address(&machine);
                match machine.machine {
                    Ok(remote) => ControlPlace::Machine {
                        machine: remote,
                        id: machine.id,
                        this_computer_address,
                    },
                    Err(_) => ControlPlace::ThisComputer,
                }
            }
        }
    }

    /// The place `target` names, for a workspace on this computer (`None`) or
    /// on `workspace`'s machine. A registered machine is dialled through its
    /// own agent, and checked to route through it, whatever its host is; one
    /// whose host is this computer's own address says so
    /// ([`ControlPlace::may_be_this_computer`]), which is what keeps a VNC
    /// session off the user's own desktop when `localhost` is registered as a
    /// machine.
    pub fn resolve(
        &self,
        target: &ControlTarget,
        workspace: Option<&RemoteMachine>,
    ) -> Result<ControlPlace, String> {
        let place = match &target.place {
            TargetPlace::ThisComputer => ControlPlace::ThisComputer,
            TargetPlace::Workspace => self.workspace_place(workspace),
            TargetPlace::Machine(name) => {
                let machine = self.machine_named(name)?;
                ControlPlace::Machine {
                    machine: machine.machine.clone()?,
                    id: machine.id.clone(),
                    this_computer_address: this_computer_address(machine),
                }
            }
        };
        if let ControlPlace::Machine { machine, .. } = &place {
            (self.route_check)(machine)?;
        }
        Ok(place)
    }

    /// The one registered machine called `name`, ignoring case.
    fn machine_named(&self, name: &str) -> Result<&ControlMachine, String> {
        let machines = self.machines.as_ref().map_err(|error| {
            format!(
                "Could not read the SSH machines registered in Mewrk to find \"{}\": {error}",
                sanitize_for_message(name)
            )
        })?;
        let wanted = name.trim().to_lowercase();
        let matching: Vec<&ControlMachine> = machines
            .iter()
            .filter(|machine| machine.name.trim().to_lowercase() == wanted)
            .collect();
        match matching.as_slice() {
            [machine] => Ok(machine),
            [] if machines.is_empty() => Err(format!(
                "No SSH machine named \"{}\" is registered in Mewrk, and none is registered at all. Ask the user to add it in Mewrk's settings, or remove \"machine\" from the entry to use the workspace's own machine.",
                sanitize_for_message(name)
            )),
            [] => {
                let mut names: Vec<String> = machines
                    .iter()
                    .map(|machine| format!("\"{}\"", sanitize_for_message(&machine.name)))
                    .collect();
                names.sort();
                names.dedup();
                Err(format!(
                    "No SSH machine named \"{}\" is registered in Mewrk. Registered machines: {}. Set \"machine\" to one of these names, or ask the user to add the machine in Mewrk's settings.",
                    sanitize_for_message(name),
                    names.join(", ")
                ))
            }
            several => Err(format!(
                "{} SSH machines registered in Mewrk are named \"{}\", so \"machine\" cannot say which one is meant. Ask the user to rename one of them in Mewrk's settings.",
                several.len(),
                sanitize_for_message(name)
            )),
        }
    }
}

/// A registered machine's SSH address (`host:port`, the port SSH's default 22
/// when the registration leaves it 0), when its host — `user@host` included —
/// names this computer, on any port.
///
/// Neither the host nor the port tells this computer from a machine behind a
/// forward on its loopback: a VM, a container or WSL registered as
/// `127.0.0.1:2222`, or forwarded to port 22 with this computer's own sshd
/// stopped, has its own adb server, its own devices and its own desktop, while a
/// second SSH server of this computer's own, on any port, is this computer.
/// Telling those apart needs an identity the agent reports, which it does not
/// yet. So such a machine is never taken for this computer — it is dialled
/// through its own agent, as the card names it — but it may be this computer,
/// and the VNC rule treats it so ([`ControlPlace::may_be_this_computer`]).
fn this_computer_address(machine: &ControlMachine) -> Option<String> {
    let host = machine.host.trim();
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    if host.is_empty() || !names_this_computer(host) {
        return None;
    }
    let port = if machine.port == 0 { 22 } else { machine.port };
    Some(format!("{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::remote_control::{
        fake::{self, machine},
        session::{RemoteControlSession, SessionSpec},
        target::Protocol,
        view::SessionPhase,
    };

    fn target(place: TargetPlace) -> ControlTarget {
        ControlTarget {
            protocol: Protocol::Adb,
            place,
            port: 5037,
            device: Some("emulator-5554".into()),
            username: None,
            launch: None,
            read_only: false,
        }
    }

    fn environment(machines: Vec<ControlMachine>) -> ControlEnvironment {
        ControlEnvironment {
            machines: Ok(machines),
            ..ControlEnvironment::this_computer()
        }
        .with_route_check(Arc::new(|_| Ok(())))
    }

    /// TEST-NET-3: no interface of a test machine holds it, and parsing it needs no lookup.
    const ELSEWHERE: &str = "203.0.113.7";

    #[test]
    fn a_machine_name_is_matched_ignoring_case_and_keyed_by_its_id() {
        let environment = environment(vec![
            machine("m1", "Mac", ELSEWHERE),
            machine("m2", "linux-box", "203.0.113.8"),
        ]);
        let place = environment
            .resolve(&target(TargetPlace::Machine("mac".into())), None)
            .unwrap();
        assert_eq!(place.key(), "ssh:m1");
        assert_eq!(place.label(), "Mac");
        assert!(!place.is_this_computer());
        assert_eq!(place.connector().place_key(), "ssh:m1");
    }

    #[test]
    fn an_unknown_machine_lists_the_registered_names() {
        let error = environment(vec![
            machine("m1", "Mac", ELSEWHERE),
            machine("m2", "linux-box", "203.0.113.8"),
        ])
        .resolve(&target(TargetPlace::Machine("pc".into())), None)
        .unwrap_err();
        assert!(error.starts_with("No SSH machine named \"pc\""), "{error}");
        assert!(error.contains("\"Mac\", \"linux-box\""), "{error}");

        let none = environment(Vec::new())
            .resolve(&target(TargetPlace::Machine("pc".into())), None)
            .unwrap_err();
        assert!(none.contains("none is registered at all"), "{none}");
    }

    #[test]
    fn a_name_two_machines_share_is_refused_rather_than_guessed() {
        let error = environment(vec![
            machine("m1", "Lab", ELSEWHERE),
            machine("m2", "lab", "203.0.113.8"),
        ])
        .resolve(&target(TargetPlace::Machine("LAB".into())), None)
        .unwrap_err();
        assert!(
            error.starts_with("2 SSH machines registered in Mewrk are named \"LAB\""),
            "{error}"
        );
        assert!(error.contains("rename one"), "{error}");
    }

    /// A machine registered at an address of this computer — `localhost`, a loopback address,
    /// on the default port or any other — is never taken for this computer: it may be a guest
    /// behind a forward, with its own devices, so it is dialled through its own agent under its
    /// own key, as the card names it. It is marked as possibly this computer, for the VNC rule.
    #[test]
    fn a_machine_at_this_computers_address_is_dialled_as_that_machine() {
        let mut ssh_port = machine("m4", "port22", "127.0.0.1");
        ssh_port.port = 22;
        let mut forwarded = machine("m5", "forwarded", "127.0.0.1");
        forwarded.port = 2222;
        let environment = environment(vec![
            machine("m1", "loop", "holycat@localhost"),
            machine("m2", "v6", "[::1]"),
            ssh_port,
            forwarded,
            machine("m6", "far", ELSEWHERE),
        ]);
        for (name, id) in [
            ("loop", "m1"),
            ("v6", "m2"),
            ("port22", "m4"),
            ("forwarded", "m5"),
        ] {
            let place = environment
                .resolve(&target(TargetPlace::Machine(name.into())), None)
                .unwrap();
            assert!(!place.is_this_computer(), "{name}");
            assert!(place.may_be_this_computer(), "{name}");
            assert!(place.this_computer_address().is_some(), "{name}");
            assert_eq!(place.key(), format!("ssh:{id}"));
            assert_eq!(place.connector().place_key(), format!("ssh:{id}"));
        }
        let far = environment
            .resolve(&target(TargetPlace::Machine("far".into())), None)
            .unwrap();
        assert!(!far.may_be_this_computer());
        assert_eq!(far.this_computer_address(), None);
        assert!(ControlPlace::ThisComputer.may_be_this_computer());

        // The workspace's own machine too.
        let workspace = machine("m3", "self", "127.0.0.1").machine.unwrap();
        let place = environment
            .resolve(&target(TargetPlace::Workspace), Some(&workspace))
            .unwrap();
        assert_eq!(place.key(), "ssh:m3");
        assert_eq!(place.this_computer_address(), Some("127.0.0.1:22"));
        assert_eq!(
            environment.workspace_place(Some(&workspace)).key(),
            "ssh:m3"
        );
    }

    /// A VM, a container or WSL reached through a forward on this computer's loopback is the
    /// machine the card names, with its own devices: it is dialled through its own agent.
    #[test]
    fn a_loopback_forward_to_another_machine_is_that_machine() {
        let mut vm = machine("vm1", "vm", "127.0.0.1");
        vm.port = 2222;
        let environment = environment(vec![vm.clone()]);
        let place = environment
            .resolve(&target(TargetPlace::Machine("vm".into())), None)
            .unwrap();
        assert!(!place.is_this_computer());
        assert_eq!(place.key(), "ssh:vm1");
        assert_eq!(place.label(), "vm");
        // A workspace on that VM is on the VM too.
        let workspace = RemoteMachine::new(
            ShellRunner::Ssh {
                host: "holycat@127.0.0.1".into(),
                port: 2222,
                identity_file: String::new(),
                env: Default::default(),
                agent_shell: Default::default(),
            },
            "ssh:vm1".into(),
            "vm".into(),
        );
        assert_eq!(
            environment
                .resolve(&target(TargetPlace::Workspace), Some(&workspace))
                .unwrap()
                .key(),
            "ssh:vm1"
        );
    }

    #[test]
    fn the_workspace_place_follows_the_workspace_and_this_computer_overrides_it() {
        let environment = environment(Vec::new());
        let workspace = machine("w1", "build", ELSEWHERE).machine.unwrap();
        assert_eq!(
            environment
                .resolve(&target(TargetPlace::Workspace), Some(&workspace))
                .unwrap()
                .key(),
            "ssh:w1"
        );
        assert_eq!(
            environment
                .resolve(&target(TargetPlace::Workspace), None)
                .unwrap()
                .key(),
            "local"
        );
        assert_eq!(
            environment
                .resolve(&target(TargetPlace::ThisComputer), Some(&workspace))
                .unwrap()
                .key(),
            "local"
        );
    }

    #[test]
    fn a_machine_without_the_agent_route_is_refused_with_the_checks_reason() {
        let environment = environment(vec![machine("m1", "Mac", ELSEWHERE)]).with_route_check(
            Arc::new(|machine| Err(format!("{} has no agent", machine.label()))),
        );
        assert_eq!(
            environment
                .resolve(&target(TargetPlace::Machine("mac".into())), None)
                .unwrap_err(),
            "Mac has no agent"
        );
        // This computer needs no route.
        assert!(environment
            .resolve(&target(TargetPlace::ThisComputer), None)
            .is_ok());
    }

    #[test]
    fn an_unreadable_machine_list_is_reported_for_a_machine_entry_only() {
        let environment = ControlEnvironment::without_machines("the document is locked".into());
        let error = environment
            .resolve(&target(TargetPlace::Machine("mac".into())), None)
            .unwrap_err();
        assert!(error.contains("the document is locked"), "{error}");
        assert!(environment
            .resolve(&target(TargetPlace::Workspace), None)
            .is_ok());
    }

    /// The registry computes the lock key before a session exists; the session computes it from
    /// its connector. Both have to name the same screen the same way.
    #[test]
    fn the_device_lock_key_is_the_sessions_own() {
        for place in [
            ControlPlace::ThisComputer,
            ControlPlace::Machine {
                machine: machine("m1", "Mac", ELSEWHERE).machine.unwrap(),
                id: "m1".into(),
                this_computer_address: None,
            },
        ] {
            for device in [None, Some("emulator-5554".to_owned())] {
                let mut target = target(TargetPlace::Workspace);
                target.device = device;
                let session = RemoteControlSession::start(SessionSpec {
                    name: "pixel".into(),
                    server_id: "pixel".into(),
                    target: target.clone(),
                    connector: place.connector(),
                    preflight: None,
                    on_change: Arc::new(|| {}),
                    log: Arc::new(|_| {}),
                    connect: Some(fake::ready(fake::FakeDriver::touch())),
                })
                .unwrap();
                assert_eq!(
                    session.wait_settled(Duration::from_secs(5)),
                    SessionPhase::Ready
                );
                assert_eq!(session.lock_key(), device_lock_key(&target, &place.key()));
                session.close();
            }
        }
    }
}
