# TokenGauge

One gauge for how much a developer has spent against their coding-CLI plans,
rendered identically across a bar widget, four desktop panels and a TUI.

## Language

### Usage and limits

**Provider**:
One coding-CLI vendor TokenGauge reads from, such as claude or codex. The unit
of enable/disable everywhere in config.
_Avoid_: backend, service, account

**Window**:
A provider-reported rate limit over a fixed span, with a percentage used and a
reset instant. Account-scoped, so it reads the same on every machine.
_Avoid_: quota, limit period

**Snapshot**:
The single state file every frontend renders from, holding provider payloads,
errors and costs as of the last fetch.
_Avoid_: cache

**Panel spec**:
The ordered sections and rows resolved once in `panel.rs`, carrying finished
display strings, fractions and tones. The abstraction that keeps five frontends
in agreement.
_Avoid_: layout, view model

**Tone**:
A semantic tier a row carries (good, warn, critical, dim, normal) that each
frontend maps onto its own palette.
_Avoid_: colour, severity

### Cost

**Usage event**:
One billed call, produced by every transcript reader and consumed by everything
downstream. Adding a source means adding a reader and nothing else.
_Avoid_: record, entry, sample

**Reader**:
A parser for one CLI's transcript tree that emits usage events.
_Avoid_: parser, importer, collector

### Fleet sync

**Fleet**:
The set of machines sharing one sync key, whose usage is summed into one panel.
_Avoid_: cluster, group, team

**Device**:
One machine in the fleet, identified by a derived id and a user-set label.
_Avoid_: host, node, client

**Peer**:
A device other than this one. Used only where the distinction changes
behaviour.
_Avoid_: remote, other

**Bucket**:
Tokens billed for one provider and model within one UTC hour. The unit that
crosses machines.
_Avoid_: sample, slice, aggregate

**Contribution**:
The encrypted document one device publishes, holding its own buckets and
nothing else.
_Avoid_: share, export, payload

**Fleet store**:
The local table of buckets keyed by device and hour, covering every device
including this one. The durable record; transcripts and contributions are its
inputs.
_Avoid_: peer cache, merge cache

**Sync key**:
The symmetric secret shared by every device in a fleet. Possession of it is
membership.
_Avoid_: token, password, secret
