import { screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { renderApp } from "../test-utils";

describe("Session", () => {
  it("shows the device and live latency stats", async () => {
    await renderApp("/session/214776390?mode=desktop");
    expect(await screen.findByText("zentrale")).toBeInTheDocument();
    expect(await screen.findByRole("region", { name: "Latenz" })).toBeInTheDocument();
    expect(screen.getByText("AV1 · VAAPI")).toBeInTheDocument();
  });

  it("falls back to the formatted id for unknown devices", async () => {
    await renderApp("/session/123456789");
    expect(await screen.findByText("123 456 789")).toBeInTheDocument();
  });

  it("defaults to desktop mode and switches via the URL", async () => {
    const { router, user } = await renderApp("/session/214776390?mode=bogus");
    expect(screen.getByRole("button", { name: "Desktop" })).toHaveAttribute("aria-pressed", "true");
    await user.click(screen.getByRole("button", { name: "Gaming" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ mode: "gaming" }));
    expect(screen.getByRole("button", { name: "Gaming" })).toHaveAttribute("aria-pressed", "true");
  });

  it("returns to the device list on disconnect", async () => {
    const { router, user } = await renderApp("/session/214776390");
    await user.click(screen.getByRole("button", { name: "Trennen" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/devices"));
  });
});
