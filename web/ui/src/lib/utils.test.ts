import { describe, expect, it } from "vitest";

import { cn, formatDeviceId, formatMs, formatPercent } from "./utils";

describe("formatMs", () => {
  it("rounds to whole milliseconds by default", () => {
    expect(formatMs(14_000)).toBe("14 ms");
    expect(formatMs(3_600)).toBe("4 ms");
    expect(formatMs(0)).toBe("0 ms");
  });

  it("uses a German decimal comma", () => {
    expect(formatMs(3_449, 1)).toBe("3,4 ms");
    expect(formatMs(3_450, 1)).toBe("3,5 ms");
  });
});

describe("formatPercent", () => {
  it("prints exact zero (and below) as 0", () => {
    expect(formatPercent(0)).toBe("0");
    expect(formatPercent(-0.1)).toBe("0");
  });

  it("prints one decimal with a comma", () => {
    expect(formatPercent(0.004)).toBe("0,4 %");
    expect(formatPercent(0.125)).toBe("12,5 %");
  });
});

describe("formatDeviceId", () => {
  it("groups digits by three", () => {
    expect(formatDeviceId("482913057")).toBe("482 913 057");
    expect(formatDeviceId("48291")).toBe("482 91");
    expect(formatDeviceId("")).toBe("");
  });

  it("ignores anything that is not a digit", () => {
    expect(formatDeviceId("482-913 x057")).toBe("482 913 057");
  });
});

describe("cn", () => {
  it("merges conflicting tailwind classes, last wins", () => {
    expect(cn("px-2 text-sm", false, "px-4")).toBe("text-sm px-4");
  });
});
