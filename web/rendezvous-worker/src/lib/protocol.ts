/**
 * The service's messages, checked at runtime: what arrives on a WebSocket
 * is someone else's, not a type to cast to. The types are generated from
 * the Rust ones (crates/internet/src/signal.rs).
 */
import type { ClientToService } from "./bindings/ClientToService";
import type { ErrorCode } from "./bindings/ErrorCode";
import type { HostToService } from "./bindings/HostToService";
import type { IceServer } from "./bindings/IceServer";
import type { ServiceToClient } from "./bindings/ServiceToClient";
import type { ServiceToHost } from "./bindings/ServiceToHost";

export type {
  ClientToService,
  ErrorCode,
  HostToService,
  IceServer,
  ServiceToClient,
  ServiceToHost,
};

/** Largest message either side accepts (as the Rust side). */
export const MAX_MESSAGE = 16 * 1024;
/** How long a client's offer waits for the host's answer. */
export const ANSWER_TIMEOUT_MS = 10_000;
/** Most offers a minute to one host, and from one client address. */
export const OFFERS_PER_MINUTE = 10;
/** Longest sealed offer or answer (base64 of 8 KiB). */
const MAX_SEALED = 11_000;

const isObject = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null;
const isConnection = (v: unknown): v is number =>
  typeof v === "number" && Number.isInteger(v) && v >= 0 && v <= 0xffff_ffff;
const isSealed = (v: unknown): v is string =>
  typeof v === "string" && v.length > 0 && v.length <= MAX_SEALED && /^[A-Za-z0-9+/=]*$/.test(v);

function parse(data: string | ArrayBuffer): Record<string, unknown> | null {
  if (typeof data !== "string" || data.length > MAX_MESSAGE) return null;
  try {
    const v: unknown = JSON.parse(data);
    return isObject(v) ? v : null;
  } catch {
    return null;
  }
}

export function parseHostMessage(data: string | ArrayBuffer): HostToService | null {
  const v = parse(data);
  if (!v) return null;
  switch (v.type) {
    case "register":
      return typeof v.key === "string" ? { type: "register", key: v.key } : null;
    case "proof":
      return typeof v.mac === "string" ? { type: "proof", mac: v.mac } : null;
    case "answer":
      return isConnection(v.connection) && isSealed(v.answer)
        ? { type: "answer", connection: v.connection, answer: v.answer }
        : null;
    case "refuse":
      return isConnection(v.connection) ? { type: "refuse", connection: v.connection } : null;
    default:
      return null;
  }
}

export function parseClientMessage(data: string | ArrayBuffer): ClientToService | null {
  const v = parse(data);
  return v && v.type === "connect" && isSealed(v.offer)
    ? { type: "connect", offer: v.offer }
    : null;
}

export const toHost = (m: ServiceToHost): string => JSON.stringify(m);
export const toClient = (m: ServiceToClient): string => JSON.stringify(m);

/** What a client is told for each error. */
export const ERROR_TEXT: Record<ErrorCode, string> = {
  offline: "the host is not connected to this service",
  timeout: "the host did not answer",
  refused: "the host refused the offer",
  rateLimited: "too many offers; wait a minute",
  badRequest: "malformed message",
  unauthorized: "key proof failed",
  replaced: "another connection registered with this key",
};
