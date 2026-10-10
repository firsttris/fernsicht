# Fernsicht-App

Die Desktop-App: [Tauri](https://tauri.app) um die Client-UI
(`apps/client-ui`). Sie zeigt die Rechner im Netz, koppelt per PIN und
startet Sitzungen.

- **Sitzungen** laufen im nativen Fenster von `fernsicht-client` (Vulkan,
  keine Webview im Bildweg). Die App startet es mit `--app` und liest
  jede Sekunde das Latenz-Overlay als JSON. Schließt die App, endet auch
  die Sitzung.
- **Rechner** findet die App per Broadcast im LAN (`fernsicht-client
  discover`), gekoppelte Hosts fragt sie zusätzlich direkt.
- **Dieser Rechner:** Läuft hier ein Host (z. B. der Dienst), öffnet
  „Gerät koppeln" dort die Kopplung und zeigt die PIN.
- **Schlüssel und Kopplungen** liegen in `~/.config/fernsicht`, wie beim
  Kommandozeilen-Client. Was dort gekoppelt ist, kennt auch die App.

## Als AppImage

```sh
./packaging/appimage.sh   # → apps/desktop/target/release/bundle/appimage/
```

Das baut alles und packt die App, das Stream-Fenster, den Host und den
Web-Viewer in eine Datei. Releases baut die CI genauso (Workflow „Bump
version“, dann „Release“).

## Bauen und starten

Die App ist ein eigener Cargo-Workspace. Sie braucht WebKitGTK (in der
Distrobox vorhanden), die gebauten Oberflächen und die beiden Programme,
die das AppImage mitträgt, unter `binaries/` (einmal `./packaging/appimage.sh`
legt sie an):

```sh
pnpm install && pnpm --filter @fernsicht/client-ui build
cargo build --release -p fernsicht-client --features vaapi,window
cd apps/desktop && cargo build --release
FERNSICHT_CLIENT=../../target/release/fernsicht-client ./target/release/fernsicht
```

Wenn du an der UI arbeitest, mit Hot Reload: `pnpm dev:client` starten,
dann `cargo run --no-default-features`. Die App lädt dann den
Vite-Server statt der eingebauten Dateien.

## Tests

```sh
cargo test   # App-Logik gegen echten Host und echten Client
pnpm test    # (im Repo-Wurzelverzeichnis) die UI, auch im App-Modus
```
