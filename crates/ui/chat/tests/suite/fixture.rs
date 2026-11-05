//! Timelines to test and to look at.
#![allow(dead_code)]

use mewrk_client::model::{ContextItem, ContextSource, HookContextMetadata, JsonObject, NativeCompaction, ToolCategory, ToolDescriptor, ToolResult};
use mewrk_client::runs::{LiveContext, StreamingToolPhase};
use serde_json::{json, Value};

pub const AT: &str = "2026-11-04T00:00:00.000Z";

pub fn result(success: bool, output: &str) -> ToolResult {
    ToolResult { success, output: output.to_owned(), images: Vec::new(), diff: None, executed_at: AT.to_owned(), duration_ms: 12 }
}

pub fn input(value: Value) -> JsonObject {
    serde_json::from_value(value).expect("an object")
}

pub fn user(id: &str, content: &str) -> ContextItem {
    ContextItem::User { id: id.to_owned(), content: content.to_owned(), images: Vec::new(), files: Vec::new(), created_at: AT.to_owned() }
}

pub fn assistant(id: &str, content: &str) -> ContextItem {
    ContextItem::Assistant { id: id.to_owned(), content: content.to_owned(), round: None, model_turn_id: None, interrupted: false, sources: Vec::new(), created_at: AT.to_owned() }
}

pub fn system(id: &str, content: &str) -> ContextItem {
    ContextItem::System { id: id.to_owned(), content: content.to_owned(), local_only: false, hook_execution: None, tools_added: Vec::new(), native_compaction: None, created_at: AT.to_owned() }
}

pub fn reasoning(id: &str, content: Option<&str>) -> ContextItem {
    ContextItem::Reasoning { id: id.to_owned(), content: content.map(str::to_owned), form: None, round: None, model_turn_id: None, interrupted: false, duration_ms: None, tokens: None, replay: None, created_at: AT.to_owned() }
}

pub fn tool(id: &str, name: &str, args: Value, output: &str) -> ContextItem {
    ContextItem::Tool { id: id.to_owned(), tool_name: name.to_owned(), round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input: input(args), result: result(true, output), subagent: None, notice: None, attestation: String::new(), created_at: AT.to_owned() }
}

pub fn failed_tool(id: &str, name: &str, args: Value, output: &str) -> ContextItem {
    ContextItem::Tool { id: id.to_owned(), tool_name: name.to_owned(), round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input: input(args), result: result(false, output), subagent: None, notice: None, attestation: String::new(), created_at: AT.to_owned() }
}

pub fn hook(id: &str, status: &str, content: &str, injected: bool) -> ContextItem {
    ContextItem::System {
        id: id.to_owned(),
        content: content.to_owned(),
        local_only: true,
        hook_execution: Some(HookContextMetadata { execution_id: format!("x-{id}"), hook_id: "h1".to_owned(), hook_name: "format-on-save".to_owned(), event: "PostToolUse".to_owned(), status: status.to_owned(), context_injected: injected }),
        tools_added: Vec::new(),
        native_compaction: None,
        created_at: AT.to_owned(),
    }
}

pub fn compaction(id: &str) -> ContextItem {
    ContextItem::System {
        id: id.to_owned(),
        content: String::new(),
        local_only: true,
        hook_execution: None,
        tools_added: Vec::new(),
        native_compaction: Some(Box::new(NativeCompaction { provider_id: "p".to_owned(), model: "m".to_owned(), model_name: "Opus".to_owned(), parts: Vec::new(), retained: Vec::new(), tokens_before: 180_000, tokens_after: 12_000, appended_tools: Vec::new(), plan_tools: false, cache_key: String::new() })),
        created_at: AT.to_owned(),
    }
}

pub fn descriptor(name: &str, label: &str, category: ToolCategory) -> ToolDescriptor {
    serde_json::from_value(json!({ "name": name, "label": label, "description": "", "category": category, "dangerous": false, "parameters": [] })).expect("a descriptor")
}

pub fn descriptors() -> Vec<ToolDescriptor> {
    vec![descriptor("read", "Read", ToolCategory::Filesystem), descriptor("write", "Write", ToolCategory::Filesystem), descriptor("bash", "Bash", ToolCategory::Shell), descriptor("preview_click", "Click", ToolCategory::Web), descriptor("web_search", "Search", ToolCategory::Web), descriptor("agent_spawn", "Spawn", ToolCategory::Orchestration)]
}

pub fn live(item: ContextItem) -> LiveContext {
    LiveContext::settled(item)
}

pub fn streaming(item: ContextItem, status: Option<StreamingToolPhase>) -> LiveContext {
    LiveContext { item, streaming: Some(true), stream_status: status, live: None, started_at: None }
}

const REPLY: &str = "Here is what I found, with **bold**, *emphasis*, `inline code` and a path `src/main.rs:12`.\n\n## A heading\n\n- first item\n- second item\n  - nested\n- [x] a finished task\n- [ ] an open one\n\n| Name | Count |\n|------|------:|\n| alpha | 1 |\n| beta | 22 |\n\n> A quote that says something.\n\n```rust\nfn main() {\n    println!(\"hello {}\", 1 + 1);\n}\n```\n\nInline math $a^2 + b^2 = c^2$ and a [link](https://example.com).";

const DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,3 +1,4 @@\n fn main() {\n-    println!(\"old\");\n+    println!(\"new\");\n+    println!(\"more\");\n }\n";

/// A conversation with a card of every kind.
pub fn every_kind() -> Vec<LiveContext> {
    let edit = ContextItem::Tool { id: "t-edit".to_owned(), tool_name: "edit".to_owned(), round: None, model_turn_id: None, provider_call_id: None, requested_input: None, input: input(json!({ "path": "src/main.rs" })), result: ToolResult { diff: Some(DIFF.to_owned()), ..result(true, "ok") }, subagent: None, notice: None, attestation: String::new(), created_at: AT.to_owned() };
    let mut interrupted = assistant("a-cut", "A reply cut off by Stop, kept for the reader but not sent.");
    if let ContextItem::Assistant { interrupted: mark, .. } = &mut interrupted {
        *mark = true;
    }
    let mut cited = assistant("a-cited", "An answer that cites its sources.");
    if let ContextItem::Assistant { sources, .. } = &mut cited {
        *sources = vec![ContextSource { id: "s1".to_owned(), url: Some("https://docs.example.com/guide".to_owned()), title: Some("The guide".to_owned()) }, ContextSource { id: "s2".to_owned(), url: Some("https://example.org/x".to_owned()), title: None }, ContextSource { id: "s3".to_owned(), url: Some("javascript:alert(1)".to_owned()), title: None }];
    }
    let mut local = system("sys-local", "Lifecycle note kept on this machine.");
    if let ContextItem::System { local_only, .. } = &mut local {
        *local_only = true;
    }
    let mut encrypted = reasoning("r-enc", None);
    if let ContextItem::Reasoning { tokens, duration_ms, .. } = &mut encrypted {
        *tokens = Some(2400);
        *duration_ms = Some(1800);
    }
    [
        system("sys", "You are a careful assistant.\nFollow the project's conventions."),
        local,
        user("u1", "Please look at src/main.rs and tell me what is wrong with it. Also check the table layout."),
        reasoning("r1", Some("## Reading the file\nFirst I should read the file and see what it prints.")),
        encrypted,
        tool("t-read", "read", json!({ "path": "src/main.rs" }), "fn main() {\n    println!(\"old\");\n}\n"),
        tool("t-grep", "grep", json!({ "pattern": "println", "output_mode": "content" }), "src/main.rs:2:    println!(\"old\");\nsrc/lib.rs:9:    println!(\"lib\");"),
        tool("t-ls", "ls", json!({ "path": "src" }), "main.rs\nlib.rs\nbin/"),
        tool("t-bash", "bash", json!({ "command": "cargo test --quiet" }), "running 3 tests\n...\ntest result: ok"),
        failed_tool("t-fail", "bash", json!({ "command": "cargo build" }), "Exit code 101\nerror: could not compile `x`"),
        edit,
        hook("h1", "completed", "formatted 1 file", true),
        hook("h2", "failed", "formatter crashed", false),
        tool("t-agent", "agent_spawn", json!({ "name": "scout", "label": "Scout", "message": "Find the bug" }), "spawned"),
        tool("t-web", "web_search", json!({ "query": "rust println" }), r#"{"findings":"Prints to stdout.","sources":[{"url":"https://doc.rust-lang.org/std/macro.println.html","title":"println in std"}]}"#),
        tool("t-mcp", "mcp__docs_0123456789__lookup__abc", json!({ "term": "macro" }), "A macro."),
        assistant("a1", REPLY),
        cited,
        interrupted,
        compaction("c1"),
        tool("q1", "ask_user", json!({ "questions": [{ "question": "Which style do you prefer?", "header": "Style", "options": [{ "label": "Terse", "description": "Short" }, { "label": "Long", "description": "Long" }] }] }), "Style: Terse"),
        user("u2", "Thanks. That is all for now."),
        assistant("a2", "You are welcome."),
    ]
    .into_iter()
    .map(live)
    .collect()
}

/// Many cards, the way a long conversation has them: messages between stretches of work.
pub fn long_timeline(count: usize) -> Vec<LiveContext> {
    let mut contexts = Vec::with_capacity(count);
    for at in 0..count {
        let id = format!("c{at}");
        contexts.push(live(match at % 7 {
            0 => user(&id, "A question about something, long enough to take a line or two of the bubble it sits in."),
            1 => reasoning(&id, Some("Thinking about what to do next, and what it would take.")),
            2 | 3 => tool(&id, "read", json!({ "path": format!("src/file{at}.rs") }), "fn a() {}\nfn b() {}\n"),
            4 => tool(&id, "bash", json!({ "command": format!("cargo test {at}") }), "ok"),
            _ => assistant(&id, "A reply with a little **Markdown** in it, and a second sentence so that it wraps somewhere.\n\n- one\n- two"),
        }));
    }
    contexts
}
