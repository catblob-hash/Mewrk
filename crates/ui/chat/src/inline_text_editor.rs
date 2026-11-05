//! `components/InlineTextEditor.tsx`: replaces a text card's body while it is being edited, and stands in for a card while one is
//! inserted.
//!
//! The draft lives in `use_state` under the editor's key, so a stream flush re-running the view does not reset it. The text field
//! is a `TextEdit` driven directly rather than `fluent::el::field`, because a user message folds long pastes into tags, and that
//! needs `EditHooks`.

use std::hash::Hash;
use std::ops::Range;
use std::path::PathBuf;

use fluent::edit::EditHooks;
use mewrk_client::model::{FileAttachment, ImageAttachment};
use mewrk_client::prelude::*;
use mewrk_client::runs::attachments::merge_file_attachments;
use mewrk_client::runs::image_short_ids::{text_without_appended_image_placeholders, with_image_placeholders};
use mewrk_client::runs::{composer::expand_pasted_texts, AttachmentRejection, AttachmentRejectionReason, PastedText};
use mewrk_ui_composer::composer_add_files;
use mewrk_ui_composer::attach::{attach_ports, drag_item_from_probe, rejections_for_drag_items, ClipboardContents, DragItem, DragVerdict, DropZoneState, DroppedPathProbe};
use mewrk_ui_composer::feedback::{attachment_drop_overlay, rejection_reason_label};
use mewrk_ui_composer::pasted_text::{copy_selection, normalize_pasted_text, paste_action, pasted_text_ranges, PasteAction};

use crate::image_strip::{image_strip_with, ImageStripProps};
use crate::kit::{action_button, action_button_disabled};

/// Upstream's `InsertableContextKind`: the model has no such type, since only the renderer's insert menu names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InsertKind {
    System,
    User,
    Reasoning,
    Tool,
    Assistant,
}

pub type SaveText = Rc<dyn Fn(&mut App, String, Vec<ImageAttachment>, Vec<FileAttachment>)>;

pub struct AddedAttachments {
    pub images: Vec<ImageAttachment>,
    pub files: Vec<FileAttachment>,
    pub rejected: Vec<AttachmentRejection>,
}

/// `lib/fileAttachments`' `AddMessageAttachments`: paths picked, pasted or dropped; `done` receives what was accepted.
pub type AddAttachments = Rc<dyn Fn(&mut App, Vec<PathBuf>, Vec<ImageAttachment>, Box<dyn FnOnce(&mut App, AddedAttachments)>)>;

pub struct TextEditorProps {
    pub kind: InsertKind,
    pub content: String,
    pub images: Vec<ImageAttachment>,
    pub files: Vec<FileAttachment>,
    /// Shown only when inserting; an edited card already names itself.
    pub show_kind: bool,
    /// Whether this message's model can see images; decides how a drag of pictures reads. `None` is upstream's default, no.
    pub image_input: Option<bool>,
    pub on_cancel: Handler<App>,
    pub on_save: SaveText,
    /// Attaches picked, pasted or dropped files to this user message, alongside what it already carries. Absent where nothing can
    /// be attached (a template that is read-only); what the message already has can still be removed.
    pub on_add_attachments: Option<AddAttachments>,
}

/// `.context-text-editor`, whose line is whole: a box rounds to whole pixels, and text a fraction taller than its box scrolls.
const CONTENT: TypeStyle = TypeStyle { size: 13.0, line: 21.0, weight: Weight::Regular, mono: false, italic: false, serif: false };
/// `.editor-kind`.
const KIND: TypeStyle = TypeStyle { size: 10.5, line: 14.0, weight: Weight::Semibold, mono: false, italic: false, serif: false };
const NOTICE: TypeStyle = TypeStyle { size: 11.5, line: 16.7, weight: Weight::Regular, mono: false, italic: false, serif: false };
const NOTICE_STRONG: TypeStyle = TypeStyle { size: 11.5, line: 16.7, weight: Weight::Semibold, mono: false, italic: false, serif: false };

pub fn kind_title(kind: InsertKind) -> &'static str {
    match kind {
        InsertKind::System => t("系统提示词上下文", "System prompt context"),
        // A tool has no text editor; upstream's type leaves it out. It reads as what an empty card would.
        InsertKind::User | InsertKind::Tool => t("用户输入", "User input"),
        InsertKind::Reasoning => t("明文思考字段", "Plain-text reasoning field"),
        InsertKind::Assistant => t("模型回复", "Model reply"),
    }
}

pub fn kind_placeholder(kind: InsertKind) -> &'static str {
    match kind {
        InsertKind::System => t("输入要在这个位置生效的系统指令…", "Enter a system instruction to apply at this point…"),
        InsertKind::User | InsertKind::Tool => t("输入用户消息…", "Enter a user message…"),
        InsertKind::Reasoning => t("输入推理过程或计划…", "Enter reasoning or a plan…"),
        InsertKind::Assistant => t("输入模型回复…", "Enter a model reply…"),
    }
}

fn kind_icon(kind: InsertKind) -> &'static str {
    match kind {
        InsertKind::System => "Shield",
        InsertKind::User | InsertKind::Tool => "UserRound",
        InsertKind::Reasoning => "BrainCircuit",
        InsertKind::Assistant => "Bot",
    }
}

struct Draft {
    images: Vec<ImageAttachment>,
    files: Vec<FileAttachment>,
    pending: usize,
    rejected: Vec<AttachmentRejection>,
    pastes: Vec<PastedText>,
    drag: DropZoneState,
    /// The paths the host was asked about, so one hover asks once.
    probed: Vec<String>,
    /// Bumped when a drag ends or changes, so a slow probe cannot land on a later one.
    generation: u64,
}

/// A long paste folds into a tag here as it does in the composer, and saves as its text. Keys the field hears first go to its
/// owner, which is what `onKeyDown` was. A paste into anything but a user message is the text as it is.
struct Hooks<'a> {
    app: &'a mut App,
    kind: InsertKind,
    draft: Local<Draft>,
    on_cancel: Handler<App>,
    save: Rc<dyn Fn(&mut App, String)>,
    add: Option<AddAttachments>,
}

impl EditHooks for Hooks<'_> {
    fn key(&mut self, edit: &mut TextEdit, press: &KeyPress) -> bool {
        match press.key {
            Key::Enter if press.cmd || press.ctrl => {
                (self.save)(self.app, edit.text.clone());
                true
            }
            Key::Escape => {
                (self.on_cancel)(self.app);
                true
            }
            _ => false,
        }
    }

    fn copied(&mut self, edit: &TextEdit, range: Range<usize>) -> Option<String> {
        if self.kind != InsertKind::User {
            return None;
        }
        let draft = self.draft.borrow();
        (!draft.pastes.is_empty()).then(|| copy_selection(&edit.text, range, &draft.pastes))
    }

    fn paste(&mut self, edit: &mut TextEdit, text: Option<String>) -> bool {
        if let Some(add) = self.add.as_ref().filter(|_| self.kind == InsertKind::User) {
            let read = attach_ports(self.app).read_clipboard.clone();
            let contents = read();
            if !contents.is_empty() {
                attach_clipboard(self.app, &self.draft, add, contents);
                // A clipboard carrying both keeps its text; the file rides along.
                return text.is_none();
            }
        }
        let Some(text) = text else { return true };
        if self.kind != InsertKind::User {
            edit.insert(&normalize_pasted_text(&text));
            return true;
        }
        let action = paste_action(&text, &self.draft.borrow().pastes);
        match action {
            PasteAction::Native => edit.insert(&normalize_pasted_text(&text)),
            PasteAction::Insert { text, added } => {
                self.draft.update(|draft| draft.pastes.extend(added));
                edit.insert(&text);
            }
        }
        true
    }

    fn atoms(&mut self, text: &str) -> Vec<Range<usize>> {
        if self.kind != InsertKind::User {
            return Vec::new();
        }
        pasted_text_ranges(text, &self.draft.borrow().pastes).into_iter().map(|range| range.start..range.end).collect()
    }
}

/// The field's identity, as `focus_request(("field", ..))` takes it.
pub fn text_field_key(editor: u64) -> (&'static str, u64) {
    ("text-editor", editor)
}

/// `attach`: a batch that lands after others were added or removed merges with what is there by then.
fn attach(app: &mut App, draft: &Local<Draft>, add: &AddAttachments, paths: Vec<PathBuf>, pre_rejected: Vec<AttachmentRejection>) {
    attach_keeping(app, draft, add, paths, pre_rejected, None);
}

/// `scratch` is a directory made for this batch, which is done with once the surface has answered.
fn attach_keeping(app: &mut App, draft: &Local<Draft>, add: &AddAttachments, paths: Vec<PathBuf>, pre_rejected: Vec<AttachmentRejection>, scratch: Option<PathBuf>) {
    if paths.is_empty() && pre_rejected.is_empty() {
        return;
    }
    draft.update(|d| d.pending += 1);
    let existing = draft.with(|d| d.images.clone());
    let landing = draft.clone();
    add(
        app,
        paths,
        existing,
        Box::new(move |_, result| {
            landing.update(|d| {
                for image in result.images {
                    if !d.images.iter().any(|known| known.id == image.id) {
                        d.images.push(image);
                    }
                }
                d.files = merge_file_attachments(&d.files, &result.files);
                let mut rejected = pre_rejected;
                rejected.extend(result.rejected);
                d.rejected = rejected;
                d.pending -= 1;
            });
            if let Some(scratch) = scratch {
                let _ = std::fs::remove_dir_all(scratch);
            }
        }),
    );
}

/// A clipboard picture as a file of its own, named the way a browser hands one over: `AddAttachments` takes paths, and a picture on the
/// clipboard has none. In a directory of its own, so two pastes never share a name.
fn write_clipboard_image(rgba: Vec<u8>, width: u32, height: u32) -> Option<(PathBuf, PathBuf)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let picture = image::RgbaImage::from_raw(width, height, rgba)?;
    let directory = std::env::temp_dir().join(format!("mewrk-pasted-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join("image.png");
    picture.save_with_format(&path, image::ImageFormat::Png).ok()?;
    Some((path, directory))
}

/// What the clipboard held besides text. Encoding a screenshot takes a while, so it happens off the UI thread.
fn attach_clipboard(app: &mut App, draft: &Local<Draft>, add: &AddAttachments, contents: ClipboardContents) {
    let ticket = park(draft, add);
    app.spawn(move |task| {
        let ClipboardContents { image, mut paths } = contents;
        let mut rejected = Vec::new();
        let mut scratch = None;
        if let Some((rgba, width, height)) = image {
            match write_clipboard_image(rgba, width, height) {
                Some((path, directory)) => {
                    paths.push(path);
                    scratch = Some(directory);
                }
                None => rejected.push(AttachmentRejection { name: Some("image.png".to_owned()), reason: AttachmentRejectionReason::Failed }),
            }
        }
        task.post(move |app| deliver_parked(app, ticket, paths, rejected, scratch));
    });
}

/// One frame of a file drag over this editor. The window reports no position during a drag, so "over" means the pointer was last
/// seen here. The composer's own zone takes a drag whose pointer left the window, so this one does not.
fn watch_drag(app: &mut App, ui: &Ui<'_>, k: u64, zone: Rect, draft: &Local<Draft>, add: &AddAttachments, image_input: bool) {
    // Not `pointer_in`, which also asks the clip, and this leaf's is its own pixel.
    let over = ui.input.pointer.is_some_and(|(x, y)| zone.contains(x, y));
    if !ui.input.dropped.is_empty() {
        let items = draft.with(|d| d.drag.items.clone());
        draft.update(|d| {
            d.drag = DropZoneState::default();
            d.probed.clear();
            d.generation += 1;
        });
        if over {
            let (paths, rejected) = match items {
                Some(items) => (accepted_paths(&items, image_input), rejections_for_drag_items(&items, image_input)),
                // A drop can outrun the probe; whatever reads the files turns away what they turn out to be.
                None => (ui.input.dropped.clone(), Vec::new()),
            };
            attach(app, draft, add, paths, rejected);
        }
        return;
    }
    if !ui.input.hovered_file {
        if draft.with(|d| d.drag.dragging || !d.probed.is_empty()) {
            draft.update(|d| {
                d.drag = DropZoneState::default();
                d.probed.clear();
                d.generation += 1;
            });
        }
        return;
    }
    let paths: Vec<String> = ui.input.hovered_paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
    if draft.with(|d| d.probed != paths) {
        let generation = draft.update(|d| {
            d.probed = paths.clone();
            d.drag.items = None;
            d.generation += 1;
            d.generation
        });
        if !paths.is_empty() {
            app.host.drag_entered(&ui.input.hovered_paths);
            // Handlers must be `Send`, so the answer waits in the app for the next frame's leaf to take.
            app.invoke::<Vec<DroppedPathProbe>>("dropped_paths_probe", json!({ "paths": paths }), move |app, result| {
                app.local::<ProbeInbox>().0.insert(k, (generation, result.map(|probes| probes.iter().map(drag_item_from_probe).collect())));
                repaint();
            });
        }
    }
    let answer = app.local::<ProbeInbox>().0.remove(&k);
    if let Some((generation, result)) = answer
        && draft.with(|d| d.generation == generation)
    {
        draft.update(|d| {
            d.drag.items = Some(result.unwrap_or_else(|_| d.probed.iter().map(|path| DragItem { name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned(), verdict: DragVerdict::Unknown, path: Some(path.clone()) }).collect()));
        });
    }
    if draft.with(|d| !d.drag.dragging || d.drag.over != over) {
        draft.update(|d| {
            d.drag.dragging = true;
            d.drag.over = over;
        });
    }
}

/// Where a probe's answer waits, by editor: the callback must be `Send`, and a draft is not.
#[derive(Default)]
struct ProbeInbox(std::collections::HashMap<u64, (u64, Result<Vec<DragItem>, String>)>);

/// What a drop takes, by what the host said about each path.
fn accepted_paths(items: &[DragItem], image_input: bool) -> Vec<PathBuf> {
    items
        .iter()
        .filter(|item| matches!(item.verdict, DragVerdict::Pdf | DragVerdict::Text | DragVerdict::Unknown) || (item.verdict == DragVerdict::Image && image_input))
        .filter_map(|item| item.path.as_ref().map(PathBuf::from))
        .collect()
}

/// `AttachmentNotice`, keyed by its editor: the composer's own has one fixed key, and both can be on screen at once.
fn attachment_notice(k: u64, rejected: &[AttachmentRejection], on_dismiss: impl Fn(&mut App) + 'static) -> Option<E> {
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
    let lines = groups.into_iter().map(|(reason, names)| {
        let names = (!names.is_empty()).then(|| text(format!("{}:", names.join(t("、", ", ")))).ty(NOTICE).color(t_.text_primary).ellipsis().min_w(0.0));
        row().wrap().gap_x(6.0).child(names).child(text(rejection_reason_label(reason)).ty(NOTICE).color(t_.text_secondary))
    });
    Some(
        row()
            .key(("text-editor-notice", k))
            .items_start()
            .gap(7.0)
            .pl(9.0)
            .pr(6.0)
            .py(7.0)
            .radius(8.0)
            .bg(t_.caution_bg)
            .border(t_.caution.fade(0.4))
            .role(Role::Alert)
            .child(icon("TriangleAlert", 14.0).color(t_.caution).mt(1.0).shrink0())
            .child(col().flex1().min_w(0.0).child(text(title).ty(NOTICE_STRONG).color(t_.text_primary)).child(div().mt(2.0).children(lines)))
            .child(row().key(("text-editor-notice-dismiss", k)).center().size(20.0, 20.0).radius(5.0).shrink0().hover_bg(t_.subtle_hover).tooltip(t("关闭提示", "Dismiss")).on_click(on_dismiss).child(icon("X", 13.0).color(t_.text_secondary))),
    )
}

/// `ComposerAddFiles`. A file dialog blocks whoever opens it, so it gets a thread, and the answer comes back through the same `attach`
/// as a drop.
fn add_files_button(k: u64, image_input: bool, draft: &Local<Draft>, add: &AddAttachments) -> E {
    let (draft, add) = (draft.clone(), add.clone());
    composer_add_files(("text-editor-add", k), false, image_input, None, Rc::new(move |app: &mut App| pick_files(app, &draft, &add)))
}

fn pick_files(app: &mut App, draft: &Local<Draft>, add: &AddAttachments) {
    let pick = attach_ports(app).pick_files.clone();
    let ticket = park(draft, add);
    app.spawn(move |task| {
        let paths = pick().filter(|paths| !paths.is_empty()).unwrap_or_default();
        task.post(move |app| deliver_parked(app, ticket, paths, Vec::new(), None));
    });
}

/// A `task.post` must be `Send` and a draft is not, so what a thread's answer goes to waits here under a ticket.
type Parked = (u64, Local<Draft>, AddAttachments);

thread_local! {
    static PARKED: std::cell::RefCell<(u64, Vec<Parked>)> = const { std::cell::RefCell::new((0, Vec::new())) };
}

fn park(draft: &Local<Draft>, add: &AddAttachments) -> u64 {
    PARKED.with(|parked| {
        let mut parked = parked.borrow_mut();
        parked.0 += 1;
        let ticket = parked.0;
        parked.1.push((ticket, draft.clone(), add.clone()));
        ticket
    })
}

fn deliver_parked(app: &mut App, ticket: u64, paths: Vec<PathBuf>, rejected: Vec<AttachmentRejection>, scratch: Option<PathBuf>) {
    let parked = PARKED.with(|parked| {
        let mut parked = parked.borrow_mut();
        parked.1.iter().position(|(at, _, _)| *at == ticket).map(|index| parked.1.remove(index))
    });
    if let Some((_, draft, add)) = parked {
        attach_keeping(app, &draft, &add, paths, rejected, scratch);
    }
}

/// The editor's two halves. A timeline row lays `bar` over the end of its header, where its edit and delete buttons were
/// (`.timeline-row .inline-text-editor__bar`, absolute against the row), and `body` in its details.
pub struct TextEditorParts {
    pub bar: E,
    pub body: E,
}

pub fn inline_text_editor_parts(key: impl Hash + Clone, props: TextEditorProps) -> TextEditorParts {
    let t_ = theme();
    let k = hash_key(key);
    let TextEditorProps { kind, content, images, files, show_kind, image_input, on_cancel, on_save, on_add_attachments } = props;
    let image_input = image_input.unwrap_or(false);
    let is_user = kind == InsertKind::User;
    let draft = use_state(("text-editor-draft", k), || Draft { images: images.clone(), files: files.clone(), pending: 0, rejected: Vec::new(), pastes: Vec::new(), drag: DropZoneState::default(), probed: Vec::new(), generation: 0 });
    let edit = use_state(("text-editor-edit", k), || {
        let mut edit = TextEdit::new();
        // `[Image #N]` is the model's way of pointing at a thumbnail this box already shows, so the box never shows the token itself:
        // it is put back on save.
        edit.set_text(if is_user { text_without_appended_image_placeholders(&content, &images) } else { content.clone() });
        edit
    });
    let field_id = hash_key(("field", text_field_key(k)));
    let opened = use_state(("text-editor-opened", k), || false);
    if !opened.get() {
        // `autoFocus`, once: focus that moved on stays where the person took it.
        opened.set(true);
        focus_request(("field", text_field_key(k)));
    }

    let (savable, has_any, pending, rejected) = {
        let d = draft.borrow();
        let keeps_attachments = is_user && (!d.images.is_empty() || !d.files.is_empty());
        (d.pending == 0 && (!edit.borrow().text.trim().is_empty() || keeps_attachments), !d.images.is_empty() || !d.files.is_empty() || d.pending > 0, d.pending, d.rejected.clone())
    };
    let accepts_attachments = is_user && on_add_attachments.is_some();

    // Reads the text it is handed: a field's key hooks run while the field's own state is borrowed.
    let save: Rc<dyn Fn(&mut App, String)> = {
        let (draft, on_save) = (draft.clone(), on_save.clone());
        Rc::new(move |app: &mut App, content: String| {
            let (images, files, pastes, pending) = draft.with(|d| (d.images.clone(), d.files.clone(), d.pastes.clone(), d.pending));
            if pending > 0 || !(!content.trim().is_empty() || (is_user && (!images.is_empty() || !files.is_empty()))) {
                return;
            }
            let expanded = expand_pasted_texts(&content, &pastes);
            let text = expanded.trim();
            if !is_user {
                on_save(app, text.to_owned(), Vec::new(), Vec::new());
                return;
            }
            let saved = with_image_placeholders(text, &images);
            on_save(app, saved, images, files);
        })
    };

    let field_view = {
        let measured = edit.clone();
        let (painted, hooks_draft, hooks_cancel, hooks_save, hooks_add) = (edit.clone(), draft.clone(), on_cancel.clone(), save.clone(), on_add_attachments.clone());
        let placeholder = kind_placeholder(kind);
        custom(move |app: &mut App, ui: &mut Ui<'_>, r: Rect| {
            let mut hooks = Hooks { app, kind, draft: hooks_draft.clone(), on_cancel: hooks_cancel.clone(), save: hooks_save.clone(), add: hooks_add.clone() };
            let mut edit = painted.borrow_mut();
            if is_user {
                // The tag behind a folded paste.
                edit.set_atom_fill(Some(ui.theme.subtle_pressed));
            }
            let response = edit.show_with(ui, field_id, r, placeholder, &CONTENT, false, &mut hooks);
            if response.changed {
                repaint();
            }
        })
        .key(field_id)
        .editable()
        .min_w(0.0)
        .aria_label(kind_title(kind))
        .measure(move |_, ui, known, avail| {
            let w = known.unwrap_or(if avail.is_finite() { avail } else { 240.0 });
            let h = measured.borrow_mut().measure(ui, &CONTENT, w.max(1.0));
            (w, h.max(CONTENT.line))
        })
    };
    let focused = focused_id() == field_id;
    // `.plain-field`, which grows with its text and never scrolls inside a frame of its own.
    let plain_field = div().key(("text-editor-frame", k)).min_w(0.0).px(5.0).py(3.0).radius(5.0).hover_bg(t_.subtle_hover).when(focused, |e| e.bg(t_.subtle_hover).border(t_.accent)).child(field_view);

    let notice = is_user.then(|| {
        let dismissing = draft.clone();
        attachment_notice(k, &rejected, move |_| dismissing.update(|d| d.rejected.clear()))
    });
    let notice = notice.flatten();
    let strip = (is_user && has_any).then(|| {
        let d = draft.borrow();
        let (removing_image, removing_file) = (draft.clone(), draft.clone());
        let on_remove: Rc<dyn Fn(&mut App, &str)> = Rc::new(move |_, id: &str| removing_image.update(|d| d.images.retain(|image| image.id != id)));
        let on_remove_file: Rc<dyn Fn(&mut App, &str)> = Rc::new(move |_, id: &str| removing_file.update(|d| d.files.retain(|file| file.id != id)));
        image_strip_with(("text-editor-images", k), ImageStripProps { images: &d.images, files: &d.files, compact: true, busy: pending > 0, on_remove: Some(on_remove), on_remove_file: Some(on_remove_file) }).map(|strip| div().mb(10.0).child(strip))
    });
    let footer = on_add_attachments.as_ref().filter(|_| accepts_attachments).map(|add| row().key(("text-editor-footer", k)).items_center().child(add_files_button(k, image_input, &draft, add)));

    // Drag state is read in a leaf because that is where the input is, and shown by the next frame's view.
    let body_key = ("text-editor-body", k);
    let drag_state = draft.with(|d| d.drag.clone());
    let watcher = on_add_attachments.as_ref().filter(|_| accepts_attachments).map(|add| {
        let (add, draft) = (add.clone(), draft.clone());
        custom(move |app: &mut App, ui: &mut Ui<'_>, _| {
            if let Some(zone) = rect_of(body_key) {
                watch_drag(app, ui, k, zone, &draft, &add, image_input);
            }
        })
        .size(1.0, 1.0)
        .absolute()
        .pass_through()
    });
    let overlay = is_user.then(|| attachment_drop_overlay(&drag_state, image_input)).flatten();
    let ready_outline = drag_state.dragging && !drag_state.over;

    let kind_chip = show_kind.then(|| {
        row().key(("text-editor-kind", k)).items_center().gap(6.0).h(29.0).px(8.0).radius(7.0).bg(t_.subtle_hover).child(icon(kind_icon(kind), 16.0).color(t_.text_secondary)).child(text(kind_title(kind)).ty(KIND).color(t_.text_secondary).nowrap())
    });
    let cancel = {
        let on_cancel = on_cancel.clone();
        action_button(("text-editor-cancel", k), "X", t("取消", "Cancel"), move |app| on_cancel(app))
    };
    let save_button = if savable {
        let (save, edit) = (save.clone(), edit.clone());
        action_button(("text-editor-save", k), "Check", t("保存", "Save"), move |app| {
            let content = edit.borrow().text.clone();
            save(app, content);
        })
    } else {
        action_button_disabled(("text-editor-save", k), "Check", t("保存", "Save"))
    };
    let bar = row().key(("text-editor-bar", k)).items_center().gap(8.0).min_h(24.0).pr(4.0).shrink0().child(kind_chip).child(row().key(("text-editor-actions", k)).items_center().gap(1.0).ml_auto().child(cancel).child(save_button));

    // "A reply's spans the column, as the reply does, for its bar to reach the end", and everything else in it keeps its side.
    let fields = col()
        .gap(6.0)
        .min_w(0.0)
        .when(kind == InsertKind::Assistant, |e| e.with_style(|s| s.max_size.width = taffy::style::Dimension::percent(0.86)))
        .child(notice)
        .child(strip.flatten())
        .child(plain_field)
        .child(footer);
    let body = div().key(body_key).child(fields).child(watcher).child(overlay).when(ready_outline, |e| e.border(t_.control_strong_stroke).radius(12.0));
    TextEditorParts { bar, body }
}

pub fn inline_text_editor(key: impl Hash + Clone, props: TextEditorProps) -> E {
    let kind = props.kind;
    let k = hash_key(key.clone());
    let TextEditorParts { bar, body } = inline_text_editor_parts(key, props);
    let root = col().key(("text-editor", k)).gap(6.0).radius(12.0).min_w(0.0);
    // `.inline-text-editor--user`: the bare editor stands in for a card that had its own width and side, so it keeps them, or an edit
    // reads as a full-width strip where the card was.
    let root = if kind == InsertKind::User { root.w_pct(78.0).ml_auto() } else { root };
    root.child(bar).child(body)
}
