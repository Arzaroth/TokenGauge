# Features

The product catalogue. Each row links to a detailed doc; every feature lands on
all six surfaces unless the doc records the exception. Back to the root map:
[../BRAIN.md](../BRAIN.md).

| Feature | What it is | Key files |
|---|---|---|
| [Providers](providers.md) | The eight vendors read, their credentials, windows, readers and store support | `providers.rs`, `{claude,codex,kimi,grok,glm,openrouter,opencode,cursor}.rs` |
| [Several plans](several-plans.md) | Every stored credential's limits, ALL PLANS header weighted by plan multiplier, split bars, `[panel]` options | `credentials.rs`, `rows.rs`, `panel.rs`, `plan_weight` |
| [Cost tracking](cost-tracking.md) | COST and TOKENS sections from local transcripts: today, month, 7-day, session, burn rate | `cost/`, `panel.rs` |
| [Spend history](spend-history.md) | 30 d / 90 d / 12 mo chart on a second screen, past months at past prices, backfill, export | `history.rs`, `sync/fleet.rs`, `price-archive.json` |
| [Fleet sync](fleet-sync.md) | Fleet-wide cost and tokens, TOKENS BY DEVICE, setup screen and commands | `sync/`, `sync_cli.rs`, `sync_view.rs` |
| [Pace](pace.md) | Projected usage at reset per window (`ends ~16%`, `empty in 2h`) | `pace.rs` |
| [Notifications](notifications.md) | Threshold alerts per window, active plan only | `waybar/src/snapshot.rs`, `launch.rs` |
| [remuda's page](remuda-page.md) | Header button that opens `remuda serve`'s page via `remuda open` | `remuda.rs`, `launch.rs` |
| [`--doctor`](doctor.md) | Diagnostic checklist across credentials, cost, sync, wiring, updates | `doctor.rs` |
| [Updates](updates.md) | Self-update, daemon update check, Update button | `project.rs`, selvedge |

## Cross-cutting architecture

Every feature sits on the [panel spec](../architecture/panel-spec.md) and the
[snapshot](../architecture/snapshot.md); see
[../architecture/](../architecture/index.md) for credentials, cost readers, sync,
frontends and distribution.
