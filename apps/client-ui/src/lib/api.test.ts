import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";

import {
  actions,
  deviceQuery,
  devicesQuery,
  errorText,
  sessionQuery,
  thisMachineQuery,
} from "./api";

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
    const session = await qc.fetchQuery(sessionQuery("214776390"));
    expect(session.active).toBe(true);
    expect(session.stats?.codec).toBe("AV1 · VAAPI");
    expect(sessionQuery("x").refetchInterval).toBe(1000);
  });

  it("does nothing on disconnect outside the app", async () => {
    await expect(actions.disconnect()).resolves.toBeUndefined();
  });

  it("puts the backend's failures in the UI's words by their code", () => {
    expect(errorText({ code: "wrong-pin", message: "wrong PIN" })).toBe("Falsche PIN.");
    expect(
      errorText({
        code: "too-many-attempts",
        message: "192.0.2.1:47800: too many wrong PINs: pairing mode closed; open it again",
      }),
    ).toBe("Zu viele falsche PINs. Am Host die Kopplung neu öffnen.");
    expect(errorText({ code: "not-paired", message: "…" })).toBe(
      "Der Host kennt dieses Gerät nicht mehr. Bitte neu koppeln.",
    );
    // No words of its own: the message.
    expect(errorText({ code: "other", message: "starting fernsicht-client: not found" })).toBe(
      "starting fernsicht-client: not found",
    );
    // Not a failure: as it is.
    expect(errorText({ code: "unknown-code", message: "x" })).toBe("[object Object]");
    expect(errorText(new Error("something else"))).toBe("something else");
    expect(errorText("plain")).toBe("plain");
  });
});
