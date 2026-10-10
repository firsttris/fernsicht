#!/bin/sh
# Installs (or updates) the Fernsicht app and client for this user, with
# a menu entry. No sudo needed. Build first: ./packaging/build.sh
#
#   ./packaging/install-app.sh
#   ./packaging/install-app.sh --uninstall
#
# PREFIX (default ~/.local) chooses where: bin/, share/applications/,
# share/icons/. Keys and paired hosts (~/.config/fernsicht) stay either way.
set -eu

cd "$(dirname "$0")/.."
prefix=${PREFIX:-$HOME/.local}
bin="$prefix/bin"
apps="$prefix/share/applications"
icons="$prefix/share/icons/hicolor"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$bin/fernsicht" "$bin/fernsicht-client" "$apps/fernsicht.desktop" \
        "$icons/32x32/apps/fernsicht.png" "$icons/128x128/apps/fernsicht.png" \
        "$icons/256x256/apps/fernsicht.png"
    echo "Fernsicht-App entfernt (Schlüssel und Kopplungen bleiben in ~/.config/fernsicht)."
    exit 0
fi

app=apps/desktop/target/release/fernsicht
client=target/release/fernsicht-client
for f in "$app" "$client"; do
    if [ ! -x "$f" ]; then
        echo "$f fehlt. Erst bauen: ./packaging/build.sh" >&2
        exit 1
    fi
    # Built in the distrobox, the libraries must exist here as well.
    missing=$(ldd "$f" | grep "not found" || true)
    if [ -n "$missing" ]; then
        echo "Auf diesem System fehlen Bibliotheken für $f:" >&2
        echo "$missing" >&2
        exit 1
    fi
done

install -D -m 0755 "$app" "$bin/fernsicht"
install -D -m 0755 "$client" "$bin/fernsicht-client"
install -D -m 0644 apps/desktop/icons/32x32.png "$icons/32x32/apps/fernsicht.png"
install -D -m 0644 apps/desktop/icons/128x128.png "$icons/128x128/apps/fernsicht.png"
install -D -m 0644 apps/desktop/icons/icon.png "$icons/256x256/apps/fernsicht.png"
mkdir -p "$apps"
sed "s|@BIN@|$bin|" packaging/fernsicht.desktop >"$apps/fernsicht.desktop"
chmod 0644 "$apps/fernsicht.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$apps" >/dev/null 2>&1 || true
fi

echo "Fernsicht ist installiert: im Startmenü unter Fernsicht, oder $bin/fernsicht"
case ":$PATH:" in
*":$bin:"*) ;;
*) echo "Hinweis: $bin ist nicht im PATH; der Client heißt dort $bin/fernsicht-client." ;;
esac
