//! Connects the runtime's tasks to the UI thread.
//!
//! Tokens arrive far more often than frames. Each session has an inbox that merges adjacent text, so a
//! backlog cannot form: however late a frame is, it finds one chunk of text, not thousands of messages. The
//! window is woken at most once per frame, and only sessions that changed get drained.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rt::{ConnTiming, Event, FailKind, SessionId, Sink, Timings, Usage};
use parking_lot::Mutex;

pub struct Notifier {
    dirty: Mutex<Vec<u32>>,
    wake_pending: AtomicBool,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Notifier {
    pub fn new(wake: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { dirty: Mutex::new(Vec::with_capacity(256)), wake_pending: AtomicBool::new(false), wake })
    }

    fn mark(&self, id: u32) {
        self.dirty.lock().push(id);
        self.poke();
    }

    /// Wakes the UI, coalesced: nothing more goes through until the next frame takes the list.
    pub fn poke(&self) {
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    pub fn take_dirty(&self, out: &mut Vec<u32>) {
        self.wake_pending.store(false, Ordering::Release);
        std::mem::swap(&mut *self.dirty.lock(), out);
    }
}

/// What a session did, in order. Text is stored as ranges into a shared arena.
#[derive(Debug)]
#[allow(dead_code)] // Some fields are only there to be seen in a debugger or a future panel.
pub enum Op {
    Queued,
    Attempt { attempt: u32, resumed_bytes: usize },
    Connected(ConnTiming),
    Progress { processed: u32, total: u32 },
    /// Time from the run's first attempt to its first token. Measured where the token arrives, so that it
    /// says something about the network and the server, and nothing about when a frame happened to run.
    FirstToken { us: u64 },
    Reasoning(u32, u32),
    Text(u32, u32),
    ToolStart { index: u32, name: String },
    ToolArgs { index: u32, range: (u32, u32) },
    ToolRewind,
    ToolResult { name: String, output: String, is_error: bool, took: Duration },
    Retry { attempt: u32, kind: FailKind, detail: String, delay: Duration },
    TurnEnd { finish: String, usage: Option<Usage>, timings: Option<Timings> },
    Finished { error: Option<(FailKind, String)>, attempts: u32, resumes: u32 },
}

/// Log-scale histogram of microseconds, at quarter-octave resolution. Fixed size and no allocation, since it
/// is updated for every token.
#[derive(Clone)]
pub struct LogHist {
    buckets: [u32; 128],
    pub count: u64,
    pub max_us: u64,
}

impl Default for LogHist {
    fn default() -> Self {
        Self { buckets: [0; 128], count: 0, max_us: 0 }
    }
}

impl LogHist {
    #[inline]
    pub fn record(&mut self, us: u64) {
        let i = bucket_of(us);
        self.buckets[i] += 1;
        self.count += 1;
        self.max_us = self.max_us.max(us);
    }

    pub fn merge(&mut self, o: &LogHist) {
        for (a, b) in self.buckets.iter_mut().zip(&o.buckets) {
            *a += *b;
        }
        self.count += o.count;
        self.max_us = self.max_us.max(o.max_us);
    }

    pub fn quantile_us(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let want = (self.count as f64 * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, &n) in self.buckets.iter().enumerate() {
            seen += n as u64;
            if seen >= want {
                return value_of(i).min(self.max_us);
            }
        }
        self.max_us
    }
}

fn bucket_of(us: u64) -> usize {
    if us < 2 {
        return 0;
    }
    let log2 = 63 - us.leading_zeros() as usize;
    let frac = if log2 >= 2 { ((us >> (log2 - 2)) & 3) as usize } else { 0 };
    (log2 * 4 + frac).min(127)
}

fn value_of(bucket: usize) -> u64 {
    let (log2, frac) = (bucket / 4, bucket % 4);
    if log2 < 2 {
        return 1 << log2;
    }
    (1u64 << log2) + ((frac as u64 + 1) << (log2 - 2))
}

#[derive(Default)]
pub struct InboxData {
    pub ops: Vec<Op>,
    pub arena: String,
    pub tokens: u32,
    pub gaps: LogHist,
    last_token: Option<Instant>,
    run_started: Option<Instant>,
    first_token_seen: bool,
}

pub struct Inbox {
    pub id: u32,
    data: Mutex<InboxData>,
    flagged: AtomicBool,
    notifier: Arc<Notifier>,
    /// Tiles only show their tail, so an undrawn backlog can safely be cut. A chat never loses text.
    tail_only: bool,
}

const TAIL_CAP: usize = 8 << 10;

impl Inbox {
    pub fn new(id: u32, notifier: Arc<Notifier>, tail_only: bool) -> Arc<Self> {
        Arc::new(Self { id, data: Mutex::new(InboxData::default()), flagged: AtomicBool::new(false), notifier, tail_only })
    }

    /// Swaps the pending data into `out`, whose (empty) buffers are recycled.
    pub fn drain(&self, out: &mut InboxData) {
        out.ops.clear();
        out.arena.clear();
        out.tokens = 0;
        out.gaps = LogHist::default();
        self.flagged.store(false, Ordering::Release);
        let mut d = self.data.lock();
        std::mem::swap(&mut d.ops, &mut out.ops);
        std::mem::swap(&mut d.arena, &mut out.arena);
        out.tokens = std::mem::take(&mut d.tokens);
        std::mem::swap(&mut d.gaps, &mut out.gaps);
    }

    fn with(&self, f: impl FnOnce(&mut InboxData)) {
        {
            let mut d = self.data.lock();
            f(&mut d);
        }
        if !self.flagged.swap(true, Ordering::AcqRel) {
            self.notifier.mark(self.id);
        }
    }

    pub fn finished(&self, error: Option<(FailKind, String)>, attempts: u32, resumes: u32) {
        self.with(|d| d.ops.push(Op::Finished { error, attempts, resumes }));
    }

    fn token(&self, text: &str, reasoning: bool) {
        let tail_only = self.tail_only;
        self.with(|d| {
            let now = Instant::now();
            if let Some(last) = d.last_token.replace(now) {
                d.gaps.record(now.duration_since(last).as_micros() as u64);
            }
            if !d.first_token_seen {
                d.first_token_seen = true;
                if let Some(started) = d.run_started {
                    d.ops.push(Op::FirstToken { us: now.duration_since(started).as_micros() as u64 });
                }
            }
            d.tokens += 1;
            let start = d.arena.len() as u32;
            d.arena.push_str(text);
            let end = d.arena.len() as u32;
            match d.ops.last_mut() {
                Some(Op::Text(_, e)) if !reasoning => *e = end,
                Some(Op::Reasoning(_, e)) if reasoning => *e = end,
                _ => d.ops.push(if reasoning { Op::Reasoning(start, end) } else { Op::Text(start, end) }),
            }
            if tail_only && d.arena.len() > TAIL_CAP {
                compact_tail(d);
            }
        });
    }
}

/// Keeps only the last few KiB of an undrawn backlog: the frame that finally comes can only show the tail.
fn compact_tail(d: &mut InboxData) {
    let keep_from = d.arena.len() - TAIL_CAP / 2;
    let cut = (keep_from..d.arena.len()).find(|&i| d.arena.is_char_boundary(i)).unwrap_or(d.arena.len());
    d.arena.drain(..cut);
    let cut = cut as u32;
    let shift = |v: u32| v.saturating_sub(cut);
    d.ops.retain_mut(|op| match op {
        Op::Text(a, b) | Op::Reasoning(a, b) => {
            *a = shift(*a);
            *b = shift(*b);
            *a < *b
        }
        Op::ToolArgs { range: (a, b), .. } => {
            *a = shift(*a);
            *b = shift(*b);
            *a < *b
        }
        _ => true,
    });
}

impl Sink for Inbox {
    fn event(&self, _: SessionId, event: Event<'_>) {
        match event {
            Event::Text(t) => self.token(t, false),
            Event::Reasoning(t) => self.token(t, true),
            Event::Queued => self.with(|d| {
                d.run_started = None;
                d.first_token_seen = false;
                d.last_token = None;
                d.ops.push(Op::Queued);
            }),
            Event::Attempt { attempt, resumed_bytes, .. } => self.with(|d| {
                // From the first attempt, not from queueing: time spent waiting for a slot is our own choice.
                d.run_started.get_or_insert_with(Instant::now);
                d.ops.push(Op::Attempt { attempt, resumed_bytes });
            }),
            Event::Connected(t) => self.with(|d| d.ops.push(Op::Connected(t))),
            Event::Headers { .. } => {}
            Event::Progress { processed, total } => self.with(|d| d.ops.push(Op::Progress { processed, total })),
            Event::ToolStart { index, name, .. } => {
                let name = name.to_owned();
                self.with(|d| d.ops.push(Op::ToolStart { index, name }));
            }
            Event::ToolArgs { index, delta } => self.with(|d| {
                let start = d.arena.len() as u32;
                d.arena.push_str(delta);
                d.ops.push(Op::ToolArgs { index, range: (start, d.arena.len() as u32) });
            }),
            Event::ToolRewind => self.with(|d| d.ops.push(Op::ToolRewind)),
            Event::ToolResult { name, output, is_error, took, .. } => {
                let (name, output) = (name.to_owned(), output.to_owned());
                self.with(|d| d.ops.push(Op::ToolResult { name, output, is_error, took }));
            }
            Event::Retry { attempt, kind, detail, delay } => {
                let detail = detail.to_owned();
                self.with(|d| {
                    // The stream's silence during an outage says nothing about token pacing.
                    d.last_token = None;
                    d.ops.push(Op::Retry { attempt, kind, detail, delay });
                });
            }
            Event::TurnEnd { finish, usage, timings } => {
                let finish = finish.to_owned();
                self.with(|d| d.ops.push(Op::TurnEnd { finish, usage, timings }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counting_notifier() -> (Arc<Notifier>, Arc<std::sync::atomic::AtomicU32>) {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let c = count.clone();
        (Notifier::new(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        })), count)
    }

    #[test]
    fn a_burst_of_tokens_wakes_the_window_once_and_merges_into_one_op() {
        let (notifier, wakes) = counting_notifier();
        let inbox = Inbox::new(7, notifier.clone(), false);
        for t in ["a", "b", "c"] {
            inbox.event(0, Event::Text(t));
        }
        assert_eq!(wakes.load(Ordering::SeqCst), 1);
        let mut dirty = Vec::new();
        notifier.take_dirty(&mut dirty);
        assert_eq!(dirty, vec![7]);
        let mut out = InboxData::default();
        inbox.drain(&mut out);
        assert_eq!(out.ops.len(), 1);
        assert_eq!(out.arena, "abc");
        assert_eq!(out.tokens, 3);
        inbox.event(0, Event::Text("d"));
        assert_eq!(wakes.load(Ordering::SeqCst), 2, "a new frame's worth of work wakes again");
    }

    #[test]
    fn reasoning_and_text_stay_in_order_and_apart() {
        let (notifier, _) = counting_notifier();
        let inbox = Inbox::new(1, notifier, false);
        inbox.event(0, Event::Reasoning("think"));
        inbox.event(0, Event::Reasoning("ing"));
        inbox.event(0, Event::Text("ans"));
        let mut out = InboxData::default();
        inbox.drain(&mut out);
        assert!(matches!(out.ops[0], Op::Reasoning(0, 8)));
        assert!(matches!(out.ops[1], Op::Text(8, 11)));
    }

    #[test]
    fn first_token_time_is_taken_where_the_token_arrives() {
        let (notifier, _) = counting_notifier();
        let inbox = Inbox::new(1, notifier, false);
        inbox.event(0, Event::Queued);
        inbox.event(0, Event::Attempt { step: 1, attempt: 1, resumed_bytes: 0 });
        std::thread::sleep(Duration::from_millis(20));
        inbox.event(0, Event::Text("a"));
        inbox.event(0, Event::Text("b"));
        let mut out = InboxData::default();
        inbox.drain(&mut out);
        let first: Vec<u64> = out.ops.iter().filter_map(|op| if let Op::FirstToken { us } = op { Some(*us) } else { None }).collect();
        assert_eq!(first.len(), 1, "exactly one per run");
        assert!(first[0] >= 20_000 && first[0] < 2_000_000, "{}", first[0]);
    }

    #[test]
    fn a_tile_backlog_stays_bounded_and_keeps_the_tail() {
        let (notifier, _) = counting_notifier();
        let inbox = Inbox::new(1, notifier, true);
        for i in 0..100_000 {
            inbox.event(0, Event::Text(if i % 2 == 0 { "中文" } else { "word " }));
        }
        inbox.event(0, Event::Text("THE END"));
        let mut out = InboxData::default();
        inbox.drain(&mut out);
        assert!(out.arena.len() <= TAIL_CAP + 16, "{}", out.arena.len());
        assert!(out.arena.ends_with("THE END"));
        assert_eq!(out.tokens, 100_001);
        let Op::Text(a, b) = out.ops.last().unwrap() else { panic!() };
        assert_eq!(&out.arena[*a as usize..*b as usize], out.arena.as_str());
    }

    #[test]
    fn a_chat_never_drops_text() {
        let (notifier, _) = counting_notifier();
        let inbox = Inbox::new(1, notifier, false);
        for _ in 0..10_000 {
            inbox.event(0, Event::Text("12345"));
        }
        let mut out = InboxData::default();
        inbox.drain(&mut out);
        assert_eq!(out.arena.len(), 50_000);
    }

    #[test]
    fn histogram_quantiles_land_within_a_quarter_octave() {
        let mut h = LogHist::default();
        for us in 1..=10_000u64 {
            h.record(us);
        }
        for (q, exact) in [(0.5, 5000.0), (0.9, 9000.0), (0.99, 9900.0)] {
            let v = h.quantile_us(q) as f64;
            assert!(v >= exact * 0.8 && v <= exact * 1.25, "q{q}: {v}");
        }
        assert_eq!(h.quantile_us(1.0), 10_000);
        assert_eq!(LogHist::default().quantile_us(0.5), 0);
    }
}
