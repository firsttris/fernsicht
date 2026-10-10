/**
 * Touch gestures on the remote screen, turned into the host's mouse input
 * and a local zoom of the picture.
 *
 * One finger
 * - direct mode: a tap clicks where the finger is; moving drags with the
 *   left button; a long press is a right click.
 * - trackpad mode: moving moves the host's pointer; a tap clicks where it
 *   is; a long press, then moving, drags.
 *
 * Two fingers
 * - moving together scrolls (or pans the picture while zoomed in);
 * - spreading or pinching zooms the picture here (the host sees nothing);
 * - a short tap with both is a right click.
 */

export type TouchMode = "direct" | "trackpad";

/** What the controller wants done. */
export type TouchAction =
  { kind: "send"; msg: Record<string, unknown> } | { kind: "zoom"; zoom: Zoom };

/** The picture's local zoom: scale around the box, then shift (px). */
export interface Zoom {
  scale: number;
  x: number;
  y: number;
}

export const NO_ZOOM: Zoom = { scale: 1, x: 0, y: 0 };

/** Finger travel below this is still a tap (px). */
export const TAP_SLOP = 10;
/** Pressed this long without moving: a long press (ms). */
export const LONG_PRESS_MS = 500;
/** Released within this: a tap (ms). */
export const TAP_MS = 300;
/** Pinch: distance change that starts zooming instead of scrolling. */
const PINCH_START = 0.12;
/** Trackpad: pointer pixels per finger pixel. */
const TRACKPAD_SPEED = 1.6;
/** Two-finger scrolling: wheel units (120 = a notch) per finger pixel. */
const SCROLL_PER_PX = 4;
const MAX_ZOOM = 4;

const LEFT = 0x110;
const RIGHT = 0x111;

interface Finger {
  id: number;
  x: number;
  y: number;
  startX: number;
  startY: number;
}

interface Point {
  x: number;
  y: number;
}

/** Feeds on touch pointer events; `toHost` maps a screen point to 0..65535. */
export class TouchController {
  private fingers = new Map<number, Finger>();
  private started = 0;
  private moved = false;
  private longPressed = false;
  private dragging = false;
  private two: { dist: number; mid: Point; zooming: boolean; startZoom: Zoom } | null = null;
  private hadTwo = false;
  private scrollRest = { x: 0, y: 0 };
  private moveRest = { x: 0, y: 0 };
  zoom: Zoom = NO_ZOOM;

  constructor(
    private mode: () => TouchMode,
    private toHost: (x: number, y: number) => { x: number; y: number },
    private act: (a: TouchAction) => void,
    /** The box the picture zooms in (screen px): limits panning. */
    private box: () => { width: number; height: number },
  ) {}

  private send(msg: Record<string, unknown>) {
    this.act({ kind: "send", msg });
  }

  private click(button: number) {
    this.send({ t: "b", c: button, p: true });
    this.send({ t: "b", c: button, p: false });
  }

  private moveTo(x: number, y: number) {
    this.send({ t: "m", ...this.toHost(x, y) });
  }

  down(id: number, x: number, y: number, now: number) {
    this.fingers.set(id, { id, x, y, startX: x, startY: y });
    if (this.fingers.size === 1) {
      this.started = now;
      this.moved = false;
      this.longPressed = false;
      this.dragging = false;
      this.hadTwo = false;
      this.moveRest = { x: 0, y: 0 };
    } else if (this.fingers.size === 2) {
      // A drag the first finger started ends; two fingers are a gesture.
      if (this.dragging) {
        this.send({ t: "b", c: LEFT, p: false });
        this.dragging = false;
      }
      const [a, b] = this.pair();
      this.two = { dist: dist(a, b), mid: mid(a, b), zooming: false, startZoom: this.zoom };
      this.hadTwo = true;
      this.scrollRest = { x: 0, y: 0 };
    }
  }

  move(id: number, x: number, y: number, now: number) {
    const f = this.fingers.get(id);
    if (!f) return;
    const dx = x - f.x;
    const dy = y - f.y;
    f.x = x;
    f.y = y;
    if (Math.hypot(x - f.startX, y - f.startY) > TAP_SLOP) this.moved = true;

    if (this.fingers.size >= 2 && this.two) {
      this.twoFingers();
      return;
    }
    if (this.fingers.size !== 1 || this.hadTwo) return;
    this.tick(now);
    if (this.mode() === "trackpad") {
      const rx = this.moveRest.x + dx * TRACKPAD_SPEED;
      const ry = this.moveRest.y + dy * TRACKPAD_SPEED;
      const ix = Math.trunc(rx);
      const iy = Math.trunc(ry);
      this.moveRest = { x: rx - ix, y: ry - iy };
      if (this.longPressed && !this.dragging) {
        this.send({ t: "b", c: LEFT, p: true });
        this.dragging = true;
      }
      if (ix || iy) this.send({ t: "r", dx: ix, dy: iy });
    } else if (this.moved && !this.longPressed) {
      if (!this.dragging) {
        this.moveTo(f.startX, f.startY);
        this.send({ t: "b", c: LEFT, p: true });
        this.dragging = true;
      }
      this.moveTo(x, y);
    }
  }

  up(id: number, x: number, y: number, now: number) {
    const f = this.fingers.get(id);
    if (!f) return;
    f.x = x;
    f.y = y;
    const count = this.fingers.size;
    this.fingers.delete(id);
    if (count === 2) {
      // Both fingers briefly, hardly moved: a right click.
      if (this.two && !this.moved && !this.two.zooming && now - this.started < TAP_MS) {
        this.click(RIGHT);
      }
      this.two = null;
      return;
    }
    if (count !== 1 || this.hadTwo) return;
    this.tick(now);
    if (this.dragging) {
      this.send({ t: "b", c: LEFT, p: false });
      this.dragging = false;
      return;
    }
    if (this.longPressed || this.moved) return;
    if (this.mode() === "direct") this.moveTo(x, y);
    this.click(LEFT);
  }

  cancel(id: number) {
    this.fingers.delete(id);
    if (this.dragging) {
      this.send({ t: "b", c: LEFT, p: false });
      this.dragging = false;
    }
    if (this.fingers.size < 2) this.two = null;
  }

  /** Call regularly (or on events): turns a held finger into a long press. */
  tick(now: number) {
    if (this.fingers.size !== 1 || this.hadTwo || this.moved || this.longPressed) return;
    if (now - this.started < LONG_PRESS_MS) return;
    this.longPressed = true;
    // Direct mode: right click where the finger is. Trackpad mode: wait;
    // moving now drags.
    const f = this.fingers.values().next().value;
    if (this.mode() === "direct" && f) {
      this.moveTo(f.x, f.y);
      this.click(RIGHT);
    }
  }

  /** The first two fingers (call with two or more down). */
  private pair(): [Finger, Finger] {
    const [a, b] = [...this.fingers.values()];
    if (!a || !b) throw new Error("two fingers expected");
    return [a, b];
  }

  private twoFingers() {
    const t = this.two;
    if (!t) return;
    const [a, b] = this.pair();
    const d = dist(a, b);
    const m = mid(a, b);
    if (!t.zooming && Math.abs(d / t.dist - 1) > PINCH_START) t.zooming = true;
    if (t.zooming) {
      const scale = clamp(t.startZoom.scale * (d / t.dist), 1, MAX_ZOOM);
      // Keep the point between the fingers where it was.
      const k = scale / t.startZoom.scale;
      const x = m.x - (t.mid.x - t.startZoom.x) * k;
      const y = m.y - (t.mid.y - t.startZoom.y) * k;
      this.setZoom({ scale, x, y });
      return;
    }
    const dx = m.x - t.mid.x;
    const dy = m.y - t.mid.y;
    t.mid = m;
    if (this.zoom.scale > 1) {
      this.setZoom({ ...this.zoom, x: this.zoom.x + dx, y: this.zoom.y + dy });
      return;
    }
    // Content follows the fingers: up is scrolling down.
    const rx = this.scrollRest.x - dx * SCROLL_PER_PX;
    const ry = this.scrollRest.y + dy * SCROLL_PER_PX;
    const ix = Math.trunc(rx);
    const iy = Math.trunc(ry);
    this.scrollRest = { x: rx - ix, y: ry - iy };
    if (ix || iy) this.send({ t: "w", dx: ix, dy: iy });
  }

  private setZoom(z: Zoom) {
    const { width, height } = this.box();
    const scale = clamp(z.scale, 1, MAX_ZOOM);
    // The picture always covers the box: no empty space when panning.
    const x = clamp(z.x, width - width * scale, 0);
    const y = clamp(z.y, height - height * scale, 0);
    this.zoom = scale === 1 ? NO_ZOOM : { scale, x, y };
    this.act({ kind: "zoom", zoom: this.zoom });
  }

  resetZoom() {
    this.setZoom(NO_ZOOM);
  }
}

function dist(a: Point, b: Point) {
  return Math.max(1, Math.hypot(a.x - b.x, a.y - b.y));
}

function mid(a: Point, b: Point): Point {
  return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 };
}

function clamp(v: number, lo: number, hi: number) {
  return Math.min(hi, Math.max(lo, v));
}
