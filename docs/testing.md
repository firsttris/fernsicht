# Teststrategie

Fernsicht wird auf mehreren Ebenen getestet. Jede Ebene beantwortet eine
andere Frage. Die unteren Ebenen sind schnell und präzise und laufen bei
jedem Push, die oberen prüfen das Zusammenspiel und laufen ebenfalls in CI,
teils nachts länger.

```text
                ┌──────────────────────────────┐
                │ Soak (nightly, 60 s Release) │  hält es eine Minute schlechtes Netz aus?
              ┌─┴──────────────────────────────┴─┐
              │ E2E: Rust-Szenarien + Binaries,  │  funktioniert das Produkt als Ganzes?
              │      Playwright + axe            │
            ┌─┴──────────────────────────────────┴─┐
            │ Integration: echte Sockets, Protokoll-│  halten sich Host und Client ans Protokoll?
            │ Konformität, Fake-Host, Komponenten   │
          ┌─┴──────────────────────────────────────┴─┐
          │ Property-Tests (proptest) + Fuzzing       │  gilt es für *alle* Eingaben?
        ┌─┴──────────────────────────────────────────┴─┐
        │ Unit-Tests in jedem Modul                     │  rechnet die Funktion richtig?
        └───────────────────────────────────────────────┘
```

## Rust

| Ebene | Wo | Was | Befehl |
|---|---|---|---|
| Unit | `#[cfg(test)]` in jedem Modul | Paketformat, FEC-Größen, Reassembly-Sonderfälle, Slot, Statistik, Overlay-Formatierung, Referenzkette | `cargo test --workspace --lib` |
| Property | `crates/*/tests/properties.rs` | Protokoll-Roundtrips und beliebige Bytes; FEC rekonstruiert bei *jedem* Verlustmuster im Budget exakt; feindliche Header; Statistik = naives Modell; Uhren-Sync exakt bzw. Fehler ≤ RTT/2; jede 1-Byte-Korruption wird erkannt | `cargo test --workspace --test properties` |
| Integration | `crates/net/tests/udp_transport.rs` | Packetizer → Pacer → echter UDP-Socket → Reassembler, mit und ohne FEC | `cargo test -p fernsicht-net` |
| Protokoll-Konformität | `apps/host-agent/tests/protocol.rs` | Ein roher UDP-Peer gegen den echten Host: Aushandlung, Keyframe-Anforderung, adaptive FEC, Bye, Timeouts, Session-Übernahme, Müll-Pakete, Zähler | `cargo test -p fernsicht-host-agent` |
| Client gegen Fake-Host | `apps/client/tests/fake_host.rs` | Skriptbarer Host: Hello-Wiederholung, Uhren-Offset von 5 s, Feedback, korrupte Frames, Bye, stummer Host | `cargo test -p fernsicht-client` |
| E2E-Szenarien | `tests/e2e/tests/scenarios.rs`, `sessions.rs` | Echter Host und Client über einen UDP-Proxy mit Verlust, Duplikaten, Reordering, Delay/Jitter und Funkloch. Prüft u. a., dass 8 ms Netz-Delay im Overlay als „Netz“ ankommen | `cargo test -p fernsicht-e2e` |
| E2E-Binaries | `tests/e2e/tests/binaries.rs` | Die ausgelieferten Programme: CLI, Fehlercodes, kompletter Lauf mit 1 % Verlust | `cargo test -p fernsicht-e2e --test binaries` |
| Soak | `tests/e2e/tests/soak.rs` | 60 s 1080p60 mit gemischten Störungen | `cargo test -p fernsicht-e2e --release --test soak -- --ignored` |
| Fuzzing | `fuzz/` | Parser, Reassembler, FEC-Roundtrip und Decoder mit libFuzzer | siehe [`fuzz/README.md`](../fuzz/README.md) |
| Benchmarks | `crates/net/benches/` | Packetize/FEC und Reassembly mit/ohne Recovery (criterion) | `cargo bench -p fernsicht-net` |
| GPU (echte Hardware) | `.github/workflows/gpu.yml` auf selbst gehosteten Runnern (`gpu-amd`, `gpu-nvidia`) | VAAPI/Vulkan-Video-Fähigkeiten und Testsuite auf der Zielmaschine. Auf NVIDIA: `crates/codec/tests/nvidia.rs` (NVENC → NVDEC: Qualität, CBR, Keyframes, Latenz, viele Sessions) und `tests/e2e/tests/gpu.rs` mit NVENC/NVDEC. Auf AMD zusätzlich: `crates/codec/tests/vaapi.rs` (Qualität, CBR, Keyframes, Latenz; DMA-BUF-Import mit BT.709-Farbfeldern und Skalierung) und `tests/e2e/tests/gpu.rs` (echtes H.264 durch Host → gestörtes Netz → Client, Stufen-Latenzen im Job-Summary). Läuft bei Pushes auf `main`, nachts und auf Knopfdruck | [docs/gpu-runner.md](gpu-runner.md) |
| Vulkan-Renderer | `crates/render/tests/vulkan.rs` | NV12 → RGB gegen Referenzfarben (Bild auslesen), Mauszeiger an der richtigen Stelle (auch bei skaliertem Video) mit vormultipliziertem Alpha, schwarze Ränder bei anderem Seitenverhältnis, Größenwechsel, fehlerhafte Bilder. Im CI auf llvmpipe (`FERNSICHT_REQUIRE_VULKAN=1`, kein stilles Überspringen); auf dem AMD-Runner zusätzlich VAAPI-Decoder → DMA-BUF → Vulkan ohne Kopie, verglichen mit dem CPU-Weg, Zeiten im Job-Summary; auf dem NVIDIA-Runner der CPU-Weg mit dem proprietären Treiber. Das Fenster selbst (Swapchain, Mailbox) von Hand | `cargo test -p fernsicht-render --features vulkan` |
| Mauszeiger | `crates/proto` (Pakete, Property-Tests, Fuzzing), `apps/client/src/cursor.rs` (Bildstücke zusammensetzen, Reihenfolge, Verlust), `apps/host-agent/tests/protocol.rs` (Form, Position, Wiederholung), `tests/e2e/tests/sessions.rs` (Host → Client, auch bei 50 % Verlust) | `cargo test -p fernsicht-e2e --test sessions` |
| Eingabe | `crates/proto` (Ereignisse, ungültige Codes, Property-Tests, Fuzzing), `crates/input` (Warteschlange: Wiederholung, Zusammenfassen, Loslassen; uinput-Ereignisformat, Monitor-Umrechnung), `apps/host-agent/src/layout.rs` (KWin-Monitoranordnung), `apps/host-agent/tests/protocol.rs` (einmal, in Reihenfolge, bestätigt; aus ohne `--input`; Fremde ignoriert), `tests/e2e/tests/sessions.rs` (40 Tastenereignisse bei 30 % Verlust in beide Richtungen) | `cargo test -p fernsicht-input --all-features` |
| Ton | `crates/audio` (Jitter-Puffer: Reihenfolge, Duplikate, Lücken, Überlauf; Testton; Opus-Roundtrip und Verschleierung), `crates/audio/tests/pulse.rs` (Ton abspielen und über den Monitor wieder aufnehmen, nur im CI mit `FERNSICHT_PULSE_TEST=1`), `crates/input/tests/uinput.rs` (echte uinput-Geräte, nur im CI mit `FERNSICHT_UINPUT_TEST=1`), `tests/e2e/tests/sessions.rs` (Testton Host → Client, sauber und mit 20 % Verlust) | `FERNSICHT_REQUIRE_OPUS=1 cargo test -p fernsicht-audio` |
| KMS-Capture | `crates/capture/src/kms.rs` | Auswahl von Karte, Monitor und Ebene, VBlank-Raster, Fehlerpfade; Host lehnt Sessions ohne Capture/Encoder ab. Echtes Capture von Hand | `cargo test -p fernsicht-capture --features kms`, [docs/kms-capture.md](kms-capture.md) |
| Coverage | CI-Job `rust-coverage` | `cargo llvm-cov`, Schwelle 90 % Zeilen | `cargo llvm-cov --workspace --ignore-filename-regex 'main\.rs$'` |

Die Streaming-Szenarien messen Zeiten. Deshalb laufen sie innerhalb des
Test-Prozesses nacheinander (`fernsicht_e2e::exclusive()`). Gezählt wird
getrennt nach Netzverlust (Client) und Host-Überlast (`HostStats`). So
schlägt ein ausgelasteter CI-Runner nicht als Netzfehler an.

## Web

| Ebene | Wo | Was | Befehl |
|---|---|---|---|
| Unit | `web/ui/src/**/*.test.ts` | Formatierung (ms, %, Geräte-ID), Demo-Daten | `pnpm test` |
| Komponente | `*.test.tsx` (Vitest, jsdom, Testing Library) | Overlay, Session-Ansicht (Modus, Tastenkürzel, Ton), Geräteliste (Suche, Filter, Dialog), Navigation, Zwischenablage, Viewer-Formular | `pnpm test` |
| Coverage | Vitest v8 | Schwellen: 90 % Zeilen/Funktionen, 85 % Branches | `pnpm test:coverage` |
| Browser-E2E | `web/e2e` (Playwright, Desktop + Pixel 7) | Echte Builds beider Apps: Abläufe, Tastatur, Deep Links, Live-Overlay | `pnpm test:e2e` |
| Accessibility | `web/e2e` (axe) | WCAG 2.1 AA ohne „serious“/„critical“ und ohne horizontales Scrollen auf dem Handy | `pnpm test:e2e` |

## Was die Tests bisher gefunden haben

Die folgenden Fehler wurden beim Aufbau der Suite gefunden und behoben.
Jeder hat jetzt einen Regressionstest.

1. **Out-of-bounds im Reassembler** (proptest): Eine FEC-Gruppe, deren
   Daten über das Frame-Ende reichen, ließ die Wiederherstellung über den
   Puffer schreiben. Ein manipuliertes Paket konnte den Client abstürzen
   lassen.
2. **Speicher-DoS** (cargo-fuzz): Ein gefälschtes `frame_len` von ~4 GB
   ließ den Client den ganzen Frame vorab allokieren. Jetzt gelten harte
   Grenzen für Frame-Größe, Gruppengröße, Gruppenzahl und Recovery-Anteil.
3. **Schwarzes Bild nach Überlast** (E2E): „Latest frame wins“ verwarf
   *komprimierte* Frames, von denen spätere abhängen. Wurde der erste
   Keyframe überschrieben, blieb das Bild dauerhaft schwarz. Jetzt laufen
   komprimierte Frames durch eine FIFO, verworfen werden nur rohe bzw.
   dekodierte Frames, und Lücken lösen eine Keyframe-Anforderung aus.
4. **1 % Verlust war nicht unsichtbar** (E2E): Feste 10 % FEC ließen kleine
   Gruppen etwa alle 20 s ausfallen. Die Redundanz wird jetzt pro Gruppe
   binomial aus der gemessenen Verlustrate bemessen (Ausfall ≤ 10⁻⁵).
5. **Zählfehler**: Frames vor dem ersten vollständigen Frame fehlten in der
   Verluststatistik. Frames, die auf einen Keyframe warteten, zählten als
   Decode-Fehler.
6. **Handy-Layout** (Playwright): Das Latenz-Overlay verdeckte auf schmalen
   Bildschirmen die umgebrochene Werkzeugleiste samt „Trennen“.
7. **Kontrast** (axe): Der Video-Platzhalter hatte nur 3,9:1.
8. **Zähler-Überlauf** (cargo-fuzz in CI): Eine gefälschte erste Frame-ID
   nahe `u32::MAX` ließ `frames_dropped` überlaufen (Panic im Debug-Build,
   stilles Umschlagen im Release). Zähler sättigen jetzt, und Lücken über
   65 536 Frames gelten als Resync.
