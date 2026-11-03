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
`resetsAt`, `samples`, `[ms, percent]` pairs oldest first, and the
credential's `accountDigest` when it has one: a name re-captured for another
account starts its own trail.

- **Recorded on every snapshot write**: `write_cache_full` hands the payloads to
  `window_history::record` before serialising them, so the daemon, the TUI, the
  tray and `--refresh` all record without knowing it.
- **One sample per change.** A value holds until the next sample. A sample is
  timed by the payload's `updated_at` (never later than the write), not the
  write: a stored credential's
  figures are carried between its own fetches, and a carried figure is not a
  new sample. A stale payload adds nothing, and neither does a reading older
  than the trail's last sample, which an overlapping fetch can deliver late.
- **A trail ends when its window resets**: when its reset has passed, when
  the figure went down, which only a reset does (a banked one included), or
  when the reading's window opened after the trail's last sample, which is a
  reset that came early and was climbed past before the next fetch. Trails
  whose window has reset are dropped, so a credential removed from the store
  leaves nothing behind for long.
- At most 2000 samples per trail; a weekly window fetched every ten minutes
  changes about a thousand times at most.
- A file that is there but cannot be read is left alone rather than written
  over; one that does not parse starts afresh.
- Writing it does not bump `tokengauge-revision`: it changes nothing a frontend
  draws until the snapshot does.

## The burn

At the same write, each window a fresh payload reports with a figure, a
duration and a reset gets `burn`, on the payloads the caller goes on to draw as
well as in the snapshot. A stale payload gets none, nor does a reading taken
before its window opened or exactly as it opened, which has no time to measure
over, one at or after its reset, or a late reading from an account other than
the one its trail now follows. `burn` holds `perHour`, the percent of the window spent per hour, and `overMinutes`, the stretch it was
measured over. The stretch is the last seventh of the window, an hour at least:
the last hour of five, the last day of a week. It starts at the latest sample
at or before the stretch's start.

A window is empty when it opens, so its start counts as a sample every trail
has. A trail younger than the stretch is measured from there, and with no
history at all the burn is the window's average since it opened, the rate the
pace always assumed. That includes a window whose figure went down: its new
trail has nothing before the stretch, so its burn is the average since the
window opened until the trail has history. `perHour` is not rounded.

## Sources

- `crates/tokengauge-core/src/window_history.rs` (`record`, `Trail`, `burn`)
- `crates/tokengauge-core/src/snapshot.rs` (`write_cache_full`)
- `crates/tokengauge-core/src/payload.rs` (`UsageWindow.burn`, `Burn`)
