# GPU-Rechner einrichten: Distrobox und GitHub-Runner

Diese Anleitung richtet einen Bazzite-Rechner so ein, dass

1. du (oder Claude Code) dort in einer **Distrobox** mit Zugriff auf die GPU
   entwickeln kannst, und
2. ein **selbst gehosteter GitHub-Runner** die GPU-Tests aus CI auf echter
   Hardware ausführt, also VAAPI auf der Radeon RX 7800 XT und NVENC/NVDEC
   auf der GTX 1080.

Beide Rechner werden gleich eingerichtet. Nur der Schalter `amd` bzw.
`nvidia` unterscheidet sich. Pro Rechner dauert das etwa 15 Minuten.

## Wie es aufgebaut ist

```text
Bazzite (Host, bleibt unverändert)
├── Distrobox "fernsicht"            ← zum Entwickeln, teilt dein $HOME
│     Image: localhost/fernsicht-dev
└── Podman-Container "fernsicht-runner-amd|nvidia"  ← für CI, isoliert
      Image: localhost/fernsicht-runner (= dev-Image + GitHub-Runner)
      läuft als systemd-User-Dienst (Quadlet), startet mit dem Rechner
```

Warum zwei Container? Eine Distrobox teilt absichtlich dein ganzes
Home-Verzeichnis mit dem Host, inklusive `~/.ssh`, Browser-Profil und
Passwörtern. Das ist zum Entwickeln praktisch. Für einen Runner, der Code
aus einem **öffentlichen** Repo ausführt, wäre es ein Risiko. Der Runner
läuft deshalb in einem eigenen Podman-Container. Er sieht nur die GPU und
seine eigenen Volumes, keine Verzeichnisse vom Host.

## Voraussetzungen

| | AMD-Rechner (RX 7800 XT) | NVIDIA-Rechner (GTX 1080) |
|---|---|---|
| Bazzite-Image | normales Bazzite | Bazzite **mit geschlossenem NVIDIA-Treiber** (`bazzite-nvidia`, *nicht* `-open`: die offenen Kernel-Module unterstützen erst RTX 20xx und neuer) |
| Prüfen | `ls /dev/dri` zeigt `renderD128` | `nvidia-smi` zeigt die GTX 1080 |
| Zusätzlich | nichts | CDI-Spezifikation, siehe unten |

**Nur NVIDIA:** Podman reicht die GPU über CDI in Container. Prüfe, ob die
Spezifikation existiert:

```sh
nvidia-ctk cdi list        # muss "nvidia.com/gpu=all" enthalten
```

Falls nicht, einmalig erzeugen. Nach jedem NVIDIA-Treiber-Update wiederholen,
falls Bazzite es nicht selbst erledigt:

```sh
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
```

> Hinweis zur GTX 1080: NVIDIA hat den 580er-Treiberzweig als letzten mit
> Unterstützung für Pascal-Karten angekündigt. Für die Entwicklung reicht
> das. Die Karte kann H.264 und HEVC kodieren und dekodieren, aber kein AV1.

## Schritt 1: Repo klonen

Auf dem Host, in einem Terminal (Ptyxis/Konsole):

```sh
git clone https://github.com/firsttris/fernsicht.git ~/fernsicht
cd ~/fernsicht
```

## Schritt 2: Distrobox zum Entwickeln

```sh
dev/setup.sh            # AMD-Rechner
dev/setup.sh --nvidia   # NVIDIA-Rechner
```

Das baut das Image `localhost/fernsicht-dev` mit Rust, Node, VAAPI, Vulkan
und FFmpeg mit allen Codecs (aus RPM Fusion, wie Bazzite selbst) und legt
die Distrobox an. Danach:

```sh
distrobox enter fernsicht          # bzw. fernsicht-nvidia
cd ~/fernsicht
dev/gpu-check.sh                   # was kann die GPU?
cargo test --workspace             # die bestehende Testsuite
```

`dev/gpu-check.sh` zeigt GPU, Treiber, VAAPI- und Vulkan-Video-Fähigkeiten.
Dazu kodiert und dekodiert es zwei Sekunden 1080p60 in Hardware: Auf AMD läuft
das über VAAPI, auf NVIDIA über NVENC/NVDEC. Auf AMD sollten
`vaapi-h264-encode` und `vaapi-h264-decode` auf **PASS** stehen, auf NVIDIA
`nvenc-h264-encode` und `nvdec-h264-decode`.

In der Distrobox kannst du auch Claude Code starten. Es hat dort dieselbe
GPU zur Verfügung und kann Hardware-Code direkt testen.

## Schritt 3: Runner-Token holen

1. <https://github.com/firsttris/fernsicht/settings/actions/runners/new> öffnen
   (*Settings → Actions → Runners → New self-hosted runner*).
2. Bei „Configure“ steht ein Befehl mit `--token XXXXX`. Nur dieses Token
   kopieren. Den Rest der Seite brauchst du nicht, das Skript erledigt
   Download und Konfiguration.

Das Token ist eine Stunde gültig und wird nur einmal zum Registrieren
gebraucht.

## Schritt 4: Runner einrichten

**Auf dem Host**, nicht in der Distrobox:

```sh
cd ~/fernsicht
dev/runner/setup-runner.sh --gpu amd       # AMD-Rechner
dev/runner/setup-runner.sh --gpu nvidia    # NVIDIA-Rechner
```

Das Skript fragt nach dem Token und macht dann Folgendes:

1. Es prüft den GPU-Zugriff (Render-Node bzw. `nvidia-smi` und CDI).
2. Es baut `localhost/fernsicht-dev` und darauf `localhost/fernsicht-runner`
   mit der aktuellen Version des GitHub-Runners.
3. Es registriert den Runner einmalig mit den Labels
   `self-hosted, linux, gpu-amd` bzw. `gpu-nvidia`. Die Registrierung liegt
   im Volume `fernsicht-runner-<gpu>` und übersteht Image-Updates.
4. Es installiert den systemd-User-Dienst `fernsicht-runner-<gpu>` als
   Quadlet unter `~/.config/containers/systemd/`, aktiviert „Lingering“
   (der Dienst läuft auch ohne Anmeldung) und startet ihn.
5. Es führt `gpu-check.sh` im Runner-Container aus. Wenn das klappt, kommt
   die GPU auch in den CI-Jobs an.

## Schritt 5: Prüfen

- <https://github.com/firsttris/fernsicht/settings/actions/runners> sollte
  den Runner als **Idle** zeigen.
- Unter <https://github.com/firsttris/fernsicht/actions/workflows/gpu.yml>
  auf **Run workflow** klicken. Der Job `GPU · amd` bzw. `GPU · nvidia`
  läuft auf deinem Rechner. Seine Zusammenfassung zeigt dieselbe Tabelle
  wie `gpu-check.sh`.

Auf dem Rechner selbst:

```sh
systemctl --user status fernsicht-runner-amd
journalctl --user -u fernsicht-runner-amd -f
```

## Wann die GPU-Jobs laufen

Der Workflow `.github/workflows/gpu.yml` läuft

- bei Pushes auf `main`, die Code oder `dev/` ändern,
- jede Nacht (fängt Mesa- und Treiber-Updates von Bazzite ab),
- auf Knopfdruck (*Run workflow*).

Er läuft **nie** bei Pull Requests, auch nicht bei solchen aus Forks.

Ist ein Rechner aus, wartet sein Job in der Warteschlange. GitHub bricht ihn
nach 24 Stunden ab, du kannst ihn aber auch im Actions-Tab abbrechen. Die
normale CI läuft davon unabhängig weiter.

## Sicherheit

Das Repo ist öffentlich, und ein selbst gehosteter Runner führt Code aus
dem Repo auf deinem Rechner aus. Die Schutzmaßnahmen:

- **Wer Code starten kann:** Nur wer auf `main` pushen darf, also du. Pull
  Requests lösen den Workflow nicht aus, und der Job prüft zusätzlich das
  Repo und das Ereignis.
- **Was der Code sieht:** Der Container ist rootless. Root im Container ist
  dein Benutzer auf dem Host, aber ohne eingebundene Host-Verzeichnisse.
  Sichtbar sind nur die GPU und die Volumes `fernsicht-runner-*`.
- **Was du zusätzlich einstellen solltest:** Unter *Settings → Actions →
  General* bei „Fork pull request workflows from outside collaborators“
  **„Require approval for all external contributors“** wählen.
- **Bekannter Kompromiss:** SELinux-Labels sind für den Container
  abgeschaltet (`label=disable`), sonst blockiert SELinux den GPU-Zugriff.
  Das ist NVIDIAs dokumentierte Einstellung für CDI-Geräte. Die Isolation
  über rootless Podman und die fehlenden Host-Mounts bleibt bestehen.

## Betrieb

| Aufgabe | Befehl |
|---|---|
| Nach Bazzite-Update neu bauen | `dev/runner/setup-runner.sh --gpu amd` (registriert nicht neu) |
| Pausieren | `systemctl --user stop fernsicht-runner-amd` |
| Entfernen | `dev/runner/setup-runner.sh --gpu amd --remove`, dann den Runner auf GitHub löschen |
| Build-Cache leeren | `podman volume rm fernsicht-runner-cache` (Dienst vorher stoppen) |

Der Build-Cache (`/cache` im Container: Cargo-Registry und `target/`) macht
die Läufe nach dem ersten schnell.

## Bekannte Grenzen

- **KMS-Capture braucht `CAP_SYS_ADMIN` auf dem Host.** Im rootless Runner
  gibt es das nicht. Capture-Tests über KMS laufen daher in der Distrobox
  oder auf dem Host:
  `sudo setcap cap_sys_admin+p target/release/fernsicht-host-agent`.
  Encode, Decode und Vulkan funktionieren im Runner.
- **Echte Glass-to-Glass-Latenz** misst weiterhin ein Mensch mit
  Handy-Slowmo (siehe [latency-baseline.md](latency-baseline.md)). Der
  Runner misst die Stufen, nicht den Bildschirm.

## Fehlerbehebung

**„Kein Zugriff auf /dev/dri/renderD128“ (AMD).** Auf Fedora Atomic
(Bazzite) liegen Systemgruppen in `/usr/lib/group` und lassen sich nicht
direkt mit `usermod` ändern. So kommst du in die `render`-Gruppe, danach neu
anmelden:

```sh
ls -l /dev/dri/renderD128                       # Gruppe und Rechte ansehen
grep -E '^render:' /usr/lib/group | sudo tee -a /etc/group
sudo usermod -aG render "$USER"
```

**`vaapi-h264-encode` FAIL, aber die GPU ist da.** Dann ist im Image das
Mesa-VA ohne H.264 aktiv. Prüfen mit
`rpm -q mesa-va-drivers-freeworld` (muss installiert sein). Das Image neu
bauen mit `dev/setup.sh` bzw. `setup-runner.sh`.

**`nvidia-smi` im Container schlägt fehl.** Meist ist die CDI-Spezifikation
nach einem Treiber-Update veraltet:
`sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml`, dann
`systemctl --user restart fernsicht-runner-nvidia`.

**Runner auf GitHub „Offline“.** Prüfe `systemctl --user status
fernsicht-runner-<gpu>` und `loginctl show-user "$USER" | grep Linger`
(muss `Linger=yes` sein).

**Token abgelaufen.** Neues Token holen (Schritt 3) und das Skript erneut
ausführen. Ist der Runner noch nicht registriert, fragt es danach.
