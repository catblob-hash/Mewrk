//! `PathChoiceMenu.tsx` and the path-open handler of `App.tsx`: a file path clicked anywhere in a transcript opens in the
//! file pane, and a file a turn changed opens on its diff in the review pane when Git tracks the change.
//!
//! A path that can only be one place opens there at once. One that could be in several (a relative path in a conversation
//! with several workspaces, an absolute one in a conversation on several machines) is looked up on all of them in one
//! request, started when the pointer reaches the link so the answer is usually in by the click; found in one, it opens
//! there, found in several, a menu at the link asks which.

use std::collections::HashMap;
use std::sync::{Arc as StdArc, Mutex};
use std::time::{Duration, Instant};

use mewrk_client::prelude::*;
use mewrk_client::workspaces::host_is_windows;
use mewrk_pane_files::browser::{browse_path, format_address, is_windows_path, location_key, machine_label, probe_args, resolve_path, same_machine, AddressContext, BrowseEntryKind, BrowseMachine, BrowseProbeResult, BrowseProbeTarget};
use mewrk_pane_files::links::PathOpenRequest;
use mewrk_pane_files::pane::FilesPaneWorkspace;
use mewrk_pane_git::git_review_lists_path;

use crate::actions::{show_in_files, show_in_review};
use crate::panes::files_pane_workspaces;
use crate::state::{shell, PathChoice, PathChoiceAnchor, PathChoiceRow};

/// A probe's answer is reused for this long: a path is probed when the pointer reaches it and again when it is clicked, and
/// the click finds the hover's answer here, already on its way.
const PATH_PROBE_REUSE: Duration = Duration::from_millis(8_000);
/// How long a click waits before showing the menu saying it is still looking.
const PATH_CHOICE_PATIENCE: Duration = Duration::from_millis(150);

/// Everywhere a path the transcript wrote could be.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The workspace the path was resolved in; `None` for an absolute path, which names a machine.
    pub workspace: Option<FilesPaneWorkspace>,
    pub machine: BrowseMachine,
    pub path: String,
}

fn is_absolute(path: &str) -> bool {
    path.starts_with('/') || path.starts_with('~') || is_windows_path(path)
}

/// `candidatesFor`.
pub fn candidates_for(request: &PathOpenRequest, workspaces: &[FilesPaneWorkspace]) -> Vec<Candidate> {
    let PathOpenRequest { path, base_dir, workspace, .. } = request;
    if let Some(number) = workspace.filter(|number| *number >= 1) {
        // A tool call that named its workspace wrote its path against that workspace.
        let Some(named) = workspaces.iter().find(|entry| entry.number == number) else { return Vec::new() };
        let base = if number == 1 { base_dir.as_deref().unwrap_or(&named.path) } else { &named.path };
        return vec![Candidate { workspace: Some(named.clone()), machine: named.machine.clone(), path: resolve_path(&browse_path(base), path) }];
    }
    if is_absolute(path) {
        let mut machines: Vec<BrowseMachine> = Vec::new();
        for machine in workspaces.iter().map(|entry| entry.machine.clone()).chain(std::iter::once(None)) {
            if !machines.iter().any(|known| same_machine(known, &machine)) {
                machines.push(machine);
            }
        }
        return machines.into_iter().map(|machine| Candidate { workspace: None, machine, path: browse_path(path) }).collect();
    }
    if workspaces.is_empty() {
        return base_dir.as_ref().map_or_else(Vec::new, |base| vec![Candidate { workspace: None, machine: None, path: resolve_path(&browse_path(base), path) }]);
    }
    // Workspace 1 is the one the transcript's working directory belongs to.
    workspaces
        .iter()
        .map(|entry| {
            let base = if entry.number == 1 { base_dir.as_deref().unwrap_or(&entry.path) } else { &entry.path };
            Candidate { workspace: Some(entry.clone()), machine: entry.machine.clone(), path: resolve_path(&browse_path(base), path) }
        })
        .collect()
}

/// `workspaceRelativePath`: where a path a model wrote lives inside the open workspace, or `None` when it lives somewhere
/// the review pane cannot reach. A path that escapes the workspace is not a workspace path, and silently clamping it at the
/// root would address the wrong file.
pub fn workspace_relative_path(raw: &str, base_dir: Option<&str>, root: Option<&str>) -> Option<String> {
    // A network location is refused rather than normalized: the host refuses it too, and `//server/share` collapsed into a
    // rooted path would name something else. The extended prefix goes, since a path a model writes never carries one.
    fn normalize(path: &str) -> Option<String> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return None;
        }
        let separated = trimmed.replace('\\', "/");
        let extended = separated.starts_with("//?/");
        if extended && separated[4..].to_ascii_lowercase().starts_with("unc/") {
            return None;
        }
        if !extended && separated.starts_with("//") {
            return None;
        }
        let mut out = if extended { separated[4..].to_owned() } else { separated };
        while out.contains("//") {
            out = out.replace("//", "/");
        }
        while out.len() > 1 && out.ends_with('/') {
            out.pop();
        }
        (!out.is_empty()).then_some(out)
    }
    fn collapse(path: &str) -> Option<Vec<&str>> {
        let mut result = Vec::new();
        for segment in path.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    result.pop()?;
                }
                other => result.push(other),
            }
        }
        Some(result)
    }
    fn absolute(path: &str) -> bool {
        path.starts_with('/') || is_windows_path(path)
    }
    fn drive(path: &str) -> bool {
        is_windows_path(path)
    }
    let path = normalize(raw)?;
    let root = root.and_then(normalize);
    let windows = root.as_deref().map_or_else(|| drive(&path), drive);
    let fold = |s: &str| if windows { s.to_lowercase() } else { s.to_owned() };
    if !absolute(&path) {
        let base = base_dir.and_then(normalize);
        return match (&base, &root) {
            (Some(base), Some(root)) if fold(base) != fold(root) => workspace_relative_path(&format!("{base}/{path}"), None, Some(root)),
            _ => collapse(&path).filter(|segments| !segments.is_empty()).map(|segments| segments.join("/")),
        };
    }
    let root = root?;
    let (root_segments, path_segments) = (collapse(&root)?, collapse(&path)?);
    if path_segments.len() <= root_segments.len() || root_segments.iter().zip(&path_segments).any(|(r, p)| fold(r) != fold(p)) {
        return None;
    }
    Some(path_segments[root_segments.len()..].join("/"))
}

type Answer = Vec<BrowseProbeResult>;

/// One probe, finished or on its way.
#[derive(Default)]
struct ProbeSlot {
    answer: Option<Answer>,
    /// Who is waiting, run on the UI thread when it lands.
    waiting: Vec<Box<dyn FnOnce(&mut App, Answer)>>,
}

#[derive(Default)]
struct PathProbes {
    probes: HashMap<String, (Instant, ProbeSlot)>,
}

fn probe(app: &mut App, candidates: &[Candidate], done: impl FnOnce(&mut App, Answer) + 'static) {
    let key = candidates.iter().map(|candidate| location_key(&candidate.machine, &candidate.path)).collect::<Vec<_>>().join("\n");
    let store = app.local::<PathProbes>();
    store.probes.retain(|_, (at, _)| at.elapsed() <= PATH_PROBE_REUSE);
    if let Some((_, slot)) = store.probes.get_mut(&key) {
        match slot.answer.clone() {
            Some(answer) => return done(app, answer),
            None => {
                slot.waiting.push(Box::new(done));
                return;
            }
        }
    }
    store.probes.insert(key.clone(), (Instant::now(), ProbeSlot { answer: None, waiting: vec![Box::new(done)] }));
    let targets: Vec<BrowseProbeTarget> = candidates.iter().map(|candidate| BrowseProbeTarget { machine: candidate.machine.clone(), path: candidate.path.clone() }).collect();
    let fallback: Answer = candidates.iter().map(|candidate| BrowseProbeResult { path: candidate.path.clone(), kind: None, reached: false }).collect();
    app.invoke::<Answer>("browse_probe_paths", probe_args(&targets, false), move |app, result| {
        let answer = result.unwrap_or(fallback);
        let waiting = match app.local::<PathProbes>().probes.get_mut(&key) {
            Some((_, slot)) => {
                slot.answer = Some(answer.clone());
                std::mem::take(&mut slot.waiting)
            }
            None => Vec::new(),
        };
        for wait in waiting {
            wait(app, answer.clone());
        }
    });
}

/// `setPathPrefetchHandler`: the pointer has rested on a path.
pub fn prefetch_path(app: &mut App, request: &PathOpenRequest) {
    if request.machine.is_some() || request.review {
        return;
    }
    let workspaces = files_pane_workspaces(app);
    let candidates = candidates_for(request, &workspaces);
    if candidates.len() > 1 {
        probe(app, &candidates, |_, _| {});
    }
}

/// `setPathOpenHandler`.
pub fn open_path(app: &mut App, request: &PathOpenRequest) -> bool {
    let line = request.line;
    let new_page = request.new_page;
    let Some(conversation) = app.active_conversation_id() else { return false };
    // A path in a document the file pane shows: on that document's machine.
    if let Some(machine) = &request.machine {
        let path = match &request.base_dir {
            Some(base) => resolve_path(&browse_path(base), &request.path),
            None => browse_path(&request.path),
        };
        show_in_files(app, machine.clone(), path, line, new_page, false);
        return true;
    }
    let workspaces = files_pane_workspaces(app);
    let number = request.workspace.filter(|number| *number >= 1).unwrap_or(1);
    if request.review
        && let Some(member) = crate::effects::git_members(app).into_iter().find(|entry| entry.member as u32 == number)
        && let Some(named) = workspaces.iter().find(|entry| entry.number == number).cloned()
    {
        let base = if number > 1 { Some(named.path.clone()) } else { request.base_dir.clone().or_else(|| crate::chat::timeline_path_base_dir(app)) };
        if let Some(relative) = workspace_relative_path(&request.path, base.as_deref(), Some(&named.path)) {
            // Only Git knows whether it tracks the file, so the pane is picked once it has answered.
            let target_path = resolve_path(&browse_path(base.as_deref().unwrap_or(&named.path)), &request.path);
            let target_machine = named.machine.clone();
            let page = crate::effects::review_pages(app).into_iter().find(|entry| entry.member == member.member);
            let (member_target, base_oid, member_number) = (member.target.clone(), member.worktree.as_ref().map(|w| w.base_oid.clone()).filter(|oid| !oid.is_empty()), member.member);
            let (key, surface) = (member.key.clone(), member.surface_key.clone());
            let conversation_id = conversation.clone();
            app.spawn(move |task| {
                let snapshot = match page {
                    Some(page) => Some(page.snapshot),
                    None => {
                        let target = member_target.clone();
                        let (sender, receiver) = std::sync::mpsc::channel();
                        task.post(move |app| {
                            mewrk_pane_git::git_refresh(app, &key, &surface, &target, move |_, result| {
                                let _ = sender.send(result.ok().flatten().flatten());
                            });
                        });
                        receiver.recv().ok().flatten()
                    }
                };
                let listed = snapshot.is_some_and(|snapshot| git_review_lists_path(&task, &member_target, &snapshot, &relative, base_oid.as_deref()).unwrap_or(false));
                task.post(move |app| {
                    if app.active_conversation_id().as_deref() != Some(conversation_id.as_str()) {
                        return;
                    }
                    if listed { show_in_review(app, member_number, relative) } else { show_in_files(app, target_machine, target_path, line, new_page, false) }
                });
            });
            return true;
        }
    }
    let candidates = candidates_for(request, &workspaces);
    match candidates.len() {
        0 => return false,
        1 => {
            let only = candidates.into_iter().next().expect("one");
            show_in_files(app, only.machine, only.path, line, new_page, false);
            return true;
        }
        _ => {}
    }
    let anchor = request.anchor.map_or_else(
        || {
            let (w, h) = window_size();
            PathChoiceAnchor::Point(w / 2.0, h / 3.0)
        },
        PathChoiceAnchor::Rect,
    );
    // Most answers are in before anyone could see a menu; one that is not gets the menu at once, saying it is still looking,
    // rather than a click that seems to do nothing.
    let settled = StdArc::new(Mutex::new(false));
    let waiting = settled.clone();
    app.set_timer(PATH_CHOICE_PATIENCE, move |app| {
        if !*waiting.lock().unwrap_or_else(|p| p.into_inner()) {
            show_path_choice(app, anchor, Vec::new(), None);
        }
    });
    let order = candidates.clone();
    probe(app, &candidates, move |app, results| {
        *settled.lock().unwrap_or_else(|p| p.into_inner()) = true;
        if app.active_conversation_id().as_deref() != Some(conversation.as_str()) {
            return;
        }
        let hits: Vec<(Candidate, BrowseProbeResult)> = order.iter().cloned().zip(results.iter().cloned()).filter(|(_, result)| result.kind.is_some()).collect();
        // Two workspaces on one machine can be the same directory under two names.
        let mut distinct: Vec<(Candidate, BrowseProbeResult)> = Vec::new();
        for hit in hits {
            if !distinct.iter().any(|other| location_key(&other.0.machine, &other.1.path) == location_key(&hit.0.machine, &hit.1.path)) {
                distinct.push(hit);
            }
        }
        if distinct.len() >= 2 {
            let label = if distinct.iter().any(|(candidate, _)| candidate.workspace.is_some()) { t("在哪个工作区打开", "Open in which workspace") } else { t("在哪台机器上打开", "Open on which machine") };
            let rows = choice_rows(app, &distinct, line, new_page);
            show_path_choice(app, anchor, rows, Some(label.to_owned()));
            return;
        }
        close_path_choice(app);
        // Nowhere: the first place it could have been, where the pane says what it found, on a machine that answered, since
        // one that is switched off would only keep the pane waiting.
        let (machine, path) = match distinct.into_iter().next() {
            Some((candidate, result)) => (candidate.machine, result.path),
            None => {
                let reached = order.iter().zip(&results).find(|(_, result)| result.reached).map(|(candidate, _)| candidate).unwrap_or(&order[0]);
                (reached.machine.clone(), reached.path.clone())
            }
        };
        show_in_files(app, machine, path, line, new_page, false);
    });
    true
}

fn choice_rows(app: &mut App, hits: &[(Candidate, BrowseProbeResult)], line: Option<u32>, new_page: bool) -> Vec<PathChoiceRow> {
    let context = AddressContext { ssh_machines: app.doc.assets.execution_environments.ssh_machines.clone(), host_windows: host_is_windows(mewrk_client::machine_shells::host_platform()) };
    hits.iter()
        .map(|(candidate, result)| {
            let machine_name = machine_label(&candidate.machine, &context.ssh_machines, t("本机", "This machine"));
            PathChoiceRow {
                id: location_key(&candidate.machine, &result.path),
                label: candidate.workspace.as_ref().map_or_else(|| machine_name.clone(), |workspace| workspace.path.clone()),
                label_is_path: candidate.workspace.is_some(),
                description: format_address(&candidate.machine, &result.path, &context),
                is_directory: result.kind == Some(BrowseEntryKind::Directory),
                machine: candidate.machine.clone(),
                path: result.path.clone(),
                line,
                new_page,
            }
        })
        .collect()
}

/// `showPathChoice`: opens the menu at `anchor`, or replaces what the open one lists.
pub fn show_path_choice(app: &mut App, anchor: PathChoiceAnchor, rows: Vec<PathChoiceRow>, label: Option<String>) {
    shell(app).path_choice = Some(PathChoice { anchor, rows, label });
    repaint();
}

/// `closePathChoice`.
pub fn close_path_choice(app: &mut App) {
    shell(app).path_choice = None;
    repaint();
}

/// `PathChoiceMenu`.
pub fn path_choice_menu(app: &mut App) -> Option<E> {
    use mewrk_ui_common::context_menu::{context_menu, ContextMenuAnchor, ContextMenuItem, ContextMenuProps, ContextMenuSection};
    let t_ = theme();
    let (anchor, rows, label) = {
        let choice = shell(app).path_choice.as_ref()?;
        let rows: Vec<(String, String, bool, String, bool, BrowseMachine, String, Option<u32>, bool)> = choice.rows.iter().map(|row| (row.id.clone(), row.label.clone(), row.label_is_path, row.description.clone(), row.is_directory, row.machine.clone(), row.path.clone(), row.line, row.new_page)).collect();
        (choice.anchor, rows, choice.label.clone())
    };
    let items: Vec<ContextMenuItem> = rows
        .into_iter()
        .map(|(id, label, label_is_path, description, is_directory, machine, path, line, new_page)| {
            let named = path.clone();
            let glyph = move || if is_directory { icon("Folder", 13.0).color(theme().text_secondary) } else { mewrk_pane_files::icons::file_icon(&named, 13.0) };
            let mut item = ContextMenuItem::new(id, label).icon_el(glyph).description(description).description_is_path().on_select(move |app: &mut App| {
                close_path_choice(app);
                show_in_files(app, machine.clone(), path.clone(), line, new_page, false);
            });
            if label_is_path {
                item = item.label_is_path();
            }
            item
        })
        .collect();
    let sections = if items.is_empty() { Vec::new() } else { vec![ContextMenuSection { id: "targets".to_owned(), label, items }] };
    let anchor = match anchor {
        PathChoiceAnchor::Rect(rect_values) => ContextMenuAnchor::Box { rect: rect(rect_values[0], rect_values[1], rect_values[2], rect_values[3]), align_end: false },
        PathChoiceAnchor::Point(x, y) => ContextMenuAnchor::Point { x, y },
    };
    let placeholder = row().gap(6.0).items_center().child(crate::widgets::spinner(12.0)).child(text(t("正在查找…", "Looking…")).ty(CAPTION).color(t_.text_secondary));
    Some(context_menu("path-choice", ContextMenuProps::new(anchor, t("打开位置", "Open location"), close_path_choice, sections).placeholder(placeholder)))
}
