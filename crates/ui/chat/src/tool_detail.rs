//! `ToolDetailRenderer`: draws what `tool_view` reads out of a call, with `ToolRenderers.css`'s sizes.
//!
//! A card sits in a virtualised expanded row, and nothing here may make a frame slow however long the output is. A text's lines
//! stay one selectable leaf while that is cheap, as they are one selection region upstream. Past that only the rows in view exist:
//! the long text is cut into rows of the known mono cell so that the skipped height stays exact and every line stays reachable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;

use fluent::text::is_cjk;
use fluent::{queue_image, ImageId};
use mewrk_client::model::ToolDescriptor;
use mewrk_client::prelude::*;
use mewrk_pane_files::format::data_url_bytes;
use mewrk_pane_files::icons::file_icon;
use mewrk_pane_files::images::decode_picture;
use mewrk_pane_git::code_table::{cell_for_output, table_leaf, CodeTable, TableRow, Tone};
use mewrk_pane_git::diff_highlight::{code_language, highlight_diff_lines, CodeToken, DiffSide};
use mewrk_pane_git::diff_output::diff_output;
use mewrk_panes::highlight::Token;
use taffy::prelude::{fr, length, max_content, minmax, Size as TaffySize};
use taffy::style::{AlignItems, Display};

use crate::image_strip::image_strip;
use crate::item::{stream_status_name, ToolItem};
use crate::kit::{chevron, retained_flag, set_retained_flag};
use crate::tool_view::{raw_data_text, source_letter, tool_detail_model, DetailBody, EntryIcon, KvCard, Lang, LineTone, ListEntry, ReadLine, ToneText, ToolDetailModel, ToolPhase, WebResultView, WebSourceChip, READ_HIGHLIGHT_LINES};

const fn mono(size: f32, line: f32) -> TypeStyle {
    TypeStyle { size, line, weight: Weight::Regular, mono: true, italic: false, serif: false }
}

const fn sans(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

/// `.tool-renderer__lines`: `font: 10.5px/1.55 var(--mono)`.
const LINES: TypeStyle = mono(10.5, 16.275);
/// The same on a whole-pixel line, where a text is cut into rows: skipped rows are counted in this height, and a fractional line
/// is rounded differently at each scale.
const LONG_LINES: TypeStyle = mono(10.5, 16.0);
const LONG_ROW_H: f32 = LONG_LINES.line;
/// `.tool-renderer__lines > li { padding-left: 2ch; text-indent: -2ch }`, at the mono cell of `LINES`.
const ITEM_HANG: f32 = 2.0 * 6.3;
/// `.tool-renderer__raw summary`.
const RAW_SUMMARY: TypeStyle = sans(10.0, 14.0, Weight::Regular);
/// `.tool-renderer__command > span`.
const PROMPT: TypeStyle = TypeStyle { size: 12.0, line: 16.275, weight: Weight::Semibold, mono: true, italic: false, serif: false };
/// `.tool-renderer__running`.
const RUNNING: TypeStyle = sans(11.0, 16.0, Weight::Regular);
/// `.tool-renderer__source`.
const SOURCE_HOST: TypeStyle = sans(11.0, 15.0, Weight::Regular);
/// `.tool-renderer__source-letter`.
const SOURCE_LETTER: TypeStyle = TypeStyle { size: 9.0, line: 9.0, weight: Weight::Semibold, mono: true, italic: false, serif: false };
/// `.tool-renderer__source-icon`'s box.
const SOURCE_ICON: f32 = 15.0;
/// `.tool-renderer__findings`.
const FINDINGS: TypeStyle = sans(11.5, 17.8, Weight::Regular);
/// `.tool-renderer__result-list strong`.
const RESULT_TITLE: TypeStyle = sans(11.5, 17.8, Weight::Semibold);
/// `.tool-renderer__result-list p`.
const RESULT_BODY: TypeStyle = sans(11.0, 16.5, Weight::Regular);

/// `--tool-renderer-max-height`: `min(52vh, 440px)`, or `min(48vh, 360px)` on a narrow window.
fn body_max_height() -> f32 {
    let (w, h) = window_size();
    if w <= 720.0 { (h * 0.48).min(360.0) } else { (h * 0.52).min(440.0) }
}

/// `.diff-output__body { max-height: 240px }`, which a read keeps.
const READ_MAX_H: f32 = 240.0;
/// `.tool-renderer__raw pre { max-height: 220px }`.
const RAW_MAX_H: f32 = 220.0;
/// `.tool-renderer .tool-kv__key { max-width: 16ch }`, at the mono cell of `LINES`.
const KV_KEY_MAX_W: f32 = 16.0 * 6.3;

/// Past either of these a text stops being one leaf: each frame measures a leaf line by line and hashes all of it, which at this
/// size costs under a millisecond in a shipped build. A whole page of output from a tool that caps its answer stays one leaf.
const LEAF_MAX_LINES: usize = 500;
const LEAF_MAX_BYTES: usize = 48 * 1024;
/// Past this a list stops drawing an element per entry.
const ENTRY_ROW_LIMIT: usize = 300;
/// Rows kept beyond the viewport on each side, and the grain the window moves in, so that a small scroll rebuilds nothing.
const WINDOW_OVERSCAN: usize = 48;
const WINDOW_STEP: usize = 32;

// ---------------------------------------------------------------- tones

#[derive(Clone, Copy, PartialEq)]
struct Palette {
    plain: Color,
    dim: Color,
    note: Color,
    error: Color,
    warning: Color,
}

impl Palette {
    fn of(t: &Theme) -> Self {
        Self { plain: t.text_primary, dim: t.text_secondary, note: t.text_tertiary, error: t.critical, warning: t.caution }
    }

    fn color(&self, tone: LineTone) -> Color {
        match tone {
            LineTone::Plain => self.plain,
            LineTone::Dim => self.dim,
            LineTone::Note => self.note,
            LineTone::Error => self.error,
            LineTone::Warning => self.warning,
        }
    }
}

fn span_of(text: impl Into<Arc<str>>, ty: TypeStyle, color: Color) -> Span {
    Span { text: text.into(), ty, color: Some(color) }
}

// ---------------------------------------------------------------- long text

/// Which rows of `count` fall in the scroller's view, from where both sat last frame. Works wherever the block starts in the
/// scroller, which `visible_rows` does not. Moves in steps, and starts at the top when nothing has been laid out yet.
pub fn window_of(block: Option<Rect>, scroller: Option<Rect>, count: usize, row_h: f32) -> Range<usize> {
    let (top, bottom) = match (block, scroller) {
        (Some(block), Some(scroller)) => ((scroller.y - block.y).max(0.0), (scroller.y + scroller.h - block.y).max(0.0)),
        _ => (0.0, 1200.0),
    };
    let first = ((top / row_h).floor() as usize).saturating_sub(WINDOW_OVERSCAN) / WINDOW_STEP * WINDOW_STEP;
    let last = (((bottom / row_h).ceil() as usize + WINDOW_OVERSCAN).div_ceil(WINDOW_STEP) * WINDOW_STEP).min(count);
    first.min(count)..last
}

/// Where each row of `text` starts when it is cut to `cols` cells: at a line break, or where a line runs out of room, after its
/// last space if it has one, as a wrap would. CJK takes two cells, which leaves it room in any font whose wide characters are
/// narrower than that.
pub fn visual_rows(text: &str, cols: usize) -> Vec<u32> {
    let mut rows = vec![0u32];
    let mut col = 0;
    // Where the row could break, and how many cells come before that.
    let mut after_space = None;
    for (at, c) in text.char_indices() {
        if c == '\n' {
            rows.push(at as u32 + 1);
            col = 0;
            after_space = None;
            continue;
        }
        let w = if is_cjk(c) { 2 } else { 1 };
        if col + w > cols {
            match after_space.take() {
                Some((cut, used)) => {
                    rows.push(cut);
                    col -= used;
                }
                None => {
                    rows.push(at as u32);
                    col = 0;
                }
            }
        }
        col += w;
        if c == ' ' {
            after_space = Some((at as u32 + 1, col));
        }
    }
    rows
}

/// The spans of rows `range`. A row that was cut gets the line break the wrap would have made.
fn window_spans(text: &ToneText, rows: &[u32], range: Range<usize>, palette: &Palette) -> Vec<Span> {
    let mut out = Vec::new();
    let mut run = String::new();
    let mut tone = None;
    let flush = |run: &mut String, tone: Option<LineTone>, out: &mut Vec<Span>| {
        if let Some(tone) = tone
            && !run.is_empty()
        {
            out.push(span_of(std::mem::take(run), LONG_LINES, palette.color(tone)));
        }
    };
    let end_row = range.end;
    for r in range {
        let start = rows[r] as usize;
        let end = rows.get(r + 1).map_or(text.text.len(), |next| *next as usize);
        let line = text.text[start..end].strip_suffix('\n').unwrap_or(&text.text[start..end]);
        let stop = start + line.len();
        let mut at = start;
        while at < stop {
            let index = text.tones.partition_point(|(from, _)| *from as usize <= at).saturating_sub(1);
            let here = text.tones.get(index).map_or(LineTone::Plain, |t| t.1);
            let until = text.tones.get(index + 1).map_or(stop, |t| (t.0 as usize).min(stop));
            if tone != Some(here) {
                flush(&mut run, tone, &mut out);
                tone = Some(here);
            }
            run.push_str(&text.text[at..until]);
            at = until;
        }
        if r + 1 < end_row {
            run.push('\n');
        }
    }
    flush(&mut run, tone, &mut out);
    out
}

fn is_long(text: &str) -> bool {
    text.len() > LEAF_MAX_BYTES || text.as_bytes().iter().filter(|b| **b == b'\n').count() > LEAF_MAX_LINES
}

struct LongWindow {
    // Held and not its address: a later text could otherwise land on the same one.
    text: Arc<str>,
    cols: usize,
    rows: Rc<Vec<u32>>,
    range: Range<usize>,
    palette: Palette,
    spans: Vec<Span>,
}

/// A text too long to be one leaf: a spacer, the rows near the view as one leaf, a spacer.
fn long_text(key: u64, scroller: u64, text: &ToneText) -> E {
    let palette = Palette::of(&theme());
    // The real cell, from the font: a paint may be the first to know it. The default is `LINES`' own, so a first frame is close.
    let cell = use_state(("tool-long-cell", key), || LONG_LINES.size * 0.6);
    let width = rect_of(key).map_or_else(|| window_size().0.min(640.0), |r| r.w);
    let cols = ((width / cell.get().max(1.0)).floor() as usize).max(8);
    let state = use_state(("tool-long-window", key), || None::<LongWindow>);
    let mut guard = state.borrow_mut();
    let rows = match guard.as_ref() {
        Some(window) if Arc::ptr_eq(&window.text, &text.text) && window.cols == cols => window.rows.clone(),
        _ => Rc::new(visual_rows(&text.text, cols)),
    };
    let range = window_of(rect_of(key), rect_of(scroller), rows.len(), LONG_ROW_H);
    let fresh = !guard.as_ref().is_some_and(|window| Arc::ptr_eq(&window.text, &text.text) && window.cols == cols && window.range == range && window.palette == palette);
    if fresh {
        *guard = Some(LongWindow { text: text.text.clone(), cols, rows: rows.clone(), range: range.clone(), palette, spans: window_spans(text, &rows, range.clone(), &palette) });
    }
    let spans_now = guard.as_ref().map_or_else(Vec::new, |window| window.spans.clone());
    drop(guard);
    col()
        .key(key)
        .w_full()
        .child(div().h(range.start as f32 * LONG_ROW_H))
        // Named by its first row: a selection is held in offsets of this leaf's text, and does not carry over to another window.
        .child(spans(spans_now).key((key, range.start)).selectable())
        .child(div().h((rows.len() - range.end) as f32 * LONG_ROW_H))
        .child(
            custom::<App>(move |_, ui, _| {
                let measured = ui.measure("0", &LONG_LINES);
                if measured > 0.0 && measured != cell.get() {
                    cell.set(measured);
                }
            })
            .size(1.0, 1.0)
            .absolute()
            .pass_through(),
        )
}

/// A text in the code face, one selection region, however long. `hanging` is for a text whose lines are items: cut into rows of a known
/// cell, a long one has no way to say how many rows a line takes once what wraps has less room.
fn mono_text(key: u64, scroller: u64, text: &ToneText, hanging: f32) -> E {
    if is_long(&text.text) {
        return long_text(key, scroller, text);
    }
    let palette = Palette::of(&theme());
    if let [(_, tone)] = text.tones.as_slice() {
        return self::text(text.text.clone()).key(key).ty(LINES).color(palette.color(*tone)).selectable().hanging(hanging);
    }
    let cache = use_state(("tool-text-spans", key), || None::<(Arc<str>, Palette, Vec<Span>)>);
    let mut cached = cache.borrow_mut();
    if !cached.as_ref().is_some_and(|(known, known_palette, _)| Arc::ptr_eq(known, &text.text) && *known_palette == palette) {
        let built = text.runs().filter(|(run, _)| !run.is_empty()).map(|(run, tone)| span_of(run, LINES, palette.color(tone))).collect();
        *cached = Some((text.text.clone(), palette, built));
    }
    spans(cached.as_ref().map_or_else(Vec::new, |(_, _, built)| built.clone())).key(key).selectable().hanging(hanging)
}

fn plain_text(key: u64, scroller: u64, value: &Arc<str>) -> E {
    if is_long(value) {
        return long_text(key, scroller, &ToneText { text: value.clone(), tones: vec![(0, LineTone::Plain)] });
    }
    self::text(value.clone()).key(key).ty(LINES).color(theme().text_primary).selectable()
}

/// `.tool-renderer__lines`' box.
fn lines_box() -> E {
    col().pt(8.0).px(11.0).pb(10.0).min_w(0.0)
}

// ---------------------------------------------------------------- the views

/// `EmptyResult`.
fn empty_result(message: &str) -> E {
    lines_box().child(self::text(message.to_owned()).ty(LINES).color(theme().text_tertiary).selectable())
}

fn entry_mark(mark: &EntryIcon, t: &Theme) -> Option<E> {
    // `vertical-align: -.2em` and `margin-inline-end: .4em`, for an icon set in a line of text.
    match mark {
        EntryIcon::None => None,
        EntryIcon::Folder => Some(icon("Folder", 12.0).color(t.text_secondary).mr(4.2).mt(2.0)),
        EntryIcon::File(path) => Some(file_icon(path, 12.0).mr(4.2).mt(2.0)),
    }
}

fn entry_row(key: u64, entry: &ListEntry, t: &Theme) -> E {
    let color = if entry.note { t.text_tertiary } else { t.text_primary };
    // Beside a mark the column it stands in does the hanging, and that is as near as makes no difference to the 2ch it should be.
    let hanging = if matches!(entry.icon, EntryIcon::None) { ITEM_HANG } else { 0.0 };
    row().key(key).items_start().min_w(0.0).role(Role::ListItem).child(entry_mark(&entry.icon, t)).child(self::text(entry.text.clone()).ty(LINES).color(color).selectable().hanging(hanging).flex1())
}

/// `FileListView`, and `GrepView`'s files: every entry may carry an icon, so every entry is an element. Past the limit they are a
/// window of rows of one height, each on one line, as a wrapped height is not known without shaping the whole list.
fn entries_view(key: u64, scroller: u64, label: &str, entries: &[ListEntry]) -> E {
    let t_ = theme();
    if entries.len() <= ENTRY_ROW_LIMIT {
        return lines_box().role(Role::List).aria_label(label).children(entries.iter().enumerate().map(|(i, entry)| entry_row(hash_key((key, i)), entry, &t_)));
    }
    let list_key = hash_key((key, "window"));
    let range = window_of(rect_of(list_key), rect_of(scroller), entries.len(), LONG_ROW_H);
    let rows = range.clone().map(|i| {
        let entry = &entries[i];
        let color = if entry.note { t_.text_tertiary } else { t_.text_primary };
        row().key((key, i)).h(LONG_ROW_H).items_center().min_w(0.0).role(Role::ListItem).child(entry_mark(&entry.icon, &t_).map(|mark| mark.mt(0.0))).child(self::text(entry.text.clone()).ty(LONG_LINES).color(color).ellipsis().flex1().tooltip(entry.text.clone()))
    });
    lines_box().role(Role::List).aria_label(label).child(col().key(list_key).child(div().h(range.start as f32 * LONG_ROW_H)).children(rows).child(div().h((entries.len() - range.end) as f32 * LONG_ROW_H)))
}

/// The rows a read's table holds: its lines coloured from clean, as the excerpt starts wherever the read did and what was open
/// above it is not known.
fn read_table(path: Option<&str>, lines: &[ReadLine]) -> CodeTable {
    let tokens: Vec<Option<Vec<CodeToken>>> = match path.filter(|_| lines.len() <= READ_HIGHLIGHT_LINES) {
        Some(path) => {
            let excerpt: Vec<(DiffSide, &str)> = lines.iter().map(|line| (DiffSide::Both, line.text.as_str())).collect();
            highlight_diff_lines(code_language(path), &excerpt)
        }
        None => vec![None; lines.len()],
    };
    let rows = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if line.note {
                return TableRow::Note(line.text.clone());
            }
            // `#!/usr/bin/env node` says how to run the file, whatever the file is in.
            let shebang;
            let line_tokens = if i == 0 && line.text.starts_with("#!") && path.is_some() {
                shebang = [CodeToken { kind: Token::Meta, value: line.text.clone() }];
                Some(&shebang[..])
            } else {
                tokens[i].as_deref()
            };
            TableRow::Line { tone: Tone::Context, number: u32::try_from(line.number).ok(), cell: cell_for_output(&line.text, line_tokens, None) }
        })
        .collect();
    CodeTable::of_card_rows(rows)
}

/// `ReadView`: on the diff's base, with source line numbers. `white-space: pre`, so a long line scrolls sideways and stays whole.
fn read_view(key: u64, stamp: u64, label: &str, path: Option<&str>, lines: &[ReadLine]) -> E {
    let table = use_state(("tool-read-rows", key, stamp), || read_table(path, lines));
    col().key(("tool-read", key)).scroll_y().max_h(READ_MAX_H).bg(theme().code_bg).role(Role::Region).aria_label(label).child(table_leaf(key, table, false, false, false, None, None))
}

/// `TerminalView`: a command apart from its output, each its own selection region.
fn terminal_view(key: u64, scroller: u64, command: &Arc<str>, output: &Arc<str>) -> E {
    let t_ = theme();
    col()
        .child(
            row()
                .items_start()
                .gap(8.0)
                .py(8.0)
                .px(11.0)
                .border_bottom(t_.divider)
                .child(self::text("›").ty(PROMPT).color(t_.success).nowrap().aria_hidden(true))
                .child(self::text(command.clone()).ty(LINES).color(t_.accent_text).selectable().flex1()),
        )
        .child(col().min_h(38.0).pt(9.0).px(11.0).pb(12.0).child(plain_text(hash_key((key, "output")), scroller, output)))
}

/// `KeyValueCard`: the lines worth knowing in a grid, then what the call answered, under a rule and across the card.
fn kv_view(key: u64, scroller: u64, card: &KvCard) -> E {
    let t_ = theme();
    let has_rows = !card.rows.is_empty();
    let grid = has_rows.then(|| {
        let cells = card.rows.iter().enumerate().flat_map(|(i, (label, value))| {
            [
                self::text(label.clone()).ty(LINES).color(t_.text_tertiary).ellipsis().max_w(KV_KEY_MAX_W).tooltip(label.clone()),
                col().min_w(0.0).child(plain_text(hash_key((key, "value", i)), scroller, value)),
            ]
        });
        div()
            .pt(8.0)
            .px(11.0)
            .pb(if card.result.is_some() { 0.0 } else { 10.0 })
            .with_style(|s| {
                s.display = Display::Grid;
                s.grid_template_columns = vec![minmax(length(0.0_f32), max_content()), minmax(length(0.0_f32), fr(1.0_f32))];
                s.gap = TaffySize { width: length(12.0_f32), height: length(0.0_f32) };
                s.align_items = Some(AlignItems::Start);
            })
            .children(cells)
    });
    let result = card.result.as_ref().map(|result| {
        let view = col().pt(8.0).px(11.0).pb(10.0).min_w(0.0).child(plain_text(hash_key((key, "result")), scroller, result));
        if has_rows { view.mt(8.0).border_top(t_.divider) } else { view }
    });
    col().child(grid).child(result)
}

/// `AgentWaitView`: who answered heads its own line, whole, over what it said.
fn envelopes_view(key: u64, scroller: u64, notice: Option<&str>, envelopes: &[crate::orchestration::WaitEnvelopeView], status_line: Option<&str>) -> E {
    let t_ = theme();
    let note = |line: &str| self::text(line.to_owned()).ty(LINES).color(t_.text_tertiary).selectable();
    // `:is(section, p) + :is(section, p) { margin-top: .8em }`.
    lines_box()
        .gap(8.4)
        .child(notice.map(note))
        .children(envelopes.iter().enumerate().map(|(i, envelope)| {
            let body: Arc<str> = Arc::from(envelope.body.as_str());
            col()
                .min_w(0.0)
                .child(self::text(format!("{} · {}", envelope.agent, envelope.status)).ty(LINES).color(t_.text_secondary).selectable())
                .child((!envelope.body.is_empty()).then(|| plain_text(hash_key((key, "envelope", i)), scroller, &body)))
        }))
        .child(status_line.map(note))
}

/// What is known of a site's icon, for the life of the app: most chips are the same few sites, and upstream's own answer comes
/// from a cache too. Absent, nobody has asked.
#[derive(Clone, Copy)]
enum SiteIcon {
    /// Asked for and not here yet, or the site has none.
    Letter,
    Ready { id: ImageId, width: u32, height: u32 },
}

thread_local! {
    static SITE_ICONS: RefCell<HashMap<String, SiteIcon>> = RefCell::new(HashMap::new());
}

fn decode_site_icon(source: &str) -> Option<(Vec<u8>, u32, u32)> {
    let media_type = source.strip_prefix("data:").and_then(|s| s.split([';', ',']).next()).unwrap_or("");
    decode_picture(&data_url_bytes(source)?, media_type)
}

fn keep_site_icon(host: String, decoded: Option<(Vec<u8>, u32, u32)>) {
    let Some((rgba, width, height)) = decoded else { return };
    let id = queue_image(rgba, width, height);
    SITE_ICONS.with(|icons| icons.borrow_mut().insert(host, SiteIcon::Ready { id, width, height }));
    repaint();
}

/// A site's icon as `web_source_icon` would answer it, a `data:` URL. Says whether it decoded.
pub fn adopt_site_icon(host: &str, source: &str) -> bool {
    let decoded = decode_site_icon(source);
    let usable = decoded.is_some();
    keep_site_icon(host.to_owned(), decoded);
    usable
}

/// Whether a chip of this site draws its icon rather than its letter.
pub fn has_site_icon(host: &str) -> bool {
    SITE_ICONS.with(|icons| matches!(icons.borrow().get(host), Some(SiteIcon::Ready { .. })))
}

/// Asks the host for a site's icon, once. It sits in the chip because a paint is where a view holds an `App`, and only one in view
/// asks, as upstream's effect only runs for a chip that mounted.
fn site_icon_request(host: String, url: String) -> E {
    custom::<App>(move |app, _, _| {
        let first = SITE_ICONS.with(|icons| {
            let mut icons = icons.borrow_mut();
            if icons.contains_key(&host) {
                return false;
            }
            icons.insert(host.clone(), SiteIcon::Letter);
            true
        });
        if !first {
            return;
        }
        app.invoke::<Option<String>>("web_source_icon", json!({ "url": url }), move |app, result| {
            // A missing icon is ordinary. Reporting it would turn every site without one into an error the user has to read.
            let Some(source) = result.ok().flatten() else { return };
            app.spawn(move |task| {
                let decoded = decode_site_icon(&source);
                task.post(move |_| keep_site_icon(host, decoded));
            });
        });
    })
    .size(1.0, 1.0)
    .absolute()
    .pass_through()
}

/// `WebSourceIcon`: the site's icon, or the first letter of its domain while the icon is still being fetched or when the site has
/// none. The letter holds the icon's exact box, so a chip does not reflow when a late-arriving icon replaces it.
fn source_icon(chip: &WebSourceChip, t: &Theme) -> E {
    let known = SITE_ICONS.with(|icons| icons.borrow().get(&chip.host).copied());
    if let Some(SiteIcon::Ready { id, width, height }) = known {
        // `object-fit: contain`.
        let fit = SOURCE_ICON / width.max(height).max(1) as f32;
        return div().size(SOURCE_ICON, SOURCE_ICON).shrink0().center().child(image(id, (width, height)).size(width as f32 * fit, height as f32 * fit).image_radius(3.0));
    }
    div()
        .size(SOURCE_ICON, SOURCE_ICON)
        .shrink0()
        .radius(3.0)
        .bg(t.subtle_hover)
        .center()
        .child(self::text(source_letter(&chip.host)).ty(SOURCE_LETTER).color(t.text_tertiary).nowrap().aria_hidden(true))
        .child(known.is_none().then(|| site_icon_request(chip.host.clone(), chip.url.clone())))
}

/// One site chip. The tooltip carries the page title and full URL: the chip itself shows only the domain, and a reader who wants
/// the rest should not have to open the page to see it.
fn source_chip(key: u64, chip: &WebSourceChip) -> E {
    let t_ = theme();
    let url = chip.url.clone();
    let tip = match &chip.title {
        Some(title) => format!("{title}\n{}", chip.url),
        None => chip.url.clone(),
    };
    row()
        .key(key)
        .items_center()
        .gap(6.0)
        .max_w(240.0)
        .pt(3.0)
        .pr(9.0)
        .pb(3.0)
        .pl(4.0)
        .border(t_.card_stroke)
        .radius(999.0)
        .hover_bg(t_.subtle_hover)
        .press_bg(t_.subtle_pressed)
        .cursor(CursorKind::Hand)
        .full_text(tip)
        .role(Role::Link)
        .child(source_icon(chip, &t_))
        .child(self::text(chip.host.clone()).ty(SOURCE_HOST).color(t_.text_secondary).ellipsis())
        .on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": url })))
}

/// `WebSearchView`: the sites it consulted, then what it found.
fn web_search_view(key: u64, label: &str, chips: &[WebSourceChip], findings: Option<&Arc<str>>, results: &[WebResultView]) -> E {
    let t_ = theme();
    let has_chips = !chips.is_empty();
    let sources = has_chips.then(|| row().wrap().gap(5.0).py(8.0).px(10.0).role(Role::Navigation).aria_label(label).children(chips.iter().map(|chip| source_chip(hash_key((key, "source", &chip.host)), chip))));
    // `.tool-renderer__sources + ...`: a rule under the sites, for whichever part comes right after them.
    let under_sources = |view: E| if has_chips { view.border_top(t_.divider).pt(9.0) } else { view };
    let findings_view = findings.map(|findings| under_sources(col().pt(2.0).px(12.0).pb(11.0).child(self::text(findings.clone()).ty(FINDINGS).color(t_.text_primary).selectable())));
    let results_view = (!results.is_empty()).then(|| {
        let list = col().pt(5.0).pr(12.0).pb(10.0).pl(12.0).gap(8.0).children(results.iter().enumerate().map(|(i, result)| {
            row()
                .items_start()
                // The `<ol>`'s marker, in the 14 px of its padding.
                .child(self::text(format!("{}.", i + 1)).ty(RESULT_BODY).color(t_.text_tertiary).nowrap().w(14.0))
                .child(
                    col()
                        .flex1()
                        .min_w(0.0)
                        .child(self::text(result.title.clone()).ty(RESULT_TITLE).color(t_.text_primary).selectable())
                        .child(result.content.as_ref().map(|content| self::text(content.clone()).ty(RESULT_BODY).color(t_.text_secondary).selectable().mt(2.0))),
                )
        }));
        if has_chips && findings.is_none() { under_sources(list) } else { list }
    });
    col().child(sources).child(findings_view).child(results_view)
}

/// `RunningView`.
fn running_view(label: &str) -> E {
    let t_ = theme();
    row().items_start().gap(8.0).py(10.0).px(12.0).role(Role::Status).child(icon("Timer", 14.0).color(t_.accent_text).mt(1.0)).child(self::text(label.to_owned()).ty(RUNNING).color(t_.accent_text))
}

/// `RawDataDisclosure`: the call as JSON, built only once it opens.
fn raw_disclosure(key: &str, item: &ToolItem<'_>, stamp: u64) -> E {
    let t_ = theme();
    let open_key = hash_key(("tool-raw-open", key));
    let open = retained_flag(open_key, false);
    let hovered = use_state(("tool-raw-hover", key), || false);
    let summary_key = hash_key(("tool-raw-summary", key));
    let hover = hovered.clone();
    let summary_color = if hovered.get() { t_.text_primary } else { t_.text_secondary };
    // A `<summary>` keeps its disclosure marker: nothing in upstream's CSS takes it away.
    let summary = row()
        .key(summary_key)
        .items_center()
        .gap(3.0)
        .py(6.0)
        .px(10.0)
        .cursor(CursorKind::Hand)
        .on_hover_change(move |_: &mut App, on| hover.set(on))
        .aria_expanded(open)
        .on_click(move |_: &mut App| set_retained_flag(open_key, !open))
        .child(chevron(open, 9.0, summary_color))
        .child(self::text(Lang::current().t("原始数据", "Raw data")).ty(RAW_SUMMARY).color(summary_color).nowrap());
    let body = open.then(|| {
        let cache = use_state(("tool-raw-text", key), || (0u64, Arc::<str>::from("")));
        if cache.borrow().0 != stamp {
            *cache.borrow_mut() = (stamp, Arc::from(raw_data_text(item)));
        }
        let scroller = hash_key(("tool-raw-pre", key));
        let value = cache.borrow().1.clone();
        col().key(scroller).scroll_y().max_h(RAW_MAX_H).pt(8.0).px(10.0).pb(11.0).child(plain_text(hash_key((scroller, "text")), scroller, &value))
    });
    col().border_top(t_.card_stroke).child(summary).child(body)
}

/// Everything the model reads, as one number. Hashed by content and not by address: nothing promises a card is the same value
/// from frame to frame. A huge string is sampled, as a settled result does not change in its middle and a streaming one changes
/// in length.
fn detail_stamp(item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>) -> u64 {
    fn sampled<'a>(s: &'a str) -> (usize, &'a [u8], &'a [u8]) {
        const SAMPLE: usize = 4096;
        let bytes = s.as_bytes();
        (bytes.len(), &bytes[..bytes.len().min(SAMPLE)], &bytes[bytes.len().saturating_sub(SAMPLE)..])
    }
    let run = item.subagent.map(|run| (run.status as u8, run.updates.len(), run.label.as_deref(), run.name.as_deref(), sampled(&run.task)));
    let input: Vec<u64> = item
        .input
        .iter()
        .map(|(name, value)| match value {
            Value::String(s) => hash_key((name, sampled(s))),
            other => hash_key((name, other.to_string())),
        })
        .collect();
    hash_key((
        (item.tool_name, sampled(&item.result.output), item.result.diff.as_deref().map(sampled), item.result.success, item.result.images.len(), item.result.duration_ms),
        (item.streaming, item.stream_status.map(stream_status_name), item.live.map(|live| Arc::as_ptr(live) as usize), run),
        input,
        (item.notice, descriptor.map(|d| d.label.as_str()), Lang::current() == Lang::En),
    ))
}

/// The card of one call. `key` names its state, so it should be unique to the call in view, the context's id for one.
pub fn tool_detail_renderer(key: &str, item: &ToolItem<'_>, descriptor: Option<&ToolDescriptor>) -> E {
    let t_ = theme();
    let stamp = detail_stamp(item, descriptor);
    let cell = use_state(("tool-detail-model", key), || None::<(u64, Rc<ToolDetailModel>)>);
    let model = {
        let mut cached = cell.borrow_mut();
        match cached.as_ref() {
            Some((known, model)) if *known == stamp => model.clone(),
            _ => {
                let model = Rc::new(tool_detail_model(item, descriptor));
                *cached = Some((stamp, model.clone()));
                model
            }
        }
    };
    let scroller = hash_key(("tool-renderer-body", key));
    let view_key = hash_key(("tool-renderer-view", key));
    let view: Option<E> = match &model.body {
        DetailBody::Running(label) => Some(running_view(label)),
        DetailBody::ImageOnly => None,
        DetailBody::Empty(message) => Some(empty_result(message)),
        DetailBody::Entries { entries, label } => Some(entries_view(view_key, scroller, label, entries)),
        DetailBody::Lines { label, list, text } => {
            let view = lines_box().child(mono_text(view_key, scroller, text, if *list { ITEM_HANG } else { 0.0 }));
            Some(match label {
                Some(label) => view.role(if *list { Role::List } else { Role::Group }).aria_label(label.as_str()),
                None => view,
            })
        }
        DetailBody::Read { label, path, lines } => Some(read_view(view_key, stamp, label, path.as_deref(), lines)),
        // The diff is the card's body, not a card inside a card: it drops its own frame, so one border holds it.
        DetailBody::Diff { value, path } => Some(diff_output(view_key, value, path.as_deref()).border(Color::TRANSPARENT).radius(0.0)),
        DetailBody::Terminal { command, output } => Some(terminal_view(view_key, scroller, command, output)),
        DetailBody::Kv(card) => Some(kv_view(view_key, scroller, card)),
        DetailBody::Envelopes { notice, envelopes, status_line } => Some(envelopes_view(view_key, scroller, notice.as_deref(), envelopes, status_line.as_deref())),
        DetailBody::WebSearch { label, chips, findings, results } => Some(web_search_view(view_key, label, chips, findings.as_ref(), results)),
    };
    let images = (model.phase != ToolPhase::Running && !item.result.images.is_empty()).then(|| col().p(10.0).child(image_strip(("tool-renderer-images", key.to_owned()), &item.result.images, &[])));
    col()
        .key(("tool-renderer", key))
        // A card is as tall as it says, whatever column it stands in: its body scrolls, the card does not give way.
        .shrink0()
        .clip()
        .border(t_.card_stroke)
        .radius(8.0)
        .bg(t_.card_secondary)
        .role(Role::Region)
        .aria_label(model.label.as_str())
        .child(col().key(scroller).scroll_y().max_h(body_max_height()).child(images).child(view))
        .child(raw_disclosure(key, item, stamp))
}

/// The keys tests look at. A scroller's key is what a test wheels; a raw block exists only while it is open.
pub fn tool_body_key(key: &str) -> u64 {
    hash_key(("tool-renderer-body", key))
}

pub fn tool_raw_summary_key(key: &str) -> u64 {
    hash_key(("tool-raw-summary", key))
}

pub fn tool_raw_pre_key(key: &str) -> u64 {
    hash_key(("tool-raw-pre", key))
}
