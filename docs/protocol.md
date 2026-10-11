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
| KeyframeRequest (21) | client → host | the client cannot show the stream: the last frame it completed and its round-trip time; sent at once, again while no keyframe comes |

Limits are enforced while parsing: frames at most 8 MiB, at most 1,024 shards per FEC group, pointer
images at most 256×256, at most 64 input events per packet, at most 8 monitors. The format is covered
by property tests and a fuzz target ([testing](testing.md)).

## Video: shards, FEC and pacing

- An encoded frame is cut into shards of equal size. Up to 512 shards (about 600 kB: every P-frame
  and most keyframes) form **one FEC group**, so its recovery shards repair any losses in the
  frame, a burst too. Larger frames are split into balanced groups (sizes differ by one at most).
  At least 10 % recovery shards per group.
- Each group gets **Reed-Solomon recovery shards** (`reed-solomon-simd`), as many as needed for the
  loss rate the client reports, so that a group fails with a probability of at most 10⁻⁵ (sized
  binomially). The estimate never drops below 1 % loss; recovery is capped at 50 % of a group, with
  at least 3 recovery shards allowed for tiny frames.
- A frame's **data shards go first**, then the recovery shards: without loss a frame is complete with
  its last data shard. With several groups, their shards take turns, so that a burst of losses
  spreads over the groups instead of breaking one.
- The **pacer** is a token bucket: up to 64 KiB leave back-to-back (a P-frame usually fits whole and
  goes at line rate), the rest at the pacing rate (`--pace-mbit`, 400 Mbit/s), so that switch and
  Wi-Fi buffers do not overflow on keyframes. The bucket refills between frames. A frame too large
  to leave within one frame interval at that rate goes faster, so the sender never falls behind.
- Datagrams leave in **chunks**: those due within 200 µs of each other, sealed one after the other
  into one buffer and handed to the kernel in one call that it cuts into datagrams (UDP GSO; one by
  one where a route cannot). The send thread sleeps between chunks with a timer slack of 1 µs
  instead of spinning. The client receives a run of datagrams in one call too (UDP GRO), and a
  completed frame goes to the decoder by swapping buffers, not by copying.
- **Marking (DSCP):** video is AF41, everything small and urgent EF: the host's pointer, sound and
  answers, the client's input, feedback and pings. Wi-Fi access points put them into the WMM video
  and voice queues, which get the air first; routers may prioritise them, some ignore or clear the
  marks. The browser's WebRTC socket carries AF41.
- The client reassembles frames in a few slots, repairs them with FEC and hands complete frames to the
  decoder. A gap in the frame IDs (a frame lost despite FEC, or a full queue) means the decoder's
  reference chain is broken: it waits for a keyframe and asks for one.
- **Keyframe requests** go out the moment the client sees the gap (KeyframeRequest), not with the
  next feedback, and again each time a keyframe should have come and did not: after the round trip,
  two frames and 10 ms. The host answers each loss once: a request from a client that has not
  completed a frame since the last keyframe, within that time after it, is answered by that
  keyframe. A client that completed a later frame lost the keyframe, and gets another at once.
  Feedback still carries the keyframe flag (for hosts before KeyframeRequest); the host weighs it the
  same way. Browsers ask with RTCP PLI; the round trip comes from their receiver reports.
- **Bitrate adaptation:** the host lowers the bitrate quickly (×0.7, at most once a second) when its
  own sender falls behind or loss is heavy (over 20 %), gently (×0.85, at most every 2 s) when frames
  are lost despite FEC or loss is over 15 %, and raises it carefully (×1.15 after 5 s without either);
  never above what the client asked for. Random Wi-Fi loss that FEC repairs is not congestion: it
  neither costs sharpness nor keeps the bitrate from coming back. The running encoder takes the new
  rate: NVENC reconfigures its session in place, VAAPI opens a new codec context on the same device,
  frame pool and converter. Either way the next frame is a keyframe; there is no gap in the picture.

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
next one. An event leaves the moment it happens, from the window's own thread (not on the network
loop's next turn); what is not acknowledged goes again after three round trips (4–15 ms). Tested
with 30 % loss. The host injects the events through `uinput` (a virtual mouse,
keyboard and up to four Xbox 360 pads) and maps absolute positions onto the monitor shown.

## Sound

Opus, 48 kHz stereo, 5 ms frames in low-delay mode. Every audio packet also carries the previous frame,
so a single loss leaves no gap. The client's jitter buffer holds about 15 ms and conceals longer
gaps. The two sound cards' clocks drift apart (up to about 100 ppm): instead of dropping a frame or
running dry, the client plays a frame one sample shorter or longer now and then (0.4 % for 5 ms,
inaudible), steered by the smoothed buffer depth.

## Monitors

The host announces its active monitors (connector name and size) and the one shown when the session
starts, after each switch and every 2 s (a lost list heals). The client answers with SelectMonitor; the
host's capture thread opens the other monitor on the same card and maps input onto it. A monitor of
another shape gets its own stream size: the stream is the monitor fitted into the size the client
asked for (or the host's limits), aspect ratio kept, never scaled up. The host rebuilds the encoder
for it, and the new stream starts with a keyframe that carries the size; decoders and the client's
mouse mapping follow the decoded size. A browser gets the list as JSON on its data channel.

## Discovery

The client broadcasts Discover (padded to 256 bytes, longer than any answer, so the host can never be
used to amplify traffic) on the stream port; hosts answer with Announce: a nonce, their public key,
name, OS, GPU and codecs, and whether pairing is open or a session is running. Paired hosts are also
asked directly at their last known address. Anyone can answer in a host's name (its key is public),
so an answer only shows where the host may be and is tried first when connecting; a client stores a
host's new address only after a session with it went through. A host answers each asking address at
most 20 times a second and all of them 200 times, so a flood from one machine cannot silence it for
the others.

## Sessions

1. The client sends a Handshake (Noise IK) whose payload is its Hello plus its wall clock time (a
   replayed recording is older than the last accepted one).
2. The host checks the client's key against its paired list, picks the codec and answers with the
   second handshake message carrying the HelloAck.
3. From then on everything is sealed. The host sends nothing until the client's first sealed packet
   (a clock ping, right away) proves it holds the keys; the stream then starts with a keyframe.
   Without security (tests, `--state-dir` with no keys) Hello and HelloAck are sent plain.

A new client replaces a running session; a session ends with Bye or after 5 s of silence.
Errors that only cost packets (the network gone for a moment, a full buffer, a monitor in standby)
don't end a session: the host keeps trying and FEC or a keyframe repairs the picture. If one of the
host's pipeline threads stops anyway, the host ends the session and sends Bye, so the client never
shows a frozen picture.
