# Providers

The eight coding-CLI vendors TokenGauge reads. A provider is one row of
`PROVIDER_META` in `providers.rs`: adding one is a row there plus a fetcher
module (and a reader in `cost/` if it writes transcripts). Every one is
read-only: TokenGauge never refreshes a token except Codex's in-place refresh of
its live `auth.json`.

| Provider | Credential (live) | Windows (session / weekly / tertiary labels) | Reader | remuda store | Plan weight |
| --- | --- | --- | --- | --- | --- |
| Codex | `$CODEX_HOME/auth.json` (`codex`), or `OPENAI_API_KEY` | Session, Weekly, Tertiary | yes | yes | vs Plus |
| Claude | `TOKENGAUGE_CLAUDE_OAUTH_TOKEN`, `~/.claude/.credentials.json`, macOS keychain / Windows Credential Manager | Session, Weekly (all), Weekly (Sonnet) + model-scoped extras | yes | yes | vs Pro |
| Kimi | `~/.kimi-code/credentials/kimi-code.json` or `KIMI_CODE_API_KEY` (`KIMI_CODE_HOME`) | Weekly, Rate Limit | yes | yes | vs Plus |
| Grok | `~/.grok/auth.json` (`grok login`, `GROK_HOME`) | Weekly, On-demand | yes | yes | by price |
| GLM (z.ai) | `Z_AI_API_KEY` (legacy `ZAI_API_TOKEN`), `Z_AI_API_HOST`, `Z_AI_QUOTA_URL` | Weekly, 30-day, 5-hour | no (ccusage) | yes (key) | weekly credits |
| OpenRouter | `OPENROUTER_API_KEY`; optional management key for the balance | Credit, Key limit | no (vendor-reported cost) | no | none |
| opencode Go | `OPENCODE_GO_API_KEY` / `OPENCODE_API_KEY` | 5-hour, Weekly, Monthly | no | yes (key) | none |
| Cursor | `~/.config/cursor/auth.json` (`cursor-agent`) or `CURSOR_ACCESS_TOKEN` | Included, Cursor models, Other models | no | yes | vs Pro |

Notes:

- **Enable/disable** is per provider in `[providers]` and from every settings pane
  through `tokengauge --set-provider <id>=true|false`.
- **OpenRouter** sells credit, not a plan: the account balance (needs a management
  key; a 403 drops it rather than failing) and an optional per-key cap become a
  window; `usage_daily/weekly/monthly` land in `CostInfo` as vendor-reported cost.
- **opencode Go** is read; opencode Zen (the pay-as-you-go half) needs a browser
  session and is not.
- **Cursor** has no public API; TokenGauge calls the dashboard's
  `/api/usage-summary` with the token `cursor-agent` leaves on disk. The IDE's
  SQLite state is deliberately not read.
- Provider protocols, `pace.rs` and the brand icons in `assets/providers/` were
  ported from CodexBar; the `upstream-check` skill watches it.
- Middle-click opens a provider's dashboard and back-button its status page
  (`ProviderMeta.urls`).
- Plan weights per provider are in [several-plans.md](several-plans.md).

## Sources

- `crates/tokengauge-core/src/providers.rs` (`PROVIDER_META`, `natively_read`, `provider_auth_status`)
- `crates/tokengauge-core/src/{claude,codex,kimi,grok,glm,openrouter,opencode,cursor}.rs`
- `crates/tokengauge-core/src/provider.rs`, `crates/tokengauge-core/src/config.rs` (`ProvidersConfig`)
- `assets/providers/`
