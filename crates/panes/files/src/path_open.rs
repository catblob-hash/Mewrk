//! The single entry point for a click on a file path the app detected (upstream's `lib/pathLinks.ts`). A click is offered
//! to the app first, through the handler the shell registers, so a file in the open workspace is shown in the file pane.
//! Only what the pane cannot reach is revealed in the system file manager.

use crate::links::{is_absolute_path, PathOpenRequest};
use mewrk_client::prelude::*;
use std::cell::RefCell;

/// Returning false is how the handler says the pane cannot serve it, and the reveal runs.
pub type PathOpenHandler = Rc<dyn Fn(&mut App, &PathOpenRequest) -> bool>;
pub type PathPrefetchHandler = Rc<dyn Fn(&mut App, &PathOpenRequest)>;

thread_local! {
    static OPEN: RefCell<Option<PathOpenHandler>> = const { RefCell::new(None) };
    static PREFETCH: RefCell<Option<PathPrefetchHandler>> = const { RefCell::new(None) };
}

/// Installs the in-app handler. `clear_path_open_handler` removes it.
pub fn set_path_open_handler(handler: PathOpenHandler) {
    OPEN.with(|h| *h.borrow_mut() = Some(handler));
}

pub fn clear_path_open_handler() {
    OPEN.with(|h| *h.borrow_mut() = None);
}

pub fn set_path_prefetch_handler(handler: Option<PathPrefetchHandler>) {
    PREFETCH.with(|h| *h.borrow_mut() = handler);
}

/// Tells the app the pointer has rested on a path.
pub fn prefetch_path(app: &mut App, request: &PathOpenRequest) {
    if let Some(h) = PREFETCH.with(|h| h.borrow().clone()) {
        h(app, request);
    }
}

/// Opens a path the way a click on it does, and says whether anything took it.
pub fn open_path(app: &mut App, request: &PathOpenRequest) -> bool {
    if let Some(handler) = OPEN.with(|h| h.borrow().clone())
        && handler(app, request)
    {
        return true;
    }
    // A relative path without a working directory cannot be resolved by the host either.
    if request.base_dir.is_none() && !is_absolute_path(&request.path) {
        return false;
    }
    let args = json!({ "path": request.path, "baseDir": request.base_dir });
    app.invoke_logged("reveal_path_in_file_manager", args);
    true
}
