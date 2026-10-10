import { describe, expect, it } from "vitest";

import { BACKSPACE, diffText, guessLayout, tapKey, typeText } from "./textkeys";

const keys = (events: Record<string, unknown>[]) =>
  events.map((e) => `${e.p ? "+" : "-"}${e.c as number}`).join(" ");

describe("typeText", () => {
  it("types letters, capitals and digits", () => {
    expect(keys(typeText("aB1", "us"))).toBe("+30 -30 +42 +48 -48 -42 +2 -2");
  });

  it("swaps Y and Z for a German host", () => {
    expect(keys(typeText("yz", "de"))).toBe("+44 -44 +21 -21");
    expect(keys(typeText("yz", "us"))).toBe("+21 -21 +44 -44");
  });

  it("knows German umlauts, ß and AltGr characters", () => {
    expect(keys(typeText("ä", "de"))).toBe("+40 -40");
    expect(keys(typeText("Ü", "de"))).toBe("+42 +26 -26 -42");
    expect(keys(typeText("ß", "de"))).toBe("+12 -12");
    expect(keys(typeText("@", "de"))).toBe("+100 +16 -16 -100");
    expect(keys(typeText("@", "us"))).toBe("+42 +3 -3 -42");
    expect(keys(typeText("|", "de"))).toBe("+100 +86 -86 -100");
  });

  it("types space, newline and tab, and leaves out what the layout lacks", () => {
    expect(keys(typeText(" \n\t", "us"))).toBe("+57 -57 +28 -28 +15 -15");
    expect(typeText("ä😀", "us")).toEqual([]);
  });
});

describe("diffText", () => {
  it("finds what was typed and deleted", () => {
    expect(diffText("", "hal")).toEqual({ backspaces: 0, text: "hal" });
    expect(diffText("hal", "hallo")).toEqual({ backspaces: 0, text: "lo" });
    expect(diffText("hallo", "hal")).toEqual({ backspaces: 2, text: "" });
    // A suggestion replaced the word.
    expect(diffText("teh", "the ")).toEqual({ backspaces: 2, text: "he " });
    expect(diffText("aä", "aäö")).toEqual({ backspaces: 0, text: "ö" });
  });
});

describe("helpers", () => {
  it("taps a key and guesses the layout from the language", () => {
    expect(tapKey(BACKSPACE)).toEqual([
      { t: "k", c: 14, p: true },
      { t: "k", c: 14, p: false },
    ]);
    expect(guessLayout(["de-DE", "en"])).toBe("de");
    expect(guessLayout(["en-US"])).toBe("us");
    expect(guessLayout([])).toBe("us");
    // Browsers (and test stubs) that report no usable language.
    expect(guessLayout([undefined, null])).toBe("us");
  });
});
