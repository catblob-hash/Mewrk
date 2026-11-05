//! The composer: the chips above the field, the field, and the row under it.

use std::path::PathBuf;

use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, MenuItem, Rect, Ui};
use fluent::TextEdit;
use mewrk_core::*;

pub const COMPOSER_MAX_W: f32 = 820.0;
const CHIP_H: f32 = 25.0;
const FIELD_MIN: f32 = 29.0;
const FIELD_MAX: f32 = 180.0;
const TYPE: TypeStyle = TypeStyle { size: 14.0, line: 21.0, weight: Weight::Regular, mono: false, italic: false };

pub struct Composer {
    pub edit: TextEdit,
    pub images: Vec<Attachment>,
    pub want_focus: bool,
}

/// What the composer's chips asked for this frame.
#[derive(Default)]
pub struct ComposerOut {
    pub send: bool,
    pub stop: bool,
    pub pick_files: bool,
    pub pick_project: bool,
    pub open_review: bool,
    pub choose_project: Option<String>,
    pub choose_model: Option<(String, String)>,
    pub choose_effort: Option<String>,
    pub choose_permission: Option<PermissionMode>,
    pub toggle_plan: bool,
    pub open_providers: bool,
}

pub struct ComposerCtx<'a> {
    pub doc: &'a Document,
    pub meta: &'a ConvMeta,
    pub project_name: &'a str,
    /// Empty when the conversation has no repository.
    pub branch: &'a str,
    pub dirty_files: u32,
    pub running: bool,
    pub is_draft: bool,
    pub enter_sends: bool,
    pub model_label: &'a str,
    pub context_tokens: u64,
    pub context_window: u64,
    pub workspace_name: &'a str,
    pub workspace_count: usize,
}

impl Composer {
    pub fn new() -> Self {
        Self { edit: TextEdit::new(), images: Vec::new(), want_focus: true }
    }

    pub fn add_images(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            let mime = match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
                Some("png") => "image/png",
                Some("jpg" | "jpeg") => "image/jpeg",
                Some("gif") => "image/gif",
                Some("webp") => "image/webp",
                _ => continue,
            };
            self.images.push(Attachment { path, mime: mime.to_owned() });
        }
    }

    /// How much room the whole thing takes at this width, chips and footer included.
    pub fn height(&mut self, ui: &mut Ui<'_>, width: f32) -> f32 {
        let field = self.edit.measure(ui, &TYPE, (width - 24.0 - 44.0).max(40.0)).clamp(FIELD_MIN, FIELD_MAX);
        let images = if self.images.is_empty() { 0.0 } else { 30.0 };
        CHIP_H + 8.0 + field + 18.0 + images + 6.0 + 29.0 + 12.0
    }

    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, area: Rect, ctx: &ComposerCtx<'_>) -> ComposerOut {
        let mut out = ComposerOut::default();
        let t = ui.theme;
        let width = (area.w - 48.0).clamp(120.0, COMPOSER_MAX_W);
        let x = area.x + (area.w - width) * 0.5;
        let mut y = area.y;

        // The chips: which project and folder this task works in, and the repository's state.
        let mut cx = x + 2.0;
        let chip_y = y;
        if ctx.is_draft {
            let items: Vec<MenuItem<'_>> = ctx.doc.projects.iter().map(|p| MenuItem::new(&p.name).icon(if p.path.is_some() { Icon::Folder } else { Icon::Clock }).checked(p.id == ctx.meta.project_id)).collect();
            let w = chip_width(ui, Some(Icon::Folder), ctx.project_name, true);
            let r = chip(ui, id + 1, cx, chip_y, Some(Icon::Folder), ctx.project_name, true, false, w);
            if let Some(i) = chip_menu(ui, id + 1, r, false, 220.0, &items) {
                out.choose_project = Some(ctx.doc.projects[i].id.clone());
            }
            cx += r.w + 6.0;
        }
        if !ctx.workspace_name.is_empty() {
            let label = if ctx.workspace_count > 1 { format!("{}  {}", ctx.workspace_count, ctx.workspace_name) } else { ctx.workspace_name.to_owned() };
            let w = chip_width(ui, Some(Icon::Folder), &label, false);
            let r = chip(ui, id + 2, cx, chip_y, Some(Icon::Folder), &label, false, true, w);
            cx += r.w + 6.0;
        }
        if !ctx.branch.is_empty() {
            let w = chip_width(ui, Some(Icon::Branch), ctx.branch, false);
            let r = chip(ui, id + 3, cx, chip_y, Some(Icon::Branch), ctx.branch, false, true, w);
            cx += r.w + 6.0;
        }
        let r = icon_chip(ui, id + 4, cx, chip_y, Icon::FolderPlus, "添加文件夹");
        if ui.interact(id + 4, r).clicked {
            out.pick_project = true;
        }
        if !ctx.branch.is_empty() {
            let label = if ctx.dirty_files > 0 { format!("Git 状态 · {}", ctx.dirty_files) } else { "Git 状态".to_owned() };
            let w = ui.measure(&label, &FINE_STRONG) + 34.0;
            let r = rect(x + width - w - 2.0, chip_y, w, CHIP_H);
            let resp = ui.interact(id + 5, r);
            ui.control(r, CONTROL_RADIUS, t.control_fill.mix(t.control_fill_hover, resp.hover_t), t.control_stroke, t.control_stroke_bottom);
            ui.icon(Icon::GitCompare, r.x + 14.0, r.y + CHIP_H * 0.5, 12.0, t.text_secondary);
            ui.label_vcentered(r.x + 26.0, r, &label, &FINE_STRONG, t.text_secondary);
            if resp.hovered {
                ui.cursor = CursorKind::Hand;
            }
            if resp.clicked {
                out.open_review = true;
            }
        }
        y += CHIP_H + 8.0;

        // The field: Fluent's card with the accent underline when focused.
        let text_w = (width - 24.0 - 44.0).max(40.0);
        let field_h = self.edit.measure(ui, &TYPE, text_w).clamp(FIELD_MIN, FIELD_MAX);
        let images_h = if self.images.is_empty() { 0.0 } else { 30.0 };
        let card = rect(x, y, width, field_h + 18.0 + images_h);
        let focused = ui.mem.focus == id + 10;
        ui.shadow(card, OVERLAY_RADIUS, 8.0, 2.0, t.shadow.fade(0.35));
        let fill = if t.dark { Color::rgb(0x2C2C2C) } else { Color::WHITE };
        ui.control(card, OVERLAY_RADIUS, fill, t.control_stroke, if focused { Color::TRANSPARENT } else { t.control_strong_stroke.fade(0.5) });
        if focused {
            ui.fill_radii(rect(card.x + 1.0, card.bottom() - 2.0, card.w - 2.0, 2.0), [0.0, 0.0, OVERLAY_RADIUS, OVERLAY_RADIUS], t.accent);
        }
        let mut field_y = card.y + 9.0;
        if !self.images.is_empty() {
            let mut ix = card.x + 12.0;
            let mut remove = None;
            for (n, img) in self.images.iter().enumerate() {
                let name = img.path.file_name().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                let w = (ui.measure(&name, &FINE) + 34.0).min(180.0);
                let r = rect(ix, card.y + 6.0, w, 24.0);
                ui.card(r, CONTROL_RADIUS, t.card, t.card_stroke);
                ui.label_ellipsis(r.x + 8.0, r.y + 5.0, w - 30.0, &name, &FINE, t.text_secondary);
                let close = rect(r.right() - 20.0, r.y + 4.0, 16.0, 16.0);
                let resp = ui.interact(id + 300 + n as u64, close);
                ui.icon(Icon::Close, close.x + 8.0, close.y + 8.0, 8.0, if resp.hovered { t.text_primary } else { t.text_tertiary });
                if resp.clicked {
                    remove = Some(n);
                }
                ix += w + 6.0;
            }
            if let Some(n) = remove {
                self.images.remove(n);
            }
            field_y += images_h;
        }
        let field = rect(card.x + 12.0, field_y + 4.0, text_w, field_h - 4.0);
        if self.want_focus {
            ui.mem.focus = id + 10;
            self.want_focus = false;
        }
        let placeholder = if ctx.running { "继续输入，发送后排到下一轮…" } else { "给 Agent 发消息…" };
        let resp = self.edit.show_bare(ui, id + 10, field, placeholder, &TYPE, ctx.enter_sends);
        if resp.submitted {
            out.send = true;
        }

        // Send, or stop while a run is going and there is nothing to queue.
        let send = rect(card.right() - 40.0, card.bottom() - 36.0, 28.0, 28.0);
        let has_text = !self.edit.text.trim().is_empty() || !self.images.is_empty();
        let sresp = ui.interact(id + 11, send);
        if ctx.running && !has_text {
            ui.fill(send, 14.0, t.critical.fade(0.15 + 0.15 * sresp.hover_t));
            ui.icon(Icon::Stop, send.x + 14.0, send.y + 14.0, 11.0, t.critical);
            if sresp.clicked {
                out.stop = true;
            }
        } else {
            let (bg, fg) = if has_text { (t.accent.mix(t.accent_hover, sresp.hover_t), t.text_on_accent) } else { (t.control_fill, t.text_disabled) };
            ui.fill(send, 14.0, bg);
            ui.icon(Icon::ArrowUp, send.x + 14.0, send.y + 14.0, 13.0, fg);
            if sresp.clicked && has_text {
                out.send = true;
            }
        }
        if sresp.hovered {
            ui.cursor = CursorKind::Hand;
        }
        y += card.h + 6.0;

        // The row under it: how much rope the agent gets, on the left, and which model, on the right.
        let mut fx = x + 2.0;
        let perm = ctx.meta.settings.permission;
        let w = chip_width(ui, Some(Icon::Shield), perm.label(), true);
        let r = chip(ui, id + 20, fx, y + 2.0, Some(Icon::Shield), perm.label(), true, false, w);
        let items: Vec<MenuItem<'_>> = PermissionMode::ALL.iter().map(|m| MenuItem::new(m.label()).checked(*m == perm)).collect();
        if let Some(i) = chip_menu(ui, id + 20, r, false, 160.0, &items) {
            out.choose_permission = Some(PermissionMode::ALL[i]);
        }
        fx += r.w + 6.0;

        let plan_on = ctx.meta.settings.plan_mode;
        let r = rect(fx, y + 2.0, ui.measure("计划", &FINE_STRONG) + 34.0, CHIP_H);
        let presp = ui.interact(id + 21, r);
        let plan_fill = if plan_on { t.accent.with_alpha(0.18) } else { t.control_fill.mix(t.control_fill_hover, presp.hover_t) };
        ui.control(r, CONTROL_RADIUS, plan_fill, t.control_stroke, t.control_stroke_bottom);
        ui.label_vcentered(r.x + 9.0, r, "计划", &FINE_STRONG, if plan_on { t.accent_text } else { t.text_secondary });
        let (dot_x, dot_y) = (r.right() - 13.0, r.y + CHIP_H * 0.5);
        ui.arc(dot_x, dot_y, 4.0, 1.25, 0.0, std::f32::consts::TAU, if plan_on { t.accent_text } else { t.text_tertiary });
        if plan_on {
            ui.fill(rect(dot_x - 2.0, dot_y - 2.0, 4.0, 4.0), 2.0, t.accent_text);
        }
        if presp.clicked {
            out.toggle_plan = true;
        }
        if presp.hovered {
            ui.cursor = CursorKind::Hand;
        }
        fx += r.w + 6.0;

        let r = icon_chip(ui, id + 22, fx, y + 2.0, Icon::Plus, "添加图片");
        if ui.interact(id + 22, r).clicked {
            out.pick_files = true;
        }

        // From the right: context meter, effort, model.
        let mut right = x + width - 2.0;
        let ring = rect(right - 25.0, y + 2.0, 25.0, CHIP_H);
        let used = if ctx.context_window > 0 { (ctx.context_tokens as f32 / ctx.context_window as f32).min(1.0) } else { 0.0 };
        let (rx, ry) = (ring.x + 12.5, ring.y + 12.5);
        ui.arc(rx, ry, 8.0, 2.0, 0.0, std::f32::consts::TAU, t.control_strong_stroke.fade(0.35));
        if used > 0.0 {
            let color = if used > 0.85 { t.critical } else if used > 0.6 { t.caution } else { t.accent };
            ui.arc(rx, ry, 8.0, 2.0, 0.0, used * std::f32::consts::TAU, color);
        }
        if ui.pointer_in(&ring) {
            ui.tooltip(ring, format!("上下文 {} / {}", crate::timeline::thousands(ctx.context_tokens), crate::timeline::thousands(ctx.context_window)));
        }
        right -= 31.0;

        let effort = if ctx.meta.settings.effort.is_empty() { "默认" } else { effort_label(&ctx.meta.settings.effort) };
        let ew = chip_width(ui, None, effort, true);
        let r = chip(ui, id + 23, right - ew, y + 2.0, None, effort, true, false, ew);
        let items: Vec<MenuItem<'_>> = EFFORTS.iter().map(|e| MenuItem::new(effort_label(e)).checked(*e == ctx.meta.settings.effort)).collect();
        if let Some(i) = chip_menu(ui, id + 23, r, true, 120.0, &items) {
            out.choose_effort = Some(EFFORTS[i].to_owned());
        }
        right -= r.w + 6.0;

        let label = if ctx.model_label.is_empty() { "选择模型" } else { ctx.model_label };
        // The model's name gives way first: it is the one that can run long, and the menu shows it in full.
        let mw = chip_width(ui, None, label, true).min((right - fx - CHIP_H - 12.0).max(60.0));
        let r = chip(ui, id + 24, right - mw, y + 2.0, None, label, true, false, mw);
        let mut model_refs: Vec<(String, String, String)> = Vec::new();
        for p in &ctx.doc.settings.providers {
            for m in &p.models {
                model_refs.push((p.id.clone(), m.id.clone(), format!("{} · {}", p.name, m.label())));
            }
        }
        let mut items: Vec<MenuItem<'_>> = model_refs.iter().map(|(p, m, l)| MenuItem::new(l).checked(*p == ctx.meta.settings.provider_id && *m == ctx.meta.settings.model_id)).collect();
        items.push(MenuItem::new("管理模型供应商…").icon(Icon::Settings).separated());
        if let Some(i) = chip_menu(ui, id + 24, r, true, 300.0, &items) {
            match model_refs.get(i) {
                Some((p, m, _)) => out.choose_model = Some((p.clone(), m.clone())),
                None => out.open_providers = true,
            }
        }
        out
    }
}

pub fn effort_label(e: &str) -> &str {
    match e {
        "off" => "关闭思考",
        "low" => "低",
        "medium" => "中",
        "high" => "高",
        "max" => "最高",
        o => o,
    }
}

fn chip_width(ui: &mut Ui<'_>, icon: Option<Icon>, label: &str, caret: bool) -> f32 {
    (ui.measure(label, &FINE_STRONG) + 18.0 + if icon.is_some() { 18.0 } else { 0.0 } + if caret { 14.0 } else { 0.0 }).min(260.0)
}

/// A small labelled control of width `w`. `muted` is for one that only shows something.
#[allow(clippy::too_many_arguments)]
fn chip(ui: &mut Ui<'_>, id: u64, x: f32, y: f32, icon: Option<Icon>, label: &str, caret: bool, muted: bool, w: f32) -> Rect {
    let t = ui.theme;
    let r = rect(x, y, w, CHIP_H);
    let resp = if muted { Default::default() } else { ui.interact(id, r) };
    let fill = t.control_fill.mix(t.control_fill_hover, resp.hover_t);
    ui.control(r, CONTROL_RADIUS, fill, t.control_stroke, t.control_stroke_bottom);
    let mut lx = r.x + 9.0;
    if let Some(i) = icon {
        ui.icon(i, lx + 6.0, r.y + CHIP_H * 0.5, 12.0, t.text_tertiary);
        lx += 18.0;
    }
    let avail = r.right() - lx - if caret { 18.0 } else { 6.0 };
    ui.label_ellipsis(lx, r.y + (CHIP_H - FINE_STRONG.line) * 0.5, avail, label, &FINE_STRONG, t.text_secondary);
    if caret {
        ui.icon(Icon::ChevronDown, r.right() - 11.0, r.y + CHIP_H * 0.5, 8.0, t.text_tertiary);
    }
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    if resp.clicked {
        ui.toggle_popup(id);
    }
    r
}

fn chip_menu(ui: &mut Ui<'_>, id: u64, anchor: Rect, align_right: bool, w: f32, items: &[MenuItem<'_>]) -> Option<usize> {
    ui.menu(id, anchor, align_right, w, items)
}

fn icon_chip(ui: &mut Ui<'_>, id: u64, x: f32, y: f32, icon: Icon, tip: &str) -> Rect {
    let t = ui.theme;
    let r = rect(x, y, CHIP_H, CHIP_H);
    // The caller asks for the click: only the look and the tooltip live here, and `interact` is idempotent within a frame only for one caller.
    let hover = ui.pointer_in(&r);
    let fill = if hover { t.control_fill_hover } else { t.control_fill };
    ui.control(r, CONTROL_RADIUS, fill, t.control_stroke, t.control_stroke_bottom);
    ui.icon(icon, r.x + CHIP_H * 0.5, r.y + CHIP_H * 0.5, 13.0, t.text_secondary);
    if hover {
        ui.cursor = CursorKind::Hand;
        ui.tooltip(r, tip.to_owned());
    }
    let _ = id;
    r
}
