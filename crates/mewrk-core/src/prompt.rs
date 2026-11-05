//! What the model is told: Mewrk's prompt profile, assembled for this conversation.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::provider::ToolSpec;
use crate::tools;

const PROFILE: &str = include_str!("../assets/prompt-profile.en-US.json");
const SCHEMAS: &str = include_str!("../assets/builtin-tool-schemas.json");

const SECTION_SEPARATOR: &str = "\n\n---\n\n";

pub struct PromptInput<'a> {
    pub workspaces: &'a [PathBuf],
    pub plan_mode: bool,
    pub offered: &'a [String],
    pub instructions: &'a str,
    pub global_instructions: &'a str,
    pub is_child: bool,
    /// The folder of the global memory tier, whose index the model has to see to know what it can fetch.
    pub global_memory: Option<&'a Path>,
}

// ---------------------------------------------------------------- the profile

fn profile() -> &'static HashMap<String, String> {
    static TEXTS: OnceLock<HashMap<String, String>> = OnceLock::new();
    TEXTS.get_or_init(|| {
        let mut file: Value = serde_json::from_str(PROFILE).expect("the built-in prompt profile is valid JSON");
        serde_json::from_value(file["prompts"].take()).expect("the built-in prompts are strings")
    })
}

/// A profile text, or nothing for a key the profile does not have.
pub(crate) fn text(key: &str) -> &'static str {
    profile().get(key).map_or("", String::as_str)
}

pub(crate) fn render(key: &str, args: &[(&str, &str)]) -> String {
    render_template(text(key), args)
}

/// One pass, so a value holding `{other}` is never expanded. An unknown placeholder stays as written.
pub(crate) fn render_template(template: &str, args: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) if close > 0 && after[..close].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => {
                let name = &after[..close];
                match args.iter().find(|(candidate, _)| *candidate == name) {
                    Some((_, value)) => out.push_str(value),
                    None => {
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

// ---------------------------------------------------------------- {?tool}…{/}

const SHELL_TOOLS: [&str; 5] = ["bash", "zsh", "sh", "pwsh", "powershell"];
const TASK_PRODUCERS: [&str; 8] = ["agent_spawn", "workflow", "bash", "zsh", "sh", "pwsh", "powershell", "preview_start"];

#[derive(Clone, Copy)]
struct Offered<'a>(&'a [String]);

impl Offered<'_> {
    fn has(&self, name: &str) -> bool {
        if name == "@shell" {
            return self.0.iter().any(|tool| SHELL_TOOLS.contains(&tool.as_str()));
        }
        self.0.iter().any(|tool| tool == name)
    }
}

enum Marker<'a> {
    Any(&'a str),
    None(&'a str),
    Close,
}

fn marker_at(text: &str) -> Option<(Marker<'_>, usize)> {
    if text.starts_with("{/}") {
        return Some((Marker::Close, 3));
    }
    let (any, rest) = match text.as_bytes().get(..2)? {
        b"{?" => (true, &text[2..]),
        b"{!" => (false, &text[2..]),
        _ => return None,
    };
    let close = rest.find('}')?;
    let names = &rest[..close];
    let well_formed = !names.is_empty()
        && names.split('|').all(|name| {
            let name = name.strip_prefix('@').unwrap_or(name);
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        });
    if !well_formed {
        return None;
    }
    Some((if any { Marker::Any(names) } else { Marker::None(names) }, close + 3))
}

/// `text` with every segment the offered tools do not meet removed, and every marker gone.
fn resolve<'a>(text: &'a str, offered: Offered<'_>) -> Cow<'a, str> {
    if !text.contains("{?") && !text.contains("{!") && !text.contains("{/}") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut open: Vec<bool> = Vec::new();
    let mut rest = text;
    while let Some(index) = rest.find('{') {
        let shown = open.iter().all(|kept| *kept);
        if shown {
            out.push_str(&rest[..index]);
        }
        let at = &rest[index..];
        match marker_at(at) {
            Some((Marker::Any(names), length)) => {
                open.push(names.split('|').any(|name| offered.has(name)));
                rest = &at[length..];
            }
            Some((Marker::None(names), length)) => {
                open.push(!names.split('|').any(|name| offered.has(name)));
                rest = &at[length..];
            }
            Some((Marker::Close, length)) => {
                open.pop();
                rest = &at[length..];
            }
            None => {
                if shown {
                    out.push('{');
                }
                rest = &at[1..];
            }
        }
    }
    if open.iter().all(|kept| *kept) {
        out.push_str(rest);
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------- the environment

fn sanitize_path(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{061c}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}'))
        .collect()
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS }
}

/// `uname -sr`, read once.
fn os_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        // SAFETY: `info` is a zeroed `utsname` and `uname` only writes into it.
        let mut info: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut info) } != 0 {
            return String::new();
        }
        let field = |raw: &[libc::c_char]| {
            let bytes: Vec<u8> = raw.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        format!("{} {}", field(&info.sysname), field(&info.release)).trim().to_owned()
    })
}

fn today() -> String {
    // SAFETY: `time` with null only returns, and `localtime_r` only writes into the zeroed `tm` it is given.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
    }
}

fn repository_top(directory: &Path) -> Option<&Path> {
    directory.ancestors().find(|a| std::fs::symlink_metadata(a.join(".git")).is_ok())
}

fn push_fact(lines: &mut Vec<String>, fact: String) {
    if !fact.trim().is_empty() {
        lines.push(format!(" - {fact}"));
    }
}

fn environment_section(workspaces: &[PathBuf]) -> String {
    let mut lines = Vec::new();
    if let Some(primary) = workspaces.first() {
        let path = sanitize_path(&primary.to_string_lossy());
        push_fact(&mut lines, render("system.environment.working_directory", &[("path", &path)]));
        let is_repo = if repository_top(primary).is_some() { "true" } else { "false" };
        push_fact(&mut lines, render("system.environment.git_repository", &[("value", is_repo)]));
    }
    // With one workspace the number is not a choice, and the line above has already named it.
    if workspaces.len() > 1 {
        push_fact(&mut lines, text("system.environment.workspaces").to_owned());
        let here = text("system.environment.workspace_on_host");
        for (i, workspace) in workspaces.iter().enumerate() {
            let number = (i + 1).to_string();
            let path = sanitize_path(&workspace.to_string_lossy());
            lines.push(format!("  - {}", render("system.environment.workspace_entry", &[("number", &number), ("path", &path), ("location", here)])));
        }
    }
    push_fact(&mut lines, render("system.environment.platform", &[("platform", platform())]));
    if !os_version().is_empty() {
        push_fact(&mut lines, render("system.environment.os_version", &[("version", &sanitize_path(os_version()))]));
    }
    push_fact(&mut lines, render("system.environment.date", &[("date", &today())]));
    render("system.environment_section", &[("facts", &lines.join("\n"))])
}

// ---------------------------------------------------------------- project instructions

const INSTRUCTION_FILE_LIMIT: u64 = 256 << 10;
const INSTRUCTION_TOTAL_LIMIT: usize = 1 << 20;
const PROMPT_START: &str = "<<<MEWRK_PROJECT_MEMORY_START:v1>>>";
const PROMPT_END: &str = "<<<MEWRK_PROJECT_MEMORY_END:v1>>>";
const SOURCE_START: &str = "<<<MEWRK_PROJECT_MEMORY_SOURCE_START:v1>>>";
const SOURCE_END: &str = "<<<MEWRK_PROJECT_MEMORY_SOURCE_END:v1>>>";

type Stamp = (u64, u64);

#[derive(Clone)]
pub(crate) enum CachedText {
    Text(Arc<str>),
    TooLarge,
    NotUtf8,
    Unreadable,
}

/// The file's text, read again only when it changes. The prompt is built before every request,
/// and a stat costs nothing next to reading every instruction file each time.
pub(crate) fn cached_text(path: &Path, limit: u64) -> Option<CachedText> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (Stamp, CachedText)>>> = OnceLock::new();
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let stamp = (tools::modified_ms(&meta), meta.len());
    let cache = CACHE.get_or_init(Default::default);
    if let Some((old, cached)) = cache.lock().get(path)
        && *old == stamp
    {
        return Some(cached.clone());
    }
    let loaded = if meta.len() > limit {
        CachedText::TooLarge
    } else {
        match std::fs::read(path) {
            Err(_) => CachedText::Unreadable,
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => CachedText::Text(s.into()),
                Err(_) => CachedText::NotUtf8,
            },
        }
    };
    cache.lock().insert(path.to_owned(), (stamp, loaded.clone()));
    Some(loaded)
}

fn escape_markers(value: &str) -> String {
    value
        .replace(PROMPT_START, "[escaped project-memory start marker]")
        .replace(PROMPT_END, "[escaped project-memory end marker]")
        .replace(SOURCE_START, "[escaped project-memory source start marker]")
        .replace(SOURCE_END, "[escaped project-memory source end marker]")
}

fn sanitize_content(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = normalized.chars().map(|c| if c.is_control() && !matches!(c, '\n' | '\t') { '\u{fffd}' } else { c }).collect();
    escape_markers(&cleaned)
}

fn sanitize_metadata(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    escape_markers(&out)
}

/// Every instruction file of the workspaces, from each repository's top down, the way Mewrk's
/// loader finds its own `MEWRK.md`; `AGENTS.md` and `CLAUDE.md` count as project files beside it.
fn project_instructions(workspaces: &[PathBuf]) -> String {
    let mut seen = HashSet::new();
    let mut total = 0usize;
    let mut sources = String::new();
    let mut count = 0usize;
    let mut skipped = Vec::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        let Ok(root) = std::fs::canonicalize(workspace) else { continue };
        let top = repository_top(&root).unwrap_or(&root).to_owned();
        let mut directories: Vec<&Path> = root.ancestors().take_while(|a| a.starts_with(&top)).collect();
        directories.reverse();
        for directory in directories {
            let at_root = directory == root;
            let regular =
                [".mewrk/MEWRK.md", "AGENTS.md", "CLAUDE.md", "MEWRK.md"].map(|name| (name, if at_root { "workspace" } else { "ancestor" }, "startup-hierarchy"));
            let local = [("MEWRK.local.md", if at_root { "workspace-local" } else { "ancestor-local" }, "local-override")];
            for (name, scope, reason) in regular.into_iter().chain(local) {
                let path = directory.join(name);
                let Ok(canonical) = std::fs::canonicalize(&path) else { continue };
                // A link that leaves the project must not send what it points at to the model's provider.
                if !canonical.starts_with(&top) || !seen.insert(canonical.clone()) {
                    continue;
                }
                let Some(content) = cached_text(&canonical, INSTRUCTION_FILE_LIMIT) else { continue };
                let relative = path.strip_prefix(&top).unwrap_or(&path).to_string_lossy().into_owned();
                let label = if index == 0 { relative } else { format!("workspace-{}/{relative}", index + 1) };
                let reason_skipped = match content {
                    CachedText::Text(body) => {
                        if total + body.len() > INSTRUCTION_TOTAL_LIMIT {
                            Some("instruction_skip.over_total_size")
                        } else {
                            total += body.len();
                            count += 1;
                            sources.push_str(SOURCE_START);
                            sources.push_str(&format!("\npath: {}\nscope: {scope}\nreason: {reason}\nimport-hops: 0\ncontent:\n", sanitize_metadata(&label)));
                            sources.push_str(&sanitize_content(&body));
                            if !sources.ends_with('\n') {
                                sources.push('\n');
                            }
                            sources.push_str(SOURCE_END);
                            sources.push('\n');
                            None
                        }
                    }
                    CachedText::TooLarge => Some("instruction_skip.too_large"),
                    CachedText::NotUtf8 => Some("instruction_skip.not_utf8"),
                    CachedText::Unreadable => Some("instruction_skip.unreadable"),
                };
                if let Some(key) = reason_skipped {
                    skipped.push(format!("{}: {}", sanitize_metadata(&label), text(key)));
                }
            }
        }
    }
    let mut out = String::new();
    if count > 0 {
        out.push_str(PROMPT_START);
        out.push('\n');
        let banner = text("project_memory.untrusted_banner");
        if !banner.trim().is_empty() {
            out.push_str(banner);
            out.push('\n');
        }
        out.push_str(&format!("source-count: {count}\n"));
        out.push_str(&sources);
        out.push_str(PROMPT_END);
    }
    if !skipped.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&render("host_notice.instruction_skips", &[("files", &skipped.join("\n"))]));
    }
    out
}

// ---------------------------------------------------------------- the system prompt

fn push_section(prompt: &mut String, section: &str) {
    if section.trim().is_empty() {
        return;
    }
    if !prompt.trim().is_empty() {
        prompt.push_str(SECTION_SEPARATOR);
    }
    prompt.push_str(section);
}

fn child_addendum(offered: Offered<'_>) -> String {
    let mut addendum = resolve(text("subagent.addendum"), offered).into_owned();
    if !addendum.is_empty() && offered.has("@shell") {
        let note = text("subagent.addendum.shell_note");
        if !note.is_empty() && !addendum.contains(note) {
            addendum.push_str("\n- ");
            addendum.push_str(note);
        }
    }
    addendum
}

/// Runs before every request and heads the prompt cache, so the stable sections come first and
/// what moves in a conversation (plan mode) comes last. Nothing finer than a date goes in.
pub fn system_prompt(input: &PromptInput<'_>) -> String {
    let offered = Offered(input.offered);
    let mut prompt = String::new();
    push_section(&mut prompt, &environment_section(input.workspaces));
    if TASK_PRODUCERS.iter().any(|tool| offered.has(tool)) {
        push_section(&mut prompt, text("system.host_messages"));
    }
    if offered.has("skill")
        && let Some(listing) = tools::skill_listing(input.workspaces)
    {
        push_section(&mut prompt, &listing);
    }
    push_section(&mut prompt, &project_instructions(input.workspaces));
    let global = input.global_memory.filter(|_| offered.has("read_global_memory"));
    let projects: &[PathBuf] = if offered.has("read_project_memory") { input.workspaces } else { &[] };
    if let Some(memory) = tools::memory_context(global, projects) {
        push_section(&mut prompt, &memory);
    }
    let tail: Vec<&str> = [input.global_instructions, input.instructions].into_iter().map(str::trim).filter(|s| !s.is_empty()).collect();
    push_section(&mut prompt, &tail.join("\n\n"));
    if input.is_child {
        push_section(&mut prompt, &child_addendum(offered));
    } else if input.plan_mode {
        push_section(&mut prompt, resolve(text("system.plan_mode"), offered).trim());
    }
    prompt
}

// ---------------------------------------------------------------- tool specs

fn schemas() -> &'static HashMap<(String, String), Value> {
    static SCHEMAS_BY_NAME: OnceLock<HashMap<(String, String), Value>> = OnceLock::new();
    SCHEMAS_BY_NAME.get_or_init(|| {
        let file: Value = serde_json::from_str(SCHEMAS).expect("the built-in tool schemas are valid JSON");
        let mut map = HashMap::new();
        for entry in file["tools"].as_array().into_iter().flatten() {
            let name = entry["name"].as_str().unwrap_or_default().to_owned();
            let variant = entry["variant"].as_str().unwrap_or("standard").to_owned();
            map.insert((name, variant), entry["schema"].clone());
        }
        map
    })
}

/// Which form of a tool this engine offers. `read` is always the text form: no image reaches a tool result yet.
fn variant_of(tool: &str, is_child: bool) -> &'static str {
    match tool {
        "read" => "text_only",
        "zsh" | "bash" | "sh" | "pwsh" | "powershell" | "task_wait" if is_child => "child",
        _ => "standard",
    }
}

fn root_description(tool: &str, variant: &str) -> Option<&'static str> {
    if tool == "skill" {
        return Some(text("skill.tool_description"));
    }
    if variant != "standard"
        && let Some(found) = profile().get(&format!("tool.{tool}.{variant}.description"))
    {
        return Some(found);
    }
    profile().get(&format!("tool.{tool}.description")).map(String::as_str)
}

fn param_key_exists(key: String) -> Option<&'static str> {
    profile().get(&key).map(String::as_str)
}

fn param_description(tool: &str, variant: &str, path: &str) -> Option<String> {
    let snake = path.replace("multiSelect", "multi_select");
    let own = (variant != "standard")
        .then(|| param_key_exists(format!("tool.{tool}.{variant}.param.{snake}")))
        .flatten()
        .or_else(|| param_key_exists(format!("tool.{tool}.param.{snake}")));
    if let Some(found) = own {
        return Some(found.to_owned());
    }
    if SHELL_TOOLS.contains(&tool) {
        if variant == "child"
            && let Some(found) = param_key_exists(format!("tool.shell.child.param.{snake}"))
        {
            return Some(found.to_owned());
        }
        let found = param_key_exists(format!("tool.shell.param.{snake}"))?;
        return Some(render_template(found, &[("default_ms", "120000"), ("max_ms", "600000")]));
    }
    let family = if tool.starts_with("create_") {
        "create"
    } else if tool.starts_with("edit_") {
        "edit"
    } else if tool.starts_with("read_") {
        "read"
    } else {
        return None;
    };
    tool.ends_with("_memory").then(|| param_key_exists(format!("tool.memory.param.{family}_{snake}"))).flatten().map(str::to_owned)
}

/// Replaces every description in a schema with its profile text, resolved against the offered
/// tools. The baseline file has them rendered for a run that offers everything, so its own words can't be used.
fn apply_descriptions(node: &mut Value, tool: &str, variant: &str, path: &str, offered: Offered<'_>) {
    let Some(map) = node.as_object_mut() else { return };
    if !path.is_empty()
        && let Some(found) = param_description(tool, variant, path)
    {
        let resolved = resolve(&found, offered).into_owned();
        if resolved.is_empty() {
            map.remove("description");
        } else {
            map.insert("description".into(), Value::String(resolved));
        }
    }
    if let Some(Value::Object(properties)) = map.get_mut("properties") {
        for (name, child) in properties.iter_mut() {
            let joined = if path.is_empty() { name.clone() } else { format!("{path}.{name}") };
            apply_descriptions(child, tool, variant, &joined, offered);
        }
    }
    if let Some(items) = map.get_mut("items") {
        if items.get("properties").is_some() {
            apply_descriptions(items, tool, variant, "", offered);
            // The item object's own properties are named under the array's path.
            if let Some(Value::Object(properties)) = items.get_mut("properties") {
                for (name, child) in properties.iter_mut() {
                    apply_descriptions(child, tool, variant, &format!("{path}.{name}"), offered);
                }
            }
        } else {
            apply_descriptions(items, tool, variant, &format!("{path}.item"), offered);
        }
    }
    if let Some(Value::Object(inner)) = map.get_mut("additionalProperties")
        && let Some(Value::Object(properties)) = inner.get_mut("properties")
    {
        for (name, child) in properties.iter_mut() {
            apply_descriptions(child, tool, variant, &format!("{path}.{name}"), offered);
        }
    }
}

fn add_workspace_parameter(properties: &mut Map<String, Value>, tool: &str, workspaces: usize) {
    let takes_one = matches!(tool, "ls" | "grep" | "find" | "read" | "write" | "edit") || SHELL_TOOLS.contains(&tool);
    let project_memory = matches!(tool, "read_project_memory" | "create_project_memory" | "edit_project_memory");
    if workspaces < 2 || !(takes_one || project_memory) {
        return;
    }
    let description = text(if project_memory { "tool.param.workspace_project_memory" } else { "tool.param.workspace" });
    properties.insert("workspace".into(), json!({"type": "integer", "enum": (1..=workspaces).collect::<Vec<_>>(), "default": 1, "description": description}));
}

pub fn tool_specs(offered: &[String], is_child: bool) -> Vec<ToolSpec> {
    tool_specs_for(offered, is_child, 1)
}

/// [`tool_specs`] for a conversation with this many workspaces: past one, the tools that act
/// somewhere say which by number, as the Environment section tells the model they do.
pub fn tool_specs_for(offered: &[String], is_child: bool, workspaces: usize) -> Vec<ToolSpec> {
    let set = Offered(offered);
    offered
        .iter()
        .filter_map(|name| {
            let variant = variant_of(name, is_child);
            let schema = schemas().get(&(name.clone(), variant.to_owned())).or_else(|| schemas().get(&(name.clone(), "standard".to_owned())))?;
            let mut parameters = schema.clone();
            apply_descriptions(&mut parameters, name, variant, "", set);
            let map = parameters.as_object_mut()?;
            map.remove("description");
            if let Some(Value::Object(properties)) = map.get_mut("properties") {
                add_workspace_parameter(properties, name, workspaces);
            }
            let description = root_description(name, variant).map_or(String::new(), |d| resolve(d, set).into_owned());
            Some(ToolSpec { name: name.clone(), description, parameters })
        })
        .collect()
}

pub fn compaction_prompt() -> String {
    "Summarise this conversation so that work can continue from the summary alone: the goal, decisions made, files changed, and what remains to do.".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn input<'a>(workspaces: &'a [PathBuf], offered: &'a [String]) -> PromptInput<'a> {
        PromptInput { workspaces, plan_mode: false, offered, instructions: "", global_instructions: "", is_child: false, global_memory: None }
    }

    #[test]
    fn templates_substitute_once_and_keep_what_they_do_not_know() {
        assert_eq!(render_template("a {x} b {y} c", &[("x", "{y}"), ("y", "Y")]), "a {y} b Y c");
        assert_eq!(render_template("{unknown} {x}", &[("x", "1")]), "{unknown} 1");
        assert_eq!(render_template("json {\"k\": 1} {x}", &[("x", "1")]), "json {\"k\": 1} 1");
        assert_eq!(render_template("open { brace {}", &[]), "open { brace {}");
    }

    #[test]
    fn conditions_keep_or_drop_nested_segments() {
        let offered = names(&["read", "zsh"]);
        let set = Offered(&offered);
        assert_eq!(resolve("a{?read} b{/} c", set), "a b c");
        assert_eq!(resolve("a{?grep} b{/} c", set), "a c");
        assert_eq!(resolve("a{!grep} b{/} c", set), "a b c");
        assert_eq!(resolve("a{!read} b{/} c", set), "a c");
        assert_eq!(resolve("{?grep|read}x{/}", set), "x");
        assert_eq!(resolve("{?@shell}s{/}{!@shell}n{/}", set), "s");
        assert_eq!(resolve("{?read}{?grep}in{/}out{/}", set), "out");
        assert_eq!(resolve("{?grep}{?read}in{/}out{/}", set), "");
        assert_eq!(resolve("x{/}y", set), "xy", "a stray close is dropped");
        assert_eq!(resolve("x{?read}y", set), "xy", "an unclosed segment runs to the end");
        assert_eq!(resolve("{name} {?read}{other}{/}", set), "{name} {other}", "placeholders are not markers");
    }

    #[test]
    fn a_tool_that_is_not_offered_leaves_the_descriptions() {
        let offered = names(&["task_wait", "bash"]);
        let specs = tool_specs(&offered, false);
        let wait = specs.iter().find(|s| s.name == "task_wait").unwrap();
        assert!(wait.description.contains("a background shell command exits"));
        assert!(!wait.description.contains("a child agent ends"));
        assert!(!wait.description.contains("workflow"));

        let offered = names(&["edit"]);
        let edit = &tool_specs(&offered, false)[0];
        assert!(edit.description.contains("Only a file this conversation wrote can be edited"));
        assert!(!edit.description.contains("must have been read"));
    }

    /// The baseline is what a run offering everything sees, so with everything offered the
    /// assembled specs have to reproduce it byte for byte, whichever keys the walk went through.
    #[test]
    fn with_everything_offered_the_specs_are_the_baseline() {
        let file: Value = serde_json::from_str(SCHEMAS).unwrap();
        let mut all: Vec<String> = file["tools"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_owned()).collect();
        all.dedup();
        for tool in [
            "ls",
            "grep",
            "find",
            "read",
            "write",
            "edit",
            "zsh",
            "bash",
            "web_fetch",
            "web_search",
            "agent_spawn",
            "task_wait",
            "task_list",
            "task_stop",
            "send_message",
            "read_global_memory",
            "read_project_memory",
            "create_global_memory",
            "create_project_memory",
            "edit_global_memory",
            "edit_project_memory",
            "skill",
            "ask_user",
            "plan",
            "exit_plan_mode",
        ] {
            for (is_child, expect_variant) in [(false, variant_of(tool, false)), (true, variant_of(tool, true))] {
                let spec = tool_specs(&all, is_child).into_iter().find(|s| s.name == tool).unwrap();
                let baseline = schemas().get(&(tool.to_owned(), expect_variant.to_owned())).unwrap_or_else(|| &schemas()[&(tool.to_owned(), "standard".to_owned())]);
                let mut expected = baseline.clone();
                let root = expected.as_object_mut().unwrap().remove("description").and_then(|d| d.as_str().map(str::to_owned)).unwrap_or_default();
                assert_eq!(spec.description, root, "{tool} root");
                assert_eq!(spec.parameters, expected, "{tool} parameters");
            }
        }
    }

    #[test]
    fn a_child_gets_the_child_forms_and_read_is_text_only() {
        let offered = names(&["zsh", "task_wait", "read"]);
        let child = tool_specs(&offered, true);
        let zsh = child.iter().find(|s| s.name == "zsh").unwrap();
        assert!(zsh.parameters["properties"]["run_in_background"]["description"].as_str().unwrap().contains("when you give your final reply"));
        let read = child.iter().find(|s| s.name == "read").unwrap();
        assert!(read.description.contains("does not take images"));
        let top = tool_specs(&offered, false);
        let wait = top.iter().find(|s| s.name == "task_wait").unwrap();
        assert!(wait.parameters["properties"]["timeout_seconds"]["description"].as_str().unwrap().contains("delivered on its own"));
    }

    #[test]
    fn several_workspaces_add_the_number_where_a_call_acts() {
        let offered = names(&["read", "task_list", "read_project_memory"]);
        assert!(tool_specs_for(&offered, false, 1).iter().all(|s| s.parameters["properties"].get("workspace").is_none()));
        let specs = tool_specs_for(&offered, false, 3);
        let of = |n: &str| specs.iter().find(|s| s.name == n).unwrap();
        assert_eq!(of("read").parameters["properties"]["workspace"]["enum"], json!([1, 2, 3]));
        assert!(of("read_project_memory").parameters["properties"]["workspace"]["description"].as_str().unwrap().contains("project memory"));
        assert!(of("task_list").parameters["properties"].get("workspace").is_none());
    }

    #[test]
    fn empty_descriptions_are_left_out() {
        let specs = tool_specs(&names(&["ls", "read"]), false);
        let ls = specs.iter().find(|s| s.name == "ls").unwrap();
        assert_eq!(ls.description, "");
        assert!(ls.parameters["properties"]["path"].get("description").is_none());
    }

    #[test]
    fn the_prompt_is_byte_stable_while_nothing_changes() {
        let dir = std::env::temp_dir().join(format!("mewrk-prompt-{}", crate::model::new_id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "be careful\n").unwrap();
        let workspaces = vec![dir.clone()];
        let offered = names(&["zsh", "read", "read_project_memory"]);
        let first = system_prompt(&input(&workspaces, &offered));
        for _ in 0..3 {
            assert_eq!(system_prompt(&input(&workspaces, &offered)), first);
        }
        assert!(first.starts_with("# Environment\n - Primary working directory: "));
        assert!(first.contains(" - Is a git repository: true"));
        assert!(first.contains(" - Platform: "));
        assert!(first.contains(" - Today's date: "));
        assert!(first.contains("\n\n---\n\n# Messages from Mewrk"));
        assert!(first.contains("path: AGENTS.md\nscope: workspace\nreason: startup-hierarchy"));
        assert!(first.contains("be careful"));
        // Editing the file is a change, and shows at the next request.
        std::fs::write(dir.join("AGENTS.md"), "be very careful, and longer\n").unwrap();
        let changed = system_prompt(&input(&workspaces, &offered));
        assert!(changed.contains("be very careful"));
        assert_ne!(changed, first);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sections_come_in_mewrk_order_with_the_conversation_tail_after_them() {
        let dir = std::env::temp_dir().join(format!("mewrk-prompt-{}", crate::model::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "project rule\n").unwrap();
        let workspaces = vec![dir.clone()];
        let offered = names(&["zsh", "ask_user"]);
        let prompt = system_prompt(&PromptInput {
            workspaces: &workspaces,
            plan_mode: true,
            offered: &offered,
            instructions: "conv text",
            global_instructions: "global text",
            is_child: false,
            global_memory: None,
        });
        let at = |needle: &str| prompt.find(needle).unwrap_or_else(|| panic!("{needle} missing"));
        assert!(at("# Environment") < at("# Messages from Mewrk"));
        assert!(at("# Messages from Mewrk") < at("project rule"));
        assert!(at("project rule") < at("global text"));
        assert!(at("global text\n\nconv text") < at("# Plan mode"));
        assert!(prompt.contains("Use `ask_user` only to clarify"));

        let child = system_prompt(&PromptInput {
            workspaces: &workspaces,
            plan_mode: true,
            offered: &offered,
            instructions: "",
            global_instructions: "",
            is_child: true,
            global_memory: None,
        });
        assert!(child.contains("You are a child agent"), "{child}");
        assert!(child.contains("still running when you give your final reply"), "the shell note follows the shell tool");
        assert!(!child.contains("# Plan mode"), "plan mode is the conversation's, not a child's");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn one_workspace_lists_no_numbers_and_two_do() {
        let workspaces = vec![PathBuf::from("/nonexistent-a"), PathBuf::from("/nonexistent-b")];
        let offered = names(&["read"]);
        let two = system_prompt(&input(&workspaces, &offered));
        assert!(two.contains("  - Workspace 2: /nonexistent-b (this machine)"));
        let one = system_prompt(&input(&workspaces[..1], &offered));
        assert!(!one.contains("Workspace 1:"));
        assert!(!one.contains("# Messages from Mewrk"), "nothing offered can start a background task");
    }

    #[test]
    fn instructions_are_bounded_and_links_out_of_the_project_stay_out() {
        let dir = std::env::temp_dir().join(format!("mewrk-prompt-{}", crate::model::new_id()));
        let root = dir.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("AGENTS.md"), vec![b'x'; (INSTRUCTION_FILE_LIMIT + 1) as usize]).unwrap();
        std::fs::write(dir.join("secret.txt"), "hunter2\n").unwrap();
        std::os::unix::fs::symlink(dir.join("secret.txt"), root.join("CLAUDE.md")).unwrap();
        std::fs::write(root.join("MEWRK.md"), "fine\n").unwrap();
        let out = project_instructions(&[root]);
        assert!(!out.contains("hunter2"));
        assert!(out.contains("fine"));
        assert!(out.contains("AGENTS.md: larger than the 256 KiB"), "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
