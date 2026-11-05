//! `components/StreamWaitingIndicator.tsx`: the stable end-of-timeline activity surface for a live model round. The cat never changes
//! identity while the run is active; the calls in flight are layered onto it as one line each, for as long as each one lasts, and
//! the tasks still running beside the round as one more, which opens the tasks pane.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};
use std::time::Instant;

use fluent::{CursorKind, TypeStyle, Weight};
use mewrk_client::model::ToolDescriptor;
use mewrk_client::prelude::*;
use mewrk_client::runs::util::{now_ms, parse_iso};
use mewrk_client::runs::{format_compact_token_count, LiveReasoningView, ModelRunRetryState};
use mewrk_pane_tasks::util::format_elapsed;
use mewrk_pane_files::links::elide_path;
use mewrk_ui_common::popover_menu::probe::say;

pub use crate::cat_art::CatMood;
use crate::cat_art::{cat_size, cubic_bezier, ease_in_out, mood_pose, paint_cat, CatPose};
use crate::item::Item;
use crate::reveal::{advance, paint_characters, quantized};
use crate::rolling_number::rolling_number;
use crate::tool_view::{get_tool_presentation, is_hoisted_tool_call};

pub const CAT_MOODS: [CatMood; 2] = [CatMood::Work, CatMood::Slack];

/// Loop period of each mood: the shortest interval after which every loop of that mood is back on its 0% keyframe, which for all of
/// them is the resting pose. Every loop's duration divides the entry here, and at least one of them equals it: that one is the clock
/// a mood change is aligned against.
pub fn cat_mood_cycle_ms(mood: CatMood) -> u64 {
    match mood {
        CatMood::Work | CatMood::Slack => 4_800,
    }
}

/// How long each mood lasts, as a window. The cat is mostly at its laptop, since that is what the round beside it is doing, and slacks
/// off between spells of it. These are only the bounds: the dwell actually used is quantised up to whole loop cycles.
fn cat_dwell_window_ms(mood: CatMood) -> (u64, u64) {
    match mood {
        CatMood::Work => (9_000, 15_000),
        CatMood::Slack => (4_000, 10_000),
    }
}

/// Close enough to a loop boundary that waiting another frame would show nothing.
const CAT_ALIGNMENT_TOLERANCE_MS: f64 = 16.0;
/// How far past a boundary still counts as on it. A frame arrives after the deadline by up to one of these, and the browser's clock
/// never had this problem because its animations start later than the timers that watch them.
const CAT_FRAME_MS: f64 = 1000.0 / 30.0;
/// A gap between two paints this long means the cat was not being drawn: the window hidden, or the conversation pane behind an open
/// task page, where upstream's `display: none` restarts the loops.
const CAT_UNSEEN_MS: f64 = 500.0;
/// `.stream-cat-gaze, .stream-cat-lids { transition: transform .8s cubic-bezier(.45, 0, .25, 1) }`.
const CAT_POSE_EASE_MS: f64 = 800.0;

/// The dwells available to one mood: every whole number of loop cycles that lands inside its window. A mood whose single cycle already
/// overruns the window gets one cycle rather than a truncated one: a change mid-loop is exactly the jump cut the alignment exists to
/// avoid.
pub fn cat_dwell_choices_ms(mood: CatMood) -> Vec<u64> {
    let period = cat_mood_cycle_ms(mood);
    let (min, max) = cat_dwell_window_ms(mood);
    let first = min.div_ceil(period).max(1);
    let last = (max / period).max(first);
    (first..=last).map(|count| count * period).collect()
}

/// How much longer the mood has to run before every loop is on its resting pose. Zero when it is on the boundary already.
fn ms_until_rest(elapsed: f64, period: f64) -> f64 {
    let into = elapsed.rem_euclid(period);
    let remaining = period - into;
    if remaining <= CAT_ALIGNMENT_TOLERANCE_MS || into <= CAT_FRAME_MS { 0.0 } else { remaining }
}

/// Work, slack, work: the cat alternates between its two moods.
///
/// Only the change is timed here. Each mood's loops are torn down and restarted by the change, which is why it waits for them all to
/// come round to rest first: the pose then moves smoothly and nothing else jumps.
#[derive(Clone, Debug)]
pub struct CatMachine {
    pub mood: CatMood,
    /// When the mood's loops started, on the clock `advance` is given. Public for a test to put it out of step with the dwell, which is
    /// what a late frame or a spell of not being drawn leaves behind.
    pub loops_started_ms: f64,
    /// `None` while motion is not allowed: what comes back gets a whole fresh dwell.
    deadline_ms: Option<f64>,
    /// The pose the last change eased from, and when.
    eased_from: (f32, f32),
    changed_at_ms: f64,
}

impl CatMachine {
    /// Every round starts at work.
    pub fn new(now_ms: f64) -> Self {
        Self { mood: CatMood::Work, loops_started_ms: now_ms, deadline_ms: None, eased_from: mood_pose(CatMood::Work), changed_at_ms: f64::NEG_INFINITY }
    }

    fn draw_dwell(&self, random: &mut dyn FnMut() -> f64) -> f64 {
        let choices = cat_dwell_choices_ms(self.mood);
        let at = ((random() * choices.len() as f64) as usize).min(choices.len() - 1);
        choices[at] as f64
    }

    /// Steps the machine to `now_ms`. `motion_allowed` is the document-wide half of `changeCanPlay`, which arms the timer at all;
    /// `rendered` is what has to be looked at again on each dwell, because a pane going out of sight announces nothing.
    pub fn advance(&mut self, now_ms: f64, motion_allowed: bool, rendered: bool, random: &mut dyn FnMut() -> f64) {
        if !motion_allowed {
            self.deadline_ms = None;
            return;
        }
        let Some(deadline) = self.deadline_ms else {
            self.deadline_ms = Some(now_ms + self.draw_dwell(random));
            return;
        };
        if now_ms < deadline {
            return;
        }
        if !rendered {
            self.deadline_ms = Some(now_ms + self.draw_dwell(random));
            return;
        }
        // Land on the loop boundary rather than near it: the dwell counted whole periods, and the loops have their own idea of where they are.
        let wait = ms_until_rest(now_ms - self.loops_started_ms, cat_mood_cycle_ms(self.mood) as f64);
        if wait > 0.0 {
            self.deadline_ms = Some(now_ms + wait);
            return;
        }
        self.eased_from = self.pose_now(now_ms);
        self.changed_at_ms = now_ms;
        self.mood = if self.mood == CatMood::Work { CatMood::Slack } else { CatMood::Work };
        self.loops_started_ms = now_ms;
        self.deadline_ms = Some(now_ms + self.draw_dwell(random));
    }

    /// What `display: none` does: the loops start over, unseen.
    pub fn restart_loops(&mut self, now_ms: f64) {
        self.loops_started_ms = now_ms;
    }

    fn pose_now(&self, now_ms: f64) -> (f32, f32) {
        let target = mood_pose(self.mood);
        let t = cubic_bezier(0.45, 0.0, 0.25, 1.0, ((now_ms - self.changed_at_ms) / CAT_POSE_EASE_MS).clamp(0.0, 1.0) as f32);
        (self.eased_from.0 + (target.0 - self.eased_from.0) * t, self.eased_from.1 + (target.1 - self.eased_from.1) * t)
    }

    pub fn pose(&self, now_ms: f64) -> CatPose {
        let (gaze, lids) = self.pose_now(now_ms);
        CatPose::at(self.mood, now_ms - self.loops_started_ms, gaze, lids)
    }
}

/// The dwell is drawn only when the machine needs one, never per frame.
fn random_unit() -> f64 {
    let bits = std::collections::hash_map::RandomState::new().hash_one(0u8);
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// A clock of milliseconds for the machine, which starts wherever the process did.
fn clock_ms() -> f64 {
    thread_local! {
        static START: Instant = Instant::now();
    }
    START.with(|start| start.elapsed().as_secs_f64() * 1000.0)
}

struct CatState {
    machine: CatMachine,
    last_paint_ms: f64,
}

/// The cat, drawn one line tall. The mood advances where it is painted, so a cat nobody draws costs nothing and asks for no frames.
fn stream_waiting_cat(k: u64, color: Color) -> E {
    let t_ = theme();
    let state = use_state(("stream-cat", k), || CatState { machine: CatMachine::new(clock_ms()), last_paint_ms: clock_ms() });
    let (width, height) = cat_size(LINE);
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let now = clock_ms();
        // Not `update`, which asks for a frame at once: the cat only ever wants the next one a thirtieth of a second from now.
        let pose = {
            let mut cat = state.borrow_mut();
            let rendered = now - cat.last_paint_ms < CAT_UNSEEN_MS;
            if !rendered {
                cat.machine.restart_loops(now);
            }
            cat.last_paint_ms = now;
            // Upstream's other half of `changeCanPlay`, a hidden document, is `rendered`.
            cat.machine.advance(now, !fluent::reduced_motion(), rendered, &mut random_unit);
            cat.machine.pose(now)
        };
        paint_cat(ui, r, &pose, color, t_.control_strong_stroke);
        ui.wake_after(1.0 / 30.0);
    })
    .key(("stream-cat-drawing", k))
    .size(width, height)
    .shrink0()
    .pass_through()
}

/// `--stream-waiting-line`. The cat is drawn one text line tall, so the line height is pinned.
const LINE: f32 = 16.0;
/// `.stream-waiting__activity`.
const ACTIVITY: TypeStyle = TypeStyle { size: 11.5, line: LINE, weight: Weight::Semibold, mono: false, italic: false, serif: false };
/// `.stream-waiting__activity-target` and `-count`.
const FIGURE: TypeStyle = TypeStyle { size: 10.5, line: 15.75, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.stream-waiting__retry-message`.
const RETRY_MESSAGE: TypeStyle = TypeStyle { size: 10.0, line: 14.0, weight: Weight::Regular, mono: false, italic: false, serif: false };

/// One in-flight call, reduced to the line the indicator narrates it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolActivity {
    pub id: String,
    pub tool_name: String,
    pub title: String,
    pub target: Option<String>,
    /// The target is a path, drawn so that it gives way in the middle.
    pub target_is_path: bool,
}

/// Every call the timeline hoisted onto this indicator, in timeline order.
///
/// Usually there is exactly one: the host runs a round's calls one after the other. Async tools are the exception that makes this a list:
/// `web_search` and `web_fetch` are dispatched and only collected at the round's settlement point, so a later call really can be running
/// beside them. Narrating only the newest would leave the others with no surface at all, since their blocks are hoisted too.
pub fn tool_activities(contexts: &[Item<'_>], tools: &[ToolDescriptor], explanations: &HashMap<String, String>) -> Vec<ToolActivity> {
    contexts
        .iter()
        .filter_map(|context| {
            let tool = context.tool().filter(is_hoisted_tool_call)?;
            let descriptor = tools.iter().find(|candidate| candidate.name == tool.tool_name);
            let presentation = get_tool_presentation(&tool, descriptor, explanations.get(tool.id).map(String::as_str), None);
            Some(ToolActivity { id: tool.id.to_owned(), tool_name: tool.tool_name.to_owned(), title: presentation.title, target: presentation.target, target_is_path: presentation.target_is_path })
        })
        .collect()
}

fn activity_label(activity: &ToolActivity) -> String {
    match &activity.target {
        Some(target) => format!("{} {target}", activity.title),
        None => activity.title.clone(),
    }
}

/// The line that opens the tasks pane; drawn only when there is a pane to open.
pub fn tasks_label(running_task_count: usize, can_open: bool) -> Option<String> {
    if running_task_count == 0 || !can_open {
        return None;
    }
    Some(if running_task_count == 1 { t("1 个任务正在运行", "1 task running").to_owned() } else { tf("{count} 个任务正在运行", "{count} tasks running", &[("count", &running_task_count)]) })
}

pub fn retry_label(notice: &ModelRunRetryState) -> String {
    tf("连接中断，正在第 {attempt}/{max} 次重试", "Connection interrupted; retrying {attempt}/{max}", &[("attempt", &notice.attempt), ("max", &notice.max_attempts)])
}

/// What a screen reader was told. Fluent has no accessibility tree yet, so this is what a test reads, and what one will say.
pub fn announced_label(thinking: bool, activities: &[ToolActivity], tasks: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if thinking {
        parts.push(t("正在思考", "Thinking").to_owned());
    }
    parts.extend(activities.iter().map(activity_label));
    if parts.is_empty() {
        parts.push(t("模型正在生成", "Model is generating").to_owned());
    }
    if let Some(tasks) = tasks {
        parts.push(tasks.to_owned());
    }
    parts.join(t("；", "; "))
}

/// Both figures are there whatever the provider: how long the round has been thinking, on a clock of its own, and how many tokens it has
/// thought, which is an estimate when the provider reports nothing until the round ends.
pub fn thinking_figures(thinking: &LiveReasoningView, now: i64) -> (Option<String>, String) {
    let elapsed = parse_iso(&thinking.started_at).map(|started| format_elapsed(now - started));
    (elapsed, tf("{tokens} token", "{tokens} tokens", &[("tokens", &format_compact_token_count(thinking.tokens))]))
}

pub struct WaitingProps {
    pub tools: Rc<Vec<ToolDescriptor>>,
    /// Reasoning the live round is doing right now. Encrypted reasoning with no summary never reaches the timeline as a card, so this line
    /// is the only place it is visible while it happens; plaintext reasoning is narrated here too, so the two read the same from outside.
    pub thinking: Option<LiveReasoningView>,
    /// Transient "request failed, retrying" hint for the live round.
    pub retry_notice: Option<ModelRunRetryState>,
    /// Rows the tasks pane lists as running. Narrated as one more line, which opens that pane; drawn only when there is a pane to open.
    pub running_task_count: usize,
    pub on_open_tasks: Option<Handler<App>>,
}

/// `stream-waiting-activity-in`: a line slides and fades in once, when it appears.
fn activity_in(line: E, key: impl Hash) -> E {
    const SECONDS: f32 = 0.22;
    let born = use_state(("stream-waiting-in", hash_key(key)), Instant::now).get();
    let t = born.elapsed().as_secs_f32() / SECONDS;
    if t >= 1.0 {
        return line;
    }
    repaint();
    let eased = cubic_bezier(0.2, 0.75, 0.25, 1.0, t);
    line.opacity(eased).left(-4.0 * (1.0 - eased))
}

/// The dots that say it has not finished. `stream-waiting-dot`, with each dot a little behind the last.
fn dots(color: Color) -> E {
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        const SECONDS: f32 = 1.2;
        const DOT: f32 = 2.5;
        let still = fluent::reduced_motion();
        for index in 0..3 {
            let phase = if still { 0.3 } else { ((ui.time - index as f32 * 0.14) / SECONDS).rem_euclid(1.0) };
            // 0%, 60%, 100% at rest and 30% at the top, eased over each stretch.
            let lift = if phase < 0.3 { ease_in_out(phase / 0.3) } else if phase < 0.6 { 1.0 - ease_in_out((phase - 0.3) / 0.3) } else { 0.0 };
            let x = r.x + index as f32 * (DOT + 2.0);
            let y = r.y + (r.h - DOT) * 0.5 - 2.0 * lift;
            ui.fill(rect(x, y, DOT, DOT), DOT * 0.5, color.fade(0.28 + 0.72 * lift));
        }
        if !still {
            ui.wake_after(1.0 / 30.0);
        }
    })
    .size(3.0 * 2.5 + 2.0 * 2.0, 5.0)
    .shrink0()
    .pass_through()
}

const PULSE_SECONDS: f32 = 1.9;

/// How much of its colour `.pulse-text` gives a point `x` px into a line `width` wide at `time`: half strength but for a highlight in the
/// middle of a gradient 3.2 boxes wide, whose position runs across the box every 1.9 seconds.
pub fn pulse_alpha(x: f32, width: f32, time: f32) -> f32 {
    let position = 1.0 - ease_in_out((time / PULSE_SECONDS).rem_euclid(1.0));
    let centre = 1.6 * width - 2.2 * width * position;
    0.5 + 0.5 * (1.0 - (x - centre).abs() / (0.448 * width.max(1.0))).clamp(0.0, 1.0)
}

/// The verb, which is the only thing on a waiting line that pulses. It never gives way (`flex: 0 0 auto`); what the line has no room for is
/// the target's to lose.
fn pulse_label(value: &str, color: Color) -> E {
    let (painted, measured) = (value.to_owned(), value.to_owned());
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let width = advance(ui, &painted, &ACTIVITY);
        let time = ui.time;
        if fluent::reduced_motion() {
            ui.label(r.x, r.y + (r.h - ACTIVITY.line) * 0.5, &painted, &ACTIVITY, color);
            return;
        }
        paint_characters(ui, r.x, r.y + (r.h - ACTIVITY.line) * 0.5, &painted, 0, &ACTIVITY, |centre| color.fade(quantized(pulse_alpha(centre, width, time))));
        ui.wake_after(1.0 / 30.0);
    })
    .measure(move |_: &mut App, ui: &mut Ui<'_>, _, _| (advance(ui, &measured, &ACTIVITY), ACTIVITY.line))
    .shrink0()
    .pass_through()
}

/// `PathText`, sized to the path itself. `mewrk_ui_common::path_text` fits a path to half a pixel less than its box, and sizes the box to the
/// path's width rounded up, so a path that has the room still loses a directory whenever that rounding is under half a pixel.
fn path_target(key: impl Hash, path: &str, color: Color) -> E {
    let (painted, measured) = (path.to_owned(), path.to_owned());
    let leaf = custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let shown = if ui.measure(&painted, &FIGURE) <= r.w + 0.5 {
            painted.clone()
        } else {
            ui.line_was_cut(r, &painted);
            elide_path(&painted, &mut |candidate| ui.measure(candidate, &FIGURE) <= r.w - 0.5)
        };
        ui.label_ellipsis(r.x, r.y + (r.h - FIGURE.line) * 0.5, r.w, &shown, &FIGURE, color);
    })
    .measure(move |_: &mut App, ui: &mut Ui<'_>, known, avail: f32| (known.unwrap_or_else(|| ui.measure(&measured, &FIGURE).ceil().min(avail)), FIGURE.line))
    .min_w(0.0)
    .pass_through();
    div().key(key).min_w(0.0).with_style(|s| s.flex_shrink = 1.0).tooltip(path.to_owned()).child(leaf)
}

/// The waiting line for one call: what it is doing, what it is doing it to, and the dots. Only the verb pulses: the target and the dots keep
/// painting in their own colour.
fn tool_activity_line(k: u64, activity: &ToolActivity, pulse: Color, dot: Color) -> E {
    let t_ = theme();
    say(&activity.title);
    let target = activity.target.as_ref().map(|target| {
        say(target);
        if activity.target_is_path {
            path_target(("stream-waiting-target", k, activity.id.as_str()), target, t_.text_tertiary)
        } else {
            text(target.clone()).ty(FIGURE).color(t_.text_tertiary).ellipsis().tooltip(target.clone()).with_style(|s| s.flex_shrink = 1.0)
        }
    });
    let line = row().items_center().gap(5.0).min_w(0.0).clip().child(pulse_label(&activity.title, pulse)).child(target).child(dots(dot));
    activity_in(line, ("activity", k, activity.id.as_str()))
}

/// The waiting line for reasoning. Same shape as a call's line (pulsing verb, then the figures it has to report, then the dots) because from
/// the outside thinking and calling a tool are the same kind of wait.
fn thinking_activity_line(k: u64, thinking: &LiveReasoningView, label: &str, pulse: Color, dot: Color) -> E {
    let t_ = theme();
    let (elapsed, tokens) = thinking_figures(thinking, now_ms());
    say(label);
    if let Some(elapsed) = &elapsed {
        say(elapsed);
    }
    say(&tokens);
    let line = row()
        .items_center()
        .gap(5.0)
        .min_w(0.0)
        .clip()
        .child(pulse_label(label, pulse))
        .child(elapsed.map(|elapsed| row().shrink0().child(rolling_number(("stream-waiting-clock", k), &elapsed, FIGURE, t_.text_tertiary))))
        .child(row().shrink0().child(rolling_number(("stream-waiting-tokens", k), &tokens, FIGURE, t_.text_tertiary)))
        .child(dots(dot));
    activity_in(line, ("thinking", k))
}

/// The tasks line's key, for a test and for nothing else.
pub fn tasks_line_key(key: u64) -> (&'static str, u64) {
    ("stream-waiting-tasks", key)
}

pub fn stream_waiting_indicator(key: impl Hash, contexts: &[Item<'_>], props: WaitingProps) -> E {
    let t_ = theme();
    let k = hash_key(key);
    let WaitingProps { tools, thinking, retry_notice, running_task_count, on_open_tasks } = props;
    // `useSyncExternalStore` for the explanations: a view has no `App` to read them from, so a leaf copies this frame's into state, and the
    // next frame's view reads them. Keyed by the indicator, so they go when it does.
    let explanations = use_state(("stream-waiting-explanations", k), HashMap::<String, String>::new);
    let activities = explanations.with(|known| tool_activities(contexts, &tools, known));
    let ids: Vec<String> = activities.iter().map(|activity| activity.id.clone()).collect();
    let reader = (!ids.is_empty()).then(|| {
        let known = explanations.clone();
        custom(move |app: &mut App, _: &mut Ui<'_>, _| {
            for id in &ids {
                if let Some(explanation) = app.tool_explanation(id, None, None)
                    && known.with(|known| known.get(id) != Some(&explanation))
                {
                    known.update(|known| known.insert(id.clone(), explanation));
                }
            }
        })
        .size(1.0, 1.0)
        .absolute()
        .pass_through()
    });

    let (muted, pulse_color) = (t_.text_secondary, t_.text_primary);
    let frame = |retrying: bool| row().key(("stream-waiting", k)).mt(12.0).py(2.0).px(3.0).items_center().gap(6.0).min_w(0.0).role(Role::Status).aria_live(Live::Polite).child(stream_waiting_cat(k, if retrying { t_.caution } else { t_.text_primary }));

    if let Some(notice) = retry_notice {
        let label = retry_label(&notice);
        say(&label);
        say(&notice.message);
        let (pulse, tone) = (t_.caution, t_.caution);
        let line = row().items_center().gap(5.0).min_w(0.0).clip().child(pulse_label(&label, pulse)).child(dots(tone));
        return frame(true)
            .aria_label(label.as_str())
            .child(reader)
            .child(activity_in(line, ("retry", k)))
            .child(text(notice.message.clone()).ty(RETRY_MESSAGE).color(t_.caution.fade(0.8)).ellipsis().tooltip(notice.message.clone()).with_style(|s| s.flex_shrink = 1.0));
    }

    let tasks = tasks_label(running_task_count, on_open_tasks.is_some());
    let narrating = thinking.is_some() || !activities.is_empty();
    let lines = (narrating || tasks.is_some()).then(|| {
        let thinking_line = thinking.as_ref().map(|thinking| thinking_activity_line(k, thinking, t("正在思考", "Thinking"), pulse_color, muted));
        // A running shell call is its line alone; what it prints is on the task page.
        let tool_lines = activities.iter().map(|activity| tool_activity_line(k, activity, pulse_color, muted));
        let tasks_line = tasks.as_ref().zip(on_open_tasks.clone()).map(|(label, open)| {
            say(label);
            let line = row().key(tasks_line_key(k)).self_start().min_w(0.0).max_w_full().cursor(CursorKind::Hand).on_click(move |app: &mut App| open(app)).child(text(label.clone()).ty(ACTIVITY).color(t_.accent).ellipsis());
            activity_in(line, ("tasks", k))
        });
        col().min_w(0.0).gap(3.0).child(thinking_line).children(tool_lines).child(tasks_line)
    });
    let announced = announced_label(thinking.is_some(), &activities, tasks.as_deref());
    say(&announced);
    frame(false).aria_label(announced).child(reader).child(lines)
}

trait MaxFull {
    fn max_w_full(self) -> Self;
}

impl MaxFull for E {
    fn max_w_full(self) -> Self {
        self.with_style(|s| s.max_size.width = taffy::style::Dimension::percent(1.0))
    }
}
