/**
 * Which icon stands for a file.
 *
 * Icons and the rules that choose between them are Material Icon Theme's: the set
 * most editors' file trees wear, so a Rust source, a lockfile or a mesh is drawn here
 * the way it is everywhere else. `scripts/vite-file-icons.mjs` distills the theme's
 * manifest into the tables read below and publishes the icons they name.
 */
import manifest from "virtual:mewrk-file-icons";

const NAMES = new Map(Object.entries(manifest.names));
const EXTENSIONS = new Map(Object.entries(manifest.extensions));
const LIGHT = new Set(manifest.light);

/**
 * Types the theme has no rule for, drawn with the icon it gives their nearest kin.
 *
 * Only consulted once the theme's own tables have missed, so a later release of the
 * theme that learns one of these takes over without an edit here.
 */
export const KINDRED_ICONS: Record<string, readonly string[]> = {
  video: ["3gp", "3g2", "f4v", "rmvb", "mxf"],
  audio: ["midi", "amr", "ape", "ac3", "dsf", "caf"],
  image: ["hdr", "jxl", "jp2", "pcx", "arw", "orf", "rw2"],
  "3d": ["usda", "usdc", "x3d", "max", "ma", "mb", "c4d", "splat", "ifc", "vrm", "lwo"],
  zip: ["war", "ear", "asar", "lz", "lz4", "lzma", "cab", "cpio"],
  database: ["avro", "mdb", "accdb", "duckdb", "feather", "arrow"],
  table: ["numbers", "xlsm", "xlsb"],
  word: ["pages", "wps"],
  epub: ["mobi", "azw", "azw3", "fb2"],
  key: ["p12", "pfx", "jks", "keystore"],
  dll: ["dylib"]
};

const KINDRED_EXTENSIONS = new Map(
  Object.entries(KINDRED_ICONS).flatMap(([icon, extensions]) => extensions.map((extension) => [extension, icon] as const))
);

/**
 * Extension of a file name, lowercased.
 *
 * A dotfile with no second dot — `.gitignore`, `.env` — is all name and no
 * extension; treating its name as one would file every dotfile under whatever
 * type its name happened to spell.
 */
export function fileExtension(name: string): string {
  const separator = name.lastIndexOf(".");
  if (separator <= 0) return "";
  return name.slice(separator + 1).toLowerCase();
}

/**
 * Name of the icon for a path, by the theme's own precedence.
 *
 * A whole file name outranks any extension, and may be qualified by the directories
 * above it (`.github/dependabot.yml`). Among extensions the longest wins, so
 * `index.d.ts` is a declaration file before it is TypeScript. Here a dotfile's name
 * does count as an extension — the theme's tables are written that way, `.env` being
 * listed under `env`.
 */
export function fileIconName(path: string): string {
  const segments = path.toLowerCase().split(/[\\/]+/).filter(Boolean);
  for (let depth = Math.min(manifest.nameDepth, segments.length); depth > 0; depth -= 1) {
    const icon = NAMES.get(segments.slice(-depth).join("/"));
    if (icon) return icon;
  }
  const name = segments.at(-1) ?? "";
  for (let dot = name.indexOf("."); dot !== -1; dot = name.indexOf(".", dot + 1)) {
    const icon = EXTENSIONS.get(name.slice(dot + 1));
    if (icon) return icon;
  }
  return KINDRED_EXTENSIONS.get(fileExtension(name)) ?? manifest.fallback;
}

/**
 * The same icon redrawn for a light background, for the few whose usual drawing is
 * too pale to read on one.
 */
export function lightFileIconName(icon: string): string | null {
  return LIGHT.has(icon) ? `${icon}_light` : null;
}

export function fileIconUrl(icon: string): string {
  return `${import.meta.env.BASE_URL}file-icons/${icon}.svg`;
}
