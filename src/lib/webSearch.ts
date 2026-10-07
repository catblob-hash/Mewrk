import type {
  ApiKeyStatus,
  ContextItem,
  ConversationWebSearchSettings,
  ProviderFamily,
  SearchCapability,
  SearchProviderKind,
  WebSearchAssets
} from "../types";
import { hasBackendRuntime, invoke } from "./backend";
import { searchProviderSupports } from "./searchProviders";

/**
 * Whether a provider family has a server-side page-fetch tool whose result the
 * host can read as text.
 *
 * Mirrors Rust `web_search::family_supports_native_fetch`. Only Anthropic both
 * exposes fetching as its own server tool and returns the page as plain text;
 * the Responses API has no page-fetch tool at all, and neither do Google or
 * xAI. Search is deliberately not symmetrical with this — no family's *search*
 * results carry readable page text — so this answers only "can this
 * conversation fetch a page without borrowing a provider".
 */
export function familySupportsNativeFetch(family: ProviderFamily | undefined): boolean {
  return family === "anthropic" || family === "bedrock";
}

/**
 * Whether this family spells its native web tools the Messages way, with the
 * version written into the tool's own `type`.
 *
 * Mirrors Rust `web_search::family_selects_native_tool_type`. Every family that
 * has native web tools at all has exactly one shape for them; only Messages
 * makes the version part of the wire, so only there is there anything to pick.
 * A conversation on any other family keeps carrying whichever version it chose
 * and simply does not send it — the selection is never rewritten, so moving
 * back to a Messages model moves back to that same version.
 */
export function familySelectsNativeToolType(family: ProviderFamily | undefined): boolean {
  return family === "anthropic" || family === "bedrock";
}

/**
 * Whether these settings grant `web_fetch` at all.
 *
 * Mirrors Rust `WebSearchSettings::fetch_withheld`, the fetch half of the one
 * rule `web_search::apply_web_tools` applies to both legs: offered unless the
 * Fetch provider is Off, or Native on a family that reads pages inside its
 * search tool. Whether the chosen provider is switched on, known or keyed is
 * not part of it — a broken one keeps the tool and fails each call with a fix —
 * so nothing in global settings can change what a conversation was offered.
 *
 * The renderer needs the answer because the tool lock records which web tools
 * a run put in front of the model. The search selection is not consulted:
 * every fetch selection names its own backend.
 */
export function grantsWebFetch(
  webSearchEnabled: boolean,
  conversation: ConversationWebSearchSettings,
  family: ProviderFamily | undefined
): boolean {
  if (!webSearchEnabled) return false;
  switch (conversation.fetchProvider.kind) {
    case "disabled":
      return false;
    case "native":
      return familySupportsNativeFetch(family);
    case "explicit":
    case "unavailable":
      return true;
  }
}

/**
 * Whether the transcript holds a report a native search produced: a settled,
 * successful `web_search` whose output is the native envelope `{findings,
 * sources}` (Rust `web_search_output`) rather than a catalog provider's
 * `{results}`. Only a model whose family has native search can produce one —
 * on every other family the call fails before it is sent — so this is what
 * "Native has actually searched here" means to the tool lock.
 */
export function nativeSearchRan(contexts: readonly ContextItem[]): boolean {
  return contexts.some((item) => {
    if (item.kind !== "tool" || item.toolName !== "web_search" || item.streaming || !item.result.success) {
      return false;
    }
    try {
      const envelope: unknown = JSON.parse(item.result.output);
      return typeof envelope === "object" && envelope !== null
        && typeof (envelope as { findings?: unknown }).findings === "string";
    } catch {
      return false;
    }
  });
}

/**
 * Why a selected catalog provider cannot serve `capability` at all, or `null`
 * when it can be picked: not in the catalog (an `unavailable` selection),
 * unable to do this, or switched off in settings — none of which the menu
 * lists, so the pickers read "Repair …" instead of a name.
 *
 * A missing API key is deliberately not one of these. The provider stays a
 * plain choice, and the call itself fails naming the setting to change: the
 * key is a fact about the credential store at call time, not about the
 * selection.
 */
export function selectedProviderProblem(
  selection: { kind: string; providerKind?: SearchProviderKind } | null,
  assets: WebSearchAssets,
  capability: SearchCapability
): "unavailable" | "disabled" | null {
  if (selection?.kind === "unavailable") return "unavailable";
  if (selection?.kind !== "explicit" || !selection.providerKind) return null;
  const kind = selection.providerKind;
  if (!searchProviderSupports(kind, capability)) return "unavailable";
  if (!assets.providers.some((provider) => provider.kind === kind && provider.enabled)) return "disabled";
  return null;
}

/**
 * Secrets a search provider may hold, mirroring Rust's `web_search::CredentialSlot`.
 * `basicAuthPassword` belongs only to SearXNG, which has no API key and may sit
 * behind Basic Auth on self-hosted instances.
 */
export type SearchCredentialSlot = "apiKey" | "basicAuthPassword";

const SEARCH_KEY_LENGTH_PREFIX = "mewrk.search-api-key-length.v2.";
const SEARCH_KEY_PREVIEW_PREFIX = "mewrk.search-preview-key-configured.v2.";

let secretMutationTail: Promise<void> = Promise.resolve();

function queueSecretMutation<T>(operation: () => Promise<T>): Promise<T> {
  const result = secretMutationTail.catch(() => undefined).then(operation);
  secretMutationTail = result.then(() => undefined, () => undefined);
  return result;
}

async function credentialFingerprint(providerKind: string, slot: SearchCredentialSlot): Promise<string> {
  if (!globalThis.crypto?.subtle) throw new Error("当前环境不支持安全的搜索凭据指纹");
  const digest = await globalThis.crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`mewrk-web-search-v2\0${providerKind}\0${slot}`)
  );
  return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function rememberKeyLength(
  providerKind: string,
  slot: SearchCredentialSlot,
  keyLength?: number
): Promise<void> {
  const fingerprint = await credentialFingerprint(providerKind, slot);
  const storageKey = `${SEARCH_KEY_LENGTH_PREFIX}${fingerprint}`;
  if (Number.isInteger(keyLength) && (keyLength ?? 0) > 0) {
    window.localStorage.setItem(storageKey, String(keyLength));
  } else {
    window.localStorage.removeItem(storageKey);
  }
}

async function storedKeyLength(providerKind: string, slot: SearchCredentialSlot): Promise<number | undefined> {
  const fingerprint = await credentialFingerprint(providerKind, slot);
  const value = Number(window.localStorage.getItem(`${SEARCH_KEY_LENGTH_PREFIX}${fingerprint}`));
  return Number.isSafeInteger(value) && value > 0 ? value : undefined;
}

async function browserKeyStatus(providerKind: string, slot: SearchCredentialSlot): Promise<ApiKeyStatus> {
  const fingerprint = await credentialFingerprint(providerKind, slot);
  const configured = window.sessionStorage.getItem(`${SEARCH_KEY_PREVIEW_PREFIX}${fingerprint}`) === "true";
  return {
    configured,
    keyLength: configured ? await storedKeyLength(providerKind, slot) : undefined
  };
}

/**
 * Credential commands send only the fixed catalog id and a known slot name. The
 * Rust side validates both against its own catalog instead of trusting renderer
 * input — and the endpoint is deliberately **not** part of the identity: a
 * credential is bound to the provider row, so editing an API host never orphans
 * the secret behind it.
 */
export async function getSearchKeyStatus(
  providerKind: string,
  slot: SearchCredentialSlot = "apiKey"
): Promise<ApiKeyStatus> {
  await secretMutationTail;
  if (!hasBackendRuntime()) return browserKeyStatus(providerKind, slot);
  const status = await invoke<ApiKeyStatus>("get_search_key_status", { providerKind, slot });
  if (status.configured) {
    const keyLength = status.keyLength ?? await storedKeyLength(providerKind, slot);
    if (keyLength) await rememberKeyLength(providerKind, slot, keyLength);
    return { ...status, keyLength };
  }
  await rememberKeyLength(providerKind, slot);
  return { configured: false };
}

export async function saveSearchApiKey(
  providerKind: string,
  slot: SearchCredentialSlot,
  apiKey: string
): Promise<ApiKeyStatus> {
  return queueSecretMutation(async () => {
    const secret = apiKey.trim();
    if (!secret) throw new Error("凭据不能为空");
    const keyLength = Array.from(secret).length;
    if (hasBackendRuntime()) {
      const status = await invoke<ApiKeyStatus | boolean | null>("save_search_api_key", {
        providerKind,
        slot,
        apiKey: secret
      });
      if (status === false) throw new Error("凭据未保存");
      await rememberKeyLength(providerKind, slot, keyLength);
      return typeof status === "object" && status && typeof status.configured === "boolean"
        ? { ...status, keyLength: status.keyLength ?? keyLength }
        : { configured: true, keyLength };
    }
    const fingerprint = await credentialFingerprint(providerKind, slot);
    window.sessionStorage.setItem(`${SEARCH_KEY_PREVIEW_PREFIX}${fingerprint}`, "true");
    await rememberKeyLength(providerKind, slot, keyLength);
    return { configured: true, keyLength };
  });
}

export async function revealSearchApiKey(
  providerKind: string,
  slot: SearchCredentialSlot = "apiKey"
): Promise<string> {
  await secretMutationTail;
  if (!hasBackendRuntime()) throw new Error("浏览器预览不会保留凭据明文");
  const secret = await invoke<string>("reveal_search_api_key", { providerKind, slot });
  await rememberKeyLength(providerKind, slot, Array.from(secret).length);
  return secret;
}

export async function deleteSearchApiKey(
  providerKind: string,
  slot: SearchCredentialSlot = "apiKey"
): Promise<ApiKeyStatus> {
  return queueSecretMutation(async () => {
    let status: ApiKeyStatus = { configured: false };
    if (hasBackendRuntime()) {
      status = await invoke<ApiKeyStatus>("delete_search_api_key", { providerKind, slot });
    } else {
      const fingerprint = await credentialFingerprint(providerKind, slot);
      window.sessionStorage.removeItem(`${SEARCH_KEY_PREVIEW_PREFIX}${fingerprint}`);
    }
    await rememberKeyLength(providerKind, slot);
    return { ...status, configured: false, keyLength: undefined };
  });
}
