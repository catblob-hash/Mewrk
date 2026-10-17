import { BookOpen, ExternalLink } from "lucide-react";
import { useI18n } from "../i18n";
import type { ResolvedAppLanguage } from "../types";

/**
 * Links to the published documentation at mewrk.dev.
 *
 * The UI says what a control is for in a few words; how it works — paths,
 * formats, caveats, what happens on each side of a switch — is on the site,
 * one `{#id}` section per subject, and a short line that leaves something out
 * ends with a link to the section that has it.
 */

const DOCUMENTATION_ORIGIN = "https://mewrk.dev";

/** The site's pages: `content/<locale>/<page>.md` in the Mewrk-site repository. */
export type DocsPage =
  | "index" | "providers" | "workspaces" | "working" | "context" | "security"
  | "agents" | "preview" | "web" | "memory" | "prompt-profiles"
  | "skills" | "mcp" | "hooks" | "lsp" | "tools" | "settings" | "privacy";

/** Configuration surfaces whose page the site names differently. */
const SURFACE_PAGES = {
  features: { page: "working", section: "conversation-settings" },
  toolDescriptions: { page: "prompt-profiles", section: undefined },
  agents: { page: "agents", section: "roles" }
} as const satisfies Record<string, { page: DocsPage; section: string | undefined }>;

export type DocsTarget = DocsPage | keyof typeof SURFACE_PAGES;

function documentationLocale(resolvedLanguage: ResolvedAppLanguage | string): string {
  return resolvedLanguage === "zh-CN" ? "zh-CN" : "en";
}

/** The address of one page of the site, or of one section of it. */
export function docsHref(
  resolvedLanguage: ResolvedAppLanguage | string,
  target: DocsTarget,
  section?: string
): string {
  const surface = Object.hasOwn(SURFACE_PAGES, target)
    ? SURFACE_PAGES[target as keyof typeof SURFACE_PAGES]
    : undefined;
  const page = surface?.page ?? (target as DocsPage);
  const anchor = section ?? surface?.section;
  return `${DOCUMENTATION_ORIGIN}/${documentationLocale(resolvedLanguage)}/${page}.html${anchor ? `#${anchor}` : ""}`;
}

/**
 * A link to the documentation.
 *
 * By default it is the one-line "Configuration docs" link a page's heading row
 * ends with. `inline` makes it the "Learn more" that trails a short
 * description, in the description's own size. It is a bare `<a>` with no
 * wrapper, so it sits in whatever line it is dropped into — never inside a
 * `<button>`, where a link is neither valid nor operable.
 */
export function DocsLink({
  page,
  section,
  inline = false
}: {
  page: DocsTarget;
  /** A heading's `{#id}` on that page. */
  section?: string;
  inline?: boolean;
}) {
  const { t, resolvedLanguage } = useI18n();
  const href = docsHref(resolvedLanguage, page, section);
  if (inline) {
    return (
      <a className="docs-link docs-link--inline" href={href} target="_blank" rel="noreferrer noopener">
        {t("详情", "Learn more")}
        <ExternalLink size={9} aria-hidden="true" />
      </a>
    );
  }
  return (
    <a className="docs-link" href={href} target="_blank" rel="noreferrer noopener">
      <ExternalLink size={11} aria-hidden="true" />
      {t("配置说明文档", "Configuration docs")}
    </a>
  );
}

/**
 * The way in to one tool's documentation, in the slot a group heading puts its
 * icon.
 *
 * It is an anchor and not a button because it leaves the application, and it is
 * a SIBLING of the tool's own toggle rather than a child of it: the row is
 * itself a button, and a link inside a button is neither valid nor operable.
 */
function ToolDocsAnchor({ path, label }: { path: string; label: string }) {
  const { t, resolvedLanguage } = useI18n();
  const locale = documentationLocale(resolvedLanguage);
  return (
    <a
      className="tool-docs-link"
      href={`${DOCUMENTATION_ORIGIN}/${locale}/${path}`}
      target="_blank"
      rel="noreferrer noopener"
      aria-label={t("{label}的说明文档", "Documentation for {label}", { label })}
      title={t("{label}的说明文档", "Documentation for {label}", { label })}
    >
      <BookOpen size={14} aria-hidden="true" />
    </a>
  );
}

/**
 * One tool's own page. The site publishes one per catalog tool at
 * `<locale>/tools/<name>.html` (built from this repository's tool catalog,
 * so every tool name in the picker has a page).
 */
export function ToolDocsLink({ name, label }: { name: string; label: string }) {
  return <ToolDocsAnchor path={`tools/${encodeURIComponent(name)}.html`} label={label} />;
}
