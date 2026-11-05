//! The Review pane: what changed in each repository, with the diffs inline, staging and a commit box.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

use fluent::text::{hash_str, FxMap};
use fluent::theme::*;
use fluent::{rect, Color, CursorKind, Doc, Icon, Key, MenuItem, Rect, Scroll, TextEdit, Ui};
use git_core::{GitAction, GitDiffRequest, GitFileChange, GitFileStatus, GitWorkspaceSnapshot};

use crate::code::{CodeDoc, View};
use crate::{diff, pinned_scroll, sub, Wake, RIGHT_GUTTER};

pub(crate) const TAB_H: f32 = 30.0;
pub(crate) const HEADER_H: f32 = 30.0;
pub(crate) const FILE_H: u32 = 32;
pub(crate) const COMMIT_BUTTON_W: f32 = 56.0;
const COMMIT_PAD: f32 = 6.0;
const CONFIRM_H: f32 = 68.0;
const MAX_COMMIT_LINES: f32 = 5.0;
const MAX_NOTICE_LINES: f32 = 4.0;
/// Expanding everything spawns one Git per file, and the page gets no more readable past this.
const MAX_EXPAND_ALL_FILES: usize = 60;
const MAX_EXPAND_ALL_LINES: u64 = 5000;
const COMMIT_TIMEOUT: Duration = Duration::from_secs(120);

enum Job {
    Status,
    Diff { path: String, rev: u64 },
    Action(GitAction),
    PrepareDiscard { path: String, untracked: bool },
    Commit(String),
}

enum Msg {
    Status(Result<Option<Box<GitWorkspaceSnapshot>>, String>),
    Diff { path: String, rev: u64, doc: Box<CodeDoc> },
    Action(Result<(), String>),
    DiscardReady(Result<PendingDiscard, String>),
    Committed(Result<(), String>),
}

/// A discard Git has proved safe to ask about: it only goes through if nothing changed since.
#[derive(Clone)]
struct PendingDiscard {
    path: String,
    untracked: bool,
    content_revision: String,
    target_revision: String,
}

/// One thread per repository: Git operations on a checkout run one at a time anyway, under git-core's lock, and a
/// repository that hangs must not hold up the tab beside it.
fn worker(root: PathBuf, jobs: Receiver<Job>, out: Sender<Msg>, wake: Wake) {
    while let Ok(job) = jobs.recv() {
        let msg = match job {
            Job::Status => Msg::Status(git_core::workspace_snapshot(&root).map(|s| s.map(Box::new))),
            Job::Diff { path, rev } => {
                let request = GitDiffRequest::Working { path: Some(path.clone()), context: None };
                let doc = match git_core::diff(&root, request) {
                    Ok(d) => diff::build(&d.patch, &path, d.truncated),
                    Err(e) => CodeDoc::note(&format!("无法读取差异：{e}")),
                };
                Msg::Diff { path, rev, doc: Box::new(doc) }
            }
            Job::Action(action) => Msg::Action(git_core::execute_action(&root, action).map(|_| ())),
            Job::PrepareDiscard { path, untracked } => Msg::DiscardReady(git_core::prepare_discard(&root, std::slice::from_ref(&path), untracked).map(|p| PendingDiscard {
                path,
                untracked,
                content_revision: p.snapshot.content_revision,
                target_revision: p.target_revision,
            })),
            Job::Commit(message) => Msg::Committed(commit(&root, &message)),
        };
        if out.send(msg).is_err() {
            return;
        }
        wake();
    }
}

/// git-core has every other write but no commit, so this one goes to the CLI directly. Hooks and signing can run for a
/// long time or wait on something that never comes, so it is bounded, and nothing a hook left behind holding the pipes
/// can keep the worker here.
fn commit(root: &Path, message: &str) -> Result<(), String> {
    let mut child = Command::new("git")
        .current_dir(root)
        .args(["-c", "core.fsmonitor=false", "-c", "gc.auto=0", "-c", "maintenance.auto=false", "-c", "color.ui=false", "commit", "--quiet", "--file=-"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法启动 Git：{e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(message.as_bytes());
    }
    let (tx, rx) = channel();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [child.stdout.take().map(|p| Box::new(p) as _), child.stderr.take().map(|p| Box::new(p) as _)];
    for mut pipe in pipes.into_iter().flatten() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.read_to_end(&mut out);
            let _ = tx.send(out);
        });
    }
    let deadline = Instant::now() + COMMIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("提交超时，已终止。".to_owned());
            }
            Err(e) => return Err(format!("无法等待 Git：{e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut output = String::new();
    for _ in 0..2 {
        if let Ok(part) = rx.recv_timeout(Duration::from_secs(1)) {
            output.push_str(&String::from_utf8_lossy(&part));
        }
    }
    let output = output.trim();
    Err(if output.is_empty() { "提交失败。".to_owned() } else { output.chars().take(2000).collect() })
}

struct FileRow {
    change: GitFileChange,
    /// Id of the row, from its path, so that a hover survives the list being rebuilt under it.
    key: u64,
    open: bool,
    doc: Option<Box<CodeDoc>>,
    /// The `Repo::rev` the doc was read at, and the one a request is out for.
    doc_rev: u64,
    inflight: u64,
}

impl FileRow {
    fn letter(&self) -> char {
        match self.change.status {
            GitFileStatus::Added => 'A',
            GitFileStatus::Modified => 'M',
            GitFileStatus::Deleted => 'D',
            GitFileStatus::Renamed => 'R',
            GitFileStatus::Copied => 'C',
            GitFileStatus::TypeChanged => 'T',
            GitFileStatus::Untracked => 'U',
            GitFileStatus::Unmerged => '!',
            GitFileStatus::Ignored => 'I',
            GitFileStatus::Unknown => '?',
        }
    }

    fn stageable(&self) -> bool {
        let c = &self.change;
        c.unstaged && (!c.submodule || c.submodule_commit_changed)
    }

    fn discardable(&self) -> bool {
        self.change.unstaged && !self.change.submodule
    }

    fn counts(&self) -> Option<(u64, u64)> {
        let c = &self.change;
        match (c.additions, c.deletions, &self.doc) {
            (Some(a), Some(d), _) => Some((a, d)),
            // Untracked files carry no counts until their diff has been read, and then only if it had lines.
            (_, _, Some(doc)) if doc.additions + doc.deletions > 0 => Some((doc.additions, doc.deletions)),
            _ => None,
        }
    }
}

/// A sentence that wraps, for what Git said, laid out only when it or the width changes.
#[derive(Default)]
struct Notice {
    text: String,
    width: f32,
    color: Option<Color>,
    doc: Option<Doc>,
}

impl Notice {
    fn height(&mut self, ui: &mut Ui<'_>, text: &str, w: f32) -> f32 {
        let color = ui.theme.critical;
        if self.doc.is_none() || self.text != text || self.width != w || self.color != Some(color) {
            let mut doc = Doc::new((w * ui.scale).max(1.0));
            doc.push(ui.text, text, CAPTION, color);
            self.doc = Some(doc);
            self.text = text.to_owned();
            self.width = w;
            self.color = Some(color);
        }
        let h = self.doc.as_ref().map_or(0.0, |d| d.height() / ui.scale);
        h.min(CAPTION.line * MAX_NOTICE_LINES)
    }
}

struct Repo {
    name: String,
    tx: Sender<Job>,
    rx: Receiver<Msg>,

    loaded: bool,
    not_repo: bool,
    branch: String,
    ahead: u32,
    behind: u32,
    additions: u64,
    deletions: u64,
    staged: u32,
    content_revision: String,
    /// Bumped whenever `content_revision` changes. Diffs read at an older one are stale.
    rev: u64,
    files: Vec<FileRow>,
    index: FxMap<String, usize>,

    tops: Vec<u32>,
    layout_dirty: bool,
    layout_key: (u32, bool, u32),
    scroll: Scroll,

    want_status: bool,
    status_busy: bool,
    /// A write is on its way, and no other starts until it is back.
    busy: bool,
    status_error: Option<String>,
    action_error: Option<String>,
    notice: Notice,
    discard: Option<PendingDiscard>,
    commit: TextEdit,
    #[cfg(test)]
    status_requests: u32,
}

impl Repo {
    fn new(root: PathBuf, wake: &Wake) -> Self {
        let (tx, jobs) = channel();
        let (out, rx) = channel();
        let name = root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into_owned());
        let wake = wake.clone();
        let spawned = std::thread::Builder::new().name("review-git".to_owned()).spawn(move || worker(root, jobs, out, wake));
        let mut repo = Self {
            name,
            tx,
            rx,
            loaded: false,
            not_repo: false,
            branch: String::new(),
            ahead: 0,
            behind: 0,
            additions: 0,
            deletions: 0,
            staged: 0,
            content_revision: String::new(),
            rev: 0,
            files: Vec::new(),
            index: FxMap::default(),
            tops: vec![0],
            layout_dirty: true,
            layout_key: (0, false, 0),
            scroll: pinned_scroll(),
            want_status: true,
            status_busy: false,
            busy: false,
            status_error: spawned.err().map(|e| format!("无法启动 Git 线程：{e}")),
            action_error: None,
            notice: Notice::default(),
            discard: None,
            commit: TextEdit::new(),
            #[cfg(test)]
            status_requests: 0,
        };
        repo.reconcile();
        repo
    }

    fn drain(&mut self) {
        let mut any = false;
        while let Ok(msg) = self.rx.try_recv() {
            self.apply(msg);
            any = true;
        }
        if any {
            self.reconcile();
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Status(result) => {
                self.status_busy = false;
                self.loaded = true;
                match result {
                    Ok(None) => {
                        self.not_repo = true;
                        self.files.clear();
                        self.index.clear();
                        self.layout_dirty = true;
                    }
                    Ok(Some(snapshot)) => self.take_snapshot(*snapshot),
                    Err(e) => self.status_error = Some(e),
                }
            }
            Msg::Diff { path, rev, doc } => {
                let Some(&i) = self.index.get(&path) else { return };
                let row = &mut self.files[i];
                if row.inflight == rev {
                    row.inflight = 0;
                }
                if rev >= row.doc_rev {
                    row.doc = Some(doc);
                    row.doc_rev = rev;
                    self.layout_dirty = true;
                }
            }
            Msg::Action(result) => {
                self.busy = false;
                self.want_status = true;
                self.action_error = result.err();
            }
            Msg::DiscardReady(result) => {
                self.busy = false;
                match result {
                    Ok(pending) => self.discard = Some(pending),
                    Err(e) => self.action_error = Some(e),
                }
            }
            Msg::Committed(result) => {
                self.busy = false;
                self.want_status = true;
                match result {
                    Ok(()) => self.commit.clear(),
                    Err(e) => self.action_error = Some(e),
                }
            }
        }
    }

    fn take_snapshot(&mut self, snapshot: GitWorkspaceSnapshot) {
        self.not_repo = false;
        self.status_error = None;
        self.branch = snapshot.branch.clone().or_else(|| snapshot.head.as_deref().map(|h| h.chars().take(7).collect())).unwrap_or_else(|| "HEAD".to_owned());
        self.ahead = snapshot.ahead;
        self.behind = snapshot.behind;
        self.additions = snapshot.additions;
        self.deletions = snapshot.deletions;
        self.staged = snapshot.files.iter().filter(|f| f.staged).count() as u32;
        if snapshot.content_revision != self.content_revision {
            self.content_revision = snapshot.content_revision;
            self.rev += 1;
        }
        let mut old: FxMap<String, FileRow> = self.files.drain(..).map(|f| (f.change.path.clone(), f)).collect();
        let mut changes = snapshot.files;
        // Tracked first: they are what a review is about, and a stray build output should not push them off the page.
        changes.sort_by(|a, b| a.untracked.cmp(&b.untracked).then_with(|| a.path.cmp(&b.path)));
        self.index.clear();
        for (i, change) in changes.into_iter().enumerate() {
            self.index.insert(change.path.clone(), i);
            let key = hash_str(&change.path);
            let row = match old.remove(&change.path) {
                Some(mut row) => {
                    if !row.open && row.doc_rev != self.rev {
                        row.doc = None;
                    }
                    // The content revision covers tracked diffs only, so a changed untracked file does not move it.
                    if row.open && change.untracked {
                        row.doc_rev = 0;
                    }
                    row.change = change;
                    row
                }
                None => FileRow { change, key, open: false, doc: None, doc_rev: 0, inflight: 0 },
            };
            self.files.push(row);
        }
        self.layout_dirty = true;
    }

    /// Sends what the current state calls for. Everything goes through here, which is what makes a burst of refreshes,
    /// or of toggles, cost one request each however they interleave with results coming back.
    fn reconcile(&mut self) {
        if self.want_status && !self.status_busy {
            self.want_status = false;
            self.status_busy = true;
            let _ = self.tx.send(Job::Status);
            #[cfg(test)]
            {
                self.status_requests += 1;
            }
        }
        for row in &mut self.files {
            if !row.open || row.doc_rev == self.rev || row.inflight == self.rev {
                continue;
            }
            let answer = if row.change.submodule {
                Some("子模块变更没有行级差异")
            } else if row.change.binary {
                Some("二进制文件，无法显示差异")
            } else {
                None
            };
            if let Some(answer) = answer {
                row.doc = Some(Box::new(CodeDoc::note(answer)));
                row.doc_rev = self.rev;
                self.layout_dirty = true;
            } else {
                row.inflight = self.rev;
                let _ = self.tx.send(Job::Diff { path: row.change.path.clone(), rev: self.rev });
            }
        }
    }

    fn run(&mut self, job: Job) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.action_error = None;
        let _ = self.tx.send(job);
    }

    fn relayout(&mut self, width: f32, view: View) {
        let key = (width.round() as u32, view.wrap, view.cw.to_bits());
        if !self.layout_dirty && key == self.layout_key {
            return;
        }
        self.layout_dirty = false;
        self.layout_key = key;
        self.tops.clear();
        let mut y = 0u32;
        for row in &mut self.files {
            self.tops.push(y);
            y += FILE_H;
            if row.open && let Some(doc) = &mut row.doc {
                let cols = doc.cols_for(width, view.cw);
                doc.layout(cols, view.wrap);
                y += doc.height();
            }
        }
        self.tops.push(y);
    }

    fn set_all_open(&mut self, open: bool) {
        for row in &mut self.files {
            row.open = open;
        }
        self.layout_dirty = true;
        self.reconcile();
    }
}

/// What a click on a file row asked for, applied once the list is done drawing.
enum RowEvent {
    Toggle(usize),
    Menu(usize, Rect),
}

#[derive(Clone, Copy)]
enum RowAction {
    Stage,
    Unstage,
    Discard,
    CopyPath,
}

pub struct ReviewPane {
    repos: Vec<Repo>,
    active: usize,
    wrap: bool,
    words: bool,
    /// The file the row menu belongs to. By path, because the list can be rebuilt while the menu is open.
    menu_file: Option<(String, Rect)>,
    /// The row a press landed on, which may scroll out of view before its release does.
    pressed: u64,
}

impl ReviewPane {
    pub fn new(repos: Vec<PathBuf>, wake: Wake) -> Self {
        Self { repos: repos.into_iter().map(|root| Repo::new(root, &wake)).collect(), active: 0, wrap: true, words: true, menu_file: None, pressed: 0 }
    }

    /// Asks for fresh status, e.g. after an agent edited files. Coalesced.
    pub fn refresh(&mut self) {
        for repo in &mut self.repos {
            repo.want_status = true;
            repo.reconcile();
        }
    }

    /// Summary for the chips and buttons elsewhere, of the repository whose tab is selected: (branch, changed files,
    /// added, removed). None until loaded or if not a repo.
    pub fn summary(&self) -> Option<(String, u32, u32, u32)> {
        let repo = self.repos.get(self.active)?;
        if !repo.loaded || repo.not_repo || repo.content_revision.is_empty() {
            return None;
        }
        let narrow = |v: u64| v.min(u32::MAX as u64) as u32;
        Some((repo.branch.clone(), repo.files.len() as u32, narrow(repo.additions), narrow(repo.deletions)))
    }

    /// Takes in what the workers finished. The chips read `summary` whether or not the pane is open, so this cannot wait for `show`.
    pub fn poll(&mut self) {
        for repo in &mut self.repos {
            repo.drain();
        }
    }

    pub fn show(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        self.poll();
        ui.with_clip(r, |ui| self.body(ui, id, r));
        // A row that scrolled away while pressed never saw its release, and an `active` nobody clears freezes the whole UI.
        if !ui.input.down && self.pressed != 0 {
            if ui.mem.active == self.pressed {
                ui.mem.active = 0;
            }
            self.pressed = 0;
        }
    }

    fn body(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        let mut y = r.y;
        if self.repos.len() > 1 {
            self.tabs(ui, id, rect(r.x, y, r.w, TAB_H));
            y += TAB_H;
        }
        if self.repos.is_empty() {
            ui.label_centered(rect(r.x, y, r.w, 64.0), "没有仓库", &CAPTION, t.text_tertiary);
            return;
        }
        self.header(ui, id, rect(r.x, y, r.w, HEADER_H));
        y += HEADER_H;

        let repo = &mut self.repos[self.active];
        let mut bottom = r.bottom();
        if !repo.content_revision.is_empty() && !repo.not_repo {
            let band = commit_band_h(ui, repo, r.w);
            bottom -= band;
            commit_band(ui, id, repo, rect(r.x, bottom, r.w, band));
        }
        if repo.discard.is_some() {
            bottom -= CONFIRM_H;
            confirm_band(ui, id, repo, rect(r.x, bottom, r.w, CONFIRM_H));
        }
        let message = repo.action_error.clone().or_else(|| repo.status_error.clone());
        if let Some(message) = message {
            let text_w = (r.w - 44.0).max(40.0);
            let h = repo.notice.height(ui, &message, text_w) + 12.0;
            bottom -= h;
            notice_band(ui, id, repo, rect(r.x, bottom, r.w, h));
        }
        self.list(ui, id, rect(r.x, y, r.w, (bottom - y).max(0.0)));
        self.menus(ui, id, r);
    }

    fn tabs(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        let n = self.repos.len();
        let gap = 4.0;
        let w = ((r.w - 8.0 - gap * (n - 1) as f32) / n as f32).clamp(24.0, 200.0);
        for i in 0..n {
            let chip = rect(r.x + 4.0 + i as f32 * (w + gap), r.y + 3.0, w, r.h - 6.0);
            let resp = ui.interact(sub(id, 200 + i as u64), chip);
            let active = i == self.active;
            let fill = if active { t.subtle_hover.mix(t.subtle_pressed, resp.press_t) } else { t.subtle_hover.fade(resp.hover_t) };
            if active || resp.hover_t > 0.0 {
                ui.fill(chip, CONTROL_RADIUS, fill);
            }
            if resp.hovered {
                ui.cursor = CursorKind::Hand;
            }
            if resp.clicked {
                self.active = i;
                ui.mem.open_popup = 0;
            }
            let repo = &self.repos[i];
            let inner = rect(chip.x + 8.0, chip.y, chip.w - 16.0, chip.h);
            let (ty, color) = if active { (&CAPTION_STRONG, t.text_primary) } else { (&CAPTION, t.text_secondary) };
            let name_w = ui.measure(&repo.name, ty).min(if repo.branch.is_empty() { inner.w } else { inner.w * 0.6 });
            let cy = inner.y + (inner.h - ty.line) * 0.5;
            ui.label_ellipsis(inner.x, cy, name_w, &repo.name, ty, color);
            if !repo.branch.is_empty() {
                let arrow = inner.x + name_w + 9.0;
                ui.icon(Icon::ArrowRight, arrow, inner.y + inner.h * 0.5, 9.0, t.text_tertiary);
                let x = arrow + 9.0;
                ui.label_ellipsis(x, inner.y + (inner.h - FINE.line) * 0.5, (inner.right() - x).max(0.0), &repo.branch, &FINE, t.text_tertiary);
            }
        }
    }

    fn header(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let t = ui.theme;
        let repo = &mut self.repos[self.active];
        let cy = r.y + r.h * 0.5;
        let more = rect(r.right() - RIGHT_GUTTER - 22.0, r.y + 4.0, 22.0, 22.0);
        let refresh = rect(more.x - 24.0, r.y + 4.0, 22.0, 22.0);
        if ui.icon_button(sub(id, 2), refresh, Icon::Refresh, Some(t.text_secondary)).clicked {
            repo.want_status = true;
            repo.reconcile();
        }
        if ui.icon_button(sub(id, 1), more, Icon::MoreV, Some(t.text_secondary)).clicked {
            ui.toggle_popup(sub(id, 11));
        }
        ui.icon(Icon::Branch, r.x + 16.0, cy, 14.0, t.text_secondary);
        let x = r.x + 28.0;
        let mut right = refresh.x - 6.0;
        if !repo.files.is_empty() {
            let stats = format!("{} 个文件", repo.files.len());
            let w = ui.measure(&stats, &FINE);
            if right - w > x + 60.0 {
                right -= w;
                ui.label(right, r.y + (r.h - FINE.line) * 0.5, &stats, &FINE, t.text_tertiary);
                right -= 8.0;
            }
        }
        let branch = if repo.branch.is_empty() { if repo.loaded { "—" } else { "正在读取…" } } else { repo.branch.as_str() };
        let w = ui.measure(branch, &BODY_SMALL_STRONG).min((right - x).max(0.0));
        ui.label_ellipsis(x, r.y + (r.h - BODY_SMALL_STRONG.line) * 0.5, w, branch, &BODY_SMALL_STRONG, t.text_primary);
        let mut after = x + w + 8.0;
        for (count, arrow) in [(repo.ahead, "↑"), (repo.behind, "↓")] {
            if count > 0 && after + 24.0 < right {
                after += ui.label(after, r.y + (r.h - FINE.line) * 0.5, &format!("{arrow}{count}"), &FINE, t.text_secondary) + 4.0;
            }
        }
        ui.fill(rect(r.x, r.bottom() - 1.0, r.w, 1.0), 0.0, t.divider);
    }

    fn list(&mut self, ui: &mut Ui<'_>, id: u64, list: Rect) {
        let t = ui.theme;
        let view = View { cw: ui.measure("0", &MONO_BODY), wrap: self.wrap, words: self.words };
        let content_w = (list.w - RIGHT_GUTTER).max(0.0);
        let menu_id = sub(id, 10);
        let repo = &mut self.repos[self.active];

        let empty = if !repo.loaded {
            Some("正在读取 Git 状态…")
        } else if repo.not_repo {
            Some("这个文件夹不是 Git 仓库的根目录")
        } else if repo.files.is_empty() && repo.status_error.is_none() {
            Some("工作区没有变更")
        } else {
            None
        };
        if let Some(empty) = empty {
            if repo.loaded && !repo.not_repo {
                ui.icon(Icon::Check, list.x + list.w * 0.5, list.y + 40.0, 18.0, t.text_tertiary);
            }
            ui.label_centered(rect(list.x, list.y + 54.0, list.w, 24.0), empty, &CAPTION, t.text_tertiary);
            return;
        }

        repo.relayout(content_w, view);
        // A list has no end to stick to: expanding the last file must not throw the reader to the bottom of it.
        repo.scroll.stick = false;
        let content = *repo.tops.last().unwrap_or(&0);
        repo.scroll.update(ui, sub(id, 3), list, content as f32);
        let offset = repo.scroll.offset as f64;

        let mut event = None;
        let mut pressed = self.pressed;
        let n = repo.files.len();
        let first = repo.tops[..n].partition_point(|&top| top as f64 <= offset).saturating_sub(1);
        ui.with_clip(list, |ui| {
            for i in first..n {
                let top = list.y as f64 + repo.tops[i] as f64 - offset;
                if top >= list.bottom() as f64 {
                    break;
                }
                let bottom = list.y as f64 + repo.tops[i + 1] as f64 - offset;
                // The header of the file being read stays in view until the next file pushes it off.
                let header_y = if i == first { top.max((list.y as f64).min(bottom - FILE_H as f64)) } else { top } as f32;
                let row = &repo.files[i];
                if row.open && let Some(doc) = &row.doc {
                    let body_y = header_y + FILE_H as f32;
                    let body = rect(list.x, body_y, list.w, (list.bottom() - body_y).max(0.0));
                    ui.with_clip(body, |ui| {
                        let visible_bottom = (bottom as f32).min(body.bottom());
                        ui.fill(rect(list.x, body_y, list.w, (visible_bottom - body_y).max(0.0)), 0.0, t.code_bg);
                        doc.paint(ui, list.x, list.w, top + FILE_H as f64, view);
                    });
                }
                let id = sub(id, 1000) ^ row.key;
                if let Some(e) = file_row(ui, id, row, i, rect(list.x, header_y, list.w, FILE_H as f32), &mut pressed) {
                    event = Some(e);
                }
            }
        });
        self.pressed = pressed;

        let repo = &mut self.repos[self.active];
        match event {
            Some(RowEvent::Toggle(i)) => {
                // Folding the file whose header had stuck: land on that header rather than somewhere far below it.
                let top = repo.tops[i] as f32;
                if repo.files[i].open && top < repo.scroll.offset {
                    let back = top - repo.scroll.offset;
                    repo.scroll.shift(back);
                }
                repo.files[i].open = !repo.files[i].open;
                repo.layout_dirty = true;
                repo.reconcile();
            }
            Some(RowEvent::Menu(i, anchor)) => {
                let same = self.menu_file.as_ref().is_some_and(|(p, _)| *p == repo.files[i].change.path);
                if ui.mem.open_popup == menu_id && same {
                    ui.mem.open_popup = 0;
                } else {
                    self.menu_file = Some((repo.files[i].change.path.clone(), anchor));
                    ui.mem.open_popup = menu_id;
                }
            }
            None => {}
        }
    }

    fn menus(&mut self, ui: &mut Ui<'_>, id: u64, r: Rect) {
        let header_y = r.y + if self.repos.len() > 1 { TAB_H } else { 0.0 };
        let repo = &mut self.repos[self.active];

        let overflow = sub(id, 11);
        if ui.mem.open_popup == overflow {
            let anchor = rect(r.right() - RIGHT_GUTTER - 22.0, header_y + 4.0, 22.0, 22.0);
            let total: u64 = repo.additions + repo.deletions;
            let can_expand = repo.files.len() <= MAX_EXPAND_ALL_FILES && total <= MAX_EXPAND_ALL_LINES;
            let any_stageable = repo.files.iter().any(FileRow::stageable);
            let any_staged = repo.staged > 0;
            let items = [
                MenuItem::new("展开所有文件").enabled(can_expand && !repo.files.is_empty()).hint(if can_expand { "" } else { "变更太多" }),
                MenuItem::new("折叠所有文件").enabled(!repo.files.is_empty()),
                MenuItem::new("暂存全部").icon(Icon::Plus).enabled(any_stageable && !repo.busy).separated(),
                MenuItem::new("取消暂存全部").enabled(any_staged && !repo.busy),
                MenuItem::new("自动换行").checked(self.wrap).separated(),
                MenuItem::new("高亮改动的词").checked(self.words),
            ];
            match ui.menu(overflow, anchor, true, 180.0, &items) {
                Some(0) => repo.set_all_open(true),
                Some(1) => repo.set_all_open(false),
                Some(2) => {
                    let paths = repo.files.iter().filter(|f| f.stageable()).map(|f| f.change.path.clone()).collect();
                    repo.run(Job::Action(GitAction::Stage { paths }));
                }
                Some(3) => {
                    let paths = repo.files.iter().filter(|f| f.change.staged).map(|f| f.change.path.clone()).collect();
                    repo.run(Job::Action(GitAction::Unstage { paths }));
                }
                Some(4) => self.wrap = !self.wrap,
                Some(5) => self.words = !self.words,
                _ => {}
            }
            return;
        }

        let menu_id = sub(id, 10);
        if ui.mem.open_popup != menu_id {
            return;
        }
        let Some((path, anchor)) = self.menu_file.clone() else { return };
        let Some(&i) = repo.index.get(&path) else {
            ui.mem.open_popup = 0;
            return;
        };
        let row = &repo.files[i];
        let mut items = Vec::with_capacity(4);
        let mut actions = Vec::with_capacity(4);
        if row.stageable() {
            items.push(MenuItem::new("暂存").icon(Icon::Plus).enabled(!repo.busy));
            actions.push(RowAction::Stage);
        }
        if row.change.staged {
            items.push(MenuItem::new("取消暂存").enabled(!repo.busy));
            actions.push(RowAction::Unstage);
        }
        if row.discardable() {
            items.push(MenuItem::new(if row.change.untracked { "删除未跟踪文件" } else { "放弃更改" }).icon(Icon::Trash).danger().enabled(!repo.busy));
            actions.push(RowAction::Discard);
        }
        items.push(MenuItem::new("复制路径").icon(Icon::Copy).separated());
        actions.push(RowAction::CopyPath);
        let untracked = row.change.untracked;
        // A kebab's menu hangs back from the pane's edge, and a right click's opens from the pointer.
        let chosen = ui.menu(menu_id, anchor, anchor.w > 1.0, 170.0, &items);
        match chosen.map(|c| actions[c]) {
            Some(RowAction::Stage) => repo.run(Job::Action(GitAction::Stage { paths: vec![path] })),
            Some(RowAction::Unstage) => repo.run(Job::Action(GitAction::Unstage { paths: vec![path] })),
            Some(RowAction::Discard) => repo.run(Job::PrepareDiscard { path, untracked }),
            Some(RowAction::CopyPath) => ui.clipboard.set(&path),
            None => {}
        }
    }
}

fn file_row(ui: &mut Ui<'_>, id: u64, row: &FileRow, i: usize, r: Rect, pressed: &mut u64) -> Option<RowEvent> {
    let t = ui.theme;
    let kebab = rect(r.right() - RIGHT_GUTTER - 22.0, r.y + (r.h - 22.0) * 0.5, 22.0, 22.0);
    let hit = rect(r.x, r.y, (kebab.x - r.x).max(0.0), r.h);
    let resp = ui.interact(id, hit);
    if resp.pressed {
        *pressed = id;
    }
    let mut event = None;
    if resp.clicked {
        event = Some(RowEvent::Toggle(i));
    }
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    let hovered = ui.pointer_in(&r);
    if ui.input.right_pressed && hovered {
        let (px, py) = ui.input.pointer.unwrap_or((r.x, r.y));
        event = Some(RowEvent::Menu(i, rect(px, py, 1.0, 1.0)));
    }

    ui.fill(r, 0.0, t.card_secondary);
    if hovered || resp.hover_t > 0.0 {
        ui.fill(r, 0.0, t.subtle_hover.fade(if hovered { resp.hover_t.max(0.5) } else { resp.hover_t }));
    }
    ui.fill(rect(r.x, r.bottom() - 1.0, r.w, 1.0), 0.0, t.divider);

    let k = ui.icon_button(id ^ 0x55, kebab, Icon::MoreV, Some(t.text_secondary));
    if k.pressed {
        *pressed = id ^ 0x55;
    }
    if k.clicked {
        event = Some(RowEvent::Menu(i, kebab));
    }

    let cy = r.y + r.h * 0.5;
    ui.icon(if row.open { Icon::ChevronDown } else { Icon::ChevronRight }, r.x + 15.0, cy, 11.0, t.text_tertiary);
    ui.icon(Icon::File, r.x + 32.0, cy, 14.0, t.text_secondary);
    let name_x = r.x + 44.0;

    let letter = row.letter();
    let letter_color = match letter {
        'A' | 'U' => t.success,
        'D' | '!' => t.critical,
        'R' | 'C' => t.attention,
        'M' | 'T' => t.caution,
        _ => t.text_tertiary,
    };
    let mut buf = [0u8; 4];
    let letter = letter.encode_utf8(&mut buf);
    let letter_w = ui.measure(letter, &FINE_STRONG);
    let mut right = kebab.x - 4.0 - letter_w;
    ui.label(right, r.y + (r.h - FINE_STRONG.line) * 0.5, letter, &FINE_STRONG, letter_color);
    right -= 6.0;

    let c = &row.change;
    let path = c.path.as_str();
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let fine_y = r.y + (r.h - FINE.line) * 0.5;

    let note = match (c.staged, c.unstaged) {
        (true, true) => "部分暂存",
        (true, false) => "已暂存",
        _ => "",
    };
    let note_w = if note.is_empty() { 0.0 } else { ui.measure(note, &FINE) + 6.0 };

    let (plus, minus, stats_w) = match row.counts() {
        Some((a, d)) => {
            let (plus, minus) = (format!("+{a}"), format!("−{d}"));
            let w = ui.measure(&plus, &FINE) + 4.0 + ui.measure(&minus, &FINE) + 6.0;
            (plus, minus, w)
        }
        None => (String::new(), String::new(), 0.0),
    };

    let room = (right - name_x - stats_w).max(0.0);
    // The note gives way before the name does.
    let note_w = if room - note_w < 60.0 { 0.0 } else { note_w };
    let name_w = ui.measure(name, &CAPTION).min(room - note_w);
    ui.label_ellipsis(name_x, r.y + (r.h - CAPTION.line) * 0.5, name_w, name, &CAPTION, t.text_primary);
    let mut x = name_x + name_w + 6.0;
    if stats_w > 0.0 {
        x += ui.label(x, fine_y, &plus, &FINE, t.success) + 4.0;
        x += ui.label(x, fine_y, &minus, &FINE, t.critical) + 6.0;
    } else if c.binary {
        x += ui.label(x, fine_y, "二进制", &FINE, t.text_tertiary) + 6.0;
    }
    if note_w > 0.0 {
        ui.label(right - note_w + 6.0, fine_y, note, &FINE, t.text_tertiary);
    }
    // What is left says where the file lives. A rename says where it came from instead.
    let detail_end = right - note_w;
    if detail_end - x > 24.0 {
        match &c.original_path {
            Some(old) => ui.label_ellipsis(x, fine_y, detail_end - x, &format!("← {old}"), &FINE, t.text_tertiary),
            None if !dir.is_empty() => ui.label_ellipsis(x, fine_y, detail_end - x, dir, &FINE, t.text_tertiary),
            None => {}
        }
    }
    event
}

fn commit_band_h(ui: &mut Ui<'_>, repo: &mut Repo, w: f32) -> f32 {
    let text_w = (w - COMMIT_BUTTON_W - 22.0 - COMMIT_PAD * 3.0).max(40.0);
    let text = repo.commit.measure(ui, &BODY_SMALL, text_w).min(BODY_SMALL.line * MAX_COMMIT_LINES);
    text + 12.0 + COMMIT_PAD * 2.0
}

fn commit_band(ui: &mut Ui<'_>, id: u64, repo: &mut Repo, r: Rect) {
    let t = ui.theme;
    ui.fill(rect(r.x, r.y, r.w, 1.0), 0.0, t.divider);
    let inner = rect(r.x + COMMIT_PAD, r.y + COMMIT_PAD, r.w - COMMIT_PAD * 2.0, r.h - COMMIT_PAD * 2.0);
    let button = rect(inner.right() - COMMIT_BUTTON_W, inner.y, COMMIT_BUTTON_W, 30.0);
    let edit = rect(inner.x, inner.y, (button.x - COMMIT_PAD - inner.x).max(0.0), inner.h);
    let edit_id = sub(id, 40);
    repo.commit.show(ui, edit_id, edit, "提交信息…", &BODY_SMALL, false);
    let message = repo.commit.text.trim();
    let can = repo.staged > 0 && !message.is_empty() && !repo.busy;
    let resp = ui.accent_button(sub(id, 41), button, "提交", can);
    let shortcut = ui.mem.focus == edit_id && ui.input.keys.iter().any(|k| k.key == Key::Enter && k.cmd);
    if can && (resp.clicked || shortcut) {
        let message = message.to_owned();
        repo.run(Job::Commit(message));
    }
}

fn confirm_band(ui: &mut Ui<'_>, id: u64, repo: &mut Repo, r: Rect) {
    let t = ui.theme;
    let Some(pending) = repo.discard.clone() else { return };
    ui.fill(r, 0.0, t.card_secondary);
    ui.fill(rect(r.x, r.y, r.w, 1.0), 0.0, t.divider);
    let name = pending.path.rsplit('/').next().unwrap_or(&pending.path);
    let question = if pending.untracked { format!("永久删除 {name}？这个文件不在 Git 里，无法撤销。") } else { format!("放弃 {name} 未暂存的更改？无法撤销。") };
    ui.label_ellipsis(r.x + 10.0, r.y + 8.0, (r.w - 20.0).max(0.0), &question, &CAPTION, t.text_primary);
    let y = r.y + 32.0;
    let confirm_label = if pending.untracked { "永久删除" } else { "放弃更改" };
    let confirm = rect(r.right() - 10.0 - 84.0, y, 84.0, 28.0);
    let cancel = rect(confirm.x - 6.0 - 64.0, y, 64.0, 28.0);
    if ui.button(sub(id, 42), cancel, "取消").clicked || ui.input.keys.iter().any(|k| k.key == Key::Escape) {
        repo.discard = None;
        return;
    }
    let resp = ui.interact(sub(id, 43), confirm);
    let fill = t.critical.mix(t.critical.with_alpha(0.85), resp.hover_t).mix(t.critical.with_alpha(0.7), resp.press_t);
    ui.control(confirm, CONTROL_RADIUS, fill, Color::argb(0x14FFFFFF), Color::argb(0x66000000));
    ui.label_centered(confirm, confirm_label, &BODY, t.text_on_accent);
    if resp.hovered {
        ui.cursor = CursorKind::Hand;
    }
    if resp.clicked {
        repo.discard = None;
        repo.run(Job::Action(GitAction::Discard {
            paths: vec![pending.path],
            include_untracked: pending.untracked,
            expected_content_revision: pending.content_revision,
            expected_target_revision: pending.target_revision,
        }));
    }
}

fn notice_band(ui: &mut Ui<'_>, id: u64, repo: &mut Repo, r: Rect) {
    let t = ui.theme;
    ui.fill(rect(r.x, r.y, r.w, 1.0), 0.0, t.divider);
    ui.icon(Icon::Warning, r.x + 16.0, r.y + 14.0, 13.0, t.critical);
    let inner = rect(r.x + 28.0, r.y + 6.0, (r.w - 44.0).max(0.0), r.h - 12.0);
    if let Some(mut doc) = repo.notice.doc.take() {
        ui.with_clip(inner, |ui| ui.doc(&mut doc, inner.x, inner.y));
        repo.notice.doc = Some(doc);
    }
    let close = rect(r.right() - 22.0, r.y + 5.0, 18.0, 18.0);
    if ui.icon_button(sub(id, 44), close, Icon::Close, Some(t.text_tertiary)).clicked {
        repo.action_error = None;
        repo.status_error = None;
    }
}

#[cfg(test)]
impl ReviewPane {
    pub(crate) fn file_count(&self) -> usize {
        self.repos[self.active].files.len()
    }
    pub(crate) fn doc_of(&self, path: &str) -> Option<&CodeDoc> {
        let repo = &self.repos[self.active];
        repo.files[*repo.index.get(path)?].doc.as_deref()
    }
    pub(crate) fn status_requests(&self) -> u32 {
        self.repos[self.active].status_requests
    }
    pub(crate) fn settled(&self) -> bool {
        let repo = &self.repos[self.active];
        repo.loaded && !repo.status_busy && !repo.want_status && !repo.busy && repo.files.iter().all(|f| f.inflight == 0)
    }
    pub(crate) fn error(&self) -> Option<&str> {
        let repo = &self.repos[self.active];
        repo.action_error.as_deref().or(repo.status_error.as_deref())
    }
    pub(crate) fn asking_discard(&self) -> bool {
        self.repos[self.active].discard.is_some()
    }
    pub(crate) fn staged(&self) -> u32 {
        self.repos[self.active].staged
    }
    pub(crate) fn commit_text(&self) -> &str {
        &self.repos[self.active].commit.text
    }
    pub(crate) fn scroll_offset(&self) -> f32 {
        self.repos[self.active].scroll.offset
    }
    pub(crate) fn scroll_max(&self) -> f32 {
        self.repos[self.active].scroll.max_offset()
    }
}
