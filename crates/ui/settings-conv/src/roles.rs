//! `AgentRolesPage` and `ConversationPresetsPage` from `ConversationSettingsPages.tsx`, and `AgentRoleEditor.tsx`.
//!
//! A role is a file like a skill is, so its page is the same catalog page and the row's switch means the same thing: this conversation offers that role to the model.
//! Roles take part in no lock: a role's settings are no part of the calling conversation's prompt cache, so a change simply reaches the next subagent.

use std::collections::HashSet;

use mewrk_client::model::{AgentModelSelection, ApiProvider, CapabilityCatalog, ContextItem, ConversationPreset, ReasoningEffort, ResourceDescriptor, ToolCategory, ToolDescriptor, WebSearchAssets};
use mewrk_client::prelude::*;
use mewrk_client::tool_lock::{BUILTIN_PROMPT_PROFILE_ID, RETIRED_GUIDED_PROMPT_PROFILE_ID};

use crate::advanced::{remembered_legs, AdvancedPage, WebPatch, WebSection, WebSubject};
use crate::catalog::{catalog_list, Badge, CapabilityPage, CatalogKind, CatalogRow};
use crate::logic::*;
use crate::ops::{Done, Handlers, SaveAgentRoleTarget, TemplateSummary};
use crate::template_editor::TemplateEditor;
use crate::tools::ToolGroups;
use crate::web::{family_selects_native_tool_type, family_supports_native_fetch};
use crate::widgets::*;
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuItem, PopoverMenuSection};

// ---------------------------------------------------------------- the roles page

pub struct RolesPage {
    pub list_id: String,
    pub agent_ids: Vec<String>,
    pub allow_roleless: bool,
    pub on_select: Rc<dyn Fn(&mut App, Vec<String>)>,
    pub on_allow_roleless: Rc<dyn Fn(&mut App, bool)>,
    pub on_created: Rc<dyn Fn(&mut App, String)>,
    pub on_untick_dangling: Option<Rc<dyn Fn(&mut App, String)>>,
    /// The whole trusted tool catalogue, never a conversation's own list: a role is a global or project asset every conversation may select.
    pub role_tools: Rc<Vec<ToolDescriptor>>,
    /// `None` for a preset's window, which points at no workspace.
    pub workspaces: Option<Vec<CapabilityWorkspace>>,
    pub handlers: Handlers,
    pub templates: Vec<TemplateSummary>,
    pub error: Option<String>,
}

/// Which role window is open over the page. An edit holds the entry as it was opened: a rescan that no longer finds it must not close the window under the user.
#[derive(Clone)]
enum OpenRoleEditor {
    Create,
    Edit(Rc<AgentRoleResource>),
}

impl RolesPage {
    pub fn view(self, app: &mut App) -> E {
        let t_ = theme();
        let editor = use_state(("role-editor-open", self.list_id.as_str()), || None::<OpenRoleEditor>);
        let catalog = app.doc.capabilities.clone();
        let providers: Rc<Vec<ApiProvider>> = Rc::new(app.doc.assets.api_providers.clone());
        let agents = Rc::new(agent_resources(&catalog));
        let workspace_keys: Option<Vec<String>> = self.workspaces.as_ref().map(|w| w.iter().map(|x| x.key.clone()).collect());
        let has_usable = has_usable_agent_role(&agents, &self.agent_ids, &providers, workspace_keys.as_deref());
        let resources: Vec<ResourceDescriptor> = agents.iter().map(|a| a.descriptor.clone()).collect();

        let (detail_agents, detail_providers) = (agents.clone(), providers.clone());
        let (badge_agents, badge_providers) = (agents.clone(), providers.clone());
        let (action_agents, action_editor, action_list) = (agents.clone(), editor.clone(), self.list_id.clone());
        let on_delete = self.handlers.on_delete_capability.clone().map(|f| -> Rc<dyn Fn(&mut App, ResourceDescriptor)> {
            Rc::new(move |app, resource: ResourceDescriptor| {
                let id = resource.id.clone();
                // A draft of a role whose file is gone would reopen on nothing, but only once it is known to be gone: a refused delete leaves the half-written edit in place.
                f(
                    app,
                    CatalogKind::Agents,
                    resource,
                    Box::new(move |app, removed| {
                        if removed {
                            forget_agent_role_draft(app, &id);
                        }
                    }),
                );
            })
        });
        let new_editor = editor.clone();
        let (allow, on_allow) = (self.allow_roleless, self.on_allow_roleless.clone());
        let hint = if has_usable { t("不指定角色的子代理沿用本对话的模型。", "A subagent with no role uses this conversation's model.") } else { t("当前没有可用角色，开关暂不生效。", "No usable role yet, so this switch has no effect.") };
        let footer = col()
            .gap(14.0)
            .child(row().key(("agent-role-add", self.list_id.as_str())).center().gap(5.0).min_h(34.0).w_full().radius(7.0).border(t_.control_stroke).hover_bg(t_.subtle_hover).on_click(move |_| new_editor.set(Some(OpenRoleEditor::Create))).child(icon("Plus", 14.0).color(t_.text_secondary)).child(label(t("新建角色", "New role"))))
            .child(
                SwitchRow {
                    key: format!("{}:roleless", self.list_id),
                    title: t("允许无角色子代理", "Allow role-less subagents").to_owned(),
                    description: hint.to_owned(),
                    docs: Some(("agents", Some("roles"))),
                    checked: allow,
                    label: if allow { t("角色可选", "Role optional") } else { t("角色必填", "Role required") }.to_owned(),
                    tone: None,
                    disabled: false,
                    disabled_note: None,
                    control: None,
                    on_change: Rc::new(move |app, on| on_allow(app, on)),
                }
                .view(),
            );
        let page = CapabilityPage {
            kind: CatalogKind::Agents,
            list_id: self.list_id.clone(),
            resources,
            selected_ids: self.agent_ids.clone(),
            single: false,
            tone_of: None,
            on_change: self.on_select.clone(),
            on_untick_dangling: self.on_untick_dangling.clone(),
            on_delete,
            error: self.error.clone(),
            unreadable_levels: catalog.unreadable_levels.clone(),
            dangling_detail: None,
            selected_unavailable_detail: None,
            search_label: t("搜索角色", "Search roles").to_owned(),
            empty_title: t("尚未发现角色", "No roles discovered").to_owned(),
            empty_description: Hint::new(t("点下面的「新建角色」即可添加。", "Press New role below to add one.")).docs("agents", "roles"),
            footer: Some(footer),
            workspaces: self.workspaces.clone(),
            ssh_machines: app.doc.assets.execution_environments.ssh_machines.clone(),
            on_rescan: self.handlers.on_rescan.clone(),
            on_reveal: self.handlers.on_reveal.clone(),
            on_probe: None,
            row_detail: Some(Rc::new(move |r| detail_agents.iter().find(|a| a.descriptor.id == r.id)?.role.as_ref().map(|role| agent_role_model_label(&role.model_selection, &detail_providers)))),
            row_badge: Some(Rc::new(move |r| {
                let role = badge_agents.iter().find(|a| a.descriptor.id == r.id)?.role.as_ref()?;
                (!agent_model_selection_is_available(&role.model_selection, &badge_providers)).then(|| Badge::warning(t("模型不可用", "No model")))
            })),
            // An unreadable file has no body to open; its reason is under its name, and the file is fixed where it lives.
            row_actions: Some(Rc::new(move |r| {
                let Some(agent) = action_agents.iter().find(|a| a.descriptor.id == r.id).filter(|a| a.role.is_some()) else { return Vec::new() };
                let (ed, res) = (action_editor.clone(), Rc::new(agent.clone()));
                vec![icon_btn(("role-configure", action_list.as_str(), r.id.as_str()), "Settings2", tf("设置角色 {name}", "Configure role {name}", &[("name", &r.name)])).on_click(move |_| ed.set(Some(OpenRoleEditor::Edit(res.clone()))))]
            })),
        };
        let mut out = col().min_w(0.0).child(page.view(app));
        if let Some(open) = editor.get() {
            let closer = editor.clone();
            let on_created = self.on_created.clone();
            out = out.child(
                RoleEditor {
                    resource: match open {
                        OpenRoleEditor::Create => None,
                        OpenRoleEditor::Edit(r) => Some(r),
                    },
                    catalog: Rc::new(catalog),
                    providers,
                    active_provider_id: app.doc.global_settings.active_provider_id.clone(),
                    tools: self.role_tools.clone(),
                    conversation_image_input_supported: model_choice_of(&app.doc).1.is_some_and(supports_vision),
                    web_search_assets: app.doc.assets.web_search.clone(),
                    templates: self.templates.clone(),
                    presets: app.doc.presets.conversation_presets.clone(),
                    workspaces: self.workspaces.clone(),
                    ssh_machines: app.doc.assets.execution_environments.ssh_machines.clone(),
                    handlers: self.handlers.clone(),
                    on_created: Some(on_created),
                    on_close: Rc::new(move |_| closer.set(None)),
                }
                .view(app),
            );
        }
        out
    }
}

// ---------------------------------------------------------------- the presets page

pub struct PresetsPage {
    pub list_id: String,
    pub presets: Vec<ConversationPreset>,
    pub applied_id: String,
    pub error: Option<String>,
    pub on_apply: Rc<dyn Fn(&mut App, String)>,
    pub on_open: Rc<dyn Fn(&mut App, String)>,
    pub on_rename: Rc<dyn Fn(&mut App, String, String)>,
    pub on_delete: Rc<dyn Fn(&mut App, String)>,
}

#[derive(Clone, Default)]
struct Renaming {
    id: String,
    original: String,
    name: String,
}

pub fn untitled_preset() -> &'static str {
    t("未命名预设", "Untitled preset")
}

impl PresetsPage {
    /// A preset is a saved copy of everything else in this pane, so its settings button opens this same pane on that copy. The arrow stamps the copy onto this conversation, a
    /// one-time action, which is why the applied row is marked rather than tethered.
    pub fn view(self, app: &mut App) -> E {
        let renaming = use_state(("preset-renaming", self.list_id.as_str()), || None::<Renaming>);
        let order = use_state(("catalog-order-presets", self.list_id.as_str()), || catalog_order(app, OrderScope::Presets));
        let ordered = sort_by_order(&self.presets, &order.borrow(), |p| p.id.as_str());
        let ids: Vec<String> = ordered.iter().map(|p| p.id.clone()).collect();
        let on_reorder: Rc<dyn Fn(&mut App, String, String, bool)> = {
            let (ids, order) = (ids.clone(), order.clone());
            Rc::new(move |app, source, target, before| {
                let next = reorder_ids(&ids, &source, &target, before);
                order.set(next.clone());
                write_catalog_order(app, OrderScope::Presets, next);
            })
        };
        let sort = crate::catalog::CatalogSort { list_id: self.list_id.clone(), ids: Rc::new(ids), enabled: true, on_reorder };
        let mut rows = Vec::new();
        for preset in &ordered {
            let is_applied = preset.id == self.applied_id;
            let is_renaming = renaming.with(|r| r.as_ref().is_some_and(|x| x.id == preset.id));
            // The built-in ships with the build: the host rewrites it on every start and keeps it through every save, so it has no rename or delete to offer.
            let editable = !is_builtin_conversation_preset(&preset.id);
            let name = if preset.name.is_empty() { untitled_preset().to_owned() } else { preset.name.clone() };
            let mut detail: Vec<String> = Vec::new();
            if !preset.description.is_empty() {
                detail.push(preset.description.clone());
            }
            if !editable {
                detail.push(t("内置预设，随 Mewrk 版本更新，不能改名、修改或删除。", "Built in. It updates with Mewrk and cannot be renamed, edited or deleted.").to_owned());
            }
            let pid = preset.id.as_str();
            let name_editor = is_renaming.then(|| {
                let current = renaming.with(|r| r.as_ref().map(|x| (x.name.clone(), x.original.clone())).unwrap_or_default());
                let field_key = ("preset-rename", self.list_id.as_str(), pid);
                let writer = renaming.clone();
                let finish = rename_finisher(&renaming, &self.on_rename);
                // Renaming commits by leaving the field, the way the sidebar renames a conversation: there is no Save button to forget to press.
                let escape = renaming.clone();
                let rename_label = tf("重命名预设 {name}", "Rename preset {name}", &[("name", &if current.1.is_empty() { untitled_preset().to_owned() } else { current.1.clone() })]);
                say(&rename_label);
                row()
                    .flex1()
                    .min_w(0.0)
                    // Global because a field's focus is its own id rather than a node's, so a handler around it would not hear the key. Enter is the field's submit.
                    .on_key_global(move |_: &mut App, press| {
                        if press.key != Key::Escape {
                            return false;
                        }
                        escape.set(None);
                        true
                    })
                    .child(
                        field(field_key, current.0)
                            .on_change(move |_, v| writer.update(|r| {
                                if let Some(x) = r {
                                    x.name = v;
                                }
                            }))
                            .on_submit({
                                let finish = finish.clone();
                                move |app, _| finish(app)
                            })
                            .build()
                            .flex1()
                            .aria_label(rename_label),
                    )
                    .child(rename_blur_watcher(field_key, finish))
            });
            let (apply, id_a) = (self.on_apply.clone(), preset.id.clone());
            let lead = icon_btn(("preset-apply", self.list_id.as_str(), pid), "CornerDownLeft", t("套用", "Apply")).on_click(move |app: &mut App| apply(app, id_a.clone()));
            // Held down while its own field is open: the button that opened the field has nothing left to do until it closes.
            let (rn, rename_id, rename_name, rename_list) = (renaming.clone(), preset.id.clone(), preset.name.clone(), self.list_id.clone());
            let rename_disabled = !editable || is_renaming;
            let rename_btn = icon_btn(("preset-rename-btn", self.list_id.as_str(), pid), "Pencil", t("重命名", "Rename")).disabled(rename_disabled).when(rename_disabled, |e| e.opacity(0.4)).on_click(move |_| {
                rn.set(Some(Renaming { id: rename_id.clone(), original: rename_name.clone(), name: rename_name.clone() }));
                focus_request(("field", ("preset-rename", rename_list.as_str(), rename_id.as_str())));
            });
            let (open, id_o) = (self.on_open.clone(), preset.id.clone());
            let open_btn = icon_btn(("preset-open", self.list_id.as_str(), pid), "Settings2", tf("打开预设 {name}", "Open preset {name}", &[("name", &preset.name)])).on_click(move |app: &mut App| open(app, id_o.clone()));
            let (del, id_d) = (self.on_delete.clone(), preset.id.clone());
            let delete = confirm_delete_button(("preset-delete", self.list_id.as_str(), pid), !editable, tf("删除预设 {name}", "Delete preset {name}", &[("name", &preset.name)]), tf("确认删除预设 {name}", "Confirm deleting preset {name}", &[("name", &preset.name)]), Rc::new(move |app: &mut App| del(app, id_d.clone())));
            if is_applied {
                say(&format!("applied:{name}"));
            }
            rows.push(
                CatalogRow {
                    id: Some(preset.id.clone()),
                    name,
                    detail: Some(detail.join("\n")),
                    icon: Some("SlidersHorizontal"),
                    lead: Some(lead),
                    actions: vec![rename_btn, open_btn, delete],
                    name_editor,
                    on: is_applied,
                    sort: Some(sort.clone()),
                    ..CatalogRow::default()
                }
                .view(),
            );
        }
        col().gap(14.0).min_w(0.0).child(self.error.map(error_text)).child(catalog_list(rows)).child(small(t("箭头把预设套用到本对话，设置按钮打开它。", "The arrow applies a preset here; the settings button opens it.")))
    }
}

/// A name emptied or left alone is not a rename, so it closes without writing rather than storing a blank.
fn rename_finisher(renaming: &Local<Option<Renaming>>, on_rename: &Rc<dyn Fn(&mut App, String, String)>) -> Rc<dyn Fn(&mut App)> {
    let (renaming, on_rename) = (renaming.clone(), on_rename.clone());
    Rc::new(move |app: &mut App| {
        let Some(current) = renaming.get() else { return };
        renaming.set(None);
        let name = current.name.trim().to_owned();
        if !name.is_empty() && name != current.original {
            on_rename(app, current.id, name);
        }
    })
}

/// Commits when the field is left. A leaf rather than a handler because `field` has no blur of its own: it watches focus from inside the frame.
fn rename_blur_watcher(field_key: (&str, &str, &str), finish: Rc<dyn Fn(&mut App)>) -> E {
    let id = hash_key(("field", field_key));
    let had_focus = use_state(("rename-had-focus", id), || false);
    custom::<App>(move |app, ui, _| {
        let focused = ui.mem.focus == id;
        if focused {
            had_focus.set(true);
        } else if had_focus.get() {
            had_focus.set(false);
            finish(app);
        }
    })
    .size(0.0, 0.0)
}

// ---------------------------------------------------------------- the role window

/// The pages the role window lists down its left edge. Laid out the way a preset's window is, because it is the same kind of thing: one reusable body, opened to be edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RolePage {
    Role,
    Tools,
    Advanced,
    Skills,
    Mcp,
    Hooks,
    Template,
}

/// A role window's state, as `editorDrafts` keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftState {
    /// The body the window opened on, `None` for a new role. The concurrency check.
    pub original: Option<AgentRole>,
    /// The level a new role is written at; `None` is the global `~/.mewrk`.
    pub workspace_key: Option<String>,
    pub draft: AgentRole,
}

/// The role windows left open in mid-edit, by role. Deliberately not persisted: a draft that survived a restart would be an unsaved edit the user could no longer tell apart from a
/// saved one. Keyed by the role's catalog id, because a role belongs to no conversation, and `new:<level>` for the one not written yet.
#[derive(Default)]
struct RoleDrafts(Vec<(String, DraftState)>);

/// A ceiling, so a long session cannot accumulate drafts without bound.
const MAX_EDITOR_DRAFTS: usize = 64;
const NEW_ROLE_DRAFT_PREFIX: &str = "new:";

fn draft_key_of(resource: Option<&AgentRoleResource>, level: Option<&str>) -> String {
    match resource {
        Some(r) => r.descriptor.id.clone(),
        None => format!("{NEW_ROLE_DRAFT_PREFIX}{}", level.unwrap_or("global")),
    }
}

fn forget_new_role_draft(app: &mut App) {
    app.local::<RoleDrafts>().0.retain(|(k, _)| !k.starts_with(NEW_ROLE_DRAFT_PREFIX));
}

fn remember_draft(app: &mut App, key: String, state: DraftState) {
    // A new role's level is part of its key, so a change of level must not leave the old key behind as a second, stale copy.
    if key.starts_with(NEW_ROLE_DRAFT_PREFIX) {
        forget_new_role_draft(app);
    }
    let drafts = &mut app.local::<RoleDrafts>().0;
    drafts.retain(|(k, _)| *k != key);
    // Re-inserting moves the key to the end, so eviction drops the least recently touched draft.
    drafts.push((key, state));
    while drafts.len() > MAX_EDITOR_DRAFTS {
        drafts.remove(0);
    }
}

/// Drops a role's draft: its file is gone, and a draft of it would reopen on a role that no longer exists.
pub fn forget_agent_role_draft(app: &mut App, role_id: &str) {
    app.local::<RoleDrafts>().0.retain(|(k, _)| k != role_id);
}

fn level_of(resource: &ResourceDescriptor) -> Option<String> {
    resource.workspace_key.clone()
}

pub struct RoleEditor {
    /// The role being edited as the catalog listed it when opened; `None` creates one.
    pub resource: Option<Rc<AgentRoleResource>>,
    /// The live catalog: its roles answer the duplicate-name and concurrency checks, and its skills, servers and hooks are what the role's pages pick out of.
    pub catalog: Rc<CapabilityCatalog>,
    pub providers: Rc<Vec<ApiProvider>>,
    /// What a role bound to "inherit" will run on, so the web page knows which native tools it has.
    pub active_provider_id: Option<String>,
    /// The whole trusted tool catalogue.
    pub tools: Rc<Vec<ToolDescriptor>>,
    pub conversation_image_input_supported: bool,
    pub web_search_assets: WebSearchAssets,
    pub templates: Vec<TemplateSummary>,
    /// Offered on the template page as bodies to copy over the role's own.
    pub presets: Vec<ConversationPreset>,
    /// The levels a new role may be written at besides the global one. `None` is a preset's window, which offers the global level alone.
    pub workspaces: Option<Vec<CapabilityWorkspace>>,
    /// For naming the SSH machine a level is on.
    pub ssh_machines: Vec<mewrk_client::model::SshMachineConfig>,
    pub handlers: Handlers,
    /// A save that wrote a new file. The opener selects it.
    pub on_created: Option<Rc<dyn Fn(&mut App, String)>>,
    pub on_close: Rc<dyn Fn(&mut App)>,
}

struct RoleWindow {
    editor: DraftState,
    page: RolePage,
    error: Option<String>,
    saving: bool,
    name_checked: bool,
    /// Every role opens on its own settings page, with the name ready to type: the one thing a new role cannot be saved without.
    focus_name: bool,
    template_body: Option<Vec<ContextItem>>,
    /// The id whose body `template_body` already holds: re-reading a body this editor just wrote would be a chance to lose what was typed since.
    held_template_id: Option<String>,
    template_generation: u64,
    pending_overwrite: Option<ConversationPreset>,
    overwriting: bool,
    overwrite_error: Option<String>,
}

/// Withheld from the role's picker. `orchestration`: the host strips it from every child, so ticking one could only make the role worse. By category rather than by name deliberately: a
/// second copy of the host's name list would drift in the dangerous direction. `memory`: availability derives from the switches, never from a tool list.
pub fn selectable_role_tools(tools: &[ToolDescriptor]) -> Vec<ToolDescriptor> {
    tools.iter().filter(|tool| tool.category != ToolCategory::Orchestration && tool.category != ToolCategory::Memory).cloned().collect()
}

/// What a new role may call: every row the picker draws, with the preview lifecycle tools brought along. The same set the host materialises for a file that names no tools.
pub fn every_selectable_tool_name(selectable: &[ToolDescriptor]) -> Vec<String> {
    let set: HashSet<String> = selectable.iter().map(|t| t.name.clone()).collect();
    let rows: Vec<String> = selectable.iter().map(|t| t.name.clone()).filter(|n| !is_host_derived_tool_name(n) && !is_preview_lifecycle_tool_name(n)).collect();
    canonical_agent_tool_names(&with_preview_lifecycle_tools(&rows, &set))
}

/// What a role written at `level` may name: the global level, the built-ins and that workspace's own. A global role naming one workspace's skill would dangle and fail its spawn elsewhere.
pub fn with_reachable_picks(role: &AgentRole, level: Option<&str>, catalog: &CapabilityCatalog) -> AgentRole {
    let reachable = |resources: &[ResourceDescriptor], ids: &[String]| -> Vec<String> {
        let in_scope: HashSet<&str> = resources.iter().filter(|e| e.source == mewrk_client::model::ResourceSource::Builtin || e.workspace_key.is_none() || e.workspace_key.as_deref() == level).map(|e| e.id.as_str()).collect();
        ids.iter().filter(|id| in_scope.contains(id.as_str())).cloned().collect()
    };
    let mut next = role.clone();
    next.skill_ids = reachable(&catalog.skills, &role.skill_ids);
    next.mcp_ids = reachable(&catalog.mcps, &role.mcp_ids);
    next.hook_ids = reachable(&catalog.hooks, &role.hook_ids);
    next
}

/// What a window opens on: a draft left open unless the file moved underneath it, in which case the file wins.
fn initial_state(app: &mut App, this: &RoleEditor, selectable: &[ToolDescriptor], creatable: &[Option<String>]) -> DraftState {
    let blank = |tools: Vec<String>| DraftState { original: None, workspace_key: None, draft: AgentRole { model_selection: AgentModelSelection::Inherit, tools, web_search: default_agent_role_web_search(), ..AgentRole::default() } };
    match &this.resource {
        None => {
            let remembered = app.local::<RoleDrafts>().0.iter().find(|(k, _)| k.starts_with(NEW_ROLE_DRAFT_PREFIX)).map(|(_, s)| s.clone());
            match remembered {
                None => blank(every_selectable_tool_name(selectable)),
                Some(r) if creatable.contains(&r.workspace_key) => r,
                // A draft begun from another conversation may name a workspace this one does not have: back to the global level, dropping what no global role can name.
                Some(r) => DraftState { draft: with_reachable_picks(&r.draft, None, &this.catalog), workspace_key: None, original: r.original },
            }
        }
        Some(resource) => {
            let id = &resource.descriptor.id;
            let remembered = app.local::<RoleDrafts>().0.iter().find(|(k, _)| k == id).map(|(_, s)| s.clone());
            if let (Some(r), Some(role)) = (&remembered, &resource.role)
                && r.original.as_ref().is_some_and(|o| same_agent_role(o, role))
            {
                return r.clone();
            }
            if remembered.is_some() {
                forget_agent_role_draft(app, id);
            }
            DraftState { original: resource.role.clone(), workspace_key: level_of(&resource.descriptor), draft: resource.role.clone().unwrap_or_else(|| blank(every_selectable_tool_name(selectable)).draft) }
        }
    }
}

fn explicit_model_options(providers: &[ApiProvider]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for p in providers.iter().filter(|p| p.enabled) {
        for m in &p.models {
            out.push((p.id.clone(), p.name.clone(), m.id.clone()));
        }
    }
    out
}

impl RoleEditor {
    pub fn view(self, app: &mut App) -> E {
        let t_ = theme();
        let this = Rc::new(self);
        let resource_id = this.resource.as_ref().map_or_else(|| "new-role".to_owned(), |r| r.descriptor.id.clone());
        let selectable: Rc<Vec<ToolDescriptor>> = Rc::new(selectable_role_tools(&this.tools));
        let creatable: Vec<Option<String>> = std::iter::once(None).chain(this.workspaces.iter().flatten().map(|w| Some(w.key.clone()))).collect();
        // A different role is a different window: nothing typed into one may leak into the next.
        let window = use_state(("role-window", resource_id.as_str()), || {
            let editor = initial_state(app, &this, &selectable, &creatable);
            // A remembered draft has already been typed into, and an existing role arrives with a valid name, so both answer for it at once.
            let name_checked = this.resource.is_some() || !editor.draft.name.trim().is_empty();
            RoleWindow { editor, page: RolePage::Role, error: None, saving: false, name_checked, focus_name: true, template_body: None, held_template_id: None, template_generation: 0, pending_overwrite: None, overwriting: false, overwrite_error: None }
        });
        if window.update(|w| std::mem::take(&mut w.focus_name)) {
            focus_request(("field", ("role-name", resource_id.as_str())));
        }
        let (page, draft, saving, editor_error, name_checked) = window.with(|w| (w.page, w.editor.draft.clone(), w.saving, w.error.clone(), w.name_checked));
        let role_level: Option<String> = match &this.resource {
            Some(r) => level_of(&r.descriptor),
            None => window.with(|w| w.editor.workspace_key.clone()),
        };

        let draft_name = draft.name.trim().to_owned();
        let name_error = validate_agent_type_name(&draft_name);
        let agents = agent_resources(&this.catalog);
        // Two roles may share a name across levels, a workspace's own shadowing the global one, but not within one, where the model could not tell them apart.
        let duplicate_name = agents.iter().any(|e| Some(&e.descriptor.id) != this.resource.as_ref().map(|r| &r.descriptor.id) && e.descriptor.workspace_key == role_level && e.role.as_ref().map_or(e.descriptor.name.as_str(), |r| r.name.as_str()).trim() == draft_name);
        let show_name_error = name_checked && (name_error.is_some() || duplicate_name);

        // The template page: the body is read the first time it is opened and kept across leaving it, since every edit on it is already written through.
        let editor_template_id = draft.template_id.clone().unwrap_or_default();
        if page == RolePage::Template && window.with(|w| w.held_template_id.as_deref() != Some(editor_template_id.as_str())) {
            let id = editor_template_id.clone();
            window.update(|w| {
                w.held_template_id = Some(id.clone());
                w.template_body = if id.is_empty() { Some(Vec::new()) } else { None };
            });
            if !id.is_empty() {
                let w = window.clone();
                let read = this.handlers.on_read_template.clone();
                read(
                    app,
                    id,
                    // An unreadable body is an empty one to work from: the page still has to open, and the next save overwrites whatever is there regardless.
                    Box::new(move |_app, result| w.update(|w| w.template_body = Some(result.unwrap_or_default()))),
                );
            }
        }
        let template_count = window.with(|w| w.template_body.as_ref().map(Vec::len)).unwrap_or_else(|| this.templates.iter().find(|s| s.id == editor_template_id).map_or(0, |s| s.message_count as usize));

        let selectable_names: HashSet<&str> = selectable.iter().map(|t| t.name.as_str()).collect();
        // A file may name tools the picker does not offer; they are inert and left alone, but must not be counted.
        let visible_selected = draft.tools.iter().filter(|n| selectable_names.contains(n.as_str()) && !is_host_derived_tool_name(n) && !is_preview_lifecycle_tool_name(n)).count();

        let titles = |p: RolePage| match p {
            RolePage::Role => t("角色设置", "Role settings"),
            RolePage::Tools => t("工具", "Tools"),
            RolePage::Advanced => t("高级工具", "Advanced tools"),
            RolePage::Skills => t("技能", "Skills"),
            RolePage::Mcp => "MCP",
            RolePage::Hooks => t("钩子", "Hooks"),
            RolePage::Template => t("对话模板", "Conversation template"),
        };
        let nav_items = [
            (RolePage::Role, "Bot", None),
            (RolePage::Tools, "Wrench", Some(visible_selected)),
            (RolePage::Advanced, "SlidersHorizontal", None),
            (RolePage::Skills, "Box", Some(draft.skill_ids.len())),
            (RolePage::Mcp, "Blocks", Some(draft.mcp_ids.len())),
            (RolePage::Hooks, "FolderCog", Some(draft.hook_ids.len())),
            (RolePage::Template, "FileText", Some(template_count)),
        ]
        .into_iter()
        .map(|(id, ic, count)| NavItem { id, icon: ic, label: titles(id).to_owned(), count })
        .collect();

        let page_blurb: (&str, bool) = match page {
            RolePage::Role => (t("名称、描述、模型与位置。", "Name, description, model and location."), true),
            RolePage::Tools => (t("这个角色能调用的工具。", "The tools this role may call."), false),
            RolePage::Skills => (t("这个角色的子代理能用的技能。", "The skills this role's subagents can use."), true),
            RolePage::Mcp => (t("这个角色的子代理可用的 MCP Server。", "The MCP servers this role's subagents can use."), true),
            RolePage::Hooks => (t("这个角色的子代理运行的钩子。", "The hooks this role's subagents run."), true),
            RolePage::Advanced => (t("这个角色自己的联网搜索与抓取。", "This role's own web search and fetch."), false),
            RolePage::Template => (t("这个角色的开局历史。", "This role's opening history."), true),
        };

        let title = match &this.resource {
            // The name as it was opened: the title is how the user knows which role this window is, and should not change under them mid-rename.
            Some(r) => r.role.as_ref().map(|x| x.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| r.descriptor.name.clone()),
            None => t("新建角色", "New role").to_owned(),
        };
        let title = if title.is_empty() { t("角色设置", "Role settings").to_owned() } else { title };

        let save_this = this.clone();
        let save_window = window.clone();
        let footer = col()
            .gap(6.0)
            .child(nav_action(btn(("role-save", resource_id.as_str()), Look::Subtle, if saving { t("正在保存…", "Saving…") } else { t("保存角色", "Save role") }), true).disabled(saving).on_click(move |app: &mut App| save_editor(app, &save_this, &save_window)))
            .child(editor_error.map(|e| error_text(e).role(Role::Alert)));
        let w_nav = window.clone();
        let nav = settings_nav(&format!("role:{resource_id}"), t("角色设置分类", "Role settings categories"), nav_items, page, NavStyle::Window, Some(title.clone()), Rc::new(move |_, p| w_nav.update(|w| w.page = p)), Some(footer));

        let heading_action = match page {
            RolePage::Tools => Some(docs_link("features", None)),
            RolePage::Advanced => Some(docs_link("agents", Some("role-window"))),
            RolePage::Template => Some(copy_from_preset_menu(&this, &window, &resource_id, template_count)),
            _ => None,
        };
        let description = row().wrap().items_center().gap(4.0).min_w(0.0).child(window_page_blurb(page_blurb.0)).child(page_blurb.1.then(|| docs_inline("agents", Some("role-window"))));
        let overwrite_note = window.with(|w| if page == RolePage::Template { w.overwrite_error.clone() } else { None }).map(|n| error_text(n).ty(CAPTION).role(Role::Alert));

        let body = match page {
            RolePage::Role => role_page(&this, &window, &draft, show_name_error, name_error, duplicate_name, &draft_name, &creatable),
            RolePage::Tools => {
                let w = window.clone();
                let this2 = this.clone();
                ToolGroups { scope: format!("role:{resource_id}"), tools: selectable.clone(), enabled_tools: draft.tools.clone(), on_change: Rc::new(move |app, tools| replace_draft(app, &this2, &w, |d| d.tools = tools)), expansion_key: resource_id.clone(), tone_of: None }.view()
            }
            RolePage::Advanced => role_advanced(app, &this, &window, &draft, &resource_id),
            RolePage::Skills | RolePage::Mcp | RolePage::Hooks => role_capability_page(app, &this, &window, &draft, page, &resource_id, role_level.as_deref()),
            RolePage::Template => {
                let (body, generation) = window.with(|w| (w.template_body.clone(), w.template_generation));
                let run_reads_images = role_reads_images(&this, &draft);
                let role_tools: Vec<String> = draft.tools.iter().filter(|n| selectable_names.contains(n.as_str())).cloned().collect();
                let (w_enable, t_enable) = (window.clone(), this.clone());
                let (w_save, t_save, id_now) = (window.clone(), this.clone(), editor_template_id.clone());
                TemplateEditor {
                    key: format!("role:{resource_id}:{generation}"),
                    template_id: editor_template_id.clone(),
                    contexts: body,
                    tools: selectable.clone(),
                    enabled_tools: role_tools,
                    editable: true,
                    image_input_supported: run_reads_images,
                    autosave: true,
                    // A template that calls a tool the role lacks widens the role's own list, on Save like every other edit here.
                    on_enable_tools: Some(Rc::new(move |app, names| {
                        replace_draft(app, &t_enable, &w_enable, |d| {
                            for n in names {
                                if !d.tools.contains(&n) {
                                    d.tools.push(n);
                                }
                            }
                        });
                    })),
                    on_save: Some(Rc::new(move |app, contexts, done| write_role_template(app, &t_save, &w_save, id_now.clone(), contexts, done))),
                }
                .view(app)
            }
        };
        let flush = page == RolePage::Template;
        let page_el = window_page(("role-page-scroll", resource_id.as_str(), page), description, heading_action, overwrite_note, body, flush).role(Role::Region).aria_label(t("角色设置", "Role settings"));

        // Deliberately not dismissible on its backdrop: a stray click would discard an in-progress role. The close button is a decision, and keeps the draft: nothing has been written either way.
        let w_close = window.clone();
        let close = this.on_close.clone();
        let win = settings_window(("role-window", resource_id.as_str()), &title, 1040.0, false, window_strip(), Rc::new(move |app: &mut App| {
            w_close.update(|w| w.pending_overwrite = None);
            close(app);
        }), nav, page_el.bg(t_.flyout));
        let mut out = col().child(win);
        if let Some(preset) = window.with(|w| w.pending_overwrite.clone()) {
            let (w1, w2, th) = (window.clone(), window.clone(), this.clone());
            let w3 = window.clone();
            let p2 = preset.clone();
            out = out.child(plain_dialog(
                ("role-overwrite", resource_id.as_str()),
                440.0,
                t("覆盖对话模板？", "Overwrite the conversation template?").to_owned(),
                Some(tf("这个角色的对话模板里已有 {count} 条消息，会被预设「{name}」的对话模板整个替换，替换后无法撤销。", "This role's template already holds {count} messages. They will all be replaced by the template of preset “{name}”, and this cannot be undone.", &[("count", &template_count), ("name", &if preset.name.is_empty() { untitled_preset().to_owned() } else { preset.name.clone() })])),
                None,
                vec![
                    btn(("role-overwrite-cancel", resource_id.as_str()), Look::Standard, t("取消", "Cancel")).on_click(move |_| w1.update(|w| w.pending_overwrite = None)),
                    btn(("role-overwrite-ok", resource_id.as_str()), Look::Danger, t("覆盖", "Overwrite")).on_click(move |app: &mut App| overwrite_template(app, &th, &w2, p2.clone())),
                ],
                Rc::new(move |_| w3.update(|w| w.pending_overwrite = None)),
            ));
        }
        out
    }
}

/// Every edit lands in the draft cache as well as in state, so closing the window needs no save of its own.
fn replace_editor(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, change: impl FnOnce(&mut DraftState)) {
    let next = window.update(|w| {
        change(&mut w.editor);
        w.error = None;
        w.editor.clone()
    });
    remember_draft(app, draft_key_of(this.resource.as_deref(), next.workspace_key.as_deref()), next);
}

fn replace_draft(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, change: impl FnOnce(&mut AgentRole)) {
    replace_editor(app, this, window, |e| change(&mut e.draft));
}

fn name_error_text(duplicate: bool, error: Option<AgentTypeNameError>) -> Option<String> {
    if duplicate {
        return Some(t("同一位置已有同名的角色。", "A role with this name already exists at this location.").to_owned());
    }
    Some(match error? {
        AgentTypeNameError::Required => t("角色名称不能为空。", "A role name is required.").to_owned(),
        AgentTypeNameError::TooLong => tf("角色名称最多 {count} 个字符。", "A role name can contain at most {count} characters.", &[("count", &MAX_AGENT_TYPE_CHARS)]),
        AgentTypeNameError::Characters => t("角色名称不能包含换行或其它控制字符。", "A role name cannot contain newlines or other control characters.").to_owned(),
    })
}

fn role_page(this: &Rc<RoleEditor>, window: &Local<RoleWindow>, draft: &AgentRole, show_name_error: bool, name_error: Option<AgentTypeNameError>, duplicate: bool, draft_name: &str, creatable: &[Option<String>]) -> E {
    let t_ = theme();
    let rid = this.resource.as_ref().map_or_else(|| "new-role".to_owned(), |r| r.descriptor.id.clone());
    let (w1, th1) = (window.clone(), this.clone());
    // Typing a name is the user taking up the question, so from the first keystroke the field answers live. Clearing it again does not put the complaint back.
    let name_field = field(("role-name", rid.as_str()), draft.name.clone()).on_change(move |app: &mut App, v| {
        if !v.trim().is_empty() {
            w1.update(|w| w.name_checked = true);
        }
        replace_draft(app, &th1, &w1, |d| d.name = v);
    });
    let name_hint = if show_name_error { name_error_text(duplicate, name_error).map(Hint::error) } else { None };
    let (w2, th2) = (window.clone(), this.clone());
    let description = field(("role-description", rid.as_str()), draft.description.clone())
        .placeholder(t("对抗式审查：负责证伪既有结论、挑错，不产出主线方案。", "Adversarial review: refutes existing conclusions and finds faults; does not produce the main proposal."))
        .multiline(88.0, 200.0)
        .on_change(move |app: &mut App, v| replace_draft(app, &th2, &w2, |d| d.description = v))
        .build();
    let renaming = this.resource.is_some() && window.with(|w| w.editor.original.as_ref().is_some_and(|o| o.name.trim() != draft_name));
    let identity = col()
        .gap(8.0)
        .child(field_block(t("角色名称", "Role name"), name_field.build().aria_label(t("角色名称", "Role name")).aria_invalid(show_name_error), name_hint))
        .child(field_block(t("子代理描述", "Subagent description"), description.aria_label(t("子代理描述", "Subagent description")), Some(Hint::new(t("告诉主代理何时选用这个角色。", "Tells the main agent when to pick this role.")).docs("agents", "role-window"))))
        .child(renaming.then(|| small(t("改名不会换文件，选着它的对话仍然选着它；但已经按旧名称在跑的子代理不会自动跟随新名称。", "Renaming keeps the file, so conversations that select this role still do; a subagent already running under the old name does not follow the new one.")).color(t_.caution)));

    // Model selection. No option carries a dead binding, so the trigger says what the role is bound to rather than falling through to a working model the role does not have.
    let selection = draft.model_selection.clone();
    let unavailable = !agent_model_selection_is_available(&selection, &this.providers);
    let options = explicit_model_options(&this.providers);
    let trigger = match &selection {
        AgentModelSelection::Inherit => t("跟随对话模型", "Follow the conversation's model").to_owned(),
        AgentModelSelection::Explicit { provider_id, model_id } => {
            let label_text = this.providers.iter().find(|p| p.id == *provider_id).map_or_else(|| model_id.clone(), |p| format!("{} · {model_id}", p.name));
            if unavailable { tf("{model}（不可用）", "{model} (unavailable)", &[("model", &label_text)]) } else { label_text }
        }
        AgentModelSelection::Unavailable => t("请选择执行模型", "Choose an execution model").to_owned(),
    };
    let (w3, th3) = (window.clone(), this.clone());
    let mut rows = vec![SelectRow::new("inherit", t("跟随对话模型", "Follow the conversation's model"), move |app: &mut App| replace_draft(app, &th3, &w3, |d| d.model_selection = AgentModelSelection::Inherit)).checked(selection == AgentModelSelection::Inherit)];
    for (i, (provider_id, provider_name, model_id)) in options.into_iter().enumerate() {
        let on = matches!(&selection, AgentModelSelection::Explicit { provider_id: p, model_id: m } if *p == provider_id && *m == model_id);
        let (w, th) = (window.clone(), this.clone());
        rows.push(SelectRow::new(format!("model-{i}"), format!("{provider_name} · {model_id}"), move |app: &mut App| {
            let (provider_id, model_id) = (provider_id.clone(), model_id.clone());
            replace_draft(app, &th, &w, |d| d.model_selection = AgentModelSelection::Explicit { provider_id, model_id });
        }).checked(on));
    }
    let model_hint = if unavailable {
        if matches!(selection, AgentModelSelection::Explicit { .. }) {
            t("这个模型现在取不到——提供商还没拉取模型、被停用，或者这一行已经不在了。角色暂时不能被调用，但绑定会一直留着，模型回来就自动恢复。", "This model cannot be resolved right now — the provider has not fetched its models, is disabled, or the row is gone. The role cannot be called for now, but the binding is kept and recovers by itself once the model is back.")
        } else {
            t("这个角色现在没有可用的模型（旧版本记下的失效绑定）；请选择跟随对话或另一个可用模型。", "This role has no model to run on right now (a broken binding an older build recorded). Choose the conversation's model or another available one.")
        }
    } else {
        t("只列出已启用提供商的模型。", "Only models from enabled providers.")
    };
    let model_box = SelectBox { error: unavailable, ..SelectBox::new(format!("role:{rid}:model"), trigger, rows) };

    let effort = draft.effort;
    let (w4, th4) = (window.clone(), this.clone());
    let mut effort_rows = vec![SelectRow::new("inherit", t("跟随对话", "Follow the conversation"), move |app: &mut App| replace_draft(app, &th4, &w4, |d| d.effort = None)).checked(effort.is_none())];
    for level in REASONING_EFFORTS {
        let (w, th) = (window.clone(), this.clone());
        effort_rows.push(SelectRow::new(effort_label(level), effort_label(level), move |app: &mut App| replace_draft(app, &th, &w, |d| d.effort = Some(level))).checked(effort == Some(level)));
    }
    let effort_trigger: String = effort.map_or_else(|| t("跟随对话", "Follow the conversation").to_owned(), |e: ReasoningEffort| effort_label(e).to_owned());
    let effort_box = SelectBox::new(format!("role:{rid}:effort"), effort_trigger, effort_rows);

    // The built-in is a row here, unlike the conversation's page where "none picked" already is it: "Mewrk concise" pins the default even under a caller on another file. A choice the catalog
    // lost stays a visible option, or the closed select would show "follow" for a role that is not following.
    let choice = match draft.tool_description_file_id.as_deref() {
        Some(RETIRED_GUIDED_PROMPT_PROFILE_ID) => Some(BUILTIN_PROMPT_PROFILE_ID.to_owned()),
        other => other.map(str::to_owned),
    };
    let dangling = choice.clone().filter(|c| !c.is_empty() && !this.catalog.tool_description_files.iter().any(|f| f.id == *c));
    let (w5, th5) = (window.clone(), this.clone());
    let mut profile_rows = vec![SelectRow::new("", t("跟随调用方", "Follow the caller"), move |app: &mut App| replace_draft(app, &th5, &w5, |d| d.tool_description_file_id = None)).checked(choice.as_deref().is_none_or(str::is_empty))];
    for file in &this.catalog.tool_description_files {
        let (w, th, id) = (window.clone(), this.clone(), file.id.clone());
        profile_rows.push(SelectRow::new(file.id.clone(), file.name.clone(), move |app: &mut App| {
            let id = id.clone();
            replace_draft(app, &th, &w, |d| d.tool_description_file_id = Some(id));
        }).checked(choice.as_deref() == Some(file.id.as_str())));
    }
    if let Some(id) = &dangling {
        let (w, th, id2) = (window.clone(), this.clone(), id.clone());
        profile_rows.push(SelectRow::new(id.clone(), tf("不可用：{id}", "Unavailable: {id}", &[("id", id)]), move |app: &mut App| {
            let id = id2.clone();
            replace_draft(app, &th, &w, |d| d.tool_description_file_id = Some(id));
        }).checked(true));
    }
    let profile_trigger = match &choice {
        None => t("跟随调用方", "Follow the caller").to_owned(),
        Some(c) if c.is_empty() => t("跟随调用方", "Follow the caller").to_owned(),
        Some(c) => this.catalog.tool_description_files.iter().find(|f| f.id == *c).map_or_else(|| tf("不可用：{id}", "Unavailable: {id}", &[("id", c)]), |f| f.name.clone()),
    };
    let profile_box = SelectBox::new(format!("role:{rid}:profile"), profile_trigger, profile_rows);

    let location: E = match &this.resource {
        Some(r) => {
            say(&r.descriptor.location);
            text(r.descriptor.location.clone()).ty(CAPTION).color(theme().text_secondary).text_end().max_w(280.0).tooltip(r.descriptor.location.clone())
        }
        None => {
            let current = window.with(|w| w.editor.workspace_key.clone());
            // A conversation with several workspaces is on the first of them unless a draft already says otherwise.
            let project = current.clone().or_else(|| creatable.iter().flatten().next().cloned());
            let project_tip: Arc<str> = match &project {
                Some(key) => {
                    let found = this.workspaces.iter().flatten().find(|w| w.key == *key);
                    let (machine, path) = found.map_or_else(|| parse_capability_workspace_key(key), |w| (w.machine.clone(), w.path.clone()));
                    let place = workspace_location_title(&path, machine.as_ref(), &this.ssh_machines);
                    say(&place);
                    tf("只有这个项目的对话能选：{place}", "Only this project's conversations can pick it: {place}", &[("place", &place)]).into()
                }
                None if this.workspaces.is_none() => t("预设不属于任何项目，它的角色只能放在全局。", "A preset belongs to no project, so its roles can only be global.").into(),
                None => t("这个对话还没有项目。", "This conversation has no project yet.").into(),
            };
            let options = vec![
                Segment { id: "global", value: false, label: t("全局", "Global"), tip: t("所有对话都能选：~/.mewrk/agents", "Every conversation can pick it: ~/.mewrk/agents").into(), disabled: false },
                Segment { id: "project", value: true, label: t("项目", "Project"), tip: project_tip, disabled: project.is_none() },
            ];
            let (w, th) = (window.clone(), this.clone());
            segmented(("role-level", rid.as_str()), options, current.is_some(), None, Rc::new(move |app: &mut App, in_project| {
                let k = if in_project { project.clone() } else { None };
                let catalog = th.catalog.clone();
                // Moving a new role to another level drops whatever it had picked that the new level cannot reach.
                replace_editor(app, &th, &w, |e| {
                    e.draft = with_reachable_picks(&e.draft, k.as_deref(), &catalog);
                    e.workspace_key = k;
                });
            }))
            .w(168.0)
            .aria_label(t("位置", "Location"))
        }
    };
    // Rows rather than stacked fields: what each is called and what it says on the left, its picker in one column on the right.
    let row_of = |id: &str, title: &str, hint: Hint, control: E| setting_row(("role-row", rid.as_str(), id), setting_lead(title.to_owned(), hint), control).px(0.0);
    let picker = |select: SelectBox, name: &str| select.view().w(168.0).aria_label(name);
    let rows_el = col()
        .child(row_of("model", t("执行模型", "Execution model"), if unavailable { Hint::error(model_hint) } else { Hint::new(model_hint) }, picker(model_box, t("执行模型", "Execution model"))))
        .child(row_of("effort", t("思考程度", "Reasoning effort"), Hint::default(), picker(effort_box, t("思考程度", "Reasoning effort"))))
        .child(row_of("profile", t("工具描述", "Tool descriptions"), Hint::new(t("决定子代理的工具与提示词措辞。", "Sets the wording of the subagent's tools and prompts.")).docs("prompt-profiles", "apply"), picker(profile_box, t("工具描述", "Tool descriptions"))))
        .child(row_of("level", t("位置", "Location"), if this.resource.is_none() { Hint::new(t("决定哪些对话能选它，保存后不可改。", "Decides which conversations can pick it; fixed once saved.")) } else { Hint::default() }, location));
    col().gap(14.0).child(identity).child(div().h(1.0).bg(t_.divider)).child(rows_el)
}

/// The provider the role's own runs go through: its bound one, or the one the calling conversation is on. A binding that no longer resolves has no provider to ask.
fn run_family(this: &RoleEditor, selection: &AgentModelSelection) -> Option<mewrk_client::model::ProviderFamily> {
    match selection {
        AgentModelSelection::Explicit { provider_id, .. } => this.providers.iter().find(|p| p.id == *provider_id).map(|p| p.family),
        AgentModelSelection::Inherit => this.providers.iter().find(|p| Some(&p.id) == this.active_provider_id.as_ref()).map(|p| p.family),
        AgentModelSelection::Unavailable => None,
    }
}

/// Whether a message written into this role's template can carry an image. A bound pair is asked directly; "inherit" is whatever the conversation runs on.
fn role_reads_images(this: &RoleEditor, draft: &AgentRole) -> bool {
    match &draft.model_selection {
        AgentModelSelection::Explicit { provider_id, model_id } => this.providers.iter().find(|p| p.enabled && p.id == *provider_id).and_then(|p| p.models.iter().find(|m| m.id == *model_id)).is_some_and(supports_vision),
        AgentModelSelection::Inherit => this.conversation_image_input_supported,
        AgentModelSelection::Unavailable => false,
    }
}

/// The preset's own advanced page with what a role has no field for left out: nothing on it follows the calling conversation.
fn role_advanced(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, draft: &AgentRole, resource_id: &str) -> E {
    let family = run_family(this, &draft.model_selection);
    let remember_key = this.resource.as_ref().map(|r| format!("role:{}", r.descriptor.id));
    let remembered = remembered_legs(app, remember_key.as_deref().unwrap_or(&format!("page:role:{resource_id}")));
    let (w, th) = (window.clone(), this.clone());
    AdvancedPage {
        scope: format!("role:{resource_id}"),
        web: WebSection {
            scope: format!("role:{resource_id}"),
            value: draft.web_search.clone(),
            on_change: Rc::new(move |app, patch: WebPatch| replace_draft(app, &th, &w, |d| patch(&mut d.web_search))),
            web_access: None,
            web_search_assets: this.web_search_assets.clone(),
            // The native backends are the role's own model's, not the conversation's the hints otherwise speak of.
            subject: WebSubject::Role,
            native_fetch_available: family_supports_native_fetch(family),
            native_tool_type_selectable: family_selects_native_tool_type(family),
            search_lock: None,
            fetch_lock: None,
            search_tone: None,
            fetch_tone: None,
            remember_key,
            remembered,
        },
        memory: None,
        file_write_guards: None,
        subagent_messaging: None,
        environment_prompt: None,
        host_messages: None,
    }
    .view()
}

/// The catalog pages a conversation's skills, servers and hooks are picked on, over the role's own lists, scoped to what every caller of the role's level can reach. Not toned, and no delivery switch.
fn role_capability_page(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, draft: &AgentRole, page: RolePage, resource_id: &str, role_level: Option<&str>) -> E {
    let (kind, resources, selected, search, empty): (CatalogKind, Vec<ResourceDescriptor>, Vec<String>, &str, &str) = match page {
        RolePage::Skills => (CatalogKind::Skills, this.catalog.skills.clone(), draft.skill_ids.clone(), t("搜索技能", "Search skills"), t("尚未发现技能", "No skills discovered")),
        RolePage::Mcp => (CatalogKind::Mcp, this.catalog.mcps.clone(), draft.mcp_ids.clone(), t("搜索 MCP Server", "Search MCP servers"), t("尚未发现 MCP Server", "No MCP servers discovered")),
        _ => (CatalogKind::Hooks, this.catalog.hooks.clone(), draft.hook_ids.clone(), t("搜索钩子", "Search hooks"), t("尚未发现钩子", "No hooks discovered")),
    };
    let role_workspaces: Vec<CapabilityWorkspace> = match role_level {
        None => Vec::new(),
        Some(level) => {
            let known = this.workspaces.iter().flatten().find(|w| w.key == level).cloned();
            vec![known.unwrap_or_else(|| {
                let (machine, path) = parse_capability_workspace_key(level);
                CapabilityWorkspace { number: 1, key: level.to_owned(), machine, path }
            })]
        }
    };
    let (w, th) = (window.clone(), this.clone());
    CapabilityPage {
        kind,
        list_id: format!("role:{resource_id}:{}", kind.wire()),
        resources,
        selected_ids: selected,
        single: false,
        tone_of: None,
        on_change: Rc::new(move |app, ids| {
            replace_draft(app, &th, &w, |d| match kind {
                CatalogKind::Skills => d.skill_ids = ids,
                CatalogKind::Mcp => d.mcp_ids = ids,
                _ => d.hook_ids = ids,
            });
        }),
        on_untick_dangling: None,
        on_delete: None,
        error: None,
        unreadable_levels: this.catalog.unreadable_levels.clone(),
        dangling_detail: None,
        selected_unavailable_detail: None,
        search_label: search.to_owned(),
        empty_title: empty.to_owned(),
        empty_description: Hint::new(t("添加后会自动出现在这里。", "Anything you add shows up here automatically.")).docs(kind.wire(), "location"),
        footer: None,
        workspaces: Some(role_workspaces),
        ssh_machines: app.doc.assets.execution_environments.ssh_machines.clone(),
        on_rescan: this.handlers.on_rescan.clone(),
        on_reveal: this.handlers.on_reveal.clone(),
        on_probe: if kind == CatalogKind::Mcp { this.handlers.on_probe.clone() } else { None },
        row_actions: None,
        row_detail: None,
        row_badge: None,
    }
    .view(app)
}

/// Writes a body under the role's template and makes sure the draft cites it: the host mints the id on a first write, and the draft is the only place the role will ever learn it from.
fn write_role_template(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, template_id: String, contexts: Vec<ContextItem>, done: Done<()>) {
    let (th, w) = (this.clone(), window.clone());
    let held = contexts.clone();
    let write = this.handlers.on_write_template.clone();
    write(
        app,
        template_id.clone(),
        contexts,
        Box::new(move |app, result| match result {
            Ok(saved) => {
                w.update(|x| {
                    x.held_template_id = Some(saved.clone());
                    x.template_body = Some(held);
                });
                if saved != template_id {
                    replace_draft(app, &th, &w, |d| d.template_id = Some(saved));
                }
                done(app, Ok(()));
            }
            Err(reason) => done(app, Err(reason)),
        }),
    );
}

/// Copies a preset's opening history over the role's own. The body is written at once, as every other edit on this page is; the editor then starts again on it.
fn overwrite_template(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>, preset: ConversationPreset) {
    window.update(|w| {
        w.pending_overwrite = None;
        w.overwrite_error = None;
        w.overwriting = true;
    });
    let (th, w) = (this.clone(), window.clone());
    let read = this.handlers.on_read_template.clone();
    read(
        app,
        preset.template_id,
        Box::new(move |app, result| {
            let fail = |w: &Local<RoleWindow>, reason: String| {
                w.update(|x| {
                    x.overwrite_error = Some(reason);
                    x.overwriting = false;
                });
            };
            let contexts = match result {
                Ok(c) => c,
                Err(reason) => return fail(&w, reason),
            };
            // The host refuses an empty body, and copying nothing over something is not what the arrow promised either.
            if contexts.is_empty() {
                return fail(&w, t("这份预设的对话模板是空的。", "This preset's conversation template is empty.").to_owned());
            }
            let id = w.with(|x| x.editor.draft.template_id.clone().unwrap_or_default());
            let w2 = w.clone();
            write_role_template(
                app,
                &th,
                &w,
                id,
                contexts,
                Box::new(move |_, result| {
                    w2.update(|x| {
                        x.overwriting = false;
                        match result {
                            Ok(()) => x.template_generation += 1,
                            Err(reason) => x.overwrite_error = Some(reason),
                        }
                    });
                }),
            );
        }),
    );
}

/// On the template page, copying a preset's body over the role's. A preset with no template has nothing to copy, so its row is there but cannot be chosen.
fn copy_from_preset_menu(this: &Rc<RoleEditor>, window: &Local<RoleWindow>, resource_id: &str, template_count: usize) -> E {
    let t_ = theme();
    let label_text = t("从预设覆盖", "Copy from a preset");
    let overwriting = window.with(|w| w.overwriting);
    let items: Vec<PopoverMenuItem> = this
        .presets
        .iter()
        .map(|preset| {
            let name = if preset.name.is_empty() { untitled_preset().to_owned() } else { preset.name.clone() };
            let count = if preset.template_id.is_empty() { 0 } else { this.templates.iter().find(|s| s.id == preset.template_id).map_or(0, |s| s.message_count as usize) };
            let (th, w, p) = (this.clone(), window.clone(), preset.clone());
            let description = if count == 0 { t("这份预设没有对话模板。", "This preset has no conversation template.").to_owned() } else { tf("{count} 条消息", "{count} messages", &[("count", &count)]) };
            // Replacing an empty template loses nothing, so it just happens; replacing one that already says something asks first.
            PopoverMenuItem::new(preset.id.clone(), name).description(description).disabled(count == 0).on(move |app: &mut App| {
                if template_count > 0 {
                    w.update(|x| x.pending_overwrite = Some(p.clone()));
                } else {
                    overwrite_template(app, &th, &w, p.clone());
                }
            })
        })
        .collect();
    PopoverMenu::new(copy_preset_menu_key(resource_id), label_text, move |_| {
        let fg = if overwriting { t_.text_disabled } else { t_.text_primary };
        button::<App>("").h(26.0).px(9.0).gap(5.0).child(icon("CopyPlus", 14.0).color(fg)).child(text(label_text).ty(CAPTION).color(fg).nowrap()).child(icon("ChevronDown", 12.0).color(t_.text_tertiary))
    })
    .disabled(overwriting)
    .menu_label(label_text)
    .align_end()
    .empty_label(t("还没有对话预设。", "No conversation presets yet."))
    .sections(move || vec![PopoverMenuSection::new("presets").items(items)])
    .build()
}

/// The key the template page's copy menu goes by.
pub fn copy_preset_menu_key(resource_id: &str) -> String {
    format!("role:{resource_id}:copy-preset")
}

/// Save: the file this window opened has to be the file still there. Another window, a text editor or the host's own migration may have rewritten it since, and writing this draft over that
/// would silently undo it. A dead model binding is deliberately not a blocker: the role arrives in that state on its own, and refusing would trap every other edit behind fixing it.
fn save_editor(app: &mut App, this: &Rc<RoleEditor>, window: &Local<RoleWindow>) {
    if window.with(|w| w.saving) {
        return;
    }
    window.update(|w| w.name_checked = true);
    let catalog_now = agent_resources(&app.doc.capabilities);
    if let Some(resource) = &this.resource {
        let current = catalog_now.iter().find(|e| e.descriptor.id == resource.descriptor.id).and_then(|e| e.role.clone());
        let original = window.with(|w| w.editor.original.clone());
        if !matches!((&current, &original), (Some(c), Some(o)) if same_agent_role(c, o)) {
            window.update(|w| w.error = Some(t("此角色的文件已在别处变化。请关闭后重新打开再编辑。", "This role's file changed elsewhere. Close and reopen it before editing.").to_owned()));
            return;
        }
    }
    let (draft, key) = window.with(|w| (w.editor.draft.clone(), w.editor.workspace_key.clone()));
    let name = draft.name.trim().to_owned();
    let role_level = match &this.resource {
        Some(r) => level_of(&r.descriptor),
        None => key,
    };
    let duplicate = catalog_now.iter().any(|e| Some(&e.descriptor.id) != this.resource.as_ref().map(|r| &r.descriptor.id) && e.descriptor.workspace_key == role_level && e.role.as_ref().map_or(e.descriptor.name.as_str(), |r| r.name.as_str()).trim() == name);
    if validate_agent_type_name(&name).is_some() || duplicate {
        // The marked field is on the role page; a refusal read from any other page would point at something the user cannot see.
        window.update(|w| {
            w.error = Some(t("请先修正标记的字段。", "Fix the marked fields before saving.").to_owned());
            w.page = RolePage::Role;
        });
        return;
    }
    // Lists canonical the way the host keeps them, so a reorder is not a change: tools sorted, the three id lists de-duplicated in the order they were picked.
    let dedup = |ids: &[String]| {
        let mut seen = HashSet::new();
        ids.iter().filter(|i| seen.insert(i.as_str())).cloned().collect::<Vec<_>>()
    };
    let body = AgentRole { name, tools: canonical_agent_tool_names(&draft.tools), disallowed_tools: canonical_agent_tool_names(&draft.disallowed_tools), skill_ids: dedup(&draft.skill_ids), mcp_ids: dedup(&draft.mcp_ids), hook_ids: dedup(&draft.hook_ids), ..clone_agent_role(&draft) };
    let target = match &this.resource {
        Some(r) => SaveAgentRoleTarget { id: Some(r.descriptor.id.clone()), workspace_key: r.descriptor.workspace_key.clone() },
        None => SaveAgentRoleTarget { id: None, workspace_key: role_level },
    };
    window.update(|w| {
        w.saving = true;
        w.error = None;
    });
    let (th, w) = (this.clone(), window.clone());
    let save = this.handlers.on_save_agent_role.clone().unwrap_or_else(|| Rc::new(crate::ops::save_agent_role));
    save(
        app,
        target,
        body,
        Box::new(move |app, result| {
            let saved = match result {
                Ok(id) => id,
                Err(reason) => {
                    w.update(|x| {
                        x.saving = false;
                        x.error = Some(reason);
                    });
                    return;
                }
            };
            // The draft has become the file, so it stops being a draft.
            match &th.resource {
                Some(r) => forget_agent_role_draft(app, &r.descriptor.id),
                None => {
                    forget_new_role_draft(app);
                    if let Some(f) = &th.on_created {
                        f(app, saved);
                    }
                }
            }
            w.update(|x| {
                x.saving = false;
                x.pending_overwrite = None;
            });
            (th.on_close)(app);
        }),
    );
}
