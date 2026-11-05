//! How files reach the composer: upstream's `<input type="file">`, `clipboardData.files` and `lib/attachmentDrop.ts`.
//!
//! The file dialog and the system clipboard go through [`AttachPorts`], so a headless run neither opens a window nor reads
//! what the person running it copied. A drag arrives as `Input::hovered_file`, `hovered_paths` and `dropped`; the host says while
//! it is in the air whether each path is a folder and what the file starts with, which is what lets a zone refuse an archive
//! before anything is dropped.

use mewrk_client::prelude::*;
use mewrk_client::runs::attachments::{MAX_FILE_ATTACHMENT_PDF_BYTES, MAX_IMAGE_ATTACHMENT_BYTES, MAX_TEXT_FILE_UPLOAD_BYTES};
use mewrk_client::runs::{AttachmentRejection, AttachmentRejectionReason, ComposerFile};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// What the system clipboard holds besides text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardContents {
    /// A picture, as straight RGBA.
    pub image: Option<(Vec<u8>, u32, u32)>,
    /// Files copied in a file manager.
    pub paths: Vec<PathBuf>,
}

impl ClipboardContents {
    pub fn is_empty(&self) -> bool {
        self.image.is_none() && self.paths.is_empty()
    }
}

type PickFiles = Arc<dyn Fn() -> Option<Vec<PathBuf>> + Send + Sync>;

/// The operating system's half of attaching.
pub struct AttachPorts {
    /// The native file dialog. Runs on a background thread: it blocks until it is answered.
    pub pick_files: PickFiles,
    pub read_clipboard: Rc<dyn Fn() -> ClipboardContents>,
}

impl Default for AttachPorts {
    fn default() -> Self {
        Self { pick_files: Arc::new(|| rfd::FileDialog::new().pick_files()), read_clipboard: Rc::new(system_clipboard) }
    }
}

/// The ports, for a test or a tool to replace.
pub fn attach_ports(app: &mut App) -> &mut AttachPorts {
    app.local::<AttachPorts>()
}

fn system_clipboard() -> ClipboardContents {
    let Ok(mut clipboard) = arboard::Clipboard::new() else { return ClipboardContents::default() };
    let paths = clipboard.get().file_list().unwrap_or_default();
    // A file copied in a file manager also offers its icon as a picture, which is not what was copied.
    let image = if paths.is_empty() { clipboard.get_image().ok().map(|image| (image.bytes.into_owned(), image.width as u32, image.height as u32)) } else { None };
    ClipboardContents { image, paths }
}

/// What a file says it is, by its first bytes. A file named anything can be anything.
fn media_type_of(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        ""
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |name| name.to_string_lossy().into_owned())
}

/// Reads files for the composer. What cannot be read is reported rather than dropped, and what is obviously too big to be any
/// kind of attachment is not read at all.
pub fn read_local_files(paths: &[PathBuf]) -> (Vec<ComposerFile>, Vec<AttachmentRejection>) {
    let limit = MAX_IMAGE_ATTACHMENT_BYTES.max(MAX_FILE_ATTACHMENT_PDF_BYTES);
    let (mut files, mut rejected) = (Vec::new(), Vec::new());
    for path in paths {
        let name = file_name(path);
        let reject = |reason| AttachmentRejection { name: Some(name.clone()), reason };
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_dir() => rejected.push(reject(AttachmentRejectionReason::Directory)),
            Ok(meta) if meta.len() > limit => rejected.push(reject(AttachmentRejectionReason::TooLarge)),
            Ok(_) => match std::fs::read(path) {
                Ok(bytes) => files.push(ComposerFile { media_type: media_type_of(&bytes).to_owned(), name, bytes }),
                Err(_) => rejected.push(reject(AttachmentRejectionReason::Failed)),
            },
            Err(_) => rejected.push(reject(AttachmentRejectionReason::Failed)),
        }
    }
    (files, rejected)
}

/// The add menu's file input: the picker takes anything, and what the message cannot carry is turned away with a reason once
/// the files are read, the same as a paste or a drop.
pub fn pick_and_attach(app: &mut App, conversation_id: &str) {
    let pick = attach_ports(app).pick_files.clone();
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |task| {
        let Some(paths) = pick().filter(|paths| !paths.is_empty()) else { return };
        let (files, rejected) = read_local_files(&paths);
        task.post(move |app| app.add_composer_attachments(&conversation_id, files, rejected));
    });
}

/// Encodes a clipboard picture the way a browser hands one over: a PNG named `image.png`.
fn encode_clipboard_image(rgba: Vec<u8>, width: u32, height: u32) -> Option<ComposerFile> {
    let picture = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut bytes = Vec::new();
    picture.write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png).ok()?;
    Some(ComposerFile { name: "image.png".to_owned(), media_type: "image/png".to_owned(), bytes })
}

/// Sends what the clipboard held to the composer. Encoding a screenshot takes a while, so it happens off the UI thread.
pub fn attach_clipboard_contents(app: &mut App, conversation_id: &str, contents: ClipboardContents) {
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |task| {
        let (mut files, mut rejected) = read_local_files(&contents.paths);
        if let Some((rgba, width, height)) = contents.image {
            match encode_clipboard_image(rgba, width, height) {
                Some(file) => files.push(file),
                None => rejected.push(AttachmentRejection { name: Some("image.png".to_owned()), reason: AttachmentRejectionReason::Failed }),
            }
        }
        task.post(move |app| app.add_composer_attachments(&conversation_id, files, rejected));
    });
}

// ---------------------------------------------------------------- what a drag carries

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DragVerdict {
    Image,
    Pdf,
    Text,
    /// Might be accepted once read.
    Unknown,
    Directory,
    Unsupported,
    Empty,
    TooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragItem {
    pub name: String,
    pub verdict: DragVerdict,
    /// The native path.
    pub path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DroppedPathProbe {
    pub path: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub sniff: String,
}

/// The host looked at the path itself: it knows a folder from a file, and what the file starts with.
pub fn drag_item_from_probe(probe: &DroppedPathProbe) -> DragItem {
    let fits = |limit: u64, verdict| if probe.size <= limit { verdict } else { DragVerdict::TooLarge };
    let verdict = match (probe.kind.as_str(), probe.sniff.as_str()) {
        ("directory", _) => DragVerdict::Directory,
        ("file", "image") => fits(MAX_IMAGE_ATTACHMENT_BYTES, DragVerdict::Image),
        ("file", "pdf") => fits(MAX_FILE_ATTACHMENT_PDF_BYTES, DragVerdict::Pdf),
        ("file", "text") => fits(MAX_TEXT_FILE_UPLOAD_BYTES, DragVerdict::Text),
        ("file", "empty") => DragVerdict::Empty,
        _ => DragVerdict::Unsupported,
    };
    DragItem { name: probe.name.clone(), verdict, path: Some(probe.path.clone()) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DragRefusal {
    Verdict(DragVerdict),
    ImageInputUnavailable,
}

/// What a drop zone would make of a drag, given whether it can take images.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragSummary {
    pub accepted: usize,
    /// Items that might be accepted once read.
    pub undecided: usize,
    pub rejected: Vec<(DragRefusal, usize)>,
}

pub fn summarize_drag(items: &[DragItem], image_input: bool) -> DragSummary {
    let mut summary = DragSummary::default();
    for item in items {
        let refusal = match item.verdict {
            DragVerdict::Image if !image_input => DragRefusal::ImageInputUnavailable,
            DragVerdict::Image | DragVerdict::Pdf | DragVerdict::Text => {
                summary.accepted += 1;
                continue;
            }
            DragVerdict::Unknown => {
                summary.undecided += 1;
                continue;
            }
            verdict => DragRefusal::Verdict(verdict),
        };
        match summary.rejected.iter_mut().find(|(known, _)| *known == refusal) {
            Some((_, count)) => *count += 1,
            None => summary.rejected.push((refusal, 1)),
        }
    }
    summary
}

/// Items a drop turns away before reading anything, as the rejections the notice lists.
pub fn rejections_for_drag_items(items: &[DragItem], image_input: bool) -> Vec<AttachmentRejection> {
    items
        .iter()
        .filter_map(|item| {
            let reason = match item.verdict {
                DragVerdict::Image if !image_input => AttachmentRejectionReason::ImageInputUnavailable,
                DragVerdict::Directory => AttachmentRejectionReason::Directory,
                DragVerdict::Unsupported => AttachmentRejectionReason::Unsupported,
                DragVerdict::Empty => AttachmentRejectionReason::Empty,
                DragVerdict::TooLarge => AttachmentRejectionReason::TooLarge,
                _ => return None,
            };
            Some(AttachmentRejection { name: Some(item.name.clone()).filter(|name| !name.is_empty()), reason })
        })
        .collect()
}

fn unknown_items(paths: &[String]) -> Vec<DragItem> {
    paths.iter().map(|path| DragItem { name: path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned(), verdict: DragVerdict::Unknown, path: Some(path.clone()) }).collect()
}

/// The composer's view of the drag.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DropZoneState {
    /// A file drag is somewhere over the window.
    pub dragging: bool,
    /// ...and it is over this zone.
    pub over: bool,
    /// What is being dragged; `None` while it is still being looked at.
    pub items: Option<Vec<DragItem>>,
}

#[derive(Default)]
struct DragProbe {
    /// The paths asked about, so one hover asks once.
    paths: Vec<String>,
    generation: u64,
    items: Option<Vec<DragItem>>,
}

/// One frame of the drag, from a leaf inside the zone. Answers the zone's state for the next frame's view. `disabled` zones take
/// nothing and say nothing. The window reports no position during a file drag, so a pointer that left the window before the
/// drag came in counts as over the one zone there is.
pub fn watch_drag(app: &mut App, ui: &Ui<'_>, zone: Rect, conversation_id: &str, image_input: bool, disabled: bool) -> DropZoneState {
    let over_zone = ui.input.pointer.is_none_or(|(x, y)| zone.contains(x, y));
    if !ui.input.dropped.is_empty() {
        let paths: Vec<String> = ui.input.dropped.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        let items = {
            let probe = app.local::<DragProbe>();
            probe.generation += 1;
            probe.paths.clear();
            probe.items.take()
        };
        if !disabled && over_zone {
            // Told here because a window reports a multi-file drop a path at a time, and the host only remembers the last list.
            app.host.drag_dropped(&ui.input.dropped);
            deliver_drop(app, conversation_id, paths, items, image_input);
        }
        return DropZoneState::default();
    }
    if !ui.input.hovered_file {
        let probe = app.local::<DragProbe>();
        if !probe.paths.is_empty() || probe.items.is_some() {
            probe.generation += 1;
            probe.paths.clear();
            probe.items = None;
        }
        return DropZoneState::default();
    }
    if disabled {
        return DropZoneState::default();
    }
    let paths: Vec<String> = ui.input.hovered_paths.iter().map(|path| path.to_string_lossy().into_owned()).collect();
    let asked = {
        let probe = app.local::<DragProbe>();
        if probe.paths != paths {
            probe.paths = paths.clone();
            probe.items = None;
            probe.generation += 1;
            Some(probe.generation)
        } else {
            None
        }
    };
    if let (Some(generation), false) = (asked, paths.is_empty()) {
        app.host.drag_entered(&ui.input.hovered_paths);
        app.invoke::<Vec<DroppedPathProbe>>("dropped_paths_probe", json!({ "paths": paths }), move |app, result| {
            let probe = app.local::<DragProbe>();
            if probe.generation != generation {
                return;
            }
            let paths = probe.paths.clone();
            probe.items = Some(match result {
                Ok(probes) => probes.iter().map(drag_item_from_probe).collect(),
                Err(_) => unknown_items(&paths),
            });
            repaint();
        });
    }
    DropZoneState { dragging: true, over: over_zone, items: app.local::<DragProbe>().items.clone() }
}

fn deliver_drop(app: &mut App, conversation_id: &str, paths: Vec<String>, items: Option<Vec<DragItem>>, image_input: bool) {
    let conversation_id = conversation_id.to_owned();
    app.spawn(move |task| {
        // A drop can outrun the probe; ask again rather than read blind.
        let known = items.unwrap_or_else(|| {
            task.invoke::<Vec<DroppedPathProbe>>("dropped_paths_probe", json!({ "paths": paths })).map_or_else(|_| unknown_items(&paths), |probes| probes.iter().map(drag_item_from_probe).collect())
        });
        let mut rejected = rejections_for_drag_items(&known, image_input);
        let mut files = Vec::new();
        for item in known.iter().filter(|item| item.path.is_some() && (matches!(item.verdict, DragVerdict::Pdf | DragVerdict::Text | DragVerdict::Unknown) || (item.verdict == DragVerdict::Image && image_input))) {
            match read_dropped_file(&task, item.path.as_deref().unwrap_or("")) {
                Ok(file) => files.push(file),
                Err(_) => rejected.push(AttachmentRejection { name: Some(item.name.clone()).filter(|name| !name.is_empty()), reason: AttachmentRejectionReason::Failed }),
            }
        }
        task.post(move |app| app.add_composer_attachments(&conversation_id, files, rejected));
    });
}

fn read_dropped_file(task: &Task, path: &str) -> Result<ComposerFile, String> {
    #[derive(Deserialize)]
    struct Dropped {
        name: String,
        data: String,
    }
    let dropped: Dropped = task.invoke("dropped_file_read", json!({ "path": path }))?;
    let bytes = base64_simd::STANDARD.decode_to_vec(dropped.data.as_bytes()).map_err(|error| error.to_string())?;
    Ok(ComposerFile { media_type: media_type_of(&bytes).to_owned(), name: dropped.name, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(verdict: DragVerdict) -> DragItem {
        DragItem { name: "f".into(), verdict, path: None }
    }

    #[test]
    fn a_drag_summary_counts_what_a_drop_would_do() {
        let items = [item(DragVerdict::Image), item(DragVerdict::Pdf), item(DragVerdict::Text), item(DragVerdict::Unknown), item(DragVerdict::Directory), item(DragVerdict::Directory), item(DragVerdict::TooLarge)];
        let summary = summarize_drag(&items, true);
        assert_eq!((summary.accepted, summary.undecided), (3, 1));
        assert_eq!(summary.rejected, [(DragRefusal::Verdict(DragVerdict::Directory), 2), (DragRefusal::Verdict(DragVerdict::TooLarge), 1)]);
        let blind = summarize_drag(&items, false);
        assert_eq!(blind.accepted, 2);
        assert_eq!(blind.rejected[0], (DragRefusal::ImageInputUnavailable, 1));
    }

    #[test]
    fn the_host_verdicts_become_drag_items() {
        let probe = |kind: &str, sniff: &str, size: u64| DroppedPathProbe { path: "/a".into(), name: "a".into(), kind: kind.into(), size, sniff: sniff.into() };
        assert_eq!(drag_item_from_probe(&probe("directory", "none", 0)).verdict, DragVerdict::Directory);
        assert_eq!(drag_item_from_probe(&probe("file", "image", 10)).verdict, DragVerdict::Image);
        assert_eq!(drag_item_from_probe(&probe("file", "image", MAX_IMAGE_ATTACHMENT_BYTES + 1)).verdict, DragVerdict::TooLarge);
        assert_eq!(drag_item_from_probe(&probe("file", "text", MAX_TEXT_FILE_UPLOAD_BYTES + 1)).verdict, DragVerdict::TooLarge);
        assert_eq!(drag_item_from_probe(&probe("file", "binary", 10)).verdict, DragVerdict::Unsupported);
        assert_eq!(drag_item_from_probe(&probe("file", "empty", 0)).verdict, DragVerdict::Empty);
    }

    #[test]
    fn reads_what_it_can_and_says_why_for_the_rest() {
        let dir = std::env::temp_dir().join(format!("mewrk-composer-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("notes.md");
        std::fs::write(&file, "# notes").unwrap();
        let (files, rejected) = read_local_files(&[file.clone(), dir.clone(), dir.join("missing")]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "notes.md");
        assert_eq!(rejected.iter().map(|r| r.reason).collect::<Vec<_>>(), [AttachmentRejectionReason::Directory, AttachmentRejectionReason::Failed]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clipboard_picture_is_a_png_named_as_a_browser_names_it() {
        let file = encode_clipboard_image(vec![255; 16], 2, 2).expect("encodes");
        assert_eq!(file.name, "image.png");
        assert_eq!(media_type_of(&file.bytes), "image/png");
        assert!(encode_clipboard_image(vec![0; 3], 2, 2).is_none());
    }
}
