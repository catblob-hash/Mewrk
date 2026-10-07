import { execFileSync } from "node:child_process";
import { existsSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Signs the macOS builds of the remote agent before Tauri copies the bundled one into
 * `Contents/Resources/remote-agents`:
 *
 * - `src-tauri/bundled-agents/*-apple-darwin/mewrk-remote`, the one build the app carries (this
 *   Mac's own, which its sandboxed commands run through). Tauri signs only the binaries it builds,
 *   not resources, and notarization rejects a bundle holding an unsigned Mach-O.
 * - `src-tauri/remote-agents/*-apple-darwin/mewrk-remote`, the builds `npm run publish:components
 *   -- --remote-agents` puts on Mewrk's channel for other Macs to fetch, so they carry the same
 *   Developer ID signature as the one in the app.
 *
 * The signature goes into the file the app hashes, so the digest the app expects and the one the
 * uploaded agent reports (`platform::self_digest`) stay the same build — sign before publishing.
 *
 * Tauri runs this as part of `beforeBundleCommand`; without APPLE_SIGNING_IDENTITY it does nothing.
 */

const crate = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri");
const signingIdentity = process.env.APPLE_SIGNING_IDENTITY;

if (process.platform !== "darwin" || !signingIdentity) process.exit(0);

for (const directory of ["bundled-agents", "remote-agents"]) {
  const agents = join(crate, directory);
  if (!existsSync(agents)) continue;
  for (const triple of readdirSync(agents).filter((name) => name.endsWith("-apple-darwin"))) {
    const agent = join(agents, triple, "mewrk-remote");
    if (!existsSync(agent)) continue;
    execFileSync(
      "codesign",
      ["--force", "--timestamp", "--options", "runtime", "--sign", signingIdentity, agent],
      { stdio: "inherit" }
    );
    console.log(`[remote-agents] 已签名 ${directory}/${triple}`);
  }
}
