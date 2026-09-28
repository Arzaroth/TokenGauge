---
name: feature-treatment
description: Ship a finished TokenGauge feature branch the full way - rebase onto master, run a max-effort multi-agent review with TokenGauge's lenses (frontend parity, credentials, cost-reader traps, cross-platform), fix everything confirmed as new layers, update the knowledge base, pass the full gate, open the PR and babysit it to a squash-merge, cut a release, and remove the worktree. Use when the user says "feature treatment", "treat this branch", "review and merge this feature", or names a worktree or branch to ship.
---

# Feature treatment

The pipeline a finished feature branch goes through before it lands:
**rebase -> max review -> fix -> update the brain -> gate -> PR and merge ->
release -> clean up**. Nothing merges without an adversarial review and a green
gate.

The branch comes from the argument. If none is given, `wt list` and pick the
non-master worktree under `<main checkout>.worktrees/`; ask if more than one is
a candidate. Work inside that worktree, never in the main checkout (the user
works in it live, and it may be on another branch). Paths are absolute, since
`wt switch` cannot move the agent's shell and worktrees nest by branch name:

```bash
MAIN=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
WT="$MAIN.worktrees/<branch>"
```

## 1. Rebase onto master

First decide whether history may be rewritten. If the branch is already pushed
with a PR that has review on it (`gh pr list -R Arzaroth/TokenGauge --head <branch>`),
do not rebase: merge `origin/master` in, or ask. Otherwise:

```bash
cd "$WT" && git branch --show-current && git fetch origin && git rebase origin/master
```

Use the `resolving-merge-conflicts` skill on conflicts.

Then check the size rule: `git diff --name-only origin/master...HEAD | wc -l`
must be at most 149. Over it, stop and propose a split along a seam (core
first, then frontends; or one subsystem per PR) before reviewing anything.

## 2. Max-effort review (parallel finders)

Diff: `git diff origin/master...HEAD -- . ':(exclude)Cargo.lock' ':(exclude)pnpm-lock.yaml' ':(exclude)tests/qml/fixtures' ':(exclude)crates/*/tests/fixtures/*' ':(exclude)crates/tokengauge-core/src/cost/prices.json' ':(exclude)crates/tokengauge-core/src/cost/price-archive.json'`
(generated files; read them only if a finder needs to confirm a regen).

Spawn independent finder agents in one message, each over the same diff with a
different lens. Scale to the feature, 4-6 is typical. Give each one the
relevant `CLAUDE.md` section(s) by name so it reviews against the rules rather
than rediscovering them.

- **Correctness** - line by line: inverted conditions, off-by-one, `unwrap` on
  data a provider controls, serde shapes that reject a `null` or a missing
  field, time math that assumes local midnight or UTC, stale-vs-fresh decisions
  that bypass `cache_is_stale()`, state files not derived from `cache_file`'s
  parent.
- **Frontend parity** - does a user-facing change land on all six surfaces
  (waybar, Plasma, GNOME, Quickshell, tray, TUI)? Is every string a user reads
  computed in `panel.rs` / `history.rs`, not in a frontend? New `SectionKind`
  handled everywhere? New JSON field declared in `gnome/*/panel.ts` with the
  exact JSON name? New data work on `Service.qml`'s side of the line? A frontend
  that reads a credential, a cache file or a provider endpoint itself is a
  finding.
- **Credentials / security** - a credential check that stats instead of
  validates; a hollow source shadowing a good one; tokens in logs, errors that
  reach the snapshot, or `--doctor` output; `CLAUDE_CONFIG_DIR` not routed
  through `claude_config_dir()`; shell injection in `scripts/*.sh`,
  `install.ps1`, or a frontend's subprocess command line; socket or state-file
  permissions.
- **Costs and history** - if `cost/` or `history.rs` moved: the five transcript
  traps (streamed duplicates, Codex cumulative totals, cached-inside-input,
  Kimi session scope, Grok `modelUsage`), `price_candidates` vs
  `attribute_price_key` agreement, `vendor_prefixes`, `natively_read`, the
  retention constants' `const` asserts, a deep read that could run on a poll.
- **Cross-platform / release** - `cfg(windows)` / `cfg(target_os = "macos")`
  code the Linux build never compiles, tray changes (Windows and the macOS
  menu bar), the `org.tokengauge.daemon` LaunchAgent label drifting from
  `LAUNCHD_LABEL` in `daemon.rs`, a new release asset name
  an old updater could match by substring, a binary or state-file rename that
  breaks a 0.22.x updater, selvedge behaviour worked around here instead of
  fixed there.
- **Tests / reuse** - new branches without a test, an e2e or harness path the
  feature should drive and does not, the `panel::tests` source-grepping guards,
  re-implemented helpers (formatters, tones) that exist in core.

Each finder returns JSON findings `{file, line, severity, summary,
failure_scenario}`, verified by quoting the line, most severe first, and fixes
nothing. Optionally run one sweep finder over the merged list that hunts only
for gaps.

## 3. Fix what is confirmed

Re-verify each claim against the code before acting; finders produce plausible
wrong items too. Fix every confirmed correctness, parity or security finding
and the worthwhile quality ones. Real but out of scope goes to `TODO.md` as one
line, not dropped.

Fixes land as **new layers on top**, one kind of work per commit, each leaving
the tree green: `[<branch>] fix(<area>): ...`. Do not fold them into the
feature commits.

## 4. Update the brain

Apply the `brain` skill's maintain checklist to what the branch changed: new
rules into `CLAUDE.md`, new terms into `CONTEXT.md`, subsystem notes into
`docs/`, an ADR for a decision with real alternatives, `README.md` for
user-visible config, and a `CHANGELOG.md` `[Unreleased]` entry for anything a
user sees. Commit as its own `docs(...)` layer on the branch so it ships in the
same merge.

## 5. Gate

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
pnpm install --frozen-lockfile && pnpm typecheck && scripts/build.sh
tests/qml/run.sh
tests/gnome/run.sh
```

Plus qmllint on touched QML (a `[syntax]` diagnostic is the failure; see
`ci.yml`). If the tray crate changed, clippy it for Windows and macOS (the
`cross-target-clippy` memory, or the cfg swap in `CLAUDE.md`, reverted after).
If a transcript reader changed, `cargo test -- --ignored agrees_with_ccusage_on_real_transcripts`
too. If the panel JSON shape changed, regenerate `tests/qml/fixtures/panel.json`
with `scripts/make-panel-fixture.sh`.

Re-count files against the 149 limit after the fixes.

## 6. PR and merge

```bash
git push -u origin <branch>
```

Then hand it to the `babysit-pr` skill: PR against `master` on
`Arzaroth/TokenGauge` (always `-R Arzaroth/TokenGauge`, `gh` defaults to the
upstream fork), CodeRabbit review rounds, squash-merge when clean. The PR title
is the squash subject, in the plain-prose style of the existing ones
(`git log --oneline origin/master | head`).

## 7. Release

Run the `release` skill. Minor for a user-facing feature, patch for
fix-only. Skip only if the user says to batch it with the next one.

## 8. Clean up

```bash
wt remove <branch>
```

## Done when

The feature is squash-merged on master, the docs reflect it, the gate was
green, a tag is pushed (unless skipped), and the worktree is gone. Report the
PR, the version, and one line on what the review caught.
