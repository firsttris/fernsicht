/** Mouse input in the browser, turned into the host's input messages. */

/** Browser `MouseEvent.button` → Linux `BTN_*`. */
const BUTTONS = [0x110, 0x112, 0x111, 0x113, 0x114];

export function linuxButton(button: number): number | undefined {
  return BUTTONS[button];
}

/** The part of an element a video with `object-fit: contain` covers. */
export interface Rect {
  left: number;
  top: number;
  width: number;
  height: number;
}

export function contentRect(box: Rect, videoWidth: number, videoHeight: number): Rect {
  if (videoWidth <= 0 || videoHeight <= 0) return box;
  const scale = Math.min(box.width / videoWidth, box.height / videoHeight);
  const width = videoWidth * scale;
  const height = videoHeight * scale;
  return {
    left: box.left + (box.width - width) / 2,
    top: box.top + (box.height - height) / 2,
    width,
    height,
  };
}

/** A pointer position over the picture as 0..65535 on each axis (clamped). */
export function toAbsolute(
  clientX: number,
  clientY: number,
  picture: Rect,
): { x: number; y: number } {
  const f = (v: number, start: number, size: number) =>
    Math.round(Math.min(1, Math.max(0, size > 0 ? (v - start) / size : 0)) * 65535);
  return { x: f(clientX, picture.left, picture.width), y: f(clientY, picture.top, picture.height) };
}

/**
 * Wheel events to Linux hi-res wheel units (120 per notch, positive = up
 * and right), carrying fractions over.
 */
export class WheelAccumulator {
  private rest = { x: 0, y: 0 };

  add(e: { deltaX: number; deltaY: number; deltaMode: number }): { dx: number; dy: number } {
    // Pixels: a notch is about 100 px in browsers. Lines: 3 per notch.
    const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 * 3 : 1.2;
    this.rest.x += e.deltaX * scale;
    this.rest.y -= e.deltaY * scale;
    const dx = Math.trunc(this.rest.x);
    const dy = Math.trunc(this.rest.y);
    this.rest.x -= dx;
    this.rest.y -= dy;
    return { dx, dy };
  }
}
