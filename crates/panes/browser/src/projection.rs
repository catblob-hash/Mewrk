//! The picture of the native page the pane paints in the page's own place while the page is down:
//! upstream's `lib/browserPageProjection.ts`, as a state machine the view steps once a frame.
//!
//! A native view paints above everything the GPU draws. The page is left up whenever nothing needs it
//! gone, and goes down, with a still in its place, only for as long as something has to be seen there.
//! Two orderings are the whole mechanism, and they aren't symmetric:
//!
//! - Sinking: publish a frame, let one frame render it, then sink. A native view can't be cross-faded.
//! - Raising: raise, await the host, wait one frame, then drop the still.
//!
//! Every failure path still sinks: a page left on top of an open dialog has no recovery, and a frozen
//! still or an empty box costs a moment of staleness and nothing else.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A decoded capture: what the still is drawn from, and what an unchanged page is recognised by.
#[derive(Clone)]
pub struct Frame {
    pub content_key: String,
    /// The base64 payload.
    pub data: Arc<str>,
    pub rgba: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({} {}x{})", self.content_key, self.width, self.height)
    }
}

/// A capture that came back one pixel wide is what an unpainted surface returns, not a frame.
const MIN_USABLE_CAPTURE_PX: u32 = 1;
const FRAME_INTERVAL: Duration = Duration::from_millis(1_000);
const FIRST_FRAME_DELAY: Duration = Duration::from_millis(300);
/// A preview nobody is touching gives byte-identical captures; recognising that takes an idle pane
/// from one capture a second to one every five.
const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(5_000);
const IDENTICAL_FRAMES_BEFORE_IDLE: u32 = 5;
const FRAME_BACKOFF: Duration = Duration::from_millis(4_000);
const FAILURES_BEFORE_BACKOFF: u32 = 4;

/// What the view knows this frame.
#[derive(Clone, Debug, Default)]
pub struct ProjectionInput {
    pub content_key: Option<String>,
    /// Whether there is a page session to speak for, not whether its page exists yet.
    pub enabled: bool,
    /// Whether the host is showing the page in the pane right now. Only a presented page is captured.
    pub presented: bool,
    /// Whether the pane is presenting the page rather than one of its own cards.
    pub has_content: bool,
    pub parked: bool,
    /// Whether a trusted surface is over the page, as opposed to the page merely being out of sight.
    pub covered: bool,
    /// What the host says it has done, from its status.
    pub host_parked: bool,
    pub host_covered: bool,
    pub box_width: f32,
    pub box_height: f32,
}

/// What the view has to do for the machine.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    SetParked { chain: u64, value: bool },
    SetCovered { chain: u64, value: bool },
    /// `browser_capture_page`. Answer with [`Projection::captured`].
    Capture { request: u64 },
}

#[derive(Debug)]
enum Step {
    Send(Command),
    /// Waits for the previous `Send`'s answer.
    AwaitAnswer,
    /// Lets one frame render what the last step published.
    AwaitFrame,
    /// Drops the still, unless nothing is standing in.
    DropStill,
    /// Releases the guard the transition holds against reconciling.
    Settled,
}

#[derive(Debug)]
struct Capturing {
    request: u64,
    /// Whether it fills the box of a sink that had no frame to publish.
    for_still: bool,
    chain: u64,
}

#[derive(Debug, Default)]
pub struct Projection {
    /// The last states the host was asked for. `None` is "never said": whatever the host holds was
    /// declared by some earlier pane, so the first decision is always sent.
    declared_parked: Option<bool>,
    declared_covered: Option<bool>,
    in_flight: u32,
    last_host_parked: bool,
    last_host_covered: bool,
    repair_owed: bool,
    settling: bool,
    chain: u64,
    steps: VecDeque<Step>,
    waiting_answer: bool,
    /// The newest frame, on screen or not: what makes covering instant.
    warm: Option<Frame>,
    /// What the pane paints in the page's place.
    still: Option<Frame>,
    /// The capture loop's clock.
    next_capture: Option<Instant>,
    identical: u32,
    failures: u32,
    capturing: Option<Capturing>,
    next_request: u64,
    loop_running: bool,
    last_key: Option<String>,
    last_enabled: bool,
}

impl Projection {
    /// The frame standing in for the page, if it belongs to `key`. One of another session isn't stale,
    /// it is wrong.
    pub fn still(&self, key: Option<&str>) -> Option<&Frame> {
        self.still.as_ref().filter(|frame| frame.content_key == key.unwrap_or(""))
    }

    /// The newest frame of this page, for a layer that wants a backdrop.
    pub fn warm(&self) -> Option<&Frame> {
        self.warm.as_ref()
    }

    /// When the view must run again for the capture loop, if nothing else wakes it first.
    pub fn next_wake(&self) -> Option<Instant> {
        self.next_capture
    }

    fn cancel_transition(&mut self) {
        self.steps.clear();
        self.waiting_answer = false;
        self.settling = false;
    }

    /// Steps the machine. Runs once a frame, first thing: effects, then what the last frame published.
    pub fn frame(&mut self, input: &ProjectionInput, now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        let key = input.content_key.clone().unwrap_or_default();

        // A frame is a picture of one session's page. Switching sessions can't be repaired.
        if self.last_key.as_deref() != Some(key.as_str()) {
            if self.warm.as_ref().is_some_and(|warm| warm.content_key != key) {
                self.warm = None;
            }
            if self.still.as_ref().is_some_and(|still| still.content_key != key) {
                self.still = None;
            }
            self.last_key = Some(key.clone());
        }
        // A frame outliving the content it was a picture of: the pane has swapped to a card.
        if !input.has_content {
            self.still = None;
            self.warm = None;
        }
        self.last_enabled = input.enabled;

        self.transition(input, &key, &mut out);
        self.advance(&mut out);
        self.capture_loop(input, &key, now, &mut out);
        self.reconcile(input, &mut out);
        out
    }

    /// The one place stacking is changed, and the only place the two flags are ordered against the frame.
    fn transition(&mut self, input: &ProjectionInput, key: &str, out: &mut Vec<Command>) {
        let sunk = input.enabled && input.parked;
        let covering = input.enabled && input.parked && input.covered;
        let was_sunk = self.declared_parked;
        let was_covering = self.declared_covered;
        if Some(sunk) == was_sunk && Some(covering) == was_covering {
            return;
        }
        // Nothing to speak for yet, and nothing said: the first decision waits for a session.
        if !input.enabled && was_sunk.is_none() {
            return;
        }
        self.cancel_transition();
        self.declared_parked = Some(sunk);
        self.declared_covered = Some(covering);
        self.settling = true;
        self.chain += 1;
        let chain = self.chain;

        // Already down and staying down: only the host's understanding of why it is down changes.
        if sunk && was_sunk == Some(true) {
            self.steps.extend([Step::Send(Command::SetCovered { chain, value: covering }), Step::AwaitAnswer, Step::Settled]);
            return;
        }

        if !sunk {
            // Raising. The cover is dropped first so the host's own union doesn't hold the page down.
            if was_covering == Some(true) {
                self.steps.extend([Step::Send(Command::SetCovered { chain, value: false }), Step::AwaitAnswer]);
            }
            self.steps.extend([Step::Send(Command::SetParked { chain, value: false }), Step::AwaitAnswer, Step::AwaitFrame, Step::DropStill, Step::Settled]);
            return;
        }

        // Sinking.
        let mut sink_steps = vec![Step::Send(Command::SetParked { chain, value: true }), Step::AwaitAnswer];
        if covering {
            sink_steps.extend([Step::Send(Command::SetCovered { chain, value: true }), Step::AwaitAnswer]);
        }
        sink_steps.push(Step::Settled);

        // Nothing of this page is worth showing: the pane draws its own card over the whole body.
        if !input.has_content {
            self.still = None;
            self.steps.extend(sink_steps);
            return;
        }
        let usable = self.warm.clone().filter(|warm| warm.content_key == key && input.box_width > 0.0 && input.box_height > 0.0);
        // Published now, the still is drawn in this very frame, whatever covered the page. One frame
        // later that is on screen and the page can go.
        if let Some(warm) = usable {
            self.still = Some(warm);
            self.steps.push_back(Step::AwaitFrame);
            self.steps.extend(sink_steps);
            return;
        }
        // No frame yet. Waiting for a capture would leave the page over whatever is covering it for the
        // whole round trip, so it goes down at once and the box is filled when the capture lands.
        self.steps.extend(sink_steps);
        if input.presented {
            self.next_request += 1;
            self.capturing = Some(Capturing { request: self.next_request, for_still: true, chain });
            out.push(Command::Capture { request: self.next_request });
        }
    }

    fn advance(&mut self, out: &mut Vec<Command>) {
        if self.waiting_answer {
            return;
        }
        while let Some(step) = self.steps.pop_front() {
            match step {
                Step::Send(command) => {
                    self.in_flight += 1;
                    out.push(command);
                }
                Step::AwaitAnswer => {
                    self.waiting_answer = true;
                    return;
                }
                // This frame is the one that renders what came before; the next frame continues.
                Step::AwaitFrame => return,
                Step::DropStill => self.still = None,
                Step::Settled => self.settling = false,
            }
        }
    }

    /// The capture loop, which keeps a frame warm for as long as the page is raised.
    fn capture_loop(&mut self, input: &ProjectionInput, key: &str, now: Instant, out: &mut Vec<Command>) {
        let sunk = input.enabled && input.parked;
        if !input.enabled || !input.presented || !input.has_content || sunk {
            self.loop_running = false;
            self.next_capture = None;
            return;
        }
        if !self.loop_running {
            self.loop_running = true;
            self.identical = 0;
            self.failures = 0;
            // Sooner than the cadence: a page just raised may be covered again at any moment.
            self.next_capture = Some(now + FIRST_FRAME_DELAY);
        }
        let _ = key;
        if self.capturing.is_some() || !(input.box_width > 0.0 && input.box_height > 0.0) {
            return;
        }
        if self.next_capture.is_some_and(|at| now >= at) {
            self.next_request += 1;
            self.capturing = Some(Capturing { request: self.next_request, for_still: false, chain: self.chain });
            self.next_capture = None;
            out.push(Command::Capture { request: self.next_request });
        }
    }

    /// Puts the host back in step whenever it and this machine have drifted apart. Only a *change* in
    /// what the host reports arms a repair, or a page the host hasn't got round to would see a request
    /// on every poll; once armed it stays armed until it is made.
    fn reconcile(&mut self, input: &ProjectionInput, out: &mut Vec<Command>) {
        let host_changed = input.host_parked != self.last_host_parked || input.host_covered != self.last_host_covered;
        self.last_host_parked = input.host_parked;
        self.last_host_covered = input.host_covered;
        let (Some(declared_parked), Some(declared_covered)) = (self.declared_parked, self.declared_covered) else {
            return;
        };
        if !input.enabled {
            return;
        }
        let parked_differs = input.host_parked != declared_parked;
        let covered_differs = input.host_covered != declared_covered;
        if !parked_differs && !covered_differs {
            self.repair_owed = false;
            return;
        }
        if host_changed {
            self.repair_owed = true;
        }
        if !self.repair_owed || self.in_flight > 0 || self.settling || !self.steps.is_empty() {
            return;
        }
        self.repair_owed = false;
        if parked_differs {
            self.in_flight += 1;
            out.push(Command::SetParked { chain: 0, value: declared_parked });
        }
        if covered_differs {
            self.in_flight += 1;
            out.push(Command::SetCovered { chain: 0, value: declared_covered });
        }
    }

    /// The host answered a `SetParked` or `SetCovered`, one way or the other: a refusal moves things
    /// along just as an answer does.
    pub fn answered(&mut self, chain: u64) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if chain == self.chain && chain != 0 {
            self.waiting_answer = false;
        }
    }

    /// A capture came back. `None` keeps the frame there was: an older picture of this page is still a
    /// better stand-in than the pane's bare background.
    pub fn captured(&mut self, request: u64, key: &str, frame: Option<(Arc<str>, Arc<Vec<u8>>, u32, u32)>, now: Instant) {
        let Some(capturing) = self.capturing.take_if(|capturing| capturing.request == request) else {
            return;
        };
        let usable = frame.filter(|(_, _, width, height)| *width > MIN_USABLE_CAPTURE_PX && *height > MIN_USABLE_CAPTURE_PX).map(|(data, rgba, width, height)| Frame { content_key: key.to_owned(), data, rgba, width, height });
        let mut delay = FRAME_INTERVAL;
        match usable {
            Some(frame) => {
                self.failures = 0;
                self.identical = if self.warm.as_ref().is_some_and(|warm| warm.data == frame.data) { self.identical + 1 } else { 0 };
                if self.identical >= IDENTICAL_FRAMES_BEFORE_IDLE {
                    delay = IDLE_FRAME_INTERVAL;
                }
                // A sink that had nothing to publish has its box filled now, unless something newer
                // took over in the meantime.
                if capturing.for_still && capturing.chain == self.chain {
                    self.still = Some(frame.clone());
                }
                self.warm = Some(frame);
            }
            None => {
                self.failures += 1;
                if self.failures >= FAILURES_BEFORE_BACKOFF {
                    delay = FRAME_BACKOFF;
                }
            }
        }
        if self.loop_running {
            self.next_capture = Some(now + delay);
        }
    }

    /// A pane going away while its page is up takes the page down on its way out, or for that frame the
    /// page would be painted over whatever took the pane's place. A pane that never declared anything
    /// leaves the page alone. The still goes with the pane: nothing is left to paint it.
    pub fn unmount(&mut self) -> Vec<Command> {
        self.cancel_transition();
        self.still = None;
        self.chain += 1;
        if !self.last_enabled || self.declared_parked != Some(false) {
            return Vec::new();
        }
        self.declared_parked = Some(true);
        self.in_flight += 1;
        vec![Command::SetParked { chain: self.chain, value: true }]
    }
}

/// A rectangle as far as overlap needs one: left, top, right, bottom.
pub type Edges = (f32, f32, f32, f32);

/// Whether `rects` contains anything that overlaps `page`.
pub fn any_rect_overlaps(rects: &[Edges], page: Edges) -> bool {
    let width = |rect: &Edges| rect.2 - rect.0;
    let height = |rect: &Edges| rect.3 - rect.1;
    if !(width(&page) > 0.0) || !(height(&page) > 0.0) {
        return false;
    }
    rects.iter().any(|rect| width(rect) > 0.0 && height(rect) > 0.0 && rect.0 < page.2 && rect.2 > page.0 && rect.1 < page.3 && rect.3 > page.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(parked: bool, covered: bool) -> ProjectionInput {
        ProjectionInput { content_key: Some("s".into()), enabled: true, presented: true, has_content: true, parked, covered, host_parked: false, host_covered: false, box_width: 800.0, box_height: 600.0 }
    }

    fn pixels(data: &str) -> Option<(Arc<str>, Arc<Vec<u8>>, u32, u32)> {
        Some((Arc::from(data), Arc::new(vec![0; 16]), 2, 2))
    }

    /// Steps until the machine asks for nothing new, answering what it asks along the way.
    fn settle(p: &mut Projection, frame: &ProjectionInput, now: Instant) -> Vec<Command> {
        let mut seen = Vec::new();
        for _ in 0..8 {
            let commands = p.frame(frame, now);
            for command in &commands {
                match command {
                    Command::SetParked { chain, .. } | Command::SetCovered { chain, .. } => p.answered(*chain),
                    Command::Capture { .. } => {}
                }
            }
            seen.extend(commands);
        }
        seen
    }

    fn warm_up(p: &mut Projection, now: Instant) -> Instant {
        let raised = input(false, false);
        settle(p, &raised, now);
        let later = now + FIRST_FRAME_DELAY;
        let commands = p.frame(&raised, later);
        let request = commands.iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("the first warm frame");
        p.captured(request, "s", pixels("frame-1"), later);
        later
    }

    fn only_stacking(commands: &[Command]) -> Vec<Command> {
        commands.iter().filter(|command| !matches!(command, Command::Capture { .. })).map(|command| match command {
            Command::SetParked { value, .. } => Command::SetParked { chain: 0, value: *value },
            Command::SetCovered { value, .. } => Command::SetCovered { chain: 0, value: *value },
            other => other.clone(),
        }).collect()
    }

    #[test]
    fn the_first_word_is_always_sent() {
        let mut p = Projection::default();
        let commands = settle(&mut p, &input(false, false), Instant::now());
        assert_eq!(only_stacking(&commands), [Command::SetParked { chain: 0, value: false }]);
    }

    #[test]
    fn nothing_is_said_before_there_is_a_session() {
        let mut p = Projection::default();
        let mut frame = input(false, false);
        frame.enabled = false;
        assert!(settle(&mut p, &frame, Instant::now()).is_empty());
    }

    #[test]
    fn a_new_page_is_declared_down_under_a_card_with_no_capture() {
        let mut p = Projection::default();
        let mut card = input(true, false);
        card.has_content = false;
        card.presented = false;
        let commands = settle(&mut p, &card, Instant::now());
        assert_eq!(only_stacking(&commands), [Command::SetParked { chain: 0, value: true }]);
        assert!(!commands.iter().any(|command| matches!(command, Command::Capture { .. })));
        assert!(p.still(Some("s")).is_none());
    }

    #[test]
    fn sinking_publishes_the_warm_frame_then_waits_a_frame() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        let covered = input(true, true);
        // The frame in which the menu opened: the still is up, and the page hasn't been asked to leave.
        let first = p.frame(&covered, now);
        assert!(only_stacking(&first).is_empty(), "{first:?}");
        assert_eq!(p.still(Some("s")).map(|still| &*still.data), Some("frame-1"));
        // One frame later it goes, and the cover follows the sink's answer.
        let second = p.frame(&covered, now);
        assert_eq!(only_stacking(&second), [Command::SetParked { chain: 0, value: true }]);
        p.answered(p.chain);
        let third = p.frame(&covered, now);
        assert_eq!(only_stacking(&third), [Command::SetCovered { chain: 0, value: true }]);
    }

    #[test]
    fn raising_answers_before_the_still_leaves() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        settle(&mut p, &input(true, true), now);
        assert!(p.still(Some("s")).is_some());
        let raised = input(false, false);
        // The cover first, so the host's union doesn't hold the page down.
        let first = p.frame(&raised, now);
        assert_eq!(only_stacking(&first), [Command::SetCovered { chain: 0, value: false }]);
        assert!(p.still(Some("s")).is_some());
        p.answered(p.chain);
        let second = p.frame(&raised, now);
        assert_eq!(only_stacking(&second), [Command::SetParked { chain: 0, value: false }]);
        assert!(p.still(Some("s")).is_some(), "dropped before the page was back");
        p.answered(p.chain);
        // The page is back: one frame for it to show, then the still goes.
        p.frame(&raised, now);
        assert!(p.still(Some("s")).is_some());
        p.frame(&raised, now);
        assert!(p.still(Some("s")).is_none());
    }

    #[test]
    fn with_no_frame_yet_the_page_goes_down_at_once_and_the_capture_fills_the_box() {
        let mut p = Projection::default();
        let now = Instant::now();
        settle(&mut p, &input(false, false), now);
        let commands = p.frame(&input(true, true), now);
        assert!(commands.contains(&Command::SetParked { chain: p.chain, value: true }), "{commands:?}");
        let request = commands.iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("a capture");
        assert!(p.still(Some("s")).is_none());
        p.captured(request, "s", pixels("late"), now);
        assert_eq!(p.still(Some("s")).map(|still| &*still.data), Some("late"));
    }

    #[test]
    fn a_surface_over_a_page_already_down_only_changes_why() {
        let mut p = Projection::default();
        let now = Instant::now();
        let mut out_of_sight = input(true, false);
        settle(&mut p, &out_of_sight, now);
        out_of_sight.covered = true;
        let commands = settle(&mut p, &out_of_sight, now);
        assert_eq!(only_stacking(&commands), [Command::SetCovered { chain: 0, value: true }]);
    }

    #[test]
    fn it_asks_once_not_once_per_poll_while_the_host_has_yet_to_confirm() {
        let mut p = Projection::default();
        let now = Instant::now();
        let covered = input(true, true);
        let commands = settle(&mut p, &covered, now);
        assert_eq!(only_stacking(&commands).len(), 2);
        // Host still reporting the old state, frame after frame: no storm.
        for _ in 0..20 {
            assert!(only_stacking(&p.frame(&covered, now)).is_empty());
        }
    }

    #[test]
    fn a_host_that_drops_the_cover_on_its_own_is_put_back() {
        let mut p = Projection::default();
        let now = Instant::now();
        let mut covered = input(true, true);
        covered.host_parked = true;
        covered.host_covered = true;
        settle(&mut p, &covered, now);
        covered.host_parked = false;
        covered.host_covered = false;
        let commands = settle(&mut p, &covered, now);
        assert_eq!(only_stacking(&commands), [Command::SetParked { chain: 0, value: true }, Command::SetCovered { chain: 0, value: true }]);
    }

    #[test]
    fn a_pane_mounted_onto_a_sunk_page_raises_it() {
        let mut p = Projection::default();
        let now = Instant::now();
        let mut raised = input(false, false);
        raised.host_parked = true;
        raised.host_covered = true;
        // The host's change is seen on the first frame, with nothing said yet; the first word raises it.
        let commands = settle(&mut p, &raised, now);
        let stacking = only_stacking(&commands);
        assert!(stacking.contains(&Command::SetParked { chain: 0, value: false }), "{stacking:?}");
    }

    #[test]
    fn a_repair_waits_out_a_request_in_flight() {
        let mut p = Projection::default();
        let now = Instant::now();
        let mut frame = input(false, false);
        settle(&mut p, &frame, now);
        // Something else is in flight, and the host changes meanwhile.
        p.in_flight = 1;
        frame.host_parked = true;
        assert!(only_stacking(&p.frame(&frame, now)).is_empty());
        p.in_flight = 0;
        // Still owed once the window closes, though the host's report didn't change again.
        assert_eq!(only_stacking(&p.frame(&frame, now)), [Command::SetParked { chain: 0, value: false }]);
    }

    #[test]
    fn a_page_only_out_of_sight_is_not_covered() {
        let mut p = Projection::default();
        let commands = settle(&mut p, &input(true, false), Instant::now());
        assert_eq!(only_stacking(&commands), [Command::SetParked { chain: 0, value: true }]);
    }

    #[test]
    fn a_page_that_is_up_goes_down_with_the_pane() {
        let mut p = Projection::default();
        settle(&mut p, &input(false, false), Instant::now());
        let commands = p.unmount();
        assert_eq!(only_stacking(&commands), [Command::SetParked { chain: 0, value: true }]);
    }

    #[test]
    fn a_pane_that_never_declared_leaves_the_page_alone() {
        assert!(Projection::default().unmount().is_empty());
        let mut p = Projection::default();
        settle(&mut p, &input(true, false), Instant::now());
        assert!(p.unmount().is_empty());
    }

    #[test]
    fn the_still_goes_with_the_pane() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        settle(&mut p, &input(true, true), now);
        assert!(p.still(Some("s")).is_some());
        p.unmount();
        assert!(p.still(Some("s")).is_none());
    }

    #[test]
    fn the_capture_loop_runs_only_while_the_page_is_up() {
        let mut p = Projection::default();
        let start = Instant::now();
        let now = warm_up(&mut p, start);
        // Sunk: nothing is captured, however long it stays down.
        settle(&mut p, &input(true, true), now);
        let later = now + Duration::from_secs(30);
        assert!(!p.frame(&input(true, true), later).iter().any(|command| matches!(command, Command::Capture { .. })));
        assert_eq!(p.next_wake(), None);
    }

    #[test]
    fn a_page_not_presented_is_not_captured() {
        let mut p = Projection::default();
        let mut frame = input(false, false);
        frame.presented = false;
        let now = Instant::now();
        settle(&mut p, &frame, now);
        assert!(!p.frame(&frame, now + Duration::from_secs(5)).iter().any(|command| matches!(command, Command::Capture { .. })));
    }

    #[test]
    fn an_unchanging_page_idles_down() {
        let mut p = Projection::default();
        let mut now = warm_up(&mut p, Instant::now());
        let raised = input(false, false);
        for _ in 0..IDENTICAL_FRAMES_BEFORE_IDLE {
            now += FRAME_INTERVAL;
            let request = p.frame(&raised, now).iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("a capture");
            p.captured(request, "s", pixels("frame-1"), now);
        }
        assert_eq!(p.next_wake(), Some(now + IDLE_FRAME_INTERVAL));
        // A change brings it straight back.
        now += IDLE_FRAME_INTERVAL;
        let request = p.frame(&raised, now).iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("a capture");
        p.captured(request, "s", pixels("frame-2"), now);
        assert_eq!(p.next_wake(), Some(now + FRAME_INTERVAL));
    }

    #[test]
    fn failing_captures_back_off_and_keep_the_frame_there_was() {
        let mut p = Projection::default();
        let mut now = warm_up(&mut p, Instant::now());
        let raised = input(false, false);
        for _ in 0..FAILURES_BEFORE_BACKOFF {
            now += FRAME_BACKOFF;
            let request = p.frame(&raised, now).iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("a capture");
            p.captured(request, "s", None, now);
        }
        assert_eq!(p.next_wake(), Some(now + FRAME_BACKOFF));
        assert_eq!(p.warm().map(|warm| &*warm.data), Some("frame-1"));
    }

    #[test]
    fn a_single_pixel_is_not_a_frame() {
        let mut p = Projection::default();
        let now = Instant::now();
        let raised = input(false, false);
        settle(&mut p, &raised, now);
        let later = now + FIRST_FRAME_DELAY;
        let request = p.frame(&raised, later).iter().find_map(|command| if let Command::Capture { request } = command { Some(*request) } else { None }).expect("a capture");
        p.captured(request, "s", Some((Arc::from("x"), Arc::new(vec![0; 4]), 1, 1)), later);
        assert!(p.warm().is_none());
    }

    #[test]
    fn a_frame_of_another_session_is_never_shown() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        settle(&mut p, &input(true, true), now);
        assert!(p.still(Some("other")).is_none());
        let mut other = input(true, true);
        other.content_key = Some("other".into());
        p.frame(&other, now);
        assert!(p.still(Some("s")).is_none() && p.warm().is_none());
    }

    #[test]
    fn a_card_takes_the_still_with_it() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        settle(&mut p, &input(true, true), now);
        let mut card = input(true, true);
        card.has_content = false;
        p.frame(&card, now);
        assert!(p.still(Some("s")).is_none() && p.warm().is_none());
    }

    #[test]
    fn a_newer_transition_cancels_an_older_one() {
        let mut p = Projection::default();
        let now = warm_up(&mut p, Instant::now());
        // A menu opens and closes before the sink's frame came.
        p.frame(&input(true, true), now);
        let commands = settle(&mut p, &input(false, false), now);
        let stacking = only_stacking(&commands);
        assert!(!stacking.contains(&Command::SetParked { chain: 0, value: true }), "{stacking:?}");
    }

    #[test]
    fn overlap_needs_area_on_both_sides() {
        let page = (0.0, 0.0, 100.0, 100.0);
        assert!(any_rect_overlaps(&[(50.0, 50.0, 150.0, 150.0)], page));
        assert!(!any_rect_overlaps(&[(100.0, 0.0, 200.0, 100.0)], page), "touching isn't overlapping");
        assert!(!any_rect_overlaps(&[(10.0, 10.0, 10.0, 90.0)], page), "no width");
        assert!(!any_rect_overlaps(&[(10.0, 10.0, 90.0, 90.0)], (0.0, 0.0, 0.0, 100.0)), "no page");
        assert!(!any_rect_overlaps(&[], page));
    }
}
