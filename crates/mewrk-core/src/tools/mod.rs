//! Built-in tools. Names and arguments are Mewrk's, so its prompt texts apply unchanged.

mod agents;
mod fs;
mod interact;
mod memory;
mod shell;
mod web;

use std::fs::Metadata;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::engine::{Engine, RunHandle, RunSpec};
use crate::inbox::{ApprovalDecision, RunEvent, RunStatus};
use crate::model::{ItemId, PermissionMode, ToolMeta};
use crate::prompt::{render, text};
use crate::tasks::Session;

pub(crate) use memory::{memory_context, skill_listing};

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;

#[derive(Clone, Debug, Default)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
    pub meta: ToolMeta,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: false, meta: ToolMeta::default() }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: true, meta: ToolMeta::default() }
    }

    pub fn with_meta(mut self, meta: ToolMeta) -> Self {
        self.meta = meta;
        self
    }
}

/// What a tool gets to work with.
pub struct ToolCtx {
    pub engine: Arc<Engine>,
    pub run: Arc<RunHandle>,
    pub session: Arc<Session>,
    /// The run's settings. Its `items` are empty.
    pub spec: Arc<RunSpec>,
    pub item: ItemId,
    pub cwd: PathBuf,
    pub workspaces: Vec<PathBuf>,
}

pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Whether this call changes the machine or reaches beyond it, and so needs approval.
    fn dangerous(&self, args: &Value) -> bool;

    /// Whether this call may run next to others.
    fn parallel_safe(&self, _args: &Value) -> bool {
        false
    }

    /// Whether plan mode refuses this call.
    fn blocked_in_plan_mode(&self, _args: &Value) -> bool {
        false
    }

    /// One line for an approval card.
    fn summary(&self, args: &Value) -> String {
        args.to_string()
    }

    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture;
}

fn registry() -> &'static Vec<Arc<dyn Tool>> {
    static REG: OnceLock<Vec<Arc<dyn Tool>>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut v: Vec<Arc<dyn Tool>> = Vec::new();
        fs::register(&mut v);
        shell::register(&mut v);
        web::register(&mut v);
        agents::register(&mut v);
        memory::register(&mut v);
        interact::register(&mut v);
        v
    })
}

pub fn find(name: &str) -> Option<Arc<dyn Tool>> {
    registry().iter().find(|t| t.name() == name).cloned()
}

/// Tools a subagent does not get: it has no user to ask, it cannot leave plan mode for anyone, and it directs no agents of its own.
const NOT_FOR_CHILDREN: [&str; 5] = ["ask_user", "exit_plan_mode", "plan", "agent_spawn", "send_message"];

/// The names offered in a request, in a stable order so that the server's prompt cache keeps working.
pub fn offered(is_child: bool, disabled: &[String]) -> Vec<String> {
    let mut names: Vec<String> =
        registry().iter().map(|t| t.name().to_owned()).filter(|n| !disabled.contains(n)).filter(|n| !is_child || !NOT_FOR_CHILDREN.contains(&n.as_str())).collect();
    names.sort();
    names
}

// ---------------------------------------------------------------- helpers the tools share

pub(crate) const MAX_PATH_CHARS: usize = 4096;
const MAX_TOOL_OUTPUT: usize = 64 * 1024;
/// How much of a spilled output the model sees in place of it.
const PREVIEW_CHARS: usize = 2_000;

/// Turns the `Result` a tool body wants to write into the future the trait wants.
fn future(body: impl Future<Output = Result<ToolResult, String>> + Send + 'static) -> ToolFuture {
    Box::pin(async move { body.await.unwrap_or_else(ToolResult::err) })
}

/// Blocking filesystem work, off the runtime's threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| format!("The tool crashed: {e}"))
}

pub(crate) fn required_string(args: &Value, key: &str, max_chars: usize, allow_empty: bool) -> Result<String, String> {
    let value = args.get(key).ok_or_else(|| format!("Missing parameter {key}"))?.as_str().ok_or_else(|| format!("Parameter {key} must be a string"))?;
    if !allow_empty && value.trim().is_empty() {
        return Err(format!("Parameter {key} cannot be empty"));
    }
    if value.chars().nth(max_chars).is_some() {
        return Err(format!("Parameter {key} exceeds the {max_chars}-character limit"));
    }
    Ok(value.to_owned())
}

/// Models that fill every optional parameter send `""` for the ones they mean to leave out.
pub(crate) fn optional_string(args: &Value, key: &str, default: &str, max_chars: usize) -> Result<String, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default.to_owned()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(default.to_owned()),
        Some(_) => required_string(args, key, max_chars, false),
    }
}

pub(crate) fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| format!("Parameter {key} must be a non-negative integer")),
    }
}

pub(crate) fn optional_bool(args: &Value, key: &str, default: bool) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v.as_bool().ok_or_else(|| format!("Parameter {key} must be a boolean")),
    }
}

/// The root a call acts in. A number rather than a path, because which folders a conversation reaches is not a tool argument's to widen.
fn workspace_root(ctx: &ToolCtx, args: &Value) -> Result<PathBuf, String> {
    workspace_at(&ctx.cwd, &ctx.workspaces, args)
}

fn workspace_at(cwd: &Path, workspaces: &[PathBuf], args: &Value) -> Result<PathBuf, String> {
    match args.get("workspace") {
        None | Some(Value::Null) => Ok(cwd.to_owned()),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|n| *n >= 1)
            .and_then(|n| workspaces.get(n as usize - 1))
            .cloned()
            .ok_or_else(|| "The workspace parameter must be one of the workspace numbers listed in the Environment section".to_owned()),
        Some(_) => Err("The workspace parameter must be a number naming one of this conversation's workspaces".to_owned()),
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, String> {
    let canonical = std::fs::canonicalize(path).map_err(|e| format!("Could not access workspace {}: {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!("Workspace is not a directory: {}", canonical.display()));
    }
    Ok(canonical)
}

/// `requested` against `base`, with `.` and `..` resolved by spelling: the file may not exist yet.
fn candidate_path(base: &Path, requested: &str) -> Result<PathBuf, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("Path must not be empty".to_owned());
    }
    if requested.contains('\0') {
        return Err("Path contains an invalid character".to_owned());
    }
    let joined = base.join(requested);
    let mut out = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn resolve_existing(workspace: &Path, requested: &str) -> Result<PathBuf, String> {
    let candidate = candidate_path(workspace, requested)?;
    std::fs::canonicalize(&candidate).map_err(|e| format!("Could not access path {requested}: {e}"))
}

/// A path as the model sees it: relative to its workspace when it is inside.
fn display_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace).unwrap_or(path).to_string_lossy().into_owned()
}

pub(crate) fn modified_ms(meta: &Metadata) -> u64 {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn truncate_chars(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        None => value.to_owned(),
        Some((end, _)) => format!("{}…", &value[..end]),
    }
}

pub(crate) fn truncate_output(value: &str) -> String {
    if value.len() <= MAX_TOOL_OUTPUT {
        return value.to_owned();
    }
    let mut end = MAX_TOOL_OUTPUT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n{}", &value[..end], text("tool.output_truncated"))
}

/// `70.3 KB`, `2 KB`, `1.4 MB`: decimal units, one decimal place when it says something.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} bytes");
    }
    let rounded = (value * 10.0).round() / 10.0;
    if rounded.fract() == 0.0 { format!("{rounded:.0} {}", UNITS[unit]) } else { format!("{rounded:.1} {}", UNITS[unit]) }
}

/// Where a conversation's oversized outputs and background command logs go. Inside the app's own folder, and `read` reaches it without asking.
fn output_dir(ctx: &ToolCtx) -> PathBuf {
    let conv: String = ctx.spec.conv_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    ctx.engine.store.dir().join("tool-output").join(conv)
}

/// What the model gets in place of an output saved at `path`. The preview ends at a line break when one is past halfway, so it does not stop
/// mid-line.
fn spill_notice(whole: &str, path: &Path) -> String {
    let window_end = whole.char_indices().nth(PREVIEW_CHARS).map_or(whole.len(), |(i, _)| i);
    let window = &whole[..window_end];
    let preview = match window.rfind('\n') {
        Some(end) if window[..end].chars().count() > PREVIEW_CHARS / 2 => &window[..end],
        _ => window,
    };
    render(
        "tool.output_spilled",
        &[("size", &human_size(whole.len() as u64)), ("path", &path.display().to_string()), ("preview_size", &human_size(PREVIEW_CHARS as u64)), ("preview", preview)],
    )
}

/// Past `limit` characters the whole output goes to a file and the model gets its path and a preview. With no file to be had, the output is cut
/// instead.
fn fit(output: String, limit: usize, dir: &Path, stem: &str) -> String {
    if output.chars().nth(limit).is_none() {
        return output;
    }
    let path = dir.join(format!("{stem}-{}.txt", crate::model::new_id()));
    if std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, &output)).is_ok() {
        return spill_notice(&output, &path);
    }
    let end = output.char_indices().nth(limit).map_or(output.len(), |(i, _)| i);
    format!("{}\n{}", &output[..end], text("tool.output_truncated"))
}

/// Reading outside the workspaces needs a person's say-so where the conversation asks for one. A tool cannot tell from its arguments alone, so
/// `dangerous` cannot answer this.
async fn allow_outside_read(ctx: &ToolCtx, tool: &str, args: &Value, target: &Path) -> Result<(), String> {
    if ctx.spec.settings.permission != PermissionMode::Ask || ctx.session.always_allowed.lock().contains(tool) {
        return Ok(());
    }
    let inside = ctx.workspaces.iter().filter_map(|w| std::fs::canonicalize(w).ok()).any(|w| target.starts_with(w))
        || std::fs::canonicalize(ctx.engine.store.dir().join("tool-output")).is_ok_and(|d| target.starts_with(d))
        || memory::user_skills_root().and_then(|d| std::fs::canonicalize(d).ok()).is_some_and(|d| target.starts_with(d));
    if inside {
        return Ok(());
    }
    ctx.run.emit(RunEvent::Status(RunStatus::WaitingForUser));
    let decision = ctx.run.ask_approval(ctx.item, tool, args, target.display().to_string()).await;
    ctx.run.emit(RunEvent::Status(RunStatus::RunningTools));
    match decision {
        Some(ApprovalDecision::Allow) => Ok(()),
        Some(ApprovalDecision::AllowAlways) => {
            ctx.session.always_allowed.lock().insert(tool.to_owned());
            Ok(())
        }
        Some(ApprovalDecision::Deny) => Err("The user denied this tool call.".to_owned()),
        None => Err("The run was stopped before this tool call finished.".to_owned()),
    }
}

/// Whether a stop came from the model rather than a person, as the result has to say which.
fn stopped_by_model(task: &crate::tasks::Task) -> bool {
    task.stopped_by_model.load(Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::engine::{Engine, RunSpec};
    use crate::inbox::Notifier;
    use crate::model::{ConvSettings, ModelProfile, ProviderConfig, ProviderKind};
    use crate::store::Store;

    pub(crate) struct Fixture {
        pub dir: PathBuf,
        pub ctx: Arc<ToolCtx>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn spec(dir: &Path, workspaces: Vec<PathBuf>, provider: ProviderConfig, permission: PermissionMode) -> RunSpec {
        RunSpec {
            conv_id: "conv".into(),
            store_key: "conv".into(),
            items: Vec::new(),
            settings: ConvSettings { permission, ..Default::default() },
            provider,
            model: ModelProfile { id: "test-model".into(), ..Default::default() },
            api_key: None,
            workspaces: if workspaces.is_empty() { vec![dir.join("work")] } else { workspaces },
            global_instructions: String::new(),
            session: Session::new(),
            child: None,
        }
    }

    /// A context whose run ended at once for want of a provider. The handle still works, and that is all a tool uses of it.
    pub(crate) fn fixture(permission: PermissionMode) -> Fixture {
        fixture_with(permission, ProviderConfig { kind: ProviderKind::OpenAiChat, base_url: "not a url".into(), ..Default::default() })
    }

    pub(crate) fn fixture_with(permission: PermissionMode, provider: ProviderConfig) -> Fixture {
        let dir = std::env::temp_dir().join(format!("mewrk-tools-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join("work")).unwrap();
        // The same spelling the tools get back from `canonicalize`, whatever `temp_dir` is linked through.
        let dir = std::fs::canonicalize(dir).unwrap();
        let store = Store::open(dir.join("data")).unwrap();
        let engine = Engine::new(tokio::runtime::Handle::current(), store, Notifier::none());
        let spec = spec(&dir, Vec::new(), provider, permission);
        let run = engine.start(spec.clone(), "test");
        let ctx = Arc::new(ToolCtx {
            engine,
            run,
            session: spec.session.clone(),
            item: 1,
            cwd: spec.workspaces[0].clone(),
            workspaces: spec.workspaces.clone(),
            spec: Arc::new(spec),
        });
        Fixture { dir, ctx }
    }

    pub(crate) async fn call(ctx: &Arc<ToolCtx>, tool: &str, args: Value) -> ToolResult {
        find(tool).unwrap_or_else(|| panic!("no tool {tool}")).run(ctx.clone(), args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn children_are_not_offered_what_is_not_theirs() {
        let top = offered(false, &[]);
        let child = offered(true, &[]);
        for name in NOT_FOR_CHILDREN {
            assert!(top.iter().any(|n| n == name), "{name}");
            assert!(!child.iter().any(|n| n == name), "{name}");
        }
        assert!(child.iter().any(|n| n == "task_wait"));
        let mut sorted = top.clone();
        sorted.sort();
        assert_eq!(top, sorted);
        assert!(!offered(false, &["grep".to_owned()]).iter().any(|n| n == "grep"));
    }

    #[test]
    fn every_offered_tool_has_a_spec() {
        for is_child in [false, true] {
            let names = offered(is_child, &[]);
            let specs = crate::prompt::tool_specs(&names, is_child);
            assert_eq!(specs.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), names);
        }
    }

    #[test]
    fn the_workspace_number_picks_a_root_and_nothing_else_does() {
        let cwd = PathBuf::from("/a");
        let all = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(workspace_at(&cwd, &all, &json!({})).unwrap(), PathBuf::from("/a"));
        assert_eq!(workspace_at(&cwd, &all, &json!({"workspace": 2})).unwrap(), PathBuf::from("/b"));
        assert!(workspace_at(&cwd, &all, &json!({"workspace": 3})).is_err());
        assert!(workspace_at(&cwd, &all, &json!({"workspace": 0})).is_err());
        assert!(workspace_at(&cwd, &all, &json!({"workspace": "2"})).is_err());
    }

    #[test]
    fn paths_resolve_by_spelling() {
        assert_eq!(candidate_path(Path::new("/w"), "a/./b/../c").unwrap(), PathBuf::from("/w/a/c"));
        assert_eq!(candidate_path(Path::new("/w"), "/abs/x").unwrap(), PathBuf::from("/abs/x"));
        assert!(candidate_path(Path::new("/w"), " ").is_err());
    }

    #[test]
    fn sizes_read_like_a_person_wrote_them() {
        assert_eq!(human_size(12), "12 bytes");
        assert_eq!(human_size(2_000), "2 KB");
        assert_eq!(human_size(70_300), "70.3 KB");
        assert_eq!(human_size(1_400_000), "1.4 MB");
    }

    #[test]
    fn a_long_output_goes_to_a_file_and_a_short_one_stays() {
        let dir = std::env::temp_dir().join(format!("mewrk-fit-{}", crate::model::new_id()));
        assert_eq!(fit("short".into(), 10, &dir, "t"), "short");
        let long = "line\n".repeat(1000);
        let out = fit(long.clone(), 100, &dir, "t");
        assert!(out.starts_with("<persisted-output>\nOutput too large (5 KB). Full output saved to: "), "{out}");
        let saved = out.lines().nth(1).unwrap().rsplit(": ").next().unwrap();
        assert_eq!(std::fs::read_to_string(saved).unwrap(), long);
        let _ = std::fs::remove_dir_all(dir);
    }
}
