# TokenGauge

## Frontend parity (hard rule)

TokenGauge ships one gauge across six surfaces. **A user-facing feature lands
on all of them, or it is not done.**

| Surface    | Where                                        | Draws the panel |
| ---------- | -------------------------------------------- | --------------- |
| Waybar     | `crates/tokengauge-waybar` (bar + tooltip)    | yes - the tooltip *is* waybar's panel |
| Plasma     | `plasma/org.tokengauge.plasmoid`              | yes |
| GNOME      | `gnome/tokengauge@arzaroth.github.io` (TypeScript, see below) | yes |
| Quickshell | `omarchy/arzaroth.tokengauge`                 | yes |
| Tray (Windows) | `crates/tokengauge-tray`                  | yes |
| TUI        | `crates/tokengauge-tui`                       | yes - exempt from layout parity only |

Shipping a feature on one frontend and leaving the rest "for later" is the
failure mode to avoid: the desktop frontends install separately from the binary,
so a gap there is invisible from the crate that grew the feature.

Data belongs in `tokengauge-core`; `tokengauge --json` is the single
snapshot every non-Rust frontend renders from. A frontend never reads a
credential, a cache file, or a provider endpoint itself.

### The panel spec is the abstraction

`crates/tokengauge-core/src/panel.rs` resolves the whole panel once:
`panel_spec(&ProviderRow)` returns an ordered `Vec<Section>`, each carrying a
`SectionKind` and rows whose display strings, fractions, tones and tooltips are
already computed. Rust frontends call it directly; QML and JS frontends read it
off the `panel` field of each row in `--json`.

**Section order and content are decided there, never in a frontend.** A
frontend implements exactly three primitives and loops:

| `SectionKind` | Shape |
| ------------- | ----- |
| `Meters` | label + value on one line, full-width bar under it, then `footnote` and the tinted `badge`. The limit gauges. |
| `Bars` | one line per row with the share bar filling the row behind the text. Tokens by day, tokens by model. |
| `Rows` | label, value, tinted `badge`, dim `suffix` on one line, no bar. The cost figures. |

Canonical sections, in order (`panel::SECTION_IDS`), each dropped when it has no
data: `status`, `limits`, `cost`, `tokens_by_day`, `tokens_by_model`,
`tokens_by_device`. `status` is how a stale row says why - the fetch error that
`apply_stale_fallback` drops rides on the payload as `stale_reason` and is
rendered as a section rather than as a per-frontend badge, because a `stale`
chip on its own is the same word whether the network blipped once or a
credential expired weeks ago.

Adding a section means editing `panel.rs` and nothing else. Adding a *kind*
means touching all six frontends - `panel::tests::every_panel_frontend_handles_every_section_kind`
reads each frontend's source and fails when one of them never mentions a kind,
which is the backstop for a rule no compiler enforces: `SectionKind` is a Rust
enum, a QML string and a TypeScript union, and none of the three makes a
frontend that never mentions a kind fail to build.

The TUI's exemption is *layout*, not content: it draws `tokens_by_day` as a bar
chart rather than a row list, and keeps its sidebar, gauges and keybindings, but
every string a user reads there is the spec's. It used to carry its own copies
of `Tone::for_pace` and `Tone::for_trend`, its own section labels and its own
money formatter, and all four had drifted from the spec by the time anyone
noticed.

What stays per-frontend is **chrome**, not content: the header, the update
banner, the provider selector, the settings pane and the input hints are
interactive and toolkit-shaped. Everything a user *reads* comes from `panel.rs`.

`Tone` is a semantic tier (`good` / `warn` / `critical` / `dim` / `normal`), not
a colour. Each frontend maps it onto its own palette - the Omarchy widget
deliberately collapses `good`/`warn` onto the bar foreground, because omarchy
themes carry a foreground and an urgent colour and nothing between them.

### Chrome that is still content

The refresh control is chrome - every toolkit draws its own - but what it says
on hover is not. `panel::refresh_hint` turns a row's `updated_iso` into the one
sentence all six read, carried to the JSON frontends as `refresh_hint` on each
row and asserted by `panel::tests::every_frontend_says_when_it_last_refreshed`.
It is resolved per render for the same reason a reset countdown is: the age in
it keeps moving after the fetch that wrote the payload. Waybar has no button to
hover, so its tooltip carries the sentence as a line.

The bar icon is the same shape of problem. Every toolkit draws its own - a
plasmoid compact representation, a St label, a `WidgetButton`, a tray icon -
but what it says on hover is content, and it was the last piece of content no
frontend agreed on: Plasma picked the lines out of `panel` in QML, the tray
re-derived `session_used` / `weekly_used` in Rust and named only those, and
GNOME and the Quickshell widget said nothing at all, which reads as broken
rather than deliberate. `panel::bar_tooltip` resolves it - every limit window
with its tier, then today's spend - carried to the JSON frontends as
`bar_tooltip` on each row and asserted by
`panel::tests::every_frontend_with_a_bar_icon_says_the_same_thing_on_hover`.
It is built off `panel_spec`, not off the row, so the summary can never name a
window the panel under it does not draw. Waybar and the TUI are absent from
that test because neither has an icon to hover: waybar's tooltip *is* the
panel, so summarising it would be the same figures twice on one surface.

The instant is the *payload's*, never the process's. The TUI header measured
`Instant::elapsed` since its own last fetch and so read "updated just now" over
a snapshot ten minutes old - a refresh that finds the snapshot fresh serves the
cache, and that is still a refresh as far as the process is concerned.

### The GNOME extension is compiled

It is TypeScript against `@girs/gnome-shell`, and the only frontend in the
repository that is not installable as it sits. `scripts/build.sh` compiles it
and assembles all three desktop payloads under `build/frontends/<payload>` -
the release archive's own layout, so one path serves both. The `.ts` sources
are dropped from the payload; the GSettings schemas stay XML, because the
compiled blob belongs on the machine that runs the extension and
`selvedge::frontend`'s `install_into` is what builds it.

`Frontend.compiled` is what makes that safe: `payload_in` looks under `build/`
for a compiled frontend and never falls back to its source directory, so
`--install-frontend gnome` and `--update` from a checkout refuse instead of
landing TypeScript in `~/.local/share/gnome-shell/extensions`, which the shell
loads as an error. Release archives are unaffected - they carry the compiled
extension and resolve through the archive branch as before.

The `--json` contract is declared once, in `gnome/*/panel.ts`, by hand: the
other side is a Rust struct and there is nothing to generate them from, which
is why the fields are named exactly as the JSON names them and only the fields
this frontend reads are declared. The four `panel::tests` that grep frontend
sources read the `.ts`, not the build output.

### Rows the spec drops

A window the provider does not report (`used: None`) and a `placeholder: true`
extra window are both omitted. Every panel used to disagree about this; now none
of them decide it.

## Snapshot, staleness, and how a change reaches a frontend

The snapshot lives at `$XDG_STATE_HOME/tokengauge/tokengauge-usage.json`
(`cache_file`), and every other state file is derived from its **parent**: the
daemon socket, the refresh sentinel, the selected provider, the notify state,
`tokengauge-prices.json`, `tokengauge-prices-missed.json`, and
`tokengauge-revision`. It is state, not cache: it
holds the only record of past days' tokens and costs.

`cache_is_stale()` in core is the single fetch-or-serve decision. A snapshot is
stale when it is missing, older than `refresh_secs`, **or** was written before a
provider that is enabled now was switched on - `CacheMeta.providers` records the
set each fetch ran with - **or** a window it reported has reset since it was
written, because those percentages describe a window that no longer exists. Age
alone was the old rule, and it is why enabling a provider used to do nothing for
ten minutes. The rollover test compares against the write, not against now
alone: a provider reporting an instant already past reports the same one on the
next fetch, and asking again on every render would never stop. `retain_enabled()`
still handles the other direction, filtering a provider switched off out of a
snapshot that is otherwise fine.

Every write goes through `write_cache_full`, which writes atomically and then
rewrites `tokengauge-revision`. Frontends watch that file (Quickshell `FileView`,
GNOME `Gio.FileMonitor`, and `--wait-change` for the Plasma applet, whose toolkit
has no watcher) and re-run `--json` when it moves. Their poll timers stay: with
no daemon running, a poll is what ages the snapshot out and triggers the next
fetch.

### Rendering is not fetching

A reset countdown is measured against the clock at the moment `panel.rs` builds
the row, not against the fetch - the instant it counts down to is absolute, so a
snapshot minutes old still yields the right countdown. What that costs is a
render: a frontend that rebuilds its rows only when it refetches shows the
countdown it last rebuilt with, which is how "Resets in 6m" survived next to a
dashboard saying 3 minutes.

So every frontend re-renders on a **short** cycle while it is on screen, and
that cycle has nothing to do with `refresh_secs`: Omarchy, Plasma and GNOME
re-run `--json` every 30s while the panel is open, the TUI and the tray rebuild
from the snapshot every 15s, and the daemon renders each socket snapshot request
rather than replaying the output it rendered at its last fetch. None of that
asks a provider anything - `cache_is_stale()` alone decides that, which is why a
render can be cheap and frequent while a fetch stays rare.

**Who does the fetch matters as much as when.** `--json` asks the daemon over
the socket and only fetches in-process when there is no daemon to ask, because a
frontend's subprocess inherits the *compositor's* environment while the daemon
inherits the systemd unit's - and `environment.d`, where an S3 sync credential
usually lives, reaches the second and not the first. A frontend that fetched
wrote its own missing-credential error into the snapshot every other frontend
reads. The other half of that rule is in the daemon: its wait between fetches
re-checks `cache_is_stale()` every 15s rather than sleeping `refresh_secs`
blind, because a window resetting mid-cycle makes the snapshot stale at an
instant no timer wakes for, and the frontend polling every 30s used to be the
only thing that noticed.

`--set-provider` fetches before it returns, because frontends run
`--set-provider && --json` in one subprocess and the `--json` has to see the new
provider. The daemon's SIGHUP reload then finds a snapshot that already covers
the new set and re-renders instead of fetching again.

## A credential is where the tool put it, which is not always a file

The Claude token used to be read from `~/.claude/.credentials.json` and nowhere
else. That broke twice for the same reason: Claude Code 2.1.x moved the token
into the macOS keychain and left the file a stub, and the Windows desktop app
delegates auth to the app over an IPC socket and writes a stub too. A present
file says nothing about whether it holds a token. `claude.rs` now reads
`TOKENGAUGE_CLAUDE_OAUTH_TOKEN`, then the file, then the OS credential store
(macOS keychain / Windows Credential Manager, via `keyring`, gated off Linux so
no dbus is pulled), and takes the first that is **usable, not merely present** -
a hollow file must never shadow a good keychain entry. An empty access token is
"not signed in - run `claude setup-token`", a distinct state from "expired",
because re-login does not repopulate a file the desktop app owns.

Two rules fall out of this and are easy to regress:

- `--doctor`'s Credentials check must **validate, not stat**. `provider_auth_status`
  for Claude runs the same source walk (no network) so a hollow file reads red,
  not green. A check that greenlights a credential the fetcher rejects is worse
  than no check.
- The credential reader and the transcript reader must agree on
  `CLAUDE_CONFIG_DIR`. They did not; the fix is `claude_config_dir()`. If you add
  a state file under `~/.claude`, route it through there.

The keychain / Credential Manager path is `cfg(any(windows, target_os =
"macos"))` and compiles away on Linux, so it is exercised only by the Windows CI
job and never on Mac - treat that path the way the tray crate is treated: build
it on the platform that has it, or it is unverified.

## Costs are read, not shelled out for

`crates/tokengauge-core/src/cost/` parses the transcripts the CLIs already
write - `~/.claude/projects`, `~/.codex/sessions`,
`~/.kimi-code/sessions/**/wire.jsonl` and `~/.grok/sessions/**/updates.jsonl` -
and rates them against LiteLLM's price table (`pricing.rs`: fetched, cached
beside the snapshot, with `prices.json` vendored in for a cold or offline
machine, regenerated by `scripts/make-prices.py`). The table is served for 24h
without asking, so a model released inside that window would be counted and
rated at nothing - a $0 that reads as a cheap day rather than as a gap.
`refetch_for_unpriced` makes tokens with no price the proof that the table is
behind, the way a window that has reset is proof the snapshot is, and buys one
download outside the freshness window. The unpriced set is recorded beside the
cache, so a model upstream will never carry - a local one, or a provider sold
under a namespace `vendor_prefixes` misses - cannot turn every fetch into a
re-download. Every reader produces the same unit, a `UsageEvent`, so **a new CLI
is one more reader and nothing else**: add a field to `cost::Roots`, call it from
`read_events_from`, and `build_report` does the rest.

The model in a transcript is the bare id the CLI was configured with, and LiteLLM
keys a model by the vendor selling it - `zai/glm-4.6`, `xai/grok-4`,
`moonshot/kimi-k2-thinking`. `pricing::price_candidates` walks from one to the
other, and `attribute_price_key` is its mirror, deciding what the table carries;
the two must agree, or a model is either unpriced or dead weight in the binary.
A provider whose models are sold under someone else's namespace needs an entry in
`vendor_prefixes` or it is counted and rated at zero - and that zero hides
itself, because the `auto` fallback only asks ccusage about providers the readers
found *nothing* for.

The token counts must match ccusage exactly - they come from the same files.
Three things assert it, at widening cost: `tests/cost_fixture.rs` diffs the
readers against a checked-in ccusage golden (no Node, no network, runs in CI),
`cost::tests::agrees_with_ccusage_on_real_transcripts` (`#[ignore]`d) does it
against the developer's own home directory, and `--doctor` does it at runtime on
a user's machine. If they drift, a transcript format changed and a reader missed
it.

Regenerate the fixture with `scripts/make-cost-fixture.py`. Two rules it learned
the hard way: **emit compact JSON** (ccusage prefilters lines with a string match
against compact separators, so pretty-printed input reads as an empty file), and
**compare token counts, never days or money** (days depend on the reader's
timezone, money on whichever price table each side fetched). The generator
refuses to write a fixture that has stopped covering the traps - verified by
reverting the dedup fix and watching it fail.

Five traps are load-bearing, each with a test named after it:

- A **streamed message is written repeatedly**, each record restating the same
  `(message.id, requestId)` with more `output_tokens`. A duplicate upgrades the
  event; keeping the first loses 46% of some models' daily output.
- Codex `total_token_usage` is **cumulative for the session**. Its delta is the
  billed unit - `last_token_usage` re-fires with the cumulative unchanged and
  summing it roughly doubles the total.
- Codex `cached_input_tokens` is **inside** `input_tokens`, where Anthropic
  reports cache reads beside them. Grok's `cachedReadTokens` and
  `cacheCreationTokens` are inside `inputTokens` the same way.
- A Kimi `usage.record` scoped to the **session** restates the running total of
  the turns beside it. Only `usageScope: "turn"` is a billed call.
- Grok's per-turn `usage` totals are the **sum of its `modelUsage` map**, so a
  reader taking both bills every turn twice.

ccusage is now the fallback and the cross-check, not the source. `cost_source`
picks: `auto` (default) reads natively and asks ccusage only about enabled
providers the readers found nothing for - a GLM plan driven from z.ai's own CLI
writes into no tree we parse, and a Kimi or Grok CLI on a format this build does
not recognise reads the same way.

`ProviderMeta.natively_read` says which providers have a reader behind them. It
gates two things: fleet sync, which buckets per-call events and so has nothing to
publish without one, and `--doctor`'s drift check, which excuses a provider only
ccusage can see. Flip it when a reader lands, or the reader ships without either.

## History is a second screen, and the store is not sync's

`crates/tokengauge-core/src/history.rs` resolves 30 days, 90 days and 12 months
of spend into finished strings, fractions and tones - the same contract as
`panel.rs`, but drawn on a **second screen** behind a toggle rather than in the
panel's scroll. A year of bars does not belong above the limit gauges.

Five frontends draw it, each with its own chart primitive. **Waybar draws none,
deliberately**: its tooltip is a hover surface that cannot have a second screen,
so a waybar user's history is the TUI that left-click has always opened.
`panel::tests::every_frontend_with_a_second_screen_draws_the_history_series`
records that decision and is the list to add waybar to if it is ever revisited.

The data is the fleet store, which is **no longer gated on `[sync] enabled`**.
It always was the only record of a day once a CLI rotates its transcript away;
building it only for sync users meant a machine that syncs with nobody had no
past. Sync now gates the cycle only - publishing, pulling, peer events, and the
`by_device` split whose presence is what says the figures cover more than this
machine. The gate that actually mattered was a level above `sync::refresh`, in
`fetch.rs::native_costs`.

Three invariants are easy to regress:

- **`HOURLY_RETENTION_DAYS` (35) must stay above the widest re-read (31) and at
  or above `WIRE_RETENTION_DAYS`.** `upsert_local` replaces buckets from `from`
  on, so a rolled-up bucket at or after that mark would be landed beside rather
  than replaced and the day would count twice. Both bounds are
  `const _: () = assert!(...)` beside the constant, not tests.
- **A rolled-up day bucket sits at midday UTC.** At midnight `Hour::date_at`
  reads it as the previous date for every reader west of the meridian.
- **The price archive carries only prices a vendor moved.** Most of what changes
  in LiteLLM's table is a missing field being filled in - `claude-sonnet-4-5`
  had no 1h cache-write price for a year - and for those today's entry is the
  *better* answer for a past month, so a model the archive omits keeps it.
  `scripts/make-prices.py` builds both files; mirror any `attribute_price_key`
  change there as before.

The first fetch runs a one-time deep read (`cost::read_history`) behind
`tokengauge-backfilled`, because a fetch's window reaches back only to the start
of the month and the feature would otherwise ship empty for a year. It defeats
the mtime filter `jsonl_files` leans on, so it must never run on a poll.

Design notes in `docs/history.md`, vocabulary in `CONTEXT.md`.

## Three frontends are driven, not just read

`qmllint` and `tsc` read these files. Three harnesses run them, each against
the same recording of what `--json` prints, so a frontend and the binary can
never quietly disagree about it:

| Harness | What it loads | What it covers |
| ------- | ------------- | -------------- |
| `crates/tokengauge-waybar/tests/e2e.rs` | the shipped binary | config, cache, staleness, `panel_spec`, the JSON |
| `tests/qml/run.sh` | `Usage.qml`, `Service.qml` | bindings, the snapshot read back, the next command |
| `tests/gnome/run.sh` | the compiled `extension.js` | all of that, plus the widget tree it draws |

None of them touches a network, a credential or a daemon. The binary's tests
seed a fresh snapshot, so `cache_is_stale()` says serve and the run never
fetches; the frontends' stubs answer a command by its command line rather than
spawning anything. The one test that wants a fetch backdates the write and
lands in the credential walk, because a provider with no token says so before
it asks anyone.

`scripts/make-panel-fixture.sh` records `tests/qml/fixtures/panel.json` off the
same seeded snapshot the binary's tests use, so the two cannot drift. Its
machine has no fleet store on purpose: the history comes out empty, which is
the state every user is in before their first recorded day and the one a
frontend most easily gets wrong by drawing nothing at all.

Two things follow from this and are easy to regress:

- **A frontend's data layer has to be instantiable on its own.** Plasma's
  `main.qml` is a `PlasmoidItem` that sets the attached `Plasmoid.icon`, which
  no QML stub can provide, so the half that owns the subprocess lives in
  `Service.qml` and imports no plasmoid module. `main.qml` re-exposes it under
  the names the representations already call. Keep new data work on that side
  of the line or it drops out of the harness.
- **The GNOME harness runs the compiled extension, not the TypeScript**, for
  the same reason CI's syntax check does: what ships is the JavaScript. It
  needs `scripts/build.sh` to have run, and says so rather than passing
  vacuously.

Waybar and the tray are absent: waybar's surface is the binary's own output,
which the e2e tests already assert, and the tray is Rust that only builds on
Windows.

## The binary is `tokengauge`, the crate is not

`crates/tokengauge-waybar` still builds the shared backend every frontend shells
out to, but its `[[bin]]` is named `tokengauge` - the crate grew out of a Waybar
module and the name outlived the scope. Clap is told the name explicitly, or
`--version` reports the package instead.

`tokengauge-waybar` survives as a **symlink** beside it, and release archives
carry a real copy under that name as well. Both are deliberate: the updater
performing an upgrade is the *old* binary, and it only knows to look for the old
name. Drop the duplicate copy once 0.22.x updaters are gone, and only then.

Frontend settings still default to `tokengauge-waybar` for the same reason -
after an upgrade driven by a 0.22.x updater, that is the only name on disk.
Flip those defaults in the release *after* the duplicate copy goes away, never
in the same one.

Two things that are not the binary and must not be renamed with it:
`tokengauge-waybar-state.json` (the waybar scroll selection, a state file users
already have) and the `[waybar]` config section, which really is Waybar-specific.
`signal_daemon_reload()` matches `tokengauge(-waybar)? --daemon`, because a
daemon started before the rename is the same process to reload.

## The updater is a crate, and it is not in this repository

Fetching a release, replacing the installed binaries, and reinstalling the
desktop payloads out of the same archive is
[selvedge](https://github.com/Arzaroth/selvedge), pinned by tag. TailGauge runs
on the same crate, which is the point: both projects had the same 1,500 lines
and both had to fix the same bug twice.

What stays here is `crates/tokengauge-core/src/project.rs`, and it is only
declarations: the binaries, the repository, the aliases, the frontends, the MSI
marker key. `TOKENGAUGE` is threaded into every selvedge call rather than read
from a global there.

Three things are easy to get wrong:

- **The cached update status is at TokenGauge's path, not selvedge's.** The
  crate would put it at `$XDG_CACHE_HOME/<binary>/update.json`; here it is
  `update_status_path`, beside the snapshot like every other state file,
  because the waybar binary writes it and the GUIs read it. Every selvedge
  entry point takes the path as an argument, so pass it. A disagreement about
  this path stops the update prompt working and fails nowhere.
- **`self-update` is a feature that maps, not one that ends.**
  `self-update = ["selvedge/self-update"]`, and the crate is taken with
  `default-features = false`, so `frontend` and `state` arrive without
  `self_update` behind them. A default build still links reqwest - that is
  ponytail, not the updater - so the check that means anything is
  `cargo tree -p tokengauge-core -e normal | grep self_update`, which must come
  back empty.
- **A gap in the machinery is fixed in selvedge, not worked around here.**
  Change it there, add a test there, cut a tag, repin. TailGauge pins the same
  crate, so a breaking change is two repositories. The Windows half compiles
  only in selvedge's CI, which has a `windows-latest` job for exactly that
  reason.

## Windows installs itself three ways, into one directory

`scripts/install.ps1`, `packaging/windows/tokengauge.wxs` and selvedge's
`update::apply` all write to `%LOCALAPPDATA%\TokenGauge\bin`. That is a
contract, not a coincidence: the updater replaces the binaries *beside the
running one*, so an installer that chose a different directory would leave two
copies on disk and only one of them would ever update. A user with a stray
binary above that folder is exactly how a July build survived twenty releases.

The MSI is per-user (`Scope="perUser"`), which is what lets it install without
elevation and manage the `PATH` entry through `<Environment>` so uninstall takes
it back. It records its `ProductCode` under `HKCU\Software\TokenGauge`, and that
marker is what `--update` reads to decide *how* to upgrade: with it, the upgrade
goes through `msiexec` so MSI stays the owner of what is on disk; without it,
the binaries are replaced in place as before. Replacing them underneath MSI is
the thing to avoid - Windows would keep describing a version nobody is running,
a repair would restore the old one, and the next MSI would compare against it.

selvedge's `msi_upgrade` returns while the installer is still running, and has
to: the package replaces the executable calling it. That is why a caller seeing
`Applied.installer_launched` exits promptly rather than reporting a version, and
why the tray quits when it launches an update.

**Adding a release asset is a compatibility event.** `asset_for` matches by
substring, so every updater already shipped takes whatever asset happens to
match first. The MSI is named `win64`, not `windows-x86_64`, purely so old
updaters cannot see it; new ones ask for selvedge's `ARCHIVE_SUFFIX`
explicitly. Name the
next Windows asset carelessly and you break `--update` on machines whose
binaries you can no longer change.

WiX only runs properly on Windows, so the `.wxs` is compiled in CI's
`build-windows` job as well as the release one. Do not trust a build of it on
Linux: it reports false errors on plain `Directory/@Name` values, though it does
still catch schema mistakes.

## Conventions

- `CHANGELOG.md` is the source of truth for GitHub release notes. Update
  `[Unreleased]` with every user-facing change.
- Before finishing: `cargo fmt --all`, `cargo clippy --workspace --all-targets`,
  `cargo test --workspace`. For QML run `qmllint`, for the GNOME extension
  `pnpm typecheck` and then `scripts/build.sh`, and then the two frontend
  harnesses: `tests/qml/run.sh` and `tests/gnome/run.sh`. CI's `frontends` job
  runs all of them, so they are enforced rather than remembered.
- `scripts/coverage.sh` runs `cargo llvm-cov` over the workspace and then ranks
  the files by uncovered lines; `--html` opens the browsable report. It is a
  local tool, not a CI gate - nothing fails on a number. The gaps it keeps
  pointing at are the process surfaces (event loops, `main`, the daemon accept
  loop) and they stay uncovered on purpose; what is worth reading is a *logic*
  file drifting down the list.
- `tokengauge-tray` is `cfg(windows)`-gated with Windows-only GUI deps, so it
  does not type-check on Linux. CI's Windows job runs `cargo clippy -p
  tokengauge-tray` and is the authority. To check a change locally before
  pushing, temporarily lift the `[target.'cfg(windows)'.dependencies]` header in
  its `Cargo.toml` and swap the three `#[cfg(windows)]` / `#[cfg(not(windows))]`
  attributes in `main.rs` for `#[cfg(all())]` / `#[cfg(any())]`, run
  `cargo clippy -p tokengauge-tray`, then revert both. eframe and tray-icon do
  build on Linux.
- A running `tokengauge --daemon` (the installed binary in
  `~/.local/bin`) serves the bar, the tooltip and `--json` over
  `<cache_file parent>/tokengauge.sock`, so a freshly built binary invoked with
  no flags - or with `--json` - proxies to the **old** daemon. To exercise new
  tooltip or panel code, point `--config` at a copy of the config whose
  `cache_file` lives elsewhere.
