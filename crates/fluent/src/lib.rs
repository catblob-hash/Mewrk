//! A small, fast Fluent Design toolkit: instanced GPU rendering, incremental text and immediate-mode widgets.

pub mod edit;
pub mod el;
pub mod gpu;
pub mod lucide;
pub mod md;
pub mod spell;
pub mod text;
pub mod theme;
pub mod ui;

pub use edit::TextEdit;
pub use gpu::{queue_image, release_image, Gpu, ImageId, Renderer, Scene};
pub use text::{BlockKind, Doc, TextCtx};
pub use theme::{Color, Theme, TypeStyle, Weight};
pub use ui::{rect, reduced_motion, set_reduced_motion, CursorKind, Icon, Input, Key, KeyPress, MenuItem, Rect, Scroll, Severity, Ui, UiMemory};
