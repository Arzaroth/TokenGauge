# Threshold notifications

Desktop alerts when a window crosses a usage threshold, plus the one-shot update
notification.

- Config: `[notifications] enabled` (default true), `thresholds` (default
  `[50, 80, 95]`).
- One-shot per threshold per window; the state (`tokengauge-notify-state.json`)
  re-arms when the window's reset instant changes (keyed on the raw `resets_at`,
  not the formatted countdown).
- Only the plan in use notifies: a payload with an error or `active: false` is
  skipped, since a notification names the provider and window and would fire in
  the active plan's name. The state key includes the credential name, so
  switching plans neither replays nor swallows thresholds.
- Fired by the daemon when one runs (even if no frontend is open), otherwise by
  the fetching process. `launch::notify` uses `notify-send` on Linux and
  `osascript` (Notification Center) on macOS.
- The daemon also checks GitHub every `update.check_interval_secs` and notifies
  once when a newer release exists (`update.check = false` opts out). See
  [updates.md](updates.md).

## Sources

- `crates/tokengauge-waybar/src/snapshot.rs` (`check_and_notify`, `notifies`, `notify_key`, `fire_notification`)
- `crates/tokengauge-waybar/src/daemon.rs` (`notify_update_available`)
- `crates/tokengauge-core/src/launch.rs` (`notify`), `crates/tokengauge-core/src/statefiles.rs` (`notify_state_path`)
