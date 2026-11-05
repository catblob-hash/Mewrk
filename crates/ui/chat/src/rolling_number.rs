//! `components/RollingNumber.tsx`. The tasks pane's copy covers upstream's behaviour: plain text between rolls that sets the
//! width, digits paired from the right, a carry rolling forward and the overlay coming down on a timer. Upstream's
//! `rollCanPlay` has no counterpart since nothing paints while hidden and a roll is timed by the wall clock, so a roll that
//! ran out unseen is simply over when the window comes back.

pub use mewrk_pane_tasks::widgets::{rolling_number, rolling_slots, RollSlot};
