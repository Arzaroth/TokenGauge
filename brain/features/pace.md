# Pace

For every usage window (model-scoped weeklies included), where it lands at reset
if the current burn continues: `ends ~16%`, or `empty in 2h 15m` when it runs out
first. Shown next to each reset on every frontend, as a badge resolved in
`panel.rs`. A port of CodexBar's `UsagePace.swift`.

- **The projection follows the recent burn.** A window carrying a `burn`
  ([window-history.md](window-history.md)) is projected at it: `used + burn x
  time left`, and empty when that passes 100% before the reset, or now when it
  is already full, however quiet it has gone. A morning's burst
  followed by a quiet afternoon ends near where it stands rather than being
  extrapolated at its morning rate. A window with no burn is projected at its
  average since it opened, which is also what the burn is with no history.
- **Ahead or behind needs no history**: used % against the even rate
  (`elapsed / duration`), from one snapshot. A window refilled in place runs
  from its `openedAt` to its reset: three days after a banked reset, 0% is on
  track, not far behind. A window that reset when due runs its full length
  however late it was first spent from, since days left idle are quota lost.
- `UsagePace::for_window` returns nothing with no reset time, no duration, past
  the reset, before the window began, or under 3% of the window elapsed
  (`MIN_ELAPSED_FRACTION`): early numbers are noise.
- `delta_percent` is actual minus expected. Stages: on track within 2 points,
  slightly ahead/behind within 6, ahead/behind within 12, far beyond. The tone is
  `Tone::for_pace`, decided in core (the TUI once carried its own copy and it
  drifted): behind is good, ahead is warn, critical past 6 points ahead.
- **Early cap.** `UsagePace.early` is set in the first 15% of a window
  (`EARLY_FRACTION`) with at most 50% used; an early ahead tops out at warn,
  because a morning's burst projects to a run-out days away. The badge is
  unchanged, only its tier.
- Countdown and pace are measured when the row is rendered, not at fetch time,
  which is why frontends re-render on a short cycle
  ([../architecture/snapshot.md](../architecture/snapshot.md)).

## Sources

- `crates/tokengauge-core/src/pace.rs` (`UsagePace`, `PaceStage`, `badge`, `EARLY_FRACTION`)
- `crates/tokengauge-core/src/panel.rs` (`Tone::for_pace`, limit rows)
