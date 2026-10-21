# Fleet sync (architecture)

How tokens and money spent on every machine reach one panel: the fleet store,
the contribution document, the envelope, the transports and the cycle. The user
view (setup, the by-device section, what crosses the wire) is
[../features/fleet-sync.md](../features/fleet-sync.md); the decisions are
[0001 and 0002 in ../decisions.md](../decisions.md#fleet-sync).

Status: built. `crates/tokengauge-core/src/sync/` holds the model, the envelope,
the transports and the cycle; `panel.rs` renders it; the TUI's sync screen sets
it up.

## Only costs are per-machine

| Data | Where it comes from | Scope |
| ---- | ------------------- | ----- |
| Rate windows, `credits`, plan label | the provider's API, for the credential | **already fleet-wide** |
| Cost and tokens (`CostInfo`, `cost/`) | transcripts on the local disk | **per-machine** |

A 5h window at 62% is 62% on every machine, because the provider counts the
credential, not the host. Merging limits would double-count them, so merging
applies to the cost side only and the `limits` section is untouched.

That is why this is not a snapshot sync. `tokengauge-usage.json` holds provider
payloads, error strings and account detail, none of which should leave the
machine. The synced document is purpose-built, carries token counts and nothing
else, so a leak has to be written on purpose rather than inherited.

## The unit is an hourly token bucket

Key `(utc_hour, provider, model)`, value a `TokenCounts` (the five fields of
`cost::TokenCounts`).

- **Hourly, in UTC, because the reader owns the calendar.** Each machine converts
  a bucket's hour into its own local day, so Paris and Montreal each see a
  "today" that matches their own panel. The hour also keeps the 5h session figure
  and the burn rate computable from peer data. Known limit: sub-hour offsets
  (+05:45, +09:30) misplace at most one hour of tokens across a day boundary.
- **Tokens, never dollars.** Money is tokens times the reader's price table (see
  [cost.md](cost.md)).
- **Granularity is a field.** A bucket carries `g: "hour"`; history rolls buckets
  older than `HOURLY_RETENTION_DAYS` (35) up to `g: "day"` locally (see
  [../features/spend-history.md](../features/spend-history.md)), but day buckets
  are not sent on a regular cycle: the rollup floor sits at or above
  `WIRE_RETENTION_DAYS`. The one exception is an adoption's wide publish (see
  [Adopting a reinstalled machine](#adopting-a-reinstalled-machine)). A
  ccusage-sourced provider has no per-call events, so it cannot produce hour
  buckets and does not sync: its `[sync.providers]` entry is accepted and
  ignored, it grows no `tokens_by_device` section, and `--doctor` names it. A
  degraded day-granularity contribution is the reserved way to let it in
  (tracked in `TODO.md`).
- **A peer bucket becomes a synthetic `UsageEvent`** (`at` = start of the hour,
  `date` = that instant in local time). `build_report` needs no changes: the peer
  set is one more reader.

## The fleet store is the durable record

A contribution cannot be regenerated from transcripts, because
`cost::window_start` reads back only to the start of the current month. Rebuilt
every cycle it would forget its own history on the first of each month, while
peers kept it.

So the durable thing is the local **fleet store**,
`<cache_file parent>/tokengauge-fleet.json`, keyed by `(device, hour, provider,
model)` and covering every device including this one. Transcripts and peer
contributions are both inputs, which makes self and peer one code path. It is
maintained on every fetch whether or not sync is on (the gate is in
`fetch.rs::native_costs`); sync gates only the cycle.

- A transcript re-read **replaces** self buckets provider by provider, from that
  provider's oldest event in the read window (`FleetStore::upsert_local`), and
  leaves older ones untouched, so a poll never double-counts. The deep re-read
  (`FleetStore::backfill_local`) does the same from its 400-day reach. A floor
  that lands inside a rolled-up day moves to the next day. A day no transcript
  mentions is unknown, not zero, and may hold rotated-away or adopted history,
  so `synthetic_events` counts the local buckets the read does not cover.
- **One writer at a time.** Every writer loads the whole store and saves it
  back, so `refresh`, `backfill`, `forget` and `adopt` hold
  `tokengauge-fleet.lock` (`store::lock`) from load to save. The lock blocks
  while another writer holds it (the CLI says it is waiting); a filesystem
  that cannot lock at all leaves every writer unlocked alike.
- Separate file from the snapshot on purpose: the snapshot is rewritten wholesale
  on every fetch; the store must not be.
- **Store retention and wire retention differ.** The store keeps
  `STORE_RETENTION_DAYS` (400) and prunes beyond that; the contribution carries
  `retention_days` (35 by default, `WIRE_RETENTION_DAYS`) because it is
  re-uploaded on every change. A device joining late sees 35 days of everyone
  else's history, which `coversFromHour` reports honestly.
- Read on the **fetch path only**; `--json` serves the snapshot, so the parse is
  per fetch, not per frontend poll.

## The contribution document

One file per device, written only by that device. Single-writer files remove
conflict resolution entirely.

```
<root>/v1/<HMAC-SHA256(sync_key, device_id) as 32 hex>.tgsync
```

The object name is keyed, not the raw machine id, so the storage holder cannot
link an object to a machine or carry a name across fleets. The real device id
travels inside the encrypted body; `--sync-forget` resolves a name locally
because this machine holds the key. Not hidden: **how many machines** (one object
each) and **when you work** (write timing).

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
  "days": [ { "d": "2026-08-25", "n": 812, "x": "5a1e9f3c00b47d21" } ],
  "adopted": [ { "id": "dd8008fc...", "atMs": 1792512000000 } ]
}
```

- `coversFromHour` is the retention floor: absent buckets before it are *not
  covered*, not zero, which lets the panel mark a total as partial.
- `days[].n` / `days[].x` are an event count and an XOR-fold of `cost::dedup_key`
  values for the day, used only to catch a shared transcript tree (hazard 1).
  The key is a SHA-256 truncation, not `DefaultHasher`, whose output Rust does
  not keep stable across releases.
- `writtenAtMs` drives freshness display, never merge order: nothing needs
  ordering.
- `adopted` lists the devices this one took over with `--sync-adopt`, each with
  the instant it did, omitted
  when empty (so the content hash of a device that adopted nothing is
  unchanged). Carried on every contribution, not only the first.
- Size: a heavy week is about 900 buckets, ~90 KB of JSON; 35 days stays under
  half a megabyte before gzip.

## Envelope and key

```
"TGSYNC1" | alg u8 | comp u8 | key_id [4] | nonce [24] | ciphertext
```

- `alg = 1` is XChaCha20-Poly1305 (`chacha20poly1305`, pure Rust); `comp = 1` is
  gzip.
- `key_id` is the first four bytes of `SHA256("tokengauge.sync.key-id.v1" || key)`,
  so a file under another key is skipped by name, not reported as a corrupt
  decrypt.
- AAD is the header plus the `device_id` from the filename, binding a file to its
  name: an object renamed over another device's is rejected.
- Nonce: 24 random bytes per write; XChaCha's nonce space makes random safe.

The key is 32 random bytes at `<state_dir>/tokengauge-sync-key`, mode 0600 on
Unix only (on Windows it inherits the profile directory's ACL), printed as
`tgsync1<base32>`. `--sync-init` generates it, `--sync-join -` installs it from
stdin. No passphrase, no KDF. What it buys: the storage holder cannot read
counts, model names or hostnames. Not multi-user, no per-device revocation, and
possession of the key is the only authentication (decision 0002).

## Transports

`SyncTransport` (`sync/transport.rs`): `put`, `list` (name, version tag, size),
`get` (`None` = unchanged), `delete`.

- **`dir`** (default): atomic write, `read_dir`, unchanged by mtime and size.
  Covers Syncthing, Dropbox, Nextcloud, iCloud Drive, rclone, a NAS mount; the
  user's tool handles reachability and NAT.
- **`s3`** (`sync/s3.rs`): PUT/GET/LIST over `reqwest::blocking` with SigV4
  signed by hand (about 150 lines, tested against AWS's published vectors).
  Works with S3, R2, B2, MinIO, Garage. `aws-sdk-s3` is rejected: it pulls tokio
  and dozens of crates to sign three verbs. Peers are re-read with
  `If-None-Match`, so an idle poll costs one 304.

One transport is active; fan-out is a non-goal. Config lives under `[sync]`
(`enabled`, `transport`, `label`, `retention_days` 35, `peer_max_age_days` 30),
`[sync.providers]`, `[sync.dir] path`, `[sync.s3]` (`endpoint`, `region`,
`bucket`, `prefix`), parsed in `sync/config.rs`;
S3 credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, never the
snapshot or a log, and `--doctor` redacts them.

## When it runs

The daemon owns sync wherever one runs. `--json` asks the daemon over the socket
rather than fetching in its own subprocess, because a credential exported through
`environment.d` reaches the systemd unit and not a panel spawned by the
compositor (see [snapshot.md](snapshot.md)). Without a daemon, the frontend's
subprocess is the only process and syncs itself, so the AWS variables must be
visible to whatever launches the panel.

Each cycle that rebuilds costs, before `write_cache_full`:

1. Bucket local events; publish only if the content hash changed, so an idle
   machine writes nothing.
2. Pull peers conditionally and merge them into the store, then fold every
   device a peer's `adopted` names (`settle_adoptions`) and drop the peers
   whose object vanished (`drop_vanished`, below).
3. Feed the store into `build_report` as synthetic events beside local ones.

The merged `CostInfo` lands in the snapshot where the local one did, so every
frontend shows the fleet number with no frontend code, and the revision file
carries a peer's new data to watchers by the existing path. `--doctor`'s ccusage
cross-check compares the **local slice** only, since ccusage sees one machine.

Failure is never fatal: a transport error keeps the last merged peer set,
surfaces in `--doctor`, the `sync` object on `--json` and the COST section's
`Sync` row, and the panel renders. The local peer cache means a cold start with
the network down still shows the fleet.

## Adopting a reinstalled machine

A reinstall writes a new `/etc/machine-id`, so the same machine joins the fleet
as a new device and its old entry freezes beside it with the same hostname. The
new device cannot rebuild the old history (`window_start`), so `--sync-forget`
on the old entry would delete days that exist nowhere else. `--sync-adopt
<device>` (`sync::adopt`, decision 0005) keeps one device with all of it:

1. **Resolve** the device like `--sync-forget` (label, hostname, id, or an id
   prefix of six digits or more), refusing this machine and an ambiguous name.
   The two entries share a hostname, so the error lists ids.
2. **Fold** (`FleetStore::adopt`): buckets and day digests strictly before the
   local slice's **oldest bucket** move over; from there the local data is
   authoritative. Not `covers_from`, which records how far a read looked and
   sits 400 days back after a backfill. Buckets at or after the edge, and a
   rolled-up day straddling it, are left out and counted (`Adoption.left_out`).
   The local `covers_from` takes the earlier of the two, and the slice records
   the id and the instant in `adopted`. Granularity is kept; `compact` and
   `prune` are idempotent over the result.
3. **Publish wide**: the local contribution with `STORE_RETENTION_DAYS` rather
   than `retention_days`, day buckets included, so peers receive the history and
   not only this machine. A failed publish deletes nothing.
4. **Save, then delete** the old object, so the only other copy never goes
   first. A failed delete is an error; every later cycle deletes the leftover
   object of a device this machine adopted (unless it has written since), so
   the next one finishes the job.

With `[sync] enabled = false` only the fold runs: a key and a transport left in
the config are not used.

The next regular cycle publishes at the usual retention, and a peer's `absorb`
keeps everything older than that contribution's `coversFromHour`.

**Peers.** A peer that missed the wide publish still holds its own copy of the
old device. Every cycle folds each device a slice's `adopted` names into that
slice by the same rule (`settle_adoptions`), so its copy moves rather than
counting twice. Never this machine, and never a device that has written since
the adoption's instant: that one is live (a wrong id, or an old id restored
from a backup), and folding it away every cycle could not be undone.

Separately, a peer whose object this target **used to list** and no longer
does is dropped (`drop_vanished`), which also makes `--sync-forget` reach the
other machines. Guards, each against losing peer history past the wire
retention:

- `objects` starts over when the transport target changes (`list_target`), so
  a new folder lacking a peer that has not moved yet drops nobody.
- A listing without this machine's own object drops nobody, and the object is
  put back on the next cycle (`published_hash` is cleared), or peers would drop
  an idle machine.
- Nobody is dropped when every known peer is gone at once: an unmounted share
  lists only the object this cycle just wrote.
- A `dir` entry that cannot be stat'ed is still listed.

## Hazards

1. **A shared transcript tree double-counts.** Identical per-day `n` and digest
   on two devices mean the same event set: the day is kept from the device with
   the smaller id and `--doctor` names the pair. Partial overlap is undetectable
   this cheaply and unsupported. Escape hatch: `[sync.providers] claude = false`.
2. **Clock skew is a non-issue**: buckets come from transcript timestamps and no
   merge orders by time.
3. **A peer model with no local price** counts tokens and no money.
4. **Retention truncation**: peer buckets age out on the local rule, not the
   peer's, so a machine gone for two months does not take its history with it.
5. **Re-keying starts a fleet rather than rotating one.** A store built under a
   different key keeps only this device; retired key ids are remembered so our
   own old objects are passed over silently, and the object published under the
   old key is deleted. The real cost is peer history older than `retention_days`.
6. **A retired machine** is reported quiet after `peer_max_age_days`, keeps its
   historical days, and leaves the by-device rows once it has no tokens in the
   period. `--sync-forget` deletes its object; every peer then drops it.
   **The same machine after a reinstall** is a new id: `--sync-adopt` folds the
   old entry's history into it instead.
7. **A cloned machine id** (VM template) shows as one id under two hostnames;
   `--doctor` warns. Cure: regenerate `/etc/machine-id`
   (`systemd-machine-id-setup`) or delete `tokengauge-device-id` beside the
   snapshot.
8. **Asymmetric per-provider config** silently excludes a machine from a
   provider's total; the by-device section shows who contributed and `--doctor`
   names a narrower provider set.
9. **Orphaned objects** are never auto-deleted; `--doctor` lists candidates older
   than `peer_max_age_days`, `--sync-forget` removes one.
10. **A lost machine cannot be revoked**, only out-run by re-keying (decision 0002).
11. **A future schema** is skipped with a named reason, never a parse crash.

## Non-goals

Merging limits or credits (wrong by construction). Team or multi-user fleets.
Real-time push. P2P and LAN discovery. Several transports at once. A settings
pane per desktop. A hosted service.

## Tests that pin it

Events -> buckets -> synthetic events -> report equals the direct report; one
bucket set read at +02:00 and -07:00 lands in the expected local days; overlap
detection fires on identical day digests; a tampered envelope byte fails and a
foreign `key_id` is skipped by name; SigV4 matches AWS's vectors; merge tests run
against a fake transport (`crates/tokengauge-core/tests/fleet_sync.rs`).

## Sources

- `crates/tokengauge-core/src/sync/{mod,contribution,fleet,hour,crypto,transport,s3,run,report,store,config}.rs` (`adopt`, `settle_adoptions`, `drop_vanished`, `store::lock`)
- `crates/tokengauge-core/src/fetch.rs` (`native_costs`), `crates/tokengauge-core/src/cost/mod.rs` (`window_start`, `dedup_key`)
- `crates/tokengauge-core/src/device.rs`, `crates/tokengauge-core/src/doctor.rs`
- `crates/tokengauge-waybar/src/sync_cli.rs`, `crates/tokengauge-tui/src/sync_view.rs`
- `crates/tokengauge-core/tests/fleet_sync.rs`
