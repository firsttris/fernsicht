import { describe, expect, it } from "vitest";

import { demoDevices, demoStats, demoThisMachine } from "./demo";
import { STAGES } from "./types";

describe("demo data", () => {
  it("has unique nine-digit device ids", () => {
    const ids = [...demoDevices.map((d) => d.id), demoThisMachine.id];
    for (const id of ids) expect(id).toMatch(/^\d{9}$/);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("produces plausible stats at any time", () => {
    for (const t of [0, 1.5, 1000, 1e9]) {
      const s = demoStats(t);
      for (const { key } of STAGES) expect(s.stagesUs[key]).toBeGreaterThan(0);
      expect(s.lossBeforeFec).toBeGreaterThanOrEqual(0);
      expect(s.lossAfterFec).toBe(0);
      expect(s.fps).toBeGreaterThan(100);
    }
  });

  it("is deterministic", () => {
    expect(demoStats(42)).toEqual(demoStats(42));
  });
});
