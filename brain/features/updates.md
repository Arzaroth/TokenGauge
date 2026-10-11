# Updates

Self-update from GitHub releases, the daemon's periodic check, and the Update
button every desktop frontend shows. The machinery is selvedge; see
[../architecture/distribution.md](../architecture/distribution.md).

- `tokengauge --update` downloads the archive matching the platform
  (`linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`,
  `windows-x86_64`) and swaps the binaries and installed desktop payloads;
  `--check-update` prints the latest release as JSON. On Windows the TUI carries
  both flags; the tray's **Update TokenGauge** item runs the updater.
- The daemon checks every `update.check_interval_secs` (21600) and notifies once;
  `update.check = false` opts out. The cached result (`tokengauge-update.json`
  beside the snapshot) is what the GUI frontends read to show their Update button,
  which runs `--update`.
- After a Linux update the daemon is restarted (or the `systemctl --user restart`
  command printed); on macOS the `org.tokengauge.daemon` LaunchAgent is
  kickstarted. Under an MSI install the upgrade goes through `msiexec`. The macOS
  `.app` never self-updates; its Update item opens the release page.
- `TOKENGAUGE_REPO=owner/repo` updates from a fork. `scripts/update.sh` remains
  for shell installs.

## Sources

- `crates/tokengauge-core/src/project.rs`, `crates/tokengauge-core/src/statefiles.rs` (`update_status_path`)
- `crates/tokengauge-waybar/src/main.rs` (`--update`, `--check-update`, `--install-frontend`), `crates/tokengauge-waybar/src/daemon.rs`
- `crates/tokengauge-tray/src/main.rs`, `scripts/update.sh`
