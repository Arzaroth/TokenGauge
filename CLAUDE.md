# TokenGauge

## Frontend parity (hard rule)

TokenGauge ships one gauge across five surfaces. **A user-facing feature lands
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
data: `limits`, `cost`, `tokens_by_day`, `tokens_by_model`, `tokens_by_device`.

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
set each fetch ran with. Age alone was the old rule, and it is why enabling a
provider used to do nothing for ten minutes. `retain_enabled()` still handles the
other direction, filtering a provider switched off out of a snapshot that is
otherwise fine.

Every write goes through `write_cache_full`, which writes atomically and then
rewrites `tokengauge-revision`. Frontends watch that file (Quickshell `FileView`,
GNOME `Gio.FileMonitor`, and `--wait-change` for the Plasma applet, whose toolkit
has no watcher) and re-run `--json` when it moves. Their poll timers stay: with
no daemon running, a poll is what ages the snapshot out and triggers the next
fetch.

`--set-provider` fetches before it returns, because frontends run
`--set-provider && --json` in one subprocess and the `--json` has to see the new
provider. The daemon's SIGHUP reload then finds a snapshot that already covers
the new set and re-renders instead of fetching again.

## Costs are read, not shelled out for

`crates/tokengauge-core/src/cost/` parses the transcripts the CLIs already
write - `~/.claude/projects` and `~/.codex/sessions` - and rates them against
LiteLLM's price table (`pricing.rs`: fetched, cached beside the snapshot, with
`prices.json` vendored in for a cold or offline machine). Every reader produces
the same unit, a `UsageEvent`, so **a new CLI is one more reader and nothing
else**; `build_report` does the rest.

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

Three traps are load-bearing, each with a test named after it:

- A **streamed message is written repeatedly**, each record restating the same
  `(message.id, requestId)` with more `output_tokens`. A duplicate upgrades the
  event; keeping the first loses 46% of some models' daily output.
- Codex `total_token_usage` is **cumulative for the session**. Its delta is the
  billed unit - `last_token_usage` re-fires with the cumulative unchanged and
  summing it roughly doubles the total.
- Codex `cached_input_tokens` is **inside** `input_tokens`, where Anthropic
  reports cache reads beside them.

ccusage is now the fallback and the cross-check, not the source. `cost_source`
picks: `auto` (default) reads natively and asks ccusage only about enabled
providers the readers found nothing for - a Kimi or Grok plan driven from its
own CLI writes into neither tree.

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
  `~/.local/bin`) serves the bar and tooltip over
  `<cache_file parent>/tokengauge.sock`, so a freshly built binary invoked with
  no flags proxies to the **old** daemon. To exercise new tooltip code, point
  `--config` at a copy of the config whose `cache_file` lives elsewhere.
