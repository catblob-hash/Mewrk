use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

use crate::{
    model::{JsonObject, ResolvedLanguage, SecurityLevel, ToolExecutionRequest},
    path_guard::{canonical_scope_root, canonical_workspace, resolve_for_write_with_scope},
};

pub use crate::path_guard::ExecutionScope;

const MAX_PATH_CHARS: usize = 4096;
const MAX_COMMAND_CHARS: usize = 64 * 1024;
const MAX_SHELL_ANALYSIS_CHARS: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    /// The level's stable name where it is stored.
    pub fn slug(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    /// How an approval card names this level.
    pub fn label(self, language: ResolvedLanguage) -> &'static str {
        match (language, self) {
            (ResolvedLanguage::ZhCn, Self::Low) => "低",
            (ResolvedLanguage::ZhCn, Self::Medium) => "中",
            (ResolvedLanguage::ZhCn, Self::High) => "高",
            (ResolvedLanguage::EnUs, Self::Low) => "Low",
            (ResolvedLanguage::EnUs, Self::Medium) => "Medium",
            (ResolvedLanguage::EnUs, Self::High) => "High",
        }
    }
}

/// Why a call was classified as it was, in both app languages. It is decided
/// before anything knows which language the approval card will be drawn in,
/// and the user can switch languages while a run is live.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    zh: String,
    en: String,
}

impl Reason {
    pub fn new(zh: impl Into<String>, en: impl Into<String>) -> Self {
        Self {
            zh: zh.into(),
            en: en.into(),
        }
    }

    pub fn text(&self, language: ResolvedLanguage) -> &str {
        match language {
            ResolvedLanguage::ZhCn => &self.zh,
            ResolvedLanguage::EnUs => &self.en,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationEffect {
    Read,
    Write,
    Unbounded,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityDecision {
    pub requires_approval: bool,
    /// Circuit-breaker prompts cannot be suppressed by a permissive mode.
    pub mandatory_prompt: bool,
    pub risk_level: RiskLevel,
    pub rule_id: &'static str,
    pub reason: Reason,
    pub scope: ExecutionScope,
    pub effect: OperationEffect,
    pub target: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum PathMode {
    Existing { default: Option<&'static str> },
    WriteTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    Bash,
    PowerShell,
}

/// Classifies one trusted built-in tool request.
///
/// `workspace`, `app_data` and `additional_directories` must come from the
/// backend's persisted state and Tauri path resolver, not renderer-provided
/// policy fields. The request is
/// used only for the built-in tool name and arguments. Unknown tools and
/// malformed security-relevant arguments are rejected even under full access.
pub fn classify(
    level: SecurityLevel,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
) -> Result<SecurityDecision, String> {
    classify_placed(
        level,
        workspace,
        app_data,
        additional_directories,
        request,
        None,
    )
}

/// [`classify`] for a caller that knows where a shell call runs: which of the
/// conversation's workspaces it named, and the directory it starts in there.
/// Without a placement a shell call is judged as one that starts somewhere in
/// `workspace`, which proves only what does not depend on where.
fn classify_placed(
    level: SecurityLevel,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
    shell: Option<&ShellPlacement>,
) -> Result<SecurityDecision, String> {
    let (effect, path_mode) = match request.tool_name.as_str() {
        "ls" | "grep" | "find" => (
            OperationEffect::Read,
            Some(PathMode::Existing { default: Some(".") }),
        ),
        "read" => (
            OperationEffect::Read,
            Some(PathMode::Existing { default: None }),
        ),
        // Code navigation reads one file and asks a language server about it.
        //
        // The read is the call's own boundary, but starting the server is not
        // nothing: a `<workspace>/.mewrk/lsp.json` is a file the *project*
        // ships, so a cloned repository can name the command Mewrk launches.
        // Every other repo-controlled command path in this app is gated —
        // `mcp.json` and `hooks.json` need a per-conversation selection,
        // `launch.json` needs `preview_start`'s unbounded approval — and a
        // language server has no selection of its own, because which one
        // answers is decided by the file extension. So the project's own
        // configuration is what raises the card, once per level, exactly as
        // `preview_start` does for `launch.json`.
        //
        // A server from `~/.mewrk/lsp.json` or a built-in preset is the user's
        // own standing choice and asks nothing.
        "lsp" => {
            validate_required_string(&request.input, "operation", 64, "operation argument")?;
            if workspace_declares_language_servers(workspace) {
                return Ok(classify_unbounded(level));
            }
            (
                OperationEffect::Read,
                Some(PathMode::Existing { default: None }),
            )
        }
        "write" => (OperationEffect::Write, Some(PathMode::WriteTarget)),
        "edit" => (
            OperationEffect::Write,
            Some(PathMode::Existing { default: None }),
        ),
        // Every shell backend's tools. zsh and sh are read with the lexer
        // Bash is read with, told which shell it reads for: where the three
        // parse a line differently it asks rather than pick a reading.
        name if crate::shell_backend::ShellBackend::of_tool(name).is_some() => {
            let command =
                required_string(&request.input, "command", MAX_COMMAND_CHARS, "command argument")?;
            let backend = crate::shell_backend::ShellBackend::of_tool(name)
                .expect("this arm's guard found the backend");
            let unplaced = ShellSite::Local { cwd: None };
            let (root, site) = shell.map_or((workspace, &unplaced), |placement| {
                (placement.root.as_path(), &placement.site)
            });
            return classify_shell(
                level,
                root,
                app_data,
                additional_directories,
                backend,
                command,
                site,
            );
        }
        // Fifteen preview tools, one policy each. The approval line is decided here; the catalog's
        // `dangerous` flag, the "Reviewed" marker the tool picker draws, is held to it by
        // `the_reviewed_marker_is_exactly_the_tools_that_ask_at_manual`.
        //
        // Starting or stopping a dev server runs or kills a process the project's launch.json
        // describes, and every tool that drives a page can act on a signed-in origin, so both
        // classify unbounded. Reading the registry, the accessibility tree, one element, or the
        // viewport touches nothing outside the host.
        "preview_start" => {
            validate_required_string(&request.input, "name", 256, "server name argument")?;
            return Ok(classify_unbounded(level));
        }
        "preview_stop" => {
            validate_required_string(&request.input, "serverId", 256, "server id argument")?;
            return Ok(classify_unbounded(level));
        }
        "preview_list" | "preview_logs" | "preview_snapshot" | "preview_resize" => {
            return classify_browser_local(workspace, app_data, additional_directories)
        }
        "preview_inspect" => {
            validate_required_string(&request.input, "selector", 2_048, "CSS selector argument")?;
            return classify_browser_local(workspace, app_data, additional_directories);
        }
        // Console lines, network rows and page pixels all carry whatever the signed-in page is
        // showing, so they share the one boundary that names that risk.
        "preview_console_logs" | "preview_network" | "preview_screenshot" => {
            return Ok(classify_browser_sensitive(level, &request.tool_name))
        }
        // Either names its element by a CSS selector or by a uid from `preview_snapshot`.
        "preview_click" => {
            validate_element_target(&request.input)?;
            return Ok(classify_unbounded(level));
        }
        "preview_fill" => {
            validate_element_target(&request.input)?;
            // The empty string is how a field is cleared, so `value` is present-but-empty-allowed.
            match request.input.get("value") {
                Some(Value::String(value)) if value.chars().count() <= 32_768 => {}
                _ => return Err("Missing or invalid fill value argument".into()),
            }
            return Ok(classify_unbounded(level));
        }
        "preview_eval" => {
            validate_required_string(
                &request.input,
                "expression",
                MAX_COMMAND_CHARS,
                "JavaScript argument",
            )?;
            return Ok(classify_unbounded(level));
        }
        "preview_dialog" => return Ok(classify_unbounded(level)),
        // The image number only means something inside a live run's transcript, so there is
        // nothing a manual execution could resolve it against.
        "preview_upload_image" => {
            return Err(
                "preview_upload_image requires the model run loop to resolve the image number in the conversation; it cannot be manually executed or approved separately"
                    .into(),
            )
        }
        "web_search" => {
            return Err(format!(
                "Network tool {} must be scheduled by the model run loop from trusted search configuration; it cannot be manually executed or approved separately",
                request.tool_name
            ))
        }
        // Orchestration and agent tools are executed by the model run loop
        // itself; manual execution and approval requests must not treat them
        // as ordinary tools. `send_message` is one of them: it messages a child
        // of a live run's pool, so a manual execution has nobody to address.
        // The retired `subagent`, `todo`, `agent_send` and `followup_task`
        // names stay rejected so legacy timeline entries cannot be
        // re-executed either.
        "subagent" | "subagent_update" | "structured_output" | "ask_user" | "todo"
        | "agent_spawn" | "agent_send" | "send_message" | "followup_task" | "task_wait"
        | "task_list" | "task_stop" | "box" | "workflow" | "workflow_step" | "skill"
        | "tool_search" | "fork"
        | "plan" | "exit_plan_mode" | "read_handoff_note" | "create_handoff_note"
        | "edit_handoff_note" | "handoff" => {
            return Err(format!(
                "Orchestration tool {} may only be scheduled by the model run loop; it cannot be manually executed or approved separately",
                request.tool_name
            ))
        }
        "read_global_memory" | "read_project_memory" | "create_global_memory"
        | "create_project_memory" | "edit_global_memory" | "edit_project_memory" => {
            return Err(format!(
                "Long-term memory tool {} requires the host to inject the current model identity; it cannot be manually executed or approved separately",
                request.tool_name
            ))
        }
        name => return Err(format!("Security classifier does not support unknown tool: {name}")),
    };

    let path_mode = path_mode.expect("filesystem tools always have path semantics");
    let path_key = path_key_for(&request.tool_name);
    let requested = match path_mode {
        PathMode::Existing { default } => path_argument(&request.input, path_key, default)?,
        PathMode::WriteTarget => path_argument(&request.input, path_key, None)?,
    };

    let workspace = canonical_workspace(workspace)?;
    let app_data = canonical_workspace(app_data)
        .map_err(|error| format!("Could not access application data directory: {error}"))?;
    let roots = trusted_roots(&workspace, &app_data, additional_directories);
    let denied = protected_stores(&app_data);
    let restricted = ExecutionScope::restricted(roots).denying(denied.clone());
    let unrestricted = ExecutionScope::Unrestricted.denying(denied);
    // Authorization judges scope, not existence. The write resolver also handles
    // missing read targets by verifying the nearest existing ancestor, and rejects
    // broken symlinks. Execution still requires the actual target to exist.
    let target = resolve_for_write_with_scope(&workspace, &requested, &unrestricted)?;
    let is_trusted = resolve_for_write_with_scope(&workspace, &requested, &restricted).is_ok();
    let protected_app_data =
        effect == OperationEffect::Write && is_protected_app_data_target(&target, &app_data);
    // Accept edits writes inside the trusted roots without asking, and at the
    // same level a proven shell command runs without asking. A write that
    // changes what those commands execute is a command by another name, so
    // that one write asks.
    if level == SecurityLevel::AllowEdits
        && effect == OperationEffect::Write
        && is_trusted
        && is_shell_trust_anchor(&target, &app_data)
    {
        return Ok(SecurityDecision {
            requires_approval: true,
            mandatory_prompt: false,
            risk_level: RiskLevel::High,
            rule_id: "filesystem.shell_trust_anchor",
            reason: Reason::new(
                "目标是仓库内部文件、Shell 启动配置或 Shell 快照；改动它会改变之后免确认执行的命令",
                "The target is a repository internal, a shell startup file, or the shell snapshot; changing it changes what later commands run unasked",
            ),
            scope: restricted,
            effect,
            target: Some(target),
        });
    }

    classify_filesystem(
        level,
        effect,
        target,
        is_trusted,
        protected_app_data,
        restricted,
        unrestricted,
    )
}

/// The places this conversation's filesystem tools may reach without an
/// escalation: its workspace, the application data directory, and the further
/// roots its caller names — every other workspace of the conversation on this
/// computer, and every file its instruction files import
/// ([`trusted_extra_roots`]).
///
/// `workspace` and `app_data` are already canonical; each further root is
/// canonicalized here, immediately before use, so a junction or case alias
/// cannot widen the set. An entry that no longer resolves is dropped rather than
/// reported — the directory may have been deleted or unplugged since it was
/// granted, and a stale grant must narrow the boundary, not block every call.
fn trusted_roots(workspace: &Path, app_data: &Path, additional: &[String]) -> Vec<PathBuf> {
    let mut roots = vec![workspace.to_path_buf()];
    if app_data != workspace {
        roots.push(app_data.to_path_buf());
    }
    for directory in additional {
        let Ok(canonical) = canonical_scope_root(Path::new(directory)) else {
            continue;
        };
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    roots
}

/// The further roots a call on this computer is judged against: the
/// conversation's other workspaces here, and every file its instruction files
/// import. The user named each imported file, so in the conversation it is a
/// file of its workspaces — whichever workspace's instructions named it, and
/// for every tool that judges a path, shells included. A path this computer
/// cannot spell as text is left out, which only ever asks more.
fn trusted_extra_roots(
    additional_directories: &[String],
    workspaces: &crate::workspace_set::WorkspaceSet,
) -> Vec<String> {
    let mut roots = additional_directories.to_vec();
    roots.extend(
        workspaces
            .imports()
            .local()
            .into_iter()
            .filter_map(|path| path.to_str().map(str::to_owned)),
    );
    roots
}

/// The tools whose target is a path in the workspace a call names.
fn is_filesystem_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "ls" | "grep" | "find" | "read" | "write" | "edit" | "lsp"
    )
}

/// [`classify`] for a call that may name a workspace on another machine.
///
/// The `workspace` argument selects which of the conversation's numbered
/// workspaces a filesystem tool acts in. When that workspace is on this
/// machine, or the call is not a filesystem tool, the decision is exactly
/// [`classify`]'s. When it is on a WSL or SSH machine the local path guard has
/// nothing to say — the path is not in this filesystem — and the call is judged
/// against that workspace's own root instead. An empty set means the caller has
/// one local workspace, which is the plain rule.
pub fn classify_in_workspaces(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
) -> Result<SecurityDecision, String> {
    classify_call_in_workspaces(
        level,
        workspaces,
        workspace,
        app_data,
        additional_directories,
        request,
        None,
        false,
    )
}

/// [`classify_model_call`] for a call that may name a workspace on another
/// machine. See [`classify_in_workspaces`].
pub fn classify_model_call_in_workspaces(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
) -> Result<SecurityDecision, String> {
    classify_call_in_workspaces(
        level,
        workspaces,
        workspace,
        app_data,
        additional_directories,
        request,
        None,
        true,
    )
}

/// [`classify_model_call_in_workspaces`] for a caller that holds the
/// conversation's session state: `shell_cwd` says where each workspace's next
/// shell call starts ([`remembered_shell_cwd`]).
///
/// That directory is what a shell command's relative paths are proven
/// against. A caller that cannot supply it uses the plain entry point, where
/// a shell call that depends on its working directory always asks.
#[allow(clippy::too_many_arguments)]
pub fn classify_model_call_in_workspaces_at(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
    shell_cwd: ShellCwdLookup<'_>,
) -> Result<SecurityDecision, String> {
    classify_call_in_workspaces(
        level,
        workspaces,
        workspace,
        app_data,
        additional_directories,
        request,
        Some(shell_cwd),
        true,
    )
}

/// The lookup the host passes to [`classify_model_call_in_workspaces_at`]:
/// where `state` remembers each workspace of `conversation_id` to be.
pub fn remembered_shell_cwd<'a>(
    state: &'a crate::state::AppState,
    conversation_id: &'a str,
) -> impl Fn(&crate::workspace_set::ResolvedWorkspace) -> Option<String> + 'a {
    move |workspace| crate::tool_executor::remembered_shell_cwd(state, conversation_id, workspace)
}

#[allow(clippy::too_many_arguments)]
fn classify_call_in_workspaces(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
    shell_cwd: Option<ShellCwdLookup<'_>>,
    model_call: bool,
) -> Result<SecurityDecision, String> {
    // A conversation's own saved tool output is on the host whatever machine
    // its workspace is on; it is judged as the host path it is — inside the
    // application data directory — and the executor serves it here.
    if !crate::tool_output::names_host_file(app_data, request) {
        if let Some(decision) = classify_remote_filesystem_call(level, workspaces, request)? {
            return Ok(decision);
        }
    }
    let placement = shell_placement(workspaces, workspace, request, shell_cwd);
    let workspace = selected_local_lsp_root(workspaces, request)?.unwrap_or(workspace);
    let roots = trusted_extra_roots(additional_directories, workspaces);
    match (model_call, placement.as_ref()) {
        // Every tool but a shell is judged by the plain rule.
        (true, None) => classify_model_call(level, workspace, app_data, &roots, request),
        (false, None) => classify(level, workspace, app_data, &roots, request),
        (true, placement) => {
            classify_model_call_placed(level, workspace, app_data, &roots, request, placement)
        }
        (false, placement) => classify_placed(level, workspace, app_data, &roots, request, placement),
    }
}

/// Where a shell call runs: the workspace it names, as the executor selects
/// it (`tool_executor::run_tool`), and where in it the call starts. `None`
/// for every other tool.
///
/// A call aimed at another machine, or at a workspace the conversation does
/// not have, is placed [`ShellSite::Elsewhere`]: the first runs where this
/// computer cannot look and the executor refuses the second, so neither has a
/// path that could be proven here.
fn shell_placement(
    workspaces: &crate::workspace_set::WorkspaceSet,
    workspace: &Path,
    request: &ToolExecutionRequest,
    shell_cwd: Option<ShellCwdLookup<'_>>,
) -> Option<ShellPlacement> {
    crate::shell_backend::ShellBackend::of_tool(&request.tool_name)?;
    let elsewhere = || ShellPlacement {
        root: workspace.to_path_buf(),
        site: ShellSite::Elsewhere,
    };
    // A caller that resolved no set has one workspace, its own, as the
    // executor reads it. The set is only built when there is a lookup to
    // hand its entry to.
    if workspaces.is_empty() && shell_cwd.is_none() {
        return Some(ShellPlacement {
            root: workspace.to_path_buf(),
            site: ShellSite::Local { cwd: None },
        });
    }
    let fallback;
    let workspaces = if workspaces.is_empty() {
        fallback = crate::workspace_set::WorkspaceSet::local_root(workspace.to_string_lossy());
        &fallback
    } else {
        workspaces
    };
    let Ok(index) = crate::tool_executor::workspace_argument(&request.input) else {
        return Some(elsewhere());
    };
    let Ok(selected) = workspaces.select_for(&request.tool_name, index) else {
        return Some(elsewhere());
    };
    if !selected.is_local() {
        return Some(elsewhere());
    }
    let root = PathBuf::from(&selected.root);
    let cwd = shell_cwd.map(|lookup| lookup(selected).map_or_else(|| root.clone(), PathBuf::from));
    Some(ShellPlacement {
        root,
        site: ShellSite::Local { cwd },
    })
}

/// The root an `lsp` call on a host-machine workspace is judged against: the
/// workspace it named, which the executor resolves the file in and whose
/// `.mewrk/lsp.json` names the server. Judging it against the primary root
/// would test the wrong project's configuration for a second local checkout.
/// `None` for every other call, and for a set with nothing to select.
fn selected_local_lsp_root<'a>(
    workspaces: &'a crate::workspace_set::WorkspaceSet,
    request: &ToolExecutionRequest,
) -> Result<Option<&'a Path>, String> {
    if workspaces.is_empty() || request.tool_name != "lsp" {
        return Ok(None);
    }
    let selected =
        workspaces.select(crate::tool_executor::workspace_argument(&request.input)?)?;
    Ok(selected.is_local().then(|| Path::new(selected.root.as_str())))
}

/// The remote decision when the call is a filesystem tool aimed at a workspace
/// on another machine; `None` hands the call to the local classifier.
///
/// A malformed or out-of-range `workspace` argument is an error here rather
/// than a fallback to the local rule: the executor would refuse the same call,
/// and classifying it against the wrong machine first would only put a
/// misleading card in front of the user.
fn classify_remote_filesystem_call(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    request: &ToolExecutionRequest,
) -> Result<Option<SecurityDecision>, String> {
    classify_remote_filesystem_call_with(level, workspaces, request, &|workspace| {
        crate::remote_lsp::workspace_declares_language_servers(&workspace.runner, &workspace.root)
    })
}

/// Whether a remote workspace ships its own language-server configuration —
/// a round trip to the machine in production, a stub in tests. `None` when the
/// machine could not be asked.
type DeclaresLanguageServers<'a> =
    dyn Fn(&crate::workspace_set::ResolvedWorkspace) -> Option<bool> + 'a;

fn classify_remote_filesystem_call_with(
    level: SecurityLevel,
    workspaces: &crate::workspace_set::WorkspaceSet,
    request: &ToolExecutionRequest,
    declares_language_servers: &DeclaresLanguageServers<'_>,
) -> Result<Option<SecurityDecision>, String> {
    if workspaces.is_empty() || !is_filesystem_tool(&request.tool_name) {
        return Ok(None);
    }
    let selected =
        workspaces.select(crate::tool_executor::workspace_argument(&request.input)?)?;
    if selected.is_local() {
        return Ok(None);
    }
    if request.tool_name == "lsp" {
        validate_required_string(&request.input, "operation", 64, "operation argument")?;
    }
    // The arguments are validated and the lexical answer settled before any
    // machine is consulted: a malformed call costs no round trip.
    let imports = workspaces
        .imports()
        .on_machine(&crate::run_environment::env_key(selected.machine.as_ref()));
    let decision = classify_remote_filesystem(level, &selected.root, &imports, request)?;
    if request.tool_name != "lsp" || level == SecurityLevel::FullAccess {
        // Full access asks nothing, and the answer would not change the
        // decision, so the machine is not consulted for it.
        return Ok(Some(decision));
    }
    // The same rule as the host leg: a project that ships `lsp.json` names
    // the command the language server is started with — on that machine,
    // through its shell — so the project's configuration raises the unbounded
    // card. Two things differ from the host leg, and both exist so that the
    // executor can rely on one fact: for a remote `lsp` call an unrestricted
    // scope means "approved for that card, or full access", nothing else.
    //
    // * A machine that cannot be asked counts as declaring one. Failing open
    //   would let a slow link turn into a repository-named command starting
    //   with no card; the price of failing closed is a card on a call that
    //   is about to fail on the same transport anyway.
    // * A file outside the workspace raises this card rather than the plain
    //   out-of-workspace read. The read card's approval is about reaching the
    //   file; it says nothing about starting the project's server, and the
    //   executor could not tell the two approvals apart.
    if decision.scope == ExecutionScope::Unrestricted
        || declares_language_servers(selected).unwrap_or(true)
    {
        return Ok(Some(classify_unbounded(level)));
    }
    Ok(Some(decision))
}

/// Classifies a filesystem tool acting in a workspace on another machine.
///
/// The host cannot canonicalize a path it cannot stat, so trust is decided
/// lexically: a relative path that never climbs above the root, or an absolute
/// one under it, is inside the workspace, and so is one of `imports` — the
/// files the conversation's instruction files import on that machine — named
/// by its absolute path. The remote leg re-checks the canonical target under a
/// confined scope, so a symlink that points out of the root is refused there
/// rather than admitted here — the fail-closed side of not being able to look.
/// The application data directory is not a consideration on that machine, and
/// `lsp` is judged like a read: the file it names is what the language server
/// on that machine is told to open.
///
/// The decision's scope is a marker, not a set of local roots: `Restricted`
/// with no roots tells the remote leg to confine the call to the workspace and
/// the imported files, `Unrestricted` lets it reach the whole machine.
fn classify_remote_filesystem(
    level: SecurityLevel,
    root: &str,
    imports: &[String],
    request: &ToolExecutionRequest,
) -> Result<SecurityDecision, String> {
    let (effect, path_mode) = match request.tool_name.as_str() {
        "ls" | "grep" | "find" => (
            OperationEffect::Read,
            PathMode::Existing { default: Some(".") },
        ),
        "read" | "lsp" => (
            OperationEffect::Read,
            PathMode::Existing { default: None },
        ),
        "write" => (OperationEffect::Write, PathMode::WriteTarget),
        "edit" => (OperationEffect::Write, PathMode::Existing { default: None }),
        other => return Err(format!("Security classifier does not support unknown tool: {other}")),
    };
    let path_key = path_key_for(&request.tool_name);
    let requested = match path_mode {
        PathMode::Existing { default } => path_argument(&request.input, path_key, default)?,
        PathMode::WriteTarget => path_argument(&request.input, path_key, None)?,
    };
    if requested.chars().any(char::is_control) {
        return Err(format!("{path_key} argument contains an invalid character"));
    }
    let is_trusted = remote_path_is_inside_root(root, &requested)
        || remote_path_is_imported(imports, &requested);
    let target = PathBuf::from(if is_trusted {
        remote_display_target(root, &requested)
    } else {
        requested
    });
    classify_filesystem(
        level,
        effect,
        target,
        is_trusted,
        false,
        ExecutionScope::Restricted { roots: Vec::new() },
        ExecutionScope::Unrestricted,
    )
}

/// Whether `requested`, resolved against `root` the way the remote shell will,
/// stays inside the root before symlinks are considered.
///
/// `~` is the remote user's home, which is known only to that machine, so a
/// `~`-spelled request is comparable only with a `~`-spelled root and an
/// absolute one only with an absolute root. Anything the host cannot place is
/// outside.
fn remote_path_is_inside_root(root: &str, requested: &str) -> bool {
    let root = root.trim().trim_end_matches('/');
    let requested = requested.trim();
    let resolved = if requested.starts_with('/') || requested.starts_with('~') {
        (requested.starts_with('~') == root.starts_with('~'))
            .then(|| normalize_remote_path(requested))
            .flatten()
    } else {
        // Relative: resolved under the root, so only climbing out can escape.
        normalize_remote_path(&format!("{root}/{requested}"))
    };
    resolved.is_some_and(|path| path == root || path.starts_with(&format!("{root}/")))
}

/// Whether `requested` names one of `imports`, the canonical paths on the
/// machine of the files the conversation's instruction files import there.
/// Only an absolute spelling is compared, after `.` and `..`, and a drive path
/// without regard to case, as Windows reads it; a relative or `~` spelling is
/// not one the host can place, and is judged as outside.
fn remote_path_is_imported(imports: &[String], requested: &str) -> bool {
    let requested = requested.trim().replace('\\', "/");
    let drive = |path: &str| path.as_bytes().get(1) == Some(&b':');
    if !requested.starts_with('/') && !drive(&requested) {
        return false;
    }
    let Some(requested) = normalize_remote_path(&requested) else {
        return false;
    };
    imports.iter().any(|import| {
        let import = import.replace('\\', "/");
        if drive(&import) {
            import.eq_ignore_ascii_case(&requested)
        } else {
            import == requested
        }
    })
}

/// Applies `.` and `..` to a POSIX path lexically. `None` when `..` climbs
/// above the leading `/` or `~`, which is a path the host will not vouch for.
fn normalize_remote_path(path: &str) -> Option<String> {
    let (prefix, rest) = if let Some(rest) = path.strip_prefix('~') {
        ("~", rest)
    } else if let Some(rest) = path.strip_prefix('/') {
        ("/", rest)
    } else {
        ("", path)
    };
    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let joined = segments.join("/");
    Some(match prefix {
        "~" if joined.is_empty() => "~".to_owned(),
        "~" => format!("~/{joined}"),
        "/" => format!("/{joined}"),
        _ => joined,
    })
}

/// The path an approval card shows for a trusted remote target: the request
/// spelled under its root, so the card names where on that machine the call
/// lands rather than a relative fragment.
fn remote_display_target(root: &str, requested: &str) -> String {
    let requested = requested.trim();
    if requested.starts_with('/') || requested.starts_with('~') {
        normalize_remote_path(requested).unwrap_or_else(|| requested.to_owned())
    } else {
        normalize_remote_path(&format!("{}/{requested}", root.trim_end_matches('/')))
            .unwrap_or_else(|| requested.to_owned())
    }
}

/// Classifies every built-in model-visible Mewrk tool. Model-only host tools
/// are included here so the catalog has one auditable safety matrix, while
/// `classify` continues to reject attempts to execute those tools manually.
/// Dynamically discovered MCP tools use the separate high-risk approval path.
pub fn classify_model_call(
    level: SecurityLevel,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
) -> Result<SecurityDecision, String> {
    classify_model_call_placed(
        level,
        workspace,
        app_data,
        additional_directories,
        request,
        None,
    )
}

/// [`classify_model_call`], told where a shell call runs ([`classify_placed`]).
fn classify_model_call_placed(
    level: SecurityLevel,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    request: &ToolExecutionRequest,
    shell: Option<&ShellPlacement>,
) -> Result<SecurityDecision, String> {
    let internal = match request.tool_name.as_str() {
        "web_search" | "web_fetch" => {
            return classify_web_network(level, &request.tool_name, &request.input)
        }
        "workflow" => Some(SecurityDecision {
            // Fan-out is broad enough to deserve one explicit authorization,
            // but it is still an ordinary orchestration of tools this
            // conversation already enabled — every step re-enters the same
            // classifier under the same level. Full access therefore clears it,
            // exactly as it clears the `web_search` executor boundary.
            // `mandatory_prompt` stays
            // false so the level, not the tool, decides. The global memory gate
            // above keeps its mandatory prompt because it crosses a boundary the
            // level does not describe.
            requires_approval: level != SecurityLevel::FullAccess,
            mandatory_prompt: false,
            risk_level: RiskLevel::High,
            effect: OperationEffect::Unbounded,
            scope: ExecutionScope::Restricted {
                roots: vec![workspace.to_path_buf()],
            },
            target: None,
            rule_id: "workflow.orchestrated_fan_out",
            reason: Reason::new(
                "工作流将并发派生多个子代理执行计划，需要一次明确授权",
                "The workflow will spawn several subagents at once to carry out its plan, so it needs one explicit authorization",
            ),
        }),
        "workflow_step" => {
            return Err("workflow_step is synthetic workflow-internal context and cannot be called as a model tool".into())
        }
        // Sends a conversation image attachment to the page. The model cites a
        // transcript number and the run loop materializes the bytes itself, so
        // there is no model-supplied filesystem path to scope — only the same
        // "local data leaves for the page" approval line. Every other preview
        // tool falls through to `classify`.
        "preview_upload_image" => {
            if !request.input.contains_key("image_id") {
                return Err("Missing image_id argument".into());
            }
            Some(SecurityDecision {
                requires_approval: level != SecurityLevel::FullAccess,
                mandatory_prompt: false,
                risk_level: RiskLevel::High,
                rule_id: "browser.image_upload",
                reason: Reason::new(
                    "会把对话中的图片附件发送给当前网页",
                    "Will send the conversation image attachment to the current web page",
                ),
                scope: ExecutionScope::Unrestricted,
                effect: OperationEffect::Unbounded,
                target: None,
            })
        }
        // `structured_output` belongs here rather than with the state-changing
        // names below: it hands this run's own result back to its parent and
        // mutates nothing that outlives the turn. `skill` belongs here for the
        // same shape of reason: the body it returns was read from disk by the
        // trusted request builder before the turn started, so the call itself
        // touches nothing — it is a lookup in host memory. `tool_search` is the
        // same lookup over a different table: the schemas it hands back were
        // discovered before the turn started and are already in the request.
        // `fork` classifies as
        // low for a third reason: it creates nothing. At every access level,
        // full access included, it only raises a card, and the user's answer on
        // that card — not this classification — is what may create a child.
        "ask_user" | "task_wait" | "task_list" | "box" | "structured_output" | "skill"
        | "tool_search" | "fork" | "read_global_memory" | "read_project_memory"
        | "exit_plan_mode" | "read_handoff_note" => Some(internal_decision(
            RiskLevel::Low,
            "host.read_or_coordinate",
            Reason::new(
                "只读取本轮宿主状态、等待结果或在同一对话内协调，不触碰文件、Shell 或外部网络",
                "Only reads this turn's host state, waits for results, or coordinates within this conversation; touches no files, shell, or external network",
            ),
        )),
        // The plan document lives in the conversation store, not on disk, so
        // writing it is a host state change, not a filesystem write. An
        // unreadable or unknown action classifies as the write.
        "plan" => {
            let action = request.input.get("action").and_then(Value::as_str);
            Some(if action == Some("read") {
                internal_decision(
                    RiskLevel::Low,
                    "host.read_or_coordinate",
                    Reason::new(
                        "只读取本轮宿主状态，不触碰文件、Shell 或外部网络",
                        "Only reads this turn's host state; touches no files, shell, or external network",
                    ),
                )
            } else {
                internal_decision(
                    RiskLevel::Medium,
                    "host.local_state_change",
                    Reason::new(
                        "会修改当前对话的宿主状态，但不直接触碰文件、Shell 或外部网络",
                        "Changes this conversation's host state, but touches no files, shell, or external network directly",
                    ),
                )
            })
        }
        // The tier is part of the tool name, so there is no scope argument to
        // validate and no way for a model to talk its way from project into
        // global memory.
        "create_global_memory" | "edit_global_memory" => Some(SecurityDecision {
            // Global memory applies in every workspace, so neither FullAccess
            // nor a hook may turn it into an implicit background mutation.
            requires_approval: true,
            mandatory_prompt: true,
            risk_level: RiskLevel::High,
            rule_id: "memory.global_persistent_mutation",
            reason: Reason::new(
                "会修改所有项目都会自动加载的全局持久记忆，必须由用户直接确认",
                "Changes the global persistent memory every project loads automatically, so the user must confirm it directly",
            ),
            scope: ExecutionScope::Unrestricted,
            effect: OperationEffect::Write,
            target: None,
        }),
        // The handoff notebook is the conversation's own, under the app data
        // directory no file tool reaches; `handoff` opens a conversation with
        // this one's settings, which grants nothing the user did not already
        // grant here. The user turned auto-compact on, and a prompt at the
        // moment the context runs out would stall the very turn it serves.
        "create_handoff_note" | "edit_handoff_note" | "handoff" => Some(internal_decision(
            RiskLevel::Medium,
            "host.local_state_change",
            Reason::new(
                "会修改当前对话的交接文档或开启续接对话，但不直接触碰文件、Shell 或外部网络",
                "Changes this conversation's handoff notes or opens its continuation, but touches no files, shell, or external network directly",
            ),
        )),
        "create_project_memory" | "edit_project_memory" => Some(internal_decision(
            RiskLevel::Medium,
            "host.local_state_change",
            Reason::new(
                "会修改当前项目的持久记忆，但不直接触碰文件、Shell 或外部网络",
                "Changes this project's persistent memory, but touches no files, shell, or external network directly",
            ),
        )),
        // Delegation requires approval only in RequestApproval mode; every child tool
        // call re-enters this classifier under the conversation's security level.
        "agent_spawn" => Some(SecurityDecision {
            requires_approval: level == SecurityLevel::RequestApproval,
            mandatory_prompt: false,
            risk_level: RiskLevel::Medium,
            rule_id: "agent.delegation",
            reason: Reason::new(
                "会派生子代理；子代理的每个后续工具调用仍继承本对话的安全策略",
                "Spawns a subagent; every tool call it makes still inherits this conversation's security policy",
            ),
            scope: ExecutionScope::Unrestricted,
            effect: OperationEffect::Write,
            target: None,
        }),
        // The task panel's stop button, pressed by the model: it ends a task
        // the caller started itself and starts nothing, so no level asks.
        "task_stop" => Some(internal_decision(
            RiskLevel::Medium,
            "host.local_state_change",
            Reason::new(
                "会停止本代理自己启动的一个后台任务，不启动新的操作",
                "Stops a background task this agent started itself; starts nothing new",
            ),
        )),
        // The main agent talking to a child it started: a running child is
        // handed a user message, a finished one is resumed with its earlier
        // context. The child's own tool calls re-enter this classifier under
        // the conversation's level like any other child's, and the message
        // runs no tool of this agent's, so no level asks.
        "send_message" => Some(internal_decision(
            RiskLevel::Medium,
            "host.local_state_change",
            Reason::new(
                "会向本对话启动的子代理发消息，可能让已结束的子代理带着原上下文继续运行；它的工具调用仍受本对话安全策略约束",
                "Messages a child agent this conversation started, which can resume a finished child with its earlier context; its tool calls still follow this conversation's security policy",
            ),
        )),
        // Child-only progress reporting: never in the public catalog, but the
        // classifier still has to name it rather than fall through to unknown.
        "subagent_update" => Some(internal_decision(
            RiskLevel::Medium,
            "host.local_state_change",
            Reason::new(
                "会向父代理报告子代理状态",
                "Reports the subagent's status to its parent",
            ),
        )),
        _ => None,
    };
    internal.map_or_else(
        || {
            classify_placed(
                level,
                workspace,
                app_data,
                additional_directories,
                request,
                shell,
            )
        },
        Ok,
    )
}

fn classify_web_network(
    level: SecurityLevel,
    tool_name: &str,
    input: &JsonObject,
) -> Result<SecurityDecision, String> {
    // A single authorization covers an entire `web_search` or `web_fetch` call,
    // including every page the search opens or every URL the fetch reads.
    match tool_name {
        "web_search" => {
            validate_required_string(input, "query", 200, "web-search query")?;
            Ok(SecurityDecision {
                requires_approval: level != SecurityLevel::FullAccess,
                mandatory_prompt: false,
                risk_level: RiskLevel::High,
                rule_id: "web.search",
                reason: Reason::new(
                    "联网搜索会把这句查询发给对话选定的搜索后端，并把它返回的不可信网页内容送入上下文",
                    "Web search sends this query to the conversation's search backend and brings the untrusted web content it returns into context",
                ),
                scope: ExecutionScope::Unrestricted,
                effect: OperationEffect::Unbounded,
                target: None,
            })
        }
        "web_fetch" => {
            let urls = input
                .get("urls")
                .and_then(|value| value.as_array())
                .ok_or_else(|| "Missing web-fetch urls".to_owned())?;
            if urls.is_empty() {
                return Err("web-fetch urls must not be empty".into());
            }
            if urls.len() > crate::model::MAX_SEARCH_INPUTS {
                return Err(format!(
                    "web-fetch accepts at most {} URLs per call",
                    crate::model::MAX_SEARCH_INPUTS
                ));
            }
            let mut named = 0usize;
            for value in urls {
                let url = value
                    .as_str()
                    .ok_or_else(|| "Each web-fetch urls entry must be a string".to_owned())?;
                // A blank entry names nothing to fetch; the pipeline drops it.
                if url.trim().is_empty() {
                    continue;
                }
                if url.chars().count() > 2_048 {
                    return Err("Invalid web-fetch URL".into());
                }
                named += 1;
            }
            if named == 0 {
                return Err("web-fetch urls must not be empty".into());
            }
            Ok(SecurityDecision {
                requires_approval: level != SecurityLevel::FullAccess,
                mandatory_prompt: false,
                risk_level: RiskLevel::High,
                rule_id: "web.fetch",
                reason: Reason::new(
                    "网页抓取会由宿主取回这些地址的正文，并把不可信网页内容送入上下文",
                    "Web fetch has the host retrieve the content at these URLs and brings that untrusted web content into context",
                ),
                scope: ExecutionScope::Unrestricted,
                effect: OperationEffect::Unbounded,
                target: None,
            })
        }
        other => Err(format!("Unknown network tool: {other}")),
    }
}

fn internal_decision(
    risk_level: RiskLevel,
    rule_id: &'static str,
    reason: Reason,
) -> SecurityDecision {
    SecurityDecision {
        requires_approval: false,
        mandatory_prompt: false,
        risk_level,
        rule_id,
        reason,
        scope: ExecutionScope::Unrestricted,
        effect: if risk_level == RiskLevel::Low {
            OperationEffect::Read
        } else {
            OperationEffect::Write
        },
        target: None,
    }
}

fn classify_browser_local(
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
) -> Result<SecurityDecision, String> {
    let workspace = canonical_workspace(workspace)?;
    let app_data = canonical_workspace(app_data)
        .map_err(|error| format!("Could not access application data directory: {error}"))?;
    let roots = trusted_roots(&workspace, &app_data, additional_directories);
    Ok(SecurityDecision {
        requires_approval: false,
        mandatory_prompt: false,
        risk_level: RiskLevel::Low,
        rule_id: "browser.local_observation",
        reason: Reason::new(
            "只读取或调整本机内置浏览器，不直接产生外部页面操作",
            "Only reads or adjusts the local built-in browser; takes no direct action on external pages",
        ),
        scope: ExecutionScope::restricted(roots).denying(protected_stores(&app_data)),
        effect: OperationEffect::Read,
        target: None,
    })
}

// ---------------------------------------------------------------------------
// Shell commands
//
// A shell call below full access runs without asking only when static
// analysis *proves* what it does: that every simple command on the line only
// reads (at every level), or only reads and changes files inside the workspace
// with one of a fixed set of commands (from Accept edits up). Everything else
// asks. The proof is a conclusion of its own ([`ShellProof`]), never something
// read off the displayed risk, and every rule below starts from "not proven":
// a command, an option, a path spelling or a piece of shell syntax the
// analyzer does not recognize is a reason to ask, not something to skip over.
//
// The analyzer has no shell parser behind it, so it accepts a deliberately
// small language: words, quotes, `;`/`&&`/`||`/`|`/newline between simple
// commands, and redirections that write nothing. Expansions, substitutions,
// groups, globs and input redirection all leave it.
// ---------------------------------------------------------------------------

/// What static analysis proved about a shell command.
///
/// This is the only thing that lets a shell call skip its approval card below
/// full access. Risk and effect describe a command for the card; a description
/// that happens to read "low" is not a proof, so nothing derives one from the
/// other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ShellProof {
    /// Nothing was proven. The default, and where every unrecognized form ends.
    #[default]
    Unproven,
    /// Every simple command only reads, and every path it names is inside the
    /// trusted roots: what the `read` tool does unasked at every level.
    ReadOnly,
    /// Every simple command only reads, but a path it names is outside the
    /// trusted roots (never a protected one): what the `read` tool does
    /// unasked from Accept edits up.
    ReadOnlyBeyondTrustedRoots,
    /// Reads, plus file changes from the fixed set, each inside a writable
    /// root: what the `write` tool does unasked from Accept edits up.
    WorkspaceEdit,
}

impl ShellProof {
    /// What a line made of two proven parts has proven: the weaker of the two.
    fn and(self, other: Self) -> Self {
        use ShellProof::{ReadOnly, ReadOnlyBeyondTrustedRoots, Unproven, WorkspaceEdit};
        match (self, other) {
            (Unproven, _) | (_, Unproven) => Unproven,
            (WorkspaceEdit, _) | (_, WorkspaceEdit) => WorkspaceEdit,
            (ReadOnlyBeyondTrustedRoots, _) | (_, ReadOnlyBeyondTrustedRoots) => {
                ReadOnlyBeyondTrustedRoots
            }
            (ReadOnly, ReadOnly) => ReadOnly,
        }
    }
}

/// What a simple command is to the rules that look at a whole line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CommandRole {
    #[default]
    Other,
    ChangeDirectory,
    Git,
    FileChange,
}

/// What a simple command's own rule leaves for the whole-line rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CommandFacts {
    /// Its meaning depends on the directory it runs in: it names a relative
    /// path, or reads its working directory.
    uses_cwd: bool,
    /// It names a filesystem path at all.
    names_path: bool,
    role: CommandRole,
}

#[derive(Clone, Debug)]
struct ShellAssessment {
    effect: OperationEffect,
    risk_level: RiskLevel,
    rule_id: &'static str,
    reason: Reason,
    mandatory_prompt: bool,
    proof: ShellProof,
    facts: CommandFacts,
}

impl ShellAssessment {
    fn read(rule_id: &'static str, reason: Reason) -> Self {
        Self {
            effect: OperationEffect::Read,
            risk_level: RiskLevel::Low,
            rule_id,
            reason,
            mandatory_prompt: false,
            proof: ShellProof::ReadOnly,
            facts: CommandFacts::default(),
        }
    }

    /// A read the level decides on: Manual asks, Accept edits does not.
    fn read_beyond_trusted_roots(rule_id: &'static str, reason: Reason) -> Self {
        Self {
            risk_level: RiskLevel::High,
            proof: ShellProof::ReadOnlyBeyondTrustedRoots,
            ..Self::read(rule_id, reason)
        }
    }

    fn workspace_write(rule_id: &'static str, reason: Reason) -> Self {
        Self {
            effect: OperationEffect::Write,
            risk_level: RiskLevel::Medium,
            rule_id,
            reason,
            mandatory_prompt: false,
            proof: ShellProof::WorkspaceEdit,
            facts: CommandFacts::default(),
        }
    }

    fn unbounded(rule_id: &'static str, reason: Reason) -> Self {
        Self {
            effect: OperationEffect::Unbounded,
            risk_level: RiskLevel::High,
            rule_id,
            reason,
            mandatory_prompt: false,
            proof: ShellProof::Unproven,
            facts: CommandFacts::default(),
        }
    }

    fn circuit_breaker(rule_id: &'static str, reason: Reason) -> Self {
        Self {
            mandatory_prompt: true,
            ..Self::unbounded(rule_id, reason)
        }
    }

    fn with_facts(mut self, facts: CommandFacts) -> Self {
        self.facts = facts;
        self
    }

    /// Keeps what the analysis said the command does, for the card, and takes
    /// back the proof: the command still asks, and `reason` says why.
    fn unproven(mut self, rule_id: &'static str, reason: Reason) -> Self {
        self.proof = ShellProof::Unproven;
        self.rule_id = rule_id;
        self.reason = reason;
        self
    }
}

/// Where a shell call runs, as far as the classifier's caller could say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShellSite {
    /// On this computer. `cwd` is the directory the call starts in — the
    /// workspace root, or the directory the workspace's last call ended in —
    /// when the caller knows it. Without it nothing that depends on the
    /// working directory can be proven.
    Local { cwd: Option<PathBuf> },
    /// On a WSL or SSH machine, or in a workspace the call does not name
    /// properly: none of its paths can be checked from here.
    Elsewhere,
}

/// The workspace a shell call runs in and where in it the call starts.
#[derive(Clone, Debug)]
struct ShellPlacement {
    root: PathBuf,
    site: ShellSite,
}

/// The stores under the application data directory that no file tool, shell
/// proof or browser observation reaches at any level.
///
/// The memory store and the handoff notebooks are their own tools' alone. The
/// file checkpoints are the host's record of what `write` and `edit` replaced
/// ([`crate::file_checkpoints`]): a model that could rewrite them would decide
/// what a rewind puts back.
fn protected_stores(app_data: &Path) -> [PathBuf; 3] {
    [
        app_data.join("memory"),
        app_data.join("handoffs"),
        app_data.join(crate::workspace_dirs::FILE_HISTORY_ROOT),
    ]
}

/// How a caller tells the classifier where a workspace's next shell call
/// starts: the directory its last successful call ended in, or `None` for its
/// root (`tool_executor::remembered_shell_cwd`).
pub type ShellCwdLookup<'a> =
    &'a dyn Fn(&crate::workspace_set::ResolvedWorkspace) -> Option<String>;

/// Whether the analyzer was given somewhere to stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellAnchor {
    /// `base` is the directory the call really starts in.
    Known,
    /// The call is local but its start directory was not supplied; `base` is
    /// the workspace root, which describes the command for the card and
    /// proves nothing about a relative path.
    Unknown,
    /// The call runs on another machine.
    Elsewhere,
}

/// Everything a shell command is judged against.
struct ShellScene<'a> {
    kind: ShellKind,
    backend: crate::shell_backend::ShellBackend,
    /// Canonical root of the workspace the command runs in.
    workspace: &'a Path,
    /// Canonical directory relative paths resolve against.
    base: PathBuf,
    anchor: ShellAnchor,
    /// The trusted roots: what a read reaches unasked at every level.
    roots: &'a [PathBuf],
    /// Where a proven file change may land: the workspace and the directories
    /// the conversation was granted. The application data directory is not
    /// one of them — its files are the host's own state — and neither is a
    /// single imported file.
    write_roots: Vec<PathBuf>,
    /// The stores no file tool may reach at any level.
    protected: [PathBuf; 3],
    app_data: &'a Path,
}

fn classify_shell(
    level: SecurityLevel,
    workspace: &Path,
    app_data: &Path,
    additional_directories: &[String],
    backend: crate::shell_backend::ShellBackend,
    command: &str,
    site: &ShellSite,
) -> Result<SecurityDecision, String> {
    let workspace = canonical_workspace(workspace)?;
    let app_data = canonical_workspace(app_data)
        .map_err(|error| format!("Could not access application data directory: {error}"))?;
    let roots = trusted_roots(&workspace, &app_data, additional_directories);
    let kind = match backend.dialect() {
        crate::shell_backend::ScriptDialect::PowerShell => ShellKind::PowerShell,
        crate::shell_backend::ScriptDialect::Posix => ShellKind::Bash,
    };
    // The start directory is whatever the host remembered, so it is checked
    // again here: only a directory that still resolves inside the workspace
    // is somewhere to stand.
    let (base, anchor) = match site {
        ShellSite::Elsewhere => (workspace.clone(), ShellAnchor::Elsewhere),
        ShellSite::Local { cwd: None } => (workspace.clone(), ShellAnchor::Unknown),
        ShellSite::Local { cwd: Some(cwd) } => match canonical_workspace(cwd) {
            Ok(cwd) if path_within(&cwd, &workspace) => (cwd, ShellAnchor::Known),
            _ => (workspace.clone(), ShellAnchor::Unknown),
        },
    };
    let write_roots = roots
        .iter()
        .filter(|root| root.is_dir() && !same_path(root, &app_data))
        .cloned()
        .collect();
    let scene = ShellScene {
        kind,
        backend,
        workspace: &workspace,
        base,
        anchor,
        roots: &roots,
        write_roots,
        protected: protected_stores(&app_data),
        app_data: &app_data,
    };

    // The delete circuit breaker comes before every other verdict, the length
    // limit included: a rule that returned first would decide whether a
    // recursive delete of `/` is asked about at full access.
    let mut assessment = match recursive_delete_risk(kind, command, &workspace) {
        Some(DeleteRisk::Critical) => ShellAssessment::circuit_breaker(
            "shell.critical_delete",
            Reason::new("递归删除的目标是文件系统根目录、用户主目录或系统目录", "The recursive delete targets the filesystem root, a home directory, or a system directory"),
        ),
        Some(DeleteRisk::Unverifiable) => ShellAssessment::circuit_breaker(
            "shell.unverifiable_recursive_delete",
            Reason::new("递归删除的目标由变量、命令替换或看不到的输入拼出，无法核对它会删除什么", "The recursive delete's target is built from a variable, a command substitution, or input that cannot be seen, so what it deletes cannot be checked"),
        ),
        None if command.chars().count() > MAX_SHELL_ANALYSIS_CHARS => ShellAssessment::unbounded(
            "shell.analysis_limit",
            Reason::new(format!("命令超过静态分析上限 {MAX_SHELL_ANALYSIS_CHARS} 字符，无法证明为只读操作"), format!("The command is longer than the {MAX_SHELL_ANALYSIS_CHARS}-character static analysis limit, so it cannot be proven read-only")),
        ),
        None => analyze_shell(&scene, command),
    };

    if level == SecurityLevel::AllowEdits && assessment.proof != ShellProof::Unproven {
        if let Some(reason) = writable_command_environment(&scene) {
            assessment = assessment.unproven("shell.writable_command_environment", reason);
        }
    }
    // Only a proof clears a shell call below full access, and which proofs
    // count is the file tools' own matrix: a read inside the trusted roots at
    // every level; a read outside them, or a write inside the workspace, from
    // Accept edits up. The match names every level so a new one has to be
    // decided here rather than inherit an answer.
    let cleared = match (level, assessment.proof) {
        (SecurityLevel::FullAccess, _) => true,
        (SecurityLevel::RequestApproval | SecurityLevel::AllowEdits, ShellProof::Unproven) => false,
        (SecurityLevel::RequestApproval | SecurityLevel::AllowEdits, ShellProof::ReadOnly) => true,
        (
            SecurityLevel::AllowEdits,
            ShellProof::ReadOnlyBeyondTrustedRoots | ShellProof::WorkspaceEdit,
        ) => true,
        (
            SecurityLevel::RequestApproval,
            ShellProof::ReadOnlyBeyondTrustedRoots | ShellProof::WorkspaceEdit,
        ) => false,
    };
    if level == SecurityLevel::AllowEdits
        && assessment.proof == ShellProof::ReadOnlyBeyondTrustedRoots
    {
        // The `read` tool's own rule for the same reach, and its risk.
        assessment.risk_level = RiskLevel::Medium;
        assessment.rule_id = "shell.outside_read_allow_edits";
        assessment.reason = Reason::new(
            "只读命令的目标在工作区外；允许编辑模式放行工作区外读取",
            "The read-only command reaches outside the workspace; Accept edits allows reads outside it",
        );
    }
    Ok(SecurityDecision {
        requires_approval: assessment.mandatory_prompt || !cleared,
        mandatory_prompt: assessment.mandatory_prompt,
        risk_level: assessment.risk_level,
        rule_id: assessment.rule_id,
        reason: assessment.reason,
        // The scope is not the shell's boundary: a statically safe verdict can
        // suppress a prompt, but it must never be misrepresented as an
        // execution boundary to the subprocess layer. What does bound a
        // command is the workspace's sandbox, when it is on, which comes
        // before this decision rather than from it: no level, answer or hook
        // widens it, and a machine that cannot sandbox refuses the command
        // before anything asks (`tool_executor::sandbox_refusal`).
        scope: ExecutionScope::Unrestricted,
        effect: assessment.effect,
        target: None,
    })
}

/// Why no shell proof holds at Accept edits for this conversation, if one
/// does not.
///
/// At that level a file tool writes inside the trusted roots without asking.
/// When those roots hold what decides which program a command name runs — a
/// directory on `PATH`, or the home directory with its shell and Git startup
/// files — a write is a command by another name, and a proof about the
/// command's text says nothing about what executes. Manual has no such gap:
/// every write there is asked about.
fn writable_command_environment(scene: &ShellScene<'_>) -> Option<Reason> {
    let inside_roots = |path: &Path| {
        fs::canonicalize(path)
            .ok()
            .is_some_and(|canonical| scene.roots.iter().any(|root| path_within(&canonical, root)))
    };
    if dirs::home_dir().is_some_and(|home| inside_roots(&home)) {
        return Some(Reason::new(
            "可信目录包含用户主目录，允许编辑模式下模型可以改写 Shell 与 Git 的启动配置，因此命令需要逐条确认",
            "The trusted directories contain the home directory, where Accept edits lets the model rewrite shell and Git startup files, so each command is confirmed",
        ));
    }
    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path)
        .any(|entry| entry.is_absolute() && inside_roots(&entry))
        .then(|| {
            Reason::new(
                "PATH 中有位于可信目录内的目录，允许编辑模式下模型可以替换命令名解析到的程序，因此命令需要逐条确认",
                "A PATH directory lies inside the trusted directories, where Accept edits lets the model replace the program a command name resolves to, so each command is confirmed",
            )
        })
}

fn analyze_shell(scene: &ShellScene<'_>, command: &str) -> ShellAssessment {
    if has_network_path(command) {
        return ShellAssessment::unbounded(
            "shell.network_path",
            Reason::new("命令包含 UNC 或网络路径，访问时可能向远端主机发送 Windows 凭据", "The command contains a UNC or network path; reaching it may send Windows credentials to a remote host"),
        );
    }
    let lexed = match scene.kind {
        ShellKind::Bash => lex_posix(scene.backend, command),
        ShellKind::PowerShell => lex_powershell(command),
    };
    let commands = match lexed {
        Ok(commands) if !commands.is_empty() => commands,
        Ok(_) => {
            return ShellAssessment::unbounded(
                "shell.empty_analysis",
                Reason::new(
                    "命令没有可静态分析的子命令",
                    "The command has no subcommand that can be statically analyzed",
                ),
            )
        }
        Err(refusal) => return refusal,
    };
    if scene.kind == ShellKind::PowerShell && has_powershell_non_filesystem_provider(command) {
        return ShellAssessment::unbounded(
            "powershell.non_filesystem_provider",
            Reason::new("PowerShell 命令引用注册表、证书、环境变量或其他非文件系统 Provider", "The PowerShell command refers to the registry, certificates, environment variables, or another non-filesystem provider"),
        );
    }

    let mut combined = ShellAssessment::read(
        "shell.read_only",
        Reason::new("所有子命令都属于内置只读集合，且未发现越界路径或写入型参数", "Every subcommand is in the built-in read-only set, with no out-of-bounds paths or write arguments"),
    );
    let mut parts = Vec::with_capacity(commands.len());
    // What the head of the current PowerShell pipeline hands down it.
    let mut pipeline_emits_text = false;
    for command in &commands {
        let next = match scene.kind {
            ShellKind::Bash => analyze_posix_command(scene, command),
            ShellKind::PowerShell => {
                analyze_powershell_command(scene, command, &mut pipeline_emits_text)
            }
        };
        if next.mandatory_prompt {
            return next;
        }
        if next.risk_level > combined.risk_level {
            combined.rule_id = next.rule_id;
            combined.reason = next.reason.clone();
        }
        combined.risk_level = combined.risk_level.max(next.risk_level);
        combined.effect = combine_effect(combined.effect, next.effect);
        combined.proof = combined.proof.and(next.proof);
        parts.push(next.facts);
    }
    if combined.proof == ShellProof::Unproven {
        return combined;
    }
    if let Some((rule_id, reason)) = line_refusal(scene, &parts) {
        return combined.unproven(rule_id, reason);
    }
    combined
}

/// Why a line whose commands were each proven is still not proven as a whole.
fn line_refusal(scene: &ShellScene<'_>, parts: &[CommandFacts]) -> Option<(&'static str, Reason)> {
    let count = |role: CommandRole| parts.iter().filter(|facts| facts.role == role).count();
    let changes_directory = count(CommandRole::ChangeDirectory);
    if changes_directory > 1 {
        return Some((
            "shell.repeated_cd",
            Reason::new("一条命令里多次 cd，之后的路径基准无法静态确定", "The command changes directory more than once, so what its later paths are relative to cannot be determined statically"),
        ));
    }
    if changes_directory > 0 && count(CommandRole::Git) > 0 {
        return Some((
            "shell.cd_then_git",
            Reason::new("cd 之后运行 git：目标目录的仓库配置与钩子未经核对", "git runs after a cd: the repository configuration and hooks of the directory it moved to are unchecked"),
        ));
    }
    if count(CommandRole::FileChange) > 0 && count(CommandRole::Git) > 0 {
        return Some((
            "shell.file_change_then_git",
            Reason::new("同一条命令先改动文件再运行 git，无法排除它先布置了仓库内部文件", "The command changes files and runs git on one line, so it cannot be ruled out that it lays down repository internals first"),
        ));
    }
    // A `cd` moves every later relative path, and where it lands is the
    // shell's to say (`CDPATH`, a symlink it walked through).
    let mut moved = false;
    for facts in parts {
        if moved && facts.uses_cwd {
            return Some((
                "shell.cd_then_relative_path",
                Reason::new("cd 之后的相对路径或对当前目录的读取无法静态核对", "A relative path or a read of the working directory after a cd cannot be checked statically"),
            ));
        }
        moved |= facts.role == CommandRole::ChangeDirectory;
    }
    match scene.anchor {
        ShellAnchor::Known => None,
        ShellAnchor::Unknown => parts.iter().any(|facts| facts.uses_cwd).then(|| {
            (
                "shell.unanchored_working_directory",
                Reason::new("命令依赖 Shell 的当前目录，而这次调用没有可核对的起始目录", "The command depends on the shell's working directory, and this call has no start directory that could be checked"),
            )
        }),
        ShellAnchor::Elsewhere => parts
            .iter()
            .any(|facts| facts.uses_cwd || facts.names_path)
            .then(|| {
                (
                    "shell.remote_paths_unverifiable",
                    Reason::new("命令在另一台机器上运行，本机无法核对它涉及的路径", "The command runs on another machine, where this computer cannot check the paths it involves"),
                )
            }),
    }
}

fn combine_effect(left: OperationEffect, right: OperationEffect) -> OperationEffect {
    match (left, right) {
        (OperationEffect::Unbounded, _) | (_, OperationEffect::Unbounded) => {
            OperationEffect::Unbounded
        }
        (OperationEffect::Write, _) | (_, OperationEffect::Write) => OperationEffect::Write,
        _ => OperationEffect::Read,
    }
}

// --- Lexing -----------------------------------------------------------------

/// One word of a simple command after quote removal, with what the shell
/// would still do to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Lexeme {
    text: String,
    /// It holds a live glob character: the shell replaces the word with
    /// whatever file names match, which may begin with `-`.
    wild: bool,
    /// It holds a live `~` where the shell expands one to a home directory.
    home: bool,
    /// The byte length of a live `NAME` in front of a live `=`: an assignment
    /// when the word comes before the command name.
    assigns: Option<usize>,
    /// Some of it was quoted or escaped.
    quoted: bool,
}

impl Lexeme {
    fn literal(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            ..Self::default()
        }
    }

    /// The rest of the word from byte `start`: an option's attached argument.
    fn tail(&self, start: usize) -> Self {
        Self {
            text: self.text[start..].to_owned(),
            wild: self.wild,
            home: self.home,
            assigns: None,
            quoted: self.quoted,
        }
    }
}

/// A command name and its arguments, with nothing else on the line.
#[derive(Clone, Debug, Default)]
struct SimpleCommand {
    words: Vec<Lexeme>,
    /// It reads the output of the command before it.
    piped: bool,
}

#[derive(Default)]
struct WordBuilder {
    /// Each character with whether it is live: unquoted and unescaped.
    chars: Vec<(char, bool)>,
    started: bool,
}

impl WordBuilder {
    fn push(&mut self, character: char, live: bool) {
        self.chars.push((character, live));
        self.started = true;
    }

    /// Whether the word so far is the file descriptor of a redirection
    /// (`2>`), which is syntax rather than an argument.
    fn is_descriptor(&self) -> bool {
        !self.chars.is_empty()
            && self
                .chars
                .iter()
                .all(|(character, live)| *live && character.is_ascii_digit())
    }

    fn take(&mut self, backend: crate::shell_backend::ShellBackend) -> Option<Lexeme> {
        use crate::shell_backend::{ScriptDialect, ShellBackend};
        let started = std::mem::take(&mut self.started);
        let chars = std::mem::take(&mut self.chars);
        if !started {
            return None;
        }
        let text: String = chars.iter().map(|(character, _)| *character).collect();
        let posix = backend.dialect() == ScriptDialect::Posix;
        let zsh = backend == ShellBackend::Zsh;
        // `[` alone is the test command and `]` its last argument.
        let lone_bracket = text == "[" || text == "]";
        let wild = chars.iter().enumerate().any(|(index, (character, live))| {
            *live
                && match character {
                    '*' | '?' => true,
                    '[' | ']' => !lone_bracket,
                    // zsh's extended globs, and its `=command` expansion.
                    '^' | '#' => zsh,
                    '=' => zsh && index == 0 && chars.len() > 1,
                    _ => false,
                }
        });
        let home = chars.iter().enumerate().any(|(index, (character, live))| {
            *live
                && *character == '~'
                && (index == 0 || (posix && matches!(chars[index - 1], ('=' | ':', true))))
        });
        let mut assigns = None;
        if posix {
            let mut bytes = 0;
            for (index, (character, live)) in chars.iter().enumerate() {
                if !*live {
                    break;
                }
                if *character == '=' {
                    assigns = (index > 0).then_some(bytes);
                    break;
                }
                let name_character = *character == '_'
                    || character.is_ascii_alphabetic()
                    || (index > 0 && character.is_ascii_digit());
                if !name_character {
                    break;
                }
                bytes += character.len_utf8();
            }
        }
        Some(Lexeme {
            text,
            wild,
            home,
            assigns,
            quoted: chars.iter().any(|(_, live)| !live),
        })
    }
}

/// How a simple command ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Separator {
    /// `;`, `&&`, `||`.
    Sequence,
    Newline,
    Pipe,
}

#[derive(Default)]
struct CommandList {
    commands: Vec<SimpleCommand>,
    current: SimpleCommand,
    /// Whether the command being read takes the previous one's output.
    piped: bool,
}

impl CommandList {
    fn end_word(&mut self, word: &mut WordBuilder, backend: crate::shell_backend::ShellBackend) {
        if let Some(lexeme) = word.take(backend) {
            self.current.words.push(lexeme);
        }
    }

    fn end_command(&mut self, separator: Separator) {
        if self.current.words.is_empty() {
            // A pipeline continues over the newline after its `|`.
            match separator {
                Separator::Pipe => self.piped = true,
                Separator::Newline => {}
                Separator::Sequence => self.piped = false,
            }
            return;
        }
        let mut finished = std::mem::take(&mut self.current);
        finished.piped = self.piped;
        self.commands.push(finished);
        self.piped = separator == Separator::Pipe;
    }
}

fn refuse_dynamic() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.dynamic_expansion",
        Reason::new("命令包含命令替换、进程替换、动态调用或表达式执行，静态分类器无法验证实际载荷", "The command contains command substitution, process substitution, dynamic invocation, or expression evaluation, so the static classifier cannot verify what actually runs"),
    )
}

fn refuse_background() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.background_or_call_operator",
        Reason::new("命令包含后台执行或 PowerShell 调用运算符，后续载荷与完成状态无法可靠复核", "The command contains a background or PowerShell call operator, so what it runs next and whether it finishes cannot be reliably checked"),
    )
}

fn refuse_redirection() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.redirection",
        Reason::new("子命令包含输出重定向或无法静态解析的输入重定向", "A subcommand contains output redirection, or input redirection that cannot be statically resolved"),
    )
}

fn refuse_grouping() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.grouping",
        Reason::new("命令包含子 Shell、命令组、花括号展开或脚本块，静态分类器不展开它们", "The command contains a subshell, a command group, a brace expansion, or a script block, which the static classifier does not expand"),
    )
}

fn refuse_unparsed() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.parse_failed",
        Reason::new("命令包含未闭合的引号、转义或括号，无法静态解析", "Command contains an unclosed quote, escape, or parenthesis and cannot be statically parsed"),
    )
}

fn refuse_characters() -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.unusual_characters",
        Reason::new("命令包含控制字符、非 ASCII 空白或形似引号与连字符的字符，Shell 与静态分类器可能对它们有不同读法", "The command contains control characters, non-ASCII whitespace, or look-alike quotes and dashes, which the shell and the static classifier may read differently"),
    )
}

/// Characters neither lexer reads the way every shell does: control
/// characters other than a newline and a tab, and whitespace outside ASCII
/// (a word break to PowerShell and to Rust, an ordinary character to Bash).
fn has_unusual_characters(kind: ShellKind, command: &str) -> bool {
    command.chars().any(|character| match character {
        '\n' | '\t' | ' ' => false,
        '\r' => kind == ShellKind::Bash,
        character => character.is_control() || character.is_whitespace(),
    })
}

fn lex_posix(
    backend: crate::shell_backend::ShellBackend,
    command: &str,
) -> Result<Vec<SimpleCommand>, ShellAssessment> {
    use crate::shell_backend::ShellBackend;
    if has_unusual_characters(ShellKind::Bash, command) {
        return Err(refuse_characters());
    }
    // zsh with `RC_QUOTES` reads `''` inside single quotes as a quote
    // character; every other shell reads it as nothing.
    if backend == ShellBackend::Zsh && command.contains("''") {
        return Err(refuse_unparsed());
    }
    let chars: Vec<char> = command.chars().collect();
    let mut at = 0;
    let mut list = CommandList::default();
    let mut word = WordBuilder::default();
    while at < chars.len() {
        let character = chars[at];
        let next = chars.get(at + 1).copied();
        match character {
            ' ' | '\t' => {
                list.end_word(&mut word, backend);
                at += 1;
            }
            '\n' => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Newline);
                at += 1;
            }
            ';' => {
                // `;;` belongs to `case`, which is not in this language.
                if next == Some(';') {
                    return Err(refuse_grouping());
                }
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 1;
            }
            '&' if next == Some('&') => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 2;
            }
            // `&>` sends both streams to one target. `sh` has no such
            // operator and reads it as a background `&`.
            '&' if next == Some('>') && backend != ShellBackend::Sh => {
                list.end_word(&mut word, backend);
                at += 2;
                if chars.get(at) == Some(&'>') {
                    at += 1;
                }
                posix_redirection_to_null(&chars, &mut at)?;
            }
            '&' => return Err(refuse_background()),
            '|' if next == Some('|') => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 2;
            }
            '|' => {
                list.end_word(&mut word, backend);
                at += 1;
                if next == Some('&') {
                    if backend == ShellBackend::Sh {
                        return Err(refuse_background());
                    }
                    at += 1;
                }
                list.end_command(Separator::Pipe);
            }
            '<' => return Err(refuse_redirection()),
            '>' => {
                // Digits written against the operator are its descriptor.
                if word.is_descriptor() {
                    word = WordBuilder::default();
                } else {
                    list.end_word(&mut word, backend);
                }
                at += 1;
                match chars.get(at) {
                    Some('>' | '|') => {
                        at += 1;
                        posix_redirection_to_null(&chars, &mut at)?;
                    }
                    Some('&') => {
                        // `>&2`, `2>&1`, `>&-`: one descriptor made a copy of
                        // another. `>&word` is a file.
                        at += 1;
                        while matches!(chars.get(at), Some(' ' | '\t')) {
                            at += 1;
                        }
                        let start = at;
                        while chars.get(at).is_some_and(char::is_ascii_digit) {
                            at += 1;
                        }
                        if at == start && chars.get(at) == Some(&'-') {
                            at += 1;
                        }
                        let ends = chars.get(at).is_none_or(|next| is_posix_break(*next));
                        if at == start || !ends {
                            return Err(refuse_redirection());
                        }
                    }
                    _ => posix_redirection_to_null(&chars, &mut at)?,
                }
            }
            '#' if !word.started => {
                // Whether `#` starts a comment in the string zsh evaluates
                // depends on its options, so there the line asks.
                if backend == ShellBackend::Zsh {
                    return Err(refuse_unparsed());
                }
                while at < chars.len() && chars[at] != '\n' {
                    at += 1;
                }
            }
            _ => posix_word_character(&chars, &mut at, &mut word)?,
        }
    }
    list.end_word(&mut word, backend);
    list.end_command(Separator::Sequence);
    Ok(list.commands)
}

fn is_posix_break(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

/// Reads the target of an output redirection, which may only be the null
/// device: any other target is a file the command writes.
fn posix_redirection_to_null(chars: &[char], at: &mut usize) -> Result<(), ShellAssessment> {
    while matches!(chars.get(*at), Some(' ' | '\t')) {
        *at += 1;
    }
    let mut target = WordBuilder::default();
    while chars.get(*at).is_some_and(|next| !is_posix_break(*next)) {
        posix_word_character(chars, at, &mut target)?;
    }
    let text: String = target.chars.iter().map(|(character, _)| *character).collect();
    if text == "/dev/null" {
        Ok(())
    } else {
        Err(refuse_redirection())
    }
}

/// Reads one piece of a word at `at`: a quoted string, an escape, or a plain
/// character. Anything the shell would expand or group refuses the line.
fn posix_word_character(
    chars: &[char],
    at: &mut usize,
    word: &mut WordBuilder,
) -> Result<(), ShellAssessment> {
    let character = chars[*at];
    *at += 1;
    match character {
        '\'' => {
            word.started = true;
            loop {
                let Some(quoted) = chars.get(*at).copied() else {
                    return Err(refuse_unparsed());
                };
                *at += 1;
                match quoted {
                    '\'' => return Ok(()),
                    // Literal to the shell, and still not let through: the
                    // word may reach something that evaluates it again
                    // (`test -v 'a[$(cmd)]'` runs the substitution).
                    '$' | '`' => return Err(refuse_dynamic()),
                    quoted => word.push(quoted, false),
                }
            }
        }
        '"' => {
            word.started = true;
            loop {
                let Some(quoted) = chars.get(*at).copied() else {
                    return Err(refuse_unparsed());
                };
                *at += 1;
                match quoted {
                    '"' => return Ok(()),
                    '$' | '`' => return Err(refuse_dynamic()),
                    '\\' => match chars.get(*at).copied() {
                        None => return Err(refuse_unparsed()),
                        // An escaped expansion character is literal, and still
                        // not worth telling apart from a live one.
                        Some('$' | '`') => return Err(refuse_dynamic()),
                        Some(escaped @ ('"' | '\\')) => {
                            *at += 1;
                            word.push(escaped, false);
                        }
                        Some('\n') => *at += 1,
                        // Before any other character the backslash is itself.
                        Some(_) => word.push('\\', false),
                    },
                    quoted => word.push(quoted, false),
                }
            }
        }
        '\\' => match chars.get(*at).copied() {
            None => Err(refuse_unparsed()),
            // A line continuation: the two characters are not there.
            Some('\n') => {
                *at += 1;
                Ok(())
            }
            Some('$' | '`') => Err(refuse_dynamic()),
            Some(escaped) => {
                *at += 1;
                word.push(escaped, false);
                Ok(())
            }
        },
        '$' | '`' => Err(refuse_dynamic()),
        '(' | ')' | '{' | '}' => Err(refuse_grouping()),
        character => {
            word.push(character, true);
            Ok(())
        }
    }
}

/// The characters PowerShell reads as a dash or a quote besides the ASCII
/// ones: a parameter may begin with an en dash, a string with a curly quote.
fn is_powershell_lookalike(character: char) -> bool {
    matches!(character, '\u{2013}'..='\u{2015}' | '\u{2018}'..='\u{201F}')
}

fn lex_powershell(command: &str) -> Result<Vec<SimpleCommand>, ShellAssessment> {
    let backend = crate::shell_backend::ShellBackend::Pwsh;
    if has_unusual_characters(ShellKind::PowerShell, command)
        || command.chars().any(is_powershell_lookalike)
    {
        return Err(refuse_characters());
    }
    if has_dynamic_powershell_syntax(command) {
        return Err(refuse_dynamic());
    }
    let chars: Vec<char> = command.chars().collect();
    let mut at = 0;
    let mut list = CommandList::default();
    let mut word = WordBuilder::default();
    while at < chars.len() {
        let character = chars[at];
        let next = chars.get(at + 1).copied();
        match character {
            ' ' | '\t' => {
                list.end_word(&mut word, backend);
                at += 1;
            }
            '\n' | '\r' => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Newline);
                at += 1;
            }
            ';' => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 1;
            }
            '&' if next == Some('&') => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 2;
            }
            '&' => return Err(refuse_background()),
            '|' if next == Some('|') => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Sequence);
                at += 2;
            }
            '|' => {
                list.end_word(&mut word, backend);
                list.end_command(Separator::Pipe);
                at += 1;
            }
            '<' => return Err(refuse_redirection()),
            '>' => {
                // `2>`, `*>`: the stream written against the operator.
                let stream = word.chars.len() == 1
                    && matches!(word.chars[0], ('1'..='6' | '*', true));
                if stream {
                    word = WordBuilder::default();
                } else {
                    list.end_word(&mut word, backend);
                }
                at += 1;
                if chars.get(at) == Some(&'>') {
                    at += 1;
                }
                if chars.get(at) == Some(&'&') {
                    // `2>&1`: one stream merged into another.
                    let merged = matches!(chars.get(at + 1), Some('1' | '2'))
                        && chars
                            .get(at + 2)
                            .is_none_or(|next| is_powershell_break(*next));
                    if !merged {
                        return Err(refuse_redirection());
                    }
                    at += 2;
                    continue;
                }
                while matches!(chars.get(at), Some(' ' | '\t')) {
                    at += 1;
                }
                let start = at;
                while chars.get(at).is_some_and(|next| !is_powershell_break(*next)) {
                    at += 1;
                }
                let target: String = chars[start..at].iter().collect();
                if !target.eq_ignore_ascii_case("$null") {
                    return Err(refuse_redirection());
                }
            }
            '#' if !word.started => {
                while at < chars.len() && !matches!(chars[at], '\n' | '\r') {
                    at += 1;
                }
            }
            quote @ ('\'' | '"') => {
                word.started = true;
                at += 1;
                loop {
                    let Some(quoted) = chars.get(at).copied() else {
                        return Err(refuse_unparsed());
                    };
                    at += 1;
                    if quoted == quote {
                        // A doubled quote is one quote character.
                        if chars.get(at) == Some(&quote) {
                            at += 1;
                            word.push(quote, false);
                            continue;
                        }
                        break;
                    }
                    word.push(quoted, false);
                }
            }
            '{' | '}' => return Err(refuse_grouping()),
            character => {
                word.push(character, true);
                at += 1;
            }
        }
    }
    list.end_word(&mut word, backend);
    list.end_command(Separator::Sequence);
    Ok(list.commands)
}

fn is_powershell_break(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | ';' | '&' | '|' | '<' | '>')
}

/// PowerShell syntax that computes what runs: variables, subexpressions,
/// arrays, splatting, escapes, the stop-parsing token, `Invoke-Expression`.
///
/// Judged on the raw text, quoted or not — without PowerShell's own parser
/// there is no telling a literal `$` in a single-quoted string from a live one
/// with enough confidence to skip a prompt on it. The one `$` let through is
/// `$null` as the target of a redirection, which discards output.
fn has_dynamic_powershell_syntax(command: &str) -> bool {
    static DISCARD: OnceLock<Regex> = OnceLock::new();
    let discard = DISCARD.get_or_init(|| {
        Regex::new(r"(?i)>\s*\$null($|[\s;|&])").expect("static redirection pattern must compile")
    });
    let rest = discard.replace_all(command, "> ");
    let lowered = rest.to_ascii_lowercase();
    rest.contains(['$', '`', '@', '(', ')', ','])
        || rest.contains("--%")
        || lowered.contains("invoke-expression")
        || lowered
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .any(|word| word == "iex")
}

fn has_network_path(value: &str) -> bool {
    value.contains(r"\\")
}

/// Whether a word names a network or device path whichever way its slashes
/// lean: `//server/share` is a UNC path to MSYS and to PowerShell alike.
fn names_network_path(text: &str) -> bool {
    // An option's attached value: `--file=//server/x`, `-Path:\\server\x`.
    let value = if text.starts_with('-') {
        text.split_once(['=', ':']).map_or(text, |(_, value)| value)
    } else {
        text
    };
    let mut leading = value.chars();
    matches!(
        (leading.next(), leading.next()),
        (Some('/' | '\\'), Some('/' | '\\'))
    ) || text.contains(r"\\")
}

fn has_powershell_non_filesystem_provider(command: &str) -> bool {
    let lowered = command.to_ascii_lowercase();
    if [
        "registry::",
        "certificate::",
        "hklm:",
        "hkcu:",
        "hkcr:",
        "hku:",
        "hkcc:",
        "cert:",
        "env:",
        "alias:",
        "function:",
        "variable:",
        "wsman:",
    ]
    .iter()
    .any(|provider| lowered.contains(provider))
    {
        return true;
    }
    lowered.split_whitespace().any(|token| {
        let trimmed =
            token.trim_matches(|character| matches!(character, '"' | '\'' | '(' | ')' | ',' | ';'));
        if url::Url::parse(trimmed)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        {
            return false;
        }
        trimmed
            .find(":\\")
            .or_else(|| trimmed.find(":/"))
            .is_some_and(|index| index != 1)
    })
}

// --- Paths ------------------------------------------------------------------

/// A path as this computer's filesystem spells it.
enum HostPath {
    /// The null device, which holds nothing and keeps nothing.
    Null,
    Path { text: String, relative: bool },
}

fn unprovable_path(text: &str, zh: &str, en: &str) -> Reason {
    Reason::new(format!("路径 {text} {zh}"), format!("The path {text} {en}"))
}

/// Translates a path as the shell's programs read it into one this computer's
/// filesystem calls can check, or says why it cannot be checked.
///
/// The checks are on spellings where what the path guard resolves and what
/// the shell opens could differ: a path only MSYS can place, a Windows device
/// name, a component Win32 would trim, a `..` that follows a directory the
/// shell might reach through a symlink.
fn host_path(scene: &ShellScene<'_>, text: &str) -> Result<HostPath, Reason> {
    let posix = scene.kind == ShellKind::Bash;
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(unprovable_path(text, "为空或含有控制字符", "is empty or contains a control character"));
    }
    if posix && text == "/dev/null" {
        return Ok(HostPath::Null);
    }
    if names_network_path(text) {
        return Err(unprovable_path(text, "是网络或设备路径", "is a network or device path"));
    }
    let (text, relative, separator) = if cfg!(windows) {
        // MSYS programs and PowerShell both read either slash as a separator.
        let unified = text.replace('/', "\\");
        let bytes = unified.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            if bytes.get(2) != Some(&b'\\') {
                return Err(unprovable_path(text, "相对于某个盘符的当前目录", "is relative to a drive's own current directory"));
            }
            (unified, false, '\\')
        } else if let Some(rooted) = unified.strip_prefix('\\') {
            // `/c/Users/me` is how MSYS spells `C:\Users\me`. Every other
            // rooted path is a mount of its own (`/tmp`, `/usr`, `/proc`,
            // `/dev`) that only MSYS can place; to PowerShell a rooted path
            // is on whichever drive the shell is.
            let (drive, rest) = rooted.split_once('\\').unwrap_or((rooted, ""));
            let letter = drive.as_bytes().first().copied().filter(u8::is_ascii_alphabetic);
            match letter {
                Some(letter) if posix && drive.len() == 1 => (
                    format!("{}:\\{rest}", letter.to_ascii_uppercase() as char),
                    false,
                    '\\',
                ),
                _ => return Err(unprovable_path(text, "的位置由 Shell 的挂载表或当前盘符决定", "is placed by the shell's mount table or current drive")),
            }
        } else {
            (unified, true, '\\')
        }
    } else {
        // PowerShell reads either slash as a separator here too: to it
        // `..\outside` is `../outside`. Only a POSIX shell's word keeps a
        // backslash as part of a name.
        let unified = if posix { text.to_owned() } else { text.replace('\\', "/") };
        // A drive qualifier (`C:`, `Temp:`) names a PowerShell drive, whose
        // place the shell decides.
        if !posix && unified.split('/').next().is_some_and(|first| first.contains(':')) {
            return Err(unprovable_path(text, "的位置由 Shell 的挂载表或当前盘符决定", "is placed by the shell's mount table or current drive"));
        }
        if unified.starts_with('/') {
            if ["/dev", "/proc", "/sys"]
                .iter()
                .any(|special| unified == *special || unified.starts_with(&format!("{special}/")))
            {
                return Err(unprovable_path(text, "是设备或内核文件系统", "is a device or a kernel filesystem"));
            }
            (unified, false, '/')
        } else {
            (unified, true, '/')
        }
    };

    let mut named = false;
    for (index, component) in text.split(separator).enumerate() {
        match component {
            "" | "." => {}
            // A leading `..` climbs from a canonical directory, which is
            // exact. After a name it is not: the shell resolves `link/..`
            // from where the link points.
            ".." if relative && !named => {}
            ".." => return Err(unprovable_path(&text, "在目录名之后使用了 ..", "uses .. after a directory name")),
            // `C:` itself.
            _ if cfg!(windows) && !relative && index == 0 => {}
            component => {
                if cfg!(windows) && is_windows_unsafe_component(component) {
                    return Err(unprovable_path(&text, "含有 Windows 设备名或会被系统改写的文件名", "contains a Windows device name or a file name the system rewrites"));
                }
                named = true;
            }
        }
    }
    Ok(HostPath::Path { text, relative })
}

/// Whether Win32 would open something other than the file this component
/// names: a device (`NUL`, `COM1`, with any extension), a name it trims
/// (trailing dots and spaces), or a stream or wildcard.
fn is_windows_unsafe_component(component: &str) -> bool {
    if component.ends_with(['.', ' '])
        || component
            .chars()
            .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return true;
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit())
}

/// `candidate` is `root` or inside it. Both are canonical.
fn path_within(candidate: &Path, root: &Path) -> bool {
    if candidate.starts_with(root) {
        return true;
    }
    if cfg!(windows) {
        let candidate = candidate.to_string_lossy().to_lowercase();
        let root = root.to_string_lossy().to_lowercase();
        let root = root.trim_end_matches('\\');
        return candidate == root
            || (candidate.starts_with(root) && candidate.as_bytes().get(root.len()) == Some(&b'\\'));
    }
    false
}

/// Whether a write to `target` changes what a shell command runs rather than
/// what it works on: a repository's own directory (its configuration can name
/// a program every `git status` starts), a shell or Git startup file, or the
/// snapshot each Bash call sources.
///
/// These are the files that make "Accept edits writes without asking" and "a
/// proven command runs without asking" unsafe together, so at that level a
/// write to one asks — from a file tool and from a shell command alike.
fn is_shell_trust_anchor(target: &Path, app_data: &Path) -> bool {
    const STARTUP_FILES: &[&str] = &[
        ".gitconfig",
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".profile",
        ".zshenv",
        ".zprofile",
        ".zshrc",
        ".zlogin",
        ".ripgreprc",
    ];
    let named = |name: &std::ffi::OsStr, expected: &str| {
        name.to_str().is_some_and(|name| name.eq_ignore_ascii_case(expected))
    };
    target.components().any(|component| {
        matches!(component, Component::Normal(name) if named(name, ".git"))
    }) || target
        .file_name()
        .is_some_and(|name| STARTUP_FILES.iter().any(|file| named(name, file)))
        || path_within(target, &crate::shell_snapshot::snapshot_directory(app_data))
}

/// What a command's paths came to.
#[derive(Default)]
struct PathLedger {
    /// A path is outside the trusted roots.
    beyond: bool,
    /// The first path that could not be proven, and why.
    failure: Option<Reason>,
    facts: CommandFacts,
}

impl PathLedger {
    fn fail(&mut self, reason: Reason) {
        self.failure.get_or_insert(reason);
    }

    /// Resolves `word` and applies what holds for a read and a write alike.
    fn resolve(&mut self, scene: &ShellScene<'_>, word: &Lexeme, write: bool) -> Option<PathBuf> {
        self.facts.names_path = true;
        if word.wild || word.home {
            self.fail(unprovable_path(&word.text, "含有通配符或 ~ 展开", "contains a glob or a ~ expansion"));
            return None;
        }
        let (text, relative) = match host_path(scene, &word.text) {
            Err(reason) => {
                self.fail(reason);
                return None;
            }
            Ok(HostPath::Null) if write => {
                self.fail(unprovable_path(&word.text, "是设备", "is a device"));
                return None;
            }
            Ok(HostPath::Null) => return None,
            Ok(HostPath::Path { text, relative }) => (text, relative),
        };
        self.facts.uses_cwd |= relative;
        // Scope, not existence, as for the file tools: a target that is not
        // there yet is judged through its nearest existing ancestor, and a
        // symlink through where it points.
        let anywhere = ExecutionScope::Unrestricted.denying(scene.protected.clone());
        let Ok(resolved) = resolve_for_write_with_scope(&scene.base, &text, &anywhere) else {
            self.fail(unprovable_path(&word.text, "无法解析，或位于受保护的应用数据内", "cannot be resolved, or is inside protected application data"));
            return None;
        };
        // A directory that holds a protected store: a recursive read would
        // walk into it and a delete would take it along.
        if scene
            .protected
            .iter()
            .any(|protected| path_within(protected, &resolved))
        {
            self.fail(unprovable_path(&word.text, "包含受保护的应用数据", "contains protected application data"));
            return None;
        }
        Some(resolved)
    }

    fn read(&mut self, scene: &ShellScene<'_>, word: &Lexeme) -> Option<PathBuf> {
        let resolved = self.resolve(scene, word, false)?;
        if !scene.roots.iter().any(|root| path_within(&resolved, root)) {
            self.beyond = true;
        }
        Some(resolved)
    }

    /// The command reads the directory it runs in.
    fn read_working_directory(&mut self, scene: &ShellScene<'_>) {
        self.read(scene, &Lexeme::literal("."));
    }

    fn write(&mut self, scene: &ShellScene<'_>, word: &Lexeme) -> Option<PathBuf> {
        let resolved = self.resolve(scene, word, true)?;
        let inside = scene
            .write_roots
            .iter()
            .any(|root| path_within(&resolved, root) && !same_path(&resolved, root));
        // Nothing that is, or holds, a root, the directory the command runs
        // in, or the application's own data.
        let holds_a_root = scene.roots.iter().any(|root| path_within(root, &resolved))
            || path_within(&scene.base, &resolved)
            || path_within(scene.app_data, &resolved)
            || path_within(&resolved, scene.app_data);
        if !inside || holds_a_root {
            self.fail(unprovable_path(&word.text, "不在工作区内，或就是工作区、当前目录本身", "is not inside the workspace, or is the workspace or the working directory itself"));
            return None;
        }
        if is_shell_trust_anchor(&resolved, scene.app_data) {
            self.fail(unprovable_path(&word.text, "是仓库内部文件或 Shell 启动配置，改动它等于改动之后执行的命令", "is a repository internal or a shell startup file; changing it changes what later commands run"));
            return None;
        }
        Some(resolved)
    }
}

// --- Options ----------------------------------------------------------------

/// What an allowlisted option's argument is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OptionArg {
    /// Not a path: a count, a pattern, a format.
    Text,
    /// A file the command reads.
    Read,
    /// A file or directory the command writes.
    Write,
}

/// The options of one command that are known to neither write, execute, nor
/// reach a file the operands do not name. An option that is not listed is not
/// a reason to guess: the command asks.
///
/// Only options whose argument count is the same on GNU and BSD are listed
/// as taking one. Where they differ the option is listed as taking none, so
/// the word after it is judged as a path — which costs a question at worst —
/// or it is left out.
struct OptionTable {
    /// Short options that take no argument.
    flags: &'static str,
    /// Short options whose argument is optional and only ever attached
    /// (`-M90%`): the next word is never theirs.
    attached: &'static str,
    /// Short options that take an argument, attached or as the next word.
    valued: &'static [(char, OptionArg)],
    /// Long options that take no argument.
    long: &'static [&'static str],
    /// Long options that take an argument, as `--name=value` or the next word.
    long_valued: &'static [(&'static str, OptionArg)],
    /// Long options whose argument is optional and only ever attached.
    long_optional: &'static [&'static str],
    /// `-5` is a count (`head -5`, `grep -3`, `git log -5`).
    counts: bool,
    /// A long option's text argument may be the next word. Only for a
    /// program with one implementation, whose parser is the same everywhere
    /// (Git). For the rest it is read only when attached (`--lines=5`): some
    /// implementation takes the argument as optional, and then the word after
    /// the option is an operand, so it is left to be judged as one.
    long_text_takes_next: bool,
}

const NO_OPTIONS: OptionTable = OptionTable {
    flags: "",
    attached: "",
    valued: &[],
    long: &[],
    long_valued: &[],
    long_optional: &[],
    counts: false,
    long_text_takes_next: false,
};

#[derive(Default)]
struct Arguments {
    /// The options present, as `-r` or `--recursive`.
    options: Vec<String>,
    /// Option arguments, with the option each belongs to.
    values: Vec<(String, OptionArg, Lexeme)>,
    operands: Vec<Lexeme>,
    /// Words read as options, or taken as an option's argument, after the
    /// first operand. GNU's parsers accept options anywhere; BSD's, and GNU's
    /// under `POSIXLY_CORRECT`, stop at the first operand and read every
    /// later word as one more operand. A proof has to hold under both.
    late: Vec<Lexeme>,
}

impl Arguments {
    fn has(&self, names: &[&str]) -> bool {
        self.options.iter().any(|option| names.contains(&option.as_str()))
    }

    fn values_of(&self, names: &'static [&'static str]) -> impl Iterator<Item = &Lexeme> + '_ {
        self.values
            .iter()
            .filter(|(option, _, _)| names.contains(&option.as_str()))
            .map(|(_, _, value)| value)
    }
}

/// Reads `words` against `table`. `Err` names the first option that is not in
/// it, or that is missing its argument.
fn parse_arguments(table: &OptionTable, words: &[Lexeme]) -> Result<Arguments, String> {
    let mut arguments = Arguments::default();
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        index += 1;
        let text = word.text.as_str();
        if text == "--" {
            arguments.operands.extend(words[index..].iter().cloned());
            break;
        }
        let late = !arguments.operands.is_empty();
        if let Some(long) = text.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, _)) => (name, Some(word.tail(2 + name.len() + 1))),
                None => (long, None),
            };
            let option = format!("--{name}");
            if late {
                arguments.late.push(word.clone());
            }
            if let Some((_, role)) = table.long_valued.iter().find(|(known, _)| *known == name) {
                let value = match attached {
                    Some(value) => Some(value),
                    None if *role == OptionArg::Text && !table.long_text_takes_next => None,
                    None => {
                        let Some(value) = words.get(index) else {
                            return Err(option);
                        };
                        index += 1;
                        if late {
                            arguments.late.push(value.clone());
                        }
                        Some(value.clone())
                    }
                };
                if let Some(value) = value {
                    arguments.values.push((option.clone(), *role, value));
                }
            } else if table.long_optional.contains(&name) {
                if let Some(value) = attached {
                    arguments.values.push((option.clone(), OptionArg::Text, value));
                }
            } else if !table.long.contains(&name) || attached.is_some() {
                return Err(option);
            }
            arguments.options.push(option);
            continue;
        }
        let Some(cluster) = text.strip_prefix('-').filter(|cluster| !cluster.is_empty()) else {
            // A plain word, or `-` alone: standard input.
            arguments.operands.push(word.clone());
            continue;
        };
        if late {
            arguments.late.push(word.clone());
        }
        if table.counts && cluster.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        for (offset, flag) in cluster.char_indices() {
            let option = format!("-{flag}");
            let rest = 1 + offset + flag.len_utf8();
            if table.flags.contains(flag) {
                arguments.options.push(option);
                continue;
            }
            if table.attached.contains(flag) {
                if rest < text.len() {
                    arguments.values.push((option.clone(), OptionArg::Text, word.tail(rest)));
                }
                arguments.options.push(option);
                break;
            }
            let Some((_, role)) = table.valued.iter().find(|(known, _)| *known == flag) else {
                return Err(option);
            };
            let value = if rest < text.len() {
                word.tail(rest)
            } else {
                let Some(value) = words.get(index) else {
                    return Err(option);
                };
                index += 1;
                if late {
                    arguments.late.push(value.clone());
                }
                value.clone()
            };
            arguments.values.push((option.clone(), *role, value));
            arguments.options.push(option);
            break;
        }
    }
    Ok(arguments)
}

// --- POSIX commands ---------------------------------------------------------

fn refuse_option(command: &str, option: &str) -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.unverified_option",
        Reason::new(
            format!("{command} 的选项 {option} 不在已核对为只读的选项之内"),
            format!("{command}'s option {option} is not among the options checked to be read-only"),
        ),
    )
}

fn refuse_command(command: &str) -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.unclassified_command",
        Reason::new(
            format!("无法证明 {command} 只读或仅修改可信目录"),
            format!("{command} cannot be proven read-only or limited to trusted directories"),
        ),
    )
}

fn refuse_arguments(command: &str) -> ShellAssessment {
    ShellAssessment::unbounded(
        "shell.unverified_arguments",
        Reason::new(
            format!("{command} 的这种参数形式未被核对为只读"),
            format!("This form of {command}'s arguments has not been checked to be read-only"),
        ),
    )
}

/// The verdict for a command that only reads, given what its paths came to.
fn read_verdict(command: &str, ledger: PathLedger) -> ShellAssessment {
    let facts = ledger.facts;
    if let Some(reason) = ledger.failure {
        return ShellAssessment::unbounded("shell.unverifiable_path", reason).with_facts(facts);
    }
    if ledger.beyond {
        return ShellAssessment::read_beyond_trusted_roots(
            "shell.read_outside_trusted_roots",
            Reason::new("只读命令显式引用了工作区和应用数据目录之外的路径", "The read-only command names a path outside the workspace and the app data directory"),
        )
        .with_facts(facts);
    }
    ShellAssessment::read(
        "shell.read_only",
        Reason::new(
            format!("{command} 属于内置只读命令集合"),
            format!("{command} is in the built-in read-only command set"),
        ),
    )
    .with_facts(facts)
}

/// The verdict for one of the file-changing commands.
fn write_verdict(command: &str, mut ledger: PathLedger) -> ShellAssessment {
    ledger.facts.role = CommandRole::FileChange;
    let facts = ledger.facts;
    if let Some(reason) = ledger.failure {
        return ShellAssessment::unbounded("shell.file_change_unverified", reason).with_facts(facts);
    }
    ShellAssessment::workspace_write(
        "shell.workspace_file_edit",
        Reason::new(
            format!("{command} 只修改工作区内的显式路径"),
            format!("{command} only changes explicit paths inside the workspace"),
        ),
    )
    .with_facts(facts)
}

/// Environment assignments that cannot change which program runs or what it
/// loads. Anything else in front of a command — `PATH`, `LD_PRELOAD`,
/// `GIT_DIR`, `IFS`, `BASH_ENV` — is a reason to ask.
fn is_inert_environment_assignment(name: &str, value: &Lexeme) -> bool {
    matches!(
        name,
        "LANG"
            | "LANGUAGE"
            | "LC_ALL"
            | "LC_COLLATE"
            | "LC_CTYPE"
            | "LC_MESSAGES"
            | "LC_NUMERIC"
            | "LC_TIME"
            | "TZ"
            | "NO_COLOR"
            | "FORCE_COLOR"
            | "CLICOLOR"
            | "CLICOLOR_FORCE"
            | "TERM"
            | "COLORTERM"
            | "COLUMNS"
            | "LINES"
            | "CI"
            | "NODE_ENV"
    ) && !value.wild
        && !value.home
        // A value is a name, never a path: `TZ=:/etc/passwd` has the C
        // library open that file, and only a zone name (`America/New_York`)
        // has a use for a slash at all.
        && !value.text.starts_with('/')
        && !value.text.contains("..")
        && value.text.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '-' | '.' | '@' | '+' | ',')
                || (character == '/' && name == "TZ")
        })
}

fn shell_search_path_is_suspicious() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return true;
    };
    std::env::split_paths(&path).any(|entry| entry.as_os_str().is_empty() || !entry.is_absolute())
}

fn is_integer(text: &str) -> bool {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `1`, `2.5`, `10s`: a count of time for `sleep` and `timeout`.
fn is_duration(text: &str) -> bool {
    let number = text.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(text);
    let mut parts = number.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let fraction = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// Takes the wrappers that run the next word as the command off the front of
/// `words`. Each is read with its own options only: an option that is not
/// recognized might take the next word as its argument, and then the command
/// would be a different word than the one judged.
fn strip_posix_wrappers(
    backend: crate::shell_backend::ShellBackend,
    mut words: &[Lexeme],
) -> Result<&[Lexeme], ShellAssessment> {
    let refuse = |wrapper: &str| {
        ShellAssessment::unbounded(
            "shell.wrapper_unverifiable",
            Reason::new(
                format!("{wrapper} 的参数无法静态确定，之后的实际命令不明"),
                format!("{wrapper}'s arguments cannot be determined statically, so the command it runs is unclear"),
            ),
        )
    };
    for _ in 0..8 {
        let Some(first) = words.first() else {
            break;
        };
        let text = |index: usize| words.get(index).map(|word| word.text.as_str());
        let skip = match first.text.as_str() {
            "command" => match text(1) {
                // `command -v name` is a lookup, handled as a command.
                Some("-v" | "-V") => return Ok(words),
                Some(next) if next.starts_with('-') => return Err(refuse("command")),
                _ => 1,
            },
            "builtin" => 1,
            "noglob" if backend == crate::shell_backend::ShellBackend::Zsh => 1,
            "time" => match text(1) {
                Some("-p") => 2,
                Some(next) if next.starts_with('-') => return Err(refuse("time")),
                _ => 1,
            },
            "nice" => match text(1) {
                Some("-n") if text(2).is_some_and(is_integer) => 3,
                Some(next)
                    if next
                        .strip_prefix("--adjustment=")
                        .or_else(|| next.strip_prefix("-n"))
                        .or_else(|| next.strip_prefix('-'))
                        .is_some_and(is_integer) =>
                {
                    2
                }
                Some(next) if next.starts_with('-') => return Err(refuse("nice")),
                _ => 1,
            },
            "timeout" => {
                let mut index = 1;
                loop {
                    match text(index) {
                        Some("--foreground" | "--preserve-status" | "-v" | "--verbose") => index += 1,
                        Some("-k" | "-s") if text(index + 1).is_some() => index += 2,
                        Some(next)
                            if next.starts_with("--kill-after=") || next.starts_with("--signal=") =>
                        {
                            index += 1
                        }
                        Some(next) if next.starts_with('-') => return Err(refuse("timeout")),
                        _ => break,
                    }
                }
                if !text(index).is_some_and(is_duration) {
                    return Err(refuse("timeout"));
                }
                index + 1
            }
            "stdbuf" => {
                let is_mode = |mode: &str| {
                    mode == "L"
                        || mode == "0"
                        || mode
                            .trim_end_matches(['K', 'M', 'G', 'B'])
                            .bytes()
                            .all(|byte| byte.is_ascii_digit())
                            && !mode.is_empty()
                };
                let mut index = 1;
                loop {
                    match text(index) {
                        Some("-i" | "-o" | "-e") if text(index + 1).is_some_and(is_mode) => index += 2,
                        Some(next)
                            if ["-i", "-o", "-e", "--input=", "--output=", "--error="]
                                .iter()
                                .any(|option| {
                                    next.strip_prefix(option).is_some_and(|mode| is_mode(mode))
                                }) =>
                        {
                            index += 1
                        }
                        Some(next) if next.starts_with('-') => return Err(refuse("stdbuf")),
                        _ => break,
                    }
                }
                index
            }
            _ => break,
        };
        words = &words[skip.min(words.len())..];
    }
    if words.is_empty() {
        return Err(ShellAssessment::unbounded(
            "shell.wrapper_missing_command",
            Reason::new(
                "进程包装器后没有可分析的实际命令",
                "No command to analyze follows the process wrapper",
            ),
        ));
    }
    Ok(words)
}

fn analyze_posix_command(scene: &ShellScene<'_>, command: &SimpleCommand) -> ShellAssessment {
    let mut words = command.words.as_slice();
    // A glob or a `~` anywhere is an argument the shell writes, not the model.
    if let Some(word) = words.iter().find(|word| word.wild || word.home) {
        return ShellAssessment::unbounded(
            "shell.dynamic_read_path",
            Reason::new(
                format!("{} 中的 glob 或 home 展开由 Shell 在运行时决定，可能越过可信目录或变成选项", word.text),
                format!("The glob or home expansion in {} is decided by the shell at run time and may reach beyond the trusted directories or turn into an option", word.text),
            ),
        );
    }
    while let Some(name_length) = words.first().and_then(|word| word.assigns) {
        let word = &words[0];
        let name = &word.text[..name_length];
        if !is_inert_environment_assignment(name, &word.tail(name_length + 1)) {
            return ShellAssessment::unbounded(
                "shell.environment_assignment_unverified",
                Reason::new(
                    format!("命令前设置了 {name}，它可能改变实际运行的程序或其加载的内容"),
                    format!("The command sets {name} first, which may change the program that runs or what it loads"),
                ),
            );
        }
        words = &words[1..];
    }
    if words.is_empty() {
        return ShellAssessment::read(
            "shell.environment_assignment",
            Reason::new(
                "只设置了当前子进程的环境变量",
                "Only sets environment variables for the child process",
            ),
        );
    }
    let words = match strip_posix_wrappers(scene.backend, words) {
        Ok(words) => words,
        Err(refusal) => return refusal,
    };
    let name = words[0].text.as_str();
    if name.contains(['/', '\\']) {
        return ShellAssessment::unbounded(
            "shell.explicit_executable_path",
            Reason::new("命令通过显式路径选择可执行文件，不能仅凭文件名白名单证明其实现安全", "The command picks its executable by explicit path, so a file-name allowlist cannot prove it safe"),
        );
    }
    let arguments = &words[1..];
    // The shell's own commands resolve before `PATH` is consulted.
    let builtin = matches!(
        name,
        "cd" | "echo" | "printf" | "pwd" | "true" | "false" | "test" | "[" | "type" | "command"
    );
    if !builtin && shell_search_path_is_suspicious() {
        return ShellAssessment::unbounded(
            "shell.suspicious_search_path",
            Reason::new("进程 PATH 含相对或空目录，白名单命令名可能解析到工作区中的替代程序", "The process PATH has a relative or empty entry, so an allowlisted command name may resolve to a substitute in the workspace"),
        );
    }
    match name {
        "cd" => analyze_cd(scene, arguments),
        "git" => analyze_git(scene, arguments),
        "find" => analyze_find(scene, arguments),
        "sed" => analyze_sed(scene, arguments),
        "mkdir" | "touch" | "rm" | "rmdir" | "mv" | "cp" => analyze_file_change(scene, name, arguments),
        "xargs" => ShellAssessment::unbounded(
            "shell.xargs_data_dependent_command",
            Reason::new("xargs 会把运行时输入拼接为新的命令或参数，静态分类器无法看到最终载荷", "xargs builds new commands or arguments from runtime input, so the static classifier cannot see what finally runs"),
        ),
        _ => analyze_posix_reader(scene, name, arguments),
    }
}

/// What a reading command's plain arguments are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operands {
    /// Nothing the command opens: text to print, names to look up.
    Text,
    /// Files it reads.
    Paths,
    /// A pattern, then files — unless an option already gave the pattern.
    PatternThenPaths,
    /// None at all.
    None,
}

/// What a reading command reads when it is given no file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unnamed {
    /// Standard input, or nothing.
    Nothing,
    /// The directory it runs in.
    WorkingDirectory,
}

struct ReaderRule {
    table: &'static OptionTable,
    operands: Operands,
    unnamed: Unnamed,
}

fn posix_reader_rule(name: &str) -> Option<ReaderRule> {
    use OptionArg::{Read, Text};
    let rule = |table: &'static OptionTable, operands, unnamed| {
        Some(ReaderRule {
            table,
            operands,
            unnamed,
        })
    };
    match name {
        "cat" => {
            static TABLE: OptionTable = OptionTable {
                flags: "AbeEnstTuv",
                long: &["show-all", "number-nonblank", "show-ends", "number", "squeeze-blank", "show-tabs", "show-nonprinting"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "head" | "tail" => {
            static TABLE: OptionTable = OptionTable {
                flags: "fFqvzr",
                valued: &[('n', Text), ('c', Text)],
                long: &["quiet", "silent", "verbose", "zero-terminated", "retry"],
                long_valued: &[("lines", Text), ("bytes", Text), ("pid", Text), ("sleep-interval", Text), ("max-unchanged-stats", Text)],
                long_optional: &["follow"],
                counts: true,
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "wc" => {
            static TABLE: OptionTable = OptionTable {
                flags: "clmwL",
                long: &["bytes", "chars", "lines", "words", "max-line-length"],
                long_valued: &[("total", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "ls" => {
            static TABLE: OptionTable = OptionTable {
                flags: "aAbBcCdfFgGhHiklmnNopqQrRsStuUvxXZ1",
                long: &[
                    "all", "almost-all", "author", "escape", "ignore-backups", "directory", "file-type", "full-time",
                    "group-directories-first", "no-group", "human-readable", "si", "inode", "kibibytes",
                    "numeric-uid-gid", "literal", "hide-control-chars", "show-control-chars", "quote-name",
                    "reverse", "recursive", "size", "context", "zero", "dereference-command-line",
                ],
                long_valued: &[
                    ("block-size", Text), ("format", Text), ("hide", Text), ("ignore", Text), ("indicator-style", Text),
                    ("quoting-style", Text), ("sort", Text), ("time", Text), ("time-style", Text), ("width", Text), ("tabsize", Text),
                ],
                long_optional: &["color", "classify", "hyperlink"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::WorkingDirectory)
        }
        "grep" | "egrep" | "fgrep" => {
            static TABLE: OptionTable = OptionTable {
                flags: "EFGPiyvwxcLlqsbHhnoTUzZaIr",
                valued: &[('e', Text), ('f', Read), ('m', Text), ('A', Text), ('B', Text), ('C', Text)],
                long: &[
                    "extended-regexp", "fixed-strings", "basic-regexp", "perl-regexp", "ignore-case", "no-ignore-case",
                    "invert-match", "word-regexp", "line-regexp", "count", "files-without-match", "files-with-matches",
                    "quiet", "silent", "no-messages", "byte-offset", "with-filename", "no-filename", "line-number",
                    "only-matching", "initial-tab", "null", "null-data", "text", "recursive", "line-buffered", "binary",
                    "no-group-separator",
                ],
                long_valued: &[
                    ("regexp", Text), ("file", Read), ("max-count", Text), ("after-context", Text), ("before-context", Text),
                    ("label", Text), ("binary-files", Text),
                    ("include", Text), ("exclude", Text), ("exclude-dir", Text), ("exclude-from", Read), ("group-separator", Text),
                ],
                // `--context` takes its count attached only: BSD grep reads
                // a separate word after it as the pattern.
                long_optional: &["color", "colour", "context"],
                counts: true,
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::PatternThenPaths, Unnamed::Nothing)
        }
        "rg" => {
            static TABLE: OptionTable = OptionTable {
                flags: "iSsFwxvclnNHIUPoaupbq0.",
                valued: &[
                    ('e', Text), ('g', Text), ('t', Text), ('T', Text), ('j', Text), ('A', Text), ('B', Text), ('C', Text),
                    ('m', Text), ('M', Text), ('E', Text), ('r', Text), ('d', Text), ('f', Read),
                ],
                long: &[
                    "ignore-case", "smart-case", "case-sensitive", "fixed-strings", "word-regexp", "line-regexp",
                    "invert-match", "count", "count-matches", "files-with-matches", "files-without-match", "files",
                    "line-number", "no-line-number", "with-filename", "no-filename", "hidden", "no-ignore",
                    "no-ignore-vcs", "no-ignore-parent", "no-ignore-global", "multiline", "multiline-dotall", "pcre2",
                    "only-matching", "text", "unrestricted", "null", "byte-offset", "column", "no-column", "heading",
                    "no-heading", "json", "stats", "trim", "vimgrep", "pretty", "quiet", "no-messages", "no-config",
                    "one-file-system", "crlf", "passthru", "sort-files", "line-buffered", "block-buffered", "no-unicode",
                ],
                long_valued: &[
                    ("regexp", Text), ("glob", Text), ("iglob", Text), ("type", Text), ("type-not", Text), ("type-add", Text),
                    ("max-count", Text), ("max-depth", Text), ("maxdepth", Text), ("after-context", Text),
                    ("before-context", Text), ("context", Text), ("threads", Text), ("encoding", Text), ("max-columns", Text),
                    ("max-filesize", Text), ("replace", Text), ("sort", Text), ("sortr", Text), ("color", Text), ("colors", Text),
                    ("context-separator", Text), ("field-match-separator", Text), ("field-context-separator", Text),
                    ("path-separator", Text), ("engine", Text), ("file", Read), ("ignore-file", Read),
                ],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::PatternThenPaths, Unnamed::WorkingDirectory)
        }
        "tree" => {
            static TABLE: OptionTable = OptionTable {
                flags: "adfxpugshDFCniqNrtUvJ",
                valued: &[('L', Text), ('I', Text), ('P', Text)],
                long: &["dirsfirst", "noreport", "prune", "du", "si", "gitignore"],
                long_valued: &[("filelimit", Text), ("charset", Text), ("sort", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::WorkingDirectory)
        }
        "du" => {
            static TABLE: OptionTable = OptionTable {
                flags: "abchHklmsSx0",
                valued: &[('d', Text), ('B', Text), ('t', Text)],
                long: &[
                    "all", "apparent-size", "bytes", "total", "human-readable", "si", "summarize", "separate-dirs",
                    "one-file-system", "null", "count-links", "inodes", "dereference-args", "no-dereference",
                ],
                long_valued: &[("max-depth", Text), ("block-size", Text), ("threshold", Text), ("exclude", Text), ("time-style", Text)],
                long_optional: &["time"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::WorkingDirectory)
        }
        "df" => {
            static TABLE: OptionTable = OptionTable {
                flags: "ahHiklmPTt",
                valued: &[('B', Text), ('x', Text)],
                long: &["all", "human-readable", "si", "inodes", "local", "portability", "print-type", "total", "no-sync"],
                long_valued: &[("block-size", Text), ("type", Text), ("exclude-type", Text)],
                long_optional: &["output"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "stat" => {
            static TABLE: OptionTable = OptionTable {
                flags: "LftxnqFlrs",
                valued: &[('c', Text)],
                long: &["dereference", "file-system", "terse"],
                long_valued: &[("format", Text), ("printf", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "file" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bihkLNnr0I",
                valued: &[('F', Text), ('e', Text)],
                long: &[
                    "brief", "mime", "mime-type", "mime-encoding", "dereference", "no-dereference", "keep-going",
                    "no-buffer", "no-pad", "raw", "print0",
                ],
                long_valued: &[("separator", Text), ("exclude", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "diff" => {
            static TABLE: OptionTable = OptionTable {
                flags: "qsucyabBwiEZtTdNp",
                valued: &[('U', Text), ('C', Text), ('W', Text), ('I', Text), ('F', Text), ('L', Text), ('x', Text)],
                long: &[
                    "brief", "report-identical-files", "side-by-side", "text", "ignore-space-change", "ignore-blank-lines",
                    "ignore-all-space", "ignore-case", "ignore-tab-expansion", "ignore-trailing-space", "expand-tabs",
                    "initial-tab", "minimal", "new-file", "show-c-function", "strip-trailing-cr", "no-dereference",
                    "suppress-common-lines", "left-column", "normal",
                ],
                long_valued: &[
                    ("label", Text), ("width", Text), ("ignore-matching-lines", Text), ("show-function-line", Text),
                    ("tabsize", Text), ("horizon-lines", Text), ("exclude", Text),
                ],
                long_optional: &["unified", "context", "color"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "cmp" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bls",
                valued: &[('i', Text), ('n', Text)],
                long: &["print-bytes", "verbose", "quiet", "silent"],
                long_valued: &[("ignore-initial", Text), ("bytes", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "comm" => {
            static TABLE: OptionTable = OptionTable {
                flags: "123z",
                long: &["check-order", "nocheck-order", "zero-terminated", "total"],
                long_valued: &[("output-delimiter", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        // No `-o`, `-T` or `--compress-program`: they write a file, write
        // temporary files where told, and run a program.
        "sort" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bdfghiMnRrVcCmsuz",
                valued: &[('k', Text), ('t', Text), ('S', Text)],
                long: &[
                    "ignore-leading-blanks", "dictionary-order", "ignore-case", "general-numeric-sort",
                    "human-numeric-sort", "ignore-nonprinting", "month-sort", "numeric-sort", "random-sort", "reverse",
                    "version-sort", "merge", "stable", "unique", "zero-terminated", "debug",
                ],
                long_valued: &[("key", Text), ("field-separator", Text), ("buffer-size", Text), ("parallel", Text), ("sort", Text), ("batch-size", Text)],
                long_optional: &["check"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "uniq" => {
            static TABLE: OptionTable = OptionTable {
                flags: "cdDiuz",
                valued: &[('f', Text), ('s', Text), ('w', Text)],
                long: &["count", "repeated", "ignore-case", "unique", "zero-terminated"],
                long_valued: &[("skip-fields", Text), ("skip-chars", Text), ("check-chars", Text)],
                long_optional: &["all-repeated", "group"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "cut" => {
            static TABLE: OptionTable = OptionTable {
                flags: "snz",
                valued: &[('b', Text), ('c', Text), ('f', Text), ('d', Text)],
                long: &["complement", "only-delimited", "zero-terminated"],
                long_valued: &[("bytes", Text), ("characters", Text), ("fields", Text), ("delimiter", Text), ("output-delimiter", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "tr" => {
            static TABLE: OptionTable = OptionTable {
                flags: "cCdst",
                long: &["complement", "delete", "squeeze-repeats", "truncate-set1"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "column" => {
            static TABLE: OptionTable = OptionTable {
                flags: "tx",
                valued: &[('s', Text), ('c', Text), ('o', Text)],
                long: &["table", "fillrows"],
                long_valued: &[("separator", Text), ("output-separator", Text), ("columns", Text), ("output-width", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "rev" => rule(&NO_OPTIONS, Operands::Paths, Unnamed::Nothing),
        "tac" => {
            static TABLE: OptionTable = OptionTable {
                flags: "br",
                valued: &[('s', Text)],
                long: &["before", "regex"],
                long_valued: &[("separator", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "fold" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bs",
                valued: &[('w', Text)],
                long: &["bytes", "spaces"],
                long_valued: &[("width", Text)],
                counts: true,
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "nl" => {
            static TABLE: OptionTable = OptionTable {
                flags: "p",
                valued: &[('b', Text), ('d', Text), ('f', Text), ('h', Text), ('i', Text), ('l', Text), ('n', Text), ('s', Text), ('v', Text), ('w', Text)],
                long: &["no-renumber"],
                long_valued: &[
                    ("body-numbering", Text), ("header-numbering", Text), ("footer-numbering", Text), ("line-increment", Text),
                    ("join-blank-lines", Text), ("number-format", Text), ("number-separator", Text),
                    ("starting-line-number", Text), ("number-width", Text), ("section-delimiter", Text),
                ],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "paste" => {
            static TABLE: OptionTable = OptionTable {
                flags: "sz",
                valued: &[('d', Text)],
                long: &["serial", "zero-terminated"],
                long_valued: &[("delimiters", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "od" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bcdDfFhHiIlLoOsvxX",
                valued: &[('A', Text), ('j', Text), ('N', Text), ('t', Text)],
                long: &["output-duplicates", "traditional"],
                long_valued: &[("address-radix", Text), ("skip-bytes", Text), ("read-bytes", Text), ("format", Text), ("endian", Text)],
                long_optional: &["width", "strings"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "hexdump" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bcCdovx",
                valued: &[('n', Text), ('s', Text), ('e', Text)],
                long: &["canonical", "no-squeezing"],
                long_valued: &[("length", Text), ("skip", Text), ("format", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "strings" => {
            static TABLE: OptionTable = OptionTable {
                flags: "afw",
                valued: &[('n', Text), ('t', Text), ('e', Text)],
                long: &["all", "print-file-name", "include-all-whitespace"],
                long_valued: &[("bytes", Text), ("radix", Text), ("encoding", Text), ("output-separator", Text)],
                counts: true,
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        // No `-o`: macOS writes the result to the file it names.
        "base64" => {
            static TABLE: OptionTable = OptionTable {
                flags: "dDi",
                valued: &[('w', Text)],
                long: &["decode", "ignore-garbage"],
                long_valued: &[("wrap", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        // No `-c`: checking reads every file a list names.
        "md5sum" | "sha1sum" | "sha224sum" | "sha256sum" | "sha384sum" | "sha512sum" | "b2sum" | "cksum" | "shasum" => {
            static TABLE: OptionTable = OptionTable {
                flags: "btz",
                valued: &[('a', Text)],
                long: &["binary", "text", "tag", "zero"],
                long_valued: &[("algorithm", Text), ("length", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "readlink" => {
            static TABLE: OptionTable = OptionTable {
                flags: "femnqsvz",
                long: &["canonicalize", "canonicalize-existing", "canonicalize-missing", "no-newline", "quiet", "silent", "verbose", "zero"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "realpath" => {
            static TABLE: OptionTable = OptionTable {
                flags: "emsqzLP",
                long: &["canonicalize-existing", "canonicalize-missing", "strip", "no-symlinks", "quiet", "zero", "logical", "physical"],
                long_valued: &[("relative-to", Read), ("relative-base", Read)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Paths, Unnamed::Nothing)
        }
        "basename" => {
            static TABLE: OptionTable = OptionTable {
                flags: "az",
                valued: &[('s', Text)],
                long: &["multiple", "zero"],
                long_valued: &[("suffix", Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "dirname" => {
            static TABLE: OptionTable = OptionTable {
                flags: "z",
                long: &["zero"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "pwd" => {
            static TABLE: OptionTable = OptionTable {
                flags: "LP",
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::None, Unnamed::Nothing)
        }
        "whoami" | "nproc" | "uptime" | "arch" => rule(&NO_OPTIONS, Operands::None, Unnamed::Nothing),
        "id" => {
            static TABLE: OptionTable = OptionTable {
                flags: "ugGnrz",
                long: &["user", "group", "groups", "name", "real", "zero"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "uname" => {
            static TABLE: OptionTable = OptionTable {
                flags: "asnrvmpio",
                long: &[
                    "all", "kernel-name", "nodename", "kernel-release", "kernel-version", "machine", "processor",
                    "hardware-platform", "operating-system",
                ],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::None, Unnamed::Nothing)
        }
        // No operand: a name given to `hostname` is the name it sets.
        "hostname" => {
            static TABLE: OptionTable = OptionTable {
                flags: "fsiIdA",
                long: &["fqdn", "long", "short", "ip-address", "all-ip-addresses", "domain", "all-fqdns"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::None, Unnamed::Nothing)
        }
        "which" => {
            static TABLE: OptionTable = OptionTable {
                flags: "as",
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "type" => {
            static TABLE: OptionTable = OptionTable {
                flags: "atpPf",
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        "free" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bkmghtw",
                long: &["bytes", "kilo", "mega", "giga", "human", "total", "wide", "si"],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::None, Unnamed::Nothing)
        }
        "pgrep" => {
            static TABLE: OptionTable = OptionTable {
                flags: "flaxnoci",
                valued: &[('u', Text), ('U', Text), ('g', Text), ('G', Text), ('P', Text), ('t', Text), ('d', Text)],
                ..NO_OPTIONS
            };
            rule(&TABLE, Operands::Text, Unnamed::Nothing)
        }
        _ => None,
    }
}

fn analyze_posix_reader(scene: &ShellScene<'_>, name: &str, words: &[Lexeme]) -> ShellAssessment {
    let pure = |ledger: PathLedger| read_verdict(name, ledger);
    match name {
        // Printed as written: no option of either does anything else.
        "echo" | "true" | "false" => return pure(PathLedger::default()),
        "printf" => {
            // `-v` assigns a shell variable and `%n` stores a count in one:
            // `printf -v PATH .` decides what the next command name runs.
            let assigns = words.first().is_some_and(|word| word.text.starts_with('-'))
                || words.iter().any(|word| printf_stores(&word.text));
            return if assigns {
                refuse_arguments(name)
            } else {
                pure(PathLedger::default())
            };
        }
        "command" => {
            // Only the lookup form reaches here.
            return if words.iter().skip(1).all(|word| !word.text.starts_with('-')) {
                pure(PathLedger::default())
            } else {
                refuse_arguments(name)
            };
        }
        "sleep" => {
            return if !words.is_empty() && words.iter().all(|word| is_duration(&word.text)) {
                pure(PathLedger::default())
            } else {
                refuse_arguments(name)
            };
        }
        "seq" => {
            let plain = words.iter().all(|word| {
                let text = word.text.as_str();
                text == "-w"
                    || text.strip_prefix(['-', '+']).unwrap_or(text).chars().all(|character| {
                        character.is_ascii_digit() || character == '.'
                    }) && !text.is_empty()
            });
            return if plain {
                pure(PathLedger::default())
            } else {
                refuse_arguments(name)
            };
        }
        "date" => {
            use OptionArg::{Read, Text};
            // No `-s`: it sets the clock, as a bare operand does on BSD, where
            // `-d` sets the kernel's daylight-saving flag too.
            static TABLE: OptionTable = OptionTable {
                flags: "uRI",
                valued: &[('r', Read)],
                long: &["utc", "universal", "rfc-email", "rfc-2822"],
                long_valued: &[("date", Text), ("reference", Read), ("rfc-3339", Text)],
                long_optional: &["iso-8601"],
                ..NO_OPTIONS
            };
            let arguments = match parse_arguments(&TABLE, words) {
                Ok(arguments) => arguments,
                Err(option) => return refuse_option(name, &option),
            };
            if !arguments.late.is_empty()
                || !arguments.operands.iter().all(|operand| operand.text.starts_with('+'))
            {
                return refuse_arguments(name);
            }
            let mut ledger = PathLedger::default();
            for (_, role, value) in &arguments.values {
                if *role == Read {
                    ledger.read(scene, value);
                }
            }
            return pure(ledger);
        }
        "test" | "[" => {
            // Every operand is judged as a path: a file test names one, and a
            // string that is not a path resolves harmlessly inside the
            // directory the command runs in.
            // `-v name[subscript]` and `-R` evaluate the subscript as
            // arithmetic, which can assign a variable.
            if words.iter().any(|word| matches!(word.text.as_str(), "-v" | "-R")) {
                return refuse_arguments(name);
            }
            let mut ledger = PathLedger::default();
            for word in words {
                let text = word.text.as_str();
                let operator = matches!(text, "]" | "!" | "=" | "==" | "!=")
                    || (text.starts_with('-')
                        && text.len() <= 3
                        && text[1..].bytes().all(|byte| byte.is_ascii_alphabetic()));
                if !operator {
                    ledger.read(scene, word);
                }
            }
            return pure(ledger);
        }
        "ps" => {
            // The spellings that list processes. BSD's `e` prints each one's
            // environment, so a cluster is matched whole.
            let listing = words.iter().all(|word| {
                matches!(
                    word.text.as_str(),
                    "aux" | "ax" | "-e" | "-f" | "-ef" | "-A" | "-a" | "-x" | "-l" | "-ax" | "-aux"
                )
            });
            return if listing {
                pure(PathLedger::default())
            } else {
                refuse_arguments(name)
            };
        }
        _ => {}
    }
    let Some(rule) = posix_reader_rule(name) else {
        return refuse_command(name);
    };
    // binutils reads `@file` as more arguments, taken from that file.
    if name == "strings" && words.iter().any(|word| word.text.starts_with('@')) {
        return refuse_arguments(name);
    }
    let arguments = match parse_arguments(rule.table, words) {
        Ok(arguments) => arguments,
        Err(option) => return refuse_option(name, &option),
    };
    let mut ledger = PathLedger::default();
    // To a parser that stops at the first operand, an option after it is a
    // file to open. ripgrep has one parser, which does not stop; `uniq`'s
    // second operand is the file it writes.
    if !arguments.late.is_empty() && rule.operands != Operands::Text && name != "rg" {
        if name == "uniq" {
            return refuse_arguments(name);
        }
        for word in &arguments.late {
            if word.text.starts_with('-') {
                // A name no path spells: all that matters is that no file
                // answers to it here.
                ledger.facts.uses_cwd = true;
                if fs::symlink_metadata(scene.base.join(&word.text)).is_ok() {
                    ledger.fail(unprovable_path(&word.text, "既像选项又是当前目录里的文件", "reads as an option and is also a file in the working directory"));
                }
            } else {
                ledger.read(scene, word);
            }
        }
    }
    for (_, role, value) in &arguments.values {
        if *role == OptionArg::Read {
            ledger.read(scene, value);
        }
    }
    let paths: &[Lexeme] = match rule.operands {
        Operands::Text => &[],
        Operands::None if arguments.operands.is_empty() => &[],
        Operands::None => return refuse_arguments(name),
        Operands::Paths => &arguments.operands,
        Operands::PatternThenPaths => {
            let pattern_given = arguments.has(&["-e", "-f", "--regexp", "--file", "--files"]);
            if pattern_given {
                &arguments.operands
            } else {
                arguments.operands.get(1..).unwrap_or_default()
            }
        }
    };
    // `uniq in out` writes its second operand.
    if name == "uniq" && paths.len() > 1 {
        return refuse_arguments(name);
    }
    let mut named = 0;
    for path in paths {
        // `-` is standard input.
        if path.text == "-" {
            continue;
        }
        named += 1;
        let resolved = ledger.read(scene, path);
        // `diff` given a directory reads the files in it, through any link.
        if name == "diff" && resolved.is_some_and(|resolved| resolved.is_dir()) {
            ledger.fail(unprovable_path(&path.text, "是目录，diff 会跟随其中的符号链接", "is a directory, and diff follows the symlinks in it"));
        }
    }
    let recursive_grep =
        matches!(name, "grep" | "egrep" | "fgrep") && arguments.has(&["-r", "--recursive"]);
    if named == 0 && (rule.unnamed == Unnamed::WorkingDirectory || recursive_grep) {
        ledger.read_working_directory(scene);
    }
    read_verdict(name, ledger)
}

/// Whether a `printf` argument holds a `%n` conversion.
fn printf_stores(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '%' {
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            continue;
        }
        // Flags, width, precision, and the C length modifiers Bash accepts
        // (`%hn`, `%ln`).
        while chars.peek().is_some_and(|next| {
            next.is_ascii_digit()
                || matches!(
                    next,
                    '-' | '+' | ' ' | '#' | '.' | '*' | '\'' | 'h' | 'l' | 'L' | 'q' | 'j' | 'z' | 't'
                )
        }) {
            chars.next();
        }
        if chars.peek() == Some(&'n') {
            return true;
        }
    }
    false
}

fn analyze_cd(scene: &ShellScene<'_>, words: &[Lexeme]) -> ShellAssessment {
    let target = match words {
        [target] => target,
        [flag, target] if matches!(flag.text.as_str(), "--" | "-P" | "-L") => target,
        [_, _, ..] => return refuse_arguments("cd"),
        _ => {
            return ShellAssessment::unbounded(
                "shell.cd_home",
                Reason::new(
                    "未指定目标的 cd 会进入用户主目录，超出可信工作区",
                    "cd without a target goes to the home directory, outside the trusted workspace",
                ),
            )
        }
    };
    let mut ledger = PathLedger::default();
    ledger.facts.role = CommandRole::ChangeDirectory;
    // `cd -` is wherever the shell was before.
    let resolved = if target.text == "-" {
        None
    } else {
        ledger.read(scene, target)
    };
    let facts = ledger.facts;
    match resolved {
        Some(resolved) if ledger.failure.is_none() && !ledger.beyond && resolved.is_dir() => {
            ShellAssessment::read(
                "shell.cd_trusted",
                Reason::new("cd 目标位于可信目录内", "The cd target is inside a trusted directory"),
            )
            .with_facts(facts)
        }
        _ => ShellAssessment::unbounded(
            "shell.cd_outside_trusted_roots",
            Reason::new("cd 目标无法证明位于工作区或应用数据目录内", "The cd target cannot be proven to be inside the workspace or the app data directory"),
        )
        .with_facts(facts),
    }
}

fn analyze_find(scene: &ShellScene<'_>, words: &[Lexeme]) -> ShellAssessment {
    // Tests and actions that only match and print. Not here, and so asked
    // about: `-exec`, `-execdir`, `-ok`, `-okdir`, `-delete`, `-fprint`,
    // `-fprint0`, `-fprintf`, `-fls`, `-files0-from`, `-L`, `-follow`.
    const PLAIN: &[&str] = &[
        "-print", "-print0", "-ls", "-prune", "-quit", "-depth", "-d", "-empty", "-true", "-false", "-nouser",
        "-nogroup", "-readable", "-writable", "-executable", "-xdev", "-mount", "-daystart", "-noleaf",
        "-ignore_readdir_race", "-noignore_readdir_race", "-warn", "-nowarn", "-not", "-a", "-and", "-o", "-or",
        "!", "(", ")", ",",
    ];
    const VALUED: &[&str] = &[
        "-name", "-iname", "-path", "-ipath", "-wholename", "-iwholename", "-lname", "-ilname", "-regex",
        "-iregex", "-regextype", "-type", "-xtype", "-maxdepth", "-mindepth", "-size", "-mtime", "-mmin",
        "-atime", "-amin", "-ctime", "-cmin", "-newermt", "-newerat", "-newerct", "-perm", "-user", "-group",
        "-uid", "-gid", "-links", "-inum", "-used", "-fstype", "-printf",
    ];
    const FILE_VALUED: &[&str] = &["-newer", "-anewer", "-cnewer", "-samefile"];
    let mut ledger = PathLedger::default();
    let mut index = 0;
    while words
        .get(index)
        .is_some_and(|word| matches!(word.text.as_str(), "-H" | "-P" | "-O0" | "-O1" | "-O2" | "-O3"))
    {
        index += 1;
    }
    let mut starts = 0;
    while let Some(word) = words.get(index) {
        if word.text.starts_with('-') || matches!(word.text.as_str(), "!" | "(" | ")" | ",") {
            break;
        }
        ledger.read(scene, word);
        starts += 1;
        index += 1;
    }
    if starts == 0 {
        ledger.read_working_directory(scene);
    }
    while let Some(word) = words.get(index) {
        let primary = word.text.as_str();
        index += 1;
        if PLAIN.contains(&primary) {
            continue;
        }
        if !VALUED.contains(&primary) && !FILE_VALUED.contains(&primary) {
            return ShellAssessment::unbounded(
                "shell.find_side_effect",
                Reason::new(
                    format!("find 的 {primary} 可能执行、删除、写文件或跟随符号链接"),
                    format!("find's {primary} may execute, delete, write a file, or follow symlinks"),
                ),
            )
            .with_facts(ledger.facts);
        }
        let Some(value) = words.get(index) else {
            return refuse_option("find", primary);
        };
        index += 1;
        if FILE_VALUED.contains(&primary) {
            ledger.read(scene, value);
        }
    }
    read_verdict("find", ledger)
}

/// Whether `script` is a sed program made only of commands that print,
/// delete, or substitute in the pattern space.
///
/// sed can write a file (`w`, `s///w`), read one (`r`), and run a command
/// (`e`, `s///e`), all from inside the script, so the script is parsed and
/// anything outside this small grammar refuses: addresses, then one of the
/// commands below, separated by `;`.
fn sed_script_is_inert(script: &str) -> bool {
    let chars: Vec<char> = script.chars().collect();
    let mut at = 0;
    let blanks = |at: &mut usize| {
        while matches!(chars.get(*at), Some(' ' | '\t')) {
            *at += 1;
        }
    };
    // Reads up to the closing `delimiter`. A bracket expression that is still
    // open there means sed implementations disagree on where the text ends.
    let delimited = |at: &mut usize, delimiter: char, regex: bool| -> bool {
        let start = *at;
        loop {
            match chars.get(*at) {
                None | Some('\n') => return false,
                Some('\\') => {
                    if matches!(chars.get(*at + 1), None | Some('\n')) {
                        return false;
                    }
                    *at += 2;
                }
                Some(character) if *character == delimiter => break,
                Some(_) => *at += 1,
            }
        }
        let text = &chars[start..*at];
        *at += 1;
        !regex || brackets_close(text)
    };
    let address = |at: &mut usize, second: bool| -> Option<bool> {
        match chars.get(*at) {
            Some(character) if character.is_ascii_digit() => {
                while chars.get(*at).is_some_and(|next| next.is_ascii_digit() || *next == '~') {
                    *at += 1;
                }
                Some(true)
            }
            Some('$') => {
                *at += 1;
                Some(true)
            }
            Some('+' | '~') if second => {
                *at += 1;
                let start = *at;
                while chars.get(*at).is_some_and(char::is_ascii_digit) {
                    *at += 1;
                }
                (*at > start).then_some(true)
            }
            Some('/') => {
                *at += 1;
                if !delimited(at, '/', true) {
                    return None;
                }
                while matches!(chars.get(*at), Some('I' | 'M')) {
                    *at += 1;
                }
                Some(true)
            }
            _ => Some(false),
        }
    };
    loop {
        while matches!(chars.get(at), Some(' ' | '\t' | ';')) {
            at += 1;
        }
        if at >= chars.len() {
            return true;
        }
        match address(&mut at, false) {
            None => return false,
            Some(true) => {
                blanks(&mut at);
                if chars.get(at) == Some(&',') {
                    at += 1;
                    blanks(&mut at);
                    if address(&mut at, true) != Some(true) {
                        return false;
                    }
                }
            }
            Some(false) => {}
        }
        blanks(&mut at);
        while chars.get(at) == Some(&'!') {
            at += 1;
            blanks(&mut at);
        }
        match chars.get(at).copied() {
            Some('p' | 'd' | 'D' | 'P' | '=' | 'g' | 'G' | 'h' | 'H' | 'x' | 'n' | 'N') => at += 1,
            Some('q' | 'Q') => {
                at += 1;
                while chars.get(at).is_some_and(char::is_ascii_digit) {
                    at += 1;
                }
            }
            Some(command @ ('s' | 'y')) => {
                at += 1;
                let Some(delimiter) = chars.get(at).copied() else {
                    return false;
                };
                if delimiter.is_alphanumeric() || delimiter.is_whitespace() || delimiter == '\\' {
                    return false;
                }
                at += 1;
                // BSD sed reads a bracket expression in both halves of a
                // `y` and in the pattern of an `s`, never in its replacement.
                if !delimited(&mut at, delimiter, true) || !delimited(&mut at, delimiter, command == 'y') {
                    return false;
                }
                // The flags that neither write (`w`) nor execute (`e`).
                while command == 's'
                    && chars
                        .get(at)
                        .is_some_and(|flag| flag.is_ascii_digit() || matches!(flag, 'g' | 'p' | 'i' | 'I' | 'm' | 'M'))
                {
                    at += 1;
                }
            }
            _ => return false,
        }
        blanks(&mut at);
        if !matches!(chars.get(at), None | Some(';')) {
            return false;
        }
    }
}

/// Whether every bracket expression in a regular expression is closed.
fn brackets_close(regex: &[char]) -> bool {
    let mut at = 0;
    while at < regex.len() {
        match regex[at] {
            '\\' => at += 2,
            '[' => {
                at += 1;
                if regex.get(at) == Some(&'^') {
                    at += 1;
                }
                if regex.get(at) == Some(&']') {
                    at += 1;
                }
                loop {
                    match regex.get(at) {
                        None => return false,
                        Some(']') => break,
                        // `[:alpha:]`, `[=a=]`, `[.x.]` inside the brackets.
                        Some('[') if matches!(regex.get(at + 1), Some(':' | '=' | '.')) => {
                            let class = regex[at + 1];
                            at += 2;
                            loop {
                                match regex.get(at) {
                                    None => return false,
                                    Some(end) if *end == class && regex.get(at + 1) == Some(&']') => {
                                        at += 2;
                                        break;
                                    }
                                    Some(_) => at += 1,
                                }
                            }
                        }
                        Some(_) => at += 1,
                    }
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    true
}

fn analyze_sed(scene: &ShellScene<'_>, words: &[Lexeme]) -> ShellAssessment {
    use OptionArg::Text;
    // No `-f`: the script would be a file nobody read. No `-l`: GNU sed
    // takes a count after it and BSD sed does not, so the word after it is
    // the script to one and not the other. `-i` is taken off first because it
    // changes what the command is.
    static TABLE: OptionTable = OptionTable {
        flags: "nErsuz",
        valued: &[('e', Text)],
        long: &["quiet", "silent", "regexp-extended", "separate", "unbuffered", "null-data", "posix", "debug", "sandbox"],
        ..NO_OPTIONS
    };
    let mut in_place = false;
    let mut rest = Vec::with_capacity(words.len());
    for word in words {
        let suffix = word
            .text
            .strip_prefix("--in-place")
            .map(|suffix| suffix.strip_prefix('=').unwrap_or(suffix))
            .or_else(|| word.text.strip_prefix("-i"));
        match suffix {
            // A backup suffix with a `/` or `*` in it names another directory.
            Some(suffix)
                if suffix
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')) =>
            {
                in_place = true;
            }
            Some(_) => return refuse_option("sed", "-i"),
            None => rest.push(word.clone()),
        }
    }
    let arguments = match parse_arguments(&TABLE, &rest) {
        Ok(arguments) => arguments,
        Err(option) => return refuse_option("sed", &option),
    };
    // Which word is the script depends on where the options end.
    if !arguments.late.is_empty() {
        return refuse_arguments("sed");
    }
    let mut scripts: Vec<&Lexeme> = arguments.values_of(&["-e"]).collect();
    let files = if scripts.is_empty() {
        let Some((script, files)) = arguments.operands.split_first() else {
            return refuse_arguments("sed");
        };
        scripts.push(script);
        files
    } else {
        &arguments.operands[..]
    };
    if !scripts.iter().all(|script| sed_script_is_inert(&script.text)) {
        return ShellAssessment::unbounded(
            "shell.sed_script_unverified",
            Reason::new("sed 脚本包含未核对的命令；w、r、e 等命令可以写文件、读文件或执行程序", "The sed script contains commands that were not checked; w, r, e and the like can write files, read files, or run programs"),
        );
    }
    let mut ledger = PathLedger::default();
    if !in_place {
        for file in files {
            if file.text != "-" {
                ledger.read(scene, file);
            }
        }
        return read_verdict("sed", ledger);
    }
    // BSD sed takes the word after `-i` as the backup suffix, so the same
    // line means two different programs there; it is not proven on macOS.
    if cfg!(target_os = "macos") || files.is_empty() {
        return refuse_arguments("sed");
    }
    for file in files {
        ledger.write(scene, file);
    }
    write_verdict("sed", ledger)
}

fn analyze_file_change(scene: &ShellScene<'_>, name: &str, words: &[Lexeme]) -> ShellAssessment {
    use OptionArg::{Read, Text, Write};
    static MKDIR: OptionTable = OptionTable {
        flags: "pv",
        valued: &[('m', Text)],
        long: &["parents", "verbose"],
        long_valued: &[("mode", Text)],
        ..NO_OPTIONS
    };
    static TOUCH: OptionTable = OptionTable {
        flags: "acmh",
        valued: &[('d', Text), ('t', Text), ('r', Read)],
        long: &["no-create", "no-dereference"],
        long_valued: &[("date", Text), ("time", Text), ("reference", Read)],
        ..NO_OPTIONS
    };
    // No `--no-preserve-root`.
    static RM: OptionTable = OptionTable {
        flags: "fiIrRdv",
        long: &["force", "recursive", "dir", "verbose", "one-file-system", "preserve-root"],
        long_optional: &["interactive"],
        ..NO_OPTIONS
    };
    // No `-p`: it removes each parent of the operand in turn.
    static RMDIR: OptionTable = OptionTable {
        flags: "v",
        long: &["verbose", "ignore-fail-on-non-empty"],
        ..NO_OPTIONS
    };
    static MV: OptionTable = OptionTable {
        flags: "fnviuT",
        valued: &[('t', Write)],
        long: &["force", "no-clobber", "verbose", "interactive", "no-target-directory"],
        long_valued: &[("target-directory", Write)],
        long_optional: &["update"],
        ..NO_OPTIONS
    };
    // No `-L`, `-H`, `-l`, `-s`: they follow, or make, links.
    static CP: OptionTable = OptionTable {
        flags: "rRapfnviuxTPd",
        valued: &[('t', Write)],
        long: &[
            "recursive", "archive", "force", "no-clobber", "verbose", "interactive", "one-file-system",
            "no-target-directory", "no-dereference",
        ],
        long_valued: &[("target-directory", Write), ("no-preserve", Text)],
        long_optional: &["update", "preserve"],
        ..NO_OPTIONS
    };
    let table = match name {
        "mkdir" => &MKDIR,
        "touch" => &TOUCH,
        "rm" => &RM,
        "rmdir" => &RMDIR,
        "mv" => &MV,
        _ => &CP,
    };
    let arguments = match parse_arguments(table, words) {
        Ok(arguments) => arguments,
        Err(option) => return refuse_option(name, &option).with_facts(CommandFacts {
            role: CommandRole::FileChange,
            ..CommandFacts::default()
        }),
    };
    let mut ledger = PathLedger::default();
    ledger.facts.role = CommandRole::FileChange;
    // `-` and `@file` are how some of these take their operands from input.
    // An option after an operand is itself a file to create or remove to a
    // parser that stops at the first operand (`touch f -d /abs/new`).
    if !arguments.late.is_empty()
        || arguments
            .operands
            .iter()
            .any(|operand| operand.text == "-" || operand.text.starts_with('@'))
    {
        return refuse_arguments(name).with_facts(ledger.facts);
    }
    let mut directory = None;
    for (_, role, value) in &arguments.values {
        match role {
            Text => {}
            Read => {
                ledger.read(scene, value);
            }
            Write => directory = ledger.write(scene, value).or(directory),
        }
    }
    let targeted = arguments.has(&["-t", "--target-directory"]);
    let (sources, destination) = match name {
        "mv" | "cp" if targeted => (&arguments.operands[..], None),
        "mv" | "cp" => match arguments.operands.split_last() {
            Some((destination, sources)) if !sources.is_empty() => (sources, Some(destination)),
            _ => return refuse_arguments(name).with_facts(ledger.facts),
        },
        _ => (&arguments.operands[..], None),
    };
    if sources.is_empty() && !(name == "rm" && arguments.has(&["-f", "--force"])) {
        return refuse_arguments(name).with_facts(ledger.facts);
    }
    if let Some(destination) = destination {
        directory = ledger
            .write(scene, destination)
            .filter(|destination| destination.is_dir());
    }
    for source in sources {
        // `cp -r src/. dest` pours one directory into another, file by file.
        let last = source
            .text
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next();
        if matches!(name, "cp" | "mv") && matches!(last, Some("." | "..")) {
            ledger.fail(unprovable_path(&source.text, "以 . 或 .. 结尾，会把目录内容并入目标", "ends in . or .., which merges a directory's contents into the destination"));
        }
        // `cp` only reads what it copies; every other command changes or
        // removes its operand.
        let resolved = if name == "cp" {
            ledger.read(scene, source)
        } else {
            ledger.write(scene, source)
        };
        // Copying into a directory writes `directory/name`, and `cp` writes
        // through whatever is already there: a symlink out of the workspace,
        // or a tree whose files might be.
        if name == "cp" {
            if let (Some(directory), Some(resolved)) = (&directory, resolved) {
                let landing = resolved.file_name().map(|name| directory.join(name));
                let clear = landing
                    .as_deref()
                    .is_some_and(|landing| match fs::symlink_metadata(landing) {
                        Ok(metadata) => metadata.is_file(),
                        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                    });
                if !clear {
                    ledger.fail(unprovable_path(&source.text, "在目标目录中已有同名的目录或链接", "already has a directory or a link of its name in the destination"));
                }
            }
        }
    }
    write_verdict(name, ledger)
}

// --- Git --------------------------------------------------------------------

/// The options of one read-only Git subcommand, and what its plain arguments
/// may be.
struct GitRule {
    table: &'static OptionTable,
    /// Whether it takes plain arguments (revisions, pathspecs) at all.
    operands: bool,
}

fn git_rule(subcommand: &str, arguments: &[Lexeme]) -> Option<GitRule> {
    use OptionArg::Text;
    // Formatting and comparison options shared by `diff`, `log` and `show`.
    // Not here, and so asked about: `--output` (writes a file), `--ext-diff`
    // and `--textconv` (run a configured program), `--no-index` (compares any
    // two files), `-O` (reads an order file), `--submodule`.
    macro_rules! diff_flags { () => { "pusRabwWzDq" } }
    macro_rules! diff_attached { () => { "MCBUlX" } }
    const DIFF_LONG: &[&str] = &[
        "patch", "no-patch", "numstat", "shortstat", "summary", "name-only", "name-status", "raw",
        "patch-with-stat", "patch-with-raw", "compact-summary", "cumulative", "full-index", "binary",
        "find-copies-harder", "irreversible-delete", "no-renames", "rename-empty", "no-rename-empty", "check",
        "no-color", "minimal", "patience", "histogram", "text", "ignore-space-at-eol", "ignore-cr-at-eol",
        "ignore-space-change", "ignore-all-space", "ignore-blank-lines", "function-context", "exit-code", "quiet",
        "no-ext-diff", "no-textconv", "no-prefix", "default-prefix", "indent-heuristic", "no-indent-heuristic",
        "pickaxe-all", "pickaxe-regex", "cached", "staged", "merge-base", "no-color-moved", "no-color-moved-ws",
        "no-relative", "ours", "theirs", "base", "cc", "combined-all-paths",
        // Revision walking, for `log`, `show` and `rev-list`.
        "oneline", "graph", "no-decorate", "all", "merges", "no-merges", "first-parent", "reverse", "follow",
        "abbrev-commit", "no-abbrev-commit", "date-order", "topo-order", "author-date-order", "source",
        "left-right", "cherry-pick", "cherry-mark", "cherry", "boundary", "simplify-by-decoration", "full-history",
        "do-walk", "parents", "children", "show-pulls", "relative-date", "walk-reflogs", "no-notes", "no-mailmap",
        "use-mailmap", "mailmap", "log-size", "left-only", "right-only", "ancestry-path", "dense", "sparse",
        "simplify-merges", "reflog", "single-worktree", "ignore-missing", "invert-grep", "regexp-ignore-case",
        "extended-regexp", "fixed-strings", "perl-regexp", "basic-regexp", "all-match", "full-diff", "remerge-diff",
        "no-diff-merges", "not", "count", "objects", "timestamp", "header", "no-max-parents", "no-min-parents",
        "clear-decorations", "no-expand-tabs", "include-untracked", "only-untracked",
    ];
    const DIFF_LONG_VALUED: &[(&str, OptionArg)] = &[
        ("diff-filter", Text), ("find-object", Text), ("src-prefix", Text), ("dst-prefix", Text),
        ("line-prefix", Text), ("inter-hunk-context", Text), ("diff-algorithm", Text), ("anchored", Text),
        ("ws-error-highlight", Text), ("word-diff-regex", Text), ("output-indicator-new", Text),
        ("output-indicator-old", Text), ("output-indicator-context", Text), ("ignore-matching-lines", Text),
        ("rotate-to", Text), ("skip-to", Text), ("max-count", Text), ("skip", Text), ("since", Text),
        ("after", Text), ("until", Text), ("before", Text), ("author", Text), ("committer", Text), ("grep", Text),
        ("grep-reflog", Text), ("format", Text), ("date", Text), ("decorate-refs", Text),
        ("decorate-refs-exclude", Text), ("min-parents", Text), ("max-parents", Text), ("glob", Text),
        ("exclude", Text), ("diff-merges", Text), ("encoding", Text),
    ];
    const DIFF_LONG_OPTIONAL: &[&str] = &[
        "unified", "stat", "dirstat", "abbrev", "color", "word-diff", "color-words", "relative",
        "ignore-submodules", "find-renames", "find-copies", "break-rewrites", "color-moved", "color-moved-ws",
        "decorate", "branches", "tags", "remotes", "pretty", "expand-tabs", "no-walk", "notes", "show-notes",
    ];
    static DIFF: OptionTable = OptionTable {
        flags: diff_flags!(),
        attached: diff_attached!(),
        valued: &[('S', Text), ('G', Text), ('I', Text)],
        long: DIFF_LONG,
        long_valued: DIFF_LONG_VALUED,
        long_optional: DIFF_LONG_OPTIONAL,
        counts: false,
        long_text_takes_next: true,
    };
    static LOG: OptionTable = OptionTable {
        flags: concat!(diff_flags!(), "giEFPcmtr"),
        attached: diff_attached!(),
        valued: &[('S', Text), ('G', Text), ('I', Text), ('n', Text), ('L', Text)],
        long: DIFF_LONG,
        long_valued: DIFF_LONG_VALUED,
        long_optional: DIFF_LONG_OPTIONAL,
        counts: true,
        long_text_takes_next: true,
    };
    let rule = |table: &'static OptionTable, operands: bool| Some(GitRule { table, operands });
    let first = arguments.first().map(|word| word.text.as_str());
    match subcommand {
        "diff" => rule(&DIFF, true),
        "log" | "show" | "rev-list" | "whatchanged" => rule(&LOG, true),
        "status" => {
            static TABLE: OptionTable = OptionTable {
                flags: "sbvz",
                attached: "u",
                long: &["short", "branch", "long", "verbose", "ahead-behind", "no-ahead-behind", "renames", "no-renames", "show-stash", "no-column", "null"],
                long_optional: &["porcelain", "untracked-files", "ignored", "column", "ignore-submodules", "find-renames"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "shortlog" => {
            static TABLE: OptionTable = OptionTable {
                flags: "nsec",
                attached: "w",
                long: &["numbered", "summary", "email", "committer", "all", "branches", "tags", "remotes", "no-merges", "merges", "first-parent"],
                long_valued: &[
                    ("group", Text), ("format", Text), ("since", Text), ("after", Text), ("until", Text), ("before", Text),
                    ("author", Text), ("grep", Text), ("max-count", Text),
                ],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        // No `--contents`, `-S` or `--ignore-revs-file`: each reads a file.
        "blame" => {
            static TABLE: OptionTable = OptionTable {
                flags: "bltsewfnpck",
                attached: "MC",
                valued: &[('L', Text)],
                long: &[
                    "root", "show-stats", "show-email", "show-name", "show-number", "porcelain", "line-porcelain",
                    "incremental", "minimal", "color-lines", "color-by-age", "progress", "no-progress", "first-parent",
                ],
                long_valued: &[("since", Text), ("date", Text)],
                long_optional: &["abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "ls-files" => {
            static TABLE: OptionTable = OptionTable {
                flags: "cdmoiskutvfz",
                valued: &[('x', Text)],
                long: &[
                    "cached", "deleted", "modified", "others", "ignored", "stage", "unmerged", "killed", "directory",
                    "no-empty-directory", "eol", "full-name", "error-unmatch", "debug", "deduplicate",
                    "exclude-standard", "sparse",
                ],
                long_valued: &[("exclude", Text), ("with-tree", Text), ("format", Text)],
                long_optional: &["abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "ls-tree" => {
            static TABLE: OptionTable = OptionTable {
                flags: "drtlz",
                long: &["long", "name-only", "name-status", "object-only", "full-name", "full-tree"],
                long_valued: &[("format", Text)],
                long_optional: &["abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "rev-parse" => {
            static TABLE: OptionTable = OptionTable {
                flags: "q",
                long: &[
                    "verify", "quiet", "symbolic", "symbolic-full-name", "show-toplevel", "show-cdup", "show-prefix",
                    "git-dir", "absolute-git-dir", "git-common-dir", "is-inside-work-tree", "is-inside-git-dir",
                    "is-bare-repository", "is-shallow-repository", "show-superproject-working-tree", "all", "sq",
                    "not", "local-env-vars", "revs-only", "no-revs", "flags", "no-flags", "end-of-options",
                ],
                long_valued: &[
                    ("default", Text), ("prefix", Text), ("glob", Text), ("exclude", Text), ("path-format", Text),
                    ("git-path", Text), ("since", Text), ("after", Text), ("until", Text), ("before", Text),
                ],
                long_optional: &["abbrev-ref", "short", "branches", "tags", "remotes", "show-object-format", "disambiguate"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "describe" => {
            static TABLE: OptionTable = OptionTable {
                long: &["all", "tags", "contains", "long", "always", "exact-match", "first-parent", "debug"],
                long_valued: &[("candidates", Text), ("match", Text), ("exclude", Text)],
                long_optional: &["dirty", "broken", "abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "merge-base" => {
            static TABLE: OptionTable = OptionTable {
                flags: "a",
                long: &["all", "octopus", "independent", "is-ancestor", "fork-point"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        // No `--textconv` or `--filters`: both run configured programs.
        "cat-file" => {
            static TABLE: OptionTable = OptionTable {
                flags: "tsep",
                long: &["batch-all-objects", "buffer", "unordered"],
                long_optional: &["batch", "batch-check"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "for-each-ref" => {
            static TABLE: OptionTable = OptionTable {
                long: &["shell", "perl", "python", "tcl", "ignore-case", "omit-empty"],
                long_valued: &[("count", Text), ("sort", Text), ("format", Text), ("points-at", Text), ("exclude", Text)],
                long_optional: &["merged", "no-merged", "contains", "no-contains", "color"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "show-ref" => {
            static TABLE: OptionTable = OptionTable {
                flags: "dqs",
                long: &["head", "dereference", "verify", "quiet", "tags", "heads", "branches", "exists"],
                long_optional: &["hash", "abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "count-objects" => {
            static TABLE: OptionTable = OptionTable {
                flags: "vH",
                long: &["verbose", "human-readable"],
                ..NO_OPTIONS
            };
            rule(&TABLE, false)
        }
        "check-ignore" => {
            static TABLE: OptionTable = OptionTable {
                flags: "vnqz",
                long: &["verbose", "non-matching", "quiet"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "version" => rule(&NO_OPTIONS, false),
        // No `-O`/`--open-files-in-pager` (runs a program on every matching
        // file), `--no-index` (searches any directory), `-f` (reads a file).
        "grep" => {
            static TABLE: OptionTable = OptionTable {
                flags: "iIawvhHEGPFnlLzocpWq",
                valued: &[('e', Text), ('A', Text), ('B', Text), ('C', Text), ('m', Text)],
                long: &[
                    "ignore-case", "text", "word-regexp", "invert-match", "full-name", "extended-regexp",
                    "basic-regexp", "perl-regexp", "fixed-strings", "line-number", "column", "files-with-matches",
                    "name-only", "files-without-match", "null", "only-matching", "count", "no-color", "break",
                    "heading", "show-function", "function-context", "quiet", "all-match", "and", "or", "not",
                    "cached", "untracked", "exclude-standard", "no-exclude-standard", "recursive", "no-recursive",
                    "no-textconv",
                ],
                long_valued: &[
                    ("max-depth", Text), ("max-count", Text), ("threads", Text), ("after-context", Text),
                    ("before-context", Text), ("context", Text),
                ],
                long_optional: &["color"],
                counts: true,
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        // Listing forms only: a plain name given to either creates one.
        "branch" => {
            static TABLE: OptionTable = OptionTable {
                flags: "larvi",
                long: &["list", "all", "remotes", "verbose", "show-current", "no-color", "no-column", "ignore-case", "omit-empty", "no-abbrev"],
                long_valued: &[("sort", Text), ("format", Text), ("points-at", Text)],
                long_optional: &["contains", "no-contains", "merged", "no-merged", "color", "column", "abbrev"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "tag" => {
            static TABLE: OptionTable = OptionTable {
                flags: "li",
                attached: "n",
                long: &["list", "no-color", "no-column", "ignore-case", "omit-empty"],
                long_valued: &[("sort", Text), ("format", Text), ("points-at", Text)],
                long_optional: &["contains", "no-contains", "merged", "no-merged", "color", "column"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        "stash" if first == Some("list") => rule(&LOG, true),
        "stash" if first == Some("show") => rule(&DIFF, true),
        "reflog" if matches!(first, None | Some("show")) => rule(&LOG, true),
        "reflog" if matches!(first, Some("list" | "exists")) => rule(&NO_OPTIONS, true),
        "worktree" if first == Some("list") => {
            static TABLE: OptionTable = OptionTable {
                flags: "vz",
                long: &["porcelain", "verbose"],
                long_optional: &["expire"],
                ..NO_OPTIONS
            };
            rule(&TABLE, true)
        }
        _ => None,
    }
}

/// Whether Git, started in `scene.base`, would take a directory between there
/// and the workspace root for a repository of its own: one laid out as a bare
/// repository (`HEAD`, `objects`, `refs`), which the tree of a cloned project
/// can carry and whose `config` then names the programs `git status` runs.
fn working_directory_is_a_planted_repository(scene: &ShellScene<'_>) -> bool {
    let mut directory = Some(scene.base.as_path());
    while let Some(current) = directory {
        if current.join("HEAD").exists() && current.join("objects").exists() && current.join("refs").exists() {
            return true;
        }
        if same_path(current, scene.workspace) || !path_within(current, scene.workspace) {
            break;
        }
        directory = current.parent();
    }
    false
}

fn analyze_git(scene: &ShellScene<'_>, words: &[Lexeme]) -> ShellAssessment {
    let facts = CommandFacts {
        uses_cwd: true,
        names_path: false,
        role: CommandRole::Git,
    };
    let unverifiable_options = || {
        ShellAssessment::unbounded(
            "git.unverifiable_options",
            Reason::new("Git 命令包含可改换仓库、执行外部程序、写入输出或在展开后改变参数语义的选项", "The Git command has options that can switch repositories, run external programs, write output, or change meaning once expanded"),
        )
        .with_facts(facts)
    };
    // Before the subcommand only options that change nothing about which
    // repository is read or what runs: not `-C`, `-c`, `--git-dir`,
    // `--work-tree`, `--exec-path`, `--config-env`, `--namespace`, `-p`.
    let mut index = 0;
    while let Some(word) = words.get(index).filter(|word| word.text.starts_with('-')) {
        match word.text.as_str() {
            "--no-pager" | "--no-optional-locks" | "--literal-pathspecs" | "--no-replace-objects" => index += 1,
            "--version" if words.len() == 1 => {
                return ShellAssessment::read(
                    "git.read_only",
                    Reason::new("git --version 属于只读 Git 操作", "git --version is a read-only Git operation"),
                )
            }
            _ => return unverifiable_options(),
        }
    }
    let Some(subcommand) = words.get(index).map(|word| word.text.as_str()) else {
        return ShellAssessment::unbounded(
            "git.missing_subcommand",
            Reason::new("git 缺少可分析的子命令", "git has no subcommand to analyze"),
        )
        .with_facts(facts);
    };
    let arguments = &words[index + 1..];
    let state_change = || {
        ShellAssessment::unbounded(
            "git.state_change",
            Reason::new(
                format!("git {subcommand} 可能修改工作树、本地历史或远端状态"),
                format!("git {subcommand} may change the working tree, local history, or remote state"),
            ),
        )
        .with_facts(facts)
    };
    let mut beyond_by_design = false;
    let parsed = match subcommand {
        // `remote` lists; `remote get-url` prints one. `remote show` asks the
        // remote, and every other form edits the configuration.
        "remote" => {
            let texts: Vec<&str> = arguments.iter().map(|word| word.text.as_str()).collect();
            let listing = matches!(texts.as_slice(), [] | ["-v"] | ["--verbose"])
                || matches!(texts.as_slice(), ["get-url", rest @ ..]
                    if rest.iter().filter(|word| !matches!(**word, "--push" | "--all")).count() == 1);
            if !listing {
                return state_change();
            }
            Arguments::default()
        }
        // Reading the configuration. Without `--local` the answer includes
        // the user's own files outside the workspace, which is a read the
        // level decides on.
        "config" => {
            let mut reads = false;
            let mut local = false;
            let mut plain = 0;
            let mut index = 0;
            while let Some(word) = arguments.get(index) {
                index += 1;
                match word.text.as_str() {
                    "--get" | "--get-all" | "--get-regexp" | "--list" | "-l" => reads = true,
                    "get" | "list" if index == 1 => reads = true,
                    "--local" => local = true,
                    "--global" | "--system" | "--worktree" | "--show-origin" | "--show-scope" | "--name-only"
                    | "-z" | "--null" | "--bool" | "--int" | "--includes" | "--no-includes" => {}
                    "--default" => index += 1,
                    text if text.starts_with("--type=") => {}
                    text if text.starts_with('-') => return unverifiable_options(),
                    _ => plain += 1,
                }
            }
            // `git config name` reads; `git config name value` writes.
            if !((reads && plain <= 2) || (!reads && plain == 1)) {
                return state_change();
            }
            beyond_by_design = !local;
            Arguments::default()
        }
        _ => {
            let Some(rule) = git_rule(subcommand, arguments) else {
                return state_change();
            };
            // The words that pick `stash list`, `reflog show`, `worktree list`.
            let skip = usize::from(matches!(subcommand, "stash" | "reflog" | "worktree") && !arguments.is_empty());
            let parsed = match parse_arguments(rule.table, &arguments[skip..]) {
                Ok(parsed) => parsed,
                Err(_) => return unverifiable_options(),
            };
            if !rule.operands && !parsed.operands.is_empty() {
                return state_change();
            }
            // A plain name creates a branch or a tag unless the command is
            // listing, which these options say it is.
            let lists = parsed.has(&["-l", "--list", "--contains", "--no-contains", "--merged", "--no-merged", "--points-at"]);
            if matches!(subcommand, "branch" | "tag") && !parsed.operands.is_empty() && !lists {
                return state_change();
            }
            // `%G?` and `%(signature)` verify a signature by running the
            // program `gpg.program` names.
            if parsed
                .values_of(&["--format", "--pretty"])
                .any(|format| format.text.contains("%G") || format.text.contains("signature"))
            {
                return unverifiable_options();
            }
            parsed
        }
    };

    let mut ledger = PathLedger {
        facts,
        ..PathLedger::default()
    };
    ledger.read_working_directory(scene);
    if scene.anchor == ShellAnchor::Known && working_directory_is_a_planted_repository(scene) {
        return ShellAssessment::unbounded(
            "git.unverified_repository",
            Reason::new("当前目录或其上级带有裸仓库的内部文件（HEAD、objects、refs），其中的配置可以指定 Git 运行的程序", "The working directory or one above it holds a bare repository's internals (HEAD, objects, refs), whose configuration can name programs Git runs"),
        )
        .with_facts(ledger.facts);
    }
    // Git reads through its object store and its own work tree, so a
    // revision or a pathspec is not a file to check. A word that spells a
    // path outright, or names something that exists here, is: two paths
    // outside the work tree turn `git diff` into a comparison of any two
    // files. Option values are looked at too, in case an option took a word
    // this table thought was its own.
    for word in parsed.operands.iter().chain(parsed.values.iter().map(|(_, _, value)| value)) {
        let text = word.text.as_str();
        let spelled_as_path = text.starts_with(['/', '\\', '.', '~'])
            || text.as_bytes().get(1) == Some(&b':')
            || names_network_path(text);
        let exists_here = !text.is_empty()
            && !text.contains(['\0', ':'])
            && fs::symlink_metadata(scene.base.join(text)).is_ok();
        if spelled_as_path || exists_here {
            ledger.read(scene, word);
        }
    }
    ledger.beyond |= beyond_by_design;
    let facts = ledger.facts;
    if let Some(reason) = ledger.failure {
        return ShellAssessment::unbounded("git.unverifiable_path", reason).with_facts(facts);
    }
    if ledger.beyond {
        return ShellAssessment::read_beyond_trusted_roots(
            "git.outside_trusted_roots",
            Reason::new("Git 只读命令显式引用了可信目录之外的路径或仓库", "The read-only Git command names a path or repository outside the trusted directories"),
        )
        .with_facts(facts);
    }
    ShellAssessment::read(
        "git.read_only",
        Reason::new(
            format!("git {subcommand} 属于只读 Git 操作"),
            format!("git {subcommand} is a read-only Git operation"),
        ),
    )
    .with_facts(facts)
}

// --- PowerShell commands ----------------------------------------------------

/// What a cmdlet's positional arguments are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Positional {
    /// Text, or objects it only formats.
    Text,
    /// One path.
    Path,
    /// A path, then text (`Get-ChildItem path filter`, `Set-Content path value`).
    PathThenText,
    /// A pattern, then one path (`Select-String pattern path`).
    PatternThenPath,
}

/// Where in a pipeline a cmdlet may stand.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// It reads what its arguments name. Fed by a pipeline it would read
    /// whatever paths came down it, which nothing here can see.
    Source,
    /// It only reshapes the objects it is handed.
    Filter,
}

/// What a cmdlet does to the paths it names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CmdletEffect {
    Read,
    /// Reads the location it runs in when it names no path.
    ReadHere,
    Write,
    ChangeDirectory,
}

/// One allowlisted cmdlet: its exact parameter names — an abbreviation or an
/// alias of a parameter is not accepted, since PowerShell resolves those
/// against the full set and this table holds only the checked part of it.
struct CmdletRule {
    name: &'static str,
    switches: &'static [&'static str],
    valued: &'static [(&'static str, OptionArg)],
    positional: Positional,
    stage: Stage,
    effect: CmdletEffect,
    /// Whether it does anything without a path (`Get-Content` would prompt).
    needs_path: bool,
}

/// The cmdlet a command name runs. Aliases are the built-in ones both
/// editions have; `sc` is not here because PowerShell 7 dropped it and the
/// name now runs `sc.exe`, the service controller.
fn canonical_powershell_command(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "get-childitem" | "gci" | "ls" | "dir" => "Get-ChildItem",
        "get-content" | "gc" | "cat" | "type" => "Get-Content",
        "get-location" | "gl" | "pwd" => "Get-Location",
        "set-location" | "sl" | "cd" | "chdir" => "Set-Location",
        "get-item" | "gi" => "Get-Item",
        "get-itemproperty" | "gp" => "Get-ItemProperty",
        "get-date" => "Get-Date",
        "get-filehash" => "Get-FileHash",
        "get-process" | "gps" | "ps" => "Get-Process",
        "test-path" => "Test-Path",
        "resolve-path" | "rvpa" => "Resolve-Path",
        "split-path" => "Split-Path",
        "join-path" => "Join-Path",
        "select-string" | "sls" => "Select-String",
        "select-object" | "select" => "Select-Object",
        "sort-object" | "sort" => "Sort-Object",
        "measure-object" | "measure" => "Measure-Object",
        "group-object" | "group" => "Group-Object",
        "format-list" | "fl" => "Format-List",
        "format-table" | "ft" => "Format-Table",
        "format-wide" | "fw" => "Format-Wide",
        "out-string" => "Out-String",
        "write-output" | "echo" | "write" => "Write-Output",
        "convertto-json" => "ConvertTo-Json",
        "convertfrom-json" => "ConvertFrom-Json",
        "set-content" => "Set-Content",
        "add-content" | "ac" => "Add-Content",
        "clear-content" | "clc" => "Clear-Content",
        "remove-item" | "ri" | "rm" | "del" | "erase" | "rd" | "rmdir" => "Remove-Item",
        _ => "",
    }
}

fn cmdlet_rule(name: &str) -> Option<CmdletRule> {
    use CmdletEffect as Effect;
    use OptionArg::{Read, Text, Write};
    let rule = |switches, valued, positional, stage, effect, needs_path| {
        Some(CmdletRule {
            name: "",
            switches,
            valued,
            positional,
            stage,
            effect,
            needs_path,
        })
    };
    const PATHS: &[(&str, OptionArg)] = &[("Path", Read), ("LiteralPath", Read)];
    let found = match name {
        // No `-FollowSymlink`.
        "Get-ChildItem" => rule(
            &["Recurse", "Force", "Name", "File", "Directory", "Hidden", "ReadOnly", "System"],
            &[("Path", Read), ("LiteralPath", Read), ("Filter", Text), ("Include", Text), ("Exclude", Text), ("Depth", Text), ("Attributes", Text)],
            Positional::PathThenText,
            Stage::Source,
            Effect::ReadHere,
            false,
        ),
        // No `-Wait` (never returns) or `-Stream`.
        "Get-Content" => rule(
            &["Raw", "Force"],
            &[
                ("Path", Read), ("LiteralPath", Read), ("TotalCount", Text), ("First", Text), ("Head", Text),
                ("Tail", Text), ("Last", Text), ("Encoding", Text), ("ReadCount", Text), ("Delimiter", Text),
            ],
            Positional::Path,
            Stage::Source,
            Effect::Read,
            true,
        ),
        "Get-Item" => rule(&["Force"], PATHS, Positional::Path, Stage::Source, Effect::Read, true),
        "Get-ItemProperty" => rule(
            &[],
            &[("Path", Read), ("LiteralPath", Read), ("Name", Text)],
            Positional::PathThenText,
            Stage::Source,
            Effect::Read,
            true,
        ),
        "Get-FileHash" => rule(
            &[],
            &[("Path", Read), ("LiteralPath", Read), ("Algorithm", Text)],
            Positional::Path,
            Stage::Source,
            Effect::Read,
            true,
        ),
        "Test-Path" => rule(
            &["IsValid"],
            &[("Path", Read), ("LiteralPath", Read), ("PathType", Text)],
            Positional::Path,
            Stage::Source,
            Effect::Read,
            true,
        ),
        "Resolve-Path" => rule(&["Relative"], PATHS, Positional::Path, Stage::Source, Effect::Read, true),
        "Set-Location" => rule(&[], PATHS, Positional::Path, Stage::Source, Effect::ChangeDirectory, true),
        "Get-Location" => rule(&[], &[], Positional::Text, Stage::Source, Effect::Read, false),
        "Get-Date" => rule(
            &[],
            &[("Format", Text), ("UFormat", Text), ("Date", Text)],
            Positional::Text,
            Stage::Source,
            Effect::Read,
            false,
        ),
        // No `-Resolve`: without it both only rearrange strings.
        "Split-Path" => rule(
            &["Parent", "Leaf", "Qualifier", "NoQualifier", "IsAbsolute", "Extension", "LeafBase"],
            &[("Path", Text), ("LiteralPath", Text)],
            Positional::Text,
            Stage::Source,
            Effect::Read,
            false,
        ),
        "Join-Path" => rule(
            &[],
            &[("Path", Text), ("ChildPath", Text), ("AdditionalChildPath", Text)],
            Positional::Text,
            Stage::Source,
            Effect::Read,
            false,
        ),
        // No `-ComputerName`: it asks another machine, with this account.
        "Get-Process" => rule(&[], &[("Name", Text), ("Id", Text)], Positional::Text, Stage::Source, Effect::Read, false),
        "Select-String" => rule(
            &["SimpleMatch", "CaseSensitive", "Quiet", "List", "NotMatch", "AllMatches", "Raw", "NoEmphasis"],
            &[
                ("Pattern", Text), ("Path", Read), ("LiteralPath", Read), ("Context", Text), ("Encoding", Text),
                ("Include", Text), ("Exclude", Text),
            ],
            Positional::PatternThenPath,
            Stage::Source,
            Effect::Read,
            false,
        ),
        "Select-Object" => rule(
            &["Unique"],
            &[
                ("First", Text), ("Last", Text), ("Skip", Text), ("SkipLast", Text), ("Index", Text),
                ("Property", Text), ("ExpandProperty", Text), ("ExcludeProperty", Text),
            ],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Sort-Object" => rule(
            &["Descending", "Unique", "CaseSensitive"],
            &[("Property", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Measure-Object" => rule(
            &["Line", "Word", "Character", "Sum", "Average", "Maximum", "Minimum"],
            &[("Property", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Group-Object" => rule(
            &["NoElement", "CaseSensitive"],
            &[("Property", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Format-List" => rule(
            &["Force"],
            &[("Property", Text), ("GroupBy", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Format-Table" => rule(
            &["AutoSize", "Wrap", "HideTableHeaders", "Force"],
            &[("Property", Text), ("GroupBy", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Format-Wide" => rule(
            &["AutoSize", "Force"],
            &[("Property", Text), ("GroupBy", Text), ("Column", Text)],
            Positional::Text,
            Stage::Filter,
            Effect::Read,
            false,
        ),
        "Out-String" => rule(&["Stream"], &[("Width", Text)], Positional::Text, Stage::Filter, Effect::Read, false),
        "Write-Output" => rule(&["NoEnumerate"], &[("InputObject", Text)], Positional::Text, Stage::Filter, Effect::Read, false),
        "ConvertTo-Json" => rule(&["Compress"], &[("Depth", Text)], Positional::Text, Stage::Filter, Effect::Read, false),
        "ConvertFrom-Json" => rule(&["AsHashtable"], &[("Depth", Text)], Positional::Text, Stage::Filter, Effect::Read, false),
        "Set-Content" | "Add-Content" => rule(
            &["NoNewline", "Force"],
            &[("Path", Write), ("LiteralPath", Write), ("Value", Text), ("Encoding", Text)],
            Positional::PathThenText,
            Stage::Source,
            Effect::Write,
            true,
        ),
        // No `-Recurse` on `Remove-Item`: a tree is not deleted unasked.
        "Clear-Content" | "Remove-Item" => rule(
            &["Force"],
            &[("Path", Write), ("LiteralPath", Write)],
            Positional::Path,
            Stage::Source,
            Effect::Write,
            true,
        ),
        _ => None,
    };
    found.map(|rule| CmdletRule { name: canonical_powershell_command(name), ..rule })
}

fn analyze_powershell_command(
    scene: &ShellScene<'_>,
    command: &SimpleCommand,
    pipeline_emits_text: &mut bool,
) -> ShellAssessment {
    let words = command.words.as_slice();
    let raw_name = words[0].text.as_str();
    if raw_name.contains(['/', '\\']) {
        return ShellAssessment::unbounded(
            "powershell.explicit_executable_path",
            Reason::new("命令通过路径或模块限定名选择实现，不能仅凭 cmdlet 名称白名单证明其安全", "The command picks its implementation by path or module-qualified name, so a cmdlet-name allowlist cannot prove it safe"),
        );
    }
    let unclassified = |name: &str| {
        ShellAssessment::unbounded(
            "powershell.unclassified_command",
            Reason::new(
                format!("无法证明 {name} 只读或仅修改可信目录"),
                format!("{name} cannot be proven read-only or limited to trusted directories"),
            ),
        )
    };
    let Some(rule) = cmdlet_rule(canonical_powershell_command(raw_name)) else {
        return unclassified(raw_name);
    };
    let name = rule.name;
    if let Some(word) = words.iter().skip(1).find(|word| word.wild || word.home) {
        return ShellAssessment::unbounded(
            "powershell.dynamic_read_path",
            Reason::new(
                format!("{} 含有通配符或 home 展开，可能跟随链接或 Provider 越过可信目录", word.text),
                format!("{} contains a wildcard or a home expansion, which may follow links or providers beyond the trusted directories", word.text),
            ),
        );
    }
    let unverified = |parameter: &str| {
        ShellAssessment::unbounded(
            "powershell.unverified_parameter",
            Reason::new(
                format!("{name} 的参数 {parameter} 不在已核对的参数之内，或用法无法静态确定"),
                format!("{name}'s parameter {parameter} is not among the checked parameters, or its use cannot be determined statically"),
            ),
        )
    };

    // What the head of this pipeline hands down it decides what a
    // `Select-String` further along searches: lines of text, or the files
    // that file objects name.
    let fed = command.piped;
    if !fed {
        *pipeline_emits_text = matches!(name, "Get-Content" | "Out-String" | "Write-Output" | "ConvertTo-Json");
    }
    let search_of_text = name == "Select-String" && fed && *pipeline_emits_text;
    if fed && rule.stage == Stage::Source && !search_of_text {
        return ShellAssessment::unbounded(
            "powershell.pipeline_input_unverifiable",
            Reason::new(
                format!("{name} 从管道接收输入，它会读取哪些路径取决于上游在运行时产生的对象"),
                format!("{name} takes its input from the pipeline, so which paths it reads depends on the objects produced upstream at run time"),
            ),
        );
    }

    let mut ledger = PathLedger::default();
    let mut paths = 0usize;
    let mut positionals = 0usize;
    let writes = rule.effect == CmdletEffect::Write;
    let mut path = |ledger: &mut PathLedger, word: &Lexeme| -> Option<PathBuf> {
        paths += 1;
        // `-Path` expands wildcards itself, quoted or not: `'a[bc].txt'`
        // opens whichever file matches.
        if word.text.contains(['*', '?', '[', ']']) {
            ledger.fail(unprovable_path(&word.text, "含有 cmdlet 自行展开的通配符", "contains a wildcard the cmdlet expands itself"));
            return None;
        }
        if writes {
            ledger.write(scene, word)
        } else {
            ledger.read(scene, word)
        }
    };
    let mut target = None;
    let mut index = 1;
    while let Some(word) = words.get(index) {
        index += 1;
        let text = word.text.as_str();
        // A quoted `'-Name'` is an argument to PowerShell, not a parameter.
        if word.quoted && text.starts_with('-') {
            return unverified(text);
        }
        let parameter = text
            .strip_prefix('-')
            .filter(|rest| rest.chars().next().is_some_and(|first| first.is_ascii_alphabetic()));
        let Some(parameter) = parameter else {
            let role = match (rule.positional, positionals) {
                (Positional::Text, _) => Some(OptionArg::Text),
                (Positional::Path | Positional::PathThenText, 0) => Some(OptionArg::Read),
                (Positional::PathThenText, 1) => Some(OptionArg::Text),
                (Positional::PatternThenPath, 0) => Some(OptionArg::Text),
                (Positional::PatternThenPath, 1) => Some(OptionArg::Read),
                _ => None,
            };
            positionals += 1;
            match role {
                Some(OptionArg::Text) => {}
                Some(_) => target = path(&mut ledger, word).or(target),
                None => return unverified(text),
            }
            continue;
        };
        // `-Name:value` binds in one word.
        let (parameter, inline) = match parameter.split_once(':') {
            Some((parameter, _)) => (parameter, Some(word.tail(1 + parameter.len() + 1))),
            None => (parameter, None),
        };
        // The preference parameters every cmdlet has, with the values that
        // only change how an error is reported.
        if parameter.eq_ignore_ascii_case("ErrorAction") || parameter.eq_ignore_ascii_case("WarningAction") {
            let value = inline.or_else(|| {
                index += 1;
                words.get(index - 1).cloned()
            });
            let known = value.is_some_and(|value| {
                ["SilentlyContinue", "Stop", "Continue", "Ignore"]
                    .iter()
                    .any(|action| value.text.eq_ignore_ascii_case(action))
            });
            if !known {
                return unverified(text);
            }
            continue;
        }
        if rule.switches.iter().any(|known| known.eq_ignore_ascii_case(parameter)) {
            if inline.is_some() {
                return unverified(text);
            }
            continue;
        }
        let Some((_, role)) = rule.valued.iter().find(|(known, _)| known.eq_ignore_ascii_case(parameter)) else {
            return unverified(text);
        };
        let value = match inline {
            Some(value) => value,
            None => {
                let Some(value) = words.get(index) else {
                    return unverified(text);
                };
                index += 1;
                value.clone()
            }
        };
        if *role != OptionArg::Text {
            target = path(&mut ledger, &value).or(target);
        }
    }
    if paths > 1 || (paths == 0 && rule.needs_path) {
        return unverified(if paths > 1 { "-Path" } else { "(path)" });
    }
    // A search with nothing to search would wait for input.
    if name == "Select-String" && paths == 0 && !search_of_text {
        return unverified("-Path");
    }
    match rule.effect {
        CmdletEffect::ReadHere if paths == 0 => ledger.read_working_directory(scene),
        CmdletEffect::ChangeDirectory => ledger.facts.role = CommandRole::ChangeDirectory,
        CmdletEffect::Write => ledger.facts.role = CommandRole::FileChange,
        _ => {}
    }
    let facts = ledger.facts;
    if let Some(reason) = ledger.failure {
        let rule_id = if writes {
            "powershell.file_change_unverified"
        } else {
            "powershell.unverifiable_path"
        };
        return ShellAssessment::unbounded(rule_id, reason).with_facts(facts);
    }
    match rule.effect {
        CmdletEffect::Write => ShellAssessment::workspace_write(
            "powershell.workspace_file_edit",
            Reason::new(format!("{name} 只修改工作区内的显式路径"), format!("{name} only changes explicit paths inside the workspace")),
        )
        .with_facts(facts),
        CmdletEffect::ChangeDirectory if ledger.beyond || !target.is_some_and(|target| target.is_dir()) => {
            ShellAssessment::unbounded(
                "shell.cd_outside_trusted_roots",
                Reason::new("cd 目标无法证明位于工作区或应用数据目录内", "The cd target cannot be proven to be inside the workspace or the app data directory"),
            )
            .with_facts(facts)
        }
        _ if ledger.beyond => ShellAssessment::read_beyond_trusted_roots(
            "powershell.read_outside_trusted_roots",
            Reason::new("只读 cmdlet 显式引用了工作区和应用数据目录之外的路径", "The read-only cmdlet names a path outside the workspace and the app data directory"),
        )
        .with_facts(facts),
        _ => ShellAssessment::read(
            "powershell.read_only",
            Reason::new(
                format!("{name} 属于内置只读 cmdlet 集合"),
                format!("{name} is in the built-in read-only cmdlet set"),
            ),
        )
        .with_facts(facts),
    }
}

/// What a command's recursive deletes put at risk. Judged per delete, on the
/// targets that delete itself names: a recursive `rm` of a project folder does
/// not become critical because some other part of the line mentions `/var`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteRisk {
    /// A target is the filesystem root, a home directory, a system directory,
    /// an ancestor of one, or a glob over one's contents.
    Critical,
    /// A target is built from a variable or a command substitution, or comes
    /// from input Mewrk cannot see (`xargs`, `find -exec`), so what the
    /// delete removes cannot be checked.
    Unverifiable,
}

/// One word of a command line, unquoted, with what its quoting left live.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ShellWord {
    text: String,
    /// An expansion the shell performs before the command sees the word: a
    /// variable, a command substitution, a PowerShell subexpression.
    dynamic: bool,
    /// Unquoted glob characters.
    glob: bool,
}

/// The highest risk among `command`'s recursive deletes (`rm -r`, `rm -R`,
/// `rm --recursive` in a POSIX shell; `Remove-Item -Recurse` and its aliases in
/// PowerShell), or `None` when it has none that is critical or unverifiable.
/// Relative targets resolve against `workspace`, where the command starts.
fn recursive_delete_risk(kind: ShellKind, command: &str, workspace: &Path) -> Option<DeleteRisk> {
    // PowerShell reads an en dash as the dash of a parameter and a curly quote
    // as a quote, so a delete spelled with them is the same delete.
    let normalized;
    let command = if kind == ShellKind::PowerShell && command.chars().any(is_powershell_lookalike) {
        normalized = command
            .chars()
            .map(|character| match character {
                '\u{2013}'..='\u{2015}' => '-',
                '\u{2018}'..='\u{201B}' => '\'',
                '\u{201C}'..='\u{201F}' => '"',
                character => character,
            })
            .collect::<String>();
        normalized.as_str()
    } else {
        command
    };
    let mut risk = None;
    for words in simple_commands(kind, command) {
        let Some(delete) = recursive_delete(kind, &words) else {
            continue;
        };
        let verdict = match delete {
            RecursiveDelete::FromUnseenInput => Some(DeleteRisk::Unverifiable),
            RecursiveDelete::Targets(targets) => {
                if targets.iter().any(|target| target.dynamic) {
                    Some(DeleteRisk::Unverifiable)
                } else if targets
                    .iter()
                    .any(|target| critical_delete_target(kind, target, workspace))
                {
                    Some(DeleteRisk::Critical)
                } else {
                    None
                }
            }
        };
        match verdict {
            Some(DeleteRisk::Critical) => return verdict,
            Some(DeleteRisk::Unverifiable) => risk = verdict,
            None => {}
        }
    }
    risk
}

/// Splits a command line into its simple commands, each as its words.
///
/// Every operator and grouping character ends a command — `;`, `&`, `|`, a
/// newline, parentheses and braces — so a delete inside a subshell, a block or
/// a pipeline is found as its own command. Only the words matter here; what
/// the grouping means is the rest of the classifier's business. A brace
/// expansion splits too, which errs toward asking: `/{usr,etc}` yields `/`.
fn simple_commands(kind: ShellKind, command: &str) -> Vec<Vec<ShellWord>> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = ShellWord::default();
    let mut started = false;
    let mut quote = None;
    let mut escaped = false;
    let escape = match kind {
        ShellKind::Bash => '\\',
        ShellKind::PowerShell => '`',
    };
    fn end_word(words: &mut Vec<ShellWord>, word: &mut ShellWord, started: &mut bool) {
        if *started {
            words.push(std::mem::take(word));
        }
        *started = false;
    }
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if escaped {
            word.text.push(character);
            started = true;
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.text.push(character);
                }
                continue;
            }
            Some(_) => {
                match character {
                    '"' => quote = None,
                    character if character == escape => escaped = true,
                    '$' | '`' => {
                        word.dynamic = true;
                        word.text.push(character);
                    }
                    character => word.text.push(character),
                }
                continue;
            }
            None => {}
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            character if character == escape => {
                escaped = true;
                started = true;
            }
            '#' if !started => {
                // A comment runs to the end of the line.
                for rest in chars.by_ref() {
                    if rest == '\n' {
                        break;
                    }
                }
                end_word(&mut words, &mut word, &mut started);
                commands.push(std::mem::take(&mut words));
            }
            ';' | '&' | '|' | '\n' | '(' | ')' | '{' | '}' => {
                end_word(&mut words, &mut word, &mut started);
                commands.push(std::mem::take(&mut words));
            }
            ',' if kind == ShellKind::PowerShell => {
                end_word(&mut words, &mut word, &mut started);
            }
            character if character.is_whitespace() => {
                end_word(&mut words, &mut word, &mut started);
            }
            character => {
                match character {
                    '$' | '`' => word.dynamic = true,
                    '*' | '?' | '[' => word.glob = true,
                    _ => {}
                }
                word.text.push(character);
                started = true;
            }
        }
    }
    end_word(&mut words, &mut word, &mut started);
    commands.push(words);
    commands.retain(|words| !words.is_empty());
    commands
}

enum RecursiveDelete<'a> {
    /// The paths are named on the command line.
    Targets(Vec<&'a ShellWord>),
    /// The paths arrive on standard input or from a search.
    FromUnseenInput,
}

/// The recursive delete `words` performs, if it is one.
fn recursive_delete(kind: ShellKind, words: &[ShellWord]) -> Option<RecursiveDelete<'_>> {
    match kind {
        ShellKind::Bash => posix_recursive_delete(words),
        ShellKind::PowerShell => powershell_recursive_delete(words),
    }
}

fn posix_recursive_delete(words: &[ShellWord]) -> Option<RecursiveDelete<'_>> {
    let command_name = |word: &ShellWord| {
        word.text
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    // Paths a search hands to `rm` (`find … -exec rm -rf {} +`) are as unseen
    // as those `xargs` reads.
    if let Some(position) = words
        .iter()
        .position(|word| matches!(word.text.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
    {
        let rest = &words[position + 1..];
        if rest.first().is_some_and(|word| command_name(word) == "rm") && rm_is_recursive(&rest[1..]) {
            return Some(RecursiveDelete::FromUnseenInput);
        }
    }
    let mut index = 0;
    // Assignments and the wrappers that run the next word as the command.
    while let Some(word) = words.get(index) {
        let name = command_name(word);
        let assignment = word.text.split_once('=').is_some_and(|(variable, _)| {
            !variable.is_empty()
                && variable
                    .chars()
                    .all(|character| character == '_' || character.is_ascii_alphanumeric())
        });
        if assignment {
            index += 1;
            continue;
        }
        match name.as_str() {
            "sudo" | "doas" | "command" | "builtin" | "exec" | "nohup" | "time" | "nice" | "env"
            | "stdbuf" | "noglob" | "setsid" | "ionice" | "timeout" => {
                index += 1;
                while words.get(index).is_some_and(|word| word.text.starts_with('-')) {
                    // `sudo -u user`, `nice -n 10`, `timeout -k 5`: an option
                    // with a value.
                    let takes_value = matches!(
                        words[index].text.as_str(),
                        "-u" | "-g" | "-n" | "-C" | "-k" | "-s" | "-c"
                    );
                    index += if takes_value { 2 } else { 1 };
                }
                // `timeout 5 rm …`: the duration comes before the command.
                if name == "timeout" {
                    index += 1;
                }
            }
            "xargs" => {
                index += 1;
                while words.get(index).is_some_and(|word| word.text.starts_with('-')) {
                    let takes_value = matches!(
                        words[index].text.as_str(),
                        "-I" | "-n" | "-P" | "-L" | "-d" | "-E" | "-s" | "-a"
                    );
                    index += if takes_value { 2 } else { 1 };
                }
                let rest = words.get(index..).unwrap_or_default();
                return (rest.first().is_some_and(|word| command_name(word) == "rm")
                    && rm_is_recursive(&rest[1..]))
                .then_some(RecursiveDelete::FromUnseenInput);
            }
            _ => break,
        }
    }
    let rest = words.get(index..)?;
    // `find <start…> -delete` removes everything under its starting points.
    if command_name(rest.first()?) == "find" && rest.iter().any(|word| word.text == "-delete") {
        let starts = rest[1..]
            .iter()
            .skip_while(|word| {
                matches!(word.text.as_str(), "-H" | "-L" | "-P") || word.text.starts_with("-O")
            })
            .take_while(|word| !word.text.starts_with('-') && word.text != "!")
            .collect();
        return Some(RecursiveDelete::Targets(starts));
    }
    // The command itself, or `rm` behind a wrapper this list does not know
    // (`chronic rm -rf /`): any later word that names it is read as the
    // command, which only ever adds a question.
    let position = if command_name(rest.first()?) == "rm" {
        0
    } else {
        rest.iter().position(|word| command_name(word) == "rm")?
    };
    let rest = &rest[position..];
    if !rm_is_recursive(&rest[1..]) {
        return None;
    }
    let mut targets = Vec::new();
    let mut options_over = false;
    for word in &rest[1..] {
        if !options_over && word.text == "--" {
            options_over = true;
        } else if options_over || !word.text.starts_with('-') || word.text == "-" {
            targets.push(word);
        }
    }
    Some(RecursiveDelete::Targets(targets))
}

/// Whether `rm`'s arguments ask it to recurse.
fn rm_is_recursive(arguments: &[ShellWord]) -> bool {
    arguments
        .iter()
        .take_while(|word| word.text != "--")
        .any(|word| {
            word.text == "--recursive"
                || (word.text.starts_with('-')
                    && !word.text.starts_with("--")
                    && word.text[1..].chars().any(|flag| matches!(flag, 'r' | 'R')))
        })
}

fn powershell_recursive_delete(words: &[ShellWord]) -> Option<RecursiveDelete<'_>> {
    let (name, arguments) = words.split_first()?;
    if !matches!(
        name.text.to_ascii_lowercase().as_str(),
        "remove-item" | "rm" | "ri" | "del" | "erase" | "rd" | "rmdir"
    ) {
        return None;
    }
    let mut recursive = false;
    let mut targets = Vec::new();
    // A target this reading cannot hand back as a word: `-Path:C:\`.
    let mut unseen_target = false;
    let mut index = 0;
    while let Some(word) = arguments.get(index) {
        index += 1;
        let Some(parameter) = word.text.strip_prefix('-').filter(|_| !word.dynamic) else {
            targets.push(word);
            continue;
        };
        let (parameter, inline_value) = match parameter.split_once(':') {
            Some((parameter, value)) => (parameter.to_ascii_lowercase(), Some(value)),
            None => (parameter.to_ascii_lowercase(), None),
        };
        let is = |full: &str, shortest: usize| {
            parameter.len() >= shortest && full.starts_with(parameter.as_str())
        };
        if is("recurse", 1) {
            recursive = inline_value.is_none_or(|value| !value.eq_ignore_ascii_case("$false"));
        } else if is("path", 2) || is("literalpath", 2) || is("pspath", 2) || parameter == "lp" {
            if inline_value.is_some() {
                unseen_target = true;
            } else if let Some(value) = arguments.get(index) {
                targets.push(value);
                index += 1;
            }
        } else if is("filter", 1) || is("include", 1) || is("exclude", 1) || is("credential", 2) || is("stream", 2)
        {
            if inline_value.is_none() {
                index += 1;
            }
        }
    }
    if !recursive {
        return None;
    }
    // With no target of its own a recursive `Remove-Item` deletes what the
    // pipeline hands it (`Get-ChildItem C:\ | Remove-Item -Recurse`).
    Some(if unseen_target || targets.is_empty() {
        RecursiveDelete::FromUnseenInput
    } else {
        RecursiveDelete::Targets(targets)
    })
}

/// A path as the delete classifier compares it: what it is anchored at and
/// the folders under that, lexically normalized and lowercased.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeletePath {
    anchor: DeleteAnchor,
    folders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum DeleteAnchor {
    /// `/` on a POSIX system, the current drive's root on Windows.
    Root,
    /// A drive root, `C:\`.
    Drive(char),
    /// The home directory of whoever runs the command (`~`), or of a named
    /// user (`~name`).
    Home,
}

/// Whether deleting `target` recursively removes the filesystem root, a home
/// directory or a system directory: the target is one, an ancestor of one, or
/// a glob over one's contents (`/usr/*`, `~/*`). Deeper paths — a project's
/// build folder under the home directory, `/tmp/build` — are ordinary deletes
/// the security level decides on.
fn critical_delete_target(kind: ShellKind, target: &ShellWord, workspace: &Path) -> bool {
    // A drive path keeps its backslashes through a POSIX shell's quotes
    // (`rm -rf 'C:\Users'`), and Git Bash opens it all the same.
    let drive_spelled = target.text.as_bytes().get(1) == Some(&b':');
    let text = match kind {
        ShellKind::Bash if !drive_spelled => target.text.clone(),
        ShellKind::Bash | ShellKind::PowerShell => target.text.replace('\\', "/"),
    };
    // A POSIX layer on Windows mounts the drives, so the same word is also
    // read as the drive path it would be there. That only ever adds a
    // question: on a machine with no such mount the directory is one letter
    // long and sits at the root.
    let mounted = (kind == ShellKind::Bash)
        .then(|| mounted_drive_path(&text))
        .flatten();
    [Some(text), mounted].into_iter().flatten().any(|text| {
        let Some(path) = delete_path(&text, workspace) else {
            return false;
        };
        // A glob is judged by the folder it lists: `/u*` lists `/`.
        let folders = if target.glob {
            let first_glob = path
                .folders
                .iter()
                .position(|folder| folder.contains(['*', '?', '[']))
                .unwrap_or(path.folders.len());
            path.folders[..first_glob].to_vec()
        } else {
            path.folders.clone()
        };
        let listed = DeletePath {
            anchor: path.anchor,
            folders,
        };
        protected_directory(&listed) || is_ancestor_of_protected(&listed)
    })
}

/// The drive path a POSIX layer on Windows means by `text`, when it spells
/// one: `/c/Users` in Git Bash, `/mnt/c/Users` in WSL, `/cygdrive/c/Users` in
/// Cygwin.
fn mounted_drive_path(text: &str) -> Option<String> {
    let rest = text.strip_prefix('/')?;
    let rest = rest
        .strip_prefix("mnt/")
        .or_else(|| rest.strip_prefix("cygdrive/"))
        .unwrap_or(rest);
    let (drive, tail) = rest.split_once('/').unwrap_or((rest, ""));
    (drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic())
        .then(|| format!("{drive}:/{tail}"))
}

/// `text` as a [`DeletePath`], relative paths resolved against `workspace`.
fn delete_path(text: &str, workspace: &Path) -> Option<DeletePath> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (anchor, rest) = if let Some(rest) = text.strip_prefix('~') {
        // `~name/…` is another user's home: the name is not a folder in it.
        let rest = rest.split_once('/').map_or("", |(_, rest)| rest);
        (DeleteAnchor::Home, rest.to_owned())
    } else if text.len() >= 2 && text.as_bytes()[1] == b':' && text.as_bytes()[0].is_ascii_alphabetic() {
        (
            DeleteAnchor::Drive(text.as_bytes()[0].to_ascii_lowercase() as char),
            text[2..].to_owned(),
        )
    } else if text.starts_with('/') {
        (DeleteAnchor::Root, text.to_owned())
    } else {
        let base = workspace_delete_path(workspace)?;
        let joined = base
            .folders
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(text))
            .collect::<Vec<_>>()
            .join("/");
        (base.anchor, joined)
    };
    let mut folders = Vec::new();
    for folder in rest.split('/') {
        match folder {
            "" | "." => {}
            ".." => {
                folders.pop();
            }
            folder => folders.push(folder.to_lowercase()),
        }
    }
    Some(DeletePath { anchor, folders })
}

/// The workspace a command starts in, as a [`DeletePath`].
fn workspace_delete_path(workspace: &Path) -> Option<DeletePath> {
    let mut anchor = DeleteAnchor::Root;
    let mut folders = Vec::new();
    for component in workspace.components() {
        match component {
            Component::Prefix(prefix) => {
                let text = prefix.as_os_str().to_string_lossy().to_lowercase();
                let letter = text.trim_start_matches(r"\\?\").chars().next()?;
                anchor = DeleteAnchor::Drive(letter);
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                folders.pop();
            }
            Component::Normal(folder) => folders.push(folder.to_string_lossy().to_lowercase()),
        }
    }
    Some(DeletePath { anchor, folders })
}

/// The directories a recursive delete must never take without asking, at
/// every level: roots, homes and the operating system's own folders, on POSIX
/// systems and on Windows alike (PowerShell runs on both).
fn protected_directory(path: &DeletePath) -> bool {
    const SYSTEM: &[&str] = &[
        "applications", "bin", "boot", "dev", "etc", "home", "lib", "lib32", "lib64", "library",
        "opt", "private", "private/etc", "private/tmp", "private/var", "proc", "root", "sbin",
        "srv", "sys", "system", "tmp", "users", "usr", "usr/bin", "usr/lib", "usr/local",
        "usr/sbin", "usr/share", "var", "volumes", "windows", "program files",
        "program files (x86)", "programdata",
    ];
    let folders = path.folders.iter().map(String::as_str).collect::<Vec<_>>();
    match path.anchor {
        DeleteAnchor::Home => folders.is_empty(),
        DeleteAnchor::Root | DeleteAnchor::Drive(_) => {
            folders.is_empty()
                || SYSTEM.contains(&folders.join("/").as_str())
                // The whole Windows directory, not only its top: unlike
                // `/usr/local`, nothing is ever built under it, so no depth of
                // `System32` is an ordinary delete.
                || folders.first() == Some(&"windows")
                || matches!(folders.as_slice(), ["users" | "home", _])
                || host_home().is_some_and(|home| home == *path)
        }
    }
}

/// Whether `path` contains a protected directory, which deleting it would take
/// along: `/Users` holds every home.
fn is_ancestor_of_protected(path: &DeletePath) -> bool {
    path.anchor != DeleteAnchor::Home
        && host_home().is_some_and(|home| {
            home.anchor == path.anchor
                && home.folders.len() > path.folders.len()
                && home.folders.starts_with(&path.folders)
        })
}

/// This machine's home directory as a [`DeletePath`]. A command on another
/// machine is still judged against it, which only ever adds a question.
fn host_home() -> Option<DeletePath> {
    static HOME: std::sync::OnceLock<Option<DeletePath>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = dirs::home_dir()?;
        workspace_delete_path(&fs::canonicalize(&home).unwrap_or(home))
    })
    .clone()
}

fn classify_unbounded(level: SecurityLevel) -> SecurityDecision {
    match level {
        SecurityLevel::FullAccess => SecurityDecision {
            requires_approval: false,
            mandatory_prompt: false,
            risk_level: RiskLevel::High,
            rule_id: "tool.unbounded",
            reason: Reason::new(
                "完全访问允许执行已验证的命令工具",
                "Full access allows validated command tools",
            ),
            scope: ExecutionScope::Unrestricted,
            effect: OperationEffect::Unbounded,
            target: None,
        },
        SecurityLevel::RequestApproval | SecurityLevel::AllowEdits => {
            SecurityDecision {
                requires_approval: true,
                mandatory_prompt: false,
                risk_level: RiskLevel::High,
                rule_id: "tool.unbounded",
                reason: Reason::new(
                    "命令执行无法可靠限制为可信目录内的只读或写入操作",
                    "Command execution cannot be reliably limited to reads or writes inside trusted directories",
                ),
                scope: ExecutionScope::Unrestricted,
                effect: OperationEffect::Unbounded,
                target: None,
            }
        }
    }
}

fn classify_browser_sensitive(level: SecurityLevel, tool_name: &str) -> SecurityDecision {
    let (subject_zh, subject) = match tool_name {
        "preview_console_logs" => (
            "控制台日志，其中可能有令牌和错误上下文",
            "console logs, which may contain tokens and error context",
        ),
        "preview_network" => (
            "网络日志，其中可能有完整 URL、查询参数和响应正文",
            "network logs, which may contain complete URLs, query parameters, and response bodies",
        ),
        "preview_screenshot" => (
            "当前页面的像素，无论它登录了什么",
            "the pixels of the current page, whatever it is signed in to",
        ),
        _ => ("浏览器敏感数据", "browser-sensitive data"),
    };
    SecurityDecision {
        requires_approval: level != SecurityLevel::FullAccess,
        mandatory_prompt: false,
        risk_level: RiskLevel::High,
        rule_id: "browser.sensitive_observation",
        reason: Reason::new(
            format!("这个工具会读取{subject_zh}"),
            format!("This tool will read {subject}"),
        ),
        scope: ExecutionScope::Unrestricted,
        effect: OperationEffect::Unbounded,
        target: None,
    }
}

fn classify_filesystem(
    level: SecurityLevel,
    effect: OperationEffect,
    target: PathBuf,
    is_trusted: bool,
    protected_app_data: bool,
    restricted: ExecutionScope,
    unrestricted: ExecutionScope,
) -> Result<SecurityDecision, String> {
    if level == SecurityLevel::FullAccess {
        return Ok(SecurityDecision {
            requires_approval: false,
            mandatory_prompt: false,
            risk_level: match effect {
                OperationEffect::Read => RiskLevel::Low,
                OperationEffect::Write => RiskLevel::Medium,
                OperationEffect::Unbounded => RiskLevel::High,
            },
            rule_id: "filesystem.full_access",
            reason: Reason::new(
                "完全访问允许执行已验证的文件操作",
                "Full access allows validated file operations",
            ),
            scope: unrestricted,
            effect,
            target: Some(target),
        });
    }

    let scope = if is_trusted { restricted } else { unrestricted };
    if !is_trusted {
        // AllowEdits permits reads outside the workspace; other outside access requires
        // approval below FullAccess.
        let outside_read_allowed =
            level == SecurityLevel::AllowEdits && effect == OperationEffect::Read;
        return Ok(SecurityDecision {
            requires_approval: !outside_read_allowed,
            mandatory_prompt: false,
            risk_level: if outside_read_allowed {
                RiskLevel::Medium
            } else {
                RiskLevel::High
            },
            rule_id: if outside_read_allowed {
                "filesystem.outside_read_allow_edits"
            } else {
                "filesystem.outside_trusted_roots"
            },
            reason: if outside_read_allowed {
                Reason::new(
                    "目标在工作区外；允许编辑模式放行工作区外读取",
                    "The target is outside the workspace; Accept edits allows reads outside it",
                )
            } else {
                Reason::new(
                    "目标位于工作区和应用数据目录之外",
                    "The target is outside the workspace and the app data directory",
                )
            },
            scope,
            effect,
            target: Some(target),
        });
    }
    if protected_app_data && level == SecurityLevel::AllowEdits {
        return Ok(SecurityDecision {
            requires_approval: true,
            mandatory_prompt: false,
            risk_level: RiskLevel::High,
            rule_id: "filesystem.protected_app_data",
            reason: Reason::new(
                "目标是应用控制数据，自动写入可能修改安全策略",
                "The target is app control data; writing it automatically could change the security policy",
            ),
            scope,
            effect,
            target: Some(target),
        });
    }

    Ok(match (level, effect) {
        (SecurityLevel::RequestApproval, OperationEffect::Read) => {
            SecurityDecision {
                requires_approval: false,
                mandatory_prompt: false,
                risk_level: RiskLevel::Low,
                rule_id: "filesystem.trusted_read",
                reason: Reason::new(
                    "只读目标位于工作区或应用数据目录内",
                    "The read target is inside the workspace or the app data directory",
                ),
                scope,
                effect,
                target: Some(target),
            }
        }
        (SecurityLevel::RequestApproval, OperationEffect::Write) => SecurityDecision {
            requires_approval: true,
            mandatory_prompt: false,
            risk_level: RiskLevel::Medium,
            rule_id: "filesystem.trusted_write_manual",
            reason: Reason::new(
                "请求批准模式要求确认所有写入操作",
                "Manual mode asks before every write",
            ),
            scope,
            effect,
            target: Some(target),
        },
        (SecurityLevel::AllowEdits, OperationEffect::Read | OperationEffect::Write) => {
            SecurityDecision {
                requires_approval: false,
                mandatory_prompt: false,
                risk_level: match effect {
                    OperationEffect::Read => RiskLevel::Low,
                    OperationEffect::Write => RiskLevel::Medium,
                    OperationEffect::Unbounded => RiskLevel::High,
                },
                rule_id: "filesystem.trusted_allow_edits",
                reason: Reason::new(
                    "允许编辑模式允许可信目录内的读写操作",
                    "Accept edits allows reads and writes inside trusted directories",
                ),
                scope,
                effect,
                target: Some(target),
            }
        }
        _ => {
            unreachable!("full access, unbounded effects and plan-mode writes are handled earlier")
        }
    })
}

/// The argument a tool names its path with. Every filesystem tool but `lsp`
/// calls it `path`; `lsp` keeps the source's `filePath`, because that name is
/// what the model was told to send.
fn path_key_for(tool: &str) -> &'static str {
    if tool == "lsp" {
        "filePath"
    } else {
        "path"
    }
}

/// Whether this workspace ships its own language-server configuration.
///
/// Only the file's existence is consulted, never its contents: the decision is
/// "does the project get to name a command here", and reading the file to find
/// out would be doing the thing the card is asking about.
fn workspace_declares_language_servers(workspace: &Path) -> bool {
    crate::capabilities::config_path_for(workspace, crate::capabilities::CapabilityKind::Lsp)
        .is_file()
}

fn path_argument(input: &JsonObject, key: &str, default: Option<&str>) -> Result<String, String> {
    match input.get(key) {
        None | Some(Value::Null) => default
            .map(str::to_owned)
            .ok_or_else(|| format!("Missing {key} argument")),
        Some(Value::String(path)) => {
            if path.trim().is_empty() {
                // A blank optional path is one left out, as the executor reads it.
                return default
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} argument must not be empty"));
            }
            if path.chars().count() > MAX_PATH_CHARS {
                return Err(format!(
                    "{key} argument exceeds the {MAX_PATH_CHARS}-character limit"
                ));
            }
            if path.contains('\0') {
                return Err(format!("{key} argument contains an invalid character"));
            }
            Ok(path.clone())
        }
        Some(_) => Err(format!("{key} argument must be a string")),
    }
}

fn validate_element_target(input: &JsonObject) -> Result<(), String> {
    crate::browser::element_target_input(input)
        .map(|_| ())
        .map_err(|error| format!("Missing or invalid element argument: {error}"))
}

fn validate_required_string(
    input: &JsonObject,
    key: &str,
    max_chars: usize,
    label: &str,
) -> Result<(), String> {
    required_string(input, key, max_chars, label).map(|_| ())
}

fn required_string<'a>(
    input: &'a JsonObject,
    key: &str,
    max_chars: usize,
    label: &str,
) -> Result<&'a str, String> {
    let value = input
        .get(key)
        .ok_or_else(|| format!("Missing {label} {key}"))?
        .as_str()
        .ok_or_else(|| format!("{label} {key} must be a string"))?;
    if value.trim().is_empty() {
        return Err(format!("{label} {key} must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(format!(
            "{label} {key} exceeds the {max_chars}-character limit"
        ));
    }
    Ok(value)
}

fn is_protected_app_data_target(target: &Path, app_data: &Path) -> bool {
    let Some(parent) = target.parent() else {
        return false;
    };
    // New write targets are returned lexically while their existing ancestor
    // is canonicalized for containment. Canonicalize the direct parent too so
    // Windows verbatim-path prefixes and symlinked app-data roots compare
    // consistently.
    let canonical_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if !same_path(&canonical_parent, app_data) {
        return false;
    }
    let Some(name) = target.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let normalized = normalize_name(name);
    normalized == "document.v1.json"
        || normalized.starts_with(".document.v1.json.tmp-")
        || (normalized.starts_with("document.v1.corrupt-") && normalized.ends_with(".json"))
}

#[cfg(windows)]
fn same_path(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn same_path(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn normalize_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

#[cfg(not(windows))]
fn normalize_name(name: &str) -> String {
    name.to_owned()
}

// ---------------------------------------------------------------------------
// Credential scanning
//
// These functions scan arbitrary text for credentials during project instruction loading
// and instruction-file write validation, so they belong to the security boundary.
// ---------------------------------------------------------------------------

/// High-confidence credential detector shared by model memory and project
/// instruction loading. It intentionally ignores ordinary hashes/UUIDs while
/// rejecting known token formats, credential-labelled assignments, cookies,
/// and long mixed-alphabet high-entropy tokens.
pub(crate) fn contains_sensitive_secret(content: &str) -> bool {
    static DIRECT_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    static ASSIGNMENT_PATTERN: OnceLock<Regex> = OnceLock::new();
    let direct_patterns = DIRECT_PATTERNS.get_or_init(|| {
        [
            r"(?i)-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----",
            r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b",
            r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
            r"\bgithub_pat_[A-Za-z0-9_]{20,}\b",
            r"\bxox[baprs]-[A-Za-z0-9-]{20,}\b",
            r"\bsk-(?:proj-|ant-api03-|live-)?[A-Za-z0-9_-]{16,}\b",
            r"\b(?:pk|rk)_live_[A-Za-z0-9]{16,}\b",
            r"\bnpm_[A-Za-z0-9]{20,}\b",
            r"\bsq0atp-[A-Za-z0-9_-]{20,}\b",
            r"\bAIza[0-9A-Za-z_-]{30,}\b",
            r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b",
            r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]{16,}",
            r"(?i)(?:https?|mongodb(?:\+srv)?|postgres(?:ql)?|mysql)://[^\s/:@]+:[^\s/@]{4,}@[^\s/]+",
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("static secret pattern must compile"))
        .collect()
    });
    if direct_patterns
        .iter()
        .any(|pattern| pattern.is_match(content))
    {
        return true;
    }
    let assignment_pattern = ASSIGNMENT_PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:api[_ -]?key|secret(?:[_ -]?key)?|client[_ -]?secret|password|passwd|access[_ -]?token|refresh[_ -]?token|auth(?:orization)?[_ -]?token|id[_ -]?token|session(?:[_ -]?id|[_ -]?token)?|cookie|set-cookie|private[_ -]?key|credential)\b\s*[:=]\s*["']?([^\s"'`]{8,})"#,
        )
        .expect("static assignment secret pattern must compile")
    });
    assignment_pattern.captures_iter(content).any(|captures| {
        captures
            .get(1)
            .is_some_and(|capture| !looks_like_secret_placeholder(capture.as_str()))
    }) || contains_high_entropy_secret(content)
}

fn contains_high_entropy_secret(content: &str) -> bool {
    content
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+' | '/' | '='))
        })
        .map(|token| token.trim_matches('='))
        .filter(|token| (40..=512).contains(&token.len()))
        .any(|token| {
            if looks_like_secret_placeholder(token)
                || token.chars().all(|character| character.is_ascii_hexdigit())
            {
                return false;
            }
            let has_lower = token.bytes().any(|byte| byte.is_ascii_lowercase());
            let has_upper = token.bytes().any(|byte| byte.is_ascii_uppercase());
            let has_digit = token.bytes().any(|byte| byte.is_ascii_digit());
            let has_symbol = token
                .bytes()
                .any(|byte| matches!(byte, b'_' | b'-' | b'+' | b'/'));
            if [has_lower, has_upper, has_digit, has_symbol]
                .into_iter()
                .filter(|present| *present)
                .count()
                < 3
            {
                return false;
            }
            let mut frequencies = [0usize; 256];
            for byte in token.bytes() {
                frequencies[usize::from(byte)] += 1;
            }
            let length = token.len() as f64;
            let entropy = frequencies
                .into_iter()
                .filter(|count| *count > 0)
                .map(|count| {
                    let probability = count as f64 / length;
                    -probability * probability.log2()
                })
                .sum::<f64>();
            entropy >= 4.3
        })
}

fn looks_like_secret_placeholder(value: &str) -> bool {
    let value = value
        .trim_matches(|character: char| {
            matches!(character, '"' | '\'' | ',' | ';' | ')' | ']' | '}')
        })
        .to_ascii_lowercase();
    value.is_empty()
        || value
            .chars()
            .all(|character| matches!(character, '*' | 'x' | '•'))
        || [
            "example",
            "placeholder",
            "redacted",
            "changeme",
            "your_",
            "your-",
            "<your",
            "${",
            "{{",
            "[redacted",
        ]
        .iter()
        .any(|placeholder| value.contains(placeholder))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path_guard::resolve_existing_with_scope;
    use serde_json::{json, Map};
    use std::fs;

    fn request(workspace: &Path, tool_name: &str, input: Value) -> ToolExecutionRequest {
        ToolExecutionRequest {
            conversation_id: "conversation-test".into(),
            workspace_path: workspace.to_string_lossy().into_owned(),
            tool_name: tool_name.into(),
            input: input.as_object().cloned().unwrap_or_else(Map::new),
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        workspace: PathBuf,
        app_data: PathBuf,
        outside: PathBuf,
        /// Extra working directories granted to the conversation under test.
        additional: Vec<String>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let app_data = root.path().join("app-data");
            let outside = root.path().join("outside");
            fs::create_dir_all(&workspace).unwrap();
            fs::create_dir_all(&app_data).unwrap();
            fs::create_dir_all(&outside).unwrap();
            fs::write(workspace.join("inside.txt"), "inside").unwrap();
            fs::write(app_data.join("app.txt"), "app").unwrap();
            fs::write(outside.join("outside.txt"), "outside").unwrap();
            Self {
                _root: root,
                workspace,
                app_data,
                outside,
                additional: Vec::new(),
            }
        }

        /// Grants `outside` as an extra working directory, as the composer's
        /// directory picker does.
        fn granting_outside(mut self) -> Self {
            self.additional
                .push(self.outside.to_string_lossy().into_owned());
            self
        }

        fn classify(
            &self,
            level: SecurityLevel,
            tool_name: &str,
            input: Value,
        ) -> Result<SecurityDecision, String> {
            classify(
                level,
                &self.workspace,
                &self.app_data,
                &self.additional,
                &request(&self.workspace, tool_name, input),
            )
        }

        fn classify_model_call(
            &self,
            level: SecurityLevel,
            tool_name: &str,
            input: Value,
        ) -> Result<SecurityDecision, String> {
            super::classify_model_call(
                level,
                &self.workspace,
                &self.app_data,
                &self.additional,
                &request(&self.workspace, tool_name, input),
            )
        }

        /// A shell call that starts at the workspace root, as a call does
        /// before the workspace's first `cd`.
        fn shell(&self, level: SecurityLevel, tool: &str, command: &str) -> SecurityDecision {
            let start = ShellSite::Local {
                cwd: Some(self.workspace.clone()),
            };
            self.shell_from(level, tool, command, &start)
        }

        fn shell_from(
            &self,
            level: SecurityLevel,
            tool: &str,
            command: &str,
            site: &ShellSite,
        ) -> SecurityDecision {
            classify_shell(
                level,
                &self.workspace,
                &self.app_data,
                &self.additional,
                crate::shell_backend::ShellBackend::of_tool(tool).expect("a shell tool"),
                command,
                site,
            )
            .unwrap_or_else(|error| panic!("{tool}: {command}: {error}"))
        }
    }

    /// The tool picker's "Reviewed" marker is the catalog's `dangerous` flag,
    /// and it has to say what the classifier does: a tool is marked exactly
    /// when an ordinary call of it stops for the user at the Manual level —
    /// or, for `fork`, raises its own card at every level.
    #[test]
    fn the_reviewed_marker_is_exactly_the_tools_that_ask_at_manual() {
        let fixture = Fixture::new();
        let representative = |name: &str| -> Value {
            match name {
                "ls" | "grep" | "find" => json!({"path": ".", "pattern": "x"}),
                "read" => json!({"path": "inside.txt"}),
                "lsp" => json!({"operation": "hover", "filePath": "inside.txt", "line": 1, "character": 1}),
                "write" => json!({"path": "new.txt", "content": "x"}),
                "edit" => json!({"path": "inside.txt", "find": "inside", "replace": "x"}),
                // An ordinary command. The few a static proof clears (`ls`)
                // are the exception the marker does not describe.
                name if crate::shell_backend::ShellBackend::of_tool(name).is_some() => {
                    json!({"command": "npm test"})
                }
                "web_search" => json!({"query": "rust"}),
                "web_fetch" => json!({"urls": ["https://example.com"]}),
                "preview_start" => json!({"name": "web"}),
                "preview_stop" => json!({"serverId": "web"}),
                "preview_inspect" | "preview_click" => json!({"selector": "button"}),
                "preview_fill" => json!({"selector": "input", "value": "x"}),
                "preview_eval" => json!({"expression": "1"}),
                "preview_upload_image" => json!({"image_id": 1}),
                "plan" => json!({"action": "write", "markdown": "# Plan"}),
                _ => json!({}),
            }
        };
        let mismatched = crate::catalog::tool_catalog()
            .into_iter()
            .filter_map(|tool| {
                let decision = fixture
                    .classify_model_call(SecurityLevel::RequestApproval, &tool.name, representative(&tool.name))
                    .unwrap_or_else(|error| panic!("{}: {error}", tool.name));
                let asks = decision.requires_approval || tool.name == "fork";
                (tool.dangerous != asks).then(|| format!("{} (marked {}, asks {asks})", tool.name, tool.dangerous))
            })
            .collect::<Vec<_>>();
        assert!(mismatched.is_empty(), "{mismatched:?}");
    }

    /// The classifier runs before the card's language is known, so every
    /// reason it gives must be readable in both. A sample across the rule
    /// families: internal tools, network, browser, filesystem, and the shell
    /// analyzer's accept, refuse, and parse-failure branches.
    #[test]
    fn every_reason_reads_in_both_app_languages() {
        let fixture = Fixture::new();
        let han = |text: &str| {
            text.chars()
                .any(|character| ('\u{4E00}'..='\u{9FFF}').contains(&character))
        };
        let calls = [
            ("plan", json!({"action": "read"})),
            ("plan", json!({"action": "write", "content": "# Plan"})),
            ("create_global_memory", json!({})),
            ("agent_spawn", json!({})),
            (
                "send_message",
                json!({"to": "reviewer", "message": "go on"}),
            ),
            ("workflow", json!({})),
            ("web_search", json!({"query": "rust"})),
            ("web_fetch", json!({"urls": ["https://example.com"]})),
            ("preview_screenshot", json!({})),
            ("read", json!({"path": "inside.txt"})),
            ("write", json!({"path": "inside.txt", "content": "x"})),
            ("read", json!({"path": fixture.outside.join("outside.txt")})),
            ("bash", json!({"command": "ls"})),
            ("bash", json!({"command": "git push"})),
            ("bash", json!({"command": "echo \"unclosed"})),
            ("bash", json!({"command": "timeout"})),
            ("bash", json!({"command": "rm -rf /"})),
            ("bash", json!({"command": "curl https://example.com"})),
        ];
        for (tool, input) in calls {
            let decision = fixture
                .classify_model_call(SecurityLevel::RequestApproval, tool, input.clone())
                .unwrap_or_else(|error| panic!("{tool} {input}: {error}"));
            let zh = decision.reason.text(ResolvedLanguage::ZhCn);
            let en = decision.reason.text(ResolvedLanguage::EnUs);
            assert!(han(zh), "{tool} {input}: Chinese reason {zh:?}");
            assert!(
                !en.is_empty() && !han(en),
                "{tool} {input}: English reason {en:?}"
            );
        }
        assert_eq!(RiskLevel::High.label(ResolvedLanguage::EnUs), "High");
        assert_eq!(RiskLevel::High.label(ResolvedLanguage::ZhCn), "高");
    }

    #[test]
    fn missing_ls_target_is_classified_without_bypassing_boundaries() {
        let fixture = Fixture::new();
        let decision = fixture
            .classify_model_call(
                SecurityLevel::FullAccess,
                "ls",
                json!({"path":"missing/nested"}),
            )
            .unwrap();
        assert!(!decision.requires_approval);
        assert!(decision.target.unwrap().ends_with("missing/nested"));
        let denied = fixture.app_data.join("memory/missing");
        assert!(fixture
            .classify_model_call(SecurityLevel::FullAccess, "ls", json!({"path":denied}))
            .is_err());
        let outside = fixture.outside.join("missing");
        let decision = fixture
            .classify_model_call(
                SecurityLevel::RequestApproval,
                "ls",
                json!({"path":outside}),
            )
            .unwrap();
        assert!(decision.requires_approval);
    }

    #[test]
    fn a_granted_extra_directory_is_trusted_like_the_workspace() {
        let plain = Fixture::new();
        let target = plain.outside.join("notes.md");
        // Without the grant, writing there is an escape and has to be approved.
        let escaping = plain
            .classify_model_call(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path": target, "content": "x"}),
            )
            .unwrap();
        assert!(escaping.requires_approval);
        assert!(is_unrestricted(&escaping));

        let granted = Fixture::new().granting_outside();
        let target = granted.outside.join("notes.md");
        let decision = granted
            .classify_model_call(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path": target, "content": "x"}),
            )
            .unwrap();
        assert!(!decision.requires_approval, "{:?}", decision.rule_id);
        assert!(is_restricted(&decision));
        // The rest of the filesystem is no wider than before.
        let elsewhere = granted._root.path().join("elsewhere.md");
        let outside_grant = granted
            .classify_model_call(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path": elsewhere, "content": "x"}),
            )
            .unwrap();
        assert!(outside_grant.requires_approval);
    }

    #[test]
    fn an_extra_directory_that_no_longer_exists_narrows_instead_of_failing() {
        let mut fixture = Fixture::new();
        fixture
            .additional
            .push(fixture._root.path().join("never-created").display().to_string());
        let decision = fixture
            .classify_model_call(SecurityLevel::AllowEdits, "read", json!({"path": "inside.txt"}))
            .unwrap();
        assert!(!decision.requires_approval);
        let escaping = fixture
            .classify_model_call(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path": fixture.outside.join("outside.txt"), "content": "x"}),
            )
            .unwrap();
        assert!(escaping.requires_approval);
    }

    #[test]
    fn a_granted_extra_directory_bounds_shell_analysis_too() {
        let granted = Fixture::new().granting_outside();
        let decision = granted
            .classify_model_call(
                SecurityLevel::FullAccess,
                "bash",
                json!({"command": "cd ../outside"}),
            )
            .unwrap();
        assert_eq!(decision.risk_level, RiskLevel::Low, "{:?}", decision.reason);
        assert_ne!(decision.rule_id, "shell.cd_outside_trusted_roots");
        let plain = Fixture::new();
        let decision = plain
            .classify_model_call(
                SecurityLevel::FullAccess,
                "bash",
                json!({"command": "cd ../outside"}),
            )
            .unwrap();
        assert_eq!(
            decision.rule_id, "shell.cd_outside_trusted_roots",
            "{:?}",
            decision.reason
        );
    }

    fn is_restricted(decision: &SecurityDecision) -> bool {
        matches!(
            decision.scope,
            ExecutionScope::Restricted { .. } | ExecutionScope::RestrictedExcept { .. }
        )
    }

    fn is_unrestricted(decision: &SecurityDecision) -> bool {
        matches!(
            decision.scope,
            ExecutionScope::Unrestricted | ExecutionScope::UnrestrictedExcept { .. }
        )
    }

    #[test]
    fn request_approval_matrix_distinguishes_reads_writes_and_scope() {
        let fixture = Fixture::new();
        let workspace_read = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "read",
                json!({"path":"inside.txt"}),
            )
            .unwrap();
        let app_read = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "read",
                json!({"path":fixture.app_data.join("app.txt")}),
            )
            .unwrap();
        let outside_read = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "read",
                json!({"path":fixture.outside.join("outside.txt")}),
            )
            .unwrap();
        let inside_write = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "write",
                json!({"path":"new.txt","content":"new"}),
            )
            .unwrap();

        assert!(!workspace_read.requires_approval);
        assert!(!app_read.requires_approval);
        assert!(is_restricted(&workspace_read));
        assert!(is_restricted(&app_read));
        assert!(outside_read.requires_approval);
        assert!(is_unrestricted(&outside_read));
        assert!(inside_write.requires_approval);
        assert!(is_restricted(&inside_write));
    }

    /// Code navigation is a read of the file it names, and it names that file
    /// with `filePath` rather than `path`. The wrong key would silently classify
    /// against a missing argument instead of the file the model asked about.
    #[test]
    fn lsp_is_a_read_scoped_to_its_file_path_argument() {
        let fixture = Fixture::new();
        let inside = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "lsp",
                json!({
                    "operation": "goToDefinition",
                    "filePath": "inside.txt",
                    "line": 1,
                    "character": 1,
                }),
            )
            .expect("a workspace file classifies");
        assert!(!inside.requires_approval);
        assert!(is_restricted(&inside));
        assert_eq!(inside.effect, OperationEffect::Read);

        // Outside the workspace it takes the same boundary `read` takes, which
        // is the whole point of routing it through the filesystem arm.
        let outside = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "lsp",
                json!({
                    "operation": "hover",
                    "filePath": fixture.outside.join("outside.txt"),
                    "line": 1,
                    "character": 1,
                }),
            )
            .expect("an outside file classifies");
        assert!(outside.requires_approval);
        assert!(is_unrestricted(&outside));

        // `path` is the other tools' key; using it here must not resolve.
        let wrong_key = fixture.classify(
            SecurityLevel::RequestApproval,
            "lsp",
            json!({ "operation": "hover", "path": "inside.txt", "line": 1, "character": 1 }),
        );
        assert!(
            wrong_key.is_err_and(|reason| reason.contains("filePath")),
            "the missing-argument message must name the key the schema promised"
        );

        // A missing or oversized operation is refused before any path work.
        assert!(fixture
            .classify(
                SecurityLevel::RequestApproval,
                "lsp",
                json!({ "filePath": "inside.txt", "line": 1, "character": 1 }),
            )
            .is_err());
        // A project that ships its own `.mewrk/lsp.json` gets to name the
        // command Mewrk launches, so the call asks — the same boundary
        // `preview_start` draws around `launch.json`.
        std::fs::create_dir_all(fixture.workspace.join(".mewrk")).unwrap();
        std::fs::write(
            fixture.workspace.join(".mewrk").join("lsp.json"),
            r#"{"lspServers":{"x":{"command":"anything","extensionToLanguage":{".txt":"text"}}}}"#,
        )
        .unwrap();
        let project_declared = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "lsp",
                json!({
                    "operation": "goToDefinition",
                    "filePath": "inside.txt",
                    "line": 1,
                    "character": 1,
                }),
            )
            .expect("still classifies");
        assert!(
            project_declared.requires_approval,
            "a repository-supplied language-server command must not launch unasked"
        );
    }

    #[test]
    fn allow_edits_matrix_allows_trusted_writes_but_asks_outside() {
        let fixture = Fixture::new();
        for path in [
            fixture.workspace.join("new.txt"),
            fixture.app_data.join("new.txt"),
        ] {
            let decision = fixture
                .classify(
                    SecurityLevel::AllowEdits,
                    "write",
                    json!({"path":path,"content":"new"}),
                )
                .unwrap();
            assert!(!decision.requires_approval);
            assert!(is_restricted(&decision));
        }

        let outside = fixture
            .classify(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path":fixture.outside.join("new.txt"),"content":"new"}),
            )
            .unwrap();
        assert!(outside.requires_approval);
        assert!(is_unrestricted(&outside));
    }

    #[test]
    fn allow_edits_protects_application_control_documents() {
        let fixture = Fixture::new();
        for name in [
            "document.v1.json",
            ".document.v1.json.tmp-42-1",
            "document.v1.corrupt-20260712T120000Z.json",
        ] {
            let decision = fixture
                .classify(
                    SecurityLevel::AllowEdits,
                    "write",
                    json!({"path":fixture.app_data.join(name),"content":"tamper"}),
                )
                .unwrap();
            assert!(decision.requires_approval, "{name} must require approval");
            assert!(decision
                .reason
                .text(ResolvedLanguage::ZhCn)
                .contains("应用控制数据"));
            assert!(is_restricted(&decision));
        }

        let ordinary = fixture
            .classify(
                SecurityLevel::AllowEdits,
                "write",
                json!({"path":fixture.app_data.join("cache.txt"),"content":"ok"}),
            )
            .unwrap();
        assert!(!ordinary.requires_approval);
    }

    #[test]
    fn memory_storage_is_denied_before_any_full_access_decision() {
        let fixture = Fixture::new();
        let memory = fixture.app_data.join("memory");
        fs::create_dir_all(&memory).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            fs::write(memory.join(format!("memory.v1.sqlite3{suffix}")), "private").unwrap();
        }

        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            for tool in ["read", "ls", "grep", "find", "edit"] {
                let input = if tool == "grep" {
                    json!({"path":memory,"pattern":"private"})
                } else if tool == "find" {
                    json!({"path":memory,"query":"*"})
                } else if tool == "edit" {
                    json!({
                        "path":memory.join("memory.v1.sqlite3"),
                        "find":"private",
                        "replace":"stolen"
                    })
                } else {
                    json!({"path":memory.join("memory.v1.sqlite3")})
                };
                assert!(
                    fixture.classify(level, tool, input).is_err(),
                    "{level:?} {tool} must not receive an executable decision"
                );
            }
            assert!(fixture
                .classify(
                    level,
                    "write",
                    json!({
                        "path":memory.join("memory.v1.sqlite3-wal"),
                        "content":"tamper"
                    })
                )
                .is_err());
        }
    }

    /// The file checkpoints are what a rewind puts back. A model that could
    /// read them would see files it was never shown, and one that could write
    /// them would decide what the user's rewind restores.
    #[test]
    fn file_checkpoints_are_denied_before_any_full_access_decision() {
        let fixture = Fixture::new();
        let history = fixture
            .app_data
            .join(crate::workspace_dirs::FILE_HISTORY_ROOT)
            .join("conversation");
        fs::create_dir_all(&history).unwrap();
        let saved = history.join("0f".repeat(32));
        fs::write(&saved, "before the edit").unwrap();

        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            for (tool, input) in [
                ("read", json!({"path":saved})),
                ("ls", json!({"path":history})),
                ("grep", json!({"path":history,"pattern":"before"})),
                ("find", json!({"path":history,"query":"*"})),
                ("edit", json!({"path":saved,"find":"before","replace":"after"})),
                ("write", json!({"path":saved,"content":"tamper"})),
            ] {
                assert!(
                    fixture.classify(level, tool, input).is_err(),
                    "{level:?} {tool} must not receive an executable decision"
                );
            }
        }
    }

    #[test]
    fn powershell_http_urls_are_not_provider_paths() {
        for value in [
            "http://example.com",
            "https://example.com",
            "HTTPS://example.com",
            "'https://example.com'",
            "\"http://example.com\"",
            "C:/work",
            r"C:\work",
        ] {
            assert!(!has_powershell_non_filesystem_provider(value), "{value}");
        }
        for value in [
            r"HKLM:\Software",
            r"Registry::HKEY_LOCAL_MACHINE\Software",
            "Cert:/item",
            "Store:/item",
            r"Store:\item",
            "https://example.com Store:/item",
        ] {
            assert!(has_powershell_non_filesystem_provider(value), "{value}");
        }
        let fixture = Fixture::new();
        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            let decision = fixture
                .classify(
                    level,
                    "powershell",
                    json!({"command":"Write-Output 'https://example.com'"}),
                )
                .unwrap();
            assert_eq!(decision.rule_id, "shell.read_only");
            assert_eq!(decision.effect, OperationEffect::Read);
            assert_eq!(decision.risk_level, RiskLevel::Low);
            // Printing a literal is proven read-only, so no level asks.
            assert!(!decision.requires_approval, "{level:?}");
            let network = fixture
                .classify(
                    level,
                    "powershell",
                    json!({"command":"Invoke-WebRequest https://example.com"}),
                )
                .unwrap();
            assert_eq!(network.rule_id, "powershell.unclassified_command");
        }
    }

    #[test]
    fn shell_static_analysis_distinguishes_read_only_and_unbounded_commands() {
        let fixture = Fixture::new();
        for level in [SecurityLevel::RequestApproval, SecurityLevel::AllowEdits] {
            // A proven read-only command asks at neither level; one that is
            // not proven asks at both.
            let read_only = fixture.shell(level, "powershell", "Get-ChildItem");
            assert!(!read_only.requires_approval);
            assert_eq!(read_only.effect, OperationEffect::Read);
            assert_eq!(read_only.risk_level, RiskLevel::Low);

            let unbounded =
                fixture.shell(level, "powershell", "Invoke-WebRequest https://example.com");
            assert!(unbounded.requires_approval);
            assert_eq!(unbounded.effect, OperationEffect::Unbounded);
            assert_eq!(unbounded.risk_level, RiskLevel::High);
            assert_eq!(unbounded.scope, ExecutionScope::Unrestricted);
        }
        let full = fixture
            .classify(
                SecurityLevel::FullAccess,
                "bash",
                json!({"command":"curl https://example.com"}),
            )
            .unwrap();
        assert!(!full.requires_approval);
        assert_eq!(full.risk_level, RiskLevel::High);
        assert_eq!(full.scope, ExecutionScope::Unrestricted);
    }

    #[test]
    fn shell_compound_commands_are_classified_by_the_riskiest_segment() {
        let fixture = Fixture::new();
        for (tool, command) in [
            ("bash", "git status --short && ls"),
            ("powershell", "Get-ChildItem | Select-Object -First 1"),
        ] {
            let decision = fixture.shell(SecurityLevel::RequestApproval, tool, command);
            // Every segment is proven read-only, so the line is; risk is the
            // maximum segment risk.
            assert!(!decision.requires_approval, "{tool}: {command}");
            assert_eq!(decision.risk_level, RiskLevel::Low);
        }

        for (tool, command) in [
            ("bash", "git status && curl https://example.com"),
            (
                "powershell",
                "Get-ChildItem; Invoke-WebRequest https://example.com",
            ),
        ] {
            let decision = fixture
                .classify(
                    SecurityLevel::RequestApproval,
                    tool,
                    json!({"command":command}),
                )
                .unwrap();
            assert!(decision.requires_approval, "{tool}: {command}");
            assert_eq!(decision.risk_level, RiskLevel::High);
        }
    }

    #[test]
    fn shell_workspace_writes_follow_the_write_tools_levels() {
        // A proven workspace write is what the `write` tool does: unasked at
        // Accept edits, asked about at Manual.
        let fixture = Fixture::new();
        let powershell = fixture.shell(
            SecurityLevel::AllowEdits,
            "powershell",
            "Set-Content -Path generated.txt -Value ok",
        );
        assert!(!powershell.requires_approval);
        assert_eq!(powershell.risk_level, RiskLevel::Medium);
        assert_eq!(powershell.rule_id, "powershell.workspace_file_edit");

        let bash = fixture.shell(SecurityLevel::AllowEdits, "bash", "touch generated.txt");
        assert!(!bash.requires_approval);
        assert_eq!(bash.risk_level, RiskLevel::Medium);
        for tool in ["powershell", "bash"] {
            let command = if tool == "bash" {
                "touch generated.txt"
            } else {
                "Set-Content -Path generated.txt -Value ok"
            };
            assert!(fixture.shell(SecurityLevel::RequestApproval, tool, command).requires_approval);
        }

        let outside = fixture
            .classify(
                SecurityLevel::AllowEdits,
                "powershell",
                json!({
                    "command": format!(
                        "Set-Content -Path \"{}\" -Value no",
                        fixture.outside.join("outside.txt").display()
                    )
                }),
            )
            .unwrap();
        assert!(outside.requires_approval);
        assert_eq!(outside.risk_level, RiskLevel::High);
    }

    #[test]
    fn allow_edits_reads_outside_workspace_without_prompt() {
        // AllowEdits permits reads outside the workspace but still asks for writes.
        let fixture = Fixture::new();
        let outside_read = fixture
            .classify(
                SecurityLevel::AllowEdits,
                "read",
                json!({"path":fixture.outside.join("outside.txt")}),
            )
            .unwrap();
        assert!(!outside_read.requires_approval);
        assert_eq!(outside_read.rule_id, "filesystem.outside_read_allow_edits");
        assert!(is_unrestricted(&outside_read));
    }

    #[test]
    fn agent_spawn_asks_in_request_approval() {
        // Delegation prompts only where the user is still deciding.
        let fixture = Fixture::new();
        for level in [SecurityLevel::RequestApproval] {
            let ask = fixture
                .classify_model_call(level, "agent_spawn", json!({}))
                .unwrap();
            assert!(ask.requires_approval, "{level:?}");
            assert!(!ask.mandatory_prompt, "{level:?}");
            assert_eq!(ask.rule_id, "agent.delegation");
        }
        for level in [SecurityLevel::AllowEdits, SecurityLevel::FullAccess] {
            let auto = fixture
                .classify_model_call(level, "agent_spawn", json!({}))
                .unwrap();
            assert!(!auto.requires_approval, "{level:?}");
        }
    }

    /// The main agent talking to its own child asks at no level: the message runs
    /// no tool of the sender's, and whatever a resumed child then does is
    /// classified call by call under the conversation's level like any child's.
    /// A manual execution has no pool to address, so it is refused outright.
    #[test]
    fn send_message_asks_at_no_level_and_cannot_be_run_by_hand() {
        let fixture = Fixture::new();
        let input = json!({"to": "reviewer", "message": "go on"});
        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            let decision = fixture
                .classify_model_call(level, "send_message", input.clone())
                .unwrap();
            assert!(
                !decision.requires_approval && !decision.mandatory_prompt,
                "{level:?}"
            );
            assert_eq!(decision.rule_id, "host.local_state_change", "{level:?}");
            assert_eq!(decision.risk_level, RiskLevel::Medium, "{level:?}");
            assert!(is_unrestricted(&decision), "{level:?}");
            assert!(decision
                .reason
                .text(ResolvedLanguage::EnUs)
                .contains("resume a finished child"));
            assert!(decision
                .reason
                .text(ResolvedLanguage::ZhCn)
                .contains("子代理"));

            let manual = fixture
                .classify(level, "send_message", input.clone())
                .unwrap_err();
            assert!(
                manual.contains("may only be scheduled by the model run loop"),
                "{level:?}: {manual}"
            );
        }
    }

    #[test]
    fn shell_fail_closed_guards_cover_dynamic_background_redirect_and_length() {
        let fixture = Fixture::new();
        for (tool, command, rule) in [
            ("bash", "echo $(whoami)", "shell.dynamic_expansion"),
            ("bash", "git status &", "shell.background_or_call_operator"),
            (
                "bash",
                "echo ok > secret.txt 2>/dev/null",
                "shell.redirection",
            ),
            (
                "powershell",
                "Get-ChildItem; & Get-ChildItem",
                "shell.background_or_call_operator",
            ),
            (
                "powershell",
                "Get-Content -Path \\\\server\\share\\file.txt",
                "shell.network_path",
            ),
        ] {
            let decision = fixture
                .classify(
                    SecurityLevel::RequestApproval,
                    tool,
                    json!({"command":command}),
                )
                .unwrap();
            assert_eq!(decision.rule_id, rule, "{tool}: {command}");
            assert!(decision.requires_approval, "{tool}: {command}");
            assert_eq!(decision.risk_level, RiskLevel::High);
        }

        let too_long = "x".repeat(MAX_SHELL_ANALYSIS_CHARS + 1);
        let decision = fixture
            .classify(
                SecurityLevel::RequestApproval,
                "bash",
                json!({"command":too_long}),
            )
            .unwrap();
        assert!(decision.requires_approval);
        assert_eq!(decision.rule_id, "shell.analysis_limit");
    }

    #[test]
    fn recursive_delete_circuit_breakers_survive_full_access() {
        let fixture = Fixture::new();
        for (tool, command) in [
            ("bash", "rm -rf /"),
            ("bash", "rm -rf \"/\""),
            ("bash", "rm -rf $HOME"),
            ("bash", "printf -v X '\\057'; rm -rf \"$X\""),
            ("powershell", "Remove-Item -Recurse C:\\"),
            ("powershell", "Remove-Item -Recurse -Force \"C:\\\""),
            ("powershell", "ri -Recurse -Force C:\\"),
            ("powershell", "Remove-Item -Re`curse C:\\"),
            ("powershell", "Remove-Item -Recurse \"$env:SystemDrive\\\""),
            ("powershell", "Remove-Item -Recurse $env:USERPROFILE"),
            ("bash", "rm -rf ~"),
            ("bash", "rm -rf ~/"),
            ("bash", "rm -rf ~/*"),
            ("bash", "rm -rf ~someone"),
            ("bash", "rm -rf /usr"),
            ("bash", "rm -rf /etc/"),
            ("bash", "rm -rf /*"),
            ("bash", "rm -rf /u*"),
            ("bash", "rm -rf /{usr,etc}"),
            ("bash", "rm -fr /Users/someone"),
            ("bash", "rm -R /home/someone/"),
            ("bash", "rm --recursive /var"),
            ("bash", "rm -rf -- /"),
            ("bash", "sudo rm -rf /opt"),
            ("bash", "env LC_ALL=C nice -n 5 rm -rf /Library"),
            ("bash", "(cd /tmp && rm -rf /)"),
            ("bash", "find . -name '*.tmp' -exec rm -rf {} +"),
            ("bash", "ls | xargs -n 1 rm -rf"),
            ("bash", "rm -rf build $(cat list)"),
            ("powershell", "Remove-Item -Recurse -Path C:\\Users"),
            ("powershell", "Remove-Item -Recurse 'C:\\Program Files'"),
            ("powershell", "rm -r C:\\Windows\\"),
            ("powershell", "Remove-Item -LiteralPath D:\\ -Recurse -Force"),
        ] {
            let decision = fixture
                .classify(SecurityLevel::FullAccess, tool, json!({"command":command}))
                .unwrap();
            assert!(decision.requires_approval, "{tool}: {command}");
            assert!(decision.mandatory_prompt, "{tool}: {command}");
            assert_eq!(decision.risk_level, RiskLevel::High);
        }
    }

    #[test]
    fn ordinary_recursive_deletes_follow_the_security_level() {
        let fixture = Fixture::new();
        let build = fixture.outside.join("project/build");
        for (tool, command) in [
            ("bash", "rm -rf /tmp/build".to_owned()),
            ("bash", format!("rm -rf {}", build.display())),
            ("bash", "rm -rf build node_modules".to_owned()),
            ("bash", "rm -rf ~/project/build".to_owned()),
            ("bash", "rm a.log && grep -rn needle /src".to_owned()),
            ("bash", "rm -rf /tmp/build && echo $(date)".to_owned()),
            ("bash", "rm -rf /usr/local/lib/node_modules/left-pad".to_owned()),
            ("powershell", "Remove-Item -Recurse -Force C:\\work\\app\\build".to_owned()),
            ("powershell", "Remove-Item -Recurse dist, out".to_owned()),
        ] {
            let decision = fixture
                .classify(SecurityLevel::FullAccess, tool, json!({"command":&command}))
                .unwrap();
            assert!(!decision.mandatory_prompt, "{tool}: {command} ({})", decision.rule_id);
            assert!(!decision.requires_approval, "{tool}: {command}");
        }
    }

    #[test]
    fn unsafe_environment_and_executable_path_cannot_borrow_read_only_trust() {
        let fixture = Fixture::new();
        for command in [
            "LD_PRELOAD=./payload.so ls",
            "./ls",
            "git diff --output=leak.txt",
            "git branch feature-created-by-mistake",
            "git stash push list",
            "git worktree add list",
            "git remote -v set-url origin https://example.com/repo.git",
            "git grep --open-files-in-pager=sh needle",
            "git reflog expire --expire=now --all",
            "find *.rs",
            "find . -fprint0 victim.txt",
            "rg --pre 'sh -c evil' needle .",
            "printf '\\057etc\\057passwd\\n' | xargs cat",
            "printf '\\057etc\\057passwd\\0' | wc --files0-from=-",
            "grep -f/etc/passwd needle inside.txt",
            "file -m/etc/passwd inside.txt",
            "file -C -mmagic inside.txt",
            "cat ~root/.ssh/id_rsa",
            "cat */secret",
        ] {
            let decision = fixture
                .classify(
                    SecurityLevel::RequestApproval,
                    "bash",
                    json!({"command":command}),
                )
                .unwrap();
            assert!(decision.requires_approval, "{command}");
            assert_ne!(decision.risk_level, RiskLevel::Low, "{command}");
        }

        // An assignment that cannot change what runs leaves the proof alone.
        let safe_env = fixture.shell(
            SecurityLevel::RequestApproval,
            "bash",
            "NO_COLOR=1 git status --short",
        );
        assert!(!safe_env.requires_approval);
        assert_eq!(safe_env.risk_level, RiskLevel::Low);
    }

    #[test]
    fn powershell_allow_edits_cannot_delete_trees_or_application_control_data() {
        let fixture = Fixture::new();
        let commands = vec![
            "Remove-Item -Recurse .".to_owned(),
            "ri -Recurse -Force C:\\".to_owned(),
            "Remove-Item -Re`curse C:\\".to_owned(),
            "Remove-Item -Recurse \"$env:SystemDrive\\\"".to_owned(),
            "Remove-Item .".to_owned(),
            "Remove-Item HKCU:\\Software\\Mewrk".to_owned(),
            "Get-ItemProperty HKLM:\\Software".to_owned(),
            "Get-Content */secret".to_owned(),
            "Write-Output (Start-Process notepad)".to_owned(),
            "Write-Output (Remove-Item inside.txt)".to_owned(),
            "Set-Content \"inside.txt\", \"C:\\outside.txt\" -Value pwn".to_owned(),
            format!(
                "Set-Content -Path \"{}\" -Value tamper",
                fixture.app_data.join("document.v1.json").display()
            ),
        ];
        for command in commands {
            let decision = fixture
                .classify(
                    SecurityLevel::AllowEdits,
                    "powershell",
                    json!({"command":&command}),
                )
                .unwrap();
            assert!(decision.requires_approval, "{command}");
            assert_ne!(decision.risk_level, RiskLevel::Low, "{command}");
        }
    }

    #[test]
    fn full_access_allows_valid_outside_paths_but_not_invalid_requests() {
        let fixture = Fixture::new();
        let outside = fixture
            .classify(
                SecurityLevel::FullAccess,
                "read",
                json!({"path":fixture.outside.join("outside.txt")}),
            )
            .unwrap();
        assert!(!outside.requires_approval);
        assert!(is_unrestricted(&outside));

        assert!(fixture
            .classify(SecurityLevel::FullAccess, "read", json!({}))
            .is_err());
        assert!(fixture
            .classify(
                SecurityLevel::FullAccess,
                "write",
                json!({"path":42,"content":"invalid"})
            )
            .is_err());
        assert!(fixture
            .classify(SecurityLevel::FullAccess, "bash", json!({"command":""}))
            .is_err());
        assert!(fixture
            .classify(SecurityLevel::FullAccess, "unknown", json!({}))
            .is_err());
    }

    #[test]
    fn optional_read_roots_default_to_workspace() {
        let fixture = Fixture::new();
        for tool in ["ls", "grep", "find"] {
            let decision = fixture
                .classify(SecurityLevel::RequestApproval, tool, json!({}))
                .unwrap();
            assert!(!decision.requires_approval);
            assert!(is_restricted(&decision));
        }
    }

    /// Fifteen tools, one policy each. `dangerous` in the catalog is only the review marker the
    /// settings page draws, so the approval line is pinned here explicitly.
    #[test]
    fn every_preview_tool_has_a_matching_security_policy() {
        let fixture = Fixture::new();
        let preview_tools = crate::catalog::tool_catalog()
            .into_iter()
            .filter(|tool| tool.name.starts_with("preview_"))
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(preview_tools.len(), 15);
        assert!(!crate::catalog::tool_catalog()
            .into_iter()
            .any(|tool| tool.name == "playwright"));

        // Observations of host state never prompt; everything that runs a process, drives a page,
        // or lifts page content into the context does.
        let free = [
            "preview_list",
            "preview_logs",
            "preview_snapshot",
            "preview_inspect",
            "preview_resize",
        ];
        for name in &preview_tools {
            let input = preview_input(name);
            // `preview_upload_image` exists only inside a run, so the manual path must refuse it
            // while the run-loop entry point still classifies it.
            if name == "preview_upload_image" {
                let manual = fixture
                    .classify(SecurityLevel::FullAccess, name, input.clone())
                    .unwrap_err();
                assert!(
                    manual.contains("requires the model run loop"),
                    "manual execution must be refused: {manual}"
                );
            }

            let guarded = fixture
                .classify_model_call(SecurityLevel::RequestApproval, name, input.clone())
                .unwrap_or_else(|error| panic!("{name} has no security policy: {error}"));
            assert_eq!(
                guarded.requires_approval,
                !free.contains(&name.as_str()),
                "{name} approval line changed"
            );

            let full = fixture
                .classify_model_call(SecurityLevel::FullAccess, name, input)
                .unwrap_or_else(|error| panic!("{name} rejects full-access execution: {error}"));
            assert!(
                !full.requires_approval,
                "{name} should execute without approval in full-access mode"
            );
        }
    }

    /// Page content reaching the context is its own boundary: console lines, network rows and
    /// pixels can all carry whatever the signed-in page is showing.
    #[test]
    fn browser_observability_and_pixels_are_sensitive() {
        let fixture = Fixture::new();
        for tool in [
            "preview_console_logs",
            "preview_network",
            "preview_screenshot",
        ] {
            let guarded = fixture
                .classify(SecurityLevel::AllowEdits, tool, json!({}))
                .unwrap();
            assert!(guarded.requires_approval, "{tool} must require approval");
            assert_eq!(guarded.effect, OperationEffect::Unbounded);
            assert_eq!(guarded.rule_id, "browser.sensitive_observation");
            assert!(!guarded.reason.text(ResolvedLanguage::EnUs).is_empty());

            let full = fixture
                .classify(SecurityLevel::FullAccess, tool, json!({}))
                .unwrap();
            assert!(!full.requires_approval);
        }
    }

    /// Reading the registry or the page's structure stays free; driving the page or a process
    /// does not, and a malformed call is refused before any of that is decided.
    #[test]
    fn preview_observations_and_interactions_have_distinct_policies() {
        let fixture = Fixture::new();
        for tool in [
            "preview_list",
            "preview_logs",
            "preview_snapshot",
            "preview_resize",
        ] {
            let decision = fixture
                .classify(SecurityLevel::RequestApproval, tool, json!({}))
                .unwrap();
            assert!(
                !decision.requires_approval,
                "{tool} should be local/read-only"
            );
            assert!(is_restricted(&decision));
        }
        for (tool, input) in [
            ("preview_start", json!({"name":"dev"})),
            ("preview_stop", json!({"serverId":"preview-1"})),
            ("preview_click", json!({"selector":"button"})),
            ("preview_click", json!({"uid":12})),
            ("preview_fill", json!({"selector":"input","value":""})),
            ("preview_fill", json!({"uid":"[7]","value":"x"})),
            ("preview_eval", json!({"expression":"document.title"})),
            ("preview_dialog", json!({})),
        ] {
            let decision = fixture
                .classify(SecurityLevel::RequestApproval, tool, input.clone())
                .unwrap();
            assert!(
                decision.requires_approval,
                "{tool} drives a page or a process"
            );
            assert_eq!(decision.effect, OperationEffect::Unbounded);
            let full = fixture
                .classify(SecurityLevel::FullAccess, tool, input)
                .unwrap();
            assert!(!full.requires_approval, "{tool}");
        }
    }

    /// A call missing the argument its policy is decided from must be refused rather than falling
    /// into the most permissive branch.
    #[test]
    fn preview_tools_require_their_policy_bearing_arguments() {
        let fixture = Fixture::new();
        for (tool, input) in [
            ("preview_start", json!({})),
            ("preview_start", json!({"name": 3})),
            ("preview_stop", json!({})),
            ("preview_eval", json!({})),
            ("preview_click", json!({})),
            ("preview_click", json!({"uid": "button"})),
            ("preview_fill", json!({"selector":"input"})),
            ("preview_fill", json!({"uid": 0, "value": ""})),
            ("preview_fill", json!({"selector":"input","value":7})),
            ("preview_inspect", json!({})),
        ] {
            assert!(
                fixture
                    .classify_model_call(SecurityLevel::FullAccess, tool, input.clone())
                    .is_err(),
                "{tool} {input} must be refused by the classifier"
            );
        }
        // A retired action name is an unknown tool, not a permissive fallthrough.
        fixture
            .classify_model_call(
                SecurityLevel::FullAccess,
                "playwright",
                json!({"action":"click"}),
            )
            .expect_err("the multiplexed browser tool is retired");
    }

    /// The one argument shape every preview tool's classifier insists on.
    fn preview_input(name: &str) -> Value {
        match name {
            "preview_start" => json!({"name":"dev"}),
            "preview_stop" => json!({"serverId":"preview-1"}),
            "preview_inspect" | "preview_click" => json!({"selector":"button"}),
            "preview_fill" => json!({"selector":"input","value":"x"}),
            "preview_eval" => json!({"expression":"document.title"}),
            "preview_upload_image" => json!({"image_id":"1"}),
            _ => json!({}),
        }
    }

    #[test]
    fn global_memory_mutations_require_a_native_confirmation_in_every_mode() {
        let fixture = Fixture::new();
        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            for tool in ["create_global_memory", "edit_global_memory"] {
                let decision = fixture
                    .classify_model_call(level, tool, json!({"name": "preferences"}))
                    .unwrap();
                assert!(decision.requires_approval, "{level:?} {tool}");
                assert!(decision.mandatory_prompt, "{level:?} {tool}");
                assert_eq!(
                    decision.rule_id, "memory.global_persistent_mutation",
                    "{level:?} {tool}"
                );
                assert_eq!(decision.risk_level, RiskLevel::High);
                assert!(decision
                    .reason
                    .text(ResolvedLanguage::ZhCn)
                    .contains("所有项目"));
            }

            // The project tier is a local state change, not a mandatory prompt.
            for tool in ["create_project_memory", "edit_project_memory"] {
                let decision = fixture
                    .classify_model_call(level, tool, json!({"name": "build"}))
                    .unwrap();
                assert!(!decision.mandatory_prompt, "{level:?} {tool}");
                assert_eq!(decision.risk_level, RiskLevel::Medium, "{level:?} {tool}");
            }

            // Reads never prompt in any mode.
            for tool in ["read_global_memory", "read_project_memory"] {
                let decision = fixture
                    .classify_model_call(level, tool, json!({"name": "build"}))
                    .unwrap();
                assert_eq!(decision.risk_level, RiskLevel::Low, "{level:?} {tool}");
                assert_eq!(decision.effect, OperationEffect::Read, "{level:?} {tool}");
            }
        }

        // The tier now comes from the tool name, so a `scope` argument is inert
        // rather than a way to talk a project write into the global tier.
        for scope in [json!("global"), json!("other"), json!(42)] {
            let decision = fixture
                .classify_model_call(
                    SecurityLevel::FullAccess,
                    "create_project_memory",
                    json!({"name": "build", "scope": scope}),
                )
                .unwrap();
            assert!(!decision.mandatory_prompt);
            assert_eq!(decision.risk_level, RiskLevel::Medium);
        }
    }

    #[test]
    fn web_search_has_one_outer_permission_boundary() {
        let fixture = Fixture::new();
        let query = json!({ "query": "Anthropic Claude 4.5 release date" });
        for level in [SecurityLevel::RequestApproval, SecurityLevel::AllowEdits] {
            let decision = fixture
                .classify_model_call(level, "web_search", query.clone())
                .unwrap();
            assert!(decision.requires_approval, "{level:?}");
            assert_eq!(decision.rule_id, "web.search");
        }
        let full = fixture
            .classify_model_call(SecurityLevel::FullAccess, "web_search", query)
            .unwrap();
        assert!(!full.requires_approval);
        assert_eq!(full.rule_id, "web.search");

        // Tool name is the only routing criterion. Legacy delegation fields are unknown
        // parameters, not an alternate security tier.
        let error = fixture
            .classify_model_call(
                SecurityLevel::FullAccess,
                "web_search",
                json!({ "objective": "compare the primary sources" }),
            )
            .expect_err("objective 形状不再被 web_search 入口接受");
        assert!(error.contains("query"), "报错必须指向缺失的 query：{error}");

        // Retired tool names must remain unknown rather than bypass approval.
        fixture
            .classify_model_call(
                SecurityLevel::FullAccess,
                "web_query",
                json!({ "query": "primary source" }),
            )
            .expect_err("被删除的原生腿必须是未知工具，而不是一条免提示通道");
    }

    /// `web_fetch` and `web_search` share one boundary: one authorization covers every
    /// URL in the call, so both must receive the same high-risk classification.
    #[test]
    fn web_fetch_shares_the_same_outer_boundary_and_bounds_its_url_list() {
        let fixture = Fixture::new();
        let urls = json!({ "urls": ["https://example.com/a", "https://example.com/b"] });
        for level in [SecurityLevel::RequestApproval, SecurityLevel::AllowEdits] {
            let decision = fixture
                .classify_model_call(level, "web_fetch", urls.clone())
                .unwrap();
            assert!(decision.requires_approval, "{level:?}");
            assert_eq!(decision.rule_id, "web.fetch");
            assert_eq!(decision.risk_level, RiskLevel::High);
        }
        let full = fixture
            .classify_model_call(SecurityLevel::FullAccess, "web_fetch", urls)
            .unwrap();
        assert!(!full.requires_approval);

        // Validate shape boundaries before authorization; empty or oversized URL lists
        // must fail without showing an approval prompt.
        // A blank entry beside a real one is skipped, not refused.
        fixture
            .classify_model_call(
                SecurityLevel::FullAccess,
                "web_fetch",
                json!({ "urls": ["https://example.com", " "] }),
            )
            .unwrap();
        for refused in [
            json!({}),
            json!({ "urls": [] }),
            json!({ "urls": [5] }),
            json!({ "urls": ["  "] }),
        ] {
            fixture
                .classify_model_call(SecurityLevel::FullAccess, "web_fetch", refused.clone())
                .expect_err("{refused} 必须在分类阶段就被拒绝");
        }
        let too_many = json!({
            "urls": (0..=crate::model::MAX_SEARCH_INPUTS)
                .map(|index| format!("https://example.com/{index}"))
                .collect::<Vec<_>>()
        });
        fixture
            .classify_model_call(SecurityLevel::FullAccess, "web_fetch", too_many)
            .expect_err("超过一次调用的地址上限必须被拒绝");
    }

    #[test]
    fn workflow_has_one_outer_permission_boundary_that_full_access_clears() {
        let fixture = Fixture::new();
        let input = json!({"script": "export const meta = { name: \"audit\", description: \"d\" }\nreturn 1"});
        for level in [SecurityLevel::RequestApproval, SecurityLevel::AllowEdits] {
            let decision = fixture
                .classify_model_call(level, "workflow", input.clone())
                .unwrap();
            assert!(decision.requires_approval, "{level:?}");
            assert_eq!(decision.rule_id, "workflow.orchestrated_fan_out");
        }

        // Full access means every model operation runs unattended. Fan-out is
        // broad, but each step re-enters this classifier at the same level, so
        // the level and not the tool decides — hence no mandatory prompt.
        let full = fixture
            .classify_model_call(SecurityLevel::FullAccess, "workflow", input)
            .unwrap();
        assert!(!full.requires_approval);
        assert!(!full.mandatory_prompt);
        assert_eq!(full.rule_id, "workflow.orchestrated_fan_out");
    }

    /// `todo` is retired, but a legacy timeline card can still carry its name,
    /// so manual execution and approval keep refusing it rather than treating
    /// it as an ordinary tool.
    #[test]
    fn the_retired_task_tool_is_rejected_before_manual_execution_or_approval() {
        let fixture = Fixture::new();
        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            for (tool, action) in [
                ("todo", "create"),
                ("todo", "update"),
                ("todo", "get"),
                ("todo", "list"),
            ] {
                let error = fixture
                    .classify(level, tool, json!({"action":action}))
                    .unwrap_err();
                assert!(
                    error.contains("may only be scheduled by the model run loop"),
                    "{tool} {action}: {error}"
                );
                assert!(
                    error.contains("cannot be manually executed or approved separately"),
                    "{tool} {action}: {error}"
                );
            }
        }
    }

    /// One tool, two actions: the read/write split of `plan` cannot be read off
    /// the tool name, so the model-run classifier reads `action`. Reads must
    /// stay Low and writes Medium — and an action the classifier cannot read
    /// must fail toward the stricter tier, because the alternative is a
    /// mutation slipping through as a read.
    #[test]
    fn the_plan_tool_classifies_reads_and_writes_by_action() {
        let fixture = Fixture::new();
        for level in [
            SecurityLevel::RequestApproval,
            SecurityLevel::AllowEdits,
            SecurityLevel::FullAccess,
        ] {
            let decision = fixture
                .classify_model_call(level, "plan", json!({"action": "read"}))
                .unwrap();
            assert_eq!(decision.rule_id, "host.read_or_coordinate");
            assert_eq!(decision.risk_level, RiskLevel::Low);
            assert!(!decision.requires_approval);

            for input in [
                json!({"action":"write","content":"# Plan"}),
                // Unreadable or unknown discriminators fail toward the write tier.
                json!({}),
                json!({"action":"delete"}),
                json!({"action":123}),
            ] {
                let decision = fixture
                    .classify_model_call(level, "plan", input.clone())
                    .unwrap();
                assert_eq!(decision.rule_id, "host.local_state_change", "{input}");
                assert_eq!(decision.risk_level, RiskLevel::Medium, "{input}");
                assert!(!decision.requires_approval, "{input}");
            }
        }
    }

    /// The numbered set of a conversation whose workspace 1 is on this machine
    /// and workspace 2 is `/home/dev/app` on an SSH machine.
    fn remote_workspaces(fixture: &Fixture) -> crate::workspace_set::WorkspaceSet {
        let assets = crate::model::ExecutionEnvironmentAssets {
            ssh_machines: vec![crate::model::SshMachineConfig {
                id: "m1".into(),
                name: "devbox".into(),
                host: "user@devbox".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::workspace_set::WorkspaceSet::resolve(
            &assets,
            &crate::model::AttachedWorkspace {
                machine: None,
                path: fixture.workspace.to_string_lossy().into_owned(),
            },
            &[crate::model::AttachedWorkspace {
                machine: Some(crate::model::RunTarget::Ssh {
                    machine_id: "m1".into(),
                }),
                path: "/home/dev/app".into(),
            }],
        )
        .unwrap()
    }

    fn classify_remote(
        fixture: &Fixture,
        level: SecurityLevel,
        tool_name: &str,
        input: Value,
    ) -> Result<SecurityDecision, String> {
        classify_model_call_in_workspaces(
            level,
            &remote_workspaces(fixture),
            &fixture.workspace,
            &fixture.app_data,
            &fixture.additional,
            &request(&fixture.workspace, tool_name, input),
        )
    }

    /// A file the conversation's instruction files import is one of its
    /// workspaces' files to the security level, whichever workspace's
    /// instructions named it: read without a card at Manual, edited without
    /// one at Accept edits — that file alone, not the folder it is in.
    #[test]
    fn an_imported_file_is_judged_as_a_workspace_file() {
        let fixture = Fixture::new();
        let imported = fs::canonicalize(fixture.outside.join("outside.txt")).unwrap();
        let beside = fixture.outside.join("beside.txt");
        fs::write(&beside, "beside").unwrap();
        let workspaces = remote_workspaces(&fixture);
        let judge = |level: SecurityLevel, tool: &str, input: Value| {
            classify_model_call_in_workspaces(
                level,
                &workspaces,
                &fixture.workspace,
                &fixture.app_data,
                &fixture.additional,
                &request(&fixture.workspace, tool, input),
            )
            .unwrap()
        };
        let read = json!({"path": imported});
        assert!(judge(SecurityLevel::RequestApproval, "read", read.clone()).requires_approval);

        workspaces.imports().replace(crate::workspace_set::ImportedFiles {
            local: [imported.clone()].into(),
            ..Default::default()
        });
        let decision = judge(SecurityLevel::RequestApproval, "read", read.clone());
        assert!(!decision.requires_approval);
        assert_eq!(decision.rule_id, "filesystem.trusted_read");
        let edit = json!({"path": imported, "find": "outside", "replace": "edited"});
        let decision = judge(SecurityLevel::AllowEdits, "edit", edit.clone());
        assert!(!decision.requires_approval);
        assert_eq!(decision.rule_id, "filesystem.trusted_allow_edits");
        // The scope the call runs under admits the file, and nothing beside it.
        let path = imported.to_str().unwrap();
        assert!(resolve_existing_with_scope(&fixture.workspace, path, &decision.scope).is_ok());
        let beside_path = beside.to_str().unwrap();
        assert!(
            resolve_existing_with_scope(&fixture.workspace, beside_path, &decision.scope).is_err()
        );
        assert!(judge(SecurityLevel::RequestApproval, "read", json!({"path": beside})).requires_approval);
        // Manual asks before a write to it, as it does before any workspace write.
        let decision = judge(SecurityLevel::RequestApproval, "edit", edit);
        assert_eq!(decision.rule_id, "filesystem.trusted_write_manual");
        // A shell reading it is judged against the same roots. The shell is
        // handed the path as a person would spell it: without the verbatim
        // prefix `canonicalize` adds on Windows, and quoted.
        let spelled = path.strip_prefix(r"\\?\").unwrap_or(path);
        for level in [SecurityLevel::RequestApproval, SecurityLevel::FullAccess] {
            let shell = classify_in_workspaces(
                level,
                &workspaces,
                &fixture.workspace,
                &fixture.app_data,
                &fixture.additional,
                &request(&fixture.workspace, "bash", json!({"command": format!("cat '{spelled}'")})),
            )
            .unwrap();
            assert_eq!(shell.rule_id, "shell.read_only");
            assert!(!shell.requires_approval, "{level:?}");
        }
    }

    /// On another machine the imported file is named by the machine's own
    /// canonical path for it; the host compares spellings it can place, and
    /// the remote leg checks the canonical target.
    #[test]
    fn an_imported_file_on_another_machine_is_judged_as_a_workspace_file() {
        let fixture = Fixture::new();
        let workspaces = remote_workspaces(&fixture);
        let machine = crate::run_environment::env_key(workspaces.get(2).unwrap().machine.as_ref());
        workspaces.imports().replace(crate::workspace_set::ImportedFiles {
            remote: [(machine, ["/home/dev/notes/style.md".to_owned()].into())].into(),
            ..Default::default()
        });
        let judge = |path: &str, workspace: u32| {
            classify_model_call_in_workspaces(
                SecurityLevel::RequestApproval,
                &workspaces,
                &fixture.workspace,
                &fixture.app_data,
                &fixture.additional,
                &request(&fixture.workspace, "read", json!({"path": path, "workspace": workspace})),
            )
            .unwrap()
        };
        for path in ["/home/dev/notes/style.md", "/home/dev/app/../notes/style.md"] {
            let decision = judge(path, 2);
            assert!(!decision.requires_approval, "{path}");
            assert_eq!(decision.scope, ExecutionScope::Restricted { roots: Vec::new() });
        }
        // A spelling the host cannot place, and the file beside it, are outside.
        for path in ["~/notes/style.md", "../notes/style.md", "/home/dev/notes/other.md"] {
            assert!(judge(path, 2).requires_approval, "{path}");
        }
        // The same path on this computer is not that machine's file.
        assert!(judge("/home/dev/notes/style.md", 1).requires_approval);
    }

    /// Output a tool saved for this conversation is a file in the app data
    /// directory on this host, whichever workspace the call that reads it
    /// back names — the executor serves it here, so it is judged here.
    #[test]
    fn saved_tool_output_is_judged_as_the_host_file_it_is() {
        let fixture = Fixture::new();
        let profile = crate::prompt_profile::PromptProfile::builtin();
        let spill = |conversation: &'static str| {
            let notice = crate::tool_output::fit(
                "output\n".repeat(100),
                10,
                crate::tool_output::Spill::new(Some(&fixture.app_data), conversation, "bash-1"),
                &profile,
            );
            crate::tool_output::saved_path(&notice).expect("saved")
        };
        let own = spill("conversation-test");
        for tool in ["read", "grep"] {
            let input = if tool == "read" {
                json!({"path": own, "workspace": 2})
            } else {
                json!({"path": own, "pattern": "output", "workspace": 2})
            };
            let decision =
                classify_remote(&fixture, SecurityLevel::RequestApproval, tool, input).unwrap();
            assert!(!decision.requires_approval, "{tool}");
            assert_eq!(decision.rule_id, "filesystem.trusted_read", "{tool}");
        }
        // Another conversation's file is nothing of this one's: the call is
        // judged as a path on the remote machine, outside its root.
        let other = spill("another-conversation");
        let decision = classify_remote(
            &fixture,
            SecurityLevel::RequestApproval,
            "read",
            json!({"path": other, "workspace": 2}),
        )
        .unwrap();
        assert!(decision.requires_approval);
        assert_eq!(decision.rule_id, "filesystem.outside_trusted_roots");
    }

    #[test]
    fn a_remote_workspace_is_judged_by_its_own_root_not_the_local_guard() {
        let fixture = Fixture::new();
        // Inside the remote root: trusted, confined, no approval for a read.
        let decision = classify_remote(
            &fixture,
            SecurityLevel::RequestApproval,
            "read",
            json!({"path": "src/main.rs", "workspace": 2}),
        )
        .unwrap();
        assert!(!decision.requires_approval);
        assert_eq!(decision.rule_id, "filesystem.trusted_read");
        assert_eq!(
            decision.scope,
            ExecutionScope::Restricted { roots: Vec::new() },
            "a confined remote call carries the empty-roots marker"
        );
        assert_eq!(
            decision.target,
            Some(PathBuf::from("/home/dev/app/src/main.rs")),
            "the card names where on that machine the call lands"
        );

        // Absolute under the root is inside too; climbing out is not.
        for (path, inside) in [
            ("/home/dev/app/x", true),
            ("/home/dev/app", true),
            ("./a/../b", true),
            ("../secrets", false),
            ("/etc/passwd", false),
            ("/home/dev/app2/x", false),
            ("~/app/x", false),
            ("a/../../x", false),
        ] {
            let decision = classify_remote(
                &fixture,
                SecurityLevel::RequestApproval,
                "read",
                json!({"path": path, "workspace": 2}),
            )
            .unwrap();
            assert_eq!(
                decision.requires_approval, !inside,
                "{path} inside={inside}: {}",
                decision.rule_id
            );
            if !inside {
                assert_eq!(decision.scope, ExecutionScope::Unrestricted, "{path}");
            }
        }
    }

    #[test]
    fn remote_writes_follow_the_same_level_matrix_as_local_ones() {
        let fixture = Fixture::new();
        let write = json!({"path": "notes.md", "content": "x", "workspace": 2});
        let manual = classify_remote(&fixture, SecurityLevel::RequestApproval, "write", write.clone())
            .unwrap();
        assert!(manual.requires_approval);
        assert_eq!(manual.rule_id, "filesystem.trusted_write_manual");
        let allowed = classify_remote(&fixture, SecurityLevel::AllowEdits, "write", write.clone())
            .unwrap();
        assert!(!allowed.requires_approval);
        assert_eq!(
            allowed.scope,
            ExecutionScope::Restricted { roots: Vec::new() }
        );
        let full = classify_remote(&fixture, SecurityLevel::FullAccess, "write", write).unwrap();
        assert!(!full.requires_approval);
        assert_eq!(full.scope, ExecutionScope::Unrestricted);
    }

    #[test]
    fn workspace_one_and_non_filesystem_tools_keep_the_local_rule() {
        let fixture = Fixture::new();
        let local = classify_remote(
            &fixture,
            SecurityLevel::RequestApproval,
            "read",
            json!({"path": "inside.txt", "workspace": 1}),
        )
        .unwrap();
        assert_eq!(
            local,
            fixture
                .classify_model_call(
                    SecurityLevel::RequestApproval,
                    "read",
                    json!({"path": "inside.txt", "workspace": 1})
                )
                .unwrap()
        );
        // An out-of-range address is refused before any machine is consulted.
        let error = classify_remote(
            &fixture,
            SecurityLevel::RequestApproval,
            "read",
            json!({"path": "inside.txt", "workspace": 9}),
        )
        .unwrap_err();
        assert!(error.contains("no workspace 9"), "{error}");
        // A shell call is never routed through the remote filesystem rule.
        let shell = classify_remote(
            &fixture,
            SecurityLevel::RequestApproval,
            "bash",
            json!({"command": "ls", "workspace": 2}),
        )
        .unwrap();
        assert!(shell.requires_approval);
        assert!(shell.rule_id.starts_with("shell."), "{}", shell.rule_id);
    }

    /// `lsp` on a remote workspace follows the host leg's rule: a read inside
    /// the workspace when the project ships no configuration, the unbounded
    /// card when it does — because that configuration names the command the
    /// language server is started with on that machine.
    #[test]
    fn a_remote_lsp_call_raises_the_unbounded_card_only_for_a_project_configuration() {
        let fixture = Fixture::new();
        let workspaces = remote_workspaces(&fixture);
        let call = |declares: Option<bool>, level: SecurityLevel| {
            classify_remote_filesystem_call_with(
                level,
                &workspaces,
                &request(
                    &fixture.workspace,
                    "lsp",
                    json!({
                        "operation": "hover",
                        "filePath": "src/main.rs",
                        "line": 1,
                        "character": 1,
                        "workspace": 2
                    }),
                ),
                &|_| declares,
            )
            .unwrap()
            .expect("a remote workspace is classified remotely")
        };
        let plain = call(Some(false), SecurityLevel::RequestApproval);
        assert!(!plain.requires_approval);
        assert_eq!(plain.rule_id, "filesystem.trusted_read");
        assert_eq!(plain.scope, ExecutionScope::Restricted { roots: Vec::new() });

        let declared = call(Some(true), SecurityLevel::RequestApproval);
        assert!(declared.requires_approval);
        assert_eq!(declared.rule_id, "tool.unbounded");
        assert_eq!(declared.scope, ExecutionScope::Unrestricted);

        // A machine that cannot be asked is treated as declaring one: the
        // card is the safe default, and the call fails on the same transport
        // anyway.
        let unreachable = call(None, SecurityLevel::RequestApproval);
        assert!(unreachable.requires_approval);
        assert_eq!(unreachable.rule_id, "tool.unbounded");

        // A file outside the workspace raises the same card, not the plain
        // out-of-workspace read: the executor reads an unrestricted scope as
        // "approved to start whatever the project configured".
        let outside = classify_remote_filesystem_call_with(
            SecurityLevel::AllowEdits,
            &workspaces,
            &request(
                &fixture.workspace,
                "lsp",
                json!({
                    "operation": "hover",
                    "filePath": "../sibling/x.rs",
                    "line": 1,
                    "character": 1,
                    "workspace": 2
                }),
            ),
            &|_| panic!("the machine is not consulted for a path the card covers anyway"),
        )
        .unwrap()
        .unwrap();
        assert!(outside.requires_approval);
        assert_eq!(outside.rule_id, "tool.unbounded");

        // Full access never asks, so the machine is not consulted at all.
        let asked = std::cell::Cell::new(false);
        let full = classify_remote_filesystem_call_with(
            SecurityLevel::FullAccess,
            &workspaces,
            &request(
                &fixture.workspace,
                "lsp",
                json!({
                    "operation": "hover",
                    "filePath": "src/main.rs",
                    "line": 1,
                    "character": 1,
                    "workspace": 2
                }),
            ),
            &|_| {
                asked.set(true);
                Some(true)
            },
        )
        .unwrap()
        .unwrap();
        assert!(!full.requires_approval);
        assert_eq!(full.scope, ExecutionScope::Unrestricted);
        assert!(!asked.get(), "full access does not pay a round trip for an answer it ignores");

        // A local secondary workspace's `lsp` is judged against that workspace,
        // not the primary root, so it is its own `.mewrk/lsp.json` that
        // decides on the card.
        let local_second = std::env::temp_dir().join(format!(
            "mewrk-lsp-second-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(local_second.join(".mewrk")).unwrap();
        std::fs::write(local_second.join(".mewrk").join("lsp.json"), "{}").unwrap();
        std::fs::write(local_second.join("x.rs"), "fn x() {}").unwrap();
        let two_local = crate::workspace_set::WorkspaceSet::resolve(
            &crate::model::ExecutionEnvironmentAssets::default(),
            &crate::model::AttachedWorkspace {
                machine: None,
                path: fixture.workspace.to_string_lossy().into_owned(),
            },
            &[crate::model::AttachedWorkspace {
                machine: None,
                path: local_second.to_string_lossy().into_owned(),
            }],
        )
        .unwrap();
        let additional = two_local.local_roots();
        let judged = classify_model_call_in_workspaces(
            SecurityLevel::RequestApproval,
            &two_local,
            &fixture.workspace,
            &fixture.app_data,
            &additional,
            &request(
                &fixture.workspace,
                "lsp",
                json!({
                    "operation": "hover",
                    "filePath": "x.rs",
                    "line": 1,
                    "character": 1,
                    "workspace": 2
                }),
            ),
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(&local_second);
        assert_eq!(
            judged.rule_id, "tool.unbounded",
            "the second workspace's own configuration raises the card"
        );

        // A missing operation is malformed before any machine is consulted.
        assert!(classify_remote_filesystem_call_with(
            SecurityLevel::RequestApproval,
            &workspaces,
            &request(
                &fixture.workspace,
                "lsp",
                json!({"filePath": "src/main.rs", "line": 1, "character": 1, "workspace": 2}),
            ),
            &|_| panic!("not consulted for a malformed call"),
        )
        .is_err());
    }

    #[test]
    fn remote_paths_are_normalized_lexically() {
        assert_eq!(normalize_remote_path("/a/./b/../c"), Some("/a/c".into()));
        assert_eq!(normalize_remote_path("~/x/../y"), Some("~/y".into()));
        assert_eq!(normalize_remote_path("~/.."), None);
        assert_eq!(normalize_remote_path("/.."), None);
        assert_eq!(normalize_remote_path("a//b/"), Some("a/b".into()));
        assert!(remote_path_is_inside_root("~/app", "~/app/src"));
        assert!(remote_path_is_inside_root("~/app", "src"));
        assert!(!remote_path_is_inside_root("~/app", "/home/dev/app/src"));
        assert!(!remote_path_is_inside_root("/home/dev/app", "~/app/src"));
        assert!(remote_path_is_inside_root("/home/dev/app/", "/home/dev/app"));
    }

    /// Which levels run a shell line without asking.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Clears {
        /// Proven read-only inside the trusted roots: no level asks.
        Always,
        /// Proven to read beyond the trusted roots, or to change only
        /// workspace files: Manual asks, Accept edits does not.
        FromAcceptEdits,
        /// Not proven: only full access runs it unasked.
        FullAccessOnly,
    }

    const LEVELS: [SecurityLevel; 3] = [
        SecurityLevel::RequestApproval,
        SecurityLevel::AllowEdits,
        SecurityLevel::FullAccess,
    ];

    fn assert_clears_from(
        fixture: &Fixture,
        site: &ShellSite,
        tool: &str,
        command: &str,
        expected: Clears,
    ) {
        for level in LEVELS {
            let decision = fixture.shell_from(level, tool, command, site);
            let asks = match (expected, level) {
                (_, SecurityLevel::FullAccess) | (Clears::Always, _) => false,
                (Clears::FromAcceptEdits, SecurityLevel::AllowEdits) => false,
                _ => true,
            };
            assert_eq!(
                decision.requires_approval, asks,
                "{tool} at {level:?}: {command:?} ({}: {})",
                decision.rule_id,
                decision.reason.text(ResolvedLanguage::EnUs)
            );
            assert!(!decision.mandatory_prompt, "{tool}: {command:?}");
            assert_eq!(decision.scope, ExecutionScope::Unrestricted);
        }
    }

    fn assert_clears(fixture: &Fixture, tool: &str, command: &str, expected: Clears) {
        let start = ShellSite::Local {
            cwd: Some(fixture.workspace.clone()),
        };
        assert_clears_from(fixture, &start, tool, command, expected);
    }

    /// A workspace with a subdirectory and a few files to name.
    fn shell_fixture() -> Fixture {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.workspace.join("sub/deep")).unwrap();
        for file in ["sub/x", "sub/a", "a", "b", "f", "in"] {
            fs::write(fixture.workspace.join(file), "needle\n").unwrap();
        }
        fixture
    }

    /// A path as a POSIX shell on this machine would be handed it.
    fn posix_quoted(path: &Path) -> String {
        format!("'{}'", path.display())
    }

    #[test]
    fn a_proven_read_only_command_asks_at_no_level() {
        let fixture = shell_fixture();
        for command in [
            "ls",
            "ls -la sub",
            "cat inside.txt",
            "head -n 5 inside.txt",
            "tail -20 inside.txt",
            "wc -l inside.txt",
            "grep -rn needle sub",
            "grep -rn needle sub --include='*.txt'",
            "grep --max-count 1 needle inside.txt",
            "ls sub -la",
            "head f -n 5",
            "rg needle sub -n",
            "echo hello",
            "echo 'a; rm -rf x'",
            "pwd",
            "true",
            "git status --short",
            "git log --oneline -5",
            "git diff --stat",
            "git diff HEAD~1 -- inside.txt",
            "git log --format='%h %(describe) %s' -n 3",
            "git branch --show-current",
            "git rev-parse --abbrev-ref HEAD",
            "git config --local --get remote.origin.url",
            "ls 2>&1",
            "ls > /dev/null",
            "ls 2>/dev/null",
            "ls &>/dev/null",
            "ls >/dev/null 2>&1",
            "cat inside.txt | head -3 | wc -l",
            "cat inside.txt |\n  wc -l",
            "command ls",
            "command -v git",
            "timeout 5 ls",
            "nice -n 5 ls",
            "time ls",
            "time -p ls",
            "LANG=C ls",
            "NO_COLOR=1 git status --short",
            "sed -n 1p f",
            "sed -n '1,5p' inside.txt",
            "sed 's/a/b/g' inside.txt",
            "find . -name '*.txt' -type f",
            "sort inside.txt | uniq -c",
            "test -f inside.txt",
            "[ -d sub ]",
            "rg needle sub",
            "diff inside.txt sub/x",
            "stat inside.txt",
            "cat /dev/null",
            "ls\ncat inside.txt",
            "ls # a trailing comment",
            "ls \\\n  -la",
            "cd sub",
            "ls && cat inside.txt; echo done || true",
        ] {
            assert_clears(&fixture, "bash", command, Clears::Always);
        }
        for command in [
            "Get-ChildItem",
            "Get-ChildItem -Path sub -Recurse",
            "Get-Content inside.txt",
            "gc inside.txt",
            "Get-Content -Path inside.txt -TotalCount 5",
            "Get-Content inside.txt | Select-String inside",
            "Get-ChildItem | Select-Object -First 1",
            "Get-ChildItem | Sort-Object Name | Format-Table Name",
            "Test-Path inside.txt",
            "Get-Location",
            "Write-Output hello",
            "Get-ChildItem 2>&1",
            "Get-ChildItem > $null",
            "Get-ChildItem 2>$null",
            "Get-Content inside.txt # a comment",
            "Get-Date -Format yyyy",
            "Get-ChildItem; Get-Content inside.txt",
            "Set-Location sub",
        ] {
            assert_clears(&fixture, "powershell", command, Clears::Always);
            assert_clears(&fixture, "pwsh", command, Clears::Always);
        }
        for tool in ["zsh", "sh"] {
            assert_clears(&fixture, tool, "cat inside.txt | wc -l", Clears::Always);
        }
    }

    #[test]
    fn a_proven_workspace_change_asks_only_at_manual() {
        let fixture = shell_fixture();
        let mut commands = vec![
            "touch new.txt",
            "mkdir -p sub/deeper/dir",
            "rm inside.txt",
            "rm -rf sub",
            "rmdir sub/deep",
            "mv inside.txt renamed.txt",
            "cp inside.txt copy.txt",
            "cp inside.txt sub/",
            "cp -r sub copy-of-sub",
            "timeout 5 rm inside.txt",
            "ls; rm -rf sub",
            "ls && touch generated.txt",
        ];
        // BSD sed reads the word after `-i` as a backup suffix, so the line
        // means two programs there and is not proven.
        if !cfg!(target_os = "macos") {
            commands.extend(["sed -i s/a/b/ f", "sed -i.bak 's/a/b/g' inside.txt"]);
        }
        for command in commands {
            assert_clears(&fixture, "bash", command, Clears::FromAcceptEdits);
            let decision = fixture.shell(SecurityLevel::AllowEdits, "bash", command);
            assert_eq!(decision.rule_id, "shell.workspace_file_edit", "{command}");
            assert_eq!(decision.effect, OperationEffect::Write, "{command}");
            assert_eq!(decision.risk_level, RiskLevel::Medium, "{command}");
        }
        for command in [
            "Set-Content -Path generated.txt -Value ok",
            "Add-Content generated.txt more",
            "Clear-Content inside.txt",
            "Remove-Item inside.txt",
        ] {
            assert_clears(&fixture, "powershell", command, Clears::FromAcceptEdits);
            let decision = fixture.shell(SecurityLevel::AllowEdits, "powershell", command);
            assert_eq!(decision.rule_id, "powershell.workspace_file_edit", "{command}");
        }
    }

    #[test]
    fn a_read_beyond_the_trusted_roots_follows_the_read_tools_rule() {
        let fixture = shell_fixture();
        let outside = fixture.outside.join("outside.txt");
        for command in [
            format!("cat {}", posix_quoted(&outside)),
            "cat ../outside/outside.txt".to_owned(),
            "cat ../../x".to_owned(),
            "grep -f ../outside/outside.txt needle inside.txt".to_owned(),
            // A parser that stops at the first operand opens what follows a
            // late `-n` as a file.
            "head f -n ../outside/outside.txt".to_owned(),
            // BSD grep reads the word after `--context` as the pattern, which
            // makes the next one a file.
            "grep --context 2 ../outside/outside.txt".to_owned(),
            "cp ../outside/outside.txt copied.txt".to_owned(),
            "git config --get user.name".to_owned(),
        ] {
            assert_clears(&fixture, "bash", &command, Clears::FromAcceptEdits);
        }
        let read = fixture.shell(
            SecurityLevel::AllowEdits,
            "bash",
            "cat ../outside/outside.txt",
        );
        assert_eq!(read.rule_id, "shell.outside_read_allow_edits");
        assert_eq!(read.risk_level, RiskLevel::Medium);
        assert_eq!(read.effect, OperationEffect::Read);
        let asked = fixture.shell(
            SecurityLevel::RequestApproval,
            "bash",
            "cat ../outside/outside.txt",
        );
        assert_eq!(asked.rule_id, "shell.read_outside_trusted_roots");
        assert_eq!(asked.risk_level, RiskLevel::High);

        assert_clears(
            &fixture,
            "powershell",
            &format!("Get-Content '{}'", outside.display()),
            Clears::FromAcceptEdits,
        );
        // A granted directory is a trusted root, for a read and a write alike.
        let granted = shell_fixture().granting_outside();
        assert_clears(&granted, "bash", "cat ../outside/outside.txt", Clears::Always);
        assert_clears(&granted, "bash", "touch ../outside/new.txt", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "touch ../outside/new.txt", Clears::FullAccessOnly);
    }

    /// Every one of these was, or would have been, read as harmless by an
    /// analyzer whose verdict did not decide anything. Each either runs a
    /// program, writes a file, reaches the network, or reads what nothing
    /// checked.
    #[test]
    fn nothing_unproven_runs_without_asking_below_full_access() {
        let fixture = shell_fixture();
        let long = format!("echo {}", "x".repeat(MAX_SHELL_ANALYSIS_CHARS));
        let mut commands = vec![
            // Redirection and input.
            "cat a > b",
            "ls >> log.txt",
            "ls 2> errors.txt",
            "ls &> all.txt",
            "ls >& all.txt",
            "cat < inside.txt",
            "cat <<EOF\nhi\nEOF",
            "cat <<< hi",
            // Options that write, execute, or follow links.
            "find . -exec rm {} \\;",
            "find . -delete",
            "find . -fprint out",
            "find -L . -name x",
            "find . \\\n-delete",
            "sort -o out in",
            "sort --output=out in",
            "sort --compress-program=sh in",
            "uniq in out",
            "tree -o out",
            "grep -R needle .",
            "du -L",
            "wc --files0-from=list",
            "wc --files0=list",
            "grep --fil=inside.txt x inside.txt",
            "rg --pre cmd pat",
            "rg -z pat",
            "date -s 2020-01-01",
            "date -d yesterday",
            "date +%s -u",
            "grep -d rec needle",
            "grep --directories=recurse needle",
            "file -p f",
            "strings @resp",
            "diff --from-file=sub a",
            // Options after the first operand: one more operand to a parser
            // that stops there.
            "uniq f -c",
            "touch f -d ../outside/new",
            "mkdir d -m ../outside/new",
            "rm sub -rf",
            "sed 'w out' f -e p",
            "sed -n 1p f -n",
            "hostname evil",
            "diff sub sub",
            // Git: options and forms that are not reads.
            "git diff --output=x",
            "git -c core.fsmonitor=evil status",
            "git -C sub status",
            "git --git-dir=x status",
            "git grep -Osh .",
            "git grep --open-files-in-pager=sh .",
            "git grep --open=sh .",
            "git log --show-signature",
            "git log --format=%G?",
            "git log --pretty='%(signature)'",
            "git diff --no-index a b",
            "git diff --ext-diff",
            "git ls-remote origin",
            "git remote show origin",
            "git branch created-by-mistake",
            "git branch -v created-by-mistake",
            "git tag v1",
            "git config user.name evil",
            "git config --unset user.name",
            "git stash",
            "git push",
            // The environment and the program that runs.
            "env X=1 ls",
            "LD_PRELOAD=x ls",
            "PATH=. ls",
            "GIT_DIR=sub git status",
            "IFS=x ls",
            "TZ=:/etc/passwd ls",
            "TZ=/etc/passwd date",
            "LANG=../x ls",
            "./ls",
            "/bin/ls",
            "LS",
            "printenv",
            "command time --output=pwned ls",
            "time -o out ls",
            "nohup time --output=pwned ls",
            "printf -v PATH . ; ls",
            "printf '%n' PATH",
            "printf 'x%hn' PATH",
            "printf '%ln' PATH",
            "test -v 'a[x=1]'",
            "[ -v PATH ]",
            "test -R x",
            "nohup ls",
            // Expansion, substitution and grouping.
            "echo $(rm x)",
            "echo `rm x`",
            "echo \"$HOME\"",
            "echo $HOME",
            // Literal to the shell, and refused all the same: some commands
            // evaluate their argument again.
            "grep -n 'a$' inside.txt",
            "test -v 'a[$(touch pwn)]'",
            "echo '`id`'",
            "cat <(ls)",
            "(ls)",
            "{ ls; }",
            "echo {a;rm -rf sub}",
            "echo [a;rm -rf sub]",
            "ls *.txt",
            "cat ~/.ssh/id_rsa",
            "cat ~root/x",
            "ls &",
            "ls & cat inside.txt",
            "xargs rm",
            "ls | xargs rm",
            "cat inside.txt | sh",
            // sed programs that write, read, or execute.
            "sed 'w out' f",
            "sed -n 's/a/b/w out' f",
            "sed '1e id' f",
            "sed 's/a/b/e' f",
            "sed -f script f",
            "sed -l 'w out' p f",
            "sed --expression 'w out' -e p f",
            "sed --expression=p f",
            "sed 'y/[/a/;s,]X,/0123456789/;w out,' f",
            "sed -i 's/a/b/w ../outside/x' f",
            "sed -i '1e id' f",
            "sed --in-place=../outside/* s/a/b/ f",
            // Paths that cannot be checked.
            "cat sub/../inside.txt",
            "cat //server/share/x",
            "cat \\\\\\\\server\\\\share\\\\x",
            "cat /dev/clipboard",
            "cat /proc/self/environ",
            // Changes outside the workspace, or to what commands run.
            "touch ../outside/new.txt",
            "cp inside.txt -t ../outside",
            "cp inside.txt --target-directory=../outside",
            "mv inside.txt ../outside/",
            "rm ../outside/outside.txt",
            "rm -rf .",
            "rm -rf .git",
            "touch .git/config",
            "cp a .gitconfig",
            "touch .bashrc",
            "rmdir -p sub/deep",
            "cp -L a b",
            "cp -r sub/. copy",
            // A `cd` moves what every later path is relative to.
            "cd sub && cat x",
            "cd sub && ls",
            "cd /tmp && git status",
            "cd sub && git status",
            "cd sub; cd deep",
            "cd",
            "cd -",
            // One line that lays a file down and then runs git.
            "touch HEAD && git status",
        ];
        commands.push(&long);
        for command in commands {
            assert_clears(&fixture, "bash", command, Clears::FullAccessOnly);
        }
        // What `sh` and `zsh` read differently from Bash.
        assert_clears(&fixture, "sh", "ls &>/dev/null", Clears::FullAccessOnly);
        assert_clears(&fixture, "zsh", "ls # comment", Clears::FullAccessOnly);
        assert_clears(&fixture, "zsh", "cat =ls", Clears::FullAccessOnly);

        for command in [
            "Get-Content x | Out-File y",
            "Get-Content inside.txt > out.txt",
            "Remove-Item -Recurse sub",
            "Remove-Item -Rec sub",
            "Remove-Item \u{2013}Recurse sub",
            "Get-Content $env:USERPROFILE\\x",
            "Get-Content $x",
            "Get-Content \"$x\"",
            "Get-ChildItem | Get-Content",
            "Get-ChildItem | Select-String secret",
            "Get-ChildItem | ForEach-Object Delete",
            "Get-Process -ComputerName evil",
            "Get-Command git",
            "Get-Command ..\\outside\\x.ps1 | Select-Object -ExpandProperty ScriptContents",
            "Set-Content inside.txt -PSPath out.txt",
            "Set-Content inside.txt value extra",
            "sc stop Spooler",
            "Get-Content inside.txt -Wait",
            "Invoke-Expression ls",
            "iex ls",
            "& ls",
            ". .\\script.ps1",
            "Get-Content ~\\x",
            "Get-Content *.txt",
            "Get-Content 'sub\\*'",
            "Get-Content 'a[bc].txt'",
            "Set-Content 'a[bc].txt' value",
            "Set-Content '-Path' evil",
            "Get-ChildItem '-Force'",
            "Start-Process notepad",
            "Get-Content \\\\server\\share\\x",
            "Get-Content //server/share/x",
            "Get-Content (Get-ChildItem)",
            "Get-ChildItem { Remove-Item x }",
            "Set-Content ..\\outside\\new.txt value",
            "Set-Content Temp:new.txt value",
            "Remove-Item .",
            "Set-Content .git\\config value",
            "Get-ItemProperty HKLM:\\Software",
        ] {
            assert_clears(&fixture, "powershell", command, Clears::FullAccessOnly);
            assert_clears(&fixture, "pwsh", command, Clears::FullAccessOnly);
        }
    }

    #[test]
    fn refusals_name_the_rule_that_refused() {
        let fixture = shell_fixture();
        for (tool, command, rule) in [
            ("bash", "cat a > b", "shell.redirection"),
            ("bash", "echo $(rm x)", "shell.dynamic_expansion"),
            ("bash", "ls &", "shell.background_or_call_operator"),
            ("bash", "(ls)", "shell.grouping"),
            ("bash", "find . -delete", "shell.find_side_effect"),
            ("bash", "sort -o out in", "shell.unverified_option"),
            ("bash", "git diff --output=x", "git.unverifiable_options"),
            ("bash", "git push", "git.state_change"),
            ("bash", "LD_PRELOAD=x ls", "shell.environment_assignment_unverified"),
            ("bash", "env X=1 ls", "shell.unclassified_command"),
            ("bash", "xargs rm", "shell.xargs_data_dependent_command"),
            ("bash", "sed 'w out' f", "shell.sed_script_unverified"),
            ("bash", "cd sub && cat x", "shell.cd_then_relative_path"),
            ("bash", "cd sub && git status", "shell.cd_then_git"),
            ("bash", "touch HEAD && git status", "shell.file_change_then_git"),
            ("bash", "cat //server/share/x", "shell.unverifiable_path"),
            ("bash", "ls *.txt", "shell.dynamic_read_path"),
            ("bash", "touch .git/config", "shell.file_change_unverified"),
            ("powershell", "Remove-Item -Recurse sub", "powershell.unverified_parameter"),
            ("powershell", "Get-ChildItem | Get-Content", "powershell.pipeline_input_unverifiable"),
            ("powershell", "Get-Content $x", "shell.dynamic_expansion"),
            ("powershell", "Start-Process notepad", "powershell.unclassified_command"),
        ] {
            let decision = fixture.shell(SecurityLevel::RequestApproval, tool, command);
            assert_eq!(decision.rule_id, rule, "{tool}: {command}");
            assert!(decision.requires_approval, "{tool}: {command}");
        }
        // A cleared call still says which rule cleared it.
        let read = fixture.shell(SecurityLevel::RequestApproval, "bash", "ls");
        assert_eq!(read.rule_id, "shell.read_only");
        assert_eq!(read.risk_level, RiskLevel::Low);
        assert_eq!(read.effect, OperationEffect::Read);
    }

    /// Where options may follow operands is the program's to say. A word
    /// after the first operand that reads as an option is proven only when no
    /// file of that name is there to be opened instead.
    #[test]
    fn an_option_after_an_operand_is_also_judged_as_the_operand_it_may_be() {
        let fixture = shell_fixture();
        assert_clears(&fixture, "bash", "ls sub --color", Clears::Always);
        fs::write(fixture.workspace.join("--color"), "x").unwrap();
        assert_clears(&fixture, "bash", "ls sub --color", Clears::FullAccessOnly);
        // In front of the operands it is an option to every parser.
        assert_clears(&fixture, "bash", "ls --color sub", Clears::Always);
    }

    /// A quote inside a comment does not hide the line after it: the comment
    /// ends at its newline and the next line is a command like any other.
    #[test]
    fn a_comment_ends_at_its_own_line() {
        let fixture = shell_fixture();
        assert_clears(&fixture, "bash", "ls # '\nrm -rf sub\n# '", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "ls # '\ncurl example.com\n# '", Clears::FullAccessOnly);
        assert_clears(
            &fixture,
            "powershell",
            "Get-ChildItem # '\nRemove-Item inside.txt # '",
            Clears::FromAcceptEdits,
        );
        assert_clears(
            &fixture,
            "powershell",
            "Get-ChildItem # '\nStart-Process notepad # '",
            Clears::FullAccessOnly,
        );
    }

    /// Path spellings a Windows shell opens as something other than the
    /// file the path guard would resolve.
    #[cfg(windows)]
    #[test]
    fn windows_path_forms_are_translated_or_refused() {
        let fixture = shell_fixture();
        // Git Bash's own spelling of a drive path is the same file.
        let workspace = fixture.workspace.to_string_lossy().replace('\\', "/");
        let mounted = format!(
            "/{}{}",
            workspace[..1].to_ascii_lowercase(),
            &workspace[2..]
        );
        assert_clears(&fixture, "bash", &format!("cat {mounted}/inside.txt"), Clears::Always);
        assert_clears(&fixture, "bash", &format!("cat {workspace}/inside.txt"), Clears::Always);
        assert_clears(&fixture, "bash", "cat /c/Windows/win.ini", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", &format!("touch {mounted}/new.txt"), Clears::FromAcceptEdits);
        for command in [
            // Mounts only MSYS can place.
            "cat /etc/passwd",
            "cat /tmp/x",
            "ls /",
            // Devices, trimmed names, streams, drive-relative paths.
            "cat NUL",
            "cat CON",
            "cat sub/com1.txt",
            "cp inside.txt COM1",
            "cp inside.txt NUL",
            "mkdir aux",
            "cat 'inside.txt.'",
            "cat 'inside.txt '",
            "touch '.git./config'",
            "cat 'inside.txt::$DATA'",
            "cat 'C:inside.txt'",
            "cat 'sub\\..\\inside.txt'",
        ] {
            assert_clears(&fixture, "bash", command, Clears::FullAccessOnly);
        }
        for command in [
            "Get-Content NUL",
            "Get-Content \\Windows\\win.ini",
            "Get-Content C:inside.txt",
            "Set-Content inside.txt:stream value",
            "Set-Content COM1 value",
        ] {
            assert_clears(&fixture, "powershell", command, Clears::FullAccessOnly);
        }
        assert_clears(
            &fixture,
            "powershell",
            "Get-Content C:\\Windows\\win.ini",
            Clears::FromAcceptEdits,
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn posix_absolute_paths_outside_the_roots_are_reads_the_level_decides() {
        let fixture = shell_fixture();
        assert_clears(&fixture, "bash", "cat /etc/hosts", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "cat /dev/zero", Clears::FullAccessOnly);
    }

    #[test]
    fn a_relative_path_is_proven_only_from_the_directory_the_call_starts_in() {
        let fixture = shell_fixture();
        let unknown = ShellSite::Local { cwd: None };
        // Nothing that depends on the working directory is proven without it…
        for command in ["ls", "cat inside.txt", "git status", "cd sub", "touch new.txt"] {
            assert_clears_from(&fixture, &unknown, "bash", command, Clears::FullAccessOnly);
        }
        let decision =
            fixture.shell_from(SecurityLevel::RequestApproval, "bash", "cat inside.txt", &unknown);
        assert_eq!(decision.rule_id, "shell.unanchored_working_directory");
        // …while what the command does is still described for the card.
        assert_eq!(decision.risk_level, RiskLevel::Low);
        assert_eq!(decision.effect, OperationEffect::Read);
        // What does not depend on it is.
        let inside = posix_quoted(&fixture.workspace.join("inside.txt"));
        for command in ["echo hi", "pwd", &format!("cat {inside}")] {
            assert_clears_from(&fixture, &unknown, "bash", command, Clears::Always);
        }
        assert_clears_from(&fixture, &unknown, "powershell", "Write-Output hi", Clears::Always);
        assert_clears_from(&fixture, &unknown, "powershell", "Get-ChildItem", Clears::FullAccessOnly);

        // From a subdirectory the same words name other files.
        let sub = ShellSite::Local {
            cwd: Some(fixture.workspace.join("sub")),
        };
        assert_clears_from(&fixture, &sub, "bash", "cat x", Clears::Always);
        assert_clears_from(&fixture, &sub, "bash", "cat ../inside.txt", Clears::Always);
        assert_clears_from(&fixture, &sub, "bash", "cat ../../outside/outside.txt", Clears::FromAcceptEdits);
        assert_clears_from(&fixture, &sub, "bash", "rm -rf ../sub", Clears::FullAccessOnly);
        assert_clears_from(&fixture, &sub, "bash", "rm x", Clears::FromAcceptEdits);

        // A start directory that is gone, or is not in the workspace, is none.
        for cwd in [fixture.workspace.join("missing"), fixture.outside.clone()] {
            let site = ShellSite::Local { cwd: Some(cwd) };
            assert_clears_from(&fixture, &site, "bash", "cat inside.txt", Clears::FullAccessOnly);
            assert_clears_from(&fixture, &site, "bash", "echo hi", Clears::Always);
        }

        // On another machine no path can be checked at all.
        for command in ["ls", "cat inside.txt", &format!("cat {inside}"), "git status"] {
            assert_clears_from(&fixture, &ShellSite::Elsewhere, "bash", command, Clears::FullAccessOnly);
        }
        let remote =
            fixture.shell_from(SecurityLevel::RequestApproval, "bash", "ls", &ShellSite::Elsewhere);
        assert_eq!(remote.rule_id, "shell.remote_paths_unverifiable");
        assert_clears_from(&fixture, &ShellSite::Elsewhere, "bash", "echo hi", Clears::Always);
    }

    #[test]
    fn protected_application_data_is_not_reached_by_a_proven_command() {
        let fixture = shell_fixture();
        fs::create_dir_all(fixture.app_data.join("memory")).unwrap();
        fs::write(fixture.app_data.join("memory/note.md"), "secret").unwrap();
        fs::create_dir_all(fixture.app_data.join("shell-snapshots")).unwrap();
        let app = |name: &str| posix_quoted(&fixture.app_data.join(name));
        // The application data directory is a trusted root for a read…
        assert_clears(&fixture, "bash", &format!("cat {}", app("app.txt")), Clears::Always);
        for command in [
            // …but not its protected stores, nor a walk that would enter them,
            format!("cat {}", app("memory/note.md")),
            format!("ls {}", app("memory")),
            format!("grep -r secret {}", posix_quoted(&fixture.app_data)),
            format!("ls {}", posix_quoted(&fixture.app_data)),
            // and nothing in it is a shell command's to change.
            format!("touch {}", app("new.txt")),
            format!("rm {}", app("document.v1.json")),
            format!("rm -rf {}", app("memory")),
            format!("cp a {}", app("shell-snapshots/snapshot-bash-1-x.sh")),
        ] {
            assert_clears(&fixture, "bash", &command, Clears::FullAccessOnly);
        }
        assert_clears(
            &fixture,
            "powershell",
            &format!("Get-Content '{}'", fixture.app_data.join("memory/note.md").display()),
            Clears::FullAccessOnly,
        );
        assert_clears(
            &fixture,
            "powershell",
            &format!("Set-Content -Path '{}' -Value tamper", fixture.app_data.join("document.v1.json").display()),
            Clears::FullAccessOnly,
        );
    }

    /// A junction needs no privilege on Windows, so this runs where the
    /// symlink test below may not be able to.
    #[cfg(windows)]
    #[test]
    fn a_junction_out_of_the_workspace_is_judged_by_where_it_points() {
        let fixture = shell_fixture();
        let link = fixture.workspace.join("jlink");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&fixture.outside)
            .output()
            .expect("cmd runs");
        assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
        assert_clears(&fixture, "bash", "cat jlink/outside.txt", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "ls jlink", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "touch jlink/new.txt", Clears::FullAccessOnly);
        assert_clears(&fixture, "bash", "cp inside.txt jlink/", Clears::FullAccessOnly);
        assert_clears(&fixture, "bash", "rm jlink/outside.txt", Clears::FullAccessOnly);
        assert_clears(&fixture, "powershell", "Get-Content jlink\\outside.txt", Clears::FromAcceptEdits);
        assert_clears(&fixture, "powershell", "Set-Content jlink\\new.txt value", Clears::FullAccessOnly);
        // As a start directory it is not a place inside the workspace either.
        let through = ShellSite::Local {
            cwd: Some(link.clone()),
        };
        assert_clears_from(&fixture, &through, "bash", "cat outside.txt", Clears::FullAccessOnly);
    }

    #[test]
    fn a_link_is_judged_by_where_it_points() {
        let fixture = shell_fixture();
        let link = fixture.workspace.join("link.txt");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(fixture.outside.join("outside.txt"), &link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(fixture.outside.join("outside.txt"), &link);
        if made.is_err() {
            // Creating a link needs a privilege this account may not have.
            return;
        }
        assert_clears(&fixture, "bash", "cat link.txt", Clears::FromAcceptEdits);
        assert_clears(&fixture, "bash", "touch link.txt", Clears::FullAccessOnly);
        assert_clears(&fixture, "bash", "cp inside.txt link.txt", Clears::FullAccessOnly);
        assert_clears(&fixture, "powershell", "Get-Content link.txt", Clears::FromAcceptEdits);
        assert_clears(&fixture, "powershell", "Set-Content link.txt value", Clears::FullAccessOnly);
    }

    #[test]
    fn git_is_not_proven_where_a_repository_could_have_been_planted() {
        let fixture = shell_fixture();
        let planted = fixture.workspace.join("sub");
        fs::write(planted.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(planted.join("objects")).unwrap();
        fs::create_dir_all(planted.join("refs")).unwrap();
        assert_clears(&fixture, "bash", "git status", Clears::Always);
        for cwd in [planted.clone(), planted.join("deep")] {
            let site = ShellSite::Local { cwd: Some(cwd) };
            assert_clears_from(&fixture, &site, "bash", "git status", Clears::FullAccessOnly);
            let decision =
                fixture.shell_from(SecurityLevel::RequestApproval, "bash", "git log", &site);
            assert_eq!(decision.rule_id, "git.unverified_repository");
            // Only git cares.
            assert_clears_from(&fixture, &site, "bash", "ls", Clears::Always);
        }
    }

    /// Accept edits writes inside the trusted roots without asking. Where the
    /// roots hold what decides which program a command name runs, no proof
    /// about the command's text holds.
    #[test]
    fn accept_edits_proves_nothing_where_it_could_rewrite_what_runs() {
        let on_path = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .find(|entry| entry.is_absolute() && entry.is_dir())
            .expect("an absolute PATH entry");
        let mut fixture = shell_fixture();
        fixture.additional.push(on_path.to_string_lossy().into_owned());
        let manual = fixture.shell(SecurityLevel::RequestApproval, "bash", "ls");
        assert!(!manual.requires_approval, "{}", manual.rule_id);
        let accept_edits = fixture.shell(SecurityLevel::AllowEdits, "bash", "ls");
        assert!(accept_edits.requires_approval);
        assert_eq!(accept_edits.rule_id, "shell.writable_command_environment");
        assert!(fixture.shell(SecurityLevel::AllowEdits, "bash", "touch new.txt").requires_approval);
    }

    /// The breaker is judged before anything that could return first.
    #[test]
    fn nothing_on_the_line_talks_the_delete_breaker_out_of_asking() {
        let fixture = Fixture::new();
        let padded = format!("rm -rf / # {}", "x".repeat(MAX_SHELL_ANALYSIS_CHARS));
        for (tool, command) in [
            ("bash", "rm -rf / &"),
            ("bash", "rm -rf ~ # \\\\\\\\server\\\\share"),
            ("bash", padded.as_str()),
            ("bash", "rm -rf /c"),
            ("bash", "rm -rf /c/"),
            ("bash", "rm -rf /c/Users"),
            ("bash", "rm -rf /c/Windows"),
            ("bash", "rm -rf /mnt/c/Windows"),
            ("bash", "rm -rf 'C:\\'"),
            ("bash", "rm -rf 'C:\\Users'"),
            ("powershell", "Remove-Item \u{2013}Recurse C:\\"),
            ("powershell", "Remove-Item \u{2014}Recurse \u{2013}Force C:\\Users"),
            ("powershell", "Remove-Item -Recurse \u{201C}C:\\\u{201D}"),
            // The same delete behind a wrapper, from a search, or down a pipe.
            ("bash", "timeout 5 rm -rf /"),
            ("bash", "timeout -k 1 5 rm -rf ~"),
            ("bash", "stdbuf -oL rm -rf /"),
            ("zsh", "noglob rm -rf /"),
            ("bash", "chronic rm -rf /usr"),
            ("bash", "find / -delete"),
            ("bash", "find /c/Users -depth -delete"),
            ("bash", "find ~ -name x -delete"),
            ("bash", "sudo find /etc /var -type f -delete"),
            ("powershell", "Get-ChildItem C:\\ -Recurse -Force | Remove-Item -Recurse -Force"),
            ("powershell", "Remove-Item -Recurse -Path:C:\\"),
            // Below the top of the Windows directory is still the system.
            ("powershell", "Remove-Item -Recurse -Force C:\\Windows\\System32"),
            ("powershell", "Remove-Item -Recurse -Force C:\\Windows\\System32\\drivers"),
            ("bash", "rm -rf /c/Windows/System32"),
            ("bash", "rm -rf 'C:\\Windows\\SysWOW64'"),
        ] {
            for level in LEVELS {
                let decision = fixture.shell(level, tool, command);
                assert!(decision.mandatory_prompt, "{tool} at {level:?}: {command:?}");
                assert!(decision.requires_approval, "{tool} at {level:?}: {command:?}");
            }
        }
        // A project folder on a mounted drive is still an ordinary delete, and
        // so is a search that deletes under the workspace.
        for command in [
            "rm -rf /c/work/app/build",
            "find . -name '*.tmp' -delete",
            "find build -delete",
            "timeout 5 rm -rf build",
            "git rm -r --cached .",
        ] {
            let ordinary = fixture.shell(SecurityLevel::FullAccess, "bash", command);
            assert!(!ordinary.mandatory_prompt && !ordinary.requires_approval, "{command}");
        }
    }

    #[test]
    fn a_sed_program_is_proven_only_from_commands_that_cannot_write_or_execute() {
        for script in [
            "",
            "p",
            "1p",
            "1,5p",
            "$d",
            "$=",
            "5q",
            "/start/,/end/p",
            "/re/I,+3d",
            "0~2p",
            "$!N;P;D",
            "s/a/b/",
            "s/a/b/g",
            "s|/usr/bin|/opt|gI",
            "s/[0-9]*//2p",
            "s/[^]a-z]//",
            "s/a\\/b/c/; s/d/e/",
            "y/abc/xyz/",
            "1!G;h;$!d",
        ] {
            assert!(sed_script_is_inert(script), "{script:?}");
        }
        for script in [
            "w out",
            "1w out",
            "r secret",
            "e",
            "1e id",
            "s/a/b/w out",
            "s/a/b/e",
            "s/a/b/ge",
            "s/a/b/;w out",
            "s/a/b/\nw out",
            "y/[/a/;s,]X,/0123456789/;w out,",
            "y/abc/[yz/",
            "s/[/]/x/w out",
            // Whether the `/` inside the brackets ends the pattern depends on
            // which sed reads it.
            "s/[]/]x//",
            "a text",
            "i\\\ntext",
            "{p}",
            "b end",
            "#n",
            "s/a/b",
            "\\%re%p",
        ] {
            assert!(!sed_script_is_inert(script), "{script:?}");
        }
    }

    /// The run loop's entry point works out which workspace a shell call
    /// named and asks the host where that workspace's shell is.
    #[test]
    fn a_shell_call_is_judged_from_the_directory_its_workspace_remembers() {
        let fixture = shell_fixture();
        let state = crate::state::AppState::default();
        let workspaces = crate::workspace_set::WorkspaceSet::default();
        let classify = |command: &str, placed: bool| {
            let request = request(&fixture.workspace, "bash", json!({"command": command}));
            let decision = if placed {
                classify_model_call_in_workspaces_at(
                    SecurityLevel::RequestApproval,
                    &workspaces,
                    &fixture.workspace,
                    &fixture.app_data,
                    &fixture.additional,
                    &request,
                    &remembered_shell_cwd(&state, "conversation-test"),
                )
            } else {
                classify_model_call_in_workspaces(
                    SecurityLevel::RequestApproval,
                    &workspaces,
                    &fixture.workspace,
                    &fixture.app_data,
                    &fixture.additional,
                    &request,
                )
            };
            decision.unwrap_or_else(|error| panic!("{command}: {error}"))
        };
        // A caller that does not say where the shell is proves nothing about
        // a relative path, and still proves what needs none.
        let unplaced = classify("cat inside.txt", false);
        assert!(unplaced.requires_approval);
        assert_eq!(unplaced.rule_id, "shell.unanchored_working_directory");
        assert!(!classify("echo hi", false).requires_approval);
        // Before the workspace's first `cd` a call starts at its root.
        assert!(!classify("cat inside.txt", true).requires_approval);
        assert!(classify("cat ../inside.txt", true).requires_approval);
        // After one, the same words name another file.
        let local = crate::workspace_set::WorkspaceSet::local_root(
            fixture.workspace.to_string_lossy().into_owned(),
        );
        let key = crate::tool_executor::shell_cwd_key(local.select(None).unwrap());
        state.set_shell_cwd(
            "conversation-test",
            &key,
            Some(fixture.workspace.join("sub").to_string_lossy().into_owned()),
        );
        assert!(!classify("cat ../inside.txt", true).requires_approval);
        assert!(classify("cat ../../outside/outside.txt", true).requires_approval);
    }

    #[test]
    fn a_shell_call_on_another_machine_proves_nothing_about_its_paths() {
        let fixture = shell_fixture();
        let state = crate::state::AppState::default();
        let workspaces = remote_workspaces(&fixture);
        let classify = |input: Value| {
            classify_model_call_in_workspaces_at(
                SecurityLevel::RequestApproval,
                &workspaces,
                &fixture.workspace,
                &fixture.app_data,
                &fixture.additional,
                &request(&fixture.workspace, "bash", input),
                &remembered_shell_cwd(&state, "conversation-test"),
            )
            .unwrap()
        };
        let remote = classify(json!({"command": "ls", "workspace": 2}));
        assert!(remote.requires_approval);
        assert_eq!(remote.rule_id, "shell.remote_paths_unverifiable");
        assert!(classify(json!({"command": "cat /etc/hostname", "workspace": 2})).requires_approval);
        assert!(!classify(json!({"command": "echo hi", "workspace": 2})).requires_approval);
        // A workspace the conversation does not have is one the executor
        // refuses; nothing is proven for it.
        assert!(classify(json!({"command": "ls", "workspace": 9})).requires_approval);
        assert!(classify(json!({"command": "ls", "workspace": "two"})).requires_approval);
        // The workspace on this machine is judged where it is.
        assert!(!classify(json!({"command": "ls", "workspace": 1})).requires_approval);
    }

    /// At Accept edits a write inside the trusted roots goes through unasked,
    /// and so does a proven shell command. The files that decide what such a
    /// command runs are therefore not ordinary files at that level.
    #[test]
    fn accept_edits_asks_before_a_write_that_changes_what_commands_run() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.workspace.join(".git")).unwrap();
        let snapshot = fixture
            .app_data
            .join("shell-snapshots/snapshot-bash-1-x.sh")
            .to_string_lossy()
            .into_owned();
        for path in [
            ".git/config",
            ".git/hooks/pre-commit",
            ".GIT/config",
            ".gitconfig",
            "nested/.bashrc",
            ".zshenv",
            snapshot.as_str(),
        ] {
            let write = |level| {
                fixture
                    .classify_model_call(level, "write", json!({"path": path, "content": "x"}))
                    .unwrap_or_else(|error| panic!("{path}: {error}"))
            };
            let asked = write(SecurityLevel::AllowEdits);
            assert!(asked.requires_approval, "{path}");
            assert!(!asked.mandatory_prompt, "{path}");
            assert_eq!(asked.rule_id, "filesystem.shell_trust_anchor", "{path}");
            assert_eq!(asked.risk_level, RiskLevel::High, "{path}");
            // The other levels keep their own rules: Manual asks about every
            // write, full access about none, and a read is a read.
            assert_eq!(
                write(SecurityLevel::RequestApproval).rule_id,
                "filesystem.trusted_write_manual",
                "{path}"
            );
            assert!(!write(SecurityLevel::FullAccess).requires_approval, "{path}");
            let read = fixture
                .classify_model_call(SecurityLevel::AllowEdits, "read", json!({"path": path}))
                .unwrap();
            assert!(!read.requires_approval, "{path}");
        }
        for path in ["notes.md", "src/git/config", ".github/workflows/ci.yml", ".gitignore"] {
            let decision = fixture
                .classify_model_call(
                    SecurityLevel::AllowEdits,
                    "write",
                    json!({"path": path, "content": "x"}),
                )
                .unwrap();
            assert!(!decision.requires_approval, "{path}");
            assert_eq!(decision.rule_id, "filesystem.trusted_allow_edits", "{path}");
        }
    }
}
