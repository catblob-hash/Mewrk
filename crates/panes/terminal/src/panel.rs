//! A terminal panel on screen: `core`'s state machine joined to the host and to the emulator, and the view of it. This
//! is upstream's `TerminalPanel` minus what `core` holds.
//!
//! Upstream keeps a panel's state in React refs for as long as the component stays mounted. Here a panel lives in the
//! pane's store, and a `use_state` guard stands for being mounted: a view that stops asking for it drops the guard, and
//! that is the unmount, with the same consequences (detach, report idle).

use std::collections::{HashMap, VecDeque};

use mewrk_client::prelude::*;
use mewrk_term::{Options, Output, Sink, Terminal};
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};

use crate::api::{self, TerminalEvent, TerminalLaunchChoice, TerminalSessionState};
use crate::controller::{terminal_session_key, TerminalController};
use crate::core::{Effect, PanelCore};
use crate::spinner::spinner;
use crate::tabs::TerminalTabsState;

/// What upstream passes a panel as callbacks.
#[derive(Clone)]
pub struct PanelHooks {
    /// Whether a command may start now, `beginTerminalCommand`: false loses the race against a Git write.
    pub on_command_start: Rc<dyn Fn(&mut App) -> bool>,
    /// Awaited before every open, for whatever the host must already know when the open reaches it. Runs on a task
    /// thread, with the task to wait through.
    pub before_open: Option<Arc<dyn Fn(&Task) + Send + Sync>>,
    /// Fires once per clean exit (code 0) that ends the session on its own, not one killed by the close control.
    pub on_clean_exit: Rc<dyn Fn(&mut App)>,
}

impl Default for PanelHooks {
    fn default() -> Self {
        Self { on_command_start: Rc::new(|_| true), before_open: None, on_clean_exit: Rc::new(|_| {}) }
    }
}

/// What a panel is given every frame.
pub struct PanelProps<'a> {
    pub conversation_id: &'a str,
    pub terminal_id: &'a str,
    pub label: &'a str,
    pub open: bool,
    pub launch: Option<TerminalLaunchChoice>,
    pub draft_workspace_id: Option<&'a str>,
    pub input_disabled_reason: Option<&'a str>,
    pub hooks: PanelHooks,
}

/// The most `write_terminal` takes at a time, what the host calls `MAX_INPUT_BYTES`. Keystrokes queued together only join
/// up to this; a single paste over it still goes out whole, for the host to refuse by name.
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// One thing for the shell, in the order it happened.
enum Io {
    Write { session: String, data: String },
    Resize { session: String, cols: u16, rows: u16 },
}

pub(crate) struct Panel {
    pub core: PanelCore,
    pub term: Option<Terminal>,
    channel: Option<Channel>,
    hooks: PanelHooks,
    /// Ran once the host has answered a close.
    close_waiters: Vec<Box<dyn FnOnce(&mut App)>>,
    /// What is waiting to go to the shell. Commands run concurrently on the host's runtime, so two sent at once can
    /// arrive either way round, and a line's Enter must not overtake its letters. One goes at a time.
    io: VecDeque<Io>,
    io_busy: bool,
    /// Where the right click that opened the menu landed, while it is open.
    menu: Option<(f32, f32)>,
    /// What the menu asked of the clipboard, which only the next paint holds.
    clipboard: Option<ClipboardAct>,
}

#[derive(Clone, Copy)]
enum ClipboardAct {
    Copy,
    Paste,
}

type CloseAnswer = Box<dyn FnOnce(&mut App, Result<(), String>)>;

/// Everything the pane keeps for as long as the app runs.
#[derive(Default)]
pub struct TerminalStore {
    /// The strip of each conversation. Lasts while the app does, across closing the pane and switching conversations.
    pub tabs: TerminalTabsState,
    pub controller: TerminalController,
    pub(crate) panels: HashMap<String, Panel>,
    close_answers: HashMap<String, Vec<CloseAnswer>>,
}

/// The pane's state.
pub fn store(app: &mut App) -> &mut TerminalStore {
    app.local::<TerminalStore>()
}

/// Stands for a panel being mounted. Dropped from inside the runtime's sweep, so it does nothing but post.
struct MountGuard {
    task: Task,
    key: String,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let key = std::mem::take(&mut self.key);
        self.task.post(move |app| unmount(app, &key));
    }
}

/// The focus it holds, which is also its element's id: a reader lands on it when the terminal has the keyboard.
fn widget_id(key: &str) -> u64 {
    hash_key(("terminal-viewport", key))
}

fn sync_state(app: &mut App, key: &str) {
    let store = store(app);
    if let Some(panel) = store.panels.get(key) {
        let state = panel.core.state();
        store.controller.update(state);
    }
}

fn run(app: &mut App, key: &str, effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::Reset => {
                if let Some(term) = store(app).panels.get_mut(key).and_then(|p| p.term.as_mut()) {
                    term.reset();
                }
            }
            Effect::Feed(bytes) => {
                if let Some(term) = store(app).panels.get(key).and_then(|p| p.term.as_ref()) {
                    term.feed(bytes);
                }
            }
            Effect::Open { attempt, previous } => begin_open(app, key, attempt, previous),
            Effect::Write { session, data } => queue_io(app, key, Io::Write { session, data }),
            Effect::Resize { session, cols, rows } => queue_io(app, key, Io::Resize { session, cols, rows }),
            Effect::Close { attempt } => {
                let Some((conversation, terminal)) = identity(app, key) else { continue };
                let owned = key.to_owned();
                request_close(app, &conversation, &terminal, Box::new(move |app, result| {
                    let waiters = match store(app).panels.get_mut(&owned) {
                        Some(panel) => {
                            panel.core.closed(attempt, result);
                            std::mem::take(&mut panel.close_waiters)
                        }
                        None => Vec::new(),
                    };
                    sync_state(app, &owned);
                    for waiter in waiters {
                        waiter(app);
                    }
                }));
            }
            Effect::Detach { session } => {
                let Some((conversation, terminal)) = identity(app, key) else { continue };
                app.invoke::<Value>("detach_terminal", api::detach_args(&conversation, &terminal, &session), |_, _| {});
            }
            Effect::CleanExit => {
                if let Some(hook) = store(app).panels.get(key).map(|p| p.hooks.on_clean_exit.clone()) {
                    hook(app);
                }
            }
        }
    }
    sync_state(app, key);
}

fn identity(app: &mut App, key: &str) -> Option<(String, String)> {
    store(app).panels.get(key).map(|p| (p.core.conversation_id.clone(), p.core.terminal_id.clone()))
}

fn queue_io(app: &mut App, key: &str, io: Io) {
    let Some(panel) = store(app).panels.get_mut(key) else { return };
    match (panel.io.back_mut(), io) {
        (Some(Io::Write { session: last, data: joined }), Io::Write { session, data }) if *last == session && joined.len() + data.len() <= MAX_INPUT_BYTES => joined.push_str(&data),
        (_, io) => panel.io.push_back(io),
    }
    pump_io(app, key);
}

fn pump_io(app: &mut App, key: &str) {
    let Some(panel) = store(app).panels.get_mut(key) else { return };
    if panel.io_busy {
        return;
    }
    let Some(io) = panel.io.pop_front() else { return };
    panel.io_busy = true;
    let (conversation, terminal) = (panel.core.conversation_id.clone(), panel.core.terminal_id.clone());
    let owned = key.to_owned();
    match io {
        Io::Write { session, data } => {
            let failed = session.clone();
            api::write_terminal(app, &conversation, &terminal, &session, &data, move |app, result| io_result(app, &owned, &failed, result));
        }
        Io::Resize { session, cols, rows } => {
            let failed = session.clone();
            api::resize_terminal(app, &conversation, &terminal, &session, cols, rows, move |app, result| io_result(app, &owned, &failed, result));
        }
    }
}

fn io_result(app: &mut App, key: &str, session: &str, result: Result<(), String>) {
    let Some(panel) = store(app).panels.get_mut(key) else { return };
    panel.io_busy = false;
    if let Err(message) = result {
        panel.core.io_failed(session, message);
        // What followed was for the same shell, and would only fail the same way.
        panel.io.retain(|io| !matches!(io, Io::Write { session: s, .. } | Io::Resize { session: s, .. } if s == session));
        sync_state(app, key);
    }
    pump_io(app, key);
}

fn on_channel(app: &mut App, key: &str, attempt: u64, value: Value) {
    let Ok(event) = serde_json::from_value::<TerminalEvent>(value) else { return };
    let Some(panel) = store(app).panels.get_mut(key) else { return };
    let effects = panel.core.on_event(attempt, event);
    run(app, key, effects);
}

fn begin_open(app: &mut App, key: &str, attempt: u64, previous: Option<String>) {
    let owned = key.to_owned();
    let channel = {
        let owned = owned.clone();
        app.open_channel(move |app, value| on_channel(app, &owned, attempt, value))
    };
    let Some(panel) = store(app).panels.get_mut(key) else {
        channel.close();
        return;
    };
    // Whatever the previous attempt was still listening to is over.
    if let Some(old) = panel.channel.replace(channel.clone()) {
        old.close();
    }
    let (conversation, terminal) = (panel.core.conversation_id.clone(), panel.core.terminal_id.clone());
    let (launch, draft) = (panel.core.launch, panel.core.draft_workspace_id.clone());
    let before_open = panel.hooks.before_open.clone();
    app.spawn(move |task| {
        if let Some(session) = previous {
            // A stopped session may already have released its attachment.
            let _ = task.invoke::<Value>("detach_terminal", api::detach_args(&conversation, &terminal, &session));
        }
        let current = |task: &Task| {
            let owned = owned.clone();
            task.update(move |app| store(app).panels.get(&owned).is_some_and(|p| p.core.attempt() == attempt))
        };
        if !current(&task) {
            return;
        }
        if let Some(before) = before_open {
            // A failure here is the open's to report: the host names what it could not find.
            before(&task);
            if !current(&task) {
                return;
            }
        }
        let size = {
            let owned = owned.clone();
            task.update(move |app| store(app).panels.get(&owned).and_then(|p| p.term.as_ref()).map_or((80, 24), Terminal::size))
        };
        let args = api::open_terminal_args(&conversation, &terminal, size.0.max(2), size.1.max(1), launch, draft.as_deref(), &channel);
        let result = task.invoke::<api::OpenTerminalResult>("open_terminal", args);
        task.post(move |app| {
            let Some(panel) = store(app).panels.get_mut(&owned) else {
                channel.close();
                return;
            };
            let effects = panel.core.opened(attempt, result);
            run(app, &owned, effects);
        });
    });
}

/// Closes the backend terminal and removes the session. Concurrent calls for the same terminal share one task.
pub fn request_close(app: &mut App, conversation_id: &str, terminal_id: &str, done: CloseAnswer) {
    let key = terminal_session_key(conversation_id, terminal_id);
    let store_ = store(app);
    store_.close_answers.entry(key.clone()).or_default().push(done);
    if !store_.controller.begin_close(conversation_id, terminal_id) {
        return;
    }
    let (conversation, terminal) = (conversation_id.to_owned(), terminal_id.to_owned());
    app.invoke::<Value>("close_terminal", api::close_args(conversation_id, terminal_id), move |app, result| {
        let answers = {
            let store_ = store(app);
            store_.controller.finish_close(&conversation, &terminal, result.is_ok());
            store_.close_answers.remove(&key).unwrap_or_default()
        };
        for answer in answers {
            answer(app, result.clone().map(|_| ()));
        }
    });
}

/// Asks the panel to kill its session, the handle upstream's tab drives. `settled` runs once that is over, whichever way
/// it went: a refusal is shown as the session's failure. With no panel mounted the request goes straight to the host.
pub fn close_panel(app: &mut App, conversation_id: &str, terminal_id: &str, settled: Box<dyn FnOnce(&mut App)>) {
    let key = terminal_session_key(conversation_id, terminal_id);
    let Some(panel) = store(app).panels.get_mut(&key) else {
        request_close(app, conversation_id, terminal_id, Box::new(move |app, _| settled(app)));
        return;
    };
    if panel.core.phase == api::TerminalPhase::Closing {
        settled(app);
        return;
    }
    panel.close_waiters.push(settled);
    let effects = panel.core.close_session();
    run(app, &key, effects);
}

fn start(app: &mut App, key: &str) {
    if let Some(panel) = store(app).panels.get_mut(key) {
        let effects = panel.core.start_session();
        run(app, key, effects);
    }
}

fn on_output(app: &mut App, key: &str, output: Output) {
    let Some(panel) = store(app).panels.get_mut(key) else { return };
    let effects = match output {
        Output::Input(data) | Output::Reply(data) => {
            let starts_command = data.contains(['\r', '\n']) && panel.core.can_send();
            let hook = panel.hooks.on_command_start.clone();
            // Asked outside the store's borrow: the hook looks at the rest of the app.
            let allowed = !starts_command || hook(app);
            let Some(panel) = store(app).panels.get_mut(key) else { return };
            panel.core.on_data(data, || allowed)
        }
        Output::Resize { cols, lines } => panel.core.on_resize(cols, lines),
    };
    run(app, key, effects);
}

fn unmount(app: &mut App, key: &str) {
    let Some(mut panel) = store(app).panels.remove(key) else { return };
    if let Some(channel) = panel.channel.take() {
        channel.close();
    }
    let effects = panel.core.unmount();
    let last_report: TerminalSessionState = panel.core.state();
    let (conversation, terminal) = (panel.core.conversation_id.clone(), panel.core.terminal_id.clone());
    let waiters = std::mem::take(&mut panel.close_waiters);
    let queued = std::mem::take(&mut panel.io);
    drop(panel);
    let detach = effects.into_iter().find_map(|effect| if let Effect::Detach { session } = effect { Some(session) } else { None });
    if !queued.is_empty() || detach.is_some() {
        // What was typed before leaving still reaches the shell, ahead of letting go of it.
        app.spawn(move |task| {
            for io in queued {
                let _ = match io {
                    Io::Write { session, data } => task.invoke::<Value>("write_terminal", json!({ "conversationId": conversation, "terminalId": terminal, "sessionId": session, "data": data })),
                    Io::Resize { session, cols, rows } => task.invoke::<Value>("resize_terminal", json!({ "conversationId": conversation, "terminalId": terminal, "sessionId": session, "cols": cols, "rows": rows })),
                };
            }
            if let Some(session) = detach {
                let _ = task.invoke::<Value>("detach_terminal", api::detach_args(&conversation, &terminal, &session));
            }
        });
    }
    store(app).controller.update(last_report);
    for waiter in waiters {
        waiter(app);
    }
}

/// Takes a panel's props for this frame, mounting it if this is the first time it has been asked for.
fn mount(app: &mut App, props: &PanelProps<'_>) -> String {
    let key = terminal_session_key(props.conversation_id, props.terminal_id);
    let task = app.task();
    let _guard = use_state(("terminal-mount", key.as_str()), {
        let key = key.clone();
        move || MountGuard { task, key }
    });
    let initial = store(app).controller.current().get(&key).cloned();
    let store_ = store(app);
    let panel = store_.panels.entry(key.clone()).or_insert_with(|| Panel {
        core: PanelCore::new(props.conversation_id, props.terminal_id, props.label, initial.as_ref()),
        term: None,
        channel: None,
        hooks: PanelHooks::default(),
        close_waiters: Vec::new(),
        io: VecDeque::new(),
        io_busy: false,
        menu: None,
        clipboard: None,
    });
    panel.core.label = props.label.to_owned();
    // A tab keeps one choice for its whole life, and a change here does not restart a running shell: these are read
    // when the panel opens its session.
    panel.core.launch = props.launch.unwrap_or_default();
    panel.core.draft_workspace_id = props.draft_workspace_id.map(str::to_owned);
    panel.core.input_disabled_reason = props.input_disabled_reason.map(str::to_owned);
    panel.core.set_open(props.open);
    panel.hooks = props.hooks.clone();
    let wants_terminal = props.open && panel.term.is_none();
    if wants_terminal {
        let (sink, wake) = {
            let sink_task = app.task();
            let wake_task = app.task();
            let sink_key = key.clone();
            let sink: Sink = Arc::new(move |output| {
                let key = sink_key.clone();
                sink_task.post(move |app| on_output(app, &key, output));
            });
            let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || wake_task.post(|_| repaint()));
            (sink, wake)
        };
        if let Some(panel) = store(app).panels.get_mut(&key) {
            panel.term = Some(Terminal::new(Options::default(), sink, wake));
        }
    }
    key
}

/// Keeps a panel mounted without showing it: the tab is not the selected one, or the pane is hidden. Every tab stays
/// mounted like this; a hidden tab keeps its shell, its scrollback and its box, so coming back to it is a repaint.
pub fn keep_panel(app: &mut App, props: PanelProps<'_>) {
    mount(app, &PanelProps { open: false, ..props });
}

/// The panel, on screen.
pub fn terminal_panel(app: &mut App, props: PanelProps<'_>) -> E {
    let t_ = theme();
    let key = mount(app, &props);
    let store_ = store(app);
    let Some(panel) = store_.panels.get(&key) else { return div() };
    let (veiled, settled, label, disabled) = (panel.core.veiled(), panel.core.settled(), panel.core.status_label(), panel.core.input_disabled_reason.is_some());
    let connecting = panel.core.phase == api::TerminalPhase::Connecting;
    let (region, reason) = (panel.core.label.clone(), panel.core.input_disabled_reason.clone());
    let error = panel.core.phase == api::TerminalPhase::Error;
    let menu = panel.menu.map(|at| (at, panel.term.as_ref().is_some_and(Terminal::has_selection), panel.core.input_accepted()));

    let paint_key = key.clone();
    let viewport: E = custom(move |app, ui, r| {
        let id = widget_id(&paint_key);
        let Some(panel) = store(app).panels.get_mut(&paint_key) else { return };
        let accepted = panel.core.input_accepted();
        if let Some(term) = panel.term.as_mut() {
            term.set_input_enabled(accepted);
            if std::mem::take(&mut panel.core.want_focus) {
                ui.mem.focus = id;
            }
            match panel.clipboard.take() {
                Some(ClipboardAct::Copy) => {
                    if let Some(text) = term.selection_text() {
                        ui.clipboard.set(&text);
                    }
                }
                Some(ClipboardAct::Paste) => {
                    if let Some(text) = ui.clipboard.get() {
                        term.paste(&text);
                    }
                }
                None => {}
            }
            term.show(ui, id, r);
            if let Some(at) = term.take_context_menu() {
                panel.menu = Some(at);
            }
        }
        // Only a terminal that never started starts: a session that ended keeps its verdict on screen until the retry.
        if panel.core.should_start() {
            start(app, &paint_key);
        }
    })
    .key(("terminal-viewport", key.as_str()))
    .focus_under(widget_id(&key))
    .keeps_tab()
    .focusable()
    .flex1()
    .min_w(0.0)
    .min_h(0.0)
    .when(disabled, |e| e.opacity(0.72))
    // Upstream's xterm is opened without `screenReaderMode`, so all a reader has of it is its one input and never its cells, whose rows
    // are `aria-hidden` there.
    .role(Role::TextInput)
    .aria_label(t("终端输入", "Terminal input"))
    .aria_disabled(disabled || connecting)
    .maybe_disabled_reason(reason.clone());

    let mut body: E = col().key(("terminal-panel", key.as_str())).flex1().min_w(0.0).min_h(0.0).role(Role::Region).aria_label(region).child(viewport);
    // The reference shell draws no chrome of its own: status lives in the accessibility tree, and the overlays carry what the eye needs.
    if !settled && a11y_active() {
        let status = row().absolute().size(1.0, 1.0).pass_through().role(Role::Status).aria_label(label.clone());
        body = body.child(match reason {
            Some(reason) => status.aria_description(reason),
            None => status,
        });
    }
    if let Some(((x, y), selected, accepted)) = menu {
        body = body.child(terminal_context_menu(&key, x, y, selected, accepted));
    }
    if veiled {
        // While connecting or terminating a centered spinner stands in for the body.
        body = body.child(row().absolute().inset(0.0).center().pass_through().bg(t_.mica_base).child(spinner(16.0, None)));
    }
    if settled {
        // A settled session replaces the body with a centered card: the verdict, then the only way back.
        let retry_key = key.clone();
        let retry: E = row()
            .key(("terminal-retry", key.as_str()))
            .items_center()
            .gap(6.0)
            .h(26.0)
            .px(10.0)
            .radius(6.0)
            .bg(t_.control_fill)
            .hover_bg(t_.control_fill_hover)
            .press_bg(t_.control_fill_pressed)
            .border(t_.control_stroke)
            .child(icon("RotateCcw", 13.0).color(t_.text_primary))
            .child(text(t("重试", "Retry")).ty(CAPTION).nowrap())
            .on_click(move |app: &mut App| start(app, &retry_key));
        body = body.child(
            col()
                .absolute()
                .inset(0.0)
                .center()
                .gap(10.0)
                .bg(t_.mica_base)
                .role(if error { Role::Alert } else { Role::Status })
                .aria_live(Live::Polite)
                .child(text(label).ty(CAPTION).color(if error { t_.critical } else { t_.text_secondary }).text_center().max_w(320.0).px(16.0))
                .child(retry),
        );
    }
    body
}

/// The right click's menu. Upstream leaves it to the system: on a right click xterm moves its helper textarea under the pointer
/// with the selection in it, so the native Copy and Paste act on the terminal. Copy reads what is selected; Paste is off while
/// the shell takes no input, as the textarea is read-only then.
fn terminal_context_menu(key: &str, x: f32, y: f32, selected: bool, accepted: bool) -> E {
    let act = |id: &str, label: &str, act: ClipboardAct| {
        let owned = key.to_owned();
        ContextMenuItem::new(id, label).on_select(move |app: &mut App| {
            if let Some(panel) = store(app).panels.get_mut(&owned) {
                panel.clipboard = Some(act);
            }
            repaint();
        })
    };
    let items = vec![act("copy", t("复制", "Copy"), ClipboardAct::Copy).disabled(!selected), act("paste", t("粘贴", "Paste"), ClipboardAct::Paste).disabled(!accepted)];
    let owned = key.to_owned();
    let close = move |app: &mut App| {
        if let Some(panel) = store(app).panels.get_mut(&owned) {
            panel.menu = None;
        }
    };
    context_menu(("terminal-menu", key), ContextMenuProps::new(ContextMenuAnchor::Point { x, y }, t("终端输入", "Terminal input"), close, vec![ContextMenuSection::new("clipboard", items)]))
}

/// What a mounted panel says about itself, `statusLabel`, which upstream keeps in the accessibility tree.
pub fn panel_status(app: &mut App, conversation_id: &str, terminal_id: &str) -> Option<String> {
    store(app).panels.get(&terminal_session_key(conversation_id, terminal_id)).map(|p| p.core.status_label())
}

/// The session state of a mounted panel.
pub fn panel_state(app: &mut App, conversation_id: &str, terminal_id: &str) -> Option<TerminalSessionState> {
    store(app).panels.get(&terminal_session_key(conversation_id, terminal_id)).map(|p| p.core.state())
}

/// The emulator of a mounted panel, once it has been opened.
pub fn panel_terminal<'a>(app: &'a mut App, conversation_id: &str, terminal_id: &str) -> Option<&'a mut Terminal> {
    store(app).panels.get_mut(&terminal_session_key(conversation_id, terminal_id)).and_then(|p| p.term.as_mut())
}

/// The retry, as a test or a menu might call it.
pub fn retry_panel(app: &mut App, conversation_id: &str, terminal_id: &str) {
    start(app, &terminal_session_key(conversation_id, terminal_id));
}
