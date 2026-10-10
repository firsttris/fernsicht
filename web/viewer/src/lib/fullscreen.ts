import { useEffect, useState } from "react";

/** Chrome's Keyboard Lock (not in TypeScript's DOM types yet). */
interface KeyboardLock {
  lock?: (codes?: string[]) => Promise<void>;
  unlock?: () => void;
}

const keyboard = (): KeyboardLock | undefined =>
  (navigator as Navigator & { keyboard?: KeyboardLock }).keyboard;

/**
 * Fullscreen for the page, with the keyboard locked while in it: Chrome
 * and Edge then pass Meta, Alt+Tab and Esc to the page (holding Esc
 * leaves). Other browsers get plain fullscreen; Ctrl+Alt+Del never
 * reaches a page (the "send keys" menu has it).
 */
export function useFullscreen(): [boolean, (on: boolean) => void] {
  const [on, setOn] = useState(() => document.fullscreenElement !== null);
  useEffect(() => {
    const onChange = () => {
      const full = document.fullscreenElement !== null;
      setOn(full);
      if (full) {
        keyboard()
          ?.lock?.()
          .catch(() => {
            // Not allowed here: system keys stay with this computer.
          });
      } else {
        keyboard()?.unlock?.();
      }
    };
    document.addEventListener("fullscreenchange", onChange);
    return () => {
      document.removeEventListener("fullscreenchange", onChange);
      keyboard()?.unlock?.();
    };
  }, []);
  const set = (want: boolean) => {
    if (want) void document.documentElement.requestFullscreen?.().catch(() => {});
    else if (document.fullscreenElement) void document.exitFullscreen().catch(() => {});
  };
  return [on, set];
}
