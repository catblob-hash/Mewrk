import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import zlib from "node:zlib";

import {
  AGENT_HELPER,
  AGENT_TARGETS,
  BLOB_HEADERS,
  CHANNEL_URL,
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
  claudeAgentSdkPin,
  compareVersions,
  describePointer,
  helloFrame,
  isPlainName,
  isRelativePath,
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
  validatePointer,
  validatePublishable,
  verifyBlob,
} from "../components-plan.mjs";
import { checkChannel, describeEntry, httpGetText } from "../components-check.mjs";
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
  return held;
}

test("the gate wants this protocol's sidecar on each platform and this source's agent on every agent platform", async () => {
  const { fetchText, asked } = fakeChannel(completeChannel());
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText });
  assert.deepEqual(result.missing, []);
  assert.equal(result.agentsChecked, true);
  assert.deepEqual(result.entries.map((entry) => entry.key), [...sidecarKeys(DEFAULT_CHECK_TRIPLES), ...agentKeys()]);
  assert.ok(result.entries.every((entry) => entry.ok && entry.problem === null && entry.pointer));
  assert.deepEqual([...asked].sort(), [...sidecarKeys(DEFAULT_CHECK_TRIPLES), ...agentKeys()].map(publicUrl).sort());
  assert.equal(publicUrl(sidecarKeys([WINDOWS])[0]), "https://dl.mewrk.dev/components/aisdk/p17/x86_64-pc-windows-msvc.json");
  assert.equal(
    describeEntry(result.entries[0]),
    `aisdk ${WINDOWS}: ${describePointer(result.entries[0].pointer)}, protocol 17, Claude Agent SDK 0.3.284`
  );
  assert.match(describeEntry(result.entries[2]), /^remote-agent x86_64-unknown-linux-musl: build [0-9a-f]{12} .*, agent source 555555555555$/u);

  // The platforms asked for are the caller's, and another protocol generation is another key.
  const other = fakeChannel({ [aisdkPointerKey(16, LINUX)]: sidecarFor(LINUX, { protocol: 16 }) });
  const looked = await checkChannel({ protocol: 17, aisdkTriples: [LINUX], source: null, fetchText: other.fetchText });
  assert.deepEqual(other.asked, [publicUrl(aisdkPointerKey(17, LINUX))]);
  assert.equal(looked.missing.length, 1);
});

test("the gate lists what is missing, unreadable or not a pointer the app would take", async () => {
  const held = completeChannel();
  delete held[aisdkPointerKey(17, "aarch64-apple-darwin")];
  held[agentPointerKey(SOURCE, "aarch64-unknown-linux-musl")] = { status: 403 };
  held[agentPointerKey(SOURCE, "x86_64-apple-darwin")] = { status: 503 };
  held[agentPointerKey(SOURCE, "aarch64-apple-darwin")] = "<html>sign in</html>";
  held[agentPointerKey(SOURCE, "aarch64-pc-windows-msvc")] = new Error("socket hang up");
  const { fetchText } = fakeChannel(held);
  const result = await checkChannel({ protocol: 17, aisdkTriples: [...DEFAULT_CHECK_TRIPLES], source: SOURCE, fetchText });
  assert.equal(result.entries.length, 8);
  const problems = Object.fromEntries(result.missing.map((entry) => [`${entry.component}/${entry.triple}`, entry.problem]));
  assert.deepEqual(problems, {
    "aisdk/aarch64-apple-darwin": "missing (HTTP 404)",
    "remote-agent/aarch64-unknown-linux-musl": "missing (HTTP 403)",
    "remote-agent/x86_64-apple-darwin": "could not be read: HTTP 503",
    "remote-agent/aarch64-apple-darwin": "the answer is not JSON",
    "remote-agent/aarch64-pc-windows-msvc": "could not be read: socket hang up",
  });
  assert.ok(result.missing.every((entry) => !entry.ok && entry.pointer === null));
  assert.equal(
    describeEntry(result.missing[0]),
    "aisdk aarch64-apple-darwin: missing (HTTP 404) - https://dl.mewrk.dev/components/aisdk/p17/aarch64-apple-darwin.json"
  );
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
  assert.equal(result.entries.length, 2);
  assert.deepEqual(result.missing, []);
  assert.equal(asked.length, 2);
  assert.ok(asked.every((url) => url.includes("/aisdk/")));
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
