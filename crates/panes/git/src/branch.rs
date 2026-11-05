//! The branch and worktree chips, upstream's `App.tsx` around `loadBranchPicker`, `checkoutComposerBranch` and
//! `toggleConversationWorktree`: where a task's branch and worktree are chosen before it starts. They sit in the composer
//! row and belong to the pane by what they do.

use mewrk_client::prelude::*;

use crate::git::*;

/// What the branch menu knows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BranchPickerStatus {
    #[default]
    Closed,
    Loading,
    Ready(Vec<GitBranch>),
    Error(String),
}

/// What the chips need from the rest of the app.
#[derive(Clone)]
pub struct BranchChipHooks {
    /// `beginGitMutation` for this conversation and workspace: false when something else holds the workspace.
    pub begin_mutation: Rc<dyn Fn(&mut App) -> bool>,
    /// `endGitMutation`.
    pub end_mutation: Rc<dyn Fn(&mut App)>,
    /// `refreshGitSnapshot` for this checkout. Runs after every switch, whichever way it went.
    pub refresh: Rc<dyn Fn(&mut App)>,
    /// A worktree was created (`Some`) or released (`None`): persist the record the host resolves the checkout through.
    /// Upstream's `withConversationWorktree`, then `flushLatestDocument`.
    pub on_worktree_changed: Rc<dyn Fn(&mut App, Option<model::ConversationWorktree>)>,
    /// For the draft: only the request is kept, until it materializes before its first send.
    pub on_draft_worktree_toggled: Option<Rc<dyn Fn(&mut App, bool)>>,
}

pub struct BranchChipProps {
    /// Names this group among all others.
    pub key: String,
    pub conversation_id: String,
    pub member: u32,
    pub target: Option<GitTarget>,
    /// `activeBranchLabel`, or `None` for a detached HEAD.
    pub branch_label: Option<String>,
    pub disabled: bool,
    /// The conversation runs on an isolated worktree: this menu operates on the workspace root, so switching is refused
    /// rather than silently switching the wrong checkout.
    pub has_worktree: bool,
    /// Whether the box is ticked.
    pub worktree_checked: bool,
    /// A directory project: each workspace can run on a worktree of its own.
    pub offers_worktree: bool,
    pub is_draft: bool,
    pub hooks: BranchChipHooks,
}

#[derive(Default)]
struct ChipState {
    status: BranchPickerStatus,
    query: String,
    open: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ChipStore {
    chips: std::collections::HashMap<String, ChipState>,
}

fn chip<'a>(app: &'a mut App, key: &str) -> &'a mut ChipState {
    app.local::<ChipStore>().chips.entry(key.to_owned()).or_default()
}

/// The error the chips last ran into: `branchChipError`.
pub fn branch_chip_error(app: &mut App, key: &str) -> Option<String> {
    chip(app, key).error.clone()
}

/// Remote branches go: checking out a remote ref creates a detached HEAD.
pub fn switchable_branches(result: GitBranchesResult) -> Vec<GitBranch> {
    result.branches.into_iter().filter(|b| b.kind == GitBranchKind::Local).collect()
}

/// Reloads branches whenever the menu opens; shows read failures in place without interrupting composition.
pub fn load_branch_picker(app: &mut App, key: &str, target: &GitTarget) {
    chip(app, key).status = BranchPickerStatus::Loading;
    let owned = key.to_owned();
    get_git_branches(app, target, move |app, result| {
        let state = chip(app, &owned);
        if state.status != BranchPickerStatus::Loading {
            return;
        }
        state.status = match result {
            Ok(result) => BranchPickerStatus::Ready(switchable_branches(result)),
            Err(reason) => {
                let reason = reason.trim();
                BranchPickerStatus::Error(if reason.is_empty() { t("无法读取分支列表", "Could not read the branch list").to_owned() } else { reason.to_owned() })
            }
        };
        repaint();
    });
}

/// Switches branches through the shared `GitAction` channel and workspace mutation lease. Preserves Git's own checkout
/// errors rather than discarding changes.
pub fn checkout_branch(app: &mut App, key: &str, target: &GitTarget, branch: &str, hooks: &BranchChipHooks) {
    if !(hooks.begin_mutation)(app) {
        chip(app, key).error = Some(t("工作区里还有别的操作在进行，请稍后再切换分支", "Another operation is running in this workspace; try switching branches later").to_owned());
        return;
    }
    chip(app, key).error = None;
    let (owned, hooks) = (key.to_owned(), hooks.clone());
    execute_git_action_send(app, target, &GitAction::Checkout { branch: branch.to_owned() }, Box::new(move |app, result| {
        match result {
            Ok(_) => {
                let state = chip(app, &owned);
                state.status = BranchPickerStatus::Closed;
                state.open = false;
            }
            Err(reason) => {
                let reason = reason.trim();
                chip(app, &owned).error = Some(if reason.is_empty() { t("切换分支失败", "Could not switch branches").to_owned() } else { reason.to_owned() });
            }
        }
        (hooks.end_mutation)(app);
        (hooks.refresh)(app);
        repaint();
    }));
}

type ActionDone = Box<dyn FnOnce(&mut App, Result<GitActionResult, String>)>;

/// The hooks hold `Rc`s, which cannot cross to the host's threads, so what they do waits on this thread's side of the
/// mailbox in a table the answer looks up.
fn execute_git_action_send(app: &mut App, target: &GitTarget, action: &GitAction, done: ActionDone) {
    let id = Pending::put(app, done);
    execute_git_action(app, target, action, move |app, result| {
        if let Some(done) = Pending::take(app, id) {
            done(app, result);
        }
    });
}

#[derive(Default)]
struct Pending {
    next: u64,
    actions: std::collections::HashMap<u64, ActionDone>,
    worktrees: std::collections::HashMap<u64, WorktreeDone>,
    releases: std::collections::HashMap<u64, ReleaseDone>,
}

type WorktreeDone = Box<dyn FnOnce(&mut App, Result<model::ConversationWorktree, String>)>;
type ReleaseDone = Box<dyn FnOnce(&mut App, Result<bool, String>)>;

impl Pending {
    fn put(app: &mut App, done: ActionDone) -> u64 {
        let p = app.local::<Pending>();
        p.next += 1;
        p.actions.insert(p.next, done);
        p.next
    }

    fn take(app: &mut App, id: u64) -> Option<ActionDone> {
        app.local::<Pending>().actions.remove(&id)
    }
}

/// Creates or releases the conversation's worktree of the workspace the chip has selected, on whichever machine it is.
/// On disable, release before clearing the record because the host uses it to find the tree. Drafts retain only the
/// request until they materialize before their first send.
pub fn toggle_conversation_worktree(app: &mut App, key: &str, conversation_id: &str, member: u32, enabled: bool, hooks: &BranchChipHooks) {
    if let Some(f) = &hooks.on_draft_worktree_toggled {
        chip(app, key).error = None;
        f(app, enabled);
        chip(app, key).status = BranchPickerStatus::Closed;
        return;
    }
    if !(hooks.begin_mutation)(app) {
        chip(app, key).error = Some(t("工作区里还有别的操作在进行，请稍后再切换工作树", "Another operation is running in this workspace; try toggling the worktree later").to_owned());
        return;
    }
    chip(app, key).error = None;
    let (owned, hooks) = (key.to_owned(), hooks.clone());
    if enabled {
        let p = app.local::<Pending>();
        p.next += 1;
        let id = p.next;
        let k = owned.clone();
        p.worktrees.insert(id, Box::new(move |app, result| {
            match result {
                Ok(worktree) => {
                    (hooks.on_worktree_changed)(app, Some(worktree));
                    chip(app, &k).status = BranchPickerStatus::Closed;
                }
                Err(reason) => {
                    let reason = reason.trim();
                    chip(app, &k).error = Some(if reason.is_empty() { t("无法建立隔离工作树", "Could not create the isolated worktree").to_owned() } else { reason.to_owned() });
                }
            }
            (hooks.end_mutation)(app);
            // The host resolves the checkout from the saved record, so the next read has to follow it.
            (hooks.refresh)(app);
            repaint();
        }));
        create_conversation_worktree(app, conversation_id, member, None, move |app, result| {
            if let Some(done) = app.local::<Pending>().worktrees.remove(&id) {
                done(app, result);
            }
        });
        return;
    }
    let p = app.local::<Pending>();
    p.next += 1;
    let id = p.next;
    let k = owned;
    p.releases.insert(id, Box::new(move |app, result| {
        match result {
            Ok(removed) => {
                // Clear the record even when uncommitted work keeps the tree; retained files belong to the user, not this
                // conversation.
                (hooks.on_worktree_changed)(app, None);
                chip(app, &k).status = BranchPickerStatus::Closed;
                if !removed {
                    chip(app, &k).error = Some(t("工作树里还有未提交的改动，目录与分支已保留", "The worktree still has uncommitted work, so its directory and branch were kept").to_owned());
                }
            }
            Err(reason) => {
                let reason = reason.trim();
                chip(app, &k).error = Some(if reason.is_empty() { t("无法释放隔离工作树", "Could not release the isolated worktree").to_owned() } else { reason.to_owned() });
            }
        }
        (hooks.end_mutation)(app);
        (hooks.refresh)(app);
        repaint();
    }));
    release_conversation_worktree(app, conversation_id, member, move |app, result| {
        if let Some(done) = app.local::<Pending>().releases.remove(&id) {
            done(app, result);
        }
    });
}

/// The group: the branch menu, and for a directory project the worktree box.
/// `.composer-chip` and `.composer-worktree`: 9.5 px at 720 and under.
fn chip_type(ty: TypeStyle) -> TypeStyle {
    if window_size().0 <= 720.0 { TypeStyle { size: 9.5, ..ty } } else { ty }
}

pub fn branch_chips(app: &mut App, props: BranchChipProps) -> E {
    let t_ = theme();
    let BranchChipProps { key, conversation_id, member, target, branch_label, disabled, has_worktree, worktree_checked, offers_worktree, is_draft, hooks } = props;
    let (open, status, query) = {
        let c = chip(app, &key);
        (c.open, c.status.clone(), c.query.clone())
    };
    let label = branch_label.unwrap_or_else(|| t("游离 HEAD", "Detached HEAD").to_owned());
    let id = hash_key(("branch-chip", key.as_str()));
    let can_open = !disabled && target.is_some();
    let trigger_name = tf("分支：{name}", "Branch: {name}", &[("name", &label)]);
    let tooltip = if target.is_some() { trigger_name.clone() } else { tf("分支：{name}（工作区在另一台机器上，只能在那台机器上切换分支）", "Branch: {name} (this workspace is on another machine; switch branches there)", &[("name", &label)]) };
    let mut trigger: E = row().key(id).items_center().gap(5.0).h(25.0).px(7.0).radius(CONTROL_RADIUS).shrink0().tooltip(tooltip.clone()).aria_label(trigger_name).aria_haspopup(HasPopup::Menu).aria_expanded(open && can_open).child(icon("GitBranch", 13.0).color(t_.text_secondary)).child(text(label).ty(chip_type(FINE_STRONG)).nowrap()).child(icon("ChevronDown", 11.0).color(t_.text_tertiary));
    if can_open {
        let (k, target_for_open) = (key.clone(), target.clone());
        trigger = trigger.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| {
            let c = chip(app, &k);
            c.open = !c.open;
            c.query.clear();
            if c.open
                && let Some(target) = &target_for_open
            {
                load_branch_picker(app, &k, target);
            }
            repaint();
        });
    } else {
        trigger = trigger.disabled(true).when(target.is_none(), |e| e.disabled_reason(tooltip));
    }
    if open && can_open {
        let (status_rows, empty): (Vec<GitBranch>, String) = match &status {
            BranchPickerStatus::Ready(branches) => (branches.clone(), t("没有匹配的分支", "No matching branches").to_owned()),
            BranchPickerStatus::Error(message) => (Vec::new(), message.clone()),
            _ => (Vec::new(), t("正在读取分支…", "Reading branches…").to_owned()),
        };
        let needle = query.trim().to_lowercase();
        let shown: Vec<&GitBranch> = status_rows.iter().filter(|b| needle.is_empty() || b.name.to_lowercase().contains(&needle)).collect();
        let k = key.clone();
        let search = field(("branch-search", key.as_str()), query.clone())
            .placeholder(t("搜索分支…", "Search branches…"))
            .ty(CAPTION)
            .on_change(move |app: &mut App, value| {
                chip(app, &k).query = value;
                repaint();
            })
            .build()
            .h(28.0)
            .aria_label(t("搜索分支…", "Search branches…"));
        let mut list: E = col().key(("branch-list", key.as_str())).max_h(280.0).scroll_y();
        if shown.is_empty() {
            list = list.child(text(empty).ty(CAPTION).color(t_.text_secondary).px(7.0).py(6.0));
        }
        for branch in shown {
            let mut r: E = row().key(("branch-row", key.as_str(), branch.name.as_str())).items_center().gap(7.0).min_h(24.0).px(7.0).radius(5.0).shrink0().child(div().size(14.0, 14.0).center().shrink0().when(branch.current, |e| e.child(icon("Check", 12.0).color(t_.accent_text)))).child(text(branch.name.clone()).ty(CAPTION).ellipsis().flex1()).role(Role::MenuItemRadio).aria_checked(branch.current);
            if has_worktree {
                r = r.disabled(true).disabled_reason(t("本对话正跑在隔离工作树上；先关掉工作树再切换分支", "This conversation runs in an isolated worktree; turn it off before switching branches"));
            } else if let Some(target) = target.clone() {
                let (k, name, hooks) = (key.clone(), branch.name.clone(), hooks.clone());
                r = r.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(move |app: &mut App| checkout_branch(app, &k, &target, &name, &hooks));
            }
            list = list.child(r);
        }
        let k = key.clone();
        let panel = flyout_panel::<App>().key(("branch-panel", key.as_str())).w(260.0).p(3.0).gap(3.0).role(Role::Menu).aria_label(t("切换分支", "Switch branch")).child(search).child(list).popover(id, Side::Above, Align::Start, 4.0, move |app: &mut App| chip(app, &k).open = false);
        trigger = trigger.child(panel);
    }
    let mut group: E = row().items_center().gap(0.0).radius(CONTROL_RADIUS).child(trigger);
    if offers_worktree {
        let (k, conv, hooks) = (key.clone(), conversation_id, hooks);
        let tooltip = if is_draft { t("在一份独立检出上运行本任务；工作树在你发出第一条消息时建立", "Run this task on its own checkout. The worktree is created when you send the first message.") } else { t("在一份独立检出上运行本对话，与项目里别的对话互不干扰", "Run this conversation on its own checkout, isolated from other conversations in this project") };
        let mut boxed: E = row().key(("worktree-box", key.as_str())).items_center().gap(5.0).h(25.0).px(7.0).shrink0().border_left(t_.divider).tooltip(tooltip);
        let toggle = move |app: &mut App| toggle_conversation_worktree(app, &k, &conv, member, !worktree_checked, &hooks);
        // `<label>`: the box is named by the word beside it.
        let name = t("工作树", "worktree");
        boxed = if disabled {
            boxed.child(checkbox(worktree_checked, |_: &mut App| {}).aria_label(name)).disabled(true)
        } else {
            boxed.child(checkbox(worktree_checked, toggle).aria_label(name))
        };
        boxed = boxed.child(text(t("工作树", "worktree")).ty(chip_type(FINE)).nowrap());
        group = group.child(boxed);
    }
    group
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, kind: GitBranchKind) -> GitBranch {
        GitBranch { name: name.into(), full_name: format!("refs/{name}"), kind, current: false, head: None, upstream: None, ahead: 0, behind: 0, merged: None }
    }

    #[test]
    fn only_local_branches_can_be_switched_to() {
        let result = GitBranchesResult { branches: vec![branch("main", GitBranchKind::Local), branch("origin/main", GitBranchKind::Remote)], default_branch: Some("main".into()) };
        let names: Vec<String> = switchable_branches(result).into_iter().map(|b| b.name).collect();
        assert_eq!(names, ["main"]);
    }
}
