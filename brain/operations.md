# Operations

How TokenGauge is built, tested, installed, released and run. There is no task
runner: cargo, pnpm and the scripts in `scripts/` are the interface.

## The gate

What CI's `build` and `frontends` jobs run (and the `release` skill runs locally,
since nothing checks a release commit on master):

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
pnpm install --frozen-lockfile && pnpm typecheck && scripts/build.sh
tests/qml/run.sh
tests/gnome/run.sh
```

Plus `qmllint` over `plasma/**/*.qml omarchy/**/*.qml` (a `[syntax]` diagnostic
is the failure, see `Lint QML` in `ci.yml`), a `node --check` of the compiled
GNOME JavaScript, and a JSON parse of the frontend manifests.

- `build-windows` and `build-macos` jobs build, test and clippy what Linux cannot
  compile (the tray, the keychain path, `cfg(windows)` / `cfg(target_os = "macos")`
  code), validate the MSI and package the app unsigned. To clippy the tray on
  Linux, temporarily lift its target-gated dependency header and swap the three
  `cfg` attributes in `main.rs` for `#[cfg(all())]` / `#[cfg(any())]`, then revert
  (recipe in `CLAUDE.md`, Conventions).
- Not enforced: `tests/gnome/shell/run.sh --live` at `--gnome 45` and 50 after a
  GNOME popup layout change; `cargo test -- --ignored agrees_with_ccusage_on_real_transcripts`
  after a reader change; `scripts/coverage.sh`. See
  [architecture/testing.md](architecture/testing.md).

## Generators

| Script | Writes |
| --- | --- |
| `scripts/build.sh` | `build/frontends/<payload>`: compiled GNOME extension + Plasma + Omarchy payloads |
| `scripts/make-panel-fixture.sh` | `tests/qml/fixtures/panel.json` from the e2e seeded snapshot |
| `scripts/make-cost-fixture.py` | the ccusage golden under `crates/tokengauge-core/tests/fixtures/` |
| `scripts/make-prices.py` | `cost/prices.json` and `cost/price-archive.json` (mirror `attribute_price_key` changes) |
| `scripts/coverage.sh` | `cargo llvm-cov` ranking, `--html` report |

## Install and run

- Linux / macOS: `scripts/install.sh` (`--no-daemon`, `--placement=left|right`,
  `--version vX.Y.Z`, `--no-tray` on macOS), or on Linux the release's `.deb`,
  `.rpm` or `tokengauge-bin` package; Windows: `scripts/install.ps1` or the MSI. Desktop
  payloads: `scripts/install-{plasma,gnome,omarchy}.sh` or
  `tokengauge --install-frontend plasma|gnome|omarchy|all`. Details in
  [architecture/distribution.md](architecture/distribution.md).
- Daemon: `scripts/tokengauge-daemon.service` (systemd user unit) or the
  `org.tokengauge.daemon` LaunchAgent. Reload config with
  `pkill -HUP -f 'tokengauge(-waybar)? --daemon'`.
- Config: `~/.config/tokengauge/config.toml` (keys in `README.md` and
  `crates/tokengauge-core/config.example.toml`). State: beside `cache_file`
  ([architecture/snapshot.md](architecture/snapshot.md)).
- Dev trap: a built binary run with no flags or `--json` proxies to the installed
  daemon; use a `--config` whose `cache_file` lives elsewhere.

## Release

The `release` skill (`.claude/skills/release/SKILL.md`) is the procedure: docs
sweep, version bump in the four crate `Cargo.toml`s, `Cargo.lock`, the GNOME
(`version-name`), Omarchy and Plasma manifests and `tests/qml/fixtures/panel.json`,
the gate, `[master] chore(release): x.y.z`, annotated `vx.y.z` tag, push, then
watch `release.yml` until the GitHub release has its assets (seven archives and
installers, six Linux packages, the PKGBUILD bundle, `SHA256SUMS`, and once the
key is committed the signatures and the public key). A pull request touching the
release or the packaging rehearses all of it without publishing. `CHANGELOG.md`
is the source of the release notes: the workflow extracts the `## [x.y.z]`
section. Pre-1.0: minor for a user-facing change, patch for fixes.

Feature branches go through the `feature-treatment` skill (rebase, multi-agent
review, brain update, gate, PR via `babysit-pr`, release, worktree removal).
`gh` always needs `-R Arzaroth/TokenGauge` (an `upstream` fork remote exists).

## Upstream watching

The `upstream-check` skill reviews CodexBar, omarchy and ai-usagebar against
`.claude/skills/upstream-check/BASELINES.md`.

## Sources

- `.github/workflows/ci.yml`, `.github/workflows/release.yml`
- `scripts/`, `packaging/windows/tokengauge.wxs`, `package.json`
- `.claude/skills/{release,feature-treatment,upstream-check}/SKILL.md`
