/**
 * Talking to the host that served this page: who it is, and starting a
 * WebRTC session with the PIN it shows. Without a host (development
 * server, the demo) there is no `/api`, and the viewer shows the demo.
 */
import type { SessionStats } from "@fernsicht/ui";

export interface HostInfo {
  name: string;
  /** Pairing is open: a PIN is shown at the host. */
  pairing: boolean;
}

/** The host serving this page, or `null` (no host: demo). */
export async function fetchHostInfo(): Promise<HostInfo | null> {
  try {
    const r = await fetch("/api/info", { headers: { Accept: "application/json" } });
    if (!r.ok || !(r.headers.get("content-type") ?? "").includes("json")) return null;
    const v = (await r.json()) as Partial<HostInfo>;
    return typeof v.name === "string" ? { name: v.name, pairing: !!v.pairing } : null;
  } catch {
    return null;
  }
}

/** Why the host said no (`/api/connect` error codes), or no answer. */
export type ConnectErrorCode =
  "wrong-pin" | "pairing-closed" | "too-many-attempts" | "failed" | "unreachable";

export class ConnectError extends Error {
  constructor(readonly code: ConnectErrorCode) {
    super(code);
  }
}

export function connectErrorText(code: ConnectErrorCode): string {
  switch (code) {
    case "wrong-pin":
      return "Falsche PIN.";
    case "pairing-closed":
      return "Am Host ist gerade keine Kopplung offen. Dort in der Fernsicht-App „Gerät koppeln“ wählen und die neue PIN eingeben.";
    case "too-many-attempts":
      return "Zu viele falsche PINs. Am Host die Kopplung neu öffnen.";
    case "unreachable":
      return "Der Host antwortet nicht.";
    default:
      return "Die Sitzung konnte nicht starten. Details stehen im Log des Hosts.";
  }
}

/** What the host sends once per second on the data channel. */
export interface HostStats {
  captureUs: number;
  encodeUs: number;
  fps: number;
  bitrateBps: number;
  width: number;
  height: number;
  codec: string;
}

/** A running browser session. */
export interface LiveSession {
  hostName: string;
  pc: RTCPeerConnection;
  channel: RTCDataChannel;
  stream: MediaStream;
  width: number;
  height: number;
  /** Sends an input message (see apps/host-agent/src/web.rs). */
  send(msg: Record<string, unknown>): void;
  /** Says goodbye and closes the connection. */
  close(): void;
}

let live: LiveSession | null = null;

/** The session the connect page started, for the session page. */
export const currentSession = () => live;

/** Starts a session with the host serving this page. */
export async function connect(pin: string, hostName: string): Promise<LiveSession> {
  const pc = new RTCPeerConnection();
  const stream = new MediaStream();
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.addTransceiver("audio", { direction: "recvonly" });
  // As little buffering as the browser allows: this is remote control,
  // not a film.
  for (const r of pc.getReceivers()) setLowLatency(r);
  pc.ontrack = (e) => {
    setLowLatency(e.receiver);
    stream.addTrack(e.track);
  };
  const channel = pc.createDataChannel("fernsicht");
  channel.binaryType = "arraybuffer";
  try {
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    let r: Response;
    try {
      r = await fetch("/api/connect", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ pin, offer: { type: offer.type, sdp: offer.sdp } }),
      });
    } catch {
      throw new ConnectError("unreachable");
    }
    const body = (await r.json().catch(() => ({}))) as {
      error?: ConnectErrorCode;
      answer?: RTCSessionDescriptionInit;
      width?: number;
      height?: number;
    };
    if (!r.ok || !body.answer) throw new ConnectError(body.error ?? "failed");
    await pc.setRemoteDescription(body.answer);
    const send = (msg: Record<string, unknown>) => {
      if (channel.readyState === "open") channel.send(JSON.stringify(msg));
    };
    const session: LiveSession = {
      hostName,
      pc,
      channel,
      stream,
      width: body.width ?? 0,
      height: body.height ?? 0,
      send,
      close: () => {
        send({ t: "bye" });
        // Let the goodbye leave before the connection goes.
        setTimeout(() => pc.close(), 100);
        if (live === session) live = null;
      },
    };
    live = session;
    return session;
  } catch (e) {
    pc.close();
    throw e instanceof ConnectError ? e : new ConnectError("failed");
  }
}

function setLowLatency(r: RTCRtpReceiver) {
  try {
    (r as RTCRtpReceiver & { jitterBufferTarget?: number }).jitterBufferTarget = 0;
  } catch {
    // Not every browser has it.
  }
}

/** Counters from the last `getStats()`, to compute per-second values. */
export interface StatsSample {
  totalDecodeTime: number;
  framesDecoded: number;
  jitterBufferDelay: number;
  jitterBufferEmittedCount: number;
  packetsLost: number;
  packetsReceived: number;
  framesDropped: number;
  framesReceived: number;
}

type Report = Iterable<[string, Record<string, unknown>]> | Map<string, Record<string, unknown>>;

const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : 0);

/**
 * The overlay from the browser's WebRTC statistics and the host's share:
 * capture and encode come from the host, network is half the round trip,
 * decode and the jitter buffer ("Anzeige") from the browser.
 */
export function summarize(
  report: Report,
  previous: StatsSample | null,
  host: HostStats | null,
): { stats: SessionStats; sample: StatsSample } {
  let video: Record<string, unknown> | undefined;
  let rtt = 0;
  for (const [, s] of report instanceof Map ? report.entries() : report) {
    if (s.type === "inbound-rtp" && s.kind === "video") video = s;
    if (s.type === "candidate-pair" && (s.nominated || s.state === "succeeded")) {
      rtt = Math.max(rtt, num(s.currentRoundTripTime));
    }
  }
  const sample: StatsSample = {
    totalDecodeTime: num(video?.totalDecodeTime),
    framesDecoded: num(video?.framesDecoded),
    jitterBufferDelay: num(video?.jitterBufferDelay),
    jitterBufferEmittedCount: num(video?.jitterBufferEmittedCount),
    packetsLost: num(video?.packetsLost),
    packetsReceived: num(video?.packetsReceived),
    framesDropped: num(video?.framesDropped),
    framesReceived: num(video?.framesReceived),
  };
  const p = previous ?? {
    totalDecodeTime: 0,
    framesDecoded: 0,
    jitterBufferDelay: 0,
    jitterBufferEmittedCount: 0,
    packetsLost: 0,
    packetsReceived: 0,
    framesDropped: 0,
    framesReceived: 0,
  };
  const per = (a: number, b: number) => (b > 0 ? a / b : 0);
  const lost = sample.packetsLost - p.packetsLost;
  const received = sample.packetsReceived - p.packetsReceived;
  const stats: SessionStats = {
    stagesUs: {
      capture: host?.captureUs ?? 0,
      encode: host?.encodeUs ?? 0,
      network: (rtt * 1e6) / 2,
      decode:
        per(sample.totalDecodeTime - p.totalDecodeTime, sample.framesDecoded - p.framesDecoded) *
        1e6,
      present:
        per(
          sample.jitterBufferDelay - p.jitterBufferDelay,
          sample.jitterBufferEmittedCount - p.jitterBufferEmittedCount,
        ) * 1e6,
    },
    codec: host?.codec ?? "H.264",
    fps: num(video?.framesPerSecond) || (host?.fps ?? 0),
    bitrateBps: host?.bitrateBps ?? 0,
    lossBeforeFec: per(Math.max(0, lost), Math.max(0, lost) + received),
    lossAfterFec: per(
      sample.framesDropped - p.framesDropped,
      sample.framesReceived - p.framesReceived,
    ),
  };
  return { stats, sample };
}
