import { describe, expect, it } from "vitest";

import { type PadSnapshot, PadTracker } from "./gamepad";

function pad(
  over: Partial<PadSnapshot> = {},
  pressed: number[] = [],
  axes = [0, 0, 0, 0],
  values: Record<number, number> = {},
): PadSnapshot {
  const buttons = Array.from({ length: 17 }, (_, i) => ({
    pressed: pressed.includes(i),
    value: values[i] ?? (pressed.includes(i) ? 1 : 0),
  }));
  return { index: 0, mapping: "standard", buttons, axes, ...over };
}

describe("gamepads in the browser", () => {
  it("sends only what changed, by position", () => {
    const t = new PadTracker();
    expect(t.update([pad()])).toEqual([]);
    // A (bottom) and X (left = BTN_WEST) pressed, left stick fully left.
    expect(t.update([pad({}, [0, 2], [-1, 0, 0, 0])])).toEqual([
      { t: "pb", n: 0, c: 0x130, p: true },
      { t: "pb", n: 0, c: 0x134, p: true },
      { t: "pa", n: 0, a: 0x00, v: -32768 },
    ]);
    expect(t.update([pad({}, [0, 2], [-1, 0, 0, 0])])).toEqual([]);
    // Y is the top button (BTN_NORTH); right trigger half; d-pad up-right.
    const half = pad({}, [3, 7, 12, 15], [0, 0, 0, 1], { 7: 0.5 });
    expect(t.update([half])).toEqual([
      { t: "pb", n: 0, c: 0x130, p: false },
      { t: "pb", n: 0, c: 0x134, p: false },
      { t: "pb", n: 0, c: 0x133, p: true },
      { t: "pa", n: 0, a: 0x00, v: 0 },
      { t: "pa", n: 0, a: 0x04, v: 32767 },
      { t: "pa", n: 0, a: 0x05, v: 128 },
      { t: "pa", n: 0, a: 0x10, v: 1 },
      { t: "pa", n: 0, a: 0x11, v: -1 },
    ]);
  });

  it("releases a pad that goes away and ignores odd ones", () => {
    const t = new PadTracker();
    t.update([pad({ index: 1 }, [9], [0.5, 0, 0, 0])]);
    expect(t.update([null])).toEqual([
      { t: "pb", n: 1, c: 0x13b, p: false },
      { t: "pa", n: 1, a: 0x00, v: 0 },
    ]);
    expect(t.update([pad({ mapping: "" }, [0]), pad({ index: 7 }, [0])])).toEqual([]);
    expect(t.update([])).toEqual([]);
  });
});
