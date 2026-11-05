//! `components/ImageStrip.tsx`, for views that have no `&mut App` while they are built.
//!
//! The composer crate has its own port, `mewrk_ui_composer::image_strip`, but it takes `&mut App` to build its chips, and every
//! editor here is a function of its key and props. So this one reads the app where a view can reach it: in a `custom` leaf's paint
//! and in handlers. The two share the memory pool, so a picture shown in both is fetched once, and the files pane's picture
//! cache, so it is one texture.
//!
//! Opening a file is the one thing that cannot happen in a leaf: `file_attachment_preview` builds elements with `&mut App`. The
//! click records the file and `attachment_preview_layer`, called once where a timeline has its `App`, draws the dialog.

use std::collections::HashMap;
use std::hash::Hash;

use fluent::CursorKind;
use mewrk_client::memory_pool::MemoryPoolKind;
use mewrk_client::model::{FileAttachment, FileAttachmentFormat, ImageAttachment};
use mewrk_client::prelude::*;
use mewrk_pane_files::attachment_preview::file_attachment_preview;
use mewrk_pane_files::format::format_bytes;
use mewrk_pane_files::icons::{file_extension, file_icon};
use mewrk_pane_files::images::{picture_for, Picture};
use mewrk_pane_tasks::widgets::spinner;

const fn style(size: f32, line: f32, weight: Weight) -> TypeStyle {
    TypeStyle { size, line, weight, mono: false, italic: false, serif: false }
}

const FILE_NAME: TypeStyle = style(12.0, 15.6, Weight::Semibold);
const FILE_NAME_COMPACT: TypeStyle = style(11.5, 15.0, Weight::Semibold);
const FILE_META: TypeStyle = style(10.5, 13.65, Weight::Regular);
const FILE_TYPE: TypeStyle = style(9.5, 13.0, Weight::Semibold);
const RETRY: TypeStyle = style(10.0, 10.0, Weight::Semibold);
const RETRY_COMPACT: TypeStyle = style(9.0, 9.0, Weight::Semibold);
const CAPTION_LINE: TypeStyle = style(11.0, 16.0, Weight::Regular);

fn image_data_key(id: &str, mime: &str) -> String {
    format!("{id}:{mime}")
}

/// Where a chip's picture stands, for a test or anything else that asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThumbnailState {
    Loading,
    Ready,
    Failed,
}

/// What a chip's paint last found, for its view to say: the view has no `App` to ask.
struct ChipStatus {
    state: ThumbnailState,
    /// A retry is on the way, so its success is news: `recoveryPendingRef`.
    recovering: bool,
}

/// `setAnnouncement`, as the paint sees each change.
fn note_status(status: &Local<ChipStatus>, state: ThumbnailState, name: &str) {
    let mut chip = status.borrow_mut();
    if chip.state == state {
        return;
    }
    match state {
        ThumbnailState::Loading => {}
        ThumbnailState::Failed => {
            chip.recovering = false;
            announce(tf("{name} 加载失败", "{name} failed to load", &[("name", &name)]));
        }
        ThumbnailState::Ready => {
            if std::mem::take(&mut chip.recovering) {
                announce(tf("{name} 已重新加载", "{name} reloaded", &[("name", &name)]));
            }
        }
    }
    chip.state = state;
    repaint();
}

#[derive(Clone)]
enum Load {
    Loading,
    Ready(Arc<str>),
    Failed,
}

#[derive(Default)]
struct StripStore {
    thumbs: HashMap<String, Load>,
    /// The picture the viewer has open, by key: full-size pictures are low-priority data, so nothing keeps them past the viewer.
    full: Option<(String, Load)>,
    open_file: Option<FileAttachment>,
}

/// The chip's picture: the host's thumbnail, high-priority data in the shared pool.
fn thumbnail(app: &mut App, id: &str, mime: &str) -> Load {
    let key = image_data_key(id, mime);
    match app.local::<StripStore>().thumbs.get(&key).cloned() {
        Some(Load::Ready(source)) => {
            // The pool may have let it go; this copy only stands for what the pool holds.
            if app.memory_pool().has(MemoryPoolKind::ImageThumbnail, &key) {
                app.memory_pool().touch(MemoryPoolKind::ImageThumbnail, &key);
                return Load::Ready(source);
            }
            app.local::<StripStore>().thumbs.remove(&key);
        }
        Some(other) => return other,
        None => {}
    }
    app.local::<StripStore>().thumbs.insert(key.clone(), Load::Loading);
    let image_id = id.to_owned();
    let writing = key.clone();
    app.pooled_fetch(MemoryPoolKind::ImageThumbnail, &key, move |task| task.invoke::<String>("image_attachment_thumbnail", json!({ "imageId": image_id })), move |app, result| {
        let next = match result {
            Ok(source) => Load::Ready(Arc::from(source.as_str())),
            Err(_) => Load::Failed,
        };
        // What the pool has dropped since has no reason to stay here either.
        let mut thumbs = std::mem::take(&mut app.local::<StripStore>().thumbs);
        thumbs.retain(|key, load| !matches!(load, Load::Ready(_)) || app.memory_pool().has(MemoryPoolKind::ImageThumbnail, key));
        thumbs.insert(writing, next);
        app.local::<StripStore>().thumbs = thumbs;
        repaint();
    });
    Load::Loading
}

/// `forgetImageData`, and the chip starts over.
fn forget_image(app: &mut App, id: &str, mime: &str) {
    let key = image_data_key(id, mime);
    app.memory_pool().delete(MemoryPoolKind::ImageThumbnail, &key);
    app.memory_pool().delete(MemoryPoolKind::ImageData, &key);
    let store = app.local::<StripStore>();
    store.thumbs.remove(&key);
    if store.full.as_ref().is_some_and(|(open, _)| *open == key) {
        store.full = None;
    }
    repaint();
}

/// The viewer's picture: the full image, low-priority data in the shared pool.
fn full_image(app: &mut App, id: &str, mime: &str) -> Load {
    let key = image_data_key(id, mime);
    if let Some((open, load)) = &app.local::<StripStore>().full
        && *open == key
    {
        return load.clone();
    }
    app.local::<StripStore>().full = Some((key.clone(), Load::Loading));
    let image_id = id.to_owned();
    let writing = key.clone();
    app.pooled_fetch(MemoryPoolKind::ImageData, &key, move |task| task.invoke::<String>("image_attachment_data", json!({ "imageId": image_id })), move |app, result| {
        let store = app.local::<StripStore>();
        // A viewer closed or moved on while this was on the way leaves it unasked for.
        if store.full.as_ref().is_some_and(|(open, _)| *open == writing) {
            store.full = Some((
                writing,
                match result {
                    Ok(source) => Load::Ready(Arc::from(source.as_str())),
                    Err(_) => Load::Failed,
                },
            ));
            repaint();
        }
    });
    Load::Loading
}

pub fn thumbnail_state(app: &mut App, image: &ImageAttachment) -> ThumbnailState {
    match thumbnail(app, &image.id, &image.mime) {
        Load::Loading => ThumbnailState::Loading,
        Load::Ready(_) => ThumbnailState::Ready,
        Load::Failed => ThumbnailState::Failed,
    }
}

fn dimensions_of(image: &ImageAttachment) -> String {
    if image.width > 0 && image.height > 0 { format!("{} × {}", image.width, image.height) } else { String::new() }
}

/// The hover text: the name and size alone.
pub fn image_title(image: &ImageAttachment) -> String {
    let dimensions = dimensions_of(image);
    if dimensions.is_empty() { image.name.clone() } else { format!("{} · {dimensions}", image.name) }
}

/// The short type label a file tile carries.
pub fn file_type_label(file: &FileAttachment) -> String {
    if file.format == FileAttachmentFormat::Pdf {
        return "PDF".to_owned();
    }
    let extension = file_extension(&file.name);
    if extension.is_empty() { "TXT".to_owned() } else { extension.chars().take(6).collect::<String>().to_uppercase() }
}

pub fn file_size_label(file: &FileAttachment) -> String {
    match (file.format, file.pages) {
        (FileAttachmentFormat::Pdf, Some(pages)) if pages > 0 => tf("{pages} 页 · {size}", "{pages} pages · {size}", &[("pages", &pages), ("size", &format_bytes(file.bytes))]),
        _ => format_bytes(file.bytes),
    }
}

fn remove_button(key: impl Hash, label: String, on_remove: impl Fn(&mut App) + 'static) -> E {
    row()
        .key(key)
        .absolute()
        .top(4.0)
        .right(4.0)
        .center()
        .size(22.0, 22.0)
        .radius(11.0)
        .bg(Color::argb(0xCC202020))
        .hover_bg(Color::BLACK)
        .tooltip(label)
        .on_click(on_remove)
        .child(icon("X", 14.0).color(Color::WHITE))
}

/// A chip's key, for whatever needs to find it.
pub fn thumbnail_key(strip: u64, index: usize) -> (&'static str, u64, usize) {
    ("image-strip-thumb", strip, index)
}

/// The press target inside a chip, which focus returns to when the viewer closes.
fn open_key(strip: u64, index: usize) -> (&'static str, u64, usize) {
    ("image-strip-thumb-open", strip, index)
}

pub fn thumbnail_remove_key(strip: u64, index: usize) -> (&'static str, u64, usize) {
    ("image-strip-thumb-remove", strip, index)
}

pub fn file_tile_key(strip: u64, id: &str) -> (&'static str, u64, String) {
    ("image-strip-file", strip, id.to_owned())
}

fn file_open_key(strip: u64, id: &str) -> (&'static str, u64, String) {
    ("image-strip-file-open", strip, id.to_owned())
}

pub fn file_remove_key(strip: u64, id: &str) -> (&'static str, u64, String) {
    ("image-strip-file-remove", strip, id.to_owned())
}

pub fn viewer_close_key(strip: u64) -> (&'static str, u64) {
    ("image-viewer-close", strip)
}

fn image_thumbnail(strip: u64, index: usize, image: &ImageAttachment, compact: bool, on_remove: Option<Rc<dyn Fn(&mut App, &str)>>, viewer: &Local<Option<(ImageAttachment, usize)>>) -> E {
    let t_ = theme();
    let (w, h, radius) = if compact { (64.0, 52.0, 8.0) } else { (112.0, 88.0, 10.0) };
    let icon_size = if compact { 16.0 } else { 22.0 };
    let status = use_state(("image-strip-status", strip, index), || ChipStatus { state: ThumbnailState::Loading, recovering: false });
    let (id, mime, name) = (image.id.clone(), image.mime.clone(), image.name.clone());
    let painted = status.clone();
    let picture = custom(move |app: &mut App, ui: &mut Ui<'_>, r: Rect| {
        let (cx, cy) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        let mut load = thumbnail(app, &id, &mime);
        if let Load::Ready(source) = &load {
            match picture_for(app, source) {
                Picture::Ready { id, width, height } => {
                    note_status(&painted, ThumbnailState::Ready, &name);
                    // No lazy loading here either: the bytes are already in memory, and a chip is only painted when it shows.
                    ui.image_cover(id, r, (width, height), radius - 1.0, 1.0);
                    return;
                }
                // Already a press away from the viewer, as `source` being set is upstream.
                Picture::Pending => note_status(&painted, ThumbnailState::Ready, &name),
                // A picture that will not decode is the same local failure as one that would not load.
                Picture::Failed => {
                    forget_image(app, &id, &mime);
                    app.local::<StripStore>().thumbs.insert(image_data_key(&id, &mime), Load::Failed);
                    load = Load::Failed;
                }
            }
        }
        if matches!(load, Load::Failed) {
            note_status(&painted, ThumbnailState::Failed, &name);
            let retry = t("重试", "Retry");
            let ty = if compact { RETRY_COMPACT } else { RETRY };
            let color = t_.critical;
            if compact {
                let label = ui.measure(retry, &ty);
                let start = cx - (icon_size + 4.0 + label) * 0.5;
                ui.named_icon("RefreshCw", start + icon_size * 0.5, cy, icon_size, color, 0.0);
                ui.label(start + icon_size + 4.0, cy - ty.line * 0.5, retry, &ty, color);
            } else {
                let height = icon_size + 3.0 + ty.line;
                let top = cy - height * 0.5;
                ui.named_icon("RefreshCw", cx, top + icon_size * 0.5, icon_size, color, 0.0);
                ui.label_centered(rect(r.x, top + icon_size + 3.0, r.w, ty.line), retry, &ty, color);
            }
            return;
        }
        if matches!(load, Load::Loading) {
            note_status(&painted, ThumbnailState::Loading, &name);
        }
        ui.named_icon("LoaderCircle", cx, cy, icon_size, t_.text_secondary, (ui.time * 360.0) % 360.0);
        ui.wake_after(1.0 / 30.0);
    })
    .w_full()
    .h_full()
    .pass_through();

    let (open_image, open_viewer, retrying) = (image.clone(), viewer.clone(), status.clone());
    let state = status.with(|chip| chip.state.clone());
    let open = row()
        .key(open_key(strip, index))
        .w_full()
        .h_full()
        .cursor(CursorKind::Hand)
        .on_click(move |app: &mut App| match thumbnail(app, &open_image.id, &open_image.mime) {
            Load::Ready(_) => {
                open_viewer.set(Some((open_image.clone(), index)));
                focus_request(viewer_close_key(strip));
            }
            Load::Failed => {
                retrying.borrow_mut().recovering = true;
                forget_image(app, &open_image.id, &open_image.mime);
            }
            Load::Loading => {}
        })
        .when(a11y_active(), |open| {
            let name = [("name", &image.name as &dyn std::fmt::Display)];
            let (action, said) = match state {
                ThumbnailState::Ready => (tf("查看原图 {name}", "View full image {name}", &name), tf("{name} 已加载", "{name} loaded", &name)),
                ThumbnailState::Failed => (tf("重试加载图片 {name}", "Retry loading image {name}", &name), tf("{name} 加载失败", "{name} failed to load", &name)),
                ThumbnailState::Loading => (tf("图片 {name} 正在加载", "Image {name} is loading", &name), tf("{name} 正在加载", "{name} is loading", &name)),
            };
            open.role(Role::Button).aria_label(action).aria_description(said).aria_disabled(state == ThumbnailState::Loading)
        })
        .child(picture);
    let remove = on_remove.map(|on_remove| {
        let id = image.id.clone();
        let label = tf("移除图片 {name}", "Remove image {name}", &[("name", &image.name)]);
        remove_button(thumbnail_remove_key(strip, index), label, move |app| on_remove(app, &id))
    });
    row().key(thumbnail_key(strip, index)).size(w, h).shrink0().radius(radius).clip().border(t_.card_stroke).bg(t_.subtle_hover).role(Role::ListItem).tooltip(image_title(image)).child(open).child(remove)
}

/// A file on a message, in the same strip as its pictures: a tile rather than a thumbnail, since a file's first look is its name and
/// type, not its pixels.
fn file_tile(strip: u64, file: &FileAttachment, compact: bool, on_remove: Option<Rc<dyn Fn(&mut App, &str)>>) -> E {
    let t_ = theme();
    let size = file_size_label(file);
    let type_label = file_type_label(file);
    // A PDF is one by its bytes, whatever it was named.
    let icon_path = if file.format == FileAttachmentFormat::Pdf { "file.pdf" } else { file.name.as_str() };
    let icon_box = if compact {
        row().center().size(30.0, 30.0).radius(7.0).bg(t_.card).shrink0().child(file_icon(icon_path, 16.0))
    } else {
        row().center().shrink0().child(file_icon(icon_path, 20.0))
    };
    let name = text(file.name.clone()).ty(if compact { FILE_NAME_COMPACT } else { FILE_NAME }).color(t_.text_primary);
    // `-webkit-line-clamp: 2`, without its ellipsis.
    let name = if compact { name.ellipsis() } else { div().max_h(FILE_NAME.line * 2.0).clip().child(name) };
    let meta = row()
        .items_center()
        .gap(5.0)
        .clip()
        .child(row().px(4.0).radius(4.0).bg(t_.card).shrink0().child(text(type_label).ty(FILE_TYPE).color(t_.text_primary).nowrap()))
        .child(text(size.clone()).ty(FILE_META).color(t_.text_secondary).nowrap());
    let words = col().w_full().min_w(0.0).gap(3.0).child(name).child(meta);
    // Side by side the words take what the icon leaves. Stacked, the icon keeps to its corner and the words to the far end.
    let (body, words) = if compact { (row().items_center().gap(9.0).pt(6.0).pb(6.0).pl(9.0).pr(28.0), words.flex1()) } else { (col().items_start().justify_between().gap(6.0).px(12.0).py(10.0), words) };
    let opened = file.clone();
    let open = body
        .key(file_open_key(strip, &file.id))
        .w_full()
        .h_full()
        .hover_bg(t_.subtle_pressed)
        .cursor(CursorKind::Hand)
        .aria_label(tf("预览文件 {name}", "Preview file {name}", &[("name", &file.name)]))
        .on_click(move |app: &mut App| {
            app.local::<StripStore>().open_file = Some(opened.clone());
            repaint();
        })
        .child(icon_box)
        .child(words);
    let remove = on_remove.map(|on_remove| {
        let id = file.id.clone();
        let label = tf("移除文件 {name}", "Remove file {name}", &[("name", &file.name)]);
        remove_button(file_remove_key(strip, &file.id), label, move |app| on_remove(app, &id))
    });
    row()
        .key(file_tile_key(strip, &file.id))
        .size(if compact { 172.0 } else { 184.0 }, if compact { 52.0 } else { 88.0 })
        .shrink0()
        .radius(if compact { 8.0 } else { 10.0 })
        .clip()
        .border(t_.card_stroke)
        .bg(t_.subtle_hover)
        .role(Role::ListItem)
        .tooltip(format!("{} · {size}", file.name))
        .child(open)
        .child(remove)
}

/// The viewer claims the whole window. What upstream's `useFloatingSurface` does about a native browser page above it is for the
/// shell's floating surfaces to do, once there are any.
fn image_viewer(strip: u64, image: &ImageAttachment, close: Handler<App>) -> E {
    let (id, mime) = (image.id.clone(), image.mime.clone());
    let picture = custom(move |app: &mut App, ui: &mut Ui<'_>, r: Rect| {
        // The chip's thumbnail stands in until the full picture arrives.
        let full = full_image(app, &id, &mime);
        let source = match full {
            Load::Ready(source) => Some(source),
            _ => match thumbnail(app, &id, &mime) {
                Load::Ready(source) => Some(source),
                _ => None,
            },
        };
        let Some(source) = source else { return };
        if let Picture::Ready { id, width, height } = picture_for(app, &source) {
            // `object-fit: contain`.
            let (w, h) = (width.max(1) as f32, height.max(1) as f32);
            let fit = (r.w / w).min(r.h / h);
            let (dw, dh) = (w * fit, h * fit);
            ui.image(id, rect(r.x + (r.w - dw) * 0.5, r.y + (r.h - dh) * 0.5, dw, dh), 0.0, 1.0);
        }
    })
    .flex1()
    .w_full()
    .min_h(0.0)
    .role(Role::Image)
    .aria_label(tf("{name} 原图", "Full image {name}", &[("name", &image.name)]));
    let dimensions = dimensions_of(image);
    let window_w = window_size().0;
    let caption = row()
        .center()
        .gap(8.0)
        .min_h(28.0)
        .shrink0()
        .max_w(if window_w <= 640.0 { (window_w - 72.0).max(0.0) } else { (window_w - 120.0).clamp(0.0, 680.0) })
        .child(text(image.name.clone()).ty(CAPTION_LINE).color(Color::WHITE).ellipsis())
        .child((!dimensions.is_empty()).then(|| text(dimensions.clone()).ty(CAPTION_LINE).color(Color::WHITE.fade(0.64)).nowrap().shrink0().aria_hidden(true)));
    let close_label = tf("关闭原图 {name}", "Close full image {name}", &[("name", &image.name)]);
    let (on_close, on_key, on_backdrop) = (close.clone(), close.clone(), close);
    let dialog = col().key(("image-viewer-dialog", strip)).items_center().w_full().h_full().min_w(0.0).min_h(0.0).on_click(|_| {}).role(Role::GenericContainer).cursor(CursorKind::Default).child(picture).child(caption).child(
        row()
            .key(viewer_close_key(strip))
            .absolute()
            .top(0.0)
            .right(0.0)
            .center()
            .size(34.0, 34.0)
            .radius(17.0)
            .bg(Color::WHITE.fade(0.1))
            .hover_bg(Color::WHITE.fade(0.34))
            .tooltip(close_label)
            .on_click(move |app: &mut App| on_close(app))
            .child(icon("X", 19.0).color(Color::WHITE)),
    );
    // Only a press on the padding round the dialog closes it, as upstream's target check says; the lightbox's scrim is the one colour
    // here that does not follow the theme, because a picture needs the same ground in both.
    div()
        .key(("image-viewer", strip))
        .p(if window_w <= 640.0 { 12.0 } else { 24.0 })
        .bg(Color::argb(0xEB202020))
        .on_click(move |app: &mut App| on_backdrop(app))
        .on_key_global(move |app: &mut App, press: &KeyPress| {
            if press.key == Key::Escape {
                on_key(app);
                return true;
            }
            false
        })
        .aria_label(tf("查看原图 {name}", "View full image {name}", &[("name", &image.name)]))
        .child(dialog)
        .modal_layer(None)
}

/// What the strip shows and what it can do.
pub struct ImageStripProps<'a> {
    pub images: &'a [ImageAttachment],
    pub files: &'a [FileAttachment],
    pub compact: bool,
    /// A placeholder for attachments still being prepared: a PDF being read, an upload in flight.
    pub busy: bool,
    pub on_remove: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub on_remove_file: Option<Rc<dyn Fn(&mut App, &str)>>,
}

/// The strip, or nothing when there is nothing to show.
pub fn image_strip_with(key: impl Hash, props: ImageStripProps<'_>) -> Option<E> {
    let ImageStripProps { images, files, compact, busy, on_remove, on_remove_file } = props;
    let k = hash_key(key);
    let viewer = use_state(("image-strip-viewer", k), || None::<(ImageAttachment, usize)>);
    if images.is_empty() && files.is_empty() && !busy {
        return None;
    }
    let t_ = theme();
    let chips = images.iter().enumerate().map(|(index, image)| image_thumbnail(k, index, image, compact, on_remove.clone(), &viewer));
    let tiles = files.iter().map(|file| file_tile(k, file, compact, on_remove_file.clone()));
    let pending = busy.then(|| {
        let (w, h, radius) = if compact { (64.0, 52.0, 8.0) } else { (112.0, 88.0, 10.0) };
        row().key(("image-strip-pending", k)).center().size(w, h).shrink0().radius(radius).border(t_.card_stroke).bg(t_.subtle_hover).role(Role::Status).aria_label(t("正在添加附件…", "Adding attachments…")).child(spinner(if compact { 16.0 } else { 22.0 }, t_.text_secondary))
    });
    let open = viewer.get().map(|(image, index)| {
        let closing = viewer.clone();
        image_viewer(
            k,
            &image,
            Rc::new(move |app: &mut App| {
                // Full pictures are low-priority data: nothing keeps this one past the viewer.
                app.local::<StripStore>().full = None;
                closing.set(None);
                focus_request(open_key(k, index));
            }),
        )
    });
    let strip = row().key(("image-strip", k)).wrap().gap(8.0).children(chips).children(tiles).child(pending).child(open);
    Some(strip.when(a11y_active(), |strip| {
        let label = if files.is_empty() { tf("{count} 张图片", "{count} images", &[("count", &images.len())]) } else { tf("{count} 个附件", "{count} attachments", &[("count", &(images.len() + files.len()))]) };
        strip.role(Role::List).aria_label(label).aria_busy(busy)
    }))
}

/// Everything attached to a message, read-only: its images, then its files.
pub fn image_strip(key: impl Hash + Clone, images: &[ImageAttachment], files: &[FileAttachment]) -> E {
    image_strip_with(key, ImageStripProps { images, files, compact: false, busy: false, on_remove: None, on_remove_file: None }).unwrap_or_else(div)
}

/// The dialog of a file tile that was clicked. `reads_documents` is upstream's `PdfReadingContext`: whether the conversation's model
/// reads a PDF as the document itself. Call it once per frame from wherever a timeline has its `App`.
pub fn attachment_preview_layer(app: &mut App, reads_documents: bool) -> Option<E> {
    let file = app.local::<StripStore>().open_file.clone()?;
    Some(file_attachment_preview(
        app,
        &file,
        reads_documents,
        Rc::new(|app: &mut App| {
            app.local::<StripStore>().open_file = None;
            repaint();
        }),
    ))
}
