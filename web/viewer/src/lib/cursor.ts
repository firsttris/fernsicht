/**
 * The host's pointer, as it arrives on the data channel: the same packets
 * as in the native protocol (crates/proto), little-endian.
 *
 * Cursor (24 bytes): magic, version, kind 8, flags (1 = visible),
 * session id, shape serial, x, y (i32, top-left corner on the screen),
 * screen width, screen height (u16).
 *
 * CursorShape (20 bytes + piece): magic, version, kind 9, flags, session
 * id, serial, width, height (u16), offset (u32); then up to 1024 bytes of
 * the image from `offset` on, B G R A per pixel, premultiplied alpha.
 */

const MAGIC = 0xf5;
const VERSION = 1;
const KIND_CURSOR = 8;
const KIND_SHAPE = 9;
export const CURSOR_CHUNK = 1024;
const MAX_CURSOR_SIZE = 256;

export interface CursorPosition {
  kind: "position";
  visible: boolean;
  serial: number;
  x: number;
  y: number;
  screenWidth: number;
  screenHeight: number;
}

export interface CursorPiece {
  kind: "shape";
  serial: number;
  width: number;
  height: number;
  offset: number;
  data: Uint8Array;
}

/** Reads one pointer packet; anything else or anything broken is `null`. */
export function decodeCursorPacket(buf: ArrayBuffer): CursorPosition | CursorPiece | null {
  const v = new DataView(buf);
  if (v.byteLength < 4 || v.getUint8(0) !== MAGIC || v.getUint8(1) !== VERSION) return null;
  const kind = v.getUint8(2);
  if (kind === KIND_CURSOR) {
    if (v.byteLength !== 24) return null;
    return {
      kind: "position",
      visible: (v.getUint8(3) & 1) === 1,
      serial: v.getUint32(8, true),
      x: v.getInt32(12, true),
      y: v.getInt32(16, true),
      screenWidth: v.getUint16(20, true),
      screenHeight: v.getUint16(22, true),
    };
  }
  if (kind === KIND_SHAPE) {
    if (v.byteLength < 20) return null;
    const serial = v.getUint32(8, true);
    const width = v.getUint16(12, true);
    const height = v.getUint16(14, true);
    const offset = v.getUint32(16, true);
    const total = width * height * 4;
    const data = new Uint8Array(buf, 20);
    const ok =
      serial !== 0 &&
      width > 0 &&
      height > 0 &&
      width <= MAX_CURSOR_SIZE &&
      height <= MAX_CURSOR_SIZE &&
      offset % CURSOR_CHUNK === 0 &&
      offset < total &&
      data.length === Math.min(CURSOR_CHUNK, total - offset);
    return ok ? { kind: "shape", serial, width, height, offset, data } : null;
  }
  return null;
}

export interface CursorImage {
  serial: number;
  width: number;
  height: number;
  /** Straight-alpha RGBA, ready for `ImageData`. */
  rgba: Uint8ClampedArray<ArrayBuffer>;
}

/** Puts a shape's pieces together; a piece of a newer shape starts over. */
export class CursorAssembler {
  private serial = 0;
  private bytes = new Uint8Array(0);
  private have = new Set<number>();
  private width = 0;
  private height = 0;

  /** Returns the image once its last missing piece arrived. */
  add(p: CursorPiece): CursorImage | null {
    if (p.serial !== this.serial || p.width !== this.width || p.height !== this.height) {
      this.serial = p.serial;
      this.width = p.width;
      this.height = p.height;
      this.bytes = new Uint8Array(p.width * p.height * 4);
      this.have.clear();
    }
    this.bytes.set(p.data, p.offset);
    this.have.add(p.offset);
    if (this.have.size * CURSOR_CHUNK < this.bytes.length) return null;
    this.have.clear();
    return {
      serial: this.serial,
      width: this.width,
      height: this.height,
      rgba: bgraPremultipliedToRgba(this.bytes),
    };
  }
}

/** DRM ARGB8888 (B, G, R, A in memory, premultiplied) to straight RGBA. */
export function bgraPremultipliedToRgba(bgra: Uint8Array): Uint8ClampedArray<ArrayBuffer> {
  const out = new Uint8ClampedArray(bgra.length);
  for (let i = 0; i < bgra.length; i += 4) {
    const a = bgra[i + 3]!;
    const un = (c: number) => (a === 0 ? 0 : Math.round((c * 255) / a));
    out[i] = un(bgra[i + 2]!);
    out[i + 1] = un(bgra[i + 1]!);
    out[i + 2] = un(bgra[i]!);
    out[i + 3] = a;
  }
  return out;
}
