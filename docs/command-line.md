# Command line

Both programs work without the app. The app and the host service use the same options.

## Host: `fernsicht-host-agent`

Streams this computer's screen to paired clients. Without a command it runs the host; the commands
talk to a running host (for example the system service).

```text
fernsicht-host-agent [OPTIONS]
fernsicht-host-agent pair      # open pairing, show the PIN for the new device
fernsicht-host-agent status    # the host's key, paired devices, the session
fernsicht-host-agent unpair X  # forget a paired device (its name or key)
```

| Option | Default | |
|---|---|---|
| `--bind <addr>` | `0.0.0.0:47800` | UDP address to listen on |
| `--max-width`, `--max-height` | `3840`, `2160` | upper bound for the stream size; a client asking for the host's size gets the screen's, scaled down to fit |
| `--max-fps` | `144` | upper bound for the frame rate a client may ask for |
| `--max-bitrate` | `80000` | upper bound for the bitrate, kbit/s |
| `--encoder` | `synthetic` | `auto` (the GPU's: VAAPI on AMD/Intel, NVENC on NVIDIA), `vaapi`, `nvenc`, or `synthetic` (no GPU, no real picture). The service uses `auto`. The hardware encoders need the build features `vaapi` / `nvidia` |
| `--render-node` | `/dev/dri/renderD128` | GPU render node for VAAPI |
| `--cuda-device` | `0` | CUDA device index for NVENC |
| `--capture` | `test-pattern` | `test-pattern` or `kms` (the monitor; build feature `kms`, needs `CAP_SYS_ADMIN`, see [KMS capture](kms-capture.md)) |
| `--kms-card` | first card with a display | e.g. `/dev/dri/card1` |
| `--kms-connector` | first active display | the monitor a session starts with, e.g. `DP-1`; switching during the session is possible |
| `--audio` | `desktop` with KMS, `off` with the test pattern | `desktop` (what this computer plays), `tone` (440 Hz test tone), `off` |
| `--input` | off | accept mouse, keyboard and gamepads from paired clients (virtual devices through `/dev/uinput`) |
| `--pair` | | open pairing for 5 minutes at start and show the PIN |
| `--state-dir` | `/var/lib/fernsicht` as root, else `~/.config/fernsicht/host` | the host's key and paired clients |
| `--control` | `/run/fernsicht/control.sock` for the service, else the user's runtime directory | the running host's control socket |
| `--web <addr>` | `0.0.0.0:47800` | TCP address of the web viewer (page and API) |
| `--no-web` | | no web viewer |
| `--web-root` | `../share/fernsicht/viewer` next to the program | the built web viewer (`web/viewer/dist`) |
| `--no-gpu-boost` | | leave the GPU's clocks alone during sessions |
| `--pace-mbit` | `400` | pacing rate in Mbit/s |
| `--loss <f>` | `0` | drop this fraction of outgoing video packets (testing, e.g. `0.01`) |

## Client: `fernsicht-client`

Shows the host's screen in a window (build feature `window`) and prints the latency overlay once per
second.

```text
fernsicht-client [OPTIONS] <HOST>
fernsicht-client discover            # look for hosts in the local network
fernsicht-client pair <HOST> <PIN>   # pair with a host that shows a PIN
fernsicht-client hosts               # list the paired hosts
fernsicht-client forget <HOST>       # forget a paired host
```

`<HOST>` is the paired host's name or address, e.g. `zentrale` or `192.168.1.20` (port 47800 unless
given).

| Option | Default | |
|---|---|---|
| `--width`, `--height` | `0` | stream size; 0 = the host's screen size |
| `--fps` | `60` | frame rate |
| `--bitrate` | `0` | kbit/s; 0 = chosen by the host for the size (20 Mbit/s for 1080p60, about 36 for 1440p60) |
| `--codec` | `auto` | `auto` (the best this machine decodes in hardware and the host encodes: AV1, HEVC, else H.264), `h264`, `hevc`, `av1` |
| `--decoder` | `auto` | hardware decoder: `auto` (VAAPI, else NVDEC), `vaapi`, `nvdec` |
| `--render-node` | `/dev/dri/renderD128` | GPU render node for VAAPI |
| `--gaming` | | gaming mode: a click captures the pointer, which then moves relatively |
| `--view-only` | | send no mouse or keyboard input |
| `--no-audio` | | do not play the host's sound |
| `--no-gamepad` | | leave this machine's gamepads out |
| `--headless` | | no window: decode only and print the overlay |
| `--record <file>` | | save the received video: `ffplay file.h264`; HEVC as `file.h265`; AV1 is raw OBUs, `ffplay -f obu file.obu` |
| `--duration <s>` | until Ctrl+C | stop after this many seconds |
| `--state-dir` | `~/.config/fernsicht` | this device's key and paired hosts |
| `--loss <f>` | `0` | drop this fraction of incoming video packets (testing) |

The window's keys are on the [app page](app.md#a-session).

## Examples

```sh
# Host by hand, the monitor with real video (as root, see kms-capture.md)
sudo fernsicht-host-agent --capture kms --encoder auto --input --pair

# Client: find, pair, connect
fernsicht-client discover
fernsicht-client pair zentrale 482913
fernsicht-client zentrale --fps 120 --codec hevc

# Without a window, video into a file
fernsicht-client zentrale --headless --duration 10 --record ~/test.h265

# Loss on purpose: what FEC hides
fernsicht-client zentrale --loss 0.01 --duration 10
```

The overlay in the terminal:

```text
Glass-to-Glass 2,3 ms  (p95 3,0 ms, max 4,0 ms)
Capture 0,7 ms · Encode 0,2 ms · Netz 1,0 ms · Decode 0,3 ms · Anzeige 0,0 ms
Codec Synthetisch · Bildrate 60 fps · Bitrate 24 Mbit/s · Verlust (FEC) 1,0 % → 0 · RTT 0,1 ms
```

If keyframes drop (`RcvbufErrors` in `/proc/net/snmp`), raise the UDP buffers:

```sh
sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
```
