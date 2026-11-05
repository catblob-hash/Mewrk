//! `setTimeout` for the UI thread: closures that run through the mailbox after a delay and can be called off.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::App;

type Job = Box<dyn FnOnce(&mut App) + Send>;

/// A timer that has been set. `clear_timer` with it is `clearTimeout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId(u64);

struct Entry(Instant, u64, Job);

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        (self.0, self.1) == (other.0, other.1)
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.0, self.1).cmp(&(other.0, other.1))
    }
}

#[derive(Default)]
struct Timers {
    next: u64,
    // Cancellation is decided on the UI thread, so a timer already handed to the mailbox still doesn't run once cleared.
    live: HashSet<u64>,
    tx: Option<mpsc::Sender<Entry>>,
}

impl App {
    /// `setTimeout(f, delay)`.
    pub fn set_timer(&mut self, delay: Duration, f: impl FnOnce(&mut App) + Send + 'static) -> TimerId {
        let task = self.task();
        let timers = self.local::<Timers>();
        let tx = timers.tx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel::<Entry>();
            // One thread for all timers: a debounce re-armed on every change must not cost a thread each time.
            std::thread::Builder::new()
                .name("mewrk-timers".to_owned())
                .spawn(move || {
                    let mut heap: BinaryHeap<Reverse<Entry>> = BinaryHeap::new();
                    loop {
                        let wait = heap.peek().map(|Reverse(e)| e.0.saturating_duration_since(Instant::now()));
                        let received = match wait {
                            Some(wait) => rx.recv_timeout(wait),
                            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        };
                        match received {
                            Ok(entry) => heap.push(Reverse(entry)),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        }
                        let now = Instant::now();
                        while heap.peek().is_some_and(|Reverse(e)| e.0 <= now) {
                            let Some(Reverse(Entry(_, id, job))) = heap.pop() else { break };
                            task.post(move |app| {
                                if app.local::<Timers>().live.remove(&id) {
                                    job(app);
                                }
                            });
                        }
                    }
                })
                .expect("thread");
            tx
        });
        let tx = tx.clone();
        timers.next += 1;
        let id = timers.next;
        timers.live.insert(id);
        let _ = tx.send(Entry(Instant::now() + delay, id, Box::new(f)));
        TimerId(id)
    }

    /// `clearTimeout(id)`.
    pub fn clear_timer(&mut self, id: TimerId) {
        self.local::<Timers>().live.remove(&id.0);
    }
}
