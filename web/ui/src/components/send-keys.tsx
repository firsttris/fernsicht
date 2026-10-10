import { Command } from "lucide-react";

import { MenuButton } from "./menu-button";

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
  return (
    <MenuButton
      label="Tasten senden"
      icon={<Command size={16} />}
      items={KEY_COMBOS.map((k) => ({ label: k.label, onSelect: () => onSend(k.codes) }))}
    />
  );
}
