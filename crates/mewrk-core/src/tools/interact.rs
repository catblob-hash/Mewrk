//! `ask_user`, `plan`, `exit_plan_mode`: what goes through the person at the keyboard.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolFuture, ToolResult, future};
use crate::inbox::{ApprovalDecision, RunEvent, RunStatus};

const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
const MAX_QUESTION_CHARS: usize = 4_000;
const MAX_HEADER_CHARS: usize = 120;
const MAX_OPTION_CHARS: usize = 120;
const MAX_OPTION_DESCRIPTION_CHARS: usize = 1_000;
const MAX_OPTION_PREVIEW_CHARS: usize = 16 * 1024;
const MAX_PLAN_CHARS: usize = 200_000;
const MAX_PLAN_TITLE_CHARS: usize = 240;

/// Claude Code's steer for a call with a question that offers fewer than two options, verbatim.
const FEWER_THAN_TWO_OPTIONS: &str = "This call included a question with fewer than 2 options, so it was rejected and the person never saw it. A question with a single option has no decision in it. Do not retry this call and do not invent a filler second option. Instead, state the one path you were going to offer as the approach you are taking, then continue with the task. If this call also contained questions with 2 to 4 options (each with distinct labels), you may re-ask those questions alone in a new call. Ask a question only when the person has at least two genuinely distinct choices.";
const QUESTION_CLOSED: &str = "The user closed the question card without answering.";
const NO_ANSWER: &str = "The user did not answer the questions.";
const REJECT_MESSAGE_WITH_REASON_PREFIX: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). To tell you how to proceed, the user said:\n";
const APPROVED_RESULT_PREFIX: &str = "User has approved your plan.";

pub(super) fn register(v: &mut Vec<Arc<dyn Tool>>) {
    v.push(Arc::new(AskUser));
    v.push(Arc::new(Plan));
    v.push(Arc::new(ExitPlanMode));
}

// ---------------------------------------------------------------- ask_user

struct Question {
    question: String,
    multi_select: bool,
    options: Vec<String>,
}

fn string_field(object: &Value, key: &str, max_chars: usize) -> Result<String, String> {
    let value = object.get(key).ok_or_else(|| format!("missing required argument {key}"))?.as_str().ok_or_else(|| format!("argument {key} must be a string"))?.trim();
    if value.is_empty() {
        return Err(format!("argument {key} must not be empty"));
    }
    if value.chars().nth(max_chars).is_some() {
        return Err(format!("argument {key} exceeds length limit {max_chars}"));
    }
    Ok(value.to_owned())
}

fn parse_question(value: &Value, index: usize) -> Result<(Question, Value), String> {
    let context = |e: String| format!("questions item {}: {e}", index + 1);
    if !value.is_object() {
        return Err(format!("questions item {} must be an object", index + 1));
    }
    let question = string_field(value, "question", MAX_QUESTION_CHARS).map_err(context)?;
    let header = string_field(value, "header", MAX_HEADER_CHARS).map_err(context)?;
    let multi_select = match value.get("multiSelect") {
        None | Some(Value::Null) => false,
        Some(v) => v.as_bool().ok_or_else(|| format!("questions item {}: multiSelect must be a boolean", index + 1))?,
    };
    let raw_options = value.get("options").and_then(Value::as_array).ok_or_else(|| context("options must be an array of objects".to_owned()))?;
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&raw_options.len()) {
        return Err(context(format!("options must contain {MIN_OPTIONS} to {MAX_OPTIONS} items")));
    }
    let mut options = Vec::with_capacity(raw_options.len());
    let mut card_options = Vec::with_capacity(raw_options.len());
    for (i, option) in raw_options.iter().enumerate() {
        let option_context = |e: String| context(format!("options item {}: {e}", i + 1));
        if !option.is_object() {
            return Err(context(format!("options item {} must be an object", i + 1)));
        }
        let label = string_field(option, "label", MAX_OPTION_CHARS).map_err(option_context)?;
        let description = string_field(option, "description", MAX_OPTION_DESCRIPTION_CHARS).map_err(option_context)?;
        let mut card = json!({"label": label, "description": description});
        if option.get("preview").is_some_and(|p| !p.is_null() && p.as_str().is_none_or(|s| !s.trim().is_empty())) {
            card["preview"] = Value::String(string_field(option, "preview", MAX_OPTION_PREVIEW_CHARS).map_err(option_context)?);
        }
        options.push(label);
        card_options.push(card);
    }
    let card = json!({"question": question, "header": header, "multiSelect": multi_select, "options": card_options});
    Ok((Question { question, multi_select, options }, card))
}

/// The questions, checked, and the card to show: what the model sent minus anything it should not have, such as `answers` of its own.
fn parse_questions(args: &Value) -> Result<(Vec<Question>, Value), String> {
    let values = args.get("questions").ok_or("Missing parameter questions")?.as_array().ok_or("questions must be an array of objects")?;
    if values.is_empty() {
        return Err("questions must contain at least 1 item".to_owned());
    }
    if values.len() > MAX_QUESTIONS {
        return Err(format!("questions must not contain more than {MAX_QUESTIONS} items"));
    }
    let too_few = values.iter().any(|v| v.get("options").and_then(Value::as_array).is_some_and(|o| o.len() < MIN_OPTIONS));
    let mut questions = Vec::with_capacity(values.len());
    let mut cards = Vec::with_capacity(values.len());
    for (i, value) in values.iter().enumerate() {
        let (question, card) = parse_question(value, i).map_err(|e| if too_few { format!("{FEWER_THAN_TWO_OPTIONS}\n\n{e}") } else { e })?;
        questions.push(question);
        cards.push(card);
    }
    let mut seen = std::collections::HashSet::new();
    let unique = questions.iter().all(|q| {
        let mut labels = std::collections::HashSet::new();
        seen.insert(q.question.as_str()) && q.options.iter().all(|o| labels.insert(o.as_str()))
    });
    if !unique {
        return Err("Question texts must be unique, option labels must be unique within each question".to_owned());
    }
    Ok((questions, Value::Array(cards)))
}

/// Claude Code's multi-select join: an item containing `", "` or a quote is written as a JSON string, the rest verbatim.
fn join_multi_select(items: &[String]) -> String {
    items
        .iter()
        .map(|item| if item.contains(", ") || item.contains('"') { serde_json::to_string(item).unwrap_or_else(|_| item.clone()) } else { item.clone() })
        .collect::<Vec<_>>()
        .join(", ")
}

fn split_multi_select(answer: &str) -> Option<Vec<String>> {
    let mut items = Vec::new();
    let mut rest = answer;
    loop {
        if rest.starts_with('"') {
            let mut end = None;
            let mut escaped = false;
            for (offset, c) in rest.char_indices().skip(1) {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    end = Some(offset + 1);
                    break;
                }
            }
            let end = end?;
            items.push(serde_json::from_str::<String>(&rest[..end]).ok()?);
            rest = &rest[end..];
            if rest.is_empty() {
                return Some(items);
            }
            rest = rest.strip_prefix(", ")?;
        } else {
            match rest.find(", ") {
                Some(at) => {
                    items.push(rest[..at].to_owned());
                    rest = &rest[at + 2..];
                }
                None => {
                    items.push(rest.to_owned());
                    return Some(items);
                }
            }
        }
    }
}

/// Exactly the options offered, with nothing typed in. Free text is a different kind of answer, and the result says so.
fn answer_is_clean(question: &Question, answer: &str) -> bool {
    let is_label = |item: &str| question.options.iter().any(|o| o == item);
    if is_label(answer) {
        return true;
    }
    if !question.multi_select {
        return false;
    }
    let Some(items) = split_multi_select(answer) else { return false };
    join_multi_select(&items) == answer && items.iter().all(|i| is_label(i))
}

#[derive(Default)]
struct Answers {
    answers: Vec<Option<String>>,
    previews: Vec<Option<String>>,
    notes: Vec<Option<String>>,
}

enum Reply {
    Submit(Answers),
    Close,
    Chat(Answers),
}

fn string_slots(value: Option<&Value>) -> Vec<Option<String>> {
    value.and_then(Value::as_array).map_or_else(Vec::new, |list| list.iter().map(|v| v.as_str().map(str::to_owned)).collect())
}

/// What the card sent back: an array of `{question, answers: [..]}` from this app's card, or Mewrk's `{action, answers, previews, notes}`.
fn parse_reply(questions: &[Question], reply: &Value) -> Reply {
    match reply {
        Value::Null => Reply::Close,
        Value::Array(rows) => {
            let mut out = Answers::default();
            for question in questions {
                let row = rows.iter().find(|r| r.get("question").and_then(Value::as_str) == Some(question.question.as_str()));
                let picked: Vec<String> = row
                    .and_then(|r| r.get("answers"))
                    .and_then(Value::as_array)
                    .map_or_else(Vec::new, |a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect());
                out.answers.push(match picked.len() {
                    0 => None,
                    1 => picked.into_iter().next(),
                    _ => Some(join_multi_select(&picked)),
                });
                out.notes.push(row.and_then(|r| r.get("notes")).and_then(Value::as_str).map(str::to_owned));
            }
            Reply::Submit(out)
        }
        Value::Object(_) => {
            let answers = Answers { answers: string_slots(reply.get("answers")), previews: string_slots(reply.get("previews")), notes: string_slots(reply.get("notes")) };
            match reply.get("action").and_then(Value::as_str).unwrap_or("submit") {
                "close" => Reply::Close,
                "chat" => Reply::Chat(answers),
                _ => Reply::Submit(answers),
            }
        }
        _ => Reply::Close,
    }
}

fn raw_slot(values: &[Option<String>], index: usize) -> Option<&str> {
    values.get(index).and_then(Option::as_deref).filter(|v| !v.is_empty())
}

fn slot(values: &[Option<String>], index: usize) -> Option<&str> {
    values.get(index).and_then(Option::as_deref).map(str::trim).filter(|v| !v.is_empty())
}

/// Worded exactly as Claude Code words them. Whether every answer is clean decides how much care the model is told to take.
fn answered_result(questions: &[Question], reply: &Answers) -> String {
    let mut fragments = Vec::new();
    let mut clean = true;
    for (index, question) in questions.iter().enumerate() {
        let answer = raw_slot(&reply.answers, index);
        let notes = slot(&reply.notes, index);
        let preview = raw_slot(&reply.previews, index);
        if notes.is_some() || answer.is_some_and(|a| !answer_is_clean(question, a)) {
            clean = false;
        }
        if answer.is_none() && notes.is_none() {
            continue;
        }
        let mut parts = vec![match answer {
            Some(answer) => format!("\"{}\"=\"{answer}\"", question.question),
            None => format!("\"{}\"=(no option selected)", question.question),
        }];
        if let Some(preview) = preview {
            parts.push(format!("selected preview:\n{preview}"));
        }
        if let Some(notes) = notes {
            parts.push(format!("notes: {notes}"));
        }
        fragments.push(parts.join(" "));
    }
    if fragments.is_empty() {
        return NO_ANSWER.to_owned();
    }
    let joined = fragments.join(", ");
    if clean {
        format!("Your questions have been answered: {joined}. You can now continue with these answers in mind.")
    } else {
        format!(
            "The user answered: {joined}. Read the answers carefully — they may request clarification, changes, or that you not proceed — and follow what they actually say."
        )
    }
}

fn clarify_feedback(questions: &[Question], reply: &Answers) -> String {
    let blocks = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let mut lines = vec![format!("- \"{}\"", question.question)];
            match raw_slot(&reply.answers, index) {
                Some(answer) => lines.push(format!("  Answer: {answer}")),
                None => lines.push("  (No answer provided)".to_owned()),
            }
            if let Some(notes) = slot(&reply.notes, index) {
                lines.push(format!("  User notes: {notes}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "The user wants to clarify these questions.\n    This means they may have additional information, context or questions for you.\n    Take their response into account and then reformulate the questions if appropriate.\n    Start by asking them what they would like to clarify.\n\n    Questions asked:\n{blocks}"
    )
}

struct AskUser;

impl Tool for AskUser {
    fn name(&self) -> &'static str {
        "ask_user"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn summary(&self, args: &Value) -> String {
        args.get("questions").and_then(Value::as_array).and_then(|q| q.first()).and_then(|q| q.get("question")).and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            if ctx.spec.child.is_some() {
                return Err("ask_user cannot be used in agent contexts".to_owned());
            }
            let (questions, card) = parse_questions(&args)?;
            ctx.run.emit(RunEvent::Status(RunStatus::WaitingForUser));
            let reply = ctx.run.ask_question(ctx.item, card).await;
            ctx.run.emit(RunEvent::Status(RunStatus::RunningTools));
            let Some(reply) = reply else {
                return Err("The run was stopped before the question was answered.".to_owned());
            };
            Ok(match parse_reply(&questions, &reply) {
                Reply::Submit(answers) => ToolResult::ok(answered_result(&questions, &answers)),
                Reply::Close => ToolResult::ok(QUESTION_CLOSED),
                Reply::Chat(answers) => ToolResult::err(format!("{REJECT_MESSAGE_WITH_REASON_PREFIX}{}", clarify_feedback(&questions, &answers))),
            })
        })
    }
}

// ---------------------------------------------------------------- plan

fn plan_title(markdown: &str) -> String {
    let line = markdown.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    line.trim_start_matches('#').trim().chars().take(MAX_PLAN_TITLE_CHARS).collect()
}

struct Plan;

impl Tool for Plan {
    fn name(&self) -> &'static str {
        "plan"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn summary(&self, args: &Value) -> String {
        args.get("action").and_then(Value::as_str).unwrap_or("").to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, args: Value) -> ToolFuture {
        future(async move {
            if ctx.spec.child.is_some() {
                return Err("plan cannot be used in agent contexts".to_owned());
            }
            let action = args.get("action").and_then(Value::as_str).unwrap_or_default().trim();
            match action {
                "write" => {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or_default().trim();
                    if content.is_empty() {
                        return Err("The write action needs the plan's Markdown in `content`.".to_owned());
                    }
                    let characters = content.chars().count();
                    if characters > MAX_PLAN_CHARS {
                        return Err(format!("The plan is {characters} characters, over the {MAX_PLAN_CHARS} character limit. Write the plan, not the code."));
                    }
                    *ctx.session.plan_text.lock() = content.to_owned();
                    Ok(ToolResult::ok(format!(
                        "Plan saved ({characters} characters). The user can read it in the plan panel. Call exit_plan_mode when you are ready for review."
                    )))
                }
                "read" => {
                    let plan = ctx.session.plan_text.lock().clone();
                    Ok(ToolResult::ok(if plan.trim().is_empty() { "No plan has been written yet.".to_owned() } else { plan }))
                }
                other => Err(format!("Unknown plan action `{other}`. Use `write` or `read`.")),
            }
        })
    }
}

struct ExitPlanMode;

impl Tool for ExitPlanMode {
    fn name(&self) -> &'static str {
        "exit_plan_mode"
    }
    fn dangerous(&self, _: &Value) -> bool {
        false
    }
    fn summary(&self, _: &Value) -> String {
        "Plan ready".to_owned()
    }
    fn run(&self, ctx: Arc<ToolCtx>, _: Value) -> ToolFuture {
        future(async move {
            if ctx.spec.child.is_some() {
                return Err("exit_plan_mode cannot be used in agent contexts".to_owned());
            }
            if !ctx.session.plan_mode.load(Ordering::Relaxed) {
                return Err(
                    "Plan mode is off, so there is no plan to ask approval for. The user turns plan mode on from the composer; until then, carry on with the task."
                        .to_owned(),
                );
            }
            let plan = ctx.session.plan_text.lock().clone();
            if plan.trim().is_empty() {
                return Err("No plan has been written yet. Write your plan with the plan tool before calling exit_plan_mode.".to_owned());
            }
            ctx.run.emit(RunEvent::Status(RunStatus::WaitingForUser));
            // Asked whatever the conversation's permission mode: approving the plan is the user's own step, and full access does not answer it.
            let decision = ctx.run.ask_approval(ctx.item, "exit_plan_mode", &json!({"plan": plan}), plan_title(&plan)).await;
            ctx.run.emit(RunEvent::Status(RunStatus::RunningTools));
            match decision {
                Some(ApprovalDecision::Allow | ApprovalDecision::AllowAlways) => {
                    ctx.session.plan_mode.store(false, Ordering::Relaxed);
                    Ok(ToolResult::ok(format!(
                        "{APPROVED_RESULT_PREFIX} You can now start implementing it.\n\nThe plan stays available through the plan tool's read action.\n\n## Approved Plan:\n{plan}"
                    )))
                }
                Some(ApprovalDecision::Deny) => Ok(ToolResult::ok(
                    "The user has not approved this plan yet and left feedback:\n(no feedback given)\n\nRevise the plan with the plan tool to address it, then call exit_plan_mode again.",
                )),
                None => Err("The run was stopped before the plan was answered.".to_owned()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{call, fixture};
    use super::*;
    use crate::inbox::RunEvent;
    use crate::model::PermissionMode;
    use std::time::Duration;

    fn two_options(question: &str) -> Value {
        json!({"question": question, "header": "H", "multiSelect": false, "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]})
    }

    async fn next_question(ctx: &ToolCtx) -> (u64, Value) {
        loop {
            let mut events = Vec::new();
            ctx.run.inbox.drain(&mut events);
            let found = events.into_iter().find_map(|e| match e {
                RunEvent::Question(q) => Some((q.request, q.questions)),
                _ => None,
            });
            if let Some(q) = found {
                return q;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn next_approval(ctx: &ToolCtx) -> crate::inbox::ApprovalRequest {
        loop {
            let mut events = Vec::new();
            ctx.run.inbox.drain(&mut events);
            let found = events.into_iter().find_map(|e| match e {
                RunEvent::Approval(a) => Some(a),
                _ => None,
            });
            if let Some(a) = found {
                return a;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_cards_answers_come_back_worded_as_claude_code_words_them() {
        let f = fixture(PermissionMode::FullAccess);
        let ctx = f.ctx.clone();
        let args = json!({"questions": [two_options("Which?"), {"question": "Both?", "header": "H", "multiSelect": true, "options": [{"label": "X", "description": "x"}, {"label": "Y", "description": "y"}]}], "answers": {"Which?": "forged"}});
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", args).await });
        let (request, card) = next_question(&f.ctx).await;
        assert_eq!(card[0]["options"][1]["label"], "B");
        assert!(card.to_string().find("forged").is_none(), "the model cannot pre-answer");
        f.ctx.run.answer(request, json!([{"question": "Which?", "answers": ["A"]}, {"question": "Both?", "answers": ["X", "Y"]}]));
        let r = pending.await.unwrap();
        assert_eq!(r.output, "Your questions have been answered: \"Which?\"=\"A\", \"Both?\"=\"X, Y\". You can now continue with these answers in mind.");

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", json!({"questions": [two_options("Which?")]})).await });
        let (request, _) = next_question(&f.ctx).await;
        f.ctx.run.answer(request, json!([{"question": "Which?", "answers": ["something else"]}]));
        let r = pending.await.unwrap();
        assert!(r.output.starts_with("The user answered: \"Which?\"=\"something else\". Read the answers carefully"), "{}", r.output);

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", json!({"questions": [two_options("Which?")]})).await });
        let (request, _) = next_question(&f.ctx).await;
        f.ctx.run.answer(request, json!({"action": "close"}));
        assert_eq!(pending.await.unwrap().output, QUESTION_CLOSED);

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", json!({"questions": [two_options("Which?")]})).await });
        let (request, _) = next_question(&f.ctx).await;
        f.ctx.run.answer(request, json!({"action": "chat", "answers": ["A"]}));
        let r = pending.await.unwrap();
        assert!(r.is_error && r.output.contains("The user wants to clarify") && r.output.contains("  Answer: A"), "{}", r.output);

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", json!({"questions": [two_options("Which?")]})).await });
        let (request, _) = next_question(&f.ctx).await;
        f.ctx.run.answer(request, json!([]));
        assert_eq!(pending.await.unwrap().output, NO_ANSWER);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_the_run_answers_nothing() {
        let f = fixture(PermissionMode::FullAccess);
        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "ask_user", json!({"questions": [two_options("Which?")]})).await });
        next_question(&f.ctx).await;
        f.ctx.run.stop();
        let r = pending.await.unwrap();
        assert!(r.is_error && r.output.contains("stopped before the question was answered"), "{}", r.output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_questions_never_reach_the_user() {
        let f = fixture(PermissionMode::FullAccess);
        let ask = |args: Value| {
            let ctx = f.ctx.clone();
            async move { call(&ctx, "ask_user", args).await.output }
        };
        assert_eq!(ask(json!({"questions": []})).await, "questions must contain at least 1 item");
        assert!(
            ask(json!({"questions": [{"question": "q", "header": "h", "options": [{"label": "only", "description": "d"}]}]}))
                .await
                .starts_with("This call included a question with fewer than 2 options")
        );
        assert_eq!(
            ask(json!({"questions": [two_options("same"), two_options("same")]})).await,
            "Question texts must be unique, option labels must be unique within each question"
        );
        let five: Vec<Value> = (0..5).map(|i| two_options(&format!("q{i}"))).collect();
        assert!(ask(json!({"questions": five})).await.contains("more than 4"));
    }

    #[test]
    fn multi_select_answers_round_trip() {
        let items = vec!["plain".to_owned(), "has, comma".to_owned(), "has \"quote\"".to_owned()];
        let joined = join_multi_select(&items);
        assert_eq!(joined, r#"plain, "has, comma", "has \"quote\"""#);
        assert_eq!(split_multi_select(&joined).unwrap(), items);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_plan_is_written_read_and_approved() {
        let f = fixture(PermissionMode::FullAccess);
        assert_eq!(call(&f.ctx, "plan", json!({"action": "read"})).await.output, "No plan has been written yet.");
        assert!(call(&f.ctx, "plan", json!({"action": "write", "content": " "})).await.is_error);
        assert!(call(&f.ctx, "plan", json!({"action": "burn"})).await.output.contains("Unknown plan action"));
        let r = call(&f.ctx, "exit_plan_mode", json!({})).await;
        assert!(r.is_error && r.output.starts_with("Plan mode is off"), "{}", r.output);

        f.ctx.session.plan_mode.store(true, Ordering::Relaxed);
        assert!(call(&f.ctx, "exit_plan_mode", json!({})).await.output.starts_with("No plan has been written"));
        let r = call(&f.ctx, "plan", json!({"action": "write", "content": "# The plan\n\nDo it.\n"})).await;
        assert!(r.output.starts_with("Plan saved (18 characters)"), "{}", r.output);
        assert_eq!(*f.ctx.session.plan_text.lock(), "# The plan\n\nDo it.");
        assert_eq!(call(&f.ctx, "plan", json!({"action": "read"})).await.output, "# The plan\n\nDo it.");

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "exit_plan_mode", json!({})).await });
        let card = next_approval(&f.ctx).await;
        assert_eq!(card.summary, "The plan");
        assert_eq!(card.args["plan"], "# The plan\n\nDo it.");
        f.ctx.run.approve(card.request, ApprovalDecision::Deny);
        let r = pending.await.unwrap();
        assert!(r.output.contains("has not approved this plan yet"), "{}", r.output);
        assert!(f.ctx.session.plan_mode.load(Ordering::Relaxed));

        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "exit_plan_mode", json!({})).await });
        let card = next_approval(&f.ctx).await;
        f.ctx.run.approve(card.request, ApprovalDecision::Allow);
        let r = pending.await.unwrap();
        assert!(r.output.starts_with("User has approved your plan.") && r.output.ends_with("## Approved Plan:\n# The plan\n\nDo it."), "{}", r.output);
        assert!(!f.ctx.session.plan_mode.load(Ordering::Relaxed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_leaves_plan_mode_on() {
        let f = fixture(PermissionMode::FullAccess);
        f.ctx.session.plan_mode.store(true, Ordering::Relaxed);
        *f.ctx.session.plan_text.lock() = "p".to_owned();
        let ctx = f.ctx.clone();
        let pending = tokio::spawn(async move { call(&ctx, "exit_plan_mode", json!({})).await });
        next_approval(&f.ctx).await;
        f.ctx.run.stop();
        assert!(pending.await.unwrap().is_error);
        assert!(f.ctx.session.plan_mode.load(Ordering::Relaxed));
    }
}
