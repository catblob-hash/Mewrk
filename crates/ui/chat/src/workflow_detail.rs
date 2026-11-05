//! `components/WorkflowRunDetail.tsx`: a workflow run as the body of its own timeline row.
//!
//! The row says *that* a run exists and how far along it is; this says roughly what it is made of, and the task container says
//! everything else. That split is why this stays small: a plan with thirty steps used to push the conversation off the screen
//! while it ran, and every one of those rows was already on screen in the panel beside it.

use std::hash::Hash;

use fluent::{TypeStyle, Weight};
use mewrk_client::prelude::*;
use mewrk_pane_tasks::workflow_runs::{format_run_elapsed, format_run_tokens, phase_tone, step_tone, WorkflowRunView, WorkflowRunStepState, WorkflowStepTone};

use crate::cat_art::ease_in_out;
use crate::rolling_number::rolling_number;

const fn style(size: f32, line: f32) -> TypeStyle {
    TypeStyle { size, line, weight: Weight::Regular, mono: false, italic: false, serif: false }
}

/// `.workflow-run__metrics`.
const METRIC: TypeStyle = style(11.0, 16.0);
/// `.workflow-run__description`.
const DESCRIPTION: TypeStyle = style(11.5, 16.7);
/// `.workflow-run__phase`.
const PHASE: TypeStyle = style(11.0, 16.0);

const PIP: f32 = 7.0;
const PIP_RADIUS: f32 = 2.0;
/// `workflow-pip-pulse`.
const PIP_PULSE_SECONDS: f32 = 1.6;

/// What upstream's accessible name says. A sentence, because thirty unlabelled squares say nothing to anyone.
pub fn workflow_run_summary(view: &WorkflowRunView) -> String {
    let done = view.steps.iter().filter(|step| step.state == WorkflowRunStepState::Finished).count();
    tf("工作流 {name}：{total} 个代理，已完成 {done} 个", "Workflow {name}: {total} agents, {done} done", &[("name", &view.name), ("total", &view.step_count), ("done", &done)])
}

fn pulse_opacity(time: f32) -> f32 {
    let t = (time / PIP_PULSE_SECONDS).fract();
    // 0% and 100% are whole, 50% is .42, eased over each half.
    let half = if t < 0.5 { t * 2.0 } else { (1.0 - t) * 2.0 };
    1.0 - (1.0 - 0.42) * ease_in_out(half)
}

fn pip(tone: WorkflowStepTone) -> E {
    let t = theme();
    let square = div().size(PIP, PIP).radius(PIP_RADIUS).shrink0().pass_through();
    match tone {
        WorkflowStepTone::Finished => square.bg(t.success),
        WorkflowStepTone::Failed => square.bg(t.critical),
        WorkflowStepTone::Skipped => square.bg(t.caution),
        WorkflowStepTone::Pending => square.bg(t.control_strong_stroke),
        WorkflowStepTone::Cached => square.bg(t.attention_bg).border(t.accent),
        WorkflowStepTone::Running => custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
            ui.fill(r, PIP_RADIUS, t.accent.fade(pulse_opacity(ui.time)));
            ui.wake_after(1.0 / 20.0);
        })
        .size(PIP, PIP)
        .shrink0()
        .pass_through(),
    }
}

fn phase_color(tone: WorkflowStepTone) -> Color {
    let t = theme();
    match tone {
        WorkflowStepTone::Running => t.accent,
        WorkflowStepTone::Failed => t.critical,
        WorkflowStepTone::Finished => t.success,
        _ => t.text_secondary,
    }
}

pub fn workflow_run_detail(key: impl Hash, view: &WorkflowRunView) -> E {
    let t = theme();
    let k = hash_key(key);
    let faint = t.text_tertiary;

    let mut metrics = row()
        .items_center()
        .wrap()
        .gap_x(10.0)
        .gap_y(4.0)
        .min_w(0.0)
        .child(text(tf("{count} 个代理", "{count} agents", &[("count", &view.step_count)])).ty(METRIC).color(faint).nowrap());
    if let Some(ms) = view.elapsed_ms {
        metrics = metrics.child(row().shrink0().child(rolling_number(("workflow-elapsed", k), &format_run_elapsed(ms), METRIC, faint)));
    }
    if let Some(tokens) = view.tokens {
        let label = tf("{tokens} token", "{tokens} tokens", &[("tokens", &format_run_tokens(tokens))]);
        metrics = metrics.child(row().shrink0().child(rolling_number(("workflow-tokens", k), &label, METRIC, faint)));
    }

    let description = (!view.description.is_empty()).then(|| text(view.description.clone()).ty(DESCRIPTION).color(t.text_secondary));
    // One square per plan slot. They wrap rather than shrink: a fifty-step plan should grow the strip by a line, not turn every
    // square into a hairline.
    let strip = (!view.steps.is_empty()).then(|| row().key(("workflow-strip", k)).wrap().gap(3.0).aria_hidden(true).children(view.steps.iter().map(|step| pip(step_tone(step)))));
    let phases = (!view.phases.is_empty()).then(|| {
        col().gap(2.0).children(view.phases.iter().map(|phase| {
            let heading = phase.heading.clone().unwrap_or_else(|| t_("未分组", "Unphased").to_owned());
            row()
                .key(("workflow-phase", k, phase.key.as_str()))
                .items_center()
                .gap(8.0)
                .min_w(0.0)
                .child(text(heading).ty(PHASE).color(phase_color(phase_tone(phase))).ellipsis())
                .child(text(format!("{}/{}", phase.done, phase.total)).ty(PHASE).color(faint).nowrap().shrink0())
        }))
    });

    col().key(("workflow-run", k)).gap(7.0).pt(2.0).pb(4.0).min_w(0.0).when(a11y_active(), |e| e.role(Role::Region).aria_label(workflow_run_summary(view)).aria_busy(view.running)).child(metrics).child(description).child(strip).child(phases)
}

use mewrk_client::t as t_;
