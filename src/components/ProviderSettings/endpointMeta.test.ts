import { describe, expect, it } from "vitest";
import type { TranslationFunction } from "../../i18n";
import { apiAddressDerivation, chatRequestPreview } from "./endpointMeta";

describe("chatRequestPreview", () => {
  it("keeps a relay path segment the host also keeps", () => {
    // `provider` and `openai` are mount points a relay routes on, not endpoint
    // suffixes, so neither the host nor the preview may drop them.
    expect(chatRequestPreview("https://relay.example.com/tenant/provider", "openai_chat"))
      .toBe("https://relay.example.com/tenant/provider/chat/completions");
    expect(chatRequestPreview("https://gateway.example.com/v1/acct/gw/openai", "openai_chat"))
      .toBe("https://gateway.example.com/v1/acct/gw/openai/chat/completions");
  });

  it("strips a pasted endpoint suffix the way the host does", () => {
    // Showing `.../v1/chat/completions/chat/completions` for a Base URL that
    // actually works pushes users to break a valid configuration.
    expect(chatRequestPreview("https://relay.example.com/v1/chat/completions", "openai_chat"))
      .toBe("https://relay.example.com/v1/chat/completions");
    expect(chatRequestPreview("https://relay.example.com/v1/messages", "anthropic"))
      .toBe("https://relay.example.com/v1/messages");
    expect(chatRequestPreview("https://relay.example.com/v1/responses", "openai_responses"))
      .toBe("https://relay.example.com/v1/responses");
  });

  it("tolerates trailing slashes and an empty Base URL", () => {
    expect(chatRequestPreview("https://relay.example.com/v1//", "openai_chat"))
      .toBe("https://relay.example.com/v1/chat/completions");
    expect(chatRequestPreview("   ", "openai_chat")).toBe("");
  });

  it("appends no suffix for families whose path depends on the model or deployment", () => {
    expect(chatRequestPreview("https://relay.example.com/v1", "google"))
      .toBe("https://relay.example.com/v1");
  });
});

describe("apiAddressDerivation", () => {
  // Stands in for the zh-CN / en-US switch: it returns the Chinese line.
  const zh = ((chinese: string) => chinese) as TranslationFunction;

  it("tells Bedrock and Vertex a blank address is normal, in one short line", () => {
    expect(apiAddressDerivation(zh, "bedrock")).toBe("留空即可，由 AWS 区域推算。");
    expect(apiAddressDerivation(zh, "vertex")).toBe("留空即可，由 GCP 项目和区域推算。");
  });

  it("says nothing for a protocol whose blank address is simply missing", () => {
    expect(apiAddressDerivation(zh, "openai_chat")).toBeNull();
  });
});
