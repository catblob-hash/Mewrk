//! Remote control: a phone, a desktop or a virtual machine as a preview server.
//!
//! A `.mewrk/launch.json` entry of `"type": "remote-control"` names a screen
//! somewhere — an Android device behind an adb server, an iOS device behind
//! WebDriverAgent, a desktop behind a VNC server — and `preview_start` connects
//! to it. The preview pane then shows its live picture and the `preview_*`
//! tools look at it and act on it. The design and its trust rules are in
//! `docs/remote-control.md`.
//!
//! The layers, bottom up:
//!
//! * [`target`] — the entry as the file wrote it: protocol, place, port, device.
//! * [`transport`] — reaching a port on a place's loopback, here or through an
//!   SSH machine's agent. Nothing above it ever sees a host name.
//! * [`driver`] — what one protocol must provide: a frame, pointer or touch
//!   input, keys, text, maybe a UI tree. [`vnc`], [`adb`] and [`wda`]
//!   implement it; [`http`] is the small HTTP client WebDriverAgent needs.
//! * [`keys`] — the key vocabulary shared by the model and the viewer.
//! * [`session`] — one connected screen: state, reconnects, frames, the
//!   coordinate frame the model works in, gestures, the UI tree's uids
//!   ([`snapshot`]), JPEG encoding ([`frame`]), stored sign-ins
//!   ([`credentials`]).
//! * [`viewer`] — the loopback page the preview pane shows, behind the
//!   [`view`] contract.
//! * [`place`] — resolving an entry's place against the user's machines, and
//!   the place and device-lock keys the preview registry shares with sessions.
//! * [`android`], [`ios`], [`desktop`] — the per-platform tools (`android_*`,
//!   `ios_*`, `desktop_*`): each call parsed and bounded, then run on a
//!   session's driver through the protocol's own extension of [`driver`]
//!   ([`driver::AndroidControl`], [`driver::IosControl`],
//!   [`driver::DesktopControl`]). The session decides the locks and the time
//!   (`session::platform`).
//!
//! A session becomes a row of [`crate::preview_servers::PreviewServerRegistry`]
//! through [`crate::preview::start`]; the registry closes it on stop.

pub mod adb;
pub mod android;
pub mod credentials;
pub mod desktop;
pub mod driver;
#[cfg(test)]
pub(crate) mod fake;
pub mod frame;
pub mod http;
pub mod ios;
pub mod keys;
pub mod picture_stream;
pub mod place;
pub mod session;
pub mod snapshot;
pub mod target;
pub mod transport;
pub mod view;
pub mod viewer;
pub mod vnc;
pub mod wda;
