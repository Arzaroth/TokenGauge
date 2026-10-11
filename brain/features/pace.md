# Pace

For every usage window (model-scoped weeklies included), where it lands at reset
if the current burn continues: `ends ~16%`, or `empty in 2h 15m` when it runs out
first. Shown next to each reset on every frontend, as a badge resolved in
`panel.rs`.

- Computed from one snapshot (used %, window duration, reset instant, now), so it
  needs no history. A port of CodexBar's `UsagePace.swift`.
- `UsagePace::for_window` returns nothing with no reset time, no duration, past
  the reset, before the window began, or under 3% of the window elapsed
  (`MIN_ELAPSED_FRACTION`): early numbers are noise.
- `delta_percent` is actual minus expected. Stages: on track within 2 points,
  slightly ahead/behind within 6, ahead/behind within 12, far beyond. The tone is
  `Tone::for_pace`, decided in core (the TUI once carried its own copy and it
  drifted).
- Countdown and pace are measured when the row is rendered, not at fetch time,
  which is why frontends re-render on a short cycle
  ([../architecture/snapshot.md](../architecture/snapshot.md)).

## Sources

- `crates/tokengauge-core/src/pace.rs` (`UsagePace`, `PaceStage`, `badge`)
- `crates/tokengauge-core/src/panel.rs` (`Tone::for_pace`, limit rows)
