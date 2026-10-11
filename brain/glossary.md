# Glossary

One-stop definitions for the domain and technical terms used across the brain and
the code, grouped by area. Each term is the one to use in code, docs and
commits; the **Avoid** list names the synonyms that blur it.

## Usage and limits

- **Provider** - one coding-CLI vendor TokenGauge reads from, such as claude or
  codex. The unit of enable/disable everywhere in config, and of the cost
  readers and the brand. See [features/providers.md](features/providers.md).
  Avoid: backend, service, account.
- **Window** - a provider-reported rate limit over a fixed span, with a
  percentage used and a reset instant. Credential-scoped, so it reads the same on
  every machine. See [architecture/snapshot.md](architecture/snapshot.md).
  Avoid: quota, limit period.
- **Credential** - one set of tokens signed in to a provider, carrying its own
  windows and plan. A provider holds one or more. Its identity is how it is
  matched, and two credentials can share one. See
  [architecture/credentials.md](architecture/credentials.md).
  Avoid: account, auth file, login.
- **Active credential** - the credential the provider's own CLI is signed into
  right now, read from the CLI's live source. Avoid: current, default, primary.
- **Credential store** - the directory of every captured credential, a stored
  copy of the active one included, that a switcher tool (remuda) writes and
  TokenGauge only reads. Avoid: vault, keyring, pool.
- **Inactive credential** - a stored credential the CLI is not signed into.
  Asked on its own, at a slower cadence than the active one, and never refreshed
  by TokenGauge. Avoid: spare, secondary, other account.
- **Sidecar** - `<name>.meta.json` beside a stored credential: its `accountId`,
  `label`, `capturedAt` and `credsDigest`. Trusted only when `credsDigest`
  matches the credential file's bytes. See
  [architecture/credentials.md](architecture/credentials.md).
- **Credential state** - `expired` or `unverified` on a stored credential's
  payload that was not asked: a payload, not an error.
- **Plan weight** - a plan's nominal multiplier against the provider's base paid
  plan (Claude Pro 1, Max 5x 5, Max 20x 20, Team 1.25 or 6.25; ChatGPT Plus 1,
  Pro 100 5, Pro 200 10, Pro 500 25; Cursor Pro 1, Pro+ 3, Ultra 20; Kimi Plus 1,
  Pro 2, Max 5, Ultra 10), its allowance in the provider's own unit (GLM weekly
  credits by thousand: Lite 10, Pro 60, Max 140), or, where none is published,
  its price (SuperGrok 3, Plus 10, Heavy 30). What the combined header weighs a
  credential by. Unknown for a plan sold as no multiple, which stays out of the
  total. See [features/several-plans.md](features/several-plans.md).
  Avoid: quota, share, capacity.
- **Combined header** - the `plans` section (titled ALL PLANS) drawn above a
  provider's credential groups: each window several weighted credentials report,
  in units of the largest plan. An estimate, and labelled as one.
  Avoid: total, aggregate, sum.
- **Split bar** - a combined-header meter drawn as one segment per credential,
  each as wide as its share of the total (never under a tenth) and filled to its
  own usage. `PanelRow.segments`. Avoid: stacked bar, multi-bar.
- **Pace** - where a window lands at reset if the current burn continues
  (`ends ~16%`, or `empty in 2h 15m`). Hidden until 3% of the window has
  elapsed. See [features/pace.md](features/pace.md).

## Panel and frontends

- **Surface / frontend** - one of the six places the gauge is drawn: waybar,
  Plasma, GNOME, Quickshell (the Omarchy widget), the tray, the TUI. See
  [architecture/frontends.md](architecture/frontends.md).
- **Snapshot** - the single state file every frontend renders from, holding
  provider payloads, errors and costs as of the last fetch. State, not cache.
  See [architecture/snapshot.md](architecture/snapshot.md). Avoid: cache.
- **Panel spec** - the ordered sections and rows resolved once in `panel.rs`,
  carrying finished display strings, fractions and tones. The abstraction that
  keeps the six frontends in agreement. See
  [architecture/panel-spec.md](architecture/panel-spec.md).
  Avoid: layout, view model.
- **Section / SectionKind** - one block of the panel spec (`status`, `plans`,
  `limits`, `cost`, `tokens_by_day`, `tokens_by_model`, `tokens_by_device`), drawn
  with one of three primitives: `Meters`, `Bars`, `Rows`.
- **Tone** - a semantic tier a row carries (good, warn, critical, dim, normal)
  that each frontend maps onto its own palette. Avoid: colour, severity.
- **Chrome** - what stays per-frontend: header, update banner, provider selector,
  settings pane, input hints. Everything a user reads is content and comes from
  the core.
- **Revision file** - `tokengauge-revision`, rewritten on every snapshot write;
  frontends watch it and re-run `--json` when it moves.
- **Stale** - a payload served from the previous snapshot after its fetch failed,
  carrying `stale_reason`; also the fetch-or-serve verdict of `cache_is_stale()`.

## Cost

- **Usage event** - one billed call, produced by every transcript reader and
  consumed by everything downstream. Adding a source means adding a reader and
  nothing else. See [architecture/cost.md](architecture/cost.md).
  Avoid: record, entry, sample.
- **Reader** - a parser for one CLI's transcript tree that emits usage events.
  Avoid: parser, importer, collector.
- **Natively read** - `ProviderMeta.natively_read`: a reader exists for the
  provider. Gates fleet sync and the `--doctor` drift check.
- **ccusage** - the external cost tool, now the fallback and the cross-check, not
  the source.
- **Unpriced** - tokens for a model the price table does not carry; proof the
  table is behind, which buys one out-of-window refetch.

## Fleet sync

- **Fleet** - the set of machines sharing one sync key, whose usage is summed into
  one panel. See [features/fleet-sync.md](features/fleet-sync.md).
  Avoid: cluster, group, team.
- **Device** - one machine in the fleet, identified by a derived id and a user-set
  label. Avoid: host, node, client.
- **Peer** - a device other than this one. Used only where the distinction
  changes behaviour. Avoid: remote, other.
- **Bucket** - tokens billed for one provider and model within one UTC hour (or,
  rolled up, one UTC day). The unit that crosses machines.
  Avoid: sample, slice, aggregate.
- **Contribution** - the encrypted document one device publishes, holding its own
  buckets and nothing else. See [architecture/sync.md](architecture/sync.md).
  Avoid: share, export, payload.
- **Fleet store** - the local table of buckets keyed by device and hour, covering
  every device including this one. The durable record; transcripts and
  contributions are its inputs. Kept whether or not sync is on: with sync off the
  fleet is just this machine, and the store is what history is read from.
  Avoid: peer cache, merge cache.
- **Sync key** - the symmetric secret shared by every device in a fleet.
  Possession of it is membership. Avoid: token, password, secret.
- **Envelope** - the sealed contribution format, `TGSYNC1` header plus
  XChaCha20-Poly1305 ciphertext. Its `key_id` lets a file under another key be
  skipped by name.
- **Transport** - where contributions live: `dir` (a synced folder) or `s3`.

## History

- **Series** - one range of spend, resolved as an ordered list of steps with no
  gaps. The unit a chart draws. See
  [features/spend-history.md](features/spend-history.md).
  Avoid: dataset, chart data.
- **Step** - one bar of a series: a day or a month, carrying finished display
  strings, a fraction and a tone. Avoid: point, bucket, sample.
- **Range** - how far back a series reaches and what it steps by: 30 days, 90
  days, 12 months. The thing the pane's selector switches.
  Avoid: period, window (a window is a provider's rate limit).
- **Partial step** - the step still in progress. Marked so a chart does not end
  on a cliff the reader takes for a collapse. Avoid: current, incomplete, today.
- **Price archive** - per-month overrides for the models whose price a vendor
  actually moved, so a past month is rated at a past month's prices.
  Avoid: price history, old prices.
- **Backfill** - the one-time deep read of every transcript still on disk,
  filling the store with history a fetch's window never reaches.
  Avoid: import, initial sync, catch-up.

## Ecosystem

- **remuda** - the credential switcher ([Arzaroth/remuda](https://github.com/Arzaroth/remuda))
  that writes the credential store and serves a local page. See
  [features/remuda-page.md](features/remuda-page.md).
- **selvedge** - the updater and payload-installer crate shared with TailGauge,
  also home of the ALL PLANS arithmetic (`selvedge::plans`). See
  [architecture/distribution.md](architecture/distribution.md).
- **LiteLLM** - the upstream model price table `pricing.rs` follows.
- **CodexBar** - the macOS project the provider protocols and `pace.rs` were
  ported from; watched by the `upstream-check` skill.
