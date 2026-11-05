//! The messages waiting for the run to finish: upstream's `QueuedMessageList.tsx`.

use mewrk_client::model::QueuedMessage;
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::probe::say;

/// What one row says, worked out from the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedRowCopy {
    /// The first thing the row shows: the text, or the first attachment's name.
    pub visible: String,
    pub image_count_summary: Option<String>,
    pub file_count_summary: Option<String>,
    /// What the actions' names and the row's title call it.
    pub action_summary: String,
}

pub fn queued_row_copy(message: &QueuedMessage) -> QueuedRowCopy {
    let content: String = message.content.split_whitespace().collect::<Vec<_>>().join(" ");
    let image_count = message.images.len();
    let first_image = message.images.first().map_or_else(|| t("图片", "Image").to_owned(), |image| image.name.clone());
    let image_identity = if image_count > 1 { tf("{name} + 另外 {count} 张", "{name} + {count} more", &[("name", &first_image), ("count", &(image_count - 1))]) } else { first_image };
    let file_count = message.files.len();
    let first_file = message.files.first().map_or_else(String::new, |file| file.name.clone());
    let file_identity = if file_count > 1 { tf("{name} + 另外 {count} 个", "{name} + {count} more", &[("name", &first_file), ("count", &(file_count - 1))]) } else { first_file };
    let visible = if !content.is_empty() {
        content.clone()
    } else if image_count > 0 {
        image_identity.clone()
    } else if !file_identity.is_empty() {
        file_identity.clone()
    } else {
        image_identity.clone()
    };
    let shortened: String = content.chars().take(40).collect();
    let mut parts: Vec<&str> = Vec::new();
    if !shortened.is_empty() {
        parts.push(&shortened);
    }
    if image_count > 0 {
        parts.push(&image_identity);
    }
    if file_count > 0 {
        parts.push(&file_identity);
    }
    QueuedRowCopy {
        action_summary: parts.join(" · "),
        visible,
        image_count_summary: (image_count > 0).then(|| tf("{count} 张图片", "{count} images", &[("count", &image_count)])),
        file_count_summary: (file_count > 0).then(|| tf("{count} 个文件", "{count} files", &[("count", &file_count)])),
    }
}

pub fn steer_key(message_id: &str) -> u64 {
    hash_key(("queued-steer", message_id))
}

pub fn delete_key(message_id: &str) -> u64 {
    hash_key(("queued-delete", message_id))
}

pub fn retry_key(message_id: &str) -> u64 {
    hash_key(("queued-retry", message_id))
}

fn action(key: u64, name: &'static str, size: f32, label: String, tip: &'static str, enabled: bool, color: Color, on_click: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    say(&label);
    let button: E = row().key(key).center().size(24.0, 24.0).radius(12.0).shrink0().aria_label(label).tooltip(tip).child(icon(name, size).color(if enabled { color } else { t_.text_disabled }));
    if enabled { button.hover_bg(t_.subtle_hover).press_bg(t_.subtle_pressed).on_click(on_click) } else { button.disabled(true).on_click(|_: &mut App| {}) }
}

/// The list, or nothing when nothing is queued. `workspace_id` is empty for a conversation not yet in a workspace.
pub fn queued_message_list(app: &mut App, workspace_id: &str, conversation_id: &str, messages: &[QueuedMessage]) -> Option<E> {
    if messages.is_empty() {
        return None;
    }
    let t_ = theme();
    let run = app.model_run_summaries().into_iter().find(|(id, _)| id == conversation_id).map(|(_, summary)| summary);
    let (steering, failed) = {
        let composer = app.composer();
        (composer.steering_message_ids.clone(), composer.failed_queued_promotion_ids.clone())
    };
    let count = messages.len();
    let mut rows: E = col().role(Role::List);
    for (index, message) in messages.iter().enumerate() {
        let copy = queued_row_copy(message);
        let is_steering = steering.contains(&message.id);
        let promotion_failed = failed.contains(&message.id);
        let can_steer = run.as_ref().is_some_and(|run| message.images.is_empty() || run.supports_vision);
        say(&copy.visible);
        let mut line = copy.visible.clone();
        for summary in [&copy.image_count_summary, &copy.file_count_summary].into_iter().flatten() {
            say(summary);
            line.push_str(" · ");
            line.push_str(summary);
        }
        let leading = if !message.images.is_empty() {
            Some("Images")
        } else if !message.files.is_empty() {
            Some("Paperclip")
        } else {
            None
        };
        let summary: E = row().flex1().min_w(0.0).items_center().gap(5.0).tooltip(copy.action_summary.clone()).child(leading.map(|name| icon(name, 13.0).color(t_.text_secondary))).child(text(line).ty(FINE).color(t_.text_secondary).ellipsis().flex1());
        let mut entry: E = row().key(("queued-row", message.id.as_str())).role(Role::ListItem).min_h(28.0).items_center().gap(8.0).child(summary);
        if index + 1 < count {
            entry = entry.border_bottom(t_.divider);
        }
        if promotion_failed {
            let (workspace, conversation, id) = (workspace_id.to_owned(), conversation_id.to_owned(), message.id.clone());
            entry = entry.child(action(retry_key(&message.id), "RotateCcw", 13.0, tf("重试发送排队消息“{message}”", "Retry queued message “{message}”", &[("message", &copy.action_summary)]), t("重试发送", "Retry sending"), !is_steering, t_.caution, move |app: &mut App| app.retry_failed_queued_promotion(&workspace, &conversation, &id)));
        }
        let (conversation, id) = (conversation_id.to_owned(), message.id.clone());
        entry = entry.child(action(steer_key(&message.id), "CornerUpLeft", 14.0, tf("将“{message}”引导到当前回合", "Steer “{message}” into the current turn", &[("message", &copy.action_summary)]), t("引导到当前回合", "Steer into current turn"), can_steer && !is_steering, t_.text_tertiary, move |app: &mut App| app.steer_queued_message(&conversation, &id, |_, _| {})));
        let (workspace, conversation, id) = (workspace_id.to_owned(), conversation_id.to_owned(), message.id.clone());
        entry = entry.child(action(delete_key(&message.id), "Trash2", 13.0, tf("删除排队消息“{message}”", "Delete queued message “{message}”", &[("message", &copy.action_summary)]), t("删除排队消息", "Delete queued message"), !is_steering, t_.text_tertiary, move |app: &mut App| app.delete_queued_message(&workspace, &conversation, &id)));
        rows = rows.child(entry);
    }
    Some(div().key("queued-messages").role(Role::Region).aria_label(t("排队消息", "Queued messages")).w_full().max_w(820.0).mx_auto().mb(5.0).px(11.0).py(3.0).child(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mewrk_client::model::{FileAttachment, FileAttachmentFormat, ImageAttachment};

    fn image(name: &str) -> ImageAttachment {
        ImageAttachment { id: name.into(), name: name.into(), mime: "image/png".into(), width: 1, height: 1, bytes: 1, short_id: None }
    }

    fn message(content: &str, images: Vec<ImageAttachment>, files: Vec<FileAttachment>) -> QueuedMessage {
        QueuedMessage { id: "q".into(), content: content.into(), images, files, created_at: String::new() }
    }

    #[test]
    fn identifies_pure_image_rows_by_first_filename_and_remaining_count() {
        let _language = crate::tests::language(true);
        let copy = queued_row_copy(&message("", vec![image("first.png"), image("b.png"), image("c.png")], vec![]));
        assert_eq!(copy.visible, "first.png + 2 more");
        assert_eq!(copy.image_count_summary.as_deref(), Some("3 images"));
        assert_eq!(copy.action_summary, "first.png + 2 more");
        let one = queued_row_copy(&message("", vec![image("only.png")], vec![]));
        assert_eq!(one.visible, "only.png");
    }

    #[test]
    fn collapses_white_space_and_names_text_with_its_attachments() {
        let _language = crate::tests::language(true);
        let file = FileAttachment { id: "f".into(), name: "notes.md".into(), format: FileAttachmentFormat::Text, bytes: 1, tokens: 1, pages: None };
        let copy = queued_row_copy(&message("  look \n at   this ", vec![image("a.png")], vec![file]));
        assert_eq!(copy.visible, "look at this");
        assert_eq!(copy.action_summary, "look at this · a.png · notes.md");
        assert_eq!(copy.file_count_summary.as_deref(), Some("1 files"));
    }
}
