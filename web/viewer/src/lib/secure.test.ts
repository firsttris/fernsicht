import { describe, expect, it } from "vitest";

import { secureHint } from "./secure";

const chrome =
  "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";
const edge = `${chrome} Edg/141.0.0.0`;
const firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:144.0) Gecko/20100101 Firefox/144.0";
const iosChrome =
  "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/141.0 Mobile/15E148 Safari/604.1";
const android =
  "Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36";
const origin = "http://192.168.178.87:47800";

describe("secureHint", () => {
  it("tells Chrome and Edge which flag lists the host", () => {
    expect(secureHint({ secure: false, origin, userAgent: chrome })).toEqual({
      flag: "chrome://flags/#unsafely-treat-insecure-origin-as-secure",
      origin,
    });
    expect(secureHint({ secure: false, origin, userAgent: edge })?.flag).toBe(
      "edge://flags/#unsafely-treat-insecure-origin-as-secure",
    );
  });

  it("says nothing where the page is secure or no flag helps", () => {
    expect(secureHint({ secure: true, origin, userAgent: chrome })).toBeNull();
    expect(secureHint({ secure: false, origin, userAgent: firefox })).toBeNull();
    expect(secureHint({ secure: false, origin, userAgent: iosChrome })).toBeNull();
    expect(secureHint({ secure: false, origin, userAgent: android })).toBeNull();
  });
});
