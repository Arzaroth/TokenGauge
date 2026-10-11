# Architecture overview

Big picture: what TokenGauge is, how data flows from a provider to six surfaces,
and the rules the layout depends on. Read this first, then jump to the layer.

## What it is

One gauge for how much a developer has spent against their coding-CLI plans:
rate-limit windows fetched natively from each provider's API, costs read from the
transcripts the CLIs already write, rendered identically on a waybar module, four
desktop panels (KDE Plasma, GNOME, Quickshell/Omarchy, the Windows and macOS
tray) and a TUI. Eight providers: Claude, Codex, Kimi, Grok, GLM (z.ai),
OpenRouter, opencode Go, Cursor. Read-only everywhere: it never refreshes or
writes a credential it does not own.

## The workspace

| Crate / dir | What |
| --- | --- |
| `crates/tokengauge-core` | Everything TokenGauge knows: providers, credentials, fetch, cost, sync, history, panel, snapshot, doctor. No frontend logic beyond the spec. |
| `crates/tokengauge-waybar` | The `tokengauge` binary: waybar output, `--json`, the daemon, CLI flags, sync and export commands. |
| `crates/tokengauge-tui` | `tokengauge-tui` (ratatui). |
| `crates/tokengauge-tray` | `tokengauge-tray` (egui + tray-icon), Windows and macOS only. |
| `plasma/`, `gnome/`, `omarchy/` | The three desktop payloads (QML, TypeScript, QML). |
| `scripts/`, `packaging/` | Installers, fixture and price generators, packaging. |
| `tests/` | The QML and GNOME frontend harnesses. |

## Data flow

```
provider APIs ──fetch.rs──► payloads ─┐
transcripts ──cost/ readers──► UsageEvents ─► fleet store (sync/) ─► CostInfo ─┤
                                                                               ▼
                         snapshot (tokengauge-usage.json) + tokengauge-revision
                                                │
                     rows.rs ─► ProviderRow ─► panel.rs ─► Vec<Section>
                                                │
          waybar / TUI / tray (in-process)      └── tokengauge --json ──► Plasma / GNOME / Quickshell
```

`lib.rs` documents the module order: `providers` (one table of what a provider
is; adding one is a row) -> `credentials` -> `fetch` -> `payload` -> `rows` (past
here nothing knows which provider it is looking at) -> `cost` -> `sync` ->
`panel` -> `snapshot`.

## Rules the layout depends on

- **Data belongs in core; frontends draw.** A frontend never reads a credential,
  a cache file or a provider endpoint. See [panel-spec.md](panel-spec.md) and
  [frontends.md](frontends.md).
- **Every state file derives from the snapshot's parent**, so moving `cache_file`
  moves the set. See [snapshot.md](snapshot.md).
- **Tone is decided in core, colour per frontend.**
- **`cache_is_stale()` is the single fetch decision; rendering is not fetching.**
- **One unit per pipeline**: a `UsageEvent` for cost, a token bucket for sync.
  Tokens cross machines, never dollars.

## Cross-cutting subsystems

| Subsystem | File |
| --- | --- |
| Panel spec, sections, tones, guards | [panel-spec.md](panel-spec.md) |
| The six surfaces | [frontends.md](frontends.md) |
| Snapshot, staleness, daemon | [snapshot.md](snapshot.md) |
| Credential sources, store and snapshot contracts | [credentials.md](credentials.md) |
| Transcript readers, pricing, ccusage | [cost.md](cost.md) |
| Fleet store, contributions, envelope, transports | [sync.md](sync.md) |
| Harnesses and guards | [testing.md](testing.md) |
| Binary name, selvedge, macOS, Windows | [distribution.md](distribution.md) |

## Sources

- `crates/tokengauge-core/src/lib.rs` (module map), `Cargo.toml` (workspace)
- `crates/tokengauge-core/src/{providers,fetch,rows,panel,snapshot}.rs`
- `CLAUDE.md` (the hard rules)
