# Distribution: binaries, updater, platforms

How TokenGauge is named, packaged, installed and updated on Linux, macOS and
Windows. The rules with their incidents are in `CLAUDE.md` ("The binary is
`tokengauge`", "The updater is a crate", "macOS is two LaunchAgents", "Windows
installs itself three ways"). The release procedure is in
[../operations.md](../operations.md).

## The binary is `tokengauge`, the crate is not

`crates/tokengauge-waybar` builds the shared backend every frontend shells out to,
with `[[bin]] name = "tokengauge"` (clap is told the name explicitly). The crate
grew out of a Waybar module.

- `tokengauge-waybar` survives as a symlink, and release archives carry a real
  copy, because the updater doing an upgrade is the **old** binary and only knows
  the old name. Drop the copy once 0.22.x updaters are gone; flip frontend
  settings defaults (still `tokengauge-waybar`) only in a later release.
- Not renamed with it: `tokengauge-waybar-state.json` and the `[waybar]` config
  section. `signal_daemon_reload()` matches `tokengauge(-waybar)? --daemon`.

## selvedge

[selvedge](https://github.com/Arzaroth/selvedge), pinned by tag in the root
`Cargo.toml`, fetches a release, replaces the installed binaries and reinstalls the
desktop payloads from the same archive. TailGauge runs on it too.
`crates/tokengauge-core/src/project.rs` holds only declarations (binaries, repo,
aliases, frontends, MSI marker key); `TOKENGAUGE` is threaded into every call.

- `selvedge::plans` is also the ALL PLANS arithmetic (units of the largest plan,
  half-even rounding, pooled fill, split-bar widths), shared with remuda's page.
  `panel.rs` keeps only the words.
- The cached update status lives at `update_status_path` beside the snapshot, not
  selvedge's default `$XDG_CACHE_HOME/<binary>/update.json`; pass it to every
  entry point.
- `self-update = ["selvedge/self-update"]` with `default-features = false`.
  `cargo tree -p tokengauge-core -e normal | grep self_update` must be empty for a
  default build (reqwest is still linked, by ponytail).
- A gap in the machinery is fixed in selvedge, tagged and repinned, never worked
  around here.

User side: `tokengauge --update` / `--check-update` (`tokengauge-tui.exe` on
Windows); the daemon checks every `update.check_interval_secs` (6h) and notifies;
desktop frontends show an Update button; `TOKENGAUGE_REPO=owner/repo` updates from
a fork. After an update the daemon is restarted (systemd) or kickstarted
(launchd).

## Linux

`scripts/install.sh` installs the binaries to `~/.local/bin`, wires waybar
(`--placement=left|right`, persisted in `[waybar] placement`), and enables the
`tokengauge-daemon.service` user unit unless `--no-daemon`. Desktop payloads come
from `scripts/install-{plasma,gnome,omarchy}.sh` or `--install-frontend`.

## macOS

- `scripts/install.sh` installs `tokengauge`, `tokengauge-tui`, `tokengauge-tray`
  to `~/.local/bin` and two LaunchAgents: `org.tokengauge.daemon`
  (`tokengauge --daemon`) and `org.tokengauge.tray` (`--hidden`). The daemon label
  must equal `LAUNCHD_LABEL` in `daemon.rs`, which `--update` kickstarts; a
  mismatch fails nowhere.
- Config and state stay on XDG paths, not `~/Library/Application Support`.
  `launch::notify` / `open_url` use `osascript` and `open`.
- `scripts/package-macos.sh` signs the per-arch binaries, `lipo`s them into
  `TokenGauge.app` (tray as main executable) and a DMG. The release job signs with
  `MACOS_CERTIFICATE_P12` and notarizes with `APPLE_API_KEY_*`; tarballs are
  notarized online, only the DMG is stapled. PR CI runs it unsigned.
- The `.app` cannot self-update (a swapped binary breaks the seal); selvedge
  refuses under `.app/Contents/` and the tray's Update item opens the release.

## Windows

- `scripts/install.ps1`, `packaging/windows/tokengauge.wxs` and selvedge's
  `update::apply` all write to `%LOCALAPPDATA%\TokenGauge\bin`: the updater
  replaces binaries beside the running one, so a second directory would leave a
  copy that never updates.
- The MSI is per-user (`Scope="perUser"`), manages `PATH` through `<Environment>`,
  and records its `ProductCode` under `HKCU\Software\TokenGauge`. With that marker
  `--update` upgrades through `msiexec`; without it, in place. `msi_upgrade`
  returns while the installer runs, so callers exit promptly.
- **A new release asset is a compatibility event**: `asset_for` matches by
  substring, so the MSI is named `win64` to stay invisible to old updaters.
- WiX is compiled in CI's `build-windows` job; a Linux build of the `.wxs` reports
  false errors.
- No waybar binary on Windows: the TUI and the tray carry everything.

## Sources

- `crates/tokengauge-waybar/Cargo.toml`, `crates/tokengauge-core/src/project.rs`, `Cargo.toml` (selvedge pin)
- `crates/tokengauge-core/src/statefiles.rs` (`update_status_path`), `crates/tokengauge-waybar/src/daemon.rs` (`LAUNCHD_LABEL`)
- `crates/tokengauge-core/src/config.rs` (`signal_daemon_reload`), `crates/tokengauge-core/src/launch.rs`
- `scripts/{install.sh,install.ps1,update.sh,package-macos.sh,tokengauge-daemon.service}`, `packaging/windows/tokengauge.wxs`
- `.github/workflows/release.yml`, `.github/workflows/ci.yml`
