import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { demoStats } from "../demo";
import type { SessionViewProps } from "./session-view";
import { SessionView } from "./session-view";

function setup(props: Partial<SessionViewProps> = {}) {
  const onModeChange = vi.fn();
  const onDisconnect = vi.fn();
  render(
    <SessionView
      session={{ deviceName: "zentrale", width: 2560, height: 1440, path: "P2P", encrypted: true }}
      stats={demoStats(1)}
      mode="desktop"
      onModeChange={onModeChange}
      onDisconnect={onDisconnect}
      {...props}
    />,
  );
  return { onModeChange, onDisconnect, user: userEvent.setup() };
}

describe("SessionView", () => {
  it("shows the device, connection path and a placeholder video", () => {
    setup();
    expect(screen.getByText("zentrale")).toBeInTheDocument();
    expect(screen.getByText("P2P · E2E")).toBeInTheDocument();
    expect(screen.getByText("Videostream von „zentrale“ – 2560 × 1440")).toBeInTheDocument();
  });

  it("omits E2E for unencrypted relay sessions", () => {
    setup({
      session: { deviceName: "x", width: 1, height: 1, path: "Relay", encrypted: false },
    });
    expect(screen.getByText("Relay")).toBeInTheDocument();
    expect(screen.queryByText(/E2E/)).not.toBeInTheDocument();
  });

  it("marks the active mode and reports changes", async () => {
    const { onModeChange, user } = setup();
    expect(screen.getByRole("button", { name: "Desktop" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "Gaming" })).toHaveAttribute("aria-pressed", "false");
    await user.click(screen.getByRole("button", { name: "Gaming" }));
    expect(onModeChange).toHaveBeenCalledWith("gaming");
  });

  it("disconnects", async () => {
    const { onDisconnect, user } = setup();
    await user.click(screen.getByRole("button", { name: "Trennen" }));
    expect(onDisconnect).toHaveBeenCalledOnce();
  });

  it("toggles sound", async () => {
    const { user } = setup();
    await user.click(screen.getByRole("button", { name: "Ton ausschalten" }));
    expect(screen.getByRole("button", { name: "Ton einschalten" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
  });

  it("hides and restores toolbar and overlay with Strg+Alt+F", () => {
    setup();
    expect(screen.getByRole("toolbar")).toBeInTheDocument();
    fireEvent.keyDown(window, { key: "f", ctrlKey: true, altKey: true });
    expect(screen.queryByRole("toolbar")).not.toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Latenz" })).not.toBeInTheDocument();
    fireEvent.keyDown(window, { key: "F", ctrlKey: true, altKey: true });
    expect(screen.getByRole("toolbar")).toBeInTheDocument();
  });

  it("ignores F without both modifiers", () => {
    setup();
    fireEvent.keyDown(window, { key: "f", ctrlKey: true });
    fireEvent.keyDown(window, { key: "f" });
    expect(screen.getByRole("toolbar")).toBeInTheDocument();
  });

  it("shows no overlay until stats arrive, and renders a custom video surface", () => {
    setup({ stats: undefined, children: <video aria-label="Stream" /> });
    expect(screen.queryByRole("region", { name: "Latenz" })).not.toBeInTheDocument();
    expect(screen.getByLabelText("Stream")).toBeInTheDocument();
    expect(screen.queryByText(/Videostream von/)).not.toBeInTheDocument();
  });

  it("labels every icon-only button", () => {
    setup();
    for (const name of ["Bildschirm wählen", "Zwischenablage", "Dateien senden", "Einstellungen"]) {
      expect(screen.getByRole("button", { name })).toBeInTheDocument();
    }
  });
});
