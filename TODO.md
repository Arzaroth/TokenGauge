# TODO

Ideas worth building, and gaps worth closing. Not a roadmap and not a
commitment - a place to keep the reasoning so it does not have to be
rediscovered.

Each entry says what it is, why it is worth doing *here* rather than in the
abstract, and what the nasty part is. Effort is S / M / L and deliberately
pessimistic.

Spend history came off this list and is in flight as
[#33](https://github.com/Arzaroth/TokenGauge/pull/33).

## Features

### Cost by project

A `tokens_by_project` section: which repos the spend went to, today and this
month, with the same per-day and per-device drilldown the model rows already
have.

Every reader already walks a tree keyed by working directory.
`cost/claude_code.rs` reads `~/.claude/projects/<encoded-cwd>/*.jsonl`, and
Codex's `session_meta` / `turn_context` carry `cwd` beside the model
`codex_cli.rs` already extracts. The data is under the parser's cursor and gets
thrown away: `UsageEvent` has `provider`, `model`, `date`, `at`, `tokens`, `key`
and no project. This is the most-asked question the tool holds the answer to and
does not answer.

**Effort: M.** One field on `UsageEvent`, four readers, one aggregation, one
section - `panel.rs` was built so a new section costs one file.

**The nasty part is the project key.** Claude Code encodes the cwd into a
directory name lossily (slashes to dashes), Codex stores a raw absolute path,
and the same repo checked out at two paths on two machines must not read as two
projects. Settle on a canonical key (git remote? basename? a user-configured
map?) *before* writing a reader. Keep it out of the sync wire format in v1:
adding a project to a bucket multiplies cardinality and is a fleet-wide
compatibility event.

### Threshold notifications on Windows and macOS

`fire_notification` (`crates/tokengauge-waybar/src/snapshot.rs`) shells out to
`notify-send`, and it lives in `tokengauge-waybar` - the Linux-only crate. So
the entire threshold-notification feature, a README headline, is silently absent
for every Windows tray user, even though the tray is a long-lived polling
process perfectly placed to fire it. This is a live violation of the frontend
parity rule in `CLAUDE.md`.

The state machine is already portable: `statefiles::thresholds_to_fire` is in
core with fourteen tests behind it. Only the transport is not.

**Effort: S/M.** Move `fire_notification` into core behind a platform-dispatched
sender. The risk is dependency weight on the tray, and Windows toasts wanting a
registered AppUserModelID - which the MSI can now provide, since it already
writes a Start Menu shortcut.

### Spend budgets, and alerts that fire before the wall

`[budget] monthly_usd = 200`, rendered as a meter in the COST section, plus
notifications driven by *projection* rather than by the current percentage.

Two things already exist and are wired to nothing. `pace.rs` computes exactly
that projection - `ends ~16%`, `empty in 2h 15m` - for every window, and it
reaches the panel as a badge but never reaches the notifier, which only compares
a raw percentage against a threshold. And costs are a headline feature with no
ceiling anywhere in the config. "You crossed 80%" is strictly less actionable
than "at this rate you run out before the reset", and the second number is
already sitting there.

**Effort: S** for the pace-based alerts (a new trigger reading `UsagePace`,
reusing `NotifyState`). **M** for budgets, which need a config section, a
`Meters` row, and a decision about whether a budget is per-provider,
fleet-wide, or both. Worth little off Linux until the item above lands.

### GLM in fleet sync, and a GLM reader

GLM is the one provider with `natively_read: false`, and that flag gates two
real things: it cannot take part in fleet sync at all, and `--doctor`'s drift
check excuses it. A user on zcode.z.ai sees limits and a blank cost row.

There are two routes and the cheap one is not the obvious one:

1. **Ship day-granularity contributions.** `contribution.rs` has reserved
   `Granularity::Day` since the sync design specifically as the degraded shape a
   ccusage-sourced provider needs, and `docs/sync.md` says the gap "should not
   stay open long". History now *produces* day buckets locally (compaction rolls
   them up past 35 days), but they are still never sent: the compaction floor is
   deliberately at or above the wire retention. Emitting them would let GLM into
   sync with no reader at all. **Effort: S/M**, and the design already exists.
2. **Write a transcript reader.** Better cost figures and it flips
   `natively_read`, but **M with an unknown at the front**: confirm the z.ai CLI
   writes a parseable per-call transcript, and where, before scoping anything. If
   it does not, this is not buildable and ccusage stays the answer.

Do (1) first.

### A macOS surface

macOS is the strangest hole in the project. `claude.rs` reads the macOS keychain
and `CLAUDE.md` admits that path "is exercised only by the Windows CI job and
never on Mac" - code written *for* macOS, shipped, and never once compiled on
it. Meanwhile `release.yml` builds linux-x86_64, linux-aarch64 and windows-msvc
and nothing else, so a Mac user has no binary at all. Claude Code's largest user
population is on macOS.

**Effort: L, honestly.** Split it:

- **First slice, ~a day:** add `aarch64-apple-darwin` to CI and the release
  matrix. This alone earns its keep by *compiling* the keychain path, and gives
  Mac users the TUI. Mind `update::asset_for` - `CLAUDE.md` is emphatic that
  naming a new release asset carelessly breaks `--update` on machines whose
  binaries can no longer be changed.
- **Second slice:** a menu bar app. "Just reuse the tray" understates it - the
  crate is `cfg(windows)`-gated with Windows-only deps, and the flyout
  anchoring, icon rendering and notification path are all platform work.

### Distribution through package managers

`packaging/` contains exactly one thing: `windows/tokengauge.wxs`. Linux
installs are `curl | bash` into `~/.local/bin` with a hand-rolled self-updater,
on a tool whose primary audience is Omarchy and Hyprland users - which is to say
Arch users who install everything from the AUR.

**Effort: S** each for AUR and Homebrew (release-tag driven, mostly CI), **M**
for Nix if the QML/GJS frontends are packaged too. The ongoing cost is real: an
AUR package is a maintenance commitment and a second update path to keep
consistent with `--update`. A package-managed install should decline to
self-update, the way the MSI marker already teaches it to.

## Correctness gaps

These are not features. They make a number quietly wrong, which this project
treats as worse than visibly broken.

- **Sub-hour timezone offsets misplace up to an hour of tokens.** The bucket key
  is a whole UTC hour, so +05:45 and +09:30 land on the wrong side of a day
  boundary (`docs/sync.md` §2). Small population, real wrong numbers.
- **Partial transcript-tree overlap is undetectable.** The double-count check
  fingerprints whole days, so two machines sharing *some* of a day's transcripts
  double-count in silence (`docs/sync.md` §8). The README only says to turn the
  provider off.
- **No fleet key rotation.** Re-keying starts a new fleet rather than rotating
  one, and a lost machine can read the fleet until every other machine is
  re-keyed. Inherent to the symmetric-key choice (ADR 0002), but the *recovery*
  story could be better than it is.
- **A history month of zeros is ambiguous** - nothing spent, or no transcript
  survives to say otherwise, and the data cannot tell you which. See
  `docs/history.md` §6.

## Chrome inconsistencies

Small, cheap, and the kind of thing that makes the set of frontends feel like
one product rather than five.

- **GNOME hides provider toggles in a separate Adwaita prefs window** while
  Plasma, Omarchy and the tray put them inline in the panel.
- **The TUI has no provider toggle and no pin**, despite owning the sync editor
  and the history screen.
- **Omarchy's scroll-rotate does not persist** to `config.toml` while waybar's
  does, so the same gesture means different things on two Linux bars.
- **The Omarchy widget still defaults `binary` to `tokengauge-waybar`**, with a
  note that it moves to `tokengauge` "a release after the rename". Check that
  against the rule in `CLAUDE.md` about 0.22.x updaters before flipping it.
- **`--client-tail` is hidden and experimental** ("most waybar versions don't
  pick up streaming exec output"). Either make it work or retire it.

## Deferred from the history work

- **`--json` grew from ~10 KB to ~48 KB for two providers** (~95 KB at five)
  because all three history ranges ship resolved. Fine for a local subprocess
  reading a local file, and it is what makes switching a range a rebind rather
  than another `--json`. If it needs trimming, the levers are dropping
  `full_label` or serving only the selected range behind a `--history-range`
  flag.
- **Per-project history** would fall out of "cost by project" above, but needs
  the project key on `UsageEvent` first.
- **The history screens have never been looked at.** Five of them shipped with
  render tests and CI lint but no human eyes; the tray's could not even be
  compiled on the machine that wrote it.
