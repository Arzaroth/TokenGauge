# Fleet sync is single-writer files carrying hourly token buckets

Showing one user's spend across their machines needs bytes to move between
machines that are rarely awake at the same time, so it has to be
store-and-forward. We chose to have each device publish **one file only it ever
writes**, into storage the user already has (a synced folder, or an S3-compatible
bucket), rather than run a service: single-writer objects remove conflict
resolution from the design entirely, and every hard part of a sync service
(hosting, auth, multi-tenancy, uptime) disappears with it. The file carries
**token counts bucketed by UTC hour, provider and model** rather than daily
totals or dollars.

## Considered options

- **A hosted or self-hosted relay.** Rejected for v1: it buys nothing a shared
  folder does not, and costs auth, TLS, uptime and a place to run it. It earns
  its place only for a fleet that can share neither a folder nor a bucket.
- **P2P over LAN or Tailscale.** Rejected on the fleet shape: the machines are
  on different networks and seldom awake together, so there is often no peer to
  talk to.
- **Daily totals on the wire.** Rejected because the day would be the *writer's*
  day. UTC hours let each reader bucket into its own local calendar, so two
  timezones each get a "today" that matches their own panel, and the 5h session
  window and burn rate stay computable from peer data.
- **Dollars on the wire.** Rejected because money is tokens times the reader's
  price table. Shipping USD lets a peer's stale `prices.json` poison the fleet
  total and makes the same day cost two different amounts on two machines.
  `scripts/make-cost-fixture.py` already learned this rule.

## Consequences

A peer bucket becomes a synthetic `UsageEvent`, so `build_report` needs no
changes and the peer set is simply one more reader.

A provider whose cost comes from ccusage has no usage events behind it and
therefore cannot sync. That excludes exactly the Kimi and Grok plans a fleet
view would be worth most to, until a day-granularity contribution fills it in.

Sub-hour timezone offsets (+05:45, +09:30) misplace at most one hour of tokens
across a day boundary.
