---
status: accepted
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
  resolution; it reads the path from its own config key,
  `[credentials] store` (see the snapshot contract below for its default).
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
  `capturedAt`, an optional `label` the panel shows beside the name, and,
  for Claude only, the `oauthAccount` block the switcher restores into
  `.claude.json` on a switch. Codex's `auth.json` is the whole
  credential, so it has no such block.
- The sidecar also records `credsDigest`, the SHA-256 of the credential file it
  was written for: over the file's exact bytes, as 64 lowercase hex
  characters, so it can be checked without parsing anything. The two files
  are two renames, so a crash between them can leave a sidecar describing
  other tokens; when the digest does not match, TokenGauge must not trust the
  sidecar's identity for that credential (remuda re-identifies it on its next
  run). A sidecar without the key predates it and is trusted.
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
  combined figure and is marked unweighted. Guessing 1x would understate a
  large plan without saying so.
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


## Decisions

The questions this record left open, settled before the code that depends on
each.

- **Who refreshes what.** TokenGauge never refreshes, moves or writes a stored
  credential. remuda never refreshes the credential that is active: the CLI
  owns it, and so does TokenGauge's Codex in-place refresh of the *live*
  `auth.json`, under `auth.json.lock`. remuda's timer refreshes the inactive
  ones every 30 minutes. A stored credential whose access token has expired is
  therefore a timer that has not run, and TokenGauge says so rather than
  fixing it.
- **Which identity counts.** An identity is used only when it describes the
  token actually sent. For Claude that is `oauthAccount.accountUuid` in
  `.claude.json`, and only when the token came from `.credentials.json` or the
  OS store: `TOKENGAUGE_CLAUDE_OAUTH_TOKEN` carries no identity, and
  `.claude.json` describes the file's login, not the override's. For Codex it is
  the seat claim remuda reads (`chatgpt_account_user_id` in the access token,
  else `<user>__<workspace>` from the id token). A personal access token and an
  API key have none; `whoami`'s workspace id is never used as one.
- **Matching the live login** follows remuda's order: a stored credential whose
  refresh token, then whose access token, equals the live one's; failing that,
  the verified sidecar whose `accountId` is the live identity. A match makes the
  live payload that credential's, and the stored copy is not fetched: the live
  source wins.
- **A live login that matches nothing** is drawn as its own group, marked
  active, and every stored credential is fetched. It counts in the combined
  figure only when it has an identity and no stored credential is unverified;
  otherwise it may be one of the stored ones, would be counted twice, and is
  left out and marked so.
- **Store entries that do not line up.** A credential file with no readable
  sidecar, or that does not parse, is skipped, as remuda skips it, and
  `--doctor` names it. Two verified sidecars with the same `accountId` read as
  one credential: the one the live login matched, else the first by name; the
  rest are skipped and named by `--doctor`. A sidecar whose credential file is
  gone is never listed, because listing is by credential file.
- **A digest mismatch** is drawn as its group in an *unverified* state, with no
  request made and nothing from its sidecar: no label, no identity, no active
  mark by identity. If the live login's tokens are that file's tokens, the live
  payload still takes its name, which is what remuda reports as an unconfirmed
  active credential. The digest is computed over the bytes read once, and those
  same bytes are parsed. A sidecar without `credsDigest` stays trusted: remuda
  trusts it too and replaces it first on its next save of that credential.
- **Session cost** follows the active credential. `cost::anchor_burn_rates`
  skips a payload with `active: false`, so an inactive plan's window can no
  longer be the one that measures the session. Threshold notifications follow
  it too: they key on provider and window, and would fire for whichever
  credential was read last.
- **The store's security.** On Unix the store root and each provider directory
  must belong to the current user and not be writable by group or others, a
  sidecar must belong to the user and not be writable by others (it names the
  account), and a credential file must belong to the user with no group or
  other permission bits. "The current user" is the home directory's owner, and
  with no home directory nothing passes. A directory that fails refuses that whole directory; a file that fails
  is skipped. Both are named by `--doctor`. Windows has no such check: the
  default store sits in the user's profile. No token reaches the snapshot,
  `--json`, `--doctor` or `stale_reason`, and `email` is never read into
  anything that is written.
- **Cadence.** The live login is fetched every refresh, as before. An inactive
  credential is fetched when its last payload is older than
  `[credentials] inactive_refresh_secs` (default 1800, remuda's refresh period)
  or a window it reported has reset since, or its name now holds another
  account; otherwise its last payload is carried into the new snapshot
  unchanged, with its own `usage.updatedAt`. Stored credentials are asked
  alongside the live login, not after it.
  `cache_is_stale()` stays the single fetch decision and keeps its whole-snapshot
  rollover rule: a rollover of an inactive window makes the snapshot stale, and
  the fetch that follows asks that credential again and carries the others.
- **A store change makes the snapshot stale.** `CacheMeta.credentials` records
  the store's credential names per provider at the write, and
  `cache_is_stale()` compares them with the store as it is now, so a credential
  added, removed or renamed refetches. `<store>/.last-switch.json` is relied on:
  a provider whose entry is later than the snapshot's write refetches, because
  its active credential changed. A missing or unreadable file is no signal
  rather than an error, and neither is an entry stamped in the future (a clock
  that stepped back), which would otherwise refetch on every render.
- **The config key's default** does not read remuda's variables:
  `~/.local/share/remuda/credentials` on Linux and macOS (remuda's own fallback,
  and not `$XDG_DATA_HOME` or `$REMUDA_STORE`, which the daemon and a frontend's
  in-process fetch can see differently), and the local application data folder
  (`%LOCALAPPDATA%\remuda\credentials`, resolved through the known folder, not
  the variable) on Windows. A user who moved remuda's store sets the key. An
  empty string turns the store off; a directory that does not exist is no store.
  A leading `~` is the home directory, as in `[sync.dir] path`. The home
  directory itself still comes from `$HOME` on Unix, which the daemon and a
  frontend share in practice.
- **`--doctor`** has a *Credential store* section: one line for the store (a
  missing store passes, since remuda is optional; a refused or unreadable one
  fails), and one validated line per stored credential, made offline with the
  same parse, digest, expiry and scope checks the fetch makes, naming which one
  is active. Skipped entries are failed lines saying why.
- **The panel spec's input** is still one `ProviderRow`.
  `payload_to_rows_with_costs` groups a provider's credential-tagged payloads
  into one row: the top level is the active credential's, so the bar text, the
  refresh hint and notifications follow the plan in use, and cost is attached
  once; `credentials` holds a row per credential, active first, then store
  order. With no active credential (the live login failed with nothing to
  restore) the top level carries no figures, so no other plan reads as the one
  in use. With any credential group, `panel_spec` emits the stale lines of every group
  in `status`, a `plans` section (the combined header, `Meters`), then one
  `limits` section per credential. The groups share the id and differ in a new
  optional `group` field on `Section` (the store name) that no frontend needs to
  read. `Section.title` becomes a string resolved per group: the name, the
  label, the plan, and the markers, `active` and `not in total`. An expired or
  unverified group is a `Rows` section with one line saying which and why. No
  `SectionKind` is added, so the six frontends draw it as they are.
- **The combined header** has one meter per window label that at least two
  weighted credentials report, valued `"<used>% of <capacity>%"` in units of the
  largest plan, filled to the pooled fraction, and tinted by it. Its reset is
  the earliest among them: that is when capacity first comes back, and the
  tooltip lists each credential's own figure and reset. Its badge says
  `estimate`, because the weights are nominal.
- **The bar icon's hover** is the active group's windows, then the combined
  figures, then today's spend.
- **Team seats weigh 1 (standard) and 5 (premium)**, as a Pro and a Max 5x.
  A standard seat's tier (`default_raven`) carries no `Nx`, so the subscription
  type decides; a premium seat is recognised by `premium` in either field or by
  an `Nx` tier. Third-party guides quote 1.25 and 6.25 for the two seats; the
  nominal pairing is used because nothing first-party states either, and no
  machine-readable table of subscription multipliers exists to follow the way
  `pricing.rs` follows LiteLLM's.
- **Codex weights.** No table: Codex plans are not sold as multiples of one
  another in a way `plan_type` names, and OpenAI is moving the plans to
  API-spend-equivalent allowances (the reopened Pro $200 nets out at half the
  API spend of the old one, with no 5h window), so a fixed multiplier would be
  wrong on arrival. A Codex provider shows its groups and no combined header.
- `CACHE_SCHEMA_VERSION` is 2. It is still written and never checked on read.

## The snapshot contract

remuda reads the snapshot TokenGauge writes and relies on exactly this.

Each entry of `payloads[]` gains:

- `credential` (string): the store name, the `<name>` of
  `<store>/<provider>/<name>.json`. Absent for a provider with no store, when
  the store is off or empty, and for a live login that matches no stored
  credential.
- `active` (bool): present on every `claude` and `codex` payload, `true` on
  the one fetched with the CLI's live login and `false` on one fetched with a
  stored credential. Absent for other providers.
- `credentialState` (string): absent on a payload whose usage was fetched.
  `"expired"` when a stored credential's access token has expired and no request
  was made; `"unverified"` when its sidecar's digest does not match its file and
  no request was made. Either way `usage` carries no windows and `error` is
  null, so it is a payload rather than an error. A value a reader does not know
  is a state it cannot draw, not a failure.
- `credentialLabel` (string): the sidecar's `label`, absent when it has none or
  is not trusted.
- `planWeight` (integer): the plan's multiplier relative to Pro, when one is
  known. Also absent on a live login that matches no stored credential and
  cannot be told apart from one, which is how it is left out of the combined
  figure.

Each entry of the top-level `errors[]` gains `credential` (the store name, when
the failed fetch is attributed to one) and `active` (`true` when it was the live
login's fetch, absent otherwise). A stored credential's error message also
starts with its name (`perso: ...`), because every frontend draws an error as
provider and message.

A payload may also carry `accountDigest`, a short hash of the credential's
account that TokenGauge uses to never carry one account's figures under a name
that now holds another. It is TokenGauge's own; remuda need not read it. A payload served from the previous snapshot
after its fetch failed keeps its `credential` and `active`, with `stale: true`
and `staleReason`. A carried inactive payload keeps its own `usage.updatedAt`,
which can be older than `meta.updatedAtMs`.

No payload or error carries a token or an email. The fields remuda already
reads (`meta.updatedAtMs`, `provider`, `stale`, `staleReason`, the windows,
`errors[].provider` and `.message`, a bare-array legacy snapshot) are unchanged.

Config: `[credentials] store` (default above) and
`[credentials] inactive_refresh_secs` (default 1800). TokenGauge relies on
`<store>/.last-switch.json` as `{"<provider>": <epoch ms of its last switch>}`.
