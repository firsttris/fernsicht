# KMS-Capture ausprobieren

Der Host-Agent kann den Bildschirm direkt über KMS abgreifen
(`--capture kms`). Er wartet auf den VBlank des Monitors, holt sich den
Framebuffer, den die Grafikkarte gerade anzeigt, und reicht ihn als DMA-BUF
an den Encoder weiter. Die GPU rechnet RGB → NV12 (BT.709) und skaliert auf
die Stream-Auflösung. Dabei wird kein einziges Pixel über die CPU kopiert.

Das funktioniert auch im Login-Bildschirm und im Gaming-Modus (gamescope)
und braucht keinen Bestätigungsdialog. Dafür verlangt der Kernel
**`CAP_SYS_ADMIN`**: Nur dann gibt er fremde Framebuffer heraus.

## Warum eine eigene Distrobox?

In der normalen (rootless) Distrobox ist `sudo` nur Root *im* Container. Für
den Kernel bist du dort weiterhin ein normaler Benutzer, und KMS-Capture
schlägt mit „KMS capture needs CAP_SYS_ADMIN“ fehl. Deshalb gibt es eine
zweite, **rootful** Box. Darin ist `sudo` echtes Root. Sie dient nur zum
Starten des Host-Agents; entwickelt wird weiter in `fernsicht`.

## Einrichten (einmalig, auf dem AMD-Rechner)

```sh
cd ~/fernsicht        # dein Klon
git pull
dev/setup.sh --root   # fragt nach deinem sudo-Passwort
```

Das baut das Dev-Image noch einmal im Speicher von root und legt die Box
`fernsicht-root` an.

## Ausprobieren

**Terminal 1 – Host-Agent**

```sh
distrobox enter --root fernsicht-root
cd ~/fernsicht
cargo build --release -p fernsicht-host-agent --features vaapi,kms
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi
```

In der Ausgabe steht, welcher Monitor erfasst wird, z. B.
`capturing 2560×1440 over KMS (Selection { plane: 71, crtc: 80, pipe: 1 })`.
Diese Zeile kommt erst, wenn sich ein Client verbindet.

**Terminal 2 – Client mit Aufnahme**

```sh
distrobox enter fernsicht
cd ~/fernsicht
cargo build --release -p fernsicht-client --features vaapi
./target/release/fernsicht-client 127.0.0.1:47800 --duration 10 --record ~/fernsicht-test.h264
```

Der Client zeigt jede Sekunde das Latenz-Overlay. Danach liegt in
`~/fernsicht-test.h264` das, was über die Leitung ging:

```sh
ffplay ~/fernsicht-test.h264     # in der Box
# oder auf dem Host: mpv/VLC mit der Datei öffnen
```

Von einem zweiten Rechner aus geht es genauso, statt `127.0.0.1` die IP des
AMD-Rechners. Der Client dekodiert aber noch per VAAPI. Auf dem
NVIDIA-Rechner geht das erst mit dem Vulkan-Client.

## Optionen

| Option | Bedeutung |
|---|---|
| `--kms-card /dev/dri/card1` | Grafikkarte wählen (Standard: die erste mit aktivem Monitor) |
| `--kms-connector DP-1` | Monitor wählen. Namen: `ls /sys/class/drm` zeigt z. B. `card1-DP-1` → `DP-1` |
| `--max-width/--max-height` | Obergrenze der Stream-Auflösung; der Encoder skaliert den Monitor darauf |

## Wenn es nicht klappt

| Meldung | Ursache und Abhilfe |
|---|---|
| `KMS capture needs CAP_SYS_ADMIN` | Nicht mit `sudo` gestartet oder in der rootless Box. Siehe oben. |
| `no display to capture` | Kein Monitor aktiv, oder die falsche Karte. Mit `--kms-card` bzw. `--kms-connector` wählen. |
| `connector DP-2 is not active (active: ["DP-1"])` | Den angezeigten Namen verwenden. |
| `DMA-BUF format XR30 is not supported yet` | Der Desktop läuft mit 10 Bit oder HDR. Zum Testen in den Anzeige-Einstellungen ausschalten. |
| `display is off (no framebuffer)` | Monitor im Standby. |
| Session wird abgelehnt (Client wartet auf Antwort) | Die Fehlermeldung steht im Terminal des Host-Agents. |

## Bekannte Grenzen

- **Mauszeiger fehlt.** Er liegt auf einer eigenen Hardware-Ebene
  (Cursor-Plane). Kommt mit der Eingabe in Phase 2.
- **Overlays fehlen.** Ebenso nur die Primär-Ebene; gamescope legt die
  Steam-Overlays teils auf eigene Ebenen.
- **Root-Prozess.** Der ganze Host-Agent läuft vorerst als root. Das ist nur
  zum Testen im LAN gedacht. Später übernimmt ein kleiner privilegierter
  Helfer nur das Capture.
- Möglich ist minimales **Tearing**, falls der Compositor in den gerade
  gelesenen Puffer schreibt (wie bei Sunshine).

## Wie es getestet wird

- Auswahl von Karte, Monitor und Ebene sowie das VBlank-Raster laufen als
  Unit-Tests ohne Hardware (`cargo test -p fernsicht-capture --features kms`).
- Der Weg DMA-BUF → VAAPI läuft in CI auf dem AMD-Runner. Statt eines
  KMS-Framebuffers dient ein exportiertes VAAPI-Bild als DMA-BUF, der Rest ist
  identisch. Geprüft werden:
  - Farbtreue nach BT.709 an sechs Farbfeldern;
  - Skalierung von 1440p auf 1080p;
  - Wechsel zwischen CPU- und DMA-BUF-Eingang;
  - Ablehnung kaputter Puffer;
  - die Encode-Zeit.
- Das eigentliche KMS-Capture braucht einen Monitor und root und läuft
  deshalb nicht im Runner, sondern so wie oben beschrieben von Hand.
