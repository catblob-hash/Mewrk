//! `components/ToolRenderers.test.tsx` and `lib/orchestration.test.ts`.
//!
//! Upstream reads its assertions off the DOM; here they read the model `tool_view` hands `tool_detail`, in both languages, and the
//! rig drives the card itself where the assertion is about drawing or an interaction.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mewrk_client::model::{ContextItem, ImageAttachment, ToolDescriptor};
use mewrk_client::prelude::E;
use mewrk_client::runs::{StreamingToolPhase, SubagentLiveState, SubagentLiveStatus};
use mewrk_pane_files::icons::file_icon_name;
use mewrk_ui_chat::item::{Item, ToolItem};
use mewrk_ui_chat::orchestration::*;
use mewrk_ui_chat::tool_detail::{adopt_site_icon, has_site_icon, tool_body_key, tool_detail_renderer, tool_raw_pre_key, tool_raw_summary_key, visual_rows, window_of};
use mewrk_ui_chat::tool_view::*;
use serde_json::{json, Value};

use crate::rig::Rig;

const MCP_WIRE_NAME: &str = "mcp__playwright_1a2b3c4d5e__browser_click__0f9e8d7c6b";

/// A tool card to hand out views of, as upstream's `tool()` fixture.
#[derive(Clone)]
struct Card {
    item: ContextItem,
    streaming: bool,
    status: Option<StreamingToolPhase>,
    live: Option<Arc<SubagentLiveState>>,
}

impl Card {
    fn view(&self) -> ToolItem<'_> {
        Item { item: &self.item, streaming: self.streaming, stream_status: self.status, live: self.live.as_ref(), started_at: None }.tool().expect("a tool card")
    }

    fn zh(&self) -> ToolPresentation {
        get_tool_presentation_in(&self.view(), None, Lang::Zh, None, None)
    }

    fn en(&self) -> ToolPresentation {
        get_tool_presentation_in(&self.view(), None, Lang::En, None, None)
    }

    fn detail(&self) -> ToolDetailModel {
        tool_detail_model_in(&self.view(), None, Lang::Zh)
    }

    fn detail_en(&self) -> ToolDetailModel {
        tool_detail_model_in(&self.view(), None, Lang::En)
    }

    fn kv(&self) -> KvCard {
        self.detail().kv().expect("a key-value card").clone()
    }

    fn streaming(mut self, status: StreamingToolPhase) -> Self {
        self.streaming = true;
        self.status = Some(status);
        self
    }

    fn running(self) -> Self {
        self.streaming(StreamingToolPhase::Running)
    }

    fn patched(&self, f: impl FnOnce(&mut Value)) -> Self {
        let mut value = serde_json::to_value(&self.item).expect("serializes");
        f(&mut value);
        Self { item: serde_json::from_value(value).expect("deserializes"), ..self.clone() }
    }

    fn with_notice(&self, notice: &str) -> Self {
        self.patched(|v| v["notice"] = json!(notice))
    }

    fn with_duration(&self, ms: u64) -> Self {
        self.patched(|v| v["result"]["durationMs"] = json!(ms))
    }

    fn with_image(&self, image: &ImageAttachment) -> Self {
        self.patched(|v| v["result"]["images"] = json!([image]))
    }

    fn with_subagent(&self, record: Value) -> Self {
        self.patched(|v| v["subagent"] = record)
    }

    fn with_output(&self, output: &str) -> Self {
        self.patched(|v| v["result"]["output"] = json!(output))
    }

    fn with_success(&self, success: bool) -> Self {
        self.patched(|v| v["result"]["success"] = json!(success))
    }
}

fn card(name: &str, input: Value, output: &str, success: bool, diff: Option<&str>) -> Card {
    let mut result = json!({ "success": success, "output": output, "executedAt": "2026-07-14T00:00:00Z", "durationMs": 12 });
    if let Some(diff) = diff {
        result["diff"] = json!(diff);
    }
    let item = serde_json::from_value(json!({
        "id": format!("tool-{name}"),
        "kind": "tool",
        "toolName": name,
        "round": 1,
        "input": input,
        "result": result,
        "createdAt": "2026-07-14T00:00:00Z"
    }))
    .expect("a tool context");
    Card { item, streaming: false, status: None, live: None }
}

fn tool(name: &str, input: Value, output: &str) -> Card {
    card(name, input, output, true, None)
}

fn failed(name: &str, input: Value, output: &str) -> Card {
    card(name, input, output, false, None)
}

fn pixel(name: &str) -> ImageAttachment {
    serde_json::from_value(json!({ "id": "0".repeat(64), "name": name, "mime": "image/png", "width": 1, "height": 1, "bytes": 68 })).expect("an attachment")
}

fn mcp_descriptor(label: &str, name: &str, category: &str) -> ToolDescriptor {
    serde_json::from_value(json!({ "name": name, "label": label, "description": "", "category": category, "dangerous": false, "parameters": [] })).expect("a descriptor")
}

/// `.tool-renderer__lines`' text, line by line.
fn lines_of(model: &ToolDetailModel) -> &ToneText {
    match &model.body {
        DetailBody::Lines { text, .. } => text,
        other => panic!("not lines: {other:?}"),
    }
}

fn entries_of(model: &ToolDetailModel) -> &[ListEntry] {
    match &model.body {
        DetailBody::Entries { entries, .. } => entries,
        other => panic!("not entries: {other:?}"),
    }
}

fn empty_of(model: &ToolDetailModel) -> &str {
    match &model.body {
        DetailBody::Empty(text) => text,
        other => panic!("not an empty result: {other:?}"),
    }
}

// ---------------------------------------------------------------- handoff rows

#[test]
fn names_the_hosts_handoff_notice_apart_from_a_background_result() {
    const HANDOFF: &str = "Context reached the auto-compact threshold; handoff requested";
    let notice = tool("host_message", json!({}), "<system-reminder>\nHand this conversation off.\n</system-reminder>").with_notice("handoff");
    assert_eq!(notice.en().title, HANDOFF);
    let result = tool("host_message", json!({}), "<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]\n\n<task-notification>\n<task-id>reviewer</task-id>\n</task-notification>\n</system-reminder>");
    assert_eq!(result.en().title, "Delivered a background result");
    // Delivered in `box`, the same notice and result say the same.
    assert_eq!(tool("box", json!({ "none": [] }), "Hand this conversation off.").with_notice("handoff").en().title, HANDOFF);
    assert_eq!(tool("box", json!({ "none": [] }), "<task-notification>\n<task-id>reviewer</task-id>\n</task-notification>").en().title, "Delivered a background result");
    // A card from the era every notice went in `box` still says what it carried.
    assert_eq!(tool("box", json!({ "none": [] }), "<task-notification>\n<summary>past the threshold</summary>\n</task-notification>").with_notice("handoff").en().title, HANDOFF);
    // A card written before the card held the whole message named its kind in its input.
    let legacy = tool("box", json!({ "tasks": [], "notification": { "kind": "handoff", "summary": "past the threshold" } }), "Hand this conversation off before its context runs out.");
    assert_eq!(legacy.en().title, HANDOFF);
}

#[test]
fn names_the_notice_about_instruction_files_a_run_left_out() {
    let notice = tool("host_message", json!({}), "<system-reminder>\nSome instruction files were left out of this run:\n- workspace:MEWRK.md: not valid UTF-8 text\n</system-reminder>").with_notice("instruction_skips");
    assert_eq!(notice.en().title, "Skipped some instruction files");
}

#[test]
fn opens_a_written_note_onto_its_name_index_line_and_body() {
    let note = tool("create_handoff_note", json!({ "name": "state", "description": "where the work stands", "content": "Step 2 of 3." }), "Created handoff note state.md and recorded it in the handoff index.");
    let presentation = note.en();
    assert_eq!(presentation.title, "Wrote handoff note");
    assert_eq!(presentation.target.as_deref(), Some("state"));
    assert_eq!(presentation.keys, Some(&["name", "description", "content"][..]));
    assert_eq!(tool_summary_kind(&note.view()), SummaryKind::Handoff);
}

// ---------------------------------------------------------------- shell command explanations

#[test]
fn replaces_a_shell_cards_title_with_the_shell_and_the_explanation() {
    let explain = |item: &Card, lang, text| get_tool_presentation_in(&item.view(), None, lang, Some(text), None).title;
    let item = tool("bash", json!({ "command": "ls -la" }), "total 0");
    assert_eq!(explain(&item, Lang::En, "List directory contents"), "Bash: List directory contents");
    assert_eq!(explain(&item, Lang::Zh, "列出文件详细信息"), "Bash：列出文件详细信息");
    let powershell = tool("powershell", json!({ "command": "Get-Process" }), "");
    let pwsh = tool("pwsh", json!({ "command": "Get-Process" }), "");
    assert_eq!(explain(&powershell, Lang::En, "List processes"), "Windows PowerShell: List processes");
    assert_eq!(explain(&pwsh, Lang::En, "List processes"), "PowerShell 7: List processes");
    assert_eq!(explain(&pwsh, Lang::Zh, "列出进程"), "PowerShell 7：列出进程");
    assert_eq!(explain(&powershell, Lang::Zh, "列出进程"), "Windows PowerShell：列出进程");
}

#[test]
fn renders_powershell_7_exactly_as_windows_powershell_under_its_own_name() {
    let command = json!({ "command": "Get-ChildItem -Force" });
    let done = |name: &str| tool(name, command.clone(), "a.txt");
    let bad = |name: &str| failed(name, command.clone(), "");
    assert_eq!(done("pwsh").en().title, "Ran PowerShell 7 command");
    assert_eq!(done("powershell").en().title, "Ran Windows PowerShell command");
    assert_eq!(done("pwsh").zh().title, "运行了 PowerShell 7 命令");
    assert_eq!(done("powershell").zh().title, "运行了 Windows PowerShell 命令");
    assert_eq!(bad("pwsh").en().title, "PowerShell 7 command failed");
    assert_eq!(bad("powershell").en().title, "Windows PowerShell command failed");
    // Everything but the name is shared: surface, family, target and summary bucket.
    let (pwsh, powershell) = (done("pwsh").en(), done("powershell").en());
    assert_eq!(pwsh.surface, ToolSurface::Group);
    assert_eq!(pwsh.family, ToolViewFamily::Terminal);
    assert_eq!(pwsh.target.as_deref(), Some("Get-ChildItem -Force"));
    assert_eq!(pwsh.icon, powershell.icon);
    assert_eq!(pwsh.surface, powershell.surface);
    assert_eq!(pwsh.family, powershell.family);
    assert_eq!(pwsh.target, powershell.target);
    assert_eq!(tool_summary_kind(&done("pwsh").view()), SummaryKind::Commands);
    assert_eq!(tool_surface_for_name("pwsh"), tool_surface_for_name("powershell"));
}

#[test]
fn keeps_the_original_title_without_an_explanation_and_marks_failures() {
    let item = failed("zsh", json!({ "command": "false" }), "");
    assert_eq!(item.en().title, "zsh command failed");
    assert_eq!(get_tool_presentation_in(&item.view(), None, Lang::En, Some("Run false"), None).title, "zsh: Run false (failed)");
    // Only shell tools take one.
    assert_eq!(get_tool_presentation_in(&tool("read", json!({ "path": "a.txt" }), "x").view(), None, Lang::En, Some("ignored"), None).title, "Read a.txt");
}

// ---------------------------------------------------------------- titles that name their subject

#[test]
fn names_the_file_a_call_read_or_wrote_and_the_lines_its_change_added_and_removed() {
    let edited = card("edit", json!({ "path": "src/components/App.tsx", "find": "a", "replace": "b" }), "ok", true, Some("--- src/components/App.tsx\n+++ src/components/App.tsx\n@@ -1,2 +1,3 @@\n-a\n+b\n+c\n d\n"));
    let presentation = edited.zh();
    assert_eq!(presentation.title, "已编辑：App.tsx");
    let subject = presentation.subject.expect("a subject");
    assert_eq!((subject.before.as_str(), subject.text.as_str(), subject.after.as_str(), subject.path.as_deref()), ("已编辑：", "App.tsx", "", Some("src/components/App.tsx")));
    assert_eq!(presentation.counts, Some((2, 1)));
    let english = edited.en().subject.expect("a subject");
    assert_eq!((english.before.as_str(), english.text.as_str()), ("Edited ", "App.tsx"));

    let read = tool("read", json!({ "path": "C:\\work\\notes.md", "start_line": 40 }), "line");
    let presentation = read.zh();
    assert_eq!(presentation.title, "已读取：notes.md");
    let subject = presentation.subject.clone().expect("a subject");
    assert_eq!((subject.text.as_str(), subject.path.as_deref(), subject.line), ("notes.md", Some("C:\\work\\notes.md"), Some(40)));
    assert_eq!(presentation.counts, None);
    // Read from the top, the file opens at its top.
    assert_eq!(tool("read", json!({ "path": "a.txt" }), "x").zh().subject.and_then(|s| s.line), None);

    let written = card("write", json!({ "path": "README.md", "content": "x" }), "ok", true, Some("--- README.md\n+++ README.md\n@@ -1 +1 @@\n-old\n+x\n"));
    assert_eq!(written.en().title, "Wrote README.md");

    // Still running or failed, the phrase stays: there is nothing yet to name.
    let running = edited.clone().running();
    assert_eq!(running.zh().title, "正在编辑文件");
    assert_eq!(running.zh().subject, None);
}

#[test]
fn names_what_a_search_looked_for_set_as_code_and_how_much_it_found() {
    let found = tool(
        "find",
        json!({ "query": "*.tsx", "path": "src" }),
        "src/App.tsx\nsrc/main.tsx\ndist/x.tsx (ignored)\n(Showing 3 of 9 matches. Narrow the pattern or path to see the rest.)\n(1 of the matches are in paths Git ignores.)",
    );
    let presentation = found.zh();
    assert_eq!(presentation.title, "已查找：*.tsx");
    let subject = presentation.subject.expect("a subject");
    assert_eq!((subject.text.as_str(), subject.code), ("*.tsx", true));
    assert_eq!(presentation.note.as_deref(), Some("3+ 个结果"));
    assert_eq!(tool("find", json!({ "query": "*.rs" }), "No matching files").zh().note.as_deref(), Some("0 个结果"));

    // The default mode answers with files, so it counts files; matching lines are counted only when asked for.
    let files = tool("grep", json!({ "pattern": "TODO|FIXME" }), "src/a.ts\nsrc/b.ts\n[skipped] src/c.bin: unreadable");
    let presentation = files.en();
    assert_eq!(presentation.title, "Searched for TODO|FIXME");
    let subject = presentation.subject.expect("a subject");
    assert_eq!((subject.text.as_str(), subject.code), ("TODO|FIXME", true));
    assert_eq!(presentation.note.as_deref(), Some("2 files"));
    assert_eq!(presentation.stat.as_deref(), Some("2 files"));
    assert_eq!(files.zh().note.as_deref(), Some("2 个文件"));

    let searched = tool("grep", json!({ "pattern": "TODO|FIXME", "output_mode": "content" }), "src/a.ts:3:// TODO one\nsrc/b.ts:10:// FIXME two\n[skipped] src/c.bin: unreadable");
    let presentation = searched.en();
    assert_eq!(presentation.title, "Searched for TODO|FIXME");
    assert_eq!(presentation.note.as_deref(), Some("2 matches"));
    assert_eq!(presentation.stat.as_deref(), Some("2 matches"));
    assert_eq!(tool("grep", json!({ "pattern": "x", "output_mode": "content" }), "src/a.ts:1:x\n… more matches follow.").zh().note.as_deref(), Some("1+ 处匹配"));
    assert_eq!(tool("grep", json!({ "pattern": "x" }), "src/a.ts\n… more matches follow.").zh().note.as_deref(), Some("1+ 个文件"));
}

#[test]
fn names_a_search_by_its_pattern_then_by_what_narrowed_it() {
    assert_eq!(tool("grep", json!({ "pattern": "needle" }), "").zh().target.as_deref(), Some("needle"));
    let narrowed = tool("grep", json!({ "pattern": "needle", "glob": "*.rs", "output_mode": "content" }), "");
    assert_eq!(narrowed.zh().target.as_deref(), Some("needle · *.rs · 内容"));
    assert_eq!(narrowed.en().target.as_deref(), Some("needle · *.rs · content"));
    assert_eq!(tool("grep", json!({ "pattern": "needle", "output_mode": "count" }), "").en().target.as_deref(), Some("needle · count"));
    // The default mode needs no word, and a mode this card does not know is the default.
    assert_eq!(tool("grep", json!({ "pattern": "needle", "output_mode": "files_with_matches" }), "").zh().target.as_deref(), Some("needle"));
    assert_eq!(tool("grep", json!({ "pattern": "needle", "output_mode": "bogus" }), "a.rs").zh().note.as_deref(), Some("1 个文件"));
    // No pattern, no target: a glob alone names nothing.
    assert_eq!(tool("grep", json!({ "glob": "*.rs" }), "").zh().target, None);
}

// ---------------------------------------------------------------- failed titles

#[test]
fn says_why_after_what_failed() {
    let edit = failed("edit", json!({ "path": "src/App.tsx" }), "The exact text to replace was not found");
    assert_eq!(edit.zh().title, "编辑文件失败：The exact text to replace was not found");
    assert_eq!(edit.en().title, "Failed to edit file: The exact text to replace was not found");
    assert_eq!(get_tool_presentation_in(&edit.view(), None, Lang::Zh, None, Some("找不到要替换的文本")).title, "编辑文件失败：找不到要替换的文本");
    // A failed edit names no file and counts no lines.
    assert_eq!(edit.zh().subject, None);
}

#[test]
fn reads_past_a_commands_exit_code_and_its_colours_to_the_line_that_explains_it() {
    let bad = failed("bash", json!({ "command": "pnpm i" }), "Exit code 127\n\u{1b}[31mbash: pnpm: command not found\u{1b}[0m\nmore");
    assert_eq!(bad.zh().title, "Bash 命令失败：bash: pnpm: command not found");
    // The reason outranks the command's description.
    assert_eq!(get_tool_presentation_in(&bad.view(), None, Lang::Zh, Some("安装依赖"), Some("没有安装 pnpm")).title, "Bash 命令失败：没有安装 pnpm");
    assert_eq!(error_excerpt("Exit code 1").as_deref(), Some("Exit code 1"));
    assert_eq!(error_excerpt("  \n "), None);
    assert_eq!(error_excerpt(&"x".repeat(500)).map(|e| e.chars().count()), Some(240));
}

#[test]
fn keeps_the_phrase_alone_when_the_call_returned_nothing_to_quote() {
    let silent = failed("zsh", json!({ "command": "false" }), "");
    assert_eq!(silent.zh().title, "zsh 命令失败");
    assert_eq!(get_tool_presentation_in(&silent.view(), None, Lang::En, Some("Run false"), None).title, "zsh: Run false (failed)");
}

// ---------------------------------------------------------------- the registry

#[test]
fn shows_the_image_alone_for_image_producing_read_results() {
    let item = tool("read", json!({ "path": "diagram.png" }), "已读取图片 diagram.png（image/png，1×1，68 字节）").with_image(&pixel("diagram.png"));
    assert_eq!(item.zh().stat.as_deref(), Some("1×1"));
    // The receipt line the model reads must not be repeated under the thumbnail.
    assert_eq!(item.detail().body, DetailBody::ImageOnly);
    assert_eq!(item.view().result.images.len(), 1);
}

#[test]
fn explicitly_registers_the_complete_catalog_and_every_legacy_agent_event() {
    // `toolCatalog` in upstream's `seed.ts`.
    for name in [
        "ls", "grep", "pwsh", "powershell", "bash", "zsh", "sh", "write", "edit", "find", "read", "lsp", "web_search", "web_fetch", "preview_start", "preview_stop", "preview_list", "preview_logs", "preview_console_logs", "preview_screenshot", "preview_snapshot", "preview_inspect", "preview_click", "preview_fill", "preview_eval", "preview_network", "preview_resize", "preview_upload_image", "preview_dialog", "preview_input", "preview_zoom", "preview_wait", "screen_app", "screen_device", "screen_clipboard", "screen_package", "android_logcat", "android_shell", "agent_spawn", "task_wait", "task_list", "task_stop", "send_message", "box", "skill", "tool_search", "read_global_memory", "read_project_memory", "create_global_memory", "create_project_memory", "edit_global_memory", "edit_project_memory", "workflow", "ask_user", "fork", "plan", "exit_plan_mode", "read_handoff_note", "create_handoff_note", "edit_handoff_note", "handoff",
    ] {
        assert!(is_registered_tool(name), "{name}");
    }
    // `structured_output` is deliberately not catalog-visible, so the loop above cannot reach it; naming it here turns a forgotten
    // registry entry from a silently mis-rendered timeline into a failure. The retired agent tools are not in the catalog either,
    // and saved conversations still carry their cards.
    for name in ["subagent", "subagent_update", "structured_output", "subagent_activity", "update", "agent_send", "send_message", "followup_task"] {
        assert!(is_registered_tool(name), "{name}");
        assert_eq!(tool_surface_for_name(name), ToolSurface::Group);
    }
    assert_eq!(tool_surface_for_name("ask_user"), ToolSurface::Question);
    // A running workflow owns a progress card of its own; a workflow step is an ordinary agent-run row.
    assert_eq!(tool_surface_for_name("workflow"), ToolSurface::Workflow);
    assert_eq!(tool_surface_for_name("workflow_step"), ToolSurface::Group);
    for name in ["agent_spawn", "agent_send", "send_message", "followup_task", "task_wait", "task_list", "task_stop", "plan", "exit_plan_mode"] {
        assert_eq!(tool_surface_for_name(name), ToolSurface::Group);
    }
    assert_eq!(tool("plan", json!({ "action": "read" }), "{}").zh().title, "已读取计划");
    assert_eq!(tool("plan", json!({ "action": "write", "content": "# x" }), "{}").zh().title, "已更新计划");
    // A saved `send_message` carries two contracts under one wire name. The child form has no `target` because its one recipient
    // was the main agent, so it is a note travelling upward rather than a row that opens a child transcript.
    let downward = tool("send_message", json!({ "target": "helper", "message": "补充" }), "已排队").zh();
    assert_eq!(downward.family, ToolViewFamily::AgentRun);
    assert_eq!(downward.title, "消息已加入子代理队列");
    assert_eq!(downward.target.as_deref(), Some("helper"));
    let upward = tool("send_message", json!({ "message": "已定位到根因" }), "已排队").zh();
    assert_eq!(upward.family, ToolViewFamily::AgentNote);
    assert_eq!(upward.title, "向主代理发送了消息");
    assert_eq!(upward.target.as_deref(), Some("已定位到根因"));
    // The main agent's own tool names its recipient with `to`; it is neither of the retired shapes. While it only queued (or was
    // refused) it is a note.
    let sent = tool("send_message", json!({ "to": "helper", "message": "补充并发风险", "summary": "并发风险" }), "Message queued for helper; it reads it before its next model request.").zh();
    assert_eq!(sent.family, ToolViewFamily::AgentNote);
    assert_eq!(sent.title, "发送了消息");
    assert_eq!(sent.target.as_deref(), Some("→ helper · 并发风险"));
    assert_eq!(tool_surface_for_name("future_tool"), ToolSurface::Group);
}

#[test]
fn builds_natural_presentations_and_a_semantic_multi_tool_summary() {
    let created = card("write", json!({ "path": "src/new.ts", "content": "new\n" }), "已写入", true, Some("--- /dev/null\n+++ src/new.ts\n@@ -0,0 +1 @@\n+new\n"));
    let shell = tool("powershell", json!({ "command": "npm test" }), "ok");
    let browser = tool("preview_click", json!({ "selector": "button.primary" }), "Successfully clicked: button.primary");
    let presentation = created.zh();
    assert_eq!(presentation.title, "已创建：new.ts");
    assert_eq!(presentation.counts, Some((1, 0)));
    assert_eq!(presentation.target.as_deref(), Some("src/new.ts"));
    assert_eq!(presentation.stat.as_deref(), Some("+1 −0"));
    assert_eq!(presentation.family, ToolViewFamily::Diff);
    // The heading counts buckets, not calls, and states them in importance order regardless of the order the block ran them in.
    let kinds = |cards: &[&Card]| cards.iter().map(|c| tool_summary_kind(&c.view())).collect::<Vec<_>>();
    assert_eq!(summarize_block_kinds_in(&kinds(&[&created, &shell, &browser]), Lang::Zh), "运行了 1 个命令，1 次文件操作，执行了 1 次浏览器操作");
    // A click or fill may name its element by the uid a snapshot printed instead of a selector.
    assert_eq!(browser.zh().target.as_deref(), Some("button.primary"));
    assert_eq!(tool("preview_click", json!({ "uid": 12 }), "Successfully clicked: uid 12").zh().target.as_deref(), Some("uid 12"));
    assert_eq!(tool("preview_fill", json!({ "uid": "7", "value": "x" }), "Successfully filled: uid 7").zh().target.as_deref(), Some("uid 7"));
    // Agent calls are ordinary rows now, so they count toward the block summary.
    let spawn = tool("agent_spawn", json!({ "label": "审查" }), "已派生");
    let read = tool("read", json!({ "path": "src/a.ts" }), "内容");
    assert_eq!(summarize_block_kinds_in(&kinds(&[&spawn, &read]), Lang::Zh), "调用了 1 个子代理，读取了 1 个文件");
    // Reasoning and hooks share the block, so they share its vocabulary even though neither is a tool call with a registry entry.
    use SummaryKind as K;
    assert_eq!(summarize_block_kinds_in(&[K::Reasoning, K::Hook, K::Fork, K::Skill], Lang::Zh), "请求了 1 次会话分叉，触发了 1 个 hook，读取了 1 个技能，思考了 1 次");
    // Repeats collapse into one counted clause per bucket.
    assert_eq!(summarize_block_kinds_in(&[K::Reads, K::Files, K::Reads, K::Commands, K::Mcp], Lang::Zh), "运行了 1 个命令，调用了 1 个 MCP 工具，读取了 2 个文件，检查了 1 次文件与目录");
    // A lone bucket is still counted; there is no special single-item phrasing.
    assert_eq!(summarize_block_kinds_in(&[K::Files], Lang::Zh), "检查了 1 次文件与目录");
    // An empty block never reaches a heading, so the summary makes no claim.
    assert_eq!(summarize_block_kinds_in(&[], Lang::Zh), "");
}

#[test]
fn buckets_every_row_by_what_it_changed_not_by_which_family_renders_it() {
    use SummaryKind as K;
    let buckets: Vec<(&str, Value, SummaryKind)> = vec![
        ("powershell", json!({ "command": "npm test" }), K::Commands),
        ("pwsh", json!({ "command": "npm test" }), K::Commands),
        ("bash", json!({ "command": "ls" }), K::Commands),
        ("write", json!({ "path": "a.ts", "content": "x" }), K::FileChanges),
        ("edit", json!({ "path": "a.ts", "find": "x", "replace": "y" }), K::FileChanges),
        // A workflow is subagent work under another name, so it counts as agents.
        ("workflow", json!({ "name": "release" }), K::Agents),
        ("workflow_step", json!({ "label": "build" }), K::Agents),
        ("agent_spawn", json!({ "name": "reviewer", "prompt": "审查" }), K::Agents),
        // A retired child-to-main message, as saved conversations hold it.
        ("send_message", json!({ "message": "已定位" }), K::Agents),
        ("send_message", json!({ "to": "reviewer", "message": "补充" }), K::Agents),
        ("task_stop", json!({ "task": "shell:1" }), K::Agents),
        // `fork` and `skill` each own a bucket rather than falling into `other`.
        ("fork", json!({ "prompt": "继续" }), K::Fork),
        ("skill", json!({ "name": "pdf" }), K::Skill),
        ("read_project_memory", json!({ "name": "build" }), K::Memory),
        ("grep", json!({ "pattern": "TODO" }), K::Search),
        ("web_search", json!({ "query": "tauri" }), K::Search),
        // Reading a file is its own bucket; listing and finding stay `files`.
        ("read", json!({ "path": "a.ts" }), K::Reads),
        ("ls", json!({ "path": "." }), K::Files),
        ("find", json!({ "query": "*.ts" }), K::Files),
        ("preview_click", json!({ "selector": "button" }), K::Browser),
        // A remote screen is not a page: its pixel batch and its screen tools are device actions.
        ("preview_input", json!({ "actions": [{ "action": "key", "key": "Back" }] }), K::Device),
        ("android_shell", json!({ "command": "ls" }), K::Device),
        ("screen_app", json!({ "action": "launch", "app": "com.example.app" }), K::Device),
        ("preview_zoom", json!({ "x": 0, "y": 0, "width": 10, "height": 10 }), K::Device),
        // Unregistered tools, and registered ones that changed nothing outside the conversation, share the catch-all bucket.
        ("plan", json!({ "action": "read" }), K::Other),
        ("ask_user", json!({ "questions": [] }), K::Other),
        ("future_tool", json!({ "value": 1 }), K::Other),
        (MCP_WIRE_NAME, json!({ "selector": "button" }), K::Mcp),
    ];
    for (name, input, kind) in buckets {
        assert_eq!(tool_summary_kind(&tool(name, input, "ok").view()), kind, "{name}");
    }
}

#[test]
fn names_mcp_calls_by_the_server_and_tool_the_user_installed() {
    let call = tool(MCP_WIRE_NAME, json!({ "selector": "button.primary" }), "clicked");
    let view = call.view();
    let named = |server: &str, name: &str| (Some(server.to_owned()), name.to_owned());
    // The descriptor's label is the host's own words for both halves.
    assert_eq!(mcp_tool_naming(&view, Some(&mcp_descriptor("MCP Playwright / browser_click", MCP_WIRE_NAME, "mcp"))), named("Playwright", "browser_click"));
    // A trailing parenthetical is the manual-confirmation notice; it belongs in the approval dock, never in the row's name.
    assert_eq!(mcp_tool_naming(&view, Some(&mcp_descriptor("MCP Playwright / browser_click (需要手动确认)", MCP_WIRE_NAME, "mcp"))), named("Playwright", "browser_click"));
    // Without a descriptor the wire name still reads, minus the server slug's trailing digest.
    assert_eq!(mcp_tool_naming(&view, None), named("playwright", "browser_click"));
    // A name too short to split names only the tool, and claims no server.
    assert_eq!(mcp_tool_naming(&tool("mcp__orphan", json!({}), "ok").view(), None), (None, "mcp__orphan".to_owned()));

    assert!(is_mcp_tool_name(MCP_WIRE_NAME));
    assert!(is_mcp_tool_name("mcp__orphan"));
    for ordinary in ["read", "bash", "mcp_legacy", "preview_click", "not_mcp__tool", ""] {
        assert!(!is_mcp_tool_name(ordinary), "{ordinary}");
    }

    // A row is one line, so it shows the tool's name: the wire name for built-ins, and for MCP the tool's own half of the label.
    assert_eq!(tool_row_name(&tool("read", json!({ "path": "a.ts" }), "内容").view(), None), "read");
    assert_eq!(tool_row_name(&tool("future_tool", json!({}), "ok").view(), None), "future_tool");
    assert_eq!(tool_row_name(&view, None), "browser_click");
    assert_eq!(tool_row_name(&view, Some(&mcp_descriptor("MCP Playwright / browser_click (需要手动确认)", MCP_WIRE_NAME, "mcp"))), "browser_click");
}

#[test]
fn presents_an_mcp_call_as_a_plugged_in_tool_rather_than_an_unknown_one() {
    let call = tool(MCP_WIRE_NAME, json!({ "selector": "button.primary", "timeout": 500 }), "clicked");
    let descriptor = mcp_descriptor("MCP Playwright / browser_click", MCP_WIRE_NAME, "mcp");
    let presented = get_tool_presentation_in(&call.view(), Some(&descriptor), Lang::Zh, None, None);
    assert_eq!(presented.surface, ToolSurface::Group);
    assert_eq!(presented.family, ToolViewFamily::Raw);
    assert_eq!(presented.icon, "Plug");
    assert_eq!(presented.title, "调用了 MCP 工具 browser_click");
    // The first scalar argument stands for the call.
    assert_eq!(presented.target.as_deref(), Some("button.primary"));
    assert_eq!(presented.stat.as_deref(), Some("12 ms"));
    // The wire-name fallback keeps the row readable with no descriptor at all.
    let bare = call.zh();
    assert_eq!((bare.icon, bare.title.as_str(), bare.target.as_deref()), ("Plug", "调用了 MCP 工具 browser_click", Some("button.primary")));
    // With no arguments to show, the row names the server it reached.
    assert_eq!(tool(MCP_WIRE_NAME, json!({}), "ok").zh().target.as_deref(), Some("playwright"));

    let running = call.clone().running();
    let bad = call.with_success(false);
    assert_eq!(running.zh().title, "正在调用 MCP 工具 browser_click");
    assert_eq!(bad.zh().title, "MCP 工具 browser_click 调用失败：clicked");
    assert_eq!(bad.with_output("").zh().title, "MCP 工具 browser_click 调用失败");

    let notice = mcp_descriptor("MCP Playwright / browser_click (requires manual confirmation)", MCP_WIRE_NAME, "mcp");
    let en = |item: &Card| get_tool_presentation_in(&item.view(), Some(&notice), Lang::En, None, None).title;
    assert_eq!(en(&call), "Called MCP tool browser_click");
    assert_eq!(en(&running), "Calling MCP tool browser_click");
    assert_eq!(en(&bad), "MCP tool browser_click failed: clicked");

    // Only `mcp__` names take that branch; other unregistered tools keep the generic wrench so a plug never claims a server that
    // does not exist.
    let future = get_tool_presentation_in(&tool("future_tool", json!({}), "ok").view(), Some(&mcp_descriptor("Future tool", "future_tool", "shell")), Lang::Zh, None, None);
    assert_eq!((future.icon, future.title.as_str()), ("Wrench", "使用了 Future tool"));
}

#[test]
fn localizes_built_in_titles_statistics_and_summaries_without_translating_tool_payloads() {
    use SummaryKind as K;
    let listing = tool("ls", json!({ "path": "用户目录" }), "a.txt\nb.txt\n");
    let presented = listing.en();
    assert_eq!((presented.title.as_str(), presented.target.as_deref(), presented.stat.as_deref()), ("Viewed directory", Some("用户目录"), Some("2 items")));
    let shell = tool("powershell", json!({ "command": "Write-Output 完成" }), "完成");
    assert_eq!(summarize_block_kinds_in(&[tool_summary_kind(&listing.view())], Lang::En), "Checked files and directories once");
    assert_eq!(summarize_block_kinds_in(&[tool_summary_kind(&listing.view()), tool_summary_kind(&shell.view())], Lang::En), "Ran 1 command, Checked files and directories once");
    // Every bucket has a singular English form; the plural is not a fallback.
    assert_eq!(
        summarize_block_kinds_in(&[K::FileChanges, K::Agents, K::Mcp, K::Browser, K::Device, K::Fork, K::Hook, K::Skill, K::Reads, K::Reasoning], Lang::En),
        ["1 file change", "Called 1 subagent", "Called 1 MCP tool", "Performed 1 browser action", "Performed 1 device action", "Requested 1 conversation fork", "Triggered 1 hook", "Loaded 1 skill", "Read 1 file", "Thought once"].join(", ")
    );
    assert_eq!(summarize_block_kinds_in(&[K::Reads, K::Reads, K::FileChanges, K::FileChanges], Lang::En), "2 file changes, Read 2 files");
    // The same block in the other locale joins its clauses with a full-width comma, in the identical order.
    assert_eq!(summarize_block_kinds_in(&[K::Reads, K::Reads, K::FileChanges, K::FileChanges], Lang::Zh), "2 次文件操作，读取了 2 个文件");
}

#[test]
fn hoists_only_a_call_in_flight_and_only_ordinary_ones_past_their_start() {
    use StreamingToolPhase as P;
    let read = tool("read", json!({ "path": "a.ts" }), "");
    assert!(!is_hoisted_tool_call(&read.view()));
    assert!(is_hoisted_tool_call(&read.clone().running().view()));
    assert!(!is_hoisted_tool_call(&read.clone().streaming(P::Completed).view()));
    for name in ["ask_user", "workflow"] {
        let call = tool(name, json!({}), "");
        assert!(is_hoisted_tool_call(&call.clone().streaming(P::Announced).view()), "{name}");
        assert!(is_hoisted_tool_call(&call.clone().streaming(P::Ready).view()), "{name}");
        assert!(!is_hoisted_tool_call(&call.clone().running().view()), "{name}");
    }
}

#[test]
fn says_what_a_call_in_flight_is_doing() {
    use StreamingToolPhase as P;
    let label = |status| running_label_in(&tool("read", json!({}), "").streaming(status).view(), Lang::Zh);
    assert_eq!(label(P::Announced), "正在准备参数");
    assert_eq!(label(P::Ready), "等待执行");
    assert_eq!(label(P::Running), "正在执行");
    assert_eq!(running_label_in(&tool("read", json!({}), "").streaming(P::Ready).view(), Lang::En), "Waiting to run");
    assert_eq!(tool("read", json!({}), "").running().detail().body, DetailBody::Running("正在执行".to_owned()));
}

// ---------------------------------------------------------------- ordinary detail renderers

#[test]
fn renders_ls_find_lists_and_grep_matches_as_plain_lines() {
    let listed = tool("ls", json!({ "path": "." }), "src/\nsrc/App.tsx\n").detail();
    let texts: Vec<&str> = entries_of(&listed).iter().map(|entry| &*entry.text).collect();
    assert_eq!(texts, ["src/", "src/App.tsx"]);
    assert!(matches!(&listed.body, DetailBody::Entries { label, .. } if label == "文件结果"));

    // What the tool says about its answer, rather than an entry of it, steps back as a note.
    let found = tool("find", json!({ "query": "*.tsx" }), "src/App.tsx\n(Showing 6 of 14 files)").detail();
    let notes: Vec<bool> = entries_of(&found).iter().map(|entry| entry.note).collect();
    assert_eq!(notes, [false, true]);

    let matched = tool("grep", json!({ "pattern": "TODO", "output_mode": "content" }), "src/a.ts:12:// TODO: fix\nsrc/b.ts:7:TODO\n… 3 more matches").detail();
    assert!(matches!(&matched.body, DetailBody::Lines { label: Some(label), .. } if label == "搜索结果"));
    let text = lines_of(&matched);
    // A line reads as the tool printed it, with where it was found set back from what was found.
    assert_eq!(text.line_texts(), ["src/a.ts:12:// TODO: fix", "src/b.ts:7:TODO", "… 3 more matches"]);
    let lines = text.lines();
    assert_eq!(lines[0], [("src/a.ts:12:".to_owned(), LineTone::Dim), ("// TODO: fix".to_owned(), LineTone::Plain)]);
    assert_eq!(lines[1][0], ("src/b.ts:7:".to_owned(), LineTone::Dim));
    // A line that is not a match has no location to set back; it is a note.
    assert_eq!(lines[2], [("… 3 more matches".to_owned(), LineTone::Note)]);
}

#[test]
fn sets_a_type_icon_ahead_of_a_listed_file_a_folder_ahead_of_a_directory_nothing_ahead_of_a_remark() {
    let glyph = |entry: &ListEntry| match &entry.icon {
        EntryIcon::Folder => Some("folder".to_owned()),
        EntryIcon::File(path) => Some(file_icon_name(path).to_owned()),
        EntryIcon::None => None,
    };
    let listed = ["src/", "target/ (ignored)", "src/App.tsx", "dist/bundle.js (ignored)", "(Showing 4 of 9 files)", "… listing cut at the 40000-character limit", "[skipped] locked.db: unreadable"];
    let model = tool("ls", json!({ "path": "." }), &listed.join("\n")).detail();
    let entries = entries_of(&model);
    let icons: Vec<Option<String>> = entries.iter().map(glyph).collect();
    let some = |name: &str| Some(name.to_owned());
    assert_eq!(icons, [some("folder"), some("folder"), some("react_ts"), some("javascript"), None, None, None]);
    // An icon is no part of what the line says: it still reads, and copies, as the tool printed it.
    let texts: Vec<&str> = entries.iter().map(|entry| &*entry.text).collect();
    assert_eq!(texts, listed);

    let matched = tool("grep", json!({ "pattern": "needle" }), "src/a.rs\nREADME.md\n… more matches follow.").detail();
    let icons: Vec<Option<String>> = entries_of(&matched).iter().map(glyph).collect();
    assert_eq!(icons, [some("rust"), some(file_icon_name("README.md")), None]);
    assert_eq!(file_icon_name("README.md"), "readme");
}

#[test]
fn reads_a_tools_own_sentence_for_finding_nothing_as_that_not_as_one_entry_found() {
    for (name, input, answer) in [("find", json!({ "query": "*.rs" }), "No matching files"), ("ls", json!({ "path": "empty" }), "(empty directory)"), ("grep", json!({ "pattern": "nowhere" }), "No matches found")] {
        let item = tool(name, input, answer);
        assert_eq!(empty_of(&item.detail()), answer);
        assert!(item.zh().stat.is_some_and(|stat| stat.starts_with("0 ")), "{name}");
    }
    assert_eq!(empty_of(&tool("ls", json!({ "path": "x" }), "").detail()), "没有文件");
    assert_eq!(empty_of(&tool("grep", json!({ "pattern": "x" }), "").detail()), "未找到匹配内容");
}

const GREP_HINT: &str = "… more matches follow. Showing 1–3; pass offset=3 for the next page, or narrow the pattern or path.";

#[test]
fn lists_the_files_a_search_matched_as_paths_whatever_a_path_happens_to_look_like() {
    // A path may hold `:12:` or `-3-`; in this mode a line is a path and nothing else.
    let listed = ["src/lib/glassPlate.ts", "src/odd:12:name.ts", "docs/2024-10-13-notes.md", "src/a-3-b.ts", GREP_HINT, "[skipped] src/c.bin: unreadable"];
    // Named or not, the mode is the default one.
    for input in [json!({ "pattern": "needle" }), json!({ "pattern": "needle", "output_mode": "files_with_matches" }), json!({ "pattern": "needle", "output_mode": "" })] {
        let item = tool("grep", input, &listed.join("\n"));
        let model = item.detail();
        let entries = entries_of(&model);
        let texts: Vec<&str> = entries.iter().map(|entry| &*entry.text).collect();
        assert_eq!(texts, listed);
        // No location is set back from a path: every one is a plain entry. The tool's own lines about its answer stay notes.
        let notes: Vec<bool> = entries.iter().map(|entry| entry.note).collect();
        assert_eq!(notes, [false, false, false, false, true, true]);
        let presented = item.zh();
        assert_eq!((presented.note.as_deref(), presented.stat.as_deref()), (Some("4+ 个文件"), Some("4 个文件")));
    }
}

#[test]
fn sets_context_lines_back_from_matching_lines_and_keeps_each_group_apart() {
    let output = [
        "src/a.rs:12:fn needle() {}",
        "src/a.rs-13-    body();",
        "--",
        // The date in the path and the clock in the text both look like a location.
        "docs/2024-10-13-notes.md-4-before it, 10:30:15",
        "docs/2024-10-13-notes.md:5:needle at 10:30:15",
        "--",
        "src/b.rs:3:needle",
        GREP_HINT,
        "[skipped] src/c.bin: unreadable",
    ];
    let item = tool("grep", json!({ "pattern": "needle", "output_mode": "content", "context": 1 }), &output.join("\n"));
    let model = item.detail();
    let text = lines_of(&model);
    assert_eq!(text.line_texts(), output);
    let lines = text.lines();
    let part = |s: &str, tone| (s.to_owned(), tone);
    assert_eq!(lines[0], [part("src/a.rs:12:", LineTone::Dim), part("fn needle() {}", LineTone::Plain)]);
    // A context line is stepped back as a whole, and its location further still.
    assert_eq!(lines[1], [part("src/a.rs-13-", LineTone::Note), part("    body();", LineTone::Dim)]);
    assert_eq!(lines[2], [part("--", LineTone::Note)]);
    assert_eq!(lines[3], [part("docs/2024-10-13-notes.md-4-", LineTone::Note), part("before it, 10:30:15", LineTone::Dim)]);
    assert_eq!(lines[4], [part("docs/2024-10-13-notes.md:5:", LineTone::Dim), part("needle at 10:30:15", LineTone::Plain)]);
    assert_eq!(lines[6][0], part("src/b.rs:3:", LineTone::Dim));
    assert_eq!(lines[7], [part(GREP_HINT, LineTone::Note)]);
    assert_eq!(lines[8], [part("[skipped] src/c.bin: unreadable", LineTone::Note)]);
    // Only the matching lines are counted, never the lines around them.
    let presented = item.zh();
    assert_eq!((presented.note.as_deref(), presented.stat.as_deref()), (Some("3+ 处匹配"), Some("3 处匹配")));
}

#[test]
fn reads_a_context_line_by_the_groups_file_when_only_one_side_of_a_match_was_asked_for() {
    // `after` alone, and no `--` because there is a single group.
    let item = tool("grep", json!({ "pattern": "needle", "output_mode": "content", "after": 1 }), "src/a-1-b.ts:7:needle\nsrc/a-1-b.ts-8-next: 12:30");
    let model = item.detail();
    let lines = lines_of(&model).lines();
    assert_eq!(lines[0][0], ("src/a-1-b.ts:7:".to_owned(), LineTone::Dim));
    assert_eq!(lines[1][0], ("src/a-1-b.ts-8-".to_owned(), LineTone::Note));
    assert_eq!(item.zh().stat.as_deref(), Some("1 处匹配"));
}

#[test]
fn reads_content_lines_with_no_context_as_matches_only_even_where_a_path_holds_a_number() {
    let item = tool("grep", json!({ "pattern": "needle", "output_mode": "content" }), "src/a-3-b.ts:7:needle\nsrc/b.ts:2:x-4-y");
    let model = item.detail();
    let lines = lines_of(&model).lines();
    assert_eq!(lines[0][0].0, "src/a-3-b.ts:7:");
    assert_eq!(lines[1][0].0, "src/b.ts:2:");
    assert_eq!(item.zh().stat.as_deref(), Some("2 处匹配"));
}

#[test]
fn still_reads_a_search_recorded_before_output_mode_existed_as_the_matching_lines_it_printed() {
    // No mode was named and every entry is `path:line:text`: the old transcripts.
    let old = tool("grep", json!({ "pattern": "needle" }), "src/a.ts:7:needle\nsrc/b.ts:2:more needle\n… more matches follow.");
    let model = old.detail();
    assert_eq!(lines_of(&model).lines()[0][0].0, "src/a.ts:7:");
    assert_eq!(old.zh().note.as_deref(), Some("2+ 处匹配"));
    // A mode that was named is never second-guessed, and a mixed answer is a list of files.
    assert_eq!(tool("grep", json!({ "pattern": "needle", "output_mode": "files_with_matches" }), "src/a.ts:7:needle").zh().stat.as_deref(), Some("1 个文件"));
    assert_eq!(tool("grep", json!({ "pattern": "needle" }), "src/a.ts\nsrc/b:1:.ts").zh().stat.as_deref(), Some("2 个文件"));
}

#[test]
fn shows_a_count_per_file_and_the_total_as_a_note_after_the_paging_hint() {
    let output = ["src/a.rs:3", "src/odd:12:name.rs:1", GREP_HINT, "Total: 4 matching line(s) in 2 file(s).", "[skipped] src/c.bin: unreadable"];
    let item = tool("grep", json!({ "pattern": "needle", "output_mode": "count" }), &output.join("\n"));
    let model = item.detail();
    let text = lines_of(&model);
    assert_eq!(text.line_texts(), output);
    let lines = text.lines();
    assert_eq!(lines[0][0], ("src/a.rs:".to_owned(), LineTone::Dim));
    // The path is everything before the last colon.
    assert_eq!(lines[1][0], ("src/odd:12:name.rs:".to_owned(), LineTone::Dim));
    for note in &lines[2..] {
        assert_eq!(note[0].1, LineTone::Note);
    }
    let presented = item.zh();
    assert_eq!((presented.note.as_deref(), presented.stat.as_deref()), (Some("2+ 个文件"), Some("2 个文件")));
}

#[test]
fn reads_the_tools_answer_for_a_page_past_the_last_entry_as_that_in_every_mode() {
    let answer = "No matches at offset 500; there are 12.";
    for input in [json!({ "pattern": "needle" }), json!({ "pattern": "needle", "output_mode": "content" }), json!({ "pattern": "needle", "output_mode": "count" })] {
        let item = tool("grep", input, answer);
        assert_eq!(empty_of(&item.detail()), answer);
        assert!(item.zh().stat.is_some_and(|stat| stat.starts_with("0 ")));
    }
}

#[test]
fn renders_read_output_on_the_diff_base_with_source_line_numbers() {
    let model = tool("read", json!({ "path": "src/a.ts", "start_line": 8 }), "first\nsecond").detail();
    let DetailBody::Read { label, path, lines } = &model.body else { panic!("not a read: {model:?}") };
    assert_eq!(label, "src/a.ts 内容");
    assert_eq!(path.as_deref(), Some("src/a.ts"));
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].number, 8);
    assert_eq!(lines[1].text, "second");
}

#[test]
fn sets_the_notes_a_read_ends_with_apart_from_the_files_own_lines() {
    let continued = "alpha\nbeta\n… showing lines 1–2 of 900. Continue with start_line=3.";
    let unsized_ = "alpha\nbeta\n… showing lines 1–2; the total line count was not computed because the file is large. Continue with start_line=3.";
    let lossy = "alpha\n(This file is not valid UTF-8, so it was decoded lossily: invalid bytes show as U+FFFD. `edit` cannot modify it.)";
    let utf16 = "alpha\n(This file is UTF-16LE text, decoded for this read. `edit` cannot modify it.)";
    let notes = |model: &ToolDetailModel| {
        let DetailBody::Read { lines, .. } = &model.body else { panic!("not a read: {model:?}") };
        lines.iter().map(|line| line.note).collect::<Vec<bool>>()
    };
    for output in [continued, unsized_, lossy, utf16] {
        let item = tool("read", json!({ "path": "src/a.txt" }), output);
        let kinds = notes(&item.detail());
        assert!(!kinds[0]);
        assert!(*kinds.last().expect("lines"));
        // The note is not a line of the file: the range counts what the file held.
        assert_eq!(item.zh().stat.as_deref(), Some(if output == lossy || output == utf16 { "1–1 行" } else { "1–2 行" }));
    }
    // Both notes can follow the text, in either order.
    let both = tool("read", json!({ "path": "src/a.txt" }), &format!("{continued}\n{}", utf16.split('\n').nth(1).expect("a note")));
    assert_eq!(notes(&both.detail()), [false, false, true, true]);
}

#[test]
fn answers_an_empty_file_or_a_start_past_the_end_with_the_sentence_not_a_numbered_line() {
    let empty = tool("read", json!({ "path": "src/empty.ts" }), "(The file exists but is empty)");
    assert_eq!(empty_of(&empty.detail()), "文件内容为空");
    assert_eq!(empty.zh().stat, None);

    let sentence = "(The file has only 3 lines, so the requested start_line is out of range)";
    let past = tool("read", json!({ "path": "src/a.ts", "start_line": 40 }), sentence);
    assert_eq!(empty_of(&past.detail()), sentence);
    assert_eq!(past.zh().stat, None);
    // One line is "1 line", not "1 lines".
    let one = "(The file has only 1 line, so the requested start_line is out of range)";
    assert_eq!(empty_of(&tool("read", json!({ "path": "src/a.ts", "start_line": 40 }), one).detail()), one);
}

#[test]
fn prefers_the_diff_for_successful_write_edit_calls() {
    let diff = "--- src/a.ts\n+++ src/a.ts\n@@ -1 +1 @@\n-old\n+new\n";
    let item = card("edit", json!({ "path": "src/a.ts", "find": "old", "replace": "new" }), "完成替换", true, Some(diff));
    assert_eq!(item.detail().body, DetailBody::Diff { value: diff.to_owned(), path: Some("src/a.ts".to_owned()) });
    // No diff: the tool's own line, or the sentence standing in for it.
    assert_eq!(lines_of(&tool("edit", json!({ "path": "a" }), "完成替换").detail()).line_texts(), ["完成替换"]);
    assert_eq!(lines_of(&tool("edit", json!({ "path": "a" }), "").detail()).line_texts(), ["操作成功，没有行级变化"]);
}

#[test]
fn renders_shell_input_and_terminal_output_without_interpreting_markup() {
    let model = tool("bash", json!({ "command": "printf '<tag>'" }), "<tag>\n[stderr]\nwarning").detail();
    let DetailBody::Terminal { command, output } = &model.body else { panic!("not a terminal: {model:?}") };
    assert_eq!(&**command, "printf '<tag>'");
    assert_eq!(&**output, "<tag>\n[stderr]\nwarning");
    let DetailBody::Terminal { output, .. } = tool("bash", json!({ "command": "true" }), "").detail().body else { panic!("not a terminal") };
    assert_eq!(&*output, "命令没有输出");
}

#[test]
fn draws_a_failed_command_as_the_same_terminal_card() {
    let model = failed("bash", json!({ "command": "pnpm i" }), "Exit code 127\nbash: pnpm: command not found").detail();
    let DetailBody::Terminal { command, output } = &model.body else { panic!("not a terminal: {model:?}") };
    assert_eq!(&**command, "pnpm i");
    assert!(output.contains("bash: pnpm: command not found"));
}

#[test]
fn opens_a_failed_call_that_has_no_result_to_picture_as_its_arguments_and_the_error() {
    // A read that failed has no lines to number, an edit no diff, a listing no files.
    let read = failed("read", json!({ "path": "missing.ts", "start_line": 40 }), "文件不存在").kv();
    let rows: Vec<(&str, &str)> = read.rows.iter().map(|(k, v)| (k.as_str(), &**v)).collect();
    assert_eq!(rows, [("path", "missing.ts"), ("start_line", "40")]);
    assert_eq!(read.result.as_deref(), Some("文件不存在"));
    // Not the success line an edit with no diff prints.
    assert_eq!(failed("edit", json!({ "path": "src/App.tsx", "find": "old" }), "The exact text to replace was not found").kv().result.as_deref(), Some("The exact text to replace was not found"));
    assert_eq!(failed("ls", json!({ "path": "nope" }), "目录不存在").kv().result.as_deref(), Some("目录不存在"));
}

#[test]
fn lays_a_curated_call_out_as_the_arguments_worth_reading_and_then_its_result() {
    // `web_search` draws its own card only when its result is the envelope the host emits. Output that is not that envelope falls
    // back to this shared card rather than rendering an empty one. `maxResults` is plumbing nobody opened the card to read.
    let card = tool("web_search", json!({ "query": "tauri", "maxResults": 3 }), "1. Tauri — https://tauri.app").kv();
    assert_eq!(card.labels(), ["query"]);
    assert_eq!(card.get("query"), Some("tauri"));
    assert_eq!(card.result.as_deref(), Some("1. Tauri — https://tauri.app"));
}

#[test]
fn draws_one_chip_per_site_a_native_search_consulted_and_counts_them_in_the_row_title() {
    let item = tool(
        "web_search",
        json!({ "query": "provider-executed web search" }),
        &json!({
            "findings": "Anthropic seals search result text in encrypted_content.",
            "sources": [
                { "url": "https://docs.anthropic.com/en/docs/web-search-tool", "title": "Web search tool" },
                { "url": "https://www.docs.anthropic.com/en/other", "title": "Another page" },
                { "url": "https://platform.openai.com/docs/guides/tools-web-search", "title": "Web search" }
            ],
            "untrustedWebContent": true
        })
        .to_string(),
    );
    let model = item.detail();
    let DetailBody::WebSearch { chips, findings, label, .. } = &model.body else { panic!("not a search: {model:?}") };
    // Two pages of one site are one chip: the row answers "where did this come from", and `www.` never distinguishes two sources.
    let hosts: Vec<&str> = chips.iter().map(|chip| chip.host.as_str()).collect();
    assert_eq!(hosts, ["docs.anthropic.com", "platform.openai.com"]);
    assert_eq!(chips[0].url, "https://docs.anthropic.com/en/docs/web-search-tool");
    assert_eq!(chips[0].title.as_deref(), Some("Web search tool"));
    assert!(findings.as_deref().is_some_and(|f| f.contains("Anthropic seals search result text")));
    assert_eq!(label, "搜索到的网站");
    // The collapsed row says how many sites, not "completed a web search".
    assert_eq!(item.zh().title, "已搜索 2 个网站");
    assert_eq!(item.en().title, "Searched 2 sites");
}

#[test]
fn builds_the_same_chips_from_a_catalog_providers_result_array() {
    let item = tool("web_search", json!({ "query": "tauri" }), &json!({ "results": [{ "id": "ab-1", "title": "Tauri", "url": "https://tauri.app/start", "content": "Build smaller apps." }], "untrustedWebContent": true }).to_string());
    let DetailBody::WebSearch { chips, results, findings, .. } = item.detail().body else { panic!("not a search") };
    assert_eq!(chips[0].host, "tauri.app");
    assert_eq!(findings, None);
    assert_eq!(results[0].title, "Tauri");
    assert_eq!(results[0].content.as_deref(), Some("Build smaller apps."));
}

#[test]
fn stands_in_a_lettered_placeholder_for_a_sites_icon() {
    let item = tool("web_search", json!({ "query": "x" }), &json!({ "findings": "f", "sources": [{ "url": "https://example.com/a" }] }).to_string());
    assert_eq!(source_letter(&web_source_chips(&item.view())[0].host), "E");
}

#[test]
fn leaves_a_url_it_cannot_read_or_a_scheme_that_is_no_site_off_the_chips() {
    let item = tool("web_search", json!({}), &json!({ "sources": [{ "url": "not a url" }, { "url": "ftp://example.com/a" }, { "url": "" }, { "title": "no url" }, 7, { "url": " HTTPS://Example.COM:8443/a " }] }).to_string());
    let chips = web_source_chips(&item.view());
    assert_eq!(chips.len(), 1);
    assert_eq!(chips[0].host, "example.com");
}

#[test]
fn keeps_fetching_on_the_ordinary_card_because_its_urls_are_already_in_the_call() {
    assert_eq!(tool("web_fetch", json!({ "urls": ["https://a.example"] }), "x").zh().family, ToolViewFamily::Raw);
    assert_eq!(tool("web_search", json!({ "query": "x" }), "x").zh().family, ToolViewFamily::WebSearch);
    // Titles show the first URL and summarize later URLs.
    assert_eq!(tool("web_fetch", json!({ "urls": [" ", "https://a.example", "https://b.example"] }), "x").zh().target.as_deref(), Some("https://a.example +2"));
}

#[test]
fn shows_every_argument_of_a_tool_whose_arguments_nobody_has_curated() {
    // An uncurated argument is still the reader's only account of the call, so an unknown tool shows all of them rather than none.
    // `JsonObject` keeps its keys sorted here where upstream keeps the order they came in.
    let card = tool("future_tool", json!({ "value": 1, "mode": "fast" }), "ok").kv();
    assert_eq!(card.labels(), ["mode", "value"]);
    assert_eq!(card.get("value"), Some("1"));
    assert_eq!(card.result.as_deref(), Some("ok"));
}

// ---------------------------------------------------------------- preview tool families

#[test]
fn renders_the_accessibility_tree_a_snapshot_answered_with_as_text() {
    let tree = "- document\n  - button \"Continue\" [uid=e1]\n  - link \"Docs\" [uid=e2]";
    let item = tool("preview_snapshot", json!({}), tree);
    let model = item.detail();
    assert!(matches!(&model.body, DetailBody::Lines { label: Some(label), .. } if label == "页面快照"));
    assert!(lines_of(&model).text.contains("button \"Continue\" [uid=e1]"));
    let presented = item.zh();
    assert_eq!((presented.family, presented.title.as_str(), presented.stat.as_deref()), (ToolViewFamily::BrowserSnapshot, "读取了页面快照", Some("3 个节点")));
    assert_eq!(empty_of(&tool("preview_snapshot", json!({}), " ").detail()), "快照中没有可读内容");
}

#[test]
fn renders_the_evaluated_expression_beside_the_value_the_page_returned() {
    let card = tool("preview_eval", json!({ "expression": "document.title" }), "\"Example\"").kv();
    assert_eq!(card.get("JavaScript"), Some("document.title"));
    assert_eq!(card.result.as_deref(), Some("\"Example\""));
    // `undefined` is a value the tool words itself, not an empty result.
    assert_eq!(tool("preview_eval", json!({ "expression": "void 0" }), "undefined").kv().result.as_deref(), Some("undefined"));
    assert_eq!(tool("preview_eval", json!({ "expression": "void 0" }), "").kv().result.as_deref(), Some("undefined"));
}

#[test]
fn renders_screenshot_dimensions_and_thumbnails_alone_when_pixels_came_back() {
    let metadata = json!({ "width": 1280, "height": 720 }).to_string();
    let item = tool("preview_screenshot", json!({}), &metadata);
    assert_eq!(item.kv().get("尺寸"), Some("1280×720"));
    assert_eq!(item.zh().stat.as_deref(), Some("1280×720"));
    // The thumbnail is the result; its metadata only repeats what the image shows.
    assert_eq!(item.with_image(&pixel("preview-20260714T000000.000Z")).detail().body, DetailBody::ImageOnly);
}

#[test]
fn gives_an_uploaded_conversation_image_the_browser_duration_never_its_number() {
    for image_id in [json!(3), json!("#3"), json!("[Image #3]")] {
        assert_eq!(tool("preview_upload_image", json!({ "selector": "input[type=file]", "image_id": image_id }), "Uploaded").zh().stat.as_deref(), Some("12 ms"));
    }
}

#[test]
fn keeps_console_entries_as_the_tools_own_lines_tinted_by_level_with_its_footer_as_a_note() {
    let footer = "(Showing last 2 of 9 entries. Use 'lines' parameter (max 200) to see more.)";
    let model = tool("preview_console_logs", json!({ "level": "all" }), &format!("[error] boom\n[log] ready\n\n{footer}")).detail();
    assert!(matches!(&model.body, DetailBody::Lines { label: Some(label), .. } if label == "Console 日志"));
    let text = lines_of(&model);
    // Each line verbatim, level tag included; the blank line between them is not an entry.
    assert_eq!(text.line_texts(), ["[error] boom", "[log] ready", footer]);
    let lines = text.lines();
    assert_eq!(lines[0][0].1, LineTone::Error);
    assert_eq!(lines[1][0].1, LineTone::Plain);
    // The footer is not lifted out of the list: it is its last line, stepped back as a note.
    assert_eq!(lines[2][0].1, LineTone::Note);
    assert_eq!(lines_of(&tool("preview_console_logs", json!({}), "[WARNING] x").detail()).lines()[0][0].1, LineTone::Warning);

    // The tool's own empty answer is not one entry.
    let none = tool("preview_console_logs", json!({}), "No console logs.");
    assert_eq!(empty_of(&none.detail()), "没有 Console 日志");
    assert_eq!(none.zh().stat.as_deref(), Some("0 条"));
}

#[test]
fn keeps_the_network_ledger_verbatim_tints_a_failed_request_and_leaves_a_fetched_body_as_text() {
    let ok = "[req-1] GET https://example.com/api → 200 OK";
    let bad = "[req-2] POST https://example.com/save [FAILED: net::ERR_ABORTED]";
    let model = tool("preview_network", json!({ "filter": "all" }), &format!("{ok}\n{bad}")).detail();
    assert!(matches!(&model.body, DetailBody::Lines { label: Some(label), .. } if label == "网络请求"));
    let text = lines_of(&model);
    // The id a line leads with is what a later call asks for its body by, so it stays.
    assert_eq!(text.line_texts(), [ok, bad]);
    let lines = text.lines();
    assert_eq!(lines[0][0].1, LineTone::Plain);
    assert_eq!(lines[1][0].1, LineTone::Error);
    // A `[FAILED:` set straight after the URL is still the failure, not part of the URL.
    let joined = lines_of(&tool("preview_network", json!({}), "[r] GET http://x[FAILED: boom]").detail()).lines();
    assert_eq!(joined[0][0].1, LineTone::Error);

    let body = tool("preview_network", json!({ "requestId": "req-1" }), "{\"ok\":true}").detail();
    assert!(matches!(&body.body, DetailBody::Lines { label: None, .. }));
    assert_eq!(lines_of(&body).line_texts(), ["{\"ok\":true}"]);
    assert_eq!(empty_of(&tool("preview_network", json!({ "requestId": "req-1" }), "").detail()), "没有响应正文");
    assert_eq!(empty_of(&tool("preview_network", json!({}), "").detail()), "没有网络记录");
    assert_eq!(empty_of(&tool("preview_network", json!({}), "No network requests recorded.").detail()), "No network requests recorded.");
}

#[test]
fn re_indents_the_compact_json_that_inspect_and_list_answer_with() {
    let inspected = tool("preview_inspect", json!({ "selector": ".button" }), "{\"tagName\":\"BUTTON\",\"id\":\"submit\"}").kv();
    assert!(inspected.result.as_deref().is_some_and(|r| r.contains("\"tagName\": \"BUTTON\"")));
    assert_eq!(inspected.get("selector"), Some(".button"));
    // Element-not-found is prose, not JSON, and stays readable.
    assert_eq!(tool("preview_inspect", json!({ "selector": ".gone" }), "Element not found: .gone").kv().result.as_deref(), Some("Element not found: .gone"));

    let servers = json!([{ "serverId": "srv-1", "name": "dev", "port": 5173, "status": "running" }]).to_string();
    let list = tool("preview_list", json!({}), &servers);
    assert!(list.kv().result.as_deref().is_some_and(|r| r.contains("\"name\": \"dev\"")));
    assert_eq!(list.zh().stat.as_deref(), Some("1 个服务器"));
}

#[test]
fn reports_where_the_page_ended_up_after_mewrks_own_two_preview_tools() {
    let report = json!({
        "answered": true,
        "page": { "url": "http://localhost:5173/", "title": "Dev", "loading": false },
        "newConsoleErrors": ["TypeError: x is not a function"],
        "notices": ["The page opened a modal state that blocks it."],
        "snapshot": { "tree": "- button \"OK\" [uid=e1]", "truncated": false }
    });
    let card = tool("preview_dialog", json!({ "accept": true }), &report.to_string()).kv();
    assert_eq!(card.get("URL"), Some("http://localhost:5173/"));
    assert_eq!(card.get("标题"), Some("Dev"));
    assert_eq!(card.get("加载中"), Some("否"));
    assert_eq!(card.get("answered"), Some("是"));
    assert_eq!(card.get("提示"), Some("The page opened a modal state that blocks it."));
    assert_eq!(card.get("新增 Console 错误"), Some("TypeError: x is not a function"));
    assert!(card.result.as_deref().is_some_and(|r| r.contains("button \"OK\"")));
    assert_eq!(tool("preview_dialog", json!({ "accept": false }), "{}").zh().stat.as_deref(), Some("取消"));
    let modal = tool("preview_dialog", json!({}), &json!({ "modalState": { "description": ["a", 1, null] } }).to_string()).kv();
    assert_eq!(modal.get("模态状态"), Some("a; 1; null"));
    let undescribed = tool("preview_dialog", json!({}), &json!({ "modalState": {} }).to_string()).kv();
    assert_eq!(undescribed.get("模态状态"), Some("undefined"));
}

#[test]
fn keeps_the_failure_message_even_when_the_result_carries_images() {
    let item = failed("read", json!({ "path": "diagram.png" }), "图片超过 5 MiB 限制").with_image(&pixel("page.png"));
    assert_eq!(item.kv().result.as_deref(), Some("图片超过 5 MiB 限制"));
}

#[test]
fn uses_a_safe_text_fallback_for_malformed_preview_json() {
    let model = tool("preview_dialog", json!({}), "{not-json").detail();
    let lines = lines_of(&model).lines();
    assert_eq!(lines[0], [("结果不是有效 JSON".to_owned(), LineTone::Note)]);
    assert_eq!(lines[1][0].0, "{not-json");
    let unrecognized = tool("preview_screenshot", json!({}), "[1]").detail();
    assert_eq!(lines_of(&unrecognized).lines()[0][0].0, "结果结构无法识别");
}

#[test]
fn renders_the_prose_the_thirteen_copied_tools_answer_with_unchanged() {
    // These tools return the source's own sentences. Re-parsing them would be inventing structure the host never sent.
    let click = tool("preview_click", json!({ "selector": "button.primary", "doubleClick": true }), "Successfully double-clicked: button.primary");
    assert_eq!(click.kv().result.as_deref(), Some("Successfully double-clicked: button.primary"));
    let presented = tool("preview_click", json!({ "selector": "button.primary", "doubleClick": true }), "ok").zh();
    assert_eq!((presented.family, presented.title.as_str(), presented.target.as_deref(), presented.stat.as_deref()), (ToolViewFamily::Raw, "点击了页面元素", Some("button.primary"), Some("双击")));
    let started = tool("preview_start", json!({ "name": "dev" }), "{\n  \"name\": \"dev\"\n}\nServer started successfully on port 5173.").kv();
    assert!(started.result.as_deref().is_some_and(|r| r.contains("Server started successfully on port 5173.")));
}

#[test]
fn presents_a_preview_input_batch_by_its_first_action_and_the_number_of_actions() {
    let batch = tool("preview_input", json!({ "actions": [{ "action": "click", "x": 100, "y": 200 }, { "action": "type", "text": "hello" }, { "action": "key", "key": "Enter" }] }), "ok");
    let presented = batch.zh();
    assert_eq!((presented.family, presented.title.as_str(), presented.target.as_deref(), presented.stat.as_deref()), (ToolViewFamily::Raw, "操作了遥控屏幕", Some("click 100,200"), Some("3 个动作")));
    // It drives a phone or a VNC desktop, not a page, so it is a device action.
    assert_eq!(tool_summary_kind(&batch.view()), SummaryKind::Device);
    let single = tool("preview_input", json!({ "actions": [{ "action": "key", "key": "Back" }] }), "ok").zh();
    assert_eq!((single.target.as_deref(), single.stat.as_deref()), (Some("key Back"), Some("1 个动作")));
    // A call whose actions never parsed still has a card.
    let unparsed = tool("preview_input", json!({}), "").zh();
    assert_eq!((unparsed.family, unparsed.title.as_str()), (ToolViewFamily::Raw, "操作了遥控屏幕"));
}

#[test]
fn says_how_much_a_pasting_type_carries_never_the_text_it_puts_on_the_clipboard() {
    let secret = "hunter2 correct horse battery staple";
    let paste = tool("preview_input", json!({ "actions": [{ "action": "type", "text": secret, "via": "paste" }, { "action": "key", "key": "Enter" }] }), "ok");
    assert_eq!(paste.zh().target, Some(format!("粘贴 {} 字", secret.len())));
    assert_eq!(paste.en().target, Some(format!("paste {} characters", secret.len())));
    assert!(!paste.zh().target.expect("a target").contains("hunter2"));
    assert_eq!(paste.zh().stat.as_deref(), Some("2 个动作"));
    let target = |input: Value| tool("preview_input", input, "ok").zh().target;
    // Characters, not UTF-16 units: a pasted emoji is one.
    assert_eq!(target(json!({ "actions": [{ "action": "type", "text": "你好🙂", "via": "paste", "chord": "cmd+v" }] })).as_deref(), Some("粘贴 3 字"));
    // The default channel types the text as it always did, and an explicit "auto" does too.
    assert_eq!(target(json!({ "actions": [{ "action": "type", "text": "hello" }] })).as_deref(), Some("type hello"));
    assert_eq!(target(json!({ "actions": [{ "action": "type", "text": "hello", "via": "auto" }] })).as_deref(), Some("type hello"));
    // A paste that is not the batch's first action does not change what the row opens with.
    assert_eq!(target(json!({ "actions": [{ "action": "click", "x": 5, "y": 6 }, { "action": "type", "text": "hello", "via": "paste" }] })).as_deref(), Some("click 5,6"));
    // A type with no text has nothing to count.
    assert_eq!(target(json!({ "actions": [{ "action": "type", "via": "paste" }] })).as_deref(), Some("type"));
}

#[test]
fn names_the_button_a_phones_dialog_was_answered_with_and_says_accept_or_dismiss_otherwise() {
    let stat = |input: Value| tool("preview_dialog", input, "ok").zh().stat.expect("a stat");
    assert_eq!(stat(json!({ "button": "Allow While Using App" })), "按下“Allow While Using App”");
    assert_eq!(tool("preview_dialog", json!({ "button": "Allow While Using App" }), "ok").en().stat.as_deref(), Some("Press \"Allow While Using App\""));
    // The button decides the row even when the call also says accept, as a phone's call may.
    assert_eq!(stat(json!({ "button": "Don’t Allow", "accept": false })), "按下“Don’t Allow”");
    // A very long label is cut like every other target.
    assert_eq!(stat(json!({ "button": "x".repeat(200) })), format!("按下“{}…”", "x".repeat(47)));
    assert_eq!(stat(json!({ "button": "   " })), "接受");
    // No button: the answer is accept or dismiss, as on a page.
    assert_eq!(stat(json!({ "accept": true })), "接受");
    assert_eq!(stat(json!({ "accept": false })), "取消");
    assert_eq!(tool("preview_dialog", json!({ "accept": false }), "ok").en().stat.as_deref(), Some("Dismiss"));
    assert_eq!(stat(json!({})), "接受");
}

#[test]
fn reads_a_phones_plain_sentence_answer_as_text_and_keeps_the_pages_json_card_for_the_page() {
    // On a phone or tablet the host answers in a sentence; the page card would call it "not valid JSON".
    let answered = tool("preview_dialog", json!({ "button": "Allow" }), "Pressed \"Allow\".");
    assert_eq!(answered.zh().family, ToolViewFamily::Raw);
    assert_eq!(answered.kv().result.as_deref(), Some("Pressed \"Allow\"."));
    // The page's answer is an object, and keeps its card.
    let page = tool("preview_dialog", json!({ "accept": true }), &json!({ "answered": true, "page": { "url": "http://localhost:5173/" } }).to_string());
    assert_eq!(page.zh().family, ToolViewFamily::BrowserPage);
    assert_eq!(page.kv().get("URL"), Some("http://localhost:5173/"));
}

// ---------------------------------------------------------------- screen tools

const SCREEN_TITLES: [(&str, [&str; 3], [&str; 3]); 8] = [
    ("preview_zoom", ["放大了屏幕局部", "正在放大屏幕局部", "放大屏幕局部失败"], ["Zoomed into screen", "Zooming into screen", "Failed to zoom into screen"]),
    ("preview_wait", ["等待了屏幕画面", "正在等待屏幕画面", "等待屏幕画面失败"], ["Waited on screen", "Waiting on screen", "Screen wait failed"]),
    ("screen_app", ["操作了屏幕应用", "正在操作屏幕应用", "操作屏幕应用失败"], ["Operated screen app", "Operating screen app", "Failed to operate screen app"]),
    ("screen_device", ["操作了屏幕设备", "正在操作屏幕设备", "操作屏幕设备失败"], ["Operated screen device", "Operating screen device", "Failed to operate screen device"]),
    ("screen_clipboard", ["操作了屏幕剪贴板", "正在操作屏幕剪贴板", "操作屏幕剪贴板失败"], ["Used screen clipboard", "Using screen clipboard", "Screen clipboard failed"]),
    ("screen_package", ["管理了应用安装包", "正在管理应用安装包", "管理应用安装包失败"], ["Managed app packages", "Managing app packages", "Failed to manage app packages"]),
    ("android_logcat", ["读取了 Android 日志", "正在读取 Android 日志", "读取 Android 日志失败"], ["Read Android logs", "Reading Android logs", "Failed to read Android logs"]),
    ("android_shell", ["运行了 Android Shell 命令", "正在运行 Android Shell 命令", "Android Shell 命令失败"], ["Ran Android shell command", "Running Android shell command", "Android shell command failed"]),
];

#[test]
fn registers_all_eight_as_plain_grouped_rows_that_count_as_device_actions() {
    for (name, _, _) in SCREEN_TITLES {
        assert!(is_registered_tool(name), "{name}");
        assert_eq!(tool_surface_for_name(name), ToolSurface::Group);
        let presented = tool(name, json!({}), "ok").zh();
        assert_eq!((presented.surface, presented.family), (ToolSurface::Group, ToolViewFamily::Raw), "{name}");
        assert_eq!(tool_summary_kind(&tool(name, json!({}), "ok").view()), SummaryKind::Device, "{name}");
    }
}

#[test]
fn states_a_block_of_device_calls_as_device_actions_in_both_languages_after_the_browser_ones() {
    use SummaryKind as K;
    let device = [K::Device; 3];
    assert_eq!(tool_summary_kind(&tool("screen_package", json!({ "action": "install", "path": "a.apk" }), "Installed a.apk").view()), K::Device);
    assert_eq!(summarize_block_kinds_in(&device, Lang::Zh), "操作了 3 次设备");
    assert_eq!(summarize_block_kinds_in(&device, Lang::En), "Performed 3 device actions");
    assert_eq!(summarize_block_kinds_in(&device[..1], Lang::Zh), "操作了 1 次设备");
    assert_eq!(summarize_block_kinds_in(&device[..1], Lang::En), "Performed 1 device action");
    // A page click still says browser; a block with both states each, the browser first.
    let mixed = [K::Device, K::Device, K::Browser];
    assert_eq!(summarize_block_kinds_in(&mixed, Lang::Zh), "执行了 1 次浏览器操作，操作了 2 次设备");
    assert_eq!(summarize_block_kinds_in(&mixed, Lang::En), "Performed 1 browser action, Performed 2 device actions");
    assert_eq!(summarize_block_kinds_in(&[K::Device, K::Commands, K::Reads], Lang::Zh), "运行了 1 个命令，操作了 1 次设备，读取了 1 个文件");
}

#[test]
fn titles_a_call_by_phase_in_both_languages_and_says_why_a_failed_one_failed() {
    for (name, zh, en) in SCREEN_TITLES {
        let (done, running, bad) = (tool(name, json!({}), "ok"), tool(name, json!({}), "ok").running(), failed(name, json!({}), ""));
        assert_eq!([done.zh().title, running.zh().title, bad.zh().title], zh, "{name}");
        assert_eq!([done.en().title, running.en().title, bad.en().title], en, "{name}");
    }
    assert_eq!(failed("screen_package", json!({ "action": "install", "path": "app.apk" }), "Failure [INSTALL_FAILED_OLDER_SDK]").zh().title, "管理应用安装包失败：Failure [INSTALL_FAILED_OLDER_SDK]");
}

#[test]
fn names_what_a_call_acted_on_from_its_main_argument() {
    let target = |name: &str, input: Value| tool(name, input, "ok").zh().target;
    let says = |text: &str| Some(text.to_owned());
    // screen_app: the action (list when none is given), then the link or the app, then what narrows it.
    assert_eq!(target("screen_app", json!({ "action": "info", "app": "com.android.settings" })), says("info com.android.settings"));
    assert_eq!(target("screen_app", json!({})), says("list"));
    assert_eq!(target("screen_app", json!({ "filter": "maps" })), says("list maps"));
    assert_eq!(target("screen_app", json!({ "action": "foreground" })), says("foreground"));
    assert_eq!(target("screen_app", json!({ "action": "launch", "app": "com.example.app", "activity": ".MainActivity" })), says("launch com.example.app .MainActivity"));
    assert_eq!(target("screen_app", json!({ "action": "launch", "app": "com.apple.Preferences" })), says("launch com.apple.Preferences"));
    assert_eq!(target("screen_app", json!({ "action": "open_url", "url": "https://example.com/a" })), says("open_url https://example.com/a"));
    assert_eq!(target("screen_app", json!({ "action": "open_url", "url": "https://example.com", "app": "com.apple.mobilesafari" })), says("open_url https://example.com com.apple.mobilesafari"));
    // screen_package: the action, then the file or the app, then the permission.
    assert_eq!(target("screen_package", json!({ "action": "install", "path": "app/build/outputs/apk/debug/app-debug.apk" })), says("install app/build/outputs/apk/debug/app-debug.apk"));
    assert_eq!(target("screen_package", json!({ "action": "grant", "app": "com.example.app", "permission": "android.permission.CAMERA" })), says("grant com.example.app android.permission.CAMERA"));
    assert_eq!(target("android_logcat", json!({ "package": "com.example.app", "level": "E" })), says("com.example.app"));
    assert_eq!(target("android_logcat", json!({ "tag": "OkHttp" })), says("OkHttp"));
    // screen_device: the action (info when none is given), then the new value or the place.
    assert_eq!(target("screen_device", json!({})), says("info"));
    assert_eq!(target("screen_device", json!({ "sections": ["device", "power"] })), says("info device, power"));
    assert_eq!(target("screen_device", json!({ "action": "rotate", "value": "landscape_left" })), says("rotate landscape_left"));
    assert_eq!(target("screen_device", json!({ "action": "appearance", "value": "dark" })), says("appearance dark"));
    assert_eq!(target("screen_device", json!({ "action": "wake" })), says("wake"));
    assert_eq!(target("screen_device", json!({ "action": "location", "latitude": 35.68, "longitude": 139.76 })), says("location 35.68,139.76"));
    assert_eq!(target("android_shell", json!({ "command": "dumpsys battery | head -5" })), says("dumpsys battery | head -5"));
    // preview_zoom names the region it cut out of the screenshot; preview_wait what it waits for.
    assert_eq!(target("preview_zoom", json!({ "x": 100.4, "y": 200, "width": 300, "height": 150.6 })), says("100,200 300×151"));
    assert_eq!(target("preview_zoom", json!({ "x": 100 })), None);
    assert_eq!(target("preview_wait", json!({})), says("change"));
    assert_eq!(target("preview_wait", json!({ "until": "still", "x": 0, "y": 0, "width": 640, "height": 480 })), says("still 0,0 640×480"));
    // A clipboard row names the direction, never what is on the clipboard.
    assert_eq!(target("screen_clipboard", json!({ "action": "write", "text": "hunter2" })), says("write"));
    assert_eq!(target("screen_clipboard", json!({ "action": "read" })), says("read"));
    assert_eq!(target("screen_clipboard", json!({})), None);
    // A call whose arguments never parsed still has a row.
    let unparsed = tool("android_shell", json!({}), "").zh();
    assert_eq!((unparsed.family, unparsed.title.as_str(), unparsed.target), (ToolViewFamily::Raw, "运行了 Android Shell 命令", None));
}

#[test]
fn states_the_figure_worth_the_row() {
    let stat = |item: Card| item.zh().stat;
    let says = |text: &str| Some(text.to_owned());
    assert_eq!(stat(tool("android_shell", json!({ "command": "false" }), "exit 1\nstdout:\nx")), says("exit 1"));
    assert_eq!(stat(tool("android_shell", json!({ "command": "true" }), "exit status unknown (this device has no framed shell; stderr is mixed into stdout)")), says("12 ms"));
    assert_eq!(stat(tool("android_logcat", json!({}), "logcat main, system, last 2 lines:\nI/a: one\nI/a: two")), says("2 行"));
    assert_eq!(stat(tool("android_logcat", json!({}), "No log lines matched.")), says("0 行"));
    assert_eq!(tool("android_logcat", json!({}), "logcat main, last 1 lines:\nI/a: one").en().stat, says("1 lines"));
    // The count is the header's, in the host's own wording: "line" for one, filters after it.
    assert_eq!(stat(tool("android_logcat", json!({ "tag": "OkHttp" }), "logcat buffer main, last 1 line, tag OkHttp:\nI/OkHttp: one")), says("1 行"));
    assert_eq!(stat(tool("android_logcat", json!({ "package": "com.foo" }), "logcat all buffers, last 3 lines, level W and above, app com.foo (pid 4242):\nW/a: 1\nW/a: 2\nW/a: 3")), says("3 行"));
    // The note on a log that was cut is not a log line.
    assert_eq!(stat(tool("android_logcat", json!({}), "logcat default buffers, last 2 lines:\nI/a: one\nI/a: two\n(The log was longer than 8 MiB; its newest lines were not read. Ask for fewer lines, or narrow it with package, tag or level.)")), says("2 行"));
    // A failed call read nothing: its error line is not "1 line", the figure is how long it took.
    let unreadable = "reading the log: com.foo is not running, so its lines cannot be told apart on this Android version; read it without package";
    assert_eq!(stat(failed("android_logcat", json!({ "package": "com.foo" }), unreadable)), says("12 ms"));
    assert_eq!(stat(failed("android_logcat", json!({}), "logcat default buffers, last 2 lines:\nI/a: one\nI/a: two")), says("12 ms"));
    // An answer without the header, or with none at all, has no line count to state.
    assert_eq!(stat(tool("android_logcat", json!({ "package": "com.foo" }), unreadable)), says("12 ms"));
    assert_eq!(stat(tool("android_logcat", json!({}), "")), says("12 ms"));
    assert_eq!(stat(failed("android_logcat", json!({}), "").with_duration(0)), None);
    let zoom = json!({ "region": { "x": 0, "y": 0, "width": 10, "height": 10 }, "image": { "width": 1024, "height": 512 }, "zoom": 2 }).to_string();
    assert_eq!(stat(tool("preview_zoom", json!({ "x": 0, "y": 0, "width": 10, "height": 10 }), &zoom)), says("1024×512"));
    assert_eq!(stat(tool("preview_zoom", json!({ "x": 0, "y": 0, "width": 10, "height": 10 }), "not json")), says("12 ms"));
    assert_eq!(stat(tool("screen_clipboard", json!({ "action": "write", "text": "hello" }), "ok")), says("5 字"));
    assert_eq!(tool("screen_clipboard", json!({ "action": "write", "text": "hello" }), "ok").en().stat, says("5 characters"));
    // The length of a clipboard write counts as JavaScript counts, in UTF-16 units.
    assert_eq!(stat(tool("screen_clipboard", json!({ "action": "write", "text": "🙂" }), "ok")), says("2 字"));
    assert_eq!(stat(tool("screen_clipboard", json!({ "action": "read" }), "Clipboard (3 characters):\nabc")), says("12 ms"));
    // An install or a wait can run for a long time, which reads better in seconds.
    assert_eq!(stat(tool("screen_package", json!({ "action": "install", "path": "a.apk" }), "Installed a.apk").with_duration(2500)), says("2.5 s"));
    assert_eq!(stat(tool("preview_wait", json!({}), "The screen changed after 0.4 s.").with_duration(400)), says("400 ms"));
    assert_eq!(stat(tool("preview_wait", json!({}), "").with_duration(0)), None);
    // `toFixed` rounds an exact tie up, where formatting would round it to even.
    assert_eq!(stat(tool("preview_wait", json!({}), "").with_duration(1250)), says("1.3 s"));
    assert_eq!(stat(tool("preview_wait", json!({}), "").with_duration(1150)), says("1.1 s"));
}

#[test]
fn lays_out_the_arguments_worth_reading_and_then_what_the_tool_answered() {
    let card = tool("android_shell", json!({ "serverId": "pixel", "command": "getprop ro.product.model", "timeout": 5000 }), "exit 0\nstdout:\nPixel 8").kv();
    let rows: Vec<(&str, &str)> = card.rows.iter().map(|(k, v)| (k.as_str(), &**v)).collect();
    assert_eq!(rows, [("serverId", "pixel"), ("command", "getprop ro.product.model"), ("timeout", "5000")]);
    assert_eq!(card.result.as_deref(), Some("exit 0\nstdout:\nPixel 8"));
    // Only the arguments the schema names are shown, whatever else the transport carried.
    let other = tool("screen_app", json!({ "action": "launch", "app": "com.example.app", "plumbing": "x" }), "Launched com.example.app.").kv();
    assert_eq!(other.labels(), ["action", "app"]);
}

#[test]
fn draws_a_close_up_as_a_thumbnail_alone_and_keeps_a_failures_message() {
    let metadata = json!({ "region": { "x": 10, "y": 20, "width": 100, "height": 50 }, "image": { "width": 200, "height": 100 }, "zoom": 2, "note": "close-up only: x/y for preview_input stay in the full screenshot's frame" }).to_string();
    let zoomed = tool("preview_zoom", json!({ "x": 10, "y": 20, "width": 100, "height": 50 }), &metadata).with_image(&pixel("zoom-20260714T000000.000Z"));
    // The thumbnail is the result; the mapping back to the frame is for the model, and stays under Raw data.
    assert_eq!(zoomed.detail().body, DetailBody::ImageOnly);
    let presented = zoomed.zh();
    assert_eq!((presented.target.as_deref(), presented.stat.as_deref()), (Some("10,20 100×50"), Some("200×100")));
    let message = "The screen size changed since the last screenshot; take a new preview_screenshot first.";
    assert_eq!(failed("preview_zoom", json!({ "x": 0, "y": 0, "width": 10, "height": 10 }), message).kv().result.as_deref(), Some(message));
}

// ---------------------------------------------------------------- memory tools

#[test]
fn registers_memory_tools_with_localized_done_running_and_failed_titles() {
    for (name, zh, en) in [
        ("read_global_memory", ["读取了全局记忆", "正在读取全局记忆", "读取全局记忆失败"], ["Read global memory", "Reading global memory", "Failed to read global memory"]),
        ("read_project_memory", ["读取了项目记忆", "正在读取项目记忆", "读取项目记忆失败"], ["Read project memory", "Reading project memory", "Failed to read project memory"]),
        ("create_global_memory", ["创建了全局记忆", "正在创建全局记忆", "创建全局记忆失败"], ["Created global memory", "Creating global memory", "Failed to create global memory"]),
        ("create_project_memory", ["创建了项目记忆", "正在创建项目记忆", "创建项目记忆失败"], ["Created project memory", "Creating project memory", "Failed to create project memory"]),
        ("edit_global_memory", ["编辑了全局记忆", "正在编辑全局记忆", "编辑全局记忆失败"], ["Edited global memory", "Editing global memory", "Failed to edit global memory"]),
        ("edit_project_memory", ["编辑了项目记忆", "正在编辑项目记忆", "编辑项目记忆失败"], ["Edited project memory", "Editing project memory", "Failed to edit project memory"]),
    ] {
        let item = tool(name, json!({ "name": "build" }), "记忆正文");
        let running = item.clone().running();
        // Nothing to quote, so the title is the registered phrase alone.
        let bad = failed(name, json!({ "name": "build" }), "");
        assert_eq!([item.zh().title, running.zh().title, bad.zh().title], zh);
        assert_eq!([item.en().title, running.en().title, bad.en().title], en);
        // Targets the document by the name the model asked for.
        assert_eq!(tool(name, json!({ "name": "build-environment" }), "ok").zh().target.as_deref(), Some("build-environment"));
    }
}

#[test]
fn labels_each_tier_and_shows_the_document_body_a_read_returned() {
    let card = tool("read_project_memory", json!({ "name": "build" }), "测试要用项目自带环境").kv();
    assert_eq!(card.get("范围"), Some("项目记忆"));
    assert_eq!(card.get("文档"), Some("build"));
    assert_eq!(card.result.as_deref(), Some("测试要用项目自带环境"));
}

#[test]
fn shows_what_a_write_recorded_index_description_body_and_receipt() {
    let card = tool("create_global_memory", json!({ "name": "preferences", "content": "回答一律用中文", "description": "用户长期偏好" }), "已在全局记忆中创建 preferences.md，并写入索引描述。").kv();
    assert_eq!(card.get("范围"), Some("全局记忆"));
    assert_eq!(card.get("索引描述"), Some("用户长期偏好"));
    assert_eq!(card.get("内容"), Some("回答一律用中文"));
    assert!(card.result.as_deref().is_some_and(|r| r.contains("已在全局记忆中创建 preferences.md")));
}

#[test]
fn shows_the_passage_an_edit_replaced_and_what_replaced_it() {
    let card = tool("edit_project_memory", json!({ "name": "build", "old_text": "端口 3000", "new_text": "端口 4000" }), "已在项目记忆中编辑 build.md。").kv();
    assert_eq!(card.get("原文"), Some("端口 3000"));
    assert_eq!(card.get("新文"), Some("端口 4000"));
}

#[test]
fn draws_a_failed_memory_call_as_the_same_card_ending_in_its_reason() {
    let card = failed("edit_project_memory", json!({ "name": "build", "old_text": "3000", "new_text": "4000", "description": "端口" }), "要替换的原文在记忆文档 build.md 中出现了 2 次；请提供唯一匹配的更长片段").kv();
    assert_eq!(card.get("文档"), Some("build"));
    assert_eq!(card.get("原文"), Some("3000"));
    assert!(card.result.as_deref().is_some_and(|r| r.contains("出现了 2 次")));
}

#[test]
fn carries_no_model_identity_scope_selector_or_version_anywhere_in_the_card() {
    // Memory belongs to a location, not to a model.
    let card = tool("create_project_memory", json!({ "name": "notes", "content": "正文", "description": "描述" }), "已在项目记忆中创建 notes.md，并写入索引描述。").kv();
    let all = format!("{:?}", card);
    for retired in ["模型", "kimi-k3", "版本", "v1", "自动加载预算"] {
        assert!(!all.contains(retired), "{retired}");
    }
}

// ---------------------------------------------------------------- agent detail views

fn run_record(status: &str) -> Value {
    json!({ "name": "reviewer", "label": "接口审查", "task": "审查接口", "status": status, "contexts": [], "updates": [{ "content": "重复内容", "createdAt": "2026-07-14T01:04:00Z" }] })
}

#[test]
fn shows_the_instruction_under_whichever_input_key_the_run_tool_declared() {
    for (name, input, expected) in [
        ("agent_spawn", json!({ "name": "p", "prompt": "用 prompt 传的指令" }), "用 prompt 传的指令"),
        ("subagent", json!({ "label": "T", "task": "用 task 传的指令" }), "用 task 传的指令"),
        ("agent_send", json!({ "agent": "p", "message": "用 message 传的追加指令" }), "用 message 传的追加指令"),
    ] {
        assert_eq!(tool(name, input, "已受理").kv().get("派发指令"), Some(expected), "{name}");
    }
    // With no instruction of its own, a run says what its record was asked.
    assert_eq!(tool("workflow_step", json!({ "label": "build" }), "").with_subagent(run_record("completed")).kv().get("派发指令"), Some("审查接口"));
}

#[test]
fn merges_the_live_and_persisted_update_feeds_into_one_deduplicated_time_ordered_list() {
    // A call in the handoff window carries both feeds with the same content in each; showing it twice claimed two updates where the
    // child sent one.
    let mut item = tool("agent_spawn", json!({ "name": "reviewer", "prompt": "审查接口" }), "已完成").with_subagent(run_record("completed")).streaming(StreamingToolPhase::Completed);
    item.live = Some(Arc::new(SubagentLiveState {
        status: Some(SubagentLiveStatus::Running),
        updates: serde_json::from_value(json!([{ "content": "较旧更新", "createdAt": "2026-07-14T01:02:00Z" }, { "content": "重复内容", "createdAt": "2026-07-14T01:03:00Z" }])).expect("updates"),
        ..Default::default()
    }));
    // One value holding exactly the two distinct updates, oldest first, states both halves at once: nothing was shown twice and
    // nothing was reordered.
    let card = item.kv();
    assert_eq!(card.get("子代理更新"), Some("较旧更新\n重复内容"));
    assert_eq!(card.get("子代理"), Some("接口审查"));
    // A live status speaks while it streams.
    assert_eq!(card.get("状态"), Some("运行中"));
}

#[test]
fn splits_task_wait_envelopes_and_counts_them_on_the_row() {
    let output = "[reviewer · 已完成]\n12 项检查全部通过\n\n[tester · 进度更新]\n正在跑 e2e\n\n当前状态：reviewer 已完成、tester 运行中";
    let item = tool("task_wait", json!({ "tasks": ["reviewer", "tester"] }), output);
    let presented = item.zh();
    assert_eq!((presented.family, presented.stat.as_deref()), (ToolViewFamily::AgentWait, Some("收到 2 条")));
    // Who answered heads its own line, whole: a name is not cut to fit a column.
    let DetailBody::Envelopes { notice, envelopes, status_line } = item.detail().body else { panic!("not envelopes") };
    let heads: Vec<(&str, &str)> = envelopes.iter().map(|e| (e.agent.as_str(), e.status.as_str())).collect();
    assert_eq!(heads, [("reviewer", "已完成"), ("tester", "进度更新")]);
    assert_eq!(envelopes[0].body, "12 项检查全部通过");
    assert_eq!(status_line.as_deref(), Some("当前状态：reviewer 已完成、tester 运行中"));
    assert_eq!(notice, None);
}

#[test]
fn keeps_an_unparsable_or_empty_wait_result_readable_instead_of_dropping_it() {
    let opaque = tool("task_wait", json!({}), "provider returned an opaque wait error");
    assert_eq!(opaque.zh().stat, None);
    assert_eq!(opaque.kv().result.as_deref(), Some("provider returned an opaque wait error"));
    assert_eq!(empty_of(&tool("task_wait", json!({}), "").detail()), "等待已结束，没有可显示的更新");
    // A wait in flight or one that failed has nothing to count, whatever it says.
    assert_eq!(failed("task_wait", json!({}), "[a · b]\nc").zh().stat, None);
}

#[test]
fn shows_a_one_shot_agent_message_in_full_and_keeps_its_receipt_beside_it() {
    let item = tool("subagent_update", json!({ "message": "正在跑 e2e" }), "状态已返回给主智能体");
    assert_eq!(item.zh().family, ToolViewFamily::AgentNote);
    let card = item.kv();
    assert_eq!(card.get("内容"), Some("正在跑 e2e"));
    assert_eq!(card.result.as_deref(), Some("状态已返回给主智能体"));
}

#[test]
fn shows_who_a_send_message_went_to_its_preview_and_its_message_in_full_beside_the_hosts_receipt() {
    let message = "请再检查一次并发边界。\n特别是取消竞态。";
    let receipt = "Message queued for reviewer; it reads it before its next model request.";
    let card = tool("send_message", json!({ "to": "reviewer", "message": message, "summary": "再查并发" }), receipt).kv();
    let rows: Vec<(&str, &str)> = card.rows.iter().map(|(k, v)| (k.as_str(), &**v)).collect();
    assert_eq!(rows, [("发给", "reviewer"), ("摘要", "再查并发"), ("内容", message)]);
    assert_eq!(card.result.as_deref(), Some(receipt));
    // The preview is optional; without one the card simply omits that row.
    let bare = tool("send_message", json!({ "to": "reviewer", "message": message }), receipt);
    assert_eq!(bare.kv().labels(), ["发给", "内容"]);
    assert_eq!(bare.zh().target.as_deref(), Some("→ reviewer"));
}

#[test]
fn titles_a_send_message_row_by_phase_and_keeps_a_refusals_reason_in_the_card() {
    let input = json!({ "to": "ghost", "message": "你好" });
    assert_eq!(tool("send_message", input.clone(), "").running().zh().title, "正在发送消息");
    let refusal = "There is no child agent named ghost; task_list shows the existing ones.";
    let bad = failed("send_message", input, refusal);
    assert_eq!(bad.zh().title, format!("发送消息失败：{refusal}"));
    let card = bad.kv();
    assert_eq!(card.get("发给"), Some("ghost"));
    assert_eq!(card.result.as_deref(), Some(refusal));
}

#[test]
fn opens_a_resumed_send_message_as_the_childs_run_under_the_message_it_sent() {
    let resumed = tool("send_message", json!({ "to": "reviewer", "message": "补充并发风险", "summary": "并发风险" }), "reviewer had finished; it was resumed in the background with your message and its full context.").with_subagent(run_record("stopped"));
    // The run it carries makes it an agent run, which the timeline opens as a panel.
    let presented = resumed.zh();
    assert_eq!((presented.family, presented.title.as_str(), presented.target.as_deref()), (ToolViewFamily::AgentRun, "发送了消息", Some("→ reviewer · 并发风险")));
    let mut live = tool("send_message", json!({ "to": "reviewer", "message": "补充并发风险" }), "resumed").streaming(StreamingToolPhase::Completed);
    live.live = Some(Arc::new(SubagentLiveState { status: Some(SubagentLiveStatus::Running), ..Default::default() }));
    assert_eq!(live.zh().family, ToolViewFamily::AgentRun);
    let card = resumed.kv();
    assert_eq!(card.get("子代理"), Some("接口审查"));
    assert_eq!(card.get("状态"), Some("已停止"));
    assert_eq!(card.get("消息"), Some("补充并发风险"));
    assert_eq!(card.get("摘要"), Some("并发风险"));
    assert_eq!(card.get("派发指令"), None);
}

#[test]
fn titles_a_task_stop_row_by_phase_and_shows_the_address_it_acted_on_beside_the_receipt() {
    let done = tool("task_stop", json!({ "task": "shell:1" }), "已请求停止 shell:1");
    let presented = done.zh();
    assert_eq!((presented.family, presented.title.as_str(), presented.target.as_deref()), (ToolViewFamily::Raw, "停止了任务", Some("shell:1")));
    assert_eq!(done.clone().running().zh().title, "正在停止任务");
    // A failed row appends the host's reason to its phrase.
    assert_eq!(failed("task_stop", json!({ "task": "shell:9" }), "没有这个任务").zh().title, "停止任务失败：没有这个任务");
    let card = done.kv();
    assert_eq!(card.get("task"), Some("shell:1"));
    assert_eq!(card.result.as_deref(), Some("已请求停止 shell:1"));
}

// ---------------------------------------------------------------- shared safeguards, and the card itself

#[test]
fn falls_back_to_a_raw_renderer_for_unknown_future_tools() {
    let item = tool("future_tool", json!({ "value": 1 }), "future output");
    let presented = item.zh();
    assert_eq!((presented.family, presented.surface, presented.title.as_str()), (ToolViewFamily::Raw, ToolSurface::Group, "使用了 future_tool"));
    assert_eq!(item.kv().result.as_deref(), Some("future output"));
    // A language other than the card's own words is only the words.
    assert_eq!(item.detail_en().kv().and_then(|card| card.result.as_deref()), Some("future output"));
    assert_eq!(empty_of(&tool("task_list", json!({}), "").detail_en()), "No results to display");
}

#[test]
fn raw_data_is_the_call_with_the_tool_first() {
    let text = raw_data_text(&tool("read", json!({ "path": "missing.ts" }), "文件不存在").view());
    assert!(text.starts_with("{\n  \"tool\": \"read\",\n  \"input\": {\n    \"path\": \"missing.ts\"\n  },\n  \"result\": {\n    \"success\": true,"), "{text}");
    assert!(text.contains("\"output\": \"文件不存在\""));
}

/// `visual_rows` and `window_of` hold what a long output depends on: every line is reachable, and only the rows in view exist.
#[test]
fn cuts_a_long_text_into_rows_of_the_mono_cell() {
    assert_eq!(visual_rows("abcd\nef", 10), [0, 5]);
    // A line that runs out of room goes on on a row of its own, at a character boundary.
    assert_eq!(visual_rows("abcdefgh", 3), [0, 3, 6]);
    // Wide characters take two cells: the third does not fit in five.
    assert_eq!(visual_rows("你好吗", 5), [0, 6]);
    // A final newline ends with an empty row.
    assert_eq!(visual_rows("a\n", 3), [0, 2]);
    // A row breaks after its last space when it has one, as a wrap would.
    assert_eq!(visual_rows("aa bb cc", 5), [0, 3]);
    assert_eq!(visual_rows("aaaa bbbbbb", 5), [0, 5, 10]);
    // A line break leaves no such space to the next line.
    assert_eq!(visual_rows("a b\ncccccc", 5), [0, 4, 9]);
}

#[test]
fn keeps_only_the_rows_near_the_view() {
    use mewrk_client::prelude::Rect;
    let rect = |y: f32, h: f32| Rect { x: 0.0, y, w: 100.0, h };
    // Nothing laid out yet: the top.
    assert_eq!(window_of(None, None, 100_000, 16.0).start, 0);
    assert!(window_of(None, None, 100_000, 16.0).end < 200);
    // Scrolled far down: 400 px of view at 1 600 000 px.
    let window = window_of(Some(rect(-1_600_000.0, 1_600_000.0)), Some(rect(0.0, 400.0)), 100_000, 16.0);
    assert!(window.start <= 100_000 - 48 && window.end >= 100_000.min(100_025), "{window:?}");
    assert!(window.len() < 200, "{window:?}");
    // The block starts below the top of the scroller: no rows have scrolled away yet.
    let window = window_of(Some(rect(300.0, 1000.0)), Some(rect(0.0, 400.0)), 1000, 16.0);
    assert_eq!(window.start, 0);
    // Never past the end.
    assert_eq!(window_of(Some(rect(-1_000_000.0, 100.0)), Some(rect(0.0, 400.0)), 10, 16.0), 10..10);
}

fn draw(rig: &mut Rig, key: &'static str, item: &Card) -> impl FnMut(&mut mewrk_client::App) -> E + use<> {
    let _ = rig;
    let item = item.clone();
    move |_| tool_detail_renderer(key, &item.view(), None)
}

#[test]
fn only_materializes_raw_data_after_its_disclosure_opens() {
    let mut rig = Rig::new();
    let item = failed("read", json!({ "path": "missing.ts" }), "文件不存在");
    let mut view = draw(&mut rig, "raw-card", &item);
    rig.settle(&mut view);
    assert!(rig.rect(tool_raw_summary_key("raw-card")).is_some(), "the summary is there");
    assert!(rig.rect(tool_raw_pre_key("raw-card")).is_none(), "the text is not");
    rig.click_key(&mut view, tool_raw_summary_key("raw-card"));
    let pre = rig.rect(tool_raw_pre_key("raw-card")).expect("the text opened");
    assert!(pre.h > 0.0);
    // And it folds away again.
    rig.click_key(&mut view, tool_raw_summary_key("raw-card"));
    assert!(rig.rect(tool_raw_pre_key("raw-card")).is_none());
}

#[test]
fn caps_a_cards_body_as_the_css_does_and_keeps_the_rest_a_wheel_away() {
    let mut rig = Rig::new();
    let output: String = (0..400).map(|i| format!("line {i}\n")).collect();
    let item = tool("bash", json!({ "command": "seq 400" }), &output);
    let mut view = draw(&mut rig, "capped", &item);
    rig.settle(&mut view);
    let body = rig.rect(tool_body_key("capped")).expect("a body");
    // `min(52vh, 440px)` of the rig's 800 px window.
    assert!((body.h - 416.0).abs() < 1.0, "{body:?}");
    let (offset, content, viewport) = mewrk_client::prelude::scroll_state(tool_body_key("capped")).expect("it scrolls");
    assert_eq!(offset, 0.0);
    assert!(content > viewport * 10.0, "all 400 lines are in its height: {content} over {viewport}");
    rig.size = (1000.0, 1200.0);
    rig.settle(&mut view);
    let body = rig.rect(tool_body_key("capped")).expect("a body");
    assert!((body.h - 440.0).abs() < 1.0, "{body:?}");
    // And on a narrow window, `min(48vh, 360px)`.
    rig.size = (700.0, 500.0);
    rig.settle(&mut view);
    let body = rig.rect(tool_body_key("capped")).expect("a body");
    assert!((body.h - 240.0).abs() < 1.0, "{body:?}");
}

#[test]
fn a_long_value_wraps_inside_a_key_value_card_rather_than_widening_it() {
    let mut rig = Rig::new();
    rig.size = (600.0, 800.0);
    let long_value = "word ".repeat(200);
    let item = tool("android_shell", json!({ "serverId": "pixel", "command": long_value, "timeout": 5000 }), "exit 0");
    let mut view = draw(&mut rig, "grid-card", &item);
    rig.settle(&mut view);
    let body = rig.rect(tool_body_key("grid-card")).expect("a body");
    assert!(body.w <= 600.0, "{body:?}");
    // Three rows, the wrapped one several lines, and the result under a rule.
    assert!(body.h > 16.0 * 8.0, "{body:?}");
}

#[test]
fn a_chip_keeps_its_letter_until_an_icon_decodes() {
    let mut rig = Rig::new();
    const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    assert!(!has_site_icon("icon.example"));
    assert!(!adopt_site_icon("icon.example", "data:image/png;base64,bm90IGEgcGljdHVyZQ=="), "not a picture");
    assert!(!has_site_icon("icon.example"));
    assert!(adopt_site_icon("icon.example", PIXEL));
    assert!(has_site_icon("icon.example"));
    // Already known, so drawing it asks the host nothing: this test stays off the network.
    let item = tool("web_search", json!({ "query": "x" }), &json!({ "findings": "f", "sources": [{ "url": "https://icon.example/a" }] }).to_string());
    let mut view = draw(&mut rig, "icon-card", &item);
    rig.settle(&mut view);
    assert!(rig.rect(tool_body_key("icon-card")).is_some());
}

#[test]
fn words_a_value_as_it_is_even_where_it_looks_like_a_parameter() {
    assert_eq!(Lang::En.f("{a} {b}", "{a}: {b} {c}", &[("a", &"{b}"), ("b", &1)]), "{b}: 1 {c}");
    let future = failed("future_tool", json!({}), "boom");
    let title = get_tool_presentation_in(&future.view(), Some(&mcp_descriptor("{reason}", "future_tool", "shell")), Lang::En, None, None).title;
    assert_eq!(title, "{reason} failed: boom");
}

#[test]
fn strips_every_escape_sequence_ansi_writes() {
    assert_eq!(strip_ansi("\u{1b}[1;31mred\u{1b}[0m \u{1b}]0;title\u{7}x\u{1b}]8;;url\u{1b}\\y\u{1b}(Bz\u{1b}=w"), "red xyzw");
    // A lone escape that opens nothing it knows stays.
    assert_eq!(strip_ansi("a\u{1b}"), "a\u{1b}");
    assert_eq!(strip_ansi("plain"), "plain");
}

#[test]
fn the_names_without_a_language_use_the_apps() {
    // The rig's host starts in English, one flag for the whole process.
    let _rig = Rig::new();
    let item = tool("ls", json!({ "path": "." }), "a.txt\n");
    assert_eq!(get_tool_presentation(&item.view(), None, None, None).title, "Viewed directory");
    assert_eq!(summarize_block_kinds(&[SummaryKind::Files]), "Checked files and directories once");
    assert_eq!(running_label(&item.clone().running().view()), "Running");
    assert!(matches!(tool_detail_model(&item.view(), None).body, DetailBody::Entries { label, .. } if label == "File results"));
}

/// Nothing here can make a frame slow, however long the output: a million lines cost what a screenful does. The budget is generous
/// because the test build is unoptimised and other builds share the machine; what it rules out is work proportional to the output.
#[test]
fn a_huge_output_does_not_make_a_frame_slow() {
    let mut rig = Rig::new();
    let output: String = (0..200_000).map(|i| format!("line number {i} of a very long output\n")).collect();
    let terminal = tool("bash", json!({ "command": "seq" }), &output);
    let listing = tool("ls", json!({ "path": "." }), &(0..50_000).map(|i| format!("src/file{i}.rs\n")).collect::<String>());
    let grep = tool("grep", json!({ "pattern": "x", "output_mode": "content" }), &(0..50_000).map(|i| format!("src/a.rs:{i}:needle\n")).collect::<String>());
    let read = tool("read", json!({ "path": "a.rs" }), &(0..50_000).map(|i| format!("let x{i} = 1;\n")).collect::<String>());
    // As long as still counts as one leaf, which is the most a text costs.
    let at_limit = tool("bash", json!({ "command": "seq" }), &(0..490).map(|i| format!("line number {i} of an output as long as a text can be and stay one leaf of a card\n")).collect::<String>());
    for (key, item) in [("huge-terminal", &terminal), ("huge-listing", &listing), ("huge-grep", &grep), ("huge-read", &read), ("leaf-limit", &at_limit)] {
        let mut view = draw(&mut rig, key, item);
        // A first frame is allowed to read the output once; later ones are not.
        let started = Instant::now();
        rig.settle(&mut view);
        let settled = started.elapsed();
        let started = Instant::now();
        for _ in 0..5 {
            rig.frame(&mut view);
        }
        let per_frame = started.elapsed() / 5;
        println!("{key}: {settled:?} to settle, {per_frame:?} a frame");
        assert!(per_frame < Duration::from_millis(250), "{key}: {per_frame:?} a frame");
    }
}

#[test]
fn a_huge_output_stays_reachable_by_wheel_and_to_its_end() {
    let mut rig = Rig::new();
    let output: String = (0..50_000).map(|i| format!("line {i}\n")).collect();
    let item = tool("bash", json!({ "command": "seq" }), &output);
    let mut view = draw(&mut rig, "wheel-card", &item);
    rig.settle(&mut view);
    let (_, content, viewport) = mewrk_client::prelude::scroll_state(tool_body_key("wheel-card")).expect("it scrolls");
    // Every line has its height, though only a window of them exists.
    assert!(content > 50_000.0 * 15.0, "{content}");
    let body = rig.rect(tool_body_key("wheel-card")).expect("a body");
    rig.point(body.x + 20.0, body.y + 20.0);
    rig.frame(&mut view);
    rig.wheel(&mut view, -600.0);
    for _ in 0..30 {
        rig.frame(&mut view);
    }
    let (offset, _, _) = mewrk_client::prelude::scroll_state(tool_body_key("wheel-card")).expect("it scrolls");
    assert!(offset > 100.0, "the wheel moved it: {offset}");
    mewrk_client::prelude::scroll_request(tool_body_key("wheel-card"), mewrk_client::prelude::ScrollRequest::To(content - viewport));
    for _ in 0..30 {
        rig.frame(&mut view);
    }
    let (offset, _, _) = mewrk_client::prelude::scroll_state(tool_body_key("wheel-card")).expect("it scrolls");
    assert!(offset > content - viewport * 2.0, "{offset} of {content}");
}

// ---------------------------------------------------------------- `lib/orchestration`

const ASK_USER_PENDING_OUTPUT: &str = "Asked the user; this turn is paused.";

fn questions_input() -> Value {
    json!({ "questions": [
        { "question": "选哪个数据库？", "header": "数据库", "options": [{ "label": "Postgres", "description": "a" }, { "label": "SQLite", "description": "b" }], "multiSelect": false },
        { "question": "部署到哪？", "header": "部署", "options": [{ "label": "Fly", "description": "a" }, { "label": "AWS", "description": "b" }], "multiSelect": false }
    ] })
}

fn object_of(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("an object").clone()
}

fn merged(mut base: Value, extra: Value) -> Value {
    for (key, value) in object_of(&extra) {
        base[key.as_str()] = value;
    }
    base
}

#[test]
fn reads_a_blocking_calls_outcome_from_its_own_result() {
    let input = questions_input();
    let answered = tool("ask_user", merged(input.clone(), json!({ "answers": { "选哪个数据库？": "Postgres" }, "annotations": {} })), "Your questions have been answered: \"选哪个数据库？\"=\"Postgres\". You can now continue with these answers in mind.");
    assert_eq!(question_outcome(&answered.view()), QuestionOutcome::Answered);
    assert_eq!(question_outcome(&tool("ask_user", input.clone(), ASK_USER_CLOSED_OUTPUT).view()), QuestionOutcome::Closed);
    assert_eq!(question_outcome(&tool("ask_user", input.clone(), ASK_USER_NO_ANSWER_OUTPUT).view()), QuestionOutcome::Unanswered);
    let declined = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\nThe user wants to clarify these questions.";
    assert_eq!(question_outcome(&failed("ask_user", input.clone(), declined).view()), QuestionOutcome::Declined);
    assert_eq!(question_outcome(&failed("ask_user", input, "questions must be an array of objects").view()), QuestionOutcome::Failed);
}

#[test]
fn treats_only_the_old_paused_receipt_as_a_question_answered_by_the_next_user_message() {
    let input = questions_input();
    let legacy = |card: &Card| is_legacy_paused_question(&Item { item: &card.item, streaming: card.streaming, stream_status: card.status, live: None, started_at: None });
    assert!(legacy(&tool("ask_user", input.clone(), ASK_USER_PENDING_OUTPUT)));
    assert!(!legacy(&tool("ask_user", input.clone(), ASK_USER_CLOSED_OUTPUT)));
    assert!(!legacy(&tool("ask_user", input.clone(), "").running()));
    // Conversation templates store the card with an empty receipt and its answer after it.
    assert!(legacy(&tool("ask_user", input, "")));
    let text: ContextItem = serde_json::from_value(json!({ "id": "user", "kind": "user", "content": ASK_USER_PENDING_OUTPUT, "createdAt": "2026-07-12T00:00:00Z" })).expect("a user context");
    assert!(!is_legacy_paused_question(&Item::settled(&text)));
}

#[test]
fn reads_answers_and_notes_keyed_by_question_text_from_the_executed_input() {
    let input = questions_input();
    let questions = questions_from_input(&object_of(&input));
    let executed = merged(input, json!({ "answers": { "部署到哪？": "AWS" }, "annotations": { "选哪个数据库？": { "notes": "都不合适" } } }));
    let answers = question_answers_from_input(&object_of(&executed), &questions);
    assert_eq!(answers, [QuestionAnswerView { answer: None, notes: Some("都不合适".to_owned()) }, QuestionAnswerView { answer: Some("AWS".to_owned()), notes: None }]);
}

#[test]
fn joins_multi_select_answers_the_way_claude_code_does() {
    assert_eq!(join_multi_select_answer(&["Red", "Blue"]), "Red, Blue");
    assert_eq!(join_multi_select_answer(&["Red", "my, custom", "say \"hi\""]), "Red, \"my, custom\", \"say \\\"hi\\\"\"");
}

fn claude_input() -> Value {
    json!({ "questions": [
        { "question": "采用哪个方案？", "header": "方案", "options": [{ "label": "方案 A", "description": "保持改动最小" }, { "label": "方案 B", "description": "完整重构", "preview": "B preview" }], "multiSelect": false },
        { "question": "启用哪些能力？", "header": "能力", "options": [{ "label": "搜索", "description": "全文搜索" }, { "label": "导出", "description": "文件导出" }], "multiSelect": true }
    ] })
}

#[test]
fn parses_all_claude_fields_and_formats_answers_by_question_text() {
    let questions = questions_from_input(&object_of(&claude_input()));
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].header, "方案");
    assert!(!questions[0].multi_select);
    assert_eq!(questions[0].options[0], QuestionOptionView { label: "方案 A".to_owned(), description: "保持改动最小".to_owned(), preview: None });
    assert_eq!(questions[0].options[1], QuestionOptionView { label: "方案 B".to_owned(), description: "完整重构".to_owned(), preview: Some("B preview".to_owned()) });
    let content = format_question_answers(&questions, &["方案 A".to_owned(), "搜索, 导出".to_owned()]);
    assert_eq!(content, "User has answered your questions: \"采用哪个方案？\"=\"方案 A\", \"启用哪些能力？\"=\"搜索, 导出\"");
    assert_eq!(answers_from_formatted_content(&content, &questions), Some(vec!["方案 A".to_owned(), "搜索, 导出".to_owned()]));
    // Not the form this reads, or a form holding no pairs, is no answer.
    assert_eq!(answers_from_formatted_content("something else", &questions), None);
    assert_eq!(answers_from_formatted_content("User has answered your questions: nothing", &questions), None);
    // A question with no pair has an empty answer, and an escape reads as JSON.
    assert_eq!(answers_from_formatted_content("User has answered your questions: \"采用哪个方案？\"=\"a \\\"b\\\"\"", &questions), Some(vec!["a \"b\"".to_owned(), String::new()]));
}

#[test]
fn validates_claude_limits_and_required_option_descriptions() {
    assert!(is_claude_question_input(&object_of(&claude_input())));
    assert!(!is_claude_question_input(&object_of(&json!({ "questions": [] }))));
    assert!(!is_claude_question_input(&object_of(&json!({ "questions": [{ "question": "继续吗？", "header": "这是一个肯定超过十二字符的标题", "options": [{ "label": "是", "description": "继续" }, { "label": "否", "description": "停止" }], "multiSelect": false }] }))));
}

#[test]
fn keeps_old_persisted_questions_readable() {
    let questions = questions_from_input(&object_of(&json!({ "question": "继续吗？", "options": ["是", " ", { "label": "否" }, { "label": " " }, 3] })));
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].header, "Question");
    assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["是", "否"]);
    assert!(questions_from_input(&object_of(&json!({ "questions": [{ "question": " " }] }))).is_empty());
}

#[test]
fn splits_every_drained_envelope_and_keeps_the_trailing_status_roll_up_apart() {
    let output = ["[review · 进度更新]", "已检查接口", "", "[review · 已完成]", "12 项检查全部通过", "第二行正文", "", "[tester · 已停止]", "（没有返回文本结果）", "", "当前状态：review 已完成、tester 已停止"].join("\n");
    let envelope = |agent: &str, status: &str, body: &str| WaitEnvelopeView { agent: agent.to_owned(), status: status.to_owned(), body: body.to_owned() };
    assert_eq!(
        parse_wait_output(&output),
        WaitOutputView {
            notice: String::new(),
            status_line: "当前状态：review 已完成、tester 已停止".to_owned(),
            envelopes: vec![envelope("review", "进度更新", "已检查接口"), envelope("review", "已完成", "12 项检查全部通过\n第二行正文"), envelope("tester", "已停止", "（没有返回文本结果）")]
        }
    );
}

#[test]
fn returns_the_timeout_and_nothing_to_drain_notices_as_notice_not_as_an_envelope() {
    let timeout = "等待 60 秒后期限到了，被等待的任务还没有给出结果——它们仍在后台运行，什么都没有丢。可以再等一次（需要更久就把 timeout_seconds 调大，上限 600 秒），也可以先做别的。";
    assert_eq!(parse_wait_output(timeout), WaitOutputView { envelopes: Vec::new(), status_line: String::new(), notice: timeout.to_owned() });
    let nothing = parse_wait_output("没有正在运行的任务，也没有待收取的更新。");
    assert!(nothing.envelopes.is_empty());
    assert_eq!(nothing.notice, "没有正在运行的任务，也没有待收取的更新。");
}

#[test]
fn keeps_a_notice_that_precedes_real_envelopes_and_never_swallows_unparsed_text() {
    // A timeout delivers accumulated progress updates with its notification; only a terminal result or expiry ends the wait.
    let timeout = "等待 30 秒后期限到了，被等待的任务还没有给出结果——它们仍在后台运行，什么都没有丢。可以再等一次（需要更久就把 timeout_seconds 调大，上限 600 秒），也可以先做别的。";
    let parsed = parse_wait_output(&[timeout, "", "[alpha · 进度更新]", "仍在跑 e2e"].join("\n"));
    assert_eq!(parsed.notice, timeout);
    assert_eq!(parsed.envelopes, [WaitEnvelopeView { agent: "alpha".to_owned(), status: "进度更新".to_owned(), body: "仍在跑 e2e".to_owned() }]);
}

#[test]
fn refuses_text_that_only_looks_like_a_header_so_opaque_provider_errors_survive_intact() {
    // A single bracketed line with no separator is not an envelope, and a header split across lines is not one either. Both must
    // reach the caller as notice so the detail view can fall back to the raw output.
    let parsed = parse_wait_output("provider returned an opaque wait error [oops]");
    assert!(parsed.envelopes.is_empty());
    assert_eq!(parsed.notice, "provider returned an opaque wait error [oops]");
    assert!(parse_wait_output("[oops]").envelopes.is_empty());
    assert!(parse_wait_output("[· nobody]").envelopes.is_empty());
    assert!(parse_wait_output("[nobody ·]").envelopes.is_empty());
}

#[test]
fn tells_host_authored_user_contexts_from_the_users_own() {
    let user = |id: &str| -> ContextItem { serde_json::from_value(json!({ "id": id, "kind": "user", "content": "x", "createdAt": "2026-07-12T00:00:00Z" })).expect("a user context") };
    assert!(is_structured_output_nudge(&user("ctx_structured-output-nudge_1")));
    assert!(!is_structured_output_nudge(&user("ctx_agent-message_1")));
    for id in ["ctx_agent-result_1", "ctx_agent-message_1", "ctx_structured-output-nudge_1"] {
        assert!(is_host_authored_user_context(&user(id)), "{id}");
    }
    assert!(!is_host_authored_user_context(&user("ctx_1")));
    let assistant: ContextItem = serde_json::from_value(json!({ "id": "ctx_agent-result_1", "kind": "assistant", "content": "x", "createdAt": "2026-07-12T00:00:00Z" })).expect("an assistant context");
    assert!(!is_host_authored_user_context(&assistant));
}

// ---------------------------------------------------------------- what a reader gets, and what wraps

mod a11y {
    use fluent::el::Role;

    use super::*;
    use crate::rig::{nameless, said, says};

    #[test]
    fn a_card_is_named_by_what_it_did_and_a_running_one_is_a_status() {
        let mut rig = Rig::new();
        let item = card("ls", json!({ "path": "src" }), "main.rs\nlib.rs", true, None);
        let mut view = draw(&mut rig, "named", &item);
        let tree = rig.a11y_tree(&mut view);
        let region = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Region).expect("the card");
        assert!(region.label().is_some_and(|l| l.contains("src")), "{:?}", region.label());
        assert_eq!(said(&tree, "File results").role(), Role::List);
        assert_eq!(tree.iter().filter(|(_, n)| n.role() == Role::ListItem).count(), 2);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        assert!(says(&tree, "main.rs").is_some());
        let raw = said(&tree, "Raw data");
        assert_eq!((raw.role(), raw.is_expanded()), (Role::Button, Some(false)));
        fluent::el::set_a11y_active(false);
    }
}

/// `.tool-renderer__lines > li { padding-left: 2ch; text-indent: -2ch }`: what wraps stands under the line's own start, two cells in, and
/// the next line starts back at the edge.
#[test]
fn what_wraps_in_a_list_of_lines_hangs_and_each_line_starts_at_the_edge() {
    let mut rig = Rig::new();
    let long = "word ".repeat(120);
    let item = card("grep", json!({ "pattern": "x", "output_mode": "content" }), &format!("src/a.rs:1:{long}\nsrc/b.rs:2:{long}"), true, None);
    let mut view = draw(&mut rig, "hang", &item);
    rig.settle(&mut view);
    let starts: Vec<f32> = {
        let mut glyphs: Vec<(f32, f32)> = rig.scene.layers.iter().flat_map(|layer| layer.glyphs.iter()).map(|glyph| (glyph.pos[1], glyph.pos[0])).collect();
        glyphs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut lines: Vec<(f32, f32)> = Vec::new();
        for (y, x) in glyphs {
            match lines.last_mut() {
                Some(line) if (line.0 - y).abs() < 6.0 => line.1 = line.1.min(x),
                _ => lines.push((y, x)),
            }
        }
        lines.into_iter().map(|line| line.1).collect()
    };
    let edge = starts[0];
    let hung = starts.iter().filter(|x| (**x - edge - 12.6).abs() < 2.0).count();
    let at_edge = starts.iter().filter(|x| (**x - edge).abs() < 2.0).count();
    assert!(hung >= 4, "wrapped lines stand two cells in: {starts:?}");
    assert!(at_edge >= 2, "each line starts at the edge: {starts:?}");
}
