//! The files pane and file previews, upstream's FilesPane and FilePreview.
//!
//! Public views, for the shell to wire (`E` is `El<App>`; every prop is upstream's, as Rust):
//!
//! ```text
//! pub fn files_pane(app: &mut App, props: FilesPaneProps) -> FilesPaneView     { header, menu_sections, body }, for `side_pane`
//!     props: { pane_id: String, workspaces: Vec<FilesPaneWorkspace>, ssh_machines: Vec<SshMachineConfig>, host_windows: bool,
//!              active: bool, open_request: Option<FilesPaneOpenRequest>,
//!              on_open_request_handled: Option<Rc<dyn Fn(&mut App, u64)>>, on_mention: Option<Rc<dyn Fn(&mut App, FileMention)>> }
//!     Call it every frame the pane is mounted, with `active: false` while it is off screen. State lives in `App::local` by
//!     `pane_id`, so it survives being off screen; `drop_files_pane(app, id)` is what unmounting it did.
//!
//! pub fn file_attachment_preview(app: &mut App, file: &FileAttachment, reads_documents: bool, on_close: Rc<dyn Fn(&mut App)>) -> E
//!     The dialog for one attached file. `reads_documents` is upstream's `PdfReadingContext`.
//!
//! pub fn file_chip(file: &FileAttachment, on_open: Rc<dyn Fn(&mut App)>, on_remove: Option<Rc<dyn Fn(&mut App)>>) -> E
//!     A file's chip in a message's strip: the file half of `ImageStrip`.
//!
//! pub fn file_icon(path: &str, size: f32) -> E                         upstream's `FileIcon`
//!
//! pub fn sketch_overlay(app: &App, props: SketchOverlayProps) -> E     upstream's `SketchOverlay`; `close_sketch(key)` when it goes
//!     props: { key: String, backdrop: Option<Arc<str>>, on_cancel: Rc<dyn Fn(&mut App)>,
//!              on_attach: Rc<dyn Fn(&mut App, String)>, attach_label: Option<String> }
//!
//! pub fn image_viewer(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>, on_reveal: Option<Action>) -> E
//! pub fn pdf_viewer(app: &mut App, key: &str, source: &Arc<str>, bytes: Option<u64>, name: &str, file: Option<Location>, on_reveal: Option<Action>) -> E
//! pub fn audio_player(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>) -> E
//! pub fn csv_table(key: &str, path: &str, content: &str) -> E
//! pub fn notebook_view(app: &App, key: &str, content: &str, ctx: &DocContext) -> E
//! pub fn font_preview(app: &App, key: &str, source: &Arc<str>, name: &str, bytes: Option<u64>) -> E
//! pub fn html_preview(app: &mut App, key: &str, path: &str, content: &str, file: Option<Location>, root: String,
//!                     on_link: Arc<dyn Fn(&mut App, &str) + Send + Sync>, ctx: &DocContext) -> E
//! pub fn numbered_code(key: String, content: &Rc<str>, language: Option<&str>, name: &str, lit_line: Option<u32>,
//!                      on_line_menu: Option<Rc<dyn Fn(&mut App, u32, (f32, f32))>>, jump: Option<u32>) -> E
//!     With `jump`, `code::take_jump_result()` says after the view is built whether it landed.
//! pub fn code_block(key: impl Hash + Clone, code: &str, language: Option<&str>) -> E    a fenced block
//! pub fn document_view(app: &App, ctx: &DocContext, key: impl Hash, source: &str) -> E    the shared `markdown_content`, as this surface uses it
//! ```
//!
//! For the shell, outside any view:
//!
//! - `path_open::{set_path_open_handler, set_path_prefetch_handler, open_path}`: upstream's `setPathOpenHandler`. `PathOpenRequest`
//!   is in `links`. `attachment_drop::{handle_native_drag, drop_zone}`: the window's drag events (after `host.drag_entered` and
//!   `host.drag_dropped`), and what a zone makes of them. `attachments::intake_attachments` takes a message's files.
//! - `fluent::Input::zoom_modifier`: set it while Command or Control is held, for a pinch or Cmd+wheel to zoom a picture.
//! - `App::native_view` must be the window's content view for PDF and HTML previews; headless they fall back.
#![allow(clippy::too_many_arguments)]

pub mod browser;
pub mod icon_table;
pub mod icons;
pub mod viewers;
pub mod format;
pub mod links;
pub mod notebook;
pub mod widgets;
pub mod pane;
pub mod path_open;
pub mod images;
pub mod code;
pub mod html_preview;
pub mod markdown;
pub mod pane_view;
pub mod viewer;
pub mod previews;
pub mod sketch;
pub mod attachment_drop;
pub mod attachment_preview;
pub mod attachments;
pub mod audio;
pub mod native;

pub use pane::{drop_files_pane, FileMention, FilesPaneOpenRequest, FilesPaneProps, FilesPaneView, FilesPaneWorkspace};
pub use pane_view::files_pane;
