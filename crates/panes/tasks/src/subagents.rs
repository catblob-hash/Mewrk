//! `lib/subagents.ts`: one view per child agent, merged from every parent tool call that touched it.
//!
//! The rules that look arbitrary are upstream's, and upstream's comments explain each; the ones repeated here are
//! those a reader would otherwise "simplify" away.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mewrk_client::model::{ContextItem, ModelUsage, SubagentRunKind, SubagentRunRecord, SubagentUpdate, ToolResult};
use serde_json::{Map, Value};

use crate::context::{tool_of, Ctx, LiveState, LiveStatus, SubagentViewStatus, ToolRef};
use crate::util::{collapse_spaces, js_trim, timestamp, utf16_len, utf16_prefix};

#[derive(Clone, Debug, PartialEq)]
pub struct AgentRole {
    pub name: String,
    pub model_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SubagentView {
    pub id: String,
    /// Addressable name; `None` on legacy views and workflow steps, which nothing can address.
    pub name: Option<String>,
    /// Whose history entries are this agent's. `None` asks for nothing, never for the conversation's own.
    pub ledger_owner: Option<String>,
    pub kind: SubagentRunKind,
    /// This view is the workflow run (the script driver), not one of the agents it starts.
    pub workflow_run: bool,
    pub label: String,
    pub task: String,
    pub status: SubagentViewStatus,
    pub summary: String,
    pub contexts: Vec<Ctx>,
    pub updates: Vec<SubagentUpdate>,
    pub live: Option<LiveState>,
    /// The context ids of the tool cards this view folds — not provider call ids. Workflow progress is keyed by these.
    pub call_ids: Vec<String>,
    pub parent_id: Option<String>,
    pub depth: usize,
    pub child_ids: Vec<String>,
    pub phase: Option<String>,
    pub phase_index: Option<i64>,
    pub step_index: Option<i64>,
    pub role: Option<AgentRole>,
    pub model_id: Option<String>,
    pub script_name: Option<String>,
    pub usage: ModelUsage,
    /// `None` is unknowable, not zero: an externalized step whose shell predates the count.
    pub tool_count: Option<u64>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Clone, Debug)]
pub struct SubagentViewMessages {
    pub fallback_label: String,
    pub workflow_step_label: String,
    pub missing_task: String,
    pub update_returned: String,
    pub running: String,
    pub interrupted: String,
    pub failed: String,
    pub stopped: String,
    pub round_limit: String,
    pub completed: String,
}

impl Default for SubagentViewMessages {
    fn default() -> Self {
        Self {
            fallback_label: "子代理".into(),
            workflow_step_label: "工作流步骤".into(),
            missing_task: "未提供任务说明".into(),
            update_returned: "状态已返回给主智能体".into(),
            running: "正在工作".into(),
            interrupted: "子代理已中断".into(),
            failed: "子代理已失败".into(),
            stopped: "子代理已停止".into(),
            round_limit: "子代理已达轮次上限".into(),
            completed: "子代理已完成".into(),
        }
    }
}

/// `subagentViewMessages(t)` from `StreamedConversationView.tsx`.
pub fn subagent_view_messages() -> SubagentViewMessages {
    use mewrk_client::t;
    SubagentViewMessages {
        fallback_label: t("子代理", "Subagent").into(),
        workflow_step_label: t("工作流步骤", "Workflow step").into(),
        missing_task: t("未提供任务说明", "No task description provided").into(),
        update_returned: t("状态已返回给主智能体", "Status returned to the primary agent").into(),
        running: t("正在工作", "Working").into(),
        interrupted: t("子代理已中断", "Subagent interrupted").into(),
        failed: t("子代理已失败", "Subagent failed").into(),
        stopped: t("子代理已停止", "Subagent stopped").into(),
        round_limit: t("子代理已达轮次上限", "Subagent hit its round limit").into(),
        completed: t("子代理已完成", "Subagent completed").into(),
    }
}

fn is_agent_run_tool(name: &str) -> bool {
    matches!(name, "subagent" | "agent_spawn" | "agent_send" | "send_message" | "followup_task" | "workflow" | "workflow_step")
}

/// Run tools whose child is conditional: a call rejected before its child exists must not open an empty thread.
fn is_optional_child_run_tool(name: &str) -> bool {
    matches!(name, "workflow" | "workflow_step")
}

fn is_agent_message_tool(name: &str) -> bool {
    matches!(name, "agent_send" | "send_message" | "followup_task")
}

fn string_input(item: &ToolRef<'_>, key: &str) -> String {
    match item.input.get(key) {
        Some(Value::String(s)) => js_trim(s).to_owned(),
        _ => String::new(),
    }
}

fn number_input(item: &ToolRef<'_>, key: &str) -> Option<i64> {
    item.input.get(key).and_then(Value::as_f64).filter(|v| v.is_finite()).map(|v| v as i64)
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn agent_role(record: Option<&SubagentRunRecord>, items: &[ToolRef<'_>]) -> Option<AgentRole> {
    if let Some(binding) = record.and_then(|r| r.agent_definition.as_ref()) {
        return Some(AgentRole { name: binding.name.clone(), model_id: Some(binding.model_id.clone()) });
    }
    // Each key is gated to the one tool that owns it: a call carries whatever the model wrote.
    for item in items.iter().rev() {
        if item.tool_name == "workflow_step" {
            let bound = string_input(item, "role");
            if !bound.is_empty() {
                return Some(AgentRole { name: bound, model_id: non_empty(string_input(item, "roleModelId")) });
            }
        }
        if item.tool_name == "agent_spawn" {
            let requested = string_input(item, "agent_type");
            if !requested.is_empty() {
                return Some(AgentRole { name: requested, model_id: None });
            }
        }
    }
    None
}

fn agent_model_id(record: Option<&SubagentRunRecord>, role: Option<&AgentRole>) -> Option<String> {
    record
        .and_then(|r| r.agent_definition.as_ref().map(|b| b.model_id.clone()))
        .or_else(|| record.and_then(|r| r.fork_model_binding.as_ref().map(|b| b.model_id.clone())))
        .or_else(|| role.and_then(|r| r.model_id.clone()))
}

/// A child's own `send_message`, the retired form with no recipient. Same wire name as the main agent's call and
/// nothing else in common: it starts no child and addresses none.
pub fn is_child_main_message_context(item: &ToolRef<'_>) -> bool {
    item.tool_name == "send_message" && string_input(item, "target").is_empty() && string_input(item, "to").is_empty()
}

/// The child the main agent's `send_message` addresses by `to`, or "".
pub fn send_message_recipient(item: &ToolRef<'_>) -> String {
    if item.tool_name != "send_message" || !string_input(item, "target").is_empty() {
        return String::new();
    }
    string_input(item, "to")
}

/// The call holds a child run: the record, or a stream with anything on it. An empty `live` shell says nothing.
pub fn carries_child_run(item: &ToolRef<'_>) -> bool {
    item.subagent.is_some() || item.live.is_some_and(|l| l.status.is_some() || !l.contexts.is_empty() || !l.updates.is_empty())
}

/// A `send_message` that owns no run: only a receipt, which never speaks for the child's status.
pub fn is_queued_message_context(item: &ToolRef<'_>) -> bool {
    !send_message_recipient(item).is_empty() && !carries_child_run(item)
}

fn accepts_background_run(item: &ToolRef<'_>) -> bool {
    matches!(item.tool_name, "agent_spawn" | "agent_send" | "followup_task") || (!send_message_recipient(item).is_empty() && carries_child_run(item))
}

/// Stable route used by timeline affordances across streaming and persisted contexts.
pub fn agent_timeline_route_id(item: &ToolRef<'_>) -> String {
    if item.tool_name == "workflow_step" {
        return agent_key_for_context(item).address.unwrap_or_else(|| item.id.to_owned());
    }
    item.subagent
        .and_then(|s| s.name.as_deref())
        .map(js_trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .or_else(|| non_empty(string_input(item, "target")))
        .or_else(|| non_empty(send_message_recipient(item)))
        .or_else(|| non_empty(string_input(item, "agent")))
        .or_else(|| non_empty(string_input(item, "name")))
        .unwrap_or_else(|| item.id.to_owned())
}

fn live_status_of(item: &ToolRef<'_>) -> Option<LiveStatus> {
    item.live.and_then(|l| l.status)
}

/// The run status a timeline row must show for an agent call.
pub fn agent_timeline_run_status(item: &ToolRef<'_>) -> SubagentViewStatus {
    if item.streaming
        && let Some(s) = live_status_of(item)
    {
        return s.into();
    }
    if item.streaming && item.stream_status != Some("completed") {
        return SubagentViewStatus::Running;
    }
    if item.streaming && item.result.success && accepts_background_run(item) && item.subagent.is_none() {
        return SubagentViewStatus::Running;
    }
    if let Some(s) = item.subagent {
        return s.status.into();
    }
    if let Some(s) = live_status_of(item).filter(|s| *s != LiveStatus::Running) {
        return s.into();
    }
    if item.result.success { SubagentViewStatus::Completed } else { SubagentViewStatus::Interrupted }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentAvatarTone {
    Amber,
    Green,
    Mint,
    Blue,
    Violet,
    Rose,
}

pub fn hash_subagent_id(value: &str) -> u32 {
    let mut hash: u32 = 2_166_136_261;
    for c in value.chars() {
        hash ^= c as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

pub fn subagent_avatar_tone(id: &str) -> SubagentAvatarTone {
    use SubagentAvatarTone::*;
    [Amber, Green, Mint, Blue, Violet, Rose][hash_subagent_id(id) as usize % 6]
}

/// Where a `workflow_step`'s full transcript lives. Written by the host only once the disk copy is confirmed, so
/// its presence is the promise that a body exists.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ExternalStepBodyRef {
    pub run_id: String,
    pub step_index: i64,
}

fn external_step_body_ref(item: &ToolRef<'_>) -> Option<ExternalStepBodyRef> {
    if item.tool_name != "workflow_step" || item.subagent.is_some() {
        return None;
    }
    let run_id = string_input(item, "runId");
    let step_index = number_input(item, "stepIndex")?;
    (!run_id.is_empty()).then_some(ExternalStepBodyRef { run_id, step_index })
}

fn finite_token_count(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0).map(|v| v.floor() as u64)
}

/// `normalizeModelUsage`.
pub fn normalize_model_usage(usage: &Map<String, Value>) -> ModelUsage {
    ModelUsage {
        input_tokens: finite_token_count(usage.get("inputTokens")),
        cached_input_tokens: finite_token_count(usage.get("cachedInputTokens")),
        output_tokens: finite_token_count(usage.get("outputTokens")),
        total_tokens: finite_token_count(usage.get("totalTokens")),
        reasoning_tokens: None,
    }
}

fn external_step_usage(item: &ToolRef<'_>) -> Option<ModelUsage> {
    if item.tool_name != "workflow_step" || item.subagent.is_some() {
        return None;
    }
    let normalized = normalize_model_usage(item.input.get("usage")?.as_object()?);
    (normalized != ModelUsage::default()).then_some(normalized)
}

/// `None`: not that kind of agent, count the transcript. `Some(None)`: an externalized step with no count on its
/// shell, where counting the absent transcript would be a confident zero.
fn external_step_tool_count(items: &[ToolRef<'_>]) -> Option<Option<u64>> {
    let shell = items.iter().rev().find(|i| external_step_body_ref(i).is_some())?;
    Some(number_input(shell, "toolUseCount").map(|v| v.max(0) as u64))
}

fn external_step_status(item: &ToolRef<'_>) -> Option<SubagentViewStatus> {
    use SubagentViewStatus::*;
    match string_input(item, "status").as_str() {
        "completed" => Some(Completed),
        "interrupted" => Some(Interrupted),
        "failed" => Some(Failed),
        "stopped" => Some(Stopped),
        "roundLimit" => Some(RoundLimit),
        _ => None,
    }
}

fn graft_item(item: &ContextItem, lookup: &dyn Fn(&ExternalStepBodyRef) -> Option<SubagentRunRecord>) -> Option<ContextItem> {
    let ContextItem::Tool { subagent, .. } = item else { return None };
    let tool = tool_of(item)?;
    if let Some(reference) = external_step_body_ref(&tool) {
        let record = lookup(&reference)?;
        let mut out = item.clone();
        if let ContextItem::Tool { subagent, .. } = &mut out {
            *subagent = Some(record);
        }
        return Some(out);
    }
    let record = subagent.as_ref()?;
    let nested = graft_items(&record.contexts, lookup)?;
    let mut out = item.clone();
    if let ContextItem::Tool { subagent: Some(r), .. } = &mut out {
        r.contexts = nested;
    }
    Some(out)
}

fn graft_items(contexts: &[ContextItem], lookup: &dyn Fn(&ExternalStepBodyRef) -> Option<SubagentRunRecord>) -> Option<Vec<ContextItem>> {
    let mut changed = false;
    let grafted: Vec<ContextItem> = contexts
        .iter()
        .map(|c| match graft_item(c, lookup) {
            Some(n) => {
                changed = true;
                n
            }
            None => c.clone(),
        })
        .collect();
    changed.then_some(grafted)
}

/// Grafts loaded step bodies back onto their externalized contexts, recursing through nested records. Borrowed back
/// when nothing landed, so a memo keyed on the result stays put.
pub fn graft_external_step_bodies<'a>(contexts: &'a [Ctx], lookup: &dyn Fn(&ExternalStepBodyRef) -> Option<SubagentRunRecord>) -> Cow<'a, [Ctx]> {
    let mut changed = false;
    let grafted: Vec<Ctx> = contexts
        .iter()
        .map(|c| match graft_item(&c.item, lookup) {
            Some(item) => {
                changed = true;
                Ctx { item, ..c.clone() }
            }
            None => c.clone(),
        })
        .collect();
    if changed { Cow::Owned(grafted) } else { Cow::Borrowed(contexts) }
}

fn find_ref_in_item(item: &ContextItem, call_ids: &[String]) -> Option<ExternalStepBodyRef> {
    let tool = tool_of(item)?;
    if call_ids.iter().any(|c| c == tool.id)
        && let Some(r) = external_step_body_ref(&tool)
    {
        return Some(r);
    }
    tool.subagent?.contexts.iter().find_map(|c| find_ref_in_item(c, call_ids))
}

/// The coordinates behind one drawer view's calls. A step already grafted carries a record again and yields nothing,
/// so it never triggers a second load.
pub fn find_external_step_body_ref(contexts: &[Ctx], call_ids: &[String]) -> Option<ExternalStepBodyRef> {
    contexts.iter().find_map(|c| find_ref_in_item(&c.item, call_ids))
}

fn compact_text(value: &str, limit: usize) -> String {
    let compact = collapse_spaces(value);
    if utf16_len(&compact) > limit {
        format!("{}…", utf16_prefix(&compact, limit - 1).trim_end_matches(crate::util::is_js_space))
    } else {
        compact
    }
}

fn merged_updates(items: &[ToolRef<'_>]) -> Vec<SubagentUpdate> {
    let mut seen = HashSet::new();
    let mut valid: Vec<SubagentUpdate> = Vec::new();
    for item in items {
        let record = item.subagent.map(|s| s.updates.as_slice()).unwrap_or_default();
        let live = item.live.map(|l| l.updates.as_slice()).unwrap_or_default();
        for update in record.iter().chain(live) {
            let content = js_trim(&update.content);
            if content.is_empty() || !seen.insert((update.created_at.clone(), content.to_owned())) {
                continue;
            }
            valid.push(SubagentUpdate { content: content.to_owned(), created_at: update.created_at.clone() });
        }
    }
    valid.sort_by_key(|u| timestamp(Some(&u.created_at)));
    valid
}

fn is_update_tool(name: &str) -> bool {
    name == "subagent_update" || name == "update"
}

fn user_ctx(id: String, content: String, created_at: String) -> Ctx {
    ContextItem::User { id, content, images: Vec::new(), files: Vec::new(), created_at }.into()
}

fn assistant_ctx(id: String, content: String, created_at: String) -> Ctx {
    ContextItem::Assistant { id, content, round: None, model_turn_id: None, interrupted: false, sources: Vec::new(), created_at }.into()
}

fn synthetic_update(id: &str, update: &SubagentUpdate, index: usize, messages: &SubagentViewMessages) -> Ctx {
    let mut input = Map::new();
    input.insert("message".to_owned(), Value::String(update.content.clone()));
    ContextItem::Tool {
        id: format!("{id}:update:{index}"),
        tool_name: "subagent_update".to_owned(),
        round: None,
        model_turn_id: None,
        provider_call_id: None,
        requested_input: None,
        input,
        result: ToolResult { success: true, output: messages.update_returned.clone(), images: Vec::new(), diff: None, executed_at: update.created_at.clone(), duration_ms: 0 },
        subagent: None,
        notice: None,
        attestation: String::new(),
        created_at: update.created_at.clone(),
    }
    .into()
}

fn chronological(mut contexts: Vec<Ctx>) -> Vec<Ctx> {
    contexts.sort_by_key(|c| timestamp(Some(c.created_at())));
    contexts
}

fn with_supplemental_contexts(id: &str, contexts: Vec<Ctx>, updates: &[SubagentUpdate], messages: &SubagentViewMessages) -> Vec<Ctx> {
    let mut structured: HashMap<String, usize> = HashMap::new();
    for context in &contexts {
        if let Some(tool) = context.tool().filter(|t| is_update_tool(t.tool_name)) {
            let content = string_input(&tool, "message");
            if !content.is_empty() {
                *structured.entry(content).or_default() += 1;
            }
        }
    }
    let mut supplemental = Vec::new();
    for (index, update) in updates.iter().enumerate() {
        match structured.get_mut(&update.content) {
            Some(count) if *count > 0 => *count -= 1,
            _ => supplemental.push(synthetic_update(id, update, index, messages)),
        }
    }
    if supplemental.is_empty() {
        return contexts;
    }
    let mut contexts = contexts.into_iter();
    let task = contexts.next();
    let mut body: Vec<Ctx> = contexts.collect();
    body.extend(supplemental);
    let merged = chronological(body);
    match task {
        Some(task) => std::iter::once(task).chain(merged).collect(),
        None => merged,
    }
}

fn last_assistant_text(contexts: &[Ctx]) -> String {
    contexts
        .iter()
        .rev()
        .find_map(|c| match &c.item {
            ContextItem::Assistant { content, .. } => {
                let t = js_trim(content);
                (!t.is_empty()).then(|| t.to_owned())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The task or message a run-starting call put in front of the child. The two tools name the field differently.
pub fn initial_message(item: &ToolRef<'_>) -> String {
    if is_agent_message_tool(item.tool_name) {
        return string_input(item, "message");
    }
    ["task", "prompt"].iter().map(|k| string_input(item, k)).find(|v| !v.is_empty()).unwrap_or_default()
}

struct Transcript {
    contexts: Vec<Ctx>,
    seen: HashSet<String>,
    persisted_users: Vec<String>,
}

impl Transcript {
    fn push(&mut self, context: Ctx, persisted: bool) {
        if !self.seen.insert(context.id().to_owned()) {
            return;
        }
        if persisted && context.is_user() {
            self.persisted_users.push(js_trim(context.content().unwrap_or_default()).to_owned());
        }
        self.contexts.push(context);
    }
}

/// One child conversation out of every parent call that touched it. Records are cumulative snapshots and live events
/// can stay attached to an older call, so all of them are unioned instead of one replacing another.
fn conversation_transcript(items: &[ToolRef<'_>], id: &str, task: &str, updates: &[SubagentUpdate], messages: &SubagentViewMessages) -> Vec<Ctx> {
    let mut t = Transcript { contexts: Vec::new(), seen: HashSet::new(), persisted_users: Vec::new() };
    for item in items {
        if let Some(record) = item.subagent {
            for context in &record.contexts {
                t.push(context.clone().into(), true);
            }
        }
    }
    // Calls and record turns pair in order: a later call may not claim a turn an earlier one already passed over.
    // Clocks cannot do this, since the host stamps a card after the child's own first turn.
    let mut matched_through = 0;
    for item in items {
        let message = initial_message(item);
        if !message.is_empty() {
            let persisted = t.persisted_users.iter().enumerate().position(|(i, c)| i >= matched_through && *c == message);
            if let Some(index) = persisted {
                matched_through = index + 1;
            } else if item.tool_name != "send_message" {
                t.push(user_ctx(format!("{id}:message:{}", item.id), message, item.created_at.to_owned()), false);
            }
        }
        if let Some(live) = item.live {
            for context in &live.contexts {
                t.push(context.clone(), false);
            }
        }
        // Old blocking `subagent` records stored only the tool output.
        let output = js_trim(&item.result.output);
        if item.tool_name == "subagent" && item.subagent.is_none_or(|s| s.contexts.is_empty()) && item.live.is_none_or(|l| l.contexts.is_empty()) && !output.is_empty() {
            let at = if item.result.executed_at.is_empty() { item.created_at } else { &item.result.executed_at };
            t.push(assistant_ctx(format!("{id}:answer:{}", item.id), output.to_owned(), at.to_owned()), false);
        }
    }
    let mut ordered = chronological(t.contexts);
    let task_index = if task.is_empty() { None } else { ordered.iter().position(|c| c.is_user() && js_trim(c.content().unwrap_or_default()) == task) };
    let task_context = match task_index {
        Some(i) => ordered.remove(i),
        None => user_ctx(format!("{id}:task"), if task.is_empty() { messages.missing_task.clone() } else { task.to_owned() }, items[0].created_at.to_owned()),
    };
    ordered.insert(0, task_context);
    with_supplemental_contexts(id, ordered, updates, messages)
}

fn context_status(item: &ToolRef<'_>) -> SubagentViewStatus {
    if item.streaming
        && let Some(s) = live_status_of(item)
    {
        return s.into();
    }
    if item.streaming && item.stream_status != Some("completed") {
        return SubagentViewStatus::Running;
    }
    // A spawn completes when the host accepted the call, not when the child has finished.
    if item.streaming && item.result.success && accepts_background_run(item) && item.subagent.is_none() {
        return SubagentViewStatus::Running;
    }
    if let Some(s) = item.subagent {
        return s.status.into();
    }
    if external_step_body_ref(item).is_some()
        && let Some(s) = external_step_status(item)
    {
        return s;
    }
    let settled = live_status_of(item);
    if let Some(s) = settled.filter(|s| *s != LiveStatus::Running) {
        return s.into();
    }
    if item.result.success && accepts_background_run(item) {
        return if settled == Some(LiveStatus::Running) { SubagentViewStatus::Running } else { SubagentViewStatus::Interrupted };
    }
    if item.result.success { SubagentViewStatus::Completed } else { SubagentViewStatus::Interrupted }
}

fn completed_at(items: &[ToolRef<'_>], status: SubagentViewStatus, contexts: &[Ctx], updates: &[SubagentUpdate]) -> Option<String> {
    if status == SubagentViewStatus::Running {
        return None;
    }
    let stamps = contexts
        .iter()
        .map(Ctx::created_at)
        .chain(updates.iter().map(|u| u.created_at.as_str()))
        .chain(items.iter().flat_map(|i| [i.result.executed_at.as_str(), i.created_at]))
        .filter(|s| !s.is_empty());
    let mut best: Option<(&str, i64)> = None;
    for stamp in stamps {
        let at = timestamp(Some(stamp));
        if best.is_none_or(|(_, b)| at > b) {
            best = Some((stamp, at));
        }
    }
    Some(best.map_or_else(|| items[0].created_at.to_owned(), |(s, _)| s.to_owned()))
}

fn summary_for(status: SubagentViewStatus, task: &str, contexts: &[Ctx], updates: &[SubagentUpdate], m: &SubagentViewMessages) -> String {
    let latest = updates.last().map_or("", |u| u.content.as_str());
    let assistant = last_assistant_text(contexts);
    let first = |options: &[&str]| options.iter().find(|s| !s.is_empty()).map_or("", |s| *s).to_owned();
    use SubagentViewStatus::*;
    let candidate = match status {
        Running => first(&[latest, &assistant, task, &m.running]),
        Interrupted => first(&[latest, &assistant, &m.interrupted]),
        Failed => first(&[latest, &assistant, &m.failed]),
        Stopped => first(&[latest, &assistant, &m.stopped]),
        RoundLimit => first(&[latest, &assistant, &m.round_limit]),
        Completed => first(&[&assistant, latest, task, &m.completed]),
    };
    compact_text(&candidate, 180)
}

fn combined_live_state(items: &[ToolRef<'_>]) -> Option<LiveState> {
    let streaming: Vec<ToolRef<'_>> = items.iter().filter(|i| i.streaming && i.live.is_some()).copied().collect();
    if streaming.is_empty() {
        return None;
    }
    let mut seen = HashSet::new();
    let contexts: Vec<Ctx> = streaming.iter().flat_map(|i| i.live.map(|l| l.contexts.iter()).into_iter().flatten()).filter(|c| seen.insert(c.id().to_owned())).cloned().collect();
    let status = streaming.iter().rev().find_map(|i| i.live.and_then(|l| l.status));
    Some(LiveState { contexts: chronological(contexts), updates: merged_updates(&streaming), status, usage_by_round: None })
}

/// `sumModelUsage`: a counter nobody reported stays absent.
pub fn sum_model_usage<'a>(usages: impl IntoIterator<Item = &'a ModelUsage> + Clone) -> ModelUsage {
    fn field<'a>(usages: impl IntoIterator<Item = &'a ModelUsage>, get: fn(&ModelUsage) -> Option<u64>) -> Option<u64> {
        usages.into_iter().filter_map(get).fold(None, |sum, v| Some(sum.unwrap_or(0) + v))
    }
    ModelUsage {
        input_tokens: field(usages.clone(), |u| u.input_tokens),
        cached_input_tokens: field(usages.clone(), |u| u.cached_input_tokens),
        output_tokens: field(usages.clone(), |u| u.output_tokens),
        total_tokens: field(usages.clone(), |u| u.total_tokens),
        reasoning_tokens: field(usages, |u| u.reasoning_tokens),
    }
}

/// Deliberately not gated on `streaming`: a settled step keeps its figure until its record is minted.
fn live_usage(items: &[ToolRef<'_>]) -> ModelUsage {
    let per_item: Vec<ModelUsage> = items.iter().filter_map(|i| i.live.and_then(|l| l.usage_by_round.as_ref())).map(|by_round| sum_model_usage(by_round.values())).collect();
    sum_model_usage(per_item.iter())
}

struct AgentKey {
    key: String,
    name: Option<String>,
    address: Option<String>,
}

fn agent_key_for_context(item: &ToolRef<'_>) -> AgentKey {
    let named = |name: String| AgentKey { key: format!("name:{name}"), name: Some(name), address: None };
    let by_context = || AgentKey { key: format!("context:{}", item.id), name: None, address: None };
    // A step is identified by its run-scoped address, never by the pool name its record carries: every run's pool
    // restarts at `ws1`, so that name would merge the first steps of every workflow in the conversation.
    if item.tool_name == "workflow_step" {
        let address = string_input(item, "ledger");
        if !address.is_empty() {
            return AgentKey { key: format!("step:{address}"), name: None, address: Some(address) };
        }
        return by_context();
    }
    let record_name = item.subagent.and_then(|s| s.name.as_deref()).map(js_trim).unwrap_or("");
    if !record_name.is_empty() {
        return named(record_name.to_owned());
    }
    if is_agent_message_tool(item.tool_name) {
        let target = [string_input(item, "target"), send_message_recipient(item), string_input(item, "agent")].into_iter().find(|s| !s.is_empty());
        if let Some(target) = target {
            return named(target);
        }
    }
    if matches!(item.tool_name, "agent_spawn" | "workflow") {
        let requested = string_input(item, "name");
        if !requested.is_empty() {
            return named(requested);
        }
    }
    by_context()
}

struct Accumulator<'a> {
    key: String,
    name: Option<String>,
    address: Option<String>,
    order: usize,
    contexts: Vec<ToolRef<'a>>,
}

/// Builds the card and drawer projection from the parent timeline's tool contexts: a pre-order flattening of the
/// whole agent tree, where `parent_id`, `depth` and `child_ids` carry the relation.
pub fn derive_subagent_views(contexts: &[Ctx], messages: &SubagentViewMessages) -> Vec<Rc<SubagentView>> {
    nested_subagent_views(contexts, messages, None, 0)
}

fn nested_subagent_views(contexts: &[Ctx], messages: &SubagentViewMessages, parent_id: Option<&str>, depth: usize) -> Vec<Rc<SubagentView>> {
    let mut accumulators: Vec<Accumulator<'_>> = Vec::new();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    for (order, context) in contexts.iter().enumerate() {
        let Some(tool) = context.tool().filter(|t| is_agent_run_tool(t.tool_name)) else { continue };
        if is_child_main_message_context(&tool) {
            continue;
        }
        let no_child = tool.subagent.is_none() && tool.live.is_none_or(|l| l.status.is_none() && l.contexts.is_empty() && l.updates.is_empty());
        if is_optional_child_run_tool(tool.tool_name) && no_child && external_step_body_ref(&tool).is_none() && !(tool.tool_name == "workflow_step" && !string_input(&tool, "role").is_empty()) {
            continue;
        }
        let AgentKey { mut key, mut name, address } = agent_key_for_context(&tool);
        // A run is exactly one `workflow` call. A card that missed the host's rename still says what the model wrote,
        // which is by construction the name of the run that already owns it.
        if tool.tool_name == "workflow" && by_key.get(&key).is_some_and(|&i| accumulators[i].contexts.iter().any(|c| c.tool_name == "workflow" && c.id != tool.id)) {
            key = format!("context:{}", tool.id);
            name = None;
        }
        if let Some(&i) = by_key.get(&key) {
            let existing = &mut accumulators[i];
            existing.contexts.push(tool);
            existing.name = existing.name.take().or(name);
            existing.address = existing.address.take().or(address);
        } else if !is_queued_message_context(&tool) {
            by_key.insert(key.clone(), accumulators.len());
            accumulators.push(Accumulator { key, name, address, order, contexts: vec![tool] });
        }
    }

    struct Built {
        view: SubagentView,
        descendants: Vec<Rc<SubagentView>>,
        order: usize,
    }
    let mut views: Vec<Built> = accumulators
        .iter()
        .map(|acc| {
            let _ = &acc.key;
            let items = &acc.contexts;
            let first = items[0];
            let last = items[items.len() - 1];
            // A queued message stays for call ids and timestamps, and is left out of every status decision.
            let status_items: Vec<ToolRef<'_>> = items.iter().filter(|i| !is_queued_message_context(i)).copied().collect();
            let status_last = status_items.last().copied().unwrap_or(last);
            let latest_record = items.iter().rev().find_map(|i| i.subagent);
            let explicit_live = items.iter().rev().find(|i| i.streaming && live_status_of(i).is_some()).copied();
            let live = combined_live_state(items);
            let name = acc.name.clone();
            let id = name.clone().or_else(|| acc.address.clone()).unwrap_or_else(|| last.id.to_owned());
            let kind = items
                .iter()
                .rev()
                .filter_map(|i| i.subagent.map(|s| s.kind))
                .find(|k| *k != SubagentRunKind::General)
                .or_else(|| items.iter().any(|i| matches!(i.tool_name, "workflow" | "workflow_step")).then_some(SubagentRunKind::WorkflowStep))
                .unwrap_or_default();
            let workflow_run = items.iter().any(|i| i.tool_name == "workflow");
            let label = items
                .iter()
                .rev()
                .map(|i| string_input(i, "label"))
                .find(|v| !v.is_empty())
                .or_else(|| name.clone())
                // A run has no `label`: its script body is stripped from the public input.
                .or_else(|| items.iter().rev().map(|i| string_input(i, "scriptName")).find(|v| !v.is_empty()))
                .or_else(|| (kind == SubagentRunKind::WorkflowStep).then(|| messages.workflow_step_label.clone()))
                .unwrap_or_else(|| messages.fallback_label.clone());
            let task = items
                .iter()
                .map(|i| i.subagent.map_or("", |s| js_trim(&s.task)).to_owned())
                .find(|v| !v.is_empty())
                .or_else(|| items.iter().map(|i| if is_agent_message_tool(i.tool_name) { String::new() } else { initial_message(i) }).find(|v| !v.is_empty()))
                .unwrap_or_else(|| initial_message(&first));
            let has_streaming = status_items.iter().any(|i| i.streaming);
            // A run tool's result says whether that call was accepted, not how the child ended.
            let status = if has_streaming { context_status(&explicit_live.unwrap_or(status_last)) } else { latest_record.map_or_else(|| context_status(&status_last), |r| r.status.into()) };
            let updates = merged_updates(items);
            let transcript = conversation_transcript(items, &id, &task, &updates, messages);
            let shell_tool_count = external_step_tool_count(items);
            let role = agent_role(latest_record, items);
            let descendants = nested_subagent_views(&transcript, messages, Some(&id), depth + 1);
            let step_owner = items.iter().rev().map(|i| if i.tool_name == "workflow_step" { string_input(i, "ledger") } else { String::new() }).find(|v| !v.is_empty());
            let usage = latest_record
                .map(|r| r.usage.clone())
                .filter(|u| *u != ModelUsage::default())
                .or_else(|| items.iter().rev().find_map(external_step_usage))
                .unwrap_or_else(|| live_usage(items));
            let view = SubagentView {
                ledger_owner: step_owner.or_else(|| name.clone()),
                kind,
                workflow_run,
                label,
                status,
                summary: summary_for(status, &task, &transcript, &updates, messages),
                live,
                call_ids: items.iter().map(|i| i.id.to_owned()).collect(),
                parent_id: parent_id.map(str::to_owned),
                depth,
                child_ids: descendants.iter().filter(|d| d.parent_id.as_deref() == Some(id.as_str())).map(|d| d.id.clone()).collect(),
                phase: items.iter().map(|i| string_input(i, "phase")).find(|v| !v.is_empty()),
                phase_index: items.iter().find_map(|i| number_input(i, "phaseIndex")),
                step_index: items.iter().find_map(|i| number_input(i, "stepIndex")),
                model_id: agent_model_id(latest_record, role.as_ref()),
                role,
                script_name: items.iter().rev().map(|i| string_input(i, "scriptName")).find(|v| !v.is_empty()),
                usage,
                tool_count: match shell_tool_count {
                    Some(count) => count,
                    None => Some(transcript.iter().filter(|c| c.is_tool()).count() as u64),
                },
                created_at: first.created_at.to_owned(),
                completed_at: completed_at(items, status, &transcript, &updates),
                id,
                name,
                task,
                contexts: transcript,
                updates,
            };
            Built { view, descendants, order: acc.order }
        })
        .collect();

    views.sort_by(|l, r| {
        let (lr, rr) = (l.view.status == SubagentViewStatus::Running, r.view.status == SubagentViewStatus::Running);
        if lr != rr {
            return rr.cmp(&lr);
        }
        let at = |v: &SubagentView| timestamp(v.completed_at.as_deref().or(Some(&v.created_at)));
        at(&r.view).cmp(&at(&l.view)).then(r.order.cmp(&l.order))
    });
    let mut out = Vec::new();
    for built in views {
        out.push(Rc::new(built.view));
        out.extend(built.descendants);
    }
    out
}

/// Finds the view a chip or stale selection refers to, by id or any call id.
pub fn find_subagent_view<'a>(views: &'a [Rc<SubagentView>], id: Option<&str>) -> Option<&'a Rc<SubagentView>> {
    let id = id.filter(|s| !s.is_empty())?;
    views.iter().find(|v| v.id == id).or_else(|| views.iter().find(|v| v.call_ids.iter().any(|c| c == id)))
}

/// The agent a surface may open a transcript for. A workflow run resolves to nothing: it is a script.
pub fn find_openable_subagent_view<'a>(views: &'a [Rc<SubagentView>], id: Option<&str>) -> Option<&'a Rc<SubagentView>> {
    find_subagent_view(views, id).filter(|v| !v.workflow_run)
}

/// The subagent page a pending approval card should open: by name, then by call id, since a streaming step has only
/// the latter.
pub fn approval_prompt_subagent_view<'a>(views: &'a [Rc<SubagentView>], source_agent: Option<&str>, source_call_id: Option<&str>) -> Option<&'a Rc<SubagentView>> {
    find_openable_subagent_view(views, source_agent).or_else(|| find_openable_subagent_view(views, source_call_id))
}
