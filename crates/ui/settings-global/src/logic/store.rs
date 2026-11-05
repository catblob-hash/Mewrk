//! The document as the settings pages change it, over the client's store (`doc.rs`): `onChange` and `onFlush`.

use mewrk_client::model::AppDocument;
use mewrk_client::App;

#[derive(Default)]
struct Store {
    /// What the next flush answers instead of saving. For tests of what a refused save looks like.
    fail_next: Option<String>,
}

/// `handleGlobalSettingsChange`: publishes the change at once and schedules the debounced save.
pub fn change(app: &mut App, f: impl FnOnce(&mut AppDocument)) {
    app.update_global_settings(f);
    // Language applies at once, whichever control changed it.
    mewrk_client::i18n::set_english(!app.language_is_chinese());
    fluent::el::repaint();
}

/// `await onFlush()`: `done` runs on the UI thread once what is on screen has reached the host, or with why it could not.
pub fn flush(app: &mut App, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    if let Some(message) = app.local::<Store>().fail_next.take() {
        app.post(move |app| done(app, Err(message)));
        return;
    }
    app.flush_latest_document(false, done);
}

/// Everything saved so far is on disk: what quitting waits for.
pub fn flush_durable(app: &mut App, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    app.flush_latest_document(true, move |app, result| {
        if let Err(e) = result {
            return done(app, Err(e));
        }
        app.flush_document_saves(done);
    });
}

/// Makes the next `flush` fail with this message. Test support, reachable only by name.
#[doc(hidden)]
pub fn fail_next_flush(app: &mut App, message: &str) {
    app.local::<Store>().fail_next = Some(message.to_owned());
}
