/**
 * Stand-ins for the browser's WebRTC objects (jsdom has none), recording
 * what the viewer does with them.
 */
import { vi } from "vitest";

export class FakeChannel extends EventTarget {
  readyState: RTCDataChannelState = "open";
  binaryType = "blob";
  init: RTCDataChannelInit | undefined;
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
  /** Data channels by label, in the order they were made. */
  channels = new Map<string, FakeChannel>();
  remote: RTCSessionDescriptionInit | undefined;
  closed = false;
  connectionState: RTCPeerConnectionState = "connected";
  iceGatheringState: RTCIceGatheringState = "new";
  config: RTCConfiguration | undefined;
  localDescription: RTCSessionDescriptionInit | null = null;
  ontrack: ((e: { receiver: object; track: object }) => void) | null = null;
  report = new Map<string, Record<string, unknown>>();

  constructor(config?: RTCConfiguration) {
    super();
    this.config = config;
    FakePeerConnection.last = this;
  }
  addTransceiver(kind: string, init?: RTCRtpTransceiverInit) {
    this.transceivers.push([kind, init?.direction]);
  }
  getReceivers() {
    return [{}, Object.freeze({})];
  }
  createDataChannel(label: string, init?: RTCDataChannelInit) {
    const c = new FakeChannel();
    c.init = init;
    this.channels.set(label, c);
    return c;
  }
  /** The reliable channel: everything but pointer moves. */
  get channel(): FakeChannel {
    return this.channels.get("fernsicht") ?? new FakeChannel();
  }
  /** Pointer moves: unordered, no retransmits. */
  get moves(): FakeChannel {
    return this.channels.get("moves") ?? new FakeChannel();
  }
  createOffer() {
    return Promise.resolve({ type: "offer" as const, sdp: "v=0 offer" });
  }
  setLocalDescription(d?: RTCSessionDescriptionInit) {
    this.localDescription = d ?? null;
    // With STUN servers, gathering finishes a moment later, the offer then
    // carrying the addresses found.
    if (this.config?.iceServers?.length) {
      setTimeout(() => {
        this.localDescription = { type: "offer", sdp: "v=0 offer a=candidate:srflx" };
        this.iceGatheringState = "complete";
        this.dispatchEvent(new Event("icegatheringstatechange"));
      }, 10);
    }
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
    this.setState("failed");
  }
  /** The connection's state changes. */
  setState(state: RTCPeerConnectionState) {
    this.connectionState = state;
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
