import { describe, expect, it } from "vitest";

import { deviceIdDigits, isDeviceId, isPin, sessionSearch } from "./session";

describe("session helpers", () => {
  it("reads the mode from a search, desktop unless gaming", () => {
    expect(sessionSearch({ mode: "gaming" })).toEqual({ mode: "gaming" });
    expect(sessionSearch({ mode: "nonsense" })).toEqual({ mode: "desktop" });
    expect(sessionSearch({})).toEqual({ mode: "desktop" });
  });

  it("tells whole PINs and device IDs", () => {
    expect(isPin("123456")).toBe(true);
    expect(isPin("12345")).toBe(false);
    expect(isPin("12345a")).toBe(false);
    expect(deviceIdDigits("482 913 057")).toBe("482913057");
    expect(isDeviceId("482 913 057")).toBe(true);
    expect(isDeviceId("482 913")).toBe(false);
  });
});
