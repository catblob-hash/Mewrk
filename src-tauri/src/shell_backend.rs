//! Shell backends, and the operating systems each one is registered on.
//!
//! A machine — the host, a WSL distribution, an SSH machine — runs commands
//! through one or more shells, and Mewrk reaches it through one of them for
//! its own work too: the remote file tools and language servers are scripts
//! the machine's *agent shell* runs. So a shell is not a property of a tool; it
//! is an execution backend under a machine, and which backends a machine has is
//! found by probing it ([`crate::machine_shells`]).
//!
//! What this module fixes is the table of combinations Mewrk supports. A
//! combination is registered only when Mewrk can do everything it needs
//! through it — run the shell tool's commands, and run its own file-tool,
//! language-server and probe scripts as the machine's agent shell. A shell that
//! cannot carry those scripts is not registered on that OS, and so it is never
//! probed there and never offered as a tool:
//!
//! | OS      | Registered backends                                   | Script dialect        |
//! |---------|-------------------------------------------------------|-----------------------|
//! | Windows | PowerShell 7, Windows PowerShell 5.1, Bash (Git Bash) | PowerShell / POSIX sh |
//! | macOS   | zsh, Bash, sh                                         | POSIX sh              |
//! | Linux   | Bash, zsh, sh                                         | POSIX sh              |
//! | WSL     | Bash, zsh, sh                                         | POSIX sh              |
//!
//! Left out on purpose: `cmd.exe` (its batch language cannot express the file
//! tools' confinement checks or byte-exact reads), fish, nushell and the csh
//! family (none of them reads a POSIX script, and each would need its own copy
//! of every script), and PowerShell off Windows (its scripts here are written
//! against Windows paths). WSL is an operating system in this table, not a
//! shell: a distribution is a Linux machine the host reaches through
//! `wsl.exe`, whichever of its shells runs the command.
//!
//! PowerShell 7 (`pwsh`) and Windows PowerShell 5.1 (`powershell.exe`) are two
//! backends, not one shell with a preference between them: their languages
//! differ (pipeline chain operators, the ternary and null operators, the
//! default file encodings), so a command written for one can fail in the
//! other. Each is probed for by its own program name and offered as its own
//! tool. Mewrk's own PowerShell scripts are written against 5.1, so either can
//! be a machine's agent shell.
//!
//! The table's order is also each OS's priority, fixed rather than
//! configurable: a newly added machine's agent shell is the first backend in
//! its OS's order that the probe found, and so is the one shell tool a fresh
//! install's presets turn on for this machine.

use serde::{Deserialize, Serialize};

use crate::host_platform::{host_platform, HostPlatform};

/// A shell Mewrk can run a command in, and run its own scripts through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellBackend {
    Bash,
    Zsh,
    Sh,
    /// PowerShell 7 (`pwsh`). Registered on Windows only.
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`), which every Windows has.
    /// Registered on Windows only. Its id is the one the single PowerShell
    /// backend had before the two editions were told apart.
    #[serde(rename = "powershell")]
    WindowsPowerShell,
}

/// How a backend reads the scripts Mewrk composes for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptDialect {
    /// POSIX `sh`: bash reads it with no startup files, zsh in `sh` emulation,
    /// and `sh` itself — dash, BusyBox ash, or bash in POSIX mode.
    Posix,
    /// PowerShell, written against Windows PowerShell 5.1 so PowerShell 7 reads
    /// it too.
    PowerShell,
}

/// The operating system a machine runs.
///
/// WSL is one of these rather than a kind of shell: a distribution is a Linux
/// user space reached through `wsl.exe`, and its own shells run there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineOs {
    Windows,
    Macos,
    Linux,
    Wsl,
}

impl ShellBackend {
    /// Every backend, in the order tools are listed.
    pub const ALL: [ShellBackend; 5] = [
        Self::Bash,
        Self::Zsh,
        Self::Sh,
        Self::Pwsh,
        Self::WindowsPowerShell,
    ];

    /// The stable identifier persisted in settings and sent to the renderer.
    pub fn id(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Pwsh => "pwsh",
            Self::WindowsPowerShell => "powershell",
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|backend| backend.id() == id)
    }

    /// The name a person reads.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Pwsh => "PowerShell 7",
            Self::WindowsPowerShell => "Windows PowerShell",
        }
    }

    /// The tool that runs a command in this backend.
    pub fn tool_name(self) -> &'static str {
        self.id()
    }

    /// The backend a shell tool runs in.
    pub fn of_tool(tool_name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|backend| backend.tool_name() == tool_name)
    }

    pub fn dialect(self) -> ScriptDialect {
        match self {
            Self::Bash | Self::Zsh | Self::Sh => ScriptDialect::Posix,
            Self::Pwsh | Self::WindowsPowerShell => ScriptDialect::PowerShell,
        }
    }

    /// The language a command in this backend is written in, as a Markdown
    /// code fence names it: both PowerShell editions write PowerShell.
    pub fn language(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Sh => "sh",
            Self::Pwsh | Self::WindowsPowerShell => "powershell",
        }
    }

    /// The program named when the machine's probe has not said where it is:
    /// each shell by its own name for `PATH` to find — `pwsh` for PowerShell 7,
    /// `powershell` for Windows PowerShell 5.1.
    pub fn default_program(self) -> &'static str {
        self.id()
    }

    /// The backend `program` really is, for a record that names `self`.
    ///
    /// A record written before the two PowerShell editions were told apart
    /// says `powershell` for whichever one the machine had, PowerShell 7
    /// first; its program's name still tells them apart. Every other record
    /// already names its own backend.
    pub fn of_recorded_program(self, program: &str) -> Self {
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .to_ascii_lowercase();
        match self {
            Self::WindowsPowerShell if name == "pwsh" || name == "pwsh.exe" => Self::Pwsh,
            other => other,
        }
    }
}

impl std::fmt::Display for ShellBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.display_name())
    }
}

impl MachineOs {
    #[cfg(test)]
    pub const ALL: [MachineOs; 4] = [Self::Windows, Self::Macos, Self::Linux, Self::Wsl];

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Macos => "macOS",
            Self::Linux => "Linux",
            Self::Wsl => "WSL",
        }
    }

    /// The host's own operating system.
    pub fn host() -> Self {
        match host_platform() {
            HostPlatform::Windows => Self::Windows,
            HostPlatform::Macos => Self::Macos,
            HostPlatform::Linux => Self::Linux,
        }
    }

    /// The OS a remote agent reports (`std::env::consts::OS` on that machine).
    /// A Unix the table does not name is read as Linux: its shells are the
    /// POSIX ones, which is all the table asks of it.
    pub fn from_agent_os(os: &str) -> Self {
        match os {
            "windows" => Self::Windows,
            "macos" => Self::Macos,
            _ => Self::Linux,
        }
    }

    /// The OS `uname -s` names. MSYS, MinGW and Cygwin are POSIX layers on a
    /// Windows machine, whose files are Windows files.
    pub fn from_uname(uname: &str) -> Self {
        let uname = uname.trim();
        if uname.starts_with("MINGW") || uname.starts_with("MSYS") || uname.starts_with("CYGWIN") {
            Self::Windows
        } else if uname == "Darwin" {
            Self::Macos
        } else {
            Self::Linux
        }
    }
}

impl std::fmt::Display for MachineOs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.display_name())
    }
}

/// The registered backends of an OS, most preferred first. This is the whole
/// combination table — a backend absent here is never probed on that OS — and
/// the one priority order: Mewrk ranks each OS's shells itself rather than
/// asking the user to.
pub fn backends_for(os: MachineOs) -> &'static [ShellBackend] {
    use ShellBackend::{Bash, Pwsh, Sh, WindowsPowerShell, Zsh};
    match os {
        MachineOs::Windows => &[Pwsh, WindowsPowerShell, Bash],
        MachineOs::Macos => &[Zsh, Bash, Sh],
        MachineOs::Linux | MachineOs::Wsl => &[Bash, Zsh, Sh],
    }
}

pub fn is_registered(os: MachineOs, backend: ShellBackend) -> bool {
    backends_for(os).contains(&backend)
}

/// The program names a probe tries for a backend on an OS, best first.
pub fn probe_names(os: MachineOs, backend: ShellBackend) -> &'static [&'static str] {
    match (os, backend) {
        (MachineOs::Windows, ShellBackend::Pwsh) => &["pwsh"],
        (MachineOs::Windows, ShellBackend::WindowsPowerShell) => &["powershell"],
        (_, ShellBackend::Bash) => &["bash"],
        (_, ShellBackend::Zsh) => &["zsh"],
        (_, ShellBackend::Sh) => &["sh"],
        (_, ShellBackend::Pwsh | ShellBackend::WindowsPowerShell) => &[],
    }
}

/// The backend a machine starts with: the first one in its OS's priority
/// order that the machine has.
pub fn preferred_backend(os: MachineOs, available: &[ShellBackend]) -> Option<ShellBackend> {
    backends_for(os)
        .iter()
        .copied()
        .find(|backend| available.contains(backend))
}

/// The shell a machine's agent runs Mewrk's own scripts in: the remote file
/// tools, the language-server probe and launch, `git check-ignore`.
///
/// The host has none — its file tools act on its own filesystem directly — so
/// this belongs to a WSL distribution or an SSH machine, chosen per machine in
/// its settings (the first backend of its OS's priority order when the machine
/// was added).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentShell {
    pub backend: ShellBackend,
    /// What the machine calls it: the probed path, or the bare name when the
    /// machine has not been probed.
    pub program: String,
}

impl Default for AgentShell {
    /// Bash by name — what every remote script ran in before machines had an
    /// agent shell, and so what an unprobed machine keeps.
    fn default() -> Self {
        Self {
            backend: ShellBackend::Bash,
            program: "bash".into(),
        }
    }
}

impl AgentShell {
    pub fn new(backend: ShellBackend, program: impl Into<String>) -> Self {
        Self {
            backend,
            program: program.into(),
        }
    }

    pub fn dialect(&self) -> ScriptDialect {
        self.backend.dialect()
    }

    /// Arguments that run `script` — written in [`Self::dialect`] — here.
    pub fn script_argv(&self, script: &str) -> Vec<String> {
        script_argv(self.backend, &self.program, script)
    }
}

/// Arguments that run the shell tool's `command` in `backend` on a machine
/// other than the host, with no startup files: what a WSL distribution, an SSH
/// machine's agent, or a per-command SSH login hands the shell.
///
/// The local legs have their own invocations in `tool_executor`, which carry
/// Claude Code's session (snapshot, login shell, working-directory tracking).
pub fn remote_command_argv(backend: ShellBackend, program: &str, command: &str) -> Vec<String> {
    let mut argv = vec![program.to_owned()];
    match backend {
        ShellBackend::Bash => argv.extend(["--noprofile", "--norc", "-c"].map(String::from)),
        ShellBackend::Zsh => argv.extend(["-f", "-c"].map(String::from)),
        ShellBackend::Sh => argv.push("-c".into()),
        ShellBackend::Pwsh | ShellBackend::WindowsPowerShell => {
            argv.extend(powershell_flags());
            argv.push("-Command".into());
            argv.push(remote_powershell_command(command));
            return argv;
        }
    }
    argv.push(command.to_owned());
    argv
}

/// Arguments that run one of Mewrk's own scripts — written in
/// `backend`'s [`dialect`](ShellBackend::dialect) — as the machine's agent shell.
///
/// zsh reads the POSIX scripts in `sh` emulation, which gives them `sh` word
/// splitting and globbing; bash reads them with no startup files, as it always
/// has.
pub fn script_argv(backend: ShellBackend, program: &str, script: &str) -> Vec<String> {
    let mut argv = vec![program.to_owned()];
    match backend {
        ShellBackend::Bash => argv.extend(["--noprofile", "--norc", "-c"].map(String::from)),
        ShellBackend::Zsh => argv.extend(["--emulate", "sh", "-f", "-c"].map(String::from)),
        ShellBackend::Sh => argv.push("-c".into()),
        ShellBackend::Pwsh | ShellBackend::WindowsPowerShell => {
            argv.extend(powershell_flags());
            argv.push("-Command".into());
        }
    }
    argv.push(script.to_owned());
    argv
}

/// PowerShell's flags for an unattended run: no banner, no profile, no prompt,
/// and no execution policy standing between `-Command` and a `.ps1` the
/// command calls — the policy gates script files, never `-Command` itself.
/// `-OutputFormat Text` keeps a redirected run from switching to serialized
/// objects.
fn powershell_flags() -> Vec<String> {
    [
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-OutputFormat",
        "Text",
    ]
    .map(String::from)
    .to_vec()
}

/// The script the `powershell` tool runs on a machine other than the host.
///
/// Claude Code's prologue, as the local tool runs it, then the one thing a
/// remote run needs that a local one does not: the console output encoding
/// set to UTF-8. The host decodes a remote command's bytes, and a Windows
/// console left at its OEM code page hands it bytes it cannot read. The exit
/// status survives the way the local epilogue keeps it.
pub fn remote_powershell_command(command: &str) -> String {
    remote_powershell_session_command(command, None, None)
}

/// [`remote_powershell_command`] within its workspace's session: it first
/// moves to `start`, the directory the workspace remembers, and given a `tag`
/// reports where it ended when it succeeds — the root it started at, US, the
/// final directory, between RS bytes behind the tag, on standard output, as
/// `tool_executor::remote_session_command` describes. `Write-Host -NoNewline`
/// carries it because it is allowed in a constrained runspace and adds no line
/// break the host would have to strip.
///
/// A command that must lead its script takes neither: nothing may precede it,
/// so it starts at the root and reports nothing.
pub fn remote_powershell_session_command(command: &str, start: Option<&str>, tag: Option<&str>) -> String {
    let leads = crate::tool_executor::powershell_command_must_lead(command);
    let prologue = if leads { "" } else { REMOTE_POWERSHELL_PROLOGUE };
    let mut enter = String::new();
    let mut report = String::new();
    if !leads {
        if tag.is_some() {
            enter.push_str("$__mewrkRoot = (Get-Location).ProviderPath; ");
        }
        if let Some(start) = start {
            enter.push_str(&format!(
                "try {{ Set-Location -LiteralPath {} -ErrorAction Stop }} catch {{}}; ",
                crate::remote_shell::ps_single_quote(start)
            ));
        }
        if let Some(tag) = tag {
            report = format!(
                "; if ($_ec -eq 0) {{ Write-Host -NoNewline (([string][char]30) + {} + $__mewrkRoot + [char]31 + (Get-Location).ProviderPath + [char]30) }}\n",
                crate::remote_shell::ps_single_quote(&format!("{tag}:"))
            );
        }
    }
    format!(
        "{enter}{prologue}{command}\n\
         ; $_ec = if ($null -ne $LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }}\n\
         {report}\
         ; if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') {{ $host.SetShouldExit($_ec) }} else {{ exit $_ec }}"
    )
}

const REMOTE_POWERSHELL_PROLOGUE: &str = "try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}; $ProgressPreference = 'SilentlyContinue'; try { $PSDefaultParameterValues['Out-File:Encoding'] = 'utf8' } catch {}; if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') { try { $OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}; if ($null -ne $PSStyle) { try { $PSStyle.OutputRendering = 'PlainText' } catch {} } }; ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_registers_no_shell_that_cannot_carry_the_scripts() {
        for os in MachineOs::ALL {
            for backend in backends_for(os) {
                match backend.dialect() {
                    ScriptDialect::Posix => {}
                    // PowerShell scripts are written against Windows paths.
                    ScriptDialect::PowerShell => assert_eq!(os, MachineOs::Windows),
                }
                assert!(!probe_names(os, *backend).is_empty(), "{os} {backend}");
            }
        }
        assert!(!is_registered(MachineOs::Windows, ShellBackend::Zsh));
        assert!(!is_registered(MachineOs::Wsl, ShellBackend::WindowsPowerShell));
        assert!(!is_registered(MachineOs::Linux, ShellBackend::Pwsh));
        assert!(is_registered(MachineOs::Wsl, ShellBackend::Sh));
    }

    /// The two PowerShell editions are probed for by their own program names,
    /// never one standing in for the other.
    #[test]
    fn each_powershell_edition_is_probed_by_its_own_name() {
        assert_eq!(probe_names(MachineOs::Windows, ShellBackend::Pwsh), ["pwsh"]);
        assert_eq!(
            probe_names(MachineOs::Windows, ShellBackend::WindowsPowerShell),
            ["powershell"]
        );
        assert_eq!(ShellBackend::Pwsh.default_program(), "pwsh");
        assert_eq!(ShellBackend::WindowsPowerShell.default_program(), "powershell");
        assert_eq!(ShellBackend::Pwsh.dialect(), ScriptDialect::PowerShell);
    }

    /// A record from before the split says `powershell` for PowerShell 7 too;
    /// its program says which one it was.
    #[test]
    fn a_recorded_powershell_is_read_by_its_program() {
        use ShellBackend::{Bash, Pwsh, WindowsPowerShell};
        assert_eq!(
            WindowsPowerShell.of_recorded_program(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            Pwsh
        );
        assert_eq!(WindowsPowerShell.of_recorded_program("pwsh"), Pwsh);
        assert_eq!(WindowsPowerShell.of_recorded_program("PWSH.EXE"), Pwsh);
        assert_eq!(
            WindowsPowerShell.of_recorded_program(
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
            ),
            WindowsPowerShell
        );
        assert_eq!(WindowsPowerShell.of_recorded_program("powershell"), WindowsPowerShell);
        // Only the old shared id is reinterpreted.
        assert_eq!(Pwsh.of_recorded_program("powershell.exe"), Pwsh);
        assert_eq!(Bash.of_recorded_program("pwsh"), Bash);
    }

    #[test]
    fn every_tool_name_maps_back_to_its_backend() {
        for backend in ShellBackend::ALL {
            assert_eq!(ShellBackend::of_tool(backend.tool_name()), Some(backend));
            assert_eq!(ShellBackend::parse(backend.id()), Some(backend));
            let json = serde_json::to_string(&backend).unwrap();
            assert_eq!(json, format!("\"{}\"", backend.id()));
        }
        assert_eq!(ShellBackend::of_tool("read"), None);
        assert_eq!(serde_json::to_string(&MachineOs::Wsl).unwrap(), "\"wsl\"");
    }

    #[test]
    fn each_os_prefers_its_first_shell_the_machine_has() {
        use ShellBackend::{Bash, Pwsh, Sh, WindowsPowerShell, Zsh};
        assert_eq!(backends_for(MachineOs::Windows), &[Pwsh, WindowsPowerShell, Bash]);
        assert_eq!(backends_for(MachineOs::Linux), &[Bash, Zsh, Sh]);
        assert_eq!(backends_for(MachineOs::Macos), &[Zsh, Bash, Sh]);
        assert_eq!(preferred_backend(MachineOs::Macos, &[Sh, Bash]), Some(Bash));
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash]), Some(Bash));
        assert_eq!(preferred_backend(MachineOs::Windows, &[Bash, WindowsPowerShell]), Some(WindowsPowerShell));
        assert_eq!(preferred_backend(MachineOs::Windows, &[WindowsPowerShell, Pwsh]), Some(Pwsh));
        assert_eq!(preferred_backend(MachineOs::Linux, &[Pwsh, WindowsPowerShell]), None);
        assert_eq!(preferred_backend(MachineOs::Linux, &[]), None);
    }

    #[test]
    fn os_names_from_the_agent_and_uname() {
        assert_eq!(MachineOs::from_agent_os("windows"), MachineOs::Windows);
        assert_eq!(MachineOs::from_agent_os("freebsd"), MachineOs::Linux);
        assert_eq!(MachineOs::from_uname("MINGW64_NT-10.0-22631"), MachineOs::Windows);
        assert_eq!(MachineOs::from_uname("Darwin\n"), MachineOs::Macos);
        assert_eq!(MachineOs::from_uname("Linux"), MachineOs::Linux);
    }

    #[test]
    fn invocations_read_no_startup_files() {
        assert_eq!(
            remote_command_argv(ShellBackend::Zsh, "zsh", "echo hi"),
            ["zsh", "-f", "-c", "echo hi"]
        );
        assert_eq!(
            script_argv(ShellBackend::Zsh, "/bin/zsh", "set -f"),
            ["/bin/zsh", "--emulate", "sh", "-f", "-c", "set -f"]
        );
        assert_eq!(
            script_argv(ShellBackend::Bash, "bash", "x"),
            ["bash", "--noprofile", "--norc", "-c", "x"]
        );
        let ps = remote_command_argv(ShellBackend::Pwsh, "pwsh", "Get-Date");
        assert_eq!(ps[0], "pwsh");
        assert!(ps.contains(&"-NoProfile".to_owned()));
        assert_eq!(ps[ps.len() - 2], "-Command");
        assert!(ps.last().unwrap().contains("Get-Date"));
        assert!(ps.last().unwrap().contains("OutputEncoding"));
    }

    /// A remote PowerShell call within its workspace's session notes the root
    /// it starts at, moves to the remembered directory, and reports where it
    /// ended only after the exit status is taken and only when it is zero.
    #[test]
    fn a_remote_powershell_command_moves_first_and_reports_only_on_success() {
        let script =
            remote_powershell_session_command("Get-Date", Some(r"C:\work\app\src"), Some("mewrk-cwd-t"));
        assert!(script.starts_with(
            r"$__mewrkRoot = (Get-Location).ProviderPath; try { Set-Location -LiteralPath 'C:\work\app\src' -ErrorAction Stop } catch {}; "
        ));
        let status = script.find("$_ec = ").unwrap();
        let report = script.find("if ($_ec -eq 0) { Write-Host -NoNewline").unwrap();
        let exit = script.find("SetShouldExit").unwrap();
        assert!(status < report && report < exit, "{script}");
        assert!(script.contains("'mewrk-cwd-t:' + $__mewrkRoot + [char]31"));

        // Nothing may precede a statement that must lead, so it gets no session.
        assert_eq!(
            remote_powershell_session_command("param($x)", Some(r"C:\x"), Some("t")),
            remote_powershell_command("param($x)")
        );
        assert!(!remote_powershell_command("Get-Date").contains("__mewrkRoot"));
    }
}
