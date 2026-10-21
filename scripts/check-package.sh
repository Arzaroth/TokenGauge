#!/bin/sh
# Usage: scripts/check-package.sh
#
# Run on a system with a TokenGauge package installed: fails unless the
# package put the binaries, the old name, the daemon's unit and both desktop
# payloads where each is looked for.

set -eu

fail() {
  echo "check-package: $*" >&2
  exit 1
}

tokengauge --version
tokengauge-tui --version
[ "$(readlink /usr/bin/tokengauge-waybar)" = tokengauge ] || fail "no /usr/bin/tokengauge-waybar"
tokengauge-waybar --version >/dev/null || fail "tokengauge-waybar does not run tokengauge"
grep -qx 'ExecStart=/usr/bin/tokengauge --daemon' /usr/lib/systemd/user/tokengauge-daemon.service ||
  fail "the daemon's unit does not run /usr/bin/tokengauge"
for file in \
  /usr/share/plasma/plasmoids/org.tokengauge.plasmoid/metadata.json \
  /usr/share/plasma/plasmoids/org.tokengauge.plasmoid/contents/ui/main.qml \
  /usr/share/gnome-shell/extensions/tokengauge@arzaroth.github.io/metadata.json \
  /usr/share/gnome-shell/extensions/tokengauge@arzaroth.github.io/extension.js \
  /usr/share/gnome-shell/extensions/tokengauge@arzaroth.github.io/schemas/gschemas.compiled; do
  [ -f "$file" ] || fail "no $file"
done
if [ -e /usr/share/gnome-shell/extensions/tokengauge@arzaroth.github.io/extension.ts ]; then
  fail "the GNOME extension ships its TypeScript"
fi
echo "check-package: everything is in place"
