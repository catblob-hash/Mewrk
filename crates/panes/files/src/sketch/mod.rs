//! The freehand annotation surface: a captured page under a drawing canvas, with a floating toolbar pill at the bottom
//! (upstream's `Sketch/`). The browser pane opens this over a capture, and an image attachment can too.

pub mod raster;
pub mod strokes;

use crate::widgets::{tool, tool_pressed};
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuItem, PopoverMenuSection};
use mewrk_client::prelude::*;
use std::cell::RefCell;
use strokes::*;

fn is_apple() -> bool {
    cfg!(target_os = "macos")
}

pub fn tool_label(tool: SketchTool) -> &'static str {
    match tool {
        SketchTool::Pen => t("画笔", "Pen"),
        SketchTool::Line => t("直线", "Line"),
        SketchTool::Arrow => t("箭头", "Arrow"),
        SketchTool::Rect => t("矩形", "Rectangle"),
        SketchTool::Ellipse => t("椭圆", "Ellipse"),
        SketchTool::Text => t("文字", "Text"),
    }
}

pub fn color_label(color: u32) -> &'static str {
    match color {
        0xE03131 => t("红色", "Red"),
        0x1971C2 => t("蓝色", "Blue"),
        0x2F9E44 => t("绿色", "Green"),
        _ => t("黑色", "Black"),
    }
}

fn ink(color: u32) -> Color {
    Color::rgb(color)
}

/// A label's type: `16px/1.2`, in the interface's own family.
const LABEL_TYPE: TypeStyle = TypeStyle { size: SKETCH_TEXT_SIZE, line: SKETCH_TEXT_SIZE * 1.2, weight: Weight::Regular, mono: false, italic: false, serif: false };

/// How wide the box a label is typed in is: `Math.max(4, length + 2)` of `ch`, and `min-width: 40px`.
pub fn draft_width(value: &str, ch: f32) -> f32 {
    ((value.chars().count() + 2).max(4) as f32 * ch).max(40.0)
}

#[derive(Clone)]
pub struct TextDraft {
    pub at: Point,
    pub value: String,
}

/// `useSketch` and the imperative half of `SketchCanvas`: one surface's state.
pub struct Sketch {
    pub tool: SketchTool,
    pub color: u32,
    pub history: SketchHistory,
    pub pending: Option<Stroke>,
    pub draft: Option<TextDraft>,
    /// The surface's size last frame, what a composite is measured against, and the device pixels to each of its own.
    pub size: (f32, f32),
    pub dpr: f32,
    /// How wide a `0` is in a label's type: the `ch` a label's box is measured in.
    ch: f32,
    pub export_ticket: u64,
    pub confirming: bool,
    compact: bool,
    full_width: f32,
}

impl Default for Sketch {
    fn default() -> Self {
        Self { tool: SketchTool::Pen, color: SKETCH_COLORS[0], history: SketchHistory::default(), pending: None, draft: None, size: (0.0, 0.0), dpr: 1.0, ch: LABEL_TYPE.size * 0.6, export_ticket: 0, confirming: false, compact: false, full_width: 0.0 }
    }
}

impl Sketch {
    /// An uncommitted text box is undoable even though it is not a stroke yet.
    pub fn state(&self) -> HistoryState {
        let state = self.history.state();
        if self.draft.as_ref().is_some_and(|d| !d.value.trim().is_empty()) { HistoryState { can_undo: true, can_redo: false, ..state } } else { state }
    }

    pub fn has_strokes(&self) -> bool {
        self.state().count > 0
    }

    pub fn commit_stroke(&mut self, stroke: Stroke) {
        let mut next = self.history.present.clone();
        next.push(stroke);
        self.history.push(next);
    }

    pub fn commit_text(&mut self) {
        let Some(draft) = self.draft.take() else { return };
        if draft.value.trim().is_empty() {
            return;
        }
        self.commit_stroke(Stroke::Text { at: draft.at, text: draft.value, size: SKETCH_TEXT_SIZE, color: self.color });
    }

    pub fn set_tool(&mut self, tool: SketchTool) {
        self.tool = tool;
        if tool != SketchTool::Text {
            self.commit_text();
        }
    }

    pub fn undo(&mut self) {
        if self.pending.is_none() {
            self.history.undo();
        }
    }

    pub fn redo(&mut self) {
        if self.pending.is_none() {
            self.history.redo();
        }
    }

    pub fn clear(&mut self) {
        if !self.history.present.is_empty() {
            self.history.push(Vec::new());
        }
        self.pending = None;
        self.draft = None;
    }

    pub fn press(&mut self, point: Point) {
        if self.tool == SketchTool::Text {
            self.commit_text();
            self.draft = Some(TextDraft { at: point, value: String::new() });
            return;
        }
        self.pending = Some(match self.tool {
            SketchTool::Pen => Stroke::Freehand { points: vec![point], color: self.color, width: SKETCH_STROKE_WIDTH },
            other => Stroke::Shape {
                kind: match other {
                    SketchTool::Line => Shape::Line,
                    SketchTool::Arrow => Shape::Arrow,
                    SketchTool::Rect => Shape::Rect,
                    _ => Shape::Ellipse,
                },
                from: point,
                to: point,
                color: self.color,
                width: SKETCH_STROKE_WIDTH,
            },
        });
    }

    pub fn drag(&mut self, point: Point, shift: bool) {
        match &mut self.pending {
            Some(Stroke::Freehand { points, .. }) => points.push(point),
            Some(Stroke::Shape { kind, from, to, .. }) => *to = constrain_point(Some(*kind), *from, point, shift),
            _ => {}
        }
    }

    pub fn release(&mut self) {
        let Some(pending) = self.pending.take() else { return };
        // A click with a shape tool draws nothing.
        if matches!(&pending, Stroke::Shape { from, to, .. } if from == to) {
            return;
        }
        self.commit_stroke(pending);
    }

    pub fn on_key(&mut self, k: &KeyPress) -> bool {
        let Key::Char(c) = k.key else { return false };
        let apple = is_apple();
        // `cmd` is Command on macOS and Control elsewhere, which is what "primary" means here.
        let Some(command) = sketch_command(c, k.alt, k.shift, if apple { k.ctrl } else { k.cmd }, if apple { k.cmd } else { false }, apple) else { return false };
        let state = self.state();
        match command {
            SketchCommand::Undo if state.can_undo => self.undo(),
            SketchCommand::Redo if state.can_redo => self.redo(),
            _ => {}
        }
        true
    }
}

fn paint_stroke(ui: &mut Ui<'_>, origin: (f32, f32), stroke: &Stroke) {
    let at = |p: &Point| (origin.0 + p.x, origin.1 + p.y);
    match stroke {
        Stroke::Freehand { points, color, width } => {
            let c = ink(*color);
            if points.len() == 1 {
                let (x, y) = at(&points[0]);
                ui.fill(rect(x - width / 2.0, y - width / 2.0, *width, *width), width / 2.0, c);
                return;
            }
            for pair in points.windows(2) {
                let ((x0, y0), (x1, y1)) = (at(&pair[0]), at(&pair[1]));
                ui.line(x0, y0, x1, y1, *width, c);
            }
        }
        Stroke::Shape { kind, from, to, color, width } => {
            let c = ink(*color);
            let ((x0, y0), (x1, y1)) = (at(from), at(to));
            match kind {
                Shape::Line => ui.line(x0, y0, x1, y1, *width, c),
                Shape::Rect => {
                    ui.line(x0, y0, x1, y0, *width, c);
                    ui.line(x1, y0, x1, y1, *width, c);
                    ui.line(x1, y1, x0, y1, *width, c);
                    ui.line(x0, y1, x0, y0, *width, c);
                }
                Shape::Ellipse => {
                    let (cx, cy, rx, ry) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0).abs() / 2.0, (y1 - y0).abs() / 2.0);
                    let segments = 64;
                    let point = |i: i32| {
                        let a = i as f32 / segments as f32 * std::f32::consts::TAU;
                        (cx + rx * a.cos(), cy + ry * a.sin())
                    };
                    for i in 0..segments {
                        let ((ax, ay), (bx, by)) = (point(i), point(i + 1));
                        ui.line(ax, ay, bx, by, *width, c);
                    }
                }
                Shape::Arrow => {
                    let (shaft, head) = arrow_geometry(*from, *to, *width);
                    let (sx, sy) = at(&shaft);
                    ui.line(x0, y0, sx, sy, *width, c);
                    // The head: lines from the tip to every point along its base fill the triangle.
                    let (tip, a, b) = (at(&head[0]), at(&head[1]), at(&head[2]));
                    for i in 0..=16 {
                        let k = i as f32 / 16.0;
                        ui.line(tip.0, tip.1, a.0 + (b.0 - a.0) * k, a.1 + (b.1 - a.1) * k, 1.5, c);
                    }
                }
            }
        }
        Stroke::Text { at: p, text, size, color } => {
            let ty = TypeStyle { size: *size, line: size * 1.2, ..LABEL_TYPE };
            for (i, line) in text.split('\n').enumerate() {
                let (x, y) = at(&Point { x: p.x, y: p.y + i as f32 * size * 1.2 });
                ui.label(x, y, line, &ty, ink(*color));
            }
        }
    }
}

fn tool_glyph(tool: SketchTool, color: Color) -> E {
    if tool == SketchTool::Pen {
        return icon("Pencil", 16.0).color(color);
    }
    // The five shape glyphs are the reference kit's own 16 px drawings.
    custom(move |_, ui, r| {
        let (x, y) = (r.x, r.y);
        let seg = |ui: &mut Ui<'_>, a: (f32, f32), b: (f32, f32)| ui.line(x + a.0, y + a.1, x + b.0, y + b.1, 1.0, color);
        match tool {
            SketchTool::Line => seg(ui, (3.5, 12.5), (12.5, 3.5)),
            SketchTool::Arrow => {
                seg(ui, (5.5, 3.5), (12.5, 3.5));
                seg(ui, (12.5, 3.5), (12.5, 10.5));
                seg(ui, (3.5, 12.5), (12.5, 3.5));
            }
            SketchTool::Rect => ui.card(rect(x + 3.5, y + 3.5, 9.0, 9.0), 1.0, Color::TRANSPARENT, color),
            SketchTool::Ellipse => ui.card(rect(x + 3.0, y + 3.0, 10.0, 10.0), 5.0, Color::TRANSPARENT, color),
            SketchTool::Text => {
                seg(ui, (3.5, 3.5), (12.5, 3.5));
                seg(ui, (8.0, 3.5), (8.0, 12.5));
            }
            SketchTool::Pen => {}
        }
    })
    .size(16.0, 16.0)
    .shrink0()
    .pass_through()
}

/// `.sketch-toolbar__swatch`: a 14 px disc with a ring outside it, 2 px of text colour on the one in use, and what a pointer over its
/// 20 px button makes of the others. Its box holds the ring as well, since a leaf draws only inside its box.
fn swatch(color: u32, active: bool) -> E {
    custom::<App>(move |_, ui, r| {
        let t_ = ui.theme;
        let disc = r.inset(2.0);
        let (spread, ring) = if active {
            (2.0, t_.text_primary)
        } else if ui.input.pointer.is_some_and(|(x, y)| disc.inset(-3.0).contains(x, y)) {
            (1.0, t_.text_secondary)
        } else {
            (1.0, t_.divider)
        };
        ui.fill(disc.inset(-spread), disc.w * 0.5 + spread, ring);
        ui.fill(disc, disc.w * 0.5, ink(color));
    })
    .size(18.0, 18.0)
    .shrink0()
    .pass_through()
}

pub struct SketchOverlayProps {
    pub key: String,
    /// The captured page, drawn under the strokes and used as the composite's resolution.
    pub backdrop: Option<Arc<str>>,
    pub on_cancel: Rc<dyn Fn(&mut App)>,
    pub on_attach: Rc<dyn Fn(&mut App, String)>,
    /// Label of the primary action; defaults to the browser pane's "Add to chat".
    pub attach_label: Option<String>,
}

thread_local! {
    static SKETCHES: RefCell<std::collections::HashMap<String, Rc<RefCell<Sketch>>>> = RefCell::new(Default::default());
}

/// One surface's state, by the overlay's key; it ends with `close_sketch`, as the surface did when dismissed.
pub fn sketch_of(key: &str) -> Rc<RefCell<Sketch>> {
    SKETCHES.with(|s| s.borrow_mut().entry(key.to_owned()).or_default().clone())
}

pub fn close_sketch(key: &str) {
    SKETCHES.with(|s| s.borrow_mut().remove(key));
}

/// Runs the export on a thread. Every export takes a ticket: cancelling invalidates outstanding tickets so a composite
/// that resolves after the surface was dismissed never reaches the caller.
fn attach(app: &mut App, key: &str, backdrop: Option<Arc<str>>, on_attach: Rc<dyn Fn(&mut App, String)>) {
    let sketch = sketch_of(key);
    let (strokes, size, dpr, ticket) = {
        let mut s = sketch.borrow_mut();
        s.commit_text();
        s.export_ticket += 1;
        (s.history.present.clone(), s.size, s.dpr, s.export_ticket)
    };
    let result: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let writer = result.clone();
    let key = key.to_owned();
    EXPORTS.with(|e| e.borrow_mut().push((key, ticket, on_attach, result)));
    app.spawn(move |task| {
        let url = match &backdrop {
            Some(backdrop) => raster::to_composite_data_url(&strokes, size, backdrop),
            None => raster::to_png_data_url(&strokes, size, Some(0xFFFFFF), dpr),
        };
        *writer.lock().unwrap_or_else(|p| p.into_inner()) = url;
        task.post(deliver_exports);
    });
}

type Waiting = (String, u64, Rc<dyn Fn(&mut App, String)>, std::sync::Arc<std::sync::Mutex<Option<String>>>);

thread_local! {
    static EXPORTS: RefCell<Vec<Waiting>> = const { RefCell::new(Vec::new()) };
}

fn deliver_exports(app: &mut App) {
    let ready: Vec<Waiting> = EXPORTS.with(|e| {
        let mut e = e.borrow_mut();
        let (ready, rest): (Vec<_>, Vec<_>) = e.drain(..).partition(|(_, _, _, r)| r.lock().unwrap_or_else(|p| p.into_inner()).is_some());
        *e = rest;
        ready
    });
    for (key, ticket, on_attach, result) in ready {
        let current = SKETCHES.with(|s| s.borrow().get(&key).map(|s| s.borrow().export_ticket));
        if current != Some(ticket) {
            continue;
        }
        if let Some(url) = result.lock().unwrap_or_else(|p| p.into_inner()).take() {
            on_attach(app, url);
        }
    }
}

/// The surface: backdrop, strokes, text being typed, and the toolbar pill. Sized by its parent (`position: absolute`,
/// `inset: 0` upstream), it keeps the keyboard's undo and redo while focus is inside it.
pub fn sketch_overlay(app: &App, props: SketchOverlayProps) -> E {
    let t_ = theme();
    let key = props.key.clone();
    let sketch = sketch_of(&key);
    let mut root = div().key(("sketch", key.as_str())).absolute().inset(0.0).focusable().skip_tab(true).aria_label(t("绘图画布", "Sketch canvas")).on_key({
        let sketch = sketch.clone();
        move |_: &mut App, k| sketch.borrow_mut().on_key(k)
    });
    root = match &props.backdrop {
        Some(source) => match crate::images::picture_for(app, source) {
            // `object-fit: cover` from the top left corner, which is where a composite measures its strokes from.
            crate::images::Picture::Ready { id, width, height } => root.child(custom::<App>(move |_, ui, r| ui.image_cover_at(id, r, (width, height), (0.0, 0.0), 0.0, 1.0)).absolute().inset(0.0).pass_through()),
            _ => root.child(div().absolute().inset(0.0).bg(t_.card_secondary).pass_through()),
        },
        None => root.child(div().absolute().inset(0.0).bg(t_.card_secondary).pass_through()),
    };
    let (paint_state, pointer_state) = (sketch.clone(), sketch.clone());
    let surface = custom::<App>(move |_, ui, r| {
        let mut s = paint_state.borrow_mut();
        s.size = (r.w, r.h);
        s.dpr = ui.scale;
        s.ch = ui.measure("0", &LABEL_TYPE);
        ui.with_clip(r, |ui| {
            for stroke in &s.history.present {
                paint_stroke(ui, (r.x, r.y), stroke);
            }
            if let Some(pending) = &s.pending {
                paint_stroke(ui, (r.x, r.y), pending);
            }
        });
    })
    .absolute()
    .inset(0.0)
    .pass_through();
    let surface_key = ("sketch-field", key.clone());
    // The strokes paint themselves; the pointer lands on the box around them, which also holds the focus the keys need.
    let surface = div().key(("sketch-canvas", key.as_str())).absolute().inset(0.0).focusable().skip_tab(true).role(Role::Application).aria_label(t("绘图画布", "Sketch canvas")).cursor(if sketch.borrow().tool == SketchTool::Text { CursorKind::Text } else { CursorKind::Crosshair }).child(surface).on_pointer({
        let surface_key = surface_key.clone();
        move |_: &mut App, ev| {
            let point = Point { x: ev.x - ev.rect.x, y: ev.y - ev.rect.y };
            let mut s = pointer_state.borrow_mut();
            match ev.phase {
                Phase::Down => {
                    s.press(point);
                    if s.draft.is_some() {
                        focus_request(("field", surface_key.clone()));
                    }
                }
                Phase::Move => s.drag(point, shift_held()),
                Phase::Up => s.release(),
            }
        }
    });
    root = root.child(surface);
    if let Some(draft) = sketch.borrow().draft.clone() {
        let (change, blur, keys, canvas) = (sketch.clone(), sketch.clone(), sketch.clone(), key.clone());
        let (width, color) = (draft_width(&draft.value, sketch.borrow().ch), sketch.borrow().color);
        // Transparent by design: the glyphs are the ink, and the caret sits on the page itself. `rows={1}`, so it shows one line however many it holds.
        let field_el = field(surface_key.clone(), draft.value.clone())
            .bare()
            .ty(LABEL_TYPE)
            .color(ink(color))
            .multiline(LABEL_TYPE.line, LABEL_TYPE.line)
            .controlled()
            .on_change(move |_: &mut App, v| {
                if let Some(d) = &mut change.borrow_mut().draft {
                    d.value = v;
                }
            })
            .on_blur(move |_: &mut App, _| blur.borrow_mut().commit_text())
            .on_key(move |_: &mut App, k| {
                match k.key {
                    Key::Escape => keys.borrow_mut().draft = None,
                    Key::Enter if k.cmd || k.ctrl => keys.borrow_mut().commit_text(),
                    _ => return false,
                }
                // Back to the surface, or the shortcuts would have nothing to arrive through.
                focus_request(("sketch-canvas", canvas.as_str()));
                true
            })
            .build()
            .aria_label(t("文字标注", "Text label"));
        root = root.child(div().absolute().left(draft.at.x).top(draft.at.y).w(width).child(field_el));
    }
    root.child(toolbar(&key, props.backdrop.clone(), props.on_cancel.clone(), props.on_attach.clone(), props.attach_label.clone()))
}

fn toolbar(key: &str, backdrop: Option<Arc<str>>, on_cancel: Rc<dyn Fn(&mut App)>, on_attach: Rc<dyn Fn(&mut App, String)>, attach_label: Option<String>) -> E {
    let t_ = theme();
    let sketch = sketch_of(key);
    let (current_tool, color, state, compact, confirming) = {
        let s = sketch.borrow();
        (s.tool, s.color, s.state(), s.compact, s.confirming)
    };
    let has = state.count > 0;
    let room = rect_of(("sketch-toolbar", key)).map_or(f32::MAX, |r| r.w);
    if !compact
        && let Some(pill) = rect_of(("sketch-pill", key))
    {
        sketch.borrow_mut().full_width = pill.w;
    }
    {
        let mut s = sketch.borrow_mut();
        s.compact = s.full_width > room;
    }
    let mut pill = row().key(("sketch-pill", key)).shrink0().items_center().gap(4.0).py(4.0).px(8.0).radius(10.0).bg(t_.flyout).border(t_.flyout_stroke).shadow(16.0, 6.0, t_.shadow);
    let divider = || div().w(1.0).h(16.0).shrink0().bg(t_.divider).role(Role::Splitter).aria_orientation(Orientation::Vertical);
    if compact {
        let tools = SKETCH_TOOLS.iter().map(|&candidate| {
            let s = sketch.clone();
            PopoverMenuItem::new(format!("{candidate:?}"), tool_label(candidate)).leading(tool_glyph(candidate, t_.text_secondary)).checked(candidate == current_tool).on(move |_| s.borrow_mut().set_tool(candidate))
        }).collect::<Vec<_>>();
        let colors = SKETCH_COLORS.iter().map(|&candidate| {
            let s = sketch.clone();
            PopoverMenuItem::new(format!("{candidate}"), color_label(candidate)).leading(swatch(candidate, false)).checked(candidate == color).on(move |_| s.borrow_mut().color = candidate)
        }).collect::<Vec<_>>();
        let menu_trigger = move |open: bool| row().size(28.0, 28.0).center().radius(6.0).when(open, |e| e.bg(t_.subtle_pressed)).hover_bg(t_.subtle_hover);
        pill = pill
            .child(PopoverMenu::new(("sketch-tools", key), t("绘图工具", "Drawing tool"), move |open| menu_trigger(open).child(tool_glyph(current_tool, if open { t_.text_primary } else { t_.text_secondary }))).title(tool_label(current_tool)).menu_label(t("绘图工具", "Drawing tool")).dense().sections(move || vec![PopoverMenuSection::new("tools").items(tools)]).build())
            .child(PopoverMenu::new(("sketch-colors", key), t("墨水颜色", "Ink color"), move |open| menu_trigger(open).child(swatch(color, true))).title(color_label(color)).menu_label(t("墨水颜色", "Ink color")).dense().sections(move || vec![PopoverMenuSection::new("colors").items(colors)]).build());
    } else {
        let mut tools = row().shrink0().items_center().gap(4.0).role(Role::Group).aria_label(t("绘图工具", "Drawing tool"));
        for candidate in SKETCH_TOOLS {
            let s = sketch.clone();
            tools = tools.child(tool_pressed(row().key(("sketch-tool", key, format!("{candidate:?}"))).size(28.0, 28.0).center().radius(6.0).shrink0().focusable().hover_bg(t_.subtle_hover).tooltip(tool_label(candidate)).on_click(move |_: &mut App| s.borrow_mut().set_tool(candidate)).child(tool_glyph(candidate, if candidate == current_tool { t_.text_primary } else { t_.text_secondary })), candidate == current_tool));
        }
        let mut colors = row().shrink0().items_center().gap(4.0).px(4.0).role(Role::Group).aria_label(t("墨水颜色", "Ink color"));
        for candidate in SKETCH_COLORS {
            let s = sketch.clone();
            colors = colors.child(row().key(("sketch-color", key, candidate)).size(20.0, 20.0).center().radius(10.0).shrink0().focusable().tooltip(color_label(candidate)).aria_pressed(candidate == color).on_click(move |_: &mut App| s.borrow_mut().color = candidate).child(swatch(candidate, candidate == color)));
        }
        pill = pill.child(tools).child(divider()).child(colors);
    }
    let apple = is_apple();
    let undo_label = if state.can_undo { t("撤销", "Undo") } else { t("暂无可撤销的内容", "Nothing to undo yet") };
    let redo_label = if state.can_redo { t("重做", "Redo") } else { t("暂无可重做的内容", "Nothing to redo yet") };
    let clear_label = if has { t("清除全部", "Clear all") } else { t("暂无可清除的内容", "Nothing to clear yet") };
    let (u, r, c) = (sketch.clone(), sketch.clone(), sketch.clone());
    pill = pill.child(divider()).child(
        row()
            .shrink0()
            .items_center()
            .gap(4.0)
            .child(tool(("sketch-undo", key), "Undo2", format!("{undo_label} · {}", shortcut_label(SketchCommand::Undo, apple)), 28.0, 15.0).focusable().disabled(!state.can_undo).on_click(move |_: &mut App| u.borrow_mut().undo()))
            .child(tool(("sketch-redo", key), "Redo2", format!("{redo_label} · {}", shortcut_label(SketchCommand::Redo, apple)), 28.0, 15.0).focusable().disabled(!state.can_redo).on_click(move |_: &mut App| r.borrow_mut().redo()))
            .child(tool(("sketch-clear", key), "Trash2", clear_label, 28.0, 15.0).focusable().disabled(!has).on_click(move |_: &mut App| c.borrow_mut().clear())),
    );
    let close_action = {
        let (s, cancel, key) = (sketch.clone(), on_cancel.clone(), key.to_owned());
        move |a: &mut App| {
            let dirty = s.borrow().has_strokes();
            if dirty {
                s.borrow_mut().confirming = true;
                return;
            }
            s.borrow_mut().export_ticket += 1;
            close_sketch(&key);
            cancel(a);
        }
    };
    pill = if compact {
        pill.child(tool(("sketch-close", key), "X", t("关闭", "Close"), 28.0, 15.0).on_click(close_action))
    } else {
        pill.child(subtle_button(t("关闭", "Close")).key(("sketch-close", key)).h(28.0).px(8.0).on_click(close_action))
    };
    let (attach_key, backdrop_for_attach) = (key.to_owned(), backdrop);
    pill = pill.child(accent_button(attach_label.unwrap_or_else(|| t("添加到对话", "Add to chat").to_owned())).key(("sketch-attach", key)).h(28.0).px(10.0).disabled(!has).on_click(move |a: &mut App| attach(a, &attach_key, backdrop_for_attach.clone(), on_attach.clone())));
    let mut bar = row().key(("sketch-toolbar", key)).absolute().left(10.0).right(10.0).bottom(10.0).justify_center().pass_through().raised().child(pill);
    if confirming {
        let (keep, discard, cancel, close_key) = (sketch.clone(), sketch, on_cancel, key.to_owned());
        let (dismiss, closing) = (keep.clone(), keep.clone());
        // `Dialog`: a one-line title with the way out in its corner, the body, and a row of buttons.
        let header = row()
            .items_center()
            .gap(12.0)
            .pt(9.0)
            .pb(8.0)
            .pl(16.0)
            .pr(10.0)
            .child(text(t("放弃这些标注？", "Discard your annotations?")).ty(BODY_SMALL_STRONG).ellipsis().flex1())
            .child(icon_button(Into::<IconRef>::into("X"), t("关闭", "Close")).key(("sketch-dialog-close", close_key.as_str())).on_click(move |_: &mut App| closing.borrow_mut().confirming = false));
        let copy = col().pt(4.0).pb(16.0).px(20.0).child(text(t("这张图片上还有未保存的标记，放弃后会一并删除。", "You have unsaved marks on this image. Discarding removes them.")).ty(CAPTION).color(t_.text_secondary));
        let footer = row()
            .justify_end()
            .items_center()
            .gap(8.0)
            .pt(6.0)
            .pb(12.0)
            .px(12.0)
            .child(button(t("继续编辑", "Keep editing")).key(("sketch-keep", close_key.as_str())).h(28.0).px(12.0).on_click(move |_: &mut App| keep.borrow_mut().confirming = false))
            .child(danger_button(t("放弃", "Discard")).key(("sketch-discard", close_key.as_str())).h(28.0).px(12.0).on_click(move |a: &mut App| {
                discard.borrow_mut().export_ticket += 1;
                close_sketch(&close_key);
                cancel(a);
            }));
        bar = bar.child(dialog(400.0, col().child(header).child(copy).child(footer), Some(Rc::new(move |_: &mut App| dismiss.borrow_mut().confirming = false))).aria_label(t("放弃这些标注？", "Discard your annotations?")));
    }
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn p(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn draw_freehand(s: &mut Sketch) {
        s.press(p(10.0, 10.0));
        s.drag(p(50.0, 60.0), false);
        s.release();
    }

    #[test]
    fn undoes_and_redoes_strokes() {
        let mut s = Sketch::default();
        draw_freehand(&mut s);
        draw_freehand(&mut s);
        assert_eq!(s.history.present.len(), 2);
        s.undo();
        assert_eq!(s.history.present.len(), 1);
        s.redo();
        assert_eq!(s.history.present.len(), 2);
        s.clear();
        assert!(s.history.present.is_empty());
    }

    #[test]
    fn keeps_the_primary_action_off_until_a_stroke_exists() {
        let mut s = Sketch::default();
        assert!(!s.has_strokes());
        assert!(!s.state().can_undo);
        draw_freehand(&mut s);
        assert!(s.has_strokes() && s.state().can_undo);
    }

    #[test]
    fn a_click_with_a_shape_tool_draws_nothing() {
        let mut s = Sketch::default();
        s.set_tool(SketchTool::Arrow);
        s.press(p(5.0, 5.0));
        s.release();
        assert!(!s.has_strokes());
        s.press(p(5.0, 5.0));
        s.drag(p(50.0, 5.0), false);
        s.release();
        assert!(s.has_strokes());
    }

    #[test]
    fn commits_a_text_label_and_drops_an_empty_one() {
        let mut s = Sketch::default();
        s.set_tool(SketchTool::Text);
        s.press(p(30.0, 30.0));
        s.commit_text();
        assert!(!s.has_strokes(), "nothing typed, nothing kept");
        s.press(p(30.0, 30.0));
        s.draft.as_mut().unwrap().value = "look here".to_owned();
        assert!(s.state().can_undo, "an uncommitted text box is undoable");
        s.commit_text();
        assert!(matches!(&s.history.present[0], Stroke::Text { text, .. } if text == "look here"));
    }

    #[test]
    fn leaving_the_text_tool_commits_what_was_typed() {
        let mut s = Sketch::default();
        s.set_tool(SketchTool::Text);
        s.press(p(1.0, 1.0));
        s.draft.as_mut().unwrap().value = "x".to_owned();
        s.set_tool(SketchTool::Pen);
        assert_eq!(s.history.present.len(), 1);
        assert!(s.draft.is_none());
    }

    #[test]
    fn holding_shift_constrains_a_shape_as_it_is_dragged() {
        let mut s = Sketch::default();
        s.set_tool(SketchTool::Rect);
        s.press(p(0.0, 0.0));
        s.drag(p(100.0, 40.0), true);
        s.release();
        assert!(matches!(&s.history.present[0], Stroke::Shape { to, .. } if *to == p(100.0, 100.0)));
    }

    #[test]
    fn measures_a_labels_box_in_ch_with_a_floor() {
        assert_eq!(draft_width("", 10.0), 40.0, "Math.max(4, 0 + 2) of ch");
        assert_eq!(draft_width("abcd", 10.0), 60.0, "two to spare");
        assert_eq!(draft_width("", 5.0), 40.0, "min-width: 40px");
    }

    #[test]
    fn the_shortcuts_undo_and_redo_from_inside_the_overlay() {
        let mut s = Sketch::default();
        draw_freehand(&mut s);
        let key = |shift| KeyPress { key: Key::Char('z'), shift, cmd: true, alt: false, ctrl: false };
        assert!(s.on_key(&key(false)));
        assert!(!s.has_strokes());
        assert!(s.on_key(&key(true)));
        assert!(s.has_strokes());
    }
}
