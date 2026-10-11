# `--doctor`

A grouped diagnostic checklist: `tokengauge --doctor` (Linux, macOS) or
`tokengauge-tui --doctor` (any platform). Exit 0 when all pass, 1 otherwise.

| Group | Checks |
| --- | --- |
| Config | the config loads |
| Credentials | each provider's sign-in, **validated, not statted** (`provider_auth_status` runs the fetcher's source walk offline) |
| Credential store | the store (a missing one passes; refused or unreadable fails) and one validated line per stored credential (parse, digest, expiry, scope), naming the active one and every skipped entry |
| Cost source | native vs ccusage cross-check on the local slice, price table, unpriced models, price archive reach |
| Dependencies | ccusage runner; `notify-send` and `xdg-open` on Linux |
| Filesystem | resolved snapshot path, writable directory, fleet store size and reach |
| Providers | enabled list and a live fetch per provider |
| Bar wiring | waybar module wired (`tokengauge` on Linux only) |
| Fleet sync | key, per-device status, overlap pairs, cloned ids, narrower provider sets, orphan candidates, providers that cannot sync |
| Updates | installed version, update available |
| Desktop UIs | Plasma / GNOME / Omarchy payload versions (Linux only) |

The report lives in `tokengauge-core`, so it is the same everywhere; the
`tokengauge` binary adds the sections only it can answer (bar wiring, click
launcher, fleet sync). Linux-only checks are omitted elsewhere rather than
failed. Secrets (S3 keys, tokens) are redacted.

## Sources

- `crates/tokengauge-core/src/doctor.rs`, `crates/tokengauge-waybar/src/doctor.rs`
- `crates/tokengauge-core/src/providers.rs` (`provider_auth_status`), `crates/tokengauge-core/src/credentials.rs`
