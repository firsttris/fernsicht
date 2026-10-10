/**
 * A browser session with a host: picture and sound from WebRTC, the
 * host's pointer drawn over the picture, mouse and keyboard to the host.
 * In gaming mode a click captures the pointer (relative movement, as games
 * want it).
 */
import { Button, type SessionMode, type SessionStats, SessionView } from "@fernsicht/ui";
import { useEffect, useRef, useState } from "react";

import {
  type CursorImage,
  CursorAssembler,
  type CursorPosition,
  decodeCursorPacket,
} from "../lib/cursor";
import { type HostStats, type LiveSession, type StatsSample, summarize } from "../lib/host";
import { useFullscreen } from "../lib/fullscreen";
import { PadTracker } from "../lib/gamepad";
import { WheelAccumulator, contentRect, linuxButton, toAbsolute } from "../lib/input";
import { linuxKeyCode } from "../lib/keys";

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
  const [ended, setEnded] = useState(false);
  const [size, setSize] = useState({ width: session.width, height: session.height });
  const [fullscreen, setFullscreen] = useFullscreen();

  // The overlay, once per second; the host's share comes on the channel.
  useEffect(() => {
    let host: HostStats | null = null;
    let previous: StatsSample | null = null;
    const onMessage = (e: MessageEvent) => {
      if (typeof e.data !== "string") return;
      try {
        const v = JSON.parse(e.data) as HostStats & { type?: string };
        if (v.type === "stats") {
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
        );
        previous = r.sample;
        setStats(r.stats);
      });
    }, 1000);
    const onState = () => {
      if (["failed", "closed", "disconnected"].includes(session.pc.connectionState)) setEnded(true);
    };
    session.pc.addEventListener("connectionstatechange", onState);
    session.channel.addEventListener("close", onState);
    return () => {
      clearInterval(timer);
      session.channel.removeEventListener("message", onMessage);
      session.pc.removeEventListener("connectionstatechange", onState);
      session.channel.removeEventListener("close", onState);
    };
  }, [session]);

  if (ended) {
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
      fullscreen={fullscreen}
      onFullscreenChange={setFullscreen}
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
      <RemoteScreen session={session} mode={mode} />
    </SessionView>
  );
}

/** The picture, the pointer, and the input that goes to the host. */
export function RemoteScreen({ session, mode }: { session: LiveSession; mode: SessionMode }) {
  const box = useRef<HTMLDivElement>(null);
  const video = useRef<HTMLVideoElement>(null);
  const pointer = useRef<HTMLCanvasElement>(null);
  const [cursor, setCursor] = useState<CursorPosition | null>(null);
  const [image, setImage] = useState<CursorImage | null>(null);
  const [needsClick, setNeedsClick] = useState(false);

  // Picture and sound. Playing with sound needs a click on the page; the
  // connect button usually was one.
  useEffect(() => {
    const v = video.current;
    if (!v) return;
    v.srcObject = session.stream;
    v.play()?.catch(() => setNeedsClick(true));
  }, [session]);

  // The host's pointer.
  useEffect(() => {
    const shapes = new CursorAssembler();
    const onMessage = (e: MessageEvent) => {
      if (!(e.data instanceof ArrayBuffer)) return;
      const p = decodeCursorPacket(e.data);
      if (p?.kind === "position") setCursor(p);
      if (p?.kind === "shape") {
        const img = shapes.add(p);
        if (img) setImage(img);
      }
    };
    session.channel.addEventListener("message", onMessage);
    return () => session.channel.removeEventListener("message", onMessage);
  }, [session]);

  useEffect(() => {
    const c = pointer.current;
    const ctx = c?.getContext("2d");
    if (!c || !ctx || !image) return;
    c.width = image.width;
    c.height = image.height;
    ctx.putImageData(new ImageData(image.rgba, image.width, image.height), 0, 0);
  }, [image]);

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
    const picture = () => {
      const r = el.getBoundingClientRect();
      const v = video.current;
      return contentRect(r, v?.videoWidth ?? 0, v?.videoHeight ?? 0);
    };
    const locked = () => document.pointerLockElement === el;
    const onMove = (e: PointerEvent) => {
      if (locked()) {
        session.send({ t: "r", dx: e.movementX, dy: e.movementY });
      } else if (mode === "desktop") {
        session.send({ t: "m", ...toAbsolute(e.clientX, e.clientY, picture()) });
      }
    };
    const onButton = (e: PointerEvent) => {
      const code = linuxButton(e.button);
      if (code === undefined) return;
      e.preventDefault();
      if (mode === "gaming" && !locked()) {
        if (e.type === "pointerdown") void el.requestPointerLock?.();
        return;
      }
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
      // Strg+Alt+F shows and hides the toolbar here.
      if (e.ctrlKey && e.altKey && e.code === "KeyF") return;
      const code = linuxKeyCode(e.code);
      if (code === undefined) return;
      e.preventDefault();
      if (!e.repeat) session.send({ t: "k", c: code, p: e.type === "keydown" });
    };
    const release = () => session.send({ t: "release" });
    const noMenu = (e: Event) => e.preventDefault();
    el.addEventListener("pointermove", onMove);
    el.addEventListener("pointerdown", onButton);
    el.addEventListener("pointerup", onButton);
    el.addEventListener("wheel", onWheel, { passive: false });
    el.addEventListener("contextmenu", noMenu);
    window.addEventListener("keydown", onKey);
    window.addEventListener("keyup", onKey);
    window.addEventListener("blur", release);
    return () => {
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

  // The pointer image where the host has it, scaled like the picture.
  const placed = (() => {
    const el = box.current;
    const v = video.current;
    if (!cursor?.visible || !image || cursor.serial !== image.serial || !el || !v) return null;
    if (!cursor.screenWidth || !cursor.screenHeight) return null;
    const r = el.getBoundingClientRect();
    const pic = contentRect(
      { left: 0, top: 0, width: r.width, height: r.height },
      v.videoWidth,
      v.videoHeight,
    );
    const sx = pic.width / cursor.screenWidth;
    const sy = pic.height / cursor.screenHeight;
    return {
      left: pic.left + cursor.x * sx,
      top: pic.top + cursor.y * sy,
      width: image.width * sx,
      height: image.height * sy,
    };
  })();

  return (
    <div
      ref={box}
      data-testid="remote-screen"
      className="absolute inset-0 touch-none select-none"
      style={{ cursor: mode === "desktop" ? "none" : "default" }}
    >
      <video
        ref={video}
        autoPlay
        playsInline
        className="h-full w-full object-contain"
        aria-label={`Bildschirm von ${session.hostName}`}
      />
      <canvas
        ref={pointer}
        aria-hidden
        className="pointer-events-none absolute"
        style={
          placed
            ? { left: placed.left, top: placed.top, width: placed.width, height: placed.height }
            : { display: "none" }
        }
      />
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
