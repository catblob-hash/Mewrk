//! The card an `ask_user` call blocks on, modelled on Claude Code's AskUserQuestion dialog: upstream's `QuestionDock.tsx`. A tab
//! per question plus a Submit tab, options with an "Other" row, "Chat about this", a preview layout with notes for single-select
//! questions whose options carry previews, and a review screen. Esc, the close button and the review screen's Cancel close the
//! card.
//!
//! The composer stays live while the card is up: `remember_question_draft` keeps the page told what the card would hand back if a
//! composer message were sent now, the answers filled in so far or a close when there are none.

use crate::approval_dock::{arriving, pager, DockStack};
use mewrk_client::prelude::*;
use mewrk_client::runs::{PendingToolPrompt, PromptKind, QuestionAction, QuestionResponse};
use mewrk_ui_common::markdown::{markdown_content, MarkdownOptions};
use mewrk_ui_common::popover_menu::probe::say;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Claude Code shows no preview longer than this; the model is told to compare labels.
const MAX_PREVIEW_CHARS: usize = 2000;
/// `.question-dock__preview-box`: a preview is a sketch of the option, set in the code face.
const PREVIEW: TypeStyle = TypeStyle { size: 10.5, line: 16.275, weight: Weight::Regular, mono: true, italic: false, serif: false };
/// `.question-dock__notes-text`.
const NOTES_TEXT: TypeStyle = TypeStyle { italic: true, ..FINE };
/// `.question-dock__chip > span:last-child`'s `max-width`, in the `ch` of `FINE_STRONG`.
const CHIP_CH: f32 = 6.6;

/// The single-select slot for the typed "Other" answer; no option label can equal it.
pub const OTHER: &str = "\u{0}other";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuestionItem {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
}

fn options_from_value(value: Option<&Value>) -> Vec<QuestionOption> {
    let Some(options) = value.and_then(Value::as_array) else { return Vec::new() };
    options
        .iter()
        .filter_map(|option| {
            if let Some(label) = option.as_str().map(str::trim).filter(|label| !label.is_empty()) {
                return Some(QuestionOption { label: label.to_owned(), description: String::new(), preview: None });
            }
            let label = option.get("label")?.as_str()?.trim();
            if label.is_empty() {
                return None;
            }
            Some(QuestionOption {
                label: label.to_owned(),
                description: option.get("description").and_then(Value::as_str).map_or_else(String::new, |text| text.trim().to_owned()),
                preview: option.get("preview").and_then(Value::as_str).filter(|text| !text.trim().is_empty()).map(str::to_owned),
            })
        })
        .collect()
}

/// `questionsFromInput` of `lib/orchestration.ts`, given the call's input.
pub fn questions_from_input(input: &Value) -> Vec<QuestionItem> {
    if let Some(list) = input.get("questions").and_then(Value::as_array) {
        let questions: Vec<QuestionItem> = list
            .iter()
            .filter_map(|item| {
                let question = item.get("question")?.as_str()?.trim();
                if question.is_empty() {
                    return None;
                }
                let header = item.get("header").and_then(Value::as_str).map(str::trim).filter(|header| !header.is_empty()).unwrap_or("Question");
                Some(QuestionItem { question: question.to_owned(), header: header.to_owned(), options: options_from_value(item.get("options")), multi_select: item.get("multiSelect").and_then(Value::as_bool) == Some(true) })
            })
            .collect();
        if !questions.is_empty() {
            return questions;
        }
    }
    // Keep old persisted calls readable after the multi-question protocol ships.
    let question = input.get("question").and_then(Value::as_str).map_or("", str::trim);
    if question.is_empty() {
        return Vec::new();
    }
    vec![QuestionItem { question: question.to_owned(), header: "Question".to_owned(), options: options_from_value(input.get("options")), multi_select: false }]
}

/// `joinMultiSelectAnswer`.
pub fn join_multi_select_answer(items: &[String]) -> String {
    items.iter().map(|item| if item.contains(", ") || item.contains('"') { serde_json::to_string(item).unwrap_or_else(|_| item.clone()) } else { item.clone() }).collect::<Vec<_>>().join(", ")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuestionDraft {
    /// Single-select: the picked option label, or [`OTHER`] for the typed answer.
    pub selected: Option<String>,
    /// Multi-select: the checked option labels.
    pub checked: Vec<String>,
    pub other: String,
    pub notes: String,
}

/// Single-select questions with any option preview use the side-by-side layout.
pub fn uses_preview_layout(question: &QuestionItem) -> bool {
    !question.multi_select && question.options.iter().any(|option| option.preview.is_some())
}

pub fn answer_of(question: &QuestionItem, draft: &QuestionDraft) -> Option<String> {
    if question.multi_select {
        let mut items: Vec<String> = question.options.iter().map(|option| option.label.clone()).filter(|label| draft.checked.contains(label)).collect();
        if !draft.other.trim().is_empty() {
            items.push(draft.other.clone());
        }
        return (!items.is_empty()).then(|| join_multi_select_answer(&items));
    }
    if draft.selected.as_deref() == Some(OTHER) {
        return (!draft.other.trim().is_empty()).then(|| draft.other.clone());
    }
    draft.selected.clone()
}

pub fn response_from(action: QuestionAction, questions: &[QuestionItem], drafts: &[QuestionDraft]) -> QuestionResponse {
    QuestionResponse {
        action,
        answers: questions.iter().zip(drafts).map(|(question, draft)| answer_of(question, draft)).collect(),
        previews: Some(
            questions
                .iter()
                .zip(drafts)
                .map(|(question, draft)| {
                    if !uses_preview_layout(question) {
                        return None;
                    }
                    question.options.iter().find(|option| Some(option.label.as_str()) == draft.selected.as_deref()).and_then(|option| option.preview.clone())
                })
                .collect(),
        ),
        notes: Some(questions.iter().zip(drafts).map(|(question, draft)| (uses_preview_layout(question) && !draft.notes.trim().is_empty()).then(|| draft.notes.trim().to_owned())).collect()),
    }
}

/// What a composer message sent now would hand back: the answers so far, or a close.
pub fn composer_response(questions: &[QuestionItem], drafts: &[QuestionDraft]) -> QuestionResponse {
    let submitted = response_from(QuestionAction::Submit, questions, drafts);
    let anything = submitted.answers.iter().any(Option::is_some) || submitted.notes.as_ref().is_some_and(|notes| notes.iter().any(Option::is_some));
    if anything { submitted } else { QuestionResponse::close() }
}

/// A key the card hears, however it reached the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockKey {
    Escape,
    Tab { shift: bool },
    Left,
    Right,
    Down,
    Up,
    /// A character typed with no modifier: a digit, `n`.
    Char(char),
    CtrlN,
    CtrlP,
}

/// What a step of the card asks of the world around it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DockEffect {
    #[default]
    None,
    /// Hand this back to the model. Locks the card until the host answers.
    Respond(QuestionResponse),
    /// Move the keyboard into the row's control, or back onto the card.
    FocusRow(usize),
    FocusOther,
    FocusNotes,
    FocusCard,
}

/// The card's state, apart from drawing it.
#[derive(Clone, Debug)]
pub struct QuestionDockState {
    pub questions: Vec<QuestionItem>,
    pub drafts: Vec<QuestionDraft>,
    pub index: usize,
    pub focus: usize,
    pub notes_open: bool,
    pub notes_focus_moved: bool,
    pub responded: bool,
    pub disabled: bool,
    /// The drafts changed since the page was last told.
    pub draft_dirty: bool,
    /// The row whose control held the keyboard when last looked: `onFocus` is the change of it.
    held_row: Option<usize>,
}

impl QuestionDockState {
    pub fn new(input: &Value) -> Self {
        let questions = questions_from_input(input);
        let drafts = vec![QuestionDraft::default(); questions.len()];
        Self { questions, drafts, index: 0, focus: 0, notes_open: false, notes_focus_moved: false, responded: false, disabled: false, draft_dirty: true, held_row: None }
    }

    pub fn locked(&self) -> bool {
        self.disabled || self.responded
    }

    pub fn hide_submit_tab(&self) -> bool {
        self.questions.len() == 1 && !self.questions[0].multi_select
    }

    pub fn max_index(&self) -> usize {
        if self.hide_submit_tab() { self.questions.len() - 1 } else { self.questions.len() }
    }

    pub fn on_review(&self) -> bool {
        self.index == self.questions.len()
    }

    pub fn question(&self) -> Option<&QuestionItem> {
        self.questions.get(self.index)
    }

    pub fn draft(&self) -> Option<&QuestionDraft> {
        self.drafts.get(self.index)
    }

    pub fn preview_layout(&self) -> bool {
        self.question().is_some_and(uses_preview_layout)
    }

    pub fn answers(&self) -> Vec<Option<String>> {
        self.questions.iter().zip(&self.drafts).map(|(question, draft)| answer_of(question, draft)).collect()
    }

    fn option_count(&self) -> usize {
        self.question().map_or(0, |question| question.options.len())
    }

    /// Row layout: options, then (standard layout only) the Other row, then the chat row.
    pub fn other_row(&self) -> Option<usize> {
        (!self.preview_layout()).then(|| self.option_count())
    }

    pub fn chat_row(&self) -> usize {
        if self.preview_layout() { self.option_count() } else { self.option_count() + 1 }
    }

    pub fn next_button_row(&self) -> Option<usize> {
        self.question().is_some_and(|question| question.multi_select).then(|| self.chat_row() + 1)
    }

    fn set_index(&mut self, index: usize) {
        if index != self.index {
            self.index = index;
            self.focus = 0;
            self.notes_open = false;
        }
    }

    pub fn go_to(&mut self, index: usize) {
        self.set_index(index.min(self.max_index()));
    }

    pub fn step_index(&mut self, delta: isize) {
        let next = (self.index as isize + delta).clamp(0, self.max_index() as isize) as usize;
        self.set_index(next);
    }

    fn respond(&mut self, response: QuestionResponse) -> DockEffect {
        if self.locked() {
            return DockEffect::None;
        }
        self.responded = true;
        DockEffect::Respond(response)
    }

    pub fn close(&mut self) -> DockEffect {
        self.respond(QuestionResponse::close())
    }

    pub fn submit(&mut self) -> DockEffect {
        let response = response_from(QuestionAction::Submit, &self.questions, &self.drafts);
        self.respond(response)
    }

    pub fn chat(&mut self) -> DockEffect {
        let response = response_from(QuestionAction::Chat, &self.questions, &self.drafts);
        self.respond(response)
    }

    fn update_draft(&mut self, change: impl FnOnce(&mut QuestionDraft)) {
        if let Some(draft) = self.drafts.get_mut(self.index) {
            change(draft);
            self.draft_dirty = true;
        }
    }

    /// Picking a single-select option answers it and moves on, or for a lone single-select question submits the card outright.
    pub fn pick(&mut self, label: &str) -> DockEffect {
        if self.locked() || self.question().is_none() {
            return DockEffect::None;
        }
        self.update_draft(|draft| draft.selected = Some(label.to_owned()));
        if self.hide_submit_tab() {
            return self.submit();
        }
        self.step_index(1);
        DockEffect::None
    }

    pub fn toggle(&mut self, label: &str) {
        if self.locked() {
            return;
        }
        self.update_draft(|draft| {
            if let Some(at) = draft.checked.iter().position(|item| item == label) {
                draft.checked.remove(at);
            } else {
                draft.checked.push(label.to_owned());
            }
        });
    }

    /// Checks the single-select Other circle without answering or moving on: the answer is whatever is typed in its box.
    pub fn choose_other(&mut self) {
        let single = self.question().is_some_and(|question| !question.multi_select);
        if self.locked() || !single || self.draft().is_some_and(|draft| draft.selected.as_deref() == Some(OTHER)) {
            return;
        }
        self.update_draft(|draft| draft.selected = Some(OTHER.to_owned()));
    }

    pub fn set_other(&mut self, value: String) {
        let multi = self.question().is_none_or(|question| question.multi_select);
        self.update_draft(|draft| {
            draft.other = value;
            if !multi {
                draft.selected = Some(OTHER.to_owned());
            }
        });
    }

    pub fn set_notes(&mut self, value: String) {
        self.update_draft(|draft| draft.notes = value);
    }

    pub fn open_notes(&mut self) -> DockEffect {
        self.notes_open = true;
        self.notes_focus_moved = false;
        DockEffect::FocusNotes
    }

    fn focus_row(&mut self, row: usize) -> DockEffect {
        self.focus = row;
        if self.preview_layout() && self.notes_open {
            self.notes_focus_moved = true;
        }
        if Some(row) == self.other_row() { DockEffect::FocusOther } else { DockEffect::FocusRow(row) }
    }

    pub fn activate_row(&mut self, row: usize) -> DockEffect {
        let Some(question) = self.question().cloned() else { return DockEffect::None };
        if self.locked() {
            return DockEffect::None;
        }
        if row == self.chat_row() {
            return self.chat();
        }
        if Some(row) == self.other_row() {
            return DockEffect::FocusOther;
        }
        if Some(row) == self.next_button_row() {
            self.step_index(1);
            return DockEffect::None;
        }
        let Some(option) = question.options.get(row) else { return DockEffect::None };
        if question.multi_select {
            self.toggle(&option.label);
            DockEffect::None
        } else {
            self.pick(&option.label)
        }
    }

    /// A key that reached the card, with the focus on the card or one of its rows rather than in one of its text boxes.
    pub fn key(&mut self, key: DockKey) -> DockEffect {
        if self.locked() {
            return DockEffect::None;
        }
        match key {
            DockKey::Escape => return self.escape(),
            DockKey::Tab { .. } | DockKey::Left | DockKey::Right => {
                let back = matches!(key, DockKey::Left | DockKey::Tab { shift: true });
                self.step_index(if back { -1 } else { 1 });
                return DockEffect::None;
            }
            _ => {}
        }
        if self.question().is_none() {
            return DockEffect::None;
        }
        let last_row = self.next_button_row().unwrap_or(0).max(self.chat_row());
        match key {
            DockKey::Down | DockKey::CtrlN => self.focus_row(last_row.min(self.focus + 1)),
            DockKey::Up | DockKey::CtrlP => self.focus_row(self.focus.saturating_sub(1)),
            DockKey::Char('n') if self.preview_layout() => self.open_notes(),
            DockKey::Char(digit @ '1'..='9') => {
                let row = digit as usize - '1' as usize;
                let question = self.question().cloned();
                // In the preview layout a digit only moves focus.
                if self.preview_layout() {
                    return if row < self.option_count() { self.focus_row(row) } else { DockEffect::None };
                }
                if Some(row) == self.other_row() {
                    let typed = self.draft().is_some_and(|draft| !draft.other.trim().is_empty());
                    return if question.is_some_and(|question| question.multi_select) || !typed { self.focus_row(row) } else { self.pick(OTHER) };
                }
                if row < self.option_count() || row == self.chat_row() { self.activate_row(row) } else { DockEffect::None }
            }
            _ => DockEffect::None,
        }
    }

    /// Esc: closes the notes box if it is open, or the card.
    pub fn escape(&mut self) -> DockEffect {
        if self.notes_open {
            self.notes_open = false;
            return DockEffect::FocusCard;
        }
        self.close()
    }

    /// Enter in the Other box.
    pub fn other_enter(&mut self, primary: bool) -> DockEffect {
        let Some(question) = self.question().cloned() else { return DockEffect::None };
        let blank = self.draft().is_none_or(|draft| draft.other.trim().is_empty());
        if question.multi_select {
            if primary {
                self.step_index(1);
            }
            return DockEffect::None;
        }
        // Claude Code: Enter on a blank "Other" dismisses the dialog.
        if blank {
            return self.close();
        }
        self.pick(OTHER)
    }

    /// Enter in the notes box.
    pub fn notes_enter(&mut self) -> DockEffect {
        let Some(question) = self.question().cloned() else { return DockEffect::None };
        let Some(draft) = self.draft().cloned() else { return DockEffect::None };
        self.notes_open = false;
        let has_notes = !draft.notes.trim().is_empty();
        let focused = question.options.get(self.focus);
        let effect = if has_notes && self.notes_focus_moved && focused.is_some() {
            let label = focused.map(|option| option.label.clone()).unwrap_or_default();
            self.pick(&label)
        } else if let Some(selected) = draft.selected {
            self.pick(&selected)
        } else {
            if has_notes {
                // Notes with nothing selected still answer the question ("(notes only)").
                self.step_index(1);
            }
            DockEffect::None
        };
        if effect == DockEffect::None { DockEffect::FocusCard } else { effect }
    }

    pub fn hints(&self) -> String {
        let hints: Vec<&str> = if self.preview_layout() {
            let mut hints = vec![t("Enter 选择", "Enter to select"), t("↑/↓ 移动", "↑/↓ to navigate"), t("n 添加备注", "n to add notes")];
            if self.questions.len() > 1 {
                hints.push(t("Tab 切换问题", "Tab to switch questions"));
            }
            hints.push(t("Esc 取消", "Esc to cancel"));
            hints
        } else {
            vec![t("Enter 选择", "Enter to select"), if self.questions.len() == 1 { t("↑/↓ 移动", "↑/↓ to navigate") } else { t("Tab/方向键切换", "Tab/Arrow keys to navigate") }, t("Esc 取消", "Esc to cancel")]
        };
        hints.join(" · ")
    }
}

// ---------------------------------------------------------------- the view

/// One card's state lives per prompt, so a card comes back as it was left when the stack is flipped through.
#[derive(Default)]
struct Docks {
    states: HashMap<String, QuestionDockState>,
    /// The card up now and when it came. One flipped to has just arrived, as a keyed remount has upstream: it takes the focus, and
    /// it slides in.
    shown: Option<(String, Instant)>,
}

pub fn dock_key(prompt_id: &str) -> u64 {
    hash_key(("question-dock", prompt_id))
}

pub fn row_key(prompt_id: &str, index: usize, row: usize) -> u64 {
    hash_key(("question-row", prompt_id, index, row))
}

pub fn chat_key(prompt_id: &str, index: usize) -> u64 {
    hash_key(("question-chat", prompt_id, index))
}

pub fn advance_key(prompt_id: &str, index: usize) -> u64 {
    hash_key(("question-advance", prompt_id, index))
}

pub fn tab_key(prompt_id: &str, tab: usize) -> u64 {
    hash_key(("question-tab", prompt_id, tab))
}

pub fn close_key(prompt_id: &str) -> u64 {
    hash_key(("question-close", prompt_id))
}

pub fn submit_key(prompt_id: &str) -> u64 {
    hash_key(("question-submit", prompt_id))
}

pub fn cancel_key(prompt_id: &str) -> u64 {
    hash_key(("question-cancel", prompt_id))
}

pub fn other_choice_key(prompt_id: &str, index: usize) -> u64 {
    hash_key(("question-other-circle", prompt_id, index))
}

/// The key the Other box's field goes by, for asking for the focus and for finding it.
pub fn other_field_key(prompt_id: &str, index: usize) -> u64 {
    hash_key(("question-other", prompt_id, index))
}

pub fn notes_field_key(prompt_id: &str, index: usize) -> u64 {
    hash_key(("question-notes", prompt_id, index))
}

/// The id a `field` keyed so holds the focus under.
fn field_focus_id(key: u64) -> u64 {
    hash_key(("field", key))
}

/// A card gone from every queue has no one to come back for.
fn forget_answered(app: &mut App) {
    if app.local::<Docks>().states.is_empty() {
        return;
    }
    let live: HashSet<String> = app.runs().tool_prompts.values().flatten().map(|prompt| prompt.prompt_id.clone()).collect();
    app.local::<Docks>().states.retain(|id, _| live.contains(id));
}

/// The state of the card for a prompt, for a test to look at.
pub fn dock_state(app: &mut App, prompt_id: &str) -> Option<QuestionDockState> {
    app.local::<Docks>().states.get(prompt_id).cloned()
}

fn apply_effect(app: &mut App, conversation_id: &str, prompt_id: &str, effect: DockEffect) {
    match effect {
        DockEffect::None => {}
        DockEffect::Respond(response) => {
            let prompt = prompt_id.to_owned();
            app.answer_question_prompt(conversation_id, prompt_id, response, move |app, taken| {
                // `false` means the host did not take the answer; the card unlocks for a retry.
                if !taken
                    && let Some(state) = app.local::<Docks>().states.get_mut(&prompt)
                {
                    state.responded = false;
                    repaint();
                }
            });
        }
        DockEffect::FocusRow(row) => {
            // The options' rows, or the two controls after them that go by keys of their own.
            let state = app.local::<Docks>().states.get(prompt_id);
            let index = state.map_or(0, |state| state.index);
            if state.is_some_and(|state| state.chat_row() == row) {
                focus_request(chat_key(prompt_id, index));
            } else if state.is_some_and(|state| state.next_button_row() == Some(row)) {
                focus_request(advance_key(prompt_id, index));
            } else {
                focus_request(row_key(prompt_id, index, row));
            }
        }
        DockEffect::FocusOther => {
            let index = app.local::<Docks>().states.get(prompt_id).map_or(0, |state| state.index);
            focus_request(("field", other_field_key(prompt_id, index)));
        }
        DockEffect::FocusNotes => {
            let index = app.local::<Docks>().states.get(prompt_id).map_or(0, |state| state.index);
            focus_request(("field", notes_field_key(prompt_id, index)));
        }
        DockEffect::FocusCard => focus_request(dock_key(prompt_id)),
    }
    repaint();
}

/// Runs `step` on the card and acts on what it asked for.
fn with_card(app: &mut App, conversation_id: &str, prompt_id: &str, step: impl FnOnce(&mut QuestionDockState) -> DockEffect) {
    let Some(state) = app.local::<Docks>().states.get_mut(prompt_id) else { return };
    let was_on = state.index;
    let effect = step(state);
    // What held the keyboard went with the page it was on, and the card is what hears the keys of the next one.
    let turned = state.index != was_on && effect == DockEffect::None;
    apply_effect(app, conversation_id, prompt_id, if turned { DockEffect::FocusCard } else { effect });
}

/// `[✔]` and `[ ]`: the row's state says this already.
fn check_mark(checked: bool) -> E {
    let t_ = theme();
    text(if checked { "[✔]" } else { "[ ]" }).ty(MONO_SMALL).color(if checked { t_.success } else { t_.text_secondary }).shrink0().aria_hidden(true)
}

fn title_key(prompt_id: &str) -> u64 {
    hash_key(("question-dock-title", prompt_id))
}

fn radio_mark(checked: bool) -> E {
    let t_ = theme();
    row().center().size(16.0, 18.0).shrink0().child(icon(if checked { "CircleCheck" } else { "Circle" }, 14.0).color(if checked { t_.success } else { t_.text_tertiary }))
}

fn pointer_mark(on: bool) -> E {
    let t_ = theme();
    div().w(10.0).shrink0().when(on, |e| e.child(text("❯").ty(FINE).color(t_.accent_text).aria_hidden(true)))
}

/// The card, or nothing without a question card.
pub fn question_dock(app: &mut App, conversation_id: &str, prompt: Option<&PendingToolPrompt>, stack: Option<DockStack>, disabled: bool) -> Option<E> {
    forget_answered(app);
    let Some(prompt) = prompt.filter(|prompt| prompt.kind == PromptKind::Question) else {
        app.local::<Docks>().shown = None;
        return None;
    };
    let prompt_id = prompt.prompt_id.clone();
    let input = json!({ "questions": prompt.questions.clone().unwrap_or_else(|| json!([])) });
    let single_dollar_math = app.doc.global_settings.appearance.single_dollar_math;
    let docks = app.local::<Docks>();
    let arrived = match &docks.shown {
        Some((shown, since)) if *shown == prompt_id => *since,
        _ => {
            focus_request(dock_key(&prompt_id));
            let now = Instant::now();
            docks.shown = Some((prompt_id.clone(), now));
            now
        }
    };
    let state = docks.states.entry(prompt_id.clone()).or_insert_with(|| QuestionDockState::new(&input));
    state.disabled = disabled;
    let snapshot = state.clone();
    if snapshot.draft_dirty {
        state.draft_dirty = false;
        app.remember_question_draft(&prompt_id, composer_response(&snapshot.questions, &snapshot.drafts));
    }
    Some(arriving(dock_view(conversation_id, &prompt_id, &snapshot, stack, single_dollar_math), arrived))
}

fn dock_view(conversation_id: &str, prompt_id: &str, state: &QuestionDockState, stack: Option<DockStack>, single_dollar_math: bool) -> E {
    let t_ = theme();
    let locked = state.locked();
    let answers = state.answers();
    let (conversation, prompt) = (Rc::new(conversation_id.to_owned()), Rc::new(prompt_id.to_owned()));
    let act = {
        let (conversation, prompt) = (conversation.clone(), prompt.clone());
        move |step: Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>| -> Handlerish {
            let (conversation, prompt) = (conversation.clone(), prompt.clone());
            Box::new(move |app: &mut App| {
                let step = step.clone();
                with_card(app, &conversation, &prompt, move |state| step(state));
            })
        }
    };

    // ---- tabs
    let mut chips: E = row().role(Role::TabList).aria_label(t("问题", "Questions")).min_w(0.0).items_center().gap(2.0).clip();
    let max_index = state.max_index();
    let arrow = |left: bool, label: &'static str, enabled: bool| -> E {
        say(label);
        let delta: isize = if left { -1 } else { 1 };
        let button: E = row().key(hash_key(("question-arrow", prompt_id, left))).min_h(24.0).px(7.0).items_center().radius(4.0).aria_label(label).tooltip(label).child(text(if left { "←" } else { "→" }).ty(FINE_STRONG).color(if enabled { t_.text_secondary } else { t_.text_disabled }));
        if enabled { button.hover_bg(t_.subtle_hover).on_click(act(Rc::new(move |state: &mut QuestionDockState| { state.step_index(delta); DockEffect::None }))) } else { button.disabled(true).on_click(|_: &mut App| {}) }
    };
    if max_index > 0 {
        chips = chips.child(arrow(true, t("切换到上一个问题", "Previous question"), state.index > 0));
    }
    for (at, item) in state.questions.iter().enumerate() {
        let current = at == state.index;
        let header = if item.header.is_empty() { format!("Q{}", at + 1) } else { item.header.clone() };
        let name = if answers[at].is_some() { tf("{header}（已回答）", "{header} (answered)", &[("header", &header)]) } else { header.clone() };
        say(&name);
        let color = if current { t_.accent_text } else { t_.text_secondary };
        chips = chips.child(
            row()
                .key(tab_key(prompt_id, at))
                .role(Role::Tab)
                .aria_selected(current)
                .aria_label(name)
                .min_h(24.0)
                .px(7.0)
                .items_center()
                .gap(5.0)
                .radius(4.0)
                .shrink0()
                .when(current, |e| e.bg(t_.accent.fade(0.12)))
                .when(!current, |e| e.hover_bg(t_.subtle_hover))
                .on_click(act(Rc::new(move |state: &mut QuestionDockState| { state.go_to(at); DockEffect::None })))
                .child(text(if answers[at].is_some() { "☒" } else { "☐" }).ty(FINE_STRONG).color(color))
                .child(text(header).ty(FINE_STRONG).color(color).ellipsis().max_w(CHIP_CH * if current { 24.0 } else { 14.0 })),
        );
    }
    if !state.hide_submit_tab() {
        let on_review = state.on_review();
        let questions = state.questions.len();
        let color = if on_review { t_.accent_text } else { t_.text_secondary };
        let label = t("提交", "Submit");
        say(label);
        chips = chips.child(row().key(tab_key(prompt_id, questions)).role(Role::Tab).aria_selected(on_review).min_h(24.0).px(7.0).items_center().gap(5.0).radius(4.0).shrink0().when(on_review, |e| e.bg(t_.accent.fade(0.12))).when(!on_review, |e| e.hover_bg(t_.subtle_hover)).on_click(act(Rc::new(move |state: &mut QuestionDockState| { state.go_to(questions); DockEffect::None }))).child(text("✓").ty(FINE_STRONG).color(color).aria_hidden(true)).child(text(label).ty(FINE_STRONG).color(color)));
    }
    if max_index > 0 {
        chips = chips.child(arrow(false, t("切换到下一个问题", "Next question"), state.index < max_index));
    }
    let close_label = t("关闭提问", "Close questions");
    say(close_label);
    let close_button: E = row().key(close_key(prompt_id)).center().size(26.0, 26.0).radius(CONTROL_RADIUS).shrink0().ml_auto().tooltip(close_label).child(icon("X", 14.0).color(if locked { t_.text_disabled } else { t_.text_secondary }));
    let close_button = if locked { close_button.disabled(true).on_click(|_: &mut App| {}) } else { close_button.hover_bg(t_.subtle_hover).on_click(act(Rc::new(|state: &mut QuestionDockState| state.close()))) };
    let tabs: E = row().min_h(30.0).items_center().gap(8.0).child(pager(conversation_id, stack, t("上一张卡片", "Previous card"), t("下一张卡片", "Next card"))).child(chips).child(close_button);

    // ---- the page
    let page = match (state.question(), state.draft()) {
        (Some(question), Some(draft)) => question_page(state, question, draft, prompt_id, single_dollar_math, &act),
        _ => review_page(state, &answers, prompt_id, &act),
    };
    let hints = state.hints();
    say(&hints);

    // Plain characters reach the window as text, not as keys, and only a leaf can read them. Enter and Space are not among them: they
    // press whatever holds the focus, a row or a button, and on the card itself they do nothing, so a card that has only just taken
    // the keyboard from someone typing is not answered by the Enter they were about to press.
    let (watch_conversation, watch_prompt) = (conversation.clone(), prompt.clone());
    let index = state.index;
    let rows: Vec<u64> = (0..state.question().map_or(0, |question| question.options.len())).map(|row| row_key(prompt_id, index, row)).collect();
    let (other_row, chat_row, next_row) = (state.other_row(), state.chat_row(), state.next_button_row());
    let card_id = hash_key(dock_key(prompt_id));
    let chat_id = hash_key(chat_key(prompt_id, index));
    let advance_id = hash_key(advance_key(prompt_id, index));
    let other_id = field_focus_id(other_field_key(prompt_id, index));
    let watcher = custom(move |app: &mut App, ui: &mut Ui<'_>, _: Rect| {
        let focus = ui.mem.focus;
        let held = if focus == other_id {
            other_row
        } else if focus == chat_id {
            Some(chat_row)
        } else if focus == advance_id {
            next_row
        } else {
            rows.iter().position(|row| hash_key(*row) == focus)
        };
        let on_card = focus == card_id || (held.is_some() && focus != other_id);
        if on_card && !ui.input.text.is_empty() {
            for character in ui.input.text.chars().filter(|c| !c.is_control() && *c != ' ') {
                with_card(app, &watch_conversation, &watch_prompt, |state| state.key(DockKey::Char(character)));
            }
        }
        // `onFocus`: the mark follows what holds the keyboard, and clicking into the Other box checks its circle. Picking an option clears it.
        let was_held = app.local::<Docks>().states.get(watch_prompt.as_str()).and_then(|state| state.held_row);
        if held != was_held {
            with_card(app, &watch_conversation, &watch_prompt, |state| {
                state.held_row = held;
                if let Some(row) = held {
                    state.focus = row;
                    if Some(row) == state.other_row() {
                        state.choose_other();
                    }
                }
                DockEffect::None
            });
        }
    })
    .w(0.0)
    .h(0.0)
    .pass_through();

    let (key_conversation, key_prompt) = (conversation.clone(), prompt.clone());
    col()
        .key(dock_key(prompt_id))
        .role(Role::Dialog)
        .aria_labelledby(title_key(prompt_id))
        .aria_disabled(locked)
        .focusable()
        .skip_tab(true)
        .w_full()
        .max_w(820.0)
        .mx_auto()
        .mt(10.0)
        .mb(2.0)
        .pt(6.0)
        .shrink0()
        .border_top(t_.divider)
        .on_key(move |app: &mut App, press: &KeyPress| {
            let key = match press.key {
                Key::Escape => DockKey::Escape,
                Key::Tab => DockKey::Tab { shift: press.shift },
                Key::Left => DockKey::Left,
                Key::Right => DockKey::Right,
                Key::Down => DockKey::Down,
                Key::Up => DockKey::Up,
                Key::Char('n') if press.ctrl => DockKey::CtrlN,
                Key::Char('p') if press.ctrl => DockKey::CtrlP,
                _ => return false,
            };
            with_card(app, &key_conversation, &key_prompt, |state| state.key(key));
            true
        })
        .child(tabs)
        .child(page)
        .child(div().min_h(22.0).pt(4.0).child(text(hints).ty(FINE).color(t_.text_tertiary)))
        .child(watcher)
        .when(locked, |card| card.opacity(0.6))
}

fn question_page(state: &QuestionDockState, question: &QuestionItem, draft: &QuestionDraft, prompt_id: &str, single_dollar_math: bool, act: &impl Fn(Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>) -> Handlerish) -> E {
    let t_ = theme();
    let locked = state.locked();
    let index = state.index;
    say(&question.question);
    let mut page: E = col().pt(10.0).pb(6.0).child(text(question.question.clone()).key(title_key(prompt_id)).role(Role::Heading).aria_level(2).ty(BODY_SMALL_STRONG).color(t_.text_primary).mb(8.0).selectable());
    if state.preview_layout() {
        let mut list: E = col().role(Role::ListBox).aria_labelledby(title_key(prompt_id)).min_w(0.0).gap(1.0);
        for (row_index, option) in question.options.iter().enumerate() {
            let selected = draft.selected.as_deref() == Some(option.label.as_str());
            let focused = state.focus == row_index;
            say(&option.label);
            let label = option.label.clone();
            let mut line: E = row()
                .key(row_key(prompt_id, index, row_index))
                .role(Role::ListBoxOption)
                .aria_selected(selected)
                .focusable()
                .min_h(30.0)
                .px(4.0)
                .py(5.0)
                .gap(6.0)
                .items_start()
                .radius(4.0)
                .when(focused, |e| e.bg(t_.subtle_hover))
                .child(pointer_mark(focused))
                .child(text(option.label.clone()).ty(FINE_STRONG).color(if selected { t_.success } else if focused { t_.accent_text } else { t_.text_primary }).flex1().min_w(0.0))
                .when(selected, |e| e.child(text("✔").ty(FINE).color(t_.success).aria_hidden(true)));
            if !locked {
                line = line.hover_bg(t_.subtle_hover).on_click(act(Rc::new(move |state: &mut QuestionDockState| state.pick(&label)))).on_hover_change(hover_focus(act, row_index));
            } else {
                line = line.disabled(true).on_click(|_: &mut App| {});
            }
            list = list.child(line);
        }
        let focused_option = question.options.get(state.focus.min(question.options.len().saturating_sub(1)));
        let preview: E = match focused_option.and_then(|option| option.preview.as_ref()) {
            None => {
                let label = t("没有预览", "No preview available");
                say(label);
                text(label).ty(FINE).color(t_.text_tertiary)
            }
            Some(preview) if preview.chars().count() > MAX_PREVIEW_CHARS => {
                let label = t("（预览过长，无法完整显示——请对比选项名称和说明）", "(preview cannot be shown in full — compare the option labels and descriptions instead)");
                say(label);
                text(label).ty(FINE).color(t_.text_tertiary)
            }
            Some(preview) => {
                say(preview);
                markdown_content(("question-preview-text", prompt_id, index, state.focus), preview, MarkdownOptions { single_dollar_math, ty: PREVIEW, ..MarkdownOptions::default() })
            }
        };
        let preview_box: E = div().key(("question-preview", prompt_id, index)).min_w(0.0).max_h(260.0).px(10.0).py(8.0).radius(5.0).border(t_.control_strong_stroke).scroll_y().child(preview);
        // `@media (max-width: 720px)`: one column.
        page = page.child(if crate::narrow_window() { col().gap(12.0).child(list).child(preview_box) } else { row().items_start().gap(12.0).child(list.w_pct(30.0).min_w(150.0).shrink0()).child(preview_box.flex1()) });
        let label = t("备注：", "Notes:");
        say(label);
        let mut notes: E = row().mt(8.0).items_center().gap(8.0).child(text(label).ty(FINE_STRONG).color(t_.accent_text).nowrap());
        if state.notes_open {
            let change = act_with_text(act, |state, value| state.set_notes(value));
            let on_key = act_keys(act, true);
            let notes_placeholder = t("为这个方案添加备注…", "Add notes on this design…");
            let notes_field = field::<App>(notes_field_key(prompt_id, index), draft.notes.clone()).bare().ty(CAPTION).submit_on_enter(false).placeholder(notes_placeholder).on_change(change).on_key(on_key).build().aria_label(notes_placeholder);
            notes = notes.child(underlined(notes_key_id(prompt_id, index), if locked { notes_field.disabled(true) } else { notes_field }));
        } else {
            let shown = if draft.notes.trim().is_empty() { t("按 n 添加备注", "press n to add notes").to_owned() } else { draft.notes.trim().to_owned() };
            say(&shown);
            let button: E = text(shown).ty(NOTES_TEXT).color(t_.text_tertiary).key(("question-notes-open", prompt_id, index));
            notes = notes.child(if locked { button } else { button.on_click(act(Rc::new(|state: &mut QuestionDockState| state.open_notes()))) });
        }
        page = page.child(notes);
    } else {
        let mut list: E = col().role(if question.multi_select { Role::Group } else { Role::ListBox }).aria_labelledby(title_key(prompt_id)).min_w(0.0).gap(1.0);
        for (row_index, option) in question.options.iter().enumerate() {
            let selected = if question.multi_select { draft.checked.contains(&option.label) } else { draft.selected.as_deref() == Some(option.label.as_str()) };
            let focused = state.focus == row_index;
            say(&option.label);
            if selected {
                say("selected");
            }
            let mut copy: E = col().min_w(0.0).flex1().gap(1.0).child(text(option.label.clone()).ty(FINE_STRONG).color(if selected { t_.success } else if focused { t_.accent_text } else { t_.text_primary }));
            if !option.description.is_empty() {
                say(&option.description);
                copy = copy.child(text(option.description.clone()).ty(FINE).color(t_.text_secondary));
            }
            let mark: E = if question.multi_select { check_mark(selected) } else { radio_mark(selected) };
            let mut line: E = row().key(row_key(prompt_id, index, row_index)).when(question.multi_select, |e| e.role(Role::CheckBox).aria_checked(selected)).when(!question.multi_select, |e| e.role(Role::ListBoxOption).aria_selected(selected)).focusable().min_h(30.0).px(4.0).py(5.0).gap(6.0).items_start().radius(4.0).when(focused, |e| e.bg(t_.subtle_hover)).child(pointer_mark(focused)).child(mark).child(copy);
            if !locked {
                line = line.hover_bg(t_.subtle_hover).on_click(act(Rc::new(move |state: &mut QuestionDockState| state.activate_row(row_index))));
            } else {
                line = line.disabled(true).on_click(|_: &mut App| {});
            }
            list = list.child(line);
        }
        // Other is picked only while its circle is checked; clicking into the box checks it, and picking an option clears it.
        let other_row_index = question.options.len();
        let other_focused = state.focus == other_row_index;
        let other_selected = if question.multi_select { !draft.other.trim().is_empty() } else { draft.selected.as_deref() == Some(OTHER) };
        let choose_label = t("选择其他", "Choose Other");
        say(choose_label);
        let mark: E = if question.multi_select {
            check_mark(other_selected)
        } else {
            let circle: E = row().key(other_choice_key(prompt_id, index)).role(Role::RadioButton).aria_checked(other_selected).aria_disabled(locked).aria_label(choose_label).tooltip(choose_label).child(radio_mark(other_selected));
            if locked { circle } else { circle.on_click(act(Rc::new(|state: &mut QuestionDockState| { state.choose_other(); DockEffect::FocusOther }))) }
        };
        let placeholder = if question.multi_select { t("输入其他内容", "Type something") } else { t("输入其他内容。", "Type something.") };
        say(placeholder);
        let change = act_with_text(act, |state, value| state.set_other(value));
        let other_field: E = field::<App>(other_field_key(prompt_id, index), draft.other.clone()).bare().ty(CAPTION).submit_on_enter(false).placeholder(placeholder).on_change(change).on_key(act_keys(act, false)).build().aria_label(t("其他", "Other"));
        let other_field = if locked { other_field.disabled(true) } else { other_field };
        let other_field = underlined(field_focus_id(other_field_key(prompt_id, index)), other_field);
        list = list.child(row().min_h(30.0).px(4.0).py(5.0).gap(6.0).items_center().radius(4.0).when(other_focused, |e| e.bg(t_.subtle_hover)).when(!locked, |e| e.hover_bg(t_.subtle_hover)).child(pointer_mark(other_focused)).child(mark).child(other_field));
        if question.multi_select {
            let label = if index + 1 == state.questions.len() { t("提交", "Submit") } else { t("下一题", "Next") };
            say(label);
            let next_row = state.next_button_row().unwrap_or(0);
            let button: E = row().key(advance_key(prompt_id, index)).focusable().self_start().min_h(26.0).ml(20.0).mt(6.0).px(12.0).items_center().radius(5.0).border(t_.accent.fade(0.55)).when(state.focus == next_row, |e| e.bg(t_.subtle_hover)).child(text(label).ty(FINE_STRONG).color(t_.accent_text));
            list = list.child(if locked { button.disabled(true).on_click(|_: &mut App| {}) } else { button.hover_bg(t_.subtle_hover).on_click(act(Rc::new(|state: &mut QuestionDockState| { state.step_index(1); DockEffect::None }))) });
        }
        page = page.child(list);
    }
    let chat_row = state.chat_row();
    let chat_focused = state.focus == chat_row;
    let chat_label = t("先聊聊这个", "Chat about this");
    say(chat_label);
    let chat: E = row().key(chat_key(prompt_id, index)).focusable().min_h(30.0).px(4.0).py(5.0).gap(6.0).items_start().radius(4.0).when(chat_focused, |e| e.bg(t_.subtle_hover)).child(pointer_mark(chat_focused)).child(text(chat_label).ty(CAPTION).color(if chat_focused { t_.accent_text } else { t_.text_primary }));
    page.child(div().h(1.0).mt(6.0).mb(2.0).bg(t_.divider)).child(if locked { chat.disabled(true).on_click(|_: &mut App| {}) } else { chat.hover_bg(t_.subtle_hover).on_click(act(Rc::new(|state: &mut QuestionDockState| state.chat()))) })
}

type Handlerish = Box<dyn Fn(&mut App)>;

/// `.question-dock__row--other input` and `.question-dock__notes input`: a box that is only its underline.
fn underlined(focus_id: u64, field: E) -> E {
    let t_ = theme();
    col().flex1().min_w(0.0).py(2.0).border_bottom(if focused_id() == focus_id { t_.accent } else { t_.control_strong_stroke }).child(field.w_full().h(CAPTION.line))
}

fn notes_key_id(prompt_id: &str, index: usize) -> u64 {
    field_focus_id(notes_field_key(prompt_id, index))
}

fn hover_focus(act: &impl Fn(Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>) -> Handlerish, row: usize) -> impl Fn(&mut App, bool) + 'static {
    let on_enter = act(Rc::new(move |state: &mut QuestionDockState| {
        state.focus = row;
        DockEffect::None
    }));
    move |app: &mut App, entered: bool| {
        if entered {
            on_enter(app);
        }
    }
}

fn act_with_text(act: &impl Fn(Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>) -> Handlerish, change: impl Fn(&mut QuestionDockState, String) + 'static) -> impl Fn(&mut App, String) + 'static {
    let value: Rc<std::cell::RefCell<String>> = Rc::default();
    let reader = value.clone();
    let change = Rc::new(change);
    let apply = act(Rc::new(move |state: &mut QuestionDockState| {
        change(state, reader.borrow().clone());
        DockEffect::None
    }));
    move |app: &mut App, typed: String| {
        *value.borrow_mut() = typed;
        apply(app);
    }
}

/// What a text box on the card hears: Esc for the card, and Enter for the box. `notes` says which box.
fn act_keys(act: &impl Fn(Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>) -> Handlerish, notes: bool) -> impl Fn(&mut App, &KeyPress) -> bool + 'static {
    let on_escape = act(Rc::new(|state: &mut QuestionDockState| state.escape()));
    let primary = Rc::new(std::cell::Cell::new(false));
    let reader = primary.clone();
    let on_enter = act(Rc::new(move |state: &mut QuestionDockState| if notes { state.notes_enter() } else { state.other_enter(reader.get()) }));
    move |app: &mut App, press: &KeyPress| match press.key {
        Key::Escape => {
            on_escape(app);
            true
        }
        Key::Enter => {
            primary.set(press.cmd || press.ctrl);
            on_enter(app);
            true
        }
        _ => false,
    }
}

fn review_page(state: &QuestionDockState, answers: &[Option<String>], prompt_id: &str, act: &impl Fn(Rc<dyn Fn(&mut QuestionDockState) -> DockEffect>) -> Handlerish) -> E {
    let t_ = theme();
    let locked = state.locked();
    let title = t("检查你的回答", "Review your answers");
    say(title);
    let mut page: E = col().pt(10.0).pb(6.0).child(text(title).key(title_key(prompt_id)).role(Role::Heading).aria_level(2).ty(BODY_SMALL_STRONG).color(t_.text_primary).mb(8.0));
    if answers.iter().any(Option::is_none) {
        let warning = t("你还有问题没有回答", "You have not answered all questions");
        say(warning);
        page = page.child(text(warning).role(Role::Status).ty(FINE).color(t_.caution).mb(8.0));
    }
    let mut summary: E = col().role(Role::List).gap(6.0).mb(10.0);
    for (item, answer) in state.questions.iter().zip(answers) {
        if let Some(answer) = answer {
            say(answer);
            summary = summary.child(col().role(Role::ListItem).gap(2.0).child(text(item.question.clone()).ty(FINE).color(t_.text_primary)).child(text(format!("→ {answer}")).ty(FINE).color(t_.success).ml(14.0)));
        }
    }
    let ready = t("确定提交这些回答？", "Ready to submit your answers?");
    say(ready);
    let submit_label = t("提交回答", "Submit answers");
    let cancel_label = t("取消", "Cancel");
    say(submit_label);
    say(cancel_label);
    let submit: E = row().key(submit_key(prompt_id)).min_h(26.0).px(12.0).items_center().radius(5.0).border(t_.accent.fade(0.55)).child(text(submit_label).ty(FINE_STRONG).color(if locked { t_.text_disabled } else { t_.accent_text }));
    let cancel: E = row().key(cancel_key(prompt_id)).min_h(26.0).px(12.0).items_center().radius(5.0).border(t_.control_strong_stroke).child(text(cancel_label).ty(FINE_STRONG).color(if locked { t_.text_disabled } else { t_.text_primary }));
    let (submit, cancel) = if locked {
        (submit.disabled(true).on_click(|_: &mut App| {}), cancel.disabled(true).on_click(|_: &mut App| {}))
    } else {
        (submit.hover_bg(t_.subtle_hover).on_click(act(Rc::new(|state: &mut QuestionDockState| state.submit()))), cancel.hover_bg(t_.subtle_hover).on_click(act(Rc::new(|state: &mut QuestionDockState| state.close()))))
    };
    page.child(summary).child(text(ready).ty(FINE).color(t_.text_tertiary).mb(6.0)).child(row().gap(4.0).child(submit).child(cancel))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(questions: Value) -> QuestionDockState {
        QuestionDockState::new(&json!({ "questions": questions }))
    }

    fn single() -> Value {
        json!([{ "question": "Which?", "header": "Pick", "options": [{ "label": "A", "description": "first" }, { "label": "B", "description": "" }] }])
    }

    fn two() -> Value {
        json!([{ "question": "One?", "header": "H1", "options": ["A", "B"] }, { "question": "Two?", "header": "H2", "options": ["C", "D"] }])
    }

    fn answers(effect: DockEffect) -> Vec<Option<String>> {
        match effect {
            DockEffect::Respond(response) => response.answers,
            other => panic!("no response: {other:?}"),
        }
    }

    #[test]
    fn reads_the_questions_a_call_asked_new_and_old() {
        let questions = questions_from_input(&json!({ "questions": [{ "question": " Which? ", "options": ["A", { "label": "B", "description": " b ", "preview": "x" }, 3, { "label": " " }], "multiSelect": true }, { "question": "" }, 7] }));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].question, "Which?");
        assert_eq!(questions[0].header, "Question");
        assert!(questions[0].multi_select);
        assert_eq!(questions[0].options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(questions[0].options[1].description, "b");
        assert_eq!(questions[0].options[1].preview.as_deref(), Some("x"));
        let old = questions_from_input(&json!({ "question": "Old?", "options": ["Y"] }));
        assert_eq!(old[0].question, "Old?");
        assert!(questions_from_input(&json!({})).is_empty());
    }

    #[test]
    fn submits_a_lone_single_select_question_as_soon_as_an_option_is_picked() {
        let mut state = card(single());
        assert!(state.hide_submit_tab());
        assert_eq!(answers(state.pick("A")), [Some("A".to_owned())]);
        assert!(state.responded);
        // One card answers once.
        assert_eq!(state.pick("B"), DockEffect::None);
    }

    #[test]
    fn digit_keys_pick_rows() {
        let mut state = card(single());
        assert_eq!(answers(state.key(DockKey::Char('2'))), [Some("B".to_owned())]);
    }

    #[test]
    fn advances_on_each_pick_and_submits_what_was_answered_from_the_review_screen() {
        let mut state = card(two());
        assert_eq!(state.pick("A"), DockEffect::None);
        assert_eq!(state.index, 1);
        assert_eq!(state.pick("D"), DockEffect::None);
        assert!(state.on_review());
        assert_eq!(answers(state.submit()), [Some("A".to_owned()), Some("D".to_owned())]);
    }

    #[test]
    fn joins_multi_select_answers_typed_other_included_and_moves_on_with_next() {
        let mut state = card(json!([{ "question": "Many?", "options": ["A", "B, with comma"], "multiSelect": true }]));
        assert!(!state.hide_submit_tab());
        state.toggle("B, with comma");
        state.toggle("A");
        state.set_other("typed".to_owned());
        let next = state.next_button_row().unwrap();
        assert_eq!(state.activate_row(next), DockEffect::None);
        assert!(state.on_review());
        // In option order, whatever order they were checked in.
        assert_eq!(answers(state.submit()), [Some("A, \"B, with comma\", typed".to_owned())]);
    }

    #[test]
    fn answers_a_single_select_question_with_the_typed_other_and_closes_on_a_blank_one() {
        let mut state = card(single());
        state.set_other("mine".to_owned());
        assert_eq!(state.draft().unwrap().selected.as_deref(), Some(OTHER));
        assert_eq!(answers(state.other_enter(false)), [Some("mine".to_owned())]);

        let mut blank = card(single());
        assert_eq!(blank.other_enter(false), DockEffect::Respond(QuestionResponse::close()));
    }

    #[test]
    fn checks_the_other_circle_when_its_box_is_entered_and_clears_it_when_an_option_is_picked() {
        let mut state = card(two());
        state.choose_other();
        assert_eq!(state.drafts[0].selected.as_deref(), Some(OTHER));
        // With nothing typed it is not an answer.
        assert_eq!(state.answers()[0], None);
        state.pick("B");
        assert_eq!(state.drafts[0].selected.as_deref(), Some("B"));
    }

    #[test]
    fn closes_with_esc() {
        let mut state = card(two());
        assert_eq!(state.key(DockKey::Escape), DockEffect::Respond(QuestionResponse::close()));
    }

    #[test]
    fn keeps_the_page_told_what_a_composer_message_would_hand_back() {
        let mut state = card(two());
        assert_eq!(composer_response(&state.questions, &state.drafts), QuestionResponse::close());
        state.pick("A");
        let response = composer_response(&state.questions, &state.drafts);
        assert_eq!(response.action, QuestionAction::Submit);
        assert_eq!(response.answers, [Some("A".to_owned()), None]);
        assert!(state.draft_dirty);
    }

    #[test]
    fn shows_the_focused_options_preview_beside_the_list_and_takes_notes_with_n() {
        let mut state = card(json!([{ "question": "Design?", "options": [{ "label": "A", "preview": "aaa" }, { "label": "B", "preview": "bbb" }] }]));
        assert!(state.preview_layout());
        assert_eq!(state.other_row(), None);
        assert_eq!(state.key(DockKey::Char('2')), DockEffect::FocusRow(1), "a digit only moves focus");
        assert_eq!(state.focus, 1);
        assert_eq!(state.key(DockKey::Char('n')), DockEffect::FocusNotes);
        assert!(state.notes_open);
        state.set_notes(" looks good ".to_owned());
        // Esc closes the box, not the card.
        assert_eq!(state.escape(), DockEffect::FocusCard);
        assert!(!state.notes_open);
        state.open_notes();
        // Focus moved to B while the notes were open, so Enter picks B.
        state.key(DockKey::Up);
        state.key(DockKey::Down);
        match state.notes_enter() {
            DockEffect::Respond(response) => {
                assert_eq!(response.answers, [Some("B".to_owned())]);
                assert_eq!(response.previews, Some(vec![Some("bbb".to_owned())]));
                assert_eq!(response.notes, Some(vec![Some("looks good".to_owned())]));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn notes_with_nothing_selected_still_answer_the_question() {
        let mut state = card(json!([{ "question": "D?", "options": [{ "label": "A", "preview": "a" }] }, { "question": "E?", "options": ["F"] }]));
        state.open_notes();
        state.set_notes("note".to_owned());
        assert_eq!(state.notes_enter(), DockEffect::FocusCard);
        assert_eq!(state.index, 1);
        let response = composer_response(&state.questions, &state.drafts);
        assert_eq!(response.action, QuestionAction::Submit);
    }

    #[test]
    fn unlocks_for_a_retry_when_the_host_did_not_take_the_answer() {
        let mut state = card(single());
        assert!(matches!(state.pick("A"), DockEffect::Respond(_)));
        assert!(state.locked());
        assert_eq!(state.key(DockKey::Char('1')), DockEffect::None);
        state.responded = false;
        assert!(matches!(state.pick("A"), DockEffect::Respond(_)));
    }

    #[test]
    fn walks_the_rows_and_the_tabs_with_the_keys() {
        let mut state = card(two());
        assert_eq!(state.key(DockKey::Down), DockEffect::FocusRow(1));
        assert_eq!(state.key(DockKey::Down), DockEffect::FocusOther);
        assert_eq!(state.key(DockKey::Down), DockEffect::FocusRow(3), "the chat row");
        assert_eq!(state.key(DockKey::Down), DockEffect::FocusRow(3), "stops at the last");
        assert_eq!(state.key(DockKey::CtrlP), DockEffect::FocusOther);
        state.key(DockKey::Tab { shift: false });
        assert_eq!(state.index, 1);
        assert_eq!(state.focus, 0, "a new question starts at its top");
        state.key(DockKey::Left);
        assert_eq!(state.index, 0);
        state.key(DockKey::Tab { shift: true });
        assert_eq!(state.index, 0, "stops at the first");
    }

    #[test]
    fn the_chat_row_hands_back_what_was_filled_in() {
        let mut state = card(two());
        state.pick("A");
        match state.chat() {
            DockEffect::Respond(response) => {
                assert_eq!(response.action, QuestionAction::Chat);
                assert_eq!(response.answers, [Some("A".to_owned()), None]);
            }
            other => panic!("{other:?}"),
        }
    }
}
