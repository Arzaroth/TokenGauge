# TokenGauge

## Frontend parity (hard rule)

TokenGauge ships one gauge across six surfaces. **A user-facing feature lands
on all of them, or it is not done.**

| Surface    | Where                                        | Draws the panel |
| ---------- | -------------------------------------------- | --------------- |
| Waybar     | `crates/tokengauge-waybar` (bar + tooltip)    | yes - the tooltip *is* waybar's panel |
| Plasma     | `plasma/org.tokengauge.plasmoid`              | yes |
| GNOME      | `gnome/tokengauge@arzaroth.github.io`         | yes |
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
which is the backstop for the QML and JS frontends the compiler cannot check.

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

### Rows the spec drops

A window the provider does not report (`used: None`) and a `placeholder: true`
extra window are both omitted. Every panel used to disagree about this; now none
of them decide it.

## Snapshot, staleness, and how a change reaches a frontend

The snapshot lives at `$XDG_STATE_HOME/tokengauge/tokengauge-usage.json`
(`cache_file`), and every other state file is derived from its **parent**: the
daemon socket, the refresh sentinel, the selected provider, the notify state,
`tokengauge-prices.json`, and `tokengauge-revision`. It is state, not cache: it
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
machine, regenerated by `scripts/make-prices.py`). Every reader produces the same
unit, a `UsageEvent`, so **a new CLI is one more reader and nothing else**: add a
field to `cost::Roots`, call it from `read_events_from`, and `build_report` does
the rest.

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

## Windows installs itself three ways, into one directory

`scripts/install.ps1`, `packaging/windows/tokengauge.wxs` and
`update::apply_full` all write to `%LOCALAPPDATA%\TokenGauge\bin`. That is a
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

`msi_upgrade` returns while the installer is still running, and has to: the
package replaces the executable calling it. That is why the caller exits
promptly and why the tray quits when it launches an update.

**Adding a release asset is a compatibility event.** `asset_for` matches by
substring, so every updater already shipped takes whatever asset happens to
match first. The MSI is named `win64`, not `windows-x86_64`, purely so old
updaters cannot see it; new ones ask for `ARCHIVE_SUFFIX` explicitly. Name the
next Windows asset carelessly and you break `--update` on machines whose
binaries you can no longer change.

WiX only runs properly on Windows, so the `.wxs` is compiled in CI's
`build-windows` job as well as the release one. Do not trust a build of it on
Linux: it reports false errors on plain `Directory/@Name` values, though it does
still catch schema mistakes.

## Conventions

- `CHANGELOG.md` is the source of truth for GitHub release notes. Update
  `[Unreleased]` with every user-facing change.
- `gh` resolves to the upstream fork parent here; always pass
  `-R Arzaroth/TokenGauge`.
- Before finishing: `cargo fmt --all`, `cargo clippy --workspace --all-targets`,
  `cargo test --workspace`. For QML run `qmllint`, for the GNOME extension
  `node --input-type=module --check`. CI's `frontends` job runs the last two, so
  they are enforced rather than remembered.
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
