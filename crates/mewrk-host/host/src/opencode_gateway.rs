//! The OpenCode gateway at `opencode.ai`, which serves OpenCode Go and
//! pay-as-you-go.
//!
//! It speaks the ordinary protocols, with two differences a provider row cannot
//! express. Both are matched by host: the gateway is one fixed address, and a
//! setting would only ask the user to know what this file knows.
//!
//! - Go answers `400 MissingSessionID` to a request without a session ID. Its
//!   docs name `x-opencode-session` and ask for one stable value per
//!   conversation. `prompt_cache_key` in the body does not count, and neither
//!   does an empty header. The docs also ask the client to name itself in its
//!   user agent rather than an SDK.
//! - The model list sits beside the protocol mounts. `/inference/go/v1/models`
//!   answers; `/inference/go/openai/v1/models` is a 404.
//!
//! <https://opencode.ai/docs/console/go/#where-can-i-use-it>

use std::collections::BTreeMap;

use reqwest::Url;
use uuid::Uuid;

use crate::http_util::normalized_base_url;

const HOST: &str = "opencode.ai";

/// The path segments the gateway mounts a protocol under.
const PROTOCOL_MOUNTS: [&str; 3] = ["openai", "anthropic", "google"];

fn gateway_url(base_url: &str) -> Option<Url> {
    normalized_base_url(base_url)
        .ok()
        .filter(|url| url.host_str() == Some(HOST))
}

/// The headers a model request to `base_url` carries, empty anywhere but the
/// gateway so no other upstream sees the conversation's id.
///
/// `session` is the conversation's prompt cache key, so the routing the header
/// asks for and the cache it serves follow the same chain across compactions.
/// Without one a request has nothing to stay stable with, and the gateway still
/// refuses it, so it goes out under an id of its own.
pub(crate) fn request_headers(base_url: &str, session: &str) -> BTreeMap<String, String> {
    if gateway_url(base_url).is_none() {
        return BTreeMap::new();
    }
    let session = match session.trim() {
        "" => Uuid::new_v4().simple().to_string(),
        session => session.to_owned(),
    };
    BTreeMap::from([
        ("x-opencode-session".to_owned(), session),
        (
            "user-agent".to_owned(),
            concat!("Mewrk/", env!("CARGO_PKG_VERSION")).to_owned(),
        ),
    ])
}

/// The address the gateway lists models under for a provider configured at
/// `base_url`: `/inference/go/openai/v1` lists from `/inference/go/v1`. `None`
/// anywhere else, including a gateway address on no protocol mount, whose own
/// `/models` is already the one to ask.
pub(crate) fn catalog_base_url(base_url: &str) -> Option<String> {
    let mut url = gateway_url(base_url)?;
    let segments = url.path_segments()?.map(str::to_owned).collect::<Vec<_>>();
    let mount = segments
        .iter()
        .position(|segment| PROTOCOL_MOUNTS.contains(&segment.as_str()))?;
    let mut catalog = segments[..mount].to_vec();
    catalog.push("v1".to_owned());
    url.set_path(&format!("/{}", catalog.join("/")));
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GO_CHAT: &str = "https://opencode.ai/inference/go/openai/v1";

    #[test]
    fn a_gateway_request_carries_the_session_and_names_mewrk() {
        for base_url in [
            GO_CHAT,
            "https://opencode.ai/inference/go/anthropic/v1",
            "https://opencode.ai/inference/go/google/v1beta",
            "https://opencode.ai/inference/openai/v1",
            "https://opencode.ai/zen/go/v1",
            "https://OPENCODE.ai/inference/go/openai/v1/chat/completions",
        ] {
            let headers = request_headers(base_url, "conv_a");
            assert_eq!(headers["x-opencode-session"], "conv_a", "{base_url}");
            assert!(headers["user-agent"].starts_with("Mewrk/"), "{base_url}");
            assert_eq!(headers.len(), 2, "{base_url}");
        }
    }

    /// The conversation's id reaches no other upstream, including an address
    /// that only looks like the gateway's.
    #[test]
    fn no_other_address_receives_the_headers() {
        for base_url in [
            "https://api.openai.com/v1",
            "https://opencode.ai.example.com/inference/go/openai/v1",
            "https://relay.example.com/opencode.ai/inference/go/openai/v1",
            "https://api.opencode.ai/v1",
            "http://127.0.0.1:8080/v1",
            "",
            "not a url",
        ] {
            assert!(request_headers(base_url, "conv_a").is_empty(), "{base_url}");
        }
    }

    /// The gateway refuses a request with no session, or an empty one.
    #[test]
    fn a_request_with_no_conversation_goes_out_under_an_id_of_its_own() {
        for session in ["", "  "] {
            let first = request_headers(GO_CHAT, session);
            let second = request_headers(GO_CHAT, session);
            assert!(!first["x-opencode-session"].trim().is_empty());
            assert_ne!(first["x-opencode-session"], second["x-opencode-session"]);
        }
    }

    #[test]
    fn every_protocol_mount_lists_from_the_catalog_beside_it() {
        for (base_url, catalog) in [
            (GO_CHAT, "https://opencode.ai/inference/go/v1"),
            (
                "https://opencode.ai/inference/go/anthropic/v1",
                "https://opencode.ai/inference/go/v1",
            ),
            (
                "https://opencode.ai/inference/go/anthropic",
                "https://opencode.ai/inference/go/v1",
            ),
            (
                "https://opencode.ai/inference/go/google/v1beta",
                "https://opencode.ai/inference/go/v1",
            ),
            (
                "https://opencode.ai/inference/openai/v1",
                "https://opencode.ai/inference/v1",
            ),
            (
                "https://opencode.ai/inference/go/openai/v1/chat/completions",
                "https://opencode.ai/inference/go/v1",
            ),
        ] {
            assert_eq!(catalog_base_url(base_url).as_deref(), Some(catalog));
        }
    }

    #[test]
    fn an_address_on_no_protocol_mount_keeps_its_own_catalog() {
        for base_url in [
            "https://opencode.ai/zen/go/v1",
            "https://opencode.ai/inference/go/v1",
            "https://relay.example.com/inference/go/openai/v1",
            "not a url",
        ] {
            assert_eq!(catalog_base_url(base_url), None, "{base_url}");
        }
    }
}
