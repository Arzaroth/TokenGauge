#!/usr/bin/env bash
# Runs the compiled GNOME extension in a real, headless GNOME Shell in a
# container, opens its popup and screenshots it.
#
# `tests/gnome/run.sh` drives the extension against stubs, which see the
# widget tree the extension builds but not what the shell does with it: GJS
# refusing an initializer the stub took, or a popup taller than the monitor.
# This is the check for those. It is local and opt-in - it pulls a Fedora image
# with GNOME Shell in it - and it is not part of CI.
#
#   tests/gnome/shell/run.sh [--live | --json FILE] [--provider NAME]
#                            [--monitor WxH] [--out DIR]
#
# The panel defaults to tests/qml/fixtures/panel.json. --live serves what the
# installed `tokengauge --json` prints instead, which is the one with token
# breakdowns and several credentials in it. It stays on this machine.
set -euo pipefail

here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
root="$(cd "$here/../../.." && pwd)"

json="$root/tests/qml/fixtures/panel.json"
live=0
provider=""
monitor="1920x1080"
out=""
while (($#)); do
  case $1 in
    --live) live=1 ;;
    --json) json="$2"; shift ;;
    --provider) provider="$2"; shift ;;
    --monitor) monitor="$2"; shift ;;
    --out) out="$2"; shift ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

ext="$root/build/frontends/gnome"
if [[ ! -f $ext/tokengauge@arzaroth.github.io/extension.js ]]; then
  echo "gnome-shell: the extension is not built - run scripts/build.sh" >&2
  exit 1
fi

engine="$(command -v podman || command -v docker || true)"
if [[ -z $engine ]]; then
  echo "gnome-shell: needs podman or docker" >&2
  exit 1
fi

out="${out:-$(mktemp -d "${TMPDIR:-/tmp}/tokengauge-gnome-shell.XXXXXX")}"
mkdir -p "$out"
rm -f "$out"/{done,probe.txt,shell.log,top.png,bottom.png}

if ((live)); then
  tokengauge --json >"$out/panel.json"
else
  cp "$json" "$out/panel.json"
fi
# The extension runs whatever binary its settings name; this one answers every
# command line with the recorded panel.
printf '#!/bin/sh\ncat /data/panel.json\n' >"$out/tokengauge"
chmod +x "$out/tokengauge"

image=tokengauge-gnome-shell
if ! "$engine" build -t "$image" -f "$here/Containerfile" "$here" >"$out/image.log" 2>&1; then
  echo "gnome-shell: the image did not build; see $out/image.log" >&2
  exit 1
fi

"$engine" run --rm \
  -e MONITOR="$monitor" -e PROVIDER="$provider" \
  -v "$ext:/ext:ro,z" \
  -v "$here/probe@tokengauge.test:/probe@tokengauge.test:ro,z" \
  -v "$here/boot.sh:/boot.sh:ro,z" \
  -v "$out/panel.json:/data/panel.json:ro,z" \
  -v "$out/tokengauge:/usr/local/bin/tokengauge:ro,z" \
  -v "$out/tokengauge:/usr/local/bin/tokengauge-waybar:ro,z" \
  -v "$out:/out:z" \
  "$image" /boot.sh >/dev/null 2>&1 || true

echo "$(cat "$out/version.txt" 2>/dev/null || echo 'GNOME Shell ?') at $monitor -> $out"
cat "$out/probe.txt" 2>/dev/null || true

status=0
if [[ ! -f $out/done ]]; then
  echo "gnome-shell: the probe never finished; see $out/shell.log" >&2
  status=1
fi
if grep -q '^error:' "$out/probe.txt" 2>/dev/null; then
  status=1
fi
if grep -A4 -E 'JS ERROR|Exception in callback' "$out/shell.log" | grep -q 'tokengauge@arzaroth.github.io'; then
  echo "gnome-shell: the extension threw:" >&2
  grep -B1 -A6 -E 'JS ERROR|Exception in callback' "$out/shell.log" >&2
  status=1
fi
exit "$status"
