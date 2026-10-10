/**
 * Text from a phone's on-screen keyboard, turned into key presses for the
 * host. Those keyboards report characters, not keys, so the host's keyboard
 * layout decides which key (and Shift or AltGr) makes a character.
 */

export type HostLayout = "de" | "us";

// Linux key codes (linux/input-event-codes.h).
const SHIFT = 42;
const ALTGR = 100;
export const ENTER = 28;
export const BACKSPACE = 14;
const LETTERS: Record<string, number> = {
  q: 16, w: 17, e: 18, r: 19, t: 20, y: 21, u: 22, i: 23, o: 24, p: 25,
  a: 30, s: 31, d: 32, f: 33, g: 34, h: 35, j: 36, k: 37, l: 38,
  z: 44, x: 45, c: 46, v: 47, b: 48, n: 49, m: 50,
}; // prettier-ignore

interface Stroke {
  code: number;
  shift?: boolean;
  altgr?: boolean;
}

const plain = (code: number): Stroke => ({ code });
const shift = (code: number): Stroke => ({ code, shift: true });
const altgr = (code: number): Stroke => ({ code, altgr: true });

/** Characters beyond letters and digits, per layout. */
const SYMBOLS: Record<HostLayout, Record<string, Stroke>> = {
  us: {
    "!": shift(2), "@": shift(3), "#": shift(4), $: shift(5), "%": shift(6),
    "^": shift(7), "&": shift(8), "*": shift(9), "(": shift(10), ")": shift(11),
    "-": plain(12), _: shift(12), "=": plain(13), "+": shift(13),
    "[": plain(26), "{": shift(26), "]": plain(27), "}": shift(27),
    ";": plain(39), ":": shift(39), "'": plain(40), '"': shift(40),
    "`": plain(41), "~": shift(41), "\\": plain(43), "|": shift(43),
    ",": plain(51), "<": shift(51), ".": plain(52), ">": shift(52),
    "/": plain(53), "?": shift(53),
  },
  de: {
    "!": shift(2), '"': shift(3), "§": shift(4), $: shift(5), "%": shift(6),
    "&": shift(7), "/": shift(8), "(": shift(9), ")": shift(10), "=": shift(11),
    ß: plain(12), "?": shift(12), "\\": altgr(12),
    "@": altgr(16), "€": altgr(18),
    "{": altgr(8), "[": altgr(9), "]": altgr(10), "}": altgr(11),
    ü: plain(26), Ü: shift(26), "+": plain(27), "*": shift(27), "~": altgr(27),
    ö: plain(39), Ö: shift(39), ä: plain(40), Ä: shift(40),
    "#": plain(43), "'": shift(43), "°": shift(41),
    ",": plain(51), ";": shift(51), ".": plain(52), ":": shift(52),
    "-": plain(53), _: shift(53),
    "<": plain(86), ">": shift(86), "|": altgr(86),
  },
}; // prettier-ignore

function stroke(ch: string, layout: HostLayout): Stroke | undefined {
  if (ch === " ") return plain(57);
  if (ch === "\n") return plain(ENTER);
  if (ch === "\t") return plain(15);
  if (/^[0-9]$/.test(ch)) return plain(ch === "0" ? 11 : Number(ch) + 1);
  const lower = ch.toLowerCase();
  if (lower in LETTERS && /^[a-z]$/i.test(ch)) {
    // QWERTZ: the keys of Y and Z are swapped.
    const swapped = layout === "de" && (lower === "y" || lower === "z");
    const code = LETTERS[swapped ? (lower === "y" ? "z" : "y") : lower];
    if (code === undefined) return undefined;
    return ch === lower ? plain(code) : shift(code);
  }
  return SYMBOLS[layout][ch];
}

/** The key events that type `text` on a host with `layout`; characters the
 * layout has no key for are left out. */
export function typeText(text: string, layout: HostLayout): Record<string, unknown>[] {
  const out: Record<string, unknown>[] = [];
  const key = (c: number, p: boolean) => out.push({ t: "k", c, p });
  for (const ch of text) {
    const s = stroke(ch, layout);
    if (!s) continue;
    if (s.altgr) key(ALTGR, true);
    if (s.shift) key(SHIFT, true);
    key(s.code, true);
    key(s.code, false);
    if (s.shift) key(SHIFT, false);
    if (s.altgr) key(ALTGR, false);
  }
  return out;
}

/** Presses and releases one key. */
export function tapKey(code: number): Record<string, unknown>[] {
  return [
    { t: "k", c: code, p: true },
    { t: "k", c: code, p: false },
  ];
}

/**
 * What changed in a text field: how many characters were removed at the
 * end, and what was typed after. Phone keyboards rewrite whole words while
 * they suggest, which this turns into backspaces and the new letters.
 */
export function diffText(before: string, after: string): { backspaces: number; text: string } {
  const a = [...before];
  const b = [...after];
  let same = 0;
  while (same < a.length && same < b.length && a[same] === b[same]) same++;
  return { backspaces: a.length - same, text: b.slice(same).join("") };
}

/** The host's layout as this browser guesses it (its language). */
export function guessLayout(languages: readonly (string | undefined | null)[]): HostLayout {
  return languages.some((l) => typeof l === "string" && l.toLowerCase().startsWith("de"))
    ? "de"
    : "us";
}
