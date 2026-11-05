//! `@ai-sdk/google-vertex`: the Node entry's `createGoogleVertex` over the Gemini model, and the Application
//! Default Credentials flow `google-auth-library` runs for it.
//!
//! What the SDK does given how `providers.ts` calls it, with the sidecar's cleared environment:
//!
//! - A non-empty `apiKey` is Express Mode: `x-goog-api-key` on every request, no credentials read anywhere.
//! - An empty `apiKey` is falsy, so the Node entry builds a `GoogleAuth` and resolves a bearer token per call.
//!   `project` and `location` are then required, even with a `baseURL`.
//! - `GoogleAuth` looks at `GOOGLE_APPLICATION_CREDENTIALS` (never set here), then the well-known file under
//!   `HOME` (`APPDATA` on Windows, which is not inherited, so nothing), then the metadata server, then fails.
//!   `CLOUDSDK_CONFIG`, `GCE_METADATA_HOST`, `METADATA_SERVER_DETECTION`, `GOOGLE_CLOUD_QUOTA_PROJECT` and the
//!   proxy variables it also reads are not inherited either, so their branches are not ported.
//! - `fromJSON` takes every credential file type. The `AWS_*` variables an AWS `credential_source` tries first are not
//!   inherited, so its region and keys come from the file's metadata URLs. A `certificate` source needs a client
//!   certificate on the connection, which this `fetch` cannot present, and is refused. The camelCase aliases
//!   `originalOrCamelOptions` reads are for options in code; credential files are snake case.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use graviola::hashing::Sha256;
use graviola::signing::ecdsa::{P256, SigningKey as EcSigningKey};
use graviola::signing::rsa::SigningKey;
use regex::Regex;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

use crate::api::{VERSION_VERTEX, allowed_env, inherited_env, with_user_agent_suffix, without_trailing_slash};
use crate::error::{AiError, ApiCallError, now_ms};
use crate::http::{BoxFuture, Cancel, DynFetch, Fetch, Headers, HttpRequest, HttpResponse};
use crate::jobj;
use crate::json::Json;
use crate::model::{DynModel, SupportedUrls};

use super::ProviderSettings;
use super::bedrock::sigv4::{Credentials, SignInput, amz_datetime, encode_uri_component, sign, url_host};
use super::google::{GoogleLanguageModel, ResolveHeaders, js_string, truthy};

const EXPRESS_MODE_BASE_URL: &str = "https://aiplatform.googleapis.com/v1/publishers/google";

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const JWT_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const METADATA_HOST: &str = "http://169.254.169.254";
const METADATA_SECONDARY_HOST: &str = "http://metadata.google.internal.";
const METADATA_PATH: &str = "/computeMetadata/v1/instance";
const AUTH_USER_AGENT: &str = "google-api-nodejs-client/10.9.1";
const DEFAULT_UNIVERSE: &str = "googleapis.com";
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded;charset=UTF-8";
const TOKEN_EXCHANGE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const GDCH_GRANT_TYPE: &str = "urn:ietf:params:oauth:token-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const GDCH_SUBJECT_TOKEN_TYPE: &str = "urn:k8s:params:oauth:token-type:serviceaccount";
const SAML_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:saml2";
const OIDC_TOKEN_TYPES: [&str; 2] = ["urn:ietf:params:oauth:token-type:id_token", "urn:ietf:params:oauth:token-type:jwt"];
const DEFAULT_EXECUTABLE_TIMEOUT_MILLIS: f64 = 30_000.0;
const MINIMUM_EXECUTABLE_TIMEOUT_MILLIS: f64 = 5_000.0;
const MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS: f64 = 120_000.0;
const MAXIMUM_EXECUTABLE_VERSION: f64 = 1.0;

/// `DEFAULT_EAGER_REFRESH_THRESHOLD_MILLIS`.
const EAGER_REFRESH_MS: u64 = 5 * 60 * 1000;
/// `requestTimeout` off Google Cloud.
const METADATA_TIMEOUT: Duration = Duration::from_secs(3);
const NO_ADC_FOUND: &str = "Could not load the default credentials. Browse to https://cloud.google.com/docs/authentication/getting-started for more information.";
/// What Node's OpenSSL says of a key it cannot decode, the same text whatever is wrong with the PEM.
const BAD_PRIVATE_KEY: &str = "error:1E08010C:DECODER routines::unsupported";

pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    create_with_adc(target, Adc::from_env())
}

/// [`create`] with the credential lookup given: tests must not read the real `HOME`.
pub fn create_with_adc(target: ProviderSettings, adc: Adc) -> Result<DynModel, AiError> {
    let express = !target.api_key.is_empty();
    // Tuned models are addressed by their deployed endpoint, which replaces `publishers/google/models/{id}`.
    let endpoint = target.model_id.starts_with("endpoints/");
    if endpoint && express {
        return Err(AiError::other("Google Vertex tuned models do not support Express Mode API keys. Use standard Google Cloud credentials instead."));
    }
    let base_url = load_base_url(&target, express, endpoint)?;
    let suffix = format!("ai-sdk/google-vertex/{VERSION_VERTEX}");
    let (headers, fetch): (Box<dyn ResolveHeaders>, DynFetch) = if express {
        let headers = with_user_agent_suffix(target.headers, &[&suffix]);
        let fetch = Arc::new(ExpressModeFetch { api_key: target.api_key, inner: target.fetch });
        (Box::new(headers), fetch)
    } else {
        let auth = GoogleAuth::new(adc, target.fetch.clone(), api_audience(&base_url));
        (Box::new(BearerHeaders { auth, headers: target.headers, suffix }), target.fetch)
    };
    let supported_urls: SupportedUrls = vec![("*", r"^https?://.*$"), ("*", r"^gs://.*$")];
    Ok(Arc::new(GoogleLanguageModel { provider: "google.vertex.chat", model_id: target.model_id, base_url, headers, fetch, supported_urls }))
}

/// `loadBaseURL`. Standard mode loads `location` and `project` even when a `baseURL` makes them unused.
fn load_base_url(target: &ProviderSettings, express: bool, endpoint: bool) -> Result<String, AiError> {
    let configured = target.base_url.as_deref().map(without_trailing_slash);
    if express {
        return Ok(configured.unwrap_or(EXPRESS_MODE_BASE_URL).to_owned());
    }
    let setting = |key: &str, label: &str, variable: &str| {
        target.settings.get(key).filter(|v| !v.is_empty()).cloned().ok_or_else(|| {
            AiError::other(format!("Google Vertex {label} setting is missing. Pass it using the '{key}' parameter or the {variable} environment variable."))
        })
    };
    let region = setting("location", "location", "GOOGLE_VERTEX_LOCATION")?;
    let project = setting("project", "project", "GOOGLE_VERTEX_PROJECT")?;
    if let Some(configured) = configured {
        return Ok(configured.to_owned());
    }
    let host = match region.as_str() {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "eu" | "us" => format!("aiplatform.{region}.rep.googleapis.com"),
        _ => format!("{region}-aiplatform.googleapis.com"),
    };
    let publisher = if endpoint { "" } else { "/publishers/google" };
    Ok(format!("https://{host}/v1beta1/projects/{project}/locations/{region}{publisher}"))
}

/// The audience of a GDCH token: `getClient` takes it from `clientOptions.apiEndpoint`, which `createGoogleVertex` never
/// sets, so upstream stops at "Audience cannot be null or empty" there. The endpoint is the origin the model is called on.
fn api_audience(base_url: &str) -> String {
    let start = base_url.find("://").map_or(0, |at| at + 3);
    let end = base_url[start..].find(['/', '?', '#']).map_or(base_url.len(), |at| start + at);
    base_url[..end].to_owned()
}

/// `createExpressModeFetch`: the key goes on last, over any header of that name.
struct ExpressModeFetch {
    api_key: String,
    inner: DynFetch,
}

impl Fetch for ExpressModeFetch {
    fn fetch(&self, mut request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        request.headers.set("x-goog-api-key", &self.api_key);
        self.inner.fetch(request)
    }
}

/// The Node entry's `headers`: the caller's own win over the token, which is fetched regardless.
struct BearerHeaders {
    auth: GoogleAuth,
    headers: Headers,
    suffix: String,
}

impl ResolveHeaders for BearerHeaders {
    fn resolve<'a>(&'a self, cancel: &'a Cancel) -> BoxFuture<'a, Result<Headers, AiError>> {
        Box::pin(async move {
            let token = self.auth.access_token(cancel).await?;
            let mut headers = Headers::new();
            headers.set("authorization", &format!("Bearer {token}"));
            for (name, value) in &self.headers.0 {
                headers.set(name, value);
            }
            Ok(with_user_agent_suffix(headers, &[&self.suffix]))
        })
    }
}

// ------------------------------------------------------------------ Where credentials come from

/// `gcpMetadata.isAvailable`'s cache lives as long as the process, and so does this one: the probe can take
/// three seconds off Google Cloud, and a step per frame must not pay that each time.
static AVAILABILITY: LazyLock<Arc<OnceCell<bool>>> = LazyLock::new(|| Arc::new(OnceCell::new()));

/// What the credential lookup reads besides the network.
#[derive(Clone)]
pub struct Adc {
    home: Option<String>,
    availability: Arc<OnceCell<bool>>,
    /// `GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES` is `1`.
    allow_executables: bool,
}

impl Adc {
    /// The sidecar's environment.
    pub fn from_env() -> Self {
        // On Windows upstream reads `APPDATA`, which the sidecar did not inherit.
        let home = if cfg!(windows) { None } else { allowed_env("HOME") };
        let allow_executables = allowed_env("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").as_deref() == Some("1");
        Self { home, availability: AVAILABILITY.clone(), allow_executables }
    }

    /// A lookup under `home`, with its own memory of the metadata probe.
    pub fn at_home(home: Option<String>) -> Self {
        Self { home, availability: Arc::new(OnceCell::new()), allow_executables: false }
    }

    /// This lookup with executables allowed, as the variable would have it.
    pub fn allowing_executables(self) -> Self {
        Self { allow_executables: true, ..self }
    }

    fn well_known_file(&self) -> Option<PathBuf> {
        let home = self.home.as_deref().filter(|h| !h.is_empty())?;
        let location = Path::new(home).join(".config").join("gcloud").join("application_default_credentials.json");
        location.exists().then_some(location)
    }
}

/// `isGoogleComputeEngineLinux`, the half of `detectGCPResidency` that works without listing network interfaces.
/// The serverless half reads variables the sidecar did not inherit. A Windows VM on GCE, which only the MAC
/// address would tell, gets the same answer from the probe.
fn gcp_residency() -> bool {
    static RESIDENCY: OnceLock<bool> = OnceLock::new();
    *RESIDENCY.get_or_init(|| {
        cfg!(target_os = "linux")
            && Path::new("/sys/class/dmi/id/bios_date").exists()
            && std::fs::read_to_string("/sys/class/dmi/id/bios_vendor").is_ok_and(|vendor| vendor.contains("Google"))
    })
}

/// The `realpath`, `lstat` and read that `_getApplicationCredentialsFromFilePath` and `FileSubjectTokenSupplier` share.
fn read_real_file(location: &Path) -> Result<String, AiError> {
    let real = std::fs::canonicalize(location).map_err(|e| AiError::other(format!("The file at {} does not exist, or it is not a file. {e}", location.display())))?;
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
        return Err(AiError::other(format!("The file at {} does not exist, or it is not a file. ", real.display())));
    }
    let bytes = std::fs::read(&real).map_err(|e| {
        let what = if e.kind() == std::io::ErrorKind::PermissionDenied { "EACCES: permission denied".to_owned() } else { e.to_string() };
        AiError::other(format!("{what}, open '{}'", real.display()))
    })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `_getApplicationCredentialsFromFilePath` and `fromStreamAsync`.
fn read_credential_file(location: &Path) -> Result<Credential, AiError> {
    let json = Json::parse(&read_real_file(location)?).map_err(AiError::other)?;
    from_json(&json)
}

fn required<'a>(json: &'a Json, key: &str) -> Result<&'a str, AiError> {
    json.str_at(key).filter(|v| !v.is_empty()).ok_or_else(|| AiError::other(format!("The incoming JSON object does not contain a {key} field")))
}

/// `fromJSON`. An unknown `type` is a service account, as it is upstream.
fn from_json(json: &Json) -> Result<Credential, AiError> {
    if json.is_null() {
        return Err(AiError::other("Cannot read properties of null (reading 'type')"));
    }
    match json.str_at("type") {
        Some("authorized_user") => Ok(Credential::User {
            client_id: required(json, "client_id")?.to_owned(),
            client_secret: required(json, "client_secret")?.to_owned(),
            refresh_token: required(json, "refresh_token")?.to_owned(),
        }),
        Some("impersonated_service_account") => impersonated_from_json(json),
        Some("external_account") => Ok(Credential::External(external_from_json(json)?)),
        Some("external_account_authorized_user") => Ok(Credential::ExternalUser {
            token_url: json.str_at("token_url").map_or_else(|| format!("https://sts.{}/v1/oauthtoken", universe_domain(json)), str::to_owned),
            client_auth: basic_auth(required(json, "client_id")?, json.str_at("client_secret").unwrap_or("")),
            refresh_token: required(json, "refresh_token")?.to_owned(),
        }),
        Some("gdch_service_account") => gdch_from_json(json),
        _ => Ok(Credential::ServiceAccount { email: required(json, "client_email")?.to_owned(), key: required(json, "private_key")?.to_owned() }),
    }
}

/// `universeDomain` of the client `fromJSON` builds: impersonation takes its source's unless the file names one.
fn universe_domain(json: &Json) -> &str {
    if let Some(explicit) = json.str_at("universe_domain").filter(|domain| !domain.is_empty()) {
        return explicit;
    }
    match (json.str_at("type"), json.get("source_credentials")) {
        (Some("impersonated_service_account"), Some(source)) => universe_domain(source),
        _ => DEFAULT_UNIVERSE,
    }
}

/// `injectAuthenticatedHeaders`.
fn basic_auth(client_id: &str, client_secret: &str) -> String {
    format!("Basic {}", base64_simd::STANDARD.encode_to_string(format!("{client_id}:{client_secret}").as_bytes()))
}

/// `fromImpersonatedJSON` and the `Impersonated` constructor, which gets the whole file spread into its options. The
/// scope is `GoogleAuth`'s, which wins over the file's `scopes`. The URL only names the target: the request goes to the
/// IAM Credentials endpoint of the universe, as `refreshToken` builds it.
fn impersonated_from_json(json: &Json) -> Result<Credential, AiError> {
    let source_json = json.get("source_credentials").filter(|source| truthy(source)).ok_or_else(|| AiError::other("The incoming JSON object does not contain a source_credentials field"))?;
    let url = required(json, "service_account_impersonation_url")?;
    let source = from_json(source_json)?;
    if url.len() > 256 {
        return Err(AiError::other(format!("Target principal is too long: {url}")));
    }
    let target = url
        .strip_suffix(":generateAccessToken")
        .or_else(|| url.strip_suffix(":generateIdToken"))
        .and_then(|head| head.rsplit('/').next())
        .filter(|target| !target.is_empty())
        .ok_or_else(|| AiError::other(format!("Cannot extract target principal from {url}")))?;
    let universe = universe_domain(json);
    let source_universe = universe_domain(source_json);
    if universe != source_universe {
        return Err(AiError::other(format!("Universe domain {source_universe} in source credentials does not match {universe} universe domain set for impersonated credentials.")));
    }
    let endpoint = json.non_null("endpoint").map_or_else(|| format!("https://iamcredentials.{universe}"), js_string);
    Ok(Credential::Impersonated {
        source: Box::new(Client::new(source)),
        url: format!("{endpoint}/v1/projects/-/serviceAccounts/{target}:generateAccessToken"),
        delegates: json.non_null("delegates").cloned().unwrap_or_else(|| Json::Arr(Vec::new())),
        lifetime: json.non_null("lifetime").map_or_else(|| "3600".to_owned(), js_string),
    })
}

/// The `BaseExternalAccountClient` constructor, then the subclass `ExternalAccountClient.fromJSON` picks.
fn external_from_json(json: &Json) -> Result<ExternalAccount, AiError> {
    static WORKFORCE_AUDIENCE: OnceLock<Regex> = OnceLock::new();
    let non_empty = |key: &str| json.str_at(key).filter(|value| !value.is_empty()).map(str::to_owned);
    let workforce_pool_user_project = non_empty("workforce_pool_user_project");
    let audience = json.str_at("audience").ok_or_else(|| AiError::other("Cannot read properties of undefined (reading 'match')"))?;
    let workforce = WORKFORCE_AUDIENCE.get_or_init(|| Regex::new(r"//iam\.googleapis\.com/locations/[^/]+/workforcePools/[^/]+/providers/.+").expect("a literal pattern"));
    if workforce_pool_user_project.is_some() && !workforce.is_match(audience) {
        return Err(AiError::other("workforcePoolUserProject should not be set for non-workforce pool credentials."));
    }
    Ok(ExternalAccount {
        source: subject_source(json.get("credential_source").filter(|source| truthy(source)))?,
        audience: audience.to_owned(),
        subject_token_type: json.str_at("subject_token_type").map(str::to_owned),
        token_url: json.str_at("token_url").map_or_else(|| format!("https://sts.{}/v1/token", universe_domain(json)), str::to_owned),
        client_auth: non_empty("client_id").map(|client_id| basic_auth(&client_id, json.str_at("client_secret").unwrap_or(""))),
        workforce_pool_user_project,
        impersonation_url: json.str_at("service_account_impersonation_url").map(str::to_owned),
        lifetime: json.path(&["service_account_impersonation", "token_lifetime_seconds"]).filter(|lifetime| truthy(lifetime)).map(js_string),
    })
}

fn subject_source(source: Option<&Json>) -> Result<SubjectSource, AiError> {
    let source = source.ok_or_else(|| AiError::other("A credential source or subject token supplier must be specified."))?;
    if source.get("environment_id").is_some_and(truthy) {
        return aws_source(source);
    }
    if let Some(executable) = source.get("executable").filter(|executable| truthy(executable)) {
        return executable_source(executable);
    }
    identity_pool_source(source)
}

/// The `AwsClient` constructor and `validateEnvironmentId`.
fn aws_source(source: &Json) -> Result<SubjectSource, AiError> {
    let invalid = || AiError::other("No valid AWS \"credential_source\" provided");
    let version = source.str_at("environment_id").and_then(|id| id.strip_prefix("aws")).filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).ok_or_else(invalid)?;
    let verification_url = source.str_at("regional_cred_verification_url").filter(|url| !url.is_empty()).ok_or_else(invalid)?;
    if version.parse::<f64>() != Ok(1.0) {
        return Err(AiError::other(format!("aws version \"{version}\" is not supported in the current build.")));
    }
    let url = |key: &str| source.str_at(key).filter(|url| !url.is_empty()).map(str::to_owned);
    Ok(SubjectSource::Aws { region_url: url("region_url"), credentials_url: url("url"), session_token_url: url("imdsv2_session_token_url"), verification_url: verification_url.to_owned() })
}

/// The `PluggableAuthClient` and `PluggableAuthHandler` constructors.
fn executable_source(executable: &Json) -> Result<SubjectSource, AiError> {
    let command = executable.str_at("command").filter(|command| !command.is_empty()).ok_or_else(|| AiError::other("No valid Pluggable Auth \"credential_source\" provided."))?;
    let timeout = executable.get("timeout_millis").map_or(DEFAULT_EXECUTABLE_TIMEOUT_MILLIS, |millis| millis.as_f64().unwrap_or(0.0));
    if !(MINIMUM_EXECUTABLE_TIMEOUT_MILLIS..=MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS).contains(&timeout) {
        return Err(AiError::other(format!("Timeout must be between {MINIMUM_EXECUTABLE_TIMEOUT_MILLIS} and {MAXIMUM_EXECUTABLE_TIMEOUT_MILLIS} milliseconds.")));
    }
    Ok(SubjectSource::Executable {
        command: parse_command(command)?,
        timeout: Duration::from_millis(timeout as u64),
        output_file: executable.str_at("output_file").filter(|file| !file.is_empty()).map(str::to_owned),
    })
}

/// `parseCommand`: split on spaces, except between quotation marks.
fn parse_command(command: &str) -> Result<Vec<String>, AiError> {
    static COMPONENT: OnceLock<Regex> = OnceLock::new();
    let component = COMPONENT.get_or_init(|| Regex::new(r#"(?:[^\s"]+|"[^"]*")+"#).expect("a literal pattern"));
    let components: Vec<String> = component
        .find_iter(command)
        .map(|found| {
            let text = found.as_str();
            let quoted = text.len() >= 2 && text.starts_with('"') && text.ends_with('"');
            if quoted { text[1..text.len() - 1].to_owned() } else { text.to_owned() }
        })
        .collect();
    if components.is_empty() {
        return Err(AiError::other(format!("Provided command: \"{command}\" could not be parsed.")));
    }
    Ok(components)
}

/// The `IdentityPoolClient` constructor.
fn identity_pool_source(source: &Json) -> Result<SubjectSource, AiError> {
    let format = source.get("format");
    let format_type = format.and_then(|f| f.str_at("type")).filter(|kind| !kind.is_empty()).unwrap_or("text");
    let field = format.and_then(|f| f.str_at("subject_token_field_name")).filter(|field| !field.is_empty());
    let format = match (format_type, field) {
        ("text", _) => TokenFormat::Text,
        ("json", Some(field)) => TokenFormat::Json(field.to_owned()),
        ("json", None) => return Err(AiError::other("Missing subject_token_field_name for JSON credential_source format")),
        (other, _) => return Err(AiError::other(format!("Invalid credential_source format \"{other}\""))),
    };
    let given = |key: &str| source.get(key).filter(|value| truthy(value));
    let (file, url, certificate) = (given("file"), given("url"), given("certificate"));
    match (file, url, certificate) {
        (Some(file), None, None) => Ok(SubjectSource::File { path: js_string(file), format }),
        (None, Some(url), None) => {
            let mut headers = Headers::new();
            for (name, value) in source.get("headers").and_then(Json::as_obj).into_iter().flat_map(|given| given.iter()) {
                headers.set(name, &js_string(value));
            }
            Ok(SubjectSource::Url { url: js_string(url), headers, format })
        }
        (None, None, Some(_)) => Err(AiError::other("Google credentials with a certificate credential_source are not supported by this build")),
        _ => Err(AiError::other("No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.")),
    }
}

/// `GdchClient.fromJSON`.
fn gdch_from_json(json: &Json) -> Result<Credential, AiError> {
    if json.str_at("format_version") != Some("1") {
        return Err(AiError::other("Only format version 1 is supported."));
    }
    Ok(Credential::Gdch(Gdch {
        project: required(json, "project")?.to_owned(),
        private_key_id: required(json, "private_key_id")?.to_owned(),
        private_key: required(json, "private_key")?.to_owned(),
        name: required(json, "name")?.to_owned(),
        token_uri: required(json, "token_uri")?.to_owned(),
        ca_cert_path: json.str_at("ca_cert_path").filter(|path| !path.is_empty()).map(str::to_owned),
    }))
}

// ------------------------------------------------------------------ Tokens

enum Credential {
    /// `JWT`.
    ServiceAccount { email: String, key: String },
    /// `UserRefreshClient`.
    User { client_id: String, client_secret: String, refresh_token: String },
    /// `Compute`.
    Compute,
    /// `Impersonated`. The source keeps a token of its own.
    Impersonated { source: Box<Client>, url: String, delegates: Json, lifetime: String },
    /// `BaseExternalAccountClient`.
    External(ExternalAccount),
    /// `ExternalAccountAuthorizedUserClient`. A refresh may hand back the next refresh token.
    ExternalUser { token_url: String, client_auth: String, refresh_token: String },
    /// `GdchClient`.
    Gdch(Gdch),
}

struct ExternalAccount {
    source: SubjectSource,
    audience: String,
    subject_token_type: Option<String>,
    token_url: String,
    client_auth: Option<String>,
    workforce_pool_user_project: Option<String>,
    impersonation_url: Option<String>,
    /// `service_account_impersonation.token_lifetime_seconds`, when the file asks for one.
    lifetime: Option<String>,
}

/// What `retrieveSubjectToken` reads, by which subclass of `BaseExternalAccountClient` it is.
enum SubjectSource {
    File { path: String, format: TokenFormat },
    Url { url: String, headers: Headers, format: TokenFormat },
    Aws { region_url: Option<String>, credentials_url: Option<String>, session_token_url: Option<String>, verification_url: String },
    Executable { command: Vec<String>, timeout: Duration, output_file: Option<String> },
}

enum TokenFormat {
    Text,
    /// The field holding the token.
    Json(String),
}

struct Gdch {
    project: String,
    private_key_id: String,
    private_key: String,
    name: String,
    token_uri: String,
    ca_cert_path: Option<String>,
}

struct Client {
    credential: Credential,
    access_token: Option<String>,
    expiry_ms: Option<u64>,
}

impl Client {
    fn new(credential: Credential) -> Self {
        Self { credential, access_token: None, expiry_ms: None }
    }

    /// `isTokenExpiring`: a token with no expiry never expires.
    fn usable_token(&self) -> Option<&str> {
        let expiring = self.expiry_ms.is_some_and(|expiry| expiry <= now_ms() + EAGER_REFRESH_MS);
        self.access_token.as_deref().filter(|_| !expiring)
    }

    /// `getAccessToken`. Boxed, because an impersonated client asks its source the same thing.
    fn token<'a>(&'a mut self, auth: &'a GoogleAuth, cancel: &'a Cancel) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move {
            if let Some(token) = self.usable_token() {
                return Ok(token.to_owned());
            }
            let fetch = &auth.fetch;
            let refreshed = match &mut self.credential {
                Credential::ServiceAccount { email, key } => refresh_service_account(fetch, email, key, cancel).await?,
                Credential::User { client_id, client_secret, refresh_token } => refresh_user(fetch, client_id, client_secret, refresh_token, cancel).await?,
                Credential::Compute => refresh_compute(fetch, cancel).await?,
                Credential::Impersonated { source, url, delegates, lifetime } => refresh_impersonated(auth, source, url, delegates, lifetime, cancel).await?,
                Credential::External(account) => refresh_external(auth, account, cancel).await?,
                Credential::ExternalUser { token_url, client_auth, refresh_token } => refresh_external_user(fetch, token_url, client_auth, refresh_token, cancel).await?,
                Credential::Gdch(gdch) => refresh_gdch(auth, gdch, cancel).await?,
            };
            self.access_token = refreshed.access_token.filter(|token| !token.is_empty());
            self.expiry_ms = refreshed.expiry_ms;
            self.access_token.clone().ok_or_else(|| AiError::other("Could not refresh access token."))
        })
    }
}

/// `GoogleAuth` and the client it settles on. One per model, which is one per step frame, as it was per
/// `createGoogleVertex` call upstream.
struct GoogleAuth {
    adc: Adc,
    fetch: DynFetch,
    /// See [`api_audience`].
    api_audience: String,
    /// Held across a refresh, which is how concurrent callers share one, as `refreshTokenPromises` does.
    client: Mutex<Option<Client>>,
}

impl GoogleAuth {
    fn new(adc: Adc, fetch: DynFetch, api_audience: String) -> Self {
        Self { adc, fetch, api_audience, client: Mutex::new(None) }
    }

    /// `generateAuthToken`.
    async fn access_token(&self, cancel: &Cancel) -> Result<String, AiError> {
        let mut slot = self.client.lock().await;
        if slot.is_none() {
            *slot = Some(Client::new(self.determine_credential(cancel).await?));
        }
        slot.as_mut().expect("just set").token(self, cancel).await
    }

    /// `getApplicationDefaultAsync`.
    async fn determine_credential(&self, cancel: &Cancel) -> Result<Credential, AiError> {
        if let Some(location) = self.adc.well_known_file() {
            return read_credential_file(&location);
        }
        if gcp_residency() {
            return Ok(Credential::Compute);
        }
        // Dropping the probe on cancel leaves the cell unset for whoever asks next.
        let available = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Abort),
            available = self.adc.availability.get_or_init(|| metadata_available(&self.fetch)) => *available,
        };
        if available { Ok(Credential::Compute) } else { Err(AiError::other(NO_ADC_FOUND)) }
    }
}

struct Refreshed {
    access_token: Option<String>,
    expiry_ms: Option<u64>,
}

/// `expires_in * 1000`, which JavaScript also does to a numeric string.
fn expires_in_ms(data: &Json) -> Option<f64> {
    let seconds = match data.non_null("expires_in")? {
        Json::Str(text) => text.trim().parse::<f64>().ok()?,
        other => other.as_f64()?,
    };
    (seconds != 0.0).then_some(seconds * 1000.0)
}

fn after_now(ms: f64) -> u64 {
    (now_ms() as f64 + ms).max(0.0) as u64
}

/// The `application/x-www-form-urlencoded` serializer `URLSearchParams` has.
fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (at, (name, value)) in pairs.iter().enumerate() {
        if at > 0 {
            out.push('&');
        }
        for (position, text) in [name, value].iter().enumerate() {
            if position == 1 {
                out.push('=');
            }
            for byte in text.bytes() {
                match byte {
                    b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(byte as char),
                    b' ' => out.push('+'),
                    _ => out.push_str(&format!("%{byte:02X}")),
                }
            }
        }
    }
    out
}

/// The first PEM block in `text`: its label, its contents, and what comes after it.
fn pem_block(text: &str) -> Option<(&str, Vec<u8>, &str)> {
    let (label, rest) = text.split_once("-----BEGIN ")?.1.split_once("-----")?;
    let (body, rest) = rest.split_once("-----END ")?;
    let compact: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let der = base64_simd::STANDARD.decode_to_vec(compact.as_bytes()).ok()?;
    Some((label, der, rest.split_once("-----").map_or("", |(_, after)| after)))
}

/// A key as Node's `crypto` takes one: `PRIVATE KEY` (PKCS#8) or `RSA PRIVATE KEY` (PKCS#1) PEM.
fn signing_key(pem: &str) -> Option<SigningKey> {
    let (label, der, _) = pem_block(pem)?;
    match label {
        "PRIVATE KEY" => SigningKey::from_pkcs8_der(&der).ok(),
        "RSA PRIVATE KEY" => SigningKey::from_pkcs1_der(&der).ok(),
        _ => None,
    }
}

/// `getJwsSign`: no `kid` and no `typ`, because `GoogleToken` is given neither.
fn signed_assertion(email: &str, pem: &str, now_seconds: u64) -> Result<String, AiError> {
    let key = signing_key(pem).ok_or_else(|| AiError::other(BAD_PRIVATE_KEY))?;
    let payload = jobj! {
        "iss": email,
        "scope": SCOPE,
        "aud": TOKEN_URL,
        "exp": now_seconds + 3600,
        "iat": now_seconds,
    };
    let url = |text: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(text.as_bytes());
    let message = format!("{}.{}", url(r#"{"alg":"RS256"}"#), url(&payload.to_text()));
    let mut signature = vec![0u8; key.modulus_len_bytes()];
    let signature = key.sign_pkcs1_sha256(&mut signature, message.as_bytes()).map_err(|e| AiError::other(format!("{e:?}")))?;
    Ok(format!("{message}.{}", base64_simd::URL_SAFE_NO_PAD.encode_to_string(signature)))
}

struct Reply {
    status: u16,
    headers: Headers,
    text: String,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `getResponseData` for `responseType: 'json'`: what does not parse stays text.
    fn json_data(&self) -> Json {
        if self.status == 204 {
            return Json::from("");
        }
        Json::parse(&self.text).unwrap_or_else(|_| Json::from(self.text.as_str()))
    }

    /// `getResponseData` for no `responseType`: by content type, with anything else an opaque blob.
    fn typed_data(&self) -> Json {
        if self.status == 204 {
            return Json::from("");
        }
        let Some(content_type) = self.headers.get("content-type").map(str::to_ascii_lowercase) else { return Json::from(self.text.as_str()) };
        if content_type.contains("application/json") {
            self.json_data()
        } else if content_type.starts_with("text/") {
            Json::from(self.text.as_str())
        } else {
            Json::obj()
        }
    }
}

async fn send(fetch: &DynFetch, request: HttpRequest) -> Result<Reply, AiError> {
    let mut response = fetch.fetch(request).await?;
    let text = response.body.read_text().await?;
    Ok(Reply { status: response.status, headers: response.headers, text })
}

/// A request through `AuthClient`'s transporter, with what its interceptor adds.
fn auth_request(url: &str, content_type: &str, body: String, accept_json: bool, cancel: &Cancel) -> HttpRequest {
    let mut headers = Headers::new();
    if accept_json {
        headers.set("accept", "application/json");
    }
    headers.set("content-type", content_type);
    headers.set("user-agent", AUTH_USER_AGENT);
    // `gl-node/<version>` upstream, the runtime segment again.
    headers.set("x-goog-api-client", "gl-rust");
    HttpRequest::post(url, headers, body, cancel.clone())
}

fn token_request(body: String, accept_json: bool, cancel: &Cancel) -> HttpRequest {
    auth_request(TOKEN_URL, FORM_CONTENT_TYPE, body, accept_json, cancel)
}

/// `extractAPIErrorFromResponse`'s message.
fn gaxios_message(status: u16, data: &Json) -> String {
    let mut message = match data {
        Json::Str(text) => text.clone(),
        _ => format!("Request failed with status code {status}"),
    };
    let Some(error) = data.get("error").filter(|e| truthy(e)) else { return message };
    if let Some(text) = error.as_str() {
        return text.to_owned();
    }
    if !error.is_obj() {
        return message;
    }
    if let Some(text) = error.str_at("message") {
        return text.to_owned();
    }
    if let Some(errors) = error.get("errors").and_then(Json::as_arr) {
        let joined = errors.iter().filter_map(|e| e.str_at("message")).collect::<Vec<_>>().join("\n");
        if !joined.is_empty() {
            message = joined;
        }
    }
    message
}

/// `wrapError` and its likes, which change an error's message and nothing else about it.
fn reworded(error: AiError, word: impl Fn(&str) -> String) -> AiError {
    match error {
        AiError::Abort => AiError::Abort,
        AiError::Api(mut api) => {
            api.message = word(&api.message);
            AiError::Api(api)
        }
        AiError::Network { code, message } => AiError::Network { code, message: word(&message) },
        other => AiError::other(word(&other.message())),
    }
}

/// A failed token request. Upstream retried 408, 429 and 5xx three times inside gaxios and then failed for good;
/// here the host owns retrying, so those are transient and the rest permanent.
fn token_failure(url: &str, reply: &Reply, message: String) -> AiError {
    let retryable = matches!(reply.status, 100..=199 | 408 | 429 | 500..=599);
    if !retryable {
        return AiError::other(message);
    }
    AiError::api(ApiCallError {
        message,
        url: url.to_owned(),
        status: Some(reply.status),
        response_headers: reply.headers.to_vec(),
        response_body: Some(reply.text.clone()),
        is_retryable: true,
        data: None,
    })
}

/// `JWT.refreshTokenNoCache` through `GoogleToken`. A reply without `expires_in` is expired at once.
async fn refresh_service_account(fetch: &DynFetch, email: &str, key: &str, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let assertion = signed_assertion(email, key, now_ms() / 1000)?;
    let body = form_encode(&[("grant_type", JWT_GRANT_TYPE), ("assertion", &assertion)]);
    let reply = send(fetch, token_request(body, true, cancel)).await?;
    let data = reply.json_data();
    if !reply.ok() {
        let message = match data.get("error").filter(|e| truthy(e)) {
            Some(error) => {
                let description = data.non_null("error_description").map_or_else(|| "undefined".to_owned(), js_string);
                format!("{}: {description}", js_string(error))
            }
            None => gaxios_message(reply.status, &data),
        };
        return Err(token_failure(TOKEN_URL, &reply, message));
    }
    Ok(Refreshed { access_token: data.str_at("access_token").map(str::to_owned), expiry_ms: Some(after_now(expires_in_ms(&data).unwrap_or(0.0))) })
}

/// `OAuth2Client.refreshTokenNoCache`. A reply without `expires_in` has no expiry at all.
async fn refresh_user(fetch: &DynFetch, client_id: &str, client_secret: &str, refresh_token: &str, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let body = form_encode(&[("refresh_token", refresh_token), ("client_id", client_id), ("client_secret", client_secret), ("grant_type", "refresh_token")]);
    let reply = send(fetch, token_request(body, false, cancel)).await?;
    let data = reply.typed_data();
    if !reply.ok() {
        let mut message = gaxios_message(reply.status, &data);
        let reauth = data.str_at("error_description").is_some_and(|d| d.to_ascii_lowercase().contains("reauth"));
        if message == "invalid_grant" && reauth {
            message = data.to_text();
        }
        return Err(token_failure(TOKEN_URL, &reply, message));
    }
    Ok(Refreshed { access_token: data.str_at("access_token").map(str::to_owned), expiry_ms: expires_in_ms(&data).map(after_now) })
}

fn metadata_request(url: String, cancel: Cancel) -> HttpRequest {
    let mut headers = Headers::new();
    headers.set("metadata-flavor", "Google");
    HttpRequest { method: "GET", url, headers, body: None, ca_roots: None, cancel }
}

/// `isAvailable` over `fastFailMetadataRequest`: both hosts race, because the IP is tarpitted on a local machine
/// and DNS is slow in some Google Cloud environments. The first to answer 2xx decides, on its `Metadata-Flavor`.
async fn metadata_available(fetch: &DynFetch) -> bool {
    let probe = |host: &'static str| async move {
        let request = metadata_request(format!("{host}{METADATA_PATH}"), Cancel::new());
        let response = tokio::time::timeout(METADATA_TIMEOUT, fetch.fetch(request)).await.ok()?.ok()?;
        response.ok().then(|| response.headers.get("metadata-flavor") == Some("Google"))
    };
    let (first, second) = (probe(METADATA_HOST), probe(METADATA_SECONDARY_HOST));
    tokio::pin!(first, second);
    let (mut first_done, mut second_done) = (false, false);
    loop {
        tokio::select! {
            answer = &mut first, if !first_done => {
                first_done = true;
                if let Some(flavored) = answer {
                    return flavored;
                }
            }
            answer = &mut second, if !second_done => {
                second_done = true;
                if let Some(flavored) = answer {
                    return flavored;
                }
            }
            else => return false,
        }
    }
}

/// `Compute.refreshTokenNoCache` and `wrapError`.
async fn refresh_compute(fetch: &DynFetch, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let url = format!("{METADATA_HOST}{METADATA_PATH}/service-accounts/default/token?{}", form_encode(&[("scopes", SCOPE)]));
    let request = metadata_request(url.clone(), cancel.clone());
    let prefix = "Could not refresh access token: ";
    let reply = if gcp_residency() {
        send(fetch, request).await
    } else {
        tokio::time::timeout(METADATA_TIMEOUT, send(fetch, request))
            .await
            .unwrap_or_else(|_| Err(AiError::other("The operation was aborted.")))
    };
    let reply = reply.map_err(|error| reworded(error, |message| format!("{prefix}{message}")))?;
    if !reply.ok() {
        let message = format!("{prefix}{}", gaxios_message(reply.status, &Json::from(reply.text.as_str())));
        let message = match reply.status {
            403 => format!("A Forbidden error was returned while attempting to retrieve an access token for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have the correct permission scopes specified: {message}"),
            // The missing space is upstream's.
            404 => format!("A Not Found error was returned while attempting to retrieve an accesstoken for the Compute Engine built-in service account. This may be because the Compute Engine instance does not have any permission scopes specified: {message}"),
            _ => message,
        };
        return Err(token_failure(&url, &reply, message));
    }
    match reply.headers.get("metadata-flavor") {
        Some("Google") => {}
        other => {
            let got = other.map_or_else(|| "no header".to_owned(), |value| format!("'{value}'"));
            return Err(AiError::other(format!("Invalid response from metadata service: incorrect Metadata-Flavor header. Expected 'Google', got {got}")));
        }
    }
    let data = reply.json_data();
    Ok(Refreshed { access_token: data.str_at("access_token").map(str::to_owned), expiry_ms: expires_in_ms(&data).map(after_now) })
}

// ------------------------------------------------------------------ Impersonation

/// `Date.parse` of what IAM Credentials writes, RFC 3339 down to nanoseconds. `None` is its `NaN`, which both clients
/// that read it take for no expiry.
fn iso_date_ms(text: &str) -> Option<u64> {
    let (date, rest) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, offset_minutes) = match rest.strip_suffix('Z') {
        Some(clock) => (clock, 0),
        None => {
            let at = rest.rfind(['+', '-'])?;
            let (hours, minutes) = rest[at + 1..].split_once(':')?;
            let magnitude = hours.parse::<i64>().ok()? * 60 + minutes.parse::<i64>().ok()?;
            (&rest[..at], if rest[at..].starts_with('-') { -magnitude } else { magnitude })
        }
    };
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut whole = whole.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (whole.next()?.ok()?, whole.next()?.ok()?, whole.next()?.ok()?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 24 || minute > 59 || second > 59 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis: i64 = format!("{fraction:0<3}")[..3].parse().ok()?;
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(seconds * 1000 + millis).ok()
}

/// The POST to `generateAccessToken` both clients make, with the bearer token going to the configured URL.
fn generate_access_token_request(url: &str, bearer: &str, body: &Json, accept_json: bool, cancel: &Cancel) -> HttpRequest {
    let mut request = auth_request(url, "application/json", body.to_text(), accept_json, cancel);
    request.headers.set("authorization", &format!("Bearer {bearer}"));
    request
}

fn generated_token(data: &Json) -> Refreshed {
    Refreshed { access_token: data.str_at("accessToken").map(str::to_owned), expiry_ms: data.str_at("expireTime").and_then(iso_date_ms) }
}

/// `Impersonated.refreshToken`.
async fn refresh_impersonated(auth: &GoogleAuth, source: &mut Client, url: &str, delegates: &Json, lifetime: &str, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let wrap = |message: &str| format!("unable to impersonate: Error: {message}");
    let token = source.token(auth, cancel).await.map_err(|error| reworded(error, wrap))?;
    let body = jobj! { "delegates": delegates, "scope": vec![Json::from(SCOPE)], "lifetime": format!("{lifetime}s") };
    let reply = send(&auth.fetch, generate_access_token_request(url, &token, &body, false, cancel)).await.map_err(|error| reworded(error, wrap))?;
    let data = reply.typed_data();
    if !reply.ok() {
        let detail = data.get("error").filter(|error| error.is_obj());
        let field = |key: &str| detail.and_then(|error| error.get(key)).filter(|value| truthy(value)).map(js_string);
        let message = match (field("status"), field("message")) {
            (Some(status), Some(message)) => format!("{status}: unable to impersonate: {message}"),
            _ => wrap(&gaxios_message(reply.status, &data)),
        };
        return Err(token_failure(url, &reply, message));
    }
    Ok(generated_token(&data))
}

// ------------------------------------------------------------------ External accounts

/// `getErrorFromOAuthErrorResponse`.
fn oauth_message(data: &Json) -> String {
    let code = data.non_null("error").map_or_else(|| "undefined".to_owned(), js_string);
    let mut message = format!("Error code {code}");
    if let Some(description) = data.get("error_description") {
        message.push_str(&format!(": {}", js_string(description)));
    }
    if let Some(uri) = data.get("error_uri") {
        message.push_str(&format!(" - {}", js_string(uri)));
    }
    message
}

/// `#internalRefreshAccessTokenAsync`: the subject token, its exchange at STS, and the impersonation after it.
async fn refresh_external(auth: &GoogleAuth, account: &ExternalAccount, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let subject_token = match &account.source {
        SubjectSource::File { path, format } => subject_from_file(path, format)?,
        SubjectSource::Url { url, headers, format } => subject_from_url(&auth.fetch, url, headers, format, cancel).await?,
        SubjectSource::Aws { region_url, credentials_url, session_token_url, verification_url } => {
            let region = aws_region(&auth.fetch, region_url.as_deref(), session_token_url.as_deref(), cancel).await?;
            let credentials = aws_credentials(&auth.fetch, region, credentials_url.as_deref(), session_token_url.as_deref(), cancel).await?;
            aws_subject_token(verification_url, &account.audience, &credentials, &amz_datetime(now_ms()))?
        }
        SubjectSource::Executable { command, timeout, output_file } => {
            executable_subject_token(auth.adc.allow_executables, account, command, *timeout, output_file.as_deref(), cancel).await?
        }
    };
    let data = exchange_token(&auth.fetch, account, &subject_token, cancel).await?;
    let Some(url) = &account.impersonation_url else {
        return Ok(Refreshed { access_token: data.str_at("access_token").map(str::to_owned), expiry_ms: expires_in_ms(&data).map(after_now) });
    };
    let token = data.str_at("access_token").ok_or_else(|| AiError::other("Could not refresh access token."))?;
    let lifetime = account.lifetime.as_deref().unwrap_or("3600");
    let body = jobj! { "scope": vec![Json::from(SCOPE)], "lifetime": format!("{lifetime}s") };
    let reply = send(&auth.fetch, generate_access_token_request(url, token, &body, true, cancel)).await?;
    let data = reply.json_data();
    if !reply.ok() {
        return Err(token_failure(url, &reply, gaxios_message(reply.status, &data)));
    }
    Ok(generated_token(&data))
}

/// `StsCredentials.exchangeToken`. Client authentication wins over `workforce_pool_user_project`. The scope is the
/// same either way: `GoogleAuth`'s, or the one `generateAccessToken` requires of a token it is handed.
async fn exchange_token(fetch: &DynFetch, account: &ExternalAccount, subject_token: &str, cancel: &Cancel) -> Result<Json, AiError> {
    let options = account.workforce_pool_user_project.as_deref().filter(|_| account.client_auth.is_none()).map(|project| jobj! { "userProject": project }.to_text());
    let mut pairs = vec![
        ("grant_type", TOKEN_EXCHANGE_GRANT_TYPE),
        ("audience", account.audience.as_str()),
        ("scope", SCOPE),
        ("requested_token_type", ACCESS_TOKEN_TYPE),
        ("subject_token", subject_token),
    ];
    pairs.extend(account.subject_token_type.as_deref().map(|kind| ("subject_token_type", kind)));
    pairs.extend(options.as_deref().map(|options| ("options", options)));
    let mut request = auth_request(&account.token_url, FORM_CONTENT_TYPE, form_encode(&pairs), true, cancel);
    // `StsCredentials` has a transporter of its own, without the interceptor that adds this.
    request.headers.delete("user-agent");
    let source = match account.source {
        SubjectSource::File { .. } => "file",
        SubjectSource::Url { .. } => "url",
        SubjectSource::Aws { .. } => "aws",
        SubjectSource::Executable { .. } => "executable",
    };
    // `getMetricsHeaderValue`, with the same runtime segment as everywhere here.
    let (impersonating, config_lifetime) = (account.impersonation_url.is_some(), account.lifetime.is_some());
    request.headers.set("x-goog-api-client", &format!("gl-rust auth/10.9.1 google-byoid-sdk source/{source} sa-impersonation/{impersonating} config-lifetime/{config_lifetime}"));
    if let Some(client_auth) = &account.client_auth {
        request.headers.set("authorization", client_auth);
    }
    let reply = send(fetch, request).await?;
    let data = reply.json_data();
    if !reply.ok() {
        return Err(token_failure(&account.token_url, &reply, oauth_message(&data)));
    }
    Ok(data)
}

/// `ExternalAccountAuthorizedUserHandler.refreshToken`, and keeping the refresh token the reply may rotate.
async fn refresh_external_user(fetch: &DynFetch, token_url: &str, client_auth: &str, refresh_token: &mut String, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let body = form_encode(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token.as_str())]);
    let mut request = auth_request(token_url, FORM_CONTENT_TYPE, body, true, cancel);
    request.headers.set("authorization", client_auth);
    let reply = send(fetch, request).await?;
    let data = reply.json_data();
    if !reply.ok() {
        return Err(token_failure(token_url, &reply, oauth_message(&data)));
    }
    if let Some(next) = data.str_at("refresh_token") {
        *refresh_token = next.to_owned();
    }
    Ok(Refreshed { access_token: data.str_at("access_token").map(str::to_owned), expiry_ms: expires_in_ms(&data).map(after_now) })
}

/// `FileSubjectTokenSupplier`. A JSON file that does not parse fails with the parser's position, never its contents.
fn subject_from_file(path: &str, format: &TokenFormat) -> Result<String, AiError> {
    let text = read_real_file(Path::new(path))?;
    let token = match format {
        TokenFormat::Text => Some(text),
        TokenFormat::Json(field) => token_field(&Json::parse(&text).map_err(AiError::other)?, field),
    };
    token.filter(|token| !token.is_empty()).ok_or_else(|| AiError::other("Unable to parse the subject_token from the credential_source file"))
}

/// `UrlSubjectTokenSupplier`.
async fn subject_from_url(fetch: &DynFetch, url: &str, headers: &Headers, format: &TokenFormat, cancel: &Cancel) -> Result<String, AiError> {
    let mut headers = headers.clone();
    if matches!(format, TokenFormat::Json(_)) {
        headers.set("accept", "application/json");
    }
    let reply = checked_request(fetch, "GET", url, headers, cancel).await?;
    let token = match format {
        TokenFormat::Text => Some(reply.text),
        TokenFormat::Json(field) => token_field(&reply.json_data(), field),
    };
    token.filter(|token| !token.is_empty()).ok_or_else(|| AiError::other("Unable to parse the subject_token from the credential_source URL"))
}

fn token_field(data: &Json, field: &str) -> Option<String> {
    data.get(field).filter(|token| truthy(token)).map(js_string)
}

/// A request through the client's transporter for its body, with what the interceptor adds, a `User-Agent` given in
/// the file kept in front. Anything but 2xx is the error.
async fn checked_request(fetch: &DynFetch, method: &'static str, url: &str, headers: Headers, cancel: &Cancel) -> Result<Reply, AiError> {
    let mut headers = with_user_agent_suffix(headers, &[AUTH_USER_AGENT]);
    if !headers.has("x-goog-api-client") {
        headers.set("x-goog-api-client", "gl-rust");
    }
    let request = HttpRequest { method, url: url.to_owned(), headers, body: None, ca_roots: None, cancel: cancel.clone() };
    let reply = send(fetch, request).await?;
    if !reply.ok() {
        return Err(token_failure(url, &reply, gaxios_message(reply.status, &reply.json_data())));
    }
    Ok(reply)
}

// ------------------------------------------------------------------ External accounts: AWS

/// `#getImdsV2SessionToken`, as the header the metadata requests carry when the file names the URL.
async fn aws_metadata_headers(fetch: &DynFetch, session_token_url: Option<&str>, cancel: &Cancel) -> Result<Headers, AiError> {
    let mut headers = Headers::new();
    if let Some(url) = session_token_url {
        let mut request = Headers::new();
        request.set("x-aws-ec2-metadata-token-ttl-seconds", "300");
        let reply = checked_request(fetch, "PUT", url, request, cancel).await?;
        headers.set("x-aws-ec2-metadata-token", &reply.text);
    }
    Ok(headers)
}

/// `getAwsRegion`: the availability zone less its last character.
async fn aws_region(fetch: &DynFetch, region_url: Option<&str>, session_token_url: Option<&str>, cancel: &Cancel) -> Result<String, AiError> {
    let headers = aws_metadata_headers(fetch, session_token_url, cancel).await?;
    let url = region_url.ok_or_else(|| AiError::other("Unable to determine AWS region due to missing \"options.credential_source.region_url\""))?;
    let mut zone = checked_request(fetch, "GET", url, headers, cancel).await?.text;
    zone.pop();
    Ok(zone)
}

/// `getAwsSecurityCredentials`: the role attached to the VM, then its temporary keys.
async fn aws_credentials(fetch: &DynFetch, region: String, credentials_url: Option<&str>, session_token_url: Option<&str>, cancel: &Cancel) -> Result<Credentials, AiError> {
    let mut headers = aws_metadata_headers(fetch, session_token_url, cancel).await?;
    let url = credentials_url.ok_or_else(|| AiError::other("Unable to determine AWS role name due to missing \"options.credential_source.url\""))?;
    let role = checked_request(fetch, "GET", url, headers.clone(), cancel).await?.text;
    headers.set("accept", "application/json");
    let keys = checked_request(fetch, "GET", &format!("{url}/{role}"), headers, cancel).await?.json_data();
    let key = |name: &str| keys.str_at(name).map(str::to_owned).ok_or_else(|| AiError::other(format!("The AWS security credentials response does not contain {name}")));
    Ok(Credentials { region, access_key_id: key("AccessKeyId")?, secret_access_key: key("SecretAccessKey")?, session_token: keys.str_at("Token").map(str::to_owned) })
}

/// `AwsClient.retrieveSubjectToken`: a signed GetCallerIdentity request, serialized rather than sent. The audience
/// header goes on after signing, and the service is the host's first label.
fn aws_subject_token(verification_url: &str, audience: &str, credentials: &Credentials, datetime: &str) -> Result<String, AiError> {
    let url = verification_url.replacen("{region}", &credentials.region, 1);
    let host = url_host(&url).ok_or_else(|| AiError::other("Invalid URL"))?;
    let service = host.split('.').next().unwrap_or("");
    let signed = sign(&SignInput { method: "POST", url: &url, headers: &Headers::new(), body: "", credentials, service, datetime })?;
    let mut headers = signed.headers;
    headers.set("host", &host);
    headers.set("x-goog-cloud-target-resource", audience);
    // `Headers` iterates sorted.
    headers.0.sort();
    let headers: Vec<Json> = headers.0.iter().map(|(key, value)| jobj! { "key": key, "value": value }).collect();
    Ok(encode_uri_component(&jobj! { "url": url, "method": "POST", "headers": headers }.to_text()))
}

// ------------------------------------------------------------------ External accounts: executables

/// `ExecutableResponse`.
struct ExecutableResponse {
    version: Option<f64>,
    expiration_time: Option<f64>,
    /// The subject token, or the error the executable reported in its place.
    outcome: Result<String, String>,
}

impl ExecutableResponse {
    fn parse(json: &Json) -> Result<Self, AiError> {
        let present = |key: &str| json.get(key).filter(|value| truthy(value));
        let invalid = |message: String| AiError::other(message);
        let version = present("version").ok_or_else(|| invalid("Executable response must contain a 'version' field.".to_owned()))?.as_f64();
        let success = json.get("success").ok_or_else(|| invalid("Executable response must contain a 'success' field.".to_owned()))?;
        let mut expiration_time = None;
        let outcome = if truthy(success) {
            expiration_time = json.get("expiration_time").and_then(Json::as_f64);
            let kind = json.str_at("token_type").unwrap_or("");
            if kind == SAML_TOKEN_TYPE {
                Ok(js_string(present("saml_response").ok_or_else(|| invalid(format!("Executable response must contain a 'saml_response' field when token_type={SAML_TOKEN_TYPE}.")))?))
            } else if OIDC_TOKEN_TYPES.contains(&kind) {
                let [first, second] = OIDC_TOKEN_TYPES;
                Ok(js_string(present("id_token").ok_or_else(|| invalid(format!("Executable response must contain a 'id_token' field when token_type={first} or {second}.")))?))
            } else {
                let [first, second] = OIDC_TOKEN_TYPES;
                return Err(invalid(format!("Executable response must contain a 'token_type' field when successful and it must be one of {first}, {second}, or {SAML_TOKEN_TYPE}.")));
            }
        } else {
            let code = present("code").ok_or_else(|| invalid("Executable response must contain a 'code' field when unsuccessful.".to_owned()))?;
            let message = present("message").ok_or_else(|| invalid("Executable response must contain a 'message' field when unsuccessful.".to_owned()))?;
            Err(format!("The executable failed with exit code: {} and error message: {}.", js_string(code), js_string(message)))
        };
        Ok(Self { version, expiration_time, outcome })
    }

    /// `isExpired`.
    fn expired(&self) -> bool {
        self.expiration_time.is_some_and(|time| time < (now_ms() as f64 / 1000.0).round())
    }
}

/// `retrieveCachedResponse`: a good answer left in the output file saves running the executable. Its text can be the
/// token, so unlike upstream's its error does not repeat it.
fn cached_executable_response(output_file: &str) -> Result<Option<ExecutableResponse>, AiError> {
    let Ok(real) = std::fs::canonicalize(output_file) else { return Ok(None) };
    if !std::fs::symlink_metadata(&real).is_ok_and(|m| m.is_file()) {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&real).map_err(|e| AiError::other(format!("{e}, open '{}'", real.display())))?;
    if text.is_empty() {
        return Ok(None);
    }
    let json = Json::parse(&text).map_err(|_| AiError::other("The output file contained an invalid response."))?;
    let response = ExecutableResponse::parse(&json)?;
    Ok((!response.expired() && response.outcome.is_ok()).then_some(response))
}

/// `retrieveResponseFromExecutable`. Dropping the wait kills the child, on a cancel as on the timeout. What it printed
/// can be the token, so unlike upstream's these errors do not repeat it.
async fn run_executable(command: &[String], timeout: Duration, env: &[(&str, &str)], cancel: &Cancel) -> Result<ExecutableResponse, AiError> {
    let mut child = Command::new(&command[0]);
    child.args(&command[1..]).env_clear().envs(inherited_env()).envs(env.iter().copied()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    child.creation_flags(0x0800_0000);
    let child = child.spawn().map_err(|e| AiError::other(format!("The executable could not be started: {e}")))?;
    let output = tokio::select! {
        _ = cancel.cancelled() => return Err(AiError::Abort),
        waited = tokio::time::timeout(timeout, child.wait_with_output()) => match waited {
            Err(_) => return Err(AiError::other("The executable failed to finish within the timeout specified.")),
            Ok(waited) => waited.map_err(|e| AiError::other(format!("The executable could not be read: {e}")))?,
        },
    };
    if !output.status.success() {
        let code = output.status.code().map_or_else(|| "null".to_owned(), |code| code.to_string());
        return Err(AiError::other(format!("The executable failed with exit code: {code}.")));
    }
    // Upstream appends stdout and stderr as they arrive.
    let mut printed = String::from_utf8_lossy(&output.stdout).into_owned();
    printed.push_str(&String::from_utf8_lossy(&output.stderr));
    let json = Json::parse(&printed).map_err(|_| AiError::other("The executable returned an invalid response."))?;
    ExecutableResponse::parse(&json)
}

/// `PluggableAuthClient.retrieveSubjectToken`.
async fn executable_subject_token(allowed: bool, account: &ExternalAccount, command: &[String], timeout: Duration, output_file: Option<&str>, cancel: &Cancel) -> Result<String, AiError> {
    if !allowed {
        return Err(AiError::other("Pluggable Auth executables need to be explicitly allowed to run by setting the GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES environment Variable to 1."));
    }
    let cached = output_file.map(cached_executable_response).transpose()?.flatten();
    let response = match cached {
        Some(response) => response,
        None => {
            let email = impersonated_email(account)?;
            let mut env = vec![("GOOGLE_EXTERNAL_ACCOUNT_AUDIENCE", account.audience.as_str()), ("GOOGLE_EXTERNAL_ACCOUNT_INTERACTIVE", "0")];
            env.extend(account.subject_token_type.as_deref().map(|kind| ("GOOGLE_EXTERNAL_ACCOUNT_TOKEN_TYPE", kind)));
            env.extend(output_file.map(|file| ("GOOGLE_EXTERNAL_ACCOUNT_OUTPUT_FILE", file)));
            env.extend(email.map(|email| ("GOOGLE_EXTERNAL_ACCOUNT_IMPERSONATED_EMAIL", email)));
            run_executable(command, timeout, &env, cancel).await?
        }
    };
    if response.version.is_some_and(|version| version > MAXIMUM_EXECUTABLE_VERSION) {
        return Err(AiError::other(format!("Version of executable is not currently supported, maximum supported version is {MAXIMUM_EXECUTABLE_VERSION}.")));
    }
    if response.outcome.is_ok() && output_file.is_some() && !response.expiration_time.is_some_and(|time| time != 0.0) {
        return Err(AiError::other("The executable response must contain the `expiration_time` field for successful responses when an output_file has been specified in the configuration."));
    }
    let expired = response.expired();
    let token = response.outcome.map_err(AiError::other)?;
    if expired {
        return Err(AiError::other("Executable response is expired."));
    }
    Ok(token)
}

/// `getServiceAccountEmail`.
fn impersonated_email(account: &ExternalAccount) -> Result<Option<&str>, AiError> {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    let Some(url) = &account.impersonation_url else { return Ok(None) };
    if url.len() > 256 {
        return Err(AiError::other(format!("URL is too long: {url}")));
    }
    let email = EMAIL.get_or_init(|| Regex::new(r"serviceAccounts/([^:]+):generateAccessToken$").expect("a literal pattern"));
    Ok(email.captures(url).and_then(|found| found.get(1)).map(|found| found.as_str()))
}

// ------------------------------------------------------------------ Google Distributed Cloud

/// An ES256 key as Node's `crypto` takes one. `openssl ecparam -genkey` writes an `EC PARAMETERS` block first.
fn ec_signing_key(pem: &str) -> Option<EcSigningKey<P256>> {
    let mut rest = pem;
    while let Some((label, der, after)) = pem_block(rest) {
        match label {
            "PRIVATE KEY" => return EcSigningKey::from_pkcs8_der(&der).ok(),
            "EC PRIVATE KEY" => return EcSigningKey::from_sec1_der(&der).ok(),
            _ => rest = after,
        }
    }
    None
}

/// `GdchClient.createAssertion`, with the signature in `ieee-p1363` form.
fn gdch_assertion(gdch: &Gdch, now_seconds: u64) -> Result<String, AiError> {
    let key = ec_signing_key(&gdch.private_key).ok_or_else(|| AiError::other(BAD_PRIVATE_KEY))?;
    let issuer = format!("system:serviceaccount:{}:{}", gdch.project, gdch.name);
    let header = jobj! { "alg": "ES256", "typ": "JWT", "kid": &gdch.private_key_id };
    let payload = jobj! { "iss": &issuer, "sub": &issuer, "iat": now_seconds, "exp": now_seconds + 3600, "aud": &gdch.token_uri };
    let url = |text: &str| base64_simd::URL_SAFE_NO_PAD.encode_to_string(text.as_bytes());
    let message = format!("{}.{}", url(&header.to_text()), url(&payload.to_text()));
    let mut signature = [0u8; 64];
    let signature = key.sign::<Sha256>(&[message.as_bytes()], &mut signature).map_err(|e| AiError::other(format!("{e:?}")))?;
    Ok(format!("{message}.{}", base64_simd::URL_SAFE_NO_PAD.encode_to_string(signature)))
}

/// `getCaAgent`: the certificates of `ca_cert_path`, which replace the bundled roots for the token request. Upstream
/// finds out a file is bad at the handshake, where it would look transient.
fn ca_roots(path: &str) -> Result<Arc<Vec<Vec<u8>>>, AiError> {
    let failed = |detail: String| AiError::other(format!("Error reading certificate file from CA cert path, value '{path}': {detail}"));
    let pem = std::fs::read_to_string(path).map_err(|e| failed(e.to_string()))?;
    let mut store = rustls::RootCertStore::empty();
    let mut roots = Vec::new();
    let mut rest = pem.as_str();
    while let Some((label, der, after)) = pem_block(rest) {
        if label == "CERTIFICATE" {
            store.add(der.clone().into()).map_err(|e| failed(e.to_string()))?;
            roots.push(der);
        }
        rest = after;
    }
    if roots.is_empty() {
        return Err(failed("no certificates in the file".to_owned()));
    }
    Ok(Arc::new(roots))
}

/// `GdchClient.refreshTokenNoCache`. A reply without a token or an `expires_in` is a failure, not an expired token.
async fn refresh_gdch(auth: &GoogleAuth, gdch: &Gdch, cancel: &Cancel) -> Result<Refreshed, AiError> {
    let ca_roots = gdch.ca_cert_path.as_deref().map(ca_roots).transpose()?;
    let assertion = gdch_assertion(gdch, now_ms() / 1000)?;
    let body = jobj! {
        "audience": &auth.api_audience,
        "grant_type": GDCH_GRANT_TYPE,
        "requested_token_type": ACCESS_TOKEN_TYPE,
        "subject_token": assertion,
        "subject_token_type": GDCH_SUBJECT_TOKEN_TYPE,
    };
    let mut request = auth_request(&gdch.token_uri, "application/json", body.to_text(), true, cancel);
    request.ca_roots = ca_roots;
    let wrap = |message: &str| format!("Error getting access token for GDCH service account: {message}, iss: {}", gdch.name);
    let reply = send(&auth.fetch, request).await.map_err(|error| reworded(error, wrap))?;
    let data = reply.json_data();
    if !reply.ok() {
        return Err(token_failure(&gdch.token_uri, &reply, wrap(&gaxios_message(reply.status, &data))));
    }
    let token = data.str_at("access_token").filter(|token| !token.is_empty()).ok_or_else(|| AiError::other(wrap("Token response did not contain an access_token.")))?;
    let expires_in = expires_in_ms(&data).ok_or_else(|| AiError::other(wrap("Token response did not contain an expires_in field.")))?;
    Ok(Refreshed { access_token: Some(token.to_owned()), expiry_ms: Some(after_now(expires_in)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_matches_url_search_params() {
        assert_eq!(form_encode(&[("a b", "1//r t+é*-._~")]), "a+b=1%2F%2Fr+t%2B%C3%A9*-._%7E");
        assert_eq!(form_encode(&[("grant_type", JWT_GRANT_TYPE), ("x", "")]), "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&x=");
    }

    #[test]
    fn gaxios_messages_read_the_body_as_upstream_does() {
        let message = |status, text: &str| gaxios_message(status, &Json::parse(text).unwrap());
        assert_eq!(message(400, r#"{"error":"invalid_grant","error_description":"x"}"#), "invalid_grant");
        assert_eq!(message(403, r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#), "denied");
        assert_eq!(message(403, r#"{"error":{"errors":[{"message":"a"},{"message":"b"}]}}"#), "a\nb");
        assert_eq!(message(500, r#"{"other":1}"#), "Request failed with status code 500");
        assert_eq!(gaxios_message(401, &Json::from("nope")), "nope");
    }

    #[test]
    fn credential_kinds_and_their_missing_fields() {
        let error = |text: &str| from_json(&Json::parse(text).unwrap()).err().map(|e| e.message());
        assert_eq!(error(r#"{"type":"service_account","private_key":"k"}"#).as_deref(), Some("The incoming JSON object does not contain a client_email field"));
        assert_eq!(error(r#"{"type":"service_account","client_email":"e"}"#).as_deref(), Some("The incoming JSON object does not contain a private_key field"));
        assert_eq!(error(r#"{"type":"authorized_user","client_secret":"s","refresh_token":"r"}"#).as_deref(), Some("The incoming JSON object does not contain a client_id field"));
        assert_eq!(error(r#"{"type":"authorized_user","client_id":"c","refresh_token":"r"}"#).as_deref(), Some("The incoming JSON object does not contain a client_secret field"));
        assert_eq!(error(r#"{"type":"authorized_user","client_id":"c","client_secret":"s"}"#).as_deref(), Some("The incoming JSON object does not contain a refresh_token field"));
        assert_eq!(error(r#"{"type":"something_else"}"#).as_deref(), Some("The incoming JSON object does not contain a client_email field"));
        assert!(matches!(from_json(&Json::parse(r#"{"client_email":"e","private_key":"k"}"#).unwrap()), Ok(Credential::ServiceAccount { .. })));
    }

    /// Every message here is what `GoogleAuth` under node threw for the same file.
    #[test]
    fn the_other_credential_kinds_refuse_what_upstream_refuses() {
        let error = |json: Json| from_json(&json).err().map(|e| e.message());
        let user = || jobj! { "type": "authorized_user", "client_id": "c", "client_secret": "s", "refresh_token": "r" };
        let iam = "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/t@p.iam:generateAccessToken";
        let impersonated = |source: Option<Json>, url: Option<&str>, universe: Option<&str>| {
            jobj! { "type": "impersonated_service_account", "source_credentials"?: source, "service_account_impersonation_url"?: url, "universe_domain"?: universe }
        };
        assert_eq!(error(impersonated(Some(user()), None, None)).as_deref(), Some("The incoming JSON object does not contain a service_account_impersonation_url field"));
        assert_eq!(error(impersonated(None, Some(iam), None)).as_deref(), Some("The incoming JSON object does not contain a source_credentials field"));
        assert_eq!(error(impersonated(Some(user()), Some("https://x/y"), None)).as_deref(), Some("Cannot extract target principal from https://x/y"));
        let long = format!("https://x/{}", "y".repeat(260));
        assert_eq!(error(impersonated(Some(user()), Some(&long), None)), Some(format!("Target principal is too long: {long}")));
        assert_eq!(
            error(impersonated(Some(user()), Some(iam), Some("other.example"))).as_deref(),
            Some("Universe domain googleapis.com in source credentials does not match other.example universe domain set for impersonated credentials.")
        );
        assert_eq!(error(impersonated(Some(jobj! { "type": "authorized_user" }), Some(iam), None)).as_deref(), Some("The incoming JSON object does not contain a client_id field"));

        let external = |rest: &str| error(Json::parse(&format!(r#"{{"type":"external_account",{rest}}}"#)).unwrap());
        let refused = |rest: &str| external(rest).unwrap_or_default();
        assert_eq!(refused(r#""x":1"#), "Cannot read properties of undefined (reading 'match')");
        assert_eq!(refused(r#""audience":"a""#), "A credential source or subject token supplier must be specified.");
        assert_eq!(refused(r#""audience":"a","workforce_pool_user_project":"p","credential_source":{"file":"f"}"#), "workforcePoolUserProject should not be set for non-workforce pool credentials.");
        assert_eq!(refused(r#""audience":"a","credential_source":{"file":"f","format":{"type":"xml"}}"#), "Invalid credential_source format \"xml\"");
        assert_eq!(refused(r#""audience":"a","credential_source":{"file":"f","format":{"type":"json"}}"#), "Missing subject_token_field_name for JSON credential_source format");
        let neither = "No valid Identity Pool \"credential_source\" provided, must be either file, url, or certificate.";
        assert_eq!(refused(r#""audience":"a","credential_source":{"file":"f","url":"u"}"#), neither);
        assert_eq!(refused(r#""audience":"a","credential_source":{"other":1}"#), neither);
        assert!(refused(r#""audience":"a","credential_source":{"certificate":{"use_default_certificate_config":true}}"#).contains("not supported"));
        assert_eq!(refused(r#""audience":"a","credential_source":{"environment_id":"aws2","regional_cred_verification_url":"x"}"#), "aws version \"2\" is not supported in the current build.");
        assert_eq!(refused(r#""audience":"a","credential_source":{"environment_id":"gcp1","regional_cred_verification_url":"x"}"#), "No valid AWS \"credential_source\" provided");
        assert_eq!(refused(r#""audience":"a","credential_source":{"environment_id":"aws1"}"#), "No valid AWS \"credential_source\" provided");
        assert_eq!(refused(r#""audience":"a","credential_source":{"executable":{"timeout_millis":5000}}"#), "No valid Pluggable Auth \"credential_source\" provided.");
        assert_eq!(refused(r#""audience":"a","credential_source":{"executable":{"command":"x","timeout_millis":10}}"#), "Timeout must be between 5000 and 120000 milliseconds.");
        assert_eq!(refused(r#""audience":"a","credential_source":{"executable":{"command":"   "}}"#), "Provided command: \"   \" could not be parsed.");
        let workforce = r#""audience":"//iam.googleapis.com/locations/global/workforcePools/p/providers/q","workforce_pool_user_project":"p","credential_source":{"file":"f"}"#;
        assert!(external(workforce).is_none());

        let gdch = |rest: &str| error(Json::parse(&format!(r#"{{"type":"gdch_service_account",{rest}}}"#)).unwrap());
        assert_eq!(gdch(r#""format_version":"2""#).as_deref(), Some("Only format version 1 is supported."));
        assert_eq!(gdch(r#""format_version":1"#).as_deref(), Some("Only format version 1 is supported."));
        assert_eq!(gdch(r#""format_version":"1""#).as_deref(), Some("The incoming JSON object does not contain a project field"));
        assert_eq!(gdch(r#""format_version":"1","project":"p""#).as_deref(), Some("The incoming JSON object does not contain a private_key_id field"));
        assert_eq!(gdch(r#""format_version":"1","project":"p","private_key_id":"k","private_key":"pk""#).as_deref(), Some("The incoming JSON object does not contain a name field"));
        assert_eq!(gdch(r#""format_version":"1","project":"p","private_key_id":"k","private_key":"pk","name":"n""#).as_deref(), Some("The incoming JSON object does not contain a token_uri field"));
    }

    /// Out of `AwsClient.retrieveSubjectToken` under node, with `Date` held at 1700000000123.
    #[test]
    fn the_aws_subject_token_is_the_librarys() {
        let credentials = |region: &str, session_token: Option<&str>| Credentials {
            region: region.to_owned(),
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: session_token.map(str::to_owned),
        };
        let url = "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
        let audience = "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/pool/providers/prov";
        let datetime = amz_datetime(1_700_000_000_123);
        assert_eq!(
            aws_subject_token(url, audience, &credentials("us-east-2", Some("SESSION/tok==")), &datetime).unwrap(),
            "%7B%22url%22%3A%22https%3A%2F%2Fsts.us-east-2.amazonaws.com%3FAction%3DGetCallerIdentity%26Version%3D2011-06-15%22%2C%22method%22%3A%22POST%22%2C%22headers%22%3A%5B%7B%22key%22%3A%22authorization%22%2C%22value%22%3A%22AWS4-HMAC-SHA256%20Credential%3DAKIDEXAMPLE%2F20231114%2Fus-east-2%2Fsts%2Faws4_request%2C%20SignedHeaders%3Dhost%3Bx-amz-date%3Bx-amz-security-token%2C%20Signature%3Db3fad79c96f1198f9694062a281f2ff0131bb15f33d5153f0f621e0a82a3c0cf%22%7D%2C%7B%22key%22%3A%22host%22%2C%22value%22%3A%22sts.us-east-2.amazonaws.com%22%7D%2C%7B%22key%22%3A%22x-amz-date%22%2C%22value%22%3A%2220231114T221320Z%22%7D%2C%7B%22key%22%3A%22x-amz-security-token%22%2C%22value%22%3A%22SESSION%2Ftok%3D%3D%22%7D%2C%7B%22key%22%3A%22x-goog-cloud-target-resource%22%2C%22value%22%3A%22%2F%2Fiam.googleapis.com%2Fprojects%2F1%2Flocations%2Fglobal%2FworkloadIdentityPools%2Fpool%2Fproviders%2Fprov%22%7D%5D%7D"
        );
        assert_eq!(
            aws_subject_token(url, audience, &credentials("eu-west-1", None), &datetime).unwrap(),
            "%7B%22url%22%3A%22https%3A%2F%2Fsts.eu-west-1.amazonaws.com%3FAction%3DGetCallerIdentity%26Version%3D2011-06-15%22%2C%22method%22%3A%22POST%22%2C%22headers%22%3A%5B%7B%22key%22%3A%22authorization%22%2C%22value%22%3A%22AWS4-HMAC-SHA256%20Credential%3DAKIDEXAMPLE%2F20231114%2Feu-west-1%2Fsts%2Faws4_request%2C%20SignedHeaders%3Dhost%3Bx-amz-date%2C%20Signature%3D861004f6a4e3b560acfa267bd26a0606cd7ecbcc0eb3f9422c665aa4da93e562%22%7D%2C%7B%22key%22%3A%22host%22%2C%22value%22%3A%22sts.eu-west-1.amazonaws.com%22%7D%2C%7B%22key%22%3A%22x-amz-date%22%2C%22value%22%3A%2220231114T221320Z%22%7D%2C%7B%22key%22%3A%22x-goog-cloud-target-resource%22%2C%22value%22%3A%22%2F%2Fiam.googleapis.com%2Fprojects%2F1%2Flocations%2Fglobal%2FworkloadIdentityPools%2Fpool%2Fproviders%2Fprov%22%7D%5D%7D"
        );
    }

    #[test]
    fn expiry_stamps_read_as_date_parse_reads_them() {
        assert_eq!(iso_date_ms("2031-01-01T00:00:00.123456Z"), Some(1_924_992_000_123));
        assert_eq!(iso_date_ms("2031-01-01T00:00:00Z"), Some(1_924_992_000_000));
        assert_eq!(iso_date_ms("2031-01-01T01:30:00+01:30"), Some(1_924_992_000_000));
        assert_eq!(iso_date_ms("2030-12-31T23:00:00.5-01:00"), Some(1_924_992_000_500));
        assert_eq!(iso_date_ms("2024-02-29T00:00:00Z"), Some(1_709_164_800_000));
        assert_eq!(iso_date_ms("soon"), None);
        assert_eq!(iso_date_ms("2031-13-01T00:00:00Z"), None);
    }

    #[test]
    fn commands_split_as_parse_command_splits_them() {
        let parsed = |command: &str| parse_command(command).unwrap();
        assert_eq!(parsed("/bin/tool --flag value"), ["/bin/tool", "--flag", "value"]);
        assert_eq!(parsed(r#""/Program Files/tool" "--a b" c"#), ["/Program Files/tool", "--a b", "c"]);
        assert_eq!(parsed(r#"--name="a b""#), [r#"--name="a b""#]);
    }

    #[test]
    fn executable_responses_are_held_to_their_fields() {
        let error = |text: &str| ExecutableResponse::parse(&Json::parse(text).unwrap()).err().map(|e| e.message()).unwrap_or_default();
        assert_eq!(error(r#"{"success":true}"#), "Executable response must contain a 'version' field.");
        assert_eq!(error(r#"{"version":1}"#), "Executable response must contain a 'success' field.");
        assert!(error(r#"{"version":1,"success":true,"token_type":"x"}"#).starts_with("Executable response must contain a 'token_type' field when successful"));
        assert!(error(r#"{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:jwt"}"#).starts_with("Executable response must contain a 'id_token' field"));
        assert!(error(r#"{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:saml2"}"#).starts_with("Executable response must contain a 'saml_response' field"));
        assert_eq!(error(r#"{"version":1,"success":false,"message":"m"}"#), "Executable response must contain a 'code' field when unsuccessful.");
        assert_eq!(error(r#"{"version":1,"success":false,"code":"401"}"#), "Executable response must contain a 'message' field when unsuccessful.");
        let parsed = ExecutableResponse::parse(&Json::parse(r#"{"version":1,"success":false,"code":"401","message":"no"}"#).unwrap()).unwrap();
        assert_eq!(parsed.outcome.unwrap_err(), "The executable failed with exit code: 401 and error message: no.");
        let saml = ExecutableResponse::parse(&Json::parse(r#"{"version":1,"success":true,"token_type":"urn:ietf:params:oauth:token-type:saml2","saml_response":"s","expiration_time":1}"#).unwrap()).unwrap();
        assert!(saml.expired());
        assert_eq!(saml.outcome.unwrap(), "s");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_executable_past_its_timeout_is_given_up_on() {
        let command = ["/bin/sleep".to_owned(), "30".to_owned()];
        let started = std::time::Instant::now();
        let error = run_executable(&command, Duration::from_millis(50), &[], &Cancel::new()).await.err().unwrap();
        assert_eq!(error.message(), "The executable failed to finish within the timeout specified.");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Passes either way: a run with the variable set checks the other half.
    #[test]
    fn executables_follow_the_variable() {
        let set = std::env::var("GOOGLE_EXTERNAL_ACCOUNT_ALLOW_EXECUTABLES").as_deref() == Ok("1");
        assert_eq!(Adc::from_env().allow_executables, set);
    }

    #[test]
    fn the_gdch_audience_is_the_origin_of_the_endpoint() {
        assert_eq!(api_audience("https://aiplatform.org.zone.gdch.test/v1/projects/p"), "https://aiplatform.org.zone.gdch.test");
        assert_eq!(api_audience("http://127.0.0.1:8080/base"), "http://127.0.0.1:8080");
        assert_eq!(api_audience("https://host"), "https://host");
    }

    #[test]
    fn a_key_that_is_not_pem_fails_as_node_does() {
        assert_eq!(signed_assertion("e", "not a pem", 1).unwrap_err().message(), BAD_PRIVATE_KEY);
    }

    #[test]
    fn expiry_is_what_the_clients_compute() {
        let client = |expiry_ms| Client { credential: Credential::Compute, access_token: Some("t".to_owned()), expiry_ms };
        assert_eq!(client(None).usable_token(), Some("t"));
        assert_eq!(client(Some(now_ms() + EAGER_REFRESH_MS + 60_000)).usable_token(), Some("t"));
        assert_eq!(client(Some(now_ms() + EAGER_REFRESH_MS - 1000)).usable_token(), None);
        assert_eq!(expires_in_ms(&Json::parse(r#"{"expires_in":"60"}"#).unwrap()), Some(60_000.0));
        assert_eq!(expires_in_ms(&Json::parse(r#"{"expires_in":0}"#).unwrap()), None);
    }
}
