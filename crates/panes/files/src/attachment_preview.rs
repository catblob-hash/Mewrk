//! One attached file, opened (upstream's `FileAttachmentPreview.tsx`): a PDF laid out, a document rendered, a table
//! tabulated, and anything else as its own numbered, coloured text. The Files pane's readers, fed from the attachment store
//! instead of the workspace. And the chip's parts of `ImageStrip` that are about files.

use crate::attachments::decode_attachment_text;
use crate::code::numbered_code;
use crate::format::*;
use crate::icons::file_icon;
use crate::links::external_http_url;
use crate::markdown::{document_view_as, fenced, DocContext};
use crate::previews::*;
use crate::viewer::{html_preview, pdf_viewer};
use crate::viewers::*;
use crate::widgets::tool;
use mewrk_client::model::{FileAttachment, FileAttachmentFormat};
use mewrk_client::prelude::*;

/// Models that read a PDF as the document itself, within this many pages. `MAX_NATIVE_PDF_PAGES` upstream.
pub const MAX_NATIVE_PDF_PAGES: u32 = 10;

pub fn format_token_estimate(tokens: u64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1000.0;
    if thousands >= 100.0 { format!("{}k", thousands.round() as u64) } else { format!("{thousands:.1}k") }
}

pub fn attachment_details(file: &FileAttachment) -> String {
    let mut parts = Vec::new();
    if file.format == FileAttachmentFormat::Pdf {
        parts.push(tf("PDF · {pages} 页", "PDF · {pages} pages", &[("pages", &file.pages.unwrap_or(0))]));
    }
    parts.push(format_bytes(file.bytes));
    parts.push(tf("约 {tokens} tokens", "≈{tokens} tokens", &[("tokens", &format_token_estimate(file.tokens))]));
    parts.join(" · ")
}

/// What the model will read of a PDF, by whether it reads one as a document and how long this one is.
pub fn pdf_reading_hint(reads_documents: bool, pages: u32) -> &'static str {
    if reads_documents && pages <= MAX_NATIVE_PDF_PAGES {
        t("模型直接读取这份 PDF", "The model reads the PDF itself")
    } else {
        t("模型读取的是上传时提取出的文字", "The model reads the text extracted when it was uploaded")
    }
}

#[derive(Clone)]
enum Loaded {
    Loading,
    Ready(Arc<str>),
    Error(String),
}

/// Only pictures the file carries in itself; anything it would fetch stays out.
fn inline_images_only(source: &str) -> Option<Arc<str>> {
    source.starts_with("data:image/").then(|| Arc::from(source))
}

/// Links inside an attachment: the open web goes to the system browser, a fragment scrolls, and anything else (a path into
/// a workspace the file never lived in) goes nowhere.
fn attachment_link(app: &mut App, href: &str) {
    if external_http_url(href).is_some() {
        app.invoke_logged("open_external_url", json!({ "url": href.trim() }));
    }
}

/// `DocsLink inline`.
fn docs_link(page: &'static str, section: &'static str) -> E {
    let t_ = theme();
    let locale = if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" };
    let href = format!("https://mewrk.dev/{locale}/{page}.html#{section}");
    row().key(("attachment-docs", page, section)).items_center().gap(2.0).shrink0().cursor(CursorKind::Hand).role(Role::Link).on_click(move |app: &mut App| app.invoke_logged("open_external_url", json!({ "url": href }))).child(text(t("详情", "Learn more")).ty(FINE).color(t_.accent_text).nowrap()).child(icon("ExternalLink", 9.0).color(t_.accent_text))
}

/// The dialog. `reads_documents` is upstream's `PdfReadingContext`: whether the conversation's model reads a PDF as the
/// document itself.
pub fn file_attachment_preview(app: &mut App, file: &FileAttachment, reads_documents: bool, on_close: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let key = format!("attachment/{}", file.id);
    let attempt = use_state(("attachment-attempt", key.as_str()), || 0u32);
    let loaded = use_state(("attachment-data", key.as_str(), attempt.get()), || Loaded::Loading);
    let show_source = use_state(("attachment-source", key.as_str()), || false);
    let started = use_state(("attachment-started", key.as_str(), attempt.get()), || false);
    if !started.get() {
        started.set(true);
        let slot: std::sync::Arc<std::sync::Mutex<Option<Result<String, String>>>> = Default::default();
        let writer = slot.clone();
        let target = loaded.clone();
        let args = crate::attachments::file_attachment_args(&file.id);
        app.spawn(move |task| {
            let answer = task.invoke::<String>("file_attachment_data", args);
            *writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(answer);
            task.post(deliver_attachment_data);
        });
        WAITING.with(|w| w.borrow_mut().push((target, slot)));
    }
    let close = on_close.clone();
    let switchable = file.format == FileAttachmentFormat::Text && has_source_form(&file.name);
    let on_source = show_source.get();
    let toggle = show_source.clone();
    let mut details = row().flex1().min_w(0.0).items_baseline().gap(10.0).clip().child(text(attachment_details(file)).ty(FINE).color(t_.text_secondary).nowrap().shrink0());
    if file.format == FileAttachmentFormat::Pdf {
        let pages = file.pages.unwrap_or(0);
        let mut hint = row().min_w(0.0).items_center().gap(4.0).child(text(pdf_reading_hint(reads_documents, pages)).ty(FINE).color(t_.text_tertiary).ellipsis());
        if reads_documents && pages <= MAX_NATIVE_PDF_PAGES {
            hint = hint.child(docs_link("context", "attachments"));
        }
        details = details.child(hint);
    }
    let mut bar = row().items_center().gap(8.0).min_h(32.0).pl(20.0).pr(12.0).shrink0().border_bottom(t_.divider).child(details);
    if switchable {
        bar = bar.child(tool(("attachment-toggle", key.as_str()), if on_source { "Eye" } else { "Code2" }, if on_source { t("显示渲染结果", "Show rendered") } else { t("显示源文本", "Show source") }, 24.0, 14.0).on_click(move |_: &mut App| toggle.update(|s| *s = !*s)));
    }
    let body: E = match loaded.get() {
        Loaded::Loading => notice(t("正在读取…", "Loading…")).px(20.0).py(12.0),
        Loaded::Error(message) => {
            let retry = attempt.clone();
            row().role(Role::Alert).items_center().gap(10.0).px(20.0).py(12.0).child(text(tf("无法读取这个附件：{reason}", "Could not read this attachment: {reason}", &[("reason", &message)])).ty(FINE).color(t_.critical).min_w(0.0)).child(row().key(("attachment-retry", key.as_str())).items_center().gap(4.0).shrink0().px(9.0).py(3.0).radius(6.0).bg(t_.control_fill).border(t_.control_stroke).hover_bg(t_.control_fill_hover).on_click(move |_: &mut App| retry.update(|a| *a += 1)).child(icon("RefreshCw", 13.0).color(t_.text_primary)).child(text(t("重试", "Retry")).ty(FINE).color(t_.text_primary).nowrap()))
        }
        Loaded::Ready(source) => preview_body(app, &key, file, &source, on_source),
    };
    let head = row().items_center().gap(8.0).pl(20.0).pr(12.0).py(10.0).shrink0().child(text(file.name.clone()).ty(BODY_STRONG).color(t_.text_primary).ellipsis().flex1().role(Role::Heading).aria_level(2)).child(tool(("attachment-close", key.as_str()), "X", t("关闭", "Close"), 24.0, 14.0).on_click(move |a: &mut App| close(a)));
    let backdrop = on_close.clone();
    // A reading window, so it takes its size from the screen rather than from what it holds: `min(880px, 100vh - 48px)`.
    dialog(1080.0, col().h(880.0f32.min(window_size().1 - 48.0)).min_h(0.0).child(head).child(bar).child(col().flex1().min_h(0.0).min_w(0.0).child(body)), Some(Rc::new(move |a: &mut App| backdrop(a)))).aria_label(file.name.as_str())
}

type AttachmentSlot = std::sync::Arc<std::sync::Mutex<Option<Result<String, String>>>>;

thread_local! {
    static WAITING: std::cell::RefCell<Vec<(Local<Loaded>, AttachmentSlot)>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn deliver_attachment_data(_: &mut App) {
    WAITING.with(|w| {
        w.borrow_mut().retain(|(target, slot)| {
            let Some(answer) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() else { return true };
            target.set(match answer {
                Ok(source) => Loaded::Ready(Arc::from(source)),
                Err(message) => Loaded::Error(message),
            });
            false
        });
    });
    repaint();
}

fn preview_body(app: &mut App, key: &str, file: &FileAttachment, source: &Arc<str>, show_source: bool) -> E {
    if file.format == FileAttachmentFormat::Pdf {
        return pdf_viewer(app, key, source, Some(file.bytes), &file.name, None, None);
    }
    let text_value = data_url_bytes(source).and_then(|b| decode_attachment_text(&b)).unwrap_or_default();
    if text_value.is_empty() {
        return notice(t("这个文件是空的。", "This file is empty."));
    }
    let ctx = DocContext {
        key: format!("doc/{key}"),
        scroller: format!("doc-scroll/{key}"),
        base_dir: None,
        machine: None,
        linkify_paths: false,
        resolve_image: Rc::new(inline_images_only),
        on_link: Rc::new(attachment_link),
        on_path: Rc::new(|_, _| {}),
    };
    if !show_source {
        match file_viewer_kind(&file.name) {
            FileViewerKind::Markdown => {
                let front = split_front_matter(&text_value);
                let body = front.as_ref().map_or(text_value.as_str(), |f| f.body.as_str());
                // `.attachment-preview__document`: 13 px at 1.65.
                let ty = TypeStyle { size: 13.0, line: 21.5, weight: Weight::Regular, mono: false, italic: false, serif: false };
                let mut scroller = col().key(ctx.scroller.as_str()).flex1().min_h(0.0).min_w(0.0).scroll_y().px(24.0).pt(14.0).pb(32.0).gap(8.0);
                if let Some(f) = &front {
                    scroller = scroller.child(document_view_as(app, &ctx, "front", &fenced(&f.source, f.language), ty));
                }
                return scroller.child(document_view_as(app, &ctx, "body", body, ty));
            }
            FileViewerKind::Html => {
                let on_link: Arc<dyn Fn(&mut App, &str) + Send + Sync> = Arc::new(|app, href| attachment_link(app, href));
                return html_preview(app, key, &file.name, &text_value, None, String::new(), on_link, &ctx);
            }
            FileViewerKind::Csv => return csv_table(key, &file.name, &text_value),
            FileViewerKind::Notebook => return notebook_view(app, key, &text_value, &ctx),
            FileViewerKind::Image => {
                // A text file that is a picture: SVG.
                let svg: Arc<str> = Arc::from(format!("data:image/svg+xml;base64,{}", source.split(',').nth(1).unwrap_or("")));
                return image_viewer(app, key, &svg, &file.name, Some(file.bytes), None);
            }
            _ => {}
        }
    }
    let content: Rc<str> = Rc::from(text_value);
    numbered_code(key.to_owned(), &content, code_language(&file.name), &file.name, None, None, None)
}

/// A file's chip in a message's strip: its icon, name and size; a click opens the preview, a cross takes it off. The image
/// half of `ImageStrip` belongs to whoever shows images.
pub fn file_chip(file: &FileAttachment, on_open: Rc<dyn Fn(&mut App)>, on_remove: Option<Rc<dyn Fn(&mut App)>>) -> E {
    let t_ = theme();
    let mut chip = row().key(("file-chip", file.id.as_str())).items_center().gap(6.0).h(32.0).pl(8.0).pr(4.0).radius(6.0).bg(t_.control_fill).border(t_.control_stroke).hover_bg(t_.control_fill_hover).max_w(240.0).tooltip(attachment_details(file)).role(Role::Button).on_click(move |a: &mut App| on_open(a)).child(file_icon(&file.name, 14.0)).child(text(file.name.clone()).ty(CAPTION).color(t_.text_primary).ellipsis().flex1()).child(text(format_bytes(file.bytes)).ty(FINE).color(t_.text_tertiary).nowrap());
    if let Some(remove) = on_remove {
        chip = chip.child(tool(("file-chip-remove", file.id.as_str()), "X", tf("移除 {name}", "Remove {name}", &[("name", &file.name)]), 18.0, 12.0).on_click(move |a: &mut App| remove(a)));
    }
    chip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_what_the_model_reads_of_a_pdf() {
        mewrk_client::i18n::set_english(true);
        assert_eq!(pdf_reading_hint(true, 3), "The model reads the PDF itself");
        assert_eq!(pdf_reading_hint(true, 30), "The model reads the text extracted when it was uploaded");
        assert_eq!(pdf_reading_hint(false, 3), "The model reads the text extracted when it was uploaded");
    }

    #[test]
    fn estimates_tokens_in_thousands() {
        assert_eq!(format_token_estimate(999), "999");
        assert_eq!(format_token_estimate(1500), "1.5k");
        assert_eq!(format_token_estimate(150_000), "150k");
    }
}
