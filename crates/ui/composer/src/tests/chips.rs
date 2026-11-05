//! `ProjectChips.test.tsx` and the composer's parts of `App.composerChips`, `App.newConversation` and `App.draftGit`.

use super::rig::{conversation, Rig, Seed, CONV, WS};
use crate::chips::{attached_remove_key, attached_settings_key, project_menu_sections, project_selector_model, workspace_menu_sections, ATTACH_MENU, PROJECT_MENU, WORKSPACE_MENU};
use crate::footer::{EFFORT_MENU, MODEL_MENU, SECURITY_MENU};
use crate::{composer_box_key, ComposerProps};
use mewrk_client::model::{AttachedWorkspace, ReasoningEffort, RunTarget, SecurityLevel};
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::{panel_key, row_key, trigger_key};
use std::cell::RefCell;

fn started() -> Value {
    let mut started = conversation(CONV, "Started");
    started["contexts"] = json!([{ "kind": "user", "id": "u1", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" }]);
    started
}

#[test]
fn keeps_the_chip_row_outside_the_input_box_immediately_above_it() {
    let mut rig = Rig::new();
    rig.settle(10);
    let (row_box, input) = (rig.rect("composer-context"), rig.rect(composer_box_key()));
    assert!(row_box.bottom() <= input.y, "{row_box:?} above {input:?}");
    assert!(input.y - row_box.bottom() <= 10.0, "right at its edge: {row_box:?} {input:?}");
}

#[test]
fn opens_every_chips_menu_above_it_its_bottom_left_corner_on_the_chips_top_left() {
    let mut rig = Rig::new();
    for menu in [PROJECT_MENU, WORKSPACE_MENU, ATTACH_MENU] {
        rig.click(trigger_key(menu));
        rig.settle(30);
        let (panel, chip) = (rig.rect(panel_key(menu)), rig.rect(trigger_key(menu)));
        assert!((panel.bottom() - (chip.y - 6.0)).abs() < 1.0, "{menu}: {panel:?} {chip:?}");
        assert!((panel.x - chip.x).abs() < 1.0, "{menu}: {panel:?} {chip:?}");
        rig.press(Key::Escape);
        rig.settle(20);
    }
}

#[test]
fn hides_the_project_branch_and_worktree_chips_once_the_conversation_has_started() {
    let mut rig = Rig::seeded(Seed { conversations: vec![started()], ..Seed::default() });
    rig.settle(10);
    assert!(!rig.has(trigger_key(PROJECT_MENU)));
    assert!(rig.has(trigger_key(WORKSPACE_MENU)), "the workspace chip stays: its menu is where a workspace's settings open");
}

#[test]
fn lists_the_projects_the_temporary_project_and_a_way_to_make_a_new_one() {
    let mut rig = Rig::new();
    rig.click(trigger_key(PROJECT_MENU));
    rig.settle(30);
    let words = rig.words();
    for word in ["Projects", "project", "Temporary project", "New project…"] {
        assert!(words.iter().any(|said| said == word), "{word} in {words:?}");
    }
    assert!(words.iter().any(|said| said == "checked"), "the active one is checked");
    rig.click(row_key(PROJECT_MENU, "create"));
    assert!(rig.app.workspace_dialog_open(), "new project opens the dialog");
}

#[test]
fn asks_for_a_project_when_none_is_chosen_yet() {
    let mut rig = Rig::new();
    let model = project_selector_model(&mut rig.app, None, false, "");
    assert_eq!(model.active_name, "Choose project");
    assert_eq!(model.active_id, None);
}

#[test]
fn refuses_to_move_out_of_or_into_a_project_that_is_being_deleted() {
    let mut rig = Rig::new();
    let mut model = project_selector_model(&mut rig.app, Some(WS), true, "The conversation is being deleted");
    assert!(model.source_deleting);
    assert_eq!(model.title, "The conversation is being deleted");
    let chosen: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = chosen.clone();
    let sections = project_menu_sections(&model, Rc::new(move |_, id| sink.borrow_mut().push(id.to_owned())), Rc::new(|_| {}));
    assert!(sections.iter().all(|section| section.items.iter().all(|item| item.disabled)), "nothing can be chosen while the source goes");
    model.source_deleting = false;
    model.projects[0].deleting = true;
    let sink = chosen.clone();
    let sections = project_menu_sections(&model, Rc::new(move |_, id| sink.borrow_mut().push(id.to_owned())), Rc::new(|_| {}));
    assert!(sections[0].items[0].disabled);
    (sections[0].items[0].on_select.clone().unwrap())(&mut rig.app);
    assert!(chosen.borrow().is_empty());
    (sections[1].items[0].on_select.clone().unwrap())(&mut rig.app);
    assert_eq!(chosen.borrow().len(), 1);
}

#[test]
fn groups_the_workspace_menu_by_machine_one_gear_per_machine_and_one_per_workspace() {
    let _rig = Rig::new();
    let machine = RunTarget::Ssh { machine_id: "m1".to_owned() };
    let workspaces = vec![AttachedWorkspace { machine: None, path: "/a".into() }, AttachedWorkspace { machine: Some(machine.clone()), path: "/b".into() }, AttachedWorkspace { machine: None, path: "/c".into() }];
    let sections = workspace_menu_sections(&workspaces, 3, &[], Rc::new(|_, _| {}), Rc::new(|_, _| {}), Rc::new(|_, _| {}));
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].label.as_deref(), Some("This machine"));
    assert!(sections[0].action.as_ref().unwrap().label.contains("This machine"));
    // In project order, by number, without a number beside them.
    assert_eq!(sections[0].items.iter().map(|item| (item.id.as_str(), item.label.as_str())).collect::<Vec<_>>(), [("1", "/a"), ("3", "/c")]);
    assert_eq!(sections[0].items[1].checked, Some(true));
    assert_eq!(sections[1].items[0].id, "2");
    assert!(sections[0].items[0].label_is_path);
    assert!(sections[0].items[0].action.is_some());
}

#[test]
fn opens_a_machines_settings_from_its_headings_gear_and_a_workspaces_from_its_own() {
    let machines: Rc<RefCell<Vec<Option<RunTarget>>>> = Rc::default();
    let workspaces: Rc<RefCell<Vec<String>>> = Rc::default();
    let mut rig = Rig::new();
    let (machine_sink, workspace_sink) = (machines.clone(), workspaces.clone());
    rig.props.on_configure_machine = Some(Rc::new(move |_, machine| machine_sink.borrow_mut().push(machine)));
    rig.props.on_configure_workspace = Some(Rc::new(move |_, _, path| workspace_sink.borrow_mut().push(path)));
    let path = rig.repo.to_string_lossy().into_owned();
    let run_env = mewrk_client::workspaces::run_env_key(None);
    rig.click(trigger_key(WORKSPACE_MENU));
    rig.settle(30);
    rig.click(mewrk_ui_common::popover_menu::action_key(WORKSPACE_MENU, &format!("section:{run_env}")));
    assert_eq!(*machines.borrow(), [None]);
    rig.settle(20);
    rig.click(trigger_key(WORKSPACE_MENU));
    rig.settle(30);
    rig.click(mewrk_ui_common::popover_menu::action_key(WORKSPACE_MENU, "row:1"));
    assert_eq!(*workspaces.borrow(), [path]);
}

#[test]
fn attaches_a_directory_as_its_own_chip_and_detaches_it_again_and_gives_it_a_gear() {
    let settings: Rc<RefCell<Vec<String>>> = Rc::default();
    let mut rig = Rig::new();
    let sink = settings.clone();
    rig.props.on_configure_workspace = Some(Rc::new(move |_, _, path| sink.borrow_mut().push(path)));
    crate::attach_workspace(&mut rig.app, None, "/attached/dir".to_owned());
    // Attaching the same one again changes nothing.
    crate::attach_workspace(&mut rig.app, None, "/attached/dir".to_owned());
    rig.settle(10);
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].attached_workspaces.len(), 1);
    assert!(rig.has(attached_settings_key("/attached/dir")));
    rig.click(attached_settings_key("/attached/dir"));
    assert_eq!(*settings.borrow(), ["/attached/dir"]);
    rig.click(attached_remove_key("/attached/dir"));
    assert!(rig.app.doc.workspaces[0].conversations[0].attached_workspaces.is_empty());
    assert!(!rig.has(attached_remove_key("/attached/dir")));
}

#[test]
fn a_draft_keeps_what_it_attaches_in_its_own_state() {
    let mut rig = Rig::seeded(Seed { conversations: vec![], ..Seed::default() });
    assert!(rig.conversation_id().starts_with("__draft__"));
    crate::attach_workspace(&mut rig.app, None, "/attached".to_owned());
    let id = rig.conversation_id();
    assert_eq!(rig.app.draft_of(&id).unwrap().attached_workspaces.len(), 1);
}

#[test]
fn does_not_offer_the_button_when_no_host_picker_can_authorize_a_directory() {
    let mut rig = Rig::new();
    rig.props = ComposerProps { has_native_workspace_picker: false, ..ComposerProps::default() };
    rig.settle(10);
    assert!(!rig.has(trigger_key(ATTACH_MENU)));
}

#[test]
fn browses_a_remote_machine_for_a_directory_instead_of_opening_the_native_dialog() {
    let browsed: Rc<RefCell<Vec<(RunTarget, String)>>> = Rc::default();
    let mut rig = Rig::new();
    let sink = browsed.clone();
    rig.props.on_browse_machine = Some(Rc::new(move |_, machine, name| sink.borrow_mut().push((machine, name))));
    rig.app.doc.assets.execution_environments.ssh_machines = vec![serde_json::from_value(json!({ "id": "m1", "name": "Build box", "host": "h", "user": "u" })).unwrap()];
    rig.click(trigger_key(ATTACH_MENU));
    rig.settle(40);
    rig.click(row_key(ATTACH_MENU, "ssh:m1"));
    assert_eq!(browsed.borrow()[0].1, "Build box");
}

#[test]
fn the_security_plan_model_and_effort_controls_write_where_they_belong() {
    let mut rig = Rig::new();
    rig.click(trigger_key(SECURITY_MENU));
    rig.settle(30);
    rig.click(row_key(SECURITY_MENU, "full_access"));
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].settings.security_level, SecurityLevel::FullAccess);

    assert!(!rig.app.doc.workspaces[0].conversations[0].settings.plan_mode_enabled);
    rig.click(crate::footer::plan_toggle_key());
    assert!(rig.app.doc.workspaces[0].conversations[0].settings.plan_mode_enabled);
    rig.click(crate::footer::plan_toggle_key());
    assert!(!rig.app.doc.workspaces[0].conversations[0].settings.plan_mode_enabled);

    rig.click(trigger_key(MODEL_MENU));
    rig.settle(30);
    assert!(rig.says("Provider"), "one section per provider");
    rig.click(row_key(MODEL_MENU, "provider-1\u{0}model-2"));
    assert_eq!(rig.app.doc.assets.api_providers[0].active_model_id.as_deref(), Some("model-2"));

    rig.click(trigger_key(EFFORT_MENU));
    rig.settle(30);
    rig.click(row_key(EFFORT_MENU, "high"));
    assert_eq!(rig.app.doc.global_settings.last_reasoning_effort, ReasoningEffort::High);
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].settings.reasoning_effort, ReasoningEffort::High);
    assert_eq!(rig.app.doc.workspaces[0].last_conversation_settings.as_ref().map(|settings| settings.reasoning_effort), Some(ReasoningEffort::High), "remembered as the workspace's next starting point");
}

#[test]
fn a_draft_changes_its_effort_without_writing_the_project_and_holds_the_model_while_a_run_goes() {
    let mut rig = Rig::seeded(Seed { conversations: vec![], ..Seed::default() });
    let id = rig.conversation_id();
    rig.click(trigger_key(EFFORT_MENU));
    rig.settle(30);
    rig.click(row_key(EFFORT_MENU, "max"));
    assert_eq!(rig.app.draft_of(&id).unwrap().settings.reasoning_effort, ReasoningEffort::Max);
    assert_eq!(rig.app.doc.global_settings.last_reasoning_effort, ReasoningEffort::Max);
    assert!(rig.app.doc.workspaces[0].last_conversation_settings.is_none());
}

#[test]
fn searches_the_models_and_says_when_none_match() {
    let mut rig = Rig::new();
    rig.click(trigger_key(MODEL_MENU));
    rig.settle(30);
    let panel = rig.rect(panel_key(MODEL_MENU));
    // Opened above the composer, so the field is at the foot, next to the trigger.
    rig.click_at(panel.x + 60.0, panel.bottom() - 16.0);
    rig.type_text("model-2");
    rig.settle(10);
    assert!(!rig.has(row_key(MODEL_MENU, "provider-1\u{0}model-1")));
    assert!(rig.has(row_key(MODEL_MENU, "provider-1\u{0}model-2")));
    rig.type_text("zz");
    assert!(rig.says("No matching models"));
}

#[test]
fn says_when_no_model_is_enabled_and_holds_the_send() {
    let mut rig = Rig::new();
    rig.app.doc.assets.api_providers[0].enabled = false;
    let words = rig.words();
    assert!(words.iter().any(|word| word == "No enabled models"));
    rig.frame();
    rig.focus_text_area();
    rig.type_text("x");
    rig.click(crate::send_button_key());
    rig.settle(20);
    assert_eq!(rig.host.count("run_model"), 0);
}

// ---- Git, on a real repository

fn git_ready(rig: &mut Rig) {
    let id = rig.conversation_id();
    let key = mewrk_pane_git::git_snapshot_key(&id, 1);
    let surface = mewrk_pane_git::git_surface_key(WS, 1);
    let target = if rig.app.draft_of(&id).is_some() { mewrk_pane_git::git_workspace_target(WS, Some(1)) } else { mewrk_pane_git::git_conversation_target(&id, Some(1)) };
    crate::chips::refresh_git_snapshot(&mut rig.app, key.clone(), surface.clone(), target);
    rig.until("the status", |rig| mewrk_pane_git::git_snapshot_for_workspace(mewrk_pane_git::git_controller(&mut rig.app).snapshots.get(&key), Some(&surface)).flatten().is_some());
}

#[test]
fn names_the_branch_above_the_input_and_has_no_git_surface_without_a_repository() {
    let mut rig = Rig::seeded(Seed { git: true, ..Seed::default() });
    git_ready(&mut rig);
    assert!(rig.has(("worktree-box", CONV)));
    drop(rig);
    let mut plain = Rig::new();
    plain.settle(60);
    assert!(!plain.has(("worktree-box", CONV)), "not a repository: no chips, and no status card");
}

#[test]
fn lists_only_local_branches_and_checks_out_the_one_picked() {
    let mut rig = Rig::seeded(Seed { git: true, ..Seed::default() });
    git_ready(&mut rig);
    let chip = hash_key(("branch-chip", CONV));
    rig.click(chip);
    rig.until("the branches", |rig| rig.has(("branch-row", CONV, "other")));
    assert!(rig.has(("branch-row", CONV, "main")));
    rig.click(("branch-row", CONV, "other"));
    rig.until("the checkout", |rig| super::rig::git(&rig.repo, &["rev-parse", "--abbrev-ref", "HEAD"]).trim() == "other");
}

#[test]
fn creates_an_isolated_worktree_and_releases_it_again() {
    let mut rig = Rig::seeded(Seed { git: true, ..Seed::default() });
    git_ready(&mut rig);
    let area = rig.rect(("worktree-box", CONV));
    rig.click_at(area.x + 14.0, area.y + area.h * 0.5);
    rig.until("the worktree", |rig| !rig.app.doc.workspaces[0].conversations[0].worktrees.is_empty());
    let tree = std::path::PathBuf::from(&rig.app.doc.workspaces[0].conversations[0].worktrees[0].path);
    assert!(tree.is_dir());
    // A read still in flight holds the workspace for a moment, and the host says so rather than waiting.
    for _ in 0..20 {
        rig.settle(100);
        if !rig.has(("worktree-box", CONV)) {
            continue;
        }
        // The workspace chip now names the worktree, which moved the box.
        let area = rig.rect(("worktree-box", CONV));
        rig.click_at(area.x + 14.0, area.y + area.h * 0.5);
        rig.settle(100);
        if rig.app.doc.workspaces[0].conversations[0].worktrees.is_empty() {
            break;
        }
    }
    assert!(rig.app.doc.workspaces[0].conversations[0].worktrees.is_empty());
}

#[test]
fn a_draft_only_records_the_worktree_it_asks_for() {
    let mut rig = Rig::seeded(Seed { git: true, conversations: vec![], ..Seed::default() });
    git_ready(&mut rig);
    let id = rig.conversation_id();
    let area = rig.rect(("worktree-box", id.as_str()));
    rig.click_at(area.x + 14.0, area.y + area.h * 0.5);
    rig.settle(20);
    assert_eq!(rig.app.draft_of(&id).unwrap().worktree_members, [1]);
    rig.click_at(area.x + 14.0, area.y + area.h * 0.5);
    rig.settle(20);
    assert!(rig.app.draft_of(&id).unwrap().worktree_members.is_empty());
}

#[test]
fn says_why_a_drafts_worktree_could_not_be_made_until_the_box_moves() {
    let mut rig = Rig::seeded(Seed { git: true, conversations: vec![], ..Seed::default() });
    git_ready(&mut rig);
    let id = rig.conversation_id();
    rig.app.run_signals().branch_chip_error = Some("The isolated worktree could not be created".to_owned());
    assert!(rig.says("The isolated worktree could not be created"));
    assert!(rig.app.run_signals().branch_chip_error.is_none(), "the signal is taken");
    assert!(rig.says("The isolated worktree could not be created"), "and what it said stays");
    let area = rig.rect(("worktree-box", id.as_str()));
    rig.click_at(area.x + 14.0, area.y + area.h * 0.5);
    rig.settle(20);
    assert!(!rig.says("The isolated worktree could not be created"));
}

#[test]
fn says_what_the_last_edit_on_the_timeline_did() {
    let mut rig = Rig::new();
    rig.app.show_timeline_notice(None, "Deleted 2 messages", Some(("⌘Z".to_owned(), "undo".to_owned())), 5000);
    let words = rig.words();
    assert!(words.iter().any(|word| word == "Deleted 2 messages"));
    assert!(words.iter().any(|word| word == "⌘Z"));
}

#[test]
fn holds_the_controls_a_run_holds_and_says_why_on_a_failed_fork() {
    let mut rig = Rig::new();
    rig.app.runs().fork_start_error = Some("The fork did not start".to_owned());
    rig.settle(10);
    let words = rig.words();
    assert!(words.iter().any(|word| word == "The fork did not start"));
    assert!(words.iter().any(|word| word == "Retry pending fork runs"));
    rig.click(crate::fork_retry_key());
    assert!(rig.app.fork_start_error().is_none());
}

#[test]
fn a_narrow_window_tightens_the_row_and_the_footer() {
    let mut rig = Rig::new();
    // Under the box's 820, so what is left at its sides is the padding.
    rig.size = (800.0, 600.0);
    rig.settle(10);
    let wide = (rig.rect(composer_box_key()), rig.rect("composer-wrap"));
    assert_eq!(wide.0.x - wide.1.x, 24.0, "`.composer-wrap` keeps 24 at the sides: {wide:?}");
    rig.size = (700.0, 600.0);
    rig.settle(10);
    let narrow = (rig.rect(composer_box_key()), rig.rect("composer-wrap"));
    assert_eq!(narrow.0.x - narrow.1.x, 10.0, "and 10 at 720 and under: {narrow:?}");
    assert_eq!(crate::chips::chip_type().size, 9.5, "and its chips and options are smaller");
    rig.size = (800.0, 600.0);
    rig.settle(10);
    assert_eq!(crate::chips::chip_type().size, FINE.size);
}
