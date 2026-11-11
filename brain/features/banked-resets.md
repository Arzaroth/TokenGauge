# Banked resets

Anthropic grants some Claude accounts limit resets to spend later (the Opus 5.5
launch gave Team, Pro and Max one each). A credential holding one says so on
its limits title, on every frontend: `LIMITS · 1 Fable reset banked, expires in
2d 8h` for a provider with one credential, the same marker after `active` on a
credential's group. TokenGauge only shows them; spending one is claude.ai's,
or remuda's page's.

- The usage request asks for them with `?cedar_ember=1`. The answer is the
  `cedar_ember` block: `eligible`, `at_limit`, and `grants[]` with
  `resets_left`, `paused`, `usable_now`, `use_requires_limit`, `ends_at`,
  `clears` (the window keys it resets), `type` and `gives_back_weekly`.
- The endpoint lists grants only to a client it takes for Claude Code's CLI:
  `USER_AGENT` is `claude-cli/<version> (external, cli)`. Any other agent gets
  `eligible: false, ineligible_reason: "surface"` and no grants.
- A grant counts when the block is eligible and the grant has at least one
  whole reset left (`resets_left`), is not paused, is `usable_now`, and either needs no limit hit
  (`use_requires_limit`) or the account is `at_limit`. Counts add up across
  grants (saturating), the expiry is the soonest `ends_at`, and a grant that
  does not parse is skipped without hiding the others.
- Each grant has a reset kind, by Claude Code's rule in its order:
  `gives_back_weekly` makes it `fable`, whatever `type` says (the Fable launch
  grant says `full`); else `type` (`session`, `weekly`, `full`); else `clears`,
  `five_hour` alone being `session`, no `five_hour` being `weekly`, and both or
  neither `full`. A Fable reset clears `seven_day_overage_included`, the Fable
  limit, and hands this week's Fable use back to the weekly limit.
- Usable grants land on the payload as `usage.bankedResets: {available,
  expiresAt, kinds}`, omitted when there are none. `kinds` is
  `[{kind, count, expiresAt, clears, idle}]` in the order the grants came, one
  entry per kind, `clears` the windows every one of its grants clears (the
  intersection, since the provider picks which grant a spend uses), `idle`
  true when every one of its grants has `percent_used` at 0 for each window it
  resets, the state claude.ai greys out as "no usage to reset right now".
  Idle resets still count in `available`: they are held, just not worth
  spending yet.
  A kind this build does not know reads as `other` and draws the plain count,
  so a snapshot a newer build wrote still loads. A Fable reset lists
  `five_hour` beside `seven_day_overage_included` on an account with a 5-hour
  window (a Team Premium seat) and not on one without (a Max Ultra), so the
  kind is the same and what it clears is the account's. remuda reads it
  ([../architecture/credentials.md](../architecture/credentials.md)).
- Codex's reset credits are the same thing under another name:
  `wham/usage` carries `rate_limit_reset_credits: {available_count,
  applicable_available_count}`, and `available_count` becomes the same
  `bankedResets`. That block has no expiry and no kind, so the Codex marker
  names neither.
  `/wham/rate-limit-reset-credits` lists each credit with one, for one more
  request per credential, and nothing here asks it yet.
- The marker names each kind it holds (`1 Fable reset banked`, `1 Fable reset,
  1 weekly reset banked`) and says only `3 resets banked` when the count is not
  split, the split does not add up to it, or it holds a kind this build cannot
  name. An idle kind is marked `(nothing to reset)`, as claude.ai greys
  it out.
- The marker is resolved in `panel.rs` when the row renders, so the countdown
  moves between fetches. Once the soonest expiry passes the marker goes, since
  the count no longer says how many are left, and `rolled_over` makes the
  snapshot stale at that moment so the next fetch brings the count back. It is a title marker rather than a row because a reset acts on
  the windows of the section it heads, and it needs no new section kind.

## Sources

- `crates/tokengauge-core/src/claude.rs` (`USAGE_URL`, `USER_AGENT`, `CedarEmber`, `Grant::kind`, `banked_resets`)
- `crates/tokengauge-core/src/payload.rs` (`BankedResets`, `BankedKind`, `ResetKind`)
- `crates/tokengauge-core/src/rows.rs` (`ProviderRow.banked_resets`)
- `crates/tokengauge-core/src/panel.rs` (`banked_marker`)
- `crates/tokengauge-core/src/snapshot.rs` (`rolled_over`)
