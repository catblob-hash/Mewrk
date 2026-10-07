// Package the AI SDK sidecar as a single executable.
//
//   node scripts/build-aisdk-sidecar.mjs
//
// The artifact is `aisdk-service/dist/mewrk-aisdk.exe` (`mewrk-aisdk` off Windows). The installer does not carry it: `npm run publish:components -- --aisdk` puts it on Mewrk's component channel, and the app fetches the build for its protocol generation at every start (`src-tauri/src/components/aisdk.rs`). A development build of the app runs this file straight from `aisdk-service/dist/`.
//
// Keep this separate from the frontend build because rebuilding the 90 MiB sidecar is comparatively expensive. `tauri.conf.json` no longer runs it: a release builds it once, for `publish:components`.

import { execFileSync } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const service = join(root, "aisdk-service");

function fail(message) {
  console.error(`[aisdk-sidecar] ${message}`);
  process.exit(1);
}

if (!existsSync(join(service, "node_modules"))) {
  fail("aisdk-service/node_modules 不存在；先在 aisdk-service/ 里 `npm install`");
}

try {
  execFileSync(process.execPath, [join(service, "build.mjs"), "--sea"], {
    cwd: service,
    stdio: "inherit",
  });
} catch (error) {
  fail(`侧车打包失败：${error.message}`);
}

const built = join(service, "dist", process.platform === "win32" ? "mewrk-aisdk.exe" : "mewrk-aisdk");
if (!existsSync(built)) {
  fail(`侧车产物不存在：${built}`);
}

console.log(`[aisdk-sidecar] ${built} — ${(statSync(built).size / 1024 / 1024).toFixed(1)} MiB`);
console.log("[aisdk-sidecar] 开发构建直接运行这个文件；发布时用 `npm run publish:components -- --aisdk` 把它传到组件频道。");
