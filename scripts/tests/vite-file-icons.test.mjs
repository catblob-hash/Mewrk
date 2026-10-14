import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";

import {
  FILE_ICON_MODULE,
  FILE_ICON_ROUTE,
  distillFileIconTheme,
  fileIconsPlugin,
  readFileIconTheme
} from "../vite-file-icons.mjs";

function theme(overrides = {}) {
  return {
    file: "file",
    iconDefinitions: {
      file: { iconPath: "./../icons/file.svg" },
      rust: { iconPath: "./../icons/rust.svg" },
      toml: { iconPath: "./../icons/toml.svg" },
      toml_light: { iconPath: "./../icons/toml_light.svg" },
      latex: { iconPath: "./../icons/latex.clone.svg" },
      "folder-src": { iconPath: "./../icons/folder-src.svg" }
    },
    fileExtensions: { rs: "rust", toml: "toml", tmLanguage: "latex" },
    fileNames: { PKGBUILD: "rust", ".config/rustfmt.toml": "toml" },
    folderNames: { src: "folder-src" },
    light: { fileExtensions: { toml: "toml_light" }, fileNames: { ".config/rustfmt.toml": "toml_light" } },
    ...overrides
  };
}

/** What the plugin's dev middleware answers a request with, or `"next"` when it passes it on. */
function request(plugin, url, method = "GET") {
  let middleware;
  plugin.configureServer({ middlewares: { use: (handler) => { middleware = handler; } } });
  const response = {
    statusCode: 200,
    headers: {},
    body: undefined,
    setHeader(name, value) { this.headers[name] = value; },
    end(body) { this.body = body; this.ended = true; }
  };
  let passed = false;
  middleware({ url, method }, response, () => { passed = true; });
  return passed ? "next" : response;
}

test("the manifest's file half is distilled into lowercased tables and the icons they name", () => {
  const { manifest, files } = distillFileIconTheme(theme());

  assert.deepEqual(manifest, {
    fallback: "file",
    names: { pkgbuild: "rust", ".config/rustfmt.toml": "toml" },
    extensions: { rs: "rust", toml: "toml", tmlanguage: "latex" },
    light: ["toml"],
    nameDepth: 2
  });
  // A light variant is published beside its icon; a folder icon is not a file's and is left out.
  assert.deepEqual(Object.keys(files).sort(), ["file", "latex", "rust", "toml", "toml_light"]);
  // An icon is published under its own name wherever the theme keeps its drawing.
  assert.equal(files.latex, "./../icons/latex.clone.svg");
});

test("a light variant that is not its icon's `_light` twin is refused", () => {
  assert.throws(
    () => distillFileIconTheme(theme({ light: { fileExtensions: { toml: "rust" } } })),
    /toml/
  );
  assert.throws(
    () => distillFileIconTheme(theme({ light: { fileExtensions: { md: "toml_light" } } })),
    /toml_light/
  );
});

test("an icon the theme names but does not define, or one no URL could carry, is refused", () => {
  assert.throws(() => distillFileIconTheme(theme({ fileExtensions: { rs: "missing" }, light: {} })), /missing/);
  assert.throws(() => distillFileIconTheme(theme({ fileExtensions: { rs: "constructor" }, light: {} })), /constructor/);
  assert.throws(
    () => distillFileIconTheme(theme({
      fileExtensions: { rs: "../rust" },
      iconDefinitions: { file: { iconPath: "./file.svg" }, "../rust": { iconPath: "./rust.svg" } },
      fileNames: {},
      light: {}
    })),
    /\.\.\/rust/
  );
});

test("the installed theme distills, and every icon it publishes is a drawing on disk", () => {
  const { manifest, files } = readFileIconTheme();

  assert.equal(manifest.extensions.rs, "rust");
  assert.equal(manifest.extensions.ts, "typescript");
  assert.equal(manifest.extensions.md, "markdown");
  assert.equal(manifest.names["package.json"], "nodejs");
  assert.ok(files[manifest.fallback]);
  for (const icon of manifest.light) assert.ok(files[`${icon}_light`], `${icon} has no light drawing`);
  for (const [icon, file] of Object.entries(files)) {
    assert.ok(existsSync(file), `${icon} names ${file}, which is not installed`);
  }
  assert.match(readFileSync(files.rust, "utf8"), /^<svg /);
});

test("the virtual module carries the distilled manifest", () => {
  const plugin = fileIconsPlugin();
  const resolved = plugin.resolveId(FILE_ICON_MODULE);

  assert.equal(plugin.resolveId("virtual:something-else"), undefined);
  assert.equal(plugin.load("src/main.tsx"), undefined);
  const source = plugin.load(resolved);
  const literal = /^export default JSON\.parse\((.*)\);$/s.exec(source)?.[1];
  assert.ok(literal, "the module is one JSON.parse of a string literal");
  assert.deepEqual(JSON.parse(JSON.parse(literal)), readFileIconTheme().manifest);
});

test("the dev server answers for a published icon and nothing else under the route", () => {
  const plugin = fileIconsPlugin();
  const { files } = readFileIconTheme();

  const served = request(plugin, `${FILE_ICON_ROUTE}rust.svg?v=1`);
  assert.equal(served.statusCode, 200);
  assert.equal(served.headers["content-type"], "image/svg+xml");
  assert.deepEqual(served.body, readFileSync(files.rust));

  const head = request(plugin, `${FILE_ICON_ROUTE}rust.svg`, "HEAD");
  assert.equal(head.statusCode, 200);
  assert.equal(head.body, undefined);

  assert.equal(request(plugin, `${FILE_ICON_ROUTE}rust.svg`, "POST").statusCode, 405);
  assert.equal(request(plugin, `${FILE_ICON_ROUTE}no-such-icon.svg`).statusCode, 404);
  // A folder icon is in the package but is not one the app publishes.
  assert.equal(request(plugin, `${FILE_ICON_ROUTE}folder-src.svg`).statusCode, 404);
  // A name is looked up, never joined to a directory, so it cannot climb out of one.
  assert.equal(request(plugin, `${FILE_ICON_ROUTE}../package.svg`).statusCode, 404);
  assert.equal(request(plugin, `${FILE_ICON_ROUTE}constructor.svg`).statusCode, 404);

  assert.equal(request(plugin, "/src/main.tsx"), "next");
  assert.equal(request(plugin, `${FILE_ICON_ROUTE}rust.png`), "next");
});

test("a build emits every published icon under the route the renderer asks for", () => {
  const plugin = fileIconsPlugin();
  const { files } = readFileIconTheme();
  const emitted = new Map();

  plugin.generateBundle.call({
    emitFile(asset) {
      assert.equal(asset.type, "asset");
      emitted.set(asset.fileName, asset.source);
    }
  });

  assert.deepEqual([...emitted.keys()].sort(), Object.keys(files).map((icon) => `file-icons/${icon}.svg`).sort());
  assert.deepEqual(emitted.get("file-icons/rust.svg"), readFileSync(files.rust));
});
