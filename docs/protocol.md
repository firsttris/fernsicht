# Protocol

Host and client talk over one UDP port (47800 by default) with their own compact protocol. It is
built for low latency: no retransmission of video (FEC repairs it instead), small fixed headers,
little-endian integers, and a parser that never panics and never allocates. The browser uses WebRTC
instead (see [web viewer](web-viewer.md)).

## Packets

Every datagram starts with a 4-byte prefix: magic `0xF5`, version `1`, kind, flags. Datagrams are at
most 1,400 bytes; once a session is secure, every packet travels inside a **Sealed** packet
([security](security.md)).

| Kind | Direction | |
|---|---|---|
| Video (1) | host → client | one shard of an encoded frame: session, frame ID, frame length, capture time and stage offsets, FEC group position, keyframe flag |
| Feedback (2) | client → host | every 100 ms: frames completed and dropped, packets received, lost and recovered; asks for a keyframe or the pointer image |
| ClockPing (3) / ClockPong (4) | both | clock sync: the client's send time, the host's receive and send times |
| Hello (5) / HelloAck (6) | client → host / back | the session request (size, fps, bitrate, the codecs the client decodes) and the answer (session ID, size, fps, codec) |
| Bye (7) | both | the session ends |
| Cursor (8) / CursorShape (9) | host → client | pointer position and visibility per frame; the pointer image in 1 KiB pieces when it changes |
| Input (10) / InputAck (11) | client → host / back | mouse, keyboard and gamepad events with sequence numbers; the host's acknowledgement |
| Audio (12) | host → client | an Opus frame and, again, the frame before it |
| Handshake (13) | both | a Noise IK handshake message |
| Sealed (14) | both | any packet above, encrypted and authenticated, with an explicit counter |
| Pair (15) / Reject (16) | both | the four steps of pairing with a PIN; why a client is turned away |
| Discover (17) / Announce (18) | client → broadcast / host → client | device discovery in the LAN |
| Monitors (19) / SelectMonitor (20) | host → client / back | the host's monitors and which is shown; the client's choice |

Limits are enforced while parsing: frames at most 8 MiB, at most 1,024 shards per FEC group, pointer
images at most 256×256, at most 64 input events per packet, at most 8 monitors. The format is covered
by property tests and a fuzz target ([testing](testing.md)).

## Video: shards, FEC and pacing

- An encoded frame is cut into shards of equal size and grouped (at most 512 data shards per group).
- Each group gets **Reed-Solomon recovery shards** (`reed-solomon-simd`), as many as needed for the
  loss rate the client reports, so that a group fails with a probability of at most 10⁻⁵ (sized
  binomially). The estimate never drops below 1 % loss; recovery is capped at 50 % of a group, with
  at least 3 recovery shards allowed for tiny frames.
- The **pacer** spreads a frame's datagrams over time: at link rate, but never later than an even
  spread over the frame's budget, so switch and Wi-Fi buffers do not overflow.
- The client reassembles frames in a few slots, repairs them with FEC and hands complete frames to the
  decoder. A gap in the frame IDs (a frame lost despite FEC, or a full queue) means the decoder's
  reference chain is broken: it waits for a keyframe and asks for one.
- **Bitrate adaptation:** the host lowers the bitrate quickly when frames are lost despite FEC or its
  own sender falls behind, and raises it carefully after 5 s of clean network; never above what the
  client asked for. Random Wi-Fi loss is FEC's job and does not cost sharpness.

The video header also carries a slice index and count, reserved for sending parts of a frame before it
is encoded completely ([next steps](next-steps.md)).

## Clock sync

NTP-style: the client sends ClockPing with its time `t0`, the host answers with its receive time `t1`
and send time `t2`, the client notes arrival `t3`. The offset is estimated from the probe with the
smallest round-trip time among the last 16. Pings go fast while the offset settles, then slowly to
follow drift. With the offset, the host's timestamps in every video packet give the latency of each
stage on the client.

## Input

Input must arrive exactly once and in order, but must not wait behind lost packets. Every Input packet
carries all events the host has not acknowledged yet, oldest first, each with its sequence number; the
host applies each sequence number once and acknowledges the highest. A lost packet is covered by the
next one. Tested with 30 % loss. The host injects the events through `uinput` (a virtual mouse,
keyboard and up to four Xbox 360 pads) and maps absolute positions onto the monitor shown.

## Sound

Opus, 48 kHz stereo, 5 ms frames in low-delay mode. Every audio packet also carries the previous frame,
so a single loss leaves no gap. The client's jitter buffer holds about 15 ms, conceals longer gaps and
compensates the drift between the two sound cards' clocks.

## Monitors

The host announces its active monitors (connector name and size) and the one shown when the session
starts, after each switch and every 2 s (a lost list heals). The client answers with SelectMonitor; the
host's capture thread opens the other monitor on the same card, sends a keyframe and maps input onto
the new monitor. A browser gets the list as JSON on its data channel.

## Discovery

The client broadcasts Discover (padded to 256 bytes, longer than any answer, so the host can never be
used to amplify traffic) on the stream port; hosts answer with Announce: a nonce, their public key,
name, OS, GPU and codecs, and whether pairing is open or a session is running. Paired hosts are also
asked directly at their last known address.

## Sessions

1. The client sends a Handshake (Noise IK) whose payload is its Hello plus its wall clock time (a
   replayed recording is older than the last accepted one).
2. The host checks the client's key against its paired list, picks the codec and answers with the
   second handshake message carrying the HelloAck.
3. From then on everything is sealed. Without security (tests, `--state-dir` with no keys) Hello and
   HelloAck are sent plain.

A new client replaces a running session; a session ends with Bye or after 5 s of silence.
