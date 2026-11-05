//! Pictures of the pane and the template editor for a person to look at: `cargo test -p mewrk-settings-conv --test shots -- --ignored`, with `MEWRK_SHOTS` naming a folder.

mod support;

use std::path::PathBuf;

use fluent::el::hash_key;
use mewrk_client::model::{ContextItem, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_settings_conv::capability_editor::{add_button_key, button_key, edit_button_key, field_key};
use mewrk_settings_conv::{conversation_settings_pane, CatalogKind, Page, TemplateEditor};
use mewrk_ui_chat::frame::unit_key;
use mewrk_ui_chat::inline_tool_editor::{arg_field_key, result_field_key};
use mewrk_ui_common::popover_menu::trigger_key;
use support::*;

#[test]
#[ignore = "draws pictures"]
fn pane_pages() {
    let out = PathBuf::from(std::env::var("MEWRK_SHOTS").expect("MEWRK_SHOTS"));
    std::fs::create_dir_all(&out).unwrap();
    let (mut rig, mut app) = Rig::new("shots");
    app.doc.tools = vec![tool("read", "读取文件", "filesystem", false), tool("write", "写入文件", "filesystem", true), tool("bash", "Bash", "shell", true), tool("preview_click", "点击", "web", false), tool("agent_spawn", "子代理", "orchestration", false)];
    select_model(&mut app, provider("p1", "Claude", "anthropic", &[("m1", &["tool_append"])]));
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    let view = |app: &mut App| -> E { conversation_settings_pane(app).unwrap_or_else(|| div()) };
    rig.until(&mut app, &view, "discovery", |app| app.doc.capabilities.skills.iter().any(|s| s.name == "demo-skill"));
    for (page, name) in [(Page::Tools, "tools"), (Page::Advanced, "advanced"), (Page::Skills, "skills"), (Page::Roles, "roles"), (Page::Presets, "presets")] {
        rig.click(&mut app, &view, ("nav", "conv1", page));
        rig.shot(&mut app, &view, &out.join(format!("{name}.png")));
    }
    // The advanced page's controls: a web leg's menu, the leg off, and the width a side pane leaves it.
    let menu = ("web-backend-menu", "conv1:search-provider");
    rig.click(&mut app, &view, ("nav", "conv1", Page::Advanced));
    rig.click(&mut app, &view, trigger_key(menu));
    rig.shot(&mut app, &view, &out.join("advanced-menu.png"));
    rig.click(&mut app, &view, trigger_key(menu));
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    rig.shot(&mut app, &view, &out.join("advanced-off.png"));
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    let narrow = |app: &mut App| -> E { row().size(1400.0, 1000.0).child(row().size(460.0, 1000.0).child(conversation_settings_pane(app).unwrap_or_else(|| div()))) };
    rig.shot(&mut app, &narrow, &out.join("advanced-narrow.png"));
    // The template editor where it lives: a preset's window, whose own Save stands in for the page's.
    let preset = app.doc.presets.conversation_presets.first().expect("the built-in preset").id.clone();
    rig.click(&mut app, &view, ("nav", "conv1", Page::Presets));
    rig.click(&mut app, &view, ("preset-open".to_owned(), "catalog:conv1:presets".to_owned(), preset.clone()));
    rig.click(&mut app, &view, ("nav", format!("preset:{preset}"), Page::Template));
    rig.shot(&mut app, &view, &out.join("preset-template-empty.png"));
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "Please review this change.");
    rig.shot(&mut app, &view, &out.join("preset-template.png"));
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("preset-window", preset.as_str()))));
    rig.click(&mut app, &view, ("settings-save", "conv1"));
    rig.shot(&mut app, &view, &out.join("save-as.png"));
    rig.click(&mut app, &view, ("save-as-cancel", "conv1"));
    rig.click(&mut app, &view, ("nav", "conv1", Page::Roles));
    rig.click(&mut app, &view, ("agent-role-add", "catalog:conv1:roles"));
    rig.shot(&mut app, &view, &out.join("role-window.png"));
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.shot(&mut app, &view, &out.join("role-window-project.png"));
    rig.click(&mut app, &view, ("nav", "role:new-role", mewrk_settings_conv::roles::RolePage::Advanced));
    rig.shot(&mut app, &view, &out.join("role-advanced.png"));
}

/// What a warm cache looks like: nothing but the controls' own colour.
#[test]
#[ignore = "draws pictures"]
fn warm_cache() {
    let out = PathBuf::from(std::env::var("MEWRK_SHOTS").expect("MEWRK_SHOTS"));
    std::fs::create_dir_all(&out).unwrap();
    let (mut rig, mut app) = Rig::new("shots-warm");
    app.doc.tools = vec![tool("read", "读取文件", "filesystem", false), tool("write", "写入文件", "filesystem", true), tool("bash", "Bash", "shell", true)];
    select_model(&mut app, provider("p1", "Claude", "anthropic", &[("m1", &["tool_append"])]));
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    app.update_active_conversation(|conversation| {
        conversation.settings.enabled_tools = vec!["read".to_owned()];
        conversation.settings.web_search_enabled = true;
        conversation.settings.web_search.exclude_domains = vec!["*://a.example/*".to_owned(), "*://b.example/*".to_owned()];
        conversation.settings.tool_lock = Some(model::ConversationToolLock { tools: vec!["read".to_owned()], web_search: true, global_memory: conversation.settings.global_memory_enabled, project_memory: conversation.settings.project_memory_enabled, file_write_guards: Some(conversation.settings.file_write_guards_enabled), host_message_container: Some(conversation.settings.host_message_container), search_backend: Some(conversation.settings.web_search.provider.clone()), ..warm_lock("p1", "m1") });
    });
    let view = |app: &mut App| -> E { conversation_settings_pane(app).unwrap_or_else(|| div()) };
    rig.click(&mut app, &view, ("nav", "conv1", Page::Advanced));
    rig.shot(&mut app, &view, &out.join("warm-advanced.png"));
    rig.click(&mut app, &view, ("nav", "conv1", Page::Tools));
    rig.shot(&mut app, &view, &out.join("warm-tools.png"));
}

fn shot_body() -> Vec<ContextItem> {
    let items = json!([
        { "kind": "system", "id": "s1", "createdAt": "", "content": "You review changes before they land. Say what you would change, and why." },
        { "kind": "user", "id": "u1", "createdAt": "", "content": "Please review this change.\n\nIt moves the template editor onto the ordinary conversation view." },
        { "kind": "reasoning", "id": "r1", "createdAt": "", "form": "plaintext", "content": "Start with what the surface withholds: a template has nothing to execute against." },
        { "kind": "tool", "id": "t1", "toolName": "ls", "input": { "path": "crates/ui/settings-conv/src" }, "result": { "success": true, "output": "lib.rs\npane.rs\ntemplate_editor.rs", "executedAt": "2026-09-15T00:00:01.000Z", "durationMs": 4 }, "attestation": "attested", "createdAt": "" },
        { "kind": "tool", "id": "t2", "toolName": "bash", "input": { "command": "cargo test -p mewrk-settings-conv" }, "result": { "success": true, "output": "test result: ok. 41 passed", "executedAt": "", "durationMs": 0 }, "createdAt": "" },
        { "kind": "assistant", "id": "a1", "createdAt": "", "content": "**Looks right.** A placed card starts with the result the host would mint, and a rewritten one drops its attestation." },
        { "kind": "tool", "id": "q1", "toolName": "ask_user", "input": { "questions": [{ "question": "Which should land first?", "header": "Order", "multiSelect": false, "options": [{ "label": "The view", "description": "Swap the list for the timeline" }, { "label": "The tests", "description": "Move the tests first" }] }] }, "result": { "success": true, "output": "", "executedAt": "", "durationMs": 0 }, "createdAt": "" },
        { "kind": "user", "id": "qa", "createdAt": "", "content": "User has answered your questions: \"Which should land first?\"=\"The view\"" },
    ]);
    serde_json::from_value(items).unwrap()
}

fn shot_tools() -> Rc<Vec<ToolDescriptor>> {
    let taking = |name: &str, label: &str, category: &str, parameter: &str, parameter_label: &str| -> ToolDescriptor { serde_json::from_value(json!({ "name": name, "label": label, "description": "", "category": category, "dangerous": false, "parameters": [{ "name": parameter, "label": parameter_label, "type": "string", "required": true }] })).unwrap() };
    Rc::new(vec![taking("ls", "列出目录", "filesystem", "path", "路径"), taking("bash", "运行命令", "shell", "command", "命令"), tool("ask_user", "提问", "orchestration", false)])
}

#[test]
#[ignore = "draws pictures"]
fn template_editor() {
    let out = PathBuf::from(std::env::var("MEWRK_SHOTS").expect("MEWRK_SHOTS"));
    std::fs::create_dir_all(&out).unwrap();
    let (mut rig, mut app) = Rig::new("shots-template");
    const ID: &str = "template_shot";
    let view = |app: &mut App| -> E {
        TemplateEditor { key: "shot".into(), template_id: ID.into(), contexts: Some(shot_body()), tools: shot_tools(), enabled_tools: vec!["ls".to_owned(), "bash".to_owned(), "ask_user".to_owned()], editable: true, image_input_supported: true, autosave: false, on_save: Some(Rc::new(|app, _, done| done(app, Ok(())))), on_enable_tools: None }.view(app)
    };
    rig.shot(&mut app, &view, &out.join("template-page.png"));
    rig.right_click(&mut app, &view, unit_key(0, "a1"));
    rig.click(&mut app, &view, timeline_menu_row(ID, &[], "tool"));
    rig.click(&mut app, &view, timeline_menu_row(ID, &["tool"], "Shell"));
    rig.shot(&mut app, &view, &out.join("template-menu.png"));
    rig.click(&mut app, &view, timeline_menu_row(ID, &["tool", "Shell"], "bash"));
    let placed = hash_key(("tl-tool-insert", "bash"));
    rig.type_into(&mut app, &view, arg_field_key(placed, "command"), "npm test");
    rig.type_into(&mut app, &view, result_field_key(placed), "全部通过");
    rig.shot(&mut app, &view, &out.join("template-place.png"));
    rig.click(&mut app, &view, ("tool-editor-cancel", placed));
    rig.click(&mut app, &view, ("tl-edit", 0u64, "t1"));
    rig.shot(&mut app, &view, &out.join("template-tool-edit.png"));
    rig.click(&mut app, &view, ("tool-editor-cancel", hash_key(("tl-tool-edit", 0u64, "t1"))));
    rig.click(&mut app, &view, ("tl-edit", 0u64, "u1"));
    rig.shot(&mut app, &view, &out.join("template-text-edit.png"));
    rig.click(&mut app, &view, ("text-editor-cancel", hash_key(("tl-text-edit", 0u64, "u1"))));
    rig.click(&mut app, &view, ("question-edit", "q1"));
    rig.shot(&mut app, &view, &out.join("template-question-edit.png"));
}

/// The pages with their new and edit buttons, and the windows those open, empty, filled in, read from a file and complaining.
#[test]
#[ignore = "draws pictures"]
fn capability_windows() {
    let out = PathBuf::from(std::env::var("MEWRK_SHOTS").expect("MEWRK_SHOTS"));
    std::fs::create_dir_all(&out).unwrap();
    let (mut rig, mut app) = Rig::new("shots-capabilities");
    std::fs::write(rig.project.join(".mewrk/mcp.json"), r#"{"mcpServers":{"demo-mcp":{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem"],"env":{"TOKEN":"${DEMO_TOKEN:-x}","LOG_LEVEL":"info"},"description":"The project's files"},"needs-token":{"type":"http","url":"https://example.com/mcp","headers":{"Authorization":"Bearer ${DEMO_TOKEN_NOT_SET}"}}}}"#).unwrap();
    init_live(&mut app);
    create_conversation(&mut app, "conv1");
    let view = |app: &mut App| -> E { conversation_settings_pane(app).unwrap_or_else(|| div()) };
    rig.until(&mut app, &view, "discovery", |app| app.doc.capabilities.mcps.len() == 2);
    let (skills, mcp, hooks) = ("catalog:conv1:skills", "catalog:conv1:mcp", "catalog:conv1:hooks");

    rig.click(&mut app, &view, ("nav", "conv1", Page::Skills));
    rig.shot(&mut app, &view, &out.join("skills-page.png"));
    rig.click(&mut app, &view, add_button_key(skills));
    rig.shot(&mut app, &view, &out.join("skill-new.png"));
    rig.click(&mut app, &view, button_key(CatalogKind::Skills, "new", "save"));
    rig.shot(&mut app, &view, &out.join("skill-new-complaints.png"));
    rig.click(&mut app, &view, button_key(CatalogKind::Skills, "new", "cancel"));

    rig.click(&mut app, &view, ("nav", "conv1", Page::Mcp));
    rig.shot(&mut app, &view, &out.join("mcp-page.png"));
    rig.click(&mut app, &view, add_button_key(mcp));
    rig.shot(&mut app, &view, &out.join("mcp-new.png"));
    rig.click(&mut app, &view, ("capability-transport", "new", "http"));
    rig.shot(&mut app, &view, &out.join("mcp-new-http.png"));
    rig.click(&mut app, &view, button_key(CatalogKind::Mcp, "new", "cancel"));
    rig.click(&mut app, &view, button_key(CatalogKind::Mcp, "new", "discard"));
    let server = app.doc.capabilities.mcps.iter().find(|r| r.name == "demo-mcp").unwrap().id.clone();
    rig.click(&mut app, &view, edit_button_key(mcp, &server));
   
    rig.settle(&mut app, &view);
    rig.shot(&mut app, &view, &out.join("mcp-edit.png"));
    rig.replace_in(&mut app, &view, field_key(CatalogKind::Mcp, &server, "name"), "renamed");
    rig.click(&mut app, &view, button_key(CatalogKind::Mcp, &server, "close"));
    rig.shot(&mut app, &view, &out.join("mcp-edit-discard.png"));
    rig.click(&mut app, &view, button_key(CatalogKind::Mcp, &server, "discard"));

    // The pointer resting on a row's edit button: it lights up, and no words appear.
    rig.hover(&mut app, &view, edit_button_key(mcp, &server));
    assert_eq!(rig.tooltip_layer_glyphs(), 0);
    rig.shot(&mut app, &view, &out.join("mcp-hover-edit.png"));

    // Resting on its name instead, it says where the row lives.
    let place = mewrk_settings_conv::logic::capability_workspace_key(None, rig.project.to_str().unwrap());
    rig.hover(&mut app, &view, mewrk_settings_conv::catalog::catalog_row_key(&format!("{mcp}:{place}"), &server));
    assert!(rig.tooltip_layer_glyphs() > 0);
    rig.shot(&mut app, &view, &out.join("mcp-hover-name.png"));

    rig.click(&mut app, &view, ("nav", "conv1", Page::Hooks));
    rig.shot(&mut app, &view, &out.join("hooks-page.png"));
    rig.click(&mut app, &view, add_button_key(hooks));
    rig.shot(&mut app, &view, &out.join("hook-new.png"));
    rig.click(&mut app, &view, ("select-box-anchor", "capability-event:new"));
    rig.shot(&mut app, &view, &out.join("hook-new-event.png"));
    rig.click(&mut app, &view, ("select-box-anchor", "capability-event:new"));
    rig.click(&mut app, &view, button_key(CatalogKind::Hooks, "new", "cancel"));
    let hook = app.doc.capabilities.hooks[0].id.clone();
    rig.click(&mut app, &view, edit_button_key(hooks, &hook));
    rig.settle(&mut app, &view);
    rig.shot(&mut app, &view, &out.join("hook-edit.png"));
    rig.click(&mut app, &view, button_key(CatalogKind::Hooks, &hook, "cancel"));

    // A short window: the buttons stay on screen and the fields scroll behind them.
    rig.size = (900.0, 500.0);
    rig.click(&mut app, &view, add_button_key(hooks));
    rig.shot(&mut app, &view, &out.join("hook-new-short.png"));
}
