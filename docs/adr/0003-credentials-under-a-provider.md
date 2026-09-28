---
status: proposed
---

# A provider holds several credentials, and TokenGauge only reads them

One person can hold several plans at the same vendor, and the panel should show
every one of their limits, not only the plan the CLI is signed into right now.
We chose to make the **credential** a sub-unit of the provider **for windows
only**, and to read the inactive credentials from a **credential store** that
a separate switcher tool writes. TokenGauge reads the store and never writes
it. Nothing in it is refreshed, moved or created by TokenGauge. Switching the
active credential, signing a new one in, and keeping the inactive ones' tokens
alive are all the switcher's job.

## The store contract

- The switcher is [remuda](https://github.com/Arzaroth/remuda). remuda
  resolves its own store from `$REMUDA_STORE`, else
  `$XDG_DATA_HOME/remuda/credentials`, else
  `~/.local/share/remuda/credentials`. TokenGauge does not repeat that
  resolution; it reads the path from its own config key (see the open
  questions for that key's default).
- The store holds every captured credential, the active one's stored copy
  included.
- One directory per provider, one file per credential:
  `<store>/<provider>/<name>.json`. The file has the shape of the provider
  CLI's own credential file (`.credentials.json` for Claude, `auth.json` for
  Codex), so the existing parsers read it unchanged. It holds the credential's
  tokens only: Claude's `mcpOAuth` block stays in the live file, shared by
  every credential.
- A credential file is a `.json` file directly under `<store>/<provider>/`
  whose name neither starts with `.` nor ends in `.meta.json`. Dot-files,
  anywhere in the store, are the switcher's own (`.dirs.json`, its lock,
  set-aside tokens) and are never credentials. So no credential is named with
  a trailing `.meta`: its file would read as another credential's sidecar.
- A sidecar `<name>.meta.json` holds what the credential file lacks:
  `accountId`, the stable identity (Claude's `accountUuid`; for Codex the
  seat, the access token's `chatgpt_account_user_id`, not `tokens.account_id`,
  which names only the workspace every seat of a Team plan shares), `email`,
  `capturedAt`, an optional `label` the panel shows beside the name, and, for Claude only, the `oauthAccount` block the switcher
  restores into `.claude.json` on a switch. Codex's `auth.json` is the whole
  credential, so it has no such block.
- The sidecar also records `credsDigest`, the SHA-256 of the credential file it
  was written for: over the file's exact bytes, as 64 lowercase hex
  characters, so it can be checked without parsing anything. The two files
  are two renames, so a crash between them can leave a sidecar describing other tokens; when the digest does not match,
  TokenGauge must not trust the sidecar's identity for that credential (remuda
  re-identifies it on its next run). A sidecar without the key predates it and
  is trusted.
- The **active credential** is found by identity, not by token: TokenGauge
  reads the live source's tokens as it does today, derives the live identity
  the way remuda does, and matches it against the sidecars. Tokens rotate, and
  identities do not. The live source always wins over the stored copy of the
  same credential, which may be hours behind.
- Deriving the live identity is new work. Nothing TokenGauge reads today
  carries one: Claude's `Oauth` holds the access token, its expiry, scopes,
  rate-limit tier and subscription type. The identity is
  `oauthAccount.accountUuid` in `.claude.json`, which no reader parses yet.
  That file is `~/.claude.json`, beside `~/.claude` rather than inside it,
  unless `CLAUDE_CONFIG_DIR` is set, when it is `$CLAUDE_CONFIG_DIR/.claude.json`;
  so it is not `claude_config_dir().join(".claude.json")`. It is also a
  different file from the one the token came from, which may be the env var or
  the OS store. For Codex the identity is the seat claim in the live access
  token, which exists only on a ChatGPT sign-in.

## Considered options

- **TokenGauge switches and refreshes itself.** Rejected. A refresh token rotates
  on use, so two processes refreshing the same credential lock one of them out.
  A gauge running as a daemon on every machine is the worst place to hold that
  race. Codex's in-place refresh of the *live* `auth.json` stays as it is: it
  predates this, runs under `auth.json.lock`, and touches only the active
  credential. remuda takes the same lock while it replaces `auth.json` on a
  switch, so the two never interleave.
- **Symlinking the CLI's credential file into the store.** Rejected. A CLI that
  writes its file with a temp file and a rename replaces the symlink, and
  then the store silently stops following it.
- **Per-credential cost.** Rejected. Transcripts carry no credential identity,
  so cost, usage events, buckets, history and fleet sync stay provider-scoped.
  A credential owns windows and a plan label, nothing else.
- **A credential as its own provider** (`claude-2`). Rejected. The provider is the
  unit of enable/disable, of the cost readers and of the brand, and none of
  those multiply with the credentials.

## Consequences

`CachedData::Full.payloads` is already a list, and a provider can already
return several payloads. What is new is that a payload names its credential,
and that everything keying on the provider string alone (stale fallback,
`retain_enabled`, `covers`) has to key on provider plus credential. The
selected tab stays a provider: credentials live inside its section.
`CACHE_SCHEMA_VERSION` goes to 2, but `schema_version` is written and never
checked on read today, so the bump protects nothing until a reader refuses a
snapshot of another version.

A provider with one credential and no store renders exactly as it does today.
Several credentials add a combined header to the provider's section and one
meters group per credential, with the active one marked. The panel spec
resolves this, so it lands on all six frontends at once, or not at all.

The combined header weighs each credential by its plan's multiplier, not by a
vote each. A Max 20x and a Pro plan are not two equal halves of one allowance.
Claude's weight comes from the credential's plan: Pro 1, and for a
`rateLimitTier` carrying an `Nx` (Max 5x, Max 20x, Team seats such as
`default_claude_team_5x`), N. That is a new reading of the tier: `plan_label`
extracts the multiplier only for Max and drops Team's, and has none for Pro,
which is identified by its subscription type instead. The header counts
in units of the largest plan: each credential contributes
`used × weight ÷ largest weight`, so a Max 20x and a Pro both at 100% read
"105% of 105%", and a Max 20x at 50% beside a Max 5x at 100% reads "75% of
125%". The bar fills to the pooled fraction, `Σ used × weight ÷ Σ weight`
(60% in the second case), so an idle Pro cannot make busy plans look free.

- The multipliers are the nominal ones the plans are sold with, relative to
  Pro. The real limits are not published and need not scale exactly in every
  window, so the combined figure is an estimate, and the panel does not
  present it as more.
- A credential that is neither Pro nor a tier carrying an `Nx` (Enterprise, or
  a tier string not seen before) has no known weight. It stays out of the
  combined figure and is marked unweighted. Guessing 1x would understate a large plan without saying
  so.
- Weights are per provider. Codex plans need their own table, and credentials
  of two providers never share a header.
- The header's reset is the earliest among the credentials. Whether the reset
  that frees the most weighted capacity is the more useful instant is left to
  the panel work.

Every refresh makes one usage request per credential. The Claude usage endpoint
already rate-limits, so the inactive credentials may need a slower cadence than
`refresh_secs`.

A stored credential whose access token has expired renders as an expired row
in its provider's section. It is not a provider error: the rest of the section
still renders, and the fix is the switcher's, not a re-login. Today an expired
token is a fetch error, `payload_to_rows_with_costs` filters errored payloads
out, and `apply_stale_fallback` drops a provider's errors once it has any live
payload, so this needs a payload state that is expired without being an error.

## Open questions

Left for the implementation to settle, each before the code that depends on it:

- **A live source with no identity.** `TOKENGAUGE_CLAUDE_OAUTH_TOKEN` and a
  Codex API key carry none. A Codex personal access token resolves through
  `whoami` to `chatgpt_account_id`, the workspace, not the seat, so matching it
  against seat identities would mark every seat in the workspace active.
  Unmatched, the active plan would be counted twice. On Claude, the token's
  source and `.claude.json` can also name different identities.
- **Store entries that do not line up.** A live identity that matches no
  sidecar (a credential not captured yet), a credential file with no sidecar,
  two store files with the same `accountId`, and a sidecar whose credential
  file is gone.
- **Which window anchors session cost.** `cost::anchor_burn_rates` takes the
  session window per payload and writes one figure per provider, so with a
  payload per credential the last one wins. Session cost and burn rate should
  follow the active credential. `docs/sync.md` still calls the window
  account-scoped, and changes with this.
- **Who may refresh the stored copy of the active credential.** The CLI, and
  Codex's in-place refresh, rotate its refresh token under the live file's
  lock, not the store's. The contract should forbid the switcher refreshing a
  credential while it is the active one.
- **A digest mismatch, rendered.** Whether the credential still shows (with no
  identity, label or active mark) or is hidden; whether `label`, `email` and
  `plan` are distrusted with the identity; whether trusting a digest-less
  sidecar ever ends; and that the digest is computed over the same bytes that
  are parsed, read once.
- **The store's security.** Refuse a store or file that is not the user's own
  and private, as `write_auth` keeps `auth.json` at 0600. No store token
  reaches the snapshot, `--json`, `--doctor` or `stale_reason`, and `email`
  stays out of the snapshot, as sync keeps identity detail on the machine.
- **Cadence and freshness.** `cache_is_stale()` is the single fetch decision
  and marks the whole snapshot stale when any window resets, so an inactive
  credential's reset refetches every credential. A slower cadence for inactive
  credentials needs a place in that decision, and a store change (a credential
  added, the active one switched) has to make the snapshot stale, which
  `covers()` cannot see.
- **The config key's default.** Mirroring remuda's resolution would read
  `$REMUDA_STORE` and `$XDG_DATA_HOME`, and the daemon and a frontend's
  in-process fetch inherit different environments, so they could read
  different stores. The default should not depend on the environment, and
  Windows and macOS need one of their own.
- **`--doctor`.** One validated check per stored credential, and a status for
  a store that is missing, unreadable or empty, under the rule that the
  Credentials check validates rather than stats.
- **The panel spec's input.** `panel_spec` and `bar_tooltip` take one
  `ProviderRow`, and `SECTION_IDS` is fixed, so a combined header needs a
  grouped input and several `limits` groups need ids. The active and unweighted
  markers should fit `badge` or `footnote`; a new field or `SectionKind` is a
  six-frontend change.
- **Codex weights.** No table exists yet, so a Codex header has no combined
  figure until one does.
