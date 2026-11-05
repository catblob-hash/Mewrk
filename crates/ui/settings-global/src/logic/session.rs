//! `settingsSession.tsx`: what one opening of the settings window has already loaded.
//!
//! A page that asks the host something slow reads it once per session: moving to another page and back reuses the
//! answer, while closing the window and opening it again starts over. The load itself is the caller's, since async
//! closures here run on the UI thread through the mailbox.

use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct SettingsSession {
    values: HashMap<String, Value>,
    loading: HashMap<String, u64>,
    generations: HashMap<String, u64>,
    next: u64,
}

impl SettingsSession {
    /// The value `key` holds, when a load of it has finished in this session.
    pub fn peek(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Records `value` under `key`, replacing an earlier one: the answer of a forced refresh.
    pub fn put(&mut self, key: &str, value: Value) {
        *self.generations.entry(key.to_owned()).or_default() += 1;
        self.values.insert(key.to_owned(), value);
    }

    /// Asks to start a load. `None` means one is running or the value is here: join it instead of asking twice.
    pub fn begin_load(&mut self, key: &str) -> Option<u64> {
        if self.values.contains_key(key) || self.loading.contains_key(key) {
            return None;
        }
        self.next += 1;
        self.loading.insert(key.to_owned(), self.next);
        Some(*self.generations.get(key).unwrap_or(&0))
    }

    /// A load finished. A `put` that landed while it ran is newer than its answer; a failed load is not remembered.
    pub fn finish_load(&mut self, key: &str, generation: u64, value: Option<Value>) {
        self.loading.remove(key);
        if let Some(value) = value
            && *self.generations.get(key).unwrap_or(&0) == generation {
                self.values.insert(key.to_owned(), value);
            }
    }

    pub fn is_loading(&self, key: &str) -> bool {
        self.loading.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_load_runs_once_per_key_and_a_put_outranks_it() {
        let mut s = SettingsSession::default();
        let generation = s.begin_load("k").unwrap();
        assert!(s.begin_load("k").is_none());
        s.put("k", json!(2));
        s.finish_load("k", generation, Some(json!(1)));
        assert_eq!(s.peek("k"), Some(&json!(2)));
    }

    #[test]
    fn a_failed_load_is_asked_again() {
        let mut s = SettingsSession::default();
        let generation = s.begin_load("k").unwrap();
        s.finish_load("k", generation, None);
        assert!(s.peek("k").is_none());
        assert!(s.begin_load("k").is_some());
    }
}
