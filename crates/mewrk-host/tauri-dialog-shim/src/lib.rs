//! The dialogs the host asks for, on the system's own.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, WebviewWindow, Wry};

pub struct Dialog;

pub trait DialogExt {
    fn dialog(&self) -> Dialog;
}

impl DialogExt for AppHandle<Wry> {
    fn dialog(&self) -> Dialog {
        Dialog
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageDialogKind {
    Info,
    Warning,
    Error,
}

pub enum MessageDialogButtons {
    Ok,
    OkCancel,
    YesNo,
    OkCustom(String),
    OkCancelCustom(String, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilePath {
    Path(PathBuf),
    Url(String),
}

impl FilePath {
    pub fn into_path(self) -> Result<PathBuf, String> {
        match self {
            FilePath::Path(p) => Ok(p),
            FilePath::Url(u) => Err(format!("not a file path: {u}")),
        }
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            FilePath::Path(p) => Some(p),
            FilePath::Url(_) => None,
        }
    }
}

impl std::fmt::Display for FilePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilePath::Path(p) => write!(f, "{}", p.display()),
            FilePath::Url(u) => f.write_str(u),
        }
    }
}

pub struct MessageDialogBuilder {
    message: String,
    title: String,
    kind: MessageDialogKind,
    buttons: MessageDialogButtons,
}

pub struct FileDialogBuilder {
    dialog: rfd::FileDialog,
}

impl Dialog {
    pub fn message(&self, message: impl Into<String>) -> MessageDialogBuilder {
        MessageDialogBuilder {
            message: message.into(),
            title: String::new(),
            kind: MessageDialogKind::Info,
            buttons: MessageDialogButtons::Ok,
        }
    }

    pub fn file(&self) -> FileDialogBuilder {
        FileDialogBuilder { dialog: rfd::FileDialog::new() }
    }
}

impl MessageDialogBuilder {
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub fn kind(mut self, kind: MessageDialogKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn buttons(mut self, buttons: MessageDialogButtons) -> Self {
        self.buttons = buttons;
        self
    }

    pub fn parent(self, _window: &WebviewWindow) -> Self {
        self
    }

    /// True when the user took the affirmative button.
    pub fn blocking_show(self) -> bool {
        let level = match self.kind {
            MessageDialogKind::Info => rfd::MessageLevel::Info,
            MessageDialogKind::Warning => rfd::MessageLevel::Warning,
            MessageDialogKind::Error => rfd::MessageLevel::Error,
        };
        let (buttons, yes) = match self.buttons {
            MessageDialogButtons::Ok => (rfd::MessageButtons::Ok, "OK".to_owned()),
            MessageDialogButtons::OkCancel => (rfd::MessageButtons::OkCancel, "OK".to_owned()),
            MessageDialogButtons::YesNo => (rfd::MessageButtons::YesNo, "Yes".to_owned()),
            MessageDialogButtons::OkCustom(ok) => (rfd::MessageButtons::OkCustom(ok.clone()), ok),
            MessageDialogButtons::OkCancelCustom(ok, cancel) => (rfd::MessageButtons::OkCancelCustom(ok.clone(), cancel), ok),
        };
        let answer = rfd::MessageDialog::new().set_level(level).set_title(self.title).set_description(self.message).set_buttons(buttons).show();
        match answer {
            rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes => true,
            rfd::MessageDialogResult::Custom(label) => label == yes,
            _ => false,
        }
    }
}

impl FileDialogBuilder {
    pub fn set_title(mut self, title: impl Into<String>) -> Self {
        self.dialog = self.dialog.set_title(title.into());
        self
    }

    pub fn add_filter(mut self, name: impl Into<String>, extensions: &[&str]) -> Self {
        self.dialog = self.dialog.add_filter(name.into(), extensions);
        self
    }

    pub fn set_directory(mut self, directory: impl AsRef<Path>) -> Self {
        self.dialog = self.dialog.set_directory(directory);
        self
    }

    pub fn set_parent(self, _window: &WebviewWindow) -> Self {
        self
    }

    pub fn blocking_pick_folder(self) -> Option<FilePath> {
        self.dialog.pick_folder().map(FilePath::Path)
    }

    pub fn blocking_pick_file(self) -> Option<FilePath> {
        self.dialog.pick_file().map(FilePath::Path)
    }

    pub fn pick_file(self, callback: impl FnOnce(Option<FilePath>) + Send + 'static) {
        std::thread::spawn(move || callback(self.dialog.pick_file().map(FilePath::Path)));
    }

    pub fn pick_folder(self, callback: impl FnOnce(Option<FilePath>) + Send + 'static) {
        std::thread::spawn(move || callback(self.dialog.pick_folder().map(FilePath::Path)));
    }
}

pub fn init() {}
