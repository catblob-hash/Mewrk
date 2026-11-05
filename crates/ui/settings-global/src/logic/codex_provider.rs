//! `lib/codexProvider.ts` and `lib/claudeAgentProvider.ts`: the two built-in rows, identified by family.

use mewrk_client::model::{ApiProvider, ProviderFamily};

pub const CODEX_PROVIDER_NAME: &str = "OpenAI Codex";
pub const CODEX_DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// The host's rejection for a sign-in the user cancelled, in each language it may have worded it in: an outcome, not an error.
pub const CODEX_SIGN_IN_CANCELLED_MESSAGES: &[&str] = &["Codex 登录已取消", "The Codex sign-in was cancelled"];

pub const CLAUDE_AGENT_PROVIDER_NAME: &str = "Claude Agent";
pub const CLAUDE_AGENT_LEGAL_URL: &str = "https://code.claude.com/docs/en/legal-and-compliance";

pub fn is_codex_provider(family: ProviderFamily) -> bool {
    family == ProviderFamily::OpenaiCodex
}

pub fn is_claude_agent_provider(family: ProviderFamily) -> bool {
    family == ProviderFamily::ClaudeAgent
}

/// Built-in is a row the user cannot create or delete.
pub fn is_builtin_provider(family: ProviderFamily) -> bool {
    is_codex_provider(family) || is_claude_agent_provider(family)
}

pub fn create_id(prefix: &str) -> String {
    format!("{prefix}_{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

fn built_in_row(name: &str, family: ProviderFamily) -> ApiProvider {
    ApiProvider { id: create_id("provider"), name: name.to_owned(), enabled: false, family, base_url: String::new(), family_settings: Default::default(), notes: String::new(), models: Vec::new(), active_model_id: None }
}

/// Exactly one Codex row: the first existing one, or a fresh disabled one. Existing order is retained.
pub fn ensure_codex_provider(providers: Vec<ApiProvider>) -> Vec<ApiProvider> {
    let mut found = false;
    let mut out: Vec<ApiProvider> = providers
        .into_iter()
        .filter(|p| {
            if !is_codex_provider(p.family) {
                return true;
            }
            let keep = !found;
            found = true;
            keep
        })
        .collect();
    if !found {
        out.push(built_in_row(CODEX_PROVIDER_NAME, ProviderFamily::OpenaiCodex));
    }
    out
}

/// Exactly one Claude Agent row. A `base_url` left over from older versions is flattened rather than rejected.
pub fn ensure_claude_agent_provider(providers: Vec<ApiProvider>) -> Vec<ApiProvider> {
    let mut found = false;
    let mut out: Vec<ApiProvider> = providers
        .into_iter()
        .filter(|p| {
            if !is_claude_agent_provider(p.family) {
                return true;
            }
            let keep = !found;
            found = true;
            keep
        })
        .map(|mut p| {
            if is_claude_agent_provider(p.family) {
                p.base_url.clear();
            }
            p
        })
        .collect();
    if !found {
        out.push(built_in_row(CLAUDE_AGENT_PROVIDER_NAME, ProviderFamily::ClaudeAgent));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, family: ProviderFamily) -> ApiProvider {
        ApiProvider { id: id.into(), name: id.into(), enabled: true, family, base_url: String::new(), family_settings: Default::default(), notes: String::new(), models: vec![], active_model_id: None }
    }

    // codexProvider.test.ts
    #[test]
    fn a_missing_codex_row_is_appended_disabled_and_keeps_the_others() {
        let out = ensure_codex_provider(vec![row("a", ProviderFamily::Anthropic)]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].family, ProviderFamily::OpenaiCodex);
        assert!(!out[1].enabled);
        assert_eq!(out[1].name, CODEX_PROVIDER_NAME);
    }

    #[test]
    fn only_the_first_codex_row_survives() {
        let out = ensure_codex_provider(vec![row("a", ProviderFamily::OpenaiCodex), row("b", ProviderFamily::OpenaiChat), row("c", ProviderFamily::OpenaiCodex)]);
        assert_eq!(out.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn built_in_means_codex_or_claude_agent() {
        assert!(is_builtin_provider(ProviderFamily::OpenaiCodex));
        assert!(is_builtin_provider(ProviderFamily::ClaudeAgent));
        assert!(!is_builtin_provider(ProviderFamily::OpenaiCompatible));
    }

    #[test]
    fn a_stale_address_on_claude_agent_is_flattened() {
        let mut p = row("a", ProviderFamily::ClaudeAgent);
        p.base_url = "https://x".into();
        let out = ensure_claude_agent_provider(vec![p]);
        assert_eq!(out.len(), 1);
        assert!(out[0].base_url.is_empty());
    }
}
