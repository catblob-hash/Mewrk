//! `useGitSurfacePolling`: each checkout on its own loop, so a slow one holds nobody up. `lib/gitPolling.test.ts`.

use std::sync::{Arc, Mutex, MutexGuard};

use mewrk_client::prelude::*;
use mewrk_client::{i18n, App, StartOptions};
use mewrk_pane_git::*;

static SERIAL: Mutex<()> = Mutex::new(());

fn start() -> (MutexGuard<'static, ()>, App) {
    let serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("mewrk-git-polling-{}-{n:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let app = App::start(StartOptions { identifier: "mewrk.test".into(), version: "0.0.0".into(), data_dir: dir, resource_dir: None, wake: Arc::new(|| {}), exit: Arc::new(|_| {}), show_window: Arc::new(|| {}) }).map_err(|e| format!("{e:?}")).unwrap();
    i18n::set_english(false);
    (serial, app)
}

fn surface(key: &str, foreground: bool) -> GitPollSurface {
    GitPollSurface { key: key.to_owned(), surface_key: "ws".to_owned(), target: git_workspace_target("ws", None), foreground }
}

type Done = Box<dyn FnOnce(&mut App, bool) + Send>;

#[derive(Default, Clone)]
struct Reads {
    calls: Rc<std::cell::RefCell<Vec<String>>>,
    waiting: Rc<std::cell::RefCell<Vec<(String, Done)>>>,
}

impl Reads {
    fn refresh(&self) -> GitPollRefresh {
        let me = self.clone();
        Rc::new(move |_, surface, done| {
            me.calls.borrow_mut().push(surface.key.clone());
            me.waiting.borrow_mut().push((surface.key.clone(), done));
        })
    }

    fn count(&self, key: &str) -> usize {
        self.calls.borrow().iter().filter(|k| *k == key).count()
    }

    fn finish(&self, app: &mut App, key: &str, ok: bool) {
        let at = self.waiting.borrow().iter().position(|(k, _)| k == key).expect("a read out");
        let (_, done) = self.waiting.borrow_mut().remove(at);
        done(app, ok);
    }
}

#[test]
fn keeps_each_checkout_on_its_own_loop_so_a_slow_one_holds_nobody_up() {
    let (_serial, mut app) = start();
    let reads = Reads::default();
    let surfaces = [surface("a", true), surface("b", false)];
    git_surface_polling(&mut app, &surfaces, reads.refresh(), true);
    assert_eq!((reads.count("a"), reads.count("b")), (1, 1), "both read at once");

    // The same set again starts nothing.
    git_surface_polling(&mut app, &surfaces, reads.refresh(), true);
    assert_eq!(reads.count("a"), 1);

    // A tick while a read is out asks once more when it lands, rather than piling requests on a slow link.
    git_polling_wake(&mut app);
    git_polling_wake(&mut app);
    assert_eq!(reads.count("a"), 1);
    reads.finish(&mut app, "a", true);
    assert_eq!((reads.count("a"), reads.count("b")), (2, 1), "only the one that landed");
    reads.finish(&mut app, "b", true);
    assert_eq!(reads.count("b"), 2);
}

#[test]
fn a_new_set_of_surfaces_starts_over_and_disabling_stops_it() {
    let (_serial, mut app) = start();
    let reads = Reads::default();
    git_surface_polling(&mut app, &[surface("a", true)], reads.refresh(), true);
    assert_eq!(reads.count("a"), 1);
    git_surface_polling(&mut app, &[surface("a", false)], reads.refresh(), true);
    assert_eq!(reads.count("a"), 2, "the cadence changed, which is another loop");

    git_surface_polling(&mut app, &[surface("a", false)], reads.refresh(), false);
    // What was out lands on nothing.
    reads.finish(&mut app, "a", true);
    reads.finish(&mut app, "a", true);
    git_polling_wake(&mut app);
    assert_eq!(reads.count("a"), 2, "stopped");
}

#[test]
fn a_hidden_window_reads_nothing_and_coming_back_reads_everything() {
    let (_serial, mut app) = start();
    let reads = Reads::default();
    git_surface_polling(&mut app, &[surface("a", true)], reads.refresh(), true);
    reads.finish(&mut app, "a", false);
    git_polling_set_visible(&mut app, false);
    git_polling_wake(&mut app);
    assert_eq!(reads.count("a"), 1);
    git_polling_set_visible(&mut app, true);
    assert_eq!(reads.count("a"), 2, "exactly when a machine that was away may be back");
}
