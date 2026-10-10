import { describe, expect, it } from "vitest";

import { linuxKeyCode } from "./keys";

describe("keys", () => {
  it.each([
    ["KeyA", 30],
    ["KeyQ", 16],
    ["KeyZ", 44],
    ["KeyM", 50],
    ["KeyP", 25],
    ["KeyL", 38],
    ["Digit1", 2],
    ["Digit9", 10],
    ["Digit0", 11],
    ["F1", 59],
    ["F10", 68],
    ["F11", 87],
    ["F12", 88],
    ["F13", 183],
    ["F24", 194],
    ["Escape", 1],
    ["Enter", 28],
    ["Space", 57],
    ["ShiftLeft", 42],
    ["AltRight", 100],
    ["MetaLeft", 125],
    ["ArrowUp", 103],
    ["NumpadEnter", 96],
    ["IntlBackslash", 86],
  ])("%s is KEY code %i", (code, linux) => {
    expect(linuxKeyCode(code)).toBe(linux);
  });

  it("knows nothing of unknown keys", () => {
    for (const code of ["", "Fn", "F25", "F0", "KeyÄ", "Lang1", "toString"]) {
      expect(linuxKeyCode(code)).toBeUndefined();
    }
  });
});
