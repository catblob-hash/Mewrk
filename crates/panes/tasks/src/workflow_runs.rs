//! `lib/workflowRuns.ts`: the one projection both workflow surfaces draw from. The roster decides the table, the live
//! ledger fills in the slots the roster cannot have, and a settled run simply has no ledger to merge.

use crate::task_container::{prompt_preview, task_state_for_status, TaskItem, TaskItemState, TaskKind};
use crate::util::{js_trim, to_fixed};
use crate::workflow_progress::{WorkflowProgressView, WorkflowStepState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowRunStepState {
    Running,
    Finished,
    Failed,
    /// A slot with no agent has not failed and has not finished.
    Pending,
}

impl From<TaskItemState> for WorkflowRunStepState {
    fn from(s: TaskItemState) -> Self {
        match s {
            TaskItemState::Running => Self::Running,
            TaskItemState::Finished => Self::Finished,
            TaskItemState::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRunStep {
    pub key: String,
    /// What Skip and Retry address. `None` must disable the controls rather than fall back to a guess: the host would
    /// happily skip whichever slot the guess named.
    pub plan_index: Option<i64>,
    pub label: String,
    pub state: WorkflowRunStepState,
    pub agent_id: Option<String>,
    pub role: Option<String>,
    pub task: Option<String>,
    pub model_id: Option<String>,
    pub tokens: Option<i64>,
    pub elapsed_ms: Option<i64>,
    pub cached: bool,
    pub blocked: bool,
    pub skipped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRunPhase {
    pub key: String,
    pub heading: Option<String>,
    pub steps: Vec<WorkflowRunStep>,
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowRunView {
    pub id: String,
    pub run_id: Option<String>,
    pub name: String,
    pub script_name: Option<String>,
    pub description: String,
    pub state: TaskItemState,
    pub step_count: usize,
    pub elapsed_ms: Option<i64>,
    pub tokens: Option<i64>,
    pub phases: Vec<WorkflowRunPhase>,
    pub steps: Vec<WorkflowRunStep>,
    pub logs: Vec<String>,
    pub running: bool,
}

const UNPHASED_ORDER: i64 = 9_007_199_254_740_991;

/// The run and slot a step's address names: `<runId>/ws<N>` is slot N-1. `None` for anything else.
pub fn step_address(ledger_owner: Option<&str>) -> Option<(String, i64)> {
    let (run, tail) = ledger_owner?.rsplit_once("/ws")?;
    if run.is_empty() || tail.is_empty() || tail.starts_with('0') || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((run.to_owned(), tail.parse::<i64>().ok()? - 1))
}

/// 1_240 → "1.2k", 12_400 → "12k". The same rounding the task rows use, so one run never reads as two numbers.
pub fn format_run_tokens(tokens: i64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1000.0;
    if thousands < 10.0 { format!("{}k", to_fixed(thousands, 1)) } else { format!("{}k", js_round(thousands)) }
}

fn js_round(v: f64) -> i64 {
    (v + 0.5).floor() as i64
}

/// "9s", "1m 24s", "19m 47s", "1h 04m". The space is the difference from the metric columns' compact form.
pub fn format_run_elapsed(ms: i64) -> String {
    let total = ms / 1000;
    if total < 60 {
        return format!("{total}s");
    }
    let (minutes, seconds) = (total / 60, total % 60);
    if minutes < 60 {
        return format!("{minutes}m {seconds:02}s");
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

fn ledger_state(state: WorkflowStepState, blocked: bool) -> WorkflowRunStepState {
    match state {
        WorkflowStepState::Done => WorkflowRunStepState::Finished,
        WorkflowStepState::Error => WorkflowRunStepState::Failed,
        _ if blocked => WorkflowRunStepState::Pending,
        _ => WorkflowRunStepState::Running,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowStepTone {
    Running,
    Finished,
    Failed,
    Pending,
    Skipped,
    Cached,
}

/// The two flags outrank the state: a replayed step is "finished" without having run, and a skipped one is an error
/// row carrying a sentinel rather than a failure.
pub fn step_tone(step: &WorkflowRunStep) -> WorkflowStepTone {
    if step.skipped {
        return WorkflowStepTone::Skipped;
    }
    if step.cached {
        return WorkflowStepTone::Cached;
    }
    match step.state {
        WorkflowRunStepState::Running => WorkflowStepTone::Running,
        WorkflowRunStepState::Finished => WorkflowStepTone::Finished,
        WorkflowRunStepState::Failed => WorkflowStepTone::Failed,
        WorkflowRunStepState::Pending => WorkflowStepTone::Pending,
    }
}

pub fn phase_tone(phase: &WorkflowRunPhase) -> WorkflowStepTone {
    let has = |t| phase.steps.iter().any(|s| step_tone(s) == t);
    if has(WorkflowStepTone::Failed) {
        WorkflowStepTone::Failed
    } else if has(WorkflowStepTone::Running) {
        WorkflowStepTone::Running
    } else if has(WorkflowStepTone::Pending) {
        WorkflowStepTone::Pending
    } else if phase.done == phase.total && phase.total > 0 {
        WorkflowStepTone::Finished
    } else {
        WorkflowStepTone::Pending
    }
}

struct Slot {
    step: WorkflowRunStep,
    phase: Option<String>,
    phase_index: i64,
    order: i64,
}

/// Folds a workflow task item, and the live ledger when there is one, into the view both surfaces render. A missing
/// ledger degrades to the roster rather than to an empty run.
pub fn derive_workflow_run(item: &TaskItem, progress: Option<&WorkflowProgressView>) -> WorkflowRunView {
    let TaskKind::Workflow { agent, steps: item_steps, .. } = &item.kind else { panic!("derive_workflow_run takes a workflow row") };
    struct Row<'a> {
        row: &'a crate::workflow_progress::WorkflowStepRow,
        phase: Option<String>,
        phase_index: i64,
    }
    let rows: Vec<Row<'_>> = progress
        .map(|p| p.phases.as_slice())
        .unwrap_or_default()
        .iter()
        .flat_map(|phase| phase.steps.iter().map(move |row| Row { row, phase: phase.phase_index.map(|_| phase.heading.clone()), phase_index: phase.phase_index.unwrap_or(UNPHASED_ORDER) }))
        .collect();
    let mut claimed: Vec<i64> = Vec::new();
    let mut slots: Vec<Slot> = Vec::new();
    for (position, step) in item_steps.iter().enumerate() {
        let metrics = item.children.get(position).map(|c| c.metrics);
        // The slot the host stated first, and only then the label. Position is the tempting fallback and the wrong
        // one: it would hand a step the number of an unrelated row, and that number is what Skip sends.
        let stated = step.step_index.or_else(|| step_address(step.ledger_owner.as_deref()).map(|a| a.1));
        let free = |r: &&Row<'_>| !claimed.contains(&r.row.index);
        let matched = stated.and_then(|s| rows.iter().filter(free).find(|r| r.row.index == s)).or_else(|| rows.iter().filter(free).find(|r| r.row.label == step.label));
        if let Some(m) = matched {
            claimed.push(m.row.index);
        }
        let preview = prompt_preview(&step.task);
        slots.push(Slot {
            step: WorkflowRunStep {
                key: format!("agent:{}", step.id),
                plan_index: matched.map(|m| m.row.index).or(stated),
                label: step.label.clone(),
                state: task_state_for_status(step.status).into(),
                agent_id: Some(step.id.clone()),
                role: step.role.as_ref().map(|r| r.name.clone()),
                task: (!preview.is_empty()).then_some(preview),
                model_id: step.model_id.clone().or_else(|| step.role.as_ref().and_then(|r| r.model_id.clone())),
                tokens: metrics.and_then(|m| m.tokens),
                elapsed_ms: metrics.and_then(|m| m.elapsed_ms),
                cached: false,
                blocked: false,
                skipped: matched.is_some_and(|m| m.row.skipped),
            },
            phase: step.phase.clone().or_else(|| matched.and_then(|m| m.phase.clone())),
            phase_index: step.phase_index.or(matched.map(|m| m.phase_index)).unwrap_or(UNPHASED_ORDER),
            order: matched.map_or(rows.len() as i64 + position as i64, |m| m.row.index),
        });
    }
    for entry in &rows {
        if claimed.contains(&entry.row.index) {
            continue;
        }
        let r = entry.row;
        slots.push(Slot {
            step: WorkflowRunStep {
                key: format!("slot:{}", r.index),
                plan_index: Some(r.index),
                label: r.label.clone(),
                state: if r.skipped { WorkflowRunStepState::Failed } else { ledger_state(r.state, r.blocked) },
                agent_id: None,
                role: None,
                task: None,
                model_id: None,
                tokens: None,
                elapsed_ms: None,
                cached: r.cached,
                blocked: r.blocked,
                skipped: r.skipped,
            },
            phase: entry.phase.clone(),
            phase_index: entry.phase_index,
            order: r.index,
        });
    }
    slots.sort_by_key(|s| s.order);
    let steps: Vec<WorkflowRunStep> = slots.iter().map(|s| s.step.clone()).collect();
    // By declared index, not title: a plan may reuse a phase name.
    let mut groups: Vec<(i64, WorkflowRunPhase)> = Vec::new();
    for slot in &slots {
        let key = if slot.phase_index == UNPHASED_ORDER { "unphased".to_owned() } else { slot.phase_index.to_string() };
        let at = groups.iter().position(|g| g.1.key == key).unwrap_or_else(|| {
            groups.push((slot.phase_index, WorkflowRunPhase { key, heading: slot.phase.clone(), steps: Vec::new(), done: 0, total: 0 }));
            groups.len() - 1
        });
        let group = &mut groups[at].1;
        group.steps.push(slot.step.clone());
        group.total += 1;
        if slot.step.state == WorkflowRunStepState::Finished {
            group.done += 1;
        }
    }
    groups.sort_by_key(|g| g.0);
    WorkflowRunView {
        id: item.id.clone(),
        run_id: item_steps.iter().find_map(|s| step_address(s.ledger_owner.as_deref()).map(|a| a.0)),
        name: item.label.clone(),
        script_name: agent.script_name.clone(),
        description: js_trim(&agent.task).to_owned(),
        state: item.state,
        step_count: slots.len(),
        elapsed_ms: item.metrics.elapsed_ms,
        tokens: item.metrics.tokens,
        phases: groups.into_iter().map(|g| g.1).collect(),
        steps,
        logs: progress.map(|p| p.logs.iter().map(|l| l.message.clone()).collect()).unwrap_or_default(),
        running: item.state == TaskItemState::Running,
    }
}
