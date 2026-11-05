//! A shell command through the real host: started by `execute_tool`, followed by the push events, drawn by the page from
//! a real output channel, and stopped by the page's own control. Plus the emulator's rules on their own.

use std::rc::Rc;

use crate::common::*;
use fluent::el::*;
use fluent::ui::Key;
use mewrk_client::model::{Conversation, SecurityLevel};
use mewrk_client::App;
use mewrk_pane_tasks::ansi_view::AnsiView;
use mewrk_pane_tasks::shell_panel::*;
use mewrk_pane_tasks::shell_tasks::{self, ShellTaskOutcome, ShellTaskSnapshot, ShellTaskStore};
use mewrk_pane_tasks::task_container::*;
use serde_json::{json, Value};

#[test]
fn treats_a_bare_lf_as_a_full_line_break() {
    let mut view = AnsiView::new();
    view.write("one\ntwo\nthree\n", false);
    assert_eq!(&view.visible_lines()[..3], ["one", "two", "three"]);
    // A CRLF is one break too, not two.
    let mut view = AnsiView::new();
    view.write("a\r\nb\r\n", false);
    assert_eq!(&view.visible_lines()[..2], ["a", "b"]);
}

#[test]
fn marks_stderr_apart_from_stdout_and_leaves_escape_codes_out_of_the_text() {
    let mut view = AnsiView::new();
    view.write("out\n", false);
    view.write("warn\n", true);
    view.write("\x1b[31mred\x1b[0m\n", false);
    assert_eq!(&view.visible_lines()[..3], ["out", "warn", "red"]);
    assert!(!view.is_dim(0, 0) && view.is_dim(1, 0) && !view.is_dim(2, 0));
}

#[test]
fn a_carriage_return_redraws_its_line_as_a_terminal_does() {
    let mut view = AnsiView::new();
    view.write("progress 10%\rprogress 99%\n", false);
    assert_eq!(view.visible_lines()[0], "progress 99%");
}

#[test]
fn selects_everything_and_keeps_the_scrollback() {
    let mut view = AnsiView::new();
    assert!(!view.has_selection());
    for i in 0..100 {
        view.write(&format!("line {i}\n"), false);
    }
    view.select_all();
    assert!(view.has_selection());
    let text = view.selection_text().unwrap();
    assert!(text.contains("line 0\n") && text.contains("line 99\n"), "{text}");
}

/// A conversation in the fresh document's temporary workspace, with the shell tool on and no approval to wait for.
fn at_conversation(app: &mut App) -> (String, String) {
    let workspace_id = app.doc.workspaces.first().expect("a fresh document has the temporary workspace").id.clone();
    let id = "conv-shell-round-trip".to_owned();
    let conversation: Conversation = serde_json::from_value(json!({ "id": id, "title": "New task", "createdAt": "2026-07-24T00:00:00Z", "updatedAt": "2026-07-24T00:00:00Z", "settings": { "enabledTools": ["bash"], "securityLevel": "full_access" }, "contexts": [] })).expect("a conversation");
    assert_eq!(conversation.settings.security_level, SecurityLevel::FullAccess);
    app.host.invoke_blocking("create_conversation", json!({ "workspaceId": workspace_id, "conversation": conversation })).expect("create_conversation");
    (workspace_id, id)
}

fn run_command(app: &App, conversation: &str, command: &str) {
    let request = json!({ "conversationId": conversation, "workspacePath": "", "toolName": "bash", "input": { "command": command } });
    app.invoke::<Value>("execute_tool", json!({ "request": request, "approvalNonce": null }), |_, result| {
        if let Err(e) = result {
            eprintln!("execute_tool: {e}");
        }
    });
}

#[test]
fn a_real_command_runs_streams_ends_and_stops_through_the_host() {
    let mut rig = Rig::new("shell-round-trip");
    let (_, conversation) = at_conversation(&mut rig.app);
    shell_tasks::install(&mut rig.app);

    let shown = conversation.clone();
    let stops = counter();
    let stop_count = stops.clone();
    let mut view = move |app: &mut App| -> E {
        let tasks = app.local::<ShellTaskStore>().for_conversation(&shown);
        let Some(task) = tasks.last().cloned() else { return div() };
        let (conversation_id, id) = (task.conversation_id.clone(), task.shell_task_id.clone());
        let stop_count = stop_count.clone();
        col().child(shell_task_panel(
            app,
            ShellTaskPanelProps {
                task,
                open: true,
                on_stop: Some(Rc::new(move |app: &mut App| {
                    stop_count.set(stop_count.get() + 1);
                    shell_tasks::stop_shell_task(app, &conversation_id, &id, |_, r| {
                        r.expect("stop_shell_task");
                    });
                })),
                stopping: false,
            },
        ))
    };

    // Started, printed on both pipes, and failed with a code.
    run_command(&rig.app, &conversation, "printf 'early\\n'; sleep 1; printf 'late-out\\n'; printf 'late-err\\n' 1>&2; printf '\\033[31mcolored\\033[0m\\n'; exit 3");
    rig.until(&mut view, "the command's row", &mut |app| !app.local::<ShellTaskStore>().for_conversation(&conversation).is_empty());
    let task = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).pop().unwrap();
    assert_eq!(task.tool_name, "bash");
    assert!(task.outcome.is_none(), "{task:?}");
    assert!(on_screen(("shell-stop", task.shell_task_id.as_str())), "a running command offers a stop control");
    let id = task.shell_task_id.clone();

    let conv2 = conversation.clone();
    rig.until(&mut view, "the command's end", &mut |app| app.local::<ShellTaskStore>().for_conversation(&conv2).iter().any(|t| t.shell_task_id == id && t.outcome.is_some()));
    for _ in 0..50 {
        rig.frame(&mut view);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let done = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).pop().unwrap();
    assert_eq!((done.outcome, done.exit_code), (Some(ShellTaskOutcome::Failed), Some(3)));
    assert!(done.ended_at.is_some());
    assert_eq!(shell_status_label(&done), "已失败（退出码 3）");
    assert!(!on_screen(("shell-stop", id.as_str())), "no process is left to stop");

    let page = shell_page_state(&conversation, &id);
    {
        let p = page.borrow();
        for line in ["early\n", "late-out\n", "late-err\n"] {
            assert!(p.transcript.contains(line), "{:?}", p.transcript);
        }
        assert!(p.transcript.contains("\u{1b}[31mcolored"), "the transcript is verbatim: {:?}", p.transcript);
        let lines = p.view.visible_lines();
        assert_eq!(lines[0], "early", "{lines:?}");
        assert!(lines.iter().any(|l| l == "colored"), "the grid has no escape codes: {lines:?}");
        let err_row = lines.iter().position(|l| l == "late-err").unwrap();
        assert!(p.view.is_dim(err_row, 0), "a live stderr chunk is dimmed");
        assert_eq!(p.dropped_head_bytes, 0);
        assert!(p.failure.is_none());
    }
    drop(page);

    // The page copies its transcript when nothing is selected.
    rig.click(&mut view, ("shell-copy", id.as_str()));
    assert!(rig.clipboard.0.borrow().contains("late-out\n"), "{:?}", rig.clipboard.0.borrow());

    // A task row says the same as the page, and `list_shell_tasks` agrees with the push events.
    let items = derive_task_items(&TaskSources { conversation_id: Some(conversation.clone()), shell_tasks: rig.app.local::<ShellTaskStore>().for_conversation(&conversation), ..Default::default() }, &task_container_messages());
    assert_eq!((items[0].state, items[0].error.as_deref()), (TaskItemState::Failed, Some("已失败（退出码 3）")));
    let before = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).len();
    shell_tasks::reconcile(&rig.app, &conversation);
    for _ in 0..30 {
        rig.frame(&mut view);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(rig.app.local::<ShellTaskStore>().for_conversation(&conversation).len(), before);

    // A long command, stopped by the page's own control.
    run_command(&rig.app, &conversation, "printf 'waiting\\n'; sleep 60");
    let conv3 = conversation.clone();
    let first = id.clone();
    rig.until(&mut view, "the second command's row", &mut |app| app.local::<ShellTaskStore>().for_conversation(&conv3).iter().any(|t| t.shell_task_id != first));
    let long = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).pop().unwrap();
    assert_ne!(long.shell_task_id, id);
    for _ in 0..3 {
        rig.frame(&mut view);
    }
    rig.click(&mut view, ("shell-stop", long.shell_task_id.as_str()));
    assert_eq!(stops.get(), 1);
    let conv4 = conversation.clone();
    let long_id = long.shell_task_id.clone();
    rig.until(&mut view, "the stop to land", &mut |app| app.local::<ShellTaskStore>().for_conversation(&conv4).iter().any(|t| t.shell_task_id == long_id && t.outcome.is_some()));
    let stopped = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).into_iter().find(|t| t.shell_task_id == long_id).unwrap();
    assert_eq!(stopped.outcome, Some(ShellTaskOutcome::Stopped));
    assert_eq!(shell_status_label(&stopped), "已中止");

    // Going away detaches, and coming back replays what the host still holds.
    let mut nothing = |_: &mut App| -> E { div() };
    for _ in 0..5 {
        rig.frame(&mut nothing);
    }
    for _ in 0..40 {
        rig.frame(&mut view);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let page = shell_page_state(&conversation, &long_id);
    assert!(page.borrow().transcript.contains("waiting\n"), "{:?}", page.borrow().transcript);
}

fn page_for(rig_task: &ShellTaskSnapshot) -> impl FnMut(&mut App) -> E + use<> {
    let task = rig_task.clone();
    move |app: &mut App| -> E { col().child(shell_task_panel(app, ShellTaskPanelProps { task: task.clone(), open: true, on_stop: None, stopping: false })) }
}

#[test]
fn says_so_when_the_hosts_buffer_dropped_the_beginning_and_copies_a_selection_rather_than_the_transcript() {
    let mut rig = Rig::new("shell-truncated");
    let (_, conversation) = at_conversation(&mut rig.app);
    shell_tasks::install(&mut rig.app);
    let mut blank = |_: &mut App| -> E { div() };
    // The page stays closed until the command has ended, so all it gets is what the host still holds: the 256 KiB tail.
    run_command(&rig.app, &conversation, "printf 'the-head\\n'; yes xxxxxxxxxxxxxxx | head -c 400000; printf '\\n\\033[31mthe-tail\\033[0m\\n'");
    let c = conversation.clone();
    rig.until(&mut blank, "the command's end", &mut |app| app.local::<ShellTaskStore>().for_conversation(&c).iter().any(|t| t.outcome.is_some()));
    let task = rig.app.local::<ShellTaskStore>().for_conversation(&conversation).pop().unwrap();
    assert_eq!(task.outcome, Some(ShellTaskOutcome::Succeeded), "{task:?}");
    let id = task.shell_task_id.clone();
    let mut view = page_for(&task);
    let (c, i) = (conversation.clone(), id.clone());
    rig.until(&mut view, "the snapshot", &mut |_| shell_page_state(&c, &i).borrow().dropped_head_bytes > 0);
    rig.settle(&mut view);
    assert!(on_screen(("shell-truncation", id.as_str())));
    let page = shell_page_state(&conversation, &id);
    assert!(page.borrow().transcript.contains("the-tail") && !page.borrow().transcript.contains("the-head"));
    assert!(page.borrow().view.visible_lines().iter().any(|l| l == "the-tail"));
    // A selection is what the user meant; it reads off the grid, with no escape codes in it.
    let r = rect_of(hash_key(("shell-task-output", id.as_str()))).unwrap();
    rig.click_at(&mut view, r.x + 40.0, r.y + 40.0);
    rig.press_key(&mut view, Key::Char('a'), true);
    rig.click(&mut view, ("shell-copy", id.as_str()));
    let copied = rig.clipboard.0.borrow().clone();
    assert!(copied.contains("the-tail") && !copied.contains('\u{1b}'), "{}", &copied[copied.len().saturating_sub(60)..]);
    // With nothing selected the header copies the transcript, verbatim.
    page.borrow_mut().view.clear_selection();
    rig.click(&mut view, ("shell-copy", id.as_str()));
    assert!(rig.clipboard.0.borrow().contains("\u{1b}[31mthe-tail"));
}

#[test]
fn reports_a_subscription_failure_instead_of_showing_an_empty_terminal() {
    let mut rig = Rig::new("shell-missing");
    let task = ShellTaskSnapshot { shell_task_id: "no-such-command".into(), conversation_id: "no-such-conversation".into(), tool_name: "bash".into(), command: "x".into(), started_at: "2026-07-20T01:00:00Z".into(), ..Default::default() };
    let mut view = page_for(&task);
    rig.until(&mut view, "the host's answer", &mut |_| shell_page_state("no-such-conversation", "no-such-command").borrow().failure.is_some());
    rig.settle(&mut view);
    assert!(on_screen(("shell-failure", "no-such-command")));
}

#[test]
fn the_store_follows_the_hosts_events_and_an_eviction_outlives_an_older_list() {
    let mut store = ShellTaskStore::default();
    let row = |id: &str, outcome: Value| json!({ "shellTaskId": id, "conversationId": "c", "toolName": "bash", "command": "x", "stopping": false, "startedAt": "2026-07-20T01:00:00Z", "endedAt": null, "outcome": outcome, "exitCode": null, "background": false });
    store.apply_event(&json!({ "type": "shellTaskStarted", "task": row("a", Value::Null) }));
    store.apply_event(&json!({ "type": "shellTaskStarted", "task": row("b", Value::Null) }));
    // Replaced, never removed: the row carries how the command went.
    store.apply_event(&json!({ "type": "shellTaskEnded", "task": row("a", json!("succeeded")) }));
    assert_eq!(store.for_conversation("c").len(), 2);
    assert_eq!(store.for_conversation("c").iter().find(|t| t.shell_task_id == "a").unwrap().outcome, Some(ShellTaskOutcome::Succeeded));
    assert_eq!(store.apply_event(&json!({ "type": "shellTaskEvicted", "conversationId": "c", "shellTaskId": "a" })), Some(("c".to_owned(), "a".to_owned())));
    assert_eq!(store.for_conversation("c").len(), 1);
    // A list asked for before the eviction arrives after it, and must not bring the row back.
    let listed: Vec<ShellTaskSnapshot> = serde_json::from_value(json!([row("a", json!("succeeded")), row("b", Value::Null)])).unwrap();
    store.reconcile("c", listed);
    assert_eq!(store.for_conversation("c").iter().map(|t| t.shell_task_id.as_str()).collect::<Vec<_>>(), ["b"]);
    // A host that restarted mints ids from one again, so a start clears the tombstone.
    store.apply_event(&json!({ "type": "shellTaskStarted", "task": row("a", Value::Null) }));
    assert_eq!(store.for_conversation("c").len(), 2);
    // Another conversation's rows are left alone.
    store.reconcile("other", vec![]);
    assert_eq!(store.for_conversation("c").len(), 2);
}
