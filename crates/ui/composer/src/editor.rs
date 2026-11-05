//! The composer's text area: upstream's `<textarea>` with `usePastedTextTags`.
//!
//! A tag is its label's text, so it selects, copies and undoes natively; `TextEdit` keeps the caret from stopping inside one, deletes
//! one whole, and draws it. What is left here is deciding what a key means and what a paste becomes. Everything the input method
//! does stays inside `TextEdit`, so a commit and a preedit never pass through a shortcut: keys are only asked about once nothing is
//! being composed.

use crate::attach::{attach_clipboard_contents, attach_ports};
use crate::pasted_text::{copy_selection, normalize_pasted_text, paste_action, pasted_text_ranges, PasteAction};
use fluent::edit::EditHooks;
use mewrk_client::prelude::*;
use mewrk_client::runs::PastedText;
use mewrk_client::shortcuts::{matches_event, KeyInput};
use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};
use std::ops::Range;
use std::time::{Duration, Instant};

/// `.composer textarea`: 13.5px on a 1.55 line.
pub const TEXTAREA_TYPE: TypeStyle = TypeStyle { size: 13.5, line: 21.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
const MIN_HEIGHT: f32 = 29.0;
const MAX_HEIGHT: f32 = 180.0;
const PADDING_X: f32 = 3.0;
const PADDING_Y: f32 = 4.0;

const KEY: &str = "composer-textarea";

/// The id the text area holds the focus under.
pub fn textarea_focus_id() -> u64 {
    hash_key(KEY)
}

/// `.key(...)` of the element, for finding its rectangle.
pub fn textarea_key() -> u64 {
    textarea_focus_id()
}

/// `composerTextareaRef.current.focus()`.
pub fn focus_composer() {
    focus_request(KEY);
}

/// `composer-text-menu`'s key, for finding its rows.
pub const TEXT_MENU: &str = "composer-text-menu";

/// How long the right-click menu waits for the checker's guesses before opening without them, so that its first row is the first guess.
const GUESSES_WAIT: Duration = Duration::from_millis(200);
/// How many of them it shows, the likeliest: the commands below are what the menu is mostly for.
const MAX_GUESSES: usize = 8;

/// The right-click menu, while it is open: where it was asked for, and the misspelt word under the press.
#[derive(Clone)]
struct TextMenu {
    at: (f32, f32),
    word: Option<(Range<usize>, String)>,
    pressed: Instant,
}

#[derive(Default)]
struct EditorState {
    edit: TextEdit,
    conversation: String,
    focused: bool,
    menu: Option<TextMenu>,
}

/// What the owner of a text box decides for it: the send and newline shortcuts, and everything about long pastes.
struct ComposerHooks<'a> {
    app: &'a mut App,
    conversation: &'a str,
    /// The card docked above, which Tab goes to: nothing here walks the focus from control to control as a page does.
    card: Option<u64>,
    pastes: Vec<PastedText>,
    pastes_changed: bool,
    send: &'a [String],
    newline: &'a [String],
    wants_send: bool,
}

impl EditHooks for ComposerHooks<'_> {
    fn key(&mut self, edit: &mut TextEdit, press: &KeyPress) -> bool {
        // Send and newline shortcuts are configurable but mutually exclusive, so checking send first cannot trigger both.
        let event = KeyInput::from_key_press(*press);
        if matches_event(self.send, &event) {
            self.wants_send = true;
            return true;
        }
        if matches_event(self.newline, &event) {
            edit.insert("\n");
            return true;
        }
        if press.key == Key::Tab {
            if let Some(card) = self.card {
                focus_request(card);
            }
            return true;
        }
        // A textarea breaks the line for a plain, Shift or Option Enter whatever the bindings say. The Command chord only does
        // something when it is bound.
        press.key == Key::Enter && press.cmd
    }

    fn copied(&mut self, edit: &TextEdit, range: Range<usize>) -> Option<String> {
        Some(copy_selection(&edit.text, range, &self.pastes))
    }

    fn paste(&mut self, edit: &mut TextEdit, text: Option<String>) -> bool {
        let read = attach_ports(self.app).read_clipboard.clone();
        let contents = read();
        if !contents.is_empty() {
            attach_clipboard_contents(self.app, self.conversation, contents);
            // A clipboard carrying both keeps its text; the files ride along.
            return text.is_none();
        }
        let Some(text) = text else { return true };
        match paste_action(&text, &self.pastes) {
            PasteAction::Native => edit.insert(&normalize_pasted_text(&text)),
            PasteAction::Insert { text, added } => {
                // A long paste folds into a tag the send expands again, so the box stays readable and the model reads it all.
                if !added.is_empty() {
                    self.pastes.extend(added);
                    self.pastes_changed = true;
                }
                edit.insert(&text);
            }
        }
        true
    }

    fn atoms(&mut self, text: &str) -> Vec<Range<usize>> {
        pasted_text_ranges(text, &self.pastes).into_iter().map(|range| range.start..range.end).collect()
    }
}

/// What the view reads back from the text area.
pub struct TextArea {
    pub element: E,
    /// It held the focus last frame: the composer's box draws its focused edge from this.
    pub focused: bool,
    /// The right-click menu, lifted out of the tree wherever it is written.
    pub menu: Option<E>,
}

/// A key a menu row presses for the text area: the same path as the key itself, so a cut folds a tag back and a paste can attach.
fn press_key(state: &Local<EditorState>, c: char) {
    state.borrow_mut().edit.press_from_menu(KeyPress { key: Key::Char(c), shift: false, cmd: true, alt: false, ctrl: false });
    focus_composer();
    repaint();
}

/// Upstream's textarea gets the system's own menu, which `spellCheck` adds to: the checker's guesses for a misspelt word and what else
/// it offers for one, over the editing commands. Here the same rows, in the shared menu.
fn text_menu(menu: TextMenu, state: &Local<EditorState>, conversation: &str) -> Option<E> {
    let mut sections = Vec::new();
    if let Some((range, word)) = menu.word {
        let mut rows = Vec::new();
        match fluent::spell::guesses(&word) {
            // Still out with the checker. If it is slow the menu opens anyway, and its rows come in when it answers.
            None => {
                repaint();
                if menu.pressed.elapsed() < GUESSES_WAIT {
                    return None;
                }
            }
            Some(guesses) if guesses.is_empty() => rows.push(ContextMenuItem::new("no-guesses", t("无拼写建议", "No Guesses Found")).disabled(true)),
            Some(guesses) => {
                for (index, guess) in guesses.into_iter().take(MAX_GUESSES).enumerate() {
                    let (state, conversation, range, word) = (state.clone(), conversation.to_owned(), range.clone(), word.clone());
                    rows.push(ContextMenuItem::new(format!("guess-{index}"), guess.clone()).on_select(move |app: &mut App| {
                        let mut st = state.borrow_mut();
                        // The draft can have moved on under an open menu, and then the guess is for a word that is gone.
                        if st.edit.text.get(range.clone()) == Some(word.as_str()) {
                            st.edit.replace_range(range.clone(), &guess);
                            app.set_composer_draft(&conversation, st.edit.text.clone());
                        }
                        focus_composer();
                        repaint();
                    }));
                }
            }
        }
        let (ignoring, learning) = (word.clone(), word);
        rows.push(ContextMenuItem::new("ignore", t("忽略拼写", "Ignore Spelling")).on_select(move |_: &mut App| {
            fluent::spell::ignore(&ignoring);
            focus_composer();
            repaint();
        }));
        rows.push(ContextMenuItem::new("learn", t("学习拼写", "Learn Spelling")).on_select(move |_: &mut App| {
            fluent::spell::learn(&learning);
            focus_composer();
            repaint();
        }));
        sections.push(ContextMenuSection::new("spelling", rows));
    }
    let selected = !state.borrow().edit.selection_range().is_empty();
    let key = |id: &str, label: &str, c: char, enabled: bool| {
        let state = state.clone();
        ContextMenuItem::new(id, label).disabled(!enabled).on_select(move |_: &mut App| press_key(&state, c))
    };
    sections.push(ContextMenuSection::new("edit", vec![key("cut", t("剪切", "Cut"), 'x', selected), key("copy", t("拷贝", "Copy"), 'c', selected), key("paste", t("粘贴", "Paste"), 'v', true), key("select-all", t("全选", "Select All"), 'a', true)]));
    let closing = state.clone();
    Some(context_menu(
        TEXT_MENU,
        ContextMenuProps::new(
            ContextMenuAnchor::Point { x: menu.at.0, y: menu.at.1 },
            t("文本", "Text"),
            move |_: &mut App| {
                closing.borrow_mut().menu = None;
                repaint();
            },
            sections,
        ),
    ))
}

/// The text area for a conversation's draft. It grows with its text, up to a height where it scrolls instead.
#[allow(clippy::too_many_arguments)]
pub fn composer_text_area(conversation_id: &str, draft: String, placeholder: &'static str, disabled: bool, spell_check: bool, send: Vec<String>, newline: Vec<String>, card: Option<u64>) -> TextArea {
    let state = use_state("composer-textarea-state", EditorState::default);
    let focused = {
        let mut st = state.borrow_mut();
        // The draft belongs to the conversation; a box that shows another's, or a draft changed from outside, takes it. Mid-composition
        // it waits, since what is on screen isn't text yet.
        if st.conversation != conversation_id {
            st.conversation = conversation_id.to_owned();
            st.edit.set_text(draft);
        } else if st.edit.text != draft && !st.edit.is_composing() {
            st.edit.set_text(draft);
        }
        st.focused
    };
    let open_menu = state.borrow().menu.clone();
    let menu = open_menu.and_then(|menu| text_menu(menu, &state, conversation_id));
    let measured = state.clone();
    let pressed = state.clone();
    let conversation = conversation_id.to_owned();
    let element = custom(move |app: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let mut guard = state.borrow_mut();
        let st = &mut *guard;
        let pastes = app.composer().pasted_texts.get(&conversation).cloned().unwrap_or_default();
        let mut hooks = ComposerHooks { app, conversation: &conversation, card, pastes, pastes_changed: false, send: &send, newline: &newline, wants_send: false };
        st.edit.set_read_only(disabled);
        st.edit.set_spell_check(spell_check);
        st.edit.set_atom_fill(Some(ui.theme.accent.fade(0.18)));
        let inner = rect(r.x + PADDING_X, r.y + PADDING_Y, (r.w - PADDING_X * 2.0).max(1.0), (r.h - PADDING_Y * 2.0).max(1.0));
        let response = st.edit.show_with(ui, textarea_focus_id(), inner, placeholder, &TEXTAREA_TYPE, false, &mut hooks);
        let (wants_send, pastes_changed, pastes) = (hooks.wants_send, hooks.pastes_changed, hooks.pastes);
        if pastes_changed {
            app.set_composer_pasted_texts(&conversation, pastes);
        }
        if response.changed {
            app.set_composer_draft(&conversation, st.edit.text.clone());
        }
        let now_focused = ui.mem.focus == textarea_focus_id();
        if now_focused != st.focused {
            st.focused = now_focused;
            repaint();
        }
        drop(guard);
        if wants_send {
            app.send_message();
        }
    })
    .key(textarea_key())
    .aria_label(t("向 Agent 发送消息", "Message the Agent"))
    .focus_under(textarea_focus_id())
    .keeps_tab()
    .flex1()
    .min_w(0.0)
    .on_context_menu(move |_: &mut App, e| {
        if disabled {
            return;
        }
        let mut st = pressed.borrow_mut();
        let word = st.edit.press_for_menu(e.x, e.y).map(|range| {
            let word = st.edit.text[range.clone()].to_owned();
            (range, word)
        });
        st.menu = Some(TextMenu { at: (e.x, e.y), word, pressed: Instant::now() });
        focus_composer();
        repaint();
    })
    .measure(move |_: &mut App, ui: &mut Ui<'_>, known: Option<f32>, avail: f32| {
        let width = known.unwrap_or(if avail.is_finite() { avail } else { 240.0 });
        let height = measured.borrow_mut().edit.measure(ui, &TEXTAREA_TYPE, (width - PADDING_X * 2.0).max(1.0)) + PADDING_Y * 2.0;
        (width, height.clamp(MIN_HEIGHT, MAX_HEIGHT))
    });
    TextArea { element, focused, menu }
}

/// The words drawn as misspelt.
#[cfg(test)]
pub(crate) fn misspelt_words() -> Vec<String> {
    let state = use_state("composer-textarea-state", EditorState::default);
    let st = state.borrow();
    st.edit.misspelt().iter().map(|word| st.edit.text[word.clone()].to_owned()).collect()
}

/// `draftWithMention`: the line goes on a line of its own at the end of the draft.
pub fn draft_with_mention(draft: &str, mention: &str) -> String {
    if draft.is_empty() {
        return mention.to_owned();
    }
    format!("{draft}{}{mention}", if draft.ends_with('\n') { "" } else { "\n" })
}

/// `mentionFileInComposer`'s end of it: a draft the composer already holds is kept, and the caret ends up after the mention, ready
/// for what there is to say about it.
pub fn put_mention_in_composer(app: &mut App, conversation_id: &str, mention: &str) {
    let draft = app.composer_draft(conversation_id);
    app.set_composer_draft(conversation_id, draft_with_mention(&draft, mention));
    focus_composer();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mention_goes_on_a_line_of_its_own() {
        assert_eq!(draft_with_mention("", "@a.rs"), "@a.rs");
        assert_eq!(draft_with_mention("look", "@a.rs"), "look\n@a.rs");
        assert_eq!(draft_with_mention("look\n", "@a.rs"), "look\n@a.rs");
    }
}
