//! `components/InlineQuestionEditor.tsx`: edits a question card in place. The question count is fixed because the host only
//! accepts a rewritten `ask_user` input that keeps its pending result and question count intact.

use std::hash::Hash;

use mewrk_client::model::{ContextItem, JsonObject};
use mewrk_client::prelude::*;
use mewrk_pane_tasks::widgets::spinner;
use serde_json::Map;

use crate::inline_tool_editor::ToolDone;
use crate::kit::{action_button, action_button_disabled};
use crate::orchestration::{answers_from_formatted_content, format_question_answers, is_claude_question_input, QuestionItemView, QuestionOptionView};

pub type SaveQuestion = Rc<dyn Fn(&mut App, JsonObject, Option<String>, ToolDone)>;

const fn style(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

const SUMMARY: TypeStyle = style(11.5, 16.0, Weight::Semibold);
const SMALL: TypeStyle = style(9.5, 13.3, Weight::Regular);
const QUESTION_HEADING: TypeStyle = style(13.0, 18.0, Weight::Semibold);
const LABEL: TypeStyle = style(11.5, 16.0, Weight::Semibold);
const INPUT: TypeStyle = style(12.0, 18.0, Weight::Regular);
const COMPACT: TypeStyle = TypeStyle { size: 10.5, line: 16.0, weight: Weight::Regular, mono: true, italic: false, serif: false };
const COUNT: TypeStyle = style(9.0, 12.0, Weight::Regular);
const INDEX: TypeStyle = style(8.5, 11.0, Weight::Regular);
const ALERT: TypeStyle = style(10.5, 15.75, Weight::Regular);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionOptionDraft {
    pub label: String,
    pub description: String,
    pub preview: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionDraft {
    pub header: String,
    pub question: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOptionDraft>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OptionValidationErrors {
    pub label: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionValidationErrors {
    pub header: Option<String>,
    pub question: Option<String>,
    pub options: Option<String>,
    pub answer: Option<String>,
    pub option_errors: Vec<OptionValidationErrors>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ValidationErrors {
    pub form: Option<String>,
    pub questions: Vec<QuestionValidationErrors>,
}

fn string_from(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or("").to_owned()
}

fn option_drafts_from(value: Option<&Value>) -> Vec<QuestionOptionDraft> {
    let Some(options) = value.and_then(Value::as_array) else { return Vec::new() };
    options
        .iter()
        .map(|option| match option {
            Value::String(label) => QuestionOptionDraft { label: label.clone(), ..QuestionOptionDraft::default() },
            other => QuestionOptionDraft { label: string_from(other.get("label")), description: string_from(other.get("description")), preview: string_from(other.get("preview")) },
        })
        .collect()
}

pub fn question_drafts_from_input(input: &JsonObject) -> Vec<QuestionDraft> {
    if let Some(questions) = input.get("questions").and_then(Value::as_array) {
        return questions
            .iter()
            .map(|question| QuestionDraft { header: string_from(question.get("header")), question: string_from(question.get("question")), multi_select: question.get("multiSelect") == Some(&Value::Bool(true)), options: option_drafts_from(question.get("options")) })
            .collect();
    }
    if let Some(question) = input.get("question").and_then(Value::as_str) {
        return vec![QuestionDraft { header: "Question".to_owned(), question: question.to_owned(), multi_select: false, options: option_drafts_from(input.get("options")) }];
    }
    Vec::new()
}

fn question_views_from_drafts(drafts: &[QuestionDraft]) -> Vec<QuestionItemView> {
    drafts
        .iter()
        .map(|draft| QuestionItemView {
            header: draft.header.trim().to_owned(),
            question: draft.question.trim().to_owned(),
            multi_select: draft.multi_select,
            options: draft
                .options
                .iter()
                .map(|option| QuestionOptionView { label: option.label.trim().to_owned(), description: option.description.trim().to_owned(), preview: Some(option.preview.trim().to_owned()).filter(|preview| !preview.is_empty()) })
                .collect(),
        })
        .collect()
}

pub fn input_from_questions(questions: &[QuestionItemView]) -> JsonObject {
    let mut input = Map::new();
    let list: Vec<Value> = questions
        .iter()
        .map(|question| {
            let options: Vec<Value> = question
                .options
                .iter()
                .map(|option| {
                    let mut entry = Map::new();
                    entry.insert("label".to_owned(), Value::String(option.label.clone()));
                    entry.insert("description".to_owned(), Value::String(option.description.clone()));
                    if let Some(preview) = &option.preview {
                        entry.insert("preview".to_owned(), Value::String(preview.clone()));
                    }
                    Value::Object(entry)
                })
                .collect();
            json!({ "header": question.header, "question": question.question, "multiSelect": question.multi_select, "options": options })
        })
        .collect();
    input.insert("questions".to_owned(), Value::Array(list));
    input
}

fn initial_answers(answer: Option<&str>, questions: &[QuestionDraft]) -> Vec<String> {
    let Some(answer) = answer else { return Vec::new() };
    if let Some(parsed) = answers_from_formatted_content(answer, &question_views_from_drafts(questions)) {
        return parsed;
    }
    questions.iter().enumerate().map(|(index, _)| if index == 0 { answer.to_owned() } else { String::new() }).collect()
}

pub fn has_validation_errors(errors: &ValidationErrors) -> bool {
    errors.form.is_some() || errors.questions.iter().any(|q| q.header.is_some() || q.question.is_some() || q.options.is_some() || q.answer.is_some() || q.option_errors.iter().any(|o| o.label.is_some() || o.description.is_some()))
}

/// Every question and option checked at once, so a save shows everything wrong with the form rather than the first thing. The
/// rewritten input and answer come back only when nothing is.
pub fn validate(questions: &[QuestionDraft], answers: &[String], has_answer: bool) -> (ValidationErrors, Option<(JsonObject, Option<String>)>) {
    let mut errors = ValidationErrors {
        form: None,
        questions: questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let header = question.header.trim();
                QuestionValidationErrors {
                    option_errors: question
                        .options
                        .iter()
                        .map(|option| OptionValidationErrors {
                            label: option.label.trim().is_empty().then(|| t("选项名称不能为空", "Option label cannot be empty").to_owned()),
                            description: option.description.trim().is_empty().then(|| t("选项说明不能为空", "Option description cannot be empty").to_owned()),
                        })
                        .collect(),
                    header: if header.is_empty() {
                        Some(t("标题不能为空", "Header cannot be empty").to_owned())
                    } else if header.chars().count() > 12 {
                        Some(t("标题不能超过 12 个字符", "Header cannot exceed 12 characters").to_owned())
                    } else {
                        None
                    },
                    question: question.question.trim().is_empty().then(|| t("问题内容不能为空", "Question cannot be empty").to_owned()),
                    options: (question.options.len() < 2 || question.options.len() > 4).then(|| t("每个问题必须保留 2 至 4 个选项", "Each question must retain 2 to 4 options").to_owned()),
                    answer: (has_answer && answers.get(index).is_none_or(|answer| answer.trim().is_empty())).then(|| t("回答不能为空", "Answer cannot be empty").to_owned()),
                }
            })
            .collect(),
    };
    if questions.is_empty() || questions.len() > 4 {
        errors.form = Some(t("提问消息必须包含 1 至 4 个问题", "A question message must contain 1 to 4 questions").to_owned());
    }
    let views = question_views_from_drafts(questions);
    let input = input_from_questions(&views);
    if !is_claude_question_input(&input) && !has_validation_errors(&errors) {
        errors.form = Some(t("提问参数不符合 Claude Code AskUserQuestion 格式", "The questions do not match the Claude Code AskUserQuestion format").to_owned());
    }
    if has_validation_errors(&errors) {
        return (errors, None);
    }
    let answer_content = has_answer.then(|| format_question_answers(&views, answers));
    (errors, Some((input, answer_content)))
}

pub struct QuestionEditorProps {
    pub item: ContextItem,
    pub answer: Option<ContextItem>,
    pub on_close: Handler<App>,
    pub on_save: SaveQuestion,
}

struct EditorState {
    questions: Vec<QuestionDraft>,
    answers: Vec<String>,
    validation: Option<ValidationErrors>,
    save_error: Option<String>,
    saving: bool,
}

impl EditorState {
    fn edited(&mut self) {
        self.validation = None;
        self.save_error = None;
    }
}

/// `validate`, then `on_save`, with the form locked until it answers.
fn submit(state: &Local<EditorState>, has_answer: bool, on_save: &SaveQuestion, app: &mut App) {
    if state.borrow().saving {
        return;
    }
    let (errors, validated) = state.with(|s| validate(&s.questions, &s.answers, has_answer));
    state.update(|s| {
        s.save_error = None;
        s.validation = Some(errors);
    });
    let Some((input, answer_content)) = validated else { return };
    state.update(|s| s.saving = true);
    let done = {
        let state = state.clone();
        Box::new(move |_: &mut App, result: Result<(), String>| {
            state.update(|s| {
                s.save_error = result.err();
                s.saving = false;
            });
        })
    };
    on_save(app, input, answer_content, done);
}

fn question_updater(state: &Local<EditorState>, question: usize, apply: impl Fn(&mut QuestionDraft, String) + 'static) -> Rc<dyn Fn(&mut App, String)> {
    let state = state.clone();
    Rc::new(move |_, value| {
        state.update(|s| {
            // `disabled` while saving: a controlled field whose change is not taken goes back to what it was given.
            if s.saving {
                return;
            }
            if let Some(draft) = s.questions.get_mut(question) {
                apply(draft, value);
            }
            s.edited();
        });
    })
}

fn option_updater(state: &Local<EditorState>, question: usize, option: usize, apply: impl Fn(&mut QuestionOptionDraft, String) + 'static) -> Rc<dyn Fn(&mut App, String)> {
    question_updater(state, question, move |draft, value| {
        if let Some(target) = draft.options.get_mut(option) {
            apply(target, value);
        }
    })
}

/// One boxed control under a label, as `.question-editor__field` frames it.
struct Control {
    key: (&'static str, u64, usize, usize),
    /// `aria-label`: what a label over the box says, with which question and option it is, for a reader who is not looking at either.
    label: Option<String>,
    value: String,
    ty: TypeStyle,
    /// `None` is one line, as tall as its text; a textarea keeps its minimum.
    min_h: Option<f32>,
    invalid: bool,
    on_change: Rc<dyn Fn(&mut App, String)>,
}

/// Keys and the implicit submit of a single-line input live on the form, which is what hears them upstream.
#[derive(Clone)]
struct FormKeys {
    on_key: Rc<dyn Fn(&mut App, &KeyPress) -> bool>,
    on_submit: Rc<dyn Fn(&mut App)>,
}

fn control(c: Control, keys: &FormKeys, saving: bool) -> E {
    let t_ = theme();
    let focused = focused_id() == hash_key(("field", c.key));
    let border = if focused { t_.accent } else if c.invalid { t_.critical } else { t_.card_stroke };
    let on_change = c.on_change.clone();
    let on_key = keys.on_key.clone();
    let mut input = field(c.key, c.value).ty(c.ty).bare().controlled().on_change(move |app, value| on_change(app, value)).on_key(move |app, press| on_key(app, press));
    input = match c.min_h {
        Some(min_h) => input.multiline(min_h - 8.0, 600.0),
        None => {
            let on_submit = keys.on_submit.clone();
            // A form's single line submits on Enter, which `multiline` turned off to size the box by its text.
            input.multiline(c.ty.line, c.ty.line).submit_on_enter(true).on_submit(move |app, _| on_submit(app))
        }
    };
    let built = input.build().aria_invalid(c.invalid).aria_disabled(saving);
    let built = match c.label {
        Some(label) => built.aria_label(label),
        None => built,
    };
    div().min_w(0.0).py(3.0).px(7.0).radius(6.0).border(border).bg(t_.control_fill).disabled(saving).child(built)
}

/// `.question-editor__option-index`: `writing-mode: vertical-rl`, in the option's left padding. Fluent draws no sideways text, so a word
/// stands a character to a line, which is how a vertical line of Chinese reads anyway.
fn vertical_label(key: impl Hash, label: &'static str, color: Color) -> E {
    let height = label.chars().count() as f32 * INDEX.line;
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let mut buffer = [0u8; 4];
        for (line, character) in label.chars().enumerate() {
            ui.label_centered(rect(r.x, r.y + line as f32 * INDEX.line, r.w, INDEX.line), character.encode_utf8(&mut buffer), &INDEX, color);
        }
    })
    .key(key)
    .absolute()
    .top(19.0)
    .left(0.0)
    .size(INDEX.size + 2.0, height)
    .pass_through()
}

/// `<Field>`: `.field`, with the margin `.question-editor__field .field` gives it.
fn labelled(label: &'static str, control: E, hint: Option<(String, bool)>, margin: f32) -> E {
    let t_ = theme();
    col()
        .gap(6.0)
        .mb(margin)
        .min_w(0.0)
        .child(text(label).ty(LABEL).color(t_.text_primary))
        .child(control)
        .child(hint.map(|(hint, is_error)| text(hint).ty(SMALL).color(if is_error { t_.critical } else { t_.text_tertiary }).mt(-2.0)))
}

/// A form's field key.
pub fn question_field_key(editor: u64, name: &'static str, question: usize, option: usize) -> (&'static str, u64, usize, usize) {
    (name, editor, question, option)
}

pub fn inline_question_editor(key: impl Hash + Clone, props: QuestionEditorProps) -> E {
    let t_ = theme();
    let k = hash_key(key);
    let QuestionEditorProps { item, answer, on_close, on_save } = props;
    let answer_content = match &answer {
        Some(ContextItem::User { content, .. }) => Some(content.clone()),
        _ => None,
    };
    let has_answer = answer_content.is_some();
    let state = use_state(("question-editor", k), || {
        let empty = JsonObject::new();
        let input = match &item {
            ContextItem::Tool { input, .. } => input,
            _ => &empty,
        };
        let questions = question_drafts_from_input(input);
        let answers = initial_answers(answer_content.as_deref(), &questions);
        EditorState { questions, answers, validation: None, save_error: None, saving: false }
    });
    let saving = state.borrow().saving;

    let keys = {
        let on_key = {
            let (state, on_close) = (state.clone(), on_close.clone());
            Rc::new(move |app: &mut App, press: &KeyPress| {
                if press.key == Key::Escape && !state.borrow().saving {
                    on_close(app);
                    return true;
                }
                false
            })
        };
        let on_submit = {
            let (state, on_save) = (state.clone(), on_save.clone());
            Rc::new(move |app: &mut App| submit(&state, has_answer, &on_save, app))
        };
        FormKeys { on_key, on_submit }
    };

    let s = state.borrow();
    let count = s.questions.len();
    let summary = row()
        .items_center()
        .gap(8.0)
        .pb(14.0)
        .border_bottom(t_.card_stroke)
        .child(text(tf("{count} 个问题", "{count} questions", &[("count", &count)])).ty(SUMMARY).color(t_.text_primary).nowrap())
        .child(text(t("问题数固定，每题 2 至 4 个选项。", "The question count is fixed; each has 2 to 4 options.")).ty(SMALL).color(t_.text_tertiary));

    let narrow = window_size().0 <= 640.0;
    let sections = s.questions.iter().enumerate().map(|(qi, question)| {
        let errors = s.validation.as_ref().and_then(|v| v.questions.get(qi));
        let option_count = question.options.len();

        let heading = row()
            .items_center()
            .gap(8.0)
            .min_h(29.0)
            .mb(13.0)
            .child(text(t("问题", "Question")).key(question_field_key(k, "q-heading", qi, 0)).ty(QUESTION_HEADING).color(t_.text_primary).nowrap().role(Role::Heading).aria_level(3))
            .child(text(if question.header.is_empty() { t("未填写标题", "Untitled").to_owned() } else { question.header.clone() }).ty(SMALL).color(t_.text_tertiary).ellipsis());

        let question_label = |zh: &str, en: &str| a11y_active().then(|| tf(zh, en, &[("index", &(qi + 1))]));
        let option_label = |zh: &str, en: &str, oi: usize| a11y_active().then(|| tf(zh, en, &[("question", &(qi + 1)), ("option", &(oi + 1))]));
        let header = labelled(
            t("标题", "Header"),
            control(Control { key: question_field_key(k, "q-header", qi, 0), label: question_label("第 {index} 题标题", "Question {index} header"), value: question.header.clone(), ty: INPUT, min_h: None, invalid: errors.is_some_and(|e| e.header.is_some()), on_change: question_updater(&state, qi, |draft, value| draft.header = value) }, &keys, saving),
            Some(match errors.and_then(|e| e.header.clone()) {
                Some(message) => (message, true),
                None => (t("最多 12 个字符", "Up to 12 characters").to_owned(), false),
            }),
            13.0,
        );
        let prompt = labelled(
            t("问题内容", "Question"),
            control(Control { key: question_field_key(k, "q-question", qi, 0), label: question_label("第 {index} 题问题内容", "Question {index} text"), value: question.question.clone(), ty: INPUT, min_h: Some(62.0), invalid: errors.is_some_and(|e| e.question.is_some()), on_change: question_updater(&state, qi, |draft, value| draft.question = value) }, &keys, saving),
            errors.and_then(|e| e.question.clone()).map(|message| (message, true)),
            13.0,
        );

        let multi = {
            let state = state.clone();
            let on = question.multi_select;
            row()
                .items_center()
                .gap(14.0)
                .min_h(49.0)
                .mt(1.0)
                .mb(14.0)
                .border_bottom(t_.card_stroke)
                .child(div().flex1().min_w(0.0).child(text(t("允许多选", "Allow multiple selections")).ty(LABEL).color(t_.text_primary)))
                .child(div().key(question_field_key(k, "q-multi", qi, 0)).disabled(saving).child(switch(on, move |_| {
                    state.update(|s| {
                        if let Some(draft) = s.questions.get_mut(qi) {
                            draft.multi_select = !on;
                        }
                        s.edited();
                    });
                }).when(a11y_active(), |e| e.aria_label(tf("第 {index} 题允许多选", "Allow multiple selections for question {index}", &[("index", &(qi + 1))])))))
        };

        let add_label = tf("为第 {index} 题增加选项", "Add an option to question {index}", &[("index", &(qi + 1))]);
        let add_key = question_field_key(k, "q-add", qi, 0);
        let add = if saving || option_count >= 4 {
            action_button_disabled(add_key, "Plus", add_label)
        } else {
            let state = state.clone();
            action_button(add_key, "Plus", add_label, move |_| {
                state.update(|s| {
                    if let Some(draft) = s.questions.get_mut(qi)
                        && draft.options.len() < 4
                    {
                        draft.options.push(QuestionOptionDraft::default());
                    }
                    s.edited();
                });
            })
        };
        let options_heading = row()
            .items_center()
            .gap(7.0)
            .min_h(31.0)
            .child(text(t("选项", "Options")).ty(LABEL).color(t_.text_primary).nowrap())
            .child(text(tf("{count} 个", "{count}", &[("count", &option_count)])).ty(COUNT).color(t_.text_tertiary).nowrap())
            // A spacer rather than `margin-left: auto`, which would cost the row its gap: see `turn_changes::counts`.
            .child(spacer())
            .child(add);
        let section_error = errors.and_then(|e| e.options.clone()).map(|message| text(message).key(question_field_key(k, "q-options-error", qi, 0)).ty(SMALL).color(t_.critical).mt(-4.0).mb(9.0));

        let options = col().children(question.options.iter().enumerate().map(|(oi, option)| {
            let option_errors = errors.and_then(|e| e.option_errors.get(oi));
            let remove_label = tf("删除第 {question} 题的选项 {option}", "Remove option {option} from question {question}", &[("question", &(qi + 1)), ("option", &(oi + 1))]);
            let remove_key = question_field_key(k, "q-remove", qi, oi);
            let remove = if saving || option_count <= 2 {
                action_button_disabled(remove_key, "Trash2", remove_label)
            } else {
                let state = state.clone();
                action_button(remove_key, "Trash2", remove_label, move |_| {
                    state.update(|s| {
                        if let Some(draft) = s.questions.get_mut(qi)
                            && draft.options.len() > 2
                            && oi < draft.options.len()
                        {
                            draft.options.remove(oi);
                        }
                        s.edited();
                    });
                })
            };
            let label_control = labelled(
                t("名称", "Label"),
                control(Control { key: question_field_key(k, "q-option-label", qi, oi), label: option_label("第 {question} 题选项 {option} 名称", "Question {question} option {option} label", oi), value: option.label.clone(), ty: INPUT, min_h: None, invalid: option_errors.is_some_and(|e| e.label.is_some()), on_change: option_updater(&state, qi, oi, |option, value| option.label = value) }, &keys, saving),
                option_errors.and_then(|e| e.label.clone()).map(|message| (message, true)),
                13.0,
            );
            let description_control = labelled(
                t("说明", "Description"),
                control(Control { key: question_field_key(k, "q-option-description", qi, oi), label: option_label("第 {question} 题选项 {option} 说明", "Question {question} option {option} description", oi), value: option.description.clone(), ty: INPUT, min_h: None, invalid: option_errors.is_some_and(|e| e.description.is_some()), on_change: option_updater(&state, qi, oi, |option, value| option.description = value) }, &keys, saving),
                option_errors.and_then(|e| e.description.clone()).map(|message| (message, true)),
                13.0,
            );
            let preview_control = labelled(
                t("预览（可选）", "Preview (optional)"),
                control(Control { key: question_field_key(k, "q-option-preview", qi, oi), label: option_label("第 {question} 题选项 {option} 预览", "Question {question} option {option} preview", oi), value: option.preview.clone(), ty: COMPACT, min_h: Some(44.0), invalid: false, on_change: option_updater(&state, qi, oi, |option, value| option.preview = value) }, &keys, saving),
                Some((t("保留代码或长文本的原始换行。", "Line breaks in code or long text are preserved.").to_owned(), false)),
                13.0,
            );
            // `grid-template-columns: minmax(130px, .65fr) minmax(180px, 1fr)`, one column at 640 px and under.
            let pair: E = if narrow {
                col().child(label_control).child(description_control)
            } else {
                row().gap(14.0).items_start().child(div().flex_grow(0.65).basis(0.0).min_w(130.0).child(label_control)).child(div().flex_grow(1.0).basis(0.0).min_w(180.0).child(description_control))
            };
            div()
                .key(question_field_key(k, "q-option", qi, oi))
                .pt(10.0)
                .pr(30.0)
                .pb(1.0)
                .pl(21.0)
                .when(oi > 0, |e| e.border_top(t_.card_stroke))
                .child(vertical_label(("q-option-index", k, qi, oi), t("选项", "Option"), t_.text_tertiary))
                .child(div().absolute().top(11.0).right(0.0).child(remove))
                .child(pair)
                .child(preview_control)
        }));

        let answer_block = has_answer.then(|| {
            let value = s.answers.get(qi).cloned().unwrap_or_default();
            let on_change: Rc<dyn Fn(&mut App, String)> = {
                let state = state.clone();
                Rc::new(move |_, value| {
                    state.update(|s| {
                        if s.saving {
                            return;
                        }
                        let length = s.questions.len();
                        s.answers.resize(length, String::new());
                        if let Some(slot) = s.answers.get_mut(qi) {
                            *slot = value;
                        }
                        s.edited();
                    });
                })
            };
            let hint = match errors.and_then(|e| e.answer.clone()) {
                Some(message) => (message, true),
                None => (t("保存时按修改后的问题重新对应回答。", "Saving re-matches the answer to the edited question.").to_owned(), false),
            };
            div()
                .mt(9.0)
                .pt(15.0)
                .border_top(t_.card_stroke)
                .child(labelled(t("对应回答", "Answer"), control(Control { key: question_field_key(k, "q-answer", qi, 0), label: question_label("第 {index} 题回答", "Answer to question {index}"), value, ty: INPUT, min_h: Some(62.0), invalid: errors.is_some_and(|e| e.answer.is_some()), on_change }, &keys, saving), Some(hint), 9.0))
        });

        col()
            .key(question_field_key(k, "q-section", qi, 0))
            .pt(18.0)
            .pb(4.0)
            .min_w(0.0)
            .when(qi > 0, |e| e.border_top(t_.card_stroke))
            .role(Role::Region)
            .aria_labelledby(question_field_key(k, "q-heading", qi, 0))
            .child(heading)
            .child(header)
            .child(prompt)
            .child(multi)
            .child(options_heading)
            .child(section_error)
            .child(options)
            .child(answer_block)
    });

    let empty = (count == 0).then(|| text(t("这条消息没有可编辑的问题，无法保存。", "This message has no editable questions and cannot be saved.")).key(("question-editor-empty", k)).ty(ALERT).color(t_.critical).mt(12.0).role(Role::Alert));
    let invalid = s.validation.as_ref().filter(|v| has_validation_errors(v)).map(|v| {
        let message = v.form.clone().unwrap_or_else(|| t("请修正标记的字段后再保存。", "Fix the marked fields before saving.").to_owned());
        text(message).key(("question-editor-invalid", k)).ty(ALERT).color(t_.critical).mt(12.0).pt(10.0).border_top(t_.card_stroke).role(Role::Alert)
    });
    let failed = s.save_error.as_ref().map(|error| text(tf("保存失败：{error}", "Failed to save: {error}", &[("error", error)])).key(("question-editor-failed", k)).ty(ALERT).color(t_.critical).mt(12.0).pt(10.0).border_top(t_.card_stroke).role(Role::Alert));

    let cancel = {
        let on_close = on_close.clone();
        subtle_button(t("取消", "Cancel")).key(("question-editor-cancel", k)).disabled(saving).when(!saving, move |e| e.on_click(move |app| on_close(app)))
    };
    let can_save = !saving && count > 0;
    let save_label = if saving { t("正在保存…", "Saving…") } else { t("保存", "Save") };
    let on_submit = keys.on_submit.clone();
    let on_accent = t_.text_on_accent;
    let mark: E = if saving { spinner(15.0, on_accent) } else { icon("Check", 15.0).color(on_accent) };
    let save = accent_button("").key(("question-editor-save", k)).disabled(!can_save).when(can_save, move |e| e.on_click(move |app| on_submit(app))).child(mark).child(text(save_label).ty(BODY_SMALL).color(on_accent).nowrap());
    let footer = row().justify_end().items_center().gap(8.0).mt(14.0).pt(12.0).border_top(t_.card_stroke).child(cancel).child(save);

    let on_key = keys.on_key.clone();
    col().key(("question-editor", k)).min_w(0.0).role(Role::Form).aria_label(if has_answer { t("编辑提问与回答", "Edit questions and answers") } else { t("编辑提问", "Edit questions") }).on_key(move |app: &mut App, press: &KeyPress| on_key(app, press)).child(summary).children(sections).child(empty).child(invalid).child(failed).child(footer)
}
