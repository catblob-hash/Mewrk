// Material Icon Theme's file icons, and the table that picks one for a path.
//
// The package is a VS Code extension: it ships one SVG per icon and the manifest VS Code
// reads to choose between them. The renderer can use neither as published — the manifest
// also describes every folder icon and is many times the size of the file half — so this
// plugin distills the file half into a virtual module and publishes the icons it names
// under `/file-icons/`, where an `<img>` may load them (`img-src 'self'`). Everything is
// read from the installed package, so no icon is vendored in this repository.
import fs from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";

export const FILE_ICON_MODULE = "virtual:mewrk-file-icons";
export const FILE_ICON_ROUTE = "/file-icons/";

const RESOLVED_FILE_ICON_MODULE = `\0${FILE_ICON_MODULE}`;
const LIGHT_SUFFIX = "_light";
const ICON_NAME_PATTERN = /^[a-z0-9_-]+$/;

function lowercaseKeys(table = {}) {
  return Object.fromEntries(Object.entries(table).map(([key, icon]) => [key.toLowerCase(), icon]));
}

/**
 * The file half of the theme's manifest, in the shape the renderer reads.
 *
 * VS Code matches names and extensions case-insensitively, so every key is lowercased
 * here once instead of on each lookup. The theme lists its light-background variants as
 * a second pair of tables; they always name the same icon with a `_light` suffix, which
 * is checked rather than assumed so the renderer can carry a flat list of icon names.
 */
export function distillFileIconTheme(theme) {
  const names = lowercaseKeys(theme.fileNames);
  const extensions = lowercaseKeys(theme.fileExtensions);
  const light = new Set();
  for (const [table, lightTable] of [
    [names, lowercaseKeys(theme.light?.fileNames)],
    [extensions, lowercaseKeys(theme.light?.fileExtensions)]
  ]) {
    for (const [key, icon] of Object.entries(lightTable)) {
      const base = Object.hasOwn(table, key) ? table[key] : undefined;
      if (icon !== `${base}${LIGHT_SUFFIX}`) {
        throw new Error(`material-icon-theme 的浅色图标 ${icon}（${key}）不是 ${base} 的 ${LIGHT_SUFFIX} 变体`);
      }
      light.add(base);
    }
  }

  const icons = new Set([theme.file, ...Object.values(names), ...Object.values(extensions)]);
  for (const icon of light) icons.add(`${icon}${LIGHT_SUFFIX}`);
  const files = {};
  for (const icon of icons) {
    // Own properties only: an icon called `constructor` is not one every object defines.
    const file = Object.hasOwn(theme.iconDefinitions, icon) ? theme.iconDefinitions[icon].iconPath : undefined;
    if (!ICON_NAME_PATTERN.test(icon) || typeof file !== "string") {
      throw new Error(`material-icon-theme 的清单引用了无法发布的图标 ${icon}`);
    }
    files[icon] = file;
  }

  return {
    manifest: {
      fallback: theme.file,
      names,
      extensions,
      light: [...light].sort(),
      nameDepth: Math.max(...Object.keys(names).map((name) => name.split("/").length))
    },
    files
  };
}

export function readFileIconTheme() {
  const manifestPath = createRequire(import.meta.url).resolve("material-icon-theme/dist/material-icons.json");
  const { manifest, files } = distillFileIconTheme(JSON.parse(fs.readFileSync(manifestPath, "utf8")));
  // Icon paths in the manifest are relative to the manifest itself.
  const directory = path.dirname(manifestPath);
  return {
    manifest,
    files: Object.fromEntries(Object.entries(files).map(([icon, file]) => [icon, path.resolve(directory, file)]))
  };
}

export function fileIconsPlugin() {
  let theme;
  const load = () => (theme ??= readFileIconTheme());
  return {
    name: "mewrk:file-icons",
    resolveId(id) {
      return id === FILE_ICON_MODULE ? RESOLVED_FILE_ICON_MODULE : undefined;
    },
    load(id) {
      if (id !== RESOLVED_FILE_ICON_MODULE) return undefined;
      return `export default JSON.parse(${JSON.stringify(JSON.stringify(load().manifest))});`;
    },
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const pathname = (request.url ?? "").split(/[?#]/, 1)[0];
        if (!pathname.startsWith(FILE_ICON_ROUTE) || !pathname.endsWith(".svg")) {
          next();
          return;
        }
        const { files } = load();
        const icon = pathname.slice(FILE_ICON_ROUTE.length, -".svg".length);
        const file = Object.hasOwn(files, icon) ? files[icon] : undefined;
        if (!file || (request.method !== "GET" && request.method !== "HEAD")) {
          response.statusCode = file ? 405 : 404;
          response.end();
          return;
        }
        response.setHeader("content-type", "image/svg+xml");
        // An icon only changes when the package does, and that restarts this server.
        response.setHeader("cache-control", "max-age=3600");
        response.end(request.method === "HEAD" ? undefined : fs.readFileSync(file));
      });
    },
    generateBundle() {
      for (const [icon, file] of Object.entries(load().files)) {
        this.emitFile({
          type: "asset",
          fileName: `${FILE_ICON_ROUTE.slice(1)}${icon}.svg`,
          source: fs.readFileSync(file)
        });
      }
    }
  };
}
