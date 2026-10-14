import { describe, expect, it } from "vitest";
import manifest from "virtual:mewrk-file-icons";
import { KINDRED_ICONS, fileExtension, fileIconName, fileIconUrl, lightFileIconName } from "./fileIcons";

describe("fileIconName", () => {
  it("draws a source file as its language", () => {
    expect(fileIconName("src-tauri/src/lib.rs")).toBe("rust");
    expect(fileIconName("src/App.ts")).toBe("typescript");
    expect(fileIconName("docs/formal-methods.md")).toBe("markdown");
    expect(fileIconName("scripts/build.py")).toBe("python");
  });

  it("gives media and models an icon of their own", () => {
    expect(fileIconName("clip.mp4")).toBe("video");
    expect(fileIconName("take.flac")).toBe("audio");
    expect(fileIconName("shot.webp")).toBe("image");
    expect(fileIconName("scene.glb")).toBe("3d");
    expect(fileIconName("bundle.7z")).toBe("zip");
  });

  it("prefers a whole file name to its extension", () => {
    expect(fileIconName("package.json")).toBe("nodejs");
    expect(fileIconName("tsconfig.json")).toBe("tsconfig");
    expect(fileIconName("skills/review/SKILL.md")).toBe("skill");
    expect(fileIconName("Dockerfile")).toBe("docker");
    expect(fileIconName(".gitignore")).toBe("git");
  });

  it("reads a name qualified by the directory above it", () => {
    expect(fileIconName("repo/.config/postcssrc.yml")).toBe("postcss");
    expect(fileIconName("repo/config/postcssrc.yml")).toBe("yaml");
  });

  it("prefers the longest extension", () => {
    expect(fileIconName("types/index.d.ts")).toBe("typescript-def");
    expect(fileIconName("src/App.test.tsx")).toBe("test-jsx");
  });

  it("counts a dotfile's name as an extension, as the theme's tables do", () => {
    expect(fileIconName(".env")).toBe(manifest.extensions.env);
  });

  it("ignores case and accepts either separator", () => {
    expect(fileIconName("C:\\Repo\\SRC\\Main.RS")).toBe("rust");
    expect(fileIconName("README.MD")).toBe("readme");
  });

  it("falls back to a type's kin, then to the plain file", () => {
    expect(fileIconName("clip.3gp")).toBe("video");
    expect(fileIconName("formal/tla/AgentKernel.tla")).toBe(manifest.fallback);
    expect(fileIconName("")).toBe(manifest.fallback);
  });

  it("is not fooled by names every object answers to", () => {
    expect(fileIconName("constructor")).toBe(manifest.fallback);
    expect(fileIconName("notes.__proto__")).toBe(manifest.fallback);
    expect(fileIconName("x.toString")).toBe(manifest.fallback);
  });
});

describe("kindred icons", () => {
  it("names only icons the theme publishes", () => {
    const published = new Set([...Object.values(manifest.names), ...Object.values(manifest.extensions)]);
    expect(Object.keys(KINDRED_ICONS).filter((icon) => !published.has(icon))).toEqual([]);
  });
});

describe("lightFileIconName", () => {
  it("offers a second drawing only where the theme has one", () => {
    expect(manifest.light).toContain("toml");
    expect(lightFileIconName(fileIconName("Cargo.toml"))).toBe("toml_light");
    expect(lightFileIconName("rust")).toBeNull();
  });
});

describe("fileIconUrl", () => {
  it("addresses the route the icons are published under", () => {
    expect(fileIconUrl("rust")).toBe("/file-icons/rust.svg");
  });
});

describe("fileExtension", () => {
  it("is the lowercased last extension, and nothing for a bare dotfile", () => {
    expect(fileExtension("Photo.JPG")).toBe("jpg");
    expect(fileIconName("archive.tar.gz")).toBe("zip");
    expect(fileExtension("archive.tar.gz")).toBe("gz");
    expect(fileExtension(".gitignore")).toBe("");
    expect(fileExtension("Makefile")).toBe("");
  });
});
