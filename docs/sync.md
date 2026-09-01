# Fleet sync

Goal: one panel that shows the tokens and money spent across every machine the
user drives a coding CLI from.

Status: built. `crates/tokengauge-core/src/sync/` holds the model, the envelope,
the transports and the cycle; `panel.rs` renders it; the TUI's sync screen sets
it up. This document is the design and the reasoning behind it - see the README
for how to use it.

## 1. Only costs are per-machine

The panel already mixes two kinds of number, and they need opposite treatment.

| Data | Where it comes from | Scope |
| ---- | ------------------- | ----- |
| Rate windows, `credits`, plan label | the provider's API, for the account | **already fleet-wide** |
| Cost and tokens (`CostInfo`, `cost/`) | transcripts on the local disk | **per-machine** |

A 5h window at 62% is 62% on every machine, because the provider counts the
account, not the host. Merging limits would double-count them. Merging is only
ever applied to the cost side, and the `limits` section stays exactly as it is.

That is why this is not a snapshot sync. `tokengauge-usage.json` holds provider
payloads, error strings and account detail, and none of it should leave the
machine. The synced document is a purpose-built record that carries token counts
and nothing else, so a leak has to be written on purpose rather than inherited.

## 2. The unit is an hourly token bucket

Key `(utc_hour, provider, model)`, value a `TokenCounts` (the five fields
already in `cost::TokenCounts`).

**Hourly, in UTC, because the reader owns the calendar.** Each machine converts a
bucket's hour into *its own* local day, so a laptop in Paris and a desktop in
Montreal each see a "today" that matches their own panel. Daily aggregates would
have forced one machine's midnight on the other. The hour also keeps the 5h
session figure and the burn rate computable from peer data, which a daily
rollup would destroy. Known limit: timezones with a sub-hour offset (+05:45,
+09:30) misplace at most one hour of tokens across a day boundary. The bucket
width is a constant if that ever matters.

**Tokens, never dollars.** Money is tokens times the reader's price table.
Shipping USD would let a peer's stale `prices.json` poison the fleet total and
would make the same day cost two different amounts on two machines. This is the
rule `scripts/make-cost-fixture.py` already learned: compare token counts, never
days or money.

**Granularity is a field, not an assumption.** A bucket carries `g: "hour"` from
v1. A provider whose cost comes from ccusage has no usage events behind it - no
per-call timestamps, day boundaries decided by ccusage's timezone, model
breakdown already aggregated - so it cannot produce hour buckets and does not
sync in v1. Its `[sync.providers]` entry is accepted and ignored rather than
failing config load, it simply grows no `tokens_by_device` section, and
`--doctor` names it. `g: "day"` is reserved for the degraded contribution that
would let a Kimi or Grok plan take part, and that gap should not stay open long:
those are exactly the users a fleet view is worth most to.

**A peer bucket becomes a synthetic `UsageEvent`** (`at` = start of the hour,
`date` = that instant in local time, tokens as recorded). `build_report` then
needs no changes at all, and the invariant in CLAUDE.md holds: the peer set is
simply one more reader, and a new reader is all a new source ever costs.

## 2b. The fleet store is the durable record

A contribution cannot be regenerated from transcripts, because
`cost::window_start` reads back only to the start of the current month. Rebuilt
every cycle, it would **forget its own history on the first of each month**, and
asymmetrically: peers append-merge and keep your July, while you lose it.

So the durable thing is a local **fleet store** at
`<cache_file parent>/tokengauge-fleet.json`, a table keyed by `(device, hour,
provider, model)` covering **every device including this one**. Transcripts and
peer contributions are both merely inputs to it, which makes self and peer the
same code path.

The rule that keeps a poll from double-counting: a transcript re-read
**replaces** every self bucket inside `[window_start, now]` and leaves
everything older untouched.

It is a separate file from the snapshot on purpose. The snapshot is rewritten
wholesale on every fetch; this must not be.

**Store retention and wire retention are different numbers.** The store keeps
400 days (a constant, not a knob) and prunes only beyond that: the data is tiny
and unrecoverable once a CLI rotates its transcripts away. The contribution
stays capped at `retention_days` on the wire, because it is re-uploaded on every
change. A device joining a fleet late therefore sees 35 days of everyone else's
history, which `coversFromHour` reports honestly rather than silently
under-counting.

Format is plain JSON with the short keys above, roughly 2 MB at full retention.
The store is read on the **fetch path only** - `--json` serves the snapshot - so
that parse happens per fetch, not per frontend poll. Hourly throughout: rolling
older data up to days would invent a second unit, and one unit is exactly the
property that leaves `build_report` untouched.

## 3. The contribution document

One file per device, written only by that device. Single-writer files are what
removes conflict resolution from the design entirely - there is no last-writer
race because no two writers ever touch the same object.

```
<root>/v1/<HMAC-SHA256(sync_key, device_id) as 32 hex>.tgsync
```

The object name is keyed, not the raw `DeviceIdentity::machine_id`, so the
folder or bucket holder cannot link an object to a machine or carry a name
across fleets. The real device id travels inside the encrypted body.
`--sync-forget` still resolves a name locally, because the local machine holds
the key.

What this does **not** hide, and the docs have to say so rather than let
"encrypted" imply it:

- **How many machines you have.** There is exactly one object per device, so
  counting objects counts the fleet. A keyed name hides *which* machine, not
  *how many*.
- **When you work.** Write timing is visible, and no naming scheme changes that.

Plaintext payload, before compression and encryption:

```json
{
  "schemaVersion": 1,
  "device": { "id": "9f3c...", "hostname": "boreas", "label": "desktop", "os": "linux" },
  "writtenAtMs": 1756123456789,
  "tzOffsetMinutes": 120,
  "coversFromHour": "2026-07-21T00",
  "providers": ["claude", "codex"],
  "buckets": [
    { "h": "2026-08-25T14", "p": "claude", "m": "claude-opus-5", "g": "hour",
      "i": 812, "o": 3122, "cw5": 15300, "cw1h": 0, "cr": 981233 }
  ],
  "days": [ { "d": "2026-08-25", "n": 812, "x": "5a1e9f3c00b47d21" } ]
}
```

- `coversFromHour` is the retention floor. Absent buckets before it are *not
  covered* rather than *zero*, which is what lets the panel mark a month-to-date
  total as partial instead of quietly under-reporting it.
- `days[].n` and `days[].x` are an event count and an XOR-fold of the existing
  `cost::dedup_key` values for that day. That key is a SHA-256 truncation
  rather than `DefaultHasher`, whose output Rust does not promise to keep
  stable across releases: two machines on different toolchains would never
  produce a matching fingerprint, and the check below would go quietly dead. They exist only to catch a shared
  transcript tree - see hazard 1.
- `writtenAtMs` drives freshness display. It is never used to order a merge,
  because nothing needs ordering.

Size: a heavy week runs around 900 non-empty buckets, roughly 90 KB of JSON;
35 days of retention stays under half a megabyte, and gzip takes about a fifth
of that.

## 4. Envelope and key

Encrypted from the first release, because the base transport is a folder inside
someone else's sync service.

```
"TGSYNC1" | alg u8 | comp u8 | key_id [4] | nonce [24] | ciphertext
```

- `alg = 1` is XChaCha20-Poly1305 (`chacha20poly1305`, pure Rust, no C, no
  runtime). `comp = 1` is gzip.
- `key_id` is the first four bytes of `SHA256("tokengauge.sync.key-id.v1" || key)`
  so a file for a different key is *skipped by name*, not reported as a
  corrupt decrypt.
- AAD is the header bytes plus the `device_id` from the filename, which binds a
  file to its name: a peer object renamed over another device's cannot be
  accepted.
- Nonce is 24 random bytes per write. XChaCha's nonce space makes random safe
  without a counter to persist.

The fleet key is 32 random bytes at `<state_dir>/tokengauge-sync-key`, mode
0600 **on unix only** - elsewhere it inherits the directory's ACL, which on
Windows means the user profile's, and no explicit restriction is applied.
Printed as `tgsync1<base32>`. `--sync-init` generates it, `--sync-join
tgsync1...` installs it on the next machine. No passphrase and no KDF: the key
is copied between the user's own machines the way a Syncthing device id is.

What this buys, stated honestly: the bucket or folder holder cannot read token
counts, model names or hostnames. It is not multi-user, there is no per-device
revocation (rotating means re-keying every machine), and possession of the key
is the only authentication. That is the right level for one person's fleet.

## 5. Transports

```rust
pub trait SyncTransport {
    fn put(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn list(&self) -> Result<Vec<PeerEntry>>;                    // name, version tag, size
    fn get(&self, entry: &PeerEntry) -> Result<Option<Vec<u8>>>; // None = unchanged
    fn delete(&self, name: &str) -> Result<()>;
}
```

**`dir`** (default): `put` through the existing atomic-write helper, `list` from
`read_dir`, unchanged detected by mtime and size. Covers Syncthing, Dropbox,
Nextcloud, iCloud Drive, rclone, a NAS mount. No new dependency, and the user's
existing tool handles reachability, retries and NAT.

**`s3`**: PUT/GET/LIST over `reqwest::blocking`, which core already links, with
SigV4 signed by hand. Works against S3, Cloudflare R2, Backblaze B2's
S3 endpoint, MinIO and Garage. `aws-sdk-s3` is rejected deliberately: it pulls
tokio and dozens of crates into a binary with no async runtime, to sign three
verbs. SigV4 for those verbs is about 150 lines and tests offline against AWS's
published vectors. Adds `hmac`, which pairs with the `sha2` already in the tree.
Peers are re-read with `If-None-Match`, so a poll that finds nothing new costs
one 304.

Both are configured, one is active. Fan-out to several transports at once
doubles the failure modes for no gain and is a non-goal.

```toml
[sync]
enabled = true
transport = "dir"          # "dir" | "s3"
label = "desktop"          # optional, defaults to hostname
retention_days = 35        # days of buckets a contribution carries
peer_max_age_days = 30     # silent this long, reported as quiet

# Which providers take part. Default: every enabled provider. Turn one off when
# that provider's transcript tree is itself synced between machines - hazard 1.
[sync.providers]
claude = false
codex = true

[sync.dir]
path = "~/Sync/tokengauge"

[sync.s3]
endpoint = "https://<account>.r2.cloudflarestorage.com"
region = "auto"
bucket = "tokengauge"
prefix = "fleet/"
# credentials default to AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
```

Credentials are never written into the snapshot and never logged; `--doctor`
redacts them.

## 6. When it runs

The daemon owns sync. Frontends never touch it - the existing rule holds: a
frontend never reads a credential, a cache file, or a provider endpoint. That
is why `--json` asks the daemon rather than fetching in the frontend's own
subprocess: credentials exported through `environment.d` reach the systemd unit
and not a panel spawned by the compositor, and a fetch that ran there wrote its
missing-credential error into the shared snapshot. On a machine with no daemon
the frontend's subprocess is the only process there is, so `AWS_ACCESS_KEY_ID`
and `AWS_SECRET_ACCESS_KEY` have to be visible to whatever launches the panel.

On each cycle that rebuilds costs, before `write_cache_full`:

1. Bucket the local events, publish if the content hash changed. A machine that
   has done nothing writes nothing, which keeps Syncthing and the bucket quiet.
2. Pull peers conditionally, merge into `<cache_file parent>/tokengauge-peers.json`.
3. Feed the fleet store into `build_report` as synthetic events, alongside the
   local events.

`--doctor`'s ccusage cross-check compares the **local slice** of the store and
reports the fleet total only as context. ccusage sees one machine's transcripts,
so a fleet-aware check would fail on every synced setup and destroy the signal
it exists for: catching a transcript format that drifted.

The merged `CostInfo` lands in the snapshot where the local one does now, so
**every frontend shows the fleet number without a line of frontend code**. This
is the whole payoff of the `--json` funnel.

`write_cache_full` already rewrites `tokengauge-revision`, so a peer's new data
reaches Quickshell's `FileView`, GNOME's `Gio.FileMonitor` and the Plasma
applet's `--wait-change` by the path that already exists.

Failure is never fatal. A transport error leaves the last merged peer set in
place, surfaces in `--doctor` and in a `sync` object on `--json`, and the panel
renders. The local peer cache means a cold start with the network down still
shows the fleet.

## 7. What the user sees

Split the surface in two. What a user *reads* comes from `panel.rs` and is
identical on all five frontends. What a user *configures* is built once, in the
TUI, and reached from anywhere.

### Status is content, so it has no per-frontend cost

Cost and token sections show fleet totals for the providers that sync,
**session cost and burn rate included**: the 5h window is account-scoped, so a
session figure counting one machine answers a question nobody asked. Merging
them from hour buckets costs at most one partial hour at the window edge, which
is less than the error already accepted from a peer polling every ten minutes.

One new section, `tokens_by_device`, `SectionKind::Bars`, **month-to-date** to
match the `tokens_by_model` section directly above it - a today-scoped bar chart
would be single-bar most mornings, which is the worst first impression right
after setup. A row per device, tokens as the value, money in the badge, bar
fraction is that device's share, `suffix` carries relative age when a device is
behind. Existing kind, so
`every_panel_frontend_handles_every_section_kind` stays green and no frontend is
touched.

**Presence is the indicator.** `panel_spec` runs per `ProviderRow`, so the
section appears on exactly the providers that are fleet-merged. A provider with
sync off looks the way it does today, which is what makes a mixed per-provider
setup readable without inventing a marker for it.

It appears as soon as sync is enabled, **including with one device** - "waiting
for another device" is the state right after setup, and that is precisely the
moment a user needs to see something.

**Error-first.** A transport failure, a peer file that cannot be read (foreign
`key_id`, future schema), or a peer gone stale becomes a `warn` or `critical`
row in the same section. Configured-but-not-working is the dangerous state: it
silently under-reports rather than breaking, and a total that is quietly too low
is worse than one that is visibly missing.

A device whose `coversFromHour` starts after the period shown gets a dim
`partial` badge, so a monthly total says so when a machine has been away longer
than its retention.

### Setup is built once

No settings pane per desktop. Settings panes are the highest-drift surface in
the project, every toolkit disagrees about input widgets, and this particular
pane handles S3 access keys and a fleet key. Five implementations of a secret
input is five chances to log one into a journal.

Instead, three layers:

1. **Core commands**, scriptable and testable: `--sync-init`, `--sync-join
   <key>`, `--sync-status`, `--sync-test` (publish and read back a probe
   object), `--sync-forget <device>`.
2. **A TUI sync screen** over those commands: transport picker, key generate and
   paste, round-trip test, live device list. The TUI is exempt from layout
   parity, which makes it the right home for an interactive multi-step flow.
3. **`tokengauge --sync-setup`**, which opens that screen in a terminal. The
   terminal discovery already exists as `tui_launch_command()` in the waybar
   crate (`[waybar] tui_command`, then `$TERMINAL`, then a list of common
   terminals); moving it into core makes the flag work everywhere.

A frontend's "Set up sync" button is then a spawn of a command it already knows
how to run - every frontend already shells out to `tokengauge --json`. No
terminal knowledge in QML or JS, no secret handling outside core. The button is
chrome, so it can land one frontend at a time without breaking the parity rule,
because nothing a user reads differs while it is missing.

Windows: the flag opens a console window. The tray is already an eframe GUI, so
if that reads badly it can grow a native dialog over the same core commands
without changing anything below it.

### Observability

One struct, three surfaces: the `sync` object in `--json`, `--sync-status`
printing it as a table, `--sync-status --json` dumping it verbatim.

A transport failure never blanks the totals - the store is durable, and blanking
would be a lie in the other direction. It ages instead: `warn` once the last
successful pull is older than three pull intervals, `critical` past 24 hours.

Session and burn rows carry their own `warn` badge when any contributing
device's newest bucket is older than three pull intervals. They stay included
rather than being dropped: excluding a stale device would make the number jump
the moment a laptop wakes, and a jumpy burn rate is unreadable.

`--json` still carries a `sync` object (enabled, transport, per-device id,
label, hostname, last update, partial flag, last error) for a frontend that
later wants a header icon rather than a panel row.

## 8. Hazards

1. **A shared transcript tree double-counts.** If `~/.claude/projects` is itself
   synced, two devices read the same events and the fleet total doubles. The
   per-day `n` and XOR digest catch the full-overlap case: identical digests on
   the same day mean the same event set, the day is kept from the device with
   the smaller id (both sides reach that answer without coordinating) and
   `--doctor` names the pair. Partial overlap is not detectable this cheaply and
   is documented as unsupported.

   The escape hatch is `[sync.providers]`: someone who syncs `~/.claude/projects`
   turns claude off and keeps codex, instead of giving up sync entirely. Setup
   asks the question directly rather than leaving it in a doc nobody opens.
2. **Clock skew is a non-issue by construction.** Buckets come from timestamps
   inside the transcripts, not from the writer's wall clock, and no merge step
   orders anything by time.
3. **A peer model with no local price** counts its tokens and no money.
   `build_report` already tracks `unpriced`; `--doctor` should attribute it to
   the device that used it.
4. **Retention truncation.** The local peer cache is append-merge keyed by
   `(device, hour)` and ages out on the *local* retention rule, not the peer's.
   Otherwise a machine that vanishes for two months would take its history out
   of the fleet with it, and this file is state, not cache.
5. **Re-keying starts a fleet rather than rotating one.** A store built under a
   different key keeps only this device: the old peers' objects are sealed under
   a key this machine no longer holds, so they would be reported foreign every
   cycle and their rows would claim a fleet it has left. Retired key ids are
   remembered so our own old objects are passed over in silence rather than
   reported, and the object this device published under the old key is deleted
   rather than left as litter nobody can read. Peers republish on their next
   cycle, so the real cost is peer history older than `retention_days`.
6. **A retired machine** is reported as quiet after `peer_max_age_days` and
   keeps contributing to historical days, because those days really did happen.
   It leaves the by-device rows on its own once it has no tokens in the period
   shown, which needs no rule. `--sync-forget <device>` deletes the object.
7. **A cloned machine id** (VM template, restored image) shows up as one device
   id publishing under two hostnames. `--doctor` warns. The id is derived from
   the system's machine id, so the cure is to give the clone its own:
   empty `/etc/machine-id` and run `systemd-machine-id-setup`, or, where there
   is no system id to derive from, delete `tokengauge-device-id` beside the
   snapshot. The next cycle publishes under a new id; `--sync-forget <device>`
   drops the object the two shared.
8. **Asymmetric per-provider config.** If one machine syncs codex and another
   does not, codex totals silently exclude the second machine. The by-device
   section shows who contributed, and `--doctor` names a device publishing a
   narrower provider set than the local one - usually a config the user forgot
   to mirror.
9. **Orphaned objects.** A device that regenerates its id leaves an object
   nobody claims. Nothing is ever auto-deleted: a machine can legitimately be
   off for a season, and a few stale kilobytes cost less than one wrongly
   deleted history. `--doctor` lists objects that have not moved in longer than
   `peer_max_age_days` as candidates, `--sync-forget` removes one on request.
10. **A lost machine cannot be revoked**, only out-run by re-keying every other
   device. See ADR 0002; the docs must say it rather than let "encrypted" imply
   otherwise.
11. **A future schema** is skipped with a named reason. Never a parse crash.

## 9. Non-goals

Merging limits or credits (wrong by construction). Team or multi-user fleets.
Real-time push - this answers "how much have I spent", not "what is running
now", and store-and-forward is the right shape for machines that are rarely
awake together. P2P and LAN discovery, ruled out by the same fleet shape.
Several transports at once. A settings pane per desktop. A hosted service, which
earns its place only for a fleet that can share neither a folder nor a bucket.

## 10. Slices

1. `sync::model` - the document, bucketing, synthetic events, merge, local peer
   cache. Pure and testable with no I/O, and `build_report` unchanged.
2. `sync::crypto` - envelope, key file, `--sync-init` / `--sync-join`.
3. `sync::transport::dir`, wiring into the fetch cycle, `--json` `sync` object,
   `--doctor` checks.
4. `panel.rs` `tokens_by_device`, including the waiting and error states. All
   five frontends inherit it.
5. `tui_launch_command()` moves to core; `--sync-setup`; the TUI sync screen.
6. `sync::transport::s3` - SigV4, conditional GET, R2/B2/MinIO notes.
7. A "Set up sync" button per frontend, one at a time. Chrome, not parity.
8. README section, CHANGELOG `[Unreleased]`, and the setup flow asking the
   synced-transcript-tree question out loud.

Decisions recorded in `docs/adr/0001-fleet-sync-shape.md` and
`docs/adr/0002-symmetric-fleet-key.md`. Vocabulary in `CONTEXT.md`.

Tests that have to exist: events -> buckets -> synthetic events -> report equals
the direct report; the same bucket set read at +02:00 and -07:00 lands in the
expected local days; overlap detection fires on identical day digests; a
tampered envelope byte fails authentication and a foreign `key_id` is skipped by
name; SigV4 matches AWS's published vectors; the merge tests run against a fake
transport.
