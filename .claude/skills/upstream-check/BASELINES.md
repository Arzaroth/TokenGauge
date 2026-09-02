# Upstream baselines

Rewritten at the end of every `upstream-check` run.

## Last checked: 2026-09-02

| Upstream | Baseline | Notes |
| --- | --- | --- |
| steipete/CodexBar | `v0.56.3` (2026-09-01) | Release notes through v0.56.3 triaged (v0.55.1, v0.56.0-v0.56.3 new this run) |
| basecamp/omarchy | `7eca64e2` on `quattro` (2026-09-02) | Previous baseline `43bfe9b9` (2026-08-23) |

## Where we forked from

- CodexBar: the native fetchers landed 2026-07-16..18 (`af11ce4`, `4c1c098`,
  `274a2dd`), against CodexBar v0.44.0 / v0.45.0. `pace.rs` (`b484b1e`) is a
  port of `UsagePace.swift`, whose last upstream change was 2026-07-03, so the
  port is current.
- Omarchy: the widget landed 2026-08-21 (`efc16ae`), against omarchy
  `4.0.0.alpha`.

## Backport status (2026-09-02)

Done this run, in `[Unreleased]`:

- **`textFormat: Text.PlainText` on every QML label** (omarchy `3af7675a`). A
  `Text` left on `Text.AutoText` promotes anything that looks like markup to
  rich text, and rich text fetches `<img src="http://...">` through
  `QQuickPixmap` - an unauthenticated GET with nothing clicked. Declared on all
  19 `Text` elements in `omarchy/arzaroth.tokengauge/Panel.qml` and all 25
  `PlasmaComponents.Label`s in the Plasma applet. Waybar (`pango_escape`) and
  GNOME (no markup) were already clean. Upstream's guard scans their own
  `shell/` tree only, so nothing on our side will catch a regression here: a new
  label needs the declaration written with it.
- **Codex `#3120`, all three items** in `cost/codex_cli.rs`, each with a test
  named after the trap it covers:
  - `cached()` takes the larger of `cached_input_tokens` and
    `cache_read_input_tokens`.
  - `regressed_from()` drops a reading that moved backwards *before* it can
    become the baseline. Proven by disabling the guard and watching
    `a_stale_reading_does_not_lower_the_baseline` bill 2000 against 1500.
  - headless `codex exec` rows (no rollout envelope, per-call rather than
    cumulative, `prompt_tokens`/`input`/`input_tokens` aliases, cached
    subtraction, timestamp inherited from the last row that carried one) are
    parsed in the branch that used to drop a payload-less line, so no line the
    rollout reader handles can be touched by it.
- **Codex access-token JWT expiry** (`#3221` / `#3222`). `needs_refresh` reads
  the `exp` claim first and keeps the 8-day `last_refresh` rule as the fallback
  for a token that carries no claim. Hand-rolled base64url decode rather than a
  new dependency.

Verified after: `cargo test --workspace` green, `tests/cost_fixture.rs` green,
and `agrees_with_ccusage_on_real_transcripts` still exact on this machine
(claude 17,571,545,295 and codex 146,483,834, 0.000% both) - the new reader
paths do not fire on the Codex version here, which is what the report said.

Checked this run, already covered on our side:

- Grok `#3261` / `#3325` / `#3357` (0% for a validated active period with an
  omitted usage scalar, unknown for malformed) and `#3181` (period-only bearer
  billing). `grok.rs` already reads `None if resets_at.is_some() => 0.0`, else
  error, with tests for the zero, truncated and `grpc-status: 16` cases.
- Claude `#2374`, the Kimi `k3[1m]` context alias. `pricing::base_model_name`
  strips a bracketed suffix generically and `moonshot/kimi-k3` is in the table.
- Claude `#3317`, duplicated "Resets Reset". No source populates
  `reset_description`; every native fetcher sets it to `None`.

Done in earlier runs (0.29.x): z.ai `CREDIT_LIMIT`, Codex personal access
tokens, `spend_control.individual_limit`, the Codex monthly window, Kimi lane
names. See git history of this file for the detail.

Deliberately not taken:

- The rest of CodexBar `#2900`: the ChatGPT dashboard spend-controls fallback
  needs a browser session cookie, which is the WebView path we do not have.
- Every macOS-only source: browser-cookie imports, claude-swap, iCloud sync.
- Omarchy's `agents` manifest sync settings (`syncMode`, `syncDir`,
  `syncFileName`, `syncDeviceId`). Our fleet sync is configured in
  `config.toml` and set up from the panel's `y` key, not per-widget.

Icons match upstream modulo the `currentColor` recolour; upstream's `-glm` is
now `-zai`.
