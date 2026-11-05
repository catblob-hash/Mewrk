//! The tile field: the chat, and the side panes beside it.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, Rect, Ui};
use mewrk_core::tasks::{Task, TaskKind, TaskState};
use mewrk_core::*;
use mewrk_panes::{FilesPane, ReviewPane};
use mewrk_term::Terminal;

use crate::app::App;
use crate::composer::{ComposerCtx, COMPOSER_MAX_W};
use crate::docks;
use crate::layout::{self, PaneId, TasksTab, Tile};
use crate::live::{LiveConv, LoadState, TermTab};
use crate::timeline::{thousands, TimelineAction};

const HEADER_H: f32 = 32.0;
/// Widget ids of the terminals: Cmd+K means something else while one has the keyboard.
pub const TERM_IDS: std::ops::Range<u64> = 0xD800..0xE800;

pub fn field(app: &mut App, ui: &mut Ui<'_>, field: Rect) {
    let Some(id) = app.active.clone() else { return };
    ensure_panes(app, &id);
    let t = ui.theme;
    let mut layout = app.layouts.remove(&id).unwrap_or_default();
    let tiles = layout::place(ui, &mut layout, field, 0xB000, &t);
    if let Some(chat) = tiles.chat {
        ui.card_radii(chat.r, chat.radii, t.layer, t.card_stroke);
        ui.with_clip(chat.r, |ui| chat_tile(app, ui, &id, chat, &mut layout));
    }
    for (pane, tile) in &tiles.panes {
        side_pane(app, ui, &id, *pane, *tile, &mut layout);
    }
    app.layouts.insert(id, layout);
}

/// A review pane is created as soon as a conversation has a folder, because the composer's chips read it too.
fn ensure_panes(app: &mut App, id: &str) {
    let Some(meta) = app.meta(id) else { return };
    let is_real_project = app.doc.project(&meta.project_id).is_some_and(|p| p.path.is_some()) || !meta.settings.extra_workspaces.is_empty();
    let workspaces = app.workspaces_of(meta);
    let wake = app.wake.clone();
    let Some(live) = app.live.get_mut(id) else { return };
    if live.review.is_none() && is_real_project {
        live.review = Some(ReviewPane::new(workspaces, wake));
    }
    if live.files_touched {
        live.files_touched = false;
        if let Some(r) = &mut live.review {
            r.refresh();
        }
    }
    if let Some(r) = &mut live.review {
        r.poll();
    }
}

fn chat_tile(app: &mut App, ui: &mut Ui<'_>, id: &str, tile: Tile, layout: &mut layout::PaneLayout) {
    let t = ui.theme;
    let Some(meta) = app.meta(id).cloned() else { return };
    let is_draft = app.is_draft(id);
    let project_name = app.doc.project(&meta.project_id).map_or_else(String::new, |p| p.name.clone());
    let provider = app.doc.provider(&meta.settings.provider_id);
    let model = provider.and_then(|p| p.models.iter().find(|m| m.id == meta.settings.model_id));
    let model_label = model.map_or_else(String::new, |m| format!("{} · {}", provider.map_or("", |p| p.name.as_str()), m.label()));
    let window = model.map_or(0, ModelProfile::context_window);
    let workspaces = app.workspaces_of(&meta);
    let has_folder = app.doc.project(&meta.project_id).is_some_and(|p| p.path.is_some());
    let workspace_name = if has_folder { workspaces.first().and_then(|p| p.file_name()).map_or_else(String::new, |s| s.to_string_lossy().into_owned()) } else { String::new() };
    let enter_sends = app.doc.settings.enter_sends;
    let wide = app.doc.settings.wide_messages;
    let doc = std::mem::take(&mut app.doc);

    let mut send = false;
    let mut actions: Vec<TimelineAction> = Vec::new();
    let out;
    let mut decisions: Vec<(u64, ApprovalDecision)> = Vec::new();
    let mut answer = None;
    {
        let Some(live) = app.live.get_mut(id) else {
            app.doc = doc;
            return;
        };
        let (branch, dirty) = live.review.as_ref().and_then(|r| r.summary()).map_or((String::new(), 0), |s| (s.0, s.1));
        let area = tile.r;
        let composer_w = (area.w - 48.0).clamp(120.0, COMPOSER_MAX_W);
        let composer_h = live.composer.height(ui, composer_w);
        let mut dock_h = 0.0;
        if !live.approvals.is_empty() {
            dock_h += docks::APPROVAL_H + 8.0;
        }
        if let Some(q) = live.questions.first() {
            dock_h += docks::question_height(q, &app.question_state) + 8.0;
        }
        let queue_h = if live.queue.is_empty() { 0.0 } else { live.queue.len() as f32 * 30.0 + 4.0 };
        let reserved = composer_h + dock_h + queue_h;
        let list = rect(area.x, area.y, area.w, (area.h - reserved - 8.0).max(0.0));

        // Timeline, or the landing page while there is nothing yet.
        let col_max = if wide { 1180.0 } else { COMPOSER_MAX_W };
        let col_w = (area.w - 48.0).clamp(120.0, col_max);
        let col_x = area.x + (area.w - col_w) * 0.5;
        if live.items.is_empty() && live.load != LoadState::Loading {
            landing(ui, list, &doc, &meta.project_id, is_draft);
        } else if live.load == LoadState::Loading {
            ui.label_centered(list, "正在载入对话…", &BODY_SMALL, t.text_secondary);
        } else {
            let running = live.is_running();
            let status = live.status.clone();
            live.view.show(ui, 0xB100, &live.items, list, col_x, col_w, running, status.as_ref());
            for action in live.view.actions.drain(..) {
                match action {
                    TimelineAction::OpenReview => layout.open(PaneId::Review),
                    other => actions.push(other),
                }
            }
        }

        let mut y = area.bottom() - reserved;
        for (i, text) in live.queue.clone().iter().enumerate() {
            let r = rect(col_x, y + i as f32 * 30.0, col_w.min(COMPOSER_MAX_W), 26.0);
            ui.card(r, CONTROL_RADIUS, t.card, t.card_stroke);
            ui.icon(Icon::Clock, r.x + 14.0, r.y + 13.0, 11.0, t.text_tertiary);
            ui.label_ellipsis(r.x + 26.0, r.y + 5.0, r.w - 56.0, text, &CAPTION, t.text_secondary);
            let close = rect(r.right() - 24.0, r.y + 3.0, 20.0, 20.0);
            let resp = ui.interact(0xB200 + i as u64, close);
            ui.icon(Icon::Close, close.x + 10.0, close.y + 10.0, 8.0, if resp.hovered { t.text_primary } else { t.text_tertiary });
            if resp.clicked {
                live.queue.remove(i);
                break;
            }
        }
        y += queue_h;
        if let Some(q) = live.questions.first().cloned() {
            let h = docks::question_height(&q, &app.question_state);
            answer = docks::question(ui, 0xB300, area, y, &q, &mut app.question_state).map(|a| (q.request, a));
            y += h + 8.0;
        }
        if let Some(a) = live.approvals.first().cloned() {
            if let Some(d) = docks::approval(ui, 0xB400, area, y, &a) {
                decisions.push((a.request, d));
            }
            y += docks::APPROVAL_H + 8.0;
        }

        let ctx = ComposerCtx {
            doc: &doc,
            meta: &meta,
            project_name: &project_name,
            branch: &branch,
            dirty_files: dirty,
            running: live.is_running(),
            is_draft,
            enter_sends,
            model_label: &model_label,
            context_tokens: live.context_tokens,
            context_window: window,
            workspace_name: &workspace_name,
            workspace_count: workspaces.len(),
        };
        let composer_area = rect(area.x, y, area.w, composer_h);
        out = {
            let c = live.composer.show(ui, 0xB500, composer_area, &ctx);
            if c.send {
                send = true;
            }
            c
        };
        for path in ui.input.dropped.iter() {
            live.composer.add_images(vec![path.clone()]);
        }
        if out.stop
            && let Some(h) = &live.handle
        {
            h.stop();
        }
        for (request, decision) in decisions.drain(..) {
            answer_approval(live, request, decision);
        }
        if let Some((request, a)) = answer.take() {
            let run = live.handle.clone();
            for c in live.children.iter().map(|c| c.handle.clone()).chain(run) {
                c.answer(request, a.clone());
            }
            live.questions.retain(|q| q.request != request);
        }
    }
    app.doc = doc;

    for action in actions {
        match action {
            TimelineAction::EditItem(item) => app.begin_edit(id, item),
            TimelineAction::DeleteItem(item) => app.delete_item(id, item),
            TimelineAction::Regenerate(item) => app.regenerate(id, item),
            TimelineAction::Retry => app.retry(id),
            TimelineAction::OpenReview => {}
        }
    }
    if send {
        app.send();
    }
    if out.pick_files {
        app.requests.pick_images = true;
    }
    if out.pick_project {
        app.requests.pick_project = true;
    }
    if out.open_review {
        layout.open(PaneId::Review);
    }
    if out.open_providers {
        app.page = crate::app::Page::Settings;
    }
    let mut changed = false;
    if let Some(m) = app.meta_mut(id) {
        if let Some(p) = out.choose_project {
            m.project_id = p;
            changed = true;
        }
        if let Some((p, model)) = out.choose_model {
            m.settings.provider_id = p;
            m.settings.model_id = model;
            changed = true;
        }
        if let Some(e) = out.choose_effort {
            m.settings.effort = e;
            changed = true;
        }
        if let Some(p) = out.choose_permission {
            m.settings.permission = p;
            changed = true;
        }
        if out.toggle_plan {
            m.settings.plan_mode = !m.settings.plan_mode;
            changed = true;
        }
    }
    if changed {
        let settings = app.meta(id).map(|m| m.settings.clone());
        if let Some(s) = settings {
            // New tasks start from what was last chosen.
            app.doc.settings.default_provider.clone_from(&s.provider_id);
            app.doc.settings.default_model.clone_from(&s.model_id);
            app.doc.settings.default_effort.clone_from(&s.effort);
            app.doc.settings.default_permission = s.permission;
            if let Some(l) = app.live.get(id) {
                l.session.plan_mode.store(s.plan_mode, Ordering::Relaxed);
            }
        }
        app.mark_dirty();
    }
}

fn answer_approval(live: &mut LiveConv, request: u64, decision: ApprovalDecision) {
    if let Some(h) = &live.handle {
        h.approve(request, decision);
    }
    for c in &live.children {
        c.handle.approve(request, decision);
    }
    live.approvals.retain(|a| a.request != request);
}

/// A new task's page: the brand, and what the project was last working on.
fn landing(ui: &mut Ui<'_>, list: Rect, doc: &Document, project_id: &str, _is_draft: bool) {
    let t = ui.theme;
    let cy = list.y + list.h * 0.42;
    let cx = list.x + list.w * 0.5;
    ui.icon(Icon::MarkFilled, cx, cy - 40.0, 36.0, t.accent);
    ui.label_centered(rect(list.x, cy - 8.0, list.w, 28.0), "这个对话还没有消息", &BODY_STRONG, t.text_primary);
    if doc.settings.providers.is_empty() {
        ui.label_centered(rect(list.x, cy + 20.0, list.w, 20.0), "先在设置里添加一个模型供应商", &CAPTION, t.text_tertiary);
    } else {
        let mut recent: Vec<&ConvMeta> = doc.conversations.iter().filter(|c| c.project_id == project_id).collect();
        recent.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        if recent.is_empty() {
            ui.label_centered(rect(list.x, cy + 20.0, list.w, 20.0), "喵。开工。", &CAPTION, t.text_tertiary);
        } else {
            ui.label_centered(rect(list.x, cy + 20.0, list.w, 20.0), &format!("这个项目已有 {} 个任务，在左侧可以继续", recent.len()), &CAPTION, t.text_tertiary);
        }
    }
}

fn side_pane(app: &mut App, ui: &mut Ui<'_>, id: &str, pane: PaneId, tile: Tile, layout: &mut layout::PaneLayout) {
    let t = ui.theme;
    let r = tile.r;
    ui.shadow(r, tile.radii[0], 8.0, 3.0, t.shadow.fade(0.3));
    ui.card_radii(r, tile.radii, if t.dark { Color::rgb(0x2B2B2B) } else { Color::rgb(0xF9F9F9) }, t.card_stroke);
    let base = 0xC000 + pane.tag() * 0x1000;

    // The title bar: controls on the right, whatever the pane puts on the left.
    let close = rect(r.right() - 28.0, r.y + 4.0, 24.0, 24.0);
    let expand = rect(close.x - 26.0, r.y + 4.0, 24.0, 24.0);
    let mut close_pane = false;
    if header_button(ui, base + 1, close, Icon::Close, 10.0, "关闭面板") {
        close_pane = true;
    }
    let expanded = layout.expanded == Some(pane);
    if header_button(ui, base + 2, expand, if expanded { Icon::Collapse } else { Icon::Expand }, 12.0, if expanded { "还原" } else { "放大" }) {
        layout.toggle_expand(pane);
    }
    let title_r = rect(r.x + 8.0, r.y, expand.x - r.x - 12.0, HEADER_H);
    let body = rect(r.x + 1.0, r.y + HEADER_H, r.w - 2.0, (r.h - HEADER_H - 1.0).max(0.0));

    ui.with_clip(rect(r.x, r.y, r.w, r.h), |ui| match pane {
        PaneId::Terminal => terminal_pane(app, ui, id, base, title_r, body),
        PaneId::Review => {
            ui.label_vcentered(title_r.x + 4.0, title_r, "审阅", &BODY_SMALL, t.text_secondary);
            if let Some(l) = app.live.get_mut(id) {
                match &mut l.review {
                    Some(review) => review.show(ui, base + 100, body),
                    None => ui.label_centered(body, "这个任务的工作区不是 Git 仓库", &CAPTION, t.text_tertiary),
                }
            }
        }
        PaneId::Preview => {
            ui.label_vcentered(title_r.x + 4.0, title_r, "预览", &BODY_SMALL, t.text_secondary);
            let covered = ui.mem.open_popup != 0 || app.search.is_some() || app.confirm.is_some() || app.item_editor.is_some();
            let (parent, frame_no) = (app.native_view, app.frame_no);
            if let Some(l) = app.live.get_mut(id) {
                l.preview.get_or_insert_with(crate::preview::Preview::new).show(ui, base + 100, body, parent, covered, frame_no);
            }
        }
        PaneId::Files => {
            ui.label_vcentered(title_r.x + 4.0, title_r, "文件", &BODY_SMALL, t.text_secondary);
            let Some(meta) = app.meta(id) else { return };
            let root = app.workspaces_of(meta).into_iter().next();
            let wake = app.wake.clone();
            let Some(l) = app.live.get_mut(id) else { return };
            let Some(root) = root else { return };
            let files = l.files.get_or_insert_with(|| FilesPane::new(root, wake));
            files.show(ui, base + 100, body);
            if let Some(path) = files.take_mention() {
                let line = format!("{} ", path.display());
                l.composer.edit.insert(&line);
                l.composer.want_focus = true;
            }
        }
        PaneId::Tasks => tasks_pane(app, ui, id, base, title_r, body, layout),
        PaneId::Settings => {
            ui.label_vcentered(title_r.x + 4.0, title_r, "对话设置", &BODY_SMALL, t.text_secondary);
            crate::settings::conversation_settings(app, ui, id, base + 100, body);
        }
        PaneId::Subagent => subagent_pane(app, ui, id, base, title_r, body),
    });
    if close_pane {
        layout.close(pane);
    }
}

fn header_button(ui: &mut Ui<'_>, id: u64, r: Rect, icon: Icon, size: f32, tip: &str) -> bool {
    let t = ui.theme;
    let resp = ui.interact(id, r);
    if resp.hover_t > 0.0 {
        ui.fill(r, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
    }
    ui.icon(icon, r.x + 12.0, r.y + 12.0, size, if resp.hovered { t.text_primary } else { t.text_secondary });
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
        ui.tooltip(r, tip.to_owned());
    }
    resp.clicked
}

fn terminal_pane(app: &mut App, ui: &mut Ui<'_>, id: &str, base: u64, title: Rect, body: Rect) {
    let t = ui.theme;
    let Some(meta) = app.meta(id) else { return };
    let cwd = app.workspaces_of(meta).into_iter().next();
    let wake = app.wake.clone();
    let Some(l) = app.live.get_mut(id) else { return };
    let spawn = |l: &mut LiveConv, wake: std::sync::Arc<dyn Fn() + Send + Sync>| {
        let Some(cwd) = &cwd else { return };
        let _ = std::fs::create_dir_all(cwd);
        if let Ok(term) = Terminal::spawn(cwd, None, wake) {
            let id = l.next_term_id;
            l.next_term_id += 1;
            l.terminals.push(TermTab { term, id });
            l.active_term = l.terminals.len() - 1;
        }
    };
    if l.terminals.is_empty() {
        spawn(l, wake.clone());
    }
    // Tabs in the title bar.
    let mut x = title.x;
    let mut close_tab = None;
    let mut select = None;
    for (i, tab) in l.terminals.iter().enumerate() {
        let name = tab.term.title();
        let w = (ui.measure(&name, &CAPTION) + 34.0).min(180.0);
        if x + w > title.right() - 28.0 {
            break;
        }
        let r = rect(x, title.y + 4.0, w, 24.0);
        let resp = ui.interact(base + 10 + tab.id * 4, r);
        let active = i == l.active_term;
        let fill = if active { t.subtle_hover } else { t.subtle_hover.fade(resp.hover_t) };
        if fill.a > 0 {
            ui.fill(r, CONTROL_RADIUS, fill);
        }
        ui.label_ellipsis(r.x + 8.0, r.y + 4.0, w - 28.0, &name, &CAPTION, if active { t.text_primary } else { t.text_secondary });
        if resp.clicked {
            select = Some(i);
        }
        if active || resp.hovered {
            let cr = rect(r.right() - 20.0, r.y + 3.0, 18.0, 18.0);
            let cresp = ui.interact(base + 11 + tab.id * 4, cr);
            ui.icon(Icon::Close, cr.x + 9.0, cr.y + 9.0, 7.0, if cresp.hovered { t.text_primary } else { t.text_tertiary });
            if cresp.clicked {
                close_tab = Some(i);
            }
        }
        x += w + 2.0;
    }
    let plus = rect((x + 2.0).min(title.right() - 24.0), title.y + 4.0, 24.0, 24.0);
    let add = header_button(ui, base + 9, plus, Icon::Plus, 11.0, "新建终端");
    if let Some(i) = select {
        l.active_term = i;
    }
    if let Some(i) = close_tab {
        let mut removed = l.terminals.remove(i);
        removed.term.kill();
        l.active_term = l.active_term.min(l.terminals.len().saturating_sub(1));
    }
    if add {
        spawn(l, wake);
    }
    if let Some(tab) = l.terminals.get_mut(l.active_term) {
        let tid = TERM_IDS.start + tab.id * 8;
        tab.term.show(ui, tid, body);
    }
}

fn format_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

fn tasks_pane(app: &mut App, ui: &mut Ui<'_>, id: &str, base: u64, title: Rect, body: Rect, layout: &mut layout::PaneLayout) {
    let t = ui.theme;
    // Two tabs share the pane: the conversation's tasks and its history.
    let mut x = title.x;
    for (tab, label, icon) in [(TasksTab::Tasks, "任务", Icon::ListChecks), (TasksTab::History, "历史记录", Icon::History)] {
        let w = ui.measure(label, &CAPTION) + 34.0;
        let r = rect(x, title.y + 4.0, w, 24.0);
        let resp = ui.interact(base + 10 + tab as u64, r);
        let active = layout.tasks_tab == tab;
        let fill = if active { t.subtle_hover } else { t.subtle_hover.fade(resp.hover_t) };
        if fill.a > 0 {
            ui.fill(r, CONTROL_RADIUS, fill);
        }
        ui.icon(icon, r.x + 13.0, r.y + 12.0, 12.0, t.text_secondary);
        ui.label_vcentered(r.x + 25.0, r, label, &CAPTION, if active { t.text_primary } else { t.text_secondary });
        if resp.clicked {
            layout.tasks_tab = tab;
        }
        x += w + 2.0;
    }
    let Some(l) = app.live.get_mut(id) else { return };
    match layout.tasks_tab {
        TasksTab::Tasks => {
            let tasks = l.session.snapshot();
            if tasks.is_empty() {
                ui.label_centered(rect(body.x, body.y + 24.0, body.w, 20.0), "这个对话还没有后台任务或子代理", &CAPTION, t.text_tertiary);
                return;
            }
            let mut y = body.y + 8.0;
            let (running, finished): (Vec<&_>, Vec<&_>) = tasks.iter().partition(|x| !x.state().finished());
            let mut open_child = None;
            for (i, task) in running.iter().chain(finished.iter()).enumerate() {
                let row = rect(body.x + 8.0, y, body.w - 16.0, 44.0);
                let (clicked, stop) = task_row(ui, base + 100 + i as u64 * 8, row, task);
                if clicked && task.kind == TaskKind::Agent {
                    open_child = Some(Arc::clone(task));
                }
                if stop {
                    task.stop();
                }
                y += 46.0;
                if y > body.bottom() {
                    break;
                }
            }
            if let Some(task) = open_child {
                let run = task.run.lock().clone();
                if let Some(run) = run {
                    l.shown_child = l.children.iter().position(|c| c.handle.id == run.id);
                    layout.open(PaneId::Subagent);
                }
            }
        }
        TasksTab::History => {
            let mut y = body.y + 8.0;
            let mut target = None;
            let users: Vec<(ItemId, String, u64)> = l
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::User { id, text, at, .. } => Some((*id, text.lines().next().unwrap_or("").to_owned(), *at)),
                    _ => None,
                })
                .collect();
            if users.is_empty() {
                ui.label_centered(rect(body.x, body.y + 24.0, body.w, 20.0), "还没有历史", &CAPTION, t.text_tertiary);
            }
            for (i, (item, text, _)) in users.iter().enumerate().rev() {
                let row = rect(body.x + 8.0, y, body.w - 16.0, 30.0);
                let resp = ui.interact(base + 3000 + i as u64, row);
                if resp.hover_t > 0.0 {
                    ui.fill(row, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
                }
                ui.label_ellipsis(row.x + 8.0, row.y + (30.0 - CAPTION.line) * 0.5, row.w - 16.0, text, &CAPTION, t.text_primary);
                if resp.clicked {
                    target = Some(*item);
                }
                y += 30.0;
                if y > body.bottom() {
                    break;
                }
            }
            if let Some(item) = target {
                l.view.jump_to(&l.items, item);
            }
        }
    }
}

/// Returns (the row was clicked, stop was clicked).
fn task_row(ui: &mut Ui<'_>, id: u64, row: Rect, task: &Task) -> (bool, bool) {
    let t = ui.theme;
    let resp = ui.interact(id, row);
    if resp.hover_t > 0.0 {
        ui.fill(row, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
    }
    let state = task.state();
    let cy = row.y + row.h * 0.5;
    let mut stop = false;
    let label_color = match &state {
        TaskState::Failed(_) => t.critical,
        TaskState::Running => t.text_primary,
        _ => t.text_secondary,
    };
    match &state {
        TaskState::Running => ui.progress_ring(row.x + 14.0, cy, 6.0, 1.5),
        TaskState::Completed => ui.icon(Icon::Check, row.x + 14.0, cy, 11.0, t.success),
        TaskState::Failed(_) => ui.icon(Icon::Close, row.x + 14.0, cy, 10.0, t.critical),
        TaskState::Stopped => ui.icon(Icon::Stop, row.x + 14.0, cy, 9.0, t.text_tertiary),
    }
    let mut right = row.right() - 6.0;
    if !state.finished() {
        let sr = rect(right - 22.0, cy - 11.0, 22.0, 22.0);
        let sresp = ui.interact(id + 1, sr);
        if sresp.hover_t > 0.0 {
            ui.fill(sr, CONTROL_RADIUS, t.critical_bg.fade(sresp.hover_t));
        }
        ui.icon(Icon::Stop, sr.x + 11.0, sr.y + 11.0, 9.0, if sresp.hovered { t.critical } else { t.text_tertiary });
        stop = sresp.clicked;
        right -= 26.0;
    }
    let end = task.finished_at.load(Ordering::Relaxed);
    let elapsed = if end > 0 { end - task.started_at } else { now_ms().saturating_sub(task.started_at) };
    let mut metrics = String::new();
    let tokens = task.tokens.load(Ordering::Relaxed);
    if tokens > 0 {
        metrics.push_str(&format!("{} ", thousands(tokens)));
    }
    let calls = task.tool_calls.load(Ordering::Relaxed);
    if calls > 0 {
        metrics.push_str(&format!("{calls}× "));
    }
    metrics.push_str(&format_elapsed(elapsed));
    let mw = ui.measure(&metrics, &MONO_SMALL);
    ui.label(right - mw, cy - MONO_SMALL.line * 0.5, &metrics, &MONO_SMALL, t.text_tertiary);
    right -= mw + 8.0;
    let name = if task.label.is_empty() { &task.name } else { &task.label };
    ui.label_ellipsis(row.x + 28.0, row.y + 6.0, right - row.x - 28.0, name, &CAPTION_STRONG, label_color);
    let detail = match (&state, task.kind) {
        (TaskState::Failed(e), _) => e.clone(),
        (_, TaskKind::Shell) => "后台命令".to_owned(),
        (_, TaskKind::Agent) => "子代理".to_owned(),
    };
    ui.label_ellipsis(row.x + 28.0, row.y + 24.0, right - row.x - 28.0, &detail, &FINE, t.text_tertiary);
    if resp.hovered && task.kind == TaskKind::Agent {
        ui.cursor = CursorKind::Hand;
    }
    (resp.clicked, stop)
}

fn subagent_pane(app: &mut App, ui: &mut Ui<'_>, id: &str, base: u64, title: Rect, body: Rect) {
    let t = ui.theme;
    let Some(l) = app.live.get_mut(id) else { return };
    let Some(child) = l.shown_child.and_then(|i| l.children.get_mut(i)) else {
        ui.label_vcentered(title.x + 4.0, title, "子代理", &BODY_SMALL, t.text_secondary);
        ui.label_centered(body, "在任务面板里选择一个子代理", &CAPTION, t.text_tertiary);
        return;
    };
    ui.label_ellipsis(title.x + 4.0, title.y + (title.h - BODY_SMALL.line) * 0.5, title.w - 8.0, &child.handle.label.clone(), &BODY_SMALL, t.text_secondary);
    let running = child.end.is_none();
    let status = child.status.clone();
    let col_w = (body.w - 32.0).max(80.0);
    ui.with_clip(body, |ui| {
        child.view.show(ui, base + 100, &child.items, body, body.x + 16.0, col_w, running, status.as_ref());
    });
}
