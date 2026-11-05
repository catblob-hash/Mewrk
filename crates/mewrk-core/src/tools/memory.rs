//! Memory, as plain files: each tier a folder of Markdown documents and a `MEMORY.md` index the host keeps. And `skill`, which is a folder too.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use super::fs::atomic_write;
use super::{Tool, ToolCtx, ToolFuture, ToolResult, blocking, future, truncate_output, workspace_root};
use crate::model::PermissionMode;
use crate::prompt::{CachedText, cached_text, render, text};

const INDEX_NAME: &str = "MEMORY.md";
const MAX_DOCUMENT_BYTES: usize = 256 * 1024;
const MAX_INDEX_BYTES: usize = 64 * 1024;
const MAX_NAME_CHARS: usize = 120;
const MAX_DESCRIPTION_CHARS: usize = 300;
const SKILL_MANIFEST: &str = "SKILL.md";
const SKILL_READ_LIMIT: u64 = 256 * 1024;
const SKILL_TRIGGER_LIMIT: usize = 1024;
const MEMORY_START: &str = "<mewrk-memory>";
const MEMORY_END: &str = "</mewrk-memory>";

/// Reading, changing and rewriting a document and its index belong together.
static MUTATION: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Global,
    Project,
}

impl Tier {
    fn key(self) -> &'static str {
        match self {
            Self::Global => "memory.tier.global",
            Self::Project => "memory.tier.project",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Global => "global memory",
            Self::Project => "project memory",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verb {
    Read,
    Create,
    Edit,
}

pub(super) fn register(v: &mut Vec<Arc<dyn Tool>>) {
    for (name, tier, verb) in [
        ("read_global_memory", Tier::Global, Verb::Read),
        ("read_project_memory", Tier::Project, Verb::Read),
        ("create_global_memory", Tier::Global, Verb::Create),
        ("create_project_memory", Tier::Project, Verb::Create),
        ("edit_global_memory", Tier::Global, Verb::Edit),
        ("edit_project_memory", Tier::Project, Verb::Edit),
    ] {
        v.push(Arc::new(MemoryTool { name, tier, verb }));
    }
    v.push(Arc::new(SkillTool));
}

fn project_memory_dir(workspace: &Path) -> PathBuf {
    workspace.join(".mewrk").join("memory")
}

/// `notes`, `notes.md` and `Notes.MD`'s stem all name one document. Anything that could leave the folder is rejected rather than cleaned up, so a
/// rejected name never silently becomes a different one.
fn normalize_document_name(raw: &str) -> Result<String, String> {
    let noun = "Memory document";
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{noun} name must not be empty"));
    }
    if trimmed.chars().count() > MAX_NAME_CHARS {
        return Err(format!("{noun} name must not exceed {MAX_NAME_CHARS} characters"));
    }
    let stem = match trimmed.len().checked_sub(3) {
        Some(cut) if trimmed.is_char_boundary(cut) && trimmed[cut..].eq_ignore_ascii_case(".md") => &trimmed[..cut],
        _ => trimmed,
    };
    if stem.is_empty() {
        return Err(format!("{noun} name must not be empty"));
    }
    if stem.contains('/') || stem.contains('\\') {
        return Err(format!("{noun} name must be a single file name, without path separators"));
    }
    if stem.contains(':') {
        return Err(format!("{noun} name must not contain a drive or data-stream separator"));
    }
    if stem.contains('\0') {
        return Err(format!("{noun} name must not contain a NUL character"));
    }
    if stem.starts_with('.') {
        return Err(format!("{noun} name must not start with a dot"));
    }
    if stem.chars().any(char::is_control) {
        return Err(format!("{noun} name must not contain control characters"));
    }
    if stem.ends_with('.') || stem.ends_with(' ') {
        return Err(format!("{noun} name must not end with a dot or space"));
    }
    if stem.chars().any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
        return Err(format!("{noun} name must not contain reserved characters: < > \" | ? *"));
    }
    let normalized = format!("{stem}.md");
    if normalized.eq_ignore_ascii_case(INDEX_NAME) {
        return Err(format!(
            "{INDEX_NAME} is the host-managed memory index and cannot be read or written directly; provide its description when creating or editing memory"
        ));
    }
    Ok(normalized)
}

fn normalize_description(raw: &str) -> Result<String, String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err("Memory index description must not be empty; describe what this memory records in one sentence".to_owned());
    }
    if collapsed.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(format!(
            "Memory index description must not exceed {MAX_DESCRIPTION_CHARS} characters; it is only an index entry, so put the full text in the memory document"
        ));
    }
    Ok(collapsed)
}

fn validate_content(content: &str) -> Result<(), String> {
    if content.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("Memory document exceeds the {MAX_DOCUMENT_BYTES}-byte limit; split it into multiple memories"));
    }
    if content.contains('\0') {
        return Err("Memory document must not contain a NUL character".to_owned());
    }
    Ok(())
}

/// A file that is really there. A link in the memory folder would have a model read or write wherever it points.
fn plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

/// Unrecognised lines are skipped, so a hand-edited index degrades to the entries still well-formed.
fn parse_index(content: &str) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for line in content.lines() {
        let Some(rest) = line.trim().strip_prefix("- [") else { continue };
        let Some((label, rest)) = rest.split_once("](") else { continue };
        let Some((_, rest)) = rest.split_once(')') else { continue };
        let Ok(name) = normalize_document_name(label) else { continue };
        let description = rest.trim_start().trim_start_matches('—').trim_start_matches('-').trim().to_owned();
        if !entries.iter().any(|(n, _)| *n == name) {
            entries.push((name, description));
        }
    }
    entries
}

/// A document deleted outside Mewrk leaves the index at once, without waiting for a write to rewrite the file.
fn read_index(dir: &Path) -> Vec<(String, String)> {
    let path = dir.join(INDEX_NAME);
    if !plain_file(&path) {
        return Vec::new();
    }
    match cached_text(&path, MAX_INDEX_BYTES as u64) {
        Some(CachedText::Text(content)) => {
            let mut entries = parse_index(&content);
            entries.retain(|(name, _)| plain_file(&dir.join(name)));
            entries
        }
        _ => Vec::new(),
    }
}

fn render_index(tier: Tier, entries: &[(String, String)]) -> String {
    let mut out = format!("# {} index\n\n", tier.label());
    if entries.is_empty() {
        out.push_str("(no memory documents yet)\n");
        return out;
    }
    out.push_str("<!-- Maintained automatically by Mewrk: descriptions supplied when memory is created or edited are recorded here. -->\n\n");
    for (name, description) in entries {
        out.push_str(&format!("- [{name}]({name})"));
        if !description.is_empty() {
            out.push_str(&format!(" — {description}"));
        }
        out.push('\n');
    }
    out
}

fn upsert_index(dir: &Path, tier: Tier, name: &str, description: &str) -> Result<(), String> {
    let mut entries = read_index(dir);
    match entries.iter_mut().find(|(n, _)| n == name) {
        Some(existing) => existing.1 = description.to_owned(),
        None => entries.push((name.to_owned(), description.to_owned())),
    }
    let rendered = render_index(tier, &entries);
    if rendered.len() > MAX_INDEX_BYTES {
        return Err(format!("Memory index exceeds the {MAX_INDEX_BYTES}-byte limit; delete or merge some memory documents first"));
    }
    atomic_write(&dir.join(INDEX_NAME), rendered.as_bytes()).map_err(|_| "Could not write the memory index".to_owned())
}

fn read_document(dir: &Path, tier: Tier, raw_name: &str) -> Result<(String, String), String> {
    let name = normalize_document_name(raw_name)?;
    let path = dir.join(&name);
    let missing = || format!("No memory document named {name} exists in {}", tier.label());
    if !plain_file(&path) {
        return Err(missing());
    }
    let bytes = fs::read(&path).map_err(|_| missing())?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(missing());
    }
    let content = String::from_utf8(bytes).map_err(|_| format!("Memory document {name} is not valid UTF-8 text"))?;
    Ok((name, content))
}

fn create_document(dir: &Path, tier: Tier, raw_name: &str, content: &str, raw_description: &str) -> Result<String, String> {
    let name = normalize_document_name(raw_name)?;
    let description = normalize_description(raw_description)?;
    validate_content(content)?;
    let _lock = MUTATION.lock();
    let path = dir.join(&name);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(format!("{name} already exists in {}; use the edit-memory tool or choose another name", tier.label()));
    }
    fs::create_dir_all(dir).map_err(|e| format!("Could not create the memory directory: {e}"))?;
    atomic_write(&path, content.as_bytes()).map_err(|_| format!("Could not write memory document {name}"))?;
    // A document the index does not mention is invisible to the next run, so an index failure takes the document back.
    if let Err(e) = upsert_index(dir, tier, &name, &description) {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(name)
}

fn edit_document(dir: &Path, tier: Tier, raw_name: &str, old_text: &str, new_text: &str, raw_description: &str) -> Result<String, String> {
    let name = normalize_document_name(raw_name)?;
    let description = normalize_description(raw_description)?;
    if old_text.is_empty() {
        return Err("Text to replace must not be empty; use the create-memory tool for a new memory".to_owned());
    }
    if old_text == new_text {
        return Err("The old and replacement text are identical; there is no change to write".to_owned());
    }
    let _lock = MUTATION.lock();
    let (_, content) = read_document(dir, tier, &name)?;
    let occurrences = content.matches(old_text).count();
    if occurrences == 0 {
        return Err(format!("Could not find the text to replace in memory document {name}"));
    }
    if occurrences > 1 {
        return Err(format!("The text to replace occurs {occurrences} times in memory document {name}; provide a longer unique match"));
    }
    let content = content.replacen(old_text, new_text, 1);
    validate_content(&content)?;
    upsert_index(dir, tier, &name, &description)?;
    atomic_write(&dir.join(&name), content.as_bytes()).map_err(|_| format!("Could not write memory document {name}"))?;
    Ok(name)
}

fn text_argument(args: &Value, field: &str) -> Result<String, String> {
    match args.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Null) | None => Err(format!("Missing argument: {field}")),
        Some(_) => Err(format!("Argument {field} must be a string")),
    }
}

struct MemoryTool {
    name: &'static str,
    tier: Tier,
    verb: Verb,
}

impl Tool for MemoryTool {
    fn name(&self) -> &'static str {
        self.name
    }
    fn dangerous(&self, _: &Value) -> bool {
        // Global memory is shared by every project, which is what makes writing it a question for the user.
        self.tier == Tier::Global && self.verb != Verb::Read
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        self.verb == Verb::Read
    }
    fn summary(&self, args: &Value) -> String {
        args.get("name").and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        let (tier, verb) = (self.tier, self.verb);
        future(async move {
            // Not dangerous, so the gate lets it by; a read-only conversation means it all the same.
            if verb != Verb::Read && ctx.spec.settings.permission == PermissionMode::ReadOnly {
                return Err("This conversation is read-only, so this tool was refused.".to_owned());
            }
            let dir = match tier {
                Tier::Global => ctx.engine.store.dir().join("memory"),
                Tier::Project => project_memory_dir(&workspace_root(&ctx, &args)?),
            };
            let name = text_argument(&args, "name")?;
            let tier_name = text(tier.key());
            match verb {
                Verb::Read => {
                    let (_, content) = blocking(move || read_document(&dir, tier, &name)).await??;
                    Ok(ToolResult::ok(truncate_output(&content)))
                }
                Verb::Create => {
                    let content = text_argument(&args, "content")?;
                    let description = text_argument(&args, "description")?;
                    let name = blocking(move || create_document(&dir, tier, &name, &content, &description)).await??;
                    Ok(ToolResult::ok(render("memory.created", &[("tier", tier_name), ("name", &name)])))
                }
                Verb::Edit => {
                    let old_text = text_argument(&args, "old_text")?;
                    let new_text = text_argument(&args, "new_text")?;
                    let description = text_argument(&args, "description")?;
                    let name = blocking(move || edit_document(&dir, tier, &name, &old_text, &new_text, &description)).await??;
                    Ok(ToolResult::ok(render("memory.updated", &[("tier", tier_name), ("name", &name)])))
                }
            }
        })
    }
}

/// The memory block of the prompt: the indexes only, since the model fetches a body by name. `None` when nothing is stored, so an empty tier costs no
/// context.
pub(crate) fn memory_context(global: Option<&Path>, projects: &[PathBuf]) -> Option<String> {
    let mut tiers: Vec<(String, String)> = Vec::new();
    if let Some(dir) = global {
        let entries = read_index(dir);
        if !entries.is_empty() {
            tiers.push((text(Tier::Global.key()).to_owned(), render_index(Tier::Global, &entries)));
        }
    }
    let several = projects.len() > 1;
    for (i, workspace) in projects.iter().enumerate() {
        let entries = read_index(&project_memory_dir(workspace));
        if entries.is_empty() {
            continue;
        }
        let name = if several {
            render("memory.tier.project_of_workspace", &[("workspace", &(i + 1).to_string()), ("path", &workspace.to_string_lossy())])
        } else {
            text(Tier::Project.key()).to_owned()
        };
        tiers.push((name, render_index(Tier::Project, &entries)));
    }
    if tiers.is_empty() {
        return None;
    }
    let mut out = format!("{MEMORY_START}\n{}\n", text("memory.context_intro"));
    for (tier, index) in tiers {
        out.push('\n');
        out.push_str(&render("memory.index_heading", &[("tier", &tier)]));
        out.push_str("\n\n");
        out.push_str(index.trim_end());
        out.push('\n');
    }
    out.push_str(MEMORY_END);
    Some(out)
}

// ---------------------------------------------------------------- skills

pub(super) fn user_skills_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join(".mewrk").join("skills"))
}

fn directory_name_is_valid(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && !name.chars().any(|c| c.is_control() || matches!(c, '(' | ')' | ','))
}

/// Every skill folder of the two levels, by folder name. A workspace's own wins over the user's of the same name.
fn discover_skills(workspaces: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut skills = BTreeMap::new();
    let mut scan = |root: PathBuf| {
        let Ok(entries) = fs::read_dir(root) else { return };
        for entry in entries.flatten() {
            let directory = entry.path();
            // A symlinked folder or manifest would delegate what enters the model's context to whoever owns its target.
            if !fs::symlink_metadata(&directory).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let manifest = directory.join(SKILL_MANIFEST);
            if directory_name_is_valid(&name) && plain_file(&manifest) {
                skills.insert(name, manifest);
            }
        }
    };
    if let Some(root) = user_skills_root() {
        scan(root);
    }
    for workspace in workspaces.iter().rev() {
        scan(workspace.join(".mewrk").join("skills"));
    }
    skills
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn raw_value(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['\'', '"']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn fold_lines(lines: &[&str]) -> String {
    let mut folded = String::new();
    let mut blank_lines = 0;
    let mut previous_plain = None;
    for line in lines {
        if line.is_empty() {
            blank_lines += 1;
            continue;
        }
        let plain = !line.starts_with([' ', '\t']);
        match previous_plain {
            None => folded.push_str(&"\n".repeat(blank_lines)),
            Some(true) if plain => {
                if blank_lines == 0 {
                    folded.push(' ');
                } else {
                    folded.push_str(&"\n".repeat(blank_lines));
                }
            }
            Some(_) => folded.push_str(&"\n".repeat(blank_lines + 1)),
        }
        folded.push_str(line);
        previous_plain = Some(plain);
        blank_lines = 0;
    }
    folded
}

/// One frontmatter value: what follows the colon, plus the lines under it. `|` keeps lines, `>` folds them, and anything else continues on indented
/// lines.
fn scalar_value(inline: &str, continuation: &[&str]) -> Option<String> {
    let header = inline.trim();
    let literal = header.starts_with('|');
    if literal || header.starts_with('>') {
        let indicators = header[1..].chars().take_while(|c| matches!(c, '+' | '-' | '1'..='9')).count();
        let after = header[1 + indicators..].trim_start();
        if indicators <= 2 && (after.is_empty() || after.starts_with('#')) {
            let explicit = header[1..].chars().find_map(|c| c.to_digit(10)).map(|d| d as usize);
            let indent = explicit.unwrap_or_else(|| continuation.iter().find(|l| !l.trim().is_empty()).map_or(0, |l| indentation(l)));
            let lines: Vec<&str> = continuation.iter().map(|l| if l.trim().is_empty() { "" } else { &l[indentation(l).min(indent)..] }).collect();
            let value = if literal { lines.join("\n") } else { fold_lines(&lines) };
            let value = value.trim_end_matches('\n');
            return (!value.trim().is_empty()).then(|| value.to_owned());
        }
    }
    let mut value = header.to_owned();
    let mut blank_lines = 0;
    for line in continuation {
        let line = line.trim();
        if line.is_empty() {
            blank_lines += 1;
            continue;
        }
        if !value.is_empty() {
            if blank_lines > 0 {
                value.push_str(&"\n".repeat(blank_lines));
            } else {
                value.push(' ');
            }
        }
        value.push_str(line);
        blank_lines = 0;
    }
    raw_value(&value)
}

/// The two parts of a `SKILL.md` the model reads: when to use it, and the body without its frontmatter.
fn skill_document(source: &str) -> (String, String) {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lines: Vec<&str> = source.lines().collect();
    let mut description = None;
    let mut when_to_use = None;
    let mut body_start = 0;
    if lines.first().is_some_and(|l| l.trim() == "---")
        && let Some(end) = lines.iter().enumerate().skip(1).find_map(|(i, l)| (l.trim() == "---").then_some(i))
    {
        let block = &lines[1..end];
        let mut index = 0;
        while index < block.len() {
            let line = block[index];
            index += 1;
            let start = index;
            while index < block.len() && (block[index].trim().is_empty() || block[index].starts_with([' ', '\t'])) {
                index += 1;
            }
            let continuation = &block[start..index];
            if let Some(value) = line.strip_prefix("description:") {
                description = scalar_value(value, continuation);
            } else if let Some(value) = line.strip_prefix("when_to_use:") {
                when_to_use = scalar_value(value, continuation);
            }
        }
        body_start = end + 1;
    }
    let description =
        description.or_else(|| lines[body_start..].iter().map(|l| l.trim()).find(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_owned)).unwrap_or_default();
    let trigger = match when_to_use {
        Some(when) if !description.is_empty() => format!("{description} - {when}"),
        Some(when) => when,
        None => description,
    };
    (super::truncate_chars(trigger.trim(), SKILL_TRIGGER_LIMIT), lines[body_start..].join("\n").trim().to_owned())
}

/// The catalog goes in the prompt rather than the tool's schema: a schema is re-declared on every request, so a listing there would change the tool
/// set the moment another skill appeared.
pub(crate) fn skill_listing(workspaces: &[PathBuf]) -> Option<String> {
    let mut rows = Vec::new();
    for (name, manifest) in discover_skills(workspaces) {
        let Some(CachedText::Text(source)) = cached_text(&manifest, SKILL_READ_LIMIT) else { continue };
        let (trigger, _) = skill_document(&source);
        rows.push(if trigger.is_empty() {
            render("skill.listing_row_untriggered", &[("name", &name)])
        } else {
            render("skill.listing_row", &[("name", &name), ("trigger", &trigger)])
        });
    }
    if rows.is_empty() {
        return None;
    }
    Some(format!("{}\n{}", text("skill.listing_heading"), rows.join("\n")))
}

struct SkillTool;

impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        "skill"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn parallel_safe(&self, _: &Value) -> bool {
        true
    }
    fn summary(&self, args: &Value) -> String {
        args.get("name").and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            let name = args.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or("Missing parameter name")?.to_owned();
            let workspaces = ctx.workspaces.clone();
            blocking(move || {
                let skills = discover_skills(&workspaces);
                let Some(manifest) = skills.get(&name) else {
                    let available = skills.keys().cloned().collect::<Vec<_>>().join(", ");
                    return Err(if available.is_empty() {
                        format!("Skill {name} is not among this conversation's available skills")
                    } else {
                        format!("Skill {name} is not among this conversation's available skills; available: {available}")
                    });
                };
                let source = match cached_text(manifest, SKILL_READ_LIMIT) {
                    Some(CachedText::Text(source)) => source,
                    Some(CachedText::TooLarge) => return Err(format!("skill file exceeds the {} KiB limit", SKILL_READ_LIMIT / 1024)),
                    Some(CachedText::NotUtf8) => return Err(format!("{SKILL_MANIFEST} is not valid UTF-8 text")),
                    _ => return Err(format!("Could not read {SKILL_MANIFEST}")),
                };
                let (_, body) = skill_document(&source);
                let directory = manifest.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                Ok(ToolResult::ok(truncate_output(&render("skill.result", &[("directory", &directory), ("body", &body)]))))
            })
            .await?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{call, fixture};
    use super::*;
    use serde_json::json;

    #[test]
    fn document_names_are_bare_and_cannot_leave_the_folder() {
        assert_eq!(normalize_document_name("notes").unwrap(), "notes.md");
        assert_eq!(normalize_document_name("notes.MD").unwrap(), "notes.md");
        assert_eq!(normalize_document_name("世界").unwrap(), "世界.md");
        for bad in ["", ".md", "../x", "a/b", "a\\b", ".hidden", "c:x", "memory", "MEMORY.md", "a?b", "trail."] {
            assert!(normalize_document_name(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_tier_round_trips_and_keeps_its_index() {
        let f = fixture(crate::model::PermissionMode::FullAccess);
        for (tier, label, dir) in [("global", "Global memory", f.dir.join("data/memory")), ("project", "Project memory", f.dir.join("work/.mewrk/memory"))] {
            let r = call(&f.ctx, &format!("create_{tier}_memory"), json!({"name": "notes", "content": "alpha beta\n", "description": "Some  notes."})).await;
            assert_eq!(r.output, format!("Created notes.md in {label} and recorded its index description."));
            let r = call(&f.ctx, &format!("read_{tier}_memory"), json!({"name": "notes.md"})).await;
            assert_eq!(r.output, "alpha beta\n");
            let index = fs::read_to_string(dir.join("MEMORY.md")).unwrap();
            assert!(index.contains("- [notes.md](notes.md) — Some notes."), "{index}");

            let r = call(&f.ctx, &format!("create_{tier}_memory"), json!({"name": "notes", "content": "x", "description": "y"})).await;
            assert!(r.is_error && r.output.contains("already exists"), "{}", r.output);

            let r = call(&f.ctx, &format!("edit_{tier}_memory"), json!({"name": "notes", "old_text": "alpha", "new_text": "gamma", "description": "Better."})).await;
            assert_eq!(r.output, format!("Updated notes.md in {label} and refreshed its index description."));
            assert_eq!(fs::read_to_string(dir.join("notes.md")).unwrap(), "gamma beta\n");
            assert!(fs::read_to_string(dir.join("MEMORY.md")).unwrap().contains("— Better."));

            let r = call(&f.ctx, &format!("edit_{tier}_memory"), json!({"name": "notes", "old_text": "nope", "new_text": "x", "description": "y"})).await;
            assert!(r.is_error && r.output.contains("Could not find"), "{}", r.output);
            fs::write(dir.join("notes.md"), "aa aa").unwrap();
            let r = call(&f.ctx, &format!("edit_{tier}_memory"), json!({"name": "notes", "old_text": "aa", "new_text": "b", "description": "y"})).await;
            assert!(r.is_error && r.output.contains("occurs 2 times"), "{}", r.output);

            let r = call(&f.ctx, &format!("read_{tier}_memory"), json!({"name": "absent"})).await;
            assert_eq!(r.output, format!("No memory document named absent.md exists in {tier} memory"));
            let r = call(&f.ctx, &format!("read_{tier}_memory"), json!({"name": "MEMORY.md"})).await;
            assert!(r.is_error && r.output.contains("host-managed"), "{}", r.output);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_prompt_carries_the_indexes_not_the_bodies() {
        let f = fixture(crate::model::PermissionMode::FullAccess);
        let global = f.dir.join("data/memory");
        let workspaces = f.ctx.workspaces.clone();
        assert!(memory_context(Some(&global), &workspaces).is_none(), "nothing stored costs nothing");
        call(&f.ctx, "create_global_memory", json!({"name": "g", "content": "SECRET BODY", "description": "Global thing."})).await;
        call(&f.ctx, "create_project_memory", json!({"name": "p", "content": "OTHER BODY", "description": "Project thing."})).await;
        let block = memory_context(Some(&global), &workspaces).unwrap();
        assert!(block.starts_with("<mewrk-memory>\nBelow is your long-term memory."), "{block}");
        assert!(block.contains("## Global memory · MEMORY.md") && block.contains("Global thing."), "{block}");
        assert!(block.contains("## Project memory · MEMORY.md") && block.contains("Project thing."), "{block}");
        assert!(!block.contains("BODY") && block.ends_with("</mewrk-memory>"), "{block}");
        // A document removed by hand leaves the index at once.
        fs::remove_file(global.join("g.md")).unwrap();
        assert!(!memory_context(Some(&global), &workspaces).unwrap().contains("Global thing."));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_only_conversation_writes_no_memory() {
        let f = fixture(crate::model::PermissionMode::ReadOnly);
        let r = call(&f.ctx, "create_project_memory", json!({"name": "n", "content": "c", "description": "d"})).await;
        assert!(r.is_error && r.output.contains("read-only"), "{}", r.output);
        assert!(call(&f.ctx, "read_project_memory", json!({"name": "n"})).await.output.starts_with("No memory document"));
    }

    #[test]
    fn only_global_memory_writes_ask() {
        for (name, dangerous) in [
            ("create_global_memory", true),
            ("edit_global_memory", true),
            ("read_global_memory", false),
            ("create_project_memory", false),
            ("edit_project_memory", false),
            ("read_project_memory", false),
            ("skill", false),
        ] {
            assert_eq!(super::super::find(name).unwrap().dangerous(&json!({})), dangerous, "{name}");
        }
    }

    #[test]
    fn skill_documents_split_into_trigger_and_body() {
        let (trigger, body) = skill_document("---\nname: x\ndescription: Does a thing\nwhen_to_use: |\n  Often\n  always\n---\n\n# Title\n\nDo it.\n");
        assert_eq!(trigger, "Does a thing - Often\nalways");
        assert_eq!(body, "# Title\n\nDo it.");
        let (trigger, body) = skill_document("# Title\n\nFirst paragraph.\nMore.\n");
        assert_eq!(trigger, "First paragraph.");
        assert_eq!(body, "# Title\n\nFirst paragraph.\nMore.");
        let (trigger, _) = skill_document("---\ndescription: >\n  folded\n  text\n---\nbody\n");
        assert_eq!(trigger, "folded text");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_skill_is_a_folder() {
        let f = fixture(crate::model::PermissionMode::FullAccess);
        let root = f.dir.join("work/.mewrk/skills/review");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("SKILL.md"), "---\ndescription: Reviews code\n---\nRead the diff.\n").unwrap();
        let r = call(&f.ctx, "skill", json!({"name": "review"})).await;
        assert_eq!(r.output, format!("Base directory for this skill: {}\n\nRead the diff.", root.display()));
        let r = call(&f.ctx, "skill", json!({"name": "absent"})).await;
        assert!(r.is_error && r.output.contains("not among this conversation's available skills"), "{}", r.output);
        assert!(r.output.contains("review"), "{}", r.output);
        let listing = skill_listing(&f.ctx.workspaces).unwrap();
        assert!(listing.starts_with("Available skills:\n"), "{listing}");
        assert!(listing.contains("- review: Reviews code"), "{listing}");
        // A linked folder is not a skill.
        std::os::unix::fs::symlink(&root, f.dir.join("work/.mewrk/skills/linked")).unwrap();
        assert!(!skill_listing(&f.ctx.workspaces).unwrap().contains("linked"));
    }
}
