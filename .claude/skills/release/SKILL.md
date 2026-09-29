---
name: release
description: Cut a TokenGauge release, the whole shebang - docs sweep (CHANGELOG, README, THIRD-PARTY.md, CLAUDE.md), version bump across the four crates, three frontend manifests and the panel fixture, full local gate, [master] chore(release) commit, annotated vX.Y.Z tag, push, then watch the Release workflow until the GitHub release has its assets. Use when the user says "cut a release", "release 0.36.0", "ship a version", or "the whole release shebang" in this repository.
---

# Cutting a TokenGauge release

There is no release task: the bump is by hand, and the value of this skill is
knowing every file the version lives in and what CI does with the tag.

## 1. Pre-flight

- **Never switch branches in, or release from, the main checkout** - the user
  works in it live. Find it and the master worktree by absolute path, since
  `wt switch` cannot move the agent's shell:

  ```bash
  MAIN=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
  git -C "$MAIN" branch --show-current   # master here: stop and ask
  wt switch master                       # creates $MAIN.worktrees/master if absent
  cd "$MAIN.worktrees/master" && git branch --show-current   # must print master
  git pull --ff-only
  ```

  Note whether this run created the worktree; step "Done when" depends on it.
  Every later command runs from `$MAIN.worktrees/master`.
- Tree clean, up to date with `origin/master`. Every PR meant for this release
  is merged (`gh pr list -R Arzaroth/TokenGauge`). Always pass
  `-R Arzaroth/TokenGauge`: `gh` otherwise resolves to the `upstream` fork.
- Commits are authored as `lekva+github@arzaroth.com`; check
  `git config user.email` in the worktree before committing.
- Previous version: `git describe --tags --abbrev=0` (tags are `vX.Y.Z`).
- Next version: minor for any `### Added` / user-visible `### Changed`, patch for
  fix-only. The project is pre-1.0; nothing is a major.

## 2. Docs sweep (against `git log v<last>..HEAD`)

- **`CHANGELOG.md` `[Unreleased]`** covers every user-visible change since the
  tag. Entries are added per-PR, so this is a completeness check: walk
  `git log v<last>..HEAD --oneline` and match each merged PR to an entry.
  Keep a Changelog categories. Entries are prose that says what the user sees,
  the style of the ones already there.
- **`README.md`**: new providers, config keys, flags, frontends, install steps.
- **`THIRD-PARTY.md`**: `git diff v<last>..HEAD -- Cargo.toml '*/Cargo.toml' package.json`.
  A new dependency whose code or protocol we took from a project gets credited
  there; a dropped one comes out.
- **`CLAUDE.md` / `docs/`**: run the `brain` skill's audit scoped to what the
  release touched.
- Commit anything here as its own layer, `[master] docs(release): ...`, before
  the bump.

## 3. Bump

The version lives in exactly these places. Afterwards
`git grep -nF '"<old>"' -- ':!Cargo.lock' ':!CHANGELOG.md' ':!crates/*/tests/fixtures'`
must come back empty (the quotes keep `pnpm@10.33.0` and prose out; the cost
fixtures carry CLI versions that are not ours and must not be touched):

| File | Field |
| --- | --- |
| `crates/tokengauge-{core,tray,tui,waybar}/Cargo.toml` | `version` |
| `Cargo.lock` | refreshed by `cargo build --workspace` (or `cargo update -w`) |
| `gnome/tokengauge@arzaroth.github.io/metadata.json` | `version-name` (not `version`) |
| `omarchy/arzaroth.tokengauge/manifest.json` | `version` |
| `plasma/org.tokengauge.plasmoid/metadata.json` | `KPlugin.Version` |
| `tests/qml/fixtures/panel.json` | top-level `version` and `update.current` |

The fixture's `update.latest` only moves when it stops being ahead of the new
version (`scripts/make-panel-fixture.sh` sets it one minor above). Edit the
fixture's version fields in place rather than regenerating it: a regen reseeds
the dated history points and churns the whole file.

`CHANGELOG.md`: rename `## [Unreleased]` to `## [x.y.z] - YYYY-MM-DD` and open a
fresh empty `## [Unreleased]` above it. The heading must start exactly
`## [x.y.z]` - the Release workflow `awk`s that prefix out as the GitHub release
body and falls back to auto-generated notes (with only a warning) if it misses.

## 4. Gate (all green before the commit)

CI runs only on `pull_request`, so nothing checks the release commit after it
is pushed straight to master: this gate is the only one it gets. It is the set
CI's `build` and `frontends` jobs run (`build-windows` and `build-macos` cover
what Linux cannot compile):

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
pnpm install --frozen-lockfile && pnpm typecheck && scripts/build.sh
for f in build/frontends/gnome/**/*.js; do node --input-type=module --check < "$f"; done
for f in gnome/**/metadata.json omarchy/**/manifest.json plasma/**/*.json; do
  node -e 'JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))' "$f"
done
tests/qml/run.sh
tests/gnome/run.sh
```

(`shopt -s globstar` first in bash.) The manifest check matters most here: the
bump hand-edits exactly those files. Plus qmllint over `plasma/**/*.qml omarchy/**/*.qml` (a `[syntax]` diagnostic is
the failure, not the exit code - see `Lint QML` in `ci.yml`). The tray and the
keychain path only compile on Windows and macOS: if `crates/tokengauge-tray` or
any `cfg(windows)` / `cfg(target_os = "macos")` code changed since the tag,
clippy it cross-target (see the `cross-target-clippy` memory) or confirm the
`build-windows` and `build-macos` checks were green on the PR that changed it
(`gh pr checks <n> -R Arzaroth/TokenGauge`); nothing runs on the merge commit.

## 5. Commit, tag, push

```bash
git commit -am "[master] chore(release): x.y.z" -m "<body>"
git tag -a vx.y.z -m vx.y.z
git push origin master --follow-tags
```

The body is two or three short paragraphs of prose on what the release is
about - the theme, not a copy of the changelog - plus any note for the next
release (e.g. that the fixture's `update.latest` will need bumping). End with
the Co-Authored-By trailer. Look at `git log -1 --format=%B v<last>` for the tone.

## 6. Watch

The tag push triggers `.github/workflows/release.yml`: Linux x86_64 and aarch64
archives (binaries + compiled frontend payloads), the Windows zip and MSI, the
macOS aarch64 and x86_64 archives (binaries + tray, no desktop payloads) and
the universal `TokenGauge.app` DMG, then the `release` job publishes. The
macOS job signs everything with the Developer ID from the repository secrets
and notarizes it; a rejected notarization fails the job and prints Apple's
log rather than publishing unsigned files.

```bash
gh run list -R Arzaroth/TokenGauge --workflow release.yml -L 1
gh run watch -R Arzaroth/TokenGauge <id> --exit-status
gh release view vx.y.z -R Arzaroth/TokenGauge --json assets --jq '.assets[].name'
```

If a build job fails after the tag is out: fix on master in a new commit, then
re-run the workflow with `workflow_dispatch` and `tag=vx.y.z` only if the fix is
in CI config; if code changed, cut the next patch instead of moving a published
tag.

## Done when

The tag is on origin, the GitHub release exists with all seven assets (the
`linux-*` and `macos-*` tarballs, the `macos-universal` DMG, the
`windows-x86_64` zip and the `win64` MSI),
and its body opens with the changelog section (GitHub's generated "What's
Changed" follows it; that is expected). Report the version, the release URL
and the one-line theme. Remove the master worktree only if this run
created it.
