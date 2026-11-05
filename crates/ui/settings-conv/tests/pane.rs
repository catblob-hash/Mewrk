//! `ConversationSettings.test.tsx`, where it tests logic or interaction. The handlers here are in memory, as upstream's mocks are; `live.rs` runs the real ones.

mod support;

use std::cell::RefCell;

use fluent::el::hash_key;
use mewrk_client::model::{Conversation, ConversationSettings, ConversationToolLock, FetchProviderSelection, HostMessageContainer, ResourceSource, SearchProviderKind, SearchProviderSelection, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_settings_conv::catalog::catalog_row_key;
use mewrk_settings_conv::logic::reset_cache_break_warnings;
use mewrk_settings_conv::ops::{Done, Handlers, McpProbeReport, SettingsPatch, TemplateSummary};
use mewrk_settings_conv::tools::tool_row_key;
use mewrk_settings_conv::{conversation_settings, CapabilityWorkspace, CatalogKind, ConversationSettingsProps, Mode, Page};
use mewrk_ui_common::popover_menu::{row_key, trigger_key};
use support::*;

const CONV: &str = "conv1";

/// `vi.fn()`s, and a conversation held the way the owner would.
#[derive(Clone)]
struct Fx {
    conv: Rc<RefCell<Conversation>>,
    calls: Rc<RefCell<Vec<String>>>,
    tools: Rc<Vec<ToolDescriptor>>,
    role_tools: Rc<Vec<ToolDescriptor>>,
    workspaces: Rc<RefCell<Option<Vec<CapabilityWorkspace>>>>,
    capability_error: Rc<RefCell<Option<String>>>,
    templates: Rc<RefCell<Vec<TemplateSummary>>>,
    fail_create: Rc<RefCell<Option<String>>>,
    fail_save_role: Rc<RefCell<Option<String>>>,
    template_store: Rc<RefCell<Vec<ContextBody>>>,
    deletes_fail: Rc<RefCell<bool>>,
    probe: Rc<RefCell<Option<Result<McpProbeReport, String>>>>,
    /// Whether the owner routes deletes at all.
    routes_delete: Rc<RefCell<bool>>,
}

type ContextBody = (String, Vec<model::ContextItem>);

fn fixture_tools() -> Vec<ToolDescriptor> {
    vec![
        tool("read", "读取", "filesystem", false),
        tool("write", "写入", "filesystem", true),
        tool("bash", "Bash", "shell", true),
        tool("agent_spawn", "子代理", "orchestration", false),
        tool("task_wait", "等任务", "orchestration", false),
        tool("skill", "技能工具", "orchestration", false),
        tool("read_global_memory", "读记忆", "memory", false),
        tool("web_search", "搜索", "web", false),
        tool("preview_click", "点击", "web", false),
        tool("preview_start", "启动预览", "web", false),
        tool("preview_stop", "停止预览", "web", false),
        tool("preview_list", "预览列表", "web", false),
    ]
}

impl Fx {
    fn new(settings: impl FnOnce(&mut ConversationSettings)) -> Self {
        let mut s: ConversationSettings = serde_json::from_value(json!({ "enabledTools": ["read"] })).unwrap();
        settings(&mut s);
        let conversation: Conversation = serde_json::from_value(json!({ "id": CONV, "title": "demo", "createdAt": "", "updatedAt": "", "settings": s, "contexts": [] })).unwrap();
        Self {
            conv: Rc::new(RefCell::new(conversation)),
            calls: Rc::default(),
            tools: Rc::new(fixture_tools()),
            role_tools: Rc::new(fixture_tools()),
            workspaces: Rc::new(RefCell::new(Some(Vec::new()))),
            capability_error: Rc::default(),
            templates: Rc::default(),
            fail_create: Rc::default(),
            fail_save_role: Rc::default(),
            template_store: Rc::default(),
            deletes_fail: Rc::default(),
            probe: Rc::default(),
            routes_delete: Rc::new(RefCell::new(true)),
        }
    }

    fn settings(&self) -> ConversationSettings {
        self.conv.borrow().settings.clone()
    }

    fn called(&self, what: &str) -> bool {
        self.calls.borrow().iter().any(|c| c == what)
    }

    fn call_count(&self, prefix: &str) -> usize {
        self.calls.borrow().iter().filter(|c| c.starts_with(prefix)).count()
    }

    fn handlers(&self) -> Handlers {
        let (c1, c2, log) = (self.conv.clone(), self.conv.clone(), self.calls.clone());
        let l = |prefix: &'static str, log: &Rc<RefCell<Vec<String>>>| -> Rc<dyn Fn(&mut App, String)> {
            let log = log.clone();
            Rc::new(move |_, v| log.borrow_mut().push(format!("{prefix}:{v}")))
        };
        let (l1, l2, l3, l4, l5, l6, l7, l8, l9) = (log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone(), log.clone());
        let (store1, store2) = (self.template_store.clone(), self.template_store.clone());
        let (fail_create, fail_role, deletes_fail, probe, c3, routes) = (self.fail_create.clone(), self.fail_save_role.clone(), self.deletes_fail.clone(), self.probe.clone(), self.conv.clone(), *self.routes_delete.borrow());
        let on_delete: Rc<dyn Fn(&mut App, CatalogKind, model::ResourceDescriptor, Box<dyn FnOnce(&mut App, bool)>)> = Rc::new(move |app, kind, r, done| {
            l6.borrow_mut().push(format!("delete:{}:{}", kind.wire(), r.id));
            done(app, !*deletes_fail.borrow());
        });
        Handlers {
            on_change: Rc::new(move |_, next| c1.borrow_mut().settings = next),
            on_change_conversation_only: Rc::new(move |_, patch: SettingsPatch| {
                patch(&mut c2.borrow_mut().settings);
                l1.borrow_mut().push("conversation-only".into());
            }),
            on_apply_preset: l("apply", &log),
            on_rename_preset: Some(Rc::new(move |_, id, name| l2.borrow_mut().push(format!("rename:{id}:{name}")))),
            on_delete_preset: Some(l("delete-preset", &log)),
            on_save_preset: Some(Rc::new(move |_, id, body| l3.borrow_mut().push(format!("save-preset:{id}:{}", body.enabled_tools.join(","))))),
            on_save_preset_copy: Some(Rc::new(move |_, id, _, body| l4.borrow_mut().push(format!("save-copy:{id}:{}", body.map_or(-1, |b| b.len() as i64))))),
            on_bind_preset_template: Some(Rc::new(move |_, id, template| l5.borrow_mut().push(format!("bind:{id}:{template}")))),
            on_read_template: Rc::new(move |app, id, done: Done<Vec<model::ContextItem>>| {
                let body = store1.borrow().iter().find(|(k, _)| *k == id).map(|(_, b)| b.clone()).unwrap_or_default();
                done(app, Ok(body));
            }),
            on_write_template: Rc::new(move |app, id, body, done: Done<String>| {
                let id = if id.is_empty() { "template_minted".to_owned() } else { id };
                let mut s = store2.borrow_mut();
                s.retain(|(k, _)| *k != id);
                s.push((id.clone(), body));
                drop(s);
                done(app, Ok(id));
            }),
            on_delete_capability: routes.then_some(on_delete),
            on_save_agent_role: Some(Rc::new(move |app, target, role, done: Done<String>| {
                l7.borrow_mut().push(format!("save-role:{}:{}:{}", target.id.clone().unwrap_or_default(), target.workspace_key.clone().unwrap_or_default(), role.name));
                match fail_role.borrow().clone() {
                    Some(reason) => done(app, Err(reason)),
                    None => done(app, Ok(target.id.unwrap_or_else(|| format!("agent_{}", role.name)))),
                }
            })),
            on_rescan: Some(Rc::new(move |_| l8.borrow_mut().push("rescan".into()))),
            on_fingerprint: None,
            on_reveal: Some(Rc::new(move |_, kind, key| l9.borrow_mut().push(format!("reveal:{}:{}", kind.wire(), key.unwrap_or_default())))),
            on_probe: Some(Rc::new(move |app, _, done| {
                let answer = probe.borrow().clone().unwrap_or_else(|| Err("no answer".into()));
                done(app, answer);
            })),
            on_create_preset: Some(Rc::new(move |app, name, capture, done: Done<model::ConversationPreset>| {
                if let Some(reason) = fail_create.borrow().clone() {
                    done(app, Err(reason));
                    return;
                }
                let settings = mewrk_client::presets::capture_conversation_preset_settings(&c3.borrow().settings);
                let preset = model::ConversationPreset { id: format!("preset_{name}"), name, description: String::new(), template_id: if capture { "template_captured".into() } else { String::new() }, settings };
                app.doc.presets.conversation_presets.push(preset.clone());
                done(app, Ok(preset));
            })),
            on_read_capability: None,
            on_save_capability: None,
            on_save_as_preset: None,
        }
    }

    fn props(&self, initial: Page) -> ConversationSettingsProps {
        ConversationSettingsProps {
            conversation: self.conv.borrow().clone(),
            tools: self.tools.clone(),
            role_tools: self.role_tools.clone(),
            handlers: self.handlers(),
            templates: self.templates.borrow().clone(),
            capability_error: self.capability_error.borrow().clone(),
            preset_error: None,
            workspaces: self.workspaces.borrow().clone(),
            mode: Mode::Conversation,
            preset_id: None,
            initial_view: initial,
            window_title: None,
        }
    }

    fn view(&self, initial: Page) -> impl Fn(&mut App) -> E + use<> {
        let fx = self.clone();
        move |app: &mut App| conversation_settings(app, fx.props(initial))
    }
}

fn rig_rect(key: impl std::hash::Hash) -> fluent::Rect {
    fluent::el::rect_of(key).expect("drawn")
}

fn start(tag: &str) -> (Rig, App) {
    let (rig, mut app) = Rig::new(tag);
    empty_catalog(&mut app);
    reset_cache_break_warnings(&mut app);
    (rig, app)
}

fn global_list(page: &str) -> String {
    format!("catalog:{CONV}:{page}:global")
}

fn row_of(page: &str, id: &str) -> u64 {
    catalog_row_key(&global_list(page), id)
}

// ---------------------------------------------------------------- the frame

#[test]
fn lists_its_pages_and_opens_on_the_tools_page() {
    let (mut rig, mut app) = start("pages");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Tools);
    rig.frame(&mut app, &view);
    for page in ["工具", "高级工具", "技能", "MCP", "钩子", "工具描述", "代理角色", "对话预设"] {
        assert!(rig.shows(page), "{page} missing from {:?}", rig.said());
    }
    // A template belongs to whatever opens with it; a live conversation opens with itself.
    assert!(!rig.shows("对话模板"));
    // There is no sandbox page: the sandbox belongs to each workspace.
    assert!(!rig.shows("沙箱"));
    assert!(rig.shows("另存为预设"));
    assert!(rig.shows("本对话交给模型的工具。"));
}

#[test]
fn counts_only_catalog_tools_and_never_shows_host_derived_ones() {
    let (mut rig, mut app) = start("counts");
    let fx = Fx::new(|s| s.enabled_tools = vec!["read".into(), "gone_tool".into(), "web_search".into(), "preview_start".into()]);
    let view = fx.view(Page::Tools);
    rig.frame(&mut app, &view);
    // Only `read` is a row: web search follows its switch, the lifecycle tools follow the other preview tools, and a vanished one counts for nothing.
    assert!(rig.shows("工具 1"), "{:?}", rig.said());
    for hidden in ["搜索已关闭", "技能工具已关闭", "读记忆已关闭", "等任务已关闭", "启动预览已关闭", "停止预览已关闭"] {
        assert!(!rig.shows(hidden), "{hidden}");
    }
    assert!(rig.shows("点击已关闭"));
}

#[test]
fn words_the_panes_one_line_descriptions_in_english() {
    let (mut rig, mut app) = start("english");
    mewrk_client::i18n::set_english(true);
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("Capabilities set by switches, outside the tool list."));
    assert!(rig.shows("Advanced tools"));
    assert!(rig.shows("Save as preset"));
    assert!(rig.shows("Enable file write guards"));
    mewrk_client::i18n::set_english(false);
}

// ---------------------------------------------------------------- the switches on the advanced page

#[test]
fn switches_the_file_write_guards_on_until_told_otherwise() {
    let (mut rig, mut app) = start("guards");
    let fx = Fx::new(|_| {});
    assert!(fx.settings().file_write_guards_enabled);
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("文件防误写保护已开启"));
    rig.click(&mut app, &view, ("switch", "conv1:file-write-guards"));
    assert!(!fx.settings().file_write_guards_enabled);
    assert!(rig.shows("文件防误写保护已关闭"));
}

#[test]
fn switches_the_environment_section_and_subagent_communication() {
    let (mut rig, mut app) = start("env");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("环境信息已拼接"));
    assert!(rig.shows("子代理通信已关闭"));
    rig.click(&mut app, &view, ("switch", "conv1:environment-prompt"));
    rig.click(&mut app, &view, ("switch", "conv1:subagent-messaging"));
    assert!(!fx.settings().environment_prompt_enabled);
    assert!(fx.settings().subagent_messaging_enabled);
    assert!(rig.shows("子代理通信已开启"));
}

#[test]
fn chooses_what_host_messages_come_in_each_choice_saying_what_it_costs() {
    let (mut rig, mut app) = start("host-messages");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("Claude Code 的做法；可能被当成你说的话。"));
    assert!(rig.shows("作为 box 调用的结果；模型可能当成自己调的。"));
    let (user, boxed) = (rig_rect(("host-message", "conv1", "user")), rig_rect(("host-message", "conv1", "box")));
    assert!((user.y - boxed.y).abs() < 0.5 && user.right() <= boxed.x, "side by side: {user:?} {boxed:?}");
    rig.click(&mut app, &view, ("host-message", "conv1", "box"));
    assert_eq!(fx.settings().host_message_container, HostMessageContainer::Box);
}

fn warm(app: &mut App, appends: bool) -> ConversationToolLock {
    let caps: &[&str] = if appends { &["tool_append"] } else { &[] };
    select_model(app, provider("p1", "Provider", "anthropic", &[("m1", caps)]));
    warm_lock("p1", "m1")
}

#[test]
fn draws_a_guard_orange_while_warm_and_asks_before_either_way_of_switching_it() {
    let (mut rig, mut app) = start("guards-warm");
    let lock = ConversationToolLock { file_write_guards: Some(true), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| s.tool_lock = Some(lock));
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("cache-orange"), "{:?}", rig.said());
    assert!(!rig.said().iter().any(|line| line == "缓存还热：改动这一项会让它失效。"), "no note of its own under the row: only the switch's colour says so");
    assert!(rig.said().iter().any(|line| line.ends_with("缓存还热：改动这一项会让它失效。")), "what it means is on hover");
    rig.click(&mut app, &view, ("switch", "conv1:file-write-guards"));
    // It asked instead of moving.
    assert!(rig.shows("这样改会让缓存失效"));
    assert!(fx.settings().file_write_guards_enabled);
    rig.click(&mut app, &view, "cache-break-cancel");
    assert!(!rig.shows("这样改会让缓存失效"));
    assert!(fx.settings().file_write_guards_enabled);
    rig.click(&mut app, &view, ("switch", "conv1:file-write-guards"));
    rig.click(&mut app, &view, "cache-break-confirm");
    assert!(!fx.settings().file_write_guards_enabled);
    // Moved away from what the last request had, the row is plain: the cache is already lost for it. Moving back asks nothing: said once per conversation.
    rig.click(&mut app, &view, ("switch", "conv1:file-write-guards"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert!(fx.settings().file_write_guards_enabled);
}

#[test]
fn draws_subagent_communication_orange_only_with_agent_spawn_in_both() {
    let (mut rig, mut app) = start("messaging-warm");
    let base = warm(&mut app, true);
    let lock_without = ConversationToolLock { subagent_messaging: Some(false), ..base.clone() };
    let fx = Fx::new(move |s| {
        s.tool_lock = Some(lock_without);
        s.enabled_tools = vec!["read".into()];
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:subagent-messaging"));
    assert!(!rig.shows("这样改会让缓存失效"), "without agent_spawn the switch changes nothing on the wire");
    assert!(fx.settings().subagent_messaging_enabled);

    let lock_with = ConversationToolLock { subagent_messaging: Some(false), tools: vec!["agent_spawn".into()], ..base };
    let fx = Fx::new(move |s| {
        s.tool_lock = Some(lock_with);
        s.enabled_tools = vec!["agent_spawn".into()];
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:subagent-messaging"));
    assert!(rig.shows("这样改会让缓存失效"));
}

#[test]
fn draws_the_host_message_container_orange_and_asks_before_it_switches() {
    let (mut rig, mut app) = start("container-warm");
    let lock = ConversationToolLock { host_message_container: Some(HostMessageContainer::User), ..warm(&mut app, false) };
    let fx = Fx::new(move |s| s.tool_lock = Some(lock));
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("host-message", "conv1", "box"));
    assert!(rig.shows("这样改会让缓存失效"));
    assert_eq!(fx.settings().host_message_container, HostMessageContainer::User);
    rig.click(&mut app, &view, "cache-break-confirm");
    assert_eq!(fx.settings().host_message_container, HostMessageContainer::Box);
}

#[test]
fn stops_warning_anywhere_once_told_not_to_show_it_again() {
    let (mut rig, mut app) = start("never");
    let lock = ConversationToolLock { global_memory: true, ..warm(&mut app, true) };
    let lock2 = lock.clone();
    let fx = Fx::new(move |s| {
        s.global_memory_enabled = true;
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:global-memory"));
    assert!(rig.shows("这样改会让缓存失效"));
    rig.click(&mut app, &view, "cache-break-never");
    assert!(rig.shows("checkbox:on"));
    rig.click(&mut app, &view, "cache-break-confirm");
    assert!(!fx.settings().global_memory_enabled);
    // Another conversation, the same warm lock: nothing asks.
    let other = Fx::new(move |s| {
        s.global_memory_enabled = true;
        s.tool_lock = Some(lock2);
    });
    other.conv.borrow_mut().id = "conv2".into();
    let view2 = other.view(Page::Advanced);
    rig.click(&mut app, &view2, ("switch", "conv2:global-memory"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert!(!other.settings().global_memory_enabled);
}

#[test]
fn drops_the_orange_once_the_cache_has_gone_cold_or_for_another_model() {
    let (mut rig, mut app) = start("cold");
    let cold = ConversationToolLock { global_memory: true, last_request: Some(model::ToolLockRequest { provider_id: "p1".into(), model_id: "m1".into(), at: "2020-01-01T00:00:00.000Z".into() }), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.global_memory_enabled = true;
        s.tool_lock = Some(cold);
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("cache-orange"));
    rig.click(&mut app, &view, ("switch", "conv1:global-memory"));
    assert!(!fx.settings().global_memory_enabled, "a cold cache has nothing to warn about");

    let warm_elsewhere = ConversationToolLock { global_memory: true, ..warm_lock("other", "m9") };
    let fx = Fx::new(move |s| {
        s.global_memory_enabled = true;
        s.tool_lock = Some(warm_elsewhere);
    });
    fx.conv.borrow_mut().id = "conv3".into();
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("cache-orange"), "the selected model is not the one that holds the cache");
}

// ---------------------------------------------------------------- web

#[test]
fn edits_search_provider_behavior_as_a_conversation_only_patch() {
    let (mut rig, mut app) = start("web-patch");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "tavily", "enabled": true }] })).unwrap();
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, trigger_key(("web-backend-menu", "conv1:search-provider")));
    rig.click(&mut app, &view, row_key(("web-backend-menu", "conv1:search-provider"), "tavily"));
    assert!(fx.called("conversation-only"));
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily });
}

#[test]
fn opens_the_native_rows_into_the_messages_tool_versions_on_a_messages_model() {
    let (mut rig, mut app) = start("web-versions");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("web_search_20250305"), "the menu names the version actually sent: {:?}", rig.said());
    rig.click(&mut app, &view, trigger_key(("web-backend-menu", "conv1:search-provider")));
    rig.click(&mut app, &view, row_key(("web-backend-menu", "conv1:search-provider"), "native"));
    rig.click(&mut app, &view, row_key(("web-backend-menu", "conv1:search-provider"), "web_search_20260209"));
    assert_eq!(fx.settings().web_search.native_search_tool, model::NativeSearchTool::WebSearch20260209);
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Native);
}

#[test]
fn falls_back_to_plain_native_off_messages_and_keeps_the_chosen_versions() {
    let (mut rig, mut app) = start("web-plain");
    select_model(&mut app, provider("p1", "Provider", "openai_responses", &[("m1", &[])]));
    let fx = Fx::new(|s| {
        s.web_search_enabled = true;
        s.web_search.native_search_tool = model::NativeSearchTool::WebSearch20260209;
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("web_search_20260209"));
    rig.click(&mut app, &view, trigger_key(("web-backend-menu", "conv1:search-provider")));
    rig.click(&mut app, &view, row_key(("web-backend-menu", "conv1:search-provider"), "native"));
    assert_eq!(fx.settings().web_search.native_search_tool, model::NativeSearchTool::WebSearch20260209);
}

#[test]
fn pins_both_native_backends_once_they_have_been_used() {
    let (mut rig, mut app) = start("web-pin");
    let lock = ConversationToolLock { search_provider: Some(SearchProviderSelection::Native), fetch_provider: Some(FetchProviderSelection::Native), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.web_search_enabled = true;
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows_containing("本对话已经用原生后端跑过了"), "{:?}", rig.said());
    // A settled switch is held with the selector under it.
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    assert!(fx.settings().web_search_enabled);
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Native);
    rig.click(&mut app, &view, trigger_key(("web-backend-menu", "conv1:fetch-provider")));
    assert!(!rig.exists(row_key(("web-backend-menu", "conv1:fetch-provider"), "native")));
}

#[test]
fn draws_a_host_run_backend_orange_while_warm_and_warns_once_before_it_moves() {
    let (mut rig, mut app) = start("web-cache");
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "tavily", "enabled": true }, { "kind": "exa", "enabled": true }] })).unwrap();
    let tavily = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
    let lock = ConversationToolLock { web_search: true, search_backend: Some(tavily.clone()), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.web_search_enabled = true;
        s.web_search.provider = tavily;
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, trigger_key(("web-backend-menu", "conv1:search-provider")));
    rig.click(&mut app, &view, row_key(("web-backend-menu", "conv1:search-provider"), "exa"));
    assert!(rig.shows("这样改会让缓存失效"));
    rig.click(&mut app, &view, "cache-break-confirm");
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Exa });
    assert!(fx.called("conversation-only"), "the web backends are written the conversation-only way even after the warning");
}

#[test]
fn flips_web_access_with_the_legs_in_one_conversation_only_patch() {
    let (mut rig, mut app) = start("web-flip");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    let fx = Fx::new(|s| s.web_search_enabled = false);
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    let s = fx.settings();
    assert!(s.web_search_enabled);
    assert_eq!(s.web_search.provider, SearchProviderSelection::Native);
    assert_eq!(s.web_search.fetch_provider, FetchProviderSelection::Disabled, "the other leg must not quietly come back with it");
    assert_eq!(fx.call_count("conversation-only"), 1);
    // Off with the last leg that names a backend, leaving that backend in place.
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    assert!(!fx.settings().web_search_enabled);
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Native);
}

#[test]
fn reads_a_native_fetch_as_off_on_a_model_without_one_without_rewriting_it() {
    let (mut rig, mut app) = start("web-fetch-off");
    select_model(&mut app, provider("p1", "Provider", "openai_responses", &[("m1", &[])]));
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("网页抓取已关闭"), "{:?}", rig.said());
    assert_eq!(fx.settings().web_search.fetch_provider, FetchProviderSelection::Native);
    select_model(&mut app, provider("p2", "Claude", "anthropic", &[("m2", &[])]));
    rig.frame(&mut app, &view);
    assert!(rig.shows("网页抓取已开启"));
}

#[test]
fn turns_fetch_on_with_a_catalog_provider_on_a_model_without_a_native_fetch() {
    let (mut rig, mut app) = start("web-fetch-catalog");
    select_model(&mut app, provider("p1", "Provider", "openai_responses", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "jina", "enabled": true }] })).unwrap();
    let fx = Fx::new(|s| {
        s.web_search_enabled = true;
        s.web_search.fetch_provider = FetchProviderSelection::Disabled;
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:web-fetch"));
    assert_eq!(fx.settings().web_search.fetch_provider, FetchProviderSelection::Explicit { provider_kind: SearchProviderKind::Jina });
}

#[test]
fn asks_for_a_repair_when_the_chosen_provider_is_off_or_unknown() {
    let (mut rig, mut app) = start("web-repair");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    let fx = Fx::new(|s| {
        s.web_search_enabled = true;
        s.web_search.provider = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
        s.web_search.fetch_provider = FetchProviderSelection::Unavailable;
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("请修复搜索提供商"));
    assert!(rig.shows("请修复抓取提供商"));
}

#[test]
fn shows_a_stored_count_through_the_backends_ceiling_and_clamps_what_is_typed() {
    let (mut rig, mut app) = start("web-numbers");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "tavily", "enabled": true }] })).unwrap();
    let fx = Fx::new(|s| {
        s.web_search_enabled = true;
        s.web_search.provider = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
        s.web_search.max_results = 30;
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("20"), "shown through the ceiling: {:?}", rig.said());
    assert!(!rig.shows("搜索结果压缩"), "Tavily has no content-length parameter");
    assert_eq!(fx.settings().web_search.max_results, 30, "and not rewritten");
    rig.type_into(&mut app, &view, ("number", "conv1:max-results"), "7");
    // The field held "20", so typing appends: 207 clamps to the ceiling.
    assert_eq!(fx.settings().web_search.max_results, 20);
}

#[test]
fn shows_jinas_floor_on_a_smaller_stored_compression_and_never_lifts_zero() {
    let (mut rig, mut app) = start("web-floor");
    select_model(&mut app, provider("p1", "Provider", "openai_responses", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "jina", "enabled": true }] })).unwrap();
    let jina = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Jina };
    let fx = Fx::new(move |s| {
        s.web_search_enabled = true;
        s.web_search.provider = jina;
        s.web_search.compression_cutoff = 100;
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(rig.shows("500"), "{:?}", rig.said());
    assert!(rig.shows("搜索结果压缩"));
    fx.conv.borrow_mut().settings.web_search.compression_cutoff = 0;
    rig.frame(&mut app, &view);
    assert!(rig.shows("0"));
}

#[test]
fn draws_the_domain_filter_only_while_either_leg_is_on_and_edits_each_list() {
    let (mut rig, mut app) = start("web-domains");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    let fx = Fx::new(|s| s.web_search_enabled = false);
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("域名过滤"));
    fx.conv.borrow_mut().settings.web_search_enabled = true;
    rig.frame(&mut app, &view);
    assert!(rig.shows("域名过滤"));
    rig.click(&mut app, &view, ("domain-edit", "conv1"));
    assert!(rig.shows("域名名单"));
    assert!(rig.shows("黑名单 0"));
    assert!(rig.shows("过滤 0 / 允许 0"));
    rig.type_into(&mut app, &view, ("domain-rules", "conv1", mewrk_settings_conv::advanced::DomainList::Exclude), "*://ads.example/*\n\n  *://b.example/*  ");
    assert_eq!(fx.settings().web_search.exclude_domains, vec!["*://ads.example/*", "*://b.example/*"]);
    assert!(fx.settings().web_search.include_domains.is_empty());
    assert_eq!(fx.settings().web_search.domain_filter, model::SearchDomainFilterMode::Off, "writing a list chooses nothing");
    rig.click(&mut app, &view, ("nav", "conv1:domain-rules", mewrk_settings_conv::advanced::DomainList::Include));
    rig.type_into(&mut app, &view, ("domain-rules", "conv1", mewrk_settings_conv::advanced::DomainList::Include), "/x/");
    assert_eq!(fx.settings().web_search.include_domains, vec!["/x/"]);
    rig.frame(&mut app, &view);
    assert!(rig.shows("过滤 2 / 允许 1"), "the row says how long each list is: {:?}", rig.said());
    let (counts, edit) = (rig_rect(("domain-counts", "conv1")), rig_rect(("domain-edit", "conv1")));
    assert!(counts.right() <= edit.x, "under the title, not among the controls: {counts:?} {edit:?}");
}

#[test]
fn picks_the_domain_filter_with_three_buttons_side_by_side() {
    let (mut rig, mut app) = start("web-domain-mode");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("domain-filter", "conv1", "include"));
    assert_eq!(fx.settings().web_search.domain_filter, model::SearchDomainFilterMode::Include);
    rig.click(&mut app, &view, ("domain-filter", "conv1", "exclude"));
    assert_eq!(fx.settings().web_search.domain_filter, model::SearchDomainFilterMode::Exclude);
    rig.click(&mut app, &view, ("domain-filter", "conv1", "off"));
    assert_eq!(fx.settings().web_search.domain_filter, model::SearchDomainFilterMode::Off);
    let (edit, count_row, exclude, off) = (rig_rect(("domain-edit", "conv1")), rig_rect(("domain-row", "conv1")), rig_rect(("domain-filter", "conv1", "exclude")), rig_rect(("domain-filter", "conv1", "off")));
    assert!(edit.right() <= exclude.x, "the way in to the lists sits to the left of the buttons: {edit:?} {exclude:?}");
    assert!((exclude.y - off.y).abs() < 0.5, "side by side");
    assert!(off.right() <= count_row.right());
}

#[test]
fn names_a_legs_backend_beside_its_switch_and_goes_dark_with_it() {
    let (mut rig, mut app) = start("web-word");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "tavily", "enabled": true }] })).unwrap();
    let tavily = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
    let chosen = tavily.clone();
    let fx = Fx::new(move |s| {
        s.web_search_enabled = true;
        s.web_search.provider = chosen;
    });
    let view = fx.view(Page::Advanced);
    let menu = ("web-backend-menu", "conv1:search-provider");
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert!(rig_rect(trigger_key(menu)).right() <= rig_rect(("switch", "conv1:web-search")).x, "the word is to the left of the switch");
    assert!(!rig.shows("搜索提供商"), "no second layer under the switch");
    // Off, it still names what would come back, and opens nothing.
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    assert_eq!(fx.settings().web_search.provider, SearchProviderSelection::Disabled);
    rig.frame(&mut app, &view);
    assert!(rig.shows("Tavily"), "{:?}", rig.said());
    rig.click(&mut app, &view, trigger_key(menu));
    assert!(!rig.exists(row_key(menu, "tavily")));
    // On again, the same word opens its menu.
    rig.click(&mut app, &view, ("switch", "conv1:web-search"));
    assert_eq!(fx.settings().web_search.provider, tavily);
    rig.click(&mut app, &view, trigger_key(menu));
    assert!(rig.exists(row_key(menu, "tavily")));
}

#[test]
fn spaces_every_item_of_the_advanced_page_alike() {
    let (mut rig, mut app) = start("advanced-gaps");
    select_model(&mut app, provider("p1", "Provider", "anthropic", &[("m1", &[])]));
    app.doc.assets.web_search = serde_json::from_value(json!({ "providers": [{ "kind": "jina", "enabled": true }] })).unwrap();
    let fx = Fx::new(|s| {
        s.web_search_enabled = true;
        s.web_search.provider = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Jina };
    });
    let view = fx.view(Page::Advanced);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let rows: Vec<fluent::Rect> = vec![
        rig_rect(("switch-row", "conv1:web-search")),
        rig_rect(("number-row", "conv1:max-results")),
        rig_rect(("number-row", "conv1:search-compression")),
        rig_rect(("switch-row", "conv1:web-fetch")),
        rig_rect(("number-row", "conv1:fetch-compression")),
        rig_rect(("domain-row", "conv1")),
        rig_rect(("switch-row", "conv1:global-memory")),
        rig_rect(("switch-row", "conv1:project-memory")),
        rig_rect(("switch-row", "conv1:file-write-guards")),
        rig_rect(("switch-row", "conv1:subagent-messaging")),
        rig_rect(("switch-row", "conv1:environment-prompt")),
        rig_rect(("host-message-row", "conv1")),
    ];
    for pair in rows.windows(2) {
        assert!((pair[1].y - pair[0].bottom() - mewrk_settings_conv::widgets::SETTING_ROW_GAP).abs() < 0.5, "{:?} {:?}", pair[0], pair[1]);
    }
}

// ---------------------------------------------------------------- tool descriptions

fn with_profiles(app: &mut App) {
    app.doc.capabilities.tool_description_files = vec![builtin("tooldesc_builtin_concise_en_us", "Mewrk concise"), builtin("tooldesc_builtin_en_us", "Mewrk guided"), resource("file_a", "Mine A", None), resource("file_b", "Mine B", None)];
}

#[test]
fn picks_one_tool_description_file_at_a_time_and_lists_no_row_for_the_builtin() {
    let (mut rig, mut app) = start("profiles");
    with_profiles(&mut app);
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::ToolDescriptions);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("Mewrk concise"), "selecting none already is it");
    assert!(rig.shows("选一份；不选则用内置的 Mewrk concise。"));
    rig.click(&mut app, &view, row_of("toolDescriptions", "file_a"));
    assert_eq!(fx.settings().tool_description_file_id.as_deref(), Some("file_a"));
    rig.click(&mut app, &view, row_of("toolDescriptions", "file_b"));
    assert_eq!(fx.settings().tool_description_file_id.as_deref(), Some("file_b"));
    rig.click(&mut app, &view, row_of("toolDescriptions", "file_b"));
    assert_eq!(fx.settings().tool_description_file_id, None);
    assert!(rig.shows("工具描述 0"));
}

#[test]
fn reads_a_conversation_naming_the_builtin_or_the_retired_one_as_having_picked_nothing() {
    let (mut rig, mut app) = start("profiles-retired");
    with_profiles(&mut app);
    for id in ["tooldesc_builtin_concise_en_us", "tooldesc_builtin_en_us"] {
        let fx = Fx::new(|s| s.tool_description_file_id = Some(id.to_owned()));
        let view = fx.view(Page::ToolDescriptions);
        rig.frame(&mut app, &view);
        assert!(rig.shows("工具描述 0"));
        assert!(!rig.shows("悬空"));
    }
}

#[test]
fn draws_every_tool_description_row_orange_while_warm_and_asks_before_changing_it() {
    let (mut rig, mut app) = start("profiles-warm");
    with_profiles(&mut app);
    let lock = ConversationToolLock { prompt_profile: Some("tooldesc_builtin_concise_en_us".into()), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| s.tool_lock = Some(lock));
    let view = fx.view(Page::ToolDescriptions);
    rig.click(&mut app, &view, row_of("toolDescriptions", "file_a"));
    assert!(rig.shows("这样改会让缓存失效"));
    assert_eq!(fx.settings().tool_description_file_id, None);
}

#[test]
fn keeps_a_tool_description_id_the_catalog_lost_as_a_dangling_row_that_unticks_without_asking() {
    let (mut rig, mut app) = start("profiles-dangling");
    with_profiles(&mut app);
    let lock = ConversationToolLock { prompt_profile: Some("file_gone".into()), ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.tool_description_file_id = Some("file_gone".into());
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::ToolDescriptions);
    rig.frame(&mut app, &view);
    assert!(rig.shows("悬空"));
    assert!(rig.shows_containing("运行时用的是内置的 Mewrk concise"));
    rig.click(&mut app, &view, catalog_row_key("", "file_gone"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert_eq!(fx.settings().tool_description_file_id, None);
}

#[test]
fn says_a_selected_tool_description_file_with_nothing_usable_falls_back_to_the_builtin() {
    let (mut rig, mut app) = start("profiles-unusable");
    let mut broken = resource("file_x", "Broken", None);
    broken.available = false;
    app.doc.capabilities.tool_description_files = vec![broken];
    let fx = Fx::new(|s| s.tool_description_file_id = Some("file_x".into()));
    let view = fx.view(Page::ToolDescriptions);
    rig.frame(&mut app, &view);
    assert!(rig.shows_containing("已选择，但文件里没有可用条目"), "{:?}", rig.said());
    assert!(rig.shows("不可用"));
}

#[test]
fn draws_only_the_global_section_on_the_tool_descriptions_page_and_opens_its_folder() {
    let (mut rig, mut app) = start("profiles-global");
    with_profiles(&mut app);
    let fx = Fx::new(|_| {});
    *fx.workspaces.borrow_mut() = Some(vec![CapabilityWorkspace { number: 1, key: "local|/w/a".into(), machine: None, path: "/w/a".into() }]);
    let view = fx.view(Page::ToolDescriptions);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("/w/a"));
    rig.click(&mut app, &view, ("reveal", "catalog:conv1:toolDescriptions", "global"));
    assert!(fx.called("reveal:toolDescriptions:"));
}

// ---------------------------------------------------------------- capability pages

fn skills_catalog(app: &mut App) {
    app.doc.capabilities.skills = vec![resource("skill_a", "Alpha", None), resource("skill_b", "Beta", Some("local|/w/a")), resource("skill_other", "Other", Some("local|/w/other"))];
}

fn workspace_a() -> Vec<CapabilityWorkspace> {
    vec![CapabilityWorkspace { number: 1, key: "local|/w/a".into(), machine: None, path: "/w/a".into() }]
}

#[test]
fn divides_a_capability_page_into_the_global_level_and_each_workspace() {
    let (mut rig, mut app) = start("sections");
    skills_catalog(&mut app);
    let fx = Fx::new(|s| s.skill_ids = vec!["skill_a".into()]);
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("全局"));
    assert!(rig.shows("~/.mewrk"));
    assert!(rig.shows("/w/a"));
    assert!(rig.shows("Alpha"));
    assert!(rig.shows("Beta"));
    assert!(!rig.shows("Other"), "a workspace this conversation does not have is not its to pick");
    assert!(rig.shows("1 / 2 个已选"), "{:?}", rig.said());
    assert!(rig.shows("1 / 1"), "the global section's own count");
    assert!(rig.shows("0 / 1"));
}

#[test]
fn offers_a_draft_with_no_project_the_global_level_alone() {
    let (mut rig, mut app) = start("sections-draft");
    skills_catalog(&mut app);
    let fx = Fx::new(|s| s.skill_ids = vec!["skill_b".into()]);
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("Alpha"));
    assert!(!rig.shows("Beta"));
    assert!(rig.shows("悬空"), "selected in a workspace this draft does not have");
}

#[test]
fn searches_a_catalog_by_name_description_and_path() {
    let (mut rig, mut app) = start("search");
    skills_catalog(&mut app);
    let fx = Fx::new(|_| {});
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view(Page::Skills);
    rig.type_into(&mut app, &view, ("capability-search", "catalog:conv1:skills"), "somewhere/beta");
    assert!(rig.shows("Beta"));
    assert!(!rig.shows("Alpha"));
    assert!(!rig.shows("全局"), "a level with no match is not drawn");
    // Toggling what the search shows works, and no drag-sort is on while it does.
    rig.click(&mut app, &view, catalog_row_key(&format!("catalog:{CONV}:skills:local|/w/a"), "skill_b"));
    assert_eq!(fx.settings().skill_ids, vec!["skill_b"]);
    let fx2 = Fx::new(|_| {});
    fx2.conv.borrow_mut().id = "conv-empty".into();
    let view2 = fx2.view(Page::Skills);
    rig.type_into(&mut app, &view2, ("capability-search", "catalog:conv-empty:skills"), "nothing-like-this");
    assert!(rig.shows("没有匹配的条目"));
}

#[test]
fn keeps_a_selected_capability_that_has_vanished_from_the_catalog() {
    let (mut rig, mut app) = start("dangling");
    let fx = Fx::new(|s| {
        s.skill_ids = vec!["gone".into()];
        s.hook_ids = vec!["gone_hook".into()];
    });
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("gone"));
    assert!(rig.shows("悬空"));
    assert!(rig.shows_containing("目录中已不存在；取消勾选之前，每次运行都会失败"));
    assert!(rig.shows("0 / 0 个已选"), "counted over the population it divides by");
    rig.click(&mut app, &view, ("nav", CONV, Page::Hooks));
    assert!(rig.shows_containing("目录中已不存在"), "a dangling hook fails every run too");
}

#[test]
fn unticks_a_dangling_mcp_row_on_a_model_that_cannot_append_from_the_lock_as_well_without_asking() {
    let (mut rig, mut app) = start("dangling-lock");
    let lock = ConversationToolLock { mcp_ids: vec!["gone".into(), "kept".into()], ..warm(&mut app, false) };
    let fx = Fx::new(move |s| {
        s.mcp_ids = vec!["gone".into()];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Mcp);
    rig.click(&mut app, &view, catalog_row_key("", "gone"));
    assert!(!rig.shows("这样改会让缓存失效"));
    let s = fx.settings();
    assert!(s.mcp_ids.is_empty());
    assert_eq!(s.tool_lock.unwrap().mcp_ids, vec!["kept"], "or the next restore of a warm surface would put it back");
}

#[test]
fn draws_a_skill_the_warm_cache_holds_orange_and_warns_once_before_it_goes() {
    let (mut rig, mut app) = start("skill-warm");
    app.doc.capabilities.skills = vec![resource("skill_a", "Alpha", None), resource("skill_b", "Beta", None)];
    let lock = ConversationToolLock { skill_ids: vec!["skill_a".into()], ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.skill_ids = vec!["skill_a".into()];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("cache-orange"));
    // An entry the last request carried is not deleted out from under it.
    assert!(!rig.shows("删除 Alpha"));
    assert!(rig.shows("删除 Beta"));
    // Adding a skill arrives as a host notice, so it costs nothing and asks nothing.
    rig.click(&mut app, &view, row_of("skills", "skill_b"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert_eq!(fx.settings().skill_ids, vec!["skill_a", "skill_b"]);
    rig.click(&mut app, &view, row_of("skills", "skill_a"));
    assert!(rig.shows("这样改会让缓存失效"));
    rig.click(&mut app, &view, "cache-break-confirm");
    assert_eq!(fx.settings().skill_ids, vec!["skill_b"]);
}

#[test]
fn offers_the_skill_delivery_switch_below_the_list_whether_or_not_a_skill_is_selected() {
    let (mut rig, mut app) = start("skill-delivery");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("尚未发现技能"), "{:?}", rig.said());
    assert!(rig.shows("技能按需加载"));
    assert!(rig.shows("拼进提示词"));
    rig.click(&mut app, &view, ("switch", "conv1:skill-tool"));
    assert!(fx.settings().skill_tool_enabled);
    assert!(rig.shows("按需加载"));
}

#[test]
fn does_not_offer_tool_discovery_on_a_model_that_cannot_take_a_tool_mid_conversation() {
    let (mut rig, mut app) = start("discovery");
    select_model(&mut app, provider("p1", "Provider", "openai_chat", &[("m1", &[])]));
    let fx = Fx::new(|s| s.mcp_tool_discovery_enabled = true);
    let view = fx.view(Page::Mcp);
    rig.frame(&mut app, &view);
    assert!(rig.shows_containing("当前模型不支持中途追加工具"), "{:?}", rig.said());
    rig.click(&mut app, &view, ("switch", "conv1:tool-discovery"));
    assert!(fx.settings().mcp_tool_discovery_enabled, "held, and not rewritten");
}

#[test]
fn offers_tool_discovery_and_draws_it_orange_once_a_request_went_out_with_it() {
    let (mut rig, mut app) = start("discovery-warm");
    let lock = ConversationToolLock { mcp_tool_discovery: true, ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.mcp_tool_discovery_enabled = true;
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Mcp);
    rig.click(&mut app, &view, ("switch", "conv1:tool-discovery"));
    assert!(rig.shows("这样改会让缓存失效"));
}

#[test]
fn draws_the_whole_tool_surface_orange_on_a_model_that_cannot_append_and_leaves_skills_free() {
    let (mut rig, mut app) = start("whole-surface");
    app.doc.capabilities.skills = vec![resource("skill_a", "Alpha", None)];
    let lock = warm(&mut app, false);
    let fx = Fx::new(move |s| {
        s.enabled_tools = vec![];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Tools);
    rig.click(&mut app, &view, tool_row_key(CONV, "read"));
    assert!(rig.shows("这样改会让缓存失效"), "adding one rewrites the declared list here");
    assert!(fx.settings().enabled_tools.is_empty());
    rig.click(&mut app, &view, "cache-break-cancel");
    rig.click(&mut app, &view, ("nav", CONV, Page::Skills));
    rig.click(&mut app, &view, row_of("skills", "skill_a"));
    assert!(!rig.shows("这样改会让缓存失效"), "skills are not tools");
    assert_eq!(fx.settings().skill_ids, vec!["skill_a"]);
}

#[test]
fn draws_the_tools_the_warm_cache_holds_orange_where_they_stand() {
    let (mut rig, mut app) = start("tools-warm");
    let lock = ConversationToolLock { tools: vec!["read".into()], ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.enabled_tools = vec!["read".into()];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Tools);
    rig.frame(&mut app, &view);
    assert!(rig.shows("cache-orange"));
    rig.click(&mut app, &view, tool_row_key(CONV, "write"));
    assert!(!rig.shows("这样改会让缓存失效"), "adding on a model that appends tools costs nothing");
    rig.click(&mut app, &view, tool_row_key(CONV, "read"));
    assert!(rig.shows("这样改会让缓存失效"));
}

#[test]
fn draws_a_tool_that_is_off_orange_while_the_transcript_holds_its_calls() {
    let (mut rig, mut app) = start("tools-called");
    let lock = ConversationToolLock { tools: vec!["read".into()], ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.enabled_tools = vec!["read".into()];
        s.tool_lock = Some(lock);
    });
    let card: model::ContextItem = serde_json::from_value(json!({
        "kind": "tool",
        "id": "card_write",
        "toolName": "write",
        "modelTurnId": "turn_1",
        "input": {},
        "result": { "success": true, "output": "", "executedAt": "", "durationMs": 0 },
        "createdAt": "",
    }))
    .unwrap();
    fx.conv.borrow_mut().contexts.push(card);
    let view = fx.view(Page::Tools);
    rig.click(&mut app, &view, tool_row_key(CONV, "write"));
    assert!(rig.shows("这样改会让缓存失效"), "its calls would come back into the prefix");
    assert!(!fx.settings().enabled_tools.contains(&"write".to_owned()));
    rig.click(&mut app, &view, "cache-break-confirm");
    assert!(fx.settings().enabled_tools.contains(&"write".to_owned()));
}

#[test]
fn tones_the_hooks_the_warm_cache_was_built_from_and_ignores_a_lock_that_never_recorded_them() {
    let (mut rig, mut app) = start("hooks-warm");
    app.doc.capabilities.hooks = vec![resource("hook_a", "Hook A", None)];
    let lock = ConversationToolLock { hook_ids: Some(vec!["hook_a".into()]), ..warm(&mut app, true) };
    let older = ConversationToolLock { hook_ids: None, ..lock.clone() };
    let fx = Fx::new(move |s| {
        s.hook_ids = vec!["hook_a".into()];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Hooks);
    rig.click(&mut app, &view, row_of("hooks", "hook_a"));
    assert!(rig.shows("这样改会让缓存失效"));
    let fx = Fx::new(move |s| {
        s.hook_ids = vec!["hook_a".into()];
        s.tool_lock = Some(older);
    });
    fx.conv.borrow_mut().id = "conv-old".into();
    let view = fx.view(Page::Hooks);
    rig.click(&mut app, &view, catalog_row_key("catalog:conv-old:hooks:global", "hook_a"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert!(fx.settings().hook_ids.is_empty());
}

#[test]
fn rescans_on_mount_and_from_the_toolbar_of_every_capability_page() {
    let (mut rig, mut app) = start("rescan");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(fx.call_count("rescan"), 1, "once per mounted pane");
    rig.click(&mut app, &view, ("rescan", "catalog:conv1:skills"));
    assert_eq!(fx.call_count("rescan"), 2);
    rig.click(&mut app, &view, ("nav", CONV, Page::Hooks));
    rig.click(&mut app, &view, ("rescan", "catalog:conv1:hooks"));
    assert_eq!(fx.call_count("rescan"), 3);
}

#[test]
fn opens_the_global_or_this_workspaces_config_folder() {
    let (mut rig, mut app) = start("reveal");
    let fx = Fx::new(|_| {});
    *fx.workspaces.borrow_mut() = Some(workspace_a());
    let view = fx.view(Page::Mcp);
    rig.click(&mut app, &view, ("reveal", "catalog:conv1:mcp", "global"));
    assert!(fx.called("reveal:mcp:"));
    rig.click(&mut app, &view, ("reveal", "catalog:conv1:mcp", "local|/w/a"));
    assert!(fx.called("reveal:mcp:local|/w/a"));
}

#[test]
fn tests_an_mcp_servers_connection_in_the_row_and_reports_what_came_back() {
    let (mut rig, mut app) = start("probe");
    let mut unavailable = resource("mcp_off", "Off", None);
    unavailable.available = false;
    app.doc.capabilities.mcps = vec![resource("mcp_a", "Alpha", None), unavailable];
    let fx = Fx::new(|_| {});
    *fx.probe.borrow_mut() = Some(Ok(McpProbeReport { ok: true, server_name: Some("srv".into()), server_version: Some("1.2".into()), tools: vec![json!({}), json!({})], ..Default::default() }));
    let view = fx.view(Page::Mcp);
    rig.frame(&mut app, &view);
    assert!(rig.shows("测试连接 Alpha"));
    assert!(!rig.shows("测试连接 Off"), "an unavailable entry already carries the reason it cannot");
    rig.click(&mut app, &view, ("probe", "catalog:conv1:mcp", "mcp_a"));
    assert!(rig.shows("2 个工具"), "{:?}", rig.said());
    assert!(rig.shows_containing("srv 1.2"), "who answered is said to a reader");
    // A re-test replaces the last result rather than layering onto it.
    *fx.probe.borrow_mut() = Some(Ok(McpProbeReport { ok: false, error: Some("refused".into()), logs: vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into(), "6".into()], ..Default::default() }));
    rig.click(&mut app, &view, ("probe", "catalog:conv1:mcp", "mcp_a"));
    assert!(rig.shows("连接失败"));
    assert!(!rig.shows("2 个工具"));
    assert!(rig.shows_containing("refused\n2\n3\n4\n5\n6"), "the last few stderr lines: {:?}", rig.said());
}

#[test]
fn says_why_a_remote_workspaces_entries_are_missing() {
    let (mut rig, mut app) = start("unreadable");
    app.doc.capabilities.unreadable_levels = vec![model::UnreadableLevel { workspace_key: "ssh:m1|/srv".into(), message: "the machine did not answer".into() }];
    let fx = Fx::new(|_| {});
    *fx.workspaces.borrow_mut() = Some(vec![CapabilityWorkspace { number: 1, key: "ssh:m1|/srv".into(), machine: Some(model::RunTarget::Ssh { machine_id: "m1".into() }), path: "/srv".into() }]);
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(rig.shows("the machine did not answer"));
    assert!(rig.shows("已删除的机器"), "no such machine in the catalogue: {:?}", rig.said());
}

#[test]
fn deletes_a_catalog_entry_from_its_own_row_in_two_clicks_and_never_a_builtin() {
    let (mut rig, mut app) = start("delete");
    app.doc.capabilities.skills = vec![resource("skill_a", "Alpha", None), builtin("skill_sdk", "Mewrk SDK")];
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("删除 Mewrk SDK"), "a built-in has no copy of its own to remove");
    let key = hash_key(("delete", "catalog:conv1:skills", "skill_a"));
    rig.click(&mut app, &view, key);
    assert!(rig.shows("确认删除 Alpha"));
    assert_eq!(fx.call_count("delete:"), 0);
    rig.click(&mut app, &view, key);
    assert!(fx.called("delete:skills:skill_a"));
    assert_eq!(app.doc.capabilities.skills[1].source, ResourceSource::Builtin);
}

#[test]
fn offers_no_delete_on_a_catalog_row_when_the_owner_cannot_route_one() {
    let (mut rig, mut app) = start("no-delete");
    app.doc.capabilities.skills = vec![resource("skill_a", "Alpha", None)];
    let fx = Fx::new(|_| {});
    *fx.routes_delete.borrow_mut() = false;
    let view = fx.view(Page::Skills);
    rig.frame(&mut app, &view);
    assert!(!rig.shows("删除 Alpha"));
}

#[test]
fn reorders_rows_by_drag_and_keyboard_keeps_them_within_their_level_and_remembers() {
    let (mut rig, mut app) = start("reorder");
    app.doc.capabilities.skills = vec![resource("a", "A", None), resource("b", "B", None), resource("c", "C", None)];
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Skills);
    let order = |app: &mut App| mewrk_settings_conv::logic::catalog_order(app, mewrk_settings_conv::logic::OrderScope::Skills);
    rig.drag(&mut app, &view, row_of("skills", "c"), row_of("skills", "a"), true);
    assert_eq!(order(&mut app), vec!["c", "a", "b"]);
    assert!(fx.settings().skill_ids.is_empty(), "a drag never ends in a click");
    // The other way a row moves: the keyboard, from the row itself.
    rig.click(&mut app, &view, row_of("skills", "c"));
    assert_eq!(fx.settings().skill_ids, vec!["c"], "a click turns it, and focuses the row");
    rig.press(&mut app, &view, Key::Down, true);
    assert_eq!(order(&mut app), vec!["a", "c", "b"]);
}

#[test]
fn ends_every_capability_page_with_its_own_documentation_link() {
    let (mut rig, mut app) = start("docs");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Skills);
    for (page, href) in [(Page::Skills, "skills.html"), (Page::Mcp, "mcp.html"), (Page::Hooks, "hooks.html"), (Page::ToolDescriptions, "prompt-profiles.html"), (Page::Roles, "agents.html#roles")] {
        rig.click(&mut app, &view, ("nav", CONV, page));
        assert!(rig.shows(&format!("docs:https://mewrk.dev/zh-CN/{href}")), "{page:?}: {:?}", rig.said());
    }
}

// ---------------------------------------------------------------- presets

fn add_preset(app: &mut App, id: &str, name: &str, template: &str) {
    let settings = mewrk_client::presets::empty_conversation_preset_settings();
    app.doc.presets.conversation_presets.push(model::ConversationPreset { id: id.into(), name: name.into(), description: String::new(), template_id: template.into(), settings });
}

fn presets_key(what: &str, id: &str) -> (String, String, String) {
    (what.to_owned(), format!("catalog:{CONV}:presets"), id.to_owned())
}

#[test]
fn keeps_the_save_as_preset_entry_and_asks_for_a_name() {
    let (mut rig, mut app) = start("save-as");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, ("settings-save", CONV));
    assert!(rig.shows("预设名称"));
    // A preset can only open with a timeline the conversation has.
    assert!(rig.shows("这段对话还没有消息"));
    rig.click(&mut app, &view, ("save-as-confirm", CONV));
    assert!(rig.shows("预设名称"), "no name, nothing saved");
    assert_eq!(app.doc.presets.conversation_presets.iter().filter(|p| p.id.starts_with("preset_")).count(), app.doc.presets.conversation_presets.iter().filter(|p| p.id.starts_with("preset_mewrk")).count());
    rig.type_into(&mut app, &view, "save-as-name:conv1", "审查");
    rig.click(&mut app, &view, ("save-as-confirm", CONV));
    // Either way the new preset opens at its template: an empty one to write.
    assert!(rig.shows("审查"), "{:?}", rig.said());
    assert!(rig.shows("对话模板"), "the page only a preset has: {:?}", rig.said());
    assert!(rig.shows("保存预设"));
}

#[test]
fn saves_a_preset_with_the_timeline_as_its_template_when_asked() {
    let (mut rig, mut app) = start("save-as-template");
    let fx = Fx::new(|_| {});
    fx.conv.borrow_mut().contexts = vec![serde_json::from_value(json!({ "kind": "user", "id": "ctx1", "createdAt": "", "content": "hello" })).unwrap()];
    let view = fx.view(Page::Tools);
    rig.click(&mut app, &view, ("settings-save", CONV));
    assert!(!rig.shows("这段对话还没有消息"));
    rig.click(&mut app, &view, ("save-as-capture", CONV));
    rig.type_into(&mut app, &view, "save-as-name:conv1", "带模板");
    rig.click(&mut app, &view, ("save-as-confirm", CONV));
    let preset = app.doc.presets.conversation_presets.iter().find(|p| p.name == "带模板").expect("saved");
    assert_eq!(preset.template_id, "template_captured");
}

#[test]
fn keeps_the_save_as_dialog_open_with_the_reason_when_saving_fails() {
    let (mut rig, mut app) = start("save-as-fail");
    let fx = Fx::new(|_| {});
    *fx.fail_create.borrow_mut() = Some("disk full".into());
    let view = fx.view(Page::Tools);
    rig.click(&mut app, &view, ("settings-save", CONV));
    rig.type_into(&mut app, &view, "save-as-name:conv1", "x");
    rig.click(&mut app, &view, ("save-as-confirm", CONV));
    assert!(rig.shows("disk full"));
    assert!(rig.shows("预设名称"), "still open");
}

#[test]
fn applies_a_preset_names_the_one_a_conversation_carries_and_forgets_a_deleted_one() {
    let (mut rig, mut app) = start("apply");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    fx.conv.borrow_mut().preset_id = "preset_a".into();
    let view = fx.view(Page::Presets);
    rig.frame(&mut app, &view);
    assert!(rig.shows("applied:Alpha"));
    rig.click(&mut app, &view, presets_key("preset-apply", "preset_a"));
    assert!(fx.called("apply:preset_a"));
    fx.conv.borrow_mut().preset_id = "preset_gone".into();
    rig.frame(&mut app, &view);
    assert!(!rig.shows_containing("applied:"), "a dangling id reads as an unnamed draft");
}

#[test]
fn shows_a_fallback_for_a_blank_preset_name_and_offers_no_rename_or_delete_for_the_builtin() {
    let (mut rig, mut app) = start("preset-builtin");
    add_preset(&mut app, "preset_blank", "", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.frame(&mut app, &view);
    assert!(rig.shows("未命名预设"));
    assert!(rig.shows_containing("内置预设，随 Mewrk 版本更新"));
    let builtin_delete = hash_key(("preset-delete", format!("catalog:{CONV}:presets"), "preset_mewrk"));
    rig.click(&mut app, &view, builtin_delete);
    rig.click(&mut app, &view, builtin_delete);
    assert_eq!(fx.call_count("delete-preset"), 0);
    rig.click(&mut app, &view, presets_key("preset-rename-btn", "preset_mewrk"));
    assert!(!rig.shows_containing("重命名预设"));
}

#[test]
fn renames_a_saved_preset_in_the_row_committing_when_the_field_is_left() {
    let (mut rig, mut app) = start("rename");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-rename-btn", "preset_a"));
    assert!(rig.shows("重命名预设 Alpha"));
    let field_key = ("preset-rename", format!("catalog:{CONV}:presets"), "preset_a");
    rig.type_into(&mut app, &view, field_key, " Two");
    assert_eq!(fx.call_count("rename:"), 0, "no write while typing");
    // Pressing elsewhere leaves the field.
    rig.click(&mut app, &view, ("nav", CONV, Page::Presets));
    assert!(fx.called("rename:preset_a:Alpha Two"), "{:?}", fx.calls.borrow());
    assert!(!rig.shows_containing("重命名预设"));
}

#[test]
fn renaming_to_nothing_or_the_same_writes_nothing_and_escape_cancels() {
    let (mut rig, mut app) = start("rename-none");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-rename-btn", "preset_a"));
    rig.click(&mut app, &view, ("nav", CONV, Page::Presets));
    assert_eq!(fx.call_count("rename:"), 0);
    assert!(!rig.shows_containing("重命名预设"));
    rig.click(&mut app, &view, presets_key("preset-rename-btn", "preset_a"));
    let field_key = ("preset-rename", format!("catalog:{CONV}:presets"), "preset_a");
    rig.type_into(&mut app, &view, field_key, "x");
    rig.press(&mut app, &view, Key::Escape, false);
    assert!(!rig.shows_containing("重命名预设"));
    assert_eq!(fx.call_count("rename:"), 0);
}

#[test]
fn deletes_a_saved_conversation_preset_from_its_own_row() {
    let (mut rig, mut app) = start("delete-preset");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    let key = hash_key(("preset-delete", format!("catalog:{CONV}:presets"), "preset_a"));
    rig.click(&mut app, &view, key);
    assert!(rig.shows("确认删除预设 Alpha"));
    rig.click(&mut app, &view, key);
    assert!(fx.called("delete-preset:preset_a"));
}

#[test]
fn opens_a_preset_into_the_whole_pane_and_saves_it_back_without_touching_the_conversation() {
    let (mut rig, mut app) = start("preset-open");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let lock = warm(&mut app, true);
    let fx = Fx::new(move |s| {
        s.enabled_tools = vec!["read".into()];
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    assert!(rig.shows("保存预设"));
    assert!(rig.shows("对话模板"));
    // A preset has run nothing and cached nothing, so nothing asks.
    rig.click(&mut app, &view, tool_row_key("preset:preset_a", "write"));
    assert!(!rig.shows("这样改会让缓存失效"));
    assert_eq!(fx.settings().enabled_tools, vec!["read"], "the conversation is untouched");
    assert_eq!(fx.call_count("save-preset"), 0);
    rig.click(&mut app, &view, ("settings-save", "preset:preset_a"));
    assert!(fx.called("save-preset:preset_a:write"), "{:?}", fx.calls.borrow());
    assert!(!rig.shows("保存预设"), "saving closes the window");
}

#[test]
fn offers_the_tool_descriptions_page_in_a_preset_window_and_saves_the_chosen_file() {
    let (mut rig, mut app) = start("preset-profile");
    with_profiles(&mut app);
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let saved: Rc<RefCell<Option<Option<String>>>> = Rc::default();
    let saved2 = saved.clone();
    let view = {
        let fx = fx.clone();
        move |app: &mut App| {
            let mut props = fx.props(Page::Presets);
            let s = saved2.clone();
            props.handlers.on_save_preset = Some(Rc::new(move |_, _, body| *s.borrow_mut() = Some(body.tool_description_file_id)));
            conversation_settings(app, props)
        }
    };
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    rig.click(&mut app, &view, ("nav", "preset:preset_a", Page::ToolDescriptions));
    rig.click(&mut app, &view, catalog_row_key("catalog:preset:preset_a:toolDescriptions:global", "file_a"));
    rig.click(&mut app, &view, ("settings-save", "preset:preset_a"));
    assert_eq!(*saved.borrow(), Some(Some("file_a".to_owned())));
}

#[test]
fn saves_an_edited_builtin_preset_as_a_new_preset_never_in_place() {
    let (mut rig, mut app) = start("preset-builtin-copy");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_mewrk"));
    assert!(rig.shows("内置预设不能修改，改动可另存为新预设。"));
    assert!(rig.shows("另存为新预设"));
    rig.click(&mut app, &view, ("settings-save", "preset:preset_mewrk"));
    assert_eq!(fx.call_count("save-preset"), 0);
    assert!(fx.called("save-copy:preset_mewrk:-1"), "{:?}", fx.calls.borrow());
}

#[test]
fn shows_the_whole_catalog_in_a_preset_window_which_belongs_to_no_workspace() {
    let (mut rig, mut app) = start("preset-catalog");
    skills_catalog(&mut app);
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    rig.click(&mut app, &view, ("nav", "preset:preset_a", Page::Skills));
    assert!(rig.shows("Other"), "{:?}", rig.said());
    assert!(rig.shows("/w/other"));
}

#[test]
fn opens_the_presets_own_message_queue_and_writes_the_edited_body_back_binding_the_minted_id() {
    let (mut rig, mut app) = start("preset-template");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    rig.click(&mut app, &view, ("nav", "preset:preset_a", Page::Template));
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "开局一句话");
    rig.frame(&mut app, &view);
    let stored = fx.template_store.borrow();
    assert_eq!(stored.len(), 1, "autosave writes each committed edit");
    assert_eq!(stored[0].0, "template_minted");
    assert!(fx.called("bind:preset_a:template_minted"), "bound at once, not on the window's save");
}

#[test]
fn edits_the_builtin_presets_template_in_its_window_and_carries_the_edit_into_the_copy() {
    let (mut rig, mut app) = start("preset-builtin-template");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_mewrk"));
    rig.click(&mut app, &view, ("nav", "preset:preset_mewrk", Page::Template));
    place_user_message(&mut rig, &mut app, &view, "template:unsaved", "held");
    rig.frame(&mut app, &view);
    assert!(fx.template_store.borrow().is_empty(), "the built-in cannot change");
    assert!(rig.shows("对话模板 1"), "the rail counts what is held: {:?}", rig.said());
    rig.click(&mut app, &view, ("settings-save", "preset:preset_mewrk"));
    assert!(fx.called("save-copy:preset_mewrk:1"), "{:?}", fx.calls.borrow());
}

#[test]
fn a_preset_window_can_be_dismissed_from_its_backdrop_and_keeps_nothing() {
    let (mut rig, mut app) = start("preset-backdrop");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    rig.click(&mut app, &view, tool_row_key("preset:preset_a", "write"));
    rig.click_at(&mut app, &view, 1.0, 1.0);
    assert!(!rig.shows("保存预设"));
    assert_eq!(fx.call_count("save-preset"), 0, "closing a preset's window is not saving it");
}

#[test]
fn a_change_that_waits_on_the_warning_lands_on_the_settings_as_they_are_when_answered() {
    let (mut rig, mut app) = start("in-flight");
    let lock = ConversationToolLock { global_memory: true, ..warm(&mut app, true) };
    let fx = Fx::new(move |s| {
        s.global_memory_enabled = true;
        s.tool_lock = Some(lock);
    });
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("switch", "conv1:global-memory"));
    assert!(rig.shows("这样改会让缓存失效"));
    // A run lands meanwhile and records what it sent.
    let later = warm_lock("p1", "m1");
    fx.conv.borrow_mut().settings.tool_lock = Some(ConversationToolLock { global_memory: true, skill_ids: vec!["from-the-run".into()], ..later });
    rig.frame(&mut app, &view);
    rig.click(&mut app, &view, "cache-break-confirm");
    let s = fx.settings();
    assert!(!s.global_memory_enabled);
    assert_eq!(s.tool_lock.unwrap().skill_ids, vec!["from-the-run"], "not written over");
}

#[test]
fn a_role_saved_later_lands_on_the_settings_as_they_are_by_then() {
    let (mut rig, mut app) = start("role-late");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Roles);
    rig.click(&mut app, &view, ("agent-role-add", format!("catalog:{CONV}:roles")));
    rig.type_into(&mut app, &view, ("role-name", "new-role"), "R");
    fx.conv.borrow_mut().settings.agent_ids = vec!["agent_other".into()];
    rig.click(&mut app, &view, ("role-save", "new-role"));
    assert_eq!(fx.settings().agent_ids, vec!["agent_other", "agent_R"]);
}

// feedback.css, settings.css and ConversationSettings.css: a window's rail is the settings windows' own, the side pane keeps its compact one.
#[test]
fn a_window_takes_the_settings_layout_and_the_pane_keeps_its_own_rail() {
    let (mut rig, mut app) = start("rails");
    add_preset(&mut app, "preset_a", "Alpha", "");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.settle(&mut app, &view);
    let side_item = rect_of(("nav", CONV, Page::Tools)).unwrap();
    assert!(side_item.w <= 136.0, "150 px, less its padding: {side_item:?}");
    assert_eq!(side_item.h, 30.0);
    rig.click(&mut app, &view, presets_key("preset-open", "preset_a"));
    let item = rect_of(("nav", "preset:preset_a", Page::Tools)).unwrap();
    assert!(item.w > 136.0 && item.w <= 172.0, "190 px, less its padding: {item:?}");
    assert_eq!(item.h, 36.0);
    let next = rect_of(("nav", "preset:preset_a", Page::Advanced)).unwrap();
    assert_eq!(item.x, next.x, "down the left edge");
    // At 720 px the rail is a strip across the top, with the page under it.
    rig.size = (700.0, 520.0);
    rig.settle(&mut app, &view);
    let item = rect_of(("nav", "preset:preset_a", Page::Tools)).unwrap();
    let next = rect_of(("nav", "preset:preset_a", Page::Advanced)).unwrap();
    assert!(next.x > item.x, "side by side: {item:?} {next:?}");
}

// `@container (max-width: 430px)`: the pane dragged towards its floor gives up rail so the body keeps a readable column.
#[test]
fn a_pane_dragged_narrow_narrows_its_rail() {
    let (mut rig, mut app) = start("narrow-pane");
    let fx = Fx::new(|_| {});
    let inner = fx.view(Page::Tools);
    let view = |app: &mut App| -> E { row().child(row().size(400.0, 1000.0).child(inner(app))) };
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    let item = rect_of(("nav", CONV, Page::Tools)).unwrap();
    assert!(item.w <= 98.0, "108 px, less its padding: {item:?}");
}

#[test]
fn a_dialog_has_its_way_out_in_the_corner() {
    let (mut rig, mut app) = start("dialog-close");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Tools);
    rig.click(&mut app, &view, ("settings-save", CONV));
    assert!(rig.shows("预设名称"));
    rig.click(&mut app, &view, ("plain-dialog-close", hash_key(("save-as", CONV))));
    assert!(!rig.shows("预设名称"));
}

// ---------------------------------------------------------------- accessibility

/// Every page with a client listening: everything a person can reach has a name, Tab goes round all of it, and the rail says where they are.
#[test]
fn every_page_names_every_control_it_has() {
    let (mut rig, mut app) = start("a11y-sweep");
    let fx = Fx::new(|s| {
        s.skill_ids = vec!["gone-skill".into()];
    });
    app.doc.capabilities.skills.push(resource("s1", "Skill one", None));
    app.doc.capabilities.mcps.push(resource("m1", "Server one", None));
    app.doc.capabilities.hooks.push(resource("h1", "Hook one", None));
    for page in [Page::Tools, Page::Advanced, Page::Skills, Page::Mcp, Page::Hooks, Page::ToolDescriptions, Page::Roles, Page::Presets] {
        let view = fx.view(page);
        let tree = a11y_tree(&mut rig, &mut app, &view);
        assert_eq!(a11y_unnamed(&tree), Vec::<String>::new(), "{page:?}");
        a11y_is(&tree, Role::Navigation, t("对话设置分类", "Conversation settings categories"));
        let stops = a11y_tab_stops(&mut rig, &mut app, &view);
        assert!(stops.len() >= 8, "{page:?}: the rail and the save at least: {stops:#?}");
        assert!(stops.iter().all(|stop| !stop.ends_with("\"\"") && !stop.starts_with("Window")), "{page:?}: {stops:#?}");
        // The next page's walk starts from the top too.
        rig.press(&mut app, &view, Key::Escape, false);
    }
    set_a11y_active(false);
}

#[test]
fn the_rail_says_which_page_is_up_and_a_tools_row_says_what_a_press_does() {
    let (mut rig, mut app) = start("a11y-tools");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Tools);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::Button, "工具 1").aria_current().is_some(), "the open page is the current one");
    assert!(a11y_is(&tree, Role::Button, "高级工具").aria_current().is_none());
    let group = a11y_is(&tree, Role::Button, "文件与搜索");
    assert_eq!(group.is_expanded(), Some(true));
    assert_eq!(group.controls().len(), 1, "the disclosure says what it opens");
    a11y_is(&tree, Role::Button, "全选文件与搜索");
    a11y_is(&tree, Role::Button, "全不选文件与搜索");
    assert_eq!(a11y_is(&tree, Role::Button, "读取已启用").toggled(), Some(accesskit::Toggled::True));
    assert_eq!(a11y_is(&tree, Role::Button, "写入已关闭").toggled(), Some(accesskit::Toggled::False));
    a11y_is(&tree, Role::Link, "读取的说明文档");
    set_a11y_active(false);
}

#[test]
fn the_advanced_page_names_its_switches_by_state_and_its_choices_as_radios() {
    let (mut rig, mut app) = start("a11y-advanced");
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Switch, "网页搜索已开启");
    a11y_is(&tree, Role::Switch, "全局记忆已关闭");
    a11y_is(&tree, Role::RadioGroup, "域名过滤");
    assert_eq!(a11y_is(&tree, Role::RadioButton, "黑名单").toggled().is_some(), true);
    a11y_is(&tree, Role::RadioGroup, "宿主消息容器");
    a11y_is(&tree, Role::RadioButton, "user");
    a11y_is(&tree, Role::RadioButton, "box");
    let backend = tree.iter().map(|(_, node)| node).find(|node| a11y_name(node).starts_with("搜索提供商：")).expect("the picker says what it picks and what it is set to");
    assert_eq!(backend.has_popup(), Some(accesskit::HasPopup::Menu));
    set_a11y_active(false);
}

#[test]
fn the_save_as_dialog_is_named_and_its_check_box_is_one_stop() {
    let (mut rig, mut app) = start("a11y-dialogs");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.click(&mut app, &view, ("settings-save", CONV));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::Dialog, "另存为预设").is_modal());
    a11y_is(&tree, Role::TextInput, "预设名称");
    assert_eq!(a11y_is(&tree, Role::CheckBox, "将当前上下文作为对话模板").toggled(), Some(accesskit::Toggled::False));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn the_domain_lists_are_a_named_window_with_a_rail() {
    let (mut rig, mut app) = start("a11y-windows");
    let fx = Fx::new(|s| s.web_search_enabled = true);
    let view = fx.view(Page::Advanced);
    rig.click(&mut app, &view, ("domain-edit", CONV));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, "域名名单");
    a11y_is(&tree, Role::Navigation, "域名名单");
    assert!(a11y_is(&tree, Role::Button, "黑名单 0").aria_current().is_some());
    a11y_is(&tree, Role::MultilineTextInput, "黑名单");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_presets_window_is_named_for_it_with_a_rail_and_tab_goes_round_it() {
    let (mut rig, mut app) = start("a11y-preset");
    let fx = Fx::new(|_| {});
    let view = fx.view(Page::Presets);
    rig.frame(&mut app, &view);
    let preset = app.doc.presets.conversation_presets.first().expect("the built-in").id.clone();
    rig.click(&mut app, &view, presets_key("preset-open", &preset));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Dialog && !a11y_name(node).is_empty()));
    a11y_is(&tree, Role::Region, "预设设置");
    a11y_is(&tree, Role::Navigation, "对话设置分类");
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    let stops = a11y_tab_stops(&mut rig, &mut app, &view);
    assert!(stops.len() >= 8 && stops.iter().all(|stop| !stop.ends_with("\"\"")), "{stops:#?}");
    set_a11y_active(false);
}
