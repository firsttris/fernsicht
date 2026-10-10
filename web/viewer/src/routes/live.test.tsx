/**
 * The viewer served by a host: PIN, a live WebRTC session (with fakes for
 * the browser's WebRTC objects), input to the host, the host's pointer.
 */
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { currentSession } from "../lib/host";
import { FakePeerConnection, installFakes } from "../test-rtc";
import { renderViewer } from "../test-utils";

let pinOk = true;

beforeEach(() => {
  pinOk = true;
  installFakes((path) => {
    if (path === "/api/info") return { status: 200, body: { name: "zentrale", pairing: true } };
    return pinOk
      ? { status: 200, body: { answer: { type: "answer", sdp: "v=0" }, width: 1920, height: 1080 } }
      : { status: 403, body: { error: "wrong-pin" } };
  });
});

afterEach(() => {
  currentSession()?.close();
  vi.unstubAllGlobals();
});

async function connected(mode: "desktop" | "gaming" = "desktop") {
  const view = await renderViewer("/");
  await screen.findByRole("heading", { name: "Mit zentrale verbinden" });
  if (mode === "gaming") await view.user.click(screen.getByRole("radio", { name: /Gaming/ }));
  await view.user.type(screen.getByLabelText("PIN"), "482913");
  await view.user.click(screen.getByRole("button", { name: "Verbinden" }));
  await waitFor(() => expect(view.router.state.location.pathname).toBe("/session/host"));
  const pc = FakePeerConnection.last!;
  return { ...view, pc, screenEl: await screen.findByTestId("remote-screen") };
}

function cursorPosition(x: number, y: number): ArrayBuffer {
  const b = new ArrayBuffer(24);
  const v = new DataView(b);
  v.setUint8(0, 0xf5);
  v.setUint8(1, 1);
  v.setUint8(2, 8);
  v.setUint8(3, 1);
  v.setUint32(8, 1, true);
  v.setInt32(12, x, true);
  v.setInt32(16, y, true);
  v.setUint16(20, 1920, true);
  v.setUint16(22, 1080, true);
  return b;
}

function cursorShape(): ArrayBuffer {
  const b = new ArrayBuffer(20 + 16 * 16 * 4);
  const v = new DataView(b);
  v.setUint8(0, 0xf5);
  v.setUint8(1, 1);
  v.setUint8(2, 9);
  v.setUint32(8, 1, true);
  v.setUint16(12, 16, true);
  v.setUint16(14, 16, true);
  return b;
}

describe("Web-Viewer am Host", () => {
  it("asks for the host's PIN", async () => {
    const { user } = await renderViewer("/");
    expect(await screen.findByRole("heading", { name: "Mit zentrale verbinden" })).toBeVisible();
    expect(screen.queryByLabelText("Geräte-ID")).not.toBeInTheDocument();
    const submit = screen.getByRole("button", { name: "Verbinden" });
    await user.type(screen.getByLabelText("PIN"), "48a29-13x9");
    expect(screen.getByLabelText("PIN")).toHaveValue("482913");
    expect(submit).toBeEnabled();
    await user.clear(screen.getByLabelText("PIN"));
    await user.type(screen.getByLabelText("PIN"), "123{Enter}");
    expect(submit).toBeDisabled();
  });

  it("says when the PIN is wrong", async () => {
    pinOk = false;
    const { user, router } = await renderViewer("/");
    await user.type(await screen.findByLabelText("PIN"), "000000");
    await user.click(screen.getByRole("button", { name: "Verbinden" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Falsche PIN.");
    expect(screen.getByLabelText("PIN")).toHaveValue("");
    expect(router.state.location.pathname).toBe("/");
  });

  it("shows the host's screen, overlay and pointer", async () => {
    const { pc } = await connected();
    expect(screen.getByLabelText("Bildschirm von zentrale")).toBeInTheDocument();
    pc.report.set("in", {
      type: "inbound-rtp",
      kind: "video",
      totalDecodeTime: 0.12,
      framesDecoded: 60,
      framesPerSecond: 60,
    });
    act(() =>
      pc.channel.receive(
        JSON.stringify({
          type: "stats",
          captureUs: 300,
          encodeUs: 4000,
          fps: 60,
          bitrateBps: 3e7,
          width: 1920,
          height: 1080,
          codec: "H.264",
        }),
      ),
    );
    act(() => pc.channel.receive("not json"));
    expect(await screen.findByRole("region", { name: "Latenz" }, { timeout: 3000 })).toBeVisible();
    expect(screen.getByText("H.264")).toBeInTheDocument();
    act(() => {
      pc.channel.receive(cursorShape());
      pc.channel.receive(cursorPosition(960, 540));
    });
  });

  it("sends mouse and keyboard to the host", async () => {
    const { pc, screenEl } = await connected();
    const sent = () => pc.channel.sent;
    fireEvent.pointerMove(screenEl, { clientX: 0, clientY: 0 });
    expect(sent().at(-1)).toEqual({ t: "m", x: 0, y: 0 });
    fireEvent.pointerDown(screenEl, { button: 0 });
    expect(sent().at(-1)).toEqual({ t: "b", c: 0x110, p: true });
    fireEvent.pointerUp(screenEl, { button: 2 });
    expect(sent().at(-1)).toEqual({ t: "b", c: 0x111, p: false });
    fireEvent.pointerDown(screenEl, { button: 9 });
    expect(sent().at(-1)).toEqual({ t: "b", c: 0x111, p: false });
    fireEvent.wheel(screenEl, { deltaY: 100, deltaMode: 0 });
    expect(sent().at(-1)).toEqual({ t: "w", dx: 0, dy: -120 });
    fireEvent.keyDown(window, { code: "KeyA" });
    expect(sent().at(-1)).toEqual({ t: "k", c: 30, p: true });
    const count = sent().length;
    fireEvent.keyDown(window, { code: "KeyA", repeat: true });
    fireEvent.keyDown(window, { code: "KeyF", ctrlKey: true, altKey: true });
    fireEvent.keyDown(window, { code: "Unidentified" });
    expect(sent()).toHaveLength(count);
    fireEvent.keyUp(window, { code: "KeyA" });
    expect(sent().at(-1)).toEqual({ t: "k", c: 30, p: false });
    expect(fireEvent.contextMenu(screenEl)).toBe(false);
    fireEvent.blur(window);
    expect(sent().at(-1)).toEqual({ t: "release" });
  });

  it("sends key combinations from the menu", async () => {
    const { pc } = await connected();
    const before = pc.channel.sent.length;
    fireEvent.click(screen.getByRole("button", { name: "Tasten senden" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Strg+Alt+Entf" }));
    expect(pc.channel.sent.slice(before)).toEqual([
      { t: "k", c: 29, p: true },
      { t: "k", c: 56, p: true },
      { t: "k", c: 111, p: true },
      { t: "k", c: 111, p: false },
      { t: "k", c: 56, p: false },
      { t: "k", c: 29, p: false },
    ]);
  });

  it("sends gamepads", async () => {
    let pressed = false;
    const buttons = () =>
      Array.from({ length: 17 }, (_, i) => ({ pressed: pressed && i === 0, value: 0 }));
    vi.stubGlobal("navigator", {
      ...navigator,
      getGamepads: () => [
        { index: 0, mapping: "standard", buttons: buttons(), axes: [0, 0, 0, 0] },
      ],
    });
    const { pc } = await connected();
    pressed = true;
    await waitFor(() =>
      expect(pc.channel.sent).toContainEqual({ t: "pb", n: 0, c: 0x130, p: true }),
    );
  });

  it("captures the pointer in gaming mode", async () => {
    const { pc, screenEl } = await connected("gaming");
    const lock = vi.fn();
    Object.assign(screenEl, { requestPointerLock: lock });
    fireEvent.pointerMove(screenEl, { clientX: 5, clientY: 5 });
    expect(pc.channel.sent).toEqual([]);
    fireEvent.pointerDown(screenEl, { button: 0 });
    expect(lock).toHaveBeenCalled();
    expect(pc.channel.sent).toEqual([]);
    Object.defineProperty(document, "pointerLockElement", {
      configurable: true,
      get: () => screenEl,
    });
    fireEvent.pointerMove(screenEl, { movementX: 3, movementY: -2 });
    expect(pc.channel.sent.at(-1)).toMatchObject({ t: "r" });
    fireEvent.pointerDown(screenEl, { button: 0 });
    expect(pc.channel.sent.at(-1)).toEqual({ t: "b", c: 0x110, p: true });
    Reflect.deleteProperty(document, "pointerLockElement");
  });

  it("disconnects with a goodbye", async () => {
    const { pc, router, user } = await connected();
    await user.click(screen.getByRole("button", { name: "Trennen" }));
    expect(pc.channel.sent).toContainEqual({ t: "bye" });
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
    expect(currentSession()).toBeNull();
  });

  it("notices a dropped connection", async () => {
    const { pc, router, user } = await connected();
    act(() => pc.drop());
    expect(
      await screen.findByRole("heading", { name: "Verbindung mit zentrale beendet" }),
    ).toBeVisible();
    await user.click(screen.getByRole("button", { name: "Neu verbinden" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/"));
  });

  it("goes back to the PIN after a reload", async () => {
    const { router } = await renderViewer("/session/host", "form");
    expect(router.state.location.pathname).toBe("/");
  });
});
