// Proves that a sidecar executable starts and speaks the protocol before it is
// published: it is started the way the host starts it, sent the `hello` frame,
// and must answer `ready` with the same protocol generation. What the frames
// look like is components-plan.mjs; the host's side of it is `wait_for_ready` in
// src-tauri/src/aisdk/process.rs.

import { spawn } from "node:child_process";

import { helloFrame, parseSidecarLine, shutdownFrame } from "./components-plan.mjs";

/** The host waits this long for `ready` (`wait_for_ready`). */
export const READY_TIMEOUT_MS = 30_000;
const EXIT_GRACE_MS = 3_000;
const NOISE_LINES = 5;

/**
 * Starts `command`, sends `hello` and waits for `ready`. Resolves (never
 * rejects) with `{ ok, protocol, message }`: `ok` only when `ready` carries
 * `protocol`. The process is gone when it resolves.
 */
export function probeSidecar({ command, args = [], protocol, timeoutMs = READY_TIMEOUT_MS, env = process.env, cwd }) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(command, args, { stdio: ["pipe", "pipe", "pipe"], windowsHide: true, env, cwd });
    } catch (error) {
      resolve({ ok: false, protocol: null, message: `could not start ${command}: ${error.message}` });
      return;
    }

    let result = null;
    let closed = false;
    let stdoutBuffer = "";
    const stdoutNoise = [];
    let stderrText = "";
    let timer = null;
    let release = null;

    const evidence = () => {
      const parts = [];
      if (stdoutNoise.length) parts.push(`stdout: ${stdoutNoise.join(" | ")}`);
      const stderr = stderrText.trim().split(/\r?\n/u).slice(-NOISE_LINES).join(" | ");
      if (stderr) parts.push(`stderr: ${stderr}`);
      return parts.length ? ` (${parts.join("; ")})` : "";
    };

    // Settles with `answer` once the process is gone: asked to shut down, then killed when it
    // lingers, so nothing is left running behind a publish.
    const finish = (answer) => {
      if (result) return;
      result = answer;
      clearTimeout(timer);
      if (closed) {
        resolve(answer);
        return;
      }
      const killer = setTimeout(() => {
        child.kill();
        setTimeout(() => release?.(), EXIT_GRACE_MS);
      }, EXIT_GRACE_MS);
      release = () => {
        clearTimeout(killer);
        resolve(answer);
      };
      try {
        child.stdin.write(shutdownFrame(protocol));
        child.stdin.end();
      } catch {
        child.kill();
      }
    };

    child.on("error", (error) => {
      closed = true;
      finish({ ok: false, protocol: null, message: `could not run ${command}: ${error.message}` });
      release?.();
    });
    // `close`, not `exit`: the last of its stdout is still to be read when the process has ended.
    child.on("close", (code, signal) => {
      closed = true;
      finish({ ok: false, protocol: null, message: `the sidecar exited (${signal ?? `code ${code}`}) before it answered ready${evidence()}` });
      release?.();
    });
    child.stdin.on("error", () => {});
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (text) => {
      stderrText = (stderrText + text).slice(-4096);
    });
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (text) => {
      stdoutBuffer += text;
      let at;
      while ((at = stdoutBuffer.indexOf("\n")) >= 0) {
        const line = stdoutBuffer.slice(0, at).replace(/\r$/u, "");
        stdoutBuffer = stdoutBuffer.slice(at + 1);
        if (!line.trim()) continue;
        const frame = parseSidecarLine(line);
        if (frame?.type === "ready") {
          if (frame.protocol === protocol) {
            finish({ ok: true, protocol: frame.protocol, message: `answered ready with protocol ${frame.protocol}` });
          } else {
            finish({
              ok: false,
              protocol: frame.protocol ?? null,
              message: `the sidecar answered ready with protocol ${frame.protocol}, not the ${protocol} this tree speaks`,
            });
          }
          return;
        }
        if (stdoutNoise.length < NOISE_LINES) stdoutNoise.push(line.slice(0, 200));
      }
    });

    timer = setTimeout(() => {
      finish({ ok: false, protocol: null, message: `no ready frame within ${Math.round(timeoutMs / 1000)} s${evidence()}` });
    }, timeoutMs);

    try {
      child.stdin.write(helloFrame(protocol));
    } catch {
      // An exit already under way reports itself.
    }
  });
}
