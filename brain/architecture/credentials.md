# Credentials

Where a provider's token comes from, how a provider holds several credentials,
and the two contracts with [remuda](https://github.com/Arzaroth/remuda): the
**store contract** (what TokenGauge reads) and the **snapshot contract** (what
remuda reads back). The decision and its rejected alternatives are
[0003 in ../decisions.md](../decisions.md#0003-credentials-under-a-provider);
the user view is [../features/several-plans.md](../features/several-plans.md).
Regression rules live in `CLAUDE.md` ("A credential is where the tool put it"
and "A provider holds several credentials").

## Live credential sources

Each provider fetcher reads its CLI's own sign-in (table in
[../features/providers.md](../features/providers.md)). Claude is the subtle one:
`claude.rs` reads `TOKENGAUGE_CLAUDE_OAUTH_TOKEN`, then
`~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), then the OS store (macOS
keychain / Windows Credential Manager via `keyring`, compiled only there), and
takes the first that is **usable, not merely present**: Claude Code 2.1.x on
macOS and the Windows desktop app both leave the file a stub. An empty access
token is "not signed in - run `claude setup-token`", distinct from "expired".

- `--doctor` validates, never stats: `provider_auth_status` runs the same source
  walk offline.
- `claude_config_dir()` is the one resolver of `CLAUDE_CONFIG_DIR`, shared by the
  credential and transcript readers.
- TokenGauge never refreshes a token, except Codex's in-place refresh of the
  **live** `auth.json` under `auth.json.lock` (remuda takes the same lock).

## The credential store contract

- The store path is `[credentials] store`. Default
  `~/.local/share/remuda/credentials` on Linux and macOS (remuda's own fallback;
  `$XDG_DATA_HOME` and `$REMUDA_STORE` are deliberately not read, since the daemon
  and a frontend may see them differently), `%LOCALAPPDATA%\remuda\credentials`
  (via the known folder) on Windows. Empty string turns it off; a missing
  directory is no store; a leading `~` is home.
- Layout: `<store>/<provider>/<name>.json`, one file per credential, in the shape
  of the CLI's own file (`.credentials.json` for Claude, `auth.json` for Codex,
  Grok and cursor-agent, `credentials/kimi-code.json` for Kimi), so the existing
  parsers read it. GLM and opencode Go hold `{"key": "..."}`. Claude's
  `mcpOAuth` block stays in the live file.
- A credential file is a `.json` directly under `<store>/<provider>/`, not a
  dot-file and not `*.meta.json`. Dot-files (`.dirs.json`, locks, set-aside
  tokens, `.last-switch.json`) are remuda's own.
- **Sidecar** `<name>.meta.json`: `accountId` (Claude `accountUuid`; Codex the
  seat, `chatgpt_account_user_id`, not the workspace `tokens.account_id`; Grok the
  token `sub`; Kimi the `user_id` from `/me`; Cursor the id after `|` in `sub`;
  an API key `key-` + first 16 hex of its SHA-256, mirrored by
  `credentials::key_account`), `email`, `capturedAt`, optional `label`, Claude's
  `oauthAccount` (TokenGauge reads only its `seatTier` and
  `userRateLimitTier`, for a Team seat's weight), and `credsDigest`.
- **`credsDigest`** is the SHA-256 of the credential file's exact bytes, 64
  lowercase hex. On mismatch the sidecar is not trusted for that credential (the
  group draws `unverified`, unasked). The bytes are read once and those bytes
  are parsed. A sidecar without the key predates it and is trusted.
- **Security (Unix).** Store root and provider directories owned by the user
  (the home directory's owner) and not group/other-writable; sidecars owned and
  not other-writable; credential files with no group/other bits. A failing
  directory is refused whole, a failing file skipped; both named by `--doctor`.
  No token reaches the snapshot, `--json`, `--doctor` or `stale_reason`, and
  `email` is never written anywhere.
- `<store>/.last-switch.json` is `{"<provider>": <epoch ms of last switch>}`. A
  missing or unreadable file, or a future-stamped entry, is no signal.
- `<store>/.attribution.jsonl` (the ledger) and `<store>/.proxy-requests.jsonl`
  (the proxy's request log) say which login spent what. Their lines are
  `selvedge::usage::{ledger, requests}`, declared once for remuda's writer and
  this reader; read by `cost::remuda`
  ([../decisions.md](../decisions.md#0006-spend-is-attributed-from-what-remuda-recorded)).
  A file that cannot be read is a read gap, so the record is merged, not
  replaced.

## Matching the live login

Follows remuda's order: a stored credential whose refresh token, then access
token, equals the live one; else the verified sidecar whose `accountId` is the
live identity. An identity counts only when it describes the token actually sent:
Claude's `oauthAccount.accountUuid` in `.claude.json` (which sits beside
`~/.claude`, or in `$CLAUDE_CONFIG_DIR`) only for a token from the file or the OS
store, never for the env override; Codex's seat claim only on a ChatGPT sign-in,
never a PAT's workspace. Kimi has no offline identity, so it matches by tokens
alone.

- A match makes the live payload that credential's; the stored copy is not
  fetched (the live source wins).
- A live login matching nothing is its own group, marked active; it counts in
  the combined figure only with an identity and no unverified stored credential.
- A credential file with no readable sidecar, or unparseable, is skipped. Two
  verified sidecars with one `accountId` read as one credential (the matched one,
  else first by name). All named by `--doctor`.

## Cadence

The live login is fetched every refresh. An inactive credential is fetched when
its last payload is older than `[credentials] inactive_refresh_secs` (1800,
remuda's refresh period, or `refresh_secs` while its 5-hour window is past 10%,
which means another machine or tool is spending from it; `in_use` in
`credentials.rs`), when one of its windows has reset, or when its name now
holds another account (`accountDigest`); otherwise its payload is carried
unchanged with its own `usage.updatedAt`. A stored credential with an expired
access token is the `expired` state, not an error: remuda's timer refreshes
inactive credentials every 30 minutes, so expiry means that timer has not run.
An API key never expires; a refused key is an error.

## From payloads to one row

`fetch_all_providers` tags each payload; `payload_to_rows_with_costs` groups a
provider's payloads into one `ProviderRow`. The top level is the active
credential's (bar text, refresh hint, notifications follow the plan in use; cost
attached once); `credentials` holds a row per credential, active first, then
store order. With no active credential the top level carries no figures.
`cost::anchor_burn_rates` and threshold notifications skip `active: false`.

## The snapshot contract (what remuda reads)

Each `payloads[]` entry gains:

- `credential` - the store `<name>`. Absent with no store, store off or empty,
  and for a live login matching nothing.
- `active` - on every `claude` and `codex` payload: `true` for the live login's
  fetch, `false` for a stored credential's. Absent for other providers.
- `credentialState` - absent when usage was fetched; `"expired"` or
  `"unverified"` when no request was made (no windows, `error` null). An unknown
  value is a state a reader cannot draw, not a failure.
- `credentialLabel` - the sidecar's `label`, when present and trusted.
- `planWeight` - the plan multiplier when known; an integer when whole, a
  fraction for Team seats (1.25, 6.25; remuda reads fractions from 0.10.0).
  Absent on an unmatched live login that cannot be told apart from a stored one.
- `usage.bankedResets` - on a Claude payload holding a usable banked reset:
  `available` (how many), `expiresAt` (the soonest grant's expiry, RFC 3339
  as Anthropic wrote it, or null) and `kinds`, the same count split by what a
  reset clears: `[{kind, count, expiresAt, clears, idle}]`, `kind` one of
  `session`, `weekly`, `full`, `fable`, `clears` the window keys every one of
  its grants resets (omitted when none are named), `idle` true when every one
  of them would clear nothing now (omitted otherwise), the whole list absent
  when the provider does not say (Codex).
  Absent otherwise
  ([../features/banked-resets.md](../features/banked-resets.md)).
- `usage.<window>.burn` - on each window with a reset and a duration (the
  extra windows' included): `perHour`, the percent of the window spent per
  hour lately, and `overMinutes`, the stretch it was measured over
  ([../features/window-history.md](../features/window-history.md)). Absent on
  a window without both, and on a snapshot from before 0.54.
- `usage.<window>.openedAt` - RFC 3339, on a window refilled in place with its
  reset left where it was (a banked Claude reset): when the instance opened.
  Absent otherwise ([../features/window-history.md](../features/window-history.md)).
- `accountDigest` - TokenGauge's own guard against carrying one account's figures
  under a name that now holds another; remuda need not read it.

Beside the snapshot, `tokengauge-window-history.json` holds `trails`, each
with `provider`, `credential` (absent for an unmatched live login), `window`
(`primary`, `secondary`, `tertiary`, or an extra window's id or title),
`windowMinutes`, `resetsAt` (RFC 3339) and `samples`, `[ms, percent]` pairs
oldest first, when each percent was first and last seen. A trail holds the
window's current instance only, and `openedAt` (RFC 3339) when it was refilled
in place. A trail may also carry `accountDigest`, TokenGauge's own as on the
payload. `closed` lists the windows of a day or longer that have ended, each
kept a year: `provider`, `credential` (absent for an unmatched live login),
`window`, `windowMinutes`, `openedAt` and `resetsAt` (RFC 3339, the reset it
was due at; a banked reset ends it sooner), `used` (the last percent seen),
`seenAt` (RFC 3339, when) and `accountDigest` when the trail had one. A trail
ended because its name now holds another account leaves no record. Absent
until a window has closed, and on a file from before 0.56.

With fleet sync on, `tokengauge-window-fleet.json` beside it has the same
shape: this machine's history with the trails and closed windows of the rest
of its fleet folded in. A reader that wants what the fleet saw reads it, and
falls back to `tokengauge-window-history.json` when it is absent, as it is
with sync off, no peer, or a history that cannot be read.

Each top-level `errors[]` entry gains `credential` and `active`; a stored
credential's message starts with its name (`perso: ...`). A payload served stale
after a failed fetch keeps `credential` and `active`, with `stale: true` and
`staleReason`. Fields remuda already read are unchanged. No payload or error
carries a token or an email.

`payload::tests::the_credential_keys_remuda_reads_are_spelled_as_decision_0003_says`
`payload::tests::the_banked_reset_keys_remuda_reads_are_spelled_as_decision_0003_says`
and `window_history::tests::the_keys_remuda_reads_are_spelled_as_decision_0003_says`
pin the names: renaming one is a change to decision 0003 and to remuda. Every
test config turns the store off (`CredentialsConfig::off()`, or
`[credentials] store = ""`).

## Sources

- `crates/tokengauge-core/src/credentials.rs` (`StoreReader`, `key_account`, store walk, permission checks)
- `crates/tokengauge-core/src/providers.rs` (`PROVIDER_META[].store`, `provider_auth_status`)
- `crates/tokengauge-core/src/{claude,codex,grok,kimi,cursor,glm,opencode}.rs` (live login, stored fetch, `plan_weight`)
- `crates/tokengauge-core/src/fetch.rs`, `crates/tokengauge-core/src/rows.rs`, `crates/tokengauge-core/src/payload.rs`
- `crates/tokengauge-core/src/snapshot.rs` (`CacheMeta.credentials`), `crates/tokengauge-core/src/doctor.rs`
