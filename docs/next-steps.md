# Next steps

What would come next, in a sensible order. Performance items are explained in detail, with their
gain and effort, on the [performance](performance.md) page; what works today is on the
[status](status.md) page.

## 1. Confirm on real hardware

Several features are covered by automated tests but have not run on real machines yet. The full
checklists are in the [handover notes](handover.md#what-nobody-has-seen-live-yet).

- **Sound**, in the app and in the web viewer.
- **The host as a system service** and **sharing from the AppImage** (“Diesen Rechner freigeben”):
  picture via KMS without a sudo session, sound from the logged-in user, input, monitor layout.
- **System keys** (Meta, Alt+Tab, Ctrl+Alt+Del with the shortcut lock and the “Tasten senden” menu).
- **Switching monitors** in both directions, with the pointer landing in the right place.
- **The web viewer on real phones** (Android Chrome, iPhone Safari): gestures, zoom, typing, rotation.
- **Gaming mode and gamepads** with Xbox and PlayStation controllers; button layout in Steam.
- **Bitrate adaptation and discovery over Wi-Fi.**
- **NVIDIA as host with real KMS** (built and tested with test patterns in NVIDIA's tiling layout).

## 2. Measure

- **Reference measurement with Sunshine/Moonlight** on the same machines and network
  ([latency baseline](latency-baseline.md)): where we stand and what is worth doing.
- **Acceptance test:** 1080p60, glass-to-glass under 20 ms, filmed with a phone's slow-motion camera.
- **GPU clock boost:** encode time at 1440p with and without (`--no-gpu-boost`); if it is not enough,
  try `pp_power_profile_mode`.
- **Sharpness at native resolution** over Wi-Fi: bitrate and keyframe quality.

## 3. Latency and picture

From the [performance](performance.md) page, in its recommended order:

| | Gain | Effort |
|---|---|---|
| **Show immediately** (Vulkan immediate present in gaming mode) | up to one refresh: ≈ 3 ms at 165 Hz, ≈ 8 ms at 60 Hz; costs tearing | small |
| **Intra refresh** instead of keyframes | no data bursts after a loss, less stutter over Wi-Fi | medium |
| **Delay-based congestion control** | reacts before packets are lost | medium |
| **Bitrate adaptation in the web viewer** (WebRTC's TWCC estimate) | browsers get no more than the network carries | medium |
| **Slices**: send parts of a frame before it is encoded (the header already has the fields) | several ms per frame | large |
| **4:4:4 colour** for the desktop | crisp text; NVENC only | medium |
| **NVDEC pictures straight into Vulkan** | saves the 0.8 ms copy on NVIDIA clients | medium |

## 4. Features

- **Clipboard and file transfer.** The buttons are in the toolbar but do nothing yet.
- **Rumble** (force feedback back to the controller) and more than four controllers.
- **A visible hint when the pointer is captured**, and how to release it.
- **PipeWire portal capture** as a second capture backend: no root needed, works where KMS cannot
  (e.g. some multi-GPU setups), at the cost of a portal dialog and a little latency.
- **A privileged helper for capture only**, so the host service no longer needs to run fully as root.
- **Remember a browser** instead of asking for a PIN every time.

## 5. Beyond the LAN

Today Fernsicht works in the LAN or over a VPN ([remote access](remote-access.md)). On its own over
the internet would need:

- **NAT traversal and a rendezvous service** so devices find each other without port forwarding, and
  a **relay** for networks where hole punching fails. Open decision: self-hosted only, or a hosted
  rendezvous service as well.
- **HTTPS for the web viewer** (today the page and the PIN travel over plain HTTP) and **TURN** for
  browsers.
- **A packet-size option** for tunnels with a smaller MTU, such as Tailscale and WireGuard.

## 6. Quality

- **A test with real Chrome in CI** for the web viewer's WebRTC path.
- **Live tests of the AppImage** on fresh installs of Bazzite, Fedora and Ubuntu.

## Open decisions

- **Name and brand.**
- **First client:** our own client (as now) or also Moonlight compatibility.
- **Self-hosted only, or a hosted rendezvous service** for connections over the internet.

Decided: the license (AGPL-3.0, commercial license on request) and the codecs (AV1, HEVC and H.264,
chosen per connection).
