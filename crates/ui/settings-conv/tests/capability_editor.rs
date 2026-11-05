//! Adding and editing skills, MCP servers and hooks through a real host: the page's button and each row's, the window they open, and what lands in the real files under a real `.mewrk`.

mod support;

use fluent::el::hash_key;
use mewrk_client::prelude::*;
use mewrk_settings_conv::capability_editor::{add_button_key, button_key, edit_button_key, field_key};
use mewrk_settings_conv::logic::capability_workspace_key;
use mewrk_settings_conv::{conversation_settings_pane, CatalogKind, Page};
use support::*;

const CONV: &str = "conv1";

fn view(app: &mut App) -> E {
    conversation_settings_pane(app).unwrap_or_else(|| div())
}

fn settings_of(app: &mut App) -> model::ConversationSettings {
    app.active_conversation_view().expect("conversation").settings
}

fn list(page: &str) -> String {
    format!("catalog:{CONV}:{page}")
}

/// A conversation in the seeded project, its settings pane open on `page`, the first scan done.
fn open(tag: &str, page: Page) -> (Rig, App) {
    let (mut rig, mut app) = Rig::new(tag);
    init_live(&mut app);
    create_conversation(&mut app, CONV);
    rig.until(&mut app, &view, "the project's entries", |app| app.doc.capabilities.skills.iter().any(|s| s.name == "demo-skill"));
    rig.click(&mut app, &view, ("nav", CONV, page));
    (rig, app)
}

fn save_and_wait(rig: &mut Rig, app: &mut App, kind: CatalogKind, scope: &str) {
    let window = ("capability-editor-frame", kind.wire(), scope);
    rig.click(app, &view, button_key(kind, scope, "save"));
    rig.until(app, &view, "the window to close", move |_| fluent::el::rect_of(window).is_none());
}

/// A window reads its file through the host, so what it holds arrives a few frames later.
fn wait_for_field(rig: &mut Rig, app: &mut App, kind: CatalogKind, scope: &str, field: &str) {
    let key = hash_key(("field", field_key(kind, scope, field)));
    rig.until(app, &view, "the form", move |_| fluent::el::rect_of(key).is_some());
}

fn id_of(resources: &[model::ResourceDescriptor], name: &str) -> String {
    resources.iter().find(|r| r.name.contains(name)).unwrap_or_else(|| panic!("{name} in {resources:?}")).id.clone()
}

#[test]
fn every_page_offers_new_and_every_file_on_it_offers_edit() {
    let (mut rig, mut app) = open("cap-buttons", Page::Skills);
    let skill = id_of(&app.doc.capabilities.skills, "demo-skill");
    rig.frame(&mut app, &view);
    assert!(rig.exists(add_button_key(&list("skills"))));
    assert!(rig.shows("新建技能"));
    assert!(rig.exists(edit_button_key(&list("skills"), &skill)));
    // The built-in has no file: nothing to edit, as there is nothing to delete.
    let builtin = app.doc.capabilities.skills.iter().find(|r| r.source == model::ResourceSource::Builtin).expect("the SDK skill").id.clone();
    assert!(rig.exists(mewrk_settings_conv::catalog::catalog_row_key(&format!("{}:global", list("skills")), &builtin)));
    assert!(!rig.exists(edit_button_key(&list("skills"), &builtin)));

    rig.click(&mut app, &view, ("nav", CONV, Page::Mcp));
    let mcp = id_of(&app.doc.capabilities.mcps, "demo-mcp");
    assert!(rig.exists(add_button_key(&list("mcp"))));
    assert!(rig.exists(edit_button_key(&list("mcp"), &mcp)));
    assert!(rig.shows("新建 MCP Server"));

    rig.click(&mut app, &view, ("nav", CONV, Page::Hooks));
    let hook = app.doc.capabilities.hooks.first().expect("the project's hook").id.clone();
    assert!(rig.exists(add_button_key(&list("hooks"))));
    assert!(rig.exists(edit_button_key(&list("hooks"), &hook)));
    assert!(rig.shows("新建钩子"));
}

#[test]
fn resting_on_a_rows_name_says_where_it_lives_and_resting_on_its_button_says_nothing() {
    let (mut rig, mut app) = open("cap-hover", Page::Mcp);
    let mcp = app.doc.capabilities.mcps.iter().find(|r| r.name.contains("demo-mcp")).expect("seeded").clone();
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    rig.hover(&mut app, &view, edit_button_key(&list("mcp"), &mcp.id));
    assert_eq!(rig.tooltip_layer_glyphs(), 0);
    rig.hover(&mut app, &view, mewrk_settings_conv::catalog::catalog_row_key(&format!("{}:{key}", list("mcp")), &mcp.id));
    assert!(!mcp.location.is_empty());
    assert_eq!(rig.tooltip_layer_glyphs(), mcp.location.chars().filter(|c| !c.is_whitespace()).count());
}

#[test]
fn a_new_skill_is_written_as_a_folder_listed_and_selected() {
    let (mut rig, mut app) = open("cap-new-skill", Page::Skills);
    let kind = CatalogKind::Skills;
    rig.click(&mut app, &view, add_button_key(&list("skills")));
    // A blank is not complained of until a save is tried.
    assert!(!rig.shows("文件夹名不能为空。"));
    rig.click(&mut app, &view, button_key(kind, "new", "save"));
    assert!(rig.shows("文件夹名不能为空。"), "{:?}", rig.said());
    assert!(rig.shows("正文不能为空。"));

    rig.type_into(&mut app, &view, field_key(kind, "new", "directory"), "release-notes");
    rig.type_into(&mut app, &view, field_key(kind, "new", "name"), "Release notes");
    rig.type_into(&mut app, &view, field_key(kind, "new", "description"), "Write the notes: from the log");
    rig.type_into(&mut app, &view, field_key(kind, "new", "body"), "# Steps\nRead the log.");
    save_and_wait(&mut rig, &mut app, kind, "new");

    let manifest = rig.home.join(".mewrk/skills/release-notes/SKILL.md");
    let text = std::fs::read_to_string(&manifest).unwrap_or_else(|e| panic!("{e}: {:?}", rig.said()));
    assert!(text.contains("name: Release notes\n"), "{text}");
    assert!(text.ends_with("# Steps\nRead the log.\n"), "{text}");
    let id = id_of(&app.doc.capabilities.skills, "Release notes");
    assert!(settings_of(&mut app).skill_ids.contains(&id), "a new entry is meant to be used here");
}

#[test]
fn a_new_skill_can_go_in_the_project_instead() {
    let (mut rig, mut app) = open("cap-skill-project", Page::Skills);
    let kind = CatalogKind::Skills;
    rig.click(&mut app, &view, add_button_key(&list("skills")));
    rig.click(&mut app, &view, ("capability-level", kind.wire(), "project"));
    rig.type_into(&mut app, &view, field_key(kind, "new", "directory"), "in-the-project");
    rig.type_into(&mut app, &view, field_key(kind, "new", "body"), "Do it.");
    save_and_wait(&mut rig, &mut app, kind, "new");
    assert!(rig.project.join(".mewrk/skills/in-the-project/SKILL.md").is_file());
    assert!(!rig.home.join(".mewrk/skills/in-the-project").exists());
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    let listed = app.doc.capabilities.skills.iter().find(|r| r.name == "in-the-project").expect("listed");
    assert_eq!(listed.workspace_key.as_deref(), Some(key.as_str()));
}

#[test]
fn editing_a_skill_opens_on_the_file_and_changes_only_what_changed() {
    let (mut rig, mut app) = open("cap-edit-skill", Page::Skills);
    let manifest = rig.project.join(".mewrk/skills/demo-skill/SKILL.md");
    std::fs::write(&manifest, "---\nauthor: me\nname: demo-skill\ndescription: A skill of this project\n---\n\nDo the thing.\n").unwrap();
    let kind = CatalogKind::Skills;
    let id = id_of(&app.doc.capabilities.skills, "demo-skill");
    rig.click(&mut app, &view, edit_button_key(&list("skills"), &id));
    wait_for_field(&mut rig, &mut app, kind, &id, "body");
    assert!(rig.shows("编辑 demo-skill"), "{:?}", rig.said());
    assert!(rig.shows_containing("SKILL.md"), "the window says where the file is");
    // The folder is the identity, so it shows and cannot be typed into.
    assert!(rig.shows("demo-skill"));
    assert!(!rig.exists(hash_key(("field", field_key(kind, &id, "directory")))));

    rig.replace_in(&mut app, &view, field_key(kind, &id, "body"), "Do the other thing.");
    save_and_wait(&mut rig, &mut app, kind, &id);
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "---\nauthor: me\nname: demo-skill\ndescription: A skill of this project\n---\n\nDo the other thing.\n");
    assert_eq!(id_of(&app.doc.capabilities.skills, "demo-skill"), id, "the folder did not move");
}

#[test]
fn a_new_server_of_either_transport_lands_in_mcp_json() {
    let (mut rig, mut app) = open("cap-new-mcp", Page::Mcp);
    let kind = CatalogKind::Mcp;
    rig.click(&mut app, &view, add_button_key(&list("mcp")));
    rig.type_into(&mut app, &view, field_key(kind, "new", "name"), "bad name");
    assert!(rig.shows("名称只能包含字母、数字、连字符和下划线。"), "{:?}", rig.said());
    rig.replace_in(&mut app, &view, field_key(kind, "new", "name"), "files");
    rig.type_into(&mut app, &view, field_key(kind, "new", "command"), "/bin/echo");
    rig.type_into(&mut app, &view, field_key(kind, "new", "args"), "-n\nhello");
    rig.type_into(&mut app, &view, field_key(kind, "new", "env"), "TOKEN=${MEWRK_TEST_UNSET}\nnot a pair");
    assert!(rig.shows("第 2 行读不出来。"), "{:?}", rig.said());
    rig.click(&mut app, &view, button_key(kind, "new", "save"));
    assert!(rig.shows("请先修正标记的字段。"));
    assert!(!rig.home.join(".mewrk/mcp.json").exists());
    rig.replace_in(&mut app, &view, field_key(kind, "new", "env"), "TOKEN=${MEWRK_TEST_UNSET}");
    save_and_wait(&mut rig, &mut app, kind, "new");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(rig.home.join(".mewrk/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["files"], json!({ "command": "/bin/echo", "args": ["-n", "hello"], "env": { "TOKEN": "${MEWRK_TEST_UNSET}" } }));

    // The row says why it cannot run: there is no hovering to find out.
    rig.frame(&mut app, &view);
    assert!(rig.shows_containing("MEWRK_TEST_UNSET"), "{:?}", rig.said());

    rig.click(&mut app, &view, add_button_key(&list("mcp")));
    rig.type_into(&mut app, &view, field_key(kind, "new", "name"), "files");
    assert!(rig.shows("同一位置已有同名的服务器。"), "{:?}", rig.said());
    rig.replace_in(&mut app, &view, field_key(kind, "new", "name"), "docs");
    rig.click(&mut app, &view, ("capability-transport", "new", "http"));
    rig.type_into(&mut app, &view, field_key(kind, "new", "url"), "https://example.com/mcp");
    rig.type_into(&mut app, &view, field_key(kind, "new", "headers"), "Authorization: Bearer ${MEWRK_TEST_UNSET}");
    save_and_wait(&mut rig, &mut app, kind, "new");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(rig.home.join(".mewrk/mcp.json")).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["docs"], json!({ "type": "http", "url": "https://example.com/mcp", "headers": { "Authorization": "Bearer ${MEWRK_TEST_UNSET}" } }));
    assert_eq!(file["mcpServers"]["files"]["command"], "/bin/echo", "the other entry stays");
}

#[test]
fn an_edited_server_keeps_its_references_and_a_renamed_one_keeps_this_selection() {
    let (mut rig, mut app) = open("cap-edit-mcp", Page::Mcp);
    let path = rig.project.join(".mewrk/mcp.json");
    std::fs::write(&path, r#"{"mcpServers":{"demo-mcp":{"command":"/bin/echo","env":{"TOKEN":"${HOME}"},"disabledTools":["danger"]}}}"#).unwrap();
    rig.click(&mut app, &view, ("rescan", list("mcp")));
    rig.until(&mut app, &view, "the rescan", |app| app.doc.capabilities.mcps.iter().any(|r| r.name == "demo-mcp"));
    let kind = CatalogKind::Mcp;
    let id = id_of(&app.doc.capabilities.mcps, "demo-mcp");
    let key = capability_workspace_key(None, rig.project.to_str().unwrap());
    rig.click(&mut app, &view, mewrk_settings_conv::catalog::catalog_row_key(&format!("{}:{key}", list("mcp")), &id));
    assert!(settings_of(&mut app).mcp_ids.contains(&id));

    rig.click(&mut app, &view, edit_button_key(&list("mcp"), &id));
    wait_for_field(&mut rig, &mut app, kind, &id, "name");
    rig.replace_in(&mut app, &view, field_key(kind, &id, "name"), "renamed");
    assert!(rig.shows_containing("改名后就是另一台服务器"), "{:?}", rig.said());
    save_and_wait(&mut rig, &mut app, kind, &id);

    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file["mcpServers"]["renamed"], json!({ "command": "/bin/echo", "env": { "TOKEN": "${HOME}" }, "disabledTools": ["danger"] }), "never the expanded value, and keys the form lacks stay");
    let new_id = id_of(&app.doc.capabilities.mcps, "renamed");
    assert_ne!(new_id, id);
    let selected = settings_of(&mut app).mcp_ids;
    assert!(selected.contains(&new_id) && !selected.contains(&id), "{selected:?}");
}

#[test]
fn a_hook_is_created_edited_and_its_selection_follows_a_new_identity() {
    let (mut rig, mut app) = open("cap-hook", Page::Hooks);
    let kind = CatalogKind::Hooks;
    rig.click(&mut app, &view, add_button_key(&list("hooks")));
    rig.type_into(&mut app, &view, field_key(kind, "new", "name"), "Check it");
    rig.type_into(&mut app, &view, field_key(kind, "new", "command"), "true");
    rig.type_into(&mut app, &view, field_key(kind, "new", "matcher"), "^bash$");
    rig.replace_in(&mut app, &view, field_key(kind, "new", "timeout"), "9");
    save_and_wait(&mut rig, &mut app, kind, "new");
    let path = rig.home.join(".mewrk/hooks.json");
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file["hooks"]["PreToolUse"][0], json!({ "matcher": "^bash$", "hooks": [{ "type": "command", "name": "Check it", "command": "true", "timeout": 9 }] }));
    let id = id_of(&app.doc.capabilities.hooks, "Check it");
    assert!(settings_of(&mut app).hook_ids.contains(&id));

    // A bad matcher is the host's to refuse, and the window stays open on the reason.
    rig.click(&mut app, &view, edit_button_key(&list("hooks"), &id));
    wait_for_field(&mut rig, &mut app, kind, &id, "command");
    rig.replace_in(&mut app, &view, field_key(kind, &id, "matcher"), "(");
    rig.click(&mut app, &view, button_key(kind, &id, "save"));
    rig.until(&mut app, &view, "the refusal", |_| probe_said("匹配器不是合法的正则表达式"));
    assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(), file, "nothing was written");

    rig.replace_in(&mut app, &view, field_key(kind, &id, "matcher"), "^zsh$");
    assert!(rig.shows_containing("就是另一个钩子"), "{:?}", rig.said());
    save_and_wait(&mut rig, &mut app, kind, &id);
    let new_id = id_of(&app.doc.capabilities.hooks, "Check it");
    assert_ne!(new_id, id, "a matcher is part of what makes it this hook");
    let selected = settings_of(&mut app).hook_ids;
    assert!(selected.contains(&new_id) && !selected.contains(&id), "{selected:?}");
}

fn probe_said(text: &str) -> bool {
    mewrk_settings_conv::widgets::probe::said().iter().any(|s| s.contains(text))
}

#[test]
fn closing_with_changes_asks_first_and_a_click_outside_changes_nothing() {
    let (mut rig, mut app) = open("cap-close", Page::Hooks);
    let kind = CatalogKind::Hooks;
    let frame = ("capability-editor-frame", kind.wire(), "new");
    rig.click(&mut app, &view, add_button_key(&list("hooks")));
    // Nothing typed, nothing to lose.
    rig.click(&mut app, &view, button_key(kind, "new", "cancel"));
    rig.frame(&mut app, &view);
    assert!(!rig.exists(frame));

    rig.click(&mut app, &view, add_button_key(&list("hooks")));
    rig.type_into(&mut app, &view, field_key(kind, "new", "command"), "true");
    rig.click_at(&mut app, &view, 2.0, 2.0);
    rig.frame(&mut app, &view);
    assert!(rig.exists(frame), "a stray click does not lose what is typed");

    rig.click(&mut app, &view, button_key(kind, "new", "close"));
    assert!(rig.shows("有未保存的修改，要放弃吗？"), "{:?}", rig.said());
    rig.click(&mut app, &view, button_key(kind, "new", "keep"));
    assert!(rig.exists(frame));
    assert!(!rig.shows("有未保存的修改，要放弃吗？"));

    rig.click(&mut app, &view, button_key(kind, "new", "cancel"));
    rig.click(&mut app, &view, button_key(kind, "new", "discard"));
    rig.frame(&mut app, &view);
    assert!(!rig.exists(frame));
    assert!(!rig.home.join(".mewrk/hooks.json").exists());

    // A window opened again starts clean.
    rig.click(&mut app, &view, add_button_key(&list("hooks")));
    rig.click(&mut app, &view, button_key(kind, "new", "cancel"));
    rig.frame(&mut app, &view);
    assert!(!rig.exists(frame), "there was nothing left to ask about");
}

#[test]
fn a_file_changed_while_the_window_was_open_is_not_written_over() {
    let (mut rig, mut app) = open("cap-conflict", Page::Mcp);
    let kind = CatalogKind::Mcp;
    let id = id_of(&app.doc.capabilities.mcps, "demo-mcp");
    rig.click(&mut app, &view, edit_button_key(&list("mcp"), &id));
    wait_for_field(&mut rig, &mut app, kind, &id, "description");
    let path = rig.project.join(".mewrk/mcp.json");
    let by_hand = r#"{"mcpServers":{"demo-mcp":{"command":"/bin/changed-by-hand"}}}"#;
    std::fs::write(&path, by_hand).unwrap();
    rig.type_into(&mut app, &view, field_key(kind, &id, "description"), "mine");
    rig.click(&mut app, &view, button_key(kind, &id, "save"));
    rig.until(&mut app, &view, "the refusal", |_| probe_said("已在别处变化"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), by_hand);
}

#[test]
fn the_window_scrolls_rather_than_running_off_a_short_screen() {
    let (mut rig, mut app) = open("cap-short", Page::Mcp);
    rig.size = (900.0, 420.0);
    rig.click(&mut app, &view, add_button_key(&list("mcp")));
    let frame = fluent::el::rect_of(("capability-editor-frame", "mcp", "new")).expect("open");
    assert!(frame.h <= 420.0 - 48.0 + 0.5, "{frame:?}");
    // The buttons stay on screen with everything else scrolled behind them.
    let save = fluent::el::rect_of(button_key(CatalogKind::Mcp, "new", "save")).expect("the save button");
    assert!(save.y + save.h <= frame.y + frame.h, "{save:?} {frame:?}");
    assert!(save.y + save.h <= 420.0, "{save:?}");
}
