// What Mewrk's own component channel holds and how it is written. Pure
// functions only; scripts/publish-components.mjs does the file and network work.
//
// The channel is the `components/` prefix of the R2 bucket behind
// https://dl.mewrk.dev (the app reads it as `https://dl.mewrk.dev/components`,
// or `MEWRK_COMPONENTS_MIRROR`). Channel keys below are relative to that base:
//
//   aisdk/p<PROTOCOL>/<triple>.json                              pointer, kept 60 s
//   aisdk/p<PROTOCOL>/<triple>/<gz sha256>/mewrk-aisdk[.exe].gz   gzip, immutable
//   remote-agent/<SOURCE_ID>/<triple>.json                       pointer, kept 60 s
//   remote-agent/<SOURCE_ID>/<triple>/<gz sha256>/<name>.gz      gzip, immutable
//                                  (mewrk-remote[.exe], and srt-win.exe on Windows)
//
// A pointer describes one build and names its files relative to the pointer's
// own directory. A blob is stored under the SHA-256 of its compressed bytes, so
// a published blob never changes; only pointers are ever replaced. The reader is
// src-tauri/src/components/mod.rs: `validatePointer` below is its
// `Pointer::validate`, so a pointer this module writes is one the app accepts.

import { createHash } from "node:crypto";
import zlib from "node:zlib";

import { MIRROR } from "./release-mirror-plan.mjs";

/** The pointer format the app reads (`POINTER_SCHEMA` in components/mod.rs). */
export const POINTER_SCHEMA = 1;
/** Where the channel lives in the bucket. */
export const BUCKET_PREFIX = "components/";
/** Where the channel is served: the app's `CHANNEL`. */
export const CHANNEL_URL = `${MIRROR.origin}/components`;
export const GZIP_LEVEL = 9;
/** No component file is anywhere near this (`FILE_LIMIT` in components/mod.rs). */
export const FILE_LIMIT = 2 ** 30;
/** The Windows sandbox helper, published beside the agent. */
export const AGENT_HELPER = "srt-win.exe";

/**
 * Every platform the agent is built for, and so every agent pointer a complete
 * release has on the channel. scripts/build-remote-agents.mjs builds exactly
 * these; `--check` of scripts/publish-components.mjs looks for exactly these.
 */
export const AGENT_TARGETS = Object.freeze([
  "x86_64-unknown-linux-musl",
  "aarch64-unknown-linux-musl",
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-pc-windows-msvc",
  "aarch64-pc-windows-msvc",
]);

/** The platforms whose sidecar `--check` looks for unless `--triple` names others: the two the app is released for. */
export const DEFAULT_CHECK_TRIPLES = Object.freeze(["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]);

/** A blob never changes, so the CDN and browsers keep it for good. */
export const BLOB_HEADERS = Object.freeze({
  "cache-control": "public, max-age=31536000, immutable",
  "content-type": "application/gzip",
});

/** A pointer is what changes with a release, so nothing keeps it for more than a minute. */
export const POINTER_HEADERS = Object.freeze({
  "cache-control": "public, max-age=60",
  "content-type": "application/json; charset=utf-8",
});

const SHA256_PATTERN = /^[0-9a-f]{64}$/;
const TRIPLE_PATTERN = /^[a-z0-9_]+(?:-[a-z0-9_]+){2,3}$/;

// ---------------------------------------------------------------------------
// Names and keys

/** `[A-Za-z0-9._-]`, at most 128, not starting with a dot (`is_plain_name` in components/mod.rs). */
export function isPlainName(name) {
  return typeof name === "string" && name.length > 0 && name.length <= 128 && !name.startsWith(".") && /^[A-Za-z0-9._-]+$/u.test(name);
}

/** `a/b/c` made of plain names only (`is_relative_path` in components/mod.rs). */
export function isRelativePath(path) {
  return typeof path === "string" && path.length > 0 && path.length <= 512 && path.split("/").every(isPlainName);
}

export const isSha256 = (text) => typeof text === "string" && SHA256_PATTERN.test(text);

export const isWindowsTriple = (triple) => triple.includes("-windows-");

/** Throws unless `triple` looks like a Rust target triple the channel can carry in a key. */
export function assertTriple(triple) {
  if (typeof triple !== "string" || !TRIPLE_PATTERN.test(triple)) throw new TypeError(`not a target triple: ${triple}`);
  return triple;
}

/** `mewrk-aisdk` or `mewrk-aisdk.exe`: the sidecar's file name on that platform. */
export function aisdkFileName(triple) {
  return isWindowsTriple(assertTriple(triple)) ? "mewrk-aisdk.exe" : "mewrk-aisdk";
}

/** The agent's file names in the order a pointer lists them: the agent, then the Windows sandbox helper. */
export function agentFileNames(triple) {
  return isWindowsTriple(assertTriple(triple)) ? ["mewrk-remote.exe", AGENT_HELPER] : ["mewrk-remote"];
}

export function aisdkPointerKey(protocol, triple) {
  if (!Number.isInteger(protocol) || protocol < 1) throw new TypeError(`not a protocol generation: ${protocol}`);
  return `aisdk/p${protocol}/${assertTriple(triple)}.json`;
}

export function agentPointerKey(source, triple) {
  if (!isSha256(source)) throw new TypeError(`not an agent source id: ${source}`);
  return `remote-agent/${source}/${assertTriple(triple)}.json`;
}

/** The directory a pointer's `files[].path` is relative to: `aisdk/p17`. */
export function pointerDirectory(pointerKey) {
  const at = pointerKey.lastIndexOf("/");
  if (at <= 0) throw new TypeError(`not a pointer key: ${pointerKey}`);
  return pointerKey.slice(0, at);
}

/** A file's path in its pointer: `<triple>/<gz sha256>/<name>.gz`. */
export function blobPath(triple, gzSha256, name) {
  if (!isSha256(gzSha256)) throw new TypeError(`not a SHA-256: ${gzSha256}`);
  if (!isPlainName(name)) throw new TypeError(`not a plain file name: ${name}`);
  return `${assertTriple(triple)}/${gzSha256}/${name}.gz`;
}

/** The channel key of the blob that `file` (an entry of `pointer.files`) names. */
export function blobKey(pointerKey, file) {
  return `${pointerDirectory(pointerKey)}/${file.path}`;
}

/** The bucket key of a channel key. */
export const bucketKey = (channelKey) => `${BUCKET_PREFIX}${channelKey}`;

/** The public address of a channel key. */
export const publicUrl = (channelKey) => `${CHANNEL_URL}/${channelKey}`;

// ---------------------------------------------------------------------------
// Compressing and describing files

const sha256Hex = (data) => createHash("sha256").update(data).digest("hex");

/**
 * Compresses one file for the channel. `entry` is what a pointer records about
 * it, minus `path` (which `buildPointer` derives from the digest); `gz` is the
 * blob to store.
 */
export function packFile(name, bytes) {
  if (!isPlainName(name)) throw new TypeError(`not a plain file name: ${name}`);
  if (!bytes.length) throw new Error(`${name} is empty`);
  const gz = zlib.gzipSync(bytes, { level: GZIP_LEVEL });
  return {
    entry: {
      name,
      size: gz.length,
      sha256: sha256Hex(gz),
      unpackedSize: bytes.length,
      unpackedSha256: sha256Hex(bytes),
    },
    gz,
  };
}

/**
 * Builds a pointer. `files` are `packFile` entries, in the order the pointer
 * lists them (the first one names the build); `extra` carries what only that
 * component reads: `{ protocol, claudeAgentSdk }` for the sidecar,
 * `{ source }` for an agent. Throws unless the app would accept the result.
 */
export function buildPointer({ component, triple, version, builtAt, extra = {}, files }) {
  const reserved = ["schema", "component", "triple", "version", "builtAt", "files"];
  for (const key of Object.keys(extra)) {
    if (reserved.includes(key)) throw new Error(`extra may not set ${key}`);
  }
  const pointer = {
    schema: POINTER_SCHEMA,
    component,
    triple,
    version,
    builtAt,
    ...extra,
    files: files.map((file) => ({
      name: file.name,
      path: blobPath(triple, file.sha256, file.name),
      size: file.size,
      sha256: file.sha256,
      unpackedSize: file.unpackedSize,
      unpackedSha256: file.unpackedSha256,
    })),
  };
  validatePointer(pointer, component, triple);
  validatePublishable(pointer);
  return pointer;
}

/** A pointer as it is stored: two-space JSON and a final newline. */
export function pointerJson(pointer) {
  return `${JSON.stringify(pointer, null, 2)}\n`;
}

// ---------------------------------------------------------------------------
// Validation

/**
 * `Pointer::validate` in src-tauri/src/components/mod.rs: refuses a pointer
 * that names files outside its own directory, that is not for
 * `component`/`triple`, or that the app cannot read. Throws the app's reason.
 */
export function validatePointer(pointer, component, triple) {
  if (!pointer || typeof pointer !== "object") throw new Error("pointer is not an object");
  if (pointer.schema !== POINTER_SCHEMA) throw new Error(`pointer schema ${pointer.schema} is not ${POINTER_SCHEMA}`);
  if (pointer.component !== component || pointer.triple !== triple) {
    throw new Error(`pointer is for ${pointer.component}/${pointer.triple}, not ${component}/${triple}`);
  }
  // `version` is a required string of the app's `Pointer`; `builtAt` is optional there.
  if (typeof pointer.version !== "string" || pointer.version === "") throw new Error("pointer has no version");
  if (pointer.builtAt !== undefined && typeof pointer.builtAt !== "string") throw new Error("pointer builtAt is not a string");
  if (!Array.isArray(pointer.files) || pointer.files.length === 0) throw new Error("pointer lists no files");
  const names = new Set();
  for (const file of pointer.files) {
    if (!file || typeof file !== "object") throw new Error("pointer file is not an object");
    if (!isPlainName(file.name) || names.has(file.name)) throw new Error(`pointer file name ${JSON.stringify(file.name)} is not a plain, unique name`);
    names.add(file.name);
    if (!isRelativePath(file.path)) throw new Error(`pointer path ${JSON.stringify(file.path)} is not inside its directory`);
    if (!isSha256(file.sha256) || !isSha256(file.unpackedSha256)) throw new Error(`pointer digests of ${file.name} are not SHA-256`);
    for (const size of [file.size, file.unpackedSize]) {
      if (!Number.isInteger(size) || size < 1 || size > FILE_LIMIT) throw new Error(`pointer sizes of ${file.name} are out of range`);
    }
  }
}

/**
 * What a publisher also insists on, beyond what the app checks: the
 * component's own fields, the file names the app looks for, and the
 * content-addressed path. Throws on the first violation.
 */
export function validatePublishable(pointer) {
  const { component, triple, files } = pointer;
  if (component === "aisdk") {
    if (!Number.isInteger(pointer.protocol) || pointer.protocol < 1) throw new Error("an aisdk pointer needs the sidecar's protocol");
    if (!/^\d+\.\d+\.\d+$/u.test(pointer.claudeAgentSdk ?? "")) throw new Error("an aisdk pointer needs the exact claudeAgentSdk version it was built against");
    if (files.map((file) => file.name).join() !== aisdkFileName(triple)) throw new Error(`an aisdk pointer for ${triple} lists exactly ${aisdkFileName(triple)}`);
  } else if (component === "remote-agent") {
    if (!isSha256(pointer.source)) throw new Error("a remote-agent pointer needs the agent's source id");
    if (files.map((file) => file.name).join() !== agentFileNames(triple).join()) {
      throw new Error(`a remote-agent pointer for ${triple} lists exactly ${agentFileNames(triple).join(", ")}, in that order`);
    }
  } else {
    throw new Error(`unknown component ${component}`);
  }
  for (const file of files) {
    if (file.path !== blobPath(triple, file.sha256, file.name)) throw new Error(`${file.name} is not stored at ${blobPath(triple, file.sha256, file.name)}`);
  }
}

// ---------------------------------------------------------------------------
// Facts read from the source tree

/** `PROTOCOL_VERSION` of aisdk-service/src/protocol.ts. */
export function parseProtocolTs(text) {
  const match = /export\s+const\s+PROTOCOL_VERSION\s*=\s*(\d+)\s*;/u.exec(text);
  if (!match) throw new Error("aisdk-service/src/protocol.ts declares no PROTOCOL_VERSION");
  return Number(match[1]);
}

/** `PROTOCOL_VERSION` of src-tauri/src/aisdk/protocol.rs. */
export function parseProtocolRs(text) {
  const match = /const\s+PROTOCOL_VERSION\s*:\s*u32\s*=\s*(\d+)\s*;/u.exec(text);
  if (!match) throw new Error("src-tauri/src/aisdk/protocol.rs declares no PROTOCOL_VERSION");
  return Number(match[1]);
}

/** The protocol generation both ends declare; they must agree, or no sidecar built from this tree could start. */
export function protocolVersion(tsText, rsText) {
  const sidecar = parseProtocolTs(tsText);
  const host = parseProtocolRs(rsText);
  if (sidecar !== host) throw new Error(`the sidecar speaks protocol ${sidecar} but the host speaks ${host} (aisdk-service/src/protocol.ts, src-tauri/src/aisdk/protocol.rs)`);
  return sidecar;
}

export const CLAUDE_AGENT_SDK = "@anthropic-ai/claude-agent-sdk";

/**
 * The exact `@anthropic-ai/claude-agent-sdk` version aisdk-service pins, in
 * `dependencies` or `devDependencies` (src-tauri/build.rs reads the same pin).
 */
export function claudeAgentSdkPin(packageJson) {
  const manifest = typeof packageJson === "string" ? JSON.parse(packageJson) : packageJson;
  const found = [manifest?.dependencies, manifest?.devDependencies]
    .map((group) => group?.[CLAUDE_AGENT_SDK])
    .filter((version) => version !== undefined);
  if (found.length === 0) throw new Error(`aisdk-service/package.json does not declare ${CLAUDE_AGENT_SDK}`);
  if (new Set(found).size > 1) throw new Error(`aisdk-service/package.json declares ${CLAUDE_AGENT_SDK} twice, as ${found.join(" and ")}`);
  if (typeof found[0] !== "string" || !/^\d+\.\d+\.\d+$/u.test(found[0])) {
    throw new Error(`${CLAUDE_AGENT_SDK} must be pinned to an exact version such as 0.3.284, not ${JSON.stringify(found[0])}`);
  }
  return found[0];
}

/** The host triple `rustc -vV` reports. */
export function parseHostTriple(rustcVersionText) {
  const line = rustcVersionText.split(/\r?\n/u).find((entry) => entry.startsWith("host: "));
  if (!line) throw new Error("rustc -vV did not report a host triple");
  return line.slice("host: ".length).trim();
}

const SOURCE_MARKER = Buffer.from("mewrk-remote-source:");

/**
 * The agent source a build was made from, read from its bytes (every build
 * carries `mewrk-remote-source:<sha256>`, see src-tauri/remote-agent/build.rs);
 * null for one from before source identities.
 */
export function agentSource(bytes) {
  for (let at = bytes.indexOf(SOURCE_MARKER); at >= 0; at = bytes.indexOf(SOURCE_MARKER, at + 1)) {
    const start = at + SOURCE_MARKER.length;
    const id = bytes.subarray(start, start + 64).toString("latin1");
    if (SHA256_PATTERN.test(id)) return id;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Which staged agent builds to publish

/**
 * The triple of the agent this computer runs its own sandboxed commands
 * through: its host triple, except that a glibc Linux host runs the static
 * musl build (the app prefers it everywhere, see `triples_for` in
 * src-tauri/src/remote_link.rs).
 */
export function ownAgentTriple(host) {
  return host.replace(/-linux-gnu$/u, "-linux-musl");
}

/**
 * The agent source of this computer's own build (`reference`, see
 * `ownAgentTriple`) among `builds` (`{ triple, source, files }`): the id every
 * published build must have and the one `--check` looks for on the channel.
 * Returns `{ source, problem: null, message: "" }`, or `{ source: null,
 * problem, message }` with `problem` "missing" (no such build here) or
 * "unidentified" (a build from before source identities).
 */
export function ownAgentSource({ reference, builds }) {
  const own = builds.find((build) => build.triple === reference);
  if (!own || !own.files.includes(agentFileNames(reference)[0])) {
    return {
      source: null,
      problem: "missing",
      message: `there is no ${reference} agent build to tell the current agent source by; build it first (npm run build:remote-agents -- --release, or --only ${reference})`,
    };
  }
  if (!own.source) {
    return { source: null, problem: "unidentified", message: `the ${reference} agent build carries no source id: it was made before source identities` };
  }
  return { source: own.source, problem: null, message: "" };
}

/**
 * Chooses the builds of `src-tauri/remote-agents/` that go on the channel.
 * `builds` are `{ triple, source, files }` — the source id read from the
 * agent's bytes, and the names of the files in its directory. The build for
 * `reference` (this computer's own, `ownAgentTriple(host)`) names the source
 * every other one must have been made from: a build of other source would be
 * published under an id it does not have, and an agent left staged by an older
 * checkout must not be mistaken for the current one. Those are skipped and
 * listed, never published; so is a Windows build without its sandbox helper.
 * With `only`, just that triple is wanted, and a reason to skip it is an error.
 *
 * Returns `{ source, publish, skipped }`: `publish` is the triples, sorted.
 */
export function selectAgentBuilds({ reference, builds, only = null }) {
  const current = ownAgentSource({ reference, builds });
  if (!current.source) throw new Error(current.message);
  const own = { source: current.source };
  const publish = [];
  const skipped = [];
  for (const build of [...builds].sort((left, right) => (left.triple < right.triple ? -1 : left.triple > right.triple ? 1 : 0))) {
    if (only && build.triple !== only) continue;
    const reason = skipReason(build, own.source);
    if (reason) skipped.push({ triple: build.triple, reason });
    else publish.push(build.triple);
  }
  if (only) {
    const refused = skipped.find((entry) => entry.triple === only);
    if (refused) throw new Error(`the ${only} agent build cannot be published: ${refused.reason}`);
    if (!publish.includes(only)) throw new Error(`there is no ${only} agent build in src-tauri/remote-agents`);
  }
  return { source: own.source, publish, skipped };
}

function skipReason(build, source) {
  if (!TRIPLE_PATTERN.test(build.triple)) return "not a target triple";
  const [agent, ...rest] = agentFileNames(build.triple);
  if (!build.files.includes(agent)) return `no ${agent}`;
  if (!build.source) return "made before agent source identities";
  if (build.source !== source) return `made from other agent source ${build.source.slice(0, 12)}, not the current ${source.slice(0, 12)}`;
  const absent = rest.find((name) => !build.files.includes(name));
  if (absent) return `no ${absent} beside it, so Windows would have no sandbox`;
  return null;
}

// ---------------------------------------------------------------------------
// The sidecar's start-up handshake (src-tauri/src/aisdk/process.rs, aisdk-service/src/main.ts)

/** The first frame the host sends: `hello`. One NDJSON line. */
export function helloFrame(protocol) {
  return `${JSON.stringify({ v: protocol, type: "hello" })}\n`;
}

/** The last frame the host sends. */
export function shutdownFrame(protocol) {
  return `${JSON.stringify({ v: protocol, type: "shutdown" })}\n`;
}

/**
 * What one sidecar stdout line says: `{ type: "ready", protocol }` for the
 * handshake answer, `{ type }` for any other frame, null for a line that is
 * not a frame.
 */
export function parseSidecarLine(line) {
  let frame;
  try {
    frame = JSON.parse(line);
  } catch {
    return null;
  }
  if (!frame || typeof frame !== "object" || typeof frame.type !== "string") return null;
  return frame.type === "ready" ? { type: "ready", protocol: frame.protocol } : { type: frame.type };
}

// ---------------------------------------------------------------------------
// Whether to publish, and checking what was written

/** The SHA-256 that names a build (`Pointer::id`): its first file, unpacked. */
export const pointerId = (pointer) => pointer?.files?.[0]?.unpackedSha256 ?? null;

const SEMVER_PATTERN =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/u;

/** A semantic version as `{ numbers: [major, minor, patch], prerelease: string[] }` (build metadata is dropped, it never counts); null when `text` is not one. */
export function parseSemver(text) {
  const match = typeof text === "string" ? SEMVER_PATTERN.exec(text) : null;
  if (!match) return null;
  return { numbers: [match[1], match[2], match[3]], prerelease: match[4] ? match[4].split(".") : [] };
}

/** Digit strings without leading zeros, compared by value however long they are. */
const compareDigits = (left, right) => (left.length !== right.length ? Math.sign(left.length - right.length) : left < right ? -1 : left > right ? 1 : 0);

/**
 * Semantic-version precedence: -1, 0 or 1 as `left` is older than, the same as
 * or newer than `right` (semver.org §11: a prerelease is older than its
 * release, build metadata is ignored). Throws unless both are versions.
 */
export function compareVersions(left, right) {
  const a = parseSemver(left);
  const b = parseSemver(right);
  if (!a || !b) throw new TypeError(`not a semantic version: ${JSON.stringify(a ? right : left)}`);
  for (let index = 0; index < 3; index += 1) {
    const order = compareDigits(a.numbers[index], b.numbers[index]);
    if (order) return order;
  }
  if (a.prerelease.length === 0 || b.prerelease.length === 0) return Math.sign(b.prerelease.length - a.prerelease.length);
  for (let index = 0; index < Math.min(a.prerelease.length, b.prerelease.length); index += 1) {
    const [x, y] = [a.prerelease[index], b.prerelease[index]];
    const [xNumeric, yNumeric] = [/^\d+$/u.test(x), /^\d+$/u.test(y)];
    if (xNumeric && yNumeric) {
      const order = compareDigits(x, y);
      if (order) return order;
    } else if (xNumeric !== yNumeric) {
      return xNumeric ? -1 : 1;
    } else if (x !== y) {
      return x < y ? -1 : 1;
    }
  }
  return Math.sign(a.prerelease.length - b.prerelease.length);
}

/** One pointer in a sentence: `build 0123456789ab (version 1.2.4, built 2026-10-07T08:00:00Z)`. */
export function describePointer(pointer) {
  const id = pointerId(pointer);
  if (!id) return "an unreadable pointer";
  const facts = [pointer.version ? `version ${pointer.version}` : null, pointer.builtAt ? `built ${pointer.builtAt}` : null].filter(Boolean);
  return `build ${id.slice(0, 12)}${facts.length ? ` (${facts.join(", ")})` : ""}`;
}

/**
 * Whether `pointer` goes on the channel, given the pointer already there
 * (`existing`, or null). An agent's pointer is keyed by its source id, so one
 * that exists is the same agent and is left alone; the sidecar's pointer is the
 * one that moves on every release, and is left alone only when it already names
 * this very build.
 *
 * The sidecar's pointer is shared by every host of its protocol generation, so
 * one made from an older checkout would silently downgrade them all: when the
 * pointer on the channel has a newer `version` than `pointer` (this tree's root
 * package.json version), the decision is `{ publish: false, refused: true }`.
 * The same version with another build is a rebuild or a hotfix, and goes on.
 * A version that is not a semantic version cannot be ordered and does not
 * block. `force` publishes regardless, and says what it replaced.
 */
export function publishDecision({ pointer, existing, force = false }) {
  if (!existing) return { publish: true, reason: "" };
  if (pointer.component === "remote-agent") {
    return force
      ? { publish: true, reason: "--force: replacing the pointer on the channel" }
      : { publish: false, reason: "its pointer is already on the channel (--force publishes it again)" };
  }
  const replacing = `${describePointer(existing)} with ${describePointer(pointer)}`;
  const comparable = parseSemver(existing.version) && parseSemver(pointer.version);
  const downgrade = comparable ? compareVersions(existing.version, pointer.version) > 0 : false;
  const generation = Number.isInteger(pointer.protocol) ? `protocol ${pointer.protocol}` : "this protocol";
  if (force) {
    return {
      publish: true,
      reason: `--force: replacing ${replacing}${downgrade ? `; this is a downgrade, every host on ${generation} is moved back to version ${pointer.version}` : ""}`,
    };
  }
  if (pointerId(existing) === pointerId(pointer)) return { publish: false, reason: "the channel already points at this build (--force publishes it again)" };
  if (downgrade) {
    return {
      publish: false,
      refused: true,
      reason:
        `the channel is at version ${existing.version}, newer than this checkout's ${pointer.version}: it has ${describePointer(existing)}, ` +
        `and publishing would replace it with ${describePointer(pointer)}, moving every host on ${generation} back to the older sidecar. ` +
        `Publish from a checkout at ${existing.version} or later, or pass --force to downgrade on purpose`,
    };
  }
  const unordered = existing.version && !comparable ? ` (its version ${JSON.stringify(existing.version)} cannot be ordered against ${pointer.version}, so whether this is a downgrade is unknown)` : "";
  return { publish: true, reason: `replaces ${replacing}${unordered}` };
}

// ---------------------------------------------------------------------------
// Whether the sidecar is started before it is published

/**
 * Whether the sidecar of `triple` is started and handshaken before publishing
 * (`{ probe: true }`), or, with `noProbe`, published without that check
 * (`{ probe: false, warning }`). A build for another platform than this
 * computer's `host` cannot be started here; publishing it stamps this tree's
 * `protocol` and `claudeAgentSdk` on it unchecked, so that takes an explicit
 * `--no-probe`. A build that can be started is always checked. Throws the
 * reason otherwise.
 */
export function probeDecision({ triple, host = null, noProbe = false, protocol, claudeAgentSdk }) {
  const file = aisdkFileName(triple);
  if (host && triple === host) {
    if (noProbe) {
      throw new Error(`--no-probe: ${file} is for ${triple}, this computer's own platform, so it can be started here and its handshake is not optional; drop --no-probe`);
    }
    return { probe: true, warning: "" };
  }
  const here = host ? `this computer's ${host}` : "this computer (rustc -vV does not run, so its triple is unknown)";
  if (!noProbe) {
    throw new Error(
      `${file} is for ${triple}, which cannot be started on ${here}, so it cannot be checked to speak protocol ${protocol}. ` +
        `Publishing a build made elsewhere without that check needs an explicit --no-probe: it stamps this checkout's protocol ${protocol} and Claude Agent SDK ${claudeAgentSdk} on the build, whatever it was made from`
    );
  }
  return {
    probe: false,
    warning:
      `WARNING: --no-probe: ${file} for ${triple} is NOT started, so nothing proves it speaks protocol ${protocol}. ` +
      `Its protocol (${protocol}) and Claude Agent SDK (${claudeAgentSdk}) are taken from THIS checkout, not read from the build: ` +
      `if it was made from another commit, every host on protocol ${protocol} gets a sidecar that may not work`,
  };
}

/**
 * What the app does with a downloaded file, so a published one is known to
 * install: `gz` must be exactly the compressed size and digest the pointer
 * records, and unpack to exactly the recorded size and digest. Throws if not.
 */
export function verifyBlob(file, gz) {
  if (gz.length !== file.size) throw new Error(`${file.name}: stored ${gz.length} bytes, the pointer says ${file.size}`);
  if (sha256Hex(gz) !== file.sha256) throw new Error(`${file.name}: the stored bytes do not have the pointer's SHA-256`);
  const bytes = zlib.gunzipSync(gz);
  if (bytes.length !== file.unpackedSize) throw new Error(`${file.name}: unpacks to ${bytes.length} bytes, the pointer says ${file.unpackedSize}`);
  if (sha256Hex(bytes) !== file.unpackedSha256) throw new Error(`${file.name}: the unpacked bytes do not have the pointer's SHA-256`);
}
