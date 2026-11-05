//! `components/WorkflowRunPanel.tsx` and the two workflow commands of `lib/runtime.ts`.

use mewrk_client::model::SubagentRunRecord;
use mewrk_client::prelude::*;

use fluent::{TypeStyle, Weight};

use crate::task_container::{role_title, TaskItemState};
use crate::tasks_pane::{DETAIL_TY, LABEL_DONE_TY, LABEL_TY, METRIC_TY, StepControlHandler};
use crate::widgets::*;
use crate::workflow_runs::*;

pub use mewrk_client::runs::WorkflowStepAction;

/// The word the host's command takes, which also keys a step's two controls.
pub fn action_name(action: WorkflowStepAction) -> &'static str {
    match action {
        WorkflowStepAction::Skip => "skip",
        WorkflowStepAction::Retry => "retry",
    }
}

/// `handleWorkflowStepControl`: the workflow may already have ended, and a refused control is non-fatal.
pub fn control_workflow_step(app: &App, conversation_id: &str, run_id: &str, step_index: i64, action: WorkflowStepAction) {
    app.invoke::<Value>("workflow_step_control", json!({ "conversationId": conversation_id, "runId": run_id, "stepIndex": step_index, "action": action_name(action) }), |_, _| {});
}

/// Loads one workflow step's externalized full record from the run directory.
pub fn workflow_step_record(app: &App, conversation_id: &str, run_id: &str, step_index: i64, done: impl FnOnce(&mut App, Result<Option<SubagentRunRecord>, String>) + Send + 'static) {
    app.invoke("workflow_step_record", json!({ "conversationId": conversation_id, "runId": run_id, "stepIndex": step_index }), done);
}

pub struct WorkflowRunPanelProps {
    pub view: WorkflowRunView,
    /// Step whose transcript the main area is showing, so its row reads as current.
    pub selected_agent_id: Option<String>,
    /// True while this run's abort has been asked for and not yet observed.
    pub stopping: bool,
    /// Run identifier Skip and Retry address. When the live ledger has not named it, the run's own steps do.
    pub run_id: Option<String>,
    /// Opens one step's read-only transcript. A step is a real agent; the run is not.
    pub on_open_agent: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub on_stop: Option<Rc<dyn Fn(&mut App)>>,
    pub on_step_control: Option<StepControlHandler>,
}

fn step_state_label(step: &WorkflowRunStep) -> &'static str {
    match step_tone(step) {
        WorkflowStepTone::Skipped => t("已跳过", "Skipped"),
        WorkflowStepTone::Cached => t("已缓存", "Cached"),
        WorkflowStepTone::Finished => t("已完成", "Completed"),
        WorkflowStepTone::Failed => t("已失败", "Failed"),
        WorkflowStepTone::Pending => t("等待中", "Waiting"),
        WorkflowStepTone::Running => t("运行中", "Running"),
    }
}

const STEP_LABEL_TY: TypeStyle = ts(10.0, 13.0, Weight::Semibold);
const STEP_DETAIL_TY: TypeStyle = ts(9.0, 12.0, Weight::Regular);
const STEP_METRIC_TY: TypeStyle = mono(9.0, 12.0);

fn step_icon(step: &WorkflowRunStep) -> E {
    let c = theme();
    match step_tone(step) {
        WorkflowStepTone::Finished => icon("Check", 11.0).color(c.success),
        WorkflowStepTone::Cached => icon("Check", 11.0).color(c.accent_text),
        WorkflowStepTone::Failed => icon("X", 11.0).color(c.critical),
        WorkflowStepTone::Skipped => icon("X", 11.0).color(c.caution),
        WorkflowStepTone::Running => spinner(11.0, c.accent_text),
        WorkflowStepTone::Pending => row().center().size(11.0, 11.0).shrink0().child(div().size(6.0, 6.0).radius(3.0).border(c.control_strong_stroke)),
    }
}

/// One step: the same shape as a task row, one size down, because a step is a task that belongs to a run.
fn step_tile(step: &WorkflowRunStep, p: &WorkflowRunPanelProps, run_id: Option<&str>) -> E {
    let c = theme();
    let tone = step_tone(step);
    let title = role_title(step.role.as_deref(), &step.label);
    let selected = step.agent_id.is_some() && step.agent_id == p.selected_agent_id;
    // No plan index, no controls: the host addresses a slot by number, and a guess would skip an unrelated one.
    let controllable = run_id.is_some() && p.on_step_control.is_some() && tone == WorkflowStepTone::Running && step.plan_index.is_some();
    let label_color = match tone {
        WorkflowStepTone::Failed => c.critical,
        WorkflowStepTone::Pending => c.text_secondary,
        _ => c.text_primary,
    };
    let tip = match &step.model_id {
        Some(model) => tf("模型：{model}", "Model: {model}", &[("model", model)]),
        None => step_state_label(step).to_owned(),
    };
    let label: E = if tone == WorkflowStepTone::Running { fitted_label(&title, None, STEP_LABEL_TY, label_color, true) } else { text(title.clone()).ty(STEP_LABEL_TY).color(label_color).ellipsis() };
    let detail = step.task.clone().or_else(|| step.model_id.clone()).unwrap_or_else(|| "—".to_owned());
    let metric_color = if tone == WorkflowStepTone::Failed { c.critical } else { c.text_tertiary };
    let tokens = step.tokens.map_or_else(|| "—".to_owned(), format_run_tokens);
    let elapsed = step.elapsed_ms.map_or_else(|| "—".to_owned(), format_run_elapsed);
    let mut main = row()
        .key(("workflow-step", step.key.as_str()))
        .items_center()
        .gap(6.0)
        .py(4.0)
        .px(5.0)
        .radius(6.0)
        .bg(if selected { c.subtle_pressed } else { c.card_secondary })
        .tooltip(tip)
        .child(step_icon(step))
        .child(col().flex1().child(label).child(text(detail).ty(STEP_DETAIL_TY).color(c.text_tertiary).ellipsis()))
        .child(
            row()
                .items_center()
                .gap(5.0)
                .shrink0()
                .aria_hidden(true)
                .child(rolling_number(("step-tokens", step.key.as_str()), &tokens, STEP_METRIC_TY, metric_color))
                .child(rolling_number(("step-elapsed", step.key.as_str()), &elapsed, STEP_METRIC_TY, metric_color)),
        );
    // A step is a real agent with a real transcript, so its tile opens. A slot with no agent has nothing to open.
    if let (Some(agent_id), Some(open)) = (step.agent_id.clone(), p.on_open_agent.clone()) {
        let (id, open_key, id_key) = (agent_id.clone(), open.clone(), agent_id);
        main = main.hover_bg(c.subtle_hover).role(Role::Button).when(a11y_active(), |e| e.aria_label(tf("打开步骤 {label}", "Open step {label}", &[("label", &title)]))).when(selected, |e| e.aria_current(AriaCurrent::True)).on_click(move |app| open(app, &id)).on_key(move |app, k| {
            if matches!(k.key, Key::Enter | Key::Char(' ')) {
                open_key(app, &id_key);
                true
            } else {
                false
            }
        });
    }
    if controllable {
        let run = run_id.expect("checked").to_owned();
        let index = step.plan_index.expect("checked");
        let handler = p.on_step_control.clone().expect("checked");
        for (action, glyph, tip) in [(WorkflowStepAction::Retry, "RotateCcw", tf("重试 {label}", "Retry {label}", &[("label", &title)])), (WorkflowStepAction::Skip, "SkipForward", tf("跳过 {label}", "Skip {label}", &[("label", &title)]))] {
            let (run, handler) = (run.clone(), handler.clone());
            main = main.child(
                row()
                    .key(("workflow-step-control", step.key.as_str(), action_name(action)))
                    .center()
                    .size(16.0, 16.0)
                    .radius(4.0)
                    .shrink0()
                    .hover_bg(c.subtle_hover)
                    .tooltip(tip)
                    .reveal_on_parent_hover()
                    .child(icon(glyph, 10.0).color(c.text_secondary))
                    .on_click(move |app| handler(app, &run, index, action)),
            );
        }
    }
    main
}

fn phase_block(phase: &WorkflowRunPhase, p: &WorkflowRunPanelProps, run_id: Option<&str>) -> E {
    let c = theme();
    let title_color = match phase_tone(phase) {
        WorkflowStepTone::Running => c.accent_text,
        WorkflowStepTone::Failed => c.critical,
        _ => c.text_tertiary,
    };
    let heading = phase.heading.clone().unwrap_or_else(|| t("未分组", "Ungrouped").to_owned());
    col()
        .mb(4.0)
        .child(
            row()
                .items_center()
                .gap(6.0)
                .mb(3.0)
                .px(4.0)
                .child(text(heading).ty(ts(9.5, 13.0, Weight::Semibold)).color(title_color).ellipsis().flex1())
                .child(text(format!("{}/{}", phase.done, phase.total)).ty(ts(9.0, 12.0, Weight::Regular)).color(c.text_tertiary).nowrap().shrink0()),
        )
        .child(col().gap(2.0).children(phase.steps.iter().map(|s| step_tile(s, p, run_id))))
}

/// One workflow run: its bar, then every phase and step inside the same block. There is no disclosure at either level,
/// because a run's whole point is the work it is doing right now. Only the run log folds: it is narration.
pub fn workflow_run_panel(p: WorkflowRunPanelProps) -> E {
    let c = theme();
    let view = &p.view;
    let logs_open = use_state(("workflow-logs-open", view.id.as_str()), || false);
    let state = view.state;
    let (icon_color, label_color, label_ty, metric_color) = match state {
        TaskItemState::Running => (c.accent_text, c.text_primary, LABEL_TY, c.text_secondary),
        TaskItemState::Finished => (c.text_tertiary, c.text_secondary, LABEL_DONE_TY, c.text_tertiary),
        TaskItemState::Failed => (c.critical, c.critical, LABEL_TY, c.critical),
    };
    let detail_color = if state == TaskItemState::Failed { c.critical } else { c.text_tertiary };
    let label: E = if view.running { fitted_label(&view.name, None, label_ty, label_color, true) } else { text(view.name.clone()).ty(label_ty).color(label_color).ellipsis() };
    let detail = view.script_name.clone().unwrap_or_else(|| tf("{total} 个步骤", "{total} steps", &[("total", &view.step_count)]));
    let mut bar = row()
        .key(("workflow-bar", view.id.as_str()))
        .items_center()
        .gap(7.0)
        .p(6.0)
        .child(icon("Workflow", 13.0).color(icon_color))
        .child(col().flex1().gap(1.0).child(label).child(text(detail).ty(DETAIL_TY).color(detail_color).ellipsis()));
    let mut meta = row().items_center().gap(6.0).shrink0();
    if let Some(ms) = view.elapsed_ms {
        meta = meta.child(rolling_number(("run-elapsed", view.id.as_str()), &format_run_elapsed(ms), METRIC_TY, metric_color));
    }
    if let Some(tokens) = view.tokens {
        let value = tf("{tokens} token", "{tokens} tokens", &[("tokens", &format_run_tokens(tokens))]);
        meta = meta.child(rolling_number(("run-tokens", view.id.as_str()), &value, METRIC_TY, metric_color));
    }
    bar = bar.child(meta);
    if let (true, Some(on_stop)) = (view.running, p.on_stop.clone()) {
        let tip = if p.stopping { t("正在中止工作流", "Stopping the workflow") } else { t("中止工作流", "Stop the workflow") };
        let mut stop = row().key(("workflow-stop", view.id.as_str())).center().size(22.0, 22.0).radius(5.0).shrink0().tooltip(tip);
        if p.stopping {
            stop = stop.on_click(|_| {}).disabled(true).child(spinner(11.0, c.text_secondary));
        } else {
            stop = stop.hover_bg(c.critical_bg).child(stop_glyph(c.text_secondary)).on_click(move |app| on_stop(app));
        }
        bar = bar.child(stop);
    }
    let run_id = p.run_id.clone().or_else(|| view.run_id.clone());
    let mut body = col().px(2.0);
    if !view.description.is_empty() {
        body = body.child(text(view.description.clone()).ty(ts(10.0, 15.0, Weight::Regular)).color(c.text_secondary).mb(4.0).px(4.0));
    }
    for phase in &view.phases {
        body = body.child(phase_block(phase, &p, run_id.as_deref()));
    }
    if !view.logs.is_empty() {
        let open = logs_open.get();
        let toggle = logs_open.clone();
        let mut logs = col().mt(4.0).child(
            row()
                .key(("workflow-logs-toggle", view.id.as_str()))
                .items_center()
                .gap(5.0)
                .py(3.0)
                .px(4.0)
                .radius(5.0)
                .hover_bg(c.subtle_hover)
                .aria_expanded(open)
                .aria_controls(("workflow-logs", view.id.as_str()))
                .on_click(move |_| toggle.update(|v| *v = !*v))
                .child(icon("ChevronRight", 12.0).color(c.text_secondary).rotate(if open { 90.0 } else { 0.0 }))
                .child(text(tf("运行日志（{count} 条）", "Run log ({count})", &[("count", &view.logs.len())])).ty(DETAIL_TY).color(c.text_secondary).ellipsis()),
        );
        if open {
            logs = logs.child(col().key(("workflow-logs", view.id.as_str())).mt(3.0).px(4.0).gap(2.0).max_h(160.0).scroll_y().children(view.logs.iter().map(|m| text(m.clone()).ty(DETAIL_TY).color(c.text_tertiary).selectable())));
        }
        body = body.child(logs);
    }
    col().pt(2.0).px(4.0).pb(4.0).radius(9.0).bg(c.card).when(a11y_active(), |e| e.role(Role::Region).aria_label(tf("工作流 {name}", "Workflow {name}", &[("name", &view.name)])).aria_busy(view.running)).child(bar).child(body)
}
