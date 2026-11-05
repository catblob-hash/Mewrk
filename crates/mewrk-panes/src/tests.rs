//! Scripted UI tests. They drive the panes through `Input` as the app would, against real Git and a real disk, and
//! need no GPU.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::theme::Theme;
use fluent::ui::{Clipboard, NoClipboard};
use fluent::{rect, Input, Key, KeyPress, Rect, Scene, TextCtx, Ui, UiMemory};

use crate::code::{CodeDoc, RowKind};
use crate::files::{FILTER_H, LIST_PAD, MAX_PREVIEW_BYTES, PATH_H, ROW_H};
use crate::review::{COMMIT_BUTTON_W, FILE_H, HEADER_H, TAB_H};
use crate::{FilesPane, ReviewPane, Wake, RIGHT_GUTTER};

const FRAME: f32 = 1.0 / 120.0;
const PANE: Rect = rect(0.0, 0.0, 480.0, 700.0);
const PANE_ID: u64 = 0xABCD;

struct Rig {
    scene: Scene,
    text: TextCtx,
    mem: UiMemory,
    input: Input,
    time: f32,
}

impl Rig {
    fn new() -> Self {
        Self { scene: Scene::default(), text: TextCtx::new(2.0), mem: UiMemory::default(), input: Input::default(), time: 0.0 }
    }

    fn frame<R>(&mut self, clipboard: &mut dyn Clipboard, f: impl FnOnce(&mut Ui<'_>) -> R) -> R {
        self.scene.clear();
        self.time += FRAME;
        let mut ui = Ui::new(&mut self.scene, &mut self.text, Theme::dark(), &self.input, &mut self.mem, clipboard, 2.0, (1000.0, 800.0), FRAME, self.time);
        let out = f(&mut ui);
        self.input.end_frame();
        out
    }

    fn glyphs(&self) -> usize {
        self.scene.counts().1
    }

    fn move_to(&mut self, x: f32, y: f32) {
        self.input.pointer = Some((x, y));
    }

    fn key(&mut self, key: Key, cmd: bool) {
        self.input.keys.push(KeyPress { key, shift: false, cmd, alt: false, ctrl: false });
    }
}

fn no_wake() -> Wake {
    Arc::new(|| {})
}

trait Pane {
    fn draw(&mut self, ui: &mut Ui<'_>);
}
impl Pane for ReviewPane {
    fn draw(&mut self, ui: &mut Ui<'_>) {
        self.show(ui, PANE_ID, PANE);
    }
}
impl Pane for FilesPane {
    fn draw(&mut self, ui: &mut Ui<'_>) {
        self.show(ui, PANE_ID, PANE);
    }
}

fn draw(rig: &mut Rig, pane: &mut impl Pane) {
    rig.frame(&mut NoClipboard, |ui| pane.draw(ui));
}

fn draw_with(rig: &mut Rig, pane: &mut impl Pane, clipboard: &mut dyn Clipboard) {
    rig.frame(clipboard, |ui| pane.draw(ui));
}

/// Press and release at a point, with a frame between them and one each side, the way a real click arrives.
fn click(rig: &mut Rig, pane: &mut impl Pane, x: f32, y: f32) {
    rig.move_to(x, y);
    draw(rig, pane);
    rig.input.down = true;
    rig.input.pressed = true;
    draw(rig, pane);
    rig.input.down = false;
    rig.input.released = true;
    draw(rig, pane);
    draw(rig, pane);
}

fn right_click(rig: &mut Rig, pane: &mut impl Pane, x: f32, y: f32) {
    rig.move_to(x, y);
    draw(rig, pane);
    rig.input.right_pressed = true;
    draw(rig, pane);
    draw(rig, pane);
}

/// Draws frames until `done`, the way the real loop keeps going until the background work has landed.
fn until<P: Pane>(rig: &mut Rig, pane: &mut P, what: &str, mut done: impl FnMut(&mut P) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        draw(rig, pane);
        if done(pane) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let n = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mewrk-panes-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // On macOS the temp dir is behind a symlink, and Git reports the real path.
        Self(dir.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, text: impl AsRef<[u8]>) {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git").current_dir(&self.0).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// A repository with `a.rs` and `src/b.ts` committed, on a branch of a known name.
    fn repo() -> Self {
        let dir = Self::new();
        dir.git(&["init", "-q", "-b", "trunk"]);
        for (k, v) in [("user.name", "T"), ("user.email", "t@example.com"), ("commit.gpgsign", "false"), ("core.hooksPath", ".no-hooks")] {
            dir.git(&["config", k, v]);
        }
        dir.write("a.rs", "fn main() {\n    let a = 1;\n}\n");
        dir.write("src/b.ts", "export const b = 1;\n");
        dir.git(&["add", "-A"]);
        dir.git(&["commit", "-q", "-m", "first"]);
        dir
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------- review

/// The middle of the nth file row, in a pane with no repository tabs and nothing scrolled.
fn file_y(n: usize) -> f32 {
    PANE.y + HEADER_H + (n as u32 * FILE_H) as f32 + FILE_H as f32 * 0.5
}

fn kebab_x() -> f32 {
    PANE.right() - RIGHT_GUTTER - 11.0
}

/// Chooses a row of the flyout hanging from a kebab at `y`, `n` rows down. The arithmetic mirrors `Ui::menu`.
fn pick_from_kebab(rig: &mut Rig, pane: &mut impl Pane, y: f32, n: usize, separators_before: usize) {
    click(rig, pane, kebab_x(), y);
    let top = y + 11.0 + 4.0 + 4.0;
    let row_y = top + n as f32 * 30.0 + separators_before as f32 * 9.0 + 15.0;
    click(rig, pane, kebab_x() - 40.0, row_y);
}

fn loaded_review(dir: &TempDir) -> (Rig, ReviewPane) {
    let mut rig = Rig::new();
    rig.move_to(-10.0, -10.0);
    let mut pane = ReviewPane::new(vec![dir.path().to_owned()], no_wake());
    until(&mut rig, &mut pane, "the first status", |p| p.settled());
    (rig, pane)
}

fn commit_edit_y() -> f32 {
    // One line of text in the box.
    PANE.bottom() - 6.0 - 15.0
}

#[test]
fn summary_says_what_changed_and_nothing_until_then() {
    let dir = TempDir::repo();
    dir.write("a.rs", "fn main() {\n    let a = 2;\n}\n");
    let mut rig = Rig::new();
    let mut pane = ReviewPane::new(vec![dir.path().to_owned()], no_wake());
    assert_eq!(pane.summary(), None, "nothing is known before the first status lands");
    until(&mut rig, &mut pane, "the first status", |p| p.settled());
    assert_eq!(pane.summary(), Some(("trunk".to_owned(), 1, 1, 1)));
}

#[test]
fn a_folder_that_is_no_repository_is_said_quietly() {
    let dir = TempDir::new();
    let (mut rig, mut pane) = loaded_review(&dir);
    assert_eq!(pane.summary(), None);
    assert_eq!(pane.error(), None, "not being a repository is not an error");
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > 0);
}

#[test]
fn a_missing_folder_is_an_inline_error_with_no_commit_box() {
    let dir = TempDir::new();
    let mut rig = Rig::new();
    let mut pane = ReviewPane::new(vec![dir.path().join("gone")], no_wake());
    until(&mut rig, &mut pane, "the failure", |p| p.error().is_some());
    assert_eq!(pane.summary(), None);
    // The box would take this click, were it there.
    click(&mut rig, &mut pane, 40.0, commit_edit_y());
    rig.input.text.push_str("typed");
    draw(&mut rig, &mut pane);
    assert_eq!(pane.commit_text(), "");
}

#[test]
fn expanding_a_file_draws_its_diff_with_colours_and_marks() {
    let dir = TempDir::repo();
    dir.write("a.rs", "fn main() {\n    let a = 2;\n}\n");
    let (mut rig, mut pane) = loaded_review(&dir);
    assert_eq!(pane.file_count(), 1);
    draw(&mut rig, &mut pane);
    let collapsed = rig.glyphs();
    assert!(pane.doc_of("a.rs").is_none(), "no diff is read until one is asked for");

    click(&mut rig, &mut pane, 100.0, file_y(0));
    until(&mut rig, &mut pane, "the diff", |p| p.doc_of("a.rs").is_some() && p.settled());
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > collapsed + 20, "{} glyphs against {collapsed}", rig.glyphs());

    let doc = pane.doc_of("a.rs").unwrap();
    let kinds: Vec<RowKind> = doc.rows.iter().map(|r| r.kind).collect();
    assert_eq!(kinds, vec![RowKind::Code, RowKind::Del, RowKind::Add, RowKind::Code]);
    assert_eq!(doc.row_text(1), "    let a = 1;");
    assert_eq!(doc.row_marks(1), &[(12, 13)]);
    assert_eq!(doc.row_marks(2), &[(12, 13)]);

    click(&mut rig, &mut pane, 100.0, file_y(0));
    draw(&mut rig, &mut pane);
    assert_eq!(rig.glyphs(), collapsed, "folding it again draws what was there before");
}

#[test]
fn untracked_files_are_listed_and_can_be_expanded() {
    let dir = TempDir::repo();
    dir.write("new.py", "def run():\n    pass\n");
    let (mut rig, mut pane) = loaded_review(&dir);
    assert_eq!(pane.file_count(), 1);
    click(&mut rig, &mut pane, 100.0, file_y(0));
    until(&mut rig, &mut pane, "the untracked diff", |p| p.doc_of("new.py").is_some());
    let doc = pane.doc_of("new.py").unwrap();
    assert_eq!(doc.additions, 2);
    assert_eq!(doc.row_text(0), "def run():");
}

#[test]
fn staging_unstaging_and_committing_go_through_to_git() {
    let dir = TempDir::repo();
    dir.write("a.rs", "fn main() {\n    let a = 2;\n}\n");
    let (mut rig, mut pane) = loaded_review(&dir);

    // Stage is the first row of the menu.
    pick_from_kebab(&mut rig, &mut pane, file_y(0), 0, 0);
    until(&mut rig, &mut pane, "the stage", |p| p.settled() && p.staged() == 1);
    assert_eq!(dir.git(&["diff", "--cached", "--name-only"]), "a.rs");
    assert_eq!(pane.error(), None);

    // Fully staged now, so Unstage is the first row.
    pick_from_kebab(&mut rig, &mut pane, file_y(0), 0, 0);
    until(&mut rig, &mut pane, "the unstage", |p| p.settled() && p.staged() == 0);
    assert_eq!(dir.git(&["diff", "--cached", "--name-only"]), "");

    // With nothing staged the button does nothing, however full the box is.
    click(&mut rig, &mut pane, 40.0, commit_edit_y());
    rig.input.text.push_str("修复 a");
    draw(&mut rig, &mut pane);
    assert_eq!(pane.commit_text(), "修复 a");
    let button_x = PANE.right() - 6.0 - COMMIT_BUTTON_W * 0.5;
    click(&mut rig, &mut pane, button_x, commit_edit_y());
    until(&mut rig, &mut pane, "nothing to happen", |p| p.settled());
    assert_eq!(dir.git(&["log", "-1", "--format=%s"]), "first");

    pick_from_kebab(&mut rig, &mut pane, file_y(0), 0, 0);
    until(&mut rig, &mut pane, "the second stage", |p| p.settled() && p.staged() == 1);
    click(&mut rig, &mut pane, button_x, commit_edit_y());
    until(&mut rig, &mut pane, "the commit", |p| p.settled() && p.file_count() == 0);
    assert_eq!(dir.git(&["log", "-1", "--format=%s"]), "修复 a");
    assert_eq!(pane.commit_text(), "", "a commit that went through empties the box");
    assert_eq!(pane.error(), None);
}

#[test]
fn a_commit_git_refuses_shows_inline_and_keeps_the_message() {
    let dir = TempDir::repo();
    dir.write(".no-hooks/pre-commit", "#!/bin/sh\necho hook says no >&2\nexit 1\n");
    Command::new("chmod").args(["+x", ".no-hooks/pre-commit"]).current_dir(dir.path()).status().unwrap();
    dir.write("a.rs", "changed\n");
    dir.git(&["add", "a.rs"]);
    let (mut rig, mut pane) = loaded_review(&dir);
    click(&mut rig, &mut pane, 40.0, commit_edit_y());
    rig.input.text.push_str("nope");
    draw(&mut rig, &mut pane);
    let before = rig.glyphs();
    click(&mut rig, &mut pane, PANE.right() - 6.0 - COMMIT_BUTTON_W * 0.5, commit_edit_y());
    until(&mut rig, &mut pane, "the refusal", |p| p.error().is_some() && p.settled());
    assert!(pane.error().unwrap().contains("hook says no"), "{:?}", pane.error());
    assert_eq!(pane.commit_text(), "nope");
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > before, "the message is on screen");
    assert_eq!(dir.git(&["log", "-1", "--format=%s"]), "first");
}

#[test]
fn discard_asks_inside_the_pane_first() {
    let dir = TempDir::repo();
    dir.write("a.rs", "changed\n");
    let (mut rig, mut pane) = loaded_review(&dir);

    // Stage, Discard, Copy path: a modified, unstaged file.
    pick_from_kebab(&mut rig, &mut pane, file_y(0), 1, 0);
    until(&mut rig, &mut pane, "the question", |p| p.asking_discard());
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "changed\n", "asking changes nothing");

    // Changing one's mind.
    rig.key(Key::Escape, false);
    draw(&mut rig, &mut pane);
    assert!(!pane.asking_discard());
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "changed\n");

    pick_from_kebab(&mut rig, &mut pane, file_y(0), 1, 0);
    until(&mut rig, &mut pane, "the question again", |p| p.asking_discard());
    // The confirm button sits in the band above the commit box.
    let confirm_y = PANE.bottom() - 42.0 - 68.0 + 32.0 + 14.0;
    click(&mut rig, &mut pane, PANE.right() - 10.0 - 42.0, confirm_y);
    until(&mut rig, &mut pane, "the discard", |p| p.settled() && p.file_count() == 0);
    // Under `core.autocrlf`, git writes it back with CRLF.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap().replace("\r\n", "\n"), "fn main() {\n    let a = 1;\n}\n");
    assert_eq!(pane.error(), None);
}

#[test]
fn discarding_after_the_file_moved_on_is_refused() {
    let dir = TempDir::repo();
    dir.write("a.rs", "changed\n");
    let (mut rig, mut pane) = loaded_review(&dir);
    pick_from_kebab(&mut rig, &mut pane, file_y(0), 1, 0);
    until(&mut rig, &mut pane, "the question", |p| p.asking_discard());
    // An agent edits the file while the question is up.
    dir.write("a.rs", "changed again\n");
    let confirm_y = PANE.bottom() - 42.0 - 68.0 + 32.0 + 14.0;
    click(&mut rig, &mut pane, PANE.right() - 10.0 - 42.0, confirm_y);
    until(&mut rig, &mut pane, "the refusal", |p| p.error().is_some() && p.settled());
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "changed again\n", "the newer edit survives");
}

/// A row of the overflow menu under the header's kebab, `n` rows down past `separators` separators.
fn overflow_pick(rig: &mut Rig, pane: &mut ReviewPane, n: usize, separators: usize) {
    click(rig, pane, kebab_x(), PANE.y + 15.0);
    click(rig, pane, kebab_x() - 40.0, PANE.y + 26.0 + 4.0 + 4.0 + n as f32 * 30.0 + separators as f32 * 9.0 + 15.0);
}

#[test]
fn the_overflow_menu_expands_folds_stages_and_stops_wrapping() {
    let dir = TempDir::repo();
    dir.write("a.rs", format!("// {}\n", "long ".repeat(100)));
    dir.write("src/b.ts", "export const b = 2;\n");
    let (mut rig, mut pane) = loaded_review(&dir);

    overflow_pick(&mut rig, &mut pane, 0, 0);
    until(&mut rig, &mut pane, "both diffs", |p| p.doc_of("a.rs").is_some() && p.doc_of("src/b.ts").is_some());
    draw(&mut rig, &mut pane);
    let wrapped = pane.doc_of("a.rs").unwrap().height();

    overflow_pick(&mut rig, &mut pane, 4, 2);
    draw(&mut rig, &mut pane);
    assert!(pane.doc_of("a.rs").unwrap().height() < wrapped, "without wrapping the long line takes one row");

    let open = rig.glyphs();
    overflow_pick(&mut rig, &mut pane, 1, 0);
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() < open, "folded");

    overflow_pick(&mut rig, &mut pane, 2, 1);
    until(&mut rig, &mut pane, "the stage", |p| p.settled() && p.staged() == 2);
    assert_eq!(dir.git(&["diff", "--cached", "--name-only"]), "a.rs\nsrc/b.ts");
    overflow_pick(&mut rig, &mut pane, 3, 1);
    until(&mut rig, &mut pane, "the unstage", |p| p.settled() && p.staged() == 0);
    assert_eq!(dir.git(&["diff", "--cached", "--name-only"]), "");
}

#[test]
fn right_click_copies_the_path() {
    struct Board(Option<String>);
    impl Clipboard for Board {
        fn get(&mut self) -> Option<String> {
            self.0.clone()
        }
        fn set(&mut self, text: &str) {
            self.0 = Some(text.to_owned());
        }
    }
    let dir = TempDir::repo();
    dir.write("src/b.ts", "changed\n");
    let (mut rig, mut pane) = loaded_review(&dir);
    let y = file_y(0);
    right_click(&mut rig, &mut pane, 200.0, y);
    // Stage, Discard, then Copy path after a separator, under the pointer.
    let row_y = y + 1.0 + 4.0 + 4.0 + 2.0 * 30.0 + 9.0 + 15.0;
    let mut board = Board(None);
    rig.move_to(250.0, row_y);
    draw_with(&mut rig, &mut pane, &mut board);
    rig.input.down = true;
    rig.input.pressed = true;
    draw_with(&mut rig, &mut pane, &mut board);
    rig.input.down = false;
    rig.input.released = true;
    draw_with(&mut rig, &mut pane, &mut board);
    assert_eq!(board.0.as_deref(), Some("src/b.ts"));
}

#[test]
fn refresh_coalesces_and_picks_up_edits() {
    let dir = TempDir::repo();
    let (mut rig, mut pane) = loaded_review(&dir);
    assert_eq!(pane.file_count(), 0);
    let before = pane.status_requests();

    dir.write("a.rs", "edited by an agent\n");
    for _ in 0..1000 {
        pane.refresh();
    }
    // One running and at most one more queued behind it, whatever the number of calls.
    assert_eq!(pane.status_requests(), before + 1);
    until(&mut rig, &mut pane, "the refresh", |p| p.settled() && p.file_count() == 1);
    assert_eq!(pane.status_requests(), before + 2);
    assert_eq!(pane.summary().unwrap().1, 1);
}

#[test]
fn an_open_diff_follows_the_file_without_flashing() {
    let dir = TempDir::repo();
    dir.write("a.rs", "one\n");
    let (mut rig, mut pane) = loaded_review(&dir);
    click(&mut rig, &mut pane, 100.0, file_y(0));
    until(&mut rig, &mut pane, "the diff", |p| p.doc_of("a.rs").is_some() && p.settled());
    let doc = pane.doc_of("a.rs").unwrap();
    assert_eq!(doc.row_text(doc.rows.len() - 1), "one");

    dir.write("a.rs", "two\n");
    pane.refresh();
    let mut gaps = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        draw(&mut rig, &mut pane);
        let doc = pane.doc_of("a.rs");
        // The old diff stays up until the new one has replaced it.
        gaps += doc.is_none() as u32;
        if doc.is_some_and(|d| d.row_text(d.rows.len() - 1) == "two") {
            break;
        }
        assert!(Instant::now() < deadline, "the diff never updated");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(gaps, 0);
}

#[test]
fn several_repositories_get_a_tab_each() {
    let (one, two) = (TempDir::repo(), TempDir::repo());
    two.git(&["checkout", "-q", "-b", "side"]);
    two.write("a.rs", "x\ny\n");
    let mut rig = Rig::new();
    rig.move_to(-10.0, -10.0);
    let mut pane = ReviewPane::new(vec![one.path().to_owned(), two.path().to_owned()], no_wake());
    until(&mut rig, &mut pane, "both statuses", |p| p.summary().is_some());
    assert_eq!(pane.summary().unwrap().0, "trunk");
    let chip = (PANE.w - 8.0 - 4.0) / 2.0;
    click(&mut rig, &mut pane, 4.0 + chip.min(200.0) + 4.0 + 10.0, TAB_H * 0.5);
    until(&mut rig, &mut pane, "the second tab", |p| p.summary().is_some_and(|s| s.0 == "side"));
    assert_eq!(pane.summary().unwrap().1, 1);
}

/// A rewrite of a 50,000-line file: a diff of 100,000 rows, which is the size that must stay smooth. Short lines, because
/// git-core cuts patches at 4 MiB.
fn big_diff_repo() -> TempDir {
    let dir = TempDir::repo();
    let body: String = (0..50_000).map(|i| format!("let value_{i} = {i};\n")).collect();
    dir.write("big.rs", body);
    dir.git(&["add", "big.rs"]);
    dir.git(&["commit", "-q", "-m", "big"]);
    let body: String = (0..50_000).map(|i| format!("let value_{i} = {i} + 1; // 中文\n")).collect();
    dir.write("big.rs", body);
    dir
}

#[test]
fn a_hundred_thousand_line_diff_draws_only_what_is_visible() {
    let dir = big_diff_repo();
    let (mut rig, mut pane) = loaded_review(&dir);
    click(&mut rig, &mut pane, 100.0, file_y(0));
    until(&mut rig, &mut pane, "the big diff", |p| p.doc_of("big.rs").is_some() && p.settled());
    let rows = pane.doc_of("big.rs").unwrap().rows.len();
    assert!(rows >= 100_000, "{rows} rows");
    assert_eq!(pane.error(), None);
    assert!(pane.doc_of("big.rs").unwrap().rows.iter().all(|r| r.kind != RowKind::Note), "the patch was not truncated");

    draw(&mut rig, &mut pane);
    let max = pane.scroll_max();
    assert!(max > 1_000_000.0, "{max}");

    // Rows in view times columns, and then some for the chrome. It has nothing to do with how long the diff is.
    let bound = (PANE.h as usize / 17 + 2) * (PANE.w as usize / 7 + 2) + 400;
    let before = Instant::now();
    let mut frames = 0;
    rig.move_to(200.0, 300.0);
    let mut worst = 0;
    for step in 0..400 {
        // Jumps as a dragged scrollbar would make them, then wheel steps.
        rig.input.scroll = if step % 2 == 0 { -(max / 200.0) } else { 40.0 };
        rig.input.scroll_is_pixels = true;
        draw(&mut rig, &mut pane);
        frames += 1;
        worst = worst.max(rig.glyphs());
        assert!(rig.glyphs() <= bound, "{} glyphs at frame {step}, bound {bound}", rig.glyphs());
    }
    assert!(pane.scroll_offset() > 0.0);
    assert!(worst > 200, "something was drawn: {worst}");
    eprintln!("100k-line diff: {rows} rows, {:?} per frame, at most {worst} glyphs", before.elapsed() / frames);

    // Sent to the very end, there is still a screenful there.
    rig.input.scroll = -1e9;
    rig.input.scroll_is_pixels = true;
    draw(&mut rig, &mut pane);
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > 200, "{} glyphs at the end", rig.glyphs());
}

#[test]
fn a_press_on_a_row_that_scrolls_away_does_not_freeze_the_ui() {
    let dir = TempDir::repo();
    for i in 0..200 {
        dir.write(&format!("many/f{i:03}.txt"), "x\n");
    }
    let (mut rig, mut pane) = loaded_review(&dir);
    assert_eq!(pane.file_count(), 200);
    rig.move_to(100.0, file_y(0));
    draw(&mut rig, &mut pane);
    rig.input.down = true;
    rig.input.pressed = true;
    draw(&mut rig, &mut pane);
    assert_ne!(rig.mem.active, 0);
    rig.input.scroll = -3000.0;
    rig.input.scroll_is_pixels = true;
    draw(&mut rig, &mut pane);
    rig.input.down = false;
    rig.input.released = true;
    draw(&mut rig, &mut pane);
    assert_eq!(rig.mem.active, 0, "a stuck `active` would dead-end every widget in the app");
}

// ---------------------------------------------------------------- files

fn tree_y(y: f32) -> f32 {
    PANE.y + PATH_H + y
}

fn loaded_files(dir: &Path) -> (Rig, FilesPane) {
    let mut rig = Rig::new();
    rig.move_to(-10.0, -10.0);
    let mut pane = FilesPane::new(dir.to_owned(), no_wake());
    until(&mut rig, &mut pane, "the root listing", |p| !p.tree().is_empty());
    (rig, pane)
}

fn names(tree: &[(u16, String, bool)]) -> Vec<&str> {
    tree.iter().map(|e| e.1.as_str()).collect()
}

fn click_row(rig: &mut Rig, pane: &mut FilesPane, name: &str) {
    let y = pane.row_y(name).unwrap_or_else(|| panic!("{name} is not in the tree"));
    click(rig, pane, 120.0, tree_y(y));
}

fn sample_tree() -> TempDir {
    let dir = TempDir::repo();
    dir.write(".gitignore", "*.log\nbuild/\n");
    dir.write("debug.log", "noise\n");
    dir.write("build/out.bin", "x\n");
    dir.write("Zeta.txt", "z\n");
    dir.write("docs/guide.md", "# Guide\n");
    dir.write("docs/deep/inner.rs", "fn inner() {}\n");
    dir
}

#[test]
fn the_tree_loads_directories_lazily_and_dims_what_git_ignores() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    let tree = pane.tree();
    assert_eq!(names(&tree), vec![".git", "build", "docs", "src", ".gitignore", "a.rs", "debug.log", "Zeta.txt"], "directories first, then by name without case");
    let dimmed: Vec<&str> = tree.iter().filter(|e| e.2).map(|e| e.1.as_str()).collect();
    assert_eq!(dimmed, vec![".git", "build", "debug.log"]);

    click_row(&mut rig, &mut pane, "docs");
    until(&mut rig, &mut pane, "docs to load", |p| p.tree().iter().any(|e| e.1 == "guide.md"));
    let tree = pane.tree();
    assert!(tree.contains(&(1, "guide.md".to_owned(), false)));
    assert!(tree.contains(&(1, "deep".to_owned(), false)));
    assert!(!tree.iter().any(|e| e.1 == "inner.rs"), "one level at a time");

    // Inside an ignored directory, everything is ignored.
    click_row(&mut rig, &mut pane, "build");
    until(&mut rig, &mut pane, "build to load", |p| p.tree().iter().any(|e| e.1 == "out.bin"));
    assert!(pane.tree().contains(&(1, "out.bin".to_owned(), true)));

    click_row(&mut rig, &mut pane, "docs");
    draw(&mut rig, &mut pane);
    draw(&mut rig, &mut pane);
    assert!(!pane.tree().iter().any(|e| e.1 == "guide.md"), "folding hides it again");
}

#[test]
fn refresh_finds_new_files_and_keeps_what_is_open() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    click_row(&mut rig, &mut pane, "docs");
    until(&mut rig, &mut pane, "docs to load", |p| p.tree().iter().any(|e| e.1 == "guide.md"));
    dir.write("docs/fresh.md", "new\n");
    dir.write("rootfile.md", "new\n");
    let refresh_x = PANE.right() - RIGHT_GUTTER - 11.0;
    click(&mut rig, &mut pane, refresh_x, PANE.y + 16.0);
    until(&mut rig, &mut pane, "the new names", |p| p.tree().iter().any(|e| e.1 == "fresh.md") && p.tree().iter().any(|e| e.1 == "rootfile.md"));
    assert!(pane.tree().iter().any(|e| e.1 == "guide.md"), "the open folder stayed open");
}

#[test]
fn a_big_directory_draws_only_what_is_visible() {
    let dir = TempDir::new();
    for i in 0..20_000 {
        std::fs::File::create(dir.path().join(format!("file-{i:05}.txt"))).unwrap();
    }
    let (mut rig, mut pane) = loaded_files(dir.path());
    assert_eq!(pane.tree().len(), 20_000);
    draw(&mut rig, &mut pane);
    let top = rig.glyphs();
    rig.move_to(100.0, 200.0);
    rig.input.scroll = -200_000.0;
    rig.input.scroll_is_pixels = true;
    draw(&mut rig, &mut pane);
    let bound = (PANE.h / ROW_H) as usize * 40 + 200;
    assert!(top <= bound, "{top}");
    assert!(rig.glyphs() <= bound, "{}", rig.glyphs());
    assert!(rig.glyphs() > 100, "{}", rig.glyphs());
}

fn focus_filter(rig: &mut Rig, pane: &mut FilesPane) {
    click(rig, pane, 100.0, PANE.bottom() - FILTER_H * 0.5);
}

#[test]
fn the_filter_finds_across_the_tree_fuzzily_and_leaves_out_what_git_ignores() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    focus_filter(&mut rig, &mut pane);
    rig.input.text.push_str("inner");
    draw(&mut rig, &mut pane);
    assert_eq!(pane.filter_text(), "inner");
    until(&mut rig, &mut pane, "the results", |p| p.hits_done());
    assert_eq!(pane.hits(), vec!["docs/deep/inner.rs"]);

    // Typing on: a subsequence matches too, and the best match comes first.
    rig.key(Key::Char('a'), true);
    draw(&mut rig, &mut pane);
    rig.key(Key::Backspace, false);
    rig.input.text.push_str("gde");
    draw(&mut rig, &mut pane);
    until(&mut rig, &mut pane, "the fuzzy results", |p| p.hits_done() && !p.hits().is_empty());
    assert_eq!(pane.hits()[0], "docs/guide.md");

    // Ignored files and Git's own are not looked at at all.
    rig.key(Key::Char('a'), true);
    draw(&mut rig, &mut pane);
    rig.key(Key::Backspace, false);
    rig.input.text.push_str("out.bin");
    draw(&mut rig, &mut pane);
    until(&mut rig, &mut pane, "no results", |p| p.hits_done());
    assert!(pane.hits().is_empty(), "{:?}", pane.hits());
    rig.key(Key::Char('a'), true);
    draw(&mut rig, &mut pane);
    rig.key(Key::Backspace, false);
    rig.input.text.push_str("HEAD");
    draw(&mut rig, &mut pane);
    until(&mut rig, &mut pane, "no results", |p| p.hits_done());
    assert!(pane.hits().iter().all(|h| !h.starts_with(".git/")), "{:?}", pane.hits());

    // Escape clears it and the tree comes back.
    rig.key(Key::Escape, false);
    draw(&mut rig, &mut pane);
    draw(&mut rig, &mut pane);
    assert_eq!(pane.filter_text(), "");
}

#[test]
fn clicking_a_result_opens_it() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    focus_filter(&mut rig, &mut pane);
    rig.input.text.push_str("inner");
    draw(&mut rig, &mut pane);
    until(&mut rig, &mut pane, "the results", |p| p.hits_done());
    click(&mut rig, &mut pane, 100.0, tree_y(LIST_PAD + ROW_H * 0.5));
    until(&mut rig, &mut pane, "the preview", |p| p.preview_state() == Some("text"));
    assert_eq!(pane.preview_doc().unwrap().row_text(0), "fn inner() {}");
}

#[test]
fn a_directory_result_becomes_the_tree() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    focus_filter(&mut rig, &mut pane);
    rig.input.text.push_str("deep");
    draw(&mut rig, &mut pane);
    until(&mut rig, &mut pane, "the results", |p| p.hits_done());
    assert_eq!(pane.hits()[0], "docs/deep");
    click(&mut rig, &mut pane, 100.0, tree_y(LIST_PAD + ROW_H * 0.5));
    until(&mut rig, &mut pane, "the directory", |p| p.tree().iter().any(|e| e.1 == "inner.rs"));
    assert_eq!(pane.current_dir(), dir.path().join("docs/deep"));
    assert_eq!(pane.filter_text(), "");
}

#[test]
fn clicking_a_file_previews_it_and_back_returns_to_the_tree() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    click_row(&mut rig, &mut pane, "a.rs");
    until(&mut rig, &mut pane, "the preview", |p| p.preview_state() == Some("text"));
    let doc = pane.preview_doc().unwrap();
    assert_eq!(doc.rows.len(), 3);
    assert_eq!(doc.row_no(1), 2);
    assert_eq!(doc.row_text(1), "    let a = 1;");
    draw(&mut rig, &mut pane);
    let glyphs = rig.glyphs();
    assert!(glyphs > 20, "{glyphs}");

    click(&mut rig, &mut pane, 16.0, PANE.y + 16.0);
    assert_eq!(pane.preview_state(), None);
    draw(&mut rig, &mut pane);
    assert!(pane.tree().iter().any(|e| e.1 == "a.rs"));
}

#[test]
fn a_large_preview_is_capped_says_so_and_draws_a_bounded_number_of_glyphs() {
    let dir = TempDir::new();
    let line = "let x = \"the same line, over and over, with some length to it\"; // 中文\n";
    dir.write("huge.rs", line.repeat((MAX_PREVIEW_BYTES as usize / line.len()) + 1000));
    let (mut rig, mut pane) = loaded_files(dir.path());
    click_row(&mut rig, &mut pane, "huge.rs");
    until(&mut rig, &mut pane, "the preview", |p| p.preview_state().is_some_and(|s| s != "loading"));
    assert_eq!(pane.preview_state(), Some("truncated"));
    let doc = pane.preview_doc().unwrap();
    assert_eq!(doc.rows.len(), MAX_PREVIEW_BYTES as usize / line.len());
    assert!(doc.row_text(doc.rows.len() - 1).ends_with("中文"), "cut at the end of a line");

    let bound = (PANE.h as usize / 17 + 2) * (PANE.w as usize / 7 + 2) + 200;
    draw(&mut rig, &mut pane);
    let max = pane.preview_scroll_max();
    assert!(max > 1_000_000.0, "{max}");
    rig.move_to(200.0, 300.0);
    for step in 0..50 {
        rig.input.scroll = if step % 2 == 0 { -(max / 25.0) } else { 40.0 };
        rig.input.scroll_is_pixels = true;
        draw(&mut rig, &mut pane);
        assert!(rig.glyphs() <= bound && rig.glyphs() > 100, "{} glyphs, bound {bound}", rig.glyphs());
    }
}

#[test]
fn one_enormous_line_stays_bounded_too() {
    let dir = TempDir::new();
    dir.write("min.js", format!("var a=1;{}\n", "b();".repeat(2_000_000 / 4)));
    let (mut rig, mut pane) = loaded_files(dir.path());
    click_row(&mut rig, &mut pane, "min.js");
    until(&mut rig, &mut pane, "the preview", |p| p.preview_state().is_some_and(|s| s != "loading"));
    let doc = pane.preview_doc().unwrap();
    assert_eq!(doc.rows.len(), 1);
    assert!(doc.row_text(0).len() < 40_000, "{}", doc.row_text(0).len());
    let bound = (PANE.h as usize / 17 + 2) * (PANE.w as usize / 7 + 2) + 200;
    rig.move_to(200.0, 300.0);
    for _ in 0..10 {
        rig.input.scroll = -500.0;
        rig.input.scroll_is_pixels = true;
        draw(&mut rig, &mut pane);
        assert!(rig.glyphs() <= bound && rig.glyphs() > 100, "{} glyphs, bound {bound}", rig.glyphs());
    }
}

#[test]
fn binary_files_and_images_say_so() {
    let dir = TempDir::new();
    dir.write("blob.dat", [0u8, 1, 2, 3, 0, 255]);
    dir.write("logo.PNG", [0x89u8, b'P', b'N', b'G']);
    dir.write("empty.txt", "");
    let (mut rig, mut pane) = loaded_files(dir.path());
    click_row(&mut rig, &mut pane, "blob.dat");
    until(&mut rig, &mut pane, "the verdict", |p| p.preview_state() == Some("binary"));
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > 0);
    click(&mut rig, &mut pane, 16.0, PANE.y + 16.0);
    click_row(&mut rig, &mut pane, "logo.PNG");
    until(&mut rig, &mut pane, "the verdict", |p| p.preview_state() == Some("image"));
    click(&mut rig, &mut pane, 16.0, PANE.y + 16.0);
    click_row(&mut rig, &mut pane, "empty.txt");
    until(&mut rig, &mut pane, "the verdict", |p| p.preview_state() == Some("text"));
    assert!(pane.preview_doc().unwrap().rows.is_empty());
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > 0, "an empty file says it is empty");
}

#[test]
fn row_menu_copies_the_path_and_mentions_once() {
    struct Board(Option<String>);
    impl Clipboard for Board {
        fn get(&mut self) -> Option<String> {
            self.0.clone()
        }
        fn set(&mut self, text: &str) {
            self.0 = Some(text.to_owned());
        }
    }
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());

    // Right click, then the second row of the menu, which opens under the pointer.
    let y = tree_y(pane.row_y("a.rs").unwrap());
    right_click(&mut rig, &mut pane, 200.0, y);
    click(&mut rig, &mut pane, 250.0, y + 1.0 + 4.0 + 4.0 + 30.0 + 15.0);
    assert_eq!(pane.take_mention(), Some(dir.path().join("a.rs")));
    assert_eq!(pane.take_mention(), None, "a mention is taken once");

    // The first row copies.
    let mut board = Board(None);
    right_click(&mut rig, &mut pane, 200.0, y);
    rig.move_to(250.0, y + 1.0 + 4.0 + 4.0 + 15.0);
    draw_with(&mut rig, &mut pane, &mut board);
    rig.input.down = true;
    rig.input.pressed = true;
    draw_with(&mut rig, &mut pane, &mut board);
    rig.input.down = false;
    rig.input.released = true;
    draw_with(&mut rig, &mut pane, &mut board);
    assert_eq!(board.0, Some(dir.path().join("a.rs").to_string_lossy().into_owned()));
    assert_eq!(pane.take_mention(), None, "copying is not mentioning");
}

#[test]
fn the_kebab_appears_under_the_pointer_and_opens_the_same_menu() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    let y = tree_y(pane.row_y("a.rs").unwrap());
    let x = PANE.x + 4.0 + (PANE.w - 4.0 - RIGHT_GUTTER) - 12.0;
    click(&mut rig, &mut pane, x, y);
    assert_ne!(rig.mem.open_popup, 0, "the kebab opened a menu");
    assert_eq!(pane.preview_state(), None, "and the row underneath did not open");
    // Below the kebab: 10 to its bottom edge, 4 of gap, 4 of padding, and then the second row.
    click(&mut rig, &mut pane, x - 40.0, y + 10.0 + 4.0 + 4.0 + 30.0 + 15.0);
    assert_eq!(pane.take_mention(), Some(dir.path().join("a.rs")));
}

#[test]
fn set_root_starts_over_and_stale_answers_are_dropped() {
    let (one, two) = (sample_tree(), TempDir::new());
    two.write("only-here.txt", "x\n");
    let (mut rig, mut pane) = loaded_files(one.path());
    click_row(&mut rig, &mut pane, "a.rs");
    until(&mut rig, &mut pane, "the preview", |p| p.preview_state() == Some("text"));
    pane.set_root(two.path().to_owned());
    assert_eq!(pane.preview_state(), None);
    until(&mut rig, &mut pane, "the new root", |p| p.tree().iter().any(|e| e.1 == "only-here.txt"));
    assert_eq!(pane.tree().len(), 1);
}

#[test]
fn the_breadcrumb_walks_up_and_a_missing_directory_is_said_quietly() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(&dir.path().join("docs"));
    assert!(pane.tree().iter().any(|e| e.1 == "guide.md"));
    click(&mut rig, &mut pane, 16.0, PANE.y + 16.0);
    until(&mut rig, &mut pane, "the parent", |p| p.tree().iter().any(|e| e.1 == "a.rs"));
    assert_eq!(pane.current_dir(), dir.path());

    let gone = dir.path().join("does-not-exist");
    pane.set_root(gone);
    until(&mut rig, &mut pane, "the failure", |p| !p.is_loading());
    draw(&mut rig, &mut pane);
    assert!(rig.glyphs() > 0);
    assert!(pane.tree().is_empty());
}

#[cfg(unix)]
#[test]
fn clicking_a_breadcrumb_goes_there() {
    // A short path, so that all of it fits: "/" then "usr" then "bin".
    let (mut rig, mut pane) = loaded_files(Path::new("/usr/bin"));
    click(&mut rig, &mut pane, 5.0 + 22.0 + 4.0 + 16.0, PANE.y + 16.0);
    assert_eq!(pane.current_dir(), Path::new("/usr"));
    until(&mut rig, &mut pane, "the listing", |p| p.tree().iter().any(|e| e.1 == "bin"));
}

#[test]
fn nothing_breaks_in_a_pane_too_small_to_use() {
    let dir = TempDir::repo();
    dir.write("a.rs", "changed\n");
    let (mut rig, mut review) = loaded_review(&dir);
    let (_, mut files) = loaded_files(dir.path());
    for r in [rect(0.0, 0.0, 0.0, 0.0), rect(5.0, 5.0, 40.0, 30.0), rect(0.0, 0.0, 120.0, 90.0)] {
        for _ in 0..3 {
            rig.frame(&mut NoClipboard, |ui| {
                review.show(ui, 1, r);
                files.show(ui, 2, r);
            });
        }
    }
}

/// Anything at all, wrapped at any width, without a panic: a bad slice here would take the app down.
#[test]
fn any_text_paints_at_any_width() {
    const BITS: [&str; 16] = ["a", "bc", " ", "\t", "中", "文字", "🐱", "é", "e\u{301}", "\u{200b}", "fn ", "'x'", "\"s", "// ", "<a>", "\u{1b}"];
    let mut rig = Rig::new();
    let lang = crate::highlight::lang_of("rs");
    let mut state = Default::default();
    let mut doc = CodeDoc::new(true);
    let mut rng = 0x9E37_79B9_u64;
    for i in 0..500 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let text: String = (0..(rng >> 58)).map(|n| BITS[((rng >> (n * 4 % 40)) % 16) as usize]).collect();
        let kind = [RowKind::Code, RowKind::Add, RowKind::Del][i % 3];
        doc.push_code(kind, i as u32 + 1, &text, lang, &mut state);
        let first = doc.row_text(i).chars().next().map_or(0, char::len_utf8) as u32;
        doc.set_marks(i, &[(0, first)]);
    }
    let view = crate::code::View { cw: 7.2, wrap: true, words: true };
    for cols in [1, 8, 9, 10, 13, 40, 200] {
        for wrap in [true, false] {
            doc.layout(cols, wrap);
            let height = doc.height() as f64;
            for step in 0..20 {
                let top = -height * step as f64 / 20.0;
                rig.frame(&mut NoClipboard, |ui| {
                    let clip = rect(0.0, 0.0, 500.0, 300.0);
                    ui.with_clip(clip, |ui| doc.paint(ui, 0.0, 500.0, top, crate::code::View { wrap, ..view }));
                });
            }
        }
    }
}

#[test]
fn nothing_in_a_settled_pane_keeps_asking_for_frames() {
    let dir = sample_tree();
    let (mut rig, mut pane) = loaded_files(dir.path());
    rig.move_to(-10.0, -10.0);
    let mut animating = true;
    for _ in 0..300 {
        animating = rig.frame(&mut NoClipboard, |ui| {
            pane.show(ui, PANE_ID, PANE);
            ui.animating
        });
    }
    assert!(!animating, "an idle pane must go quiet, or it would burn power for ever");

    let repo = TempDir::repo();
    repo.write("a.rs", "changed\n");
    let (mut rig, mut pane) = loaded_review(&repo);
    let mut animating = true;
    for _ in 0..300 {
        animating = rig.frame(&mut NoClipboard, |ui| {
            pane.show(ui, PANE_ID, PANE);
            ui.animating
        });
    }
    assert!(!animating);
}

#[test]
fn wake_is_called_when_results_land() {
    let woke = Arc::new(AtomicU32::new(0));
    let w = woke.clone();
    let wake: Wake = Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    });
    let dir = sample_tree();
    let _files = FilesPane::new(dir.path().to_owned(), wake.clone());
    let _review = ReviewPane::new(vec![dir.path().to_owned()], wake);
    let deadline = Instant::now() + Duration::from_secs(30);
    while woke.load(Ordering::Relaxed) < 2 {
        assert!(Instant::now() < deadline, "no wake within 30 s");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_doc_with_a_trailing_marker_on_no_language_still_paints_as_plain() {
    let mut doc = CodeDoc::new(true);
    doc.push_code(RowKind::Add, 1, "abc", None, &mut Default::default());
    doc.append_to_last(" ↵");
    assert_eq!(doc.row_token_kinds(0), vec![crate::highlight::Token::Plain, crate::highlight::Token::Comment]);
}
