//! Conversation bodies as pooled data (`lib/conversationBodies.ts`).
//!
//! The document keeps every conversation, but a conversation's body (its contexts on every branch) is high-priority
//! data in the shared memory pool. Past the pool's cap the least recently opened bodies are unloaded: the conversation
//! stays in the document with its metadata, settings and queued messages, its id in [`App::unloaded`], and its contexts
//! are fetched from the host again when it is opened (`load_conversation`).
//!
//! The host is the only writer of bodies, so unloading and loading are purely local: neither is ever written back. A
//! body that is not loaded must never reach the host as if it were the conversation's timeline; see
//! [`UNLOADED_BODY_EXPECTED_IDS`] and `conversation_sync.rs`.
//!
//! Upstream's `bodyUnloaded` flag on the conversation is `App::unloaded` here: `model::Conversation` is the host's type.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::memory_pool::{MemoryPool, MemoryPoolKind, Pin, Unloaded};
use crate::model::{AppDocument, ContextItem, Conversation};
use crate::{App, Task};

/// The expected-context-ids a write sends for a conversation whose body is not loaded. It matches no real timeline, so
/// the host takes the write's metadata and keeps its own body, never the empty one the renderer holds.
pub const UNLOADED_BODY_EXPECTED_IDS: [&str; 1] = ["\u{0}unloaded-body"];

/// A body is re-measured at most this often. Upstream measures each item once by identity, which Rust values don't
/// have; a stale size for a moment costs nothing against a 2 GiB cap, and re-serializing on every change would.
const REMEASURE_INTERVAL: Duration = Duration::from_secs(1);

/// The conversation without its body: contexts and branches go, metadata, settings and queue stay. Branch records are
/// body: each hangs off a user message of the timeline, and the host refuses a conversation holding branches without it.
pub fn hollow_conversation(conversation: &mut Conversation) {
    conversation.contexts = Vec::new();
    conversation.branches = Vec::new();
}

/// Whether a conversation has no contexts. Never true of one whose body is not loaded: an unloaded body is not an empty one.
pub fn conversation_has_no_contexts(conversation: &Conversation, unloaded: &HashSet<String>) -> bool {
    !unloaded.contains(&conversation.id) && conversation.contexts.is_empty()
}

/// What an update may change about an unloaded conversation is its metadata. Anything an updater derived from the empty
/// stand-in body is dropped, not applied.
pub fn keep_unloaded_body(is_unloaded: bool, next: &mut Conversation) {
    if !is_unloaded {
        return;
    }
    if !next.contexts.is_empty() || !next.branches.is_empty() {
        eprintln!("对话 {} 的正文未载入，这次改动只保留元数据", next.id);
        hollow_conversation(next);
    }
}

/// Counts what serializing writes, so measuring a body allocates nothing.
struct Counter(u64);

impl std::io::Write for Counter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn contexts_bytes(contexts: &[ContextItem]) -> u64 {
    contexts.iter().map(|item| {
        let mut counter = Counter(0);
        let _ = serde_json::to_writer(&mut counter, item);
        counter.0
    }).sum()
}

/// What a conversation's body costs, as the JSON length of its items (bytes here; upstream counts UTF-16 units).
pub fn conversation_body_bytes(conversation: &Conversation) -> u64 {
    contexts_bytes(&conversation.contexts) + conversation.branches.iter().map(|branch| contexts_bytes(&branch.contexts)).sum::<u64>()
}

fn find_conversation<'a>(document: &'a AppDocument, conversation_id: &str) -> Option<&'a Conversation> {
    document.workspaces.iter().flat_map(|workspace| workspace.conversations.iter()).find(|conversation| conversation.id == conversation_id)
}

fn find_conversation_mut<'a>(document: &'a mut AppDocument, conversation_id: &str) -> Option<&'a mut Conversation> {
    document.workspaces.iter_mut().flat_map(|workspace| workspace.conversations.iter_mut()).find(|conversation| conversation.id == conversation_id)
}

type Waiter = Box<dyn FnOnce(&mut App, Result<bool, String>) + Send>;

/// Keeps a body loaded until [`App::release_conversation_body`].
#[must_use]
pub struct BodyHold(String);

#[derive(Default)]
pub(crate) struct BodyCache {
    pub pool: MemoryPool,
    sync_pins: HashMap<String, Pin>,
    holds: HashMap<String, (usize, Option<Pin>)>,
    loads: HashMap<String, Vec<Waiter>>,
    /// `bodyLoadFailures`: why the on-screen conversation's body last failed to load, by conversation.
    failures: HashMap<String, String>,
    /// Changed since they were last measured.
    dirty: HashSet<String>,
    measured_at: HashMap<String, Instant>,
    remeasure_timer: Option<crate::timers::TimerId>,
}

impl App {
    fn bodies(&mut self) -> &mut BodyCache {
        self.local::<BodyCache>()
    }

    /// The pool every renderer cache shares (`memoryPool`). Use [`App::pool_set`] and [`App::pool_resize`] to add to it,
    /// since a body they push out has to leave the document as well.
    pub fn memory_pool(&mut self) -> &mut MemoryPool {
        &mut self.bodies().pool
    }

    /// `pool.set`: stores a value as just used and enforces the cap.
    pub fn pool_set<T: std::any::Any + Send + Sync>(&mut self, kind: MemoryPoolKind, key: &str, value: T, bytes: u64) {
        let unloaded = self.bodies().pool.set(kind, key, value, bytes);
        self.handle_pool_unloads(unloaded);
    }

    /// `pool.resize`: re-counts an entry's bytes without counting it as a use and enforces the cap.
    pub fn pool_resize(&mut self, kind: MemoryPoolKind, key: &str, bytes: u64) {
        let unloaded = self.bodies().pool.resize(kind, key, bytes);
        self.handle_pool_unloads(unloaded);
    }

    /// `pooledFetch`: reads through the pool. The cached value, or `load`'s, kept once it arrives (a string's length is
    /// what it costs) and forgotten if it fails. Concurrent readers of one key share a single load.
    pub fn pooled_fetch(&mut self, kind: MemoryPoolKind, key: &str, load: impl FnOnce(&Task) -> Result<String, String> + Send + 'static, done: impl FnOnce(&mut App, Result<std::sync::Arc<String>, String>) + Send + 'static) {
        if let Some(cached) = self.memory_pool().get::<String>(kind, key) {
            return done(self, Ok(cached));
        }
        type Reader = Box<dyn FnOnce(&mut App, Result<std::sync::Arc<String>, String>) + Send>;
        #[derive(Default)]
        struct Pending(HashMap<(MemoryPoolKind, String), Vec<Reader>>);
        let id = (kind, key.to_owned());
        let pending = self.local::<Pending>();
        if let Some(readers) = pending.0.get_mut(&id) {
            readers.push(Box::new(done));
            return;
        }
        pending.0.insert(id.clone(), vec![Box::new(done)]);
        self.spawn(move |task| {
            let result = load(&task);
            task.post(move |app| {
                let readers = app.local::<Pending>().0.remove(&id).unwrap_or_default();
                let result = result.map(|value| {
                    // A value too big for the pool is still this load's answer; it just isn't kept.
                    let value = std::sync::Arc::new(value);
                    let bytes = value.len() as u64;
                    let unloaded = app.bodies().pool.set_shared(id.0, &id.1, value.clone(), bytes);
                    app.handle_pool_unloads(unloaded);
                    value
                });
                for reader in readers {
                    reader(app, result.clone());
                }
            });
        });
    }

    fn handle_pool_unloads(&mut self, unloaded: Unloaded) {
        let bodies: Vec<String> = unloaded.into_iter().filter(|(kind, _)| *kind == MemoryPoolKind::ConversationBody).map(|(_, key)| key).collect();
        if !bodies.is_empty() {
            self.unload_conversation_bodies(&bodies);
        }
    }

    /// `isBodyUnloaded`.
    pub fn is_body_unloaded(&self, conversation_id: &str) -> bool {
        self.unloaded.contains(conversation_id)
    }

    /// `conversationHasNoContexts`: an unloaded body is not an empty one.
    pub fn conversation_has_no_contexts(&self, conversation: &Conversation) -> bool {
        conversation_has_no_contexts(conversation, &self.unloaded)
    }

    /// Notes that a body changed, so its size is measured again.
    pub(crate) fn mark_body_dirty(&mut self, conversation_id: &str) {
        self.bodies().dirty.insert(conversation_id.to_owned());
    }

    /// Drops these conversations' bodies from the document without writing it back.
    fn unload_conversation_bodies(&mut self, conversation_ids: &[String]) {
        let mut changed = false;
        for id in conversation_ids {
            if self.unloaded.contains(id) {
                continue;
            }
            if let Some(conversation) = find_conversation_mut(&mut self.doc, id) {
                hollow_conversation(conversation);
                self.unloaded.insert(id.clone());
                changed = true;
            }
        }
        if changed {
            self.document_changed();
        }
    }

    /// `conversationBodies.sync`: mirrors the document into the pool: each loaded body's size, the pins, conversations
    /// that went away. Unloads what the cap then requires.
    pub(crate) fn sync_conversation_bodies(&mut self, pinned_ids: &HashSet<String>) {
        // Pins first, so the sizes that follow never unload what must stay.
        let cache = self.bodies();
        for id in pinned_ids {
            if !cache.sync_pins.contains_key(id) {
                let pin = cache.pool.pin(MemoryPoolKind::ConversationBody, id);
                cache.sync_pins.insert(id.clone(), pin);
            }
        }
        let now = Instant::now();
        let mut present = HashSet::new();
        let mut measures = Vec::new();
        let mut deferred = false;
        let dirty = std::mem::take(&mut self.bodies().dirty);
        let mut still_dirty = HashSet::new();
        // Taken out so the document can be read while the cache is written.
        let mut cache = std::mem::take(self.bodies());
        for workspace in &self.doc.workspaces {
            for conversation in &workspace.conversations {
                if self.unloaded.contains(&conversation.id) {
                    continue;
                }
                present.insert(conversation.id.clone());
                let known = cache.pool.has(MemoryPoolKind::ConversationBody, &conversation.id);
                if known && !dirty.contains(&conversation.id) {
                    continue;
                }
                if known && cache.measured_at.get(&conversation.id).is_some_and(|at| now.duration_since(*at) < REMEASURE_INTERVAL) {
                    still_dirty.insert(conversation.id.clone());
                    deferred = true;
                    continue;
                }
                measures.push((conversation.id.clone(), conversation_body_bytes(conversation), known));
                cache.measured_at.insert(conversation.id.clone(), now);
            }
        }
        cache.dirty.extend(still_dirty);
        let mut unloaded = Vec::new();
        for (id, bytes, known) in measures {
            let more = if known { cache.pool.resize(MemoryPoolKind::ConversationBody, &id, bytes) } else { cache.pool.set(MemoryPoolKind::ConversationBody, &id, true, bytes) };
            unloaded.extend(more);
        }
        for key in cache.pool.keys(MemoryPoolKind::ConversationBody) {
            if !present.contains(&key) {
                cache.pool.delete(MemoryPoolKind::ConversationBody, &key);
                cache.measured_at.remove(&key);
            }
        }
        let releases: Vec<String> = cache.sync_pins.keys().filter(|id| !pinned_ids.contains(*id)).cloned().collect();
        for id in releases {
            if let Some(pin) = cache.sync_pins.remove(&id) {
                unloaded.extend(cache.pool.release(pin));
            }
        }
        *self.bodies() = cache;
        self.handle_pool_unloads(unloaded);
        if deferred && self.bodies().remeasure_timer.is_none() {
            let timer = self.set_timer(REMEASURE_INTERVAL, |app| {
                app.bodies().remeasure_timer = None;
                app.schedule_doc_effects();
            });
            self.bodies().remeasure_timer = Some(timer);
        }
    }

    /// `conversationBodies.touch`: marks a conversation's body as just used; the user opened it.
    pub fn touch_conversation_body(&mut self, conversation_id: &str) {
        self.memory_pool().touch(MemoryPoolKind::ConversationBody, conversation_id);
    }

    /// `conversationBodies.hold`: keeps a body loaded (once loaded) until released.
    pub fn hold_conversation_body(&mut self, conversation_id: &str) -> BodyHold {
        let cache = self.bodies();
        let entry = cache.holds.entry(conversation_id.to_owned()).or_insert((0, None));
        entry.0 += 1;
        if entry.1.is_none() {
            entry.1 = Some(cache.pool.pin(MemoryPoolKind::ConversationBody, conversation_id));
        }
        BodyHold(conversation_id.to_owned())
    }

    /// Lets go of a [`BodyHold`].
    pub fn release_conversation_body(&mut self, hold: BodyHold) {
        let cache = self.bodies();
        let Some(entry) = cache.holds.get_mut(&hold.0) else { return };
        entry.0 -= 1;
        if entry.0 > 0 {
            return;
        }
        let pin = cache.holds.remove(&hold.0).and_then(|(_, pin)| pin);
        if let Some(pin) = pin {
            let unloaded = self.bodies().pool.release(pin);
            self.handle_pool_unloads(unloaded);
        }
    }

    /// `conversationBodies.isLoading`.
    pub fn conversation_body_is_loading(&mut self, conversation_id: &str) -> bool {
        self.bodies().loads.contains_key(conversation_id)
    }

    /// `bodyLoadFailures[id]`: why the body last failed to load, for the retry view.
    pub fn body_load_failure(&mut self, conversation_id: &str) -> Option<String> {
        self.bodies().failures.get(conversation_id).cloned()
    }

    /// `conversationBodies.ensureLoaded`: fetches the body when it isn't loaded. `done` gets `Ok(false)` when the
    /// conversation is gone, also if it went while its body was on the way. Concurrent callers share one fetch.
    pub fn ensure_conversation_loaded(&mut self, conversation_id: &str, done: impl FnOnce(&mut App, Result<bool, String>) + Send + 'static) {
        if find_conversation(&self.doc, conversation_id).is_none() {
            return done(self, Ok(false));
        }
        if !self.unloaded.contains(conversation_id) {
            return done(self, Ok(true));
        }
        if let Some(waiters) = self.bodies().loads.get_mut(conversation_id) {
            waiters.push(Box::new(done));
            return;
        }
        let hold = self.hold_conversation_body(conversation_id);
        self.bodies().loads.insert(conversation_id.to_owned(), vec![Box::new(done)]);
        crate::prelude::repaint();
        let id = conversation_id.to_owned();
        let backend = self.doc_backend();
        self.spawn(move |task| {
            let request = json!({ "conversationId": id });
            let loaded = crate::doc::call_host(&task, backend.as_ref(), "load_conversation", request).and_then(|value| serde_json::from_value::<Option<Conversation>>(value).map_err(|e| format!("load_conversation: {e}")));
            // Measured here so a long conversation doesn't stall a frame.
            let measured = loaded.map(|conversation| conversation.map(|conversation| {
                let bytes = conversation_body_bytes(&conversation);
                (conversation, bytes)
            }));
            task.post(move |app| {
                let result = match measured {
                    Err(error) => Err(error),
                    // A conversation deleted while its body was on the way stays deleted.
                    Ok(None) => Ok(false),
                    Ok(Some((conversation, bytes))) => {
                        if find_conversation(&app.doc, &id).is_none() {
                            Ok(false)
                        } else {
                            if app.unloaded.contains(&id) {
                                app.install_conversation_body(conversation, bytes);
                            }
                            Ok(true)
                        }
                    }
                };
                let waiters = app.bodies().loads.remove(&id).unwrap_or_default();
                app.release_conversation_body(hold);
                crate::prelude::repaint();
                for waiter in waiters {
                    waiter(app, result.clone());
                }
            });
        });
    }

    /// Puts a loaded conversation into the document without writing it back.
    fn install_conversation_body(&mut self, conversation: Conversation, bytes: u64) {
        let id = conversation.id.clone();
        if let Some(slot) = find_conversation_mut(&mut self.doc, &id) {
            *slot = conversation;
            self.unloaded.remove(&id);
            self.bodies().measured_at.insert(id.clone(), Instant::now());
            self.pool_set(MemoryPoolKind::ConversationBody, &id, true, bytes);
            self.document_changed();
        }
    }

    /// `loadActiveBody`: loads the on-screen conversation's body, keeping why it failed for the retry button. Retrying
    /// is calling this again.
    pub fn load_active_body(&mut self, conversation_id: &str) {
        self.bodies().failures.remove(conversation_id);
        let id = conversation_id.to_owned();
        self.ensure_conversation_loaded(conversation_id, move |app, result| {
            if let Err(error) = result {
                app.bodies().failures.insert(id, error);
                crate::prelude::repaint();
            }
        });
    }
}

impl Task {
    /// [`App::ensure_conversation_loaded`], for a task thread.
    pub fn ensure_conversation_loaded(&self, conversation_id: &str) -> Result<bool, String> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let id = conversation_id.to_owned();
        self.post(move |app| {
            app.ensure_conversation_loaded(&id, move |_, result| {
                let _ = tx.send(result);
            });
        });
        rx.recv().unwrap_or_else(|_| Err("the app is closing".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::{conversation, text};

    #[test]
    fn an_unloaded_body_is_not_an_empty_one() {
        let mut loaded = conversation("c");
        loaded.contexts = vec![text("u", "user")];
        let mut hollow = loaded.clone();
        hollow_conversation(&mut hollow);
        let unloaded: HashSet<String> = ["c".to_owned()].into();
        assert!(hollow.contexts.is_empty());
        assert!(!conversation_has_no_contexts(&hollow, &unloaded));
        assert!(conversation_has_no_contexts(&conversation("e"), &HashSet::new()));
    }

    #[test]
    fn an_update_to_an_unloaded_conversation_keeps_its_metadata_and_drops_any_body_built_from_nothing() {
        let mut renamed = conversation("c");
        renamed.title = "renamed".into();
        keep_unloaded_body(true, &mut renamed);
        assert_eq!(renamed.title, "renamed");
        let mut spliced = conversation("c");
        spliced.contexts = vec![text("x", "user")];
        keep_unloaded_body(true, &mut spliced);
        assert!(spliced.contexts.is_empty());
        let mut next = conversation("d");
        next.contexts = vec![text("u", "user")];
        keep_unloaded_body(false, &mut next);
        assert_eq!(next.contexts.len(), 1);
    }

    #[test]
    fn measures_a_body_as_the_json_length_of_its_items() {
        let mut c = conversation("c");
        c.contexts = vec![text("a", "user"), text("b", "assistant")];
        let expected: usize = c.contexts.iter().map(|item| serde_json::to_string(item).unwrap().len()).sum();
        assert_eq!(conversation_body_bytes(&c), expected as u64);
    }
}
