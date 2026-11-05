//! Files dragged onto the window, and which drop zone is under them (upstream's `lib/attachmentDrop.ts`). The window's
//! owner hands the operating system's drag to `handle_native_drag`; the host says, while the drag is still in the air,
//! whether each path is a folder and what the file starts with, which is what lets a zone refuse an archive before anything
//! is dropped. A zone is any element by its key: `drop_zone` reports the drag and registers what a drop does.

use crate::attachments::*;
use mewrk_client::prelude::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragPoint {
    pub x: f32,
    pub y: f32,
}

/// What the window reports. macOS reports points from the top-left of the view, the same numbers `rect_of` uses.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeDragEvent {
    Enter { paths: Vec<String>, position: DragPoint },
    Over { position: DragPoint },
    Drop { paths: Vec<String>, position: DragPoint },
    Leave,
}

type OnDrop = Rc<dyn Fn(&mut App, Vec<IncomingFile>, Vec<AttachmentRejection>)>;

struct Zone {
    rect_key: u64,
    image_input: bool,
    disabled: bool,
    on_drop: OnDrop,
}

/// One zone's view of the drag.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DropZoneState {
    /// A file drag is somewhere over the window.
    pub dragging: bool,
    /// ...and it is over this zone.
    pub over: bool,
    /// What is being dragged; `None` while it is still being looked at.
    pub items: Option<Vec<DragItem>>,
}

#[derive(Default)]
struct Drag {
    zones: HashMap<String, Zone>,
    active: bool,
    items: Option<Vec<DragItem>>,
    zone: Option<String>,
    /// Bumped on every enter, so a slow probe cannot land on a later drag.
    generation: u64,
}

fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()).unwrap_or(path).to_owned()
}

fn unknown_items(paths: &[String]) -> Vec<DragItem> {
    paths.iter().map(|path| DragItem { name: basename(path), verdict: DragVerdict::Unknown, path: Some(path.clone()) }).collect()
}

fn zone_at(drag: &Drag, position: DragPoint) -> Option<String> {
    drag.zones.iter().find(|(_, z)| !z.disabled && fluent::el::rect_by_id(z.rect_key).is_some_and(|r| r.contains(position.x, position.y))).map(|(id, _)| id.clone())
}

/// Makes the element keyed `key` a place files can be dropped, and reports the drag. The options are read at drop time, so
/// a zone whose model changes mid-drag judges the drop by the model it has then.
pub fn drop_zone(app: &mut App, key: impl std::hash::Hash, image_input: bool, disabled: bool, on_drop: OnDrop) -> DropZoneState {
    let rect_key = hash_key(&key);
    let id = rect_key.to_string();
    let drag = app.local::<Drag>();
    drag.zones.insert(id.clone(), Zone { rect_key, image_input, disabled, on_drop });
    DropZoneState { dragging: drag.active && !disabled, over: drag.active && !disabled && drag.zone.as_deref() == Some(id.as_str()), items: drag.items.clone() }
}

/// A zone that is no longer on screen.
pub fn remove_drop_zone(app: &mut App, key: impl std::hash::Hash) {
    app.local::<Drag>().zones.remove(&hash_key(key).to_string());
}

/// Handles one native drag event. The shell calls this; `App::host.drag_entered` / `drag_dropped` must have been told first,
/// since the host only answers for paths its own window saw.
pub fn handle_native_drag(app: &mut App, event: NativeDragEvent) {
    match event {
        NativeDragEvent::Enter { paths, position } => {
            let drag = app.local::<Drag>();
            // A drag that carries no files (text, a link) is not ours to show anything for.
            if paths.is_empty() {
                drag.active = false;
                drag.items = None;
                drag.zone = None;
                return;
            }
            drag.generation += 1;
            let generation = drag.generation;
            drag.active = true;
            drag.items = None;
            drag.zone = zone_at(drag, position);
            app.invoke::<Vec<DroppedPathProbe>>("dropped_paths_probe", json!({ "paths": paths }), move |app, result| {
                let drag = app.local::<Drag>();
                if generation != drag.generation || !drag.active {
                    return;
                }
                drag.items = Some(match result {
                    Ok(probes) => probes.iter().map(drag_item_from_probe).collect(),
                    Err(_) => unknown_items(&paths),
                });
                repaint();
            });
        }
        NativeDragEvent::Over { position } => {
            let drag = app.local::<Drag>();
            if drag.active {
                drag.zone = zone_at(drag, position);
            }
        }
        NativeDragEvent::Leave => {
            let drag = app.local::<Drag>();
            drag.active = false;
            drag.items = None;
            drag.zone = None;
        }
        NativeDragEvent::Drop { paths, position } => {
            let drag = app.local::<Drag>();
            let zone = zone_at(drag, position);
            let items = drag.items.take();
            let generation = drag.generation;
            drag.active = false;
            drag.zone = None;
            let Some(zone) = zone.filter(|_| !paths.is_empty()) else { return };
            let image_input = drag.zones.get(&zone).is_some_and(|z| z.image_input);
            let (id, slot): (String, std::sync::Arc<std::sync::Mutex<Option<(Vec<IncomingFile>, Vec<AttachmentRejection>)>>>) = (zone, Default::default());
            let writer = slot.clone();
            PENDING.with(|p| p.borrow_mut().push((id, slot)));
            app.spawn(move |task| {
                // A drop can outrun the probe; ask again rather than read blind.
                let known = items.unwrap_or_else(|| probe_dropped_paths(&task, &paths).map_or_else(|_| unknown_items(&paths), |probes| probes.iter().map(drag_item_from_probe).collect()));
                if task.update(move |app| app.local::<Drag>().generation) != generation {
                    return;
                }
                let mut pre_rejected = rejections_for_drag_items(&known, image_input);
                let mut files = Vec::new();
                for item in known.iter().filter(|i| i.path.is_some() && (matches!(i.verdict, DragVerdict::Pdf | DragVerdict::Text | DragVerdict::Unknown) || (i.verdict == DragVerdict::Image && image_input))) {
                    match read_dropped_file(&task, item.path.as_deref().unwrap_or("")) {
                        Ok(file) => files.push(file),
                        Err(_) => pre_rejected.push(AttachmentRejection { name: Some(item.name.clone()).filter(|n| !n.is_empty()), reason: RejectionReason::Failed }),
                    }
                }
                *writer.lock().unwrap_or_else(|p| p.into_inner()) = Some((files, pre_rejected));
                task.post(deliver_drops);
            });
        }
    }
    repaint();
}

type PendingDrop = (String, std::sync::Arc<std::sync::Mutex<Option<(Vec<IncomingFile>, Vec<AttachmentRejection>)>>>);

thread_local! {
    static PENDING: std::cell::RefCell<Vec<PendingDrop>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn deliver_drops(app: &mut App) {
    let ready: Vec<PendingDrop> = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let (ready, rest): (Vec<_>, Vec<_>) = p.drain(..).partition(|(_, s)| s.lock().unwrap_or_else(|x| x.into_inner()).is_some());
        *p = rest;
        ready
    });
    for (zone, slot) in ready {
        let Some((files, rejected)) = slot.lock().unwrap_or_else(|p| p.into_inner()).take() else { continue };
        let Some(on_drop) = app.local::<Drag>().zones.get(&zone).map(|z| z.on_drop.clone()) else { continue };
        on_drop(app, files, rejected);
    }
}
