# Architecture

The technical layers and cross-cutting subsystems. Start with
[overview.md](overview.md) for the big picture, then jump to the layer you need.
Back to the root map: [../BRAIN.md](../BRAIN.md).

| File | What's here | Grep keywords |
|---|---|---|
| [overview.md](overview.md) | Workspace, data flow from provider to six surfaces, the rules the layout depends on | crates, lib.rs, data flow, --json funnel |
| [panel-spec.md](panel-spec.md) | `panel_spec`, section ids and kinds, tones, refresh hint, bar tooltip, `[panel]` options, the source-grepping guards | SECTION_IDS, SectionKind, Meters, Bars, Rows, Tone, refresh_hint, bar_tooltip, segments |
| [frontends.md](frontends.md) | Waybar, Plasma, GNOME, Quickshell, tray, TUI: toolkit, data path, per-surface quirks | waybar, plasmoid, Service.qml, gnome-shell, panel.ts, omarchy, egui, ratatui, build.sh |
| [snapshot.md](snapshot.md) | The snapshot and its sibling state files, `cache_is_stale`, revision watching, the daemon and setters | cache_file, statefiles, revision, wait-change, SIGHUP, daemon, socket |
| [credentials.md](credentials.md) | Live credential sources, the remuda store contract, live-login matching, cadence, the snapshot contract | keychain, CLAUDE_CONFIG_DIR, sidecar, credsDigest, StoreReader, last-switch, planWeight |
| [cost.md](cost.md) | Transcript readers, the five traps, LiteLLM pricing, price archive, ccusage cross-check | UsageEvent, Roots, natively_read, price_candidates, vendor_prefixes, unpriced, ccusage |
| [sync.md](sync.md) | Fleet store, contribution document, envelope and key, dir/s3 transports, the cycle, hazards | fleet store, bucket, tgsync, XChaCha20, HMAC, SigV4, retention |
| [testing.md](testing.md) | e2e, QML, GNOME and GNOME Shell harnesses, the panel fixture, cost/sync tests, coverage | e2e.rs, run.sh, panel.json, make-panel-fixture, llvm-cov |
| [distribution.md](distribution.md) | Binary name and its legacy alias, selvedge updater, Linux/macOS/Windows install and packaging | tokengauge-waybar, selvedge, self-update, LaunchAgent, DMG, MSI, win64 |

## How to use this folder

- Each file stands alone but cites real source paths so you can jump to code.
- The regression rules, each with the incident behind it, stay in the repo's
  `CLAUDE.md`; these docs describe the mechanism and link there.
- Product features (providers, several plans, history, sync, pace...) live in
  [../features/](../features/index.md); this folder is the plumbing under them.
