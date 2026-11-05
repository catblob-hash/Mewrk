//! What an open file looks like (upstream's `FileViewerBody`, `FileAttachmentPreview`'s body, `PdfViewer`, `HtmlPreview`).
//! One file, one reading: a document is rendered, a picture drawn, a PDF laid out, a recording played, a font set, a table
//! tabulated, and everything else is its own text, numbered and coloured.

use crate::audio::audio_player;
use crate::browser::*;
use crate::code::numbered_code;
use crate::format::*;
use crate::html_preview::{compose_document, load_stylesheets, prepare_html_preview, HtmlPreviewDocument, HtmlResources};
use crate::markdown::{document_view, embed_label, fenced, land_on_fragment, DocContext};
use crate::native::*;
use crate::pane::*;
use crate::path_open;
use crate::previews::*;
use crate::viewers::*;
use crate::widgets::Action;
use mewrk_client::prelude::*;
use mewrk_ui_common::context_menu::{ContextMenuAnchor, ContextMenuItem, ContextMenuSection};
use std::collections::BTreeMap;

const DOCUMENT_PAD: (f32, f32, f32) = (14.0, 10.0, 28.0);

/// A document's own scrolling column.
fn document_scroller(key: &str) -> E {
    col().key(key).flex1().min_w(0.0).min_h(0.0).scroll_y().px(DOCUMENT_PAD.0).pt(DOCUMENT_PAD.1).pb(DOCUMENT_PAD.2)
}

pub fn file_viewer_body(app: &mut App, st: &PaneState, file: &Location, on_mention: Option<Rc<dyn Fn(&mut App, FileMention)>>) -> E {
    let id = st.id.clone();
    let key = file.key();
    let root = st.document_root(file);
    let relative = relative_path(&root, &file.path).unwrap_or_else(|| file.path.clone());
    let kind = file_viewer_kind(&file.path);
    let source_on = st.source_keys.contains(&key);
    let viewer = st.viewers.get(&key).cloned();
    let picture = st.media.get(&key).cloned();
    let name = browse_name(&relative);
    let lit = st.lit_line.as_ref().filter(|(k, _)| *k == key).map(|(_, l)| *l);
    let reveal: Option<Action> = file.machine.is_none().then(|| {
        let (id, path) = (id.clone(), file.path.clone());
        Rc::new(move |app: &mut App| reveal_in_file_manager(app, &id, &path)) as Action
    });
    if kind == FileViewerKind::Video {
        return unsupported_notice(key.as_str(), t("视频无法在应用内播放：界面的安全策略不允许加载媒体。", "Video cannot be played inside the app: the window's security policy allows no media."), reveal);
    }
    if reads_bytes(&relative) && !source_on {
        match &picture {
            None | Some(MediaState::Loading) => return notice(t("正在读取…", "Loading…")),
            Some(MediaState::TooLarge) => return unsupported_notice(key.as_str(), t("文件过大（超过 8 MB），无法在面板中预览。", "This file is larger than 8 MB and cannot be previewed in the pane."), reveal),
            Some(MediaState::Error(message)) => return error_notice(message.clone()),
            Some(MediaState::Ready(source)) => {
                let bytes = Some(data_url_byte_length(source));
                match kind {
                    FileViewerKind::Image => return image_viewer(app, &key, source, &name, bytes, reveal),
                    FileViewerKind::Pdf => return pdf_viewer(app, &key, source, bytes, &name, Some(file.clone()), reveal),
                    FileViewerKind::Audio => return audio_player(app, &key, source, &name, bytes),
                    FileViewerKind::Font => return font_preview(app, &key, source, &name, bytes),
                    _ => {}
                }
            }
        }
    }
    let (content, binary) = match &viewer {
        None | Some(ViewerState::Loading) => return notice(t("正在读取…", "Loading…")),
        Some(ViewerState::Error(message)) => return error_notice(message.clone()),
        Some(ViewerState::Ready { content, binary, .. }) => (content.clone(), *binary),
    };
    if binary {
        return unsupported_notice(key.as_str(), t("二进制文件，无法显示", "Binary file cannot be shown"), reveal);
    }
    if content.is_empty() {
        return notice(t("这个文件是空的。", "This file is empty."));
    }
    let doc_ctx = document_context(st, file, &root, &key);
    if !source_on {
        match kind {
            FileViewerKind::Markdown => {
                let front = split_front_matter(&content);
                let body = front.as_ref().map_or(&*content, |f| f.body.as_str());
                consume_fragment(st, &key, &doc_ctx);
                let mut scroller = document_scroller(&doc_ctx.scroller).gap(8.0);
                if let Some(f) = &front {
                    scroller = scroller.child(document_view(app, &doc_ctx, "front", &fenced(&f.source, f.language)));
                }
                return scroller.child(document_view(app, &doc_ctx, "body", body));
            }
            FileViewerKind::Html => {
                let (open_id, doc) = (id.clone(), relative.clone());
                let on_link: Arc<dyn Fn(&mut App, &str) + Send + Sync> = Arc::new(move |app, href| {
                    open_html_link(app, &open_id, &doc, href);
                });
                return html_preview(app, &key, &relative, &content, Some(file.clone()), root.clone(), on_link, &doc_ctx);
            }
            FileViewerKind::Csv => return csv_table(&key, &relative, &content),
            FileViewerKind::Notebook => {
                let fragment = st.jump_fragment.as_ref().filter(|(k, _)| *k == key).map(|(_, f)| f.as_str());
                let (view, landed) = notebook_view_landing(app, &key, &content, &doc_ctx, fragment);
                if landed {
                    CLEAR_FRAGMENT.with(|c| c.set(true));
                }
                return view;
            }
            _ => {}
        }
    }
    let jump = st.jump_line.as_ref().filter(|(k, _)| *k == key).map(|(_, l)| *l);
    let line_menu: Option<Rc<dyn Fn(&mut App, u32, (f32, f32))>> = on_mention.map(|mention| {
        let (id, file) = (id.clone(), file.clone());
        Rc::new(move |app: &mut App, line: u32, point: (f32, f32)| {
            let Some(st) = pane(app, &id) else { return };
            let workspace = st.containing_workspace(&file);
            let relative = workspace.as_ref().and_then(|w| relative_path(&w.root, &file.path));
            let (mention, number, path) = (mention.clone(), workspace.map(|w| w.number), native_spelling(&file.path));
            let item = ContextMenuItem::new("mention", t("在会话中提及", "Mention in Conversation")).icon("AtSign").on_select(move |app| {
                mention(app, match (number, relative.clone().filter(|r| !r.is_empty())) {
                    (Some(n), Some(r)) => FileMention { path: r, workspace: Some(n), line },
                    _ => FileMention { path: path.clone(), workspace: None, line },
                });
            });
            open_menu(app, &id, ContextMenuAnchor::Point { x: point.0, y: point.1 }, vec![ContextMenuSection::new("mention", vec![item])]);
        }) as Rc<dyn Fn(&mut App, u32, (f32, f32))>
    });
    numbered_code(key.clone(), &content, code_language(&relative), &relative, lit, line_menu, jump)
}

/// A page's relative link, once it has been clicked in the web view or the drawing of it.
pub fn open_html_link(app: &mut App, id: &str, document: &str, href: &str) {
    let href = href.trim();
    if crate::links::external_http_url(href).is_some() {
        app.invoke_logged("open_external_url", json!({ "url": href }));
        return;
    }
    let Some(target) = crate::links::local_link_target(href) else { return };
    if target.path.is_empty() {
        return;
    }
    if let Some(resolved) = resolve_document_reference(document, &target.path) {
        open_file_from_page(app, id, &resolved, target.line);
    }
}

fn consume_fragment(st: &PaneState, key: &str, ctx: &DocContext) {
    if let Some((k, fragment)) = &st.jump_fragment
        && k == key
        && land_on_fragment(ctx, "body", fragment)
    {
        CLEAR_FRAGMENT.with(|c| c.set(true));
    }
}

thread_local! {
    /// A jump that landed; the pane clears it when the view is done borrowing the state.
    pub static CLEAR_FRAGMENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn document_context(st: &PaneState, file: &Location, root: &str, key: &str) -> DocContext {
    let id = st.id.clone();
    // What resolves a picture is asked while the document is built, from the state as it stands this frame.
    let media = st.media.clone();
    let file_for_images = file.clone();
    let root_for_images = root.to_owned();
    let relative = relative_path(root, &file.path).unwrap_or_else(|| file.path.clone());
    let (link_id, path_id) = (id.clone(), id);
    DocContext {
        key: format!("doc/{key}"),
        scroller: format!("doc-scroll/{key}"),
        base_dir: Some(root.to_owned()),
        machine: Some(file.machine.clone()),
        linkify_paths: true,
        resolve_image: Rc::new(move |source: &str| {
            if has_scheme(source) || source.starts_with("//") {
                return Some(Arc::from(source));
            }
            let resolved = resolve_document_reference(&relative, source.split('#').next().unwrap_or(""))?;
            match media.get(&location_key(&file_for_images.machine, &join_path(&root_for_images, &resolved))) {
                Some(MediaState::Ready(data)) => Some(data.clone()),
                _ => None,
            }
        }),
        on_link: Rc::new(move |app, href| on_document_link(app, &link_id, href)),
        on_path: Rc::new(move |app, request| on_document_path(app, &path_id, request)),
    }
}

// ---------------------------------------------------------------- PDF

const PDF_ZOOM_STEPS: [f32; 15] = [0.25, 0.33, 0.5, 0.67, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0];

/// The card a file falls back to where its reader cannot run: its name and size, and the one thing left to do with it.
fn file_card(key: &str, name: &str, bytes: Option<u64>, message: &str, file: Option<&Location>, on_reveal: Option<Action>) -> E {
    let t_ = theme();
    let mut card = col().gap(8.0).p(14.0).radius(10.0).bg(t_.card).border(t_.card_stroke).max_w(420.0).items_center().child(crate::icons::file_icon(name, 28.0)).child(text(name.to_owned()).ty(BODY_SMALL_STRONG).color(t_.text_primary).ellipsis()).child(text(bytes.map_or(String::new(), format_bytes)).ty(FINE).color(t_.text_tertiary).nowrap()).child(text(message.to_owned()).ty(FINE).color(t_.text_secondary).text_center());
    if let Some(file) = file.filter(|f| f.machine.is_none()) {
        let path = native_spelling(&file.path);
        card = card.child(subtle_button(t("用默认程序打开", "Open With Default App")).key(("card-open", key)).on_click(move |app: &mut App| {
            app.invoke_logged("browse_open_with", json!({ "path": path, "app": Value::Null }));
        }));
    }
    if let Some(reveal) = on_reveal {
        card = card.child(subtle_button(t("在文件管理器中显示", "Show in file manager")).key(("card-reveal", key)).on_click(move |app: &mut App| reveal(app)));
    }
    col().flex1().min_h(0.0).center().p(16.0).child(card)
}

/// A PDF through PDFKit: it lays the pages out, draws them as they scroll near, and selects their text itself. The zoom
/// controls and the page count sit in a bar below it rather than floating over it: a native view covers what is drawn.
pub fn pdf_viewer(app: &mut App, key: &str, source: &Arc<str>, bytes: Option<u64>, name: &str, file: Option<Location>, on_reveal: Option<Action>) -> E {
    #[cfg(target_os = "macos")]
    {
        let t_ = theme();
        let hash = fluent::text::hash_str(source);
        struct Pdf {
            host: Option<PdfHost>,
            error: Option<PdfError>,
        }
        let state = use_state(("pdf", key, hash), || {
            let raw = data_url_bytes(source).unwrap_or_default();
            match PdfHost::new(&raw) {
                Ok(host) => Pdf { host, error: None },
                Err(e) => Pdf { host: None, error: Some(e) },
            }
        });
        if let Some(e) = state.with(|s| s.error.clone()) {
            return error_notice(match e {
                PdfError::Password => t("这个 PDF 有密码保护，无法预览。", "This PDF is password-protected and cannot be previewed.").to_owned(),
                PdfError::Unreadable(reason) => tf("无法读取这个 PDF：{reason}", "Could not read this PDF: {reason}", &[("reason", &reason)]),
                PdfError::WithoutText => String::new(),
            });
        }
        if state.with(|s| s.host.is_none()) || app.native_view.is_null() {
            return file_card(key, name, bytes, t("PDF 预览只在窗口里可用", "The PDF preview needs a window"), file.as_ref(), on_reveal);
        }
        let width_fit = use_state(("pdf-fit", key), || true);
        let (scale, page, pages) = state.with(|s| s.host.as_ref().map_or((1.0, 1, 0), |h| (h.scale() as f32, h.current_page(), h.page_count())));
        let surface_state = state.clone();
        let surface = custom::<App>(move |app, ui, r| {
            let parent = app.native_view;
            surface_state.update(|s| {
                if let Some(host) = &mut s.host {
                    host.place(parent, r, overlay_open());
                }
            });
            // Scrolling a native view reaches nothing here, and the page number below it follows the scroll.
            ui.wake_after(0.25);
        })
        .key(("pdf-surface", key))
        .flex1()
        .min_h(0.0)
        .min_w(0.0);
        let step = |up: bool| {
            let (state, fit) = (state.clone(), width_fit.clone());
            move |_: &mut App| {
                fit.set(false);
                state.with(|s| {
                    if let Some(h) = &s.host {
                        h.set_scale(next_zoom_step(&PDF_ZOOM_STEPS, h.scale() as f32, up, 1e-3) as f64);
                    }
                });
            }
        };
        let actual = {
            let (state, fit) = (state.clone(), width_fit.clone());
            move |_: &mut App| {
                fit.set(false);
                state.with(|s| {
                    if let Some(h) = &s.host {
                        h.set_scale(1.0);
                    }
                });
            }
        };
        let fit = {
            let (state, fit) = (state.clone(), width_fit.clone());
            move |_: &mut App| {
                fit.set(true);
                state.with(|s| {
                    if let Some(h) = &s.host {
                        h.fit_width();
                    }
                });
            }
        };
        let mut meta = tf("第 {page} / {total} 页", "Page {page} / {total}", &[("page", &page), ("total", &pages)]);
        if let Some(b) = bytes {
            meta.push_str(" · ");
            meta.push_str(&format_bytes(b));
        }
        let is_fit = width_fit.get();
        let bar = row().h(32.0).shrink0().items_center().justify_center().gap(2.0).px(6.0).border_top(t_.divider).role(Role::Toolbar).aria_label(t("PDF 缩放", "PDF zoom"))
            .child(crate::widgets::tool(("pdf-out", key), "Minus", t("缩小", "Zoom out"), 24.0, 13.0).on_click(step(false)))
            .child(row().key(("pdf-label", key)).center().min_w(44.0).h(24.0).radius(5.0).hover_bg(t_.subtle_hover).tooltip(t("适应宽度", "Fit width")).on_click(fit.clone()).child(text(format!("{}%", (scale * 100.0).round() as i32)).ty(FINE).color(t_.text_primary).nowrap()))
            .child(crate::widgets::tool(("pdf-in", key), "Plus", t("放大", "Zoom in"), 24.0, 13.0).on_click(step(true)))
            .child(crate::widgets::tool_pressed(crate::widgets::tool(("pdf-actual", key), "Scan", t("实际大小", "Actual size"), 24.0, 13.0), !is_fit && (scale - 1.0).abs() < 0.001).on_click(actual))
            .child(crate::widgets::tool_pressed(crate::widgets::tool(("pdf-fit", key), "Maximize", t("适应宽度", "Fit width"), 24.0, 13.0), is_fit).on_click(fit))
            .child(text(meta).ty(FINE).color(t_.text_tertiary).nowrap().pl(6.0));
        col().flex1().min_h(0.0).min_w(0.0).child(surface).child(bar)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (app, source);
        file_card(key, name, bytes, t("这个系统上没有 PDF 预览。", "There is no PDF preview on this system."), file.as_ref(), on_reveal)
    }
}

// ---------------------------------------------------------------- HTML

/// What a page reads, through the host on its file's machine, from a task thread.
struct HostResources {
    task: Task,
    machine: BrowseMachine,
    root: String,
}

impl HtmlResources for HostResources {
    fn read_text(&self, path: &str) -> Option<String> {
        let file: BrowseTextFile = self.task.invoke("browse_read_file", list_args(&self.machine, &join_path(&self.root, path))).ok()?;
        (!file.binary).then_some(file.content)
    }

    fn read_image(&self, path: &str) -> Option<String> {
        let media_type = image_media_type(path)?;
        let file: BrowseFileBytes = self.task.invoke("browse_read_file_bytes", list_args(&self.machine, &join_path(&self.root, path))).ok()?;
        (!file.too_large).then(|| format!("data:{media_type};base64,{}", file.data))
    }
}

#[derive(Default)]
struct HtmlPage {
    composed: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    started: bool,
    version: u64,
}

/// A workspace HTML file, drawn from its markup and its own stylesheets with nothing in it running. In a window that is a
/// web view with scripts off, showing a document nothing in which asks for anything; without one, the same tree drawn as
/// a document. Links are the pane's to follow.
pub fn html_preview(app: &mut App, key: &str, path: &str, content: &str, file: Option<Location>, root: String, on_link: Arc<dyn Fn(&mut App, &str) + Send + Sync>, ctx: &DocContext) -> E {
    let t_ = theme();
    let hash = fluent::text::hash_str(content);
    let page: Rc<HtmlPreviewDocument> = use_state(("html-page", key, hash), || Rc::new(prepare_html_preview(content))).get();
    let banner = page.had_scripts.then(|| row().items_center().gap(6.0).px(10.0).py(5.0).shrink0().border_bottom(t_.divider).bg(t_.card_secondary).child(icon("Info", 12.0).color(t_.text_secondary)).child(text(t("静态预览：页面里的脚本不会运行。", "Static preview: the page's scripts do not run.")).ty(FINE).color(t_.text_secondary)));
    let mut out = col().flex1().min_h(0.0).min_w(0.0).child(banner);
    #[cfg(target_os = "macos")]
    if !app.native_view.is_null() {
        let host = use_state(("html-host", key), {
            let task = app.task();
            let (path, on_link) = (path.to_owned(), on_link.clone());
            move || {
                let wrapped: LinkHandler = Arc::new(move |app, url| on_link(app, &preview_link(url)));
                WebHost::new(task, &path, wrapped)
            }
        });
        let state = use_state(("html-compose", key, hash), HtmlPage::default);
        if !state.with(|s| s.started) {
            state.update(|s| {
                s.started = true;
                s.version = hash;
            });
            let slot = state.with(|s| s.composed.clone());
            let (page_owned, path_owned, machine) = ((*page).clone(), path.to_owned(), file.as_ref().and_then(|f| f.machine.clone()));
            app.spawn(move |task| {
                let page = page_owned;
                let resources = HostResources { task: task.clone(), machine, root };
                let sheets = load_stylesheets(&page.styles, &path_owned, &resources);
                let mut pictures = BTreeMap::new();
                for reference in &page.images {
                    let resolved = resolve_document_reference(&path_owned, reference.split(['?', '#']).next().unwrap_or(""));
                    pictures.insert(reference.clone(), resolved.and_then(|p| resources.read_image(&p)));
                }
                let html = compose_document(&page, &sheets, &pictures, &embed_label);
                *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(html);
                task.post(|_| repaint());
            });
        }
        let surface_path = path.to_owned();
        let surface = custom::<App>(move |app, _ui, r| {
            let parent = app.native_view;
            host.update(|h| {
                let Some(h) = h else { return };
                state.with(|s| {
                    if let Some(html) = s.composed.lock().unwrap_or_else(|p| p.into_inner()).as_deref() {
                        h.load(s.version, html, &surface_path);
                    }
                });
                h.place(parent, r, overlay_open());
            });
        })
        .key(("html-surface", key))
        .flex1()
        .min_h(0.0)
        .min_w(0.0)
        .role(Role::Document)
        .aria_label(page.title.as_deref().unwrap_or(path));
        return out.child(surface);
    }
    let _ = (file, root);
    let open = on_link.clone();
    let drawn_ctx = DocContext {
        key: format!("html/{key}"),
        scroller: format!("html-scroll/{key}"),
        resolve_image: {
            let resolve = ctx.resolve_image.clone();
            Rc::new(move |s: &str| if s.starts_with("data:") { Some(Arc::from(s)) } else { resolve(s) })
        },
        on_link: Rc::new(move |app, href| open(app, href)),
        ..ctx.clone()
    };
    out = out.child(document_scroller(&drawn_ctx.scroller).role(Role::Document).aria_label(page.title.as_deref().unwrap_or(path)).child(document_view(app, &drawn_ctx, "page", content)));
    out
}

// ---------------------------------------------------------------- path opening helper

/// A click on a path outside any pane's reach, for surfaces with no pane of their own.
pub fn open_path_or_reveal(app: &mut App, request: crate::links::PathOpenRequest) {
    path_open::open_path(app, &request);
}
