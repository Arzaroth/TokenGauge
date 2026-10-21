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
  half-even rounding, pooled fill, split-bar widths) and the Claude Team seat
  rule (`team_seat`), shared with remuda's page.
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
a fork. A binary a package manager installed is refused by `selvedge::update::owner`,
so `--json`'s `update.upgraded_by` (set only while `available`) names who upgrades
it, and the Plasma, GNOME and Omarchy banners and the daemon's notification say
that instead of offering `--update`
(`panel::tests::every_frontend_with_an_update_button_hides_it_for_a_packaged_install`). After an update the daemon is restarted (systemd) or kickstarted
(launchd).

## Linux

`scripts/install.sh` installs the binaries to `~/.local/bin`, wires waybar
(`--placement=left|right`, persisted in `[waybar] placement`), and enables the
`tokengauge-daemon.service` user unit unless `--no-daemon`. Desktop payloads come
from `scripts/install-{plasma,gnome,omarchy}.sh` or `--install-frontend`.

### install.sh

- It is the `curl | bash` installer only; it never builds a checkout. The body
  is `main`, called on the last line, so a pipe cut short runs nothing.
- The latest tag comes from the `releases/latest` redirect, not the API.
  `--version vX.Y.Z` names a release instead.
- It downloads `SHA256SUMS` first, picks the one `<os>-<arch>.tar.gz` it lists,
  and refuses an archive whose digest differs, or a release with no sums
  (anything before 0.48.0). It runs the extracted `tokengauge --version` before
  installing it.
- `TOKENGAUGE_DOWNLOAD_BASE=<dir>` reads the assets from a directory: the release
  workflow's `verify` job and `crates/tokengauge-waybar/tests/install.rs` run it
  that way.

### Packages

Decision 0005 in [../decisions.md](../decisions.md). `scripts/package.sh` turns a
`linux-<arch>` archive into a `.deb` and an `.rpm` through `packaging/nfpm.yaml`;
`scripts/arch.sh` renders `packaging/arch/tokengauge-bin` (from the archives) and
`packaging/arch/tokengauge` (from the source tarball) and runs `makepkg`.

- Layout: `/usr/bin/tokengauge`, `tokengauge-tui`, `tokengauge-waybar` (a symlink,
  never the archive's real copy), `/usr/lib/systemd/user/tokengauge-daemon.service`
  with `ExecStart` rewritten from `%h/.local/bin` to `/usr/bin`,
  `/usr/share/plasma/plasmoids/org.tokengauge.plasmoid`,
  `/usr/share/gnome-shell/extensions/tokengauge@arzaroth.github.io`.
- The archive carries `systemd/tokengauge-daemon.service` for them.
- The GNOME schemas are compiled at package time, since the archive leaves them
  XML for selvedge and a system-wide extension is never installed through it.
- No Omarchy widget: the shell reads third-party plugins only from
  `~/.config/omarchy/plugins`.
- The binary is the glibc one `cross` builds, whose floor is below Debian 12 and
  Ubuntu 22.04; the `verify` job installs on both to keep it so.
- Asset names (`tokengauge_<v>_amd64.deb`, `tokengauge-<v>-1.x86_64.rpm`,
  `tokengauge-bin-<v>-1-x86_64.pkg.tar.zst`, `tokengauge-v<v>-pkgbuild.tar.gz`)
  contain no `linux-<arch>`, so no 0.22.x updater picks one.

### The release workflow

`release.yml` also runs on a pull request touching it or the packaging, as a
rehearsal: everything a tag builds and checks, unsigned, published nowhere
(`PUBLISH` false). Jobs: the four platform builds, `linux-packages`,
`arch-packages`, `checksums` (signs, then writes `SHA256SUMS` over every asset),
`verify` (checks the sums and signatures, installs each package on a clean image
and runs `scripts/check-package.sh`, runs `install.sh` against the assets), and
`publish`, which needs `verify` and runs only from a tag.

Signing is on once `packaging/tokengauge.asc` is committed: from then on a tag
without `TOKENGAUGE_GPG_KEY` / `TOKENGAUGE_GPG_PASSPHRASE` refuses to publish. It
signs the `.rpm`s (`rpmsign`), each Arch package (`.sig`) and `SHA256SUMS`
(`SHA256SUMS.asc`), and publishes the public key as `tokengauge-release-key.asc`.

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
- `scripts/{package.sh,arch.sh,check-package.sh}`, `packaging/nfpm.yaml`, `packaging/arch/*/PKGBUILD`
- `crates/tokengauge-waybar/src/snapshot.rs` (`upgraded_by`), `crates/tokengauge-waybar/tests/install.rs`
- `.github/workflows/release.yml`, `.github/workflows/ci.yml`
