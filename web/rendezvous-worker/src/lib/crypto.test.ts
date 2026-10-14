// @vitest-environment node
import { describe, expect, it } from "vitest";

import {
  hex,
  isRendezvousId,
  makeChallenge,
  proof,
  rendezvousId,
  unhex,
  verifyProof,
} from "./crypto";

const raw = async (k: CryptoKey) =>
  new Uint8Array((await crypto.subtle.exportKey("raw", k)) as ArrayBuffer);

/** The host's side, as crates/internet/src/signal.rs does it. */
async function hostProof(host: CryptoKeyPair, ephemeral: string, nonce: string) {
  const theirs = await crypto.subtle.importKey(
    "raw",
    unhex(ephemeral, 32)!,
    { name: "X25519" },
    false,
    [],
  );
  const shared = new Uint8Array(
    await crypto.subtle.deriveBits(
      { name: "X25519", public: theirs } as never,
      host.privateKey,
      256,
    ),
  );
  return hex(await proof(shared, unhex(nonce, 16)!, await raw(host.publicKey)));
}

describe("rendezvous IDs", () => {
  it("are SHA-256 of the context and the key, as the Rust side computes them", async () => {
    const id = await rendezvousId(new Uint8Array(32).fill(1));
    expect(id).toBe("c07924470b2c9f33dd7bd2b8ad14d0cf330f0dab5d29343eb7d8b893474c88a8");
    expect(isRendezvousId(id)).toBe(true);
    expect(isRendezvousId(id.toUpperCase())).toBe(false);
    expect(isRendezvousId("ab")).toBe(false);
  });

  it("parse hex strictly", () => {
    expect(unhex("00ff", 2)).toEqual(new Uint8Array([0, 255]));
    expect(unhex("00ff", 3)).toBeNull();
    expect(unhex("zz", 1)).toBeNull();
    expect(unhex("FF", 1)).toBeNull();
  });
});

describe("the key proof", () => {
  it("matches the independent vector (host 0x11…, ephemeral 0x22…, nonce 0x33…)", async () => {
    // The ephemeral key as a JWK: d is the private key, x the public one.
    const secret: JsonWebKey = {
      kty: "OKP",
      crv: "X25519",
      d: "IiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiIiI",
      x: "D6poTtKIZ7l_Smot7l34zpdOdrcBjj8iocTPJnhXDyA",
    };
    const hostKey = unhex("7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13", 32)!;
    const mac = "6260281feb9f567b4d0162b4400352079e3d9fb5db3e0205bc45742d6e999901";
    const nonce = "33".repeat(16);
    expect(await verifyProof(secret, nonce, hostKey, mac)).toBe(true);
    expect(await verifyProof(secret, nonce, hostKey, `00${mac.slice(2)}`)).toBe(false);
    expect(await verifyProof(secret, "44".repeat(16), hostKey, mac)).toBe(false);
  });

  it("is given by the key's holder only", async () => {
    const host = (await crypto.subtle.generateKey({ name: "X25519" }, true, [
      "deriveBits",
    ])) as CryptoKeyPair;
    const other = (await crypto.subtle.generateKey({ name: "X25519" }, true, [
      "deriveBits",
    ])) as CryptoKeyPair;
    const c = await makeChallenge();
    const key = await raw(host.publicKey);
    expect(
      await verifyProof(c.secret, c.nonce, key, await hostProof(host, c.ephemeral, c.nonce)),
    ).toBe(true);
    expect(
      await verifyProof(c.secret, c.nonce, key, await hostProof(other, c.ephemeral, c.nonce)),
    ).toBe(false);
    expect(await verifyProof(c.secret, c.nonce, new Uint8Array(32), "00".repeat(32))).toBe(false);
    expect(await verifyProof(c.secret, c.nonce, key, "not hex")).toBe(false);
  });
});
