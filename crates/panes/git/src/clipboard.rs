//! A handler has no clipboard to write to, but the next paint does. `copy_text` from a handler, and put `clipboard_leaf`
//! somewhere in the tree.

use std::cell::RefCell;

use mewrk_client::prelude::*;

thread_local! {
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn copy_text(text: impl Into<String>) {
    PENDING.with(|p| *p.borrow_mut() = Some(text.into()));
    repaint();
}

pub fn clipboard_leaf() -> E {
    custom::<App>(|_, ui, _| {
        if let Some(text) = PENDING.with(|p| p.borrow_mut().take()) {
            ui.clipboard.set(&text);
        }
    })
    .size(1.0, 1.0)
    .absolute()
    .pass_through()
}
