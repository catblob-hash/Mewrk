//! `RemoteDirectoryPicker.tsx`: directory browser for a machine that is not this one. The host has a native folder dialog
//! only for its own filesystem, so a directory on a WSL distribution or an SSH machine is chosen here instead: each level is
//! read through that machine's own shell, and confirming records the grant the way the native dialog does for a local path.
//!
//! The path field is editable on purpose: browsing from the home directory to a deeply nested project is a lot of round
//! trips over SSH, and what makes the typed path safe is that the host still resolves and verifies it before the grant exists.

use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;
use mewrk_client::workspaces::RemoteDirectoryListing;

use crate::state::{shell, RemoteWorkspacePicker};
use crate::widgets::{error_line, dialog_button, Look, spinner, DialogWindow};

/// Where the browser starts when it has nowhere better: the remote user's home.
const HOME: &str = "~";

#[derive(Default)]
struct PickerStore {
    pickers: std::collections::HashMap<u64, Picker>,
}

#[derive(Default, Clone)]
struct Picker {
    listing: Option<RemoteDirectoryListing>,
    draft: String,
    error: Option<String>,
    busy: bool,
    /// Only the newest navigation may write state: a slow parent listing must not land after the child already clicked into.
    generation: u64,
    started: bool,
}

fn picker(app: &mut App, id: u64) -> &mut Picker {
    app.local::<PickerStore>().pickers.entry(id).or_default()
}

fn navigate(app: &mut App, id: u64, machine: &RunTarget, path: &str) {
    let state = picker(app, id);
    state.generation += 1;
    let attempt = state.generation;
    state.busy = true;
    state.error = None;
    app.list_remote_directory(machine, path, move |app, result| {
        let state = picker(app, id);
        if attempt != state.generation {
            return;
        }
        state.busy = false;
        match result {
            Ok(next) => {
                state.draft = next.path.clone();
                state.listing = Some(next);
            }
            Err(reason) => state.error = Some(reason),
        }
        repaint();
    });
}

/// `RemoteDirectoryPickerProps`.
pub struct RemotePickerProps {
    pub key: u64,
    pub machine: RunTarget,
    pub machine_name: String,
    /// Receives the path the host resolved and authorized.
    pub on_pick: Rc<dyn Fn(&mut App, String)>,
    pub on_close: Handler<App>,
}

pub fn remote_directory_picker(app: &mut App, p: RemotePickerProps) -> E {
    let t_ = theme();
    let RemotePickerProps { key, machine, machine_name, on_pick, on_close } = p;
    // The browse is the window's, so a new one starts at the machine's home again however this one was left.
    let leave: Handler<App> = Rc::new(move |app: &mut App| {
        app.local::<PickerStore>().pickers.remove(&key);
        on_close(app);
    });
    if !std::mem::replace(&mut picker(app, key).started, true) {
        picker(app, key).draft = HOME.to_owned();
        navigate(app, key, &machine, HOME);
    }
    let state = picker(app, key).clone();
    let busy = state.busy;

    let change_key = key;
    let (submit_machine, submit_key) = (machine.clone(), key);
    let path_field = field::<App>(("remote-picker-path", key), state.draft.clone())
        .controlled()
        .on_change(move |app, value| {
            let s = picker(app, change_key);
            s.draft = value;
            s.error = None;
        })
        .on_submit(move |app, value| {
            let path = value.trim().to_owned();
            navigate(app, submit_key, &submit_machine, if path.is_empty() { HOME } else { &path });
        })
        .build()
        .aria_label(t("路径", "Path"));
    let path_row = col().gap(4.0).child(text(t("路径", "Path")).ty(FINE).color(t_.text_secondary)).child(path_field);

    let entry = |element_key: u64, name: &'static str, label: String, target: String, machine: RunTarget| -> E {
        let mut e: E = row().key(element_key).items_center().gap(7.0).h(26.0).px(7.0).radius(6.0).shrink0().child(icon(name, 13.0).color(t_.text_secondary)).child(text(label).ty(CAPTION).ellipsis().flex1());
        if busy {
            e = e.disabled(true);
        } else {
            e = e.hover_bg(t_.subtle_hover).on_click(move |app: &mut App| navigate(app, key, &machine, &target));
        }
        e
    };
    let mut list: E = col().key(("remote-picker-list", key)).role(Role::ListBox).aria_busy(busy).h(260.0).scroll_y().p(4.0).radius(8.0).bg(t_.card_secondary).border(t_.card_stroke);
    // Parent and child paths come from the host, which knows whether this machine spells them `/srv/app` or `C:/Users/dev`.
    if let Some(parent) = state.listing.as_ref().and_then(|listing| listing.parent.clone()) {
        list = list.child(entry(hash_key(("remote-picker-up", key)), "CornerLeftUp", t("上一级", "Up one level").to_owned(), parent, machine.clone()));
    }
    if let Some(listing) = &state.listing {
        for directory in &listing.entries {
            list = list.child(entry(hash_key(("remote-picker-entry", key, directory.path.as_str())), "Folder", directory.name.clone(), directory.path.clone(), machine.clone()));
        }
        if listing.entries.is_empty() {
            list = list.child(text(t("这个目录里没有子目录", "This directory has no subdirectories")).ty(CAPTION).color(t_.text_tertiary).p(7.0));
        }
    } else if busy {
        list = list.child(row().gap(6.0).items_center().p(7.0).child(spinner(13.0)).child(text(t("正在读取…", "Reading…")).ty(CAPTION).color(t_.text_tertiary)));
    }

    let body = col().gap(10.0).child(path_row).child(list).child(state.error.clone().map(error_line));

    let cancel = leave.clone();
    let choose_machine = machine.clone();
    let mut choose: E = dialog_button(Look::Accent, t("选择", "Choose")).key(("remote-picker-choose", key));
    if busy {
        choose = choose.disabled(true);
    } else {
        choose = choose.on_click(move |app: &mut App| {
            let path = picker(app, key).draft.trim().to_owned();
            if path.is_empty() {
                picker(app, key).error = Some(t("请填写一个目录", "Enter a directory").to_owned());
                return;
            }
            let s = picker(app, key);
            s.busy = true;
            s.error = None;
            let pick = PickSlot::put(on_pick.clone());
            app.authorize_remote_workspace(&choose_machine, &path, move |app, result| match result {
                Ok(path) => {
                    if let Some(f) = PickSlot::take(pick) {
                        f(app, path);
                    }
                    app.local::<PickerStore>().pickers.remove(&key);
                    repaint();
                }
                Err(reason) => {
                    PickSlot::take(pick);
                    let s = picker(app, key);
                    s.error = Some(reason);
                    s.busy = false;
                    repaint();
                }
            });
        });
    }
    let footer = row().gap(8.0).child(dialog_button(Look::Standard, t("取消", "Cancel")).key(("remote-picker-cancel", key)).on_click(move |app: &mut App| cancel(app))).child(choose);
    let close = leave;
    let description = text(t("选中的目录会成为一个工作区。", "The directory you choose becomes a workspace.")).ty(FINE).color(t_.text_secondary);
    DialogWindow::new(("remote-picker", key), tf("选择 {name} 上的工作区", "Choose a workspace on {name}", &[("name", &machine_name)]), 460.0, body, move |app: &mut App| close(app))
    .description(description)
    .footer(footer)
    .build()
}

/// `onPick` is a UI-thread closure and host answers cross threads, so the closure waits here and the answer names it.
struct PickSlot;

thread_local! {
    static PICKS: std::cell::RefCell<std::collections::HashMap<u64, Rc<dyn Fn(&mut App, String)>>> = std::cell::RefCell::new(std::collections::HashMap::new());
    static NEXT_PICK: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

impl PickSlot {
    fn put(f: Rc<dyn Fn(&mut App, String)>) -> u64 {
        let id = NEXT_PICK.with(|next| {
            let id = next.get();
            next.set(id + 1);
            id
        });
        PICKS.with(|slots| slots.borrow_mut().insert(id, f));
        id
    }

    fn take(id: u64) -> Option<Rc<dyn Fn(&mut App, String)>> {
        PICKS.with(|slots| slots.borrow_mut().remove(&id))
    }
}

/// The composer row's attach-a-workspace picker: `remoteWorkspacePicker` of `App.tsx`.
pub fn attach_picker(app: &mut App) -> Option<E> {
    let RemoteWorkspacePicker { machine, name } = shell(app).remote_picker.clone()?;
    let close_machine = machine.clone();
    let key = hash_key(("attach-picker", mewrk_client::workspaces::run_env_key(Some(&machine))));
    let _ = close_machine;
    Some(remote_directory_picker(
        app,
        RemotePickerProps {
            key,
            machine: machine.clone(),
            machine_name: name,
            on_pick: Rc::new(move |app: &mut App, path| {
                shell(app).remote_picker = None;
                mewrk_ui_composer::attach_workspace(app, Some(machine.clone()), path);
            }),
            on_close: Rc::new(|app: &mut App| shell(app).remote_picker = None),
        },
    ))
}
