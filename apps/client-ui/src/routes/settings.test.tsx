import { screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { DEFAULT_SETTINGS, loadSettings, saveSettings } from "../lib/api";
import { renderApp } from "../test-utils";

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

describe("Einstellungen", () => {
  it("starts with the host's size, 60 fps and automatic bitrate", async () => {
    await renderApp("/settings");
    expect(await screen.findByRole("radio", { name: "Wie der Host" })).toBeChecked();
    expect(screen.getByRole("radio", { name: "60 fps" })).toBeChecked();
    expect(screen.getByRole("radio", { name: "Automatisch" })).toBeChecked();
  });

  it("keeps choices and resets them", async () => {
    const { user } = await renderApp("/settings");
    await user.click(await screen.findByRole("radio", { name: "720p" }));
    await user.click(screen.getByRole("radio", { name: "144 fps" }));
    await user.click(screen.getByRole("radio", { name: "50 Mbit/s" }));
    expect(loadSettings()).toEqual({ width: 1280, height: 720, fps: 144, bitrateMbit: 50 });
    await user.click(screen.getByRole("radio", { name: "Wie der Host" }));
    expect(loadSettings()).toMatchObject({ width: 0, height: 0 });
    await user.click(screen.getByRole("button", { name: "Zurücksetzen" }));
    expect(loadSettings()).toEqual(DEFAULT_SETTINGS);
    expect(screen.getByRole("radio", { name: "60 fps" })).toBeChecked();
  });

  it("survives broken or unavailable storage", () => {
    localStorage.setItem("fernsicht.settings", "{not json");
    expect(loadSettings()).toEqual(DEFAULT_SETTINGS);
    localStorage.setItem("fernsicht.settings", JSON.stringify({ fps: 30, odd: 1, width: 7 }));
    expect(loadSettings()).toMatchObject({ fps: 30, width: 7, height: 0 });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    expect(() => saveSettings(DEFAULT_SETTINGS)).not.toThrow();
  });
});
