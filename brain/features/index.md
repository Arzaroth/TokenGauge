# Features

The product catalogue. Each row links to a detailed doc; every feature lands on
all six surfaces unless the doc records the exception. Back to the root map:
[../BRAIN.md](../BRAIN.md).

| Feature | What it is | Key files |
|---|---|---|
| [Languages](i18n.md) | English and French: one table in core, `[panel] language`, what stays English |
| [Providers](providers.md) | The vendors read, their credentials, windows, readers and store support | `providers.rs`, `{claude,codex,kimi,grok,glm,openrouter,opencode,cursor}.rs` |
| [Several plans](several-plans.md) | Every stored credential's limits, ALL PLANS header weighted by plan multiplier, split bars, `[panel]` options | `credentials.rs`, `rows.rs`, `panel.rs`, `plan_weight` |
| [Cost tracking](cost-tracking.md) | COST and TOKENS sections from local transcripts: today, month, 7-day, session, burn rate | `cost/`, `panel.rs` |
| [Spend history](spend-history.md) | 30 d / 90 d / 12 mo chart on a second screen, past months at past prices, backfill, export | `history.rs`, `sync/fleet.rs`, `price-archive.json` |
| [Fleet sync](fleet-sync.md) | Fleet-wide cost and tokens, TOKENS BY DEVICE, setup screen and commands | `sync/`, `sync_cli.rs`, `sync_view.rs` |
| [Banked resets](banked-resets.md) | Claude limit resets granted to spend later, in the note under the credential's limits title | `claude.rs`, `panel.rs` |
| [Pace](pace.md) | Projected usage at reset per window (`ends ~16%`, `empty in 2h`), at the window's recent burn | `pace.rs` |
| [Window history](window-history.md) | Each window's trail since it opened, and the burn read off it | `window_history.rs` |
| [Notifications](notifications.md) | Threshold alerts per window, active plan only | `waybar/src/snapshot.rs`, `launch.rs` |
| [remuda's page](remuda-page.md) | Header button that opens `remuda serve`'s page via `remuda open` | `remuda.rs`, `launch.rs` |
| [`--doctor`](doctor.md) | Diagnostic checklist across credentials, cost, sync, wiring, updates | `doctor.rs` |
| [Updates](updates.md) | Self-update, daemon update check, Update button | `project.rs`, selvedge |

## Cross-cutting architecture

Every feature sits on the [panel spec](../architecture/panel-spec.md) and the
[snapshot](../architecture/snapshot.md); see
[../architecture/](../architecture/index.md) for credentials, cost readers, sync,
frontends and distribution.
