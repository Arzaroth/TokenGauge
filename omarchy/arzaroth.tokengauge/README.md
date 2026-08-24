# TokenGauge for Omarchy

A bar widget for the Omarchy 4 Quickshell shell. One bar icon and one panel
covering every AI coding subscription TokenGauge knows about: Claude, Codex,
Kimi, Grok, and GLM.

The QML is strictly a display. Everything it draws comes from a single
`tokengauge --json` snapshot, the same one the KDE Plasma applet and the
GNOME extension read, so credentials, endpoints, ccusage, and the cache never
enter the shell process.

## Install

From a checkout:

```bash
scripts/install-omarchy.sh
```

That builds the binaries, copies this folder to
`~/.config/omarchy/plugins/arzaroth.tokengauge/`, enables the widget, and
restarts the shell. Pass `--placement=left|center|right` to choose a bar
section (default `right`).

Omarchy also ships its own `omarchy.agents` widget, which covers Claude, Codex,
and Fireworks. Both can sit in the bar at once; drop theirs with
`omarchy plugin disable omarchy.agents`.

## Panel

- **Hero** - the provider and the plan it runs on ("Claude Max 20x").
- **Provider switch** - one chip per enabled provider, shown only when more
  than one is enabled.
- **Limits** - the percentage of each window used, a matching meter, the time
  until it resets, and the pace projection next to it (`ends ~9%`, or
  `empty in 2h 15m` when the window runs out first). Session, weekly, and any
  provider-specific extra windows all render the same way.
- **Cost** - today and this month in dollars and tokens, plus the current burn
  rate per hour, from ccusage.
- **Tokens by day** - one row per day for the last week: day, bar, then tokens
  and dollars, with today bolded at the bottom. Hover a row for its full date
  and exact figures. ccusage omits days with no spend, so each row is labelled
  from its own date rather than by counting back from today.
- **Tokens by model** - tokens and cost per model, the bar behind each row
  scaled to the heaviest model. Hover for the input / output / cache-write /
  cache-read split.
- **Update banner** - when a newer TokenGauge release is out, an Update button
  installs it in place.
- **Settings** - the gear on the hero (or `s`) opens provider toggles and the
  pin-to-bar picker, both writing straight to `config.toml`. A provider enabled
  for the first time has nothing cached yet, so its tab appears a few seconds
  later, once the daemon has fetched it.

## Interactions

- Bar icon: left = panel, right = refresh, middle = usage dashboard, back
  (mouse 8) = status page, scroll = previous / next provider. The same set the
  Waybar module binds, minus its rotate-and-persist: scrolling here moves the
  panel's own selection and leaves `config.toml` alone.
- Panel: `h`/`l` switch provider, `j`/`k` scroll, `r` or Enter refresh, `u` and
  `s` open the active provider's usage dashboard and status page, `,` toggles
  the settings pane, Tab moves to the neighboring bar panel, Esc closes.
  `u` and `s` mean what they mean in the TUI.
- Settings pane: a number key toggles the provider on that row, `p` walks the
  pin through Highest and each enabled provider.
- IPC: `omarchy-shell arzaroth.tokengauge <open|close|toggle|refresh|next>`.

## Settings

Widget settings live inline on its entry in `~/.config/omarchy/shell.json`:

| Key | Default | What it does |
|---|---|---|
| `refreshIntervalSec` | `600` | How often the snapshot is re-read |
| `binary` | `tokengauge` | Command used to read the snapshot |

Numbers need `--json`, or they land in `shell.json` as strings:

```bash
omarchy bar set arzaroth.tokengauge refreshIntervalSec 300 --json
```

Provider enablement and the pinned primary are editable from the panel's
settings pane; everything else - notification thresholds, the refresh cadence
of the daemon, the click action, the cache location - stays in
`~/.config/tokengauge/config.toml`, shared with the Waybar module and the
daemon.

The daemon serves from the binary it started with, so after rebuilding run
`systemctl --user restart tokengauge-daemon` (the installer does this for you)
or the widget keeps reading the old snapshot shape.

## Developing

The shell watches the plugin folder and logs `Local plugin changed, reloading`,
but it keeps the already-instantiated widget: **QML edits only take effect after
`omarchy-restart-shell`.** Manifest changes are picked up by
`omarchy-shell shell rescanPlugins`.

Read the shell's own log for QML errors:

```bash
qs log --pid "$(pgrep -f 'quickshell -n -p /usr/share/omarchy/shell')" -t 50
```

Symlinks are refused anywhere inside a plugin folder, so the installer copies
this directory rather than linking it. Re-run `scripts/install-omarchy.sh` to
push local edits.
