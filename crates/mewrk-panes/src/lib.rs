//! The Review and Files side panes, and the syntax highlighter they share with the chat.
//!
//! Neither pane touches the disk or spawns Git on the UI thread: each owns background threads and picks results up on
//! the next frame, calling `Wake` so that a frame happens. Documents are built whole on those threads, syntax colours
//! and word marks included, so that painting one is only copying its visible rows however long it is.

mod code;
mod diff;
mod files;
pub mod highlight;
mod review;

pub use files::FilesPane;
pub use review::ReviewPane;

/// Asks the app for a frame. Called from background threads whenever a pane has something new to draw.
pub type Wake = std::sync::Arc<dyn Fn() + Send + Sync>;

/// Room kept clear on a list's right for the scrollbar, whose track would otherwise take the clicks meant for what is there.
pub(crate) const RIGHT_GUTTER: f32 = 12.0;

/// A widget id under a pane's id. Ids are global to the UI, and panes come in several.
pub(crate) fn sub(id: u64, n: u64) -> u64 {
    id.rotate_left(17) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// A list's scroll state. The default sticks to the bottom, for streams, and a list opens at its top.
pub(crate) fn pinned_scroll() -> fluent::Scroll {
    let mut scroll = fluent::Scroll::default();
    scroll.stick = false;
    scroll
}

#[cfg(test)]
mod tests;
