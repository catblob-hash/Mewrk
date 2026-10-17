import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import { SEARCH_PROVIDERS } from "../lib/searchProviders";
import type { WebSearchAssets } from "../types";

const searchMocks = vi.hoisted(() => ({
  deleteSearchApiKey: vi.fn(),
  getSearchKeyStatus: vi.fn(),
  revealSearchApiKey: vi.fn(),
  saveSearchApiKey: vi.fn()
}));
vi.mock("../lib/webSearch", () => searchMocks);
import { WebSearchSettings } from "./WebSearchSettings";

afterEach(() => configureI18n("zh-CN"));

const assets = (): WebSearchAssets => ({
  providers: SEARCH_PROVIDERS.map((provider) => ({
    kind: provider.kind,
    enabled: false,
    searchApiHost: "",
    fetchApiHost: "",
    engines: [],
    basicAuthUsername: ""
  }))
});

function renderSettings(initial = assets()) {
  let current = initial;
  const onFlush = vi.fn(() => Promise.resolve());
  function Harness() {
    const [settings, setSettings] = useState(initial);
    current = settings;
    return <WebSearchSettings settings={settings} onFlush={onFlush} onChange={(change) => setSettings((value) => typeof change === "function" ? change(value) : change)} />;
  }
  return { ...render(<Harness />), getSettings: () => current, onFlush };
}

/** Every rail row is a provider, so a row is selected by its catalog label. */
async function selectProvider(user: ReturnType<typeof userEvent.setup>, label: string) {
  await user.click(screen.getByRole("button", { name: label }));
}

describe("WebSearchSettings", () => {
  beforeEach(() => {
    configureI18n("en-US");
    searchMocks.deleteSearchApiKey.mockReset().mockResolvedValue({ configured: false });
    searchMocks.getSearchKeyStatus.mockReset().mockResolvedValue({ configured: false });
    searchMocks.revealSearchApiKey.mockReset().mockRejectedValue(new Error("not configured"));
    searchMocks.saveSearchApiKey.mockReset().mockImplementation((_kind: string, _slot: string, secret: string) => Promise.resolve({ configured: true, keyLength: secret.length }));
  });

  it("lists the fixed Cherry-Studio catalog and nothing else", () => {
    const { container } = renderSettings();
    // The page uses the model-provider rail layout without a title card.
    expect(container.querySelector(".settings-rail-page > .provider-rail")).toBeInTheDocument();
    expect(container.querySelector(".provider-pane__body .provider-pane__stack")).toBeInTheDocument();
    expect(container.querySelector("h3")).toBeNull();
    for (const provider of SEARCH_PROVIDERS) {
      expect(screen.getByRole("button", { name: provider.label })).toBeInTheDocument();
    }
    // The fixed catalog keeps the Add provider position disabled.
    expect(screen.getByRole("button", { name: "Add provider" })).toBeDisabled();
  });

  /* How a search behaves is asked of the conversation that runs it, because it
     is that conversation's context window being spent. Nothing on this page
     answers for every conversation at once, so there is no row here that
     belongs to no provider. */
  it("asks nothing here about how a search behaves", () => {
    renderSettings();
    expect(screen.queryByRole("button", { name: "General" })).not.toBeInTheDocument();
    for (const label of ["Result count", "Result compression", "Fetch provider", "Blocked domains"]) {
      expect(screen.queryByLabelText(label)).not.toBeInTheDocument();
    }
    // The page opens on the first catalog provider rather than on an empty pane.
    expect(
      screen.getByRole("heading", { level: 1, name: SEARCH_PROVIDERS[0].label })
    ).toBeInTheDocument();
  });

  // The enable switch exists only in the selected pane header; rail dots are status indicators.
  it("keeps the enable switch to the selected provider's pane header", async () => {
    const user = userEvent.setup();
    renderSettings();
    await selectProvider(user, "Tavily");
    expect(screen.getByRole("heading", { level: 1, name: "Tavily" })).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Enable search provider Tavily" })).toBeInTheDocument();
    for (const provider of SEARCH_PROVIDERS.filter((entry) => entry.kind !== "tavily")) {
      expect(screen.queryByRole("switch", { name: `Enable search provider ${provider.label}` })).toBeNull();
    }
  });

  it("updates a provider configuration and enablement", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderSettings();
    await selectProvider(user, "Tavily");
    await user.click(screen.getByRole("switch", { name: "Enable search provider Tavily" }));
    await user.type(screen.getByLabelText("Search endpoint"), "https://proxy.example/tavily");
    expect(getSettings().providers.find((provider) => provider.kind === "tavily")).toEqual({
      kind: "tavily",
      enabled: true,
      searchApiHost: "https://proxy.example/tavily",
      fetchApiHost: "",
      engines: [],
      basicAuthUsername: ""
    });
  });

  // Jina has separate search and fetch hosts; sharing a base URL routes fetches incorrectly.
  it("gives each capability its own endpoint field", async () => {
    const user = userEvent.setup();
    renderSettings();
    await selectProvider(user, "Jina");
    expect(screen.getByLabelText("Search endpoint")).toHaveAttribute("placeholder", "https://s.jina.ai");
    expect(screen.getByLabelText("Fetch endpoint")).toHaveAttribute("placeholder", "https://r.jina.ai");

    await selectProvider(user, "Tavily");
    expect(screen.queryByLabelText("Fetch endpoint")).toBeNull();
  });

  // The local fetch provider has neither endpoint nor credentials.
  it("shows no endpoint or credential for the local fetch provider", async () => {
    const user = userEvent.setup();
    renderSettings();
    await selectProvider(user, "fetch");
    expect(screen.queryByLabelText("Search endpoint")).toBeNull();
    expect(screen.queryByLabelText("Fetch endpoint")).toBeNull();
    expect(screen.queryByLabelText("API Key")).toBeNull();
    // The limits on what it may reach are on the site, one link away.
    expect(screen.getByText(/The app reads pages itself; nothing to configure/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Learn more/ }))
      .toHaveAttribute("href", "https://mewrk.dev/en/web.html#local-fetch");
  });

  /* Jina searches only with a key but reads pages without one, so the field is
     not marked required and the help says which capability needs it. */
  it("marks the key required only where every capability needs it", async () => {
    const user = userEvent.setup();
    renderSettings();
    await selectProvider(user, "Jina");
    expect(screen.queryByText("API Key (required)")).toBeNull();
    expect(screen.getByText(/Only search needs an API key/)).toBeInTheDocument();

    await selectProvider(user, "Tavily");
    expect(screen.getByText("API Key (required)")).toBeInTheDocument();
    // The required mark already says it, so the help has nothing to add about the key itself.
    expect(screen.queryByText(/needs an API key|is optional/)).toBeNull();
    expect(screen.getByText(/^Browser preview sends no requests/)).toBeInTheDocument();
  });

  // The validator lets any local or private-network address use http, not only loopback;
  // the line names https and the site spells the exception out.
  it("states the endpoint rule in one line and links the rest", async () => {
    const user = userEvent.setup();
    renderSettings();
    await selectProvider(user, "Jina");
    expect(screen.getAllByText(/Leave empty for the default; must be https/)).toHaveLength(2);
    for (const link of screen.getAllByRole("link", { name: /Learn more/ })) {
      expect(link).toHaveAttribute("href", "https://mewrk.dev/en/web.html#provider-settings");
    }
    expect(screen.queryByText(/Only loopback/)).toBeNull();
  });

  // SearXNG uses an engine list and Basic Auth credentials instead of an API key.
  it("swaps the api key for engines and basic auth on searxng", async () => {
    const user = userEvent.setup();
    const { getSettings } = renderSettings();
    await selectProvider(user, "Searxng");
    expect(screen.queryByLabelText("API Key")).toBeNull();
    expect(screen.getByLabelText("Basic auth password")).toBeInTheDocument();

    await user.type(screen.getByLabelText("Search engines"), "google, brave");
    await user.type(screen.getByLabelText("Basic auth username"), "searx");
    const searxng = getSettings().providers.find((provider) => provider.kind === "searxng");
    expect(searxng?.engines).toEqual(["google", "brave"]);
    expect(searxng?.basicAuthUsername).toBe("searx");

    // The password uses the credential store path with a distinct slot name.
    await user.type(screen.getByLabelText("Basic auth password"), "hunter2");
    await user.tab();
    await waitFor(() => expect(searchMocks.saveSearchApiKey).toHaveBeenCalledWith("searxng", "basicAuthPassword", "hunter2"));
  });

  it("flushes before saving and deletes an emptied provider key", async () => {
    const user = userEvent.setup();
    const { onFlush } = renderSettings();
    await selectProvider(user, "Tavily");
    const key = screen.getByLabelText("API Key");
    await user.type(key, "provider-secret");
    await user.tab();
    await waitFor(() => expect(searchMocks.saveSearchApiKey).toHaveBeenCalledWith("tavily", "apiKey", "provider-secret"));
    expect(onFlush.mock.invocationCallOrder[0]).toBeLessThan(searchMocks.saveSearchApiKey.mock.invocationCallOrder[0]);
    await user.clear(key);
    await user.tab();
    await waitFor(() => expect(searchMocks.deleteSearchApiKey).toHaveBeenCalledWith("tavily", "apiKey"));
  });

  it("loads key status for the provider the user selects", async () => {
    const user = userEvent.setup();
    searchMocks.getSearchKeyStatus.mockImplementation((kind: string) => Promise.resolve(
      kind === "exa" ? { configured: true, keyLength: 9 } : { configured: false }
    ));
    renderSettings();
    await selectProvider(user, "Exa");
    await waitFor(() => expect(searchMocks.getSearchKeyStatus).toHaveBeenCalledWith("exa", "apiKey"));
    await waitFor(() => expect(screen.getByLabelText("API Key")).toHaveValue("•".repeat(9)));
  });

  // A stored secret must stay masked until the user asks for it by name. This is
  // the one assertion standing between "masked field" and "plaintext key sitting
  // in the DOM of a settings page nobody opened deliberately".
  it("reveals the stored key only when explicitly asked", async () => {
    const user = userEvent.setup();
    searchMocks.getSearchKeyStatus.mockResolvedValue({ configured: true, keyLength: 12 });
    searchMocks.revealSearchApiKey.mockResolvedValue("stored-provider-key");
    renderSettings();
    await selectProvider(user, "Tavily");

    const keyInput = screen.getByLabelText("API Key");
    await waitFor(() => expect(keyInput).toHaveValue("•".repeat(12)));
    expect(searchMocks.revealSearchApiKey).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Show API Key" }));
    await waitFor(() => expect(keyInput).toHaveValue("stored-provider-key"));
    expect(keyInput).toHaveAttribute("type", "text");

    await user.click(screen.getByRole("button", { name: "Hide API Key" }));
    await waitFor(() => expect(keyInput).toHaveAttribute("type", "password"));
    expect(keyInput).toHaveValue("•".repeat("stored-provider-key".length));
  });

  // An unconfigured provider must open an empty editable field without ever
  // calling reveal — the old code decided this by regex-matching the backend's
  // Chinese error text, which broke the moment that copy was reworded.
  it("opens an empty field for an unconfigured provider without asking to reveal", async () => {
    const user = userEvent.setup();
    searchMocks.getSearchKeyStatus.mockResolvedValue({ configured: false });
    renderSettings();
    await selectProvider(user, "Tavily");
    await user.click(screen.getByRole("button", { name: "Show API Key" }));
    await waitFor(() => expect(screen.getByLabelText("API Key")).toHaveValue(""));
    expect(searchMocks.revealSearchApiKey).not.toHaveBeenCalled();
  });
});
