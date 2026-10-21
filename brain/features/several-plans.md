# Several plans per provider

One person can hold several plans at one vendor (a work Max 20x and a personal
Pro). With [remuda](https://github.com/Arzaroth/remuda) keeping the logins,
TokenGauge shows every one of their limits, the active one first, and adds them
up by plan multiplier. The mechanism and the remuda contracts are
[../architecture/credentials.md](../architecture/credentials.md); the decision is
[0003 in ../decisions.md](../decisions.md#0003-credentials-under-a-provider).

## What the panel shows

- The provider stays one tab. Its panel gets one `limits` group per credential,
  titled with the store name, remuda's label, the plan and the markers (`active`,
  `not in total`), active first. The bar keeps showing the plan the CLI is signed
  into.
- An expired or unverified credential is a `Rows` group with one line saying
  which and why, not a provider error.
- **ALL PLANS** (the `plans` section, `Meters`): one meter per window label at
  least two weighted credentials report, valued `"<used>% of <capacity>%"` in
  units of the largest plan, filled to the pooled fraction
  `Σ used × weight ÷ Σ weight`, tinted by it, badge `estimate`. A Max 20x and a
  Pro both spent read `105% of 105%`; Max 20x at 50% beside Max 5x at 100% reads
  `75% of 125%` (pooled 60%). Its reset is the earliest among them; the tooltip
  lists each credential's own figure and reset. A window that reset since its
  payload was written counts as empty, not spent, so a credential that is not
  asked again cannot hold the pool up (`rows::expire_elapsed`,
  [../architecture/snapshot.md](../architecture/snapshot.md#how-a-change-reaches-a-frontend)).
- Bar icon hover: the active group's windows, then the combined figures, then
  today's spend.
- Session cost and threshold notifications follow the active credential only.

## Plan weights

Nominal multipliers the plans are sold with, so the combined figure is an
estimate. A plan with no known weight stays out of the total and is marked
`not in total`, never guessed at 1x.

| Provider | Base | Weights |
| --- | --- | --- |
| Claude (`claude.rs`) | Pro | Pro 1; an `Nx` tier N (Max 5x 5, Max 20x 20); Team standard 1.25, premium 6.25 (`selvedge::plans::team_seat`): a `userRateLimitTier` naming an `Nx` is premium, else `oauthAccount.seatTier` when it is `team_premium` / `team_standard`, else the token. Enterprise none |
| Codex (`codex.rs`, wire `plan_type`) | Plus | `plus` 1, `prolite` (Pro 100) 5, `pro` (Pro 200) 10, `promax` (Pro 500) 25, `team` (Business) 1, `self_serve_business_prolite` 5. `business` (Enterprise), free, Go, EDU none |
| GLM (`glm.rs`) | weekly credits / 1000 | Lite 10, Pro 60, Max 140 |
| Cursor (`cursor.rs`) | Pro | Pro 1, Pro+ 3, Ultra 20; team seats none |
| Kimi (`kimi.rs`, `membership.level`) | Plus | Plus 1, Pro 2, Max 5, Ultra 10; legacy `GOODS_VERSION_V1` none |
| Grok (`grok.rs`, `subscription_tier_display`) | price | SuperGrok 3, Plus 10, Heavy 30 |
| opencode Go | | none (the tier is not on the wire) |

The arithmetic (units of the largest plan, half-even rounding, pooled fill,
split-bar widths) and the Team seat rule are `selvedge::plans`, shared with
remuda's page.

## `[panel]` options

From `config.toml`, `tokengauge --set-panel KEY=VALUE`, or the settings pane of
Plasma, GNOME, Omarchy and the tray (waybar and the TUI use the file):

- `active_credential_only = false` - draw only the active group plus any stale,
  expired, unasked or out-of-total group. ALL PLANS still adds them all; its title
  says how many ("ALL PLANS · 5 credentials").
- `plans_total = "weighted" | "absolute"` - absolute counts every asked credential
  100% and needs no weight (five plans read `409% of 500%`).
- `split_bars = true` - one segment per credential under each ALL PLANS meter,
  each as wide as its share (a tenth at least) and filled to its own usage.

A value that does not parse reads as its default.

## Cadence and freshness

The live login is asked every refresh; an inactive credential every
`inactive_refresh_secs` (1800) or when a window resets, and as often as the live
login while its 5-hour window is past 10%, since that is another machine
spending from it. Adding a credential or
`remuda use` refreshes the panel immediately (the snapshot goes stale on a store
change). `--doctor` checks every stored credential offline.

## Sources

- `crates/tokengauge-core/src/credentials.rs`, `crates/tokengauge-core/src/rows.rs`
- `crates/tokengauge-core/src/panel.rs` (the `plans` section, `Section.group`, `segments`, `tests::the_combined_header_weighs_each_plan_by_its_multiplier`)
- `crates/tokengauge-core/src/{claude,codex,glm,cursor,kimi,grok}.rs` (`plan_weight` / plan tables)
- `crates/tokengauge-core/src/config.rs` (`PanelConfig`, `CredentialsConfig`)
