//! `CatalogRow.tsx` and `CapabilitySelectionPage` from `ConversationSettingsPages.tsx`: one line per catalog entry, and the one page every
//! catalog of named things is drawn on.
//!
//! The catalogs differ only in what they are called and which of them a run can lock, so they share one page rather than near-copies.
//! Entries are read from the global `~/.mewrk` and from each workspace, so the page is divided that way, and a conversation uses the union.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::{ResourceDescriptor, ResourceSource, RunTarget, SshMachineConfig, UnreadableLevel};
use mewrk_client::prelude::*;

use crate::lock::{lock_hint, LockTone};
use crate::logic::{catalog_order, parse_capability_workspace_key, reorder_ids, sort_by_order, workspace_location_title, workspace_machine_label, write_catalog_order, CapabilityWorkspace, OrderScope};
use crate::ops::McpProbeReport;
use crate::widgets::*;

// ---------------------------------------------------------------- rows

#[derive(Default)]
struct DragState {
    source: Option<String>,
    moved: bool,
    /// The row and whether the drop lands before it.
    target: Option<(String, bool)>,
    /// A drag never ends in a click: the release that finished one still counts as a click on the row it started on when it ends there.
    swallow_click: bool,
}

/// The drag-sort a catalog list is under. `list_id` must be unique among the lists on screen, because a preset's window mounts a second copy.
#[derive(Clone)]
pub struct CatalogSort {
    pub list_id: String,
    /// The row ids in their current visible order, for keyboard reordering and hit-testing a drop.
    pub ids: Rc<Vec<String>>,
    /// False while a search is on: adjacency means nothing then.
    pub enabled: bool,
    pub on_reorder: Rc<dyn Fn(&mut App, String, String, bool)>,
}

pub fn catalog_row_key(list_id: &str, id: &str) -> u64 {
    hash_key(("catalog-row", list_id, id))
}

impl CatalogSort {
    fn state(&self) -> Local<DragState> {
        use_state(("catalog-drag", self.list_id.as_str()), DragState::default)
    }

    fn drop_target_at(&self, source: &str, y: f32) -> Option<(String, bool)> {
        for id in self.ids.iter() {
            if id == source {
                continue;
            }
            if let Some(r) = rect_of(catalog_row_key(&self.list_id, id))
                && y >= r.y
                && y < r.y + r.h
            {
                return Some((id.clone(), y < r.y + r.h / 2.0));
            }
        }
        None
    }

    /// A list row moved one place up (`-1`) or down (`1`) from the keyboard.
    pub fn move_by_keyboard(&self, app: &mut App, id: &str, direction: i32) {
        let Some(index) = self.ids.iter().position(|i| i == id) else { return };
        let Some(neighbour) = index.checked_add_signed(direction as isize).and_then(|i| self.ids.get(i)) else { return };
        (self.on_reorder)(app, id.to_owned(), neighbour.clone(), direction < 0);
    }
}

#[derive(Clone)]
pub struct Badge {
    pub text: String,
    pub warning: bool,
}

impl Badge {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), warning: false }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self { text: text.into(), warning: true }
    }

    fn view(self) -> E {
        let t_ = theme();
        say(&self.text);
        text(self.text).ty(FINE).nowrap().color(if self.warning { t_.caution } else { t_.text_tertiary }).px(4.0).py(1.0).radius(4.0).when(self.warning, |e| e.bg(t_.caution_bg))
    }
}

pub struct Toggle {
    pub label: String,
    pub disabled: bool,
    pub on_toggle: Rc<dyn Fn(&mut App)>,
}

/// One line in a catalog: what the row is goes on the left, what it is called fills the middle, what can be done to it sits on the right,
/// ending in delete. It is one line tall, so a path or a model binding rides `detail`, which only a reader says, a path also comes up on hover as
/// `location`, and what is wrong with the entry goes under its name as `note`. The body is the drag handle; a row that is on or off (`toggle`) takes a click too, and a drag never ends in one.
#[derive(Default)]
pub struct CatalogRow {
    pub id: Option<String>,
    pub name: String,
    pub detail: Option<String>,
    /// Where the entry lives, drawn while the pointer rests on its name.
    pub location: Option<String>,
    /// Why the entry cannot run, said under its name.
    pub note: Option<String>,
    pub badge: Option<Badge>,
    pub icon: Option<&'static str>,
    pub lead: Option<E>,
    pub actions: Vec<E>,
    pub name_editor: Option<E>,
    pub on: bool,
    pub tone: Option<LockTone>,
    pub toggle: Option<Toggle>,
    pub sort: Option<CatalogSort>,
    /// Identity for a row that cannot be arranged, so it can still be found.
    pub fallback_key: Option<String>,
}

impl CatalogRow {
    pub fn view(self) -> E {
        let t_ = theme();
        let sortable = self.id.is_some() && self.sort.as_ref().is_some_and(|s| s.enabled);
        let list_id = self.sort.as_ref().map_or("", |s| s.list_id.as_str()).to_owned();
        let id = self.id.clone().unwrap_or_else(|| self.fallback_key.clone().unwrap_or_else(|| self.name.clone()));
        let key = catalog_row_key(&list_id, &id);
        let mut reader_lines: Vec<String> = Vec::new();
        if let Some(d) = self.detail.filter(|d| !d.is_empty()) {
            reader_lines.push(d);
        }
        if sortable {
            let k = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
            reader_lines.push(tf("拖动整行排序 · {key} + ↑/↓ 键盘排序", "Drag the row to reorder · {key} + ↑/↓ to reorder with the keyboard", &[("key", &k)]));
        }
        let picks = self.toggle.is_some();
        let disabled_pick = self.toggle.as_ref().is_some_and(|tg| tg.disabled);
        let spoken: Arc<str> = Arc::from(self.toggle.as_ref().map_or(self.name.as_str(), |tg| tg.label.as_str()));
        let tint = if self.on || picks { cache_fill(self.tone) } else { None };
        let fill = tint.or((self.on && picks).then_some(t_.success_bg));
        say(&self.name);
        let mut el = row()
            .key(key)
            .items_center()
            .gap(8.0)
            .w_full()
            .min_h(30.0)
            .px(7.0)
            .radius(6.0)
            .shrink0()
            .min_w(0.0)
            .hover_bg(t_.subtle_hover)
            .when(disabled_pick, |e| e.opacity(0.6))
            // A row on the move is one stop, the keyboard's way to arrange it. One that picks is a button whatever it holds beside its name.
            .when(picks || sortable, |e| e.aria_label(spoken))
            .when(picks, |e| e.role(Role::Button).aria_pressed(self.on).aria_disabled(disabled_pick))
            .when(sortable, |e| e.aria_keyshortcuts("Alt+ArrowUp Alt+ArrowDown"));
        if let Some(fill) = fill {
            el = el.bg(fill);
        }
        if !reader_lines.is_empty() {
            el = el.tip(reader_lines.join("\n"));
        }
        let on_toggle = self.toggle.as_ref().filter(|tg| !tg.disabled).map(|tg| tg.on_toggle.clone());
        if let Some(sort) = self.sort.clone().filter(|_| sortable) {
            let drag = sort.state();
            // Drawn where a drop would land, so a reader can see it before letting go.
            if let Some((_, before)) = drag.with(|d| d.target.clone()).filter(|(t, _)| *t == id) {
                let line = div().absolute().left(0.0).right(0.0).h(2.0).bg(t_.accent).pass_through();
                el = el.child(if before { line.top(-1.0) } else { line.bottom(-1.0) });
            }
            if drag.with(|d| d.moved && d.source.as_deref() == Some(id.as_str())) {
                el = el.opacity(0.5);
            }
            let (source_id, moving) = (id.clone(), sort.clone());
            let drag_for_pointer = drag.clone();
            el = el.on_pointer(move |app: &mut App, e| match e.phase {
                Phase::Down => drag_for_pointer.set(DragState { source: Some(source_id.clone()), ..DragState::default() }),
                Phase::Move => {
                    if e.dy.abs() < 4.0 && !drag_for_pointer.with(|d| d.moved) {
                        return;
                    }
                    let target = moving.drop_target_at(&source_id, e.y);
                    drag_for_pointer.update(|d| {
                        d.moved = true;
                        d.target = target;
                    });
                }
                Phase::Up => {
                    let (moved, target) = drag_for_pointer.with(|d| (d.moved, d.target.clone()));
                    drag_for_pointer.set(DragState { swallow_click: moved, ..DragState::default() });
                    if let (true, Some((target, before))) = (moved, target) {
                        (moving.on_reorder)(app, source_id.clone(), target, before);
                    }
                }
            });
            let (key_id, key_sort) = (id.clone(), sort.clone());
            el = el.focusable().on_key(move |app: &mut App, press| {
                if !press.alt || !matches!(press.key, Key::Up | Key::Down) {
                    return false;
                }
                key_sort.move_by_keyboard(app, &key_id, if press.key == Key::Up { -1 } else { 1 });
                true
            });
            let swallow = drag;
            if let Some(toggle) = on_toggle {
                el = el.on_click(move |app: &mut App| {
                    if swallow.with(|d| d.swallow_click) {
                        swallow.update(|d| d.swallow_click = false);
                        return;
                    }
                    toggle(app);
                });
            }
        } else if let Some(toggle) = on_toggle {
            el = el.on_click(move |app: &mut App| toggle(app));
        }
        // A field being typed in has nothing to add on hover, and the row's place doesn't belong over it.
        let location = self.location.filter(|_| self.name_editor.is_none());
        let name_view = match self.note {
            Some(note) => col().flex1().min_w(0.0).py(4.0).child(text(self.name.clone()).ty(CAPTION).ellipsis().min_w(0.0)).child(error_text(note)),
            None => self.name_editor.unwrap_or_else(|| text(self.name.clone()).ty(CAPTION).ellipsis().flex1().min_w(0.0)),
        };
        // A name cut short comes up with it, since the two share one place to rest on.
        let name_view = match location {
            Some(location) => name_view.cut_text(format!("{}\n{location}", self.name)).hover_text(location),
            None => name_view,
        };
        if !picks {
            el = el.child(self.lead);
        }
        el = el
            .child(self.icon.map(|name| icon(name, 13.0).color(t_.text_secondary)))
            .child(name_view)
            .child(self.badge.map(Badge::view));
        if !self.actions.is_empty() {
            el = el.child(row().items_center().gap(2.0).shrink0().children(self.actions));
        }
        if picks {
            say(if self.on { "sign:-" } else { "sign:+" });
            el = el.child(icon(if self.on { "Minus" } else { "Plus" }, 14.0).color(t_.text_tertiary).shrink0());
        }
        el
    }
}

/// The list every catalog page draws its rows into.
pub fn catalog_list(rows: Vec<E>) -> E {
    col().gap(1.0).min_w(0.0).children(rows)
}

// ---------------------------------------------------------------- the page

/// Which catalog a page lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CatalogKind {
    Skills,
    Mcp,
    Hooks,
    Agents,
    ToolDescriptions,
}

impl CatalogKind {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Skills => "Box",
            Self::Mcp => "Blocks",
            Self::Hooks => "FolderCog",
            Self::Agents => "Bot",
            Self::ToolDescriptions => "ScrollText",
        }
    }

    /// The name the host's `reveal_capability_location` and the docs site know it by.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Skills => "skills",
            Self::Mcp => "mcp",
            Self::Hooks => "hooks",
            Self::Agents => "agents",
            Self::ToolDescriptions => "toolDescriptions",
        }
    }

    fn order_scope(self) -> OrderScope {
        match self {
            Self::Skills => OrderScope::Skills,
            Self::Mcp => OrderScope::Mcp,
            Self::Hooks => OrderScope::Hooks,
            Self::Agents => OrderScope::Agents,
            Self::ToolDescriptions => OrderScope::ToolDescriptions,
        }
    }
}

/// What a connection test on an MCP row currently knows, held per id so a re-test replaces the last result.
#[derive(Clone, Debug, PartialEq)]
pub enum McpProbeState {
    Running,
    Ok { tool_count: usize, detail: String },
    Failed { detail: String },
}

pub type ResourceFn<R> = Rc<dyn Fn(&ResourceDescriptor) -> R>;

pub struct CapabilityPage {
    pub kind: CatalogKind,
    /// Unique among the lists on screen: the preset editor mounts a second copy.
    pub list_id: String,
    pub resources: Vec<ResourceDescriptor>,
    pub selected_ids: Vec<String>,
    /// At most one entry is selected; turning one on replaces the other.
    pub single: bool,
    /// How the conversation's lock draws a row.
    pub tone_of: Option<Rc<dyn Fn(&str, bool) -> Option<LockTone>>>,
    pub on_change: Rc<dyn Fn(&mut App, Vec<String>)>,
    /// Unticks a dangling selection past the lock: clearing it is never toned and never refused.
    pub on_untick_dangling: Option<Rc<dyn Fn(&mut App, String)>>,
    /// Removes the entry from the catalog itself, not merely from this conversation.
    pub on_delete: Option<Rc<dyn Fn(&mut App, ResourceDescriptor)>>,
    pub error: Option<String>,
    pub unreadable_levels: Vec<UnreadableLevel>,
    pub dangling_detail: Option<String>,
    pub selected_unavailable_detail: Option<String>,
    pub search_label: String,
    pub empty_title: String,
    pub empty_description: Hint,
    /// Controls below the list for a policy over the whole catalog, drawn whether or not the catalog has anything in it.
    pub footer: Option<E>,
    /// `None` is the preset editor, which points at no conversation and shows every workspace the catalog has entries of.
    pub workspaces: Option<Vec<CapabilityWorkspace>>,
    pub ssh_machines: Vec<SshMachineConfig>,
    pub on_rescan: Option<Rc<dyn Fn(&mut App)>>,
    pub on_reveal: Option<Rc<dyn Fn(&mut App, CatalogKind, Option<String>)>>,
    pub on_probe: Option<crate::ops::ProbeFn>,
    pub row_actions: Option<ResourceFn<Vec<E>>>,
    pub row_detail: Option<ResourceFn<Option<String>>>,
    pub row_badge: Option<ResourceFn<Option<Badge>>>,
}

fn machine_icon(machine: Option<&RunTarget>) -> &'static str {
    match machine {
        None => "Folder",
        Some(RunTarget::Wsl { .. }) => "SquareTerminal",
        Some(RunTarget::Ssh { .. }) => "Server",
    }
}

/// Unlike the toolbar's count, this reads the rows handed to it.
fn section_count(rows: &[ResourceDescriptor], selected: &HashSet<&String>) -> E {
    let t_ = theme();
    let s = format!("{} / {}", rows.iter().filter(|r| selected.contains(&r.id)).count(), rows.len());
    say(&s);
    text(s).ty(FINE).color(t_.text_tertiary).nowrap().shrink0()
}

/// One section of a page: the global level or one workspace. Each is its own sortable list, so a row is only ever dragged among the rows of
/// the level it was read from.
fn capability_section(list_id: String, label: String, first: bool, heading: Vec<E>, ids: Rc<Vec<String>>, sortable: bool, on_reorder: Rc<dyn Fn(&mut App, String, String, bool)>, notice: Option<String>, empty: Option<String>, rows: impl FnOnce(CatalogSort) -> Vec<E>) -> E {
    let sort = CatalogSort { list_id, ids: ids.clone(), enabled: sortable, on_reorder };
    col()
        .gap(3.0)
        .min_w(0.0)
        .when(!first, |e| e.pt(8.0).border_top(theme().divider))
        .role(Role::Region)
        .aria_label(label)
        .child(row().items_center().gap(6.0).min_h(24.0).px(4.0).min_w(0.0).children(heading))
        .child(notice.map(error_text))
        .when(!ids.is_empty(), |e| e.child(catalog_list(rows(sort))))
        .child(if ids.is_empty() { empty.map(|s| small(s).px(4.0).pt(2.0)) } else { None })
}

impl CapabilityPage {
    /// The rows this conversation may select: the global level and each of its workspaces. Anything else belongs to a workspace this
    /// conversation does not have, so a selection of one shows as dangling, as the host treats it.
    fn scoped(&self) -> Vec<ResourceDescriptor> {
        match &self.workspaces {
            None => self.resources.clone(),
            Some(ws) => self.resources.iter().filter(|r| r.workspace_key.as_ref().is_none_or(|k| ws.iter().any(|w| w.key == *k))).cloned().collect(),
        }
    }

    /// The workspaces drawn as sections. The preset editor draws every workspace the catalog read something from, or tried to.
    fn sections(&self) -> Vec<CapabilityWorkspace> {
        if let Some(ws) = &self.workspaces {
            return ws.clone();
        }
        let mut seen = HashSet::new();
        self.resources
            .iter()
            .filter_map(|r| r.workspace_key.as_deref())
            .chain(self.unreadable_levels.iter().map(|l| l.workspace_key.as_str()))
            .filter(|k| !k.is_empty() && seen.insert(k.to_string()))
            .map(|key| {
                let (machine, path) = parse_capability_workspace_key(key);
                CapabilityWorkspace { number: 0, key: key.to_owned(), machine, path }
            })
            .collect()
    }

    pub fn view(self, app: &mut App) -> E {
        let t_ = theme();
        let list_id = self.list_id.clone();
        let query = use_state(("capability-query", list_id.as_str()), String::new);
        let probes = use_state(("capability-probes", list_id.as_str()), HashMap::<String, McpProbeState>::new);
        let order = use_state(("catalog-order", self.kind, list_id.as_str()), || catalog_order(app, self.kind.order_scope()));
        let sections = self.sections();
        let scoped = self.scoped();
        let ordered = sort_by_order(&scoped, &order.borrow(), |r| r.id.as_str());
        let selected_set: HashSet<&String> = self.selected_ids.iter().collect();
        let known: HashSet<&String> = scoped.iter().map(|r| &r.id).collect();
        // A selection whose resource has left the catalog is kept: the scan may simply not have reached it yet.
        let dangling: Vec<String> = self.selected_ids.iter().filter(|id| !known.contains(id)).cloned().collect();
        let needle = query.with(|q| q.trim().to_lowercase());
        let matches = |hay: &str| hay.to_lowercase().contains(&needle);
        let visible: Vec<ResourceDescriptor> = if needle.is_empty() { ordered.clone() } else { ordered.iter().filter(|r| matches(&r.name) || matches(&r.description) || matches(&r.location)).cloned().collect() };
        let visible_dangling: Vec<String> = if needle.is_empty() { dangling.clone() } else { dangling.iter().filter(|id| matches(id)).cloned().collect() };
        let rows_of = |key: Option<&str>| -> Vec<ResourceDescriptor> { visible.iter().filter(|r| r.workspace_key.as_deref() == key).cloned().collect() };

        let reorder: Rc<dyn Fn(&mut App, String, String, bool)> = {
            let ordered_ids: Vec<String> = ordered.iter().map(|r| r.id.clone()).collect();
            let (order, scope) = (order.clone(), self.kind.order_scope());
            Rc::new(move |app: &mut App, source, target, before| {
                let next = reorder_ids(&ordered_ids, &source, &target, before);
                order.set(next.clone());
                write_catalog_order(app, scope, next);
            })
        };

        let on_change = self.on_change.clone();
        let current = self.selected_ids.clone();
        let single = self.single;
        let toggle: Rc<dyn Fn(&mut App, &str, bool)> = Rc::new(move |app: &mut App, id: &str, checked: bool| {
            let next: Vec<String> = if single {
                if checked { vec![id.to_owned()] } else { Vec::new() }
            } else {
                let mut next: Vec<String> = current.iter().filter(|e| *e != id).cloned().collect();
                if checked {
                    next.push(id.to_owned());
                }
                next
            };
            on_change(app, next);
        });

        // A missing skill, server or hook fails every run. A role is the exception: the host simply does not offer one it cannot find.
        let agents = self.kind == CatalogKind::Agents;
        let dangling_detail = self.dangling_detail.clone().unwrap_or_else(|| {
            if agents { t("找不到这个角色的文件，模型看不到它；可以取消勾选。", "This role's file is gone, so the model does not see it; untick it.") } else { t("目录中已不存在；取消勾选之前，每次运行都会失败", "No longer in the catalog. Every run fails until it is unchecked.") }.to_owned()
        });
        let unusable_selected = self.selected_unavailable_detail.clone().unwrap_or_else(|| {
            if agents { t("已选择，但文件无法使用，模型看不到它；修好文件或取消勾选。", "Selected, but the file cannot be used, so the model does not see it; fix the file or untick it.") } else { t("已选择；修好或取消勾选之前，每次运行都会失败", "Selected. Every run fails until it is fixed or unchecked.") }.to_owned()
        });

        let row_ctx = Rc::new(RowContext {
            kind: self.kind,
            tone_of: self.tone_of.clone(),
            on_delete: self.on_delete.clone(),
            on_probe: self.on_probe.clone(),
            row_actions: self.row_actions.clone(),
            row_detail: self.row_detail.clone(),
            row_badge: self.row_badge.clone(),
            probes: probes.clone(),
            toggle: toggle.clone(),
            unusable_selected,
            list_id: list_id.clone(),
        });

        let empty_note = if !scoped.is_empty() || !dangling.is_empty() { Some(t("这里还没有条目", "Nothing here yet").to_owned()) } else { None };
        let render_rows = |rows: &[ResourceDescriptor], sort: CatalogSort, selected: &HashSet<&String>| -> Vec<E> { rows.iter().map(|r| row_ctx.row(r, sort.clone(), selected.contains(&r.id))).collect() };

        let mut sections_col = col().gap(10.0).min_w(0.0);
        let global_rows = rows_of(None);
        let mut first_section = true;
        if needle.is_empty() || !global_rows.is_empty() {
            let ids: Rc<Vec<String>> = Rc::new(global_rows.iter().map(|r| r.id.clone()).collect());
            say(t("全局", "Global"));
            say("~/.mewrk");
            let mut heading: Vec<E> = vec![
                icon("Home", 13.0).color(t_.text_secondary),
                text(t("全局", "Global")).ty(CAPTION_STRONG),
                text("~/.mewrk").ty(FINE).color(t_.text_tertiary).flex1(),
                section_count(&global_rows, &selected_set),
            ];
            if let Some(reveal) = self.on_reveal.clone() {
                let kind = self.kind;
                heading.push(icon_btn(("reveal", list_id.as_str(), "global"), "FolderOpen", t("打开全局配置目录", "Open the global config folder")).size(22.0, 22.0).on_click(move |app: &mut App| reveal(app, kind, None)));
            }
            let (rows_ref, sel) = (&global_rows, &selected_set);
            first_section = false;
            sections_col = sections_col.child(capability_section(format!("{list_id}:global"), t("全局", "Global").to_owned(), true, heading, ids, needle.is_empty(), reorder.clone(), None, if needle.is_empty() { empty_note.clone() } else { None }, |sort| render_rows(rows_ref, sort, sel)));
        }
        for workspace in &sections {
            let rows = rows_of(Some(&workspace.key));
            if !needle.is_empty() && rows.is_empty() {
                continue;
            }
            let machine = workspace_machine_label(workspace.machine.as_ref(), &self.ssh_machines);
            let ids: Rc<Vec<String>> = Rc::new(rows.iter().map(|r| r.id.clone()).collect());
            say(&workspace.path);
            let location = workspace_location_title(&workspace.path, workspace.machine.as_ref(), &self.ssh_machines);
            let mut heading: Vec<E> = vec![icon(machine_icon(workspace.machine.as_ref()), 13.0).color(t_.text_secondary), mewrk_ui_common::path_text::path_text(("capability-section-path", list_id.as_str(), workspace.key.as_str()), workspace.path.clone(), CAPTION_STRONG, t_.text_secondary).min_w(0.0).full_text(location.clone())];
            if let Some(m) = machine {
                say(&m);
                heading.push(text(m).ty(FINE).color(t_.text_tertiary).nowrap().shrink0());
            }
            heading.push(div().flex1());
            heading.push(section_count(&rows, &selected_set));
            if let Some(reveal) = self.on_reveal.clone() {
                let (kind, key) = (self.kind, workspace.key.clone());
                let tip = tf("打开 {path} 的配置目录", "Open the config folder of {path}", &[("path", &workspace.path)]);
                heading.push(icon_btn(("reveal", list_id.as_str(), workspace.key.as_str()), "FolderOpen", tip).size(22.0, 22.0).on_click(move |app: &mut App| reveal(app, kind, Some(key.clone()))));
            }
            let notice = self.unreadable_levels.iter().find(|l| l.workspace_key == workspace.key).map(|l| l.message.clone());
            let (rows_ref, sel) = (&rows, &selected_set);
            sections_col = sections_col.child(capability_section(format!("{list_id}:{}", workspace.key), location, std::mem::replace(&mut first_section, false), heading, ids, needle.is_empty(), reorder.clone(), notice, if needle.is_empty() { empty_note.clone() } else { None }, |sort| render_rows(rows_ref, sort, sel)));
        }

        // Never toned: the entry it named has nothing left to declare, so unticking it rewrites no cache.
        if !visible_dangling.is_empty() {
            let rows = visible_dangling
                .iter()
                .map(|id| {
                    let id_for_toggle = id.clone();
                    let (untick, on_change, current) = (self.on_untick_dangling.clone(), self.on_change.clone(), self.selected_ids.clone());
                    CatalogRow {
                        name: id.clone(),
                        fallback_key: Some(id.clone()),
                        note: Some(dangling_detail.clone()),
                        badge: Some(Badge::warning(t("悬空", "Dangling"))),
                        on: true,
                        toggle: Some(Toggle {
                            label: id.clone(),
                            disabled: false,
                            on_toggle: Rc::new(move |app: &mut App| match &untick {
                                Some(f) => f(app, id_for_toggle.clone()),
                                None => on_change(app, current.iter().filter(|e| **e != id_for_toggle).cloned().collect()),
                            }),
                        }),
                        ..CatalogRow::default()
                    }
                    .view()
                })
                .collect();
            sections_col = sections_col.child(catalog_list(rows));
        }

        if visible.is_empty() && visible_dangling.is_empty() {
            let body = if needle.is_empty() {
                col().items_center().gap(3.0).child(icon(self.kind.icon(), 18.0).color(t_.text_tertiary)).child(strong(self.empty_title.clone())).child(self.empty_description.clone().view())
            } else {
                col().items_center().gap(3.0).child(icon("Search", 18.0).color(t_.text_tertiary)).child(strong(t("没有匹配的条目", "No matching entry"))).child(small(t("搜索会匹配名称、说明与所在路径。", "Search matches names, descriptions and paths.")))
            };
            sections_col = sections_col.child(div().py(26.0).px(12.0).radius(9.0).border(t_.control_stroke).bg(t_.card_secondary).child(body));
        }

        let selected_in_scope = scoped.iter().filter(|r| selected_set.contains(&r.id)).count();
        let count_text = tf("{selected} / {total} 个已选", "{selected} / {total} selected", &[("selected", &selected_in_scope), ("total", &scoped.len())]);
        let query_write = query.clone();
        say(&self.search_label);
        let search = row()
            .items_center()
            .gap(6.0)
            .flex1()
            .min_w(120.0)
            .h(28.0)
            .px(8.0)
            .radius(6.0)
            .bg(t_.control_fill)
            .border(t_.control_stroke)
            .child(icon("Search", 13.0).color(t_.text_tertiary))
            // One line tall rather than a field's own 30, which would sit its text at the top of this box.
            .child(field(("capability-search", list_id.as_str()), query.get()).placeholder(self.search_label.clone()).bare().on_change(move |_, v| query_write.set(v)).build().flex1().h(18.0).aria_label(self.search_label.as_str()));
        let toolbar = row()
            .wrap()
            .items_center()
            .gap(7.0)
            .min_w(0.0)
            .child(search)
            .child(small(count_text).nowrap().shrink0())
            .child(self.on_rescan.clone().map(|rescan| icon_btn(("rescan", list_id.as_str()), "RefreshCw", t("重新扫描", "Rescan")).on_click(move |app: &mut App| rescan(app))))
            .child(docs_link(self.kind.wire(), None));

        col().gap(14.0).min_w(0.0).child(self.error.clone().map(error_text)).child(toolbar).child(sections_col).child(self.footer)
    }
}

struct RowContext {
    kind: CatalogKind,
    tone_of: Option<Rc<dyn Fn(&str, bool) -> Option<LockTone>>>,
    on_delete: Option<Rc<dyn Fn(&mut App, ResourceDescriptor)>>,
    on_probe: Option<crate::ops::ProbeFn>,
    row_actions: Option<ResourceFn<Vec<E>>>,
    row_detail: Option<ResourceFn<Option<String>>>,
    row_badge: Option<ResourceFn<Option<Badge>>>,
    probes: Local<HashMap<String, McpProbeState>>,
    toggle: Rc<dyn Fn(&mut App, &str, bool)>,
    unusable_selected: String,
    list_id: String,
}

impl RowContext {
    fn row(&self, resource: &ResourceDescriptor, sort: CatalogSort, is_selected: bool) -> E {
        let row_tone = self.tone_of.as_ref().and_then(|f| f(&resource.id, is_selected));
        // An entry the last request carried is not deleted out from under it.
        let in_play = is_selected && row_tone.is_some();
        let probe = self.probes.with(|p| p.get(&resource.id).cloned());
        // The badge is one slot with one meaning: what a test just said, or that the entry cannot run. An unavailable row is never probed.
        let badge = match &probe {
            Some(McpProbeState::Running) => Some(Badge::plain(t("测试中…", "Testing…"))),
            Some(McpProbeState::Ok { tool_count, .. }) => Some(Badge::plain(tf("{count} 个工具", "{count} tools", &[("count", tool_count)]))),
            Some(McpProbeState::Failed { .. }) => Some(Badge::warning(t("连接失败", "Connection failed"))),
            None if !resource.available => Some(Badge::warning(t("不可用", "Unavailable"))),
            None => self.row_badge.as_ref().and_then(|f| f(resource)),
        };
        let mut actions: Vec<E> = Vec::new();
        if self.kind == CatalogKind::Mcp
            && resource.available
            && let Some(on_probe) = self.on_probe.clone()
        {
            let label = tf("测试连接 {name}", "Test connection to {name}", &[("name", &resource.name)]);
            let (probes, res) = (self.probes.clone(), resource.clone());
            let running = matches!(probe, Some(McpProbeState::Running));
            actions.push(icon_btn(("probe", self.list_id.as_str(), resource.id.as_str()), "Plug", label).disabled(running).on_click(move |app: &mut App| {
                let id = res.id.clone();
                probes.update(|p| {
                    p.insert(id.clone(), McpProbeState::Running);
                });
                let probes = probes.clone();
                on_probe(
                    app,
                    res.clone(),
                    Box::new(move |_app, result| {
                        let next = probe_state_of(result);
                        probes.update(|p| {
                            p.insert(id, next);
                        });
                    }),
                );
            }));
        }
        if let Some(own) = &self.row_actions {
            actions.extend(own(resource));
        }
        // A built-in entry has no copy of its own to remove, and one the last request carried is still in the model's hands.
        if let Some(on_delete) = self.on_delete.clone().filter(|_| resource.source != ResourceSource::Builtin && !in_play) {
            let res = resource.clone();
            actions.push(confirm_delete_button(
                ("delete", self.list_id.as_str(), resource.id.as_str()),
                false,
                tf("删除 {name}", "Delete {name}", &[("name", &resource.name)]),
                tf("确认删除 {name}", "Confirm deleting {name}", &[("name", &resource.name)]),
                Rc::new(move |app: &mut App| on_delete(app, res.clone())),
            ));
        }
        let mut detail: Vec<String> = Vec::new();
        if resource.available && !resource.description.is_empty() {
            detail.push(resource.description.clone());
        }
        if let Some(line) = self.row_detail.as_ref().and_then(|f| f(resource)) {
            detail.push(line);
        }
        if !resource.location.is_empty() {
            detail.push(resource.location.clone());
        }
        if let Some(tone) = row_tone {
            detail.push(lock_hint(tone).to_owned());
        }
        if let Some(McpProbeState::Ok { detail: d, .. }) = &probe
            && !d.is_empty()
        {
            detail.push(d.clone());
        }
        // A reason nobody can read is no reason: these two are what a row exists to say when it cannot run.
        let note = match &probe {
            Some(McpProbeState::Failed { detail: d }) if !d.is_empty() => Some(d.clone()),
            _ if !resource.available => Some(if !in_play && is_selected { format!("{}\n{}", resource.description, self.unusable_selected) } else { resource.description.clone() }),
            _ => None,
        };
        let toggle = self.toggle.clone();
        let id = resource.id.clone();
        CatalogRow {
            id: Some(resource.id.clone()),
            name: resource.name.clone(),
            detail: Some(detail.join("\n")),
            location: Some(resource.location.clone()).filter(|location| !location.is_empty()),
            note,
            badge,
            actions,
            on: is_selected,
            tone: row_tone,
            sort: Some(sort),
            toggle: Some(Toggle { label: resource.name.clone(), disabled: false, on_toggle: Rc::new(move |app: &mut App| toggle(app, &id, !is_selected)) }),
            ..CatalogRow::default()
        }
        .view()
    }
}

/// A failed probe says why, and then what the server said while it tried: the last few stderr lines are usually the whole diagnosis.
pub fn probe_state_of(result: Result<McpProbeReport, String>) -> McpProbeState {
    match result {
        Err(reason) => McpProbeState::Failed { detail: reason },
        Ok(report) if report.ok => McpProbeState::Ok { tool_count: report.tools.len(), detail: [report.server_name, report.server_version].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ") },
        Ok(report) => {
            let tail = report.logs.len().saturating_sub(5);
            McpProbeState::Failed { detail: report.error.into_iter().chain(report.logs.into_iter().skip(tail)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n") }
        }
    }
}
