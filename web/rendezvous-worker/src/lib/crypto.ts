/**
 * The host's identity at the service: its rendezvous ID, and the proof
 * that it holds its key (X25519 agreement with an ephemeral key of ours,
 * HMAC-SHA256 over the nonce and its key). Mirrors
 * crates/internet/src/signal.rs.
 */

const encoder = new TextEncoder();
const ID_CONTEXT = encoder.encode("fernsicht rendezvous v1");
const PROOF_CONTEXT = encoder.encode("fernsicht rendezvous proof v1");

export const hex = (bytes: Uint8Array): string =>
  Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

/** Bytes from lower-case hex of exactly `length` bytes, or `null`. */
export function unhex(s: string, length: number): Uint8Array<ArrayBuffer> | null {
  if (s.length !== length * 2 || !/^[0-9a-f]*$/.test(s)) return null;
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) out[i] = Number.parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

export const isRendezvousId = (s: string): boolean => /^[0-9a-f]{64}$/.test(s);

function concat(...parts: Uint8Array[]): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** SHA-256 of the context and the host's public key, in hex. */
export async function rendezvousId(key: Uint8Array): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", concat(ID_CONTEXT, key))));
}

/** A challenge for a host: our ephemeral public key and a nonce (hex),
 * and the secret half to check the proof with later. */
export interface Challenge {
  ephemeral: string;
  nonce: string;
  secret: JsonWebKey;
}

export async function makeChallenge(): Promise<Challenge> {
  const pair = (await crypto.subtle.generateKey({ name: "X25519" }, true, [
    "deriveBits",
  ])) as CryptoKeyPair;
  const raw = new Uint8Array((await crypto.subtle.exportKey("raw", pair.publicKey)) as ArrayBuffer);
  const secret = (await crypto.subtle.exportKey("jwk", pair.privateKey)) as JsonWebKey;
  const nonce = crypto.getRandomValues(new Uint8Array(16));
  return { ephemeral: hex(raw), nonce: hex(nonce), secret };
}

/** The proof a host holding `key` gives for `shared` and `nonce`. */
export async function proof(
  shared: Uint8Array<ArrayBuffer>,
  nonce: Uint8Array,
  key: Uint8Array,
): Promise<Uint8Array> {
  const mac = await crypto.subtle.importKey(
    "raw",
    shared,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  return new Uint8Array(await crypto.subtle.sign("HMAC", mac, concat(PROOF_CONTEXT, nonce, key)));
}

/** Whether `mac` (hex) proves the host holds `key` (its public key, 32
 * bytes), for the challenge whose secret and nonce are given. */
export async function verifyProof(
  secret: JsonWebKey,
  nonceHex: string,
  key: Uint8Array<ArrayBuffer>,
  macHex: string,
): Promise<boolean> {
  const mac = unhex(macHex, 32);
  const nonce = unhex(nonceHex, 16);
  if (!mac || !nonce) return false;
  try {
    const ours = await crypto.subtle.importKey("jwk", secret, { name: "X25519" }, false, [
      "deriveBits",
    ]);
    const theirs = await crypto.subtle.importKey("raw", key, { name: "X25519" }, false, []);
    // The Web Crypto field is `public`; workers-types spells it `$public`
    // (a reserved word in its generator), the runtime reads `public`.
    const algorithm = {
      name: "X25519",
      public: theirs,
    } as unknown as SubtleCryptoDeriveKeyAlgorithm;
    const shared = new Uint8Array(await crypto.subtle.deriveBits(algorithm, ours, 256));
    // A low-order key makes the agreement predictable.
    if (shared.every((b) => b === 0)) return false;
    const expected = await proof(shared, nonce, key);
    return expected.reduce((acc, b, i) => acc | (b ^ (mac[i] ?? 0)), 0) === 0;
  } catch {
    // WebCrypto refuses some low-order keys outright.
    return false;
  }
}
