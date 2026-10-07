//! Calls whose result arrives later, on the call itself.
//!
//! A background task is started by a call that returns at once — `agent_spawn`
//! answers `ok`, `workflow` the same — and its result comes much later. How
//! that result reaches the model depends on the model:
//!
//! - **A model that takes asynchronous tool calls** (OpenAI's Responses
//!   protocol, `async: true` on a function tool, GPT-6 Astra and later):
//!   `agent_spawn` and `workflow` are declared asynchronous. The launch has no
//!   output of its own; the task's result is the call's output, sent in a later
//!   request under the call's id. The model goes on working meanwhile, and a
//!   response that only launched tasks ends the turn like a final reply.
//! - **Every other model** gets the launch's receipt as the call's output, and
//!   the result later as a user-role message, the way Claude Code delivers a
//!   `<task-notification>` (`wire_history::task_notification_message`).
//!
//! Both are projections of one transcript. The launch card keeps its receipt
//! and is marked as a deferred launch on the wire ([`is_deferred_launch`]); the
//! result's delivery card names the call it answers (its `provider_call_id`).
//! The sidecar turns the pair into the asynchronous form where the request
//! declares asynchronous tools, and leaves both as they are everywhere else, so
//! a conversation that changes models replays correctly either way
//! (`aisdk-service/src/async-tools.ts`).
//!
//! Not to be confused with `agents::ASYNC_TOOL_NAMES`: `web_search` and
//! `web_fetch` run concurrently inside one round and settle before it ends.
//! Background shell commands are not covered either: a shell call may run in
//! the foreground or the background, while `async: true` is a property of the
//! tool, so a background command's result always arrives as a user message.

use crate::model::{ApiProvider, ModelCapability, ModelProfile, ProviderFamily, ToolResult};

/// The tools declared asynchronous where the model takes asynchronous calls:
/// the two that only ever start a background task.
pub(crate) const ASYNC_DECLARED_TOOLS: [&str; 2] = ["agent_spawn", "workflow"];

/// Whether a conversation on this model, at this provider's endpoint, declares
/// [`ASYNC_DECLARED_TOOLS`] asynchronous: the model declares the `AsyncTools`
/// capability — filled in by Mewrk where it knows ([`known`]), by the user
/// everywhere else — and its protocol has the interface. Mirrored by TS
/// `takesAsyncTools` in `src/lib/modelCapabilities.ts`.
pub(crate) fn takes_async_tools(provider: &ApiProvider, model: &ModelProfile) -> bool {
    provider.family.async_tools_take_effect() && model.has(ModelCapability::AsyncTools)
}

/// What Mewrk knows about this model taking asynchronous tool calls at this
/// endpoint: `Some` where it knows, `None` where only the user can say.
///
/// Only the Responses protocol has the interface, and OpenAI documents it for
/// GPT-6 Astra and later models ([`openai_documents`]). On the ChatGPT Codex
/// backend every GPT-6 model takes them ([`codex_backend_takes`]). A relay in
/// front of OpenAI and Azure (whose model names are deployments) may or may
/// not pass `async` on, so those are the user's.
pub(crate) fn known(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    match family {
        ProviderFamily::OpenaiResponses => crate::host_append::at_vendor_endpoint(family, base_url)
            .then(|| openai_documents(model_id))
            .flatten(),
        ProviderFamily::OpenaiCodex => codex_backend_takes(model_id),
        ProviderFamily::Azure => None,
        ProviderFamily::Anthropic
        | ProviderFamily::ClaudeAgent
        | ProviderFamily::OpenaiChat
        | ProviderFamily::Google
        | ProviderFamily::Xai
        | ProviderFamily::Bedrock
        | ProviderFamily::Vertex
        | ProviderFamily::OpenaiCompatible => Some(false),
    }
}

/// What OpenAI documents about `model_id` taking asynchronous tool calls:
/// GPT-6 Astra and every later model do, every GPT generation before 6 and
/// the o-series do not. GPT-6 Sol and Luna came out before Astra and the
/// documentation names neither, so they — like a model id that is not a GPT
/// version at all — are the user's to say.
fn openai_documents(model_id: &str) -> Option<bool> {
    match openai_version(&model_id.trim().to_ascii_lowercase())? {
        OpenaiVersion::Before6 => Some(false),
        OpenaiVersion::Gpt { major: 6, minor: 0, variant } => {
            (variant == "-astra" || variant.starts_with("-astra-")).then_some(true)
        }
        OpenaiVersion::Gpt { .. } => Some(true),
    }
}

/// Whether the ChatGPT Codex backend takes asynchronous calls from
/// `model_id`, as measured against it on 2026-10-05: every GPT-6 model it
/// serves does (Sol and Luna as well as Astra), and every GPT-5 model refuses
/// a request that declares one ("Async tools are not supported with …"). A
/// model id that is not a GPT version (`gpt-reserve` refuses too) is the
/// user's to say.
fn codex_backend_takes(model_id: &str) -> Option<bool> {
    match openai_version(&model_id.trim().to_ascii_lowercase())? {
        OpenaiVersion::Before6 => Some(false),
        OpenaiVersion::Gpt { .. } => Some(true),
    }
}

enum OpenaiVersion<'a> {
    /// A GPT generation before 6, or an o-series, ChatGPT or Codex model.
    Before6,
    Gpt {
        major: u32,
        minor: u32,
        /// What follows the version number, such as `-astra`.
        variant: &'a str,
    },
}

/// The OpenAI model generation a trimmed, lowercased model id names, or
/// `None` for an id that is not a GPT version.
fn openai_version(id: &str) -> Option<OpenaiVersion<'_>> {
    if ["o1", "o3", "o4", "chatgpt-", "codex-"]
        .iter()
        .any(|prefix| id.starts_with(prefix))
    {
        return Some(OpenaiVersion::Before6);
    }
    let version = id.strip_prefix("gpt-")?;
    let major_end = version
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(version.len());
    let major = version[..major_end].parse::<u32>().ok()?;
    let rest = &version[major_end..];
    let (minor, variant) = match rest.strip_prefix('.') {
        Some(after) => {
            let minor_end = after
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(after.len());
            (after[..minor_end].parse::<u32>().ok()?, &after[minor_end..])
        }
        None => (0, rest),
    };
    Some(if major <= 5 {
        OpenaiVersion::Before6
    } else {
        OpenaiVersion::Gpt { major, minor, variant }
    })
}

/// Whether a call started a background task whose result is still to come as
/// the call's own output — the launch an asynchronous request leaves without
/// an output. Only a launch that answered with the bare acknowledgement
/// qualifies: a workflow that had to be renumbered answers with its new id,
/// which the model needs now, and a refused launch answers with the refusal.
pub(crate) fn is_deferred_launch(tool_name: &str, result: &ToolResult) -> bool {
    ASYNC_DECLARED_TOOLS.contains(&tool_name)
        && result.success
        && result.output.trim() == crate::orchestration::SPAWN_ACK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(success: bool, output: &str) -> ToolResult {
        ToolResult {
            success,
            output: output.into(),
            images: Vec::new(),
            diff: None,
            executed_at: String::new(),
            duration_ms: 0,
        }
    }

    #[test]
    fn only_a_bare_acknowledgement_from_a_task_launch_defers_its_result() {
        assert!(is_deferred_launch("agent_spawn", &result(true, "ok")));
        assert!(is_deferred_launch("workflow", &result(true, "ok\n")));
        // A renumbered workflow's id is news the model needs now.
        assert!(!is_deferred_launch("workflow", &result(true, "workflow:review-2")));
        assert!(!is_deferred_launch("agent_spawn", &result(false, "ok")));
        // A background shell command answers with its address, and a shell tool
        // is not declared asynchronous.
        assert!(!is_deferred_launch("bash", &result(true, "ok")));
    }

    #[test]
    fn openai_documents_gpt_6_astra_and_later() {
        for (model, expected) in [
            ("gpt-6-astra", Some(true)),
            ("gpt-6-astra-2026-09-03", Some(true)),
            ("GPT-6-Astra", Some(true)),
            ("gpt-6.1-sol", Some(true)),
            ("gpt-6.2", Some(true)),
            ("gpt-7", Some(true)),
            ("gpt-6-sol", None),
            ("gpt-6-luna", None),
            ("gpt-6", None),
            ("gpt-5.6-sol", Some(false)),
            ("gpt-5.5", Some(false)),
            ("gpt-4o", Some(false)),
            ("o3-mini", Some(false)),
            ("chatgpt-4o-latest", Some(false)),
            ("gpt-reserve", None),
            ("gpt-oss-120b", None),
            ("model-test", None),
        ] {
            assert_eq!(openai_documents(model), expected, "{model}");
        }
    }

    #[test]
    fn the_codex_backend_takes_them_from_every_gpt_6_model() {
        for (model, expected) in [
            ("gpt-6-astra", Some(true)),
            ("gpt-6-sol", Some(true)),
            ("gpt-6.1-sol", Some(true)),
            ("GPT-6-Luna", Some(true)),
            ("gpt-7", Some(true)),
            ("gpt-5.6-sol", Some(false)),
            ("gpt-5.6-terra", Some(false)),
            ("gpt-5.5", Some(false)),
            ("codex-auto-review", Some(false)),
            ("gpt-reserve", None),
        ] {
            assert_eq!(codex_backend_takes(model), expected, "{model}");
        }
    }
}
