//! The surface through a real host: a real `.mewrk` folder shows up, toggling it changes `ConversationSettings`, and that survives a save and a restart.

mod support;

use mewrk_client::prelude::*;
use mewrk_settings_conv::catalog::catalog_row_key;
use mewrk_settings_conv::logic::capability_workspace_key;
use mewrk_settings_conv::{conversation_settings_pane, CatalogKind, Page};
use support::*;

fn settings_of(app: &mut App) -> model::ConversationSettings {
    app.active_conversation_view().expect("conversation").settings
}

fn view(app: &mut App) -> E {
    conversation_settings_pane(app).unwrap_or_else(|| div())
}

fn discovered(app: &App) -> bool {
    app.doc.capabilities.skills.iter().any(|s| s.name == "demo-skill")
}

/// The first run, in a process of its own: only a process's end lets go of the data folder, so that is what a restart is.
#[test]
#[ignore = "run through a_real_mewrk_folder_shows_up_toggles_and_survives_a_restart"]
fn phase_toggle_everything() {
    let (mut rig, mut app) = Rig::open(Rig::seeded_from_env());
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());

    // Opening the pane is one of the moments discovery runs.
    rig.until(&mut app, &view, "the project's skill", |app| app.doc.capabilities.skills.iter().any(|s| s.name == "demo-skill"));
    let skill = app.doc.capabilities.skills.iter().find(|s| s.name == "demo-skill").unwrap().clone();
    assert_eq!(skill.workspace_key.as_deref(), Some(key.as_str()));
    let mcp = app.doc.capabilities.mcps.iter().find(|s| s.name.contains("demo-mcp")).expect("mcp.json").clone();
    let hook = app.doc.capabilities.hooks.iter().find(|s| s.workspace_key.as_deref() == Some(key.as_str())).expect("hooks.json").clone();
    let role = app.doc.capabilities.agents.iter().find(|s| s.descriptor.name == "reviewer").expect("agents/reviewer.json").descriptor.clone();

    let conv = "conv1";
    rig.click(&mut app, &view, ("nav", conv, Page::Skills));
    assert!(rig.shows("demo-skill"), "{:?}", rig.said());
    assert!(!settings_of(&mut app).skill_ids.contains(&skill.id));
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{conv}:skills:{key}"), &skill.id));
    assert!(settings_of(&mut app).skill_ids.contains(&skill.id));

    rig.click(&mut app, &view, ("nav", conv, Page::Mcp));
    assert!(rig.shows(&mcp.name));
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{conv}:mcp:{key}"), &mcp.id));
    assert!(settings_of(&mut app).mcp_ids.contains(&mcp.id));

    rig.click(&mut app, &view, ("nav", conv, Page::Hooks));
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{conv}:hooks:{key}"), &hook.id));
    assert!(settings_of(&mut app).hook_ids.contains(&hook.id));

    rig.click(&mut app, &view, ("nav", conv, Page::Roles));
    assert!(rig.shows("reviewer"));
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{conv}:roles:{key}"), &role.id));
    assert!(settings_of(&mut app).agent_ids.contains(&role.id));

    // Every one of those has to reach the host and its store before this process ends.
    rig.settle(&mut app, &view);
    std::fs::write(rig.dir.join("expected.json"), serde_json::to_vec(&json!({ "skill": skill.id, "mcp": mcp.id, "hook": hook.id, "role": role.id })).unwrap()).unwrap();
    app.host.finalize();
}

#[test]
#[ignore = "run through a_real_mewrk_folder_shows_up_toggles_and_survives_a_restart"]
fn phase_read_it_back() {
    let (rig, mut app) = Rig::open(Rig::seeded_from_env());
    init_live(&mut app);
    let expected: Value = serde_json::from_slice(&std::fs::read(rig.dir.join("expected.json")).unwrap()).unwrap();
    let after = app.find_conversation("ws1", "conv1").expect("still there").clone();
    assert_eq!(after.settings.skill_ids, vec![expected["skill"].as_str().unwrap().to_owned()]);
    assert_eq!(after.settings.mcp_ids, vec![expected["mcp"].as_str().unwrap().to_owned()]);
    assert_eq!(after.settings.hook_ids, vec![expected["hook"].as_str().unwrap().to_owned()]);
    assert!(after.settings.agent_ids.contains(&expected["role"].as_str().unwrap().to_owned()));
    assert_eq!(after.title, "demo");
    app.host.finalize();
}

#[test]
fn a_real_mewrk_folder_shows_up_toggles_and_survives_a_restart() {
    let seeded = seed("live");
    Rig::in_new_process("phase_toggle_everything", &seeded);
    Rig::in_new_process("phase_read_it_back", &seeded);
    seeded.remove();
}

#[test]
fn follows_the_disk_while_open_and_deletes_a_skill_through_the_host() {
    let (mut rig, mut app) = Rig::new("live-disk");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    let list = "catalog:conv1:skills";
    rig.click(&mut app, &view, ("nav", "conv1", Page::Skills));

    // The watch only reacts to a change from what it read first, two seconds in.
    let opened = std::time::Instant::now();
    rig.until(&mut app, &view, "the first look at the disk", move |_| opened.elapsed().as_millis() > 2500);

    // A new skill folder shows up by itself shortly after.
    let extra = rig.project.join(".mewrk/skills/extra-skill");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(extra.join("SKILL.md"), "---\nname: extra-skill\ndescription: Added while open\n---\nBody\n").unwrap();
    rig.until(&mut app, &view, "the new skill to appear unasked", |app| app.doc.capabilities.skills.iter().any(|s| s.name == "extra-skill"));
    assert!(rig.shows("extra-skill"));

    // Deleted: the folder goes, and the row leaves because the rescan says so, not because this crate removed it.
    let doomed = app.doc.capabilities.skills.iter().find(|s| s.name == "extra-skill").unwrap().clone();
    let delete = fluent::el::hash_key(("delete", list, doomed.id.as_str()));
    let _ = &key;
    rig.click(&mut app, &view, delete);
    rig.click(&mut app, &view, delete);
    rig.until(&mut app, &view, "the row to leave", |app| !app.doc.capabilities.skills.iter().any(|s| s.name == "extra-skill"));
    assert!(!extra.exists());
    assert!(!rig.shows("extra-skill"));
}

#[test]
fn deletes_a_hook_and_a_server_out_of_their_files() {
    let (mut rig, mut app) = Rig::new("live-delete");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    let hook = app.doc.capabilities.hooks.iter().find(|h| h.workspace_key.as_deref() == Some(key.as_str())).unwrap().clone();
    let mcp = app.doc.capabilities.mcps.iter().find(|m| m.name.contains("demo-mcp")).unwrap().clone();
    rig.click(&mut app, &view, ("nav", "conv1", Page::Hooks));
    let d = fluent::el::hash_key(("delete", "catalog:conv1:hooks", hook.id.as_str()));
    rig.click(&mut app, &view, d);
    rig.click(&mut app, &view, d);
    rig.until(&mut app, &view, "the hook to leave", |app| !app.doc.capabilities.hooks.iter().any(|h| h.id == hook.id));
    rig.click(&mut app, &view, ("nav", "conv1", Page::Mcp));
    let d = fluent::el::hash_key(("delete", "catalog:conv1:mcp", mcp.id.as_str()));
    rig.click(&mut app, &view, d);
    rig.click(&mut app, &view, d);
    rig.until(&mut app, &view, "the server to leave", |app| !app.doc.capabilities.mcps.iter().any(|m| m.id == mcp.id));
    assert!(!std::fs::read_to_string(rig.project.join(".mewrk/mcp.json")).unwrap().contains("demo-mcp"));
}

#[test]
fn says_when_a_delete_fails_on_the_page_it_happened_on() {
    let (mut rig, mut app) = Rig::new("live-delete-fail");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let skill = app.doc.capabilities.skills.iter().find(|s| s.name == "demo-skill").unwrap().clone();
    let mut gone = skill.clone();
    gone.id = "skill_that_is_not_there".into();
    rig.click(&mut app, &view, ("nav", "conv1", Page::Skills));
    mewrk_settings_conv::ops::delete_capability(&mut app, CatalogKind::Skills, gone, Box::new(|_, removed| assert!(!removed)));
    rig.until(&mut app, &view, "the error", |app| app.capability_error().is_some());
    let error = app.capability_error().unwrap();
    assert!(rig.shows(&error), "{:?}", rig.said());
}

#[test]
fn tests_a_real_server_that_is_not_one_and_reports_the_failure() {
    let (mut rig, mut app) = Rig::new("live-probe");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let mcp = app.doc.capabilities.mcps.iter().find(|m| m.name.contains("demo-mcp")).unwrap().clone();
    rig.click(&mut app, &view, ("nav", "conv1", Page::Mcp));
    rig.click(&mut app, &view, ("probe", "catalog:conv1:mcp", mcp.id.as_str()));
    assert!(rig.shows("测试中…"), "{:?}", rig.said());
    let start = std::time::Instant::now();
    loop {
        rig.frame(&mut app, &view);
        if rig.shows("连接失败") {
            break;
        }
        assert!(start.elapsed().as_secs() < 60, "{:?}", rig.said());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn writes_a_new_role_as_a_real_file_selects_it_and_edits_it_in_place() {
    let (mut rig, mut app) = Rig::new("live-role");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    rig.click(&mut app, &view, ("nav", "conv1", Page::Roles));
    rig.click(&mut app, &view, ("agent-role-add", "catalog:conv1:roles"));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "Tester");
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    let found = |app: &App| app.doc.capabilities.agents.iter().find(|a| a.descriptor.name == "Tester").map(|a| a.descriptor.id.clone());
    rig.until(&mut app, &view, "the role in the catalog", |app| found(app).is_some());
    let id = found(&app).unwrap();
    rig.until(&mut app, &view, "the role selected", |app| settings_of(app).agent_ids.contains(&id));
    let files: Vec<String> = std::fs::read_dir(rig.project.join(".mewrk/agents")).unwrap().map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap()).collect();
    assert!(files.iter().any(|f| f.contains("Tester")), "{files:?}");

    rig.click(&mut app, &view, ("role-configure", "catalog:conv1:roles", id.as_str()));
    rig.type_into(&mut app, &view, ("role-description", id.as_str()), "Runs the tests");
    rig.click(&mut app, &view, ("role-save", id.as_str()));
    let project = rig.project.clone();
    rig.until(&mut app, &view, "the description on disk", move |_| std::fs::read_dir(project.join(".mewrk/agents")).unwrap().any(|e| std::fs::read_to_string(e.unwrap().path()).unwrap().contains("Runs the tests")));
    // The file did not move: the id still names it.
    assert_eq!(found(&app).as_deref(), Some(id.as_str()));
    assert!(settings_of(&mut app).agent_ids.contains(&id));
}

fn user_context(id: &str, content: &str) -> Value {
    json!({ "kind": "user", "id": id, "createdAt": "2026-01-01T00:00:00.000Z", "content": content })
}

#[test]
fn saves_a_preset_with_its_timeline_captured_by_the_host_and_lays_it_over_another_conversation() {
    let (mut rig, mut app) = Rig::new("live-template");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    set_timeline(&mut app, "conv1", json!([user_context("c1", "first"), user_context("c2", "second")]));
    rig.until(&mut app, &view, "discovery", |app| discovered(app));

    rig.click(&mut app, &view, ("settings-save", "conv1"));
    rig.click(&mut app, &view, ("save-as-capture", "conv1"));
    rig.type_into(&mut app, &view, "save-as-name:conv1", "Captured");
    rig.click(&mut app, &view, ("save-as-confirm", "conv1"));
    let preset_named = |app: &App| app.doc.presets.conversation_presets.iter().find(|p| p.name == "Captured").cloned();
    rig.until(&mut app, &view, "the preset", |app| preset_named(app).is_some());
    let preset = preset_named(&app).unwrap();
    assert!(!preset.template_id.is_empty());
    // Its body is what the conversation held, read back through the host.
    let body = app.host.invoke_blocking("preview_conversation_template", json!({ "templateId": preset.template_id })).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let second = mewrk_ui_chat::frame::unit_key(0, body[1]["id"].as_str().unwrap());
    assert!(rig.exists(second), "the window opened at the page: {:?}", rig.said());
    rig.click(&mut app, &view, ("settings-window-close", fluent::el::hash_key(("preset-window", preset.id.as_str()))));

    // Another conversation, with work of its own: the preset's queue asks before it replaces it.
    create_conversation(&mut app, "conv2");
    set_timeline(&mut app, "conv2", json!([user_context("o1", "my own work")]));
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, ("nav", "conv2", Page::Presets));
    rig.click(&mut app, &view, ("preset-apply", "catalog:conv2:presets", preset.id.as_str()));
    assert!(rig.shows("套用这份预设？"), "{:?}", rig.said());
    assert_eq!(app.active_conversation_view().unwrap().contexts.len(), 1);
    rig.click(&mut app, &view, "template-switch-replace");
    rig.until(&mut app, &view, "the queue laid in", |app| app.active_conversation_view().unwrap().contexts.len() == 2);
    let after = app.active_conversation_view().unwrap();
    assert_eq!(after.preset_id, preset.id, "applying re-establishes the trace");
    assert_eq!(after.template_id, preset.template_id);
    rig.frame(&mut app, &view);
    assert!(rig.shows(&format!("applied:{}", preset.name)));
}

#[test]
fn an_edit_that_moves_a_preset_owned_field_forgets_the_preset_it_came_from() {
    let (mut rig, mut app) = Rig::new("live-trace");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let carried = app.active_conversation_view().unwrap().preset_id;
    assert!(!carried.is_empty());
    rig.click(&mut app, &view, ("nav", "conv1", Page::Advanced));
    let before = settings_of(&mut app).global_memory_enabled;
    rig.click(&mut app, &view, ("switch", "conv1:global-memory"));
    assert_eq!(settings_of(&mut app).global_memory_enabled, !before);
    assert_eq!(app.active_conversation_view().unwrap().preset_id, "", "a preset is a template, not a tether");
    rig.settle(&mut app, &view);
    app.host.finalize();
}

#[test]
fn writes_a_preset_template_and_deletes_it_with_the_preset() {
    let (mut rig, mut app) = Rig::new("live-preset");
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    rig.click(&mut app, &view, ("settings-save", "conv1"));
    rig.type_into(&mut app, &view, "save-as-name:conv1", "Mine");
    rig.click(&mut app, &view, ("save-as-confirm", "conv1"));
    let preset_named = |app: &App| app.doc.presets.conversation_presets.iter().find(|p| p.name == "Mine").cloned();
    rig.until(&mut app, &view, "the preset", |app| preset_named(app).is_some());
    // It opened at its empty template page, to write one.
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "开局");
    rig.until(&mut app, &view, "the minted id bound at once", |app| preset_named(app).is_some_and(|p| !p.template_id.is_empty()));
    let preset = preset_named(&app).unwrap();
    let body = app.host.invoke_blocking("preview_conversation_template", json!({ "templateId": preset.template_id })).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1);
    let listed: Vec<Value> = serde_json::from_value(app.host.invoke_blocking("list_conversation_templates", json!({})).unwrap()).unwrap();
    assert!(listed.iter().any(|t| t["id"] == json!(preset.template_id) && t["messageCount"] == json!(1)), "{listed:?}");

    rig.click(&mut app, &view, ("settings-window-close", fluent::el::hash_key(("preset-window", preset.id.as_str()))));
    rig.click(&mut app, &view, ("nav", "conv1", Page::Presets));
    let delete = fluent::el::hash_key(("preset-delete", "catalog:conv1:presets", preset.id.as_str()));
    rig.click(&mut app, &view, delete);
    rig.click(&mut app, &view, delete);
    rig.settle(&mut app, &view);
    assert!(preset_named(&app).is_none());
    let body = app.host.invoke_blocking("preview_conversation_template", json!({ "templateId": preset.template_id })).unwrap_or(json!([]));
    assert_eq!(body.as_array().unwrap().len(), 0, "nothing else cites the body, so it goes too");
}

fn draft_scope() -> String {
    mewrk_client::drafts::draft_conversation_id("ws1")
}

#[test]
#[ignore = "run through a_draft_keeps_its_settings_across_a_restart"]
fn phase_draft_toggle() {
    let (mut rig, mut app) = Rig::open(Rig::seeded_from_env());
    init_live(&mut app);
    app.open_draft_conversation(Some("ws1"));
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    let skill = app.doc.capabilities.skills.iter().find(|s| s.name == "demo-skill").unwrap().clone();
    let scope = draft_scope();
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    rig.click(&mut app, &view, ("nav", scope.as_str(), Page::Skills));
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{scope}:skills:{key}"), &skill.id));
    assert!(settings_of(&mut app).skill_ids.contains(&skill.id));
    // A draft has no timeline to capture, and its settings are its own.
    rig.click(&mut app, &view, ("settings-save", scope.as_str()));
    assert!(rig.shows("这段对话还没有消息"));
    rig.settle(&mut app, &view);
    std::fs::write(rig.dir.join("expected.json"), serde_json::to_vec(&json!({ "skill": skill.id })).unwrap()).unwrap();
    app.host.finalize();
}

#[test]
#[ignore = "run through a_draft_keeps_its_settings_across_a_restart"]
fn phase_draft_read_back() {
    let (rig, mut app) = Rig::open(Rig::seeded_from_env());
    init_live(&mut app);
    let expected: Value = serde_json::from_slice(&std::fs::read(rig.dir.join("expected.json")).unwrap()).unwrap();
    app.open_draft_conversation(Some("ws1"));
    assert!(settings_of(&mut app).skill_ids.contains(&expected["skill"].as_str().unwrap().to_owned()), "{:?}", settings_of(&mut app).skill_ids);
    app.host.finalize();
}

#[test]
fn a_draft_keeps_its_settings_across_a_restart() {
    let seeded = seed("live-draft");
    Rig::in_new_process("phase_draft_toggle", &seeded);
    Rig::in_new_process("phase_draft_read_back", &seeded);
    seeded.remove();
}

#[test]
fn a_draft_given_a_preset_with_a_template_becomes_a_conversation() {
    let (mut rig, mut app) = Rig::new("live-redeem");
    init_live(&mut app);
    app.open_draft_conversation(Some("ws1"));
    rig.until(&mut app, &view, "discovery", |app| discovered(app));
    app.host.invoke_blocking("update_conversation_template", json!({ "templateId": "template_x", "contexts": [user_context("t1", "from the template")] })).expect("written");
    let mut settings = mewrk_client::presets::empty_conversation_preset_settings();
    settings.skill_tool_enabled = true;
    let preset = model::ConversationPreset { id: "preset_x".into(), name: "X".into(), description: String::new(), template_id: "template_x".into(), settings };
    app.update_document(|doc| doc.presets.conversation_presets.push(preset));
    let scope = draft_scope();
    rig.click(&mut app, &view, ("nav", scope.as_str(), Page::Presets));
    rig.click(&mut app, &view, ("preset-apply", format!("catalog:{scope}:presets"), "preset_x"));
    let conv = |app: &App| app.doc.workspaces.iter().find(|w| w.id == "ws1").and_then(|w| w.conversations.first()).cloned();
    rig.until(&mut app, &view, "the redeemed conversation", |app| conv(app).is_some_and(|c| c.contexts.len() == 1));
    let c = conv(&app).unwrap();
    assert_eq!(c.preset_id, "preset_x");
    assert!(c.settings.skill_tool_enabled);
    assert_eq!(c.template_id, "template_x");
}
