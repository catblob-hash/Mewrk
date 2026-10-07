//! Process groups that end when Mewrk does (macOS and Linux).
//!
//! On Windows the shell tool's commands, dev and language servers, and the
//! managed MCP sidecar each live in a kill-on-close job object, so they die
//! with Mewrk however it exits. On macOS and Linux each is instead started as
//! the leader of its own process group — so stopping it can reach its whole
//! tree without reaching Mewrk — and nothing ends such a group when Mewrk
//! goes away: a `run_in_background` dev server started by the agent kept
//! running, and kept its port, after the application was gone. Each group is
//! registered here for as long as its owner holds it. The application's exit
//! path ends whatever is still registered, and for every other way Mewrk can
//! end — a panic that aborts, a crash, `SIGKILL`, a force quit — a
//! [`Watchdog`] outside it does, as closing the job does on Windows.
//!
//! Unix only; Windows has the job objects.

use std::{
    collections::BTreeSet,
    io::Write as _,
    process::{Child, ChildStdin, Command, Stdio},
    sync::Mutex,
};

/// The registered groups and the watchdog told about each of them.
struct Groups {
    registered: BTreeSet<i32>,
    watchdog: Option<Watchdog>,
    /// Set once a watchdog could not be started, so a machine without
    /// `/bin/sh` does not try again on every registration.
    watchdog_unavailable: bool,
}

static GROUPS: Mutex<Groups> = Mutex::new(Groups {
    registered: BTreeSet::new(),
    watchdog: None,
    watchdog_unavailable: false,
});

fn groups() -> std::sync::MutexGuard<'static, Groups> {
    GROUPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Groups {
    /// Tells the watchdog `line`, starting one first if there is none, and a
    /// fresh one told every registered group if the last one has gone away —
    /// something killed it, which is no reason for the groups to outlive
    /// Mewrk after all.
    fn tell(&mut self, line: &str) {
        if self.watchdog.as_mut().is_some_and(|watchdog| watchdog.tell(line).is_ok()) {
            return;
        }
        self.watchdog = None;
        if self.watchdog_unavailable {
            return;
        }
        let Ok(mut watchdog) = Watchdog::start() else {
            self.watchdog_unavailable = true;
            return;
        };
        let replayed = self
            .registered
            .iter()
            .try_for_each(|group| watchdog.tell(&format!("+{group}")));
        if replayed.is_ok() {
            self.watchdog = Some(watchdog);
        }
    }
}

/// A registered group; dropping it unregisters the group. Owners drop it only
/// after reaping the leader, so an id here never names a reused pid for long.
#[derive(Debug)]
pub(crate) struct GroupRegistration(i32);

impl Drop for GroupRegistration {
    fn drop(&mut self) {
        let mut groups = groups();
        if groups.registered.remove(&self.0) {
            groups.tell(&format!("-{}", self.0));
        }
    }
}

/// Registers the group led by `leader`, a child that made itself a group
/// leader (`setsid` or `setpgid(0, 0)`) before exec. `None` for an id that
/// cannot name a group.
pub(crate) fn register(leader: u32) -> Option<GroupRegistration> {
    let group = i32::try_from(leader).ok().filter(|group| *group > 1)?;
    let mut groups = groups();
    groups.registered.insert(group);
    groups.tell(&format!("+{group}"));
    Some(GroupRegistration(group))
}

/// Starts the watchdog ahead of the first registration. Called once at
/// startup, before any other thread exists: on macOS a pipe is made close-on-
/// exec only after it is created, and a child another thread spawned in
/// between would inherit the watchdog's pipe and hold it open after Mewrk is
/// gone. Registering starts one too, for a process that never called this.
pub(crate) fn start_watchdog() {
    let mut groups = groups();
    if groups.watchdog.is_none() && !groups.watchdog_unavailable {
        match Watchdog::start() {
            Ok(watchdog) => groups.watchdog = Some(watchdog),
            Err(_) => groups.watchdog_unavailable = true,
        }
    }
}

/// Ends every registered group: SIGTERM so servers can release what they hold,
/// then SIGKILL for any group still alive after a short grace. For the
/// application's exit only; the registrations stay with their owners, and the
/// watchdog is told the groups are gone so it has nothing left to do when
/// Mewrk's exit closes its pipe.
pub(crate) fn terminate_all() {
    let registered: Vec<i32> = groups().registered.iter().copied().collect();
    terminate(&registered);
    let mut groups = groups();
    for group in &registered {
        if let Some(watchdog) = groups.watchdog.as_mut() {
            let _ = watchdog.tell(&format!("-{group}"));
        }
    }
}

/// A process outside Mewrk that ends the groups it was told about when
/// Mewrk ends, however it ends.
///
/// It reads registrations (`+<group>`) and unregistrations (`-<group>`) from a
/// pipe whose only write end Mewrk holds. The kernel closes that end when
/// Mewrk's process goes, for any reason; the watchdog's read then sees end of
/// file, and it signals every group still on its list — SIGTERM, a short
/// grace, then SIGKILL — exactly as [`terminate_all`] would have. It leads a
/// session of its own and ignores the signals a terminal or a logout sends a
/// whole group, so whatever takes Mewrk down does not take it down first.
///
/// A `/bin/sh` script rather than a mode of Mewrk's own binary: a second copy
/// of the application would load its frameworks just to wait on a pipe, and
/// the job is a read loop and two `kill`s, which every POSIX shell has built
/// in (`kill -TERM -<group>` is the spelling dash, bash and zsh all accept).
pub(crate) struct Watchdog {
    input: ChildStdin,
    /// Never waited on while Mewrk runs; the watchdog outlives it by design.
    _process: Child,
}

const WATCHDOG_SCRIPT: &str = r#"trap '' HUP INT QUIT TERM
groups=' '
while IFS= read -r line; do
  case $line in
    +*) groups="$groups${line#?} " ;;
    -*)
      kept=' '
      for group in $groups; do
        [ "$group" = "${line#?}" ] || kept="$kept$group "
      done
      groups=$kept ;;
  esac
done
[ "$groups" = ' ' ] && exit 0
for group in $groups; do kill -TERM "-$group" 2>/dev/null; done
sleep 0.3 2>/dev/null || sleep 1
for group in $groups; do kill -KILL "-$group" 2>/dev/null; done
"#;

impl Watchdog {
    pub(crate) fn start() -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", WATCHDOG_SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir("/");
        // SAFETY: `setsid` is async-signal-safe and touches no Rust state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut process = command.spawn()?;
        let input = process
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("the watchdog has no input pipe"))?;
        Ok(Self {
            input,
            _process: process,
        })
    }

    /// One line to the watchdog. A failure means it is gone.
    pub(crate) fn tell(&mut self, line: &str) -> std::io::Result<()> {
        self.input.write_all(format!("{line}\n").as_bytes())?;
        self.input.flush()
    }
}

fn terminate(registered: &[i32]) {
    const GRACE: std::time::Duration = std::time::Duration::from_millis(300);
    if registered.is_empty() {
        return;
    }
    // SAFETY: `kill` with a negative id only signals the processes of that group.
    let alive = |group: &i32| unsafe { libc::kill(-group, 0) } == 0;
    for group in registered {
        unsafe {
            libc::kill(-group, libc::SIGTERM);
        }
    }
    let deadline = std::time::Instant::now() + GRACE;
    while registered.iter().any(alive) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for group in registered.iter().filter(|group| alive(group)) {
        unsafe {
            libc::kill(-group, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn leader(script: &str) -> std::process::Child {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]).stdin(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap()
    }

    #[test]
    fn exit_ends_registered_groups_and_their_children() {
        let mut polite = leader("sleep 30 & wait");
        let mut stubborn = leader("trap '' TERM; sleep 30 & wait");
        let _polite = register(polite.id()).unwrap();
        let _stubborn = register(stubborn.id()).unwrap();
        // Let the stubborn shell install its trap before the signal arrives.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let groups = [polite.id() as i32, stubborn.id() as i32];

        // Only this test's groups: other tests' commands are registered too.
        terminate(&groups);

        polite.wait().unwrap();
        stubborn.wait().unwrap();
        // The `sleep` children went with their leaders. They were orphaned, so
        // init reaps them on its own time: allow it a moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        for group in groups {
            while unsafe { libc::kill(-group, 0) } == 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_ne!(unsafe { libc::kill(-group, 0) }, 0, "group {group} survived");
        }
    }

    #[test]
    fn a_dropped_registration_is_forgotten() {
        let registration = register(4_000_000).unwrap();
        assert!(groups().registered.contains(&4_000_000));
        drop(registration);
        assert!(!groups().registered.contains(&4_000_000));
        assert!(register(0).is_none());
        assert!(register(1).is_none());
    }

    /// What the watchdog is for: when the pipe from its owner closes — here by
    /// dropping it, in the application by the process ending, however it ends
    /// — it ends every group still on its list, children included, and leaves
    /// alone a group that was unregistered.
    #[test]
    fn the_watchdog_ends_its_groups_when_its_owner_goes_away() {
        let mut registered = leader("trap '' TERM; sleep 30 & wait");
        let mut unregistered = leader("sleep 30");
        let mut watchdog = Watchdog::start().unwrap();
        watchdog.tell(&format!("+{}", registered.id())).unwrap();
        watchdog.tell(&format!("+{}", unregistered.id())).unwrap();
        watchdog.tell(&format!("-{}", unregistered.id())).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        drop(watchdog);

        registered.wait().unwrap();
        let group = registered.id() as i32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { libc::kill(-group, 0) } == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_ne!(unsafe { libc::kill(-group, 0) }, 0, "group {group} survived its watchdog");
        assert!(
            unregistered.try_wait().unwrap().is_none(),
            "an unregistered group is not the watchdog's to end"
        );
        let _ = unregistered.kill();
        let _ = unregistered.wait();
    }
}
