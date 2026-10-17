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

type BehaviorExtra = Partial<Pick<
  WebSearchBehaviorSettingsProps,
  | "searchLock"
  | "fetchLock"
  | "nativeFetchAvailable"
  | "nativeToolTypeSelectable"
  | "subject"
  | "switchTones"
  | "lockHints"
  | "rememberKey"
>>;

/**
 * `webAccess` gives the page the surface's own access switch, the way a
 * conversation does; leaving it out is a role's page, which has none.
 */
function renderBehavior(
  initial: Partial<ConversationWebSearchSettings> = {},
  extra: BehaviorExtra = {},
  { webAccess }: { webAccess?: boolean } = {}
) {
  const onChange = vi.fn();
  const onAccessChange = vi.fn();
  const start = { ...defaultConversationWebSearchSettings(), ...initial };
  function Harness({ props }: { props: BehaviorExtra }) {
    const [value, setValue] = useState(start);
    const [enabled, setEnabled] = useState(webAccess ?? true);
    return (
      <div className="web-search-provider-field">
        <WebSearchBehaviorSettings
          {...props}
          value={value}
          webSearchAssets={assets()}
          webAccess={webAccess === undefined
            ? undefined
            : {
              enabled,
              onChange: (next, patch) => {
                onAccessChange(next, patch);
                setEnabled(next);
                setValue((current) => ({ ...current, ...patch }));
              }
            }}
          onChange={(patch) => {
            onChange(patch);
            setValue((current) => ({ ...current, ...patch }));
          }}
        />
      </div>
    );
  }
  const view = render(<Harness props={extra} />);
  return {
    onChange,
    onAccessChange,
    lastPatch: () => onChange.mock.calls.at(-1)?.[0],
    /** The same page, told something new about the model it is drawn for. */
    rerenderWith: (next: BehaviorExtra) => view.rerender(<Harness props={next} />)
  };
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

/** The docs link a row's hint ends with, when it has one. */
const resultsLinkOf = (input: HTMLElement) =>
  within(input.closest<HTMLElement>("label.field")!.querySelector<HTMLElement>(".field__hint")!)
    .getByRole("link", { name: /详情/ });

async function openMenu(user: ReturnType<typeof userEvent.setup>, label: string) {
  await user.click(screen.getByRole("button", { name: new RegExp(`^${label}：`) }));
  return screen.getByRole("menu", { name: label });
}

/** A leg's switch, whichever way it currently reads; its name says the state. */
const searchSwitch = () => screen.getByRole("switch", { name: /^网页搜索已/ });
const fetchSwitch = () => screen.getByRole("switch", { name: /^网页抓取已/ });
const searchTrigger = () => screen.queryByRole("button", { name: /^搜索提供商：/ });
const fetchTrigger = () => screen.queryByRole("button", { name: /^抓取提供商：/ });
const domainFilter = () => screen.queryByRole("combobox", { name: "域名过滤" });

describe("WebSearchBehaviorSettings", () => {
  beforeEach(() => {
    configureI18n("zh-CN");
  });

  /* A provider that is switched off is not a choice. Offering it greyed out
     would make the menu a list of things that do not work, and the global page
     is where a provider is switched on. */
  it("lists only enabled providers, and names the native row without a parenthetical", async () => {
    const user = userEvent.setup();
    renderBehavior({}, { nativeFetchAvailable: true });

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

  /* Turning a leg off is its switch, so neither menu has a row for it. */
  it("offers no 不启用 row in either menu", async () => {
    const user = userEvent.setup();
    renderBehavior({}, { nativeFetchAvailable: true });

    const search = await openMenu(user, "搜索提供商");
    expect(within(search).queryByText(/不启用/)).toBeNull();
    expect(within(search).queryByRole("menuitemradio", { name: /不启用/ })).toBeNull();
    expect(within(search).getAllByRole("menuitemradio").length).toBeGreaterThan(1);
    await user.keyboard("{Escape}");

    const fetch = await openMenu(user, "抓取提供商");
    expect(within(fetch).queryByText(/不启用/)).toBeNull();
    expect(within(fetch).queryByRole("menuitemradio", { name: /不启用/ })).toBeNull();
    expect(within(fetch).getAllByRole("menuitemradio").length).toBeGreaterThan(1);
  });

  /* Native is a fetch backend only on a model with a fetch tool of its own; a
     menu offering it elsewhere would offer a choice that fetches nothing. */
  it("has no Native row in the fetch menu on a model without a native fetch", async () => {
    const user = userEvent.setup();
    renderBehavior(fetchWith("jina"), { nativeFetchAvailable: false });

    const fetch = await openMenu(user, "抓取提供商");
    expect(within(fetch).queryByRole("menuitemradio", { name: "原生" })).toBeNull();
    expect(within(fetch).getByRole("menuitemradio", { name: "Jina" })).toBeChecked();
    await user.keyboard("{Escape}");

    // Search has a native backend of its own on every family.
    const search = await openMenu(user, "搜索提供商");
    expect(within(search).getByRole("menuitemradio", { name: "原生" })).toBeInTheDocument();
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
    renderBehavior({}, { subject, nativeFetchAvailable: true });

    expect(screen.getByText(searchAdvice)).toBeInTheDocument();
    expect(screen.getByText(fetchAdvice)).toBeInTheDocument();
    expect(screen.queryByText(notThis)).toBeNull();
  });

  /* Without a native fetch there is no native row to advise on, so the fetch
     advice says who reads pages instead — still naming the right model. */
  it.each([
    ["zh-CN" as const, "conversation" as const, /^这个模型没有自带的抓取，由所选提供商读取/, /这个角色/],
    ["zh-CN" as const, "role" as const, /^这个角色所跑的模型没有自带的抓取，由所选提供商读取/, /^这个模型没有/],
    ["en-US" as const, "conversation" as const, /^This model has no fetch of its own; the provider reads pages/, /this role/],
    ["en-US" as const, "role" as const, /^This role's model has no fetch of its own; the provider reads pages/, /^This model has no/]
  ])("says who reads pages when the model has no native fetch, in %s for a %s", (language, subject, advice, notThis) => {
    configureI18n(language);
    renderBehavior(fetchWith("jina"), { subject, nativeFetchAvailable: false });

    expect(screen.getByText(advice)).toBeInTheDocument();
    expect(screen.queryByText(notThis)).toBeNull();
  });

  it("speaks of the conversation unless it is told the settings are a role's", () => {
    renderBehavior();
    expect(screen.getByText(/^原生用模型自带的搜索/)).toBeInTheDocument();
  });

  /* A subagent role answers every row for itself, through this same component,
     so no row has a "follow the conversation" answer to offer: only the
     backends and the three filter modes — a leg is off by its switch, not by a
     row of its own. */
  it("offers no row that follows the conversation's settings", async () => {
    const user = userEvent.setup();
    renderBehavior({}, { nativeFetchAvailable: true });

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

  /* The page has two switches where it used to have one: the old "联网搜索"
     switch is gone, and each leg reads its own state. */
  it("draws a switch for each leg and none for web access as a whole", () => {
    renderBehavior({}, { nativeFetchAvailable: true });

    expect(screen.getByRole("switch", { name: "网页搜索已开启" })).toBeChecked();
    expect(screen.getByRole("switch", { name: "网页抓取已开启" })).toBeChecked();
    expect(screen.queryByRole("switch", { name: /联网搜索/ })).toBeNull();
    expect(screen.getAllByRole("switch")).toHaveLength(2);
    expect(screen.getByText("网页搜索", { selector: "strong" })).toBeInTheDocument();
    expect(screen.getByText("网页抓取", { selector: "strong" })).toBeInTheDocument();
  });

  it("speaks English in the switches' names when the language is English", () => {
    configureI18n("en-US");
    renderBehavior({ provider: { kind: "disabled" } }, { nativeFetchAvailable: true });

    expect(screen.getByRole("switch", { name: "Web search disabled" })).not.toBeChecked();
    expect(screen.getByRole("switch", { name: "Web fetch enabled" })).toBeChecked();
  });

  /* A leg is off by its switch, and a switched-off leg draws nothing under it:
     not its selector, not its numbers. */
  it("turns either leg off on its own", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(
      { ...searchWith("exa"), ...fetchWith("jina") },
      { nativeFetchAvailable: true }
    );
    expect(searchTrigger()).not.toBeNull();
    expect(countRow()).not.toBeNull();
    expect(fetchCompressionRow()).not.toBeNull();

    await user.click(searchSwitch());
    expect(lastPatch()).toEqual({ provider: { kind: "disabled" } });
    expect(screen.getByRole("switch", { name: "网页搜索已关闭" })).not.toBeChecked();
    expect(searchTrigger()).toBeNull();
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
    // The other leg answers for itself.
    expect(screen.getByRole("switch", { name: "网页抓取已开启" })).toBeChecked();
    expect(fetchTrigger()).not.toBeNull();
    expect(fetchCompressionRow()).not.toBeNull();

    await user.click(fetchSwitch());
    expect(lastPatch()).toEqual({ fetchProvider: { kind: "disabled" } });
    expect(screen.getByRole("switch", { name: "网页抓取已关闭" })).not.toBeChecked();
    expect(fetchTrigger()).toBeNull();
    expect(fetchCompressionRow()).toBeNull();
  });

  /* The domain filter narrows what either leg returns, so it has nothing to say
     while both are off. */
  it("draws the domain filter only while either leg is on", async () => {
    const user = userEvent.setup();
    renderBehavior({}, { nativeFetchAvailable: true });
    expect(domainFilter()).not.toBeNull();

    await user.click(searchSwitch());
    expect(domainFilter()).not.toBeNull();
    await user.click(fetchSwitch());
    expect(domainFilter()).toBeNull();
    expect(screen.queryByRole("button", { name: "编辑名单" })).toBeNull();

    await user.click(searchSwitch());
    expect(domainFilter()).not.toBeNull();
  });

  /* Off keeps the backend it had, so the leg comes back as it was left rather
     than as a default. */
  it("restores the provider a leg had when it is switched off and on again", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(
      { ...searchWith("tavily"), ...fetchWith("jina") },
      { nativeFetchAvailable: true }
    );

    await user.click(searchSwitch());
    expect(lastPatch()).toEqual({ provider: { kind: "disabled" } });
    await user.click(searchSwitch());
    expect(lastPatch()).toEqual({ provider: { kind: "explicit", providerKind: "tavily" } });
    expect(screen.getByRole("button", { name: "搜索提供商：Tavily" })).toBeInTheDocument();

    await user.click(fetchSwitch());
    await user.click(fetchSwitch());
    expect(lastPatch()).toEqual({ fetchProvider: { kind: "explicit", providerKind: "jina" } });
    expect(screen.getByRole("button", { name: "抓取提供商：Jina" })).toBeInTheDocument();
  });

  /* Leaving the page and coming back is the ordinary way back to a switch, so
     a surface that names itself keeps what its legs set aside for the session. */
  it("keeps a set-aside provider across leaving the page when the surface names itself", async () => {
    const user = userEvent.setup();
    const first = renderBehavior(searchWith("tavily"), { rememberKey: "conversation:keep" });
    await user.click(searchSwitch());
    expect(first.lastPatch()).toEqual({ provider: { kind: "disabled" } });
    cleanup();

    const again = renderBehavior({ provider: { kind: "disabled" } }, { rememberKey: "conversation:keep" });
    await user.click(searchSwitch());
    expect(again.lastPatch()).toEqual({ provider: { kind: "explicit", providerKind: "tavily" } });
    cleanup();

    // Another surface has nothing set aside, and starts from the default.
    const other = renderBehavior({ provider: { kind: "disabled" } }, { rememberKey: "conversation:other" });
    await user.click(searchSwitch());
    expect(other.lastPatch()).toEqual({ provider: { kind: "native" } });
  });

  it("restores a native backend too, and remembers a later pick in its place", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior({}, { nativeFetchAvailable: true });

    await user.click(searchSwitch());
    await user.click(searchSwitch());
    expect(lastPatch()).toEqual({ provider: { kind: "native" } });

    // Pick another backend, then round-trip the switch: the pick is what returns.
    const menu = await openMenu(user, "搜索提供商");
    await user.click(within(menu).getByRole("menuitemradio", { name: "Tavily" }));
    await user.click(searchSwitch());
    await user.click(searchSwitch());
    expect(lastPatch()).toEqual({ provider: { kind: "explicit", providerKind: "tavily" } });
  });

  /* Web access is the union of the two legs. A conversation hands the page its
     own switch, which goes off with the last leg that names a backend and on
     with the first one switched on, written in the same change as the backend. */
  describe("with the surface's own web access switch", () => {
    it("writes the flip and the backend in one change, and keeps access on while the other leg is on", async () => {
      const user = userEvent.setup();
      const { onChange, onAccessChange } = renderBehavior(
        { ...searchWith("tavily"), ...fetchWith("jina") },
        { nativeFetchAvailable: true },
        { webAccess: true }
      );

      await user.click(searchSwitch());
      expect(onAccessChange).toHaveBeenLastCalledWith(true, { provider: { kind: "disabled" } });
      await user.click(searchSwitch());
      expect(onAccessChange).toHaveBeenLastCalledWith(true, {
        provider: { kind: "explicit", providerKind: "tavily" }
      });
      // Nothing went through the plain change path.
      expect(onChange).not.toHaveBeenCalled();
      // A pick inside a leg is not a flip.
      const menu = await openMenu(user, "抓取提供商");
      await user.click(within(menu).getByRole("menuitemradio", { name: "原生" }));
      expect(onChange).toHaveBeenLastCalledWith({ fetchProvider: { kind: "native" } });
      expect(onAccessChange).toHaveBeenCalledTimes(2);
    });

    it("turns access off with the last leg that names a backend, leaving that backend in place", async () => {
      const user = userEvent.setup();
      const { onAccessChange } = renderBehavior(
        { provider: { kind: "disabled" }, ...fetchWith("jina") },
        { nativeFetchAvailable: true },
        { webAccess: true }
      );
      expect(searchSwitch()).not.toBeChecked();
      expect(fetchSwitch()).toBeChecked();

      await user.click(fetchSwitch());
      expect(onAccessChange).toHaveBeenLastCalledWith(false, {});
      // Both legs now read off, and nothing is drawn under either.
      expect(searchSwitch()).not.toBeChecked();
      expect(fetchSwitch()).not.toBeChecked();
      expect(searchTrigger()).toBeNull();
      expect(fetchTrigger()).toBeNull();
      expect(domainFilter()).toBeNull();
    });

    /* Switching one leg on from a page whose access is off must not bring the
       other leg's stored backend back with it. */
    it("turns fetch on when access was off by calling webAccess.onChange(true, …) with the search leg set aside", async () => {
      const user = userEvent.setup();
      const { onAccessChange } = renderBehavior(
        fetchWith("jina"),
        { nativeFetchAvailable: true },
        { webAccess: false }
      );
      // Access off reads both legs off, whatever they still name.
      expect(searchSwitch()).not.toBeChecked();
      expect(fetchSwitch()).not.toBeChecked();
      expect(fetchTrigger()).toBeNull();

      await user.click(fetchSwitch());
      expect(onAccessChange).toHaveBeenCalledTimes(1);
      expect(onAccessChange).toHaveBeenLastCalledWith(true, {
        fetchProvider: { kind: "explicit", providerKind: "jina" },
        provider: { kind: "disabled" }
      });
      expect(fetchSwitch()).toBeChecked();
      expect(searchSwitch()).not.toBeChecked();
      expect(searchTrigger()).toBeNull();
      expect(screen.getByRole("button", { name: "抓取提供商：Jina" })).toBeInTheDocument();

      // The set-aside search backend is remembered: it is what comes back.
      await user.click(searchSwitch());
      expect(onAccessChange).toHaveBeenLastCalledWith(true, { provider: { kind: "native" } });
    });

    /* A stored native fetch on a model without one is a fact about another
       model. It reads off here, is not rewritten by being looked at, and is
       found on again when the page is drawn for a model that has one. */
    it("reads a stored native fetch as off on a model without one, and back on when the model has one", () => {
      const { onChange, onAccessChange, rerenderWith } = renderBehavior(
        { provider: { kind: "native" }, fetchProvider: { kind: "native" } },
        { nativeFetchAvailable: false },
        { webAccess: true }
      );

      expect(screen.getByRole("switch", { name: "网页搜索已开启" })).toBeChecked();
      expect(screen.getByRole("switch", { name: "网页抓取已关闭" })).not.toBeChecked();
      expect(fetchTrigger()).toBeNull();
      expect(fetchCompressionRow()).toBeNull();

      rerenderWith({ nativeFetchAvailable: true });
      expect(screen.getByRole("switch", { name: "网页抓取已开启" })).toBeChecked();
      expect(screen.getByRole("button", { name: "抓取提供商：原生" })).toBeInTheDocument();

      // Looking at it did not write it.
      expect(onChange).not.toHaveBeenCalled();
      expect(onAccessChange).not.toHaveBeenCalled();

      rerenderWith({ nativeFetchAvailable: false });
      expect(screen.getByRole("switch", { name: "网页抓取已关闭" })).not.toBeChecked();
      expect(fetchTrigger()).toBeNull();
      expect(onChange).not.toHaveBeenCalled();
      expect(onAccessChange).not.toHaveBeenCalled();
    });

    it("keeps access on when search is switched off beside a fetch that is only off for lack of a native one", async () => {
      const user = userEvent.setup();
      const { onAccessChange, rerenderWith } = renderBehavior(
        { provider: { kind: "native" }, fetchProvider: { kind: "native" } },
        { nativeFetchAvailable: false },
        { webAccess: true }
      );

      await user.click(searchSwitch());
      // The fetch leg still names a backend, so access stays on for it.
      expect(onAccessChange).toHaveBeenLastCalledWith(true, { provider: { kind: "disabled" } });

      rerenderWith({ nativeFetchAvailable: true });
      expect(screen.getByRole("switch", { name: "网页抓取已开启" })).toBeChecked();
      expect(searchSwitch()).not.toBeChecked();
    });

    it("turns fetch on with native when the model has one, and with the first catalog provider when it does not", async () => {
      const user = userEvent.setup();
      const none = renderBehavior(
        { fetchProvider: { kind: "disabled" } },
        { nativeFetchAvailable: false },
        { webAccess: true }
      );
      await user.click(fetchSwitch());
      expect(none.onAccessChange).toHaveBeenLastCalledWith(true, {
        fetchProvider: { kind: "explicit", providerKind: "jina" }
      });
      cleanup();

      const native = renderBehavior(
        { fetchProvider: { kind: "disabled" } },
        { nativeFetchAvailable: true },
        { webAccess: true }
      );
      await user.click(fetchSwitch());
      expect(native.onAccessChange).toHaveBeenLastCalledWith(true, { fetchProvider: { kind: "native" } });
    });
  });

  /* A role has no access switch: its caller decides that and stays the ceiling.
     Its legs answer only for themselves, so a flip is a plain change. */
  describe("without a web access switch", () => {
    it("flips a leg through the plain change, whatever the other leg names", async () => {
      const user = userEvent.setup();
      const { onChange, onAccessChange } = renderBehavior(
        { provider: { kind: "disabled" }, ...fetchWith("jina") },
        { subject: "role", nativeFetchAvailable: true }
      );

      // Switching off the last named leg writes the leg, not a master switch.
      await user.click(fetchSwitch());
      expect(onChange).toHaveBeenLastCalledWith({ fetchProvider: { kind: "disabled" } });
      expect(searchSwitch()).not.toBeChecked();
      expect(fetchSwitch()).not.toBeChecked();

      // Switching one on writes only that leg, with nothing set aside.
      await user.click(searchSwitch());
      expect(onChange).toHaveBeenLastCalledWith({ provider: { kind: "native" } });
      await user.click(fetchSwitch());
      expect(onChange).toHaveBeenLastCalledWith({ fetchProvider: { kind: "explicit", providerKind: "jina" } });
      expect(onAccessChange).not.toHaveBeenCalled();
    });
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
    expect(hintOf(count)).toBe("最多 20；0 使用后端默认值。");
    await user.clear(count);
    await user.type(count, "12");
    expect(lastPatch()).toEqual({ maxResults: 12 });
  });

  it("draws both rows for Exa and takes 0 as a legal answer on the compression", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(searchWith("exa"));

    expect(countRow()).toHaveValue(5);
    expect(hintOf(countRow()!)).toBe("最多 100；0 使用后端默认值。");
    const compression = searchCompressionRow()!;
    expect(compression).toHaveValue(2000);
    expect(hintOf(compression)).toMatch(/^每条结果最多保留多少 token；0 不设限。/u);
    expect(resultsLinkOf(compression)).toHaveAttribute("href", "https://mewrk.dev/zh-CN/web.html#results");
    await user.clear(compression);
    await user.type(compression, "0");
    expect(lastPatch()).toEqual({ compressionCutoff: 0 });
    expect(compression).toHaveValue(0);
  });

  it("explains SearXNG's count as the result pages read, since its API has no count", () => {
    renderBehavior(searchWith("searxng"));
    expect(hintOf(countRow()!)).toBe("读取的结果页数，最多 50；0 为全部。");
    expect(hintOf(searchCompressionRow()!)).toMatch(/^每条结果最多保留多少 token/u);
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
    expect(hintOf(compression)).toMatch(/^每条结果最多保留多少 token；0 不设限。/u);
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
    expect(hintOf(fetchCompressionRow()!)).toMatch(/^每个页面最多保留多少 token；0 不设限。/u);
    expect(resultsLinkOf(fetchCompressionRow()!)).toHaveAttribute("href", "https://mewrk.dev/zh-CN/web.html#results");
    cleanup();

    renderBehavior({ ...fetchWith("fetch"), fetchCompressionCutoff: 100 });
    expect(fetchCompressionRow()).toHaveValue(100);
    expect(hintOf(fetchCompressionRow()!)).toMatch(/^每个页面最多保留多少 token；0 不设限。/u);
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

  /* A native fetch carries a number only as the Messages `web_fetch` tool, so
     the row is drawn where the native fetch version is selectable. On a model
     that has no native fetch the leg reads off, and on one that has it without
     writing the Messages tool (the agent family) there is no number to send. */
  it("draws the native fetch compression only where the model has a native fetch", async () => {
    const user = userEvent.setup();
    // No native fetch: the leg reads off, so there is no selector and no number.
    renderBehavior({ fetchProvider: { kind: "native" } });
    expect(fetchTrigger()).toBeNull();
    expect(fetchCompressionRow()).toBeNull();
    cleanup();

    // A native fetch that is not the Messages tool has no number to put.
    renderBehavior({ fetchProvider: { kind: "native" } }, { nativeFetchAvailable: true });
    expect(fetchTrigger()).not.toBeNull();
    expect(fetchCompressionRow()).toBeNull();
    cleanup();

    const { lastPatch } = renderBehavior(
      { fetchProvider: { kind: "native" } },
      { nativeFetchAvailable: true, nativeToolTypeSelectable: true }
    );
    const compression = fetchCompressionRow()!;
    expect(compression).toHaveValue(2000);
    expect(hintOf(compression)).toBe("每个页面最多保留多少 token；0 不设限。");
    // A conversation knows its model, so it is not told which family sends the number.
    expect(hintOf(compression)).not.toContain("Anthropic");
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
     when it is sent — and only where the role's own model can send it. */
  it("offers a role's own native choices their rows only where they can take effect", () => {
    const native: Partial<ConversationWebSearchSettings> = {
      provider: { kind: "native" },
      fetchProvider: { kind: "native" }
    };

    // A model with no native fetch reads the native fetch off: no selector, no number.
    renderBehavior(native, { subject: "role" });
    expect(fetchTrigger()).toBeNull();
    expect(fetchCompressionRow()).toBeNull();
    cleanup();

    // A native fetch that is not the Messages tool is offered without a number.
    renderBehavior(native, { subject: "role", nativeFetchAvailable: true });
    expect(fetchTrigger()).not.toBeNull();
    expect(fetchCompressionRow()).toBeNull();
    cleanup();

    // Native search has no number on any family.
    renderBehavior(native, { subject: "role", nativeFetchAvailable: true, nativeToolTypeSelectable: true });
    expect(countRow()).toBeNull();
    expect(searchCompressionRow()).toBeNull();
    const hint = hintOf(fetchCompressionRow()!);
    expect(hint).toBe("每页上限，仅 Anthropic 家族模型会发送。");
  });

  it("speaks English in the rows' names and hints when the language is English", () => {
    configureI18n("en-US");
    renderBehavior({ ...searchWith("jina"), ...fetchWith("jina"), compressionCutoff: 100 });
    expect(screen.getByRole("spinbutton", { name: "Result count" })).toHaveValue(5);
    expect(screen.getByRole("spinbutton", { name: "Search result compression" })).toHaveValue(500);
    expect(screen.getByRole("spinbutton", { name: "Fetch result compression" })).toHaveValue(2000);
    expect(hintOf(screen.getByRole("spinbutton", { name: "Result count" })))
      .toBe("Up to 20; 0 uses the backend's default.");
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
    expect(hint).toHaveTextContent("原生用模型自带的搜索。");
  });

  /* A native backend a run has already used is a fact about the transcript, not
     a lock: the selector is simply held, and its reason stands where the standing
     advice would — with no lock mark and no tone, since nothing is warned about.
     The leg's switch is held with it, since turning the leg off would move the
     backend too, and says the same thing in its own row. */
  it("holds a settled selector and says why in place of the advice, with no lock drawn", () => {
    renderBehavior(
      {},
      {
        nativeFetchAvailable: true,
        searchLock: { kind: "settled", note: "这个对话已经用过原生搜索" }
      }
    );

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
    expect(fetchSwitch()).toBeEnabled();
  });

  it("holds a settled leg's switch, with the reason in its row and no tone drawn", async () => {
    const user = userEvent.setup();
    const { onChange } = renderBehavior(
      {},
      {
        nativeFetchAvailable: true,
        searchLock: { kind: "settled", note: "这个对话已经用过原生搜索" },
        // A tone would be orange; a settled switch shows its reason instead.
        switchTones: { search: "cache" },
        lockHints: { cache: "缓存还热" }
      }
    );

    const toggle = searchSwitch();
    expect(toggle).toBeDisabled();
    expect(toggle).toBeChecked();
    expect(toggle).not.toHaveClass("switch--cache");
    const row = toggle.closest<HTMLElement>(".tool-toggle-row")!;
    expect(row).toHaveClass("tool-toggle-row--disabled");
    expect(row).not.toHaveClass("tool-toggle-row--cache");
    expect(within(row).getByText("这个对话已经用过原生搜索")).toBeInTheDocument();
    expect(within(row).queryByText("缓存还热")).toBeNull();
    expect(row.querySelector(".lock-mark")).toBeNull();

    await user.click(toggle);
    expect(onChange).not.toHaveBeenCalled();
    expect(screen.getByRole("switch", { name: "网页搜索已开启" })).toBeChecked();
  });

  /* Orange is a warning, and the selector still moves — the caller's one-time
     confirmation is what stands between the click and the change. */
  it("keeps a warm-cache selector moving, orange and locked, with its note above the advice", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(
      {},
      { nativeFetchAvailable: true, fetchLock: { kind: "cache", note: "缓存还热" } }
    );

    const fetch = screen.getByRole("button", { name: /^抓取提供商：/ });
    expect(fetch).toBeEnabled();
    expect(fetch).toHaveClass("popover-select__trigger--cache");
    expect(fetch.querySelector(".lock-mark--cache")).not.toBeNull();
    const hint = fetch.closest<HTMLElement>("label.field")!.querySelector<HTMLElement>(".field__hint")!;
    expect(hint.firstElementChild).toHaveTextContent("缓存还热");
    expect(hint.firstElementChild).toHaveClass("lock-note", "lock-note--cache");
    expect(hint).toHaveTextContent(/原生用模型自带的抓取/u);

    // No choice in the menu is held, and none of them is a way to switch the leg off.
    const menu = await openMenu(user, "抓取提供商");
    for (const item of within(menu).getAllByRole("menuitemradio")) expect(item).toBeEnabled();
    expect(within(menu).queryByText(/不启用/)).toBeNull();
    await user.click(within(menu).getByRole("menuitemradio", { name: "Jina" }));
    expect(lastPatch()).toEqual({ fetchProvider: { kind: "explicit", providerKind: "jina" } });
  });

  /* The switch is toned like a tool's own row, and moves all the same: the
     caller's confirmation is what stands in front of the flip. */
  it("draws a toned leg switch orange with its hint, and lets it move", async () => {
    const user = userEvent.setup();
    const { lastPatch } = renderBehavior(
      {},
      {
        nativeFetchAvailable: true,
        switchTones: { fetch: "cache" },
        lockHints: { cache: "缓存还热：改动这一项会让它失效。" }
      }
    );

    const toggle = fetchSwitch();
    expect(toggle).toBeEnabled();
    expect(toggle).toHaveClass("switch--cache");
    const row = toggle.closest<HTMLElement>(".tool-toggle-row")!;
    expect(row).toHaveClass("tool-toggle-row--cache");
    expect(within(row).getByText("缓存还热：改动这一项会让它失效。")).toBeInTheDocument();
    expect(row.querySelector(".lock-mark--cache")).not.toBeNull();

    // The other leg's switch carries no tone of its own.
    const other = searchSwitch();
    expect(other).not.toHaveClass("switch--cache");
    expect(other.closest(".tool-toggle-row")).not.toHaveClass("tool-toggle-row--cache");

    await user.click(toggle);
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
    // The syntax is on the site; the window keeps one line and a way there.
    expect(within(dialog).getByText(/^命中的结果会被丢掉，一行一条规则。/)).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: /详情/ }))
      .toHaveAttribute("href", "https://mewrk.dev/zh-CN/web.html#domains");
  });
});
