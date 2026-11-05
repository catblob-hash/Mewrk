//! Upstream's `AttachmentFeedback.tsx`: what a drop here would do, and what the last attempt to attach left out.

use crate::attach::*;
use mewrk_client::prelude::*;
use mewrk_client::runs::attachments::{MAX_FILE_ATTACHMENT_PDF_BYTES, MAX_IMAGE_ATTACHMENT_BYTES, MAX_MESSAGE_ATTACHMENT_BYTES, MAX_TEXT_FILE_TOKENS, MAX_TEXT_FILE_UPLOAD_BYTES, TRUNCATED_TEXT_FILE_LINES};
use mewrk_client::runs::{AttachmentRejection, AttachmentRejectionReason};
use mewrk_ui_common::popover_menu::probe::say;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * 1024;

fn drag_rejection_label(refusal: DragRefusal, count: usize) -> String {
    match refusal {
        DragRefusal::Verdict(DragVerdict::Directory) => tf("{count} 个文件夹", "{count} folder(s)", &[("count", &count)]),
        DragRefusal::Verdict(DragVerdict::TooLarge) => tf("{count} 个过大的文件", "{count} file(s) too large", &[("count", &count)]),
        DragRefusal::Verdict(DragVerdict::Empty) => tf("{count} 个空文件", "{count} empty file(s)", &[("count", &count)]),
        DragRefusal::ImageInputUnavailable => tf("{count} 张图片（当前模型不支持图片）", "{count} image(s) (the model takes none)", &[("count", &count)]),
        DragRefusal::Verdict(_) => tf("{count} 个不支持的文件", "{count} unsupported file(s)", &[("count", &count)]),
    }
}

/// What the overlay says: its icon, its headline, a second line, and whether it refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropOverlayCopy {
    pub icon: &'static str,
    pub title: String,
    pub detail: Option<String>,
    pub unavailable: bool,
}

/// A drag of nothing this message can carry (a folder, an archive, an image for a model without eyes) reads as unavailable
/// before it is dropped, so the gesture is never completed only to be refused. `None` while nothing hovers this zone.
pub fn drop_overlay_copy(state: &DropZoneState, image_input: bool) -> Option<DropOverlayCopy> {
    if !state.over {
        return None;
    }
    let Some(items) = &state.items else {
        return Some(DropOverlayCopy { icon: "LoaderCircle", title: t("正在检查文件…", "Checking files…").to_owned(), detail: None, unavailable: false });
    };
    let summary = summarize_drag(items, image_input);
    let takeable = summary.accepted + summary.undecided;
    let ignored = summary.rejected.iter().map(|(refusal, count)| drag_rejection_label(*refusal, *count)).collect::<Vec<_>>().join(t("、", ", "));
    if takeable == 0 {
        let only_folders = summary.rejected.len() == 1 && summary.rejected[0].0 == DragRefusal::Verdict(DragVerdict::Directory);
        return Some(DropOverlayCopy {
            icon: "Ban",
            title: if only_folders { t("不能添加文件夹", "Folders can't be attached") } else { t("无法添加这些内容", "These can't be attached") }.to_owned(),
            detail: Some(if only_folders { t("请拖入文件夹里的文件", "Drag the files inside it instead").to_owned() } else { format!("{ignored}{}", t("。可添加图片、PDF 和文本文件", ". Images, PDFs and text files can be attached")) }),
            unavailable: true,
        });
    }
    Some(DropOverlayCopy {
        icon: "FilePlus2",
        title: tf("松开以添加 {count} 个文件", "Drop to attach {count} file(s)", &[("count", &takeable)]),
        detail: (!ignored.is_empty()).then(|| tf("将忽略 {items}", "Will skip {items}", &[("items", &ignored)])),
        unavailable: false,
    })
}

/// `AttachmentDropOverlay`.
pub fn attachment_drop_overlay(state: &DropZoneState, image_input: bool) -> Option<E> {
    let t_ = theme();
    let copy = drop_overlay_copy(state, image_input)?;
    say(&copy.title);
    let tone = if copy.unavailable { t_.critical } else { t_.accent };
    let fg = if copy.unavailable { t_.critical } else { t_.text_primary };
    let mut overlay: E = col()
        .key("attachment-drop")
        .role(Role::Status)
        .absolute()
        .inset(0.0)
        .raised()
        .center()
        .gap(4.0)
        .px(16.0)
        .py(10.0)
        .radius(8.0)
        .bg(if copy.unavailable { t_.critical_bg } else { t_.flyout })
        .border(tone)
        .pass_through()
        .child(if copy.icon == "LoaderCircle" { crate::image_strip::spinner(18.0) } else { icon(copy.icon, 18.0).color(fg) })
        .child(text(copy.title).ty(CAPTION_STRONG).color(fg).text_center());
    if let Some(detail) = copy.detail {
        say(&detail);
        overlay = overlay.child(text(detail).ty(FINE).color(t_.text_secondary).text_center());
    }
    Some(overlay)
}

pub fn rejection_reason_label(reason: AttachmentRejectionReason) -> String {
    use AttachmentRejectionReason as R;
    match reason {
        R::Directory => t("不能添加文件夹", "Folders can't be attached").to_owned(),
        R::Empty => t("文件是空的", "The file is empty").to_owned(),
        R::Unsupported => t("不支持的格式；可添加图片、PDF 和文本文件", "Unsupported format; images, PDFs and text files can be attached").to_owned(),
        R::TooLarge => tf(
            "文件过大（文本不超过 {text} KB，PDF 不超过 {pdf} MB，图片不超过 {image} MB）",
            "Too large (text up to {text} KB, PDFs up to {pdf} MB, images up to {image} MB)",
            &[("text", &(MAX_TEXT_FILE_UPLOAD_BYTES / KIB)), ("pdf", &(MAX_FILE_ATTACHMENT_PDF_BYTES / MIB)), ("image", &(MAX_IMAGE_ATTACHMENT_BYTES / MIB))],
        ),
        R::TooLong => tf("内容太长：前 {lines} 行仍超过 {tokens} tokens", "Too long: even its first {lines} lines are over {tokens} tokens", &[("lines", &TRUNCATED_TEXT_FILE_LINES), ("tokens", &MAX_TEXT_FILE_TOKENS)]),
        R::ImageInputUnavailable => t("当前模型不支持图片输入", "The current model has no image input").to_owned(),
        R::ImageRejected => t("图片的格式或尺寸超出限制", "The image format or size is over the limit").to_owned(),
        R::PdfWithoutText => t("PDF 里没有可读取的文字（可能是扫描件）", "The PDF has no readable text (it may be a scan)").to_owned(),
        R::PdfPassword => t("PDF 有密码保护", "The PDF is password-protected").to_owned(),
        R::PdfUnreadable => t("无法读取这个 PDF", "The PDF could not be read").to_owned(),
        R::MessageTooLarge => tf("一条消息的附件合计不能超过 {size} MB", "A message's attachments can come to at most {size} MB in all", &[("size", &(MAX_MESSAGE_ATTACHMENT_BYTES / MIB))]),
        R::Failed => t("读取或上传失败", "Reading or uploading failed").to_owned(),
    }
}

/// `AttachmentNotice`: what the last attempt to attach left out, and why, until dismissed or superseded.
pub fn attachment_notice(rejected: &[AttachmentRejection], on_dismiss: impl Fn(&mut App) + 'static) -> Option<E> {
    if rejected.is_empty() {
        return None;
    }
    let t_ = theme();
    let mut groups: Vec<(AttachmentRejectionReason, Vec<String>)> = Vec::new();
    for entry in rejected {
        let at = groups.iter().position(|(reason, _)| *reason == entry.reason).unwrap_or_else(|| {
            groups.push((entry.reason, Vec::new()));
            groups.len() - 1
        });
        if let Some(name) = &entry.name {
            groups[at].1.push(name.clone());
        }
    }
    let title = tf("{count} 项没有添加", "{count} item(s) not attached", &[("count", &rejected.len())]);
    say(&title);
    let mut body: E = col().flex1().min_w(0.0).child(text(title).ty(FINE_STRONG).color(t_.text_primary));
    for (reason, names) in groups {
        let label = rejection_reason_label(reason);
        say(&label);
        let mut line: E = row().wrap().gap_x(6.0).mt(2.0);
        if !names.is_empty() {
            let joined = format!("{}:", names.join(t("、", ", ")));
            say(&joined);
            line = line.child(text(joined).ty(FINE).color(t_.text_primary).ellipsis().min_w(0.0));
        }
        body = body.child(line.child(text(label).ty(FINE).color(t_.text_secondary)));
    }
    Some(
        row()
            .key("attachment-notice")
            .role(Role::Alert)
            .items_start()
            .gap(7.0)
            .mb(8.0)
            .pl(9.0)
            .pr(6.0)
            .py(7.0)
            .radius(8.0)
            .bg(t_.caution_bg)
            .border(t_.caution.fade(0.4))
            .child(icon("TriangleAlert", 14.0).color(t_.caution).mt(1.0))
            .child(body)
            .child(icon_button("X", t("关闭提示", "Dismiss")).key("attachment-notice-dismiss").size(20.0, 20.0).on_click(on_dismiss)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(items: Option<Vec<DragItem>>) -> DropZoneState {
        DropZoneState { dragging: true, over: true, items }
    }

    fn item(verdict: DragVerdict) -> DragItem {
        DragItem { name: "f".into(), verdict, path: None }
    }

    #[test]
    fn says_what_a_drop_would_do_before_it_is_dropped() {
        let _language = crate::tests::language(true);
        assert!(drop_overlay_copy(&DropZoneState { dragging: true, over: false, items: None }, true).is_none(), "a drag elsewhere over the window");
        assert_eq!(drop_overlay_copy(&state(None), true).unwrap().title, "Checking files…");
        let taking = drop_overlay_copy(&state(Some(vec![item(DragVerdict::Text), item(DragVerdict::Directory)])), true).unwrap();
        assert_eq!(taking.title, "Drop to attach 1 file(s)");
        assert_eq!(taking.detail.as_deref(), Some("Will skip 1 folder(s)"));
        assert!(!taking.unavailable);
        let folder = drop_overlay_copy(&state(Some(vec![item(DragVerdict::Directory)])), true).unwrap();
        assert!(folder.unavailable);
        assert_eq!(folder.title, "Folders can't be attached");
        let blind = drop_overlay_copy(&state(Some(vec![item(DragVerdict::Image)])), false).unwrap();
        assert!(blind.unavailable);
        assert_eq!(blind.detail.as_deref(), Some("1 image(s) (the model takes none). Images, PDFs and text files can be attached"));
    }
}
