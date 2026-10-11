# KMS capture

The host agent can grab the screen directly over KMS (`--capture kms`). It
waits for the monitor's VBlank, takes the framebuffer that the graphics
card is currently showing, and passes it to the encoder as a DMA-BUF. The
GPU converts RGB → NV12 (BT.709) and scales to the stream resolution. Not a
single pixel is copied through the CPU.

This works on both machines:

- **AMD/Intel** (`--encoder vaapi`): the GPU's video processor
  (`scale_vaapi`) does the conversion.
- **NVIDIA** (`--encoder nvenc`): a Vulkan compute shader does the
  conversion into GPU memory that CUDA imports. CUDA copies the image on
  the GPU into the NVENC input. On the GTX 1080 all of this together takes
  2.5 ms at 1080p.

`--encoder auto` picks the encoder that matches the graphics card.

This also works on the login screen and in Gaming Mode (gamescope), and it
needs no confirmation dialog. In return, the kernel requires
**`CAP_SYS_ADMIN`**: only then does it hand out other processes'
framebuffers.

## Build (in the Distrobox)

```sh
distrobox enter fernsicht
cd ~/fernsicht
git pull
cargo build --release -p fernsicht-host-agent --features vaapi,kms
cargo build --release -p fernsicht-client --features vaapi,window
```

## Start (directly on the host)

Bazzite ships FFmpeg and libva itself, so the built programs run without
the box. This is also the easiest way to get root rights: `sudo` on the
host is real root, in the normal box it is not.

### Once: pair devices

Connections are encrypted, and only paired devices are accepted. You pair
once per client computer, like with Bluetooth:

1. Start the host with `--pair`. It shows a 6-digit PIN that is valid for
   5 minutes:
   ```sh
   sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi --pair
   ```
   ```text
   Kopplung offen für 5 Minuten. PIN: 482913
   ```
2. Enter the PIN on the client computer:
   ```sh
   fernsicht-client pair 192.168.178.87 482913
   ```
   ```text
   Gekoppelt mit zentrale (192.168.178.87:47800, Schlüssel 3f2a-…).
   ```

If the host is already running (for example as a service, see
[installation.md](installation.md)), `fernsicht-host-agent pair` in a
second terminal opens pairing without restarting it.

After three wrong PINs the host closes pairing. Then just open it again
(`fernsicht-host-agent pair` or restart with `--pair`).
`fernsicht-client hosts` lists the paired hosts, and
`fernsicht-client forget zentrale` forgets one.

On the host, the keys are stored in `/var/lib/fernsicht` (when started as
root) or `~/.config/fernsicht/host`; on the client in
`~/.config/fernsicht`.

**Terminal 1 – host agent**

```sh
cd ~/fernsicht
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi
```

The output shows which monitor is captured, for example
`capturing 2560×1440 over KMS (Selection { plane: 71, crtc: 80, pipe: 1 })`.
This line only appears once a client connects.

If a program reports
`libavcodec.so.NN: cannot open shared object file` after a Bazzite update,
the FFmpeg major version has changed: update the box (`dev/setup.sh`) and
build again.

**Terminal 2 – client**

```sh
~/fernsicht/target/release/fernsicht-client zentrale
```

Instead of the name you can also use the address (`192.168.178.87`; port
47800 is the default).

A window opens with the video, and the title shows the latency. Esc closes
it, F11 switches to full screen. On the same computer, the window shows the
screen it is on itself: a mirror in a mirror. This is expected.

Without a window, recording to a file:

```sh
./target/release/fernsicht-client zentrale --headless --duration 10 --record ~/fernsicht-test.h264
ffplay -framerate 60 ~/fernsicht-test.h264
```

It works the same way from a second computer (pair first). On the NVIDIA
machine, build the client with NVDEC (`--features nvidia,window`); it uses
NVDEC automatically when VAAPI does not work.

## Audio

With `--capture kms` the host sends what the computer plays by default,
and the client plays it back. To turn this off: `--audio off` on the host
or `--no-audio` on the client. `--audio tone` sends a 440 Hz test tone.
When started with sudo, the host connects to the user's sound server by
itself (through `SUDO_UID`).

## Remote control (mouse and keyboard)

With `--input` the host accepts the client's mouse and keyboard:

```sh
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi --input
```

The host accepts input only from paired devices, and input is encrypted
like everything else.

In the client window, the mouse and all keys go to the host, including Esc
and F11. The client itself then only listens to:

| Keys | Effect |
|---|---|
| Ctrl+Alt+Shift+Q | Quit the client |
| Ctrl+Alt+Shift+F | Toggle full screen |

With `--view-only` the client sends nothing; then Esc and F11 quit and
toggle full screen as before. When the window loses focus, the client
releases all keys so that nothing gets stuck on the host.

With several monitors, the host reads the layout from KDE's
`~/.config/kwinoutputconfig.json`, so that the pointer lands on the
streamed monitor. The output shows `pointer input mapped to DP-1: …`.

## Options

| Option | Meaning |
|---|---|
| `--kms-card /dev/dri/card1` | Choose the graphics card (default: the first one with an active monitor) |
| `--kms-connector DP-1` | The monitor a session starts with. Names: `ls /sys/class/drm` shows, for example, `card1-DP-1` → `DP-1`. During the session you switch with “Bildschirm wählen” (choose screen) in the toolbar or Ctrl+Alt+Shift+←/→ in the app window |
| `--max-width/--max-height` | Upper limit of the stream resolution; the encoder scales the monitor down to it |

## Troubleshooting

| Message | Cause and fix |
|---|---|
| `KMS capture needs CAP_SYS_ADMIN` | Not started with `sudo`, or started in the Distrobox instead of on the host. See above. |
| `no display to capture` | No monitor active, or the wrong card. Choose with `--kms-card` or `--kms-connector`. |
| `connector DP-2 is not active (active: ["DP-1"])` | Use the name shown. |
| `the driver cannot import this DMA-BUF (…)` | The driver cannot read the buffer format. Please send the whole line. |
| `display is off (no framebuffer)` | The monitor is in standby. |
| Session is rejected (client waits for a reply) | The error message is in the host agent's terminal. |

## Known limitations

- **Mouse pointer:** It sits on its own hardware plane (cursor plane) and
  is therefore not in the image. The host reads it from there and sends
  its position and image separately, and the client draws it on top. If
  the compositor draws the pointer into the image itself (software
  cursor), it is in the video anyway.
- **Overlays are missing.** Likewise, only the primary plane is captured;
  gamescope puts some of the Steam overlays on their own planes.
- **Root process.** For now the whole host agent runs as root. This is
  only meant for testing on the LAN. Later, a small privileged helper will
  take over only the capture.
- **HDR** is not transferred faithfully yet. 10-bit desktops (KDE on AMD
  uses `AB30`) work, but are converted down to 8-bit H.264.
- Minimal **tearing** is possible if the compositor writes into the buffer
  that is being read (as with Sunshine).

## How it is tested

- Selecting the card, monitor and plane, and the VBlank timing grid, run
  as unit tests without hardware
  (`cargo test -p fernsicht-capture --features kms`).
- The DMA-BUF → Vulkan → NVENC path runs in CI on the NVIDIA runner
  ([`crates/codec/tests/nvidia.rs`](https://github.com/firsttris/fernsicht/blob/main/crates/codec/tests/nvidia.rs)),
  with the same checks as below. There, the test image is in NVIDIA's tiled
  layout, the way the compositor creates it. The Vulkan conversion on its
  own is also checked by the normal CI (software Vulkan, with validation
  layers) and by both GPU runners
  ([`crates/gpu/tests/convert.rs`](https://github.com/firsttris/fernsicht/blob/main/crates/gpu/tests/convert.rs)).
- The DMA-BUF → VAAPI path runs in CI on the AMD runner. Instead of a KMS
  framebuffer, an exported VAAPI image serves as the DMA-BUF; the rest is
  identical. The tests check:
  - color accuracy according to BT.709 on six color patches, for all
    eight formats (8 and 10 bit, RGB and BGR order, with and without
    alpha);
  - scaling from 1440p to 1080p;
  - switching between CPU and DMA-BUF input;
  - rejection of broken buffers;
  - the encode time.
- The actual KMS capture needs a monitor and root, so it does not run on
  the runner but by hand, as described above.

## Alternative: rootful Distrobox

If the programs ever fail to start on the host (different FFmpeg
version), you can also use a second, rootful box in which `sudo` is real
root: run `dev/setup.sh --root` once, then
`distrobox enter --root fernsicht-root`, and start there with `sudo` as
above. The first time you enter it, the box asks for a new password that
is only for `sudo` in this box.
