import type { CSSProperties, JSX } from "react";
import { codeThemeVariables } from "../../lib/codeThemes";
import type { CodeTheme } from "../../lib/codeThemes";
import { parseUnifiedDiff } from "../../lib/unifiedDiff";
import { UnifiedHunk } from "../DiffViewer";

/**
 * What every preview shows: a few lines with one rewritten, so a theme's syntax colours,
 * its bands for a removed and an added line, and its mark for the words that changed are
 * all in view at once.
 */
const SAMPLE_HUNK = parseUnifiedDiff([
  "diff --git a/greet.ts b/greet.ts",
  "--- a/greet.ts",
  "+++ b/greet.ts",
  "@@ -1,4 +1,4 @@",
  " // Greets a visitor by name.",
  " function greet(name: string, times = 1) {",
  "-  return \"Hello, \" + name;",
  // biome-ignore lint/suspicious/noTemplateCurlyInString: the sample is source text, and this line of it is a template literal.
  "+  return `Hello, ${name}!`.repeat(times);",
  " }",
  ""
].join("\n"))[0].hunks[0];

/**
 * A code theme in miniature: the review pane's own diff (`DiffViewer.tsx`), drawn in the
 * theme's colours whichever theme is on screen. The variables the window reads from the
 * root are set here on the preview instead, so the same stylesheet draws it.
 *
 * A theme that keeps the app's surface is shown on what that surface is in its scheme:
 * the window's well, or with `glass` the veil liquid glass makes of it.
 */
export function CodeThemePreview({ theme, glass }: { theme: CodeTheme; glass: boolean }): JSX.Element {
  const variables = codeThemeVariables(theme, theme.adaptive && glass ? "glass" : "theme");
  const style: Record<string, string> = {};
  for (const [name, value] of Object.entries(variables)) style[`--code-${name}`] = value;
  return (
    <div className="code-theme-preview" data-scheme={theme.scheme} style={style as CSSProperties} aria-hidden="true">
      <div className="diff-viewer__code diff-viewer__code--unified">
        <UnifiedHunk hunk={SAMPLE_HUNK} wordDiff language="typescript" />
      </div>
    </div>
  );
}
