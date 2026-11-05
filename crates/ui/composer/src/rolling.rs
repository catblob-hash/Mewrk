//! `RollingNumber`: digits turn like the wheels of a lock when the value changes, and it is plain text between rolls. A copy of the
//! tasks pane's, until one lands in `ui/common`.
//! TODO(ui-common): share this with `crates/panes/tasks/src/widgets.rs`.

use mewrk_client::prelude::*;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
enum RollSlot {
    Digit { from: u8, to: u8 },
    Glyph(String),
}

/// Outgoing and incoming text paired from the right, the way an odometer's wheels line up. A column that was not a digit arrives
/// showing its value; runs of non-digits stay whole, keeping their kerning.
fn rolling_slots(previous: &str, next: &str) -> Vec<RollSlot> {
    let (prev, chars): (Vec<char>, Vec<char>) = (previous.chars().collect(), next.chars().collect());
    let mut slots: Vec<RollSlot> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        let column = chars.len() - 1 - i;
        let before = prev.len().checked_sub(column + 1).map(|k| prev[k]);
        match c.to_digit(10) {
            Some(to) => {
                let from = before.and_then(|b| b.to_digit(10)).unwrap_or(to);
                slots.push(RollSlot::Digit { from: from as u8, to: to as u8 });
            }
            None => match slots.last_mut() {
                Some(RollSlot::Glyph(run)) => run.push(c),
                _ => slots.push(RollSlot::Glyph(c.to_string())),
            },
        }
    }
    slots
}

const ROLL_SECONDS: f32 = 0.42;

struct RollState {
    settled: String,
    roll: Option<(Vec<RollSlot>, Instant)>,
}

pub fn rolling_number(key: impl std::hash::Hash, value: &str, ty: TypeStyle, color: Color) -> E {
    let state = use_state(("composer-rolling-number", key), || RollState { settled: value.to_owned(), roll: None });
    {
        let mut s = state.borrow_mut();
        if s.settled != value {
            let slots = rolling_slots(&s.settled, value);
            s.settled = value.to_owned();
            s.roll = (!fluent::reduced_motion()).then(|| (slots, Instant::now()));
        }
        if s.roll.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs_f32() > ROLL_SECONDS + 0.08) {
            s.roll = None;
        }
    }
    let Some((slots, at)) = state.borrow().roll.clone() else {
        return text(value.to_owned()).ty(ty).color(color).nowrap().pass_through();
    };
    let measured = value.to_owned();
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let t = (at.elapsed().as_secs_f32() / ROLL_SECONDS).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        ui.with_clip(r, |ui| {
            let mut x = r.x;
            for slot in &slots {
                match slot {
                    RollSlot::Glyph(run) => x += ui.label(x, r.y, run, &ty, color),
                    RollSlot::Digit { from, to } => {
                        // Always forward, so a carry rolls 9 to 0 rather than back through the wheel.
                        let landing = *from as f32 + ((*to as i32 - *from as i32 + 10) % 10) as f32;
                        let offset = *from as f32 + (landing - *from as f32) * eased;
                        let (cell, fraction) = (offset.floor(), offset.fract());
                        let cw = ui.measure("0", &ty);
                        let digit = |v: f32| ((v as i32).rem_euclid(10)).to_string();
                        ui.label(x, r.y - fraction * ty.line, &digit(cell), &ty, color);
                        if fraction > 0.0 {
                            ui.label(x, r.y + (1.0 - fraction) * ty.line, &digit(cell + 1.0), &ty, color);
                        }
                        x += cw;
                    }
                }
            }
        });
        ui.wake_after(1.0 / 60.0);
    })
    .aria_label(value.to_owned())
    .measure(move |_, ui, _, _| (ui.measure(&measured, &ty), ty.line))
    .shrink0()
    .pass_through()
}
