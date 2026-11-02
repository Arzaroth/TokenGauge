# Window history

How full each window has been since it opened, kept so a pace can follow how
fast a window has been filling lately rather than assume it filled evenly
([pace.md](pace.md)). remuda reads it too, to draw a window's trail
([decision 0007](../decisions.md#0007-window-history-is-kept-beside-the-snapshot)).

## The trail

`tokengauge-window-history.json`, beside the snapshot (`history_path`), holds
one trail per provider, credential and window: `primary`, `secondary`,
`tertiary`, or an extra window's id (its title when it has none). A trail is
the window's current instance only. Each holds `windowMinutes`, the latest
`resetsAt` and `samples`, `[ms, percent]` pairs oldest first.

- **Recorded on every snapshot write**: `write_cache_full` hands the payloads to
  `window_history::record` before serialising them, so the daemon, the TUI, the
  tray and `--refresh` all record without knowing it.
- **One sample per change.** A value holds until the next sample. A sample is
  timed by the payload's `updated_at`, not the write: a stored credential's
  figures are carried between its own fetches, and a carried figure is not a
  new sample. A stale payload adds nothing.
- **A trail ends when its window resets**: when its reset has passed, or when
  the figure went down, which only a reset does (a banked one included). Trails
  whose window has reset are dropped, so a credential removed from the store
  leaves nothing behind for long.
- At most 2000 samples per trail; a weekly window fetched every ten minutes
  changes about a thousand times at most.
- Writing it does not bump `tokengauge-revision`: it changes nothing a frontend
  draws until the snapshot does.

## The burn

At the same write, each window in the snapshot gets `burn`: `perHour`, the
percent of the window spent per hour, and `overMinutes`, the stretch it was
measured over. The stretch is the last seventh of the window, an hour at least:
the last hour of five, the last day of a week. It starts at the latest sample
at or before the stretch's start.

A window is empty when it opens, so its start counts as a sample every trail
has. A trail younger than the stretch is measured from there, and with no
history at all the burn is the window's average since it opened, the rate the
pace always assumed. A figure that went down reads as no burn rather than a
negative one.

## Sources

- `crates/tokengauge-core/src/window_history.rs` (`record`, `Trail`, `burn`)
- `crates/tokengauge-core/src/snapshot.rs` (`write_cache_full`)
- `crates/tokengauge-core/src/payload.rs` (`UsageWindow.burn`, `Burn`)
