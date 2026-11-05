//! `AppearanceSettings/BackgroundLibrary.tsx`: the library of backgrounds, and importing into it.

use std::collections::HashMap;
use std::path::PathBuf;

use fluent::{queue_image, release_image, ImageId};
use mewrk_client::prelude::*;
use serde::Deserialize;

use crate::effects::{resolved_scheme, Picture};
use crate::logic::background::{parse_background, solid_background, Background, BackgroundScheme, BuiltinPicture, BUILTIN_PICTURES, THEME_SOLID};
use crate::logic::background_image::{base64_encode, build_background_tiers, decode, decode_data_url, read_picture};
use crate::logic::store;
use crate::widgets::*;

/// The smallest tier of an imported picture, which is plenty for a thumbnail.
const THUMBNAIL_WIDTH: u32 = 480;
const THUMBNAIL_HEIGHT: u32 = 270;

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct BackgroundImage {
    pub id: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy)]
enum Thumb {
    Loading,
    Ready(Picture),
    Gone,
}

#[derive(Default)]
pub struct BackgroundState {
    pub generation: u64,
    /// Every import and removal bumps this, and whatever lists or shows the pictures reads again.
    pub library_generation: u64,
    listed_generation: Option<u64>,
    pub imported: Option<Vec<BackgroundImage>>,
    pub list_error: Option<String>,
    pub importing: bool,
    pub error: Option<String>,
    thumbs: HashMap<String, (Thumb, u64)>,
}

fn state(app: &mut App) -> &mut BackgroundState {
    app.local::<BackgroundState>()
}

pub fn unmount(app: &mut App) {
    let s = state(app);
    let generation = s.generation + 1;
    let library_generation = s.library_generation;
    for (thumb, _) in s.thumbs.values() {
        if let Thumb::Ready(p) = thumb {
            release_image(p.id);
        }
    }
    *s = BackgroundState { generation, library_generation, ..Default::default() };
}

/// How many thumbnails are loaded, for tests.
pub fn thumbnails_in(app: &mut App) -> usize {
    state(app).thumbs.values().filter(|(t, _)| matches!(t, Thumb::Ready(_))).count()
}

fn store_thumb(app: &mut App, id: String, generation: u64, thumb: Thumb) {
    let s = state(app);
    if s.generation != generation {
        if let Thumb::Ready(p) = thumb {
            release_image(p.id);
        }
        return;
    }
    let library = s.library_generation;
    if let Some((Thumb::Ready(old), _)) = s.thumbs.insert(id, (thumb, library)) {
        release_image(old.id);
    }
    repaint();
}

/// An imported picture's thumbnail, read again after any import, which may have restored this id's files.
fn imported_thumb(app: &mut App, image_id: &str) -> Thumb {
    let s = state(app);
    let library = s.library_generation;
    if let Some((thumb, at)) = s.thumbs.get(image_id)
        && (*at == library || matches!(thumb, Thumb::Loading)) {
            return *thumb;
        }
    let generation = s.generation;
    s.thumbs.insert(image_id.to_owned(), (Thumb::Loading, library));
    let id = image_id.to_owned();
    let args = json!({ "imageId": image_id, "width": THUMBNAIL_WIDTH, "height": THUMBNAIL_HEIGHT });
    app.spawn(move |task| {
        let read = task.invoke::<Value>("background_image_data", args).and_then(|tier| tier.get("dataUrl").and_then(Value::as_str).map(str::to_owned).ok_or_else(|| "no data".to_owned()));
        let thumb = read.and_then(|url| decode_data_url(&url)).map_or(Thumb::Gone, |p| Thumb::Ready(Picture { id: queue_image(p.data, p.width, p.height), size: (p.width, p.height) }));
        task.post(move |app| store_thumb(app, id, generation, thumb));
    });
    Thumb::Loading
}

fn builtin_thumb(app: &mut App, picture: &'static BuiltinPicture) -> Thumb {
    let s = state(app);
    if let Some((thumb, _)) = s.thumbs.get(picture.id) {
        return *thumb;
    }
    let generation = s.generation;
    let library = s.library_generation;
    s.thumbs.insert(picture.id.to_owned(), (Thumb::Loading, library));
    let id = picture.id.to_owned();
    let bytes = picture.thumbnail;
    app.spawn(move |task| {
        let thumb = decode(bytes).map_or(Thumb::Gone, |p| Thumb::Ready(Picture { id: queue_image(p.data, p.width, p.height), size: (p.width, p.height) }));
        task.post(move |app| store_thumb(app, id, generation, thumb));
    });
    Thumb::Loading
}

fn picture_surface(id: ImageId, size: (u32, u32)) -> E {
    // It takes no presses: a custom leaf claims them otherwise, and the tile around it is what's clicked.
    custom::<App>(move |_, ui, r| ui.image_cover(id, r, size, CONTROL_RADIUS, 1.0)).w_full().h_full().pass_through()
}

fn surface(app: &mut App, value: &str, w: f32, h: f32) -> E {
    let t_ = theme();
    let base = row().size(w, h).center().radius(CONTROL_RADIUS).border(t_.control_stroke).clip();
    let badge = |name: &'static str| icon(name, 14.0).color(t_.text_tertiary);
    let draw = |base: E, thumb: Thumb| match thumb {
        Thumb::Ready(p) => base.child(picture_surface(p.id, p.size)),
        Thumb::Loading => base.child(spinner(14.0)),
        Thumb::Gone => base.child(badge("ImageOff")),
    };
    match parse_background(value) {
        Background::Solid(scheme) => {
            let on_screen = resolved_scheme(app);
            let dark = scheme.unwrap_or(on_screen) == BackgroundScheme::Night;
            base.bg(if dark { fluent::Theme::dark().mica_base } else { fluent::Theme::light().mica_base })
        }
        Background::Builtin(b) => {
            let thumb = builtin_thumb(app, b);
            draw(base, thumb)
        }
        Background::Imported(id) => {
            let thumb = imported_thumb(app, id);
            draw(base, thumb)
        }
    }
}

/// The background the settings point at, beside the button that opens the library.
pub fn background_preview(app: &mut App, value: &str) -> E {
    surface(app, value, 64.0, 40.0)
}

/// `useBackgroundPicture`: the picture a background shows, for drawing it small. `None` for a solid ground, while it loads, or when its files
/// can no longer be read.
pub fn background_picture(app: &mut App, value: &str) -> Option<Picture> {
    let thumb = match parse_background(value) {
        Background::Solid(_) => return None,
        Background::Builtin(b) => builtin_thumb(app, b),
        Background::Imported(id) => imported_thumb(app, id),
    };
    match thumb {
        Thumb::Ready(p) => Some(p),
        Thumb::Loading | Thumb::Gone => None,
    }
}

fn set_background(app: &mut App, value: String) {
    store::change(app, move |doc| doc.global_settings.appearance.background = value);
}

fn library_changed(app: &mut App) {
    state(app).library_generation += 1;
    repaint();
}

fn import_failed(app: &mut App, reason: String) {
    let s = state(app);
    s.importing = false;
    s.error = Some(if reason == "unreadable" {
        t("无法读取这张图片，请换一张 PNG、JPEG、WebP 或 HEIC 图片。", "This picture can't be read. Try a PNG, JPEG, WebP or HEIC file.").to_owned()
    } else {
        tf("背景图片导入失败：{reason}", "Couldn't import the picture: {reason}", &[("reason", &reason)])
    });
}

/// Cuts a picture into its ladder and stores it, on a background thread: the top rung can be 8K. Choosing it applies it.
pub fn import_background_bytes(app: &mut App, bytes: Vec<u8>) {
    let s = state(app);
    s.importing = true;
    s.error = None;
    let generation = s.generation;
    app.spawn(move |task| {
        let result = (|| {
            let tiers = build_background_tiers(&bytes)?;
            let mut upload: Option<String> = None;
            for tier in &tiers {
                let id: String = task.invoke("background_image_put", json!({ "uploadId": upload, "data": base64_encode(&tier.bytes) }))?;
                upload = Some(id);
            }
            let upload = upload.ok_or_else(|| "empty upload".to_owned())?;
            task.invoke::<BackgroundImage>("background_image_commit", json!({ "uploadId": upload }))
        })();
        task.post(move |app| {
            if state(app).generation != generation {
                return;
            }
            match result {
                Ok(image) => {
                    state(app).importing = false;
                    library_changed(app);
                    set_background(app, image.id);
                }
                Err(reason) => import_failed(app, reason),
            }
        });
    });
}

/// `<input type="file" accept="image/*">`, on a thread since the dialog blocks whoever opens it until it's answered.
fn pick_and_import(app: &mut App) {
    let generation = state(app).generation;
    app.spawn(move |task| {
        let picked: Option<PathBuf> = rfd::FileDialog::new().add_filter("Images", &["png", "jpg", "jpeg", "webp", "heic", "heif", "avif", "tif", "tiff", "gif", "bmp"]).pick_file();
        let Some(path) = picked else { return };
        let bytes = read_picture(&path);
        task.post(move |app| {
            if state(app).generation != generation {
                return;
            }
            match bytes {
                Ok(bytes) => import_background_bytes(app, bytes),
                Err(e) => import_failed(app, e),
            }
        });
    });
}

fn remove_picture(app: &mut App, image_id: String) {
    state(app).error = None;
    let generation = state(app).generation;
    let id = image_id.clone();
    app.invoke::<Value>("background_image_delete", json!({ "imageId": image_id }), move |app, result| {
        if state(app).generation != generation {
            return;
        }
        match result {
            Ok(_) => {
                library_changed(app);
                // The window falls back to the theme's ground rather than to a picture that is gone.
                if app.doc.global_settings.appearance.background == id {
                    set_background(app, THEME_SOLID.to_owned());
                }
            }
            Err(e) => state(app).error = Some(tf("无法移除这张图片：{reason}", "Couldn't remove the picture: {reason}", &[("reason", &e)])),
        }
    });
}

fn list_if_needed(app: &mut App) {
    let s = state(app);
    if s.listed_generation == Some(s.library_generation) {
        return;
    }
    s.listed_generation = Some(s.library_generation);
    let generation = s.generation;
    app.invoke::<Vec<BackgroundImage>>("background_image_list", json!({}), move |app, result| {
        let s = state(app);
        if s.generation != generation {
            return;
        }
        match result {
            Ok(images) => {
                s.imported = Some(images);
                s.list_error = None;
            }
            Err(e) => {
                s.imported = Some(Vec::new());
                s.list_error = Some(e);
            }
        }
    });
}

/// `repeat(auto-fill, minmax(128px, 1fr))` in a 620 px window with its 20 px sides: four columns with 10 px between them.
const TILE_WIDTH: f32 = 137.0;
const SURFACE_WIDTH: f32 = 125.0;
const SURFACE_HEIGHT: f32 = 78.0;

fn tile(key: impl std::hash::Hash, selected: bool, surface_el: E, name: String, on_choose: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    col().key(key).w(TILE_WIDTH).gap(6.0).pt(5.0).px(5.0).pb(6.0).radius(10.0).border(if selected { t_.accent } else { t_.card_stroke }).hover_bg(t_.subtle_hover).aria_pressed(selected).on_click(on_choose).child(surface_el).child(text(name).ty(CAPTION_STRONG).color(if selected { t_.accent_text } else { t_.text_secondary }).ellipsis())
}

fn builtin_name(picture: &BuiltinPicture) -> &'static str {
    match picture.name {
        "desk" => t("桌子下", "Under the desk"),
        "chair" => t("椅子上", "On the chair"),
        "shelf" => t("书架中", "On the bookshelf"),
        _ => t("窗帘后", "Behind the curtain"),
    }
}

/// Choosing one applies it at once; the window behind the dialog is the preview.
pub fn background_library_dialog(app: &mut App, value: &str, on_close: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    list_if_needed(app);
    let on_screen = resolved_scheme(app);
    let background = parse_background(value);
    let solid_selected = |scheme: BackgroundScheme| matches!(background, Background::Solid(s) if s.unwrap_or(on_screen) == scheme);
    let mut builtin = row().wrap().gap(10.0);
    for (scheme, name) in [(BackgroundScheme::Day, t("浅色纯色", "Light solid")), (BackgroundScheme::Night, t("深色纯色", "Dark solid"))] {
        let wire = if scheme == BackgroundScheme::Day { "solid:day" } else { "solid:night" };
        let surface_el = surface(app, wire, SURFACE_WIDTH, SURFACE_HEIGHT);
        builtin = builtin.child(tile(("background-solid", wire), solid_selected(scheme), surface_el, name.to_owned(), move |app| {
            let on_screen = resolved_scheme(app);
            set_background(app, solid_background(scheme, on_screen));
        }));
    }
    for picture in BUILTIN_PICTURES {
        let surface_el = surface(app, picture.id, SURFACE_WIDTH, SURFACE_HEIGHT);
        builtin = builtin.child(tile(("background-builtin", picture.id), value == picture.id, surface_el, builtin_name(picture).to_owned(), move |app| set_background(app, picture.id.to_owned())));
    }
    let s = state(app);
    let (importing, imported, list_error, error) = (s.importing, s.imported.clone(), s.list_error.clone(), s.error.clone());
    let glyph = if importing { spinner(18.0) } else { icon("ImagePlus", 18.0).color(t_.text_secondary) };
    let add = col().key("background-add").w(TILE_WIDTH).gap(6.0).pt(5.0).px(5.0).pb(6.0).radius(10.0).border(t_.card_stroke).hover_bg(t_.subtle_hover).disabled(importing).aria_busy(importing).on_click(pick_and_import).child(row().size(SURFACE_WIDTH, SURFACE_HEIGHT).center().radius(CONTROL_RADIUS).bg(t_.subtle_hover).border(t_.control_stroke).child(glyph)).child(text(if importing { t("正在导入…", "Importing…") } else { t("添加图片…", "Add picture…") }).ty(CAPTION_STRONG).color(t_.text_secondary).ellipsis());
    let mut mine = row().wrap().gap(10.0).child(add);
    for image in imported.iter().flatten() {
        let size = format!("{} × {}", image.width, image.height);
        let (choose_id, remove_id) = (image.id.clone(), image.id.clone());
        let surface_el = surface(app, &image.id, SURFACE_WIDTH, SURFACE_HEIGHT);
        let delete = confirm_delete(("background-remove", image.id.as_str()), t("移除这张图片", "Remove this picture").to_owned(), t("确认移除这张图片", "Confirm removing this picture").to_owned(), false, move |app| remove_picture(app, remove_id.clone()));
        // Over the picture's corner, shown while the tile is pointed at. An armed button stays, so the second press can land: leaving it disarms it anyway.
        let tile_name = tf("导入的图片 {size}", "Imported picture {size}", &[("size", &size)]);
        mine = mine.child(col().key(("background-tile", image.id.as_str())).child(tile(("background-imported", image.id.as_str()), value == image.id, surface_el, size, move |app| set_background(app, choose_id.clone())).aria_label(tile_name)).child(delete.absolute().top(9.0).right(9.0).bg(t_.flyout).reveal_on_parent_hover()));
    }
    let heading = |s: &'static str| text(s).ty(CAPTION_STRONG).color(t_.text_secondary).mb(8.0).nowrap().role(Role::Heading).aria_level(3);
    let body = col()
        .gap(18.0)
        .child(col().role(Role::Region).aria_label(t("内置背景", "Built-in backgrounds")).child(heading(t("内置", "Built-in"))).child(builtin))
        .child(
            col()
                .role(Role::Region)
                .aria_label(t("我的图片", "My pictures"))
                .child(heading(t("我的图片", "My pictures")))
                .child(mine)
                .child(help(t("按窗口比例裁切，不拉伸。", "Cropped to the window's shape, never stretched.")).mt(10.0))
                .child(list_error.map(|e| error_line(tf("无法读取导入的图片：{reason}", "Couldn't read the imported pictures: {reason}", &[("reason", &e)])).mt(6.0)))
                .child(error.map(|e| error_line(e).mt(6.0))),
        );
    let close: Rc<dyn Fn(&mut App)> = Rc::new(on_close);
    let close_footer = close.clone();
    modal("background-library", t("背景", "Background"), Some(help(t("纯色随主题切换，图片不变。", "A solid color follows the theme; a picture stays."))), 620.0, body, Some(accent_button::<App>(t("完成", "Done")).key("background-done").on_click(move |a: &mut App| close_footer(a))), move |a| close(a))
}
