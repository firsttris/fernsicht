import { afterEach, describe, expect, it, vi } from "vitest";

import { FakePeerConnection, installFakes } from "../test-rtc";
import {
  connect,
  connectErrorText,
  connectionPhase,
  currentSession,
  fetchHostInfo,
  frameTiming,
  holdSession,
  type HostStats,
  meanTiming,
  parseToBrowser,
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
    const api = installFakes(() => ({
      status: 200,
      body: { answer, width: 2560, height: 1440, fps: 60 },
    }));
    const r = await connect("482913", "zentrale");
    if (!("session" in r)) throw new Error(r.error);
    const s = r.session;
    const pc = FakePeerConnection.last!;
    expect(pc.transceivers).toEqual([
      ["video", "recvonly"],
      ["audio", "recvonly"],
    ]);
    expect([...pc.channels.keys()]).toEqual(["fernsicht", "moves"]);
    expect(pc.channel.binaryType).toBe("arraybuffer");
    expect(pc.moves.init).toEqual({ ordered: false, maxRetransmits: 0 });
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

    // Moves take the unordered channel; positions are numbered on both
    // channels from one counter, so the host can drop stale ones.
    s.move({ t: "m", x: 1, y: 2 });
    s.send({ t: "m", x: 3, y: 4 });
    s.move({ t: "r", dx: 5, dy: 0 });
    expect(pc.moves.sent).toEqual([
      { t: "m", x: 1, y: 2, s: 1 },
      { t: "r", dx: 5, dy: 0 },
    ]);
    expect(pc.channel.sent.at(-1)).toEqual({ t: "m", x: 3, y: 4, s: 2 });
    // Until the moves channel is open, they go the reliable way.
    pc.moves.readyState = "connecting";
    s.move({ t: "m", x: 7, y: 8 });
    expect(pc.channel.sent.at(-1)).toEqual({ t: "m", x: 7, y: 8, s: 3 });
    pc.moves.readyState = "open";

    vi.useFakeTimers();
    s.close();
    expect(pc.channel.sent.at(-1)).toEqual({ t: "bye" });
    expect(currentSession()).toBeNull();
    vi.advanceTimersByTime(200);
    expect(pc.closed).toBe(true);
    // Once: a second close says no second goodbye.
    const sent = pc.channel.sent.length;
    s.close();
    expect(pc.channel.sent).toHaveLength(sent);
    vi.useRealTimers();
  });

  it("tells a passing interruption from the end", () => {
    expect(connectionPhase("connected", "open")).toBe("live");
    expect(connectionPhase("connecting", "connecting")).toBe("live");
    expect(connectionPhase("disconnected", "open")).toBe("interrupted");
    expect(connectionPhase("failed", "open")).toBe("ended");
    expect(connectionPhase("closed", "open")).toBe("ended");
    // The host closed the data channel: over, whatever ICE says.
    expect(connectionPhase("connected", "closed")).toBe("ended");
    expect(connectionPhase("disconnected", "closing")).toBe("ended");
  });

  it("closes a session no page holds any more", async () => {
    installFakes(() => ({ status: 200, body: { answer, width: 1920, height: 1080, fps: 60 } }));
    const r = await connect("482913", "zentrale");
    if (!("session" in r)) throw new Error(r.error);
    const s = r.session;
    const pc = FakePeerConnection.last!;
    vi.useFakeTimers();
    // StrictMode: let go and take it again at once → stays open.
    holdSession(s)();
    const release = holdSession(s);
    vi.advanceTimersByTime(500);
    expect(pc.channel.sent).not.toContainEqual({ t: "bye" });
    // The page goes away → goodbye and closed.
    release();
    vi.advanceTimersByTime(500);
    expect(pc.channel.sent).toContainEqual({ t: "bye" });
    expect(pc.closed).toBe(true);
    vi.useRealTimers();
  });

  it.each([
    [{ status: 403, body: { error: "wrong-pin" } }, "wrong-pin"],
    [{ status: 403, body: { error: "pairing-closed" } }, "pairing-closed"],
    [{ status: 403, body: { error: "wrong-host" } }, "wrong-host"],
    [{ status: 403, body: { error: "something-new" } }, "failed"],
    [{ status: 500, body: {} }, "failed"],
    [{ status: 200, body: { width: 1 } }, "failed"],
    [
      { status: 200, body: { answer: { type: "offer", sdp: "v=0" }, width: 1, height: 1, fps: 1 } },
      "failed",
    ],
    [{ status: 502, body: "<html>", json: false }, "failed"],
    ["offline" as const, "unreachable"],
  ])("turns %j into %s", async (reply, code) => {
    installFakes(() => reply);
    expect(await connect("000000", "zentrale")).toEqual({ error: code });
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
      "wrong-host",
      "forbidden",
      "bad-request",
      "not-found",
    ] as const) {
      expect(connectErrorText(code).length).toBeGreaterThan(5);
    }
    expect(connectErrorText("wrong-pin")).toBe("Falsche PIN.");
  });
});

describe("messages from the host", () => {
  it("takes stats and monitors that have their fields, nothing else", () => {
    const stats = {
      type: "stats",
      captureUs: 1,
      encodeUs: 2,
      fps: 60,
      bitrateBps: 3,
      width: 1920,
      height: 1080,
      codec: "AV1",
    };
    expect(parseToBrowser(JSON.stringify(stats))).toEqual(stats);
    const monitors = {
      type: "monitors",
      current: 1,
      list: [{ name: "DP-1", width: 1, height: 2 }],
    };
    expect(parseToBrowser(JSON.stringify(monitors))).toEqual(monitors);
    for (const bad of [
      JSON.stringify({ ...stats, fps: "60" }),
      JSON.stringify({ ...stats, codec: undefined }),
      JSON.stringify({ ...monitors, list: [{ name: 1, width: 1, height: 2 }] }),
      JSON.stringify({ type: "other" }),
      "not json",
      "null",
      new ArrayBuffer(4),
    ]) {
      expect(parseToBrowser(bad)).toBeNull();
    }
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
  const host: HostStats = {
    type: "stats",
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
    // No codec guessed before the host says it; no round trip known.
    expect(stats).toMatchObject({ codec: "–", fps: 0, lossBeforeFec: 0, rttUs: null });
  });

  it("says there is no FEC and gives the round trip", () => {
    const { stats } = summarize(report(60), null, host);
    expect(stats.fec).toBe(false);
    expect(stats.rttUs).toBeCloseTo(2000);
  });

  it("takes decode and display from the frames shown", () => {
    const shown = [
      frameTiming({ receiveTime: 1000, processingDuration: 0.002, expectedDisplayTime: 1010 }),
      frameTiming({ receiveTime: 2000, processingDuration: 0.004, expectedDisplayTime: 2012 }),
    ].filter((t) => t !== null);
    expect(shown).toEqual([
      { decodeUs: 2000, presentUs: 8000 },
      { decodeUs: 4000, presentUs: 8000 },
    ]);
    const { stats } = summarize(report(60), null, host, meanTiming(shown));
    expect(stats.stagesUs.decode).toBe(3000);
    expect(stats.stagesUs.present).toBe(8000);
    // Browsers without these fields: none, and the statistics are used.
    expect(frameTiming({ expectedDisplayTime: 5 })).toBeNull();
    expect(meanTiming([])).toBeNull();
  });
});
