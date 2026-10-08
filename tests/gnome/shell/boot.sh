#!/bin/sh
# Runs inside the container: a headless GNOME Shell with the compiled extension
# and the probe enabled, until the probe writes `done` or the deadline passes.
set -eu

mkdir -p /tmp/rt /run/dbus
chmod 700 /tmp/rt
export XDG_RUNTIME_DIR=/tmp/rt

# GNOME Shell 50 will not start without logind and upower on a system bus.
dbus-daemon --system --fork
python3 -m dbusmock --template logind --system >/out/mock.log 2>&1 &
python3 -m dbusmock --template upower --system >>/out/mock.log 2>&1 &
sleep 2

exec dbus-run-session -- sh -eu -c '
ext="$HOME/.local/share/gnome-shell/extensions"
mkdir -p "$ext"
cp -r /ext/tokengauge@arzaroth.github.io /probe@tokengauge.test "$ext/"
glib-compile-schemas "$ext/tokengauge@arzaroth.github.io/schemas"
gsettings set org.gnome.shell disable-user-extensions false
gsettings set org.gnome.shell disable-extension-version-validation true
gsettings set org.gnome.shell enabled-extensions "[\"tokengauge@arzaroth.github.io\", \"probe@tokengauge.test\"]"
gnome-shell --version >/out/version.txt

gnome-shell --headless --wayland --no-x11 --virtual-monitor "$MONITOR" >/out/shell.log 2>&1 &
shell=$!
i=0
while [ ! -f /out/done ] && [ "$i" -lt 60 ]; do sleep 1; i=$((i + 1)); done
kill "$shell" 2>/dev/null || true
wait "$shell" 2>/dev/null || true
'
