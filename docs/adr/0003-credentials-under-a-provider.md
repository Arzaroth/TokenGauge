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

- The switcher is [remuda](https://github.com/Arzaroth/remuda). Its store is
  `$REMUDA_STORE`, else `$XDG_DATA_HOME/remuda/credentials`.
- One directory per provider, one file per credential:
  `<store>/<provider>/<name>.json`. The file has the shape of the provider
  CLI's own credential file (`.credentials.json` for Claude, `auth.json` for
  Codex), so the existing parsers read it unchanged. It holds the login only:
  Claude's `mcpOAuth` block stays in the live file, shared by every
  credential.
- A sidecar `<name>.meta.json` holds what the credential file lacks: the stable
  identity (Claude's `accountUuid`, Codex's `account_id`), the email, when it
  was captured, an optional display label, and the block the switcher restores
  into the CLI's config on a switch (Claude's `oauthAccount`).
- The **active credential** is found by identity, not by token: TokenGauge
  reads the live source exactly as it does today and matches its identity
  against the sidecars. Tokens rotate, and identities do not. The live source
  always wins over the stored copy of the same credential, which may be hours
  behind.
- TokenGauge reads the store path from a config key that defaults to
  remuda's.

## Considered options

- **TokenGauge switches and refreshes itself.** Rejected. A refresh token rotates
  on use, so two processes refreshing the same credential lock one of them out.
  A gauge running as a daemon on every machine is the worst place to hold that
  race. Codex's in-place refresh of the *live* `auth.json` stays as it is: it
  predates this, runs under `auth.json.lock`, and touches only the active
  credential.
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

The snapshot carries one payload per credential instead of one per provider,
so `CACHE_SCHEMA_VERSION` goes to 2. Everything that keys on the provider string
(stale fallback, `retain_enabled`, `covers`, the selected tab) has to key on
provider plus credential.

A provider with one credential and no store renders exactly as it does today.
Several credentials add a combined header to the provider's section and one
meters group per credential, with the active one marked. The panel spec
resolves this, so it lands on all six frontends at once, or not at all.

The combined header weighs each credential by its plan's multiplier, not by a
vote each. A Max 20x and a Pro plan are not two equal halves of a pool. Claude's
weight is the multiplier `plan_label` already reads out of `rateLimitTier`
(Pro 1, Max 5x 5, Max 20x 20, Team seats by their own `Nx`). The header counts
in units of the largest plan: each credential contributes
`used × weight ÷ largest weight`, so a Max 20x and a Pro both at 100% read
"105% of 105%", and a Max 20x at 50% beside a Max 5x at 100% reads "75% of
125%". The bar fills to the pooled fraction, `Σ used × weight ÷ Σ weight`
(60% in the second case), so an idle Pro cannot make a busy pool look free.

- The multipliers are the nominal ones the plans are sold with, relative to
  Pro. The real limits are not published and need not scale exactly in every
  window, so the combined figure is an estimate, and the panel does not
  present it as more.
- A credential whose tier carries no `Nx` (Enterprise, or a tier string not
  seen before) has no known weight. It stays out of the combined figure and is
  marked unweighted. Guessing 1x would understate a large plan without saying
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
still renders, and the fix is the switcher's, not a re-login.
