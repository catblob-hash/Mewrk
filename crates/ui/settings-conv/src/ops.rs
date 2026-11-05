//! What `App.tsx` hands the pane: the handlers behind `ConversationSettings`' props.
//!
//! The pane itself only ever calls `Handlers`, so the shell can replace any of them; `Handlers::live` is the wiring upstream's `App` has. The document, drafts, presets and templates
//! are the client's. The capability operations below are the part of `App.tsx` nothing else has a use for: skills, servers, hooks and roles as files, removed and written by the host.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};

use mewrk_client::doc::{failure_message, ConversationTemplateSummary};
use mewrk_client::model::{ConversationPreset, ConversationPresetSettings, ConversationSettings, ContextItem, ResolvedLanguage, ResourceDescriptor, RunTarget, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_client::tool_defaults::localize_tool_descriptor;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::catalog::CatalogKind;
use crate::logic::AgentRole;

/// An `await` that may touch view state: the continuation runs on the UI thread and may hold `Rc` and `Local`.
pub type Done<T> = Box<dyn FnOnce(&mut App, Result<T, String>)>;
pub type ProbeFn = Rc<dyn Fn(&mut App, ResourceDescriptor, Done<McpProbeReport>)>;
/// A change as a function, so it lands on the settings as they are when it is applied rather than when it was asked for.
pub type SettingsPatch = Rc<dyn Fn(&mut ConversationSettings)>;

#[derive(Default)]
struct PendingCalls {
    next: u64,
    table: HashMap<u64, Box<dyn Any>>,
}

/// Lets a continuation that is not `Send` wait in the app until an answer reaches the UI thread. The client's calls take `Send` closures, because they cross a thread on the way there.
pub fn bridge<T: 'static>(app: &mut App, done: impl FnOnce(&mut App, T) + 'static) -> impl FnOnce(&mut App, T) + Send + 'static {
    let boxed: Box<dyn FnOnce(&mut App, T)> = Box::new(done);
    let calls = app.local::<PendingCalls>();
    calls.next += 1;
    let id = calls.next;
    calls.table.insert(id, Box::new(boxed));
    move |app: &mut App, value: T| {
        let Some(entry) = app.local::<PendingCalls>().table.remove(&id) else { return };
        let Ok(f) = entry.downcast::<Box<dyn FnOnce(&mut App, T)>>() else { return };
        f(app, value);
        repaint();
    }
}

/// `invoke` whose continuation need not be `Send`.
pub fn invoke_ui<T: DeserializeOwned + Send + 'static>(app: &mut App, command: &str, args: Value, done: impl FnOnce(&mut App, Result<T, String>) + 'static) {
    let answer = bridge(app, done);
    app.invoke::<T>(command, args, answer);
}

/// Whether every call this module started has been answered. Tests wait on it.
pub fn idle(app: &mut App) -> bool {
    app.local::<PendingCalls>().table.is_empty()
}

/// A connection test's answer (`McpProbeReport`).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct McpProbeReport {
    pub ok: bool,
    pub server_name: Option<String>,
    pub server_version: Option<String>,
    pub tools: Vec<Value>,
    pub logs: Vec<String>,
    pub error: Option<String>,
}

/// Where `save_agent_role` writes: over the file of the role `id` names, or a new file at the level `workspace_key` names.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SaveAgentRoleTarget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub workspace_key: Option<String>,
}

/// A `SKILL.md` as the host's form reads it: the frontmatter's own words, blank where it does not say.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct SkillForm {
    pub directory: String,
    pub name: String,
    pub description: String,
    pub body: String,
}

/// One `mcpServers` entry as written, `${VAR}` references and all. `transport` is `stdio` or `http`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct McpServerForm {
    pub name: String,
    pub description: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub timeout_seconds: u32,
}

/// One `hooks.json` handler, with the event and matcher of the group it is in.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct HookForm {
    pub name: String,
    pub event: String,
    pub matcher: String,
    pub command: String,
    pub command_windows: String,
    pub status_message: String,
    pub timeout_seconds: u32,
}

/// What an edit window holds, of whichever kind it edits.
#[derive(Clone, Debug, PartialEq)]
pub enum CapabilityForm {
    Skill(SkillForm),
    Mcp(McpServerForm),
    Hook(HookForm),
}

/// `ConversationSettings`' callback props, as functions of the app.
#[derive(Clone)]
pub struct Handlers {
    /// Changes to preset components apply directly to this conversation's settings.
    pub on_change: Rc<dyn Fn(&mut App, ConversationSettings)>,
    /// Changes to conversation-only fields.
    pub on_change_conversation_only: Rc<dyn Fn(&mut App, SettingsPatch)>,
    pub on_apply_preset: Rc<dyn Fn(&mut App, String)>,
    pub on_rename_preset: Option<Rc<dyn Fn(&mut App, String, String)>>,
    pub on_delete_preset: Option<Rc<dyn Fn(&mut App, String)>>,
    pub on_save_preset: Option<Rc<dyn Fn(&mut App, String, ConversationPresetSettings)>>,
    pub on_save_preset_copy: Option<Rc<dyn Fn(&mut App, String, ConversationPresetSettings, Option<Vec<ContextItem>>)>>,
    pub on_bind_preset_template: Option<Rc<dyn Fn(&mut App, String, String)>>,
    pub on_read_template: Rc<dyn Fn(&mut App, String, Done<Vec<ContextItem>>)>,
    pub on_write_template: Rc<dyn Fn(&mut App, String, Vec<ContextItem>, Done<String>)>,
    /// Removes an entry from the catalog itself. `done` says whether it is gone. `None` where the owner has nowhere to route that.
    pub on_delete_capability: Option<Rc<dyn Fn(&mut App, CatalogKind, ResourceDescriptor, Box<dyn FnOnce(&mut App, bool)>)>>,
    pub on_save_agent_role: Option<Rc<dyn Fn(&mut App, SaveAgentRoleTarget, AgentRole, Done<String>)>>,
    /// A skill, a server or a hook as its file says now, for an edit window. Either of these two being `None` leaves a page with no way to add or edit.
    pub on_read_capability: Option<Rc<dyn Fn(&mut App, CatalogKind, String, Done<CapabilityForm>)>>,
    /// Writes one, given the form as it was opened for the host to check against, and says the id it is listed under now.
    pub on_save_capability: Option<Rc<dyn Fn(&mut App, SaveAgentRoleTarget, CapabilityForm, Option<CapabilityForm>, Done<String>)>>,
    pub on_rescan: Option<Rc<dyn Fn(&mut App)>>,
    pub on_fingerprint: Option<Rc<dyn Fn(&mut App, Done<String>)>>,
    pub on_reveal: Option<Rc<dyn Fn(&mut App, CatalogKind, Option<String>)>>,
    pub on_probe: Option<ProbeFn>,
    /// Saves these components as a new preset, with the timeline as its template when asked to.
    pub on_create_preset: Option<Rc<dyn Fn(&mut App, String, bool, Done<ConversationPreset>)>>,
    /// In preset mode, saves the open preset in place, or as a copy of the built-in with the template as edited.
    pub on_save_as_preset: Option<Rc<dyn Fn(&mut App, Option<Vec<ContextItem>>)>>,
}

/// What the live wiring keeps for as long as the app runs.
#[derive(Default)]
pub struct LiveState {
    /// A workspace on another machine keeps its configuration there, so its folder opens in the Files pane. The shell wires this.
    pub on_remote_reveal: Option<Rc<dyn Fn(&mut App, RunTarget, String)>>,
    localized: Option<(ResolvedLanguage, usize, Rc<Vec<ToolDescriptor>>)>,
}

pub fn live(app: &mut App) -> &mut LiveState {
    app.local::<LiveState>()
}

// ---------------------------------------------------------------- capabilities

/// Surfaces a failed rescan on the page that asked for it rather than dropping it.
pub fn rescan_from_pane(app: &mut App) {
    app.set_capability_error(None);
    app.rescan_capabilities(|app, result| {
        if let Err(reason) = result {
            app.set_capability_error(Some(failure_message(&reason, t("无法扫描配置目录", "Could not scan the configuration directories"))));
        }
    });
}

/// Removes a skill, an MCP server, a hook or a role from the place it is configured. All four are files the user owns, so only the host can remove one, and the entry is named by its
/// catalog id rather than described. The catalog is rescanned rather than patched: only a fresh scan knows what is left. `done` says whether it is gone, not whether the rescan worked.
pub fn delete_capability(app: &mut App, kind: CatalogKind, resource: ResourceDescriptor, done: Box<dyn FnOnce(&mut App, bool)>) {
    app.set_capability_error(None);
    let (command, args) = match kind {
        CatalogKind::Hooks => ("delete_hook", json!({ "hookId": resource.id })),
        CatalogKind::Skills => ("delete_skill", json!({ "skillId": resource.id })),
        CatalogKind::Agents => ("delete_agent_role", json!({ "roleId": resource.id })),
        _ => ("delete_mcp_server", json!({ "serverId": resource.id })),
    };
    let fail = t("无法删除这一项", "Could not delete this entry");
    invoke_ui::<Value>(app, command, args, move |app, result| match result {
        Ok(_) => {
            let answer = bridge(app, move |app: &mut App, rescan: Result<(), String>| {
                if let Err(reason) = rescan {
                    app.set_capability_error(Some(failure_message(&reason, fail)));
                }
                done(app, true);
            });
            app.rescan_capabilities(answer);
        }
        Err(reason) => {
            app.set_capability_error(Some(failure_message(&reason, fail)));
            done(app, false);
        }
    });
}

/// Writes a role file, then rescans so the opener finds the role it is about to select. A rescan that fails after a save that worked is left to the next one: the file is written either way.
pub fn save_agent_role(app: &mut App, target: SaveAgentRoleTarget, role: AgentRole, done: Done<String>) {
    invoke_ui::<String>(app, "save_agent_role", json!({ "target": target, "role": role }), move |app, result| match result {
        Ok(id) => {
            let answer = bridge(app, move |app: &mut App, _: Result<(), String>| done(app, Ok(id)));
            app.rescan_capabilities(answer);
        }
        Err(reason) => done(app, Err(reason)),
    });
}

/// Reads one entry as its file says now, never the catalog's view of it: an expanded `${VAR}` written back would put a secret in the file.
pub fn read_capability_form(app: &mut App, kind: CatalogKind, id: String, done: Done<CapabilityForm>) {
    match kind {
        CatalogKind::Skills => invoke_ui::<SkillForm>(app, "read_skill_form", json!({ "skillId": id }), move |app, result| done(app, result.map(CapabilityForm::Skill))),
        CatalogKind::Mcp => invoke_ui::<McpServerForm>(app, "read_mcp_server_form", json!({ "serverId": id }), move |app, result| done(app, result.map(CapabilityForm::Mcp))),
        _ => invoke_ui::<HookForm>(app, "read_hook_form", json!({ "hookId": id }), move |app, result| done(app, result.map(CapabilityForm::Hook))),
    }
}

/// Writes a skill, a server or a hook, then rescans so the page finds it. A rescan that fails after a save that worked is left to the next one: the file is written either way.
pub fn save_capability(app: &mut App, target: SaveAgentRoleTarget, form: CapabilityForm, original: Option<CapabilityForm>, done: Done<String>) {
    fn inner(form: &CapabilityForm) -> Value {
        match form {
            CapabilityForm::Skill(f) => json!(f),
            CapabilityForm::Mcp(f) => json!(f),
            CapabilityForm::Hook(f) => json!(f),
        }
    }
    let command = match form {
        CapabilityForm::Skill(_) => "save_skill",
        CapabilityForm::Mcp(_) => "save_mcp_server",
        CapabilityForm::Hook(_) => "save_hook",
    };
    invoke_ui::<String>(app, command, json!({ "target": target, "form": inner(&form), "original": original.as_ref().map(inner) }), move |app, result| match result {
        Ok(id) => {
            let answer = bridge(app, move |app: &mut App, _: Result<(), String>| done(app, Ok(id)));
            app.rescan_capabilities(answer);
        }
        Err(reason) => done(app, Err(reason)),
    });
}

/// Opens the directory a capability kind is configured in. The host creates it when it does not exist yet, so a first-time user lands somewhere.
pub fn reveal_capability(app: &mut App, kind: CatalogKind, workspace_key: Option<String>) {
    app.set_capability_error(None);
    invoke_ui::<Option<RemoteLocation>>(app, "reveal_capability_location", json!({ "kind": kind.wire(), "workspaceKey": workspace_key }), move |app, result| match result {
        Ok(Some(remote)) => {
            if let Some(f) = live(app).on_remote_reveal.clone() {
                f(app, remote.machine, remote.path);
            }
        }
        Ok(None) => {}
        Err(reason) => app.set_capability_error(Some(failure_message(&reason, t("无法打开配置目录", "Could not open the configuration directory")))),
    });
}

#[derive(Deserialize)]
struct RemoteLocation {
    machine: RunTarget,
    path: String,
}

/// Dials one discovered MCP server. The host looks it up in a fresh scan by id, so nothing executable leaves here.
pub fn probe_mcp_server(app: &mut App, resource: ResourceDescriptor, done: Done<McpProbeReport>) {
    invoke_ui(app, "mcp_probe_server", json!({ "serverId": resource.id }), done);
}

/// `environment_tool_snapshots`, the dependency state a settings surface shows beside what needs it.
pub fn environment_tool_snapshots(app: &mut App, done: Done<Value>) {
    invoke_ui(app, "environment_tool_snapshots", json!({}), done);
}

/// `reveal_environment_tool`: opens where a detected executable lives.
pub fn reveal_environment_tool(app: &mut App, executable: &str) {
    app.invoke_logged("reveal_environment_tool", json!({ "executable": executable }));
}

impl Handlers {
    /// What upstream's `App` wires up for the active conversation or draft.
    pub fn live() -> Self {
        Self {
            on_change: Rc::new(|app, settings| app.save_active_conversation_composition(settings)),
            on_change_conversation_only: Rc::new(|app, patch| app.update_active_conversation_settings(|s| patch(s))),
            on_apply_preset: Rc::new(|app, id| app.apply_preset_to_active_conversation(&id)),
            on_rename_preset: Some(Rc::new(|app, id, name| app.rename_conversation_preset(&id, &name))),
            on_delete_preset: Some(Rc::new(|app, id| app.delete_conversation_preset(&id))),
            on_save_preset: Some(Rc::new(|app, id, settings| app.save_conversation_preset(&id, settings))),
            on_save_preset_copy: Some(Rc::new(|app, id, settings, body| app.save_conversation_preset_copy(&id, settings, body))),
            on_bind_preset_template: Some(Rc::new(|app, id, template| app.bind_preset_template(&id, &template))),
            on_read_template: Rc::new(|app, id, done| {
                let answer = bridge(app, done);
                app.read_template_body(&id, answer);
            }),
            on_write_template: Rc::new(|app, id, body, done| {
                let answer = bridge(app, done);
                app.write_template_body(&id, body, answer);
            }),
            on_delete_capability: Some(Rc::new(delete_capability)),
            on_save_agent_role: Some(Rc::new(save_agent_role)),
            on_read_capability: Some(Rc::new(read_capability_form)),
            on_save_capability: Some(Rc::new(save_capability)),
            on_rescan: Some(Rc::new(rescan_from_pane)),
            on_fingerprint: Some(Rc::new(|app, done| invoke_ui(app, "capability_fingerprint", json!({}), done))),
            on_reveal: Some(Rc::new(reveal_capability)),
            on_probe: Some(Rc::new(probe_mcp_server)),
            on_create_preset: Some(Rc::new(|app, name, capture, done| {
                let answer = bridge(app, done);
                app.create_conversation_preset(&name, capture, answer);
            })),
            on_save_as_preset: None,
        }
    }
}

/// `catalogTools`: the catalogue with labels in the app's language, kept until either changes.
pub fn localized_tools(app: &mut App) -> Rc<Vec<ToolDescriptor>> {
    let language = if app.language_is_chinese() { ResolvedLanguage::ZhCn } else { ResolvedLanguage::EnUs };
    let count = app.doc.tools.len();
    if let Some((l, n, tools)) = &live(app).localized
        && *l == language
        && *n == count
    {
        return tools.clone();
    }
    let tools: Rc<Vec<ToolDescriptor>> = Rc::new(app.doc.tools.iter().map(|tool| localize_tool_descriptor(tool, language)).collect());
    live(app).localized = Some((language, count, tools.clone()));
    tools
}

/// `activeConversationTools`: the catalog as this conversation can use it. A shell tool is listed only when one of its machines has that shell.
pub fn active_conversation_tools(app: &mut App, catalog: &Rc<Vec<ToolDescriptor>>) -> Rc<Vec<ToolDescriptor>> {
    use mewrk_client::machine_shells::{available_shell_backends, host_platform, tools_for_shell_backends};
    let workspaces = app.active_conversation_workspaces();
    let backends = available_shell_backends(&workspaces, app.machine_shells(), host_platform());
    Rc::new(tools_for_shell_backends(catalog, |tool| tool.name.as_str(), &backends))
}

pub type TemplateSummary = ConversationTemplateSummary;
