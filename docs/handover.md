# Handover notes

Where the work stands and how to pick it up: for the next session, human or agent.

Status as of 2026-10-10, commit `ed802fd`, CI and GPU runner green.

## The machines

| | zentrale | bazzite |
|---|---|---|
| Role | Host (is remote-controlled), development machine | Client |
| GPU | AMD RX 7800 XT (VAAPI) | NVIDIA GTX 1080, driver 580 (NVENC/NVDEC) |
| Network | Wired, 192.168.178.87 | Wi-Fi |
| System | Bazzite 44, KDE Wayland, two monitors 2560×1440@165 (DP-2 left, DP-1 right) | Bazzite |
| Repo | `~/fernsicht` | `~/Projects/fernsicht` |

- **Building:** in the distrobox `fernsicht` (image: `dev/Containerfile`).
- **Running:** The programs built in the box run directly on Bazzite.
  Bazzite ships FFmpeg 8, libva and WebKitGTK 4.1.
- **Self-hosted runner:** Both machines have one for the GPU tests
  (`.github/workflows/gpu.yml`, [gpu-runner.md](gpu-runner.md)).

## What is done

Goal of the last stage: use Fernsicht like a normal user. All five steps
are built, tested and pushed:

1. **Pairing and encryption.**
   - One-time, with a 6-digit PIN (SPAKE2).
   - Every session uses a Noise IK handshake, after which everything is
     sealed.
   - Only paired devices get in. Code: `crates/secure`.
2. **Host as a service.**
   - systemd unit `packaging/fernsicht-host.service`.
   - Control socket `/run/fernsicht/control.sock` with the commands
     `fernsicht-host-agent pair | status | unpair`.
   - Encoder chosen to match the GPU (`--encoder auto`).
   - Code: `apps/host-agent/src/control.rs`.
3. **Device discovery.**
   - `fernsicht-client discover`: broadcast over the stream port 47800.
     Deliberately no mDNS: the firewall on the zentrale blocks 5353.
   - Hosts answer with name, key, OS, GPU, whether pairing is open, and
     whether they are busy.
   - Code: `apps/client/src/discover.rs`, packets `Discover`/`Announce`
     in `crates/proto`.
4. **The app.**
   - Tauri around the client UI, in `apps/desktop`. This is a separate
     Cargo workspace with its own CI job.
   - It lists machines, pairs and starts sessions.
   - The picture runs in the native window of `fernsicht-client --app`.
     The client prints the overlay as JSON and exits when stdin closes.
   - “Dieser Rechner” (this computer) opens pairing on the local host and
     shows the PIN.
   - The UI (`apps/client-ui`) uses the real commands in Tauri, and demo
     data in the browser.
5. **Installation.**
   - `packaging/build.sh` builds everything in the box.
   - `packaging/install-app.sh` installs the app and client to
     `~/.local`, including a start menu entry, without sudo.
   - `packaging/install-host.sh` sets up the service and needs sudo.
   - Instructions: [installation.md](installation.md).

Added after that:

6. **Web viewer on the LAN.**
   - The host serves the page itself (`--web`, default `0.0.0.0:47800`
     TCP; `--web-root`, installed to `/usr/local/share/fernsicht/viewer`).
   - Access with the pairing PIN, once.
   - WebRTC with str0m: H.264 and Opus, plus a data channel (mouse cursor
     as protocol packets and statistics to the browser, input as JSON to
     the host).
   - Code: `apps/host-agent/src/web.rs`, `web/viewer/src/lib/host.ts`,
     `web/viewer/src/routes/live.tsx`.
   - Tested: with headless Chrome (in the box) against a real host with a
     VAAPI test pattern, 1920×1080 at 60 fps, glass-to-glass according to
     the overlay ≈ 15 ms on localhost.

7. **Gaming mode, gamepads, bitrate adaptation, app settings.**
   - Native client: `--gaming` (a click captures the pointer, relative
     motion), Ctrl+Alt+Shift+M captures and releases.
   - Gamepads: The client reads controllers directly via evdev
     (`crates/input/src/gamepad.rs`). The browser uses the Gamepad API.
     The host creates one virtual Xbox 360 controller per player
     (`crates/input/src/uinput.rs`). Buttons map by position (A at the
     bottom, Y at the top); the X/Y swap of the `xpad` driver is
     compensated on both sides.
   - Bitrate: `crates/net/src/rate.rs`. Down on frames that are lost
     despite FEC, on an overloaded sender, or on more than 20 % loss. Up
     after 5 s of clean network. VAAPI/NVENC get a new encoder for this
     (one keyframe). Tested with VAAPI and 10 % artificial loss.
   - App: page “Einstellungen” (settings) (resolution, frame rate,
     bitrate), “Gerät vergessen” (forget device); sound and mode during
     the session go to the client over stdin (`mute`, `unmute`, `gaming`,
     `desktop`).

8. **One AppImage for everything.**
   - `packaging/appimage.sh` builds locally: app, stream window, host and
     web viewer in one file. The GPU libraries (libva, libdrm, Vulkan
     loader) are left out; `appimagetool` takes care of that.
   - In the app: “Diesen Rechner freigeben” (share this computer)
     (`apps/desktop/src/share.rs`) extracts the AppImage via `pkexec` to
     `/opt/fernsicht/app` and sets up `fernsicht-host.service`. Plus
     “Freigabe beenden” (stop sharing) and “Host aktualisieren” (update
     host).
   - Release as in the other repos: **Bump version**
     (`firsttris/workflows`), then `release.yml` (checks, AppImage on
     Ubuntu 24.04, GitHub release).
   - Tested locally: The programs from the AppImage stream on Bazzite
     with the system drivers (VAAPI H.264). The app window shows
     “Diesen Rechner freigeben”.
   - Not tested live yet: the setup with the password dialog.

9. **AV1 and HEVC.** Each connection takes the best codec both sides can
   do in hardware (AV1, HEVC, H.264); the web viewer uses AV1 if the
   browser offers it, otherwise H.264. Setting “Videoformat” (video
   format) in the app, `--codec` on the client. Details and measurements
   in [performance.md](performance.md).
   Not seen live yet:
   - App on the bazzite → zentrale: the overlay should show “HEVC”.
   - Chrome/Firefox on the bazzite → zentrale: “AV1” (in the web
     viewer's overlay; in Chrome also under `chrome://webrtc-internals`).

Measured latency (earlier sessions):

| Path | Glass-to-glass |
|---|---|
| Loopback | ≈ 9.4 ms |
| zentrale → bazzite, 1080p | 11.5 ms |
| zentrale → bazzite, 1440p | 16.3 ms (encode 10 ms, because the GPU clocks down between frames) |

## What nobody has seen live yet

These items are tested automatically but have never run on real
hardware. They need the user at the machine.

Confirmed live (2026-10-10, according to the user):

- Web viewer, also with Firefox and on a phone: picture, mouse and
  keyboard.
- bazzite (NVIDIA, client) → zentrale (AMD, host): picture, mouse and
  keyboard; the app window shows something under NVIDIA (item 3 done).
- **Sound** was not checked in either case, neither in the web viewer
  nor in the app.

1. **Service on the zentrale.**
   - First stop the old host that the user started in the root box
     `fernsicht-root`. It holds UDP 47800, otherwise the service does not
     start.
   - Then install:
     `./packaging/build.sh && sudo ./packaging/install-host.sh`.
   - Check:
     - Picture via KMS as root without a sudo session.
     - Sound from the logged-in user (pulse as root via
       `/run/user/1000/pulse/native`).
     - Monitor layout (KWin config of the desktop user).
     - Input via uinput.
     - Log: `journalctl -u fernsicht-host -f`.
2. **App on both machines.**
   - Install: `./packaging/build.sh && ./packaging/install-app.sh`.
   - On the zentrale: “Gerät koppeln” (pair a device).
   - On the bazzite: zentrale in the list, “Koppeln” (pair), enter the
     PIN, “Desktop”.
   - Check:
     - List, online status, PIN countdown.
     - Overlay in the app.
     - Disconnect.
     - Closing the app ends the session.
3. ~~**App on NVIDIA (bazzite).**~~ Done, see above. (The app sets
   `WEBKIT_DISABLE_DMABUF_RENDERER=1` if `/proc/driver/nvidia` exists;
   without it the windows stay empty, tauri#9394.)
4. **Web viewer for real.** On the zentrale, after the service
   installation, open `http://192.168.178.87:47800` from another device,
   enter the PIN, then check KMS picture, sound, mouse and keyboard. Also
   with Firefox and a phone.
5. **AppImage sharing:** Start the AppImage from the first release on the
   zentrale and choose “Diesen Rechner freigeben” (KDE asks for the
   password). Then check: Is `fernsicht-host` running (`systemctl status
   fernsicht-host`)? Is there picture, sound and input? Then “Freigabe
   beenden” and share again.
6. **Gaming mode and gamepad.** Choose “Gaming” in the app. A click into
   the window captures the pointer, Ctrl+Alt+Shift+M releases it. Check a
   game with mouse control. A controller on the client shows up on the
   host as “Fernsicht X-Box 360 pad 1”: check in Steam or `evtest` that
   A is at the bottom and Y at the top. With Xbox and PlayStation
   controllers.
7. **Bitrate adaptation over Wi-Fi.** Connect on the bazzite, look for
   `bitrate … Mbit/s` in the host's log. Normal Wi-Fi losses leave the
   bitrate alone. Only frames lost despite FEC, or an overloaded sender,
   lower it.
8. **Device discovery over Wi-Fi.** Does the broadcast from the bazzite
   reach the zentrale, and the answer come back? The answer goes to an
   ephemeral port of the client. Fedora's zone `FedoraWorkstation`
   allows UDP 1025–65535; check on the bazzite
   (`firewall-cmd --list-all`). As a fallback the app also queries paired
   hosts directly.
9. **System keys to the host.** Built, never seen live:
   - App: In fullscreen or with a captured pointer, the window asks the
     desktop to pass its shortcuts through (Wayland
     `keyboard-shortcuts-inhibit`, X11 keyboard grab;
     `apps/client/src/shortcuts.rs`). KDE asks the first time. Check:
     Meta, Meta+W, Alt+Tab and Ctrl+Alt+Del reach the host; in a normal
     window Alt+Tab stays local; Ctrl+Alt+Shift+F/M/Q still work.
   - Web viewer: fullscreen button in the toolbar; Chrome/Edge then lock
     the keyboard (Keyboard Lock, hold Esc to leave).
   - Both: menu “Tasten senden” (send keys) (keyboard icon in the
     toolbar) with Ctrl+Alt+Del, Windows key, Windows+W, Alt+Tab, Alt+F4,
     Print.
10. **Switching monitors.** Built, never seen live. The host reports its
    monitors (packet `Monitors`, JSON in the browser), the client chooses
    (`SelectMonitor`). The capture thread then opens the other monitor
    on the same card and sends a keyframe; the mouse mapping follows
    (`FollowScreen`). Controls: “Bildschirm wählen” (choose screen) in
    the toolbar (from two monitors up), in the app window
    Ctrl+Alt+Shift+←/→. Check on the zentrale (DP-2 left, DP-1 right):
    switching in both directions, the mouse lands on the monitor shown,
    the pointer is in the right place. A monitor of a different size is
    scaled to the stream size from the start of the session (distorted
    if the aspect ratio differs).
11. **Web viewer on a phone.** Built, never seen live: gestures
    (`web/viewer/src/lib/touch.ts`), touchpad mode, local zoom,
    on-screen keyboard (`lib/textkeys.ts`, layout from the browser
    language), gaming mode on a phone without pointer capture. Check on
    a real phone (Android Chrome, iPhone Safari): tap, drag, long press
    for right click, two-finger scrolling, zoom and reset, typing with
    Gboard/iOS keyboard (including word suggestions), rotation.

## Open tasks, by importance

The performance items (AV1/HEVC, immediate display, slices, intra
refresh, congestion control, 4:4:4) with their benefit and effort are
collected in [performance.md](performance.md).

1. **The live tests above**, then fix the bugs that turn up.
2. **Measure GPU clock boost.** It is built: during a session the host
   sets `power_dpm_force_performance_level` to `high`
   (`apps/host-agent/src/power.rs`, switch in the app under
   “Einstellungen”). Encode at 1440p used to be 10 ms. Now measure on
   the zentrale with the host as a service (root), once with and once
   without (`--no-gpu-boost` or the switch), and compare the encode time
   in the overlay. If `high` does not help enough, try
   `pp_power_profile_mode` (e.g. the VR profile).
3. **Check sharpness at native resolution** (the user found the picture
   at 1080p over Wi-Fi not quite sharp). Bitrate and keyframe quality.
4. **Reference measurement with Sunshine/Moonlight**
   ([latency-baseline.md](latency-baseline.md)).
5. **Games, remaining:** rumble (force feedback back to the controller),
   more than 4 controllers.
6. **NVIDIA as host: check live.** Screen capture with NVENC is built.
   The path: KMS DMA-BUF → Vulkan compute (RGB → NV12, BT.709, scaled) →
   memory imported by CUDA → NVENC. Code: `crates/gpu/src/convert.rs`,
   `crates/codec/src/{nvidia,cuda}.rs`.
   - Tested on the bazzite with test patterns in the NVIDIA tiling
     layout: 2.5 ms per 1080p frame, no Xid messages.
   - Never run live: real KMS on the bazzite (HDMI-A-1). The user starts
     `sudo ./target/release/fernsicht-host-agent --capture kms --encoder nvenc --pair`
     on the bazzite, and the zentrale connects as the client.
7. **Small things in the app:** The toolbar buttons for screen,
   clipboard and files do nothing yet (sound and mode already work).
8. **Bitrate adaptation in the web viewer:** WebRTC provides a bandwidth
   estimate (TWCC), but the host does not use it yet; the bitrate stays
   fixed there.
9. **Later:** internet (NAT, rendezvous server), PipeWire capture. Web
   viewer over the internet (rendezvous, TURN), remembering the browser
   instead of asking for a PIN every time, a test with real Chrome in CI.

## Rules for working

- **Never without explicit OK** move the mouse or keyboard, play sound
  or open windows on a desktop where the user is sitting. Ask first and
  wait for the answer. Exception: a virtual display. On the zentrale the
  box has Xvfb, xdotool and ImageMagick, see below.
- **Commands as root** (`sudo`, `pkexec`) are run by the user. Do not
  work around this.
- **Other processes** of the user (e.g. their running host) must not be
  stopped. Use other ports for tests.
- **Pushing** via SSH, to both branches:
  `git push git@github.com:firsttris/fernsicht.git HEAD:main` and
  `HEAD:ccr-a9432ae9-5iz8to`. Afterwards check CI (`gh run list`).
- **Language:** documentation, README and code comments in English; the
  app's user interface stays German; commit messages in English.

## Checking that everything is right

In the box, in the repo:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install && pnpm format:check && pnpm -r typecheck && pnpm test
pnpm --filter @fernsicht/client-ui build     # the app needs the built UI
cd apps/desktop && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
shellcheck packaging/*.sh
```

The test setup is described in [testing.md](testing.md) (fuzzing,
coverage threshold 90 %, GPU tests).

## Viewing the app without a real desktop

This keeps the user's screen free: virtual X display, no Wayland, sound
goes nowhere.

```sh
Xvfb :99 -screen 0 1280x800x24 &
env -u WAYLAND_DISPLAY DISPLAY=:99 GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  LIBGL_ALWAYS_SOFTWARE=1 PULSE_SERVER=unix:/nonexistent \
  XDG_CONFIG_HOME=$(mktemp -d) FERNSICHT_CLIENT=$PWD/target/debug/fernsicht-client \
  apps/desktop/target/debug/fernsicht &
sleep 8; xwd -root -display :99 | magick xwd:- target/shots/app.png
```

Important here:

- **Test host:** A test host on a free port (e.g.
  `--bind 127.0.0.1:47801 --pair`) is not found by broadcast. Pair it
  first with the same `XDG_CONFIG_HOME` via
  `fernsicht-client pair 127.0.0.1:47801 PIN`; then the app queries it
  directly.
- **Clicking:** `DISPLAY=:99 xdotool mousemove X Y click 1`.
- **Storage:** Put screenshots in `target/shots`. The `/tmp` folder of
  the VS Code Flatpak session is not visible on the host.
