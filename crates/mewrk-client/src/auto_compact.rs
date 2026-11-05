//! Auto-compact settings (lib/autoCompact.ts).
//!
//! The host owns the behaviour: past the threshold a conversation is armed to hand off (`handoff.rs`), or the provider
//! compacts in place (`native_compaction.rs`). These mirror their ranges and rounding so the menu can say when that happens.

use crate::model::{ApiProvider, AutoCompactSettings, CompactionMethod, ModelProfile, NativeCompactSettings, NativeCompaction};
use crate::model_capabilities::{appends_tools, takes_native_compaction};
use serde_json::Value;

/// The lowest threshold the setting offers.
pub const AUTO_COMPACT_MIN_PERCENT: u32 = 20;
/// The highest threshold the setting offers.
pub const AUTO_COMPACT_MAX_PERCENT: u32 = 97;
const AUTO_COMPACT_DEFAULT_PERCENT: u32 = 80;
/// Native compaction starts at 90%, as Codex compacts (Rust `native_compaction::DEFAULT_THRESHOLD_PERCENT`).
const NATIVE_DEFAULT_PERCENT: u32 = 90;
/// Codex keeps 64,000 tokens of the latest user messages (Rust `DEFAULT_RETAINED_TOKENS`).
pub const NATIVE_RETAINED_DEFAULT_TOKENS: u32 = 64_000;
/// Rust `native_compaction::MAX_RETAINED_TOKENS`.
pub const NATIVE_RETAINED_MAX_TOKENS: u32 = 128_000;

fn default_native_compact_settings() -> NativeCompactSettings {
    NativeCompactSettings { threshold_percent: NATIVE_DEFAULT_PERCENT, retained_tokens: NATIVE_RETAINED_DEFAULT_TOKENS }
}

/// The settings a document without any starts from.
pub fn default_auto_compact_settings() -> AutoCompactSettings {
    AutoCompactSettings { enabled: true, threshold_percent: AUTO_COMPACT_DEFAULT_PERCENT, native: default_native_compact_settings() }
}

/// A whole percent inside the range the setting offers.
pub fn clamp_auto_compact_percent(value: impl Into<f64>) -> u32 {
    let value = value.into();
    if !value.is_finite() {
        return AUTO_COMPACT_DEFAULT_PERCENT;
    }
    value.round().clamp(f64::from(AUTO_COMPACT_MIN_PERCENT), f64::from(AUTO_COMPACT_MAX_PERCENT)) as u32
}

/// A whole token count inside the range the retained budget offers.
pub fn clamp_retained_tokens(value: impl Into<f64>) -> u32 {
    let value = value.into();
    if !value.is_finite() {
        return NATIVE_RETAINED_DEFAULT_TOKENS;
    }
    value.round().clamp(0.0, f64::from(NATIVE_RETAINED_MAX_TOKENS)) as u32
}

fn normalize_native_compact_settings(value: Option<&Value>, fallback: &NativeCompactSettings) -> NativeCompactSettings {
    let number = |key: &str| value.and_then(|value| value.get(key)).and_then(Value::as_f64);
    NativeCompactSettings {
        threshold_percent: number("thresholdPercent").map_or(fallback.threshold_percent, clamp_auto_compact_percent),
        retained_tokens: number("retainedTokens").map_or(fallback.retained_tokens, clamp_retained_tokens),
    }
}

/// The settings a stored value says, each field of the wrong type or absent reading as `fallback`'s, each number clamped.
pub fn normalize_auto_compact_settings(value: &Value, fallback: &AutoCompactSettings) -> AutoCompactSettings {
    AutoCompactSettings {
        enabled: value.get("enabled").and_then(Value::as_bool).unwrap_or(fallback.enabled),
        threshold_percent: value
            .get("thresholdPercent")
            .and_then(Value::as_f64)
            .map_or(fallback.threshold_percent, clamp_auto_compact_percent),
        native: normalize_native_compact_settings(value.get("native"), &fallback.native),
    }
}

/// Tokens at which a conversation is armed to hand off, or compacts: the percent of the window, rounded down.
pub fn auto_compact_threshold_tokens(context_window: u64, percent: impl Into<f64>) -> u64 {
    (u128::from(context_window) * u128::from(clamp_auto_compact_percent(percent)) / 100) as u64
}

/// What a native compaction keeps its user messages within: the setting as set, within what it offers. Mirrors Rust
/// `native_compaction::retained_budget`.
pub fn native_retained_budget(retained_tokens: impl Into<f64>) -> u32 {
    clamp_retained_tokens(retained_tokens)
}

/// The method a new conversation chooses: native where its model compacts natively, the handoff elsewhere.
pub fn default_compaction_method(target: Option<(&ApiProvider, &ModelProfile)>) -> CompactionMethod {
    match target {
        Some((provider, model)) if takes_native_compaction(provider, model) => CompactionMethod::Native,
        _ => CompactionMethod::Handoff,
    }
}

/// The method a conversation auto-compacts by on this model: the one it chose where the model can do it, otherwise the
/// other where the model can do that, and `None` where it can do neither. Native compaction needs the capability; the
/// handoff needs a model that takes its tools mid-conversation. Mirrors Rust `native_compaction::method_in_effect`.
pub fn compaction_method_in_effect(chosen: CompactionMethod, target: Option<(&ApiProvider, &ModelProfile)>) -> Option<CompactionMethod> {
    let (provider, model) = target?;
    let available = |method: &CompactionMethod| match method {
        CompactionMethod::Native => takes_native_compaction(provider, model),
        CompactionMethod::Handoff => appends_tools(provider, model),
    };
    let other = match chosen {
        CompactionMethod::Native => CompactionMethod::Handoff,
        CompactionMethod::Handoff => CompactionMethod::Native,
    };
    [chosen, other].into_iter().find(available)
}

/// What a native compaction's card says, in the timeline and the history record alike: which model compacted, and from how
/// much context to how much. The card's own text is empty; this is read from its fields and never reaches a model.
pub fn compaction_title(compaction: &NativeCompaction) -> String {
    let name = compaction.model_name.trim();
    crate::tf(
        "由 {model} 压缩，{before} token → {after} token",
        "Compacted by {model}, {before} tokens → {after} tokens",
        &[
            ("model", &if name.is_empty() { compaction.model.as_str() } else { name }),
            ("before", &format_compact_token_count(compaction.tokens_before)),
            ("after", &format_compact_token_count(compaction.tokens_after)),
        ],
    )
}

/// lib/contextTokens.ts `formatCompactTokenCount`: `1.2k`, `245k`, `3.5m`.
fn format_compact_token_count(tokens: u64) -> String {
    let scaled = |divisor: f64, digits: usize, unit: char| {
        let fixed = to_fixed(tokens as f64 / divisor, digits);
        format!("{}{unit}", fixed.strip_suffix(".0").unwrap_or(&fixed))
    };
    if tokens >= 1_000_000 {
        return scaled(1_000_000.0, if tokens >= 10_000_000 { 0 } else { 1 }, 'm');
    }
    if tokens >= 1_000 {
        return scaled(1_000.0, if tokens >= 100_000 { 0 } else { 1 }, 'k');
    }
    tokens.to_string()
}

/// `Number.prototype.toFixed`, for values of at least 1. Both round the double's exact value, but a tie goes up in
/// JavaScript and to even in Rust's `{:.N}`, so 1,250 tokens would read `1.2k` here and `1.3k` upstream.
fn to_fixed(value: f64, digits: usize) -> String {
    // 80 places hold the exact expansion of any double of at least 1, which has at most 52 fractional bits.
    let exact = format!("{value:.80}");
    let beyond = exact.split_once('.').map_or("", |(_, fraction)| &fraction[digits..]);
    let tie = beyond.starts_with('5') && beyond[1..].bytes().all(|byte| byte == b'0');
    let value = if tie { value + 0.5 / 10f64.powi(digits as i32) } else { value };
    format!("{value:.digits$}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelCapability, ProviderFamily};
    use serde_json::json;

    fn provider(family: ProviderFamily) -> ApiProvider {
        serde_json::from_value(json!({ "id": "p", "name": "P", "family": family, "baseUrl": "" })).unwrap()
    }

    fn model(capabilities: &[ModelCapability]) -> ModelProfile {
        ModelProfile { capabilities: capabilities.iter().copied().collect(), ..serde_json::from_value(json!({ "id": "m" })).unwrap() }
    }

    fn compact(enabled: bool, handoff_percent: u32, native_percent: u32, retained_tokens: u32) -> AutoCompactSettings {
        AutoCompactSettings {
            enabled,
            threshold_percent: handoff_percent,
            native: NativeCompactSettings { threshold_percent: native_percent, retained_tokens },
        }
    }

    #[test]
    fn starts_on_handing_off_at_80_and_compacting_natively_at_90_keeping_codexs_64k() {
        assert_eq!(default_auto_compact_settings(), compact(true, 80, 90, 64_000));
    }

    #[test]
    fn reads_a_document_from_before_native_compaction_with_its_defaults() {
        let normalized = normalize_auto_compact_settings(&json!({ "enabled": false, "thresholdPercent": 64 }), &default_auto_compact_settings());
        assert_eq!(normalized, compact(false, 64, 90, 64_000));
    }

    #[test]
    fn clamps_each_threshold_and_the_budget_to_the_ranges_offered() {
        let normalized = normalize_auto_compact_settings(
            &json!({ "enabled": true, "thresholdPercent": 5, "native": { "thresholdPercent": 120, "retainedTokens": 1_000_000 } }),
            &default_auto_compact_settings(),
        );
        assert_eq!(normalized, compact(true, 20, 97, 128_000));
        let negative = normalize_auto_compact_settings(&json!({ "native": { "retainedTokens": -5 } }), &default_auto_compact_settings());
        assert_eq!(negative.native.retained_tokens, 0);
    }

    #[test]
    fn keeps_the_retained_budget_the_user_set_whatever_the_threshold() {
        assert_eq!(auto_compact_threshold_tokens(272_000, 90), 244_800);
        assert_eq!(native_retained_budget(64_000), 64_000);
        assert_eq!(native_retained_budget(128_000), 128_000);
        assert_eq!(native_retained_budget(500_000), 128_000);
        assert_eq!(native_retained_budget(-5), 0);
    }

    /// Not an upstream case: the wrong-type and rounding edges of the normalizer.
    #[test]
    fn reads_values_of_the_wrong_type_as_the_fallback_and_rounds_like_upstream() {
        let fallback = compact(false, 60, 70, 1_000);
        assert_eq!(
            normalize_auto_compact_settings(&json!({ "enabled": "yes", "thresholdPercent": "64", "native": 3 }), &fallback),
            fallback
        );
        assert_eq!(normalize_auto_compact_settings(&json!(null), &fallback), fallback);
        assert_eq!(clamp_auto_compact_percent(64.5), 65);
        assert_eq!(clamp_auto_compact_percent(f64::NAN), 80);
        assert_eq!(clamp_retained_tokens(f64::INFINITY), 64_000);
        // Rounded down to tokens, not to the nearest.
        assert_eq!(auto_compact_threshold_tokens(1_001, 90), 900);
    }

    #[test]
    fn is_native_for_a_new_conversation_on_a_model_that_compacts_natively_the_handoff_elsewhere() {
        use ModelCapability::{NativeCompaction as Native, ToolAppend};
        let codex = provider(ProviderFamily::OpenaiCodex);
        assert_eq!(default_compaction_method(Some((&codex, &model(&[ToolAppend, Native])))), CompactionMethod::Native);
        assert_eq!(default_compaction_method(Some((&codex, &model(&[ToolAppend])))), CompactionMethod::Handoff);
        assert_eq!(
            default_compaction_method(Some((&provider(ProviderFamily::Anthropic), &model(&[Native])))),
            CompactionMethod::Handoff
        );
        assert_eq!(default_compaction_method(None), CompactionMethod::Handoff);
    }

    #[test]
    fn is_the_conversations_choice_where_the_model_can_do_it_the_other_where_only_that_one_works() {
        use CompactionMethod::{Handoff, Native};
        use ModelCapability::{NativeCompaction, ToolAppend};
        let codex = provider(ProviderFamily::OpenaiCodex);
        let both = model(&[ToolAppend, NativeCompaction]);
        assert_eq!(compaction_method_in_effect(Native, Some((&codex, &both))), Some(Native));
        assert_eq!(compaction_method_in_effect(Handoff, Some((&codex, &both))), Some(Handoff));
        assert_eq!(compaction_method_in_effect(Native, Some((&codex, &model(&[ToolAppend])))), Some(Handoff));
        assert_eq!(compaction_method_in_effect(Handoff, Some((&codex, &model(&[NativeCompaction])))), Some(Native));
        assert_eq!(compaction_method_in_effect(Native, Some((&codex, &model(&[])))), None);
        assert_eq!(compaction_method_in_effect(Handoff, None), None);
    }

    /// Not an upstream case: `compactionTitle` has none of its own.
    #[test]
    fn titles_a_compaction_by_its_model_and_its_token_counts() {
        let card = |model_name: &str| -> NativeCompaction {
            serde_json::from_value(json!({
                "providerId": "p", "model": "gpt-5.5", "modelName": model_name, "parts": [],
                "tokensBefore": 244_800, "tokensAfter": 1_250
            }))
            .unwrap()
        };
        // Either language says the same three things; the language itself is a process-wide flag other tests may move.
        let named = compaction_title(&card("  GPT Five  "));
        assert!(named.contains("GPT Five") && !named.contains("  GPT") && named.contains("245k") && named.contains("1.3k"), "{named}");
        assert!(compaction_title(&card("   ")).contains("gpt-5.5"));
    }

    /// Not an upstream case: the expected values are what the TypeScript function returns under node.
    #[test]
    fn formats_token_counts_as_to_fixed_does_ties_going_up() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_250, "1.3k"),
            (1_150, "1.1k"),
            (99_949, "99.9k"),
            (100_000, "100k"),
            (100_500, "101k"),
            (1_000_000, "1m"),
            (1_250_000, "1.3m"),
            (10_500_000, "11m"),
        ] {
            assert_eq!(format_compact_token_count(tokens), expected, "{tokens}");
        }
    }
}
