# KMS-Capture ausprobieren

Der Host-Agent kann den Bildschirm direkt über KMS abgreifen
(`--capture kms`). Er wartet auf den VBlank des Monitors, holt sich den
Framebuffer, den die Grafikkarte gerade anzeigt, und reicht ihn als DMA-BUF
an den Encoder weiter. Die GPU rechnet RGB → NV12 (BT.709) und skaliert auf
die Stream-Auflösung. Dabei wird kein einziges Pixel über die CPU kopiert.

Das funktioniert auch im Login-Bildschirm und im Gaming-Modus (gamescope)
und braucht keinen Bestätigungsdialog. Dafür verlangt der Kernel
**`CAP_SYS_ADMIN`**: Nur dann gibt er fremde Framebuffer heraus.

## Bauen (in der Distrobox)

```sh
distrobox enter fernsicht
cd ~/fernsicht
git pull
cargo build --release -p fernsicht-host-agent --features vaapi,kms
cargo build --release -p fernsicht-client --features vaapi,window
```

## Starten (direkt auf dem Host)

Bazzite bringt FFmpeg und libva selbst mit, deshalb laufen die fertigen
Programme ohne Box. Das ist auch der einfachste Weg zu den root-Rechten:
`sudo` auf dem Host ist echtes root, in der normalen Box nicht.

**Terminal 1 – Host-Agent**

```sh
cd ~/fernsicht
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi
```

In der Ausgabe steht, welcher Monitor erfasst wird, z. B.
`capturing 2560×1440 over KMS (Selection { plane: 71, crtc: 80, pipe: 1 })`.
Diese Zeile kommt erst, wenn sich ein Client verbindet.

Meldet ein Programm nach einem Bazzite-Update
`libavcodec.so.NN: cannot open shared object file`, hat sich die
FFmpeg-Hauptversion geändert: Box aktualisieren (`dev/setup.sh`) und neu
bauen.

**Terminal 2 – Client**

```sh
~/fernsicht/target/release/fernsicht-client 127.0.0.1:47800
```

Es öffnet sich ein Fenster mit dem Bild, im Titel steht die Latenz. Esc
schließt es, F11 schaltet auf Vollbild. Auf demselben Rechner zeigt das
Fenster den Bildschirm, auf dem es selbst liegt: ein Spiegel im Spiegel.
Das ist erwartet.

Ohne Fenster, mit Aufnahme in eine Datei:

```sh
./target/release/fernsicht-client 127.0.0.1:47800 --headless --duration 10 --record ~/fernsicht-test.h264
ffplay -framerate 60 ~/fernsicht-test.h264
```

Von einem zweiten Rechner aus geht es genauso, statt `127.0.0.1` die IP des
AMD-Rechners. Auf dem NVIDIA-Rechner den Client mit NVDEC bauen
(`--features nvidia,window`); er nimmt automatisch NVDEC, wenn VAAPI nicht
geht.

## Fernsteuern (Maus und Tastatur)

Mit `--input` nimmt der Host Maus und Tastatur des Clients an:

```sh
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi --input
```

**Achtung:** Es gibt noch keine Anmeldung. Solange `--input` an ist, kann
jeder, der den Port 47800 erreicht, auf diesem Rechner tippen. Nur im
eigenen Netz verwenden.

Im Client-Fenster gehen Maus und alle Tasten an den Host, auch Esc und F11.
Der Client selbst hört dann auf:

| Tasten | Wirkung |
|---|---|
| Strg+Alt+Shift+Q | Client beenden |
| Strg+Alt+Shift+F | Vollbild an/aus |

Mit `--view-only` schickt der Client nichts; dann beenden Esc und F11 wie
bisher. Verliert das Fenster den Fokus, lässt der Client alle Tasten los,
damit am Host nichts hängen bleibt.

Bei mehreren Monitoren liest der Host die Anordnung aus KDEs
`~/.config/kwinoutputconfig.json`, damit der Zeiger auf dem gestreamten
Monitor landet. In der Ausgabe steht `pointer input mapped to DP-1: …`.

## Optionen

| Option | Bedeutung |
|---|---|
| `--kms-card /dev/dri/card1` | Grafikkarte wählen (Standard: die erste mit aktivem Monitor) |
| `--kms-connector DP-1` | Monitor wählen. Namen: `ls /sys/class/drm` zeigt z. B. `card1-DP-1` → `DP-1` |
| `--max-width/--max-height` | Obergrenze der Stream-Auflösung; der Encoder skaliert den Monitor darauf |

## Wenn es nicht klappt

| Meldung | Ursache und Abhilfe |
|---|---|
| `KMS capture needs CAP_SYS_ADMIN` | Nicht mit `sudo` gestartet, oder in der Distrobox statt auf dem Host. Siehe oben. |
| `no display to capture` | Kein Monitor aktiv, oder die falsche Karte. Mit `--kms-card` bzw. `--kms-connector` wählen. |
| `connector DP-2 is not active (active: ["DP-1"])` | Den angezeigten Namen verwenden. |
| `the driver cannot import this DMA-BUF (…)` | Der Treiber kann das Puffer-Format nicht lesen. Bitte die ganze Zeile schicken. |
| `display is off (no framebuffer)` | Monitor im Standby. |
| Session wird abgelehnt (Client wartet auf Antwort) | Die Fehlermeldung steht im Terminal des Host-Agents. |

## Bekannte Grenzen

- **Mauszeiger:** Er liegt auf einer eigenen Hardware-Ebene (Cursor-Plane)
  und ist deshalb nicht im Bild. Der Host liest ihn dort aus und schickt
  Position und Bild extra mit, der Client zeichnet ihn darüber. Zeichnet
  der Compositor den Zeiger selbst ins Bild (Software-Cursor), ist er
  ohnehin im Video.
- **Overlays fehlen.** Ebenso nur die Primär-Ebene; gamescope legt die
  Steam-Overlays teils auf eigene Ebenen.
- **Root-Prozess.** Der ganze Host-Agent läuft vorerst als root. Das ist nur
  zum Testen im LAN gedacht. Später übernimmt ein kleiner privilegierter
  Helfer nur das Capture.
- **HDR** wird noch nicht getreu übertragen. 10-Bit-Desktops (KDE auf AMD
  nutzt `AB30`) gehen, werden aber auf 8 Bit H.264 heruntergerechnet.
- Möglich ist minimales **Tearing**, falls der Compositor in den gerade
  gelesenen Puffer schreibt (wie bei Sunshine).

## Wie es getestet wird

- Auswahl von Karte, Monitor und Ebene sowie das VBlank-Raster laufen als
  Unit-Tests ohne Hardware (`cargo test -p fernsicht-capture --features kms`).
- Der Weg DMA-BUF → VAAPI läuft in CI auf dem AMD-Runner. Statt eines
  KMS-Framebuffers dient ein exportiertes VAAPI-Bild als DMA-BUF, der Rest ist
  identisch. Geprüft werden:
  - Farbtreue nach BT.709 an sechs Farbfeldern, für alle acht Formate
    (8 und 10 Bit, RGB- und BGR-Reihenfolge, mit und ohne Alpha);
  - Skalierung von 1440p auf 1080p;
  - Wechsel zwischen CPU- und DMA-BUF-Eingang;
  - Ablehnung kaputter Puffer;
  - die Encode-Zeit.
- Das eigentliche KMS-Capture braucht einen Monitor und root und läuft
  deshalb nicht im Runner, sondern so wie oben beschrieben von Hand.

## Alternative: rootful Distrobox

Falls die Programme auf dem Host einmal nicht starten (andere
FFmpeg-Version), geht es auch über eine zweite, rootful Box, in der `sudo`
echtes root ist: einmal `dev/setup.sh --root`, dann
`distrobox enter --root fernsicht-root` und dort wie oben mit `sudo`
starten. Beim ersten Betreten fragt die Box nach einem neuen Passwort nur
für `sudo` in dieser Box.
