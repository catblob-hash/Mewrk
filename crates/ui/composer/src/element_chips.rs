//! The elements the user picked out of a page, waiting to be sent: upstream's `SelectedElementChips.tsx` and the composer half of
//! `lib/selectedElement.ts`.
//!
//! A chip stands for a block of prompt text the message will carry but the composer never shows: the label is what the user
//! pointed at, not what the model will read. The client keeps a pick as its block and its crop, so the chip reads its label back
//! out of the block, as restoring a sent message's chips does. Two picks of one element stay distinct by their place in the list.

use mewrk_client::model::ImageAttachment;
use mewrk_client::prelude::*;
use mewrk_client::runs::composer::{opening_attribute, opening_of};
use mewrk_client::runs::ElementPick;
use mewrk_ui_common::popover_menu::probe::say;

/// `selectedElementLabel` from a block: the React component when the page has one, else the tag and its first two classes. A
/// chip is a label, not the element's whole class attribute.
pub fn element_label(block: &str) -> String {
    if let Some(start) = block.find("<react component=\"") {
        let rest = &block[start + "<react component=\"".len()..];
        if let Some(end) = rest.find('"')
            && end > 0
        {
            return format!("<{} />", &rest[..end]);
        }
    }
    let opening = opening_of(block);
    let tag = opening_attribute(opening, "tag").unwrap_or_else(|| "element".to_owned());
    let classes = opening_attribute(opening, "class").unwrap_or_default().split(' ').filter(|class| !class.is_empty()).take(2).collect::<Vec<_>>().join(" ");
    if classes.is_empty() { format!("<{tag} />") } else { format!("<{tag} class=\"{classes}\" />") }
}

/// `selectedElementExcerpt` from a block: the first non-empty line of the element's own text.
pub fn element_excerpt(block: &str) -> String {
    let Some(start) = block.find("\n  <text>") else { return String::new() };
    let rest = &block[start + "\n  <text>".len()..];
    let text = &rest[..rest.find("</text>").unwrap_or(rest.len())];
    let first = text.split('\n').map(str::trim).find(|part| !part.is_empty()).unwrap_or_default();
    if first.chars().count() > 40 { format!("{}…", first.chars().take(40).collect::<String>()) } else { first.to_owned() }
}

/// `imagesWithoutElementCrops`: every draft image except the element crops, which a chip already stands for. The send gates still
/// count them: they are attachments on this message like any other, and hiding one from the strip must not hide it from the budget.
pub fn images_without_element_crops(images: &[ImageAttachment], picks: &[ElementPick]) -> Vec<ImageAttachment> {
    images.iter().filter(|image| !picks.iter().any(|pick| pick.screenshot_image_id.as_deref() == Some(image.id.as_str()))).cloned().collect()
}

pub fn element_remove_key(index: usize) -> u64 {
    hash_key(("element-chip-remove", index))
}

/// The chips, or nothing when no element is staged. The chip is the only handle a crop has, since the strip does not draw it, so
/// taking the chip off takes the crop off the message too.
pub fn selected_element_chips(conversation_id: &str, picks: &[ElementPick]) -> Option<E> {
    if picks.is_empty() {
        return None;
    }
    let t_ = theme();
    let mut chips: E = row().key("selected-element-chips").role(Role::List).aria_label(t("已选择的页面元素", "Selected page elements")).wrap().gap(4.0).px(2.0).pt(4.0).pb(8.0).min_w(0.0);
    for (index, pick) in picks.iter().enumerate() {
        let (label, excerpt) = (element_label(&pick.block), element_excerpt(&pick.block));
        say(&label);
        let remove_label = tf("移除 {name}", "Remove {name}", &[("name", &label)]);
        say(&remove_label);
        let mut chip: E = row().role(Role::ListItem).items_center().gap(5.0).max_w(420.0).min_w(0.0).pl(7.0).pr(2.0).py(2.0).radius(11.0).bg(t_.card_secondary).border(t_.card_stroke).child(text(label).ty(MONO_SMALL).color(t_.accent_text).ellipsis().min_w(0.0));
        if !excerpt.is_empty() {
            say(&excerpt);
            chip = chip.child(text(excerpt).ty(FINE).color(t_.text_secondary).ellipsis().min_w(0.0));
        }
        let conversation = conversation_id.to_owned();
        chips = chips.child(chip.child(row().key(element_remove_key(index)).center().size(18.0, 18.0).radius(9.0).shrink0().hover_bg(t_.subtle_hover).tooltip(remove_label).on_click(move |app: &mut App| remove_element_pick(app, &conversation, index)).child(icon("X", 11.0).color(t_.text_tertiary))));
    }
    Some(chips)
}

fn remove_element_pick(app: &mut App, conversation_id: &str, index: usize) {
    let mut picks = app.composer().element_picks.get(conversation_id).cloned().unwrap_or_default();
    if index >= picks.len() {
        return;
    }
    let removed = picks.remove(index);
    app.set_composer_element_picks(conversation_id, picks);
    if let Some(image_id) = removed.screenshot_image_id {
        app.remove_composer_image(conversation_id, &image_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<mewrk-selected-element>\n<element tag=\"button\" has-screenshot=\"true\" class=\"btn btn-primary btn-lg\">\n  <text>Place order\nsecond line</text>\n</element>\n(trailer)\n</mewrk-selected-element>";

    #[test]
    fn names_a_pick_by_what_the_user_pointed_at_not_by_what_the_model_will_read() {
        // Two classes at most: a chip is a label, not the element's whole class attribute.
        assert_eq!(element_label(BLOCK), "<button class=\"btn btn-primary\" />");
        assert_eq!(element_excerpt(BLOCK), "Place order");
        let react = "<element tag=\"div\" has-screenshot=\"false\">\n  <react component=\"SubmitButton\" />\n</element>";
        assert_eq!(element_label(react), "<SubmitButton />");
        assert_eq!(element_excerpt(react), "");
        assert_eq!(element_label("<element tag=\"a\" has-screenshot=\"false\">"), "<a />");
    }

    #[test]
    fn a_long_excerpt_is_cut_at_forty_characters() {
        let block = format!("<element tag=\"p\">\n  <text>{}</text>", "x".repeat(50));
        assert_eq!(element_excerpt(&block), format!("{}…", "x".repeat(40)));
    }

    #[test]
    fn restores_the_chips_of_a_sent_message_with_their_crops_in_pick_order() {
        let image = |id: &str, name: &str| ImageAttachment { id: id.into(), name: name.into(), mime: "image/png".into(), width: 1, height: 1, bytes: 1, short_id: None };
        let images = [image("other", "shot.png"), image("crop", "element-button.png")];
        let block = |tag: &str, shot: bool| format!("<mewrk-selected-element>\n<element tag=\"{tag}\" has-screenshot=\"{shot}\">\n</element>\n</mewrk-selected-element>");
        let text = format!("{}\n\n{}\n\nwhat I typed", block("a", false), block("button", true));
        let (written, picks) = mewrk_client::runs::composer::selected_elements_from_text(&text, &images);
        assert_eq!(written, "what I typed");
        assert_eq!(picks.len(), 2);
        assert_eq!(picks[0].block, block("a", false), "verbatim, so it sends the same words again");
        assert_eq!(picks[0].screenshot_image_id, None);
        assert_eq!(picks[1].screenshot_image_id.as_deref(), Some("crop"));
        // A block only counts leading the message.
        let (written, picks) = mewrk_client::runs::composer::selected_elements_from_text(&format!("hi {}", block("a", false)), &images);
        assert!(picks.is_empty());
        assert!(written.starts_with("hi "));
    }

    #[test]
    fn a_crop_a_chip_stands_for_is_not_drawn_in_the_strip() {
        let image = |id: &str| ImageAttachment { id: id.into(), name: "n".into(), mime: "image/png".into(), width: 1, height: 1, bytes: 1, short_id: None };
        let picks = vec![ElementPick { block: BLOCK.into(), screenshot_image_id: Some("crop".into()) }];
        let shown = images_without_element_crops(&[image("crop"), image("other")], &picks);
        assert_eq!(shown.iter().map(|image| image.id.as_str()).collect::<Vec<_>>(), ["other"]);
    }
}
