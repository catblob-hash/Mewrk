//! A small, fast Fluent Design toolkit: instanced GPU rendering, incremental text and immediate-mode widgets.

pub mod edit;
pub mod el;
pub mod gpu;
pub mod lucide;
pub mod mark;
pub mod md;
mod occlusion;
pub mod spell;
pub mod text;
pub mod theme;
pub mod ui;

pub use edit::TextEdit;
pub use gpu::{blur_into, queue_image, queue_live_image, queue_target_image, release_image, write_image, Gpu, ImageId, PlatePass, Renderer, Scene};
#[cfg(target_os = "macos")]
pub use gpu::{queue_surface_image, show_surface, SurfaceFrame};
pub use text::{BlockKind, Doc, TextCtx};
pub use theme::{Color, Theme, TypeStyle, Weight};
pub use ui::{rect, reduced_motion, set_reduced_motion, CursorKind, Icon, Input, Key, KeyPress, MenuItem, Rect, Scroll, Severity, Ui, UiMemory};
