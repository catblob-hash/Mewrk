//! The pane on a real host, with a real shell: `App::start` over a temp data dir and a temp project, no GPU.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use fluent::el::{rect_of, render, set_a11y_active, set_theme, set_window_size, take_a11y_update, theme};
use fluent::ui::{Clipboard, Input, Key, KeyPress, UiMemory};
use fluent::{Scene, TextCtx, Theme, Ui};
use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_terminal::*;
use mewrk_ui_common::context_menu::{menu_panel_key, menu_row_key};

const WIN: (f32, f32) = (1000.0, 700.0);
const SCHEMA_VERSION: u32 = 6;
const CONV: &str = "draft-conversation-1";

/// One test at a time: `HOME` is process-wide, and the host keeps statics of its own.
static SERIAL: Mutex<()> = Mutex::new(());

fn fresh_dir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-terminal-{tag}-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A clipboard a test can read and fill.
#[derive(Default)]
struct Held(Option<String>);

impl Clipboard for Held {
    fn get(&mut self) -> Option<String> {
        self.0.clone()
    }
    fn set(&mut self, text: &str) {
        self.0 = Some(text.to_owned());
    }
}

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    clipboard: Held,
    project: PathBuf,
    _serial: MutexGuard<'static, ()>,
}

fn start() -> (Rig, App) {
    start_with(true)
}

/// `seeded`: a directory project in the document before the host starts, since the picker that grants new ones needs a
/// person. Without it the document is a fresh install's, with its temporary project and its full catalogue of tools.
fn start_with(seeded: bool) -> (Rig, App) {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (dir, home, project) = (fresh_dir("data"), fresh_dir("home"), fresh_dir("project"));
    if seeded {
        let document = json!({
            "schemaVersion": SCHEMA_VERSION,
            "globalSettings": {},
            "workspaces": [{ "id": "ws1", "name": "project", "kind": "directory", "path": project.to_str().unwrap(), "createdAt": "2026-01-01T00:00:00.000Z", "defaultConversationPresetId": "", "conversations": [] }],
            "tools": [{ "name": "read", "label": "Read", "description": "", "category": "filesystem", "dangerous": false, "parameters": [] }],
            "capabilities": { "skills": [], "mcps": [] },
        });
        std::fs::write(dir.join("document.v1.json"), serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }
    // The shell's startup files are read from here, and a test must not run the machine's own.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("LANG", "en_US.UTF-8");
    }
    set_theme(Theme::dark());
    set_window_size(WIN.0, WIN.1);
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(false);
    (Rig { scene: Scene::default(), text: TextCtx::new(1.0), mem: UiMemory::default(), input: Input::default(), clipboard: Held::default(), project, _serial: serial }, app)
}

fn props<'a>(on_last: Rc<dyn Fn(&mut App)>, pane_open: bool, gate: Rc<dyn Fn(&mut App) -> bool>, reason: Option<&'a str>) -> TerminalPaneProps<'a> {
    TerminalPaneProps {
        owner_id: CONV,
        draft_workspace_id: Some("ws1"),
        pane_open,
        input_disabled_reason: reason,
        read_only_task: None,
        read_only_stopping: false,
        on_stop_task: None,
        command_gate: gate,
        before_open: None,
        on_last_tab_closed: on_last,
        new_terminal: NewTerminalMenu { workspaces: vec![TerminalWorkspace { path: "/p".into(), title: None, machine_icon: "Folder", shells: vec![TerminalShell::Zsh, TerminalShell::Bash] }], selected: 1, disabled: false },
    }
}

fn frame(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) {
    app.tick();
    rig.scene.clear();
    let root = view(app);
    let mut ui = Ui::new(&mut rig.scene, &mut rig.text, theme(), &rig.input, &mut rig.mem, &mut rig.clipboard, 1.0, WIN, 0.016, 0.0);
    render(root, app, &mut ui);
    rig.input.end_frame();
}

fn until(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, what: &str, done: &mut dyn FnMut(&mut App) -> bool) {
    let start = Instant::now();
    loop {
        frame(rig, app, view);
        if done(app) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: status {:?}, text {:?}", panel_status(app, CONV, "terminal-1"), panel_terminal(app, CONV, "terminal-1").map(|t| t.text().into_iter().filter(|l| !l.is_empty()).collect::<Vec<_>>()));
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn shown(app: &mut App, terminal_id: &str, needle: &str) -> bool {
    panel_terminal(app, CONV, terminal_id).is_some_and(|t| t.text().iter().any(|l| l == needle))
}

fn type_line(rig: &mut Rig, line: &str) {
    rig.input.text = line.to_owned();
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
}

fn live_count(app: &App, conversation: &str) -> u64 {
    app.host.invoke_blocking("live_terminal_count", json!({ "conversationId": conversation })).unwrap().as_u64().unwrap()
}

fn zsh() -> Option<TerminalLaunchChoice> {
    Some(TerminalLaunchChoice { workspace: Some(1), shell: Some(TerminalShell::Zsh) })
}

/// The whole of it through the real pipe: a shell opened by the host, keys through `write_terminal`, bytes back through
/// the `TerminalEvent` channel, into the grid. Then the replay on reattach, which is what makes the pipe byte-accurate.
#[test]
fn echo_goes_through_the_host_and_comes_back_into_the_grid_and_a_reattach_replays_it() {
    let (mut rig, mut app) = start();
    let last_closed = Rc::new(std::cell::Cell::new(false));
    let on_last: Rc<dyn Fn(&mut App)> = {
        let last_closed = last_closed.clone();
        Rc::new(move |_| last_closed.set(true))
    };
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    ensure_terminal_tab(&mut app, CONV, zsh());
    assert_eq!(store(&mut app).tabs.layout(CONV).tabs[0].id, "terminal-1");
    let mut view = {
        let (on_last, gate) = (on_last.clone(), gate.clone());
        move |app: &mut App| -> E {
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), None));
            col().child(pane.header).child(pane.body)
        }
    };

    until(&mut rig, &mut app, &mut view, "the shell to be ready", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    let state = panel_state(&mut app, CONV, "terminal-1").unwrap();
    assert_eq!(state.phase, TerminalPhase::Running);
    assert_eq!(state.cwd, rig.project.to_str().unwrap());
    assert_eq!(state.shell, "zsh");
    assert_eq!(live_count(&app, CONV), 1);
    assert_eq!(store(&mut app).controller.session(CONV, "terminal-1").unwrap().session_id, state.session_id);

    // The panel took focus when the shell came up, so typing reaches it. The echoed command line spells the output apart.
    type_line(&mut rig, "printf 'he%s\\n' llo");
    until(&mut rig, &mut app, &mut view, "the echo in the grid", &mut |app| shown(app, "terminal-1", "hello"));

    // The grid is the size the pane gave it, and the shell agrees.
    let (cols, rows) = panel_terminal(&mut app, CONV, "terminal-1").unwrap().size();
    assert_ne!((cols, rows), (80, 24), "the first look fitted it");
    type_line(&mut rig, "stty size");
    let size = format!("{rows} {cols}");
    until(&mut rig, &mut app, &mut view, "the shell to know its size", &mut |app| shown(app, "terminal-1", &size));

    // A multi-byte character and a colour, in one piece, through JSON and out the far side.
    type_line(&mut rig, "printf '\\xe4\\xb8\\x96\\xe7\\x95\\x8c \\033[31mred\\033[0m\\n'");
    until(&mut rig, &mut app, &mut view, "UTF-8 in the grid", &mut |app| shown(app, "terminal-1", "世界 red"));

    // The command-state frames came through too: it has run something now, and is back at its prompt.
    until(&mut rig, &mut app, &mut view, "the command to settle", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("待机"));

    // Stop looking: the guard goes, the panel detaches, and the shell lives on at the host.
    let session = panel_state(&mut app, CONV, "terminal-1").unwrap().session_id;
    let mut nothing = |_: &mut App| -> E { div() };
    for _ in 0..5 {
        frame(&mut rig, &mut app, &mut nothing);
    }
    assert!(panel_state(&mut app, CONV, "terminal-1").is_none(), "unmounted");
    assert!(store(&mut app).controller.session(CONV, "terminal-1").is_none(), "no busy flag outlives the panel");
    assert_eq!(live_count(&app, CONV), 1, "the shell is still running");
    assert_eq!(store(&mut app).tabs.layout(CONV).tabs.len(), 1, "its tab too");

    // Coming back replays what the shell wrote, byte for byte, into a new emulator.
    until(&mut rig, &mut app, &mut view, "the replay", &mut |app| shown(app, "terminal-1", "世界 red"));
    assert!(shown(&mut app, "terminal-1", "hello"));
    assert_eq!(panel_state(&mut app, CONV, "terminal-1").unwrap().session_id, session, "the same session");

    // And it still takes input.
    type_line(&mut rig, "printf 'ag%s\\n' ain");
    until(&mut rig, &mut app, &mut view, "input after the reattach", &mut |app| shown(app, "terminal-1", "again"));

    // Ending the shell takes its tab, and as the last one the pane.
    close_terminal_tab(&mut app, CONV, "terminal-1", on_last.clone());
    assert_eq!(panel_status(&mut app, CONV, "terminal-1").as_deref(), Some("正在终止"));
    until(&mut rig, &mut app, &mut view, "the tab to go", &mut |app| store(app).tabs.layout(CONV).tabs.is_empty());
    assert!(last_closed.get(), "the pane goes with the last tab");
    assert_eq!(live_count(&app, CONV), 0);
    assert!(store(&mut app).controller.current().is_empty());
}

/// Commands run concurrently on the host's runtime, so keystrokes sent one by one could arrive in any order. A line typed
/// a key per frame, with its Enter last, has to come out as it went in.
#[test]
fn keystrokes_reach_the_shell_in_the_order_they_were_typed() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate) = (on_last.clone(), gate.clone());
        move |app: &mut App| -> E {
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), None));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    for round in 0..5 {
        let word = format!("{round}abcdefghijklmnopqrstuvwxyz0123456789{round}");
        let line = format!("printf '{}%s\\n' done", word);
        for c in line.chars() {
            rig.input.text = c.to_string();
            frame(&mut rig, &mut app, &mut view);
        }
        rig.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
        frame(&mut rig, &mut app, &mut view);
        let wanted = format!("{word}done");
        until(&mut rig, &mut app, &mut view, "the whole line, in order", &mut |app| shown(app, "terminal-1", &wanted));
    }
}

/// Upstream's `onCleanExit`: `exit` at the prompt folds the terminal away, and a failing exit stays on screen.
#[test]
fn a_clean_exit_folds_the_tab_away_and_a_failing_one_stays_for_the_retry() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    ensure_terminal_tab(&mut app, CONV, zsh());
    open_terminal_in(&mut app, CONV, 1, TerminalShell::Zsh);
    assert_eq!(store(&mut app).tabs.layout(CONV).active_id.as_deref(), Some("terminal-2"));
    let mut view = {
        let (on_last, gate) = (on_last.clone(), gate.clone());
        move |app: &mut App| -> E {
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), None));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the second shell", &mut |app| panel_status(app, CONV, "terminal-2").as_deref() == Some("空终端"));
    type_line(&mut rig, "exit 3");
    until(&mut rig, &mut app, &mut view, "the failing exit", &mut |app| panel_status(app, CONV, "terminal-2").as_deref() == Some("已退出（代码 3）"));
    assert_eq!(store(&mut app).tabs.layout(CONV).tabs.len(), 2, "a failing exit keeps its tab");
    assert!(store(&mut app).controller.session(CONV, "terminal-2").is_none(), "and is no longer a live session");

    // The retry starts another shell in the same tab.
    retry_panel(&mut app, CONV, "terminal-2");
    until(&mut rig, &mut app, &mut view, "the retry", &mut |app| panel_status(app, CONV, "terminal-2").as_deref() == Some("空终端"));
    type_line(&mut rig, "exit 0");
    until(&mut rig, &mut app, &mut view, "the clean exit to fold the tab", &mut |app| store(app).tabs.layout(CONV).tabs.len() == 1);
    assert_eq!(store(&mut app).tabs.layout(CONV).active_id.as_deref(), Some("terminal-1"), "focus falls to what is left");

    // The first shell only starts now that it is shown: becoming visible for the first time asks for one.
    until(&mut rig, &mut app, &mut view, "the first shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
}

/// Upstream's "blocks terminal input while a Git mutation owns the workspace" and "does not send a command when the
/// workspace lock loses the start race", end to end.
#[test]
fn a_git_write_stops_typing_and_a_lost_race_sends_nothing() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let allow = Rc::new(std::cell::Cell::new(true));
    let gate: Rc<dyn Fn(&mut App) -> bool> = {
        let allow = allow.clone();
        Rc::new(move |_| allow.get())
    };
    let reason = Rc::new(std::cell::Cell::new(false));
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate, reason) = (on_last.clone(), gate.clone(), reason.clone());
        move |app: &mut App| -> E {
            let why = reason.get().then_some("Git 写操作进行中");
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), why));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));

    reason.set(true);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(panel_status(&mut app, CONV, "terminal-1").as_deref(), Some("Git 写操作进行中"));
    type_line(&mut rig, "printf 'bl%s\\n' ocked");
    for _ in 0..10 {
        frame(&mut rig, &mut app, &mut view);
    }
    std::thread::sleep(Duration::from_millis(200));
    frame(&mut rig, &mut app, &mut view);
    assert!(!shown(&mut app, "terminal-1", "blocked"));

    reason.set(false);
    allow.set(false);
    frame(&mut rig, &mut app, &mut view);
    assert_eq!(panel_status(&mut app, CONV, "terminal-1").as_deref(), Some("空终端"));
    // The line's characters go on; its Enter does not, which is what holds it back.
    rig.input.keys.push(KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
    frame(&mut rig, &mut app, &mut view);
    for _ in 0..5 {
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(panel_status(&mut app, CONV, "terminal-1").as_deref(), Some("空终端"), "a lost race starts nothing");

    allow.set(true);
    type_line(&mut rig, "printf 'wo%s\\n' rks");
    until(&mut rig, &mut app, &mut view, "typing once allowed", &mut |app| shown(app, "terminal-1", "works"));
}

/// A press and release at a key's rectangle, with a frame on either side.
fn click_key(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash) {
    let r = rect_of(key).expect("on screen");
    rig.input.pointer = Some((r.x + r.w * 0.5, r.y + r.h * 0.5));
    frame(rig, app, view);
    rig.input.pressed = true;
    rig.input.down = true;
    frame(rig, app, view);
    rig.input.down = false;
    rig.input.released = true;
    frame(rig, app, view);
    frame(rig, app, view);
}

/// What the system's menu offers over xterm, through the real pipe: Copy takes what the click selected, Paste reaches the
/// shell, and neither survives what took input away.
#[test]
fn a_right_click_offers_copy_and_paste_and_paste_waits_for_a_shell_that_takes_input() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    let reason = Rc::new(std::cell::RefCell::new(None::<String>));
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate, reason) = (on_last.clone(), gate.clone(), reason.clone());
        move |app: &mut App| -> E {
            let held = reason.borrow().clone();
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), held.as_deref()));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    type_line(&mut rig, "printf 'he%s\\n' llo");
    until(&mut rig, &mut app, &mut view, "the word", &mut |app| shown(app, "terminal-1", "hello"));

    let key = terminal_session_key(CONV, "terminal-1");
    let menu = ("terminal-menu", key.as_str());
    let line = panel_terminal(&mut app, CONV, "terminal-1").unwrap().text().iter().position(|l| l == "hello").unwrap();
    let viewport = rect_of(("terminal-viewport", key.as_str())).unwrap();
    let on_word = (viewport.x + 8.0 + 10.0, viewport.y + 4.0 + line as f32 * 17.0 + 8.0);
    let open_menu = |rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E| {
        rig.input.pointer = Some(on_word);
        frame(rig, app, view);
        rig.input.right_pressed = true;
        frame(rig, app, view);
        frame(rig, app, view);
        frame(rig, app, view);
    };

    open_menu(&mut rig, &mut app, &mut view);
    click_key(&mut rig, &mut app, &mut view, menu_row_key(menu, &[], "copy"));
    assert_eq!(rig.clipboard.0.as_deref(), Some("hello"), "the word the click landed on");
    assert!(rect_of(menu_panel_key(menu, &[])).is_none(), "the menu goes with the choice");

    rig.clipboard.0 = Some("printf 'pa%s\\n' sted".to_owned());
    open_menu(&mut rig, &mut app, &mut view);
    click_key(&mut rig, &mut app, &mut view, menu_row_key(menu, &[], "paste"));
    type_line(&mut rig, "");
    until(&mut rig, &mut app, &mut view, "what was pasted to run", &mut |app| shown(app, "terminal-1", "pasted"));

    // A Git write has the checkout: the row is there, and does nothing.
    *reason.borrow_mut() = Some("Git 操作进行中".to_owned());
    rig.clipboard.0 = Some("printf 'no%s\\n' pe".to_owned());
    open_menu(&mut rig, &mut app, &mut view);
    click_key(&mut rig, &mut app, &mut view, menu_row_key(menu, &[], "paste"));
    rig.input.pressed = true;
    rig.input.pointer = Some((1.0, 1.0));
    frame(&mut rig, &mut app, &mut view);
    rig.input.pressed = false;
    frame(&mut rig, &mut app, &mut view);
    *reason.borrow_mut() = None;
    click_key(&mut rig, &mut app, &mut view, ("terminal-viewport", key.as_str()));
    type_line(&mut rig, "printf 'do%s\\n' ne");
    until(&mut rig, &mut app, &mut view, "the line after", &mut |app| shown(app, "terminal-1", "done"));
    assert!(!shown(&mut app, "terminal-1", "nope"), "nothing went to a shell that takes no input");
}

/// A closed pane keeps its shells mounted, and the tabs a conversation has stay through it.
#[test]
fn the_panes_close_button_leaves_every_shell_running() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    let pane_open = Rc::new(std::cell::Cell::new(true));
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate, pane_open) = (on_last.clone(), gate.clone(), pane_open.clone());
        move |app: &mut App| -> E {
            let open = pane_open.get();
            let pane = terminal_pane(app, props(on_last.clone(), open, gate.clone(), None));
            if open { col().child(pane.header).child(pane.body) } else { div() }
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    type_line(&mut rig, "printf 'be%s\\n' fore");
    until(&mut rig, &mut app, &mut view, "output", &mut |app| shown(app, "terminal-1", "before"));

    pane_open.set(false);
    for _ in 0..5 {
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(panel_state(&mut app, CONV, "terminal-1").unwrap().phase, TerminalPhase::Running, "still mounted");
    assert_eq!(live_count(&app, CONV), 1);

    // Output arriving while hidden lands in the same emulator: the shell is sleeping when the pane closes, then speaks.
    pane_open.set(true);
    frame(&mut rig, &mut app, &mut view);
    assert!(shown(&mut app, "terminal-1", "before"), "coming back is a repaint, no replay needed");
    assert_eq!(panel_status(&mut app, CONV, "terminal-1").as_deref(), Some("待机"));
    // Ending it, so the test's shell does not outlive the test.
    let on_last2 = on_last.clone();
    close_terminal_tab(&mut app, CONV, "terminal-1", on_last2);
    until(&mut rig, &mut app, &mut view, "the end", &mut |app| store(app).tabs.layout(CONV).tabs.is_empty());
}

/// The read-only page on a command the host no longer has: the failure is shown rather than an empty terminal.
#[test]
fn a_read_only_page_for_a_command_the_host_has_let_go_of_says_so() {
    let (mut rig, mut app) = start();
    let task = ShellTaskView { shell_task_id: "gone".into(), conversation_id: CONV.into(), tool_name: "bash".into(), command: "npm test".into(), stopping: false, outcome: None, exit_code: None, workspace_root: None, cwd: None };
    let mut view = {
        let task = task.clone();
        move |app: &mut App| -> E { shell_task_panel(app, &task, false, None) }
    };
    until(&mut rig, &mut app, &mut view, "the subscription to fail", &mut |app| shell_task_failure(app, CONV, "gone").is_some());
    assert_eq!(shell_task_failure(&mut app, CONV, "gone").as_deref(), Some("该命令已不在任务列表中"), "the host's own reason");
    assert_eq!(shell_task_copy_text(&mut app, CONV, "gone").as_deref(), Some(""));

    // The page takes what it is fed through the emulator the view shows, with bare LFs as full line breaks.
    let term = shell_task_terminal(&mut app, CONV, "gone").unwrap();
    term.feed(b"one\ntwo\n".to_vec());
    term.flush();
    let lines = term.text();
    assert_eq!(&lines[..2], ["one", "two"]);
}

/// A real command through the host's shell tool, on the read-only page: the snapshot on subscribing, what streams after,
/// and both pipes told apart. `App.readOnlyTerminal.test.tsx` and `ShellTaskPanel.test.tsx`, with nothing mocked.
#[test]
fn the_read_only_page_shows_a_real_command_and_its_tab_leads_the_strip() {
    let (mut rig, mut app) = start_with(false);
    let conversation = "conversation-read-only";
    let workspace_id = app.doc.workspaces.first().expect("a fresh document has the temporary project").id.clone();
    let record = json!({ "id": conversation, "title": "t", "createdAt": "2026-07-24T00:00:00Z", "updatedAt": "2026-07-24T00:00:00Z", "settings": { "enabledTools": ["bash"], "securityLevel": "full_access" }, "contexts": [] });
    app.host.invoke_blocking("create_conversation", json!({ "workspaceId": workspace_id, "conversation": record })).unwrap();
    let request = json!({ "conversationId": conversation, "workspacePath": "", "toolName": "bash", "input": { "command": "printf 'early\\n'; sleep 0.6; printf 'late-out\\n'; printf 'late-err\\n' 1>&2; exit 3" } });
    app.invoke::<Value>("execute_tool", json!({ "request": request, "approvalNonce": null }), |_, result| {
        result.unwrap();
    });
    let list = |app: &App| -> Vec<ShellTaskView> { serde_json::from_value(app.host.invoke_blocking("list_shell_tasks", json!({ "conversationId": conversation })).unwrap()).unwrap() };
    let start = Instant::now();
    let task = loop {
        app.tick();
        if let Some(task) = list(&app).pop() {
            break task;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the command to start");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(task.status_label(), "正在运行");
    assert!(task.title().starts_with("bash"), "{}", task.title());

    // The row points the page at the command, and opening the pane starts no shell for it.
    show_read_only_terminal(&mut app, conversation, &task.shell_task_id);
    ensure_terminal_tab(&mut app, conversation, zsh());
    assert!(store(&mut app).tabs.layout(conversation).tabs.is_empty());
    let current = Rc::new(std::cell::RefCell::new(task.clone()));
    let mut view = {
        let current = current.clone();
        move |app: &mut App| -> E {
            let mut p = props(Rc::new(|_| {}), true, Rc::new(|_| true), None);
            p.owner_id = conversation;
            p.draft_workspace_id = None;
            p.read_only_task = Some(current.borrow().clone());
            p.on_stop_task = Some(Rc::new(|_, _| {}));
            let pane = terminal_pane(app, p);
            col().child(row().h(32.0).child(pane.header)).child(pane.body)
        }
    };
    let id = task.shell_task_id.clone();
    let has = |app: &mut App, needle: &str| shell_task_terminal(app, conversation, &id).is_some_and(|t| t.text().iter().any(|l| l == needle));
    until(&mut rig, &mut app, &mut view, "the snapshot or the stream", &mut |app| has(app, "early"));
    let stop_key = format!("{conversation}/{id}");
    frame(&mut rig, &mut app, &mut view);
    assert!(fluent::el::rect_of(("shell-task-stop", stop_key.as_str())).is_some(), "a stop control while it runs");
    until(&mut rig, &mut app, &mut view, "what streams after", &mut |app| has(app, "late-out") && has(app, "late-err"));

    // Nothing is sent anywhere: this page has no process to talk to.
    assert!(store(&mut app).controller.current().is_empty());
    assert_eq!(live_count(&app, conversation), 0);

    // It ended as the host says, with its code.
    let start = Instant::now();
    loop {
        let task = list(&app).pop().unwrap();
        if task.outcome.is_some() {
            assert_eq!(task.status_label(), "已失败（退出码 3）");
            *current.borrow_mut() = task;
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the command to end");
        frame(&mut rig, &mut app, &mut view);
        std::thread::sleep(Duration::from_millis(5));
    }

    frame(&mut rig, &mut app, &mut view);
    frame(&mut rig, &mut app, &mut view);
    assert!(fluent::el::rect_of(("shell-task-stop", stop_key.as_str())).is_none(), "and none once it has ended");

    // Copying with nothing selected takes the transcript, verbatim.
    let copied = shell_task_copy_text(&mut app, conversation, &id).unwrap();
    assert!(copied.contains("early\n") && copied.contains("late-out\n") && copied.contains("late-err\n"), "{copied:?}");

    // A shell opened beside it goes after it, and closing the page ends nothing.
    open_terminal_in(&mut app, conversation, 1, TerminalShell::Zsh);
    assert_eq!(terminal_strip_ids(&store(&mut app).tabs.layout(conversation)), [READ_ONLY_TERMINAL_TAB_ID, "terminal-1"]);
    close_terminal_tab(&mut app, conversation, READ_ONLY_TERMINAL_TAB_ID, Rc::new(|_| panic!("a shell is left")));
    assert_eq!(terminal_strip_ids(&store(&mut app).tabs.layout(conversation)), ["terminal-1"]);
}

// ---------------------------------------------------------------- the accessibility tree

type Tree = Vec<(accesskit::NodeId, accesskit::Node)>;

fn a11y_tree(rig: &mut Rig, app: &mut App, view: &mut dyn FnMut(&mut App) -> E) -> Tree {
    set_a11y_active(true);
    for _ in 0..4 {
        frame(rig, app, view);
    }
    take_a11y_update().expect("a client gets the tree").nodes
}

fn said<'a>(tree: &'a Tree, words: &str) -> &'a accesskit::Node {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(words) || (node.role() == Role::Label && node.value() == Some(words))).unwrap_or_else(|| panic!("nothing says {words:?}: {:?}", tree.iter().filter_map(|(_, n)| n.label().or(n.value())).collect::<Vec<_>>()))
}

fn nameless(tree: &Tree) -> Vec<String> {
    tree.iter()
        .filter(|(_, node)| matches!(node.role(), Role::Button | Role::Link | Role::TextInput | Role::MultilineTextInput | Role::Tab | Role::Switch | Role::CheckBox | Role::MenuItem | Role::ComboBox) || node.supports_action(accesskit::Action::Click))
        .filter(|(_, node)| node.label().is_none_or(str::is_empty) && node.labelled_by().is_empty() && (matches!(node.role(), Role::TextInput | Role::MultilineTextInput) || node.value().is_none_or(str::is_empty)))
        .map(|(id, node)| format!("{:?} {id:?}", node.role()))
        .collect()
}

/// Upstream opens xterm without `screenReaderMode`: a reader has the input and the status, and what the shell prints is not in the tree.
#[test]
fn a_reader_has_the_terminals_input_and_status_and_as_upstream_not_its_cells() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate) = (on_last.clone(), gate.clone());
        move |app: &mut App| -> E {
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), None));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    type_line(&mut rig, "printf 'he%s\\n' llo");
    until(&mut rig, &mut app, &mut view, "the word", &mut |app| shown(app, "terminal-1", "hello"));

    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    let input = said(&tree, "终端输入");
    assert_eq!(input.role(), Role::TextInput);
    assert!(!input.is_disabled());
    assert!(tree.iter().any(|(_, n)| n.role() == Role::Region && n.label() == Some("zsh")), "the panel goes by its tab's name");
    let status = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Status).expect("the status");
    assert!(status.label().is_some_and(|l| !l.is_empty()));
    assert_eq!(status.live(), Some(accesskit::Live::Polite));
    assert!(!tree.iter().any(|(_, n)| n.label() == Some("hello") || n.value() == Some("hello")), "the cells are `aria-hidden` upstream");
    assert_eq!(nameless(&tree), Vec::<String>::new());

    // Holding the keyboard, the terminal is where a reader is.
    let key = terminal_session_key(CONV, "terminal-1");
    click_key(&mut rig, &mut app, &mut view, ("terminal-viewport", key.as_str()));
    set_a11y_active(false);
    let id = tree.iter().find(|(_, n)| n.label() == Some("终端输入")).unwrap().0;
    set_a11y_active(true);
    for _ in 0..4 {
        frame(&mut rig, &mut app, &mut view);
    }
    assert_eq!(take_a11y_update().expect("the whole tree").focus, id);
    set_a11y_active(false);
}

/// `role={phase === "error" ? "alert" : "status"}` on the card that says how the session ended, and a named way back.
#[test]
fn a_session_that_ended_says_so_and_offers_the_retry() {
    let (mut rig, mut app) = start();
    let on_last: Rc<dyn Fn(&mut App)> = Rc::new(|_| {});
    let gate: Rc<dyn Fn(&mut App) -> bool> = Rc::new(|_| true);
    ensure_terminal_tab(&mut app, CONV, zsh());
    let mut view = {
        let (on_last, gate) = (on_last.clone(), gate.clone());
        move |app: &mut App| -> E {
            let pane = terminal_pane(app, props(on_last.clone(), true, gate.clone(), None));
            col().child(pane.header).child(pane.body)
        }
    };
    until(&mut rig, &mut app, &mut view, "the shell", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("空终端"));
    type_line(&mut rig, "exit 3");
    until(&mut rig, &mut app, &mut view, "the failing exit", &mut |app| panel_status(app, CONV, "terminal-1").as_deref() == Some("已退出（代码 3）"));
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    let verdict = said(&tree, "已退出（代码 3）");
    assert_eq!((verdict.role(), verdict.live()), (Role::Status, Some(accesskit::Live::Polite)));
    assert_eq!(said(&tree, "重试").role(), Role::Button);
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_read_only_page_says_how_the_command_stands_and_what_went_wrong() {
    let (mut rig, mut app) = start();
    let task = ShellTaskView { shell_task_id: "gone".into(), conversation_id: CONV.into(), tool_name: "bash".into(), command: "npm test".into(), stopping: false, outcome: None, exit_code: None, workspace_root: None, cwd: None };
    let mut view = {
        let task = task.clone();
        move |app: &mut App| -> E { shell_task_panel(app, &task, false, Some(Rc::new(|_| {}))) }
    };
    until(&mut rig, &mut app, &mut view, "the subscription to fail", &mut |app| shell_task_failure(app, CONV, "gone").is_some());
    let tree = a11y_tree(&mut rig, &mut app, &mut view);
    assert_eq!(said(&tree, "正在运行").role(), Role::Status);
    assert_eq!(said(&tree, "该命令已不在任务列表中").role(), Role::Alert);
    assert_eq!(said(&tree, "命令输出（只读）").role(), Role::Region);
    assert_eq!(said(&tree, "复制输出").role(), Role::Button);
    assert_eq!(said(&tree, "中止命令").role(), Role::Button);
    said(&tree, "npm test");
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}
