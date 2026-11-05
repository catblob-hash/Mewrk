//! `ConfirmDialog.tsx`: the in-app replacement for `window.confirm`.

use mewrk_client::prelude::*;

use crate::state::{shell, ConfirmationRequest};
use crate::widgets::{copy, dialog_button, Look, DialogWindow};

const CONFIRM_KEY: &str = "confirm-ok";

/// Puts a question up.
pub fn ask(app: &mut App, request: ConfirmationRequest) {
    shell(app).confirmation = Some(request);
    repaint();
}

pub fn confirm_dialog(app: &mut App) -> Option<E> {
    let request = shell(app).confirmation.clone()?;
    Some(confirmation_window(request, Rc::new(|app: &mut App| shell(app).confirmation = None)))
}

/// The window for one question, for a caller that holds the question itself (`ErrorView` does).
pub fn confirmation_window(request: ConfirmationRequest, on_close: Handler<App>) -> E {
    let ConfirmationRequest { question, detail, confirm_label, destructive, on_confirm } = request;
    let cancel = on_close.clone();
    let confirm = on_close.clone();
    let press: Handler<App> = Rc::new(move |app: &mut App| {
        confirm(app);
        on_confirm(app);
    });
    // `autoFocus`: the user has just asked for this; the answer is one key away.
    let asked = use_state(("confirm-asked", question.as_str()), || false);
    if !asked.get() {
        asked.set(true);
        focus_request(CONFIRM_KEY);
    }
    let (click, keys) = (press.clone(), press);
    let confirm_button: E = dialog_button(if destructive { Look::Danger } else { Look::Accent }, confirm_label).key(CONFIRM_KEY).on_click(move |app: &mut App| click(app)).on_key(move |app: &mut App, key: &KeyPress| {
        if matches!(key.key, Key::Enter | Key::Char(' ')) {
            keys(app);
            return true;
        }
        false
    });
    let footer = row().gap(8.0).child(dialog_button(Look::Standard, t("取消", "Cancel")).key("confirm-cancel").on_click(move |app: &mut App| cancel(app))).child(confirm_button);
    let close = on_close;
    let body = col().child(detail.map(copy));
    DialogWindow::new("confirm", question, 420.0, body, move |app: &mut App| close(app)).footer(footer).build()
}
