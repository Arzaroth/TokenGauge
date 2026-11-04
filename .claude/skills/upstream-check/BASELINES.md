# Upstream baselines

Rewritten at the end of every `upstream-check` run.

## Last checked: 2026-11-04

| Upstream | Baseline | Notes |
| --- | --- | --- |
| steipete/CodexBar | `v0.83.0` (2026-10-31) | v0.79.0-v0.83.0 triaged, 5 releases. `UsagePace.swift` moved (#4692) |
| basecamp/omarchy | `9b525a93` on `quattro` (2026-11-04) | Previous `0a7beb7d`; 394 commits, compare capped at 300 files, per-path queries authoritative. `v4.5.0` tagged 2026-11-04 (`76e11b4f`), not yet a published release; this machine runs v4.0.6 |
| akitaonrails/ai-usagebar | `v1.48.1` (2026-11-03) | v1.42.0-v1.48.1 triaged, 8 releases |

## Where we forked from

- CodexBar: the native fetchers landed 2026-07-16..18 (`af11ce4`, `4c1c098`,
  `274a2dd`), against CodexBar v0.44.0 / v0.45.0. `pace.rs` (`b484b1e`) is a
  port of `UsagePace.swift`. Upstream changed it on 2026-10 (#4692, a daily
  target for monthly allowances), so the port is now one feature behind.
- Omarchy: the widget landed 2026-08-21 (`efc16ae`), against omarchy
  `4.0.0.alpha`.

## Method notes

- The per-path `commits?path=` query **misses merge commits** (omarchy's
  `e78d89ee` surfaced only under `shell/Ui`). Cross-check with
  `compare/<base>...<head>` when its `files` count is under 300, otherwise
  scan the subjects of `commits?sha=quattro&since=`.
- Check the **installed** shell version (`/usr/share/omarchy/shell`) and the
  installed Qt, not only upstream HEAD: the Qt 6.12 break arrived through a
  distro Qt bump, not an omarchy one.
- One agent per upstream, each told to check our code before calling a gap,
  worked again. Omarchy's own commits are worth reading for **core** gaps too:
  the Codex `.jsonl.zst` change surfaced there (`ada0b511`) as well as in
  CodexBar.

## Backports 2026-11-04 (branch `fix/upstream-backports-2026-11`)

Every candidate found this run landed, one layer each:

- **opencode 2 login in `opencode.db`** (CodexBar #4771/#4815, ai-usagebar
  `cd063fce9`/#492). Read-only `rusqlite` (bundled): the `credential` row
  opencode selects (active, then newest), `opencode-go` then opencode 1's
  `opencode`, `key` or unexpired `oauth`; `auth.json` under either id only
  before opencode's `*_import_legacy_credentials` migration. Read out of the
  opencode 2.2.4 binary. remuda reads and switches the same row on its
  `fix/opencode-2-credentials` branch.
- **Codex credits are not dollars** (ai-usagebar 1.45.0): `Credits.unit`,
  drawn as a grouped count.
- **Codex `.jsonl.zst` and `archived_sessions`** (CodexBar #4534/#4818,
  omarchy `ada0b511`). ccusage's Rust Codex adapter already reads both, so this
  restores parity rather than breaking it: plain over compressed, `sessions`
  over `archived_sessions`. Decoded with `ruzstd`. Not seen on this machine yet.
- **GLM error echoing the key** (ai-usagebar `73039d210`): replaced by `[key]`.
- **Codex Fast mode** (CodexBar #4814): `thread_settings_applied` stamps the
  tier, LiteLLM's `_priority` / `_flex` / `_ultrafast` prices rate it, as
  ccusage does. ccusage also falls back to `config.toml`'s `service_tier` for
  calls the transcript does not classify; not taken.
- **Cursor X Subscription weekly pool** (CodexBar #4695): `isUnified` from
  `get-sand-usage-status`. No X Subscription account here, tests only.
- **Kimi `kimi-k2-thinking` dropped by LiteLLM** (open since 2026-10-20):
  decided as "keep vendored entries LiteLLM dropped", at runtime too
  (`PriceTable::or_vendored`). Vendored tables regenerated, now with
  `claude-mythos-5-1` and the tier prices.
- **Omarchy reduced motion** (`6ddadd45`): the two 160ms animations go through
  `Style.duration` where the shell has it.

### Already covered (checked this run)

Claude Ultra with `five_hour: null` and the Max/Ultra renames
(`claude.rs:557-600`, `839-842`), Claude Free both windows null, Grok
zero-usage week (`grok.rs:277-284`), Codex 30-day Free window pace
(`codex.rs:633-642`), Kimi login path, Kimi lapsed plan / gift balances (web
API we do not call), Cursor opaque `curs…` / encrypted 3.27 token (we never
read `state.vscdb`), Codex "Not reported" window (dropped by design).

### Omarchy surfaces (2026-11-04)

No watched change breaks the widget. All additive: `aaccd247` panel grows out
of the bar (we already set `fittedContentWidth/Height`), `59d6f7f1` late
content resizes, `13daf594` / `a08ff344` / `33d6472a` WidgetButton press
feedback, `84d4435f` tooltip wrap, `a0010440` / `c726e6a7` registry fixes
with unchanged manifest keys. `6ddadd45` makes upstream tests require
`Style.duration`; the widget now asks it where the shell has it.

Adoption pass once 4.5.0 is a published release and this machine runs it:
`ShellIpc` (their agents widget uses it; our plain `IpcHandler` still works
via `qs ipc`), `BarWidget.fixedEdge`,
`Commons.Translations`, `KeyboardPanel.content`. Keep `Commons.Color`.

Their agents manifest now lists 23 providers; schema otherwise unchanged
(`refreshIntervalSec` 900, four `sync*` keys).

### Worth considering (features, not defects)

- Codex banked resets (`available_count`, ai-usagebar 1.42.0): read-only.
- Claude cloud session credits (`iguana_necktie`, ai-usagebar `cf3175a63`)
  and usage-credit `spend.balance` / `auto_reload`.
- SuperGrok on-demand spend past the included pool (ai-usagebar #488).
- `pace.rs` daily target for monthly allowances (CodexBar #4692).
- Kimi monthly Total split by feature (CodexBar #4759).
- opencode Go local cost history (`opencode.db` `session_message`, CodexBar
  #4587/#4606), needs SQLite and a `natively_read` flip; pairs with item 1.
- Claude Desktop token (safeStorage); highest-percent `auto` bar window;
  `schema_version` on `--json`.
- New providers upstream: Vercel AI Gateway, Ollama Cloud, Synthetic, Cline,
  Devin (multi-account), Model Studio, DeepInfra, Nous, Copilot, Fireworks,
  DeepRelay, Factory Droid, MiniMax, DeepSeek, Qwen. ai-usagebar is the
  reference to read first for any of them.

## Deliberately not taken

- The rest of CodexBar `#2900` (ChatGPT dashboard spend controls, needs a
  browser cookie).
- Every macOS-only source: browser-cookie imports, claude-swap, iCloud sync,
  Claude Desktop Cowork transcripts (#3209, #4712, would also break ccusage
  parity).
- Omarchy's `agents` manifest sync settings: our fleet sync is in
  `config.toml`.
- In-panel account sign-in, autoswitch and device-code login: remuda owns
  credentials, TokenGauge only reads them (decision 0003).
- z.ai BigModel CN prepaid CNY balance (CodexBar #4796), ai-usagebar's MCP
  server and `--priorities` ranking.

## Earlier runs

2026-10-20 (branch `fix/upstream-backports-2026-10`): Omarchy `Commons.Color`
under Qt 6.12, Kimi ratio pools and older CLI home, `provider::lenient_list`,
Cursor macOS auth / WAF 403 / Enterprise team spend, Claude extra usage, Codex
spend cap, elapsed windows, Grok monthly period, early-window pace, unreadable
transcript root, long-context tiers, rustls 0.23.46. 2026-09-14: z.ai
implausible reset, `format_tokens` 1000.0K, 403 vs 401, Codex subagent history
boundary, the `null`-for-a-list serde trap. 0.30.x (2026-09-02): QML
`PlainText`, Codex #3120, Codex JWT expiry. 0.29.x: z.ai `CREDIT_LIMIT`, Codex
PATs, `spend_control`, Codex monthly window, Kimi lane names. See git history
of this file.
