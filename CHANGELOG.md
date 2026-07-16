# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.0] - 2026-07-16

Provider toggles apply immediately across all frontends.

### Added

- The popover and TUI fetch fresh data when opened; the popover shows the current cache immediately while the refresh runs, and the TUI blocks on the fetch behind its spinner.
- Refresh indicator in the popover: a ⟳ marker shows in the header for the duration of a fetch, and the view re-renders when the data lands.

### Fixed

- Disabling a provider now takes effect immediately. The daemon refetches when a config reload changes the enabled provider set (previously it re-rendered from cache, so a disabled provider kept showing, and a newly enabled one stayed missing, until the next refresh tick - up to `refresh_secs`).
- The popover's "updated" stamp reports when the cache was last written, taken from the cache's mtime, and shows the date when the write wasn't today. Note a stale-fallback round also writes the cache, so the stamp tracks the last write, not necessarily a successful fetch. It rendered the current time, so it always claimed the data was fresh even when the fetch behind it was hours old or failing.
- `scripts/install.sh` reports that `tokengauge-popover` isn't in the release tarball (it needs GTK4 at build time) instead of skipping it in silence, and points at the source build. An upgrade previously left a stale popover next to freshly-updated binaries with no hint anything had been left behind.
- Every read of the cache is scoped to the enabled providers, so a disabled provider can't surface from a cache written before the toggle. This covers the no-daemon case, where nothing signals a reload; the bar's scroll rotation is scoped too, so scrolling no longer stops on a disabled provider.
- The popover's Refresh button works without a running daemon. It shelled straight to the daemon socket and silently did nothing when there was none; it now goes through `--refresh`, which falls back to a detached worker.

## [0.9.1] - 2026-07-16

### Fixed

- The daemon resolves `codexbar` again. Its systemd unit inherited a PATH without `~/.local/bin` - where the installer puts the binary - so every fetch failed to spawn it, and the stale fallback silently served frozen usage indefinitely. The unit now sets `Environment=PATH` to include the install dir.
- Stale fallback rounds are visible in the fetch log (`stale=N`) instead of reporting `errors=0` and reading like a clean fetch.

## [0.9.0] - 2026-07-15

Windows support and self-updating binaries.

### Added

- Native Windows 10+ support for `tokengauge-tui`, installed via the new `scripts/install.ps1` PowerShell installer.
- `tokengauge-tray`, a Windows system-tray GUI. Renders current session usage as the tray icon number/colour, click-to-open surfaces the full window, and it builds as a windowless GUI app so no console window flashes on launch.
- Self-update from GitHub releases. `--check-update` performs a live GitHub check, caches the result and prints JSON status without installing anything; `--update` downloads the latest release and swaps the installed binaries in place. The Plasma applet and tray GUI drive the same path from their "Update" buttons, and a one-shot desktop notification fires when an update is available.
- Win-CodexBar usable as a codexbar drop-in on Windows, so the Codex provider works there without a separate shim.

### Fixed

- All cached payloads are restored for a failed provider, not just the first - a provider erroring no longer drops the rest of its cached data.
- The Plasma update button resets on completion or failure instead of sticking in its in-progress state, and the update-flag reset is scoped so one applet's update doesn't clear another's.
- The Plasma applet matches the exact update source, so an update offered for one install target can't be applied against another.
- Update stderr is preserved rather than swallowed, so a failed update reports why.
- snake_case and float-percent codexbar JSON parse correctly - Win-CodexBar's output shape no longer trips the core parser.

### Changed

- Linux CI builds exclude `tokengauge-popover` instead of apt-installing GTK, and the tree is clean under current-stable rustfmt/clippy.

## [0.8.0] - 2026-07-14

Native KDE Plasma 6 applet - run TokenGauge as a panel widget instead of (or alongside) the Waybar module.

### Added

- Native KDE Plasma 6 applet (`org.tokengauge.plasmoid`): compact and full representations, provider/pin settings pane, installed via `scripts/install-plasma.sh`.
- Per-window limits on panel hover - the compact representation surfaces session/weekly limits without opening the full view.
- Waybar JSON bridge for non-waybar frontends. `tokengauge-waybar --json` emits the full snapshot as one enriched JSON object (label, brand SVG path, glyph, colour), and `--set-provider` / `--set-primary` let the applet edit config and signal the daemon.

### Fixed

- The plasmoid refetches stale data instead of serving the cache forever. `--json` now goes through `maybe_refresh` (serve-if-fresh / refetch-if-stale), so a standalone applet with no daemon keeping the cache warm still updates on its 60s timer.
- Config edits actually reach the running daemon. `--set-provider` / `--set-primary` now signal via `pkill -HUP -f 'tokengauge-waybar --daemon'` (the plain-name `pkill` matched nothing - 17-char name vs procps' 15-char `comm` cap). The reload helper is lifted into core so the popover and applet share one fix.
- A failed toggle surfaces its error - the applet chains the action flag and `--json` with `&&`, so a failed `--set-provider` reports stderr instead of being masked by the `--json` exit code.
- `install-plasma.sh` fails clearly on a missing asset dir (nullglob guard) instead of `set -e` aborting on an unexpanded glob.

## [0.7.0] - 2026-07-14

Waybar / codexbar parity release - the popover and core catch up to the Waybar module, plus resilience when providers misbehave.

### Added

- Claude CLI source fallback on OAuth error. When a provider's OAuth fetch fails, core falls back to the Claude CLI source instead of surfacing a blank error.
- Stale last-good cache on fetch failure. A transient `429` or network blip serves the last-good cached usage marked `stale` instead of a blank bar.
- Staggered provider fetches (`stagger_ms`). Config knob spreading codexbar calls out by `index * stagger_ms` for rate-limit relief; `0` disables (all at once).
- Real provider brand SVG logos in the popover, with the glyph as fallback.
- Inline settings pane in the popover: toggle OAuth providers and pick the bar-pinned provider live, comments preserved, daemon reloaded on change.

### Fixed

- Inline provider tables are no longer wiped on toggle. `providers = { codex = true }` configs keep their keys instead of being overwritten with an empty table.
- No duplicate stale rows when a provider returns mixed success/error sub-payloads or multiple error entries.
- The daemon reload signal now actually reaches the daemon. The 17-char binary name exceeds procps' 15-char `comm` cap, so `pkill tokengauge-waybar` matched nothing - the live provider/pin toggles never reloaded. Now `pkill -HUP -f 'tokengauge-waybar --daemon'`, and the child is reaped instead of leaking a zombie.
- Settings pane reflects edits immediately - a disabled provider drops from the bar-pin list without restarting the popover.
- Stagger sleep uses `saturating_mul` to remove a theoretical overflow panic.

## [0.6.3] - 2026-06-29

Patch release - waybar click + daemon environment fixes.

### Fixed

- Middle/back waybar clicks opening no browser tab. `--open` was routed through the daemon socket, and the daemon (a systemd `--user` service started at boot) runs with a stripped environment, so the browser it spawned could not reach the running instance. `--open` now runs in the waybar-invoked process, which has the full graphical session env.
- Daemon notifications being silently dropped. The daemon unit was `WantedBy=default.target`, so it started before the compositor imported `WAYLAND_DISPLAY` / `DBUS_SESSION_BUS_ADDRESS` / `BROWSER` into the systemd `--user` env. It is now bound to `graphical-session.target` (with `PartOf=`), and `install.sh` reenables the unit so upgrades drop the stale early-start symlink.

## [0.6.2] - 2026-05-27

### Fixed

- Provider fetch errors now surface the full anyhow cause chain. `failed to run codexbar for codex` previously hid the actual reason; the cache and tooltip now show e.g. `failed to run codexbar for codex: timeout after 10s`.

### Changed

- Default `timeout_secs` bumped from 10 to 20. Codexbar's typical fetch is 9-10s, so the old default raced the deadline and intermittently failed. Override in config if you need it tighter.

## [0.6.1] - 2026-05-27

### Fixed

- Waybar text stacked all providers on first boot (no scroll state, no `waybar.primary` set). It now defaults to the first provider; scrolling rotates as before. Tooltip and popover unchanged - both still surface every provider via their own tab UI.

## [0.6.0] - 2026-05-26

Native GTK4 popover + click action.

### Added

- Config-driven left-click action (`[waybar].click_action = "tui" | "popover"`). Waybar's `on-click` uniformly calls `tokengauge-waybar --click`; the binary dispatches based on config. `--doctor` reports the resolved launcher and warns when it isn't on `$PATH`.
- Bundled native GTK4 popover (`tokengauge-popover`) for `click_action = "popover"`: `gtk4-layer-shell` window anchored under waybar (margins configurable via `popover_margin_top` / `popover_margin_side`), codexbar-style provider tabs, a card per provider with proportional usage bars, monospace-aligned cost rows, a collapsible 7-day chart, `--toggle` second-click close (PID-file based), and an initial active tab that respects waybar's scroll selection.
- `scripts/eww-popup/`, a starter eww window for users who'd rather drive their own widget toolkit; set `popover_command = "eww open --toggle tokengauge-popup"`.
- Daemon SIGHUP reloads config + theme without restarting the systemd unit; socket protocol covered by 6 new tests.

### Changed

- TUI UX redesign: module split (`app.rs`, `ui.rs`, `refresh.rs`, `theme.rs`), `ratatui::init()` lifecycle, sidebar provider list + per-card layout, BarChart 7-day cost, popup help (`?`).
- Tooltip left-click hint reflects the configured action (`open TUI` vs `open panel`).

## [0.5.0] - 2026-05-26

Major feature batch on top of upstream v0.4.2.

### Added

- Daemon mode (`tokengauge-waybar --daemon`): long-lived Unix-socket service that owns fetch + cache writes. `install.sh` enables a systemd `--user` unit when available. Waybar polls become near-instant snapshots.
- Cost tracking via ccusage: today / month / 7-day / per-model breakdown / current burn rate \$/hr / 7-day sparkline / trend vs 7d average. Cost section mirrors Session / Weekly usage windows.
- Threshold notifications: `notify-send` alerts on configurable percentages (default 50/80/95). One-shot per threshold with reset on window roll-over.
- `--doctor`: diagnostic checklist for codexbar, ccusage runner, notify-send, xdg-open, providers (live fetch), waybar wiring.
- CSS tier classes: waybar class flips to `tokengauge-warn` (>=50%) / `tokengauge-crit` (>=80%) / `tokengauge-error` for theming.
- Mouse + key bindings: middle-click dashboard, back-button status, right-click refresh (with `⟳ Refreshing...` indicator), scroll rotate provider with debounce. TUI gains `h/l/Tab` tabs, `u` dashboard, `s` status.
- Config knobs: `waybar.primary`, `waybar.scroll_throttle_ms`, `ccusage_enabled`, `ccusage_timeout_secs`, `notifications.enabled`, `notifications.thresholds`.

### Changed

- Provider tabs in TUI with brand-coloured icons (Anthropic / OpenAI / GitHub Copilot / Z.ai).
- CodexBar-style hover popup with Session, Weekly (all), Weekly (Sonnet), Extra usage rows.
- Reset time renders with a days bucket when > 24h.
- Hover hint footer documents all mouse actions.

## [0.4.2] and earlier

Released by the upstream project, [oorestisime/TokenGauge](https://github.com/oorestisime/TokenGauge/releases). This fork's own history starts at 0.5.0.

[Unreleased]: https://github.com/Arzaroth/TokenGauge/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/Arzaroth/TokenGauge/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.3...v0.7.0
[0.6.3]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/Arzaroth/TokenGauge/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/Arzaroth/TokenGauge/compare/v0.4.2...v0.5.0
