/**
 * TURN credentials from Cloudflare's TURN service (Realtime → TURN), for
 * hosts to pass to their clients. Optional: without a key the service
 * hands out none and connections are direct only.
 */
import type { IceServer } from "./protocol";

export interface TurnEnv {
  TURN_KEY_ID?: string;
  TURN_API_TOKEN?: string;
}

/** How long handed-out credentials stay valid. */
const TTL_SECONDS = 4 * 60 * 60;

export const hasRelay = (env: TurnEnv): boolean => !!(env.TURN_KEY_ID && env.TURN_API_TOKEN);

const isStrings = (v: unknown): v is string[] =>
  Array.isArray(v) && v.every((u) => typeof u === "string");

/** The ICE servers in Cloudflare's answer, in either of its shapes (one
 * object, or a list), checked. */
export function iceServersFrom(body: unknown): IceServer[] {
  if (typeof body !== "object" || body === null) return [];
  const list = (body as { iceServers?: unknown }).iceServers;
  const entries = Array.isArray(list) ? list : [list];
  return entries.flatMap((e): IceServer[] => {
    if (typeof e !== "object" || e === null) return [];
    const { urls, username, credential } = e as Record<string, unknown>;
    const all = typeof urls === "string" ? [urls] : urls;
    if (!isStrings(all) || all.length === 0) return [];
    return [
      {
        urls: all,
        ...(typeof username === "string" ? { username } : {}),
        ...(typeof credential === "string" ? { credential } : {}),
      },
    ];
  });
}

/** Fresh credentials, or none (no key set, or Cloudflare said no). */
export async function fetchIceServers(env: TurnEnv, fetcher: typeof fetch): Promise<IceServer[]> {
  if (!hasRelay(env)) return [];
  const url = `https://rtc.live.cloudflare.com/v1/turn/keys/${env.TURN_KEY_ID}/credentials/generate-ice-servers`;
  try {
    const r = await fetcher(url, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${env.TURN_API_TOKEN}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ ttl: TTL_SECONDS }),
    });
    return r.ok ? iceServersFrom(await r.json()) : [];
  } catch {
    return [];
  }
}
