//! The skills, agent roles, MCP servers and hooks of a workspace on another
//! machine.
//!
//! A workspace on a WSL distribution or an SSH machine works like a local one:
//! its `.mewrk/skills/`, `.mewrk/agents/`, `mcp.json` and `hooks.json` are
//! the ones on that machine, and what they declare runs there — a hook command
//! in the workspace folder, a stdio server started in it. Only the reading and
//! the starting happen through the machine's transport; parsing, ids, the
//! catalog and every decision stay on this host, exactly as for a local level.
//!
//! One probe script reads everything a level holds in one round trip — the
//! two configuration files, every skill manifest, every agent role file, and
//! the machine's own environment, which is what `${VAR}` in that machine's
//! `mcp.json` expands against — in the POSIX dialect or, for a Windows machine
//! whose scripts run in PowerShell,
//! [`crate::remote_powershell::capabilities_probe`]. The answer is counted
//! blocks rather than delimited text, because a file may hold any bytes at
//! all.

use std::{collections::BTreeMap, time::Duration};

use serde_json::Value;

use crate::{
    cancel::CancelSignal,
    hooks::HookCommandOutput,
    model::HookDefinition,
    run_environment::{self, quote_remote_path, sh_single_quote, ShellRunner},
    shell_backend::ScriptDialect,
    tool_executor::ShellChild,
};

/// The largest `mcp.json` or `hooks.json` pulled over. A configuration file is
/// a few kilobytes; past this it is not one.
pub(crate) const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The largest agent role file whose bytes are pulled over. A role is a small
/// JSON file; a larger one is listed by its size and read as too large.
pub(crate) const MAX_AGENT_FILE_BYTES: u64 = 256 * 1024;

/// How long a run waits for a level to be read. The run cannot start without
/// it, and the machine is the one its workspace is on.
pub(crate) const RUN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a scan for the settings pane waits. A machine that is away costs
/// the list its rows, not the pane a minute.
pub(crate) const SCAN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the quick "is the machine there" check before a scan may take.
pub(crate) const SCAN_PRESENCE_CHECK: Duration = Duration::from_millis(700);

const HEADER: &str = "mewrk-capabilities 2";

/// One workspace level on another machine: how to reach it, and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLevel {
    /// The machine's transport, carrying the workspace's variables.
    pub runner: ShellRunner,
    /// The machine's identity (`run_environment::env_key`), which keeps the
    /// ids of two machines' identical paths apart.
    pub machine: String,
    /// The workspace root, spelled as the workspace records it.
    pub root: String,
}

/// What a probe of one level found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LevelFiles {
    /// Whether the machine runs Windows, which decides between a hook's
    /// `command` and `commandWindows`.
    pub windows: bool,
    /// The machine's environment as its scripts see it, before the
    /// workspace's variables.
    pub env: BTreeMap<String, String>,
    pub mcp: Option<ConfigFile>,
    pub hooks: Option<ConfigFile>,
    /// The `skills/` directory read (whichever spelling exists), or `None`.
    pub skills_root: Option<String>,
    pub skills: Vec<SkillFile>,
    /// The `agents/` directory read (whichever spelling exists), or `None`.
    pub agents_root: Option<String>,
    pub agents: Vec<AgentFileEntry>,
}

/// A configuration file: its path on the machine and its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// One skill folder holding a regular, unlinked `SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFile {
    /// The folder's name, which is the skill's.
    pub directory: String,
    /// The manifest's path on the machine.
    pub manifest: String,
    pub size: u64,
    /// The manifest's bytes, `None` when it is larger than a skill may be.
    pub bytes: Option<Vec<u8>>,
}

/// One agent role file: a regular, unlinked `*.json` directly inside the
/// `agents/` directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFileEntry {
    /// The file's name, extension included (`reviewer.json`).
    pub file_name: String,
    /// The file's path on the machine.
    pub path: String,
    pub size: u64,
    /// The file's bytes, `None` when it is larger than a role file may be.
    pub bytes: Option<Vec<u8>>,
}

/// A machine as a message names it: its SSH host, or its WSL distribution.
pub(crate) fn machine_label(runner: &ShellRunner) -> String {
    match runner {
        ShellRunner::Ssh { host, .. } => host.clone(),
        ShellRunner::Wsl { distro, .. } => format!("WSL ({distro})"),
        ShellRunner::Local { .. } => crate::ui_text::pick("本机", "this computer").to_owned(),
    }
}

impl RemoteLevel {
    /// The machine, as an error names it.
    fn describe(&self) -> String {
        machine_label(&self.runner)
    }

    fn dialect(&self) -> ScriptDialect {
        self.runner.script_dialect()
    }
}

/// Reads a level's configuration on its machine.
pub(crate) fn read(level: &RemoteLevel, timeout: Duration) -> Result<LevelFiles, String> {
    check_operand(&level.root, "workspace root")?;
    let script = match level.dialect() {
        ScriptDialect::Posix => posix_probe(&level.root),
        ScriptDialect::PowerShell => crate::remote_powershell::capabilities_probe(
            &level.root,
            MAX_CONFIG_BYTES,
            crate::capabilities::SKILL_READ_LIMIT,
            MAX_AGENT_FILE_BYTES,
        ),
    };
    let output = run_environment::run_remote_script(
        &level.runner,
        &script,
        None,
        timeout,
        &CancelSignal::default(),
    )
    .map_err(|error| unreadable(level, &error))?;
    if output.status != Some(0) {
        let reason = run_environment::legible_remote_reply(&output.stderr)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exit {:?}", output.status));
        return Err(unreadable(level, &reason));
    }
    parse(&output.stdout).map_err(|error| unreadable(level, &error))
}

fn unreadable(level: &RemoteLevel, reason: &str) -> String {
    crate::ui_text::ui_text!(
        "无法读取 {} 上工作区 {} 的 .mewrk：{reason}",
        "Could not read the .mewrk of workspace {} on {}: {reason}",
        level.root,
        level.describe()
    )
}

fn check_operand(text: &str, label: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err(format!(
            "The {label} is empty or contains control characters"
        ));
    }
    Ok(())
}

/// The POSIX probe. See [`crate::remote_powershell::capabilities_probe`] for
/// the format both write.
///
/// The preferred spelling of a file or directory wins whenever it exists, as
/// [`crate::capabilities::config_path_for`] has it on this host. Skill folders
/// and manifests that are links, and role files that are links or folders, are
/// passed over, as the local scan passes over them.
fn posix_probe(root: &str) -> String {
    let max_config = MAX_CONFIG_BYTES;
    let max_skill = crate::capabilities::SKILL_READ_LIMIT;
    let max_agent = MAX_AGENT_FILE_BYTES;
    format!(
        r#"cd -- {root} 2>/dev/null || {{ printf '%s\n' 'cannot enter the workspace root' >&2; exit 64; }}
R=$(pwd)
size() {{ wc -c < "$1" | tr -d ' \t'; }}
printf '%s\n' '{HEADER}'
uname -s 2>/dev/null || printf 'unknown\n'
n=$(env | wc -c | tr -d ' \t'); printf '%s\n' "$n"; env
pick() {{ if [ -e "$R/.mewrk/$1" ]; then printf '%s' "$R/.mewrk/$1"; else printf '%s' "$R/.naiword/$1"; fi; }}
emit() {{
  f=$(pick "$1")
  if [ -f "$f" ]; then
    n=$(size "$f")
    if [ "$n" -le {max_config} ]; then printf '%s\n%s\n' "$f" "$n"; cat -- "$f"; return 0; fi
  fi
  printf -- '-\n'
}}
emit mcp.json
emit hooks.json
S=$(pick skills)
nl='
'
if [ -d "$S" ]; then
  printf '%s\n' "$S"
  for d in "$S"/* "$S"/.[!.]* "$S"/..?*; do
    [ -d "$d" ] && [ ! -L "$d" ] || continue
    m="$d/SKILL.md"
    [ -f "$m" ] && [ ! -L "$m" ] || continue
    name=${{d##*/}}
    case $name in *"$nl"*) continue ;; esac
    n=$(size "$m")
    printf 'S\n%s\n%s\n%s\n' "$name" "$m" "$n"
    if [ "$n" -le {max_skill} ]; then cat -- "$m"; fi
  done
else
  printf -- '-\n'
fi
printf 'E\n'
A=$(pick agents)
if [ -d "$A" ]; then
  printf '%s\n' "$A"
  for f in "$A"/* "$A"/.[!.]* "$A"/..?*; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    name=${{f##*/}}
    case $name in *"$nl"*) continue ;; esac
    case $name in *.json) ;; *) continue ;; esac
    n=$(size "$f")
    printf 'A\n%s\n%s\n%s\n' "$name" "$f" "$n"
    if [ "$n" -le {max_agent} ]; then cat -- "$f"; fi
  done
else
  printf -- '-\n'
fi
printf 'E\n'
"#,
        root = quote_remote_path(root.trim()),
    )
}

/// A cursor over a probe's answer.
struct Answer<'a> {
    rest: &'a [u8],
}

impl<'a> Answer<'a> {
    fn line(&mut self) -> Result<String, String> {
        let end = self
            .rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(incomplete)?;
        let line = String::from_utf8_lossy(&self.rest[..end]).into_owned();
        self.rest = &self.rest[end + 1..];
        Ok(line)
    }

    fn count(&mut self) -> Result<u64, String> {
        self.line()?.trim().parse().map_err(|_| incomplete())
    }

    fn bytes(&mut self, count: u64) -> Result<Vec<u8>, String> {
        let count = usize::try_from(count).map_err(|_| incomplete())?;
        if self.rest.len() < count {
            return Err(incomplete());
        }
        let bytes = self.rest[..count].to_vec();
        self.rest = &self.rest[count..];
        Ok(bytes)
    }

    /// A path, a count and that many bytes — or `-` for none.
    fn file(&mut self) -> Result<Option<ConfigFile>, String> {
        let path = self.line()?;
        if path == "-" {
            return Ok(None);
        }
        let count = self.count()?;
        Ok(Some(ConfigFile {
            path,
            bytes: self.bytes(count)?,
        }))
    }
}

/// The answer was cut short or does not follow the format — most likely a
/// file that changed size between being measured and being read.
fn incomplete() -> String {
    "the machine's answer was incomplete (a file may have changed while it was read); try again"
        .to_owned()
}

fn parse(bytes: &[u8]) -> Result<LevelFiles, String> {
    let mut answer = Answer { rest: bytes };
    if answer.line()? != HEADER {
        return Err(incomplete());
    }
    let system = answer.line()?;
    let windows = system == "windows"
        || ["MINGW", "MSYS", "CYGWIN"]
            .iter()
            .any(|prefix| system.to_ascii_uppercase().starts_with(prefix));
    let env_size = answer.count()?;
    let env = String::from_utf8_lossy(&answer.bytes(env_size)?)
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
    let mcp = answer.file()?;
    let hooks = answer.file()?;
    let skills_root = Some(answer.line()?).filter(|line| line != "-");
    let mut skills = Vec::new();
    loop {
        match answer.line()?.as_str() {
            "E" => break,
            "S" => {
                let directory = answer.line()?;
                let manifest = answer.line()?;
                let size = answer.count()?;
                let bytes = if size <= crate::capabilities::SKILL_READ_LIMIT {
                    Some(answer.bytes(size)?)
                } else {
                    None
                };
                skills.push(SkillFile {
                    directory,
                    manifest,
                    size,
                    bytes,
                });
            }
            _ => return Err(incomplete()),
        }
    }
    let agents_root = Some(answer.line()?).filter(|line| line != "-");
    let mut agents = Vec::new();
    loop {
        match answer.line()?.as_str() {
            "E" => break,
            "A" => {
                let file_name = answer.line()?;
                let path = answer.line()?;
                let size = answer.count()?;
                let bytes = if size <= MAX_AGENT_FILE_BYTES {
                    Some(answer.bytes(size)?)
                } else {
                    None
                };
                agents.push(AgentFileEntry {
                    file_name,
                    path,
                    size,
                    bytes,
                });
            }
            _ => return Err(incomplete()),
        }
    }
    Ok(LevelFiles {
        windows,
        env,
        mcp,
        hooks,
        skills_root,
        skills,
        agents_root,
        agents,
    })
}

/// Creates `relative` (`.mewrk` or `.mewrk/skills`) under the workspace root
/// on its machine and returns the directory's path there.
pub(crate) fn ensure_directory(level: &RemoteLevel, relative: &str) -> Result<String, String> {
    check_operand(&level.root, "workspace root")?;
    let script = match level.dialect() {
        ScriptDialect::Posix => format!(
            "cd -- {} 2>/dev/null || {{ printf '%s\\n' 'cannot enter the workspace root' >&2; exit 64; }}\n\
             mkdir -p -- {rel} || exit 73\n\
             cd -- {rel} && pwd\n",
            quote_remote_path(level.root.trim()),
            rel = sh_single_quote(relative)
        ),
        ScriptDialect::PowerShell => crate::remote_powershell::ensure_directory(&level.root, relative),
    };
    let output = run(level, &script, None)?;
    let path = String::from_utf8_lossy(&output).trim().to_owned();
    if path.is_empty() {
        return Err(failed(level, "the machine did not say where the folder is"));
    }
    Ok(path)
}

/// Replaces a configuration file on the machine with `contents`, through a
/// temporary file beside it so an interrupted write leaves the original.
pub(crate) fn replace_file(level: &RemoteLevel, path: &str, contents: &str) -> Result<(), String> {
    check_operand(path, "path")?;
    let script = match level.dialect() {
        ScriptDialect::Posix => format!(
            "f={}\nt=\"$f.mewrk-$$.tmp\"\ncat > \"$t\" || {{ rm -f -- \"$t\"; exit 73; }}\nmv -f -- \"$t\" \"$f\" || {{ rm -f -- \"$t\"; exit 73; }}\n",
            sh_single_quote(path)
        ),
        ScriptDialect::PowerShell => crate::remote_powershell::replace_file(path),
    };
    run(level, &script, Some(contents.as_bytes())).map(|_| ())
}

/// The exit status [`create_file`]'s script answers with when its path is
/// taken.
const PATH_TAKEN: i32 = 75;

/// Creates the file `path` on the machine holding `contents`, unless the path
/// is taken in any form — a file, a folder, a link, a dangling one included —
/// in which case nothing is written and the answer is `false`, for the caller
/// to try another name. The contents go to a temporary file beside it first,
/// so the file never appears half written; the move is one the machine
/// refuses to make over anything (or, in the POSIX dialect, the path is
/// checked again by the same script just before it): a folder by that name
/// never receives the file.
pub(crate) fn create_file(level: &RemoteLevel, path: &str, contents: &str) -> Result<bool, String> {
    check_operand(path, "path")?;
    let output = run_environment::run_remote_script(
        &level.runner,
        &create_file_script(level.dialect(), path),
        Some(contents.as_bytes()),
        RUN_TIMEOUT,
        &CancelSignal::default(),
    )
    .map_err(|error| failed(level, &error))?;
    match output.status {
        Some(0) => Ok(true),
        Some(PATH_TAKEN) => Ok(false),
        status => {
            let reason = run_environment::legible_remote_reply(&output.stderr)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("exit {status:?}"));
            Err(failed(level, &reason))
        }
    }
}

fn create_file_script(dialect: ScriptDialect, path: &str) -> String {
    match dialect {
        ScriptDialect::Posix => format!(
            "f={}\n\
             taken() {{ [ -e \"$f\" ] || [ -L \"$f\" ]; }}\n\
             if taken; then exit {PATH_TAKEN}; fi\n\
             t=\"$f.mewrk-$$.tmp\"\n\
             cat > \"$t\" || {{ rm -f -- \"$t\"; exit 73; }}\n\
             if taken; then rm -f -- \"$t\"; exit {PATH_TAKEN}; fi\n\
             mv -f -- \"$t\" \"$f\" || {{ rm -f -- \"$t\"; exit 73; }}\n",
            sh_single_quote(path)
        ),
        ScriptDialect::PowerShell => crate::remote_powershell::create_file(path, PATH_TAKEN),
    }
}

/// Deletes the skill folder `directory` of `skills_root` on the machine, for
/// good: a remote machine has no Trash. Anything that is not a plain folder
/// directly inside the skills directory is refused.
pub(crate) fn remove_skill(
    level: &RemoteLevel,
    skills_root: &str,
    directory: &str,
) -> Result<(), String> {
    check_operand(skills_root, "skills directory")?;
    if !crate::skills::directory_name_is_valid(directory)
        || directory.contains(['/', '\\'])
        || matches!(directory, "." | "..")
    {
        return Err(crate::ui_text::pick(
            "这个技能文件夹的名字不能删除",
            "A skill folder by that name cannot be deleted",
        )
        .to_owned());
    }
    let script = match level.dialect() {
        ScriptDialect::Posix => format!(
            "d={}/{}\n[ -d \"$d\" ] && [ ! -L \"$d\" ] || exit 66\nrm -rf -- \"$d\"\n",
            sh_single_quote(skills_root),
            sh_single_quote(directory)
        ),
        ScriptDialect::PowerShell => crate::remote_powershell::remove_skill(skills_root, directory),
    };
    run(level, &script, None).map(|_| ())
}

/// Deletes the one regular file `path` on the machine, for good: a remote
/// machine has no Trash. A file that is already gone is not an error; a folder
/// or a link where the file should be is refused.
pub(crate) fn remove_file(level: &RemoteLevel, path: &str) -> Result<(), String> {
    check_operand(path, "path")?;
    run(level, &remove_file_script(level.dialect(), path), None).map(|_| ())
}

fn remove_file_script(dialect: ScriptDialect, path: &str) -> String {
    match dialect {
        ScriptDialect::Posix => format!(
            "f={}\nif [ -e \"$f\" ] || [ -L \"$f\" ]; then [ -f \"$f\" ] && [ ! -L \"$f\" ] || exit 66; fi\nrm -f -- \"$f\"\n",
            sh_single_quote(path)
        ),
        ScriptDialect::PowerShell => crate::remote_powershell::remove_file(path),
    }
}

/// Runs a short host-authored script on the level's machine and returns its
/// standard output, or why it failed.
fn run(level: &RemoteLevel, script: &str, stdin: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let output = run_environment::run_remote_script(
        &level.runner,
        script,
        stdin,
        RUN_TIMEOUT,
        &CancelSignal::default(),
    )
    .map_err(|error| failed(level, &error))?;
    if output.status != Some(0) {
        let reason = run_environment::legible_remote_reply(&output.stderr)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("exit {:?}", output.status));
        return Err(failed(level, &reason));
    }
    Ok(output.stdout)
}

fn failed(level: &RemoteLevel, reason: &str) -> String {
    crate::ui_text::ui_text!(
        "在 {} 上操作失败：{reason}",
        "The operation on {} failed: {reason}",
        level.describe()
    )
}

/// Where a hook declared by a workspace on another machine runs: that machine,
/// in the workspace folder (or the conversation's worktree of it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookPlace {
    pub runner: ShellRunner,
    /// The folder the command runs in, on that machine.
    pub cwd: String,
    /// Whether that machine runs Windows, which picks `commandWindows`.
    pub windows: bool,
}

/// Runs one hook command on its machine: the event on standard input,
/// `MEWRK_HOOK_EVENT` and `CLAUDE_PROJECT_DIR` set on top of the machine's
/// environment and the workspace's variables (which the transport applies),
/// within the hook's timeout, and stopped as soon as `cancellation` is raised.
pub(crate) fn run_hook(
    place: &HookPlace,
    hook: &HookDefinition,
    input: &Value,
    remaining_budget: Duration,
    cancellation: &CancelSignal,
) -> Result<HookCommandOutput, String> {
    if cancellation.cancelled() {
        return Err("The run or task was stopped; the hook command was not started".into());
    }
    let script = hook_script(place, hook)?;
    // The event says where the hook runs, and this one runs in a folder on
    // its machine, not in the run's folder here.
    let mut input = input.clone();
    if let Some(fields) = input.as_object_mut() {
        fields.insert("cwd".to_owned(), Value::String(place.cwd.clone()));
    }
    let payload = serde_json::to_vec(&input)
        .map_err(|error| format!("Could not serialize hook input: {error}"))?;
    let timeout = Duration::from_millis(hook.timeout_ms)
        .min(Duration::from_secs(10 * 60))
        .min(remaining_budget);
    let output = run_environment::run_remote_script(
        &place.runner,
        &script,
        Some(&payload),
        timeout,
        cancellation,
    )
    .map_err(|error| {
        if cancellation.cancelled() {
            "The run or task was stopped; the hook command was terminated".to_owned()
        } else if error.contains("did not finish within") {
            format!("Hook exceeded its {} ms timeout limit", timeout.as_millis())
        } else {
            error
        }
    })?;
    Ok(HookCommandOutput {
        code: output.status.unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: output.stderr,
    })
}

/// The script that runs one hook's command on its machine: into the folder,
/// the hook's variables exported, then the command as written — `command`, or
/// `commandWindows` on a Windows machine — in the machine's own shell.
fn hook_script(place: &HookPlace, hook: &HookDefinition) -> Result<String, String> {
    let command = if place.windows {
        hook.command_windows
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&hook.command)
    } else {
        hook.command.as_str()
    };
    if command.trim().is_empty() {
        return Err("Hook command must not be empty".into());
    }
    check_operand(&place.cwd, "hook folder")?;
    let event = crate::hooks::event_label(hook.event);
    let env = [
        (crate::hooks::HOOK_EVENT_ENV.to_owned(), event.to_owned()),
        (crate::hooks::LEGACY_HOOK_EVENT_ENV.to_owned(), event.to_owned()),
        (crate::hooks::PROJECT_DIR_ENV.to_owned(), place.cwd.clone()),
    ];
    let script = match place.runner.script_dialect() {
        ScriptDialect::Posix => {
            let mut script = format!(
                "cd -- {} 2>/dev/null || {{ printf '%s\\n' 'Hook workspace does not exist or cannot be accessed' >&2; exit 64; }}\n",
                quote_remote_path(place.cwd.trim())
            );
            for (name, value) in &env {
                script.push_str(&format!("export {name}={}\n", sh_single_quote(value)));
            }
            script.push_str(command);
            script.push('\n');
            script
        }
        ScriptDialect::PowerShell => crate::remote_powershell::hook_command(&place.cwd, &env, command),
    };
    Ok(script)
}

/// The script that starts a stdio MCP server on another machine, in `cwd`
/// there, with `env` on top of the machine's environment and the workspace's
/// variables: every fragment single-quoted, since all of it comes from an
/// `mcp.json` a repository may have shipped, and the command checked before
/// anything runs, so a server that is not installed there says so by name
/// rather than as "the server exited".
pub(crate) fn stdio_launch_script(
    runner: &ShellRunner,
    cwd: &str,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<String, String> {
    check_operand(cwd, "working directory")?;
    check_operand(command, "command")?;
    if args.iter().any(|argument| argument.chars().any(char::is_control)) {
        return Err("The server's arguments contain control characters".into());
    }
    for (name, value) in env {
        run_environment::validate_env_var_name(name)?;
        if value.chars().any(char::is_control) {
            return Err("The server's environment values contain control characters".into());
        }
    }
    if runner.script_dialect() == ScriptDialect::PowerShell {
        let env = env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        return Ok(crate::remote_powershell::lsp_launch(cwd, command, args, &env));
    }
    let mut script = format!(
        "cd -- {} || exit 64\n",
        quote_remote_path(cwd.trim())
    );
    script.push_str(&format!(
        "command -v {cmd} >/dev/null 2>&1 || {{ printf '%s\\n' {message} >&2; exit 127; }}\n",
        cmd = sh_single_quote(command),
        message = sh_single_quote(&format!(
            "{command}: command not found on the remote machine's PATH"
        )),
    ));
    script.push_str("exec ");
    if !env.is_empty() {
        script.push_str("env ");
        for (name, value) in env {
            script.push_str(&sh_single_quote(&format!("{name}={value}")));
            script.push(' ');
        }
    }
    script.push_str(&sh_single_quote(command));
    for argument in args {
        script.push(' ');
        script.push_str(&sh_single_quote(argument));
    }
    script.push('\n');
    Ok(script)
}

/// Starts `script` on the machine `runner` reaches with its standard streams
/// piped back: through the machine's agent when it has one, so the server
/// belongs to the agent and survives a dropped link, otherwise as the child of
/// a `wsl.exe` or `ssh` wrapper here.
pub(crate) fn spawn(runner: &ShellRunner, script: &str, label: &str) -> Result<ShellChild, String> {
    let argv = runner
        .agent_shell()
        .cloned()
        .unwrap_or_default()
        .script_argv(script);
    if let Some(spawned) = crate::remote_link::spawn(
        runner,
        argv,
        None,
        remote_agent::protocol::StdinMode::Pipe,
        label,
    ) {
        return spawned.map(|child| ShellChild::Remote {
            child,
            killed: false,
        });
    }
    run_environment::spawn_remote_script(runner, script, true).map(ShellChild::Local)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(text: &[u8]) -> Vec<u8> {
        let mut block = format!("{}\n", text.len()).into_bytes();
        block.extend_from_slice(text);
        block
    }

    /// An answer up to its agents section: the environment, an `mcp.json`, no
    /// `hooks.json`, and a skills section that found no directory.
    fn answer_before_agents() -> Vec<u8> {
        let mut answer = format!("{HEADER}\nLinux\n").into_bytes();
        answer.extend(counted(b"HOME=/home/me\n"));
        answer.extend(b"/srv/app/.mewrk/mcp.json\n".iter());
        answer.extend(counted(br#"{"mcpServers":{}}"#));
        answer.extend(b"-\n".iter());
        answer.extend(b"-\nE\n".iter());
        answer
    }

    /// The format both probes write, read back: environment, both files, a
    /// readable skill and one too large to carry, then the role files the same
    /// way.
    #[test]
    fn a_probe_answer_reads_back_into_its_files() {
        let mut answer = format!("{HEADER}\nLinux\n").into_bytes();
        answer.extend(counted(b"HOME=/home/me\nTOKEN=a=b\n"));
        answer.extend(b"/srv/app/.mewrk/mcp.json\n".iter());
        answer.extend(counted(br#"{"mcpServers":{}}"#));
        answer.extend(b"-\n".iter());
        answer.extend(b"/srv/app/.mewrk/skills\n".iter());
        answer.extend(b"S\nreview\n/srv/app/.mewrk/skills/review/SKILL.md\n".iter());
        answer.extend(counted(b"# Review\nline\n"));
        answer.extend(format!("S\nhuge\n/srv/app/.mewrk/skills/huge/SKILL.md\n{}\n", crate::capabilities::SKILL_READ_LIMIT + 1).as_bytes());
        answer.extend(b"E\n".iter());
        answer.extend(b"/srv/app/.mewrk/agents\n".iter());
        answer.extend(b"A\nreviewer.json\n/srv/app/.mewrk/agents/reviewer.json\n".iter());
        answer.extend(counted(br#"{"name":"reviewer"}"#));
        answer.extend(
            format!(
                "A\nhuge.json\n/srv/app/.mewrk/agents/huge.json\n{}\n",
                MAX_AGENT_FILE_BYTES + 1
            )
            .as_bytes(),
        );
        answer.extend(b"E\n".iter());

        let files = parse(&answer).unwrap();
        assert!(!files.windows);
        assert_eq!(files.env["HOME"], "/home/me");
        assert_eq!(files.env["TOKEN"], "a=b");
        let mcp = files.mcp.unwrap();
        assert_eq!(mcp.path, "/srv/app/.mewrk/mcp.json");
        assert_eq!(mcp.bytes, br#"{"mcpServers":{}}"#);
        assert!(files.hooks.is_none());
        assert_eq!(files.skills_root.as_deref(), Some("/srv/app/.mewrk/skills"));
        assert_eq!(files.skills.len(), 2);
        assert_eq!(files.skills[0].directory, "review");
        assert_eq!(files.skills[0].bytes.as_deref(), Some(&b"# Review\nline\n"[..]));
        assert!(files.skills[1].bytes.is_none());
        assert_eq!(files.agents_root.as_deref(), Some("/srv/app/.mewrk/agents"));
        assert_eq!(
            files.agents,
            vec![
                AgentFileEntry {
                    file_name: "reviewer.json".into(),
                    path: "/srv/app/.mewrk/agents/reviewer.json".into(),
                    size: 19,
                    bytes: Some(br#"{"name":"reviewer"}"#.to_vec()),
                },
                AgentFileEntry {
                    file_name: "huge.json".into(),
                    path: "/srv/app/.mewrk/agents/huge.json".into(),
                    size: MAX_AGENT_FILE_BYTES + 1,
                    bytes: None,
                },
            ]
        );

        // Cut short anywhere, it is refused rather than half read.
        assert!(parse(&answer[..answer.len() - 3]).is_err());
        assert!(parse(&answer[..answer.len() - 2]).is_err());
        assert!(parse(b"something else\n").is_err());
        // An answer in the format from before roles were read is refused too.
        let old_header = String::from_utf8_lossy(&answer).replacen("mewrk-capabilities 2", "mewrk-capabilities 1", 1);
        assert!(parse(old_header.as_bytes()).is_err());
    }

    /// A machine without an `agents/` directory answers `-` and nothing else.
    #[test]
    fn an_answer_without_an_agents_directory_has_no_roles() {
        let mut answer = answer_before_agents();
        answer.extend(b"-\nE\n".iter());
        let files = parse(&answer).unwrap();
        assert!(files.agents_root.is_none());
        assert!(files.agents.is_empty());
        assert_eq!(files.mcp.unwrap().path, "/srv/app/.mewrk/mcp.json");

        // The directory may be there and hold nothing.
        let mut answer = answer_before_agents();
        answer.extend(b"/srv/app/.mewrk/agents\nE\n".iter());
        let files = parse(&answer).unwrap();
        assert_eq!(files.agents_root.as_deref(), Some("/srv/app/.mewrk/agents"));
        assert!(files.agents.is_empty());

        // The section is not optional: an answer that ends before it is cut short.
        assert!(parse(&answer_before_agents()).is_err());
        // Nor may a skill record turn up where a role record belongs.
        let mut answer = answer_before_agents();
        answer.extend(b"-\nS\nx\ny\n0\nE\n".iter());
        assert!(parse(&answer).is_err());
    }

    /// The legacy `.naiword` spelling is reported as it was found, and a file
    /// name may hold spaces and any Unicode — it is a line, not a token — as
    /// may the file's bytes, which are counted rather than delimited.
    #[test]
    fn role_files_keep_their_spelling_names_and_bytes() {
        let body = "{\"name\":\"评审 \u{1f431}\"}\r\n\u{0}\n".as_bytes();
        let mut answer = answer_before_agents();
        answer.extend(b"/srv/app/.naiword/agents\n".iter());
        answer.extend("A\n评审 my role.json\n/srv/app/.naiword/agents/评审 my role.json\n".as_bytes());
        answer.extend(counted(body));
        answer.extend(b"A\n.json\n/srv/app/.naiword/agents/.json\n".iter());
        answer.extend(counted(b""));
        answer.extend(b"E\n".iter());
        let files = parse(&answer).unwrap();
        assert_eq!(files.agents_root.as_deref(), Some("/srv/app/.naiword/agents"));
        assert_eq!(files.agents.len(), 2);
        assert_eq!(files.agents[0].file_name, "评审 my role.json");
        assert_eq!(files.agents[0].path, "/srv/app/.naiword/agents/评审 my role.json");
        assert_eq!(files.agents[0].size, body.len() as u64);
        assert_eq!(files.agents[0].bytes.as_deref(), Some(body));
        assert_eq!(files.agents[1].file_name, ".json");
        assert_eq!(files.agents[1].bytes.as_deref(), Some(&b""[..]));
    }

    /// A role file at the limit is carried, one byte past it is only measured,
    /// and a carried file whose bytes fall short is a cut-short answer.
    #[test]
    fn a_role_file_is_carried_up_to_the_limit() {
        let mut answer = answer_before_agents();
        answer.extend(b"/a\nA\nat.json\n/a/at.json\n".iter());
        answer.extend(format!("{MAX_AGENT_FILE_BYTES}\n").as_bytes());
        answer.extend(vec![b'x'; MAX_AGENT_FILE_BYTES as usize]);
        answer.extend(b"A\npast.json\n/a/past.json\n".iter());
        answer.extend(format!("{}\n", MAX_AGENT_FILE_BYTES + 1).as_bytes());
        answer.extend(b"E\n".iter());
        let files = parse(&answer).unwrap();
        assert_eq!(files.agents[0].bytes.as_ref().map(Vec::len), Some(MAX_AGENT_FILE_BYTES as usize));
        assert!(files.agents[1].bytes.is_none());
        assert_eq!(files.agents[1].size, MAX_AGENT_FILE_BYTES + 1);

        let mut short = answer_before_agents();
        short.extend(b"/a\nA\nshort.json\n/a/short.json\n10\nabc\nE\n".iter());
        assert!(parse(&short).is_err());
    }

    /// The PowerShell probe and the POSIX probe are one format: the same
    /// header, and the agents section after the skills section.
    #[test]
    fn both_probes_write_the_header_and_the_agents_section() {
        let posix = posix_probe("/srv/app");
        assert!(posix.contains(&format!("'{HEADER}'")), "{posix}");
        assert!(posix.contains(&format!("-le {MAX_AGENT_FILE_BYTES} ]")), "{posix}");
        assert!(posix.find("pick skills").unwrap() < posix.find("pick agents").unwrap());
        let powershell = crate::remote_powershell::capabilities_probe(
            "C:/app",
            MAX_CONFIG_BYTES,
            crate::capabilities::SKILL_READ_LIMIT,
            MAX_AGENT_FILE_BYTES,
        );
        assert!(powershell.contains(&format!("Out-Line '{HEADER}'")), "{powershell}");
        assert!(powershell.contains(&format!("$f.Length -le {MAX_AGENT_FILE_BYTES}")), "{powershell}");
        assert!(powershell.find("Pick 'skills'").unwrap() < powershell.find("Pick 'agents'").unwrap());
    }

    /// The POSIX probe run by a local shell over a real directory, standing
    /// in for the machine: it finds what the local scan would.
    #[cfg(unix)]
    #[test]
    fn the_posix_probe_reads_a_workspace_like_the_local_scan() {
        let Some(sh) = crate::run_environment::local_bash_candidates().into_iter().next() else {
            eprintln!("skipped: no local bash");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".mewrk");
        std::fs::create_dir_all(config.join("skills").join("review")).unwrap();
        std::fs::write(config.join("skills").join("review").join("SKILL.md"), "# Review\n").unwrap();
        std::fs::create_dir_all(config.join("skills").join("empty")).unwrap();
        std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
        std::fs::write(temp.path().join("elsewhere").join("SKILL.md"), "# Linked\n").unwrap();
        std::os::unix::fs::symlink(temp.path().join("elsewhere"), config.join("skills").join("linked")).unwrap();
        std::fs::write(config.join("hooks.json"), "\u{feff}{\"hooks\":{}}").unwrap();
        let script = posix_probe(&temp.path().to_string_lossy());
        let output = std::process::Command::new(sh)
            .args(["--noprofile", "--norc", "-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let files = parse(&output.stdout).unwrap();
        assert!(files.mcp.is_none());
        let hooks = files.hooks.unwrap();
        assert!(hooks.path.ends_with(".mewrk/hooks.json"), "{}", hooks.path);
        assert_eq!(hooks.bytes, "\u{feff}{\"hooks\":{}}".as_bytes());
        assert_eq!(files.skills.len(), 1, "{:?}", files.skills);
        assert_eq!(files.skills[0].directory, "review");
        assert!(files.env.contains_key("PATH"));
    }

    /// What a role directory holds, laid out the way both probe tests read it
    /// back: two role files (one with spaces and Unicode in its name, one
    /// past the limit) among everything a probe must pass over — another
    /// extension, a folder and links named like role files, and a nested file.
    #[cfg(any(unix, windows))]
    fn lay_out_role_directory(agents: &std::path::Path, outside: &std::path::Path) {
        std::fs::create_dir_all(agents.join("nested.json")).unwrap();
        std::fs::write(agents.join("nested.json").join("inner.json"), "{}").unwrap();
        std::fs::write(agents.join("reviewer.json"), "{\"name\":\"reviewer\"}").unwrap();
        std::fs::write(agents.join("评审 my role.json"), "{\"name\":\"评审\"}\n").unwrap();
        std::fs::write(agents.join("huge.json"), vec![b' '; MAX_AGENT_FILE_BYTES as usize + 1]).unwrap();
        std::fs::write(agents.join("notes.txt"), "not a role").unwrap();
        std::fs::write(outside.join("elsewhere.json"), "{\"name\":\"linked\"}").unwrap();
    }

    /// What `lay_out_role_directory` must read back as, whichever probe ran.
    #[cfg(any(unix, windows))]
    fn assert_role_directory_read(files: &LevelFiles) {
        let mut names = files.agents.iter().map(|entry| entry.file_name.as_str()).collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["huge.json", "reviewer.json", "评审 my role.json"], "{:?}", files.agents);
        let entry = |name: &str| files.agents.iter().find(|entry| entry.file_name == name).unwrap();
        assert_eq!(entry("reviewer.json").bytes.as_deref(), Some(&b"{\"name\":\"reviewer\"}"[..]));
        assert_eq!(entry("reviewer.json").size, 19);
        assert_eq!(entry("评审 my role.json").bytes.as_deref(), Some("{\"name\":\"评审\"}\n".as_bytes()));
        assert_eq!(entry("huge.json").size, MAX_AGENT_FILE_BYTES + 1);
        assert!(entry("huge.json").bytes.is_none());
        let root = files.agents_root.as_deref().unwrap();
        for entry in &files.agents {
            assert!(entry.path.ends_with(&format!("/{}", entry.file_name)), "{}", entry.path);
            assert!(entry.path.starts_with(root), "{} / {root}", entry.path);
        }
    }

    /// The POSIX probe over a real `agents/` directory: only direct, regular,
    /// unlinked `*.json` files, with the bytes of those within the limit.
    #[cfg(unix)]
    #[test]
    fn the_posix_probe_reads_role_files_and_passes_over_what_is_not_one() {
        let Some(sh) = crate::run_environment::local_bash_candidates().into_iter().next() else {
            eprintln!("skipped: no local bash");
            return;
        };
        let probe = |root: &std::path::Path| {
            let output = std::process::Command::new(&sh)
                .args(["--noprofile", "--norc", "-c", &posix_probe(&root.to_string_lossy())])
                .output()
                .unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            parse(&output.stdout).unwrap()
        };
        let temp = tempfile::tempdir().unwrap();

        // No directory, no section.
        let files = probe(temp.path());
        assert!(files.agents_root.is_none() && files.agents.is_empty());

        let agents = temp.path().join(".mewrk").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let outside = temp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        lay_out_role_directory(&agents, &outside);
        std::os::unix::fs::symlink(outside.join("elsewhere.json"), agents.join("linked.json")).unwrap();
        std::os::unix::fs::symlink(&outside, agents.join("linked-folder.json")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("nowhere"), agents.join("dangling.json")).unwrap();
        let files = probe(temp.path());
        assert_role_directory_read(&files);
        assert!(files.agents_root.as_deref().unwrap().ends_with("/.mewrk/agents"));

        // The legacy spelling is read only while the preferred one is absent.
        let legacy = tempfile::tempdir().unwrap();
        let old = legacy.path().join(".naiword").join("agents");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("old.json"), "{}").unwrap();
        let files = probe(legacy.path());
        assert!(files.agents_root.as_deref().unwrap().ends_with("/.naiword/agents"));
        assert_eq!(files.agents.len(), 1);
        assert_eq!(files.agents[0].file_name, "old.json");
        let preferred = legacy.path().join(".mewrk").join("agents");
        std::fs::create_dir_all(&preferred).unwrap();
        let files = probe(legacy.path());
        assert!(files.agents_root.as_deref().unwrap().ends_with("/.mewrk/agents"));
        assert!(files.agents.is_empty());
    }

    /// The PowerShell probe run by a real Windows PowerShell over a real
    /// directory: it reads the role files as the POSIX probe does, and an
    /// extension is compared without regard to case.
    #[cfg(windows)]
    #[test]
    fn the_powershell_probe_reads_role_files_like_the_posix_probe() {
        let Some(powershell) = windows_powershell() else {
            eprintln!("skipped: no Windows PowerShell");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let probe = |root: &std::path::Path| {
            let script = crate::remote_powershell::capabilities_probe(
                &root.to_string_lossy().replace('\\', "/"),
                MAX_CONFIG_BYTES,
                crate::capabilities::SKILL_READ_LIMIT,
                MAX_AGENT_FILE_BYTES,
            );
            // Windows PowerShell reads a script file without a byte order mark
            // as ANSI, so the file name above would not survive.
            let file = root.with_extension("probe.ps1");
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(script.as_bytes());
            std::fs::write(&file, bytes).unwrap();
            let output = std::process::Command::new(&powershell)
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&file)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(&file);
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            parse(&output.stdout).unwrap()
        };

        let files = probe(temp.path());
        assert!(files.windows);
        assert!(files.agents_root.is_none() && files.agents.is_empty());

        let agents = temp.path().join(".mewrk").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let outside = temp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        lay_out_role_directory(&agents, &outside);
        std::fs::write(agents.join("Upper.JSON"), "{}").unwrap();
        // A link needs a privilege a developer machine may not grant.
        let linked = std::os::windows::fs::symlink_file(outside.join("elsewhere.json"), agents.join("linked.json")).is_ok();
        let mut files = probe(temp.path());
        let upper = files.agents.iter().position(|entry| entry.file_name == "Upper.JSON").unwrap();
        assert_eq!(files.agents.remove(upper).bytes.as_deref(), Some(&b"{}"[..]));
        assert_role_directory_read(&files);
        assert!(files.agents_root.as_deref().unwrap().ends_with("/.mewrk/agents"));
        assert!(files.agents.iter().all(|entry| entry.file_name != "linked.json"), "linked={linked}");

        let legacy = tempfile::tempdir().unwrap();
        let old = legacy.path().join(".naiword").join("agents");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("old.json"), "{}").unwrap();
        let files = probe(legacy.path());
        assert!(files.agents_root.as_deref().unwrap().ends_with("/.naiword/agents"));
        assert_eq!(files.agents.len(), 1);
        std::fs::create_dir_all(legacy.path().join(".mewrk").join("agents")).unwrap();
        assert!(probe(legacy.path()).agents.is_empty());
    }

    #[cfg(windows)]
    fn windows_powershell() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("SystemRoot")?;
        let path = std::path::Path::new(&root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        path.is_file().then_some(path)
    }

    /// A hook's script, run by a local shell standing in for the machine:
    /// it runs in the folder, sees the event on stdin and its variables, picks
    /// `command` on a machine that is not Windows, and exits with the command.
    #[cfg(unix)]
    #[test]
    fn a_hook_script_runs_the_command_in_the_folder_with_its_variables() {
        let Some(sh) = crate::run_environment::local_bash_candidates().into_iter().next() else {
            eprintln!("skipped: no local bash");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let place = HookPlace {
            runner: ShellRunner::Wsl {
                distro: "Ubuntu".into(),
                env: BTreeMap::new(),
                agent_shell: Default::default(),
            },
            cwd: temp.path().to_string_lossy().into_owned(),
            windows: false,
        };
        let hook = HookDefinition {
            id: "hook".into(),
            name: "check".into(),
            event: crate::model::HookEvent::Stop,
            matcher: None,
            command: r#"read -r event; printf '%s|%s|%s|%s' "$(pwd)" "$MEWRK_HOOK_EVENT" "$CLAUDE_PROJECT_DIR" "$event"; exit 2"#.into(),
            command_windows: Some("echo windows".into()),
            status_message: None,
            enabled: true,
            timeout_ms: 5_000,
            on_machine: None,
            workspace_key: None,
            member: None,
            local_place: None,
        };
        let script = hook_script(&place, &hook).unwrap();
        let mut child = std::process::Command::new(sh)
            .args(["--noprofile", "--norc", "-c", &script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"{\"hook_event_name\":\"Stop\"}\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        let text = String::from_utf8(output.stdout).unwrap();
        let fields = text.split('|').collect::<Vec<_>>();
        assert!(
            fields[0].ends_with(temp.path().file_name().unwrap().to_str().unwrap()),
            "{text}"
        );
        assert_eq!(fields[1], "Stop");
        assert_eq!(fields[2], place.cwd);
        assert_eq!(fields[3], "{\"hook_event_name\":\"Stop\"}");

        let windows = HookPlace {
            windows: true,
            ..place.clone()
        };
        assert!(hook_script(&windows, &hook).unwrap().contains("echo windows"));
    }

    fn wsl_level() -> RemoteLevel {
        RemoteLevel {
            runner: ShellRunner::Wsl {
                distro: "Ubuntu".into(),
                env: BTreeMap::new(),
                agent_shell: Default::default(),
            },
            machine: "wsl:Ubuntu".into(),
            root: "/srv/app".into(),
        }
    }

    /// An empty path, or one with a control character, is refused before
    /// anything is run on the machine.
    #[test]
    fn remove_file_refuses_a_path_that_cannot_be_an_operand() {
        let level = wsl_level();
        for path in [
            "",
            "   ",
            "/srv/app/.mewrk/agents/a.json\nrm -rf /",
            "/srv/app/a\u{0}.json",
            "/srv/app/\ta.json",
        ] {
            let error = remove_file(&level, path).unwrap_err();
            assert!(error.contains("empty or contains control characters"), "{path:?}: {error}");
        }
    }

    /// The path is one single-quoted word, and a folder or a link where the
    /// file should be is refused (66) rather than removed, in both dialects.
    #[test]
    fn remove_file_quotes_the_path_and_refuses_what_is_not_a_plain_file() {
        let posix = remove_file_script(ScriptDialect::Posix, "/srv/my app/it's $(x).json");
        assert!(posix.starts_with("f='/srv/my app/it'\\''s $(x).json'\n"), "{posix}");
        assert!(posix.contains("[ -f \"$f\" ] && [ ! -L \"$f\" ] || exit 66"), "{posix}");
        assert!(posix.ends_with("rm -f -- \"$f\"\n"), "{posix}");
        assert!(!posix.contains("rm -rf") && !posix.contains("-r "), "{posix}");

        let powershell = remove_file_script(ScriptDialect::PowerShell, "C:/my app/it's.json");
        assert!(powershell.contains("$f = Native-Path 'C:/my app/it''s.json'\n"), "{powershell}");
        assert!(powershell.contains("$item.PSIsContainer -or"), "{powershell}");
        assert!(powershell.contains("ReparsePoint)) { Quit 66 }"), "{powershell}");
        assert!(powershell.contains("Remove-Item -LiteralPath $f -Force\n"), "{powershell}");
        assert!(!powershell.contains("-Recurse"), "{powershell}");
    }

    /// The deletion script run by a local shell: a regular file goes, one that
    /// is already gone is fine, and a folder or a link leaves everything where
    /// it was.
    #[cfg(unix)]
    #[test]
    fn the_posix_remove_script_removes_only_a_regular_file() {
        let Some(sh) = crate::run_environment::local_bash_candidates().into_iter().next() else {
            eprintln!("skipped: no local bash");
            return;
        };
        let run = |path: &std::path::Path| {
            std::process::Command::new(&sh)
                .args(["--noprofile", "--norc", "-c"])
                .arg(remove_file_script(ScriptDialect::Posix, &path.to_string_lossy()))
                .status()
                .unwrap()
                .code()
        };
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("my role's.json");
        std::fs::write(&file, "{}").unwrap();
        assert_eq!(run(&file), Some(0));
        assert!(!file.exists());
        assert_eq!(run(&file), Some(0));

        let folder = temp.path().join("folder.json");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep"), "x").unwrap();
        assert_eq!(run(&folder), Some(66));
        assert!(folder.join("keep").exists());

        let target = temp.path().join("target.json");
        std::fs::write(&target, "{}").unwrap();
        let link = temp.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(run(&link), Some(66));
        assert!(target.exists() && link.symlink_metadata().is_ok());
        let dangling = temp.path().join("dangling.json");
        std::os::unix::fs::symlink(temp.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(run(&dangling), Some(66));
        assert!(dangling.symlink_metadata().is_ok());
    }

    /// The same, through a real Windows PowerShell.
    #[cfg(windows)]
    #[test]
    fn the_powershell_remove_script_removes_only_a_regular_file() {
        let Some(powershell) = windows_powershell() else {
            eprintln!("skipped: no Windows PowerShell");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let run = |path: &std::path::Path| {
            let script = remove_file_script(ScriptDialect::PowerShell, &path.to_string_lossy().replace('\\', "/"));
            let file = temp.path().join("remove.ps1");
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(script.as_bytes());
            std::fs::write(&file, bytes).unwrap();
            let status = std::process::Command::new(&powershell)
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&file)
                .status()
                .unwrap();
            status.code()
        };
        let file = temp.path().join("my role's.json");
        std::fs::write(&file, "{}").unwrap();
        assert_eq!(run(&file), Some(0));
        assert!(!file.exists());
        assert_eq!(run(&file), Some(0));

        let folder = temp.path().join("folder.json");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("keep"), "x").unwrap();
        assert_eq!(run(&folder), Some(66));
        assert!(folder.join("keep").exists());

        let target = temp.path().join("target.json");
        std::fs::write(&target, "{}").unwrap();
        let link = temp.path().join("link.json");
        // A link needs a privilege a developer machine may not grant.
        if std::os::windows::fs::symlink_file(&target, &link).is_ok() {
            assert_eq!(run(&link), Some(66));
            assert!(target.exists() && link.symlink_metadata().is_ok());
        }
    }

    /// Creating a file refuses a path that is taken in any form, before and
    /// after the contents are written, in both dialects; the PowerShell one
    /// moves with `File.Move`, which never replaces.
    #[test]
    fn create_file_refuses_a_taken_path_in_both_dialects() {
        let posix = create_file_script(ScriptDialect::Posix, "/srv/my app/it's.json");
        assert!(posix.starts_with("f='/srv/my app/it'\\''s.json'\n"), "{posix}");
        assert!(posix.contains("taken() { [ -e \"$f\" ] || [ -L \"$f\" ]; }"), "{posix}");
        assert!(posix.contains(&format!("if taken; then exit {PATH_TAKEN}; fi")), "{posix}");
        assert!(
            posix.find("cat > \"$t\"").unwrap()
                < posix.find(&format!("if taken; then rm -f -- \"$t\"; exit {PATH_TAKEN}; fi")).unwrap()
        );
        let powershell = create_file_script(ScriptDialect::PowerShell, "C:/my app/it's.json");
        assert!(powershell.contains("$f = Native-Path 'C:/my app/it''s.json'\n"), "{powershell}");
        assert!(powershell.contains(&format!("if (Taken) {{ Quit {PATH_TAKEN} }}")), "{powershell}");
        assert!(powershell.contains("[System.IO.File]::Move($t, $f)"), "{powershell}");
        assert!(!powershell.contains("Move-Item"), "{powershell}");
        assert!(create_file(&wsl_level(), "a\nb.json", "{}").is_err());
    }

    /// The POSIX creation script run by a local shell: a free path gets the
    /// contents; a file, a folder or a link by that name is left as it was.
    #[cfg(unix)]
    #[test]
    fn the_posix_create_script_never_replaces_or_enters_what_is_there() {
        let Some(sh) = crate::run_environment::local_bash_candidates().into_iter().next() else {
            eprintln!("skipped: no local bash");
            return;
        };
        let run = |path: &std::path::Path| {
            let mut child = std::process::Command::new(&sh)
                .args(["--noprofile", "--norc", "-c"])
                .arg(create_file_script(ScriptDialect::Posix, &path.to_string_lossy()))
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"{\"name\":\"new\"}").unwrap();
            drop(child.stdin.take());
            child.wait().unwrap().code()
        };
        let temp = tempfile::tempdir().unwrap();
        let free = temp.path().join("free.json");
        assert_eq!(run(&free), Some(0));
        assert_eq!(std::fs::read(&free).unwrap(), b"{\"name\":\"new\"}");
        std::fs::write(&free, "{}").unwrap();
        assert_eq!(run(&free), Some(PATH_TAKEN));
        assert_eq!(std::fs::read(&free).unwrap(), b"{}");
        let folder = temp.path().join("folder.json");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(run(&folder), Some(PATH_TAKEN));
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
        let dangling = temp.path().join("dangling.json");
        std::os::unix::fs::symlink(temp.path().join("nowhere"), &dangling).unwrap();
        assert_eq!(run(&dangling), Some(PATH_TAKEN));
        assert!(!temp.path().join("nowhere").exists());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 3, "no temporary left behind");
    }

    /// The same, through a real Windows PowerShell.
    #[cfg(windows)]
    #[test]
    fn the_powershell_create_script_never_replaces_or_enters_what_is_there() {
        let Some(powershell) = windows_powershell() else {
            eprintln!("skipped: no Windows PowerShell");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let scripts = tempfile::tempdir().unwrap();
        let run = |path: &std::path::Path| {
            let script = create_file_script(ScriptDialect::PowerShell, &path.to_string_lossy().replace('\\', "/"));
            let file = scripts.path().join("create.ps1");
            let mut bytes = vec![0xEF, 0xBB, 0xBF];
            bytes.extend_from_slice(script.as_bytes());
            std::fs::write(&file, bytes).unwrap();
            let mut child = std::process::Command::new(&powershell)
                .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(&file)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"{\"name\":\"new\"}").unwrap();
            drop(child.stdin.take());
            child.wait().unwrap().code()
        };
        let free = temp.path().join("free.json");
        assert_eq!(run(&free), Some(0));
        assert_eq!(std::fs::read(&free).unwrap(), b"{\"name\":\"new\"}");
        std::fs::write(&free, "{}").unwrap();
        assert_eq!(run(&free), Some(PATH_TAKEN));
        assert_eq!(std::fs::read(&free).unwrap(), b"{}");
        let folder = temp.path().join("folder.json");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(run(&folder), Some(PATH_TAKEN));
        assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2, "no temporary left behind");
    }

    #[test]
    fn a_stdio_launch_quotes_everything_and_names_a_missing_command() {
        let runner = ShellRunner::Wsl {
            distro: "Ubuntu".into(),
            env: BTreeMap::new(),
            agent_shell: Default::default(),
        };
        let script = stdio_launch_script(
            &runner,
            "/srv/my app",
            "./server",
            &["--root".into(), "it's".into()],
            &BTreeMap::from([("TOKEN".to_owned(), "a b".to_owned())]),
        )
        .unwrap();
        assert!(script.starts_with("cd -- '/srv/my app' || exit 64\n"), "{script}");
        assert!(script.contains("exec env 'TOKEN=a b' './server' '--root' 'it'\\''s'"), "{script}");
        assert!(script.contains("command not found on the remote machine"), "{script}");
        assert!(stdio_launch_script(&runner, "/srv", "x", &["a\nb".into()], &BTreeMap::new()).is_err());
    }
}
