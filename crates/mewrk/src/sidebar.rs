//! `Sidebar.tsx` and `WorkspaceOptionsMenu.tsx`: the projects and their conversations.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use mewrk_client::drafts::visible_conversations;
use mewrk_client::model::{RunTarget, Workspace};
use mewrk_client::prelude::*;
use mewrk_client::runs::ConversationStatus;
use mewrk_client::ui_store::{SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH};
use mewrk_client::workspaces::{is_reserved_workspace, is_temporary_workspace, registered_project_workspaces, workspace_location_title};

use crate::chrome::{drag_region, mewrk_mark, BRAND_ORANGE, MARK_HEIGHT, MARK_WIDTH, TILE_RADIUS, TOPBAR_HEIGHT};
use crate::menu::{MenuItem, MenuSection, PopoverMenu};
use crate::state::shell;
use crate::widgets::{confirm_delete_button, tool_button, tool_button_disabled};

/// `--sidebar-row`.
const ROW: f32 = 28.0;
/// `--sidebar-row-button`.
const ROW_BUTTON: f32 = 22.0;
/// How far a press has to move to be a drag rather than a click: `usePointerDrag`'s threshold.
const DRAG_THRESHOLD: f32 = 5.0;
/// How far outside the list a drag still counts as over it.
const LIST_SLOP: f32 = 24.0;
/// How long the click that ends a drag is watched for: `suppressNextClick`'s timeout. A drag let go over something else
/// produces no click, and the next real one must not pay for it.
const CLICK_BLOCK: Duration = Duration::from_millis(350);

/// `clampSidebarWidth`.
pub fn clamp_sidebar_width(width: f64) -> f64 {
    width.round().clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragItem {
    Workspace { workspace_id: String },
    Conversation { workspace_id: String, conversation_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DropTarget {
    Workspace { workspace_id: String, after: bool },
    Conversation { workspace_id: String, conversation_id: String, after: bool },
}

#[derive(Clone, Default)]
struct DragSession {
    item: Option<DragItem>,
    dragging: bool,
    target: Option<DropTarget>,
    /// Swallows the click that ends a drag, up to this moment.
    suppress_click_until: Option<Instant>,
}

impl DragSession {
    fn take_suppressed_click(&mut self) -> bool {
        self.suppress_click_until.take().is_some_and(|until| Instant::now() <= until)
    }
}

/// Whether a row or a project is being dragged: `pointer-sort-active`, which shows `grabbing` wherever the pointer strays.
pub fn sort_drag_active() -> bool {
    use_state("sidebar-drag", DragSession::default).with(|session| session.dragging)
}

/// The sidebar closes by leaving the frame, and what it folded is not to be forgotten for that.
pub fn keep_state_while_closed() {
    let _ = use_state("sidebar-collapsed", HashSet::<String>::new);
}

#[derive(Clone, Debug)]
struct RenameDraft {
    workspace_id: String,
    conversation_id: String,
    original: String,
    value: String,
}

/// `canonicalVerticalTarget`: before the first candidate whose middle is below the pointer, or after the last.
pub fn canonical_vertical_target<'a, T>(candidates: &'a [(T, Rect)], y: f32) -> Option<(&'a T, bool)> {
    if candidates.is_empty() {
        return None;
    }
    for (candidate, rect) in candidates {
        if y < rect.y + rect.h / 2.0 {
            return Some((candidate, false));
        }
    }
    let last = candidates.last().expect("not empty");
    Some((&last.0, true))
}

/// The geometry of the list as last laid out, which is what `getSidebarDropTarget` read off the DOM.
pub struct ListGeometry {
    pub list: Rect,
    pub groups: Vec<GroupGeometry>,
}

pub struct GroupGeometry {
    pub id: String,
    /// The temporary project: pinned to the bottom, so it is no target.
    pub pinned: bool,
    pub rect: Rect,
    pub heading: Rect,
    pub conversations: Vec<(String, Rect)>,
}

/// `getSidebarDropTarget`.
pub fn sidebar_drop_target(point: (f32, f32), item: &DragItem, geometry: &ListGeometry) -> Option<DropTarget> {
    let (x, y) = point;
    let list = geometry.list;
    if x < list.x - LIST_SLOP || x > list.right() + LIST_SLOP || y < list.y - LIST_SLOP || y > list.bottom() + LIST_SLOP {
        return None;
    }
    match item {
        DragItem::Workspace { workspace_id } => {
            // The pinned temporary project is no target: a project dropped below it lands just above it.
            let headings: Vec<(String, Rect)> = geometry.groups.iter().filter(|group| group.id != *workspace_id && !group.pinned).map(|group| (group.id.clone(), group.heading)).collect();
            let (candidate, after) = canonical_vertical_target(&headings, y)?;
            Some(DropTarget::Workspace { workspace_id: candidate.clone(), after })
        }
        DragItem::Conversation { workspace_id, conversation_id } => {
            let source = geometry.groups.iter().find(|group| group.id == *workspace_id)?;
            if y < source.rect.y || y > source.rect.bottom() || y < source.heading.bottom() {
                return None;
            }
            let rows: Vec<(String, Rect)> = source.conversations.iter().filter(|(id, _)| id != conversation_id).cloned().collect();
            let (candidate, after) = canonical_vertical_target(&rows, y)?;
            Some(DropTarget::Conversation { workspace_id: workspace_id.clone(), conversation_id: candidate.clone(), after })
        }
    }
}

fn group_key(workspace_id: &str) -> u64 {
    hash_key(("workspace-group", workspace_id))
}

fn heading_key(workspace_id: &str) -> u64 {
    hash_key(("workspace-heading", workspace_id))
}

fn conversation_key(conversation_id: &str) -> u64 {
    hash_key(("conversation-row", conversation_id))
}

const LIST_KEY: &str = "workspace-list";

fn list_geometry(app: &App) -> Option<ListGeometry> {
    let list = rect_of(LIST_KEY)?;
    let groups = app
        .doc
        .workspaces
        .iter()
        .filter_map(|workspace| {
            let rect = rect_of(group_key(&workspace.id))?;
            let heading = rect_of(heading_key(&workspace.id))?;
            let conversations = workspace.conversations.iter().filter_map(|conversation| rect_of(conversation_key(&conversation.id)).map(|r| (conversation.id.clone(), r))).collect();
            Some(GroupGeometry { id: workspace.id.clone(), pinned: is_temporary_workspace(workspace), rect, heading, conversations })
        })
        .collect();
    Some(ListGeometry { list, groups })
}

/// How tall the status light stands in its row.
const STATUS_MARK_HEIGHT: f32 = 11.0;

/// `ConversationStatusMark`: the brand mark as a status light: an outline while idle, a blinking cursor while working,
/// orange while it waits on the user and blue once a run finished unseen.
pub fn conversation_status_mark(status: ConversationStatus) -> E {
    let t_ = theme();
    let label = match status {
        ConversationStatus::Running => Some(t("正在进行", "In progress")),
        ConversationStatus::Blocked => Some(t("等待你处理", "Waiting for you")),
        ConversationStatus::Completed => Some(t("已完成", "Finished")),
        ConversationStatus::Idle => None,
    };
    let mark = match status {
        ConversationStatus::Idle => mewrk_mark(STATUS_MARK_HEIGHT, t_.text_tertiary, true),
        ConversationStatus::Running => {
            // `conversation-status-blink`: 1.06 s, on for the first half. The mark holds its box while off, so nothing shifts.
            let shown = status_blink_on();
            if shown { mewrk_mark(STATUS_MARK_HEIGHT, t_.text_primary, false) } else { div().size(STATUS_MARK_HEIGHT * MARK_WIDTH / MARK_HEIGHT, STATUS_MARK_HEIGHT).shrink0() }
        }
        ConversationStatus::Blocked => mewrk_mark(STATUS_MARK_HEIGHT, BRAND_ORANGE, false),
        ConversationStatus::Completed => mewrk_mark(STATUS_MARK_HEIGHT, Color::rgb(0x3769BB), false),
    };
    let mut holder: E = row().size(14.0, 12.0).center().shrink0().child(mark);
    if let Some(label) = label {
        holder = holder.role(Role::Image).aria_label(label).tooltip(label);
    }
    holder
}

fn status_blink_on() -> bool {
    const PERIOD: f32 = 1.06;
    let epoch = use_state("status-blink-epoch", std::time::Instant::now);
    let elapsed = epoch.with(|at| at.elapsed().as_secs_f32());
    let phase = elapsed % PERIOD;
    let on = phase < PERIOD / 2.0;
    // Wakes only at the next edge, not every frame.
    let until_edge = if on { PERIOD / 2.0 - phase } else { PERIOD - phase };
    request_wake(until_edge);
    on
}

/// Asks for one more frame after `secs`, from a leaf, since a view has no `Ui` of its own.
fn request_wake(secs: f32) {
    WAKE.with(|w| {
        let mut slot = w.borrow_mut();
        *slot = Some(slot.map_or(secs, |current: f32| current.min(secs)));
    });
}

thread_local! {
    static WAKE: std::cell::RefCell<Option<f32>> = const { std::cell::RefCell::new(None) };
}

/// A leaf that hands on what the marks asked for. One per frame, at the sidebar's root.
fn wake_leaf() -> E {
    custom::<App>(|_, ui: &mut Ui<'_>, _| {
        if let Some(secs) = WAKE.with(|w| w.borrow_mut().take()) {
            ui.wake_after(secs.max(0.016));
        }
    })
    .size(0.0, 0.0)
    .absolute()
    .pass_through()
}

fn workspace_display_name(workspace: &Workspace) -> String {
    if is_temporary_workspace(workspace) { t("临时项目", "Temporary project").to_owned() } else { workspace.name.clone() }
}

/// `WorkspaceOptionsMenu`.
fn workspace_options_menu(workspace: &Workspace, name: &str, disabled: bool, reserved: bool, presets: &[(String, String)]) -> E {
    let t_ = theme();
    let label = tf("{name} 的更多选项", "More options for {name}", &[("name", &name)]);
    let selected_id = presets.iter().find(|(id, _)| *id == workspace.default_conversation_preset_id).map(|(id, _)| id.clone());
    let workspace_id = workspace.id.clone();
    let mut items = Vec::new();
    if !reserved {
        let id = workspace_id.clone();
        items.push(MenuItem::new("edit-project", t("编辑项目…", "Edit project…")).icon("Pencil").on(move |app: &mut App| app.set_project_editor(Some(id.clone()))));
    }
    // Nothing to choose from means nothing to follow; the workspace keeps reusing its most recent settings.
    let mut children = Vec::new();
    {
        let id = workspace_id.clone();
        // No preset of its own: a new task starts from the settings the project last used.
        children.push(MenuItem::new("last-used", t("上一次", "Last used")).description(t("沿用项目上次用的设置", "Reuse the project's last settings")).checked(selected_id.is_none()).on(move |app: &mut App| app.set_workspace_default_preset(&id, "")));
    }
    for (preset_id, preset_name) in presets {
        let (id, preset) = (workspace_id.clone(), preset_id.clone());
        let shown = if preset_name.is_empty() { t("未命名预设", "Untitled preset").to_owned() } else { preset_name.clone() };
        children.push(MenuItem::new(preset_id.clone(), shown).checked(selected_id.as_ref() == Some(preset_id)).on(move |app: &mut App| app.set_workspace_default_preset(&id, &preset)));
    }
    items.push(MenuItem::new("default-preset", t("默认对话预设", "Default conversation preset")).disabled(presets.is_empty()).children(children));
    let key = ("workspace-options", workspace.id.as_str());
    let trigger_label = label.clone();
    PopoverMenu::new(key, label.clone(), move |open| row().center().size(ROW_BUTTON, ROW_BUTTON).radius(6.0).shrink0().hover_bg(t_.subtle_pressed).tooltip(trigger_label).child(icon("MoreHorizontal", 15.0).color(if disabled { t_.text_disabled } else { t_.text_secondary })).reveal_unless(open))
        .menu_label(label)
        .sections(move || vec![MenuSection::new("workspace").items(items)])
        .dense()
        .anchor_to_pointer()
        .disabled(disabled)
        .build()
}

/// One project's heading and its conversations.
#[allow(clippy::too_many_arguments)]
fn workspace_group(app: &mut App, workspace: &Workspace, collapsed: &Local<HashSet<String>>, drag: &Local<DragSession>, rename: &Local<Option<RenameDraft>>, presets: &[(String, String)], active_workspace: Option<&str>, active_conversation: Option<&str>) -> E {
    let t_ = theme();
    let temporary = is_temporary_workspace(workspace);
    let reserved = is_reserved_workspace(workspace);
    let name = workspace_display_name(workspace);
    let id = workspace.id.clone();
    let is_collapsed = collapsed.with(|set| set.contains(&id));
    let unloaded = app.unloaded.clone();
    let listed: Vec<(String, String)> = visible_conversations(&workspace.conversations, &unloaded).iter().map(|conversation| (conversation.id.clone(), conversation.title.clone())).collect();
    let conversation_ids: Vec<String> = workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect();
    let running = conversation_ids.iter().any(|conversation| app.conversation_is_busy(conversation));
    let deleting = app.workspace_is_deleting(&id);
    let delete_blocked = running || deleting;
    // The temporary project is pinned to the bottom of the list.
    let sortable = !deleting && !temporary;

    let (dragged, target) = drag.with(|d| (d.dragging.then(|| d.item.clone()).flatten(), d.target.clone()));
    let being_dragged = matches!(&dragged, Some(DragItem::Workspace { workspace_id }) if *workspace_id == id);

    // The heading.
    let mark: IconRef = if temporary {
        "FolderClock".into()
    } else {
        match &workspace.machine {
            None => "Folder".into(),
            Some(RunTarget::Wsl { .. }) => "SquareTerminal".into(),
            Some(RunTarget::Ssh { .. }) => "Server".into(),
        }
    };
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let title = if temporary { name.clone() } else { registered_project_workspaces(Some(workspace)).iter().map(|entry| workspace_location_title(&entry.path, entry.machine.as_ref(), &ssh_machines)).collect::<Vec<_>>().join("\n") };
    let toggle_id = id.clone();
    let toggle_collapsed = collapsed.clone();
    let swallow = drag.clone();
    let mut toggle: E = row()
        .key(("workspace-toggle", id.as_str()))
        .flex1()
        .min_w(0.0)
        .h(ROW)
        .items_center()
        .gap(7.0)
        .px(7.0)
        .radius(7.0)
        .full_text(title)
        .aria_expanded(!is_collapsed)
        .aria_controls(("workspace-conversations", id.as_str()))
        .when(!temporary, |e| e.aria_keyshortcuts("Alt+ArrowUp Alt+ArrowDown"))
        .child(icon(mark, 14.0).color(t_.text_secondary))
        .child(text(name.clone()).ty(CAPTION_STRONG).color(t_.text_primary).ellipsis().with_style(|s| s.flex_shrink = 1.0))
        .child(icon("ChevronRight", 13.0).color(t_.text_tertiary).ml(-3.0).rotate(if is_collapsed { 0.0 } else { 90.0 }))
        .on_click(move |_: &mut App| {
            if swallow.update(DragSession::take_suppressed_click) {
                return;
            }
            toggle_collapsed.update(|set| {
                if !set.remove(&toggle_id) {
                    set.insert(toggle_id.clone());
                }
            });
        });
    if sortable {
        let source = DragItem::Workspace { workspace_id: id.clone() };
        toggle = toggle.cursor(if being_dragged { CursorKind::Grabbing } else { CursorKind::Grab }).on_pointer(drag_handler(drag, source));
        let move_id = id.clone();
        toggle = toggle.focusable().on_key(move |app: &mut App, press: &KeyPress| move_workspace_by_keyboard(app, &move_id, press));
    }
    let new_task_id = id.clone();
    let new_task_label = tf("在 {name} 新建任务", "Create a task in {name}", &[("name", &name)]);
    let new_task = if deleting { tool_button_disabled(("workspace-new-task", id.as_str()), "Plus", new_task_label, ROW_BUTTON, 14.0).disabled_reason(t("项目正在删除", "Project is being deleted")) } else { tool_button(("workspace-new-task", id.as_str()), "Plus", new_task_label, ROW_BUTTON, 14.0).on_click(move |app: &mut App| app.open_draft_conversation(Some(&new_task_id))) }.reveal_on_parent_hover();
    let mut heading: E = row().key(heading_key(&id)).h(ROW).shrink0().items_center().gap(1.0).radius(7.0).hover_bg(t_.subtle_hover).child(toggle).child(workspace_options_menu(workspace, &name, deleting, reserved, presets)).child(new_task);
    if !reserved {
        let delete_id = id.clone();
        let title = if deleting {
            Some(t("项目正在删除", "Project is being deleted").to_owned())
        } else if running {
            Some(t("当前操作结束后才能删除项目", "Wait for the current operation to finish before deleting the project").to_owned())
        } else {
            None
        };
        // An armed delete must not vanish from under the pointer that armed it.
        let armed = use_state(("confirm-delete", hash_key(("workspace-delete", id.as_str()))), || false).get();
        let delete_button =
            confirm_delete_button(
                ("workspace-delete", id.as_str()),
                tf("删除项目 {name}", "Delete project {name}", &[("name", &name)]),
                tf("确认永久删除项目 {name} 及其所有任务（目录里的文件不受影响）", "Confirm deleting project {name} and all its tasks for good (files in its directories stay)", &[("name", &name)]),
                title,
                delete_blocked,
                13.0,
                ROW_BUTTON,
                move |app: &mut App| app.delete_workspace(&delete_id),
            );
        heading = heading.child(if armed { delete_button } else { delete_button.reveal_on_parent_hover() }.mr(3.0));
    } else {
        heading = heading.pr(3.0);
    }

    // Opens and closes at once: a project is a list to scan, not a panel to reveal.
    let mut list: E = col().key(("workspace-conversations", id.as_str())).role(Role::List).aria_hidden(is_collapsed);
    if !is_collapsed {
        for (conversation_id, conversation_title) in &listed {
            let row_el = conversation_row(app, &id, conversation_id, conversation_title, deleting, rename, drag, active_workspace == Some(id.as_str()) && active_conversation == Some(conversation_id.as_str()), dragged.as_ref(), target.as_ref());
            list = list.child(row_el);
        }
        if listed.is_empty() {
            list = list.child(row().h(ROW).pl(28.0).items_center().child(text(t("还没有任务", "No tasks yet")).ty(FINE).color(t_.text_tertiary)));
        }
    }

    let mut group: E = col().key(group_key(&id)).shrink0().radius(8.0).child(heading).child(list);
    if being_dragged {
        group = group.opacity(0.38);
    }
    if let Some(DropTarget::Workspace { workspace_id, after }) = &target
        && *workspace_id == id
    {
        group = group.child(drop_bar(*after));
    }
    group
}

/// `drop-target--before` and `--after`.
fn drop_bar(after: bool) -> E {
    let t_ = theme();
    let bar: E = div().absolute().left(4.0).right(4.0).h(2.0).radius(1.0).bg(t_.accent).pass_through();
    if after { bar.bottom(0.0) } else { bar.top(0.0) }
}

fn drag_handler(drag: &Local<DragSession>, source: DragItem) -> impl Fn(&mut App, PointerEvent) + 'static {
    let drag = drag.clone();
    move |app: &mut App, e: PointerEvent| match e.phase {
        Phase::Down => drag.set(DragSession { item: Some(source.clone()), ..DragSession::default() }),
        Phase::Move => {
            let far = (e.dx * e.dx + e.dy * e.dy).sqrt() >= DRAG_THRESHOLD;
            if drag.with(|d| d.item.is_none() || (!d.dragging && !far)) {
                return;
            }
            let target = list_geometry(app).and_then(|geometry| sidebar_drop_target((e.x, e.y), &source, &geometry));
            drag.update(|d| {
                d.dragging = true;
                d.target = target;
            });
            repaint();
        }
        Phase::Up => {
            let target = list_geometry(app).and_then(|geometry| sidebar_drop_target((e.x, e.y), &source, &geometry));
            let moved = (e.dx * e.dx + e.dy * e.dy).sqrt() >= DRAG_THRESHOLD;
            let was_dragging = drag.update(|d| {
                let was = d.dragging || (d.item.is_some() && moved);
                // A drag Escape already ended keeps the watch that started.
                let suppress_click_until = if was { Some(Instant::now() + CLICK_BLOCK) } else { d.suppress_click_until };
                *d = DragSession { suppress_click_until, ..DragSession::default() };
                was
            });
            if was_dragging && let Some(target) = target {
                apply_drop(app, &source, &target);
            }
            repaint();
        }
    }
}

fn apply_drop(app: &mut App, item: &DragItem, target: &DropTarget) {
    match (item, target) {
        (DragItem::Workspace { workspace_id }, DropTarget::Workspace { workspace_id: target_id, after }) if workspace_id != target_id => {
            app.reorder_workspace(workspace_id, target_id, *after);
            announce(t("项目顺序已更新", "Project order updated"));
        }
        (DragItem::Conversation { workspace_id, conversation_id }, DropTarget::Conversation { workspace_id: target_workspace, conversation_id: target_id, after }) if workspace_id == target_workspace && conversation_id != target_id => {
            app.reorder_sidebar_conversation(workspace_id, conversation_id, target_id, *after);
            announce(t("对话顺序已更新", "Conversation order updated"));
        }
        _ => {}
    }
}

/// `moveWorkspaceByKeyboard`.
fn move_workspace_by_keyboard(app: &mut App, workspace_id: &str, press: &KeyPress) -> bool {
    if !press.alt || !matches!(press.key, Key::Up | Key::Down) {
        return false;
    }
    let up = press.key == Key::Up;
    let workspaces = &app.doc.workspaces;
    let Some(index) = workspaces.iter().position(|workspace| workspace.id == workspace_id) else { return true };
    let next = if up { index.checked_sub(1) } else { Some(index + 1) };
    let Some(target) = next.and_then(|i| workspaces.get(i)) else { return true };
    if is_temporary_workspace(&workspaces[index]) || is_temporary_workspace(target) {
        return true;
    }
    let target_id = target.id.clone();
    app.reorder_workspace(workspace_id, &target_id, !up);
    announce(if up { t("项目已向上移动", "Project moved up") } else { t("项目已向下移动", "Project moved down") });
    true
}

/// `moveConversationByKeyboard`.
fn move_conversation_by_keyboard(app: &mut App, workspace_id: &str, conversation_id: &str, press: &KeyPress) -> bool {
    if !press.alt || !matches!(press.key, Key::Up | Key::Down) {
        return false;
    }
    let up = press.key == Key::Up;
    let Some(workspace) = app.workspace(workspace_id) else { return true };
    let Some(index) = workspace.conversations.iter().position(|conversation| conversation.id == conversation_id) else { return true };
    let next = if up { index.checked_sub(1) } else { Some(index + 1) };
    let Some(target) = next.and_then(|i| workspace.conversations.get(i)) else { return true };
    let target_id = target.id.clone();
    app.reorder_sidebar_conversation(workspace_id, conversation_id, &target_id, !up);
    announce(if up { t("对话已向上移动", "Conversation moved up") } else { t("对话已向下移动", "Conversation moved down") });
    true
}

#[allow(clippy::too_many_arguments)]
fn conversation_row(app: &mut App, workspace_id: &str, conversation_id: &str, title: &str, locked: bool, rename: &Local<Option<RenameDraft>>, drag: &Local<DragSession>, is_active: bool, dragged: Option<&DragItem>, target: Option<&DropTarget>) -> E {
    let t_ = theme();
    let renaming = rename.with(|draft| draft.as_ref().is_some_and(|d| d.workspace_id == workspace_id && d.conversation_id == conversation_id));
    let running = locked || app.conversation_is_busy(conversation_id);
    // Reordering touches only the project's order, never the conversation, so a running one moves like any other.
    let sortable = !locked;
    let status = app.sidebar_conversation_status(conversation_id);
    let (workspace_id_owned, conversation_id_owned) = (workspace_id.to_owned(), conversation_id.to_owned());
    let being_dragged = matches!(dragged, Some(DragItem::Conversation { conversation_id: dragged_id, .. }) if dragged_id == conversation_id);

    let mut row_el: E = row().key(conversation_key(conversation_id)).role(Role::ListItem).h(ROW).my(1.0).shrink0().items_center().radius(7.0).hover_bg(t_.subtle_hover);
    if is_active {
        row_el = row_el.bg(t_.subtle_pressed);
    }
    if being_dragged {
        row_el = row_el.opacity(0.38);
    }
    row_el = row_el.child(conversation_status_mark(status).absolute().left(7.0).top(8.0).pass_through());

    if renaming {
        let field_key = ("conversation-rename", conversation_id);
        let raw_focus = hash_key(("field", field_key));
        let value = rename.with(|draft| draft.as_ref().map_or_else(String::new, |d| d.value.clone()));
        let (change, submit, keys, blur) = (rename.clone(), rename.clone(), rename.clone(), rename.clone());
        let input = field::<App>(field_key, value)
            .ty(CAPTION)
            .select_on_focus(true)
            .on_change(move |_, value| change.update(|draft| {
                if let Some(d) = draft {
                    d.value = value;
                }
            }))
            .on_submit(move |app, _| finish_rename(app, &submit))
            .on_key(move |_, press| {
                if press.key == Key::Escape {
                    keys.set(None);
                    return true;
                }
                false
            })
            .on_blur(move |app, _| finish_rename(app, &blur))
            .build()
            .aria_label(tf("重命名 {title}", "Rename {title}", &[("title", &title)]))
            .flex1()
            .h(22.0);
        let _ = raw_focus;
        row_el = row_el.pl(25.0).child(input);
    } else {
        let (select_workspace, select_conversation, swallow) = (workspace_id_owned.clone(), conversation_id_owned.clone(), drag.clone());
        let mut main: E = row().key(("conversation-main", conversation_id)).aria_keyshortcuts("Alt+ArrowUp Alt+ArrowDown").flex1().min_w(0.0).h(ROW).pl(28.0).pr(7.0).items_center().child(text(title.to_owned()).ty(CAPTION).color(t_.text_primary).ellipsis().flex1()).on_click(move |app: &mut App| {
            if swallow.update(DragSession::take_suppressed_click) {
                return;
            }
            app.select_conversation(&select_workspace, &select_conversation);
        });
        if sortable {
            let source = DragItem::Conversation { workspace_id: workspace_id_owned.clone(), conversation_id: conversation_id_owned.clone() };
            main = main.cursor(if being_dragged { CursorKind::Grabbing } else { CursorKind::Grab }).on_pointer(drag_handler(drag, source));
            let (key_workspace, key_conversation) = (workspace_id_owned.clone(), conversation_id_owned.clone());
            main = main.focusable().on_key(move |app: &mut App, press: &KeyPress| move_conversation_by_keyboard(app, &key_workspace, &key_conversation, press));
        }
        row_el = row_el.child(main);
    }

    // The actions.
    let rename_label = tf("重命名 {title}", "Rename {title}", &[("title", &title)]);
    let rename_button = if renaming || locked {
        tool_button_disabled(("conversation-rename-button", conversation_id), "Pencil", rename_label, ROW_BUTTON, 13.0)
    } else {
        let (start, starting_workspace, starting_conversation, starting_title) = (rename.clone(), workspace_id_owned.clone(), conversation_id_owned.clone(), title.to_owned());
        tool_button(("conversation-rename-button", conversation_id), "Pencil", rename_label, ROW_BUTTON, 13.0).on_click(move |_: &mut App| {
            start.set(Some(RenameDraft { workspace_id: starting_workspace.clone(), conversation_id: starting_conversation.clone(), original: starting_title.clone(), value: starting_title.clone() }));
            focus_request(("field", ("conversation-rename", starting_conversation.as_str())));
        })
    };
    let (delete_workspace, delete_conversation) = (workspace_id_owned.clone(), conversation_id_owned.clone());
    let delete_button = confirm_delete_button(
        ("conversation-delete", conversation_id),
        tf("删除 {title}", "Delete {title}", &[("title", &title)]),
        tf("确认删除 {title}", "Confirm deleting {title}", &[("title", &title)]),
        running.then(|| t("当前操作结束后才能删除", "Wait for the current operation to finish before deleting").to_owned()),
        running,
        13.0,
        ROW_BUTTON,
        move |app: &mut App| app.delete_conversation(&delete_workspace, &delete_conversation),
    );
    let actions: E = row().items_center().gap(1.0).mr(3.0).shrink0().child(rename_button).child(delete_button);
    // A row being renamed keeps them lit; the rest show on hover, and a row's armed delete must not vanish from under it.
    let armed = use_state(("confirm-delete", hash_key(("conversation-delete", conversation_id))), || false).get();
    row_el = row_el.child(if renaming || armed { actions } else { actions.reveal_on_parent_hover() });

    if let Some(DropTarget::Conversation { workspace_id: target_workspace, conversation_id: target_id, after }) = target
        && target_workspace == workspace_id
        && target_id == conversation_id
    {
        row_el = row_el.child(drop_bar(*after));
    }
    row_el
}

/// `finishRename`.
fn finish_rename(app: &mut App, rename: &Local<Option<RenameDraft>>) {
    let Some(draft) = rename.update(Option::take) else { return };
    let title = draft.value.trim();
    if !title.is_empty() && title != draft.original {
        app.rename_conversation(&draft.workspace_id, &draft.conversation_id, title);
    }
}

/// The resize handle: drag, arrow keys, and a double click for the default.
fn resize_handle(app: &mut App) -> E {
    let t_ = theme();
    let width = app.sidebar_width();
    let session = use_state("sidebar-resize-start", || None::<f64>);
    let resizing = shell(app).sidebar_resizing;
    let press = session.clone();
    let mut handle: E = div()
        .key("sidebar-resize-handle")
        .role(Role::Splitter)
        .aria_label(t("调整侧栏宽度", "Resize sidebar"))
        .aria_orientation(Orientation::Vertical)
        .aria_value(width, SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
        .absolute()
        .top(0.0)
        .right(0.0)
        .w(8.0)
        .h_full()
        .cursor(CursorKind::ColResize)
        .tooltip(t("拖动调整侧栏宽度；双击恢复默认宽度", "Drag to resize the sidebar; double-click to restore the default width"))
        .focusable()
        .on_pointer(move |app: &mut App, e| match e.phase {
            Phase::Down => {
                let start = app.sidebar_width();
                press.set(Some(start));
                shell(app).sidebar_resizing = true;
            }
            Phase::Move => {
                if let Some(start) = press.get() {
                    app.update_sidebar_width(clamp_sidebar_width(start + f64::from(e.dx)));
                }
            }
            Phase::Up => {
                press.set(None);
                shell(app).sidebar_resizing = false;
                repaint();
            }
        })
        .on_double_click(|app: &mut App| {
            app.update_sidebar_width(SIDEBAR_DEFAULT_WIDTH);
        })
        .on_key(move |app: &mut App, press: &KeyPress| {
            let step = if press.shift { 24.0 } else { 12.0 };
            let next = match press.key {
                Key::Left => width - step,
                Key::Right => width + step,
                Key::Home => SIDEBAR_MIN_WIDTH,
                Key::End => SIDEBAR_MAX_WIDTH,
                _ => return false,
            };
            app.update_sidebar_width(clamp_sidebar_width(next));
            true
        });
    if a11y_active() {
        handle = handle.aria_value_text(tf("{width} 像素", "{width} pixels", &[("width", &width)]));
    }
    if resizing {
        handle = handle.child(div().absolute().top(0.0).right(0.0).w(2.0).h_full().bg(t_.accent.with_alpha(0.72)).pass_through());
        // Escape puts the width back.
        let cancel = session.clone();
        handle = handle.on_key_global(move |app: &mut App, press| {
            if press.key != Key::Escape {
                return false;
            }
            if let Some(start) = cancel.update(Option::take) {
                app.update_sidebar_width(start);
                shell(app).sidebar_resizing = false;
            }
            true
        });
    }
    handle
}

/// The sidebar. Its first row holds room for the traffic lights and `ShellNav`, which float over it, and moves the window.
pub fn sidebar(app: &mut App, width: f32) -> E {
    let t_ = theme();
    let collapsed = use_state("sidebar-collapsed", HashSet::<String>::new);
    let rename = use_state("sidebar-rename", || None::<RenameDraft>);
    let drag = use_state("sidebar-drag", DragSession::default);
    let presets: Vec<(String, String)> = workspace_preset_options(app);
    let active_workspace = app.active_workspace_id();
    let active_conversation = app.active_conversation_id();
    let active_deleting = active_workspace.as_ref().is_some_and(|id| app.workspace_is_deleting(id));

    let new_task_fg = if active_deleting { t_.text_disabled } else { t_.text_primary };
    let mut new_task: E = row().key("new-task-button").w_full().h(ROW).shrink0().items_center().gap(7.0).px(7.0).radius(7.0).child(icon("Plus", 15.0).color(new_task_fg)).child(text(t("新建任务", "New task")).ty(CAPTION_STRONG).color(new_task_fg).ellipsis().flex1());
    if active_deleting {
        new_task = new_task.disabled(true).disabled_reason(t("项目正在删除", "Project is being deleted"));
    } else {
        new_task = new_task.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(|app: &mut App| app.open_draft_conversation(None));
    }

    let heading = row().h(ROW).mt(14.0).pl(7.0).pr(3.0).shrink0().items_center().child(text(t("项目", "Projects")).ty(ty_section()).color(t_.text_tertiary).flex1()).child(tool_button("new-project", "FolderPlus", t("新建项目", "New project"), ROW_BUTTON, 15.0).on_click(|app: &mut App| app.set_workspace_dialog_open(true, false)));

    let workspaces = app.doc.workspaces.clone();
    let mut list: E = col().key(LIST_KEY).role(Role::Navigation).aria_label(t("对话列表", "Conversation list")).flex1().min_h(0.0).scroll_y().pb(12.0).gap(6.0);
    for workspace in &workspaces {
        list = list.child(workspace_group(app, workspace, &collapsed, &drag, &rename, &presets, active_workspace.as_deref(), active_conversation.as_deref()));
    }

    let settings: E = row()
        .key("sidebar-settings")
        .flex1()
        .min_w(0.0)
        .h(ROW)
        .items_center()
        .gap(7.0)
        .px(7.0)
        .radius(7.0)
        .hover_bg(t_.subtle_hover)
        .press_bg(t_.subtle_pressed)
        .child(icon("Settings", 14.0).color(t_.text_secondary))
        .child(text(t("设置", "Settings")).ty(CAPTION).ellipsis().flex1())
        .on_click(|app: &mut App| crate::actions::open_global_settings(app, "providers"));
    let footer = row().min_h(42.0).shrink0().pt(7.0).items_center().border_top(t_.divider).child(settings);

    let handle = resize_handle(app);
    let mut sidebar: E = col()
        .key("sidebar")
        .role(Role::Complementary)
        .aria_label(t("项目和对话", "Projects and conversations"))
        .w(width)
        .h_full()
        .shrink0()
        .px(9.0)
        .pb(8.0)
        .bg(sidebar_ground())
        // Glass has no line along this edge: the strip beside the sidebar meets it without a seam.
        .when(!crate::glass::layers().glass, |sidebar| sidebar.border_right(t_.divider))
        .radii([0.0, TILE_RADIUS, TILE_RADIUS, 0.0])
        // The window's top row: room for the lights and `ShellNav`, and it moves the window.
        .child(drag_region(div().key("sidebar-chrome-row").h(TOPBAR_HEIGHT).shrink0().mx(-9.0)))
        .child(new_task)
        .child(heading)
        .child(list)
        .child(footer)
        .child(handle)
        .child(wake_leaf());
    if drag.with(|session| session.dragging) {
        // `cancelWithEscape`: nothing moves. The click the release may still produce waits out its watch.
        sidebar = sidebar.on_key_global(move |_: &mut App, press| {
            if press.key != Key::Escape {
                return false;
            }
            drag.set(DragSession { suppress_click_until: Some(Instant::now() + CLICK_BLOCK), ..DragSession::default() });
            true
        });
    }
    sidebar
}

fn ty_section() -> fluent::TypeStyle {
    fluent::TypeStyle { size: 10.0, line: 13.0, weight: fluent::Weight::Semibold, mono: false, italic: false, serif: false }
}

/// `--sidebar`: a step off the window's ground, in whichever theme.
pub fn sidebar_ground() -> Color {
    let t_ = theme();
    if crate::glass::layers().glass {
        return crate::glass::glass_tile();
    }
    if t_.dark { Color::rgb(0x1B1B1B) } else { Color::rgb(0xEFEFEB) }
}

/// `workspacePresetOptions`: the presets a project can default to, in display order.
fn workspace_preset_options(app: &App) -> Vec<(String, String)> {
    app.doc.presets.conversation_presets.iter().map(|preset| (preset.id.clone(), preset.name.clone())).collect()
}
