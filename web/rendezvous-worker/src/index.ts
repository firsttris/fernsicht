/**
 * The Fernsicht rendezvous service as a Cloudflare Worker
 * (docs/internet.md): a Durable Object per host, holding its WebSocket
 * with the hibernation API (an idle host costs no running time), passing
 * clients' sealed offers to it and its sealed answers back. The same
 * protocol as the Rust service in apps/rendezvous.
 */
import { DurableObject } from "cloudflare:workers";

import {
  type Attachment,
  firstAttachment,
  onAlarm,
  onMessage,
  type Peer,
  type RoomEnv,
  route,
  type Store,
} from "./lib/room";
import { fetchIceServers, hasRelay, type TurnEnv } from "./lib/turn";

export interface Env extends TurnEnv {
  HOSTS: DurableObjectNamespace<HostRoom>;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const r = route(url.pathname);
    if (!r) {
      return new Response("Fernsicht rendezvous service\n", {
        status: url.pathname === "/" ? 200 : 404,
      });
    }
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("expected a WebSocket\n", { status: 426 });
    }
    return env.HOSTS.get(env.HOSTS.idFromName(r.id)).fetch(request);
  },
} satisfies ExportedHandler<Env>;

/** A WebSocket as the room sees it. */
function peer(ws: WebSocket): Peer {
  return {
    send: (text) => {
      try {
        ws.send(text);
      } catch {
        // Closed meanwhile.
      }
    },
    close: (code, reason) => {
      try {
        ws.close(code, reason);
      } catch {
        // Closed already.
      }
    },
    attachment: () => ws.deserializeAttachment() as Attachment | null,
    attach: (a) => ws.serializeAttachment(a),
  };
}

export class HostRoom extends DurableObject<Env> {
  private store(): Store {
    const storage = this.ctx.storage;
    return {
      get: (key) => storage.get(key),
      put: (key, value) => storage.put(key, value),
      setAlarm: (at) => storage.setAlarm(at),
    };
  }

  private room(): RoomEnv {
    return {
      relay: hasRelay(this.env),
      iceServers: () => fetchIceServers(this.env, fetch),
      now: () => Date.now(),
    };
  }

  private peers(): Peer[] {
    return this.ctx.getWebSockets().map(peer);
  }

  async fetch(request: Request): Promise<Response> {
    const r = route(new URL(request.url).pathname);
    if (!r) return new Response(null, { status: 404 });
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    this.ctx.acceptWebSocket(server);
    const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
    server.serializeAttachment(firstAttachment(r, ip));
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    await onMessage(peer(ws), message, this.peers(), this.store(), this.room());
  }

  async webSocketClose(ws: WebSocket, code: number): Promise<void> {
    peer(ws).close(code === 1005 ? 1000 : code, "closed");
  }

  async alarm(): Promise<void> {
    await onAlarm(this.peers(), this.store(), this.room());
  }
}
