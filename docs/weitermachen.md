# Stand und nächste Schritte

Übergabe für die nächste Sitzung, Mensch oder Agent. Stand: 10.10.2026,
Commit `ed802fd`, CI und GPU-Runner grün.

## Die Rechner

| | zentrale | bazzite |
|---|---|---|
| Rolle | Host (wird ferngesteuert), Entwicklungsrechner | Client |
| GPU | AMD RX 7800 XT (VAAPI) | NVIDIA GTX 1080, Treiber 580 (NVENC/NVDEC) |
| Netz | Kabel, 192.168.178.87 | WLAN |
| System | Bazzite 44, KDE Wayland, zwei Monitore 2560×1440@165 (DP-2 links, DP-1 rechts) | Bazzite |
| Repo | `~/fernsicht` | `~/Projects/fernsicht` |

- **Bauen:** in der Distrobox `fernsicht` (Image: `dev/Containerfile`).
- **Ausführen:** Die Programme aus der Box laufen direkt auf Bazzite.
  FFmpeg 8, libva und WebKitGTK 4.1 bringt Bazzite mit.
- **Self-hosted Runner:** Beide Rechner haben einen für die GPU-Tests
  (`.github/workflows/gpu.yml`, [gpu-runner.md](gpu-runner.md)).

## Was fertig ist

Ziel des letzten Abschnitts: Fernsicht wie ein normaler Benutzer
verwenden. Alle fünf Schritte sind gebaut, getestet und gepusht:

1. **Kopplung und Verschlüsselung.**
   - Einmalig per 6-stelliger PIN (SPAKE2).
   - Jede Sitzung mit Noise-IK-Handshake, danach alles versiegelt.
   - Nur gekoppelte Geräte kommen herein. Code: `crates/secure`.
2. **Host als Dienst.**
   - systemd-Unit `packaging/fernsicht-host.service`.
   - Steuer-Socket `/run/fernsicht/control.sock` mit den Befehlen
     `fernsicht-host-agent pair | status | unpair`.
   - Encoder passend zur GPU (`--encoder auto`).
   - Code: `apps/host-agent/src/control.rs`.
3. **Gerätesuche.**
   - `fernsicht-client discover`: Broadcast über den Stream-Port 47800.
     Bewusst kein mDNS: Die Firewall der zentrale sperrt 5353.
   - Hosts antworten mit Name, Schlüssel, OS, GPU, Kopplung offen und
     belegt.
   - Code: `apps/client/src/discover.rs`, Pakete `Discover`/`Announce`
     in `crates/proto`.
4. **Die App.**
   - Tauri um die Client-UI, in `apps/desktop`. Das ist ein eigener
     Cargo-Workspace mit eigenem CI-Job.
   - Sie listet Rechner, koppelt und startet Sitzungen.
   - Das Bild läuft im nativen Fenster von `fernsicht-client --app`. Der
     Client gibt das Overlay als JSON aus und endet, wenn stdin
     schließt.
   - „Dieser Rechner“ öffnet die Kopplung am eigenen Host und zeigt die
     PIN.
   - Die UI (`apps/client-ui`) nutzt in Tauri die echten Befehle, im
     Browser Demo-Daten.
5. **Installation.**
   - `packaging/build.sh` baut alles in der Box.
   - `packaging/install-app.sh` installiert App und Client nach
     `~/.local` samt Startmenü-Eintrag, ohne sudo.
   - `packaging/install-host.sh` richtet den Dienst ein und braucht
     sudo.
   - Anleitung: [install.md](install.md).

Danach kam dazu:

6. **Web-Viewer im LAN.**
   - Der Host liefert die Seite selbst (`--web`, Standard `0.0.0.0:47800`
     TCP; `--web-root`, installiert nach `/usr/local/share/fernsicht/viewer`).
   - Zugang mit der Kopplungs-PIN, einmalig.
   - WebRTC mit str0m: H.264 und Opus, dazu ein Datenkanal (Mauszeiger als
     Protokoll-Pakete und Statistik zum Browser, Eingaben als JSON zum
     Host).
   - Code: `apps/host-agent/src/web.rs`, `web/viewer/src/lib/host.ts`,
     `web/viewer/src/routes/live.tsx`.
   - Getestet: mit headless Chrome (in der Box) gegen einen echten Host
     mit VAAPI-Testbild, 1920×1080 bei 60 fps, Glass-to-Glass laut
     Overlay ≈ 15 ms auf localhost.

7. **Gaming-Modus, Gamepads, Bitratenanpassung, App-Einstellungen.**
   - Nativer Client: `--gaming` (Klick fängt den Zeiger, relative
     Bewegung), Strg+Alt+Shift+M fängt und lässt los.
   - Gamepads: Der Client liest Controller direkt über evdev
     (`crates/input/src/gamepad.rs`). Der Browser nutzt die Gamepad-API.
     Der Host legt pro Spieler einen virtuellen Xbox-360-Controller an
     (`crates/input/src/uinput.rs`). Tasten gehen nach Position (A
     unten, Y oben); die X/Y-Vertauschung des `xpad`-Treibers wird auf
     beiden Seiten ausgeglichen.
   - Bitrate: `crates/net/src/rate.rs`. Runter bei Frames, die trotz
     FEC verloren gehen, bei überlastetem Sender oder mehr als 20 %
     Verlust. Hoch nach 5 s sauberem Netz. VAAPI/NVENC bekommen dafür
     einen neuen Encoder (ein Keyframe). Getestet mit VAAPI und 10 %
     künstlichem Verlust.
   - App: Seite „Einstellungen“ (Auflösung, Bildrate, Bitrate),
     „Gerät vergessen“, Ton und Modus in der Sitzung gehen über stdin an
     den Client (`mute`, `unmute`, `gaming`, `desktop`).

8. **Ein AppImage für alles.**
   - `packaging/appimage.sh` baut lokal: App, Stream-Fenster, Host und
     Web-Viewer in einer Datei. Die GPU-Bibliotheken (libva, libdrm,
     Vulkan-Loader) werden herausgenommen; dafür sorgt `appimagetool`.
   - In der App: „Diesen Rechner freigeben“ (`apps/desktop/src/share.rs`)
     entpackt das AppImage per `pkexec` nach `/opt/fernsicht/app` und
     richtet `fernsicht-host.service` ein. Dazu „Freigabe beenden“ und
     „Host aktualisieren“.
   - Release wie in den anderen Repos: **Bump version**
     (`firsttris/workflows`), dann `release.yml` (Checks, AppImage auf
     Ubuntu 24.04, GitHub-Release).
   - Lokal getestet: Die Programme aus dem AppImage streamen auf Bazzite
     mit den System-Treibern (VAAPI H.264). Das App-Fenster zeigt
     „Diesen Rechner freigeben“.
   - Noch nicht live getestet: das Einrichten mit Passwort-Dialog.

9. **AV1 und HEVC.** Jede Verbindung nimmt das Beste, das beide Seiten in
   Hardware können (AV1, HEVC, H.264); der Web-Viewer AV1, wenn der
   Browser es anbietet, sonst H.264. Einstellung „Videoformat“ in der App,
   `--codec` am Client. Details und Messungen in
   [performance.md](performance.md#3--av1--hevc).
   Noch nicht live gesehen:
   - App auf der bazzite → zentrale: das Overlay sollte „HEVC“ zeigen.
   - Chrome/Firefox auf der bazzite → zentrale: „AV1“ (im Overlay des
     Web-Viewers; in Chrome auch unter `chrome://webrtc-internals`).

Gemessene Latenz (frühere Sitzungen):

| Strecke | Glass-to-Glass |
|---|---|
| Loopback | ≈ 9,4 ms |
| zentrale → bazzite, 1080p | 11,5 ms |
| zentrale → bazzite, 1440p | 16,3 ms (Encode 10 ms, weil die GPU zwischen Frames heruntertaktet) |

## Was noch niemand live gesehen hat

Diese Punkte sind automatisch getestet, aber nie auf echter Hardware
gelaufen. Sie brauchen den Benutzer am Rechner.

Live bestätigt (10.10.2026, laut Benutzer):

- Web-Viewer, auch mit Firefox und am Handy: Bild, Maus und Tastatur.
- bazzite (NVIDIA, Client) → zentrale (AMD, Host): Bild, Maus und
  Tastatur; das App-Fenster zeigt unter NVIDIA etwas (Punkt 3 erledigt).
- **Ton** ist dabei nicht geprüft worden, weder im Web-Viewer noch in
  der App.

1. **Dienst auf der zentrale.**
   - Vorher den alten Host stoppen, den der Benutzer in der Root-Box
     `fernsicht-root` gestartet hat. Er belegt UDP 47800, sonst startet
     der Dienst nicht.
   - Dann installieren:
     `./packaging/build.sh && sudo ./packaging/install-host.sh`.
   - Prüfen:
     - Bild über KMS als root ohne sudo-Sitzung.
     - Ton vom angemeldeten Benutzer (pulse als root über
       `/run/user/1000/pulse/native`).
     - Monitoranordnung (KWin-Config des Desktop-Benutzers).
     - Eingabe über uinput.
     - Log: `journalctl -u fernsicht-host -f`.
2. **App auf beiden Rechnern.**
   - Installieren: `./packaging/build.sh && ./packaging/install-app.sh`.
   - Auf der zentrale: „Gerät koppeln“.
   - Auf der bazzite: zentrale in der Liste, „Koppeln“, PIN eingeben,
     „Desktop“.
   - Prüfen:
     - Liste, Online-Status, PIN-Countdown.
     - Overlay in der App.
     - Trennen.
     - Schließen der App beendet die Sitzung.
3. ~~**App auf NVIDIA (bazzite).**~~ Erledigt, siehe oben. (Die App
   setzt `WEBKIT_DISABLE_DMABUF_RENDERER=1`, wenn `/proc/driver/nvidia`
   existiert; ohne bleiben die Fenster leer, tauri#9394.)
4. **Web-Viewer echt.** Auf der zentrale nach der Dienst-Installation
   von einem anderen Gerät `http://192.168.178.87:47800` öffnen, PIN, dann
   KMS-Bild, Ton, Maus und Tastatur prüfen. Auch mit Firefox und einem
   Handy.
5. **AppImage-Freigabe:** AppImage aus dem ersten Release auf der
   zentrale starten und „Diesen Rechner freigeben“ wählen (KDE fragt nach
   dem Passwort). Dann prüfen: Läuft `fernsicht-host` (`systemctl status
   fernsicht-host`)? Gibt es Bild, Ton und Eingabe? Danach „Freigabe
   beenden“ und noch einmal freigeben.
6. **Gaming-Modus und Gamepad.** In der App „Gaming“ wählen. Ein Klick
   ins Fenster fängt den Zeiger, Strg+Alt+Shift+M gibt ihn frei. Ein
   Spiel mit Maussteuerung prüfen. Ein Controller am Client erscheint
   auf dem Host als „Fernsicht X-Box 360 pad 1“: in Steam oder
   `evtest` prüfen, ob A unten und Y oben liegt. Bei Xbox- und
   PlayStation-Controllern.
7. **Bitratenanpassung über WLAN.** Auf der bazzite verbinden, im Log
   des Hosts nach `bitrate … Mbit/s` sehen. Normale WLAN-Verluste
   lassen die Bitrate stehen. Nur Frames, die trotz FEC verloren gehen,
   oder ein überlasteter Sender senken sie.
8. **Gerätesuche über WLAN.** Kommt der Broadcast von der bazzite bei
   der zentrale an, und die Antwort zurück? Die Antwort geht an einen
   kurzlebigen Port des Clients. Fedoras Zone `FedoraWorkstation`
   erlaubt UDP 1025–65535; auf der bazzite prüfen
   (`firewall-cmd --list-all`). Zur Not fragt die App gekoppelte Hosts
   auch direkt.
9. **Systemtasten an den Host.** Gebaut, nie live gesehen:
   - App: Im Vollbild oder mit gefangenem Zeiger bittet das Fenster den
     Desktop, seine Tastenkürzel durchzulassen (Wayland
     `keyboard-shortcuts-inhibit`, X11 Tastatur-Grab;
     `apps/client/src/shortcuts.rs`). KDE fragt beim ersten Mal. Prüfen:
     Meta, Meta+W, Alt+Tab und Strg+Alt+Entf kommen beim Host an; im
     normalen Fenster bleibt Alt+Tab lokal; Strg+Alt+Shift+F/M/Q gehen
     weiter.
   - Web-Viewer: Vollbild-Knopf in der Toolbar; Chrome/Edge sperren dann
     die Tastatur (Keyboard Lock, Esc gedrückt halten zum Verlassen).
   - Beide: Menü „Tasten senden“ (Tastatur-Symbol in der Toolbar) mit
     Strg+Alt+Entf, Windows-Taste, Windows+W, Alt+Tab, Alt+F4, Druck.

## Offene Aufgaben, nach Wichtigkeit

Die Performance-Punkte (AV1/HEVC, sofort anzeigen, Slices, Intra-Refresh,
Staukontrolle, 4:4:4) mit Nutzen und Aufwand stehen gesammelt in
[performance.md](performance.md).

1. **Die Live-Tests oben**, dann Fehler beheben, die dabei auftauchen.
2. **GPU-Hochtakten messen.** Gebaut ist es: Der Host setzt während
   einer Sitzung `power_dpm_force_performance_level` auf `high`
   (`apps/host-agent/src/power.rs`, Schalter in der App unter
   „Einstellungen“). Encode bei 1440p war vorher 10 ms. Jetzt auf der
   zentrale mit dem Host als Dienst (root) messen, einmal mit und einmal
   ohne (`--no-gpu-boost` oder der Schalter), und das Encode im Overlay
   vergleichen. Hilft `high` nicht genug, `pp_power_profile_mode`
   probieren (z. B. Profil VR).
3. **Bildschärfe bei nativer Auflösung** prüfen (der Benutzer fand das
   Bild bei 1080p über WLAN nicht ganz scharf). Bitrate und
   Keyframe-Qualität.
4. **Referenzmessung mit Sunshine/Moonlight**
   ([latency-baseline.md](latency-baseline.md)).
5. **Spiele, Rest:** Rumble (Force Feedback zurück zum Controller),
   mehr als 4 Controller.
6. **NVIDIA als Host: live prüfen.** Die Bildschirmaufnahme mit NVENC ist
   gebaut. Der Weg: KMS-DMA-BUF → Vulkan-Compute (RGB → NV12, BT.709,
   skaliert) → von CUDA importierter Speicher → NVENC. Code:
   `crates/gpu/src/convert.rs`, `crates/codec/src/{nvidia,cuda}.rs`.
   - Auf der bazzite getestet, mit Testbildern im NVIDIA-Kachel-Layout:
     2,5 ms pro 1080p-Frame, keine Xid-Meldungen.
   - Noch nie live gelaufen: echtes KMS auf der bazzite (HDMI-A-1). Der
     Benutzer startet
     `sudo ./target/release/fernsicht-host-agent --capture kms --encoder nvenc --pair`
     auf der bazzite, die zentrale verbindet sich als Client.
7. **Kleinigkeiten in der App:** Die Toolbar-Knöpfe Bildschirm,
   Zwischenablage und Dateien tun noch nichts (Ton und Modus schon).
8. **Bitratenanpassung im Web-Viewer:** WebRTC liefert Bandbreitenschätzung
   (TWCC), der Host nutzt sie noch nicht; dort bleibt die Bitrate fest.
9. **Später:** Internet (NAT, Rendezvous-Server), PipeWire-Capture. Web-Viewer über das Internet (Rendezvous, TURN),
   Browser merken statt jedes Mal eine PIN, Test mit echtem Chrome in der
   CI.

## Regeln für die Arbeit

- **Nie ohne ausdrückliches OK** Maus oder Tastatur bewegen, Ton
  abspielen oder Fenster öffnen auf einem Desktop, an dem der Benutzer
  sitzt. Vorher fragen und auf die Antwort warten. Ausnahme: ein
  virtuelles Display. Auf der zentrale hat die Box Xvfb, xdotool und
  ImageMagick, siehe unten.
- **Befehle mit root** (`sudo`, `pkexec`) führt der Benutzer selbst aus.
  Nicht umgehen.
- **Fremde Prozesse** des Benutzers (z. B. seinen laufenden Host) nicht
  beenden. Für Tests andere Ports nehmen.
- **Pushen** per SSH, auf beide Branches:
  `git push git@github.com:firsttris/fernsicht.git HEAD:main` und
  `HEAD:ccr-a9432ae9-5iz8to`. Danach CI prüfen (`gh run list`).
- **Sprache:** Doku und Oberfläche auf Deutsch, Code, Kommentare und
  Commit-Nachrichten auf Englisch.

## Prüfen, ob alles stimmt

In der Box, im Repo:

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
pnpm install && pnpm format:check && pnpm -r typecheck && pnpm test
pnpm --filter @fernsicht/client-ui build     # die App braucht die gebaute UI
cd apps/desktop && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
shellcheck packaging/*.sh
```

Der Testaufbau ist in [testing.md](testing.md) beschrieben (Fuzzing,
Coverage-Grenze 90 %, GPU-Tests).

## Die App ohne echten Desktop ansehen

So bleibt der Bildschirm des Benutzers frei: virtuelles X-Display, kein
Wayland, Ton ins Leere.

```sh
Xvfb :99 -screen 0 1280x800x24 &
env -u WAYLAND_DISPLAY DISPLAY=:99 GDK_BACKEND=x11 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
  LIBGL_ALWAYS_SOFTWARE=1 PULSE_SERVER=unix:/nonexistent \
  XDG_CONFIG_HOME=$(mktemp -d) FERNSICHT_CLIENT=$PWD/target/debug/fernsicht-client \
  apps/desktop/target/debug/fernsicht &
sleep 8; xwd -root -display :99 | magick xwd:- target/shots/app.png
```

Wichtig dabei:

- **Test-Host:** Ein Test-Host auf einem freien Port (z. B.
  `--bind 127.0.0.1:47801 --pair`) wird nicht per Broadcast gefunden.
  Vorher mit derselben `XDG_CONFIG_HOME` per
  `fernsicht-client pair 127.0.0.1:47801 PIN` koppeln, dann fragt die
  App ihn direkt.
- **Klicken:** `DISPLAY=:99 xdotool mousemove X Y click 1`.
- **Ablage:** Screenshots nach `target/shots` legen. Der
  `/tmp`-Ordner der VS-Code-Flatpak-Sitzung ist auf dem Host nicht
  sichtbar.
