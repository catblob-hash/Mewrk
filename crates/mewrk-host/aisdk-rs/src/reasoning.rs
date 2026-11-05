//! Reasoning levels per family and model (`reasoning.ts`).
//!
//! The host sends one of five levels and never names a model. Each upstream takes a different subset, and
//! the SDK's shared option has no `max`, drops what it does not know, and raises some levels. So: send the
//! highest level the model takes that is not above the one asked for, the lowest it takes when all of them
//! are above, and never `none` or `minimal`, since there is no "off".

use regex::Regex;
use std::sync::OnceLock;

use crate::jobj;
use crate::json::{Json, Obj};

const LADDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const UP_TO_HIGH: [&str; 3] = ["low", "medium", "high"];
const UP_TO_XHIGH: [&str; 4] = ["low", "medium", "high", "xhigh"];

#[derive(Default, Debug, PartialEq)]
pub struct ReasoningPlan {
    /// The SDK's `reasoning` option, when the provider's own mapping is right for the model.
    pub reasoning: Option<String>,
    /// Provider options to merge over the request's own.
    pub provider_options: Option<Json>,
    /// The visible-output ceiling, when thinking is carved out of the request's (Bedrock budget models).
    pub max_output_tokens: Option<u64>,
}

fn rank(level: &str) -> Option<usize> {
    LADDER.iter().position(|l| *l == level)
}

fn clamp_level<'a>(level: &'a str, supported: &[&'a str]) -> &'a str {
    // A level off the ladder ranks below all of it, so it gets the lowest the model takes.
    let asked = rank(level);
    let mut best: Option<&str> = None;
    for candidate in supported {
        let Some(at) = rank(candidate) else { continue };
        if asked.is_some_and(|asked| at <= asked) && best.is_none_or(|b| at > rank(b).unwrap_or(0)) {
            best = Some(candidate);
        }
    }
    if let Some(best) = best {
        return best;
    }
    supported.iter().copied().min_by_key(|c| rank(c).unwrap_or(usize::MAX)).unwrap_or(level)
}

fn shared(level: &str) -> Option<String> {
    (level != "max").then(|| level.to_owned())
}

#[derive(PartialEq, Clone, Copy)]
enum ClaudeThinking {
    Adaptive,
    AdaptiveWithoutXhigh,
    Budget,
}

fn re(cell: &'static OnceLock<Regex>, source: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(source).expect("a literal pattern"))
}

fn claude_thinking(model_id: &str) -> ClaudeThinking {
    static ADAPTIVE: OnceLock<Regex> = OnceLock::new();
    static SIX: OnceLock<Regex> = OnceLock::new();
    static BUDGET: OnceLock<Regex> = OnceLock::new();
    let id = model_id.to_ascii_lowercase();
    if re(&ADAPTIVE, r"claude-(?:opus-5|opus-4-[78]|fable-5|sonnet-5)").is_match(&id) {
        return ClaudeThinking::Adaptive;
    }
    if re(&SIX, r"claude-(?:opus|sonnet)-4-6").is_match(&id) {
        return ClaudeThinking::AdaptiveWithoutXhigh;
    }
    if re(&BUDGET, r"claude-(?:(?:sonnet|opus|haiku)-4-5|opus-4-1|sonnet-4-|opus-4-|3-|instant|v?2(?:$|[-.:]))").is_match(&id) {
        return ClaudeThinking::Budget;
    }
    if id.contains("claude-") { ClaudeThinking::Adaptive } else { ClaudeThinking::Budget }
}

fn claude_levels(thinking: ClaudeThinking) -> Vec<&'static str> {
    if thinking == ClaudeThinking::AdaptiveWithoutXhigh { vec!["low", "medium", "high", "max"] } else { LADDER.to_vec() }
}

fn adaptive_thinking() -> Json {
    jobj! { "type": "adaptive", "display": "summarized" }
}

fn options(key: &str, value: Json) -> Option<Json> {
    let mut out = Obj::new();
    out.insert(key, value);
    Some(Json::Obj(out))
}

fn anthropic_plan(model_id: &str, level: &str) -> ReasoningPlan {
    let thinking = claude_thinking(model_id);
    // Budget models have no effort to name: `max` is the largest budget the SDK's share table has.
    if thinking == ClaudeThinking::Budget {
        return ReasoningPlan { reasoning: Some(shared(level).unwrap_or_else(|| "xhigh".to_owned())), ..Default::default() };
    }
    let levels = claude_levels(thinking);
    let effort = clamp_level(level, &levels);
    // `max` has no shared name, and naming the effort in provider options skips the SDK's mapping, so
    // adaptive thinking has to be named with it. `xhigh` on a 4.6 model never reaches the SDK: it would
    // send `max`.
    if effort == "max" {
        return ReasoningPlan {
            provider_options: options("anthropic", jobj! { "effort": effort, "thinking": adaptive_thinking() }),
            ..Default::default()
        };
    }
    ReasoningPlan { reasoning: Some(effort.to_owned()), ..Default::default() }
}

/// `^gpt-(\d+)(?:\.(\d+))?`.
fn gpt_version(id: &str, prefix: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let major: u32 = digits.parse().ok()?;
    let rest = &rest[digits.len()..];
    let minor = rest
        .strip_prefix('.')
        .map(|r| r.chars().take_while(char::is_ascii_digit).collect::<String>())
        .filter(|d| !d.is_empty())
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    Some((major, minor))
}

fn openai_levels(model_id: &str) -> Vec<&'static str> {
    let id = model_id.to_ascii_lowercase();
    let Some((major, minor)) = gpt_version(&id, "gpt-") else { return UP_TO_HIGH.to_vec() };
    if major >= 6 {
        return LADDER.to_vec();
    }
    if major < 5 {
        return UP_TO_HIGH.to_vec();
    }
    if id.starts_with("gpt-5-pro") {
        return vec!["high"];
    }
    if minor >= 6 {
        return LADDER.to_vec();
    }
    if minor >= 2 || id.contains("codex-max") {
        return UP_TO_XHIGH.to_vec();
    }
    UP_TO_HIGH.to_vec()
}

fn responses_plan(options_key: &str, model_id: &str, level: &str) -> ReasoningPlan {
    let levels = openai_levels(model_id);
    let effort = clamp_level(level, &levels);
    if effort == "max" {
        return ReasoningPlan {
            provider_options: options(options_key, jobj! { "reasoningEffort": effort }),
            ..Default::default()
        };
    }
    ReasoningPlan { reasoning: Some(effort.to_owned()), ..Default::default() }
}

fn chat_plan(model_id: &str, level: &str) -> ReasoningPlan {
    let lowered = model_id.to_ascii_lowercase();
    let id = lowered.strip_prefix("openai/").unwrap_or(&lowered);
    // Chat models from OpenAI that take no `reasoning_effort` at all: any value is a `400`.
    if id.starts_with("gpt-3") || id.starts_with("gpt-4") || id.starts_with("chatgpt-") {
        return ReasoningPlan::default();
    }
    let openai = id.starts_with("gpt-") || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()));
    let effort = if openai {
        let levels: Vec<&str> = openai_levels(id).into_iter().filter(|c| *c != "max").collect();
        clamp_level(level, &levels)
    } else {
        level
    };
    ReasoningPlan {
        provider_options: options("openaiCompatible", jobj! { "reasoningEffort": effort }),
        ..Default::default()
    }
}

fn google_plan(model_id: &str, level: &str) -> ReasoningPlan {
    let levels: &[&str] = if model_id.to_ascii_lowercase().contains("gemini-3-pro-preview") { &["low", "high"] } else { &UP_TO_HIGH };
    ReasoningPlan { reasoning: shared(clamp_level(level, levels)), ..Default::default() }
}

fn xai_plan(model_id: &str, level: &str) -> ReasoningPlan {
    static PAIR: OnceLock<Regex> = OnceLock::new();
    let id = model_id.to_ascii_lowercase();
    if id.starts_with("grok-3-mini") {
        return ReasoningPlan { reasoning: shared(clamp_level(level, &["low", "high"])), ..Default::default() };
    }
    if re(&PAIR, r"^grok-4\.20(?:-\d{4})?-(?:non-)?reasoning$").is_match(&id) {
        return ReasoningPlan::default();
    }
    let current = gpt_version(&id, "grok-").is_some_and(|(major, minor)| major > 4 || (major == 4 && minor >= 3));
    if !current {
        return ReasoningPlan::default();
    }
    ReasoningPlan {
        provider_options: options("xai", jobj! { "reasoningEffort": clamp_level(level, &UP_TO_XHIGH) }),
        ..Default::default()
    }
}

fn budget_share(level: &str) -> f64 {
    match level {
        "low" => 0.1,
        "medium" => 0.3,
        "high" => 0.6,
        _ => 0.9,
    }
}

const MIN_THINKING_BUDGET: u64 = 1024;

fn bedrock_plan(model_id: &str, level: &str, max_output_tokens: Option<u64>) -> ReasoningPlan {
    let id = model_id.to_ascii_lowercase();
    if id.contains("anthropic") && id.contains("claude") {
        let thinking = claude_thinking(&id[id.find("claude").unwrap_or(0)..]);
        if thinking != ClaudeThinking::Budget {
            let levels = claude_levels(thinking);
            let effort = clamp_level(level, &levels);
            return ReasoningPlan {
                provider_options: options("bedrock", jobj! {
                    "reasoningConfig": jobj! { "type": "adaptive", "display": "summarized", "maxReasoningEffort": effort }
                }),
                ..Default::default()
            };
        }
        let Some(max_output) = max_output_tokens else {
            return ReasoningPlan { reasoning: Some(shared(level).unwrap_or_else(|| "xhigh".to_owned())), ..Default::default() };
        };
        let budget = MIN_THINKING_BUDGET.max(js_round(max_output as f64 * budget_share(level)));
        return ReasoningPlan {
            provider_options: options("bedrock", jobj! {
                "reasoningConfig": jobj! { "type": "enabled", "budgetTokens": budget }
            }),
            max_output_tokens: Some(1.max(max_output as i64 - budget as i64) as u64),
            ..Default::default()
        };
    }
    if id.contains("nova-2") {
        return ReasoningPlan { reasoning: shared(clamp_level(level, &["low", "medium"])), ..Default::default() };
    }
    if id.contains("gpt-oss") {
        return ReasoningPlan { reasoning: shared(clamp_level(level, &UP_TO_HIGH)), ..Default::default() };
    }
    ReasoningPlan::default()
}

/// `Math.round`, half up.
pub(crate) fn js_round(value: f64) -> u64 {
    (value + 0.5).floor() as u64
}

/// How a step's `level` reaches `family`'s model `model_id`.
pub fn plan_reasoning(family: &str, model_id: &str, level: Option<&str>, max_output_tokens: Option<u64>) -> ReasoningPlan {
    let Some(level) = level else { return ReasoningPlan::default() };
    match family {
        "anthropic" => anthropic_plan(model_id, level),
        "openai-responses" | "openai-codex" => responses_plan("openai", model_id, level),
        "azure" => responses_plan("azure", model_id, level),
        "openai-chat" | "openai-compatible" => chat_plan(model_id, level),
        "google" | "vertex" => google_plan(model_id, level),
        "xai" => xai_plan(model_id, level),
        "bedrock" => bedrock_plan(model_id, level, max_output_tokens),
        _ => ReasoningPlan::default(),
    }
}

/// `base` with `patch` merged in, one level deep per provider key.
pub fn merge_provider_options(base: Option<&Json>, patch: Option<&Json>) -> Option<Json> {
    let Some(patch) = patch.and_then(Json::as_obj) else { return base.cloned() };
    let mut merged = base.and_then(Json::as_obj).cloned().unwrap_or_default();
    for (key, options) in patch.iter() {
        match (merged.get_mut(key), options.as_obj()) {
            (Some(Json::Obj(existing)), Some(incoming)) => existing.extend_from(incoming),
            _ => merged.insert(key, options.clone()),
        }
    }
    Some(Json::Obj(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasoning(plan: ReasoningPlan) -> Option<String> {
        plan.reasoning
    }

    #[test]
    fn anthropic_levels_follow_the_model() {
        assert_eq!(reasoning(plan_reasoning("anthropic", "claude-opus-5", Some("xhigh"), None)).as_deref(), Some("xhigh"));
        // 4.6 has no xhigh: the SDK would raise it to max, so it steps down.
        assert_eq!(reasoning(plan_reasoning("anthropic", "claude-sonnet-4-6", Some("xhigh"), None)).as_deref(), Some("high"));
        let max = plan_reasoning("anthropic", "claude-opus-5", Some("max"), None);
        assert_eq!(max.provider_options.unwrap().to_text(), r#"{"anthropic":{"effort":"max","thinking":{"type":"adaptive","display":"summarized"}}}"#);
        // A budget model has nothing above xhigh.
        assert_eq!(reasoning(plan_reasoning("anthropic", "claude-sonnet-4-5", Some("max"), None)).as_deref(), Some("xhigh"));
        assert_eq!(reasoning(plan_reasoning("anthropic", "deepseek-reasoner", Some("low"), None)).as_deref(), Some("low"));
    }

    #[test]
    fn openai_ladder() {
        assert_eq!(reasoning(plan_reasoning("openai-responses", "gpt-5.1", Some("xhigh"), None)).as_deref(), Some("high"));
        assert_eq!(reasoning(plan_reasoning("openai-responses", "gpt-5.2", Some("xhigh"), None)).as_deref(), Some("xhigh"));
        assert_eq!(reasoning(plan_reasoning("openai-responses", "gpt-5-pro", Some("low"), None)).as_deref(), Some("high"));
        let max = plan_reasoning("azure", "gpt-6", Some("max"), None);
        assert_eq!(max.provider_options.unwrap().to_text(), r#"{"azure":{"reasoningEffort":"max"}}"#);
        assert_eq!(reasoning(plan_reasoning("openai-codex", "o3", Some("max"), None)).as_deref(), Some("high"));
    }

    #[test]
    fn chat_families() {
        assert_eq!(plan_reasoning("openai-chat", "gpt-4o", Some("high"), None), ReasoningPlan::default());
        let deepseek = plan_reasoning("openai-compatible", "deepseek-r1", Some("max"), None);
        assert_eq!(deepseek.provider_options.unwrap().to_text(), r#"{"openaiCompatible":{"reasoningEffort":"max"}}"#);
        let openai = plan_reasoning("openai-chat", "openai/gpt-5.6", Some("max"), None);
        assert_eq!(openai.provider_options.unwrap().to_text(), r#"{"openaiCompatible":{"reasoningEffort":"xhigh"}}"#);
    }

    #[test]
    fn google_xai_bedrock() {
        assert_eq!(reasoning(plan_reasoning("google", "gemini-3-pro-preview", Some("medium"), None)).as_deref(), Some("low"));
        assert_eq!(reasoning(plan_reasoning("vertex", "gemini-2.5-pro", Some("max"), None)).as_deref(), Some("high"));
        assert_eq!(plan_reasoning("xai", "grok-4.20-reasoning", Some("high"), None), ReasoningPlan::default());
        assert_eq!(plan_reasoning("xai", "grok-2", Some("high"), None), ReasoningPlan::default());
        assert_eq!(plan_reasoning("xai", "grok-4.3", Some("max"), None).provider_options.unwrap().to_text(), r#"{"xai":{"reasoningEffort":"xhigh"}}"#);
        let budget = plan_reasoning("bedrock", "anthropic.claude-sonnet-4-5", Some("medium"), Some(32000));
        assert_eq!(budget.provider_options.unwrap().to_text(), r#"{"bedrock":{"reasoningConfig":{"type":"enabled","budgetTokens":9600}}}"#);
        assert_eq!(budget.max_output_tokens, Some(22400));
        let adaptive = plan_reasoning("bedrock", "us.anthropic.claude-opus-5", Some("max"), Some(32000));
        assert_eq!(adaptive.provider_options.unwrap().to_text(), r#"{"bedrock":{"reasoningConfig":{"type":"adaptive","display":"summarized","maxReasoningEffort":"max"}}}"#);
        assert_eq!(reasoning(plan_reasoning("bedrock", "amazon.nova-2-lite", Some("high"), Some(1))).as_deref(), Some("medium"));
        assert_eq!(plan_reasoning("bedrock", "meta.llama3", Some("high"), Some(1)), ReasoningPlan::default());
    }

    #[test]
    fn provider_options_merge_one_level() {
        let base = Json::parse(r#"{"anthropic":{"a":1,"effort":"low"},"other":{"x":1}}"#).unwrap();
        let patch = Json::parse(r#"{"anthropic":{"effort":"max"}}"#).unwrap();
        assert_eq!(merge_provider_options(Some(&base), Some(&patch)).unwrap().to_text(), r#"{"anthropic":{"a":1,"effort":"max"},"other":{"x":1}}"#);
    }
}
