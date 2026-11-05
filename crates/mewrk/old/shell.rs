//! The window's chrome: the sidebar, the top bar, and the overlays that sit above everything.

use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, Key, MenuItem, Rect, Ui};
use fluent::TextEdit;
use mewrk_core::*;

use crate::app::{App, Confirm, Page, TOPBAR_H};
use crate::layout::{PaneId, TasksTab};

const ROW: f32 = 28.0;
const ROW_BUTTON: f32 = 22.0;
/// The first traffic light's left edge is 16 in and three of them end at 76.
const LIGHTS_END: f32 = 84.0;
const NAV_BUTTON: f32 = 28.0;
const NAV_GAP: f32 = 2.0;

fn hash(tag: u64, s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ tag;
    for b in s.as_bytes() {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn small_icon_button(ui: &mut Ui<'_>, id: u64, r: Rect, icon: Icon, size: f32, tip: &str, enabled: bool) -> bool {
    let t = ui.theme;
    let resp = ui.interact(id, r);
    if !enabled {
        ui.icon(icon, r.x + r.w * 0.5, r.y + r.h * 0.5, size, t.text_disabled);
        return false;
    }
    let bg = t.subtle_hover.fade(resp.hover_t);
    if bg.a > 0 {
        ui.fill(r, 6.0, bg);
    }
    ui.icon(icon, r.x + r.w * 0.5, r.y + r.h * 0.5, size, if resp.hovered { t.text_primary } else { t.text_secondary });
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
        if !tip.is_empty() {
            ui.tooltip(r, tip.to_owned());
        }
    }
    resp.clicked
}

/// `ShellNav`: over the sidebar's first row, past the traffic lights. With the sidebar closed it shrinks to its four buttons.
pub fn nav(app: &mut App, ui: &mut Ui<'_>, sidebar_w: f32) {
    let inset = if app.fullscreen { 10.0 } else { LIGHTS_END };
    let y = (TOPBAR_H - NAV_BUTTON) * 0.5;
    let mut x = inset;
    let b = |x: f32| rect(x, y, NAV_BUTTON, NAV_BUTTON);
    if small_icon_button(ui, 0xA001, b(x), Icon::SidebarToggle, 15.0, "显示或隐藏侧栏  ⌘B", true) {
        app.doc.settings.sidebar_open = !app.doc.settings.sidebar_open;
        app.mark_dirty();
    }
    x += NAV_BUTTON + NAV_GAP;
    if small_icon_button(ui, 0xA002, b(x), Icon::ArrowLeft, 14.0, "后退", app.can_go_back()) {
        app.go_back();
    }
    x += NAV_BUTTON + NAV_GAP;
    if small_icon_button(ui, 0xA003, b(x), Icon::ArrowRight, 14.0, "前进", app.can_go_forward()) {
        app.go_forward();
    }
    x += NAV_BUTTON + NAV_GAP;
    let search_x = if sidebar_w > 0.0 { sidebar_w - 12.0 - NAV_BUTTON } else { x };
    if small_icon_button(ui, 0xA004, b(search_x), Icon::Search, 14.0, "搜索对话  ⌘K", true) && app.search.is_none() {
        app.search = Some(crate::app::Search { edit: TextEdit::new(), current: 0 });
    }
}

pub fn sidebar(app: &mut App, ui: &mut Ui<'_>, r: Rect) {
    let t = ui.theme;
    // The new-task row and the section heading under the window's top row.
    let pad = 9.0;
    let inner_x = r.x + pad;
    let inner_w = r.w - pad * 2.0;
    let mut y = TOPBAR_H;

    let row = rect(inner_x, y, inner_w, ROW);
    let resp = ui.interact(0xA010, row);
    if resp.hover_t > 0.0 {
        ui.fill(row, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
    }
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    ui.icon(Icon::Plus, row.x + 14.0, row.y + ROW * 0.5, 13.0, t.text_primary);
    ui.label_vcentered(row.x + 28.0, row, "新任务", &BODY_SMALL_STRONG, t.text_primary);
    ui.label_right(row.right() - 8.0, row.y + (ROW - FINE.line) * 0.5, "⌘N", &FINE, t.text_tertiary);
    if resp.clicked {
        app.new_task(None);
    }
    y += ROW + 14.0;

    let heading = rect(inner_x, y, inner_w, ROW);
    ui.label_vcentered(heading.x + 7.0, heading, "项目", &FINE_STRONG, t.text_tertiary);
    if small_icon_button(ui, 0xA011, rect(heading.right() - ROW_BUTTON - 3.0, heading.y + 3.0, ROW_BUTTON, ROW_BUTTON), Icon::FolderPlus, 13.0, "添加项目", true) {
        app.requests.pick_project = true;
    }
    y += ROW;

    // The list scrolls under its heading and over the footer.
    let footer_h = 42.0;
    let list = rect(r.x, y, r.w, (r.h - y - footer_h).max(0.0));
    let content = sidebar_content_height(app);
    app.sidebar_scroll.update(ui, 0xA012, list, content);
    let offset = app.sidebar_scroll.offset;
    let saved = ui.clip;
    ui.clip = saved.intersect(&list);
    let mut ry = list.y - offset;

    let projects: Vec<Project> = app.doc.projects.clone();
    let active = app.active.clone();
    let mut select: Option<String> = None;
    let mut new_in: Option<String> = None;
    let mut toggle: Option<String> = None;
    let mut ask_remove: Option<String> = None;
    let mut delete: Option<String> = None;
    let mut commit_rename: Option<(String, String)> = None;
    for (pi, p) in projects.iter().enumerate() {
        if pi > 0 {
            ry += 6.0;
        }
        let head = rect(inner_x, ry, inner_w, ROW);
        if head.bottom() >= list.y && head.y <= list.bottom() {
            let pid = hash(0xA100, &p.id);
            let hovered = ui.pointer_in(&head);
            let resp = ui.interact(pid, head);
            if resp.hover_t > 0.0 {
                ui.fill(head, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
            }
            let is_temp = p.path.is_none();
            ui.icon(if is_temp { Icon::Clock } else { Icon::Folder }, head.x + 14.0, head.y + ROW * 0.5, 13.0, t.text_secondary);
            let lw = ui.measure(&p.name, &CAPTION_STRONG).min(inner_w - 100.0);
            ui.label_ellipsis(head.x + 28.0, head.y + (ROW - CAPTION_STRONG.line) * 0.5, inner_w - 100.0, &p.name, &CAPTION_STRONG, t.text_secondary);
            ui.icon(if p.collapsed { Icon::ChevronRight } else { Icon::ChevronDown }, head.x + 28.0 + lw + 9.0, head.y + ROW * 0.5, 8.0, t.text_tertiary);
            if resp.hovered {
                ui.cursor = CursorKind::Hand;
            }
            if resp.clicked {
                toggle = Some(p.id.clone());
            }
            let open_menu = ui.mem.open_popup == pid + 1;
            if hovered || open_menu {
                let more = rect(head.right() - ROW_BUTTON - 3.0, head.y + 3.0, ROW_BUTTON, ROW_BUTTON);
                let plus = rect(more.x - ROW_BUTTON - 1.0, head.y + 3.0, ROW_BUTTON, ROW_BUTTON);
                if small_icon_button(ui, pid + 2, plus, Icon::Plus, 12.0, "在这个项目里新建任务", true) {
                    new_in = Some(p.id.clone());
                }
                if !is_temp {
                    if small_icon_button(ui, pid + 3, more, Icon::MoreH, 12.0, "", true) {
                        ui.toggle_popup(pid + 1);
                    }
                    let path = p.path.as_ref().map(|x| x.display().to_string()).unwrap_or_default();
                    let items = [MenuItem::new(&path).enabled(false), MenuItem::new("移除项目").icon(Icon::Trash).danger().separated()];
                    if ui.menu(pid + 1, more, true, 280.0, &items) == Some(1) {
                        ask_remove = Some(p.id.clone());
                    }
                }
            }
        }
        ry += ROW;
        if p.collapsed {
            continue;
        }
        let mut any = false;
        let mut convs: Vec<&ConvMeta> = app.doc.conversations.iter().filter(|c| c.project_id == p.id).collect();
        convs.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        for c in convs {
            any = true;
            let row = rect(inner_x, ry + 1.0, inner_w, ROW);
            if row.bottom() >= list.y && row.y <= list.bottom() {
                let selected = active.as_deref() == Some(c.id.as_str());
                let rid = hash(0xA200, &c.id);
                if let Some((rename_id, edit)) = &mut app.rename
                    && *rename_id == c.id
                {
                    let done = edit.show(ui, rid + 5, rect(row.x + 24.0, row.y + 1.0, row.w - 26.0, ROW - 2.0), "", &CAPTION, true);
                    let enter = done.submitted;
                    let blur = ui.mem.focus != rid + 5 && ui.input.pressed;
                    if enter || blur {
                        commit_rename = Some((c.id.clone(), edit.text.trim().to_owned()));
                    }
                } else {
                    let hovered = ui.pointer_in(&row);
                    let resp = ui.interact(rid, row);
                    let fill = if selected { t.subtle_hover.mix(t.subtle_pressed, 0.0) } else { t.subtle_hover.fade(resp.hover_t) };
                    if fill.a > 0 {
                        ui.fill(row, CONTROL_RADIUS, fill);
                    }
                    if selected {
                        // NavigationView's selection pill: the Fluent mark on Mewrk's row.
                        ui.fill(rect(row.x, row.y + 7.0, 3.0, ROW - 14.0), 1.5, t.accent);
                    }
                    if resp.hovered {
                        ui.cursor = CursorKind::Hand;
                    }
                    if resp.clicked {
                        select = Some(c.id.clone());
                    }
                    status_mark(app, ui, &c.id, row.x + 14.0, row.y + ROW * 0.5);
                    let actions_w = if hovered { ROW_BUTTON * 2.0 + 6.0 } else { 0.0 };
                    ui.label_ellipsis(row.x + 28.0, row.y + (ROW - CAPTION.line) * 0.5, row.w - 34.0 - actions_w, &c.title, &CAPTION, t.text_primary);
                    if hovered {
                        let del = rect(row.right() - ROW_BUTTON - 3.0, row.y + 3.0, ROW_BUTTON, ROW_BUTTON);
                        let ren = rect(del.x - ROW_BUTTON - 1.0, row.y + 3.0, ROW_BUTTON, ROW_BUTTON);
                        if small_icon_button(ui, rid + 1, ren, Icon::Pencil, 11.0, "重命名", true) {
                            let mut edit = TextEdit::new();
                            edit.set_text(c.title.clone());
                            edit.select_all();
                            ui.mem.focus = rid + 5;
                            app.rename = Some((c.id.clone(), edit));
                        }
                        let armed = app.armed_delete.as_deref() == Some(c.id.as_str());
                        if armed {
                            ui.fill(del, 6.0, t.critical_bg);
                        }
                        if small_icon_button(ui, rid + 2, del, Icon::Trash, 11.0, if armed { "再点一次确认删除" } else { "删除" }, true) {
                            if armed {
                                delete = Some(c.id.clone());
                                app.armed_delete = None;
                            } else {
                                app.armed_delete = Some(c.id.clone());
                            }
                        }
                    } else if app.armed_delete.as_deref() == Some(c.id.as_str()) {
                        app.armed_delete = None;
                    }
                }
            }
            ry += ROW + 2.0;
        }
        if !any {
            ui.label(inner_x + 28.0, ry + (ROW - FINE.line) * 0.5, "还没有任务", &FINE, t.text_tertiary);
            ry += ROW;
        }
    }
    ui.clip = saved;

    if let Some(id) = toggle
        && let Some(p) = app.doc.projects.iter_mut().find(|p| p.id == id)
    {
        p.collapsed = !p.collapsed;
        app.mark_dirty();
    }
    if let Some(id) = select {
        app.select_conversation(&id);
    }
    if let Some(id) = new_in {
        app.new_task(Some(id));
    }
    if let Some(id) = ask_remove {
        app.confirm = Some(Confirm::RemoveProject(id));
    }
    if let Some(id) = delete {
        app.delete_conversation(&id);
    }
    if let Some((id, title)) = commit_rename {
        if !title.is_empty()
            && let Some(m) = app.meta_mut(&id)
        {
            m.title = title;
        }
        app.rename = None;
        app.mark_dirty();
    }

    // The footer: settings is a row like the others, its gear in the lead column.
    let fy = r.bottom() - footer_h;
    ui.fill(rect(inner_x, fy, inner_w, 1.0), 0.0, t.divider);
    let row = rect(inner_x, fy + 7.0, inner_w, ROW);
    let resp = ui.interact(0xA013, row);
    let selected = app.page == Page::Settings;
    let fill = if selected { t.subtle_hover } else { t.subtle_hover.fade(resp.hover_t) };
    if fill.a > 0 {
        ui.fill(row, CONTROL_RADIUS, fill);
    }
    if selected {
        ui.fill(rect(row.x, row.y + 7.0, 3.0, ROW - 14.0), 1.5, t.accent);
    }
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    ui.icon(Icon::Settings, row.x + 14.0, row.y + ROW * 0.5, 14.0, t.text_secondary);
    ui.label_vcentered(row.x + 28.0, row, "设置", &BODY_SMALL, t.text_primary);
    ui.label_right(row.right() - 8.0, row.y + (ROW - FINE.line) * 0.5, "⌘,", &FINE, t.text_tertiary);
    if resp.clicked {
        app.page = if selected { Page::Chat } else { Page::Settings };
    }

    // The right edge resizes the sidebar.
    let edge = rect(r.right() - 4.0, r.y, 8.0, r.h);
    let eresp = ui.interact(0xA014, edge);
    if eresp.hovered || eresp.pressed {
        ui.cursor = CursorKind::ColResize;
    }
    if eresp.pressed {
        if let Some((px, _)) = ui.input.pointer {
            app.doc.settings.sidebar_width = px.clamp(200.0, 480.0);
            app.sidebar_resizing = true;
        }
    } else if app.sidebar_resizing {
        app.sidebar_resizing = false;
        app.mark_dirty();
    }
}

fn sidebar_content_height(app: &App) -> f32 {
    let mut h = 12.0;
    for (i, p) in app.doc.projects.iter().enumerate() {
        if i > 0 {
            h += 6.0;
        }
        h += ROW;
        if p.collapsed {
            continue;
        }
        let n = app.doc.conversations.iter().filter(|c| c.project_id == p.id).count();
        h += if n == 0 { ROW } else { n as f32 * (ROW + 2.0) };
    }
    h
}

/// The brand's eared cursor as a task's state: hollow idle, blinking while working, amber when it waits on the user, blue when a run finished out of sight.
fn status_mark(app: &App, ui: &mut Ui<'_>, id: &str, x: f32, y: f32) {
    let t = ui.theme;
    let Some(live) = app.live.get(id) else {
        ui.icon(Icon::Mark, x, y, 12.0, t.text_tertiary);
        return;
    };
    if live.is_blocked() {
        ui.icon(Icon::MarkFilled, x, y, 12.0, t.caution);
    } else if live.is_running() {
        // Blink on a schedule rather than animating every frame: a running task in a list should not keep the GPU awake.
        let phase = (ui.time / 0.53) as u32;
        if phase.is_multiple_of(2) {
            ui.icon(Icon::MarkFilled, x, y, 12.0, t.text_primary);
        }
        ui.wake_after(0.53 - ui.time % 0.53);
    } else if live.unseen_done {
        ui.icon(Icon::MarkFilled, x, y, 12.0, t.accent);
    } else {
        ui.icon(Icon::Mark, x, y, 12.0, t.text_tertiary);
    }
}

/// The window's top row, beside the sidebar: the title, then the pane buttons.
pub fn topbar(app: &mut App, ui: &mut Ui<'_>, sidebar_w: f32, w: f32) {
    let t = ui.theme;
    let closed_nav = if app.fullscreen { 10.0 } else { LIGHTS_END } + 4.0 * NAV_BUTTON + 3.0 * NAV_GAP + 14.0;
    let left = if sidebar_w > 0.0 { sidebar_w + 16.0 } else { closed_nav };
    let bar = rect(0.0, 0.0, w, TOPBAR_H);
    let Some(id) = app.active.clone() else { return };
    let Some(meta) = app.meta(&id).cloned() else { return };
    let is_draft = app.is_draft(&id);

    // The actions come first, so the title knows how much room is left.
    let mut right = w - 12.0;
    let btn = 32.0;
    let cy = (TOPBAR_H - btn) * 0.5;
    let (mut toggle_pane, mut toggle_tasks) = (None, None);
    let layout_state = {
        let l = app.layouts.entry(id.clone()).or_default();
        (l.is_open(PaneId::Terminal), l.is_open(PaneId::Review), l.is_open(PaneId::Files), l.is_open(PaneId::Tasks), l.is_open(PaneId::Settings), l.tasks_tab, l.is_open(PaneId::Preview))
    };
    let (dirty, term_busy, running_tasks) = app.live.get(&id).map_or((false, false, 0), |l| {
        let dirty = l.review.as_ref().and_then(|r| r.summary()).is_some_and(|s| s.1 > 0);
        (dirty, l.terminals.iter().any(|x| x.term.is_busy()), l.session.snapshot().iter().filter(|x| !x.state().finished()).count())
    });

    let more = rect(right - btn, cy, btn, btn);
    if pane_button(ui, 0xA020, more, Icon::MoreV, "更多面板", false, false, true) {
        ui.toggle_popup(0xA021);
    }
    let items = [
        MenuItem::new("文件").icon(Icon::Folder).checked(layout_state.2),
        MenuItem::new("任务").icon(Icon::ListChecks).checked(layout_state.3 && layout_state.5 == TasksTab::Tasks),
        MenuItem::new("历史记录").icon(Icon::History).checked(layout_state.3 && layout_state.5 == TasksTab::History),
        MenuItem::new("对话设置").icon(Icon::Sliders).checked(layout_state.4),
    ];
    match ui.menu(0xA021, more, true, 180.0, &items) {
        Some(0) => toggle_pane = Some(PaneId::Files),
        Some(1) => toggle_tasks = Some(TasksTab::Tasks),
        Some(2) => toggle_tasks = Some(TasksTab::History),
        Some(3) => toggle_pane = Some(PaneId::Settings),
        _ => {}
    }
    right -= btn + 2.0;
    let r = rect(right - btn, cy, btn, btn);
    if pane_button(ui, 0xA022, r, Icon::Globe, "预览", layout_state.6, false, true) {
        toggle_pane = Some(PaneId::Preview);
    }
    right -= btn + 2.0;
    let r = rect(right - btn, cy, btn, btn);
    let has_repo = app.live.get(&id).is_some_and(|l| l.review.as_ref().is_some_and(|x| x.summary().is_some()));
    if pane_button(ui, 0xA023, r, Icon::GitCompare, if has_repo { "审阅" } else { "这个任务的工作区不是 Git 仓库" }, layout_state.1, dirty, has_repo) {
        toggle_pane = Some(PaneId::Review);
    }
    right -= btn + 2.0;
    let r = rect(right - btn, cy, btn, btn);
    if pane_button(ui, 0xA024, r, Icon::Terminal, "终端  ⌘J", layout_state.0, term_busy, true) {
        toggle_pane = Some(PaneId::Terminal);
    }
    right -= btn + 2.0;
    if running_tasks > 0 {
        let label = format!("{running_tasks} 个任务运行中");
        let lw = ui.measure(&label, &FINE) + 26.0;
        let r = rect(right - lw - 4.0, (TOPBAR_H - 24.0) * 0.5, lw, 24.0);
        let resp = ui.interact(0xA025, r);
        ui.fill(r, 12.0, t.subtle_hover.fade(0.5 + 0.5 * resp.hover_t));
        ui.progress_ring(r.x + 12.0, r.y + 12.0, 5.0, 1.25);
        ui.label_vcentered(r.x + 21.0, r, &label, &FINE, t.text_secondary);
        if resp.clicked {
            toggle_tasks = Some(TasksTab::Tasks);
        }
        right -= lw + 8.0;
    }

    if let Some(p) = toggle_pane {
        app.layouts.entry(id.clone()).or_default().toggle(p);
    }
    if let Some(tab) = toggle_tasks {
        let l = app.layouts.entry(id.clone()).or_default();
        if l.is_open(PaneId::Tasks) && l.tasks_tab == tab {
            l.close(PaneId::Tasks);
        } else {
            l.tasks_tab = tab;
            l.open(PaneId::Tasks);
        }
    }

    // One line: the title, which renames in place, then its project in the quiet colour.
    let project = app.doc.project(&meta.project_id).map_or("", |p| p.name.as_str()).to_owned();
    let title = if meta.title.is_empty() { "新任务" } else { meta.title.as_str() };
    let avail = (right - left - 12.0).max(40.0);
    if let Some((edit_id, edit)) = &mut app.title_edit
        && *edit_id == id
    {
        let w = (ui.measure(&edit.text, &BODY_SMALL_STRONG) + 40.0).clamp(120.0, avail.min(420.0));
        let done = edit.show(ui, 0xA030, rect(left - 6.0, (TOPBAR_H - 28.0) * 0.5, w, 28.0), "", &BODY_SMALL_STRONG, true);
        let finished = done.submitted || (ui.input.pressed && ui.mem.focus != 0xA030);
        let cancelled = ui.input.keys.iter().any(|k| k.key == Key::Escape);
        if finished || cancelled {
            let text = edit.text.trim().to_owned();
            app.title_edit = None;
            if finished && !cancelled && !text.is_empty() {
                if let Some(m) = app.meta_mut(&id) {
                    m.title = text;
                }
                app.mark_dirty();
            }
        }
        return;
    }
    let tw = ui.measure(title, &BODY_SMALL_STRONG).min(avail * 0.75);
    let tr = rect(left - 6.0, (TOPBAR_H - 26.0) * 0.5, tw + 12.0, 26.0);
    let resp = ui.interact(0xA031, tr);
    if !is_draft {
        if resp.hover_t > 0.0 {
            ui.fill(tr, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
        }
        if resp.hovered {
            ui.cursor = CursorKind::Text;
            ui.tooltip(tr, "点击重命名");
        }
        if resp.clicked {
            let mut edit = TextEdit::new();
            edit.set_text(title.to_owned());
            edit.select_all();
            ui.mem.focus = 0xA030;
            app.title_edit = Some((id.clone(), edit));
        }
    }
    ui.label_ellipsis(left, bar.y + (TOPBAR_H - BODY_SMALL_STRONG.line) * 0.5, tw, title, &BODY_SMALL_STRONG, t.text_primary);
    ui.label_ellipsis(left + tw + 9.0, bar.y + (TOPBAR_H - CAPTION.line) * 0.5, (avail - tw - 9.0).max(0.0), &project, &CAPTION, t.text_tertiary);
}

#[allow(clippy::too_many_arguments)]
fn pane_button(ui: &mut Ui<'_>, id: u64, r: Rect, icon: Icon, tip: &str, pressed: bool, activity: bool, enabled: bool) -> bool {
    let t = ui.theme;
    let resp = ui.interact(id, r);
    if pressed {
        ui.fill(r, CONTROL_RADIUS, t.subtle_pressed.mix(t.subtle_hover, resp.hover_t));
        ui.card(r, CONTROL_RADIUS, t.subtle_hover, t.control_stroke);
    } else if enabled && resp.hover_t > 0.0 {
        ui.fill(r, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
    }
    let color = if !enabled {
        t.text_disabled
    } else if pressed {
        t.accent_text
    } else {
        t.text_secondary
    };
    ui.icon(icon, r.x + r.w * 0.5, r.y + r.h * 0.5, 15.0, color);
    if activity {
        ui.fill(rect(r.right() - 9.0, r.y + 5.0, 5.0, 5.0), 2.5, t.accent);
    }
    if resp.hovered {
        if enabled {
            ui.cursor = CursorKind::Hand;
        }
        ui.tooltip(r, tip.to_owned());
    }
    enabled && resp.clicked
}

pub fn overlays(app: &mut App, ui: &mut Ui<'_>) {
    if app.confirm.is_some() {
        confirm_dialog(app, ui);
    }
    if app.item_editor.is_some() {
        item_editor(app, ui);
    }
    if app.search.is_some() {
        search_palette(app, ui);
    }
}

fn confirm_dialog(app: &mut App, ui: &mut Ui<'_>) {
    let t = ui.theme;
    let saved_clip = ui.clip;
    let saved_layer = ui.layer;
    let (title, body) = match app.confirm.as_ref().unwrap() {
        Confirm::RemoveProject(id) => {
            let name = app.doc.project(id).map_or("", |p| p.name.as_str());
            let n = app.doc.conversations.iter().filter(|c| c.project_id == *id).count();
            (format!("移除项目“{name}”？"), format!("这个项目里的 {n} 个对话会被删除。磁盘上的文件夹不受影响。"))
        }
    };
    let r = ui.modal(420.0, 160.0);
    ui.label(r.x + 24.0, r.y + 22.0, &title, &BODY_STRONG, t.text_primary);
    ui.label_ellipsis(r.x + 24.0, r.y + 54.0, r.w - 48.0, &body, &BODY_SMALL, t.text_secondary);
    let by = r.bottom() - 52.0;
    if ui.accent_button(0xA040, rect(r.right() - 24.0 - 96.0, by, 96.0, 32.0), "确定", true).clicked {
        match app.confirm.take().unwrap() {
            Confirm::RemoveProject(id) => app.remove_project(&id),
        }
    }
    if ui.button(0xA041, rect(r.right() - 24.0 - 96.0 - 8.0 - 96.0, by, 96.0, 32.0), "取消").clicked {
        app.confirm = None;
    }
    ui.layer = saved_layer;
    ui.clip = saved_clip;
}

/// Edits one item of the conversation: what the model is told on the next request is what this says.
fn item_editor(app: &mut App, ui: &mut Ui<'_>) {
    let t = ui.theme;
    let (saved_clip, saved_layer) = (ui.clip, ui.layer);
    let r = ui.modal(680.0, 480.0);
    ui.label(r.x + 20.0, r.y + 16.0, "编辑内容", &BODY_STRONG, t.text_primary);
    ui.label(r.x + 20.0, r.y + 38.0, "模型下一次请求看到的就是这里的文字。", &CAPTION, t.text_tertiary);
    let editor = app.item_editor.as_mut().unwrap();
    editor.edit.show(ui, 0xA070, rect(r.x + 20.0, r.y + 62.0, r.w - 40.0, r.h - 62.0 - 60.0), "", &BODY_SMALL, false);
    let by = r.bottom() - 46.0;
    let save = ui.accent_button(0xA071, rect(r.right() - 20.0 - 88.0, by, 88.0, 32.0), "保存", true).clicked;
    let cancel = ui.button(0xA072, rect(r.right() - 20.0 - 88.0 - 8.0 - 88.0, by, 88.0, 32.0), "取消").clicked;
    ui.layer = saved_layer;
    ui.clip = saved_clip;
    if save {
        app.commit_edit();
    } else if cancel {
        app.item_editor = None;
    }
}

fn search_palette(app: &mut App, ui: &mut Ui<'_>) {
    let t = ui.theme;
    let saved_clip = ui.clip;
    let saved_layer = ui.layer;
    let query = app.search.as_ref().unwrap().edit.text.trim().to_lowercase();
    let mut results: Vec<(String, String, String)> = Vec::new();
    let mut convs: Vec<&ConvMeta> = app.doc.conversations.iter().collect();
    convs.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    for c in convs {
        if query.is_empty() || c.title.to_lowercase().contains(&query) {
            let project = app.doc.project(&c.project_id).map_or_else(String::new, |p| p.name.clone());
            results.push((c.id.clone(), c.title.clone(), project));
            if results.len() >= 12 {
                break;
            }
        }
    }
    let list_h = results.len().max(1) as f32 * 34.0 + 10.0;
    let r = ui.modal_at(64.0, 560.0, 44.0 + list_h);
    let search = app.search.as_mut().unwrap();
    let field = rect(r.x + 38.0, r.y + 12.0, r.w - 50.0, 22.0);
    ui.mem.focus = 0xA050;
    ui.icon(Icon::Search, r.x + 20.0, r.y + 22.0, 13.0, t.text_tertiary);
    let resp = search.edit.show_bare(ui, 0xA050, field, "搜索对话…", &BODY_SMALL, true);
    if resp.changed {
        search.current = 0;
    }
    for k in &ui.input.keys {
        match k.key {
            Key::Down => search.current = (search.current + 1).min(results.len().saturating_sub(1)),
            Key::Up => search.current = search.current.saturating_sub(1),
            _ => {}
        }
    }
    let current = search.current.min(results.len().saturating_sub(1));
    ui.fill(rect(r.x, r.y + 44.0, r.w, 1.0), 0.0, t.divider);
    let mut choose = None;
    if results.is_empty() {
        ui.label(r.x + 18.0, r.y + 56.0, "没有匹配的对话", &CAPTION, t.text_tertiary);
    }
    for (i, (id, title, project)) in results.iter().enumerate() {
        let row = rect(r.x + 5.0, r.y + 49.0 + i as f32 * 34.0, r.w - 10.0, 32.0);
        let rr = ui.interact(0xA060 + i as u64, row);
        if i == current || rr.hovered {
            ui.fill(row, CONTROL_RADIUS, t.subtle_hover);
        }
        ui.label_ellipsis(row.x + 12.0, row.y + (32.0 - BODY_SMALL.line) * 0.5, row.w - 160.0, title, &BODY_SMALL, t.text_primary);
        ui.label_right(row.right() - 12.0, row.y + (32.0 - FINE.line) * 0.5, project, &FINE, t.text_tertiary);
        if rr.clicked {
            choose = Some(id.clone());
        }
    }
    if resp.submitted
        && let Some((id, ..)) = results.get(current)
    {
        choose = Some(id.clone());
    }
    ui.layer = saved_layer;
    ui.clip = saved_clip;
    if let Some(id) = choose {
        app.search = None;
        ui.mem.focus = 0;
        app.select_conversation(&id);
    }
}
