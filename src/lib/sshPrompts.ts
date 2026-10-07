import { hasBackendRuntime, invoke } from "./backend";

/**
 * A question an SSH connection is waiting on the user for. Mirror of Rust
 * `ssh_askpass::SshPrompt`.
 *
 * - `hostKey`: a host key met for the first time; accepting it lets `ssh` add it to
 *   `~/.ssh/known_hosts`. A key that differs from an accepted one is refused without asking.
 * - `secret`: a password, key passphrase or PIN. The host keeps it in memory only, until Mewrk
 *   quits, so the next connection to the same machine does not ask again.
 * - `confirm`: another yes/no `ssh` asks.
 * - `notice`: something to do elsewhere (touch a security key); `ssh` takes it down itself.
 */
export interface SshPrompt {
  id: string;
  /** The machine as its `ssh` destination names it. */
  machine: string;
  kind: "hostKey" | "secret" | "confirm" | "notice";
  /** What `ssh` asked, verbatim. */
  prompt: string;
  hostKey: { host: string; keyType: string; fingerprint: string } | null;
  /** The answer last given to this question was not accepted. */
  retry: boolean;
}

/** The questions still waiting, for a renderer that just started; new ones arrive as push events. */
export async function listSshPrompts(): Promise<SshPrompt[]> {
  if (!hasBackendRuntime()) return [];
  return invoke<SshPrompt[]>("list_ssh_prompts");
}

/**
 * Answers a question: the typed secret, any value to accept a host key or confirm, or `null` to
 * turn it down — after which that machine is not asked again until it is probed from its settings.
 */
export async function answerSshPrompt(id: string, answer: string | null): Promise<void> {
  await invoke("answer_ssh_prompt", { id, answer });
}

/** Adds a newly announced question, once, keeping announcement order. */
export function withSshPrompt(prompts: SshPrompt[], prompt: SshPrompt): SshPrompt[] {
  return prompts.some((existing) => existing.id === prompt.id) ? prompts : [...prompts, prompt];
}

/** Takes a settled question away. */
export function withoutSshPrompt(prompts: SshPrompt[], id: string): SshPrompt[] {
  return prompts.some((prompt) => prompt.id === id) ? prompts.filter((prompt) => prompt.id !== id) : prompts;
}
