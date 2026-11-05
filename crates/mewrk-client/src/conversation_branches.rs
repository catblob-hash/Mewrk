//! Suffixes that hang off a user message (`lib/conversationBranches.ts`).

use std::collections::{HashMap, HashSet};

use crate::model::{ContextItem, Conversation};

/// Where a fork message sits among its branches, for the `‹ 2/3 ›` control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextBranchNavigation {
    pub active_index: usize,
    pub branch_ids: Vec<String>,
}

fn is_user(context: &ContextItem) -> bool {
    matches!(context, ContextItem::User { .. })
}

/// Swaps the current suffix with one inactive slot at the same fork point, in place. `false` when it can't: the fork
/// message isn't on the timeline, it has no active slot, or the target is absent or active already.
pub fn switch_conversation_branch(conversation: &mut Conversation, fork_context_id: &str, target_branch_id: &str, now: &str) -> bool {
    let Some(index) = conversation.contexts.iter().position(|context| context.id() == fork_context_id && is_user(context)) else { return false };
    let at_fork = || conversation.branches.iter().filter(|branch| branch.fork_context_id == fork_context_id);
    let Some(active_id) = at_fork().find(|branch| branch.active).map(|branch| branch.id.clone()) else { return false };
    let Some(target_id) = at_fork().find(|branch| branch.id == target_branch_id && !branch.active).map(|branch| branch.id.clone()) else { return false };

    let current_suffix = conversation.contexts.split_off(index + 1);
    let mut arriving = Vec::new();
    for branch in &mut conversation.branches {
        if branch.id == target_id {
            arriving = std::mem::take(&mut branch.contexts);
            branch.active = true;
            branch.updated_at = now.to_owned();
        }
    }
    for branch in &mut conversation.branches {
        if branch.id == active_id {
            branch.active = false;
            branch.contexts = current_suffix;
            branch.updated_at = now.to_owned();
            break;
        }
    }
    conversation.contexts.extend(arriving);
    conversation.updated_at = now.to_owned();
    true
}

/// Navigation metadata, by fork message, only for fork messages visible on the active timeline.
pub fn context_branch_navigations(conversation: &Conversation) -> HashMap<String, ContextBranchNavigation> {
    let visible: HashSet<&str> = conversation.contexts.iter().filter(|context| is_user(context)).map(ContextItem::id).collect();
    let mut grouped: Vec<(&str, Vec<&crate::model::ConversationBranch>)> = Vec::new();
    for branch in &conversation.branches {
        if !visible.contains(branch.fork_context_id.as_str()) {
            continue;
        }
        match grouped.iter_mut().find(|(fork, _)| *fork == branch.fork_context_id) {
            Some((_, siblings)) => siblings.push(branch),
            None => grouped.push((&branch.fork_context_id, vec![branch])),
        }
    }
    let mut navigations = HashMap::new();
    for (fork, siblings) in grouped {
        let Some(active_index) = siblings.iter().position(|branch| branch.active) else { continue };
        if siblings.len() > 1 {
            navigations.insert(fork.to_owned(), ContextBranchNavigation { active_index, branch_ids: siblings.iter().map(|branch| branch.id.clone()).collect() });
        }
    }
    navigations
}

/// Whether a message anchors a branch, which forbids taking it out of the timeline.
pub fn is_conversation_branch_fork(conversation: &Conversation, context_id: &str) -> bool {
    conversation.branches.iter().any(|branch| branch.fork_context_id == context_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::{conversation, text};
    use crate::model::ConversationBranch;

    fn ids(contexts: &[ContextItem]) -> Vec<&str> {
        contexts.iter().map(ContextItem::id).collect()
    }

    /// Builds branch fixtures: creates a new active branch while retaining the previous suffix verbatim.
    fn fork_at_user(c: &mut Conversation, context_id: &str, mut create_id: impl FnMut() -> String, now: &str) {
        let index = c.contexts.iter().position(|context| context.id() == context_id && is_user(context)).unwrap();
        let suffix = c.contexts.split_off(index + 1);
        let active = c.branches.iter().position(|branch| branch.fork_context_id == context_id && branch.active);
        match active {
            Some(position) => {
                c.branches[position].active = false;
                c.branches[position].contexts = suffix;
                c.branches[position].updated_at = now.to_owned();
            }
            None => c.branches.push(ConversationBranch { id: create_id(), fork_context_id: context_id.to_owned(), active: false, contexts: suffix, created_at: c.created_at.clone(), updated_at: now.to_owned() }),
        }
        c.branches.push(ConversationBranch { id: create_id(), fork_context_id: context_id.to_owned(), active: true, contexts: Vec::new(), created_at: now.to_owned(), updated_at: now.to_owned() });
        c.updated_at = now.to_owned();
    }

    fn fixture(contexts: Vec<ContextItem>) -> Conversation {
        let mut c = conversation("conversation-branches");
        c.contexts = contexts;
        c
    }

    #[test]
    fn switches_suffixes_reversibly_while_branch_positions_stay_stable() {
        let mut c = fixture(vec![text("u1", "user"), text("old-answer", "assistant")]);
        let mut values = vec!["new", "old"];
        fork_at_user(&mut c, "u1", || values.pop().unwrap().to_owned(), "2026-07-20T01:00:00Z");
        c.contexts.push(text("new-answer", "assistant"));
        assert!(switch_conversation_branch(&mut c, "u1", "old", "2026-07-20T02:00:00Z"));
        assert_eq!(ids(&c.contexts), vec!["u1", "old-answer"]);
        assert_eq!(context_branch_navigations(&c)["u1"], ContextBranchNavigation { active_index: 0, branch_ids: vec!["old".into(), "new".into()] });
        assert!(switch_conversation_branch(&mut c, "u1", "new", "2026-07-20T03:00:00Z"));
        assert_eq!(ids(&c.contexts), vec!["u1", "new-answer"]);
        assert_eq!(context_branch_navigations(&c)["u1"].active_index, 1);
    }

    #[test]
    fn keeps_nested_fork_suffixes_reachable_through_their_parent_branch() {
        let mut to_create = vec!["grandchild", "child-old", "child", "root"];
        let mut c = fixture(vec![text("u1", "user"), text("a1", "assistant"), text("u2", "user"), text("a2", "assistant")]);
        fork_at_user(&mut c, "u1", || to_create.pop().unwrap().to_owned(), "2026-07-20T01:00:00Z");
        c.contexts.extend([text("b1", "assistant"), text("u3", "user"), text("b2", "assistant")]);
        fork_at_user(&mut c, "u3", || to_create.pop().unwrap().to_owned(), "2026-07-20T02:00:00Z");
        c.contexts.push(text("b3", "assistant"));
        assert!(switch_conversation_branch(&mut c, "u1", "root", "2026-07-20T03:00:00Z"));
        assert_eq!(ids(&c.contexts), vec!["u1", "a1", "u2", "a2"]);
        assert!(!context_branch_navigations(&c).contains_key("u3"));
        assert!(switch_conversation_branch(&mut c, "u1", "child", "2026-07-20T04:00:00Z"));
        assert!(ids(&c.contexts).contains(&"u3"));
        assert!(context_branch_navigations(&c).contains_key("u3"));
    }

    #[test]
    fn refuses_an_active_target_or_an_absent_fork_and_changes_nothing() {
        let mut c = fixture(vec![text("u1", "user"), text("a", "assistant")]);
        let mut values = vec!["new", "old"];
        fork_at_user(&mut c, "u1", || values.pop().unwrap().to_owned(), "2026-07-20T01:00:00Z");
        let before = c.clone();
        assert!(!switch_conversation_branch(&mut c, "u1", "new", "2026-07-20T02:00:00Z"));
        assert!(!switch_conversation_branch(&mut c, "absent", "old", "2026-07-20T02:00:00Z"));
        assert_eq!(c, before);
    }
}
