//! Port of `lib/conversationQueue.ts`.

use crate::model::Conversation;

/// Whether the user's Stop holds this conversation's queue: its queued messages
/// wait for the user's next send instead of going out as rounds end. Saved with
/// the conversation (`Conversation.queue_paused`), so a restart does not send what
/// the user held back.
pub fn queue_is_paused(conversation: &Conversation) -> bool {
    conversation.queue_paused
}
