//! `lib/orchestration.ts`: reading `ask_user` calls and `task_wait` output, which the question card and the tool rows share.

use mewrk_client::model::{ContextItem, JsonObject};
use mewrk_pane_tasks::util::js_trim;
use serde_json::Value;

use crate::item::{Item, ToolItem};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionItemView {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOptionView>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QuestionOptionView {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

fn options_from_value(value: Option<&Value>) -> Vec<QuestionOptionView> {
    let Some(options) = value.and_then(Value::as_array) else { return Vec::new() };
    options
        .iter()
        .filter_map(|option| {
            if let Some(label) = option.as_str() {
                let label = js_trim(label);
                return (!label.is_empty()).then(|| QuestionOptionView { label: label.to_owned(), ..Default::default() });
            }
            let record = option.as_object()?;
            let label = js_trim(record.get("label")?.as_str()?);
            if label.is_empty() {
                return None;
            }
            Some(QuestionOptionView {
                label: label.to_owned(),
                description: record.get("description").and_then(Value::as_str).map_or(String::new(), |d| js_trim(d).to_owned()),
                preview: record.get("preview").and_then(Value::as_str).filter(|p| !js_trim(p).is_empty()).map(str::to_owned),
            })
        })
        .collect()
}

pub fn questions_from_input(input: &JsonObject) -> Vec<QuestionItemView> {
    if let Some(items) = input.get("questions").and_then(Value::as_array) {
        let questions: Vec<QuestionItemView> = items
            .iter()
            .filter_map(|value| {
                let item = value.as_object()?;
                let question = js_trim(item.get("question")?.as_str()?);
                if question.is_empty() {
                    return None;
                }
                let header = item.get("header").and_then(Value::as_str).map(js_trim).filter(|h| !h.is_empty()).unwrap_or("Question");
                Some(QuestionItemView {
                    question: question.to_owned(),
                    header: header.to_owned(),
                    options: options_from_value(item.get("options")),
                    multi_select: item.get("multiSelect") == Some(&Value::Bool(true)),
                })
            })
            .collect();
        if !questions.is_empty() {
            return questions;
        }
    }
    // Keep old persisted calls readable after the multi-question protocol ships.
    let question = input.get("question").and_then(Value::as_str).map_or("", js_trim);
    if question.is_empty() {
        return Vec::new();
    }
    vec![QuestionItemView { question: question.to_owned(), header: "Question".to_owned(), options: options_from_value(input.get("options")), multi_select: false }]
}

pub fn is_claude_question_input(input: &JsonObject) -> bool {
    let Some(questions) = input.get("questions").and_then(Value::as_array) else { return false };
    if questions.is_empty() || questions.len() > 4 {
        return false;
    }
    let non_blank = |v: Option<&Value>| v.and_then(Value::as_str).is_some_and(|s| !js_trim(s).is_empty());
    questions.iter().all(|value| {
        let Some(item) = value.as_object() else { return false };
        if !non_blank(item.get("question")) {
            return false;
        }
        // Counted in characters, so that a header of emoji is as long as it looks.
        let Some(header) = item.get("header").and_then(Value::as_str).map(js_trim) else { return false };
        if header.is_empty() || header.chars().count() > 12 {
            return false;
        }
        if !item.get("multiSelect").is_some_and(Value::is_boolean) {
            return false;
        }
        let Some(options) = item.get("options").and_then(Value::as_array) else { return false };
        if options.len() < 2 || options.len() > 4 {
            return false;
        }
        options.iter().all(|option| {
            let Some(record) = option.as_object() else { return false };
            non_blank(record.get("label")) && non_blank(record.get("description")) && record.get("preview").is_none_or(Value::is_string)
        })
    })
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

pub fn format_question_answers(questions: &[QuestionItemView], answers: &[String]) -> String {
    let pairs: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(index, question)| format!("{}={}", json_string(&question.question), json_string(answers.get(index).map_or("", |a| js_trim(a)))))
        .collect();
    format!("User has answered your questions: {}", pairs.join(", "))
}

/// The end of the quoted string that opens at `at`: `"(?:\\.|[^"\\])*"`. `.` stops at a line terminator, as in JavaScript.
fn quoted_end(content: &str, at: usize) -> Option<usize> {
    let mut chars = content[at..].char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    while let Some((offset, c)) = chars.next() {
        match c {
            '"' => return Some(at + offset + 1),
            '\\' => {
                let (_, escaped) = chars.next()?;
                if matches!(escaped, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

pub fn answers_from_formatted_content(content: &str, questions: &[QuestionItemView]) -> Option<Vec<String>> {
    if !content.starts_with("User has answered your questions: ") {
        return None;
    }
    let mut answers: Vec<(String, String)> = Vec::new();
    let mut search = 0;
    while let Some(found) = content[search..].find('"') {
        let start = search + found;
        // A quote that opens no `"a"="b"` pair is passed by one character, as a regex's next attempt would be.
        let pair = quoted_end(content, start).and_then(|key_end| {
            if content.as_bytes().get(key_end) != Some(&b'=') {
                return None;
            }
            let value_end = quoted_end(content, key_end + 1)?;
            Some((key_end, value_end))
        });
        let Some((key_end, value_end)) = pair else {
            search = start + 1;
            continue;
        };
        let key: String = serde_json::from_str(&content[start..key_end]).ok()?;
        let value: String = serde_json::from_str(&content[key_end + 1..value_end]).ok()?;
        match answers.iter_mut().find(|(k, _)| *k == key) {
            Some(existing) => existing.1 = value,
            None => answers.push((key, value)),
        }
        search = value_end;
    }
    if answers.is_empty() {
        return None;
    }
    Some(questions.iter().map(|question| answers.iter().find(|(k, _)| *k == question.question).map_or(String::new(), |(_, v)| v.clone())).collect())
}

/// One task-result envelope section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitEnvelopeView {
    pub agent: String,
    pub status: String,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WaitOutputView {
    pub envelopes: Vec<WaitEnvelopeView>,
    /// Trailing host-appended status roll-up, if present.
    pub status_line: String,
    /// Text preceding the first envelope: the timeout or nothing-to-drain notice.
    pub notice: String,
}

/// `^\[([^\]\r\n]+?)\s*·\s*([^\]\r\n]+?)\]$` on a trimmed line.
fn wait_envelope_header(line: &str) -> Option<(&str, &str)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    if inner.contains([']', '\r', '\n']) {
        return None;
    }
    // Each side needs a character of its own, so a `·` at the very start can only belong to the agent's name.
    let first_len = inner.chars().next()?.len_utf8();
    let dot = first_len + inner[first_len..].find('·')?;
    let status = &inner[dot + '·'.len_utf8()..];
    if status.is_empty() {
        return None;
    }
    Some((js_trim(&inner[..dot]), js_trim(status)))
}

/// The status roll-up heading of the built-in profile (`task.wait_status_heading`), and the Chinese one older transcripts
/// carry. A custom profile that words it differently folds the roll-up into the last envelope body; the card still
/// renders, only less structured.
fn is_wait_status_line(line: &str) -> bool {
    line.starts_with("当前状态：") || line.starts_with("Current status:")
}

/// Splits `task_wait` output into envelopes.
///
/// The host emits an optional leading notice, blank-line-separated sections with bracketed headers and bodies, then an
/// optional status roll-up. Unrecognized text stays as `notice` so callers can fall back to raw output.
pub fn parse_wait_output(output: &str) -> WaitOutputView {
    fn flush(view: &mut WaitOutputView, open: &mut Option<(String, String)>, body: &mut Vec<&str>) {
        if let Some((agent, status)) = open.take() {
            view.envelopes.push(WaitEnvelopeView { agent, status, body: js_trim(&body.join("\n")).to_owned() });
        }
        body.clear();
    }

    let mut view = WaitOutputView::default();
    let mut notices: Vec<&str> = Vec::new();
    let mut open: Option<(String, String)> = None;
    let mut body: Vec<&str> = Vec::new();
    for raw in output.split('\n') {
        let line = js_trim(raw);
        if let Some((agent, status)) = wait_envelope_header(line) {
            flush(&mut view, &mut open, &mut body);
            open = Some((agent.to_owned(), status.to_owned()));
            continue;
        }
        if is_wait_status_line(line) {
            flush(&mut view, &mut open, &mut body);
            view.status_line = line.to_owned();
            continue;
        }
        if open.is_some() {
            body.push(raw.trim_end_matches(mewrk_pane_tasks::util::is_js_space));
            continue;
        }
        if view.status_line.is_empty() && !line.is_empty() {
            notices.push(line);
        }
    }
    flush(&mut view, &mut open, &mut body);
    view.notice = notices.join("\n");
    view
}

/// Host-authored user contexts include agent-mailbox prose, structured-output nudges and legacy task-result folds. They
/// appear in the timeline but do not answer pending questions. `ctx_agent-result_` excludes persisted legacy folds
/// represented as user contexts; current task-result folds are tool contexts.
const STRUCTURED_OUTPUT_NUDGE_PREFIX: &str = "ctx_structured-output-nudge_";
const HOST_AUTHORED_USER_CONTEXT_PREFIXES: [&str; 3] = ["ctx_agent-result_", "ctx_agent-message_", STRUCTURED_OUTPUT_NUDGE_PREFIX];

/// The reminder a schema-bound child gets mid-run when a round ends without its result.
pub fn is_structured_output_nudge(context: &ContextItem) -> bool {
    matches!(context, ContextItem::User { .. }) && context.id().starts_with(STRUCTURED_OUTPUT_NUDGE_PREFIX)
}

pub fn is_host_authored_user_context(context: &ContextItem) -> bool {
    matches!(context, ContextItem::User { .. }) && HOST_AUTHORED_USER_CONTEXT_PREFIXES.iter().any(|prefix| context.id().starts_with(prefix))
}

/// The result of a question card the user closed without answering (host `ask_user.rs`).
pub const ASK_USER_CLOSED_OUTPUT: &str = "The user closed the question card without answering.";
/// Claude Code's result for a card submitted with nothing answered.
pub const ASK_USER_NO_ANSWER_OUTPUT: &str = "The user did not answer the questions.";
const ASK_USER_ANSWERED_PREFIXES: [&str; 2] = ["Your questions have been answered: ", "The user answered: "];
const ASK_USER_DECLINED_PREFIX: &str = "The user doesn't want to proceed with this tool use.";

/// How an `ask_user` call ended.
///
/// `Legacy` is a call from before questions blocked: its result is only the "asked, paused" receipt, and the answer, if
/// any, is the next real user message. Every newer call carries its outcome in its own result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered,
    Unanswered,
    Closed,
    Declined,
    Failed,
    Legacy,
}

pub fn question_outcome(context: &ToolItem<'_>) -> QuestionOutcome {
    let output = context.result.output.as_str();
    if !context.result.success {
        return if output.starts_with(ASK_USER_DECLINED_PREFIX) { QuestionOutcome::Declined } else { QuestionOutcome::Failed };
    }
    if output == ASK_USER_CLOSED_OUTPUT {
        return QuestionOutcome::Closed;
    }
    if output == ASK_USER_NO_ANSWER_OUTPUT {
        return QuestionOutcome::Unanswered;
    }
    if context.input.get("answers").is_some_and(Value::is_object) || ASK_USER_ANSWERED_PREFIXES.iter().any(|prefix| output.starts_with(prefix)) {
        return QuestionOutcome::Answered;
    }
    QuestionOutcome::Legacy
}

/// A settled pre-blocking `ask_user` receipt, whose answer is the next user message.
pub fn is_legacy_paused_question(context: &Item<'_>) -> bool {
    context.tool().is_some_and(|tool| tool.tool_name == "ask_user" && !tool.streaming && question_outcome(&tool) == QuestionOutcome::Legacy)
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct QuestionAnswerView {
    pub answer: Option<String>,
    pub notes: Option<String>,
}

/// The answers a blocking `ask_user` call ran with, read from the `answers` and `annotations` the host merged into its
/// input (keyed by question text, as in Claude Code), one slot per question.
pub fn question_answers_from_input(input: &JsonObject, questions: &[QuestionItemView]) -> Vec<QuestionAnswerView> {
    let answers = input.get("answers").and_then(Value::as_object);
    let annotations = input.get("annotations").and_then(Value::as_object);
    questions
        .iter()
        .map(|question| QuestionAnswerView {
            answer: answers.and_then(|a| a.get(&question.question)).and_then(Value::as_str).filter(|a| !a.is_empty()).map(str::to_owned),
            notes: annotations
                .and_then(|a| a.get(&question.question))
                .and_then(Value::as_object)
                .and_then(|a| a.get("notes"))
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
        })
        .collect()
}

/// Claude Code's multi-select join: an item containing `", "` or a quote is written as a JSON string, the rest verbatim.
pub fn join_multi_select_answer<S: AsRef<str>>(items: &[S]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|item| {
            let item = item.as_ref();
            if item.contains(", ") || item.contains('"') { json_string(item) } else { item.to_owned() }
        })
        .collect();
    parts.join(", ")
}
