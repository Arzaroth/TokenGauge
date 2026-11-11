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
- **When each figure was first and last seen.** A new figure adds a sample, and
  the same figure seen again moves its last-seen sample forward, so a run of
  one figure is two samples at most. A trail only rises, so the figure held
  between two equal samples; between two that differ it moved at some point
  nobody saw, a gap the machine was off for included. A sample is
  timed by the payload's `updated_at` (never later than the write), not the
  write: a stored credential's
  figures are carried between its own fetches, and a carried figure is not a
  new sample. A stale payload adds nothing, and neither does a reading older
  than the trail's last sample, which an overlapping fetch can deliver late.
- **A trail ends when its window resets**: when its reset has passed, when
  the figure went down, which only a reset does (a banked one included), or
  when the reading's window opened after the trail's last sample, which is a
  reset that came early and was climbed past before the next fetch. A trail
  that went down with its reset still ahead, its window the same, was refilled
  in place: Claude's banked resets refill a week without moving its reset.
  The new trail records `openedAt`, the last moment the old one was seen, and
  the snapshot's window carries it ([pace.md](pace.md)). Trails
  whose window has reset are dropped, so a credential removed from the store
  leaves nothing behind for long.
- **A closed window leaves a record**: when a window of a day or longer ends,
  early or when due, `closed` gains its `provider`, `credential`, `window`,
  `windowMinutes`, `openedAt` (the trail's, else a whole window before its
  reset), the `resetsAt` it was due at, `used`, the last
  figure seen, `seenAt`, when it was seen, and `samples`, the trail up to
  then, with the `accountDigest` when there was one. A record is kept a year
  after its reset, its `samples` eight weeks: enough for remuda to learn
  which hours of a usual week spend a login, not a year of them rewritten at
  every fetch. The same window closed on two machines keeps both their
  samples. A trail that ends
  because its name now holds another account leaves none: its week did not
  close. Five-hour windows
  leave none: they close five times a day, and nothing reads them once closed.
- At most 2000 samples per trail; a whole percent that only rises makes about
  two hundred.
- A file that is there but cannot be read is left alone rather than written
  over; one that does not parse starts afresh.
- Writing it does not bump `tokengauge-revision`: it changes nothing a frontend
  draws until the snapshot does.
- **With fleet sync on, other machines fill in a view of it.** This file holds
  only what this machine saw, and is all it publishes. The trails and closed
  windows the fleet published for the same accounts are folded with it into
  `tokengauge-window-fleet.json`, rewritten at every write and every sync
  cycle and removed when there is no fleet or the history cannot be read, so a machine that was off reads
  the samples the others took meanwhile. The burn and the opening are taken
  from the view ([../architecture/sync.md](../architecture/sync.md#window-trails),
  [decision 0008](../decisions.md#0008-window-trails-ride-on-the-fleet-contribution)).

## The burn

At the same write, each window a fresh payload reports with a figure, a
duration and a reset gets `burn`, on the payloads the caller goes on to draw as
well as in the snapshot. A stale payload gets none, nor does a reading taken
before its window opened or exactly as it opened, which has no time to measure
over, one at or after its reset, or a late reading from an account other than
the one its trail now follows. `burn` holds `perHour`, the percent of the window spent per hour, and `overMinutes`, the stretch it was
measured over. The stretch is the last seventh of the window, an hour at least:
the last hour of five, the last day of a week, or since the window opened
when that is sooner: its `openedAt` when it was refilled in place. The figure
at the stretch's start is read off the trail: held between two equal samples,
on a straight line between two that differ. A week left at 0% for three days
then climbing is measured over its last day, not from its opening.

A window is empty when it opens, so its opening counts as a sample every
trail has. With no history at all the line runs from there to the reading, so
the burn is the window's average since it opened, the rate the pace always
assumed. `perHour` is not rounded.

## Sources

- `crates/tokengauge-core/src/window_history.rs` (`record`, `Trail`, `burn`, `fold`, `SharedWindows`)
- `crates/tokengauge-core/src/snapshot.rs` (`write_cache_full`)
- `crates/tokengauge-core/src/payload.rs` (`UsageWindow.burn`, `Burn`)
