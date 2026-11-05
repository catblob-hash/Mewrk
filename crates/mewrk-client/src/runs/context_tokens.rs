//! Port of `lib/contextTokens.ts`: the shared token estimate, the wire view and the live gauge.

use serde_json::Value;

use super::capabilities::takes_native_compaction;
use super::model_stream::ModelRunState;
use crate::model::{ApiProvider, ContextItem, ImageAttachment, ModelProfile, NativeCompaction};

/// Stable structural JSON form used for open-set supersession comparisons.
pub fn canonical_json(value: &Value) -> String {
    fn canonicalize(item: &Value, out: &mut String) {
        match item {
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    canonicalize(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    canonicalize(&map[key], out);
                }
                out.push('}');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    canonicalize(value, &mut out);
    out
}

/// The same for a tool's arguments.
pub fn canonical_object(value: &crate::model::JsonObject) -> String {
    canonical_json(&Value::Object(value.clone()))
}

/// Approximation shared with Rust: ASCII / 4 plus non-ASCII / 1.6, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for character in text.chars() {
        if (character as u32) <= 0x7f {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    (ascii as f64 / 4.0 + non_ascii as f64 / 1.6).ceil() as u64
}

fn projected_context_tokens(item: &ContextItem) -> u64 {
    match item {
        ContextItem::Tool { tool_name, input, result, .. } => {
            estimate_tokens(&format!("{tool_name}\n{}\n{}", canonical_object(input), result.output))
        }
        ContextItem::Reasoning { content, .. } => estimate_tokens(&format!("[Reasoning]\n{}", content.as_deref().unwrap_or(""))),
        // A native compaction's card sends the messages it kept, wherever it stands;
        // its own text is empty and never sent.
        ContextItem::System { native_compaction: Some(compaction), .. } => {
            estimate_tokens(&compaction.retained.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n"))
        }
        ContextItem::System { local_only: true, .. } => 0,
        ContextItem::System { content, .. } | ContextItem::User { content, .. } | ContextItem::Assistant { content, .. } => estimate_tokens(content),
    }
}

pub fn estimate_images_tokens(images: &[ImageAttachment]) -> u64 {
    images.iter().fold(0, |total, image| {
        if image.width == 0 || image.height == 0 {
            return total + 1024;
        }
        let tiled = 85 + (image.width as u64).div_ceil(512) * (image.height as u64).div_ceil(512) * 170;
        total + tiled.max(1024)
    })
}

fn estimate_context_image_tokens(item: &ContextItem) -> u64 {
    match item {
        ContextItem::User { images, .. } => estimate_images_tokens(images),
        ContextItem::Tool { result, .. } => estimate_images_tokens(&result.images),
        _ => 0,
    }
}

/// Attached files are read by the model as their text, inlined at request time;
/// the host estimated that text's tokens when the file was stored.
fn estimate_context_file_tokens(item: &ContextItem) -> u64 {
    match item {
        ContextItem::User { files, .. } => files.iter().map(|f| f.tokens).sum(),
        _ => 0,
    }
}

pub fn estimate_context_tokens(item: &ContextItem) -> u64 {
    projected_context_tokens(item) + estimate_context_image_tokens(item) + estimate_context_file_tokens(item)
}

/// Whether a card is part of what the model is sent. A host-local record is not,
/// and neither is a reply or reasoning an error or Stop cut off: the fragment
/// stays on the timeline, marked, until an edit makes it an ordinary card.
pub fn is_sent_to_model(item: &ContextItem) -> bool {
    match item {
        ContextItem::System { local_only, native_compaction, .. } => !local_only || native_compaction.is_some(),
        ContextItem::Assistant { interrupted, .. } | ContextItem::Reasoning { interrupted, .. } => !interrupted,
        _ => true,
    }
}

pub fn estimate_contexts_tokens<'a>(contexts: impl IntoIterator<Item = &'a ContextItem>) -> u64 {
    contexts.into_iter().map(|item| if is_sent_to_model(item) { estimate_context_tokens(item) } else { 0 }).sum()
}

/// What of a timeline a request on this model carries: from the native
/// compaction the conversation opens on, when it applies — made through this
/// provider, for a model that compacts natively — or the whole timeline, where
/// the card sends only the messages it kept. Mirrors Rust
/// `native_compaction::wire_start`.
#[derive(Clone, Copy, Debug)]
pub struct WireView<'a> {
    pub contexts: &'a [ContextItem],
    /// The compaction the view opens with: its kept messages and item stand for the compacted conversation.
    pub compaction: Option<&'a NativeCompaction>,
}

pub fn wire_view<'a>(contexts: &'a [ContextItem], target: Option<(&ApiProvider, &ModelProfile)>) -> WireView<'a> {
    if let Some((provider, model)) = target {
        if takes_native_compaction(provider, model) {
            for index in (0..contexts.len()).rev() {
                if let ContextItem::System { native_compaction: Some(compaction), .. } = &contexts[index] {
                    if compaction.provider_id == provider.id {
                        return WireView { contexts: &contexts[index..], compaction: Some(compaction) };
                    }
                }
            }
        }
    }
    WireView { contexts, compaction: None }
}

/// A view's estimate: the compaction it opens with, as the host weighed it, plus what follows.
pub fn estimate_wire_tokens(view: WireView<'_>) -> u64 {
    let following = if view.compaction.is_some() { &view.contexts[1..] } else { view.contexts };
    estimate_contexts_tokens(following) + view.compaction.map_or(0, |c| c.tokens_after)
}

/// Rounds a run has streamed anything for, whether or not usage arrived.
fn streamed_rounds(run: &ModelRunState) -> Vec<usize> {
    let mut rounds: Vec<usize> = run
        .streamed_text_by_round
        .keys()
        .chain(run.streamed_reasoning_by_round.keys())
        .chain(run.streamed_tools_by_round.keys())
        .copied()
        .collect();
    rounds.sort_unstable();
    rounds.dedup();
    rounds
}

/// What one round has put on screen since the last provider snapshot.
fn streamed_round_tokens(run: &ModelRunState, round: usize) -> u64 {
    let text = run.streamed_text_by_round.get(&round).map_or("", String::as_str);
    // Include the `[Reasoning]` prefix for each segment to match settled cards;
    // combining segments once would make the estimate jump at settlement.
    let reasoning = run
        .streamed_reasoning_by_round
        .get(&round)
        .map(|items| items.iter().filter(|item| !item.is_empty()).map(|item| format!("[Reasoning]\n{item}")).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default();
    let tools = run.streamed_tools_by_round.get(&round).map_or(&[][..], Vec::as_slice);
    estimate_tokens(text)
        + estimate_tokens(&reasoning)
        // Same projection the settled tool context uses, so a round's estimate does
        // not change shape the moment it stops being live.
        + tools
            .iter()
            .map(|tool| {
                estimate_tokens(&format!("{}\n{}\n{}", tool.tool_name, canonical_object(&tool.input), tool.result.output))
                    + estimate_images_tokens(&tool.result.images)
            })
            .sum::<u64>()
}

/// What a reported round's calls have returned. The calls are in the round's
/// output, but their results reach the provider only with the next request, so
/// no snapshot has counted them yet.
fn round_result_tokens(run: &ModelRunState, round: usize) -> u64 {
    run.streamed_tools_by_round
        .get(&round)
        .map_or(0, |tools| tools.iter().map(|tool| estimate_tokens(&tool.result.output) + estimate_images_tokens(&tool.result.images)).sum())
}

/// Messages the user steered, and the host delivered, into rounds after
/// `round`, which its snapshot predates.
fn steered_input_tokens(run: &ModelRunState, round: usize) -> u64 {
    let steered: u64 = run.steered_inputs_by_round.range(round + 1..).map(|(_, inputs)| estimate_contexts_tokens(inputs)).sum();
    let host: u64 =
        run.host_contexts_by_round.range(round + 1..).map(|(_, entries)| estimate_contexts_tokens(entries.iter().map(|e| &e.context))).sum();
    steered + host
}

/// `liveContextTokens`' answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveContextTokens {
    pub tokens: u64,
    pub estimated: bool,
}

/// How large the conversation's context is *right now*, mid-turn.
///
/// Providers only report usage at round boundaries — Anthropic at
/// `message_start` and `message_delta`, Chat Completions in its final chunk,
/// Responses at `response.completed` — so a gauge that waits for the turn's
/// final response sits frozen for the entire turn and then jumps. This anchors
/// on the newest snapshot the provider has actually given and adds an estimate
/// of everything that has joined since — what the round's calls returned, what
/// later rounds streamed, what the user steered in — which is the only part
/// that can move between snapshots. The host measures the auto-compact
/// threshold the same way (`api.rs::ContextMeasure`).
///
/// `fallback_tokens` is the caller's estimate of the conversation as persisted,
/// used before this run's first snapshot arrives. Nothing is added on top of it:
/// the host writes streamed prose back as throttled `streaming` rows, so that
/// estimate already covers what has streamed and already climbs on its own —
/// adding the run's buffers as well would bill the same tokens twice.
///
/// `estimated` is true whenever any part of the answer is estimated rather than
/// provider-reported, and drives the `~` the gauge prints.
pub fn live_context_tokens(run: &ModelRunState, fallback_tokens: u64) -> LiveContextTokens {
    let anchor = run.usage_by_round.iter().next_back();
    // `inputTokens` is the context the provider actually read, so it is the only
    // figure worth anchoring on. Cached input is deliberately not added: it is
    // already inside `inputTokens`. A snapshot reporting only output says nothing
    // about how large the context is, and anchoring on it would claim the
    // conversation had shrunk to the size of one response.
    let Some((&anchor_round, anchor_usage)) = anchor else {
        return LiveContextTokens { tokens: fallback_tokens, estimated: true };
    };
    let Some(anchor_input) = anchor_usage.input_tokens else {
        return LiveContextTokens { tokens: fallback_tokens, estimated: true };
    };
    // A round that has reported its output tokens is accounted for in full; one
    // that has only reported its input is still producing, so its own stream is
    // the growth.
    let settled = anchor_usage.output_tokens;
    let growth_from = if settled.is_none() { anchor_round } else { anchor_round + 1 };
    let growth = streamed_rounds(run).into_iter().filter(|&round| round >= growth_from).map(|round| streamed_round_tokens(run, round)).sum::<u64>()
        + if settled.is_none() { 0 } else { round_result_tokens(run, anchor_round) }
        + steered_input_tokens(run, anchor_round);
    LiveContextTokens { tokens: anchor_input + settled.unwrap_or(0) + growth, estimated: growth > 0 }
}

/// JavaScript's `toFixed`, without a trailing `.0`. Both print the exact binary value, but an exact tie rounds up there
/// and to even here; a tie only exists where the value has that many binary digits, so it is told apart exactly.
fn fixed_trimmed(value: f64, digits: usize) -> String {
    let power = 10f64.powi(digits as i32);
    let scaled = value * power;
    let exact_tie = scaled.fract() == 0.5 && (value * f64::from(2u32 << digits)).fract() == 0.0;
    let value = if exact_tie { (scaled + 0.5) / power } else { value };
    let text = format!("{value:.digits$}");
    text.strip_suffix(".0").map_or_else(|| text.clone(), str::to_owned)
}

pub fn format_compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        return format!("{}m", fixed_trimmed(tokens as f64 / 1_000_000.0, if tokens >= 10_000_000 { 0 } else { 1 }));
    }
    if tokens >= 1_000 {
        return format!("{}k", fixed_trimmed(tokens as f64 / 1_000.0, if tokens >= 100_000 { 0 } else { 1 }));
    }
    tokens.to_string()
}
