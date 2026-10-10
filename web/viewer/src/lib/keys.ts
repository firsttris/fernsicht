/**
 * The browser's physical keys (`KeyboardEvent.code`) as Linux key codes
 * (`KEY_*` in linux/input-event-codes.h). Physical keys, not characters:
 * the host's keyboard layout decides what they type, as with a real
 * keyboard plugged into it.
 */
const KEYS: Record<string, number> = {
  Escape: 1,
  Minus: 12,
  Equal: 13,
  Backspace: 14,
  Tab: 15,
  BracketLeft: 26,
  BracketRight: 27,
  Enter: 28,
  ControlLeft: 29,
  Semicolon: 39,
  Quote: 40,
  Backquote: 41,
  ShiftLeft: 42,
  Backslash: 43,
  Comma: 51,
  Period: 52,
  Slash: 53,
  ShiftRight: 54,
  NumpadMultiply: 55,
  AltLeft: 56,
  Space: 57,
  CapsLock: 58,
  NumLock: 69,
  ScrollLock: 70,
  Numpad7: 71,
  Numpad8: 72,
  Numpad9: 73,
  NumpadSubtract: 74,
  Numpad4: 75,
  Numpad5: 76,
  Numpad6: 77,
  NumpadAdd: 78,
  Numpad1: 79,
  Numpad2: 80,
  Numpad3: 81,
  Numpad0: 82,
  NumpadDecimal: 83,
  IntlBackslash: 86,
  F11: 87,
  F12: 88,
  IntlRo: 89,
  NumpadEnter: 96,
  ControlRight: 97,
  NumpadDivide: 98,
  PrintScreen: 99,
  AltRight: 100,
  Home: 102,
  ArrowUp: 103,
  PageUp: 104,
  ArrowLeft: 105,
  ArrowRight: 106,
  End: 107,
  ArrowDown: 108,
  PageDown: 109,
  Insert: 110,
  Delete: 111,
  AudioVolumeMute: 113,
  AudioVolumeDown: 114,
  AudioVolumeUp: 115,
  NumpadEqual: 117,
  Pause: 119,
  NumpadComma: 121,
  IntlYen: 124,
  MetaLeft: 125,
  MetaRight: 126,
  ContextMenu: 127,
  MediaTrackNext: 163,
  MediaPlayPause: 164,
  MediaTrackPrevious: 165,
  MediaStop: 166,
};

const ROW_QWERTY = "QWERTYUIOP";
const ROW_ASDF = "ASDFGHJKL";
const ROW_ZXCV = "ZXCVBNM";

/** The Linux key code for a `KeyboardEvent.code`; `undefined` if unknown. */
export function linuxKeyCode(code: string): number | undefined {
  if (Object.hasOwn(KEYS, code)) return KEYS[code];
  let m = /^Digit(\d)$/.exec(code);
  if (m) return m[1] === "0" ? 11 : 1 + Number(m[1]);
  m = /^Key([A-Z])$/.exec(code);
  if (m) {
    const c = m[1]!;
    for (const [row, start] of [
      [ROW_QWERTY, 16],
      [ROW_ASDF, 30],
      [ROW_ZXCV, 44],
    ] as const) {
      const i = row.indexOf(c);
      if (i >= 0) return start + i;
    }
  }
  m = /^F(\d+)$/.exec(code);
  if (m) {
    const n = Number(m[1]);
    if (n >= 1 && n <= 10) return 58 + n;
    if (n >= 13 && n <= 24) return 170 + n;
  }
  return undefined;
}
