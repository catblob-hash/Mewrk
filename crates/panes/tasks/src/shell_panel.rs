//! `components/ShellTaskPanel.tsx`: one command's output, read-only. It owns no process: it subscribes to a command the
//! model started and draws what comes back.

use std::sync::Mutex;
use std::time::Instant;

use mewrk_client::prelude::*;

use fluent::Weight;

use crate::ansi_view::AnsiView;
use crate::shell_tasks::{open_shell_task_output, ShellTaskOutcome, ShellTaskOutputHandle, ShellTaskSnapshot};
use crate::util::{utf16_len, utf16_suffix};
use crate::widgets::{mono, spinner, stop_glyph, ts};

/// How long the copy control reports its outcome before reverting to its label.
const COPY_STATUS_SECONDS: f32 = 1.6;
/// Longest transcript kept for the copy control, in UTF-16 units. Like the host's buffer it keeps the end.
const MAX_TRANSCRIPT_LENGTH: usize = 4 * 1024 * 1024;

fn append_transcript(transcript: &mut String, text: &str) {
    transcript.push_str(text);
    if utf16_len(transcript) > MAX_TRANSCRIPT_LENGTH {
        *transcript = utf16_suffix(transcript, MAX_TRANSCRIPT_LENGTH).to_owned();
    }
}

/// Status word and whether the command has ended.
pub fn shell_status_label(task: &ShellTaskSnapshot) -> String {
    match task.outcome {
        None if task.stopping => t("正在中止", "Stopping").to_owned(),
        None => t("正在运行", "Running").to_owned(),
        Some(ShellTaskOutcome::Stopped) => t("已中止", "Stopped").to_owned(),
        Some(ShellTaskOutcome::Failed) => match task.exit_code {
            Some(code) => tf("已失败（退出码 {code}）", "Failed (exit {code})", &[("code", &code)]),
            None => t("已失败", "Failed").to_owned(),
        },
        Some(ShellTaskOutcome::Succeeded) => t("已完成", "Finished").to_owned(),
    }
}

enum Incoming {
    Text(String, bool),
    Opened(ShellTaskOutputHandle),
    Failed(String),
}

/// Shared with the channel and the `open` answer, both of which must be `Send`.
#[derive(Default)]
struct Inbox {
    pending: Vec<Incoming>,
    /// The page went away while its subscribe was in flight.
    cancelled: bool,
    subscription_id: Option<u64>,
}

/// One subscription, for as long as the page keeps asking for it. Dropping it detaches the sink it installed and no
/// other: a detach that reached the host after a newer subscribe would otherwise leave that page a snapshot that never
/// updates.
struct Subscription {
    inbox: Arc<Mutex<Inbox>>,
    channel: Channel,
    task: Task,
    conversation_id: String,
    shell_task_id: String,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|p| p.into_inner());
        inbox.cancelled = true;
        let id = inbox.subscription_id;
        drop(inbox);
        self.channel.close();
        if let Some(id) = id {
            detach_on_thread(self.task.clone(), self.conversation_id.clone(), self.shell_task_id.clone(), id);
        }
    }
}

fn detach_on_thread(task: Task, conversation_id: String, shell_task_id: String, subscription_id: u64) {
    let _ = std::thread::Builder::new().name("shell-output-detach".to_owned()).spawn(move || {
        let _ = task.invoke::<Value>("detach_shell_task_output", json!({ "conversationId": conversation_id, "shellTaskId": shell_task_id, "subscriptionId": subscription_id }));
    });
}

/// Upstream's third state, "copy failed", has no place here: `Clipboard::set` cannot report a refusal.
#[derive(Clone, Copy, PartialEq)]
enum CopyStatus {
    Idle,
    Success,
}

pub struct ShellPage {
    pub view: AnsiView,
    /// Everything written since the subscribe, verbatim: xterm's buffer was not a transcript, and neither is a grid,
    /// which pads the viewport and trims every row.
    pub transcript: String,
    pub failure: Option<String>,
    pub dropped_head_bytes: u64,
    copy_status: (CopyStatus, Option<Instant>),
    copy_requested: bool,
    subscription: Option<Subscription>,
}

impl ShellPage {
    fn new() -> Self {
        Self { view: AnsiView::new(), transcript: String::new(), failure: None, dropped_head_bytes: 0, copy_status: (CopyStatus::Idle, None), copy_requested: false, subscription: None }
    }

    /// Output of one pipe: the terminal gets it, and the transcript keeps it verbatim beside it.
    pub fn write(&mut self, text: &str, stderr: bool) {
        if text.is_empty() {
            return;
        }
        append_transcript(&mut self.transcript, text);
        self.view.write(text, stderr);
    }

    fn drain(&mut self) {
        let Some(sub) = &self.subscription else { return };
        let pending = std::mem::take(&mut sub.inbox.lock().unwrap_or_else(|p| p.into_inner()).pending);
        for message in pending {
            match message {
                Incoming::Text(text, stderr) => self.write(&text, stderr),
                Incoming::Opened(handle) => {
                    self.dropped_head_bytes = handle.dropped_head_bytes;
                    // The snapshot lost the pipe boundaries, so it is replayed as plain output.
                    self.write(&handle.snapshot, false);
                }
                Incoming::Failed(reason) => self.failure = Some(reason),
            }
        }
    }
}

fn subscribe(app: &App, conversation_id: &str, shell_task_id: &str) -> Subscription {
    let inbox: Arc<Mutex<Inbox>> = Arc::default();
    let sink = inbox.clone();
    let channel = app.open_channel(move |_, event| {
        if event.get("type").and_then(Value::as_str) != Some("output") {
            return;
        }
        let text = event.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
        let stderr = event.get("stream").and_then(Value::as_str) == Some("stderr");
        sink.lock().unwrap_or_else(|p| p.into_inner()).pending.push(Incoming::Text(text, stderr));
        repaint();
    });
    let answered = inbox.clone();
    let (conversation, task_id, task) = (conversation_id.to_owned(), shell_task_id.to_owned(), app.task());
    open_shell_task_output(app, conversation_id, shell_task_id, &channel, move |_, result| {
        let mut inbox = answered.lock().unwrap_or_else(|p| p.into_inner());
        match result {
            Ok(handle) => {
                // A finished command installs no sink and answers zero.
                let id = handle.subscription_id;
                if inbox.cancelled {
                    if id != 0 {
                        detach_on_thread(task, conversation, task_id, id);
                    }
                    return;
                }
                if id != 0 {
                    inbox.subscription_id = Some(id);
                }
                inbox.pending.push(Incoming::Opened(handle));
            }
            Err(error) => {
                if !inbox.cancelled {
                    let reason = if error.trim().is_empty() { t("无法读取命令输出", "Unable to read the command output").to_owned() } else { error };
                    inbox.pending.push(Incoming::Failed(reason));
                }
            }
        }
        repaint();
    });
    Subscription { inbox, channel, task: app.task(), conversation_id: conversation_id.to_owned(), shell_task_id: shell_task_id.to_owned() }
}

pub struct ShellTaskPanelProps {
    pub task: ShellTaskSnapshot,
    /// Whether this page is the one on screen. A hidden page stays subscribed and draws nothing.
    pub open: bool,
    pub on_stop: Option<Rc<dyn Fn(&mut App)>>,
    pub stopping: bool,
}

pub fn shell_page_state(conversation_id: &str, shell_task_id: &str) -> Local<ShellPage> {
    use_state(("shell-task-page", conversation_id, shell_task_id), ShellPage::new)
}

/// The page of one shell command. Its state lives while the page keeps being asked for; when the shell stops drawing it
/// the subscription detaches.
pub fn shell_task_panel(app: &mut App, p: ShellTaskPanelProps) -> E {
    let c = theme();
    let task = &p.task;
    let page = shell_page_state(&task.conversation_id, &task.shell_task_id);
    {
        let mut guard = page.borrow_mut();
        if guard.subscription.is_none() {
            guard.subscription = Some(subscribe(app, &task.conversation_id, &task.shell_task_id));
        }
        guard.drain();
    }
    let id = hash_key(("shell-task-output", task.shell_task_id.as_str()));
    if !p.open {
        return div();
    }
    let running = task.outcome.is_none();
    let status_color = match task.outcome {
        None => c.accent_text,
        Some(ShellTaskOutcome::Failed) => c.critical,
        _ => c.text_tertiary,
    };
    let mut status = row().key(("shell-status", task.shell_task_id.as_str())).items_center().gap(5.0).h(25.0).shrink0().role(Role::Status).aria_live(Live::Polite);
    if running {
        status = status.child(spinner(12.0, status_color));
    }
    status = status.child(text(shell_status_label(task)).ty(ts(9.0, 12.0, Weight::Regular)).color(status_color).nowrap());

    let (copy_status, _) = page.borrow().copy_status;
    let (copy_label, copy_icon, copy_color) = match copy_status {
        CopyStatus::Success => (t("已复制", "Copied"), "Check", c.success),
        CopyStatus::Idle => (t("复制输出", "Copy output"), "Copy", c.text_secondary),
    };
    let copy_page = page.clone();
    let copy = row().key(("shell-copy", task.shell_task_id.as_str())).center().size(25.0, 25.0).radius(5.0).shrink0().hover_bg(c.subtle_hover).tooltip(copy_label).on_click(move |_| copy_page.borrow_mut().copy_requested = true).child(icon(copy_icon, 12.0).color(copy_color));

    let mut header = row()
        .items_start()
        .gap(9.0)
        .min_h(34.0)
        .shrink0()
        .pt(6.0)
        .pb(6.0)
        .pl(13.0)
        .pr(10.0)
        .bg(c.card_secondary)
        .border_bottom(c.divider)
        .radii([0.0, 0.0, 10.0, 10.0])
        .child(col().key(("shell-command", task.shell_task_id.as_str())).flex1().max_h(100.0).py(5.0).scroll_y().child(text(task.command.clone()).ty(mono(10.0, 15.0)).color(c.text_primary).selectable()))
        .child(status)
        .child(copy);
    if let (true, Some(on_stop)) = (running, p.on_stop.clone()) {
        let tip = if p.stopping { t("正在中止命令", "Stopping the command") } else { t("中止命令", "Stop the command") };
        let mut stop = row().key(("shell-stop", task.shell_task_id.as_str())).center().size(25.0, 25.0).radius(5.0).shrink0().tooltip(tip).child(stop_glyph(c.text_secondary));
        if p.stopping {
            stop = stop.on_click(|_| {}).disabled(true);
        } else {
            stop = stop.hover_bg(c.subtle_hover).on_click(move |app| on_stop(app));
        }
        header = header.child(stop);
    }

    let (dropped, failure) = {
        let g = page.borrow();
        (g.dropped_head_bytes, g.failure.clone())
    };
    let notice_ty = ts(9.5, 14.0, Weight::Regular);
    let mut pane = col().key(("shell-task-panel", task.shell_task_id.as_str())).w_full().h_full().min_w(0.0).min_h(0.0).clip().child(header);
    if dropped > 0 {
        // The page keeps the tail; the model's copy keeps the head. A suffix presented as the whole would be a quiet lie.
        let message = tf("输出过长，已省略开头 {bytes} 字节。模型收到的是开头那一段，这里显示的是结尾。", "Output too long; the first {bytes} bytes were dropped. The model received the beginning; this shows the end.", &[("bytes", &dropped)]);
        pane = pane.child(text(message).key(("shell-truncation", task.shell_task_id.as_str())).ty(notice_ty).color(c.text_tertiary).py(6.0).px(13.0).border_bottom(c.divider));
    }
    if let Some(failure) = failure {
        pane = pane.child(text(failure).key(("shell-failure", task.shell_task_id.as_str())).ty(notice_ty).color(c.critical).py(6.0).px(13.0).border_bottom(c.divider).role(Role::Alert));
    }
    let view_page = page.clone();
    let viewport = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let mut guard = view_page.borrow_mut();
        let g = &mut *guard;
        if std::mem::take(&mut g.copy_requested) {
            // A selection is what the user meant; without one the whole transcript is.
            let copied = if g.view.has_selection() { g.view.selection_text().unwrap_or_default() } else { g.transcript.clone() };
            ui.clipboard.set(&copied);
            g.copy_status = (CopyStatus::Success, Some(Instant::now()));
            ui.animating = true;
        }
        if let (_, Some(at)) = g.copy_status {
            let left = COPY_STATUS_SECONDS - at.elapsed().as_secs_f32();
            if left <= 0.0 {
                g.copy_status = (CopyStatus::Idle, None);
                ui.animating = true;
            } else {
                ui.wake_after(left);
            }
        }
        g.view.show(ui, id, r);
    })
    .key(id)
    .flex1()
    .role(Role::Region)
    .aria_label(t("命令输出（只读）", "Command output (read-only)"));
    pane.child(viewport)
}

