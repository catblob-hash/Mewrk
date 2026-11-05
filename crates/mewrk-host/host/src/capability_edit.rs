//! Creating and editing skills, MCP servers and hooks from the settings pane.
//!
//! All three are files the user owns, so an edit goes through what the file says
//! now: every read is of the file as written (a `${VAR}` stays a reference, a
//! value inferred for the catalog stays out of it), every save names the entry by
//! a fresh scan, refuses when the file moved since the form was opened, and
//! changes only what the form changed, leaving the rest of the file as it was.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::{
    capabilities::{
        discover_levels, is_continuation_line, level_of, parse_hook_event, parse_hook_location,
        remote_config_text, remote_level_of, skill_frontmatter_as_written, write_config_file,
        CapabilityKind, ConfigLevel, DiscoveredCapabilities, HookAddress, CONFIG_DIRECTORY,
        SKILL_MANIFEST, SKILL_READ_LIMIT,
    },
    config_file, json_edit,
    json_edit::Step,
    mcp_config,
    model::{ResolvedLanguage, ResourceDescriptor, ResourceSource},
    remote_capabilities, skills,
};

/// Where a save writes: over the entry a fresh scan lists under `id`, or a new
/// one at the level `workspace_key` names (`None` is the global `~/.mewrk`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveTarget {
    pub id: Option<String>,
    pub workspace_key: Option<String>,
}

fn moved_elsewhere() -> String {
    crate::ui_text::pick(
        "这个条目的文件已在别处变化。请关闭后重新打开再编辑。",
        "This entry's file changed elsewhere. Close and reopen it before editing.",
    )
    .to_owned()
}

/// Whether a listed entry is the one a save just wrote. By its location, or on another machine, where the
/// path a new file was given can be spelled differently from the one a scan reads back, by what it is called at that level.
fn is_saved_entry(descriptor: &ResourceDescriptor, level: &ConfigLevel, location_matches: bool, named: bool) -> bool {
    location_matches || (level.remote.is_some() && descriptor.workspace_key == level.workspace_key() && named)
}

fn not_listed_after_save(path: &str) -> String {
    crate::ui_text::ui_text!(
        "已写入 {path}，但重新扫描时没有列出它",
        "Written to {path}, but a fresh scan does not list it"
    )
}

fn parse_strict(text: &str, path: &str) -> Result<Value, String> {
    serde_json::from_str(config_file::text_without_bom(text))
        .map_err(|error| crate::ui_text::ui_text!("{path} 不是合法的 JSON：{error}", "{path} is not valid JSON: {error}"))
}

/// The configuration file of one kind at a level: where it is, or would go, and
/// what it holds now, `None` while there is none. An existing file wins whichever
/// spelling of the folder it is in, since writing a new one beside it would hide it.
fn config_file_at(level: &ConfigLevel, kind: CapabilityKind) -> Result<(String, Option<String>), String> {
    if let Some(remote) = &level.remote {
        let files = remote_capabilities::read(remote, remote_capabilities::RUN_TIMEOUT)?;
        let current = if kind == CapabilityKind::Mcp { files.mcp } else { files.hooks };
        return match current {
            Some(file) => {
                let text = String::from_utf8(file.bytes)
                    .map_err(|_| crate::ui_text::ui_text!("{} 不是 UTF-8 文本", "{} is not UTF-8 text", file.path))?;
                Ok((file.path, Some(text)))
            }
            None => {
                let directory = remote_capabilities::ensure_directory(remote, CONFIG_DIRECTORY)?;
                Ok((join_remote(&directory, kind.relative_path()), None))
            }
        };
    }
    let resolved = level.path_for(kind);
    let path = if resolved.exists() { resolved } else { level.base.join(CONFIG_DIRECTORY).join(kind.relative_path()) };
    let display = path.to_string_lossy().into_owned();
    match fs::read_to_string(&path) {
        Ok(text) => Ok((display, Some(text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((display, None)),
        Err(error) => Err(crate::ui_text::ui_text!("无法读取 {display}：{error}", "Could not read {display}: {error}")),
    }
}

fn join_remote(directory: &str, name: &str) -> String {
    let separator = if directory.contains('\\') && !directory.contains('/') { '\\' } else { '/' };
    format!("{}{separator}{name}", directory.trim_end_matches(['/', '\\']))
}

fn write_config(level: &ConfigLevel, path: &str, text: &str) -> Result<(), String> {
    if let Some(remote) = &level.remote {
        return remote_capabilities::replace_file(remote, path, text);
    }
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)
            .map_err(|error| crate::ui_text::ui_text!("无法创建 {}：{error}", "Could not create {}: {error}", parent.display()))?;
    }
    write_config_file(Path::new(path), text)
}

/// The current text of the file a listed entry was read from.
fn listed_file_text(
    levels: &[ConfigLevel],
    descriptor: &ResourceDescriptor,
    path: &Path,
    file: impl Fn(&remote_capabilities::LevelFiles) -> Option<&remote_capabilities::ConfigFile>,
) -> Result<String, String> {
    match remote_level_of(levels, descriptor) {
        Some(remote) => remote_config_text(remote, &path.to_string_lossy(), file),
        None => {
            let display = path.display();
            fs::read_to_string(path)
                .map_err(|error| crate::ui_text::ui_text!("无法读取 {display}：{error}", "Could not read {display}: {error}"))
        }
    }
}

fn set_text(entry: &mut Map<String, Value>, key: &str, value: &str) {
    if value.trim().is_empty() {
        entry.remove(key);
    } else {
        entry.insert(key.to_owned(), Value::from(value));
    }
}

fn text_of(entry: &Value, key: &str) -> String {
    entry.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------- skills

/// A `SKILL.md`, in its folder. `directory` is the name the model loads it by, so
/// it is chosen once. `name` and `description` are the frontmatter's own words,
/// empty when it does not say: a blank is the list falling back to the body.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct SkillForm {
    pub directory: String,
    pub name: String,
    pub description: String,
    pub body: String,
}

fn listed_skill(discovered: &DiscoveredCapabilities, id: &str) -> Result<ResourceDescriptor, String> {
    let descriptor = discovered
        .catalog
        .skills
        .iter()
        .find(|descriptor| descriptor.id == id)
        .cloned()
        .ok_or_else(|| crate::ui_text::ui_text!("技能 {id} 不存在", "There is no skill {id}"))?;
    if descriptor.source == ResourceSource::Builtin {
        return Err(crate::ui_text::pick("内置技能随 Mewrk 发布，不能修改", "A built-in skill ships with Mewrk and cannot be edited").into());
    }
    Ok(descriptor)
}

fn skill_source(discovered: &DiscoveredCapabilities, descriptor: &ResourceDescriptor) -> Result<String, String> {
    match discovered.remote_skills.get(&descriptor.id) {
        Some(remote) => remote.text.clone(),
        None => skills::read_manifest(Path::new(&descriptor.location)),
    }
}

fn skill_form_of(source: &str, directory: String) -> SkillForm {
    let text = config_file::text_without_bom(source);
    let lines = text.lines().collect::<Vec<_>>();
    let (name, description, body_start) = skill_frontmatter_as_written(&lines);
    SkillForm { directory, name, description, body: lines[body_start..].join("\n").trim().to_owned() }
}

pub(crate) fn read_skill_form(levels: &[ConfigLevel], language: ResolvedLanguage, id: &str) -> Result<SkillForm, String> {
    let discovered = discover_levels(levels, language);
    let descriptor = listed_skill(&discovered, id)?;
    Ok(skill_form_of(&skill_source(&discovered, &descriptor)?, skills::directory_name_of(&descriptor)))
}

/// Whether a value can sit after `key: ` and read back as itself, here and in
/// Claude Code's YAML parser. Anything else goes in a block scalar, which both read.
fn plain_scalar(value: &str) -> bool {
    !value.contains('\n')
        && !value.contains(": ")
        && !value.contains(" #")
        && !value.ends_with([':', '\'', '"'])
        && !value.starts_with(['!', '&', '*', '-', '?', '|', '>', '\'', '"', '%', '@', '`', '[', ']', '{', '}', ',', '#', ':', '~'])
        && !matches!(value.to_ascii_lowercase().as_str(), "true" | "false" | "null" | "yes" | "no" | "on" | "off")
        && value.parse::<f64>().is_err()
}

fn frontmatter_lines(key: &str, value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        Vec::new()
    } else if plain_scalar(value) {
        vec![format!("{key}: {value}")]
    } else {
        std::iter::once(format!("{key}: |-"))
            .chain(value.lines().map(|line| if line.is_empty() { String::new() } else { format!("  {line}") }))
            .collect()
    }
}

/// Puts one key's value in a frontmatter block: where it already is, with the
/// lines under it, or at the end. An empty value takes the key out.
fn set_frontmatter_key(block: &mut Vec<String>, key: &str, value: &str) {
    let prefix = format!("{key}:");
    let mut index = 0;
    while index < block.len() {
        let start = index;
        index += 1;
        while index < block.len() && is_continuation_line(&block[index]) {
            index += 1;
        }
        if block[start].starts_with(&prefix) {
            block.splice(start..index, frontmatter_lines(key, value));
            return;
        }
    }
    block.extend(frontmatter_lines(key, value));
}

/// `source` with what the form changed. A field left alone keeps its spelling,
/// every other frontmatter key stays, and so do the byte-order mark and line endings.
fn edited_manifest(source: &str, form: &SkillForm, was: &SkillForm) -> String {
    let (bom, text) = config_file::split_bom(source);
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines = text.lines().collect::<Vec<_>>();
    let (_, _, body_start) = skill_frontmatter_as_written(&lines);
    let mut block = if body_start > 0 { lines[1..body_start - 1].iter().map(|line| (*line).to_owned()).collect() } else { Vec::new() };
    if form.name != was.name {
        set_frontmatter_key(&mut block, "name", &form.name);
    }
    if form.description != was.description {
        set_frontmatter_key(&mut block, "description", &form.description);
    }
    let body = if form.body == was.body { lines[body_start..].join(newline) } else { format!("{newline}{}", form.body.trim().replace('\n', newline)) };
    let body = body.trim_end();
    let mut out = String::from(bom);
    if block.is_empty() {
        out.push_str(body.trim_start());
    } else {
        out.push_str(&format!("---{newline}{}{newline}---{newline}", block.join(newline)));
        // A frontmatter added above a body that had none gets the blank line one written by hand has.
        if !body.starts_with(['\r', '\n']) {
            out.push_str(newline);
        }
        out.push_str(body);
    }
    out.push_str(newline);
    out
}

fn check_skill_form(form: &SkillForm) -> Result<(), String> {
    if form.body.trim().is_empty() {
        return Err(crate::ui_text::pick("技能的正文不能为空", "A skill needs a body").into());
    }
    Ok(())
}

fn check_size(text: &str) -> Result<(), String> {
    if text.len() as u64 > SKILL_READ_LIMIT {
        let limit = SKILL_READ_LIMIT / 1024;
        return Err(crate::ui_text::ui_text!(
            "{SKILL_MANIFEST} 会超过 {limit} KiB 上限，所以没有保存",
            "{SKILL_MANIFEST} would exceed the {limit} KiB limit, so it was not saved"
        ));
    }
    Ok(())
}

/// The folder names a new skill may have: what discovery accepts, and a single
/// path component, so it can only ever land directly inside `skills/`.
pub(crate) fn new_directory_name_is_valid(name: &str) -> bool {
    skills::directory_name_is_valid(name) && !name.contains(['/', '\\']) && !matches!(name, "." | "..")
}

pub(crate) fn save_skill(levels: &[ConfigLevel], language: ResolvedLanguage, target: &SaveTarget, form: &SkillForm, original: Option<&SkillForm>) -> Result<String, String> {
    check_skill_form(form)?;
    let location = match &target.id {
        Some(id) => edit_skill(levels, language, id, form, original)?,
        None => create_skill(level_of(levels, target.workspace_key.as_deref())?, form)?,
    };
    let level = level_of(levels, target.workspace_key.as_deref());
    let discovered = discover_levels(levels, language);
    discovered
        .catalog
        .skills
        .iter()
        .find(|descriptor| {
            descriptor.location == location
                || (target.id.is_none() && level.as_ref().is_ok_and(|level| is_saved_entry(descriptor, level, false, skills::directory_name_of(descriptor) == form.directory.trim())))
        })
        .map(|descriptor| descriptor.id.clone())
        .ok_or_else(|| not_listed_after_save(&location))
}

fn edit_skill(levels: &[ConfigLevel], language: ResolvedLanguage, id: &str, form: &SkillForm, original: Option<&SkillForm>) -> Result<String, String> {
    let discovered = discover_levels(levels, language);
    let descriptor = listed_skill(&discovered, id)?;
    let source = skill_source(&discovered, &descriptor)?;
    let was = skill_form_of(&source, skills::directory_name_of(&descriptor));
    if original.is_some_and(|original| *original != was) {
        return Err(moved_elsewhere());
    }
    let form = SkillForm { directory: was.directory.clone(), ..form.clone() };
    if form == was {
        return Ok(descriptor.location);
    }
    let edited = edited_manifest(&source, &form, &was);
    check_size(&edited)?;
    match remote_level_of(levels, &descriptor) {
        Some(remote) => remote_capabilities::replace_file(remote, &descriptor.location, &edited)?,
        None => write_config_file(Path::new(&descriptor.location), &edited)?,
    }
    Ok(descriptor.location)
}

fn taken_directory(name: &str) -> String {
    crate::ui_text::ui_text!("这里已有名为 {name} 的技能文件夹", "A skill folder named {name} already exists here")
}

fn create_skill(level: &ConfigLevel, form: &SkillForm) -> Result<String, String> {
    let directory = form.directory.trim();
    if !new_directory_name_is_valid(directory) {
        return Err(crate::ui_text::pick(
            "文件夹名不能为空，不能以空格开头或结尾，也不能包含路径分隔符、括号、逗号或控制字符",
            "A folder name cannot be empty, start or end with a space, or contain a path separator, a parenthesis, a comma or a control character",
        )
        .into());
    }
    let empty = SkillForm::default();
    let text = edited_manifest("", form, &empty);
    check_size(&text)?;
    if let Some(remote) = &level.remote {
        let relative = format!("{CONFIG_DIRECTORY}/skills/{directory}");
        let folder = remote_capabilities::ensure_directory(remote, &relative)?;
        let manifest = join_remote(&folder, SKILL_MANIFEST);
        if !remote_capabilities::create_file(remote, &manifest, &text)? {
            return Err(taken_directory(directory));
        }
        return Ok(manifest);
    }
    let resolved = level.path_for(CapabilityKind::Skills);
    let root = if resolved.exists() { resolved } else { level.base.join(CONFIG_DIRECTORY).join("skills") };
    fs::create_dir_all(&root)
        .map_err(|error| crate::ui_text::ui_text!("无法创建 {}：{error}", "Could not create {}: {error}", root.display()))?;
    let folder: PathBuf = root.join(directory);
    fs::create_dir(&folder).map_err(|error| match error.kind() {
        std::io::ErrorKind::AlreadyExists => taken_directory(directory),
        _ => crate::ui_text::ui_text!("无法创建 {}：{error}", "Could not create {}: {error}", folder.display()),
    })?;
    let manifest = folder.join(SKILL_MANIFEST);
    if let Err(error) = write_config_file(&manifest, &text) {
        let _ = fs::remove_dir_all(&folder);
        return Err(error);
    }
    Ok(manifest.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------- MCP servers

/// One `mcpServers` entry as written. Keys the form has no field for stay in the
/// file. `transport` is `stdio` or `http`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct McpServerForm {
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

const HTTP_TYPES: [&str; 3] = ["http", "streamable-http", "streamable_http"];

fn string_map_of(entry: &Value, key: &str) -> BTreeMap<String, String> {
    entry
        .get(key)
        .and_then(Value::as_object)
        .map(|values| values.iter().filter_map(|(name, value)| Some((name.clone(), value.as_str()?.to_owned()))).collect())
        .unwrap_or_default()
}

fn timeout_seconds_of(entry: &Value) -> u32 {
    match (entry.get("timeoutSeconds").and_then(Value::as_u64), entry.get("timeout").and_then(Value::as_u64)) {
        (Some(seconds), _) => u32::try_from(seconds).unwrap_or(u32::MAX),
        (None, Some(millis)) if millis >= 1_000 => u32::try_from(millis.div_ceil(1_000)).unwrap_or(u32::MAX),
        _ => 0,
    }
}

/// What a form opens on. Lenient where the file is wrong, since an entry listed as
/// unavailable is what the form is there to fix: a field of the wrong type reads
/// as blank, and a transport Mewrk cannot dial opens as the one its fields suggest.
fn mcp_form_of(name: &str, entry: &Value) -> McpServerForm {
    let kind = text_of(entry, "type");
    let http = HTTP_TYPES.contains(&kind.as_str()) || (kind != "stdio" && text_of(entry, "command").trim().is_empty() && !text_of(entry, "url").trim().is_empty());
    McpServerForm {
        name: name.to_owned(),
        description: text_of(entry, "description"),
        transport: if http { "http" } else { "stdio" }.to_owned(),
        command: text_of(entry, "command"),
        args: entry.get("args").and_then(Value::as_array).map(|args| args.iter().filter_map(|arg| arg.as_str().map(str::to_owned)).collect()).unwrap_or_default(),
        env: string_map_of(entry, "env"),
        cwd: text_of(entry, "cwd"),
        url: text_of(entry, "url"),
        headers: string_map_of(entry, "headers"),
        timeout_seconds: timeout_seconds_of(entry),
    }
}

/// The entry `current` becomes under the form. What belongs to the other
/// transport goes, so the entry says one thing.
fn merged_mcp_entry(current: Option<&Value>, form: &McpServerForm) -> Value {
    let was = current.cloned().unwrap_or_else(|| json!({}));
    let mut entry = was.as_object().cloned().unwrap_or_default();
    let string_map = |values: &BTreeMap<String, String>| Value::Object(values.iter().map(|(name, value)| (name.clone(), Value::from(value.as_str()))).collect());
    let set_map = |entry: &mut Map<String, Value>, key: &str, values: &BTreeMap<String, String>| {
        if values.is_empty() {
            entry.remove(key);
        } else {
            entry.insert(key.to_owned(), string_map(values));
        }
    };
    if form.transport == "http" {
        if !HTTP_TYPES.contains(&text_of(&was, "type").as_str()) {
            entry.insert("type".to_owned(), Value::from("http"));
        }
        for key in ["command", "args", "env", "cwd"] {
            entry.remove(key);
        }
        set_text(&mut entry, "url", form.url.trim());
        set_map(&mut entry, "headers", &form.headers);
    } else {
        entry.remove("type");
        if text_of(&was, "type") == "stdio" {
            entry.insert("type".to_owned(), Value::from("stdio"));
        }
        for key in ["url", "headers"] {
            entry.remove(key);
        }
        set_text(&mut entry, "command", form.command.trim());
        if form.args.is_empty() {
            entry.remove("args");
        } else {
            entry.insert("args".to_owned(), Value::from(form.args.clone()));
        }
        set_map(&mut entry, "env", &form.env);
        set_text(&mut entry, "cwd", form.cwd.trim());
    }
    set_text(&mut entry, "description", form.description.trim());
    if timeout_seconds_of(&was) != form.timeout_seconds {
        entry.remove("timeout");
        entry.remove("timeoutSeconds");
        if form.timeout_seconds > 0 {
            entry.insert("timeoutSeconds".to_owned(), Value::from(form.timeout_seconds));
        }
    }
    Value::Object(entry)
}

/// Refuses what could only be listed as unavailable. A `${VAR}` nothing has set
/// yet is not one of those: it can be set later, and the row will say so until it is.
fn check_mcp_entry(name: &str, entry: &Value, level: &ConfigLevel) -> Result<(), String> {
    if !mcp_config::server_name_is_valid(name) {
        return Err(crate::ui_text::pick(
            "服务器名称只能包含字母、数字、连字符和下划线",
            "A server name can contain only letters, digits, hyphens and underscores",
        )
        .into());
    }
    let local_env = |variable: &str| std::env::var(variable).ok();
    // Another machine's environment is not known here, so every reference counts as unset.
    let remote_env = |_: &str| None;
    let env: &dyn Fn(&str) -> Option<String> = if level.remote.is_some() { &remote_env } else { &local_env };
    match mcp_config::parse_entry("mcp_edit_check", name, entry, env, level.remote.as_ref()) {
        Err(reason) if !reason.starts_with(mcp_config::MISSING_VARIABLES_PREFIX) => Err(reason),
        _ => Ok(()),
    }
}

fn listed_mcp(discovered: &DiscoveredCapabilities, id: &str) -> Result<(ResourceDescriptor, PathBuf, String), String> {
    let descriptor = discovered
        .catalog
        .mcps
        .iter()
        .find(|descriptor| descriptor.id == id)
        .cloned()
        .ok_or_else(|| crate::ui_text::ui_text!("MCP 服务器 {id} 不存在", "There is no MCP server {id}"))?;
    let (path, name) = mcp_config::parse_location(&descriptor.location)
        .ok_or_else(|| crate::ui_text::ui_text!("MCP 服务器 {id} 的位置无法解析", "The location of MCP server {id} cannot be read"))?;
    Ok((descriptor, path, name))
}

pub(crate) fn read_mcp_form(levels: &[ConfigLevel], language: ResolvedLanguage, id: &str) -> Result<McpServerForm, String> {
    let (descriptor, path, name) = listed_mcp(&discover_levels(levels, language), id)?;
    let text = listed_file_text(levels, &descriptor, &path, |files| files.mcp.as_ref())?;
    let document = parse_strict(&text, &path.to_string_lossy())?;
    Ok(mcp_form_of(&name, document.get("mcpServers").and_then(|servers| servers.get(&name)).unwrap_or(&Value::Null)))
}

fn taken_server(name: &str, path: &str) -> String {
    crate::ui_text::ui_text!("{path} 里已有名为 {name} 的服务器", "{path} already has a server named {name}")
}

/// `text` with the server `name` added, or the file it starts as.
fn text_with_new_server(text: Option<&str>, path: &str, name: &str, entry: &Value) -> Result<String, String> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        let mut out = serde_json::to_string_pretty(&json!({ "mcpServers": { name: entry } })).map_err(|error| error.to_string())?;
        out.push('\n');
        return Ok(out);
    };
    let document = parse_strict(text, path)?;
    let (bom, text) = config_file::split_bom(text);
    let edited = match document.get("mcpServers") {
        None => json_edit::insert_object_member(text, &[], "mcpServers", &json!({ name: entry }))?,
        Some(Value::Object(servers)) if servers.contains_key(name) => return Err(taken_server(name, path)),
        Some(_) => json_edit::insert_object_member(text, &["mcpServers"], name, entry)?,
    };
    Ok(format!("{bom}{edited}"))
}

pub(crate) fn save_mcp(levels: &[ConfigLevel], language: ResolvedLanguage, target: &SaveTarget, form: &McpServerForm, original: Option<&McpServerForm>) -> Result<String, String> {
    let name = form.name.trim();
    let (level, path) = match &target.id {
        Some(id) => {
            let discovered = discover_levels(levels, language);
            let (descriptor, path, old_name) = listed_mcp(&discovered, id)?;
            let level = level_of(levels, descriptor.workspace_key.as_deref())?;
            let display = path.to_string_lossy().into_owned();
            let text = listed_file_text(levels, &descriptor, &path, |files| files.mcp.as_ref())?;
            let document = parse_strict(&text, &display)?;
            let servers = document.get("mcpServers").and_then(Value::as_object);
            let current = servers.and_then(|servers| servers.get(&old_name)).ok_or_else(moved_elsewhere)?;
            if original.is_some_and(|original| *original != mcp_form_of(&old_name, current)) {
                return Err(moved_elsewhere());
            }
            if name != old_name && servers.is_some_and(|servers| servers.contains_key(name)) {
                return Err(taken_server(name, &display));
            }
            let entry = merged_mcp_entry(Some(current), form);
            check_mcp_entry(name, &entry, level)?;
            let (bom, body) = config_file::split_bom(&text);
            let edited = json_edit::replace_object_member(body, &["mcpServers"], &old_name, name, &entry)?;
            write_config(level, &display, &format!("{bom}{edited}"))?;
            (level, display)
        }
        None => {
            let level = level_of(levels, target.workspace_key.as_deref())?;
            let (path, text) = config_file_at(level, CapabilityKind::Mcp)?;
            let entry = merged_mcp_entry(None, form);
            check_mcp_entry(name, &entry, level)?;
            let edited = text_with_new_server(text.as_deref(), &path, name, &entry)?;
            write_config(level, &path, &edited)?;
            (level, path)
        }
    };
    let location = mcp_config::location_for(Path::new(&path), name);
    let discovered = discover_levels(levels, language);
    discovered
        .catalog
        .mcps
        .iter()
        .find(|descriptor| is_saved_entry(descriptor, level, descriptor.location == location, descriptor.name == name))
        .map(|descriptor| descriptor.id.clone())
        .ok_or_else(|| not_listed_after_save(&path))
}

// ---------------------------------------------------------------- hooks

/// One `hooks.json` handler with the event and matcher of the group it is in.
/// Keys the form has no field for stay in the file.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct HookForm {
    pub name: String,
    pub event: String,
    pub matcher: String,
    pub command: String,
    pub command_windows: String,
    pub status_message: String,
    pub timeout_seconds: u32,
}

const DEFAULT_HOOK_TIMEOUT_SECONDS: u64 = 30;
const MAX_HOOK_COMMAND_BYTES: usize = 64 * 1024;

/// A group's matcher as discovery reads it: `*` and a blank both match everything.
fn matcher_key(matcher: &str) -> &str {
    let matcher = matcher.trim();
    if matcher == "*" { "" } else { matcher }
}

fn check_hook_form(form: &HookForm) -> Result<(), String> {
    if parse_hook_event(&form.event).is_none() {
        let event = &form.event;
        return Err(crate::ui_text::ui_text!("未知的钩子事件 {event}", "Unknown hook event {event}"));
    }
    let matcher = matcher_key(&form.matcher);
    if !matcher.is_empty() {
        regex::Regex::new(matcher).map_err(|error| crate::ui_text::ui_text!("匹配器不是合法的正则表达式：{error}", "The matcher is not a valid regular expression: {error}"))?;
    }
    if form.command.trim().is_empty() {
        return Err(crate::ui_text::pick("钩子的命令不能为空", "A hook needs a command").into());
    }
    if form.command.len() > MAX_HOOK_COMMAND_BYTES || form.command_windows.len() > MAX_HOOK_COMMAND_BYTES {
        let limit = MAX_HOOK_COMMAND_BYTES / 1024;
        return Err(crate::ui_text::ui_text!("命令不能超过 {limit} KiB", "A command cannot exceed {limit} KiB"));
    }
    if !(1..=600).contains(&form.timeout_seconds) {
        return Err(crate::ui_text::pick("超时要在 1 到 600 秒之间", "The timeout has to be between 1 and 600 seconds").into());
    }
    Ok(())
}

fn merged_handler(current: Option<&Value>, form: &HookForm, event_changed: bool) -> Value {
    let mut handler = current.and_then(Value::as_object).cloned().unwrap_or_default();
    handler.insert("type".to_owned(), Value::from("command"));
    handler.insert("command".to_owned(), Value::from(form.command.trim()));
    handler.remove("command_windows");
    set_text(&mut handler, "commandWindows", form.command_windows.trim());
    set_text(&mut handler, "name", form.name.trim());
    set_text(&mut handler, "statusMessage", form.status_message.trim());
    if handler.contains_key("timeout") || u64::from(form.timeout_seconds) != DEFAULT_HOOK_TIMEOUT_SECONDS {
        handler.insert("timeout".to_owned(), Value::from(form.timeout_seconds));
    }
    // `async` is honoured for one event only: under any other the handler would stop being read at all.
    if event_changed && form.event != "InstructionsLoaded" {
        handler.remove("async");
    }
    Value::Object(handler)
}

/// The handler and the group holding it, where the address says.
fn addressed_handler<'a>(document: &'a Value, address: &HookAddress) -> Option<(&'a Value, &'a Value)> {
    let group = document.get("hooks")?.get(&address.event)?.as_array()?.get(address.group_index)?;
    let handler = group.get("hooks")?.as_array()?.get(address.handler_index)?;
    Some((group, handler))
}

fn hook_form_of(event: &str, group: &Value, handler: &Value) -> HookForm {
    let windows = match text_of(handler, "commandWindows") {
        spelled if spelled.is_empty() => text_of(handler, "command_windows"),
        spelled => spelled,
    };
    HookForm {
        name: text_of(handler, "name"),
        event: event.to_owned(),
        matcher: text_of(group, "matcher"),
        command: text_of(handler, "command"),
        command_windows: windows,
        status_message: text_of(handler, "statusMessage"),
        timeout_seconds: u32::try_from(handler.get("timeout").and_then(Value::as_u64).unwrap_or(DEFAULT_HOOK_TIMEOUT_SECONDS)).unwrap_or(u32::MAX),
    }
}

fn listed_hook(discovered: &DiscoveredCapabilities, id: &str) -> Result<(ResourceDescriptor, HookAddress), String> {
    let descriptor = discovered
        .catalog
        .hooks
        .iter()
        .find(|descriptor| descriptor.id == id)
        .cloned()
        .ok_or_else(|| crate::ui_text::ui_text!("钩子 {id} 不存在", "There is no hook {id}"))?;
    let address = parse_hook_location(&descriptor.location)
        .ok_or_else(|| crate::ui_text::ui_text!("钩子 {id} 的位置无法解析", "The location of hook {id} cannot be read"))?;
    Ok((descriptor, address))
}

pub(crate) fn read_hook_form(levels: &[ConfigLevel], language: ResolvedLanguage, id: &str) -> Result<HookForm, String> {
    let (descriptor, address) = listed_hook(&discover_levels(levels, language), id)?;
    let text = listed_file_text(levels, &descriptor, &address.path, |files| files.hooks.as_ref())?;
    let document = parse_strict(&text, &address.path.to_string_lossy())?;
    let (group, handler) = addressed_handler(&document, &address).ok_or_else(moved_elsewhere)?;
    Ok(hook_form_of(&address.event, group, handler))
}

/// `text` with `handler` added at `event`: to a group with this matcher when
/// there is one, since that is how the file would be written by hand, else in a new one.
fn text_with_new_handler(text: Option<&str>, path: &str, event: &str, matcher: &str, handler: &Value) -> Result<String, String> {
    let mut group = Map::new();
    if !matcher.trim().is_empty() {
        group.insert("matcher".to_owned(), Value::from(matcher.trim()));
    }
    group.insert("hooks".to_owned(), json!([handler]));
    let group = Value::Object(group);
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        let mut out = serde_json::to_string_pretty(&json!({ "hooks": { event: [group] } })).map_err(|error| error.to_string())?;
        out.push('\n');
        return Ok(out);
    };
    let document = parse_strict(text, path)?;
    let (bom, body) = config_file::split_bom(text);
    let edited = match document.get("hooks") {
        None => json_edit::insert_object_member(body, &[], "hooks", &json!({ event: [group] }))?,
        Some(hooks) => match hooks.get(event).and_then(Value::as_array) {
            None => json_edit::insert_object_member(body, &["hooks"], event, &json!([group]))?,
            Some(groups) => {
                let same = groups.iter().position(|existing| matcher_key(&text_of(existing, "matcher")) == matcher_key(matcher) && existing.get("hooks").is_some_and(Value::is_array));
                match same {
                    Some(index) => json_edit::append_array_element(body, &[Step::Key("hooks"), Step::Key(event), Step::Index(index), Step::Key("hooks")], handler)?,
                    None => json_edit::append_array_element(body, &[Step::Key("hooks"), Step::Key(event)], &group)?,
                }
            }
        },
    };
    Ok(format!("{bom}{edited}"))
}

/// `text` with the handler at `address` changed. It stays in place unless its
/// event or matcher moved: a matcher belongs to the group, which may hold other
/// handlers, so the handler leaves for a group of its new matcher instead.
fn edited_hooks_text(text: &str, path: &str, address: &HookAddress, form: &HookForm, group: &Value, handler: &Value) -> Result<String, String> {
    let event_changed = form.event != address.event;
    let merged = merged_handler(Some(handler), form, event_changed);
    let (bom, body) = config_file::split_bom(text);
    if !event_changed && matcher_key(&form.matcher) == matcher_key(&text_of(group, "matcher")) {
        let steps = [Step::Key("hooks"), Step::Key(&address.event), Step::Index(address.group_index), Step::Key("hooks"), Step::Index(address.handler_index)];
        return Ok(format!("{bom}{}", json_edit::replace_value(body, &steps, &merged)?));
    }
    let steps = [Step::Key("hooks"), Step::Key(&address.event), Step::Index(address.group_index), Step::Key("hooks")];
    let cut = json_edit::remove_array_element(body, &steps, address.handler_index)?;
    let added = text_with_new_handler(Some(&cut), path, &form.event, &form.matcher, &merged)?;
    Ok(format!("{bom}{added}"))
}

pub(crate) fn save_hook(levels: &[ConfigLevel], language: ResolvedLanguage, target: &SaveTarget, form: &HookForm, original: Option<&HookForm>) -> Result<String, String> {
    check_hook_form(form)?;
    let (level, path) = match &target.id {
        Some(id) => {
            let (descriptor, address) = listed_hook(&discover_levels(levels, language), id)?;
            let level = level_of(levels, descriptor.workspace_key.as_deref())?;
            let display = address.path.to_string_lossy().into_owned();
            let text = listed_file_text(levels, &descriptor, &address.path, |files| files.hooks.as_ref())?;
            let document = parse_strict(&text, &display)?;
            let (group, handler) = addressed_handler(&document, &address).ok_or_else(moved_elsewhere)?;
            let was = hook_form_of(&address.event, group, handler);
            if original.is_some_and(|original| *original != was) {
                return Err(moved_elsewhere());
            }
            if *form == was {
                return Ok(id.clone());
            }
            let edited = edited_hooks_text(&text, &display, &address, form, group, handler)?;
            write_config(level, &display, &edited)?;
            (level, display)
        }
        None => {
            let level = level_of(levels, target.workspace_key.as_deref())?;
            let (path, text) = config_file_at(level, CapabilityKind::Hooks)?;
            let handler = merged_handler(None, form, false);
            let edited = text_with_new_handler(text.as_deref(), &path, &form.event, &form.matcher, &handler)?;
            write_config(level, &path, &edited)?;
            (level, path)
        }
    };
    // A hook's id is what it runs and when, so the handler is found again by that.
    let discovered = discover_levels(levels, language);
    let prefix = format!("{path}#/hooks/");
    discovered
        .catalog
        .hooks
        .iter()
        .filter(|descriptor| is_saved_entry(descriptor, level, descriptor.location.starts_with(&prefix), true))
        .filter(|descriptor| {
            discovered.hooks.get(&descriptor.id).is_some_and(|definition| {
                parse_hook_event(&form.event) == Some(definition.event)
                    && definition.command == form.command.trim()
                    && definition.command_windows.as_deref().unwrap_or_default() == form.command_windows.trim()
                    && definition.matcher.as_deref().unwrap_or_default() == matcher_key(&form.matcher)
            })
        })
        .last()
        .map(|descriptor| descriptor.id.clone())
        .ok_or_else(|| not_listed_after_save(&path))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: ResolvedLanguage = ResolvedLanguage::EnUs;

    fn global(home: &Path) -> ConfigLevel {
        ConfigLevel { source: ResourceSource::User, base: home.to_path_buf(), workspace: None, remote: None }
    }

    fn target(id: Option<&str>) -> SaveTarget {
        SaveTarget { id: id.map(str::to_owned), workspace_key: None }
    }

    fn hook_form(command: &str, event: &str, matcher: &str) -> HookForm {
        HookForm { event: event.into(), matcher: matcher.into(), command: command.into(), timeout_seconds: 30, ..HookForm::default() }
    }

    fn value_of(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_new_skill_gets_its_own_folder_and_reads_back_as_written() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let form = SkillForm { directory: "commit-helper".into(), name: "Commit helper".into(), description: "Prepare a commit: the staged part".into(), body: "# Steps\n\nRun git.".into() };
        let id = save_skill(&levels, ENGLISH, &target(None), &form, None).unwrap();
        let manifest = temp.path().join(".mewrk/skills/commit-helper/SKILL.md");
        let text = fs::read_to_string(&manifest).unwrap();
        assert_eq!(text, "---\nname: Commit helper\ndescription: |-\n  Prepare a commit: the staged part\n---\n\n# Steps\n\nRun git.\n");
        assert_eq!(read_skill_form(&levels, ENGLISH, &id).unwrap(), form);
        // The catalog reads the same words.
        let listed = discover_levels(&levels, ENGLISH).catalog.skills.into_iter().find(|skill| skill.id == id).unwrap();
        assert_eq!(listed.name, "Commit helper");
        assert_eq!(listed.description, "Prepare a commit: the staged part");

        let again = save_skill(&levels, ENGLISH, &target(None), &form, None).unwrap_err();
        assert!(again.contains("已有名为"), "{again}");
    }

    #[test]
    fn a_new_skill_folder_is_one_path_component() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        for bad in ["../evil", "a/b", "a\\b", "..", ".", "a,b", "call(me)", ""] {
            let form = SkillForm { directory: bad.into(), body: "x".into(), ..SkillForm::default() };
            assert!(save_skill(&levels, ENGLISH, &target(None), &form, None).is_err(), "{bad}");
        }
        assert!(!temp.path().join(".mewrk/evil").exists());
        assert!(!temp.path().join("evil").exists());
    }

    #[test]
    fn editing_a_skill_keeps_every_frontmatter_key_it_did_not_touch() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join(".mewrk/skills/deploy");
        fs::create_dir_all(&folder).unwrap();
        let manifest = folder.join(SKILL_MANIFEST);
        fs::write(&manifest, "\u{feff}---\r\nauthor: me\r\nname: 'Deploy'\r\nwhen_to_use: |\r\n  Shipping it\r\ndescription: >\r\n  Ships\r\n  the app\r\n---\r\n\r\nBody one\r\n").unwrap();
        let levels = [global(temp.path())];
        let id = discover_levels(&levels, ENGLISH).catalog.skills.into_iter().find(|skill| skill.source != ResourceSource::Builtin).unwrap().id;
        let opened = read_skill_form(&levels, ENGLISH, &id).unwrap();
        assert_eq!(opened, SkillForm { directory: "deploy".into(), name: "Deploy".into(), description: "Ships the app".into(), body: "Body one".into() });

        // Only the name changes, so only the name's line does.
        let form = SkillForm { name: "Ship".into(), ..opened.clone() };
        assert_eq!(save_skill(&levels, ENGLISH, &target(Some(&id)), &form, Some(&opened)).unwrap(), id, "the folder is the identity");
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "\u{feff}---\r\nauthor: me\r\nname: Ship\r\nwhen_to_use: |\r\n  Shipping it\r\ndescription: >\r\n  Ships\r\n  the app\r\n---\r\n\r\nBody one\r\n");

        // The body changes, and the folder name in the form means nothing.
        let opened = read_skill_form(&levels, ENGLISH, &id).unwrap();
        let form = SkillForm { directory: "elsewhere".into(), description: String::new(), body: "Body\ntwo".into(), ..opened.clone() };
        save_skill(&levels, ENGLISH, &target(Some(&id)), &form, Some(&opened)).unwrap();
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "\u{feff}---\r\nauthor: me\r\nname: Ship\r\nwhen_to_use: |\r\n  Shipping it\r\n---\r\n\r\nBody\r\ntwo\r\n");
        assert!(!temp.path().join(".mewrk/skills/elsewhere").exists());
    }

    #[test]
    fn naming_a_skill_that_had_no_frontmatter_gives_it_one_above_its_body() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join(".mewrk/skills/bare");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(SKILL_MANIFEST), "# Bare\n\nDo it.\n").unwrap();
        let levels = [global(temp.path())];
        let id = discover_levels(&levels, ENGLISH).catalog.skills.into_iter().find(|skill| skill.source != ResourceSource::Builtin).unwrap().id;
        let opened = read_skill_form(&levels, ENGLISH, &id).unwrap();
        assert_eq!(opened.name, "", "the catalog's inferred name is not the file's");
        save_skill(&levels, ENGLISH, &target(Some(&id)), &SkillForm { name: "Named".into(), ..opened.clone() }, Some(&opened)).unwrap();
        assert_eq!(fs::read_to_string(folder.join(SKILL_MANIFEST)).unwrap(), "---\nname: Named\n---\n\n# Bare\n\nDo it.\n");
    }

    #[test]
    fn a_skill_changed_on_disk_since_the_form_opened_is_not_written_over() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let form = SkillForm { directory: "x".into(), body: "One".into(), ..SkillForm::default() };
        let id = save_skill(&levels, ENGLISH, &target(None), &form, None).unwrap();
        let opened = read_skill_form(&levels, ENGLISH, &id).unwrap();
        let manifest = temp.path().join(".mewrk/skills/x/SKILL.md");
        fs::write(&manifest, "Changed by hand\n").unwrap();
        let error = save_skill(&levels, ENGLISH, &target(Some(&id)), &SkillForm { body: "Two".into(), ..opened.clone() }, Some(&opened)).unwrap_err();
        assert!(error.contains("已在别处变化"), "{error}");
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "Changed by hand\n");
    }

    #[test]
    fn the_builtin_skill_cannot_be_opened_or_saved() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        assert!(read_skill_form(&levels, ENGLISH, skills::MEWRK_SDK_ID).is_err());
        let form = SkillForm { body: "x".into(), ..SkillForm::default() };
        assert!(save_skill(&levels, ENGLISH, &target(Some(skills::MEWRK_SDK_ID)), &form, None).is_err());
    }

    #[test]
    fn a_new_server_starts_the_file_or_joins_it_without_touching_the_others() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let path = temp.path().join(".mewrk/mcp.json");
        let form = McpServerForm { name: "first".into(), transport: "stdio".into(), command: "npx".into(), args: vec!["-y".into(), "pkg".into()], env: BTreeMap::from([("TOKEN".to_owned(), "${MEWRK_TEST_UNSET_TOKEN}".to_owned())]), ..McpServerForm::default() };
        let id = save_mcp(&levels, ENGLISH, &target(None), &form, None).unwrap();
        assert_eq!(value_of(&path)["mcpServers"]["first"], json!({ "command": "npx", "args": ["-y", "pkg"], "env": { "TOKEN": "${MEWRK_TEST_UNSET_TOKEN}" } }));
        // An unset variable is the row's to report, not a reason to lose the entry.
        let listed = discover_levels(&levels, ENGLISH).catalog.mcps.into_iter().find(|server| server.id == id).unwrap();
        assert!(!listed.available && listed.description.contains("MEWRK_TEST_UNSET_TOKEN"), "{listed:?}");

        fs::write(&path, "{\n    \"note\": 1,\n    \"mcpServers\": {\n        \"kept\":   {\"command\":\"x\"}\n    }\n}\n").unwrap();
        let second = McpServerForm { name: "docs".into(), transport: "http".into(), url: "https://example.com/mcp".into(), ..McpServerForm::default() };
        save_mcp(&levels, ENGLISH, &target(None), &second, None).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\n    \"note\": 1,\n    \"mcpServers\": {\n        \"kept\":   {\"command\":\"x\"},\n        \"docs\": {\n"), "{text}");

        let error = save_mcp(&levels, ENGLISH, &target(None), &second, None).unwrap_err();
        assert!(error.contains("已有名为"), "{error}");
    }

    #[test]
    fn editing_a_server_shows_references_unexpanded_and_keeps_keys_the_form_lacks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".mewrk/mcp.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"mcpServers":{"docs":{"type":"streamable-http","url":"https://example.com/${MEWRK_TEST_UNSET_PATH:-mcp}","headers":{"Authorization":"Bearer ${MEWRK_TEST_UNSET_PATH:-x}"},"disabledTools":["danger"],"timeout":90000}}}"#).unwrap();
        let levels = [global(temp.path())];
        let id = discover_levels(&levels, ENGLISH).catalog.mcps[0].id.clone();
        let opened = read_mcp_form(&levels, ENGLISH, &id).unwrap();
        assert_eq!(opened.url, "https://example.com/${MEWRK_TEST_UNSET_PATH:-mcp}", "never the expanded value");
        assert_eq!(opened.headers["Authorization"], "Bearer ${MEWRK_TEST_UNSET_PATH:-x}");
        assert_eq!(opened.transport, "http");
        assert_eq!(opened.timeout_seconds, 90);

        let form = McpServerForm { description: "Project docs".into(), ..opened.clone() };
        assert_eq!(save_mcp(&levels, ENGLISH, &target(Some(&id)), &form, Some(&opened)).unwrap(), id, "the name is unchanged, so the id is");
        let entry = value_of(&path)["mcpServers"]["docs"].clone();
        assert_eq!(entry["type"], "streamable-http");
        assert_eq!(entry["url"], "https://example.com/${MEWRK_TEST_UNSET_PATH:-mcp}");
        assert_eq!(entry["disabledTools"], json!(["danger"]));
        assert_eq!(entry["timeout"], 90000, "left alone, it keeps Claude Code's spelling");
        assert_eq!(entry["description"], "Project docs");
    }

    #[test]
    fn renaming_or_switching_a_server_rewrites_only_its_own_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".mewrk/mcp.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\n  \"mcpServers\": {\n    \"a\": {\"command\":\"x\",\"env\":{\"K\":\"v\"}},\n    \"b\":   {\"command\":\"y\"}\n  }\n}\n").unwrap();
        let levels = [global(temp.path())];
        let id = discover_levels(&levels, ENGLISH).catalog.mcps.into_iter().find(|s| s.name == "a").unwrap().id;
        let opened = read_mcp_form(&levels, ENGLISH, &id).unwrap();

        let taken = McpServerForm { name: "b".into(), ..opened.clone() };
        assert!(save_mcp(&levels, ENGLISH, &target(Some(&id)), &taken, Some(&opened)).unwrap_err().contains("已有名为"));

        let form = McpServerForm { name: "c".into(), transport: "http".into(), url: "https://example.com/mcp".into(), ..opened.clone() };
        let new_id = save_mcp(&levels, ENGLISH, &target(Some(&id)), &form, Some(&opened)).unwrap();
        assert_ne!(new_id, id, "a server's name is part of its id");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"b\":   {\"command\":\"y\"}"), "{text}");
        let value = value_of(&path);
        assert!(value["mcpServers"].get("a").is_none());
        assert_eq!(value["mcpServers"]["c"], json!({ "type": "http", "url": "https://example.com/mcp" }), "nothing of the old transport stays");
    }

    #[test]
    fn a_server_that_could_never_run_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let cases = [
            McpServerForm { name: "bad name".into(), transport: "stdio".into(), command: "x".into(), ..McpServerForm::default() },
            McpServerForm { name: "empty".into(), transport: "stdio".into(), ..McpServerForm::default() },
            McpServerForm { name: "no-url".into(), transport: "http".into(), ..McpServerForm::default() },
            McpServerForm { name: "plain".into(), transport: "http".into(), url: "http://example.com/mcp".into(), ..McpServerForm::default() },
        ];
        for form in &cases {
            assert!(save_mcp(&levels, ENGLISH, &target(None), form, None).is_err(), "{}", form.name);
        }
        assert!(!temp.path().join(".mewrk/mcp.json").exists());
    }

    #[test]
    fn a_new_hook_joins_a_group_with_its_matcher_or_starts_one() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let path = temp.path().join(".mewrk/hooks.json");
        let first = save_hook(&levels, ENGLISH, &target(None), &hook_form("one", "PreToolUse", "^bash$"), None).unwrap();
        assert_eq!(value_of(&path), json!({ "hooks": { "PreToolUse": [{ "matcher": "^bash$", "hooks": [{ "type": "command", "command": "one" }] }] } }));
        let second = save_hook(&levels, ENGLISH, &target(None), &hook_form("two", "PreToolUse", "^bash$"), None).unwrap();
        save_hook(&levels, ENGLISH, &target(None), &hook_form("three", "PreToolUse", ""), None).unwrap();
        save_hook(&levels, ENGLISH, &target(None), &HookForm { timeout_seconds: 5, ..hook_form("four", "Stop", "*") }, None).unwrap();
        let value = value_of(&path);
        let groups = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 2);
        assert_eq!(value["hooks"]["Stop"][0]["hooks"][0]["timeout"], 5);
        assert_ne!(first, second);
        assert_eq!(discover_levels(&levels, ENGLISH).catalog.hooks.len(), 4);
    }

    #[test]
    fn changing_a_matcher_moves_only_that_handler_out_of_its_group() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".mewrk/hooks.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"keep":1,"hooks":{"PreToolUse":[{"matcher":"^bash$","hooks":[{"type":"command","command":"one","extra":true},{"type":"command","command":"two"}]}]}}"#).unwrap();
        let levels = [global(temp.path())];
        let listed = discover_levels(&levels, ENGLISH);
        let one = listed.catalog.hooks.iter().find(|hook| listed.hooks[&hook.id].command == "one").unwrap().id.clone();
        let two = listed.catalog.hooks.iter().find(|hook| listed.hooks[&hook.id].command == "two").unwrap().id.clone();

        // A name is how a hook is shown, so it keeps the id and its place.
        let opened = read_hook_form(&levels, ENGLISH, &one).unwrap();
        assert_eq!(opened.timeout_seconds, 30);
        let named = HookForm { name: "First".into(), ..opened.clone() };
        assert_eq!(save_hook(&levels, ENGLISH, &target(Some(&one)), &named, Some(&opened)).unwrap(), one);
        let value = value_of(&path);
        assert_eq!(value["hooks"]["PreToolUse"][0]["hooks"][0]["name"], "First");
        assert_eq!(value["hooks"]["PreToolUse"][0]["hooks"][0]["extra"], true);

        let opened = read_hook_form(&levels, ENGLISH, &one).unwrap();
        let moved = HookForm { matcher: "^write$".into(), ..opened.clone() };
        let new_id = save_hook(&levels, ENGLISH, &target(Some(&one)), &moved, Some(&opened)).unwrap();
        assert_ne!(new_id, one, "a matcher is part of what makes a hook itself");
        let value = value_of(&path);
        let groups = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(value["keep"], 1);
        assert_eq!(groups[0]["matcher"], "^bash$");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1, "its neighbour stays where it was");
        assert_eq!(groups[1]["matcher"], "^write$");
        assert_eq!(groups[1]["hooks"][0]["extra"], true);
        let listed = discover_levels(&levels, ENGLISH);
        assert!(listed.catalog.hooks.iter().any(|hook| hook.id == two), "the neighbour's id did not move");
        assert_eq!(listed.hooks[&new_id].matcher.as_deref(), Some("^write$"));

        // To another event, an `async` that only one event honours would hide it.
        fs::write(&path, r#"{"hooks":{"InstructionsLoaded":[{"hooks":[{"type":"command","command":"late","async":true}]}]}}"#).unwrap();
        let id = discover_levels(&levels, ENGLISH).catalog.hooks[0].id.clone();
        let opened = read_hook_form(&levels, ENGLISH, &id).unwrap();
        let new_id = save_hook(&levels, ENGLISH, &target(Some(&id)), &HookForm { event: "Stop".into(), ..opened.clone() }, Some(&opened)).unwrap();
        assert!(discover_levels(&levels, ENGLISH).hooks.contains_key(&new_id));
    }

    #[test]
    fn a_hook_is_checked_before_anything_is_written() {
        let temp = tempfile::tempdir().unwrap();
        let levels = [global(temp.path())];
        let cases = [
            hook_form("", "Stop", ""),
            hook_form("x", "NoSuchEvent", ""),
            hook_form("x", "PreToolUse", "("),
            HookForm { timeout_seconds: 0, ..hook_form("x", "Stop", "") },
            HookForm { timeout_seconds: 601, ..hook_form("x", "Stop", "") },
        ];
        for form in &cases {
            assert!(save_hook(&levels, ENGLISH, &target(None), form, None).is_err(), "{form:?}");
        }
        assert!(!temp.path().join(".mewrk/hooks.json").exists());
    }

    #[test]
    fn an_existing_legacy_file_is_edited_where_it_is_rather_than_hidden_by_a_new_one() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join(".naiword/mcp.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, r#"{"mcpServers":{"old":{"command":"x"}}}"#).unwrap();
        let levels = [global(temp.path())];
        let form = McpServerForm { name: "new".into(), transport: "stdio".into(), command: "y".into(), ..McpServerForm::default() };
        save_mcp(&levels, ENGLISH, &target(None), &form, None).unwrap();
        assert!(!temp.path().join(".mewrk/mcp.json").exists());
        assert_eq!(discover_levels(&levels, ENGLISH).catalog.mcps.len(), 2);
    }
}
