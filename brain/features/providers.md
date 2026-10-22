# Providers

The eight coding-CLI vendors TokenGauge reads. A provider is one row of
`PROVIDER_META` in `providers.rs`: adding one is a row there plus a fetcher
module (and a reader in `cost/` if it writes transcripts). Every one is
read-only: TokenGauge never refreshes a token except Codex's in-place refresh of
its live `auth.json`.

| Provider | Credential (live) | Windows (session / weekly / tertiary labels) | Reader | remuda store | Plan weight |
| --- | --- | --- | --- | --- | --- |
| Codex | `$CODEX_HOME/auth.json` (`codex`), or `OPENAI_API_KEY` | Session, Weekly, Tertiary + Spend limit (`individual_limit`) | yes | yes | vs Plus |
| Claude | `TOKENGAUGE_CLAUDE_OAUTH_TOKEN`, `~/.claude/.credentials.json`, macOS keychain / Windows Credential Manager | Session, Weekly (all), Weekly (Sonnet) + model-scoped extras; extra usage as an on-demand cap | yes | yes | vs Pro |
| Kimi | `~/.kimi-code/credentials/kimi-code.json` (`KIMI_CODE_HOME`; the older CLI's `~/.kimi` when `~/.kimi-code` holds no credential) or `KIMI_CODE_API_KEY` | Weekly, Rate Limit + Monthly total (ratio pools) | yes | yes | vs Plus |
| Grok | `~/.grok/auth.json` (`grok login`, `GROK_HOME`) | Pool (weekly or monthly, per the CLI's billing period), On-demand | yes | yes | by price |
| GLM (z.ai) | `Z_AI_API_KEY` (legacy `ZAI_API_TOKEN`), `Z_AI_API_HOST`, `Z_AI_QUOTA_URL` | Weekly, 30-day, 5-hour | no (ccusage) | yes (key) | weekly credits |
| OpenRouter | `OPENROUTER_API_KEY`; optional management key for the balance | Credit, Key limit | no (vendor-reported cost) | no | none |
| opencode Go | `OPENCODE_GO_API_KEY` / `OPENCODE_API_KEY` | 5-hour, Weekly, Monthly | no | yes (key) | none |
| Cursor | `CURSOR_ACCESS_TOKEN`, `~/.config/cursor/auth.json` (`~/.cursor/auth.json` on macOS, `CURSOR_CONFIG_DIR`) from `cursor-agent`, then the macOS Keychain | Included, Cursor models, Other models | no | yes | vs Pro |

Notes:

- **Enable/disable** is per provider in `[providers]` and from every settings pane
  through `tokengauge --set-provider <id>=true|false`.
- **OpenRouter** sells credit, not a plan: the account balance (needs a management
  key; a 403 drops it rather than failing) and an optional per-key cap become a
  window; `usage_daily/weekly/monthly` land in `CostInfo` as vendor-reported cost.
- **opencode Go** is read; opencode Zen (the pay-as-you-go half) needs a browser
  session and is not.
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
  Grok, Z.ai or opencode, gets `NEUTRAL_HEX`), and `provider_icon_svg_path`
  writes the mark to `~/.local/share/tokengauge/icons`, rewriting one that
  differs from this build's, for the frontends to load by path.
- Middle-click opens a provider's dashboard and back-button its status page
  (`ProviderMeta.urls`).
- Plan weights per provider are in [several-plans.md](several-plans.md).

## Sources

- `crates/tokengauge-core/src/providers.rs` (`PROVIDER_META`, `natively_read`, `provider_auth_status`)
- `crates/tokengauge-core/src/{claude,codex,kimi,grok,glm,openrouter,opencode,cursor}.rs`
- `crates/tokengauge-core/src/provider.rs`, `crates/tokengauge-core/src/config.rs` (`ProvidersConfig`)
- selvedge's `src/brands.rs` and `assets/providers/`
