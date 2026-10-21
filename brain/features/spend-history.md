# Spend history

The panel answers "what have I spent today, this week, this month". History
answers "and before that": 30 days, 90 days or 12 months as a chart, on a
**second screen** behind a toggle, because a year of bars does not belong above
the limit gauges. Rules not to regress: `CLAUDE.md` ("History is a second
screen, and the store is not sync's").

## The data was already there

The fleet store ([../architecture/sync.md](../architecture/sync.md)) keeps token
buckets for `STORE_RETENTION_DAYS` (400) and is the only record of a day once a
CLI rotates its transcript away. It used to be built only with `[sync] enabled`,
so a machine syncing with nobody had no past. It is now maintained on every fetch
(the gate is in `fetch.rs::native_costs`); sync gates only the cycle. History is a
rendering feature over that data, plus a backfill.

## Two granularities

Buckets stay hourly for `HOURLY_RETENTION_DAYS` (35), then roll up to one per UTC
day, provider and model (`Granularity::Day`): an hourly year is most of a
megabyte rewritten on every fetch. Two `const _: () = assert!(...)` bounds hold it:

- **Wider than the widest re-read** (31 days): `upsert_local` replaces this
  device's buckets from the window start on, so a rolled-up bucket inside that
  window would be landed beside, not replaced, and count twice.
- **No narrower than `WIRE_RETENTION_DAYS`**, so a contribution never carries a
  granularity no peer has seen.

A day bucket sits at **midday UTC**: at midnight `Hour::date_at` would read it as
the previous date for every reader west of the meridian. Offsets beyond +12 still
shift.

## A past month is rated at a past month's prices

History is stored as tokens, so money is a rating decision at render time. Rating
a year at today's prices lets a past month restate itself whenever LiteLLM moves
a number (`xai/grok-4` lost 6x of its output cost, `xai/grok-code-fast-1` gained
5x).

Most of what moves in that table is a missing field being filled in, not a vendor
change: `claude-sonnet-4-5` had no 1h cache-write price for its first year, so it
undercounted by about a quarter under a cache-heavy mix. For those, today's entry
is the better answer for the past too. So the **price archive**
(`cost/price-archive.json`) carries overrides only for prices a vendor really
moved, and a model it omits keeps today's price.

Enforced **field by field**: `PriceOverride` is sparse and merges onto today's
entry. A whole-entry override zeroed every field the old table lacked (the first
archive was missing `cache_read_input_token_cost` from 124 model-months).
`pricing::tests::the_vendored_archive_never_restates_a_current_price` guards it.
`scripts/make-prices.py` builds it from LiteLLM's git history (13 months, ~670
overrides, ~90 KiB), vendored rather than fetched. Months after the vendoring
release are rated at today's prices; `--doctor` says how far back it reaches.
The archive carries no long-context tiers: history is rated per hourly bucket,
which is never one request, so a tier is nothing it could apply
([../architecture/cost.md](../architecture/cost.md#pricing)).

## Backfill

A fetch reads back only to the start of the month, so a new store would hold a
fortnight. `cost::read_history` reads as far back as the store holds and
`sync::backfill` upserts it once, behind `tokengauge-backfilled`, before the first
fetch and never on a poll (it defeats `jsonl_files`'s mtime filter). The marker is
written even when nothing was found, but not while a transcript root exists and
cannot be read (`cost::Roots::unreadable`): then it reports the error unmarked
and runs again later, and every fetch's re-read keeps this device's buckets as
they were for the same reason
([../architecture/sync.md](../architecture/sync.md#the-fleet-store-is-the-durable-record)).
`--backfill` forces it again, e.g. after restoring transcripts. It replaces each
provider only from the oldest event its transcripts still hold, so the days
before that (rotated away, or adopted with `--sync-adopt`) survive, and a floor
inside a rolled-up day moves to the next.

## What the screens draw

`history::history_panel` resolves **every range at once**, each with finished
strings, a fraction per step and a tone, so switching range is a rebind. Each
frontend draws with its own primitive: ratatui `BarChart` (TUI, `H`), QML
`Canvas` (Plasma, Omarchy `.`), `St.DrawingArea` with Cairo (GNOME), egui
painting (tray, History button). The chart is the only part a frontend decides.

- A quiet step is a zero, not a missing bar.
- The step in progress is marked `partial` (dim tone, reduced alpha).
- The average excludes the partial step, as `CostInfo::avg_daily_cost` excludes
  today.
- **Waybar has no history screen, on purpose**: its tooltip cannot have a second
  screen; left-click opens the TUI.
  `every_frontend_with_a_second_screen_draws_the_history_series` records it.

`tokengauge --export [csv|json] [--since YYYY-MM-DD]` writes one row per day,
provider, model and device, through the same walk the chart uses.

## Hazards

1. **The store is the only copy.** A lost `tokengauge-fleet.json` is a year that
   will not come back; `--doctor` reports its size and reach.
2. **Zeros are ambiguous**: nothing spent, or no transcript survives. `covers`
   reports the read window rather than the oldest bucket.
3. **Retro-pricing is partial**: a change before the archive begins is rated at
   today's price; a field LiteLLM never filled is zero for every month.
4. **A fleet history mixes bases**: peer buckets are rated with this machine's
   archive, the reason dollars never cross the wire.

## Non-goals

Per-project history (needs a project key on every `UsageEvent`), editing or
annotating the past, budgets and forecasts, exports beyond the raw rows.

## Sources

- `crates/tokengauge-core/src/history.rs` (`history_panel`, `covers`)
- `crates/tokengauge-core/src/sync/fleet.rs` (retention constants, rollup, `upsert_local`), `crates/tokengauge-core/src/sync/hour.rs`, `crates/tokengauge-core/src/sync/run.rs` (`backfill`)
- `crates/tokengauge-core/src/cost/mod.rs` (`read_history`, `Roots::unreadable`), `crates/tokengauge-core/src/cost/pricing.rs`, `crates/tokengauge-core/src/cost/price-archive.json`, `scripts/make-prices.py`
- `crates/tokengauge-tui/src/history_view.rs`, `crates/tokengauge-tray/src/main.rs` (`draw_history_chart`), `crates/tokengauge-waybar/src/export.rs`
