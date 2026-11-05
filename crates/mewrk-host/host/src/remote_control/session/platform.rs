//! The screen tools' way onto a session: `screen_app`, `screen_device`,
//! `screen_clipboard`, `screen_package`, `android_logcat` and `android_shell`.
//!
//! Each of those tools parses its call and runs it on the session's driver,
//! through the protocol's own extension ([`Driver::android`], [`Driver::ios`],
//! [`Driver::desktop`]); the parsing and the running live with the platform
//! (`remote_control::{android, ios, desktop}`). What a run may do with the
//! screen is decided here, once for all of them, by the [`Effect`] it declares
//! — the same rules `preview_input` and the screenshot keep
//! (`docs/remote-control.md` §3.2):
//!
//! * a **look** takes no input lock, so it overlaps the person's input like a
//!   screenshot does, and works on a `"readOnly": true` entry;
//! * an **act** is input: it refuses a read-only entry and holds the input lock
//!   for the whole run — the person's pages are let go of first and their
//!   batches are dropped meanwhile, exactly as during a `preview_input` batch;
//! * a **background** change (an install) changes the device without driving
//!   its screen: it refuses a read-only entry, but leaves the screen to the
//!   person. A clipboard write is an act, not a background change: it must not
//!   land between the copy and the paste of someone else's paste.
//!
//! Acts and background changes count as input, so the next look lets a phone
//! settle and measures it again. Every run gets the call's time as a [`Step`]:
//! what is left of it, and whether the call was stopped.
//!
//! Two operations are the session's own, because they work on its frames and
//! its coordinate frame rather than on a protocol, and on every kind of screen:
//! `preview_zoom`'s close-up ([`RemoteControlSession::zoom_within`]) and
//! `preview_wait`'s watch ([`RemoteControlSession::wait_within`]). Neither
//! moves the coordinate frame the model's input is read in: that is always the
//! latest screenshot's.

use std::{
    fmt,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use super::{lock, Budget, CallContext, Halt, Inner, Live, RemoteControlSession};
use crate::remote_control::{
    driver::{Driver, DriverError, Frame, Platform, Rect},
    frame,
};

/// How often `preview_wait` looks at a desktop, whose driver is pushed frames and so has the
/// latest one at hand.
const WAIT_POLL: Duration = Duration::from_millis(50);
/// How long the picture must stay the same for `preview_wait` `until: still`.
const STILL_QUIET: Duration = Duration::from_millis(500);
/// A phone's captures go out at least this far apart, so a fast device is not asked for a
/// picture the moment the last one came; a slow one is asked as soon as it has answered.
const PHONE_MIN_GAP: Duration = Duration::from_millis(250);
/// The captures in a row that show the same picture before a phone is called still.
const PHONE_STILL_CAPTURES: usize = 3;
/// What a phone's capture is taken to need until one has been seen: well under what a real
/// device takes (`screencap` over adb, WebDriverAgent's screenshot), so a call with less left
/// than this has no time to look at all.
const PHONE_CAPTURE_GUESS: Duration = Duration::from_millis(600);
/// The most `preview_wait` takes as a `timeout`, which an advice of how long to wait is held to.
const WAIT_TIMEOUT_MAX_MS: u64 = 20_000;
/// The most a close-up enlarges the screen's own pixels: past that a small
/// region only grows blurrier, and costs more of the model's image budget.
const ZOOM_MAX_SCALE: f64 = 2.0;

/// What an operation does to the screen, which decides what it may wait for
/// and hold (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Reads: no input lock; allowed on a read-only entry.
    Look,
    /// Changes what the screen shows: refused on a read-only entry; holds the
    /// input lock; counts as input.
    Act,
    /// Changes the device's state without driving its screen: refused on a
    /// read-only entry; no input lock; counts as input.
    Background,
}

/// The budget a platform operation runs in: what is left of the call, and
/// whether it was stopped. An operation of several steps asks before each one
/// ([`Self::proceed`], [`Self::fits`]) and never starts one it cannot finish;
/// every driver call it makes takes at most [`Self::remaining`].
#[derive(Clone, Copy)]
pub struct Step<'a> {
    budget: Budget<'a>,
}

impl<'a> Step<'a> {
    /// The step of a batch's own budget: what a `preview_input` paste runs
    /// `desktop::paste` in, at its turn, under the batch's lock.
    pub(super) fn of(budget: Budget<'a>) -> Self {
        Self { budget }
    }

    /// A step of `within` from now, for tests of an operation's `run` on a fake
    /// driver, without a session.
    #[cfg(test)]
    pub(crate) fn for_tests(cancelled: &'a dyn Fn() -> bool, within: Duration) -> Self {
        Self {
            budget: Budget {
                cancelled,
                limit: Instant::now() + within,
            },
        }
    }
}

impl Step<'_> {
    /// What is left of the call's time for the work itself (the session keeps
    /// back its margin for the answer).
    pub fn remaining(&self) -> Duration {
        self.budget.remaining()
    }

    /// Whether the call was stopped: its turn or task, or the host's wall clock.
    pub fn cancelled(&self) -> bool {
        (self.budget.cancelled)()
    }

    /// Whether something taking `needed` would end inside the call's time.
    pub fn fits(&self, needed: Duration) -> bool {
        self.budget.fits(needed)
    }

    /// Ok unless cancelled or out of time; the error says `what` did not start.
    pub fn proceed(&self, what: &str) -> Result<(), String> {
        match self.budget.halt() {
            None => Ok(()),
            Some(Halt::Cancelled) => Err(format!("Cancelled before {what}.")),
            Some(Halt::OutOfTime) => Err(format!("Ran out of this call's time before {what}.")),
        }
    }

    /// Sleeps at most `duration`, never past the budget; false if it was cut
    /// short — the call was stopped or its time ran out.
    pub fn pause(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        loop {
            if self.budget.halt().is_some() {
                return false;
            }
            let now = Instant::now();
            if now >= deadline {
                return true;
            }
            let until = deadline.min(self.budget.limit);
            thread::sleep(until.saturating_duration_since(now).min(super::WAIT_SLICE));
        }
    }
}

/// Why an operation failed: the driver's error (the session decides whether the
/// connection is lost), or a refusal or failure the operation states in its own
/// words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpError {
    Driver(DriverError),
    Said(String),
}

impl From<DriverError> for OpError {
    fn from(error: DriverError) -> Self {
        Self::Driver(error)
    }
}

impl From<String> for OpError {
    fn from(message: String) -> Self {
        Self::Said(message)
    }
}

/// A rectangle of the model's coordinate frame — the latest screenshot's
/// pixels — as `preview_zoom` and `preview_wait` take it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameRegion {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `preview_zoom`'s close-up.
#[derive(Clone)]
pub struct ZoomShot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The part of the coordinate frame the close-up shows, rounded to whole
    /// pixels of the frame: the region asked for, clamped to the frame and
    /// widened to the screen pixels that cover it.
    pub region: FrameRegion,
    /// The same part in screen pixels.
    pub screen_region: Rect,
    /// Close-up pixels per frame pixel.
    pub zoom: f64,
}

impl fmt::Debug for ZoomShot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ZoomShot")
            .field("bytes", &self.jpeg.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("region", &self.region)
            .field("screen_region", &self.screen_region)
            .field("zoom", &self.zoom)
            .finish()
    }
}

/// What `preview_wait` waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitUntil {
    /// The picture differs from the one at the start.
    Change,
    /// The picture stayed the same for half a second — on a phone, whose pictures each take a
    /// capture, for three captures in a row at least half a second apart.
    Still,
}

impl RemoteControlSession {
    /// The kind of screen: what the driver says once connected, else what the
    /// entry's protocol drives.
    pub fn platform(&self) -> Platform {
        let inner = &self.inner;
        lock(&inner.state)
            .info
            .as_ref()
            .map_or(inner.target.protocol.platform(), |info| info.platform)
    }

    /// Runs `work` against the connected driver inside `call`: `Look` waits for
    /// the screen only; `Act` refuses a read-only entry and holds the input lock
    /// (as `preview_input`); `Background` refuses a read-only entry without the
    /// lock. `Act` and `Background` count as input (`touched`). Nothing runs once
    /// the call is cancelled or out of time after the wait for the screen.
    ///
    /// A `Driver(Disconnected)` error marks the connection lost, which starts the
    /// session's reconnects; a driver error's message is `"<What>: <error>"` —
    /// `what` with its first letter capitalised ("launching com.foo" reads
    /// "Launching com.foo: …"). An `OpError::Said` is the answer as it is.
    pub fn operate_within<T>(
        &self,
        effect: Effect,
        what: &str,
        call: &CallContext,
        work: impl FnOnce(&dyn Driver, &Step<'_>) -> Result<T, OpError>,
    ) -> Result<T, String> {
        let inner = &self.inner;
        let budget = call.budget();
        match effect {
            Effect::Look => {
                let live = inner.ready_within(&budget)?;
                budget.proceed(what)?;
                inner.operate(&live, what, budget, work)
            }
            Effect::Act => {
                inner.refuse_read_only()?;
                let (_input, live) = inner.take_screen(&budget)?;
                budget.proceed(what)?;
                let result = inner.operate(&live, what, budget, work);
                inner.touched();
                result
            }
            Effect::Background => {
                inner.refuse_read_only()?;
                let live = inner.ready_within(&budget)?;
                budget.proceed(what)?;
                let result = inner.operate(&live, what, budget, work);
                inner.touched();
                result
            }
        }
    }

    /// `preview_zoom`: a fresh capture — of a phone too, whose driver captures anew when asked
    /// for a fresh frame, whether or not anyone is watching — the frame region mapped to
    /// screen pixels and cropped, scaled by min(1280 / its long edge, 2), as a JPEG within
    /// the model's image budget. The region is clamped to the frame; one with
    /// nothing left is refused. Refuses when the screen changed size since the
    /// last screenshot, whose frame the region is in ("take a new
    /// preview_screenshot first"); before any screenshot, the frame is the one
    /// `preview_start` stated.
    ///
    /// Does not change the coordinate frame the model's input is read in
    /// (`last_shown`): a close-up is never a frame to place input by.
    pub fn zoom_within(&self, region: FrameRegion, call: &CallContext) -> Result<ZoomShot, String> {
        let inner = &self.inner;
        let budget = call.budget();
        let live = inner.ready_within(&budget)?;
        inner.settle(&live, &budget);
        budget.proceed("taking the close-up")?;
        let picture = live
            .driver
            .frame(true)
            .map_err(|error| inner.failed_call(&live, error, "Could not capture the screen"))?;
        frame::check_frame(&picture)
            .map_err(|error| format!("Could not capture the screen: {error}"))?;
        inner.note_size(picture.width, picture.height);
        let screen = (picture.width, picture.height);
        let model = inner.frame_for_region(&live, screen)?;
        let (shown, area) = map_region(region, model, screen)?;
        let (width, height) = zoom_size(area.width, area.height);
        let jpeg = frame::encode_area_for_model(&picture, area, (width, height))
            .map_err(|error| format!("Could not make the close-up: {error}"))?;
        Ok(ZoomShot {
            jpeg,
            width,
            height,
            region: FrameRegion {
                x: shown.x.round(),
                y: shown.y.round(),
                width: shown.width.round(),
                height: shown.height.round(),
            },
            screen_region: area,
            zoom: f64::from(width) / shown.width,
        })
    }

    /// `preview_wait`: watches the screen — or a frame region of it — until it
    /// changes (`Change`: differs from the picture at the start) or holds still
    /// (`Still`: the same for half a second), or `timeout` passes. A desktop is
    /// looked at every 50 ms, at the picture the driver has, touching nothing; a
    /// phone cannot be, because its driver keeps its last capture and nothing
    /// refreshes that while no viewer is watching, so each look there is a fresh
    /// capture ([`Inner::watch_phone`]). A change of the screen's size is a
    /// change. Bounded by the call's time as well, and stopped by its
    /// cancellation. The answer is how long it waited, never a picture.
    pub fn wait_within(
        &self,
        until: WaitUntil,
        timeout: Duration,
        region: Option<FrameRegion>,
        call: &CallContext,
    ) -> Result<String, String> {
        let inner = &self.inner;
        let budget = call.budget();
        let live = inner.ready_within(&budget)?;
        budget.proceed("watching the screen")?;
        if live.driver.info().platform.is_touch() {
            return inner.watch_phone(&live, until, timeout, region, budget);
        }
        let look = || -> Result<Frame, String> {
            let picture = live
                .driver
                .frame(false)
                .map_err(|error| inner.failed_call(&live, error, "Could not read the screen"))?;
            frame::check_frame(&picture)
                .map_err(|error| format!("Could not read the screen: {error}"))?;
            Ok(picture)
        };
        let first = look()?;
        let area = match region {
            Some(region) => {
                let screen = (first.width, first.height);
                let model = inner.frame_for_region(&live, screen)?;
                Some(map_region(region, model, screen)?.1)
            }
            None => None,
        };
        let stopped = || Err("Cancelled while watching the screen.".to_owned());
        let started = Instant::now();
        let asked = started + timeout;
        let end = asked.min(budget.limit);
        let mut reference = first;
        let mut last_change = started;
        // Whether the picture changed at all: a `Still` wait that saw no change and still ran
        // out did so because its time was shorter than the quiet it needs, not because the
        // screen kept moving.
        let mut changed = false;
        loop {
            if (budget.cancelled)() {
                return stopped();
            }
            let now = Instant::now();
            if now >= end {
                break;
            }
            thread::sleep((end - now).min(WAIT_POLL));
            if (budget.cancelled)() {
                return stopped();
            }
            let next = look()?;
            let seen = Instant::now();
            if differs(&reference, &next, area) {
                match until {
                    WaitUntil::Change => {
                        return Ok(format!(
                            "The screen changed after {}.",
                            seconds(seen - started)
                        ))
                    }
                    WaitUntil::Still => {
                        changed = true;
                        last_change = seen;
                        reference = next;
                    }
                }
            } else if until == WaitUntil::Still && seen - last_change >= STILL_QUIET {
                return Ok(format!(
                    "The screen held still from {} on.",
                    seconds(last_change - started)
                ));
            }
        }
        let (waited, cut) = if end < asked {
            (
                end.saturating_duration_since(started),
                " (this call ran out of time first)",
            )
        } else {
            (timeout, "")
        };
        Ok(match until {
            WaitUntil::Change => {
                format!("The screen did not change within {}{cut}.", seconds(waited))
            }
            WaitUntil::Still if changed => {
                format!("The screen kept changing for {}{cut}.", seconds(waited))
            }
            // Never changed, yet never still for long enough to say so.
            WaitUntil::Still if cut.is_empty() => format!(
                "The screen did not change in {}; that is too short to call it still, which takes half a second without a change. Give a timeout of 500 or more.",
                seconds(waited)
            ),
            WaitUntil::Still => format!(
                "The screen did not change in {}{cut}; that is too short to call it still, which takes half a second without a change.",
                seconds(waited)
            ),
        })
    }
}

impl Inner {
    /// Runs one platform operation on `live`, mapping its error.
    fn operate<T>(
        &self,
        live: &Live,
        what: &str,
        budget: Budget<'_>,
        work: impl FnOnce(&dyn Driver, &Step<'_>) -> Result<T, OpError>,
    ) -> Result<T, String> {
        let step = Step { budget };
        work(live.driver.as_ref(), &step).map_err(|error| match error {
            OpError::Driver(error) => self.failed_call(live, error, &capitalized(what)),
            OpError::Said(message) => message,
        })
    }

    /// `preview_wait` on a phone or tablet. Every look is a fresh capture ([`Driver::frame`]
    /// with `fresh`): the driver's other picture is its last capture, which nothing refreshes
    /// while no viewer is watching, so a change would never show in it. A capture takes as long
    /// as the device takes (half a second to a couple of seconds over adb or WebDriverAgent),
    /// so before each one the call's time is asked and none starts that could not end inside
    /// it — judged by the longest seen so far — and the captures are spaced
    /// [`PHONE_MIN_GAP`] apart at least. A picture is stamped with the moment its request went
    /// out, the latest the screen can have shown it.
    ///
    /// `Change` answers at the first capture that differs from the first. `Still` takes
    /// [`PHONE_STILL_CAPTURES`] captures in a row showing the same picture, the first and the
    /// last [`STILL_QUIET`] apart at least. The answer says how often it looked.
    fn watch_phone(
        &self,
        live: &Live,
        until: WaitUntil,
        timeout: Duration,
        region: Option<FrameRegion>,
        budget: Budget<'_>,
    ) -> Result<String, String> {
        let capture = || -> Result<(Frame, Instant), String> {
            let sent = Instant::now();
            let picture = live
                .driver
                .frame(true)
                .map_err(|error| self.failed_call(live, error, "Could not read the screen"))?;
            frame::check_frame(&picture)
                .map_err(|error| format!("Could not read the screen: {error}"))?;
            Ok((picture, sent))
        };
        if !budget.fits(PHONE_CAPTURE_GUESS) {
            return Err("Ran out of time before watching the screen: a phone's screen takes about a second to capture, and this call has less left. Try again.".to_owned());
        }
        let started = Instant::now();
        let (first, sent) = capture()?;
        // The longest capture seen, which the next is expected to take as well.
        let mut longest = sent.elapsed();
        let area = match region {
            Some(region) => {
                let screen = (first.width, first.height);
                let model = self.frame_for_region(live, screen)?;
                Some(map_region(region, model, screen)?.1)
            }
            None => None,
        };
        let asked = started + timeout;
        let mut sends = vec![sent];
        let mut reference = first;
        // The run of captures showing `reference`'s picture: when its first request went out,
        // and how many there are.
        let mut run_began = sent;
        let mut run_length = 1;
        // Whether the picture changed at all: a `Still` wait that saw no change and still ran
        // out did so because it was too short for the captures it needs, not because the
        // screen kept moving.
        let mut changed = false;
        let cut = loop {
            if (budget.cancelled)() {
                return Err("Cancelled while watching the screen.".to_owned());
            }
            let now = Instant::now();
            if now >= asked {
                break false;
            }
            if now >= budget.limit {
                break true;
            }
            let due = sends.last().map_or(now, |last| *last + PHONE_MIN_GAP);
            if now < due {
                thread::sleep((due - now).min(WAIT_POLL));
                continue;
            }
            // Never a capture that could not end inside the call.
            if !budget.fits(longest.mul_f64(1.25)) {
                break true;
            }
            let (next, sent) = capture()?;
            longest = longest.max(sent.elapsed());
            sends.push(sent);
            if differs(&reference, &next, area) {
                if until == WaitUntil::Change {
                    return Ok(format!(
                        "The screen changed after {}{}.",
                        seconds(sent.saturating_duration_since(started)),
                        phone_notes(false, looked_every(&sends))
                    ));
                }
                changed = true;
                reference = next;
                (run_began, run_length) = (sent, 1);
            } else {
                run_length += 1;
                if until == WaitUntil::Still
                    && run_length >= PHONE_STILL_CAPTURES
                    && sent.saturating_duration_since(run_began) >= STILL_QUIET
                {
                    return Ok(format!(
                        "The screen held still from {} on{}.",
                        seconds(run_began.saturating_duration_since(started)),
                        phone_notes(false, looked_every(&sends))
                    ));
                }
            }
        };
        let waited = if cut {
            Instant::now().saturating_duration_since(started)
        } else {
            timeout
        };
        let every = looked_every(&sends);
        // What one capture takes here, as far as it is known.
        let each = every.unwrap_or(longest);
        let notes = phone_notes(cut, every);
        Ok(if sends.len() < 2 {
            // Nothing to compare: the first picture took the call's time, or the timeout was
            // shorter than a capture.
            let advice = if cut {
                String::new()
            } else {
                format!(
                    " Give a timeout of {} or more.",
                    advised_timeout(each, PHONE_STILL_CAPTURES)
                )
            };
            format!(
                "Only one picture of the screen was taken within {}{}, so nothing could be compared: a capture takes about {} here.{advice}",
                seconds(waited),
                if cut { " (this call ran out of time first)" } else { "" },
                span(each)
            )
        } else {
            match until {
                WaitUntil::Change => {
                    format!(
                        "The screen did not change within {}{notes}.",
                        seconds(waited)
                    )
                }
                WaitUntil::Still if changed => {
                    format!("The screen kept changing for {}{notes}.", seconds(waited))
                }
                WaitUntil::Still => {
                    let advice = if cut {
                        String::new()
                    } else {
                        format!(
                            " Give a timeout of {} or more.",
                            advised_timeout(each, PHONE_STILL_CAPTURES)
                        )
                    };
                    format!(
                        "The screen did not change in {}{notes}; that is too short to call it still, which takes three captures in a row without a change, half a second apart at least (a capture takes about {} here).{advice}",
                        seconds(waited),
                        span(each)
                    )
                }
            }
        })
    }

    /// The coordinate frame a region the model gives is in, for a screen of
    /// `screen` pixels: the latest screenshot's — refused when the screen has
    /// changed size since, as input is — or, before any, the screen's own.
    /// Changes nothing: a region read off a stale frame is the caller's error.
    fn frame_for_region(&self, live: &Live, screen: (u32, u32)) -> Result<(u32, u32), String> {
        let current = frame::model_frame(screen.0, screen.1);
        let look = self.look_again(live);
        match lock(&self.state).last_shown {
            Some(shown) if shown != current => Err(format!(
                "The screen changed size: it is {}x{} pixels now (coordinate frame {}x{}), but the last {look} was {}x{}. Take a new {look} first.",
                screen.0, screen.1, current.0, current.1, shown.0, shown.1
            )),
            _ => Ok(current),
        }
    }
}

/// `what` as the start of a sentence.
fn capitalized(what: &str) -> String {
    let mut characters = what.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

/// A duration as an answer says it: whole seconds when it is whole (`5 s`),
/// else tenths (`1.3 s`).
fn seconds(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis % 1000 == 0 {
        format!("{} s", millis / 1000)
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

/// How far apart a phone's captures went out on average, when there were two or more.
fn looked_every(sends: &[Instant]) -> Option<Duration> {
    let (first, last) = (sends.first()?, sends.last()?);
    let gaps = u32::try_from(sends.len().checked_sub(1)?)
        .ok()
        .filter(|gaps| *gaps > 0)?;
    Some(last.saturating_duration_since(*first) / gaps)
}

/// A span as a phone's answer names how long a capture takes: tenths of a second, or
/// milliseconds when that is all it is.
fn span(duration: Duration) -> String {
    if duration < Duration::from_millis(100) {
        format!("{} ms", duration.as_millis())
    } else {
        seconds(duration)
    }
}

/// What a phone's answer notes in brackets: that the call's time ended the watch, and how
/// often it looked (`looked every ~1.2 s`).
fn phone_notes(cut: bool, every: Option<Duration>) -> String {
    let mut notes = Vec::new();
    if cut {
        notes.push("this call ran out of time first".to_owned());
    }
    if let Some(every) = every {
        notes.push(format!("looked every ~{}", span(every)));
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!(" ({})", notes.join("; "))
    }
}

/// The `timeout` in milliseconds a phone's `preview_wait` is advised to give for `captures`
/// captures of `each`: rounded up to a tenth of a second, at least half a second, and no more
/// than the tool takes.
fn advised_timeout(each: Duration, captures: usize) -> u64 {
    let wanted = each.as_millis().saturating_mul(captures as u128);
    let rounded = wanted.saturating_add(99) / 100 * 100;
    u64::try_from(rounded)
        .unwrap_or(WAIT_TIMEOUT_MAX_MS)
        .clamp(500, WAIT_TIMEOUT_MAX_MS)
}

/// `region` of the coordinate frame `frame`, clamped to it, on a screen of
/// `screen` pixels: the screen pixels that cover it — whole pixels, so a
/// region's edge inside a screen pixel takes all of it — and the part of the
/// frame those pixels show, exactly.
fn map_region(
    region: FrameRegion,
    frame: (u32, u32),
    screen: (u32, u32),
) -> Result<(FrameRegion, Rect), String> {
    let finite = [region.x, region.y, region.width, region.height]
        .iter()
        .all(|value| value.is_finite());
    if !finite || frame.0 == 0 || frame.1 == 0 || screen.0 == 0 || screen.1 == 0 {
        return Err("The region must be four numbers, and the screen's size known; take a preview_screenshot first.".to_owned());
    }
    let (frame_width, frame_height) = (f64::from(frame.0), f64::from(frame.1));
    let left = region.x.clamp(0.0, frame_width);
    let top = region.y.clamp(0.0, frame_height);
    let right = (region.x + region.width).clamp(0.0, frame_width);
    let bottom = (region.y + region.height).clamp(0.0, frame_height);
    if right <= left || bottom <= top {
        return Err(format!(
            "The region at ({}, {}), {} by {}, has nothing of the {}x{} coordinate frame in it; give one inside the latest screenshot.",
            region.x, region.y, region.width, region.height, frame.0, frame.1
        ));
    }
    let scale_x = f64::from(screen.0) / frame_width;
    let scale_y = f64::from(screen.1) / frame_height;
    // A hair off whole numbers either way, so that a product like 640 × 1.5
    // that floating point makes 960.0000001 does not take one more pixel.
    const EPSILON: f64 = 1e-9;
    let first = |value: f64, scale: f64, size: u32| {
        ((value * scale + EPSILON).floor().max(0.0) as u32).min(size - 1)
    };
    let past = |value: f64, scale: f64, size: u32, start: u32| {
        ((value * scale - EPSILON).ceil().max(0.0) as u32).clamp(start + 1, size)
    };
    let x0 = first(left, scale_x, screen.0);
    let y0 = first(top, scale_y, screen.1);
    let x1 = past(right, scale_x, screen.0, x0);
    let y1 = past(bottom, scale_y, screen.1, y0);
    let area = Rect {
        x: x0 as i32,
        y: y0 as i32,
        width: x1 - x0,
        height: y1 - y0,
    };
    let shown = FrameRegion {
        x: f64::from(x0) / scale_x,
        y: f64::from(y0) / scale_y,
        width: f64::from(area.width) / scale_x,
        height: f64::from(area.height) / scale_y,
    };
    Ok((shown, area))
}

/// The close-up of `width × height` screen pixels: scaled so its long edge is
/// the coordinate frame's ([`frame::MODEL_LONG_EDGE`]), but never by more
/// than [`ZOOM_MAX_SCALE`].
fn zoom_size(width: u32, height: u32) -> (u32, u32) {
    let long = f64::from(width.max(height).max(1));
    let scale = (f64::from(frame::MODEL_LONG_EDGE) / long).min(ZOOM_MAX_SCALE);
    let side =
        |value: u32| ((f64::from(value) * scale).round() as u32).clamp(1, frame::MODEL_LONG_EDGE);
    (side(width), side(height))
}

/// Whether `next` shows something other than `reference` in `area` (screen
/// pixels), or anywhere when there is no area. A picture of another size is
/// another picture; an area that no longer fits in it is watched as the whole.
fn differs(reference: &Frame, next: &Frame, area: Option<Rect>) -> bool {
    if (reference.width, reference.height) != (next.width, next.height) {
        return true;
    }
    if Arc::ptr_eq(&reference.rgba, &next.rgba) {
        return false;
    }
    let fits = |area: &Rect| {
        u32::try_from(area.x)
            .ok()
            .and_then(|x| x.checked_add(area.width))
            .is_some_and(|right| right <= reference.width)
            && u32::try_from(area.y)
                .ok()
                .and_then(|y| y.checked_add(area.height))
                .is_some_and(|bottom| bottom <= reference.height)
    };
    match area.filter(fits) {
        None => reference.rgba != next.rgba,
        Some(area) => {
            let width = reference.width as usize;
            let (x, y) = (area.x as usize, area.y as usize);
            let row = area.width as usize * 4;
            (y..y + area.height as usize).any(|line| {
                let start = (line * width + x) * 4;
                reference.rgba[start..start + row] != next.rgba[start..start + row]
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Mutex,
    };

    use super::*;
    use crate::remote_control::{
        credentials::tests::MemoryStore,
        fake::FakeDriver,
        session::{ConnectFn, SessionSpec, Timing},
        target::{ControlTarget, Protocol, TargetPlace},
        transport::{Connection, Connector},
        view::{PointerPhase, SessionPhase, ViewerBackend, ViewerEvent},
    };

    /// A place that is never dialled: the fake driver does not use it.
    struct Lab;

    impl Connector for Lab {
        fn connect(&self, port: u16, _timeout: Duration) -> Result<Connection, String> {
            Err(format!("nothing is listening on 127.0.0.1:{port} on lab"))
        }

        fn place_label(&self) -> &str {
            "lab"
        }

        fn place_key(&self) -> &str {
            "ssh:m1"
        }

        fn is_local(&self) -> bool {
            false
        }
    }

    struct Screen {
        session: Arc<RemoteControlSession>,
        logs: Arc<Mutex<Vec<String>>>,
    }

    impl Screen {
        fn logs(&self) -> String {
            self.logs.lock().unwrap().join("\n")
        }

        fn last_shown(&self) -> Option<(u32, u32)> {
            lock(&self.session.inner.state).last_shown
        }

        fn last_input(&self) -> Option<Instant> {
            lock(&self.session.inner.state).last_input
        }
    }

    /// A session on `driver`, connected, with short waits: two seconds for the
    /// screen, 300 ms for the input lock.
    fn connect(driver: Arc<dyn Driver>, protocol: Protocol, read_only: bool) -> Screen {
        let logs = Arc::new(Mutex::new(Vec::new()));
        let connect: ConnectFn =
            Arc::new(move |_target, _connector, _auth, _events| Ok(driver.clone()));
        let spec = SessionSpec {
            name: "screen".into(),
            server_id: "screen".into(),
            target: ControlTarget {
                protocol,
                place: TargetPlace::Machine("lab".into()),
                port: protocol.default_port(),
                device: None,
                username: None,
                launch: None,
                read_only,
            },
            connector: Arc::new(Lab),
            preflight: None,
            on_change: Arc::new(|| {}),
            log: {
                let logs = logs.clone();
                Arc::new(move |line: &str| logs.lock().unwrap().push(line.to_owned()))
            },
            connect: Some(connect),
        };
        let timing = Timing {
            backoff: vec![Duration::from_millis(10); 5],
            ready_wait: Duration::from_secs(2),
            input_lock_wait: Duration::from_millis(300),
            touch_settle: Duration::from_millis(1),
            click_gap: Duration::from_millis(1),
            pull_pause: Duration::from_millis(5),
            encode_interval: Duration::from_millis(1),
            fill_focus: Duration::from_millis(1),
            replace: Default::default(),
        };
        let session =
            RemoteControlSession::start_with(spec, Arc::new(MemoryStore::default()), timing)
                .unwrap();
        assert_eq!(
            session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
        Screen { session, logs }
    }

    fn desktop(width: u32, height: u32) -> (Arc<FakeDriver>, Screen) {
        let driver = FakeDriver::new(Platform::Desktop, width, height);
        let screen = connect(driver.clone(), Protocol::Vnc, false);
        (driver, screen)
    }

    fn within<'a>(cancelled: &'a dyn Fn() -> bool, time: Duration) -> CallContext<'a> {
        CallContext {
            deadline: Instant::now() + time,
            cancelled,
        }
    }

    fn call(cancelled: &dyn Fn() -> bool) -> CallContext<'_> {
        within(cancelled, Duration::from_secs(20))
    }

    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn region(x: f64, y: f64, width: f64, height: f64) -> FrameRegion {
        FrameRegion {
            x,
            y,
            width,
            height,
        }
    }

    fn decode(jpeg: &[u8]) -> (usize, usize, Vec<u8>) {
        use zune_jpeg::zune_core::{colorspace::ColorSpace, options::DecoderOptions};
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder = zune_jpeg::JpegDecoder::new_with_options(jpeg, options);
        let pixels = decoder.decode().expect("a valid JPEG");
        let (width, height) = decoder.dimensions().expect("dimensions");
        (width, height, pixels)
    }

    /// What a look, an act and a background change may do: a look works on a
    /// read-only entry and takes no lock; the other two are refused there, an
    /// act holding the input lock and a background change not.
    #[test]
    fn each_effect_holds_what_it_should_and_a_read_only_screen_only_looks() {
        let read_only = connect(
            FakeDriver::new(Platform::Desktop, 640, 400),
            Protocol::Vnc,
            true,
        );
        let session = &read_only.session;
        let inner = &session.inner;
        let looked = session
            .operate_within(
                Effect::Look,
                "reading the clipboard",
                &call(&|| false),
                |_, _| {
                    assert!(inner.input.try_lock().is_ok(), "a look takes no input lock");
                    assert!(!inner.model_active.load(Ordering::Acquire));
                    Ok("looked")
                },
            )
            .unwrap();
        assert_eq!(looked, "looked");
        assert!(read_only.last_input().is_none(), "a look is not input");
        for effect in [Effect::Act, Effect::Background] {
            let ran = AtomicBool::new(false);
            let refused = session
                .operate_within(effect, "pasting", &call(&|| false), |_, _| {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap_err();
            assert!(refused.contains("is read-only"), "{effect:?}: {refused}");
            assert!(!ran.load(Ordering::SeqCst), "{effect:?}");
        }
        assert!(read_only.last_input().is_none());

        // A background change does not wait for the input lock someone else holds; an act does,
        // and gives up when it does not come.
        let (_driver, writable) = desktop(640, 400);
        let session = &writable.session;
        let inner = &session.inner;
        {
            let _held = inner.input.lock().unwrap();
            let background = session.operate_within(
                Effect::Background,
                "writing the clipboard",
                &call(&|| false),
                |_, _| {
                    assert!(!inner.model_active.load(Ordering::Acquire));
                    Ok(())
                },
            );
            assert_eq!(background, Ok(()));
            let started = Instant::now();
            let act = session
                .operate_within(Effect::Act, "pasting", &call(&|| false), |_, _| Ok(()))
                .unwrap_err();
            assert!(act.contains("Another input batch"), "{act}");
            assert!(started.elapsed() >= Duration::from_millis(250));
        }
        assert!(
            writable.last_input().is_some(),
            "a background change is input"
        );
    }

    /// An act has the screen to itself: the model is shown as operating, a page's batch that
    /// comes meanwhile is dropped (not queued), and afterwards the next look settles and
    /// measures again.
    #[test]
    fn an_act_holds_the_screen_and_drops_the_persons_batch() {
        let (driver, screen) = desktop(640, 400);
        let session = &screen.session;
        let inner = &session.inner;
        let backend: &dyn ViewerBackend = inner.as_ref();
        let result = session.operate_within(Effect::Act, "pasting", &call(&|| false), |_, step| {
            assert!(inner.model_active.load(Ordering::Acquire));
            assert!(
                inner.input.try_lock().is_err(),
                "the act holds the input lock"
            );
            backend.input(
                "page-1",
                vec![ViewerEvent::Pointer {
                    phase: PointerPhase::Down,
                    x: 0.5,
                    y: 0.5,
                    buttons: 1,
                    at: None,
                }],
            );
            assert!(step.remaining() > Duration::from_secs(10));
            Ok(7)
        });
        assert_eq!(result, Ok(7));
        assert!(driver.inputs().is_empty(), "{:?}", driver.inputs());
        assert!(!inner.model_active.load(Ordering::Acquire));
        assert!(screen.last_input().is_some(), "an act is input");
        // With the act over, the page's input goes through again.
        backend.input(
            "page-1",
            vec![ViewerEvent::Pointer {
                phase: PointerPhase::Down,
                x: 0.5,
                y: 0.5,
                buttons: 1,
                at: None,
            }],
        );
        assert_eq!(
            driver.inputs(),
            vec!["pointer 320,200 buttons 1".to_owned()]
        );
    }

    /// A driver that says the device is gone ends the connection, and the session reconnects;
    /// any other failure, and an operation's own words, are only the call's answer.
    #[test]
    fn errors_are_the_operations_and_a_lost_device_reconnects() {
        let (_driver, screen) = desktop(640, 400);
        let session = &screen.session;
        let failed = session
            .operate_within(
                Effect::Look,
                "reading the clipboard",
                &call(&|| false),
                |_, _| {
                    Err::<(), _>(OpError::Driver(DriverError::Failed(
                        "no answer in 2 s".into(),
                    )))
                },
            )
            .unwrap_err();
        assert_eq!(failed, "Reading the clipboard: no answer in 2 s");
        let said = session
            .operate_within(Effect::Act, "pasting", &call(&|| false), |_, _| {
                Err::<(), _>(OpError::Said("Nothing to paste.".into()))
            })
            .unwrap_err();
        assert_eq!(said, "Nothing to paste.");
        assert!(!screen.logs().contains("lost the connection"));

        let lost = session
            .operate_within(Effect::Act, "pasting", &call(&|| false), |_, _| {
                Err::<(), _>(DriverError::Disconnected("the server went away".into()).into())
            })
            .unwrap_err();
        assert_eq!(lost, "Pasting: the server went away");
        eventually("the reconnect", || {
            screen
                .logs()
                .contains("lost the connection (the server went away)")
        });
        assert_eq!(
            session.wait_settled(Duration::from_secs(5)),
            SessionPhase::Ready
        );
    }

    /// Nothing starts once the call is cancelled or its time is gone.
    #[test]
    fn a_stopped_call_runs_no_operation_and_a_step_keeps_to_its_budget() {
        let (_driver, screen) = desktop(640, 400);
        let session = &screen.session;
        let ran = AtomicBool::new(false);
        let cancelled = session
            .operate_within(Effect::Act, "pasting", &call(&|| true), |_, _| {
                ran.store(true, Ordering::SeqCst);
                Ok(())
            })
            .unwrap_err();
        assert!(cancelled.starts_with("Cancelled"), "{cancelled}");
        // A call with less than its margin left has no time for the work at all.
        let late = session
            .operate_within(
                Effect::Look,
                "listing the apps",
                &within(&|| false, Duration::from_secs(1)),
                |_, _| {
                    ran.store(true, Ordering::SeqCst);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(
            late.starts_with("Ran out of time before listing the apps"),
            "{late}"
        );
        assert!(!ran.load(Ordering::SeqCst));

        let step = Step::for_tests(&|| false, Duration::from_millis(300));
        assert!(step.remaining() <= Duration::from_millis(300));
        assert!(step.fits(Duration::from_millis(100)));
        assert!(!step.fits(Duration::from_secs(1)));
        assert_eq!(step.proceed("the second step"), Ok(()));
        assert!(step.pause(Duration::from_millis(50)));
        let started = Instant::now();
        assert!(
            !step.pause(Duration::from_secs(5)),
            "cut short by the budget"
        );
        assert!(started.elapsed() < Duration::from_millis(400));
        assert_eq!(
            step.proceed("the second step"),
            Err("Ran out of this call's time before the second step.".to_owned())
        );
        let stopped = Step::for_tests(&|| true, Duration::from_secs(5));
        assert!(stopped.cancelled());
        assert!(!stopped.pause(Duration::from_millis(10)));
        assert_eq!(
            stopped.proceed("installing"),
            Err("Cancelled before installing.".to_owned())
        );
        assert_eq!(session.platform(), Platform::Desktop);
    }

    #[test]
    fn a_region_maps_onto_the_screen_pixels_that_cover_it() {
        // A 1920x1080 desktop in its 1280x720 frame: 1.5 screen pixels per frame pixel.
        let (shown, area) = map_region(
            region(640.0, 360.0, 320.0, 180.0),
            (1280, 720),
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(
            area,
            Rect {
                x: 960,
                y: 540,
                width: 480,
                height: 270
            }
        );
        assert_eq!(shown, region(640.0, 360.0, 320.0, 180.0));
        // Clamped to the frame.
        let (shown, area) = map_region(
            region(1200.0, 700.0, 200.0, 100.0),
            (1280, 720),
            (1920, 1080),
        )
        .unwrap();
        assert_eq!(
            area,
            Rect {
                x: 1800,
                y: 1050,
                width: 120,
                height: 30
            }
        );
        assert_eq!(shown, region(1200.0, 700.0, 80.0, 20.0));
        // An edge inside a screen pixel takes the whole pixel.
        let (shown, area) =
            map_region(region(0.5, 0.0, 1.0, 1.0), (1280, 720), (1920, 1080)).unwrap();
        assert_eq!(
            area,
            Rect {
                x: 0,
                y: 0,
                width: 3,
                height: 2
            }
        );
        assert_eq!(shown.width, 2.0);
        // Nothing of the frame in it.
        let outside =
            map_region(region(1280.0, 0.0, 10.0, 10.0), (1280, 720), (1920, 1080)).unwrap_err();
        assert!(
            outside.contains("has nothing of the 1280x720 coordinate frame"),
            "{outside}"
        );
        assert!(map_region(region(10.0, 10.0, 0.0, 5.0), (1280, 720), (1920, 1080)).is_err());

        // Up to the coordinate frame's long edge, never more than twice the screen's pixels.
        assert_eq!(zoom_size(480, 270), (960, 540));
        assert_eq!(zoom_size(1000, 500), (1280, 640));
        assert_eq!(zoom_size(3840, 2160), (1280, 720));
        assert_eq!(zoom_size(1, 1), (2, 2));
    }

    /// A close-up shows the region, larger, and leaves the frame the model's input is read in
    /// alone; a screen that changed size since the screenshot is refused.
    #[test]
    fn a_close_up_shows_its_region_and_never_moves_the_frame() {
        let (driver, screen) = desktop(1920, 1080);
        driver.paint((960, 540), (480, 270), [220, 30, 30]);
        let session = &screen.session;
        let shot = session
            .zoom_within(region(640.0, 360.0, 320.0, 180.0), &call(&|| false))
            .unwrap();
        assert_eq!((shot.width, shot.height), (960, 540));
        assert_eq!(shot.region, region(640.0, 360.0, 320.0, 180.0));
        assert_eq!(
            shot.screen_region,
            Rect {
                x: 960,
                y: 540,
                width: 480,
                height: 270
            }
        );
        assert_eq!(shot.zoom, 3.0);
        let (width, height, pixels) = decode(&shot.jpeg);
        assert_eq!((width, height), (960, 540));
        let middle = (270 * 960 + 480) * 3;
        assert!(
            pixels[middle] > 180 && pixels[middle + 1] < 80,
            "{:?}",
            &pixels[middle..middle + 3]
        );
        assert!(shot.jpeg.len() <= crate::image_attachments::MODEL_IMAGE_BYTE_BUDGET);
        assert_eq!(screen.last_shown(), None, "a close-up is no frame");

        let whole = session
            .zoom_within(region(-50.0, -50.0, 5000.0, 5000.0), &call(&|| false))
            .unwrap();
        assert_eq!((whole.width, whole.height), (1280, 720));
        assert_eq!(whole.region, region(0.0, 0.0, 1280.0, 720.0));
        assert!((whole.zoom - 1.0).abs() < 1e-9);

        session.screenshot_within(&call(&|| false)).unwrap();
        assert_eq!(screen.last_shown(), Some((1280, 720)));
        let corner = session
            .zoom_within(region(1200.0, 700.0, 200.0, 100.0), &call(&|| false))
            .unwrap();
        assert_eq!((corner.width, corner.height), (240, 60));
        assert_eq!(corner.region, region(1200.0, 700.0, 80.0, 20.0));
        assert_eq!(screen.last_shown(), Some((1280, 720)));

        driver.resize(1080, 1920);
        let refused = session
            .zoom_within(region(0.0, 0.0, 100.0, 100.0), &call(&|| false))
            .unwrap_err();
        assert!(refused.starts_with("The screen changed size"), "{refused}");
        assert!(
            refused.contains("Take a new preview_screenshot first"),
            "{refused}"
        );
        assert_eq!(screen.last_shown(), Some((1280, 720)));
        let empty = session.zoom_within(region(5000.0, 0.0, 10.0, 10.0), &call(&|| false));
        assert!(empty.is_err());
    }

    /// Runs `paint` on its own thread while the wait runs, and returns the wait's answer.
    fn wait_while(
        screen: &Screen,
        until: WaitUntil,
        timeout: Duration,
        watched: Option<FrameRegion>,
        paint: impl FnOnce() + Send + 'static,
    ) -> Result<String, String> {
        let painter = thread::spawn(paint);
        let answer = screen
            .session
            .wait_within(until, timeout, watched, &call(&|| false));
        painter.join().unwrap();
        answer
    }

    fn seconds_in(answer: &str) -> f64 {
        answer
            .split_whitespace()
            .find_map(|word| word.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no seconds in {answer:?}"))
    }

    #[test]
    fn a_wait_for_a_change_returns_when_the_picture_changes() {
        let (driver, screen) = desktop(320, 200);
        let painted = driver.clone();
        let changed = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_secs(5),
            None,
            move || {
                thread::sleep(Duration::from_millis(200));
                painted.paint((10, 10), (5, 5), [255, 255, 255]);
            },
        )
        .unwrap();
        assert!(
            changed.starts_with("The screen changed after "),
            "{changed}"
        );
        let after = seconds_in(&changed);
        assert!((0.1..2.0).contains(&after), "{changed}");

        let quiet = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_millis(300),
            None,
            || {},
        )
        .unwrap();
        assert_eq!(quiet, "The screen did not change within 0.3 s.");

        // A resize is a change.
        let resized = driver.clone();
        let turned = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_secs(5),
            None,
            move || {
                thread::sleep(Duration::from_millis(100));
                resized.resize(200, 320);
            },
        )
        .unwrap();
        assert!(turned.starts_with("The screen changed after "), "{turned}");
    }

    #[test]
    fn a_wait_on_a_region_sees_only_that_region() {
        let (driver, screen) = desktop(320, 200);
        // The frame is the screen itself here: 320x200 is under the long edge.
        let watched = region(100.0, 100.0, 50.0, 50.0);
        let outside = driver.clone();
        let unchanged = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_millis(400),
            Some(watched),
            move || {
                thread::sleep(Duration::from_millis(100));
                outside.paint((0, 0), (50, 50), [255, 0, 0]);
            },
        )
        .unwrap();
        assert_eq!(unchanged, "The screen did not change within 0.4 s.");
        let inside = driver.clone();
        let changed = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_secs(5),
            Some(watched),
            move || {
                thread::sleep(Duration::from_millis(100));
                inside.paint((120, 120), (2, 2), [0, 255, 0]);
            },
        )
        .unwrap();
        assert!(
            changed.starts_with("The screen changed after "),
            "{changed}"
        );
        // A region in a frame the screen no longer has is refused, as input would be.
        screen.session.screenshot_within(&call(&|| false)).unwrap();
        driver.resize(200, 320);
        let refused = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_secs(1),
                Some(watched),
                &call(&|| false),
            )
            .unwrap_err();
        assert!(refused.starts_with("The screen changed size"), "{refused}");
    }

    #[test]
    fn a_wait_for_stillness_returns_half_a_second_after_the_last_change() {
        let (driver, screen) = desktop(320, 200);
        let busy = driver.clone();
        let still = wait_while(
            &screen,
            WaitUntil::Still,
            Duration::from_secs(5),
            None,
            move || {
                for step in 0..15u8 {
                    busy.paint((u32::from(step) * 5, 0), (5, 5), [step * 10, 0, 0]);
                    thread::sleep(Duration::from_millis(20));
                }
            },
        )
        .unwrap();
        assert!(still.starts_with("The screen held still from "), "{still}");
        let from = seconds_in(&still);
        assert!((0.1..1.5).contains(&from), "{still}");

        let restless = driver.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let painter = thread::spawn(move || {
            let mut step = 0u32;
            while !stopping.load(Ordering::SeqCst) {
                restless.paint((step % 300, 50), (3, 3), [(step % 250) as u8, 9, 9]);
                step += 1;
                thread::sleep(Duration::from_millis(15));
            }
        });
        let kept = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_millis(700),
                None,
                &call(&|| false),
            )
            .unwrap();
        stop.store(true, Ordering::SeqCst);
        painter.join().unwrap();
        assert_eq!(kept, "The screen kept changing for 0.7 s.");
    }

    /// A wait for stillness given less time than the half second of quiet it needs, on a screen
    /// that never moves, says that the time was too short, never that the screen kept changing.
    #[test]
    fn a_still_wait_too_short_for_half_a_second_of_quiet_says_so() {
        let (_driver, screen) = desktop(320, 200);
        let short = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_millis(300),
                None,
                &call(&|| false),
            )
            .unwrap();
        assert_eq!(
            short,
            "The screen did not change in 0.3 s; that is too short to call it still, which takes half a second without a change. Give a timeout of 500 or more."
        );
        // The call's own time ends first: that is said instead of the timeout hint.
        let cut = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_secs(10),
                None,
                &within(&|| false, Duration::from_millis(2_300)),
            )
            .unwrap();
        assert!(cut.starts_with("The screen did not change in "), "{cut}");
        assert!(
            cut.ends_with(" (this call ran out of time first); that is too short to call it still, which takes half a second without a change."),
            "{cut}"
        );
        assert!(!cut.contains("kept changing"), "{cut}");
    }

    #[test]
    fn a_wait_stops_when_cancelled_and_keeps_inside_the_calls_time() {
        let (_driver, screen) = desktop(320, 200);
        let flag = Arc::new(AtomicBool::new(false));
        let raise = flag.clone();
        let raiser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            raise.store(true, Ordering::SeqCst);
        });
        let cancelled = || flag.load(Ordering::SeqCst);
        let started = Instant::now();
        let stopped = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_secs(10),
                None,
                &call(&cancelled),
            )
            .unwrap_err();
        raiser.join().unwrap();
        assert_eq!(stopped, "Cancelled while watching the screen.");
        assert!(started.elapsed() < Duration::from_secs(2));

        // Half a second left after the session's margin: the wait ends there, and says why.
        let short = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_secs(10),
                None,
                &within(&|| false, Duration::from_millis(2_500)),
            )
            .unwrap();
        assert!(
            short.starts_with("The screen did not change within "),
            "{short}"
        );
        assert!(
            short.ends_with(" (this call ran out of time first)."),
            "{short}"
        );
        assert!(seconds_in(&short) <= 0.5, "{short}");
    }

    /// A phone as adb and WebDriverAgent are: `frame(true)` captures anew, taking its time, and
    /// `frame(false)` is the last capture, which nothing refreshes while no viewer watches.
    /// The screen is one shade a test changes.
    struct Phone {
        shade: AtomicU8,
        capture_time: Mutex<Duration>,
        captures: AtomicUsize,
        last: Mutex<Option<Frame>>,
        /// Every [`Driver::frame`] call, as the `fresh` it asked for.
        requests: Mutex<Vec<bool>>,
    }

    impl Phone {
        fn new(shade: u8) -> Arc<Self> {
            Arc::new(Self {
                shade: AtomicU8::new(shade),
                capture_time: Mutex::new(Duration::ZERO),
                captures: AtomicUsize::new(0),
                last: Mutex::new(None),
                requests: Mutex::new(Vec::new()),
            })
        }

        /// The frames asked for since the first `after` requests, as the `fresh` each asked for.
        fn requests_after(&self, after: usize) -> Vec<bool> {
            self.requests.lock().unwrap()[after..].to_vec()
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn show(&self, shade: u8) {
            self.shade.store(shade, Ordering::SeqCst);
        }

        fn takes(&self, time: Duration) {
            *self.capture_time.lock().unwrap() = time;
        }

        fn captures(&self) -> usize {
            self.captures.load(Ordering::SeqCst)
        }

        fn capture(&self) -> Frame {
            let time = *self.capture_time.lock().unwrap();
            thread::sleep(time);
            let count = self.captures.fetch_add(1, Ordering::SeqCst) + 1;
            let frame = Frame {
                width: 64,
                height: 48,
                rgba: Arc::from([self.shade.load(Ordering::SeqCst), 40, 90, 255].repeat(64 * 48)),
                seq: count as u64,
                captured_at: Instant::now(),
            };
            *self.last.lock().unwrap() = Some(frame.clone());
            frame
        }
    }

    impl Driver for Phone {
        fn info(&self) -> crate::remote_control::driver::DriverInfo {
            crate::remote_control::driver::DriverInfo {
                platform: Platform::Android,
                label: "Fake phone".into(),
                width: 64,
                height: 48,
                has_ui_tree: false,
            }
        }

        fn frame(&self, fresh: bool) -> crate::remote_control::driver::DriverResult<Frame> {
            self.requests.lock().unwrap().push(fresh);
            if !fresh {
                if let Some(last) = self.last.lock().unwrap().clone() {
                    return Ok(last);
                }
            }
            Ok(self.capture())
        }

        fn key(
            &self,
            _key: &crate::remote_control::keys::Key,
            _down: Option<bool>,
        ) -> crate::remote_control::driver::DriverResult<()> {
            Ok(())
        }

        fn text(&self, _text: &str) -> crate::remote_control::driver::DriverResult<()> {
            Ok(())
        }

        fn close(&self) {}
    }

    /// A phone's driver keeps its last capture, and nothing refreshes it with no viewer: a
    /// wait that looked at that would never see the screen change. It captures anew for every
    /// look, and the answer says how often it did.
    #[test]
    fn a_wait_on_a_phone_captures_anew_so_a_change_shows_without_a_viewer() {
        let phone = Phone::new(10);
        let screen = connect(phone.clone(), Protocol::Adb, false);
        let before = phone.captures();
        let asked_before = phone.request_count();
        let changing = phone.clone();
        let changed = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_secs(5),
            None,
            move || {
                thread::sleep(Duration::from_millis(300));
                changing.show(200);
            },
        )
        .unwrap();
        assert!(
            changed.starts_with("The screen changed after ")
                && changed.contains(" s (looked every ~0.")
                && changed.ends_with(" s)."),
            "{changed}"
        );
        assert!(phone.captures() - before >= 2);
        // Every look asked for a fresh frame: the driver's other frame is its last capture.
        let asked = phone.requests_after(asked_before);
        assert!(
            asked.len() >= 2 && asked.iter().all(|fresh| *fresh),
            "{asked:?}"
        );
        assert!(seconds_in(&changed) < 2.0, "{changed}");
        // What the driver kept from before was not what the wait looked at.
        assert_eq!(
            phone.last.lock().unwrap().as_ref().unwrap().rgba[0],
            200,
            "the wait captured the new picture itself"
        );

        // A screen that does not change within the timeout says so, and how often it looked.
        let quiet = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_millis(700),
            None,
            || {},
        )
        .unwrap();
        assert!(
            quiet.starts_with("The screen did not change within 0.7 s (looked every ~0."),
            "{quiet}"
        );
    }

    /// On a phone a picture takes a capture, so "still" is three captures in a row showing the
    /// same picture, at least half a second apart from first to last — not half a second of
    /// quiet in a stream of frames.
    #[test]
    fn a_phone_is_still_after_three_identical_captures_spanning_half_a_second() {
        let phone = Phone::new(10);
        let screen = connect(phone.clone(), Protocol::Adb, false);
        let before = phone.captures();
        let started = Instant::now();
        let still = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_secs(5),
                None,
                &call(&|| false),
            )
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(500));
        assert_eq!(phone.captures() - before, 3, "{still}");
        assert!(
            still.starts_with("The screen held still from 0 s on (looked every ~0."),
            "{still}"
        );

        // A change restarts the count: the run is three captures of the picture it ended on.
        let busy = phone.clone();
        let settled = wait_while(
            &screen,
            WaitUntil::Still,
            Duration::from_secs(5),
            None,
            move || {
                thread::sleep(Duration::from_millis(100));
                busy.show(99);
            },
        )
        .unwrap();
        assert!(
            settled.starts_with("The screen held still from "),
            "{settled}"
        );
        assert!(seconds_in(&settled) > 0.0, "{settled}");

        // A screen that never stops is not called still, and a wait too short for the
        // captures it takes is told so — for a phone, in captures.
        let restless = phone.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let painter = thread::spawn(move || {
            let mut shade = 0u8;
            while !stopping.load(Ordering::SeqCst) {
                shade = shade.wrapping_add(7);
                restless.show(shade);
                thread::sleep(Duration::from_millis(20));
            }
        });
        let kept = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_millis(900),
                None,
                &call(&|| false),
            )
            .unwrap();
        stop.store(true, Ordering::SeqCst);
        painter.join().unwrap();
        assert!(
            kept.starts_with("The screen kept changing for 0.9 s (looked every ~0."),
            "{kept}"
        );

        let short = screen
            .session
            .wait_within(
                WaitUntil::Still,
                Duration::from_millis(400),
                None,
                &call(&|| false),
            )
            .unwrap();
        assert!(
            short.starts_with("The screen did not change in 0.4 s (looked every ~0.")
                && short.contains("); that is too short to call it still, which takes three captures in a row without a change, half a second apart at least (a capture takes about 0.")
                && short.contains(" s here). Give a timeout of ")
                && short.ends_with(" or more."),
            "{short}"
        );
    }

    /// A capture that could not end inside the call's time is not started, and a timeout
    /// shorter than a capture says that one picture compares to nothing.
    #[test]
    fn a_phone_wait_starts_no_capture_it_cannot_finish() {
        let phone = Phone::new(10);
        phone.takes(Duration::from_millis(400));
        let screen = connect(phone.clone(), Protocol::Adb, false);
        let before = phone.captures();
        let never = || false;
        // 1.2 s of working time: the first capture ends at 0.4 s, the second at 0.8 s, and a
        // third (another 0.5 s with the margin for variation) would end past the limit.
        let limited = within(
            &never,
            super::super::CALL_MARGIN + Duration::from_millis(1_200),
        );
        let started = Instant::now();
        let cut = screen
            .session
            .wait_within(WaitUntil::Still, Duration::from_secs(10), None, &limited)
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1_250),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(phone.captures() - before, 2, "{cut}");
        assert!(
            cut.contains("(this call ran out of time first; looked every ~0.4 s)")
                && cut.contains("too short to call it still")
                && !cut.contains("Give a timeout"),
            "{cut}"
        );

        // No time for even the first capture: nothing is captured.
        let before = phone.captures();
        let late = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_secs(5),
                None,
                &within(
                    &never,
                    super::super::CALL_MARGIN + Duration::from_millis(300),
                ),
            )
            .unwrap_err();
        assert!(
            late.starts_with("Ran out of time before watching the screen"),
            "{late}"
        );
        assert_eq!(phone.captures(), before);

        // A timeout of a tenth of a second is shorter than a capture: one picture, no verdict.
        phone.takes(Duration::from_millis(500));
        let one = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_millis(100),
                None,
                &call(&|| false),
            )
            .unwrap();
        assert!(
            one.starts_with("Only one picture of the screen was taken within 0.1 s, so nothing could be compared: a capture takes about 0.5 s here. Give a timeout of "),
            "{one}"
        );

        // A stop is heard between captures.
        phone.takes(Duration::from_millis(100));
        let flag = Arc::new(AtomicBool::new(false));
        let raise = flag.clone();
        let raiser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400));
            raise.store(true, Ordering::SeqCst);
        });
        let cancelled = || flag.load(Ordering::SeqCst);
        let stopped = screen
            .session
            .wait_within(
                WaitUntil::Change,
                Duration::from_secs(10),
                None,
                &call(&cancelled),
            )
            .unwrap_err();
        raiser.join().unwrap();
        assert_eq!(stopped, "Cancelled while watching the screen.");
    }

    /// The region a phone wait watches is the same region maths as a desktop's: a change
    /// outside it is no change.
    #[test]
    fn a_phone_wait_on_a_region_sees_only_that_region() {
        let (driver, screen) = {
            let driver = FakeDriver::new(Platform::Android, 320, 200);
            let screen = connect(driver.clone(), Protocol::Adb, false);
            (driver, screen)
        };
        let watched = region(100.0, 100.0, 50.0, 50.0);
        let outside = driver.clone();
        let unchanged = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_millis(700),
            Some(watched),
            move || {
                thread::sleep(Duration::from_millis(100));
                outside.paint((0, 0), (50, 50), [255, 0, 0]);
            },
        )
        .unwrap();
        assert!(
            unchanged.starts_with("The screen did not change within 0.7 s"),
            "{unchanged}"
        );
        let inside = driver.clone();
        let changed = wait_while(
            &screen,
            WaitUntil::Change,
            Duration::from_secs(5),
            Some(watched),
            move || {
                thread::sleep(Duration::from_millis(100));
                inside.paint((120, 120), (2, 2), [0, 255, 0]);
            },
        )
        .unwrap();
        assert!(
            changed.starts_with("The screen changed after "),
            "{changed}"
        );
    }

    /// A close-up works on a phone as on a desktop: a fresh capture, the region of the
    /// coordinate frame, larger — and the frame the model's input is read in stays the
    /// screenshot's.
    #[test]
    fn a_close_up_works_on_phones_too() {
        for (platform, protocol) in [
            (Platform::Android, Protocol::Adb),
            (Platform::Ios, Protocol::Wda),
        ] {
            let driver = FakeDriver::new(platform, 1080, 2400);
            // The top-left quarter of the screen is red.
            driver.paint((0, 0), (540, 1200), [220, 30, 30]);
            let screen = connect(driver.clone(), protocol, false);
            let session = &screen.session;
            // The frame is 576x1280: the region is the top-left quarter of it.
            let asked_before = driver.frame_requests().len();
            let shot = session
                .zoom_within(region(0.0, 0.0, 288.0, 640.0), &call(&|| false))
                .unwrap();
            // A phone's driver captures anew only when asked for a fresh frame, and keeps its
            // last capture otherwise: a close-up that asked for the other would show the screen
            // as it was when someone last looked.
            let asked = driver.frame_requests()[asked_before..].to_vec();
            assert!(
                !asked.is_empty() && asked.iter().all(|fresh| *fresh),
                "{platform:?}: {asked:?}"
            );
            assert_eq!(
                shot.screen_region,
                Rect {
                    x: 0,
                    y: 0,
                    width: 540,
                    height: 1200
                }
            );
            assert_eq!((shot.width, shot.height), (576, 1280), "{platform:?}");
            let (width, height, pixels) = decode(&shot.jpeg);
            assert_eq!((width, height), (576, 1280));
            let middle = (640 * 576 + 288) * 3;
            assert!(
                pixels[middle] > 180 && pixels[middle + 1] < 80,
                "{:?}",
                &pixels[middle..middle + 3]
            );
            assert_eq!(screen.last_shown(), None, "a close-up is no frame");

            session.screenshot_within(&call(&|| false)).unwrap();
            assert_eq!(screen.last_shown(), Some((576, 1280)));
            // A phone turned since the screenshot is refused as a desktop resized would be.
            driver.resize(2400, 1080);
            let refused = session
                .zoom_within(region(0.0, 0.0, 100.0, 100.0), &call(&|| false))
                .unwrap_err();
            assert!(refused.starts_with("The screen changed size"), "{refused}");
            assert!(
                refused.contains("Take a new preview_screenshot or preview_snapshot first")
                    || refused.contains("Take a new preview_screenshot first"),
                "{refused}"
            );
        }
    }

    #[test]
    fn durations_read_as_whole_seconds_or_tenths() {
        assert_eq!(seconds(Duration::from_secs(5)), "5 s");
        assert_eq!(seconds(Duration::from_millis(1_300)), "1.3 s");
        assert_eq!(seconds(Duration::from_millis(300)), "0.3 s");
        assert_eq!(seconds(Duration::ZERO), "0 s");
        assert_eq!(capitalized("launching com.foo"), "Launching com.foo");
    }
}
