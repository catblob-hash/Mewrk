import type { TranslationFunction } from "../../i18n";
import { chatEndpointOf } from "../../lib/modelCapabilities";
import { CODEX_DEFAULT_BASE_URL } from "../../lib/codexProvider";
import type { FamilySetting, ProviderFamily } from "../../types";

/**
 * Options for the provider-family picker. Labels are protocol or vendor brands
 * and are not localized. `defaultBaseUrl` replaces an unmodified URL when the
 * family changes; an empty string means the provider has no fixed public URL.
 */
export const API_FORMAT_OPTIONS: ReadonlyArray<{ value: ProviderFamily; label: string; defaultBaseUrl: string }> = [
  { value: "openai_responses", label: "OpenAI Responses", defaultBaseUrl: "https://api.openai.com/v1" },
  { value: "openai_chat", label: "OpenAI Chat Completions", defaultBaseUrl: "https://api.openai.com/v1" },
  { value: "anthropic", label: "Anthropic Messages", defaultBaseUrl: "https://api.anthropic.com/v1" },
  { value: "google", label: "Google Gemini", defaultBaseUrl: "https://generativelanguage.googleapis.com/v1beta" },
  { value: "xai", label: "xAI", defaultBaseUrl: "https://api.x.ai/v1" },
  { value: "azure", label: "Azure OpenAI", defaultBaseUrl: "" },
  { value: "bedrock", label: "AWS Bedrock", defaultBaseUrl: "" },
  { value: "vertex", label: "Google Vertex AI", defaultBaseUrl: "" },
  { value: "openai_compatible", label: "OpenAI Compatible", defaultBaseUrl: "" }
];

/** The built-in OAuth provider is fixed rather than selectable in the add dialog. */
const CODEX_FAMILY_OPTION = {
  value: "openai_codex" as const,
  label: "OpenAI Codex (ChatGPT)",
  defaultBaseUrl: CODEX_DEFAULT_BASE_URL,
};

/** The built-in local-CLI provider is fixed rather than selectable in the add dialog. */
const CLAUDE_AGENT_FAMILY_OPTION = {
  value: "claude_agent" as const,
  label: "Claude Agent (Claude Code)",
  defaultBaseUrl: "",
};

/** The fixed option for a built-in family, or undefined for user-selectable families. */
export function builtinFamilyOption(family: ProviderFamily) {
  if (family === CODEX_FAMILY_OPTION.value) return CODEX_FAMILY_OPTION;
  if (family === CLAUDE_AGENT_FAMILY_OPTION.value) return CLAUDE_AGENT_FAMILY_OPTION;
  return undefined;
}

export function familyLabel(family: ProviderFamily): string {
  const builtin = builtinFamilyOption(family);
  if (builtin) return builtin.label;
  return API_FORMAT_OPTIONS.find((option) => option.value === family)?.label ?? family;
}

/**
 * The example a protocol's API address field shows while it is empty: the
 * default public address where the protocol has one, otherwise the shape an
 * address of that kind takes. Bedrock and Vertex have none to show, because a
 * blank address is their normal setting (`apiAddressDerivation`).
 *
 * Azure's example stops at `/openai`: for an `*.openai.azure.com` address the
 * AI SDK appends `/v1/responses?api-version=…` itself, so an address ending in
 * `/v1` would send requests to `/v1/v1/responses`.
 */
export function apiAddressExample(family: ProviderFamily): string {
  switch (family) {
    case "azure": return "https://<resource>.openai.azure.com/openai";
    case "openai_compatible": return "http://localhost:11434/v1";
    case "bedrock":
    case "vertex":
      return "";
    default:
      return builtinFamilyOption(family)?.defaultBaseUrl
        ?? API_FORMAT_OPTIONS.find((option) => option.value === family)?.defaultBaseUrl
        ?? "";
  }
}

/**
 * What Mewrk derives the endpoint from when a Bedrock or Vertex address is
 * left blank, which is how both are normally set up; `null` for every protocol
 * whose blank address is simply missing.
 */
export function apiAddressDerivation(t: TranslationFunction, family: ProviderFamily): string | null {
  switch (family) {
    case "bedrock":
      return t("留空即可：Mewrk 会根据 AWS 区域推算端点。", "Leave it blank: Mewrk derives the endpoint from the AWS region.");
    case "vertex":
      return t("留空即可：Mewrk 会根据 GCP 项目和区域推算端点。", "Leave it blank: Mewrk derives the endpoint from the GCP project and location.");
    default:
      return null;
  }
}

/** Path segment that the host POSTs for this protocol. */
function chatRequestPath(family: ProviderFamily): string {
  switch (chatEndpointOf(family)) {
    case "openai_responses": return "/responses";
    case "openai_chat_completions": return "/chat/completions";
    case "anthropic_messages": return "/messages";
    // Google, Azure, and Bedrock do not append a fixed suffix: their paths or
    // hosts depend on the model, deployment, API version, or region. Do not
    // display a guessed URL that could make users change a valid configuration.
    default: return "";
  }
}

/**
 * Endpoint suffixes the host removes from a pasted Base URL. Mirrors Rust
 * `strip_known_endpoint`; the paired forms are matched before the single ones.
 */
const PASTED_ENDPOINT_SUFFIXES: readonly (readonly string[])[] = [
  ["chat", "completions"],
  ["responses", "compact"],
  ["models"],
  ["responses"],
  ["messages"],
  ["completions"]
];

/** Base URL with a pasted endpoint suffix removed, matching host normalization. */
function withoutPastedEndpoint(baseUrl: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/u, "");
  if (!trimmed) return "";
  const segments = trimmed.split("/");
  for (const suffix of PASTED_ENDPOINT_SUFFIXES) {
    if (segments.length <= suffix.length) continue;
    const tail = segments.slice(-suffix.length).map((segment) => segment.toLowerCase());
    if (tail.every((segment, index) => segment === suffix[index])) {
      return segments.slice(0, -suffix.length).join("/");
    }
  }
  return trimmed;
}

/**
 * Full conversation request URL for a Base URL and protocol.
 *
 * The host strips a pasted endpoint suffix before the request goes out, so the
 * preview strips it too. Showing `.../v1/responses/responses` for a Base URL
 * that actually works would push users to "fix" a valid configuration.
 */
export function chatRequestPreview(baseUrl: string, family: ProviderFamily): string {
  const normalized = withoutPastedEndpoint(baseUrl);
  if (!normalized) return "";
  return `${normalized}${chatRequestPath(family)}`;
}

/** Labels and placeholders for family-specific identity fields. */
export function familySettingMeta(
  t: TranslationFunction,
  setting: FamilySetting
): { label: string; placeholder: string; hint: string } {
  switch (setting) {
    case "region":
      return {
        label: t("AWS 区域", "AWS region"),
        placeholder: "us-east-1",
        hint: t("Bedrock 的端点主机名由它决定。", "Bedrock derives its endpoint host from this."),
      };
    case "project":
      return {
        label: t("GCP 项目", "GCP project"),
        placeholder: "my-project-123456",
        hint: t("Vertex 的模型路径里带着它。", "Vertex puts this in the model path."),
      };
    case "location":
      return {
        label: t("GCP 区域", "GCP location"),
        placeholder: "us-central1",
        hint: t("Vertex 的端点主机名与模型路径都带着它。", "Vertex puts this in both the endpoint host and the model path."),
      };
    case "api_version":
      return {
        label: "api-version",
        placeholder: t("留空 = 用默认版本", "blank = provider default"),
        hint: t(
          "Azure 把它放在查询串里。留空时用 AI SDK 自带的默认版本——写一个没验证过的版本号比不写更糟。",
          "Azure passes this as a query parameter. Blank uses the AI SDK default; pinning an unverified version is worse than not pinning one."
        ),
      };
  }
}
