import { cleanup, fireEvent, render, screen } from "@testing-library/react";
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

  it("sends key combinations from the menu", async () => {
    const onSendKeys = vi.fn();
    const { user } = setup({ onSendKeys });
    const button = screen.getByRole("button", { name: "Tasten senden" });
    expect(button).toHaveAttribute("aria-expanded", "false");
    await user.click(button);
    await user.click(screen.getByRole("menuitem", { name: "Strg+Alt+Entf" }));
    expect(onSendKeys).toHaveBeenCalledWith([29, 56, 111]);
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    // Esc and a click elsewhere close it without sending.
    await user.click(button);
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    await user.click(button);
    await user.click(screen.getByText("zentrale"));
    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    expect(onSendKeys).toHaveBeenCalledOnce();
  });

  it("switches between the host's monitors", async () => {
    const onSelectMonitor = vi.fn();
    const monitors = {
      current: 0,
      list: [
        { name: "DP-2", width: 2560, height: 1440 },
        { name: "DP-1", width: 1920, height: 1080 },
      ],
    };
    const { user } = setup({ monitors, onSelectMonitor });
    await user.click(screen.getByRole("button", { name: "Bildschirm wählen" }));
    expect(
      screen.getByRole("menuitemradio", { name: /Bildschirm 1 · DP-2 · 2560×1440/ }),
    ).toHaveAttribute("aria-checked", "true");
    await user.click(screen.getByRole("menuitemradio", { name: /Bildschirm 2 · DP-1/ }));
    expect(onSelectMonitor).toHaveBeenCalledWith(1);
  });

  it("offers no monitor menu for a single monitor", () => {
    setup({
      monitors: { current: 0, list: [{ name: "DP-1", width: 1920, height: 1080 }] },
      onSelectMonitor: vi.fn(),
    });
    expect(screen.queryByRole("button", { name: "Bildschirm wählen" })).not.toBeInTheDocument();
  });

  it("has no keys menu or fullscreen button without a handler", () => {
    setup();
    expect(screen.queryByRole("button", { name: "Tasten senden" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Vollbild" })).not.toBeInTheDocument();
  });

  it("asks for fullscreen and back", async () => {
    const onFullscreenChange = vi.fn();
    const { user } = setup({ onFullscreenChange });
    await user.click(screen.getByRole("button", { name: "Vollbild" }));
    expect(onFullscreenChange).toHaveBeenLastCalledWith(true);
    cleanup();
    setup({ onFullscreenChange, fullscreen: true });
    await user.click(screen.getByRole("button", { name: "Vollbild verlassen" }));
    expect(onFullscreenChange).toHaveBeenLastCalledWith(false);
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
    setup({
      monitors: {
        current: 0,
        list: [
          { name: "DP-2", width: 2560, height: 1440 },
          { name: "DP-1", width: 2560, height: 1440 },
        ],
      },
      onSelectMonitor: vi.fn(),
      onSendKeys: vi.fn(),
      onFullscreenChange: vi.fn(),
    });
    for (const name of [
      "Bildschirm wählen",
      "Tasten senden",
      "Zwischenablage",
      "Dateien senden",
      "Ton ausschalten",
      "Vollbild",
      "Einstellungen",
    ]) {
      expect(screen.getByRole("button", { name })).toBeInTheDocument();
    }
  });
});
