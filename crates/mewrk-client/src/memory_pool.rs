//! The renderer's one shared memory pool (`lib/memoryPool.ts`), the counterpart of the host's `memory_pool.rs`, with the
//! same rules.
//!
//! Every conversation keeps its body here, and attachments keep their chips and raw bytes; all of them draw on one
//! budget. Each entry has a priority, and once the pool is over its cap it unloads the least recently used entry of the
//! lowest priority first:
//!
//! - high: what the timeline draws: message bodies, reasoning, tool output, pasted text, and the small chip an image
//!   shows. Opening a conversation or expanding a card must not wait on the host.
//! - low: the raw bytes of images and uploaded files. Only the full-size viewer and the file preview read them, and both
//!   can afford a fetch.
//!
//! Only data is managed here, never a process: what goes in is a copy of something the host can hand back, so unloading
//! it costs a fetch and nothing else. Run state, the composer, terminals and the rest of the live state are not entries
//! and are never unloaded.
//!
//! A pinned entry (the conversation on screen, one with a run in flight) is never unloaded, so the pool can sit over its
//! cap until the pin goes.
//!
//! Upstream notifies listeners of what it unloaded; here each call that can unload returns what it unloaded instead, so
//! the caller holding `&mut App` can act on it.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The pool's cap: 2 GiB across every tier, as on the host.
const MEMORY_POOL_CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoryPoolKind {
    ConversationBody,
    ImageThumbnail,
    ImageData,
    FileData,
}

/// The kind fixes the priority, so raw bytes can never be filed as timeline content. 0 goes first.
fn priority_rank(kind: MemoryPoolKind) -> u8 {
    match kind {
        MemoryPoolKind::ImageData | MemoryPoolKind::FileData => 0,
        MemoryPoolKind::ConversationBody | MemoryPoolKind::ImageThumbnail => 1,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryPoolEntryStats {
    pub entries: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPoolStats {
    pub cap_bytes: u64,
    pub total_bytes: u64,
    pub pinned_entries: usize,
    pub by_kind: BTreeMap<MemoryPoolKind, MemoryPoolEntryStats>,
}

type Value = Arc<dyn Any + Send + Sync>;

struct Entry {
    value: Value,
    bytes: u64,
    last_used: u64,
}

type Id = (MemoryPoolKind, String);

/// What a call unloaded, after it is gone.
pub type Unloaded = Vec<(MemoryPoolKind, String)>;

/// Keeps an entry loaded until released with [`MemoryPool::release`]. Pins nest.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Pin(Id);

pub struct MemoryPool {
    cap_bytes: u64,
    entries: HashMap<Id, Entry>,
    pins: HashMap<Id, usize>,
    total: u64,
    clock: u64,
}

impl Default for MemoryPool {
    fn default() -> Self {
        Self::new(MEMORY_POOL_CAP_BYTES)
    }
}

impl MemoryPool {
    /// `createMemoryPool(capBytes)`.
    pub fn new(cap_bytes: u64) -> Self {
        Self { cap_bytes, entries: HashMap::new(), pins: HashMap::new(), total: 0, clock: 0 }
    }

    fn id(kind: MemoryPoolKind, key: &str) -> Id {
        (kind, key.to_owned())
    }

    fn remove(&mut self, id: &Id) {
        if let Some(entry) = self.entries.remove(id) {
            self.total -= entry.bytes;
        }
    }

    /// Unloads unpinned entries, lowest priority and least recently used first.
    fn enforce(&mut self) -> Unloaded {
        if self.total <= self.cap_bytes {
            return Vec::new();
        }
        let mut candidates: Vec<(Id, u8, u64)> = self.entries.iter().filter(|(id, _)| !self.pins.contains_key(*id)).map(|(id, entry)| (id.clone(), priority_rank(id.0), entry.last_used)).collect();
        candidates.sort_by_key(|(_, rank, last_used)| (*rank, *last_used));
        let mut unloaded = Vec::new();
        for (id, _, _) in candidates {
            if self.total <= self.cap_bytes {
                break;
            }
            self.remove(&id);
            unloaded.push(id);
        }
        unloaded
    }

    /// The cached value, marked as just used.
    pub fn get<T: Any + Send + Sync>(&mut self, kind: MemoryPoolKind, key: &str) -> Option<Arc<T>> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(&Self::id(kind, key))?;
        entry.last_used = clock;
        entry.value.clone().downcast().ok()
    }

    /// The cached value without counting it as a use.
    pub fn peek<T: Any + Send + Sync>(&self, kind: MemoryPoolKind, key: &str) -> Option<Arc<T>> {
        self.entries.get(&Self::id(kind, key))?.value.clone().downcast().ok()
    }

    pub fn has(&self, kind: MemoryPoolKind, key: &str) -> bool {
        self.entries.contains_key(&Self::id(kind, key))
    }

    /// Stores a value as just used, replacing any earlier one, then enforces the cap.
    pub fn set<T: Any + Send + Sync>(&mut self, kind: MemoryPoolKind, key: &str, value: T, bytes: u64) -> Unloaded {
        self.set_shared(kind, key, Arc::new(value), bytes)
    }

    /// The same for a value the caller keeps a handle on, which outlives the entry if the cap takes it straight away.
    pub fn set_shared<T: Any + Send + Sync>(&mut self, kind: MemoryPoolKind, key: &str, value: Arc<T>, bytes: u64) -> Unloaded {
        let id = Self::id(kind, key);
        self.remove(&id);
        self.clock += 1;
        self.entries.insert(id, Entry { value, bytes, last_used: self.clock });
        self.total += bytes;
        self.enforce()
    }

    /// Re-counts an entry's bytes without counting it as a use, then enforces the cap.
    pub fn resize(&mut self, kind: MemoryPoolKind, key: &str, bytes: u64) -> Unloaded {
        let Some(entry) = self.entries.get_mut(&Self::id(kind, key)) else { return Vec::new() };
        self.total = self.total + bytes - entry.bytes;
        entry.bytes = bytes;
        self.enforce()
    }

    /// Marks an entry as just used.
    pub fn touch(&mut self, kind: MemoryPoolKind, key: &str) {
        self.clock += 1;
        let clock = self.clock;
        if let Some(entry) = self.entries.get_mut(&Self::id(kind, key)) {
            entry.last_used = clock;
        }
    }

    pub fn delete(&mut self, kind: MemoryPoolKind, key: &str) {
        self.remove(&Self::id(kind, key));
    }

    pub fn keys(&self, kind: MemoryPoolKind) -> Vec<String> {
        self.entries.keys().filter(|(entry_kind, _)| *entry_kind == kind).map(|(_, key)| key.clone()).collect()
    }

    /// Keeps the entry loaded until [`Self::release`], loaded yet or not. Pins nest.
    pub fn pin(&mut self, kind: MemoryPoolKind, key: &str) -> Pin {
        let id = Self::id(kind, key);
        *self.pins.entry(id.clone()).or_insert(0) += 1;
        Pin(id)
    }

    /// Lets go of a pin, then enforces the cap.
    pub fn release(&mut self, pin: Pin) -> Unloaded {
        match self.pins.get_mut(&pin.0) {
            Some(count) if *count > 1 => *count -= 1,
            _ => {
                self.pins.remove(&pin.0);
            }
        }
        self.enforce()
    }

    pub fn stats(&self) -> MemoryPoolStats {
        let mut by_kind: BTreeMap<MemoryPoolKind, MemoryPoolEntryStats> = BTreeMap::new();
        for ((kind, _), entry) in &self.entries {
            let usage = by_kind.entry(*kind).or_default();
            usage.entries += 1;
            usage.bytes += entry.bytes;
        }
        MemoryPoolStats { cap_bytes: self.cap_bytes, total_bytes: self.total, pinned_entries: self.pins.len(), by_kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use MemoryPoolKind::*;

    fn sorted(mut keys: Vec<String>) -> Vec<String> {
        keys.sort();
        keys
    }

    #[test]
    fn unloads_low_priority_data_first_even_when_it_was_used_more_recently() {
        let mut pool = MemoryPool::new(100);
        assert!(pool.set(ConversationBody, "old-body", true, 40).is_empty());
        assert!(pool.set(ImageData, "new-image", "data:", 40).is_empty());
        let unloaded = pool.set(ConversationBody, "new-body", true, 40);
        assert_eq!(unloaded, vec![(ImageData, "new-image".to_owned())]);
        assert_eq!(sorted(pool.keys(ConversationBody)), vec!["new-body", "old-body"]);
    }

    #[test]
    fn within_a_tier_unloads_the_least_recently_used() {
        let mut pool = MemoryPool::new(100);
        let _ = pool.set(ConversationBody, "a", true, 40);
        let _ = pool.set(ImageThumbnail, "b", "thumb", 40);
        pool.touch(ConversationBody, "a");
        let _ = pool.set(ConversationBody, "c", true, 40);
        assert!(!pool.has(ImageThumbnail, "b"));
        assert_eq!(sorted(pool.keys(ConversationBody)), vec!["a", "c"]);
    }

    #[test]
    fn a_resize_or_a_peek_is_not_a_use() {
        let mut pool = MemoryPool::new(100);
        let _ = pool.set(ConversationBody, "a", true, 40);
        let _ = pool.set(ConversationBody, "b", true, 40);
        let _ = pool.resize(ConversationBody, "a", 45);
        let _ = pool.peek::<bool>(ConversationBody, "a");
        let _ = pool.set(ConversationBody, "c", true, 40);
        assert_eq!(sorted(pool.keys(ConversationBody)), vec!["b", "c"]);
    }

    #[test]
    fn keeps_pinned_entries_over_the_cap_and_enforces_it_once_they_are_released() {
        let mut pool = MemoryPool::new(100);
        let pin = pool.pin(ConversationBody, "active");
        let _ = pool.set(ConversationBody, "active", true, 90);
        let _ = pool.set(ConversationBody, "other", true, 40);
        assert_eq!(pool.keys(ConversationBody), vec!["active"]);
        let _ = pool.resize(ConversationBody, "active", 150);
        assert_eq!(pool.stats().total_bytes, 150);
        let _ = pool.release(pin);
        assert_eq!(pool.stats().total_bytes, 0);
    }

    #[test]
    fn counts_bytes_by_kind() {
        let mut pool = MemoryPool::new(1000);
        let _ = pool.set(ConversationBody, "a", true, 10);
        let _ = pool.set(ConversationBody, "b", true, 5);
        let _ = pool.set(FileData, "f", "data:", 7);
        let stats = pool.stats();
        assert_eq!((stats.cap_bytes, stats.total_bytes, stats.pinned_entries), (1000, 22, 0));
        assert_eq!(stats.by_kind[&ConversationBody], MemoryPoolEntryStats { entries: 2, bytes: 15 });
        assert_eq!(stats.by_kind[&FileData], MemoryPoolEntryStats { entries: 1, bytes: 7 });
    }

    #[test]
    fn pins_nest_and_values_come_back_typed() {
        let mut pool = MemoryPool::new(10);
        let first = pool.pin(ImageData, "x");
        let second = pool.pin(ImageData, "x");
        let _ = pool.set(ImageData, "x", "value".to_owned(), 20);
        assert_eq!(pool.get::<String>(ImageData, "x").as_deref().map(String::as_str), Some("value"));
        assert_eq!(pool.get::<bool>(ImageData, "x"), None);
        let _ = pool.release(first);
        assert!(pool.has(ImageData, "x"));
        let _ = pool.release(second);
        assert!(!pool.has(ImageData, "x"));
    }
}
