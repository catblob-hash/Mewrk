//! Port of `lib/contextTokens.test.ts` and `lib/contextBreakdown.test.ts`.

use serde_json::json;

use super::fixtures::*;
use crate::model::{ApiProvider, ContextItem, ModelProfile, ModelUsage};
use crate::runs::context_breakdown::{compute_context_breakdown, ContextSegmentId};
use crate::runs::context_tokens::*;
use crate::runs::model_stream::{HostContextState, ModelRunState, StreamingToolPhase, StreamingToolState};

/// 40 ASCII characters, which the shared estimator prices at exactly 10.
fn forty() -> String {
    "a".repeat(40)
}

fn usage(input: Option<u64>, output: Option<u64>) -> ModelUsage {
    ModelUsage { input_tokens: input, output_tokens: output, ..Default::default() }
}

fn live(tokens: u64, estimated: bool) -> LiveContextTokens {
    LiveContextTokens { tokens, estimated }
}

#[test]
fn leans_on_the_callers_estimate_alone_until_the_first_provider_snapshot() {
    // The host writes streamed prose back as throttled `streaming` rows, so the
    // caller's estimate of the conversation already covers what has streamed and
    // already climbs. Adding the run's own buffers on top would bill the same tokens twice.
    let mut run = run_fixture();
    run.streamed_text_by_round.insert(0, forty());
    assert_eq!(live_context_tokens(&run, 1_000), live(1_000, true));
}

#[test]
fn anchors_on_the_rounds_reported_input_and_adds_what_has_streamed_since() {
    let mut run = run_fixture();
    run.usage_by_round.insert(2, ModelUsage { input_tokens: Some(5_000), cached_input_tokens: Some(4_000), ..Default::default() });
    run.streamed_text_by_round.insert(2, forty());
    assert_eq!(live_context_tokens(&run, 1), live(5_010, true));
}

#[test]
fn stops_estimating_a_round_the_provider_has_finished_reporting() {
    let mut run = run_fixture();
    run.usage_by_round.insert(2, usage(Some(5_000), Some(300)));
    run.streamed_text_by_round.insert(2, forty());
    assert_eq!(live_context_tokens(&run, 1), live(5_300, false));
}

#[test]
fn counts_a_later_rounds_stream_on_top_of_the_last_closed_round() {
    let mut run = run_fixture();
    run.usage_by_round.insert(2, usage(Some(5_000), Some(300)));
    run.streamed_text_by_round.insert(2, "已计入的旧内容".into());
    run.streamed_text_by_round.insert(3, forty());
    assert_eq!(live_context_tokens(&run, 1), live(5_310, true));
}

fn read_tool() -> StreamingToolState {
    StreamingToolState {
        id: "t1".into(),
        call_id: "call-1".into(),
        tool_name: "read".into(),
        requested_input: None,
        input: json!({ "path": "a.ts" }).as_object().unwrap().clone(),
        result: serde_json::from_value(tool_result(true, &forty())).unwrap(),
        stream_status: StreamingToolPhase::Completed,
        live: Default::default(),
        created_at: String::new(),
    }
}

#[test]
fn prices_a_rounds_tool_calls_the_same_way_the_settled_context_will() {
    let mut run = run_fixture();
    run.usage_by_round.insert(0, usage(Some(4_000), None));
    run.streamed_tools_by_round.insert(0, vec![read_tool()]);
    assert_eq!(live_context_tokens(&run, 0), live(4_000 + estimate_tokens(&format!("read\n{{\"path\":\"a.ts\"}}\n{}", forty())), true));
}

#[test]
fn counts_what_a_reported_rounds_calls_returned_before_the_next_round_reports() {
    let mut run = run_fixture();
    run.usage_by_round.insert(1, usage(Some(5_000), Some(300)));
    run.streamed_tools_by_round.insert(1, vec![read_tool()]);
    assert_eq!(live_context_tokens(&run, 1), live(5_310, true));
}

#[test]
fn counts_a_message_steered_in_after_the_snapshot_not_one_it_already_read() {
    let mut run = run_fixture();
    run.usage_by_round.insert(2, usage(Some(5_000), Some(300)));
    run.steered_inputs_by_round.insert(2, vec![user("read", &forty())]);
    run.steered_inputs_by_round.insert(3, vec![user("new", &forty())]);
    assert_eq!(live_context_tokens(&run, 1), live(5_310, true));
}

#[test]
fn counts_what_the_host_delivered_after_the_snapshot_not_what_it_already_read() {
    let mut run: ModelRunState = run_fixture();
    run.usage_by_round.insert(2, usage(Some(5_000), Some(300)));
    run.host_contexts_by_round.insert(2, vec![HostContextState { context: user("read", &forty()), after_steered: 0 }]);
    run.host_contexts_by_round.insert(3, vec![HostContextState { context: user("new", &forty()), after_steered: 0 }]);
    assert_eq!(live_context_tokens(&run, 1), live(5_310, true));
}

#[test]
fn declines_to_anchor_on_a_snapshot_with_no_input_count() {
    let mut run = run_fixture();
    run.usage_by_round.insert(0, usage(None, Some(300)));
    run.streamed_text_by_round.insert(0, forty());
    assert_eq!(live_context_tokens(&run, 2_000), live(2_000, true));
}

#[test]
fn leaves_out_what_is_never_sent_a_cut_off_fragment_and_a_host_local_record() {
    let sent = assistant("a", &"x".repeat(400));
    let total = estimate_contexts_tokens(std::slice::from_ref(&sent));
    assert!(total > 0);
    let at = "2026-10-03T00:00:00Z";
    assert_eq!(
        estimate_contexts_tokens(&[
            sent,
            context(json!({ "id": "cut", "kind": "assistant", "content": "x".repeat(400), "interrupted": true, "createdAt": at })),
            context(json!({ "id": "r", "kind": "reasoning", "content": "y".repeat(400), "interrupted": true, "createdAt": at })),
            context(json!({ "id": "s", "kind": "system", "content": "z".repeat(400), "localOnly": true, "createdAt": at })),
        ]),
        total
    );
}

fn card(id: &str, provider_id: &str) -> ContextItem {
    context(json!({
        "id": id, "kind": "system", "content": "Context compacted natively.", "localOnly": true,
        "nativeCompaction": { "providerId": provider_id, "model": "gpt-6-astra", "parts": [], "retained": [{ "role": "user", "sourceId": "u1", "content": "kept" }], "tokensBefore": 200000, "tokensAfter": 3000 },
        "createdAt": "2026-10-05T00:00:00Z"
    }))
}

fn timeline() -> Vec<ContextItem> {
    vec![user("u1", &"a".repeat(4000)), card("c1", "codex"), user("u2", &"b".repeat(40)), card("c2", "other"), user("u3", &"c".repeat(40))]
}

fn target(id: &str, family: &str, capabilities: &[&str]) -> (ApiProvider, ModelProfile) {
    let mut provider = provider(family);
    provider.id = id.into();
    (provider, model(capabilities, "plaintext"))
}

#[test]
fn starts_at_the_latest_compaction_its_provider_made_on_a_model_that_compacts_natively() {
    let contexts = timeline();
    let (provider, model) = target("codex", "openai_codex", &["native_compaction"]);
    let view = wire_view(&contexts, Some((&provider, &model)));
    assert_eq!(view.contexts.iter().map(ContextItem::id).collect::<Vec<_>>(), ["c1", "u2", "c2", "u3"]);
    assert_eq!(view.compaction.map(|compaction| compaction.provider_id.as_str()), Some("codex"));
    // The card weighs what the host weighed it; the other provider's card the message it kept ("kept", one token).
    assert_eq!(estimate_wire_tokens(view), 3_000 + 10 + 1 + 10);
}

#[test]
fn is_the_whole_timeline_anywhere_else() {
    let contexts = timeline();
    let targets = [target("codex", "openai_codex", &[]), target("codex", "anthropic", &["native_compaction"]), target("third", "openai_codex", &["native_compaction"])];
    let mut cases: Vec<Option<(&ApiProvider, &ModelProfile)>> = targets.iter().map(|(provider, model)| Some((provider, model))).collect();
    cases.push(None);
    for case in cases {
        let view = wire_view(&contexts, case);
        assert_eq!(view.contexts.len(), contexts.len());
        assert!(view.compaction.is_none());
        assert_eq!(estimate_wire_tokens(view), estimate_contexts_tokens(&contexts));
    }
}

#[test]
fn prints_compact_counts_like_the_gauge() {
    assert_eq!(format_compact_token_count(999), "999");
    assert_eq!(format_compact_token_count(1_000), "1k");
    assert_eq!(format_compact_token_count(1_250), "1.3k");
    assert_eq!(format_compact_token_count(123_456), "123k");
    assert_eq!(format_compact_token_count(1_000_000), "1m");
    assert_eq!(format_compact_token_count(1_500_000), "1.5m");
    assert_eq!(format_compact_token_count(12_000_000), "12m");
}

mod breakdown {
    use super::*;

    fn text(kind: &str, content: &str) -> ContextItem {
        context(json!({ "id": format!("{kind}-{}", content.len()), "kind": kind, "content": content, "createdAt": AT }))
    }

    fn contexts() -> Vec<ContextItem> {
        vec![
            text("system", "prompt"),
            text("user", &"u".repeat(400)),
            text("assistant", &"a".repeat(800)),
            text("reasoning", &"r".repeat(200)),
            context(json!({ "id": "tool", "kind": "tool", "toolName": "read_file", "input": {}, "result": tool_result(true, &"t".repeat(1200)), "createdAt": AT })),
        ]
    }

    #[test]
    fn charges_the_gap_between_the_provider_total_and_the_local_estimate_to_other() {
        let contexts = contexts();
        let accounted = estimate_contexts_tokens(&contexts);
        let breakdown = compute_context_breakdown(&contexts, None, accounted + 5000, Some(100_000));
        let other = breakdown.segments.iter().find(|segment| segment.id == ContextSegmentId::Other).unwrap();
        assert_eq!(other.tokens, 5000);
        // Preserve visible rows unchanged when the authoritative total exceeds the estimate.
        let visible: u64 = breakdown.segments.iter().filter(|segment| segment.id != ContextSegmentId::Other).map(|segment| segment.tokens).sum();
        assert_eq!(visible, accounted);
        assert_eq!(breakdown.free, Some(100_000 - (accounted + 5000)));
    }

    #[test]
    fn scales_the_estimate_back_down_when_it_overshoots_the_authoritative_total() {
        let contexts = contexts();
        let accounted = estimate_contexts_tokens(&contexts);
        let used = accounted / 2;
        let breakdown = compute_context_breakdown(&contexts, None, used, Some(100_000));
        assert!(!breakdown.segments.iter().any(|segment| segment.id == ContextSegmentId::Other));
        let total: u64 = breakdown.segments.iter().map(|segment| segment.tokens).sum();
        // Per-row rounding can introduce a few tokens of error, but rows must never exceed the header total.
        assert!(total <= used + breakdown.segments.len() as u64 / 2);
        assert!(total + breakdown.segments.len() as u64 > used);
    }

    #[test]
    fn folds_timeline_system_contexts_into_the_system_prompt_row() {
        let (prompt, notice) = (text("system", "prompt"), context(json!({ "id": "n", "kind": "system", "content": "n".repeat(120), "createdAt": AT })));
        let row = |contexts: &[ContextItem]| compute_context_breakdown(contexts, None, 10_000, Some(100_000)).segments.iter().find(|segment| segment.id == ContextSegmentId::SystemPrompt).map_or(0, |segment| segment.tokens);
        assert_eq!(row(&[prompt.clone(), notice]), row(&[prompt]) + estimate_tokens(&"n".repeat(120)));
    }

    #[test]
    fn drops_empty_rows_and_shares_the_window_as_the_denominator() {
        let breakdown = compute_context_breakdown(&[text("user", &"u".repeat(400))], None, 1000, Some(10_000));
        assert_eq!(breakdown.segments.iter().map(|segment| segment.id).collect::<Vec<_>>(), [ContextSegmentId::User, ContextSegmentId::Other]);
        assert!((breakdown.ratio.unwrap() - 0.1).abs() < 1e-6);
        assert!((breakdown.free_share.unwrap() - 0.9).abs() < 1e-6);
        let user = breakdown.segments[0];
        assert!((user.share - user.tokens as f64 / 10_000.0).abs() < 1e-6);
    }

    #[test]
    fn reports_no_window_when_the_model_does_not_declare_one() {
        let breakdown = compute_context_breakdown(&contexts(), None, 5000, None);
        assert_eq!((breakdown.window, breakdown.free, breakdown.free_share, breakdown.ratio), (None, None, None, None));
        // Without a window, shares use used tokens, so component rows still sum to one.
        let shares: f64 = breakdown.segments.iter().map(|segment| segment.share).sum();
        assert!((shares - 1.0).abs() < 0.01);
    }

    #[test]
    fn never_reports_a_negative_free_space_when_the_context_is_over_budget() {
        let breakdown = compute_context_breakdown(&contexts(), None, 12_000, Some(8000));
        assert_eq!(breakdown.free, Some(0));
        assert_eq!(breakdown.free_share, Some(0.0));
        assert_eq!(breakdown.ratio, Some(1.0));
    }

    #[test]
    fn survives_an_empty_conversation() {
        let breakdown = compute_context_breakdown(&[], None, 0, Some(200_000));
        assert!(breakdown.segments.is_empty());
        assert_eq!((breakdown.used, breakdown.free, breakdown.ratio), (0, Some(200_000), Some(0.0)));
    }

    #[test]
    fn counts_a_compactions_kept_messages_as_what_they_are_and_its_item_as_compacted_history() {
        let contexts = [card("c1", "codex"), user("u", &"b".repeat(40))];
        let ContextItem::System { native_compaction: Some(compaction), .. } = &contexts[0] else { panic!() };
        let breakdown = compute_context_breakdown(&contexts, Some(compaction), 3_500, Some(10_000));
        let row = |id| breakdown.segments.iter().find(|segment| segment.id == id).map_or(0, |segment| segment.tokens);
        assert_eq!(row(ContextSegmentId::Compacted), 3_000 - 1);
        assert_eq!(row(ContextSegmentId::User), 10 + 1);
        assert_eq!(row(ContextSegmentId::Other), 3_500 - 3_010);
    }
}
