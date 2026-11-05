//! Port of `lib/contextBreakdown.ts`: what the context gauge's menu lists.

use std::collections::BTreeMap;

use super::context_tokens::{estimate_context_tokens, estimate_tokens};
use crate::model::{ContextItem, NativeCompaction, RetainedRole};

/// A row in the breakdown. The id is stable; the view supplies labels and colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextSegmentId {
    SystemPrompt,
    /// A native compaction's item: everything above the card it stands in for.
    Compacted,
    User,
    Assistant,
    Reasoning,
    Tool,
    Other,
}

const SEGMENT_ORDER: [ContextSegmentId; 7] = [
    ContextSegmentId::SystemPrompt,
    ContextSegmentId::Compacted,
    ContextSegmentId::User,
    ContextSegmentId::Assistant,
    ContextSegmentId::Reasoning,
    ContextSegmentId::Tool,
    ContextSegmentId::Other,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextSegment {
    pub id: ContextSegmentId,
    pub tokens: u64,
    /// Fraction of the context window (0..1), or of used tokens when the window is unknown.
    pub share: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextBreakdown {
    /// Used tokens, preferring the provider's authoritative snapshot over local estimation.
    pub used: u64,
    /// Current model context window, or `None` when unknown.
    pub window: Option<u64>,
    pub free: Option<u64>,
    pub free_share: Option<f64>,
    /// `used / window`, capped at 1, or `None` when the window is unknown.
    pub ratio: Option<f64>,
    /// Nonzero component rows in a fixed order.
    pub segments: Vec<ContextSegment>,
}

/// Splits used context into visible components.
///
/// `used` is usually the provider's authoritative input-token count, while individual components
/// can only be estimated with `estimate_context_tokens`. Assign any positive difference to `other`,
/// which represents invisible host prompt content, tool schemas, memory, and skills. When estimates
/// exceed `used`, scale visible components down proportionally so they sum to the authoritative
/// total. `local_only` items estimate to zero because they project to empty content.
///
/// `contexts` is what the request carries: the timeline from the compaction that applies, or the whole of it
/// (`wire_view`). `compaction` is the one `contexts` opens with: its kept messages count as what they are, its item
/// as compacted history.
pub fn compute_context_breakdown(contexts: &[ContextItem], compaction: Option<&NativeCompaction>, used: u64, window: Option<u64>) -> ContextBreakdown {
    let total = used;
    let limit = window.filter(|&w| w > 0);
    let mut raw: BTreeMap<ContextSegmentId, u64> = BTreeMap::new();
    // The view's opening card is weighed below, as the compaction it carries.
    let body = if compaction.is_some() && !contexts.is_empty() { &contexts[1..] } else { contexts };
    for item in body {
        if let ContextItem::System { native_compaction: Some(card), .. } = item {
            // A card the request does not open on sends only the messages it kept.
            for message in &card.retained {
                let row = if message.role == RetainedRole::User { ContextSegmentId::User } else { ContextSegmentId::SystemPrompt };
                *raw.entry(row).or_default() += estimate_tokens(&message.content);
            }
            continue;
        }
        let tokens = estimate_context_tokens(item);
        let row = match item {
            ContextItem::System { .. } => ContextSegmentId::SystemPrompt,
            ContextItem::User { .. } => ContextSegmentId::User,
            ContextItem::Assistant { .. } => ContextSegmentId::Assistant,
            ContextItem::Reasoning { .. } => ContextSegmentId::Reasoning,
            ContextItem::Tool { .. } => ContextSegmentId::Tool,
        };
        *raw.entry(row).or_default() += tokens;
    }
    if let Some(compaction) = compaction {
        let mut kept = 0;
        for message in &compaction.retained {
            let tokens = estimate_tokens(&message.content);
            kept += tokens;
            let row = if message.role == RetainedRole::User { ContextSegmentId::User } else { ContextSegmentId::SystemPrompt };
            *raw.entry(row).or_default() += tokens;
        }
        *raw.entry(ContextSegmentId::Compacted).or_default() += compaction.tokens_after.saturating_sub(kept);
    }
    let accounted: u64 = raw.values().sum();
    let other = total.saturating_sub(accounted);
    let scale = if accounted > total && accounted > 0 { total as f64 / accounted as f64 } else { 1.0 };

    let denominator = limit.unwrap_or(total);
    let segments = SEGMENT_ORDER
        .iter()
        .map(|&id| {
            let tokens = if id == ContextSegmentId::Other { other } else { (raw.get(&id).copied().unwrap_or(0) as f64 * scale).round() as u64 };
            ContextSegment { id, tokens, share: if denominator > 0 { tokens as f64 / denominator as f64 } else { 0.0 } }
        })
        .filter(|segment| segment.tokens > 0)
        .collect();

    let free = limit.map(|limit| limit.saturating_sub(total));
    ContextBreakdown {
        used: total,
        window: limit,
        free,
        free_share: match (limit, free) {
            (Some(limit), Some(free)) => Some(free as f64 / limit as f64),
            _ => None,
        },
        ratio: limit.map(|limit| (total as f64 / limit as f64).min(1.0)),
        segments,
    }
}
