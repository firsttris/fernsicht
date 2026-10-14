import { describe, expect, it } from "vitest";

import { take } from "./rate";

describe("the rate limit", () => {
  it("allows so many a minute", () => {
    let hits: number[] = [];
    for (let i = 0; i < 3; i++) {
      const r = take(hits, 1000, 3);
      expect(r.allowed).toBe(true);
      hits = r.hits;
    }
    expect(take(hits, 1000, 3).allowed).toBe(false);
    const later = take(hits, 61_000, 3);
    expect(later.allowed).toBe(true);
    expect(later.hits).toEqual([61_000]);
  });
});
