#!/usr/bin/env bash
# Records `tokengauge --json` into tests/qml/fixtures/panel.json.
#
# The frontend harnesses drive the QML and the GNOME extension against this
# file, so it has to be what the binary actually prints rather than something
# hand-written - a fixture a frontend agrees with and the binary does not is
# worse than no fixture. It is recorded off the same seeded snapshot the
# binary's own end-to-end tests use
# (crates/tokengauge-waybar/tests/fixtures/snapshot.json), so the two cannot
# drift apart.
#
# The machine it records on has no credentials, no daemon and no fleet store,
# which is deliberate: the run serves the seeded snapshot instead of fetching,
# and the history comes out empty - the state every user is in before the
# first day of spend is recorded, and the one a frontend most easily gets
# wrong by drawing nothing at all.
#
# Two fields are rewritten afterwards, and only these two: `revision_file`,
# because the recording path is a temporary directory, and `update`, because
# nothing has asked GitHub and a fixture with no update in it would leave the
# frontends' update banner undrawn.
set -euo pipefail

ROOT="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)"
OUT="$ROOT/tests/qml/fixtures/panel.json"
SEED="$ROOT/crates/tokengauge-waybar/tests/fixtures/snapshot.json"

cargo build --release --manifest-path "$ROOT/Cargo.toml" -p tokengauge-waybar

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/state"

cat > "$WORK/config.toml" <<EOF
refresh_secs = 3600
cache_file = "$WORK/state/tokengauge-usage.json"
ccusage_enabled = false

[providers]
claude = true
codex = true

[waybar]
window = "weekly"
primary = "claude"
EOF

python3 - "$SEED" "$WORK/state/tokengauge-usage.json" <<'PY'
import datetime, sys
src, dest = sys.argv[1], sys.argv[2]
now = datetime.datetime.now(datetime.timezone.utc)
iso = lambda d: d.isoformat().replace("+00:00", "Z")
text = (open(src).read()
        .replace("{{SESSION_RESET}}", iso(now + datetime.timedelta(hours=2)))
        .replace("{{WEEKLY_RESET}}", iso(now + datetime.timedelta(days=3)))
        .replace("{{UPDATED_AT}}", iso(now))
        .replace("{{UPDATED_AT_MS}}", str(int(now.timestamp() * 1000))))
open(dest, "w").write(text)
PY

HOME="$WORK" \
XDG_STATE_HOME="$WORK/state" \
XDG_CONFIG_HOME="$WORK" \
XDG_DATA_HOME="$WORK/data" \
XDG_CACHE_HOME="$WORK/cache" \
CLAUDE_CONFIG_DIR="$WORK/claude" \
  "$ROOT/target/release/tokengauge" --config "$WORK/config.toml" --json > "$WORK/panel.json"

python3 - "$WORK/panel.json" "$OUT" <<'PY'
import json, sys
panel = json.load(open(sys.argv[1]))
if panel["errors"]:
    raise SystemExit(f"the recording machine fetched: {panel['errors']}")
if not panel["rows"]:
    raise SystemExit("the recording served no rows - the seed did not take")
panel["revision_file"] = "/tmp/tokengauge/tokengauge-revision"
# Derived from whatever the binary just reported, so a release never has to
# edit this file: `current` is the real version and `latest` is one minor above
# it, which is all the frontends' update banner needs to have something to draw.
current = str(panel.get("version") or "0.0.0")
parts = (current.split(".") + ["0", "0"])[:3]
parts[1] = str(int(parts[1]) + 1 if parts[1].isdigit() else 1)
parts[2] = "0"
panel["update"] = {"current": current, "latest": ".".join(parts), "available": True}
# Compact, because this is a recording and not something to read by eye. The
# harnesses parse it; `python3 -m json.tool` is there for when you do not.
open(sys.argv[2], "w").write(json.dumps(panel, sort_keys=True) + "\n")
print(f"{sys.argv[2]}: {len(panel['rows'])} rows, "
      f"{len(panel['rows'][0]['panel'])} sections")
PY
