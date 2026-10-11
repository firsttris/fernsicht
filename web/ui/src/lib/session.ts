import type { SessionMode } from "../types";

/** The session modes, the first the usual one. */
export const SESSION_MODES: readonly { mode: SessionMode; label: string; hint: string }[] = [
  { mode: "desktop", label: "Desktop", hint: "Maus und Tastatur, der Zeiger bleibt frei" },
  { mode: "gaming", label: "Gaming", hint: "Zeiger gefangen (Pointer-Lock), Gamepads" },
];

/** A session page's search: the mode, desktop unless it says gaming. */
export function sessionSearch(search: Record<string, unknown>): { mode: SessionMode } {
  return { mode: search.mode === "gaming" ? "gaming" : "desktop" };
}

/** Whether `pin` is a whole pairing PIN. */
export const isPin = (pin: string) => /^\d{6}$/.test(pin);

/** The digits of a device ID as typed ("482 913 057"). */
export const deviceIdDigits = (id: string) => id.replace(/\D/g, "");

/** Whether `id` (as typed) is a whole device ID. */
export const isDeviceId = (id: string) => deviceIdDigits(id).length === 9;

/** Why pairing failed, in the same words in the app and the browser. */
export const PAIRING_ERROR_TEXT = {
  "wrong-pin": "Falsche PIN.",
  "too-many-attempts": "Zu viele falsche PINs. Am Host die Kopplung neu öffnen.",
  "pairing-closed": "Am Host ist gerade keine Kopplung offen.",
} as const;
