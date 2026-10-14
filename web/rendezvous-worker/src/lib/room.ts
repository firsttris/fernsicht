/**
 * One host's room (a Durable Object per rendezvous ID): the host's
 * WebSocket, the clients waiting for its answer. What each socket is lives
 * in its attachment, so the room can sleep between messages (WebSocket
 * hibernation) and pick up where it was.
 *
 * Kept free of the Workers runtime: the Durable Object in `index.ts`
 * hands in its sockets and storage through the small interfaces here, and
 * the tests hand in fakes.
 */
import { makeChallenge, rendezvousId, unhex, verifyProof } from "./crypto";
import {
  ANSWER_TIMEOUT_MS,
  ERROR_TEXT,
  type ErrorCode,
  type IceServer,
  OFFERS_PER_MINUTE,
  parseClientMessage,
  parseHostMessage,
  toClient,
  toHost,
} from "./protocol";
import { take } from "./rate";

/** What a socket is, kept with it across hibernation. */
export type Attachment =
  | { role: "host"; id: string; stage: "register" }
  | {
      role: "host";
      id: string;
      stage: "proof";
      key: string;
      secret: JsonWebKey;
      nonce: string;
    }
  | { role: "host"; id: string; stage: "ready"; since: number }
  | { role: "client"; id: string; ip: string; stage: "new" }
  | {
      role: "client";
      id: string;
      ip: string;
      stage: "waiting";
      connection: number;
      deadline: number;
    };

/** A WebSocket of the room. */
export interface Peer {
  send(text: string): void;
  close(code: number, reason: string): void;
  attachment(): Attachment | null;
  attach(a: Attachment): void;
}

/** The room's storage and alarm. */
export interface Store {
  get<T>(key: string): Promise<T | undefined>;
  put<T>(key: string, value: T): Promise<void>;
  setAlarm(at: number): Promise<void>;
}

/** What the room needs from outside. */
export interface RoomEnv {
  /** The service hands out TURN credentials. */
  relay: boolean;
  iceServers(): Promise<IceServer[]>;
  now(): number;
}

/** WebSocket close codes: normal, and "policy violation". */
const NORMAL = 1000;
const POLICY = 1008;

function failHost(peer: Peer, code: ErrorCode) {
  peer.send(toHost({ type: "error", code, message: ERROR_TEXT[code] }));
  peer.close(POLICY, code);
}

function failClient(peer: Peer, code: ErrorCode) {
  peer.send(toClient({ type: "error", code, message: ERROR_TEXT[code] }));
  peer.close(NORMAL, code);
}

/** The host that proved its key, if one is here. */
function readyHost(peers: readonly Peer[]): Peer | undefined {
  return peers.find((p) => {
    const a = p.attachment();
    return a?.role === "host" && a.stage === "ready";
  });
}

/** The client waiting for `connection`. */
function waitingClient(peers: readonly Peer[], connection: number): Peer | undefined {
  return peers.find((p) => {
    const a = p.attachment();
    return a?.role === "client" && a.stage === "waiting" && a.connection === connection;
  });
}

async function onHostMessage(
  peer: Peer,
  a: Extract<Attachment, { role: "host" }>,
  data: string | ArrayBuffer,
  peers: readonly Peer[],
  env: RoomEnv,
): Promise<void> {
  const msg = parseHostMessage(data);
  if (!msg) return failHost(peer, "badRequest");
  if (a.stage === "register" && msg.type === "register") {
    const key = unhex(msg.key, 32);
    if (!key || (await rendezvousId(key)) !== a.id) return failHost(peer, "badRequest");
    const c = await makeChallenge();
    peer.attach({
      role: "host",
      id: a.id,
      stage: "proof",
      key: msg.key,
      secret: c.secret,
      nonce: c.nonce,
    });
    peer.send(toHost({ type: "challenge", ephemeral: c.ephemeral, nonce: c.nonce }));
    return;
  }
  if (a.stage === "proof" && msg.type === "proof") {
    const key = unhex(a.key, 32);
    if (!key || !(await verifyProof(a.secret, a.nonce, key, msg.mac))) {
      return failHost(peer, "unauthorized");
    }
    // A host registering again (a new connection after a network change)
    // replaces the old one.
    for (const other of peers) {
      const o = other.attachment();
      if (other !== peer && o?.role === "host" && o.stage === "ready") failHost(other, "replaced");
    }
    peer.attach({ role: "host", id: a.id, stage: "ready", since: env.now() });
    peer.send(toHost({ type: "registered", relay: env.relay }));
    return;
  }
  if (a.stage === "ready" && (msg.type === "answer" || msg.type === "refuse")) {
    const client = waitingClient(peers, msg.connection);
    if (!client) return; // It gave up meanwhile.
    if (msg.type === "answer") {
      client.send(toClient({ type: "answer", answer: msg.answer }));
      client.close(NORMAL, "answered");
    } else {
      failClient(client, "refused");
    }
    return;
  }
  failHost(peer, "badRequest");
}

async function onClientMessage(
  peer: Peer,
  a: Extract<Attachment, { role: "client" }>,
  data: string | ArrayBuffer,
  peers: readonly Peer[],
  store: Store,
  env: RoomEnv,
): Promise<void> {
  const msg = parseClientMessage(data);
  if (!msg || a.stage !== "new") return failClient(peer, "badRequest");
  const now = env.now();
  const byHost = take((await store.get<number[]>("hits:host")) ?? [], now, OFFERS_PER_MINUTE);
  const ipKey = `hits:ip:${a.ip}`;
  const byIp = take((await store.get<number[]>(ipKey)) ?? [], now, OFFERS_PER_MINUTE);
  await store.put("hits:host", byHost.hits);
  await store.put(ipKey, byIp.hits);
  if (!byHost.allowed || !byIp.allowed) return failClient(peer, "rateLimited");
  const host = readyHost(peers);
  if (!host) return failClient(peer, "offline");
  const connection = (((await store.get<number>("next")) ?? 0) + 1) >>> 0;
  await store.put("next", connection);
  const deadline = now + ANSWER_TIMEOUT_MS;
  peer.attach({ role: "client", id: a.id, ip: a.ip, stage: "waiting", connection, deadline });
  const iceServers = env.relay ? await env.iceServers() : [];
  host.send(toHost({ type: "offer", connection, offer: msg.offer, iceServers }));
  await store.setAlarm(deadline);
}

/** A message on one of the room's sockets. */
export async function onMessage(
  peer: Peer,
  data: string | ArrayBuffer,
  peers: readonly Peer[],
  store: Store,
  env: RoomEnv,
): Promise<void> {
  const a = peer.attachment();
  if (!a) return peer.close(POLICY, "unknown");
  if (a.role === "host") return onHostMessage(peer, a, data, peers, env);
  return onClientMessage(peer, a, data, peers, store, env);
}

/** The alarm: clients whose host did not answer in time are told so. */
export async function onAlarm(peers: readonly Peer[], store: Store, env: RoomEnv): Promise<void> {
  const now = env.now();
  let next = Number.POSITIVE_INFINITY;
  for (const p of peers) {
    const a = p.attachment();
    if (a?.role !== "client" || a.stage !== "waiting") continue;
    if (a.deadline <= now) failClient(p, "timeout");
    else next = Math.min(next, a.deadline);
  }
  if (Number.isFinite(next)) await store.setAlarm(next);
}

/** What a new WebSocket to the room is, from its path. */
export function route(path: string): { role: "host" | "client"; id: string } | null {
  const m = /^\/v1\/(host|connect)\/([0-9a-f]{64})$/.exec(path);
  if (!m) return null;
  return { role: m[1] === "host" ? "host" : "client", id: m[2]! };
}

/** The attachment a new socket starts with. */
export function firstAttachment(
  r: { role: "host" | "client"; id: string },
  ip: string,
): Attachment {
  return r.role === "host"
    ? { role: "host", id: r.id, stage: "register" }
    : { role: "client", id: r.id, ip, stage: "new" };
}
