//! `lib/workflowProgress.ts`: a flat row stream folded into a phase and step tree. The fold is idempotent and
//! order-insensitive, because the renderer gets transitions rather than the host's ledger, possibly replayed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowStepState {
    Start,
    Progress,
    Done,
    Error,
}

/// One row of a run's progress ledger, mirroring `ProgressRow` in `workflow-core`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowProgressEntry {
    pub kind: String,
    pub index: i64,
    pub state: WorkflowStepState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_index: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub blocked: bool,
    #[serde(default)]
    pub skipped: bool,
}

/// The run state's entries (`ModelRunState::workflow_progress_by_call`), which this fold reads. Mine keeps `kind` a string so
/// a row kind from a newer host degrades to a partial card rather than an error.
impl From<&mewrk_client::runs::WorkflowProgressEntry> for WorkflowProgressEntry {
    fn from(e: &mewrk_client::runs::WorkflowProgressEntry) -> Self {
        use mewrk_client::runs::{WorkflowEntryKind as K, WorkflowStepState as S};
        Self {
            kind: match e.kind {
                K::Agent => "agent",
                K::Log => "log",
            }
            .to_owned(),
            index: e.index as i64,
            state: match e.state {
                S::Start => WorkflowStepState::Start,
                S::Progress => WorkflowStepState::Progress,
                S::Done => WorkflowStepState::Done,
                S::Error => WorkflowStepState::Error,
            },
            label: e.label.clone(),
            phase: e.phase.clone(),
            phase_index: e.phase_index.map(|i| i as i64),
            preview: e.preview.clone(),
            message: e.message.clone(),
            cached: e.cached,
            blocked: e.blocked,
            skipped: e.skipped,
        }
    }
}

pub struct WorkflowProgressMessages {
    pub fallback_step_label: String,
    pub unphased_heading: String,
    pub cached_badge: String,
    pub blocked_badge: String,
    pub skipped_badge: String,
    pub progress_label: Box<dyn Fn(usize, usize) -> String>,
}

impl Default for WorkflowProgressMessages {
    fn default() -> Self {
        Self {
            fallback_step_label: "步骤".into(),
            unphased_heading: "未分组".into(),
            cached_badge: "已缓存".into(),
            blocked_badge: "等待中".into(),
            skipped_badge: "已跳过".into(),
            progress_label: Box::new(|done, total| format!("工作流进度：{total} 步中已完成 {done} 步")),
        }
    }
}

/// The bag `App.tsx` and `StreamedConversationView.tsx` both build.
pub fn workflow_progress_messages() -> WorkflowProgressMessages {
    use mewrk_client::{t, tf};
    WorkflowProgressMessages {
        fallback_step_label: t("步骤", "Step").into(),
        unphased_heading: t("未分组", "Ungrouped").into(),
        cached_badge: t("已缓存", "Cached").into(),
        blocked_badge: t("等待中", "Waiting").into(),
        skipped_badge: t("已跳过", "Skipped").into(),
        progress_label: Box::new(|done, total| tf("工作流进度：{total} 步中已完成 {done} 步", "Workflow progress: {done} of {total} steps completed", &[("done", &done), ("total", &total)])),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowStepRow {
    pub index: i64,
    pub label: String,
    pub state: WorkflowStepState,
    pub preview: Option<String>,
    pub message: Option<String>,
    pub cached: bool,
    pub blocked: bool,
    pub skipped: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressPhaseGroup {
    pub phase_index: Option<i64>,
    pub heading: String,
    pub steps: Vec<WorkflowStepRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowLogRow {
    pub index: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowProgressView {
    pub phases: Vec<ProgressPhaseGroup>,
    pub logs: Vec<WorkflowLogRow>,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub progress_label: String,
    pub running: bool,
}

const MAX_LOG_ROWS: usize = 1_000;
const LOG_TRIM_TARGET: usize = 500;

/// Pure and total. Unknown row kinds are dropped: a host newer than the renderer degrades to a partial card.
pub fn derive_workflow_progress(entries: &[WorkflowProgressEntry], messages: &WorkflowProgressMessages) -> WorkflowProgressView {
    let mut steps: BTreeMap<i64, &WorkflowProgressEntry> = BTreeMap::new();
    let mut logs: Vec<WorkflowLogRow> = Vec::new();
    for entry in entries {
        match entry.kind.as_str() {
            "log" => logs.push(WorkflowLogRow { index: entry.index, message: entry.message.clone().unwrap_or_default() }),
            "agent" => {
                steps.insert(entry.index, entry);
            }
            _ => {}
        }
    }
    if logs.len() > MAX_LOG_ROWS {
        logs.drain(0..logs.len() - LOG_TRIM_TARGET);
    }
    // By declared index, not heading text: two phases may share a name.
    let mut phases: Vec<ProgressPhaseGroup> = Vec::new();
    for step in steps.values() {
        let row = WorkflowStepRow {
            index: step.index,
            label: step.label.clone().unwrap_or_else(|| messages.fallback_step_label.clone()),
            state: step.state,
            preview: step.preview.clone(),
            message: step.message.clone(),
            cached: step.cached,
            blocked: step.blocked,
            skipped: step.skipped,
        };
        match phases.iter_mut().find(|g| g.phase_index == step.phase_index) {
            Some(group) => group.steps.push(row),
            None => phases.push(ProgressPhaseGroup { phase_index: step.phase_index, heading: step.phase.clone().unwrap_or_else(|| messages.unphased_heading.clone()), steps: vec![row] }),
        }
    }
    // Unphased steps sort last: the plan never placed them.
    phases.sort_by_key(|g| (g.phase_index.is_none(), g.phase_index));
    let all = || phases.iter().flat_map(|p| p.steps.iter());
    let done = all().filter(|s| s.state == WorkflowStepState::Done).count();
    // Skipped counts as failed, following the host ledger.
    let failed = all().filter(|s| s.state == WorkflowStepState::Error).count();
    let running = all().any(|s| matches!(s.state, WorkflowStepState::Start | WorkflowStepState::Progress));
    let total = all().count();
    let progress_label = (messages.progress_label)(done, total);
    WorkflowProgressView { phases, logs, total, done, failed, progress_label, running }
}
