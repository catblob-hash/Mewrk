//! Who is presented and what was last asked of each page: upstream's `lib/browserController.ts` and
//! `lib/browserLifecycleIntent.ts`.
//!
//! Every native lifecycle transition claims a fresh epoch, and each asynchronous continuation re-checks
//! that its epoch is still the newest for the session before committing anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::browser::BrowserStatus;

const EPOCHS_PER_MILLISECOND: u64 = 1024;
const MAX_EPOCH: u64 = 9_007_199_254_740_991;

static LAST_ISSUED: AtomicU64 = AtomicU64::new(0);

/// A monotonic epoch from the clock. Upstream also persists the last one, for a renderer that reloads
/// under a host that remembers: here they live and die in one process.
pub fn issue_browser_lifecycle_intent_epoch() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_millis() as u64);
    issue_at(now)
}

fn issue_at(now_ms: u64) -> u64 {
    let clock = (now_ms.min(MAX_EPOCH / EPOCHS_PER_MILLISECOND) * EPOCHS_PER_MILLISECOND).max(1);
    let mut previous = LAST_ISSUED.load(Ordering::SeqCst);
    loop {
        let next = clock.max(previous + 1).min(MAX_EPOCH);
        match LAST_ISSUED.compare_exchange(previous, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(actual) => previous = actual,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserIntentDesired {
    Open,
    Hidden,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserIntent {
    pub epoch: u64,
    pub desired: BrowserIntentDesired,
}

/// Lives in `app.local::<BrowserController>()`.
#[derive(Default)]
pub struct BrowserController {
    /// Native browser status per session id: one entry per open browser tab.
    pub statuses: HashMap<String, BrowserStatus>,
    /// Whether the presented native surface finished opening.
    pub runtime_ready: bool,
    /// The remote screen each tab is showing, by viewer address. A screen has no page, so the host's status can't say.
    screens: HashMap<String, String>,
    visible_session: Option<String>,
    intents: HashMap<String, BrowserIntent>,
    closing: std::collections::HashSet<String>,
    open_calls: HashMap<String, usize>,
}

impl BrowserController {
    /// The address a tab is showing: its remote screen, or else its page. `None`: nothing has been heard of it.
    pub fn page_url(&self, session_id: &str) -> Option<&str> {
        self.screens.get(session_id).or_else(|| self.statuses.get(session_id).map(|status| &status.url)).map(String::as_str)
    }

    /// Whether a session is showing nothing. Such a session is the pane's start page, which takes no tab.
    pub fn page_is_blank(&self, session_id: &str) -> bool {
        self.page_url(session_id).is_none_or(|url| url.trim().is_empty() || url == "about:blank")
    }

    pub fn screen_of(&self, session_id: &str) -> Option<&str> {
        self.screens.get(session_id).map(String::as_str)
    }

    pub fn set_screen(&mut self, session_id: &str, url: Option<String>) {
        match url {
            Some(url) => {
                self.screens.insert(session_id.to_owned(), url);
            }
            None => {
                self.screens.remove(session_id);
            }
        }
    }

    /// The page is gone: nothing said about it holds any more.
    pub fn forget_page(&mut self, session_id: &str) {
        self.statuses.remove(session_id);
        self.screens.remove(session_id);
    }

    pub fn visible_session(&self) -> Option<&str> {
        self.visible_session.as_deref()
    }

    pub fn set_visible_session(&mut self, session_id: Option<&str>) {
        self.visible_session = session_id.map(str::to_owned);
    }

    /// Clears the presented session only if it is the given one.
    pub fn clear_visible_session_if(&mut self, session_id: &str) {
        if self.visible_session.as_deref() == Some(session_id) {
            self.visible_session = None;
        }
    }

    /// Clears and returns the presented session in one step (the hide flow).
    pub fn take_visible_session(&mut self) -> Option<String> {
        self.visible_session.take()
    }

    pub fn issue_intent(&mut self, session_id: &str, desired: BrowserIntentDesired) -> u64 {
        let epoch = issue_browser_lifecycle_intent_epoch();
        self.intents.insert(session_id.to_owned(), BrowserIntent { epoch, desired });
        epoch
    }

    pub fn intent_is_current(&self, session_id: &str, epoch: u64, desired: BrowserIntentDesired) -> bool {
        self.intents.get(session_id).is_some_and(|intent| intent.epoch == epoch && intent.desired == desired)
    }

    pub fn current_intent(&self, session_id: &str) -> Option<BrowserIntent> {
        self.intents.get(session_id).copied()
    }

    /// Rolls back to the pre-close intent after a rejected close disposition.
    pub fn restore_intent(&mut self, session_id: &str, previous: Option<BrowserIntent>) {
        match previous {
            Some(previous) => {
                self.intents.insert(session_id.to_owned(), previous);
            }
            None => {
                self.intents.remove(session_id);
            }
        }
    }

    pub fn close_in_flight(&self, session_id: &str) -> bool {
        self.closing.contains(session_id)
    }

    /// Whether this call starts the close. A later caller shares the first task, untouched.
    pub fn begin_close(&mut self, session_id: &str) -> bool {
        self.closing.insert(session_id.to_owned())
    }

    pub fn end_close(&mut self, session_id: &str) {
        self.closing.remove(session_id);
    }

    /// Tracks an open call so a structured close can fence stragglers.
    pub fn open_started(&mut self, session_id: &str) {
        *self.open_calls.entry(session_id.to_owned()).or_default() += 1;
    }

    pub fn open_settled(&mut self, session_id: &str) {
        if let Some(count) = self.open_calls.get_mut(session_id) {
            *count -= 1;
            if *count == 0 {
                self.open_calls.remove(session_id);
            }
        }
    }

    pub fn pending_opens(&self, session_id: &str) -> usize {
        self.open_calls.get(session_id).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BrowserIntentDesired::*;

    #[test]
    fn epochs_only_go_up_even_when_the_clock_goes_back() {
        let first = issue_at(10_000_000);
        let second = issue_at(10_000_000);
        let third = issue_at(1);
        assert!(second > first && third > second);
    }

    #[test]
    fn only_the_newest_intent_is_current() {
        let mut controller = BrowserController::default();
        let older = controller.issue_intent("a", Open);
        assert!(controller.intent_is_current("a", older, Open));
        let newer = controller.issue_intent("a", Closed);
        assert!(!controller.intent_is_current("a", older, Open));
        assert!(controller.intent_is_current("a", newer, Closed));
        assert!(!controller.intent_is_current("a", newer, Open));
        assert!(!controller.intent_is_current("b", newer, Closed));
    }

    #[test]
    fn a_rejected_close_rolls_back() {
        let mut controller = BrowserController::default();
        let open = controller.issue_intent("a", Open);
        let previous = controller.current_intent("a");
        controller.issue_intent("a", Closed);
        controller.restore_intent("a", previous);
        assert!(controller.intent_is_current("a", open, Open));
        controller.restore_intent("a", None);
        assert_eq!(controller.current_intent("a"), None);
    }

    #[test]
    fn the_presented_session_is_taken_once_and_cleared_only_by_name() {
        let mut controller = BrowserController::default();
        controller.set_visible_session(Some("a"));
        controller.clear_visible_session_if("b");
        assert_eq!(controller.visible_session(), Some("a"));
        assert_eq!(controller.take_visible_session().as_deref(), Some("a"));
        assert_eq!(controller.take_visible_session(), None);
    }

    #[test]
    fn a_session_is_blank_until_it_shows_a_page_or_a_screen() {
        let mut controller = BrowserController::default();
        assert!(controller.page_is_blank("a"), "nothing heard of it yet");
        controller.statuses.insert("a".to_owned(), BrowserStatus::default());
        assert!(controller.page_is_blank("a"));
        controller.statuses.get_mut("a").unwrap().url = "http://localhost:5173/".to_owned();
        assert!(!controller.page_is_blank("a"));
        controller.statuses.get_mut("a").unwrap().url = "about:blank".to_owned();
        controller.set_screen("a", Some("http://127.0.0.1:9/token/".to_owned()));
        assert!(!controller.page_is_blank("a"), "a screen has no page and is still something to show");
        assert_eq!(controller.page_url("a"), Some("http://127.0.0.1:9/token/"));
        controller.forget_page("a");
        assert!(controller.page_is_blank("a"));
        assert_eq!(controller.page_url("a"), None);
    }

    #[test]
    fn closes_are_deduplicated_per_session() {
        let mut controller = BrowserController::default();
        assert!(controller.begin_close("a"));
        assert!(!controller.begin_close("a"));
        assert!(controller.begin_close("b"));
        assert!(controller.close_in_flight("a"));
        controller.end_close("a");
        assert!(!controller.close_in_flight("a"));
        assert!(controller.begin_close("a"));
    }

    #[test]
    fn open_calls_are_counted_until_they_settle() {
        let mut controller = BrowserController::default();
        controller.open_started("a");
        controller.open_started("a");
        assert_eq!(controller.pending_opens("a"), 2);
        controller.open_settled("a");
        controller.open_settled("a");
        controller.open_settled("a");
        assert_eq!(controller.pending_opens("a"), 0);
    }
}
