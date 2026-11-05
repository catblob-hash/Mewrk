//! `zsh` and `bash`: one process group per command, so a stop reaches the whole tree.

use std::collections::VecDeque;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::time::Instant;

use super::{
    Tool, ToolCtx, ToolFuture, ToolResult, agents, blocking, canonical_workspace, fit, future, human_size, output_dir, required_string, spill_notice, stopped_by_model,
    workspace_root,
};
use crate::model::ToolMeta;
use crate::prompt::{render, text};
use crate::tasks::{Task, TaskKind, TaskState};

const MAX_COMMAND_CHARS: usize = 64 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(120_000);
const MAX_TIMEOUT: Duration = Duration::from_millis(600_000);
const SHELL_INLINE_CHARS: usize = 30_000;
/// Kept from a command's start and its end. What the model gets is cut smaller and the rest saved; this bounds that file.
const CAPTURE_HEAD_BYTES: usize = 4 * 1024 * 1024;
const CAPTURE_TAIL_BYTES: usize = 4 * 1024 * 1024;
/// The largest log a background command writes while it runs.
const MAX_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CWD_FILE: u64 = 64 * 1024;
const READ_CHUNK: usize = 64 << 10;
/// How long a group gets to act on SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(2);
/// After the command itself is gone, how long a silent pipe is waited on, and how long at all. Something it left running
/// may hold the pipe for days, and the call must not wait for that.
const DRAIN_IDLE: Duration = Duration::from_millis(100);
const DRAIN_MAX: Duration = Duration::from_secs(2);

static NEXT_SHELL: AtomicU64 = AtomicU64::new(1);

pub(super) fn register(v: &mut Vec<Arc<dyn Tool>>) {
    v.push(Arc::new(ShellTool { name: "zsh" }));
    v.push(Arc::new(ShellTool { name: "bash" }));
}

struct ShellTool {
    name: &'static str,
}

impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn dangerous(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("").trim();
        if args.get("run_in_background") == Some(&Value::Bool(true)) { format!("[Background] {command}") } else { command.to_owned() }
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        let name = self.name;
        future(async move {
            let command = required_string(&args, "command", MAX_COMMAND_CHARS, false)?;
            let root = workspace_root(&ctx, &args)?;
            if args.get("run_in_background") == Some(&Value::Bool(true)) {
                // Only a label for a row, so a long one is cut rather than costing the model a round.
                let label: String = args.get("description").and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
                background(ctx, name, command, root, label).await
            } else {
                foreground(ctx, name, command, root, parse_timeout(&args)).await
            }
        })
    }
}

/// An absent, zero or negative value is the default rather than an error: a bad number should not cost the model a round, and the ceiling is the real
/// protection.
fn parse_timeout(args: &Value) -> Duration {
    let requested = args.get("timeout").and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())));
    match requested {
        Some(ms) if ms.is_finite() && ms > 0.0 => Duration::from_millis(ms as u64).min(MAX_TIMEOUT),
        _ => DEFAULT_TIMEOUT,
    }
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The command under `eval`, so the shell parses it rather than argv splitting. Joined with `&&`,
/// so a failed command never reaches the `pwd` and cannot move the session's directory.
fn tool_script(command: &str, cwd_file: Option<&Path>) -> String {
    let quoted = posix_quote(command);
    // A heredoc or a redirect supplies its own stdin; anything else must not sit waiting for one.
    let eval = if command.contains('<') { format!("eval {quoted}") } else { format!("eval {quoted} < /dev/null") };
    match cwd_file {
        Some(file) => format!("{eval} && pwd -P >| {}", posix_quote(&file.to_string_lossy())),
        None => eval,
    }
}

fn shell_program(name: &str) -> PathBuf {
    let on_path = std::env::var_os("PATH").and_then(|path| std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file()));
    on_path.unwrap_or_else(|| Path::new("/bin").join(name))
}

/// A command that leads its own session, so its group id is its pid, and its output on one pipe, so stdout and stderr stay in the order they were
/// written.
struct Proc {
    child: Child,
    rx: pipe::Receiver,
    guard: GroupGuard,
}

/// The tool's future can go away with the command still running, and then nothing is left to stop it later.
struct GroupGuard {
    group: i32,
    /// Off once the group has been dealt with, so dropping this does not signal a pid that may have been reused.
    armed: bool,
}

impl GroupGuard {
    fn signal(&self, signal: libc::c_int) {
        if self.group > 0 {
            // SAFETY: `kill` with a negative id only signals the processes of that group.
            unsafe {
                libc::kill(-self.group, signal);
            }
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal(libc::SIGKILL);
        }
    }
}

impl Proc {
    fn spawn(name: &str, script: &str, start_dir: &Path) -> Result<Self, String> {
        let program = shell_program(name);
        let (reader, writer) = std::io::pipe().map_err(|e| format!("Failed to start {name}: {e}"))?;
        let also_writer = writer.try_clone().map_err(|e| format!("Failed to start {name}: {e}"))?;
        let mut command = Command::new(&program);
        command.args(["-l", "-c", script]).current_dir(start_dir).stdin(Stdio::null()).stdout(Stdio::from(writer)).stderr(Stdio::from(also_writer));
        command.env("SHELL", &program).env("GIT_EDITOR", "true").env("GIT_PAGER", "cat").env("PAGER", "cat").env("GIT_TERMINAL_PROMPT", "0");
        if std::env::var_os("NO_COLOR").is_none() && std::env::var_os("FORCE_COLOR").is_none() {
            command.env("NO_COLOR", "1");
        }
        if std::env::var_os("TERM").is_none() {
            command.env("TERM", "dumb");
        }
        if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
            // Without a UTF-8 locale, `git`, `python` and `ls` mangle every non-English file name.
            command.env("LANG", "en_US.UTF-8");
        }
        // SAFETY: `setsid` is async-signal-safe, and this runs between fork and exec touching nothing else.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("Failed to start {}: {e}", program.display()))?;
        // The command object holds the write ends, and the pipe reports its end only once those are gone.
        drop(command);
        let group = child.id().map_or(-1, |id| id as i32);
        let rx = pipe::Receiver::from_owned_fd(OwnedFd::from(reader)).map_err(|e| format!("Failed to read command output: {e}"))?;
        Ok(Self { child, rx, guard: GroupGuard { group, armed: true } })
    }

    fn group_is_gone(&self) -> bool {
        // SAFETY: signal 0 only asks whether the group exists.
        self.guard.group <= 0 || unsafe { libc::kill(-self.guard.group, 0) } != 0
    }

    /// SIGTERM, then SIGKILL for whatever is left in the group. The pipe keeps being read meanwhile, or a process stuck writing to it could never act
    /// on the first.
    async fn terminate(&mut self, sink: &mut Sink, buf: &mut [u8]) -> ExitStatus {
        self.guard.signal(libc::SIGTERM);
        let deadline = Instant::now() + TERM_GRACE;
        let mut status = None;
        let mut eof = false;
        while Instant::now() < deadline && (status.is_none() || !self.group_is_gone()) {
            tokio::select! {
                s = self.child.wait(), if status.is_none() => status = Some(s),
                r = read_some(&mut self.rx, buf), if !eof => match r {
                    Some(n) => sink.push(&buf[..n]).await,
                    None => eof = true,
                },
                _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
        self.guard.signal(libc::SIGKILL);
        let status = match status {
            Some(s) => s,
            None => self.child.wait().await,
        };
        self.guard.armed = false;
        status.unwrap_or_else(|_| failed_status())
    }

    /// What is still in the pipe. Whatever holds it past this keeps writing into a drain instead of dying of a closed pipe: leaving a server running
    /// is a legitimate thing for a command to do.
    async fn drain(self, sink: &mut Sink, buf: &mut [u8]) {
        let Self { mut rx, mut guard, .. } = self;
        guard.armed = false;
        let deadline = Instant::now() + DRAIN_MAX;
        while Instant::now() < deadline {
            match tokio::time::timeout(DRAIN_IDLE, read_some(&mut rx, buf)).await {
                Ok(Some(n)) => sink.push(&buf[..n]).await,
                Ok(None) => return,
                Err(_) => break,
            }
        }
        tokio::spawn(async move {
            let mut discard = vec![0u8; READ_CHUNK];
            while read_some(&mut rx, &mut discard).await.is_some() {}
        });
    }
}

async fn read_some(rx: &mut pipe::Receiver, buf: &mut [u8]) -> Option<usize> {
    use tokio::io::AsyncReadExt;
    match rx.read(buf).await {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(n),
    }
}

fn failed_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(1 << 8)
}

enum End {
    Exited(ExitStatus),
    Stopped,
}

#[allow(clippy::large_enum_variant)] // Only ever a local that is matched at once, so there is nothing to save by boxing it.
enum Driven {
    Ended(End),
    /// The deadline came with the command still running. It is the caller's to keep going.
    Deadline(Proc),
}

/// Runs a command to its end, or until `stop` resolves or the deadline passes. `stop` is whatever may end it: a run being stopped, a task, a child's
/// final reply.
async fn drive(mut proc: Proc, sink: &mut Sink, deadline: Option<Instant>, stop: impl Future<Output = ()>) -> Driven {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut eof = false;
    tokio::pin!(stop);
    let sleep = tokio::time::sleep_until(deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(1 << 40)));
    tokio::pin!(sleep);
    let end = loop {
        tokio::select! {
            biased;
            status = proc.child.wait() => break End::Exited(status.unwrap_or_else(|_| failed_status())),
            () = &mut stop => {
                proc.terminate(sink, &mut buf).await;
                break End::Stopped;
            }
            () = &mut sleep, if deadline.is_some() => return Driven::Deadline(proc),
            r = read_some(&mut proc.rx, &mut buf), if !eof => match r {
                Some(n) => sink.push(&buf[..n]).await,
                None => eof = true,
            },
        }
    };
    proc.drain(sink, &mut buf).await;
    Driven::Ended(end)
}

/// A command's output as it comes: its start and its end kept for the model, and for a background command, what the task shows live and the log the
/// model can read.
#[derive(Default)]
struct Sink {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: u64,
    live: Option<Live>,
}

struct Live {
    task: Arc<Task>,
    /// The start of a character a read cut in two.
    partial: Vec<u8>,
    file: Option<tokio::fs::File>,
    written: u64,
}

impl Sink {
    async fn push(&mut self, bytes: &[u8]) {
        let room = CAPTURE_HEAD_BYTES.saturating_sub(self.head.len());
        let taken = bytes.len().min(room);
        self.head.extend_from_slice(&bytes[..taken]);
        self.tail.extend(&bytes[taken..]);
        let excess = self.tail.len().saturating_sub(CAPTURE_TAIL_BYTES);
        if excess > 0 {
            self.tail.drain(..excess);
            self.omitted += excess as u64;
        }
        if let Some(live) = &mut self.live {
            live.push(bytes).await;
        }
    }

    /// The output as text, with the omission marked where it happened.
    fn text(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        if self.omitted == 0 {
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        // Both cuts can land inside a character.
        let mut head = self.head.clone();
        let head_len = head.len();
        let valid = match std::str::from_utf8(&head) {
            Ok(_) => head_len,
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => head_len,
        };
        head.truncate(valid);
        let skip = tail.iter().take(3).take_while(|b| **b & 0xc0 == 0x80).count();
        let omitted = self.omitted + (head_len - head.len()) as u64 + skip as u64;
        format!(
            "{}\n{}\n{}",
            String::from_utf8_lossy(&head),
            render("tool.shell_output_omitted", &[("size", &human_size(omitted))]),
            String::from_utf8_lossy(&tail[skip..])
        )
    }
}

impl Live {
    async fn push(&mut self, bytes: &[u8]) {
        if self.partial.is_empty() {
            self.show(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(bytes);
            self.show(&joined);
        }
        if let Some(file) = &mut self.file {
            let room = MAX_OUTPUT_FILE_BYTES.saturating_sub(self.written) as usize;
            let take = bytes.len().min(room);
            let ok = file.write_all(&bytes[..take]).await.is_ok();
            self.written += take as u64;
            if !ok || take < bytes.len() {
                let note = format!("\n{}\n", render("task.shell_output_file_limit", &[("size", &human_size(MAX_OUTPUT_FILE_BYTES))]));
                let _ = file.write_all(note.as_bytes()).await;
                let _ = file.flush().await;
                self.file = None;
            }
        }
    }

    fn show(&mut self, bytes: &[u8]) {
        match std::str::from_utf8(bytes) {
            Ok(s) => self.task.append_output(s),
            Err(e) if e.error_len().is_none() => {
                let (done, rest) = bytes.split_at(e.valid_up_to());
                self.task.append_output(&String::from_utf8_lossy(done));
                self.partial = rest.to_owned();
            }
            Err(_) => self.task.append_output(&String::from_utf8_lossy(bytes)),
        }
    }
}

fn normalize_line_endings(text: &str) -> String {
    if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text.to_owned() }
}

/// Drops the blank lines a command sometimes opens with. Indentation on the first line that says something is content.
fn strip_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    loop {
        let Some(end) = rest.find('\n') else { return rest };
        if !rest[..end].trim().is_empty() {
            return rest;
        }
        rest = &rest[end + 1..];
    }
}

fn cleaned(output: &str) -> String {
    strip_leading_blank_lines(&normalize_line_endings(output)).trim_end().to_owned()
}

/// A successful call is its output; a failed one leads with why, because a long output would otherwise bury it.
fn format_output(end: &End, output: &str) -> (bool, String) {
    let output = cleaned(output);
    let status = match end {
        End::Stopped => Some(text("tool.shell_user_aborted").to_owned()),
        End::Exited(status) if !status.success() => {
            let code = status.code().map_or_else(|| text("tool.shell_exit_unknown").to_owned(), |c| c.to_string());
            Some(render("tool.shell_exit_code", &[("code", &code)]))
        }
        End::Exited(_) => None,
    };
    let success = status.is_none();
    let joined: Vec<String> = status.into_iter().chain((!output.is_empty()).then_some(output)).collect();
    if joined.is_empty() {
        // A silent success still has to say something, or the round reads as if the tool produced nothing.
        return (true, render("tool.shell_completed", &[("code", "0")]));
    }
    (success, joined.join("\n"))
}

/// Where a finished command left the session: only inside the workspace does it carry over. A command may `cd` anywhere within a call.
enum ReportedCwd {
    Unchanged,
    Inside(PathBuf),
    Gone,
    Outside(String),
}

fn read_reported_cwd(cwd_file: &Path, workspace: &Path) -> ReportedCwd {
    let too_long = std::fs::metadata(cwd_file).is_ok_and(|m| m.len() > MAX_CWD_FILE);
    let raw = std::fs::read(cwd_file).ok();
    let _ = std::fs::remove_file(cwd_file);
    let Some(raw) = raw.filter(|_| !too_long) else { return ReportedCwd::Unchanged };
    let reported = String::from_utf8_lossy(&raw);
    let reported = reported.trim();
    let path = Path::new(reported);
    if reported.is_empty() || !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return ReportedCwd::Unchanged;
    }
    let Some(resolved) = std::fs::canonicalize(path).ok().filter(|p| p.is_dir()) else { return ReportedCwd::Gone };
    let Ok(workspace) = std::fs::canonicalize(workspace) else { return ReportedCwd::Gone };
    if resolved.starts_with(&workspace) { ReportedCwd::Inside(resolved) } else { ReportedCwd::Outside(reported.to_owned()) }
}

async fn foreground(ctx: Arc<ToolCtx>, name: &'static str, command: String, root: PathBuf, timeout: Duration) -> Result<ToolResult, String> {
    if ctx.run.cancel.is_cancelled() {
        return Err("The run or task was stopped, so the command did not start; do not retry directly, and first confirm the user's intent".to_owned());
    }
    let (root, start_dir) = start_directory(&ctx, root).await?;
    let cwd_file = std::env::temp_dir().join(format!("mewrk-cwd-{}", crate::model::new_id()));
    let proc = Proc::spawn(name, &tool_script(&command, Some(&cwd_file)), &start_dir)?;
    let mut sink = Sink::default();
    let cancel = ctx.run.cancel.clone();
    let deadline = Instant::now() + timeout;
    let end = match drive(proc, &mut sink, Some(deadline), async move { cancel.cancelled().await }).await {
        Driven::Ended(end) => end,
        Driven::Deadline(proc) => {
            let _ = std::fs::remove_file(&cwd_file);
            return move_to_background(ctx, name, command, proc, sink, timeout).await;
        }
    };
    let (success, output) = format_output(&end, &sink.text());
    let number = ctx.workspaces.iter().position(|w| std::fs::canonicalize(w).is_ok_and(|w| w == root)).map_or(1, |i| i + 1);
    let spill = output_dir(&ctx);
    let session = ctx.session.clone();
    let stem = format!("{name}-{}", crate::model::new_id());
    let exited = matches!(end, End::Exited(_));
    let text = blocking(move || {
        // A killed command may still have written its directory, from a call the model is told did not finish.
        let reported = if exited { read_reported_cwd(&cwd_file, &root) } else { ReportedCwd::Unchanged };
        let _ = std::fs::remove_file(&cwd_file);
        let note = match reported {
            ReportedCwd::Unchanged => None,
            ReportedCwd::Inside(dir) => {
                session.shell_cwd.lock().insert(root, dir);
                None
            }
            ReportedCwd::Gone => {
                session.shell_cwd.lock().remove(&root);
                None
            }
            ReportedCwd::Outside(dir) => {
                let shown = root.to_string_lossy().into_owned();
                session.shell_cwd.lock().remove(&root);
                Some(render("tool.shell_cwd_outside_workspace", &[("directory", &dir), ("workspace", &number.to_string()), ("root", &shown)]))
            }
        };
        let mut text = fit(output, SHELL_INLINE_CHARS, &spill, &stem);
        if let Some(note) = note {
            text.push('\n');
            text.push_str(&note);
        }
        text
    })
    .await?;
    Ok(if success { ToolResult::ok(text) } else { ToolResult::err(text) })
}

async fn start_directory(ctx: &ToolCtx, root: PathBuf) -> Result<(PathBuf, PathBuf), String> {
    let session = ctx.session.clone();
    blocking(move || {
        let root = canonical_workspace(&root)?;
        // A remembered directory that has gone away must not fail the call.
        let start = session.shell_cwd.lock().get(&root).filter(|d| d.is_dir()).cloned().unwrap_or_else(|| root.clone());
        Ok((root, start))
    })
    .await?
}

/// The file a background command's output goes to while it runs, the only way to see that output before the command ends.
pub(super) fn output_file(ctx: &ToolCtx, shell_ref: &str) -> PathBuf {
    output_dir(ctx).join(format!("{}-output.txt", shell_ref.replace(':', "-")))
}

/// A command that outlives its timeout keeps running as a task. Killing it would throw away a build that was nearly done, and the model can always stop it.
async fn move_to_background(ctx: Arc<ToolCtx>, name: &'static str, command: String, proc: Proc, mut sink: Sink, timeout: Duration) -> Result<ToolResult, String> {
    let shell_ref = format!("shell:{}", NEXT_SHELL.fetch_add(1, Ordering::Relaxed));
    let task = Task::new(shell_ref.clone(), command.trim().to_owned(), TaskKind::Shell);
    *task.owner.lock() = ctx.spec.child.as_ref().map(|c| c.task.id.clone());
    ctx.session.add_task(task.clone());
    let path = output_file(&ctx, &shell_ref);
    let dir = output_dir(&ctx);
    let opened = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::File::create(&path).await
    }
    .await
    .ok();
    let seconds = timeout.as_secs().max(1).to_string();
    let receipt = match &opened {
        Some(_) => render("task.shell_timeout_backgrounded_file", &[("seconds", &seconds), ("shell_ref", &shell_ref), ("path", &path.to_string_lossy())]),
        None => render("task.shell_timeout_backgrounded", &[("seconds", &seconds), ("shell_ref", &shell_ref)]),
    };
    // From here on it is a background command in every way, starting with what it has printed so far.
    let mut live = Live { task: task.clone(), partial: Vec::new(), file: opened, written: 0 };
    let so_far = sink.text();
    live.push(so_far.as_bytes()).await;
    sink.live = Some(live);
    let meta = ToolMeta { task: Some(task.id.clone()), ..Default::default() };
    let worker_ctx = ctx.clone();
    ctx.engine.rt.spawn(settle_background(worker_ctx, name, proc, task, shell_ref, path, sink));
    Ok(ToolResult::ok(receipt).with_meta(meta))
}

async fn background(ctx: Arc<ToolCtx>, name: &'static str, command: String, root: PathBuf, description: String) -> Result<ToolResult, String> {
    if ctx.run.cancel.is_cancelled() {
        return Err("The run or task was stopped, so the command did not start; do not retry directly, and first confirm the user's intent".to_owned());
    }
    let (_, start_dir) = start_directory(&ctx, root).await?;
    // A command that never spawned is not a task, so nothing is registered before this.
    let proc = Proc::spawn(name, &tool_script(&command, None), &start_dir)?;
    let shell_ref = format!("shell:{}", NEXT_SHELL.fetch_add(1, Ordering::Relaxed));
    let label = if description.trim().is_empty() { command.trim().to_owned() } else { description };
    let task = Task::new(shell_ref.clone(), label, TaskKind::Shell);
    *task.owner.lock() = ctx.spec.child.as_ref().map(|c| c.task.id.clone());
    ctx.session.add_task(task.clone());

    let path = output_file(&ctx, &shell_ref);
    let dir = output_dir(&ctx);
    let opened = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::File::create(&path).await
    }
    .await
    .ok();
    let receipt = match &opened {
        Some(_) => render("task.shell_background_receipt", &[("shell_ref", &shell_ref), ("path", &path.to_string_lossy())]),
        None => shell_ref.clone(),
    };
    let live = Live { task: task.clone(), partial: Vec::new(), file: opened, written: 0 };
    let meta = ToolMeta { task: Some(task.id.clone()), ..Default::default() };
    let worker_ctx = ctx.clone();
    let sink = Sink { live: Some(live), ..Default::default() };
    ctx.engine.rt.spawn(settle_background(worker_ctx, name, proc, task, shell_ref, path, sink));
    Ok(ToolResult::ok(receipt).with_meta(meta))
}

async fn settle_background(ctx: Arc<ToolCtx>, name: &'static str, proc: Proc, task: Arc<Task>, shell_ref: String, path: PathBuf, mut sink: Sink) {
    let has_file = sink.live.as_ref().is_some_and(|l| l.file.is_some());
    let stop_task = task.clone();
    let owner = ctx.spec.child.as_ref().map(|c| c.task.clone());
    // A child's commands end with its final reply. The conversation's own outlive the turn, and only their own task's stop ends them.
    let stop = async move {
        match owner {
            Some(owner) => tokio::select! {
                () = stop_task.cancel.cancelled() => {}
                () = owner.wait() => {}
            },
            None => stop_task.cancel.cancelled().await,
        }
    };
    // No deadline was given, so only the end of the command or a stop comes back.
    let end = match drive(proc, &mut sink, None, stop).await {
        Driven::Ended(end) => end,
        Driven::Deadline(_) => unreachable!("no deadline"),
    };
    if let Some(mut live) = sink.live.take()
        && let Some(file) = &mut live.file
    {
        let _ = file.flush().await;
    }
    let spill = output_dir(&ctx);
    let output = cleaned(&sink.text());
    // Over the limit, the log the command wrote is already the saved copy. A log that hit its own limit is the one case with more in memory's tail,
    // and it says so itself.
    let stem = format!("{name}-{shell_ref}").replace(':', "-");
    let log = has_file.then(|| path.clone());
    let fitted = blocking(move || {
        if output.chars().nth(SHELL_INLINE_CHARS).is_none() {
            return output;
        }
        match log {
            Some(path) => spill_notice(&output, &path),
            None => fit(output, SHELL_INLINE_CHARS, &spill, &stem),
        }
    })
    .await
    .unwrap_or_default();
    let body = if fitted.trim().is_empty() { text("task.shell_no_output").to_owned() } else { fitted };
    let (state, result) = match end {
        End::Exited(status) => {
            let exit = match status.code() {
                Some(code) => render("task.shell_exit_code", &[("code", &code.to_string())]),
                None => text("task.shell_exit_unknown").to_owned(),
            };
            let result = render("task.shell_result", &[("shell_ref", &shell_ref), ("tool_name", name), ("exit", &exit), ("body", &body)]);
            let state = if status.success() {
                TaskState::Completed
            } else {
                TaskState::Failed(status.code().map_or_else(|| "exit code unknown".to_owned(), |c| format!("exit code {c}")))
            };
            (state, result)
        }
        End::Stopped => {
            let key = if stopped_by_model(&task) { "task.shell_stopped_by_model" } else { "task.shell_stopped_by_user" };
            let body = format!("{body}\n{}", text("tool.shell_user_aborted"));
            (TaskState::Stopped, render(key, &[("shell_ref", &shell_ref), ("tool_name", name), ("body", &body)]))
        }
    };
    task.set_output(result);
    task.finish(state);
    agents::notify_finished(&agents::outbox(&ctx), &task);
    // The conversation may be idle by now, and this is what wakes it.
    ctx.engine.notifier.poke();
}

#[cfg(test)]
mod tests {
    use super::super::testing::{call, fixture};
    use super::*;
    use crate::model::PermissionMode;
    use serde_json::json;

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn wait_gone(pid: i32) -> bool {
        for _ in 0..300 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn output_is_merged_in_order_and_formatted() {
        let f = fixture(PermissionMode::FullAccess);
        let r = call(&f.ctx, "zsh", json!({"command": "echo one; echo two >&2; echo three"})).await;
        assert!(!r.is_error, "{}", r.output);
        assert_eq!(r.output, "one\ntwo\nthree");
        let r = call(&f.ctx, "bash", json!({"command": "\n\necho '  indented'  ; echo"})).await;
        assert_eq!(r.output, "  indented");
        let r = call(&f.ctx, "bash", json!({"command": "true"})).await;
        assert_eq!(r.output, "Command finished (exit code 0)");
        let r = call(&f.ctx, "zsh", json!({"command": "echo out; exit 3"})).await;
        assert!(r.is_error);
        assert_eq!(r.output, "Exit code 3\nout");
        let r = call(&f.ctx, "bash", json!({"command": "echo \"it's\" 'quoted'; printf 'crlf\\r\\nx'"})).await;
        assert_eq!(r.output, "it's quoted\ncrlf\nx");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_output_is_saved_and_previewed() {
        let f = fixture(PermissionMode::FullAccess);
        let r = call(&f.ctx, "bash", json!({"command": "for i in $(seq 1 12000); do echo line $i; done"})).await;
        assert!(r.output.starts_with("<persisted-output>\nOutput too large"), "{}", &r.output[..100]);
        let saved = r.output.lines().nth(1).unwrap().rsplit(": ").next().unwrap();
        let whole = std::fs::read_to_string(saved).unwrap();
        assert!(whole.ends_with("line 12000"));
        assert!(saved.starts_with(f.dir.join("data/tool-output").to_str().unwrap()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_directory_carries_over_only_from_a_success_inside_the_workspace() {
        let f = fixture(PermissionMode::FullAccess);
        std::fs::create_dir_all(f.dir.join("work/sub")).unwrap();
        call(&f.ctx, "zsh", json!({"command": "cd sub"})).await;
        assert!(call(&f.ctx, "bash", json!({"command": "pwd -P"})).await.output.ends_with("/work/sub"));
        // A failed command does not move it.
        call(&f.ctx, "zsh", json!({"command": "cd .. && false"})).await;
        assert!(call(&f.ctx, "bash", json!({"command": "pwd -P"})).await.output.ends_with("/work/sub"));
        // Leaving the workspace resets it, and says so.
        let r = call(&f.ctx, "zsh", json!({"command": "cd /"})).await;
        assert!(r.output.contains("[Ended in /, outside workspace 1; the next command there starts at its root,"), "{}", r.output);
        assert!(call(&f.ctx, "bash", json!({"command": "pwd -P"})).await.output.ends_with("/work"));
        // A directory that went away falls back to the root.
        std::fs::create_dir_all(f.dir.join("work/gone")).unwrap();
        call(&f.ctx, "zsh", json!({"command": "cd gone"})).await;
        std::fs::remove_dir(f.dir.join("work/gone")).unwrap();
        assert!(call(&f.ctx, "bash", json!({"command": "pwd -P"})).await.output.ends_with("/work"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_timeout_moves_the_command_to_the_background() {
        let f = fixture(PermissionMode::FullAccess);
        let marker = f.dir.join("done");
        let command = format!("echo started; sleep 1; echo finished; touch {}", marker.display());
        let t0 = std::time::Instant::now();
        let r = call(&f.ctx, "zsh", json!({"command": command, "timeout": 200})).await;
        assert!(!r.is_error, "{}", r.output);
        assert!(r.output.contains("keeps running in the background as shell:"), "{}", r.output);
        assert!(t0.elapsed() < Duration::from_millis(900), "the call returned at its timeout");
        let task = f.ctx.session.snapshot().into_iter().next().expect("it became a task");
        assert_eq!(r.meta.task.as_deref(), Some(task.id.as_str()));
        task.wait().await;
        assert_eq!(task.state(), TaskState::Completed);
        assert!(marker.exists(), "the command was left to finish");
        let out = task.output();
        assert!(out.contains("started") && out.contains("finished"), "both sides of the timeout are in its result: {out}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_ignores_sigterm_gets_sigkill() {
        let f = fixture(PermissionMode::FullAccess);
        let pidfile = f.dir.join("pid");
        let command = format!("trap '' TERM; sleep 300 & echo $! > {}; wait", pidfile.display());
        let r = call(&f.ctx, "bash", json!({"command": command, "run_in_background": true})).await;
        assert!(!r.is_error);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let task = f.ctx.session.snapshot().into_iter().next().unwrap();
        let started = std::time::Instant::now();
        task.stop();
        task.wait().await;
        assert!(started.elapsed() >= TERM_GRACE, "it got its grace");
        let pid: i32 = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        assert!(wait_gone(pid).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_the_run_kills_the_tree_and_says_so() {
        let f = fixture(PermissionMode::FullAccess);
        let pidfile = f.dir.join("pid");
        let command = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "zsh", json!({"command": command})).await });
        while !pidfile.exists() || std::fs::read_to_string(&pidfile).unwrap().trim().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        f.ctx.run.stop();
        let r = pending.await.unwrap();
        assert!(r.is_error);
        assert!(r.output.starts_with("<error>Command was aborted before completion</error>"), "{}", r.output);
        assert!(wait_gone(pid).await);
        // And nothing starts once the run has been stopped.
        let r = call(&f.ctx, "zsh", json!({"command": "echo no"})).await;
        assert!(r.output.starts_with("The run or task was stopped"), "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_command_that_leaves_something_running_returns_all_the_same() {
        let f = fixture(PermissionMode::FullAccess);
        let pidfile = f.dir.join("pid");
        let command = format!("sleep 30 & echo $! > {}; echo done", pidfile.display());
        let started = std::time::Instant::now();
        let r = call(&f.ctx, "zsh", json!({"command": command})).await;
        assert_eq!(r.output, "done");
        assert!(started.elapsed() < Duration::from_secs(10));
        let pid: i32 = std::fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        assert!(alive(pid), "leaving a server running is the command's call");
        // SAFETY: the test's own child.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_background_command_reports_through_a_notification() {
        let f = fixture(PermissionMode::FullAccess);
        let r = call(&f.ctx, "zsh", json!({"command": "echo first; sleep 0.2; echo second; exit 3", "run_in_background": true})).await;
        assert!(!r.is_error, "{}", r.output);
        let shell_ref = r.output.lines().next().unwrap().to_owned();
        assert!(shell_ref.starts_with("shell:"), "{}", r.output);
        assert!(r.output.contains("Output so far: "), "{}", r.output);
        assert!(r.meta.task.is_some());
        let task = f.ctx.session.find_task(&shell_ref).unwrap();
        assert_eq!(task.kind, TaskKind::Shell);
        assert_eq!(task.state(), TaskState::Running, "it returned immediately");
        task.wait().await;
        assert_eq!(task.state(), TaskState::Failed("exit code 3".into()));
        let notes = f.ctx.session.take_notes();
        assert_eq!(notes.len(), 1, "{notes:?}");
        let note = &notes[0];
        assert!(note.starts_with("<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]"), "{note}");
        assert!(note.contains(&format!("<task-id>{shell_ref}</task-id>\n<status>failed</status>\n<summary>Background task {shell_ref} failed</summary>")), "{note}");
        assert!(note.contains(&format!("Background command {shell_ref} (zsh) finished, exit code 3:\nfirst\nsecond")), "{note}");
        let log = f.dir.join("data/tool-output/conv").join(format!("{}-output.txt", shell_ref.replace(':', "-")));
        assert_eq!(std::fs::read_to_string(log).unwrap(), "first\nsecond\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_task_shows_output_as_it_comes_and_a_stop_ends_the_tree() {
        let f = fixture(PermissionMode::FullAccess);
        let pidfile = f.dir.join("pid");
        let command = format!("echo live; sleep 300 & echo $! > {}; wait", pidfile.display());
        let r = call(&f.ctx, "bash", json!({"command": command, "run_in_background": true})).await;
        let shell_ref = r.output.lines().next().unwrap().to_owned();
        let task = f.ctx.session.find_task(&shell_ref).unwrap();
        for _ in 0..500 {
            if task.output().contains("live") && pidfile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(task.output().contains("live"));
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // The run ending does not end it.
        f.ctx.run.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(alive(pid));
        task.stop();
        task.wait().await;
        assert_eq!(task.state(), TaskState::Stopped);
        assert!(wait_gone(pid).await);
        let notes = f.ctx.session.take_notes();
        assert!(
            notes[0].contains(&format!("Background command {shell_ref} (bash) was stopped by the user:\nlive\n<error>Command was aborted before completion</error>")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_character_cut_by_a_read_arrives_whole() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let task = Task::new("shell:1", "x", TaskKind::Shell);
            let mut live = Live { task: task.clone(), partial: Vec::new(), file: None, written: 0 };
            let bytes = "世界".as_bytes();
            live.push(&bytes[..4]).await;
            live.push(&bytes[4..]).await;
            assert_eq!(task.output(), "世界");
        });
    }

    #[test]
    fn a_huge_output_keeps_both_ends_and_says_what_fell_between() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut sink = Sink::default();
            sink.push(b"START").await;
            let filler = vec![b'x'; CAPTURE_HEAD_BYTES];
            sink.push(&filler).await;
            sink.push(&filler).await;
            sink.push(b"END").await;
            let text = sink.text();
            assert!(text.starts_with("START"));
            assert!(text.ends_with("END"));
            assert!(text.contains("[… 8 bytes of output omitted …]"), "{}", &text[CAPTURE_HEAD_BYTES..CAPTURE_HEAD_BYTES + 60]);
        });
    }

    #[test]
    fn timeouts_are_clamped_not_refused() {
        assert_eq!(parse_timeout(&json!({})), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(&json!({"timeout": -5})), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(&json!({"timeout": 0})), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(&json!({"timeout": "2000"})), Duration::from_secs(2));
        assert_eq!(parse_timeout(&json!({"timeout": 99_999_999})), MAX_TIMEOUT);
    }

    #[test]
    fn only_the_two_shells_are_registered() {
        for (name, there) in [("zsh", true), ("bash", true), ("sh", false), ("pwsh", false), ("powershell", false)] {
            assert_eq!(super::super::find(name).is_some(), there, "{name}");
        }
        assert_eq!(super::super::find("zsh").unwrap().summary(&json!({"command": "ls", "run_in_background": true})), "[Background] ls");
    }
}
