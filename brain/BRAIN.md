# BRAIN - TokenGauge knowledge base

The map of the codebase, written so a developer or AI can understand TokenGauge
**without reading the source**. Start here, follow the links, stop when you have
the answer. Every leaf doc cites the real source files if you need to go deeper.

> Product: **TokenGauge** - one gauge for how much a developer has spent against
> their coding-CLI plans (Claude, Codex, Kimi, Grok, GLM, OpenRouter, opencode Go,
> Cursor), rendered identically on a waybar module, KDE Plasma, GNOME,
> Quickshell, a Windows / macOS tray and a TUI. Stack snapshot in
> [stack.md](stack.md); the current version is `version` in
> `crates/tokengauge-core/Cargo.toml`.

## How to use this

1. Know the topic? Jump straight to its file via the indexes below.
2. Have a question, not a topic? Scan **Find by question** first.
3. Need a term defined? See [glossary.md](glossary.md).
4. Want the "why" behind a design? See [decisions.md](decisions.md).
5. About to change something? The rules a change can break, each with the
   incident behind it, are in the repo's `CLAUDE.md`.

The brain is a tree: this file -> two folder indexes
([architecture/index.md](architecture/index.md),
[features/index.md](features/index.md)) -> leaf docs. Don't grep the whole tree;
use the indexes.

## Top-level docs

| Doc | What's here |
|---|---|
| [stack.md](stack.md) | Rust edition and MSRV, crates and key dependencies, TypeScript/QML, pnpm. |
| [operations.md](operations.md) | The gate, generators, install and daemon, releases, upstream watching. |
| [glossary.md](glossary.md) | Domain and technical terms, with the synonyms to avoid. |
| [decisions.md](decisions.md) | The non-obvious decisions and their rationale, incl. the former ADRs 0001-0004. |

## Architecture (the how) - [architecture/index.md](architecture/index.md)

| Doc | What's here |
|---|---|
| [overview.md](architecture/overview.md) | Workspace, data flow from provider to six surfaces, layout rules. Read first. |
| [panel-spec.md](architecture/panel-spec.md) | `panel_spec`, sections and kinds, tones, content-in-chrome, the source-grepping guards. |
| [frontends.md](architecture/frontends.md) | The six surfaces: toolkit, data path, per-surface quirks. |
| [snapshot.md](architecture/snapshot.md) | The snapshot and its state files, `cache_is_stale`, revision watching, the daemon. |
| [credentials.md](architecture/credentials.md) | Credential sources, the remuda store and snapshot contracts, matching, cadence. |
| [cost.md](architecture/cost.md) | Transcript readers, the five traps, LiteLLM pricing, ccusage cross-check. |
| [sync.md](architecture/sync.md) | Fleet store, contribution, envelope and key, transports, cycle, hazards. |
| [testing.md](architecture/testing.md) | e2e, QML, GNOME and GNOME Shell harnesses, fixtures, coverage. |
| [distribution.md](architecture/distribution.md) | Binary name, selvedge updater, Linux / macOS / Windows packaging. |

## Features (the what) - [features/index.md](features/index.md)

| Doc | What's here |
|---|---|
| [providers.md](features/providers.md) | The eight providers: credentials, windows, readers, store support. |
| [several-plans.md](features/several-plans.md) | Every stored plan's limits, ALL PLANS by multiplier, split bars, `[panel]` options. |
| [cost-tracking.md](features/cost-tracking.md) | COST and TOKENS sections from local transcripts. |
| [spend-history.md](features/spend-history.md) | 30 d / 90 d / 12 mo second screen, price archive, backfill, export. |
| [fleet-sync.md](features/fleet-sync.md) | Fleet-wide figures, TOKENS BY DEVICE, setup, status. |
| [banked-resets.md](features/banked-resets.md) | Claude's banked limit resets on the limits title. |
| [pace.md](features/pace.md) | Projected usage at reset per window. |
| [notifications.md](features/notifications.md) | Threshold alerts, active plan only. |
| [remuda-page.md](features/remuda-page.md) | The button that opens `remuda serve`'s page. |
| [doctor.md](features/doctor.md) | The `--doctor` checklist. |
| [updates.md](features/updates.md) | Self-update, update check, Update button. |

## Find by question

| If you're asking... | Go to |
|---|---|
| Where is the panel built? Why can't a frontend format its own numbers? | [architecture/panel-spec.md](architecture/panel-spec.md) |
| How do I add a section, or a new `SectionKind`? | [architecture/panel-spec.md](architecture/panel-spec.md) |
| Which frontends must a feature land on, and what is exempt? | [architecture/frontends.md](architecture/frontends.md) |
| When does TokenGauge fetch, and when does it serve the snapshot? | [architecture/snapshot.md](architecture/snapshot.md) |
| Why does my freshly built binary show old output? | [architecture/snapshot.md](architecture/snapshot.md) |
| Where does the Claude token come from? Why is a present file not enough? | [architecture/credentials.md](architecture/credentials.md) |
| What does remuda write, and what does it read back from the snapshot? | [architecture/credentials.md](architecture/credentials.md) |
| How are several plans added up? What is a plan weight? | [features/several-plans.md](features/several-plans.md) |
| How do I add a provider? | [features/providers.md](features/providers.md), [architecture/cost.md](architecture/cost.md) |
| Why is a model rated at $0? Why do token counts disagree with ccusage? | [architecture/cost.md](architecture/cost.md) |
| How does fleet sync work, and what crosses the wire? | [architecture/sync.md](architecture/sync.md), [features/fleet-sync.md](features/fleet-sync.md) |
| Why no key rotation or revocation? | [decisions.md](decisions.md#0002-symmetric-fleet-key) |
| Why is last March priced differently from today? | [features/spend-history.md](features/spend-history.md) |
| Why has waybar no history screen? | [features/spend-history.md](features/spend-history.md) |
| How is remuda's page opened without its token? | [features/remuda-page.md](features/remuda-page.md) |
| Why is the binary `tokengauge` but the crate `tokengauge-waybar`? | [architecture/distribution.md](architecture/distribution.md) |
| How does the updater work on Windows (MSI) and macOS (DMG)? | [architecture/distribution.md](architecture/distribution.md) |
| What does CI run? How do I cut a release? | [operations.md](operations.md) |
| How are the QML and GNOME frontends tested? | [architecture/testing.md](architecture/testing.md) |

## Maintaining the brain

This brain is **load-bearing documentation**: it must stay true to the code.
When a change makes a brain doc wrong, fix the doc in the same change. When you
discover the brain disagrees with reality, the code wins: correct the brain and
note it. The enforceable version of this rule lives in the repo's `CLAUDE.md`
("The brain (knowledge base)"). New feature -> new `features/<name>.md` + a row
in [features/index.md](features/index.md) + a row here. New subsystem -> an
`architecture/*.md` + rows in its index and here. New term ->
[glossary.md](glossary.md); new decision -> [decisions.md](decisions.md).
