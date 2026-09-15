#!/usr/bin/env bash
# Loads the compiled GNOME extension in Node with the shell stubbed out, and
# drives it with a recorded panel.
#
# GNOME Shell is not installed here and installing it would not help: an
# extension only runs inside a live shell. So the toolkit is stubbed instead -
# `gi://St` records the widget tree rather than drawing it, `gi://Gio` records
# the subprocess rather than spawning it, and `gi://GLib` records the timeout
# rather than arming it. What that leaves under test is everything the
# extension actually decides: what it asks the binary, what it does with the
# answer, and what it draws.
#
# It runs the *compiled* extension, not the TypeScript, for the same reason
# CI's syntax check does: what ships is the JavaScript.
set -euo pipefail

here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
root="$(cd "$here/../.." && pwd)"

if [[ ! -f $root/build/frontends/gnome/tokengauge@arzaroth.github.io/extension.js ]]; then
  echo "gnome: the extension is not built - run scripts/build.sh" >&2
  exit 1
fi

cd "$root"
exec node --import "$here/register.mjs" --test "$here"/*.test.mjs
