//! The contract between the viewer page's HTTP server ([`super::viewer`]) and
//! the session behind it ([`super::session`]).
//!
//! The server knows HTTP and nothing about protocols; the session knows the
//! screen and nothing about HTTP. This is the whole of what passes between
//! them: the state the page draws, frames already encoded as JPEG (encoded once
//! and shared by every page watching), and what a person did on the page.

use std::{fmt, sync::Arc, time::Duration};

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroizing;

use super::{driver::Platform, target::Protocol};

/// One frame as the viewer page shows it.
#[derive(Clone, Debug)]
pub struct ViewerFrame {
    pub jpeg: Arc<[u8]>,
    /// The JPEG's size. The page reads it from the image itself, so only tests look at it.
    #[cfg(test)]
    pub width: u32,
    #[cfg(test)]
    pub height: u32,
    /// The [`super::driver::Frame::seq`] it was encoded from.
    pub seq: u64,
}

/// Where a session is, in the words the page and the pane share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionPhase {
    Connecting,
    /// Waiting for the person at the pane to sign in.
    Credentials,
    Ready,
    Reconnecting,
    Failed,
}

/// What the sign-in form asks for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialRequest {
    /// Prefilled from the entry's `username`.
    pub username: Option<String>,
    /// Apple Remote Desktop authentication needs an account name; VNC
    /// authentication has only a password.
    pub needs_username: bool,
    /// The last answer was refused.
    pub retry: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ScreenSize {
    pub width: u32,
    pub height: u32,
}

/// `GET /<token>/state`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerState {
    /// The entry's name.
    pub name: String,
    pub protocol: Protocol,
    pub platform: Platform,
    /// The place as a person knows it, with the port: "this computer · 5901",
    /// "mac · 5900". Shown on the sign-in form so the person sees where the
    /// password goes.
    pub endpoint: String,
    /// The device model or desktop name, once connected.
    pub label: Option<String>,
    pub phase: SessionPhase,
    /// Why it failed, why it is reconnecting, why the sign-in was refused.
    pub message: Option<String>,
    pub screen: Option<ScreenSize>,
    pub read_only: bool,
    pub credentials: Option<CredentialRequest>,
    /// The model is in the middle of an input batch: the page's own input is
    /// dropped until it ends.
    pub model_active: bool,
}

/// Where a pointer is in its press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PointerPhase {
    Down,
    Move,
    Up,
    /// The page lost the pointer (it left the window mid-drag): release
    /// whatever is held.
    Cancel,
}

/// One thing a person did on the page. Coordinates are fractions of the
/// picture, `0.0..=1.0` from its top-left corner, so the page never needs to
/// know the screen's size.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ViewerEvent {
    /// `buttons` is the DOM's `PointerEvent.buttons` (1 left, 2 right, 4
    /// middle) at the moment of the event. `at` is when it happened on the
    /// page, in milliseconds of the page's own clock (`performance.now()`):
    /// a touch's down and up can reach the host in one batch, behind a slow
    /// gesture, so only the page can say how long the finger was held. Pages
    /// that do not send it are timed by arrival.
    Pointer {
        phase: PointerPhase,
        x: f64,
        y: f64,
        buttons: u8,
        #[serde(default)]
        at: Option<f64>,
    },
    /// Wheel notches (`deltaY / 100`, clamped), positive down and right.
    Wheel { x: f64, y: f64, dx: f64, dy: f64 },
    /// `KeyboardEvent.key` and `.code`, one edge of the press. `altGraph` is
    /// the key-down's `getModifierState("AltGraph")`: AltGr is held, so `key`
    /// is a character the layout composed (`@` on a German keyboard), not a
    /// shortcut. Windows reports AltGr as a Control press of its own, so
    /// without this a phone would be sent Ctrl+@ — which no phone can type.
    /// Pages that do not send it are read as AltGr not held.
    Key {
        key: String,
        code: String,
        down: bool,
        #[serde(default, rename = "altGraph")]
        alt_graph: bool,
    },
    /// Composed or pasted text.
    Text { text: String },
    /// A toolbar button: a key name of [`super::keys`] (`Back`, `HomeScreen`,
    /// `AppSwitch`) or a chord (`ctrl+alt+Delete`).
    Button { name: String },
}

/// `POST /<token>/input` body.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerInput {
    /// The page's own id (random per page load).
    pub viewer: String,
    /// Counts up per page; a batch with a `seq` not above the last one applied
    /// from this page is stale and dropped.
    pub seq: u64,
    pub events: Vec<ViewerEvent>,
}

/// `POST /<token>/credentials` body.
///
/// The password arrives base64-encoded (of its UTF-8), as `passwordBase64`.
/// Not to hide it — the loopback page and its token are the protection — but so
/// that it is never a JSON string with escapes in it: serde_json unescapes into
/// a scratch buffer of its own, which is freed without being wiped. Base64 has
/// nothing to unescape, so the encoded text is read in place from the request
/// body (which is wiped) and decoded straight into memory that is wiped too.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewerCredentials {
    pub username: Option<String>,
    #[serde(rename = "passwordBase64", deserialize_with = "secret_from_base64")]
    pub password: Zeroizing<String>,
    pub remember: bool,
}

/// Decodes a base64 secret into a buffer that is wiped when it drops, without
/// an intermediate copy that is not.
fn secret_from_base64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Zeroizing<String>, D::Error> {
    struct Secret;

    impl serde::de::Visitor<'_> for Secret {
        type Value = Zeroizing<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a base64 string")
        }

        fn visit_str<E: serde::de::Error>(self, encoded: &str) -> Result<Self::Value, E> {
            let engine = &base64::engine::general_purpose::STANDARD;
            let mut decoded = Zeroizing::new(vec![0u8; base64::decoded_len_estimate(encoded.len())]);
            // The error says nothing about the value, which is the secret.
            let length = engine
                .decode_slice(encoded, &mut decoded)
                .map_err(|_| E::custom("the password is not valid base64"))?;
            decoded.truncate(length);
            // The text takes over the buffer itself; a buffer that is not UTF-8 is
            // wiped as it drops.
            match String::from_utf8(std::mem::take(&mut *decoded)) {
                Ok(text) => Ok(Zeroizing::new(text)),
                Err(error) => {
                    drop(Zeroizing::new(error.into_bytes()));
                    Err(E::custom("the password is not UTF-8"))
                }
            }
        }
    }

    deserializer.deserialize_str(Secret)
}

impl std::fmt::Debug for ViewerCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ViewerCredentials")
            .field("username", &self.username)
            .field("remember", &self.remember)
            .finish_non_exhaustive()
    }
}

/// The session as the viewer server sees it.
pub trait ViewerBackend: Send + Sync {
    fn state(&self) -> ViewerState;

    /// Blocks up to `timeout` for a frame whose `seq` is above `after`, already
    /// encoded. `None` when nothing newer came (the server then resends the
    /// latest as a keepalive).
    fn next_frame(&self, after: u64, timeout: Duration) -> Option<ViewerFrame>;

    /// The newest encoded frame, if there has been one.
    fn latest_frame(&self) -> Option<ViewerFrame>;

    /// A batch from one page, already checked for order by the server.
    fn input(&self, viewer: &str, events: Vec<ViewerEvent>);

    fn credentials(&self, credentials: ViewerCredentials);

    /// Whether page `viewer` is visible. Only visible pages with an open stream
    /// make a pulling driver capture frames.
    fn set_viewer_visible(&self, viewer: &str, visible: bool);

    /// Page `viewer`'s stream ended (the page went away or was hidden). Called
    /// while the server keeps a page's stream openings and endings in order, so
    /// it must not wait on the screen.
    fn viewer_gone(&self, viewer: &str);

    /// Right after [`Self::viewer_gone`], outside that ordering: whatever page
    /// `viewer` still holds down (a key, a mouse button) comes up. This may
    /// wait for the screen.
    fn release_viewer(&self, _viewer: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_input_reads_the_page_wire_format() {
        let input: ViewerInput = serde_json::from_str(
            r#"{"viewer":"v1","seq":7,"events":[
                {"t":"pointer","phase":"down","x":0.5,"y":0.25,"buttons":1},
                {"t":"wheel","x":0.5,"y":0.5,"dx":0,"dy":1.5},
                {"t":"key","key":"a","code":"KeyA","down":true},
                {"t":"text","text":"你好"},
                {"t":"button","name":"Back"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(input.seq, 7);
        assert_eq!(input.events.len(), 5);
        assert_eq!(
            input.events[0],
            ViewerEvent::Pointer {
                phase: PointerPhase::Down,
                x: 0.5,
                y: 0.25,
                buttons: 1,
                at: None
            }
        );
    }

    /// A page's own clock goes with each pointer event; a batch without it is still read.
    #[test]
    fn pointer_events_carry_the_pages_time_when_it_sends_one() {
        let input: ViewerInput = serde_json::from_str(
            r#"{"viewer":"v1","seq":1,"events":[
                {"t":"pointer","phase":"down","x":0.5,"y":0.5,"buttons":1,"at":1500.25},
                {"t":"pointer","phase":"up","x":0.5,"y":0.5,"buttons":0}
            ]}"#,
        )
        .unwrap();
        assert!(matches!(
            input.events[0],
            ViewerEvent::Pointer { at: Some(at), .. } if at == 1500.25
        ));
        assert!(matches!(input.events[1], ViewerEvent::Pointer { at: None, .. }));
    }

    #[test]
    fn credentials_decode_the_password_and_never_print_it() {
        // "hunter2 \"quoted\" 密码" as UTF-8, base64.
        let encoded = base64::engine::general_purpose::STANDARD
            .encode("hunter2 \"quoted\" 密码".as_bytes());
        let credentials: ViewerCredentials = serde_json::from_str(&format!(
            r#"{{"username":"me","passwordBase64":"{encoded}","remember":true}}"#
        ))
        .unwrap();
        assert_eq!(credentials.password.as_str(), "hunter2 \"quoted\" 密码");
        assert!(!format!("{credentials:?}").contains("hunter2"));
        for body in [
            r#"{"passwordBase64":"not base64!","remember":true}"#,
            r#"{"passwordBase64":"/w==","remember":true}"#,
            r#"{"password":"hunter2","remember":true}"#,
        ] {
            let error = serde_json::from_str::<ViewerCredentials>(body)
                .err()
                .expect(body)
                .to_string();
            assert!(!error.contains("hunter2") && !error.contains("/w=="), "{error}");
        }
    }
}
