import { describe, expect, it } from "vitest";

import { LONG_PRESS_MS, NO_ZOOM, TouchController, type TouchMode, type Zoom } from "./touch";

/** A controller over a 1000×500 box whose picture fills it exactly. */
function setup(mode: TouchMode = "direct") {
  const sent: Record<string, unknown>[] = [];
  const zooms: Zoom[] = [];
  let current: TouchMode = mode;
  const c = new TouchController(
    () => current,
    (x, y) => ({ x: Math.round((x / 1000) * 65535), y: Math.round((y / 500) * 65535) }),
    (a) => (a.kind === "send" ? sent.push(a.msg) : zooms.push(a.zoom)),
    () => ({ width: 1000, height: 500 }),
  );
  return {
    c,
    sent,
    zooms,
    setMode: (m: TouchMode) => (current = m),
  };
}

const LEFT = 0x110;
const RIGHT = 0x111;
const click = (c: number) => [
  { t: "b", c, p: true },
  { t: "b", c, p: false },
];

describe("TouchController, direct", () => {
  it("a tap clicks where the finger is", () => {
    const { c, sent } = setup();
    c.down(1, 500, 250, 0);
    c.up(1, 502, 251, 100);
    expect(sent).toEqual([{ t: "m", x: 32899, y: 32899 }, ...click(LEFT)]);
  });

  it("moving drags with the left button, from where it started", () => {
    const { c, sent } = setup();
    c.down(1, 100, 100, 0);
    c.move(1, 150, 100, 50);
    c.move(1, 200, 100, 80);
    c.up(1, 200, 100, 120);
    expect(sent).toEqual([
      { t: "m", x: 6554, y: 13107 },
      { t: "b", c: LEFT, p: true },
      { t: "m", x: 9830, y: 13107 },
      { t: "m", x: 13107, y: 13107 },
      { t: "b", c: LEFT, p: false },
    ]);
  });

  it("a long press is a right click, and the release does nothing more", () => {
    const { c, sent } = setup();
    c.down(1, 100, 100, 0);
    c.tick(LONG_PRESS_MS - 1);
    expect(sent).toEqual([]);
    c.tick(LONG_PRESS_MS);
    c.tick(LONG_PRESS_MS + 100);
    c.up(1, 100, 100, LONG_PRESS_MS + 200);
    expect(sent).toEqual([{ t: "m", x: 6554, y: 13107 }, ...click(RIGHT)]);
  });

  it("a tap that took too long is not a click", () => {
    const { c, sent } = setup();
    c.down(1, 100, 100, 0);
    c.move(1, 100, 100, LONG_PRESS_MS + 1);
    c.up(1, 100, 100, LONG_PRESS_MS + 2);
    // The long press fired on the move; no left click after it.
    expect(sent.filter((m) => m.c === LEFT)).toEqual([]);
  });
});

describe("TouchController, trackpad", () => {
  it("moving moves the pointer relatively, a tap clicks in place", () => {
    const { c, sent } = setup("trackpad");
    c.down(1, 100, 100, 0);
    c.move(1, 110, 95, 30);
    c.up(1, 110, 95, 60);
    c.down(1, 300, 300, 1000);
    c.up(1, 300, 300, 1050);
    expect(sent).toEqual([{ t: "r", dx: 16, dy: -8 }, ...click(LEFT)]);
  });

  it("a long press, then moving, drags", () => {
    const { c, sent } = setup("trackpad");
    c.down(1, 100, 100, 0);
    c.tick(LONG_PRESS_MS);
    expect(sent).toEqual([]);
    c.move(1, 120, 100, LONG_PRESS_MS + 50);
    c.up(1, 120, 100, LONG_PRESS_MS + 90);
    expect(sent).toEqual([
      { t: "b", c: LEFT, p: true },
      { t: "r", dx: 32, dy: 0 },
      { t: "b", c: LEFT, p: false },
    ]);
  });

  it("carries fractions of a pixel over", () => {
    const { c, sent } = setup("trackpad");
    c.down(1, 0, 0, 0);
    for (let i = 1; i <= 5; i++) c.move(1, i * 0.5, 0, i);
    c.cancel(1);
    const total = sent.reduce((s, m) => s + ((m.dx as number) ?? 0), 0);
    expect(total).toBe(4); // 2.5 px × 1.6
  });
});

describe("TouchController, two fingers", () => {
  it("a short tap with both is a right click", () => {
    const { c, sent } = setup();
    c.down(1, 100, 100, 0);
    c.down(2, 200, 100, 20);
    c.up(1, 100, 100, 120);
    c.up(2, 200, 100, 130);
    expect(sent).toEqual(click(RIGHT));
  });

  it("moving together scrolls, content following the fingers", () => {
    const { c, sent } = setup();
    c.down(1, 100, 300, 0);
    c.down(2, 200, 300, 10);
    c.move(1, 100, 270, 40);
    c.move(2, 200, 270, 50);
    c.up(1, 100, 270, 400);
    c.up(2, 200, 270, 410);
    const dy = sent.reduce((s, m) => s + ((m.dy as number) ?? 0), 0);
    expect(dy).toBe(-120); // up by 30 px: a notch downwards
    expect(sent.every((m) => m.t === "w")).toBe(true);
  });

  it("spreading zooms in around the fingers, panning moves, pinching back resets", () => {
    const { c, zooms, sent } = setup();
    c.down(1, 400, 250, 0);
    c.down(2, 600, 250, 10);
    c.move(1, 300, 250, 50);
    c.move(2, 700, 250, 60);
    const z = zooms.at(-1)!;
    expect(z.scale).toBeCloseTo(2);
    // The point between the fingers (500, 250) stays put.
    expect(500 - z.x).toBeCloseTo(500 * 2);
    expect(sent).toEqual([]);
    c.up(1, 300, 250, 100);
    c.up(2, 700, 250, 110);

    // Zoomed in, two fingers pan instead of scrolling.
    c.down(1, 400, 250, 1000);
    c.down(2, 600, 250, 1010);
    c.move(1, 380, 250, 1050);
    c.move(2, 580, 250, 1060);
    expect(zooms.at(-1)!.x).toBeLessThan(z.x);
    expect(sent).toEqual([]);
    c.cancel(1);
    c.cancel(2);

    c.resetZoom();
    expect(zooms.at(-1)).toEqual(NO_ZOOM);
  });

  it("never pans past the picture's edge", () => {
    const { c, zooms } = setup();
    c.down(1, 450, 250, 0);
    c.down(2, 550, 250, 0);
    c.move(1, 350, 250, 10);
    c.move(2, 650, 250, 10);
    c.cancel(1);
    c.cancel(2);
    c.down(1, 400, 250, 100);
    c.down(2, 600, 250, 100);
    for (let i = 1; i <= 50; i++) {
      c.move(1, 400 + i * 40, 250, 100 + i);
      c.move(2, 600 + i * 40, 250, 100 + i);
    }
    const z = zooms.at(-1)!;
    expect(z.x).toBe(0);
    expect(z.x).toBeGreaterThanOrEqual(1000 - 1000 * z.scale);
  });

  it("a second finger ends a drag the first one started", () => {
    const { c, sent } = setup();
    c.down(1, 100, 100, 0);
    c.move(1, 150, 100, 30);
    c.down(2, 300, 100, 40);
    expect(sent.at(-1)).toEqual({ t: "b", c: LEFT, p: false });
    // Lifting the fingers after a two-finger gesture clicks nothing.
    c.up(2, 300, 100, 400);
    c.up(1, 150, 100, 410);
    expect(sent.filter((m) => m.t === "b" && m.p === true)).toHaveLength(1);
  });

  it("ignores fingers it never saw", () => {
    const { c, sent } = setup();
    c.move(9, 1, 1, 0);
    c.up(9, 1, 1, 0);
    expect(sent).toEqual([]);
  });
});
