# Referenzmessung: Sunshine/Moonlight (Phase 0)

Die Messlatte für Phase 1 ist glass-to-glass < 20 ms bei 1080p60 im LAN. Sie
wird an derselben Szene mit Sunshine (vorinstalliert auf Bazzite) und
Moonlight gemessen, bevor Fernsicht gegen dieselbe Szene antritt.

## Aufbau

| | |
|---|---|
| Host | Bazzite, KDE Plasma (Wayland), Radeon RX 7800 XT |
| Client | [Gerät, OS, GPU] |
| Netz | [Kabel/WLAN, Switch, Link-Rate] |
| Monitor Client | [Modell, Bildwiederholrate] |
| Sunshine | [Version], Capture: KMS, Encoder: VAAPI H.264 |
| Moonlight | [Version], 1920×1080, 60 fps, [Bitrate] Mbit/s, V-Sync aus, Frame-Pacing aus |

## Methode

1. Auf dem Host eine Stoppuhr mit Millisekunden im Vollbild (z. B. eine
   Webseite mit `performance.now()`), daneben der Client-Monitor mit dem
   Stream.
2. Handy im Slowmo-Modus (240 fps, also 4,2 ms pro Bild) filmt beide
   Bildschirme gleichzeitig.
3. Pro Durchlauf 20 Einzelbilder auswerten: Differenz Host-Anzeige minus
   Client-Anzeige. Median und p95 notieren.
4. Zusätzlich die Statistik-Anzeige von Moonlight (Strg+Alt+Shift+S)
   abfotografieren: Netzwerk-Latenz, Decode-Zeit, Queue-Zeit.
5. Gleiches mit 1 % künstlichem Verlust:
   `sudo tc qdisc add dev <iface> root netem loss 1%` (auf dem Host),
   danach `sudo tc qdisc del dev <iface> root`.

## Ergebnisse

| Durchlauf | Datum | Median | p95 | Moonlight-Overlay (Netz / Decode) | Artefakte bei 1 % Verlust |
|---|---|---|---|---|---|
| Sunshine 1080p60 | [JJJJ-MM-TT] | [ms] | [ms] | [ms / ms] | [ja/nein] |
| Sunshine 1080p120 | | | | | |
| Fernsicht 1080p60 | | | | | |

## Fernsicht-eigene Messung

Der Client misst jede Stufe pro Frame (Capture, Encode, Netz, Decode,
Anzeige) über Zeitstempel im Paket-Header und eine NTP-artige
Uhren-Synchronisation:

```sh
# Host
fernsicht-host-agent --bind 0.0.0.0:47800
# Client, optional mit künstlichem Verlust
fernsicht-client <host-ip>:47800 --fps 60 --bitrate 20000 --loss 0.01 --duration 30
```

Das Overlay misst bis zur Übergabe an den Presenter. Die Scan-out-Zeit des
Monitors fehlt darin, deshalb bleibt die Handy-Slowmo-Messung die Abnahme.
