# Cost tracking

The COST and TOKENS sections: what was spent, read from local transcripts and
rated against LiteLLM's prices. Mechanism (readers, traps, pricing, ccusage):
[../architecture/cost.md](../architecture/cost.md).

## What the panel shows

- **COST** (`Rows`): today, month, 7-day rolling, the session cost and burn rate
  ($/hr) anchored to the active credential's real session window
  (`cost::anchor_burn_rates`), today vs the average of prior days (today
  excluded from the average). A `Sync` row when fleet sync is on.
- **TOKENS BY DAY / BY MODEL / BY DEVICE** (`Bars`): per-day and per-model shares;
  the device split appears only for a fleet-merged provider
  ([fleet-sync.md](fleet-sync.md)).
- OpenRouter's figures are its own billing numbers (`ReportedCost`), not a
  transcript estimate.

## Config

`ccusage_enabled` (master switch for cost figures), `cost_source`
(`auto` / `native` / `ccusage`), `ccusage_timeout_secs` (also the price-refresh
timeout).

## Known gaps

- Transcripts carry no credential, so cost is provider-scoped, never
  per-credential (decision 0003).
- A provider with no reader (GLM from z.ai's CLI, OpenRouter, opencode, Cursor)
  relies on ccusage or the vendor, and cannot sync.
- Cost by project is a `TODO.md` idea: readers walk cwd-keyed trees but
  `UsageEvent` carries no project.

## Sources

- `crates/tokengauge-core/src/cost/mod.rs` (`build_report`, `anchor_burn_rates`), `crates/tokengauge-core/src/cost/pricing.rs`
- `crates/tokengauge-core/src/panel.rs` (`cost`, `tokens_by_*` sections)
- `crates/tokengauge-core/src/openrouter.rs` (`ReportedCost`), `crates/tokengauge-core/src/ccusage.rs`
