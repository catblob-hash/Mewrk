//! The shell through a real host: `App::start` over a temp data dir and a temp project, a scripted window and no GPU.
//! One binary on purpose: each test binary links the whole host.

mod mock;
mod rig;
mod run;

mod a11y;
mod chrome;
mod dialogs;
mod panes;
mod sidebar;
mod tiles;
