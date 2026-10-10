# Fernsicht installieren

Es gibt zwei Teile:

- **Die App** auf jedem Rechner, *von dem aus* man zugreift. Sie findet
  die Rechner im Netz, koppelt und öffnet Sitzungen.
- **Den Host-Dienst** auf jedem Rechner, *auf den* man zugreift. Er
  startet mit dem Rechner und läuft im Hintergrund.

Ein Rechner kann beides haben.

## 1. Bauen

Einmal pro Rechner (oder nach einem `git pull`). Das Skript läuft in der
Distrobox `fernsicht` und betritt sie selbst:

```sh
cd ~/fernsicht
./packaging/build.sh
```

Bazzite bringt FFmpeg, libva und WebKitGTK selbst mit. Die Programme aus
der Box laufen deshalb direkt auf dem System.

## 2. App installieren

Ohne `sudo`, für den eigenen Benutzer:

```sh
./packaging/install-app.sh
```

Danach steht **Fernsicht** im Startmenü. Das Skript legt App und Client
nach `~/.local/bin` und richtet Startmenü-Eintrag und Symbol ein. Mit
`--uninstall` entfernt es beides wieder.

## 3. Host-Dienst installieren

Auf dem Rechner, den man fernsteuern will:

```sh
sudo ./packaging/install-host.sh
```

```text
Fernsicht-Host läuft. Gerät koppeln: fernsicht-host-agent pair
```

Das Skript erledigt Folgendes:

- Es kopiert das Programm nach `/usr/local/bin`.
- Es richtet den Dienst `fernsicht-host` ein ([Unit](../packaging/fernsicht-host.service)) und startet ihn.
- Läuft firewalld und ist UDP-Port 47800 zu, öffnet es den Port.

Ein Update geht genauso: neu bauen und die Skripte noch einmal starten.

Der Dienst läuft so:

- **Bild:** Der Dienst nimmt den Monitor über KMS auf. Dafür läuft er als root.
- **Encoder:** Der Dienst wählt den Encoder passend zur Grafikkarte (`--encoder auto`): VAAPI auf AMD/Intel, NVENC auf NVIDIA. Bildschirmaufnahme mit NVENC fehlt allerdings noch. Ein Rechner mit NVIDIA-Karte taugt deshalb vorerst nicht als Host, als Client schon.
- **Maus und Tastatur:** Eingaben gekoppelter Geräte nimmt der Dienst an (`--input`).
- **Ton:** Ton und Monitoranordnung holt sich der Dienst vom angemeldeten Benutzer. Er nimmt also auf, was auf dessen Desktop läuft.

## 4. Koppeln und verbinden (in der App)

1. **Am Host:** Fernsicht öffnen und unten links unter „Dieser Rechner“
   auf **Gerät koppeln** klicken. Die App zeigt eine 6-stellige PIN, die
   5 Minuten gilt.
2. **Am anderen Rechner:** Fernsicht öffnen. Der Host steht in der
   Liste. Dort auf **Koppeln** klicken und die PIN eingeben.
3. **Verbinden:** Auf **Desktop** klicken. Das Bild öffnet sich in einem
   eigenen Fenster, die App zeigt die Latenz.
   - <kbd>Strg</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>F</kbd> schaltet Vollbild um.
   - <kbd>Strg</kbd>+<kbd>Alt</kbd>+<kbd>Shift</kbd>+<kbd>Q</kbd> oder **Trennen** in der App beendet die Sitzung.

Gekoppelt wird nur einmal pro Gerätepaar. Dasselbe geht auch im
Terminal, wie die folgenden Abschnitte zeigen.

## Im Browser

Ohne Installation, von jedem Rechner, Tablet oder Handy im selben Netz:

1. Im Browser die Adresse des Hosts öffnen: `http://192.168.178.87:47800`
   (das Installationsskript nennt sie am Ende).
2. Am Host „Gerät koppeln“ wählen (App, oder `fernsicht-host-agent pair`).
3. Die PIN im Browser eingeben und auf **Verbinden** klicken.

So funktioniert es:

- **Zugang:** Die PIN gilt für eine Sitzung. Der Browser wird nicht
  gekoppelt und braucht beim nächsten Mal eine neue PIN.
- **Übertragung:** Bild (H.264), Ton und Eingaben laufen verschlüsselt
  über WebRTC.
- **Eingabe:** Im Modus **Desktop** steuert die Maus direkt. Im Modus
  **Gaming** fängt ein Klick ins Bild den Mauszeiger, Esc gibt ihn frei.
- **Was nicht geht:** Tasten, die der Browser selbst behält (z. B.
  Strg+W), kommen nicht beim Host an. Dafür ist die App da.

Grenzen:

- **Unverschlüsselt im LAN:** Die Seite und die PIN gehen unverschlüsselt
  durchs LAN (`http://`). Die PIN gilt nur einmal und nur 5 Minuten.
- **Firewall:** Der Host braucht TCP 47800 für die Seite und einen freien
  UDP-Port über 1024 für WebRTC. Fedora und Bazzite lassen beides zu;
  `install-host.sh` öffnet 47800.
- **Browser:** Getestet mit Chrome. Firefox spielt H.264 nur mit
  passendem Decoder; noch nicht ausprobiert.

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
