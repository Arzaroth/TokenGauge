#!/usr/bin/env bash
# Loads each QML frontend's data layer in a plain QML runtime and drives it
# with a recorded panel.
#
# The desktops these files are written for are not installed here, so their
# imports are stubbed: a Process and a data engine that answer by command line
# rather than spawning. What that leaves under test is the half a syntax check
# cannot reach - the bindings, the JSON the frontend reads back out, and what
# it hands the binary on the next call.
#
# The binary's own behaviour is covered by crates/tokengauge-waybar/tests/e2e.rs,
# which runs the real executable. This covers the other side of that boundary,
# and tests/gnome covers the third frontend that reads the same snapshot.
set -uo pipefail

here="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"

qml=""
for candidate in qml6 /usr/lib/qt6/bin/qml qml; do
  if command -v "$candidate" >/dev/null 2>&1; then
    qml="$candidate"
    break
  fi
done
if [[ -z $qml ]]; then
  echo "qml: no Qt QML runtime found - install qt6-declarative" >&2
  exit 1
fi

# Qt writes its diagnostics through a categorized logger that is off unless the
# stream is a terminal, so a failure would otherwise be an exit code and
# nothing else. The XHR permission is what lets a harness read its fixture.
export QT_QPA_PLATFORM=offscreen
export QT_FORCE_STDERR_LOGGING=1
export QML_XHR_ALLOW_FILE_READ=1

status=0
for harness in "$here"/*.qml; do
  name="$(basename "$harness" .qml)"
  # Check.qml is a helper, not a harness.
  [[ $name == "Check" ]] && continue
  if ! timeout 60 "$qml" -I "$here/stubs" -I "$here" "$harness"; then
    echo "::error::the $name frontend failed its QML harness" >&2
    status=1
  fi
done
exit $status
