# Testing

Fernsicht is tested on several levels. Each level answers a different
question. The lower levels are fast and precise and run on every push. The
upper levels check how the parts work together. They also run in CI, some
of them longer at night.

```text
                ┌──────────────────────────────┐
                │ Soak (nightly, 60 s release) │  does it survive a minute of bad network?
              ┌─┴──────────────────────────────┴─┐
              │ E2E: Rust scenarios + binaries,  │  does the product work as a whole?
              │      Playwright + axe            │
            ┌─┴──────────────────────────────────┴─┐
            │ Integration: real sockets, protocol  │  do host and client follow the protocol?
            │ conformance, fake host, components   │
          ┌─┴──────────────────────────────────────┴─┐
          │ Property tests (proptest) + fuzzing      │  does it hold for *all* inputs?
        ┌─┴──────────────────────────────────────────┴─┐
        │ Unit tests in every module                   │  does the function compute correctly?
        └──────────────────────────────────────────────┘
```

## Rust

| Level | Where | What | Command |
|---|---|---|---|
| Unit | `#[cfg(test)]` in every module | Packet format, FEC sizes, reassembly edge cases, slot, statistics, overlay formatting, reference chain | `cargo test --workspace --lib` |
| Property | `crates/*/tests/properties.rs` | Protocol round trips and arbitrary bytes; FEC reconstructs exactly for *every* loss pattern within the budget; hostile headers; statistics = naive model; clock sync exact, or error ≤ RTT/2; every 1-byte corruption is detected | `cargo test --workspace --test properties` |
| Integration | `crates/net/tests/udp_transport.rs` | Packetizer → pacer → real UDP socket → reassembler, with and without FEC | `cargo test -p fernsicht-net` |
| Protocol conformance | `apps/host-agent/tests/protocol.rs` | A raw UDP peer against the real host: negotiation, keyframe request, adaptive FEC, Bye, timeouts, session takeover, garbage packets, counters | `cargo test -p fernsicht-host-agent` |
| Client against fake host | `apps/client/tests/fake_host.rs` | Scriptable host: Hello retries, clock offset of 5 s, feedback, corrupt frames, Bye, silent host | `cargo test -p fernsicht-client` |
| E2E scenarios | `tests/e2e/tests/scenarios.rs`, `sessions.rs` | Real host and client through a UDP proxy with loss, duplicates, reordering, delay/jitter and a dead zone (signal dropout). Among other things, checks that 8 ms of network delay shows up in the overlay as "Netz" (network) | `cargo test -p fernsicht-e2e` |
| E2E binaries | `tests/e2e/tests/binaries.rs` | The shipped programs: CLI, exit codes, a complete run with 1 % loss | `cargo test -p fernsicht-e2e --test binaries` |
| Soak | `tests/e2e/tests/soak.rs` | 60 s of 1080p60 with mixed impairments | `cargo test -p fernsicht-e2e --release --test soak -- --ignored` |
| Fuzzing | `fuzz/` | Parser, reassembler, FEC round trip and decoder with libFuzzer | see [`fuzz/README.md`](https://github.com/firsttris/fernsicht/blob/main/fuzz/README.md) |
| Benchmarks | `crates/net/benches/` | Packetize/FEC and reassembly with/without recovery (criterion) | `cargo bench -p fernsicht-net` |
| GPU (real hardware) | `.github/workflows/gpu.yml` on self-hosted runners (`gpu-amd`, `gpu-nvidia`) | VAAPI/Vulkan Video capabilities and the whole test suite on the target machine. **NVIDIA:** `crates/codec/tests/nvidia.rs`: NVENC → NVDEC (quality, CBR, keyframes, latency, many sessions); the screen as a DMA-BUF through Vulkan → CUDA (BT.709 colour patches in all eight formats, scaling, mixed operation, broken buffers, latency); HEVC (parameter sets in every keyframe, late join, quality against H.264 at the same bitrate, screen input, capability query). **AMD:** `crates/codec/tests/vaapi.rs` (quality, CBR, keyframes, latency; DMA-BUF import with BT.709 colour patches and scaling), the same HEVC tests with VAAPI, and AV1 (sequence header in every keyframe, late join, quality against HEVC and H.264, screen input). **Both:** `tests/e2e/tests/gpu.rs` sends real video through host → impaired network → client (stage latencies in the job summary), HEVC also with 1 % loss and with “Automatisch” (automatic), which must pick the best codec both sides support; `tests/e2e/tests/web_gpu.rs` plays a browser that offers AV1 and H.264: AV1 must arrive on AMD, H.264 on the GTX 1080, and everything must decode. Runs on pushes to `main`, at night and on demand | [GPU runner](gpu-runner.md) |
| Vulkan renderer | `crates/render/tests/vulkan.rs` | NV12 → RGB against reference colors (reading back the image), mouse cursor in the right place (also with scaled video) with premultiplied alpha, black bars for a different aspect ratio, resizing, faulty images. In CI on llvmpipe (`FERNSICHT_REQUIRE_VULKAN=1`, no silent skipping); on the AMD runner additionally VAAPI decoder → DMA-BUF → Vulkan without a copy, compared with the CPU path, timings in the job summary; on the NVIDIA runner the CPU path with the proprietary driver. The window itself (swapchain, mailbox) is tested by hand | `cargo test -p fernsicht-render --features vulkan` |
| Mouse cursor | `crates/proto` (packets, property tests, fuzzing), `apps/client/src/cursor.rs` (assembling image pieces, ordering, loss), `apps/host-agent/tests/protocol.rs` (shape, position, repetition), `tests/e2e/tests/sessions.rs` (host → client, also at 50 % loss) | | `cargo test -p fernsicht-e2e --test sessions` |
| Input | `crates/proto` (events, invalid codes, property tests, fuzzing), `crates/input` (queue: repetition, coalescing, release; uinput event format, monitor coordinate mapping), `apps/host-agent/src/layout.rs` (KWin monitor layout), `apps/host-agent/tests/protocol.rs` (once, in order, acknowledged; off without `--input`; strangers ignored), `tests/e2e/tests/sessions.rs` (40 key events at 30 % loss in both directions) | | `cargo test -p fernsicht-input --all-features` |
| Audio | `crates/audio` (jitter buffer: ordering, duplicates, gaps, overflow; test tone; Opus round trip and concealment), `crates/audio/tests/pulse.rs` (play a tone and record it again via the monitor, only in CI with `FERNSICHT_PULSE_TEST=1`), `crates/input/tests/uinput.rs` (real uinput devices, only in CI with `FERNSICHT_UINPUT_TEST=1`), `tests/e2e/tests/sessions.rs` (test tone host → client, clean and with 20 % loss) | | `FERNSICHT_REQUIRE_OPUS=1 cargo test -p fernsicht-audio` |
| Security | `crates/secure` (pairing: correct/wrong PIN, tampering, broken messages; session: handshake, wrong host, fresh keys, forgeries, replays, reordering, multiple threads; replay window via proptest against a model), `apps/host-agent/tests/protocol.rs` (after the handshake only sealed traffic, sealed input exactly once, plaintext ignored, pairing mode and 3 attempts, repeated handshake discarded), `tests/e2e/tests/security.rs` (pair, stream and type encrypted, wrong PIN, unpaired, foreign host, 30 % loss), `tests/e2e/tests/binaries.rs` (the real flow with `--pair` and `fernsicht-client pair`; pairing, status and removal via the control socket of a running host), `apps/host-agent/src/control.rs` (control socket: commands, other users rejected) | | `cargo test -p fernsicht-secure` |
| Device discovery | `crates/proto` (query and reply, padding against amplification, strings via proptest, fuzzing), `apps/client/src/discover.rs` (collecting replies, discarding duplicate and stale ones, silent targets), `apps/host-agent/src/lib.rs` (replies limited to 20/s, encoder choice, GPU name), `crates/core/src/sysinfo.rs` (OS and GPU names), `tests/e2e/tests/security.rs` (host describes itself, shows pairing and connection; no reply without a key), `tests/e2e/tests/binaries.rs` (`fernsicht-client discover` shows the paired host) | | `cargo test -p fernsicht-e2e --test security` |
| Desktop app | `apps/desktop/tests/backend.rs` (the app logic against a real host and the real client program: find, pair with the PIN from the host, session with overlay, disconnect, removed by the host → clear error message, forget), `apps/client-ui/src/app-mode.test.tsx` (the UI in the app with mocked commands: list, pairing, wrong PIN, starting and ending a session, error reasons, PIN on your own host with a countdown), `tests/e2e/tests/binaries.rs` (client with `--app`: JSON overlay, exits when stdin closes) | | `cargo test --manifest-path apps/desktop/Cargo.toml` |
| Vulkan conversion for NVENC | `crates/gpu/tests/convert.rs` | RGB DMA-BUF → NV12: BT.709 exact (±1.5) for all eight formats, scaling, memory export, import cache with new descriptors per frame, error paths. In CI on llvmpipe with validation layers, on both GPU runners with the real driver | `cargo test -p fernsicht-gpu` |
| Web viewer | `tests/e2e/tests/web.rs` (a WebRTC peer built on str0m plays the browser: page and API, PIN missing/wrong/correct, video, audio, mouse cursor, statistics, input, saying goodbye ends the session immediately), `apps/host-agent/src/web.rs` (input messages and limits, paths stay inside the viewer folder, PIN single-use), `web/viewer/src/lib/*.test.ts` (mouse cursor packets, key codes, mouse position, wheel, connection setup, overlay from `getStats()`), `web/viewer/src/routes/live.test.tsx` (a whole session with mocked WebRTC: PIN, video, overlay, input, pointer capture, disconnect, abort) | | `cargo test -p fernsicht-e2e --test web` |
| Gamepad, bitrate | `crates/proto` (gamepad events, limits, proptest), `crates/input/src/gamepad.rs` (scaling evdev values, D-pad and trigger buttons, xpad mapping, unplugging), `crates/input/tests/uinput.rs` (the host's virtual pad read back via evdev, only in CI), `crates/input/src/queue.rs` (coalescing axes, release), `crates/net/src/rate.rs` (clean, Wi-Fi loss, congestion, lower bound, recovery), `apps/host-agent/tests/protocol.rs` (reported congestion makes the frames smaller), `web/viewer/src/lib/gamepad.test.ts`, `apps/client-ui` (settings, forgetting, audio and mode during the session) | | `cargo test -p fernsicht-net rate` |
| KMS capture | `crates/capture/src/kms.rs` | Selection of card, monitor and plane, VBlank timing grid, error paths; the host rejects sessions without capture/encoder. Real capture is tested by hand | `cargo test -p fernsicht-capture --features kms`, [docs/kms-capture.md](kms-capture.md) |
| Coverage | CI job `rust-coverage` | `cargo llvm-cov`, threshold 90 % of lines | `cargo llvm-cov --workspace --ignore-filename-regex 'main\.rs$'` |

The streaming scenarios measure time. That is why they run one after
another within the test process (`fernsicht_e2e::exclusive()`). Network
loss (client) and host overload (`HostStats`) are counted separately. That
way a busy CI runner does not show up as a network error.

## Web

| Level | Where | What | Command |
|---|---|---|---|
| Unit | `web/ui/src/**/*.test.ts` | Formatting (ms, %, device ID), demo data | `pnpm test` |
| Component | `*.test.tsx` (Vitest, jsdom, Testing Library) | Overlay, session view (mode, keyboard shortcuts, audio), device list (search, filter, dialog), navigation, clipboard, viewer form | `pnpm test` |
| Coverage | Vitest v8 | Thresholds: 90 % lines/functions, 85 % branches | `pnpm test:coverage` |
| Browser E2E | `web/e2e` (Playwright, desktop + Pixel 7) | Real builds of both apps: flows, keyboard, deep links, live overlay | `pnpm test:e2e` |
| Accessibility | `web/e2e` (axe) | WCAG 2.1 AA with no "serious"/"critical" findings and no horizontal scrolling on the phone | `pnpm test:e2e` |

## What the tests have found so far

The following bugs were found while building the suite and have been
fixed. Each one now has a regression test.

1. **Out-of-bounds in the reassembler** (proptest): An FEC group whose data
   extended past the end of the frame made recovery write past the buffer.
   A tampered packet could crash the client.
2. **Memory DoS** (cargo-fuzz): A forged `frame_len` of ~4 GB made the
   client allocate the whole frame up front. There are now hard limits on
   frame size, group size, group count and recovery share.
3. **Black screen after overload** (E2E): "Latest frame wins" discarded
   *compressed* frames that later frames depend on. If the first keyframe
   was overwritten, the picture stayed black for good. Compressed frames
   now go through a FIFO, only raw or decoded frames are discarded, and gaps
   trigger a keyframe request.
4. **1 % loss was not invisible** (E2E): A fixed 10 % FEC let small groups
   fail about every 20 s. The redundancy is now sized per group, binomially,
   from the measured loss rate (failure ≤ 10⁻⁵).
5. **Counting errors**: Frames before the first complete frame were missing
   from the loss statistics. Frames that were waiting for a keyframe
   counted as decode errors.
6. **Phone layout** (Playwright): On narrow screens, the latency overlay
   covered the wrapped toolbar, including "Trennen" (disconnect).
7. **Contrast** (axe): The video placeholder had only 3.9:1.
8. **Counter overflow** (cargo-fuzz in CI): A forged first frame ID close to
   `u32::MAX` made `frames_dropped` overflow (panic in a debug build, silent
   wraparound in release). Counters now saturate, and gaps of more than
   65,536 frames count as a resync.
