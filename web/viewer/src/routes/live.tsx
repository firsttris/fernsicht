/**
 * A browser session with a host: picture and sound from WebRTC, the
 * host's pointer drawn over the picture, mouse and keyboard to the host.
 * In gaming mode a click captures the pointer (relative movement, as games
 * want it).
 */
import {
  Button,
  type HostMonitors,
  type SessionMode,
  type SessionStats,
  SessionView,
} from "@fernsicht/ui";
import { type RefObject, useEffect, useRef, useState } from "react";

import {
  type CursorImage,
  CursorAssembler,
  type CursorPosition,
  decodeCursorPacket,
  placement,
} from "../lib/cursor";
import {
  type ConnectionPhase,
  type FrameTiming,
  type HostStats,
  type LiveSession,
  RECONNECT_GRACE_MS,
  type StatsSample,
  connectionPhase,
  frameTiming,
  holdSession,
  meanTiming,
  summarize,
} from "../lib/host";
import { useFullscreen } from "../lib/fullscreen";
import { PadTracker } from "../lib/gamepad";
import { WheelAccumulator, contentRect, linuxButton, toAbsolute } from "../lib/input";
import { BACKSPACE, ENTER, diffText, guessLayout, tapKey, typeText } from "../lib/textkeys";
import { NO_ZOOM, TouchController, type TouchMode, type Zoom } from "../lib/touch";
import { keysGoToHost, linuxKeyCode } from "../lib/keys";

export function LiveSessionPage({
  session,
  mode,
  onModeChange,
  onEnd,
}: {
  session: LiveSession;
  mode: SessionMode;
  onModeChange: (m: SessionMode) => void;
  onEnd: () => void;
}) {
  const [stats, setStats] = useState<SessionStats>();
  const phase = useConnectionPhase(session);
  const [size, setSize] = useState({ width: session.width, height: session.height });
  const [fullscreen, setFullscreen] = useFullscreen();
  const [monitors, setMonitors] = useState<HostMonitors>();
  const [touchMode, setTouchMode] = useState<TouchMode>("direct");
  const [muted, setMuted] = useState(false);
  // Decode and display times of the frames shown, gathered by the screen.
  const frameTimes = useRef<FrameTiming[]>([]);
  const [zoom, setZoom] = useState<Zoom>(NO_ZOOM);
  const keyboard = useRef<HTMLInputElement>(null);
  const touch = useTouchScreen();

  // The overlay, once per second; the host's share comes on the channel.
  useEffect(() => {
    let host: HostStats | null = null;
    let previous: StatsSample | null = null;
    const onMessage = (e: MessageEvent) => {
      if (typeof e.data !== "string") return;
      try {
        const v = JSON.parse(e.data) as HostStats & { type?: string };
        if (v.type === "monitors") {
          setMonitors(v as unknown as HostMonitors);
        } else if (v.type === "stats") {
          host = v;
          if (v.width && v.height) setSize({ width: v.width, height: v.height });
        }
      } catch {
        // Not for us.
      }
    };
    session.channel.addEventListener("message", onMessage);
    const timer = setInterval(() => {
      void session.pc.getStats().then((report) => {
        const r = summarize(
          report as unknown as Map<string, Record<string, unknown>>,
          previous,
          host,
          meanTiming(frameTimes.current.splice(0)),
        );
        previous = r.sample;
        setStats(r.stats);
      });
    }, 1000);
    return () => {
      clearInterval(timer);
      session.channel.removeEventListener("message", onMessage);
    };
  }, [session]);

  if (phase === "ended") {
    return (
      <div className="flex h-dvh flex-col items-center justify-center gap-4 bg-background p-6 text-center">
        <h1 className="m-0 text-xl font-semibold tracking-tight">
          Verbindung mit {session.hostName} beendet
        </h1>
        <Button onClick={onEnd}>Neu verbinden</Button>
      </div>
    );
  }

  return (
    <SessionView
      session={{
        deviceName: session.hostName,
        width: size.width,
        height: size.height,
        path: "P2P",
        encrypted: true,
      }}
      stats={stats}
      mode={mode}
      onModeChange={onModeChange}
      onMuteChange={setMuted}
      fullscreen={fullscreen}
      onFullscreenChange={setFullscreen}
      monitors={monitors}
      onSelectMonitor={(i) => session.send({ t: "monitor", i })}
      onShowKeyboard={() => keyboard.current?.focus()}
      touchMode={touch ? touchMode : undefined}
      onTouchModeChange={setTouchMode}
      zoomed={zoom.scale > 1}
      onResetZoom={() => setZoom(NO_ZOOM)}
      onSendKeys={(codes) => {
        // Pressed in order, released in reverse, like a person would.
        for (const c of codes) session.send({ t: "k", c, p: true });
        for (const c of [...codes].reverse()) session.send({ t: "k", c, p: false });
      }}
      onDisconnect={() => {
        session.close();
        onEnd();
      }}
    >
      <RemoteScreen
        session={session}
        mode={mode}
        muted={muted}
        frameTimes={frameTimes}
        touchMode={touchMode}
        zoom={zoom}
        onZoom={setZoom}
      />
      <TextInput session={session} input={keyboard} />
      {phase === "interrupted" && (
        <div
          role="status"
          className="pointer-events-none absolute inset-x-0 top-1/3 mx-auto w-fit rounded-md bg-background/90 px-4 py-2 text-sm shadow-lg"
        >
          Verbindung unterbrochen …
        </div>
      )}
    </SessionView>
  );
}

/**
 * The connection's phase. An interruption that lasts longer than the
 * grace time ends the session; an ended session stays ended and is closed
 * (also when the page goes away, e.g. browser back).
 */
function useConnectionPhase(session: LiveSession): ConnectionPhase {
  const [phase, setPhase] = useState<ConnectionPhase>("live");
  useEffect(() => holdSession(session), [session]);
  useEffect(() => {
    let grace: ReturnType<typeof setTimeout> | undefined;
    const update = () => {
      const next = connectionPhase(session.pc.connectionState, session.channel.readyState);
      clearTimeout(grace);
      if (next === "interrupted") {
        grace = setTimeout(() => setPhase("ended"), RECONNECT_GRACE_MS);
      }
      setPhase((p) => (p === "ended" ? p : next));
    };
    session.pc.addEventListener("connectionstatechange", update);
    session.channel.addEventListener("close", update);
    return () => {
      clearTimeout(grace);
      session.pc.removeEventListener("connectionstatechange", update);
      session.channel.removeEventListener("close", update);
    };
  }, [session]);
  useEffect(() => {
    if (phase === "ended") session.close();
  }, [phase, session]);
  return phase;
}

/** The picture, the pointer, and the input that goes to the host. */
export function RemoteScreen({
  session,
  mode,
  muted = false,
  frameTimes,
  touchMode = "direct",
  zoom = NO_ZOOM,
  onZoom = () => {},
}: {
  session: LiveSession;
  mode: SessionMode;
  /** The host's sound off (the toolbar's sound button). */
  muted?: boolean;
  /** Where the times of the frames shown go (for the overlay). */
  frameTimes?: RefObject<FrameTiming[]>;
  touchMode?: TouchMode;
  zoom?: Zoom;
  onZoom?: (z: Zoom) => void;
}) {
  const box = useRef<HTMLDivElement>(null);
  const zoomed = useRef<HTMLDivElement>(null);
  // The latest values for the input handlers, without re-registering them.
  const live = useRef({ touchMode, zoom, onZoom });
  live.current = { touchMode, zoom, onZoom };
  const video = useRef<HTMLVideoElement>(null);
  const pointer = useRef<HTMLCanvasElement>(null);
  const [needsClick, setNeedsClick] = useState(false);

  // Picture and sound. Playing with sound needs a click on the page; the
  // connect button usually was one.
  useEffect(() => {
    const v = video.current;
    if (!v) return;
    v.srcObject = session.stream;
    v.play()?.catch(() => setNeedsClick(true));
  }, [session]);

  // Each frame shown: how long it took to decode and to reach the screen.
  useEffect(() => {
    const v = video.current;
    if (!v?.requestVideoFrameCallback || !frameTimes) return;
    let handle = 0;
    const onFrame: VideoFrameRequestCallback = (_, meta) => {
      const t = frameTiming(meta);
      // At most a few seconds' worth if nobody reads them.
      if (t && frameTimes.current.length < 1000) frameTimes.current.push(t);
      handle = v.requestVideoFrameCallback(onFrame);
    };
    handle = v.requestVideoFrameCallback(onFrame);
    return () => v.cancelVideoFrameCallback(handle);
  }, [frameTimes]);

  // The host's pointer, drawn and placed outside React: its packets come
  // with every frame, and a render per packet would redo the whole screen.
  useEffect(() => {
    const c = pointer.current;
    const el = box.current;
    const v = video.current;
    if (!c || !el || !v) return;
    const shapes = new CursorAssembler();
    let image: CursorImage | null = null;
    let at: CursorPosition | null = null;
    // The box's size is read when it changes, not per packet.
    let size = { width: el.clientWidth, height: el.clientHeight };
    let shown = "";
    const place = () => {
      const p = placement(at, image, size, { width: v.videoWidth, height: v.videoHeight });
      const key = p ? `${p.left} ${p.top} ${p.width} ${p.height}` : "";
      if (key === shown) return;
      shown = key;
      if (!p) {
        c.style.display = "none";
        return;
      }
      Object.assign(c.style, {
        display: "",
        left: `${p.left}px`,
        top: `${p.top}px`,
        width: `${p.width}px`,
        height: `${p.height}px`,
      });
    };
    const draw = (img: CursorImage) => {
      image = img;
      c.width = img.width;
      c.height = img.height;
      c.getContext("2d")?.putImageData(new ImageData(img.rgba, img.width, img.height), 0, 0);
    };
    const onMessage = (e: MessageEvent) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      const p = decodeCursorPacket(e.data);
      if (p?.kind === "position") {
        at = p;
        place();
      } else if (p?.kind === "shape" && p.serial !== image?.serial) {
        // The host repeats shapes so a lost piece heals; a known one is
        // drawn already.
        const img = shapes.add(p);
        if (img) {
          draw(img);
          place();
        }
      }
    };
    const resized = () => {
      size = { width: el.clientWidth, height: el.clientHeight };
      place();
    };
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(resized);
    observer?.observe(el);
    window.addEventListener("resize", resized);
    // The picture's size changes with the monitor shown.
    v.addEventListener("resize", place);
    session.channel.addEventListener("message", onMessage);
    return () => {
      session.channel.removeEventListener("message", onMessage);
      v.removeEventListener("resize", place);
      window.removeEventListener("resize", resized);
      observer?.disconnect();
    };
  }, [session]);

  // Gamepads: polled once per frame (the API has no events for changes).
  // Some browsers keep them from plain-http pages; then there are none.
  useEffect(() => {
    const tracker = new PadTracker();
    let frame = 0;
    const poll = () => {
      let pads: (Gamepad | null)[] = [];
      try {
        pads = navigator.getGamepads?.() ?? [];
      } catch {
        // Not allowed here.
      }
      for (const msg of tracker.update(pads)) session.send(msg);
      frame = requestAnimationFrame(poll);
    };
    frame = requestAnimationFrame(poll);
    return () => cancelAnimationFrame(frame);
  }, [session]);

  // Mouse and keyboard.
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const wheel = new WheelAccumulator();
    // The picture as it is shown now, zoom included.
    const picture = () => {
      const r = (zoomed.current ?? el).getBoundingClientRect();
      const v = video.current;
      return contentRect(r, v?.videoWidth ?? 0, v?.videoHeight ?? 0);
    };
    const touch = new TouchController(
      // Games want relative motion: the finger is a touchpad there.
      () => (mode === "gaming" ? "trackpad" : live.current.touchMode),
      (x, y) => {
        const r = el.getBoundingClientRect();
        return toAbsolute(x + r.left, y + r.top, picture());
      },
      (a) => (a.kind === "send" ? session.send(a.msg) : live.current.onZoom(a.zoom)),
      () => el.getBoundingClientRect(),
    );
    touch.zoom = live.current.zoom;
    const isTouch = (e: PointerEvent) => e.pointerType === "touch" || e.pointerType === "pen";
    const local = (e: PointerEvent): [number, number] => {
      const r = el.getBoundingClientRect();
      return [e.clientX - r.left, e.clientY - r.top];
    };
    const ticker = setInterval(() => touch.tick(performance.now()), 100);
    const locked = () => document.pointerLockElement === el;
    const onMove = (e: PointerEvent) => {
      if (isTouch(e)) {
        touch.zoom = live.current.zoom;
        touch.move(e.pointerId, ...local(e), performance.now());
        return;
      }
      if (locked()) {
        session.move({ t: "r", dx: e.movementX, dy: e.movementY });
      } else if (mode === "desktop") {
        session.move({ t: "m", ...toAbsolute(e.clientX, e.clientY, picture()) });
      }
    };
    const onButton = (e: PointerEvent) => {
      if (isTouch(e)) {
        e.preventDefault();
        touch.zoom = live.current.zoom;
        if (e.type === "pointerdown") touch.down(e.pointerId, ...local(e), performance.now());
        else touch.up(e.pointerId, ...local(e), performance.now());
        return;
      }
      const code = linuxButton(e.button);
      if (code === undefined) return;
      e.preventDefault();
      const down = e.type === "pointerdown";
      if (mode === "gaming" && !locked()) {
        // The click that captures the pointer stays here; a release still
        // goes on (the button may be down on the host since before Esc let
        // the pointer go; releasing a button that is up does nothing).
        if (down) {
          void el.requestPointerLock?.();
          return;
        }
        session.send({ t: "b", c: code, p: false });
        return;
      }
      // The release comes here even when the mouse is let go over the
      // toolbar (dragging a window up to the top of the screen).
      if (down) el.setPointerCapture?.(e.pointerId);
      if (mode === "desktop")
        session.send({ t: "m", ...toAbsolute(e.clientX, e.clientY, picture()) });
      session.send({ t: "b", c: code, p: e.type === "pointerdown" });
    };
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const { dx, dy } = wheel.add(e);
      if (dx || dy) session.send({ t: "w", dx, dy });
    };
    const onKey = (e: KeyboardEvent) => {
      // The session's own controls keep their keys.
      if (!keysGoToHost(e.target)) return;
      // Strg+Alt+F shows and hides the toolbar, Strg+Alt+T goes into it.
      if (e.ctrlKey && e.altKey && (e.code === "KeyF" || e.code === "KeyT")) return;
      const code = linuxKeyCode(e.code);
      if (code === undefined) return;
      e.preventDefault();
      if (!e.repeat) session.send({ t: "k", c: code, p: e.type === "keydown" });
    };
    const release = () => session.send({ t: "release" });
    const noMenu = (e: Event) => e.preventDefault();
    const onCancel = (e: PointerEvent) => touch.cancel(e.pointerId);
    el.addEventListener("pointermove", onMove);
    el.addEventListener("pointerdown", onButton);
    el.addEventListener("pointerup", onButton);
    el.addEventListener("pointercancel", onCancel);
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("contextmenu", noMenu);
    window.addEventListener("keydown", onKey);
    window.addEventListener("keyup", onKey);
    window.addEventListener("blur", release);
    return () => {
      clearInterval(ticker);
      el.removeEventListener("pointercancel", onCancel);
      el.removeEventListener("pointermove", onMove);
      el.removeEventListener("pointerdown", onButton);
      el.removeEventListener("pointerup", onButton);
      el.removeEventListener("wheel", onWheel);
      el.removeEventListener("contextmenu", noMenu);
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("keyup", onKey);
      window.removeEventListener("blur", release);
      release();
    };
  }, [session, mode]);

  return (
    <div
      ref={box}
      data-testid="remote-screen"
      className="absolute inset-0 touch-none select-none"
      style={{ cursor: mode === "desktop" ? "none" : "default" }}
    >
      <div
        ref={zoomed}
        className="absolute inset-0 origin-top-left"
        style={
          zoom.scale > 1
            ? { transform: `translate(${zoom.x}px, ${zoom.y}px) scale(${zoom.scale})` }
            : undefined
        }
      >
        <video
          ref={video}
          autoPlay
          playsInline
          muted={muted}
          className="h-full w-full object-contain"
          aria-label={`Bildschirm von ${session.hostName}`}
        />
        {/* Placed by the pointer effect above, not by React. */}
        <canvas
          ref={pointer}
          aria-hidden
          className="pointer-events-none absolute"
          style={{ display: "none" }}
        />
      </div>
      {needsClick && (
        <button
          type="button"
          className="absolute inset-0 flex items-center justify-center bg-black/60 text-lg text-foreground"
          onClick={() => {
            void video.current?.play();
            setNeedsClick(false);
          }}
        >
          Zum Starten klicken
        </button>
      )}
    </div>
  );
}

/** Whether this device is used with a finger (phones, tablets). */
function useTouchScreen(): boolean {
  const query = "(pointer: coarse)";
  const [coarse, setCoarse] = useState(() => window.matchMedia?.(query).matches ?? false);
  useEffect(() => {
    const m = window.matchMedia?.(query);
    if (!m) return;
    const on = () => setCoarse(m.matches);
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);
  return coarse;
}

/**
 * The on-screen keyboard's target: an almost invisible text field. What
 * the keyboard writes into it goes to the host as key presses (phone
 * keyboards report text, not keys); hardware keys still go the usual way.
 */
export function TextInput({
  session,
  input,
}: {
  session: LiveSession;
  input: RefObject<HTMLInputElement | null>;
}) {
  // Two characters the user cannot see: Backspace has something to delete.
  const SENTINEL = "\u200b\u200b";
  const layout = guessLayout(navigator.languages ?? [navigator.language]);
  const before = useRef(SENTINEL);
  const reset = (el: HTMLInputElement) => {
    el.value = SENTINEL;
    before.current = SENTINEL;
  };
  return (
    <input
      ref={input}
      data-keys-to-host
      aria-label="Text an den Host"
      autoCapitalize="off"
      autoComplete="off"
      autoCorrect="off"
      spellCheck={false}
      defaultValue={SENTINEL}
      className="fixed bottom-0 left-0 h-px w-px opacity-0"
      onInput={(e) => {
        const el = e.currentTarget;
        const { backspaces, text } = diffText(before.current, el.value);
        for (let i = 0; i < backspaces; i++) for (const m of tapKey(BACKSPACE)) session.send(m);
        for (const m of typeText(text.replaceAll("\u200b", ""), layout)) session.send(m);
        before.current = el.value;
        // While a word is being composed, the keyboard owns the field.
        if (!(e.nativeEvent as InputEvent).isComposing) reset(el);
      }}
      onCompositionEnd={(e) => reset(e.currentTarget)}
      onKeyDown={(e) => {
        // Enter from an on-screen keyboard: no text, but a key.
        if (e.key === "Enter" && e.nativeEvent.code === "") {
          for (const m of tapKey(ENTER)) session.send(m);
        }
      }}
      onFocus={(e) => reset(e.currentTarget)}
    />
  );
}
