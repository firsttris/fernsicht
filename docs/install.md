# Host als Dienst einrichten

Als Dienst startet der Host mit dem Rechner und läuft im Hintergrund. Es
braucht kein Terminal und kein `sudo` bei jedem Start. Geräte koppelt man
dann über den laufenden Dienst.

## Installieren

Einmal bauen, als normaler Benutzer in der Distrobox:

```sh
distrobox enter fernsicht
cd ~/fernsicht
git pull
cargo build --release -p fernsicht-host-agent --features vaapi,kms,nvidia
exit
```

Dann auf dem Host (nicht in der Box) installieren:

```sh
cd ~/fernsicht
sudo ./packaging/install-host.sh
```

```text
Fernsicht-Host läuft. Gerät koppeln: fernsicht-host-agent pair
```

Das Skript erledigt Folgendes:

- Es kopiert das Programm nach `/usr/local/bin`.
- Es richtet den Dienst `fernsicht-host` ein ([Unit](../packaging/fernsicht-host.service)) und startet ihn.
- Läuft firewalld und ist UDP-Port 47800 zu, öffnet es den Port.

Ein Update geht genauso: neu bauen und das Skript noch einmal starten.

Der Dienst läuft so:

- **Bild:** Der Dienst nimmt den Monitor über KMS auf. Dafür läuft er als root.
- **Encoder:** Der Dienst wählt den Encoder passend zur Grafikkarte (`--encoder auto`): VAAPI auf AMD/Intel, NVENC auf NVIDIA. Bildschirmaufnahme mit NVENC fehlt allerdings noch. Ein Rechner mit NVIDIA-Karte taugt deshalb vorerst nicht als Host, als Client schon.
- **Maus und Tastatur:** Eingaben gekoppelter Geräte nimmt der Dienst an (`--input`).
- **Ton:** Ton und Monitoranordnung holt sich der Dienst vom angemeldeten Benutzer. Er nimmt also auf, was auf dessen Desktop läuft.

## Hosts im Netz finden

Auf einem Client-Rechner:

```sh
fernsicht-client discover
```

```text
zentrale  192.168.178.87:47800  Bazzite · Radeon RX 7700 XT / 7800 XT · H.264  gekoppelt
```

Der Client fragt per Broadcast ins lokale Netz und direkt bei den
gekoppelten Hosts nach. Dafür ist kein zusätzlicher Port nötig, es läuft
über denselben UDP-Port 47800. Hat ein gekoppelter Host inzwischen eine
neue Adresse (DHCP), merkt sich der Client die neue.

Die Antworten sind nicht beglaubigt. Ob am anderen Ende wirklich der
gekoppelte Host sitzt, prüft erst der Schlüssel beim Verbinden. Ein
gefälschter Eintrag in der Liste kann also nichts anrichten.

## Geräte koppeln

Auf dem Host, ohne `sudo`:

```sh
fernsicht-host-agent pair
```

```text
Kopplung offen für 5 Minuten. PIN: 482913
Auf dem neuen Gerät: fernsicht-client pair <diese Adresse> <PIN>
```

Auf dem neuen Gerät, mit dem Namen aus `discover` oder der Adresse:

```sh
fernsicht-client pair zentrale 482913
```

Koppeln dürfen nur root und der Benutzer, der am Desktop angemeldet ist. Der
Dienst prüft das über den Unix-Socket (`/run/fernsicht/control.sock`).
Andere Benutzer auf demselben Rechner werden abgewiesen.

## Überblick und Geräte entfernen

```sh
fernsicht-host-agent status
```

```text
Host zentrale (Schlüssel 3f2a-…)
Verbunden: bazzite (192.168.178.20:51234), 2560×1440 bei 60 fps, verschlüsselt
Gekoppelte Geräte: 1
  bazzite  9c41-…
```

Ein Gerät entfernen:

```sh
fernsicht-host-agent unpair bazzite
```

Danach kommt dieses Gerät nicht mehr herein, bis es neu gekoppelt ist.

## Log, Stoppen, Entfernen

```sh
journalctl -u fernsicht-host -f        # Log live
sudo systemctl stop fernsicht-host     # anhalten (startet beim nächsten Boot wieder)
sudo systemctl disable --now fernsicht-host   # nicht mehr automatisch starten
sudo ./packaging/install-host.sh --uninstall       # ganz entfernen
```

Schlüssel und Kopplungen liegen in `/var/lib/fernsicht` und bleiben beim
Entfernen erhalten. Wer die Datei `host.json` dort löscht, bekommt einen
neuen Schlüssel. Alle Geräte müssen dann neu gekoppelt werden.

Meldet der Dienst nach einem Bazzite-Update
`libavcodec.so.NN: cannot open shared object file`, hat sich die
FFmpeg-Hauptversion geändert. Dann die Box aktualisieren (`dev/setup.sh`),
neu bauen und neu installieren.
