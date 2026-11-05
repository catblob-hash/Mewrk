//! Upstream's `lib/historyRecord.test.ts`.

use mewrk_pane_tasks::history_record::*;
use serde_json::{json, Value};

fn entry(seq: i64, kind: &str, over: Value) -> HistoryEntry {
    let mut base = json!({ "seq": seq, "createdAt": "2026-09-30T10:00:00Z", "kind": kind, "detail": {} });
    if kind != "edit" {
        base["requestId"] = json!("run_a");
    }
    for (k, v) in over.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

fn request(seq: i64, run: &str, detail: Value) -> HistoryEntry {
    let mut d = json!({ "type": "model", "attempt": 1, "modelId": "claude-opus-5", "messagesAdded": 1 });
    for (k, v) in detail.as_object().unwrap() {
        if v.is_null() {
            d.as_object_mut().unwrap().remove(k);
        } else {
            d[k] = v.clone();
        }
    }
    entry(seq, "request", json!({ "requestId": run, "detail": d }))
}

fn req(seq: i64, run: &str) -> HistoryEntry {
    request(seq, run, json!({}))
}

fn response(seq: i64, run: &str, answers: i64) -> HistoryEntry {
    entry(seq, "response", json!({ "requestId": run, "answers": answers }))
}

fn sent(seq: i64) -> HistoryEntry {
    entry(seq, "edit", json!({ "detail": { "source": "message", "inserted": 1 } }))
}

fn edited(seq: i64, counts: Value) -> HistoryEntry {
    let mut d = counts;
    d["source"] = json!("edit");
    entry(seq, "edit", json!({ "detail": d }))
}

fn settled(seq: i64, run: &str) -> HistoryEntry {
    entry(seq, "run", json!({ "requestId": run, "detail": { "source": "run", "inserted": 1 } }))
}

fn labels() -> EventLabels {
    EventLabels {
        tools_added: Box::new(|c| format!("{c} tools added")),
        native_compaction: Box::new(|c| format!("Compacted by {}, {} → {}", if c.model_name.is_empty() { c.model } else { c.model_name }, c.tokens_before, c.tokens_after)),
        tools_registered: Box::new(|c| format!("{c} tools registered")),
        local_only: "local only".into(),
        interrupted: "cut off".into(),
        unseen: "(unseen)".into(),
        rewrote_input: "rewrote input".into(),
        added_context: "added context".into(),
        blocked: "blocked".into(),
        halted: "halted".into(),
        failed: "failed".into(),
        denied: "denied".into(),
        permission: Box::new(|d| format!("permission:{d}")),
        truncated: "truncated".into(),
        paused: "paused".into(),
        images: Box::new(|c| format!("{c} images")),
        files: Box::new(|c| format!("{c} files")),
    }
}

fn seqs(entries: &[HistoryEntry]) -> Vec<i64> {
    entries.iter().map(|e| e.seq).collect()
}

fn kinds(bars: &[HistoryBar]) -> Vec<BarKind> {
    bars.iter().map(|b| b.kind).collect()
}

#[test]
fn reads_a_requests_detail_as_the_summary_a_bar_is_drawn_from() {
    let s = request_summary(&request(4, "run_a", json!({ "type": "search" })));
    assert_eq!((s.seq, s.kind.as_str(), s.request_id.as_str(), s.model_id.as_str(), s.messages_added), (4, "search", "run_a", "claude-opus-5", Some(1.0)));
}

#[test]
fn says_a_count_it_was_never_told_is_unknown_rather_than_zero() {
    assert_eq!(request_summary(&entry(1, "request", json!({}))).messages_added, None);
}

#[test]
fn leaves_a_counter_nobody_reported_absent_rather_than_calling_it_zero() {
    let total = sum_usage(&[Some(HistoryUsage { input_tokens: Some(5), ..Default::default() }), Some(HistoryUsage { input_tokens: Some(7), output_tokens: Some(2), ..Default::default() })]);
    assert_eq!(total, HistoryUsage { input_tokens: Some(12), cached_input_tokens: None, output_tokens: Some(2) });
    assert!(usage_is_empty(&sum_usage(&[None, Some(HistoryUsage::default())])));
}

#[test]
fn opens_a_turn_with_what_the_user_sent_and_keeps_every_entry_of_its_run_with_it() {
    let bars = history_bars(
        &[
            sent(1),
            entry(2, "hook", json!({ "requestId": "run_a", "detail": { "event": "UserPromptSubmit" } })),
            req(3, "run_a"),
            response(4, "run_a", 3),
            entry(5, "tool", json!({ "requestId": "run_a", "callId": "c1" })),
            entry(6, "result", json!({ "requestId": "run_a", "callId": "c1" })),
            request(7, "run_a", json!({ "messagesAdded": 0 })),
            response(8, "run_a", 7),
            entry(9, "hook", json!({ "requestId": "run_a", "detail": { "event": "Stop" } })),
            settled(10, "run_a"),
        ],
        false,
    );
    assert_eq!(kinds(&bars), [BarKind::Turn]);
    assert_eq!(bars[0].requests.iter().map(|r| r.seq).collect::<Vec<_>>(), [3, 7]);
    assert_eq!(seqs(&bars[0].entries), [1, 2, 4, 5, 6, 8, 9, 10]);
}

#[test]
fn keeps_a_run_that_brought_no_message_of_the_users_in_the_turn_before_it() {
    let bars = history_bars(&[sent(1), req(2, "run_a"), response(3, "run_a", 2), request(4, "run_b", json!({ "messagesAdded": 0 })), response(5, "run_b", 4)], false);
    assert_eq!(bars.iter().map(|b| b.requests.len()).collect::<Vec<_>>(), [2]);
}

#[test]
fn stands_the_users_edits_between_turns_as_a_bar_of_their_own() {
    let bars = history_bars(
        &[
            sent(1),
            req(2, "run_a"),
            response(3, "run_a", 2),
            settled(4, "run_a"),
            edited(5, json!({ "removed": 1 })),
            edited(6, json!({ "replaced": 1 })),
            request(7, "run_b", json!({ "messagesAdded": 0 })),
            response(8, "run_b", 7),
            settled(9, "run_b"),
            edited(10, json!({ "inserted": 1 })),
            sent(11),
            req(12, "run_c"),
            response(13, "run_c", 12),
        ],
        false,
    );
    assert_eq!(kinds(&bars), [BarKind::Turn, BarKind::Edits, BarKind::Turn, BarKind::Edits, BarKind::Turn]);
    assert_eq!(seqs(&bars[1].entries), [5, 6]);
    assert_eq!(bars[2].requests.iter().map(|r| r.seq).collect::<Vec<_>>(), [7]);
    assert_eq!(seqs(&bars[2].entries), [8, 9]);
    assert_eq!(bars.iter().filter(|b| b.kind == BarKind::Turn).map(|b| b.index).collect::<Vec<_>>(), [1, 2, 3]);
}

#[test]
fn calls_a_turn_interrupted_when_its_last_request_never_got_an_answer_back() {
    let bars = history_bars(
        &[
            sent(1),
            req(2, "run_a"),
            response(3, "run_a", 2),
            sent(4),
            req(5, "run_b"),
            response(6, "run_b", 5),
            entry(7, "tool", json!({ "requestId": "run_b", "callId": "c1" })),
            request(8, "run_b", json!({ "messagesAdded": 0 })),
            settled(9, "run_b"),
            sent(10),
            req(11, "run_c"),
            response(12, "run_c", 11),
        ],
        false,
    );
    assert_eq!(kinds(&bars), [BarKind::Turn, BarKind::Interrupted, BarKind::Turn]);
}

#[test]
fn does_not_call_a_turn_interrupted_from_before_answers_were_recorded() {
    let bars = history_bars(&[sent(1), req(2, "run_a"), settled(3, "run_a"), sent(4), req(5, "run_b"), response(6, "run_b", 5)], false);
    assert_eq!(kinds(&bars), [BarKind::Turn, BarKind::Turn]);
}

#[test]
fn does_not_call_the_turn_that_is_still_running_interrupted() {
    let history = [sent(1), req(2, "run_a"), response(3, "run_a", 2), sent(4), req(5, "run_b")];
    assert_eq!(kinds(&history_bars(&history, true)), [BarKind::Turn, BarKind::Turn]);
    assert_eq!(kinds(&history_bars(&history, false)), [BarKind::Turn, BarKind::Interrupted]);
}

#[test]
fn gives_what_nothing_has_sent_yet_a_bar_of_its_own() {
    let bars = history_bars(&[sent(1), req(2, "run_a"), response(3, "run_a", 2), sent(4)], false);
    assert_eq!(kinds(&bars), [BarKind::Turn, BarKind::Pending]);
    assert_eq!(seqs(&bars[1].entries), [4]);
}

#[test]
fn keeps_a_run_a_hook_stopped_before_it_sent_anything_with_the_turn_it_followed() {
    let bars = history_bars(&[sent(1), entry(2, "hook", json!({ "requestId": "run_blocked", "detail": { "event": "UserPromptSubmit", "blocked": true } }))], false);
    assert_eq!(kinds(&bars), [BarKind::Pending]);
    assert_eq!(seqs(&bars[0].entries), [1, 2]);
}

#[test]
fn falls_back_to_the_run_boundary_for_requests_recorded_without_a_message_of_their_own() {
    let bars = history_bars(&[request(1, "run_a", json!({ "messagesAdded": null })), request(2, "run_b", json!({ "messagesAdded": null })), request(3, "", json!({ "type": "search", "messagesAdded": null }))], false);
    assert_eq!(bars.iter().map(|b| b.requests.iter().map(|r| r.seq).collect::<Vec<_>>()).collect::<Vec<_>>(), [vec![1], vec![2, 3]]);
}

#[test]
fn adds_up_what_a_turn_cost_names_its_model_and_says_when_the_models_differ() {
    let bars = history_bars(
        &[
            sent(1),
            entry(2, "request", json!({ "requestId": "run_a", "detail": { "type": "model", "modelId": "claude-opus-5", "messagesAdded": 1 }, "usage": { "inputTokens": 1000, "cachedInputTokens": 800 } })),
            entry(3, "request", json!({ "requestId": "run_a", "detail": { "type": "model", "modelId": "claude-sonnet-5", "messagesAdded": 0 }, "usage": { "inputTokens": 1400, "outputTokens": 60 } })),
        ],
        false,
    );
    assert_eq!(bars[0].usage, HistoryUsage { input_tokens: Some(2400), cached_input_tokens: Some(800), output_tokens: Some(60) });
    assert_eq!(bars[0].model_id, "claude-opus-5");
    assert!(bars[0].mixed_models);
}

#[test]
fn returns_nothing_for_an_empty_history() {
    assert!(history_bars(&[], false).is_empty());
}

#[test]
fn draws_a_settled_run_as_its_settlement_and_one_that_never_settled_as_its_entries() {
    let bars = history_bars(
        &[
            sent(1),
            req(2, "run_a"),
            response(3, "run_a", 2),
            entry(4, "tool", json!({ "requestId": "run_a", "callId": "c1" })),
            entry(5, "result", json!({ "requestId": "run_a", "callId": "c1" })),
            settled(6, "run_a"),
            sent(7),
            req(8, "run_b"),
            response(9, "run_b", 8),
            entry(10, "tool", json!({ "requestId": "run_b", "callId": "c2" })),
            request(11, "run_b", json!({ "messagesAdded": 0 })),
        ],
        false,
    );
    let shape = |b: &HistoryBar| -> Vec<(&'static str, i64)> {
        bar_items(b)
            .iter()
            .map(|i| match i {
                BarItem::Change(e) => ("change", e.seq),
                BarItem::Event(e) => ("event", e.seq),
                BarItem::Request(r) => ("request", r.seq),
            })
            .collect()
    };
    assert_eq!(shape(&bars[0]), [("change", 1), ("change", 6)]);
    assert_eq!(bars[1].kind, BarKind::Interrupted);
    assert_eq!(shape(&bars[1]), [("change", 7), ("request", 8), ("event", 9), ("event", 10), ("request", 11)]);
}

fn op(ordinal: i64, change: &str, value: Option<Value>, before: Option<Value>) -> HistoryOp {
    HistoryOp { ordinal, op: change.into(), context_id: format!("ctx_{ordinal}"), position: None, body: value.map(|v| v.to_string()), before: before.map(|v| v.to_string()) }
}

fn change(of: &HistoryEntry, ops: Vec<HistoryOp>) -> HistoryEntryDetail {
    HistoryEntryDetail { entry: of.clone(), body: None, truncated: false, parts: None, ops: Some(ops) }
}

fn badge_labels(row: &MessageRow) -> Vec<&str> {
    row.badges.iter().map(|b| b.label.as_str()).collect()
}

#[test]
fn reads_what_a_run_settled_as_the_messages_it_put_on_the_timeline() {
    let run = settled(9, "run_a");
    let rows = describe_change(
        &run,
        &change(
            &run,
            vec![
                op(0, "insert", Some(json!({ "kind": "system", "content": "Tools added: shell, grep", "localOnly": true, "toolsAdded": ["shell", "grep"] })), None),
                op(1, "insert", Some(json!({ "kind": "reasoning", "content": "先看看目录" })), None),
                op(2, "insert", Some(json!({ "kind": "assistant", "content": "" })), None),
                // `requestedInput` before `input`: the body keeps whichever order it was written in.
                op(3, "insert", Some(json!({ "kind": "tool", "toolName": "shell", "input": { "command": "ls -a" }, "requestedInput": { "command": "ls" }, "result": { "success": false, "output": "denied" } })), None),
                op(4, "insert", Some(json!({ "kind": "assistant", "content": "只写到一半", "interrupted": true })), None),
                op(5, "insert", Some(json!({ "kind": "system", "content": "branch: main", "localOnly": true, "hookExecution": { "hookName": "context" } })), None),
            ],
        ),
        &labels(),
    );
    let got: Vec<_> = rows.iter().map(|r| (r.kind, r.label, r.detail.as_str(), r.preview.as_str())).collect();
    assert_eq!(
        got,
        [
            (MessageKind::ToolsAdded, "system", "2 tools added", "shell, grep"),
            (MessageKind::Reasoning, "reasoning", "", "先看看目录"),
            (MessageKind::Tool, "tool", "shell", "{\"command\":\"ls -a\"}"),
            (MessageKind::Assistant, "assistant", "", "只写到一半"),
            (MessageKind::System, "system", "context", "branch: main"),
        ]
    );
    assert_eq!(badge_labels(&rows[2]), ["rewrote input", "failed"]);
    assert_eq!(rows[2].text, "{\n  \"command\": \"ls -a\"\n}\n\ndenied\n\n[requestedInput]\n{\n  \"command\": \"ls\"\n}");
    assert_eq!(rows[3].badges, [EventBadge { label: "cut off".into(), tone: BadgeTone::Warning }]);
    assert_eq!(rows[4].badges, [EventBadge { label: "local only".into(), tone: BadgeTone::Neutral }]);
    assert!(rows.iter().all(|r| r.change == "insert"));
}

#[test]
fn draws_a_native_compactions_card_as_its_title_with_nothing_to_open() {
    let edit = edited(5, json!({ "inserted": 1 }));
    let rows = describe_change(&edit, &change(&edit, vec![op(0, "insert", Some(json!({ "kind": "system", "content": "", "localOnly": true, "nativeCompaction": { "providerId": "codex", "model": "gpt-6-astra", "modelName": "GPT-6 Astra", "parts": [{ "type": "custom", "kind": "openai.compaction" }], "tokensBefore": 180000, "tokensAfter": 4200 } })), None)]), &labels());
    let r = &rows[0];
    assert_eq!((r.label, r.detail.as_str(), r.preview.as_str(), r.text.as_str(), r.fixed), ("system", "Compacted by GPT-6 Astra, 180000 → 4200", "", "", true));
    assert!(r.badges.is_empty());
}

#[test]
fn draws_a_removed_message_as_the_message_that_went() {
    let edit = edited(5, json!({ "removed": 1 }));
    let rows = describe_change(&edit, &change(&edit, vec![op(0, "remove", None, Some(json!({ "kind": "assistant", "content": "原来的回复" })))]), &labels());
    let r = &rows[0];
    assert_eq!((r.change.as_str(), r.kind, r.label, r.preview.as_str(), r.additions, r.deletions), ("remove", MessageKind::Assistant, "assistant", "原来的回复", 0, 0));
}

#[test]
fn says_so_when_a_removed_row_was_never_seen_by_the_record() {
    let edit = edited(5, json!({ "removed": 1 }));
    let rows = describe_change(&edit, &change(&edit, vec![op(0, "remove", None, None)]), &labels());
    assert_eq!((rows[0].change.as_str(), rows[0].label, rows[0].preview.as_str()), ("remove", "", "(unseen)"));
}

#[test]
fn opens_a_rewritten_message_into_its_diff() {
    let edit = edited(6, json!({ "replaced": 1 }));
    let rows = describe_change(&edit, &change(&edit, vec![op(0, "replace", Some(json!({ "kind": "user", "content": "看看这两个文件" })), Some(json!({ "kind": "user", "content": "看看这个文件" })))]), &labels());
    let r = &rows[0];
    assert_eq!((r.change.as_str(), r.kind, r.label, r.preview.as_str(), r.additions, r.deletions), ("replace", MessageKind::User, "user", "看看这两个文件", 1, 1));
    assert!(r.patch.contains("--- a/user#ctx_0") && r.patch.contains("-看看这个文件") && r.patch.contains("+看看这两个文件"));
}

#[test]
fn reads_what_the_user_sent_with_the_attachments_it_carried() {
    let edit = sent(1);
    let rows = describe_change(&edit, &change(&edit, vec![op(0, "insert", Some(json!({ "kind": "user", "content": "看图\n第二行", "images": [{}], "files": [{}, {}] })), None)]), &labels());
    assert_eq!((rows[0].kind, rows[0].label, rows[0].preview.as_str()), (MessageKind::User, "user", "看图"));
    assert_eq!(badge_labels(&rows[0]), ["1 images", "2 files"]);
}

fn prompt(kind: &str, body: &str) -> HistoryPart {
    HistoryPart { ordinal: 0, kind: kind.into(), hash: body.into(), body: body.into(), bytes: body.len() as i64, truncated: false }
}

#[test]
fn draws_the_system_prompt_a_conversation_opened_with_as_new() {
    let rows = describe_prompts(None, &[prompt("system", "You are Mewrk."), prompt("systemDynamic", "Today")], "p1", &labels());
    let got: Vec<_> = rows.iter().map(|r| (r.change.as_str(), r.label, r.preview.as_str())).collect();
    assert_eq!(got, [("insert", "system", "You are Mewrk."), ("insert", "system", "Today")]);
}

#[test]
fn draws_nothing_for_a_prompt_the_request_before_already_carried() {
    let parts = [prompt("system", "You are Mewrk."), prompt("tools", "[]")];
    assert!(describe_prompts(Some(&parts), &parts, "p2", &labels()).is_empty());
}

#[test]
fn opens_a_prompt_that_changed_into_its_diff() {
    let rows = describe_prompts(Some(&[prompt("system", "You are Mewrk.")]), &[prompt("system", "You are Mewrk, careful.")], "p3", &labels());
    assert_eq!(rows[0].change, "replace");
    assert!(rows[0].patch.contains("+You are Mewrk, careful."));
}

fn tool_list(tools: &[(&str, &str)]) -> HistoryPart {
    // Written by hand so its keys stay in the order the host wrote them, which is not alphabetical in general.
    let items: Vec<String> = tools.iter().map(|(name, description)| format!("{{\"description\":{},\"inputSchema\":{{}},\"name\":{}}}", json!(description), json!(name))).collect();
    prompt("tools", &format!("[{}]", items.join(",")))
}

fn marker(tools: &[&str]) -> HistoryPart {
    prompt("message", &json!({ "role": "system", "content": "", "providerOptions": { "mewrk": { "toolAddition": tools } } }).to_string())
}

#[test]
fn draws_the_tools_a_conversation_opened_with_as_one_row_tagged_as_the_field() {
    let rows = describe_tools(None, &[prompt("system", "You are Mewrk."), tool_list(&[("ls", ""), ("grep", "")])], "p1", &labels());
    let r = &rows[0];
    assert_eq!((r.change.as_str(), r.kind, r.label, r.detail.as_str(), r.preview.as_str(), r.patch.as_str()), ("insert", MessageKind::Tools, "tools", "2 tools registered", "ls, grep", ""));
    assert!(r.text.contains("ls\n{") && r.text.contains("grep\n{"));
}

#[test]
fn draws_nothing_for_a_list_the_request_before_already_declared() {
    assert!(describe_tools(Some(&[tool_list(&[("ls", "")])]), &[tool_list(&[("ls", "")])], "p2", &labels()).is_empty());
}

#[test]
fn names_what_a_changed_list_added_took_away_and_rewrote_and_opens_into_its_diff() {
    let rows = describe_tools(Some(&[tool_list(&[("ls", ""), ("plan", ""), ("grep", "old")])]), &[tool_list(&[("ls", ""), ("grep", "new"), ("handoff", "")])], "p3", &labels());
    let r = &rows[0];
    assert_eq!((r.change.as_str(), r.detail.as_str(), r.preview.as_str()), ("replace", "3 tools registered", "+handoff −plan ~grep"));
    assert!(r.patch.contains("+  \"description\": \"new\""));
    assert!(r.additions > 0 && r.deletions > 0);
}

#[test]
fn leaves_a_tool_handed_over_by_append_out_of_the_declared_list() {
    let user = prompt("message", "{\"role\":\"user\",\"content\":\"hi\"}");
    assert!(describe_tools(Some(&[tool_list(&[("ls", "")]), user.clone()]), &[tool_list(&[("ls", ""), ("handoff", "")]), user, marker(&["handoff"])], "p4", &labels()).is_empty());
}

#[test]
fn reads_the_tools_a_requests_markers_hand_over_in_order_and_once() {
    assert_eq!(appended_tools(&[marker(&["handoff", "note"]), prompt("message", "{\"role\":\"user\",\"content\":\"toolAddition\"}"), marker(&["note", "mcp__x"])]), ["handoff", "note", "mcp__x"]);
}

#[test]
fn draws_only_what_this_request_was_the_first_to_hand_over() {
    let before = [tool_list(&[("ls", "")]), marker(&["handoff"])];
    let after = [tool_list(&[("ls", "")]), marker(&["handoff"]), marker(&["mcp__x", "mcp__y"])];
    let rows = describe_appended(Some(&before), &after, "a9", &labels());
    let got: Vec<_> = rows.iter().map(|r| (r.change.as_str(), r.kind, r.label, r.detail.as_str(), r.preview.as_str())).collect();
    assert_eq!(got, [("insert", MessageKind::ToolsAdded, "system", "2 tools added", "mcp__x, mcp__y")]);
    assert!(describe_appended(Some(&after), &after, "a10", &labels()).is_empty());
}

fn detail_of(e: &HistoryEntry, body: &str) -> HistoryEntryDetail {
    HistoryEntryDetail { entry: e.clone(), body: Some(body.to_owned()), truncated: false, parts: None, ops: None }
}

#[test]
fn reads_a_response_as_its_text_and_the_tools_it_called() {
    let e = entry(3, "response", json!({ "detail": { "finishReason": "length" } }));
    let body = json!({ "role": "assistant", "content": [{ "type": "text", "text": "先看看" }, { "type": "tool-call", "toolCallId": "c1", "toolName": "shell", "input": { "command": "ls" } }] }).to_string();
    let d = describe_event(&e, Some(&detail_of(&e, &body)), &labels());
    assert_eq!((d.label, d.detail.as_str(), d.preview.as_str()), ("assistant", "shell", "先看看"));
    assert_eq!(d.badges, [EventBadge { label: "truncated".into(), tone: BadgeTone::Warning }]);
}

#[test]
fn marks_reasoning_a_response_carried_with_a_provider_signature() {
    let e = entry(3, "response", json!({}));
    let body = json!({ "role": "assistant", "content": [{ "type": "reasoning", "text": "先想一步", "providerOptions": { "anthropic": { "signature": "sig" } } }, { "type": "text", "text": "好" }] }).to_string();
    let d = describe_event(&e, Some(&detail_of(&e, &body)), &labels());
    assert!(d.text.contains("[reasoning · signed]") && d.text.contains("\"signature\": \"sig\""));
}

#[test]
fn keeps_a_response_body_it_cannot_parse_rather_than_dropping_it() {
    let e = entry(3, "response", json!({}));
    assert_eq!(describe_event(&e, Some(&detail_of(&e, "{ not json")), &labels()).text, "{ not json");
}

#[test]
fn says_what_a_hook_decided_and_opens_into_its_rewrite() {
    let hook = entry(4, "hook", json!({ "detail": { "event": "PreToolUse", "hookName": "guard", "matcher": "shell", "blocked": false, "halted": false, "permission": "allow", "rewroteInput": true, "addedContext": false, "success": true } }));
    let d = describe_event(&hook, Some(&detail_of(&hook, &json!({ "output": "", "updatedInput": { "command": "ls -a" } }).to_string())), &labels());
    assert_eq!((d.label, d.detail.as_str()), ("hook", "PreToolUse · guard · shell"));
    assert_eq!(d.badges.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(), ["permission:allow", "rewrote input"]);
    assert!(d.text.contains("\"command\": \"ls -a\""));
}

#[test]
fn opens_a_call_a_hook_rewrote_into_the_diff_between_what_was_sent_and_what_ran() {
    let tool = entry(5, "tool", json!({ "callId": "c1", "detail": { "name": "shell", "rewritten": true } }));
    let d = describe_event(&tool, Some(&detail_of(&tool, &json!({ "input": { "command": "ls -a" }, "requestedInput": { "command": "ls" } }).to_string())), &labels());
    assert_eq!((d.label, d.detail.as_str()), ("tool", "shell"));
    assert_eq!(d.badges.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(), ["rewrote input"]);
    assert!(d.patch.contains("-  \"command\": \"ls\"") && d.patch.contains("+  \"command\": \"ls -a\""));
}

#[test]
fn marks_a_failed_result_and_keeps_its_output() {
    let result = entry(6, "result", json!({ "callId": "c1", "detail": { "name": "shell", "success": false } }));
    let d = describe_event(&result, Some(&detail_of(&result, &json!({ "output": "boom\nstack" }).to_string())), &labels());
    assert_eq!((d.label, d.detail.as_str(), d.preview.as_str()), ("tool", "shell", "boom"));
    assert_eq!(d.badges, [EventBadge { label: "failed".into(), tone: BadgeTone::Warning }]);
}

#[test]
fn still_says_what_an_entry_is_before_its_body_has_been_read() {
    let d = describe_event(&entry(8, "tool", json!({ "detail": { "name": "shell", "denied": "no" } })), None, &labels());
    assert_eq!((d.label, d.detail.as_str()), ("tool", "shell"));
    assert_eq!(d.badges, [EventBadge { label: "denied".into(), tone: BadgeTone::Warning }]);
}

#[test]
fn shows_a_recorded_body_in_the_order_it_was_written() {
    // `JSON.stringify` keeps insertion order; a sorted map would rewrite evidence.
    let e = entry(9, "tool", json!({ "detail": { "name": "shell" } }));
    let d = describe_event(&e, Some(&detail_of(&e, r#"{"input":{"zeta":1,"alpha":2}}"#)), &labels());
    assert_eq!(d.preview, r#"{"zeta":1,"alpha":2}"#);
    assert_eq!(d.text, "{\n  \"zeta\": 1,\n  \"alpha\": 2\n}");
}
