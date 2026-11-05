//! The file manager's state and everything that changes it (upstream's `FilesPane.tsx`, the part above its JSX).
//!
//! Each tab is a page: a directory in the tree column beside one open file, on any machine the reader can reach and
//! anywhere on it. Directories are listed one at a time, on expansion, and kept per machine and path, so every page
//! shares what any page has read. A request counter fences every response, so a listing answered after a refresh is
//! dropped instead of contradicting the newer one.
//!
//! A view here runs every frame, so React's effects become `sync`: run at the top of the view, doing only what has not
//! been done. State lives in `App::local` keyed by pane id rather than in `use_state`, because answers from the host
//! and timers reach it with nothing but `&mut App`, and because a pane keeps its pages while it is off screen.

use crate::browser::*;
use crate::links::{external_http_url, local_link_target, PathOpenRequest};
use crate::notebook::notebook_markdown;
use crate::viewers::*;
pub use mewrk_client::file_mention::FileMention;
use mewrk_client::model::{RunTarget, SshMachineConfig};
use mewrk_client::prelude::*;
use mewrk_ui_common::context_menu::{ContextMenuAnchor, ContextMenuItem, ContextMenuSection, MenuLoader};
use mewrk_ui_common::popover_menu::{PopoverMenuItem, PopoverMenuSection};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

/// Row indent, matching the reference shell's `8 + depth * 12`.
pub const INDENT_PER_LEVEL: f32 = 12.0;
pub const ROOT_INDENT: f32 = 8.0;
/// Fixed width of the tree column while it sits beside a file.
pub const TREE_COLUMN_WIDTH: f32 = 240.0;
/// Below this body width the tree and the file cannot share the pane.
pub const TREE_SIDE_BY_SIDE_MIN_WIDTH: f32 = 400.0;
pub const FILTER_DEBOUNCE_MS: u64 = 120;
/// How long a revealed row stays lit after the tree scrolls to it.
pub const REVEAL_FLASH_MS: u64 = 1200;
/// How soon a second click on a folder has to follow the first for the two to go into it. A single click opens or closes
/// the folder only once this has passed without one, so a double click never opens the folder on its way in.
pub const DOUBLE_CLICK_MS: u64 = 200;
/// How much longer an opening folder waits for its listing before it opens on a "reading" row.
pub const EXPAND_WAIT_MS: u64 = 300;
/// How long the pointer rests on a folder before the folder is read ahead of a click.
pub const PREFETCH_HOVER_MS: u64 = 80;
pub const SHOW_TREE_STORAGE_KEY: &str = "mewrk.filesPane.showTree";
pub const ROW_H: f32 = 24.0;

/// A file the pane has been asked to show from somewhere outside it.
#[derive(Clone, Debug, PartialEq)]
pub struct FilesPaneOpenRequest {
    pub machine: BrowseMachine,
    /// Absolute on that machine.
    pub path: String,
    /// The line the reference named, scrolled to and lit once the file is on screen.
    pub line: Option<u32>,
    /// Bumped per request, so asking twice for the same file asks twice.
    pub nonce: u64,
    /// The pane was not open when the file was asked for: it opens on the file alone, with the tree folded away.
    pub collapse_tree: bool,
    /// The file gets a page of its own, kept like one opened from a menu.
    pub new_page: bool,
    /// `path` is a folder to browse rather than a file to show.
    pub folder: bool,
}

/// One of the conversation's workspaces, as the pane offers it.
#[derive(Clone, Debug, PartialEq)]
pub struct FilesPaneWorkspace {
    /// The number the conversation addresses it by, 1-based.
    pub number: u32,
    pub machine: BrowseMachine,
    /// Its root as the conversation records it: the worktree standing in for it when there is one.
    pub path: String,
}

#[derive(Clone)]
pub struct FilesPaneProps {
    pub pane_id: String,
    /// The conversation's workspaces, in its numbering; the first is where a new page opens.
    pub workspaces: Vec<FilesPaneWorkspace>,
    /// The SSH catalog, for naming machines and reading addresses.
    pub ssh_machines: Vec<SshMachineConfig>,
    /// Whether this computer is Windows: its paths print with backslashes, and it may have WSL.
    pub host_windows: bool,
    /// False while the pane is not on screen; nothing is fetched until it is.
    pub active: bool,
    /// A file the timeline asked for, or `None` while nothing has been clicked.
    pub open_request: Option<FilesPaneOpenRequest>,
    /// Told once `open_request` has been acted on, so a later mount does not act on it again.
    pub on_open_request_handled: Option<Rc<dyn Fn(&mut App, u64)>>,
    /// Given, a line of a file shown as text can be mentioned in the conversation from its menu.
    pub on_mention: Option<Rc<dyn Fn(&mut App, FileMention)>>,
}

/// What the shell's `side_pane` frames: the pane's own title bar, the rows of its `⋮`, and its body.
pub struct FilesPaneView {
    pub header: E,
    pub menu_sections: Vec<PopoverMenuSection>,
    pub body: E,
}

/// A place on a machine.
#[derive(Clone, Debug)]
pub struct Location {
    pub machine: BrowseMachine,
    pub path: String,
}

impl Location {
    pub fn new(machine: &BrowseMachine, path: impl Into<String>) -> Self {
        Self { machine: machine.clone(), path: path.into() }
    }

    pub fn key(&self) -> String {
        location_key(&self.machine, &self.path)
    }
}

pub fn same_location(left: Option<&Location>, right: Option<&Location>) -> bool {
    match (left, right) {
        (Some(l), Some(r)) => same_machine(&l.machine, &r.machine) && l.path == r.path,
        (None, None) => true,
        _ => false,
    }
}

/// Whether `path` is `root` or under it, on the same machine.
pub fn within(root: &Location, location: &Location) -> bool {
    same_machine(&root.machine, &location.machine) && relative_path(&root.path, &location.path).is_some()
}

/// `path` with `from` swapped for `to` when it is `from` or under it.
pub fn moved(path: &str, from: &str, to: &str) -> Option<String> {
    let relative = relative_path(from, path)?;
    Some(if relative.is_empty() { to.to_owned() } else { join_path(to, &relative) })
}

#[derive(Clone)]
pub enum DirectoryState {
    Loading,
    Ready(Rc<Listing>),
    Error(String),
}

/// A directory's answer, with entries shared so a row costs a pointer rather than three strings.
pub struct Listing {
    pub path: String,
    pub entries: Vec<Rc<BrowseEntry>>,
    pub truncated: bool,
}

#[derive(Clone)]
pub enum ViewerState {
    Loading,
    Ready { content: Rc<str>, binary: bool, truncated: bool },
    Error(String),
}

/// One file read as bytes: a picture, a PDF, a recording, a font. Held as a `data:` URL.
#[derive(Clone)]
pub enum MediaState {
    Loading,
    Ready(Arc<str>),
    TooLarge,
    Error(String),
}

#[derive(Clone)]
pub enum SearchState {
    Idle,
    Loading,
    Ready { root: String, matches: Rc<Vec<BrowseSearchMatch>>, truncated: bool },
    Error(String),
}

/// One page of the pane. `preview` marks the page the transcript reuses: following a second path replaces it rather than
/// stacking another page. Double-clicking the tab, or a file in its tree, keeps it.
#[derive(Clone)]
pub struct FilePage {
    pub id: String,
    /// The directory the tree shows; `None` is the list of machines.
    pub directory: Option<Location>,
    /// The file shown beside the tree.
    pub file: Option<Location>,
    pub preview: bool,
    /// The directories open in this page's tree, by location key.
    pub expanded: HashSet<String>,
}

#[derive(Clone)]
pub enum TreeRow {
    Entry { entry: Rc<BrowseEntry>, depth: usize, expanded: bool },
    Machine { key: String, machine: BrowseMachine, name: String, detail: Option<String> },
    Message { id: String, depth: usize, text: String, failed: bool },
}

#[derive(Clone)]
pub struct Renaming {
    pub machine: BrowseMachine,
    pub path: String,
    pub draft: String,
    pub busy: bool,
    /// The rename happens in the tab of this page when the tab's menu asked.
    pub tab: Option<String>,
}

#[derive(Clone)]
pub struct Creating {
    pub machine: BrowseMachine,
    pub parent: String,
    pub draft: String,
    pub busy: bool,
}

#[derive(Clone)]
pub struct ConfirmDelete {
    pub machine: BrowseMachine,
    pub path: String,
    pub busy: bool,
}

#[derive(Clone)]
pub struct AddressEdit {
    pub text: String,
    pub error: Option<String>,
    pub busy: bool,
}

#[derive(Clone)]
pub struct PendingJump {
    pub key: String,
    pub line: Option<u32>,
    pub fragment: Option<String>,
}

#[derive(Clone)]
pub struct OpenMenu {
    pub anchor: ContextMenuAnchor,
    pub sections: Rc<Vec<ContextMenuSection>>,
    pub serial: u64,
}

#[derive(Clone)]
pub struct WorkspaceRoot {
    pub number: u32,
    pub machine: BrowseMachine,
    pub root: String,
}

struct PendingClick {
    key: String,
    location: Location,
    page: String,
    token: u64,
}

pub struct PaneState {
    pub id: String,
    pub props_workspaces: Vec<FilesPaneWorkspace>,
    pub context: AddressContext,
    pub resolved_roots: HashMap<u32, String>,
    workspace_signature: Option<String>,
    pub pages: Vec<FilePage>,
    pristine_page: Option<String>,
    pub active_page: Option<String>,
    pub listings: HashMap<String, DirectoryState>,
    pub viewers: HashMap<String, ViewerState>,
    pub media: HashMap<String, MediaState>,
    pub focused_path: Option<String>,
    pending_reveal: Option<String>,
    pub revealed_key: Option<String>,
    reveal_serial: u64,
    pub filter: String,
    pub debounced_filter: String,
    filter_serial: u64,
    last_directory_key: Option<Option<String>>,
    last_active_page: Option<Option<String>>,
    last_search: Option<(String, Option<String>, bool)>,
    pub search: SearchState,
    pub show_tree: bool,
    pub can_fit_tree: bool,
    /// The files being read as source rather than as what they are, by location key. A set of the exceptions: every
    /// renderable file opens rendered, and reading one document's source says nothing about the next one.
    pub source_keys: HashSet<String>,
    pub pending_jump: Option<PendingJump>,
    /// Where the open file's viewer should land, for it to consume once it has laid out.
    pub jump_line: Option<(String, u32)>,
    pub jump_fragment: Option<(String, String)>,
    pub lit_line: Option<(String, u32)>,
    lit_serial: u64,
    pub address: Option<AddressEdit>,
    pub menu: Option<OpenMenu>,
    menu_serial: u64,
    pub renaming: Option<Renaming>,
    pub creating: Option<Creating>,
    pub confirm_delete: Option<ConfirmDelete>,
    pub action_error: Option<String>,
    pub wsl_distros: Option<Vec<String>>,
    wsl_asked: bool,
    generation: u64,
    directory_requests: HashMap<String, u64>,
    /// Directory reads still out, with the openings waiting for them.
    directory_loads: HashMap<String, Vec<u64>>,
    pending_opens: HashMap<u64, (String, Location)>,
    next_token: u64,
    pending_click: Option<PendingClick>,
    hover_token: u64,
    file_requests: HashMap<String, u64>,
    media_requests: HashMap<String, u64>,
    search_request: u64,
    handled_nonce: Option<u64>,
    /// Text waiting for the next paint, the only place the clipboard can be reached from.
    pub pending_copy: Option<String>,
    /// Every path `browse_open_with` or the file manager was asked for, for nothing but tests to read.
    pub keyboard_ring: bool,
    pub mounted_on: u64,
}

#[derive(Default)]
struct Panes {
    map: HashMap<String, PaneState>,
    next_page: u64,
    /// The pane's own choice (`showTree`), for a pane that opens later in this process when the store has no answer.
    stored_show_tree: Option<bool>,
}

fn store_flag(app: &mut App) -> bool {
    app.ui_store().read_flag(SHOW_TREE_STORAGE_KEY, true)
}

fn next_page_id(app: &mut App) -> String {
    let panes = app.local::<Panes>();
    panes.next_page += 1;
    format!("page-{}", panes.next_page)
}

pub fn pane<'a>(app: &'a mut App, id: &str) -> Option<&'a mut PaneState> {
    app.local::<Panes>().map.get_mut(id)
}

/// Lifts a pane's state out of the app while a view is built from it, so `app` stays free for what the view needs.
pub fn take_pane(app: &mut App, id: &str) -> Option<PaneState> {
    app.local::<Panes>().map.remove(id)
}

pub fn put_pane(app: &mut App, state: PaneState) {
    app.local::<Panes>().map.insert(state.id.clone(), state);
}

/// Forgets a pane: what unmounting it did upstream. Call when the pane closes.
pub fn drop_files_pane(app: &mut App, pane_id: &str) {
    app.local::<Panes>().map.remove(pane_id);
}

fn new_page(id: String, directory: Option<Location>, file: Option<Location>, preview: bool, expanded: impl IntoIterator<Item = String>) -> FilePage {
    FilePage { id, directory, file, preview, expanded: expanded.into_iter().collect() }
}

impl PaneState {
    pub fn has_pending_reveal(&self) -> bool {
        self.pending_reveal.is_some()
    }

    /// Brings a row into view without lighting it: where the keyboard went.
    pub fn request_reveal(&mut self, key: String) {
        self.pending_reveal = Some(key);
    }

    pub fn active(&self) -> Option<&FilePage> {
        let id = self.active_page.as_deref()?;
        self.pages.iter().find(|p| p.id == id)
    }

    pub fn workspace_roots(&self) -> Vec<WorkspaceRoot> {
        self.props_workspaces.iter().map(|w| WorkspaceRoot { number: w.number, machine: w.machine.clone(), root: self.resolved_roots.get(&w.number).cloned().unwrap_or_else(|| browse_path(&w.path)) }).collect()
    }

    /// Where a new page opens: the first workspace, or this computer's home.
    pub fn home_location(&self) -> Location {
        self.workspace_roots().first().map_or_else(|| Location::new(&None, "~"), |w| Location::new(&w.machine, w.root.clone()))
    }

    /// The workspace `location` is in, the innermost when they nest.
    pub fn containing_workspace(&self, location: &Location) -> Option<WorkspaceRoot> {
        let mut best: Option<WorkspaceRoot> = None;
        for w in self.workspace_roots() {
            if !within(&Location::new(&w.machine, w.root.clone()), location) {
                continue;
            }
            if best.as_ref().is_none_or(|b| w.root.len() > b.root.len()) {
                best = Some(w);
            }
        }
        best
    }

    /// What a document's references are written against: its workspace when it is in one, and the top of its filesystem
    /// otherwise. The viewers work on paths relative to it.
    pub fn document_root(&self, file: &Location) -> String {
        self.containing_workspace(file).map_or_else(|| filesystem_root(&file.path), |w| w.root)
    }

    pub fn is_pristine(&self, page: &FilePage) -> bool {
        self.pristine_page.as_deref() == Some(page.id.as_str()) && page.file.is_none() && page.expanded.is_empty()
    }

    /// A change the reader made to a page; the first page is no longer the one the pane opened with.
    pub fn update_page(&mut self, id: &str, f: impl FnOnce(&mut FilePage)) {
        if self.pristine_page.as_deref() == Some(id) {
            self.pristine_page = None;
        }
        if let Some(page) = self.pages.iter_mut().find(|p| p.id == id) {
            f(page);
        }
    }

    pub fn searching(&self) -> bool {
        !self.debounced_filter.is_empty() && self.active().is_some_and(|p| p.directory.is_some())
    }

    /// Depth-first flattening of the loaded listings under a page's directory, or the list of machines.
    pub fn rows(&self) -> Vec<TreeRow> {
        let Some(page) = self.active() else { return Vec::new() };
        match &page.directory {
            Some(root) => build_rows(&self.listings, root, &page.expanded, t("正在读取…", "Loading…"), t("文件夹为空", "Folder is empty")),
            None => self.machine_rows(),
        }
    }

    fn machine_rows(&self) -> Vec<TreeRow> {
        let this_computer = t("本机", "This machine");
        let mut machines: Vec<BrowseMachine> = vec![None];
        machines.extend(self.wsl_distros.iter().flatten().map(|distro| Some(RunTarget::Wsl { distro: distro.clone() })));
        machines.extend(self.context.ssh_machines.iter().map(|m| Some(RunTarget::Ssh { machine_id: m.id.clone() })));
        machines
            .into_iter()
            .map(|machine| {
                let detail = match &machine {
                    Some(RunTarget::Ssh { machine_id }) => self.context.ssh_machines.iter().find(|m| m.id == *machine_id).map(|m| m.host.clone()),
                    _ => None,
                };
                TreeRow::Machine { key: machine_key(&machine), name: machine_label(&machine, &self.context.ssh_machines, this_computer), machine, detail }
            })
            .collect()
    }

    /// The files' bytes the open pages need: a picture, PDF, recording or font page needs its own, and a rendered
    /// document needs every picture it points at. Derived rather than accumulated, so the set shrinks when a page closes.
    pub fn needed_media(&self) -> BTreeMap<String, Location> {
        let mut needed = BTreeMap::new();
        for page in &self.pages {
            let Some(file) = &page.file else { continue };
            let key = file.key();
            if self.source_keys.contains(&key) {
                continue;
            }
            if reads_bytes(&file.path) {
                needed.insert(key, file.clone());
                continue;
            }
            let kind = file_viewer_kind(&file.path);
            if kind != FileViewerKind::Markdown && kind != FileViewerKind::Notebook {
                continue;
            }
            let Some(ViewerState::Ready { content, binary: false, .. }) = self.viewers.get(&key) else { continue };
            let root = self.document_root(file);
            let Some(relative) = relative_path(&root, &file.path) else { continue };
            for reference in cached_references(content, kind == FileViewerKind::Notebook).iter() {
                let bare = reference.split('#').next().unwrap_or("");
                let Some(resolved) = resolve_document_reference(&relative, bare) else { continue };
                if image_media_type(&resolved).is_none() {
                    continue;
                }
                let absolute = join_path(&root, &resolved);
                needed.insert(location_key(&file.machine, &absolute), Location::new(&file.machine, absolute));
            }
        }
        needed
    }

    /// Turns a document's own image reference into bytes already read. Anything not read yet answers `None` and is drawn
    /// as its alternative text until the read lands.
    pub fn resolve_image(&self, source: &str) -> Option<Arc<str>> {
        let file = self.active()?.file.clone()?;
        if has_scheme(source) || source.starts_with("//") {
            return Some(Arc::from(source));
        }
        let root = self.document_root(&file);
        let relative = relative_path(&root, &file.path).unwrap_or_else(|| file.path.clone());
        let resolved = resolve_document_reference(&relative, source.split('#').next().unwrap_or(""))?;
        match self.media.get(&location_key(&file.machine, &join_path(&root, &resolved))) {
            Some(MediaState::Ready(data)) => Some(data.clone()),
            _ => None,
        }
    }

    /// Whether `path` is a directory as far as the listings already read can tell.
    pub fn is_known_directory(&self, machine: &BrowseMachine, path: &str) -> bool {
        let Some(parent) = parent_path(path) else { return true };
        match self.listings.get(&location_key(machine, &parent)) {
            Some(DirectoryState::Ready(listing)) => listing.entries.iter().any(|e| e.path == path && e.kind == BrowseEntryKind::Directory),
            _ => false,
        }
    }

    /// `path` relative to its workspace, or to the page's directory when it is in none.
    pub fn relative_for(&self, location: &Location) -> Option<String> {
        let directory = self.active().and_then(|p| p.directory.clone());
        let base = self.containing_workspace(location).map(|w| w.root).or_else(|| directory.filter(|d| same_machine(&d.machine, &location.machine)).map(|d| d.path))?;
        relative_path(&base, &location.path).filter(|r| !r.is_empty())
    }

    pub fn page_label(&self, page: &FilePage) -> String {
        if let Some(file) = &page.file {
            return browse_name(&file.path);
        }
        match &page.directory {
            None => t("所有机器", "All machines").to_owned(),
            Some(d) if d.path == "/" || d.path == "~" => machine_label(&d.machine, &self.context.ssh_machines, t("本机", "This machine")),
            Some(d) => browse_name(&d.path),
        }
    }

    pub fn page_title(&self, page: &FilePage) -> String {
        match page.file.as_ref().or(page.directory.as_ref()) {
            Some(shown) => format_address(&shown.machine, &shown.path, &self.context),
            None => t("所有机器", "All machines").to_owned(),
        }
    }

    pub fn navigable_keys(&self) -> Vec<String> {
        let machine = self.active().and_then(|p| p.directory.as_ref()).map(|d| d.machine.clone()).unwrap_or(None);
        if self.searching() {
            let SearchState::Ready { root, matches, .. } = &self.search else { return Vec::new() };
            return matches.iter().map(|m| location_key(&machine, &join_path(root, &m.path))).collect();
        }
        self.rows().iter().filter_map(|r| match r {
            TreeRow::Entry { entry, .. } => Some(location_key(&machine, &entry.path)),
            TreeRow::Machine { key, .. } => Some(key.clone()),
            TreeRow::Message { .. } => None,
        }).collect()
    }
}

pub fn build_rows(listings: &HashMap<String, DirectoryState>, root: &Location, expanded: &HashSet<String>, loading: &str, empty: &str) -> Vec<TreeRow> {
    fn walk(rows: &mut Vec<TreeRow>, listings: &HashMap<String, DirectoryState>, machine: &BrowseMachine, path: &str, expanded: &HashSet<String>, depth: usize, loading: &str, empty: &str) {
        match listings.get(&location_key(machine, path)) {
            None | Some(DirectoryState::Loading) => rows.push(TreeRow::Message { id: format!("loading:{path}"), depth, text: loading.to_owned(), failed: false }),
            Some(DirectoryState::Error(message)) => rows.push(TreeRow::Message { id: format!("error:{path}"), depth, text: message.clone(), failed: true }),
            Some(DirectoryState::Ready(listing)) => {
                if listing.entries.is_empty() {
                    rows.push(TreeRow::Message { id: format!("empty:{path}"), depth, text: empty.to_owned(), failed: false });
                    return;
                }
                for entry in &listing.entries {
                    let open = entry.kind == BrowseEntryKind::Directory && !expanded.is_empty() && expanded.contains(&location_key(machine, &entry.path));
                    rows.push(TreeRow::Entry { entry: entry.clone(), depth, expanded: open });
                    if open {
                        walk(rows, listings, machine, &entry.path, expanded, depth + 1, loading, empty);
                    }
                }
            }
        }
    }
    let mut rows = Vec::new();
    walk(&mut rows, listings, &root.machine, &root.path, expanded, 0, loading, empty);
    rows
}

pub fn has_scheme(source: &str) -> bool {
    static SCHEME: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").expect("regex"));
    SCHEME.is_match(source)
}

/// The references of a text the pane holds, found once per text rather than once per frame.
fn cached_references(content: &Rc<str>, notebook: bool) -> Rc<Vec<String>> {
    thread_local! {
        // The text is held with its answer, so its address cannot come back as another text's while the entry stands.
        static CACHE: std::cell::RefCell<HashMap<(usize, usize), (Rc<str>, Rc<Vec<String>>)>> = std::cell::RefCell::new(HashMap::new());
    }
    let key = (content.as_ptr() as usize, content.len());
    if let Some((_, found)) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return found;
    }
    let markdown = if notebook { notebook_markdown(content) } else { content.to_string() };
    let found = Rc::new(document_image_references(&markdown));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > 64 {
            c.clear();
        }
        c.insert(key, (content.clone(), found.clone()));
    });
    found
}

/// Every reference in a Markdown source that names a file rather than something on the web: inline images, link-reference
/// definitions, and `<img src>`, which READMEs use for anything that needs a width or a centre. Collected from the source
/// rather than the rendered tree because the bytes have to be fetched before the tree can render.
pub fn document_image_references(source: &str) -> Vec<String> {
    static PATTERNS: LazyLock<[regex::Regex; 3]> = LazyLock::new(|| {
        [
            regex::Regex::new(r"!\[[^\]]*\]\(\s*<?([^)\s>]+)>?[^)]*\)").expect("regex"),
            regex::Regex::new(r"(?m)^ {0,3}\[[^\]]+\]:\s*<?([^\s>]+)>?").expect("regex"),
            regex::Regex::new(r#"(?i)<img\b[^>]*?\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).expect("regex"),
        ]
    });
    let mut references = Vec::new();
    for pattern in PATTERNS.iter() {
        for c in pattern.captures_iter(source) {
            let reference = c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3)).map_or("", |m| m.as_str());
            // An address with a scheme, a protocol-relative one, and a bare fragment are all somebody else's to resolve.
            if has_scheme(reference) || reference.starts_with("//") || reference.starts_with('#') {
                continue;
            }
            if !references.iter().any(|r| r == reference) {
                references.push(reference.to_owned());
            }
        }
    }
    references
}

fn describe(e: String) -> String {
    e.strip_prefix("Error: ").map(str::to_owned).unwrap_or(e)
}

// ---------------------------------------------------------------- mounting and effects

fn mount(app: &mut App, props: &FilesPaneProps) {
    if app.local::<Panes>().map.contains_key(&props.pane_id) {
        return;
    }
    let directory = props.workspaces.first().map_or_else(|| Location::new(&None, "~"), |w| Location::new(&w.machine, browse_path(&w.path)));
    let first = new_page(next_page_id(app), Some(directory), None, false, []);
    let show_tree = {
        let stored = app.local::<Panes>().stored_show_tree;
        stored.unwrap_or_else(|| store_flag(app))
    };
    let state = PaneState {
        id: props.pane_id.clone(),
        props_workspaces: props.workspaces.clone(),
        context: AddressContext { ssh_machines: props.ssh_machines.clone(), host_windows: props.host_windows },
        resolved_roots: HashMap::new(),
        workspace_signature: None,
        pristine_page: Some(first.id.clone()),
        active_page: Some(first.id.clone()),
        pages: vec![first],
        listings: HashMap::new(),
        viewers: HashMap::new(),
        media: HashMap::new(),
        focused_path: None,
        pending_reveal: None,
        revealed_key: None,
        reveal_serial: 0,
        filter: String::new(),
        debounced_filter: String::new(),
        filter_serial: 0,
        last_directory_key: None,
        last_active_page: None,
        last_search: None,
        search: SearchState::Idle,
        show_tree,
        can_fit_tree: true,
        source_keys: HashSet::new(),
        pending_jump: None,
        jump_line: None,
        jump_fragment: None,
        lit_line: None,
        lit_serial: 0,
        address: None,
        menu: None,
        menu_serial: 0,
        renaming: None,
        creating: None,
        confirm_delete: None,
        action_error: None,
        wsl_distros: None,
        wsl_asked: false,
        generation: 0,
        directory_requests: HashMap::new(),
        directory_loads: HashMap::new(),
        pending_opens: HashMap::new(),
        next_token: 0,
        pending_click: None,
        hover_token: 0,
        file_requests: HashMap::new(),
        media_requests: HashMap::new(),
        search_request: 0,
        handled_nonce: None,
        pending_copy: None,
        keyboard_ring: false,
        mounted_on: app.frame_no,
    };
    app.local::<Panes>().map.insert(props.pane_id.clone(), state);
}

/// What the effects of `FilesPane` did on each render: reads what the active page shows that nobody has read yet, and
/// acts on what the props now say.
pub fn sync(app: &mut App, props: &FilesPaneProps) {
    mount(app, props);
    let id = props.pane_id.as_str();
    {
        let Some(st) = pane(app, id) else { return };
        st.props_workspaces = props.workspaces.clone();
        st.context = AddressContext { ssh_machines: props.ssh_machines.clone(), host_windows: props.host_windows };
    }
    handle_open_request(app, props);
    let body = rect_of(("files-body", id));
    let Some(st) = pane(app, id) else { return };
    // The body decides whether the tree can sit beside a file.
    if let Some(body) = body
        && body.w > 0.0
    {
        st.can_fit_tree = body.w >= TREE_SIDE_BY_SIDE_MIN_WIDTH;
    }
    // A filter belongs to the directory it searched.
    let directory_key = st.active().and_then(|p| p.directory.as_ref()).map(Location::key);
    let active_page = st.active_page.clone();
    if st.last_directory_key.as_ref() != Some(&directory_key) || st.last_active_page.as_ref() != Some(&active_page) {
        if st.last_directory_key.is_some() {
            st.filter.clear();
            st.debounced_filter.clear();
        }
        st.last_directory_key = Some(directory_key.clone());
        st.last_active_page = Some(active_page);
    }
    if !props.active {
        return;
    }
    let wanted: Vec<(BrowseMachine, String)> = match st.active() {
        Some(FilePage { directory: Some(root), expanded, .. }) => {
            let mut wanted = vec![(root.machine.clone(), root.path.clone())];
            for key in expanded {
                // Only what this tree can show: a folder opened under another root waits for it.
                let path = key_path(key);
                if key_machine(key) == machine_key(&root.machine) && relative_path(&root.path, path).is_some_and(|r| !r.is_empty()) {
                    wanted.push((root.machine.clone(), path.to_owned()));
                }
            }
            wanted
        }
        _ => Vec::new(),
    };
    let missing: Vec<(BrowseMachine, String)> = wanted.into_iter().filter(|(m, p)| !st.listings.contains_key(&location_key(m, p))).collect();
    let file = st.active().and_then(|p| p.file.clone()).filter(|f| needs_text(&f.path) && !st.viewers.contains_key(&f.key()));
    for (machine, path) in missing {
        load_directory(app, id, &machine, &path);
    }
    if let Some(file) = file {
        load_file(app, id, &file.machine, &file.path);
    }
    sync_workspaces(app, props);
    sync_search(app, id);
    sync_machines(app, id, props.host_windows);
    sync_media(app, id);
    sync_jump(app, id);
}

/// The roots as the machines spell them: a remote workspace may be recorded as `~/…`, and this computer's as the host
/// canonicalized it.
fn sync_workspaces(app: &mut App, props: &FilesPaneProps) {
    let id = props.pane_id.clone();
    let signature = props.workspaces.iter().map(|w| location_key(&w.machine, &w.path)).collect::<Vec<_>>().join("\n");
    let Some(st) = pane(app, &id) else { return };
    if st.workspace_signature.as_deref() == Some(signature.as_str()) || props.workspaces.is_empty() {
        return;
    }
    st.workspace_signature = Some(signature.clone());
    let targets: Vec<BrowseProbeTarget> = props.workspaces.iter().map(|w| BrowseProbeTarget { machine: w.machine.clone(), path: browse_path(&w.path) }).collect();
    let numbers: Vec<u32> = props.workspaces.iter().map(|w| w.number).collect();
    app.invoke::<Vec<BrowseProbeResult>>("browse_probe_paths", probe_args(&targets, false), move |app, result| {
        let Ok(results) = result else { return };
        let Some(st) = pane(app, &id) else { return };
        if st.workspace_signature.as_deref() != Some(signature.as_str()) {
            return;
        }
        st.resolved_roots = results.into_iter().zip(numbers).filter(|(r, _)| r.reached).map(|(r, n)| (n, r.path)).collect();
    });
}

fn sync_search(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let directory = st.active().and_then(|p| p.directory.clone());
    let signature = (st.debounced_filter.clone(), directory.as_ref().map(Location::key), true);
    if st.last_search.as_ref() == Some(&signature) {
        return;
    }
    st.last_search = Some(signature);
    st.search_request += 1;
    let request = st.search_request;
    let query = st.debounced_filter.clone();
    let Some(directory) = directory.filter(|_| !query.is_empty()) else {
        st.search = SearchState::Idle;
        return;
    };
    if !matches!(st.search, SearchState::Ready { .. }) {
        st.search = SearchState::Loading;
    }
    let id = id.to_owned();
    app.invoke::<BrowseSearchResults>("browse_search_files", search_args(&directory.machine, &directory.path, &query), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        if st.search_request != request {
            return;
        }
        st.search = match result {
            Ok(results) => SearchState::Ready { root: results.root, matches: Rc::new(results.matches), truncated: results.truncated },
            Err(e) => SearchState::Error(describe(e)),
        };
        repaint();
    });
}

fn sync_machines(app: &mut App, id: &str, host_windows: bool) {
    let Some(st) = pane(app, id) else { return };
    let showing = st.active().is_some_and(|p| p.directory.is_none());
    if !showing || !host_windows || st.wsl_distros.is_some() || st.wsl_asked {
        return;
    }
    st.wsl_asked = true;
    let id = id.to_owned();
    app.invoke::<Vec<Value>>("list_wsl_distros", json!({}), move |app, result| {
        let names = result.map_or(Vec::new(), |d| d.iter().filter_map(|x| x.get("name").and_then(Value::as_str).map(str::to_owned)).collect());
        if let Some(st) = pane(app, &id) {
            st.wsl_distros = Some(names);
        }
        repaint();
    });
}

fn sync_media(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let needed = st.needed_media();
    let missing: Vec<Location> = needed.iter().filter(|(k, _)| !st.media.contains_key(*k)).map(|(_, l)| l.clone()).collect();
    st.media.retain(|k, _| needed.contains_key(k));
    for location in missing {
        load_media(app, id, &location.machine, &location.path);
    }
}

/// The jump waits for the file it is a place in: the rows do not exist until the read lands.
fn sync_jump(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(jump) = st.pending_jump.clone() else { return };
    let Some(file) = st.active().and_then(|p| p.file.clone()) else { return };
    if file.key() != jump.key {
        return;
    }
    if needs_text(&file.path) && !matches!(st.viewers.get(&jump.key), Some(ViewerState::Ready { .. })) {
        return;
    }
    st.pending_jump = None;
    if let Some(line) = jump.line {
        st.jump_line = Some((jump.key.clone(), line));
        st.lit_line = Some((jump.key, line));
        st.lit_serial += 1;
        let serial = st.lit_serial;
        let id = id.to_owned();
        app.set_timer(Duration::from_millis(REVEAL_FLASH_MS), move |app| {
            if let Some(st) = pane(app, &id)
                && st.lit_serial == serial
            {
                st.lit_line = None;
            }
            repaint();
        });
    } else if let Some(fragment) = jump.fragment {
        st.jump_fragment = Some((jump.key, fragment));
    }
}

// ---------------------------------------------------------------- reading

pub fn load_directory(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    let Some(st) = pane(app, id) else { return };
    let key = location_key(machine, path);
    let request = st.directory_requests.get(&key).copied().unwrap_or(0) + 1;
    st.directory_requests.insert(key.clone(), request);
    let generation = st.generation;
    // Entries already on screen stay there while they are re-read, so a refresh does not blank the tree.
    if !matches!(st.listings.get(&key), Some(DirectoryState::Ready(_))) {
        st.listings.insert(key.clone(), DirectoryState::Loading);
    }
    st.directory_loads.entry(key.clone()).or_default();
    let (id, machine, path) = (id.to_owned(), machine.clone(), path.to_owned());
    app.invoke::<BrowseListing>("browse_list_directory", list_args(&machine, &path), move |app, result| {
        settle_directory(app, &id, &machine, &path, &key, request, generation, result);
    });
}

fn settle_directory(app: &mut App, id: &str, machine: &BrowseMachine, path: &str, key: &str, request: u64, generation: u64, result: Result<BrowseListing, String>) {
    let Some(st) = pane(app, id) else { return };
    let current = generation == st.generation && st.directory_requests.get(key) == Some(&request);
    if current {
        match result {
            Ok(listing) => {
                let shared = Rc::new(Listing { path: listing.path.clone(), truncated: listing.truncated, entries: listing.entries.into_iter().map(Rc::new).collect() });
                st.listings.insert(key.to_owned(), DirectoryState::Ready(shared.clone()));
                if listing.path != path {
                    st.listings.insert(location_key(machine, &listing.path), DirectoryState::Ready(shared));
                    // `~`, a trailing slash, a `..`: the page takes the spelling the machine gave.
                    for page in &mut st.pages {
                        if let Some(d) = &mut page.directory
                            && same_machine(&d.machine, machine)
                            && d.path == path
                        {
                            d.path = listing.path.clone();
                        }
                    }
                }
            }
            Err(e) => {
                st.listings.insert(key.to_owned(), DirectoryState::Error(describe(e)));
            }
        }
    }
    if st.directory_requests.get(key) == Some(&request) {
        let waiting = st.directory_loads.remove(key).unwrap_or_default();
        for token in waiting {
            finish_open(app, id, token);
        }
    }
    repaint();
}

pub fn load_file(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    let Some(st) = pane(app, id) else { return };
    // Two reads of the same file can be in flight at once, so the fence is the request's own identity, not the path it
    // asked for; otherwise the older answer would land on top of the newer one.
    let key = location_key(machine, path);
    let request = st.file_requests.get(&key).copied().unwrap_or(0) + 1;
    st.file_requests.insert(key.clone(), request);
    let generation = st.generation;
    if !matches!(st.viewers.get(&key), Some(ViewerState::Ready { .. })) {
        st.viewers.insert(key.clone(), ViewerState::Loading);
    }
    let id = id.to_owned();
    app.invoke::<BrowseTextFile>("browse_read_file", list_args(machine, path), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        if generation != st.generation || st.file_requests.get(&key) != Some(&request) {
            return;
        }
        let next = match result {
            Ok(file) => ViewerState::Ready { content: Rc::from(file.content), binary: file.binary, truncated: file.truncated },
            Err(e) => ViewerState::Error(describe(e)),
        };
        st.viewers.insert(key, next);
        repaint();
    });
}

/// Reads a file as bytes. Fenced the same way as `load_file`: a document's images are requested as it is parsed, so
/// several reads of the same file can be outstanding while the reader clicks through a directory.
pub fn load_media(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    let Some(st) = pane(app, id) else { return };
    let key = location_key(machine, path);
    let request = st.media_requests.get(&key).copied().unwrap_or(0) + 1;
    st.media_requests.insert(key.clone(), request);
    let generation = st.generation;
    st.media.entry(key.clone()).or_insert(MediaState::Loading);
    let Some(media_type) = binary_media_type(path) else {
        st.media.insert(key, MediaState::Error(t("这不是可显示的文件格式", "This is not a displayable file format").to_owned()));
        return;
    };
    let id = id.to_owned();
    app.invoke::<BrowseFileBytes>("browse_read_file_bytes", list_args(machine, path), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        if generation != st.generation || st.media_requests.get(&key) != Some(&request) {
            return;
        }
        let next = match result {
            Ok(file) if file.too_large => MediaState::TooLarge,
            Ok(file) => MediaState::Ready(Arc::from(format!("data:{media_type};base64,{}", file.data))),
            Err(e) => MediaState::Error(describe(e)),
        };
        st.media.insert(key, next);
        repaint();
    });
}

/// Re-reads what the active page shows under a fresh generation. Directories still waiting on the previous one are
/// dropped: their answers are about to be ignored, and a cached `Loading` nobody is driving would show the next expansion
/// a reader that never finishes.
pub fn refresh(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    st.generation += 1;
    st.listings.retain(|_, s| !matches!(s, DirectoryState::Loading));
    st.media.clear();
    let tokens: Vec<u64> = st.directory_loads.drain().flat_map(|(_, w)| w).collect();
    let Some(page) = st.active().cloned() else { return };
    for token in tokens {
        finish_open(app, id, token);
    }
    if let Some(directory) = &page.directory {
        load_directory(app, id, &directory.machine, &directory.path);
        let machine = machine_key(&directory.machine);
        for key in &page.expanded {
            if key_machine(key) == machine {
                load_directory(app, id, &directory.machine, key_path(key));
            }
        }
    }
    if let Some(file) = page.file.filter(|f| needs_text(&f.path)) {
        load_file(app, id, &file.machine, &file.path);
    }
}

// ---------------------------------------------------------------- page actions

pub fn set_show_tree_preference(app: &mut App, id: &str, next: bool) {
    app.local::<Panes>().stored_show_tree = Some(next);
    app.ui_store_update(|s| s.write_flag(SHOW_TREE_STORAGE_KEY, next));
    if let Some(st) = pane(app, id) {
        st.show_tree = next;
    }
}

/// Shows `target` in the active page's tree; `None` is the list of machines.
pub fn navigate(app: &mut App, id: &str, target: Option<Location>) {
    let page_id = next_page_id(app);
    let Some(st) = pane(app, id) else { return };
    let Some(active) = st.active_page.clone() else {
        st.pages.push(new_page(page_id.clone(), target, None, false, []));
        st.active_page = Some(page_id);
        return;
    };
    st.update_page(&active, |page| page.directory = target);
    st.address = None;
    st.focused_path = None;
}

fn arm_reveal(app: &mut App, id: &str, key: String) {
    let Some(st) = pane(app, id) else { return };
    st.focused_path = Some(key.clone());
    st.revealed_key = Some(key.clone());
    st.pending_reveal = Some(key);
    st.reveal_serial += 1;
    let serial = st.reveal_serial;
    let id = id.to_owned();
    // The row lights up so the eye can find where the tree jumped to, then lets go.
    app.set_timer(Duration::from_millis(REVEAL_FLASH_MS), move |app| {
        if let Some(st) = pane(app, &id)
            && st.reveal_serial == serial
        {
            st.revealed_key = None;
        }
        repaint();
    });
}

/// A reveal scrolls its row into view once, as soon as the row is drawn. Only once: rows change all the time afterwards,
/// and the tree must stay where the reader scrolled it rather than be pulled back each time.
pub fn take_pending_reveal(st: &mut PaneState, keys: &[String]) -> Option<usize> {
    let key = st.pending_reveal.as_ref()?;
    let index = keys.iter().position(|k| k == key)?;
    st.pending_reveal = None;
    Some(index)
}

pub fn go_up(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(directory) = st.active().and_then(|p| p.directory.clone()) else { return };
    let parent = parent_path(&directory.path);
    let child = directory.key();
    let reveal = if parent.is_none() { machine_key(&directory.machine) } else { child };
    navigate(app, id, parent.map(|p| Location::new(&directory.machine, p)));
    // The directory just left is where the reader's eye goes back to.
    arm_reveal(app, id, reveal);
}

/// Opens or closes the folder `path` in the page `page_id`.
pub fn set_directory_open(app: &mut App, id: &str, page_id: &str, machine: &BrowseMachine, path: &str, open: bool) {
    let key = location_key(machine, path);
    let Some(st) = pane(app, id) else { return };
    st.update_page(page_id, |page| {
        if open {
            page.expanded.insert(key.clone());
        } else {
            page.expanded.remove(&key);
        }
    });
    // A directory that failed is retried on the next expansion; a cached one is not re-read.
    let cached = st.listings.get(&key);
    let reread = open && matches!(cached, None | Some(DirectoryState::Error(_))) && !st.directory_loads.contains_key(&key);
    if reread {
        load_directory(app, id, machine, path);
    }
}

pub fn toggle_directory(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(page_id) = st.active_page.clone() else { return };
    let open = !st.active().is_some_and(|p| p.expanded.contains(&location_key(machine, path)));
    set_directory_open(app, id, &page_id, machine, path, open);
}

/// Reads a folder before it is asked to open, so that by the time a click has waited out the double click's window its
/// listing is usually in.
pub fn prefetch_directory(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    let key = location_key(machine, path);
    let Some(st) = pane(app, id) else { return };
    if st.listings.contains_key(&key) || st.directory_loads.contains_key(&key) {
        return;
    }
    load_directory(app, id, machine, path);
}

fn finish_open(app: &mut App, id: &str, token: u64) {
    let Some(st) = pane(app, id) else { return };
    let Some((page, location)) = st.pending_opens.remove(&token) else { return };
    set_directory_open(app, id, &page, &location.machine, &location.path, true);
}

/// A single click on a folder, once the double click's window has passed: a closed folder opens with its listing in
/// place, waiting up to `EXPAND_WAIT_MS` for one still being read, so the read is hidden rather than shown as a
/// "reading" row that is replaced a moment later.
fn settle_directory_click(app: &mut App, id: &str, page_id: &str, location: &Location) {
    let key = location.key();
    let Some(st) = pane(app, id) else { return };
    let already = st.pages.iter().find(|p| p.id == page_id).is_some_and(|p| p.expanded.contains(&key));
    if already {
        set_directory_open(app, id, page_id, &location.machine, &location.path, false);
        return;
    }
    prefetch_directory(app, id, &location.machine, &location.path);
    let Some(st) = pane(app, id) else { return };
    st.next_token += 1;
    let token = st.next_token;
    st.pending_opens.insert(token, (page_id.to_owned(), location.clone()));
    if let Some(waiters) = st.directory_loads.get_mut(&key) {
        waiters.push(token);
        let id = id.to_owned();
        app.set_timer(Duration::from_millis(EXPAND_WAIT_MS), move |app| {
            finish_open(app, &id, token);
            repaint();
        });
    } else {
        finish_open(app, id, token);
    }
}

/// A click on a folder. Two within `DOUBLE_CLICK_MS` go into it; one alone opens or closes it once that window has
/// passed. A click on another row meanwhile settles the first as a single click.
pub fn click_directory(app: &mut App, id: &str, key: &str, location: Location) {
    let Some(st) = pane(app, id) else { return };
    st.focused_path = Some(key.to_owned());
    let pending = st.pending_click.take();
    if pending.as_ref().is_some_and(|p| p.key == key) {
        navigate(app, id, Some(location));
        return;
    }
    if let Some(p) = pending {
        settle_directory_click(app, id, &p.page, &p.location);
    }
    let Some(st) = pane(app, id) else { return };
    let Some(page) = st.active_page.clone() else { return };
    st.next_token += 1;
    let token = st.next_token;
    st.pending_click = Some(PendingClick { key: key.to_owned(), location: location.clone(), page, token });
    prefetch_directory(app, id, &location.machine, &location.path);
    let id = id.to_owned();
    app.set_timer(Duration::from_millis(DOUBLE_CLICK_MS), move |app| {
        let Some(st) = pane(app, &id) else { return };
        if st.pending_click.as_ref().is_none_or(|p| p.token != token) {
            return;
        }
        let Some(p) = st.pending_click.take() else { return };
        settle_directory_click(app, &id, &p.page, &p.location);
        repaint();
    });
}

/// The pointer resting on a folder reads it ahead, as a link to a path is probed ahead.
pub fn hover_directory(app: &mut App, id: &str, location: Option<Location>) {
    let Some(st) = pane(app, id) else { return };
    st.hover_token += 1;
    let token = st.hover_token;
    let Some(location) = location else { return };
    let id = id.to_owned();
    app.set_timer(Duration::from_millis(PREFETCH_HOVER_MS), move |app| {
        if pane(app, &id).is_none_or(|st| st.hover_token != token) {
            return;
        }
        prefetch_directory(app, &id, &location.machine, &location.path);
        repaint();
    });
}

/// Shows `file` in the page. `keep` is the difference between a glance and a decision: a double click, or opening it
/// from a menu, keeps the page.
pub fn show_file(app: &mut App, id: &str, page_id: &str, file: Location, keep: bool) {
    let Some(st) = pane(app, id) else { return };
    let key = file.key();
    let read = needs_text(&file.path) && !matches!(st.viewers.get(&key), Some(ViewerState::Ready { .. }));
    let (machine, path) = (file.machine.clone(), file.path.clone());
    st.update_page(page_id, |page| {
        page.file = Some(file);
        if keep {
            page.preview = false;
        }
    });
    if read {
        load_file(app, id, &machine, &path);
    }
}

/// Opens every directory on the way to `target` in the active page and scrolls its row into view.
pub fn reveal_in_tree(app: &mut App, id: &str, target: Location) {
    let Some(st) = pane(app, id) else { return };
    let Some(page) = st.active().cloned() else { return };
    let mut root = page.directory.clone();
    if root.as_ref().is_none_or(|r| !within(r, &target) || same_location(Some(r), Some(&target))) {
        let workspace = st.containing_workspace(&target).filter(|w| !same_location(Some(&Location::new(&target.machine, w.root.clone())), Some(&target)));
        root = Some(match workspace {
            Some(w) => Location::new(&target.machine, w.root),
            None => Location::new(&target.machine, parent_path(&target.path).unwrap_or_else(|| target.path.clone())),
        });
    }
    let root = root.expect("set above");
    let ancestors: Vec<String> = browse_ancestors(&root.path, &target.path).iter().map(|p| location_key(&target.machine, p)).collect();
    let keys = ancestors.clone();
    st.update_page(&page.id, |page| {
        page.directory = Some(root);
        page.expanded.extend(keys);
    });
    let stale: Vec<String> = ancestors.into_iter().filter(|k| matches!(st.listings.get(k), None | Some(DirectoryState::Error(_)))).collect();
    st.filter.clear();
    st.debounced_filter.clear();
    for key in stale {
        load_directory(app, id, &target.machine, key_path(&key));
    }
    set_show_tree_preference(app, id, true);
    arm_reveal(app, id, target.key());
}

/// A new page, made the active one.
pub fn open_page(app: &mut App, id: &str, page: FilePage) {
    let file = page.file.clone();
    let Some(st) = pane(app, id) else { return };
    st.active_page = Some(page.id.clone());
    st.pages.push(page);
    if let Some(file) = file.filter(|f| needs_text(&f.path)) {
        let read = !matches!(st.viewers.get(&file.key()), Some(ViewerState::Ready { .. }));
        if read {
            load_file(app, id, &file.machine, &file.path);
        }
    }
}

pub fn make_page(app: &mut App, directory: Option<Location>) -> FilePage {
    new_page(next_page_id(app), directory, None, false, [])
}

/// The page a file opens into when nothing else is said: its workspace's root, or its own folder.
pub fn page_for(app: &mut App, id: &str, file: Location, preview: bool) -> Option<FilePage> {
    let page_id = next_page_id(app);
    let st = pane(app, id)?;
    let directory = match st.containing_workspace(&file) {
        Some(w) => Location::new(&file.machine, w.root),
        None => Location::new(&file.machine, parent_path(&file.path).unwrap_or_else(|| file.path.clone())),
    };
    let expanded: Vec<String> = browse_ancestors(&directory.path, &file.path).iter().map(|p| location_key(&file.machine, p)).collect();
    Some(new_page(page_id, Some(directory), Some(file), preview, expanded))
}

/// `target` in a page of its own: a directory as the page's tree, a file beside its folder.
pub fn open_in_new_page(app: &mut App, id: &str, target: Location, is_directory: bool) {
    if is_directory {
        let page = make_page(app, Some(target));
        open_page(app, id, page);
        return;
    }
    let current = pane(app, id).and_then(|st| st.active().and_then(|p| p.directory.clone()));
    if let Some(current) = current.filter(|c| within(c, &target)) {
        let expanded: Vec<String> = browse_ancestors(&current.path, &target.path).iter().map(|p| location_key(&target.machine, p)).collect();
        let page = new_page(next_page_id(app), Some(current), Some(target), false, expanded);
        open_page(app, id, page);
    } else if let Some(page) = page_for(app, id, target, false) {
        open_page(app, id, page);
    }
}

pub fn keep_page(app: &mut App, id: &str, page_id: &str) {
    if let Some(st) = pane(app, id) {
        st.update_page(page_id, |page| page.preview = false);
    }
}

pub fn close_pages(app: &mut App, id: &str, doomed: impl Fn(&FilePage) -> bool) {
    let Some(st) = pane(app, id) else { return };
    let removed_at = st.active_page.as_deref().and_then(|a| st.pages.iter().position(|p| p.id == a)).unwrap_or(0);
    let before = st.pages.len();
    st.pages.retain(|p| !doomed(p));
    if st.pages.len() == before {
        return;
    }
    if st.active_page.as_deref().is_some_and(|a| st.pages.iter().any(|p| p.id == a)) {
        return;
    }
    // Closing the active page lands on its neighbour rather than the start of the strip: the page next to the one being
    // dismissed is the one being worked through.
    st.active_page = st.pages.get(removed_at.min(st.pages.len().saturating_sub(1))).map(|p| p.id.clone());
}

pub fn reorder_pages(app: &mut App, id: &str, ids: Vec<String>) {
    let Some(st) = pane(app, id) else { return };
    st.pages.sort_by_key(|p| ids.iter().position(|i| *i == p.id).unwrap_or(usize::MAX));
}

// ---------------------------------------------------------------- requests from outside

/// Opens `file` and lands on a place in it once it is on screen. A line names a place in the source, so a file that has
/// a rendered form shows its source for it; a heading names a place in the rendered form, so it stays.
pub fn jump_to(app: &mut App, id: &str, file: &Location, line: Option<u32>, fragment: Option<String>) {
    let Some(st) = pane(app, id) else { return };
    let key = file.key();
    if line.is_some() && has_source_form(&file.path) && !reads_bytes(&file.path) {
        st.source_keys.insert(key.clone());
    }
    if line.is_none() && fragment.is_none() {
        return;
    }
    st.pending_jump = Some(PendingJump { key, line, fragment });
}

/// Opens what the timeline asked for: on the page already showing it, or on the preview page (reused, the way a single
/// click in a tree is a glance), or on a new preview page. The owner is told once it has been, so a later mount does not
/// replay it.
fn handle_open_request(app: &mut App, props: &FilesPaneProps) {
    let id = props.pane_id.as_str();
    let Some(request) = props.open_request.clone() else { return };
    let Some(st) = pane(app, id) else { return };
    if st.handled_nonce == Some(request.nonce) {
        return;
    }
    st.handled_nonce = Some(request.nonce);
    let handled = |app: &mut App| {
        if let Some(f) = &props.on_open_request_handled {
            f(app, request.nonce);
        }
    };
    if request.folder {
        let folder = make_page(app, Some(Location::new(&request.machine, browse_path(&request.path))));
        let Some(st) = pane(app, id) else { return };
        st.show_tree = true;
        st.active_page = Some(folder.id.clone());
        st.pages.push(folder);
        handled(app);
        return;
    }
    // A pane opened just to show this file shows the file: the tree folds away for this visit without changing what the
    // reader chose for the pane itself.
    if request.collapse_tree {
        st.show_tree = false;
    }
    let file = Location::new(&request.machine, browse_path(&request.path));
    let showing = st.pages.iter().find(|p| same_location(p.file.as_ref(), Some(&file))).map(|p| p.id.clone());
    if let Some(page) = showing {
        st.active_page = Some(page);
    } else {
        let preview = if request.new_page { None } else { st.pages.iter().find(|p| p.preview).map(|p| p.id.clone()) };
        let lone_pristine = (st.pages.len() == 1 && st.is_pristine(&st.pages[0])).then(|| st.pages[0].id.clone());
        let Some(mut fresh) = page_for(app, id, file.clone(), !request.new_page) else { return };
        let Some(st) = pane(app, id) else { return };
        let expanded: Vec<String> = fresh.expanded.iter().cloned().collect();
        if let Some(preview) = preview {
            fresh.id = preview.clone();
            if let Some(slot) = st.pages.iter_mut().find(|p| p.id == preview) {
                *slot = fresh;
            }
            st.active_page = Some(preview);
        } else if let Some(first) = lone_pristine {
            // The pane's untouched first page is where the file goes, not beside it.
            fresh.id = first.clone();
            st.pages = vec![fresh];
            st.active_page = Some(first);
        } else {
            st.active_page = Some(fresh.id.clone());
            st.pages.push(fresh);
        }
        let unread: Vec<String> = expanded.into_iter().filter(|k| !st.listings.contains_key(k)).collect();
        for key in unread {
            load_directory(app, id, &file.machine, key_path(&key));
        }
    }
    let Some(st) = pane(app, id) else { return };
    if needs_text(&file.path) && !matches!(st.viewers.get(&file.key()), Some(ViewerState::Ready { .. })) {
        load_file(app, id, &file.machine, &file.path);
    }
    jump_to(app, id, &file, request.line, None);
    handled(app);
}

// ---------------------------------------------------------------- links in a document

/// Opens `file` in the active page and lands where a link pointed.
pub fn follow_to(app: &mut App, id: &str, file: Location, line: Option<u32>, fragment: Option<String>) {
    let Some(page_id) = pane(app, id).and_then(|st| st.active_page.clone()) else { return };
    show_file(app, id, &page_id, file.clone(), false);
    jump_to(app, id, &file, line, fragment);
}

/// Follows a link inside a rendered document. External addresses go to the system browser. A link to a file opens it, at
/// the line or heading it names; a link to a directory shows it in the tree.
pub fn on_document_link(app: &mut App, id: &str, href: &str) {
    if external_http_url(href).is_some() {
        app.invoke_logged("open_external_url", json!({ "url": href.trim() }));
        return;
    }
    let Some(target) = local_link_target(href) else { return };
    let Some(st) = pane(app, id) else { return };
    if target.path.is_empty() {
        if let (Some(fragment), Some(file)) = (target.fragment, st.active().and_then(|p| p.file.clone())) {
            st.jump_fragment = Some((file.key(), fragment));
        }
        return;
    }
    let Some(file) = st.active().and_then(|p| p.file.clone()) else { return };
    let root = st.document_root(&file);
    let relative = relative_path(&root, &file.path).unwrap_or_else(|| file.path.clone());
    let Some(resolved) = resolve_document_reference(&relative, &target.path) else { return };
    let location = Location::new(&file.machine, join_path(&root, &resolved));
    if target.path.ends_with('/') || st.is_known_directory(&location.machine, &location.path) {
        reveal_in_tree(app, id, location.clone());
        let open = pane(app, id).and_then(|st| st.active().map(|p| p.expanded.contains(&location.key()))).unwrap_or(false);
        if !open {
            toggle_directory(app, id, &location.machine, &location.path);
        }
        return;
    }
    follow_to(app, id, location, target.line, target.fragment);
}

/// Opens a file an HTML page links to, relative to the document root.
pub fn open_file_from_page(app: &mut App, id: &str, path: &str, line: Option<u32>) {
    let Some(st) = pane(app, id) else { return };
    let Some(file) = st.active().and_then(|p| p.file.clone()) else { return };
    let root = st.document_root(&file);
    follow_to(app, id, Location::new(&file.machine, join_path(&root, path)), line, None);
}

/// A path written in a document, clicked. Handled as the app-wide handler would: on this document's machine.
pub fn on_document_path(app: &mut App, id: &str, request: PathOpenRequest) {
    if crate::path_open::open_path(app, &request) {
        return;
    }
    let _ = id;
}

// ---------------------------------------------------------------- file operations

fn reload_parent(app: &mut App, id: &str, machine: &BrowseMachine, path: &str) {
    if let Some(parent) = parent_path(path) {
        load_directory(app, id, machine, &parent);
    }
}

pub fn commit_rename(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(current) = st.renaming.clone().filter(|r| !r.busy) else { return };
    let name = current.draft.trim().to_owned();
    if name.is_empty() || name == browse_name(&current.path) {
        st.renaming = None;
        return;
    }
    if let Some(r) = &mut st.renaming {
        r.busy = true;
    }
    let id = id.to_owned();
    #[derive(serde::Deserialize)]
    struct Changed {
        path: String,
    }
    app.invoke::<Changed>("browse_rename_path", json!({ "machine": current.machine, "path": current.path, "name": name }), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        match result {
            Ok(Changed { path: renamed }) => {
                st.renaming = None;
                st.action_error = None;
                let shift = |location: &mut Option<Location>| {
                    if let Some(l) = location
                        && same_machine(&l.machine, &current.machine)
                        && let Some(next) = moved(&l.path, &current.path, &renamed)
                    {
                        l.path = next;
                    }
                };
                let machine = machine_key(&current.machine);
                for page in &mut st.pages {
                    shift(&mut page.directory);
                    shift(&mut page.file);
                    page.expanded = page.expanded.iter().map(|key| {
                        if key_machine(key) != machine {
                            return key.clone();
                        }
                        moved(key_path(key), &current.path, &renamed).map_or_else(|| key.clone(), |next| location_key(&current.machine, &next))
                    }).collect();
                }
                reload_parent(app, &id, &current.machine, &current.path);
                arm_reveal(app, &id, location_key(&current.machine, &renamed));
            }
            Err(e) => {
                if let Some(r) = &mut st.renaming {
                    r.busy = false;
                }
                st.action_error = Some(describe(e));
            }
        }
        repaint();
    });
}

pub fn commit_create(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(current) = st.creating.clone().filter(|c| !c.busy) else { return };
    let name = current.draft.trim().to_owned();
    if name.is_empty() {
        st.creating = None;
        return;
    }
    if let Some(c) = &mut st.creating {
        c.busy = true;
    }
    let id = id.to_owned();
    #[derive(serde::Deserialize)]
    struct Changed {
        path: String,
    }
    app.invoke::<Changed>("browse_create_directory", json!({ "machine": current.machine, "parent": current.parent, "name": name }), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        match result {
            Ok(Changed { path: made }) => {
                st.creating = None;
                st.action_error = None;
                load_directory(app, &id, &current.machine, &current.parent);
                arm_reveal(app, &id, location_key(&current.machine, &made));
            }
            Err(e) => {
                if let Some(c) = &mut st.creating {
                    c.busy = false;
                }
                st.action_error = Some(describe(e));
            }
        }
        repaint();
    });
}

pub fn commit_delete(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(current) = st.confirm_delete.clone().filter(|c| !c.busy) else { return };
    if let Some(c) = &mut st.confirm_delete {
        c.busy = true;
    }
    let id = id.to_owned();
    app.invoke::<Value>("browse_delete_path", json!({ "machine": current.machine, "path": current.path }), move |app, result| {
        let Some(st) = pane(app, &id) else { return };
        st.confirm_delete = None;
        match result {
            Ok(_) => {
                st.action_error = None;
                let gone = |l: &Location| same_machine(&l.machine, &current.machine) && relative_path(&current.path, &l.path).is_some();
                for page in &mut st.pages {
                    if page.file.as_ref().is_some_and(gone) {
                        page.file = None;
                    }
                    if page.directory.as_ref().is_some_and(gone) {
                        page.directory = Some(Location::new(&current.machine, parent_path(&current.path).unwrap_or_else(|| current.path.clone())));
                    }
                }
                reload_parent(app, &id, &current.machine, &current.path);
            }
            Err(e) => st.action_error = Some(describe(e)),
        }
        repaint();
    });
}

pub fn copy_text(app: &mut App, id: &str, text: String) {
    if let Some(st) = pane(app, id) {
        st.pending_copy = Some(text);
    }
    repaint();
}

// ---------------------------------------------------------------- filter

pub fn set_filter(app: &mut App, id: &str, value: String) {
    let Some(st) = pane(app, id) else { return };
    st.filter = value;
    st.filter_serial += 1;
    let serial = st.filter_serial;
    let id = id.to_owned();
    app.set_timer(Duration::from_millis(FILTER_DEBOUNCE_MS), move |app| {
        if let Some(st) = pane(app, &id)
            && st.filter_serial == serial
        {
            st.debounced_filter = st.filter.trim().to_owned();
        }
        repaint();
    });
}

// ---------------------------------------------------------------- address bar

pub fn submit_address(app: &mut App, id: &str) {
    let Some(st) = pane(app, id) else { return };
    let Some(current) = st.address.clone().filter(|a| !a.busy) else { return };
    let directory = st.active().and_then(|p| p.directory.clone());
    let parsed = parse_address(&current.text, &st.context, directory.as_ref().map(|d| (&d.machine, d.path.as_str())));
    match parsed {
        ParsedAddress::Machines => navigate(app, id, None),
        ParsedAddress::UnknownMachine { name } => {
            st.address = Some(AddressEdit { error: Some(tf("没有登记名为 {name} 的 SSH 机器", "No SSH machine named {name} is registered", &[("name", &name)])), ..current });
        }
        ParsedAddress::Location { machine, path } => {
            st.address = Some(AddressEdit { busy: true, error: None, ..current.clone() });
            let id = id.to_owned();
            // Patient: the first look at a machine may be what brings its link up.
            app.invoke::<Vec<BrowseProbeResult>>("browse_probe_paths", probe_args(&[BrowseProbeTarget { machine: machine.clone(), path }], true), move |app, result| {
                let fail = |app: &mut App, error: String| {
                    if let Some(st) = pane(app, &id) {
                        st.address = Some(AddressEdit { text: current.text.clone(), busy: false, error: Some(error) });
                    }
                };
                match result {
                    Err(e) => fail(app, describe(e)),
                    Ok(results) => {
                        let Some(result) = results.into_iter().next().filter(|r| r.reached) else {
                            fail(app, t("这台机器现在连不上", "That machine cannot be reached right now").to_owned());
                            return;
                        };
                        let location = Location::new(&machine, result.path);
                        match result.kind {
                            Some(BrowseEntryKind::Directory) => navigate(app, &id, Some(location)),
                            Some(BrowseEntryKind::File) => {
                                let Some(st) = pane(app, &id) else { return };
                                st.address = None;
                                let page = st.active().cloned();
                                match page {
                                    None => {
                                        if let Some(page) = page_for(app, &id, location, false) {
                                            open_page(app, &id, page);
                                        }
                                    }
                                    Some(page) => {
                                        if page.directory.as_ref().is_none_or(|d| !within(d, &location)) {
                                            navigate(app, &id, Some(Location::new(&location.machine, parent_path(&location.path).unwrap_or_else(|| location.path.clone()))));
                                        }
                                        show_file(app, &id, &page.id, location, true);
                                    }
                                }
                            }
                            _ => fail(app, t("找不到这个位置", "Nothing is there").to_owned()),
                        }
                    }
                }
                repaint();
            });
        }
    }
}

// ---------------------------------------------------------------- menus

/// A row with an icon and an action: nearly every row these menus have.
fn menu_item(id: &str, label: impl Into<String>, icon: &'static str, on_select: impl Fn(&mut App) + 'static) -> ContextMenuItem {
    ContextMenuItem::new(id, label).icon(icon).on_select(on_select)
}

pub fn open_menu(app: &mut App, id: &str, anchor: ContextMenuAnchor, sections: Vec<ContextMenuSection>) {
    if let Some(st) = pane(app, id) {
        st.menu_serial += 1;
        st.menu = Some(OpenMenu { anchor, sections: Rc::new(sections), serial: st.menu_serial });
    }
}

fn open_with_item(id: &str, location: &Location) -> ContextMenuItem {
    let local = location.machine.is_none();
    let pane_id = id.to_owned();
    let path = native_spelling(&location.path);
    let mut item = ContextMenuItem::new("open-with", t("用…打开", "Open With")).icon("SquareArrowOutUpRight").disabled(!local).load_children(move |app, loader| load_open_with(app, pane_id.clone(), path.clone(), loader));
    if !local {
        item = item.title(t("文件在另一台机器上，本机的程序打不开它", "The file is on another machine, which this computer's programs cannot open"));
    }
    item
}

fn load_open_with(app: &mut App, pane_id: String, path: String, loader: MenuLoader) {
    let args = json!({ "path": path });
    app.invoke::<OpenWithChoices>("browse_open_with_choices", args, move |_app, result| {
        let choices = match result {
            Ok(choices) => choices,
            Err(e) => return loader.reject(describe(e)),
        };
        let mut sections = vec![ContextMenuSection::new("apps", choices.apps.iter().map(|program| {
            let (pane_id, path, app_id) = (pane_id.clone(), path.clone(), program.id.clone());
            let mut item = ContextMenuItem::new(format!("app:{}", program.id), program.name.clone()).on_select(move |app| report_action(app, &pane_id, "browse_open_with", json!({ "path": path, "app": app_id })));
            item = match program.icon.as_deref().and_then(crate::images::icon_from_data_url) {
                Some(picture) => item.icon_el(move || image(picture, (32, 32)).size(16.0, 16.0).shrink0()),
                None => item.icon("SquareArrowOutUpRight"),
            };
            if program.default {
                item = item.hint(t("默认", "Default"));
            }
            item
        }).collect())];
        if choices.chooser {
            let (pane_id, path) = (pane_id.clone(), path.clone());
            sections.push(ContextMenuSection::new("chooser", vec![ContextMenuItem::new("chooser", t("选择其他应用…", "Choose Another App…")).on_select(move |app| {
                report_action(app, &pane_id, "browse_open_with_chooser", json!({ "path": path }));
            })]));
        }
        loader.resolve(sections);
        repaint();
    });
}

fn report_action(app: &mut App, id: &str, command: &str, args: Value) {
    let id = id.to_owned();
    app.invoke::<Value>(command, args, move |app, result| {
        if let (Err(e), Some(st)) = (result, pane(app, &id)) {
            st.action_error = Some(describe(e));
        }
        repaint();
    });
}

pub fn reveal_in_file_manager(app: &mut App, id: &str, path: &str) {
    report_action(app, id, "browse_open_in_file_manager", json!({ "path": native_spelling(path) }));
}

fn reveal_item(id: &str, location: &Location) -> ContextMenuItem {
    let (pane_id, path) = (id.to_owned(), location.path.clone());
    let mut item = menu_item("reveal", t("在文件管理器中打开", "Open in File Manager"), "FolderInput", move |app| reveal_in_file_manager(app, &pane_id, &path)).disabled(location.machine.is_some());
    item.title = location.machine.is_some().then(|| t("文件管理器只能打开本机的文件", "The file manager only reaches this computer's files").to_owned());
    item
}

fn copy_items(st: &PaneState, location: &Location) -> Vec<ContextMenuItem> {
    let relative = st.relative_for(location);
    let id = st.id.clone();
    let absolute = native_spelling(&location.path);
    let mut relative_item = {
        let (id, relative, windows) = (id.clone(), relative.clone(), is_windows_path(&location.path));
        menu_item("copy-relative", t("复制相对路径", "Copy Relative Path"), "Copy", move |app| {
            if let Some(r) = &relative {
                copy_text(app, &id, if windows { r.replace('/', "\\") } else { r.clone() });
            }
        })
    };
    relative_item.disabled = relative.is_none();
    vec![menu_item("copy-absolute", t("复制绝对路径", "Copy Absolute Path"), "Copy", move |app| copy_text(app, &id, absolute.clone())), relative_item]
}

fn new_page_item(id: &str, location: &Location, is_directory: bool) -> ContextMenuItem {
    let (pane_id, location) = (id.to_owned(), location.clone());
    menu_item("new-page", t("在新页面打开", "Open in New Page"), "FilePlus2", move |app| open_in_new_page(app, &pane_id, location.clone(), is_directory))
}

/// The menu of one row of the tree, or of a search result.
pub fn entry_menu(st: &PaneState, location: &Location, kind: BrowseEntryKind) -> Vec<ContextMenuSection> {
    let id = st.id.clone();
    let directory_row = kind == BrowseEntryKind::Directory;
    let open = {
        let (id, location) = (id.clone(), location.clone());
        menu_item("open", if directory_row { t("打开文件夹", "Open Folder") } else { t("打开文件", "Open File") }, if directory_row { "FolderOpen" } else { "File" }, move |app| {
            if directory_row {
                navigate(app, &id, Some(location.clone()));
            } else {
                match pane(app, &id).and_then(|st| st.active_page.clone()) {
                    Some(page) => show_file(app, &id, &page, location.clone(), true),
                    None => open_in_new_page(app, &id, location.clone(), false),
                }
            }
        })
    };
    let rename = {
        let (id, location) = (id.clone(), location.clone());
        menu_item("rename", t("重命名…", "Rename…"), "PenLine", move |app| {
            if let Some(st) = pane(app, &id) {
                st.action_error = None;
                st.filter.clear();
                st.debounced_filter.clear();
                st.renaming = Some(Renaming { machine: location.machine.clone(), path: location.path.clone(), draft: browse_name(&location.path), busy: false, tab: None });
            }
            focus_request(("field", ("files-rename", id.as_str())));
        })
    };
    let delete = {
        let (id, location) = (id.clone(), location.clone());
        menu_item("delete", t("删除…", "Delete…"), "Trash2", move |app| {
            if let Some(st) = pane(app, &id) {
                st.action_error = None;
                st.confirm_delete = Some(ConfirmDelete { machine: location.machine.clone(), path: location.path.clone(), busy: false });
            }
        }).danger()
    };
    vec![
        ContextMenuSection::new("open", vec![reveal_item(&id, location), open, open_with_item(&id, location), new_page_item(&id, location, directory_row)]),
        ContextMenuSection::new("edit", vec![rename, delete]),
        ContextMenuSection::new("copy", copy_items(st, location)),
    ]
}

fn start_create(app: &mut App, id: &str, target: &Location) {
    if let Some(st) = pane(app, id) {
        st.action_error = None;
        st.filter.clear();
        st.debounced_filter.clear();
        st.creating = Some(Creating { machine: target.machine.clone(), parent: target.path.clone(), draft: String::new(), busy: false });
    }
    focus_request(("field", ("files-create", id)));
}

/// The menu of the tree's empty space: the directory the tree shows.
pub fn directory_menu(st: &PaneState, location: &Location) -> Vec<ContextMenuSection> {
    let id = st.id.clone();
    let new_folder = {
        let (id, location) = (id.clone(), location.clone());
        menu_item("new-folder", t("新建目录", "New Folder"), "FolderPlus", move |app| start_create(app, &id, &location))
    };
    vec![
        ContextMenuSection::new("open", vec![reveal_item(&id, location), new_page_item(&id, location, true), new_folder]),
        ContextMenuSection::new("copy", copy_items(st, location)),
    ]
}

pub fn machine_menu(id: &str, machine: &BrowseMachine) -> Vec<ContextMenuSection> {
    let home = Location::new(machine, "~");
    let (pane_id, open_home) = (id.to_owned(), home.clone());
    vec![ContextMenuSection::new("open", vec![
        menu_item("open", t("打开", "Open"), "FolderOpen", move |app| navigate(app, &pane_id, Some(open_home.clone()))),
        new_page_item(id, &home, true),
    ])]
}

/// A tab's right-click menu: the page's own actions, then those of the file or folder it shows, then closing pages.
pub fn tab_menu(st: &PaneState, page_id: &str) -> Vec<ContextMenuSection> {
    let Some(page) = st.pages.iter().find(|p| p.id == page_id) else { return Vec::new() };
    let id = st.id.clone();
    let mut sections = Vec::new();
    let mut page_items = Vec::new();
    if page.preview {
        let (id, page_id) = (id.clone(), page_id.to_owned());
        page_items.push(menu_item("keep-open", t("固定打开", "Keep open"), "Pin", move |app| keep_page(app, &id, &page_id)));
    }
    if let Some(file) = page.file.clone() {
        let (id, page_id) = (id.clone(), page_id.to_owned());
        page_items.push(menu_item("reveal-in-tree", t("在文件树中显示", "Reveal in file tree"), "FolderTree", move |app| {
            if let Some(st) = pane(app, &id) {
                st.active_page = Some(page_id.clone());
            }
            reveal_in_tree(app, &id, file.clone());
        }));
    }
    if !page_items.is_empty() {
        sections.push(ContextMenuSection::new("page", page_items));
    }
    if let Some(target) = page.file.as_ref().or(page.directory.as_ref()).cloned() {
        let is_directory = page.file.is_none();
        let mut open_items = vec![reveal_item(&id, &target), open_with_item(&id, &target), new_page_item(&id, &target, is_directory)];
        if is_directory {
            let (id, page_id, target) = (id.clone(), page_id.to_owned(), target.clone());
            open_items.push(menu_item("new-folder", t("新建目录", "New Folder"), "FolderPlus", move |app| {
                if let Some(st) = pane(app, &id) {
                    st.active_page = Some(page_id.clone());
                }
                set_show_tree_preference(app, &id, true);
                start_create(app, &id, &target);
            }));
        }
        sections.push(ContextMenuSection::new("open", open_items));
        let rename = {
            let (id, page_id, target) = (id.clone(), page_id.to_owned(), target.clone());
            menu_item("rename", t("重命名…", "Rename…"), "PenLine", move |app| {
                if let Some(st) = pane(app, &id) {
                    st.action_error = None;
                    st.renaming = Some(Renaming { machine: target.machine.clone(), path: target.path.clone(), draft: browse_name(&target.path), busy: false, tab: Some(page_id.clone()) });
                }
                focus_request(("field", ("files-rename", id.as_str())));
            })
        };
        let delete = {
            let (id, page_id, target) = (id.clone(), page_id.to_owned(), target.clone());
            menu_item("delete", t("删除…", "Delete…"), "Trash2", move |app| {
                if let Some(st) = pane(app, &id) {
                    st.active_page = Some(page_id.clone());
                    st.action_error = None;
                    st.confirm_delete = Some(ConfirmDelete { machine: target.machine.clone(), path: target.path.clone(), busy: false });
                }
                set_show_tree_preference(app, &id, true);
            }).danger()
        };
        sections.push(ContextMenuSection::new("edit", vec![rename, delete]));
        sections.push(ContextMenuSection::new("copy", copy_items(st, &target)));
    }
    let close_one = {
        let (id, page_id) = (id.clone(), page_id.to_owned());
        menu_item("close", t("关闭页面", "Close page"), "X", move |app| close_pages(app, &id, |p| p.id == page_id))
    };
    let close_others = {
        let (id, page_id) = (id.clone(), page_id.to_owned());
        menu_item("close-others", t("关闭其他页面", "Close other pages"), "ListX", move |app| close_pages(app, &id, |p| p.id != page_id)).disabled(st.pages.len() < 2)
    };
    let close_all = {
        let id = id.clone();
        menu_item("close-all", t("关闭所有页面", "Close all pages"), "SquareX", move |app| close_pages(app, &id, |_| true))
    };
    sections.push(ContextMenuSection::new("close", vec![close_one, close_others, close_all]));
    sections
}

/// The workspaces, by machine: the footer's menu and the tab strip's `+`. `pick` is what choosing one does.
pub fn workspace_sections(st: &PaneState, pick: Rc<dyn Fn(&mut App, Location)>) -> Vec<PopoverMenuSection> {
    let mut groups: Vec<(String, BrowseMachine, Vec<WorkspaceRoot>)> = Vec::new();
    for w in st.workspace_roots() {
        let key = machine_key(&w.machine);
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push(w),
            None => groups.push((key, w.machine.clone(), vec![w])),
        }
    }
    if groups.is_empty() {
        return vec![PopoverMenuSection::new("home").item(PopoverMenuItem::new("home", t("主目录", "Home")).icon("Monitor").on(move |app| pick(app, Location::new(&None, "~"))))];
    }
    groups.into_iter().map(|(key, machine, items)| {
        PopoverMenuSection::new(key).label(machine_label(&machine, &st.context.ssh_machines, t("本机", "This machine"))).items(items.into_iter().map(|w| {
            let pick = pick.clone();
            let location = Location::new(&w.machine, w.root.clone());
            PopoverMenuItem::new(format!("workspace-{}", w.number), format_address(&w.machine, &w.root, &st.context)).path_label().icon("FolderOpen").on(move |app| pick(app, location.clone()))
        }))
    }).collect()
}

/// What the reader's "Reload file" does: the text again, and the bytes behind a picture, which are cached separately.
pub fn reload_file(app: &mut App, id: &str, file: &Location) {
    if needs_text(&file.path) {
        load_file(app, id, &file.machine, &file.path);
    }
    let Some(st) = pane(app, id) else { return };
    let needed = st.needed_media();
    st.media.remove(&file.key());
    for key in needed.keys() {
        st.media.remove(key);
    }
}

pub fn toggle_source(st: &mut PaneState, key: &str) {
    if !st.source_keys.remove(key) {
        st.source_keys.insert(key.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_image_references_that_name_files() {
        let source = "![a](docs/a.png) ![b](https://x/b.png)\n[ref]: <img/c.png>\n<img width=3 src=\"d.png\">\n![e](#frag)\n![f](//cdn/f.png)";
        assert_eq!(document_image_references(source), ["docs/a.png", "img/c.png", "d.png"]);
    }

    #[test]
    fn a_rename_carries_what_was_under_it() {
        assert_eq!(moved("/w/a/b.txt", "/w/a", "/w/z").as_deref(), Some("/w/z/b.txt"));
        assert_eq!(moved("/w/a", "/w/a", "/w/z").as_deref(), Some("/w/z"));
        assert_eq!(moved("/w/other", "/w/a", "/w/z"), None);
    }
}
