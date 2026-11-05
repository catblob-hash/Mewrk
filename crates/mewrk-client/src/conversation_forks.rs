//! Naming for conversations forked from the timeline's context menu (`lib/conversationForks.ts`).
//!
//! A fork is titled `<origin title>-fork-<number>` and follows the origin's title for as long as both exist. The origin
//! of a fork of a fork is the first conversation's, so every fork of one conversation shares one numbering, and that
//! numbering is keyed by the origin's id: two conversations that happen to share a title count their forks separately.

use std::collections::HashMap;

use crate::model::{AppDocument, Conversation};

/// Not localized: the suffix is part of the name, and a language switch must not rename forks.
pub fn fork_title(origin_title: &str, number: u32) -> String {
    format!("{origin_title}-fork-{number}")
}

fn conversations_by_id(document: &AppDocument) -> HashMap<&str, &Conversation> {
    document.workspaces.iter().flat_map(|workspace| workspace.conversations.iter()).map(|conversation| (conversation.id.as_str(), conversation)).collect()
}

/// Which conversation a new fork of `source` counts under, and the title it is named after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkOriginName {
    pub conversation_id: String,
    pub title: String,
}

/// `forkOrigin`.
pub fn fork_origin(document: &AppDocument, source: &Conversation) -> ForkOriginName {
    let Some(origin) = &source.fork_of else {
        return ForkOriginName { conversation_id: source.id.clone(), title: source.title.clone() };
    };
    if let Some(found) = conversations_by_id(document).get(origin.conversation_id.as_str()) {
        return ForkOriginName { conversation_id: found.id.clone(), title: found.title.clone() };
    }
    // The origin is gone; the title this fork still carries is all that is left of its name.
    let suffix = format!("-fork-{}", origin.number);
    ForkOriginName { conversation_id: origin.conversation_id.clone(), title: source.title.strip_suffix(&suffix).unwrap_or(&source.title).to_owned() }
}

/// One past the highest number any living fork of the origin holds.
pub fn next_fork_number(document: &AppDocument, origin_conversation_id: &str) -> u32 {
    let highest = document.workspaces.iter().flat_map(|workspace| workspace.conversations.iter()).filter_map(|conversation| conversation.fork_of.as_ref()).filter(|fork| fork.conversation_id == origin_conversation_id).map(|fork| fork.number).max().unwrap_or(0);
    highest + 1
}

/// A fork whose title has fallen behind its origin's, with the title it should have now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkTitleUpdate {
    pub workspace_id: String,
    pub conversation_id: String,
    pub title: String,
}

/// Forks whose title has fallen behind their origin's.
pub fn stale_fork_titles(document: &AppDocument) -> Vec<ForkTitleUpdate> {
    let by_id = conversations_by_id(document);
    let mut updates = Vec::new();
    for workspace in &document.workspaces {
        for conversation in &workspace.conversations {
            let Some(fork_of) = &conversation.fork_of else { continue };
            // A deleted origin leaves the fork the title it last had. An origin that is
            // itself a fork was not made here, and following it could chase a cycle.
            let Some(origin) = by_id.get(fork_of.conversation_id.as_str()) else { continue };
            if origin.fork_of.is_some() {
                continue;
            }
            let title = fork_title(&origin.title, fork_of.number);
            if conversation.title != title {
                updates.push(ForkTitleUpdate { workspace_id: workspace.id.clone(), conversation_id: conversation.id.clone(), title });
            }
        }
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::{conversation, document_of};
    use crate::model::ConversationForkOrigin;

    fn titled(id: &str, title: &str, fork_of: Option<(&str, u32)>) -> Conversation {
        let mut c = conversation(id);
        c.title = title.to_owned();
        c.fork_of = fork_of.map(|(conversation_id, number)| ConversationForkOrigin { conversation_id: conversation_id.to_owned(), number });
        c
    }

    #[test]
    fn names_a_fork_after_its_origin_and_counts_per_origin() {
        let origin = titled("a", "修复登录", None);
        let first = titled("f1", "修复登录-fork-1", Some(("a", 1)));
        let document = document_of(vec![vec![origin.clone(), first]]);
        assert_eq!(fork_title("修复登录", 2), "修复登录-fork-2");
        assert_eq!(fork_origin(&document, &origin), ForkOriginName { conversation_id: "a".into(), title: "修复登录".into() });
        assert_eq!(next_fork_number(&document, "a"), 2);
    }

    #[test]
    fn forks_a_fork_under_the_first_origins_name_and_numbering() {
        let origin = titled("a", "修复登录", None);
        let first = titled("f1", "修复登录-fork-1", Some(("a", 1)));
        let second = titled("f2", "修复登录-fork-2", Some(("a", 2)));
        let document = document_of(vec![vec![origin, first.clone()], vec![second]]);
        assert_eq!(fork_origin(&document, &first), ForkOriginName { conversation_id: "a".into(), title: "修复登录".into() });
        assert_eq!(next_fork_number(&document, "a"), 3);
    }

    #[test]
    fn keeps_counting_separately_for_two_origins_that_share_a_title() {
        let document = document_of(vec![vec![titled("a", "同名", None), titled("b", "同名", None), titled("fa1", "同名-fork-1", Some(("a", 1))), titled("fa2", "同名-fork-2", Some(("a", 2)))]]);
        assert_eq!(next_fork_number(&document, "a"), 3);
        assert_eq!(next_fork_number(&document, "b"), 1);
    }

    #[test]
    fn recovers_the_origins_name_from_a_fork_whose_origin_is_gone() {
        let orphan = titled("f2", "修复登录-fork-2", Some(("gone", 2)));
        let document = document_of(vec![vec![orphan.clone()]]);
        assert_eq!(fork_origin(&document, &orphan), ForkOriginName { conversation_id: "gone".into(), title: "修复登录".into() });
        assert_eq!(next_fork_number(&document, "gone"), 3);
    }

    #[test]
    fn lists_forks_whose_title_fell_behind_their_origins() {
        let document = document_of(vec![
            vec![titled("a", "新标题", None), titled("f1", "旧标题-fork-1", Some(("a", 1)))],
            vec![
                titled("f2", "新标题-fork-2", Some(("a", 2))),
                // Its origin is gone, so it keeps the title it last had.
                titled("f3", "别的-fork-1", Some(("gone", 1))),
                // Not a fork: renamed by the user.
                titled("f4", "我的名字", None),
            ],
        ]);
        assert_eq!(stale_fork_titles(&document), vec![ForkTitleUpdate { workspace_id: "ws0".into(), conversation_id: "f1".into(), title: "新标题-fork-1".into() }]);
    }

    #[test]
    fn does_not_follow_an_origin_that_is_itself_a_fork() {
        let document = document_of(vec![vec![titled("x", "x-fork-1", Some(("y", 1))), titled("y", "y-fork-1", Some(("x", 1)))]]);
        assert_eq!(stale_fork_titles(&document), vec![]);
    }
}
