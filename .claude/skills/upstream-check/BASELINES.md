# Upstream baselines

Rewritten at the end of every `upstream-check` run.

## Last checked: 2026-09-14

| Upstream | Baseline | Notes |
| --- | --- | --- |
| steipete/CodexBar | `v0.60.1` (2026-09-13) | Release notes through v0.60.1 triaged (v0.56.4-v0.56.8, v0.57.0-v0.60.1 new this run) |
| basecamp/omarchy | `b679363b` on `quattro` (2026-09-14) | Previous baseline `7eca64e2` (2026-09-02); 104 commits, 222 files in the span |
| akitaonrails/ai-usagebar | `v1.17.0` (2026-09-12) | **New this run.** Nothing taken from it in code; it is a parallel implementation, not a source we forked from |

## Where we forked from

- CodexBar: the native fetchers landed 2026-07-16..18 (`af11ce4`, `4c1c098`,
  `274a2dd`), against CodexBar v0.44.0 / v0.45.0. `pace.rs` (`b484b1e`) is a
  port of `UsagePace.swift`, whose last upstream change was 2026-07-03, so the
  port is current.
- Omarchy: the widget landed 2026-08-21 (`efc16ae`), against omarchy
  `4.0.0.alpha`.

## Method note learned this run

The per-path `commits?path=` query **misses merge commits**. Omarchy's plugin
auth boundary (`e78d89ee`) modifies `shell/services/PluginRegistry.qml` and did
not appear under that path's query; it surfaced only under `shell/Ui`, because
the merge added a new file there. Cross-check with the compare endpoint
(`compare/<base>...<head>`) and confirm its `files` count is under 300, or a
watched path changed by a merge reads as quiet.

## Backport status (2026-09-14)

Done this run, in `[Unreleased]`:

- **z.ai implausible five-hour reset** (CodexBar `#2871` mitigation, v0.56.5).
  `glm.rs::to_window` passed `nextResetTime` straight through. Upstream's
  `zai.js` drops a reset on a non-`TIME_LIMIT` 300-minute window landing more
  than 5h + 1min out, with the comment "a five-hour Coding Plan reset cannot be
  ten hours away; never guess a timezone correction". z.ai sends exactly that,
  recurringly. Any GLM Coding Plan user reads "Resets in 9h 40m" under a window
  labelled 5-hour. A generic guard (drop a `resets_at` further out than the
  window it belongs to) covers all three slots, and is what landed:
  `glm::plausible_reset`, with the far / near / unknown-length cases tested.
- **`format_tokens` prints `1000.0K` / `1000.0M`** (CodexBar `#3519`, v0.58.0).
  `fmt.rs:91` rounds before it picks the unit, so 999,950..999,999 reads as
  `1000.0K` instead of `1.0M`, and the M/B boundary the same way. Reaches
  `tokens_by_day` and `tokens_by_model` on all six frontends. Narrow band, one
  line to fix.
- **403 is not 401** (CodexBar `#3379`, v0.56.8). `provider::check_status`
  collapses both into "unauthorized - <re-login hint>" for every provider.
  Upstream made 403 terminal: it is a permission failure, and re-login does not
  fix it. We have no auto-recovery loop, so the cost here was a misleading hint
  only. 403 now reads `access denied - check your plan or account access`,
  which is the wording `kimi.rs` already had; its local branch is gone, since
  the ladder owns the state for every provider now.

- **Codex subagent history boundary** (CodexBar `#3527`, v0.58.0). Settled
  against the producer rather than guessed: `subagent_history_start_ordinal`
  lives in `SessionMeta` in `openai/codex`, and a child thread builds a fresh
  `TokenUsageInfo` from `TokenUsage::default()` rather than inheriting the
  parent's, so a copied prefix row carries the **parent's** cumulative. It is
  therefore dropped outright, not latched as a baseline: latching it would make
  every later child reading read as a regression and the session would bill
  nothing at all. That is also what upstream does (`previousTotals = nil` at
  the boundary). A row with no ordinal reads as inherited, mirroring upstream's
  `ordinal ?? Int.min`. No subagent rollout exists on this machine (every
  September rollout is `ordinal: 0`, `thread_source: "user"`), so the three
  tests are the only proof; `an_inherited_prefix_is_not_the_subagents_spend`
  and `a_prefix_only_subagent_rollout_bills_nothing` were both watched to fail
  with the guard disabled, and `a_session_without_a_boundary_bills_every_row`
  is the regression guard against zeroing ordinary sessions.

Checked this run, already covered on our side:

- z.ai `#3590` (a missing quota limit must not read as 100% remaining).
  `glm::used_percent` returns `None` with no basis and `panel.rs` drops a
  `used: None` window. The comment there already says "never a false 100%".
- Codex `#3589` (oversized numeric reset timestamps). `epoch_to_rfc3339`
  saturates the `f64 as i64` cast and `timestamp_opt` rejects out of range, so
  the window is dropped rather than crashing.
- Codex `#3504` (whitespace before `"type": "event_msg"` defeating a
  compact-text prefilter). `cost/codex_cli.rs` deserializes every line
  structurally and prefilters nothing. `claude_code.rs`, `grok_cli.rs` and
  `kimi_cli.rs` do prefilter, but on `"usage"` / `turn_completed` substrings
  that survive whitespace either side.
- Kimi `#3543` (Auto showing unused quota while the monthly pool is exhausted).
  We have no Auto: the bar window is the configured `Daily`/`Weekly` slot and
  the panel renders every window. Kimi's membership pool is our `primary`.
- Pace colours `#3429`. `Tone::for_pace` is already green when behind and
  warn/critical when ahead, unconditionally rather than as a setting.

Not a backport, worth knowing: CodexBar v0.60.0 shipped a **Linux Qt desktop
app and its own native Omarchy Quickshell bar widget** (`#3568`-`#3571`,
`#3573`-`#3577`). That is the first time upstream has landed on our platform
and in one of our frontend slots.

## Omarchy surfaces (2026-09-14)

One watched surface moved: **third-party plugin auth boundary** (`#9618`,
`e78d89ee`, 2026-09-07), adding `shell/Ui/PluginBarApi.qml`, touching
`shell/Ui/qmldir` and `shell/services/PluginRegistry.qml`. Third-party plugins
now receive capability-scoped facades where built-ins receive trusted host
objects.

`omarchy/arzaroth.tokengauge` is unaffected, verified against the installed
shell on this machine, which already carries the change:

- It declares `kinds: ["bar-widget"]` with no service entry point, so the
  service-less facade for widgets under a replacement bar costs it nothing.
- It never touches `shell`, `pluginRegistry` or `barWidgetRegistry`.
- It reads `bar.vertical` / `foreground` / `urgent` / `fontFamily`, which is
  the "detached scalar bar state" the boundary explicitly still permits, and
  each read already falls back when `bar` is null. `Bar.qml` still passes
  `bar: root` to widget entry points.
- `PluginRegistry.trustedCapabilities` grants `[]` to any third party that is
  not a stamped clone of a first-party plugin, so there is nothing for our
  manifest to declare.

Also in the span but not a surface we use: `shell/Ui/BackgroundMedia.qml` and
`BackgroundVideo.qml` from the native video wallpaper commit (`41b6cc69`).

`shell/plugins/agents` did not change in this span. Its manifest still carries
`refreshIntervalSec` plus the four sync keys, and `defaults.providers` for
claude / codex / fireworks. Nothing new to weigh.

## akitaonrails/ai-usagebar (added 2026-09-14)

A parallel implementation, not something we forked from: Rust, MIT, ~494 stars,
and the same six-surface shape we have (waybar, TUI, KDE plasmoid, Omarchy
Quattro plugin, Windows tray, macOS menu bar). It covers more providers than we
do (Antigravity, OpenRouter, Ollama Cloud, Copilot, Cursor, MiniMax, Kiro,
Command Code, SuperGrok) and keeps a changelog as detailed as ours, which makes
it the better of the two upstreams to read for *shared* bugs: same language,
same serde, same endpoints.

Taken this run:

- **The `null`-where-a-list-belongs trap.** Their 1.12.x fix was Codex sending
  `null` rather than `[]` for `additional_rate_limits` / `model_usage`, which
  failed the whole response and put `⚠ API schema drift` on the bar. We carried
  the identical bug on `claude.rs`'s `limits` and `scopes`, proven by a test
  before the fix. Ours is `provider::null_as_default`. Our Codex fetcher was
  immune by accident: it reads extra windows out of a `serde_json::Value`.

Checked, we already do it another way:

- **Their five-minute `.retry_after` backoff after a 429.** They poll every 60s
  and need it. `fetch_and_write` writes the snapshot even when the fetch failed,
  so `cache_is_stale()` goes quiet for `refresh_secs` on its own and the
  rollover rule already refuses to re-ask about an instant that is merely past.

Worth considering, not taken (features, not defects):

- **`schema_version` on `--json`.** They stamp one and document the tolerance
  rule (ignore unknown fields, absent means not applicable). Our `--json` is the
  contract five non-Rust frontends read, and CLAUDE.md already notes those
  install separately from the binary, so version skew is a standing condition
  here rather than a hypothetical.
- **A highest-percent bar window.** Their bar defaults to `auto`, the window
  with the highest percent, with `session` / `weekly` / `monthly` to pin it.
  Ours offers only the fixed `Daily` / `Weekly`. This is the same idea as
  CodexBar `#3543`, and it is the reason that issue could not apply to us.
- **Banked reset credits** (`wham/rate-limit-reset-credits` for Codex). Quota
  resets you earn and redeem by hand, on their own expiry clock. Nothing in our
  payload carries them. They are careful to be read-only and never redeem; so
  should we be.
- **The Claude Desktop token.** The Desktop app stores its own token under the
  same public OAuth client as Claude Code, inside the encrypted `safeStorage`
  blob, and the usage endpoint accepts it. That is a fourth source for the walk
  in `claude.rs`, and it is exactly the hole CLAUDE.md already describes: a
  desktop app that owns the credential and leaves the file a stub. macOS and
  Windows only, so it lands on the path our CI barely exercises.

## Deliberately not taken

- The rest of CodexBar `#2900`: the ChatGPT dashboard spend-controls fallback
  needs a browser session cookie, which is the WebView path we do not have.
- Every macOS-only source: browser-cookie imports, claude-swap, iCloud sync.
- Omarchy's `agents` manifest sync settings (`syncMode`, `syncDir`,
  `syncFileName`, `syncDeviceId`). Our fleet sync is configured in
  `config.toml` and set up from the panel's `y` key, not per-widget.

## Earlier runs

Done in 0.30.x (2026-09-02): QML `textFormat: Text.PlainText` on every label,
Codex `#3120` (cached-field max, backwards-reading guard, headless `codex exec`
rows), Codex access-token JWT expiry (`#3221` / `#3222`). Done in 0.29.x: z.ai
`CREDIT_LIMIT`, Codex personal access tokens, `spend_control.individual_limit`,
the Codex monthly window, Kimi lane names. See git history of this file.

Icons match upstream modulo the `currentColor` recolour, all five re-diffed
this run. Upstream's `-glm` is `-zai`.
