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
- Each address gets **3 attempts** per pairing window, all together 10; then the host closes
  pairing. A message that cannot be a pairing message costs no attempt. One machine therefore
  cannot close pairing for the others, and guessing needs a full exchange per try, which a forged
  address cannot complete.

## Sessions: Noise IK

Each connection starts with a **Noise IK** handshake (`Noise_IK_25519_ChaChaPoly_BLAKE2s`, the
pattern WireGuard uses):

- The client knows the host's key from pairing and sends its own key encrypted in the first message,
  so the host checks it against its paired list before answering. Unknown clients get a Reject.
- One round trip; both sides end up with fresh keys (forward secrecy). The payloads of both messages
  are encrypted too: the Hello with the client's wall clock and the HelloAck. A handshake not newer
  than the last one accepted from that device is a replay and is refused; the times are kept with
  the paired list, so a restart does not forget them (pairing a device again resets its time).
- The host sends nothing to a new session until the client's first sealed packet proves it holds the
  keys. A handshake replayed from a forged address therefore cannot point a video stream at someone.

## Sealed packets

The client believes only what proves to come from the host: a handshake answer that does not
verify is ignored (the real one can still come), a rejection counts only while it asks for a
session, and only sealed packets that open keep a session alive.

After the handshake every packet travels as a Sealed packet: ChaCha20-Poly1305 with an explicit 64-bit
counter (UDP loses and reorders, so the nonce cannot be implicit). A **replay window** of 2,048 packets
accepts each counter once, tolerates reordering within the window and rejects anything older, as
WireGuard and IPsec do. Packets that fail to open are dropped silently.

## The host

- The host service runs as root, because KMS capture needs `CAP_SYS_ADMIN` and input injection needs
  `/dev/uinput`. Its unit keeps only the capabilities it uses (`CAP_SYS_ADMIN`, `CAP_DAC_OVERRIDE`,
  `CAP_DAC_READ_SEARCH`, `CAP_SYS_NICE`, `CAP_NET_ADMIN`), mounts the system read-only except its
  state and runtime directories, and forbids loading kernel modules, new privileges, namespaces and
  set-uid files (`packaging/fernsicht-host-agent.service`). A small privileged helper for capture only
  is a possible next step ([next steps](next-steps.md)).
- The control socket (`/run/fernsicht/control.sock`) is local only; pairing and unpairing go through
  it. It answers root, the service's own user and the user of the active session on the machine's
  seat, as logind reports it (`sd_seat_get_active`): another local or SSH user cannot open pairing.
  The same user's sound and monitor layout are the ones streamed.
- `fernsicht-host-agent unpair <name>` or the app forgets a device; a session it has running ends at
  once, it is turned away from then on, and the app says why. `fernsicht-host-agent disconnect` ends
  the running session, whoever it is.

## The web viewer

- A browser is not paired: the PIN opens one session, once, within 5 minutes, with the same attempt
  limit as pairing. Using it in a browser closes pairing for devices, and a device pairing that
  began with that PIN is refused (it must finish in the pairing window it began in). Someone who
  reads the PIN off the network therefore cannot pair a device with it; only an attacker who can
  intercept and hold back the browser's request could use it first, which HTTPS for the viewer
  would close ([next steps](next-steps.md)).
- Picture, sound and input then travel over WebRTC, which encrypts with DTLS-SRTP.
- The API answers only requests addressed to this machine (its IP address or its name; a page
  elsewhere whose name was pointed at the host, DNS rebinding, is refused), and `/api/connect` only
  JSON from the page's own origin (another website cannot spend the PIN attempts). Every answer
  carries a content security policy (the page's own scripts only), no framing, no sniffing.
- The host's small web server takes one request per connection with everything bounded: a request
  head of at most 8 KiB and 32 headers, a body of at most 64 KiB, 10 s to send it, 32 connections
  at once. A slow or oversized request cannot tie up the root service.
- **The page and the PIN travel over plain HTTP.** That is acceptable in your own LAN, not over the
  internet. Use a VPN for remote access ([remote access](remote-access.md)).
