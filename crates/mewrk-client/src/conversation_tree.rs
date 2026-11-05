//! Parenting in the sidebar's tree of conversations (`lib/conversationTree.ts`).

use std::collections::HashSet;

use crate::model::Conversation;

/// Detaches conversations whose parent isn't in this list, after a move across projects.
pub fn detach_absent_parents(conversations: &mut [Conversation]) {
    let ids: HashSet<String> = conversations.iter().map(|conversation| conversation.id.clone()).collect();
    for conversation in conversations {
        if conversation.parent_conversation_id.as_ref().is_some_and(|parent| !parent.is_empty() && !ids.contains(parent)) {
            conversation.parent_conversation_id = None;
        }
    }
}

/// Re-parents the children of `deleted_id` onto its own parent, or to the top level when it had none. The deleted
/// conversation stays in the list; callers remove it separately.
pub fn reparent_children(conversations: &mut [Conversation], deleted_id: &str) {
    let grandparent = conversations.iter().find(|conversation| conversation.id == deleted_id).and_then(|deleted| deleted.parent_conversation_id.clone());
    for conversation in conversations {
        if conversation.parent_conversation_id.as_deref() != Some(deleted_id) {
            continue;
        }
        // A conversation may never become its own parent.
        conversation.parent_conversation_id = if grandparent.as_deref() == Some(conversation.id.as_str()) { None } else { grandparent.clone() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::conversation;

    fn with_parent(id: &str, parent: Option<&str>) -> Conversation {
        let mut c = conversation(id);
        c.parent_conversation_id = parent.map(str::to_owned);
        c
    }

    fn parents(list: &[Conversation]) -> Vec<Option<&str>> {
        list.iter().map(|c| c.parent_conversation_id.as_deref()).collect()
    }

    #[test]
    fn detaches_moved_parent_or_child_edges_preserving_grandchildren_and_batch_moves() {
        let mut source = vec![with_parent("c", Some("p")), with_parent("g", Some("c"))];
        detach_absent_parents(&mut source);
        assert_eq!(parents(&source), vec![None, Some("c")]);
        let mut alone = vec![with_parent("c", Some("p"))];
        detach_absent_parents(&mut alone);
        assert_eq!(parents(&alone), vec![None]);
        let mut both = vec![with_parent("p", None), with_parent("c", Some("p"))];
        detach_absent_parents(&mut both);
        assert_eq!(parents(&both), vec![None, Some("p")]);
        let mut batch = vec![with_parent("g", Some("c")), with_parent("c", Some("p")), with_parent("p", None)];
        detach_absent_parents(&mut batch);
        assert_eq!(parents(&batch), vec![Some("c"), Some("p"), None]);
    }

    #[test]
    fn moves_children_onto_the_grandparent_and_keeps_untouched_items() {
        let mut list = vec![with_parent("root", None), with_parent("doomed", Some("root")), with_parent("child", Some("doomed")), with_parent("sibling", Some("root"))];
        reparent_children(&mut list, "doomed");
        assert_eq!(parents(&list), vec![None, Some("root"), Some("root"), Some("root")]);
    }

    #[test]
    fn lifts_children_to_the_top_level_when_the_deleted_conversation_was_a_root() {
        let mut list = vec![with_parent("root", None), with_parent("child", Some("root"))];
        reparent_children(&mut list, "root");
        assert_eq!(list[1].parent_conversation_id, None);
    }

    #[test]
    fn leaves_every_conversation_untouched_when_the_deleted_id_is_unknown() {
        let mut list = vec![with_parent("root", None), with_parent("child", Some("root"))];
        reparent_children(&mut list, "absent");
        assert_eq!(parents(&list), vec![None, Some("root")]);
    }
}
