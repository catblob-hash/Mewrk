//! The conversation's timeline. Only blocks near the viewport are laid out, so opening a long conversation costs
//! the same as opening a short one, and a block's text is shaped once and then only painted.

use std::collections::{HashMap, HashSet};

use fluent::md::{CodeColors, MdStream, Palette};
use fluent::theme::*;
use fluent::ui::{rect, CursorKind, Icon, Key, Rect, Scroll, Ui};
use fluent::{BlockKind, Doc};
use mewrk_core::*;
use mewrk_panes::highlight;
use serde_json::Value;

pub const CHAT_TYPE: TypeStyle = TypeStyle { size: 14.0, line: 22.0, weight: Weight::Regular, mono: false, italic: false };
const PROSE_TYPE: TypeStyle = TypeStyle { size: 13.0, line: 20.0, weight: Weight::Regular, mono: false, italic: false };
const TOP_PAD: f32 = 24.0;
const BOTTOM_PAD: f32 = 28.0;
const BLOCK_GAP: f32 = 16.0;
const ROW_H: f32 = 30.0;
/// Rows of tool output shown inline. The whole output stays in the conversation either way.
const MAX_DETAIL_BYTES: usize = 96 << 10;

pub enum TimelineAction {
    OpenReview,
    EditItem(ItemId),
    DeleteItem(ItemId),
    /// Runs the model again from this user message, dropping what came after it.
    Regenerate(ItemId),
    /// Asks the model again after a failure.
    Retry,
}

#[derive(Clone, Debug)]
struct FileChange {
    file: String,
    added: u32,
    removed: u32,
}

#[derive(Clone, Debug)]
enum Block {
    User { index: usize },
    Text { index: usize },
    Group { key: ItemId, rows: Vec<usize>, tools: u32, thinks: u32 },
    Changes { key: ItemId, files: Vec<FileChange>, added: u32, removed: u32 },
    Notice { index: usize },
    Compaction { index: usize },
    Waiting,
}

struct Slot {
    block: Block,
    h: f32,
    measured: bool,
}

/// Code blocks in an answer, coloured by the same tables as the file and diff panes.
struct Highlighter {
    theme: Theme,
    lang: Option<highlight::Lang>,
    state: highlight::State,
    tokens: Vec<(usize, highlight::Token)>,
}

impl CodeColors for Highlighter {
    fn begin(&mut self, info: &str) {
        self.lang = highlight::lang_of(info.split_whitespace().next().unwrap_or(""));
        self.state = highlight::State::default();
    }

    fn line(&mut self, line: &str, out: &mut Vec<(usize, Color)>) {
        let Some(lang) = self.lang else { return };
        self.tokens.clear();
        highlight::line(lang, &mut self.state, line, &mut self.tokens);
        out.extend(self.tokens.iter().map(|(at, token)| (*at, highlight::color(*token, &self.theme))));
    }
}

struct TextDoc {
    md: MdStream,
    fed: usize,
    finished: bool,
}

struct DetailDoc {
    doc: Doc,
    /// How much of the source this was built from. Thinking grows while it streams.
    fed: usize,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Selection {
    key: u64,
    a: usize,
    b: usize,
}

pub struct Timeline {
    slots: Vec<Slot>,
    dirty: bool,
    pub scroll: Scroll,
    texts: HashMap<ItemId, TextDoc>,
    users: HashMap<ItemId, Doc>,
    details: HashMap<ItemId, DetailDoc>,
    summaries: HashMap<ItemId, (usize, String)>,
    /// A group's open state once the user has touched it. Otherwise a group is open while it is the one running.
    group_open: HashMap<ItemId, bool>,
    row_open: HashSet<ItemId>,
    selection: Option<Selection>,
    dragging: bool,
    last_w: f32,
    last_scale: f32,
    last_dark: bool,
    was_running: bool,
    pub actions: Vec<TimelineAction>,
}

fn fnv(tag: u64, v: u64) -> u64 {
    (tag ^ 0xcbf2_9ce4_8422_2325).wrapping_mul(0x100_0000_01b3) ^ v.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

pub fn thousands(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn tool_label(name: &str) -> &str {
    match name {
        "read" => "读取",
        "write" => "写入",
        "edit" => "编辑",
        "ls" => "列目录",
        "find" => "查找文件",
        "grep" => "搜索内容",
        "zsh" | "bash" | "sh" => "命令",
        "web_fetch" => "抓取网页",
        "web_search" => "搜索网页",
        "agent_spawn" => "子代理",
        "task_wait" => "等待任务",
        "task_list" => "任务列表",
        "task_stop" => "停止任务",
        "send_message" => "发送消息",
        "ask_user" => "提问",
        "plan" => "计划",
        "exit_plan_mode" => "提交计划",
        "skill" => "技能",
        n if n.contains("memory") => "记忆",
        n => n,
    }
}

fn tool_icon(name: &str) -> Icon {
    match name {
        "read" => Icon::File,
        "write" | "edit" | "plan" => Icon::Pencil,
        "ls" => Icon::Folder,
        "find" | "grep" => Icon::Search,
        "zsh" | "bash" | "sh" => Icon::Terminal,
        "web_fetch" | "web_search" => Icon::Globe,
        "agent_spawn" | "send_message" => Icon::Agents,
        "task_wait" => Icon::Clock,
        "task_list" | "task_stop" => Icon::ListChecks,
        "ask_user" => Icon::Info,
        _ => Icon::Tool,
    }
}

/// The one line that says what a call does.
fn tool_summary(t: &ToolItem) -> String {
    let Ok(v) = serde_json::from_str::<Value>(&t.args) else {
        return String::new();
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let line = match t.name.as_str() {
        "read" | "write" | "edit" | "ls" => s("path"),
        "find" => s("query"),
        "grep" => s("pattern"),
        "zsh" | "bash" | "sh" => s("description").or_else(|| s("command")),
        "web_fetch" => v.get("urls").and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_str).map(str::to_owned),
        "agent_spawn" => s("label").or_else(|| s("name")),
        "task_wait" | "task_stop" => s("name").or_else(|| s("task")),
        _ => None,
    };
    line.unwrap_or_default().lines().next().unwrap_or("").to_owned()
}

fn estimate_height(items: &[Item], block: &Block, w: f32) -> f32 {
    let lines = |text: &str, cols: f32| -> f32 {
        let mut n = 0.0;
        for l in text.split('\n') {
            let width: f32 = l.chars().map(|c| if c.is_ascii() { 1.0 } else { 2.0 }).sum();
            n += (width / cols.max(8.0)).ceil().max(1.0);
        }
        n
    };
    let cols = w / 7.2;
    match block {
        Block::User { index } => match &items[*index] {
            Item::User { text, .. } => lines(text, cols * 0.78) * CHAT_TYPE.line + 20.0,
            _ => 44.0,
        },
        Block::Text { index } => match &items[*index] {
            Item::Text { text, .. } => lines(text, cols) * CHAT_TYPE.line,
            _ => CHAT_TYPE.line,
        },
        Block::Group { rows, .. } => ROW_H * (rows.len() as f32 + if rows.len() > 1 { 1.0 } else { 0.0 }),
        Block::Changes { files, .. } => 38.0 + files.len() as f32 * 32.0,
        Block::Notice { .. } | Block::Compaction { .. } | Block::Waiting => 36.0,
    }
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            dirty: true,
            scroll: Scroll::default(),
            texts: HashMap::new(),
            users: HashMap::new(),
            details: HashMap::new(),
            summaries: HashMap::new(),
            group_open: HashMap::new(),
            row_open: HashSet::new(),
            selection: None,
            dragging: false,
            last_w: 0.0,
            last_scale: 0.0,
            last_dark: false,
            was_running: false,
            actions: Vec::new(),
        }
    }

    pub fn structure_changed(&mut self) {
        self.dirty = true;
    }

    pub fn tool_changed(&mut self, id: ItemId) {
        self.details.remove(&id);
        self.summaries.remove(&id);
    }

    pub fn rolled_back(&mut self, from: ItemId) {
        self.texts.retain(|k, _| *k < from);
        self.details.retain(|k, _| *k < from);
        self.summaries.retain(|k, _| *k < from);
        self.selection = None;
    }

    /// An item's text changed under its document.
    pub fn item_edited(&mut self, id: ItemId) {
        self.texts.remove(&id);
        self.users.remove(&id);
        self.details.remove(&id);
        self.summaries.remove(&id);
        self.dirty = true;
        for s in &mut self.slots {
            s.measured = false;
        }
    }

    /// Scrolls so that this item's block is at the top of the view.
    pub fn jump_to(&mut self, items: &[Item], id: ItemId) {
        let Some(index) = items.iter().position(|i| i.id() == id) else { return };
        let mut y = TOP_PAD;
        for s in &self.slots {
            let here = match &s.block {
                Block::User { index: i } | Block::Text { index: i } | Block::Notice { index: i } | Block::Compaction { index: i } => *i == index,
                Block::Group { rows, .. } => rows.contains(&index),
                _ => false,
            };
            if here {
                self.scroll.scroll_to(y - 12.0);
                return;
            }
            y += s.h + BLOCK_GAP;
        }
    }

    pub fn reset_docs(&mut self) {
        self.texts.clear();
        self.users.clear();
        self.details.clear();
        self.selection = None;
        self.dirty = true;
    }

    fn rebuild(&mut self, items: &[Item], running: bool, w: f32) {
        let mut blocks: Vec<Block> = Vec::with_capacity(self.slots.len() + 2);
        let mut rows: Vec<usize> = Vec::new();
        let (mut tools, mut thinks) = (0, 0);
        let mut key: ItemId = 0;
        let mut changes: Vec<FileChange> = Vec::new();
        let mut changes_key: ItemId = 0;

        macro_rules! flush_group {
            () => {
                if !rows.is_empty() {
                    blocks.push(Block::Group { key, rows: std::mem::take(&mut rows), tools: std::mem::take(&mut tools), thinks: std::mem::take(&mut thinks) });
                }
            };
        }
        macro_rules! flush_changes {
            () => {
                if !changes.is_empty() {
                    let added = changes.iter().map(|f| f.added).sum();
                    let removed = changes.iter().map(|f| f.removed).sum();
                    blocks.push(Block::Changes { key: changes_key, files: std::mem::take(&mut changes), added, removed });
                }
            };
        }

        for (i, item) in items.iter().enumerate() {
            match item {
                Item::User { id, .. } => {
                    flush_group!();
                    flush_changes!();
                    changes_key = *id;
                    blocks.push(Block::User { index: i });
                }
                Item::Text { .. } => {
                    flush_group!();
                    blocks.push(Block::Text { index: i });
                }
                Item::Thinking { id, text, signature, .. } => {
                    // Reasoning with nothing to show is only there to be handed back to the provider.
                    if text.is_empty() && !signature.is_empty() {
                        continue;
                    }
                    if rows.is_empty() {
                        key = *id;
                    }
                    rows.push(i);
                    thinks += 1;
                }
                Item::Host { id, .. } => {
                    if rows.is_empty() {
                        key = *id;
                    }
                    rows.push(i);
                }
                Item::Tool(t) => {
                    if rows.is_empty() {
                        key = t.id;
                    }
                    rows.push(i);
                    tools += 1;
                    if let Some(file) = &t.meta.file
                        && t.status == ToolStatus::Done
                    {
                        match changes.iter_mut().find(|f| f.file == *file) {
                            Some(c) => {
                                c.added += t.meta.added;
                                c.removed += t.meta.removed;
                            }
                            None => changes.push(FileChange { file: file.clone(), added: t.meta.added, removed: t.meta.removed }),
                        }
                    }
                }
                Item::Notice { .. } => {
                    flush_group!();
                    blocks.push(Block::Notice { index: i });
                }
                Item::Compaction { .. } => {
                    flush_group!();
                    blocks.push(Block::Compaction { index: i });
                }
                Item::Turn { .. } => {}
            }
        }
        flush_group!();
        if !running {
            flush_changes!();
        }
        let tail_streams = matches!(blocks.last(), Some(Block::Text { .. }));
        if running && !tail_streams {
            blocks.push(Block::Waiting);
        }

        // Keep the measured heights of what did not move, which is nearly everything.
        let mut slots = Vec::with_capacity(blocks.len());
        for (i, block) in blocks.into_iter().enumerate() {
            match self.slots.get(i) {
                Some(old) if std::mem::discriminant(&old.block) == std::mem::discriminant(&block) && same_identity(&old.block, &block) => {
                    slots.push(Slot { block, h: old.h, measured: old.measured });
                }
                _ => {
                    let h = estimate_height(items, &block, w);
                    slots.push(Slot { block, h, measured: false });
                }
            }
        }
        self.slots = slots;
        self.dirty = false;
    }

    /// Draws the timeline into `list`. `col` is the column's x and width, centred by the caller.
    #[allow(clippy::too_many_arguments)]
    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, items: &[Item], list: Rect, col_x: f32, col_w: f32, running: bool, status: Option<&RunStatus>) {
        if self.last_scale != ui.scale || self.last_dark != ui.theme.dark {
            self.reset_docs();
            self.last_scale = ui.scale;
            self.last_dark = ui.theme.dark;
        }
        if running != self.was_running {
            self.was_running = running;
            self.dirty = true;
        }
        if self.dirty {
            self.rebuild(items, running, col_w);
        }
        if (self.last_w - col_w).abs() > 0.5 {
            // Stale heights are only a scrollbar's approximation until a block is next in view.
            for s in &mut self.slots {
                s.measured = false;
                if self.last_w > 0.0 && matches!(s.block, Block::User { .. } | Block::Text { .. }) {
                    s.h = estimate_height(items, &s.block, col_w);
                }
            }
            self.last_w = col_w;
        }

        // Measure what is in view at the current offset, keeping everything on screen where it is.
        let margin = list.h * 0.5;
        let offset = self.scroll.offset;
        let mut y = TOP_PAD;
        let mut shift = 0.0;
        for i in 0..self.slots.len() {
            let h = self.slots[i].h;
            let near = y + h > offset - margin && y < offset + list.h + margin;
            if near {
                let new_h = self.layout_block(ui, items, i, col_w, running);
                if (new_h - h).abs() > 0.25 {
                    self.slots[i].h = new_h;
                    if y + h <= offset {
                        shift += new_h - h;
                    }
                }
                self.slots[i].measured = true;
            }
            y += self.slots[i].h + BLOCK_GAP;
        }
        if shift != 0.0 && !self.scroll.stick {
            self.scroll.shift(shift);
        }
        let content = y - BLOCK_GAP + BOTTOM_PAD;
        self.scroll.update(ui, id, list, content.max(0.0));

        // With a stream pinned to the end, the offset just jumped: anything new in view gets measured now and corrected next frame.
        let offset = self.scroll.offset;
        let saved = ui.clip;
        ui.clip = saved.intersect(&list);
        let mut y = list.y + TOP_PAD - offset;
        let mut clicked_doc = false;
        let mut changed_now = false;
        for i in 0..self.slots.len() {
            let h = self.slots[i].h;
            if y + h >= list.y - 4.0 && y <= list.bottom() + 4.0 {
                if !self.slots[i].measured {
                    let new_h = self.layout_block(ui, items, i, col_w, running);
                    if (new_h - h).abs() > 0.25 {
                        self.slots[i].h = new_h;
                        changed_now = true;
                    }
                    self.slots[i].measured = true;
                }
                self.paint_block(ui, id, items, i, col_x, y, col_w, running, status, &mut clicked_doc);
            }
            y += self.slots[i].h + BLOCK_GAP;
        }
        ui.clip = saved;
        if changed_now {
            ui.animating = true;
        }

        if ui.input.pressed && !clicked_doc && ui.pointer_in(&list) {
            self.selection = None;
        }
        if ui.input.released {
            self.dragging = false;
        }
        if let Some(sel) = self.selection
            && sel.a != sel.b
            && ui.input.keys.iter().any(|k| k.cmd && matches!(k.key, Key::Char('c')))
        {
            self.copy_selection(ui, items, sel);
        }
    }

    fn copy_selection(&self, ui: &mut Ui<'_>, _items: &[Item], sel: Selection) {
        let doc: Option<&Doc> = self.texts.get(&sel.key).map(|d| &d.md.doc).or_else(|| self.users.get(&sel.key)).or_else(|| self.details.get(&sel.key).map(|d| &d.doc));
        if let Some(doc) = doc {
            let (a, b) = (sel.a.min(sel.b), sel.a.max(sel.b));
            if let Some(text) = doc.text().get(a..b) {
                ui.clipboard.set(text);
            }
        }
    }

    fn text_doc(&mut self, ui: &mut Ui<'_>, id: ItemId, text: &str, width: f32, streaming: bool) -> &mut TextDoc {
        let t = ui.theme;
        let scale = ui.scale;
        let palette = Palette { text: t.text_primary, muted: t.text_secondary, code: t.accent_text };
        let entry = self.texts.entry(id).or_insert_with(|| {
            let mut md = MdStream::new(width * scale, CHAT_TYPE, palette);
            md.set_code_colors(Box::new(Highlighter { theme: t, lang: None, state: highlight::State::default(), tokens: Vec::new() }));
            TextDoc { md, fed: 0, finished: false }
        });
        entry.md.doc.set_width(ui.text, width * scale);
        if entry.fed < text.len() {
            entry.md.push(ui.text, &text[entry.fed..]);
            entry.fed = text.len();
        }
        if !streaming && !entry.finished {
            entry.md.finish(ui.text);
            entry.finished = true;
        }
        entry
    }

    /// Lays out one block and returns its height. Only ever called for blocks near the viewport.
    fn layout_block(&mut self, ui: &mut Ui<'_>, items: &[Item], i: usize, w: f32, running: bool) -> f32 {
        let scale = ui.scale;
        match self.slots[i].block.clone() {
            Block::User { index } => {
                let Item::User { id, text, images, .. } = &items[index] else { return 0.0 };
                let doc = self.user_doc(ui, *id, text, w);
                let h = doc.height() / scale + 20.0;
                if images.is_empty() { h } else { h + 22.0 }
            }
            Block::Text { index } => {
                let Item::Text { id, text, .. } = &items[index] else { return 0.0 };
                let streaming = running && index + 1 == items.len();
                self.text_doc(ui, *id, text, w, streaming).md.doc.height() / scale
            }
            Block::Group { key, rows, .. } => {
                let mut h = 0.0;
                let open = self.group_is_open(key, &rows, items, running);
                if rows.len() > 1 {
                    h += ROW_H + 2.0;
                }
                if open || rows.len() == 1 {
                    for &r in &rows {
                        h += ROW_H;
                        let rid = items[r].id();
                        if self.row_open.contains(&rid) {
                            h += self.detail_height(ui, items, r, w) + 6.0;
                        }
                    }
                }
                h
            }
            Block::Changes { files, .. } => 38.0 + files.len() as f32 * 32.0,
            Block::Notice { index } => match &items[index] {
                Item::Notice { text, .. } => (ui.measure(text, &CAPTION) / (w - 40.0).max(1.0)).ceil().max(1.0) * 18.0 + 16.0,
                _ => 34.0,
            },
            Block::Compaction { .. } | Block::Waiting => 28.0,
        }
    }

    fn group_is_open(&self, key: ItemId, rows: &[usize], items: &[Item], running: bool) -> bool {
        if let Some(open) = self.group_open.get(&key) {
            return *open;
        }
        // Open while it is the live end of the run, so what the agent is doing is visible. Closed once done.
        running && rows.last().is_some_and(|r| items[*r..].iter().all(|i| !matches!(i, Item::Text { .. } | Item::User { .. })))
    }

    fn user_doc(&mut self, ui: &mut Ui<'_>, id: ItemId, text: &str, col_w: f32) -> &mut Doc {
        let scale = ui.scale;
        let t = ui.theme;
        let width = (col_w * 0.78 - 24.0).max(40.0) * scale;
        let doc = self.users.entry(id).or_insert_with(|| {
            let mut d = Doc::new(width);
            d.push(ui.text, text, CHAT_TYPE, t.text_primary);
            d
        });
        doc.set_width(ui.text, width);
        doc
    }

    fn detail_height(&mut self, ui: &mut Ui<'_>, items: &[Item], r: usize, w: f32) -> f32 {
        let scale = ui.scale;
        self.detail_doc(ui, items, r, w).map_or(0.0, |d| d.doc.height() / scale)
    }

    /// The expanded body of a row: reasoning as prose, a change as a diff, anything else as its call and its output.
    fn detail_doc(&mut self, ui: &mut Ui<'_>, items: &[Item], r: usize, w: f32) -> Option<&mut DetailDoc> {
        let t = ui.theme;
        let scale = ui.scale;
        let width = (w - 28.0).max(40.0) * scale;
        let item = items.get(r)?;
        let id = item.id();
        let d = self.details.entry(id).or_insert_with(|| DetailDoc { doc: Doc::new(width), fed: 0 });
        d.doc.set_width(ui.text, width);
        match item {
            Item::Thinking { text, .. } => {
                if d.fed < text.len() {
                    d.doc.push(ui.text, &text[d.fed..], PROSE_TYPE, t.text_secondary);
                    d.fed = text.len();
                }
            }
            Item::Host { text, .. } => {
                if d.fed == 0 {
                    d.doc.push(ui.text, text, MONO_SMALL, t.text_secondary);
                    d.fed = text.len().max(1);
                }
            }
            // Arguments are still arriving: start over when they have moved on, but no more often than a frame.
            Item::Tool(tool) if d.fed != tool.args.len() + tool.output.len() + 1 => {
                d.doc.clear();
                d.fed = tool.args.len() + tool.output.len() + 1;
                fill_tool_detail(&mut d.doc, ui.text, tool, &t);
            }
            _ => {}
        }
        Some(d)
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_doc(&mut self, ui: &mut Ui<'_>, key: u64, kind: DocKind, x: f32, y: f32, h: f32, w: f32, clicked: &mut bool) {
        let scale = ui.scale;
        let t = ui.theme;
        let r = rect(x, y, w, h.max(1.0));
        let sel = self.selection.filter(|s| s.key == key);
        let doc: &mut Doc = match kind {
            DocKind::Text => match self.texts.get_mut(&key) {
                Some(d) => &mut d.md.doc,
                None => return,
            },
            DocKind::User => match self.users.get_mut(&key) {
                Some(d) => d,
                None => return,
            },
            DocKind::Detail => match self.details.get_mut(&key) {
                Some(d) => &mut d.doc,
                None => return,
            },
        };
        let hit = ui.input.pointer.map(|(px, py)| doc.hit((px - x) * scale, (py - y) * scale));
        let mut new_sel = None;
        if ui.pointer_in(&r) {
            ui.cursor = CursorKind::Text;
            if ui.input.pressed && let Some(at) = hit {
                *clicked = true;
                new_sel = Some(Selection { key, a: at, b: at });
                self.dragging = true;
            }
        }
        if let (Some(s), true, Some(at)) = (sel, self.dragging && ui.input.down, hit) {
            new_sel = Some(Selection { b: at, ..s });
        }
        if let Some(s) = sel {
            let (a, b) = (s.a.min(s.b), s.a.max(s.b));
            if a < b {
                let view_top = (ui.clip.y - y) * scale;
                let view_bottom = (ui.clip.bottom() - y) * scale;
                let mut rects = Vec::new();
                doc.selection_rects(a..b, view_top..view_bottom, &mut rects);
                for [sx, sy, sw, sh] in rects {
                    ui.fill(rect(x + sx / scale, y + sy / scale, sw / scale, sh / scale), 2.0, t.selection);
                }
            }
        }
        ui.doc(doc, x, y);
        if let Some(s) = new_sel {
            self.selection = Some(s);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_block(&mut self, ui: &mut Ui<'_>, id: u64, items: &[Item], i: usize, x: f32, y: f32, w: f32, running: bool, status: Option<&RunStatus>, clicked: &mut bool) {
        let t = ui.theme;
        let scale = ui.scale;
        let h = self.slots[i].h;
        match self.slots[i].block.clone() {
            Block::User { index } => {
                let Item::User { id: item, text, images, .. } = &items[index] else { return };
                let doc = self.user_doc(ui, *item, text, w);
                let natural = doc.lines.iter().map(|l| l.width).fold(0.0f32, f32::max) / scale;
                let dh = doc.height() / scale;
                let bw = (natural + 24.0).max(if images.is_empty() { 0.0 } else { 160.0 }).min(w * 0.78);
                let b = rect(x + w - bw, y, bw, h);
                let fill = if t.dark { t.accent.with_alpha(0.13) } else { t.accent.with_alpha(0.10) };
                ui.card_radii(b, [OVERLAY_RADIUS, OVERLAY_RADIUS, 3.0, OVERLAY_RADIUS], fill, t.accent.with_alpha(0.18));
                self.paint_doc(ui, *item, DocKind::User, b.x + 12.0, b.y + 9.0, dh, bw - 24.0, clicked);
                if !images.is_empty() {
                    let label = format!("{} 张图片", images.len());
                    ui.icon(Icon::Image, b.x + 19.0, b.bottom() - 18.0, 12.0, t.text_tertiary);
                    ui.label(b.x + 30.0, b.bottom() - 26.0, &label, &CAPTION, t.text_tertiary);
                }
                if ui.pointer_in(&rect(x, y, w, h)) && !running {
                    // To the left of the bubble, where there is room whatever its width.
                    let mut ax = b.x - 28.0;
                    if block_action(ui, fnv(id ^ 0x74, *item), ax, y + 2.0, Icon::Trash, "从上下文删除") {
                        self.actions.push(TimelineAction::DeleteItem(*item));
                    }
                    ax -= 26.0;
                    if block_action(ui, fnv(id ^ 0x75, *item), ax, y + 2.0, Icon::Pencil, "编辑") {
                        self.actions.push(TimelineAction::EditItem(*item));
                    }
                    ax -= 26.0;
                    if block_action(ui, fnv(id ^ 0x76, *item), ax, y + 2.0, Icon::Refresh, "从这里重新生成") {
                        self.actions.push(TimelineAction::Regenerate(*item));
                    }
                    ax -= 26.0;
                    if block_action(ui, fnv(id ^ 0x77, *item), ax, y + 2.0, Icon::Copy, "复制") {
                        ui.clipboard.set(text);
                    }
                }
            }
            Block::Text { index } => {
                let Item::Text { id: item, text, .. } = &items[index] else { return };
                let streaming = running && index + 1 == items.len();
                let d = self.text_doc(ui, *item, text, w, streaming);
                let doc = &mut d.md.doc;
                let view_top = (ui.clip.y - y) * scale;
                let view_bottom = (ui.clip.bottom() - y) * scale;
                let mut spans = Vec::new();
                doc.block_spans(BlockKind::Code, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    ui.card(rect(x, y + top / scale - 8.0, w, (bottom - top) / scale + 16.0), CONTROL_RADIUS, t.code_bg, t.card_stroke);
                }
                doc.block_spans(BlockKind::Quote, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    ui.fill(rect(x + 2.0, y + top / scale, 3.0, (bottom - top) / scale), 1.5, t.accent);
                }
                doc.block_spans(BlockKind::Rule, view_top..view_bottom, &mut spans);
                for [top, bottom, _] in spans.drain(..) {
                    ui.fill(rect(x, y + (top + bottom) * 0.5 / scale, w, 1.0), 0.0, t.divider);
                }
                let caret = if streaming { Some(doc.caret_pos(usize::MAX)) } else { None };
                self.paint_doc(ui, *item, DocKind::Text, x, y, h, w, clicked);
                if let Some((cx, cy, ch)) = caret {
                    let pulse = 0.55 + 0.45 * (ui.time * 5.0).sin();
                    ui.fill(rect(x + cx / scale + 2.0, y + cy / scale + 3.0, 2.0, ch / scale - 6.0), 1.0, t.accent.fade(pulse));
                    ui.animating = true;
                }
                if ui.pointer_in(&rect(x, y, w, h.max(24.0))) && !running {
                    // Over the top right corner of the answer, out of the text's way.
                    let mut ax = x + w - 26.0;
                    if block_action(ui, fnv(id ^ 0x74, *item), ax, y - 2.0, Icon::Trash, "从上下文删除") {
                        self.actions.push(TimelineAction::DeleteItem(*item));
                    }
                    ax -= 26.0;
                    if block_action(ui, fnv(id ^ 0x75, *item), ax, y - 2.0, Icon::Pencil, "编辑") {
                        self.actions.push(TimelineAction::EditItem(*item));
                    }
                    ax -= 26.0;
                    if block_action(ui, fnv(id ^ 0x77, *item), ax, y - 2.0, Icon::Copy, "复制") {
                        ui.clipboard.set(text);
                    }
                }
            }
            Block::Group { key, rows, tools, thinks } => {
                let mut ry = y;
                let open = self.group_is_open(key, &rows, items, running);
                if rows.len() > 1 {
                    let header = rect(x, ry, w, ROW_H);
                    let resp = ui.interact(fnv(id ^ 0x71, key), header);
                    if resp.hover_t > 0.0 {
                        ui.fill(header, 6.0, t.subtle_hover.fade(resp.hover_t));
                    }
                    if resp.hovered {
                        ui.cursor = CursorKind::Hand;
                    }
                    if resp.clicked {
                        self.group_open.insert(key, !open);
                        self.slots[i].measured = false;
                    }
                    let mut title = String::new();
                    if tools > 0 {
                        title.push_str(&format!("调用了 {tools} 个工具"));
                    }
                    if thinks > 0 {
                        if !title.is_empty() {
                            title.push('，');
                        }
                        title.push_str(&format!("思考了 {thinks} 次"));
                    }
                    if title.is_empty() {
                        title.push_str("系统消息");
                    }
                    let tw = ui.label_vcentered(x + 4.0, header, &title, &CAPTION_STRONG, t.text_secondary);
                    ui.icon(if open { Icon::ChevronDown } else { Icon::ChevronRight }, x + 4.0 + tw + 10.0, header.y + ROW_H * 0.5, 9.0, t.text_tertiary);
                    let tokens: u32 = rows.iter().map(|&r| item_tokens(&items[r])).sum();
                    if tokens > 0 {
                        let s = format!("{} tokens", thousands(tokens as u64));
                        ui.label_right(x + w - 6.0, header.y + (ROW_H - MONO_SMALL.line) * 0.5, &s, &MONO_SMALL, t.text_tertiary);
                    }
                    ry += ROW_H + 2.0;
                }
                if open || rows.len() == 1 {
                    for r in rows {
                        ry = self.paint_row(ui, id, items, r, x, ry, w, clicked, i);
                    }
                }
            }
            Block::Changes { key, files, added, removed } => {
                let card = rect(x, y, w, h);
                ui.card(card, OVERLAY_RADIUS, t.card, t.card_stroke);
                let head = rect(x, y, w, 38.0);
                ui.icon(Icon::File, x + 19.0, y + 19.0, 13.0, t.text_secondary);
                ui.label_vcentered(x + 34.0, head, &format!("编辑了 {} 个文件", files.len()), &CAPTION_STRONG, t.text_secondary);
                figures(ui, x + w - 12.0, head, added, removed);
                ui.fill(rect(x, y + 38.0, w, 1.0), 0.0, t.divider);
                let mut ry = y + 38.0;
                for (n, f) in files.iter().enumerate() {
                    let row = rect(x + 1.0, ry, w - 2.0, 32.0);
                    let resp = ui.interact(fnv(id ^ 0x72, key.wrapping_add(n as u64)), row);
                    if resp.hover_t > 0.0 {
                        ui.fill(row, 4.0, t.subtle_hover.fade(resp.hover_t));
                    }
                    if resp.hovered {
                        ui.cursor = CursorKind::Hand;
                    }
                    if resp.clicked {
                        self.actions.push(TimelineAction::OpenReview);
                    }
                    ui.icon(Icon::File, x + 19.0, ry + 16.0, 12.0, t.text_tertiary);
                    let name = f.file.rsplit('/').next().unwrap_or(&f.file);
                    ui.label_ellipsis(x + 34.0, ry + (32.0 - CAPTION.line) * 0.5, w - 160.0, name, &CAPTION, t.text_primary);
                    let right = x + w - 26.0;
                    figures(ui, right, row, f.added, f.removed);
                    ui.icon(Icon::ChevronRight, x + w - 14.0, ry + 16.0, 8.0, t.text_tertiary);
                    ry += 32.0;
                }
            }
            Block::Notice { index } => {
                let Item::Notice { level, text, .. } = &items[index] else { return };
                let (icon, color, bg) = match level {
                    NoticeLevel::Info => (Icon::Info, t.text_secondary, t.card),
                    NoticeLevel::Warning => (Icon::Warning, t.caution, t.caution_bg),
                    NoticeLevel::Error => (Icon::Warning, t.critical, t.critical_bg),
                };
                ui.card(rect(x, y, w, h), CONTROL_RADIUS, bg, t.card_stroke);
                ui.icon(icon, x + 18.0, y + 16.0, 13.0, color);
                let last = index + 1 == items.len() && *level == NoticeLevel::Error && !running;
                ui.label_ellipsis(x + 34.0, y + 8.0, w - if last { 120.0 } else { 44.0 }, text, &CAPTION, t.text_primary);
                if last && ui.button(fnv(id ^ 0x78, index as u64), rect(x + w - 72.0, y + 4.0, 64.0, 26.0), "重试").clicked {
                    self.actions.push(TimelineAction::Retry);
                }
            }
            Block::Compaction { .. } => {
                let mid = y + 14.0;
                let label = "上下文已压缩";
                let lw = ui.measure(label, &CAPTION);
                ui.fill(rect(x, mid, (w - lw) * 0.5 - 10.0, 1.0), 0.0, t.divider);
                ui.fill(rect(x + (w + lw) * 0.5 + 10.0, mid, (w - lw) * 0.5 - 10.0, 1.0), 0.0, t.divider);
                ui.label(x + (w - lw) * 0.5, y + 6.0, label, &CAPTION, t.text_tertiary);
            }
            Block::Waiting => {
                ui.progress_ring(x + 10.0, y + 14.0, 7.0, 1.5);
                let label = match status {
                    Some(RunStatus::Retrying { attempt, detail, .. }) => format!("重试中（第 {attempt} 次）：{detail}"),
                    Some(RunStatus::Connecting) => "连接中…".to_owned(),
                    Some(RunStatus::Compacting) => "正在压缩上下文…".to_owned(),
                    Some(RunStatus::WaitingForUser) => "等待你的回应…".to_owned(),
                    Some(RunStatus::RunningTools) => "运行工具…".to_owned(),
                    _ => "等待响应…".to_owned(),
                };
                ui.label_ellipsis(x + 26.0, y + 6.0, w - 30.0, &label, &CAPTION, t.text_secondary);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_row(&mut self, ui: &mut Ui<'_>, id: u64, items: &[Item], r: usize, x: f32, y: f32, w: f32, clicked: &mut bool, slot: usize) -> f32 {
        let t = ui.theme;
        let rid = items[r].id();
        let row = rect(x, y, w, ROW_H);
        let resp = ui.interact(fnv(id ^ 0x73, rid), row);
        if resp.hover_t > 0.0 {
            ui.fill(row, 6.0, t.subtle_hover.fade(resp.hover_t));
        }
        if resp.hovered {
            ui.cursor = CursorKind::Hand;
        }
        if resp.clicked {
            if !self.row_open.insert(rid) {
                self.row_open.remove(&rid);
            }
            self.slots[slot].measured = false;
        }
        let cy = y + ROW_H * 0.5;
        let mut right = x + w - 6.0;
        match &items[r] {
            Item::Thinking { text, tokens, .. } => {
                ui.icon(Icon::Bulb, x + 13.0, cy, 14.0, t.text_tertiary);
                let lw = ui.label_vcentered(x + 28.0, row, "思考", &CAPTION_STRONG, t.text_secondary);
                let streaming = items.get(r + 1).is_none() && self.was_running;
                if streaming {
                    ui.progress_ring(right - 7.0, cy, 6.0, 1.5);
                }
                let _ = text;
                let s = format!("{} tokens", thousands(*tokens as u64));
                ui.label(x + 28.0 + lw + 8.0, y + (ROW_H - MONO_SMALL.line) * 0.5, &s, &MONO_SMALL, t.text_tertiary);
            }
            Item::Host { .. } => {
                ui.icon(Icon::Info, x + 13.0, cy, 13.0, t.text_tertiary);
                ui.label_vcentered(x + 28.0, row, "系统消息", &CAPTION_STRONG, t.text_tertiary);
            }
            Item::Tool(tool) => {
                let color = match tool.status {
                    ToolStatus::Error | ToolStatus::Denied => t.critical,
                    _ => t.text_tertiary,
                };
                ui.icon(tool_icon(&tool.name), x + 13.0, cy, 14.0, color);
                let lw = ui.label_vcentered(x + 28.0, row, tool_label(&tool.name), &CAPTION_STRONG, t.text_secondary);
                match tool.status {
                    ToolStatus::Streaming | ToolStatus::Running => {
                        ui.progress_ring(right - 7.0, cy, 6.0, 1.5);
                        right -= 20.0;
                    }
                    ToolStatus::Pending => {
                        ui.icon(Icon::Warning, right - 7.0, cy, 12.0, t.caution);
                        right -= 20.0;
                    }
                    ToolStatus::Error => {
                        ui.icon(Icon::Close, right - 6.0, cy, 10.0, t.critical);
                        right -= 18.0;
                    }
                    ToolStatus::Denied | ToolStatus::Cancelled => {
                        let s = if tool.status == ToolStatus::Denied { "已拒绝" } else { "已停止" };
                        let sw = ui.measure(s, &CAPTION);
                        ui.label(right - sw, y + (ROW_H - CAPTION.line) * 0.5, s, &CAPTION, t.text_tertiary);
                        right -= sw + 8.0;
                    }
                    ToolStatus::Done => {}
                }
                if tool.meta.added > 0 || tool.meta.removed > 0 {
                    right = figures(ui, right, row, tool.meta.added, tool.meta.removed) - 8.0;
                } else if tool.tokens > 0 {
                    let s = format!("{} tokens", thousands(tool.tokens as u64));
                    let sw = ui.measure(&s, &MONO_SMALL);
                    ui.label(right - sw, y + (ROW_H - MONO_SMALL.line) * 0.5, &s, &MONO_SMALL, t.text_tertiary);
                    right -= sw + 8.0;
                }
                let args_len = tool.args.len();
                let cached = self.summaries.get(&rid).filter(|(n, _)| *n == args_len);
                if cached.is_none() {
                    self.summaries.insert(rid, (args_len, tool_summary(tool)));
                }
                let line = &self.summaries[&rid].1;
                let lx = x + 28.0 + lw + 8.0;
                ui.label_ellipsis(lx, y + (ROW_H - MONO_SMALL.line) * 0.5, (right - lx).max(0.0), line, &MONO_SMALL, t.text_tertiary);
            }
            _ => {}
        }
        let mut y = y + ROW_H;
        if self.row_open.contains(&rid) {
            let scale = ui.scale;
            let h = self.detail_doc(ui, items, r, w).map_or(0.0, |d| d.doc.height() / scale);
            ui.fill(rect(x + 13.0, y, 1.0, h + 2.0), 0.0, t.divider);
            self.paint_doc(ui, rid, DocKind::Detail, x + 26.0, y + 1.0, h, w - 28.0, clicked);
            y += h + 6.0;
        }
        y
    }
}

/// A small button that shows up when its block is hovered.
fn block_action(ui: &mut Ui<'_>, id: u64, x: f32, y: f32, icon: Icon, tip: &str) -> bool {
    let t = ui.theme;
    let r = rect(x, y, 24.0, 24.0);
    // Within a layer, glyphs draw over every rectangle. One layer up, the button covers the text it sits on.
    let saved = ui.layer;
    ui.layer = fluent::gpu::LAYER_MODAL;
    let resp = ui.interact(id, r);
    ui.acrylic(r, CONTROL_RADIUS, if resp.hovered { t.flyout.mix(t.text_primary, 0.08) } else { t.flyout }, t.flyout_stroke);
    ui.icon(icon, x + 12.0, y + 12.0, 12.0, if resp.hovered { t.text_primary } else { t.text_secondary });
    ui.layer = saved;
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
        ui.tooltip(r, tip.to_owned());
    }
    resp.clicked
}

#[derive(Clone, Copy)]
enum DocKind {
    Text,
    User,
    Detail,
}

fn same_identity(a: &Block, b: &Block) -> bool {
    match (a, b) {
        (Block::User { index: x }, Block::User { index: y }) | (Block::Text { index: x }, Block::Text { index: y }) | (Block::Notice { index: x }, Block::Notice { index: y }) | (Block::Compaction { index: x }, Block::Compaction { index: y }) => x == y,
        (Block::Group { key: x, .. }, Block::Group { key: y, .. }) | (Block::Changes { key: x, .. }, Block::Changes { key: y, .. }) => x == y,
        (Block::Waiting, Block::Waiting) => true,
        _ => false,
    }
}

fn item_tokens(item: &Item) -> u32 {
    match item {
        Item::Thinking { tokens, .. } | Item::Text { tokens, .. } => *tokens,
        Item::Tool(t) => t.tokens,
        _ => 0,
    }
}

/// `+a −r`, right-aligned at `right`. Returns where it starts.
fn figures(ui: &mut Ui<'_>, right: f32, row: Rect, added: u32, removed: u32) -> f32 {
    let t = ui.theme;
    let y = row.y + (row.h - MONO_SMALL.line) * 0.5;
    let minus = format!("−{removed}");
    let plus = format!("+{added}");
    let mw = ui.measure(&minus, &MONO_SMALL);
    ui.label(right - mw, y, &minus, &MONO_SMALL, t.critical);
    let pw = ui.measure(&plus, &MONO_SMALL);
    ui.label(right - mw - 6.0 - pw, y, &plus, &MONO_SMALL, t.success);
    right - mw - 6.0 - pw
}

fn fill_tool_detail(doc: &mut Doc, ctx: &mut fluent::TextCtx, tool: &ToolItem, t: &Theme) {
    let capped = |s: &str| -> usize {
        let mut n = s.len().min(MAX_DETAIL_BYTES);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        n
    };
    if let Some(diff) = &tool.meta.diff {
        for line in diff[..capped(diff)].lines() {
            let color = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("@@") {
                t.text_tertiary
            } else if line.starts_with('+') {
                t.success
            } else if line.starts_with('-') {
                t.critical
            } else {
                t.text_secondary
            };
            doc.push(ctx, line, MONO_SMALL, color);
            doc.push(ctx, "\n", MONO_SMALL, color);
        }
        return;
    }
    let args: String = match serde_json::from_str::<Value>(&tool.args) {
        Ok(Value::Object(m)) => {
            let mut s = String::new();
            for (k, v) in &m {
                let value = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
                s.push_str(k);
                s.push_str(": ");
                s.push_str(&value);
                s.push('\n');
            }
            s
        }
        _ => tool.args.clone(),
    };
    doc.push(ctx, &args[..capped(&args)], MONO_SMALL, t.text_tertiary);
    if !tool.output.is_empty() {
        if !args.ends_with('\n') {
            doc.push(ctx, "\n", MONO_SMALL, t.text_tertiary);
        }
        let color = if tool.status == ToolStatus::Error { t.critical } else { t.text_secondary };
        let n = capped(&tool.output);
        doc.push(ctx, &tool.output[..n], MONO_SMALL, color);
        if n < tool.output.len() {
            doc.push(ctx, "\n…（输出太长，这里只显示开头）", MONO_SMALL, t.text_tertiary);
        }
    }
}
