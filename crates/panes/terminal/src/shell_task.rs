//! `ShellTaskPanel`: one `bash` / `powershell` command's output, read-only, which is the terminal pane's read-only page.
//!
//! Deliberately not `panel`: that one owns a PTY (open, write, resize, detach). This page has no process to talk to. It
//! subscribes to a command the model started and draws what comes back. Its state is keyed by command and held in the
//! pane's store while a guard says the page is mounted, so coming back to it is a repaint rather than a fresh subscribe
//! and replay.

use std::collections::HashMap;

use mewrk_client::prelude::*;
use mewrk_term::{Options, Terminal};
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};
use serde::Deserialize;

/// How a shell task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellTaskOutcome {
    Succeeded,
    Failed,
    Stopped,
}

/// As much of `lib/shellTasks`' `ShellTaskSnapshot` as this page reads.
// TODO: replace with the tasks pane's own type once it is public (`mewrk_pane_tasks`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellTaskView {
    pub shell_task_id: String,
    pub conversation_id: String,
    pub tool_name: String,
    pub command: String,
    #[serde(default)]
    pub stopping: bool,
    pub outcome: Option<ShellTaskOutcome>,
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ShellTaskView {
    /// `shellTaskDirectory`: where a shell command ran. `None` when neither its directory nor its workspace is known.
    pub fn directory(&self) -> Option<String> {
        self.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()).or_else(|| self.workspace_root.as_deref().map(str::trim).filter(|c| !c.is_empty())).map(str::to_owned)
    }

    /// `shellTaskTitle`: the shell, then the directory it ran in.
    pub fn title(&self) -> String {
        match self.directory() {
            Some(directory) => format!("{}:{directory}", self.tool_name),
            None => self.tool_name.clone(),
        }
    }

    /// `statusLabel`.
    pub fn status_label(&self) -> String {
        match self.outcome {
            None if self.stopping => t("正在中止", "Stopping").to_owned(),
            None => t("正在运行", "Running").to_owned(),
            Some(ShellTaskOutcome::Stopped) => t("已中止", "Stopped").to_owned(),
            Some(ShellTaskOutcome::Failed) => match self.exit_code {
                Some(code) => tf("已失败（退出码 {code}）", "Failed (exit {code})", &[("code", &code)]),
                None => t("已失败", "Failed").to_owned(),
            },
            Some(ShellTaskOutcome::Succeeded) => t("已完成", "Finished").to_owned(),
        }
    }
}

/// stderr is dimmed rather than coloured red: a warning on stderr is not a failure.
const STDERR_PREFIX: &[u8] = b"\x1b[2m";
const STDERR_SUFFIX: &[u8] = b"\x1b[0m";
/// How long the copy control reports its outcome before reverting to its label.
const COPY_STATUS_SECS: f32 = 1.6;
/// Longest transcript kept for the copy control, in bytes. The host retains a tail and the page receives everything
/// after it subscribed, so a chatty build could otherwise grow this without bound; like the host's buffer it keeps the
/// end.
const MAX_TRANSCRIPT: usize = 4 * 1024 * 1024;
/// The page keeps more than a shell's scrollback: a build's log is worth reading back.
const HISTORY: usize = 20_000;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ShellOutputEvent {
    Output {
        text: String,
        #[serde(default)]
        stream: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellTaskOutputHandle {
    snapshot: String,
    #[serde(default)]
    dropped_head_bytes: u64,
    #[serde(default)]
    subscription_id: u64,
}

/// Appends to the transcript, keeping its end, on a character boundary.
fn append_transcript(transcript: &mut String, text: &str) {
    transcript.push_str(text);
    if transcript.len() > MAX_TRANSCRIPT {
        let mut cut = transcript.len() - MAX_TRANSCRIPT;
        while !transcript.is_char_boundary(cut) {
            cut += 1;
        }
        transcript.drain(..cut);
    }
}

/// What the terminal is fed for a chunk: stderr wrapped so it reads dimmed.
pub(crate) fn output_bytes(text: &str, stderr: bool) -> Vec<u8> {
    if !stderr {
        return text.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + STDERR_PREFIX.len() + STDERR_SUFFIX.len());
    out.extend_from_slice(STDERR_PREFIX);
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(STDERR_SUFFIX);
    out
}

struct Page {
    term: Terminal,
    /// Everything written to the terminal since the last subscribe, verbatim, because the emulator's own buffer is not a
    /// transcript: it pads the viewport with empty rows and trims every row's trailing spaces.
    transcript: String,
    failure: Option<String>,
    dropped_head_bytes: u64,
    /// The subscription to hand back when the page goes away.
    subscription: Option<u64>,
    channel: Option<Channel>,
    /// When the copy control's outcome stops showing, by `ui.time`, and what it was.
    copy_status: Option<(f32, bool)>,
    /// Where the right click that opened the menu landed, while it is open.
    menu: Option<(f32, f32)>,
}

#[derive(Default)]
pub(crate) struct Pages {
    pages: HashMap<String, Page>,
}

fn pages(app: &mut App) -> &mut Pages {
    app.local::<Pages>()
}

fn page_key(conversation_id: &str, shell_task_id: &str) -> String {
    format!("{conversation_id}/{shell_task_id}")
}

fn write_page(page: &mut Page, text: &str, stderr: bool) {
    if text.is_empty() {
        return;
    }
    append_transcript(&mut page.transcript, text);
    // The host reads both pipes on their own threads, so chunks arrive interleaved rather than in the "all stdout, then
    // stderr" order the model's copy of this output uses.
    page.term.feed(output_bytes(text, stderr));
}

fn subscribe(app: &mut App, key: &str, conversation_id: &str, shell_task_id: &str) {
    let channel = {
        let key = key.to_owned();
        app.open_channel(move |app, value| {
            let Ok(ShellOutputEvent::Output { text, stream }) = serde_json::from_value::<ShellOutputEvent>(value) else { return };
            if let Some(page) = pages(app).pages.get_mut(&key) {
                write_page(page, &text, stream == "stderr");
            }
        })
    };
    if let Some(page) = pages(app).pages.get_mut(key) {
        page.channel = Some(channel.clone());
    }
    let (key, owned_key) = (key.to_owned(), key.to_owned());
    let _ = owned_key;
    app.invoke::<ShellTaskOutputHandle>("open_shell_task_output", json!({ "conversationId": conversation_id, "shellTaskId": shell_task_id, "onEvent": channel.arg() }), move |app, result| {
        let Some(page) = pages(app).pages.get_mut(&key) else {
            // The page went while this was on its way: let the sink go too, and only this one.
            if let Ok(handle) = result {
                let (conversation, task) = split_key(&key);
                app.invoke::<Value>("detach_shell_task_output", json!({ "conversationId": conversation, "shellTaskId": task, "subscriptionId": handle.subscription_id }), |_, _| {});
            }
            channel.close();
            return;
        };
        match result {
            Ok(handle) => {
                page.subscription = Some(handle.subscription_id);
                page.dropped_head_bytes = handle.dropped_head_bytes;
                // The snapshot is what the host still retains; it lost the pipe boundaries, so it is replayed as plain
                // output.
                write_page(page, &handle.snapshot, false);
            }
            Err(message) => {
                let message = message.trim();
                page.failure = Some(if message.is_empty() { t("无法读取命令输出", "Unable to read the command output").to_owned() } else { message.to_owned() });
            }
        }
    });
}

fn split_key(key: &str) -> (&str, &str) {
    key.split_once('/').unwrap_or((key, ""))
}

struct PageGuard {
    task: Task,
    key: String,
}

impl Drop for PageGuard {
    fn drop(&mut self) {
        let key = std::mem::take(&mut self.key);
        self.task.post(move |app| {
            let Some(page) = pages(app).pages.remove(&key) else { return };
            if let Some(channel) = &page.channel {
                channel.close();
            }
            if let Some(subscription) = page.subscription {
                let (conversation, task) = split_key(&key);
                app.invoke::<Value>("detach_shell_task_output", json!({ "conversationId": conversation, "shellTaskId": task, "subscriptionId": subscription }), |_, _| {});
            }
        });
    }
}

fn ensure_page(app: &mut App, key: &str, task_view: &ShellTaskView) {
    if pages(app).pages.contains_key(key) {
        return;
    }
    let wake_task = app.task();
    let term = Terminal::new(
        // Nothing here can accept input: there is no process on the other end. And these are pipes, not a pty: no line
        // discipline turns a program's LF into CRLF on the way here, so without `convert_eol` every line would start
        // where the previous one ended.
        Options { history: HISTORY, blink: false, convert_eol: true, select_all_chord: true },
        Arc::new(|_| {}),
        Arc::new(move || wake_task.post(|_| repaint())),
    );
    let mut term = term;
    term.set_input_enabled(false);
    pages(app).pages.insert(key.to_owned(), Page { term, transcript: String::new(), failure: None, dropped_head_bytes: 0, subscription: None, channel: None, copy_status: None, menu: None });
    subscribe(app, key, &task_view.conversation_id, &task_view.shell_task_id);
}

/// Copies what the control promises: a selection is what the user meant, even one that reads back as nothing but spaces;
/// without one the whole transcript is.
pub(crate) fn copy_text(selection: Option<String>, transcript: &str) -> String {
    selection.unwrap_or_else(|| transcript.to_owned())
}

/// Keeps a page mounted without showing it, so coming back is a repaint.
pub fn keep_shell_task_panel(app: &mut App, task_view: &ShellTaskView) {
    let key = page_key(&task_view.conversation_id, &task_view.shell_task_id);
    let task = app.task();
    let guard_key = key.clone();
    let _guard = use_state(("shell-task-mount", key.as_str()), move || PageGuard { task, key: guard_key });
    ensure_page(app, &key, task_view);
}

/// The page, on screen. `on_stop` asks the command to stop; `stopping` says it was already asked.
pub fn shell_task_panel(app: &mut App, task_view: &ShellTaskView, stopping: bool, on_stop: Option<Rc<dyn Fn(&mut App)>>) -> E {
    let t_ = theme();
    keep_shell_task_panel(app, task_view);
    let key = page_key(&task_view.conversation_id, &task_view.shell_task_id);
    let Some(page) = pages(app).pages.get(&key) else { return div() };
    let (failure, dropped) = (page.failure.clone(), page.dropped_head_bytes);
    let copy_status = page.copy_status;
    let running = task_view.outcome.is_none();
    let failed = task_view.outcome == Some(ShellTaskOutcome::Failed);

    let status_color = if running { t_.accent_text } else if failed { t_.critical } else { t_.text_tertiary };
    let mut status: E = row().items_center().gap(5.0).h(25.0).shrink0().role(Role::Status).aria_live(Live::Polite);
    if running {
        status = status.child(crate::spinner::spinner(12.0, Some(t_.accent_text)));
    }
    status = status.child(text(task_view.status_label()).ty(FINE).color(status_color).nowrap());

    let copied = copy_status.map(|(_, ok)| ok);
    let copy_label = match copied {
        Some(true) => t("已复制", "Copied"),
        Some(false) => t("复制失败", "Copy failed"),
        None => t("复制输出", "Copy output"),
    };
    let copy_key = key.clone();
    let copy: E = row()
        .key(("shell-task-copy", key.as_str()))
        .center()
        .size(25.0, 25.0)
        .radius(CONTROL_RADIUS)
        .shrink0()
        .hover_bg(t_.subtle_hover)
        .press_bg(t_.subtle_pressed)
        .tooltip(copy_label)
        .child(icon(if copied == Some(true) { "Check" } else { "Copy" }, 12.0).color(match copied {
            Some(true) => t_.success,
            Some(false) => t_.critical,
            None => t_.text_secondary,
        }))
        .on_click(move |app: &mut App| {
            let Some(page) = pages(app).pages.get_mut(&copy_key) else { return };
            page.copy_status = None;
            let text = copy_text(page.term.selection_text(), &page.transcript);
            copy_request(app, &copy_key, text);
        });

    let mut header: E = row().items_start().gap(9.0).min_h(34.0).pl(13.0).pr(10.0).py(6.0).shrink0().border_bottom(t_.divider).radii([0.0, 0.0, 10.0, 10.0]).bg(t_.card_secondary);
    header = header.child(div().flex1().max_h(100.0).py(5.0).scroll_y().key(("shell-task-command", key.as_str())).child(text(task_view.command.clone()).ty(MONO_SMALL).selectable()));
    header = header.child(status).child(copy);
    if running && let Some(stop) = on_stop {
        let label = if stopping { t("正在中止命令", "Stopping the command") } else { t("中止命令", "Stop the command") };
        let mut b: E = row().key(("shell-task-stop", key.as_str())).center().size(25.0, 25.0).radius(CONTROL_RADIUS).shrink0().tooltip(label).child(icon("Square", 9.0).color(t_.text_secondary));
        if stopping {
            b = b.disabled(true);
        } else {
            b = b.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(move |app: &mut App| stop(app));
        }
        header = header.child(b);
    }

    let paint_key = key.clone();
    let viewport: E = custom(move |app, ui, r| {
        let id = hash_key(("shell-task-viewport", paint_key.as_str()));
        let Some(page) = pages(app).pages.get_mut(&paint_key) else { return };
        // The copy control's outcome reverts on its own.
        if let Some((until, _)) = page.copy_status {
            if ui.time >= until {
                page.copy_status = None;
            } else {
                ui.wake_after(until - ui.time);
            }
        }
        if let Some(text) = pending_copy(&paint_key) {
            ui.clipboard.set(&text);
            page.copy_status = Some((ui.time + COPY_STATUS_SECS, true));
        }
        page.term.show(ui, id, r);
        if let Some(at) = page.term.take_context_menu() {
            page.menu = Some(at);
        }
    })
    .key(("shell-task-viewport", key.as_str()))
    .flex1()
    .min_w(0.0)
    .min_h(0.0)
    .role(Role::Region)
    .aria_label(t("命令输出（只读）", "Command output (read-only)"));

    let mut body: E = col().flex1().min_w(0.0).min_h(0.0).child(header);
    if dropped > 0 {
        // The page keeps the tail; the model's copy of the same command keeps the head. Saying so is the whole point.
        body = body.child(text(tf("输出过长，已省略开头 {bytes} 字节。模型收到的是开头那一段，这里显示的是结尾。", "Output too long; the first {bytes} bytes were dropped. The model received the beginning; this shows the end.", &[("bytes", &dropped)])).ty(FINE).color(t_.text_secondary).px(13.0).py(6.0).border_bottom(t_.divider).shrink0());
    }
    if let Some(failure) = failure {
        body = body.child(text(failure).ty(FINE).color(t_.critical).px(13.0).py(6.0).border_bottom(t_.divider).shrink0().role(Role::Alert));
    }
    body = body.child(viewport);
    let open_menu = pages(app).pages.get(&key).and_then(|page| page.menu.map(|at| (at, page.term.selection_text())));
    if let Some(((x, y), selection)) = open_menu {
        // The same menu the shells have, less the Paste: there is nothing on the other end to paste to.
        let (menu_key, close_key) = (key.clone(), key.clone());
        let copy = ContextMenuItem::new("copy", t("复制", "Copy")).disabled(selection.is_none()).on_select(move |app: &mut App| {
            if let Some(text) = selection.clone() {
                copy_request(app, &menu_key, text);
            }
        });
        let close = move |app: &mut App| {
            if let Some(page) = pages(app).pages.get_mut(&close_key) {
                page.menu = None;
            }
        };
        body = body.child(context_menu(("shell-task-menu", key.as_str()), ContextMenuProps::new(ContextMenuAnchor::Point { x, y }, t("命令输出（只读）", "Command output (read-only)"), close, vec![ContextMenuSection::new("clipboard", vec![copy])])));
    }
    body
}

thread_local! {
    static PENDING_COPIES: std::cell::RefCell<HashMap<String, String>> = std::cell::RefCell::new(HashMap::new());
}

/// A handler has no clipboard to write to, but the next paint does.
fn copy_request(_app: &mut App, key: &str, text: String) {
    PENDING_COPIES.with(|p| {
        p.borrow_mut().insert(key.to_owned(), text);
    });
    repaint();
}

fn pending_copy(key: &str) -> Option<String> {
    PENDING_COPIES.with(|p| p.borrow_mut().remove(key))
}

/// The emulator behind a mounted page.
pub fn shell_task_terminal<'a>(app: &'a mut App, conversation_id: &str, shell_task_id: &str) -> Option<&'a mut Terminal> {
    pages(app).pages.get_mut(&page_key(conversation_id, shell_task_id)).map(|p| &mut p.term)
}

/// Why a mounted page has nothing to show, when that is so.
pub fn shell_task_failure(app: &mut App, conversation_id: &str, shell_task_id: &str) -> Option<String> {
    pages(app).pages.get(&page_key(conversation_id, shell_task_id)).and_then(|p| p.failure.clone())
}

/// What the copy control would copy from a mounted page.
pub fn shell_task_copy_text(app: &mut App, conversation_id: &str, shell_task_id: &str) -> Option<String> {
    pages(app).pages.get(&page_key(conversation_id, shell_task_id)).map(|p| copy_text(p.term.selection_text(), &p.transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ShellTaskView {
        ShellTaskView { shell_task_id: "shell-1".into(), conversation_id: "c".into(), tool_name: "bash".into(), command: "npm test".into(), stopping: false, outcome: None, exit_code: None, workspace_root: None, cwd: None }
    }

    #[test]
    fn says_how_the_command_stands() {
        let s = snapshot();
        assert_eq!(s.status_label(), "正在运行");
        assert_eq!(ShellTaskView { stopping: true, ..s.clone() }.status_label(), "正在中止");
        assert_eq!(ShellTaskView { outcome: Some(ShellTaskOutcome::Stopped), ..s.clone() }.status_label(), "已中止");
        assert_eq!(ShellTaskView { outcome: Some(ShellTaskOutcome::Failed), exit_code: Some(3), ..s.clone() }.status_label(), "已失败（退出码 3）");
        assert_eq!(ShellTaskView { outcome: Some(ShellTaskOutcome::Failed), ..s.clone() }.status_label(), "已失败");
        assert_eq!(ShellTaskView { outcome: Some(ShellTaskOutcome::Succeeded), ..s }.status_label(), "已完成");
    }

    #[test]
    fn is_named_after_the_shell_and_where_it_ran() {
        let s = snapshot();
        assert_eq!(s.title(), "bash");
        assert_eq!(ShellTaskView { workspace_root: Some("/w".into()), ..s.clone() }.title(), "bash:/w");
        assert_eq!(ShellTaskView { workspace_root: Some("/w".into()), cwd: Some("/w/sub".into()), ..s }.title(), "bash:/w/sub");
    }

    #[test]
    fn marks_stderr_apart_from_stdout() {
        assert_eq!(output_bytes("out", false), b"out");
        assert_eq!(output_bytes("err", true), b"\x1b[2merr\x1b[0m");
    }

    #[test]
    fn the_transcript_keeps_its_end_on_a_character_boundary() {
        let mut t = "世".repeat(MAX_TRANSCRIPT / 3);
        append_transcript(&mut t, "末尾");
        assert!(t.len() <= MAX_TRANSCRIPT);
        assert!(t.ends_with("末尾"));
    }

    #[test]
    fn copies_a_selection_even_of_spaces_and_the_transcript_without_one() {
        assert_eq!(copy_text(Some("   ".into()), "all"), "   ");
        assert_eq!(copy_text(None, "all"), "all");
    }

    #[test]
    fn the_host_s_output_events_read_as_output_and_anything_else_is_ignored() {
        let e: ShellOutputEvent = serde_json::from_value(json!({"type": "output", "text": "hi", "stream": "stderr"})).unwrap();
        assert!(matches!(e, ShellOutputEvent::Output { ref text, ref stream } if text == "hi" && stream == "stderr"));
        let e: ShellOutputEvent = serde_json::from_value(json!({"type": "end"})).unwrap();
        assert!(matches!(e, ShellOutputEvent::Other));
    }
}
