//! Back and forward through the conversations opened before, the way a browser walks its history
//! (`lib/conversationHistory.ts`): opening a conversation drops everything ahead of the current entry and appends it;
//! back and forward only move the cursor.
//!
//! Entries are real conversations only. The new-task draft is a place to start from, not a page to return to, so while
//! it is open the cursor still points at the conversation left for it: back returns there, and forward goes on from it.

/// A conversation and the project it was in when it was opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationHistoryEntry {
    pub workspace_id: String,
    pub conversation_id: String,
}

/// The entries and the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversationHistory {
    pub entries: Vec<ConversationHistoryEntry>,
    /// The entry last visited; −1 before any.
    pub index: isize,
}

impl Default for ConversationHistory {
    fn default() -> Self {
        Self { entries: Vec::new(), index: -1 }
    }
}

/// Older entries fall off the front; nobody walks back a hundred conversations.
const HISTORY_LIMIT: usize = 100;

/// Where a step lands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryTarget {
    pub index: isize,
    pub workspace_id: String,
    pub conversation_id: String,
}

impl ConversationHistory {
    fn current(&self) -> Option<&ConversationHistoryEntry> {
        usize::try_from(self.index).ok().and_then(|index| self.entries.get(index))
    }
}

/// Records that `entry` was opened. Landing on the current entry (back, forward) changes nothing.
pub fn visit_conversation(history: &mut ConversationHistory, entry: ConversationHistoryEntry) {
    if history.current().is_some_and(|current| current.conversation_id == entry.conversation_id) {
        return;
    }
    history.entries.truncate((history.index + 1).max(0) as usize);
    history.entries.push(entry);
    if history.entries.len() > HISTORY_LIMIT {
        let excess = history.entries.len() - HISTORY_LIMIT;
        history.entries.drain(..excess);
    }
    history.index = history.entries.len() as isize - 1;
}

/// Where one step back (`-1`) or forward (`1`) lands, or `None` when nothing is there. Entries whose conversation is
/// gone are stepped over, as is the one already open. The conversation may have moved to another project since, so
/// `locate` answers where it is now.
pub fn conversation_history_target(history: &ConversationHistory, active_conversation_id: Option<&str>, step: isize, locate: impl Fn(&str) -> Option<String>) -> Option<HistoryTarget> {
    let on_current = match (history.current(), active_conversation_id) {
        (Some(current), Some(active)) => current.conversation_id == active,
        // `undefined === null` is false upstream: no entry and no active conversation aren't "on current".
        _ => false,
    };
    let mut index = if step < 0 {
        if on_current { history.index - 1 } else { history.index }
    } else {
        history.index + 1
    };
    while index >= 0 && (index as usize) < history.entries.len() {
        let conversation_id = &history.entries[index as usize].conversation_id;
        if Some(conversation_id.as_str()) != active_conversation_id {
            if let Some(workspace_id) = locate(conversation_id) {
                return Some(HistoryTarget { index, workspace_id, conversation_id: conversation_id.clone() });
            }
        }
        index += step;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit_all(ids: &[&str]) -> ConversationHistory {
        let mut history = ConversationHistory::default();
        for id in ids {
            visit_conversation(&mut history, ConversationHistoryEntry { workspace_id: "w".into(), conversation_id: (*id).into() });
        }
        history
    }

    fn everywhere(_: &str) -> Option<String> {
        Some("w".into())
    }

    fn entry_ids(history: &ConversationHistory) -> Vec<&str> {
        history.entries.iter().map(|entry| entry.conversation_id.as_str()).collect()
    }

    #[test]
    fn appends_visits_and_ignores_landing_on_the_current_entry_again() {
        let history = visit_all(&["a", "b", "b", "c"]);
        assert_eq!(entry_ids(&history), vec!["a", "b", "c"]);
        assert_eq!(history.index, 2);
    }

    #[test]
    fn steps_back_and_forward_and_has_nowhere_to_go_past_either_end() {
        let history = visit_all(&["a", "b", "c"]);
        let back = conversation_history_target(&history, Some("c"), -1, everywhere).unwrap();
        assert_eq!((back.index, back.conversation_id.as_str()), (1, "b"));
        assert_eq!(conversation_history_target(&history, Some("c"), 1, everywhere), None);

        let start = ConversationHistory { index: 0, ..history };
        assert_eq!(conversation_history_target(&start, Some("a"), -1, everywhere), None);
        let forward = conversation_history_target(&start, Some("a"), 1, everywhere).unwrap();
        assert_eq!((forward.index, forward.conversation_id.as_str()), (1, "b"));
    }

    #[test]
    fn drops_the_entries_ahead_when_a_conversation_is_opened_after_going_back() {
        let mut history = ConversationHistory { index: 0, ..visit_all(&["a", "b", "c"]) };
        visit_conversation(&mut history, ConversationHistoryEntry { workspace_id: "w".into(), conversation_id: "d".into() });
        assert_eq!(entry_ids(&history), vec!["a", "d"]);
        assert_eq!(conversation_history_target(&history, Some("d"), 1, everywhere), None);
    }

    #[test]
    fn returns_from_the_draft_to_the_conversation_it_was_opened_from() {
        let history = visit_all(&["a", "b"]);
        // The draft is not an entry, so the cursor still points at b.
        let target = conversation_history_target(&history, Some("draft"), -1, everywhere).unwrap();
        assert_eq!((target.index, target.conversation_id.as_str()), (1, "b"));
    }

    #[test]
    fn steps_over_conversations_that_were_deleted_and_follows_ones_that_moved() {
        let history = visit_all(&["a", "gone", "moved", "c"]);
        let locate = |id: &str| match id {
            "gone" => None,
            "moved" => Some("elsewhere".to_owned()),
            _ => Some("w".to_owned()),
        };
        assert_eq!(conversation_history_target(&history, Some("c"), -1, locate), Some(HistoryTarget { index: 2, workspace_id: "elsewhere".into(), conversation_id: "moved".into() }));
        let earlier = ConversationHistory { index: 2, ..history };
        let target = conversation_history_target(&earlier, Some("moved"), -1, locate).unwrap();
        assert_eq!((target.index, target.conversation_id.as_str()), (0, "a"));
    }

    #[test]
    fn older_entries_fall_off_the_front() {
        let mut history = ConversationHistory::default();
        for n in 0..105 {
            visit_conversation(&mut history, ConversationHistoryEntry { workspace_id: "w".into(), conversation_id: format!("c{n}") });
        }
        assert_eq!(history.entries.len(), 100);
        assert_eq!(history.entries[0].conversation_id, "c5");
        assert_eq!(history.index, 99);
    }
}
