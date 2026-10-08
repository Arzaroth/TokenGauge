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
#                            [--gnome VERSION] [--monitor WxH] [--out DIR]
#
# The panel defaults to tests/qml/fixtures/panel.json. --live serves what the
# installed `tokengauge --json` prints instead, which is the one with token
# breakdowns and several credentials in it. --gnome picks the shell (45-50,
# default 50) through the Fedora release that shipped it. Screenshots and logs
# land in --out, created private to you, or in a fresh temporary directory.
set -euo pipefail

here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
root="$(cd "$here/../../.." && pwd)"

json="$root/tests/qml/fixtures/panel.json"
live=0
provider=""
gnome=50
monitor="1920x1080"
out=""
while (($#)); do
  case $1 in
    --live) live=1 ;;
    --json) json="$2"; shift ;;
    --provider) provider="$2"; shift ;;
    --gnome) gnome="$2"; shift ;;
    --monitor) monitor="$2"; shift ;;
    --out) out="$2"; shift ;;
    -h|--help) sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

if ! [[ $gnome =~ ^[0-9]+$ ]] || ((gnome < 45)); then
  echo "gnome-shell: --gnome takes 45 or later" >&2
  exit 2
fi
# Fedora 39 shipped GNOME 45, and every release since has moved both by one.
fedora=$((gnome - 6))

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

# The container writes into a directory made for this run alone, because the
# mount relabels everything under it for SELinux; pointing that at a directory
# the caller named would relabel whatever it holds.
work="$(mktemp -d "${TMPDIR:-/tmp}/tokengauge-gnome-shell.XXXXXX")"
if [[ -n $out ]]; then
  (umask 077 && mkdir -p "$out")
  trap 'rm -rf "$work"' EXIT
else
  out="$work"
fi

if ((live)); then
  tokengauge --json >"$work/panel.json"
else
  cp "$json" "$work/panel.json"
fi
# The extension runs whatever binary its settings name; this one answers every
# command line with the recorded panel.
printf '#!/bin/sh\ncat /data/panel.json\n' >"$work/tokengauge"
chmod +x "$work/tokengauge"

image="tokengauge-gnome-shell:$gnome"
if ! "$engine" build -t "$image" --build-arg "FEDORA=$fedora" \
    -f "$here/Containerfile" "$here" >"$work/image.log" 2>&1; then
  cp "$work/image.log" "$out/" 2>/dev/null || true
  echo "gnome-shell: the image did not build; see $out/image.log" >&2
  exit 1
fi

"$engine" run --rm \
  -e MONITOR="$monitor" -e PROVIDER="$provider" \
  -v "$ext:/ext:ro,z" \
  -v "$here/probe@tokengauge.test:/probe@tokengauge.test:ro,z" \
  -v "$here/boot.sh:/boot.sh:ro,z" \
  -v "$work/panel.json:/data/panel.json:ro,z" \
  -v "$work/tokengauge:/usr/local/bin/tokengauge:ro,z" \
  -v "$work/tokengauge:/usr/local/bin/tokengauge-waybar:ro,z" \
  -v "$work:/out:z" \
  "$image" /boot.sh >/dev/null 2>&1 || true

if [[ $out != "$work" ]]; then
  for f in version.txt probe.txt shell.log mock.log top.png bottom.png done; do
    if [[ -f $work/$f ]]; then cp "$work/$f" "$out/"; else rm -f "$out/$f"; fi
  done
fi

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
# Read the log whole rather than through a pipe into `grep -q`: under pipefail
# the first grep dying of SIGPIPE would read as no match.
thrown="$(grep -A12 -E 'JS ERROR|Exception in callback' "$out/shell.log" 2>/dev/null || true)"
if [[ $thrown == *tokengauge@arzaroth.github.io* ]]; then
  echo "gnome-shell: the extension threw:" >&2
  echo "$thrown" >&2
  status=1
fi
exit "$status"
