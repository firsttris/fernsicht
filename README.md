# Fernsicht

Self-hosted Remote-Desktop und Game-Streaming für Linux, in Rust. Ziel ist
die Latenz von Sunshine/Moonlight oder Parsec und der Komfort von
TeamViewer. Messlatte für Phase 1: glass-to-glass unter 20 ms im LAN bei
1080p60.

## Stand

| Phase | Inhalt | Stand |
|---|---|---|
| 0 – Fundament | Workspace, CI, Latenz-Messung pro Stufe, Uhren-Sync, Overlay, Distrobox | ✅ fertig. Die Sunshine-Referenzmessung steht noch aus ([Vorlage](docs/latency-baseline.md)) |
| 1 – Hot Path im LAN | Paketformat, FEC, Pacing, UDP, Slots, Threads | ✅ Transport fertig und getestet (inkl. 1 % Verlust ohne verlorenen Frame) |
| | VAAPI H.264 Encode/Decode | ✅ läuft auf dem AMD-Runner durch die ganze Pipeline: glass-to-glass ohne Bildschirm ≈ 10 ms (1080p60, Debug-Build) |
| | KMS-Capture → DMA-BUF → VAAPI ohne Kopie | ✅ implementiert; Import und GPU-Farbkonvertierung in CI getestet, KMS selbst von Hand ([Anleitung](docs/kms-capture.md)) |
| | Client-Fenster (Vulkan, winit) | ✅ VAAPI-Bild ohne Kopie in Vulkan (0,15 ms für Umrechnen + Zeichnen bei 1080p), Mailbox-Present, CPU-Rückfallweg; Render-Tests im CI mit llvmpipe |
| | NVIDIA: NVENC/NVDEC | ✅ Encoder und Decoder über FFmpeg/CUDA, getestet auf dem NVIDIA-Runner; Bilder gehen vorerst über die CPU (Bildschirmaufnahme auf NVIDIA fehlt noch) |
| | Mauszeiger | ✅ eigene Pakete (Position pro Frame, Bild bei Änderung, Wiederholung gegen Verlust); der Client zeichnet ihn über das Video. KMS liest die Cursor-Plane, das Testbild hat einen kreisenden Pfeil |
| | PipeWire-Capture | ⏳ offen |
| 2 – Steuerung | Maus und Tastatur | ✅ zuverlässig über UDP (Wiederholung bis zur Bestätigung, jedes Ereignis genau einmal, getestet bei 30 % Verlust), Host über `uinput` (`--input`, ohne root), absolute Zeigerposition auf den aufgenommenen Monitor umgerechnet (KDE-Monitoranordnung) |
| | Ton | ✅ was der Host abspielt (PipeWire), Opus 5 ms, jedes Paket trägt den Vorgänger mit (ein Verlust hinterlässt keine Lücke), Jitter-Puffer 15 ms mit Verlustverschleierung und Uhrdrift-Ausgleich; im Test 15 ms Verzögerung, bei 20 % Verlust 2,5 % überbrückt |
| | Gamepad, Zeigerfang für Spiele | ⏳ offen |
| 3 – Sicherheit | Kopplung und Verschlüsselung | ✅ einmalig koppeln per 6-stelliger PIN (SPAKE2: kein Offline-Raten, Kopplung schließt nach 3 Fehlversuchen), jede Sitzung mit Noise-IK-Handshake (wie WireGuard), danach alles mit ChaCha20-Poly1305 versiegelt, Wiederholungen werden verworfen; nur gekoppelte Geräte kommen herein |
| | Internet (NAT), Bitratenanpassung | ⏳ offen |
| 4–5 | Host als Dienst | ✅ systemd-Dienst mit Installationsskript, Encoder passend zur Grafikkarte, Koppeln/Status/Entfernen über einen lokalen Steuer-Socket ([Anleitung](docs/install.md)) |
| | Gerätesuche | ✅ `fernsicht-client discover`: Broadcast über den Stream-Port (keine Firewall-Änderung), Hosts nennen Name, Schlüssel, OS, GPU und ob Kopplung offen ist; neue Adressen gekoppelter Hosts werden übernommen |
| | Desktop-App | ✅ Tauri um die Client-UI: Rechner im Netz, Koppeln per PIN, Sitzung starten (Bild im nativen Vulkan-Fenster, Latenz-Overlay in der App), „Dieser Rechner" öffnet die Kopplung am eigenen Host ([apps/desktop](apps/desktop)) |
| | Installer, Web-Viewer | ⏳ offen |
| UI | Client-UI und Web-Viewer nach Mockup (React, TanStack, shadcn/ui) | ✅ Oberflächen mit Demo-Daten |

Ohne GPU läuft die komplette Pipeline mit einem **Testbild** und einem
**synthetischen Codec**. Der erzeugt Frames in realistischer Größe für die
eingestellte Bitrate und prüft sie per Checksumme. So messen CI und
Rechner ohne GPU Transport, FEC, Pacing und Latenz trotzdem echt. Mit
`--encoder vaapi` (Feature `vaapi`) wird echtes H.264 gestreamt. Mit
`--capture kms` (Feature `kms`) kommt das Bild vom Monitor.

## Schnellstart

```sh
cargo build --release

# Host, beim ersten Mal mit --pair: zeigt eine PIN zum Koppeln
./target/release/fernsicht-host-agent --pair

# Client (zweites Terminal oder zweiter Rechner): Hosts im Netz finden …
./target/release/fernsicht-client discover
# … einmal koppeln (Name aus discover oder Adresse) …
./target/release/fernsicht-client pair <host> <PIN>
# … dann per Name oder Adresse verbinden
./target/release/fernsicht-client <host-name> --fps 60
# mit 1 % künstlichem Paketverlust
./target/release/fernsicht-client <host-name> --loss 0.01 --duration 10

# Als Dienst, der mit dem Rechner startet (docs/install.md)
cargo build --release -p fernsicht-host-agent --features vaapi,kms,nvidia
sudo ./packaging/install-host.sh
fernsicht-host-agent pair      # PIN für ein neues Gerät
fernsicht-host-agent status    # Verbindung, gekoppelte Geräte

# Echtes H.264 vom Monitor (AMD/Intel, als root: docs/kms-capture.md)
cargo build --release -p fernsicht-host-agent --features vaapi,kms
sudo ./target/release/fernsicht-host-agent --capture kms --encoder vaapi
cargo build --release -p fernsicht-client --features vaapi,window
./target/release/fernsicht-client <host-name>   # Fenster, Strg+Alt+Shift+Q beendet
# ohne Fenster, Video in eine Datei (ffplay -framerate 60 ~/test.h264)
./target/release/fernsicht-client <host-name> --headless --record ~/test.h264

# NVIDIA (z. B. GTX 1080): Client mit NVDEC, Host mit NVENC (vorerst Testbild)
cargo build --release -p fernsicht-client --features nvidia,window
cargo build --release -p fernsicht-host-agent --features nvidia
./target/release/fernsicht-host-agent --encoder nvenc
```

Der Client gibt jede Sekunde das Latenz-Overlay aus:

```text
Glass-to-Glass 2,3 ms  (p95 3,0 ms, max 4,0 ms)
Capture 0,7 ms · Encode 0,2 ms · Netz 1,0 ms · Decode 0,3 ms · Anzeige 0,0 ms
Codec Synthetisch · Bildrate 60 fps · Bitrate 24 Mbit/s · Verlust (FEC) 1,0 % → 0 · RTT 0,1 ms
```

Die App (Rechner im Netz, Koppeln, Sitzungen):

```sh
pnpm install && pnpm --filter @fernsicht/client-ui build
cargo build --release -p fernsicht-client --features vaapi,window   # das Stream-Fenster
cargo build --release --manifest-path apps/desktop/Cargo.toml        # die App
FERNSICHT_CLIENT=target/release/fernsicht-client apps/desktop/target/release/fernsicht
```

Die App sucht den Client neben sich, sonst im `PATH`. `FERNSICHT_CLIENT`
zeigt ihr den Weg, solange nichts installiert ist.

Oberflächen im Browser (mit Demo-Daten):

```sh
pnpm install
pnpm dev:client   # Client-UI auf http://localhost:1420
pnpm dev:viewer   # Web-Viewer auf http://localhost:5174
```

## Aufbau

```text
crates/  core · proto · net · capture · codec · render · input · audio
apps/    host-agent · client · client-ui
web/     ui · viewer
dev/     Distrobox-Container (Fedora) für Bazzite
docs/    Messprotokolle
```

| Crate | Inhalt |
|---|---|
| `core` | Slot mit Kapazität 1 (latest frame wins, Puffer-Recycling), monotone Uhr, Latenz-Statistik pro Stufe, Hot-Threads mit erhöhter Priorität |
| `proto` | UDP-Paketformat v1: Video-Shards mit Stufen-Zeitstempeln, Feedback, Clock-Ping/-Pong, Hello/Ack, Bye. Der Parser panict nie und allokiert nicht |
| `net` | Reed-Solomon-FEC (`reed-solomon-simd`) in Gruppen; Recovery-Shards pro Gruppe binomial aus der gemessenen Verlustrate (Gruppenausfall ≤ 10⁻⁵, mindestens 10 %), Reassembly mit Keyframe-Anforderung und harten Größengrenzen, Pacer, NTP-artiger Uhren-Sync, UDP-Sockets mit 4 MiB Puffer, Verlust-Simulation |
| `capture` | `FrameSource`-Trait, Testbild (NV12, bewegter Balken), DMA-BUF-Beschreibung, KMS-Capture im VBlank-Takt (Feature `kms`, pures Rust) |
| `codec` | `Encoder`/`Decoder`-Traits, synthetischer Codec, VAAPI H.264 über FFmpeg (Feature `vaapi`): DMA-BUF-Import ohne Kopie, RGB→NV12 und Skalierung per `scale_vaapi` |
| `render` | `Presenter`-Trait, Overlay-Formatierung |
| `input`, `audio` | Event-Typen, Traits, Duplikat-Filter (Phase 2) |

### Threading

```text
Host:   [capture] --slot(1)--> [encode] --fifo(2)--> [packetize + FEC + pacing + send]
        [control]  Hello/Ack, Clock-Pong, Feedback → FEC-Bemessung, Keyframe
Client: [network] --fifo(4)--> [decode] --latest wins--> [present]
```

Jede Stufe läuft auf einem eigenen OS-Thread. Nach dem Aufwärmen wird
nicht mehr allokiert: Frame-Puffer laufen über kleine Freilisten im Kreis.
„Latest frame wins“ gilt nur für *rohe* bzw. *dekodierte* Frames.
Komprimierte Frames hängen voneinander ab und gehen deshalb durch eine
kurze FIFO. Läuft die über, zeigt eine Lücke in der Frame-ID das an, und
ein Keyframe wird angefordert.

### Latenz-Messung

Der Host schreibt Capture-Zeitpunkt sowie die Abstände bis „Capture fertig“
und „Encode fertig“ in jeden Paket-Header. Der Client misst Ankunft,
Decode und Anzeige selbst und rechnet alles über den geschätzten
Uhren-Offset auf eine Zeitachse um. Für den Offset zählt die Probe mit der
kleinsten RTT aus den letzten 16. Die Stufen heißen wie im UI: Capture,
Encode, Netz, Decode, Anzeige.

## Entwicklung und Tests

```sh
# Rust: Lint, Unit-, Property-, Integrations- und E2E-Tests
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Web: Format, Typen, Unit-/Komponententests mit Coverage, Browser-E2E + axe
pnpm install
pnpm format:check && pnpm -r typecheck
pnpm test:coverage
pnpm test:e2e
```

Die Testebenen (Unit, Property, Integration, Protokoll-Konformität, E2E
über ein gestörtes Netz, Binaries, Soak, Fuzzing, Benchmarks, Browser-E2E,
Accessibility) und ihre CI-Jobs beschreibt [docs/testing.md](docs/testing.md).
CI läuft bei jedem Push. Nachts kommen 15 Minuten Fuzzing pro Target und
der Soak-Test dazu.

Auf Bazzite: `dev/setup.sh` (bzw. `dev/setup.sh --nvidia`) baut den
Dev-Container und legt die Distrobox an. `dev/gpu-check.sh` zeigt, was die
GPU kann, und testet Hardware-Encode und -Decode. Wie du einen Rechner als
selbst gehosteten GitHub-Runner für die GPU-Tests einrichtest, steht in
[docs/gpu-runner.md](docs/gpu-runner.md).

Bei Drops auf Keyframes (`RcvbufErrors` in `/proc/net/snmp`) die
UDP-Puffer anheben:

```sh
sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
```

## Nächste Schritte (Phase 1)

1. Sunshine-Referenz messen und in `docs/latency-baseline.md` eintragen.
2. NVIDIA ohne Kopie: Bildschirmaufnahme (DMA-BUF → CUDA) für NVENC,
   NVDEC-Bilder direkt in Vulkan.
3. PipeWire-Portal als zweites
   Capture-Backend.
4. Abnahme: 1080p60, glass-to-glass < 20 ms per Handy-Slowmo.

## Offene Entscheidungen

Lizenz, Name/Marke, erster Client (eigener vs. Moonlight-kompatibel),
Codec-Start (H.264 vs. AV1), Self-hosted only vs. gehosteter
Rendezvous-Dienst. Details stehen im Implementierungsplan.
