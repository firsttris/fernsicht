# Installing Fernsicht

## With the AppImage (recommended)

One file contains everything: the app, the stream window, the host and the
web viewer.

1. Download the file `Fernsicht-<Version>-x86_64.AppImage` from the
   [Releases](https://github.com/firsttris/fernsicht/releases) page, make it
   executable (file manager: Properties › Permissions, or
   `chmod +x Fernsicht-*.AppImage`) and start it.
2. **On the computer you want to control:** in the bottom left, under
   “Dieser Rechner” (this computer), click **“Diesen Rechner freigeben”**
   (share this computer). This asks for the administrator password once.
   - After that, the host runs as a system service (`fernsicht-host`). It
     starts at boot and runs independently of the app. It is installed in
     `/opt/fernsicht`.
   - **“Freigabe beenden”** (stop sharing) removes it again. Keys and
     pairings in `/var/lib/fernsicht` are kept.
   - If you later start a newer AppImage, the app offers to update the
     host.
3. **On the computer you connect from:** start the same AppImage. The
   shared computer appears in the list. Then pair (see section 4, “Pair and
   connect”) and connect.

The AppImage bundles its own libraries (FFmpeg, WebKit). It takes the
graphics drivers (Mesa/VAAPI, NVIDIA, Vulkan) from the system, so that
hardware encoding matches the installed drivers.

## From source

There are two parts:

- **The app** on every computer you connect *from*. It finds the
  computers on the network, pairs with them and opens sessions.
- **The host service** on every computer you connect *to*. It starts with
  the computer and runs in the background.

A computer can have both.

## 1. Build

Do this once per computer (or after a `git pull`). The script runs in the
Distrobox `fernsicht` and enters it by itself:

```sh
cd ~/fernsicht
./packaging/build.sh
```

Bazzite ships FFmpeg, libva and WebKitGTK itself. The programs built in the
box therefore run directly on the system.

## 2. Install the app

Without `sudo`, for your own user:

```sh
./packaging/install-app.sh
```

After that, **Fernsicht** appears in the start menu. The script puts the
app and the client into `~/.local/bin` and sets up the start menu entry and
the icon. With `--uninstall` it removes both again.

## 3. Install the host service

On the computer you want to control remotely:

```sh
sudo ./packaging/install-host.sh
```

```text
Fernsicht-Host läuft. Gerät koppeln: fernsicht-host-agent pair
```

The script does the following:

- It copies the program to `/usr/local/bin`.
- It sets up the `fernsicht-host` service ([unit](https://github.com/firsttris/fernsicht/blob/main/packaging/fernsicht-host.service)) and starts it.
- If firewalld is running and UDP port 47800 is closed, it opens the port.

Updating works the same way: build again and run the scripts again.

This is how the service runs:

- **Video:** The service captures the monitor over KMS. For this it runs as root.
- **Encoder:** The service picks the encoder that matches the graphics card (`--encoder auto`): VAAPI on AMD/Intel, NVENC on NVIDIA. Both take the screen straight from the GPU (KMS capture, no CPU copy); on NVIDIA the picture goes from KMS through Vulkan to CUDA ([KMS capture](kms-capture.md)). So a computer with an NVIDIA card works as a host as well as a client.
- **Mouse and keyboard:** The service accepts input from paired devices (`--input`).
- **Audio:** The service takes the audio and the monitor layout from the logged-in user. So it captures what runs on that user's desktop.
- **Graphics card:** During a session the service keeps the graphics card
  at full clock speed (AMD: `power_dpm_force_performance_level` set to
  `high`, Intel: minimum clock raised). Otherwise the card would clock down
  between two frames, and encoding would take longer. Afterwards the
  service restores everything. To turn this off: in the app under
  “Einstellungen › Dieser Rechner als Host” (settings › this computer as
  host), or with `--no-gpu-boost`.

## 4. Pair and connect (in the app)

1. **On the host:** open Fernsicht and, in the bottom left under “Dieser
   Rechner” (this computer), click **“Gerät koppeln”** (pair device). The
   app shows a 6-digit PIN that is valid for 5 minutes.
2. **On the other computer:** open Fernsicht. The host appears in the
   list. Click **“Koppeln”** (pair) there and enter the PIN.
3. **Connect:** click **Desktop**. The video opens in its own window, and
   the app shows the latency.
   - <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>F</kbd> toggles full screen.
   - <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>Q</kbd> or **“Trennen”** (disconnect) in the app ends the session.

You only pair once per pair of devices. You can do the same in the
terminal, as the following sections show.

## In the browser

You can also connect from a browser, without installing anything, from any
computer, tablet or phone on the same network. See
[the web viewer page](web-viewer.md).

## Gaming

In the app, choose **Gaming** for a device:

- **Mouse pointer:** A click into the window captures the pointer. It then
  moves relatively, as games need it. <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>M</kbd>
  captures and releases the pointer, also in desktop mode.
- **Controllers** on the client computer are passed through automatically,
  up to four. On the host they appear as Xbox 360 controllers.
- **Settings** (the “Einstellungen” (settings) page): resolution, frame
  rate (up to 144) and a bitrate limit. The host lowers the bitrate by
  itself when the network cannot keep up.

## Find hosts on the network

On a client computer:

```sh
fernsicht-client discover
```

```text
zentrale  192.168.178.87:47800  Bazzite · Radeon RX 7700 XT / 7800 XT · H.264  gekoppelt
```

The client sends a broadcast query into the local network and also asks
the paired hosts directly. No extra port is needed for this; it runs over
the same UDP port 47800. If a paired host has a new address in the
meantime (DHCP), the client remembers the new one.

The answers are not authenticated. Only the key check during connection
verifies that the other end really is the paired host. So a fake entry in
the list cannot do any harm.

## Pair devices

On the host, without `sudo`:

```sh
fernsicht-host-agent pair
```

```text
Kopplung offen für 5 Minuten. PIN: 482913
Auf dem neuen Gerät: fernsicht-client pair <diese Adresse> <PIN>
```

On the new device, with the name from `discover` or the address:

```sh
fernsicht-client pair zentrale 482913
```

Only root and the user logged in at the desktop may pair. The service
checks this through the Unix socket (`/run/fernsicht/control.sock`). Other
users on the same computer are rejected.

## Overview and removing devices

```sh
fernsicht-host-agent status
```

```text
Host zentrale (Schlüssel 3f2a-…)
Verbunden: bazzite (192.168.178.20:51234), 2560×1440 bei 60 fps, verschlüsselt
Gekoppelte Geräte: 1
  bazzite  9c41-…
```

To remove a device:

```sh
fernsicht-host-agent unpair bazzite
```

After that, this device can no longer connect until it is paired again.

## Log, stop, uninstall

```sh
journalctl -u fernsicht-host -f        # follow the log
sudo systemctl stop fernsicht-host     # stop (starts again at the next boot)
sudo systemctl disable --now fernsicht-host   # stop and no longer start automatically
sudo ./packaging/install-host.sh --uninstall       # remove completely
```

Keys and pairings are stored in `/var/lib/fernsicht` and are kept when you
uninstall. If you delete the file `host.json` there, the host gets a new
key. All devices must then be paired again.

If the service reports
`libavcodec.so.NN: cannot open shared object file` after a Bazzite update,
the FFmpeg major version has changed. Then update the box
(`dev/setup.sh`), build again and install again.
