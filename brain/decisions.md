# Decisions

The non-obvious design decisions and the "why" behind them: the things you cannot
recover by reading the code, only by reading the rationale. Grouped by area; each
entry links the doc that implements it. Entries numbered `0001`-`0004` were the
repository's ADRs (`docs/adr/NNNN-*.md` until the brain migration) and keep their
numbers so "ADR 000N" in commit history stays traceable. New
decisions with real alternatives get the next number; smaller ones are bullets.

## Fleet sync

### 0001 Fleet sync shape

**Decision.** Each device publishes **one file only it ever writes**, into storage
the user already has (a synced folder or an S3-compatible bucket), carrying
**token counts bucketed by UTC hour, provider and model**. No service.

**Why.** The machines are rarely awake at the same time, so sync must be
store-and-forward. Single-writer objects remove conflict resolution entirely, and
every hard part of a sync service (hosting, auth, multi-tenancy, uptime)
disappears with the service.

**Rejected.**
- *A hosted or self-hosted relay*: buys nothing a shared folder does not, costs
  auth, TLS, uptime and a place to run it. Earns its place only for a fleet that
  can share neither a folder nor a bucket.
- *P2P over LAN or Tailscale*: machines are on different networks and seldom
  awake together.
- *Daily totals on the wire*: the day would be the writer's. UTC hours let each
  reader bucket into its own calendar and keep the 5h session window and burn
  rate computable from peer data.
- *Dollars on the wire*: money is tokens times the reader's price table; a peer's
  stale `prices.json` would poison the total and one day would cost two amounts.

**Consequences.** A peer bucket becomes a synthetic `UsageEvent`, so
`build_report` is unchanged and the peer set is one more reader. A ccusage-sourced
provider has no events and cannot sync (Kimi and Grok gained readers since; GLM
still cannot). Sub-hour offsets misplace at most one hour across a day boundary.
See [architecture/sync.md](architecture/sync.md).

### 0002 Symmetric fleet key

**Decision.** One 32-byte symmetric key shared by every device
(XChaCha20-Poly1305), copied between machines the way a Syncthing device id is,
over per-device keypairs.

**Why.** A one-person fleet needs no key distribution, and every mechanism that
would give real revocation costs a flow larger than the feature.

**Consequences.**
- **No revocation.** A lost machine reads fleet usage until every remaining device
  is re-keyed by hand. The docs say so rather than letting "encrypted" imply
  "revocable". `--sync-rotate` is deliberately not built: the manual path
  (`--sync-init --sync-force`, `--sync-join` on each machine) is the honest shape.
  Re-keying starts a new fleet (see hazard 5 in the architecture doc).
- Object names are `HMAC(sync_key, device_id)`, so the storage holder cannot link
  an object to a machine. Fleet size (one object per device) and write timing
  still leak.
- Possession of the key is the only authentication: right for one person's
  machines, wrong for a team.

See [architecture/sync.md](architecture/sync.md#envelope-and-key).

### 0005 Adopting a reinstalled machine

Status: shipped.

**Decision.** `--sync-adopt <device>` folds the retired device's days that
predate this machine's oldest bucket into this machine's slice, publishes once
with the whole store, deletes the retired object, and names the retired id with
the instant in an `adopted` list carried on every later contribution. Peers
fold their own copy by the same rule on every cycle, unless the retired device
has written since, and drop a peer whose object vanished from a target they
listed it on before, unless every known peer vanished at once.

**Why.** A reinstall changes the device id, and the retired entry holds history
no transcript can rebuild. Two entries for one machine double-count every day
whose digests differ.

**Rejected.**
- *Pruning vanished devices alone*: a peer that missed the wide publish (a
  laptop closed for a day) would drop its copy before receiving the history,
  and keep only the wire retention.
- *Always publishing the whole store*: doubles every upload to cover a one-off.
- *Keeping the retired object*: its days keep counting beside the adopted copy
  wherever the digests differ.
- *Merging inside the local window too*: the local device read those days from
  its own transcripts; a second copy can only double them.
- *`covers_from` as the edge*: it is how far a read looked, and a backfill on
  the reinstalled machine's first fetch sets it 400 days back, leaving nothing
  to move.
- *Bare ids in `adopted`*: an adoption that only grows cannot be undone, and a
  live device adopted by mistake would vanish from every peer.

**Consequences.** A peer on an older build ignores `adopted` and double-counts
until it updates or the retired slice ages out. A sync tool that propagates the
delete before the new object can still cost a peer its copy past the wire
retention. See [architecture/sync.md](architecture/sync.md#adopting-a-reinstalled-machine).

### Other sync decisions

- **Tokens, never dollars, cross machines.** Same rule as the cost fixture:
  compare token counts, never days or money. See
  [features/spend-history.md](features/spend-history.md) (a fleet history is
  rated with this machine's archive).
- **The fleet store is local, durable and not gated on sync.** A contribution
  cannot be rebuilt from transcripts (reads reach back to the month start), and a
  machine syncing with nobody still deserves a past. See
  [architecture/sync.md](architecture/sync.md#the-fleet-store-is-the-durable-record).
- **S3 is SigV4 by hand, not `aws-sdk-s3`**: the SDK pulls tokio and dozens of
  crates into a binary with no async runtime to sign three verbs.
- **Setup is built once, in the TUI**, not as a settings pane per desktop: five
  implementations of a secret input is five chances to log one. See
  [features/fleet-sync.md](features/fleet-sync.md).

## Credentials

- **Credentials are read where the tool put them, first usable not first
  present.** Claude Code 2.1.x and the Windows desktop app leave a stub file. See
  [architecture/credentials.md](architecture/credentials.md#live-credential-sources).

### 0003 Credentials under a provider

**Decision.** The **credential** is a sub-unit of the provider **for windows
only**, and inactive credentials are read from a **credential store** a separate
switcher (remuda) writes. TokenGauge reads the store and never writes, moves,
creates or refreshes anything in it.

**Why.** One person can hold several plans at a vendor and should see every one of
their limits, not only the plan the CLI is signed into. Switching, signing in and
keeping inactive tokens alive are the switcher's job.

**Rejected.**
- *TokenGauge switches and refreshes itself*: a refresh token rotates on use, so
  two refreshers lock one out, and a daemon on every machine is the worst place
  for that race. (Codex's in-place refresh of the live `auth.json` stays: it runs
  under `auth.json.lock`, which remuda also takes.)
- *Symlinking the CLI's file into the store*: a CLI that writes with temp file +
  rename replaces the symlink and the store silently stops following.
- *Per-credential cost*: transcripts carry no credential identity, so cost,
  events, buckets, history and sync stay provider-scoped.
- *A credential as its own provider* (`claude-2`): the provider is the unit of
  enable/disable, cost readers and brand, none of which multiply.

**Settled questions.**
- Who refreshes: TokenGauge never refreshes a stored credential; remuda never
  refreshes the active one (the CLI owns it); remuda's timer refreshes inactive
  ones every 30 minutes, so an expired stored token is the `expired` state, not an
  error.
- Which identity counts: only one describing the token actually sent (never
  `.claude.json` for the env override, never a PAT's workspace for Codex).
- Matching follows remuda's order (refresh token, access token, verified
  identity); a digest mismatch is `unverified`, unasked; an unmatched live login
  is its own group and counts in the total only when it cannot be a stored one.
- Store security: owner-only permissions on Unix, refusing a directory or skipping
  a file that fails; no token or email reaches anything written.
- Cadence: live every refresh, inactive every `inactive_refresh_secs` (1800, or
  `refresh_secs` while its 5-hour window is past 10%) or on a reset; a store change or `.last-switch.json` entry makes the snapshot stale.
- The config default does not read remuda's environment variables, which the
  daemon and a frontend can see differently.
- The panel spec input stays one `ProviderRow`; no new `SectionKind`, so six
  frontends draw it unchanged.
- **Weighting.** The combined header weighs each credential by its plan's nominal
  multiplier, not one vote each, in units of the largest plan, with the bar at the
  pooled fraction so an idle Pro cannot make busy plans look free. An unknown
  weight stays out of the total rather than guessed at 1x (which would understate
  a large plan silently). The header's reset is the earliest, when capacity first
  comes back. Team seats weigh 1.25 / 6.25 (Anthropic's first-party "What is the
  Team plan?"; they were 1 and 5 until that was found). The profile names the
  seat where the token only hints at it, so `oauthAccount` decides: off a
  sidecar whose digest matches its tokens, or off `.claude.json` for a live
  token from Claude Code's own stores, and only while the token is still on
  Team. A premium seat reports `seatTier: "team_tier_1"`, and its token the
  organization's `default_raven`, so a `userRateLimitTier` naming an `Nx`
  (`default_claude_max_5x`) is premium first, as is a token tier naming one on
  a token that says `team`; otherwise `seatTier`
  `team_standard` / `team_premium` decides, and no mapping is guessed for
  `team_tier_N`. Without either, the subscription type and an `Nx` tier
  decide. The rule is `selvedge::plans::team_seat`, shared with remuda. A
  seat chosen by hand in remuda (`remuda seat`) outranked the profile for one
  release, while the profile read wrong; it went once the user tier was read.
  Codex first shipped
  without a table, then gained one when OpenAI split Pro into 5x / 10x / 25x Plus
  tiers named on the wire. Grok is weighed by price because xAI publishes no
  multiplier and a guessed 1x would hide a Heavy behind a SuperGrok. opencode Go
  has none (the tier is not on the wire).
- `CACHE_SCHEMA_VERSION` went to 2; it is written and never checked on read.

See [architecture/credentials.md](architecture/credentials.md) (store and snapshot
contracts) and [features/several-plans.md](features/several-plans.md).

### 0004 Opening remuda's page

Status: shipped (the ADR was still marked `proposed`).

**Decision.** Show a button **only while a page is served**, detect that from a
status file remuda writes, and open the page **only by running `remuda open`**.
TokenGauge never sees the page's URL or token.

**Why.** The panel is where a user notices a plan is close to its limit, so it is
where they want remuda's page. The page token exists so only the person who
started the page can drive it.

**Rejected.**
- *Detect an open port*: a listener on 7429 says nothing about whose it is, and the
  page refuses every request without the token.
- *Read `serve.url` and `xdg-open` it*: puts the page credential in TokenGauge's
  process, on a command line every local process can read, and in reach of every
  frontend.
- *Show the button whenever remuda is installed*: a button that fails for anyone
  without `remuda-serve.service` reads as broken. Can come later.

**Consequences.** Needs remuda after 0.4.2. Status is resolved per render, never
stored, so a page started after the last fetch shows on the next render; the
check never spawns `remuda`. Tests never read the developer's runtime directory.
See [features/remuda-page.md](features/remuda-page.md).

## Panel and frontends

- **One panel spec, resolved in core; frontends own chrome only.** Five toolkits
  each growing their own thresholds, labels and formatters drifted every time (the
  TUI's copies of `Tone::for_pace`, `for_trend`, labels and money formatter all
  had). See [architecture/panel-spec.md](architecture/panel-spec.md).
- **Source-grepping tests stand in for a compiler.** `SectionKind` is a Rust enum,
  a QML string and a TypeScript union; nothing else fails when a frontend never
  handles one.
- **A stale row says why, as a `status` section**, because a bare `stale` chip
  reads the same for a blip and a weeks-old expiry.
- **Waybar has no history screen and no remuda button**: its tooltip is the panel
  and a hover surface; left-click opens the TUI. See
  [features/spend-history.md](features/spend-history.md).
- **The GNOME extension is compiled TypeScript**, and `payload_in` refuses a
  source checkout rather than installing `.ts` the shell loads as an error. See
  [architecture/frontends.md](architecture/frontends.md).

## Snapshot and fetching

- **`cache_is_stale()` is the single fetch decision, and age is not the only
  input**: enabling a provider used to do nothing for ten minutes. See
  [architecture/snapshot.md](architecture/snapshot.md).
- **Rendering is not fetching.** A countdown rebuilt only on refetch showed
  "Resets in 6m" beside a dashboard saying 3.
- **`--json` asks the daemon** rather than fetching in the frontend's subprocess:
  the compositor's environment lacks `environment.d`, and a frontend's fetch wrote
  its missing-credential error into the shared snapshot.
- **The instant is the payload's, never the process's**: the TUI once said
  "updated just now" over a ten-minute-old snapshot.

## Cost and history

- **Costs are read from transcripts, ccusage is the cross-check.** Token counts
  must equal ccusage's. See [architecture/cost.md](architecture/cost.md).
- **Unpriced tokens buy one out-of-window price refetch**, recorded so a model
  upstream never carries cannot force a download every fetch.
- **History is a second screen**, not a panel section: a year of bars does not
  belong above the limit gauges.
- **A past month is rated at its own prices, but the archive carries only prices a
  vendor moved**, merged field by field; filled-in missing fields keep today's
  value. See [features/spend-history.md](features/spend-history.md).
- **Hourly buckets roll up to days after 35 days, at midday UTC.** An hourly year
  is a megabyte rewritten per fetch; midnight would shift west-of-UTC readers a
  day.

## Distribution

- **The binary is `tokengauge`, the legacy `tokengauge-waybar` name is kept** for
  0.22.x updaters that only know the old name. See
  [architecture/distribution.md](architecture/distribution.md).
- **The updater is selvedge, shared with TailGauge**: both projects had the same
  1,500 lines and fixed the same bug twice. Gaps are fixed there, not worked
  around here. `selvedge::plans` holds the ALL PLANS arithmetic and the Team seat
  rule so remuda's page cannot print different figures.
- **macOS keeps XDG paths**, so installer, binaries and docs name one place on
  every Unix. The `.app` does not self-update (it would break the seal).
- **Windows installs into one directory**, `%LOCALAPPDATA%\TokenGauge\bin`, and an
  MSI install upgrades through `msiexec`: a stray binary elsewhere is how a July
  build survived twenty releases. The MSI is named `win64` so old updaters'
  substring match cannot pick it.
- **No upstream-polling timer in the product**: a monthly omarchy check shipped in
  0.17.0 and was removed in the same branch; it stays the `upstream-check` skill.
