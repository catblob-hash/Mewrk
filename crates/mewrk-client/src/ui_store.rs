//! What upstream kept in `localStorage`: view preferences that aren't the document. They live in `ui-state.json` in the
//! data folder, a flat map of strings as `localStorage` was, written debounced and atomically.
//!
//! These are preferences, not data: a read that fails falls back and a write that fails costs the preference, as with
//! upstream's guarded `localStorage` calls.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tauri::Manager;

use crate::timers::TimerId;
use crate::App;

const FILE_NAME: &str = "ui-state.json";
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// `localStorage`, as a value. Reach it with [`App::ui_store`].
#[derive(Default)]
pub struct UiStore {
    values: BTreeMap<String, String>,
    path: Option<PathBuf>,
    loaded: bool,
    dirty: bool,
}

impl UiStore {
    fn load_from(path: PathBuf) -> Self {
        // An unreadable file reads as empty; the next write replaces it.
        let values = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Self { values, path: Some(path), loaded: true, dirty: false }
    }

    /// `getItem`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// `setItem`.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        if self.values.get(key) != Some(&value) {
            self.values.insert(key.to_owned(), value);
            self.dirty = true;
        }
    }

    /// `removeItem`.
    pub fn remove(&mut self, key: &str) {
        if self.values.remove(key).is_some() {
            self.dirty = true;
        }
    }

    /// `JSON.parse(getItem(key))`, or `None` when it's absent or isn't that shape.
    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_str(self.get(key)?).ok()
    }

    /// `setItem(key, JSON.stringify(value))`.
    pub fn set_json<T: Serialize>(&mut self, key: &str, value: &T) {
        if let Ok(text) = serde_json::to_string(value) {
            self.set(key, text);
        }
    }

    /// `readStoredFlag` of `lib/paneSettings.ts`.
    pub fn read_flag(&self, key: &str, fallback: bool) -> bool {
        self.get(key).map_or(fallback, |stored| stored == "true")
    }

    /// `writeStoredFlag` of `lib/paneSettings.ts`.
    pub fn write_flag(&mut self, key: &str, value: bool) {
        self.set(key, if value { "true" } else { "false" });
    }

    /// `readStoredChoice` of `lib/paneSettings.ts`: one of `choices`, falling back when the stored value is stale or absent.
    pub fn read_choice<'a>(&self, key: &str, choices: &[&'a str], fallback: &'a str) -> &'a str {
        self.get(key).and_then(|stored| choices.iter().find(|c| **c == stored).copied()).unwrap_or(fallback)
    }

    /// `writeStoredChoice` of `lib/paneSettings.ts`.
    pub fn write_choice(&mut self, key: &str, value: &str) {
        self.set(key, value);
    }

    /// Writes now, if anything changed. Written beside the file and renamed over it, so a crash leaves the old one.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let Some(path) = &self.path else { return Ok(()) };
        let text = serde_json::to_vec_pretty(&self.values)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(&temporary, path)?;
        self.dirty = false;
        Ok(())
    }

    fn data_file(app: &App) -> Option<PathBuf> {
        app.host.app().path().app_data_dir().ok().map(|dir| dir.join(FILE_NAME))
    }
}

#[derive(Default)]
struct SaveTimer(Option<TimerId>);

/// `SIDEBAR_WIDTH_STORAGE_KEY`.
const SIDEBAR_WIDTH_KEY: &str = "mewrk.sidebar-width";
/// `LEGACY_SIDEBAR_WIDTH_STORAGE_KEY`: read when the current key has nothing.
const LEGACY_SIDEBAR_WIDTH_KEY: &str = "naiword.sidebar-width";
/// `SIDEBAR_DEFAULT_WIDTH`.
pub const SIDEBAR_DEFAULT_WIDTH: f64 = 264.0;
/// `SIDEBAR_MIN_WIDTH`.
pub const SIDEBAR_MIN_WIDTH: f64 = 220.0;
/// `SIDEBAR_MAX_WIDTH`.
pub const SIDEBAR_MAX_WIDTH: f64 = 420.0;

/// `clampSidebarWidth`.
pub fn clamp_sidebar_width(width: f64) -> f64 {
    // `Math.round` rounds halves up, where `f64::round` rounds them away from zero; widths aren't negative.
    width.round().clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH)
}

/// `loadSidebarWidth`: a stored width that isn't a positive number reads as the default.
pub fn load_sidebar_width(store: &UiStore) -> f64 {
    let stored = store.get(SIDEBAR_WIDTH_KEY).or_else(|| store.get(LEGACY_SIDEBAR_WIDTH_KEY)).and_then(|text| text.trim().parse::<f64>().ok());
    match stored {
        Some(width) if width.is_finite() && width > 0.0 => clamp_sidebar_width(width),
        _ => SIDEBAR_DEFAULT_WIDTH,
    }
}

/// `lib/catalogOrder.ts`'s key: where the user put the rows of a catalog this pane cannot write to. Skills, MCP servers,
/// hooks, roles and tool-description files are scanned off disk; templates and presets carry no sort field. None of them
/// has anywhere to keep a position, so the order is a view preference.
const CATALOG_ORDER_KEY: &str = "mewrk.catalog-order.v1";

/// `CatalogOrderScope`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogOrderScope {
    Skills,
    Mcp,
    Hooks,
    Agents,
    ToolDescriptions,
    Templates,
    Presets,
}

impl CatalogOrderScope {
    fn key(self) -> &'static str {
        match self {
            Self::Skills => "skills",
            Self::Mcp => "mcp",
            Self::Hooks => "hooks",
            Self::Agents => "agents",
            Self::ToolDescriptions => "toolDescriptions",
            Self::Templates => "templates",
            Self::Presets => "presets",
        }
    }
}

/// `readCatalogOrder`.
pub fn read_catalog_order(store: &UiStore, scope: CatalogOrderScope) -> Vec<String> {
    let stored: Option<serde_json::Value> = store.get_json(CATALOG_ORDER_KEY);
    stored.and_then(|stored| stored.get(scope.key()).and_then(|ids| ids.as_array()).map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect())).unwrap_or_default()
}

/// `writeCatalogOrder`.
pub fn write_catalog_order(store: &mut UiStore, scope: CatalogOrderScope, ids: &[String]) {
    let mut stored: serde_json::Value = store.get_json(CATALOG_ORDER_KEY).filter(serde_json::Value::is_object).unwrap_or_else(|| serde_json::json!({}));
    stored[scope.key()] = serde_json::json!(ids);
    store.set_json(CATALOG_ORDER_KEY, &stored);
}

/// `sortByOrder`: stable by construction: an unranked row keeps its catalog position relative to the other unranked rows,
/// and the whole unranked group follows the ranked one rather than being scattered through it. A row the catalog lost
/// simply drops out.
pub fn sort_by_catalog_order<T>(items: Vec<T>, order: &[String], get_id: impl Fn(&T) -> &str) -> Vec<T> {
    if order.is_empty() {
        return items;
    }
    let mut ranked: Vec<(Option<usize>, T)> = items.into_iter().map(|item| (order.iter().position(|id| id == get_id(&item)), item)).collect();
    ranked.sort_by_key(|(rank, _)| rank.unwrap_or(usize::MAX));
    ranked.into_iter().map(|(_, item)| item).collect()
}

/// `lib/cacheBreakWarning.ts`'s key. "Don't show again" is a view preference of the same class as a catalog's row order.
const CACHE_BREAK_WARNING_KEY: &str = "mewrk.cache-break-warning.v1";

#[derive(Default)]
struct CacheBreakAcknowledged(std::collections::HashSet<String>);

impl App {
    /// The store. Read freely; after writing call [`App::ui_store_changed`], or use [`App::ui_store_update`].
    pub fn ui_store(&mut self) -> &mut UiStore {
        if !self.local::<UiStore>().loaded {
            let store = match UiStore::data_file(self) {
                Some(path) => UiStore::load_from(path),
                None => UiStore { loaded: true, ..UiStore::default() },
            };
            *self.local::<UiStore>() = store;
        }
        self.local::<UiStore>()
    }

    /// Writes to the store and schedules the debounced save.
    pub fn ui_store_update<R>(&mut self, f: impl FnOnce(&mut UiStore) -> R) -> R {
        let result = f(self.ui_store());
        self.ui_store_changed();
        result
    }

    /// Schedules the debounced save, when something has changed.
    pub fn ui_store_changed(&mut self) {
        if !self.ui_store().dirty {
            return;
        }
        if let Some(timer) = self.local::<SaveTimer>().0.take() {
            self.clear_timer(timer);
        }
        let timer = self.set_timer(SAVE_DEBOUNCE, |app| {
            app.local::<SaveTimer>().0 = None;
            app.flush_ui_store();
        });
        self.local::<SaveTimer>().0 = Some(timer);
    }

    /// `sidebarWidth`.
    pub fn sidebar_width(&mut self) -> f64 {
        load_sidebar_width(self.ui_store())
    }

    /// `updateSidebarWidth`: clamps and remembers it, and answers what it became.
    pub fn update_sidebar_width(&mut self, width: f64) -> f64 {
        let next = clamp_sidebar_width(width);
        self.ui_store_update(|store| store.set(SIDEBAR_WIDTH_KEY, next.to_string()));
        crate::prelude::repaint();
        next
    }

    /// `useCatalogOrder`'s reorder: rewrites the stored order from the whole visible list, so it re-anchors to what the
    /// catalog actually holds instead of accumulating ids of things that are long gone.
    pub fn write_catalog_order(&mut self, scope: CatalogOrderScope, ids: &[String]) {
        self.ui_store_update(|store| write_catalog_order(store, scope, ids));
    }

    /// `shouldWarnCacheBreak`: whether to warn before a settings change that would throw a warm prompt cache away. Said
    /// once per conversation, for this session of the app only: the cache it is about doesn't outlive the session by much.
    pub fn should_warn_cache_break(&mut self, conversation_id: &str) -> bool {
        !self.local::<CacheBreakAcknowledged>().0.contains(conversation_id) && self.ui_store().get(CACHE_BREAK_WARNING_KEY) != Some("off")
    }

    /// `acknowledgeCacheBreak`: the user went ahead: no second warning in this conversation, and none anywhere if `never`.
    pub fn acknowledge_cache_break(&mut self, conversation_id: &str, never: bool) {
        self.local::<CacheBreakAcknowledged>().0.insert(conversation_id.to_owned());
        if never {
            self.ui_store_update(|store| store.set(CACHE_BREAK_WARNING_KEY, "off"));
        }
    }

    /// Writes the store now. Quitting calls this, as a pending debounce wouldn't get to run.
    pub fn flush_ui_store(&mut self) {
        if let Err(error) = self.ui_store().flush() {
            eprintln!("{FILE_NAME}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UiStore {
        UiStore { loaded: true, ..UiStore::default() }
    }

    #[test]
    fn behaves_as_local_storage_and_reads_stale_values_as_their_fallback() {
        let mut store = store();
        assert_eq!(store.get("k"), None);
        store.set("k", "v");
        assert_eq!(store.get("k"), Some("v"));
        store.remove("k");
        assert_eq!(store.get("k"), None);
        assert!(store.read_flag("f", true));
        store.write_flag("f", false);
        assert!(!store.read_flag("f", true));
        store.write_choice("c", "stale");
        assert_eq!(store.read_choice("c", &["a", "b"], "a"), "a");
        store.write_choice("c", "b");
        assert_eq!(store.read_choice("c", &["a", "b"], "a"), "b");
        store.set("j", "not json");
        assert_eq!(store.get_json::<Vec<u32>>("j"), None);
        store.set_json("j", &vec![1u32, 2]);
        assert_eq!(store.get_json::<Vec<u32>>("j"), Some(vec![1, 2]));
    }

    #[test]
    fn writes_atomically_and_reads_back_only_when_something_changed() {
        let dir = std::env::temp_dir().join(format!("mewrk-ui-store-{}-{}", std::process::id(), crate::doc::create_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let mut first = UiStore::load_from(path.clone());
        first.set("a", "1");
        first.flush().unwrap();
        assert!(!dir.join("ui-state.json.tmp").exists());
        assert_eq!(UiStore::load_from(path.clone()).get("a"), Some("1"));
        // Nothing changed, so a file replaced meanwhile is left alone.
        std::fs::write(&path, "{\"a\":\"2\"}").unwrap();
        first.flush().unwrap();
        assert_eq!(UiStore::load_from(path.clone()).get("a"), Some("2"));
        // A file that isn't a map reads as empty, and the next write replaces it.
        std::fs::write(&path, "[]").unwrap();
        let mut broken = UiStore::load_from(path.clone());
        assert_eq!(broken.get("a"), None);
        broken.set("b", "3");
        broken.flush().unwrap();
        assert_eq!(UiStore::load_from(path).get("b"), Some("3"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reads_the_sidebar_width_from_the_legacy_key_and_clamps_it() {
        let mut store = store();
        assert_eq!(load_sidebar_width(&store), 264.0);
        store.set(LEGACY_SIDEBAR_WIDTH_KEY, "300");
        assert_eq!(load_sidebar_width(&store), 300.0);
        store.set(SIDEBAR_WIDTH_KEY, "1000");
        assert_eq!(load_sidebar_width(&store), 420.0);
        store.set(SIDEBAR_WIDTH_KEY, "-5");
        assert_eq!(load_sidebar_width(&store), 264.0);
        store.set(SIDEBAR_WIDTH_KEY, "wide");
        assert_eq!(load_sidebar_width(&store), 264.0);
        assert_eq!(clamp_sidebar_width(100.0), 220.0);
    }

    #[test]
    fn orders_a_catalog_with_new_rows_last_in_catalog_order() {
        let mut store = store();
        assert!(read_catalog_order(&store, CatalogOrderScope::Skills).is_empty());
        write_catalog_order(&mut store, CatalogOrderScope::Skills, &["c".to_owned(), "gone".to_owned(), "a".to_owned()]);
        write_catalog_order(&mut store, CatalogOrderScope::Mcp, &["x".to_owned()]);
        let order = read_catalog_order(&store, CatalogOrderScope::Skills);
        assert_eq!(order, vec!["c", "gone", "a"]);
        let sorted = sort_by_catalog_order(vec!["a", "new1", "b", "c", "new2"], &["c".to_owned(), "gone".to_owned(), "a".to_owned(), "b".to_owned()], |item| item);
        assert_eq!(sorted, vec!["c", "a", "b", "new1", "new2"]);
        assert_eq!(sort_by_catalog_order(vec!["z", "y"], &[], |item| item), vec!["z", "y"]);
    }
}
