# Performance: wo es noch schneller und besser geht

Stand: 10.10.2026. Was Fernsicht heute macht, was es bringt und wo noch
Luft ist. Für den Überblick über offene Aufgaben siehe
[weitermachen.md](weitermachen.md).

## Wo wir stehen

| Strecke | Glass-to-Glass | Größter Posten |
|---|---|---|
| Loopback (ein Rechner) | ≈ 9,4 ms | – |
| zentrale → bazzite, 1080p, WLAN | 11,5 ms | Netz (WLAN) |
| zentrale → bazzite, 1440p, WLAN | 16,3 ms | Encode 10 ms (GPU taktet herunter) |
| GTX 1080, NVENC → NVDEC, ohne Bildschirm | ≈ 5 ms | – |

Das ist schon auf dem Stand der Technik:

- **Bildweg ohne Kopie:** KMS → DMA-BUF → Hardware-Encoder auf dem Host,
  Decoder → Vulkan auf dem Client. Kein Pixel geht über die CPU.
- **Encoder auf Latenz eingestellt:** keine B-Frames, kein Lookahead, CBR,
  Keyframes nur auf Anforderung.
- **Netz:** eigenes UDP-Protokoll mit Fehlerkorrektur (Reed-Solomon-FEC)
  statt Nachsenden, Pacing und Bitratenanpassung bei Stau.
- **Verschlüsselung:** Noise/ChaCha20, kostet praktisch keine Zeit.
- **Rust:** keine Pausen durch einen Garbage Collector, keine Allokationen
  im heißen Pfad.

Ob wir schneller sind als Sunshine/Moonlight, ist noch offen: Die
Vergleichsmessung fehlt ([latency-baseline.md](latency-baseline.md)).

## Was noch geht

| # | Maßnahme | Was es bringt | Aufwand | Stand |
|---|---|---|---|---|
| 1 | **Vergleichsmessung mit Sunshine/Moonlight** | Wir wissen, wo wir stehen und was sich lohnt | Klein, braucht den Benutzer am Rechner | Offen |
| 2 | **GPU während Sitzungen hochtakten** | Encode bei 1440p von 10 ms Richtung 3–4 ms (geschätzt) | Klein | Gebaut, Messung offen |
| 3 | **AV1 oder HEVC statt H.264** | Gleiche Qualität mit 30–50 % weniger Bitrate: deutlich schärfer, vor allem über WLAN. Latenz gleich | Mittel | **HEVC fertig** (Standard, wo beide es können); AV1 offen |
| 4 | **Sofort anzeigen statt auf den Bildaufbau warten** (Vulkan „Immediate“ im Gaming-Modus) | Bis zu einem Bildschirmtakt weniger: im Mittel ≈ 3 ms bei 165 Hz, ≈ 8 ms bei 60 Hz. Dafür Tearing | Klein bis mittel | Offen |
| 5 | **Slices: Bildteile senden, bevor das Bild fertig ist** (wie Parsec) | Mehrere Millisekunden pro Bild, weil Kodieren, Senden und Dekodieren überlappen | Groß | Offen |
| 6 | **Intra-Refresh statt Keyframes** | Keine großen Datenstöße nach einem Verlust, also weniger Ruckler über WLAN | Mittel | Offen |
| 7 | **Verzögerungsbasierte Staukontrolle** (wie WebRTC) | Reagiert auf wachsende Laufzeit, bevor Pakete verloren gehen. Besser über WLAN | Mittel | Offen (heute: Verlust-basiert) |
| 8 | **4:4:4-Farbe für den Desktop** | Gestochen scharfe Schrift (heute wird Farbe in halber Auflösung übertragen) | Mittel | Offen, nur mit NVENC (H.264/HEVC). AMD und Intel kodieren kein 4:4:4 |
| 9 | **Bitratenanpassung im Web-Viewer** (WebRTC-Bandbreitenschätzung, TWCC) | Der Browser bekommt nicht mehr, als das Netz trägt | Mittel | Offen (heute feste Bitrate) |
| 10 | **Kabel statt WLAN** | Weniger Latenz und Schwankung. Keine Software-Sache | – | Empfehlung |

## Zu den einzelnen Punkten

### 3 · AV1 / HEVC

AV1 ist der modernste der drei Codecs, HEVC liegt dazwischen. Beide
brauchen für dieselbe Qualität deutlich weniger Bitrate als H.264. Die
Latenz bleibt gleich, weil die Hardware-Encoder ähnlich schnell sind.

Wer was kann:

| Grafikkarte | Kodieren (Host) | Dekodieren (Client) |
|---|---|---|
| AMD RX 7800 XT (zentrale, VCN 4) | H.264, HEVC, **AV1** | H.264, HEVC, AV1 |
| NVIDIA GTX 1080 (bazzite, Pascal) | H.264, HEVC | H.264, HEVC (**kein AV1**) |
| NVIDIA ab RTX 30, AMD ab RX 6000, Intel Arc | – | AV1 |
| NVIDIA ab RTX 40, AMD ab RX 7000, Intel Arc | AV1 | – |

Deshalb müssen Host und Client aushandeln, was beide können:

- Der Client sagt im Verbindungsaufbau, was er dekodiert.
- Der Host nimmt das Beste, das er kodieren kann.

Für zentrale → bazzite heißt das HEVC. AV1 ginge erst mit einer neueren
Karte im Client. Im Web-Viewer entscheidet der Browser: Chrome dekodiert
AV1 und meist auch HEVC.

**HEVC ist gebaut** (10.10.2026):

- Der Client meldet im `Hello`, was er in Hardware dekodiert (VAAPI:
  `vaQueryConfigProfiles`, NVIDIA: `cuvidGetDecoderCaps`). Der Host nimmt
  HEVC vor H.264 und fällt auf H.264 zurück, wenn seine GPU kein HEVC
  kodiert. Ältere Clients und Hosts sprechen weiter H.264.
- Wählbar in der App („Einstellungen → Videoformat“) und mit
  `fernsicht-client --codec auto|h264|hevc`.
- Der Web-Viewer bleibt bei H.264 (WebRTC in Firefox kann kein HEVC).

Gemessen auf der GTX 1080 (NVENC → NVDEC, 1080p60):

| | H.264 | HEVC |
|---|---|---|
| 4 Mbit/s: Ø Bytes pro Frame | 8.273 | 2.681 |
| 4 Mbit/s: PSNR (Y), schlechtester Frame | 29,1 dB | 32,4 dB |
| 20 Mbit/s, Loopback: Glass-to-Glass ohne Bildschirm | 5,0 ms | 4,1 ms |

Die Zahlen für VAAPI (zentrale) stehen im Job-Summary des AMD-Runners.

### 4 · Sofort anzeigen

Heute zeigt das Stream-Fenster mit „Mailbox“ an: Das neueste Bild wird
beim nächsten Bildaufbau des Monitors gezeigt. Im Mittel wartet es einen
halben Takt.

„Immediate“ zeigt es sofort, auch mitten im Bildaufbau. Das spart diese
Wartezeit, verursacht aber Tearing (eine sichtbare Kante, wo altes und
neues Bild aufeinandertreffen). Für Spiele ist das üblich, für den Desktop
nicht. Deshalb nur im Gaming-Modus. Mit VRR/FreeSync verschwindet das
Tearing weitgehend.

### 5 · Slices

Der Encoder teilt das Bild in Streifen. Jeder fertige Streifen geht sofort
auf die Leitung, und der Client dekodiert ihn schon, während der Host noch
am nächsten arbeitet. Das spart einen guten Teil der Encode- und
Übertragungszeit.

Das braucht:

- Slice-Ausgabe im Encoder (NVENC kann sie gut, VAAPI eingeschränkt);
- eine Änderung im Protokoll (Pakete pro Slice statt pro Bild);
- einen Decoder, der Teile annimmt.

Das ist der größte Umbau in dieser Liste, aber auch der letzte große
Hebel.

### 6 · Intra-Refresh

Heute schickt der Host nach einem Verlust einen Keyframe, ein großes Bild,
das kurz viel Bandbreite braucht. Beim Intra-Refresh wird stattdessen über
einige Bilder verteilt Streifen für Streifen erneuert. Die Datenmenge
bleibt gleichmäßig, und es gibt keinen Ruckler durch einen Stoß.

### 7 · Staukontrolle

Heute senkt der Host die Bitrate, wenn Bilder trotz FEC verloren gehen
oder sein Sender nicht nachkommt (`crates/net/src/rate.rs`). Eine
verzögerungsbasierte Regelung achtet zusätzlich darauf, ob die Laufzeit
der Pakete wächst. Das ist das frühe Zeichen, dass sich irgendwo eine
Warteschlange füllt. Sie bremst dann, bevor überhaupt etwas verloren geht.

## Was sich nicht beschleunigen lässt

- **Der Monitor:** Ein Bild ist erst sichtbar, wenn er es zeichnet. Bei
  165 Hz sind das bis zu 6 ms, bei 60 Hz bis zu 16,7 ms (Punkt 4 holt
  davon einen Teil zurück).
- **Funk:** WLAN hat eigene Latenz und Schwankungen, das kann keine
  Software ganz ausgleichen.
- **Kodieren braucht Zeit:** Ein Bild muss zumindest teilweise kodiert
  sein, bevor es losgeht (Punkt 5 verkürzt das).

## Empfohlene Reihenfolge

1. **Vergleichsmessung** (1): Wissen, wo wir stehen.
2. **GPU-Hochtakten messen** (2): ist gebaut, nur noch messen.
3. **HEVC/AV1** (3): sichtbar besseres Bild.
4. **Sofort anzeigen im Gaming-Modus** (4): kleiner Umbau, spürbarer
   Gewinn.
5. **Intra-Refresh und Staukontrolle** (6, 7): für WLAN.
6. **Slices** (5): die letzten Millisekunden.

## So wird gemessen

- **Overlay:** Das Overlay in App und Browser zeigt jede Sekunde
  Glass-to-Glass und die einzelnen Stufen (Capture, Encode, Netz, Decode,
  Anzeige).
- **Messen:** immer bei gleicher Auflösung, Bildrate und gleichem Netz,
  vorher und nachher.
- **Ohne Bildschirm:** `fernsicht-client <host> --headless --duration 10`
  gibt dieselben Werte im Terminal aus.
