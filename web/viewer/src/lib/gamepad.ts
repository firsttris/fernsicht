/**
 * Gamepads in the browser (Gamepad API, "standard" layout) as the host's
 * gamepad messages: buttons by position (BTN_SOUTH = bottom), sticks
 * -32768..32767, triggers 0..255, d-pad -1..1. Only changes are sent.
 */

/** Standard-layout button index → Linux button code. */
const BUTTONS: Record<number, number> = {
  0: 0x130, // bottom (A)
  1: 0x131, // right (B)
  2: 0x134, // left (X): BTN_WEST
  3: 0x133, // top (Y): BTN_NORTH
  4: 0x136, // left bumper
  5: 0x137, // right bumper
  8: 0x13a, // back / select
  9: 0x13b, // start
  10: 0x13d, // left stick click
  11: 0x13e, // right stick click
  16: 0x13c, // guide
};

/** Linux ABS codes of the protocol's axes. */
const ABS = {
  leftX: 0x00,
  leftY: 0x01,
  leftTrigger: 0x02,
  rightX: 0x03,
  rightY: 0x04,
  rightTrigger: 0x05,
  dpadX: 0x10,
  dpadY: 0x11,
} as const;

export const MAX_PADS = 4;

/** What a pad looks like (the parts of the browser's `Gamepad` we read). */
export interface PadSnapshot {
  index: number;
  mapping: string;
  buttons: readonly { pressed: boolean; value: number }[];
  axes: readonly number[];
}

export type PadMessage =
  { t: "pb"; n: number; c: number; p: boolean } | { t: "pa"; n: number; a: number; v: number };

// `|| 0`: a stick at rest is 0, never -0 (which would look like a change).
const stick = (v: number) => Math.max(-32768, Math.min(32767, Math.round(v * 32767.5 - 0.5))) || 0;
const trigger = (v: number) => Math.max(0, Math.min(255, Math.round(v * 255)));

/** The protocol state of one pad. */
function state(p: PadSnapshot): Map<string, PadMessage> {
  const m = new Map<string, PadMessage>();
  const n = p.index;
  for (const [i, code] of Object.entries(BUTTONS)) {
    const b = p.buttons[Number(i)];
    m.set(`b${code}`, { t: "pb", n, c: code, p: !!b?.pressed });
  }
  const axis = (a: number, v: number) => m.set(`a${a}`, { t: "pa", n, a, v });
  axis(ABS.leftX, stick(p.axes[0] ?? 0));
  axis(ABS.leftY, stick(p.axes[1] ?? 0));
  axis(ABS.rightX, stick(p.axes[2] ?? 0));
  axis(ABS.rightY, stick(p.axes[3] ?? 0));
  axis(ABS.leftTrigger, trigger(p.buttons[6]?.value ?? 0));
  axis(ABS.rightTrigger, trigger(p.buttons[7]?.value ?? 0));
  const held = (i: number) => (p.buttons[i]?.pressed ? 1 : 0);
  axis(ABS.dpadX, held(15) - held(14));
  axis(ABS.dpadY, held(13) - held(12));
  return m;
}

const atRest = (msg: PadMessage) => (msg.t === "pb" ? !msg.p : msg.v === 0);

/** Turns successive pad snapshots into the messages for what changed. */
export class PadTracker {
  private last = new Map<number, Map<string, PadMessage>>();

  update(pads: readonly (PadSnapshot | null)[]): PadMessage[] {
    const out: PadMessage[] = [];
    const seen = new Set<number>();
    for (const p of pads) {
      if (!p || p.index >= MAX_PADS || p.mapping !== "standard") continue;
      seen.add(p.index);
      const before = this.last.get(p.index);
      const now = state(p);
      for (const [key, msg] of now) {
        const old = before?.get(key);
        const changed = old ? JSON.stringify(old) !== JSON.stringify(msg) : !atRest(msg);
        if (changed) out.push(msg);
      }
      this.last.set(p.index, now);
    }
    // A pad that went away: everything released.
    for (const [n, before] of this.last) {
      if (seen.has(n)) continue;
      for (const msg of before.values()) {
        if (!atRest(msg)) out.push(msg.t === "pb" ? { ...msg, p: false } : { ...msg, v: 0 });
      }
      this.last.delete(n);
    }
    return out;
  }
}
