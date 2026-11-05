//! `GitStatusCard`: the composer-context row's chip and the panel it opens. It reports the workspace repository and
//! nothing else; the caller renders nothing at all when the workspace is not a Git working directory.

use mewrk_client::prelude::*;

use crate::git::{GitRepositoryOperation, GitWorkspaceSnapshot};

/// How many paths have changed: the summary's count, or what the snapshot holds.
pub fn change_count(git: &GitWorkspaceSnapshot) -> u64 {
    if git.changed_files > 0 {
        return u64::from(git.changed_files);
    }
    if !git.files.is_empty() {
        return git.files.len() as u64;
    }
    u64::from(git.staged + git.unstaged + git.untracked + git.conflicted)
}

/// The repository's next step, said as a state rather than a button's verb: Mewrk itself commits, pushes and pulls
/// nothing (the model or a terminal does), so the row only opens the Review pane where the step is seen.
pub fn next_step(git: &GitWorkspaceSnapshot) -> String {
    if git.conflicted > 0 {
        return tf("{count} 个冲突待解决", "{count} conflicts to resolve", &[("count", &git.conflicted)]);
    }
    let operation = match git.operation {
        Some(GitRepositoryOperation::Merge) => Some(t("合并待继续", "A merge to continue")),
        Some(GitRepositoryOperation::Rebase) => Some(t("变基待继续", "A rebase to continue")),
        Some(GitRepositoryOperation::CherryPick) => Some(t("拣选提交待继续", "A cherry-pick to continue")),
        Some(GitRepositoryOperation::Revert) => Some(t("还原提交待继续", "A revert to continue")),
        Some(GitRepositoryOperation::Bisect) => Some(t("二分查找进行中", "A bisect in progress")),
        None => None,
    };
    if let Some(step) = operation {
        return step.to_owned();
    }
    if change_count(git) > 0 {
        return t("有更改待提交", "Changes to commit").to_owned();
    }
    if git.ahead > 0 {
        return tf("{count} 个提交待推送", "{count} commits to push", &[("count", &git.ahead)]);
    }
    if git.behind > 0 {
        return tf("{count} 个提交待拉取", "{count} commits to pull", &[("count", &git.behind)]);
    }
    t("没有待提交或推送的内容", "Nothing to commit or push").to_owned()
}

/// The branch row: the branch, a detached head, or a repository with no commits yet.
pub fn branch_label(git: &GitWorkspaceSnapshot) -> String {
    match (&git.branch, &git.head) {
        (Some(branch), _) => branch.clone(),
        (None, Some(head)) => tf("分离头指针 {head}", "Detached at {head}", &[("head", head)]),
        (None, None) => t("尚无提交", "No commits yet").to_owned(),
    }
}

/// `↑2 ↓1`, or nothing when the branch is level with its upstream.
pub fn ahead_behind(git: &GitWorkspaceSnapshot) -> Option<String> {
    if git.ahead == 0 && git.behind == 0 {
        return None;
    }
    let mut out = String::new();
    if git.ahead > 0 {
        out.push_str(&format!("↑{}", git.ahead));
    }
    if git.ahead > 0 && git.behind > 0 {
        out.push(' ');
    }
    if git.behind > 0 {
        out.push_str(&format!("↓{}", git.behind));
    }
    Some(out)
}

/// `toLocaleString`: `12,345`. Both languages the app speaks group by thousands with a comma.
pub fn grouped(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// The chip. `on_open_review` opens the Review pane.
pub fn git_status_card(key: impl std::hash::Hash, git: &GitWorkspaceSnapshot, git_open: bool, on_open_review: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let id = hash_key(("git-status-card", key));
    let expanded = use_state(("git-status-card-open", id), || false);
    let is_open = expanded.get();
    let toggle = expanded.clone();
    let name = if is_open { t("收起 Git 状态卡片", "Collapse Git status card") } else { t("展开 Git 状态卡片", "Expand Git status card") };
    let mut chip: E = row()
        .key(id)
        .items_center()
        .gap(5.0)
        .h(25.0)
        .px(7.0)
        .radius(CONTROL_RADIUS)
        .shrink0()
        .hover_bg(t_.subtle_hover)
        .when(is_open, |e| e.bg(t_.subtle_hover))
        .tooltip(name)
        .aria_label(name)
        .aria_expanded(is_open)
        .aria_controls(("git-status-card-panel", id))
        .child(icon("GitCompareArrows", 13.0).color(t_.text_secondary))
        .child(text(t("Git 状态", "Git status")).ty(FINE_STRONG).nowrap())
        .on_click(move |_| toggle.update(|v| *v = !*v));
    // `<aside>`: what the chip and its panel are, for a reader coming on them in the composer's row.
    let aside = |chip: E| if a11y_active() { row().shrink0().role(Role::Complementary).aria_label(t("Git 状态", "Git status")).child(chip) } else { chip };
    if !is_open {
        return aside(chip);
    }
    let rows_bg = |e: E, open: bool| e.when(open, |e| e.bg(t_.subtle_hover));
    let stat: E = row().items_center().gap(5.0).child(text(format!("+{}", grouped(u64::from(git.additions)))).ty(FINE_STRONG).color(t_.success).nowrap()).child(text(format!("−{}", grouped(u64::from(git.deletions)))).ty(FINE_STRONG).color(t_.critical).nowrap());
    let open_row = |key: &'static str, body: E, on_open: Rc<dyn Fn(&mut App)>| -> E {
        rows_bg(row().key((key, id)).items_center().gap(7.0).min_h(24.0).px(7.0).py(2.0).radius(5.0).hover_bg(t_.subtle_hover), git_open).when(git_open, |e| e.aria_current(AriaCurrent::Page)).child(body).on_click(move |app: &mut App| on_open(app))
    };
    let static_row = |label: E, small: Option<String>| -> E {
        row().items_center().gap(7.0).min_h(24.0).px(7.0).py(2.0).child(label.flex1()).when(small.is_some(), |e| e.child(text(small.clone().unwrap_or_default()).ty(FINE).color(t_.text_tertiary).nowrap()))
    };
    let close = expanded.clone();
    let panel = flyout_panel::<App>()
        .key(("git-status-card-panel", id))
        .w(288.0)
        .max_h(440.0)
        .p(3.0)
        .scroll_y()
        .child(text(t("Git 状态", "Git status")).ty(FINE_STRONG).color(t_.text_tertiary).px(7.0).pt(4.0).pb(2.0))
        .child(open_row("git-status-card-changes", row().flex1().items_center().justify_between().child(text(t("变更", "Changes")).ty(CAPTION_STRONG).nowrap()).child(stat), on_open_review.clone()).when(a11y_active(), |e| {
            let counts = tf("新增 {additions} 行，删除 {deletions} 行", "{additions} lines added, {deletions} lines deleted", &[("additions", &git.additions), ("deletions", &git.deletions)]);
            e.aria_label(format!("{} {counts}", t("变更", "Changes")))
        }))
        .child(static_row(text(t("本地", "Local")).ty(CAPTION_STRONG).ellipsis(), Some(git.remote.as_ref().map_or_else(|| t("无远程仓库", "No remote").to_owned(), |r| r.name.clone()))))
        .child(static_row(text(branch_label(git)).ty(CAPTION_STRONG).ellipsis().full_text(git.branch.clone().or_else(|| git.head.clone()).unwrap_or_default()), ahead_behind(git)))
        .child(open_row("git-status-card-next", row().flex1().items_center().justify_between().child(text(next_step(git)).ty(CAPTION_STRONG).ellipsis().flex1()).child(icon("ChevronRight", 14.0).color(t_.text_tertiary)), on_open_review))
        .popover(id, Side::Above, Align::End, 8.0, move |_| close.set(false));
    chip = chip.child(panel);
    aside(chip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::snapshot;

    fn clean() -> GitWorkspaceSnapshot {
        GitWorkspaceSnapshot { changed_files: 0, staged: 0, unstaged: 0, untracked: 0, conflicted: 0, ahead: 0, behind: 0, ..snapshot("t") }
    }

    #[test]
    fn groups_a_large_count_by_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[test]
    fn says_changes_to_commit_and_counts_ahead_and_behind() {
        let git = GitWorkspaceSnapshot { changed_files: 3, ahead: 2, behind: 1, ..clean() };
        assert_eq!(next_step(&git), "有更改待提交");
        assert_eq!(ahead_behind(&git).as_deref(), Some("↑2 ↓1"));
        assert_eq!(ahead_behind(&clean()), None);
        assert_eq!(ahead_behind(&GitWorkspaceSnapshot { ahead: 1, ..clean() }).as_deref(), Some("↑1"));
    }

    #[test]
    fn names_an_operation_to_continue_ahead_of_changes_to_commit() {
        let git = GitWorkspaceSnapshot { changed_files: 3, operation: Some(GitRepositoryOperation::Rebase), ..clean() };
        assert_eq!(next_step(&git), "变基待继续");
        assert_eq!(next_step(&GitWorkspaceSnapshot { operation: Some(GitRepositoryOperation::Bisect), ..clean() }), "二分查找进行中");
    }

    #[test]
    fn puts_conflicts_ahead_of_every_other_next_step() {
        let git = GitWorkspaceSnapshot { conflicted: 2, changed_files: 3, operation: Some(GitRepositoryOperation::Merge), ..clean() };
        assert_eq!(next_step(&git), "2 个冲突待解决");
    }

    #[test]
    fn a_clean_repository_says_so_or_what_is_left_to_sync() {
        assert_eq!(next_step(&clean()), "没有待提交或推送的内容");
        assert_eq!(next_step(&GitWorkspaceSnapshot { ahead: 2, ..clean() }), "2 个提交待推送");
        assert_eq!(next_step(&GitWorkspaceSnapshot { behind: 3, ..clean() }), "3 个提交待拉取");
    }

    #[test]
    fn names_a_detached_head_and_a_repository_with_no_commits() {
        assert_eq!(branch_label(&GitWorkspaceSnapshot { branch: None, head: Some("abc".into()), ..clean() }), "分离头指针 abc");
        assert_eq!(branch_label(&GitWorkspaceSnapshot { branch: None, head: None, ..clean() }), "尚无提交");
        assert_eq!(branch_label(&clean()), "feature/review");
    }
}
