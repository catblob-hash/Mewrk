//! The global keyboard shortcuts of `App.tsx`: command definitions and defaults are code constants in
//! `mewrk_client::shortcuts`; documents store user overrides only.

use mewrk_client::appearance::{clamp_zoom, ZOOM_STEP};
use mewrk_client::model::ContextItem;
use mewrk_client::prelude::*;
use mewrk_client::shortcuts::{is_ime_key_event, matches_event, resolve_shortcut, should_suppress_for_focus, KeyInput, SHORTCUT_COMMANDS};

use crate::actions::{close_last_pane, open_global_settings, open_pane, toggle_preview};

fn adjust_zoom(app: &mut App, delta: f64) {
    app.update_global_settings(move |doc| doc.global_settings.appearance.zoom = clamp_zoom(doc.global_settings.appearance.zoom + delta));
}

fn last_context_text(app: &mut App, assistant: bool) -> Option<(String, String)> {
    let id = app.active_conversation_id()?;
    let conversation = app.conversation(&id)?;
    conversation.contexts.iter().rev().find_map(|context| match context {
        ContextItem::Assistant { id, content, .. } if assistant => Some((id.clone(), content.clone())),
        ContextItem::User { id, content, .. } if !assistant => Some((id.clone(), content.clone())),
        _ => None,
    })
}

/// `shortcutActionsRef`.
pub fn run_command(app: &mut App, command: &str) -> bool {
    match command {
        "app.settings.open" => open_global_settings(app, "providers"),
        "app.conversation_settings.open" => {
            mewrk_settings_global::close(app);
            open_pane(app, "settings");
        }
        "app.zoom.in" => adjust_zoom(app, ZOOM_STEP),
        "app.zoom.out" => adjust_zoom(app, -ZOOM_STEP),
        "app.zoom.reset" => app.update_global_settings(|doc| doc.global_settings.appearance.zoom = 1.0),
        "conversation.create" => app.open_draft_conversation(None),
        "conversation.next" => app.step_conversation(1),
        "conversation.previous" => app.step_conversation(-1),
        "conversation.stop" => {
            if let Some(id) = app.active_conversation_id() {
                app.cancel_run(&id, |_, _| {});
            }
        }
        "message.copy_last" => {
            if let Some((_, content)) = last_context_text(app, true)
                && !content.is_empty()
            {
                crate::root::set_clipboard(app, content);
            }
        }
        "message.edit_last_user" => {
            if let Some((id, _)) = last_context_text(app, false) {
                crate::chat::edit_context(app, &id);
            }
        }
        // Toggles this conversation's preview pane: on the page last seen there, or, with no page yet, on the start page of
        // the workspace the composer's chip has selected.
        "panel.browser.toggle" => toggle_preview(app),
        "panel.close" => close_last_pane(app),
        _ => return false,
    }
    repaint();
    true
}

/// The window's `keydown` listener: `on_key_global` of the root. A focused field hears keys first and doesn't stop these,
/// so a press with a modifier reaches here either way, as it does upstream.
pub fn handle_key(app: &mut App, press: &KeyPress, in_text_input: bool) -> bool {
    let event = KeyInput::from_key_press(*press);
    // IME composition keystrokes are not shortcuts.
    if is_ime_key_event(&event) {
        return false;
    }
    let shortcuts = app.doc.global_settings.shortcuts.clone();
    for command in SHORTCUT_COMMANDS {
        let preference = resolve_shortcut(&shortcuts, command);
        if !preference.enabled || preference.binding.is_empty() || !matches_event(&preference.binding, &event) {
            continue;
        }
        // In focused inputs, unmodified combinations yield to typing.
        if should_suppress_for_focus(&preference.binding, in_text_input) {
            continue;
        }
        if run_command(app, command.id) {
            return true;
        }
    }
    false
}
