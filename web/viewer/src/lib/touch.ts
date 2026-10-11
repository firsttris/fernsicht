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
import type { FromBrowser } from "./bindings/FromBrowser";

export type TouchMode = "direct" | "trackpad";

/** What the controller wants done. */
export type TouchAction = { kind: "send"; msg: FromBrowser } | { kind: "zoom"; zoom: Zoom };

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

/** What the touch controller is told: touch pointer events, a clock tick,
 * and the zoom the page shows now. */
export interface TouchController {
  down(id: number, x: number, y: number, now: number): void;
  move(id: number, x: number, y: number, now: number): void;
  up(id: number, x: number, y: number, now: number): void;
  cancel(id: number): void;
  /** Call regularly (or on events): turns a held finger into a long press. */
  tick(now: number): void;
  syncZoom(zoom: Zoom): void;
}

/** Feeds on touch pointer events; `toHost` maps a screen point to 0..65535,
 * `box` is the box the picture zooms in (screen px), which limits panning. */
export function createTouchController(
  mode: () => TouchMode,
  toHost: (x: number, y: number) => { x: number; y: number },
  act: (a: TouchAction) => void,
  box: () => { width: number; height: number },
): TouchController {
  const s = {
    fingers: new Map<number, Finger>(),
    started: 0,
    moved: false,
    longPressed: false,
    dragging: false,
    two: null as { dist: number; mid: Point; zooming: boolean; startZoom: Zoom } | null,
    hadTwo: false,
    scrollRest: { x: 0, y: 0 },
    moveRest: { x: 0, y: 0 },
    zoom: NO_ZOOM as Zoom,
  };

  function send(msg: FromBrowser) {
    act({ kind: "send", msg });
  }

  function click(button: number) {
    send({ t: "b", c: button, p: true });
    send({ t: "b", c: button, p: false });
  }

  function moveTo(x: number, y: number) {
    send({ t: "m", ...toHost(x, y) });
  }

  function down(id: number, x: number, y: number, now: number) {
    s.fingers.set(id, { id, x, y, startX: x, startY: y });
    if (s.fingers.size === 1) {
      s.started = now;
      s.moved = false;
      s.longPressed = false;
      s.dragging = false;
      s.hadTwo = false;
      s.moveRest = { x: 0, y: 0 };
    } else if (s.fingers.size === 2) {
      // A drag the first finger started ends; two fingers are a gesture.
      if (s.dragging) {
        send({ t: "b", c: LEFT, p: false });
        s.dragging = false;
      }
      const [a, b] = pair();
      s.two = { dist: dist(a, b), mid: mid(a, b), zooming: false, startZoom: s.zoom };
      s.hadTwo = true;
      s.scrollRest = { x: 0, y: 0 };
    }
  }

  function move(id: number, x: number, y: number, now: number) {
    const f = s.fingers.get(id);
    if (!f) return;
    const dx = x - f.x;
    const dy = y - f.y;
    f.x = x;
    f.y = y;
    if (Math.hypot(x - f.startX, y - f.startY) > TAP_SLOP) s.moved = true;

    if (s.fingers.size >= 2 && s.two) {
      twoFingers();
      return;
    }
    if (s.fingers.size !== 1 || s.hadTwo) return;
    tick(now);
    if (mode() === "trackpad") {
      const rx = s.moveRest.x + dx * TRACKPAD_SPEED;
      const ry = s.moveRest.y + dy * TRACKPAD_SPEED;
      const ix = Math.trunc(rx);
      const iy = Math.trunc(ry);
      s.moveRest = { x: rx - ix, y: ry - iy };
      if (s.longPressed && !s.dragging) {
        send({ t: "b", c: LEFT, p: true });
        s.dragging = true;
      }
      if (ix || iy) send({ t: "r", dx: ix, dy: iy });
    } else if (s.moved && !s.longPressed) {
      if (!s.dragging) {
        moveTo(f.startX, f.startY);
        send({ t: "b", c: LEFT, p: true });
        s.dragging = true;
      }
      moveTo(x, y);
    }
  }

  function up(id: number, x: number, y: number, now: number) {
    const f = s.fingers.get(id);
    if (!f) return;
    f.x = x;
    f.y = y;
    const count = s.fingers.size;
    s.fingers.delete(id);
    if (count === 2) {
      // Both fingers briefly, hardly moved: a right click.
      if (s.two && !s.moved && !s.two.zooming && now - s.started < TAP_MS) {
        click(RIGHT);
      }
      s.two = null;
      return;
    }
    if (count !== 1 || s.hadTwo) return;
    tick(now);
    if (s.dragging) {
      send({ t: "b", c: LEFT, p: false });
      s.dragging = false;
      return;
    }
    if (s.longPressed || s.moved) return;
    if (mode() === "direct") moveTo(x, y);
    click(LEFT);
  }

  function cancel(id: number) {
    s.fingers.delete(id);
    if (s.dragging) {
      send({ t: "b", c: LEFT, p: false });
      s.dragging = false;
    }
    if (s.fingers.size < 2) s.two = null;
  }

  function tick(now: number) {
    if (s.fingers.size !== 1 || s.hadTwo || s.moved || s.longPressed) return;
    if (now - s.started < LONG_PRESS_MS) return;
    s.longPressed = true;
    // Direct mode: right click where the finger is. Trackpad mode: wait;
    // moving now drags.
    const f = s.fingers.values().next().value;
    if (mode() === "direct" && f) {
      moveTo(f.x, f.y);
      click(RIGHT);
    }
  }

  /** The first two fingers (call with two or more down). */
  function pair(): [Finger, Finger] {
    const [a, b] = [...s.fingers.values()];
    if (!a || !b) throw new Error("two fingers expected");
    return [a, b];
  }

  function twoFingers() {
    const t = s.two;
    if (!t) return;
    const [a, b] = pair();
    const d = dist(a, b);
    const m = mid(a, b);
    if (!t.zooming && Math.abs(d / t.dist - 1) > PINCH_START) t.zooming = true;
    if (t.zooming) {
      const scale = clamp(t.startZoom.scale * (d / t.dist), 1, MAX_ZOOM);
      // Keep the point between the fingers where it was.
      const k = scale / t.startZoom.scale;
      const x = m.x - (t.mid.x - t.startZoom.x) * k;
      const y = m.y - (t.mid.y - t.startZoom.y) * k;
      setZoom({ scale, x, y });
      return;
    }
    const dx = m.x - t.mid.x;
    const dy = m.y - t.mid.y;
    t.mid = m;
    if (s.zoom.scale > 1) {
      setZoom({ ...s.zoom, x: s.zoom.x + dx, y: s.zoom.y + dy });
      return;
    }
    // Content follows the fingers: up is scrolling down.
    const rx = s.scrollRest.x - dx * SCROLL_PER_PX;
    const ry = s.scrollRest.y + dy * SCROLL_PER_PX;
    const ix = Math.trunc(rx);
    const iy = Math.trunc(ry);
    s.scrollRest = { x: rx - ix, y: ry - iy };
    if (ix || iy) send({ t: "w", dx: ix, dy: iy });
  }

  function setZoom(z: Zoom) {
    const { width, height } = box();
    const scale = clamp(z.scale, 1, MAX_ZOOM);
    // The picture always covers the box: no empty space when panning.
    const x = clamp(z.x, width - width * scale, 0);
    const y = clamp(z.y, height - height * scale, 0);
    s.zoom = scale === 1 ? NO_ZOOM : { scale, x, y };
    act({ kind: "zoom", zoom: s.zoom });
  }

  return {
    down,
    move,
    up,
    cancel,
    tick,
    syncZoom: (zoom) => {
      s.zoom = zoom;
    },
  };
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
