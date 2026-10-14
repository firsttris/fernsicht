# Next steps

What would come next, in a sensible order. The findings of the
[code review](code-review.md) (bugs, security, performance, clean-up) come first. Performance items are explained in detail, with their
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
| **Slices**: send parts of a frame before it is encoded (the header already has the fields) | several ms per frame | large |
| **4:4:4 colour** for the desktop | crisp text; NVENC only | medium |

## 4. Features

- **File transfer** between client and host, and pictures in the clipboard (text works).
- **Rumble in the web viewer**, more than four controllers, and the DualSense's adaptive triggers
  and lightbar (gyro and touchpad work).
- **A visible hint when the pointer is captured**, and how to release it.
- **PipeWire portal capture** as a second capture backend: no root needed, works where KMS cannot
  (e.g. some multi-GPU setups), at the cost of a portal dialog and a little latency.
- **A privileged helper for capture only**, so the host service no longer needs to run fully as root.
- **Remember a browser** instead of asking for a PIN every time.

## 5. Beyond the LAN

Connections over the internet through one's own rendezvous service, with hole punching, IPv6 and
a TURN relay, are built ([over the internet](internet.md)). What is left:

- **See it live** between two homes, on a phone's hotspot (carrier-grade NAT) and on IPv6 only.
- **Port forwarding by the router** (PCP, NAT-PMP, UPnP IGD) as an option, with discovery answered
  only locally ([stage 4](internet.md#stages)).
- **The web viewer over the internet**: HTTPS for its page and the PIN, the host's public
  candidates and ICE servers for the browser ([stage 5](internet.md#stages)).

## 6. Quality

- **A test with real Chrome in CI** for the web viewer's WebRTC path.
- **Live tests of the AppImage** on fresh installs of Bazzite, Fedora and Ubuntu.

## Open decisions

- **Name and brand.**
- **First client:** our own client (as now) or also Moonlight compatibility.

Decided: the license (AGPL-3.0, commercial license on request), the codecs (AV1, HEVC and H.264,
chosen per connection) and the rendezvous service for the internet: one protocol with two
implementations, a Cloudflare Worker and a small Rust service; everyone runs their own, the host
hands its address to its clients ([over the internet](internet.md#everyone-runs-their-own)).
