//! Live end-to-end check of how a background task's result reaches the model
//! on the ChatGPT Codex backend, through the production `run_model` loop and
//! the real AI SDK sidecar.
//!
//! - GPT-6 Astra takes asynchronous tool calls: `agent_spawn` is declared
//!   `async`, the turn ends after the launch, and the next turn hands the
//!   child's result over as the launch's own output.
//! - Every GPT-5 model on the backend refuses an `async` tool, so it gets the
//!   receipt at once and the result later as a Claude Code-style user message.
//!
//! The session is the Codex CLI's own sign-in (`~/.codex/auth.json`, or the
//! file `MEWRK_CODEX_LIVE_AUTH` names). Only its access token is used, never
//! its refresh token, so the CLI's session is left as it was.
//! `MEWRK_CODEX_LIVE_BASE_URL` may point the requests at a loopback relay
//! that records what Mewrk sends.
//!
//! ```sh
//! cargo test --lib -- codex_async_live --ignored --nocapture --test-threads=1
//! ```

use super::*;

const AUTH_ENV: &str = "MEWRK_CODEX_LIVE_AUTH";
const BASE_ENV: &str = "MEWRK_CODEX_LIVE_BASE_URL";
const ASYNC_MODEL_ENV: &str = "MEWRK_CODEX_LIVE_ASYNC_MODEL";
const SYNC_MODEL_ENV: &str = "MEWRK_CODEX_LIVE_SYNC_MODEL";

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

/// Puts the Codex CLI's access token into the test process's own Codex store
/// under `provider_id`. The refresh token stays behind: the access token is
/// valid for days, and a refresh would rotate the CLI's token out from under it.
fn install_cli_session(provider_id: &str) {
    let path = std::env::var(AUTH_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("home directory")
                .join(".codex")
                .join("auth.json")
        });
    let auth: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "sign in with the Codex CLI first ({}): {error}",
                path.display()
            )
        }))
        .expect("auth.json is JSON");
    let tokens = &auth["tokens"];
    let access = tokens["access_token"].as_str().expect("an access token");
    let account = tokens["account_id"].as_str().expect("a ChatGPT account id");
    crate::codex_oauth::test_support::install_session(
        crate::codex_oauth::host(),
        provider_id,
        access,
        "not-the-cli-refresh-token",
        account,
    );
}

/// A fresh conversation on the Codex backend with `agent_spawn` enabled, the
/// model's protocol capabilities filled in the way model discovery fills them.
fn codex_request(model_id: &str, workspace: &std::path::Path, prompt: &str) -> RunModelRequest {
    let mut request = run_request(ProviderFamily::OpenaiCodex);
    request.provider.id = "codex-live".into();
    request.provider.name = "OpenAI Codex".into();
    request.provider.base_url = env_or(BASE_ENV, "");
    request.model.id = model_id.to_owned();
    request.model.capabilities =
        crate::model_discovery::known_protocol_capabilities(&request.provider, model_id)
            .into_iter()
            .collect();
    request.model.max_output_tokens = None;
    request.reasoning_effort = ReasoningEffort::Low;
    request.tools = catalog::tool_catalog();
    request.enabled_tools = vec!["agent_spawn".into()];
    crate::agents::apply_task_runtime_tools(&mut request.enabled_tools, request.host_message_container);
    request.workspace_path = workspace.to_string_lossy().into_owned();
    request.conversation_id = format!("codex-live-{model_id}");
    request.assembled_system_prompt =
        "You are Mewrk's engineering agent. Use the available tools to complete the task and report the result concisely.".into();
    request.contexts = vec![ContextItem::User {
        id: "live-user-1".into(),
        content: prompt.to_owned(),
        images: Vec::new(),
        files: Vec::new(),
        created_at: "2026-10-05T00:00:00Z".into(),
    }];
    install_cli_session(&request.provider.id);
    request
}

const LAUNCH_PROMPT: &str = "Start exactly one background subagent named `hello` whose only task is to reply with the single word PINEAPPLE. Tell me you started it, then end your turn without waiting for it. When its result reaches you, tell me exactly what it answered.";

fn final_text(contexts: &[ContextItem]) -> String {
    contexts
        .iter()
        .rev()
        .find_map(|context| match context {
            ContextItem::Assistant { content, .. } if !content.trim().is_empty() => {
                Some(content.clone())
            }
            _ => None,
        })
        .unwrap_or_default()
}

fn tool_cards(contexts: &[ContextItem]) -> Vec<(String, Option<String>, String)> {
    contexts
        .iter()
        .filter_map(|context| match context {
            ContextItem::Tool {
                id,
                tool_name,
                provider_call_id,
                result,
                ..
            } => Some((
                format!("{tool_name} [{id}]"),
                provider_call_id.clone(),
                result.output.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// Waits for the background child to finish while the conversation is idle.
fn wait_for_child(state: &AppState, conversation_id: &str) {
    let tasks = state.conversation_tasks(conversation_id);
    let deadline = Instant::now() + Duration::from_secs(300);
    while !tasks.pool.has_undrained_foldable_results() {
        assert!(Instant::now() < deadline, "the child never finished");
        thread::sleep(Duration::from_millis(200));
    }
}

/// Launches and, unless the child's result already arrived before the launch
/// turn settled, lets the child finish while idle and runs the wake turn.
/// Returns both turns' new contexts (the second empty without a wake).
fn launch_then_wake(
    model_id: &str,
) -> (
    AppState,
    RunModelRequest,
    Vec<ContextItem>,
    Vec<ContextItem>,
) {
    let workspace = tempfile::tempdir().unwrap();
    let request = codex_request(model_id, workspace.path(), LAUNCH_PROMPT);
    let state = AppState::default();
    register_quiet_task_surface(&state, &request.conversation_id);

    let first = run_model(request.clone(), &state, &discard_event, &approve_tool)
        .unwrap_or_else(|error| panic!("{model_id}: the launch turn failed: {error}"));
    assert!(first.error.is_none(), "{model_id}: {:?}", first.error);
    eprintln!(
        "[{model_id}] turn 1 cards: {:#?}",
        tool_cards(&first.contexts)
    );
    eprintln!(
        "[{model_id}] turn 1 reply: {:?}",
        final_text(&first.contexts)
    );

    if first
        .contexts
        .iter()
        .any(|context| context.id().starts_with(AGENT_RESULT_CONTEXT_ID_PREFIX))
    {
        return (state, request, first.contexts, Vec::new());
    }
    wait_for_child(&state, &request.conversation_id);

    let mut wake = request.clone();
    wake.contexts.extend(first.contexts.clone());
    let second = run_model(wake, &state, &discard_event, &approve_tool)
        .unwrap_or_else(|error| panic!("{model_id}: the wake turn failed: {error}"));
    assert!(second.error.is_none(), "{model_id}: {:?}", second.error);
    eprintln!(
        "[{model_id}] turn 2 cards: {:#?}",
        tool_cards(&second.contexts)
    );
    eprintln!(
        "[{model_id}] turn 2 reply: {:?}",
        final_text(&second.contexts)
    );
    (state, request, first.contexts, second.contexts)
}

fn launch_card(contexts: &[ContextItem]) -> String {
    contexts
        .iter()
        .find_map(|context| match context {
            ContextItem::Tool {
                tool_name,
                provider_call_id: Some(call),
                result,
                ..
            } if tool_name == "agent_spawn" && result.success => Some(call.clone()),
            _ => None,
        })
        .expect("the model started the subagent")
}

fn delivery_card(contexts: &[ContextItem]) -> (Option<String>, String) {
    contexts
        .iter()
        .find_map(|context| match context {
            ContextItem::Tool {
                id,
                provider_call_id,
                result,
                ..
            } if id.starts_with(AGENT_RESULT_CONTEXT_ID_PREFIX) => {
                Some((provider_call_id.clone(), result.output.clone()))
            }
            _ => None,
        })
        .expect("the child's result was delivered")
}

#[test]
#[ignore = "live network test against the ChatGPT Codex backend"]
fn codex_async_live_the_result_lands_on_the_launch() {
    let model_id = env_or(ASYNC_MODEL_ENV, "gpt-6-astra");
    let (state, request, first, second) = launch_then_wake(&model_id);
    assert!(
        request.model.has(crate::model::ModelCapability::AsyncTools),
        "{model_id} is known to take async calls"
    );

    let launch = launch_card(&first);
    // The launch turn ended on the launch: no delivery yet, and no second
    // request owed a receipt.
    assert!(!first
        .iter()
        .any(|context| context.id().starts_with(AGENT_RESULT_CONTEXT_ID_PREFIX)));

    let (answers, output) = delivery_card(&second);
    assert_eq!(
        answers.as_deref(),
        Some(launch.as_str()),
        "the result answers the launch"
    );
    assert!(output.starts_with("<task-notification>"), "{output}");
    assert!(!output.contains("SYSTEM NOTIFICATION"), "{output}");
    assert!(output.contains("PINEAPPLE"), "{output}");
    assert!(
        final_text(&second).contains("PINEAPPLE"),
        "{}",
        final_text(&second)
    );
    let tasks = state.conversation_tasks(&request.conversation_id);
    assert!(
        tasks.shadow.divergences().is_empty(),
        "{:?}",
        tasks.shadow.divergences()
    );
}

#[test]
#[ignore = "live network test against the ChatGPT Codex backend"]
fn codex_async_live_other_models_get_the_result_as_a_user_message() {
    let model_id = env_or(SYNC_MODEL_ENV, "gpt-5.5");
    let (state, request, first, second) = launch_then_wake(&model_id);
    assert!(
        !request.model.has(crate::model::ModelCapability::AsyncTools),
        "{model_id} refuses async tools"
    );

    // The receipt is the launch's output, so the turn goes on; the result
    // comes before it settles, or on the wake after.
    let launch = launch_card(&first);
    let both = first.iter().chain(&second).cloned().collect::<Vec<_>>();
    let (answers, output) = delivery_card(&both);
    // The card still names the launch, so a model that takes asynchronous
    // calls, should the conversation move to one, reads it as that output.
    assert_eq!(answers.as_deref(), Some(launch.as_str()));
    assert!(
        output.starts_with("<system-reminder>\n[SYSTEM NOTIFICATION - NOT USER INPUT]"),
        "{output}"
    );
    assert!(output.contains("<task-notification>"), "{output}");
    assert!(output.contains("PINEAPPLE"), "{output}");
    assert!(
        final_text(&both).contains("PINEAPPLE"),
        "{}",
        final_text(&both)
    );
    let tasks = state.conversation_tasks(&request.conversation_id);
    assert!(
        tasks.shadow.divergences().is_empty(),
        "{:?}",
        tasks.shadow.divergences()
    );
}

#[test]
#[ignore = "live network test against the ChatGPT Codex backend"]
fn codex_async_live_a_wait_reports_status_and_the_result_lands_on_the_launch() {
    let model_id = env_or(ASYNC_MODEL_ENV, "gpt-6-astra");
    let workspace = tempfile::tempdir().unwrap();
    let request = codex_request(
        &model_id,
        workspace.path(),
        "Start exactly one background subagent named `hello` whose only task is to reply with the single word PINEAPPLE. Then call task_wait on it, and once you have its answer, tell me exactly what it answered.",
    );
    let state = AppState::default();
    register_quiet_task_surface(&state, &request.conversation_id);

    let response = run_model(request.clone(), &state, &discard_event, &approve_tool)
        .unwrap_or_else(|error| panic!("{model_id}: {error}"));
    assert!(response.error.is_none(), "{:?}", response.error);
    let cards = tool_cards(&response.contexts);
    eprintln!("[{model_id}] cards: {cards:#?}");
    eprintln!("[{model_id}] reply: {:?}", final_text(&response.contexts));

    let launch = launch_card(&response.contexts);
    let waits = cards
        .iter()
        .filter(|(name, _, _)| name.starts_with("task_wait "))
        .map(|(_, _, output)| output.as_str())
        .collect::<Vec<_>>();
    assert!(!waits.is_empty(), "the model waited");
    assert!(
        waits.iter().all(|output| !output.contains("PINEAPPLE")),
        "a wait reports status only: {waits:?}"
    );
    assert!(
        waits
            .iter()
            .any(|output| output.contains(PromptKey::TaskWaitDeliveredOnCall.builtin_en())),
        "{waits:?}"
    );
    let (answers, output) = delivery_card(&response.contexts);
    assert_eq!(answers.as_deref(), Some(launch.as_str()));
    assert!(output.contains("PINEAPPLE"), "{output}");
    assert!(
        final_text(&response.contexts).contains("PINEAPPLE"),
        "{}",
        final_text(&response.contexts)
    );
    let tasks = state.conversation_tasks(&request.conversation_id);
    assert!(
        tasks.shadow.divergences().is_empty(),
        "{:?}",
        tasks.shadow.divergences()
    );
}

/// One transcript, read by either kind of model: a conversation that started
/// on one side carries on on the other, and the backend accepts the replay.
#[test]
#[ignore = "live network test against the ChatGPT Codex backend"]
fn codex_async_live_a_conversation_moves_between_models_both_ways() {
    let async_model = env_or(ASYNC_MODEL_ENV, "gpt-6-astra");
    let sync_model = env_or(SYNC_MODEL_ENV, "gpt-5.5");
    for (from, to) in [(&sync_model, &async_model), (&async_model, &sync_model)] {
        let (state, request, first, second) = launch_then_wake(from);
        let workspace = tempfile::tempdir().unwrap();
        let mut follow_up = codex_request(to, workspace.path(), LAUNCH_PROMPT);
        follow_up.conversation_id = request.conversation_id.clone();
        follow_up.contexts = request.contexts.clone();
        follow_up.contexts.extend(first);
        follow_up.contexts.extend(second);
        follow_up.contexts.push(ContextItem::User {
            id: "live-user-2".into(),
            content: "Which word did `hello` answer? Reply with that word only.".into(),
            images: Vec::new(),
            files: Vec::new(),
            created_at: "2026-10-05T00:01:00Z".into(),
        });
        let response = run_model(follow_up, &state, &discard_event, &approve_tool)
            .unwrap_or_else(|error| panic!("{from} → {to}: {error}"));
        assert!(
            response.error.is_none(),
            "{from} → {to}: {:?}",
            response.error
        );
        let reply = final_text(&response.contexts);
        eprintln!("[{from} → {to}] reply: {reply:?}");
        assert!(reply.contains("PINEAPPLE"), "{from} → {to}: {reply}");
    }
}
