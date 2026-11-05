//! Everything attached to a message: its images, then its files. Upstream's `ImageStrip.tsx`.
//!
//! Thumbnails come from the host and live in the shared memory pool as high-priority data, the full pictures as low-priority,
//! and a `data:` URL becomes a texture through the files pane's picture cache, so a picture shown twice is one texture.

use fluent::ImageId;
use mewrk_client::memory_pool::MemoryPoolKind;
use mewrk_client::model::{FileAttachment, FileAttachmentFormat, ImageAttachment};
use mewrk_client::prelude::*;
use mewrk_pane_files::attachment_preview::file_attachment_preview;
use mewrk_pane_files::format::format_bytes;
use mewrk_pane_files::icons::file_icon;
use mewrk_pane_files::images::{picture_for, Picture};
use mewrk_ui_common::popover_menu::probe::say;
use std::collections::{HashMap, HashSet};

fn image_data_key(image: &ImageAttachment) -> String {
    format!("{}:{}", image.id, image.mime)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Thumb {
    #[default]
    Loading,
    Ready(Arc<str>),
    Failed,
}

/// What each picture's chip is showing. Lives with the app, as the pool it reads from does.
#[derive(Default)]
struct Thumbs {
    states: HashMap<String, Thumb>,
    full: HashMap<String, Arc<str>>,
    full_asked: HashSet<String>,
    /// Asked for again after failing, so that it arriving is news.
    recovering: HashSet<String>,
}

fn thumb_of(app: &mut App, image: &ImageAttachment) -> Thumb {
    let key = image_data_key(image);
    if let Some(known) = app.local::<Thumbs>().states.get(&key) {
        return known.clone();
    }
    app.local::<Thumbs>().states.insert(key.clone(), Thumb::Loading);
    let image_id = image.id.clone();
    let writing = key.clone();
    let name = image.name.clone();
    app.pooled_fetch(MemoryPoolKind::ImageThumbnail, &key, move |task| task.invoke::<String>("image_attachment_thumbnail", json!({ "imageId": image_id })), move |app, result| {
        let next = match result {
            Ok(source) => Thumb::Ready(Arc::from(source.as_str())),
            Err(_) => {
                app.local::<Thumbs>().recovering.remove(&writing);
                announce(tf("{name} 加载失败", "{name} failed to load", &[("name", &name)]));
                Thumb::Failed
            }
        };
        app.local::<Thumbs>().states.insert(writing, next);
        repaint();
    });
    Thumb::Loading
}

/// `forgetImageData`, and the chip starts over.
fn retry_thumb(app: &mut App, image: &ImageAttachment) {
    let key = image_data_key(image);
    app.memory_pool().delete(MemoryPoolKind::ImageThumbnail, &key);
    app.memory_pool().delete(MemoryPoolKind::ImageData, &key);
    let thumbs = app.local::<Thumbs>();
    thumbs.states.remove(&key);
    thumbs.full.remove(&key);
    thumbs.full_asked.remove(&key);
    thumbs.recovering.insert(key);
    repaint();
}

/// The full picture for the viewer, once it arrives. The chip's thumbnail stands in until then.
fn full_of(app: &mut App, image: &ImageAttachment) -> Option<Arc<str>> {
    let key = image_data_key(image);
    let thumbs = app.local::<Thumbs>();
    if let Some(known) = thumbs.full.get(&key) {
        return Some(known.clone());
    }
    if thumbs.full_asked.insert(key.clone()) {
        let image_id = image.id.clone();
        let writing = key.clone();
        app.pooled_fetch(MemoryPoolKind::ImageData, &key, move |task| task.invoke::<String>("image_attachment_data", json!({ "imageId": image_id })), move |app, result| {
            if let Ok(source) = result {
                app.local::<Thumbs>().full.insert(writing, Arc::from(source.as_str()));
                repaint();
            }
        });
    }
    None
}

/// `key` of a picture's chip, for whatever needs to find it.
pub fn image_key(strip: &str, image_id: &str) -> u64 {
    hash_key(("image-strip-item", strip, image_id))
}

pub fn image_remove_key(strip: &str, image_id: &str) -> u64 {
    hash_key(("image-strip-remove", strip, image_id))
}

pub fn file_key(strip: &str, file_id: &str) -> u64 {
    hash_key(("image-strip-file", strip, file_id))
}

pub fn file_remove_key(strip: &str, file_id: &str) -> u64 {
    hash_key(("image-strip-file-remove", strip, file_id))
}

fn dimensions_of(image: &ImageAttachment) -> String {
    if image.width > 0 && image.height > 0 { format!("{} × {}", image.width, image.height) } else { String::new() }
}

fn remove_button(key: u64, label: String, on_remove: Rc<dyn Fn(&mut App)>) -> E {
    say(&label);
    row().key(key).absolute().top(4.0).right(4.0).center().size(18.0, 18.0).radius(9.0).bg(Color::rgb(0x202020)).hover_bg(Color::BLACK).tooltip(label).on_click(move |app: &mut App| on_remove(app)).child(icon("X", 12.0).color(Color::WHITE))
}

fn image_thumbnail(app: &mut App, strip: &str, image: &ImageAttachment, compact: bool, on_remove: Option<Rc<dyn Fn(&mut App)>>, viewer: &Local<Option<ImageAttachment>>) -> E {
    let t_ = theme();
    let (w, h, radius) = if compact { (64.0, 52.0, 8.0) } else { (112.0, 88.0, 10.0) };
    let dimensions = dimensions_of(image);
    let title = if dimensions.is_empty() { image.name.clone() } else { format!("{} · {dimensions}", image.name) };
    let mut state = thumb_of(app, image);
    // A picture that will not decode is the same local failure as one that would not load.
    let mut texture: Option<(ImageId, (u32, u32))> = None;
    if let Thumb::Ready(source) = &state {
        match picture_for(app, source) {
            Picture::Ready { id, width, height } => {
                texture = Some((id, (width, height)));
                if app.local::<Thumbs>().recovering.remove(&image_data_key(image)) {
                    announce(tf("{name} 已重新加载", "{name} reloaded", &[("name", &image.name)]));
                }
            }
            Picture::Failed => {
                app.memory_pool().delete(MemoryPoolKind::ImageThumbnail, &image_data_key(image));
                let thumbs = app.local::<Thumbs>();
                thumbs.states.insert(image_data_key(image), Thumb::Failed);
                thumbs.recovering.remove(&image_data_key(image));
                announce(tf("{name} 加载失败", "{name} failed to load", &[("name", &image.name)]));
                state = Thumb::Failed;
            }
            Picture::Pending => {}
        }
    }
    let action_label = match &state {
        Thumb::Ready(_) => tf("查看原图 {name}", "View full image {name}", &[("name", &image.name)]),
        Thumb::Failed => tf("重试加载图片 {name}", "Retry loading image {name}", &[("name", &image.name)]),
        Thumb::Loading => tf("图片 {name} 正在加载", "Image {name} is loading", &[("name", &image.name)]),
    };
    say(&action_label);
    let mut open: E = col().absolute().inset(0.0).center().radius(radius).clip().tooltip(title).role(Role::Button).aria_label(action_label).aria_disabled(state == Thumb::Loading);
    if a11y_active() {
        open = open.aria_description(match &state {
            Thumb::Ready(_) => tf("{name} 已加载", "{name} loaded", &[("name", &image.name)]),
            Thumb::Failed => tf("{name} 加载失败", "{name} failed to load", &[("name", &image.name)]),
            Thumb::Loading => tf("{name} 正在加载", "{name} is loading", &[("name", &image.name)]),
        });
    }
    open = match &state {
        Thumb::Ready(_) => {
            let (viewer, picked) = (viewer.clone(), image.clone());
            let open = open.cursor(CursorKind::Hand).on_click(move |_: &mut App| viewer.set(Some(picked.clone())));
            match texture {
                Some((id, natural)) => open.child(fluent::el::image::<App>(id, natural).cover().image_radius(radius).w_full().h_full()),
                None => open.child(spinner(16.0)),
            }
        }
        Thumb::Failed => {
            let retrying = image.clone();
            open.gap(4.0).row().cursor(CursorKind::Hand).on_click(move |app: &mut App| retry_thumb(app, &retrying)).child(icon("RefreshCw", 16.0).color(t_.critical)).child(text(t("重试", "Retry")).ty(FINE).color(t_.critical))
        }
        Thumb::Loading => open.child(spinner(16.0)),
    };
    let mut item: E = div().key(image_key(strip, &image.id)).role(Role::ListItem).size(w, h).radius(radius).border(t_.card_stroke).bg(t_.card_secondary).shrink0().child(open);
    if let Some(remove) = on_remove {
        item = item.child(remove_button(image_remove_key(strip, &image.id), tf("移除图片 {name}", "Remove image {name}", &[("name", &image.name)]), remove));
    }
    item
}

/// `LoaderCircle.spin`.
pub(crate) fn spinner(size: f32) -> E {
    let t_ = theme();
    custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let (cx, cy) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        ui.arc(cx, cy, size * 0.42, 1.6, ui.time * std::f32::consts::TAU, std::f32::consts::PI * 1.5, t_.text_tertiary);
        ui.animating = true;
    })
    .size(size, size)
    .pass_through()
}

/// The short type label a file tile carries.
pub fn file_type_label(file: &FileAttachment) -> String {
    if file.format == FileAttachmentFormat::Pdf {
        return "PDF".to_owned();
    }
    match file.name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => extension.chars().take(6).collect::<String>().to_uppercase(),
        _ => "TXT".to_owned(),
    }
}

fn file_size_label(file: &FileAttachment) -> String {
    match (file.format, file.pages) {
        (FileAttachmentFormat::Pdf, Some(pages)) if pages > 0 => tf("{pages} 页 · {size}", "{pages} pages · {size}", &[("pages", &pages), ("size", &format_bytes(file.bytes))]),
        _ => format_bytes(file.bytes),
    }
}

/// A file on a message, in the same strip as its pictures: a tile rather than a thumbnail, since a file's first look is its name
/// and type, not its pixels.
fn file_tile(strip: &str, file: &FileAttachment, compact: bool, on_remove: Option<Rc<dyn Fn(&mut App)>>, opened: &Local<Option<FileAttachment>>) -> E {
    let t_ = theme();
    let size = file_size_label(file);
    let (w, h, radius) = if compact { (172.0, 52.0, 8.0) } else { (184.0, 88.0, 10.0) };
    // A PDF is one by its bytes, whatever it was named.
    let icon_path = if file.format == FileAttachmentFormat::Pdf { "file.pdf" } else { file.name.as_str() };
    let label = tf("预览文件 {name}", "Preview file {name}", &[("name", &file.name)]);
    say(&label);
    say(&file.name);
    say(&size);
    let meta: E = row().items_center().gap(5.0).child(div().px(4.0).radius(4.0).bg(t_.card).shrink0().child(text(file_type_label(file)).ty(FINE_STRONG).color(t_.text_primary))).child(text(size.clone()).ty(FINE).color(t_.text_secondary).ellipsis().min_w(0.0));
    let name: E = text(file.name.clone()).ty(CAPTION_STRONG).color(t_.text_primary);
    let name = if compact { name.ellipsis() } else { name };
    let copy: E = col().flex1().min_w(0.0).gap(3.0).child(name).child(meta);
    let leading: E = if compact { row().center().size(30.0, 30.0).radius(7.0).bg(t_.card).shrink0().child(file_icon(icon_path, 16.0)) } else { div().shrink0().child(file_icon(icon_path, 20.0)) };
    let (opening, picked) = (opened.clone(), file.clone());
    let open: E = if compact { row().items_center().gap(9.0).pl(9.0).pr(28.0).py(6.0) } else { col().items_start().justify_between().gap(6.0).px(12.0).py(10.0) };
    let open = open.absolute().inset(0.0).radius(radius).hover_bg(t_.subtle_hover).aria_label(label).tooltip(format!("{} · {size}", file.name)).on_click(move |_: &mut App| opening.set(Some(picked.clone()))).child(leading).child(copy);
    let mut item: E = div().key(file_key(strip, &file.id)).role(Role::ListItem).size(w, h).radius(radius).border(t_.card_stroke).bg(t_.card_secondary).shrink0().child(open);
    if let Some(remove) = on_remove {
        item = item.child(remove_button(file_remove_key(strip, &file.id), tf("移除文件 {name}", "Remove file {name}", &[("name", &file.name)]), remove));
    }
    item
}

/// The viewer dims the whole window, so it claims the whole window. Escape and a press on the backdrop close it.
fn image_viewer(app: &mut App, image: &ImageAttachment, on_close: Rc<dyn Fn(&mut App)>) -> E {
    let label = tf("查看原图 {name}", "View full image {name}", &[("name", &image.name)]);
    say(&label);
    let full = full_of(app, image);
    let thumb = thumb_of(app, image);
    // The chip's thumbnail stands in until the full picture arrives.
    let source = full.or(match thumb {
        Thumb::Ready(source) => Some(source),
        _ => None,
    });
    let picture = source.as_ref().map(|source| picture_for(app, source));
    let picture: E = match picture {
        Some(Picture::Ready { id, width, height }) => {
            custom(move |_: &mut App, ui: &mut Ui<'_>, r: Rect| {
                // `object-fit: contain`.
                let k = (r.w / width.max(1) as f32).min(r.h / height.max(1) as f32);
                let (w, h) = (width as f32 * k, height as f32 * k);
                ui.image(id, rect(r.x + (r.w - w) * 0.5, r.y + (r.h - h) * 0.5, w, h), 0.0, 1.0);
            })
            .role(Role::Image)
            .aria_label(tf("{name} 原图", "Full image {name}", &[("name", &image.name)]))
            .flex1()
            .min_h(0.0)
            .w_full()
        }
        _ => row().flex1().min_h(0.0).center().child(spinner(22.0)),
    };
    let dimensions = dimensions_of(image);
    let close = on_close.clone();
    let close_label = tf("关闭原图 {name}", "Close full image {name}", &[("name", &image.name)]);
    say(&close_label);
    let caption: E = row().items_center().justify_center().gap(8.0).min_h(28.0).shrink0().child(text(image.name.clone()).ty(FINE).color(Color::WHITE).ellipsis().min_w(0.0)).when(!dimensions.is_empty(), |e| e.child(text(dimensions.clone()).ty(FINE).color(Color::WHITE.fade(0.64)).nowrap().shrink0().aria_hidden(true)));
    let backdrop = on_close.clone();
    col()
        .key(("image-viewer", image.id.as_str()))
        .aria_label(label)
        .p(24.0)
        .bg(Color::rgb(0x20201F))
        .on_click(move |app: &mut App| backdrop(app))
        .child(picture)
        .child(caption)
        .child(row().key("image-viewer-close").absolute().top(24.0).right(24.0).center().size(34.0, 34.0).radius(17.0).bg(Color::rgb(0x363636)).hover_bg(Color::rgb(0x626262)).tooltip(close_label).on_click(move |app: &mut App| close(app)).child(icon("X", 19.0).color(Color::WHITE)))
        .modal_layer(Some(on_close))
}

/// What the strip shows and what it can do.
pub struct ImageStripProps {
    /// Names this strip's chips among all others on screen.
    pub key: String,
    pub images: Vec<ImageAttachment>,
    pub files: Vec<FileAttachment>,
    pub compact: bool,
    /// A placeholder for attachments still being prepared: a PDF being read, an upload in flight.
    pub busy: bool,
    pub on_remove: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub on_remove_file: Option<Rc<dyn Fn(&mut App, &str)>>,
    /// Whether the conversation's model reads a PDF as the document itself: upstream's `PdfReadingContext`.
    pub reads_documents: bool,
}

/// The strip, or nothing when there is nothing to show.
pub fn image_strip(app: &mut App, props: ImageStripProps) -> Option<E> {
    let ImageStripProps { key, images, files, compact, busy, on_remove, on_remove_file, reads_documents } = props;
    let viewer = use_state(("image-strip-viewer", key.as_str()), || None::<ImageAttachment>);
    let opened = use_state(("image-strip-file-open", key.as_str()), || None::<FileAttachment>);
    if images.is_empty() && files.is_empty() && !busy {
        return None;
    }
    let t_ = theme();
    let count = images.len() + files.len();
    let strip_label = if files.is_empty() { tf("{count} 张图片", "{count} images", &[("count", &images.len())]) } else { tf("{count} 个附件", "{count} attachments", &[("count", &count)]) };
    say(&strip_label);
    let mut strip: E = row().key(("image-strip", key.as_str())).role(Role::List).aria_label(strip_label).aria_busy(busy).wrap().gap(8.0);
    for image in &images {
        let remove = on_remove.clone().map(|f| {
            let id = image.id.clone();
            Rc::new(move |app: &mut App| f(app, &id)) as Rc<dyn Fn(&mut App)>
        });
        strip = strip.child(image_thumbnail(app, &key, image, compact, remove, &viewer));
    }
    for file in &files {
        let remove = on_remove_file.clone().map(|f| {
            let id = file.id.clone();
            Rc::new(move |app: &mut App| f(app, &id)) as Rc<dyn Fn(&mut App)>
        });
        strip = strip.child(file_tile(&key, file, compact, remove, &opened));
    }
    if busy {
        say(t("正在添加附件…", "Adding attachments…"));
        let (w, h, radius) = if compact { (64.0, 52.0, 8.0) } else { (112.0, 88.0, 10.0) };
        strip = strip.child(row().key(("image-strip-pending", key.as_str())).role(Role::Status).aria_label(t("正在添加附件…", "Adding attachments…")).center().size(w, h).radius(radius).border(t_.card_stroke).bg(t_.card_secondary).shrink0().child(spinner(16.0)));
    }
    if let Some(image) = viewer.get() {
        let closing = viewer.clone();
        strip = strip.child(image_viewer(app, &image, Rc::new(move |_: &mut App| closing.set(None))));
    }
    if let Some(file) = opened.get() {
        let closing = opened.clone();
        strip = strip.child(file_attachment_preview(app, &file, reads_documents, Rc::new(move |_: &mut App| closing.set(None))));
    }
    Some(strip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, format: FileAttachmentFormat) -> FileAttachment {
        FileAttachment { id: "f".into(), name: name.into(), format, bytes: 2048, tokens: 1, pages: None }
    }

    #[test]
    fn a_file_tile_says_its_type_short() {
        assert_eq!(file_type_label(&file("x.pdf", FileAttachmentFormat::Pdf)), "PDF");
        assert_eq!(file_type_label(&file("scan", FileAttachmentFormat::Pdf)), "PDF");
        assert_eq!(file_type_label(&file("notes.md", FileAttachmentFormat::Text)), "MD");
        assert_eq!(file_type_label(&file("Makefile", FileAttachmentFormat::Text)), "TXT");
        assert_eq!(file_type_label(&file(".env", FileAttachmentFormat::Text)), "TXT");
        assert_eq!(file_type_label(&file("a.typescript", FileAttachmentFormat::Text)), "TYPESC");
    }
}
