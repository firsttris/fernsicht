# Performance

Where Fernsicht can still get faster and better. As of 2026-10-10: what
Fernsicht does today, what it gains, and where there is still room. For the
overview of open tasks, see [handover.md](handover.md).

## Where we stand

| Route | Glass-to-glass | Largest contributor |
|---|---|---|
| Loopback (one machine) | ≈ 9.4 ms | – |
| zentrale → bazzite, 1080p, Wi-Fi | 11.5 ms | Network (Wi-Fi) |
| zentrale → bazzite, 1440p, Wi-Fi | 16.3 ms | Encode 10 ms (GPU clocks down) |
| GTX 1080, NVENC → NVDEC, without a screen | ≈ 5 ms | – |

This is already state of the art:

- **Zero-copy image path:** KMS → DMA-BUF → hardware encoder on the host,
  decoder → Vulkan on the client. No pixel goes through the CPU.
- **Encoder tuned for latency:** no B-frames, no lookahead, CBR, keyframes
  only on request.
- **Network:** a custom UDP protocol with forward error correction
  (Reed-Solomon FEC) instead of retransmission, pacing, and bitrate
  adaptation under congestion.
- **Encryption:** Noise/ChaCha20, costs practically no time.
- **Rust:** no garbage collector pauses, no allocations in the hot path.

Whether we are faster than Sunshine/Moonlight is still open: the comparison
measurement is missing ([latency-baseline.md](latency-baseline.md)).

## What is still possible

| # | Measure | What it gains | Effort | Status |
|---|---|---|---|---|
| 1 | **Comparison measurement with Sunshine/Moonlight** | We know where we stand and what is worth doing | Small, needs the user at the machine | Open |
| 2 | **Clock the GPU up during sessions** | Encode at 1440p from 10 ms towards 3–4 ms (estimated) | Small | Built, measurement open |
| 3 | **AV1 or HEVC instead of H.264** | Same quality with 30–50 % less bitrate: noticeably sharper, especially over Wi-Fi. Same latency | Medium | **Done:** AV1, HEVC, H.264 – every connection uses the best format both sides can handle in hardware; the web viewer uses AV1 or H.264 |
| 4 | **Show immediately instead of waiting for the refresh** (Vulkan "Immediate" in gaming mode) | Up to one display refresh less: on average ≈ 3 ms at 165 Hz, ≈ 8 ms at 60 Hz. The cost is tearing | Small to medium | Open |
| 5 | **Slices: send parts of the image before the image is finished** (like Parsec) | Several milliseconds per frame, because encoding, sending and decoding overlap | Large | Open |
| 6 | **Intra refresh instead of keyframes** | No large data bursts after a loss, so less stutter over Wi-Fi | Medium | Open |
| 7 | **Delay-based congestion control** (like WebRTC) | Reacts to growing transit time before packets are lost. Better over Wi-Fi | Medium | Open (today: loss-based) |
| 8 | **4:4:4 color for the desktop** | Crisp text (today, color is transmitted at half resolution) | Medium | Open, only with NVENC (H.264/HEVC). AMD and Intel do not encode 4:4:4 |
| 9 | **Bitrate adaptation in the web viewer** (WebRTC bandwidth estimation, TWCC) | The browser gets no more than the network can carry | Medium | Open (today: fixed bitrate) |
| 10 | **Cable instead of Wi-Fi** | Less latency and variation. Not a software matter | – | Recommendation |

## The individual items

### 3 · AV1 / HEVC

AV1 is the most modern of the three codecs; HEVC sits in between. Both need
noticeably less bitrate than H.264 for the same quality. The latency stays
the same, because the hardware encoders are similarly fast.

Who can do what:

| Graphics card | Encode (host) | Decode (client) |
|---|---|---|
| AMD RX 7800 XT (zentrale, VCN 4) | H.264, HEVC, **AV1** | H.264, HEVC, AV1 |
| NVIDIA GTX 1080 (bazzite, Pascal) | H.264, HEVC | H.264, HEVC (**no AV1**) |
| NVIDIA RTX 30 and newer, AMD RX 6000 and newer, Intel Arc | – | AV1 |
| NVIDIA RTX 40 and newer, AMD RX 7000 and newer, Intel Arc | AV1 | – |

That is why host and client must negotiate what both can do:

- The client says during connection setup what it can decode.
- The host picks the best format it can encode.

For zentrale → bazzite, this means HEVC. AV1 would only work with a newer
card in the client. In the web viewer, the browser decides: Chrome decodes
AV1 and usually HEVC too.

**Built** (2026-10-10): HEVC, then AV1.

- The client reports in its `Hello` what it decodes in hardware (VAAPI:
  `vaQueryConfigProfiles`, NVIDIA: `cuvidGetDecoderCaps`). The host prefers
  AV1 over HEVC over H.264 and falls back if its GPU does not encode a
  format. Older clients and hosts keep speaking H.264.
- The app decodes only in hardware. The bazzite (GTX 1080, no AV1)
  therefore gets HEVC, even from the zentrale, which could do AV1.
- Selectable in the app ("Einstellungen → Videoformat", settings → video
  format) and with `fernsicht-client --codec auto|h264|hevc|av1`.
- The web viewer reads from the browser's offer whether it can do AV1
  (Chrome, Firefox: yes, even without AV1 hardware), and then gets AV1,
  otherwise H.264. The web viewer does not offer HEVC.

What this means for the two machines:

| Connection to the zentrale (RX 7800 XT) | Format |
|---|---|
| Chrome/Firefox on the bazzite | AV1 |
| App on the bazzite | HEVC |
| Safari | H.264 |
| Connection to the bazzite (GTX 1080) | HEVC (app), H.264 (browser) |

Measured on the GTX 1080 (NVENC → NVDEC, 1080p60):

| | H.264 | HEVC |
|---|---|---|
| 4 Mbit/s: average bytes per frame | 8,273 | 2,681 |
| 4 Mbit/s: PSNR (Y), worst frame | 29.1 dB | 32.4 dB |
| 20 Mbit/s, loopback: glass-to-glass without a screen | 5.0 ms | 4.1 ms |

The numbers for VAAPI (zentrale), including AV1 against HEVC and H.264, are
in the job summary of the AMD runner.

### 4 · Show immediately

Today the stream window presents with "Mailbox": the newest image is shown
at the monitor's next refresh. On average it waits half a refresh cycle.

"Immediate" shows it right away, even in the middle of a refresh. This
saves that wait, but causes tearing (a visible edge where the old and the
new image meet). That is common for games, but not for the desktop. So it
is for gaming mode only. With VRR/FreeSync, the tearing largely
disappears.

### 5 · Slices

The encoder splits the image into stripes. Each finished stripe goes onto
the wire immediately, and the client already decodes it while the host is
still working on the next one. This saves a good part of the encode and
transmission time.

This needs:

- slice output in the encoder (NVENC handles it well, VAAPI only in a
  limited way);
- a change in the protocol (packets per slice instead of per image);
- a decoder that accepts parts.

This is the largest rework in this list, but also the last big lever.

### 6 · Intra refresh

Today, after a loss, the host sends a keyframe: a large image that briefly
needs a lot of bandwidth. With intra refresh, the image is instead renewed
stripe by stripe, spread over several frames. The amount of data stays
even, and there is no stutter from a burst.

### 7 · Congestion control

Today the host lowers the bitrate when frames are lost despite FEC or when
its sender cannot keep up
([`crates/net/src/rate.rs`](https://github.com/firsttris/fernsicht/blob/main/crates/net/src/rate.rs)).
Delay-based control also watches whether the transit time of the packets
grows. That is the early sign that a queue is filling up somewhere. It then
slows down before anything is lost at all.

## What cannot be made faster

- **The monitor:** An image is visible only once the monitor draws it. At
  165 Hz that is up to 6 ms, at 60 Hz up to 16.7 ms (item 4 wins back part
  of this).
- **Radio:** Wi-Fi has its own latency and variation; no software can fully
  compensate for that.
- **Encoding takes time:** An image must be at least partly encoded before
  sending starts (item 5 shortens this).

## Recommended order

1. **Comparison measurement** (1): know where we stand.
2. **Measure GPU up-clocking** (2): it is built, it only needs measuring.
3. **HEVC/AV1** (3): a visibly better picture.
4. **Show immediately in gaming mode** (4): a small change, a noticeable
   gain.
5. **Intra refresh and congestion control** (6, 7): for Wi-Fi.
6. **Slices** (5): the last milliseconds.

## How to measure

- **Overlay:** The overlay in the app and the browser shows glass-to-glass
  and the individual stages ("Capture", "Encode", "Netz" (network),
  "Decode", "Anzeige" (display)) every second.
- **Measuring:** always at the same resolution, frame rate and network,
  before and after.
- **Without a screen:** `fernsicht-client <host> --headless --duration 10`
  prints the same values in the terminal.
