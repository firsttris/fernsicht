# Security

Only devices you paired can connect, and everything they exchange is encrypted and authenticated. The
code is in the `secure` crate.

## Keys

Every device has a long-term key pair (X25519), created on first start and kept in its state
directory (`~/.config/fernsicht`, for the host service `/var/lib/fernsicht`). Devices are shown by a
fingerprint of their public key (e.g. `3f2a-…`). Each side keeps the list of devices it is paired
with.

## Pairing with a PIN

Pairing happens once per pair of devices:

```text
client                               host (pairing open, PIN shown)
  1  spake A, name           ─────►
                             ◄─────  2  spake B, seal(host key, name)
  3  seal(client key)        ─────►     host stores the client
                             ◄─────  4  seal("ok")
  client stores the host
```

- The host shows a 6-digit PIN for 5 minutes (`fernsicht-host-agent pair` or “Gerät koppeln” in the
  app).
- Both sides run **SPAKE2** with the PIN and get the same key only if the PIN matched. Someone who
  records the exchange learns nothing to test PINs against offline; an active attacker gets one guess
  per attempt.
- With that key they swap their long-term public keys and names, sealed with ChaCha20-Poly1305 and
  bound to the SPAKE2 messages.
- After **3 wrong attempts** the host closes pairing.

## Sessions: Noise IK

Each connection starts with a **Noise IK** handshake (`Noise_IK_25519_ChaChaPoly_BLAKE2s`, the
pattern WireGuard uses):

- The client knows the host's key from pairing and sends its own key encrypted in the first message,
  so the host checks it against its paired list before answering. Unknown clients get a Reject.
- One round trip; both sides end up with fresh keys (forward secrecy). The payloads of both messages
  are encrypted too: the Hello with the client's wall clock (a replayed recording of an old handshake
  is refused) and the HelloAck.

## Sealed packets

After the handshake every packet travels as a Sealed packet: ChaCha20-Poly1305 with an explicit 64-bit
counter (UDP loses and reorders, so the nonce cannot be implicit). A **replay window** of 2,048 packets
accepts each counter once, tolerates reordering within the window and rejects anything older, as
WireGuard and IPsec do. Packets that fail to open are dropped silently.

## The host

- The host service runs as root, because KMS capture needs `CAP_SYS_ADMIN` and input injection needs
  `/dev/uinput`. A small privileged helper for capture only is a possible next step
  ([next steps](next-steps.md)).
- The control socket (`/run/fernsicht/control.sock`) is local only; pairing and unpairing go through
  it.
- `fernsicht-host-agent unpair <name>` or the app forgets a device; it is turned away from then on and
  the app says why.

## The web viewer

- A browser is not paired: the PIN opens one session, once, within 5 minutes, with the same attempt
  limit as pairing.
- Picture, sound and input then travel over WebRTC, which encrypts with DTLS-SRTP.
- **The page and the PIN travel over plain HTTP.** That is acceptable in your own LAN, not over the
  internet. Use a VPN for remote access ([remote access](remote-access.md)).
