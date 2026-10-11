# Latency baseline: Sunshine/Moonlight (phase 0)

The target for phase 1 is glass-to-glass < 20 ms at 1080p60 on the LAN.
The baseline is measured on the same scene with Sunshine (preinstalled on
Bazzite) and Moonlight, before Fernsicht is measured against the same
scene.

## Setup

| | |
|---|---|
| Host | Bazzite, KDE Plasma (Wayland), Radeon RX 7800 XT |
| Client | [device, OS, GPU] |
| Network | [cable/Wi-Fi, switch, link rate] |
| Client monitor | [model, refresh rate] |
| Sunshine | [version], capture: KMS, encoder: VAAPI H.264 |
| Moonlight | [version], 1920×1080, 60 fps, [bitrate] Mbit/s, V-Sync off, frame pacing off |

## Method

1. On the host, show a stopwatch with milliseconds in full screen (for
   example a web page with `performance.now()`); next to it, the client
   monitor with the stream.
2. A phone in slow-motion mode (240 fps, that is 4.2 ms per frame) films
   both screens at the same time.
3. Evaluate 20 single frames per run: the host display minus the client
   display. Note the median and p95.
4. In addition, take a photo of Moonlight's statistics overlay
   (Ctrl+Alt+Shift+S): network latency, decode time, queue time.
5. Repeat with 1 % artificial packet loss:
   `sudo tc qdisc add dev <iface> root netem loss 1%` (on the host),
   afterwards `sudo tc qdisc del dev <iface> root`.

## Results

| Run | Date | Median | p95 | Moonlight overlay (network / decode) | Artifacts at 1 % loss |
|---|---|---|---|---|---|
| Sunshine 1080p60 | [YYYY-MM-DD] | [ms] | [ms] | [ms / ms] | [yes/no] |
| Sunshine 1080p120 | | | | | |
| Fernsicht 1080p60 | | | | | |

## Fernsicht's own measurement

The client measures every stage per frame (capture, encode, network,
decode, display) using timestamps in the packet header and an NTP-like
clock synchronization:

```sh
# Host
fernsicht-host-agent --bind 0.0.0.0:47800
# Client, optionally with artificial packet loss
fernsicht-client <host-ip>:47800 --fps 60 --bitrate 20000 --loss 0.01 --duration 30
```

The overlay measures up to the hand-off to the presenter. It does not
include the monitor's scan-out time, so the phone slow-motion measurement
remains the acceptance test.
