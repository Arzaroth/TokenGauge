# Providers

The eight coding-CLI vendors TokenGauge reads. A provider is one row of
`PROVIDER_META` in `providers.rs`: adding one is a row there plus a fetcher
module (and a reader in `cost/` if it writes transcripts). Every one is
read-only: TokenGauge never refreshes a token except Codex's in-place refresh of
its live `auth.json`.

| Provider | Credential (live) | Windows (session / weekly / tertiary labels) | Reader | remuda store | Plan weight |
| --- | --- | --- | --- | --- | --- |
| Codex | `$CODEX_HOME/auth.json` (`codex`), or `OPENAI_API_KEY` | Session, Weekly, Tertiary + Spend limit (`individual_limit`) | yes | yes | vs Plus |
| Claude | `TOKENGAUGE_CLAUDE_OAUTH_TOKEN`, `~/.claude/.credentials.json`, macOS keychain / Windows Credential Manager | Session, Weekly (all), Weekly (Sonnet) + model-scoped extras, each in its own slot (Ultra has no Session); extra usage as an on-demand cap | yes | yes | vs Pro |
| Kimi | `~/.kimi-code/credentials/kimi-code.json` (`KIMI_CODE_HOME`; the older CLI's `~/.kimi` when `~/.kimi-code` holds no credential) or `KIMI_CODE_API_KEY` | Weekly, Rate Limit + Monthly total (ratio pools) | yes | yes | vs Plus |
| Grok | `~/.grok/auth.json` (`grok login`, `GROK_HOME`) | Pool (weekly or monthly, per the CLI's billing period), On-demand | yes | yes | by price |
| GLM (z.ai) | `Z_AI_API_KEY` (legacy `ZAI_API_TOKEN`), `Z_AI_API_HOST`, `Z_AI_QUOTA_URL` | Weekly, 30-day, 5-hour | no (ccusage) | yes (key) | weekly credits |
| OpenRouter | `OPENROUTER_API_KEY`; optional management key for the balance | Credit, Key limit | no (vendor-reported cost) | no | none |
| opencode Go | `OPENCODE_API_KEY` / `OPENCODE_GO_API_KEY`, then opencode 2's `opencode.db`, then opencode 1's `auth.json` | 5-hour, Weekly, Monthly | no | yes (key) | none |
| Cursor | `CURSOR_ACCESS_TOKEN`, `~/.config/cursor/auth.json` (`~/.cursor/auth.json` on macOS, `CURSOR_CONFIG_DIR`) from `cursor-agent`, then the macOS Keychain | Included, Cursor models, Other models | no | yes | vs Pro |
| Copilot | `COPILOT_QUOTA_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token` | Monthly | no | no | none |
| Antigravity | `agy`'s sign-in: Secret Service keyring, then `antigravity-oauth-token` | Session, Weekly + other model groups | no | no | none |
| DeepSeek | `DEEPSEEK_API_KEY`, opencode's or pi's saved key | balance only | no | no | none |
| MiniMax | `MINIMAX_API_KEY`, opencode's or pi's saved key | Session, Weekly | no | no | none |

Notes:

- **Enable/disable** is per provider in `[providers]` and from every settings pane
  through `tokengauge --set-provider <id>=true|false`.
- **OpenRouter** sells credit, not a plan: the account balance (needs a management
  key; a 403 drops it rather than failing) and an optional per-key cap become a
  window; `usage_daily/weekly/monthly` land in `CostInfo` as vendor-reported cost.
- **opencode Go** is read; opencode Zen (the pay-as-you-go half) needs a browser
  session and is not. opencode 2 files the login in `opencode.db` (table
  `credential`, `OPENCODE_DB` overrides the name), read read-only through
  `rusqlite`: the row opencode selects for `opencode-go`, then for `opencode`
  (opencode 1's id), `active` first, then newest, a `key` or an `oauth` login
  inside its `expires`. Once the database has imported `auth.json` it is the
  only answer; with no database, or one before that import, `auth.json`
  (`opencode-go`, then `opencode`, an `api` entry only) is asked, and a
  database that cannot be read is an error. A row fetched with an OAuth token
  has source `oauth`. An OAuth access token names no account, since it rotates
  (`Login::Access`). remuda switches the key in that same row.
- **Copilot** reads `copilot_internal/user`, which needs a `User-Agent` or answers
  403. A plan with no metered allowance reports zero of every figure, and draws
  no gauge rather than a spent one.
- **Antigravity** only reads `agy`'s sign-in, like every other read-only login. The
  access token lapses within the hour and `agy` alone renews it, so a lapsed token
  is the error "sign-in expired" and the last figures go stale. On Linux the keyring
  goes through `secret-tool`, so no dbus is linked.
- **DeepSeek** sells a balance, one entry per currency, and the funded one is
  shown. CNY is `CreditUnit::Cny` and is never drawn as dollars.
- **MiniMax** reports what is left, per model. The `general` row stands for the
  plan, and a window's status outranks its percentage. A key answers only on its
  own region's host.
- **Cursor** has no public API; TokenGauge calls the dashboard's
  `/api/usage-summary` with the token `cursor-agent` leaves, in its `auth.json`
  or, on macOS, the Keychain (`cursor-access-token` / `cursor-refresh-token`
  under `cursor-user`, read only when the file holds no usable token).
  `--doctor` names the source that answered. The IDE's SQLite state is
  deliberately not read. Every dashboard request carries the full browser
  client-hint set (`BROWSER_HEADERS`), because Vercel's WAF refuses a lone user
  agent with a 403; a 403 therefore reads as "session ended or edge blocked",
  never as a plan problem (`check_cursor_status`).
- **Cursor team budget.** A team plan's summary can report its pools as zero;
  only then (`wants_member_budget`: every pool absent or zero) the fetch reads
  the member's budget (`/api/auth/me` for the email,
  `/api/dashboard/teams`, `/api/dashboard/get-team-spend`) and draws it as the
  single primary window. Best effort on every count: one team only (the
  browser's team selection is not ours to read), at most 20 pages of 50 that
  must all agree and name the member once, a 10s budget that every request,
  `/api/auth/me` included, takes its timeout from. Anything else keeps the
  summary.
- **Claude extra usage** (`extra_usage_cap`) is an on-demand `CreditLimit`
  that resets monthly, USD only, never uncapped. It is kept while reported
  disabled because it ran out (`out_of_credits`, `monthly_limit_reached`,
  `spend_limit_reached`) and dropped when `user_disabled`. A spend-only
  Enterprise seat with no windows reads as that cap rather than an error.
- **Kimi ratio pools.** Some accounts get `usages.{limit_5h,limit_7d,limit_month_total}`
  (ratios) instead of, or beside, `usage`. They fill the 5-hour and weekly
  gauges; the month total is the extra row `kimi-monthly`; `limit_month_code`
  is never added. Where a ratio and a counter describe one window, the counter
  wins only when it reports more and resets within 2s of the ratio
  (`reconcile`).
- **Codex spend cap.** `individual_limit` is the extra row `codex-spend-limit`
  beside the rate windows, and still the primary when there are none.
- **Codex credits are not dollars.** `credits.balance` counts Codex credits,
  so the payload's `Credits.unit` is `credits` and the panel draws a grouped
  count (`62,500`); every other balance is USD, the default an old snapshot
  reads as.
- **Cursor X Subscription.** `/api/dashboard/get-sand-usage-status` answers
  `isUnified: true` on an account whose one weekly pool is shared with Grok,
  while `usage-summary` goes on reporting a monthly cycle at 0%. With an
  included allowance and a reset ahead, the pool is the Included gauge on a
  7-day window and the two monthly pools are dropped (`SandUsage::shared_pool`).
  Asked only when no member budget answered; any failure keeps the summary.
- **Grok period.** The pool's `window_minutes` comes from cli-chat-proxy's
  `/v1/billing?format=credits` (`config.currentPeriod.type`, or the legacy
  monthly fields), best effort; a failure keeps 10080.
- **Malformed limit entries.** Claude's `limits[]` and z.ai's `limits` decode
  through `provider::lenient_list` / `lenient_opt_list`: each entry on its own,
  so a sibling of another shape costs only itself. A field that is not a list
  is still refused as drift.
- Provider protocols and `pace.rs` were ported from CodexBar; the
  `upstream-check` skill watches it.
- Each provider's mark and colour come from `selvedge::brands`, shared with
  remuda. `provider_icon` takes its colour from there (a black-and-white brand,
  Grok, Z.ai or opencode, gets `NEUTRAL_HEX`) and its `mark_hex` from the
  brand's `mark_color`: Claude's mark is drawn in its orange, every other one
  in the foreground, so Codex, Cursor, Kimi and OpenRouter keep their colour as
  an accent only. `--json` carries both, as `color` and `mark_color`.
  `provider_icon_svg_path` writes the mark, its `currentColor` in the mark
  colour or else the neutral foreground, to
  `$XDG_DATA_HOME/tokengauge/icons` (`~/.local/share/tokengauge/icons` when
  that is unset or relative), rewriting one that differs from this build's,
  for the frontends to load by path. A non-empty `TOKENGAUGE_ICON_DIR` is the
  caller's: a mark there is only read, never written or refreshed.
- The neutral foreground suits a dark panel only. The Quickshell widget masks
  the mark with `mark_color` or its theme's foreground instead of drawing the
  file as written, so a black-and-white mark follows a light Omarchy theme;
  that mask paints Kimi's blue dot in the foreground too.
- Middle-click opens a provider's dashboard and back-button its status page
  (`ProviderMeta.urls`).
- Plan weights per provider are in [several-plans.md](several-plans.md).

## Sources

- `crates/tokengauge-core/src/providers.rs` (`PROVIDER_META`, `natively_read`, `provider_auth_status`)
- `crates/tokengauge-core/src/{claude,codex,kimi,grok,glm,openrouter,opencode,cursor}.rs`
- `crates/tokengauge-core/src/provider.rs`, `crates/tokengauge-core/src/config.rs` (`ProvidersConfig`)
- selvedge's `src/brands.rs` and `assets/providers/`
