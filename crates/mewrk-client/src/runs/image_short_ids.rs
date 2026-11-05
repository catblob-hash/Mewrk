//! Port of `lib/imageShortIds.ts`.
//!
//! Conversation image numbering, mirroring the Rust run loop's helpers in
//! api.rs (`image_placeholder_ids` / `image_short_ids_in_use` /
//! `renumber_user_message_images`). The composer assigns numbers eagerly, the
//! way it must to keep two draft images apart, but shows a thumbnail rather
//! than the `[Image #N]` the number stands for: the token is written into the
//! text at the send boundary and hidden again when the message is read back.
//! The run loop re-checks on delivery, so a stale draft number is corrected
//! rather than trusted.

use std::collections::HashSet;

use crate::model::{ContextItem, ImageAttachment, QueuedMessage};

fn image_placeholder(short_id: u32) -> String {
    format!("[Image #{short_id}]")
}

/// Every `[Image #N]` match in the text: where it stands and its digits. `[Image #` plus one to nine digits and `]`.
fn placeholders(text: &str) -> Vec<(usize, usize, u32)> {
    const OPEN: &str = "[Image #";
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(OPEN) {
        let start = from + at;
        let digits_start = start + OPEN.len();
        let rest = &text[digits_start..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if (1..=9).contains(&digits) && rest.as_bytes().get(digits) == Some(&b']') {
            let end = digits_start + digits + 1;
            found.push((start, end, rest[..digits].parse().unwrap_or(0)));
            from = end;
        } else {
            from = digits_start;
        }
    }
    found
}

/// Every valid `[Image #N]` id (N >= 1) literally present in the text.
pub fn image_placeholder_ids(text: &str) -> Vec<u32> {
    placeholders(text).into_iter().map(|(_, _, id)| id).filter(|&id| id >= 1).collect()
}

/// Collects every image number already spoken for: stored short ids on user
/// and tool images plus literal placeholders in user text. Placeholders count
/// even when their image is gone, so a number the model may still be reading
/// about is never reissued.
pub fn image_short_ids_in_use<'a>(contexts: impl IntoIterator<Item = &'a ContextItem>) -> HashSet<u32> {
    let mut taken = HashSet::new();
    for context in contexts {
        match context {
            ContextItem::User { images, content, .. } => {
                taken.extend(images.iter().filter_map(|image| image.short_id));
                taken.extend(image_placeholder_ids(content));
            }
            ContextItem::Tool { result, .. } => taken.extend(result.images.iter().filter_map(|image| image.short_id)),
            _ => {}
        }
    }
    taken
}

/// Lowest fresh number strictly above everything taken.
pub fn next_image_short_id(taken: &HashSet<u32>) -> u32 {
    taken.iter().copied().max().unwrap_or(0) + 1
}

/// Queued messages are not transcript contexts yet, but their numbers are
/// already visible to the user, so allocation must treat them as taken.
pub fn reserve_queued_message_ids<'a>(mut taken: HashSet<u32>, messages: impl IntoIterator<Item = &'a QueuedMessage>) -> HashSet<u32> {
    for message in messages {
        taken.extend(message.images.iter().filter_map(|image| image.short_id));
        taken.extend(image_placeholder_ids(&message.content));
    }
    taken
}

/// Appends ` [Image #N]` to the draft, without a leading space on empty text.
pub fn append_image_placeholder(text: &str, short_id: u32) -> String {
    let placeholder = image_placeholder(short_id);
    if text.is_empty() {
        return placeholder;
    }
    if text.ends_with(' ') || text.ends_with('\n') { format!("{text}{placeholder}") } else { format!("{text} {placeholder}") }
}

/// Writes the placeholder the model reads for every image the text does not
/// already name.
///
/// No input box shows `[Image #N]` — a thumbnail stands for the image while the
/// message is being written — so the token is added at the boundary where the
/// text stops being a draft and becomes what the model will read. A placeholder
/// the user typed themselves keeps its place: only numbers nothing in the text
/// claims are appended, in image order.
pub fn with_image_placeholders(text: &str, images: &[ImageAttachment]) -> String {
    let mut present: HashSet<u32> = image_placeholder_ids(text).into_iter().collect();
    let mut result = text.to_owned();
    for image in images {
        let Some(short_id) = image.short_id else { continue };
        if !present.insert(short_id) {
            continue;
        }
        result = append_image_placeholder(&result, short_id);
    }
    result
}

/// The inverse of [`with_image_placeholders`], for reading and re-editing.
///
/// Only the trailing run is dropped, and only numbers this message carries a
/// thumbnail for — a `[Image #3]` citing an image from further up the transcript
/// has no thumbnail beside it and stays as text. The run is all-or-nothing: if
/// dropping it would leave another of this message's own numbers stranded in the
/// prose, the user was citing their images by number and every one of them is
/// kept, or "compare [Image #1] with [Image #2]" would read "compare with".
pub fn text_without_appended_image_placeholders(text: &str, images: &[ImageAttachment]) -> String {
    let carried: HashSet<u32> = images.iter().filter_map(|image| image.short_id).collect();
    if carried.is_empty() {
        return text.to_owned();
    }
    let mut stripped = text;
    loop {
        let Some(&(start, end, id)) = placeholders(stripped).last() else { break };
        if end != stripped.len() || !carried.contains(&id) {
            break;
        }
        // `\s*` before the placeholder goes with it.
        stripped = stripped[..start].trim_end_matches(char::is_whitespace);
    }
    if stripped == text {
        return text.to_owned();
    }
    if image_placeholder_ids(stripped).iter().any(|id| carried.contains(id)) { text.to_owned() } else { stripped.to_owned() }
}

/// Human-facing fragment for title fallbacks: placeholders removed, whitespace
/// collapsed. A message that is nothing but placeholders yields "" so callers
/// can fall through to an image name instead.
pub fn text_without_image_placeholders(text: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    for (start, end, _) in placeholders(text) {
        out.push_str(&text[at..start]);
        out.push(' ');
        at = end;
    }
    out.push_str(&text[at..]);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Removes every literal `[Image #N]` occurrence for the given id, collapsing
/// the double space an appended placeholder leaves behind. Text the user wrote
/// around the placeholder is untouched.
pub fn strip_image_placeholder(text: &str, short_id: u32) -> String {
    let placeholder = image_placeholder(short_id);
    let mut result = String::new();
    let mut rest = text.to_owned();
    while let Some(at) = rest.find(&placeholder) {
        let mut before = rest[..at].to_owned();
        let mut after = rest[at + placeholder.len()..].to_owned();
        if before.ends_with(' ') && (after.starts_with(' ') || after.starts_with('\n') || after.is_empty()) {
            before.pop();
        } else if after.starts_with(' ') && before.is_empty() {
            after.remove(0);
        }
        result.push_str(&before);
        rest = after;
    }
    result + &rest
}

/// Send-time collision repair, mirroring Rust `renumber_user_message_images`:
/// an image keeps its draft number when no transcript entry claims it;
/// a real collision gets a fresh number and its placeholder text is rewritten
/// to match. Keepers are reserved before any fresh number is chosen, so a
/// rewrite can never land on a number another draft image still holds.
pub fn renumber_images_for_send(mut taken: HashSet<u32>, content: &str, images: &[ImageAttachment]) -> (String, Vec<ImageAttachment>) {
    let keeps: Vec<bool> = images
        .iter()
        .map(|image| match image.short_id {
            Some(short_id) if !taken.contains(&short_id) => {
                taken.insert(short_id);
                true
            }
            _ => false,
        })
        .collect();
    let mut text = content.to_owned();
    let mut renumbered = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        if keeps[index] {
            renumbered.push(image.clone());
            continue;
        }
        let fresh = next_image_short_id(&taken);
        taken.insert(fresh);
        if let Some(short_id) = image.short_id {
            text = text.replace(&image_placeholder(short_id), &image_placeholder(fresh));
        }
        renumbered.push(ImageAttachment { short_id: Some(fresh), ..image.clone() });
    }
    (text, renumbered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolResult;

    fn image(short_id: Option<u32>) -> ImageAttachment {
        ImageAttachment { id: "a".repeat(64), name: "pixel.png".into(), mime: "image/png".into(), width: 1, height: 1, bytes: 68, short_id }
    }

    #[test]
    fn finds_every_well_formed_placeholder() {
        assert_eq!(image_placeholder_ids("see [Image #1] and [Image #23]"), [1, 23]);
    }

    #[test]
    fn rejects_zero_overlong_digits_and_malformed_brackets() {
        for text in ["[Image #0]", "[Image #1234567890]", "[Image #2", "[Image # 3]", "[image #3]"] {
            assert!(image_placeholder_ids(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn collects_user_image_ids_user_text_placeholders_and_tool_image_ids() {
        let contexts = vec![
            ContextItem::User {
                id: "u1".into(),
                content: "kept text [Image #7] after its image was removed".into(),
                images: vec![image(Some(2))],
                files: vec![],
                created_at: "2026-08-06T00:00:00Z".into(),
            },
            ContextItem::Tool {
                id: "t1".into(),
                tool_name: "preview_screenshot".into(),
                round: None,
                model_turn_id: None,
                provider_call_id: None,
                requested_input: None,
                input: Default::default(),
                result: ToolResult { success: true, output: "{}".into(), images: vec![image(Some(4))], diff: None, executed_at: "2026-08-06T00:00:00Z".into(), duration_ms: 1 },
                subagent: None,
                notice: None,
                attestation: String::new(),
                created_at: "2026-08-06T00:00:00Z".into(),
            },
            ContextItem::Assistant {
                id: "a1".into(),
                content: "[Image #9] mentioned by the model does not reserve a number".into(),
                round: None,
                model_turn_id: None,
                interrupted: false,
                sources: vec![],
                created_at: "2026-08-06T00:00:00Z".into(),
            },
        ];
        let mut taken: Vec<u32> = image_short_ids_in_use(&contexts).into_iter().collect();
        taken.sort_unstable();
        assert_eq!(taken, [2, 4, 7]);
    }

    #[test]
    fn next_id_starts_at_one_and_goes_above_the_maximum() {
        assert_eq!(next_image_short_id(&HashSet::new()), 1);
        assert_eq!(next_image_short_id(&HashSet::from([3, 7])), 8);
    }

    #[test]
    fn appends_with_a_separating_space_only_when_needed() {
        assert_eq!(append_image_placeholder("", 1), "[Image #1]");
        assert_eq!(append_image_placeholder("hello", 2), "hello [Image #2]");
        assert_eq!(append_image_placeholder("hello ", 3), "hello [Image #3]");
        assert_eq!(append_image_placeholder("line\n", 4), "line\n[Image #4]");
    }

    #[test]
    fn writes_a_number_for_every_image_the_text_does_not_already_name() {
        assert_eq!(with_image_placeholders("", &[image(Some(1)), image(Some(2))]), "[Image #1] [Image #2]");
        assert_eq!(with_image_placeholders("看看这个", &[image(Some(1))]), "看看这个 [Image #1]");
    }

    #[test]
    fn leaves_a_placeholder_the_user_typed_where_they_put_it() {
        assert_eq!(with_image_placeholders("对比 [Image #2] 和这个", &[image(Some(2)), image(Some(3))]), "对比 [Image #2] 和这个 [Image #3]");
    }

    #[test]
    fn ignores_an_image_that_has_no_number_yet() {
        assert_eq!(with_image_placeholders("只有文字", &[image(None)]), "只有文字");
    }

    #[test]
    fn drops_the_trailing_run_this_messages_own_thumbnails_stand_for() {
        assert_eq!(text_without_appended_image_placeholders("看看这个 [Image #1]", &[image(Some(1))]), "看看这个");
        assert_eq!(text_without_appended_image_placeholders("[Image #1]", &[image(Some(1))]), "");
    }

    #[test]
    fn keeps_a_number_this_message_carries_no_thumbnail_for() {
        assert_eq!(text_without_appended_image_placeholders("再看 [Image #7]", &[image(Some(1))]), "再看 [Image #7]");
        assert_eq!(text_without_appended_image_placeholders("再看 [Image #7]", &[]), "再看 [Image #7]");
    }

    #[test]
    fn keeps_the_whole_message_when_the_user_was_citing_their_images_by_number() {
        let cited = "对比 [Image #1] 和 [Image #2]";
        assert_eq!(text_without_appended_image_placeholders(cited, &[image(Some(1)), image(Some(2))]), cited);
    }

    #[test]
    fn leaves_an_older_message_whose_numbers_lead_the_text_exactly_as_stored() {
        let legacy = "[Image #1]先保存这条图文消息";
        assert_eq!(text_without_appended_image_placeholders(legacy, &[image(Some(1))]), legacy);
    }

    #[test]
    fn round_trips_against_the_writer() {
        let images = [image(Some(1)), image(Some(2))];
        let stored = with_image_placeholders("看看这两张", &images);
        let shown = text_without_appended_image_placeholders(&stored, &images);
        assert_eq!(shown, "看看这两张");
        assert_eq!(with_image_placeholders(&shown, &images), stored);
    }

    #[test]
    fn title_fragment_drops_placeholders_and_collapses_whitespace() {
        assert_eq!(text_without_image_placeholders("[Image #1]保存失败也不能丢"), "保存失败也不能丢");
        assert_eq!(text_without_image_placeholders("a [Image #2] b"), "a b");
        assert_eq!(text_without_image_placeholders("[Image #1] [Image #2]"), "");
        assert_eq!(text_without_image_placeholders("plain text"), "plain text");
    }

    #[test]
    fn strip_removes_the_placeholder_and_the_space_the_append_added() {
        assert_eq!(strip_image_placeholder("hello [Image #2]", 2), "hello");
        assert_eq!(strip_image_placeholder("[Image #1] hello", 1), "hello");
        assert_eq!(strip_image_placeholder("a [Image #3] b", 3), "a b");
        assert_eq!(strip_image_placeholder("[Image #4]", 4), "");
    }

    #[test]
    fn strip_leaves_other_placeholders_and_surrounding_text_alone() {
        assert_eq!(strip_image_placeholder("a [Image #1] b [Image #2]", 2), "a [Image #1] b");
        assert_eq!(strip_image_placeholder("untouched", 5), "untouched");
    }

    #[test]
    fn strip_removes_every_occurrence_of_the_id() {
        assert_eq!(strip_image_placeholder("[Image #6] mid [Image #6]", 6), "mid");
    }

    #[test]
    fn renumber_keeps_non_colliding_numbers_untouched() {
        let (content, images) = renumber_images_for_send(HashSet::from([1, 2]), "a [Image #3]", &[image(Some(3))]);
        assert_eq!(content, "a [Image #3]");
        assert_eq!(images[0].short_id, Some(3));
    }

    #[test]
    fn renumber_renumbers_collisions_and_rewrites_their_placeholders() {
        let (content, images) = renumber_images_for_send(HashSet::from([1, 2, 3]), "see [Image #2]", &[image(Some(2))]);
        assert_eq!(content, "see [Image #4]");
        assert_eq!(images[0].short_id, Some(4));
    }

    #[test]
    fn renumber_reserves_keepers_before_choosing_fresh_numbers() {
        // A(#3) collides with the transcript, B(#5) does not. The fresh number
        // for A must skip B's 5; a single-pass counter would hand A the 5 and
        // then corrupt A's rewritten placeholder while renumbering B.
        let (content, images) = renumber_images_for_send(HashSet::from([1, 2, 3, 4]), "first [Image #3] second [Image #5]", &[image(Some(3)), image(Some(5))]);
        assert_eq!(content, "first [Image #6] second [Image #5]");
        assert_eq!(images.iter().map(|i| i.short_id).collect::<Vec<_>>(), [Some(6), Some(5)]);
    }

    #[test]
    fn renumber_assigns_fresh_numbers_to_unnumbered_images_without_touching_text() {
        let (content, images) = renumber_images_for_send(HashSet::from([2]), "plain", &[image(None)]);
        assert_eq!(content, "plain");
        assert_eq!(images[0].short_id, Some(3));
    }
}
