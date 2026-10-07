// Publishes Mewrk's own components — the AI SDK sidecar and the remote agent
// builds — to the component channel at https://dl.mewrk.dev/components (the
// `components/` prefix of the R2 bucket `mewrk-releases`). The installer
// carries neither of them: the app fetches the sidecar at every start and an
// agent the first time a machine needs one. Layout, pointer format and
// validation are in components-plan.mjs; the app reads them with
// src-tauri/src/components/.
//
// Usage:
//   node scripts/publish-components.mjs [--aisdk] [--remote-agents]
//                                       [--triple <target triple>] [--no-probe]
//                                       [--out <dir>] [--dry-run] [--force]
//   node scripts/publish-components.mjs --check [--triple <target triple> ...]
//
//   --aisdk           publish aisdk-service/dist/mewrk-aisdk[.exe] (built by
//                     `npm run build:sidecar`) for this computer's triple, as
//                     the sidecar of the protocol generation both ends declare
//   --remote-agents   publish every agent build in src-tauri/remote-agents/
//                     (built by `npm run build:remote-agents -- --release`)
//                     made from the same agent source as this computer's own
//   --triple <t>      the sidecar's triple when it is not this computer's (a
//                     build made elsewhere, copied into aisdk-service/dist/;
//                     needs --no-probe); with --remote-agents, publish only
//                     that triple. With --check, repeatable: the sidecar
//                     platforms to look for
//   --no-probe        publish a sidecar of another platform without starting it
//                     (see below)
//   --out <dir>       write the channel (pointers and blobs) under <dir> instead
//                     of uploading; serve <dir> over HTTP and set the app's
//                     MEWRK_COMPONENTS_MIRROR to that address to try it
//   --dry-run         print what would be published, change nothing
//   --force           publish a build whose pointer is already on the channel,
//                     or replace a pointer of a newer version (see below)
//   --check           the release gate (npm run check:components): look on the
//                     public channel for what this tree needs, change nothing,
//                     exit non-zero listing what is missing (see below)
//
// The sidecar is started and sent the `hello` frame before it is published; it
// must answer `ready` with the protocol generation of this tree. A build for
// another platform than this computer's cannot be started here, so it is
// published only with an explicit --no-probe, which stamps this tree's protocol
// and Claude Agent SDK pin on it without anything proving the build has them.
//
// The sidecar's pointer (aisdk/p<protocol>/<triple>.json) is shared by every
// host of that protocol generation, so publishing from an older checkout would
// move them all back. It is refused when the pointer on the channel has a newer
// version than this tree's root package.json (the same version with another
// build is a rebuild or a hotfix and goes on), unless --force.
//
// --check looks, over public HTTPS GET and without credentials, for the sidecar
// pointer of this tree's protocol on each --triple (default: the Windows x64 and
// Apple silicon builds the app is released for), and for the agent pointer of
// this tree's agent source on every platform the agent is built for
// (AGENT_TARGETS in components-plan.mjs). The agent source is read from this
// computer's own agent build, as publishing does; with no such build the agent
// part is reported as skipped.
//
// Per build, the order is: compressed files first (a file already stored with
// the same size is kept), each checked at its public address, then the pointer,
// whose CDN copy is purged. Nothing points at a file that is not yet readable.
//
// Environment (upload only; see scripts/r2-client.mjs):
//   CLOUDFLARE_API_TOKEN   an API token with Workers R2 Storage: Edit and, for
//                          the purge, Cache Purge on the mewrk.dev zone
//   CLOUDFLARE_ACCOUNT_ID  the account that owns the bucket
//   CLOUDFLARE_ZONE_ID     the mewrk.dev zone; without it a replaced pointer
//                          is purged by nothing but its 60 s lifetime
//   R2_ACCESS_KEY_ID / R2_SECRET_ACCESS_KEY  optional S3 credentials to use
//                          instead of deriving them from the API token

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

import {
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
  buildPointer,
  bucketKey,
  claudeAgentSdkPin,
  ownAgentSource,
  ownAgentTriple,
  packFile,
  parseHostTriple,
  parseSemver,
  pointerJson,
  probeDecision,
  protocolVersion,
  publicUrl,
  publishDecision,
  selectAgentBuilds,
  validatePointer,
  verifyBlob,
} from "./components-plan.mjs";
import { checkChannel, describeEntry, httpGetText } from "./components-check.mjs";
import { MIRROR } from "./release-mirror-plan.mjs";
import { createR2Client, sha256Hex } from "./r2-client.mjs";
import { probeSidecar } from "./sidecar-probe.mjs";

const label = "[publish:components]";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const sidecarDirectory = path.join(root, "aisdk-service", "dist");
const agentsDirectory = path.join(root, "src-tauri", "remote-agents");
const bundledAgentsDirectory = path.join(root, "src-tauri", "bundled-agents");

const USAGE = `usage: node scripts/publish-components.mjs [--aisdk] [--remote-agents] [--triple <target triple>] [--no-probe] [--out <dir>] [--dry-run] [--force]
       node scripts/publish-components.mjs --check [--triple <target triple> ...]
  --aisdk           publish the AI SDK sidecar built by \`npm run build:sidecar\`
  --remote-agents   publish the agent builds in src-tauri/remote-agents/ (\`npm run build:remote-agents -- --release\`)
  --triple <t>      the sidecar's target triple (default: this computer's); with --remote-agents, only that triple;
                    with --check, repeatable: the sidecar platforms to look for (default: ${DEFAULT_CHECK_TRIPLES.join(", ")})
  --no-probe        publish a sidecar built for another platform, which cannot be started here, without the handshake:
                    this checkout's protocol and Claude Agent SDK pin are stamped on it unchecked
  --out <dir>       write the channel under <dir> instead of uploading (stands in for ${CHANNEL_URL} via MEWRK_COMPONENTS_MIRROR)
  --dry-run         print the plan, change nothing
  --force           publish a build whose pointer is already on the channel, or replace a pointer of a newer version
  --check           release gate (npm run check:components): are this tree's sidecar pointers (protocol and --triple) and
                    agent pointers (agent source, every agent platform) on ${CHANNEL_URL}? Exits 1 listing what is missing`;

const log = (message) => console.log(`${label} ${message}`);
const warn = (message) => console.warn(`${label} ${message}`);

function fail(message) {
  console.error(`${label} ${message}`);
  process.exit(1);
}

function parseArguments(argv) {
  const options = { aisdk: false, remoteAgents: false, triples: [], triple: null, noProbe: false, out: null, dryRun: false, force: false, check: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--help" || argument === "-h") {
      console.log(USAGE);
      process.exit(0);
    } else if (argument === "--aisdk") options.aisdk = true;
    else if (argument === "--remote-agents") options.remoteAgents = true;
    else if (argument === "--dry-run") options.dryRun = true;
    else if (argument === "--force") options.force = true;
    else if (argument === "--no-probe") options.noProbe = true;
    else if (argument === "--check") options.check = true;
    else if (argument === "--triple" || argument === "--out") {
      const value = argv[index + 1];
      if (!value || value.startsWith("--")) fail(`${argument} needs a value\n${USAGE}`);
      index += 1;
      if (argument === "--triple") options.triples.push(value);
      else options.out = path.resolve(value);
    } else fail(`Unsupported argument: ${argument}\n${USAGE}`);
  }
  for (const triple of options.triples) {
    try {
      assertTriple(triple);
    } catch (error) {
      fail(`${error.message}\n${USAGE}`);
    }
  }
  if (options.check) {
    const publishOnly = { "--aisdk": options.aisdk, "--remote-agents": options.remoteAgents, "--no-probe": options.noProbe, "--out": options.out, "--dry-run": options.dryRun, "--force": options.force };
    for (const [flag, given] of Object.entries(publishOnly)) {
      if (given) fail(`--check only looks at the channel and cannot be combined with ${flag}\n${USAGE}`);
    }
    options.triples = [...new Set(options.triples)];
    return options;
  }
  if (!options.aisdk && !options.remoteAgents) fail(`nothing to publish: pass --aisdk and/or --remote-agents (or --check)\n${USAGE}`);
  if (options.triples.length > 1) fail(`--triple can be given once when publishing (several only with --check)\n${USAGE}`);
  options.triple = options.triples[0] ?? null;
  if (options.noProbe && !options.aisdk) fail(`--no-probe is about the sidecar: it needs --aisdk\n${USAGE}`);
  return options;
}

const options = parseArguments(process.argv.slice(2));
const r2 = createR2Client({ label, userAgent: "mewrk-publish-components" });
const mebibytes = (bytes) => (bytes < 1024 * 1024 ? `${(bytes / 1024).toFixed(1)} KiB` : `${(bytes / 1024 / 1024).toFixed(1)} MiB`);

/** This computer's target triple, or null when rustc cannot say. */
function hostTriple() {
  try {
    return parseHostTriple(execFileSync("rustc", ["-vV"], { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }));
  } catch {
    return null;
  }
}

/** The time `file` was written, as the pointer's `builtAt`: second precision, never in the future. */
function builtAt(file) {
  const modified = Math.min(fs.statSync(file).mtimeMs, Date.now());
  return new Date(modified).toISOString().replace(/\.\d{3}Z$/u, "Z");
}

function readText(...segments) {
  return fs.readFileSync(path.join(root, ...segments), "utf8");
}

// ---------------------------------------------------------------------------
// What to publish

/**
 * One publishable build: its pointer, the pointer's channel key and the
 * compressed files (`blobs`, each with its channel key and bytes).
 */
function assemble({ component, triple, pointerKey, version, built, extra, sources }) {
  const packed = sources.map((source) => {
    log(`compressing ${source.name} (${mebibytes(source.bytes.length)}) for ${triple}`);
    return { ...packFile(source.name, source.bytes), source };
  });
  const pointer = buildPointer({ component, triple, version, builtAt: built, extra, files: packed.map((file) => file.entry) });
  const blobs = pointer.files.map((file, index) => ({ name: file.name, key: blobKey(pointerKey, file), size: file.size, sha256: file.sha256, bytes: packed[index].gz }));
  return { component, triple, pointerKey, pointer, blobs };
}

async function prepareSidecar(host, version) {
  const triple = options.triple ?? host;
  if (!triple) throw new Error("rustc -vV does not run here, so this computer's target triple is unknown: pass --triple");
  const protocol = protocolVersion(readText("aisdk-service", "src", "protocol.ts"), readText("src-tauri", "src", "aisdk", "protocol.rs"));
  const claudeAgentSdk = claudeAgentSdkPin(readText("aisdk-service", "package.json"));
  const installedSdk = path.join(root, "aisdk-service", "node_modules", "@anthropic-ai", "claude-agent-sdk", "package.json");
  if (fs.existsSync(installedSdk)) {
    const installed = JSON.parse(fs.readFileSync(installedSdk, "utf8")).version;
    if (installed !== claudeAgentSdk) warn(`aisdk-service/node_modules has the Claude Agent SDK ${installed}, the pin is ${claudeAgentSdk}: run npm ci in aisdk-service and rebuild the sidecar`);
  }

  const name = aisdkFileName(triple);
  const file = path.join(sidecarDirectory, name);
  // Decided before anything else is read: a build that cannot be started here is published unchecked only on request.
  const plan = probeDecision({ triple, host, noProbe: options.noProbe, protocol, claudeAgentSdk });
  if (!fs.existsSync(file)) {
    throw new Error(`${file} does not exist; run npm run build:sidecar${triple === host ? "" : " on a computer of that platform and copy the result here"}`);
  }

  if (plan.probe) {
    log(`starting ${name} to check it speaks protocol ${protocol}`);
    const probe = await probeSidecar({ command: file, protocol });
    if (!probe.ok) {
      throw new Error(`${file} is not a sidecar of protocol ${protocol}: ${probe.message}\nrebuild it from this tree (npm run build:sidecar) and run this again`);
    }
    log(`${name} ${probe.message}`);
  } else {
    warn("!".repeat(78));
    warn(plan.warning);
    warn("!".repeat(78));
  }

  const build = assemble({
    component: "aisdk",
    triple,
    pointerKey: aisdkPointerKey(protocol, triple),
    version,
    built: builtAt(file),
    extra: { protocol, claudeAgentSdk },
    sources: [{ name, bytes: fs.readFileSync(file) }],
  });
  return { ...build, warning: plan.warning };
}

/** The agent builds staged under `parent` (src-tauri/remote-agents by default, or just `only`), each with the source id read from its bytes. */
function stagedAgentBuilds(parent = agentsDirectory, only = null) {
  if (!fs.existsSync(parent)) return [];
  return fs
    .readdirSync(parent, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && (!only || entry.name === only))
    .map((entry) => {
      const directory = path.join(parent, entry.name);
      const files = fs.readdirSync(directory);
      const agent = ["mewrk-remote.exe", "mewrk-remote"].find((name) => files.includes(name));
      return { triple: entry.name, directory, files, source: agent ? agentSource(fs.readFileSync(path.join(directory, agent))) : null };
    });
}

function prepareAgents(host, version) {
  if (!host) throw new Error("rustc -vV does not run here, so which agent build is this computer's own is unknown: the agent source is read from it");
  const reference = ownAgentTriple(host);
  const builds = stagedAgentBuilds();
  const { source, publish, skipped } = selectAgentBuilds({ reference, builds, only: options.triple });
  log(`agent source ${source}, taken from this computer's ${reference} build`);
  // The installer carries this computer's agent from bundled-agents/ (`build:remote-agents -- --bundle`);
  // built from other source, it would not be the one the published builds are of.
  const bundled = path.join(root, "src-tauri", "bundled-agents", reference, agentFileNames(reference)[0]);
  if (fs.existsSync(bundled) && agentSource(fs.readFileSync(bundled)) !== source) {
    warn(`src-tauri/bundled-agents/${reference} is an agent of other source than the ${source.slice(0, 12)} published here: build both from the same commit (npm run build:remote-agents -- --bundle, and --release)`);
  }
  for (const entry of skipped) warn(`skipping the ${entry.triple} agent: ${entry.reason}`);
  if (publish.length === 0) throw new Error("no agent build to publish");
  return publish.map((triple) => {
    const build = builds.find((candidate) => candidate.triple === triple);
    const sources = agentFileNames(triple).map((name) => ({ name, bytes: fs.readFileSync(path.join(build.directory, name)) }));
    const agent = path.join(build.directory, sources[0].name);
    return assemble({
      component: "remote-agent",
      triple,
      pointerKey: agentPointerKey(source, triple),
      version,
      built: builtAt(agent),
      extra: { source },
      sources,
    });
  });
}

// ---------------------------------------------------------------------------
// The channel, as it is now

/** The pointer at `key` on the channel (or in --out), null when there is none; undefined when a dry run could not look. */
async function currentPointer(key) {
  if (options.out) {
    const file = path.join(options.out, key);
    return fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, "utf8")) : null;
  }
  const url = publicUrl(key);
  const headers = { "user-agent": "mewrk-publish-components", "accept-encoding": "identity" };
  let response;
  try {
    response = options.dryRun ? await fetch(url, { headers, signal: AbortSignal.timeout(15_000) }) : await r2.request(url, { headers });
  } catch (error) {
    if (options.dryRun) return undefined;
    throw error;
  }
  // R2 answers a missing key with either (the app reads it the same way).
  if (response.status === 404 || response.status === 403) return null;
  if (!response.ok) {
    if (options.dryRun) return undefined;
    throw new Error(`${url}: HTTP ${response.status}`);
  }
  try {
    return JSON.parse(await response.text());
  } catch {
    return { unreadable: true };
  }
}

// ---------------------------------------------------------------------------
// Writing it

function writeFileAtomic(file, bytes) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const partial = `${file}.partial`;
  fs.writeFileSync(partial, bytes);
  fs.renameSync(partial, file);
}

/** Writes the build under --out the way R2 would hold it, then reads it back the way the app would. */
function writeToDirectory(build) {
  for (const blob of build.blobs) writeFileAtomic(path.join(options.out, blob.key), blob.bytes);
  writeFileAtomic(path.join(options.out, build.pointerKey), pointerJson(build.pointer));
  const stored = JSON.parse(fs.readFileSync(path.join(options.out, build.pointerKey), "utf8"));
  validatePointer(stored, build.component, build.triple);
  for (const file of stored.files) verifyBlob(file, fs.readFileSync(path.join(options.out, blobKey(build.pointerKey, file))));
  for (const blob of build.blobs) log(`wrote ${blob.key} (${mebibytes(blob.size)})`);
  log(`wrote ${build.pointerKey}`);
}

async function uploadToBucket(credentials, build) {
  const replacedBlobs = [];
  for (const blob of build.blobs) {
    const key = bucketKey(blob.key);
    const stored = await r2.headObject(credentials, key);
    if (stored && stored.size === blob.size && (!stored.sha256 || stored.sha256 === blob.sha256)) {
      log(`${key} already stored`);
      continue;
    }
    await r2.putObject(credentials, key, blob.bytes, blob.sha256, BLOB_HEADERS);
    if (stored) replacedBlobs.push(publicUrl(blob.key));
    log(`${key} uploaded (${mebibytes(blob.size)}${stored ? ", replacing a different copy" : ""})`);
  }
  // A replaced blob is purged first, or the edge would answer with the old copy.
  if (replacedBlobs.length) await r2.purge(replacedBlobs);
  for (const blob of build.blobs) await r2.checkPublic(publicUrl(blob.key), blob.size, blob.sha256);
  log(`${build.blobs.length} file${build.blobs.length === 1 ? "" : "s"} readable at ${CHANNEL_URL}/`);

  const bytes = Buffer.from(pointerJson(build.pointer));
  const sha256 = sha256Hex(bytes);
  await r2.putObject(credentials, bucketKey(build.pointerKey), bytes, sha256, POINTER_HEADERS);
  log(`${bucketKey(build.pointerKey)} uploaded`);
  const pointerUrl = publicUrl(build.pointerKey);
  await r2.purge([pointerUrl]).catch((error) => warn(error.message));
  try {
    await r2.checkPublic(pointerUrl, bytes.length, sha256);
  } catch (error) {
    throw new Error(`${error.message}\nthe pointer is stored; if the CDN still holds the previous one it expires within 60 s`);
  }
  log(`${pointerUrl} serves the new pointer`);
}

function describe(build) {
  const { pointer } = build;
  const facts = pointer.component === "aisdk" ? `protocol ${pointer.protocol}, Claude Agent SDK ${pointer.claudeAgentSdk}` : `agent source ${pointer.source.slice(0, 12)}`;
  const files = pointer.files.map((file) => `${file.name} ${mebibytes(file.unpackedSize)} -> ${mebibytes(file.size)} gz`).join(", ");
  return `${pointer.component} ${build.triple}: ${facts}; ${files}`;
}

// ---------------------------------------------------------------------------
// Run: publishing

async function runPublish() {
  const host = hostTriple();
  const version = JSON.parse(readText("package.json")).version;
  if (!parseSemver(version)) throw new Error(`the root package.json version ${JSON.stringify(version)} is not a semantic version, so it cannot be told from a newer one already on the channel`);
  const upload = !options.dryRun && !options.out;
  if (upload) {
    if (!process.env.CLOUDFLARE_ACCOUNT_ID) throw new Error("CLOUDFLARE_ACCOUNT_ID is not set");
    if (!process.env.CLOUDFLARE_ZONE_ID) warn("CLOUDFLARE_ZONE_ID is not set: a replaced pointer cannot be purged from the CDN and is served stale for up to 60 s");
  }

  // Credentials first: a bad token should not wait behind the compression below.
  const credentials = upload ? await r2.s3Credentials() : null;

  const builds = [];
  if (options.aisdk) builds.push(await prepareSidecar(host, version));
  if (options.remoteAgents) builds.push(...prepareAgents(host, version));

  const target = options.dryRun ? "dry run" : options.out ? `writing under ${options.out}` : `${MIRROR.bucket}/components/ -> ${CHANNEL_URL}`;
  log(`${builds.length} build${builds.length === 1 ? "" : "s"}, version ${version}, ${target}`);

  // Every build is weighed against the channel before any is written, so a refusal leaves all of them unpublished.
  const planned = [];
  const refused = [];
  for (const build of builds) {
    log(describe(build));
    const existing = await currentPointer(build.pointerKey);
    const decision = existing === undefined ? { publish: true, reason: "(the channel could not be read, so whether it has this pointer is unknown)" } : publishDecision({ pointer: build.pointer, existing, force: options.force });
    planned.push({ build, decision });
    if (decision.refused) refused.push(`${build.pointerKey}: ${decision.reason}`);
  }
  if (refused.length) throw new Error(`refusing to publish, nothing was written:\n  ${refused.join("\n  ")}`);

  const published = [];
  const skipped = [];
  for (const { build, decision } of planned) {
    if (!decision.publish) {
      log(`skipping ${build.pointerKey}: ${decision.reason}`);
      skipped.push(build.pointerKey);
      continue;
    }
    if (decision.reason) log(decision.reason);
    if (options.dryRun) {
      log(`would publish ${build.pointerKey} and its ${build.blobs.length} file${build.blobs.length === 1 ? "" : "s"}:`);
      for (const blob of build.blobs) log(`  ${blob.key}`);
      console.log(pointerJson(build.pointer));
      continue;
    }
    if (options.out) writeToDirectory(build);
    else await uploadToBucket(credentials, build);
    published.push(build.pointerKey);
  }

  if (options.dryRun) log("dry run complete; nothing was written");
  else log(`${published.length} published${skipped.length ? `, ${skipped.length} left as they were` : ""}`);
  // The warning is repeated where it will be read: the end of the run.
  for (const { build } of planned) if (build.warning && (options.dryRun || published.includes(build.pointerKey))) warn(build.warning);
  if (process.env.GITHUB_STEP_SUMMARY && published.length) {
    fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${["### Components published", "", ...published.map((key) => `- ${publicUrl(key)}`)].join("\n")}\n`);
  }
}

// ---------------------------------------------------------------------------
// Run: the release gate

/**
 * This tree's agent source id, read the way publishing reads it: from this
 * computer's own agent build in src-tauri/remote-agents (or, failing that, the
 * one the installer carries, src-tauri/bundled-agents). `{ source, reference,
 * from }`, or `{ source: null, skip }` with why there is none to read.
 */
function localAgentSource(host) {
  if (!host) return { source: null, skip: "rustc -vV does not run here, so which agent build is this computer's own is unknown" };
  const reference = ownAgentTriple(host);
  const looked = [];
  for (const parent of [agentsDirectory, bundledAgentsDirectory]) {
    const relative = path.relative(root, parent).split(path.sep).join("/");
    const found = ownAgentSource({ reference, builds: stagedAgentBuilds(parent, reference) });
    if (found.source) return { source: found.source, reference, from: `${relative}/${reference}` };
    if (found.problem === "unidentified") throw new Error(`${relative}: ${found.message}`);
    looked.push(`${relative}/`);
  }
  return { source: null, skip: `there is no ${reference} agent build in ${looked.join(" or ")} to read the agent source id from (npm run build:remote-agents -- --release)` };
}

async function runCheck() {
  const protocol = protocolVersion(readText("aisdk-service", "src", "protocol.ts"), readText("src-tauri", "src", "aisdk", "protocol.rs"));
  const triples = options.triples.length ? options.triples : [...DEFAULT_CHECK_TRIPLES];
  const agent = localAgentSource(hostTriple());

  log(`checking ${CHANNEL_URL} for this tree: protocol ${protocol}, sidecars for ${triples.join(", ")}`);
  if (agent.source) log(`agent source ${agent.source} (read from this computer's ${agent.reference} agent, ${agent.from}), agents for ${AGENT_TARGETS.length} platforms`);
  else warn(`SKIPPING the agent pointers: ${agent.skip}`);

  const result = await checkChannel({ protocol, aisdkTriples: triples, source: agent.source, agentTriples: AGENT_TARGETS, fetchText: (url) => httpGetText(url) });
  for (const entry of result.entries) if (entry.ok) log(`ok   ${describeEntry(entry)}`);

  if (result.missing.length) {
    console.error(`${label} ${result.missing.length} of ${result.entries.length} pointers are missing or invalid:`);
    for (const entry of result.missing) console.error(`${label}   - ${describeEntry(entry)}`);
    process.exitCode = 1;
  } else {
    log(`all ${result.entries.length} pointers are on the channel`);
  }
  if (!result.agentsChecked) warn(`the agent pointers were NOT checked (${agent.skip}): the channel is not known to be complete`);
}

try {
  if (options.check) await runCheck();
  else await runPublish();
} catch (error) {
  fail(error.message);
}
