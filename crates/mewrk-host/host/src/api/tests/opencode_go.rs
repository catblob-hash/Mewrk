//! What reaches the OpenCode gateway.
//!
//! The hermetic test reads the request off the wire. The ignored ones ask the
//! real gateway, and need network access but no key: it checks the session before
//! the credential, so `401` is the answer of a request it would have served and
//! `400` the answer of one it refuses whoever sends it.
//!
//! ```sh
//! cargo test -p mewrk-host --lib -- opencode_go_live --ignored --nocapture
//! ```

use super::*;
use crate::model::ModelCapability;

const GO: &str = "https://opencode.ai/inference/go";

/// Each protocol's mount under [`GO`], with a model served natively there.
const MOUNTS: [(ProviderFamily, &str, &str); 4] = [
    (ProviderFamily::OpenaiChat, "openai/v1", "glm-5.3-flash"),
    (ProviderFamily::OpenaiResponses, "openai/v1", "gpt-5.6-luna"),
    (ProviderFamily::Anthropic, "anthropic/v1", "minimax-m3"),
    (ProviderFamily::Google, "google/v1beta", "gemini-4-argon"),
];

fn go_request(family: ProviderFamily, mount: &str, model: &str) -> RunModelRequest {
    let mut request = run_request(family);
    request.provider.base_url = format!("{GO}/{mount}");
    request.model.id = model.into();
    request.conversation_id = "conv_opencode_go".into();
    request
}

fn answer_to(step: &crate::aisdk::protocol::StepRequest) -> String {
    match post_model_request(step, None, 1, &discard_event) {
        Ok(_) => panic!("no key was sent, so the request cannot succeed"),
        Err(exhausted) => exhausted.error.message().to_owned(),
    }
}

#[test]
fn every_protocol_sends_the_session_and_names_mewrk_first() {
    for (family, mount, model) in MOUNTS {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request_with_body(&mut stream);
            write_json_response(
                &mut stream,
                "401 Unauthorized",
                &json!({"error":{"message":"no key"}}),
            );
            request
        });

        let request = go_request(family, mount, model);
        let mut step = step_request_with(&request, &[], &format!("http://{address}/{mount}"));
        step.headers = step_credential(&request, &None).unwrap().headers;
        answer_to(&step);

        let seen = server.join().unwrap().to_ascii_lowercase();
        assert!(
            seen.contains("\r\nx-opencode-session: conv_opencode_go\r\n"),
            "{mount}: {seen}"
        );
        assert!(seen.contains("\r\nuser-agent: mewrk/"), "{mount}: {seen}");
    }
}

#[test]
fn a_provider_elsewhere_sends_neither() {
    let mut request = run_request(ProviderFamily::OpenaiChat);
    request.provider.base_url = "https://relay.example.com/inference/go/openai/v1".into();
    assert!(step_credential(&request, &None).unwrap().headers.is_empty());
}

#[test]
#[ignore = "live network test against opencode.ai; needs no key"]
fn opencode_go_live_every_protocol_passes_the_session_check() {
    for (family, mount, model) in MOUNTS {
        let request = go_request(family, mount, model);
        let mut step = step_request_with(&request, &[], &request.provider.base_url.clone());

        let refused = answer_to(&step);
        assert!(refused.contains("HTTP 400"), "{mount} {model}: {refused}");

        step.headers = step_credential(&request, &None).unwrap().headers;
        let answered = answer_to(&step);
        println!("{mount} {model}: {answered}");
        assert!(answered.contains("HTTP 401"), "{mount} {model}: {answered}");
    }
}

/// The production path rather than a step assembled here: a key that is not
/// real gets the whole run as far as the gateway's credential check.
#[test]
#[ignore = "live network test against opencode.ai; needs no key"]
fn opencode_go_live_a_whole_run_passes_the_session_check() {
    let workspace = tempfile::tempdir().unwrap();
    let mut request = go_request(ProviderFamily::OpenaiChat, "openai/v1", "glm-5.3-flash");
    request.provider.id = "provider_opencode_go_live_test".into();
    request.workspace_path = workspace.path().to_string_lossy().into_owned();
    request.app_data_path = request.workspace_path.clone();
    save_api_key(&request.provider.id, "not-a-real-key").unwrap();

    let provider_id = request.provider.id.clone();
    let response = run_model(request, &AppState::default(), &discard_event, &approve_tool).unwrap();
    delete_api_key(&provider_id).unwrap();

    let error = response.error.expect("the key is not real");
    println!("{}", error.message);
    assert!(error.message.contains("HTTP 401"), "{}", error.message);
}

#[test]
#[ignore = "live network test against opencode.ai; needs no key"]
fn opencode_go_live_every_protocol_lists_the_catalog() {
    for (family, mount, _) in MOUNTS {
        let mut provider = provider(family);
        provider.base_url = format!("{GO}/{mount}");
        let models = fetch_models(&provider).unwrap_or_else(|error| panic!("{mount}: {error}"));
        println!("{mount}: {} models", models.len());
        let listed = |id: &str| models.iter().find(|model| model.id == id);
        assert!(listed("glm-5.3-flash").is_some(), "{mount}");
        let reads_images = |id: &str| {
            listed(id).is_some_and(|model| {
                model
                    .capabilities
                    .contains(&ModelCapability::ImageRecognition)
            })
        };
        // Named in the catalog by MIME type, and in no built-in table of ours.
        assert!(reads_images("kimi-k3.1-pro"), "{mount}");
        assert!(!reads_images("deepseek-v4-pro"), "{mount}");
    }
}
