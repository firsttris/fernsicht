import { Keyboard } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "./button";

/** A key combination for the host, as Linux key codes (pressed in order). */
export interface KeyCombo {
  label: string;
  codes: number[];
}

// Linux input key codes (linux/input-event-codes.h).
const CTRL = 29;
const ALT = 56;
const META = 125;

/**
 * Keys this computer's desktop keeps for itself, so they cannot simply be
 * typed into the session (and in a browser never reach the page).
 */
export const KEY_COMBOS: KeyCombo[] = [
  { label: "Strg+Alt+Entf", codes: [CTRL, ALT, 111] },
  { label: "Windows-Taste", codes: [META] },
  { label: "Windows+W", codes: [META, 17] },
  { label: "Alt+Tab", codes: [ALT, 15] },
  { label: "Alt+F4", codes: [ALT, 62] },
  { label: "Druck", codes: [99] },
];

/** Toolbar button with a menu of key combinations to send to the host. */
export function SendKeysMenu({ onSend }: { onSend: (codes: number[]) => void }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    const onPointer = (e: PointerEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("pointerdown", onPointer);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("pointerdown", onPointer);
    };
  }, [open]);

  return (
    <div ref={root} className="relative">
      <Button
        size="icon"
        variant="ghost"
        aria-label="Tasten senden"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <Keyboard size={16} />
      </Button>
      {open && (
        <div
          role="menu"
          aria-label="Tasten senden"
          className="absolute top-full left-1/2 z-10 mt-2 flex min-w-44 -translate-x-1/2 flex-col rounded-lg border border-border bg-overlay p-1 backdrop-blur"
        >
          {KEY_COMBOS.map((k) => (
            <button
              key={k.label}
              type="button"
              role="menuitem"
              className="cursor-pointer rounded-md border-0 bg-transparent px-3 py-1.5 text-left text-sm text-foreground hover:bg-secondary"
              onClick={() => {
                onSend(k.codes);
                setOpen(false);
              }}
            >
              {k.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
