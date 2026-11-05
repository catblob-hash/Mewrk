//! The windows that create and edit a skill, an MCP server or a hook, and what puts them on the catalog pages: the same "new" button under the list a role page has, and an edit button on each row.
//!
//! All three are files the user owns. The form holds what the file says as written, a `${VAR}` staying a reference, and the host writes only what changed. A role's window has many pages, so it has a rail; these are one page each, so they are a dialog that scrolls.

use std::collections::BTreeMap;

use mewrk_client::model::{ResourceDescriptor, ResourceSource, SshMachineConfig};
use mewrk_client::prelude::*;

use crate::catalog::{CapabilityPage, CatalogKind};
use crate::lock::lock_hint;
use crate::logic::{parse_capability_workspace_key, workspace_location_title, CapabilityWorkspace};
use crate::ops::{CapabilityForm, Handlers, HookForm, McpServerForm, SaveAgentRoleTarget, SkillForm};
use crate::widgets::*;

/// The events a hook can run at, in the order a run reaches them, with what each means.
pub const HOOK_EVENTS: [(&str, &str, &str); 7] = [
    ("SessionStart", "对话开始时", "When a conversation starts"),
    ("InstructionsLoaded", "项目指令载入后", "After project instructions load"),
    ("UserPromptSubmit", "用户发送消息时", "When the user sends a message"),
    ("PreToolUse", "工具调用前", "Before a tool call"),
    ("PermissionRequest", "请求批准时", "When approval is requested"),
    ("PostToolUse", "工具调用后", "After a tool call"),
    ("Stop", "回合结束时", "When a turn ends"),
];

const DEFAULT_HOOK_TIMEOUT: &str = "30";
const MAX_MCP_TIMEOUT: u32 = 300;
const MAX_HOOK_TIMEOUT: u32 = 600;

/// What a form's fields hold while it is typed into. Lists and maps are text, one item a line, so they can be pasted; a line that cannot be read is said under the field rather than dropped.
#[derive(Clone, Debug, Default, PartialEq)]
struct Draft {
    name: String,
    description: String,
    directory: String,
    body: String,
    http: bool,
    command: String,
    args: String,
    env: String,
    cwd: String,
    url: String,
    headers: String,
    timeout: String,
    event: String,
    matcher: String,
    command_windows: String,
    status_message: String,
}

fn map_lines(values: &BTreeMap<String, String>, separator: &str) -> String {
    values.iter().map(|(name, value)| format!("{name}{separator}{value}")).collect::<Vec<_>>().join("\n")
}

impl Draft {
    fn of(form: &CapabilityForm) -> Self {
        match form {
            CapabilityForm::Skill(f) => Self { directory: f.directory.clone(), name: f.name.clone(), description: f.description.clone(), body: f.body.clone(), ..Self::default() },
            CapabilityForm::Mcp(f) => Self {
                name: f.name.clone(),
                description: f.description.clone(),
                http: f.transport == "http",
                command: f.command.clone(),
                args: f.args.join("\n"),
                env: map_lines(&f.env, "="),
                cwd: f.cwd.clone(),
                url: f.url.clone(),
                headers: map_lines(&f.headers, ": "),
                timeout: if f.timeout_seconds == 0 { String::new() } else { f.timeout_seconds.to_string() },
                ..Self::default()
            },
            CapabilityForm::Hook(f) => Self { name: f.name.clone(), event: f.event.clone(), matcher: f.matcher.clone(), command: f.command.clone(), command_windows: f.command_windows.clone(), status_message: f.status_message.clone(), timeout: f.timeout_seconds.to_string(), ..Self::default() },
        }
    }

    fn blank(kind: CatalogKind) -> Self {
        match kind {
            CatalogKind::Hooks => Self { event: "PreToolUse".to_owned(), timeout: DEFAULT_HOOK_TIMEOUT.to_owned(), ..Self::default() },
            _ => Self::default(),
        }
    }
}

/// `NAME=value` or `Name: value`, one a line. The number of the first line that has no separator, or no name, is what is wrong.
fn parse_map(text: &str, separator: char) -> Result<BTreeMap<String, String>, usize> {
    let mut out = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(separator).ok_or(index + 1)?;
        if name.trim().is_empty() {
            return Err(index + 1);
        }
        out.insert(name.trim().to_owned(), value.trim().to_owned());
    }
    Ok(out)
}

fn skill_directory_error(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Some(t("文件夹名不能为空。", "A folder name is required."));
    }
    let bad = name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | '(' | ')' | ',')) || matches!(name, "." | "..");
    bad.then(|| t("文件夹名不能包含路径分隔符、括号、逗号或控制字符。", "A folder name cannot contain a path separator, a parenthesis, a comma or a control character."))
}

fn mcp_name_error(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Some(t("名称不能为空。", "A name is required."));
    }
    let bad = !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) || matches!(name, "__proto__" | "constructor" | "prototype");
    bad.then(|| t("名称只能包含字母、数字、连字符和下划线。", "A name can contain only letters, digits, hyphens and underscores."))
}

fn required(value: &str, message: &'static str) -> Option<String> {
    value.trim().is_empty().then(|| message.to_owned())
}

fn bad_line(line: usize) -> String {
    tf("第 {line} 行读不出来。", "Line {line} cannot be read.", &[("line", &line)])
}

fn timeout_error(text: &str, min: u32, max: u32) -> Option<String> {
    let text = text.trim();
    if text.is_empty() && min == 0 {
        return None;
    }
    match text.parse::<u32>() {
        Ok(seconds) if (min..=max).contains(&seconds) => None,
        _ => Some(tf("要是 {min} 到 {max} 之间的整数。", "Has to be a whole number from {min} to {max}.", &[("min", &min), ("max", &max)])),
    }
}

/// What a field complains of. `checked` is whether the user has tried to save: a blank is not a complaint before then, though a line that cannot be read is at once.
#[derive(Default)]
struct Complaints {
    directory: Option<String>,
    body: Option<String>,
    name: Option<String>,
    command: Option<String>,
    url: Option<String>,
    env: Option<String>,
    headers: Option<String>,
    timeout: Option<String>,
}

impl Complaints {
    fn of(kind: CatalogKind, draft: &Draft, creating: bool, duplicate: bool) -> Self {
        match kind {
            CatalogKind::Skills => Self { directory: creating.then(|| skill_directory_error(&draft.directory).map(str::to_owned)).flatten(), body: required(&draft.body, t("正文不能为空。", "A body is required.")), ..Self::default() },
            CatalogKind::Mcp => Self {
                name: if duplicate { Some(t("同一位置已有同名的服务器。", "A server with this name already exists at this location.").to_owned()) } else { mcp_name_error(&draft.name).map(str::to_owned) },
                command: if draft.http { None } else { required(&draft.command, t("命令不能为空。", "A command is required.")) },
                url: if draft.http { required(&draft.url, t("URL 不能为空。", "A URL is required.")) } else { None },
                env: if draft.http { None } else { parse_map(&draft.env, '=').err().map(bad_line) },
                headers: if draft.http { parse_map(&draft.headers, ':').err().map(bad_line) } else { None },
                timeout: timeout_error(&draft.timeout, 0, MAX_MCP_TIMEOUT),
                ..Self::default()
            },
            _ => Self { command: required(&draft.command, t("命令不能为空。", "A command is required.")), timeout: timeout_error(&draft.timeout, 1, MAX_HOOK_TIMEOUT), ..Self::default() },
        }
    }

    fn any(&self) -> bool {
        [&self.directory, &self.body, &self.name, &self.command, &self.url, &self.env, &self.headers, &self.timeout].iter().any(|c| c.is_some())
    }
}

/// The form a save sends. Only called once nothing complains.
fn form_of(kind: CatalogKind, draft: &Draft) -> CapabilityForm {
    match kind {
        CatalogKind::Skills => CapabilityForm::Skill(SkillForm { directory: draft.directory.trim().to_owned(), name: draft.name.trim().to_owned(), description: draft.description.trim().to_owned(), body: draft.body.trim().to_owned() }),
        CatalogKind::Mcp => CapabilityForm::Mcp(McpServerForm {
            name: draft.name.trim().to_owned(),
            description: draft.description.trim().to_owned(),
            transport: if draft.http { "http" } else { "stdio" }.to_owned(),
            command: draft.command.trim().to_owned(),
            args: draft.args.lines().map(str::trim).filter(|line| !line.is_empty()).map(str::to_owned).collect(),
            env: parse_map(&draft.env, '=').unwrap_or_default(),
            cwd: draft.cwd.trim().to_owned(),
            url: draft.url.trim().to_owned(),
            headers: parse_map(&draft.headers, ':').unwrap_or_default(),
            timeout_seconds: draft.timeout.trim().parse().unwrap_or(0),
        }),
        _ => CapabilityForm::Hook(HookForm { name: draft.name.trim().to_owned(), event: draft.event.clone(), matcher: draft.matcher.trim().to_owned(), command: draft.command.trim().to_owned(), command_windows: draft.command_windows.trim().to_owned(), status_message: draft.status_message.trim().to_owned(), timeout_seconds: draft.timeout.trim().parse().unwrap_or(0) }),
    }
}

/// Whether saving makes this another entry to the host, which conversations selecting the old one cannot follow by themselves: a server is its name, a hook is what it runs and when.
fn changes_identity(kind: CatalogKind, draft: &Draft, was: &Draft) -> bool {
    match kind {
        CatalogKind::Mcp => draft.name.trim() != was.name.trim(),
        CatalogKind::Hooks => draft.command.trim() != was.command.trim() || draft.command_windows.trim() != was.command_windows.trim() || draft.event != was.event || draft.matcher.trim() != was.matcher.trim(),
        _ => false,
    }
}

fn scope_of(resource: Option<&ResourceDescriptor>) -> String {
    resource.map_or_else(|| "new".to_owned(), |r| r.id.clone())
}

/// A field of the window, for focusing it and for tests. `scope` is the entry's id, or `new`.
pub fn field_key(kind: CatalogKind, scope: &str, id: &str) -> (&'static str, String, String, String) {
    ("capability-field", kind.wire().to_owned(), scope.to_owned(), id.to_owned())
}

/// The save, cancel and close buttons of a window.
pub fn button_key(kind: CatalogKind, scope: &str, id: &str) -> (&'static str, String, String, String) {
    ("capability-button", kind.wire().to_owned(), scope.to_owned(), id.to_owned())
}

/// The "new" button under a page's list.
pub fn add_button_key(list_id: &str) -> (&'static str, String) {
    ("capability-add", list_id.to_owned())
}

/// A row's edit button.
pub fn edit_button_key(list_id: &str, id: &str) -> (&'static str, String, String) {
    ("capability-edit", list_id.to_owned(), id.to_owned())
}

/// A window's state. Dropped with the window: it lives as long as a frame draws it.
struct Window {
    /// What the file said when the window opened, for the concurrency check. `None` for a new entry, and until the read answers.
    opened: Option<CapabilityForm>,
    draft: Draft,
    /// What the draft started as, to tell whether closing loses anything.
    was: Draft,
    level: Option<String>,
    reading: bool,
    read_error: Option<String>,
    saving: bool,
    error: Option<String>,
    checked: bool,
    asking_to_discard: bool,
    focus: bool,
}

/// One window, over the page that opened it. Not dismissed by a click on its backdrop, which would lose what is typed; closing with changes asks first.
pub struct CapabilityEditor {
    pub kind: CatalogKind,
    /// The entry as it was listed when opened; `None` creates one.
    pub resource: Option<ResourceDescriptor>,
    /// Said at the top: changing this entry throws the warm cache away.
    pub cache_note: Option<String>,
    /// Where a new entry may go besides the global level. `None` is a preset's window, which has no project.
    pub workspaces: Option<Vec<CapabilityWorkspace>>,
    pub ssh_machines: Vec<SshMachineConfig>,
    pub handlers: Handlers,
    /// The id it was opened on, `None` for a new entry, and the id it is listed under now.
    pub on_saved: Rc<dyn Fn(&mut App, Option<String>, String)>,
    pub on_close: Rc<dyn Fn(&mut App)>,
}

fn try_close(state: &Local<Window>, on_close: &Rc<dyn Fn(&mut App)>, app: &mut App) {
    // A save on its way closes the window itself, or comes back with why it could not.
    if state.with(|s| s.saving) {
        return;
    }
    if state.with(|s| s.draft != s.was) {
        state.update(|s| s.asking_to_discard = true);
    } else {
        on_close(app);
    }
}

fn save(app: &mut App, this: &Rc<CapabilityEditor>, state: &Local<Window>, complaints_any: bool) {
    let Some(write) = this.handlers.on_save_capability.clone() else { return };
    if state.with(|s| s.saving) {
        return;
    }
    if complaints_any {
        state.update(|s| {
            s.checked = true;
            s.error = Some(t("请先修正标记的字段。", "Fix the marked fields before saving.").to_owned());
        });
        return;
    }
    let (draft, opened, level) = state.with(|s| (s.draft.clone(), s.opened.clone(), s.level.clone()));
    let target = SaveAgentRoleTarget { id: this.resource.as_ref().map(|r| r.id.clone()), workspace_key: this.resource.as_ref().map_or(level, |r| r.workspace_key.clone()) };
    state.update(|s| {
        s.saving = true;
        s.error = None;
    });
    let (th, st) = (this.clone(), state.clone());
    write(
        app,
        target,
        form_of(this.kind, &draft),
        opened,
        Box::new(move |app, result| match result {
            Ok(id) => {
                st.update(|s| s.saving = false);
                (th.on_saved)(app, th.resource.as_ref().map(|r| r.id.clone()), id);
                (th.on_close)(app);
            }
            Err(reason) => st.update(|s| {
                s.saving = false;
                s.error = Some(reason);
            }),
        }),
    );
}

impl CapabilityEditor {
    pub fn view(self, app: &mut App) -> E {
        let t_ = theme();
        let this = Rc::new(self);
        let kind = this.kind;
        let scope = scope_of(this.resource.as_ref());
        let creating = this.resource.is_none();
        let state = use_state(("capability-editor", kind.wire(), scope.as_str()), || {
            let draft = Draft::blank(kind);
            Window { opened: None, was: draft.clone(), draft, level: None, reading: !creating, read_error: None, saving: false, error: None, checked: false, asking_to_discard: false, focus: creating }
        });
        // The file is read once, when the window opens, so an edit never loses what is typed to a later change on disk: that is the host's to refuse on saving.
        if state.with(|s| s.reading && s.opened.is_none() && s.read_error.is_none())
            && let (Some(read), Some(resource)) = (this.handlers.on_read_capability.clone(), this.resource.clone())
            && !use_state(("capability-editor-read", kind.wire(), scope.as_str()), || false).update(|asked| std::mem::replace(asked, true))
        {
            let st = state.clone();
            read(app, kind, resource.id, Box::new(move |_, result| match result {
                Ok(form) => st.update(|s| {
                    s.draft = Draft::of(&form);
                    s.was = s.draft.clone();
                    s.opened = Some(form);
                    s.reading = false;
                    s.focus = true;
                }),
                Err(reason) => st.update(|s| {
                    s.reading = false;
                    s.read_error = Some(reason);
                }),
            }));
        }
        let first_field = match kind {
            CatalogKind::Skills if creating => "directory",
            CatalogKind::Skills => "body",
            _ => "name",
        };
        if state.update(|s| std::mem::take(&mut s.focus)) {
            focus_request(("field", field_key(kind, &scope, first_field)));
        }

        let (draft, was, reading, read_error, saving, error, checked, asking, level) = state.with(|s| (s.draft.clone(), s.was.clone(), s.reading, s.read_error.clone(), s.saving, s.error.clone(), s.checked, s.asking_to_discard, s.level.clone()));
        let duplicate = kind == CatalogKind::Mcp && {
            let at = this.resource.as_ref().map_or(level.clone(), |r| r.workspace_key.clone());
            let own = this.resource.as_ref().map(|r| r.id.as_str());
            app.doc.capabilities.mcps.iter().any(|r| Some(r.id.as_str()) != own && r.workspace_key == at && r.name == draft.name.trim())
        };
        let complaints = Complaints::of(kind, &draft, creating, duplicate);
        let title = match (&this.resource, kind) {
            (Some(r), _) => tf("编辑 {name}", "Edit {name}", &[("name", &r.name)]),
            (None, CatalogKind::Skills) => t("新建技能", "New skill").to_owned(),
            (None, CatalogKind::Mcp) => t("新建 MCP Server", "New MCP server").to_owned(),
            (None, _) => t("新建钩子", "New hook").to_owned(),
        };

        let body = if reading {
            col().py(26.0).items_center().child(small(t("正在读取…", "Reading…")))
        } else if let Some(reason) = &read_error {
            col().py(14.0).child(error_text(reason.clone()).role(Role::Alert))
        } else {
            let ctx = FormContext { this: this.clone(), state: state.clone(), draft: draft.clone(), complaints: &complaints, checked, scope: scope.clone() };
            let fields = match kind {
                CatalogKind::Skills => skill_fields(&ctx),
                CatalogKind::Mcp => mcp_fields(&ctx),
                _ => hook_fields(&ctx),
            };
            let identity_note = (!creating && changes_identity(kind, &draft, &was)).then(|| {
                small(match kind {
                    CatalogKind::Mcp => t("改名后就是另一台服务器：这里会跟过去，其它勾选了它的对话和预设要重新勾选，否则它们的运行会失败。", "A new name makes it another server: this page follows it, but other conversations and presets that selected it have to select it again, or their runs fail."),
                    _ => t("改命令、事件或匹配器就是另一个钩子：这里会跟过去，其它勾选了它的对话和预设要重新勾选，否则它们的运行会失败。", "Another command, event or matcher makes it another hook: this page follows it, but other conversations and presets that selected it have to select it again, or their runs fail."),
                })
                .color(t_.caution)
            });
            col()
                .gap(12.0)
                .min_w(0.0)
                .child(this.resource.as_ref().filter(|r| !r.available).map(|r| error_text(r.description.clone())))
                .child(this.cache_note.clone().map(|note| small(note).color(t_.caution)))
                .child(location_block(&this, &state, level.as_deref()))
                .child(fields)
                .child(identity_note)
        };

        let close: Rc<dyn Fn(&mut App)> = {
            let (st, on_close) = (state.clone(), this.on_close.clone());
            Rc::new(move |app: &mut App| try_close(&st, &on_close, app))
        };
        let footer = if asking {
            let (st_keep, on_close) = (state.clone(), this.on_close.clone());
            vec![
                label(t("有未保存的修改，要放弃吗？", "There are unsaved changes. Discard them?")).flex1().min_w(0.0),
                btn(button_key(kind, &scope, "keep"), Look::Standard, t("继续编辑", "Keep editing")).on_click(move |_| st_keep.update(|s| s.asking_to_discard = false)),
                btn(button_key(kind, &scope, "discard"), Look::Danger, t("放弃", "Discard")).on_click(move |app: &mut App| on_close(app)),
            ]
        } else {
            let (cl, th, st, any) = (close.clone(), this.clone(), state.clone(), complaints.any());
            let cannot_save = reading || read_error.is_some() || saving;
            vec![
                btn(button_key(kind, &scope, "cancel"), Look::Standard, t("取消", "Cancel")).on_click(move |app: &mut App| cl(app)),
                btn(button_key(kind, &scope, "save"), Look::Accent, if saving { t("正在保存…", "Saving…") } else { t("保存", "Save") }).disabled(cannot_save).when(cannot_save, |e| e.opacity(0.5)).on_click(move |app: &mut App| save(app, &th, &st, any)),
            ]
        };

        let (w, h) = window_size();
        let width = 640.0_f32.min(w - if window_strip() { 16.0 } else { 48.0 }).max(320.0);
        let close_x = close.clone();
        say(&title);
        let inner = col()
            .key(("capability-editor-frame", kind.wire(), scope.as_str()))
            .max_h((h - 48.0).clamp(240.0, 760.0))
            .min_h(0.0)
            // Over the dialog's own fill, as a role's window is: a form is read closely, and the page's text would otherwise show through it.
            .bg(t_.flyout)
            .radius(OVERLAY_RADIUS)
            .child(row().pt(9.0).pb(8.0).pl(20.0).pr(10.0).items_center().gap(12.0).shrink0().child(text(title.clone()).ty(BODY_STRONG).ellipsis().flex1().role(Role::Heading).aria_level(2)).child(icon_button::<App>("X", tf("关闭 {title}", "Close {title}", &[("title", &title)])).key(button_key(kind, &scope, "close")).on_click(move |app: &mut App| close_x(app))))
            .child(col().key(("capability-editor-scroll", kind.wire(), scope.as_str())).min_h(0.0).min_w(0.0).with_style(|s| s.flex_shrink = 1.0).scroll_y().pt(4.0).px(20.0).pb(14.0).child(body))
            .child(error.map(|e| col().px(20.0).pb(6.0).shrink0().child(error_text(e).role(Role::Alert))))
            .child(row().pt(6.0).pb(12.0).px(20.0).items_center().justify_end().gap(8.0).shrink0().children(footer));
        dialog(width, inner, None).aria_label(title)
    }
}

struct FormContext<'a> {
    this: Rc<CapabilityEditor>,
    state: Local<Window>,
    draft: Draft,
    complaints: &'a Complaints,
    checked: bool,
    scope: String,
}

impl FormContext<'_> {
    /// A one-line field of the draft.
    fn field(&self, id: &str, value: &str, write: fn(&mut Draft, String)) -> Field<App> {
        let st = self.state.clone();
        field(field_key(self.this.kind, &self.scope, id), value.to_owned()).on_change(move |_: &mut App, v| {
            st.update(|s| {
                write(&mut s.draft, v);
                s.error = None;
                s.asking_to_discard = false;
            });
        })
    }

    /// A complaint about a blank waits for a save to be tried. One about what is there does not.
    fn hint(&self, complaint: &Option<String>, value: &str, standing: Option<&str>) -> Option<Hint> {
        match complaint {
            Some(c) if self.checked || !value.trim().is_empty() => Some(Hint::error(c.clone())),
            _ => standing.map(Hint::new),
        }
    }

    fn block(&self, title: &str, control: Field<App>, hint: Option<Hint>) -> E {
        let invalid = hint.as_ref().is_some_and(|h| h.error);
        field_block(title.to_owned(), control.build().aria_label(title.to_owned()).aria_invalid(invalid), hint)
    }
}

/// Where the entry is. Chosen once, like a role's: a file does not move between levels by editing it.
fn location_block(this: &Rc<CapabilityEditor>, state: &Local<Window>, level: Option<&str>) -> E {
    let title = t("位置", "Location");
    if let Some(resource) = &this.resource {
        // The location of a server or a hook goes on past the file into where it sits there.
        let file = resource.location.split("#/").next().unwrap_or_default().to_owned();
        return field_block(title, small(file), None);
    }
    let kind = this.kind;
    let project = level.map(str::to_owned).or_else(|| this.workspaces.iter().flatten().next().map(|w| w.key.clone()));
    let global_tip: Arc<str> = tf("所有对话都能选：~/.mewrk/{folder}", "Every conversation can pick it: ~/.mewrk/{folder}", &[("folder", &folder_of(kind))]).into();
    let project_tip: Arc<str> = match &project {
        Some(key) => {
            let found = this.workspaces.iter().flatten().find(|w| w.key == *key);
            let (machine, path) = found.map_or_else(|| parse_capability_workspace_key(key), |w| (w.machine.clone(), w.path.clone()));
            let place = workspace_location_title(&path, machine.as_ref(), &this.ssh_machines);
            tf("只有这个项目的对话能选：{place}", "Only this project's conversations can pick it: {place}", &[("place", &place)]).into()
        }
        None if this.workspaces.is_none() => t("预设不属于任何项目，这里只能放在全局。", "A preset belongs to no project, so this can only be global.").into(),
        None => t("这个对话还没有项目，只能放在全局。", "This conversation has no project yet, so this can only be global.").into(),
    };
    let chosen = level.is_some();
    // What each choice means is said here rather than on hover. The project's line only while it is chosen, or while it says why it cannot be.
    let line = if chosen { project_tip.to_string() } else if project.is_none() { format!("{global_tip}\n{project_tip}") } else { global_tip.to_string() };
    let options = vec![Segment { id: "global", value: false, label: t("全局", "Global"), tip: global_tip, disabled: false }, Segment { id: "project", value: true, label: t("项目", "Project"), tip: project_tip, disabled: project.is_none() }];
    let st = state.clone();
    let control = segmented(("capability-level", kind.wire()), options, chosen, None, Rc::new(move |_: &mut App, in_project| st.update(|s| s.level = if in_project { project.clone() } else { None }))).aria_label(title).w(168.0);
    field_block(title, control, Some(Hint::new(line)))
}

fn folder_of(kind: CatalogKind) -> String {
    match kind {
        CatalogKind::Skills => "skills",
        CatalogKind::Mcp => "mcp.json",
        _ => "hooks.json",
    }
    .to_owned()
}

fn skill_fields(ctx: &FormContext) -> E {
    let d = &ctx.draft;
    let creating = ctx.this.resource.is_none();
    let directory = if creating {
        let control = ctx.field("directory", &d.directory, |x, v| x.directory = v).placeholder("commit-helper");
        ctx.block(t("文件夹名", "Folder name"), control, ctx.hint(&ctx.complaints.directory, &d.directory, Some(t("模型靠它点名这个技能，保存后不可改。", "The model loads the skill by this name; fixed once saved."))))
    } else {
        field_block(t("文件夹名", "Folder name"), small(d.directory.clone()), Some(Hint::new(t("模型靠它点名这个技能，不可修改。", "The model loads the skill by this name; it cannot be changed."))))
    };
    col()
        .gap(12.0)
        .min_w(0.0)
        .child(directory)
        .child(ctx.block(t("显示名称", "Display name"), ctx.field("name", &d.name, |x, v| x.name = v), Some(Hint::new(t("列表里显示的名字。留空时用正文的第一个标题。", "The label in the list. Blank uses the body's first heading.")))))
        .child(ctx.block(t("描述", "Description"), ctx.field("description", &d.description, |x, v| x.description = v).multiline(56.0, 120.0), Some(Hint::new(t("告诉模型什么时候用它。留空时用正文的第一段。", "Tells the model when to use it. Blank uses the body's first paragraph.")))))
        .child(ctx.block(t("正文", "Body"), ctx.field("body", &d.body, |x, v| x.body = v).multiline(200.0, 360.0).ty(MONO_SMALL), ctx.hint(&ctx.complaints.body, &d.body, Some(t("模型读到的指令，原样交给它。", "The instructions the model reads, verbatim.")))))
}

fn mcp_fields(ctx: &FormContext) -> E {
    let d = &ctx.draft;
    let scope = ctx.scope.as_str();
    let st = ctx.state.clone();
    let transport = segmented(
        ("capability-transport", scope),
        vec![
            Segment { id: "stdio", value: false, label: t("本地命令", "Local command"), tip: t("启动一个本机进程，用 stdio 通信。", "Starts a process and talks to it over stdio.").into(), disabled: false },
            Segment { id: "http", value: true, label: "HTTP", tip: t("连接一个 Streamable HTTP 服务器。", "Connects to a Streamable HTTP server.").into(), disabled: false },
        ],
        d.http,
        None,
        Rc::new(move |_: &mut App, http| st.update(|s| s.draft.http = http)),
    )
    .aria_label(t("连接方式", "Transport"))
    .w(200.0);
    let references = t("值里可以写 ${VAR} 引用环境变量，密钥就不用写进文件。", "Values can use ${VAR} to read an environment variable, which keeps secrets out of the file.");
    let connection = if d.http {
        col()
            .gap(12.0)
            .child(ctx.block("URL", ctx.field("url", &d.url, |x, v| x.url = v).placeholder("https://example.com/mcp"), ctx.hint(&ctx.complaints.url, &d.url, Some(t("公网地址要用 https；本机和内网可以用 http。", "Public hosts need https; loopback and private addresses may use http.")))))
            .child(ctx.block(t("请求头", "Headers"), ctx.field("headers", &d.headers, |x, v| x.headers = v).multiline(56.0, 140.0).ty(MONO_SMALL).placeholder("Authorization: Bearer ${TOKEN}"), ctx.hint(&ctx.complaints.headers, &d.headers, Some(references))))
    } else {
        col()
            .gap(12.0)
            .child(ctx.block(t("命令", "Command"), ctx.field("command", &d.command, |x, v| x.command = v).ty(MONO_SMALL).placeholder("npx"), ctx.hint(&ctx.complaints.command, &d.command, Some(t("直接运行，不经过 shell。", "Runs directly, never through a shell.")))))
            .child(ctx.block(t("参数", "Arguments"), ctx.field("args", &d.args, |x, v| x.args = v).multiline(56.0, 140.0).ty(MONO_SMALL), Some(Hint::new(t("每行一个。", "One a line.")))))
            .child(ctx.block(t("环境变量", "Environment variables"), ctx.field("env", &d.env, |x, v| x.env = v).multiline(56.0, 140.0).ty(MONO_SMALL).placeholder("LOG_LEVEL=info"), ctx.hint(&ctx.complaints.env, &d.env, Some(references))))
            .child(ctx.block(t("工作目录", "Working folder"), ctx.field("cwd", &d.cwd, |x, v| x.cwd = v).ty(MONO_SMALL), Some(Hint::new(t("可选。要是已存在的绝对路径。", "Optional. Has to be an existing absolute path.")))))
    };
    col()
        .gap(12.0)
        .min_w(0.0)
        .child(ctx.block(t("名称", "Name"), ctx.field("name", &d.name, |x, v| x.name = v), ctx.hint(&ctx.complaints.name, &d.name, Some(t("只能用字母、数字、连字符和下划线。", "Letters, digits, hyphens and underscores only.")))))
        .child(ctx.block(t("描述", "Description"), ctx.field("description", &d.description, |x, v| x.description = v).multiline(40.0, 100.0), Some(Hint::new(t("显示在列表里，选中后也会告诉模型。", "Shown in the list, and to the model once selected.")))))
        .child(field_block(t("连接方式", "Transport"), transport, None))
        .child(connection)
        .child(ctx.block(t("超时（秒）", "Timeout (seconds)"), ctx.field("timeout", &d.timeout, |x, v| x.timeout = v), ctx.hint(&ctx.complaints.timeout, &d.timeout, Some(t("单次请求的上限，最多 300。留空用默认的 45 秒。", "The limit on one request, at most 300. Blank uses the default 45 s.")))))
}

fn hook_fields(ctx: &FormContext) -> E {
    let d = &ctx.draft;
    let scope = ctx.scope.as_str();
    let event_label = |id: &str| HOOK_EVENTS.iter().find(|(e, ..)| *e == id).map_or_else(|| id.to_owned(), |(e, zh, en)| format!("{e} · {}", t(zh, en)));
    let rows = HOOK_EVENTS
        .iter()
        .map(|(event, zh, en)| {
            let st = ctx.state.clone();
            SelectRow::new(*event, format!("{event} · {}", t(zh, en)), move |_: &mut App| st.update(|s| s.draft.event = (*event).to_owned())).checked(d.event == *event)
        })
        .collect();
    let event = SelectBox::new(format!("capability-event:{scope}"), event_label(&d.event), rows).view().aria_label(t("事件", "Event"));
    let matcher_hint = match d.event.as_str() {
        "SessionStart" => t("正则表达式，匹配 startup。留空匹配全部。", "A regular expression tested against startup. Blank matches everything."),
        "InstructionsLoaded" => t("正则表达式，匹配载入原因。留空匹配全部。", "A regular expression tested against the load reason. Blank matches everything."),
        "PreToolUse" | "PermissionRequest" | "PostToolUse" => t("正则表达式，匹配工具名，比如 ^bash$。留空匹配全部。", "A regular expression tested against the tool name, like ^bash$. Blank matches everything."),
        _ => t("这个事件不看匹配器，留空即可。", "This event does not use a matcher; leave it blank."),
    };
    col()
        .gap(12.0)
        .min_w(0.0)
        .child(ctx.block(t("名称", "Name"), ctx.field("name", &d.name, |x, v| x.name = v), Some(Hint::new(t("列表里显示的名字。可选。", "The label in the list. Optional.")))))
        .child(field_block(t("事件", "Event"), event, Some(Hint::new(t("这个钩子在什么时候运行。", "When this hook runs.")))))
        .child(ctx.block(t("匹配器", "Matcher"), ctx.field("matcher", &d.matcher, |x, v| x.matcher = v).ty(MONO_SMALL), Some(Hint::new(matcher_hint))))
        .child(ctx.block(t("命令", "Command"), ctx.field("command", &d.command, |x, v| x.command = v).multiline(56.0, 160.0).ty(MONO_SMALL), ctx.hint(&ctx.complaints.command, &d.command, Some(t("通过 bash -lc 运行，从 stdin 读事件 JSON。", "Runs through bash -lc and reads the event as JSON on stdin.")))))
        .child(ctx.block(t("Windows 命令", "Windows command"), ctx.field("commandWindows", &d.command_windows, |x, v| x.command_windows = v).multiline(40.0, 120.0).ty(MONO_SMALL), Some(Hint::new(t("可选。在 Windows 上替代上面的命令。", "Optional. Replaces the command above on Windows.")))))
        .child(ctx.block(t("超时（秒）", "Timeout (seconds)"), ctx.field("timeout", &d.timeout, |x, v| x.timeout = v), ctx.hint(&ctx.complaints.timeout, &d.timeout, Some(t("1 到 600，默认 30。", "1 to 600; 30 by default.")))))
        .child(ctx.block(t("状态文字", "Status message"), ctx.field("statusMessage", &d.status_message, |x, v| x.status_message = v), Some(Hint::new(t("运行时显示的文字。可选。", "Shown while it runs. Optional.")))))
}

/// Which window is open over a page. An edit holds the entry as it was opened: a rescan that no longer finds it must not close the window under the user.
#[derive(Clone)]
enum OpenEditor {
    Create,
    Edit(ResourceDescriptor),
}

/// A catalog page with the way to add to it and edit what is on it: the "new" button goes where a role page has one, above the page's own footer, and each row gets an edit button before its delete. A built-in has no file to edit. `on_saved` says what the page does about its selection.
pub fn editable_capability_page(app: &mut App, mut page: CapabilityPage, handlers: &Handlers, add_label: &str, on_saved: Rc<dyn Fn(&mut App, Option<String>, String)>) -> E {
    if handlers.on_read_capability.is_none() || handlers.on_save_capability.is_none() {
        return page.view(app);
    }
    let t_ = theme();
    let kind = page.kind;
    let list_id = page.list_id.clone();
    let open = use_state(("capability-editor-open", list_id.as_str()), || None::<OpenEditor>);
    let new_open = open.clone();
    let add_text = add_label.to_owned();
    let add = row().key(add_button_key(&list_id)).center().gap(5.0).min_h(34.0).w_full().radius(7.0).border(t_.control_stroke).hover_bg(t_.subtle_hover).role(Role::Button).aria_label(add_text.clone()).on_click(move |_| new_open.set(Some(OpenEditor::Create))).child(icon("Plus", 14.0).color(t_.text_secondary)).child(label(add_text));
    page.footer = Some(col().gap(14.0).child(add).child(page.footer.take()));
    let (edit_open, edit_list) = (open.clone(), list_id.clone());
    page.row_actions = Some(Rc::new(move |resource: &ResourceDescriptor| {
        if resource.source == ResourceSource::Builtin {
            return Vec::new();
        }
        let (slot, entry) = (edit_open.clone(), resource.clone());
        vec![icon_btn(edit_button_key(&edit_list, &resource.id), "Pencil", tf("编辑 {name}", "Edit {name}", &[("name", &resource.name)])).on_click(move |_| slot.set(Some(OpenEditor::Edit(entry.clone()))))]
    }));
    let tone_of = page.tone_of.clone();
    let selected = page.selected_ids.clone();
    let (workspaces, ssh_machines) = (page.workspaces.clone(), page.ssh_machines.clone());
    let mut out = col().min_w(0.0).child(page.view(app));
    if let Some(editing) = open.get() {
        let resource = match editing {
            OpenEditor::Create => None,
            OpenEditor::Edit(resource) => Some(resource),
        };
        let cache_note = resource.as_ref().and_then(|r| tone_of.as_ref()?(&r.id, selected.contains(&r.id))).map(|tone| lock_hint(tone).to_owned());
        let closer = open.clone();
        out = out.child(CapabilityEditor { kind, resource, cache_note, workspaces, ssh_machines, handlers: handlers.clone(), on_saved, on_close: Rc::new(move |_| closer.set(None)) }.view(app));
    }
    out
}
