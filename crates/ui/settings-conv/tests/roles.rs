//! `AgentRoleEditor.test.tsx` and the roles half of `ConversationSettings.test.tsx`, where they test logic or interaction.

mod support;

use std::cell::RefCell;

use fluent::el::hash_key;
use mewrk_client::model::{AgentModelSelection, ConversationSettings, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_settings_conv::catalog::catalog_row_key;
use mewrk_settings_conv::logic::*;
use mewrk_settings_conv::ops::{Done, Handlers, SaveAgentRoleTarget};
use mewrk_settings_conv::roles::{forget_agent_role_draft, RolePage, RolesPage};
use mewrk_settings_conv::tools::tool_row_key;
use mewrk_settings_conv::CapabilityWorkspace;
use support::*;

/// What a save asked for.
#[derive(Clone, Debug, PartialEq)]
struct Saved {
    target: SaveAgentRoleTarget,
    role: AgentRole,
}

#[derive(Clone)]
struct Fx {
    agent_ids: Rc<RefCell<Vec<String>>>,
    allow: Rc<RefCell<bool>>,
    saved: Rc<RefCell<Vec<Saved>>>,
    fail: Rc<RefCell<Option<String>>>,
    tools: Rc<Vec<ToolDescriptor>>,
    workspaces: Rc<RefCell<Option<Vec<CapabilityWorkspace>>>>,
    store: Rc<RefCell<Vec<(String, Vec<model::ContextItem>)>>>,
    delete_result: Rc<RefCell<bool>>,
    deleted: Rc<RefCell<Vec<String>>>,
    list_id: String,
}

impl Fx {
    fn new(list: &str) -> Self {
        Self {
            agent_ids: Rc::default(),
            allow: Rc::default(),
            saved: Rc::default(),
            fail: Rc::default(),
            tools: Rc::new(vec![
                tool("read", "读取", "filesystem", false),
                tool("bash", "Bash", "shell", true),
                tool("zsh", "Zsh", "shell", true),
                tool("preview_click", "点击", "web", false),
                tool("preview_start", "启动预览", "web", false),
                tool("preview_stop", "停止预览", "web", false),
                tool("preview_list", "预览列表", "web", false),
                tool("agent_spawn", "子代理", "orchestration", false),
                tool("ask_user", "提问", "orchestration", false),
                tool("read_global_memory", "读记忆", "memory", false),
            ]),
            workspaces: Rc::new(RefCell::new(Some(Vec::new()))),
            store: Rc::default(),
            delete_result: Rc::new(RefCell::new(true)),
            deleted: Rc::default(),
            list_id: list.to_owned(),
            ..Self::blank()
        }
    }

    fn blank() -> Self {
        Self { agent_ids: Rc::default(), allow: Rc::default(), saved: Rc::default(), fail: Rc::default(), tools: Rc::default(), workspaces: Rc::default(), store: Rc::default(), delete_result: Rc::default(), deleted: Rc::default(), list_id: String::new() }
    }

    fn handlers(&self) -> Handlers {
        let (saved, fail, s1, s2, deleted, result) = (self.saved.clone(), self.fail.clone(), self.store.clone(), self.store.clone(), self.deleted.clone(), self.delete_result.clone());
        Handlers {
            on_change: Rc::new(|_, _| {}),
            on_change_conversation_only: Rc::new(|_, _| {}),
            on_apply_preset: Rc::new(|_, _| {}),
            on_rename_preset: None,
            on_delete_preset: None,
            on_save_preset: None,
            on_save_preset_copy: None,
            on_bind_preset_template: None,
            on_read_template: Rc::new(move |app, id, done: Done<Vec<model::ContextItem>>| {
                let body = s1.borrow().iter().find(|(k, _)| *k == id).map(|(_, b)| b.clone()).unwrap_or_default();
                done(app, Ok(body));
            }),
            on_write_template: Rc::new(move |app, id, body, done: Done<String>| {
                let id = if id.is_empty() { "template_minted".to_owned() } else { id };
                s2.borrow_mut().retain(|(k, _)| *k != id);
                s2.borrow_mut().push((id.clone(), body));
                done(app, Ok(id));
            }),
            on_delete_capability: Some(Rc::new(move |app, _, r, done| {
                deleted.borrow_mut().push(r.id);
                done(app, *result.borrow());
            })),
            on_save_agent_role: Some(Rc::new(move |app, target, role, done: Done<String>| {
                saved.borrow_mut().push(Saved { target: target.clone(), role: role.clone() });
                match fail.borrow().clone() {
                    Some(reason) => done(app, Err(reason)),
                    None => done(app, Ok(target.id.unwrap_or_else(|| format!("agent_{}", role.name)))),
                }
            })),
            on_rescan: None,
            on_fingerprint: None,
            on_reveal: None,
            on_probe: None,
            on_create_preset: None,
            on_read_capability: None,
            on_save_capability: None,
            on_save_as_preset: None,
        }
    }

    fn view(&self) -> impl Fn(&mut App) -> E + use<> {
        let fx = self.clone();
        move |app: &mut App| {
            let (a1, a2, a3) = (fx.agent_ids.clone(), fx.allow.clone(), fx.agent_ids.clone());
            RolesPage {
                list_id: fx.list_id.clone(),
                agent_ids: fx.agent_ids.borrow().clone(),
                allow_roleless: *fx.allow.borrow(),
                on_select: Rc::new(move |_, ids| *a1.borrow_mut() = ids),
                on_allow_roleless: Rc::new(move |_, v| *a2.borrow_mut() = v),
                on_created: Rc::new(move |_, id| a3.borrow_mut().push(id)),
                on_untick_dangling: None,
                role_tools: fx.tools.clone(),
                workspaces: fx.workspaces.borrow().clone(),
                handlers: fx.handlers(),
                templates: Vec::new(),
                error: None,
            }
            .view(app)
        }
    }
}

const LIST: &str = "roles-list";

fn start(tag: &str) -> (Rig, App, Fx) {
    let (rig, mut app) = Rig::new(tag);
    empty_catalog(&mut app);
    (rig, app, Fx::new(LIST))
}

fn workspace_a() -> Vec<CapabilityWorkspace> {
    vec![CapabilityWorkspace { number: 1, key: "local|/w/a".into(), machine: None, path: "/w/a".into() }]
}

// ---------------------------------------------------------------- the page

#[test]
fn lists_the_roles_as_a_catalog_global_files_first_then_each_workspace_and_selects_by_id() {
    let (mut rig, mut app, fx) = start("roles-list");
    add_role(&mut app, resource("agent_w", "WorkRole", Some("local|/w/a")), Some(role_file("WorkRole")));
    add_role(&mut app, resource("agent_g", "GlobalRole", None), Some(role_file("GlobalRole")));
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view();
    rig.frame(&mut app, &view);
    let said = rig.said();
    let at = |what: &str| said.iter().position(|s| s == what).unwrap_or_else(|| panic!("{what} in {said:?}"));
    assert!(at("GlobalRole") < at("WorkRole"));
    rig.click(&mut app, &view, catalog_row_key(&format!("{LIST}:global"), "agent_g"));
    assert_eq!(*fx.agent_ids.borrow(), vec!["agent_g"]);
}

#[test]
fn does_not_count_a_selected_role_of_a_workspace_this_conversation_does_not_have() {
    let (mut rig, mut app, fx) = start("roles-count");
    add_role(&mut app, resource("agent_w", "WorkRole", Some("local|/elsewhere")), Some(role_file("WorkRole")));
    *fx.agent_ids.borrow_mut() = vec!["agent_w".into(), "agent_gone".into()];
    let view = fx.view();
    rig.frame(&mut app, &view);
    assert!(rig.shows("0 / 0 个已选"), "{:?}", rig.said());
    assert!(rig.shows("agent_w"), "still a row to untick");
    // Says the model simply does not see it, rather than promising a failure.
    assert!(rig.shows_containing("找不到这个角色的文件，模型看不到它"));
    let agents = agent_resources(&app.doc.capabilities);
    assert_eq!(selected_agent_role_count(&agents, &["agent_w".to_owned()], Some(&[])), 0);
    assert_eq!(selected_agent_role_count(&agents, &["agent_w".to_owned()], None), 1, "a preset narrows nothing");
}

#[test]
fn says_each_roles_model_to_a_reader_and_marks_one_whose_model_is_gone() {
    let (mut rig, mut app, fx) = start("roles-model");
    let mut bound = role_file("Bound");
    bound["modelSelection"] = json!({ "kind": "explicit", "providerId": "p1", "modelId": "m1" });
    let mut dead = role_file("Dead");
    dead["modelSelection"] = json!({ "kind": "explicit", "providerId": "p1", "modelId": "m9" });
    add_role(&mut app, resource("agent_bound", "Bound", None), Some(bound));
    add_role(&mut app, resource("agent_dead", "Dead", None), Some(dead));
    add_role(&mut app, resource("agent_inherit", "Inherit", None), Some(role_file("Inherit")));
    select_model(&mut app, provider("p1", "OpenAI", "anthropic", &[("m1", &[])]));
    let view = fx.view();
    rig.frame(&mut app, &view);
    assert!(rig.shows_containing("OpenAI · m1"));
    assert!(rig.shows_containing("跟随对话模型"));
    assert!(rig.shows_containing("OpenAI · m9 · 模型暂时取不到"), "{:?}", rig.said());
    assert!(rig.shows("模型不可用"));
}

#[test]
fn offers_no_settings_for_a_file_that_could_not_be_read_only_its_reason() {
    let (mut rig, mut app, fx) = start("roles-broken");
    let mut broken = resource("agent_x", "Broken", None);
    broken.description = "expected a JSON object".into();
    broken.available = false;
    add_role(&mut app, broken, None);
    *fx.agent_ids.borrow_mut() = vec!["agent_x".into()];
    let view = fx.view();
    rig.frame(&mut app, &view);
    assert!(!rig.shows("设置角色 Broken"));
    assert!(rig.shows_containing("expected a JSON object"));
    assert!(rig.shows_containing("已选择，但文件无法使用，模型看不到它"));
    assert!(rig.shows("不可用"));
}

#[test]
fn explains_the_role_less_switch_by_whether_a_selected_role_can_be_called() {
    let (mut rig, mut app, fx) = start("roles-roleless");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    rig.frame(&mut app, &view);
    assert!(rig.shows("当前没有可用角色，开关暂不生效。"), "drawn even with no usable role: {:?}", rig.said());
    assert!(rig.shows("角色必填"));
    rig.click(&mut app, &view, ("switch", format!("{LIST}:roleless")));
    assert!(*fx.allow.borrow());
    assert!(rig.shows("角色可选"));
    fx.agent_ids.borrow_mut().push("agent_g".into());
    rig.frame(&mut app, &view);
    assert!(rig.shows("不指定角色的子代理沿用本对话的模型。"));
}

#[test]
fn deletes_a_roles_file_on_the_second_click_and_keeps_its_draft_when_that_did_not_go_through() {
    let (mut rig, mut app, fx) = start("roles-delete");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    // Leave a half-written edit on the role.
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.type_into(&mut app, &view, ("role-name", "agent_g"), "2");
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "agent_g"))));
    *fx.delete_result.borrow_mut() = false;
    let key = hash_key(("delete", LIST, "agent_g"));
    rig.click(&mut app, &view, key);
    rig.click(&mut app, &view, key);
    assert_eq!(*fx.deleted.borrow(), vec!["agent_g"]);
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    assert!(rig.shows("保存角色"));
    rig.frame(&mut app, &view);
    // A refused delete leaves the half-written edit in place.
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.name, "G2");
    let _ = forget_agent_role_draft;
}

// ---------------------------------------------------------------- the window

#[test]
fn lays_the_role_out_as_a_presets_window_seven_pages_down_a_rail_the_save_at_its_foot() {
    let (mut rig, mut app, fx) = start("role-window");
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    for page in ["角色设置", "工具", "高级工具", "技能", "MCP", "钩子", "对话模板", "保存角色", "新建角色"] {
        assert!(rig.shows(page), "{page}: {:?}", rig.said());
    }
    // Nothing complains about an empty name before the user has had a chance to type one.
    assert!(!rig.shows("角色名称不能为空。"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert!(rig.shows("角色名称不能为空。"));
    assert!(rig.shows("请先修正标记的字段。"));
    assert!(fx.saved.borrow().is_empty());
}

#[test]
fn creates_a_role_at_the_level_it_is_asked_for_and_selects_it_in_the_conversation() {
    let (mut rig, mut app, fx) = start("role-create");
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "  审查员  ");
    assert!(rig.shows("全局") && rig.shows("项目"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    let saved = fx.saved.borrow().last().cloned().expect("saved");
    assert_eq!(saved.role.name, "审查员", "edge whitespace is the one thing normalised");
    assert_eq!(saved.target, SaveAgentRoleTarget { id: None, workspace_key: Some("local|/w/a".into()) });
    assert_eq!(*fx.agent_ids.borrow(), vec!["agent_审查员"], "a role written from here is meant to be used here");
    assert!(!rig.shows("保存角色"), "saving closes the window");
    // A new role is a reusable asset: every tool the picker offers, nothing of the conversation's.
    assert_eq!(saved.role.tools, vec!["bash", "preview_click", "preview_list", "preview_start", "preview_stop", "read", "zsh"]);
    assert_eq!(saved.role.model_selection, AgentModelSelection::Inherit);
    assert_eq!(saved.role.web_search, default_agent_role_web_search());
}

// `levelLabel`: a level on another machine is named as that machine, which is what tells two equal paths apart.
#[test]
fn a_level_on_an_ssh_machine_is_named_by_it() {
    let (mut rig, mut app, fx) = start("role-ssh-level");
    app.doc.assets.execution_environments.ssh_machines = vec![serde_json::from_value(json!({ "id": "m1", "name": "devbox", "host": "devbox.local" })).unwrap()];
    *fx.workspaces.borrow_mut() = Some(vec![CapabilityWorkspace { number: 1, key: "ssh:m1|/w/a".into(), machine: Some(mewrk_client::model::RunTarget::Ssh { machine_id: "m1".into() }), path: "/w/a".into() }]);
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    assert!(rig.shows("/w/a (SSH: devbox)"), "{:?}", mewrk_settings_conv::widgets::probe::said());
}

#[test]
fn offers_a_presets_window_the_global_level_alone() {
    let (mut rig, mut app, fx) = start("role-preset-level");
    *fx.workspaces.borrow_mut() = None;
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    // On show, so the choice reads the same everywhere, and it says why it can't be made.
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    assert!(rig.shows("预设不属于任何项目，它的角色只能放在全局。"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert_eq!(fx.saved.borrow().last().cloned().expect("saved").target.workspace_key, None);
}

#[test]
fn a_conversation_with_no_project_yet_can_only_write_a_global_role() {
    let (mut rig, mut app, fx) = start("role-no-project");
    *fx.workspaces.borrow_mut() = Some(Vec::new());
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    assert!(rig.shows("这个对话还没有项目。"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert_eq!(fx.saved.borrow().last().cloned().expect("saved").target.workspace_key, None);
}

#[test]
fn the_project_is_the_conversations_first_workspace() {
    let (mut rig, mut app, fx) = start("role-first-workspace");
    let second = CapabilityWorkspace { number: 2, key: "local|/w/b".into(), machine: None, path: "/w/b".into() };
    *fx.workspaces.borrow_mut() = Some(workspace_a().into_iter().chain([second]).collect());
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert_eq!(fx.saved.borrow().last().cloned().expect("saved").target.workspace_key.as_deref(), Some("local|/w/a"));
}

#[test]
fn overwrites_an_existing_roles_file_in_place_and_leaves_the_selection_alone() {
    let (mut rig, mut app, fx) = start("role-edit");
    add_role(&mut app, resource("agent_g", "Old", None), Some(role_file("Old")));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.type_into(&mut app, &view, ("role-name", "agent_g"), "New");
    // A rename keeps the file: it says so, and the window is still named as it was opened.
    assert!(rig.shows_containing("改名不会换文件"));
    assert!(rig.shows("Old"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    let saved = fx.saved.borrow().last().cloned().unwrap();
    assert_eq!(saved.target.id.as_deref(), Some("agent_g"));
    assert_eq!(saved.role.name, "OldNew");
    assert!(fx.agent_ids.borrow().is_empty());
}

#[test]
fn offers_the_tool_description_file_following_the_caller_by_default_and_saves_what_was_picked() {
    let (mut rig, mut app, fx) = start("role-profile");
    app.doc.capabilities.tool_description_files = vec![builtin("tooldesc_builtin_concise_en_us", "Mewrk concise"), resource("file_a", "Mine", None)];
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    assert!(rig.shows("跟随调用方"));
    // Unlike the conversation's page, the built-in is a row: it pins the default even under a caller on another file.
    assert!(rig.shows("option:Mewrk concise"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("select-box-anchor", "role:agent_g:profile"));
    rig.click(&mut app, &view, ("select-row", "role:agent_g:profile", "file_a"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.tool_description_file_id.as_deref(), Some("file_a"));
}

#[test]
fn keeps_a_tool_description_file_the_catalog_lost_as_an_unavailable_option_selected_and_saved_as_it_was() {
    let (mut rig, mut app, fx) = start("role-profile-gone");
    let mut file = role_file("G");
    file["toolDescriptionFileId"] = json!("file_gone");
    add_role(&mut app, resource("agent_g", "G", None), Some(file));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    assert!(rig.shows("不可用：file_gone"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.tool_description_file_id.as_deref(), Some("file_gone"));
}

#[test]
fn refuses_a_name_another_role_already_has_at_the_same_level_and_only_there() {
    let (mut rig, mut app, fx) = start("role-duplicate");
    add_role(&mut app, resource("agent_a", "Taken", None), Some(role_file("Taken")));
    add_role(&mut app, resource("agent_w", "ThereOnly", Some("local|/w/a")), Some(role_file("ThereOnly")));
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "Taken");
    assert!(rig.shows("同一位置已有同名的角色。"), "answers live once typed: {:?}", rig.said());
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert!(rig.shows("请先修正标记的字段。"));
    assert!(fx.saved.borrow().is_empty());
}

#[test]
fn a_workspace_role_may_share_a_name_with_a_global_one() {
    let (mut rig, mut app, fx) = start("role-shadow");
    add_role(&mut app, resource("agent_w", "Shared", Some("local|/w/a")), Some(role_file("Shared")));
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "Shared");
    assert!(!rig.shows("同一位置已有同名的角色。"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert_eq!(fx.saved.borrow().len(), 1);
}

#[test]
fn refuses_to_write_over_a_file_that_changed_since_the_window_opened() {
    let (mut rig, mut app, fx) = start("role-stale");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    // A text editor rewrote it meanwhile.
    empty_catalog(&mut app);
    let mut changed = role_file("G");
    changed["description"] = json!("rewritten elsewhere");
    add_role(&mut app, resource("agent_g", "G", None), Some(changed));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert!(rig.shows("此角色的文件已在别处变化。请关闭后重新打开再编辑。"));
    assert!(fx.saved.borrow().is_empty());
}

#[test]
fn shows_the_hosts_refusal_and_keeps_the_window_open() {
    let (mut rig, mut app, fx) = start("role-refused");
    *fx.fail.borrow_mut() = Some("disk is read-only".into());
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert!(rig.shows("disk is read-only"));
    assert!(rig.shows("保存角色"));
}

#[test]
fn draws_exactly_the_selectable_tool_catalogue_and_keeps_names_the_picker_cannot_draw_without_counting_them() {
    let (mut rig, mut app, fx) = start("role-tools");
    let mut file = role_file("G");
    file["tools"] = json!(["read", "agent_spawn", "retired_tool"]);
    add_role(&mut app, resource("agent_g", "G", None), Some(file));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Tools));
    assert!(rig.shows("工具 1"), "{:?}", rig.said());
    assert!(rig.shows("读取已启用"));
    for withheld in ["子代理已关闭", "子代理已启用", "提问已关闭", "读记忆已关闭", "启动预览已关闭"] {
        assert!(!rig.shows(withheld), "{withheld}");
    }
    rig.click(&mut app, &view, tool_row_key("role:agent_g", "preview_click"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    let tools = fx.saved.borrow().last().unwrap().role.tools.clone();
    // Names it does not offer are left alone, not stripped; the lifecycle tools come with the first preview tool.
    assert_eq!(tools, vec!["agent_spawn", "preview_click", "preview_list", "preview_start", "preview_stop", "read", "retired_tool"]);
}

#[test]
fn draws_the_presets_web_section_for_the_roles_own_configuration_speaking_of_its_own_model() {
    let (mut rig, mut app, fx) = start("role-web");
    let mut bound = role_file("G");
    bound["modelSelection"] = json!({ "kind": "explicit", "providerId": "p1", "modelId": "m1" });
    add_role(&mut app, resource("agent_g", "G", None), Some(bound));
    select_model(&mut app, provider("p1", "Claude", "anthropic", &[("m1", &[])]));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Advanced));
    assert!(rig.shows("原生用这个角色所跑模型自带的搜索。"), "{:?}", rig.said());
    assert!(rig.shows("让这个角色搜索网页。"));
    // What a role has no field for is left out.
    assert!(!rig.shows("启用全局记忆"));
    assert!(!rig.shows("启用文件防误写保护"));
    assert!(!rig.shows("宿主消息容器"));
    // A role bound to a Messages model carries the native versions and the number native fetch sends.
    assert!(rig.shows("option:web_search_20260209"));
    assert!(rig.shows("抓取结果压缩"));
    // It has no web access switch of its own: its caller decides that.
    rig.click(&mut app, &view, ("switch", "role:agent_g:web-search"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.web_search.provider, model::SearchProviderSelection::Disabled);
}

#[test]
fn has_no_version_to_pick_on_a_model_of_another_protocol_and_reads_native_fetch_off_without_rewriting() {
    let (mut rig, mut app, fx) = start("role-web-other");
    let mut bound = role_file("G");
    bound["modelSelection"] = json!({ "kind": "explicit", "providerId": "p1", "modelId": "m1" });
    bound["webSearch"]["fetchProvider"] = json!({ "kind": "native" });
    add_role(&mut app, resource("agent_g", "G", None), Some(bound));
    select_model(&mut app, provider("p1", "OpenAI", "openai_responses", &[("m1", &[])]));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Advanced));
    assert!(!rig.shows("option:web_search_20260209"));
    assert!(rig.shows("网页抓取已关闭"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.web_search.fetch_provider, model::FetchProviderSelection::Native, "saved as it was");
}

#[test]
fn picks_a_global_roles_own_skills_out_of_the_global_level_alone_and_a_workspace_role_its_own_too() {
    let (mut rig, mut app, fx) = start("role-skills");
    app.doc.capabilities.skills = vec![resource("skill_g", "GSkill", None), resource("skill_w", "WSkill", Some("local|/w/a"))];
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    add_role(&mut app, resource("agent_w", "W", Some("local|/w/a")), Some(role_file("W")));
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Skills));
    assert!(rig.shows("GSkill"));
    assert!(!rig.shows("WSkill"));
    // A delivery switch is the conversation's, and nothing here is deleted: this is a pick.
    assert!(!rig.shows("技能按需加载"));
    assert!(!rig.shows("删除 GSkill"));
    rig.click(&mut app, &view, catalog_row_key("role:agent_g:skills:global", "skill_g"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.skill_ids, vec!["skill_g"]);

    rig.click(&mut app, &view, ("role-configure", LIST, "agent_w"));
    rig.click(&mut app, &view, ("nav", "role:agent_w", mewrk_settings_conv::roles::RolePage::Skills));
    assert!(rig.shows("GSkill"));
    assert!(rig.shows("WSkill"));
}

#[test]
fn drops_a_new_roles_workspace_picks_when_it_moves_to_the_global_level() {
    let (mut rig, mut app, fx) = start("role-move");
    app.doc.capabilities.skills = vec![resource("skill_g", "GSkill", None), resource("skill_w", "WSkill", Some("local|/w/a"))];
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view();
    let page = mewrk_settings_conv::roles::RolePage::Skills;
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.click(&mut app, &view, ("nav", "role:new-role", page));
    rig.click(&mut app, &view, catalog_row_key("role:new-role:skills:global", "skill_g"));
    rig.click(&mut app, &view, catalog_row_key("role:new-role:skills:local|/w/a", "skill_w"));
    rig.click(&mut app, &view, ("nav", "role:new-role", mewrk_settings_conv::roles::RolePage::Role));
    rig.click(&mut app, &view, ("role-level", "new-role", "global"));
    rig.click(&mut app, &view, ("role-save", "new-role"));
    let saved = fx.saved.borrow().last().cloned().unwrap();
    assert_eq!(saved.role.skill_ids, vec!["skill_g"], "a global role naming a workspace's skill would dangle elsewhere");
    assert_eq!(saved.target.workspace_key, None);
}

#[test]
fn keeps_a_half_written_role_until_it_is_saved_whichever_window_opens_it() {
    let (mut rig, mut app, fx) = start("role-draft");
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "半成品");
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "new-role"))));
    assert!(!rig.shows("保存角色"));
    // A window dropped from memory, as leaving the page would: only the draft cache can bring it back.
    let quiet = |_: &mut App| -> E { div() };
    for _ in 0..4 {
        rig.frame(&mut app, &quiet);
    }
    let fx2 = Fx::new("another-list");
    let view2 = fx2.view();
    rig.click(&mut app, &view2, ("agent-role-add", "another-list"));
    rig.click(&mut app, &view2, ("role-save", "new-role"));
    assert_eq!(fx2.saved.borrow().last().unwrap().role.name, "半成品");
    // Saving has made it the file, so the next window starts blank.
    for _ in 0..4 {
        rig.frame(&mut app, &quiet);
    }
    let fx3 = Fx::new("third-list");
    let view3 = fx3.view();
    rig.click(&mut app, &view3, ("agent-role-add", "third-list"));
    rig.click(&mut app, &view3, ("role-save", "new-role"));
    assert!(rig.shows("角色名称不能为空。"));
}

#[test]
fn brings_a_half_written_workspace_role_to_a_window_without_that_workspace_as_a_global_one() {
    let (mut rig, mut app, fx) = start("role-draft-level");
    app.doc.capabilities.skills = vec![resource("skill_w", "WSkill", Some("local|/w/a"))];
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    rig.click(&mut app, &view, ("role-level", "new-role", "project"));
    rig.click(&mut app, &view, ("nav", "role:new-role", mewrk_settings_conv::roles::RolePage::Skills));
    rig.click(&mut app, &view, catalog_row_key("role:new-role:skills:local|/w/a", "skill_w"));
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "new-role"))));
    let quiet = |_: &mut App| -> E { div() };
    for _ in 0..4 {
        rig.frame(&mut app, &quiet);
    }
    let fx2 = Fx::new("no-workspace");
    let view2 = fx2.view();
    rig.click(&mut app, &view2, ("agent-role-add", "no-workspace"));
    rig.click(&mut app, &view2, ("role-save", "new-role"));
    let saved = fx2.saved.borrow().last().cloned().unwrap();
    assert_eq!(saved.target.workspace_key, None);
    assert!(saved.role.skill_ids.is_empty());
}

#[test]
fn keeps_an_edits_draft_until_the_file_changes_under_it() {
    let (mut rig, mut app, fx) = start("role-draft-file");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    let quiet = |_: &mut App| -> E { div() };
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.type_into(&mut app, &view, ("role-name", "agent_g"), "X");
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "agent_g"))));
    for _ in 0..4 {
        rig.frame(&mut app, &quiet);
    }
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "agent_g"))));
    for _ in 0..4 {
        rig.frame(&mut app, &quiet);
    }
    // The file moved: the stale draft is dropped rather than shown as if it were live.
    empty_catalog(&mut app);
    let mut changed = role_file("G");
    changed["description"] = json!("changed");
    add_role(&mut app, resource("agent_g", "G", None), Some(changed));
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    let saved = fx.saved.borrow().last().cloned().unwrap();
    assert_eq!(saved.role.name, "G");
    assert_eq!(saved.role.description, "changed");
}

#[test]
fn does_not_dismiss_an_in_progress_window_when_its_backdrop_is_clicked() {
    let (mut rig, mut app, fx) = start("role-backdrop");
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    // A press at the window's corner, outside the box: a stray click would discard an in-progress role without asking.
    rig.click_at(&mut app, &view, 1.0, 1.0);
    assert!(rig.shows("保存角色"));
    // The close button is a decision.
    rig.click(&mut app, &view, ("settings-window-close", hash_key(("role-window", "new-role"))));
    assert!(!rig.shows("保存角色"));
}

#[test]
fn names_a_binding_whose_provider_is_only_disabled_and_saves_it_untouched() {
    let (mut rig, mut app, fx) = start("role-binding");
    let mut bound = role_file("G");
    bound["modelSelection"] = json!({ "kind": "explicit", "providerId": "p1", "modelId": "m1" });
    add_role(&mut app, resource("agent_g", "G", None), Some(bound));
    let mut p = provider("p1", "Claude", "anthropic", &[("m1", &[])]);
    p.enabled = false;
    select_model(&mut app, p);
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    assert!(rig.shows("Claude · m1（不可用）"), "{:?}", rig.said());
    assert!(rig.shows_containing("这个模型现在取不到"));
    // A dead binding is not a save blocker: it would trap every other edit behind it.
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.model_selection, AgentModelSelection::Explicit { provider_id: "p1".into(), model_id: "m1".into() });
}

#[test]
fn says_a_broken_binding_an_older_build_recorded_cannot_run_and_offers_the_enabled_models() {
    let (mut rig, mut app, fx) = start("role-binding-old");
    let mut dead = role_file("G");
    dead["modelSelection"] = json!({ "kind": "unavailable" });
    add_role(&mut app, resource("agent_g", "G", None), Some(dead));
    select_model(&mut app, provider("p1", "Claude", "anthropic", &[("m1", &[]), ("m2", &[])]));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    assert!(rig.shows("请选择执行模型"));
    assert!(rig.shows_containing("旧版本记下的失效绑定"));
    rig.click(&mut app, &view, ("select-box-anchor", "role:agent_g:model"));
    rig.click(&mut app, &view, ("select-row", "role:agent_g:model", "model-1"));
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.model_selection, AgentModelSelection::Explicit { provider_id: "p1".into(), model_id: "m2".into() });
}

#[test]
fn edits_the_roles_template_on_its_own_page_and_saves_the_minted_id_into_the_file() {
    let (mut rig, mut app, fx) = start("role-template");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Template));
    assert!(rig.shows("这个角色的开局历史。"));
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "开局");
    rig.frame(&mut app, &view);
    assert_eq!(fx.store.borrow().len(), 1);
    assert!(rig.shows("对话模板 1"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("role-save", "agent_g"));
    assert_eq!(fx.saved.borrow().last().unwrap().role.template_id.as_deref(), Some("template_minted"));
}

#[test]
fn copies_a_presets_body_over_an_empty_template_at_once_and_asks_before_replacing_one_that_says_something() {
    let (mut rig, mut app, fx) = start("role-overwrite");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let settings = mewrk_client::presets::empty_conversation_preset_settings();
    app.doc.presets.conversation_presets.push(model::ConversationPreset { id: "preset_t".into(), name: "WithBody".into(), description: String::new(), template_id: "template_p".into(), settings: settings.clone() });
    app.doc.presets.conversation_presets.push(model::ConversationPreset { id: "preset_none".into(), name: "Bare".into(), description: String::new(), template_id: String::new(), settings });
    let body: Vec<model::ContextItem> = vec![serde_json::from_value(json!({ "kind": "user", "id": "c1", "createdAt": "", "content": "from preset" })).unwrap()];
    fx.store.borrow_mut().push(("template_p".into(), body));
    // The summaries the menu counts by.
    let view = {
        let fx = fx.clone();
        move |app: &mut App| {
            let (a1, a2, a3) = (fx.agent_ids.clone(), fx.allow.clone(), fx.agent_ids.clone());
            RolesPage {
                list_id: fx.list_id.clone(),
                agent_ids: fx.agent_ids.borrow().clone(),
                allow_roleless: false,
                on_select: Rc::new(move |_, ids| *a1.borrow_mut() = ids),
                on_allow_roleless: Rc::new(move |_, v| *a2.borrow_mut() = v),
                on_created: Rc::new(move |_, id| a3.borrow_mut().push(id)),
                on_untick_dangling: None,
                role_tools: fx.tools.clone(),
                workspaces: Some(Vec::new()),
                handlers: fx.handlers(),
                templates: vec![mewrk_settings_conv::ops::TemplateSummary { id: "template_p".into(), name: String::new(), message_count: 1, created_at: String::new(), updated_at: String::new() }],
                error: None,
            }
            .view(app)
        }
    };
    use mewrk_ui_common::popover_menu::{probe, row_key, trigger_key};
    let menu = mewrk_settings_conv::roles::copy_preset_menu_key("agent_g");
    rig.click(&mut app, &view, ("role-configure", LIST, "agent_g"));
    rig.click(&mut app, &view, ("nav", "role:agent_g", mewrk_settings_conv::roles::RolePage::Template));
    // A menu of its own, whose words its own probe hears.
    probe::start();
    rig.click(&mut app, &view, trigger_key(menu.as_str()));
    rig.frame(&mut app, &view);
    let words = probe::take();
    assert!(words.iter().any(|said| said == "1 条消息"), "{words:?}");
    assert!(words.iter().any(|said| said == "这份预设没有对话模板。"));
    rig.click(&mut app, &view, row_key(menu.as_str(), "preset_none"));
    assert!(rig.exists(row_key(menu.as_str(), "preset_t")), "a preset with nothing to copy cannot be chosen");
    rig.click(&mut app, &view, row_key(menu.as_str(), "preset_t"));
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    // Replacing an empty template loses nothing, so it just happens.
    assert!(rig.exists(mewrk_ui_chat::frame::unit_key(0, "c1")), "{:?}", rig.said());
    assert!(!rig.shows("覆盖对话模板？"));
    // Now it says something, so the next one asks.
    rig.click(&mut app, &view, trigger_key(menu.as_str()));
    rig.click(&mut app, &view, row_key(menu.as_str(), "preset_t"));
    assert!(rig.shows("覆盖对话模板？"));
    assert!(rig.shows_containing("会被预设「WithBody」的对话模板整个替换"));
    rig.click(&mut app, &view, ("role-overwrite-cancel", "agent_g"));
    assert!(!rig.shows("覆盖对话模板？"));
}

#[test]
fn says_what_each_page_is_for_in_one_line_and_links_the_role_window_where_the_caller_matters() {
    let (mut rig, mut app, fx) = start("role-lines");
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    for (page, line) in [(mewrk_settings_conv::roles::RolePage::Role, "名称、描述、模型与位置。"), (mewrk_settings_conv::roles::RolePage::Tools, "这个角色能调用的工具。"), (mewrk_settings_conv::roles::RolePage::Advanced, "这个角色自己的联网搜索与抓取。"), (mewrk_settings_conv::roles::RolePage::Skills, "这个角色的子代理能用的技能。"), (mewrk_settings_conv::roles::RolePage::Mcp, "这个角色的子代理可用的 MCP Server。"), (mewrk_settings_conv::roles::RolePage::Hooks, "这个角色的子代理运行的钩子。")] {
        rig.click(&mut app, &view, ("nav", "role:new-role", page));
        assert!(rig.shows(line), "{line}: {:?}", rig.said());
    }
}

// ---------------------------------------------------------------- pure rules

#[test]
fn validates_a_role_name_as_free_text_refusing_only_what_cannot_survive() {
    assert_eq!(validate_agent_type_name("代码审查"), None);
    assert_eq!(validate_agent_type_name("Code Reviewer"), None);
    assert_eq!(validate_agent_type_name(""), Some(AgentTypeNameError::Required));
    assert_eq!(validate_agent_type_name(&"x".repeat(MAX_AGENT_TYPE_CHARS + 1)), Some(AgentTypeNameError::TooLong));
    assert_eq!(validate_agent_type_name("a\nb"), Some(AgentTypeNameError::Characters));
}

#[test]
fn a_new_roles_tools_are_every_picker_row_with_the_lifecycle_tools_brought_along() {
    let tools = vec![tool("read", "r", "filesystem", false), tool("skill", "s", "orchestration", false), tool("preview_click", "c", "web", false), tool("preview_start", "s", "web", false), tool("agent_spawn", "a", "orchestration", false), tool("read_global_memory", "m", "memory", false)];
    let selectable = mewrk_settings_conv::roles::selectable_role_tools(&tools);
    assert_eq!(mewrk_settings_conv::roles::every_selectable_tool_name(&selectable), vec!["preview_click", "preview_start", "read"]);
}

fn settings_with(ids: &[&str]) -> ConversationSettings {
    let mut s: ConversationSettings = serde_json::from_value(json!({ "enabledTools": [] })).unwrap();
    s.agent_ids = ids.iter().map(|x| (*x).to_owned()).collect();
    s
}

#[test]
fn reachable_picks_keep_the_builtins_and_the_level_alone() {
    let catalog: model::CapabilityCatalog = serde_json::from_value(json!({ "mcps": [], "skills": [
        { "id": "b", "name": "b", "description": "", "location": "", "source": "builtin", "available": true, "workspaceKey": "local|/w/x" },
        { "id": "g", "name": "g", "description": "", "location": "", "source": "user", "available": true },
        { "id": "w", "name": "w", "description": "", "location": "", "source": "workspace", "available": true, "workspaceKey": "local|/w/a" },
    ] })).unwrap();
    let role = AgentRole { skill_ids: vec!["b".into(), "g".into(), "w".into()], ..AgentRole::default() };
    assert_eq!(mewrk_settings_conv::roles::with_reachable_picks(&role, None, &catalog).skill_ids, vec!["b", "g"]);
    assert_eq!(mewrk_settings_conv::roles::with_reachable_picks(&role, Some("local|/w/a"), &catalog).skill_ids, vec!["b", "g", "w"]);
    let _ = settings_with(&[]);
}

// ---------------------------------------------------------------- accessibility

#[test]
fn a_roles_window_names_its_fields_and_says_when_one_is_wrong() {
    let (mut rig, mut app, fx) = start("a11y-role");
    add_role(&mut app, resource("agent_g", "G", None), Some(role_file("G")));
    let view = fx.view();
    fx.agent_ids.borrow_mut().push("agent_g".into());
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    assert_eq!(a11y_is(&tree, Role::Button, "G").toggled(), Some(accesskit::Toggled::True));
    a11y_is(&tree, Role::Button, "设置角色 G");
    a11y_is(&tree, Role::Switch, "角色必填");

    rig.click(&mut app, &view, ("agent-role-add", LIST));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::Dialog, "新建角色").is_modal());
    a11y_is(&tree, Role::Navigation, "角色设置分类");
    a11y_is(&tree, Role::Region, "角色设置");
    a11y_is(&tree, Role::TextInput, "角色名称");
    a11y_is(&tree, Role::MultilineTextInput, "子代理描述");
    a11y_is(&tree, Role::ComboBox, "执行模型");
    a11y_is(&tree, Role::ComboBox, "思考程度");
    a11y_is(&tree, Role::ComboBox, "工具描述");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());

    // Pressing save on a blank name is how a window with nothing greyed out says what is missing.
    rig.click(&mut app, &view, ("role-save", "new-role"));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::TextInput, "角色名称").invalid().is_some());
    set_a11y_active(false);
}

#[test]
fn every_page_of_a_roles_window_names_every_control() {
    let (mut rig, mut app, fx) = start("a11y-role-pages");
    let view = fx.view();
    rig.click(&mut app, &view, ("agent-role-add", LIST));
    // Not the template page: that is the conversation view, which has its own names.
    for page in [RolePage::Role, RolePage::Tools, RolePage::Advanced, RolePage::Skills, RolePage::Mcp, RolePage::Hooks] {
        rig.click(&mut app, &view, ("nav", "role:new-role", page));
        let tree = a11y_tree(&mut rig, &mut app, &view);
        assert_eq!(a11y_unnamed(&tree), Vec::<String>::new(), "{page:?}");
        a11y_is(&tree, Role::Navigation, "角色设置分类");
    }
    set_a11y_active(false);
}
