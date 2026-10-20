# Banked resets

Anthropic grants some Claude accounts limit resets to spend later (the Opus 5.5
launch gave Team, Pro and Max one each). A credential holding one says so on
its limits title, on every frontend: `LIMITS · 1 reset banked, expires in 2d 8h`
for a provider with one credential, the same marker after `active` on a
credential's group. TokenGauge only shows them; spending one is claude.ai's,
or remuda's page's.

- The usage request asks for them with `?cedar_ember=1`. The answer is the
  `cedar_ember` block: `eligible`, `at_limit`, and `grants[]` with
  `resets_left`, `paused`, `usable_now`, `use_requires_limit`, `ends_at`.
- The endpoint lists grants only to a client it takes for Claude Code's CLI:
  `USER_AGENT` is `claude-cli/<version> (external, cli)`. Any other agent gets
  `eligible: false, ineligible_reason: "surface"` and no grants.
- A grant counts when the block is eligible and the grant has at least one
  whole reset left (`resets_left`), is not paused, is `usable_now`, and either needs no limit hit
  (`use_requires_limit`) or the account is `at_limit`. Counts add up across
  grants (saturating), the expiry is the soonest `ends_at`, and a grant that
  does not parse is skipped without hiding the others.
- Usable grants land on the payload as `usage.bankedResets: {available,
  expiresAt}`, omitted when there are none. remuda reads it
  ([../architecture/credentials.md](../architecture/credentials.md)).
- The marker is resolved in `panel.rs` when the row renders, so the countdown
  moves between fetches. Once the soonest expiry passes the marker goes, since
  the count no longer says how many are left, and `rolled_over` makes the
  snapshot stale at that moment so the next fetch brings the count back. It is a title marker rather than a row because a reset clears
  every window in the section, and it needs no new section kind.

## Sources

- `crates/tokengauge-core/src/claude.rs` (`USAGE_URL`, `USER_AGENT`, `CedarEmber`, `banked_resets`)
- `crates/tokengauge-core/src/payload.rs` (`BankedResets`)
- `crates/tokengauge-core/src/rows.rs` (`ProviderRow.banked_resets`)
- `crates/tokengauge-core/src/panel.rs` (`banked_marker`)
