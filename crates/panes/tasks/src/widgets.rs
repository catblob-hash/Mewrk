//! Small pieces upstream had in `Common`, `RollingNumber`, `PathText`, `PageTabs` and the `pulse-text` utility, as the tasks
//! pane uses them.
//!
//! `page_tabs` is the shared strip under this crate's two callers.

use std::time::Instant;

use mewrk_client::prelude::*;

use fluent::{TypeStyle, Weight};

pub const fn ts(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

pub const fn mono(size: f32, line: f32) -> TypeStyle {
    TypeStyle { size, line, weight: Weight::Regular, mono: true, italic: false, serif: false }
}

/// Asks for a frame after `secs` while it is on screen: `useNow`, and any other clock nothing else would move.
pub fn ticker(secs: f32) -> E {
    custom(move |_: &mut App, ui: &mut Ui<'_>, _| ui.wake_after(secs)).w(1.0).h(1.0).absolute().top(0.0).left(0.0).pass_through()
}

/// `LoaderCircle` turning once a second.
pub fn spinner(size: f32, color: Color) -> E {
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        ui.named_icon("LoaderCircle", r.x + r.w * 0.5, r.y + r.h * 0.5, size, color, (ui.time * 360.0) % 360.0);
        ui.wake_after(1.0 / 30.0);
    })
    .size(size, size)
    .shrink0()
    .pass_through()
}

/// A filled square: upstream's `<Square fill="currentColor">`, the stop glyph.
pub fn stop_glyph(color: Color) -> E {
    div().size(8.0, 8.0).radius(1.5).bg(color).pass_through()
}

const PULSE_SECONDS: f32 = 1.9;

/// One column of the sweep: `.pulse-text`'s gradient is 3.2 boxes wide with a highlight at its centre, half strength
/// outside 36% to 64% of it, and its position runs across the box in 1.9 seconds.
pub(crate) fn pulse_alpha(x: f32, width: f32, time: f32) -> f32 {
    let t = (time / PULSE_SECONDS).fract();
    let eased = t * t * (3.0 - 2.0 * t);
    let position = 1.0 - eased;
    let centre = 1.6 * width - 2.2 * width * position;
    let ramp = 0.448 * width.max(1.0);
    0.5 + 0.5 * (1.0 - (x - centre).abs() / ramp).clamp(0.0, 1.0)
}

/// A label that ends in an ellipsis, optionally sweeping (`pulse-text`) and optionally shortened as a path gives way
/// (`PathText`): `prefix` stays whole and `path` shortens in the middle.
pub fn fitted_label(prefix: &str, path: Option<&str>, ty: TypeStyle, color: Color, pulse: bool) -> E {
    let (prefix, path): (String, Option<String>) = (prefix.to_owned(), path.map(str::to_owned));
    let (measure_prefix, measure_path) = (prefix.clone(), path.clone());
    let said = a11y_active().then(|| format!("{prefix}{}", path.as_deref().unwrap_or("")));
    let label: E = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let y = r.y + (r.h - ty.line) * 0.5;
        let text = match &path {
            Some(path) => {
                let room = r.w - ui.advance_of(&prefix, &ty) - 0.5;
                let shown = elide_path(path, &mut |candidate| ui.measure(candidate, &ty) <= room);
                format!("{prefix}{shown}")
            }
            None => prefix.clone(),
        };
        let whole = format!("{prefix}{}", path.as_deref().unwrap_or(""));
        if text != whole || ui.measure(&text, &ty) > r.w {
            ui.line_was_cut(r, &whole);
        }
        if !pulse || fluent::reduced_motion() {
            ui.label_ellipsis(r.x, y, r.w, &text, &ty, color);
            return;
        }
        ui.wake_after(1.0 / 30.0);
        let truncate = ui.measure(&text, &ty) > r.w;
        let ell = ui.measure("…", &ty);
        let mut x = r.x;
        let mut buf = [0u8; 4];
        for c in text.chars() {
            let s = c.encode_utf8(&mut buf);
            let w = ui.advance_of(s, &ty);
            let shade = |x: f32, ui: &Ui<'_>| color.fade(pulse_alpha(x - r.x, r.w, ui.time));
            if truncate && x + w + ell > r.right() {
                let c = shade(x, ui);
                ui.label(x, y, "…", &ty, c);
                break;
            }
            let c = shade(x + w * 0.5, ui);
            ui.label(x, y, s, &ty, c);
            x += w;
        }
    })
    .measure(move |_, ui, _, _| {
        let w = ui.advance_of(&measure_prefix, &ty) + measure_path.as_deref().map_or(0.0, |p| ui.measure(p, &ty));
        (w, ty.line)
    })
    .min_w(0.0)
    .with_style(|s| s.flex_shrink = 1.0)
    .pass_through();
    match said {
        Some(words) => label.aria_label(words),
        None => label,
    }
}

const PATH_ELLIPSIS: &str = "…";

fn shorten_name(name: &str, keep: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if keep >= chars.len() {
        return name.to_owned();
    }
    let (start, end) = (keep.div_ceil(2), keep / 2);
    let head: String = chars[..start].iter().collect();
    let tail: String = chars[chars.len() - end..].iter().collect();
    format!("{head}{PATH_ELLIPSIS}{tail}")
}

/// `lib/pathDisplay.ts`: a path gives way in the middle, whole directories first, keeping its first and last names.
/// TODO(files pane): one copy of this across panes.
pub fn elide_path(path: &str, fits: &mut dyn FnMut(&str) -> bool) -> String {
    if fits(path) {
        return path.to_owned();
    }
    // Names and separator runs alternate, starting with a (possibly empty) name.
    let mut tokens: Vec<String> = vec![String::new()];
    for c in path.chars() {
        let separator = c == '/' || c == '\\';
        if separator != (tokens.len() % 2 == 0) {
            tokens.push(String::new());
        }
        tokens.last_mut().expect("never empty").push(c);
    }
    let named: Vec<usize> = tokens.iter().enumerate().filter(|(i, t)| i % 2 == 0 && !t.is_empty()).map(|(i, _)| i).collect();
    if named.is_empty() {
        return path.to_owned();
    }
    let count = named.len();
    let compose = |head: usize, tail: usize, replaced: &[(usize, String)]| -> String {
        let part = |from: usize, to: usize| -> String { (from..to).map(|i| replaced.iter().find(|r| r.0 == i).map_or(tokens[i].as_str(), |r| r.1.as_str())).collect() };
        if head + tail >= count {
            return part(0, tokens.len());
        }
        let head_end = named[head - 1];
        let tail_start = named[count - tail];
        format!("{}{}{PATH_ELLIPSIS}{}{}", part(0, head_end + 1), tokens[head_end + 1], tokens[tail_start - 1], part(tail_start, tokens.len()))
    };
    let (mut head, mut tail) = if count >= 3 { (1, 1) } else { (count, 0) };
    if count >= 3 && fits(&compose(head, tail, &[])) {
        let mut grew = true;
        while grew {
            grew = false;
            if head + tail + 1 < count && fits(&compose(head, tail + 1, &[])) {
                tail += 1;
                grew = true;
            }
            if head + tail + 1 < count && fits(&compose(head + 1, tail, &[])) {
                head += 1;
                grew = true;
            }
        }
        return compose(head, tail, &[]);
    }
    let (first, last) = (named[0], named[count - 1]);
    let mut replaced: Vec<(usize, String)> = Vec::new();
    let shorten = |index: usize, replaced: &mut Vec<(usize, String)>, fits: &mut dyn FnMut(&str) -> bool| -> bool {
        let name = &tokens[index];
        let length = name.chars().count();
        let attempt = |keep: usize, replaced: &[(usize, String)], fits: &mut dyn FnMut(&str) -> bool| {
            let mut with: Vec<(usize, String)> = replaced.to_vec();
            with.push((index, shorten_name(name, keep)));
            fits(&compose(head, tail, &with))
        };
        let mut found = None;
        if length > 1 && attempt(1, replaced, fits) {
            let (mut low, mut high) = (1, length - 1);
            while low < high {
                let middle = (low + high).div_ceil(2);
                if attempt(middle, replaced, fits) {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            found = Some(low);
        }
        replaced.push((index, shorten_name(name, found.unwrap_or(1))));
        found.is_some()
    };
    if first != last && shorten(first, &mut replaced, fits) {
        return compose(head, tail, &replaced);
    }
    shorten(last, &mut replaced, fits);
    compose(head, tail, &replaced)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RollSlot {
    Digit { from: u8, to: u8 },
    Glyph(String),
}

/// `rollingSlots`: outgoing and incoming text paired from the right, the way an odometer's wheels line up. A column
/// that was not a digit arrives showing its value; runs of non-digits stay whole, keeping their kerning.
pub fn rolling_slots(previous: &str, next: &str) -> Vec<RollSlot> {
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

/// `RollingNumber`: digits turn like the wheels of a lock when the value changes, and it is plain text between rolls.
pub fn rolling_number(key: impl std::hash::Hash, value: &str, ty: TypeStyle, color: Color) -> E {
    let state = use_state(("rolling-number", key), || RollState { settled: value.to_owned(), roll: None });
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
    .measure(move |_, ui, _, _| (ui.measure(&measured, &ty), ty.line))
    .shrink0()
    .pass_through()
    .aria_label(value)
}

/// One page of a pane, as its tab.
pub struct PageTab {
    pub id: String,
    pub label: String,
    pub title: Option<String>,
    pub icon: Option<E>,
}

/// What differs between this crate's two strips.
pub struct PageTabsOptions {
    pub aria_label: &'static str,
    pub more_label: &'static str,
    pub on_close: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// Delete and Backspace close the focused tab: pages that cost nothing to reopen.
    pub close_on_delete_key: bool,
    pub on_reorder: Option<Rc<dyn Fn(&mut App, Vec<String>)>>,
}

/// The shared strip, for these two callers' tabs.
pub fn page_tabs(key: &str, tabs: Vec<PageTab>, active: Option<&str>, on_select: Rc<dyn Fn(&mut App, &str)>, options: PageTabsOptions) -> E {
    use mewrk_ui_common::page_tabs as shared;
    let tabs = tabs
        .into_iter()
        .map(|tab| {
            let mut page = shared::PageTab::new(tab.id, tab.label);
            page.title = tab.title;
            page.leading = tab.icon;
            page
        })
        .collect();
    shared::page_tabs(shared::PageTabs {
        key: key.to_owned(),
        tabs,
        active_id: active.map(str::to_owned),
        aria_label: Arc::from(options.aria_label),
        more_label: Arc::from(options.more_label),
        on_select,
        on_close: options.on_close,
        close_label: Some(Rc::new(|tab: &shared::PageTab| tf("关闭标签 {label}", "Close tab {label}", &[("label", &tab.label)]))),
        close_on_delete_key: options.close_on_delete_key,
        on_reorder: options.on_reorder,
        max_tab_width: 180.0,
        on_tab_context_menu: None,
        on_tab_double_click: None,
        editor: None,
        trailing: None,
        trailing_width: 0.0,
    })
}
