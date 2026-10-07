import type {
  EndpointType,
  FamilySetting,
  ModelCapability,
  ModelProfile,
  ProviderFamily,
  ReasoningContent,
  ReasoningContext,
} from "../types";

/**
 * Endpoint-type catalog in the same order as Rust `EndpointType::CATALOG`.
 * `modelCapabilities.test.ts` compares both sides exactly.
 */
export const ENDPOINT_TYPES: readonly EndpointType[] = [
  "openai_chat_completions",
  "openai_responses",
  "anthropic_messages",
  "google_generative",
  "azure_openai",
  "bedrock_converse",
  "openai_image_generation",
  "openai_image_edit",
  "openai_text_to_speech",
  "openai_audio_transcription",
];

/** Adapter family to chat endpoint type. Mirrors Rust `ProviderFamily::chat_endpoint`. */
export function chatEndpointOf(family: ProviderFamily): EndpointType {
  switch (family) {
    case "openai_responses":
    case "openai_codex":
      return "openai_responses";
    // xAI and the generic compatibility layer use the `/chat/completions` shape.
    case "openai_chat":
    case "xai":
    case "openai_compatible":
      return "openai_chat_completions";
    // The Claude Code CLI speaks Messages upstream, so the endpoint type is a
    // model vocabulary here rather than a request path the host issues itself.
    case "anthropic":
    case "claude_agent":
      return "anthropic_messages";
    case "google":
    case "vertex":
      return "google_generative";
    case "azure":
      return "azure_openai";
    case "bedrock":
      return "bedrock_converse";
  }
}

/** Required identity fields for this family. Mirrors Rust `ProviderFamily::required_settings`. */
export function requiredFamilySettings(family: ProviderFamily): readonly FamilySetting[] {
  switch (family) {
    case "bedrock":
      return ["region"];
    case "vertex":
      return ["project", "location"];
    default:
      return [];
  }
}

/**
 * Whether the chat base URL is derived from identity fields. Mirrors Rust
 * `ProviderFamily::derives_base_url`. Vertex and Bedrock derive endpoints from their
 * identity fields, Codex derives its default endpoint from the ChatGPT backend, and
 * Claude Agent lets the local CLI pick the endpoint, so an empty Base URL remains valid.
 */
export function derivesBaseUrl(family: ProviderFamily): boolean {
  return family === "vertex"
    || family === "bedrock"
    || family === "openai_codex"
    || family === "claude_agent";
}

/** Whether this provider has a usable chat endpoint. */
export function hasUsableBaseUrl(provider: { family: ProviderFamily; baseUrl: string }): boolean {
  return provider.baseUrl.trim().length > 0 || derivesBaseUrl(provider.family);
}

/**
 * Whether this provider has a model catalog Fetch models can read. Mirrors Rust
 * `ProviderFamily::lists_models_without_address`.
 *
 * Every catalog is read from the provider's address, except that Codex reads its
 * fixed ChatGPT backend and Claude Agent asks the bundled CLI. Bedrock and Vertex
 * derive a chat endpoint from their identity fields, but nothing there lists
 * models, so with a blank address their model IDs are added by hand.
 */
export function hasModelCatalog(provider: { family: ProviderFamily; baseUrl: string }): boolean {
  return provider.baseUrl.trim().length > 0
    || provider.family === "openai_codex"
    || provider.family === "claude_agent";
}

/**
 * Identity fields recognized by this family, including optional fields. Settings use
 * this to determine which inputs to display. Mirrors Rust `ProviderFamily::known_settings`.
 */
export function knownFamilySettings(family: ProviderFamily): readonly FamilySetting[] {
  switch (family) {
    case "bedrock":
      return ["region"];
    case "vertex":
      return ["project", "location"];
    // Azure's `api_version` is optional because the AI SDK provides a default.
    case "azure":
      return ["api_version"];
    // Claude Agent has no identity fields; Mewrk bundles and version-locks the
    // executable, which the host locates itself.
    case "claude_agent":
    default:
      return [];
  }
}

/** Capability catalog in chip-rendering order, matching Rust `ModelCapability::CATALOG`. */
export const MODEL_CAPABILITIES: readonly ModelCapability[] = [
  "image_recognition",
  "tool_append",
  "system_append",
  "async_tools",
  "native_compaction",
];

/** Reasoning-form catalog in selector-rendering order, matching Rust `ReasoningContent::CATALOG`. */
export const REASONING_CONTENTS: readonly ReasoningContent[] = [
  "plaintext",
  "encrypted",
];

export function supportsVision(model: ModelProfile): boolean {
  return model.capabilities.includes("image_recognition");
}

/** De-duplicated capabilities in catalog order, for persistence and rendering. */
export function normalizeCapabilities(values: readonly unknown[]): ModelCapability[] {
  const requested = new Set(values.filter((value): value is ModelCapability => (
    typeof value === "string" && (MODEL_CAPABILITIES as readonly string[]).includes(value)
  )));
  return MODEL_CAPABILITIES.filter((capability) => requested.has(capability));
}

/**
 * Persistence form of reasoning content. The field is concrete on every model, so
 * an unrecognized or absent value — including the retired `"auto"` in an older
 * document — resolves against the family here rather than staying deferred.
 */
export function normalizeReasoningContent(
  value: unknown,
  family: ProviderFamily
): ReasoningContent {
  if (typeof value === "string" && (REASONING_CONTENTS as readonly string[]).includes(value)) {
    return value as ReasoningContent;
  }
  return reasoningContentTakesEffect(family) ? "encrypted" : "plaintext";
}

/**
 * Whether this family has a real consumer for {@link ReasoningContent}. Mirrors Rust
 * `ProviderFamily::reasoning_content_takes_effect`. Responses-family providers,
 * including Codex, and Azure use this setting to determine `include`; for other
 * families the upstream decides and the stored value is descriptive.
 */
export function reasoningContentTakesEffect(family: ProviderFamily): boolean {
  return family === "openai_responses" || family === "openai_codex" || family === "azure";
}

/** The most pages a PDF may have to go to the model as itself. Mirrors Rust `MAX_NATIVE_PDF_PAGES`. */
export const MAX_NATIVE_PDF_PAGES = 10;

/**
 * Whether this family and model read a PDF as the document itself rather than
 * its extracted text. Mirrors Rust `aisdk::step::reads_pdf_documents`: the
 * families whose wire has a document part, on a model that takes images.
 */
export function readsPdfDocuments(family: ProviderFamily, vision: boolean): boolean {
  return vision && (
    family === "anthropic"
    || family === "claude_agent"
    || family === "openai_responses"
    || family === "openai_codex"
    || family === "azure"
    || family === "openai_chat"
    || family === "google"
    || family === "vertex"
  );
}

/**
 * Persistence form of the prompt-cache attribute. A boolean has no family-derived
 * default: anything that is not a boolean, including the absent key of an older
 * document, resolves to Claude Code's default of enabled.
 */
export function normalizePromptCache(value: unknown): boolean {
  return typeof value === "boolean" ? value : true;
}

/**
 * Whether this family's dialect places prompt-cache breakpoints, so the model's
 * `promptCache` attribute reaches the wire. Mirrors Rust
 * `ProviderFamily::prompt_cache_takes_effect`: only the Messages protocol takes
 * explicit `cache_control` markers.
 */
export function promptCacheTakesEffect(family: ProviderFamily): boolean {
  return family === "anthropic";
}

/** Models the Messages API takes `tool_addition` from. Mirrors Rust `ANTHROPIC_TOOL_CHANGE_MODELS`. */
const ANTHROPIC_TOOL_CHANGE_MODELS = ["fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5"] as const;
/** Earlier Claude models the same documentation leaves out. Mirrors Rust `ANTHROPIC_EARLIER_MODELS`. */
const ANTHROPIC_EARLIER_MODELS = [
  "claude-instant", "claude-2", "claude-3", "opus-4", "sonnet-4", "haiku-4", "haiku-3",
] as const;
/** Current models documented without it, as released. Mirrors Rust `ANTHROPIC_RELEASES_WITHOUT`. */
const ANTHROPIC_RELEASES_WITHOUT = ["sonnet-5"] as const;
/** Models Claude Code appends tools for itself. Mirrors Rust `CLAUDE_CODE_TOOL_CHANGE_MODELS`. */
const CLAUDE_CODE_TOOL_CHANGE_MODELS = ["fable-5", "mythos-5", "opus-5", "opus-4-8"] as const;

/**
 * Whether `modelId` names one of `families`, followed by whatever `restOk`
 * accepts, the family appearing whole (at the start or after a `-`). Case,
 * `.` for `-` and Claude Code's `[1m]` suffix do not matter. Mirrors Rust
 * `tool_append::names_model_with`.
 */
function namesModelWith(modelId: string, families: readonly string[], restOk: (rest: string) => boolean): boolean {
  const id = modelId.toLowerCase().replaceAll(".", "-").replace(/\[1m\]$/, "");
  return families.some((family) => {
    let at = id.indexOf(family);
    while (at !== -1) {
      if ((at === 0 || id[at - 1] === "-") && restOk(id.slice(at + family.length))) return true;
      at = id.indexOf(family, at + 1);
    }
    return false;
  });
}

/** The family followed by the end of the id or another `-` segment. Mirrors Rust `names_model`. */
function namesModel(modelId: string, families: readonly string[]): boolean {
  return namesModelWith(modelId, families, (rest) => rest === "" || rest.startsWith("-"));
}

/** The family as released: the end of the id or a dated snapshot. Mirrors Rust `names_release`. */
function namesRelease(modelId: string, families: readonly string[]): boolean {
  return namesModelWith(modelId, families, (rest) => rest === "" || /^-\d{8}$/u.test(rest));
}

/**
 * Whether `baseUrl` is the vendor's own endpoint for `family`; empty is the
 * provider's default, which is. Mirrors Rust `host_append::at_vendor_endpoint`.
 */
export function atVendorEndpoint(family: ProviderFamily, baseUrl: string): boolean {
  const trimmed = baseUrl.trim();
  if (!trimmed) return true;
  let host: string;
  try {
    host = new URL(trimmed).hostname.toLowerCase();
  } catch {
    return false;
  }
  switch (family) {
    case "anthropic":
      return host === "api.anthropic.com";
    case "openai_responses":
    case "openai_codex":
    case "azure":
    case "openai_chat":
      return host === "api.openai.com"
        || host === "chatgpt.com"
        || [".openai.azure.com", ".cognitiveservices.azure.com", ".services.ai.azure.com"]
          .some((suffix) => host.endsWith(suffix));
    default:
      return false;
  }
}

/** What Anthropic documents for this model at its own endpoint. Mirrors Rust `tool_append::anthropic_documents`. */
function anthropicDocuments(modelId: string): boolean | null {
  if (namesModel(modelId, ANTHROPIC_TOOL_CHANGE_MODELS)) return true;
  if (namesModel(modelId, ANTHROPIC_EARLIER_MODELS) || namesRelease(modelId, ANTHROPIC_RELEASES_WITHOUT)) return false;
  return null;
}

/**
 * What Mewrk knows about this model taking a tool mid-conversation at this
 * endpoint; `null` is the user's to say (a relay, a model the vendor's
 * documentation does not cover). Mirrors Rust `tool_append::known`.
 */
export function knownToolAppend(family: ProviderFamily, baseUrl: string, modelId: string): boolean | null {
  const vendor = atVendorEndpoint(family, baseUrl);
  switch (family) {
    case "openai_codex":
      return true;
    case "openai_responses":
    case "azure":
      return vendor ? true : null;
    case "anthropic":
      return vendor ? anthropicDocuments(modelId) : null;
    case "claude_agent":
      return namesModel(modelId, CLAUDE_CODE_TOOL_CHANGE_MODELS);
    default:
      return false;
  }
}

/**
 * What Mewrk knows about this model taking a system message mid-conversation
 * at this endpoint; `null` is the user's to say. Mirrors Rust
 * `system_append::known`.
 */
export function knownSystemAppend(family: ProviderFamily, baseUrl: string, modelId: string): boolean | null {
  const vendor = atVendorEndpoint(family, baseUrl);
  switch (family) {
    case "openai_responses":
    case "openai_codex":
    case "azure":
      return true;
    case "openai_chat":
      return vendor ? true : null;
    case "anthropic":
      return vendor ? anthropicDocuments(modelId) : null;
    case "openai_compatible":
    case "xai":
      return null;
    default:
      return false;
  }
}

/**
 * What OpenAI documents about this model taking asynchronous tool calls: GPT-6
 * Astra and every later model do, GPT generations before 6 and the o-series
 * do not, and GPT-6 Sol and Luna — older than Astra, named by nothing — are
 * the user's to say. Mirrors Rust `async_tools::openai_documents`.
 */
function openaiDocumentsAsyncTools(modelId: string): boolean | null {
  const version = openaiVersion(modelId);
  if (version === null || version === "before6") return version === null ? null : false;
  if (version.major === 6 && version.minor === 0) {
    return version.variant === "-astra" || version.variant.startsWith("-astra-") ? true : null;
  }
  return true;
}

/**
 * Whether the ChatGPT Codex backend takes asynchronous calls from this model,
 * as measured on 2026-10-05: every GPT-6 model does, every GPT-5 model refuses
 * them. Mirrors Rust `async_tools::codex_backend_takes`.
 */
function codexBackendTakesAsyncTools(modelId: string): boolean | null {
  const version = openaiVersion(modelId);
  return version === null ? null : version !== "before6";
}

/** The OpenAI model generation an id names; mirrors Rust `async_tools::openai_version`. */
function openaiVersion(modelId: string): "before6" | { major: number; minor: number; variant: string } | null {
  const id = modelId.trim().toLowerCase();
  if (["o1", "o3", "o4", "chatgpt-", "codex-"].some((prefix) => id.startsWith(prefix))) return "before6";
  const match = /^gpt-(\d+)(?:\.(\d+))?(.*)$/u.exec(id);
  if (!match) return null;
  const major = Number(match[1]);
  if (major <= 5) return "before6";
  return { major, minor: match[2] === undefined ? 0 : Number(match[2]), variant: match[3] };
}

/**
 * What Mewrk knows about this model taking asynchronous tool calls at this
 * endpoint; `null` is the user's to say (a relay, Azure's deployment names).
 * Only the Responses protocol has them; on the ChatGPT Codex backend every
 * GPT-6 model takes them. Mirrors Rust `async_tools::known`.
 */
export function knownAsyncTools(family: ProviderFamily, baseUrl: string, modelId: string): boolean | null {
  switch (family) {
    case "openai_responses":
      return atVendorEndpoint(family, baseUrl) ? openaiDocumentsAsyncTools(modelId) : null;
    case "openai_codex":
      return codexBackendTakesAsyncTools(modelId);
    case "azure":
      return null;
    default:
      return false;
  }
}

/**
 * What OpenAI documents about this model compacting natively: every GPT from 5
 * on does; anything else is the user's to say. Mirrors Rust
 * `native_compaction::openai_documents`.
 */
function openaiDocumentsNativeCompaction(modelId: string): boolean | null {
  const match = /^gpt-(\d+)/u.exec(modelId.trim().toLowerCase());
  return match && Number(match[1]) >= 5 ? true : null;
}

/**
 * What Mewrk knows about this model compacting natively at this endpoint;
 * `null` is the user's to say (a relay, Azure's deployment names). Only the
 * Responses protocol has the interface; Codex compacts this way with every
 * model the ChatGPT backend serves. Mirrors Rust `native_compaction::known`.
 */
export function knownNativeCompaction(family: ProviderFamily, baseUrl: string, modelId: string): boolean | null {
  switch (family) {
    case "openai_responses":
      return atVendorEndpoint(family, baseUrl) ? openaiDocumentsNativeCompaction(modelId) : null;
    case "openai_codex":
      return true;
    case "azure":
      return null;
    default:
      return false;
  }
}

/**
 * The protocol capabilities Mewrk declares for a model it fills in itself:
 * those it knows the model has at this endpoint. Mirrors Rust
 * `model_discovery::known_protocol_capabilities`.
 */
export function knownProtocolCapabilities(
  provider: { family: ProviderFamily; baseUrl: string },
  modelId: string
): ModelCapability[] {
  return [
    ...(knownToolAppend(provider.family, provider.baseUrl, modelId) === true ? ["tool_append" as const] : []),
    ...(knownSystemAppend(provider.family, provider.baseUrl, modelId) === true ? ["system_append" as const] : []),
    ...(knownAsyncTools(provider.family, provider.baseUrl, modelId) === true ? ["async_tools" as const] : []),
    ...(knownNativeCompaction(provider.family, provider.baseUrl, modelId) === true
      ? ["native_compaction" as const]
      : []),
  ];
}

/**
 * Whether this family has a tool-append interface, so the `tool_append`
 * capability is read. Mirrors Rust `ProviderFamily::tool_append_takes_effect`.
 */
export function toolAppendTakesEffect(family: ProviderFamily): boolean {
  return family === "anthropic"
    || family === "openai_responses"
    || family === "openai_codex"
    || family === "azure"
    || family === "claude_agent";
}

/**
 * Whether this family can carry a system message mid-conversation, so the
 * `system_append` capability is read. Mirrors Rust
 * `ProviderFamily::system_append_takes_effect`.
 */
export function systemAppendTakesEffect(family: ProviderFamily): boolean {
  return family === "anthropic"
    || family === "openai_responses"
    || family === "openai_codex"
    || family === "azure"
    || family === "openai_chat"
    || family === "openai_compatible"
    || family === "xai";
}

/**
 * Whether this family can mark a function tool asynchronous, so the
 * `async_tools` capability is read. Mirrors Rust
 * `ProviderFamily::async_tools_take_effect`.
 */
export function asyncToolsTakeEffect(family: ProviderFamily): boolean {
  return family === "openai_responses" || family === "openai_codex" || family === "azure";
}

/**
 * Whether this family can compact a conversation's context natively, so the
 * `native_compaction` capability is read. Mirrors Rust
 * `ProviderFamily::native_compaction_takes_effect`.
 */
export function nativeCompactionTakesEffect(family: ProviderFamily): boolean {
  return family === "openai_responses" || family === "openai_codex" || family === "azure";
}

/**
 * Whether a conversation on this model compacts natively: the model declares
 * `native_compaction` and its protocol has the interface. Mirrors Rust
 * `native_compaction::takes_native_compaction`.
 */
export function takesNativeCompaction(
  provider: { family: ProviderFamily },
  model: { capabilities?: readonly ModelCapability[] }
): boolean {
  return nativeCompactionTakesEffect(provider.family)
    && (model.capabilities ?? []).includes("native_compaction");
}

/**
 * Whether a conversation on this model can take a tool mid-conversation at all:
 * the model declares `tool_append` — filled in by Mewrk where it knows, by the
 * user everywhere else — and its protocol has an append interface. Where it
 * cannot, an added tool is folded back into the declared list, so while the
 * cache is warm the settings draw the whole surface orange, either way — and
 * auto-compact and MCP tool discovery, which both add tools mid-run, are
 * unavailable. Mirrors Rust `tool_append::appends_tools`.
 */
export function appendsTools(
  provider: { family: ProviderFamily },
  model: { capabilities?: readonly ModelCapability[] }
): boolean {
  return toolAppendTakesEffect(provider.family) && (model.capabilities ?? []).includes("tool_append");
}

/**
 * Whether a reasoning card is encrypted and therefore removable but not editable.
 * `form` is authoritative. Its absence falls back to an empty body for legacy cards;
 * all reads must use this function to keep the fallback consistent. Streaming cards
 * are already non-editable, and settlement writes the definitive `form`.
 */
export function isEncryptedReasoning(item: Pick<ReasoningContext, "form" | "content">): boolean {
  return item.form === undefined ? (item.content ?? "").length === 0 : item.form === "encrypted";
}

/** Display name, falling back to the ID. Mirrors Rust `ModelProfile::display_name`. */
export function modelDisplayName(model: ModelProfile): string {
  return model.name.trim() || model.id;
}

/**
 * Repairs `activeModelId` to a model that still exists. Presence in the provider's
 * list is the whole of usability now, but a removed model can still leave the id
 * dangling, which storage validation rejects on save.
 */
export function repairActiveModelId(provider: {
  models: ModelProfile[];
  activeModelId: string | null;
}): string | null {
  const current = provider.models.find((model) => model.id === provider.activeModelId);
  if (current) return current.id;
  return provider.models[0]?.id ?? null;
}

/**
 * Derives a model group when `group` is empty. This must match Rust
 * `derive_model_group_name`: slash-separated IDs use their first segment, while flat
 * IDs use their family prefix. Matching rules keep discovered and legacy models grouped
 * consistently.
 */
export function modelGroup(model: ModelProfile): string {
  const explicit = model.group.trim();
  if (explicit) return explicit;
  const id = model.id.trim();
  if (!id) return "";
  if (id.includes("/")) return id.slice(0, id.indexOf("/")).trim();
  const family = id.slice(0, id.indexOf("-") === -1 ? id.length : id.indexOf("-")).trim();
  return family === id ? "" : family;
}
