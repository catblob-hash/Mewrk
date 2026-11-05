//! `ProjectDialog.tsx`: creates or edits a project: a display name and one or more workspaces, each a directory on a machine
//! (this one, a WSL distribution, or an SSH machine).
//!
//! Every path comes back from a picker that authorizes it (the host's folder dialog here, the remote browser elsewhere),
//! because the host refuses to save a project naming a directory no picker returned.

use std::collections::{HashMap, HashSet};

use mewrk_client::model::{AttachedWorkspace, RunTarget, SshMachineConfig, WorkspaceKind};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::{host_is_windows, is_deleted_machine, registered_project_workspaces, run_env_key};
use mewrk_settings_global::machine_dialogs::{ssh_machine_dialog, WslDistro};

use super::machines::{machine_window, save_ssh_machine};
use super::remote_picker::{remote_directory_picker, RemotePickerProps};
use crate::menu::{MenuAction, MenuItem, MenuSection, PopoverMenu, Submenu};
use crate::widgets::{boundary_note, machine_icon_name, dialog_button, Look, spinner, tool_button, DialogWindow};

/// `MAX_PROJECT_WORKSPACES`.
pub const MAX_PROJECT_WORKSPACES: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    /// Stable key; rows are reordered by removal.
    pub id: String,
    pub machine: Option<RunTarget>,
    pub path: String,
    /// The primary of a project being edited, while its machine exists: its machine and path cannot change.
    pub locked: bool,
}

/// A drive-letter or UNC path: Windows compares those without regard to case.
fn looks_like_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/')) || path.starts_with("\\\\")
}

/// `projectWorkspaceKey`: identity of a row for duplicate detection: the machine, then the path as that machine compares it.
pub fn project_workspace_key(machine: Option<&RunTarget>, path: &str) -> String {
    let trimmed = path.trim();
    format!("{} {}", run_env_key(machine), if looks_like_windows_path(trimmed) { trimmed.to_lowercase() } else { trimmed.to_owned() })
}

/// `duplicates`: ids of the rows that repeat an earlier row's directory.
pub fn duplicate_rows(rows: &[ProjectRow]) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut found = HashSet::new();
    for row in rows {
        if row.path.trim().is_empty() {
            continue;
        }
        if !seen.insert(project_workspace_key(row.machine.as_ref(), &row.path)) {
            found.insert(row.id.clone());
        }
    }
    found
}

/// `canSubmit`.
pub fn can_submit(rows: &[ProjectRow]) -> bool {
    rows.iter().all(|row| !row.path.trim().is_empty()) && duplicate_rows(rows).is_empty()
}

/// The dialog's `useState`s, kept in `app.local` by dialog since the host's answers come back after a frame.
#[derive(Default, Clone)]
pub struct ProjectForm {
    pub name: String,
    pub rows: Vec<ProjectRow>,
    pub next_row: u64,
    pub distros: Option<Vec<WslDistro>>,
    /// Machines created from this dialog, until the catalog carries them.
    pub created_machines: Vec<SshMachineConfig>,
    pub machine_dialog_row: Option<String>,
    /// The machine whose settings its gear opened; `Some(None)` is this one.
    pub machine_settings: Option<Option<RunTarget>>,
    pub remote_picker_row: Option<String>,
    pub picking_row: Option<String>,
    pub pick_errors: HashMap<String, String>,
}

#[derive(Default)]
struct Forms(HashMap<String, ProjectForm>);

pub fn form<'a>(app: &'a mut App, key: &str) -> &'a mut ProjectForm {
    app.local::<Forms>().0.entry(key.to_owned()).or_default()
}

/// `setRowMachine`: a row that changes machine gives up its path, which named a directory on the other one.
fn set_row_machine(app: &mut App, key: &str, row_id: &str, machine: Option<RunTarget>) {
    let f = form(app, key);
    if let Some(row) = f.rows.iter_mut().find(|row| row.id == row_id)
        && !row.locked
        && run_env_key(row.machine.as_ref()) != run_env_key(machine.as_ref())
    {
        row.machine = machine;
        row.path = String::new();
    }
    f.pick_errors.remove(row_id);
}

fn set_row_path(app: &mut App, key: &str, row_id: &str, path: String) {
    if let Some(row) = form(app, key).rows.iter_mut().find(|row| row.id == row_id && !row.locked) {
        row.path = path;
    }
}

fn pick_local(app: &mut App, key: &str, row_id: &str) {
    let f = form(app, key);
    f.picking_row = Some(row_id.to_owned());
    f.pick_errors.remove(row_id);
    let (key, row) = (key.to_owned(), row_id.to_owned());
    app.pick_workspace_directory(move |app, result| {
        match result {
            Ok(Some(path)) => set_row_path(app, &key, &row, path),
            Ok(None) => {}
            Err(error) => {
                let message = if error.is_empty() { t("未知错误", "Unknown error").to_owned() } else { error };
                form(app, &key).pick_errors.insert(row.clone(), tf("无法打开目录选择器：{error}", "Unable to open the directory picker: {error}", &[("error", &message)]));
            }
        }
        let f = form(app, &key);
        if f.picking_row.as_deref() == Some(row.as_str()) {
            f.picking_row = None;
        }
        repaint();
    });
}

/// `deleteMachine`: deleting a machine takes it off every row still pointing at it, back to an unpicked local row, the locked
/// first one included, which now has to be chosen again.
fn delete_machine_from_rows(app: &mut App, key: &str, machine_id: &str) {
    let target = run_env_key(Some(&RunTarget::Ssh { machine_id: machine_id.to_owned() }));
    let f = form(app, key);
    f.created_machines.retain(|machine| machine.id != machine_id);
    for row in f.rows.iter_mut().filter(|row| run_env_key(row.machine.as_ref()) == target) {
        row.machine = None;
        row.path = String::new();
        row.locked = false;
    }
}

fn settings_action(key: &str, machine: Option<RunTarget>, name: &str) -> MenuAction {
    let key = key.to_owned();
    MenuAction::new(tf("{name} 的设置", "Settings for {name}", &[("name", &name)]), "Settings", move |app: &mut App| form(app, &key).machine_settings = Some(machine.clone()))
}

fn menu_items(key: &str, row: &ProjectRow, machines: &[SshMachineConfig], distros: Option<&[WslDistro]>, show_wsl: bool) -> Vec<MenuItem> {
    let current = run_env_key(row.machine.as_ref());
    let on_machine = |machine: Option<RunTarget>| {
        let (key, row_id) = (key.to_owned(), row.id.clone());
        move |app: &mut App| set_row_machine(app, &key, &row_id, machine.clone())
    };
    let wsl_children: Vec<MenuItem> = match distros {
        None => vec![MenuItem::new("wsl:loading", t("正在枚举 WSL 发行版…", "Listing WSL distributions…")).disabled(true)],
        Some([]) => vec![MenuItem::new("wsl:none", t("没有 WSL 发行版", "No WSL distributions")).disabled(true)],
        Some(list) => list
            .iter()
            .map(|distro| {
                let target = RunTarget::Wsl { distro: distro.name.clone() };
                let mut item = MenuItem::new(format!("wsl:{}", distro.name), distro.name.clone()).icon("SquareTerminal").checked(current == run_env_key(Some(&target))).on(on_machine(Some(target.clone()))).action(settings_action(key, Some(target), &distro.name));
                if distro.is_default {
                    item = item.hint(t("默认", "default"));
                }
                item
            })
            .collect(),
    };
    let mut items = vec![MenuItem::new("local", t("本机", "This machine")).icon("Monitor").checked(row.machine.is_none()).on(on_machine(None)).action(settings_action(key, None, t("本机", "This machine")))];
    if show_wsl {
        items.push(MenuItem::new("wsl", "WSL").icon("SquareTerminal").children(wsl_children));
    }
    let mut ssh_children: Vec<MenuItem> = machines
        .iter()
        .map(|machine| {
            let target = RunTarget::Ssh { machine_id: machine.id.clone() };
            MenuItem::new(format!("ssh:{}", machine.id), machine.name.clone()).icon("Server").checked(current == run_env_key(Some(&target))).on(on_machine(Some(target.clone()))).action(settings_action(key, Some(target), &machine.name))
        })
        .collect();
    {
        let (key, row_id) = (key.to_owned(), row.id.clone());
        ssh_children.push(MenuItem::new("ssh:add", t("添加 SSH 机器…", "Add SSH machine…")).icon("Plus").on(move |app: &mut App| form(app, &key).machine_dialog_row = Some(row_id.clone())));
    }
    items.push(MenuItem::new("ssh", "SSH").icon("Server").children(ssh_children));
    items
}

/// `renderPath`: a chip that opens the picker, or a typed path where the host has no native folder dialog.
fn path_control(key: &str, row: &ProjectRow, native_picker: bool, picking: bool) -> E {
    let t_ = theme();
    if row.machine.is_none() && !native_picker && !row.locked {
        let (key, row_id) = (key.to_owned(), row.id.clone());
        return field::<App>(("project-path", key.as_str(), row.id.as_str()), row.path.clone())
            .placeholder(t("绝对路径，如 C:\\Projects\\app 或 /home/me/app", "Absolute path, e.g. C:\\Projects\\app or /home/me/app"))
            .on_change(move |app, value| set_row_path(app, &key, &row_id, value))
            .build()
            .aria_label(t("工作区的绝对路径", "Absolute path of the workspace"))
            .flex1();
    }
    let tooltip = if row.path.is_empty() { t("为工作区选择目录", "Choose a directory for the workspace").to_owned() } else { row.path.clone() };
    let name = if row.path.is_empty() { t("为工作区选择目录", "Choose a directory for the workspace").to_owned() } else { tf("工作区：{path}", "Workspace: {path}", &[("path", &row.path)]) };
    let mut chip: E = row_chip(("project-path-chip", key, row.id.as_str())).flex1().min_w(0.0).aria_label(name).tooltip(tooltip).child(if picking { spinner(13.0) } else { icon("Folder", 13.0).color(t_.text_secondary) });
    chip = chip.child(if row.path.is_empty() { text(t("选择目录…", "Choose a directory…")).ty(CAPTION).color(t_.text_tertiary).flex1() } else { mewrk_pane_terminal::path_text(row.path.clone(), "", CAPTION, None).flex1() });
    if row.locked || picking {
        return chip.disabled(true);
    }
    let (key, row_id, is_remote) = (key.to_owned(), row.id.clone(), row.machine.is_some());
    chip.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| {
        if is_remote {
            form(app, &key).remote_picker_row = Some(row_id.clone());
        } else {
            pick_local(app, &key, &row_id);
        }
    })
}

fn row_chip(key: impl std::hash::Hash) -> E {
    let t_ = theme();
    row().key(key).items_center().gap(5.0).h(28.0).px(8.0).radius(7.0).bg(t_.control_fill).border(t_.control_stroke)
}

/// `ProjectDialogProps`.
pub struct ProjectDialogProps {
    /// Names this dialog's form.
    pub key: String,
    pub editing: bool,
    pub initial_name: String,
    pub initial_workspaces: Vec<AttachedWorkspace>,
    pub on_submit: Rc<dyn Fn(&mut App, String, Vec<AttachedWorkspace>)>,
    pub on_close: Handler<App>,
}

/// Whether this computer offers WSL distributions at all.
fn show_wsl() -> bool {
    host_is_windows(mewrk_client::machine_shells::host_platform())
}

/// The windows the project dialog stacks, in order: itself, then whatever a row opened over it.
pub fn project_dialog(app: &mut App, p: ProjectDialogProps) -> Vec<E> {
    let t_ = theme();
    let ProjectDialogProps { key, editing, initial_name, initial_workspaces, on_submit, on_close } = p;
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let fresh = !app.local::<Forms>().0.contains_key(&key);
    if fresh {
        let rows: Vec<ProjectRow> = if initial_workspaces.is_empty() {
            vec![ProjectRow { id: "initial-0".to_owned(), machine: None, path: String::new(), locked: false }]
        } else {
            initial_workspaces.iter().enumerate().map(|(index, workspace)| ProjectRow { id: format!("initial-{index}"), machine: workspace.machine.clone(), path: workspace.path.clone(), locked: editing && index == 0 && !is_deleted_machine(workspace.machine.as_ref(), &ssh_machines) }).collect()
        };
        let f = form(app, &key);
        f.name = initial_name;
        f.rows = rows;
    }
    let state = form(app, &key).clone();
    let machines: Vec<SshMachineConfig> = ssh_machines.iter().cloned().chain(state.created_machines.iter().filter(|created| !ssh_machines.iter().any(|machine| machine.id == created.id)).cloned()).collect();
    let native_picker = true;
    let wsl = show_wsl();

    let machine_label = |machine: Option<&RunTarget>| -> String {
        match machine {
            None => t("本机", "This machine").to_owned(),
            Some(RunTarget::Wsl { distro }) => distro.clone(),
            Some(RunTarget::Ssh { machine_id }) => machines.iter().find(|entry| entry.id == *machine_id).map_or_else(|| t("已删除的机器", "Deleted machine").to_owned(), |entry| entry.name.clone()),
        }
    };

    let duplicates = duplicate_rows(&state.rows);
    let at_cap = state.rows.len() >= MAX_PROJECT_WORKSPACES;
    let submit_ok = can_submit(&state.rows);

    // ---- the name
    let name_key = key.clone();
    let name_field = field::<App>(("project-name", key.as_str()), state.name.clone())
        .placeholder(t("留空时使用第一个工作区的文件夹名称", "Leave blank to use the first workspace's folder name"))
        .on_change(move |app, value| form(app, &name_key).name = value)
        .build()
        .aria_label(t("显示名称", "Display name"));
    let mut body: E = col().gap(14.0).child(col().gap(4.0).child(text(t("显示名称", "Display name")).ty(FINE).color(t_.text_secondary)).child(name_field));

    // ---- the rows
    let mut rows_el: E = col().role(Role::List).gap(6.0);
    for (index, row_state) in state.rows.iter().enumerate() {
        let label = machine_label(row_state.machine.as_ref());
        let trigger_title = if row_state.locked { t("项目的第一个工作区不能更换", "The project's first workspace cannot be changed").to_owned() } else { tf("工作区的机器：{name}", "Machine for the workspace: {name}", &[("name", &label)]) };
        let (trigger_key_key, trigger_row, trigger_machine) = (key.clone(), row_state.id.clone(), row_state.machine.clone());
        let trigger_text = label.clone();
        let distros_key = key.clone();
        let items = menu_items(&key, row_state, &machines, state.distros.as_deref(), wsl);
        let mut menu = PopoverMenu::new(("project-machine", key.as_str(), row_state.id.as_str()), tf("工作区的机器：{name}", "Machine for the workspace: {name}", &[("name", &label)]), move |_| row_chip(("project-machine-trigger", trigger_key_key.as_str(), trigger_row.as_str())).shrink0().tooltip(trigger_title).child(icon(machine_icon_name(trigger_machine.as_ref()), 13.0).color(t_.text_secondary)).child(text(trigger_text).ty(CAPTION).nowrap()).child(icon("ChevronDown", 11.0).color(t_.text_tertiary)).hover_bg(t_.subtle_hover))
            .menu_label(t("选择机器", "Choose a machine"))
            .sections(move || vec![MenuSection::new("machines").items(items)])
            .width(220.0)
            .above()
            .submenu(Submenu::Flyout)
            .disabled(row_state.locked);
        if wsl {
            // Distributions are machine state, so enumerate them each time the menu opens.
            menu = menu.on_open(move |app: &mut App| {
                let key = distros_key.clone();
                app.invoke::<Vec<WslDistro>>("list_wsl_distros", json!({}), move |app, result| {
                    form(app, &key).distros = Some(result.unwrap_or_default());
                    repaint();
                });
            });
        }
        let mut main: E = row().gap(6.0).items_center().child(menu.build()).child(path_control(&key, row_state, native_picker, state.picking_row.as_deref() == Some(row_state.id.as_str())));
        if index > 0 && !row_state.locked {
            let remove_label = if row_state.path.is_empty() { t("移除工作区", "Remove workspace").to_owned() } else { tf("移除工作区 {path}", "Remove workspace {path}", &[("path", &row_state.path)]) };
            let (remove_key, row_id) = (key.clone(), row_state.id.clone());
            main = main.child(tool_button(("project-remove", key.as_str(), row_state.id.as_str()), "X", remove_label, 24.0, 13.0).on_click(move |app: &mut App| {
                let f = form(app, &remove_key);
                f.rows.retain(|row| row.id != row_id || row.locked);
                f.pick_errors.remove(&row_id);
            }));
        }
        let mut entry: E = col().role(Role::ListItem).gap(3.0).child(main);
        let line = |s: String| text(s).role(Role::Alert).ty(FINE).color(t_.critical);
        if is_deleted_machine(row_state.machine.as_ref(), &machines) {
            entry = entry.child(line(t("这台机器已删除；为这个工作区重新选择机器和目录后它才能使用", "This machine was deleted; choose a machine and directory for this workspace again to use it").to_owned()));
        }
        if duplicates.contains(&row_state.id) {
            entry = entry.child(line(t("与上面的工作区是同一台机器上的同一个目录", "Same directory on the same machine as a workspace above").to_owned()));
        }
        if let Some(error) = state.pick_errors.get(&row_state.id) {
            entry = entry.child(line(error.clone()));
        }
        rows_el = rows_el.child(entry);
    }
    let add_key = key.clone();
    let mut add: E = row().key(("project-add", key.as_str())).self_start().items_center().gap(5.0).h(26.0).px(8.0).radius(6.0).child(icon("Plus", 13.0).color(if at_cap { t_.text_disabled } else { t_.text_secondary })).child(text(t("添加工作区", "Add workspace")).ty(CAPTION).color(if at_cap { t_.text_disabled } else { t_.text_primary }));
    add = if at_cap {
        add.disabled(true).disabled_reason(tf("一个项目最多 {max} 个工作区", "A project can have at most {max} workspaces", &[("max", &MAX_PROJECT_WORKSPACES)]))
    } else {
        add.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| {
            let f = form(app, &add_key);
            f.next_row += 1;
            let id = format!("row-{}", f.next_row);
            f.rows.push(ProjectRow { id, machine: None, path: String::new(), locked: false });
        })
    };
    body = body.child(col().gap(6.0).child(text(t("工作区", "Workspaces")).ty(FINE).color(t_.text_secondary)).child(rows_el).child(add));
    body = body.child(boundary_note("Settings", t("删除项目会把它和它的所有任务从 Mewrk 中永久移除；项目目录里的文件不受影响。", "Deleting a project removes it and all its tasks from Mewrk for good; the files in its directories are never touched.")));

    // ---- the footer
    let close_all = {
        let (on_close, key) = (on_close.clone(), key.clone());
        Rc::new(move |app: &mut App| {
            app.local::<Forms>().0.remove(&key);
            on_close(app);
        })
    };
    let cancel_close = close_all.clone();
    let cancel = dialog_button(Look::Subtle, t("取消", "Cancel")).key(("project-cancel", key.as_str())).on_click(move |app: &mut App| cancel_close(app));
    let mut submit_button: E = accent_button("").h(28.0).px(12.0).key(("project-submit", key.as_str()));
    if !editing {
        submit_button = submit_button.child(icon("FolderPlus", 15.0).color(t_.text_on_accent));
    }
    submit_button = submit_button.child(text(if editing { t("保存", "Save") } else { t("创建项目", "Create project") }).ty(CAPTION).color(t_.text_on_accent).nowrap());
    if submit_ok {
        let submit_key = key.clone();
        submit_button = submit_button.on_click(move |app: &mut App| {
            let f = form(app, &submit_key).clone();
            if !can_submit(&f.rows) {
                return;
            }
            let workspaces: Vec<AttachedWorkspace> = f.rows.iter().map(|row| AttachedWorkspace { machine: row.machine.clone(), path: if row.machine.is_none() && !row.locked && !native_picker { row.path.trim().to_owned() } else { row.path.clone() } }).collect();
            // Closing is the handler's to do: a project it refuses, one registered already or a move a run holds back, leaves
            // the dialog up with what was typed.
            on_submit(app, f.name.trim().to_owned(), workspaces);
        });
    } else {
        submit_button = submit_button.disabled(true).opacity(0.5);
    }
    let footer = row().gap(8.0).child(cancel).child(submit_button);

    let description = row().role(Role::Group).gap(4.0).items_center().child(text(t("项目由工作区组成，每个是某台机器上的目录。", "A project groups workspaces: directories on any machine.")).ty(FINE).color(t_.text_secondary)).child(mewrk_settings_global::widgets::docs_link("workspaces", "projects"));
    let title = if editing { t("编辑项目", "Edit project") } else { t("新建项目", "New project") };
    let dialog_close = close_all.clone();
    // The dialog's Escape runs before a menu's, so while a menu is open the dialog must not treat Escape or a backdrop click
    // as a request to close.
    let dismissible = !state.rows.iter().any(|row| crate::menu::menu_is_open(("project-machine", key.as_str(), row.id.as_str())));
    let mut windows = vec![DialogWindow::new(("project", key.as_str()), title, 560.0, body, move |app: &mut App| dialog_close(app)).description(description).dismissible(dismissible).footer(footer).build()];

    // ---- what a row opened over it
    if let Some(row_id) = state.machine_dialog_row.clone() {
        let (save_key, close_key) = (key.clone(), key.clone());
        windows.push(ssh_machine_dialog(
            app,
            None,
            None,
            machines.len(),
            Rc::new(move |app: &mut App, machine: SshMachineConfig| {
                save_ssh_machine(app, machine.clone());
                let id = machine.id.clone();
                form(app, &save_key).created_machines.push(machine);
                set_row_machine(app, &save_key, &row_id, Some(RunTarget::Ssh { machine_id: id }));
                form(app, &save_key).machine_dialog_row = None;
            }),
            None,
            Rc::new(move |app: &mut App| form(app, &close_key).machine_dialog_row = None),
        ));
    }
    if let Some(machine) = state.machine_settings.clone() {
        let (close_key, delete_key) = (key.clone(), key.clone());
        windows.push(machine_window(app, machine, Rc::new(move |app: &mut App| form(app, &close_key).machine_settings = None), Some(Rc::new(move |app: &mut App, id: String| delete_machine_from_rows(app, &delete_key, &id)))));
    }
    if let Some(row_state) = state.remote_picker_row.as_ref().and_then(|id| state.rows.iter().find(|row| row.id == *id && row.machine.is_some())) {
        let machine = row_state.machine.clone().expect("checked");
        let (pick_key, close_key, row_id) = (key.clone(), key.clone(), row_state.id.clone());
        windows.push(remote_directory_picker(
            app,
            RemotePickerProps {
                key: hash_key(("project-remote", key.as_str(), row_state.id.as_str())),
                machine_name: machine_label(Some(&machine)),
                machine,
                on_pick: Rc::new(move |app: &mut App, path| {
                    set_row_path(app, &pick_key, &row_id, path);
                    form(app, &pick_key).remote_picker_row = None;
                }),
                on_close: Rc::new(move |app: &mut App| form(app, &close_key).remote_picker_row = None),
            },
        ));
    }
    windows
}

/// `workspaceDialogOpen`.
pub fn create_project_dialog(app: &mut App) -> Option<E> {
    if !app.workspace_dialog_open() {
        // The next time it opens it starts over.
        app.local::<Forms>().0.remove("create");
        return None;
    }
    let windows = project_dialog(
        app,
        ProjectDialogProps {
            key: "create".to_owned(),
            editing: false,
            initial_name: String::new(),
            initial_workspaces: Vec::new(),
            on_submit: Rc::new(|app: &mut App, name, workspaces| app.add_workspace(&name, &workspaces, None)),
            on_close: Rc::new(|app: &mut App| app.set_workspace_dialog_open(false, false)),
        },
    );
    Some(stack(windows))
}

/// `editedProject`.
pub fn edit_project_dialog(app: &mut App) -> Option<E> {
    let edited = app.project_editor().filter(|id| app.workspace(id).is_some_and(|workspace| workspace.kind == WorkspaceKind::Directory));
    let open_key = edited.as_ref().map(|id| format!("edit:{id}"));
    app.local::<Forms>().0.retain(|form, _| !form.starts_with("edit:") || open_key.as_ref() == Some(form));
    let id = edited?;
    let project = app.workspace(&id)?.clone();
    let submit_id = id.clone();
    let windows = project_dialog(
        app,
        ProjectDialogProps {
            key: format!("edit:{id}"),
            editing: true,
            initial_name: project.name.clone(),
            initial_workspaces: registered_project_workspaces(Some(&project)),
            on_submit: Rc::new(move |app: &mut App, name, workspaces| app.update_project(&submit_id, &name, &workspaces)),
            on_close: Rc::new(|app: &mut App| app.set_project_editor(None)),
        },
    );
    Some(stack(windows))
}

/// Windows lift themselves to the dialog layer from wherever they are written; this just gives them one parent.
fn stack(windows: Vec<E>) -> E {
    div().children(windows)
}
