# Fleet sync

One set of figures across every machine you code on: the cost, token and burn
rows cover the whole fleet, with a TOKENS BY DEVICE section showing where the
spend came from. No service, no account: each machine writes one encrypted
object only it ever writes, into a folder your sync tool handles or an
S3-compatible bucket. Mechanism: [../architecture/sync.md](../architecture/sync.md);
decisions [0001 and 0002](../decisions.md#fleet-sync).

## Setup is built once

Settings panes are the highest-drift surface and this one handles S3 keys and a
fleet key, so setup is not reimplemented per desktop:

1. **Core commands**: `--sync-init` (generate and print the key), `--sync-join -`
   (key from stdin; an argument lands in shell history and `/proc/<pid>/cmdline`),
   `--sync-force`, `--sync-status [--json]`, `--sync-test` (probe round trip),
   `--sync-forget <device>`, `--sync-adopt <device>` (fold a reinstalled
   machine's old entry into this one, see
   [../architecture/sync.md](../architecture/sync.md#adopting-a-reinstalled-machine)).
2. **The TUI sync screen** (`S`, `sync_view.rs`): `e` enable, `d` folder, `x`
   switch to S3, `g` generate, `j` join, `t` test, live device list. The TUI is
   exempt from layout parity, so it hosts the multi-step flow.
3. **`tokengauge --sync-setup`** opens that screen in a terminal
   (`launch::tui_command_with`). Each desktop frontend's "Set up sync" button
   (Plasma settings, GNOME header, `y` in Omarchy settings, the tray menu) spawns
   it. The button is chrome, so it can land one frontend at a time.

Setup asks whether `~/.claude/projects` is itself synced; if so, turn that provider
off in `[sync.providers]` (hazard 1 in the architecture doc).

## Status is content

What a user reads comes from `panel.rs`, so every surface shows the fleet number
with no frontend code:

- Cost and token sections show fleet totals for the providers that sync, session
  cost and burn rate included (the 5h window is the active credential's, the same
  on every machine signed into it). A fleet whose machines are signed into
  different credentials of one provider is the case this gets wrong: transcripts
  carry no credential to split by.
- `tokens_by_device` (`Bars`, month-to-date to match `tokens_by_model`): a row per
  device, tokens as value, money in the badge, share as the bar, relative age in
  the suffix when behind. Appears as soon as sync is on, even with one device
  ("waiting for another device"). Presence is the indicator.
- **Error-first**: the COST section ends with a `Sync` row (device count, a
  headline badge, the detail as suffix and tooltip, from `SyncNote`) that leads
  with a transport failure, an unreadable peer (foreign `key_id`, future schema)
  or a stale peer as `warn` / `critical`, because a total that is quietly too low
  is worse than one visibly missing. A transcript root that exists but cannot
  be read is reported there too, and every provider whose read comes in short
  of this device's record keeps that record
  ([../architecture/sync.md](../architecture/sync.md#the-fleet-store-is-the-durable-record)). A device whose `coversFromHour` starts after
  the period gets a dim `partial` badge in `tokens_by_device`.
- A transport failure never blanks totals; it ages them: `warn` past three pull
  intervals, `critical` past 24 hours. Session and burn rows get their own `warn`
  when a contributing device is stale, but keep it in, so the number does not
  jump when a laptop wakes.
- The `sync` object on `--json` (enabled, transport, per-device id, label,
  hostname, last update, partial, last error) backs `--sync-status`.

## Who takes part

Claude, Codex, Kimi and Grok (the natively read providers). GLM, OpenRouter,
opencode Go and Cursor have no per-call events and cannot sync yet.

## Sources

- `crates/tokengauge-core/src/sync/report.rs`, `crates/tokengauge-core/src/panel.rs` (`tokens_by_device`, the Sync row)
- `crates/tokengauge-core/src/sync/run.rs` (`refresh`, `unreadable_error`), `crates/tokengauge-core/src/cost/mod.rs` (`Roots::unreadable`)
- `crates/tokengauge-waybar/src/sync_cli.rs`, `crates/tokengauge-tui/src/sync_view.rs`, `crates/tokengauge-core/src/launch.rs`
- `crates/tokengauge-core/src/sync/config.rs` (`[sync]`, `[sync.providers]`, `[sync.dir]`, `[sync.s3]`)
