//! What a `hooks` block copied from Claude Code expects to see.
//!
//! A Claude Code hook names tools in Claude Code's words (`Bash`, `Read`,
//! `Write|Edit`) and reads their arguments under Claude Code's field names
//! (`tool_input.file_path`, `tool_input.old_string`, `tool_response.filePath`).
//! Mewrk's tools do the same jobs under their own names and arguments, so a
//! copied block would match nothing and read nothing. This module is the
//! translation: every Mewrk tool that does a Claude Code tool's job answers
//! to that name as well as its own, and its input and result carry Claude
//! Code's fields beside Mewrk's — added, never replacing, so a hook written
//! for Mewrk reads exactly what it always did.
//!
//! An `updatedInput` a hook sends back may be in either vocabulary;
//! [`mewrk_input`] folds Claude Code's fields back onto Mewrk's before the
//! call runs, so what executes is always a Mewrk call.

use serde_json::{Map, Value};

use crate::model::ToolResult;

/// The Claude Code tools a Mewrk tool stands in for, by Mewrk name.
///
/// Only tools that do the same job: a matcher written for one is a statement
/// about that job, and must not start firing on a tool that does another.
pub(crate) fn tool_names(tool: &str) -> &'static [&'static str] {
    match tool {
        "bash" | "zsh" | "sh" => &["Bash"],
        "powershell" => &["PowerShell"],
        "read" => &["Read"],
        "write" => &["Write"],
        "edit" => &["Edit"],
        "find" => &["Glob"],
        "grep" => &["Grep"],
        "ls" => &["LS"],
        "web_fetch" => &["WebFetch"],
        "web_search" => &["WebSearch"],
        "agent_spawn" => &["Task", "Agent"],
        "ask_user" => &["AskUserQuestion"],
        "exit_plan_mode" => &["ExitPlanMode"],
        "skill" => &["Skill"],
        "lsp" => &["LSP"],
        _ => &[],
    }
}

/// One argument both products have, under its two names.
struct Alias {
    mewrk: &'static str,
    claude_code: &'static str,
}

const FILE_PATH: Alias = Alias {
    mewrk: "path",
    claude_code: "file_path",
};

/// The arguments that are the same thing under a different name. Arguments
/// both products already spell alike (`command`, `content`, `path` of `ls`,
/// `pattern` of `grep`) need nothing.
fn aliases(tool: &str) -> &'static [Alias] {
    match tool {
        "read" | "write" => &[FILE_PATH],
        "edit" => &[
            FILE_PATH,
            Alias {
                mewrk: "find",
                claude_code: "old_string",
            },
            Alias {
                mewrk: "replace",
                claude_code: "new_string",
            },
        ],
        "find" => &[Alias {
            mewrk: "query",
            claude_code: "pattern",
        }],
        "agent_spawn" => &[
            Alias {
                mewrk: "agent_type",
                claude_code: "subagent_type",
            },
            Alias {
                mewrk: "label",
                claude_code: "description",
            },
        ],
        _ => &[],
    }
}

/// `input` with Claude Code's names for its arguments added beside Mewrk's.
pub(crate) fn tool_input(tool: &str, input: &Map<String, Value>) -> Value {
    let mut extended = input.clone();
    for alias in aliases(tool) {
        if let Some(value) = input.get(alias.mewrk) {
            extended
                .entry(alias.claude_code)
                .or_insert_with(|| value.clone());
        }
    }
    match tool {
        // Claude Code reads a line range as a start and a count.
        "read" => {
            let start = input.get("start_line").and_then(Value::as_u64);
            let end = input.get("end_line").and_then(Value::as_u64);
            if let Some(start) = start {
                extended.entry("offset").or_insert(Value::from(start));
                if let Some(end) = end.filter(|end| *end >= start) {
                    extended.entry("limit").or_insert(Value::from(end - start + 1));
                }
            }
        }
        // One URL is the only form Claude Code's fetch takes.
        "web_fetch" => {
            if let Some([url]) = input.get("urls").and_then(Value::as_array).map(Vec::as_slice) {
                extended.entry("url").or_insert_with(|| url.clone());
            }
        }
        "grep" => {
            if let Some(sensitive) = input.get("case_sensitive").and_then(Value::as_bool) {
                extended.entry("-i").or_insert(Value::Bool(!sensitive));
            }
        }
        _ => {}
    }
    Value::Object(extended)
}

/// A tool's result as a hook sees it: Mewrk's own fields (`success`,
/// `output`, …) plus the ones Claude Code's result for the same tool carries.
pub(crate) fn tool_response(tool: &str, input: &Map<String, Value>, result: &ToolResult) -> Value {
    let mut response = match serde_json::to_value(result) {
        Ok(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_owned);
    let mut add = |key: &str, value: Value| {
        response.entry(key).or_insert(value);
    };
    match tool {
        "bash" | "zsh" | "sh" | "powershell" => {
            add("stdout", Value::String(result.output.clone()));
            add("stderr", Value::String(String::new()));
            add("interrupted", Value::Bool(false));
        }
        "read" => {
            if let Some(path) = text("path") {
                add("filePath", Value::String(path.clone()));
                add(
                    "file",
                    serde_json::json!({ "filePath": path, "content": result.output }),
                );
            }
        }
        "write" => {
            if let Some(path) = text("path") {
                add("filePath", Value::String(path));
            }
            if let Some(content) = text("content") {
                add("content", Value::String(content));
            }
        }
        "edit" => {
            if let Some(path) = text("path") {
                add("filePath", Value::String(path));
            }
            if let Some(find) = text("find") {
                add("oldString", Value::String(find));
            }
            if let Some(replace) = text("replace") {
                add("newString", Value::String(replace));
            }
            // Read as the tool read it, so `"true"` replaced every occurrence.
            let replace_all = crate::tool_executor::edit_replace_all(input).unwrap_or(false);
            add("replaceAll", Value::Bool(replace_all));
        }
        "web_fetch" => {
            add("result", Value::String(result.output.clone()));
            if let Some([url]) = input.get("urls").and_then(Value::as_array).map(Vec::as_slice) {
                add("url", url.clone());
            }
        }
        "web_search" => {
            if let Some(query) = text("query") {
                add("query", Value::String(query));
            }
        }
        _ => {}
    }
    Value::Object(response)
}

/// A hook's `updatedInput` as the Mewrk call it describes.
///
/// `sent` is the input the hook was given ([`tool_input`]'s output). A Claude
/// Code field the hook changed, or wrote without its Mewrk twin, becomes that
/// twin; one it echoed unchanged beside its twin is only the echo of what
/// Mewrk sent, and is dropped. A hook that writes Mewrk's names is taken as
/// written.
pub(crate) fn mewrk_input(
    tool: &str,
    sent: &Map<String, Value>,
    updated: Map<String, Value>,
) -> Map<String, Value> {
    let mut call = updated;
    for alias in aliases(tool) {
        if let Some(value) = call.remove(alias.claude_code) {
            if sent.get(alias.claude_code) != Some(&value) || !call.contains_key(alias.mewrk) {
                call.insert(alias.mewrk.to_owned(), value);
            }
        }
    }
    match tool {
        "read" => {
            let offset = call.remove("offset");
            let limit = call.remove("limit");
            let changed = |key: &str, value: &Option<Value>| {
                value.is_some() && sent.get(key) != value.as_ref()
            };
            if changed("offset", &offset)
                || changed("limit", &limit)
                || (offset.is_some() && !call.contains_key("start_line"))
            {
                let start = offset
                    .as_ref()
                    .and_then(Value::as_u64)
                    .or_else(|| call.get("start_line").and_then(Value::as_u64))
                    .unwrap_or(1);
                call.insert("start_line".to_owned(), Value::from(start));
                match limit.as_ref().and_then(Value::as_u64) {
                    Some(count) if count > 0 => {
                        call.insert("end_line".to_owned(), Value::from(start + count - 1));
                    }
                    _ => {
                        call.remove("end_line");
                    }
                }
            }
        }
        "web_fetch" => {
            if let Some(url) = call.remove("url") {
                if sent.get("url") != Some(&url) || !call.contains_key("urls") {
                    call.insert("urls".to_owned(), Value::Array(vec![url]));
                }
            }
        }
        "grep" => {
            if let Some(insensitive) = call.remove("-i") {
                if sent.get("-i") != Some(&insensitive) || !call.contains_key("case_sensitive") {
                    if let Some(insensitive) = insensitive.as_bool() {
                        call.insert("case_sensitive".to_owned(), Value::Bool(!insensitive));
                    }
                }
            }
        }
        _ => {}
    }
    call
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn an_edit_reads_under_both_vocabularies() {
        let input = object(json!({"path": "/w/a.rs", "find": "old", "replace": "new"}));
        let sent = tool_input("edit", &input);
        assert_eq!(sent["path"], "/w/a.rs");
        assert_eq!(sent["file_path"], "/w/a.rs");
        assert_eq!(sent["old_string"], "old");
        assert_eq!(sent["new_string"], "new");
        assert_eq!(sent["find"], "old");

        let result = ToolResult {
            success: true,
            output: "ok".into(),
            images: Vec::new(),
            diff: None,
            executed_at: "now".into(),
            duration_ms: 1,
        };
        let response = tool_response("edit", &input, &result);
        assert_eq!(response["success"], true);
        assert_eq!(response["filePath"], "/w/a.rs");
        assert_eq!(response["oldString"], "old");
        assert_eq!(response["newString"], "new");
        assert_eq!(
            response["replaceAll"], false,
            "an edit without replace_all replaced one"
        );
        let shell = tool_response("bash", &object(json!({"command": "ls"})), &result);
        assert_eq!(shell["stdout"], "ok");

        // `replace_all` is spelled alike in both inputs; the result names it
        // Claude Code's way.
        let input = object(json!({
            "path": "/w/a.rs", "find": "old", "replace": "new", "replace_all": true
        }));
        assert_eq!(tool_input("edit", &input)["replace_all"], true);
        assert_eq!(tool_response("edit", &input, &result)["replaceAll"], true);
        // Spelled as a string, it is read as the tool read it.
        let spelled = object(json!({
            "path": "/w/a.rs", "find": "old", "replace": "new", "replace_all": "true"
        }));
        assert_eq!(tool_response("edit", &spelled, &result)["replaceAll"], true);
    }

    #[test]
    fn a_read_range_is_also_a_start_and_a_count() {
        let input = object(json!({"path": "/w/a", "start_line": 10, "end_line": 19}));
        let sent = tool_input("read", &input);
        assert_eq!(sent["offset"], 10);
        assert_eq!(sent["limit"], 10);
        let mut updated = object(sent.clone());
        updated.insert("limit".into(), json!(5));
        let call = mewrk_input("read", sent.as_object().unwrap(), updated);
        assert_eq!(call.get("start_line"), Some(&json!(10)));
        assert_eq!(call.get("end_line"), Some(&json!(14)));
        assert!(!call.contains_key("limit") && !call.contains_key("file_path"));
    }

    /// Whichever vocabulary a hook rewrote, the call that runs is a Mewrk call
    /// carrying the rewrite, and the echoed twins are gone.
    #[test]
    fn an_updated_input_folds_back_onto_mewrk_names() {
        let input = object(json!({"path": "/w/a.rs", "find": "old", "replace": "new"}));
        let sent = object(tool_input("edit", &input));

        // A Claude Code script: echoes everything, rewrites `new_string`.
        let mut claude = sent.clone();
        claude.insert("new_string".into(), json!("newer"));
        let call = mewrk_input("edit", &sent, claude);
        assert_eq!(Value::Object(call), json!({"path": "/w/a.rs", "find": "old", "replace": "newer"}));

        // A Claude Code script that returns only the fields it set.
        let call = mewrk_input("edit", &sent, object(json!({"file_path": "/w/b.rs", "old_string": "old", "new_string": "new"})));
        assert_eq!(Value::Object(call), json!({"path": "/w/b.rs", "find": "old", "replace": "new"}));

        // A Mewrk script: rewrites `replace`, echoes the rest.
        let mut mewrk = sent.clone();
        mewrk.insert("replace".into(), json!("newest"));
        let call = mewrk_input("edit", &sent, mewrk);
        assert_eq!(Value::Object(call), json!({"path": "/w/a.rs", "find": "old", "replace": "newest"}));
    }

    #[test]
    fn claude_code_names_cover_only_tools_that_do_the_same_job() {
        for shell in ["bash", "zsh", "sh"] {
            assert_eq!(tool_names(shell), ["Bash"]);
        }
        assert_eq!(tool_names("edit"), ["Edit"]);
        assert_eq!(tool_names("find"), ["Glob"]);
        assert!(tool_names("workflow").is_empty());
        assert!(tool_names("mcp__docs_0123456789__search__0123456789").is_empty());
    }
}
