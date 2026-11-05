//! The pure parts of `ProviderSettings/index.tsx`: how providers and models are created, merged and repaired.

use mewrk_client::model::{ApiProvider, ModelProfile, ProviderFamily};

use super::codex_provider::create_id;
use super::endpoint_meta::default_base_url;
use super::model_caps::{default_reasoning_content, repair_active_model_id};

/// Merges discovery results into the installed list without overwriting what the user curated. Empty values are not
/// curated: empty legacy names, groups and capabilities yield to discovered metadata.
pub fn merge_model_profiles(existing: &[ModelProfile], discovered: &[ModelProfile]) -> Vec<ModelProfile> {
    let mut merged = existing.to_vec();
    for incoming in discovered {
        let id = incoming.id.trim();
        if id.is_empty() {
            continue;
        }
        let normalized = ModelProfile { id: id.to_owned(), ..incoming.clone() };
        match merged.iter().position(|m| m.id.trim() == id) {
            None => merged.push(normalized),
            Some(index) => {
                let current = &merged[index];
                let mut next = normalized;
                // Curated values take precedence over discovery.
                next.id = current.id.clone();
                if !current.name.trim().is_empty() {
                    next.name = current.name.clone();
                }
                if !current.group.trim().is_empty() {
                    next.group = current.group.clone();
                }
                if !current.capabilities.is_empty() {
                    next.capabilities = current.capabilities.clone();
                }
                next.context_window = current.context_window.or(next.context_window);
                next.max_output_tokens = current.max_output_tokens.or(next.max_output_tokens);
                next.cache_ttl_minutes = current.cache_ttl_minutes.or(next.cache_ttl_minutes);
                next.reasoning_content = current.reasoning_content;
                next.prompt_cache = current.prompt_cache;
                merged[index] = next;
            }
        }
    }
    merged
}

/// An empty user-defined provider.
pub fn create_provider(name: &str, family: ProviderFamily) -> ApiProvider {
    ApiProvider { id: create_id("provider"), name: name.to_owned(), enabled: true, family, base_url: default_base_url(family).to_owned(), family_settings: Default::default(), notes: String::new(), models: Vec::new(), active_model_id: None }
}

/// An empty user-defined model; prompt caching starts on, as Claude Code has it.
pub fn create_model(family: ProviderFamily) -> ModelProfile {
    ModelProfile { id: String::new(), name: String::new(), group: String::new(), context_window: None, max_output_tokens: None, capabilities: Default::default(), reasoning_content: default_reasoning_content(family), prompt_cache: true, cache_ttl_minutes: None }
}

/// The reason for a failed credential transaction, with the secret scrubbed: host errors may echo the request.
pub fn key_error_reason(error: &str, secret: Option<&str>) -> String {
    let message = error.trim();
    let readable = if message.is_empty() { "unknown error" } else { message };
    match secret {
        Some(s) if !s.is_empty() => readable.replace(s, "***"),
        _ => readable.to_owned(),
    }
}

pub fn provider_endpoint_fingerprint(provider: &ApiProvider) -> String {
    format!("{}\u{0}{:?}\u{0}{}", provider.id, provider.family, provider.base_url.trim())
}

/// The active provider first, then any enabled provider, then the first in the list.
pub fn preferred_provider_id(providers: &[ApiProvider], active: Option<&str>) -> Option<String> {
    providers
        .iter()
        .find(|p| Some(p.id.as_str()) == active)
        .or_else(|| providers.iter().find(|p| p.enabled))
        .or_else(|| providers.first())
        .map(|p| p.id.clone())
}

/// Searches provider IDs and names as well as each provider's model IDs and names.
pub fn provider_matches(provider: &ApiProvider, needle: &str) -> bool {
    needle.is_empty()
        || provider.id.to_lowercase().contains(needle)
        || provider.name.to_lowercase().contains(needle)
        || provider.models.iter().any(|m| m.id.to_lowercase().contains(needle) || m.name.to_lowercase().contains(needle))
}

/// Applies one edit to a provider the way `replaceProvider` does: renames the active model along, then repairs it.
pub fn apply_provider_update(provider: &mut ApiProvider, update: impl FnOnce(&mut ApiProvider), rename_active: Option<(&str, Option<&str>)>) {
    update(provider);
    if let Some((from, to)) = rename_active
        && provider.active_model_id.as_deref() == Some(from) {
            provider.active_model_id = to.map(str::to_owned);
        }
    provider.active_model_id = repair_active_model_id(provider);
}

/// Moves `id` before or after `target`, keeping everything else in order (`reorderItems`).
pub fn reorder<T>(items: &mut Vec<T>, id: &str, target: &str, after: bool, id_of: impl Fn(&T) -> &str) {
    if id == target {
        return;
    }
    let Some(from) = items.iter().position(|i| id_of(i) == id) else { return };
    let moved = items.remove(from);
    let Some(to) = items.iter().position(|i| id_of(i) == target) else {
        items.insert(from, moved);
        return;
    };
    items.insert(if after { to + 1 } else { to }, moved);
}

/// Who is active after enabling or disabling a provider.
pub fn active_after_enable_change(providers: &[ApiProvider], current: Option<&str>) -> Option<String> {
    if providers.iter().any(|p| Some(p.id.as_str()) == current && p.enabled) {
        return current.map(str::to_owned);
    }
    providers.iter().find(|p| p.enabled).map(|p| p.id.clone())
}

/// Who is active after removing `removed` from `remaining`.
pub fn active_after_removal(remaining: &[ApiProvider], current: Option<&str>, removed: &str) -> Option<String> {
    if current == Some(removed) {
        return remaining.iter().find(|p| p.enabled).map(|p| p.id.clone());
    }
    remaining.iter().find(|p| Some(p.id.as_str()) == current && p.enabled).map(|p| p.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mewrk_client::model::{ModelCapability, ReasoningContent};

    fn model(id: &str) -> ModelProfile {
        ModelProfile { id: id.into(), name: String::new(), group: String::new(), context_window: None, max_output_tokens: None, capabilities: Default::default(), reasoning_content: ReasoningContent::Plaintext, prompt_cache: true, cache_ttl_minutes: None }
    }

    fn provider(id: &str, enabled: bool) -> ApiProvider {
        ApiProvider { id: id.into(), name: id.to_uppercase(), enabled, family: ProviderFamily::OpenaiChat, base_url: String::new(), family_settings: Default::default(), notes: String::new(), models: vec![], active_model_id: None }
    }

    #[test]
    fn curated_values_outrank_discovery_and_empty_ones_do_not() {
        let mut curated = model("gpt-5");
        curated.name = "Mine".into();
        curated.capabilities.insert(ModelCapability::ImageRecognition);
        let mut found = model(" gpt-5 ");
        found.name = "GPT 5".into();
        found.group = "gpt".into();
        found.context_window = Some(100);
        let mut second = model("new");
        second.name = "New".into();
        let out = merge_model_profiles(&[curated], &[found, second, model("  ")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Mine");
        assert_eq!(out[0].group, "gpt");
        assert_eq!(out[0].context_window, Some(100));
        assert!(out[0].capabilities.contains(&ModelCapability::ImageRecognition));
        assert_eq!(out[1].name, "New");
    }

    #[test]
    fn it_opens_on_the_provider_in_use_rather_than_the_first_row() {
        let list = [provider("a", false), provider("b", true), provider("c", true)];
        assert_eq!(preferred_provider_id(&list, Some("c")).as_deref(), Some("c"));
        assert_eq!(preferred_provider_id(&list, None).as_deref(), Some("b"));
        assert_eq!(preferred_provider_id(&[provider("a", false)], None).as_deref(), Some("a"));
        assert_eq!(preferred_provider_id(&[], None), None);
    }

    #[test]
    fn a_secret_never_survives_in_a_reported_error() {
        assert_eq!(key_error_reason("boom sk-123 refused sk-123", Some("sk-123")), "boom *** refused ***");
        assert_eq!(key_error_reason("  ", None), "unknown error");
    }

    #[test]
    fn reordering_moves_one_row_and_keeps_the_rest() {
        let mut v = vec![provider("a", true), provider("b", true), provider("c", true)];
        reorder(&mut v, "a", "c", true, |p| &p.id);
        assert_eq!(v.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["b", "c", "a"]);
        reorder(&mut v, "a", "b", false, |p| &p.id);
        assert_eq!(v.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[test]
    fn removing_the_active_provider_falls_to_the_next_enabled_one() {
        let rest = [provider("a", false), provider("b", true)];
        assert_eq!(active_after_removal(&rest, Some("x"), "x").as_deref(), Some("b"));
        assert_eq!(active_after_removal(&rest, Some("a"), "x"), None);
        assert_eq!(active_after_enable_change(&rest, Some("a")).as_deref(), Some("b"));
    }

    #[test]
    fn removing_the_active_model_repairs_the_selection() {
        let mut p = provider("a", true);
        p.models = vec![model("one"), model("two")];
        p.active_model_id = Some("one".into());
        apply_provider_update(&mut p, |p| p.models.retain(|m| m.id != "one"), None);
        assert_eq!(p.active_model_id.as_deref(), Some("two"));
        apply_provider_update(&mut p, |p| p.models[0].id = "three".into(), Some(("two", Some("three"))));
        assert_eq!(p.active_model_id.as_deref(), Some("three"));
    }
}
