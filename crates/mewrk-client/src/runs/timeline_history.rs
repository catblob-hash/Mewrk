//! Port of `lib/timelineHistory.ts`: undo and redo for edits made on a timeline.
//!
//! Each conversation keeps its own two stacks, so an undo only ever reaches the
//! timeline it was pressed in; the stacks are keyed by conversation id, which is
//! also why a fork or a hand-over starts with none — it is a new id. They live in
//! memory only: nothing is written to disk, and a restart forgets them.
//!
//! The timelines share one budget of [`TIMELINE_HISTORY_LIMIT`] entries,
//! counting both stacks. Past it, the entry touched longest ago goes, whichever
//! conversation it belongs to. Every push takes a fresh tick and every stack
//! grows at its top, so that entry is always at the bottom of some stack: the
//! oldest edit there is still to undo, or the last one there is to redo.
//!
//! An edit is recorded as a patch — what it took out, put in and rewrote, each
//! by id — rather than as two snapshots of the whole list. A model run appends to
//! the same list between an edit and its undo, and restoring a snapshot would
//! take those replies away with it.

use std::collections::{HashMap, HashSet};

use crate::model::ContextItem;

const TIMELINE_HISTORY_LIMIT: usize = 64;

/// A context as it sat in a list: where to put it back.
#[derive(Clone, Debug, PartialEq)]
pub struct PlacedContext {
    pub context: ContextItem,
    /// Its position in that list, the fallback when its neighbour is gone.
    pub index: usize,
    /// The context right before it, or `None` at the head of the list.
    pub after_id: Option<String>,
}

/// One edit on a timeline, readable in both directions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelinePatch {
    /// Contexts the edit took out, as they sat before it.
    pub removed: Vec<PlacedContext>,
    /// Contexts the edit put in, as they sit after it.
    pub inserted: Vec<PlacedContext>,
    /// Contexts the edit rewrote in place, `(before, after)`.
    pub replaced: Vec<(ContextItem, ContextItem)>,
}

fn clamp_index(index: isize, len: usize) -> usize {
    index.clamp(0, len as isize) as usize
}

/// Where `ids` sit in `contexts`, in list order, with what to put each one back after.
pub fn placed_contexts(contexts: &[ContextItem], ids: &HashSet<String>) -> Vec<PlacedContext> {
    contexts
        .iter()
        .enumerate()
        .filter(|(_, context)| ids.contains(context.id()))
        .map(|(index, context)| PlacedContext { context: context.clone(), index, after_id: index.checked_sub(1).map(|before| contexts[before].id().to_owned()) })
        .collect()
}

/// The patch for putting `context` at `index` of `contexts`.
pub fn insertion_patch(contexts: &[ContextItem], context: ContextItem, index: isize) -> TimelinePatch {
    let at = clamp_index(index, contexts.len());
    TimelinePatch { inserted: vec![PlacedContext { context, index: at, after_id: at.checked_sub(1).map(|before| contexts[before].id().to_owned()) }], ..TimelinePatch::default() }
}

/// The patch for gathering `ids` together, in list order, and putting them where
/// `index` points in `contexts` — or `None` when that leaves the list as it is.
///
/// A move takes the same contexts out and puts them back in, so undoing it is
/// the same patch read the other way: nothing is copied, and no id changes.
pub fn move_patch(contexts: &[ContextItem], ids: &HashSet<String>, index: isize) -> Option<TimelinePatch> {
    if !contexts.iter().any(|context| ids.contains(context.id())) {
        return None;
    }
    let at = clamp_index(index, contexts.len());
    let ahead = contexts[..at].iter().filter(|context| !ids.contains(context.id())).count();
    let (moved, rest): (Vec<&ContextItem>, Vec<&ContextItem>) = contexts.iter().partition(|context| ids.contains(context.id()));
    let next: Vec<ContextItem> = rest[..ahead].iter().chain(moved.iter()).chain(rest[ahead..].iter()).map(|context| (*context).clone()).collect();
    if next.iter().zip(contexts).all(|(left, right)| left.id() == right.id()) {
        return None;
    }
    Some(TimelinePatch { removed: placed_contexts(contexts, ids), inserted: placed_contexts(&next, ids), ..TimelinePatch::default() })
}

/// The patch for putting `pasted` where `index` points in `contexts`, taking
/// `replaced_ids` out as it does. With something to replace, the pasted contexts
/// go where the first of it sat, wherever `index` points.
pub fn paste_patch(contexts: &[ContextItem], pasted: &[ContextItem], index: isize, replaced_ids: &HashSet<String>) -> Option<TimelinePatch> {
    if pasted.is_empty() {
        return None;
    }
    let at = contexts.iter().position(|context| replaced_ids.contains(context.id())).unwrap_or_else(|| clamp_index(index, contexts.len()));
    let ahead = contexts[..at].iter().filter(|context| !replaced_ids.contains(context.id())).count();
    let rest: Vec<&ContextItem> = contexts.iter().filter(|context| !replaced_ids.contains(context.id())).collect();
    let next: Vec<ContextItem> = rest[..ahead].iter().copied().chain(pasted.iter()).chain(rest[ahead..].iter().copied()).cloned().collect();
    let pasted_ids: HashSet<String> = pasted.iter().map(|context| context.id().to_owned()).collect();
    Some(TimelinePatch { removed: placed_contexts(contexts, replaced_ids), inserted: placed_contexts(&next, &pasted_ids), ..TimelinePatch::default() })
}

/// The same edit, the other way round.
pub fn invert_timeline_patch(patch: &TimelinePatch) -> TimelinePatch {
    TimelinePatch { removed: patch.inserted.clone(), inserted: patch.removed.clone(), replaced: patch.replaced.iter().map(|(before, after)| (after.clone(), before.clone())).collect() }
}

/// The ids a patch takes out of a list.
pub fn removed_ids(patch: &TimelinePatch) -> Vec<String> {
    patch.removed.iter().map(|entry| entry.context.id().to_owned()).collect()
}

/// Applies a patch to `contexts`, or returns `None` when none of it lands any more:
/// everything it would take out is already gone, everything it would rewrite has
/// been deleted, and everything it would put back is already there.
///
/// What does land, lands by id. A context comes back after the neighbour it had,
/// wherever that neighbour is now, and only falls back to its old position when
/// the neighbour is gone too. Contexts put back together go in list order, so one
/// restored context can be the neighbour of the next.
pub fn apply_timeline_patch(contexts: &[ContextItem], patch: &TimelinePatch) -> Option<Vec<ContextItem>> {
    let mut changed = false;
    let dropped: HashSet<String> = removed_ids(patch).into_iter().collect();
    let mut next: Vec<ContextItem> = contexts.iter().filter(|context| !dropped.contains(context.id()) || { changed = true; false }).cloned().collect();
    let rewrites: HashMap<&str, &ContextItem> = patch.replaced.iter().map(|(_, after)| (after.id(), after)).collect();
    for context in &mut next {
        if let Some(rewrite) = rewrites.get(context.id()) {
            if *rewrite != context {
                *context = (*rewrite).clone();
                changed = true;
            }
        }
    }
    let mut present: HashSet<String> = next.iter().map(|context| context.id().to_owned()).collect();
    let mut inserted: Vec<&PlacedContext> = patch.inserted.iter().collect();
    inserted.sort_by_key(|entry| entry.index);
    for entry in inserted {
        if present.contains(entry.context.id()) {
            continue;
        }
        let at = match &entry.after_id {
            None => 0,
            Some(after) => next.iter().position(|context| context.id() == after).map_or(entry.index.min(next.len()), |anchor| anchor + 1),
        };
        next.insert(at, entry.context.clone());
        present.insert(entry.context.id().to_owned());
        changed = true;
    }
    changed.then_some(next)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineHistoryEntry {
    pub patch: TimelinePatch,
    /// What the edit did, in words, for the notice an undo or a redo shows.
    pub label: String,
}

#[derive(Clone, Debug)]
struct StoredEntry {
    entry: TimelineHistoryEntry,
    tick: u64,
}

#[derive(Clone, Debug, Default)]
struct Stacks {
    undo: Vec<StoredEntry>,
    redo: Vec<StoredEntry>,
}

#[derive(Clone, Debug)]
pub struct TimelineHistory {
    stacks: HashMap<String, Stacks>,
    clock: u64,
    limit: usize,
}

impl Default for TimelineHistory {
    fn default() -> Self {
        Self::with_limit(TIMELINE_HISTORY_LIMIT)
    }
}

impl TimelineHistory {
    pub fn with_limit(limit: usize) -> Self {
        Self { stacks: HashMap::new(), clock: 0, limit }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Records an edit made on `key`'s timeline. Whatever was there to redo is gone.
    pub fn record(&mut self, key: &str, entry: TimelineHistoryEntry) {
        let tick = self.tick();
        let stacks = self.stacks.entry(key.to_owned()).or_default();
        stacks.undo.push(StoredEntry { entry, tick });
        stacks.redo.clear();
        self.trim();
    }

    /// The edit an undo would revert, still in place.
    pub fn next_undo(&self, key: &str) -> Option<&TimelineHistoryEntry> {
        self.stacks.get(key)?.undo.last().map(|stored| &stored.entry)
    }

    /// The edit a redo would make again, still in place.
    pub fn next_redo(&self, key: &str) -> Option<&TimelineHistoryEntry> {
        self.stacks.get(key)?.redo.last().map(|stored| &stored.entry)
    }

    /// The last edit was reverted: it moves over to be redone.
    pub fn undone(&mut self, key: &str) {
        let tick = self.tick();
        if let Some(stacks) = self.stacks.get_mut(key) {
            if let Some(stored) = stacks.undo.pop() {
                stacks.redo.push(StoredEntry { entry: stored.entry, tick });
            }
        }
    }

    /// The last reverted edit was made again: it moves back to be undone.
    pub fn redone(&mut self, key: &str) {
        let tick = self.tick();
        if let Some(stacks) = self.stacks.get_mut(key) {
            if let Some(stored) = stacks.redo.pop() {
                stacks.undo.push(StoredEntry { entry: stored.entry, tick });
            }
        }
    }

    /// Drops the next undo without applying it: it no longer lands anywhere.
    pub fn drop_undo(&mut self, key: &str) {
        if let Some(stacks) = self.stacks.get_mut(key) {
            stacks.undo.pop();
        }
    }

    /// Drops the next redo without applying it.
    pub fn drop_redo(&mut self, key: &str) {
        if let Some(stacks) = self.stacks.get_mut(key) {
            stacks.redo.pop();
        }
    }

    /// Forgets everything recorded for `key`.
    pub fn forget(&mut self, key: &str) {
        self.stacks.remove(key);
    }

    /// Entries held across every timeline.
    pub fn size(&self) -> usize {
        self.stacks.values().map(|stacks| stacks.undo.len() + stacks.redo.len()).sum()
    }

    fn trim(&mut self) {
        while self.size() > self.limit {
            let mut oldest: Option<(String, bool, u64)> = None;
            for (key, stacks) in &self.stacks {
                for (is_undo, stack) in [(true, &stacks.undo), (false, &stacks.redo)] {
                    if let Some(bottom) = stack.first() {
                        if oldest.as_ref().is_none_or(|(_, _, tick)| bottom.tick < *tick) {
                            oldest = Some((key.clone(), is_undo, bottom.tick));
                        }
                    }
                }
            }
            let Some((key, is_undo, _)) = oldest else { return };
            let stacks = self.stacks.get_mut(&key).expect("found above");
            if is_undo {
                stacks.undo.remove(0);
            } else {
                stacks.redo.remove(0);
            }
            if stacks.undo.is_empty() && stacks.redo.is_empty() {
                self.stacks.remove(&key);
            }
        }
    }
}

/// What a keydown means on the timeline. `apple` is whether this platform holds ⌘ where others hold Ctrl.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimelineKeyPress<'a> {
    pub key: &'a str,
    pub ctrl: bool,
    pub meta: bool,
    pub alt: bool,
    pub shift: bool,
}

/// `isApplePlatform`.
pub const IS_APPLE_PLATFORM: bool = cfg!(target_os = "macos");

fn primary_held(press: TimelineKeyPress<'_>, apple: bool) -> bool {
    !press.alt && !press.shift && if apple { press.meta && !press.ctrl } else { press.ctrl && !press.meta }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineHistoryAction {
    Undo,
    Redo,
}

/// Whether a keydown is the timeline's undo or redo: Ctrl+Z and Ctrl+X, with ⌘ in
/// place of Ctrl on a Mac, where Ctrl+click is a right-click and ⌘ is what every
/// other shortcut there is held with.
pub fn timeline_history_key(press: TimelineKeyPress<'_>, apple: bool) -> Option<TimelineHistoryAction> {
    if !primary_held(press, apple) {
        return None;
    }
    match press.key.to_lowercase().as_str() {
        "z" => Some(TimelineHistoryAction::Undo),
        "x" => Some(TimelineHistoryAction::Redo),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelineClipboardAction {
    Copy,
    Paste,
}

/// Whether a keydown copies what is picked out on the timeline, or pastes what
/// was copied from one: Ctrl+C and Ctrl+V, with ⌘ in place of Ctrl on a Mac.
pub fn timeline_clipboard_key(press: TimelineKeyPress<'_>, apple: bool) -> Option<TimelineClipboardAction> {
    if !primary_held(press, apple) {
        return None;
    }
    match press.key.to_lowercase().as_str() {
        "c" => Some(TimelineClipboardAction::Copy),
        "v" => Some(TimelineClipboardAction::Paste),
        _ => None,
    }
}

/// The keycaps of the timeline's copy or paste, as this platform writes them.
pub fn timeline_clipboard_shortcut(action: TimelineClipboardAction, apple: bool) -> String {
    let letter = if action == TimelineClipboardAction::Copy { "C" } else { "V" };
    if apple { format!("⌘{letter}") } else { format!("Ctrl+{letter}") }
}

/// Whether a press holds the modifier that draws a selection box over the timeline.
pub fn timeline_selection_modifier(ctrl: bool, meta: bool, apple: bool) -> bool {
    if apple { meta } else { ctrl }
}

/// The keycaps of the timeline's undo or redo, as this platform writes them.
pub fn timeline_history_shortcut(action: TimelineHistoryAction, apple: bool) -> String {
    let letter = if action == TimelineHistoryAction::Undo { "Z" } else { "X" };
    if apple { format!("⌘{letter}") } else { format!("Ctrl+{letter}") }
}
