# Upstream baselines

Rewritten at the end of every `upstream-check` run.

## Last checked: 2026-08-24

| Upstream | Baseline | Notes |
| --- | --- | --- |
| steipete/CodexBar | `v0.55.0` (2026-08-24) | Release notes through v0.55.0 triaged |
| basecamp/omarchy | `43bfe9b9` on `quattro` (2026-08-23) | Previous baseline `ed7bae4a` (2026-08-20), left by the removed watcher in `~/.local/state/tokengauge/omarchy-upstream.sha` |

## Where we forked from

- CodexBar: the native fetchers landed 2026-07-16..18 (`af11ce4`, `4c1c098`,
  `274a2dd`), against CodexBar v0.44.0 / v0.45.0. `pace.rs` (`b484b1e`) is a
  port of `UsagePace.swift`, whose last upstream change was 2026-07-03, so the
  port is current.
- Omarchy: the widget landed 2026-08-21 (`efc16ae`), against omarchy
  `4.0.0.alpha`.

## Backport status (2026-08-24)

Done, in `[Unreleased]`:

- **z.ai/GLM `CREDIT_LIMIT`** (CodexBar #2724 / #2712). `glm.rs` classified only
  `TOKENS_LIMIT` as a quota, so a credit plan's windows were mistaken for the
  time limit and read 0% used. Quota now means `TOKENS_LIMIT` or `CREDIT_LIMIT`.
- **Codex personal access tokens** (#3060). `personal_access_token` in
  `auth.json`, identity resolved through
  `auth.openai.com/api/accounts/v1/user-auth-credential/whoami`, OAuth still
  preferred when both are present.
- **Codex `spend_control.individual_limit`** (#2900). Third fallback for the
  primary gauge, after the root and `rate_limit` individual limits.
- **Codex monthly window** (#2600). A window of four weeks or more gets its own
  "Monthly" row instead of the Session slot.
- **Kimi lane names** (#2741). Extra windows named by duration, and a rolling
  limit that duplicates the weekly primary is dropped.

Deliberately not taken:

- The rest of #2900. When `wham/usage` carries no limit at all, upstream falls
  back to the ChatGPT dashboard's spend-controls monthly-usage API, which needs
  a browser session cookie. That is the WebView path TokenGauge does not have.
- Grok #3157, Claude #2634: checked, already covered on our side (our
  percent-less guard matches the first; the second is macOS Keychain, and Linux
  reads `~/.claude/.credentials.json`).
- Every macOS-only source: browser-cookie imports, claude-swap, iCloud sync.

Icons match upstream modulo the `currentColor` recolour; upstream's `-glm` is
now `-zai`.
