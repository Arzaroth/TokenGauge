# TokenGauge for Omarchy

A bar widget for the Omarchy 4 Quickshell shell. One bar icon and one panel
covering every AI coding subscription TokenGauge knows about: Claude, Codex,
Kimi, Grok, GLM, OpenRouter, opencode Go and Cursor.

The QML is strictly a display. Everything it draws comes from a single
`tokengauge --json` snapshot, the same one the KDE Plasma applet and the
GNOME extension read, so credentials, endpoints, transcripts and the snapshot
never enter the shell process. What each section says is resolved in
`tokengauge-core` (`panel.rs`), so this panel reads the same as every other
TokenGauge surface.

## Install

From a checkout:

```bash
scripts/install-omarchy.sh
```

That builds the binaries, copies this folder to
`~/.config/omarchy/plugins/arzaroth.tokengauge/`, enables the widget, and
restarts the shell. Pass `--placement=<section>` to choose a bar section
(default `right`).

The section is Omarchy's shell config, not the widget's, and the installer
only sets it the first time: once the widget is enabled, a rerun leaves it
where it is. To move it later, enable it again with a section:

```bash
omarchy plugin enable arzaroth.tokengauge --section centerEnd
```

Omarchy takes `left`, `centerBegin`, `center`, `centerEnd`, `tray` and
`right`; `centerEnd` is the group to the right of the clock.

Omarchy ships its own `omarchy.agents` widget. Drop it with
`omarchy plugin disable omarchy.agents` once this one is in the bar.

## Panel

- **Hero** - the provider and the plan it runs on, with refresh, history,
  remuda (while its page is serving) and settings buttons. Hovering refresh
  says when the snapshot was last written.
- **Provider switch** - one chip per enabled provider, shown only when more
  than one is enabled.
- **Sections** - the panel spec's, in its order, each dropped when it has no
  data:
  - **Status** - why a stale row is stale (a failed fetch, an expired token).
  - **Plans** and one **Limits** group per credential, for a provider holding
    several logins in remuda's store; a single login draws one Limits group.
    Each window shows its use, a meter with the pace marker, and the time
    until it resets.
  - **Cost** - today, this month and the burn rate, rated from the CLIs' own
    transcripts against LiteLLM's price table.
  - **Tokens by day**, **by model** and **by device** - one row each, the bar
    behind it scaled to the largest; hover for the exact figures.
- **History** - a second screen (`.`) with 30 days, 90 days and 12 months of
  spend.
- **Update banner** - when a newer TokenGauge release is out, an Update button
  installs it in place, or names the package manager that owns the install.
- **Settings** - the gear (or `,`) opens provider toggles, the pin-to-bar
  picker, the panel options and sync setup, all writing to `config.toml`. A
  provider enabled for the first time has nothing cached yet, so its tab
  appears a few seconds later, once it has been fetched.

## Interactions

- Bar icon: hover = every limit and today's spend, left = panel, right =
  refresh, middle = usage dashboard, back (mouse 8) = status page, scroll =
  previous / next provider. The clicks are the set the Waybar module binds,
  minus its rotate-and-persist: scrolling here moves the panel's own selection
  and leaves `config.toml` alone. The hover is this widget's own - Waybar's
  tooltip is the whole panel rather than a summary of it.
- Panel header: refresh, history, remuda, settings.
- Panel: `h`/`l` switch provider, `j`/`k` scroll, `r` or Enter refresh, `u` and
  `s` open the active provider's usage dashboard and status page, `.` toggles
  history, `,` toggles settings, `m` opens remuda while it serves, Tab moves to
  the neighboring bar panel, Esc closes. `u` and `s` mean what they mean in the
  TUI.
- History: the number keys pick a range.
- Settings pane: a number key toggles the provider on that row, `p` walks the
  pin through Highest and each enabled provider, `a`, `t` and `b` flip the
  panel options (active credential only, plans total, split bars), `y` opens
  sync setup.
- IPC: `omarchy-shell arzaroth.tokengauge <open|close|toggle|refresh|next>`.

## Settings

Widget settings live inline on its entry in `~/.config/omarchy/shell.json`:

| Key | Default | What it does |
|---|---|---|
| `refreshIntervalSec` | `600` | How often the snapshot is re-read |
| `binary` | `tokengauge-waybar` | Command used to read the snapshot (the binary ships a symlink under that name; the default moves to `tokengauge` a release after the rename) |

Numbers need `--json`, or they land in `shell.json` as strings:

```bash
omarchy bar set arzaroth.tokengauge refreshIntervalSec 300 --json
```

Provider enablement, the pinned primary and the `[panel]` options are
editable from the panel's settings pane; everything else - notification thresholds, the refresh cadence
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
