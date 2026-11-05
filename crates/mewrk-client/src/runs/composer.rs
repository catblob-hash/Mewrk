//! Port of `lib/composerController.ts`: what each conversation's composer holds, and the queued-message save barrier.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

use super::attachments::AttachmentRejection;
use crate::model::{FileAttachment, ImageAttachment};
use crate::Task;

/// A promise whose result several threads may wait for: what a queued message's save is to the steer barrier.
#[derive(Clone)]
pub struct Deferred<T: Clone>(Arc<(Mutex<Option<T>>, Condvar)>);

impl<T: Clone> Default for Deferred<T> {
    fn default() -> Self {
        Self(Arc::new((Mutex::new(None), Condvar::new())))
    }
}

impl<T: Clone> Deferred<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Settles it; a second settlement is ignored, as with a promise.
    pub fn resolve(&self, value: T) {
        let mut slot = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(value);
            self.0.1.notify_all();
        }
    }

    /// `await`. Never call this on the UI thread: what settles it usually has to run there.
    pub fn wait(&self) -> T {
        let mut slot = self.0.0.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(value) = slot.as_ref() {
                return value.clone();
            }
            slot = self.0.1.wait(slot).unwrap_or_else(|p| p.into_inner());
        }
    }

    pub fn is_settled(&self) -> bool {
        self.0.0.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A long paste's label in a draft and the text it stands for (`PastedText` of `lib/pastedText.ts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedText {
    /// Numbered per draft from 1, and never reused while the draft lasts.
    pub id: u32,
    /// What the draft holds in the text's place. Its number makes it unique in the draft.
    pub label: String,
    pub content: String,
}

/// The draft as it is sent: every label replaced by the text it stands for (`expandPastedTexts`).
///
/// TODO: call `lib/pastedText.ts`'s port once the composer view brings it.
pub fn expand_pasted_texts(value: &str, pastes: &[PastedText]) -> String {
    if value.is_empty() || pastes.is_empty() {
        return value.to_owned();
    }
    let mut found: Vec<(usize, usize, &PastedText)> = Vec::new();
    for paste in pastes {
        if paste.label.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(at) = value[from..].find(&paste.label) {
            let start = from + at;
            found.push((start, start + paste.label.len(), paste));
            from = start + paste.label.len();
        }
    }
    // Labels differ in their numbers, so none sits inside another; this only
    // guards a label a user built by hand out of pieces of two.
    found.sort_by(|left, right| left.0.cmp(&right.0).then(right.1.cmp(&left.1)));
    let mut expanded = String::new();
    let mut at = 0;
    for (start, end, paste) in found {
        if start < at {
            continue;
        }
        expanded.push_str(&value[at..start]);
        expanded.push_str(&paste.content);
        at = end;
    }
    expanded.push_str(&value[at..]);
    expanded
}

/// An element the user picked out of a page. Kept beside the image drafts rather than inside them: the crop travels
/// as an ordinary attachment, but the description is prompt text, and `ImageAttachment` is persisted.
///
/// TODO: this is the part of `SelectedElement` the send path reads. `lib/selectedElement.ts` formats `block`; whoever
/// ports the preview pane's pick owns that, and hands this composer the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementPick {
    /// The block prepended to the outgoing message (`sentBlock ?? selectedElementBlock(element)`).
    pub block: String,
    /// The composer attachment the crop became, once its upload landed.
    pub screenshot_image_id: Option<String>,
}

/// Prepends the blocks to what the user typed, in the order the elements were picked (`withSelectedElements`).
pub fn with_selected_elements(text: &str, elements: &[ElementPick]) -> String {
    if elements.is_empty() {
        return text.to_owned();
    }
    let blocks = elements.iter().map(|element| element.block.as_str()).collect::<Vec<_>>().join("\n\n");
    if text.is_empty() { blocks } else { format!("{blocks}\n\n{text}") }
}

/// One upload batch: runs on its own thread with a way to ask whether the batch still matters.
pub type UploadBatch = Box<dyn FnOnce(Task, UploadTicket) + Send>;

/// Identifies a queued batch, for `uploadStillCurrent` and for finishing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadTicket {
    pub conversation_id: String,
    pub generation: u64,
    pub queue: u64,
}

#[derive(Default)]
struct UploadQueue {
    id: u64,
    pending: VecDeque<UploadBatch>,
}

/// The per-conversation composer state (`ComposerControllerState`), and the barriers beside it.
#[derive(Default)]
pub struct ComposerState {
    /// Something asked for the caret in the box; the composer takes it on its next frame.
    pub focus_pending: bool,
    /// Composer text drafts per conversation.
    pub drafts: HashMap<String, String>,
    /// The text behind each long paste's label in a draft, per conversation. Kept until the draft is sent, not only
    /// while its label is in the box: an undo can bring a deleted label back.
    pub pasted_texts: HashMap<String, Vec<PastedText>>,
    /// Prepared image attachments per conversation.
    pub image_drafts: HashMap<String, Vec<ImageAttachment>>,
    /// Prepared non-image file attachments per conversation.
    pub file_drafts: HashMap<String, Vec<FileAttachment>>,
    /// What the last attempt to attach to a conversation's composer left out, until dismissed.
    pub attachment_notices: HashMap<String, Vec<AttachmentRejection>>,
    /// Elements the user picked out of a page, per conversation.
    pub element_picks: HashMap<String, Vec<ElementPick>>,
    /// Conversations with an attachment upload batch (images or files) still in flight.
    pub image_loading_ids: HashSet<String>,
    /// Queued messages currently being steered into the running turn.
    pub steering_message_ids: HashSet<String>,
    /// Queued messages whose promotion to a run failed and needs manual retry.
    pub failed_queued_promotion_ids: HashSet<String>,
    upload_queues: HashMap<String, UploadQueue>,
    upload_generations: HashMap<String, u64>,
    next_queue: u64,
    queued_saves: HashMap<String, Deferred<Result<(), String>>>,
    queued_needs_save: HashSet<String>,
}

impl ComposerState {
    /// Whether the conversation's uploads were invalidated after this batch was queued (`uploadStillCurrent`).
    pub fn upload_still_current(&self, ticket: &UploadTicket) -> bool {
        self.upload_generations.get(&ticket.conversation_id).copied().unwrap_or(0) == ticket.generation
    }

    /// Serializes upload batches per conversation and keeps the loading flag up while any batch is pending. Returns
    /// the batch to start now, if no other is running.
    pub fn enqueue_image_upload(&mut self, conversation_id: &str, batch: UploadBatch) -> Option<(UploadBatch, UploadTicket)> {
        let generation = self.upload_generations.get(conversation_id).copied().unwrap_or(0);
        self.image_loading_ids.insert(conversation_id.to_owned());
        if let Some(queue) = self.upload_queues.get_mut(conversation_id) {
            queue.pending.push_back(batch);
            return None;
        }
        self.next_queue += 1;
        let id = self.next_queue;
        self.upload_queues.insert(conversation_id.to_owned(), UploadQueue { id, pending: VecDeque::new() });
        Some((batch, UploadTicket { conversation_id: conversation_id.to_owned(), generation, queue: id }))
    }

    /// A batch finished, however it went. Returns the next one to start. A batch whose queue was invalidated meanwhile
    /// changes nothing: the queue it belonged to is gone, and a newer one may be running.
    pub fn finish_image_upload(&mut self, ticket: &UploadTicket) -> Option<(UploadBatch, UploadTicket)> {
        let queue = self.upload_queues.get_mut(&ticket.conversation_id)?;
        if queue.id != ticket.queue {
            return None;
        }
        match queue.pending.pop_front() {
            Some(batch) => {
                let generation = self.upload_generations.get(&ticket.conversation_id).copied().unwrap_or(0);
                Some((batch, UploadTicket { generation, ..ticket.clone() }))
            }
            None => {
                self.upload_queues.remove(&ticket.conversation_id);
                self.image_loading_ids.remove(&ticket.conversation_id);
                None
            }
        }
    }

    /// Invalidates in-flight uploads and drops attachment drafts and element picks for the conversations. Batches that
    /// had not started come back to run stale, so whoever waits on them still hears.
    pub fn invalidate_images(&mut self, conversation_ids: &[String]) -> Vec<(UploadBatch, UploadTicket)> {
        let mut stale = Vec::new();
        for conversation_id in conversation_ids {
            let generation = self.upload_generations.entry(conversation_id.clone()).or_default();
            let previous = *generation;
            *generation += 1;
            if let Some(queue) = self.upload_queues.remove(conversation_id) {
                stale.extend(queue.pending.into_iter().map(|batch| (batch, UploadTicket { conversation_id: conversation_id.clone(), generation: previous, queue: 0 })));
            }
            self.image_drafts.remove(conversation_id);
            self.file_drafts.remove(conversation_id);
            self.attachment_notices.remove(conversation_id);
            self.element_picks.remove(conversation_id);
            self.image_loading_ids.remove(conversation_id);
        }
        stale
    }

    /// Queued-message save barrier: a live steer can make a queued message part
    /// of an in-flight provider request, so the exact message must be durable
    /// before the steer acknowledges it.
    pub fn mark_queued_message_unsaved(&mut self, message_id: &str) {
        self.queued_needs_save.insert(message_id.to_owned());
    }

    pub fn track_queued_message_save(&mut self, message_id: &str, save: Deferred<Result<(), String>>) {
        self.queued_saves.insert(message_id.to_owned(), save);
    }

    /// A tracked save settled. A failed save stays tracked: the steer barrier awaits it, observes the rejection, and
    /// decides whether to drop or retry.
    pub fn queued_message_save_settled(&mut self, message_id: &str, save: &Deferred<Result<(), String>>, result: &Result<(), String>) {
        if result.is_err() || !self.queued_saves.get(message_id).is_some_and(|current| current.same(save)) {
            return;
        }
        self.queued_saves.remove(message_id);
        self.queued_needs_save.remove(message_id);
    }

    pub fn queued_message_save(&self, message_id: &str) -> Option<Deferred<Result<(), String>>> {
        self.queued_saves.get(message_id).cloned()
    }

    pub fn drop_queued_message_save_if_current(&mut self, message_id: &str, save: &Deferred<Result<(), String>>) {
        if self.queued_saves.get(message_id).is_some_and(|current| current.same(save)) {
            self.queued_saves.remove(message_id);
        }
    }

    pub fn queued_message_needs_save(&self, message_id: &str) -> bool {
        self.queued_needs_save.contains(message_id)
    }

    pub fn clear_queued_message_needs_save(&mut self, message_id: &str) {
        self.queued_needs_save.remove(message_id);
    }

    /// Drops every save/needs-save/failed-promotion record for removed messages.
    pub fn forget_queued_messages(&mut self, message_ids: &[String]) {
        for message_id in message_ids {
            self.queued_saves.remove(message_id);
            self.queued_needs_save.remove(message_id);
            self.failed_queued_promotion_ids.remove(message_id);
        }
    }

    /// What every send clears from a conversation's composer. Element picks go wherever images go: a clear that dropped
    /// the images but kept the picks would silently re-send the same elements with the next message, and the chips
    /// would stay on screen claiming they were never sent.
    pub fn clear_after_send(&mut self, conversation_id: &str) {
        self.drafts.insert(conversation_id.to_owned(), String::new());
        self.pasted_texts.insert(conversation_id.to_owned(), Vec::new());
        self.image_drafts.insert(conversation_id.to_owned(), Vec::new());
        self.element_picks.insert(conversation_id.to_owned(), Vec::new());
        if self.file_drafts.get(conversation_id).is_some_and(|files| !files.is_empty()) {
            self.file_drafts.insert(conversation_id.to_owned(), Vec::new());
        }
        self.attachment_notices.remove(conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch() -> UploadBatch {
        Box::new(|_, _| {})
    }

    #[test]
    fn serializes_upload_batches_per_conversation_and_tracks_the_loading_flag() {
        let mut composer = ComposerState::default();
        let (_, first) = composer.enqueue_image_upload("c1", batch()).expect("nothing running");
        assert!(composer.enqueue_image_upload("c1", batch()).is_none(), "the second waits for the first");
        assert!(composer.image_loading_ids.contains("c1"));
        let (_, second) = composer.finish_image_upload(&first).expect("the second starts now");
        // Keeps the loading flag while a later batch is still queued.
        assert!(composer.image_loading_ids.contains("c1"));
        assert!(composer.finish_image_upload(&second).is_none());
        assert!(!composer.image_loading_ids.contains("c1"));
    }

    #[test]
    fn other_conversations_upload_independently() {
        let mut composer = ComposerState::default();
        assert!(composer.enqueue_image_upload("c1", batch()).is_some());
        assert!(composer.enqueue_image_upload("c2", batch()).is_some());
    }

    #[test]
    fn invalidate_flips_upload_still_current_drops_drafts_and_clears_loading() {
        let mut composer = ComposerState::default();
        let (_, ticket) = composer.enqueue_image_upload("c1", batch()).unwrap();
        assert!(composer.enqueue_image_upload("c1", batch()).is_none());
        assert!(composer.upload_still_current(&ticket));
        composer.element_picks.insert("c1".into(), vec![]);
        composer.image_drafts.insert("c2".into(), vec![]);
        let stale = composer.invalidate_images(&["c1".to_owned()]);
        assert_eq!(stale.len(), 1, "a batch that never started still runs, to report it is stale");
        assert!(!composer.upload_still_current(&stale[0].1));
        assert!(!composer.upload_still_current(&ticket));
        assert!(!composer.image_loading_ids.contains("c1"));
        assert!(composer.element_picks.get("c1").is_none());
        assert!(composer.image_drafts.contains_key("c2"));
        // The batch that was running finds its queue gone and changes nothing.
        let (_, newer) = composer.enqueue_image_upload("c1", batch()).unwrap();
        assert!(composer.finish_image_upload(&ticket).is_none());
        assert!(composer.image_loading_ids.contains("c1"));
        assert!(composer.upload_still_current(&newer));
    }

    #[test]
    fn clears_save_tracking_when_the_tracked_save_resolves() {
        let mut composer = ComposerState::default();
        let save = Deferred::new();
        composer.mark_queued_message_unsaved("m");
        composer.track_queued_message_save("m", save.clone());
        assert!(composer.queued_message_save("m").is_some_and(|tracked| tracked.same(&save)));
        assert!(composer.queued_message_needs_save("m"));
        composer.queued_message_save_settled("m", &save, &Ok(()));
        assert!(composer.queued_message_save("m").is_none());
        assert!(!composer.queued_message_needs_save("m"));
    }

    #[test]
    fn keeps_a_rejected_save_tracked_for_the_steer_barrier_to_observe() {
        let mut composer = ComposerState::default();
        let save = Deferred::new();
        composer.mark_queued_message_unsaved("m");
        composer.track_queued_message_save("m", save.clone());
        composer.queued_message_save_settled("m", &save, &Err("save failed".to_owned()));
        assert!(composer.queued_message_save("m").is_some());
        composer.drop_queued_message_save_if_current("m", &save);
        assert!(composer.queued_message_save("m").is_none());
    }

    #[test]
    fn does_not_let_a_superseded_save_clear_a_newer_one() {
        let mut composer = ComposerState::default();
        let (stale, fresh) = (Deferred::new(), Deferred::new());
        composer.track_queued_message_save("m", stale.clone());
        composer.track_queued_message_save("m", fresh.clone());
        composer.queued_message_save_settled("m", &stale, &Ok(()));
        assert!(composer.queued_message_save("m").is_some_and(|tracked| tracked.same(&fresh)));
        composer.drop_queued_message_save_if_current("m", &stale);
        assert!(composer.queued_message_save("m").is_some_and(|tracked| tracked.same(&fresh)));
    }

    #[test]
    fn forget_drops_saves_and_failed_promotion_ids_in_one_sweep() {
        let mut composer = ComposerState::default();
        composer.mark_queued_message_unsaved("m1");
        composer.track_queued_message_save("m1", Deferred::new());
        composer.failed_queued_promotion_ids.extend(["m1".to_owned(), "m2".to_owned()]);
        composer.forget_queued_messages(&["m1".to_owned()]);
        assert!(composer.queued_message_save("m1").is_none());
        assert!(!composer.queued_message_needs_save("m1"));
        assert_eq!(composer.failed_queued_promotion_ids.len(), 1);
    }

    /// A staged element pick becomes prompt text at send time. If a clear dropped the images but kept the picks, the
    /// next message would silently re-send the same elements, and the chips would stay claiming they were never sent.
    #[test]
    fn a_clear_takes_element_picks_wherever_it_takes_images() {
        let mut composer = ComposerState::default();
        composer.drafts.insert("c".into(), "hi".into());
        composer.element_picks.insert("c".into(), vec![ElementPick { block: "<a>".into(), screenshot_image_id: None }]);
        composer.attachment_notices.insert("c".into(), vec![]);
        composer.clear_after_send("c");
        assert_eq!(composer.drafts["c"], "");
        assert!(composer.element_picks["c"].is_empty());
        assert!(composer.image_drafts["c"].is_empty());
        assert!(!composer.attachment_notices.contains_key("c"));
    }

    #[test]
    fn a_deferred_wakes_every_waiter_and_keeps_its_first_settlement() {
        let deferred: Deferred<u32> = Deferred::new();
        let waiter = {
            let deferred = deferred.clone();
            std::thread::spawn(move || deferred.wait())
        };
        deferred.resolve(1);
        deferred.resolve(2);
        assert_eq!(waiter.join().unwrap(), 1);
        assert_eq!(deferred.wait(), 1);
    }

    #[test]
    fn expands_every_label_and_leaves_hand_edited_ones_alone() {
        let pastes = vec![PastedText { id: 1, label: "[#1]".into(), content: "long text".into() }, PastedText { id: 2, label: "[#2]".into(), content: "other".into() }];
        assert_eq!(expand_pasted_texts("a [#1] b [#2] c [#1]", &pastes), "a long text b other c long text");
        assert_eq!(expand_pasted_texts("a [#", &pastes), "a [#");
    }

    #[test]
    fn element_blocks_go_ahead_of_what_was_typed() {
        let picks = vec![ElementPick { block: "<a>".into(), screenshot_image_id: None }, ElementPick { block: "<b>".into(), screenshot_image_id: None }];
        assert_eq!(with_selected_elements("hi", &picks), "<a>\n\n<b>\n\nhi");
        assert_eq!(with_selected_elements("", &picks), "<a>\n\n<b>");
        assert_eq!(with_selected_elements("hi", &[]), "hi");
    }
}

/// The value of `name="…"` in a block's opening `<element …>`.
pub fn opening_attribute(opening: &str, name: &str) -> Option<String> {
    let marker = format!(" {name}=\"");
    let start = opening.find(&marker)? + marker.len();
    let end = opening[start..].find('"')?;
    Some(opening[start..start + end].to_owned())
}

pub fn opening_of(block: &str) -> &str {
    let Some(start) = block.find("<element ") else { return "" };
    let end = block[start..].find('>').map_or(block.len(), |at| start + at + 1);
    &block[start..end]
}

const BLOCK_OPEN: &str = "<mewrk-selected-element>";
const BLOCK_CLOSE: &str = "</mewrk-selected-element>";

/// `selectedElementsFromText`, the inverse of what the send prepends: each leading block becomes the chip it was, carrying the block
/// verbatim so sending it again sends the same words, and each crop a block names is matched back to its image, in pick order.
pub fn selected_elements_from_text(text: &str, images: &[ImageAttachment]) -> (String, Vec<ElementPick>) {
    let mut crops = images.iter().filter(|image| image.name.starts_with("element-") && image.name.ends_with(".png"));
    let mut picks = Vec::new();
    let mut rest = text;
    while rest.starts_with(BLOCK_OPEN) {
        let Some(end) = rest.find(BLOCK_CLOSE).map(|at| at + BLOCK_CLOSE.len()) else { break };
        let block = &rest[..end];
        let crop = (opening_attribute(opening_of(block), "has-screenshot").as_deref() == Some("true")).then(|| crops.next()).flatten();
        picks.push(ElementPick { block: block.to_owned(), screenshot_image_id: crop.map(|image| image.id.clone()) });
        rest = rest[end..].trim_start();
    }
    (rest.to_owned(), picks)
}
