import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import zlib from "node:zlib";

import {
  AGENT_HELPER,
  AGENT_TARGETS,
  BLOB_HEADERS,
  CHANNEL_URL,
  CLAUDE_AGENT_PLATFORM_PACKAGES,
  CLAUDE_AGENT_RELEASE_KEY,
  CLAUDE_AGENT_RELEASE_SCHEMA,
  DEFAULT_CHECK_TRIPLES,
  POINTER_HEADERS,
  agentFileNames,
  agentPointerKey,
  agentSource,
  aisdkFileName,
  aisdkPointerKey,
  assertTriple,
  blobKey,
  blobPath,
  bucketKey,
  buildPointer,
  claudeAgentReleaseDecision,
  claudeAgentReleaseJson,
  claudeAgentSdkPin,
  compareVersions,
  describePointer,
  helloFrame,
  isExactRelease,
  isPlainName,
  isRelativePath,
  npmVersionUrl,
  ownAgentSource,
  ownAgentTriple,
  packFile,
  parseHostTriple,
  parseProtocolRs,
  parseProtocolTs,
  parseSemver,
  parseSidecarLine,
  pointerDirectory,
  pointerId,
  pointerJson,
  probeDecision,
  protocolVersion,
  publicUrl,
  publishDecision,
  selectAgentBuilds,
  shutdownFrame,
  validateClaudeAgentRelease,
  validatePointer,
  validatePublishable,
  verifyBlob,
  withinCaretRange,
} from "../components-plan.mjs";
import { checkChannel, checkClaudeAgentRelease, checkNpmRelease, describeEntry, httpGetText, parseClaudeAgentRelease } from "../components-check.mjs";
import { probeSidecar } from "../sidecar-probe.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const sha = (data) => createHash("sha256").update(data).digest("hex");
const SOURCE = "5".repeat(64);
const OTHER_SOURCE = "6".repeat(64);
const WINDOWS = "x86_64-pc-windows-msvc";
const LINUX = "x86_64-unknown-linux-musl";

function aisdkPointer(overrides = {}) {
  const exe = packFile("mewrk-aisdk.exe", Buffer.from("MZ sidecar ".repeat(1000)));
  return buildPointer({
    component: "aisdk",
    triple: WINDOWS,
    version: "1.2.4",
    builtAt: "2026-10-07T08:00:00Z",
    extra: { protocol: 17, claudeAgentSdk: "0.3.284" },
    files: [exe.entry],
    ...overrides,
  });
}

function agentPointer(triple = WINDOWS) {
  const files = agentFileNames(triple).map((name) => packFile(name, Buffer.from(`${name} bytes `.repeat(500))).entry);
  return buildPointer({ component: "remote-agent", triple, version: "1.2.4", builtAt: "2026-10-07T08:00:00Z", extra: { source: SOURCE }, files });
}

test("channel keys follow the layout the app reads", () => {
  assert.equal(aisdkPointerKey(17, WINDOWS), "aisdk/p17/x86_64-pc-windows-msvc.json");
  assert.equal(agentPointerKey(SOURCE, LINUX), `remote-agent/${SOURCE}/x86_64-unknown-linux-musl.json`);
  assert.equal(pointerDirectory("aisdk/p17/x86_64-pc-windows-msvc.json"), "aisdk/p17");
  assert.equal(pointerDirectory(`remote-agent/${SOURCE}/${LINUX}.json`), `remote-agent/${SOURCE}`);
  const digest = "a".repeat(64);
  assert.equal(blobPath(WINDOWS, digest, "mewrk-aisdk.exe"), `x86_64-pc-windows-msvc/${digest}/mewrk-aisdk.exe.gz`);
  assert.equal(
    blobKey("aisdk/p17/x86_64-pc-windows-msvc.json", { path: blobPath(WINDOWS, digest, "mewrk-aisdk.exe") }),
    `aisdk/p17/x86_64-pc-windows-msvc/${digest}/mewrk-aisdk.exe.gz`
  );
  assert.equal(bucketKey("aisdk/p17/a.json"), "components/aisdk/p17/a.json");
  assert.equal(publicUrl("aisdk/p17/a.json"), "https://dl.mewrk.dev/components/aisdk/p17/a.json");
  assert.equal(CHANNEL_URL, "https://dl.mewrk.dev/components");
  for (const protocol of [0, -1, 1.5, "17", null]) assert.throws(() => aisdkPointerKey(protocol, WINDOWS), /protocol/u);
  for (const source of ["", "abc", "G".repeat(64), "A".repeat(64)]) assert.throws(() => agentPointerKey(source, LINUX), /source id/u);
  for (const triple of ["", "windows", "../x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc/x", "X86_64-PC-WINDOWS-MSVC"]) {
    assert.throws(() => aisdkPointerKey(17, triple), /target triple/u, triple);
  }
});

test("file names are the ones the app installs", () => {
  assert.equal(aisdkFileName(WINDOWS), "mewrk-aisdk.exe");
  assert.equal(aisdkFileName("aarch64-apple-darwin"), "mewrk-aisdk");
  assert.deepEqual(agentFileNames(WINDOWS), ["mewrk-remote.exe", "srt-win.exe"]);
  assert.deepEqual(agentFileNames("aarch64-pc-windows-msvc"), ["mewrk-remote.exe", AGENT_HELPER]);
  assert.deepEqual(agentFileNames(LINUX), ["mewrk-remote"]);
  assert.equal(ownAgentTriple("x86_64-unknown-linux-gnu"), LINUX);
  assert.equal(ownAgentTriple("aarch64-unknown-linux-gnu"), "aarch64-unknown-linux-musl");
  assert.equal(ownAgentTriple(WINDOWS), WINDOWS);
  assert.equal(ownAgentTriple("aarch64-apple-darwin"), "aarch64-apple-darwin");
});

test("a packed file records both digests and unpacks to the original", () => {
  const bytes = Buffer.from("hello component ".repeat(4096));
  const { entry, gz } = packFile("mewrk-remote", bytes);
  assert.deepEqual(entry, {
    name: "mewrk-remote",
    size: gz.length,
    sha256: sha(gz),
    unpackedSize: bytes.length,
    unpackedSha256: sha(bytes),
  });
  assert.ok(gz.length < bytes.length / 10);
  assert.deepEqual(zlib.gunzipSync(gz), bytes);
  verifyBlob(entry, gz);
  assert.throws(() => packFile("mewrk-remote", Buffer.alloc(0)), /empty/u);
  assert.throws(() => packFile("dir/mewrk-remote", bytes), /plain file name/u);
});

test("verifyBlob refuses what the app would refuse to install", () => {
  const { entry, gz } = packFile("mewrk-remote", Buffer.from("agent ".repeat(2000)));
  assert.throws(() => verifyBlob(entry, gz.subarray(0, gz.length - 1)), /stored/u);
  const flipped = Buffer.from(gz);
  flipped[12] ^= 0xff;
  assert.throws(() => verifyBlob(entry, flipped), /SHA-256|stored/u);
  assert.throws(() => verifyBlob({ ...entry, unpackedSize: entry.unpackedSize + 1 }, gz), /unpacks to/u);
  assert.throws(() => verifyBlob({ ...entry, unpackedSha256: "0".repeat(64) }, gz), /unpacked bytes/u);
});

test("a sidecar pointer has the documented shape", () => {
  const pointer = aisdkPointer();
  const [file] = pointer.files;
  assert.deepEqual(Object.keys(pointer), ["schema", "component", "triple", "version", "builtAt", "protocol", "claudeAgentSdk", "files"]);
  assert.equal(pointer.schema, 1);
  assert.equal(pointer.component, "aisdk");
  assert.equal(pointer.protocol, 17);
  assert.equal(pointer.claudeAgentSdk, "0.3.284");
  assert.deepEqual(Object.keys(file), ["name", "path", "size", "sha256", "unpackedSize", "unpackedSha256"]);
  assert.equal(file.path, `${WINDOWS}/${file.sha256}/mewrk-aisdk.exe.gz`);
  assert.equal(pointerId(pointer), file.unpackedSha256);
  const text = pointerJson(pointer);
  assert.ok(text.endsWith("}\n"));
  assert.deepEqual(JSON.parse(text), pointer);
  validatePointer(JSON.parse(text), "aisdk", WINDOWS);
});

test("a Windows agent pointer lists the agent, then the sandbox helper", () => {
  const pointer = agentPointer(WINDOWS);
  assert.deepEqual(pointer.files.map((file) => file.name), ["mewrk-remote.exe", "srt-win.exe"]);
  assert.equal(pointer.source, SOURCE);
  assert.deepEqual(agentPointer(LINUX).files.map((file) => file.name), ["mewrk-remote"]);
  assert.throws(
    () => buildPointer({ component: "remote-agent", triple: WINDOWS, version: "1", builtAt: "x", extra: { source: SOURCE }, files: [packFile("mewrk-remote.exe", Buffer.from("a")).entry] }),
    /srt-win\.exe/u
  );
  assert.throws(
    () => buildPointer({ component: "remote-agent", triple: LINUX, version: "1", builtAt: "x", extra: { source: "nope" }, files: [packFile("mewrk-remote", Buffer.from("a")).entry] }),
    /source id/u
  );
});

test("a pointer cannot be built without what its component needs", () => {
  const exe = packFile("mewrk-aisdk.exe", Buffer.from("sidecar"));
  const base = { component: "aisdk", triple: WINDOWS, version: "1.2.4", builtAt: "2026-10-07T08:00:00Z", files: [exe.entry] };
  assert.throws(() => buildPointer({ ...base, extra: { claudeAgentSdk: "0.3.284" } }), /protocol/u);
  assert.throws(() => buildPointer({ ...base, extra: { protocol: 17 } }), /claudeAgentSdk/u);
  assert.throws(() => buildPointer({ ...base, extra: { protocol: 17, claudeAgentSdk: "^0.3.284" } }), /claudeAgentSdk/u);
  assert.throws(() => buildPointer({ ...base, extra: { protocol: 17, claudeAgentSdk: "0.3.284", files: [] } }), /extra may not set files/u);
  assert.throws(() => buildPointer({ ...base, component: "other", extra: {} }), /unknown component/u);
  assert.throws(() => buildPointer({ ...base, version: "", extra: { protocol: 17, claudeAgentSdk: "0.3.284" } }), /version/u);
  // The sidecar's file is named for its platform.
  assert.throws(() => buildPointer({ ...base, triple: LINUX, extra: { protocol: 17, claudeAgentSdk: "0.3.284" } }), /exactly mewrk-aisdk/u);
});

test("validatePointer mirrors Pointer::validate of the app", () => {
  const good = aisdkPointer();
  const mutate = (change) => {
    const copy = structuredClone(good);
    change(copy);
    return copy;
  };
  validatePointer(good, "aisdk", WINDOWS);
  assert.throws(() => validatePointer(mutate((p) => (p.schema = 2)), "aisdk", WINDOWS), /schema 2 is not 1/u);
  assert.throws(() => validatePointer(good, "remote-agent", WINDOWS), /pointer is for aisdk\/x86_64-pc-windows-msvc, not remote-agent/u);
  assert.throws(() => validatePointer(good, "aisdk", LINUX), /not aisdk\/x86_64-unknown-linux-musl/u);
  assert.throws(() => validatePointer(mutate((p) => (p.files = [])), "aisdk", WINDOWS), /lists no files/u);
  assert.throws(() => validatePointer(mutate((p) => p.files.push(structuredClone(p.files[0]))), "aisdk", WINDOWS), /plain, unique name/u);
  for (const name of ["", ".hidden", "a/b", "a b", "é.exe", "x".repeat(129), "..", "a\\b"]) {
    assert.throws(() => validatePointer(mutate((p) => (p.files[0].name = name)), "aisdk", WINDOWS), /plain, unique name/u, JSON.stringify(name));
  }
  for (const bad of ["", "../escape.gz", "/abs/x.gz", "a//b.gz", "a/./b.gz", "a\\b.gz", "a/.hidden/b.gz", "a/b/", `${"d/".repeat(300)}x.gz`]) {
    assert.throws(() => validatePointer(mutate((p) => (p.files[0].path = bad)), "aisdk", WINDOWS), /not inside its directory/u, bad);
  }
  for (const field of ["sha256", "unpackedSha256"]) {
    for (const bad of ["", "abc", "A".repeat(64), "g".repeat(64), "a".repeat(63), "a".repeat(65)]) {
      assert.throws(() => validatePointer(mutate((p) => (p.files[0][field] = bad)), "aisdk", WINDOWS), /not SHA-256/u, `${field} ${bad}`);
    }
  }
  for (const field of ["size", "unpackedSize"]) {
    for (const bad of [0, -1, 2 ** 30 + 1, 1.5, "10", null]) {
      assert.throws(() => validatePointer(mutate((p) => (p.files[0][field] = bad)), "aisdk", WINDOWS), /out of range/u, `${field} ${bad}`);
    }
    validatePointer(mutate((p) => (p.files[0][field] = 2 ** 30)), "aisdk", WINDOWS);
  }
  assert.throws(() => validatePointer(mutate((p) => (p.version = 3)), "aisdk", WINDOWS), /version/u);
  // validatePublishable is the publisher's own, stricter, check: the path is content-addressed.
  assert.throws(() => validatePublishable(mutate((p) => (p.files[0].path = "x/y/mewrk-aisdk.exe.gz"))), /is not stored at/u);
  assert.equal(isPlainName("mewrk-aisdk.exe"), true);
  assert.equal(isRelativePath("a/b/c.gz"), true);
  assert.equal(isRelativePath("a/../c.gz"), false);
});

test("blobs are kept for good and pointers for a minute", () => {
  assert.deepEqual({ ...BLOB_HEADERS }, { "cache-control": "public, max-age=31536000, immutable", "content-type": "application/gzip" });
  assert.deepEqual({ ...POINTER_HEADERS }, { "cache-control": "public, max-age=60", "content-type": "application/json; charset=utf-8" });
});

test("the protocol generation is read from both ends and they must agree", () => {
  const ts = "/** doc */\nexport const PROTOCOL_VERSION = 17;\nconst MAX_LINE_BYTES = 1;\n";
  const rs = "/// doc\npub(crate) const PROTOCOL_VERSION: u32 = 17;\n#[test] fn t() { assert_eq!(PROTOCOL_VERSION, 16); }\n";
  assert.equal(parseProtocolTs(ts), 17);
  assert.equal(parseProtocolRs(rs), 17);
  assert.equal(protocolVersion(ts, rs), 17);
  assert.throws(() => protocolVersion(ts, rs.replace("= 17", "= 16")), /protocol 17 but the host speaks 16/u);
  assert.throws(() => parseProtocolTs("export const OTHER = 1;"), /no PROTOCOL_VERSION/u);
  assert.throws(() => parseProtocolRs("const PROTOCOL_VERSION: i64 = 1;"), /no PROTOCOL_VERSION/u);
});

test("the Claude Agent SDK pin is the exact version, from either dependency group", () => {
  assert.equal(claudeAgentSdkPin({ dependencies: { "@anthropic-ai/claude-agent-sdk": "0.3.284" } }), "0.3.284");
  assert.equal(claudeAgentSdkPin(JSON.stringify({ devDependencies: { "@anthropic-ai/claude-agent-sdk": "0.4.0" } })), "0.4.0");
  assert.throws(() => claudeAgentSdkPin({ dependencies: { ai: "1.0.0" } }), /does not declare/u);
  assert.throws(() => claudeAgentSdkPin({ dependencies: { "@anthropic-ai/claude-agent-sdk": "^0.3.284" } }), /exact version/u);
  assert.throws(() => claudeAgentSdkPin({ devDependencies: { "@anthropic-ai/claude-agent-sdk": "latest" } }), /exact version/u);
  assert.throws(
    () => claudeAgentSdkPin({ dependencies: { "@anthropic-ai/claude-agent-sdk": "0.3.284" }, devDependencies: { "@anthropic-ai/claude-agent-sdk": "0.3.285" } }),
    /twice/u
  );
});

test("this tree's protocol and SDK pin are readable and agree with the host", () => {
  const text = (...segments) => fs.readFileSync(path.join(root, ...segments), "utf8");
  const protocol = protocolVersion(text("aisdk-service", "src", "protocol.ts"), text("src-tauri", "src", "aisdk", "protocol.rs"));
  assert.ok(Number.isInteger(protocol) && protocol >= 16);
  assert.match(claudeAgentSdkPin(text("aisdk-service", "package.json")), /^\d+\.\d+\.\d+$/u);
});

test("the host triple is the one rustc reports", () => {
  assert.equal(parseHostTriple("rustc 1.97.1\nbinary: rustc\nhost: x86_64-pc-windows-msvc\nrelease: 1.97.1\n"), WINDOWS);
  assert.equal(parseHostTriple("host: aarch64-apple-darwin\r\n"), "aarch64-apple-darwin");
  assert.throws(() => parseHostTriple("rustc 1.97.1\n"), /host triple/u);
});

test("an agent's source id is the first well-formed marker in its bytes", () => {
  const marker = "mewrk-remote-source:";
  const bytes = Buffer.concat([
    Buffer.from("\0\0junk"),
    Buffer.from(`${marker}not-a-digest-at-all-not-a-digest-at-all-not-a-digest-at-all-xxxx`),
    Buffer.from("\0"),
    Buffer.from(`${marker}${SOURCE}`),
    Buffer.from(`\0${marker}${OTHER_SOURCE}`),
  ]);
  assert.equal(agentSource(bytes), SOURCE);
  assert.equal(agentSource(Buffer.from(`${marker}${"A".repeat(64)}`)), null);
  assert.equal(agentSource(Buffer.from("no marker here")), null);
  assert.equal(agentSource(Buffer.from(`${marker}${"7".repeat(63)}`)), null);
});

function staged(triple, source, extra = []) {
  return { triple, source, files: [...agentFileNames(triple).filter((name) => !extra.includes(`-${name}`)), ...extra.filter((name) => !name.startsWith("-"))] };
}

test("only builds of the current agent source are published", () => {
  const builds = [
    staged(LINUX, SOURCE),
    staged(WINDOWS, SOURCE),
    staged("aarch64-pc-windows-msvc", SOURCE, ["-srt-win.exe"]),
    staged("aarch64-unknown-linux-musl", OTHER_SOURCE),
    staged("aarch64-apple-darwin", null),
    { triple: "x86_64-apple-darwin", source: null, files: [] },
    { triple: "not a triple", source: SOURCE, files: ["mewrk-remote"] },
  ];
  const selection = selectAgentBuilds({ reference: WINDOWS, builds });
  assert.equal(selection.source, SOURCE);
  assert.deepEqual(selection.publish, [WINDOWS, LINUX]);
  assert.deepEqual(
    selection.skipped.map((entry) => entry.triple),
    ["aarch64-apple-darwin", "aarch64-pc-windows-msvc", "aarch64-unknown-linux-musl", "not a triple", "x86_64-apple-darwin"]
  );
  const reason = (triple) => selection.skipped.find((entry) => entry.triple === triple).reason;
  assert.match(reason("aarch64-unknown-linux-musl"), /other agent source 666666666666, not the current 555555555555/u);
  assert.match(reason("aarch64-apple-darwin"), /before agent source identities/u);
  assert.match(reason("aarch64-pc-windows-msvc"), /no srt-win\.exe/u);
  assert.match(reason("x86_64-apple-darwin"), /no mewrk-remote/u);
});

test("the agent source is named by this computer's own build, and --triple narrows to one", () => {
  const builds = [staged(LINUX, SOURCE), staged(WINDOWS, SOURCE), staged("aarch64-unknown-linux-musl", OTHER_SOURCE)];
  assert.throws(() => selectAgentBuilds({ reference: "aarch64-apple-darwin", builds }), /no aarch64-apple-darwin agent build/u);
  assert.throws(() => selectAgentBuilds({ reference: WINDOWS, builds: [staged(WINDOWS, null)] }), /carries no source id/u);
  // Even this computer's own Windows build stays home without its sandbox helper.
  const helperless = selectAgentBuilds({ reference: WINDOWS, builds: [staged(WINDOWS, SOURCE, ["-srt-win.exe"]), staged(LINUX, SOURCE)] });
  assert.deepEqual(helperless.publish, [LINUX]);
  assert.equal(helperless.skipped[0].triple, WINDOWS);
  assert.deepEqual(selectAgentBuilds({ reference: WINDOWS, builds, only: LINUX }).publish, [LINUX]);
  assert.throws(() => selectAgentBuilds({ reference: WINDOWS, builds, only: "aarch64-unknown-linux-musl" }), /cannot be published: made from other agent source/u);
  assert.throws(() => selectAgentBuilds({ reference: WINDOWS, builds, only: "aarch64-apple-darwin" }), /no aarch64-apple-darwin agent build in/u);
});

test("an agent's pointer is published once; the sidecar's moves with each build", () => {
  const agent = agentPointer(LINUX);
  assert.deepEqual(publishDecision({ pointer: agent, existing: null }), { publish: true, reason: "" });
  assert.equal(publishDecision({ pointer: agent, existing: agent }).publish, false);
  assert.equal(publishDecision({ pointer: agent, existing: { unreadable: true } }).publish, false);
  assert.equal(publishDecision({ pointer: agent, existing: agent, force: true }).publish, true);

  const current = aisdkPointer();
  assert.equal(publishDecision({ pointer: current, existing: null }).publish, true);
  assert.match(publishDecision({ pointer: current, existing: current }).reason, /already points at this build/u);
  const newer = aisdkPointer({ files: [packFile("mewrk-aisdk.exe", Buffer.from("a newer sidecar")).entry] });
  const replace = publishDecision({ pointer: newer, existing: current });
  assert.equal(replace.publish, true);
  assert.match(
    replace.reason,
    /^replaces build [0-9a-f]{12} \(version 1\.2\.4, built 2026-10-07T08:00:00Z\) with build [0-9a-f]{12} \(version 1\.2\.4, built 2026-10-07T08:00:00Z\)$/u
  );
  assert.equal(publishDecision({ pointer: current, existing: current, force: true }).publish, true);
  assert.equal(publishDecision({ pointer: current, existing: { unreadable: true } }).publish, true);
});

test("the handshake frames are the lines the sidecar and the host exchange", () => {
  assert.equal(helloFrame(17), '{"v":17,"type":"hello"}\n');
  assert.equal(shutdownFrame(17), '{"v":17,"type":"shutdown"}\n');
  assert.deepEqual(parseSidecarLine('{"v":17,"seq":0,"type":"ready","protocol":17}'), { type: "ready", protocol: 17 });
  assert.deepEqual(parseSidecarLine('{"v":17,"seq":1,"type":"heartbeat"}'), { type: "heartbeat" });
  assert.equal(parseSidecarLine("not json"), null);
  assert.equal(parseSidecarLine("42"), null);
  assert.equal(parseSidecarLine('{"v":17}'), null);
});

// A sidecar stand-in: reads the hello line, answers as the script says.
function fakeSidecar(directory, name, body) {
  const file = path.join(directory, `${name}.mjs`);
  fs.writeFileSync(
    file,
    `import readline from "node:readline";
const lines = readline.createInterface({ input: process.stdin });
const send = (frame) => process.stdout.write(JSON.stringify({ v: 17, seq: 0, ...frame }) + "\\n");
lines.on("line", (line) => {
  const frame = JSON.parse(line);
  if (frame.type === "shutdown") process.exit(0);
  ${body}
});
lines.on("close", () => process.exit(0));
`
  );
  return file;
}

test("the start-up check accepts a sidecar that answers ready with the same protocol", async (context) => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "mewrk-probe-"));
  context.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const probe = (body, options = {}) => probeSidecar({ command: process.execPath, args: [fakeSidecar(directory, `s${Math.random().toString(16).slice(2)}`, body)], protocol: 17, ...options });

  const ok = await probe('if (frame.type === "hello") send({ type: "ready", protocol: 17 });');
  assert.equal(ok.ok, true, ok.message);
  assert.equal(ok.protocol, 17);

  const older = await probe('if (frame.type === "hello") send({ type: "ready", protocol: 16 });');
  assert.equal(older.ok, false);
  assert.equal(older.protocol, 16);
  assert.match(older.message, /protocol 16, not the 17/u);

  const silent = await probe("", { timeoutMs: 400 });
  assert.equal(silent.ok, false);
  assert.match(silent.message, /no ready frame within/u);

  const dies = await probe('if (frame.type === "hello") { console.error("协议世代不匹配"); process.exit(3); }');
  assert.equal(dies.ok, false);
  assert.match(dies.message, /exited \(code 3\) before it answered ready.*协议世代不匹配/u);

  const noisy = await probe('if (frame.type === "hello") { process.stdout.write("warming up\\n"); send({ type: "ready", protocol: 17 }); }');
  assert.equal(noisy.ok, true, noisy.message);

  const missing = await probeSidecar({ command: path.join(directory, "no-such-sidecar.exe"), protocol: 17, timeoutMs: 2000 });
  assert.equal(missing.ok, false);
  assert.match(missing.message, /could not run/u);
});

// ---------------------------------------------------------------------------
// The downgrade guard

/** A sidecar pointer of `version`; `label` makes the build (and so its id) differ. */
function sidecarOf(version, label = version, overrides = {}) {
  return aisdkPointer({ version, files: [packFile("mewrk-aisdk.exe", Buffer.from(`sidecar ${label}`)).entry], ...overrides });
}

test("versions are ordered by semver precedence", () => {
  const ascending = ["0.9.9", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.1", "1.2.0", "1.10.0", "2.0.0", "10.0.0"];
  for (let low = 0; low < ascending.length; low += 1) {
    for (let high = 0; high < ascending.length; high += 1) {
      assert.equal(compareVersions(ascending[low], ascending[high]), Math.sign(low - high), `${ascending[low]} vs ${ascending[high]}`);
    }
  }
  assert.equal(compareVersions("1.2.3+build.5", "1.2.3+build.9"), 0);
  assert.equal(compareVersions("1.2.3+build", "1.2.3"), 0);
  assert.equal(compareVersions("99999999999999999999.0.0", "99999999999999999998.0.0"), 1);
  assert.deepEqual(parseSemver("1.2.3-rc.1+x"), { numbers: ["1", "2", "3"], prerelease: ["rc", "1"] });
  for (const bad of ["", "1.2", "v1.2.3", "01.2.3", "1.2.3.4", "1.2.3-", "1.2.3-01", "latest", null, undefined, 123]) {
    assert.equal(parseSemver(bad), null, JSON.stringify(bad));
    assert.throws(() => compareVersions(bad, "1.2.3"), /not a semantic version/u);
    assert.throws(() => compareVersions("1.2.3", bad), /not a semantic version/u);
  }
});

test("a sidecar from an older checkout is refused, saying what is published and what would replace it", () => {
  const onChannel = sidecarOf("1.2.4", "built on 1.2.4");
  const older = sidecarOf("1.2.3", "built on 1.2.3", { builtAt: "2026-10-01T08:00:00Z" });
  const refusal = publishDecision({ pointer: older, existing: onChannel });
  assert.equal(refusal.publish, false);
  assert.equal(refusal.refused, true);
  assert.match(refusal.reason, /the channel is at version 1\.2\.4, newer than this checkout's 1\.2\.3/u);
  assert.ok(refusal.reason.includes(describePointer(onChannel)), refusal.reason);
  assert.ok(refusal.reason.includes(describePointer(older)), refusal.reason);
  assert.match(refusal.reason, /every host on protocol 17/u);
  assert.match(refusal.reason, /--force/u);
  assert.match(describePointer(onChannel), /^build [0-9a-f]{12} \(version 1\.2\.4, built 2026-10-07T08:00:00Z\)$/u);

  // The same refusal across the version parts, and for a prerelease of the version that is out.
  for (const [published, mine] of [["2.0.0", "1.99.99"], ["1.10.0", "1.9.9"], ["1.2.10", "1.2.9"], ["1.2.4", "1.2.4-rc.1"]]) {
    const decision = publishDecision({ pointer: sidecarOf(mine, "mine"), existing: sidecarOf(published, "theirs") });
    assert.equal(decision.refused, true, `${published} on the channel, ${mine} here`);
  }
});

test("a rebuild of the same version, a newer one, and a prerelease's release all replace the pointer", () => {
  const onChannel = sidecarOf("1.2.4", "first build");
  const rebuild = publishDecision({ pointer: sidecarOf("1.2.4", "hotfix"), existing: onChannel });
  assert.equal(rebuild.publish, true);
  assert.equal(rebuild.refused, undefined);
  assert.ok(rebuild.reason.startsWith(`replaces ${describePointer(onChannel)} with `), rebuild.reason);
  assert.equal(publishDecision({ pointer: sidecarOf("1.2.5", "newer"), existing: onChannel }).publish, true);
  assert.equal(publishDecision({ pointer: sidecarOf("1.2.4", "release"), existing: sidecarOf("1.2.4-rc.1", "candidate") }).publish, true);
  // The very same build under an older version number changes nothing, so there is nothing to refuse.
  const same = publishDecision({ pointer: aisdkPointer({ version: "1.2.3" }), existing: aisdkPointer({ version: "1.2.4" }) });
  assert.deepEqual({ publish: same.publish, refused: same.refused }, { publish: false, refused: undefined });
  assert.match(same.reason, /already points at this build/u);
});

test("--force replaces a newer pointer and names the downgrade; a pointer nobody can order does not block", () => {
  const onChannel = sidecarOf("1.2.4", "newer");
  const older = sidecarOf("1.2.3", "older");
  const forced = publishDecision({ pointer: older, existing: onChannel, force: true });
  assert.equal(forced.publish, true);
  assert.equal(forced.refused, undefined);
  assert.match(forced.reason, /^--force: replacing build [0-9a-f]{12} \(version 1\.2\.4, .*\) with build [0-9a-f]{12} \(version 1\.2\.3, .*\); this is a downgrade, every host on protocol 17 is moved back to version 1\.2\.3$/u);
  assert.doesNotMatch(publishDecision({ pointer: sidecarOf("1.2.5", "x"), existing: onChannel, force: true }).reason, /downgrade/u);
  assert.deepEqual(publishDecision({ pointer: older, existing: null, force: true }), { publish: true, reason: "" });

  const odd = publishDecision({ pointer: older, existing: { ...onChannel, version: "nightly" } });
  assert.equal(odd.publish, true);
  assert.match(odd.reason, /"nightly" cannot be ordered against 1\.2\.3/u);
  assert.equal(publishDecision({ pointer: older, existing: { unreadable: true } }).publish, true);
  assert.equal(publishDecision({ pointer: older, existing: { files: [] } }).publish, true);
});

test("an agent pointer is never moved by a version: it is keyed by its source", () => {
  const onChannel = { ...agentPointer(LINUX), version: "1.2.9" };
  const mine = { ...agentPointer(LINUX), version: "1.2.3" };
  const decision = publishDecision({ pointer: mine, existing: onChannel });
  assert.deepEqual({ publish: decision.publish, refused: decision.refused }, { publish: false, refused: undefined });
  assert.equal(publishDecision({ pointer: mine, existing: onChannel, force: true }).publish, true);
});

// ---------------------------------------------------------------------------
// --no-probe

test("a sidecar this computer cannot start is published unchecked only with --no-probe, and loudly", () => {
  const stamp = { protocol: 17, claudeAgentSdk: "0.3.284" };
  assert.deepEqual(probeDecision({ triple: WINDOWS, host: WINDOWS, ...stamp }), { probe: true, warning: "" });
  assert.throws(() => probeDecision({ triple: WINDOWS, host: WINDOWS, noProbe: true, ...stamp }), /own platform.*handshake is not optional/u);

  const message = /mewrk-aisdk is for aarch64-apple-darwin, which cannot be started on this computer's x86_64-pc-windows-msvc, so it cannot be checked to speak protocol 17\. .*needs an explicit --no-probe.*protocol 17 and Claude Agent SDK 0\.3\.284/u;
  assert.throws(() => probeDecision({ triple: "aarch64-apple-darwin", host: WINDOWS, ...stamp }), message);

  const unchecked = probeDecision({ triple: "aarch64-apple-darwin", host: WINDOWS, noProbe: true, ...stamp });
  assert.equal(unchecked.probe, false);
  assert.match(unchecked.warning, /^WARNING: --no-probe: mewrk-aisdk for aarch64-apple-darwin is NOT started/u);
  assert.match(unchecked.warning, /protocol \(17\) and Claude Agent SDK \(0\.3\.284\) are taken from THIS checkout, not read from the build/u);

  // Without a known host triple nothing is known to be startable, so the same rule holds.
  assert.throws(() => probeDecision({ triple: WINDOWS, host: null, ...stamp }), /rustc -vV does not run.*--no-probe/u);
  assert.equal(probeDecision({ triple: WINDOWS, host: null, noProbe: true, ...stamp }).probe, false);
});

// ---------------------------------------------------------------------------
// This tree's agent source

test("this computer's own agent build names the agent source, and says why when it cannot", () => {
  const builds = [staged(LINUX, OTHER_SOURCE), staged(WINDOWS, SOURCE)];
  assert.deepEqual(ownAgentSource({ reference: WINDOWS, builds }), { source: SOURCE, problem: null, message: "" });
  const missing = ownAgentSource({ reference: "aarch64-apple-darwin", builds });
  assert.deepEqual({ source: missing.source, problem: missing.problem }, { source: null, problem: "missing" });
  assert.match(missing.message, /no aarch64-apple-darwin agent build to tell the current agent source by/u);
  assert.equal(ownAgentSource({ reference: WINDOWS, builds: [] }).problem, "missing");
  // A directory without the agent in it is no build.
  assert.equal(ownAgentSource({ reference: LINUX, builds: [{ triple: LINUX, source: SOURCE, files: [] }] }).problem, "missing");
  const unidentified = ownAgentSource({ reference: WINDOWS, builds: [staged(WINDOWS, null)] });
  assert.deepEqual({ source: unidentified.source, problem: unidentified.problem }, { source: null, problem: "unidentified" });
  assert.match(unidentified.message, /carries no source id/u);
});

test("the agent platforms are listed once, for the builder and for the gate", () => {
  assert.equal(AGENT_TARGETS.length, 6);
  assert.equal(new Set(AGENT_TARGETS).size, AGENT_TARGETS.length);
  for (const triple of AGENT_TARGETS) assertTriple(triple);
  for (const host of ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]) {
    assert.ok(AGENT_TARGETS.includes(ownAgentTriple(host)), `the agent this computer runs, ${ownAgentTriple(host)}, is one of the targets`);
  }
  assert.throws(() => AGENT_TARGETS.push("x"), TypeError);
  assert.deepEqual([...DEFAULT_CHECK_TRIPLES], ["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]);
  // build-remote-agents.mjs runs on import, so it is read, not imported: it must take the list from here.
  const builder = fs.readFileSync(path.join(root, "scripts", "build-remote-agents.mjs"), "utf8");
  assert.match(builder, /import \{[^}]*\bAGENT_TARGETS as TARGETS\b[^}]*\} from "\.\/components-plan\.mjs"/u);
  assert.doesNotMatch(builder, /const TARGETS\s*=/u);
});

// ---------------------------------------------------------------------------
// The Claude Agent SDK release the provider page offers

test("claude-agent/version.json is a schema 1 document naming one exact release, written the way pointers are", () => {
  assert.equal(CLAUDE_AGENT_RELEASE_KEY, "claude-agent/version.json");
  assert.equal(CLAUDE_AGENT_RELEASE_SCHEMA, 1);
  assert.equal(publicUrl(CLAUDE_AGENT_RELEASE_KEY), "https://dl.mewrk.dev/components/claude-agent/version.json");
  assert.equal(bucketKey(CLAUDE_AGENT_RELEASE_KEY), "components/claude-agent/version.json");
  assert.equal(claudeAgentReleaseJson("0.3.293"), '{\n  "schema": 1,\n  "sdkVersion": "0.3.293"\n}\n');
  assert.equal(validateClaudeAgentRelease(JSON.parse(claudeAgentReleaseJson("10.20.30"))), "10.20.30");
  // A document the app would refuse cannot be written.
  for (const bad of ["0.3", "v0.3.293", " 0.3.293", "0.3.293 ", "0.3.293\n", "0.3.293-rc.1", "0.3.293+build.5", "0.3.0293", "00.3.293", "0.03.293", "1.2.3.4", "^0.3.293", "", "latest", 5, null, undefined]) {
    assert.throws(() => claudeAgentReleaseJson(bad), /sdkVersion/u, String(bad));
  }
});

test("the release document is checked the way the app reads it: schema 1, an exact release, nothing else", () => {
  const ok = { schema: 1, sdkVersion: "0.3.293" };
  assert.equal(validateClaudeAgentRelease(ok), "0.3.293");
  assert.equal(validateClaudeAgentRelease({ sdkVersion: "1.0.0", schema: 1 }), "1.0.0");
  assert.equal(validateClaudeAgentRelease({ schema: 1, sdkVersion: "0.0.0" }), "0.0.0");
  const refused = (doc, pattern) => assert.throws(() => validateClaudeAgentRelease(doc), pattern, JSON.stringify(doc));
  for (const doc of [null, undefined, "0.3.293", 5, true, [], [ok]]) refused(doc, /is not an object/u);
  refused({ schema: 2, sdkVersion: "0.3.293" }, /schema 2, not 1/u);
  refused({ schema: "1", sdkVersion: "0.3.293" }, /schema "1", not 1/u);
  refused({ sdkVersion: "0.3.293" }, /schema undefined, not 1/u);
  refused({ schema: 1 }, /sdkVersion undefined is not an exact release/u);
  refused({ schema: 1, sdkVersion: 293 }, /sdkVersion 293 is not an exact release/u);
  refused({ schema: 1, sdkVersion: null }, /sdkVersion null is not an exact release/u);
  const notReleases = ["v0.3.293", "0.3.293-beta.1", "0.3.293-0", "0.3.293+sha.5", "0.3", "1", "0.3.293.1", "0.3.293 ", " 0.3.293", "0.3.293\n", "01.3.293", "0.03.293", "0.3.0293", "^0.3.293", "~0.3.293", ">=0.3.293", "*", "latest", "", "0.3.x"];
  for (const sdkVersion of notReleases) refused({ schema: 1, sdkVersion }, /sdkVersion .* is not an exact release such as 0\.3\.293/u);
  for (const sdkVersion of notReleases) assert.equal(isExactRelease(sdkVersion), false, JSON.stringify(sdkVersion));
  for (const release of ["0.3.293", "0.0.0", "1.2.3", "10.20.30", "0.3.1000"]) assert.equal(isExactRelease(release), true, release);
  refused({ schema: 1, sdkVersion: "0.3.293", channel: "beta" }, /unknown key "channel"/u);
  refused({ schema: 1, sdkVersion: "0.3.293", a: 1, b: 2 }, /unknown keys "a", "b"/u);
  refused({ schema: 1, sdkVersion: "0.3.293", version: "0.3.293" }, /unknown key "version"/u);
});

test("a release is inside ^pin the way npm's caret says: 0.x keeps its minor, 0.0.x is exact, 1.x keeps its major", () => {
  const inside = (versions, pin) => versions.map((version) => withinCaretRange(version, pin));
  assert.deepEqual(inside(["0.3.284", "0.3.285", "0.3.293", "0.3.1000"], "0.3.284"), [true, true, true, true]);
  assert.deepEqual(inside(["0.3.283", "0.2.999", "0.4.0", "0.30.0", "1.0.0", "0.0.9"], "0.3.284"), [false, false, false, false, false, false]);
  assert.deepEqual(inside(["0.0.3"], "0.0.3"), [true]);
  assert.deepEqual(inside(["0.0.2", "0.0.4", "0.0.30", "0.1.3", "0.1.0", "1.0.3"], "0.0.3"), [false, false, false, false, false, false]);
  assert.deepEqual(inside(["1.2.3", "1.2.4", "1.3.0", "1.10.0", "1.99.99"], "1.2.3"), [true, true, true, true, true]);
  assert.deepEqual(inside(["1.2.2", "1.1.9", "2.0.0", "0.9.9", "10.2.3"], "1.2.3"), [false, false, false, false, false]);
  for (const [version, pin] of [["0.3.293-rc.1", "0.3.284"], ["0.3.293", "^0.3.284"], ["v0.3.293", "0.3.284"], [undefined, "0.3.284"]]) {
    assert.throws(() => withinCaretRange(version, pin), TypeError, `${version} ${pin}`);
  }
});

test("a release is named only if it is exact, inside ^pin and not older than the one published; the one published is left alone", () => {
  const decide = (requested, published, pin, force = false) => claudeAgentReleaseDecision({ requested, published, pin, force });

  // Publishing: the first release, a newer one, the pin itself.
  assert.deepEqual(decide("0.3.293", null, "0.3.284"), { action: "publish", reason: "the channel names no Claude Agent SDK release yet" });
  assert.equal(decide("0.3.293", undefined, "0.3.284").action, "publish");
  assert.deepEqual(decide("0.3.293", "0.3.290", "0.3.284"), { action: "publish", reason: "replacing 0.3.290 with 0.3.293" });
  assert.equal(decide("0.3.1000", "0.3.999", "0.3.284").action, "publish", "numbers are compared as numbers");
  assert.equal(decide("0.3.284", null, "0.3.284").action, "publish");
  assert.equal(decide("1.4.0", "1.3.9", "1.2.3").action, "publish");

  // Skipping: it is already there.
  const skipped = decide("0.3.293", "0.3.293", "0.3.284");
  assert.equal(skipped.action, "skip");
  assert.match(skipped.reason, /already offers Claude Agent SDK 0\.3\.293 \(--force publishes it again\)/u);
  assert.deepEqual(decide("0.3.293", "0.3.293", "0.3.284", true), { action: "publish", reason: "--force: publishing 0.3.293 again" });

  // Older than the published release moves every provider page back.
  assert.throws(() => decide("0.3.290", "0.3.293", "0.3.284"), /offers Claude Agent SDK 0\.3\.293, newer than 0\.3\.290.*move every provider page back.*--force/su);
  assert.throws(() => decide("0.3.9", "0.3.10", "0.3.1"), /newer than 0\.3\.9/u, "9 is older than 10");
  const downgrade = decide("0.3.290", "0.3.293", "0.3.284", true);
  assert.equal(downgrade.action, "publish");
  assert.equal(downgrade.reason, "--force: replacing 0.3.293 with 0.3.290; a downgrade from 0.3.293");

  // Outside ^pin no host built from this tree could install it.
  for (const requested of ["0.3.283", "0.4.0", "0.2.999", "1.0.0"]) {
    assert.throws(() => decide(requested, null, "0.3.284"), /outside \^0\.3\.284: hosts built from this tree only take \^0\.3\.284.*--force/su, requested);
    const forced = decide(requested, null, "0.3.284", true);
    assert.equal(forced.action, "publish");
    assert.match(forced.reason, /^--force: naming .*; .* is outside \^0\.3\.284/u);
  }
  for (const requested of ["0.0.2", "0.0.4", "0.1.0", "1.0.3"]) {
    assert.throws(() => decide(requested, null, "0.0.3"), /outside \^0\.0\.3/u, requested);
    assert.equal(decide(requested, null, "0.0.3", true).action, "publish");
  }
  assert.equal(decide("0.0.3", null, "0.0.3").action, "publish");
  for (const requested of ["1.2.2", "2.0.0", "0.9.9"]) {
    assert.throws(() => decide(requested, "1.2.3", "1.2.3"), /outside \^1\.2\.3/u, requested);
    assert.equal(decide(requested, null, "1.2.3", true).action, "publish");
  }
  assert.equal(decide("1.9.0", "1.2.3", "1.2.3").action, "publish");
  // Both reasons are named when --force overrides both.
  assert.equal(
    decide("0.4.0", "0.5.0", "0.3.284", true).reason,
    "--force: replacing 0.5.0 with 0.4.0; 0.4.0 is outside ^0.3.284, which hosts built from this tree take; a downgrade from 0.5.0"
  );
  // The range is checked before the order.
  assert.throws(() => decide("0.4.0", "0.5.0", "0.3.284"), /outside \^0\.3\.284/u);

  // Anything but an exact release is refused, --force or not.
  for (const requested of ["0.3.293-rc.1", "0.3.293+build.1", "v0.3.293", "0.3", " 0.3.293", "0.3.293\n", "latest", "^0.3.293", "", null, undefined, 3, {}]) {
    for (const force of [false, true]) {
      assert.throws(() => decide(requested, null, "0.3.284", force), /is not an exact release/u, `${String(requested)} force=${force}`);
    }
  }
  assert.throws(() => decide("0.3.293", null, "^0.3.284"), /pin "\^0\.3\.284" is not an exact release/u);
  assert.throws(() => decide("0.3.293", null, undefined), /pin undefined is not an exact release/u);
  assert.throws(() => decide("0.3.293", "0.3.290-rc.1", "0.3.284", true), /on the channel .* is not an exact release/u);
});

// ---------------------------------------------------------------------------
// The release gate

function sidecarFor(triple, { protocol = 17, version = "1.2.4" } = {}) {
  const file = packFile(aisdkFileName(triple), Buffer.from(`sidecar for ${triple} ${protocol}`));
  return buildPointer({ component: "aisdk", triple, version, builtAt: "2026-10-07T08:00:00Z", extra: { protocol, claudeAgentSdk: "0.3.284" }, files: [file.entry] });
}

function agentFor(triple, source = SOURCE) {
  const files = agentFileNames(triple).map((name) => packFile(name, Buffer.from(`${name} for ${triple}`)).entry);
  return buildPointer({ component: "remote-agent", triple, version: "1.2.4", builtAt: "2026-10-07T08:00:00Z", extra: { source }, files });
}

const sidecarKeys = (triples, protocol = 17) => triples.map((triple) => aisdkPointerKey(protocol, triple));
const agentKeys = (source = SOURCE) => AGENT_TARGETS.map((triple) => agentPointerKey(source, triple));

/** A channel holding `held` (channel key -> pointer object, raw text, or `{ status }`); every other key is a 404. Records the URLs asked for. */
function fakeChannel(held) {
  const asked = [];
  const fetchText = async (url) => {
    asked.push(url);
    assert.ok(url.startsWith(`${CHANNEL_URL}/`), url);
    const value = held[url.slice(CHANNEL_URL.length + 1)];
    if (value === undefined) return { status: 404, text: "" };
    if (value instanceof Error) throw value;
    if (typeof value === "string") return { status: 200, text: value };
    if ("status" in value && !("schema" in value)) return { status: value.status, text: "" };
    return { status: 200, text: pointerJson(value) };
  };
  return { fetchText, asked };
}

function completeChannel() {
  const held = {};
  for (const triple of DEFAULT_CHECK_TRIPLES) held[aisdkPointerKey(17, triple)] = sidecarFor(triple);
  for (const triple of AGENT_TARGETS) held[agentPointerKey(SOURCE, triple)] = agentFor(triple);
  held[CLAUDE_AGENT_RELEASE_KEY] = claudeAgentReleaseJson("0.3.293");
  return held;
}

test("the gate wants this protocol's sidecar on each platform and this source's agent on every agent platform", async () => {
  const { fetchText, asked } = fakeChannel(completeChannel());
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText });
  assert.deepEqual(result.missing, []);
  assert.equal(result.agentsChecked, true);
  // The release document is looked at last, and is not a pointer.
  assert.deepEqual(result.entries.map((entry) => entry.key), [...sidecarKeys(DEFAULT_CHECK_TRIPLES), ...agentKeys(), CLAUDE_AGENT_RELEASE_KEY]);
  assert.ok(result.entries.every((entry) => entry.ok && entry.problem === null && (entry.pointer || entry.component === "claude-agent")));
  assert.deepEqual([...asked].sort(), [...sidecarKeys(DEFAULT_CHECK_TRIPLES), ...agentKeys(), CLAUDE_AGENT_RELEASE_KEY].map(publicUrl).sort());
  assert.equal(publicUrl(sidecarKeys([WINDOWS])[0]), "https://dl.mewrk.dev/components/aisdk/p17/x86_64-pc-windows-msvc.json");
  assert.equal(
    describeEntry(result.entries[0]),
    `aisdk ${WINDOWS}: ${describePointer(result.entries[0].pointer)}, protocol 17, Claude Agent SDK 0.3.284`
  );
  assert.match(describeEntry(result.entries[2]), /^remote-agent x86_64-unknown-linux-musl: build [0-9a-f]{12} .*, agent source 555555555555$/u);
  assert.equal(describeEntry(result.entries.at(-1)), "claude-agent version.json: Claude Agent SDK 0.3.293");

  // The platforms asked for are the caller's, and another protocol generation is another key.
  const other = fakeChannel({ [aisdkPointerKey(16, LINUX)]: sidecarFor(LINUX, { protocol: 16 }) });
  const looked = await checkChannel({ protocol: 17, aisdkTriples: [LINUX], source: null, fetchText: other.fetchText });
  assert.deepEqual(other.asked.sort(), [publicUrl(aisdkPointerKey(17, LINUX)), publicUrl(CLAUDE_AGENT_RELEASE_KEY)].sort());
  assert.deepEqual(looked.missing.map((entry) => entry.key), [aisdkPointerKey(17, LINUX), CLAUDE_AGENT_RELEASE_KEY]);
});

test("the gate lists what is missing, unreadable or not a pointer the app would take", async () => {
  const held = completeChannel();
  delete held[aisdkPointerKey(17, "aarch64-apple-darwin")];
  held[agentPointerKey(SOURCE, "aarch64-unknown-linux-musl")] = { status: 403 };
  held[agentPointerKey(SOURCE, "x86_64-apple-darwin")] = { status: 503 };
  held[agentPointerKey(SOURCE, "aarch64-apple-darwin")] = "<html>sign in</html>";
  held[agentPointerKey(SOURCE, "aarch64-pc-windows-msvc")] = new Error("socket hang up");
  delete held[CLAUDE_AGENT_RELEASE_KEY];
  const { fetchText } = fakeChannel(held);
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText });
  assert.equal(result.entries.length, 9);
  const problems = Object.fromEntries(result.missing.map((entry) => [`${entry.component}/${entry.triple ?? "version.json"}`, entry.problem]));
  assert.deepEqual(problems, {
    "aisdk/aarch64-apple-darwin": "missing (HTTP 404)",
    "remote-agent/aarch64-unknown-linux-musl": "missing (HTTP 403)",
    "remote-agent/x86_64-apple-darwin": "could not be read: HTTP 503",
    "remote-agent/aarch64-apple-darwin": "the answer is not JSON",
    "remote-agent/aarch64-pc-windows-msvc": "could not be read: socket hang up",
    "claude-agent/version.json": "missing (HTTP 404)",
  });
  assert.ok(result.missing.every((entry) => !entry.ok && entry.pointer === null));
  assert.equal(
    describeEntry(result.missing[0]),
    "aisdk aarch64-apple-darwin: missing (HTTP 404) - https://dl.mewrk.dev/components/aisdk/p17/aarch64-apple-darwin.json"
  );
  assert.equal(describeEntry(result.missing.at(-1)), "claude-agent version.json: missing (HTTP 404) - https://dl.mewrk.dev/components/claude-agent/version.json");
});

test("the gate refuses a pointer that is for another protocol, platform, source or lacks a file", async () => {
  const held = completeChannel();
  held[aisdkPointerKey(17, WINDOWS)] = sidecarFor(WINDOWS, { protocol: 16 });
  held[aisdkPointerKey(17, "aarch64-apple-darwin")] = sidecarFor("x86_64-apple-darwin");
  held[agentPointerKey(SOURCE, LINUX)] = agentFor(LINUX, OTHER_SOURCE);
  const noHelper = agentFor(WINDOWS);
  held[agentPointerKey(SOURCE, WINDOWS)] = { ...noHelper, files: noHelper.files.slice(0, 1) };
  held[agentPointerKey(SOURCE, "aarch64-unknown-linux-musl")] = { ...agentFor("aarch64-unknown-linux-musl"), schema: 2 };
  const { fetchText } = fakeChannel(held);
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText });
  const problem = (component, triple) => result.missing.find((entry) => entry.component === component && entry.triple === triple)?.problem;
  assert.match(problem("aisdk", WINDOWS), /^not a valid pointer: it is for protocol 16, not the 17 it is filed under$/u);
  assert.match(problem("aisdk", "aarch64-apple-darwin"), /not a valid pointer: pointer is for aisdk\/x86_64-apple-darwin, not aisdk\/aarch64-apple-darwin/u);
  assert.match(problem("remote-agent", LINUX), /it is for agent source 666666666666, not the 555555555555 it is filed under/u);
  assert.match(problem("remote-agent", WINDOWS), /lists exactly mewrk-remote\.exe, srt-win\.exe, in that order/u);
  assert.match(problem("remote-agent", "aarch64-unknown-linux-musl"), /schema 2 is not 1/u);
  assert.equal(result.missing.length, 5);
});

test("without an agent source the gate checks the sidecars only and says the agents were not checked", async () => {
  const { fetchText, asked } = fakeChannel(completeChannel());
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: null, fetchText });
  assert.equal(result.agentsChecked, false);
  assert.equal(result.entries.length, 3);
  assert.deepEqual(result.missing, []);
  assert.equal(asked.length, 3);
  assert.ok(asked.every((url) => url.includes("/aisdk/") || url === publicUrl(CLAUDE_AGENT_RELEASE_KEY)));
});

test("the gate reads the release document: OK with its version, missing or invalid otherwise", async () => {
  const read = async (value) => checkClaudeAgentRelease(fakeChannel({ [CLAUDE_AGENT_RELEASE_KEY]: value }).fetchText);
  const url = publicUrl(CLAUDE_AGENT_RELEASE_KEY);

  const ok = await read(claudeAgentReleaseJson("0.3.293"));
  assert.deepEqual(
    { component: ok.component, triple: ok.triple, key: ok.key, url: ok.url, ok: ok.ok, state: ok.state, problem: ok.problem, version: ok.version },
    { component: "claude-agent", triple: null, key: CLAUDE_AGENT_RELEASE_KEY, url, ok: true, state: "ok", problem: null, version: "0.3.293" }
  );
  assert.equal(describeEntry(ok), "claude-agent version.json: Claude Agent SDK 0.3.293");

  const notAnObject = "not a valid release document: the Claude Agent SDK release document is not an object";
  const cases = [
    [undefined, "missing", "missing (HTTP 404)"],
    [{ status: 403 }, "missing", "missing (HTTP 403)"],
    [{ status: 503 }, "unreadable", "could not be read: HTTP 503"],
    [{ status: 302 }, "unreadable", "could not be read: HTTP 302"],
    [new Error("socket hang up"), "unreadable", "could not be read: socket hang up"],
    ["<html>sign in</html>", "invalid", "the answer is not JSON"],
    ["", "invalid", "the answer is not JSON"],
    ["null", "invalid", notAnObject],
    ["[]", "invalid", notAnObject],
    ['{"schema":2,"sdkVersion":"0.3.293"}', "invalid", "not a valid release document: the Claude Agent SDK release document has schema 2, not 1"],
    ['{"sdkVersion":"0.3.293"}', "invalid", "not a valid release document: the Claude Agent SDK release document has schema undefined, not 1"],
    ['{"schema":1}', "invalid", /^not a valid release document: .*sdkVersion undefined is not an exact release/u],
    ['{"schema":1,"sdkVersion":"0.3.0293"}', "invalid", /sdkVersion "0\.3\.0293" is not an exact release/u],
    ['{"schema":1,"sdkVersion":"0.3.293-rc.1"}', "invalid", /sdkVersion "0\.3\.293-rc\.1" is not an exact release/u],
    ['{"schema":1,"sdkVersion":"v0.3.293"}', "invalid", /sdkVersion "v0\.3\.293" is not an exact release/u],
    ['{"schema":1,"sdkVersion":" 0.3.293"}', "invalid", /is not an exact release/u],
    ['{"schema":1,"sdkVersion":293}', "invalid", /sdkVersion 293 is not an exact release/u],
    ['{"schema":1,"sdkVersion":"0.3.293","note":"beta"}', "invalid", /unknown key "note"/u],
  ];
  for (const [value, state, problem] of cases) {
    const entry = await read(value);
    const label = typeof value === "string" ? value : String(value?.message ?? JSON.stringify(value));
    assert.equal(entry.ok, false, label);
    assert.equal(entry.state, state, label);
    assert.equal(entry.version, null, label);
    assert.equal(entry.pointer, null, label);
    if (problem instanceof RegExp) assert.match(entry.problem, problem, label);
    else assert.equal(entry.problem, problem, label);
    assert.equal(describeEntry(entry), `claude-agent version.json: ${entry.problem} - ${url}`, label);
  }

  // The text on its own, as the publisher reads --out with it.
  assert.deepEqual(parseClaudeAgentRelease(claudeAgentReleaseJson("0.3.293")), { version: "0.3.293", problem: null });
  assert.deepEqual(parseClaudeAgentRelease("{"), { version: null, problem: "the answer is not JSON" });
  assert.match(parseClaudeAgentRelease('{"schema":1,"sdkVersion":"1.0"}').problem, /^not a valid release document: /u);
});

test("a missing or invalid release document alone fails the gate, whatever the pointers say", async () => {
  const run = async (override) => {
    const held = { ...completeChannel(), ...override };
    return checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText: fakeChannel(held).fetchText });
  };
  assert.deepEqual((await run({})).missing, []);
  for (const bad of ['{"schema":1,"sdkVersion":"0.3.293-rc.1"}', '{"schema":3,"sdkVersion":"0.3.293"}', "gone", { status: 500 }, undefined]) {
    const result = await run({ [CLAUDE_AGENT_RELEASE_KEY]: bad });
    assert.deepEqual(result.missing.map((entry) => entry.key), [CLAUDE_AGENT_RELEASE_KEY], JSON.stringify(bad));
    assert.equal(result.entries.length, 9);
  }
});

test("npm must have the SDK and the CLI package of each release platform at the requested version", async () => {
  assert.deepEqual([...CLAUDE_AGENT_PLATFORM_PACKAGES], ["@anthropic-ai/claude-agent-sdk-win32-x64", "@anthropic-ai/claude-agent-sdk-darwin-arm64"]);
  assert.equal(npmVersionUrl("@anthropic-ai/claude-agent-sdk", "0.3.293"), "https://registry.npmjs.org/@anthropic-ai%2fclaude-agent-sdk/0.3.293");
  assert.equal(npmVersionUrl("@anthropic-ai/claude-agent-sdk-win32-x64", "0.3.293"), "https://registry.npmjs.org/@anthropic-ai%2fclaude-agent-sdk-win32-x64/0.3.293");

  const packages = ["@anthropic-ai/claude-agent-sdk", ...CLAUDE_AGENT_PLATFORM_PACKAGES];
  const urls = packages.map((name) => npmVersionUrl(name, "0.3.293"));
  const found = (name, version = "0.3.293") => ({ status: 200, text: JSON.stringify({ name, version, dist: { tarball: "x" } }) });
  const registry = (answers) => {
    const asked = [];
    const fetchText = async (url) => {
      asked.push(url);
      const answer = answers[url];
      if (answer instanceof Error) throw answer;
      return answer ?? { status: 404, text: '{"error":"version not found: 0.3.293"}' };
    };
    return { fetchText, asked };
  };
  const everything = Object.fromEntries(packages.map((name, index) => [urls[index], found(name)]));

  const complete = registry(everything);
  assert.deepEqual(await checkNpmRelease("0.3.293", complete.fetchText), { checked: packages, problems: [] });
  assert.deepEqual([...complete.asked].sort(), [...urls].sort());

  const problemsOf = async (answers) => (await checkNpmRelease("0.3.293", registry(answers).fetchText)).problems;
  assert.match(
    (await problemsOf({ ...everything, [urls[0]]: undefined }))[0],
    /^npm has no @anthropic-ai\/claude-agent-sdk 0\.3\.293 \(https:\/\/registry\.npmjs\.org\/@anthropic-ai%2fclaude-agent-sdk\/0\.3\.293\)$/u
  );
  assert.match((await problemsOf({ ...everything, [urls[1]]: undefined }))[0], /^npm has no @anthropic-ai\/claude-agent-sdk-win32-x64 0\.3\.293/u);
  assert.match((await problemsOf({ ...everything, [urls[2]]: undefined }))[0], /^npm has no @anthropic-ai\/claude-agent-sdk-darwin-arm64 0\.3\.293/u);
  assert.match((await problemsOf({ ...everything, [urls[2]]: { status: 502, text: "bad gateway" } }))[0], /darwin-arm64 0\.3\.293 could not be looked up on npm: HTTP 502/u);
  assert.match((await problemsOf({ ...everything, [urls[1]]: new Error("socket hang up") }))[0], /win32-x64 0\.3\.293 could not be looked up on npm: socket hang up/u);
  assert.match((await problemsOf({ ...everything, [urls[0]]: { status: 200, text: "<html>" } }))[0], /npm's answer for @anthropic-ai\/claude-agent-sdk 0\.3\.293 is not JSON/u);
  assert.match((await problemsOf({ ...everything, [urls[0]]: found(packages[0], "0.3.292") }))[0], /is for version "0\.3\.292"/u);
  assert.match((await problemsOf({ ...everything, [urls[0]]: { status: 200, text: "{}" } }))[0], /is for version undefined/u);
  assert.equal((await problemsOf({})).length, 3, "all three are reported at once");
});

// ---------------------------------------------------------------------------
// The one network call

/** A local HTTP server answering with `respond(request, response, count)`, `count` being the number of requests so far; closed after the test. */
async function serve(context, respond) {
  const seen = [];
  const server = http.createServer((request, response) => {
    seen.push(request.headers);
    respond(request, response, seen.length);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  context.after(() => new Promise((resolve) => (server.closeAllConnections(), server.close(resolve))));
  return { url: `http://127.0.0.1:${server.address().port}/aisdk/p17/x.json`, seen };
}

test("a public read gives the status and the text, and retries only what a retry can fix", async (context) => {
  const fast = { retryDelayMs: 1, timeoutMs: 2000 };

  const ok = await serve(context, (_request, response) => response.writeHead(200, { "content-type": "application/json" }).end('{"a":1}'));
  assert.deepEqual(await httpGetText(ok.url, fast), { status: 200, text: '{"a":1}' });
  assert.equal(ok.seen.length, 1);
  assert.equal(ok.seen[0]["user-agent"], "mewrk-check-components");
  assert.equal(ok.seen[0]["accept-encoding"], "identity");

  // A missing key is an answer, not a failure to retry.
  const gone = await serve(context, (_request, response) => response.writeHead(404).end("nope"));
  assert.deepEqual(await httpGetText(gone.url, fast), { status: 404, text: "nope" });
  assert.equal(gone.seen.length, 1);

  const flaky = await serve(context, (_request, response, count) => (count < 3 ? response.writeHead(502).end("bad gateway") : response.writeHead(200).end("fine")));
  assert.deepEqual(await httpGetText(flaky.url, fast), { status: 200, text: "fine" });
  assert.equal(flaky.seen.length, 3);

  const down = await serve(context, (_request, response) => response.writeHead(500).end("still broken"));
  assert.deepEqual(await httpGetText(down.url, { ...fast, attempts: 2 }), { status: 500, text: "still broken" });
  assert.equal(down.seen.length, 2);

  const hangs = await serve(context, () => {});
  await assert.rejects(httpGetText(hangs.url, { retryDelayMs: 1, timeoutMs: 100, attempts: 2 }));
  assert.equal(hangs.seen.length, 2);

  // Nothing listens on a port that was just closed.
  const server = http.createServer();
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address();
  await new Promise((resolve) => server.close(resolve));
  await assert.rejects(httpGetText(`http://127.0.0.1:${port}/x.json`, { ...fast, attempts: 2 }), /fetch failed|ECONNREFUSED/u);
});

// ---------------------------------------------------------------------------
// publish-components.mjs's command line (every case ends before it touches a file or the network)

function publishCli(...args) {
  const run = spawnSync(process.execPath, [path.join(root, "scripts", "publish-components.mjs"), ...args], { encoding: "utf8", timeout: 60_000 });
  return { status: run.status, out: `${run.stdout}${run.stderr}` };
}

test("the command line has the release gate and --no-probe, and refuses what does not fit together", () => {
  const help = publishCli("--help");
  assert.equal(help.status, 0);
  for (const flag of ["--check", "--no-probe", "--triple <t>", "--force"]) assert.ok(help.out.includes(flag), flag);

  const fails = (args, pattern) => {
    const run = publishCli(...args);
    assert.equal(run.status, 1, `${args.join(" ")}\n${run.out}`);
    assert.match(run.out, pattern, args.join(" "));
  };
  fails(["--check", "--force"], /--check only looks at the channel and cannot be combined with --force/u);
  fails(["--check", "--aisdk"], /cannot be combined with --aisdk/u);
  fails(["--check", "--out", "somewhere"], /cannot be combined with --out/u);
  fails(["--check", "--triple", "windows"], /not a target triple: windows/u);
  fails(["--remote-agents", "--no-probe"], /--no-probe is about the sidecar: it needs --aisdk/u);
  fails(["--aisdk", "--triple", WINDOWS, "--triple", LINUX], /--triple can be given once when publishing/u);
  fails([], /nothing to publish: pass --aisdk and\/or --remote-agents \(or --check\)/u);
  // A platform that is nobody's computer cannot be started here, so it is not published without --no-probe.
  fails(["--aisdk", "--dry-run", "--triple", "riscv64gc-unknown-none-elf"], /cannot be started on this computer.*needs an explicit --no-probe/su);
});

test("--claude-agent-sdk is in the usage and refuses what does not fit together", () => {
  const help = publishCli("--help");
  assert.equal(help.status, 0);
  assert.ok(help.out.includes("--claude-agent-sdk <version>"));
  assert.ok(help.out.includes("claude-agent/version.json"));

  const fails = (args, pattern) => {
    const run = publishCli(...args);
    assert.equal(run.status, 1, `${args.join(" ")}\n${run.out}`);
    assert.match(run.out, pattern, args.join(" "));
  };
  fails(["--claude-agent-sdk"], /--claude-agent-sdk needs a value/u);
  fails(["--claude-agent-sdk", "--dry-run"], /--claude-agent-sdk needs a value/u);
  for (const version of ["v0.3.293", "0.3", "0.3.293-rc.1", "0.3.293+1", "latest", "^0.3.293", "0.3.0293"]) {
    fails(["--claude-agent-sdk", version, "--dry-run"], /--claude-agent-sdk needs an exact release such as 0\.3\.293/u);
  }
  fails(["--claude-agent-sdk", "0.3.293", "--claude-agent-sdk", "0.3.294"], /--claude-agent-sdk can be given once/u);
  fails(["--check", "--claude-agent-sdk", "0.3.293"], /--check only looks at the channel and cannot be combined with --claude-agent-sdk/u);
  fails(["--claude-agent-sdk", "0.3.293", "--triple", WINDOWS], /--triple is about the sidecar and the agents: it needs --aisdk or --remote-agents/u);
  fails(["--claude-agent-sdk", "0.3.293", "--no-probe"], /--no-probe is about the sidecar: it needs --aisdk/u);
  fails([], /nothing to publish: .*--claude-agent-sdk <version>/u);
});

// A stand-in for the network, loaded into the publisher with --import: npm has what STUB_NPM lists
// ("<package>@<version>,..."), the channel's release document is STUB_CHANNEL when set (else a 404),
// and any other request is an error, so a run that must not touch the network is shown not to.
const STUB_FETCH = String.raw`
const npm = new Set((process.env.STUB_NPM ?? "").split(",").filter(Boolean));
globalThis.fetch = async (input) => {
  const url = String(input);
  const found = /^https:\/\/registry\.npmjs\.org\/(@anthropic-ai%2f[^/]+)\/([^/]+)$/u.exec(url);
  if (found) {
    const name = decodeURIComponent(found[1]);
    return npm.has(name + "@" + found[2]) ? new Response(JSON.stringify({ name, version: found[2] })) : new Response("{}", { status: 404 });
  }
  if (url === "https://dl.mewrk.dev/components/claude-agent/version.json" && process.env.STUB_CHANNEL !== undefined) return new Response(process.env.STUB_CHANNEL);
  if (url.startsWith("https://dl.mewrk.dev/components/")) return new Response("", { status: 404 });
  throw new Error("unexpected request " + url);
};
`;

test("--claude-agent-sdk asks npm, then writes, skips or refuses claude-agent/version.json (--out and --dry-run, no sidecar, no credentials)", (context) => {
  const pin = claudeAgentSdkPin(fs.readFileSync(path.join(root, "aisdk-service", "package.json"), "utf8"));
  const [major, minor, patch] = pin.split(".").map(Number);
  const newer = `${major}.${minor}.${patch + 1}`;
  const outside = `${major + 1}.0.0`;
  if (!withinCaretRange(newer, pin)) return context.skip(`pin ${pin} takes no newer release, so there is no newer one to publish`);

  const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "mewrk-claude-release-"));
  context.after(() => fs.rmSync(scratch, { recursive: true, force: true }));
  const stub = path.join(scratch, "stub-fetch.mjs");
  fs.writeFileSync(stub, STUB_FETCH);
  const has = (...versions) => versions.flatMap((version) => ["@anthropic-ai/claude-agent-sdk", ...CLAUDE_AGENT_PLATFORM_PACKAGES].map((name) => `${name}@${version}`)).join(",");
  const run = (env, ...args) => {
    const done = spawnSync(process.execPath, ["--import", pathToFileURL(stub).href, path.join(root, "scripts", "publish-components.mjs"), ...args], {
      encoding: "utf8",
      timeout: 60_000,
      env: { ...process.env, CLOUDFLARE_API_TOKEN: "", CLOUDFLARE_ACCOUNT_ID: "", CLOUDFLARE_ZONE_ID: "", STUB_NPM: "", ...env },
    });
    return { status: done.status, out: `${done.stdout}${done.stderr}` };
  };
  const out = path.join(scratch, "channel");
  const document = path.join(out, "claude-agent", "version.json");
  const written = () => (fs.existsSync(document) ? fs.readFileSync(document, "utf8") : null);
  const expectOk = (result, pattern) => {
    assert.equal(result.status, 0, result.out);
    assert.match(result.out, pattern, result.out);
  };
  const expectFail = (result, pattern) => {
    assert.equal(result.status, 1, result.out);
    assert.match(result.out, pattern, result.out);
  };

  // A first release goes under --out as the exact body, after npm was asked about all three packages.
  const first = run({ STUB_NPM: has(pin, newer) }, "--claude-agent-sdk", newer, "--out", out);
  expectOk(first, /wrote claude-agent\/version\.json \(Claude Agent SDK /u);
  assert.ok(first.out.includes(`npm has @anthropic-ai/claude-agent-sdk@${newer}, ${CLAUDE_AGENT_PLATFORM_PACKAGES.map((name) => `${name}@${newer}`).join(", ")}`), first.out);
  assert.equal(written(), claudeAgentReleaseJson(newer));
  assert.deepEqual(fs.readdirSync(out), ["claude-agent"], "nothing but the document is written");
  assert.deepEqual(fs.readdirSync(path.dirname(document)), ["version.json"], "no partial file is left");

  // The same release again is left alone; --force writes it again.
  expectOk(run({ STUB_NPM: has(newer) }, "--claude-agent-sdk", newer, "--out", out), /skipping claude-agent\/version\.json: the channel already offers Claude Agent SDK /u);
  expectOk(run({ STUB_NPM: has(newer) }, "--claude-agent-sdk", newer, "--out", out, "--force"), /--force: publishing .* again/u);

  // An older release is refused, and --force names the downgrade.
  expectFail(run({ STUB_NPM: has(pin) }, "--claude-agent-sdk", pin, "--out", out), /newer than .*move every provider page back/su);
  assert.equal(written(), claudeAgentReleaseJson(newer));
  expectOk(run({ STUB_NPM: has(pin) }, "--claude-agent-sdk", pin, "--out", out, "--force"), /--force: replacing .*a downgrade from /u);
  assert.equal(written(), claudeAgentReleaseJson(pin));

  // One outside ^pin is refused, even though npm has it.
  expectFail(run({ STUB_NPM: has(outside) }, "--claude-agent-sdk", outside, "--out", out), /outside \^.*hosts built from this tree only take/su);
  assert.equal(written(), claudeAgentReleaseJson(pin));

  // npm lacking one of the three packages stops the run before anything is written.
  const fresh = path.join(scratch, "fresh");
  const lacking = has(newer).split(",").filter((entry) => !entry.includes("darwin-arm64")).join(",");
  expectFail(run({ STUB_NPM: lacking }, "--claude-agent-sdk", newer, "--out", fresh), /npm has no @anthropic-ai\/claude-agent-sdk-darwin-arm64 /u);
  expectFail(run({ STUB_NPM: "" }, "--claude-agent-sdk", newer, "--out", fresh), /not on npm as the app needs it/u);
  assert.equal(fs.existsSync(fresh), false);

  // A document under --out that is no release document stops the run, unless --force replaces it.
  fs.writeFileSync(document, '{"schema":1,"sdkVersion":"0.3.0293"}\n');
  expectFail(run({ STUB_NPM: has(newer) }, "--claude-agent-sdk", newer, "--out", out), /not a valid release document.*pass --force/su);
  expectOk(run({ STUB_NPM: has(newer) }, "--claude-agent-sdk", newer, "--out", out, "--force"), /--force: not relying on the release document/u);
  assert.equal(written(), claudeAgentReleaseJson(newer));

  // A dry run reads the public channel (a 404 is "none yet"), prints the body and writes nothing.
  const dry = run({ STUB_NPM: has(newer) }, "--claude-agent-sdk", newer, "--dry-run");
  expectOk(dry, /would publish claude-agent\/version\.json:\n\{\n  "schema": 1,\n  "sdkVersion": "/u);
  assert.match(dry.out, /dry run complete; nothing was written/u);
  expectOk(run({ STUB_NPM: has(newer), STUB_CHANNEL: claudeAgentReleaseJson(newer) }, "--claude-agent-sdk", newer, "--dry-run"), /skipping claude-agent\/version\.json/u);
  expectFail(run({ STUB_NPM: has(pin), STUB_CHANNEL: claudeAgentReleaseJson(newer) }, "--claude-agent-sdk", pin, "--dry-run"), /newer than /u);
  expectFail(run({ STUB_NPM: has(newer), STUB_CHANNEL: "<html>sign in</html>" }, "--claude-agent-sdk", newer, "--dry-run"), /the answer is not JSON/u);
  expectOk(run({ STUB_NPM: has(newer), STUB_CHANNEL: "<html>sign in</html>" }, "--claude-agent-sdk", newer, "--dry-run", "--force"), /would publish claude-agent\/version\.json/u);
});
