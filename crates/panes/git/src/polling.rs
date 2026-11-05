//! Keeping every checkout's snapshot fresh, each on its own loop: upstream's `lib/gitPolling.ts`.
//!
//! Checkouts are independent: one on a slow or unreachable machine never holds up, cancels or delays the others. Each has
//! at most one read in flight; a tick that comes while one is out asks once more when it lands rather than piling
//! requests on a slow link. Returning to the window reads every checkout at once and forgives their failures, since that
//! is exactly when a machine that was away may be back.

use std::collections::HashMap;
use std::time::Duration;

use mewrk_client::prelude::*;
use mewrk_client::timers::TimerId;

use crate::git::{git_target_key, GitTarget};

/// One checkout the app keeps a Git snapshot of: a project workspace of the open conversation, as it sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPollSurface {
    /// The snapshot's cache key: a `git_snapshot_key`.
    pub key: String,
    /// The `git_surface_key` the snapshot is stored under.
    pub surface_key: String,
    pub target: GitTarget,
    /// The checkout is on screen and is polled at the fast cadence. The rest only keep their tab labels honest.
    pub foreground: bool,
}

/// Cadence for a checkout on screen.
pub const GIT_POLL_FOREGROUND: Duration = Duration::from_millis(4_000);
/// Cadence for the conversation's other checkouts.
pub const GIT_POLL_BACKGROUND: Duration = Duration::from_millis(15_000);
/// The longest a failing checkout waits between attempts.
pub const GIT_POLL_MAX_BACKOFF: Duration = Duration::from_millis(60_000);

/// How long to wait before polling a checkout again. A checkout that keeps failing backs off exponentially from its
/// cadence, so an unreachable machine is asked about once a minute while every other workspace carries on at full speed.
pub fn next_git_poll_delay(foreground: bool, failures: u32) -> Duration {
    let cadence = if foreground { GIT_POLL_FOREGROUND } else { GIT_POLL_BACKGROUND };
    if failures == 0 {
        return cadence;
    }
    GIT_POLL_MAX_BACKOFF.min(cadence * 2u32.pow(failures.min(8)))
}

/// Reads a checkout; calls `done` with false when the read failed.
pub type GitPollRefresh = Rc<dyn Fn(&mut App, &GitPollSurface, Box<dyn FnOnce(&mut App, bool) + Send>)>;

#[derive(Default)]
struct Loop {
    surface: Option<GitPollSurface>,
    in_flight: bool,
    queued: bool,
    failures: u32,
    timer: Option<TimerId>,
    /// Which generation of the set of loops this belongs to. A timer from an older one does nothing.
    generation: u64,
}

#[derive(Default)]
struct Polling {
    signature: String,
    generation: u64,
    loops: HashMap<String, Loop>,
    refresh: Option<GitPollRefresh>,
    enabled: bool,
    /// The window is not on screen: `document.visibilityState === "hidden"`. A tick only re-arms itself.
    hidden: bool,
}

fn signature_of(surfaces: &[GitPollSurface]) -> String {
    surfaces.iter().map(|s| format!("{}\0{}\0{}\0{}", s.key, s.surface_key, git_target_key(&s.target), s.foreground)).collect::<Vec<_>>().join("\u{1}")
}

/// Keeps every surface's snapshot fresh. Call it from any view that runs every frame: the loops start, restart and stop
/// as `surfaces` and `enabled` change. `refresh` is read fresh each time, so a new closure does not restart anything.
pub fn git_surface_polling(app: &mut App, surfaces: &[GitPollSurface], refresh: GitPollRefresh, enabled: bool) {
    let signature = signature_of(surfaces);
    let polling = app.local::<Polling>();
    polling.refresh = Some(refresh);
    if polling.signature == signature && polling.enabled == enabled {
        return;
    }
    polling.signature = signature;
    polling.enabled = enabled;
    polling.generation += 1;
    let generation = polling.generation;
    let old: Vec<TimerId> = polling.loops.drain().filter_map(|(_, l)| l.timer).collect();
    if enabled {
        for surface in surfaces {
            polling.loops.insert(surface.key.clone(), Loop { surface: Some(surface.clone()), generation, ..Loop::default() });
        }
    }
    for timer in old {
        app.clear_timer(timer);
    }
    let keys: Vec<String> = app.local::<Polling>().loops.keys().cloned().collect();
    for key in keys {
        tick(app, &key, generation);
    }
}

/// Whether the window is on screen. Coming back reads every checkout at once, as returning to it does upstream.
pub fn git_polling_set_visible(app: &mut App, visible: bool) {
    let polling = app.local::<Polling>();
    let was_hidden = std::mem::replace(&mut polling.hidden, !visible);
    if visible && was_hidden {
        git_polling_wake(app);
    }
}

/// The window came back: reads every checkout at once and forgives their failures.
pub fn git_polling_wake(app: &mut App) {
    let polling = app.local::<Polling>();
    if polling.hidden {
        return;
    }
    let generation = polling.generation;
    let keys: Vec<String> = polling.loops.keys().cloned().collect();
    for key in keys {
        if let Some(l) = app.local::<Polling>().loops.get_mut(&key) {
            l.failures = 0;
        }
        tick(app, &key, generation);
    }
}

fn schedule(app: &mut App, key: &str, generation: u64) {
    let Some(l) = app.local::<Polling>().loops.get_mut(key) else { return };
    if l.generation != generation {
        return;
    }
    let delay = next_git_poll_delay(l.surface.as_ref().is_some_and(|s| s.foreground), l.failures);
    if let Some(old) = l.timer.take() {
        app.clear_timer(old);
    }
    let owned = key.to_owned();
    let timer = app.set_timer(delay, move |app| tick(app, &owned, generation));
    if let Some(l) = app.local::<Polling>().loops.get_mut(key) {
        l.timer = Some(timer);
    }
}

fn tick(app: &mut App, key: &str, generation: u64) {
    let polling = app.local::<Polling>();
    let refresh = polling.refresh.clone();
    let hidden = polling.hidden;
    let Some(l) = polling.loops.get_mut(key) else { return };
    if l.generation != generation {
        return;
    }
    if hidden {
        schedule(app, key, generation);
        return;
    }
    if l.in_flight {
        l.queued = true;
        return;
    }
    l.in_flight = true;
    let (Some(surface), Some(refresh)) = (l.surface.clone(), refresh) else { return };
    let owned = key.to_owned();
    refresh(app, &surface, Box::new(move |app, ok| {
        let Some(l) = app.local::<Polling>().loops.get_mut(&owned) else { return };
        if l.generation != generation {
            return;
        }
        l.failures = if ok { 0 } else { l.failures + 1 };
        l.in_flight = false;
        if std::mem::take(&mut l.queued) {
            tick(app, &owned, generation);
        } else {
            schedule(app, &owned, generation);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_what_is_on_screen_fast_the_rest_slower_and_backs_a_failing_checkout_off() {
        assert_eq!(next_git_poll_delay(true, 0), Duration::from_millis(4_000));
        assert_eq!(next_git_poll_delay(false, 0), Duration::from_millis(15_000));
        assert_eq!(next_git_poll_delay(true, 1), Duration::from_millis(8_000));
        assert_eq!(next_git_poll_delay(true, 2), Duration::from_millis(16_000));
        assert_eq!(next_git_poll_delay(true, 100), GIT_POLL_MAX_BACKOFF);
        assert_eq!(next_git_poll_delay(false, 3), GIT_POLL_MAX_BACKOFF);
    }
}
