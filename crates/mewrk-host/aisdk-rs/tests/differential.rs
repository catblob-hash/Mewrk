//! The same frames through the original Node sidecar and through this crate, against the same scripted upstream:
//! the requests must match byte for byte, the frames value for value.
//!
//! Skipped unless `MEWRK_AISDK_NODE_SIDECAR` names a built `aisdk-service/dist/main.mjs`.

mod common;

use common::{Mock, Reply, Rust, comparable_frames, comparable_request, node_sidecar_path, run_node};
use mewrk_aisdk::json::{Json, Obj};

/// Runs `payload` (its `baseURL`, when `base_path` is `Some`, set to the mock's) both ways.
pub fn differential(name: &str, base_path: Option<&str>, payload: Json, replies: Vec<Reply>) {
    differential_normalized(name, base_path, payload, replies, &|text| text.to_owned());
}

/// As [`differential`], with `normalize` applied to each request and frame text first, for what is legitimately
/// random on both sides (generated ids, timestamps).
pub fn differential_normalized(name: &str, base_path: Option<&str>, payload: Json, replies: Vec<Reply>, normalize: &dyn Fn(&str) -> String) {
    let Some(node) = node_sidecar_path() else {
        eprintln!("{name}: skipped, MEWRK_AISDK_NODE_SIDECAR is not set");
        return;
    };
    let with_base = |mock: &Mock| {
        let mut object: Obj = payload.as_obj().unwrap().clone();
        if let Some(path) = base_path {
            object.insert("baseURL", format!("{}{path}", mock.base));
        }
        Json::Obj(object)
    };
    let node_mock = Mock::script(replies.clone());
    let node_outcome = run_node(&node, &with_base(&node_mock));
    let rust_mock = Mock::script(replies);
    let rust = Rust::start();
    let rust_outcome = rust.run("s", &with_base(&rust_mock));

    let (node_requests, rust_requests) = (node_mock.requests(), rust_mock.requests());
    assert_eq!(node_requests.len(), rust_requests.len(), "{name}: request count");
    for (node_request, rust_request) in node_requests.iter().zip(&rust_requests) {
        assert_eq!(normalize(&comparable_request(node_request)), normalize(&comparable_request(rust_request)), "{name}: request");
    }
    let frames = |outcome| comparable_frames(outcome).iter().map(|frame| normalize(frame)).collect::<Vec<_>>();
    assert_eq!(frames(&node_outcome), frames(&rust_outcome), "{name}: frames");
}

#[path = "differential_cases/anthropic.rs"]
mod anthropic_cases;
#[path = "differential_cases/openai_responses.rs"]
mod openai_responses_cases;
#[path = "differential_cases/openai_compatible.rs"]
mod openai_compatible_cases;
#[path = "differential_cases/xai.rs"]
mod xai_cases;
#[path = "differential_cases/google.rs"]
mod google_cases;
#[path = "differential_cases/bedrock.rs"]
mod bedrock_cases;
