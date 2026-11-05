//! `components/QuestionTimelineCard.tsx`: a question as the timeline keeps it, with what the user said back.

use fluent::el::PointerEvent;
use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;

use crate::inline_question_editor::{inline_question_editor, QuestionEditorProps, SaveQuestion};
use crate::inline_tool_editor::tool_context;
use crate::item::ToolItem;
use crate::kit::action_button;
use crate::orchestration::{answers_from_formatted_content, question_answers_from_input, question_outcome, questions_from_input, QuestionOutcome};

const fn style(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

/// `.question-history__header > strong`.
const TITLE: TypeStyle = style(10.5, 15.0, Weight::Semibold);
/// `.question-history__header > span` and every other small line.
const SMALL: TypeStyle = style(9.5, 13.0, Weight::Regular);
const SMALL_STRONG: TypeStyle = style(9.5, 13.0, Weight::Semibold);
/// `.question-history__prompt p`.
const PROMPT: TypeStyle = style(13.0, 20.5, Weight::Regular);
/// `.question-history__output p`.
const OUTPUT: TypeStyle = style(12.5, 19.75, Weight::Regular);

pub struct QuestionCardProps<'a> {
    pub item: ToolItem<'a>,
    pub index: usize,
    pub answer: Option<&'a ContextItem>,
    pub answer_index: Option<usize>,
    pub read_only: bool,
    /// Replaces the card body with the question form while true.
    pub editing: bool,
    pub selected: bool,
    pub on_edit: Option<Handler<App>>,
    pub on_delete: Option<Handler<App>>,
    pub on_cancel_edit: Option<Handler<App>>,
    pub on_save_question: Option<SaveQuestion>,
    pub on_open_insert: Option<Rc<dyn Fn(&mut App, PointerEvent, usize)>>,
}

/// The word beside the title. A question from before questions blocked is answered by the user message after it; a newer one
/// carries its answers in its own executed input.
pub fn question_status(outcome: QuestionOutcome, answered_by_message: bool) -> &'static str {
    match outcome {
        QuestionOutcome::Legacy if answered_by_message => t("已回答", "Answered"),
        QuestionOutcome::Legacy => t("未回答", "Unanswered"),
        QuestionOutcome::Answered => t("已回答", "Answered"),
        QuestionOutcome::Unanswered => t("未回答", "Unanswered"),
        QuestionOutcome::Closed => t("已关闭", "Closed"),
        QuestionOutcome::Declined => t("想先聊聊", "Wanted to chat first"),
        QuestionOutcome::Failed => t("失败", "Failed"),
    }
}

/// `.question-history__output`.
fn output(label: &'static str, value: String) -> E {
    let t_ = theme();
    col().gap(1.0).min_w(0.0).child(text(label).ty(SMALL).color(t_.text_tertiary)).child(text(value).ty(OUTPUT).color(t_.text_secondary).selectable())
}

pub fn question_timeline_card(props: QuestionCardProps<'_>) -> E {
    let t_ = theme();
    let QuestionCardProps { item, index, answer, answer_index, read_only, editing, selected, on_edit, on_delete, on_cancel_edit, on_save_question, on_open_insert } = props;
    let id = item.id;
    let questions = questions_from_input(item.input);
    let outcome = question_outcome(&item);
    let legacy = outcome == QuestionOutcome::Legacy;
    let answer_content = match answer {
        Some(ContextItem::User { content, .. }) => Some(content.as_str()),
        _ => None,
    };
    let has_answer = answer_content.is_some();
    let parsed_answers = answer_content.and_then(|content| answers_from_formatted_content(content, &questions));
    let blocking_answers = (outcome == QuestionOutcome::Answered).then(|| question_answers_from_input(item.input, &questions));
    let status = question_status(outcome, has_answer);
    let after_index = answer_index.map_or(index + 1, |at| at + 1);
    let editing_in_place = editing && on_cancel_edit.is_some() && on_save_question.is_some();
    let failed = outcome == QuestionOutcome::Failed;

    // Across the column, so its controls end on the column every other card's do.
    let mark = if failed {
        t_.critical
    } else if has_answer || outcome == QuestionOutcome::Answered {
        t_.success
    } else {
        t_.text_tertiary
    };
    // `.question-history:hover .question-history__actions`: the card's hover, where a leaf's reveal only hears its own row's.
    let hovered = use_state(("question-hover", id), || false);
    let actions = (!read_only && !editing_in_place).then(|| {
        // A timeline with nowhere to commit a rewritten question gets no pencil rather than one that opens an editor with no Save.
        let pencil = on_edit.filter(|_| legacy).map(|on_edit| {
            let label = if has_answer { t("编辑提问与回答", "Edit questions and answers") } else { t("编辑提问", "Edit questions") };
            action_button(("question-edit", id), "Pencil", label, move |app| on_edit(app))
        });
        let label = if has_answer { t("删除整条提问消息", "Delete the complete question message") } else { t("删除提问", "Delete questions") };
        let trash = action_button(("question-delete", id), "Trash2", label, move |app| {
            if let Some(on_delete) = &on_delete {
                on_delete(app);
            }
        });
        row().key(("question-actions", id)).items_center().gap(1.0).shrink0().reveal_unless(hovered.get()).child(pencil).child(trash)
    });
    let header = row()
        .key(("question-header", id))
        .items_center()
        .gap(7.0)
        .min_h(25.0)
        .pr(4.0)
        .pb(4.0)
        .child(icon("CircleHelp", 14.0).color(mark).shrink0())
        .child(text(t("提问", "Questions")).ty(TITLE).color(t_.text_secondary).nowrap())
        .child(text(status).ty(SMALL).color(if failed { t_.critical } else { t_.text_tertiary }).nowrap())
        // A spacer rather than `margin-right: auto`, which would cost the row its gap: see `turn_changes::counts`.
        .child(spacer())
        .child(actions);

    // `max-width: 86%`, the whole width once the window is too narrow to spare it.
    let body_width = if window_size().0 <= 720.0 { 1.0 } else { 0.86 };
    let body: E = if editing_in_place {
        let context = tool_context(id, item.tool_name, item.input.clone(), None, item.result.clone(), item.created_at);
        let (on_close, on_save) = (on_cancel_edit.expect("checked above"), on_save_question.expect("checked above"));
        div().key(("question-body", id)).min_w(0.0).max_w_fraction(body_width).child(inline_question_editor(("question-editor", id), QuestionEditorProps { item: context, answer: answer.cloned(), on_close, on_save }))
    } else {
        let sections = questions.iter().enumerate().map(|(position, question)| {
            let parsed = parsed_answers.as_ref().map(|answers| output(t("你的回答", "Your answer"), answers.get(position).cloned().unwrap_or_default()));
            let blocking = blocking_answers.as_ref().and_then(|answers| answers.get(position));
            let answered = blocking.and_then(|view| view.answer.clone()).filter(|answer| !answer.is_empty()).map(|answer| output(t("你的回答", "Your answer"), answer));
            let notes = blocking.and_then(|view| view.notes.clone()).filter(|notes| !notes.is_empty()).map(|notes| output(t("备注", "Notes"), notes));
            col()
                .key(("question-section", id, position))
                .gap(5.0)
                .min_w(0.0)
                .child(col().gap(1.0).min_w(0.0).child(text(question.header.clone()).ty(SMALL_STRONG).color(t_.text_tertiary)).child(text(question.question.clone()).ty(PROMPT).color(t_.text_primary).selectable()))
                .child(parsed)
                .child(answered)
                .child(notes)
        });
        let failure = failed.then(|| text(item.result.output.clone()).key(("question-failure", id)).ty(OUTPUT).color(t_.critical).selectable());
        let raw = answer_content.filter(|_| parsed_answers.is_none()).map(|content| output(t("你的回答", "Your answer"), content.to_owned()));
        col().key(("question-body", id)).gap(15.0).min_w(0.0).max_w_fraction(body_width).children(sections).child(failure).child(raw)
    };

    let mut card = col().key(("question-card", id)).pt(2.0).pb(4.0).min_w(0.0).on_hover_change(move |_: &mut App, on| hovered.set(on));
    if selected {
        // `[data-timeline-selected]`.
        card = card.bg(t_.selection).border(t_.accent.fade(0.35)).radius(6.0);
    }
    if !read_only {
        card = card.on_context_menu(move |app: &mut App, event: PointerEvent| {
            if let Some(open) = &on_open_insert {
                let at = if event.y > event.rect.y + event.rect.h / 2.0 { after_index } else { index };
                open(app, event, at);
            }
        });
    }
    card.child(header).child(body)
}

trait MaxWidthFraction {
    fn max_w_fraction(self, fraction: f32) -> Self;
}

impl MaxWidthFraction for E {
    fn max_w_fraction(self, fraction: f32) -> Self {
        self.with_style(|s| s.max_size.width = taffy::style::Dimension::percent(fraction))
    }
}
