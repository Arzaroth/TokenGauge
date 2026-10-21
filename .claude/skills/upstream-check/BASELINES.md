# Upstream baselines

Rewritten at the end of every `upstream-check` run.

## Last checked: 2026-10-20

| Upstream | Baseline | Notes |
| --- | --- | --- |
| steipete/CodexBar | `v0.78.0` (2026-10-19) | v0.60.2-v0.78.0 triaged, 25 releases |
| basecamp/omarchy | `0a7beb7d` on `quattro` (2026-10-20) | Previous `b679363b`; 735 commits, compare capped at 300 files, so per-path queries were authoritative. HEAD is v4.5.0-rc1 + 3; this machine runs stable v4.0.6 |
| akitaonrails/ai-usagebar | `v1.41.0` (2026-10-18) | v1.17.1-v1.41.0 triaged, 26 releases |

## Where we forked from

- CodexBar: the native fetchers landed 2026-07-16..18 (`af11ce4`, `4c1c098`,
  `274a2dd`), against CodexBar v0.44.0 / v0.45.0. `pace.rs` (`b484b1e`) is a
  port of `UsagePace.swift`, whose last upstream change is still 2026-07-03
  (#1789), so the port is current.
- Omarchy: the widget landed 2026-08-21 (`efc16ae`), against omarchy
  `4.0.0.alpha`.

## Method notes

- The per-path `commits?path=` query **misses merge commits** (omarchy's
  `e78d89ee` surfaced only under `shell/Ui`). Cross-check with
  `compare/<base>...<head>` when its `files` count is under 300. This run it
  was capped (735 commits), so the per-path list plus reading each suspicious
  diff was the method.
- Check the **installed** shell version (`/usr/share/omarchy/shell`) and the
  installed Qt, not only upstream HEAD: the Qt 6.12 break below arrived through
  a distro Qt bump, not an omarchy one.
- A month of all three upstreams is too much to read inline; one agent per
  upstream, each told to check our code before calling a gap, worked.

## Backports 2026-10-20 (branch `fix/upstream-backports-2026-10`)

Every candidate found this run landed, one layer each:

- **Omarchy widget colours under Qt 6.12** (omarchy `b83d3df0` / #14553).
  Qt 6.12 adds a QtQuick `Color` type that shadows `qs.Commons`' `Color`;
  `Panel.qml` now writes `Commons.Color.*`. Arrived through a distro Qt bump
  (`qt6-declarative 6.12.0`, 2026-10-19), not an omarchy one.
- **Kimi ratio pools** (CodexBar #3697/#4306/#3755, ai-usagebar 1.19.0).
  `usages.{limit_5h,limit_7d,limit_month_total}` stand in for a missing
  `usage`; a counter for the same window on the same clock (2s) wins when it
  reports more; the month total is a `kimi-monthly` row; `limit_month_code`
  is never added.
- **Kimi older CLI home** (omarchy `c88e7dd5`): `~/.kimi` when `~/.kimi-code`
  holds no credential.
- **One bad limit entry fails the fetch** (CodexBar #4614):
  `provider::lenient_list` on Claude's `limits[]` and z.ai's `limits`.
- **Cursor on macOS** (ai-usagebar 1.29.0): `~/.cursor/auth.json`, then the
  Keychain. Compiled only by the macOS CI job; nothing here built it.
- **Cursor WAF 403** (ai-usagebar #410): full client-hint header set, and a
  403 no longer reads "check your plan".
- **Cursor Enterprise member budget** (CodexBar #3646): `get-team-spend`,
  sole team only, every page must agree.
- **Claude extra usage / spend-only seats** (CodexBar #4480, ai-usagebar
  #456): `extra_usage` is an on-demand cap; USD only; kept when it ran out.
- **Codex spend cap beside rate windows** (omarchy `f41da937`): a
  `codex-spend-limit` row.
- **Elapsed windows** (omarchy `d21e5810`): `rows::expire_elapsed`, same
  predicate as `snapshot::rolled_over`.
- **Grok monthly period** (ai-usagebar #418): the gRPC-web response has no
  schema to find it in, so it comes from the CLI's own JSON billing,
  `cli-chat-proxy.grok.com/v1/billing?format=credits`. Best effort.
- **Early-window pace** (ai-usagebar #390): ahead tops out at warn in the
  first 15% with half left.
- **Unreadable transcript root** (CodexBar #4617): the fleet store keeps the
  device's record; backfill stays unmarked.
- **Long-context price tiers** (CodexBar #4302): `ModelPrice.long_context`,
  applied only to `one_request` events (Claude messages, Codex responses).
- **rustls 0.23.46** (RUSTSEC-2026-0285, -0346..-0349).

None of Kimi, Cursor or Grok has a login on the machine that did this, so
those fetchers are verified by tests only.

### Open: LiteLLM dropped `moonshot/kimi-k2-thinking`

Found while regenerating the vendored tables for the tiers: LiteLLM's `main`
no longer carries `moonshot/kimi-k2-thinking` (nor `-turbo`, `kimi-latest*`,
`kimi-k2-0905-preview`). Only `kimi-k2-thinking-251104` and Bedrock copies
remain. An online machine, which rates against the live table, already prices
current-month `kimi-k2-thinking` at $0; a regenerated `prices.json` would do
the same offline and fails
`a_model_sold_under_a_vendor_path_is_priced_by_its_bare_name`. The regen was
therefore not committed, so the vendored table carries no tiers yet. Needs a
decision: a dated-id walk in `price_candidates` (`-251104`), or keeping
vendored entries LiteLLM dropped.

### Already covered (checked this run)

Claude `five_hour: null` (#4107), Linux `cursor-agent` path (#4397), Grok ACP
billing (#4595), OpenRouter `rate_limit` (#3720), Codex `plan_type` after
upgrade (#3389), Codex auto-review $0 (unpriced already), z.ai HTTP-200
auth-failure envelope (`glm.rs:244`), Codex numeric-string credits
(`json_num`), blank access tokens, macOS stub file shadowing the keychain,
notification reset drift (`ROLLOVER_TOLERANCE`), opencode Go `auth.json`,
omarchy's streamed-Claude dedup (equivalent to ours).

### Omarchy surfaces (2026-10-20)

Apart from the Qt 6.12 colour break, no watched change breaks the widget, at
HEAD or at v4.0.6:

- `c231097d` shell IPC socket: plain `IpcHandler` still answers via the `qs
  ipc` fallback. `ShellIpc` is HEAD-only.
- `b7e900ce` sections: `defaultSection: "right"` still valid; do not use
  `tray` (v4.0.6 rejects it). `fixedEdge` optional.
- `201de1c0` icon slot: vertical glyph lands in the normalised slot; fine.
- `86234a91` / `47e86a27` registry internals: manifest keys unchanged.
- `4c1b4cb3` lazy panel content: opt-in, our eager panel still works.
- `b18ab495` no-animations: our 160ms animations ignore `Style.duration`
  (HEAD-only); guard with `typeof` if wanted.
- `84cfb939` i18n: `KeyboardPanel` mirrors for RTL, our Canvas chart does not.

HEAD-only APIs to adopt once 4.5 is stable: `ShellIpc`, `Commons.Translations`,
`KeyboardPanel.content`, `Style.duration`, `BarWidget.fixedEdge`.

The agents widget grew account switching (remuda's job here), Grok and Kimi
collectors, prompt/session counts, starter prompts, and opencode v2 / pi
transcripts. Their manifest added per-provider `enabled`, sync keys and a 900s
refresh; no contract change for ours.

### Worth considering (features, not defects)

- Claude banked limit resets (`cedar_ember`, ai-usagebar v1.24.0) and Codex
  banked reset credits: read-only, never redeem.
- Claude Desktop token (safeStorage), now with account switching upstream.
- Highest-percent `auto` bar window; `schema_version` on `--json` (no move).
- opencode Go local cost history from `opencode.db` (CodexBar #4606), needs
  SQLite and a `natively_read` flip.
- Codex Priority/Fast pricing from `service_tier`; Codex rollouts whose
  `model_provider` is not OpenAI land under Codex at $0 here.
- New providers upstream: Vercel AI Gateway, Ollama Cloud `/api/balance`,
  Synthetic, Cline, Devin CLI, Model Studio, DeepInfra, Nous, Copilot
  multi-account, Fireworks (omarchy). ai-usagebar is the reference to read
  first for any of them.

## Deliberately not taken

- The rest of CodexBar `#2900` (ChatGPT dashboard spend controls, needs a
  browser cookie).
- Every macOS-only source: browser-cookie imports, claude-swap, iCloud sync,
  Claude Desktop Cowork transcripts (#3209, would also break ccusage parity).
- Omarchy's `agents` manifest sync settings: our fleet sync is in
  `config.toml`.
- In-panel account sign-in and autoswitch: remuda owns credentials, TokenGauge
  only reads them (decision 0003).

## Earlier runs

2026-09-14: z.ai implausible reset, `format_tokens` 1000.0K, 403 vs 401, Codex
subagent history boundary, the `null`-for-a-list serde trap (from
ai-usagebar). 0.30.x (2026-09-02): QML `PlainText`, Codex #3120, Codex JWT
expiry. 0.29.x: z.ai `CREDIT_LIMIT`, Codex PATs, `spend_control`, Codex monthly
window, Kimi lane names. See git history of this file.

Icons match upstream modulo the `currentColor` recolour, all eight re-diffed
this run. Upstream's `-glm` is `-zai`.
