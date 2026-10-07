import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import { SEARCH_PROVIDERS } from "../lib/searchProviders";
import { defaultConversationWebSearchSettings } from "../lib/runtime";
import type { ConversationWebSearchSettings, SearchProviderKind, WebSearchAssets } from "../types";
import { WebSearchBehaviorSettings, type WebSearchBehaviorSettingsProps } from "./WebSearchBehaviorSettings";

afterEach(() => configureI18n("zh-CN"));

/** Jina searches and fetches; Tavily only searches; Exa is left switched off. */
const assets = (): WebSearchAssets => ({
  providers: SEARCH_PROVIDERS.map((provider) => ({
    kind: provider.kind,
    enabled: provider.kind === "jina" || provider.kind === "tavily",
    searchApiHost: "",
    fetchApiHost: "",
    engines: [],
    basicAuthUsername: ""
  }))
});

function renderBehavior(
  initial: Partial<ConversationWebSearchSettings> = {},
  extra: Partial<Pick<WebSearchBehaviorSettingsProps, "searchLock" | "fetchLock" | "nativeFetchAvailable" | "subject">> = {}
) {
  const onChange = vi.fn();
  const start = { ...defaultConversationWebSearchSettings(), ...initial };
  function Harness() {
    const [value, setValue] = useState(start);
    return (
      <div className="web-search-provider-field">
        <WebSearchBehaviorSettings
          {...extra}
          value={value}
          webSearchAssets={assets()}
          onChange={(patch) => {
            onChange(patch);
            setValue((current) => ({ ...current, ...patch }));
          }}
        />
      </div>
    );
  }
  render(<Harness />);
  return { onChange, lastPatch: () => onChange.mock.calls.at(-1)?.[0] };
}

const searchWith = (providerKind: SearchProviderKind): Partial<ConversationWebSearchSettings> => ({
  provider: { kind: "explicit", providerKind }
});
const fetchWith = (providerKind: SearchProviderKind): Partial<ConversationWebSearchSettings> => ({
  fetchProvider: { kind: "explicit", providerKind }
});

const countRow = () => screen.queryByRole("spinbutton", { name: "结果数" });
const searchCompressionRow = () => screen.queryByRole("spinbutton", { name: "搜索结果压缩" });
const fetchCompressionRow = () => screen.queryByRole("spinbutton", { name: "抓取结果压缩" });
/** The hint line of the field a spinbutton sits in. */
const hintOf = (input: HTMLElement) =>
  input.closest<HTMLElement>("label.field")!.querySelector<HTMLElement>(".field__hint")!.textContent ?? "";

async function openMenu(user: ReturnType<typeof userEvent.setup>, label: string) {
  await user.click(screen.getByRole("button", { name: new RegExp(`^${label}：`) }));
  return screen.getByRole("menu", { name: label });
}

describe("WebSearchBehaviorSettings", () => {
  beforeEach(() => {
    configureI18n("zh-CN");
  });

  /* A provider that is switched off is not a choice. Offering it greyed out
     would make the menu a list of things that do not work, and the global page
     is where a provider is switched on. */
  it("lists only enabled providers, and names the native row without a parenthetical", async () => {
    const user = userEvent.setup();
    renderBehavior();

    const search = await openMenu(user, "搜索提供商");
    expect(within(search).getByRole("menuitemradio", { name: "原生" })).toBeInTheDocument();
    expect(within(search).getByRole("menuitemradio", { name: "Tavily" })).toBeInTheDocument();
    expect(within(search).queryByRole("menuitemradio", { name: /Exa/ })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");

    const fetch = await openMenu(user, "抓取提供商");
    expect(within(fetch).getByRole("menuitemradio", { name: "原生" })).toBeInTheDocument();
    expect(within(fetch).getByRole("menuitemradio", { name: "Jina" })).toBeInTheDocument();
    // Tavily searches but cannot fetch, so it is not a fetch backend at all.
    expect(within(fetch).queryByRole("menuitemradio", { name: "Tavily" })).not.toBeInTheDocument();
    // Nothing resolves to a backend chosen elsewhere.
    expect(within(fetch).queryByRole("menuitemradio", { name: /自动/ })).not.toBeInTheDocument();
  });

  /* The standing advice on the native backends names whose model it is: the
     conversation's own, or — on a role's page — the model the role runs on.
     Saying "the conversation's model" to a role's editor would send its author
     looking for a setting this page does not have. */
  it.each([
    ["zh-CN" as const, "conversation" as const, /^原生用模型自带的搜索/, /^原生用模型自带的抓取/, /这个角色/],
    ["zh-CN" as const, "role" as const, /^原生用这个角色所跑模型自带的搜索/, /^原生用这个角色所跑模型自带的抓取/, /^原生用模型自带的/],
    ["en-US" as const, "conversation" as const, /^Native uses the model's own search/, /^Native uses the model's own fetch/, /this role/],
    ["en-US" as const, "role" as const, /^Native uses the built-in search of the model this role runs on/, /^Native uses the built-in fetch of the model this role runs on/, /^Native uses the model's own/]
  ])("says whose model the native backends are, in %s for a %s", (language, subject, searchAdvice, fetchAdvice, notThis) => {
    configureI18n(language);
    renderBehavior({}, { subject });

    expect(screen.getByText(searchAdvice)).toBeInTheDocument();
    expect(screen.getByText(fetchAdvice)).toBeInTheDocument();
    expect(screen.queryByText(notThis)).toBeNull();
  });

  it("speaks of the conversation unless it is told the settings are a role's", () => {
    renderBehavior();
    expect(screen.getByText(/^原生用模型自带的搜索/)).toBeInTheDocument();
  });

  /* A subagent role answers every row for itself, through this same component,
     so no row has a "follow the conversation" answer to offer: only the two
     backends, "不启用", and the three filter modes. */
  it("offers no row that follows the conversation's settings", async () => {
    const user = userEvent.setup();
    renderBehavior();

    const search = await openMenu(user, "搜索提供商");
    expect(within(search).queryByRole("menuitemradio", { name: /跟随对话设置/ })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");

    const fetch = await openMenu(user, "抓取提供商");
    expect(within(fetch).queryByRole("menuitemradio", { name: /跟随对话设置/ })).not.toBeInTheDocument();
    await user.keyboard("{Escape}");

    const filter = screen.getByRole("combobox", { name: "域名过滤" });
    expect(within(filter).getAllByRole("option").map((option) => option.textContent))
      .toEqual(["启用黑名单", "启用白名单", "不启用"]);
    expect(within(filter).queryByRole("option", { name: /跟随对话设置/ })).not.toBeInTheDocument();
  });

  it("turns either leg off on its own", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior();

    const search = await openMenu(user, "搜索提供商");
    await user.click(within(search).getByRole("menuitemradio", { name: "不启用" }));
    expect(lastPatch()).toEqual({ provider: { kind: "disabled" } });

    const fetch = await openMenu(user, "抓取提供商");
    await user.click(within(fetch).getByRole("menuitemradio", { name: "不启用" }));
    expect(lastPatch()).toEqual({ fetchProvider: { kind: "disabled" } });
  });

  /* Each number is the selected backend's own request parameter, so a backend
     that has no such parameter draws no row for it. A native search has neither
     on any family, and a native fetch only exists as its own tool on one. */
  it("draws no numbers for a native search, and none for a native fetch the model cannot carry", () => {
    renderBehavior();
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
    expect(fetchCompressionRow()).toBeNull();
    expect(document.querySelector(".web-search-leg-params")).toBeNull();
  });

  it("draws only the result count for Tavily, which has no content-length parameter", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(searchWith("tavily"));

    const count = countRow()!;
    expect(count).toHaveValue(5);
    expect(searchCompressionRow()).toBeNull();
    expect(hintOf(count)).toContain("max_results");
    await user.clear(count);
    await user.type(count, "12");
    expect(lastPatch()).toEqual({ maxResults: 12 });
  });

  it("draws both rows for Exa and takes 0 as a legal answer on the compression", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(searchWith("exa"));

    expect(countRow()).toHaveValue(5);
    expect(hintOf(countRow()!)).toContain("numResults");
    const compression = searchCompressionRow()!;
    expect(compression).toHaveValue(2000);
    expect(hintOf(compression)).toContain("contents.text.maxCharacters");
    await user.clear(compression);
    await user.type(compression, "0");
    expect(lastPatch()).toEqual({ compressionCutoff: 0 });
    expect(compression).toHaveValue(0);
  });

  it("explains SearXNG's count as the pages mewrk reads, and its compression as a local truncation", () => {
    renderBehavior(searchWith("searxng"));
    expect(hintOf(countRow()!)).toMatch(/mewrk 自己读取的结果页面数；0 表示返回多少读多少，最多 50/u);
    expect(hintOf(searchCompressionRow()!)).toMatch(/在本地截断/u);
  });

  it("draws neither search compression for a backend with no content-length parameter", () => {
    // Every catalog search backend has a count; only Exa, Jina and SearXNG have a cap.
    for (const kind of ["zhipu", "bocha", "querit", "firecrawl", "exa-mcp"] as const) {
      renderBehavior(searchWith(kind));
      expect(countRow(), kind).not.toBeNull();
      expect(searchCompressionRow(), kind).toBeNull();
      cleanup();
    }
  });

  it("draws nothing under a search that is off or lost", () => {
    renderBehavior({ provider: { kind: "disabled" } });
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
    cleanup();
    renderBehavior({ provider: { kind: "unavailable" } });
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
  });

  /* The stored number is the user's answer and the ceiling the backend's: what
     is shown is what is sent, and the stored number is not rewritten by looking
     at it. */
  it("shows a stored count through the backend's own ceiling and clamps what is typed to it", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({ ...searchWith("tavily"), maxResults: 30 });

    const count = countRow()!;
    expect(count).toHaveValue(20);
    expect(count).toHaveAttribute("max", "20");
    await user.clear(count);
    await user.type(count, "30");
    expect(lastPatch()).toEqual({ maxResults: 20 });
    await user.clear(count);
    await user.type(count, "0");
    expect(lastPatch()).toEqual({ maxResults: 0 });
  });

  it("lets a wider backend take the stored count the narrower one displayed down", () => {
    renderBehavior({ ...searchWith("exa"), maxResults: 30 });
    expect(countRow()).toHaveValue(30);
    expect(countRow()).toHaveAttribute("max", "100");
  });

  it("shows Jina's 500 floor on a smaller stored compression, and never lifts 0 to it", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({ ...searchWith("jina"), compressionCutoff: 100 });

    const compression = searchCompressionRow()!;
    expect(compression).toHaveValue(500);
    expect(hintOf(compression)).toContain("X-Max-Tokens");
    expect(hintOf(compression)).toContain("至少 500");
    // A small number is committed as the floor ...
    await user.clear(compression);
    await user.type(compression, "40");
    expect(lastPatch()).toEqual({ compressionCutoff: 500 });
    // ... and 0 stays "no cap".
    await user.clear(compression);
    await user.type(compression, "0");
    expect(lastPatch()).toEqual({ compressionCutoff: 0 });
    expect(compression).toHaveValue(0);
  });

  it("draws the fetch compression for Jina and for the local fetch, with each one's own floor", () => {
    renderBehavior({ ...fetchWith("jina"), fetchCompressionCutoff: 100 });
    expect(fetchCompressionRow()).toHaveValue(500);
    expect(hintOf(fetchCompressionRow()!)).toMatch(/Jina Reader 的 X-Max-Tokens/u);
    cleanup();

    renderBehavior({ ...fetchWith("fetch"), fetchCompressionCutoff: 100 });
    expect(fetchCompressionRow()).toHaveValue(100);
    expect(hintOf(fetchCompressionRow()!)).toMatch(/mewrk 在本地抓取页面后截断/u);
  });

  it("draws no fetch compression for Firecrawl or Querit, nor under a fetch that is off or lost", () => {
    for (const kind of ["firecrawl", "querit"] as const) {
      renderBehavior(fetchWith(kind));
      expect(fetchCompressionRow(), kind).toBeNull();
      cleanup();
    }
    renderBehavior({ fetchProvider: { kind: "disabled" } });
    expect(fetchCompressionRow()).toBeNull();
    cleanup();
    renderBehavior({ fetchProvider: { kind: "unavailable" } });
    expect(fetchCompressionRow()).toBeNull();
  });

  /* A native fetch is a separate tool on one family only; elsewhere the leg is
     ignored, so there is nothing to put a number on. */
  it("draws the native fetch compression only where the model has a native fetch", async () => {
    const user = userEvent.setup();
    renderBehavior({ fetchProvider: { kind: "native" } });
    expect(fetchCompressionRow()).toBeNull();
    cleanup();

    const { lastPatch } = renderBehavior({ fetchProvider: { kind: "native" } }, { nativeFetchAvailable: true });
    const compression = fetchCompressionRow()!;
    expect(compression).toHaveValue(2000);
    expect(hintOf(compression)).toContain("max_content_tokens");
    // A conversation knows its model, so it is not told about "this role's model".
    expect(hintOf(compression)).not.toContain("这个角色的模型");
    await user.clear(compression);
    await user.type(compression, "0");
    expect(lastPatch()).toEqual({ fetchCompressionCutoff: 0 });
  });

  it("keeps the two compression rows apart: same visible label, different names and values", () => {
    renderBehavior({
      ...searchWith("exa"),
      ...fetchWith("jina"),
      compressionCutoff: 3000,
      fetchCompressionCutoff: 4000
    });
    expect(screen.getAllByText("结果压缩", { selector: ".field__label" })).toHaveLength(2);
    expect(searchCompressionRow()).toHaveValue(3000);
    expect(fetchCompressionRow()).toHaveValue(4000);
  });

  it("writes each leg's compression to its own field", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({ ...searchWith("exa"), ...fetchWith("jina") });

    await user.clear(searchCompressionRow()!);
    await user.type(searchCompressionRow()!, "900");
    expect(lastPatch()).toEqual({ compressionCutoff: 900 });
    await user.clear(fetchCompressionRow()!);
    await user.type(fetchCompressionRow()!, "800");
    expect(lastPatch()).toEqual({ fetchCompressionCutoff: 800 });
  });

  /* The rows must read as belonging to the selector above them, so each leg's
     rows come straight after that leg's picker and before the next one. */
  it("draws each leg's rows under its own selector, in a group that is absent when empty", () => {
    renderBehavior({ ...searchWith("exa"), ...fetchWith("jina") });

    const searchPicker = screen.getByRole("button", { name: /^搜索提供商：/ });
    const fetchPicker = screen.getByRole("button", { name: /^抓取提供商：/ });
    const domain = screen.getByRole("combobox", { name: "域名过滤" });
    const follows = (earlier: Element, later: Element) =>
      Boolean(earlier.compareDocumentPosition(later) & Node.DOCUMENT_POSITION_FOLLOWING);
    expect(follows(searchPicker, countRow()!)).toBe(true);
    expect(follows(searchCompressionRow()!, fetchPicker)).toBe(true);
    expect(follows(fetchPicker, fetchCompressionRow()!)).toBe(true);
    expect(follows(fetchCompressionRow()!, domain)).toBe(true);
    const groups = document.querySelectorAll(".web-search-leg-params");
    expect(groups).toHaveLength(2);
    expect(groups[0]).toContainElement(countRow());
    expect(groups[0]).toContainElement(searchCompressionRow());
    expect(groups[1]).toContainElement(fetchCompressionRow());

    cleanup();
    // Native search and a fetch with no cap: no rows, so no group either.
    renderBehavior(fetchWith("firecrawl"));
    expect(document.querySelector(".web-search-leg-params")).toBeNull();
  });

  /* A role answers every row for itself, so it never meets the generic rows of
     a backend it does not know; what it cannot know from its editor is the
     family its model is in, so its native fetch carries the number and says
     when it is sent. */
  it("offers a role's own native choices their rows only where they can take effect", () => {
    // Native search has no number on any family.
    renderBehavior({ provider: { kind: "native" }, fetchProvider: { kind: "native" } }, { subject: "role" });
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
    const hint = hintOf(fetchCompressionRow()!);
    expect(hint).toContain("max_content_tokens");
    expect(hint).toContain("只有这个角色的模型属于 Anthropic 家族时才会发出");
  });

  it("speaks English in the rows' names and hints when the language is English", () => {
    configureI18n("en-US");
    renderBehavior({ ...searchWith("jina"), ...fetchWith("jina"), compressionCutoff: 100 });
    expect(screen.getByRole("spinbutton", { name: "Result count" })).toHaveValue(5);
    expect(screen.getByRole("spinbutton", { name: "Search result compression" })).toHaveValue(500);
    expect(screen.getByRole("spinbutton", { name: "Fetch result compression" })).toHaveValue(2000);
    expect(hintOf(screen.getByRole("spinbutton", { name: "Result count" })))
      .toBe("Sent as Jina's count; 0 sends nothing and leaves Jina's own default. At most 20.");
  });

  /* The row is a choice between the two lists rather than a switch on each: a
     result admitted by one and refused by the other has no obvious answer. */
  it("selects which domain list is in effect without emptying either", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({ excludeDomains: ["*://ads.example/*"] });

    const filter = screen.getByRole("combobox", { name: "域名过滤" });
    expect(filter).toHaveValue("off");
    await user.selectOptions(filter, "include");
    expect(lastPatch()).toEqual({ domainFilter: "include" });
    await user.selectOptions(filter, "off");
    expect(lastPatch()).toEqual({ domainFilter: "off" });
  });

  it("writes each list on its own page of the rules window", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior();

    await user.click(screen.getByRole("button", { name: "编辑名单" }));
    const dialog = screen.getByRole("dialog", { name: "域名名单" });

    // With filtering off the window opens on the blocklist, the list a person
    // reaching for this row almost always means.
    await user.type(
      within(dialog).getByRole("textbox", { name: "黑名单" }),
      "*://ads.example/*"
    );
    expect(lastPatch()).toEqual({ excludeDomains: ["*://ads.example/*"] });

    await user.click(within(dialog).getByRole("button", { name: /白名单/ }));
    await user.type(
      within(dialog).getByRole("textbox", { name: "白名单" }),
      "*://docs.example/*"
    );
    expect(lastPatch()).toEqual({ includeDomains: ["*://docs.example/*"] });
  });

  /* A provider that cannot serve the leg — switched off in settings or unknown
     — keeps its tool, so the picker says Repair rather than passing for a
     working choice or for "off". The subtitle stays the standing advice. */
  it("asks for a repair when the chosen provider is off or unknown", () => {
    renderBehavior({ fetchProvider: { kind: "unavailable" } });
    expect(screen.getByRole("button", { name: "抓取提供商：请修复抓取提供商" })).toBeInTheDocument();
    cleanup();

    // Exa is switched off in settings.
    renderBehavior({ provider: { kind: "explicit", providerKind: "exa" } });
    const search = screen.getByRole("button", { name: "搜索提供商：请修复搜索提供商" });
    const hint = search.closest<HTMLElement>("label.field")!.querySelector<HTMLElement>(".field__hint")!;
    expect(hint).toHaveTextContent("原生用模型自带的搜索；选提供商则由 Mewrk 代为检索。");
  });

  /* A native backend a run has already used is a fact about the transcript, not
     a lock: the selector is simply held, and its reason stands where the standing
     advice would — with no lock mark and no tone, since nothing is warned about. */
  it("holds a settled selector and says why in place of the advice, with no lock drawn", () => {
    renderBehavior({}, { searchLock: { kind: "settled", note: "这个对话已经用过原生搜索" } });

    const search = screen.getByRole("button", { name: /^搜索提供商：/ });
    expect(search).toBeDisabled();
    expect(search).not.toHaveClass("popover-select__trigger--cache");
    expect(search.querySelector(".lock-mark")).toBeNull();
    const field = search.closest<HTMLElement>("label.field")!;
    expect(within(field).getByText("这个对话已经用过原生搜索")).toBeInTheDocument();
    expect(field.querySelector(".lock-note")).toBeNull();
    expect(within(field).queryByText(/原生用模型自带的搜索/u)).not.toBeInTheDocument();

    // The other leg answers for itself.
    expect(screen.getByRole("button", { name: /^抓取提供商：/ })).toBeEnabled();
  });

  /* Orange is a warning, and the selector still moves — the caller's one-time
     confirmation is what stands between the click and the change. */
  it("keeps a warm-cache selector moving, orange and locked, with its note above the advice", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({}, { fetchLock: { kind: "cache", note: "缓存还热" } });

    const fetch = screen.getByRole("button", { name: /^抓取提供商：/ });
    expect(fetch).toBeEnabled();
    expect(fetch).toHaveClass("popover-select__trigger--cache");
    expect(fetch.querySelector(".lock-mark--cache")).not.toBeNull();
    const hint = fetch.closest<HTMLElement>("label.field")!.querySelector<HTMLElement>(".field__hint")!;
    expect(hint.firstElementChild).toHaveTextContent("缓存还热");
    expect(hint.firstElementChild).toHaveClass("lock-note", "lock-note--cache");
    expect(hint).toHaveTextContent(/原生用模型自带的抓取/u);

    // No choice in the menu is held, including the one that removes the tool.
    const menu = await openMenu(user, "抓取提供商");
    for (const item of within(menu).getAllByRole("menuitemradio")) expect(item).toBeEnabled();
    await user.click(within(menu).getByRole("menuitemradio", { name: "不启用" }));
    expect(lastPatch()).toEqual({ fetchProvider: { kind: "disabled" } });
  });

  /* A key is checked when the call is made, not when the provider is picked:
     no credential is consulted here, and nothing on the field mentions one. */
  it("selects a provider that needs a key like any other", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior();

    const search = await openMenu(user, "搜索提供商");
    await user.click(within(search).getByRole("menuitemradio", { name: "Tavily" }));
    expect(lastPatch()).toEqual({ provider: { kind: "explicit", providerKind: "tavily" } });
    const trigger = screen.getByRole("button", { name: "搜索提供商：Tavily" });
    expect(trigger).not.toHaveClass("input--error");
    expect(screen.queryByText(/API Key/u)).not.toBeInTheDocument();
  });

  /* The placeholder is read as a template to copy, so every line of it has to
     be something the host's matcher accepts (`domain_rules.rs`): `<all_urls>`,
     `scheme://host/path`, or `/regex/`. A bare `*.example.com` is ignored. */
  it("shows only working rules as examples", async () => {
    const user = userEvent.setup();
    renderBehavior();

    await user.click(screen.getByRole("button", { name: "编辑名单" }));
    const dialog = screen.getByRole("dialog", { name: "域名名单" });
    const examples = (within(dialog).getByRole("textbox", { name: "黑名单" }).getAttribute("placeholder") ?? "")
      .split("\n");
    expect(examples).toContain("*://*.example.com/*");
    for (const rule of examples) {
      expect(
        rule === "<all_urls>"
          || /^(\*|https?):\/\/[^/]+\/.*$/.test(rule)
          || (rule.length >= 2 && rule.startsWith("/") && rule.endsWith("/")),
        rule
      ).toBe(true);
    }
    expect(within(dialog).getByText(/裸写的 example\.com 或 \*\.example\.com 不是规则/)).toBeInTheDocument();
  });
});
