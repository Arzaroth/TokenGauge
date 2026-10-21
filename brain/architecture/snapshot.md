# Snapshot, staleness and the daemon

The one state file every frontend renders from, the single decision about when
to fetch, and how a change reaches a frontend. The regression rules with their
incidents are in `CLAUDE.md` ("Snapshot, staleness, and how a change reaches a
frontend").

## The snapshot and its siblings

The snapshot lives at `$XDG_STATE_HOME/tokengauge/tokengauge-usage.json`
(`cache_file`; `%LOCALAPPDATA%\TokenGauge\...` on Windows). It is state, not
cache: it holds the only record of past days' tokens and costs. Every other
state file is derived from its **parent** (`statefiles.rs`), so pointing
`cache_file` elsewhere moves the whole set and a test gets a directory of its
own:

| File | What |
| --- | --- |
| `tokengauge.sock` | the daemon socket (Unix) |
| `tokengauge-revision` | bumped on every write; what frontends watch |
| `tokengauge-refreshing` | the refresh sentinel (a fetch is in flight) |
| `tokengauge-notify-state.json` | which notification thresholds already fired |
| `tokengauge-prices.json`, `tokengauge-prices-missed.json` | the fetched price table and the unpriced set ([cost.md](cost.md)) |
| `tokengauge-fleet.json`, `tokengauge-fleet.lock`, `tokengauge-backfilled` | the fleet store, its writer lock and the backfill marker ([sync.md](sync.md)) |
| `tokengauge-device-id`, `tokengauge-sync-key` | device identity and fleet key |
| `tokengauge-update.json` (`update_status_path`) | the cached update check ([distribution.md](distribution.md)) |
| `tokengauge-waybar-state.json` | the selected provider and waybar scroll state (keeps its old name) |

`CACHE_SCHEMA_VERSION` is 2 (payloads name their credential). It is written and
never checked on read.

## `cache_is_stale()` is the single fetch-or-serve decision

A snapshot is stale when it is:

- missing, or older than `refresh_secs` (default 600);
- written before a provider enabled now was switched on (`CacheMeta.providers`);
- reporting a window that has **reset since the write** (compared against the
  write, not against now alone, or a provider reporting a past instant would
  refetch on every render);
- older than a credential-store change: a credential added, removed or renamed
  against `CacheMeta.credentials`, or a switch in remuda's `.last-switch.json`
  later than `credentialsAtMs` (see [credentials.md](credentials.md)).

`retain_enabled()` handles the other direction, filtering a disabled provider out
of an otherwise fresh snapshot. `apply_stale_fallback` serves the last good
payload when a fetch fails, marked `stale` with `stale_reason`.

## How a change reaches a frontend

Every write goes through `write_cache_full`: atomic write, then rewrite
`tokengauge-revision`. Frontends watch it (Quickshell `FileView`, GNOME
`Gio.FileMonitor`, and `--wait-change` long-polling for Plasma, whose toolkit has
no watcher) and re-run `--json`. Their poll timers stay: with no daemon, a poll
is what ages the snapshot out and triggers the next fetch.

**Rendering is not fetching.** Reset countdowns are measured when `panel.rs`
builds the row, so every frontend re-renders on a short cycle while visible
(Omarchy, Plasma, GNOME re-run `--json` every 30s; TUI and tray rebuild every
15s; the daemon renders each socket request fresh). None of that asks a provider
anything.

## The daemon

`tokengauge --daemon` (systemd user unit `scripts/tokengauge-daemon.service`, or
the `org.tokengauge.daemon` LaunchAgent on macOS) is optional: it serves the bar,
the tooltip and `--json` over the socket, runs threshold notifications and the
update check, and reloads config on SIGHUP (`signal_daemon_reload` matches
`tokengauge(-waybar)? --daemon`).

- **Who fetches matters.** `--json` asks the daemon and fetches in-process only
  when there is none, because a frontend's subprocess inherits the compositor's
  environment while the daemon inherits the unit's, and `environment.d` (where an
  S3 credential lives) reaches only the latter.
- The daemon's wait between fetches re-checks `cache_is_stale()` every 15s rather
  than sleeping `refresh_secs`, so a mid-cycle window reset is noticed.
- **Setters.** `--set-provider` fetches before it returns, so a chained
  `--set-provider && --json` sees the new provider. `--set-panel` and
  `--set-primary` wait (two seconds at most) for the daemon to move the revision
  after its SIGHUP reload, or a switch reads back its old state.
- Dev trap: a freshly built binary run with no flags or `--json` proxies to the
  **installed** daemon. Point `--config` at a config whose `cache_file` lives
  elsewhere to exercise new panel code.

## Sources

- `crates/tokengauge-core/src/snapshot.rs` (`cache_is_stale`, `write_cache_full`, `retain_enabled`, `CacheMeta`, `CACHE_SCHEMA_VERSION`)
- `crates/tokengauge-core/src/statefiles.rs`, `crates/tokengauge-core/src/config.rs`
- `crates/tokengauge-core/src/fetch.rs` (`fetch_all_providers`, `apply_stale_fallback`)
- `crates/tokengauge-waybar/src/daemon.rs`, `crates/tokengauge-waybar/src/snapshot.rs`, `crates/tokengauge-waybar/src/main.rs`
- `crates/tokengauge-core/tests/state_files.rs`
