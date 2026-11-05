//! The windows `App.tsx` mounts at the root.

pub mod confirm;
pub mod error_view;
pub mod machines;
pub mod path_choice;
pub mod project;
pub mod remote_picker;
pub mod search;
pub mod ssh_prompt;

use mewrk_client::prelude::*;

use crate::state::shell;

/// Every window that is open, in `App.tsx`'s order: what is written later sits over what is written earlier.
pub fn windows(app: &mut App) -> Vec<E> {
    let mut out = Vec::new();
    out.extend(mewrk_settings_global::global_settings_window(app));
    // `WindowLayerOutlet`: the preset's and the role's windows are drawn by the settings pane, and lift themselves to the
    // dialog layer from wherever they are written, so nothing has to host them here.
    if !settings_pane_open(app) {
        out.extend(mewrk_settings_conv::template_switch_prompt(app));
    }
    out.extend(confirm::confirm_dialog(app));
    // The timeline's own questions, its delete confirmation and the rewind window.
    out.extend(mewrk_ui_chat::handlers::timeline_dialogs(app));
    out.extend(project::create_project_dialog(app));
    out.extend(project::edit_project_dialog(app));
    out.extend(machines::machine_settings_window(app));
    out.extend(machines::workspace_settings_window(app));
    out.extend(remote_picker::attach_picker(app));
    out.extend(search::conversation_search(app));
    out.extend(mewrk_settings_global::first_launch_dialog(app));
    // Last, so it sits above whichever window started the connection that is asking.
    out.extend(ssh_prompt::ssh_prompt_dialog(app));
    restore_focus(app, !out.is_empty());
    out
}

/// `Dialog`: a window holds the keyboard while it is up, and gives it back to what had it. What is under a window must not hear
/// the keys: a terminal would take the Escape that closes the window for the start of a sequence and eat the next key with it.
/// Giving it back waits a frame, so that whatever closed the window can send it elsewhere first, as picking a conversation in
/// the search sends it to that conversation's composer.
fn restore_focus(app: &mut App, open: bool) {
    let state = shell(app);
    if let Some((previous, left_behind)) = state.modal_focus_return.take()
        && !open
        && focused_id() == left_behind
    {
        focus_request_id(previous);
    }
    match (open, state.modal_focus) {
        (true, None) => {
            state.modal_focus = Some(focused_id());
            state.modal_focus_settling = true;
            repaint();
        }
        (true, Some(previous)) if state.modal_focus_settling => {
            state.modal_focus_settling = false;
            // Nothing in the window asked for it, so nothing holds it.
            if focused_id() == previous {
                focus_request_id(0);
            }
        }
        (false, Some(previous)) => {
            state.modal_focus = None;
            state.modal_focus_settling = false;
            state.modal_focus_return = Some((previous, focused_id()));
            repaint();
        }
        _ => {}
    }
}

/// Whether the conversation-settings pane is drawing, which is what already shows the template prompt.
fn settings_pane_open(app: &mut App) -> bool {
    let Some(id) = app.active_conversation_id() else { return false };
    mewrk_client::side_panes::pane_is_open(mewrk_client::side_panes::side_pane_layout_for(app.side_panes(), Some(&id)), "settings")
}
