import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";

import { deviceQuery, devicesQuery, sessionStatsQuery, thisMachineQuery } from "./api";

const client = () => new QueryClient();

describe("api", () => {
  it("loads devices", async () => {
    const devices = await client().fetchQuery(devicesQuery);
    expect(devices).toHaveLength(4);
  });

  it("loads a known device and synthesises an unknown one", async () => {
    const qc = client();
    expect((await qc.fetchQuery(deviceQuery("903118452"))).name).toBe("heimserver");
    const unknown = await qc.fetchQuery(deviceQuery("111222333"));
    expect(unknown).toMatchObject({ id: "111222333", name: "111 222 333", online: true });
  });

  it("loads this machine and session stats", async () => {
    const qc = client();
    expect(await qc.fetchQuery(thisMachineQuery)).toEqual({ id: "482913057", code: "k7f-2qx" });
    const stats = await qc.fetchQuery(sessionStatsQuery("214776390"));
    expect(stats.codec).toBe("AV1 · VAAPI");
    expect(sessionStatsQuery("x").refetchInterval).toBe(1000);
  });
});
