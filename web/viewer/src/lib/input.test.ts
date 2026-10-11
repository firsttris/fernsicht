import { describe, expect, it, vi } from "vitest";

import {
  NO_WHEEL_REST,
  wheelStep,
  contentRect,
  linuxButton,
  lockPointer,
  toAbsolute,
} from "./input";

describe("pointer for games", () => {
  it("asks for raw movement, else the plain lock, and swallows a refusal", async () => {
    const el = document.createElement("div");
    const raw = vi.fn(() => Promise.resolve());
    Object.assign(el, { requestPointerLock: raw });
    await lockPointer(el);
    expect(raw).toHaveBeenCalledWith({ unadjustedMovement: true });

    // A browser without raw movement refuses the option: the plain lock.
    const plain = vi.fn((o?: object) => (o ? Promise.reject(new Error("no")) : Promise.resolve()));
    Object.assign(el, { requestPointerLock: plain });
    await lockPointer(el);
    expect(plain.mock.calls).toEqual([[{ unadjustedMovement: true }], []]);

    // Not allowed at all (no click): no error.
    Object.assign(el, { requestPointerLock: () => Promise.reject(new Error("no")) });
    await expect(lockPointer(el)).resolves.toBeUndefined();
  });
});

describe("mouse", () => {
  it("maps buttons", () => {
    expect([0, 1, 2, 3, 4].map(linuxButton)).toEqual([0x110, 0x112, 0x111, 0x113, 0x114]);
    expect(linuxButton(5)).toBeUndefined();
  });

  it("finds the picture inside a letterboxed video", () => {
    const box = { left: 0, top: 0, width: 1000, height: 1000 };
    // 16:9 in a square: bars above and below.
    const r = contentRect(box, 1920, 1080);
    expect(r.left).toBeCloseTo(0);
    expect(r.width).toBeCloseTo(1000);
    expect(r.top).toBeCloseTo(218.75);
    expect(r.height).toBeCloseTo(562.5);
    // Tall picture in a wide box: bars left and right.
    expect(contentRect({ ...box, width: 2000 }, 1000, 1000)).toEqual({
      left: 500,
      top: 0,
      width: 1000,
      height: 1000,
    });
    // No picture yet: the whole box.
    expect(contentRect(box, 0, 0)).toBe(box);
  });

  it("gives positions on the picture from 0 to 65535, clamped", () => {
    const pic = { left: 100, top: 50, width: 200, height: 100 };
    expect(toAbsolute(100, 50, pic)).toEqual({ x: 0, y: 0 });
    expect(toAbsolute(300, 150, pic)).toEqual({ x: 65535, y: 65535 });
    expect(toAbsolute(200, 100, pic)).toEqual({ x: 32768, y: 32768 });
    expect(toAbsolute(0, 999, pic)).toEqual({ x: 0, y: 65535 });
    expect(toAbsolute(5, 5, { left: 0, top: 0, width: 0, height: 0 })).toEqual({ x: 0, y: 0 });
  });

  it("turns wheel events into notches of 120, up positive", () => {
    let rest = NO_WHEEL_REST;
    const add = (e: { deltaX: number; deltaY: number; deltaMode: number }) => {
      const r = wheelStep(rest, e);
      rest = r.rest;
      return { dx: r.dx, dy: r.dy };
    };
    // A mouse notch in pixels (about 100 px): one notch down.
    expect(add({ deltaX: 0, deltaY: 100, deltaMode: 0 })).toEqual({ dx: 0, dy: -120 });
    // Lines: 3 per notch.
    expect(add({ deltaX: 0, deltaY: -3, deltaMode: 1 })).toEqual({ dx: 0, dy: 120 });
    // Pages.
    expect(add({ deltaX: 1, deltaY: 0, deltaMode: 2 })).toEqual({ dx: 360, dy: 0 });
    // Touchpad fractions carry over.
    expect(add({ deltaX: 0.5, deltaY: 0, deltaMode: 0 })).toEqual({ dx: 0, dy: 0 });
    expect(add({ deltaX: 0.5, deltaY: 0, deltaMode: 0 })).toEqual({ dx: 1, dy: 0 });
  });
});
