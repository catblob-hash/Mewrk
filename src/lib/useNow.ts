import { useEffect, useState } from "react";

/**
 * The wall clock, re-read once a second for as long as `active` holds.
 *
 * A running task's elapsed column is `now - startedAt`, so it only advances
 * when `now` does. Nothing upstream will do that for it: a subagent can sit in
 * one long tool call, a dev server never reports anything after it starts, and
 * a memo keyed on their records recomputes only when a record changes. Left to
 * those, a row freezes until something unrelated remounts the panel.
 *
 * Armed only while something is running, because finished rows are frozen at a
 * real duration and must not make the whole surface re-render every second.
 */
export function useNow(active: boolean, intervalMs = 1_000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return undefined;
    // Re-read on arming too: the value from the previous active stretch, or
    // from mount, is as old as the idle time since.
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(timer);
  }, [active, intervalMs]);
  return now;
}
