//! The review pane on screen: `review_core`'s state machine joined to the host, and the view of it. Upstream's
//! `GitReviewPanel`, minus what the core holds.

use std::collections::HashMap;

use mewrk_client::prelude::*;

use crate::diff_view::{diff_view, DiffStyle, DiffViewProps};
use crate::git::*;
use crate::menu::{popover_menu, MenuItem, MenuSection};
use crate::path::path_text;
use crate::review_core::*;
use crate::spinner::spinner;
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};

/// What upstream passes the panel as callbacks.
#[derive(Clone)]
pub struct GitReviewHooks {
    /// Told when a read or a write found the checkout changed: the caller stores it where everything else reads it.
    pub on_snapshot_change: Rc<dyn Fn(&mut App, Option<GitWorkspaceSnapshot>)>,
    /// May a write start now? False means another task got there first.
    pub on_mutation_start: Rc<dyn Fn(&mut App) -> bool>,
    pub on_mutation_end: Rc<dyn Fn(&mut App)>,
    /// A reveal request has been acted on, so a later mount does not act on it again.
    pub on_reveal_handled: Rc<dyn Fn(&mut App, u64)>,
    /// Given, a line of a diff can be mentioned in the conversation from its menu.
    pub on_mention_line: Option<Rc<dyn Fn(&mut App, &str, u32)>>,
}

impl Default for GitReviewHooks {
    fn default() -> Self {
        Self { on_snapshot_change: Rc::new(|_, _| {}), on_mutation_start: Rc::new(|_| true), on_mutation_end: Rc::new(|_| {}), on_reveal_handled: Rc::new(|_, _| {}), on_mention_line: None }
    }
}

/// Everything the panel is given every frame.
pub struct GitReviewProps {
    /// Names the panel among all others. Upstream keys the component by the member and `git_review_snapshot_cache_key`:
    /// do the same, so that a checkout that moved starts over.
    pub key: String,
    pub inputs: ReviewInputs,
    pub hooks: GitReviewHooks,
    /// The pane's pages, one per workspace under review, when there is more than one. They take the title bar, and the
    /// diff scope moves into the pane's `⋮` menu.
    pub page_tabs: Option<E>,
}

/// What the panel draws. `SidePane` holds them: `header` in its title bar, `menu_sections` behind its `⋮`.
pub struct GitReviewView {
    pub header: E,
    pub menu_sections: Vec<MenuSection>,
    pub body: E,
}

#[derive(Default)]
struct ReviewStore {
    panels: HashMap<String, Panel>,
    /// `onMutationEnd` for each write that is out. A panel that goes while one is out has no hooks left, and the lease it
    /// took still has to go.
    mutation_ends: HashMap<String, Rc<dyn Fn(&mut App)>>,
}

/// The write this panel had out is over, whichever way, and nobody is left to say so.
fn end_orphaned_mutation(app: &mut App, key: &str) {
    if let Some(end) = app.local::<ReviewStore>().mutation_ends.remove(key) {
        end(app);
    }
}

struct Panel {
    core: ReviewCore,
    hooks: GitReviewHooks,
}

fn panel<'a>(app: &'a mut App, key: &str) -> Option<&'a mut Panel> {
    app.local::<ReviewStore>().panels.get_mut(key)
}

/// A mounted panel's state, for tests and for whoever needs to look.
pub fn review_core<'a>(app: &'a mut App, key: &str) -> Option<&'a mut ReviewCore> {
    panel(app, key).map(|p| &mut p.core)
}

struct MountGuard {
    task: Task,
    key: String,
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        let key = std::mem::take(&mut self.key);
        self.task.post(move |app| {
            app.local::<ReviewStore>().panels.remove(&key);
        });
    }
}

fn run(app: &mut App, key: &str, effects: Vec<Effect>) {
    for effect in effects {
        run_one(app, key, effect);
    }
    repaint();
}

fn run_one(app: &mut App, key: &str, effect: Effect) {
    let Some(p) = panel(app, key) else { return };
    let target = p.core.target.clone();
    let hooks = p.hooks.clone();
    let owned = key.to_owned();
    match effect {
        Effect::LoadPage { request_id, scope_key, cursor, request } => {
            get_git_change_page(app, &target, &request, move |app, result| {
                let Some(p) = panel(app, &owned) else { return };
                let effects = p.core.page_answer(request_id, &scope_key, cursor.as_deref(), result);
                run(app, &owned, effects);
            });
        }
        Effect::LoadScopeDiff { request_id, request } => {
            get_git_diff(app, &target, &request, move |app, result| {
                if let Some(p) = panel(app, &owned) {
                    p.core.scope_diff_answer(request_id, result);
                    repaint();
                }
            });
        }
        Effect::LoadFilePatch { path, request_id, step, request } => {
            get_git_diff(app, &target, &request, move |app, result| {
                if let Some(p) = panel(app, &owned) {
                    p.core.file_patch_answer(&path, request_id, step, result);
                    repaint();
                }
            });
        }
        Effect::Refresh { identity, known_revision } => {
            let known = (!known_revision.is_empty()).then_some(known_revision);
            get_git_workspace_summary(app, &target, known.as_deref(), move |app, result| {
                let Some(p) = panel(app, &owned) else { return };
                let effects = p.core.refresh_answer(&identity, result);
                run(app, &owned, effects);
            });
        }
        Effect::RunAction { request_id, identity, key: action_key, action } => {
            let quiet = action_is_quiet(&action);
            app.local::<ReviewStore>().mutation_ends.insert(owned.clone(), hooks.on_mutation_end.clone());
            execute_git_action(app, &target, &action, move |app, result| {
                let Some(p) = panel(app, &owned) else {
                    end_orphaned_mutation(app, &owned);
                    return;
                };
                let effects = p.core.action_answer(request_id, &identity, &action_key, quiet, result);
                run(app, &owned, effects);
            });
        }
        Effect::Reconcile { request_id, identity, key: action_key, message } => {
            get_git_workspace_summary(app, &target, None, move |app, result| {
                let Some(p) = panel(app, &owned) else {
                    end_orphaned_mutation(app, &owned);
                    return;
                };
                let effects = p.core.reconcile_answer(request_id, &identity, &action_key, message, result);
                run(app, &owned, effects);
            });
        }
        Effect::PrepareDiscard { request_id, scope_key, snapshot_key, path, include_untracked, confirmation } => {
            let paths = [path.clone()];
            prepare_git_discard(app, &target, &paths, include_untracked, move |app, result| {
                let Some(p) = panel(app, &owned) else { return };
                let (effects, confirmed) = p.core.discard_prepared(request_id, &scope_key, &snapshot_key, &path, include_untracked, confirmation, result);
                run(app, &owned, effects);
                if let Some((action, action_key)) = confirmed {
                    run_git_action(app, &owned, action, &action_key);
                }
            });
        }
        Effect::Publish(snapshot) => (hooks.on_snapshot_change)(app, snapshot),
        Effect::MutationEnd => {
            app.local::<ReviewStore>().mutation_ends.remove(key);
            (hooks.on_mutation_end)(app);
        }
        Effect::RevealHandled(nonce) => (hooks.on_reveal_handled)(app, nonce),
    }
}

/// `runGitAction`: the write, if nothing else holds the workspace.
pub fn run_git_action(app: &mut App, key: &str, action: GitAction, action_key: &str) {
    let Some(p) = panel(app, key) else { return };
    let can = p.core.can_run_action();
    let start = p.hooks.on_mutation_start.clone();
    let started = can && start(app);
    let Some(p) = panel(app, key) else { return };
    let effects = p.core.begin_action(action, action_key, started);
    run(app, key, effects);
}

/// What a row of a file's `⋮` does.
pub fn file_action(app: &mut App, key: &str, path: &str, id: &str) {
    match id {
        "stage" => run_git_action(app, key, GitAction::Stage { paths: vec![path.to_owned()] }, &format!("stage:{path}")),
        "unstage" => run_git_action(app, key, GitAction::Unstage { paths: vec![path.to_owned()] }, &format!("unstage:{path}")),
        "discard" => {
            let Some(p) = panel(app, key) else { return };
            let (untracked, armed) = (p.core.is_untracked(path), p.core.armed_discard(path));
            let effects = p.core.request_discard(path, untracked, armed);
            run(app, key, effects);
        }
        _ => {}
    }
}

fn scope_sections(core: &ReviewCore, key: &str) -> Vec<MenuSection> {
    let base = core.branch_base().is_some();
    let scope = core.diff_scope;
    let mut items = Vec::new();
    let mut add = |id: &str, which: DiffScope, label: &str, description: Option<&str>| {
        let k = key.to_owned();
        items.push(MenuItem { description: description.map(str::to_owned), checked: Some(scope == which), ..MenuItem::new(format!("scope-{id}"), label) }.on(move |app: &mut App| {
            if let Some(p) = panel(app, &k) {
                p.core.diff_scope = which;
                repaint();
            }
        }));
    };
    if base {
        add("branch", DiffScope::Branch, t("分支上的全部变更", "All changes on the branch"), Some(t("自工作树分出以来，含已提交的", "Since the worktree was forked, commits included")));
    }
    add("working", DiffScope::Working, if base { t("未提交的变更", "Uncommitted changes") } else { t("全部变更", "All changes") }, None);
    add("staged", DiffScope::Staged, t("已暂存的变更", "Staged changes"), None);
    add("unstaged", DiffScope::Unstaged, t("未暂存的变更", "Unstaged changes"), None);
    vec![MenuSection { id: "diff-scope".to_owned(), label: Some(t("差异范围", "Diff scope").to_owned()), items }]
}

fn set_flag(app: &mut App, key: &str, storage: &'static str, f: impl FnOnce(&mut ReviewCore) -> bool) {
    let Some(p) = panel(app, key) else { return };
    let value = f(&mut p.core);
    app.ui_store().write_flag(storage, value);
    repaint();
}

/// The pane's `⋮`: the display settings it owns, which outlive the conversation.
fn pane_menu_sections(core: &ReviewCore, key: &str, has_pages: bool) -> Vec<MenuSection> {
    let mut sections = Vec::new();
    // With pages in the title bar the scope has no trigger of its own there.
    if has_pages {
        sections.extend(scope_sections(core, key));
    }
    let files_visible = core.show_diff_files && core.diff_can_fit_files;
    let k = key.to_owned();
    sections.push(MenuSection {
        id: "diff-file-list".to_owned(),
        label: None,
        items: vec![MenuItem { checked: Some(files_visible), disabled: !core.diff_can_fit_files, description: (!core.diff_can_fit_files).then(|| t("面板太窄，放不下文件列表", "The pane is too narrow for the file list").to_owned()), ..MenuItem::new("show-files", t("显示文件", "Show files")) }.on(move |app: &mut App| {
            set_flag(app, &k, DIFF_TREE_STORAGE_KEY, |c| {
                c.show_diff_files = !c.show_diff_files;
                c.show_diff_files
            });
        })],
    });
    let (k1, k2) = (key.to_owned(), key.to_owned());
    sections.push(MenuSection {
        id: "diff-fold".to_owned(),
        label: None,
        items: vec![
            MenuItem::new("collapse-all", t("折叠所有文件", "Collapse all files")).on(move |app: &mut App| {
                if let Some(p) = panel(app, &k1) {
                    let seq = p.core.diff_fold_all.map_or(0, |f| f.1) + 1;
                    p.core.diff_fold_all = Some((true, seq));
                    repaint();
                }
            }),
            MenuItem { disabled: core.diff_is_large, description: core.diff_is_large.then(|| t("这次变更太大，无法一次展开。选择一个文件来展开它。", "This diff is too large to expand at once. Select a file to expand it.").to_owned()), ..MenuItem::new("expand-all", t("展开所有文件", "Expand all files")) }.on(move |app: &mut App| {
                if let Some(p) = panel(app, &k2) {
                    let seq = p.core.diff_fold_all.map_or(0, |f| f.1) + 1;
                    p.core.diff_fold_all = Some((false, seq));
                    repaint();
                }
            }),
        ],
    });
    let (k1, k2, k3, k4) = (key.to_owned(), key.to_owned(), key.to_owned(), key.to_owned());
    sections.push(MenuSection {
        id: "diff-display".to_owned(),
        label: None,
        items: vec![
            MenuItem { checked: Some(core.diff_style == DiffStyle::Split), hint: (core.diff_style == DiffStyle::Split && !core.diff_can_fit_split).then(|| t("面板需要更宽", "Needs a wider pane").to_owned()), ..MenuItem::new("side-by-side", t("并排显示", "Side by side")) }.on(move |app: &mut App| {
                let Some(p) = panel(app, &k1) else { return };
                let next = if p.core.diff_style == DiffStyle::Split { DiffStyle::Unified } else { DiffStyle::Split };
                p.core.diff_style = next;
                app.ui_store().set(DIFF_STYLE_STORAGE_KEY, if next == DiffStyle::Split { "split" } else { "unified" });
                repaint();
            }),
            MenuItem { checked: Some(core.diff_word_wrap), ..MenuItem::new("word-wrap", t("自动换行", "Word wrap")) }.on(move |app: &mut App| {
                set_flag(app, &k2, DIFF_WORD_WRAP_STORAGE_KEY, |c| {
                    c.diff_word_wrap = !c.diff_word_wrap;
                    c.diff_word_wrap
                });
            }),
            MenuItem { checked: Some(core.diff_word_diff), ..MenuItem::new("word-diff", t("高亮改动的词", "Highlight changed words")) }.on(move |app: &mut App| {
                set_flag(app, &k3, DIFF_WORD_DIFF_STORAGE_KEY, |c| {
                    c.diff_word_diff = !c.diff_word_diff;
                    c.diff_word_diff
                });
            }),
            MenuItem { checked: Some(core.diff_hide_whitespace), ..MenuItem::new("hide-whitespace", t("隐藏空白改动", "Hide whitespace changes")) }.on(move |app: &mut App| {
                set_flag(app, &k4, DIFF_HIDE_WHITESPACE_STORAGE_KEY, |c| {
                    c.diff_hide_whitespace = !c.diff_hide_whitespace;
                    c.diff_hide_whitespace
                });
            }),
        ],
    });
    let k = key.to_owned();
    sections.push(MenuSection {
        id: "refresh".to_owned(),
        label: None,
        items: vec![MenuItem { disabled: core.busy_action.is_some(), ..MenuItem::new("refresh", t("刷新", "Refresh")) }.on(move |app: &mut App| {
            let Some(p) = panel(app, &k) else { return };
            let effects = p.core.refresh_snapshot();
            run(app, &k, effects);
        })],
    });
    sections
}

/// A review page's name as a page tab draws it: the workspace, the branch the checkout is read against, and for an
/// isolated worktree the worktree, joined by arrows.
pub fn git_review_page_label(workspace: &str, branch: &str, worktree: Option<&str>) -> E {
    let t_ = theme();
    let mut label: E = row().items_center().gap(3.0).min_w(0.0).clip().child(path_text(workspace.to_owned(), "", CAPTION_STRONG, None).with_style(|s| s.flex_shrink = 1.0)).child(icon("ArrowRight", 10.0).color(t_.text_tertiary)).child(text(branch.to_owned()).ty(CAPTION).ellipsis());
    if let Some(worktree) = worktree {
        label = label.child(icon("ArrowRight", 10.0).color(t_.text_tertiary)).child(text(worktree.to_owned()).ty(CAPTION).ellipsis());
    }
    label
}

/// A line above the diff. A message that runs to a second line keeps its icon level with the first.
fn notice(role: Role, icon_name: &'static str, color: Color, message: String, ground: Color) -> E {
    let t_ = theme();
    row().items_start().gap(7.0).px(10.0).py(7.0).shrink0().bg(ground).border_bottom(t_.divider).role(role).child(icon(icon_name, 14.0).color(color).mt(1.0).shrink0()).child(text(message).ty(FINE).color(color).flex1().min_w(0.0))
}

/// What the panel's own width decides: `@container git-review (max-width: 520px)`.
const NARROW_PANEL: f32 = 520.0;
/// `.git-review__repository-operation`'s inline padding, and the icon's column.
const BANNER_INSET: f32 = 18.0;
const BANNER_ICON_COLUMN: f32 = 24.0;

/// One button of the operation banner. `armed` shows the confirmation look, and a press elsewhere disarms it.
#[allow(clippy::too_many_arguments)]
fn banner_button(panel_key: &str, id: &str, label: String, disabled: bool, armed: bool, busy: bool, on: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    let mut b: E = row().key(("review-banner", panel_key, id)).items_center().gap(4.0).min_h(25.0).px(7.0).radius(5.0).shrink0();
    if armed {
        b = b.bg(t_.critical_bg);
    }
    if busy {
        b = b.child(spinner(12.0, Some(t_.text_secondary)));
    }
    b = b.child(text(label).ty(FINE_STRONG).color(if armed { t_.critical } else { t_.text_primary }).nowrap());
    if disabled {
        b.disabled(true)
    } else {
        b.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(on)
    }
}

fn repository_banner(core: &ReviewCore, key: &str) -> Option<E> {
    let t_ = theme();
    let s = &core.current_snapshot;
    let operation = s.operation?;
    let scope = core.operation_scope().unwrap_or_default();
    let (skip_key, abort_key, old_key, new_key, bisect_skip_key) = (format!("skip-operation:{scope}"), format!("abort-operation:{scope}"), format!("bisect-old:{scope}"), format!("bisect-new:{scope}"), format!("bisect-skip:{scope}"));
    let head = s.head.clone().unwrap_or_default();
    let revision = s.operation_revision.clone().unwrap_or_default();
    let disabled = core.mutations_disabled();
    let no_proof = s.head.is_none() || s.operation_revision.is_none();
    let pending = core.pending_git_action.clone();
    let busy = core.busy_action.clone();
    let width = rect_of(("git-review", key)).map(|r| r.w);
    let narrow = width.is_some_and(|w| w <= NARROW_PANEL);
    let mut buttons: E = row().items_center().gap(2.0).shrink0().when(narrow, |e| e.w_full().justify_end());
    let confirm = |action_key: String, action: GitAction| {
        let (panel_key, action_key) = (key.to_owned(), action_key);
        move |app: &mut App| {
            let Some(p) = panel(app, &panel_key) else { return };
            if p.core.confirm_local_git_action(&action_key) {
                run_git_action(app, &panel_key, action.clone(), &action_key);
            }
            repaint();
        }
    };
    if operation == GitRepositoryOperation::Bisect {
        let bisect_disabled = disabled || !s.is_clean || no_proof;
        for (id, outcome, action_key, idle, armed, working) in [("old", GitBisectOutcome::Old, &old_key, t("标为旧状态", "Mark old"), t("确认旧状态", "Confirm old"), t("标记中", "Marking")), ("new", GitBisectOutcome::New, &new_key, t("标为新状态", "Mark new"), t("确认新状态", "Confirm new"), t("标记中", "Marking")), ("skip", GitBisectOutcome::Skip, &bisect_skip_key, t("跳过", "Skip"), t("确认跳过", "Confirm skip"), t("跳过中", "Skipping"))] {
            let is_armed = pending.as_deref() == Some(action_key.as_str());
            let is_busy = busy.as_deref() == Some(action_key.as_str());
            let label = if is_busy { working } else if is_armed { armed } else { idle };
            let action = GitAction::BisectStep { outcome, expected_head: head.clone(), expected_operation_revision: revision.clone(), expected_content_revision: s.content_revision.clone() };
            buttons = buttons.child(banner_button(key, id, label.to_owned(), bisect_disabled, is_armed, is_busy, confirm(action_key.clone(), action)));
        }
    }
    if operation_supports_continue(operation) {
        let is_busy = busy.as_deref() == Some("continue-operation");
        let action = GitAction::ContinueOperation { operation, expected_head: head.clone(), expected_operation_revision: revision.clone() };
        let panel_key = key.to_owned();
        buttons = buttons.child(banner_button(key, "continue", if is_busy { t("继续中", "Continuing") } else { t("继续", "Continue") }.to_owned(), disabled || s.conflicted > 0 || no_proof, false, is_busy, move |app: &mut App| run_git_action(app, &panel_key, action.clone(), "continue-operation")));
    }
    if operation_supports_skip(operation) {
        let is_armed = pending.as_deref() == Some(skip_key.as_str());
        let action = GitAction::SkipOperation { operation, expected_head: head.clone(), expected_operation_revision: revision.clone() };
        buttons = buttons.child(banner_button(key, "skip-operation", if is_armed { t("确认跳过", "Confirm skip") } else { t("跳过", "Skip") }.to_owned(), disabled || no_proof, is_armed, false, confirm(skip_key.clone(), action)));
    }
    let is_armed = pending.as_deref() == Some(abort_key.as_str());
    let bisect = operation == GitRepositoryOperation::Bisect;
    let abort_label = match (is_armed, bisect) {
        (true, true) => t("确认结束", "Confirm stop"),
        (true, false) => t("确认中止", "Confirm abort"),
        (false, true) => t("结束", "Stop"),
        (false, false) => t("中止", "Abort"),
    };
    let action = GitAction::AbortOperation { operation, expected_head: head, expected_operation_revision: revision };
    buttons = buttons.child(banner_button(key, "abort", abort_label.to_owned(), disabled || no_proof, is_armed, false, confirm(abort_key.clone(), action)));

    // Blur disarms: a press anywhere but the armed button takes the confirmation back.
    let armed_id = pending.as_ref().and_then(|p| {
        [(&old_key, "old"), (&new_key, "new"), (&bisect_skip_key, "skip"), (&skip_key, "skip-operation"), (&abort_key, "abort")].into_iter().find(|(k, _)| *k == p).map(|(_, id)| id)
    });
    let disarm: E = {
        let panel_key = key.to_owned();
        custom::<App>(move |app, ui, _| {
            let Some(id) = armed_id else { return };
            let on_it = rect_of(("review-banner", panel_key.as_str(), id)).is_some_and(|r| ui.input.pointer.is_some_and(|p| r.contains(p.0, p.1)));
            if ui.input.pressed && !on_it
                && let Some(p) = panel(app, &panel_key)
            {
                p.core.pending_git_action = None;
                repaint();
            }
        })
        .size(1.0, 1.0)
        .absolute()
        .pass_through()
    };
    let title = tf("Git {operation} 正在进行", "Git {operation} in progress", &[("operation", &repository_operation_label(operation))]);
    // Narrow, the words take a line of their own and the buttons the one under them, against the right edge.
    let copy: E = col().flex1().min_w(0.0).gap(1.0).child(text(title).ty(FINE_STRONG)).child(text(core.operation_note().unwrap_or_default()).ty(FINE).color(t_.text_secondary).ellipsis());
    let copy = match width.filter(|_| narrow) {
        Some(w) => copy.basis((w - BANNER_INSET - BANNER_ICON_COLUMN).max(0.0)),
        None => copy,
    };
    Some(
        row()
            .key(("review-banner-row", key))
            .when(narrow, |e| e.items_start().wrap())
            .when(!narrow, |e| e.items_center())
            .gap(7.0)
            .min_h(42.0)
            .pl(10.0)
            .pr(8.0)
            .py(6.0)
            .shrink0()
            .bg(t_.caution_bg)
            .border_bottom(t_.divider)
            .role(Role::Region)
            .aria_label(t("进行中的 Git 操作", "Git operation in progress"))
            .child(icon("CircleAlert", 15.0).color(t_.caution).shrink0())
            .child(copy)
            .child(buttons)
            .child(disarm),
    )
}

/// The panel. `header` and `menu_sections` belong in `SidePane`'s title bar, `body` in its body.
pub fn git_review_panel(app: &mut App, props: GitReviewProps) -> GitReviewView {
    let t_ = theme();
    let GitReviewProps { key, inputs, hooks, page_tabs } = props;
    let task = app.task();
    let guard_key = key.clone();
    let _guard = use_state(("git-review-mount", key.as_str()), move || MountGuard { task, key: guard_key });
    let mounted = app.local::<ReviewStore>().panels.contains_key(&key);
    if !mounted {
        let store = app.ui_store();
        let (show_files, split, wrap, words, hide) = (store.read_flag(DIFF_TREE_STORAGE_KEY, true), store.get(DIFF_STYLE_STORAGE_KEY) == Some("split"), store.read_flag(DIFF_WORD_WRAP_STORAGE_KEY, true), store.read_flag(DIFF_WORD_DIFF_STORAGE_KEY, true), store.read_flag(DIFF_HIDE_WHITESPACE_STORAGE_KEY, false));
        let core = ReviewCore::new(&inputs, show_files, if split { DiffStyle::Split } else { DiffStyle::Unified }, wrap, words, hide);
        app.local::<ReviewStore>().panels.insert(key.clone(), Panel { core, hooks: hooks.clone() });
    }
    let Some(p) = panel(app, &key) else { return GitReviewView { header: div(), menu_sections: Vec::new(), body: div() } };
    p.hooks = hooks.clone();
    let effects = p.core.sync(&inputs);
    run(app, &key, effects);
    let Some(p) = panel(app, &key) else { return GitReviewView { header: div(), menu_sections: Vec::new(), body: div() } };
    let core = &p.core;
    let has_pages = page_tabs.is_some();

    // ---- the title bar: the file-column toggle, then one dropdown that answers "what am I looking at"
    let files_visible = core.show_diff_files && core.diff_can_fit_files;
    let toggle_key = key.clone();
    let mut toggle: E = row().key(("review-files-toggle", key.as_str())).center().size(24.0, 24.0).radius(5.0).shrink0().tooltip(if files_visible { t("隐藏文件", "Hide files") } else { t("显示文件", "Show files") }).aria_pressed(files_visible).child(icon("PanelLeft", 14.0).color(if files_visible { t_.text_primary } else { t_.text_secondary }));
    if files_visible {
        toggle = toggle.bg(t_.subtle_pressed);
    }
    if core.diff_can_fit_files {
        toggle = toggle.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| {
            set_flag(app, &toggle_key, DIFF_TREE_STORAGE_KEY, |c| {
                c.show_diff_files = !c.show_diff_files;
                c.show_diff_files
            });
        });
    } else {
        toggle = toggle.disabled(true);
    }
    let mut header: E = row().items_center().gap(4.0).flex1().min_w(0.0).child(toggle);
    header = match page_tabs {
        Some(tabs) => header.child(tabs),
        None => {
            let mut trigger: E = row().items_center().gap(4.0).h(24.0).px(6.0).radius(5.0).min_w(0.0).hover_bg(t_.subtle_hover).child(text(core.branch_label()).ty(CAPTION).color(t_.text_secondary).ellipsis());
            if let Some(worktree) = &core.checkout.worktree_name {
                trigger = trigger.child(icon("ArrowRight", 11.0).color(t_.text_tertiary)).child(text(worktree.clone()).ty(CAPTION).color(t_.text_secondary).ellipsis());
            }
            if let Some(note) = core.scope_note() {
                trigger = trigger.child(row().px(5.0).radius(4.0).shrink0().bg(t_.subtle_pressed).child(text(note).ty(FINE).nowrap()));
            }
            let sections = scope_sections(core, &key);
            header.child(popover_menu(("review-scope", key.as_str()), t("审阅范围", "Review scope"), trigger, move || sections, Align::Start))
        }
    };
    let menu_sections = pane_menu_sections(core, &key, has_pages);

    // ---- what is there to say above the diff
    let mut body: E = col().key(("git-review", key.as_str())).flex1().min_w(0.0).min_h(0.0).role(Role::Region).aria_label(t("Git 审阅", "Git review"));
    if let Some(banner) = repository_banner(core, &key) {
        body = body.child(banner);
    }
    if let Some(reason) = &core.mutation_disabled_reason {
        body = body.child(notice(Role::Status, "CircleAlert", t_.text_secondary, reason.clone(), t_.card_secondary));
    }
    if let Some(error) = &core.operation_error {
        body = body.child(notice(Role::Alert, "CircleAlert", t_.critical, error.clone(), t_.critical_bg));
    }
    if let Some(message) = &core.operation_message {
        body = body.child(row().items_center().gap(7.0).px(10.0).py(7.0).shrink0().bg(t_.success_bg).border_bottom(t_.divider).role(Role::Status).child(icon("Check", 14.0).color(t_.success).shrink0()).child(text(message.clone()).ty(FINE).color(t_.success).flex1().min_w(0.0)));
    }
    if !core.current_snapshot.warnings.is_empty() {
        body = body.child(notice(Role::Status, "CircleAlert", t_.caution, core.current_snapshot.warnings.join(" · "), t_.caution_bg));
    }
    // `renderChanges`: from here on is one section.
    let mut changes: E = col().flex1().min_w(0.0).min_h(0.0).role(Role::Region).aria_label(t("变更", "Changes"));
    if let Some(error) = &core.diff_error {
        changes = changes.child(notice(Role::Alert, "CircleAlert", t_.critical, error.clone(), t_.critical_bg));
    }
    if core.diff.as_ref().is_some_and(|d| d.truncated) {
        changes = changes.child(notice(Role::Alert, "CircleAlert", t_.caution, t("差异内容超过安全读取上限，当前审阅不完整；刷新或缩小范围后再查看。", "The diff exceeded the safe read limit, so this review is incomplete. Refresh or narrow the scope.").to_owned(), t_.caution_bg));
    }

    // ---- the changes
    if core.total_changed_files() > 0 {
        // The column's chrome: the filter above the list and the load-more below it, outside what scrolls.
        let (filter_key, clear_key) = (key.clone(), key.clone());
        let filter = field(("review-filter", key.as_str()), core.change_filter.clone())
            .placeholder(t("筛选文件", "Filter files"))
            .ty(CAPTION)
            .on_change(move |app: &mut App, value| {
                if let Some(p) = panel(app, &filter_key) {
                    p.core.change_filter = value;
                    repaint();
                }
            })
            // `type="search"`: Escape empties the field.
            .on_key(move |app: &mut App, press| {
                let Some(p) = panel(app, &clear_key).filter(|p| press.key == Key::Escape && !p.core.change_filter.is_empty()) else { return false };
                p.core.change_filter.clear();
                repaint();
                true
            })
            .build()
            .h(24.0)
            .aria_label(t("筛选变更文件", "Filter changed files"));
        let list_header: E = col().gap(2.0).px(8.0).pt(4.0).shrink0().child(filter).child(text(tf("显示 {shown}/{total} 个变更文件", "Showing {shown}/{total} changed files", &[("shown", &core.change_files.len()), ("total", &core.change_matched_count)])).ty(FINE).color(t_.text_tertiary).role(Role::Status));
        let mut list_footer: E = col().shrink0();
        if core.change_page_state == RequestState::Loading && core.change_files.is_empty() {
            list_footer = list_footer.child(row().center().gap(6.0).min_h(44.0).px(9.0).py(12.0).role(Role::Status).child(spinner(14.0, None)).child(text(t("正在读取变更文件", "Loading changed files")).ty(FINE).color(t_.text_tertiary)));
        }
        if let Some(error) = &core.change_page_error {
            list_footer = list_footer.child(row().center().gap(6.0).min_h(44.0).px(9.0).py(12.0).role(Role::Alert).child(icon("CircleAlert", 13.0).color(t_.critical).shrink0()).child(text(error.clone()).ty(FINE).color(t_.critical).text_center().min_w(0.0)));
        }
        if core.change_page_state != RequestState::Loading && core.change_page_error.is_none() && core.change_matched_count == 0 {
            list_footer = list_footer.child(text(t("没有匹配的变更文件", "No changed files match")).ty(FINE).color(t_.text_tertiary).text_center().px(9.0).py(16.0));
        }
        if let Some(cursor) = core.change_next_cursor.clone() {
            let loading = core.change_loading_more;
            let more_key = key.clone();
            let mut more: E = row().key(("review-more", key.as_str())).center().gap(6.0).w_full().min_h(32.0).shrink0().border_top(t_.divider);
            if loading {
                more = more.disabled(true).child(spinner(12.0, None)).child(text(t("正在加载", "Loading")).ty(FINE).color(t_.text_secondary));
            } else {
                more = more.hover_bg(t_.subtle_hover).child(text(tf("再显示 {count} 个文件", "Show {count} more files", &[("count", &core.next_change_page_count())])).ty(FINE).color(t_.text_secondary)).on_click(move |app: &mut App| {
                    let Some(p) = panel(app, &more_key) else { return };
                    let mut effects = Vec::new();
                    p.core.load_change_page(Some(cursor.clone()), &mut effects);
                    run(app, &more_key, effects);
                });
            }
            list_footer = list_footer.child(more);
        }

        let menu_key = key.clone();
        let file_menu: Rc<dyn Fn(&str) -> Vec<MenuSection>> = Rc::new({
            // A menu is built while the frame is, so it reads what the core said at the frame's start.
            let core_snapshot = FileMenuData::of(core);
            move |path: &str| core_snapshot.sections(&menu_key, path)
        });
        let (fit_tree, fit_split, large, select, need, expand) = {
            let (a, b, c, d, e, f) = (key.clone(), key.clone(), key.clone(), key.clone(), key.clone(), key.clone());
            (
                Rc::new(move |app: &mut App, v: bool| {
                    if let Some(p) = panel(app, &a) {
                        p.core.diff_can_fit_files = v;
                        repaint();
                    }
                }) as Rc<dyn Fn(&mut App, bool)>,
                Rc::new(move |app: &mut App, v: bool| {
                    if let Some(p) = panel(app, &b) {
                        p.core.diff_can_fit_split = v;
                        repaint();
                    }
                }) as Rc<dyn Fn(&mut App, bool)>,
                Rc::new(move |app: &mut App, v: bool| {
                    if let Some(p) = panel(app, &c) {
                        p.core.diff_is_large = v;
                        repaint();
                    }
                }) as Rc<dyn Fn(&mut App, bool)>,
                Rc::new(move |app: &mut App, path: &str| {
                    if let Some(p) = panel(app, &d) {
                        p.core.selected_path = Some(path.to_owned());
                        repaint();
                    }
                }) as Rc<dyn Fn(&mut App, &str)>,
                Rc::new(move |app: &mut App, path: &str| {
                    let Some(p) = panel(app, &e) else { return };
                    if let Some(effect) = p.core.request_file_patch(path, None) {
                        run(app, &e, vec![effect]);
                    }
                }) as Rc<dyn Fn(&mut App, &str)>,
                Rc::new(move |app: &mut App, path: &str| {
                    let Some(p) = panel(app, &f) else { return };
                    if let Some(effect) = p.core.expand_file_context(path) {
                        run(app, &f, vec![effect]);
                    }
                }) as Rc<dyn Fn(&mut App, &str)>,
            )
        };
        let line_menu_state = use_state(("review-line-menu", key.as_str()), || None::<(String, u32, f32, f32)>);
        let on_line_menu = hooks.on_mention_line.as_ref().map(|_| {
            let state = line_menu_state.clone();
            Rc::new(move |_: &mut App, path: &str, line: u32, point: (f32, f32)| state.set(Some((path.to_owned(), line, point.0, point.1)))) as Rc<dyn Fn(&mut App, &str, u32, (f32, f32))>
        });
        let viewer = DiffViewProps {
            key: format!("review-{key}"),
            entries: core.diff_entries(),
            patches: core.diff_patches(),
            show_tree: core.show_diff_files,
            on_can_fit_tree_change: Some(fit_tree),
            diff_style: core.diff_style,
            on_can_fit_split_change: Some(fit_split),
            word_wrap: core.diff_word_wrap,
            word_diff: core.diff_word_diff,
            hide_whitespace: core.diff_hide_whitespace,
            on_is_large_change: Some(large),
            fold_all_request: core.diff_fold_all,
            active_path: core.selected_path.clone(),
            on_select_file: Some(select),
            reveal_request: core.diff_reveal.clone(),
            file_menu: Some(file_menu),
            list_header: Some(list_header),
            list_footer: Some(list_footer),
            on_need_patch: Some(need),
            on_expand_context: Some(expand),
            expanding_paths: core.expanding_paths(),
            fully_expanded_paths: core.fully_expanded_paths(),
            on_line_menu,
            empty_label: if core.diff_state == RequestState::Loading { t("正在读取差异", "Loading the diff").to_owned() } else { t("这个范围没有行级差异", "No line-level diff in this scope").to_owned() },
        };
        changes = changes.child(diff_view(app, viewer));
        // The diff line a right click landed on, while its menu is open.
        if let (Some((path, line, x, y)), Some(mention)) = (line_menu_state.get(), hooks.on_mention_line.clone()) {
            let close = line_menu_state.clone();
            let item = ContextMenuItem::new("mention", t("在会话中提及", "Mention in Conversation")).icon("AtSign").on_select(move |app: &mut App| mention(app, &path, line));
            let props = ContextMenuProps::new(ContextMenuAnchor::Point { x, y }, t("这一行的操作", "Actions for this line"), move |_: &mut App| close.set(None), vec![ContextMenuSection::new("mention", vec![item])]);
            changes = changes.child(context_menu(("review-line-menu", key.as_str()), props));
        }
    } else {
        changes = changes.child(row().flex1().center().gap(7.0).min_h(96.0).p(18.0).child(icon("Check", 20.0).color(t_.text_secondary).shrink0()).child(text(core.empty_label()).ty(FINE).color(t_.text_tertiary).text_center().min_w(0.0)));
    }
    let body = body.child(changes);
    GitReviewView { header, menu_sections, body }
}

/// What a file's `⋮` needs of the core, taken at the frame's start, since the menu is built during the frame's layout
/// where the app is out of reach.
struct FileMenuData {
    actions: HashMap<String, Vec<FileAction>>,
}

impl FileMenuData {
    fn of(core: &ReviewCore) -> Self {
        Self { actions: core.change_files.iter().map(|f| (f.path.clone(), core.file_actions(&f.path))).collect() }
    }

    fn sections(&self, key: &str, path: &str) -> Vec<MenuSection> {
        let Some(actions) = self.actions.get(path) else { return Vec::new() };
        let mut sections = Vec::new();
        if !actions.is_empty() {
            let items = actions
                .iter()
                .map(|action| {
                    let (panel_key, file, id) = (key.to_owned(), path.to_owned(), action.id);
                    MenuItem { description: action.description.clone(), icon: Some(action.icon), disabled: action.disabled, ..MenuItem::new(id, action.label.clone()) }.on(move |app: &mut App| file_action(app, &panel_key, &file, id))
                })
                .collect();
            sections.push(MenuSection { id: "file-actions".to_owned(), label: None, items });
        }
        let copied = path.to_owned();
        sections.push(MenuSection { id: "file-path".to_owned(), label: None, items: vec![MenuItem::new("copy-path", t("复制路径", "Copy path")).on(move |_| crate::clipboard::copy_text(copied.clone()))] });
        sections
    }
}
