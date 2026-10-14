import { describe, expect, it, vi } from "vitest";

import { fetchIceServers, hasRelay, iceServersFrom } from "./turn";

const env = { TURN_KEY_ID: "key", TURN_API_TOKEN: "token" };

describe("TURN credentials", () => {
  it("are read from either of Cloudflare's answers, checked", () => {
    const one = {
      iceServers: {
        urls: ["turn:turn.cloudflare.com:3478?transport=udp"],
        username: "u",
        credential: "c",
      },
    };
    expect(iceServersFrom(one)).toEqual([
      { urls: ["turn:turn.cloudflare.com:3478?transport=udp"], username: "u", credential: "c" },
    ]);
    const list = { iceServers: [{ urls: "stun:stun.cloudflare.com:3478" }, { urls: [1] }, null] };
    expect(iceServersFrom(list)).toEqual([{ urls: ["stun:stun.cloudflare.com:3478"] }]);
    expect(iceServersFrom(null)).toEqual([]);
    expect(iceServersFrom({ iceServers: { urls: [] } })).toEqual([]);
  });

  it("come from Cloudflare when a key is set, and never fail the offer", async () => {
    expect(hasRelay({})).toBe(false);
    expect(await fetchIceServers({}, vi.fn())).toEqual([]);
    const ok = vi.fn(async () =>
      Response.json({ iceServers: [{ urls: ["turn:t:3478"], username: "u", credential: "c" }] }),
    );
    expect(await fetchIceServers(env, ok as unknown as typeof fetch)).toHaveLength(1);
    const [url, init] = ok.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe(
      "https://rtc.live.cloudflare.com/v1/turn/keys/key/credentials/generate-ice-servers",
    );
    expect(init.headers).toMatchObject({ Authorization: "Bearer token" });
    const refused = vi.fn(async () => new Response("no", { status: 401 }));
    expect(await fetchIceServers(env, refused as unknown as typeof fetch)).toEqual([]);
    const down = vi.fn(async () => {
      throw new Error("network");
    });
    expect(await fetchIceServers(env, down as unknown as typeof fetch)).toEqual([]);
  });
});
