//! The readers a file gets (upstream's `FilePreview/`): `UnsupportedNotice`, `ImageViewer`, `CsvTable`, `NotebookView` and
//! `FontPreview`. The PDF, HTML and audio ones are in their own modules because they talk to the system.

use crate::code::{ansi_text, code_block};
use crate::format::*;
use crate::images::{picture_for, Picture};
use crate::markdown::{document_view, land_on_fragment, DocContext};
use crate::notebook::{parse_notebook, NotebookCell, NotebookOutput, OUTPUT_PREFERENCE};
use crate::viewers::fence_language;
use crate::widgets::{tool, tool_pressed, Action};
use mewrk_client::prelude::*;

pub fn notice(message: impl Into<Arc<str>>) -> E {
    text(message).ty(FINE).color(theme().text_secondary).px(10.0).py(6.0).shrink0()
}

pub fn error_notice(message: impl Into<Arc<str>>) -> E {
    text(message).ty(FINE).color(theme().critical).px(10.0).py(6.0).shrink0().role(Role::Alert)
}

/// What the pane says about a file it has no way to show, and the one thing it can still do with it.
pub fn unsupported_notice(key: impl std::hash::Hash, message: impl Into<Arc<str>>, on_reveal: Option<Action>) -> E {
    let t_ = theme();
    let mut body = col().flex1().min_h(0.0).p(24.0).center().gap(10.0).child(text(message).ty(FINE).color(t_.text_secondary).text_center().max_w(280.0));
    if let Some(reveal) = on_reveal {
        body = body.child(row().key(("reveal", key)).items_center().gap(6.0).h(26.0).px(10.0).radius(6.0).bg(t_.control_fill).border(t_.control_stroke).hover_bg(t_.control_fill_hover).on_click(move |a: &mut App| reveal(a)).child(icon("FolderOpen", 13.0).color(t_.text_primary)).child(text(t("在文件管理器中显示", "Show in file manager")).ty(FINE).color(t_.text_primary).nowrap()));
    }
    body
}

/// The pill of controls floating over a viewer.
pub fn toolbar_pill(label: &str, children: Vec<E>) -> E {
    let t_ = theme();
    let mut pill = row().items_center().gap(2.0).py(3.0).pl(3.0).pr(6.0).radius(8.0).bg(t_.flyout).border(t_.flyout_stroke).role(Role::Toolbar).aria_label(label);
    for c in children {
        pill = pill.child(c);
    }
    row().absolute().left(0.0).right(0.0).bottom(10.0).justify_center().pass_through().child(pill).raised()
}

const ZOOM_STEPS: [f32; 16] = [0.1, 0.25, 0.33, 0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0];

/// The next of a set of zoom stops from `current`.
pub fn next_zoom_step(steps: &[f32], current: f32, up: bool, slack: f32) -> f32 {
    if up {
        steps.iter().copied().find(|s| *s > current + slack).unwrap_or(steps[steps.len() - 1])
    } else {
        steps.iter().rev().copied().find(|s| *s < current - slack).unwrap_or(steps[0])
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Zoom {
    Fit,
    Scale(f32),
}

#[derive(Default, Clone, Copy)]
struct Pan {
    x: f32,
    y: f32,
}

/// One picture, shown whole by default and at any size on request. `Fit` is the reading most pictures want: the whole
/// thing, never enlarged past its own pixels. The checkerboard makes transparency visible.
pub fn image_viewer(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>, on_reveal: Option<Action>) -> E {
    let t_ = theme();
    let picture = picture_for(app, source);
    let (id, nw, nh) = match picture {
        Picture::Pending => return notice(t("正在读取…", "Loading…")),
        Picture::Failed => return unsupported_notice(key, t("当前界面引擎无法解码这种图片格式。", "This window's engine cannot decode this image format."), on_reveal),
        Picture::Ready { id, width, height } => (id, width, height),
    };
    let _ = name;
    let zoom = use_state(("image-zoom", key), || Zoom::Fit);
    let pan = use_state(("image-pan", key), Pan::default);
    let stage_key = ("image-stage", key.to_owned());
    let stage = rect_of(stage_key.clone()).unwrap_or(rect(0.0, 0.0, nw as f32, nh as f32));
    let fit_scale = ((stage.w - 32.0) / nw.max(1) as f32).min((stage.h - 68.0) / nh.max(1) as f32).clamp(0.01, 1.0);
    let effective = match zoom.get() {
        Zoom::Fit => fit_scale,
        Zoom::Scale(s) => s,
    };
    let (z, p) = (zoom.clone(), pan.clone());
    let surface = custom::<App>(move |_app, ui, r| {
        let theme_now = ui.theme;
        let scale_now = match z.get() {
            Zoom::Fit => ((r.w - 32.0) / nw.max(1) as f32).min((r.h - 68.0) / nh.max(1) as f32).clamp(0.01, 1.0),
            Zoom::Scale(s) => s,
        };
        let (w, h) = (nw as f32 * scale_now, nh as f32 * scale_now);
        let mut offset = p.get();
        // A pinch arrives as a wheel with the command or control key held; a plain wheel scrolls.
        if ui.pointer_in(&r) && ui.input.scroll != 0.0 {
            if ui.input.zoom_modifier {
                let delta = if ui.input.scroll_is_pixels { -ui.input.scroll } else { -ui.input.scroll * 40.0 };
                z.set(Zoom::Scale((scale_now * (-delta / 300.0).exp()).clamp(0.05, 16.0)));
            } else if !matches!(z.get(), Zoom::Fit) {
                offset.y += if ui.input.scroll_is_pixels { ui.input.scroll } else { ui.input.scroll * 40.0 };
            }
        }
        if ui.pointer_in(&r) && ui.input.scroll_x != 0.0 && !matches!(z.get(), Zoom::Fit) {
            offset.x += if ui.input.scroll_is_pixels { ui.input.scroll_x } else { ui.input.scroll_x * 40.0 };
        }
        if resp_pan(ui, r, &mut offset) {
            ui.animating = true;
        }
        let (max_x, max_y) = (((w + 32.0 - r.w) * 0.5).max(0.0), ((h + 68.0 - r.h) * 0.5).max(0.0));
        offset.x = offset.x.clamp(-max_x, max_x);
        offset.y = offset.y.clamp(-max_y, max_y);
        p.set(offset);
        // The picture sits in the middle while it is smaller than the pane, and pans from there once it is larger.
        let at = rect(r.x + (r.w - w) * 0.5 + offset.x, r.y + (r.h - 36.0 - h) * 0.5 + offset.y, w, h);
        ui.with_clip(r, |ui| {
            let visible = at.intersect(&r);
            ui.fill(at, 0.0, Color::rgb(0xF7F7F4));
            let square = 8.0;
            let (x0, y0) = (((visible.x - at.x) / square).floor() as i32, ((visible.y - at.y) / square).floor() as i32);
            let (x1, y1) = (((visible.right() - at.x) / square).ceil() as i32, ((visible.bottom() - at.y) / square).ceil() as i32);
            for yy in y0..y1 {
                for xx in x0..x1 {
                    if (xx + yy) % 2 != 0 {
                        ui.fill(rect(at.x + xx as f32 * square, at.y + yy as f32 * square, square, square).intersect(&at), 0.0, Color::rgb(0xE5E5E0));
                    }
                }
            }
            ui.image(id, at, 0.0, 1.0);
            ui.card(at, 0.0, Color::TRANSPARENT, theme_now.divider);
        });
    })
    .key(stage_key)
    .flex1()
    .min_h(0.0)
    .min_w(0.0)
    .role(Role::Image)
    .aria_label(name);
    let set = |value: Zoom| {
        let z = zoom.clone();
        let p = pan.clone();
        move |_: &mut App| {
            z.set(value);
            p.set(Pan::default());
        }
    };
    let step = |up: bool| {
        let z = zoom.clone();
        move |_: &mut App| z.set(Zoom::Scale(next_zoom_step(&ZOOM_STEPS, effective, up, 1e-6)))
    };
    let is = zoom.get();
    let label = match is {
        Zoom::Fit => t("适应", "Fit").to_owned(),
        Zoom::Scale(s) => format!("{}%", (s * 100.0).round() as i32),
    };
    let mut meta = format!("{nw} × {nh}");
    if let Some(b) = bytes {
        meta.push_str(" · ");
        meta.push_str(&format_bytes(b));
    }
    col().flex1().min_h(0.0).min_w(0.0).clip().child(surface).child(toolbar_pill(t("图片缩放", "Image zoom"), vec![
        tool(("zoom-out", key), "Minus", t("缩小", "Zoom out"), 24.0, 13.0).on_click(step(false)),
        row().key(("zoom-label", key)).center().min_w(44.0).h(24.0).px(4.0).radius(5.0).hover_bg(t_.subtle_hover).tooltip(t("适应窗口", "Fit to pane")).on_click(set(Zoom::Fit)).child(text(label).ty(FINE).color(t_.text_primary).nowrap()),
        tool(("zoom-in", key), "Plus", t("放大", "Zoom in"), 24.0, 13.0).on_click(step(true)),
        tool_pressed(tool(("zoom-actual", key), "Scan", t("实际大小", "Actual size"), 24.0, 13.0), is == Zoom::Scale(1.0)).on_click(set(Zoom::Scale(1.0))),
        tool_pressed(tool(("zoom-fit", key), "Maximize", t("适应窗口", "Fit to pane"), 24.0, 13.0), is == Zoom::Fit).on_click(set(Zoom::Fit)),
        text(meta).ty(FINE).color(t_.text_tertiary).nowrap().pl(6.0),
    ]))
}

/// Drags the picture around while the pointer is held on it; there is no horizontal wheel to scroll it with.
fn resp_pan(ui: &mut Ui<'_>, r: Rect, offset: &mut Pan) -> bool {
    thread_local! {
        static LAST: std::cell::Cell<Option<(f32, f32)>> = const { std::cell::Cell::new(None) };
    }
    if ui.input.down
        && ui.pointer_in(&r)
        && let Some((x, y)) = ui.input.pointer
    {
        if let Some((lx, ly)) = LAST.with(|l| l.get()) {
            offset.x += x - lx;
            offset.y += y - ly;
        }
        LAST.with(|l| l.set(Some((x, y))));
        return true;
    }
    LAST.with(|l| l.set(None));
    false
}

/// Rows drawn at once; a table longer than this is a job for a spreadsheet, not a pane.
const MAX_ROWS: usize = 5000;

pub fn delimiter_for(path: &str, text: &str) -> char {
    match path.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "tsv" | "tab" => '\t',
        "psv" => '|',
        _ => guess_delimiter(text),
    }
}

struct CsvData {
    rows: Rc<Vec<Vec<String>>>,
    truncated: bool,
}

/// Delimited text as a table: a numbered row per record, the first record as the header by default, and numbers
/// right-aligned so a column of them can be read down.
pub fn csv_table(key: &str, path: &str, content: &str) -> E {
    let t_ = theme();
    let header = use_state(("csv-header", key), || true);
    let hash = fluent::text::hash_str(content);
    let data = use_state(("csv-data", key, hash), || {
        let table = parse_delimited(content, delimiter_for(path, content), MAX_ROWS + 1);
        Rc::new(CsvData { rows: Rc::new(table.rows), truncated: table.truncated })
    }).get();
    let has_header = header.get();
    let take = (MAX_ROWS + usize::from(has_header)).min(data.rows.len());
    let rows = &data.rows[..take];
    let truncated = data.truncated || data.rows.len() > rows.len();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let numeric = numeric_columns(rows, usize::from(has_header));
    let body_count = rows.len() - usize::from(has_header && !rows.is_empty());
    let mut meta = tf("{rows} 行 · {columns} 列", "{rows} rows · {columns} columns", &[("rows", &body_count), ("columns", &width)]);
    if truncated {
        meta.push_str(" · ");
        meta.push_str(&tf("只显示了前 {count} 行", "only the first {count} rows are shown", &[("count", &MAX_ROWS)]));
    }
    let toggle = header.clone();
    let bar = row().items_center().gap(8.0).px(10.0).py(5.0).shrink0().border_bottom(t_.divider).child(row().items_center().gap(6.0).child(checkbox(has_header, move |_: &mut App| toggle.update(|h| *h = !*h))).child(text(t("首行是表头", "First row is the header")).ty(FINE).color(t_.text_secondary).nowrap())).child(text(meta).ty(FINE).color(t_.text_tertiary).ellipsis().flex1());

    // A column is as wide as its widest cell would like, within limits, on an estimate: laying out thousands of rows of
    // text every frame to learn that exactly is what a virtual list is here to avoid.
    let digits = (body_count.max(1)).to_string().len() as f32;
    let index_w = 12.0 + digits * 7.0;
    let mut widths = vec![60.0f32; width];
    for row_cells in rows.iter().take(200) {
        for (c, cell) in row_cells.iter().enumerate() {
            widths[c] = widths[c].max((cell.chars().take(60).map(|ch| if fluent::text::is_cjk(ch) { 12.0 } else { 7.2 }).sum::<f32>() + 20.0).min(320.0));
        }
    }
    let columns_w: f32 = widths.iter().sum();
    let offset = use_state(("csv-x", key), || 0.0f32);
    let viewport_key = ("csv-viewport", key.to_owned());
    let room = rect_of(viewport_key.clone()).map_or(0.0, |r| r.w - index_w);
    let most = (columns_w - room).max(0.0);
    let x = offset.get().clamp(0.0, most);
    // The numbers stay at the left edge and the header at the top, as `position: sticky` keeps them: each line clips its own
    // cells and slides them under the numbers.
    let cell_row = |cells: Option<&Vec<String>>, index: Option<usize>, head: bool, row_key: Option<usize>| -> E {
        let ty = if head { CAPTION_STRONG } else { CAPTION };
        let mut line = row().h(24.0).shrink0().min_w(0.0).border_bottom(t_.divider);
        if let Some(i) = row_key {
            line = line.key(("csv-row", key, i)).hover_bg(t_.subtle_hover);
        }
        line = line.child(row().w(index_w).h(24.0).items_center().justify_end().px(6.0).shrink0().bg(t_.card_secondary).child(text(index.map_or(String::new(), |i| (i + 1).to_string())).ty(TypeStyle { mono: true, ..FINE }).color(t_.text_tertiary).nowrap()));
        let mut slide = row().h(24.0).w(columns_w).shrink0().ml(-x);
        if head {
            slide = slide.bg(t_.card_secondary);
        }
        for (c, w) in widths.iter().enumerate() {
            let value = cells.and_then(|r| r.get(c)).map_or("", String::as_str).replace('\n', " ");
            let mut cell = text(value).ty(ty).color(t_.text_primary).ellipsis().selectable();
            if numeric.get(c).copied().unwrap_or(false) {
                cell = cell.text_end();
            }
            slide = slide.child(row().w(*w).h(24.0).items_center().px(8.0).shrink0().border_right(t_.divider).child(cell.flex1()));
        }
        line.child(div().flex1().min_w(0.0).h(24.0).clip().child(slide))
    };
    let body: &[Vec<String>] = if has_header && !rows.is_empty() { &rows[1..] } else { rows };
    let scroller = ("csv-scroll", key.to_owned());
    let range = visible_rows(scroller.clone(), body.len(), 24.0, 8);
    let mut list = div().key(scroller).flex1().min_h(0.0).min_w(0.0).scroll_y();
    list = list.child(div().h(range.start as f32 * 24.0).shrink0());
    for i in range.clone() {
        list = list.child(cell_row(body.get(i), Some(i), false, Some(i)));
    }
    list = list.child(div().h((body.len() - range.end) as f32 * 24.0).shrink0());
    let head = (has_header && !rows.is_empty()).then(|| cell_row(rows.first(), None, true, None));
    // The sideways wheel, read while painting as anything from `Input` is.
    let wheel = offset.clone();
    let reader = custom::<App>(move |_app, ui, r| {
        if ui.pointer_in(&r) && ui.input.scroll_x != 0.0 {
            let delta = if ui.input.scroll_is_pixels { ui.input.scroll_x } else { ui.input.scroll_x * 40.0 };
            wheel.set((wheel.get() - delta).clamp(0.0, most));
        }
    }).absolute().inset(0.0).pass_through();
    let mut table = col().key(viewport_key).flex1().min_h(0.0).min_w(0.0).child(head).child(list).child(reader);
    if most > 0.0 {
        table = table.child(csv_scrollbar(key, &offset, x, most, room, columns_w));
    }
    col().flex1().min_h(0.0).min_w(0.0).clip().child(bar).child(table)
}

/// The sideways scrollbar of a table wider than the pane. There is one thumb to drag, and a press off it jumps there.
fn csv_scrollbar(key: &str, offset: &Local<f32>, x: f32, most: f32, room: f32, columns_w: f32) -> E {
    let thumb_share = (room / columns_w).clamp(0.05, 1.0);
    let offset = offset.clone();
    let track = custom::<App>(move |_app, ui, r| {
        let w = r.w * thumb_share;
        ui.fill(rect(r.x + (r.w - w) * (x / most.max(1.0)), r.y + 2.0, w, r.h - 4.0), 2.0, ui.theme.text_tertiary.with_alpha(0.45));
    });
    div().key(("csv-hbar", key)).h(10.0).shrink0().child(track.w_full().h_full().pass_through()).on_pointer(move |_: &mut App, ev| {
        if ev.phase != Phase::Up {
            let share = (ev.x - ev.rect.x - ev.rect.w * thumb_share * 0.5) / (ev.rect.w * (1.0 - thumb_share)).max(1.0);
            offset.set(share.clamp(0.0, 1.0) * most);
        }
    })
}

fn prompt(count: Option<f64>) -> String {
    count.map_or_else(|| "[ ]".to_owned(), |c| format!("[{}]", c as i64))
}

/// A Jupyter notebook, read the way Jupyter shows it: Markdown cells rendered, code cells coloured with their execution
/// counts, and each output in the richest form it carries. Nothing runs.
pub fn notebook_view(app: &App, key: &str, content: &str, ctx: &DocContext) -> E {
    notebook_view_landing(app, key, content, ctx, None).0
}

/// The same, landing on what a link's `fragment` names in whichever text cell holds it. Answers whether it did.
pub fn notebook_view_landing(app: &App, key: &str, content: &str, ctx: &DocContext, fragment: Option<&str>) -> (E, bool) {
    let t_ = theme();
    let notebook = match parse_notebook(content) {
        Ok(n) => n,
        Err(reason) => return (error_notice(tf("这个笔记本不是有效的 JSON：{reason}", "This notebook is not valid JSON: {reason}", &[("reason", &reason)])), false),
    };
    let language = fence_language(Some(&notebook.language)).unwrap_or("python");
    let mut landed = false;
    let mut out = col().key(ctx.scroller.as_str()).flex1().min_h(0.0).min_w(0.0).scroll_y().gap(10.0).pt(8.0).pr(12.0).pb(28.0).pl(4.0);
    if notebook.cells.is_empty() {
        out = out.child(notice(t("这个笔记本没有单元格。", "This notebook has no cells.")));
    }
    let prompt_ty = TypeStyle { mono: true, ..FINE };
    let gutter = |label: String, color: Color| row().w(PROMPT_W).shrink0().justify_end().pt(6.0).child(text(label).ty(prompt_ty).color(color).nowrap());
    for (i, cell) in notebook.cells.iter().enumerate() {
        let cell_key = format!("{key}/cell{i}");
        match cell {
            NotebookCell::Markdown { source, attachments } => {
                // `attachment:name` is a picture pasted into the cell and stored with it.
                let attachments = attachments.clone();
                let resolve = ctx.resolve_image.clone();
                let cell_ctx = DocContext {
                    resolve_image: Rc::new(move |source: &str| {
                        if let Some(name) = source.strip_prefix("attachment:") {
                            let bundle = attachments.get(name)?;
                            let (kind, data) = bundle.iter().find(|(k, _)| k.starts_with("image/"))?;
                            return Some(Arc::from(format!("data:{kind};base64,{}", data.split_whitespace().collect::<String>())));
                        }
                        resolve(source)
                    }),
                    linkify_paths: true,
                    ..ctx.clone()
                };
                if let Some(fragment) = fragment {
                    landed |= land_on_fragment(&cell_ctx, cell_key.as_str(), fragment);
                }
                out = out.child(row().items_start().gap(6.0).min_w(0.0).shrink0().child(div().w(PROMPT_W).shrink0()).child(document_view(app, &cell_ctx, cell_key.as_str(), source).flex1()));
            }
            NotebookCell::Raw { source } => {
                out = out.child(row().items_start().gap(6.0).min_w(0.0).shrink0().child(div().w(PROMPT_W).shrink0()).child(text(source.clone()).ty(MONO_SMALL).color(t_.text_secondary).selectable().flex1()));
            }
            NotebookCell::Code { source, execution_count, outputs } => {
                let mut group = col().gap(4.0).min_w(0.0).shrink0();
                group = group.child(row().items_start().gap(6.0).min_w(0.0).child(gutter(format!("In {}:", prompt(*execution_count)), t_.accent_text)).child(div().flex1().min_w(0.0).child(code_block(cell_key.as_str(), source, Some(language)))));
                for (n, output) in outputs.iter().enumerate() {
                    let label = match output {
                        NotebookOutput::Data { execution_count, result: true, .. } => format!("Out {}:", prompt(*execution_count)),
                        _ => String::new(),
                    };
                    group = group.child(row().items_start().gap(6.0).min_w(0.0).child(gutter(label, t_.critical)).child(div().flex1().min_w(0.0).child(output_view(app, ctx, &format!("{cell_key}/out{n}"), output))));
                }
                out = out.child(group);
            }
        }
    }
    (out, landed)
}

/// `.notebook__prompt`'s width.
const PROMPT_W: f32 = 64.0;

fn output_view(app: &App, ctx: &DocContext, key: &str, output: &NotebookOutput) -> E {
    let t_ = theme();
    match output {
        NotebookOutput::Stream { name, text: body } => ansi_text(key, body, if name == "stderr" { t_.critical } else { t_.text_primary }),
        NotebookOutput::Error { name, value, traceback } => {
            let body = if traceback.is_empty() { format!("{name}: {value}") } else { traceback.clone() };
            ansi_text(key, &body, t_.critical)
        }
        NotebookOutput::Data { data, .. } => {
            let kind = OUTPUT_PREFERENCE.iter().find(|k| data.contains_key(**k)).map(|k| k.to_string()).or_else(|| data.keys().next().cloned());
            let Some(kind) = kind else { return div() };
            let value = &data[&kind];
            if kind.starts_with("image/") {
                let source: Arc<str> = if kind == "image/svg+xml" { Arc::from(text_data_url(value, &kind)) } else { Arc::from(format!("data:{kind};base64,{}", value.split_whitespace().collect::<String>())) };
                return match picture_for(app, &source) {
                    Picture::Ready { id, width, height } => {
                        let room = rect_of(("doc-out", key)).map_or(width as f32, |r| r.w);
                        let k = (room / width.max(1) as f32).min(1.0);
                        div().key(("doc-out", key)).child(image(id, (width, height)).size(width as f32 * k, height as f32 * k))
                    }
                    _ => div().key(("doc-out", key)).h(20.0),
                };
            }
            // Outputs carry their own pictures; a notebook's HTML never needs the workspace, and has no page around it.
            if kind == "text/html" {
                let inner = DocContext { resolve_image: Rc::new(|s: &str| s.starts_with("data:").then(|| Arc::from(s))), linkify_paths: false, ..ctx.clone() };
                return crate::markdown::document_view_as(app, &inner, key, value, TypeStyle { size: 13.0, line: 19.0, weight: Weight::Regular, mono: false, italic: false, serif: false });
            }
            if kind == "text/markdown" || kind == "text/latex" {
                let inner = DocContext { linkify_paths: false, ..ctx.clone() };
                return document_view(app, &inner, key, value);
            }
            ansi_text(key, value, t_.text_primary)
        }
    }
}

// ---------------------------------------------------------------- fonts

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontNames {
    pub family: Option<String>,
    pub subfamily: Option<String>,
    pub full_name: Option<String>,
    pub version: Option<String>,
    pub designer: Option<String>,
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn be32(b: &[u8], at: usize) -> Option<usize> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize)
}

/// The names an OpenType or TrueType font gives itself, from its `name` table. A malformed table says nothing.
pub fn read_font_names(bytes: &[u8]) -> Option<FontNames> {
    let tag = be32(bytes, 0)?;
    if tag != 0x0001_0000 && tag != 0x4f54_544f && tag != 0x7472_7565 {
        return None;
    }
    let tables = be16(bytes, 4)?;
    let mut name_offset = None;
    for i in 0..tables {
        let record = 12 + i * 16;
        if be32(bytes, record)? == 0x6e61_6d65 {
            name_offset = Some(be32(bytes, record + 8)?);
        }
    }
    let name_offset = name_offset?;
    let count = be16(bytes, name_offset + 2)?;
    let storage = name_offset + be16(bytes, name_offset + 4)?;
    let mut found: [Option<(String, u8)>; 10] = Default::default();
    for i in 0..count {
        let record = name_offset + 6 + i * 12;
        let (Some(platform), Some(encoding), Some(language), Some(name_id), Some(length), Some(offset)) = (be16(bytes, record), be16(bytes, record + 2), be16(bytes, record + 4), be16(bytes, record + 6), be16(bytes, record + 8), be16(bytes, record + 10)) else { break };
        let offset = storage + offset;
        if ![1, 2, 4, 5, 9].contains(&name_id) || offset + length > bytes.len() {
            continue;
        }
        let raw = &bytes[offset..offset + length];
        let text = if platform == 3 || platform == 0 {
            String::from_utf16_lossy(&raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect::<Vec<_>>())
        } else if platform == 1 && encoding == 0 {
            raw.iter().map(|b| *b as char).collect()
        } else {
            continue;
        };
        // English (US) Windows names first, then any Unicode name, then whatever Mac Roman has.
        let score = if platform == 3 && language == 0x409 { 3 } else if platform == 3 || platform == 0 { 2 } else { 1 };
        if found[name_id].as_ref().is_none_or(|(_, s)| *s < score) {
            found[name_id] = Some((text.trim().to_owned(), score));
        }
    }
    let take = |i: usize| found[i].as_ref().map(|(t, _)| t.clone()).filter(|t| !t.is_empty());
    Some(FontNames { family: take(1), subfamily: take(2), full_name: take(4), version: take(5), designer: take(9) })
}

/// Bytes a rasterizer can read: an sfnt as it is, a WOFF unpacked, the first font of a collection by index.
pub fn sfnt_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.get(..4)? {
        b"wOFF" => woff_to_sfnt(bytes),
        b"wOF2" => wuff::decompress_woff2(bytes).ok(),
        _ => Some(bytes.to_vec()),
    }
}

fn woff_to_sfnt(b: &[u8]) -> Option<Vec<u8>> {
    let flavor = b.get(4..8)?;
    let tables = be16(b, 12)?;
    let mut entries = Vec::with_capacity(tables);
    for i in 0..tables {
        let r = 44 + i * 20;
        let tag = b.get(r..r + 4)?.to_vec();
        let (offset, comp, orig, sum) = (be32(b, r + 4)?, be32(b, r + 8)?, be32(b, r + 12)?, b.get(r + 16..r + 20)?.to_vec());
        let raw = b.get(offset..offset + comp)?;
        let data = if comp == orig { raw.to_vec() } else { miniz_oxide::inflate::decompress_to_vec_zlib(raw).ok()? };
        entries.push((tag, sum, data));
    }
    let mut out = Vec::new();
    out.extend_from_slice(flavor);
    out.extend_from_slice(&(tables as u16).to_be_bytes());
    let log = (tables.max(1) as f32).log2().floor() as u32;
    let range = (1u16 << log) * 16;
    out.extend_from_slice(&range.to_be_bytes());
    out.extend_from_slice(&(log as u16).to_be_bytes());
    out.extend_from_slice(&((tables as u16 * 16).saturating_sub(range)).to_be_bytes());
    let mut at = 12 + tables * 16;
    let mut body = Vec::new();
    for (tag, sum, data) in &entries {
        out.extend_from_slice(tag);
        out.extend_from_slice(sum);
        out.extend_from_slice(&(at as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        body.extend_from_slice(data);
        let pad = (4 - data.len() % 4) % 4;
        body.extend(std::iter::repeat_n(0u8, pad));
        at += data.len() + pad;
    }
    out.extend_from_slice(&body);
    Some(out)
}

/// One line of text in a font of the file's own, as an alpha mask. Glyphs are placed by their advances: enough to judge a
/// typeface by, without shaping the sample like a document.
pub fn rasterize_sample(font: &[u8], sample: &str, px: f32) -> Option<(Vec<u8>, u32, u32)> {
    use swash::scale::{image::Content, Render, ScaleContext, Source};
    use swash::zeno::Format;
    use swash::FontRef;
    let face = FontRef::from_index(font, 0)?;
    let charmap = face.charmap();
    let metrics = face.metrics(&[]).scale(px);
    let glyph_metrics = face.glyph_metrics(&[]).scale(px);
    let (ascent, descent) = (metrics.ascent.ceil(), metrics.descent.ceil());
    let height = (ascent + descent).max(1.0) as u32;
    let width = sample.chars().map(|c| glyph_metrics.advance_width(charmap.map(c))).sum::<f32>().ceil().clamp(1.0, 8192.0) as u32;
    let mut mask = vec![0u8; (width * height) as usize];
    let mut context = ScaleContext::new();
    let mut scaler = context.builder(face).size(px).build();
    let mut pen = 0.0f32;
    for c in sample.chars() {
        let glyph = charmap.map(c);
        if glyph != 0 && let Some(image) = Render::new(&[Source::Outline]).format(Format::Alpha).render(&mut scaler, glyph)
            && image.content == Content::Mask
        {
            let (gx, gy) = (pen.round() as i32 + image.placement.left, ascent as i32 - image.placement.top);
            for y in 0..image.placement.height as i32 {
                for x in 0..image.placement.width as i32 {
                    let (tx, ty) = (gx + x, gy + y);
                    if tx >= 0 && ty >= 0 && (tx as u32) < width && (ty as u32) < height {
                        let v = image.data[(y as u32 * image.placement.width + x as u32) as usize];
                        let slot = &mut mask[(ty as u32 * width + tx as u32) as usize];
                        *slot = (*slot).max(v);
                    }
                }
            }
        }
        pen += glyph_metrics.advance_width(glyph);
    }
    Some((mask, width, height))
}

const SIZES: [f32; 6] = [12.0, 16.0, 20.0, 28.0, 40.0, 56.0];
const GLYPH_ROW_SIZE: f32 = 22.0;
const GLYPH_ROWS: [&str; 4] = ["ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz", "0123456789 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~", "ÀÉÎÕÜ àéîõü ßæøå ΑΒΓΔ αβγδ АБВГ абвг"];
const PANGRAM: &str = "The quick brown fox jumps over the lazy dog 敏捷的棕色狐狸跳过了懒狗";
const CJK_ROW: &str = "永和九年，岁在癸丑，暮春之初。あいうえお アイウエオ 한국어";

struct LoadedFont {
    bytes: Rc<Vec<u8>>,
    names: Option<FontNames>,
}

/// Pieces of a sample by whether the font has them. Spaces go with what surrounds them, so a word gap is never a piece of its
/// own in the system's face.
pub fn font_runs(font: &[u8], sample: &str) -> Vec<(bool, String)> {
    let Some(face) = swash::FontRef::from_index(font, 0) else { return vec![(false, sample.to_owned())] };
    let charmap = face.charmap();
    let mut runs: Vec<(bool, String)> = Vec::new();
    for c in sample.chars() {
        let has = charmap.map(c) != 0;
        match runs.last_mut() {
            Some((own, run)) if *own == has || c.is_whitespace() => run.push(c),
            _ => runs.push((has, c.to_string())),
        }
    }
    runs
}

/// A line of the file's own face: rasterized once per text and size, and given back when it scrolls out of asking. What the
/// font has no glyph for is set in the system's, as `"family", system-ui` falls through upstream: a sample that shows holes
/// says nothing about the face.
fn font_line(key: &str, font: &Rc<Vec<u8>>, sample: &str, size: f32) -> E {
    let runs = use_state(("font-runs", key, fluent::text::hash_str(sample)), {
        let (font, sample) = (font.clone(), sample.to_owned());
        move || Rc::new(font_runs(&font, &sample))
    }).get();
    let mut line = row().items_end().min_w(0.0).shrink0();
    for (index, (own, run)) in runs.iter().enumerate() {
        line = line.child(if *own {
            font_run(("font-run", key, index), font, run, size)
        } else {
            text(run.clone()).ty(TypeStyle { size, line: size * 1.2, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(theme().text_primary).nowrap().shrink0()
        });
    }
    line
}

fn font_run(key: impl std::hash::Hash, font: &Rc<Vec<u8>>, sample: &str, size: f32) -> E {
    let (font_for_state, text_owned) = (font.clone(), sample.to_owned());
    let hash = fluent::text::hash_str(sample);
    let scale_hint = 2.0;
    let image_state = use_state(("font-line", key, size.to_bits(), hash), move || FontLine::make(&font_for_state, &text_owned, size * scale_hint));
    let FontLine { id, w, h, .. } = *image_state.borrow();
    let (lw, lh) = (w as f32 / scale_hint, (h as f32 / scale_hint).max(size));
    match id {
        Some(id) => image(id, (w, h)).size(lw, lh).shrink0(),
        None => text(sample.to_owned()).ty(TypeStyle { size, line: size * 1.2, weight: Weight::Regular, mono: false, italic: false, serif: false }).color(theme().text_tertiary).nowrap(),
    }
}

struct FontLine {
    id: Option<fluent::ImageId>,
    w: u32,
    h: u32,
}

impl FontLine {
    fn make(font: &[u8], sample: &str, px: f32) -> Self {
        let dark = theme().dark;
        let ink: u8 = if dark { 0xF0 } else { 0x20 };
        match rasterize_sample(font, sample, px) {
            Some((mask, w, h)) => {
                let rgba: Vec<u8> = mask.iter().flat_map(|a| [ink, ink, ink, *a]).collect();
                Self { id: Some(fluent::queue_image(rgba, w, h)), w, h }
            }
            None => Self { id: None, w: 0, h: 0 },
        }
    }
}

impl Drop for FontLine {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            fluent::release_image(id);
        }
    }
}

type FontSlot = std::sync::Arc<std::sync::Mutex<Option<Option<Vec<u8>>>>>;

/// What a font preview has of its file so far.
enum FontLoad {
    /// Being unpacked on a task thread: a WOFF2 of a CJK face is seconds of Brotli and outline rebuilding in a debug build.
    Reading(FontSlot),
    Done(Option<Rc<LoadedFont>>),
}

/// A font, shown at the sizes a reader compares type at. Nothing is installed: the glyphs are drawn from the file's bytes.
pub fn font_preview(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>) -> E {
    let t_ = theme();
    let hash = fluent::text::hash_str(source);
    let load = use_state(("font-bytes", key, hash), || {
        let slot: FontSlot = Default::default();
        let (writer, source) = (slot.clone(), source.clone());
        app.spawn(move |task| {
            let raw = data_url_bytes(&source).unwrap_or_default();
            let sfnt = sfnt_bytes(&raw).filter(|b| swash::FontRef::from_index(b, 0).is_some());
            *writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(sfnt);
            task.post(|_| repaint());
        });
        FontLoad::Reading(slot)
    });
    let arrived = load.with(|l| match l {
        FontLoad::Reading(slot) => slot.lock().unwrap_or_else(|p| p.into_inner()).take(),
        FontLoad::Done(_) => None,
    });
    if let Some(sfnt) = arrived {
        load.set(FontLoad::Done(sfnt.map(|bytes| Rc::new(LoadedFont { names: read_font_names(&bytes), bytes: Rc::new(bytes) }))));
    }
    let font = load.with(|l| match l {
        FontLoad::Reading(_) => None,
        FontLoad::Done(font) => Some(font.clone()),
    });
    let Some(font) = font else { return notice(t("正在读取…", "Loading…")) };
    let Some(font) = font else { return notice(t("无法加载这个字体文件。", "This font file could not be loaded.")) };
    let sample = use_state(("font-sample", key), String::new);
    let names = font.names.clone().unwrap_or_default();
    let title = names.full_name.clone().or_else(|| names.family.clone()).unwrap_or_else(|| name.to_owned());
    let mut meta: Vec<String> = [names.family, names.subfamily, names.version, names.designer].into_iter().flatten().collect();
    if let Some(b) = bytes {
        meta.push(format_bytes(b));
    }
    let pangram = {
        let typed = sample.get();
        if typed.is_empty() { PANGRAM.to_owned() } else { typed }
    };
    let setter = sample.clone();
    let room = rect_of(("font-scroll", key)).map_or(f32::MAX, |r| r.w - 32.0);
    let mut header = col().gap(6.0).min_w(0.0).shrink0();
    for piece in wrap_sample(&font.bytes, &title, 26.0, room) {
        header = header.child(div().clip().shrink0().child(font_line(key, &font.bytes, &piece, 26.0)));
    }
    header = header.child(text(meta.join(" · ")).ty(FINE).color(t_.text_tertiary)).child(field(("font-sample-input", key), sample.get()).placeholder(t("输入示例文字…", "Type sample text…")).on_change(move |_: &mut App, v| setter.set(v)).build().h(28.0).shrink0().aria_label(t("示例文字", "Sample text")));
    let mut sizes = col().gap(8.0).min_w(0.0).shrink0();
    for size in SIZES {
        sizes = sizes.child(row().items_baseline().gap(10.0).min_w(0.0).shrink0().clip().child(text(format!("{size}")).ty(TypeStyle { mono: true, ..FINE }).color(t_.text_tertiary).text_end().w(22.0).shrink0()).child(font_line(key, &font.bytes, &pangram, size)));
    }
    let mut glyphs = col().pt(12.0).min_w(0.0).shrink0().border_top(t_.divider);
    for line in GLYPH_ROWS.iter().chain([&CJK_ROW]) {
        for piece in wrap_sample(&font.bytes, line, GLYPH_ROW_SIZE, room) {
            glyphs = glyphs.child(div().clip().shrink0().child(font_line(key, &font.bytes, &piece, GLYPH_ROW_SIZE)));
        }
    }
    col().key(("font-scroll", key)).flex1().min_h(0.0).min_w(0.0).scroll_y().px(16.0).pt(14.0).pb(24.0).gap(16.0).child(header).child(sizes).child(glyphs)
}

/// A row of glyphs broken where it stops fitting (`overflow-wrap: anywhere`): each piece is one picture.
pub fn wrap_sample(font: &[u8], line: &str, px: f32, room: f32) -> Vec<String> {
    let Some(face) = swash::FontRef::from_index(font, 0) else { return vec![line.to_owned()] };
    let (charmap, metrics) = (face.charmap(), face.glyph_metrics(&[]).scale(px));
    let mut out = Vec::new();
    let (mut current, mut used) = (String::new(), 0.0);
    for c in line.chars() {
        let advance = metrics.advance_width(charmap.map(c));
        if used + advance > room && !current.is_empty() {
            out.push(std::mem::take(&mut current));
            used = 0.0;
        }
        current.push(c);
        used += advance;
    }
    out.push(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_the_zoom_stops_from_what_is_on_screen() {
        assert_eq!(next_zoom_step(&ZOOM_STEPS, 0.8, true, 1e-6), 1.0);
        assert_eq!(next_zoom_step(&ZOOM_STEPS, 0.8, false, 1e-6), 0.75);
        assert_eq!(next_zoom_step(&ZOOM_STEPS, 16.0, true, 1e-6), 16.0);
        assert_eq!(next_zoom_step(&ZOOM_STEPS, 0.1, false, 1e-6), 0.1);
    }

    #[test]
    fn picks_the_delimiter_a_files_name_says_before_guessing() {
        assert_eq!(delimiter_for("a.tsv", "x,y"), '\t');
        assert_eq!(delimiter_for("a.psv", "x,y"), '|');
        assert_eq!(delimiter_for("a.csv", "x;y\n1;2"), ';');
    }

    /// A font the system has, so the name table and rasterizer are read from a real file.
    fn system_font() -> Option<Vec<u8>> {
        ["/System/Library/Fonts/SFNSMono.ttf", "/System/Library/Fonts/Supplemental/Arial.ttf", "/System/Library/Fonts/Geneva.ttf", "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"].iter().find_map(|p| std::fs::read(p).ok())
    }

    #[test]
    fn reads_a_real_fonts_names_and_draws_its_glyphs() {
        let Some(bytes) = system_font() else { return };
        let names = read_font_names(&bytes).expect("a font's names");
        assert!(names.family.is_some());
        let (mask, w, h) = rasterize_sample(&bytes, "Ag", 32.0).expect("glyphs");
        assert!(w > 10 && h >= 32);
        assert!(mask.iter().any(|a| *a > 200), "ink on the mask");
    }

    #[test]
    fn breaks_a_row_of_glyphs_where_it_stops_fitting() {
        let Some(bytes) = system_font() else { return };
        let pieces = wrap_sample(&bytes, "ABCDEFGHIJKLMNOPQRSTUVWXYZ", 22.0, 100.0);
        assert!(pieces.len() > 1);
        assert_eq!(pieces.concat(), "ABCDEFGHIJKLMNOPQRSTUVWXYZ");
    }

    #[test]
    fn says_nothing_about_a_file_that_is_not_a_font() {
        assert_eq!(read_font_names(b"not a font at all"), None);
        assert!(sfnt_bytes(b"wOF2 and more").is_none());
    }

    /// A real WOFF2 from the wild: Fraunces (OFL), with `glyf` and `loca` transformed and everything Brotli-compressed.
    const WOFF2: &[u8] = include_bytes!("../tests/fixtures/fraunces-latin-600-normal.woff2");

    #[test]
    fn unpacks_a_real_woff2_and_draws_from_it() {
        assert_eq!(&WOFF2[..4], b"wOF2");
        let sfnt = sfnt_bytes(WOFF2).expect("a WOFF2 unpacks");
        assert_eq!(&sfnt[..4], &[0, 1, 0, 0]);
        let names = read_font_names(&sfnt).expect("its name table");
        assert!(names.family.as_deref().is_some_and(|f| f.contains("Fraunces")), "{names:?}");
        // The rebuilt `glyf` and `hmtx` are what these come from: two letters that differ, with ink and advances of their own.
        let (a, aw, ah) = rasterize_sample(&sfnt, "A", 64.0).expect("glyphs");
        let (i, iw, _) = rasterize_sample(&sfnt, "i", 64.0).expect("glyphs");
        assert!(aw > iw && ah >= 64, "{aw} {iw} {ah}");
        assert!(a.iter().filter(|p| **p > 200).count() > 100 && i.iter().filter(|p| **p > 200).count() > 50);
        assert_ne!(a.len(), i.len());
        let face = swash::FontRef::from_index(&sfnt, 0).unwrap();
        assert!(face.glyph_metrics(&[]).advance_width(face.charmap().map('A')) > 0.0);
    }

    #[test]
    fn says_nothing_of_a_woff2_cut_short_or_corrupted() {
        for length in [4, 12, 48, 200, WOFF2.len() / 2, WOFF2.len() - 1] {
            assert!(sfnt_bytes(&WOFF2[..length]).is_none(), "{length} bytes");
        }
        let mut corrupted = WOFF2.to_vec();
        for at in (400..corrupted.len()).step_by(97) {
            corrupted[at] ^= 0x5a;
        }
        assert!(sfnt_bytes(&corrupted).is_none_or(|b| swash::FontRef::from_index(&b, 0).is_some() || b.len() > 12));
    }

    #[test]
    fn sets_what_a_font_lacks_in_the_systems_face() {
        let sfnt = sfnt_bytes(WOFF2).unwrap();
        // Fraunces here is a Latin subset.
        assert_eq!(font_runs(&sfnt, "The dog"), vec![(true, "The dog".to_owned())]);
        assert_eq!(font_runs(&sfnt, "dog 狐狸 fox"), vec![(true, "dog ".to_owned()), (false, "狐狸 ".to_owned()), (true, "fox".to_owned())]);
    }

    #[test]
    fn unpacks_a_woff_into_an_sfnt() {
        // Two tables, one deflated: what a WOFF is, built by hand.
        let plain = b"hello table data".to_vec();
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&plain, 6);
        let stored = b"stored";
        let mut woff = Vec::new();
        woff.extend_from_slice(b"wOFF");
        woff.extend_from_slice(&0x00010000u32.to_be_bytes());
        woff.extend_from_slice(&[0, 0, 0, 0]);
        woff.extend_from_slice(&2u16.to_be_bytes());
        woff.resize(44, 0);
        let first = 44 + 2 * 20;
        for (tag, offset, comp, orig) in [(b"aaaa", first, packed.len(), plain.len()), (b"bbbb", first + packed.len(), stored.len(), stored.len())] {
            woff.extend_from_slice(tag);
            for v in [offset as u32, comp as u32, orig as u32, 7u32] {
                woff.extend_from_slice(&v.to_be_bytes());
            }
        }
        woff.extend_from_slice(&packed);
        woff.extend_from_slice(stored);
        let sfnt = sfnt_bytes(&woff).unwrap();
        assert_eq!(&sfnt[..4], &[0, 1, 0, 0]);
        assert_eq!(be16(&sfnt, 4), Some(2));
        assert_eq!(&sfnt[12..16], b"aaaa");
        let offset = be32(&sfnt, 12 + 8).unwrap();
        assert_eq!(&sfnt[offset..offset + plain.len()], &plain[..]);
    }
}
