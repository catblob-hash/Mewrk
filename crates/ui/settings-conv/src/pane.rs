//! `ConversationSettings.tsx`: the pane itself, its rail, its one warning, saving as a preset, and a preset opened in a window of this same pane.

use std::rc::Weak;

use mewrk_client::model::{ContextItem, Conversation, ConversationSettings as Settings, ConversationPreset, HostMessageContainer, ResourceDescriptor, ToolDescriptor};
use mewrk_client::prelude::*;

use crate::advanced::{remembered_legs, settled_backend_hint, AdvancedPage, BackendLock, Flag, WebPatch, WebSection, WebSubject};
use crate::capability_editor::editable_capability_page;
use crate::catalog::{Badge, CapabilityPage, CatalogKind};
use crate::lock::*;
use crate::logic::*;
use crate::ops::{self, Handlers, SettingsPatch, TemplateSummary};
use crate::roles::{untitled_preset, PresetsPage, RolesPage};
use crate::template_editor::TemplateEditor;
use crate::tools::ToolGroups;
use crate::web::{family_selects_native_tool_type, family_supports_native_fetch, web_leg_on, WebLeg};
use crate::widgets::*;

/// The pages the pane lists down its left edge. `Tools` is the tool picker alone; `Advanced` carries what is derived from a switch. The others each own one kind of thing the conversation composes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    Tools,
    Advanced,
    Skills,
    Mcp,
    Hooks,
    ToolDescriptions,
    Roles,
    Template,
    Presets,
}

const NAVIGATION: [(Page, &str); 9] = [(Page::Tools, "Wrench"), (Page::Advanced, "SlidersHorizontal"), (Page::Skills, "Box"), (Page::Mcp, "Blocks"), (Page::Hooks, "FolderCog"), (Page::ToolDescriptions, "ScrollText"), (Page::Roles, "Bot"), (Page::Template, "FileText"), (Page::Presets, "SlidersHorizontal")];

/// What this pane is editing. `Preset` is the same pane opened from the presets page on a saved preset, so the two can never drift apart: it drops the page that describes a live
/// conversation (a preset holds no presets), gains the one only a preset has, and its save writes in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Conversation,
    Preset,
}

/// How often the open pane asks whether the capability files changed.
const CAPABILITY_WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2_000);

#[derive(Clone)]
pub struct ConversationSettingsProps {
    pub conversation: Conversation,
    /// The tools this conversation can run: the catalogue narrowed to the shells its machines have.
    pub tools: Rc<Vec<ToolDescriptor>>,
    /// The whole trusted catalogue, for the role window. A role is an asset any conversation may select, so what it may hold is not bounded by the machines of whoever opened it.
    pub role_tools: Rc<Vec<ToolDescriptor>>,
    pub handlers: Handlers,
    pub templates: Vec<TemplateSummary>,
    pub capability_error: Option<String>,
    pub preset_error: Option<String>,
    /// The conversation's workspaces. `Some(vec![])` is a draft with no project yet, which sees the global level alone. `None` means no narrowing: the preset editor, which points at nothing in particular.
    pub workspaces: Option<Vec<CapabilityWorkspace>>,
    pub mode: Mode,
    /// In preset mode, the preset this pane is a window onto.
    pub preset_id: Option<String>,
    pub initial_view: Page,
    /// In a window the pane takes the settings layout: a wide rail with the window's title.
    pub window_title: Option<String>,
}

#[derive(Clone)]
struct PresetDraft {
    id: String,
    settings: Settings,
    view: Option<Page>,
}

#[derive(Clone, Default)]
struct SaveAs {
    name: String,
    capture_template: bool,
    saving: bool,
    error: Option<String>,
}

struct PaneState {
    view: Page,
    preset_draft: Option<PresetDraft>,
    /// The open preset's body, read once the user asks for that page. Reading it on the chance the page is opened would slow down opening a preset for everyone who never looks.
    template_body: Option<Vec<ContextItem>>,
    template_read_for: Option<String>,
    /// The built-in preset's opening messages as edited in its window. The built-in cannot change, so the edits are held here and go with the copy that saving makes.
    held_template: Option<Vec<ContextItem>>,
    save_as: Option<SaveAs>,
    /// A change that would throw the warm cache away waits here for the one warning every page shares: the patch rather than the whole body, so a run that lands meanwhile is not written over.
    cache_break: Option<(SettingsPatch, bool)>,
    cache_break_never: bool,
    rescanned: bool,
    /// Lives exactly as long as the pane is on screen; the watch below holds a weak copy.
    watch_token: Rc<()>,
    watching: bool,
    /// The moment the cache runs out, already scheduled to redraw.
    armed_until: Option<i64>,
}

impl PaneState {
    fn new(view: Page) -> Self {
        Self { view, preset_draft: None, template_body: None, template_read_for: None, held_template: None, save_as: None, cache_break: None, cache_break_never: false, rescanned: false, watch_token: Rc::new(()), watching: false, armed_until: None }
    }
}

// While the pane is open the lists follow the disk: a saved `mcp.json` or a new skill folder shows up by itself shortly after. Only the cheap summary is polled; a full rescan runs when it moves.

struct Watched {
    alive: Weak<()>,
    handlers: Handlers,
    id: u64,
    last: Option<String>,
    busy: bool,
}

#[derive(Default)]
struct Watch {
    panes: Vec<Watched>,
    armed: bool,
    next: u64,
}

fn watch_pane(app: &mut App, token: &Rc<()>, handlers: Handlers) {
    let watch = app.local::<Watch>();
    watch.next += 1;
    let id = watch.next;
    watch.panes.push(Watched { alive: Rc::downgrade(token), handlers, id, last: None, busy: false });
    if !std::mem::replace(&mut watch.armed, true) {
        app.set_timer(CAPABILITY_WATCH_INTERVAL, watch_tick);
    }
}

fn watch_tick(app: &mut App) {
    let watch = app.local::<Watch>();
    watch.panes.retain(|p| p.alive.strong_count() > 0);
    if watch.panes.is_empty() {
        watch.armed = false;
        return;
    }
    let due: Vec<(u64, Handlers)> = watch.panes.iter_mut().filter(|p| !p.busy).map(|p| {
        p.busy = true;
        (p.id, p.handlers.clone())
    }).collect();
    for (id, handlers) in due {
        let Some(read) = handlers.on_fingerprint.clone() else { continue };
        read(app, Box::new(move |app, result| {
            let mut changed = false;
            if let Some(p) = app.local::<Watch>().panes.iter_mut().find(|p| p.id == id) {
                p.busy = false;
                if let Ok(current) = result {
                    changed = p.last.as_ref().is_some_and(|last| *last != current);
                    p.last = Some(current);
                }
            }
            if changed && let Some(rescan) = handlers.on_rescan.clone() {
                rescan(app);
            }
        }));
    }
    app.set_timer(CAPABILITY_WATCH_INTERVAL, watch_tick);
}

fn page_title(page: Page) -> &'static str {
    match page {
        Page::Tools => t("工具", "Tools"),
        Page::Advanced => t("高级工具", "Advanced tools"),
        Page::Skills => t("技能", "Skills"),
        Page::Mcp => "MCP",
        Page::Hooks => t("钩子", "Hooks"),
        Page::ToolDescriptions => t("工具描述", "Tool descriptions"),
        Page::Roles => t("代理角色", "Agent roles"),
        Page::Template => t("对话模板", "Conversation template"),
        Page::Presets => t("对话预设", "Conversation presets"),
    }
}

fn page_blurb(page: Page) -> &'static str {
    match page {
        Page::Tools => t("本对话交给模型的工具。", "The tools this conversation hands the model."),
        Page::Advanced => t("由开关控制、不在工具列表里的能力。", "Capabilities set by switches, outside the tool list."),
        Page::Skills => t("本对话可用的技能。", "The skills this conversation can use."),
        Page::Mcp => t("本对话接入的 MCP Server。", "The MCP servers this conversation connects to."),
        Page::Hooks => t("本对话运行的钩子。", "The hooks this conversation runs."),
        Page::ToolDescriptions => t("改写模型所见工具描述与提示词的文件。", "Files that reword the model's tool descriptions and prompts."),
        Page::Roles => t("子代理可以点名使用的角色。", "The roles a subagent can be spawned as."),
        Page::Template => t("套用预设时铺进对话的开局消息。", "The opening messages laid in when this preset is applied."),
        Page::Presets => t("存好的整页设置，套用即复制到本对话。", "Saved copies of these settings; apply one to copy it here."),
    }
}

/// The section of the site's working page that explains each page with no documentation link of its own. The catalog pages end their toolbar with one.
fn page_docs_section(page: Page) -> Option<&'static str> {
    match page {
        Page::Tools => Some("tools"),
        Page::Advanced => Some("advanced-tools"),
        Page::Template => Some("templates"),
        Page::Presets => Some("presets"),
        _ => None,
    }
}

/// The pane with the wiring upstream's `App` gives the active conversation or draft. `None` while nothing is active.
pub fn conversation_settings_pane(app: &mut App) -> Option<E> {
    let conversation = app.active_conversation_view()?;
    let role_tools = ops::localized_tools(app);
    let tools = ops::active_conversation_tools(app, &role_tools);
    let attached = conversation.attached_workspaces.clone();
    let workspaces = capability_workspaces(app.active_workspace(), &attached);
    let props = ConversationSettingsProps {
        conversation,
        tools,
        role_tools,
        handlers: Handlers::live(),
        templates: app.conversation_templates(),
        capability_error: app.capability_error(),
        preset_error: app.template_error(),
        workspaces: Some(workspaces),
        mode: Mode::Conversation,
        preset_id: None,
        initial_view: Page::Tools,
        window_title: None,
    };
    let pane = conversation_settings(app, props);
    Some(col().flex1().min_h(0.0).min_w(0.0).child(pane).child(template_switch_prompt(app)))
}

/// `App`'s dialog: the messages on this timeline will be replaced by the preset's opening queue.
pub fn template_switch_prompt(app: &mut App) -> Option<E> {
    let preset_id = app.template_switch_prompt()?;
    Some(plain_dialog(
        "template-switch",
        420.0,
        t("套用这份预设？", "Apply this preset?").to_owned(),
        Some(t("当前时间线上的消息会被这份预设的开局消息整体替换。", "The messages on this timeline will be replaced wholesale by the preset's opening messages.").to_owned()),
        Some(label(t("这段对话现在的内容不是某份预设的开局消息，替换后不会保留。", "This conversation's current content is not a preset's opening queue, and replacing it will not keep it."))),
        vec![
            btn("template-switch-cancel", Look::Standard, t("取消", "Cancel")).on_click(|app: &mut App| app.dismiss_template_switch_prompt()),
            btn("template-switch-replace", Look::Accent, t("替换", "Replace")).on_click(move |app: &mut App| {
                app.dismiss_template_switch_prompt();
                let preset = app.doc.presets.conversation_presets.iter().find(|p| p.id == preset_id).cloned();
                if let Some(preset) = preset {
                    app.apply_preset_body(preset);
                }
            }),
        ],
        Rc::new(|app: &mut App| app.dismiss_template_switch_prompt()),
    ))
}

pub fn conversation_settings(app: &mut App, props: ConversationSettingsProps) -> E {
    let t_ = theme();
    let scope = props.conversation.id.clone();
    let state = use_state(("conversation-settings", scope.as_str()), || PaneState::new(props.initial_view));
    let settings = props.conversation.settings.clone();
    let editing_preset = props.mode == Mode::Preset;
    let view = state.with(|s| s.view);
    let handlers = props.handlers.clone();

    // Opening the pane is one of the moments discovery runs, so the catalog it draws is what is on disk now rather than what startup saw. Once per mounted pane.
    if !state.with(|s| s.rescanned) {
        state.borrow_mut().rescanned = true;
        if let Some(rescan) = handlers.on_rescan.clone() {
            rescan(app);
        }
    }
    if handlers.on_fingerprint.is_some() && handlers.on_rescan.is_some() && !state.with(|s| s.watching) {
        state.borrow_mut().watching = true;
        let token = state.with(|s| s.watch_token.clone());
        watch_pane(app, &token, handlers.clone());
    }

    let catalog = Rc::new(app.doc.capabilities.clone());
    let presets = app.doc.presets.conversation_presets.clone();
    let provider_family = app.doc.assets.api_providers.iter().find(|p| Some(&p.id) == app.doc.global_settings.active_provider_id.as_ref()).map(|p| p.family);
    let image_input = model_choice_of(&app.doc).1.is_some_and(supports_vision);
    let lock_model = {
        let (provider, model) = model_choice_of(&app.doc);
        tool_lock_model_of(provider, model)
    };
    let lock_state = Rc::new(tool_lock_state(&settings, lock_model.as_ref(), now_ms()).with_called_tools(&props.conversation.contexts));
    // Redraw when the cache goes cold, so the orange goes with it. Nothing renders while idle, so the moment has to be asked for.
    if lock_state.warm
        && let Some(until) = lock_state.warm_until
        && state.with(|s| s.armed_until) != Some(until)
    {
        state.borrow_mut().armed_until = Some(until);
        let delay = std::time::Duration::from_millis((until - now_ms()).max(0) as u64 + 250);
        app.set_timer(delay, |_| repaint());
    }
    let lock = Rc::new(lock_state.lock.clone());

    // Counted over the same population the page divides by. Host-derived tools and the preview lifecycle ones follow switches, so they are not counted.
    let tool_names: Vec<&str> = {
        let mut seen = std::collections::HashSet::new();
        props.tools.iter().filter(|tool| tool.category != mewrk_client::model::ToolCategory::Memory && !is_host_derived_tool_name(&tool.name) && !is_preview_lifecycle_tool_name(&tool.name) && seen.insert(tool.name.as_str())).map(|tool| tool.name.as_str()).collect()
    };
    let valid_enabled = tool_names.iter().filter(|n| settings.enabled_tools.iter().any(|e| e == *n)).count();
    let agents = agent_resources(&catalog);
    let workspace_keys: Option<Vec<String>> = props.workspaces.as_ref().map(|w| w.iter().map(|x| x.key.clone()).collect());
    let role_count = selected_agent_role_count(&agents, &settings.agent_ids, workspace_keys.as_deref());
    // The tool-description page lists every profile but the built-in: that one is what selecting none means, so its id, or the retired guided one's, reads as no selection.
    let tool_description_files: Vec<ResourceDescriptor> = catalog.tool_description_files.iter().filter(|r| !is_builtin_prompt_profile_id(Some(r.id.as_str()))).cloned().collect();
    let tool_description_ids: Vec<String> = settings.tool_description_file_id.iter().filter(|id| !is_builtin_prompt_profile_id(Some(id.as_str()))).cloned().collect();

    let builtin_preset = editing_preset && props.preset_id.as_deref().is_some_and(is_builtin_conversation_preset);
    // A deleted preset leaves a dangling id, which reads as an unnamed draft.
    let applied_id = if presets.iter().any(|p| p.id == props.conversation.preset_id) { props.conversation.preset_id.clone() } else { String::new() };
    let open_template_id = if editing_preset { props.conversation.template_id.clone() } else { String::new() };

    // Read the body the first time the page is opened, and only then: the editor holds the draft, so re-reading would be a second chance to lose unsaved edits, not a refresh.
    if view == Page::Template && state.with(|s| s.template_read_for.as_deref() != Some(open_template_id.as_str())) {
        let id = open_template_id.clone();
        state.update(|s| {
            s.template_read_for = Some(id.clone());
            s.template_body = if id.is_empty() { Some(Vec::new()) } else { None };
        });
        if !id.is_empty() {
            let st = state.clone();
            let read = handlers.on_read_template.clone();
            // An unreadable body is an empty one to work from: the page still has to open, and saving overwrites whatever is there regardless.
            read(app, id, Box::new(move |_app, result| st.update(|s| s.template_body = Some(result.unwrap_or_default()))));
        }
    }

    let held_len = state.with(|s| s.held_template.as_ref().map(Vec::len));
    let count_of = |page: Page| -> Option<usize> {
        match page {
            Page::Tools => Some(valid_enabled),
            Page::Advanced => None,
            Page::Skills => Some(settings.skill_ids.len()),
            Page::Mcp => Some(settings.mcp_ids.len()),
            Page::Hooks => Some(settings.hook_ids.len()),
            Page::ToolDescriptions => Some(tool_description_ids.len()),
            Page::Roles => Some(role_count),
            Page::Template => Some(held_len.unwrap_or_else(|| props.templates.iter().find(|x| x.id == open_template_id).map_or(0, |x| x.message_count as usize))),
            Page::Presets => Some(presets.len()),
        }
    };
    let pages: Vec<Page> = NAVIGATION.iter().map(|(p, _)| *p).filter(|p| if editing_preset { *p != Page::Presets } else { *p != Page::Template }).collect();

    // The warning's guard. It reads the settings of this frame, so a dialog answered later commits on what is there by then.
    let commit: Rc<dyn Fn(&mut App, SettingsPatch, bool)> = {
        let (on_change, on_only, settings) = (handlers.on_change.clone(), handlers.on_change_conversation_only.clone(), settings.clone());
        Rc::new(move |app, patch, conversation_only| {
            if conversation_only {
                on_only(app, patch);
            } else {
                let mut next = settings.clone();
                patch(&mut next);
                on_change(app, next);
            }
        })
    };
    let guarded: Rc<dyn Fn(&mut App, SettingsPatch, bool)> = {
        let (commit, state, lock_state, settings, id) = (commit.clone(), state.clone(), lock_state.clone(), settings.clone(), scope.clone());
        Rc::new(move |app, patch, conversation_only| {
            let mut after = settings.clone();
            patch(&mut after);
            if lock_touch(&lock_state, &settings, &after) && should_warn_cache_break(app, &id) {
                state.update(|s| {
                    s.cache_break_never = false;
                    s.cache_break = Some((patch, conversation_only));
                });
                return;
            }
            commit(app, patch, conversation_only);
        })
    };
    let update = {
        let guarded = guarded.clone();
        move |app: &mut App, patch: SettingsPatch| guarded(app, patch, false)
    };
    let untick_dangling = {
        let (on_change, settings) = (handlers.on_change.clone(), settings.clone());
        move |kind: DanglingSelectionKind| -> Rc<dyn Fn(&mut App, String)> {
            let (on_change, settings) = (on_change.clone(), settings.clone());
            // Past the guard: a dangling selection fails every run, so clearing it is never a cache question and never refused.
            Rc::new(move |app, id| on_change(app, without_dangling_selection(&settings, kind, &id)))
        }
    };
    let list_id = |page: &str| format!("catalog:{scope}:{page}");
    let on_delete_of = |kind: CatalogKind| -> Option<Rc<dyn Fn(&mut App, ResourceDescriptor)>> {
        handlers.on_delete_capability.clone().map(|f| -> Rc<dyn Fn(&mut App, ResourceDescriptor)> { Rc::new(move |app, resource| f(app, kind, resource, Box::new(|_, _| {}))) })
    };
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let unreadable = catalog.unreadable_levels.clone();

    let capability = |kind: CatalogKind, wire: &str, resources: Vec<ResourceDescriptor>, selected: Vec<String>, tone_of: Rc<dyn Fn(&str, bool) -> Option<LockTone>>, on_change: Rc<dyn Fn(&mut App, Vec<String>)>, dangling: Rc<dyn Fn(&mut App, String)>| CapabilityPage {
        kind,
        list_id: list_id(wire),
        resources,
        selected_ids: selected,
        single: false,
        tone_of: Some(tone_of),
        on_change,
        on_untick_dangling: Some(dangling),
        on_delete: on_delete_of(kind),
        error: props.capability_error.clone(),
        unreadable_levels: unreadable.clone(),
        dangling_detail: None,
        selected_unavailable_detail: None,
        search_label: String::new(),
        empty_title: String::new(),
        empty_description: Hint::default(),
        footer: None,
        workspaces: props.workspaces.clone(),
        ssh_machines: ssh_machines.clone(),
        on_rescan: handlers.on_rescan.clone(),
        on_reveal: handlers.on_reveal.clone(),
        on_probe: None,
        row_actions: None,
        row_detail: None,
        row_badge: None,
    };
    let tone_list = |kind: LockedSettingKind, last: Vec<String>| -> Rc<dyn Fn(&str, bool) -> Option<LockTone>> {
        let lock_state = lock_state.clone();
        Rc::new(move |id: &str, selected: bool| lock_tone(&lock_state, kind, last.iter().any(|x| x == id), selected))
    };

    // A new entry is written to be used here, so it is selected, the way a new role is. An edit that made it another entry to the host takes this selection along. Either can move a warm cache, so both go through the warning; they land on the settings as they are once the save has rescanned.
    let select_saved = |field: fn(&mut Settings) -> &mut Vec<String>| -> Rc<dyn Fn(&mut App, Option<String>, String)> {
        let guarded = guarded.clone();
        Rc::new(move |app, old, new| {
            if old.as_ref() == Some(&new) {
                return;
            }
            let patch: SettingsPatch = {
                let (old, new) = (old.clone(), new.clone());
                Rc::new(move |s| {
                    let ids = field(s);
                    match &old {
                        Some(old) => {
                            if let Some(slot) = ids.iter_mut().find(|id| *id == old) {
                                slot.clone_from(&new);
                            }
                        }
                        None if ids.contains(&new) => {}
                        None => ids.push(new.clone()),
                    }
                })
            };
            guarded(app, patch, true);
        })
    };

    let page_body: E = match view {
        Page::Tools => {
            let u = update.clone();
            let tone: Rc<dyn Fn(&str, bool) -> Option<LockTone>> = {
                let (lock_state, last) = (lock_state.clone(), lock.tools.clone());
                Rc::new(move |id: &str, selected: bool| tool_tone(&lock_state, id, last.iter().any(|x| x == id), selected))
            };
            ToolGroups { scope: scope.clone(), tools: props.tools.clone(), enabled_tools: settings.enabled_tools.clone(), on_change: Rc::new(move |app, tools| u(app, Rc::new(move |s| s.enabled_tools = tools.clone()))), expansion_key: scope.clone(), tone_of: Some(tone) }.view()
        }
        Page::Advanced => {
            let remembered = remembered_legs(app, &format!("conversation:{scope}"));
            advanced_page(&scope, &settings, &lock_state, &guarded, provider_family, app.doc.assets.web_search.clone(), remembered)
        }
        Page::Skills => {
            let u = update.clone();
            let u2 = update.clone();
            let on = settings.skill_tool_enabled;
            let footer = SwitchRow {
                key: format!("{scope}:skill-tool"),
                title: t("技能按需加载", "Load skills on demand").to_owned(),
                description: t("只给模型技能清单，需要时再取正文。", "Shows the model a skill list; bodies load when needed.").to_owned(),
                docs: Some(("skills", Some("delivery"))),
                checked: on,
                label: if on { t("按需加载", "On demand") } else { t("拼进提示词", "In the prompt") }.to_owned(),
                tone: lock_tone(&lock_state, LockedSettingKind::SkillTool, lock.skill_tool, on),
                disabled: false,
                disabled_note: None,
                control: None,
                on_change: Rc::new(move |app, v| u2(app, Rc::new(move |s| s.skill_tool_enabled = v))),
            }
            .view();
            let page = CapabilityPage {
                search_label: t("搜索技能", "Search skills").to_owned(),
                empty_title: t("尚未发现技能", "No skills discovered").to_owned(),
                empty_description: Hint::new(t("点下面的「新建技能」即可添加。", "Press New skill below to add one.")).docs("skills", "location"),
                footer: Some(footer),
                ..capability(CatalogKind::Skills, "skills", catalog.skills.clone(), settings.skill_ids.clone(), tone_list(LockedSettingKind::Skill, lock.skill_ids.clone()), Rc::new(move |app, ids| u(app, Rc::new(move |s| s.skill_ids = ids.clone()))), untick_dangling(DanglingSelectionKind::Skills))
            };
            editable_capability_page(app, page, &handlers, t("新建技能", "New skill"), select_saved(|s| &mut s.skill_ids))
        }
        Page::Mcp => {
            let u = update.clone();
            let u2 = update.clone();
            // A model that cannot take a tool mid-conversation has no way to hand over a fetched schema, so it is not offered discovery. The host ignores the setting too.
            let discovery_available = lock_model.as_ref().is_none_or(|m| m.appends_tools);
            let on = settings.mcp_tool_discovery_enabled;
            let footer = SwitchRow {
                key: format!("{scope}:tool-discovery"),
                title: t("工具发现", "Tool discovery").to_owned(),
                description: t("只报工具名，需要时再取回 schema。", "Lists tool names only; schemas are fetched when needed.").to_owned(),
                docs: Some(("mcp", Some("tool-discovery"))),
                checked: discovery_available && on,
                label: if on { t("按需取回", "On demand") } else { t("全部声明", "All declared") }.to_owned(),
                tone: lock_tone(&lock_state, LockedSettingKind::Discovery, lock.mcp_tool_discovery, on),
                disabled: !discovery_available,
                disabled_note: Some(t("当前模型不支持中途追加工具，取回的 schema 无处交付，所以不提供工具发现。", "The selected model cannot take a tool mid-conversation, so a fetched schema would have nowhere to go; tool discovery is not offered.").to_owned()),
                control: None,
                on_change: Rc::new(move |app, v| u2(app, Rc::new(move |s| s.mcp_tool_discovery_enabled = v))),
            }
            .view();
            let page = CapabilityPage {
                search_label: t("搜索 MCP Server", "Search MCP servers").to_owned(),
                empty_title: t("尚未发现 MCP Server", "No MCP servers discovered").to_owned(),
                empty_description: Hint::new(t("点下面的「新建 MCP Server」即可添加。", "Press New MCP server below to add one.")).docs("mcp", "location"),
                footer: Some(footer),
                on_probe: handlers.on_probe.clone(),
                ..capability(CatalogKind::Mcp, "mcp", catalog.mcps.clone(), settings.mcp_ids.clone(), tone_list(LockedSettingKind::Mcp, lock.mcp_ids.clone()), Rc::new(move |app, ids| u(app, Rc::new(move |s| s.mcp_ids = ids.clone()))), untick_dangling(DanglingSelectionKind::Mcp))
            };
            editable_capability_page(app, page, &handlers, t("新建 MCP Server", "New MCP server"), select_saved(|s| &mut s.mcp_ids))
        }
        Page::Hooks => {
            let u = update.clone();
            // The system prompt lists the selected hooks. A lock from before they were recorded knows nothing to tone.
            let tone: Rc<dyn Fn(&str, bool) -> Option<LockTone>> = match lock.hook_ids.clone() {
                None => Rc::new(|_, _| None),
                Some(last) => tone_list(LockedSettingKind::Hook, last),
            };
            let page = CapabilityPage {
                search_label: t("搜索钩子", "Search hooks").to_owned(),
                empty_title: t("尚未发现钩子", "No hooks discovered").to_owned(),
                empty_description: Hint::new(t("点下面的「新建钩子」即可添加。", "Press New hook below to add one.")).docs("hooks", "location"),
                ..capability(CatalogKind::Hooks, "hooks", catalog.hooks.clone(), settings.hook_ids.clone(), tone, Rc::new(move |app, ids| u(app, Rc::new(move |s| s.hook_ids = ids.clone()))), untick_dangling(DanglingSelectionKind::Hooks))
            };
            editable_capability_page(app, page, &handlers, t("新建钩子", "New hook"), select_saved(|s| &mut s.hook_ids))
        }
        // One file words the whole run, so the page picks one: another row replaces it, and unticking it goes back to Mewrk concise. There is no workspace level: the files are read from ~/.mewrk alone.
        Page::ToolDescriptions => {
            let u = update.clone();
            let (ls, s2) = (lock_state.clone(), settings.clone());
            let (on_change, s3) = (handlers.on_change.clone(), settings.clone());
            // Whichever row is clicked, the profile the cache was worded with changes, so the lock draws every row alike.
            let tone: Rc<dyn Fn(&str, bool) -> Option<LockTone>> = Rc::new(move |_, _| prompt_profile_tone(&ls, &s2));
            CapabilityPage {
                kind: CatalogKind::ToolDescriptions,
                list_id: list_id("toolDescriptions"),
                resources: tool_description_files,
                selected_ids: tool_description_ids.clone(),
                single: true,
                tone_of: Some(tone),
                on_change: Rc::new(move |app, ids| u(app, Rc::new(move |s| s.tool_description_file_id = ids.first().cloned()))),
                // A dangling file was never read: the host already worded the run with Mewrk concise, so going back to it rewrites nothing.
                on_untick_dangling: Some(Rc::new(move |app, _| {
                    let mut next = s3.clone();
                    next.tool_description_file_id = None;
                    on_change(app, next);
                })),
                on_delete: None,
                error: None,
                unreadable_levels: Vec::new(),
                dangling_detail: Some(t("目录中已不存在；运行时用的是内置的 Mewrk concise", "No longer in the catalog. Runs use the built-in Mewrk concise instead.").to_owned()),
                selected_unavailable_detail: Some(t("已选择，但文件里没有可用条目；运行时用的是内置的 Mewrk concise", "Selected, but the file has no usable entries. Runs use the built-in Mewrk concise instead.").to_owned()),
                search_label: t("搜索工具描述", "Search tool descriptions").to_owned(),
                empty_title: t("尚未发现工具描述文件", "No tool-description files discovered").to_owned(),
                empty_description: Hint::new(t("添加工具描述文件后自动列出。", "Tool-description files you add are listed automatically.")).docs("prompt-profiles", "locations"),
                footer: Some(small(t("选一份；不选则用内置的 Mewrk concise。", "Pick one; with none, the built-in Mewrk concise is used."))),
                workspaces: Some(Vec::new()),
                ssh_machines: Vec::new(),
                on_rescan: handlers.on_rescan.clone(),
                on_reveal: handlers.on_reveal.clone(),
                on_probe: None,
                row_actions: None,
                row_detail: None,
                row_badge: Some(Rc::new(|r| (r.source == mewrk_client::model::ResourceSource::Builtin).then(|| Badge::plain(t("内置", "Built-in"))))),
            }
            .view(app)
        }
        Page::Roles => {
            let (u1, u2) = (update.clone(), update.clone());
            let on_only = handlers.on_change_conversation_only.clone();
            RolesPage {
                list_id: list_id("roles"),
                agent_ids: settings.agent_ids.clone(),
                allow_roleless: settings.allow_roleless_subagents,
                on_select: Rc::new(move |app, ids| u1(app, Rc::new(move |s| s.agent_ids = ids.clone()))),
                on_allow_roleless: Rc::new(move |app, v| u2(app, Rc::new(move |s| s.allow_roleless_subagents = v))),
                // A role written from here is meant to be used here: creating one also selects it. The save resolves after a rescan, so this has to land on the settings as they are by then,
                // not as they were when Save was pressed: hence a patch. Roles take part in no lock, so nothing about this is a cache question.
                on_created: Rc::new(move |app, id| {
                    on_only(app, Rc::new(move |s| {
                        s.agent_ids.retain(|e| *e != id);
                        s.agent_ids.push(id.clone());
                    }));
                }),
                on_untick_dangling: Some(untick_dangling(DanglingSelectionKind::Agents)),
                role_tools: props.role_tools.clone(),
                workspaces: props.workspaces.clone(),
                handlers: handlers.clone(),
                templates: props.templates.clone(),
                error: props.capability_error.clone(),
            }
            .view(app)
        }
        // The preset's own message queue. Each edit is written back as it lands, and saving mints the id when the preset has none and binds it immediately: the body is already on disk by then,
        // so deferring the binding to the window's Save would be a moment in which abandoning the window stranded it.
        Page::Template => {
            let contexts = state.with(|s| s.held_template.clone().or_else(|| s.template_body.clone()));
            let u = update.clone();
            let enabled = Rc::new(settings.enabled_tools.clone());
            let on_save: Rc<dyn Fn(&mut App, Vec<ContextItem>, ops::Done<()>)> = if builtin_preset {
                let st = state.clone();
                Rc::new(move |app, contexts, done| {
                    st.update(|s| s.held_template = Some(contexts));
                    done(app, Ok(()));
                })
            } else {
                let (st, write, bind, preset_id, id_now) = (state.clone(), handlers.on_write_template.clone(), handlers.on_bind_preset_template.clone(), props.preset_id.clone(), open_template_id.clone());
                Rc::new(move |app, contexts: Vec<ContextItem>, done| {
                    let (st, bind, preset_id, id_now2, held) = (st.clone(), bind.clone(), preset_id.clone(), id_now.clone(), contexts.clone());
                    write(app, id_now.clone(), contexts, Box::new(move |app, result| match result {
                        Ok(saved) => {
                            st.update(|s| {
                                s.template_body = Some(held);
                                s.template_read_for = Some(saved.clone());
                            });
                            if saved != id_now2
                                && let (Some(p), Some(bind)) = (preset_id, bind)
                            {
                                bind(app, p, saved);
                            }
                            done(app, Ok(()));
                        }
                        Err(reason) => done(app, Err(reason)),
                    }));
                })
            };
            TemplateEditor {
                key: format!("pane:{scope}"),
                template_id: open_template_id.clone(),
                contexts,
                tools: props.tools.clone(),
                enabled_tools: settings.enabled_tools.clone(),
                editable: true,
                image_input_supported: image_input,
                autosave: true,
                on_save: Some(on_save),
                on_enable_tools: Some(Rc::new(move |app, names| {
                    let mut all = (*enabled).clone();
                    for n in names {
                        if !all.contains(&n) {
                            all.push(n);
                        }
                    }
                    u(app, Rc::new(move |s| s.enabled_tools = all.clone()));
                })),
            }
            .view(app)
        }
        Page::Presets => {
            let (apply, rename, delete) = (handlers.on_apply_preset.clone(), handlers.on_rename_preset.clone(), handlers.on_delete_preset.clone());
            let (st, all, conv_settings) = (state.clone(), presets.clone(), settings.clone());
            PresetsPage {
                list_id: list_id("presets"),
                presets: presets.clone(),
                applied_id,
                error: props.preset_error.clone(),
                on_apply: apply,
                on_open: Rc::new(move |_, id| {
                    let Some(preset) = all.iter().find(|p| p.id == id) else { return };
                    st.update(|s| s.preset_draft = Some(PresetDraft { id: preset.id.clone(), settings: preset_body_as_settings(preset, &conv_settings), view: None }));
                }),
                on_rename: Rc::new(move |app, id, name| {
                    if let Some(f) = &rename {
                        f(app, id, name);
                    }
                }),
                on_delete: Rc::new(move |app, id| {
                    if let Some(f) = &delete {
                        f(app, id);
                    }
                }),
            }
            .view(app)
        }
    };

    // The template page is a timeline, which brings its own scroller and its own edges, so that page gets the body flush.
    let flush = view == Page::Template;
    let save_label = if editing_preset && !builtin_preset {
        t("保存预设", "Save preset")
    } else if builtin_preset {
        t("另存为新预设", "Save as new preset")
    } else {
        t("另存为预设", "Save as preset")
    };
    let save: Rc<dyn Fn(&mut App)> = {
        let st = state.clone();
        let on_save_as = handlers.on_save_as_preset.clone();
        let save_key = format!("save-as-name:{scope}");
        Rc::new(move |app: &mut App| {
            if editing_preset {
                let held = if builtin_preset { st.with(|s| s.held_template.clone()) } else { None };
                if let Some(f) = &on_save_as {
                    f(app, held);
                }
            } else {
                st.update(|s| s.save_as = Some(SaveAs::default()));
                focus_request(("field", save_key.as_str()));
            }
        })
    };
    let nav_items: Vec<NavItem<Page>> = pages.iter().map(|p| NavItem { id: *p, icon: NAVIGATION.iter().find(|(x, _)| x == p).map_or("Box", |(_, i)| i), label: page_title(*p).to_owned(), count: count_of(*p) }).collect();
    let in_window = props.window_title.is_some();
    let save_btn = nav_action(btn(("settings-save", scope.as_str()), Look::Subtle, save_label), in_window).on_click({
        let save = save.clone();
        move |app: &mut App| save(app)
    });
    let st_nav = state.clone();
    // The pane can be dragged down to the side column's floor, far narrower than a window: there the page list gives way, so the body keeps a readable column.
    let pane_rect = rect_of(("conversation-settings-pane", scope.as_str()));
    if pane_rect.is_none() {
        // Its width is known from the first frame on, and nothing else would ask for the frame that uses it.
        repaint();
    }
    let narrow = !in_window && pane_rect.is_some_and(|r| r.w <= 430.0);
    let nav_style = if in_window {
        NavStyle::Window
    } else if narrow {
        NavStyle::NarrowPane
    } else {
        NavStyle::Pane
    };
    let nav = settings_nav(&scope, t("对话设置分类", "Conversation settings categories"), nav_items, view, nav_style, props.window_title.clone(), Rc::new(move |_, p| st_nav.update(|s| s.view = p)), Some(save_btn));

    let docs = page_docs_section(view).map(|section| docs_link("working", Some(section)));
    let note = builtin_preset.then(|| small(t("内置预设不能修改，改动可另存为新预设。", "The built-in preset is read-only; save changes as a new one.")).ty(CAPTION).color(t_.text_secondary));
    let page = if in_window {
        // In a window the page is a global settings page: it spans the page's column as theirs do, with no measure of its own.
        window_page(("settings-page-scroll", scope.as_str(), view), window_page_blurb(page_blurb(view)), docs, note, page_body, flush)
    } else {
        // The blurb takes the body's measure, so the link's right edge is the one the rows beneath it end on.
        let (edge, top, bottom) = if narrow { (10.0, 10.0, 9.0) } else { (14.0, 9.0, 8.0) };
        let heading = row().items_center().gap(10.0).min_w(0.0).w_full().max_w(680.0).mx_auto().child(small(page_blurb(view)).flex1().min_w(0.0)).child(docs);
        let stack = if flush { col().flex1().min_h(0.0).min_w(0.0).child(page_body) } else { col().flex1().min_h(0.0).min_w(0.0).scroll_y().key(("settings-page-scroll", scope.as_str(), view)).px(edge).pt(if narrow { 10.0 } else { 12.0 }).pb(if narrow { 18.0 } else { 20.0 }).child(col().w_full().max_w(680.0).mx_auto().min_w(0.0).child(page_body)) };
        col().flex1().min_w(0.0).min_h(0.0).child(col().px(edge).pt(top).pb(bottom).gap(4.0).border_bottom(t_.divider).child(heading).child(note)).child(stack)
    };

    let frame = if in_window && window_strip() { col() } else { row() };
    let page = if in_window && window_strip() { page.border_top(t_.divider) } else { page };
    let mut pane = frame.key(("conversation-settings-pane", scope.as_str())).flex1().min_w(0.0).min_h(0.0).when(in_window, |e| e.role(Role::Region).aria_label(if editing_preset { t("预设设置", "Preset settings") } else { t("对话设置", "Conversation settings") })).child(nav).child(page);
    pane = pane.child(cache_break_dialog(&state, &scope, &commit));
    pane = pane.child(save_as_dialog(&state, &scope, &props));
    pane = pane.child(preset_window(app, &state, &props, &presets));
    pane
}

/// The advanced tools page, as a conversation or a preset's window answers it.
fn advanced_page(scope: &str, settings: &Settings, lock_state: &Rc<ToolLockState>, guarded: &Rc<dyn Fn(&mut App, SettingsPatch, bool)>, family: Option<mewrk_client::model::ProviderFamily>, assets: mewrk_client::model::WebSearchAssets, remembered: crate::web::RememberedWebLegs) -> E {
    let lock = &lock_state.lock;
    // Whether the model this conversation runs on can retrieve a page with its own provider's tool. Read from the active provider, since a conversation carries no model binding.
    let native_fetch = family_supports_native_fetch(family);
    let backend_lock = |leg: WebLeg, current: WebBackendChoice| -> Option<BackendLock> {
        if backend_pinned(lock, leg) {
            return Some(BackendLock::Settled { note: settled_backend_hint().to_owned() });
        }
        backend_tone(lock_state, leg, current).map(|tone| BackendLock::Cache { note: lock_hint(tone).to_owned() })
    };
    let web = &settings.web_search;
    let access = Some(settings.web_search_enabled);
    let g = guarded.clone();
    let on_web: Rc<dyn Fn(&mut App, WebPatch)> = Rc::new(move |app, patch: WebPatch| g(app, Rc::new(move |s| patch(&mut s.web_search)), true));
    let g2 = guarded.clone();
    let write_access: Rc<dyn Fn(&mut App, bool, crate::web::WebLegPatch)> = Rc::new(move |app, enabled, patch| {
        g2(app, Rc::new(move |s| {
            s.web_search_enabled = enabled;
            patch.apply(&mut s.web_search);
        }), true);
    });
    let section = WebSection {
        scope: scope.to_owned(),
        value: web.clone(),
        on_change: on_web,
        web_access: Some((settings.web_search_enabled, write_access)),
        web_search_assets: assets,
        subject: WebSubject::Conversation,
        native_fetch_available: native_fetch,
        native_tool_type_selectable: family_selects_native_tool_type(family),
        search_lock: backend_lock(WebLeg::Search, WebBackendChoice::from(&web.provider)),
        fetch_lock: backend_lock(WebLeg::Fetch, WebBackendChoice::from(&web.fetch_provider)),
        search_tone: web_leg_tone(lock_state, WebLeg::Search, web_leg_on(WebLeg::Search, access, web, native_fetch)),
        fetch_tone: web_leg_tone(lock_state, WebLeg::Fetch, web_leg_on(WebLeg::Fetch, access, web, native_fetch)),
        remember_key: Some(format!("conversation:{scope}")),
        remembered,
    };
    let update = |write: fn(&mut Settings, bool)| -> Rc<dyn Fn(&mut App, bool)> {
        let g = guarded.clone();
        Rc::new(move |app, v| g(app, Rc::new(move |s| write(s, v)), false))
    };
    let g3 = guarded.clone();
    AdvancedPage {
        scope: scope.to_owned(),
        web: section,
        memory: Some((
            Flag { enabled: settings.global_memory_enabled, tone: lock_tone(lock_state, LockedSettingKind::Memory, lock.global_memory, settings.global_memory_enabled), on_change: update(|s, v| s.global_memory_enabled = v) },
            Flag { enabled: settings.project_memory_enabled, tone: lock_tone(lock_state, LockedSettingKind::Memory, lock.project_memory, settings.project_memory_enabled), on_change: update(|s, v| s.project_memory_enabled = v) },
        )),
        file_write_guards: Some(Flag { enabled: settings.file_write_guards_enabled, tone: file_write_guards_tone(lock_state, settings), on_change: update(|s, v| s.file_write_guards_enabled = v) }),
        subagent_messaging: Some(Flag { enabled: settings.subagent_messaging_enabled, tone: subagent_messaging_tone(lock_state, settings), on_change: update(|s, v| s.subagent_messaging_enabled = v) }),
        environment_prompt: Some(Flag { enabled: settings.environment_prompt_enabled, tone: environment_prompt_tone(lock_state, settings), on_change: update(|s, v| s.environment_prompt_enabled = v) }),
        host_messages: Some((settings.host_message_container, host_message_container_tone(lock_state, settings), Rc::new(move |app, value: HostMessageContainer| g3(app, Rc::new(move |s| s.host_message_container = value), false)))),
    }
    .view()
}

/// The one warning every orange row shares: said once per conversation, or never again. The change waits as a patch and lands on the settings as they are when the user answers.
fn cache_break_dialog(state: &Local<PaneState>, scope: &str, commit: &Rc<dyn Fn(&mut App, SettingsPatch, bool)>) -> Option<E> {
    state.with(|s| s.cache_break.is_some()).then(|| {
        let never = state.with(|s| s.cache_break_never);
        let (st1, st2, st3, st4) = (state.clone(), state.clone(), state.clone(), state.clone());
        let (commit, id) = (commit.clone(), scope.to_owned());
        let body = col()
            .gap(8.0)
            .child(row().wrap().items_center().gap(4.0).child(label(t("改动橘色设置会让缓存失效，整段对话要重新计费。", "Changing an orange setting breaks the cache and re-bills the conversation."))).child(docs_inline("working", Some("tool-lock"))))
            .child(check_row("cache-break-never", never, false, t("不再显示", "Don't show this again"), None, move |_| st4.update(|s| s.cache_break_never = !s.cache_break_never)));
        plain_dialog(
            ("cache-break", scope),
            440.0,
            t("这样改会让缓存失效", "This change throws the cache away").to_owned(),
            None,
            Some(body),
            vec![
                btn("cache-break-cancel", Look::Standard, t("取消", "Cancel")).on_click(move |_| st1.update(|s| s.cache_break = None)),
                btn("cache-break-confirm", Look::Accent, t("仍然更改", "Change anyway")).on_click(move |app: &mut App| {
                    let (pending, never) = st2.with(|s| (s.cache_break.clone(), s.cache_break_never));
                    acknowledge_cache_break(app, &id, never);
                    st2.update(|s| s.cache_break = None);
                    if let Some((patch, only)) = pending {
                        commit(app, patch, only);
                    }
                }),
            ],
            Rc::new(move |_| st3.update(|s| s.cache_break = None)),
        )
    })
}

/// "Save as preset" asks for a name, and whether the conversation's own timeline becomes the new preset's template. Either way the new preset opens at its template: the timeline it took, or an empty one to write.
fn create_preset_now(app: &mut App, state: &Local<PaneState>, props: &ConversationSettingsProps) {
    let Some(on_create) = props.handlers.on_create_preset.clone() else { return };
    let Some(request) = state.with(|s| s.save_as.clone()).filter(|r| !r.saving) else { return };
    let name = request.name.trim().to_owned();
    if name.is_empty() {
        return;
    }
    let capture = request.capture_template && !props.conversation.contexts.is_empty();
    state.update(|s| {
        if let Some(r) = &mut s.save_as {
            r.saving = true;
            r.error = None;
        }
    });
    let (st, settings) = (state.clone(), props.conversation.settings.clone());
    on_create(app, name, capture, Box::new(move |_app, result: Result<ConversationPreset, String>| match result {
        Ok(preset) => st.update(|s| {
            s.save_as = None;
            s.preset_draft = Some(PresetDraft { id: preset.id.clone(), settings: preset_body_as_settings(&preset, &settings), view: Some(Page::Template) });
        }),
        Err(reason) => st.update(|s| {
            if let Some(r) = &mut s.save_as {
                r.saving = false;
                r.error = Some(reason);
            }
        }),
    }));
}

fn save_as_dialog(state: &Local<PaneState>, scope: &str, props: &ConversationSettingsProps) -> Option<E> {
    let request = state.with(|s| s.save_as.clone())?;
    // A preset can only open with a timeline the conversation has.
    let can_capture = !props.conversation.contexts.is_empty();
    let name_key = format!("save-as-name:{scope}");
    let (st_name, st_check, st_close1, st_close2, st_submit, st_save) = (state.clone(), state.clone(), state.clone(), state.clone(), state.clone(), state.clone());
    let (props1, props2) = (Rc::new(props.clone()), Rc::new(props.clone()));
    let name_field = field(name_key.as_str(), request.name.clone())
        .placeholder(t("例如：代码评审", "e.g. Code review"))
        .on_change(move |_, v| st_name.update(|s| {
            if let Some(r) = &mut s.save_as {
                r.name = v;
            }
        }))
        .on_submit(move |app, _| create_preset_now(app, &st_submit, &props1))
        .build()
        .aria_label(t("预设名称", "Preset name"));
    let body = col()
        .gap(10.0)
        .child(field_block(t("预设名称", "Preset name"), name_field, None))
        .child(check_row(("save-as-capture", scope), request.capture_template && can_capture, !can_capture, t("将当前上下文作为对话模板", "Use the current context as the conversation template"), (!can_capture).then_some(t("这段对话还没有消息", "This conversation has no messages yet")), move |_| st_check.update(|s| {
            if let Some(r) = &mut s.save_as {
                r.capture_template = !r.capture_template;
            }
        })))
        .child(request.error.clone().map(|e| error_text(e).role(Role::Alert)));
    let disabled = request.name.trim().is_empty() || request.saving;
    Some(plain_dialog(
        ("save-as", scope),
        560.0,
        t("另存为预设", "Save as preset").to_owned(),
        None,
        Some(body),
        vec![
            btn(("save-as-cancel", scope), Look::Subtle, t("取消", "Cancel")).on_click(move |_| st_close1.update(|s| s.save_as = None)),
            btn(("save-as-confirm", scope), Look::Accent, if request.saving { t("正在保存…", "Saving…") } else { t("保存为预设", "Save as preset") }).disabled(disabled).when(disabled, |e| e.opacity(0.5)).on_click(move |app: &mut App| create_preset_now(app, &st_save, &props2)),
        ],
        Rc::new(move |_| st_close2.update(|s| s.save_as = None)),
    ))
}

/// A preset opens into this same pane rather than a second editor: they would otherwise have to be kept in step by hand forever. It is handed no workspaces: a preset is reusable and points at none in particular.
fn preset_window(app: &mut App, state: &Local<PaneState>, props: &ConversationSettingsProps, presets: &[ConversationPreset]) -> Option<E> {
    let draft = state.with(|s| s.preset_draft.clone())?;
    let opened = presets.iter().find(|p| p.id == draft.id)?;
    let h = &props.handlers;
    let (st_change, st_only, st_save, st_close) = (state.clone(), state.clone(), state.clone(), state.clone());
    let draft_id = draft.id.clone();
    let (on_save_preset, on_save_copy) = (h.on_save_preset.clone(), h.on_save_preset_copy.clone());
    let handlers = Handlers {
        on_change: Rc::new(move |_, next| st_change.update(|s| {
            if let Some(d) = &mut s.preset_draft {
                d.settings = next;
            }
        })),
        on_change_conversation_only: Rc::new(move |_, patch| st_only.update(|s| {
            if let Some(d) = &mut s.preset_draft {
                patch(&mut d.settings);
            }
        })),
        on_create_preset: None,
        on_fingerprint: None,
        on_rename_preset: None,
        on_delete_preset: None,
        on_save_preset: None,
        on_save_preset_copy: None,
        on_save_as_preset: Some(Rc::new(move |app, template_body| {
            let Some(settings) = st_save.with(|s| s.preset_draft.as_ref().map(|d| d.settings.clone())) else { return };
            let body = capture_conversation_preset_settings(&settings);
            if is_builtin_conversation_preset(&draft_id) {
                if let Some(f) = &on_save_copy {
                    f(app, draft_id.clone(), body, template_body);
                }
            } else if let Some(f) = &on_save_preset {
                f(app, draft_id.clone(), body);
            }
            st_save.update(|s| s.preset_draft = None);
        })),
        ..h.clone()
    };
    let mut conversation = props.conversation.clone();
    conversation.id = format!("preset:{}", draft.id);
    conversation.preset_id = String::new();
    // The preset's own binding, read live rather than from the draft: saving a body binds the id at once, so the draft is not where that fact lives.
    conversation.template_id = opened.template_id.clone();
    conversation.settings = draft.settings.clone();
    let title = if opened.name.is_empty() { untitled_preset().to_owned() } else { opened.name.clone() };
    let inner = conversation_settings(
        app,
        ConversationSettingsProps {
            conversation,
            handlers,
            workspaces: None,
            mode: Mode::Preset,
            preset_id: Some(draft.id.clone()),
            initial_view: draft.view.unwrap_or(Page::Tools),
            window_title: Some(title.clone()),
            ..props.clone()
        },
    );
    Some(settings_window(("preset-window", draft.id.as_str()), &title, 1040.0, true, false, Rc::new(move |_| st_close.update(|s| s.preset_draft = None)), div(), inner))
}
