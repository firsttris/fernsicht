/**
 * Stand-ins for the browser's WebRTC objects (jsdom has none), recording
 * what the viewer does with them.
 */
import { vi } from "vitest";

export class FakeChannel extends EventTarget {
  readyState: RTCDataChannelState = "open";
  binaryType = "blob";
  sent: Record<string, unknown>[] = [];
  send(data: string) {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }
  /** A message from the host. */
  receive(data: string | ArrayBuffer) {
    this.dispatchEvent(new MessageEvent("message", { data }));
  }
}

export class FakePeerConnection extends EventTarget {
  static last: FakePeerConnection | undefined;
  transceivers: [string, string | undefined][] = [];
  channel = new FakeChannel();
  label = "";
  remote: RTCSessionDescriptionInit | undefined;
  closed = false;
  connectionState: RTCPeerConnectionState = "connected";
  ontrack: ((e: { receiver: object; track: object }) => void) | null = null;
  report = new Map<string, Record<string, unknown>>();

  constructor() {
    super();
    FakePeerConnection.last = this;
  }
  addTransceiver(kind: string, init?: RTCRtpTransceiverInit) {
    this.transceivers.push([kind, init?.direction]);
  }
  getReceivers() {
    return [{}, Object.freeze({})];
  }
  createDataChannel(label: string) {
    this.label = label;
    return this.channel;
  }
  createOffer() {
    return Promise.resolve({ type: "offer" as const, sdp: "v=0 offer" });
  }
  setLocalDescription() {
    return Promise.resolve();
  }
  setRemoteDescription(d: RTCSessionDescriptionInit) {
    this.remote = d;
    // The host's tracks arrive.
    this.ontrack?.({ receiver: {}, track: { kind: "video" } });
    return Promise.resolve();
  }
  getStats() {
    return Promise.resolve(this.report);
  }
  close() {
    this.closed = true;
  }
  /** The connection drops. */
  drop() {
    this.connectionState = "failed";
    this.dispatchEvent(new Event("connectionstatechange"));
  }
}

class FakeMediaStream {
  tracks: object[] = [];
  addTrack(t: object) {
    this.tracks.push(t);
  }
}

type Reply = { status: number; body: unknown; json?: boolean };

/** Installs the fakes; `api` answers the viewer's requests by path. */
export function installFakes(api: (path: string, body: unknown) => Reply | "offline") {
  vi.stubGlobal("RTCPeerConnection", FakePeerConnection);
  vi.stubGlobal("MediaStream", FakeMediaStream);
  const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
    const body: unknown = init?.body ? JSON.parse(init.body as string) : undefined;
    const r = api(url, body);
    if (r === "offline") throw new TypeError("Failed to fetch");
    const json = r.json ?? true;
    return new Response(json ? JSON.stringify(r.body) : String(r.body), {
      status: r.status,
      headers: { "Content-Type": json ? "application/json" : "text/html" },
    });
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}
