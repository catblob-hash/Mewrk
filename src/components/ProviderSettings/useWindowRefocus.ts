import { useEffect, useRef } from "react";

/**
 * Calls `onRefocus` when the app window regains focus after having lost it.
 *
 * A `focus` with no `blur` before it is not a return (the window was never away: a
 * focus event the page itself raised, or the first focus after mounting), so it asks
 * for nothing. `onRefocus` is read through a ref, so callers may pass a fresh closure
 * on every render without re-registering the listeners.
 */
export function useWindowRefocus(enabled: boolean, onRefocus: () => void): void {
  const callback = useRef(onRefocus);
  callback.current = onRefocus;
  useEffect(() => {
    if (!enabled) return undefined;
    let away = false;
    const onBlur = () => {
      away = true;
    };
    const onFocus = () => {
      if (!away) return;
      away = false;
      callback.current();
    };
    window.addEventListener("blur", onBlur);
    window.addEventListener("focus", onFocus);
    return () => {
      window.removeEventListener("blur", onBlur);
      window.removeEventListener("focus", onFocus);
    };
  }, [enabled]);
}
