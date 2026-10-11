/** Mouse input in the browser, turned into the host's input messages. */

/** Browser `MouseEvent.button` → Linux `BTN_*`. */
const BUTTONS = [0x110, 0x112, 0x111, 0x113, 0x114];

/**
 * Captures the pointer for a game: raw movement, without the desktop's
 * acceleration (`unadjustedMovement`), where the browser can; else the
 * plain lock. A refusal (no click, the page not focused) is no error.
 */
export async function lockPointer(el: HTMLElement): Promise<void> {
  type Lock = (options?: { unadjustedMovement?: boolean }) => Promise<void> | undefined;
  const request = (el.requestPointerLock as Lock | undefined)?.bind(el);
  if (!request) return;
  try {
    await request({ unadjustedMovement: true });
  } catch {
    try {
      await request();
    } catch {
      // Not now; the next click asks again.
    }
  }
}

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

/** Fractions of wheel units not sent yet. */
export interface WheelRest {
  x: number;
  y: number;
}

export const NO_WHEEL_REST: WheelRest = { x: 0, y: 0 };

/**
 * A wheel event in Linux hi-res wheel units (120 per notch, positive = up
 * and right), with the fractions carried over in `rest`.
 */
export function wheelStep(
  rest: WheelRest,
  e: { deltaX: number; deltaY: number; deltaMode: number },
): { rest: WheelRest; dx: number; dy: number } {
  // Pixels: a notch is about 100 px in browsers. Lines: 3 per notch.
  const scale = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 120 * 3 : 1.2;
  const x = rest.x + e.deltaX * scale;
  const y = rest.y - e.deltaY * scale;
  const dx = Math.trunc(x);
  const dy = Math.trunc(y);
  return { rest: { x: x - dx, y: y - dy }, dx, dy };
}
