# Status

What is built, phase by phase. "Done" means implemented and covered by automatic tests; what has not
been tried on real hardware by a person yet is listed at the end and in the
[handover notes](handover.md).

Goal of phase 1: glass-to-glass under 20 ms in the LAN at 1080p60. It is met: about 10 ms without
the monitor's own delay ([performance](performance.md)).

## Phases

| Phase | Part | State |
|---|---|---|
| 0 – Foundation | Workspace, CI, latency measured per stage, clock sync, overlay, Distrobox | ✅ Done. The reference measurement against Sunshine is still to do ([method](latency-baseline.md)) |
| 1 – Hot path in the LAN | Packet format, FEC, pacing, UDP, slots, threads | ✅ Transport done and tested (1 % loss without a lost frame) |
| | VAAPI encode and decode | ✅ Through the whole pipeline on the AMD runner: glass-to-glass without the monitor ≈ 10 ms (1080p60, debug build) |
| | KMS capture → DMA-BUF → VAAPI without a copy | ✅ Implemented; import and GPU colour conversion tested in CI (all eight desktop formats, 8 and 10 bit), KMS itself by hand ([guide](kms-capture.md)) |
| | Client window (Vulkan, winit) | ✅ The VAAPI picture without a copy in Vulkan (0.15 ms to convert and draw at 1080p), mailbox present, CPU fallback; render tests in CI on llvmpipe with the validation layers |
| | NVIDIA: NVENC/NVDEC | ✅ Encoder and decoder through FFmpeg/CUDA, tested on the NVIDIA runner. Screen capture for NVENC without a CPU copy: KMS DMA-BUF → Vulkan compute (RGB → NV12, BT.709, scaled) → memory CUDA imports → NVENC, 2.5 ms per 1080p frame on the GTX 1080. Decoded NVDEC pictures still go through the CPU to Vulkan |
| | Codecs | ✅ AV1, HEVC and H.264; each session takes the best codec both sides do in hardware; the web viewer AV1 or H.264 ([video path](video.md#codecs)) |
| | Mouse pointer | ✅ Its own packets (position per frame, image when it changes, repeated against loss); the client draws it over the video. KMS reads the cursor plane, the test pattern has a circling arrow |
| | Several monitors | ✅ The host lists its monitors; the client switches during the session (menu, Ctrl+Alt+Shift+←/→); pointer mapping follows |
| | PipeWire capture | ⏳ Open |
| 2 – Control | Mouse and keyboard | ✅ Reliable over UDP (retransmitted until acknowledged, every event exactly once, tested at 30 % loss); the host injects through `uinput` (`--input`); absolute pointer positions mapped onto the captured monitor (KDE's monitor arrangement) |
| | System keys | ✅ The app window passes the desktop's shortcuts (Meta, Alt+Tab, Ctrl+Alt+Del) to the host in fullscreen or with a captured pointer (Wayland keyboard-shortcuts-inhibit, X11 grab); “Tasten senden” (send keys) menu in app and browser; Keyboard Lock in Chrome/Edge fullscreen |
| | Sound | ✅ What the host plays (PipeWire), Opus with 5 ms frames, every packet also carries the previous frame (one loss leaves no gap), 15 ms jitter buffer with loss concealment and clock drift compensation; 15 ms delay in the test, 2.5 % concealed at 20 % loss |
| | Gamepads, pointer capture for games | ✅ Gaming mode: a click captures the pointer (relative mouse), Ctrl+Alt+Shift+M lets go. Up to 4 controllers (evdev in the client, Gamepad API in the browser) become virtual Xbox 360 controllers on the host, which Steam and games know without setup |
| 3 – Security | Pairing and encryption | ✅ Pair once with a 6-digit PIN (SPAKE2: no offline guessing, pairing closes after 3 wrong attempts); every session with a Noise IK handshake (as in WireGuard), then everything sealed with ChaCha20-Poly1305, replays dropped; only paired devices get in ([security](security.md)) |
| | Bitrate adaptation | ✅ The host lowers the bitrate when frames are lost despite FEC or its sender falls behind, and raises it again after 5 s of clean network. Random Wi-Fi loss stays FEC's job |
| | Internet (NAT) | ⏳ Open; today through a VPN ([remote access](remote-access.md)) |
| 4–5 – Product | Host as a service | ✅ systemd service with an install script, encoder picked for the GPU, pairing, status and removal through a local control socket ([installation](installation.md)) |
| | GPU clocks during sessions | ✅ The host keeps the GPU at full clocks while someone watches (AMD/Intel), switchable in the app; the effect is still to be measured |
| | Device discovery | ✅ `fernsicht-client discover`: broadcast over the stream port (no firewall change); hosts answer with name, key, OS, GPU and whether pairing is open; new addresses of paired hosts are taken over; the computer's own host is not listed |
| | Desktop app | ✅ Tauri around the client UI: computers in the network, pairing with a PIN, starting sessions (the picture in the native Vulkan window, the latency overlay in the app), “Dieser Rechner” (this computer) opens pairing on the local host ([the app](app.md)) |
| | Installation | ✅ One AppImage with app, client, host and web viewer; “Diesen Rechner freigeben” (share this computer) installs the host service; releases through GitHub Actions; `packaging/build.sh`, `install-app.sh`, `install-host.sh` for builds from source ([installation](installation.md)) |
| | Web viewer | ✅ In the LAN: the host serves the page itself (`http://host:47800`), access with the pairing PIN (once), picture (AV1 or H.264) and Opus sound over WebRTC (str0m), pointer, mouse and keyboard over a data channel, pointer capture in gaming mode, latency overlay from `getStats()`; on phones gestures, touchpad mode, pinch zoom and the on-screen keyboard ([web viewer](web-viewer.md)) |
| UI | Client UI and web viewer after the mockup (React, TanStack, shadcn/ui) | ✅ Interfaces with demo data in the browser, the real backend in the app |

Without a GPU the whole pipeline runs with a **test pattern** and a **synthetic codec**. That codec
makes frames of realistic size for the chosen bitrate and checks them with a checksum. So CI and
machines without a GPU still measure transport, FEC, pacing and latency for real. With
`--encoder vaapi` (feature `vaapi`) or `--encoder nvenc` (feature `nvidia`) real video is streamed;
with `--capture kms` (feature `kms`) the picture comes from the monitor.

## Seen working live

Confirmed by hand on the two test machines (zentrale: AMD RX 7800 XT; bazzite: NVIDIA GTX 1080):

- The app on bazzite connected to zentrale: picture, mouse, keyboard; the app window shows its
  content under NVIDIA.
- The web viewer, also in Firefox and on a phone: picture, mouse, keyboard.
- KMS capture on zentrale (the 10-bit desktop of KDE, `AB30`), streamed to bazzite at 1080p and
  1440p over Wi-Fi.

Built and tested automatically, but not yet tried by a person: sound, the host as a service with
pairing from the app, AppImage sharing, gaming mode with controllers, bitrate adaptation over Wi-Fi,
discovery over Wi-Fi, system keys, monitor switching, the web viewer's phone gestures, NVIDIA as
host, AV1/HEVC between the two machines. The steps to check each are in the
[handover notes](handover.md).
