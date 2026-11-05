//! Port of `lib/streamReveal.ts`: the streaming reveal.
//!
//! The text each commit adds sweeps in from left to right, so a stream reads as being written instead of as runs of
//! text blinking into place ten times a second.
//!
//! "What this commit added" is decided on the text as rendered, not on the source. A Markdown source and its
//! rendering are not the same string (`**bo` renders as `**bo` until the closing `**` turns it into bold `bo`), and
//! the text that changed is what has to move — so the split is the longest prefix this render shares with the one
//! already on screen.
//!
//! Upstream wraps the new text in CSS-animated spans; what survives without a DOM is the split and each text run's
//! share of the sweep, which a painter applies as an alpha mask.

use super::model_stream::MODEL_STREAM_COMMIT_INTERVAL_MS;

/// One sweep lasts one commit, so each commit's text finishes arriving just as the next begins and the stream moves
/// continuously.
pub const STREAM_REVEAL_MS: u64 = MODEL_STREAM_COMMIT_INTERVAL_MS;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamRevealSplit {
    /// The text the split was taken against.
    pub text: String,
    /// How much of `text` was already on screen, in bytes on a character boundary; everything after it is new.
    pub settled: usize,
    /// Alternates each time new text arrives, which is how upstream's animation knows to replay.
    pub tick: u8,
}

fn common_prefix_length(left: &str, right: &str) -> usize {
    // Never split a character: half of one is not a glyph to sweep.
    left.char_indices().zip(right.chars()).take_while(|((_, l), r)| l == r).last().map_or(0, |((at, l), _)| at + l.len_utf8())
}

/// The split for `text`, given the split last put on screen.
///
/// Nothing on screen yet means everything is new. The same text again — a
/// render for some other reason — keeps the split it had, so a sweep still in
/// flight is not cut short.
pub fn next_stream_reveal_split(previous: Option<&StreamRevealSplit>, text: &str) -> StreamRevealSplit {
    let Some(previous) = previous else { return StreamRevealSplit { text: text.to_owned(), settled: 0, tick: 0 } };
    if previous.text == text {
        return previous.clone();
    }
    StreamRevealSplit { text: text.to_owned(), settled: common_prefix_length(&previous.text, text), tick: 1 - previous.tick }
}

/// `.stream-fresh`'s mask: how far the fresh text has been uncovered at `x` into it. The edge runs from 0 to the text's width plus two
/// ems, in one sweep, and everything more than two ems behind it is fully shown. `x`, `width` and `em` in one unit.
pub fn fresh_alpha(x: f32, width: f32, em: f32, progress: f32) -> f32 {
    let band = 2.0 * em;
    let edge = progress.clamp(0.0, 1.0) * (width + band);
    ((edge - x) / band.max(0.001)).clamp(0.0, 1.0)
}

/// A slot's share of one sweep: when its new text starts and how long it takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RevealTiming {
    /// Where the fresh text starts inside the slot, in bytes.
    pub cut: usize,
    pub delay_ms: f64,
    pub duration_ms: f64,
}

/// The new text spread across one sweep in reading order: each run starts when the text before it has finished, and
/// takes its share of the commit, so text that crosses a paragraph or a bold run still arrives as one stroke.
/// `slots` are text runs as `(offset in the rendered text, length)`; a run with nothing new gets `None`.
pub fn reveal_timings(split: &StreamRevealSplit, slots: &[(usize, usize)]) -> Vec<Option<RevealTiming>> {
    let fresh = split.text.len().saturating_sub(split.settled);
    slots
        .iter()
        .map(|&(offset, length)| {
            if fresh == 0 || offset + length <= split.settled {
                return None;
            }
            let cut = split.settled.saturating_sub(offset);
            let start = offset + cut - split.settled;
            let tail = length - cut;
            Some(RevealTiming {
                cut,
                delay_ms: start as f64 / fresh as f64 * STREAM_REVEAL_MS as f64,
                duration_ms: tail as f64 / fresh as f64 * STREAM_REVEAL_MS as f64,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_on_screen_means_everything_is_new() {
        assert_eq!(next_stream_reveal_split(None, "abc"), StreamRevealSplit { text: "abc".into(), settled: 0, tick: 0 });
    }

    #[test]
    fn the_same_text_keeps_the_split_so_a_sweep_is_not_cut_short() {
        let first = StreamRevealSplit { text: "abc".into(), settled: 2, tick: 1 };
        assert_eq!(next_stream_reveal_split(Some(&first), "abc"), first);
    }

    #[test]
    fn new_text_splits_on_the_shared_prefix_and_flips_the_tick() {
        let first = next_stream_reveal_split(None, "**bo");
        // Rendering changed what was on screen, so only what still matches counts as settled.
        let second = next_stream_reveal_split(Some(&first), "**bold");
        assert_eq!((second.settled, second.tick), (4, 1));
        let third = next_stream_reveal_split(Some(&second), "bold");
        assert_eq!((third.settled, third.tick), (0, 0));
    }

    #[test]
    fn never_splits_a_character() {
        let first = next_stream_reveal_split(None, "你好");
        let second = next_stream_reveal_split(Some(&first), "你们");
        assert_eq!(second.settled, "你".len());
    }

    #[test]
    fn the_sweep_is_shared_across_runs_in_reading_order() {
        let split = StreamRevealSplit { text: "aaaabbbb".into(), settled: 4, tick: 0 };
        let timings = reveal_timings(&split, &[(0, 2), (2, 4), (6, 2)]);
        assert_eq!(timings[0], None);
        assert_eq!(timings[1], Some(RevealTiming { cut: 2, delay_ms: 0.0, duration_ms: 50.0 }));
        assert_eq!(timings[2], Some(RevealTiming { cut: 0, delay_ms: 50.0, duration_ms: 50.0 }));
    }
}
