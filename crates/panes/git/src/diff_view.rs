//! `DiffViewer`: the reference shell's continuous diff. One scroller holding every file, each behind a sticky 32px header,
//! with the file column beside it. It knows nothing of panes or Git: it is handed entries and patches, and asks for what
//! it is about to draw, so chat and the files pane can use it as readily as the review pane does.

use std::collections::{BTreeMap, HashMap, HashSet};

use mewrk_client::prelude::*;

use crate::clipboard::clipboard_leaf;
use crate::code_table::{suppress_whitespace_only, table_leaf, CodeTable, LineMenu};
use crate::diff_highlight::code_language;
use crate::menu::{popover_menu, MenuSection};
use mewrk_ui_common::popover_menu::{use_popover_anchor, AnchorOptions};
use crate::unified_diff::{DiffFile, DiffHunk};

/// Fixed width of the file column while it sits beside the diff.
pub const DIFF_TREE_WIDTH: f32 = 240.0;
/// Below this body width the file column and the diff cannot share the pane.
pub const DIFF_TREE_MIN_WIDTH: f32 = 400.0;
/// Below this body width side-by-side is unreadable and the viewer falls back to unified.
pub const DIFF_SPLIT_MIN_WIDTH: f32 = 560.0;
/// Changed lines past which every file starts collapsed.
pub const DIFF_LARGE_LINE_COUNT: u64 = 5000;
const HEADER_H: f32 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffEntryStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

/// One row of the file column. The column lists what the scope contains, which is not the same as what the patch carries:
/// an untracked or binary file is a row with no hunks behind it, and a file whose patch has not been fetched yet is a row
/// that is still waiting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEntry {
    pub path: String,
    pub old_path: Option<String>,
    pub status: DiffEntryStatus,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    /// A note shown after the path in the file header, such as a staging state.
    pub note: Option<String>,
    pub loading: bool,
    /// Why this file has no patch, when that is expected rather than pending.
    pub unavailable: Option<String>,
}

impl DiffEntry {
    pub fn new(path: impl Into<String>, status: DiffEntryStatus, additions: u64, deletions: u64) -> Self {
        Self { path: path.into(), old_path: None, status, additions, deletions, binary: false, note: None, loading: false, unavailable: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DiffStyle {
    #[default]
    Unified,
    Split,
}

/// What `diff_view` is given.
pub struct DiffViewProps {
    /// Names this viewer among all others.
    pub key: String,
    pub entries: Vec<DiffEntry>,
    /// Parsed patches, keyed by the same paths the entries use.
    pub patches: HashMap<String, Arc<DiffFile>>,
    pub show_tree: bool,
    pub on_can_fit_tree_change: Option<Rc<dyn Fn(&mut App, bool)>>,
    pub diff_style: DiffStyle,
    pub on_can_fit_split_change: Option<Rc<dyn Fn(&mut App, bool)>>,
    pub word_wrap: bool,
    pub word_diff: bool,
    pub hide_whitespace: bool,
    pub on_is_large_change: Option<Rc<dyn Fn(&mut App, bool)>>,
    /// Bumped to fold or unfold every file at once.
    pub fold_all_request: Option<(bool, u64)>,
    pub active_path: Option<String>,
    pub on_select_file: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// A file asked for from outside the viewer, opened and scrolled to the way a click on its row does. Bumping `seq`
    /// asks again; a path not among the entries yet waits for them.
    pub reveal_request: Option<(String, u64)>,
    /// Rows for the file column's and the file header's `⋮`. Asked only while that menu is open. Whether there are rows
    /// at all is `has_file_menu`.
    pub file_menu: Option<Rc<dyn Fn(&str) -> Vec<MenuSection>>>,
    /// Chrome the caller owns at the top and bottom of the file column: a filter, a load-more.
    pub list_header: Option<E>,
    pub list_footer: Option<E>,
    /// Called with a path whose card is open but whose patch is missing. A scope-wide patch cannot carry every file, so
    /// the viewer asks for what it is about to draw. It asks again whenever it draws, so this must be idempotent.
    pub on_need_patch: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// Given, each hunk boundary grows controls that ask for more context.
    pub on_expand_context: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub expanding_paths: HashSet<String>,
    /// Paths whose patch already carries the whole file, so expanding is done.
    pub fully_expanded_paths: HashSet<String>,
    /// Given, a right click on a line of a file is the caller's to answer: it is told the file, the number the line shows
    /// in the gutter, and where the pointer was.
    pub on_line_menu: Option<Rc<dyn Fn(&mut App, &str, u32, (f32, f32))>>,
    pub empty_label: String,
}

impl Default for DiffViewProps {
    fn default() -> Self {
        Self {
            key: String::new(),
            entries: Vec::new(),
            patches: HashMap::new(),
            show_tree: false,
            on_can_fit_tree_change: None,
            diff_style: DiffStyle::Unified,
            on_can_fit_split_change: None,
            word_wrap: true,
            word_diff: true,
            hide_whitespace: false,
            on_is_large_change: None,
            fold_all_request: None,
            active_path: None,
            on_select_file: None,
            reveal_request: None,
            file_menu: None,
            list_header: None,
            list_footer: None,
            on_need_patch: None,
            on_expand_context: None,
            expanding_paths: HashSet::new(),
            fully_expanded_paths: HashSet::new(),
            on_line_menu: None,
            empty_label: String::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeNode {
    Directory { name: String, path: String, children: Vec<TreeNode> },
    File { name: String, path: String, index: usize },
}

/// Groups the flat path list into a directory tree, then folds every chain of single-child directories into one row. A
/// tree of one file per directory is a staircase that says nothing; `src/lib/git` on one row says the same thing in a
/// sixth of the height.
pub fn build_tree(paths: &[&str]) -> Vec<TreeNode> {
    #[derive(Default)]
    struct Dir {
        order: Vec<Item>,
        dirs: BTreeMap<String, usize>,
    }
    enum Item {
        Dir(String),
        File(String, String, usize),
    }
    fn walk(dirs: &[Dir], at: usize, prefix: &str) -> Vec<TreeNode> {
        dirs[at]
            .order
            .iter()
            .map(|item| match item {
                Item::Dir(name) => {
                    let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                    let children = walk(dirs, dirs[at].dirs[name], &path);
                    TreeNode::Directory { name: name.clone(), path, children }
                }
                Item::File(name, path, index) => TreeNode::File { name: name.clone(), path: path.clone(), index: *index },
            })
            .collect()
    }
    let mut dirs = vec![Dir::default()];
    for (index, path) in paths.iter().enumerate() {
        let mut segments: Vec<&str> = path.split('/').collect();
        let name = segments.pop().unwrap_or(path).to_owned();
        let mut at = 0;
        for segment in segments {
            let existing = dirs[at].dirs.get(segment).copied();
            at = match existing {
                Some(next) => next,
                None => {
                    let next = dirs.len();
                    dirs.push(Dir::default());
                    dirs[at].dirs.insert(segment.to_owned(), next);
                    dirs[at].order.push(Item::Dir(segment.to_owned()));
                    next
                }
            };
        }
        dirs[at].order.push(Item::File(name, (*path).to_owned(), index));
    }
    collapse_chains(walk(&dirs, 0, ""))
}

fn collapse_chains(nodes: Vec<TreeNode>) -> Vec<TreeNode> {
    nodes
        .into_iter()
        .map(|node| match node {
            TreeNode::Directory { name, path, children } => {
                let mut children = collapse_chains(children);
                if children.len() == 1
                    && let TreeNode::Directory { .. } = &children[0]
                {
                    let TreeNode::Directory { name: only, path, children } = children.remove(0) else { unreachable!() };
                    return TreeNode::Directory { name: format!("{name}/{only}"), path, children };
                }
                TreeNode::Directory { name, path, children }
            }
            file => file,
        })
        .collect()
}

/// What to say about a file beyond its path and its counts. Added, deleted and renamed files already read as such.
/// Untracked and conflicted are states Git has and the diff cannot show, so those are the ones that get a word.
pub fn status_note(status: DiffEntryStatus) -> Option<&'static str> {
    match status {
        DiffEntryStatus::Untracked => Some(t("未跟踪", "Untracked")),
        DiffEntryStatus::Conflicted => Some(t("冲突", "Conflicted")),
        DiffEntryStatus::TypeChanged => Some(t("类型变更", "Type changed")),
        _ => None,
    }
}

// The file's type icon: `FileIcon`. The files pane owns the real table, and depends on this crate through its diffs, so
// it is passed in rather than called. The shell sets it once at startup.
// TODO: call `set_file_icon(mewrk_pane_files::icons::file_icon)` from the shell.
thread_local! {
    static FILE_ICON: std::cell::Cell<Option<fn(&str, f32) -> E>> = const { std::cell::Cell::new(None) };
}

pub fn set_file_icon(f: fn(&str, f32) -> E) {
    FILE_ICON.with(|slot| slot.set(Some(f)));
}

pub(crate) fn file_icon(path: &str, size: f32) -> E {
    match FILE_ICON.with(std::cell::Cell::get) {
        Some(f) => f(path, size),
        None => icon("File", size).color(theme().text_tertiary),
    }
}

/// `−` is the typographic minus the reference shell uses; a hyphen reads as a dash here.
pub fn stat_counts(additions: u64, deletions: u64) -> E {
    let t_ = theme();
    row().items_center().gap(4.0).shrink0().child(text(format!("+{additions}")).ty(FINE).color(t_.success).nowrap()).child(text(format!("−{deletions}")).ty(FINE).color(t_.critical).nowrap())
}

#[derive(Default)]
struct ViewState {
    can_fit_tree: bool,
    can_fit_split: bool,
    measured: bool,
    collapsed_directories: HashSet<String>,
    /// Files the reader has flipped away from the state they would have defaulted to. Membership is a flip rather than a
    /// state so the default can be decided while drawing, which keeps the asks for missing patches from including files
    /// about to be hidden.
    toggled_files: HashSet<String>,
    /// Set by a fold-all request; overrides the per-file default until the next one.
    fold_all: Option<bool>,
    fold_seq: Option<u64>,
    reveal_seq: Option<u64>,
    /// The file the column asked to jump to, held until its card can be drawn. Scrolling to a card that is still folded
    /// lands the reader on a header with nothing under it; the jump is the last step of opening a file, not the first.
    scroll_target: Option<String>,
    last_large: Option<bool>,
    last_fit_tree: Option<bool>,
    last_fit_split: Option<bool>,
}

impl ViewState {
    /// Whether the reader has asked for a file, folded or not. Every file starts folded: the pane is a reading surface
    /// for one file at a time.
    fn is_requested(&self, path: &str) -> bool {
        let folded_by_default = self.fold_all.unwrap_or(true);
        if self.toggled_files.contains(path) { folded_by_default } else { !folded_by_default }
    }

    /// Asks for a file without flipping one that is already asked for.
    fn request_file(&mut self, path: &str) {
        let folded_by_default = self.fold_all.unwrap_or(true);
        if folded_by_default {
            self.toggled_files.insert(path.to_owned());
        } else {
            self.toggled_files.remove(path);
        }
    }

    fn toggle_file(&mut self, path: &str) {
        if !self.toggled_files.remove(path) {
            self.toggled_files.insert(path.to_owned());
        }
    }
}

fn is_ready(entry: &DiffEntry, patches: &HashMap<String, Arc<DiffFile>>) -> bool {
    entry.binary || entry.unavailable.is_some() || patches.contains_key(&entry.path)
}

/// A hunk is drawn from an `Arc`, and the table built from it is kept while its identity and its look hold.
fn table_key(viewer: &str, patch: &Arc<DiffFile>, style: DiffStyle, word_diff: bool, hide_ws: bool, can_expand: bool) -> u64 {
    hash_key((viewer, patch.path.as_str(), Arc::as_ptr(patch) as usize, style == DiffStyle::Split, word_diff, hide_ws, can_expand))
}

struct Ctx {
    key: String,
    state: Local<ViewState>,
    file_menu: Option<Rc<dyn Fn(&str) -> Vec<MenuSection>>>,
    on_select: Option<Rc<dyn Fn(&mut App, &str)>>,
    scroller: u64,
}

fn file_header(c: &Ctx, entry: &DiffEntry, selected: bool, collapsed: bool, whitespace_only: bool, namespace: &str) -> E {
    let t_ = theme();
    let separator = entry.path.rfind('/');
    let (directory, name) = match separator {
        Some(i) if i > 0 => (&entry.path[..=i], &entry.path[i + 1..]),
        _ => ("", entry.path.as_str()),
    };
    let mut path_spans = Vec::new();
    if let Some(old) = entry.old_path.as_deref().filter(|old| *old != entry.path) {
        path_spans.push(Span { text: Arc::from(format!("{old} → ")), ty: CAPTION, color: Some(t_.text_tertiary) });
    }
    if !directory.is_empty() {
        path_spans.push(Span { text: Arc::from(directory), ty: CAPTION, color: Some(t_.text_secondary) });
    }
    path_spans.push(Span { text: Arc::from(name), ty: CAPTION_STRONG, color: Some(t_.text_primary) });
    let (state, select, path) = (c.state.clone(), c.on_select.clone(), entry.path.clone());
    let mut toggle: E = row()
        .key(("diff-file-toggle", c.key.as_str(), namespace, entry.path.as_str()))
        .items_center()
        .gap(4.0)
        .h_full()
        .flex1()
        .min_w(0.0)
        .aria_expanded(!collapsed)
        .on_click(move |app: &mut App| {
            if let Some(f) = &select {
                f(app, &path);
            }
            state.update(|s| s.toggle_file(&path));
        })
        .child(icon("ChevronDown", 14.0).color(t_.text_tertiary).rotate(if collapsed { -90.0 } else { 0.0 }))
        .child(file_icon(&entry.path, 14.0))
        // Shrinks but does not grow: what is said about a file follows its path, not the far edge.
        .child(spans(path_spans).ellipsis().min_w(0.0).with_style(|s| s.flex_shrink = 1.0).full_text(entry.path.clone()));
    if let Some(note) = status_note(entry.status) {
        let color = match entry.status {
            DiffEntryStatus::Untracked => t_.success,
            DiffEntryStatus::Conflicted => t_.critical,
            DiffEntryStatus::TypeChanged => t_.caution,
            _ => t_.text_tertiary,
        };
        toggle = toggle.child(text(note).ty(FINE).color(color).nowrap());
    }
    if let Some(note) = &entry.note {
        toggle = toggle.child(text(note.clone()).ty(FINE).color(t_.text_tertiary).nowrap());
    }
    if whitespace_only {
        toggle = toggle.child(text(t("仅空白改动", "Whitespace only")).ty(FINE).color(t_.text_tertiary).nowrap());
    }
    toggle = toggle.child(stat_counts(entry.additions, entry.deletions).key(("diff-file-stat", c.key.as_str(), namespace, entry.path.as_str())));
    let mut header: E = row().key(("diff-file-header", c.key.as_str(), namespace, entry.path.as_str())).items_center().gap(4.0).h(HEADER_H).shrink0().bg(if selected { t_.card_secondary } else { t_.layer }).border_bottom(t_.divider).pl(8.0).pr(4.0).child(toggle);
    if let Some(menu) = &c.file_menu {
        let trigger: E = row().center().size(22.0, 22.0).radius(5.0).shrink0().hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).child(icon("MoreVertical", 13.0).color(t_.text_secondary));
        let (menu, path) = (menu.clone(), entry.path.clone());
        header = header.child(popover_menu(("diff-file-menu", c.key.as_str(), namespace, entry.path.as_str()), tf("{name} 的操作", "Actions for {name}", &[("name", &name)]), trigger, move || menu(&path), Align::End));
    }
    header
}

fn tree_rows(c: &Ctx, nodes: &[TreeNode], entries: &[DiffEntry], active: Option<&str>, depth: usize, out: &mut Vec<E>) {
    let t_ = theme();
    for node in nodes {
        let indent = 8.0 + depth as f32 * 12.0;
        match node {
            TreeNode::Directory { name, path, children } => {
                let collapsed = c.state.with(|s| s.collapsed_directories.contains(path));
                let (state, owned) = (c.state.clone(), path.clone());
                out.push(
                    row()
                        .key(("diff-tree-dir", c.key.as_str(), path.as_str()))
                        .items_center()
                        .gap(4.0)
                        .min_h(24.0)
                        .pl(indent)
                        .pr(8.0)
                        .radius(5.0)
                        .shrink0()
                        .hover_bg(t_.subtle_hover)
                        .aria_expanded(!collapsed)
                        .on_click(move |_| {
                            state.update(|s| {
                                if !s.collapsed_directories.remove(&owned) {
                                    s.collapsed_directories.insert(owned.clone());
                                }
                            });
                        })
                        .child(icon("ChevronDown", 13.0).color(t_.text_tertiary).rotate(if collapsed { -90.0 } else { 0.0 }))
                        .child(text(name.clone()).ty(CAPTION).color(t_.text_secondary).ellipsis().flex1()),
                );
                if !collapsed {
                    tree_rows(c, children, entries, active, depth + 1, out);
                }
            }
            TreeNode::File { name, path, index } => {
                let entry = &entries[*index];
                let selected = active == Some(path.as_str());
                let (state, select, owned) = (c.state.clone(), c.on_select.clone(), path.clone());
                let open: E = row()
                    .key(("diff-tree-file", c.key.as_str(), path.as_str()))
                    .items_center()
                    .gap(4.0)
                    .pl(indent)
                    .pr(8.0)
                    .flex1()
                    .min_h(24.0)
                    .full_text(path.clone())
                    .when(selected, |e| e.aria_current(AriaCurrent::True))
                    .on_click(move |app: &mut App| {
                        if let Some(f) = &select {
                            f(app, &owned);
                        }
                        state.update(|s| {
                            s.request_file(&owned);
                            s.scroll_target = Some(owned.clone());
                        });
                    })
                    .child(file_icon(path, 14.0))
                    .child(text(name.clone()).ty(CAPTION).ellipsis().flex1())
                    .child(stat_counts(entry.additions, entry.deletions));
                // A selected row keeps its ground under the pointer.
                let mut r: E = row().items_center().radius(5.0).shrink0().when(selected, |e| e.bg(t_.subtle_pressed)).when(!selected, |e| e.hover_bg(t_.subtle_hover)).child(open);
                if let Some(menu) = &c.file_menu {
                    let trigger: E = row().center().size(18.0, 18.0).radius(4.0).shrink0().hover_bg(t_.subtle_hover).child(icon("MoreVertical", 12.0).color(t_.text_secondary));
                    let menu_key = ("diff-tree-menu", c.key.as_str(), path.as_str());
                    // Stays while its menu is open, with the pointer gone off to the panel.
                    let open_menu = use_popover_anchor(menu_key, AnchorOptions { align_end: true, ..AnchorOptions::default() }).is_open();
                    let (menu, owned) = (menu.clone(), path.clone());
                    let built = popover_menu(menu_key, tf("{name} 的操作", "Actions for {name}", &[("name", name)]), trigger, move || menu(&owned), Align::End);
                    r = r.child(if open_menu { built } else { built.reveal_on_parent_hover() });
                }
                out.push(r);
            }
        }
    }
}

/// The continuous diff, as an element. It reports what it measured through the props' callbacks.
pub fn diff_view(app: &mut App, props: DiffViewProps) -> E {
    let t_ = theme();
    let DiffViewProps { key, entries, patches, show_tree, on_can_fit_tree_change, diff_style, on_can_fit_split_change, word_wrap, word_diff, hide_whitespace, on_is_large_change, fold_all_request, active_path, on_select_file, reveal_request, file_menu, list_header, list_footer, on_need_patch, on_expand_context, expanding_paths, fully_expanded_paths, on_line_menu, empty_label } = props;
    let state = use_state(("diff-view", key.as_str()), || ViewState { can_fit_tree: true, can_fit_split: true, ..ViewState::default() });
    let body_key = hash_key(("diff-view-body", key.as_str()));
    let scroller = hash_key(("diff-view-scroll", key.as_str()));

    // ---- what the frame before measured
    if let Some(r) = rect_of(body_key)
        && r.w > 0.0
    {
        state.update(|s| {
            s.can_fit_tree = r.w >= DIFF_TREE_MIN_WIDTH;
            s.can_fit_split = r.w >= DIFF_SPLIT_MIN_WIDTH;
            s.measured = true;
        });
    }
    let (can_fit_tree, can_fit_split) = state.with(|s| (s.can_fit_tree, s.can_fit_split));
    let report = |f: &Option<Rc<dyn Fn(&mut App, bool)>>, value: bool, slot: fn(&mut ViewState) -> &mut Option<bool>, app: &mut App| {
        let changed = state.update(|s| {
            let last = slot(s);
            let changed = *last != Some(value);
            *last = Some(value);
            changed
        });
        if changed && let Some(f) = f {
            f(app, value);
        }
    };
    let is_large = entries.iter().map(|e| e.additions + e.deletions).sum::<u64>() > DIFF_LARGE_LINE_COUNT;
    report(&on_is_large_change, is_large, |s| &mut s.last_large, app);
    report(&on_can_fit_tree_change, can_fit_tree, |s| &mut s.last_fit_tree, app);
    report(&on_can_fit_split_change, can_fit_split, |s| &mut s.last_fit_split, app);

    // ---- requests from outside
    if let Some((collapsed, seq)) = fold_all_request {
        state.update(|s| {
            if s.fold_seq != Some(seq) {
                s.fold_seq = Some(seq);
                s.fold_all = Some(collapsed);
                s.toggled_files.clear();
            }
        });
    }
    if let Some((path, seq)) = &reveal_request {
        let due = state.with(|s| s.reveal_seq != Some(*seq));
        if due && entries.iter().any(|e| e.path == *path) {
            state.update(|s| {
                s.reveal_seq = Some(*seq);
                s.request_file(path);
                s.scroll_target = Some(path.clone());
            });
            if let Some(f) = &on_select_file {
                f(app, path);
            }
        }
    }

    // ---- a requested card with no patch behind it is a request, not an empty file
    if let Some(f) = &on_need_patch {
        for entry in &entries {
            if !state.with(|s| s.is_requested(&entry.path)) || entry.loading || entry.binary || entry.unavailable.is_some() || patches.contains_key(&entry.path) {
                continue;
            }
            f(app, &entry.path);
        }
    }

    // ---- side by side needs room for two code columns; below it the setting stays on but the viewer draws what fits
    let effective_style = if can_fit_split { diff_style } else { DiffStyle::Unified };
    let tree_visible = show_tree && can_fit_tree;

    let c = Ctx { key: key.clone(), state: state.clone(), file_menu, on_select: on_select_file.clone(), scroller };

    // ---- the jump, once its card is open
    let target = state.with(|s| s.scroll_target.clone());
    if let Some(target) = target {
        let gone = !entries.iter().any(|e| e.path == target) || !state.with(|s| s.is_requested(&target));
        if gone {
            state.update(|s| s.scroll_target = None);
        } else if entries.iter().any(|e| e.path == target && is_ready(e, &patches))
            && let (Some(card), Some(list), Some((offset, _, _))) = (rect_of(("diff-file", key.as_str(), target.as_str())), rect_of(scroller), scroll_state(scroller))
        {
            scroll_request(scroller, ScrollRequest::To(card.y - list.y + offset));
            state.update(|s| s.scroll_target = None);
        }
    }

    // ---- the cards
    let mut cards: Vec<E> = Vec::with_capacity(entries.len() + 1);
    if entries.is_empty() {
        cards.push(text(empty_label).ty(CAPTION).color(t_.text_secondary).text_center().py(32.0).px(16.0));
    }
    let mut sticky: Option<E> = None;
    let list_rect = rect_of(scroller);
    for entry in &entries {
        let patch = patches.get(&entry.path);
        let requested = state.with(|s| s.is_requested(&entry.path));
        let collapsed = !(requested && is_ready(entry, &patches));
        let hunks: Vec<DiffHunk> = match patch {
            Some(p) if !collapsed || hide_whitespace => p.hunks.iter().filter_map(|h| if hide_whitespace { suppress_whitespace_only(h) } else { Some(h.clone()) }).collect(),
            _ => Vec::new(),
        };
        let whitespace_only = hide_whitespace && patch.is_some_and(|p| !p.hunks.is_empty()) && hunks.is_empty();
        let card_key = ("diff-file", key.as_str(), entry.path.as_str());
        let selected = active_path.as_deref() == Some(entry.path.as_str());
        let mut card: E = col().key(card_key).shrink0().when(a11y_active(), |e| e.role(Role::Region).aria_label(tf("{path} 差异", "{path} diff", &[("path", &entry.path)]))).child(file_header(&c, entry, selected, collapsed, whitespace_only, ""));
        if !collapsed {
            let note = |s: String| -> E { text(s).ty(FINE).color(t_.text_secondary).px(10.0).py(8.0) };
            let mut body: E = col().shrink0();
            if let Some(reason) = &entry.unavailable {
                body = body.child(note(reason.clone()));
            } else if entry.binary {
                body = body.child(note(t("二进制文件，无法显示差异", "Binary file; no diff to show").to_owned()));
            } else {
                if let Some(p) = patch.filter(|p| p.mode_change_only) {
                    body = body.child(note(tf("仅文件模式变更：{from} → {to}", "File mode only: {from} → {to}", &[("from", &p.old_mode.as_deref().unwrap_or("?")), ("to", &p.new_mode.as_deref().unwrap_or("?"))])));
                }
                if whitespace_only {
                    body = body.child(note(t("这个文件只有空白改动，已按设置隐藏。", "This file changes only whitespace, which is hidden.").to_owned()));
                }
                if !hunks.is_empty()
                    && let Some(p) = patch
                {
                    let can_expand = on_expand_context.is_some() && !fully_expanded_paths.contains(&entry.path);
                    let expanding = expanding_paths.contains(&entry.path);
                    let tkey = table_key(&key, p, effective_style, word_diff, hide_whitespace, can_expand);
                    // The key is this file's address, so it stays taken for as long as the table is kept: a later patch
                    // landing on it would otherwise be drawn as this one.
                    let _ = use_state(("code-table-patch", tkey), || p.clone());
                    let table = use_state(("code-table", tkey), || CodeTable::of_hunks(&hunks, effective_style == DiffStyle::Split, word_diff, code_language(&entry.path), can_expand));
                    let expand = on_expand_context.as_ref().map(|f| {
                        let (f, path) = (f.clone(), entry.path.clone());
                        Rc::new(move |app: &mut App| f(app, &path)) as Handler<App>
                    });
                    let line_menu: Option<LineMenu> = on_line_menu.as_ref().map(|f| {
                        let (f, path) = (f.clone(), entry.path.clone());
                        Rc::new(move |app: &mut App, line: u32, point: (f32, f32)| f(app, &path, line, point)) as LineMenu
                    });
                    body = body.child(table_leaf(hash_key((key.as_str(), entry.path.as_str())), table, word_wrap, can_expand, expanding, expand, line_menu));
                }
            }
            if patch.is_some_and(|p| p.incomplete) {
                body = body.child(note(t("差异过大，已被截断。", "The diff was truncated.").to_owned()));
            }
            card = card.child(body);
            // A card scrolled under the top keeps its header on screen, until the next card's pushes it away.
            if let (Some(list), Some(r)) = (list_rect, rect_of(card_key))
                && r.y < list.y
                && r.bottom() > list.y + HEADER_H * 0.5
            {
                let y = (r.bottom() - list.y - HEADER_H).min(0.0);
                // Upstream's is the one header, stuck by CSS. This copy is for the eye: a reader has the real one, in its section.
                sticky = Some(file_header(&c, entry, selected, collapsed, whitespace_only, "sticky").absolute().top(y).left(0.0).right(0.0).raised().aria_hidden(true));
            }
        }
        cards.push(card);
    }

    // ---- the column
    let mut body: E = row().key(body_key).flex1().min_w(0.0).min_h(0.0).clip().child(clipboard_leaf());
    if tree_visible {
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        let tree = build_tree(&paths);
        let mut rows = Vec::new();
        tree_rows(&c, &tree, &entries, active_path.as_deref(), 0, &mut rows);
        let list: E = col().key(("diff-tree", key.as_str())).flex1().min_h(0.0).py(6.0).px(4.0).scroll_y().role(Role::Navigation).aria_label(t("变更文件", "Changed files")).children(rows);
        body = body.child(col().w(DIFF_TREE_WIDTH).shrink0().min_h(0.0).border_right(t_.divider).bg(t_.card_secondary).child(list_header).child(list).child(list_footer));
    }
    let scroll: E = col().key(c.scroller).flex1().min_h(0.0).scroll_y().children(cards);
    let mut main: E = col().flex1().min_w(0.0).min_h(0.0).clip().child(scroll);
    if let Some(s) = sticky {
        main = main.child(s);
    }
    body.child(main)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir<'a>(n: &'a TreeNode) -> (&'a str, &'a Vec<TreeNode>) {
        match n {
            TreeNode::Directory { name, children, .. } => (name, children),
            TreeNode::File { .. } => panic!("not a directory"),
        }
    }

    #[test]
    fn a_tree_folds_single_child_chains_into_one_row() {
        let tree = build_tree(&["src/lib/git/a.ts", "src/lib/git/b.ts", "src/app.ts", "README.md"]);
        assert_eq!(tree.len(), 2);
        let (name, children) = dir(&tree[0]);
        assert_eq!(name, "src");
        assert_eq!(children.len(), 2);
        let (name, leaves) = dir(&children[0]);
        assert_eq!(name, "lib/git", "a chain of single-child directories is one row");
        assert_eq!(leaves.len(), 2);
        assert!(matches!(&children[1], TreeNode::File { name, .. } if name == "app.ts"));
        assert!(matches!(&tree[1], TreeNode::File { name, index: 3, .. } if name == "README.md"));
    }

    #[test]
    fn a_whole_chain_down_to_one_file_keeps_its_directory_row() {
        let tree = build_tree(&["a/b/c.ts"]);
        let (name, children) = dir(&tree[0]);
        assert_eq!(name, "a/b");
        assert!(matches!(&children[0], TreeNode::File { .. }));
    }

    #[test]
    fn files_start_folded_a_fold_all_overrides_and_a_flip_flips() {
        let mut s = ViewState::default();
        assert!(!s.is_requested("a"), "every file starts folded");
        s.toggle_file("a");
        assert!(s.is_requested("a"));
        s.request_file("a");
        assert!(s.is_requested("a"), "asking for one already asked for does not flip it");
        s.toggled_files.clear();
        s.fold_all = Some(false);
        assert!(s.is_requested("b"), "unfold all");
        s.toggle_file("b");
        assert!(!s.is_requested("b"));
        s.request_file("b");
        assert!(s.is_requested("b"));
        s.fold_all = Some(true);
        s.toggled_files.clear();
        assert!(!s.is_requested("b"), "fold all");
    }

    #[test]
    fn only_states_git_has_and_a_diff_cannot_show_get_a_word() {
        assert_eq!(status_note(DiffEntryStatus::Untracked), Some("未跟踪"));
        assert_eq!(status_note(DiffEntryStatus::Conflicted), Some("冲突"));
        assert_eq!(status_note(DiffEntryStatus::TypeChanged), Some("类型变更"));
        assert_eq!(status_note(DiffEntryStatus::Added), None);
        assert_eq!(status_note(DiffEntryStatus::Renamed), None);
    }
}
