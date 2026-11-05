//! The Files pane: a lazily loaded tree, a filter over everything under it, and a read-only preview.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::text::{hash_str, FxMap};
use fluent::theme::*;
use fluent::{rect, CursorKind, Icon, Key, MenuItem, Rect, Scroll, TextEdit, Ui};
use ignore::WalkBuilder;
use parking_lot::{Condvar, Mutex};

use crate::code::{CodeDoc, View};
use crate::{highlight, pinned_scroll, sub, Wake, RIGHT_GUTTER};

pub(crate) const PATH_H: f32 = 32.0;
pub(crate) const FILTER_H: f32 = 32.0;
pub(crate) const ROW_H: f32 = 24.0;
pub(crate) const LIST_PAD: f32 = 4.0;
const INDENT: f32 = 12.0;
const ROOT_INDENT: f32 = 8.0;

/// Entries of one directory, past which it says it has more.
const MAX_LIST_ENTRIES: usize = 200_000;
pub(crate) const MAX_PREVIEW_BYTES: u64 = 8 * 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
/// Paths the filter holds. A home directory would otherwise take it all the way to the bottom of the disk.
const MAX_INDEX: usize = 300_000;
const MAX_RESULTS: usize = 200;
/// Dependencies and build output: enough of them to eat the walker's whole budget, and never what anyone is looking for.
const SKIPPED: [&str; 11] = [".git", "node_modules", "target", ".next", ".venv", "venv", "__pycache__", ".cache", ".turbo", ".gradle", ".idea"];
const IMAGES: [&str; 11] = ["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "avif", "heic", "tif", "tiff"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Dir,
    File,
    /// A socket, or a link to nothing: listed, but with nothing to open.
    Other,
}

const NO_NODE: u32 = u32::MAX;

struct Entry {
    name: Box<str>,
    kind: Kind,
    link: bool,
    ignored: bool,
    /// This directory's node, once it has been opened.
    child: u32,
}

struct Listing {
    entries: Vec<Entry>,
    truncated: bool,
}

enum FsJob {
    List { node: u32, path: PathBuf, all_ignored: bool, epoch: u64 },
    Read { path: PathBuf, epoch: u64 },
}

enum FsMsg {
    Listed { node: u32, epoch: u64, result: Result<Listing, String> },
    Preview { epoch: u64, body: PreviewBody },
}

enum PreviewBody {
    Loading,
    Text { doc: Box<CodeDoc>, truncated: bool },
    Binary,
    Image,
    Failed(String),
}

fn fs_worker(jobs: Receiver<FsJob>, out: Sender<FsMsg>, wake: Wake) {
    while let Ok(job) = jobs.recv() {
        let msg = match job {
            FsJob::List { node, path, all_ignored, epoch } => FsMsg::Listed { node, epoch, result: list_dir(&path, all_ignored) },
            FsJob::Read { path, epoch } => FsMsg::Preview { epoch, body: read_preview(&path) },
        };
        if out.send(msg).is_err() {
            return;
        }
        wake();
    }
}

fn list_dir(path: &Path, all_ignored: bool) -> Result<Listing, String> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for item in std::fs::read_dir(path).map_err(|e| e.to_string())? {
        if entries.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned().into_boxed_str();
        let (kind, link) = match item.file_type() {
            Ok(ty) if ty.is_symlink() => {
                let target = std::fs::metadata(item.path());
                (target.map_or(Kind::Other, |m| if m.is_dir() { Kind::Dir } else if m.is_file() { Kind::File } else { Kind::Other }), true)
            }
            Ok(ty) if ty.is_dir() => (Kind::Dir, false),
            Ok(ty) if ty.is_file() => (Kind::File, false),
            _ => (Kind::Other, false),
        };
        entries.push(Entry { name, kind, link, ignored: all_ignored, child: NO_NODE });
    }
    // Asking the walker for one level and seeing what it leaves out is how the `ignore` crate answers for every rule
    // Git has (parents, the global file, info/exclude) without being taught any of them here. Anything under an ignored
    // directory is ignored with it, but the walker only tests each path on its own, so that comes from the parent.
    if !all_ignored {
        let kept: HashSet<OsString> = WalkBuilder::new(path).max_depth(Some(1)).hidden(false).ignore(false).build().flatten().filter(|e| e.depth() == 1).map(|e| e.file_name().to_owned()).collect();
        for e in &mut entries {
            e.ignored = &*e.name == ".git" || !kept.contains(std::ffi::OsStr::new(&*e.name));
        }
    }
    // Directories first, then by name without case. Folding is why the keys are cached.
    entries.sort_by_cached_key(|e| (e.kind != Kind::Dir, e.name.to_lowercase()));
    Ok(Listing { entries, truncated })
}

fn is_image(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGES.iter().any(|i| e.eq_ignore_ascii_case(i)))
}

fn read_preview(path: &Path) -> PreviewBody {
    if is_image(path) {
        return PreviewBody::Image;
    }
    let read = || -> std::io::Result<(Vec<u8>, u64)> {
        let file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        let mut bytes = Vec::with_capacity(size.min(MAX_PREVIEW_BYTES) as usize);
        file.take(MAX_PREVIEW_BYTES).read_to_end(&mut bytes)?;
        Ok((bytes, size))
    };
    let (mut bytes, size) = match read() {
        Ok(read) => read,
        Err(e) => return PreviewBody::Failed(e.to_string()),
    };
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0) {
        return PreviewBody::Binary;
    }
    let truncated = size > MAX_PREVIEW_BYTES;
    if truncated && let Some(last) = bytes.iter().rposition(|b| *b == b'\n') {
        bytes.truncate(last + 1);
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let doc = CodeDoc::of_text(&String::from_utf8_lossy(&bytes), highlight::lang_of(name));
    PreviewBody::Text { doc: Box::new(doc), truncated }
}

// ---------------------------------------------------------------- the filter's walker

struct Item {
    /// Relative to the walk's root, with `/` between names.
    path: Box<str>,
    /// `path` in lower case, when that differs from it.
    lower: Option<Box<str>>,
    is_dir: bool,
}

#[derive(Clone)]
pub(crate) struct Hit {
    pub(crate) path: Box<str>,
    pub(crate) is_dir: bool,
}

struct WalkMsg {
    epoch: u64,
    hits: Vec<Hit>,
    done: bool,
    truncated: bool,
}

#[derive(Default)]
struct WalkReq {
    epoch: u64,
    root: PathBuf,
    query: String,
    /// Bumped by a refresh, which makes what has been walked so far worthless.
    index_gen: u64,
    quit: bool,
}

#[derive(Default)]
struct WalkShared {
    req: Mutex<WalkReq>,
    cv: Condvar,
}

/// Scored as Mewrk scores it, deterministically: from 0, each matched character in the base name is +8 and outside it +2;
/// the query as a substring of the base name +12, and the base name starting with it +20 more; each unmatched character
/// between two matched ones −1; each level of depth −1, so shallower paths win a tie; a file +3 over a directory.
fn score(query: &str, path: &str, is_dir: bool) -> Option<i32> {
    let base_start = path.rfind('/').map_or(0, |i| i + 1);
    let mut chars = path.char_indices().enumerate();
    let mut total = 0i32;
    let mut last: Option<usize> = None;
    for wanted in query.chars() {
        let (i, at) = chars.find(|(_, (_, c))| *c == wanted).map(|(i, (at, _))| (i, at))?;
        total += if at >= base_start { 8 } else { 2 };
        if let Some(last) = last {
            total -= (i - last - 1) as i32;
        }
        last = Some(i);
    }
    let base = &path[base_start..];
    if base.contains(query) {
        total += 12;
    }
    if base.starts_with(query) {
        total += 20;
    }
    total -= path.matches('/').count() as i32 + 1;
    if !is_dir {
        total += 3;
    }
    Some(total.max(0))
}

#[derive(Default)]
struct Index {
    root: Option<PathBuf>,
    index_gen: u64,
    items: Vec<Item>,
    truncated: bool,
}

impl Index {
    /// Everything under `root` that Git does not ignore. False when a newer request made the walk pointless.
    fn fill(&mut self, root: &Path, index_gen: u64, shared: &WalkShared, out: &Sender<WalkMsg>, wake: &Wake) -> bool {
        self.root = None;
        self.items.clear();
        self.truncated = false;
        let walk = WalkBuilder::new(root).hidden(false).ignore(false).filter_entry(|e| e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| SKIPPED.contains(&n))).build();
        let mut last_post = Instant::now();
        for (seen, item) in walk.enumerate() {
            let Ok(item) = item else { continue };
            if item.depth() == 0 {
                continue;
            }
            let Ok(relative) = item.path().strip_prefix(root) else { continue };
            let path: Box<str> = relative.to_string_lossy().replace('\\', "/").into_boxed_str();
            let lower = path.to_lowercase();
            let lower = (lower != *path).then(|| lower.into_boxed_str());
            self.items.push(Item { path, lower, is_dir: item.file_type().is_some_and(|t| t.is_dir()) });
            if self.items.len() >= MAX_INDEX {
                self.truncated = true;
                break;
            }
            if seen % 4096 != 0 {
                continue;
            }
            let (epoch, query) = {
                let req = shared.req.lock();
                if req.quit || req.root != root || req.index_gen != index_gen {
                    return false;
                }
                (req.epoch, req.query.clone())
            };
            // A long walk still answers as it goes, so that typing into a big tree shows something straight away.
            if !query.is_empty() && last_post.elapsed() > Duration::from_millis(120) {
                last_post = Instant::now();
                if let Some(hits) = self.search(&query, epoch, shared) {
                    let _ = out.send(WalkMsg { epoch, hits, done: false, truncated: false });
                    wake();
                }
            }
        }
        self.root = Some(root.to_owned());
        self.index_gen = index_gen;
        true
    }

    /// The best matches for `query`. None when a newer query has arrived, whose answer is the one wanted.
    fn search(&self, query: &str, epoch: u64, shared: &WalkShared) -> Option<Vec<Hit>> {
        let query = query.to_lowercase();
        let mut scored = Vec::new();
        for (i, item) in self.items.iter().enumerate() {
            if i % 8192 == 0 && shared.req.lock().epoch != epoch {
                return None;
            }
            if let Some(s) = score(&query, item.lower.as_deref().unwrap_or(&item.path), item.is_dir) {
                scored.push((s, i));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| self.items[a.1].path.cmp(&self.items[b.1].path)));
        scored.truncate(MAX_RESULTS);
        Some(scored.into_iter().map(|(_, i)| Hit { path: self.items[i].path.clone(), is_dir: self.items[i].is_dir }).collect())
    }
}

fn walker(shared: Arc<WalkShared>, out: Sender<WalkMsg>, wake: Wake) {
    let mut index = Index::default();
    let mut seen = 0;
    loop {
        let (root, index_gen) = {
            let mut req = shared.req.lock();
            while req.epoch == seen && !req.quit {
                shared.cv.wait(&mut req);
            }
            if req.quit {
                return;
            }
            // Nobody is asking, so nothing here is worth a walk.
            if req.query.is_empty() {
                seen = req.epoch;
                continue;
            }
            (req.root.clone(), req.index_gen)
        };
        if (index.root.as_deref() != Some(root.as_path()) || index.index_gen != index_gen) && !index.fill(&root, index_gen, &shared, &out, &wake) {
            continue;
        }
        // Read after the walk, because typing goes on during one.
        let (epoch, query) = {
            let req = shared.req.lock();
            if req.root != root || req.index_gen != index_gen {
                continue;
            }
            (req.epoch, req.query.clone())
        };
        seen = epoch;
        if query.is_empty() {
            continue;
        }
        if let Some(hits) = index.search(&query, epoch, &shared) {
            if out.send(WalkMsg { epoch, hits, done: true, truncated: index.truncated }).is_err() {
                return;
            }
            wake();
        }
    }
}

// ---------------------------------------------------------------- the pane

struct Node {
    path: PathBuf,
    ignored: bool,
    open: bool,
    listing: Option<Listing>,
    error: Option<String>,
    requested: bool,
    /// Nodes of the subdirectories opened so far, kept by name so that a refresh's new listing can find them again.
    children: FxMap<Box<str>, u32>,
}

#[derive(Clone, Copy)]
struct TreeRow {
    node: u32,
    /// `NO_NODE` for a row that only says something about the directory.
    entry: u32,
    depth: u16,
    open: bool,
}

struct Preview {
    path: PathBuf,
    body: PreviewBody,
    scroll: Scroll,
    epoch: u64,
}

enum RowClick {
    Primary,
    Menu(Rect),
}

struct EntryRow<'a> {
    id: u64,
    indent: f32,
    name: &'a str,
    detail: &'a str,
    kind: Kind,
    /// Tree rows keep a chevron's column, and filter results do not.
    tree: bool,
    open: bool,
    link: bool,
    dim: bool,
    selected: bool,
}

fn entry_row(ui: &mut Ui<'_>, row: &EntryRow<'_>, r: Rect, pressed: &mut u64) -> Option<RowClick> {
    let t = ui.theme;
    let kebab = rect(r.right() - 22.0, r.y + 2.0, 20.0, 20.0);
    let hovered = ui.pointer_in(&r);
    let resp = ui.interact(row.id, rect(r.x, r.y, (kebab.x - r.x).max(0.0), r.h));
    if resp.pressed {
        *pressed = row.id;
    }
    let mut click = None;
    if resp.clicked && row.kind != Kind::Other {
        click = Some(RowClick::Primary);
    }
    if resp.hovered && row.kind != Kind::Other {
        ui.cursor = CursorKind::Hand;
    }
    if ui.input.right_pressed && hovered {
        let (x, y) = ui.input.pointer.unwrap_or((r.x, r.y));
        click = Some(RowClick::Menu(rect(x, y, 1.0, 1.0)));
    }
    let fill = if row.selected { t.subtle_hover.mix(t.subtle_pressed, resp.press_t) } else { t.subtle_hover.fade(if hovered { resp.hover_t.max(0.5) } else { resp.hover_t }) };
    if fill.a > 0 {
        ui.fill(r, CONTROL_RADIUS, fill);
    }
    // Drawn only under the pointer, as Mewrk does, and for as long as a press on it is in flight: its `interact` is what
    // clears that press.
    let kebab_id = row.id ^ 0x55;
    let mut right = r.right() - 6.0;
    if hovered || ui.mem.active == kebab_id {
        let k = ui.icon_button(kebab_id, kebab, Icon::MoreV, Some(t.text_secondary));
        if k.pressed {
            *pressed = kebab_id;
        }
        if k.clicked {
            click = Some(RowClick::Menu(kebab));
        }
        right = kebab.x - 2.0;
    }

    let cy = r.y + r.h * 0.5;
    let mut x = r.x + row.indent;
    let name_color = if row.dim { t.text_tertiary } else if row.kind == Kind::Other { t.text_secondary } else { t.text_primary };
    if row.tree {
        if row.kind == Kind::Dir {
            ui.icon(if row.open { Icon::ChevronDown } else { Icon::ChevronRight }, x + 7.0, cy, 10.0, t.text_tertiary);
        }
        x += 18.0;
    }
    let icon_color = if row.dim { t.text_tertiary } else { t.text_secondary };
    ui.icon(if row.kind == Kind::Dir { Icon::Folder } else { Icon::File }, x + 7.0, cy, 14.0, icon_color);
    x += 18.0;
    let y = r.y + (r.h - CAPTION.line) * 0.5;
    let room = (right - x).max(0.0);
    let name_w = ui.measure(row.name, &CAPTION).min(room);
    ui.label_ellipsis(x, y, name_w, row.name, &CAPTION, name_color);
    x += name_w + 6.0;
    if row.link && x + 12.0 < right {
        ui.icon(Icon::ArrowRight, x + 4.0, cy, 9.0, t.text_tertiary);
        x += 14.0;
    }
    if !row.detail.is_empty() && right - x > 16.0 {
        ui.label_ellipsis(x, r.y + (r.h - FINE.line) * 0.5, right - x, row.detail, &FINE, t.text_tertiary);
    }
    click
}

pub struct FilesPane {
    root: PathBuf,
    dir: PathBuf,
    wake: Wake,
    tx: Sender<FsJob>,
    rx: Receiver<FsMsg>,
    walker: Option<(Arc<WalkShared>, Receiver<WalkMsg>)>,

    /// Generation of the tree. A listing asked for under an older one is dropped.
    epoch: u64,
    nodes: Vec<Node>,
    rows: Vec<TreeRow>,
    rows_dirty: bool,
    scroll: Scroll,
    crumbs: Vec<(String, PathBuf)>,
    selected: Option<(u32, Box<str>)>,

    filter: TextEdit,
    query: String,
    walk_gen: u64,
    index_gen: u64,
    hits: Vec<Hit>,
    hits_for: u64,
    hits_done: bool,
    hits_truncated: bool,
    hits_scroll: Scroll,

    preview: Option<Preview>,
    preview_gen: u64,
    wrap: bool,

    menu: Option<(PathBuf, Rect)>,
    mention: Option<PathBuf>,
    /// The row a press landed on, which may scroll out of view before its release does.
    pressed: u64,
}

impl FilesPane {
    pub fn new(root: PathBuf, wake: Wake) -> Self {
        let (tx, jobs) = channel();
        let (out, rx) = channel();
        let thread_wake = wake.clone();
        // With no thread there is simply nothing coming back: the pane stays empty rather than the app going down.
        let _ = std::thread::Builder::new().name("files-fs".to_owned()).spawn(move || fs_worker(jobs, out, thread_wake));
        let mut pane = Self {
            dir: root.clone(),
            root,
            wake,
            tx,
            rx,
            walker: None,
            epoch: 0,
            nodes: Vec::new(),
            rows: Vec::new(),
            rows_dirty: true,
            scroll: pinned_scroll(),
            crumbs: Vec::new(),
            selected: None,
            filter: TextEdit::new(),
            query: String::new(),
            walk_gen: 0,
            index_gen: 0,
            hits: Vec::new(),
            hits_for: 0,
            hits_done: false,
            hits_truncated: false,
            hits_scroll: pinned_scroll(),
            preview: None,
            preview_gen: 0,
            wrap: true,
            menu: None,
            mention: None,
            pressed: 0,
        };
        pane.go_to(pane.root.clone());
        pane
    }

    pub fn set_root(&mut self, root: PathBuf) {
        self.root = root.clone();
        self.preview = None;
        self.selected = None;
        self.mention = None;
        self.menu = None;
        self.filter.clear();
        self.query.clear();
        self.hits.clear();
        self.go_to(root);
    }

    /// A file the user asked to mention in the chat, once.
    pub fn take_mention(&mut self) -> Option<PathBuf> {
        self.mention.take()
    }

    fn go_to(&mut self, dir: PathBuf) {
        self.epoch += 1;
        self.dir = dir.clone();
        self.nodes.clear();
        self.selected = None;
        self.crumbs.clear();
        let mut at = PathBuf::new();
        for part in dir.components() {
            at.push(part);
            let name = part.as_os_str().to_string_lossy().into_owned();
            self.crumbs.push((name, at.clone()));
        }
        self.nodes.push(Node { path: dir, ignored: false, open: true, listing: None, error: None, requested: false, children: FxMap::default() });
        self.request_list(0);
        self.rows_dirty = true;
        self.scroll = pinned_scroll();
        // Told even with nothing to search for, so that a walk of the old directory stops.
        if !self.query.is_empty() || self.walker.is_some() {
            self.search();
        }
    }

    fn request_list(&mut self, node: u32) {
        let n = &mut self.nodes[node as usize];
        n.requested = true;
        let _ = self.tx.send(FsJob::List { node, path: n.path.clone(), all_ignored: n.ignored, epoch: self.epoch });
    }

    fn refresh(&mut self) {
        for node in 0..self.nodes.len() as u32 {
            if self.nodes[node as usize].open {
                self.request_list(node);
            }
        }
        self.index_gen += 1;
        if !self.query.is_empty() {
            self.search();
        }
    }

    /// Hands the current query to the walker, starting it the first time anyone filters.
    fn search(&mut self) {
        if self.walker.is_none() {
            let shared = Arc::new(WalkShared::default());
            let (out, rx) = channel();
            let (thread_shared, wake) = (shared.clone(), self.wake.clone());
            if std::thread::Builder::new().name("files-walk".to_owned()).spawn(move || walker(thread_shared, out, wake)).is_err() {
                return;
            }
            self.walker = Some((shared, rx));
        }
        let Some((shared, _)) = &self.walker else { return };
        let mut req = shared.req.lock();
        req.epoch += 1;
        req.root = self.dir.clone();
        req.query = self.query.clone();
        req.index_gen = self.index_gen;
        self.walk_gen = req.epoch;
        self.hits_done = false;
        drop(req);
        shared.cv.notify_one();
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                FsMsg::Listed { node, epoch, result } => {
                    if epoch != self.epoch {
                        continue;
                    }
                    let n = &mut self.nodes[node as usize];
                    match result {
                        Ok(mut listing) => {
                            if !n.children.is_empty() {
                                for e in &mut listing.entries {
                                    if let Some(&child) = n.children.get(&e.name) {
                                        e.child = child;
                                    }
                                }
                            }
                            n.listing = Some(listing);
                            n.error = None;
                        }
                        Err(e) => {
                            n.error = Some(e);
                            n.listing = None;
                        }
                    }
                    self.rows_dirty = true;
                }
                FsMsg::Preview { epoch, body } => {
                    if let Some(p) = &mut self.preview
                        && p.epoch == epoch
                    {
                        p.body = body;
                    }
                }
            }
        }
        if let Some((_, rx)) = &self.walker {
            while let Ok(msg) = rx.try_recv() {
                if msg.epoch <= self.walk_gen {
                    // A partial answer to an older query beats an empty list while the newer one is on its way, but a
                    // finished one never gives way to a partial one for the same query.
                    if msg.epoch == self.walk_gen {
                        self.hits_done = msg.done;
                        self.hits_truncated = msg.truncated;
                    }
                    if msg.epoch >= self.hits_for {
                        self.hits_for = msg.epoch;
                        self.hits = msg.hits;
                    }
                }
            }
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows_dirty = false;
        self.rows.clear();
        let mut stack: Vec<(u32, u32, u16)> = vec![(0, 0, 0)];
        while let Some(top) = stack.last_mut() {
            let (node, i, depth) = *top;
            let n = &self.nodes[node as usize];
            let say = TreeRow { node, entry: NO_NODE, depth, open: false };
            if n.error.is_some() {
                self.rows.push(say);
                stack.pop();
                continue;
            }
            let Some(listing) = &n.listing else {
                stack.pop();
                continue;
            };
            if listing.entries.is_empty() {
                self.rows.push(say);
                stack.pop();
                continue;
            }
            let Some(e) = listing.entries.get(i as usize) else {
                if listing.truncated {
                    self.rows.push(say);
                }
                stack.pop();
                continue;
            };
            top.1 += 1;
            let open = e.child != NO_NODE && self.nodes[e.child as usize].open;
            let child = e.child;
            self.rows.push(TreeRow { node, entry: i, depth, open });
            if open {
                stack.push((child, 0, depth + 1));
            }
        }
    }

    fn toggle(&mut self, node: u32, entry: u32) {
        let (name, child) = {
            let e = &self.nodes[node as usize].listing.as_ref().map(|l| &l.entries[entry as usize]);
            let Some(e) = e else { return };
            (e.name.clone(), e.child)
        };
        let child = if child != NO_NODE {
            child
        } else {
            let parent = &self.nodes[node as usize];
            let id = self.nodes.len() as u32;
            let ignored = parent.ignored || parent.listing.as_ref().is_some_and(|l| l.entries[entry as usize].ignored);
            let path = parent.path.join(&*name);
            self.nodes.push(Node { path, ignored, open: false, listing: None, error: None, requested: false, children: FxMap::default() });
            let parent = &mut self.nodes[node as usize];
            parent.children.insert(name, id);
            if let Some(l) = &mut parent.listing {
                l.entries[entry as usize].child = id;
            }
            id
        };
        let n = &mut self.nodes[child as usize];
        n.open = !n.open;
        if n.open && !n.requested {
            self.request_list(child);
        }
        self.rows_dirty = true;
    }

    fn open_file(&mut self, path: PathBuf) {
        self.preview_gen += 1;
        let epoch = self.preview_gen;
        let _ = self.tx.send(FsJob::Read { path: path.clone(), epoch });
        self.preview = Some(Preview { path, body: PreviewBody::Loading, scroll: pinned_scroll(), epoch });
    }

    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        self.drain();
        ui.with_clip(r, |ui| {
            if self.preview.is_some() {
                self.show_preview(ui, id, r);
            } else {
                self.show_tree(ui, id, r);
            }
            self.row_menu(ui, id);
        });
        // A row that scrolled away while pressed never saw its release, and an `active` nobody clears freezes the whole UI.
        if !ui.input.down && self.pressed != 0 {
            if ui.mem.active == self.pressed {
                ui.mem.active = 0;
            }
            self.pressed = 0;
        }
    }

    fn show_tree(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        self.path_row(ui, id, rect(r.x, r.y, r.w, PATH_H));
        let list = rect(r.x, r.y + PATH_H, r.w, (r.h - PATH_H - FILTER_H).max(0.0));
        self.filter_band(ui, id, rect(r.x, r.bottom() - FILTER_H, r.w, FILTER_H));
        if self.query.is_empty() {
            self.tree_list(ui, id, list);
        } else {
            self.hit_list(ui, id, list);
        }
        ui.fill(rect(r.x, r.y + PATH_H - 1.0, r.w, 1.0), 0.0, t.divider);
    }

    /// The directory being looked at. A long path gives way at the front, because its end is what says where this is.
    fn path_row(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        let up = rect(r.x + 5.0, r.y + 5.0, 22.0, 22.0);
        let parent = self.dir.parent().map(Path::to_owned);
        let color = if parent.is_some() { t.text_secondary } else { t.text_disabled };
        if ui.icon_button(sub(id, 20), up, Icon::ArrowLeft, Some(color)).clicked
            && let Some(parent) = parent
        {
            self.go_to(parent);
            return;
        }
        let refresh = rect(r.right() - RIGHT_GUTTER - 22.0, r.y + 5.0, 22.0, 22.0);
        if ui.icon_button(sub(id, 21), refresh, Icon::Refresh, Some(t.text_secondary)).clicked {
            self.refresh();
        }

        let (left, right) = (up.right() + 4.0, refresh.x - 4.0);
        let sep = ui.measure("/", &CAPTION);
        let dots = ui.measure("…/", &CAPTION);
        let n = self.crumbs.len();
        let widths: Vec<f32> = self.crumbs.iter().map(|c| ui.measure(&c.0, &CAPTION) + 4.0).collect();
        let mut first = n;
        let mut used = 0.0;
        while first > 0 {
            let w = widths[first - 1] + if first < n { sep } else { 0.0 };
            if used + w + dots > right - left && first < n {
                break;
            }
            used += w;
            first -= 1;
        }
        let mut x = left;
        if first > 0 {
            x += ui.label_vcentered(x, r, "…/", &CAPTION, t.text_tertiary);
        }
        let mut go = None;
        for (i, &width) in widths.iter().enumerate().skip(first) {
            let w = width.min((right - x).max(0.0));
            let seg = rect(x, r.y + 5.0, w, 22.0);
            let last = i + 1 == n;
            let resp = ui.interact(sub(id, 300 + i as u64), seg);
            if !last && resp.hover_t > 0.0 {
                ui.fill(seg, CONTROL_RADIUS, t.subtle_hover.fade(resp.hover_t));
            }
            if !last && resp.hovered {
                ui.cursor = CursorKind::Hand;
            }
            if !last && resp.clicked {
                go = Some(self.crumbs[i].1.clone());
            }
            let color = if last || resp.hovered { t.text_primary } else { t.text_secondary };
            ui.label_ellipsis(x + 2.0, r.y + (r.h - CAPTION.line) * 0.5, (w - 4.0).max(0.0), &self.crumbs[i].0, &CAPTION, color);
            x += w;
            // The root's own name is its separator.
            if !last && !self.crumbs[i].0.ends_with(['/', '\\']) {
                ui.label_vcentered(x, r, "/", &CAPTION, t.text_tertiary);
                x += sep;
            }
        }
        if let Some(go) = go {
            self.go_to(go);
        }
    }

    fn tree_list(&mut self, ui: &mut Ui<'_>, id: u64, list: Rect) {
        let t = ui.theme;
        if self.rows_dirty {
            self.rebuild_rows();
        }
        let content = self.rows.len() as f32 * ROW_H + LIST_PAD * 2.0;
        self.scroll.stick = false;
        self.scroll.update(ui, sub(id, 3), list, content);
        let offset = self.scroll.offset;
        let first = ((offset - LIST_PAD) / ROW_H).floor().max(0.0) as usize;
        let last = (((offset + list.h - LIST_PAD) / ROW_H).ceil().max(0.0) as usize).min(self.rows.len());
        let row_x = list.x + 4.0;
        let row_w = (list.w - 4.0 - RIGHT_GUTTER).max(0.0);
        let base = sub(id, 1000);

        let mut event = None;
        let mut pressed = self.pressed;
        ui.with_clip(list, |ui| {
            for i in first..last {
                let row = self.rows[i];
                let y = list.y + LIST_PAD + i as f32 * ROW_H - offset;
                let r = rect(row_x, y, row_w, ROW_H);
                let node = &self.nodes[row.node as usize];
                let indent = ROOT_INDENT + row.depth as f32 * INDENT;
                if row.entry == NO_NODE {
                    let (text, color) = match (&node.error, &node.listing) {
                        (Some(e), _) => (format!("无法读取：{e}"), t.critical),
                        (None, Some(l)) if l.truncated => ("目录里的项目过多，只列出了一部分".to_owned(), t.text_tertiary),
                        _ => ("空文件夹".to_owned(), t.text_tertiary),
                    };
                    ui.label_ellipsis(r.x + indent + 18.0, y + (ROW_H - CAPTION.line) * 0.5, (row_w - indent - 18.0).max(0.0), &text, &CAPTION, color);
                    continue;
                }
                let Some(e) = node.listing.as_ref().map(|l| &l.entries[row.entry as usize]) else { continue };
                let selected = self.selected.as_ref().is_some_and(|(n, name)| *n == row.node && **name == *e.name);
                let entry = EntryRow { id: base ^ hash_str(&e.name) ^ (row.node as u64).wrapping_mul(0x9E37_79B9), indent, name: &e.name, detail: "", kind: e.kind, tree: true, open: row.open, link: e.link, dim: e.ignored, selected };
                if let Some(click) = entry_row(ui, &entry, r, &mut pressed) {
                    event = Some((row.node, row.entry, click));
                }
            }
        });
        self.pressed = pressed;

        match event {
            Some((node, entry, RowClick::Primary)) => {
                let Some(e) = self.nodes[node as usize].listing.as_ref().map(|l| &l.entries[entry as usize]) else { return };
                if e.kind == Kind::Dir {
                    self.toggle(node, entry);
                } else {
                    let name = e.name.clone();
                    let path = self.nodes[node as usize].path.join(&*name);
                    self.selected = Some((node, name));
                    self.open_file(path);
                }
            }
            Some((node, entry, RowClick::Menu(anchor))) => {
                let n = &self.nodes[node as usize];
                let Some(e) = n.listing.as_ref().map(|l| &l.entries[entry as usize]) else { return };
                self.open_row_menu(ui, id, n.path.join(&*e.name), anchor);
            }
            None => {}
        }
    }

    fn hit_list(&mut self, ui: &mut Ui<'_>, id: u64, list: Rect) {
        let t = ui.theme;
        let extra = if self.hits_truncated { 1 } else { 0 };
        let content = (self.hits.len() + extra) as f32 * ROW_H + LIST_PAD * 2.0;
        self.hits_scroll.stick = false;
        self.hits_scroll.update(ui, sub(id, 4), list, content);
        let offset = self.hits_scroll.offset;
        let first = ((offset - LIST_PAD) / ROW_H).floor().max(0.0) as usize;
        let last = (((offset + list.h - LIST_PAD) / ROW_H).ceil().max(0.0) as usize).min(self.hits.len());
        let row_x = list.x + 4.0;
        let row_w = (list.w - 4.0 - RIGHT_GUTTER).max(0.0);
        let base = sub(id, 2000);
        let message = if !self.hits.is_empty() {
            None
        } else if self.hits_done {
            Some("没有匹配的文件")
        } else {
            Some("正在搜索…")
        };
        if let Some(message) = message {
            ui.label_vcentered(row_x + ROOT_INDENT, rect(list.x, list.y + LIST_PAD, list.w, ROW_H), message, &CAPTION, t.text_tertiary);
            return;
        }

        let mut event = None;
        let mut pressed = self.pressed;
        ui.with_clip(list, |ui| {
            for i in first..last {
                let hit = &self.hits[i];
                let y = list.y + LIST_PAD + i as f32 * ROW_H - offset;
                let (dir, name) = hit.path.rsplit_once('/').unwrap_or(("", &hit.path));
                let entry = EntryRow { id: base ^ hash_str(&hit.path), indent: ROOT_INDENT, name, detail: dir, kind: if hit.is_dir { Kind::Dir } else { Kind::File }, tree: false, open: false, link: false, dim: false, selected: false };
                if let Some(click) = entry_row(ui, &entry, rect(row_x, y, row_w, ROW_H), &mut pressed) {
                    event = Some((i, click));
                }
            }
            if self.hits_truncated {
                let y = list.y + LIST_PAD + self.hits.len() as f32 * ROW_H - offset;
                ui.label_vcentered(row_x + ROOT_INDENT, rect(row_x, y, row_w, ROW_H), "结果过多，只显示了一部分", &CAPTION, t.text_tertiary);
            }
        });
        self.pressed = pressed;

        match event {
            Some((i, RowClick::Primary)) => {
                let path = self.dir.join(&*self.hits[i].path);
                if self.hits[i].is_dir {
                    self.filter.clear();
                    self.query.clear();
                    self.hits.clear();
                    self.go_to(path);
                } else {
                    self.open_file(path);
                }
            }
            Some((i, RowClick::Menu(anchor))) => {
                let path = self.dir.join(&*self.hits[i].path);
                self.open_row_menu(ui, id, path, anchor);
            }
            None => {}
        }
    }

    fn filter_band(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        ui.fill(rect(r.x, r.y, r.w, 1.0), 0.0, t.divider);
        ui.icon(Icon::Search, r.x + 16.0, r.y + r.h * 0.5, 13.0, t.text_tertiary);
        let clear = rect(r.right() - RIGHT_GUTTER - 18.0, r.y + 7.0, 18.0, 18.0);
        let has_text = !self.filter.text.is_empty();
        let right = if has_text { clear.x - 4.0 } else { r.right() - RIGHT_GUTTER };
        let edit = rect(r.x + 30.0, r.y + (r.h - CAPTION.line) * 0.5, (right - r.x - 30.0).max(0.0), CAPTION.line);
        let edit_id = sub(id, 22);
        self.filter.show_bare(ui, edit_id, edit, "筛选文件…", &CAPTION, true);
        let escape = ui.mem.focus == edit_id && ui.input.keys.iter().any(|k| k.key == Key::Escape);
        if (has_text && ui.icon_button(sub(id, 23), clear, Icon::Close, Some(t.text_tertiary)).clicked) || escape {
            self.filter.clear();
        }
        let query = self.filter.text.trim();
        if query != self.query {
            self.query = query.to_owned();
            self.hits_scroll = pinned_scroll();
            if self.query.is_empty() {
                self.hits.clear();
                self.hits_for = 0;
            }
            self.search();
        }
    }

    fn show_preview(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        let view = View { cw: ui.measure("0", &MONO_BODY), wrap: self.wrap, words: false };
        let bar = rect(r.x, r.y, r.w, PATH_H);
        let back = rect(r.x + 5.0, r.y + 5.0, 22.0, 22.0);
        if ui.icon_button(sub(id, 24), back, Icon::ArrowLeft, Some(t.text_secondary)).clicked {
            self.preview = None;
            return;
        }
        let kebab = rect(r.right() - RIGHT_GUTTER - 22.0, r.y + 5.0, 22.0, 22.0);
        let menu_id = sub(id, 11);
        if ui.icon_button(sub(id, 25), kebab, Icon::MoreV, Some(t.text_secondary)).clicked {
            ui.toggle_popup(menu_id);
        }
        let Some(preview) = &mut self.preview else { return };
        let shown = preview.path.strip_prefix(&self.root).unwrap_or(&preview.path).to_string_lossy().into_owned();
        let (dir, name) = shown.rsplit_once(['/', '\\']).unwrap_or(("", &shown));
        let x = back.right() + 6.0;
        let room = (kebab.x - 4.0 - x).max(0.0);
        let y = bar.y + (bar.h - CAPTION.line) * 0.5;
        let name_w = ui.measure(name, &CAPTION_STRONG).min(room);
        ui.label_ellipsis(x, y, name_w, name, &CAPTION_STRONG, t.text_primary);
        if !dir.is_empty() && room - name_w > 24.0 {
            ui.label_ellipsis(x + name_w + 8.0, bar.y + (bar.h - FINE.line) * 0.5, room - name_w - 8.0, dir, &FINE, t.text_tertiary);
        }
        ui.fill(rect(r.x, bar.bottom() - 1.0, r.w, 1.0), 0.0, t.divider);

        let mut body = rect(r.x, bar.bottom(), r.w, (r.h - PATH_H).max(0.0));
        let say = |ui: &mut Ui<'_>, text: &str, color| ui.label_centered(rect(body.x, body.y + 32.0, body.w, 24.0), text, &CAPTION, color);
        match &mut preview.body {
            PreviewBody::Loading => say(ui, "正在读取…", t.text_tertiary),
            PreviewBody::Binary => say(ui, "二进制文件，无法显示", t.text_tertiary),
            PreviewBody::Image => say(ui, "图片文件，暂时无法在这里预览", t.text_tertiary),
            PreviewBody::Failed(e) => {
                let message = format!("无法读取：{e}");
                say(ui, &message, t.critical);
            }
            PreviewBody::Text { doc, truncated } => {
                if *truncated {
                    let band = rect(body.x, body.y, body.w, 26.0);
                    ui.fill(band, 0.0, t.card_secondary);
                    ui.label_vcentered(band.x + 10.0, band, "文件过大，只显示了开头 8 MB", &CAPTION, t.text_secondary);
                    body.y += band.h;
                    body.h = (body.h - band.h).max(0.0);
                }
                if doc.rows.is_empty() {
                    ui.label_centered(rect(body.x, body.y + 32.0, body.w, 24.0), "空文件", &CAPTION, t.text_tertiary);
                } else {
                    let cols = doc.cols_for((body.w - RIGHT_GUTTER).max(0.0), view.cw);
                    doc.layout(cols, view.wrap);
                    preview.scroll.stick = false;
                    preview.scroll.update(ui, sub(id, 5), body, doc.height() as f32);
                    let top = body.y as f64 - preview.scroll.offset as f64;
                    ui.with_clip(body, |ui| doc.paint(ui, body.x, body.w, top, view));
                }
            }
        }

        if ui.mem.open_popup == menu_id {
            let path = preview.path.clone();
            let items = [MenuItem::new("复制路径").icon(Icon::Copy), MenuItem::new("在会话中提及"), MenuItem::new("自动换行").checked(self.wrap).separated()];
            match ui.menu(menu_id, kebab, true, 170.0, &items) {
                Some(0) => ui.clipboard.set(&path.to_string_lossy()),
                Some(1) => self.mention = Some(path),
                Some(2) => self.wrap = !self.wrap,
                _ => {}
            }
        }
    }

    fn open_row_menu(&mut self, ui: &mut Ui<'_>, id: u64, path: PathBuf, anchor: Rect) {
        let menu_id = sub(id, 10);
        let same = self.menu.as_ref().is_some_and(|(p, _)| *p == path);
        if ui.mem.open_popup == menu_id && same {
            ui.mem.open_popup = 0;
        } else {
            self.menu = Some((path, anchor));
            ui.mem.open_popup = menu_id;
        }
    }

    fn row_menu(&mut self, ui: &mut Ui<'_>, id: u64) {
        let menu_id = sub(id, 10);
        if ui.mem.open_popup != menu_id {
            return;
        }
        let Some((path, anchor)) = self.menu.clone() else { return };
        let items = [MenuItem::new("复制路径").icon(Icon::Copy), MenuItem::new("在会话中提及")];
        // A kebab's menu hangs back from the pane's edge, and a right click's opens from the pointer.
        match ui.menu(menu_id, anchor, anchor.w > 1.0, 170.0, &items) {
            Some(0) => ui.clipboard.set(&path.to_string_lossy()),
            Some(1) => self.mention = Some(path),
            _ => {}
        }
    }
}

impl Drop for FilesPane {
    fn drop(&mut self) {
        if let Some((shared, _)) = &self.walker {
            shared.req.lock().quit = true;
            shared.cv.notify_one();
        }
    }
}

#[cfg(test)]
impl FilesPane {
    /// What the list shows, as `(depth, name, ignored)`.
    pub(crate) fn tree(&self) -> Vec<(u16, String, bool)> {
        self.rows
            .iter()
            .filter(|r| r.entry != NO_NODE)
            .map(|r| {
                let e = &self.nodes[r.node as usize].listing.as_ref().unwrap().entries[r.entry as usize];
                (r.depth, e.name.to_string(), e.ignored)
            })
            .collect()
    }
    pub(crate) fn row_y(&self, name: &str) -> Option<f32> {
        let i = self.rows.iter().position(|r| r.entry != NO_NODE && &*self.nodes[r.node as usize].listing.as_ref().unwrap().entries[r.entry as usize].name == name)?;
        Some(LIST_PAD + i as f32 * ROW_H + ROW_H * 0.5 - self.scroll.offset)
    }
    pub(crate) fn hits(&self) -> Vec<String> {
        self.hits.iter().map(|h| h.path.to_string()).collect()
    }
    pub(crate) fn hits_done(&self) -> bool {
        self.hits_done
    }
    pub(crate) fn filter_text(&self) -> &str {
        &self.filter.text
    }
    pub(crate) fn preview_state(&self) -> Option<&'static str> {
        self.preview.as_ref().map(|p| match &p.body {
            PreviewBody::Loading => "loading",
            PreviewBody::Text { truncated: true, .. } => "truncated",
            PreviewBody::Text { .. } => "text",
            PreviewBody::Binary => "binary",
            PreviewBody::Image => "image",
            PreviewBody::Failed(_) => "failed",
        })
    }
    pub(crate) fn preview_doc(&self) -> Option<&CodeDoc> {
        match &self.preview.as_ref()?.body {
            PreviewBody::Text { doc, .. } => Some(doc),
            _ => None,
        }
    }
    pub(crate) fn preview_scroll_max(&self) -> f32 {
        self.preview.as_ref().map_or(0.0, |p| p.scroll.max_offset())
    }
    pub(crate) fn current_dir(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn is_loading(&self) -> bool {
        self.nodes[0].listing.is_none() && self.nodes[0].error.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_prefers_the_base_name_and_shallow_paths() {
        let s = |q: &str, p: &str| score(q, p, false).unwrap();
        assert!(s("main", "src/main.rs") > s("main", "main_helpers/lib.rs"), "a match in the base name beats one in the directory");
        assert!(s("main", "src/main.rs") > s("main", "a/b/c/main.rs"), "shallower wins a tie");
        assert!(s("mrs", "src/main.rs") > 0, "fuzzy: a subsequence matches");
        assert!(score("xyz", "src/main.rs", false).is_none());
        assert!(score("中文", "docs/中文说明.md", false).is_some());
        assert!(s("rs", "lib.rs") > score("rs", "lib.rs", true).unwrap(), "a file over a directory");
    }
}
