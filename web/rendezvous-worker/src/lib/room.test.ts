// @vitest-environment node
import { describe, expect, it } from "vitest";

import { hex, proof, rendezvousId, unhex } from "./crypto";
import { ANSWER_TIMEOUT_MS, OFFERS_PER_MINUTE } from "./protocol";
import {
  type Attachment,
  firstAttachment,
  onAlarm,
  onMessage,
  type Peer,
  type RoomEnv,
  route,
  type Store,
} from "./room";

/** A socket that keeps what it was sent. */
class FakePeer implements Peer {
  sent: unknown[] = [];
  closed: { code: number; reason: string } | null = null;
  constructor(private a: Attachment) {}
  send(text: string) {
    this.sent.push(JSON.parse(text));
  }
  close(code: number, reason: string) {
    this.closed = { code, reason };
  }
  attachment() {
    return this.a;
  }
  attach(a: Attachment) {
    this.a = a;
  }
  last() {
    return this.sent.at(-1) as Record<string, unknown>;
  }
}

function fakeStore(): Store & { alarm: number | null } {
  const data = new Map<string, unknown>();
  return {
    alarm: null,
    get: async <T>(k: string) => data.get(k) as T | undefined,
    put: async <T>(k: string, v: T) => {
      data.set(k, v);
    },
    async setAlarm(at: number) {
      this.alarm = at;
    },
  };
}

function env(now: { t: number }, relay = false): RoomEnv {
  return {
    relay,
    iceServers: async () => [{ urls: ["turn:t:3478"], username: "u", credential: "c" }],
    now: () => now.t,
  };
}

/** A host with a fresh key that registers and proves it. */
async function registeredHost(peers: Peer[], store: Store, e: RoomEnv) {
  const pair = (await crypto.subtle.generateKey({ name: "X25519" }, true, [
    "deriveBits",
  ])) as CryptoKeyPair;
  const key = new Uint8Array((await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer);
  const id = await rendezvousId(key);
  const host = new FakePeer(firstAttachment({ role: "host", id }, "198.51.100.1"));
  peers.push(host);
  await onMessage(host, JSON.stringify({ type: "register", key: hex(key) }), peers, store, e);
  const challenge = host.last();
  expect(challenge.type).toBe("challenge");
  const theirs = await crypto.subtle.importKey(
    "raw",
    unhex(challenge.ephemeral as string, 32)!,
    { name: "X25519" },
    false,
    [],
  );
  const shared = new Uint8Array(
    await crypto.subtle.deriveBits(
      { name: "X25519", public: theirs } as never,
      pair.privateKey,
      256,
    ),
  );
  const mac = hex(await proof(shared, unhex(challenge.nonce as string, 16)!, key));
  await onMessage(host, JSON.stringify({ type: "proof", mac }), peers, store, e);
  return { host, id, key };
}

function client(id: string, ip = "203.0.113.9") {
  return new FakePeer(firstAttachment({ role: "client", id }, ip));
}

describe("a host's room", () => {
  it("passes an offer to the proven host and its answer back", async () => {
    const now = { t: 1_000_000 };
    const peers: Peer[] = [];
    const store = fakeStore();
    const e = env(now, true);
    const { host, id } = await registeredHost(peers, store, e);
    expect(host.last()).toEqual({ type: "registered", relay: true });
    const c = client(id);
    peers.push(c);
    await onMessage(c, JSON.stringify({ type: "connect", offer: "QUJD" }), peers, store, e);
    const offer = host.last();
    expect(offer).toMatchObject({ type: "offer", offer: "QUJD", iceServers: [{ username: "u" }] });
    expect(store.alarm).toBe(now.t + ANSWER_TIMEOUT_MS);
    await onMessage(
      host,
      JSON.stringify({ type: "answer", connection: offer.connection, answer: "REVG" }),
      peers,
      store,
      e,
    );
    expect(c.last()).toEqual({ type: "answer", answer: "REVG" });
    expect(c.closed?.code).toBe(1000);
  });

  it("tells clients when the host is away, refuses, or is too slow", async () => {
    const now = { t: 1_000_000 };
    const peers: Peer[] = [];
    const store = fakeStore();
    const e = env(now);
    const id = "a".repeat(64);
    const lonely = client(id);
    await onMessage(lonely, JSON.stringify({ type: "connect", offer: "QUJD" }), [lonely], store, e);
    expect(lonely.last()).toMatchObject({ type: "error", code: "offline" });

    const { host, id: hostId } = await registeredHost(peers, store, e);
    const refused = client(hostId);
    peers.push(refused);
    await onMessage(refused, JSON.stringify({ type: "connect", offer: "QUJD" }), peers, store, e);
    const connection = host.last().connection;
    await onMessage(host, JSON.stringify({ type: "refuse", connection }), peers, store, e);
    expect(refused.last()).toMatchObject({ type: "error", code: "refused" });

    const slow = client(hostId, "203.0.113.10");
    peers.push(slow);
    await onMessage(slow, JSON.stringify({ type: "connect", offer: "QUJD" }), peers, store, e);
    now.t += ANSWER_TIMEOUT_MS - 1;
    await onAlarm(peers, store, e);
    expect(slow.closed).toBeNull();
    now.t += 1;
    await onAlarm(peers, store, e);
    expect(slow.last()).toMatchObject({ type: "error", code: "timeout" });
  });

  it("limits offers per minute", async () => {
    const now = { t: 1_000_000 };
    const peers: Peer[] = [];
    const store = fakeStore();
    const e = env(now);
    const { id } = await registeredHost(peers, store, e);
    const codes: unknown[] = [];
    for (let i = 0; i <= OFFERS_PER_MINUTE; i++) {
      const c = client(id);
      await onMessage(
        c,
        JSON.stringify({ type: "connect", offer: "QUJD" }),
        [...peers, c],
        store,
        e,
      );
      codes.push(c.last()?.code);
    }
    expect(codes.at(-1)).toBe("rateLimited");
    expect(codes.slice(0, -1).every((c) => c === undefined)).toBe(true);
  });

  it("turns away wrong keys, failed proofs and messages out of turn", async () => {
    const now = { t: 1 };
    const store = fakeStore();
    const e = env(now);
    const wrongId = new FakePeer(firstAttachment({ role: "host", id: "b".repeat(64) }, "x"));
    await onMessage(
      wrongId,
      JSON.stringify({ type: "register", key: "11".repeat(32) }),
      [wrongId],
      store,
      e,
    );
    expect(wrongId.last()).toMatchObject({ code: "badRequest" });

    const key = new Uint8Array(32).fill(1);
    const id = await rendezvousId(key);
    const impostor = new FakePeer(firstAttachment({ role: "host", id }, "x"));
    await onMessage(
      impostor,
      JSON.stringify({ type: "register", key: hex(key) }),
      [impostor],
      store,
      e,
    );
    await onMessage(
      impostor,
      JSON.stringify({ type: "proof", mac: "00".repeat(32) }),
      [impostor],
      store,
      e,
    );
    expect(impostor.last()).toMatchObject({ code: "unauthorized" });
    expect(impostor.closed?.code).toBe(1008);

    const early = new FakePeer(firstAttachment({ role: "host", id }, "x"));
    await onMessage(
      early,
      JSON.stringify({ type: "answer", connection: 1, answer: "QQ==" }),
      [early],
      store,
      e,
    );
    expect(early.last()).toMatchObject({ code: "badRequest" });
  });

  it("lets a new registration of the same host replace the old", async () => {
    const now = { t: 1 };
    const peers: Peer[] = [];
    const store = fakeStore();
    const e = env(now);
    const first = await registeredHost(peers, store, e);
    // The same host again (a new connection): rebuilt with the same key
    // is not possible here, so a second proven host in the room stands in.
    const second = await registeredHost(peers, store, e);
    expect(first.host.last()).toMatchObject({ code: "replaced" });
    expect(second.host.last()).toEqual({ type: "registered", relay: false });
  });
});

describe("routes", () => {
  it("lead hosts and clients to their room", () => {
    const id = "c".repeat(64);
    expect(route(`/v1/host/${id}`)).toEqual({ role: "host", id });
    expect(route(`/v1/connect/${id}`)).toEqual({ role: "client", id });
    expect(route("/v1/host/xyz")).toBeNull();
    expect(route(`/v1/host/${id}/more`)).toBeNull();
  });
});
