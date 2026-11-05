//! Upstream's tests beside these files, and the composer's parts of `App.composerChips`, `App.images`, `App.newConversation` and
//! `App.draftGit`, scripted against a real host. One test binary for the whole crate: the disk this runs on is small.

mod a11y;
mod add_menu;
mod attachments;
mod chips;
mod docks;
mod landing;
mod meter;
mod popover;
pub mod rig;
mod spell;
mod text_area;

use std::sync::{Mutex, MutexGuard};

static LANGUAGE: Mutex<()> = Mutex::new(());

/// The language is one flag for the whole process, so a test that reads words holds it for as long as it reads them.
pub fn language(english: bool) -> MutexGuard<'static, ()> {
    let guard = LANGUAGE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    mewrk_client::i18n::set_english(english);
    guard
}
