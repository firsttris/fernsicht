import { afterEach, describe, expect, it, vi } from "vitest";

import { FakePeerConnection, installFakes } from "../test-rtc";
import {
  ConnectError,
  connect,
  connectErrorText,
  currentSession,
  fetchHostInfo,
  summarize,
} from "./host";

afterEach(() => {
  currentSession()?.close();
  vi.unstubAllGlobals();
});

const answer = { type: "answer", sdp: "v=0 answer" };

describe("host API", () => {
  it("finds the host serving the page, or none", async () => {
    installFakes(() => ({ status: 200, body: { name: "zentrale", pairing: true } }));
    expect(await fetchHostInfo()).toEqual({ name: "zentrale", pairing: true });
    // A plain web server answers every path with the page.
    installFakes(() => ({ status: 200, body: "<!doctype html>", json: false }));
    expect(await fetchHostInfo()).toBeNull();
    installFakes(() => ({ status: 404, body: {} }));
    expect(await fetchHostInfo()).toBeNull();
    installFakes(() => ({ status: 200, body: { nonsense: 1 } }));
    expect(await fetchHostInfo()).toBeNull();
    installFakes(() => "offline");
    expect(await fetchHostInfo()).toBeNull();
  });

  it("offers to receive picture and sound and sends the PIN", async () => {
    const api = installFakes(() => ({ status: 200, body: { answer, width: 2560, height: 1440 } }));
    const s = await connect("482913", "zentrale");
    const pc = FakePeerConnection.last!;
    expect(pc.transceivers).toEqual([
      ["video", "recvonly"],
      ["audio", "recvonly"],
    ]);
    expect(pc.label).toBe("fernsicht");
    expect(pc.channel.binaryType).toBe("arraybuffer");
    expect(api).toHaveBeenCalledWith("/api/connect", expect.objectContaining({ method: "POST" }));
    expect(JSON.parse(api.mock.calls[0]![1]!.body as string)).toEqual({
      pin: "482913",
      offer: { type: "offer", sdp: "v=0 offer" },
    });
    expect(pc.remote).toEqual(answer);
    expect(s).toMatchObject({ hostName: "zentrale", width: 2560, height: 1440 });
    expect(currentSession()).toBe(s);
    expect((s.stream as unknown as { tracks: object[] }).tracks).toHaveLength(1);

    s.send({ t: "k", c: 30, p: true });
    expect(pc.channel.sent).toEqual([{ t: "k", c: 30, p: true }]);
    pc.channel.readyState = "closing";
    s.send({ t: "k", c: 30, p: false });
    expect(pc.channel.sent).toHaveLength(1);
    pc.channel.readyState = "open";

    vi.useFakeTimers();
    s.close();
    expect(pc.channel.sent.at(-1)).toEqual({ t: "bye" });
    expect(currentSession()).toBeNull();
    vi.advanceTimersByTime(200);
    expect(pc.closed).toBe(true);
    vi.useRealTimers();
  });

  it.each([
    [{ status: 403, body: { error: "wrong-pin" } }, "wrong-pin"],
    [{ status: 403, body: { error: "pairing-closed" } }, "pairing-closed"],
    [{ status: 500, body: {} }, "failed"],
    [{ status: 200, body: { width: 1 } }, "failed"],
    [{ status: 502, body: "<html>", json: false }, "failed"],
    ["offline" as const, "unreachable"],
  ])("turns %j into %s", async (reply, code) => {
    installFakes(() => reply);
    const err = await connect("000000", "zentrale").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ConnectError);
    expect((err as ConnectError).code).toBe(code);
    expect(FakePeerConnection.last!.closed).toBe(true);
    expect(currentSession()).toBeNull();
  });

  it("says every error in words", () => {
    for (const code of [
      "wrong-pin",
      "pairing-closed",
      "too-many-attempts",
      "unreachable",
      "failed",
    ] as const) {
      expect(connectErrorText(code).length).toBeGreaterThan(5);
    }
    expect(connectErrorText("wrong-pin")).toBe("Falsche PIN.");
  });
});

describe("overlay from WebRTC statistics", () => {
  const report = (n: number) =>
    new Map<string, Record<string, unknown>>([
      [
        "in",
        {
          type: "inbound-rtp",
          kind: "video",
          totalDecodeTime: 0.002 * n,
          framesDecoded: n,
          jitterBufferDelay: 0.01 * n,
          jitterBufferEmittedCount: n,
          packetsLost: n / 60,
          packetsReceived: 99 * (n / 60),
          framesDropped: 0,
          framesReceived: n,
          framesPerSecond: 60,
        },
      ],
      ["audio", { type: "inbound-rtp", kind: "audio", packetsLost: 999 }],
      ["pair", { type: "candidate-pair", nominated: true, currentRoundTripTime: 0.002 }],
      ["other-pair", { type: "candidate-pair", state: "waiting", currentRoundTripTime: 1 }],
    ]);
  const host = {
    captureUs: 300,
    encodeUs: 4000,
    fps: 59,
    bitrateBps: 35_000_000,
    width: 2560,
    height: 1440,
    codec: "H.264",
  };

  it("adds the host's share to the browser's", () => {
    const first = summarize(report(60), null, host);
    expect(first.stats.stagesUs).toEqual({
      capture: 300,
      encode: 4000,
      network: 1000,
      decode: 2000,
      present: 10_000,
    });
    expect(first.stats).toMatchObject({ codec: "H.264", fps: 60, bitrateBps: 35_000_000 });
    expect(first.stats.lossBeforeFec).toBeCloseTo(0.01);
    // The next second counts only what happened since.
    const second = summarize(report(120), first.sample, host);
    expect(second.stats.stagesUs.decode).toBeCloseTo(2000);
    expect(second.stats.lossAfterFec).toBe(0);
  });

  it("copes with empty statistics and no word from the host", () => {
    const { stats } = summarize([], null, null);
    expect(stats.stagesUs).toEqual({ capture: 0, encode: 0, network: 0, decode: 0, present: 0 });
    expect(stats).toMatchObject({ codec: "H.264", fps: 0, lossBeforeFec: 0 });
  });
});
