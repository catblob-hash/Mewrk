// The release gate of the component channel: whether what this tree needs is on
// https://dl.mewrk.dev/components. `node scripts/publish-components.mjs --check`
// runs it; this module is its logic and its one network call, kept apart from
// that script (which acts on import) so both can be tested.
//
// A release of the app is only as good as its channel: the installer carries
// neither the AI SDK sidecar nor any agent but its own platform's, so a host
// whose protocol generation (or agent source) has no pointer on the channel can
// never fetch them. Checked here, over plain public HTTPS GET, with no
// credentials: for this tree's `PROTOCOL_VERSION`, a valid sidecar pointer for
// each of the release platforms, and, for this tree's agent source id, a valid
// agent pointer for every platform the agent is built for.

import { AGENT_TARGETS, agentPointerKey, aisdkPointerKey, describePointer, publicUrl, validatePointer, validatePublishable } from "./components-plan.mjs";

/** What a missing key looks like: R2 answers with either (the app reads it the same way). */
const ABSENT = new Set([403, 404]);

const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

/**
 * One public GET of `url`: `{ status, text }` for any answer the server
 * gives, retried on a network error or a 5xx (what a run meets by chance);
 * throws with the last error when it never got one. Uncompressed, like the
 * publisher's own reads.
 */
export async function httpGetText(url, { timeoutMs = 15_000, attempts = 3, retryDelayMs = 2_000, fetchImpl = fetch } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    try {
      const response = await fetchImpl(url, {
        headers: { "user-agent": "mewrk-check-components", "accept-encoding": "identity" },
        signal: AbortSignal.timeout(timeoutMs),
      });
      const text = await response.text();
      if (response.status < 500 || attempt >= attempts) return { status: response.status, text };
    } catch (error) {
      if (attempt >= attempts) throw new Error(error?.cause?.code ? `${error.message} (${error.cause.code})` : (error?.message ?? String(error)));
    }
    await sleep(retryDelayMs * attempt);
  }
}

/** Why `body` is not the pointer the app needs under `key`, or null when it is one. */
function pointerProblem(body, { component, triple, protocol, source }) {
  let pointer;
  try {
    pointer = JSON.parse(body);
  } catch {
    return { problem: "the answer is not JSON", pointer: null };
  }
  try {
    validatePointer(pointer, component, triple);
    validatePublishable(pointer);
    if (component === "aisdk" && pointer.protocol !== protocol) {
      throw new Error(`it is for protocol ${pointer.protocol}, not the ${protocol} it is filed under`);
    }
    if (component === "remote-agent" && pointer.source !== source) {
      throw new Error(`it is for agent source ${String(pointer.source).slice(0, 12)}, not the ${source.slice(0, 12)} it is filed under`);
    }
  } catch (error) {
    return { problem: `not a valid pointer: ${error.message}`, pointer: null };
  }
  return { problem: null, pointer };
}

async function checkOne(fetchText, { component, triple, key, protocol, source }) {
  const url = publicUrl(key);
  const entry = { component, triple, key, url, ok: false, problem: null, pointer: null };
  let answer;
  try {
    answer = await fetchText(url);
  } catch (error) {
    entry.problem = `could not be read: ${error.message}`;
    return entry;
  }
  if (ABSENT.has(answer.status)) {
    entry.problem = `missing (HTTP ${answer.status})`;
  } else if (answer.status < 200 || answer.status >= 300) {
    entry.problem = `could not be read: HTTP ${answer.status}`;
  } else {
    const { problem, pointer } = pointerProblem(answer.text, { component, triple, protocol, source });
    entry.problem = problem;
    entry.pointer = pointer;
    entry.ok = problem === null;
  }
  return entry;
}

/**
 * Looks on the channel for what this tree needs. `fetchText(url)` answers
 * `{ status, text }` (or throws), so the network is the caller's: `httpGetText`
 * for real, a stand-in in tests.
 *
 * - `protocol` and `aisdkTriples`: `aisdk/p<protocol>/<triple>.json` must exist
 *   and be a valid sidecar pointer for each triple.
 * - `source`: this tree's agent source id; when it is null (no build here to
 *   read it from) the agent part is skipped and `agentsChecked` is false.
 *   Otherwise `remote-agent/<source>/<triple>.json` must exist and be a valid
 *   agent pointer for each of `agentTriples`.
 *
 * Returns `{ entries, agentsChecked, missing }`: every pointer looked at, in
 * order (`ok`, `problem` and the parsed `pointer`), and the entries that are not
 * `ok`.
 */
export async function checkChannel({ protocol, aisdkTriples, source = null, agentTriples = AGENT_TARGETS, fetchText }) {
  const wanted = aisdkTriples.map((triple) => ({ component: "aisdk", triple, key: aisdkPointerKey(protocol, triple), protocol, source: null }));
  if (source) wanted.push(...agentTriples.map((triple) => ({ component: "remote-agent", triple, key: agentPointerKey(source, triple), protocol: null, source })));
  const entries = await Promise.all(wanted.map((item) => checkOne(fetchText, item)));
  return { entries, agentsChecked: Boolean(source), missing: entries.filter((entry) => !entry.ok) };
}

/** One line about `entry`: what it is, then what it holds or what is wrong with it. */
export function describeEntry(entry) {
  const name = `${entry.component} ${entry.triple}`;
  if (!entry.ok) return `${name}: ${entry.problem} - ${entry.url}`;
  const { pointer } = entry;
  const facts = entry.component === "aisdk" ? `protocol ${pointer.protocol}, Claude Agent SDK ${pointer.claudeAgentSdk}` : `agent source ${pointer.source.slice(0, 12)}`;
  return `${name}: ${describePointer(pointer)}, ${facts}`;
}
