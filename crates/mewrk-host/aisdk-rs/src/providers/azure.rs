//! `@ai-sdk/azure`: `createAzure`'s Responses model. The model itself is `openai_responses`'s; Azure differs in its
//! URL, its `api-key` header, its provider options key and its legacy file ids.

use std::sync::Arc;

use super::ProviderSettings;
use super::openai_responses::ResponsesModel;
use super::openai_responses::url::parse_url;
use crate::api::{VERSION_AZURE, combine_headers, with_user_agent_suffix, without_trailing_slash};
use crate::error::AiError;
use crate::http::Headers;
use crate::model::DynModel;

const OFFICIAL_HOST_SUFFIX: &str = ".openai.azure.com";

/// `config.url` for `/responses`, the part of it this crate reaches: `useDeploymentBasedUrls` is never set.
fn responses_url(base_url: Option<&str>, api_version: &str) -> Result<String, AiError> {
    let Some(base_url) = base_url else {
        // `getResourceName` with no `resourceName` setting, in an environment cleared of `AZURE_RESOURCE_NAME`.
        return Err(AiError::other(
            "Azure OpenAI resource name setting is missing. Pass it using the 'resourceName' parameter or the AZURE_RESOURCE_NAME environment variable.",
        ));
    };
    // `isAzureOpenAIBaseURL`. A custom gateway owns Azure routing and versioning itself.
    let official = parse_url(base_url).ok_or_else(|| AiError::other("Invalid URL"))?.host.ends_with(OFFICIAL_HOST_SUFFIX);
    let prefix = without_trailing_slash(base_url);
    if !official {
        let url = parse_url(&format!("{prefix}/responses")).ok_or_else(|| AiError::other("Invalid URL"))?;
        return Ok(url.href());
    }
    let mut url = parse_url(&format!("{prefix}/v1/responses")).ok_or_else(|| AiError::other("Invalid URL"))?;
    url.set_search_param("api-version", api_version);
    Ok(url.href())
}

/// `createAzure` as `providers.ts` configures it. `provider(modelId)` is the Responses model.
pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    // The base URL's hostname is read when the provider is created; a bad one fails then.
    if let Some(base_url) = target.base_url.as_deref() {
        parse_url(base_url).ok_or_else(|| AiError::other("Invalid URL"))?;
    }
    let api_version = target.settings.get("apiVersion").filter(|version| !version.is_empty()).map_or("v1", String::as_str);
    let url = responses_url(target.base_url.as_deref(), api_version);
    let mut headers = Headers::new();
    headers.set("api-key", &target.api_key);
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/azure/{VERSION_AZURE}")]);
    Ok(Arc::new(ResponsesModel {
        provider: "azure.responses",
        provider_options_name: "azure",
        model_id: target.model_id,
        url,
        headers,
        fetch: target.fetch,
        file_id_prefixes: &["assistant-"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each expectation is what `createAzure`'s `url` produced under Node for the same inputs.
    #[test]
    fn urls_match_upstream() {
        let cases = [
            ("https://res.openai.azure.com/openai", "v1", "https://res.openai.azure.com/openai/v1/responses?api-version=v1"),
            ("https://res.openai.azure.com/openai/", "v1", "https://res.openai.azure.com/openai/v1/responses?api-version=v1"),
            ("https://RES.OpenAI.Azure.com:443/openai", "2025-04-01-preview", "https://res.openai.azure.com/openai/v1/responses?api-version=2025-04-01-preview"),
            ("https://res.openai.azure.com/openai", "a b/c", "https://res.openai.azure.com/openai/v1/responses?api-version=a+b%2Fc"),
            ("https://gw.example.com/azure", "v1", "https://gw.example.com/azure/responses"),
            ("https://gw.example.com/azure/", "2025-04-01", "https://gw.example.com/azure/responses"),
            ("http://GW.example.com:80/a b/../c", "v1", "http://gw.example.com/c/responses"),
            ("https://gw.example.com:8443", "v1", "https://gw.example.com:8443/responses"),
            ("https://res.openai.azure.com/openai?x=1", "v1", "https://res.openai.azure.com/openai?x=1%2Fv1%2Fresponses&api-version=v1"),
            ("https://res.openai.azure.com/openai?x=1&api-version=old&y=a+b", "new", "https://res.openai.azure.com/openai?x=1&api-version=new&y=a+b%2Fv1%2Fresponses"),
            ("https://gw.example.com/q?a=b c", "v1", "https://gw.example.com/q?a=b%20c/responses"),
            ("https://user:pw@gw.example.com/p", "v1", "https://user:pw@gw.example.com/p/responses"),
            ("https://gw.example.com/a/./b/%7Efoo/{x}", "v1", "https://gw.example.com/a/b/%7Efoo/%7Bx%7D/responses"),
            ("https://gw.example.com/p#frag", "v1", "https://gw.example.com/p#frag/responses"),
            ("http://[::1]:9/x", "v1", "http://[::1]:9/x/responses"),
            ("https://gw.example.com//double//slash", "v1", "https://gw.example.com//double//slash/responses"),
            ("  https://gw.example.com/trim  ", "v1", "https://gw.example.com/trim%20%20/responses"),
            ("https:\\\\gw.example.com\\back", "v1", "https://gw.example.com/back/responses"),
        ];
        for (base, version, expected) in cases {
            assert_eq!(responses_url(Some(base), version).unwrap(), expected, "{base}");
        }
        assert_eq!(
            responses_url(None, "v1").unwrap_err().message(),
            "Azure OpenAI resource name setting is missing. Pass it using the 'resourceName' parameter or the AZURE_RESOURCE_NAME environment variable."
        );
        assert_eq!(responses_url(Some("nonsense"), "v1").unwrap_err().message(), "Invalid URL");
    }
}
