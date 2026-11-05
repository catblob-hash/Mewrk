//! The conversation's tool lock (lib/toolLock.ts): the tool surface its last request went out with, and which model sent
//! it when.
//!
//! The prompt cache belongs to that model and that surface. While the model selected now is the one the last request used
//! and its cache is still warm, a change that would rewrite the cached prefix is drawn orange and asks once before it is
//! made. It is a warning, not a refusal: the change is the user's to make.
//!
//! What rewrites the prefix depends on the model. One that takes a tool mid-conversation is handed an added tool at the end
//! of the transcript, so only taking one away rewrites the declared list. On any other model the sidecar folds an added
//! tool back into that list, so the lock covers the whole surface in both directions (`ToolLockState::whole_surface`).
//! A tool turned off leaves the requests along with its calls, which stay in the transcript. Turning it back on brings
//! those calls back into the prefix, on any model, so while the transcript holds some that rewrites it too (`tool_tone`).
//!
//! Four fields are pins rather than parts of the surface: native search, native fetch, which skills the system prompt was
//! built with, and whether it was built for a tool that can start a background task. Each is `None` until the run that sets
//! it and never moves afterwards, whatever model is selected: a second answer would contradict the transcript rather than
//! extend it. Native search pins only once the transcript holds a report it produced, and only while it does: offering
//! `web_search` isn't enough, because on a family without native search every such call fails, and the fix that failure
//! asks for is choosing a catalog provider. A host-run backend is no pin: its results are ordinary tool output any backend
//! can follow, so it is part of the surface and only the cache says anything about moving it.
//!
//! Every model the conversation has sent to keeps its own cache for its own lifetime (`model_requests`), which the
//! composer's model menu marks; the surface and its tone belong to the last request alone.

use crate::model::{
    ApiProvider, ContextItem, ConversationSettings, ConversationToolLock, ConversationWebSearchSettings, FetchProviderSelection,
    ModelProfile, ProviderFamily, SearchProviderKind, SearchProviderSelection, ToolLockRequest,
};
use crate::model_capabilities::appends_tools;
use crate::settings_flags::{
    environment_prompt_enabled_of, file_write_guards_enabled_of, host_message_container_of, subagent_messaging_enabled_of,
    SUBAGENT_SPAWN_TOOL,
};
use crate::task_tools::{produces_tasks, unique_names};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;

/// The lock of a conversation that has never run.
pub fn empty_tool_lock() -> ConversationToolLock {
    ConversationToolLock::default()
}

/// The built-in prompt profile's id ("Mewrk concise"); a conversation that selects none uses it. Mirrors
/// `prompt_profile::BUILTIN_ID`.
pub const BUILTIN_PROMPT_PROFILE_ID: &str = "tooldesc_builtin_concise_en_us";

/// The id of "Mewrk guided", removed in favor of the concise one. Older settings and role files may still name it; it
/// reads as the built-in. Mirrors `prompt_profile::RETIRED_GUIDED_ID`.
pub const RETIRED_GUIDED_PROMPT_PROFILE_ID: &str = "tooldesc_builtin_en_us";

/// Whether `id` selects the built-in profile: nothing, its id, or the retired one's.
pub fn is_builtin_prompt_profile_id(id: Option<&str>) -> bool {
    match id {
        None | Some("") => true,
        Some(id) => id == BUILTIN_PROMPT_PROFILE_ID || id == RETIRED_GUIDED_PROMPT_PROFILE_ID,
    }
}

/// The prompt profile these settings word their requests with.
fn prompt_profile_of(settings: &ConversationSettings) -> String {
    match settings.tool_description_file_id.as_deref() {
        Some(id) if !is_builtin_prompt_profile_id(Some(id)) => id.to_owned(),
        _ => BUILTIN_PROMPT_PROFILE_ID.to_owned(),
    }
}

/// How long a model's prompt cache is taken to stay warm when its profile says nothing.
pub const DEFAULT_CACHE_TTL_MINUTES: u32 = 30;

/// What this run's settings would put in front of the model, beyond what the settings alone can say: `web_fetch` depends on
/// the family of the model this run uses, and `native_searched` on the transcript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExposureContext {
    /// Whether this run grants `web_fetch` at all.
    pub web_fetch: bool,
    /// Whether the transcript holds a report a native search produced.
    pub native_searched: bool,
}

/// The request a run is about to send: which model, and when.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLockRequestContext {
    pub web_fetch: bool,
    pub native_searched: bool,
    pub provider_id: String,
    pub model_id: String,
    /// RFC 3339.
    pub at: String,
}

/// The model selected now, as the lock needs to know it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLockModel {
    pub provider_id: String,
    pub model_id: String,
    pub family: ProviderFamily,
    /// Whether the model takes a tool mid-conversation.
    pub appends_tools: bool,
    /// Minutes the model's prompt cache is taken to stay warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl_minutes: Option<u32>,
}

/// A conversation's lock, a pre-lock conversation's absent one reading as empty.
///
/// A backend pin written before host-run backends joined the surface may name one of them; it reads as no pin, since nothing
/// in the transcript it stands for is sealed to that backend. A lock from before `model_requests` existed still knows one
/// model's request: its last.
pub fn tool_lock_of(settings: &ConversationSettings) -> ConversationToolLock {
    let Some(lock) = &settings.tool_lock else {
        return empty_tool_lock();
    };
    ConversationToolLock {
        search_provider: native_search_pin(lock.search_provider.as_ref()),
        fetch_provider: native_fetch_pin(lock.fetch_provider.as_ref()),
        model_requests: if lock.model_requests.is_empty() {
            lock.last_request.iter().cloned().collect()
        } else {
            lock.model_requests.clone()
        },
        ..lock.clone()
    }
}

/// A search selection as a pin: only native seals anything into the transcript.
fn native_search_pin(selection: Option<&SearchProviderSelection>) -> Option<SearchProviderSelection> {
    selection.filter(|selection| matches!(selection, SearchProviderSelection::Native)).cloned()
}

/// A fetch selection as a pin, on the same terms.
fn native_fetch_pin(selection: Option<&FetchProviderSelection>) -> Option<FetchProviderSelection> {
    selection.filter(|selection| matches!(selection, FetchProviderSelection::Native)).cloned()
}

/// The exposure these settings ask for, in the same shape as the lock.
fn tool_exposure_of(settings: &ConversationSettings, context: ToolExposureContext) -> ConversationToolLock {
    let web = settings.web_search_enabled;
    let searching = web && !matches!(settings.web_search.provider, SearchProviderSelection::Disabled);
    let fetching = web && context.web_fetch;
    ConversationToolLock {
        tools: settings.enabled_tools.clone(),
        mcp_ids: settings.mcp_ids.clone(),
        global_memory: settings.global_memory_enabled,
        project_memory: settings.project_memory_enabled,
        skill_tool: settings.skill_tool_enabled,
        mcp_tool_discovery: settings.mcp_tool_discovery_enabled,
        web_search: web,
        plan_mode: settings.plan_mode_enabled,
        skill_ids: settings.skill_ids.clone(),
        // What the system prompt would be built from if this were the first run. The merge keeps what the first run used.
        prompt_skill_ids: Some(settings.skill_ids.clone()),
        // Likewise whether it would be built for a tool that can start a background task.
        // `with_run_tool_lock` reads it off the tools the request actually sends.
        prompt_task_tools: Some(settings.enabled_tools.iter().any(|name| produces_tasks(name))),
        // Both selections are recorded whether or not their tool went out: on a model that can't take a tool
        // mid-conversation the whole surface is put back from them, and "off" is part of it.
        search_backend: web.then(|| settings.web_search.provider.clone()),
        fetch_backend: web.then(|| settings.web_search.fetch_provider.clone()),
        web_fetch: fetching,
        // A pin records a native backend a run actually used, so a selection that sends no tool pins nothing. Search goes
        // further and waits for the search itself, since only a family with native search can produce one.
        search_provider: if searching && context.native_searched { native_search_pin(Some(&settings.web_search.provider)) } else { None },
        fetch_provider: if fetching { native_fetch_pin(Some(&settings.web_search.fetch_provider)) } else { None },
        last_request: None,
        model_requests: Vec::new(),
        // Not tools, but the system prompt is built from them, so the same request caches them.
        hook_ids: Some(settings.hook_ids.clone()),
        prompt_profile: Some(prompt_profile_of(settings)),
        // Not a tool, but `box` is declared only for it, and every host message the transcript holds is projected in it.
        host_message_container: Some(host_message_container_of(settings.host_message_container)),
        // The `edit` and `write` descriptions carry the guard rules only while it is on, and those sit in the cached prefix.
        file_write_guards: Some(file_write_guards_enabled_of(settings.file_write_guards_enabled)),
        // The environment section heads the cached system prompt.
        environment_prompt: Some(environment_prompt_enabled_of(settings.environment_prompt_enabled)),
        // The switch itself, as the run saw it. It reaches the wire only with `agent_spawn` among the tools, so the tone and
        // the restore read it together with the lock's tool list rather than this bit alone.
        subagent_messaging: Some(subagent_messaging_enabled_of(settings.subagent_messaging_enabled)),
    }
}

/// The lock a request leaves behind: its own surface and model replace the last one's, each pin keeps the answer the first
/// run that set it gave, and the request becomes its model's latest.
fn next_tool_lock(
    lock: &ConversationToolLock,
    exposure: ConversationToolLock,
    request: ToolLockRequest,
    native_searched: bool,
) -> ConversationToolLock {
    ConversationToolLock {
        tools: unique_names(exposure.tools),
        mcp_ids: unique_names(exposure.mcp_ids),
        global_memory: exposure.global_memory,
        project_memory: exposure.project_memory,
        skill_tool: exposure.skill_tool,
        mcp_tool_discovery: exposure.mcp_tool_discovery,
        web_search: exposure.web_search,
        // Sticky, as the host's plan pair is: once offered it stays offered, whatever the switch says now.
        plan_mode: lock.plan_mode || exposure.plan_mode,
        skill_ids: unique_names(exposure.skill_ids),
        prompt_skill_ids: lock.prompt_skill_ids.clone().or(exposure.prompt_skill_ids),
        prompt_task_tools: lock.prompt_task_tools.or(exposure.prompt_task_tools),
        search_backend: exposure.search_backend,
        fetch_backend: exposure.fetch_backend,
        web_fetch: exposure.web_fetch,
        search_provider: search_pin(lock, exposure.search_provider, native_searched),
        fetch_provider: lock.fetch_provider.clone().or(exposure.fetch_provider),
        model_requests: with_model_request(&lock.model_requests, &request),
        last_request: Some(request),
        hook_ids: Some(unique_names(exposure.hook_ids.unwrap_or_default())),
        prompt_profile: exposure.prompt_profile,
        host_message_container: exposure.host_message_container,
        file_write_guards: exposure.file_write_guards,
        environment_prompt: exposure.environment_prompt,
        subagent_messaging: exposure.subagent_messaging,
    }
}

/// The native-search pin, kept for as long as the transcript holds what sealed it. Without that evidence there is nothing to
/// contradict, so a pin a build set merely for offering `web_search`, on a model that could never search natively, is lifted
/// at the next request rather than held forever.
fn search_pin(
    lock: &ConversationToolLock,
    candidate: Option<SearchProviderSelection>,
    native_searched: bool,
) -> Option<SearchProviderSelection> {
    if native_searched {
        lock.search_provider.clone().or(candidate)
    } else {
        None
    }
}

/// The list with `request` as its model's latest, in place of any earlier one.
fn with_model_request(requests: &[ToolLockRequest], request: &ToolLockRequest) -> Vec<ToolLockRequest> {
    requests
        .iter()
        .filter(|item| !same_model(item, &request.provider_id, &request.model_id))
        .cloned()
        .chain([request.clone()])
        .collect()
}

fn same_model(request: &ToolLockRequest, provider_id: &str, model_id: &str) -> bool {
    request.provider_id == provider_id && request.model_id == model_id
}

/// The lock with its last request moved to `at`, the moment a long run's final request actually went out, which is what the
/// cache is counted from, and its native-search pin brought up to date with the transcript the run left: a search that ran
/// during the run pins native, provided the run went out with native search and the selector still names it. Unchanged
/// when there is no last request to move.
pub fn refreshed_tool_lock(settings: &ConversationSettings, at: &str, native_searched: bool) -> ConversationToolLock {
    let lock = tool_lock_of(settings);
    let Some(last) = &lock.last_request else {
        return lock;
    };
    let last_request = ToolLockRequest { at: at.to_owned(), ..last.clone() };
    let searched_natively = lock.web_search
        && matches!(lock.search_backend, Some(SearchProviderSelection::Native))
        && matches!(settings.web_search.provider, SearchProviderSelection::Native);
    ConversationToolLock {
        search_provider: search_pin(&lock, searched_natively.then_some(SearchProviderSelection::Native), native_searched),
        model_requests: with_model_request(&lock.model_requests, &last_request),
        last_request: Some(last_request),
        ..lock
    }
}

/// Guards the per-run write so an unchanged lock doesn't rewrite the conversation.
fn same_tool_lock(left: &ConversationToolLock, right: &ConversationToolLock) -> bool {
    same_id_set(&left.tools, &right.tools)
        && same_id_set(&left.mcp_ids, &right.mcp_ids)
        && left.global_memory == right.global_memory
        && left.project_memory == right.project_memory
        && left.skill_tool == right.skill_tool
        && left.mcp_tool_discovery == right.mcp_tool_discovery
        && left.web_search == right.web_search
        && left.plan_mode == right.plan_mode
        && same_id_set(&left.skill_ids, &right.skill_ids)
        && same_id_list(left.prompt_skill_ids.as_deref(), right.prompt_skill_ids.as_deref())
        && left.prompt_task_tools == right.prompt_task_tools
        && left.search_backend == right.search_backend
        && left.fetch_backend == right.fetch_backend
        && left.web_fetch == right.web_fetch
        && left.search_provider == right.search_provider
        && left.fetch_provider == right.fetch_provider
        && left.last_request == right.last_request
        && left.model_requests.len() == right.model_requests.len()
        && left.model_requests.iter().all(|item| right.model_requests.contains(item))
        && same_id_list(left.hook_ids.as_deref(), right.hook_ids.as_deref())
        && left.prompt_profile == right.prompt_profile
        && left.host_message_container == right.host_message_container
        && left.file_write_guards == right.file_write_guards
        && left.environment_prompt == right.environment_prompt
        && left.subagent_messaging == right.subagent_messaging
}

fn same_id_set(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<HashSet<_>>() == right.iter().collect::<HashSet<_>>()
}

/// Order matters for neither pin, but "set at all" does.
fn same_id_list(left: Option<&[String]>, right: Option<&[String]>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => same_id_set(left, right),
        (None, None) => true,
        _ => false,
    }
}

/// The settings a request leaves behind: its exposure recorded as the lock. `Cow::Borrowed` when nothing moved, so a caller
/// can skip the write rather than dirty the document once per round.
pub fn with_run_tool_lock<'a>(
    settings: &'a ConversationSettings,
    tools: &[String],
    context: &ToolLockRequestContext,
) -> Cow<'a, ConversationSettings> {
    let current = tool_lock_of(settings);
    let exposure = ToolExposureContext { web_fetch: context.web_fetch, native_searched: context.native_searched };
    let next = next_tool_lock(
        &current,
        ConversationToolLock {
            tools: tools.to_vec(),
            prompt_task_tools: Some(tools.iter().any(|name| produces_tasks(name))),
            ..tool_exposure_of(settings, exposure)
        },
        ToolLockRequest { provider_id: context.provider_id.clone(), model_id: context.model_id.clone(), at: context.at.clone() },
        context.native_searched,
    );
    if settings.tool_lock.is_some() && same_tool_lock(&current, &next) {
        return Cow::Borrowed(settings);
    }
    Cow::Owned(ConversationSettings { tool_lock: Some(next), ..settings.clone() })
}

// ---------------------------------------------------------------- Lock state

/// Where a conversation's lock stands for the model selected now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolLockState {
    pub lock: ConversationToolLock,
    /// The selected model is the one the last request used.
    pub engaged: bool,
    /// ...and it can't take a tool mid-conversation, so adding one rewrites the cached prefix as surely as removing one:
    /// while the cache is warm, the lock covers the whole surface in both directions.
    pub whole_surface: bool,
    /// ...and its prompt cache is still warm: changes that would rewrite it are orange.
    pub warm: bool,
    /// When `warm` runs out, in epoch milliseconds, so a view can redraw then.
    pub warm_until: Option<i64>,
    /// The selected model's family, which decides whether native fetch is a tool of its own.
    pub family: Option<ProviderFamily>,
    /// The tools the transcript holds the model's calls of (`with_called_tools`).
    #[serde(default)]
    pub called_tools: Vec<String>,
}

impl ToolLockState {
    /// This state for a conversation whose transcript is `contexts`. A card with no model turn was written by hand or laid
    /// in by a template, and the host never hides those (`tool_append.rs`).
    pub fn with_called_tools(mut self, contexts: &[ContextItem]) -> Self {
        self.called_tools = unique_names(contexts.iter().filter_map(|context| match context {
            ContextItem::Tool { tool_name, model_turn_id: Some(_), .. } => Some(tool_name.clone()),
            _ => None,
        }));
        self
    }

    fn holds_calls_of(&self, tool: &str) -> bool {
        self.called_tools.iter().any(|name| name == tool)
    }
}

/// Where the lock stands for `model` at `now` (epoch milliseconds).
pub fn tool_lock_state(settings: &ConversationSettings, model: Option<&ToolLockModel>, now: i64) -> ToolLockState {
    let lock = tool_lock_of(settings);
    let engaged = match (model, &lock.last_request) {
        (Some(model), Some(last)) => same_model(last, &model.provider_id, &model.model_id).then_some((model, last)),
        _ => None,
    };
    let Some((model, last)) = engaged else {
        let family = model.map(|model| model.family);
        return ToolLockState { lock, engaged: false, whole_surface: false, warm: false, warm_until: None, family, called_tools: Vec::new() };
    };
    let warm_until = cache_expiry(last, model.cache_ttl_minutes);
    let (whole_surface, family) = (!model.appends_tools, Some(model.family));
    ToolLockState { lock, engaged: true, whole_surface, warm: warm_until.is_some_and(|until| now < until), warm_until, family, called_tools: Vec::new() }
}

/// When a request's cache runs out, in epoch milliseconds, or `None` for an unreadable time.
fn cache_expiry(request: &ToolLockRequest, cache_ttl_minutes: Option<u32>) -> Option<i64> {
    let sent_at = parse_timestamp(&request.at)?;
    Some(sent_at + i64::from(cache_ttl_minutes.unwrap_or(DEFAULT_CACHE_TTL_MINUTES)) * 60_000)
}

/// `Date.parse` for the RFC 3339 times a lock holds, in epoch milliseconds. Digits past the milliseconds are cut, as V8
/// cuts them. Outside RFC 3339 it differs: a date-time without an offset reads as UTC rather than local time, and an
/// impossible date is unreadable rather than rolled over.
fn parse_timestamp(text: &str) -> Option<i64> {
    let bytes = text.trim().as_bytes();
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = bytes.get(range)?;
        digits.iter().all(u8::is_ascii_digit).then(|| digits.iter().fold(0, |sum, digit| sum * 10 + i64::from(digit - b'0')))
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month as usize - 1];
    if !(1..=month_days).contains(&day) {
        return None;
    }
    let date_ms = days_from_civil(year, month, day) * 86_400_000;
    if bytes.len() == 10 {
        return Some(date_ms);
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let (hour, minute) = (number(11..13)?, number(14..16)?);
    if bytes.get(13) != Some(&b':') {
        return None;
    }
    let mut at = 16;
    let mut second = 0;
    if bytes.get(at) == Some(&b':') {
        second = number(17..19)?;
        at = 19;
    }
    let mut milli = 0;
    if bytes.get(at) == Some(&b'.') {
        let digits = bytes[at + 1..].iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        for place in 0..3 {
            milli = milli * 10 + bytes.get(at + 1 + place).filter(|_| place < digits).map_or(0, |digit| i64::from(digit - b'0'));
        }
        at += 1 + digits;
    }
    // 24:00:00 is the end of the day; every other value keeps to its range.
    let midnight = hour == 24 && minute == 0 && second == 0 && milli == 0;
    if !(midnight || (hour <= 23 && minute <= 59 && second <= 59)) {
        return None;
    }
    let offset_ms = match bytes.get(at) {
        None => 0,
        Some(b'Z' | b'z') if at + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            let colon = usize::from(bytes.get(at + 3) == Some(&b':'));
            let (hours, minutes) = (number(at + 1..at + 3)?, number(at + 3 + colon..at + 5 + colon)?);
            if at + 5 + colon != bytes.len() || hours > 23 || minutes > 59 {
                return None;
            }
            (hours * 60 + minutes) * 60_000 * if *sign == b'+' { 1 } else { -1 }
        }
        _ => return None,
    };
    Some(date_ms + (hour * 3_600 + minute * 60 + second) * 1_000 + milli - offset_ms)
}

/// Days from 1970-01-01 to a date in the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// When this conversation's cache on a model runs out, in epoch milliseconds, counted from that model's own latest request
/// here, whether or not it was the last one; `None` when that moment has passed or the model never sent. The composer's
/// model menu marks every model this answers for.
pub fn model_cache_warm_until(
    settings: &ConversationSettings,
    provider_id: &str,
    model_id: &str,
    cache_ttl_minutes: Option<u32>,
    now: i64,
) -> Option<i64> {
    let lock = tool_lock_of(settings);
    let request = lock.model_requests.iter().find(|item| same_model(item, provider_id, model_id))?;
    cache_expiry(request, cache_ttl_minutes).filter(|until| now < *until)
}

/// The kinds of setting the lock covers, by which way of changing them rewrites the cached prefix.
///
/// - `Tool`, `WebSearch`: turning one on appends tools, which every protocol with an append interface takes at the end; only
///   turning one off rewrites the declared list. A model without that interface takes the added tool into the list.
/// - `Skill`: a skill added later arrives as a host notice at the end; removing one changes the system prompt it was built
///   into. Skills aren't tools, so adding one is free on every model.
/// - `Mcp`, `Memory`, `SkillTool`, `Discovery`: either way: the selected servers are listed in the system prompt, a memory
///   tier's instructions sit ahead of the history, and the two delivery switches move skills and schemas around.
/// - `Hook`, `Profile`: either way: the system prompt lists the hooks, and the profile words every tool description.
/// - `HostMessages`: either way: every host message is projected again in the other container, and `box` comes and goes.
/// - `FileWriteGuards`: either way: the `edit` and `write` descriptions gain or lose the guard rules.
/// - `Environment`: either way: the environment section heads the system prompt.
/// - `SubagentMessaging`: either way, with `agent_spawn` among the tools: the switch adds or drops `send_message` and rewrites
///   the `agent_spawn` description. Without it the switch changes nothing, so it is drawn only while both the last request
///   and the settings hold that tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LockedSettingKind {
    Tool,
    WebSearch,
    Skill,
    Mcp,
    Memory,
    SkillTool,
    Discovery,
    Hook,
    Profile,
    HostMessages,
    FileWriteGuards,
    Environment,
    SubagentMessaging,
}

/// The one tone the lock draws: orange, a warning that the change throws a warm cache away. Nothing the lock covers is
/// refused. A native backend a run has already used is settled rather than locked (`backend_pinned`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LockTone {
    Cache,
}

fn rewrites_either_way(kind: LockedSettingKind) -> bool {
    use LockedSettingKind::*;
    matches!(
        kind,
        Mcp | Memory | SkillTool | Discovery | Hook | Profile | HostMessages | FileWriteGuards | Environment | SubagentMessaging
    )
}

/// The kinds that are no part of the tool surface, whose additions no model folds into the declared list.
fn off_surface(kind: LockedSettingKind) -> bool {
    use LockedSettingKind::*;
    matches!(kind, Skill | Hook | Profile)
}

/// How one setting is drawn: orange while the cache is warm and changing it would rewrite the prefix. On a model that can't
/// take a tool mid-conversation that is every part of the surface, on or off. A setting moved away from what the last request
/// had is plain again, the cache already lost for it, and turns orange again once it is moved back.
pub fn lock_tone(state: &ToolLockState, kind: LockedSettingKind, last_on: bool, now_on: bool) -> Option<LockTone> {
    if !state.warm || now_on != last_on {
        return None;
    }
    if last_on || rewrites_either_way(kind) {
        return Some(LockTone::Cache);
    }
    (state.whole_surface && !off_surface(kind)).then_some(LockTone::Cache)
}

/// How one tool's row is drawn: [`lock_tone`], and orange as well while it was off for the last request and still is with its
/// calls in the transcript. They were left out of that request, so turning it back on rewrites the prefix on any model.
pub fn tool_tone(state: &ToolLockState, tool: &str, last_on: bool, now_on: bool) -> Option<LockTone> {
    if state.warm && !last_on && !now_on && state.holds_calls_of(tool) {
        return Some(LockTone::Cache);
    }
    lock_tone(state, LockedSettingKind::Tool, last_on, now_on)
}

/// How the prompt-profile selector is drawn: orange while warm and it still names the profile the last request was worded
/// with. A lock from before profiles were recorded knows nothing to protect.
pub fn prompt_profile_tone(state: &ToolLockState, settings: &ConversationSettings) -> Option<LockTone> {
    let profile = state.lock.prompt_profile.as_deref()?;
    lock_tone(state, LockedSettingKind::Profile, profile == prompt_profile_of(settings), true)
}

/// How the host-message container choice is drawn: orange while warm and it still names the container the last request
/// projected its host messages in. A lock from before the choice was recorded knows nothing to protect.
pub fn host_message_container_tone(state: &ToolLockState, settings: &ConversationSettings) -> Option<LockTone> {
    let last = state.lock.host_message_container?;
    lock_tone(state, LockedSettingKind::HostMessages, last == host_message_container_of(settings.host_message_container), true)
}

/// How the file-write-guards switch is drawn: orange while warm and it still stands where the last request had it. A lock
/// from before the switch was recorded knows nothing to protect.
pub fn file_write_guards_tone(state: &ToolLockState, settings: &ConversationSettings) -> Option<LockTone> {
    let last = state.lock.file_write_guards?;
    lock_tone(state, LockedSettingKind::FileWriteGuards, last == file_write_guards_enabled_of(settings.file_write_guards_enabled), true)
}

/// How the environment switch is drawn: orange while warm and it still stands where the last request had it. A lock from
/// before the switch was recorded knows nothing to protect.
pub fn environment_prompt_tone(state: &ToolLockState, settings: &ConversationSettings) -> Option<LockTone> {
    let last = state.lock.environment_prompt?;
    lock_tone(state, LockedSettingKind::Environment, last == environment_prompt_enabled_of(settings.environment_prompt_enabled), true)
}

/// Whether the lock's request and these settings both hold `agent_spawn`, the tool the messaging switch rides on.
fn holds_subagent_spawn(lock: &ConversationToolLock, settings: &ConversationSettings) -> bool {
    lock.tools.iter().any(|name| name == SUBAGENT_SPAWN_TOOL) && settings.enabled_tools.iter().any(|name| name == SUBAGENT_SPAWN_TOOL)
}

/// How the subagent-communication switch is drawn: orange while warm, it still stands where the last request had it, and
/// `agent_spawn` is there for it to act on, in the last request and in these settings alike. Without that tool the switch
/// changes nothing on the wire, and a tool added or removed since is tinted by its own row. A lock from before the switch
/// was recorded knows nothing to protect.
pub fn subagent_messaging_tone(state: &ToolLockState, settings: &ConversationSettings) -> Option<LockTone> {
    let last = state.lock.subagent_messaging?;
    if !holds_subagent_spawn(&state.lock, settings) {
        return None;
    }
    lock_tone(state, LockedSettingKind::SubagentMessaging, last == subagent_messaging_enabled_of(settings.subagent_messaging_enabled), true)
}

/// How the composer's plan-mode switch is drawn. Its pair is sticky and its guidance is an appended system prompt, so
/// switching it off never touches the cache, and switching it on touches it only before the pair has ever gone out, on a
/// model that folds the two tools into the declared list: orange there while warm, plain everywhere else.
pub fn plan_mode_tone(state: &ToolLockState, plan_mode_enabled: bool) -> Option<LockTone> {
    (state.warm && state.whole_surface && !state.lock.plan_mode && !plan_mode_enabled).then_some(LockTone::Cache)
}

/// The two web legs, each with a backend selector of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WebBackendLeg {
    Search,
    Fetch,
}

/// A backend selection on either leg. Upstream passes the two selection unions interchangeably since they share a shape;
/// the Rust enums are distinct types, and this is what the comparisons between them see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebBackendChoice {
    Native,
    Explicit(SearchProviderKind),
    Disabled,
    Unavailable,
}

impl From<&SearchProviderSelection> for WebBackendChoice {
    fn from(selection: &SearchProviderSelection) -> Self {
        match selection {
            SearchProviderSelection::Native => Self::Native,
            SearchProviderSelection::Explicit { provider_kind } => Self::Explicit(*provider_kind),
            SearchProviderSelection::Disabled => Self::Disabled,
            SearchProviderSelection::Unavailable => Self::Unavailable,
        }
    }
}

impl From<&FetchProviderSelection> for WebBackendChoice {
    fn from(selection: &FetchProviderSelection) -> Self {
        match selection {
            FetchProviderSelection::Native => Self::Native,
            FetchProviderSelection::Explicit { provider_kind } => Self::Explicit(*provider_kind),
            FetchProviderSelection::Disabled => Self::Disabled,
            FetchProviderSelection::Unavailable => Self::Unavailable,
        }
    }
}

/// How a web backend selector is drawn: orange while warm and the selector still names the backend the last request had. A
/// leg whose tool didn't go out is plain on a model that appends tools, because giving it one is an addition; on any other
/// model that rewrites the declared list, so the selector is orange there too. A pinned native backend isn't drawn this way:
/// it is settled for every model, and its selector says so instead.
pub fn backend_tone(state: &ToolLockState, leg: WebBackendLeg, current: impl Into<WebBackendChoice>) -> Option<LockTone> {
    if !state.warm || backend_pinned(&state.lock, leg) {
        return None;
    }
    let last: Option<WebBackendChoice> = match leg {
        WebBackendLeg::Search => state.lock.search_backend.as_ref().map(Into::into),
        WebBackendLeg::Fetch => state.lock.fetch_backend.as_ref().map(Into::into),
    };
    if last != Some(current.into()) {
        return None;
    }
    (leg_sent(&state.lock, leg) || state.whole_surface).then_some(LockTone::Cache)
}

/// How a web leg's switch is drawn: like a tool's own row, orange while warm and the leg still stands where the last request
/// had it: on when its tool went out, or off on a model that folds an added tool into the declared list, or off with its
/// tool's calls in the transcript (`tool_tone`).
pub fn web_leg_tone(state: &ToolLockState, leg: WebBackendLeg, on: bool) -> Option<LockTone> {
    let last_on = leg_sent(&state.lock, leg);
    let tool = match leg {
        WebBackendLeg::Search => "web_search",
        WebBackendLeg::Fetch => "web_fetch",
    };
    if state.warm && !last_on && !on && state.holds_calls_of(tool) {
        return Some(LockTone::Cache);
    }
    lock_tone(state, LockedSettingKind::WebSearch, last_on, on)
}

/// Whether the last request offered this leg's tool.
fn leg_sent(lock: &ConversationToolLock, leg: WebBackendLeg) -> bool {
    match leg {
        WebBackendLeg::Search => {
            lock.web_search && !matches!(lock.search_backend, None | Some(SearchProviderSelection::Disabled))
        }
        WebBackendLeg::Fetch => lock.web_fetch,
    }
}

/// lib/webSearch.ts `familySupportsNativeFetch`: whether this conversation can fetch a page without borrowing a provider. The
/// Messages families have a fetch tool and Claude Agent borrows the CLI's own `WebFetch`; the Responses API, Google and xAI
/// have none.
pub fn family_supports_native_fetch(family: Option<ProviderFamily>) -> bool {
    matches!(family, Some(ProviderFamily::Anthropic | ProviderFamily::Bedrock | ProviderFamily::ClaudeAgent))
}

/// Whether a backend choice offers its leg's tool, by the host's one rule for both (`web_search::apply_web_tools`): anything
/// but Off does, except native fetch on a family that reads pages inside its search tool. Whether the provider is configured
/// never enters into it.
fn choice_offers_tool(leg: WebBackendLeg, choice: WebBackendChoice, family: Option<ProviderFamily>) -> bool {
    match choice {
        WebBackendChoice::Disabled => false,
        WebBackendChoice::Native if leg == WebBackendLeg::Fetch => family_supports_native_fetch(family),
        _ => true,
    }
}

/// Whether moving a leg's selector from what the last request had to `choice` rewrites the cached prefix. A leg whose tool
/// went out is rewritten by any move, since the tool's own wording follows its backend. A leg whose tool didn't is rewritten
/// only by a choice that offers the tool, and only on a model that folds an added tool into the declared list.
fn backend_move_rewrites(state: &ToolLockState, leg: WebBackendLeg, choice: WebBackendChoice) -> bool {
    leg_sent(&state.lock, leg) || (state.whole_surface && choice_offers_tool(leg, choice, state.family))
}

/// Whether a leg's native backend is pinned, which no model and no cache lifts.
pub fn backend_pinned(lock: &ConversationToolLock, leg: WebBackendLeg) -> bool {
    match leg {
        WebBackendLeg::Search => lock.search_provider.is_some(),
        WebBackendLeg::Fetch => lock.fetch_provider.is_some(),
    }
}

/// The web settings with each leg named by `restore_*` put back to what the last request had.
fn restored_backends(
    web: &ConversationWebSearchSettings,
    lock: &ConversationToolLock,
    restore_search: bool,
    restore_fetch: bool,
) -> ConversationWebSearchSettings {
    let mut restored = web.clone();
    if let (true, Some(backend)) = (restore_search, &lock.search_backend) {
        restored.provider = backend.clone();
    }
    if let (true, Some(backend)) = (restore_fetch, &lock.fetch_backend) {
        restored.fetch_provider = backend.clone();
    }
    restored
}

/// Puts the settings the lock holds back to what the last request had, for the moment the model that sent it is selected
/// again.
///
/// Only what the lock actually holds moves: what `lock_tone` and `backend_tone` would draw orange. On a model that appends
/// tools, a tool the user added since stays, because adding it cost nothing, unless the transcript holds its calls (`tool_tone`);
/// on any other model it goes too, since there adding it rewrote the declared list. A cold cache holds nothing. `Cow::Borrowed`
/// when nothing moves.
pub fn restore_locked_settings<'a>(settings: &'a ConversationSettings, state: &ToolLockState) -> Cow<'a, ConversationSettings> {
    if !state.engaged || !state.warm {
        return Cow::Borrowed(settings);
    }
    let lock = &state.lock;
    let whole = state.whole_surface;
    let union = |now: &[String], last: &[String]| unique_names(now.iter().chain(last).cloned());
    let mut next = settings.clone();
    // The container goes back whenever the lock holds one: it is part of the surface (`box` comes and goes with it) and of
    // the cached prefix alike.
    if let Some(container) = lock.host_message_container {
        next.host_message_container = container;
    }
    // The guards go back the same way: their rules sit in the cached `edit` and `write` descriptions.
    if let Some(guards) = lock.file_write_guards {
        next.file_write_guards_enabled = guards;
    }
    // The environment switch goes back the same way: the section heads the cached system prompt.
    if let Some(environment) = lock.environment_prompt {
        next.environment_prompt_enabled = environment;
    }
    // The messaging switch goes back only where it acted: the last request held `agent_spawn`, which is put back below too.
    if let (Some(messaging), true) = (lock.subagent_messaging, lock.tools.iter().any(|name| name == SUBAGENT_SPAWN_TOOL)) {
        next.subagent_messaging_enabled = messaging;
    }
    next.enabled_tools = if whole {
        lock.tools.clone()
    } else {
        let kept = settings.enabled_tools.iter().filter(|name| lock.tools.contains(*name) || !state.holds_calls_of(name)).cloned().collect::<Vec<_>>();
        union(&kept, &lock.tools)
    };
    next.mcp_ids = lock.mcp_ids.clone();
    next.global_memory_enabled = lock.global_memory;
    next.project_memory_enabled = lock.project_memory;
    next.skill_tool_enabled = lock.skill_tool;
    next.mcp_tool_discovery_enabled = lock.mcp_tool_discovery;
    next.web_search_enabled = if whole { lock.web_search } else { settings.web_search_enabled || lock.web_search };
    next.skill_ids = union(&settings.skill_ids, &lock.skill_ids);
    // On a model that appends tools only a leg whose tool went out is held; on any other model every leg the last request
    // recorded is. A pinned leg stays on its pin, which no restore moves.
    next.web_search = restored_backends(
        &settings.web_search,
        lock,
        !backend_pinned(lock, WebBackendLeg::Search) && (whole || leg_sent(lock, WebBackendLeg::Search)),
        !backend_pinned(lock, WebBackendLeg::Fetch) && (whole || leg_sent(lock, WebBackendLeg::Fetch)),
    );
    // What the system prompt was built from besides the tools, as far as the lock recorded it.
    if let Some(hooks) = &lock.hook_ids {
        next.hook_ids = hooks.clone();
    }
    if let Some(profile) = lock.prompt_profile.as_deref().filter(|profile| *profile != prompt_profile_of(settings)) {
        next.tool_description_file_id = (!is_builtin_prompt_profile_id(Some(profile))).then(|| profile.to_owned());
    }
    if same_surface(settings, &next) {
        Cow::Borrowed(settings)
    } else {
        Cow::Owned(next)
    }
}

fn same_surface(left: &ConversationSettings, right: &ConversationSettings) -> bool {
    same_id_set(&left.enabled_tools, &right.enabled_tools)
        && same_id_set(&left.mcp_ids, &right.mcp_ids)
        && same_id_set(&left.skill_ids, &right.skill_ids)
        && left.global_memory_enabled == right.global_memory_enabled
        && left.project_memory_enabled == right.project_memory_enabled
        && left.skill_tool_enabled == right.skill_tool_enabled
        && left.mcp_tool_discovery_enabled == right.mcp_tool_discovery_enabled
        && left.web_search_enabled == right.web_search_enabled
        && left.web_search == right.web_search
        && same_id_set(&left.hook_ids, &right.hook_ids)
        && prompt_profile_of(left) == prompt_profile_of(right)
        && left.host_message_container == right.host_message_container
        && left.file_write_guards_enabled == right.file_write_guards_enabled
        && left.environment_prompt_enabled == right.environment_prompt_enabled
        && left.subagent_messaging_enabled == right.subagent_messaging_enabled
}

/// The selection lists a dangling row can belong to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DanglingSelectionKind {
    Skills,
    Mcp,
    Hooks,
    Agents,
}

/// The settings with a dangling selection, an id the catalog no longer has, unticked from the conversation and its lock alike.
///
/// The host fails every run while such an id is selected, so clearing it must always be possible: it is never toned and never
/// asks, since the entry it named has nothing left to declare. It also leaves the lock, or the next restore of a warm surface
/// would put back the very id that stops every run. A role takes part in no lock, its settings being no part of the calling
/// conversation's prompt cache, so for one only the selection changes.
pub fn without_dangling_selection(settings: &ConversationSettings, kind: DanglingSelectionKind, id: &str) -> ConversationSettings {
    let drop = |ids: &mut Vec<String>| ids.retain(|existing| existing != id);
    let mut next = settings.clone();
    match kind {
        DanglingSelectionKind::Agents => drop(&mut next.agent_ids),
        DanglingSelectionKind::Mcp => {
            drop(&mut next.mcp_ids);
            if let Some(lock) = &mut next.tool_lock {
                drop(&mut lock.mcp_ids);
            }
        }
        DanglingSelectionKind::Skills => {
            drop(&mut next.skill_ids);
            if let Some(lock) = &mut next.tool_lock {
                drop(&mut lock.skill_ids);
            }
        }
        DanglingSelectionKind::Hooks => {
            drop(&mut next.hook_ids);
            if let Some(hooks) = next.tool_lock.as_mut().and_then(|lock| lock.hook_ids.as_mut()) {
                drop(hooks);
            }
        }
    }
    next
}

/// The lock's view of the model selected now, or `None` when none is.
pub fn tool_lock_model_of(provider: Option<&ApiProvider>, model: Option<&ModelProfile>) -> Option<ToolLockModel> {
    let (provider, model) = (provider?, model?);
    Some(ToolLockModel {
        provider_id: provider.id.clone(),
        model_id: model.id.clone(),
        family: provider.family,
        appends_tools: appends_tools(provider, model),
        cache_ttl_minutes: model.cache_ttl_minutes,
    })
}

/// Whether going from `before` to `after` moves any setting `lock_tone` draws orange in `before`, so that the move throws the
/// warm cache away: the one question the settings pane asks before writing a change, so every page shares one warning.
pub fn lock_touch(state: &ToolLockState, before: &ConversationSettings, after: &ConversationSettings) -> bool {
    use LockedSettingKind as Kind;
    let lock = &state.lock;
    let mut touched = false;
    let mut note = |tone: Option<LockTone>| touched |= tone == Some(LockTone::Cache);

    let mut lists: Vec<(Kind, &[String], &[String], &[String])> = vec![
        (Kind::Tool, &lock.tools, &before.enabled_tools, &after.enabled_tools),
        (Kind::Mcp, &lock.mcp_ids, &before.mcp_ids, &after.mcp_ids),
        (Kind::Skill, &lock.skill_ids, &before.skill_ids, &after.skill_ids),
    ];
    // An older lock never recorded hooks, so it has nothing to say about them.
    if let Some(hooks) = &lock.hook_ids {
        lists.push((Kind::Hook, hooks, &before.hook_ids, &after.hook_ids));
    }
    for (kind, last, from, to) in lists {
        let last_set: HashSet<&String> = last.iter().collect();
        let from_set: HashSet<&String> = from.iter().collect();
        let to_set: HashSet<&String> = to.iter().collect();
        // Only a membership change moves anything, so the ids in exactly one of the two sets are all that's read.
        for id in from_set.symmetric_difference(&to_set) {
            let (last_on, now_on) = (last_set.contains(*id), from_set.contains(*id));
            note(if kind == Kind::Tool { tool_tone(state, id, last_on, now_on) } else { lock_tone(state, kind, last_on, now_on) });
        }
    }

    let switches = [
        (Kind::WebSearch, lock.web_search, before.web_search_enabled, after.web_search_enabled),
        (Kind::Memory, lock.global_memory, before.global_memory_enabled, after.global_memory_enabled),
        (Kind::Memory, lock.project_memory, before.project_memory_enabled, after.project_memory_enabled),
        (Kind::SkillTool, lock.skill_tool, before.skill_tool_enabled, after.skill_tool_enabled),
        (Kind::Discovery, lock.mcp_tool_discovery, before.mcp_tool_discovery_enabled, after.mcp_tool_discovery_enabled),
    ];
    for (kind, last_on, from, to) in switches {
        if from != to {
            note(lock_tone(state, kind, last_on, from));
        }
    }

    if prompt_profile_of(before) != prompt_profile_of(after) {
        note(prompt_profile_tone(state, before));
    }
    if before.host_message_container != after.host_message_container {
        note(host_message_container_tone(state, before));
    }
    if before.file_write_guards_enabled != after.file_write_guards_enabled {
        note(file_write_guards_tone(state, before));
    }
    if before.environment_prompt_enabled != after.environment_prompt_enabled {
        note(environment_prompt_tone(state, before));
    }
    if before.subagent_messaging_enabled != after.subagent_messaging_enabled {
        note(subagent_messaging_tone(state, before));
    }

    let legs: [(WebBackendLeg, WebBackendChoice, WebBackendChoice); 2] = [
        (WebBackendLeg::Search, (&before.web_search.provider).into(), (&after.web_search.provider).into()),
        (WebBackendLeg::Fetch, (&before.web_search.fetch_provider).into(), (&after.web_search.fetch_provider).into()),
    ];
    for (leg, from, to) in legs {
        if from != to && backend_move_rewrites(state, leg, to) {
            note(backend_tone(state, leg, from));
        }
        // A leg coming on where the transcript holds its tool's calls.
        let offered = |web_access: bool, choice: WebBackendChoice| web_access && choice_offers_tool(leg, choice, state.family);
        if !offered(before.web_search_enabled, from) && offered(after.web_search_enabled, to) {
            note(web_leg_tone(state, leg, false));
        }
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostMessageContainer, ModelCapability};
    use serde_json::json;
    use LockedSettingKind as Kind;
    use WebBackendLeg::{Fetch, Search};

    const SENT_AT: &str = "2026-09-30T10:00:00.000Z";
    const SENT: i64 = 1_790_762_400_000;
    const MINUTE: i64 = 60_000;
    const CACHE: Option<LockTone> = Some(LockTone::Cache);

    /// Options that upstream's tests spread, as a closure over a copy.
    fn with<T: Clone>(base: &T, patch: impl FnOnce(&mut T)) -> T {
        let mut copy = base.clone();
        patch(&mut copy);
        copy
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Settings from JSON, so what a test doesn't state reads as serde's defaults do.
    fn settings(patch: impl FnOnce(&mut ConversationSettings)) -> ConversationSettings {
        let mut settings: ConversationSettings = serde_json::from_value(json!({ "enabledTools": [] })).unwrap();
        patch(&mut settings);
        settings
    }

    fn plain() -> ConversationSettings {
        settings(|_| {})
    }

    fn tool_card(tool: &str, turn: Option<&str>) -> ContextItem {
        serde_json::from_value(json!({
            "kind": "tool",
            "id": format!("card_{tool}"),
            "toolName": tool,
            "modelTurnId": turn,
            "input": {},
            "result": { "success": true, "output": "", "executedAt": "", "durationMs": 0 },
            "createdAt": "",
        }))
        .unwrap()
    }

    fn model(provider_id: &str, model_id: &str, family: ProviderFamily, appends_tools: bool) -> ToolLockModel {
        ToolLockModel { provider_id: provider_id.into(), model_id: model_id.into(), family, appends_tools, cache_ttl_minutes: None }
    }

    /// A model that appends tools.
    fn opus() -> ToolLockModel {
        model("anthropic", "claude-opus-5-5", ProviderFamily::Anthropic, true)
    }

    /// A model whose protocol can't.
    fn haiku() -> ToolLockModel {
        model("anthropic", "claude-haiku-4-5", ProviderFamily::Anthropic, false)
    }

    fn gpt() -> ToolLockModel {
        model("openai", "gpt-5.5", ProviderFamily::OpenaiResponses, true)
    }

    fn agent() -> ToolLockModel {
        model("agent", "claude-opus-5-5", ProviderFamily::ClaudeAgent, true)
    }

    /// A lock as a request by `model` would have left it, so a test only states what it is about.
    fn lock(model: &ToolLockModel, patch: impl FnOnce(&mut ConversationToolLock)) -> ConversationToolLock {
        let mut lock = ConversationToolLock {
            prompt_skill_ids: Some(Vec::new()),
            last_request: Some(ToolLockRequest { provider_id: model.provider_id.clone(), model_id: model.model_id.clone(), at: SENT_AT.into() }),
            ..empty_tool_lock()
        };
        patch(&mut lock);
        lock
    }

    const OFFLINE: ToolExposureContext = ToolExposureContext { web_fetch: false, native_searched: false };
    /// A run that offered `web_fetch` after a native search had already run.
    const FETCHED_AFTER_SEARCH: ToolExposureContext = ToolExposureContext { web_fetch: true, native_searched: true };
    /// The transcript already holds a report a native search produced.
    const SEARCHED: ToolExposureContext = ToolExposureContext { web_fetch: false, native_searched: true };

    fn request_with(model: &ToolLockModel, context: ToolExposureContext) -> ToolLockRequestContext {
        ToolLockRequestContext {
            web_fetch: context.web_fetch,
            native_searched: context.native_searched,
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            at: SENT_AT.into(),
        }
    }

    fn request(model: &ToolLockModel) -> ToolLockRequestContext {
        request_with(model, OFFLINE)
    }

    fn run(settings: &ConversationSettings, tools: &[&str], request: &ToolLockRequestContext) -> ConversationSettings {
        with_run_tool_lock(settings, &names(tools), request).into_owned()
    }

    fn state(settings: &ConversationSettings, model: &ToolLockModel, now: i64) -> ToolLockState {
        tool_lock_state(settings, Some(model), now)
    }

    /// Upstream's `expect(withRunToolLock(...)).toBe(settings)`: nothing moved, so the caller skips the write.
    fn run_moves_nothing(settings: &ConversationSettings, tools: &[&str], request: &ToolLockRequestContext) -> bool {
        matches!(with_run_tool_lock(settings, &names(tools), request), Cow::Borrowed(_))
    }

    /// Upstream's `expect(restoreLockedSettings(...)).toBe(settings)`.
    fn restore_moves_nothing(settings: &ConversationSettings, state: &ToolLockState) -> bool {
        matches!(restore_locked_settings(settings, state), Cow::Borrowed(_))
    }

    fn explicit_search(provider_kind: SearchProviderKind) -> SearchProviderSelection {
        SearchProviderSelection::Explicit { provider_kind }
    }

    fn explicit_fetch(provider_kind: SearchProviderKind) -> FetchProviderSelection {
        FetchProviderSelection::Explicit { provider_kind }
    }

    fn native_web(provider: SearchProviderSelection, fetch_provider: FetchProviderSelection) -> ConversationWebSearchSettings {
        ConversationWebSearchSettings { provider, fetch_provider, ..Default::default() }
    }

    /// `Date.parse` agrees with the constant upstream computes at load, and the other forms RFC 3339 allows.
    #[test]
    fn reads_the_times_a_lock_holds_as_date_parse_does() {
        assert_eq!(parse_timestamp(SENT_AT), Some(SENT));
        assert_eq!(parse_timestamp("2026-09-30T10:00:00Z"), Some(SENT));
        assert_eq!(parse_timestamp("2026-09-30T18:00:00+08:00"), Some(SENT));
        assert_eq!(parse_timestamp("2026-09-30T05:30:00.000-04:30"), Some(SENT));
        assert_eq!(parse_timestamp("2026-09-30T10:00:00.0009999Z"), Some(SENT));
        assert_eq!(parse_timestamp("2026-09-30T10:00:00.5Z"), Some(SENT + 500));
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("1969-12-31T23:59:59.999Z"), Some(-1));
        assert_eq!(parse_timestamp("2024-02-29"), Some(1_709_164_800_000));
        for unreadable in ["", "not a date", "2026-13-01T00:00:00Z", "2026-02-30T00:00:00Z", "2026-09-30T25:00:00Z", "2026-09-30T10:00:00Zjunk"] {
            assert_eq!(parse_timestamp(unreadable), None, "{unreadable}");
        }
        // An unreadable time keeps no cache warm.
        let current = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.last_request.as_mut().unwrap().at = "soon".into())));
        let unreadable = state(&current, &opus(), SENT);
        assert!(unreadable.engaged && !unreadable.warm);
        assert_eq!(unreadable.warm_until, None);
    }

    mod the_lock_a_request_leaves {
        use super::*;

        #[test]
        fn is_empty_and_unengaged_for_a_conversation_that_has_never_run() {
            let current = settings(|s| {
                s.enabled_tools = names(&["read"]);
                s.global_memory_enabled = true;
            });
            let lock = tool_lock_of(&current);
            assert!(lock.tools.is_empty());
            assert_eq!(lock.last_request, None);
            // Not the same as "opened its prompt with no skills": until a run says so, every selected skill is still part of
            // the prompt about to be built.
            assert_eq!(lock.prompt_skill_ids, None);
            // Likewise the prompt's task-tool wording: no run has settled it yet.
            assert_eq!(lock.prompt_task_tools, None);
            assert!(!state(&current, &opus(), SENT).engaged);
        }

        #[test]
        fn records_each_requests_own_surface_and_model_replacing_the_last_ones() {
            let first = run(
                &settings(|s| {
                    s.enabled_tools = names(&["read", "write"]);
                    s.global_memory_enabled = true;
                }),
                &["read", "write"],
                &request(&opus()),
            );
            assert_eq!(tool_lock_of(&first).tools, names(&["read", "write"]));
            assert_eq!(
                tool_lock_of(&first).last_request,
                Some(ToolLockRequest { provider_id: "anthropic".into(), model_id: "claude-opus-5-5".into(), at: SENT_AT.into() })
            );

            // The next request went out without `write` and without global memory, on another model: that is what its cache
            // holds now.
            let second = run(&with(&first, |s| s.global_memory_enabled = false), &["read"], &request(&gpt()));
            assert_eq!(tool_lock_of(&second).tools, names(&["read"]));
            assert!(!tool_lock_of(&second).global_memory);
            assert_eq!(tool_lock_of(&second).last_request.unwrap().model_id, "gpt-5.5");

            // Nothing moved, so the caller skips the write.
            assert!(run_moves_nothing(&second, &["read"], &request(&gpt())));
        }

        #[test]
        fn keeps_the_plan_pair_once_a_request_offered_it_as_the_host_does() {
            let planning = run(&settings(|s| s.plan_mode_enabled = true), &[], &request(&opus()));
            assert!(tool_lock_of(&planning).plan_mode);
            // The plan was approved and the switch went off; the pair did not leave.
            let approved = run(&with(&planning, |s| s.plan_mode_enabled = false), &[], &request(&opus()));
            assert!(tool_lock_of(&approved).plan_mode);
        }

        #[test]
        fn records_a_host_run_backend_as_the_surface_and_pins_only_a_native_one_that_went_out() {
            let host_run = run(
                &settings(|s| {
                    s.web_search_enabled = true;
                    s.web_search = native_web(explicit_search(SearchProviderKind::Tavily), FetchProviderSelection::Disabled);
                }),
                &[],
                &request(&opus()),
            );
            // Its results are ordinary tool output any backend can follow: nothing to pin.
            let lock_of = tool_lock_of(&host_run);
            assert_eq!(lock_of.search_provider, None);
            assert_eq!(lock_of.search_backend, Some(explicit_search(SearchProviderKind::Tavily)));
            assert_eq!(lock_of.fetch_backend, Some(FetchProviderSelection::Disabled));
            assert!(!lock_of.web_fetch);
            // No `web_fetch` went out, so nothing about fetching is settled yet.
            assert_eq!(lock_of.fetch_provider, None);

            let native = run(
                &settings(|s| {
                    s.web_search_enabled = true;
                    s.web_search = native_web(SearchProviderSelection::Native, FetchProviderSelection::Native);
                }),
                &[],
                &request_with(&opus(), FETCHED_AFTER_SEARCH),
            );
            assert_eq!(tool_lock_of(&native).search_provider, Some(SearchProviderSelection::Native));
            assert_eq!(tool_lock_of(&native).fetch_provider, Some(FetchProviderSelection::Native));
            assert!(tool_lock_of(&native).web_fetch);

            // Native fetch on a family that folds retrieval into search grants no `web_fetch`, so it seals nothing.
            let folded = run(
                &settings(|s| {
                    s.web_search_enabled = true;
                    s.web_search = native_web(SearchProviderSelection::Native, FetchProviderSelection::Native);
                }),
                &[],
                &request(&gpt()),
            );
            assert_eq!(tool_lock_of(&folded).fetch_provider, None);

            // Switching backends afterwards leaves a native pin where it was, while the surface follows the request.
            let moved = run(
                &with(&native, |s| {
                    s.web_search.provider = explicit_search(SearchProviderKind::Exa);
                    s.web_search.fetch_provider = explicit_fetch(SearchProviderKind::Jina);
                }),
                &[],
                &request_with(&opus(), FETCHED_AFTER_SEARCH),
            );
            assert_eq!(tool_lock_of(&moved).search_provider, Some(SearchProviderSelection::Native));
            assert_eq!(tool_lock_of(&moved).fetch_provider, Some(FetchProviderSelection::Native));
            assert_eq!(tool_lock_of(&moved).search_backend, Some(explicit_search(SearchProviderKind::Exa)));

            // A host-run backend used first doesn't stop native from pinning later.
            let then_native = run(&with(&host_run, |s| s.web_search.provider = SearchProviderSelection::Native), &[], &request_with(&opus(), SEARCHED));
            assert_eq!(tool_lock_of(&then_native).search_provider, Some(SearchProviderSelection::Native));
        }

        /// Offering `web_search` with Native isn't searching with it: on a family with no native search every call fails, and
        /// the fix that failure asks for is choosing a provider, which a pin would make impossible.
        #[test]
        fn pins_native_search_only_once_a_native_search_has_run() {
            let native_search = settings(|s| s.web_search_enabled = true);
            let offered = run(&native_search, &[], &request(&agent()));
            assert_eq!(tool_lock_of(&offered).search_provider, None);
            let later_state = state(&offered, &agent(), SENT + 90 * MINUTE);
            assert_eq!(backend_tone(&later_state, Search, &SearchProviderSelection::Native), None);
            // A provider can be chosen at any time, then.
            let repaired = with(&offered, |s| s.web_search.provider = explicit_search(SearchProviderKind::ExaMcp));
            assert!(!lock_touch(&later_state, &offered, &repaired));

            // On a model that searches natively, the run's own search pins native at the run's end, when the report is in
            // the transcript.
            let sent = run(&native_search, &[], &request(&opus()));
            assert_eq!(tool_lock_of(&sent).search_provider, None);
            let later = "2026-09-30T10:05:00.000Z";
            assert_eq!(refreshed_tool_lock(&sent, later, false).search_provider, None);
            let searched = refreshed_tool_lock(&sent, later, true);
            assert_eq!(searched.search_provider, Some(SearchProviderSelection::Native));
            assert!(backend_pinned(&searched, Search));
            // A run that went out with a host-run backend seals nothing natively.
            let host_run = run(&with(&native_search, |s| s.web_search.provider = explicit_search(SearchProviderKind::Tavily)), &[], &request(&opus()));
            assert_eq!(refreshed_tool_lock(&host_run, later, true).search_provider, None);
        }

        /// A pin an older build set merely for offering native search, with no report in the transcript to contradict, is
        /// lifted at the next request.
        #[test]
        fn lifts_a_native_search_pin_the_transcript_gives_no_reason_for() {
            let stale = settings(|s| {
                s.web_search_enabled = true;
                s.tool_lock = Some(lock(&agent(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Native);
                    l.search_provider = Some(SearchProviderSelection::Native);
                }));
            });
            assert_eq!(tool_lock_of(&run(&stale, &[], &request(&agent()))).search_provider, None);
            assert_eq!(tool_lock_of(&run(&stale, &[], &request_with(&agent(), SEARCHED))).search_provider, Some(SearchProviderSelection::Native));
        }

        #[test]
        fn reads_a_host_run_pin_written_by_an_older_build_as_no_pin() {
            let older = settings(|s| {
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.search_provider = Some(explicit_search(SearchProviderKind::Tavily));
                    l.fetch_provider = Some(explicit_fetch(SearchProviderKind::Jina));
                }));
            });
            assert_eq!(tool_lock_of(&older).search_provider, None);
            assert_eq!(tool_lock_of(&older).fetch_provider, None);
        }

        #[test]
        fn records_the_opening_prompts_skills_on_the_first_run_even_when_there_were_none() {
            let first = run(&plain(), &["read"], &request(&opus()));
            assert_eq!(tool_lock_of(&first).prompt_skill_ids, Some(Vec::new()));
            let added = run(&with(&first, |s| s.skill_ids = names(&["skill-a"])), &["read"], &request(&opus()));
            assert_eq!(tool_lock_of(&added).prompt_skill_ids, Some(Vec::new()));
            assert_eq!(tool_lock_of(&added).skill_ids, names(&["skill-a"]));
        }

        #[test]
        fn records_on_the_first_run_whether_the_opening_prompt_had_a_task_producing_tool_and_keeps_that_answer() {
            assert_eq!(empty_tool_lock().prompt_task_tools, None);
            // Read off the tools the request sent, not off the selection.
            let producing = run(&plain(), &["read", "bash"], &request(&opus()));
            assert_eq!(tool_lock_of(&producing).prompt_task_tools, Some(true));
            let plain_run = run(&plain(), &["read", "edit"], &request(&opus()));
            assert_eq!(tool_lock_of(&plain_run).prompt_task_tools, Some(false));
            for tool in ["agent_spawn", "workflow", "bash", "zsh", "sh", "pwsh", "powershell", "preview_start"] {
                assert_eq!(tool_lock_of(&run(&plain(), &[tool], &request(&opus()))).prompt_task_tools, Some(true), "{tool}");
            }

            // Dropped later, the opening prompt was still worded for it...
            let dropped = run(&with(&producing, |s| s.enabled_tools = names(&["read"])), &["read"], &request(&opus()));
            assert_eq!(tool_lock_of(&dropped).prompt_task_tools, Some(true));
            assert_eq!(tool_lock_of(&dropped).tools, names(&["read"]));
            // ...and added later, the prompt never was.
            let added = run(&with(&plain_run, |s| s.enabled_tools = names(&["read", "edit", "bash"])), &["read", "edit", "bash"], &request(&opus()));
            assert_eq!(tool_lock_of(&added).prompt_task_tools, Some(false));
            assert_eq!(tool_lock_of(&added).tools, names(&["read", "edit", "bash"]));
            // Nothing moved, so the caller skips the write.
            assert!(run_moves_nothing(&added, &["read", "edit", "bash"], &request(&opus())));
        }

        // Upstream's second half reads a pin of the wrong type (`"yes"`), which `Option<bool>` can't hold.
        #[test]
        fn reads_a_task_tool_pin_an_older_build_never_set_as_unset_until_the_next_run_sets_it() {
            let older = settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})));
            assert_eq!(tool_lock_of(&older).prompt_task_tools, None);
            assert_eq!(tool_lock_of(&run(&older, &["bash"], &request(&opus()))).prompt_task_tools, Some(true));
        }
    }

    mod where_the_lock_stands_for_the_selected_model {
        use super::*;

        #[test]
        fn engages_only_for_the_model_the_last_request_used() {
            let current = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.tools = names(&["read"]))));
            assert!(state(&current, &opus(), SENT).engaged);
            assert!(!state(&current, &gpt(), SENT).engaged);
            assert!(!state(&current, &with(&opus(), |m| m.provider_id = "relay".into()), SENT).engaged);
            assert!(!tool_lock_state(&current, None, SENT).engaged);
        }

        #[test]
        fn keeps_the_cache_warm_for_the_models_lifetime_thirty_minutes_by_default() {
            let current = settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})));
            assert_eq!(DEFAULT_CACHE_TTL_MINUTES, 30);
            assert!(state(&current, &opus(), SENT + 29 * MINUTE).warm);
            assert!(!state(&current, &opus(), SENT + 30 * MINUTE).warm);
            assert_eq!(state(&current, &opus(), SENT).warm_until, Some(SENT + 30 * MINUTE));

            let hourly = with(&opus(), |m| m.cache_ttl_minutes = Some(60));
            assert!(state(&current, &hourly, SENT + 45 * MINUTE).warm);
            assert!(!state(&current, &with(&opus(), |m| m.cache_ttl_minutes = Some(5)), SENT + 6 * MINUTE).warm);
        }

        #[test]
        fn covers_the_whole_surface_of_a_model_that_cannot_append_tools_but_only_while_its_cache_is_warm() {
            let current = settings(|s| s.tool_lock = Some(lock(&haiku(), |_| {})));
            let warm = state(&current, &haiku(), SENT);
            assert!(warm.whole_surface);
            assert!(warm.warm);
            // Cold, the cache has nothing left to lose: the model alone holds nothing.
            let cold = state(&current, &haiku(), SENT + 120 * MINUTE);
            assert!(!cold.warm);
            assert_eq!(lock_tone(&cold, Kind::Tool, false, false), None);
            assert_eq!(lock_tone(&cold, Kind::Tool, true, true), None);
            assert!(!state(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {}))), &opus(), SENT).whole_surface);
            // Another model's request isn't this one's to cover.
            assert!(!state(&current, &opus(), SENT).whole_surface);
        }

        #[test]
        fn builds_the_models_view_from_the_provider_and_profile_or_none() {
            let provider = |family: ProviderFamily| -> ApiProvider {
                serde_json::from_value(json!({ "id": "p", "name": "P", "family": family, "baseUrl": "" })).unwrap()
            };
            let profile = || -> ModelProfile { serde_json::from_value(json!({ "id": "m" })).unwrap() };
            let appending = ModelProfile { capabilities: [ModelCapability::ToolAppend].into(), ..profile() };
            assert_eq!(
                tool_lock_model_of(Some(&provider(ProviderFamily::Anthropic)), Some(&ModelProfile { cache_ttl_minutes: Some(5), ..profile() })),
                Some(ToolLockModel { cache_ttl_minutes: Some(5), ..model("p", "m", ProviderFamily::Anthropic, false) })
            );
            // Whether the lock covers the whole surface is the model's declared capability, read only where the protocol has
            // an append interface.
            assert!(tool_lock_model_of(Some(&provider(ProviderFamily::Anthropic)), Some(&appending)).unwrap().appends_tools);
            assert!(!tool_lock_model_of(Some(&provider(ProviderFamily::OpenaiCompatible)), Some(&appending)).unwrap().appends_tools);
            assert_eq!(tool_lock_model_of(None, Some(&profile())), None);
            assert_eq!(tool_lock_model_of(Some(&provider(ProviderFamily::Anthropic)), None), None);
        }
    }

    mod how_a_setting_is_drawn {
        use super::*;

        #[test]
        fn a_tool_off_with_its_calls_in_the_transcript_is_orange_on_any_model() {
            let called = warm().with_called_tools(&[tool_card("bash", Some("turn"))]);
            assert_eq!(tool_tone(&called, "bash", false, false), CACHE);
            // Touched since the last request, the cache is already lost for it.
            assert_eq!(tool_tone(&called, "bash", true, false), None);
            assert_eq!(tool_tone(&called, "bash", false, true), None);
            // No calls: adding it costs nothing, as for any other tool.
            assert_eq!(tool_tone(&called, "read", false, false), None);
            // A card somebody wrote is not a call the host hid.
            let written = warm().with_called_tools(&[tool_card("bash", None)]);
            assert_eq!(tool_tone(&written, "bash", false, false), None);
            let cold = cold().with_called_tools(&[tool_card("bash", Some("turn"))]);
            assert_eq!(tool_tone(&cold, "bash", false, false), None);
        }

        #[test]
        fn a_web_leg_off_with_its_calls_in_the_transcript_warns_before_it_comes_back() {
            let called = warm().with_called_tools(&[tool_card("web_search", Some("turn"))]);
            assert_eq!(web_leg_tone(&called, Search, false), CACHE);
            assert_eq!(web_leg_tone(&called, Fetch, false), None);
            assert_eq!(web_leg_tone(&warm(), Search, false), None);
            let off = settings(|s| {
                s.web_search_enabled = true;
                s.web_search.provider = SearchProviderSelection::Disabled;
            });
            let on = settings(|s| {
                s.web_search_enabled = true;
                s.web_search.provider = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
            });
            assert!(lock_touch(&called, &off, &on));
            assert!(!lock_touch(&warm(), &off, &on));
        }

        fn warm() -> ToolLockState {
            state(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {}))), &opus(), SENT)
        }

        fn cold() -> ToolLockState {
            state(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {}))), &opus(), SENT + 31 * MINUTE)
        }

        fn whole() -> ToolLockState {
            state(&settings(|s| s.tool_lock = Some(lock(&haiku(), |_| {}))), &haiku(), SENT)
        }

        #[test]
        fn draws_a_tool_the_last_request_carried_orange_while_the_cache_is_warm() {
            assert_eq!(lock_tone(&warm(), Kind::Tool, true, true), CACHE);
            // Adding a tool appends it at the end: nothing cached is lost.
            assert_eq!(lock_tone(&warm(), Kind::Tool, false, false), None);
            // Moved away, the cache is already lost for it; moved back, orange again.
            assert_eq!(lock_tone(&warm(), Kind::Tool, true, false), None);
            assert_eq!(lock_tone(&cold(), Kind::Tool, true, true), None);
        }

        #[test]
        fn draws_the_settings_that_rewrite_the_prefix_either_way_orange_whichever_way_they_stand() {
            for kind in [Kind::Mcp, Kind::Memory, Kind::SkillTool, Kind::Discovery, Kind::HostMessages, Kind::FileWriteGuards, Kind::Environment, Kind::SubagentMessaging] {
                assert_eq!(lock_tone(&warm(), kind, false, false), CACHE, "{kind:?}");
                assert_eq!(lock_tone(&warm(), kind, true, true), CACHE, "{kind:?}");
                assert_eq!(lock_tone(&warm(), kind, false, true), None, "{kind:?}");
            }
        }

        #[test]
        fn draws_every_part_of_the_surface_orange_on_or_off_on_a_model_that_cannot_append_tools_while_the_cache_is_warm() {
            for kind in [Kind::Tool, Kind::WebSearch, Kind::Mcp, Kind::Memory, Kind::SkillTool, Kind::Discovery] {
                // Adding one folds it into the declared list, just as removing one rewrites it.
                assert_eq!(lock_tone(&whole(), kind, false, false), CACHE, "{kind:?}");
                assert_eq!(lock_tone(&whole(), kind, true, true), CACHE, "{kind:?}");
                // Moved away, the cache is already lost for it: plain until it is moved back.
                assert_eq!(lock_tone(&whole(), kind, true, false), None, "{kind:?}");
                assert_eq!(lock_tone(&whole(), kind, false, true), None, "{kind:?}");
            }
            // Skills aren't tools: one added later is a host notice on every model, and only the cache speaks for the ones
            // already sent.
            assert_eq!(lock_tone(&whole(), Kind::Skill, false, false), None);
            assert_eq!(lock_tone(&whole(), Kind::Skill, true, true), CACHE);
        }

        #[test]
        fn tones_nothing_on_a_model_that_cannot_append_tools_once_the_cache_is_cold_nor_for_another_model() {
            let current = settings(|s| s.tool_lock = Some(lock(&haiku(), |_| {})));
            let cold = state(&current, &haiku(), SENT + 31 * MINUTE);
            let elsewhere = state(&current, &opus(), SENT);
            let kinds = [
                Kind::Tool, Kind::WebSearch, Kind::Skill, Kind::Mcp, Kind::Memory, Kind::SkillTool, Kind::Discovery, Kind::Hook,
                Kind::Profile, Kind::HostMessages, Kind::FileWriteGuards, Kind::Environment, Kind::SubagentMessaging,
            ];
            for state in [&cold, &elsewhere] {
                for kind in kinds {
                    assert_eq!(lock_tone(state, kind, false, false), None, "{kind:?}");
                    assert_eq!(lock_tone(state, kind, true, true), None, "{kind:?}");
                }
            }
        }
    }

    mod putting_the_lock_back_when_its_model_is_picked_again {
        use super::*;

        #[test]
        fn returns_what_the_cache_holds_and_keeps_what_was_added_for_free() {
            let last_sent = settings(|s| {
                s.enabled_tools = names(&["write", "bash"]);
                s.mcp_ids = names(&["mcp-b"]);
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.tools = names(&["read", "write"]);
                    l.mcp_ids = names(&["mcp-a"]);
                    l.global_memory = true;
                    l.skill_ids = names(&["s"]);
                }));
            });
            let restored = restore_locked_settings(&last_sent, &state(&last_sent, &opus(), SENT));
            assert_eq!(restored.enabled_tools, names(&["write", "bash", "read"]));
            assert_eq!(restored.mcp_ids, names(&["mcp-a"]));
            assert!(restored.global_memory_enabled);
            assert_eq!(restored.skill_ids, names(&["s"]));
        }

        #[test]
        fn puts_the_whole_surface_back_exactly_on_a_model_that_cannot_append_tools_while_the_cache_is_warm_and_nothing_once_it_is_cold() {
            let moved = settings(|s| {
                s.enabled_tools = names(&["read", "bash"]);
                s.mcp_ids = names(&["mcp-b"]);
                s.web_search_enabled = true;
                s.skill_ids = names(&["t"]);
                s.hook_ids = names(&["hook_b"]);
                s.plan_mode_enabled = true;
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.tools = names(&["read", "write"]);
                    l.mcp_ids = names(&["mcp-a"]);
                    l.skill_ids = names(&["s"]);
                    l.hook_ids = Some(names(&["hook_a"]));
                    l.prompt_profile = Some("tooldesc_user_terse_01234567".into());
                }));
            });
            let restored = restore_locked_settings(&moved, &state(&moved, &haiku(), SENT));
            // A tool added since goes too: on this model adding it rewrote the declared list.
            assert_eq!(restored.enabled_tools, names(&["read", "write"]));
            assert_eq!(restored.mcp_ids, names(&["mcp-a"]));
            assert!(!restored.web_search_enabled);
            // A skill added since is a host notice, which cost nothing: it stays.
            assert_eq!(restored.skill_ids, names(&["t", "s"]));
            // What the system prompt was built from goes back as well.
            assert_eq!(restored.hook_ids, names(&["hook_a"]));
            assert_eq!(restored.tool_description_file_id.as_deref(), Some("tooldesc_user_terse_01234567"));
            // Plan mode is the composer's switch, not part of the surface: putting the tools back leaves it where the user put it.
            assert!(restored.plan_mode_enabled);

            // Cold, the cache holds nothing to put back, whatever the model.
            assert!(restore_moves_nothing(&moved, &state(&moved, &haiku(), SENT + 60 * MINUTE)));
        }

        #[test]
        fn moves_nothing_for_another_model_a_cold_cache_or_settings_already_in_place() {
            let moved = settings(|s| {
                s.enabled_tools = names(&["bash"]);
                s.tool_lock = Some(lock(&opus(), |l| l.tools = names(&["read"])));
            });
            assert!(restore_moves_nothing(&moved, &state(&moved, &gpt(), SENT)));
            assert!(restore_moves_nothing(&moved, &state(&moved, &opus(), SENT + 31 * MINUTE)));
            let whole_moved = with(&moved, |s| s.tool_lock = Some(lock(&haiku(), |l| l.tools = names(&["read"]))));
            assert!(restore_moves_nothing(&whole_moved, &state(&whole_moved, &opus(), SENT)));
            let in_place = settings(|s| {
                s.enabled_tools = names(&["read"]);
                s.tool_lock = Some(lock(&opus(), |l| l.tools = names(&["read"])));
            });
            assert!(restore_moves_nothing(&in_place, &state(&in_place, &opus(), SENT)));
        }
    }

    mod what_a_change_does_to_the_lock {
        use super::*;
        use crate::model::SecurityLevel;

        fn before() -> ConversationSettings {
            settings(|s| {
                s.enabled_tools = names(&["read"]);
                s.skill_ids = names(&["s"]);
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.tools = names(&["read"]);
                    l.skill_ids = names(&["s"]);
                }));
            })
        }

        #[test]
        fn warns_when_an_orange_setting_moves_and_only_then() {
            let before = before();
            let warm = state(&before, &opus(), SENT);
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.enabled_tools.clear())));
            assert!(!lock_touch(&warm, &before, &with(&before, |s| s.enabled_tools = names(&["read", "bash"]))));
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.skill_ids.clear())));
            assert!(!lock_touch(&warm, &before, &with(&before, |s| s.skill_ids = names(&["s", "t"]))));
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.mcp_ids = names(&["mcp-a"]))));
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.project_memory_enabled = true)));
            // Settings the lock doesn't cover never warn.
            assert!(!lock_touch(&warm, &before, &with(&before, |s| s.security_level = SecurityLevel::FullAccess)));
        }

        #[test]
        fn warns_before_a_tool_joins_or_leaves_on_a_model_that_cannot_append_tools_while_the_cache_is_warm_skills_aside() {
            let whole_before = with(&before(), |s| {
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.tools = names(&["read"]);
                    l.skill_ids = names(&["s"]);
                }));
            });
            let whole = state(&whole_before, &haiku(), SENT);
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.enabled_tools = names(&["read", "bash"]))));
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.enabled_tools.clear())));
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.web_search_enabled = true)));
            // Plan mode is the composer's switch, not a setting the lock covers.
            assert!(!lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.plan_mode_enabled = true)));
            // A skill added later is a host notice on every model.
            assert!(!lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.skill_ids = names(&["s", "t"]))));

            // Cold, nothing warns, whichever way the surface moves.
            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert!(!lock_touch(&cold, &whole_before, &with(&whole_before, |s| s.enabled_tools = names(&["read", "bash"]))));
            assert!(!lock_touch(&cold, &whole_before, &with(&whole_before, |s| s.enabled_tools.clear())));
        }

        #[test]
        fn says_nothing_once_another_model_is_selected() {
            let before = before();
            let elsewhere = state(&before, &gpt(), SENT);
            assert!(!lock_touch(&elsewhere, &before, &with(&before, |s| {
                s.enabled_tools.clear();
                s.mcp_ids = names(&["x"]);
            })));
            // Not even about adding a tool where the last request's model could not take one.
            let whole_before = with(&before, |s| s.tool_lock = Some(lock(&haiku(), |l| l.tools = names(&["read"]))));
            let whole_elsewhere = state(&whole_before, &opus(), SENT);
            assert!(!lock_touch(&whole_elsewhere, &whole_before, &with(&whole_before, |s| s.enabled_tools = names(&["read", "bash"]))));
        }
    }

    mod the_web_backends {
        use super::*;

        fn tavily() -> SearchProviderSelection {
            explicit_search(SearchProviderKind::Tavily)
        }

        fn exa() -> SearchProviderSelection {
            explicit_search(SearchProviderKind::Exa)
        }

        fn jina() -> FetchProviderSelection {
            explicit_fetch(SearchProviderKind::Jina)
        }

        /// The last request searched with Tavily and fetched with Jina.
        fn host_run(model: &ToolLockModel) -> ConversationSettings {
            settings(|s| {
                s.web_search_enabled = true;
                s.web_search = native_web(tavily(), jina());
                s.tool_lock = Some(lock(model, |l| {
                    l.web_search = true;
                    l.search_backend = Some(tavily());
                    l.fetch_backend = Some(jina());
                    l.web_fetch = true;
                }));
            })
        }

        #[test]
        fn draws_a_host_run_backend_orange_while_the_cache_is_warm_and_plain_once_it_has_moved() {
            let current = host_run(&opus());
            let warm = state(&current, &opus(), SENT);
            assert_eq!(backend_tone(&warm, Search, &tavily()), CACHE);
            assert_eq!(backend_tone(&warm, Fetch, &jina()), CACHE);
            assert_eq!(backend_tone(&warm, Search, &exa()), None);
            assert_eq!(backend_tone(&state(&current, &opus(), SENT + 31 * MINUTE), Search, &tavily()), None);
            // Another model's cache isn't this one's.
            assert_eq!(backend_tone(&state(&current, &gpt(), SENT), Search, &tavily()), None);
        }

        #[test]
        fn leaves_a_leg_plain_on_a_model_that_appends_tools_when_its_tool_did_not_go_out_since_giving_it_one_is_an_addition() {
            let current = settings(|s| {
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Disabled);
                    l.fetch_backend = Some(FetchProviderSelection::Native);
                    l.web_fetch = false;
                }));
            });
            let warm = state(&current, &opus(), SENT);
            assert_eq!(backend_tone(&warm, Search, &SearchProviderSelection::Disabled), None);
            assert_eq!(backend_tone(&warm, Fetch, &FetchProviderSelection::Native), None);
        }

        #[test]
        fn never_tones_a_native_pin_for_any_model_warm_or_cold() {
            for sender in [opus(), haiku()] {
                let pinned = settings(|s| {
                    s.tool_lock = Some(lock(&sender, |l| {
                        l.web_search = true;
                        l.search_backend = Some(SearchProviderSelection::Native);
                        l.search_provider = Some(SearchProviderSelection::Native);
                    }));
                });
                assert!(backend_pinned(&tool_lock_of(&pinned), Search));
                assert!(!backend_pinned(&tool_lock_of(&pinned), Fetch));
                // Settled rather than locked: its selector says so in its own words.
                for model in [opus(), haiku(), gpt()] {
                    for now in [SENT, SENT + 90 * MINUTE] {
                        assert_eq!(backend_tone(&state(&pinned, &model, now), Search, &SearchProviderSelection::Native), None);
                    }
                }
            }
        }

        /// On a model that can't take a tool mid-conversation, giving a leg its tool folds it into the declared list, so the
        /// selector of a leg that sent nothing is orange too. A move between two choices that both offer no tool shows the model
        /// nothing new, and doesn't warn.
        #[test]
        fn warns_on_a_model_that_cannot_append_tools_before_a_backend_move_adds_a_tool_but_not_between_two_choices_that_offer_none() {
            let chat = model("openai", "gpt-chat", ProviderFamily::OpenaiChat, false);
            // Search was Off, and native fetch is no tool of its own on this family: the last request offered neither web tool.
            let before = settings(|s| {
                s.web_search_enabled = true;
                s.web_search = native_web(SearchProviderSelection::Disabled, FetchProviderSelection::Native);
                s.tool_lock = Some(lock(&chat, |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Disabled);
                    l.fetch_backend = Some(FetchProviderSelection::Native);
                    l.web_fetch = false;
                }));
            });
            let warm = state(&before, &chat, SENT);
            assert!(warm.whole_surface);
            assert_eq!(backend_tone(&warm, Search, &SearchProviderSelection::Disabled), CACHE);
            assert_eq!(backend_tone(&warm, Fetch, &FetchProviderSelection::Native), CACHE);

            let moved = |patch: fn(&mut ConversationWebSearchSettings)| with(&before, |s| patch(&mut s.web_search));
            assert!(lock_touch(&warm, &before, &moved(|web| web.provider = exa())));
            assert!(lock_touch(&warm, &before, &moved(|web| web.provider = SearchProviderSelection::Native)));
            // Upstream names `firecrawl` here, a kind the host's enum doesn't have; any explicit provider means the same.
            assert!(lock_touch(&warm, &before, &moved(|web| web.fetch_provider = explicit_fetch(SearchProviderKind::Fetch))));
            // Off and native fetch both leave `web_fetch` out here.
            assert!(!lock_touch(&warm, &before, &moved(|web| web.fetch_provider = FetchProviderSelection::Disabled)));
            // Cold, nothing warns.
            assert!(!lock_touch(&state(&before, &chat, SENT + 90 * MINUTE), &before, &moved(|web| web.provider = exa())));

            // Picked again while warm, both legs go back; cold, what was picked stays.
            let added = moved(|web| {
                web.provider = exa();
                web.fetch_provider = jina();
            });
            let restored = restore_locked_settings(&added, &state(&added, &chat, SENT));
            assert_eq!(restored.web_search.provider, SearchProviderSelection::Disabled);
            assert_eq!(restored.web_search.fetch_provider, FetchProviderSelection::Native);
            assert!(restore_moves_nothing(&added, &state(&added, &chat, SENT + 90 * MINUTE)));
        }

        #[test]
        fn warns_before_a_warm_host_run_backend_moves_on_any_model_and_never_about_a_pinned_native_one() {
            for model in [opus(), haiku()] {
                let before = host_run(&model);
                let warm = state(&before, &model, SENT);
                let moved = |patch: fn(&mut ConversationWebSearchSettings)| with(&before, |s| patch(&mut s.web_search));
                assert!(lock_touch(&warm, &before, &moved(|web| web.provider = exa())));
                assert!(lock_touch(&warm, &before, &moved(|web| web.fetch_provider = FetchProviderSelection::Disabled)));
                // Settings beside the backends aren't the lock's.
                assert!(!lock_touch(&warm, &before, &moved(|web| web.max_results = 3)));
                assert!(!lock_touch(&state(&before, &model, SENT + 31 * MINUTE), &before, &moved(|web| web.provider = exa())));

                // A pinned leg is settled, not locked: its selector doesn't move, and the warning has nothing to say about it.
                let pinned = with(&before, |s| {
                    s.web_search.provider = SearchProviderSelection::Native;
                    s.tool_lock = Some(lock(&model, |l| {
                        l.web_search = true;
                        l.search_backend = Some(SearchProviderSelection::Native);
                        l.search_provider = Some(SearchProviderSelection::Native);
                    }));
                });
                let to_exa = with(&pinned, |s| s.web_search.provider = exa());
                assert!(!lock_touch(&state(&pinned, &model, SENT), &pinned, &to_exa));
                assert!(!lock_touch(&state(&pinned, &gpt(), SENT + 90 * MINUTE), &pinned, &to_exa));
            }
        }

        #[test]
        fn puts_a_warm_backend_back_when_its_model_is_picked_again_and_on_a_model_that_cannot_append_tools_even_a_leg_that_sent_nothing() {
            let moved = with(&host_run(&opus()), |s| {
                s.web_search.provider = exa();
                s.web_search.fetch_provider = FetchProviderSelection::Disabled;
            });
            let restored = restore_locked_settings(&moved, &state(&moved, &opus(), SENT));
            assert_eq!(restored.web_search.provider, tavily());
            assert_eq!(restored.web_search.fetch_provider, jina());

            // A leg that sent nothing keeps what was picked since: that was free.
            let off_leg = settings(|s| {
                s.web_search_enabled = true;
                s.web_search.provider = exa();
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Disabled);
                }));
            });
            assert_eq!(restore_locked_settings(&off_leg, &state(&off_leg, &opus(), SENT)).web_search.provider, exa());
            // Where turning a leg on folds a tool into the declared list, even "off" goes back while warm; cold, nothing does.
            let whole_off = with(&off_leg, |s| {
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Disabled);
                }));
            });
            assert_eq!(restore_locked_settings(&whole_off, &state(&whole_off, &haiku(), SENT)).web_search.provider, SearchProviderSelection::Disabled);
            assert!(restore_moves_nothing(&whole_off, &state(&whole_off, &haiku(), SENT + 90 * MINUTE)));

            // Backends already in place move nothing.
            let in_place = host_run(&opus());
            assert!(restore_moves_nothing(&in_place, &state(&in_place, &opus(), SENT)));
        }

        /// A leg's switch is toned like a tool's own row, with the selector under it toned on its own.
        #[test]
        fn draws_a_legs_switch_orange_while_its_tool_went_out_and_the_cache_is_warm_and_plain_once_it_has_moved_or_gone_cold() {
            let current = host_run(&opus());
            let warm = state(&current, &opus(), SENT);
            assert_eq!(web_leg_tone(&warm, Search, true), CACHE);
            assert_eq!(web_leg_tone(&warm, Fetch, true), CACHE);
            // Switched off since: the cache is already lost for that leg.
            assert_eq!(web_leg_tone(&warm, Search, false), None);
            assert_eq!(web_leg_tone(&warm, Fetch, false), None);
            // Cold, or another model's cache: nothing to protect.
            let cold = state(&current, &opus(), SENT + 31 * MINUTE);
            assert_eq!(web_leg_tone(&cold, Search, true), None);
            assert_eq!(web_leg_tone(&cold, Fetch, true), None);
            let elsewhere = state(&current, &gpt(), SENT);
            assert_eq!(web_leg_tone(&elsewhere, Search, true), None);
            assert_eq!(web_leg_tone(&elsewhere, Fetch, true), None);
        }

        #[test]
        fn tones_each_leg_by_its_own_tool_and_a_conversation_that_never_ran_by_neither() {
            // Only search went out; there is no `web_fetch` to protect.
            let search_only = settings(|s| {
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(tavily());
                    l.fetch_backend = Some(FetchProviderSelection::Disabled);
                    l.web_fetch = false;
                }));
            });
            let warm = state(&search_only, &opus(), SENT);
            assert_eq!(web_leg_tone(&warm, Search, true), CACHE);
            // Adding the missing tool appends it: plain on a model that can append.
            assert_eq!(web_leg_tone(&warm, Fetch, false), None);
            assert_eq!(web_leg_tone(&warm, Fetch, true), None);

            // A search leg whose backend went out as Off sent no tool either.
            let search_off = settings(|s| {
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.web_search = true;
                    l.search_backend = Some(SearchProviderSelection::Disabled);
                    l.web_fetch = true;
                }));
            });
            let off_warm = state(&search_off, &opus(), SENT);
            assert_eq!(web_leg_tone(&off_warm, Search, false), None);
            assert_eq!(web_leg_tone(&off_warm, Fetch, true), CACHE);

            assert_eq!(web_leg_tone(&state(&plain(), &opus(), SENT), Search, true), None);
            assert_eq!(web_leg_tone(&state(&plain(), &opus(), SENT), Fetch, false), None);
        }

        /// Where turning a leg on folds its tool into the declared list, a leg that sent nothing is orange too, while warm.
        #[test]
        fn tones_a_leg_that_sent_nothing_only_on_a_model_that_cannot_append_tools_and_only_while_it_stays_off() {
            let none = |model: &ToolLockModel| {
                settings(|s| {
                    s.tool_lock = Some(lock(model, |l| {
                        l.web_search = true;
                        l.search_backend = Some(SearchProviderSelection::Disabled);
                        l.fetch_backend = Some(FetchProviderSelection::Disabled);
                        l.web_fetch = false;
                    }));
                })
            };
            assert_eq!(web_leg_tone(&state(&none(&opus()), &opus(), SENT), Search, false), None);
            assert_eq!(web_leg_tone(&state(&none(&opus()), &opus(), SENT), Fetch, false), None);

            let whole = state(&none(&haiku()), &haiku(), SENT);
            assert_eq!(web_leg_tone(&whole, Search, false), CACHE);
            assert_eq!(web_leg_tone(&whole, Fetch, false), CACHE);
            // Switched on since, it has moved away from what the cache holds.
            assert_eq!(web_leg_tone(&whole, Search, true), None);
            assert_eq!(web_leg_tone(&whole, Fetch, true), None);
            assert_eq!(web_leg_tone(&state(&none(&haiku()), &haiku(), SENT + 31 * MINUTE), Search, false), None);
        }
    }

    mod each_models_own_cache {
        use super::*;

        fn at(context: ToolLockRequestContext, at: &str) -> ToolLockRequestContext {
            ToolLockRequestContext { at: at.into(), ..context }
        }

        fn warm_until(settings: &ConversationSettings, model: &ToolLockModel, now: i64) -> Option<i64> {
            model_cache_warm_until(settings, &model.provider_id, &model.model_id, model.cache_ttl_minutes, now)
        }

        #[test]
        fn keeps_every_models_latest_request_one_entry_per_model() {
            let first = run(&plain(), &[], &request(&opus()));
            let later = "2026-09-30T10:10:00.000Z";
            let second = run(&first, &[], &at(request(&gpt()), later));
            assert_eq!(tool_lock_of(&second).last_request.unwrap().model_id, "gpt-5.5");
            let models: Vec<String> = tool_lock_of(&second).model_requests.into_iter().map(|item| item.model_id).collect();
            assert_eq!(models, names(&["claude-opus-5-5", "gpt-5.5"]));
            let third = run(&second, &[], &at(request(&opus()), later));
            assert_eq!(
                tool_lock_of(&third).model_requests,
                [
                    ToolLockRequest { provider_id: "openai".into(), model_id: "gpt-5.5".into(), at: later.into() },
                    ToolLockRequest { provider_id: "anthropic".into(), model_id: "claude-opus-5-5".into(), at: later.into() },
                ]
            );
        }

        #[test]
        fn marks_each_model_warm_for_its_own_lifetime_whichever_sent_last() {
            let current = run(&run(&plain(), &[], &request(&opus())), &[], &at(request(&gpt()), "2026-09-30T10:20:00.000Z"));
            // Opus sent first and isn't selected, yet its cache still holds.
            assert_eq!(warm_until(&current, &opus(), SENT + 25 * MINUTE), Some(SENT + 30 * MINUTE));
            assert_eq!(warm_until(&current, &opus(), SENT + 30 * MINUTE), None);
            assert_eq!(warm_until(&current, &gpt(), SENT + 45 * MINUTE), Some(SENT + 50 * MINUTE));
            assert_eq!(warm_until(&current, &with(&gpt(), |m| m.cache_ttl_minutes = Some(5)), SENT + 26 * MINUTE), None);
            assert_eq!(warm_until(&current, &haiku(), SENT), None);
        }

        #[test]
        fn reads_a_lock_from_before_the_list_existed_as_knowing_its_last_request() {
            let older = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.model_requests = Vec::new())));
            assert_eq!(warm_until(&older, &opus(), SENT), Some(SENT + 30 * MINUTE));
        }

        #[test]
        fn counts_from_the_moment_a_long_runs_last_request_went_out_for_that_model_too() {
            let current = run(&run(&plain(), &[], &request(&gpt())), &[], &request(&opus()));
            let later = "2026-09-30T10:40:00.000Z";
            let refreshed = refreshed_tool_lock(&current, later, false);
            assert_eq!(refreshed.last_request.as_ref().unwrap().at, later);
            let at_of = |model_id: &str| refreshed.model_requests.iter().find(|item| item.model_id == model_id).unwrap().at.as_str();
            assert_eq!(at_of("claude-opus-5-5"), later);
            assert_eq!(at_of("gpt-5.5"), SENT_AT);
            assert_eq!(refreshed_tool_lock(&plain(), later, false), empty_tool_lock());
        }
    }

    mod unticking_a_dangling_selection {
        use super::*;

        #[test]
        fn leaves_the_conversation_and_the_lock_alike_so_no_restore_puts_it_back() {
            let selected = settings(|s| {
                s.mcp_ids = names(&["mcp_gone", "mcp_ok"]);
                s.skill_ids = names(&["skill_gone"]);
                s.hook_ids = names(&["hook_gone"]);
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.mcp_ids = names(&["mcp_gone", "mcp_ok"]);
                    l.skill_ids = names(&["skill_gone"]);
                }));
            });
            let cleared = without_dangling_selection(&selected, DanglingSelectionKind::Mcp, "mcp_gone");
            assert_eq!(cleared.mcp_ids, names(&["mcp_ok"]));
            assert_eq!(cleared.tool_lock.as_ref().unwrap().mcp_ids, names(&["mcp_ok"]));
            let restored = restore_locked_settings(&cleared, &state(&cleared, &haiku(), SENT));
            assert_eq!(restored.mcp_ids, names(&["mcp_ok"]));

            let skill = without_dangling_selection(&selected, DanglingSelectionKind::Skills, "skill_gone");
            assert!(skill.skill_ids.is_empty());
            assert!(skill.tool_lock.unwrap().skill_ids.is_empty());
            assert!(without_dangling_selection(&selected, DanglingSelectionKind::Hooks, "hook_gone").hook_ids.is_empty());
            // A conversation that never ran has no lock to edit.
            let never = settings(|s| s.mcp_ids = names(&["mcp_gone"]));
            assert_eq!(without_dangling_selection(&never, DanglingSelectionKind::Mcp, "mcp_gone").tool_lock, None);
        }

        /// Not an upstream case: the two branches upstream's code has and its case doesn't reach.
        #[test]
        fn takes_a_role_from_the_selection_alone_and_a_hook_from_a_lock_that_recorded_it() {
            let selected = settings(|s| {
                s.agent_ids = names(&["agent_gone", "agent_ok"]);
                s.hook_ids = names(&["hook_gone"]);
                s.tool_lock = Some(lock(&haiku(), |l| l.hook_ids = Some(names(&["hook_gone"]))));
            });
            let role = without_dangling_selection(&selected, DanglingSelectionKind::Agents, "agent_gone");
            assert_eq!(role.agent_ids, names(&["agent_ok"]));
            assert_eq!(role.tool_lock, selected.tool_lock);
            let hook = without_dangling_selection(&selected, DanglingSelectionKind::Hooks, "hook_gone");
            assert_eq!(hook.tool_lock.unwrap().hook_ids, Some(Vec::new()));
        }
    }

    mod hooks_and_the_prompt_profile {
        use super::*;

        const TERSE: &str = "tooldesc_user_terse_01234567";

        fn before() -> ConversationSettings {
            settings(|s| {
                s.hook_ids = names(&["hook_a"]);
                s.tool_description_file_id = Some(TERSE.into());
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.hook_ids = Some(names(&["hook_a"]));
                    l.prompt_profile = Some(TERSE.into());
                }));
            })
        }

        #[test]
        fn records_what_the_requests_system_prompt_was_built_from() {
            let sent = run(&before(), &[], &request(&opus()));
            assert_eq!(tool_lock_of(&sent).hook_ids, Some(names(&["hook_a"])));
            assert_eq!(tool_lock_of(&sent).prompt_profile.as_deref(), Some(TERSE));
            // No selection is the built-in profile.
            let builtin = run(&plain(), &[], &request(&opus()));
            assert_eq!(tool_lock_of(&builtin).prompt_profile.as_deref(), Some(BUILTIN_PROMPT_PROFILE_ID));
        }

        #[test]
        fn warns_before_any_of_them_moves_while_the_cache_is_warm_either_way() {
            let before = before();
            let warm = state(&before, &opus(), SENT);
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.hook_ids.clear())));
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.hook_ids = names(&["hook_a", "hook_b"]))));
            assert!(lock_touch(&warm, &before, &with(&before, |s| s.tool_description_file_id = None)));
            assert_eq!(prompt_profile_tone(&warm, &before), CACHE);
            assert_eq!(lock_tone(&warm, Kind::Hook, true, true), CACHE);
            // Moved away, the cache is already lost for it: plain again.
            assert_eq!(prompt_profile_tone(&warm, &with(&before, |s| s.tool_description_file_id = None)), None);
            // Cold, nothing warns.
            let cold = state(&before, &opus(), SENT + 31 * MINUTE);
            assert!(!lock_touch(&cold, &before, &with(&before, |s| s.hook_ids.clear())));
        }

        #[test]
        fn warns_before_they_move_on_a_model_that_cannot_append_tools_just_the_same_and_only_while_the_cache_is_warm() {
            let whole_before = with(&before(), |s| {
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.hook_ids = Some(names(&["hook_a"]));
                    l.prompt_profile = Some(TERSE.into());
                }));
            });
            let whole = state(&whole_before, &haiku(), SENT);
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.hook_ids.clear())));
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.hook_ids = names(&["hook_a", "hook_b"]))));
            assert!(lock_touch(&whole, &whole_before, &with(&whole_before, |s| s.tool_description_file_id = None)));
            assert_eq!(prompt_profile_tone(&whole, &whole_before), CACHE);
            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert!(!lock_touch(&cold, &whole_before, &with(&whole_before, |s| s.hook_ids.clear())));
            assert_eq!(prompt_profile_tone(&cold, &whole_before), None);
        }

        #[test]
        fn tones_nothing_a_lock_from_before_they_were_recorded_never_saw() {
            let older = settings(|s| {
                s.hook_ids = names(&["hook_a"]);
                s.tool_lock = Some(lock(&opus(), |_| {}));
            });
            let locked = state(&older, &opus(), SENT);
            assert!(!lock_touch(&locked, &older, &with(&older, |s| {
                s.hook_ids.clear();
                s.tool_description_file_id = Some("x".into());
            })));
            assert!(restore_moves_nothing(&older, &locked));
        }

        #[test]
        fn puts_them_back_when_the_warm_model_is_picked_again_whether_or_not_it_appends_tools() {
            let moved = with(&before(), |s| {
                s.hook_ids = names(&["hook_b"]);
                s.tool_description_file_id = None;
            });
            let restored = restore_locked_settings(&moved, &state(&moved, &opus(), SENT));
            assert_eq!(restored.hook_ids, names(&["hook_a"]));
            assert_eq!(restored.tool_description_file_id.as_deref(), Some(TERSE));

            let whole_moved = with(&moved, |s| {
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.hook_ids = Some(names(&["hook_a"]));
                    l.prompt_profile = Some(TERSE.into());
                }));
            });
            let whole_restored = restore_locked_settings(&whole_moved, &state(&whole_moved, &haiku(), SENT));
            assert_eq!(whole_restored.hook_ids, names(&["hook_a"]));
            assert_eq!(whole_restored.tool_description_file_id.as_deref(), Some(TERSE));
        }

        /// Not an upstream case: nothing, the built-in's id and the retired one's all read as the built-in.
        #[test]
        fn reads_the_retired_profile_as_the_built_in() {
            assert!(is_builtin_prompt_profile_id(None));
            assert!(is_builtin_prompt_profile_id(Some("")));
            assert!(is_builtin_prompt_profile_id(Some(BUILTIN_PROMPT_PROFILE_ID)));
            assert!(is_builtin_prompt_profile_id(Some(RETIRED_GUIDED_PROMPT_PROFILE_ID)));
            assert!(!is_builtin_prompt_profile_id(Some(TERSE)));
            let retired = settings(|s| s.tool_description_file_id = Some(RETIRED_GUIDED_PROMPT_PROFILE_ID.into()));
            assert_eq!(prompt_profile_of(&retired), BUILTIN_PROMPT_PROFILE_ID);
        }
    }

    mod the_host_message_container {
        use super::*;

        fn before() -> ConversationSettings {
            settings(|s| {
                s.host_message_container = HostMessageContainer::Box;
                s.tool_lock = Some(lock(&opus(), |l| l.host_message_container = Some(HostMessageContainer::Box)));
            })
        }

        #[test]
        fn records_the_container_each_request_projected_its_host_messages_in() {
            assert_eq!(tool_lock_of(&run(&before(), &["box"], &request(&opus()))).host_message_container, Some(HostMessageContainer::Box));
            // Absent is the host's default: user messages.
            assert_eq!(tool_lock_of(&run(&plain(), &[], &request(&opus()))).host_message_container, Some(HostMessageContainer::User));
        }

        #[test]
        fn warns_before_it_moves_while_the_cache_is_warm_either_way() {
            let before = before();
            let warm = state(&before, &opus(), SENT);
            let user = with(&before, |s| s.host_message_container = HostMessageContainer::User);
            assert_eq!(host_message_container_tone(&warm, &before), CACHE);
            assert!(lock_touch(&warm, &before, &user));
            // Moved away, the cache is already lost for it: plain again.
            assert_eq!(host_message_container_tone(&warm, &user), None);
            assert_eq!(host_message_container_tone(&state(&before, &opus(), SENT + 31 * MINUTE), &before), None);
        }

        #[test]
        fn warns_on_a_model_that_cannot_append_tools_while_the_cache_is_warm_and_holds_nothing_once_it_is_cold() {
            let whole_before = settings(|s| s.tool_lock = Some(lock(&haiku(), |l| l.host_message_container = Some(HostMessageContainer::User))));
            let whole = state(&whole_before, &haiku(), SENT);
            let moved = with(&whole_before, |s| s.host_message_container = HostMessageContainer::Box);
            assert_eq!(host_message_container_tone(&whole, &whole_before), CACHE);
            assert!(lock_touch(&whole, &whole_before, &moved));
            // Put back when the warm model is picked again: `box` comes and goes with it.
            assert_eq!(restore_locked_settings(&moved, &state(&moved, &haiku(), SENT)).host_message_container, HostMessageContainer::User);

            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert_eq!(host_message_container_tone(&cold, &whole_before), None);
            assert!(!lock_touch(&cold, &whole_before, &moved));
            assert!(restore_moves_nothing(&moved, &state(&moved, &haiku(), SENT + 60 * MINUTE)));
        }

        #[test]
        fn puts_it_back_when_the_warm_model_is_picked_again_and_tones_nothing_an_older_lock_never_saw() {
            let moved = with(&before(), |s| s.host_message_container = HostMessageContainer::User);
            assert_eq!(restore_locked_settings(&moved, &state(&moved, &opus(), SENT)).host_message_container, HostMessageContainer::Box);
            let older = settings(|s| {
                s.host_message_container = HostMessageContainer::Box;
                s.tool_lock = Some(lock(&opus(), |_| {}));
            });
            let locked = state(&older, &opus(), SENT);
            assert_eq!(host_message_container_tone(&locked, &older), None);
            assert!(!lock_touch(&locked, &older, &with(&older, |s| s.host_message_container = HostMessageContainer::User)));
            assert!(restore_moves_nothing(&older, &locked));
        }
    }

    mod the_file_write_guards {
        use super::*;

        fn before() -> ConversationSettings {
            settings(|s| {
                s.file_write_guards_enabled = false;
                s.tool_lock = Some(lock(&opus(), |l| l.file_write_guards = Some(false)));
            })
        }

        #[test]
        fn records_whether_each_request_ran_with_the_guards_absent_meaning_on() {
            assert_eq!(tool_lock_of(&run(&before(), &[], &request(&opus()))).file_write_guards, Some(false));
            assert_eq!(tool_lock_of(&run(&plain(), &[], &request(&opus()))).file_write_guards, Some(true));
            assert_eq!(tool_lock_of(&run(&settings(|s| s.file_write_guards_enabled = true), &[], &request(&opus()))).file_write_guards, Some(true));
            // A lock that holds the same answer isn't a change to write.
            let sent = run(&before(), &[], &request(&opus()));
            assert!(run_moves_nothing(&sent, &[], &request(&opus())));
            // The switch moving is: the next request leaves the other answer behind.
            let flipping = with(&sent, |s| s.file_write_guards_enabled = true);
            assert!(!run_moves_nothing(&flipping, &[], &request(&opus())));
            assert_eq!(tool_lock_of(&run(&flipping, &[], &request(&opus()))).file_write_guards, Some(true));
        }

        #[test]
        fn reads_a_lock_written_before_the_switch_as_knowing_nothing() {
            assert_eq!(empty_tool_lock().file_write_guards, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})))).file_write_guards, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.file_write_guards = Some(true))))).file_write_guards, Some(true));
        }

        #[test]
        fn warns_before_it_moves_while_the_cache_is_warm_either_way() {
            let before = before();
            let warm = state(&before, &opus(), SENT);
            let flipped = with(&before, |s| s.file_write_guards_enabled = true);
            assert_eq!(file_write_guards_tone(&warm, &before), CACHE);
            assert!(lock_touch(&warm, &before, &flipped));
            // The other way too: guards on at the last request, switched off now.
            let on = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.file_write_guards = Some(true))));
            let on_warm = state(&on, &opus(), SENT);
            assert_eq!(file_write_guards_tone(&on_warm, &on), CACHE);
            assert!(lock_touch(&on_warm, &on, &with(&on, |s| s.file_write_guards_enabled = false)));
            // Writing the value it already has is no move.
            assert!(!lock_touch(&on_warm, &on, &with(&on, |s| s.file_write_guards_enabled = true)));
            // Moved away, the cache is already lost for it: plain again.
            assert_eq!(file_write_guards_tone(&warm, &flipped), None);
            let cold = state(&before, &opus(), SENT + 31 * MINUTE);
            assert_eq!(file_write_guards_tone(&cold, &before), None);
            assert!(!lock_touch(&cold, &before, &flipped));
        }

        #[test]
        fn warns_on_a_model_that_cannot_append_tools_while_the_cache_is_warm_and_holds_nothing_once_it_is_cold() {
            let whole_before = settings(|s| s.tool_lock = Some(lock(&haiku(), |l| l.file_write_guards = Some(true))));
            let whole = state(&whole_before, &haiku(), SENT);
            let moved = with(&whole_before, |s| s.file_write_guards_enabled = false);
            assert_eq!(file_write_guards_tone(&whole, &whole_before), CACHE);
            assert!(lock_touch(&whole, &whole_before, &moved));
            assert!(restore_locked_settings(&moved, &state(&moved, &haiku(), SENT)).file_write_guards_enabled);

            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert_eq!(file_write_guards_tone(&cold, &whole_before), None);
            assert!(!lock_touch(&cold, &whole_before, &moved));
            assert!(restore_moves_nothing(&moved, &state(&moved, &haiku(), SENT + 60 * MINUTE)));
        }

        #[test]
        fn puts_it_back_when_the_warm_model_is_picked_again_and_tones_nothing_an_older_lock_never_saw() {
            let moved = with(&before(), |s| s.file_write_guards_enabled = true);
            assert!(!restore_locked_settings(&moved, &state(&moved, &opus(), SENT)).file_write_guards_enabled);
            // Absent is on, so a lock holding on puts back nothing against an absent field.
            let absent = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.file_write_guards = Some(true))));
            assert!(restore_moves_nothing(&absent, &state(&absent, &opus(), SENT)));

            let older = settings(|s| {
                s.file_write_guards_enabled = false;
                s.tool_lock = Some(lock(&opus(), |_| {}));
            });
            let locked = state(&older, &opus(), SENT);
            assert_eq!(file_write_guards_tone(&locked, &older), None);
            assert!(!lock_touch(&locked, &older, &with(&older, |s| s.file_write_guards_enabled = true)));
            assert!(restore_moves_nothing(&older, &locked));
        }
    }

    mod the_environment_prompt {
        use super::*;

        fn before() -> ConversationSettings {
            settings(|s| {
                s.environment_prompt_enabled = false;
                s.tool_lock = Some(lock(&opus(), |l| l.environment_prompt = Some(false)));
            })
        }

        #[test]
        fn records_whether_each_request_ran_with_the_environment_section_absent_meaning_on() {
            assert_eq!(tool_lock_of(&run(&before(), &[], &request(&opus()))).environment_prompt, Some(false));
            assert_eq!(tool_lock_of(&run(&plain(), &[], &request(&opus()))).environment_prompt, Some(true));
            assert_eq!(tool_lock_of(&run(&settings(|s| s.environment_prompt_enabled = true), &[], &request(&opus()))).environment_prompt, Some(true));
            // A lock that holds the same answer isn't a change to write.
            let sent = run(&before(), &[], &request(&opus()));
            assert!(run_moves_nothing(&sent, &[], &request(&opus())));
            // The switch moving is: the next request leaves the other answer behind.
            let flipping = with(&sent, |s| s.environment_prompt_enabled = true);
            assert!(!run_moves_nothing(&flipping, &[], &request(&opus())));
            assert_eq!(tool_lock_of(&run(&flipping, &[], &request(&opus()))).environment_prompt, Some(true));
        }

        #[test]
        fn reads_a_lock_written_before_the_switch_as_knowing_nothing() {
            assert_eq!(empty_tool_lock().environment_prompt, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})))).environment_prompt, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.environment_prompt = Some(true))))).environment_prompt, Some(true));
        }

        #[test]
        fn warns_before_it_moves_while_the_cache_is_warm_either_way() {
            let before = before();
            let warm = state(&before, &opus(), SENT);
            let flipped = with(&before, |s| s.environment_prompt_enabled = true);
            assert_eq!(environment_prompt_tone(&warm, &before), CACHE);
            assert!(lock_touch(&warm, &before, &flipped));
            // The other way too: the section on at the last request, switched off now.
            let on = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.environment_prompt = Some(true))));
            let on_warm = state(&on, &opus(), SENT);
            assert_eq!(environment_prompt_tone(&on_warm, &on), CACHE);
            assert!(lock_touch(&on_warm, &on, &with(&on, |s| s.environment_prompt_enabled = false)));
            // Writing the value it already has is no move.
            assert!(!lock_touch(&on_warm, &on, &with(&on, |s| s.environment_prompt_enabled = true)));
            // Moved away, the cache is already lost for it: plain again.
            assert_eq!(environment_prompt_tone(&warm, &flipped), None);
            let cold = state(&before, &opus(), SENT + 31 * MINUTE);
            assert_eq!(environment_prompt_tone(&cold, &before), None);
            assert!(!lock_touch(&cold, &before, &flipped));
        }

        #[test]
        fn warns_on_a_model_that_cannot_append_tools_while_the_cache_is_warm_and_holds_nothing_once_it_is_cold() {
            let whole_before = settings(|s| s.tool_lock = Some(lock(&haiku(), |l| l.environment_prompt = Some(true))));
            let whole = state(&whole_before, &haiku(), SENT);
            let moved = with(&whole_before, |s| s.environment_prompt_enabled = false);
            assert_eq!(environment_prompt_tone(&whole, &whole_before), CACHE);
            assert!(lock_touch(&whole, &whole_before, &moved));
            assert!(restore_locked_settings(&moved, &state(&moved, &haiku(), SENT)).environment_prompt_enabled);

            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert_eq!(environment_prompt_tone(&cold, &whole_before), None);
            assert!(!lock_touch(&cold, &whole_before, &moved));
            assert!(restore_moves_nothing(&moved, &state(&moved, &haiku(), SENT + 60 * MINUTE)));
        }

        #[test]
        fn puts_it_back_when_the_warm_model_is_picked_again_and_tones_nothing_an_older_lock_never_saw() {
            let moved = with(&before(), |s| s.environment_prompt_enabled = true);
            assert!(!restore_locked_settings(&moved, &state(&moved, &opus(), SENT)).environment_prompt_enabled);
            // Absent is on, so a lock holding on puts back nothing against an absent field.
            let absent = settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.environment_prompt = Some(true))));
            assert!(restore_moves_nothing(&absent, &state(&absent, &opus(), SENT)));

            let older = settings(|s| {
                s.environment_prompt_enabled = false;
                s.tool_lock = Some(lock(&opus(), |_| {}));
            });
            let locked = state(&older, &opus(), SENT);
            assert_eq!(environment_prompt_tone(&locked, &older), None);
            assert!(!lock_touch(&locked, &older, &with(&older, |s| s.environment_prompt_enabled = true)));
            assert!(restore_moves_nothing(&older, &locked));
        }
    }

    mod subagent_communication {
        use super::*;

        // The switch reaches the wire only with `agent_spawn` among the tools: it adds `send_message` and rewrites the
        // `agent_spawn` wording.
        const SPAWN: [&str; 1] = ["agent_spawn"];

        fn off() -> ConversationSettings {
            settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.tools = names(&SPAWN);
                    l.subagent_messaging = Some(false);
                }));
            })
        }

        fn on() -> ConversationSettings {
            settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.subagent_messaging_enabled = true;
                s.tool_lock = Some(lock(&opus(), |l| {
                    l.tools = names(&SPAWN);
                    l.subagent_messaging = Some(true);
                }));
            })
        }

        #[test]
        fn records_the_switch_each_request_saw_absent_meaning_off() {
            assert_eq!(tool_lock_of(&run(&plain(), &SPAWN, &request(&opus()))).subagent_messaging, Some(false));
            assert_eq!(tool_lock_of(&run(&settings(|s| s.subagent_messaging_enabled = false), &SPAWN, &request(&opus()))).subagent_messaging, Some(false));
            assert_eq!(tool_lock_of(&run(&on(), &SPAWN, &request(&opus()))).subagent_messaging, Some(true));
            // A lock that holds the same answer isn't a change to write.
            let sent = run(&on(), &SPAWN, &request(&opus()));
            assert!(run_moves_nothing(&sent, &SPAWN, &request(&opus())));
            // The switch moving is: the next request leaves the other answer behind.
            let flipping = with(&sent, |s| s.subagent_messaging_enabled = false);
            assert!(!run_moves_nothing(&flipping, &SPAWN, &request(&opus())));
            assert_eq!(tool_lock_of(&run(&flipping, &SPAWN, &request(&opus()))).subagent_messaging, Some(false));
        }

        #[test]
        fn reads_a_lock_written_before_the_switch_as_knowing_nothing() {
            assert_eq!(empty_tool_lock().subagent_messaging, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})))).subagent_messaging, None);
            assert_eq!(tool_lock_of(&settings(|s| s.tool_lock = Some(lock(&opus(), |l| l.subagent_messaging = Some(true))))).subagent_messaging, Some(true));
        }

        #[test]
        fn warns_before_it_moves_while_the_cache_is_warm_and_agent_spawn_is_on_either_way() {
            let (off, on) = (off(), on());
            let warm_off = state(&off, &opus(), SENT);
            let warm_on = state(&on, &opus(), SENT);
            let switched_on = with(&off, |s| s.subagent_messaging_enabled = true);
            assert_eq!(subagent_messaging_tone(&warm_off, &off), CACHE);
            assert!(lock_touch(&warm_off, &off, &switched_on));
            assert_eq!(subagent_messaging_tone(&warm_on, &on), CACHE);
            assert!(lock_touch(&warm_on, &on, &with(&on, |s| s.subagent_messaging_enabled = false)));
            // Writing the value it already has is no move.
            assert!(!lock_touch(&warm_on, &on, &with(&on, |s| s.subagent_messaging_enabled = true)));
            assert!(!lock_touch(&warm_off, &off, &with(&off, |s| s.subagent_messaging_enabled = false)));
            // Moved away, the cache is already lost for it: plain again.
            assert_eq!(subagent_messaging_tone(&warm_off, &switched_on), None);
            let cold = state(&off, &opus(), SENT + 31 * MINUTE);
            assert_eq!(subagent_messaging_tone(&cold, &off), None);
            assert!(!lock_touch(&cold, &off, &switched_on));
        }

        #[test]
        fn tones_nothing_without_agent_spawn_in_the_last_request_or_in_these_settings() {
            let off = off();
            let warm_off = state(&off, &opus(), SENT);
            // The settings no longer hold the tool: the tool's own row speaks.
            let dropped = with(&off, |s| s.enabled_tools.clear());
            assert_eq!(subagent_messaging_tone(&warm_off, &dropped), None);
            assert!(!lock_touch(&warm_off, &dropped, &with(&dropped, |s| s.subagent_messaging_enabled = true)));
            // The last request never carried it: giving it the tool is the tool's change.
            let never = settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.tool_lock = Some(lock(&opus(), |l| l.subagent_messaging = Some(false)));
            });
            let never_state = state(&never, &opus(), SENT);
            assert_eq!(subagent_messaging_tone(&never_state, &never), None);
            assert!(!lock_touch(&never_state, &never, &with(&never, |s| s.subagent_messaging_enabled = true)));
        }

        #[test]
        fn warns_on_a_model_that_cannot_append_tools_and_holds_nothing_once_the_cache_is_cold() {
            let whole_before = settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.tool_lock = Some(lock(&haiku(), |l| {
                    l.tools = names(&SPAWN);
                    l.subagent_messaging = Some(false);
                }));
            });
            let whole = state(&whole_before, &haiku(), SENT);
            let switched_on = with(&whole_before, |s| s.subagent_messaging_enabled = true);
            assert_eq!(subagent_messaging_tone(&whole, &whole_before), CACHE);
            assert!(lock_touch(&whole, &whole_before, &switched_on));
            let cold = state(&whole_before, &haiku(), SENT + 60 * MINUTE);
            assert_eq!(subagent_messaging_tone(&cold, &whole_before), None);
            assert!(!lock_touch(&cold, &whole_before, &switched_on));
        }

        #[test]
        fn puts_it_back_when_the_warm_model_is_picked_again_only_where_agent_spawn_went_out() {
            let moved = with(&off(), |s| s.subagent_messaging_enabled = true);
            assert!(!restore_locked_settings(&moved, &state(&moved, &opus(), SENT)).subagent_messaging_enabled);
            let moved_off = with(&on(), |s| s.subagent_messaging_enabled = false);
            assert!(restore_locked_settings(&moved_off, &state(&moved_off, &opus(), SENT)).subagent_messaging_enabled);
            // Absent is off, so a lock holding off puts back nothing against an absent field.
            let absent = off();
            assert!(restore_moves_nothing(&absent, &state(&absent, &opus(), SENT)));
            // A cold cache holds nothing.
            assert!(restore_moves_nothing(&moved, &state(&moved, &opus(), SENT + 60 * MINUTE)));

            // The last request had no agent_spawn, so the switch was never on the wire: left alone.
            let never = settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.subagent_messaging_enabled = true;
                s.tool_lock = Some(lock(&opus(), |l| l.subagent_messaging = Some(false)));
            });
            assert!(restore_locked_settings(&never, &state(&never, &opus(), SENT)).subagent_messaging_enabled);

            // An older lock knows nothing of it.
            let older = settings(|s| {
                s.enabled_tools = names(&SPAWN);
                s.subagent_messaging_enabled = true;
                s.tool_lock = Some(lock(&opus(), |l| l.tools = names(&SPAWN)));
            });
            let older_state = state(&older, &opus(), SENT);
            assert_eq!(subagent_messaging_tone(&older_state, &older), None);
            assert!(!lock_touch(&older_state, &older, &with(&older, |s| s.subagent_messaging_enabled = false)));
            assert!(restore_moves_nothing(&older, &older_state));
        }
    }

    mod the_plan_mode_switch {
        use super::*;

        /// Its pair is sticky and its guidance is appended, so it touches the cache only when switched on before the pair has
        /// ever gone out, on a model that folds the two tools into the declared list.
        #[test]
        fn is_orange_only_on_a_model_that_cannot_append_tools_before_the_pair_went_out_while_the_cache_is_warm() {
            let never = settings(|s| s.tool_lock = Some(lock(&haiku(), |_| {})));
            assert_eq!(plan_mode_tone(&state(&never, &haiku(), SENT), false), CACHE);
            // Already switched on: moved away, so the cache is already lost for it.
            assert_eq!(plan_mode_tone(&state(&never, &haiku(), SENT), true), None);
            // Cold, another model, or a model that appends tools: nothing to protect.
            assert_eq!(plan_mode_tone(&state(&never, &haiku(), SENT + 90 * MINUTE), false), None);
            assert_eq!(plan_mode_tone(&state(&never, &opus(), SENT), false), None);
            let appended = settings(|s| s.tool_lock = Some(lock(&opus(), |_| {})));
            assert_eq!(plan_mode_tone(&state(&appended, &opus(), SENT), false), None);
            // Once the pair has gone out it stays out, so switching moves no tool.
            let offered = settings(|s| s.tool_lock = Some(lock(&haiku(), |l| l.plan_mode = true)));
            assert_eq!(plan_mode_tone(&state(&offered, &haiku(), SENT), false), None);
        }
    }

    mod a_pinned_backend_under_restore {
        use super::*;

        #[test]
        fn stays_on_its_pin_when_the_warm_model_is_picked_again_whatever_the_last_request_recorded() {
            for model in [opus(), haiku()] {
                let current = settings(|s| {
                    s.web_search_enabled = true;
                    s.tool_lock = Some(lock(&model, |l| {
                        l.web_search = true;
                        l.search_backend = Some(explicit_search(SearchProviderKind::Exa));
                        l.search_provider = Some(SearchProviderSelection::Native);
                    }));
                });
                let restored = restore_locked_settings(&current, &state(&current, &model, SENT));
                assert_eq!(restored.web_search.provider, SearchProviderSelection::Native);
            }
        }
    }
}
