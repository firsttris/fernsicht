import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useFullscreen } from "./fullscreen";

function fakeFullscreen() {
  let element: Element | null = null;
  Object.defineProperty(document, "fullscreenElement", {
    configurable: true,
    get: () => element,
  });
  const change = (el: Element | null) => {
    element = el;
    document.dispatchEvent(new Event("fullscreenchange"));
  };
  document.documentElement.requestFullscreen = vi.fn(() => {
    change(document.documentElement);
    return Promise.resolve();
  });
  document.exitFullscreen = vi.fn(() => {
    change(null);
    return Promise.resolve();
  });
  const keyboard = { lock: vi.fn(() => Promise.resolve()), unlock: vi.fn() };
  Object.defineProperty(navigator, "keyboard", { configurable: true, value: keyboard });
  return keyboard;
}

afterEach(() => {
  Reflect.deleteProperty(document, "fullscreenElement");
  Reflect.deleteProperty(navigator, "keyboard");
});

describe("useFullscreen", () => {
  it("locks the keyboard in fullscreen and lets go after", async () => {
    const keyboard = fakeFullscreen();
    const { result } = renderHook(() => useFullscreen());
    expect(result.current[0]).toBe(false);
    await act(async () => result.current[1](true));
    expect(result.current[0]).toBe(true);
    expect(keyboard.lock).toHaveBeenCalledOnce();
    await act(async () => result.current[1](false));
    expect(result.current[0]).toBe(false);
    expect(keyboard.unlock).toHaveBeenCalled();
  });

  it("works without Keyboard Lock (Firefox) and survives a refusal", async () => {
    const keyboard = fakeFullscreen();
    Reflect.deleteProperty(navigator, "keyboard");
    const { result } = renderHook(() => useFullscreen());
    await act(async () => result.current[1](true));
    expect(result.current[0]).toBe(true);
    Object.defineProperty(navigator, "keyboard", {
      configurable: true,
      value: { lock: vi.fn(() => Promise.reject(new Error("no"))), unlock: vi.fn() },
    });
    await act(async () => result.current[1](false));
    await act(async () => result.current[1](true));
    expect(result.current[0]).toBe(true);
    expect(keyboard.lock).not.toHaveBeenCalled();
  });
});
