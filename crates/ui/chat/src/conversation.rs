//! `ConversationView.tsx` and `StreamedConversationView.tsx`: the single conversation surface used by both the primary and
//! child sessions. A read-only child is the same surface with mutation controls and composer physically omitted.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;
use mewrk_client::runs::{ConversationTurn, LiveContext, RunView, StreamingToolPhase, SubagentLiveState, SubagentLiveStatus};
use mewrk_pane_tasks::context::{Ctx, LiveState, LiveStatus};
use mewrk_pane_tasks::subagent_panel::TranscriptArgs;
use mewrk_pane_tasks::task_container::{derive_workflow_items, task_container_messages};
use mewrk_pane_tasks::workflow_progress::{derive_workflow_progress, workflow_progress_messages, WorkflowProgressEntry, WorkflowProgressView};
use mewrk_pane_tasks::workflow_runs::{derive_workflow_run, WorkflowRunView};
use mewrk_pane_tasks::task_container::TaskKind;

use crate::props::ConversationViewProps;
use crate::stream::context_stream;
use crate::turn_changes::subagent_change_spans;

/// `ConversationView`.
pub fn conversation_view(app: &mut App, props: ConversationViewProps) -> E {
    let props = Rc::new(props);
    let mut view = col().flex1().min_w(0.0).min_h(0.0);
    if let Some(slot) = &props.before_timeline {
        view = view.child(slot(app));
    }
    view = view.child(match &props.timeline_placeholder {
        Some(slot) => slot(app),
        None => context_stream(app, props.clone()),
    });
    if props.editable
        && let Some(slot) = &props.composer
    {
        view = view.child(slot(app));
    }
    view
}

#[derive(Default)]
struct StreamedCache {
    turns_key: u64,
    turns: Rc<Vec<ConversationTurn>>,
    workflow_key: u64,
    workflow: Rc<HashMap<String, Rc<WorkflowRunView>>>,
    progress_at: usize,
    progress: Rc<BTreeMap<String, Rc<WorkflowProgressView>>>,
}

/// The tasks pane's copy of the same wire shape.
fn progress_entries(entries: &[mewrk_client::runs::WorkflowProgressEntry]) -> Vec<WorkflowProgressEntry> {
    entries.iter().filter_map(|entry| serde_json::to_value(entry).ok().and_then(|value| serde_json::from_value(value).ok())).collect()
}

/// `StreamedConversationView`: persistent conversation contexts plus the active run's streaming projection. Fine-grained
/// model-stream reads stay here, so what a stream commits reaches this view and nothing above it.
pub fn streamed_conversation_view(app: &mut App, mut props: ConversationViewProps) -> E {
    let Some(id) = props.conversation_id.clone() else { return conversation_view(app, props) };
    let run: Option<Arc<RunView>> = app.run_view(&id);
    // A workflow that outlived the last run reports through the conversation's task overlay until the next one starts.
    let overlay = if run.is_none() { app.task_overlay(&id) } else { None };
    props.timeline_id = Some(id.clone());
    props.streaming = run.is_some();
    props.thinking = run.as_ref().and_then(|run| run.thinking.clone());
    props.retry_notice = run.as_ref().and_then(|run| run.retry.clone());
    let cache = use_state(("streamed-view", id.as_str()), StreamedCache::default);
    let mut cached = cache.borrow_mut();

    // `renderedTurns`: only when a turn or a commit changed. Upstream's `useMemo`.
    let turns_key = hash_key((app.conversation_turns_revision(), run.as_ref().map(|run| run.revision)));
    if cached.turns_key != turns_key || cached.turns_key == 0 {
        cached.turns_key = turns_key;
        cached.turns = Rc::new(app.rendered_turns(&id));
    }
    props.turns = cached.turns.clone();

    // Folded progress views for every workflow call streaming here, keyed on the run's progress entries, not on the run: the
    // run is new on every flush, and its entries only when a workflow reports.
    let progress_by_call = run.as_ref().map(|run| &run.workflow_progress_by_call).or(overlay.as_ref().map(|overlay| &overlay.workflow_progress_by_call));
    let progress_at = progress_by_call.map_or(0, |progress| Arc::as_ptr(progress) as usize);
    if cached.progress_at != progress_at {
        cached.progress_at = progress_at;
        let messages = workflow_progress_messages();
        cached.progress = Rc::new(progress_by_call.into_iter().flat_map(|progress| progress.iter()).map(|(call, entries)| (call.clone(), Rc::new(derive_workflow_progress(&progress_entries(entries), &messages)))).collect());
    }

    // A running workflow's card shows its own and its steps' elapsed time, and nothing else re-derives it while a step sits in one long call.
    let running = props.agents.iter().any(|agent| agent.workflow_run && agent.completed_at.is_none());
    let now = mewrk_pane_tasks::util::now_ms();
    let workflow_key = hash_key((Rc::as_ptr(&props.agents) as usize, progress_at, running.then_some(now / 1000)));
    if cached.workflow_key != workflow_key {
        cached.workflow_key = workflow_key;
        // One card view per workflow call. A run is registered under every call id that touched it, so the timeline node finds itself whichever call the projection settled on.
        let mut by_call = HashMap::new();
        for item in derive_workflow_items(&props.agents, &task_container_messages(), now, None) {
            let TaskKind::Workflow { agent, .. } = &item.kind else { continue };
            let progress = agent.call_ids.iter().find_map(|call| cached.progress.get(call).cloned());
            let view = Rc::new(derive_workflow_run(&item, progress.as_deref()));
            for call in &agent.call_ids {
                by_call.insert(call.clone(), view.clone());
            }
        }
        cached.workflow = Rc::new(by_call);
    }
    props.workflow_run_by_call = cached.workflow.clone();
    drop(cached);

    let view = conversation_view(app, props);
    if running {
        return view.child(custom::<App>(|_, ui, _| ui.wake_after(1.0)).size(1.0, 1.0).absolute().pass_through());
    }
    view
}

fn live_context_of(ctx: &Ctx) -> LiveContext {
    LiveContext {
        item: ctx.item.clone(),
        streaming: ctx.streaming.then_some(true),
        stream_status: ctx.stream_status.as_deref().and_then(|status| match status {
            "announced" => Some(StreamingToolPhase::Announced),
            "ready" => Some(StreamingToolPhase::Ready),
            "running" => Some(StreamingToolPhase::Running),
            "completed" => Some(StreamingToolPhase::Completed),
            _ => None,
        }),
        live: ctx.live.as_ref().map(|live| Arc::new(live_state_of(live))),
        started_at: None,
    }
}

fn live_state_of(live: &LiveState) -> SubagentLiveState {
    SubagentLiveState {
        contexts: live.contexts.iter().map(live_context_of).collect(),
        updates: live.updates.clone(),
        status: live.status.map(|status| match status {
            LiveStatus::Running => SubagentLiveStatus::Running,
            LiveStatus::Idle | LiveStatus::Completed => SubagentLiveStatus::Idle,
            LiveStatus::Interrupted => SubagentLiveStatus::Interrupted,
            LiveStatus::Failed => SubagentLiveStatus::Failed,
            LiveStatus::Stopped => SubagentLiveStatus::Stopped,
            LiveStatus::RoundLimit => SubagentLiveStatus::RoundLimit,
        }),
        ..SubagentLiveState::default()
    }
}

#[derive(Default)]
struct TranscriptCache {
    key: u64,
    contexts: Rc<Vec<LiveContext>>,
    spans: Option<Rc<Vec<crate::turn_changes::ChangeSpan>>>,
}

/// The subagent drawer's transcript: the same surface, read-only, for `SubagentPanelProps::transcript`. `tools` names the tools
/// a row looks up its label by.
pub fn transcript_view(app: &mut App, args: TranscriptArgs<'_>, tools: Rc<Vec<mewrk_client::model::ToolDescriptor>>, on_open_subagent: Option<crate::props::OnId>) -> E {
    // A transcript's identity is its first context; a child's page is one timeline however much it streams.
    let first = args.contexts.first().map_or("", Ctx::id);
    let cache = use_state(("transcript-view", first), TranscriptCache::default);
    let mut cached = cache.borrow_mut();
    // The contexts are rebuilt whenever the panel derives its views, so the conversion follows their contents rather than their address.
    let key = hash_key((args.contexts.len(), args.contexts.last().map(|ctx| ctx.id()), args.contexts.last().map(|ctx| serde_json::to_string(ctx).map(|text| text.len()).unwrap_or(0)), args.streaming));
    if cached.key != key {
        cached.key = key;
        cached.contexts = Rc::new(args.contexts.iter().map(live_context_of).collect());
        let items: Vec<&ContextItem> = args.contexts.iter().map(|ctx| &ctx.item).collect();
        cached.spans = Some(Rc::new(subagent_change_spans(&items, args.streaming)));
    }
    let contexts = cached.contexts.clone();
    let change_spans = cached.spans.clone();
    drop(cached);
    conversation_view(
        app,
        ConversationViewProps {
            editable: false,
            contexts,
            change_spans,
            tools,
            streaming: args.streaming,
            timeline_id: Some(format!("transcript:{first}")),
            path_base_dir: args.path_base_dir.map(str::to_owned),
            aria_label: Some(args.aria_label),
            on_open_subagent,
            ..ConversationViewProps::default()
        },
    )
}
