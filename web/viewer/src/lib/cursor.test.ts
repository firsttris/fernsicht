import { describe, expect, it } from "vitest";

import {
  CURSOR_CHUNK,
  CursorAssembler,
  bgraPremultipliedToRgba,
  decodeCursorPacket,
} from "./cursor";

function position(visible: boolean, x: number, y: number): ArrayBuffer {
  const b = new ArrayBuffer(24);
  const v = new DataView(b);
  v.setUint8(0, 0xf5);
  v.setUint8(1, 1);
  v.setUint8(2, 8);
  v.setUint8(3, visible ? 1 : 0);
  v.setUint32(4, 7, true);
  v.setUint32(8, 3, true);
  v.setInt32(12, x, true);
  v.setInt32(16, y, true);
  v.setUint16(20, 2560, true);
  v.setUint16(22, 1440, true);
  return b;
}

function shape(serial: number, w: number, h: number, offset: number, fill: number): ArrayBuffer {
  const len = Math.min(CURSOR_CHUNK, w * h * 4 - offset);
  const b = new ArrayBuffer(20 + len);
  const v = new DataView(b);
  v.setUint8(0, 0xf5);
  v.setUint8(1, 1);
  v.setUint8(2, 9);
  v.setUint32(8, serial, true);
  v.setUint16(12, w, true);
  v.setUint16(14, h, true);
  v.setUint32(16, offset, true);
  new Uint8Array(b, 20).fill(fill);
  return b;
}

describe("pointer packets", () => {
  it("reads positions, also off the screen's edge", () => {
    expect(decodeCursorPacket(position(true, -4, 100))).toEqual({
      kind: "position",
      visible: true,
      serial: 3,
      x: -4,
      y: 100,
      screenWidth: 2560,
      screenHeight: 1440,
    });
    expect(decodeCursorPacket(position(false, 0, 0))).toMatchObject({ visible: false });
  });

  it("refuses what is not a pointer packet", () => {
    expect(decodeCursorPacket(new ArrayBuffer(2))).toBeNull();
    const wrongMagic = position(true, 0, 0);
    new DataView(wrongMagic).setUint8(0, 0);
    expect(decodeCursorPacket(wrongMagic)).toBeNull();
    const video = position(true, 0, 0);
    new DataView(video).setUint8(2, 1);
    expect(decodeCursorPacket(video)).toBeNull();
    expect(decodeCursorPacket(position(true, 0, 0).slice(0, 20))).toBeNull();
    expect(decodeCursorPacket(shape(0, 4, 4, 0, 1))).toBeNull(); // serial 0
    expect(decodeCursorPacket(shape(1, 300, 4, 0, 1))).toBeNull(); // too wide
    expect(decodeCursorPacket(shape(1, 4, 4, 0, 1).slice(0, 30))).toBeNull(); // short piece
    expect(decodeCursorPacket(shape(1, 4, 4, 0, 1).slice(0, 10))).toBeNull();
  });

  it("puts a shape together from its pieces", () => {
    // 32×16 pixels = 2048 bytes = two pieces.
    const a = new CursorAssembler();
    const first = decodeCursorPacket(shape(5, 32, 16, 0, 0x80));
    const second = decodeCursorPacket(shape(5, 32, 16, 1024, 0x40));
    expect(first?.kind).toBe("shape");
    expect(a.add(first as never)).toBeNull();
    const img = a.add(second as never)!;
    expect(img).toMatchObject({ serial: 5, width: 32, height: 16 });
    expect(img.rgba.length).toBe(2048);
    // A repeat of a piece does not complete a new shape.
    expect(a.add(decodeCursorPacket(shape(6, 32, 16, 0, 1)) as never)).toBeNull();
    expect(a.add(decodeCursorPacket(shape(6, 32, 16, 0, 1)) as never)).toBeNull();
    expect(a.add(decodeCursorPacket(shape(6, 32, 16, 1024, 1)) as never)?.serial).toBe(6);
  });

  it("turns premultiplied BGRA into straight RGBA", () => {
    // Half-transparent pure red (premultiplied: R = 128 at A = 128), and a
    // fully transparent pixel.
    const out = bgraPremultipliedToRgba(new Uint8Array([0, 0, 128, 128, 9, 9, 9, 0]));
    expect([...out]).toEqual([255, 0, 0, 128, 0, 0, 0, 0]);
  });
});
