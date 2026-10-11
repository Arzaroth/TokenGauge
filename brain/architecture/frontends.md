# Frontends

The six surfaces that draw the gauge, and what each is built with. Everything a
user reads comes from [panel-spec.md](panel-spec.md); a frontend owns only
chrome. The hard rule is in `CLAUDE.md` ("Frontend parity"): a user-facing
feature lands on all six or it is not done.

| Surface | Where | Toolkit | Gets its data |
| --- | --- | --- | --- |
| Waybar | `crates/tokengauge-waybar` (bar + tooltip) | pango markup | in-process `panel_spec`; the tooltip *is* the panel |
| Plasma | `plasma/org.tokengauge.plasmoid` | QML (Plasma 6) | `tokengauge --json` |
| GNOME | `gnome/tokengauge@arzaroth.github.io` | TypeScript, compiled to GJS (GNOME 45+) | `tokengauge --json` |
| Quickshell | `omarchy/arzaroth.tokengauge` | QML (omarchy-shell plugin) | `tokengauge --json` |
| Tray | `crates/tokengauge-tray` | eframe/egui + tray-icon (Windows, macOS menu bar) | in-process |
| TUI | `crates/tokengauge-tui` | ratatui | in-process; exempt from layout parity only |

A frontend never reads a credential, a cache file or a provider endpoint;
`--json` is the single snapshot the non-Rust frontends render. The desktop
frontends install separately from the binary (`--install-frontend`, the
`scripts/install-*.sh`), so a gap there is invisible from the crate that grew a
feature.

## Per-surface notes

- **Waybar.** Bar text per provider with brand icons and CSS tier classes
  (`tokengauge-warn`, `-crit`, `-error`, `-stale`). Scroll rotates providers,
  left-click runs `--click` (the TUI in a terminal, resolved by
  `launch::tui_command`: `[waybar] tui_command`, `omarchy-launch-or-focus-tui`,
  `$TERMINAL`, then a list of terminals). No history screen and no remuda button,
  by design.
- **TUI.** Draws `tokens_by_day` as a bar chart and keeps its sidebar, gauges and
  keybindings, but every string is the spec's. Carries the sync setup screen
  (`S`, `sync_view.rs`) and history (`H`, `history_view.rs`). On Windows it also
  carries the updater.
- **Plasma.** `Service.qml` owns the subprocess and imports no plasmoid module, so
  the test harness can instantiate it; `main.qml` (a `PlasmoidItem`) re-exposes
  it. Keep data work on `Service.qml`'s side. Watches the revision through
  `--wait-change`.
- **GNOME.** TypeScript against `@girs/gnome-shell`, the only frontend not
  installable as it sits. `scripts/build.sh` compiles it and assembles all three
  desktop payloads under `build/frontends/<payload>`, the release archive's own
  layout. `Frontend.compiled` makes `payload_in` look only under `build/`, so
  `--install-frontend gnome` from a checkout refuses rather than installing
  TypeScript. GSettings schemas stay XML (compiled on the target by selvedge's
  `install_into`). The `--json` contract is declared by hand in `panel.ts`, named
  exactly as the JSON names it.
- **Quickshell (Omarchy).** Adapted from omarchy's `omarchy.agents` plugin (MIT
  notice in `NOTICE`); watches the revision with `FileView`. Collapses
  `good`/`warn` onto the bar foreground.
- **Tray.** Gated to Windows and macOS (GUI deps only there), so it does not
  type-check on Linux; CI's Windows and macOS jobs are the authority. History is
  painted with egui (`draw_history_chart`). The tray quits when it launches an
  MSI update, and on macOS its Update item opens the release page instead.

## Sources

- `crates/tokengauge-waybar/src/{main,render,snapshot,daemon}.rs`
- `crates/tokengauge-tui/src/{app,ui,history_view,sync_view,refresh}.rs`
- `crates/tokengauge-tray/src/main.rs`
- `plasma/org.tokengauge.plasmoid/contents/ui/{main,Service,FullRep,CompactRep,ConfigGeneral}.qml`
- `gnome/tokengauge@arzaroth.github.io/{extension,panel,widgets,prefs,util}.ts`
- `omarchy/arzaroth.tokengauge/{Usage,Panel}.qml`, `omarchy/arzaroth.tokengauge/manifest.json`
- `crates/tokengauge-core/src/launch.rs`, `crates/tokengauge-core/src/project.rs` (frontend declarations), `scripts/build.sh`
