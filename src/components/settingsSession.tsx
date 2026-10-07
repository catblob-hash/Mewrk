import { createContext, useContext, useState } from "react";
import type { ReactNode } from "react";

/**
 * What one opening of the settings window has already loaded.
 *
 * A page that asks the host something slow (npm, a CLI) reads it once per session:
 * moving to another page or provider and back reuses the answer, while closing the
 * settings window and opening it again starts over. `GlobalSettings` mounts one
 * session per opening, and the dialog unmounts it on close.
 *
 * The cache holds the last answer only; what a page shows meanwhile (and whether it
 * keeps an older answer on screen while a newer one loads) is the page's own business.
 */
export interface SettingsSession {
  /** The value `key` holds, when a load of it has finished in this session. */
  peek<T>(key: string): T | undefined;
  /** Records `value` under `key`, replacing an earlier one: the answer of a forced refresh. */
  put<T>(key: string, value: T): void;
  /**
   * `key`'s value: the remembered one, else the answer of `load`. The load starts once
   * per key, so a page shown again while its first load is still running joins it
   * instead of asking twice. A failed load is not remembered: the next visit asks again.
   */
  load<T>(key: string, load: () => Promise<T>): Promise<T>;
}

export function createSettingsSession(): SettingsSession {
  const values = new Map<string, unknown>();
  const loading = new Map<string, Promise<unknown>>();
  // A `put` that lands while a load is still running is newer than that load's answer.
  const generations = new Map<string, number>();
  const generationOf = (key: string) => generations.get(key) ?? 0;
  return {
    peek<T>(key: string): T | undefined {
      return values.get(key) as T | undefined;
    },
    put<T>(key: string, value: T): void {
      generations.set(key, generationOf(key) + 1);
      values.set(key, value);
    },
    load<T>(key: string, load: () => Promise<T>): Promise<T> {
      if (values.has(key)) return Promise.resolve(values.get(key) as T);
      const running = loading.get(key);
      if (running) return running as Promise<T>;
      const generation = generationOf(key);
      const started = load().then(
        (value) => {
          loading.delete(key);
          if (generationOf(key) === generation) values.set(key, value);
          return value;
        },
        (reason: unknown) => {
          loading.delete(key);
          throw reason;
        }
      );
      loading.set(key, started);
      return started;
    }
  };
}

const SettingsSessionContext = createContext<SettingsSession | null>(null);

export function SettingsSessionProvider({ children }: { children: ReactNode }) {
  const [session] = useState(createSettingsSession);
  return <SettingsSessionContext.Provider value={session}>{children}</SettingsSessionContext.Provider>;
}

/**
 * The enclosing settings session. A page rendered outside one (a test, a future
 * embedding) gets a session of its own that lives as long as the page does, which is
 * the older "ask on every mount" behavior.
 */
export function useSettingsSession(): SettingsSession {
  const shared = useContext(SettingsSessionContext);
  const [own] = useState(createSettingsSession);
  return shared ?? own;
}
