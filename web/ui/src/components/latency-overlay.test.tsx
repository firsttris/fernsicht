import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import type { SessionStats } from "../types";
import { LatencyOverlay } from "./latency-overlay";

const stats: SessionStats = {
  stagesUs: { capture: 1_000, encode: 4_000, network: 3_000, decode: 2_000, present: 4_000 },
  codec: "AV1 · VAAPI",
  fps: 119.6,
  bitrateBps: 38_400_000,
  lossBeforeFec: 0.004,
  lossAfterFec: 0,
};

describe("LatencyOverlay", () => {
  it("shows glass-to-glass as the sum of all stages", () => {
    render(<LatencyOverlay stats={stats} />);
    const panel = screen.getByRole("region", { name: "Latenz" });
    expect(within(panel).getByText("14 ms")).toBeInTheDocument();
  });

  it("lists every stage with its label and duration", () => {
    render(<LatencyOverlay stats={stats} />);
    for (const [label, value] of [
      ["Capture", "1 ms"],
      ["Encode", "4 ms"],
      ["Netz", "3 ms"],
      ["Decode", "2 ms"],
      ["Anzeige", "4 ms"],
    ] as const) {
      const term = screen.getByText(label);
      expect(term.closest("div")).toHaveTextContent(`${label}${value}`);
    }
  });

  it("shows stream info rounded for humans", () => {
    render(<LatencyOverlay stats={stats} />);
    expect(screen.getByText("AV1 · VAAPI")).toBeInTheDocument();
    expect(screen.getByText("120 fps")).toBeInTheDocument();
    expect(screen.getByText("38 Mbit/s")).toBeInTheDocument();
    expect(screen.getByText("0,4 % → 0")).toBeInTheDocument();
  });

  it("sizes the stage bar proportionally", () => {
    const { container } = render(<LatencyOverlay stats={stats} />);
    const bar = container.querySelector("[aria-hidden].flex.h-1\\.5");
    const grows = [...bar!.children].map((c) => (c as HTMLElement).style.flexGrow);
    expect(grows).toEqual(["1000", "4000", "3000", "2000", "4000"]);
  });
});
