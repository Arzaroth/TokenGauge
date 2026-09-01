# Spend history

The panel answers "what have I spent today, this week, this month". This
answers "and before that".

It is a second screen rather than another panel section: a year of bars does
not belong above the limit gauges, and every frontend that has a settings pane
already has the shape for it.

## 1. The data was already there

Fleet sync built a durable table of buckets keyed by device and hour, kept for
`fleet::STORE_RETENTION_DAYS` (400). It has always been the only record of a day
once a CLI rotates its transcript away - a contribution cannot be rebuilt from
transcripts, because `cost::window_start` reaches back only to the start of the
current month.

It was built only when `[sync] enabled`. That was the bug: a machine that syncs
with nobody had no past to look at. The store is now maintained on every fetch
either way, and sync gates only the cycle - publishing, pulling, peer events,
and the per-device split whose presence is what tells a reader the figures cover
more than this machine.

So history is a **rendering** feature over data the tool already kept, plus a
backfill to fill in what it had not kept yet.

## 2. Two granularities

Buckets stay hourly for `fleet::HOURLY_RETENTION_DAYS` (35) and are then rolled
up into one per UTC day, provider and model. An hourly year is most of a
megabyte rewritten on every fetch, and nothing that reads history that far back
reads the hour. The rollup produces the `Granularity::Day` variant that had been
reserved since the sync design and never emitted.

Two bounds hold it in place, both `const _: () = assert!(...)` rather than tests:

- **Wider than the widest re-read.** `upsert_local` replaces this device's
  buckets from `from` on, and the widest window `cost::read_window` asks for is
  31 days, on the last day of a month. A rolled-up bucket at or after that mark
  would be landed *beside* rather than replaced, and the day would count twice.
- **No narrower than the wire retention.** Otherwise a contribution would carry
  a granularity no peer has been sent before.

A day bucket sits at **midday** UTC. It has no hour left to place it by, and at
midnight `Hour::date_at` would read it as the previous date for every reader
west of the meridian, shifting a year of history one day left in Montreal.
Offsets beyond +12 still shift; that is the same class of edge as the sub-hour
offsets sync already documents.

## 3. A past month is rated at a past month's prices

History is stored as tokens, so money is a rating decision made at render time.
Rating a year against today's table means a past month silently restates itself
whenever LiteLLM moves a number, and over the last year that was not a rounding
error: `xai/grok-4` lost 6x of its output cost, `xai/grok-code-fast-1` gained 5x.

The complication is that **most of what moves in that table is a missing field
being filled in, not a vendor changing anything**. `claude-sonnet-4-5` carried no
`cache_creation_input_token_cost_above_1hr` for its first year, so every read of
it fell back to the 5m price and undercounted by about a quarter under a
cache-heavy mix. The 1h price was always what it is now, merely unrecorded - so
for that case *today's* entry is the better answer for a past month too.

The two are indistinguishable from the table alone, so the archive carries
overrides **only for prices a vendor really moved**, and a model it says nothing
about keeps today's price. `scripts/make-prices.py` builds it by walking
LiteLLM's own git history: 13 months, ~670 overrides, ~90 KiB, vendored into the
binary rather than fetched, because a month that is over does not change its mind
and a cold machine should not need a request per month to rate a year.

Months after the release that vendored it carry no overrides and are rated at
today's prices, which is what every figure did before this existed. `--doctor`
says how far back the archive reaches.

## 4. Backfill

A fetch reads back only to the start of the month, so a store created today holds
a fortnight however many months of transcripts the CLIs kept. Without a backfill
the feature would ship empty and take a year to become worth opening.

`cost::read_history` reads as far back as the store can hold and `sync::backfill`
upserts the lot, once, behind `tokengauge-backfilled`. It is slow by
construction - reaching back a year defeats the mtime filter `jsonl_files` leans
on, so it opens most of the tree where a fetch opens a handful of files - which
is why it runs before the first fetch and never on a poll. `--backfill` forces it
again, which is what to reach for after restoring transcripts from a backup.

The marker is written whether or not the read found anything, or a machine with
no transcripts would re-walk the whole tree on every fetch forever.

## 5. What the screens draw

`history::history_panel` resolves **every range at once** (30 days, 90 days, 12
months), each carrying finished display strings, a fraction per step, and a tone.
Switching range is then a rebind in an open pane rather than another `--json`.

Five frontends draw it, each with its own chart primitive: ratatui's `BarChart`
in the TUI, a QML `Canvas` in Plasma and Omarchy, an `St.DrawingArea` painted
with Cairo in GNOME, and an `egui::Painter` in the tray. The chart is the only
part a frontend decides; every string on the screen is the core's.

Three details are resolved centrally because all five would otherwise get them
wrong separately:

- **A quiet step is a zero, not a missing bar.** Otherwise a quiet fortnight
  draws as a narrower chart instead of as a quiet fortnight.
- **The step in progress is marked `partial`.** Its bar is short because it is
  not over, and every chart that does not say so ends on a cliff the reader takes
  for a collapse. Frontends draw it at reduced alpha and the spec gives it the
  dim tone.
- **The average excludes the partial step**, on the same reasoning
  `CostInfo::avg_daily_cost` already excludes today.

### Waybar has no history screen, on purpose

Its tooltip is a hover surface with no second screen and no way to gain one.
A waybar user's history is the TUI, which left-click has opened since long before
there was any history to open it for. `panel.rs`'s
`every_frontend_with_a_second_screen_draws_the_history_series` records the
decision and is the list to add it to if that ever changes.

## 6. Hazards

1. **The store is the only copy.** Once transcripts rotate away, a lost or
   corrupted `tokengauge-fleet.json` is a year that will not come back.
   `--doctor` reports its size and reach for that reason.
2. **Zeros are ambiguous.** A month of zeros means either nothing was spent or
   no transcript survives to say otherwise, and TokenGauge cannot tell which.
   `covers` reports the read window rather than the oldest bucket, so a quiet
   stretch does not read as missing data - which is the right default and still
   not a distinction the data supports.
3. **Retro-pricing is partial.** See §3: a real price change before the archive
   begins is rated at today's price, and a field LiteLLM has never filled in is
   rated at zero for every month.
4. **A fleet history mixes bases.** Peer buckets arrive as tokens and are rated
   with this machine's archive, which is the same rule the fleet total already
   follows and the reason dollars never cross the wire.

## 7. Non-goals

Per-project or per-repo history (a separate feature: the readers would have to
carry a project key on every `UsageEvent`). Editing or annotating the past.
Budgets and forecasts. Exporting anything but the raw rows - `--export` writes
one row per day, provider, model and device and stops there, because every
consumer past that point disagrees about what it wants.
