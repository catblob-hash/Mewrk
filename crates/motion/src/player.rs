//! Playing a background on a clock: what is due now goes to the GPU, and the answer is when to look again.
//!
//! The frame loop asks, rather than a timer pushing frames at it, so nothing runs while the window can't be seen and a frame that came
//! late is simply skipped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluent::{queue_live_image, release_image, write_image, ImageId};

use crate::flipbook::Flipbook;

/// What a look at the clock found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// A new frame went to the GPU, so the window is out of date.
    pub changed: bool,
    /// When to look again. `None` when nothing is going to change by itself.
    pub next: Option<Instant>,
    /// What is on screen has to be drawn from something else now, so the last frame's scene won't do.
    pub rebuild: bool,
}

/// How long a video may stay paused before its decoder and frames are given back. Picking it up again starts where it stopped.
#[cfg(target_os = "macos")]
const PAUSE_KEEPS: Duration = Duration::from_secs(10);

pub struct Player {
    kind: Kind,
    /// When it was paused, and `None` while it plays.
    paused_since: Option<Instant>,
    /// When a look at the clock last moved it, to pick up from without a jump.
    position_us: u64,
}

enum Kind {
    Flip(FlipPlayer),
    #[cfg(target_os = "macos")]
    Video(mac::VideoPlayer),
}

struct FlipPlayer {
    book: Arc<Flipbook>,
    image: ImageId,
    /// The moment time zero was.
    start: Option<Instant>,
    shown: Option<u32>,
}

impl FlipPlayer {
    fn tick(&mut self, now: Instant) -> (Tick, u64) {
        let start = *self.start.get_or_insert(now);
        let time = now.saturating_duration_since(start).as_micros() as u64;
        let Some(index) = self.book.index_at(time) else { return (Tick { next: Some(now + Duration::from_millis(16)), ..Tick::default() }, time) };
        let mut changed = false;
        if self.shown != Some(index) {
            if let Some(frame) = self.book.frame(index) {
                let (width, height) = self.book.size();
                write_image(self.image, 0, 0, width, height, frame);
                changed = true;
                self.book.prefetch((index + 1) % self.book.count().max(1));
            }
            self.shown = Some(index);
        }
        let total = self.book.duration_us().max(1);
        let lap = time / total;
        let next = lap * total + self.book.end_us(index).unwrap_or(total);
        (Tick { changed, next: Some(start + Duration::from_micros(next)), rebuild: false }, time % total)
    }
}

impl Player {
    /// Plays a book of whole frames from its first, on a texture of its own.
    pub fn flipbook(book: Arc<Flipbook>) -> Self {
        let (width, height) = book.size();
        let image = queue_live_image(width, height);
        Self { kind: Kind::Flip(FlipPlayer { book, image, start: None, shown: None }), paused_since: None, position_us: 0 }
    }

    /// Plays a video file on a loop from `from`, decoded at `size`. Its frames are on their way as soon as this returns.
    #[cfg(target_os = "macos")]
    pub fn video(path: std::path::PathBuf, info: crate::VideoInfo, size: (u32, u32)) -> Self {
        Self { kind: Kind::Video(mac::VideoPlayer::start(path, info, size, 0.0)), paused_since: None, position_us: 0 }
    }

    /// The texture on screen and its size, from the first frame on.
    pub fn image(&self) -> Option<(ImageId, (u32, u32))> {
        match &self.kind {
            Kind::Flip(flip) => flip.shown.map(|_| (flip.image, flip.book.size())),
            #[cfg(target_os = "macos")]
            Kind::Video(video) => video.image(),
        }
    }

    /// Whether all of it is known to be solid: a video's frames are, whatever the one before was, as long as its first frame had no holes.
    pub fn is_flipbook_opaque(&self) -> Option<bool> {
        match &self.kind {
            Kind::Flip(flip) => Some(flip.book.is_opaque()),
            #[cfg(target_os = "macos")]
            Kind::Video(_) => None,
        }
    }

    /// The time of what is showing, within the loop.
    pub fn position_us(&self) -> u64 {
        self.position_us
    }

    /// A video output of a larger size, should the window have grown past the one it is playing at.
    #[cfg(target_os = "macos")]
    pub fn resize(&mut self, size: (u32, u32)) {
        if let Kind::Video(video) = &mut self.kind {
            video.resize(size, self.position_us as f64 / 1e6);
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn resize(&mut self, _size: (u32, u32)) {}

    /// Stops or starts playing: the window can't be seen, or nothing it shows comes from this.
    pub fn set_paused(&mut self, paused: bool, now: Instant) {
        match (paused, self.paused_since) {
            (true, None) => self.paused_since = Some(now),
            (false, Some(since)) => {
                self.paused_since = None;
                match &mut self.kind {
                    Kind::Flip(flip) => {
                        if let Some(start) = &mut flip.start {
                            *start += now.saturating_duration_since(since);
                        }
                    }
                    #[cfg(target_os = "macos")]
                    Kind::Video(video) => video.resume(self.position_us as f64 / 1e6),
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, now: Instant) -> Tick {
        if let Some(since) = self.paused_since {
            #[cfg(target_os = "macos")]
            if let Kind::Video(video) = &mut self.kind {
                if now.saturating_duration_since(since) >= PAUSE_KEEPS {
                    video.stand_down();
                } else {
                    // One more look, to hand the decoder back if this goes on.
                    return Tick { next: Some(since + PAUSE_KEEPS), ..Tick::default() };
                }
            }
            let _ = since;
            return Tick::default();
        }
        match &mut self.kind {
            Kind::Flip(flip) => {
                let (tick, position) = flip.tick(now);
                self.position_us = position;
                tick
            }
            #[cfg(target_os = "macos")]
            Kind::Video(video) => {
                let tick = video.tick(now);
                self.position_us = video.position_us();
                tick
            }
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        match &self.kind {
            Kind::Flip(flip) => release_image(flip.image),
            #[cfg(target_os = "macos")]
            Kind::Video(video) => release_image(video.texture()),
        }
    }
}

/// Stops a worker that holds this, whichever way it is waiting.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use fluent::{queue_surface_image, show_surface, ImageId};

    use super::Tick;
    use crate::video::VideoReader;
    use crate::VideoInfo;

    /// Frames decoded ahead. Each holds a surface as large as the output, so few, but enough to cover a loop's start over.
    const AHEAD: usize = 4;
    /// A frame later than this means the decoder stalled: carrying on from here beats rushing through what was missed.
    const LATE: Duration = Duration::from_millis(120);
    /// Past this a window shows no difference, and every frame redraws the window.
    const SHORTEST_FRAME: f64 = 1.0 / 62.0;
    /// How soon to look again when no frame has arrived yet.
    const WAITING: Duration = Duration::from_millis(4);

    struct Decoded {
        /// Seconds since this player began, counting every lap.
        stream: f64,
        time: f64,
        frame: crate::video::VideoFrame,
    }

    #[derive(Default)]
    struct Queue {
        frames: VecDeque<Decoded>,
        stop: bool,
        /// The decoder has stopped for good: at the end of something too short to loop, or because the video can't be read further.
        finished: bool,
    }

    #[derive(Default)]
    struct Shared {
        queue: Mutex<Queue>,
        room: Condvar,
    }

    impl Shared {
        fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
            self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        fn stop(&self) {
            let mut queue = self.lock();
            queue.stop = true;
            queue.frames.clear();
            self.room.notify_all();
        }
    }

    /// Reads laps of the video into the queue, waiting whenever it is full, until told to stop.
    fn decode(shared: &Shared, path: &PathBuf, info: VideoInfo, size: (u32, u32), from: f64) {
        let mut from = from;
        let mut lap_start = 0.0;
        let mut last_kept = f64::NEG_INFINITY;
        loop {
            let mut frames_this_lap = 0;
            let Ok(mut reader) = VideoReader::open(path, Some(size), from) else { return };
            while let Some(frame) = reader.next_frame() {
                let stream = lap_start + frame.time;
                if stream - last_kept < SHORTEST_FRAME {
                    continue;
                }
                last_kept = stream;
                frames_this_lap += 1;
                let mut queue = shared.lock();
                while queue.frames.len() >= AHEAD && !queue.stop {
                    queue = shared.room.wait(queue).unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if queue.stop {
                    return;
                }
                queue.frames.push_back(Decoded { stream, time: frame.time, frame });
            }
            // A video with too little in it to loop keeps its one frame on screen, and doesn't spin.
            if !reader.ended_cleanly() || frames_this_lap < 2 {
                return;
            }
            lap_start += info.duration.max(SHORTEST_FRAME);
            from = 0.0;
        }
    }

    pub(super) struct VideoPlayer {
        path: PathBuf,
        info: VideoInfo,
        size: (u32, u32),
        image: ImageId,
        shared: Option<Arc<Shared>>,
        /// The moment stream time zero was.
        base: Option<Instant>,
        shown: Option<(u32, u32)>,
        position_us: u64,
    }

    impl VideoPlayer {
        pub(super) fn start(path: PathBuf, info: VideoInfo, size: (u32, u32), from: f64) -> Self {
            let mut player = Self { path, info, size, image: queue_surface_image(), shared: None, base: None, shown: None, position_us: (from * 1e6) as u64 };
            player.spawn(from);
            player
        }

        fn spawn(&mut self, from: f64) {
            let shared = Arc::new(Shared::default());
            self.shared = Some(shared.clone());
            self.base = None;
            let (path, info, size) = (self.path.clone(), self.info, self.size);
            let _ = std::thread::Builder::new().name("mewrk-video".to_owned()).spawn(move || {
                decode(&shared, &path, info, size, from);
                shared.lock().finished = true;
            });
        }

        pub(super) fn image(&self) -> Option<(ImageId, (u32, u32))> {
            self.shown.map(|size| (self.image, size))
        }

        pub(super) fn texture(&self) -> ImageId {
            self.image
        }

        pub(super) fn position_us(&self) -> u64 {
            self.position_us
        }

        pub(super) fn resize(&mut self, size: (u32, u32), from: f64) {
            if size != self.size {
                self.size = size;
                if let Some(shared) = self.shared.take() {
                    shared.stop();
                    self.spawn(from);
                }
            }
        }

        /// Paused long enough: the decoder goes, and with it the frames it held.
        pub(super) fn stand_down(&mut self) {
            if let Some(shared) = self.shared.take() {
                shared.stop();
            }
        }

        pub(super) fn resume(&mut self, from: f64) {
            self.base = None;
            if self.shared.is_none() {
                self.spawn(from);
            }
        }

        pub(super) fn tick(&mut self, now: Instant) -> Tick {
            let Some(shared) = self.shared.clone() else { return Tick::default() };
            let mut queue = shared.lock();
            let mut newest = None;
            while let Some(front) = queue.frames.front() {
                let due = match self.base {
                    Some(base) => base + Duration::from_secs_f64(front.stream),
                    // Nothing to keep time with yet: the first frame sets the clock.
                    None => now,
                };
                if due > now {
                    break;
                }
                if self.base.is_none() || now.saturating_duration_since(due) > LATE {
                    self.base = now.checked_sub(Duration::from_secs_f64(front.stream)).or(Some(now));
                }
                newest = queue.frames.pop_front();
            }
            let next = match queue.frames.front() {
                Some(front) => self.base.map(|base| base + Duration::from_secs_f64(front.stream)),
                // What is on screen stays, with nothing more to come.
                None if queue.finished => None,
                None => Some(now + WAITING),
            };
            let had_frame = newest.is_some();
            if had_frame {
                shared.room.notify_all();
            }
            drop(queue);
            let mut changed = false;
            if let Some(decoded) = newest {
                self.position_us = (decoded.time * 1e6) as u64;
                let size = (decoded.frame.width, decoded.frame.height);
                if let Some(surface) = decoded.frame.into_surface() {
                    show_surface(self.image, surface);
                    self.shown = Some(size);
                    changed = true;
                }
            }
            Tick { changed, next, rebuild: false }
        }
    }

    impl Drop for VideoPlayer {
        fn drop(&mut self) {
            if let Some(shared) = self.shared.take() {
                shared.stop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flipbook::FlipWriter;

    fn book(name: &str, ends: &[u64]) -> Arc<Flipbook> {
        let dir = std::env::temp_dir().join(format!("mewrk-player-{name}-{}-{:x}", std::process::id(), fastrand::u64(..)));
        let mut writer = FlipWriter::create(&dir.join("book.flip"), 1, 1, ends.len() as u32).unwrap();
        for (index, end) in ends.iter().enumerate() {
            writer.push(&[index as u8; 4], *end);
        }
        writer.finish().unwrap()
    }

    #[test]
    fn a_flipbook_shows_each_frame_once_says_when_the_next_is_due_and_loops() {
        let mut player = Player::flipbook(book("flip", &[100_000, 300_000]));
        let start = Instant::now();
        assert_eq!(player.image(), None, "nothing to show until the first look");
        let first = player.tick(start);
        assert!(first.changed);
        assert_eq!(first.next, Some(start + Duration::from_millis(100)));
        assert!(player.image().is_some());
        let again = player.tick(start + Duration::from_millis(50));
        assert!(!again.changed, "the same frame costs nothing");
        let second = player.tick(start + Duration::from_millis(100));
        assert!(second.changed);
        assert_eq!(second.next, Some(start + Duration::from_millis(300)));
        let lap = player.tick(start + Duration::from_millis(310));
        assert!(lap.changed);
        assert_eq!(lap.next, Some(start + Duration::from_millis(400)));
        assert_eq!(player.position_us(), 10_000);
    }

    #[test]
    fn a_paused_flipbook_asks_for_nothing_and_picks_up_where_it_stopped() {
        let mut player = Player::flipbook(book("pause", &[100_000, 200_000]));
        let start = Instant::now();
        player.tick(start);
        player.set_paused(true, start + Duration::from_millis(50));
        assert_eq!(player.tick(start + Duration::from_millis(500)), Tick::default());
        player.set_paused(false, start + Duration::from_secs(10));
        let resumed = player.tick(start + Duration::from_secs(10));
        assert!(!resumed.changed, "it was still on its first frame");
        assert_eq!(resumed.next, Some(start + Duration::from_secs(10) + Duration::from_millis(50)));
    }

    #[cfg(target_os = "macos")]
    mod video {
        use super::*;
        use std::path::PathBuf;

        fn fixture() -> PathBuf {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/counting.mp4")
        }

        /// Runs the clock for real until `until` says so, or a few seconds.
        fn play(player: &mut Player, until: impl Fn(&Player, Tick) -> bool) -> u32 {
            let begun = Instant::now();
            let mut changes = 0;
            while begun.elapsed() < Duration::from_secs(5) {
                let tick = player.tick(Instant::now());
                changes += u32::from(tick.changed);
                if until(player, tick) {
                    return changes;
                }
                if let Some(next) = tick.next {
                    std::thread::sleep(next.saturating_duration_since(Instant::now()).min(Duration::from_millis(20)));
                }
            }
            panic!("it never got there, {changes} frames in");
        }

        #[test]
        fn a_video_plays_on_time_and_goes_round_again() {
            let path = fixture();
            let info = crate::video::probe(&path).unwrap();
            let mut player = Player::video(path, info, (64, 36));
            let begun = Instant::now();
            play(&mut player, |player, _| player.image().is_some());
            assert_eq!(player.image().unwrap().1, (64, 36));
            let first = Instant::now();
            let seen = play(&mut player, |player, _| player.position_us() >= 1_000_000);
            let spent = first.elapsed().as_secs_f64();
            assert!((0.8..1.5).contains(&spent), "one second of video took {spent}, from {begun:?}");
            assert!(seen >= 6, "only {seen} of 8 frames");
            play(&mut player, |player, _| player.position_us() < 500_000);
        }

        #[test]
        fn a_paused_video_gives_back_its_decoder_and_starts_again_from_where_it_was() {
            let path = fixture();
            let info = crate::video::probe(&path).unwrap();
            let mut player = Player::video(path, info, (64, 36));
            play(&mut player, |player, _| player.position_us() >= 500_000);
            let at = player.position_us();
            let now = Instant::now();
            player.set_paused(true, now);
            assert_eq!(player.tick(now), Tick { next: Some(now + PAUSE_KEEPS), ..Tick::default() }, "it comes back to give the decoder up");
            assert_eq!(player.tick(now + PAUSE_KEEPS + Duration::from_secs(1)), Tick::default(), "and then asks for nothing");
            player.set_paused(false, Instant::now());
            play(&mut player, |player, tick| tick.changed && player.position_us() >= at);
        }

        #[test]
        fn a_video_too_short_to_loop_holds_its_frame_and_stops_asking() {
            let dir = std::env::temp_dir().join(format!("mewrk-player-short-{}-{:x}", std::process::id(), fastrand::u64(..)));
            std::fs::create_dir_all(&dir).unwrap();
            let path = fixture();
            let info = crate::video::probe(&path).unwrap();
            // Read one frame in, then ask to start past the end: nothing more comes.
            let mut player = Player::video(path, crate::VideoInfo { duration: 0.0, ..info }, (64, 36));
            play(&mut player, |player, _| player.image().is_some());
            // It loops on as long as frames come at all, so this is only about the worker going away.
            drop(player);

            let mut none = Player::video(dir.join("missing.mp4"), info, (64, 36));
            let begun = Instant::now();
            loop {
                let tick = none.tick(Instant::now());
                if tick.next.is_none() {
                    break;
                }
                assert!(begun.elapsed() < Duration::from_secs(5), "it never stopped asking");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(none.image(), None);
        }
    }
}
