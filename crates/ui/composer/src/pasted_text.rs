//! A long paste folds into a tag in the box, the way Claude Code folds it: upstream's `lib/pastedText.ts`. The draft holds a
//! short label, and the text the label stands for waits beside the draft until the message leaves the box, where every label
//! becomes that text again. Nothing downstream of the box ever sees a label.
//!
//! A label is plain text in the box, so it selects, copies and undoes like the words around it. `TextEdit` treats its range as
//! one character and draws the tag behind it; a label edited by hand is no longer one, and sends as the characters it was left
//! with. Positions here are byte offsets into the draft, where upstream's were UTF-16 ones.

use mewrk_client::runs::composer::expand_pasted_texts;
use mewrk_client::runs::PastedText;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;

/// A paste longer than this many characters folds into a tag (Claude Code's threshold).
const PASTED_TEXT_FOLD_CHARACTERS: usize = 800;

const NBSP: char = '\u{a0}';

/// Word joiner and zero-width joiner: invisible, and neither lets a line break beside it.
const ID_MARK_BITS: [char; 2] = ['\u{2060}', '\u{200d}'];

/// A paste's number spelled in invisible marks. Two tags that read the same on screen still differ in the text, which is how
/// the draft tells them apart.
pub fn pasted_text_id_mark(id: u32) -> String {
    format!("{:b}", id.max(1)).chars().map(|bit| ID_MARK_BITS[usize::from(bit == '1')]).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedTextRange {
    pub start: usize,
    pub end: usize,
    pub paste: PastedText,
}

/// What copying out of a box keeps beside the plain text: the text as the box held it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftClipboard {
    /// The copied span as the box held it, labels and all.
    pub text: String,
    /// The text behind each label in the span.
    pub pastes: Vec<DraftPaste>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftPaste {
    pub label: String,
    pub content: String,
}

/// A text box keeps LF only; the text behind a label is what the box would have held.
pub fn normalize_pasted_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn folds_pasted_text(text: &str) -> bool {
    text.encode_utf16().nth(PASTED_TEXT_FOLD_CHARACTERS).is_some()
}

/// Claude Code's "+N lines": the line breaks in the text, so the lines after its first.
pub fn pasted_text_extra_lines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

pub fn next_pasted_text_id(pastes: &[PastedText]) -> u32 {
    pastes.iter().map(|paste| paste.id).max().unwrap_or(0) + 1
}

/// Where the draft's labels stand, first to last.
pub fn pasted_text_ranges(value: &str, pastes: &[PastedText]) -> Vec<PastedTextRange> {
    if value.is_empty() || pastes.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<PastedTextRange> = Vec::new();
    for paste in pastes {
        if paste.label.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(at) = value[from..].find(&paste.label) {
            let start = from + at;
            found.push(PastedTextRange { start, end: start + paste.label.len(), paste: paste.clone() });
            from = start + paste.label.len();
        }
    }
    found.sort_by(|left, right| left.start.cmp(&right.start).then(right.end.cmp(&left.end)));
    // Labels differ in their numbers, so none sits inside another; this only guards a label a user built by hand out of
    // pieces of two.
    let mut ranges: Vec<PastedTextRange> = Vec::new();
    for range in found {
        if range.start >= ranges.last().map_or(0, |last| last.end) {
            ranges.push(range);
        }
    }
    ranges
}

/// The label a position cuts into, if it falls strictly inside one.
pub fn pasted_text_range_around(ranges: &[PastedTextRange], position: usize) -> Option<&PastedTextRange> {
    ranges.iter().find(|range| range.start < position && position < range.end)
}

/// What copying `value[start..end]` out of a box puts on the clipboard: the text as it reads anywhere else (every label
/// expanded), and the box's own form, so a paste back into a box brings back exactly what was copied: tags as tags, and text as
/// text however long it is.
pub fn draft_clipboard(value: &str, start: usize, end: usize, pastes: &[PastedText]) -> (String, DraftClipboard) {
    let text = &value[start..end];
    let mut used: Vec<DraftPaste> = Vec::new();
    for range in pasted_text_ranges(text, pastes) {
        if !used.iter().any(|paste| paste.label == range.paste.label) {
            used.push(DraftPaste { label: range.paste.label, content: range.paste.content });
        }
    }
    (expand_pasted_texts(text, pastes), DraftClipboard { text: text.to_owned(), pastes: used })
}

pub fn parse_draft_clipboard(raw: &str) -> Option<DraftClipboard> {
    if raw.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let text = parsed.get("text")?.as_str()?.to_owned();
    let pastes = parsed.get("pastes")?.as_array()?;
    let pastes = pastes
        .iter()
        .filter_map(|paste| {
            let label = paste.get("label")?.as_str().filter(|label| !label.is_empty())?;
            Some(DraftPaste { label: label.to_owned(), content: paste.get("content")?.as_str()?.to_owned() })
        })
        .collect();
    Some(DraftClipboard { text, pastes })
}

/// Names a paste by its number and by how many lines follow its first.
pub type PastedTextLabeler<'a> = &'a dyn Fn(u32, usize) -> String;

/// Brings copied tags into a draft. A tag whose text the draft already holds keeps that paste's number (cutting a tag and pasting
/// it back leaves it as it was), and any other takes the draft's next free number, so a #1 copied out of another draft never
/// collides with this one's #1.
pub fn adopt_draft_clipboard(clip: &DraftClipboard, existing: &[PastedText], label_for: PastedTextLabeler<'_>) -> (String, Vec<PastedText>) {
    if clip.pastes.is_empty() {
        return (clip.text.clone(), Vec::new());
    }
    let mut added: Vec<PastedText> = Vec::new();
    let mut next_id = next_pasted_text_id(existing);
    let incoming: Vec<PastedText> = clip.pastes.iter().enumerate().map(|(index, paste)| PastedText { id: u32::MAX - index as u32, label: paste.label.clone(), content: paste.content.clone() }).collect();
    let mut adopted: Vec<(String, PastedText)> = Vec::new();
    for paste in &incoming {
        let held = existing.iter().chain(added.iter()).find(|candidate| candidate.content == paste.content).cloned();
        let target = held.unwrap_or_else(|| {
            let fresh = PastedText { id: next_id, label: label_for(next_id, pasted_text_extra_lines(&paste.content)), content: paste.content.clone() };
            next_id += 1;
            added.push(fresh.clone());
            fresh
        });
        adopted.push((paste.label.clone(), target));
    }
    let mut text = String::new();
    let mut at = 0;
    for range in pasted_text_ranges(&clip.text, &incoming) {
        text.push_str(&clip.text[at..range.start]);
        if let Some((_, target)) = adopted.iter().find(|(label, _)| *label == range.paste.label) {
            text.push_str(&target.label);
        }
        at = range.end;
    }
    text.push_str(&clip.text[at..]);
    (text, added)
}

/// A paste's label in the current language. No-break spaces pad the tag and keep a label from wrapping partway through; the
/// number is carried invisibly, so tags that read alike stay distinct.
pub fn pasted_text_label(id: u32, extra_lines: usize) -> String {
    use mewrk_client::{t, tf};
    let name = if extra_lines > 0 { tf("粘贴文本 +{lines} 行", "Pasted text +{lines} lines", &[("lines", &extra_lines)]) } else { t("粘贴文本", "Pasted text").to_owned() };
    format!("{NBSP}{}{}{NBSP}", name.replace(' ', "\u{a0}"), pasted_text_id_mark(id))
}

thread_local! {
    /// The last copy out of any box. A clipboard that kept the plain text but dropped the box's own form still pastes back as
    /// the tags it was copied as, and the system clipboard here only ever carries text.
    static LAST_DRAFT_COPY: RefCell<Option<(String, DraftClipboard)>> = const { RefCell::new(None) };
}

/// A copy or a cut of `value[range]`: remembers the box's form, and answers the text the clipboard takes.
pub fn copy_selection(value: &str, range: std::ops::Range<usize>, pastes: &[PastedText]) -> String {
    let (plain, draft) = draft_clipboard(value, range.start, range.end, pastes);
    LAST_DRAFT_COPY.with(|last| *last.borrow_mut() = Some((plain.clone(), draft)));
    plain
}

/// What a paste of `plain` into a box does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteAction {
    /// Left to the box: it inserts the text as text.
    Native,
    /// The box inserts this instead, and the draft takes these pastes.
    Insert { text: String, added: Vec<PastedText> },
}

/// `usePastedTextTags.onPaste`: folds a long paste into a tag, and brings text copied out of a box back as it was copied.
pub fn paste_action(plain: &str, existing: &[PastedText]) -> PasteAction {
    let plain = normalize_pasted_text(plain);
    let copied = LAST_DRAFT_COPY.with(|last| last.borrow().as_ref().filter(|(copied, _)| !plain.is_empty() && normalize_pasted_text(copied) == plain).map(|(_, draft)| draft.clone()));
    if let Some(copied) = copied {
        // Text copied out of a box comes back as it was copied, however long.
        if copied.pastes.is_empty() && copied.text == plain {
            return PasteAction::Native;
        }
        let (text, added) = adopt_draft_clipboard(&copied, existing, &pasted_text_label);
        return PasteAction::Insert { text, added };
    }
    if !folds_pasted_text(&plain) {
        return PasteAction::Native;
    }
    let id = next_pasted_text_id(existing);
    let paste = PastedText { id, label: pasted_text_label(id, pasted_text_extra_lines(&plain)), content: plain };
    PasteAction::Insert { text: paste.label.clone(), added: vec![paste] }
}

/// Forgets the last copy, for a test that starts clean.
pub fn forget_last_copy() {
    LAST_DRAFT_COPY.with(|last| *last.borrow_mut() = None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label_for(id: u32, extra: usize) -> String {
        if extra > 0 { format!("[Pasted text #{id} +{extra} lines]") } else { format!("[Pasted text #{id}]") }
    }

    fn paste(id: u32, content: &str) -> PastedText {
        PastedText { id, label: label_for(id, pasted_text_extra_lines(content)), content: content.to_owned() }
    }

    #[test]
    fn folds_only_a_paste_longer_than_800_characters() {
        assert!(!folds_pasted_text(&"a".repeat(800)));
        assert!(folds_pasted_text(&"a".repeat(801)));
        // Line count alone does not fold: short lines stay in the box.
        assert!(!folds_pasted_text(&vec!["x"; 200].join("\n")));
    }

    #[test]
    fn counts_the_lines_after_the_first_and_keeps_only_lf() {
        assert_eq!(pasted_text_extra_lines("one"), 0);
        assert_eq!(pasted_text_extra_lines("one\ntwo\nthree"), 2);
        assert_eq!(normalize_pasted_text("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn numbers_pastes_past_the_highest_the_draft_ever_held() {
        assert_eq!(next_pasted_text_id(&[]), 1);
        assert_eq!(next_pasted_text_id(&[paste(1, "a"), paste(4, "b")]), 5);
    }

    #[test]
    fn spells_a_pastes_number_in_invisible_marks_so_tags_that_read_alike_stay_apart() {
        let ids = [1, 2, 3, 4, 5, 8, 16, 255];
        let marks: Vec<String> = ids.iter().map(|id| pasted_text_id_mark(*id)).collect();
        for mark in &marks {
            assert!(mark.chars().all(|c| c == '\u{2060}' || c == '\u{200d}'));
        }
        assert_eq!(marks.iter().collect::<std::collections::HashSet<_>>().len(), ids.len());
        // The id in binary: a 1 is the zero-width joiner and a 0 the word joiner.
        assert_eq!(pasted_text_id_mark(1), "\u{200d}");
        assert_eq!(pasted_text_id_mark(2), "\u{200d}\u{2060}");
        assert_eq!(pasted_text_id_mark(3), "\u{200d}\u{200d}");

        // Two pastes whose labels read the same on screen: one name, different marks.
        let unmarked = |text: &str| text.replace(['\u{2060}', '\u{200d}'], "");
        let label = |id: u32| format!("\u{a0}粘贴文本\u{a0}+1\u{a0}行{}\u{a0}", pasted_text_id_mark(id));
        let first = PastedText { id: 1, label: label(1), content: "one\ntwo".into() };
        let second = PastedText { id: 2, label: label(2), content: "three\nfour".into() };
        assert_ne!(first.label, second.label);
        assert_eq!(unmarked(&first.label), unmarked(&second.label));
        assert!(!unmarked(&first.label).contains('#'));

        let value = format!("a {} b {} c {}", first.label, second.label, first.label);
        let pastes = [first.clone(), second.clone()];
        let ranges = pasted_text_ranges(&value, &pastes);
        assert_eq!(ranges.iter().map(|range| range.paste.id).collect::<Vec<_>>(), [1, 2, 1]);
        assert_eq!(ranges.iter().map(|range| &value[range.start..range.end]).collect::<Vec<_>>(), [first.label.as_str(), second.label.as_str(), first.label.as_str()]);
        assert_eq!(expand_pasted_texts(&value, &pastes), "a one\ntwo b three\nfour c one\ntwo");
        // Take the marks away and the label is only the words around it.
        let edited = format!("x {} y", unmarked(&first.label));
        assert!(pasted_text_ranges(&edited, &pastes).is_empty());
        assert_eq!(expand_pasted_texts(&edited, &pastes), edited);
    }

    #[test]
    fn finds_each_label_where_it_stands_and_none_a_hand_edit_broke() {
        let first = paste(1, "A\nB");
        let second = paste(2, "C");
        let value = format!("x {} y {}{} [Pasted text #2", first.label, second.label, first.label);
        let found: Vec<(usize, u32)> = pasted_text_ranges(&value, &[first.clone(), second.clone()]).iter().map(|range| (range.start, range.paste.id)).collect();
        assert_eq!(found, [(2, 1), (2 + first.label.len() + 3, 2), (2 + first.label.len() + 3 + second.label.len(), 1)]);
        assert!(pasted_text_ranges(&value, &[]).is_empty());
        let ranges = pasted_text_ranges(&value, std::slice::from_ref(&first));
        let range = &ranges[0];
        assert!(pasted_text_range_around(&ranges[..1], range.start).is_none());
        assert_eq!(pasted_text_range_around(&ranges[..1], range.start + 1), Some(range));
        assert!(pasted_text_range_around(&ranges[..1], range.end).is_none());
    }

    #[test]
    fn sends_every_label_as_the_text_it_stands_for() {
        let first = paste(1, "first\npaste");
        let second = paste(2, "second");
        assert_eq!(expand_pasted_texts(&format!("see {} and {}.", first.label, second.label), &[first.clone(), second]), "see first\npaste and second.");
        // A label for a paste this draft does not hold is just text.
        assert_eq!(expand_pasted_texts("[Pasted text #9]", &[first]), "[Pasted text #9]");
    }

    #[test]
    fn copies_a_span_as_the_text_it_reads_as_and_as_the_box_held_it() {
        let first = paste(1, "the long text");
        let other = paste(2, "not copied");
        let value = format!("before {} after {}", first.label, other.label);
        let end = value.find(" after").unwrap() + " after".len();
        let (plain, draft) = draft_clipboard(&value, 0, end, &[first.clone(), other]);
        assert_eq!(plain, "before the long text after");
        assert_eq!(draft, DraftClipboard { text: format!("before {} after", first.label), pastes: vec![DraftPaste { label: first.label.clone(), content: "the long text".into() }] });
        assert_eq!(parse_draft_clipboard(&serde_json::to_string(&draft).unwrap()), Some(draft));
        assert_eq!(parse_draft_clipboard("not json"), None);
        assert_eq!(parse_draft_clipboard(&serde_json::json!({ "text": 1, "pastes": [] }).to_string()), None);
        assert_eq!(parse_draft_clipboard(&serde_json::json!({ "text": "t", "pastes": [{ "label": "", "content": "c" }] }).to_string()), Some(DraftClipboard { text: "t".into(), pastes: vec![] }));
    }

    #[test]
    fn renumbers_copied_tags_into_a_draft_keeping_a_tag_whose_text_the_draft_already_holds() {
        let held = paste(1, "held text");
        let clip = DraftClipboard {
            text: format!("{} and {} again {}", label_for(1, 0), label_for(3, 1), label_for(1, 0)),
            pastes: vec![DraftPaste { label: label_for(1, 0), content: "from elsewhere".into() }, DraftPaste { label: label_for(3, 1), content: "held text".into() }],
        };
        let (text, added) = adopt_draft_clipboard(&clip, std::slice::from_ref(&held), &label_for);
        assert_eq!(added, [PastedText { id: 2, label: label_for(2, 0), content: "from elsewhere".into() }]);
        assert_eq!(text, format!("{} and {} again {}", label_for(2, 0), held.label, label_for(2, 0)));
        let mut all = vec![held];
        all.extend(added);
        assert_eq!(expand_pasted_texts(&text, &all), "from elsewhere and held text again from elsewhere");
        // Plain text copied out of a box comes back as it was, however long.
        let long = "z".repeat(2_000);
        assert_eq!(adopt_draft_clipboard(&DraftClipboard { text: long.clone(), pastes: vec![] }, &[], &label_for), (long, vec![]));
    }
}
