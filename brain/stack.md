# Stack

The technologies and versions TokenGauge is built on. Bump these when a
`Cargo.toml`, `package.json` or a frontend manifest changes.

Sources of truth: `Cargo.toml` (workspace), `crates/*/Cargo.toml`,
`package.json`, `tsconfig.gnome.json`, `gnome/*/metadata.json`,
`plasma/*/metadata.json`, `omarchy/*/manifest.json`, `.github/workflows/ci.yml`.

## Language and toolchain

- **Rust**, edition 2024, `rust-version = "1.89"` (MSRV for
  `File::try_lock`); CI builds on stable. Four workspace crates, all at the same
  version (0.43.0 at the time of writing): `tokengauge-core`,
  `tokengauge-waybar` (binary `tokengauge`), `tokengauge-tui`, `tokengauge-tray`.
  `default-members` is core + TUI so a bare `cargo build` works on Windows.
- **TypeScript** ~5.7 against `@girs/gnome-shell` 50 for the GNOME extension
  (`pnpm typecheck` = `tsc -p tsconfig.gnome.json --noEmit`), compiled by
  `scripts/build.sh`.
- **QML** (Qt 6) for the Plasma 6 applet and the omarchy-shell (Quickshell)
  widget; linted with `qmllint`.
- **pnpm** 10.33.0 (`packageManager`), Node 22 in CI.

## Core crate dependencies

- `serde` / `serde_json`, `toml` + `toml_edit` (config edits keep comments),
  `chrono`, `anyhow`, `dirs`.
- `reqwest` 0.13 blocking with rustls (provider APIs, LiteLLM, S3); no async
  runtime anywhere.
- `sha2`, `hmac`, `chacha20poly1305`, `flate2`: device ids, the sync envelope,
  SigV4.
- `keyring` 3, native backends only, on macOS and Windows (Claude's OS credential
  store); nothing on Linux, so no dbus.
- `selvedge` (git, pinned by tag) without default features; `self-update`
  feature maps to `selvedge/self-update`. See
  [architecture/distribution.md](architecture/distribution.md).

## Frontend crates

- **TUI**: `ratatui` 0.29 + `crossterm`.
- **Tray**: `eframe` 0.35 (egui) + `tray-icon` 0.24, gated to Windows and macOS.
- **Waybar**: `clap` 4.5; pango markup output; Unix socket daemon.

## External data

- Provider HTTP APIs (one module each, see [features/providers.md](features/providers.md)).
- LiteLLM's model price table (fetched, cached, vendored `prices.json` and
  `price-archive.json`).
- ccusage, optional, found on `PATH` / `bunx` / `npx`.

## Packaging

GitHub Actions (`ci.yml` on pull requests, `release.yml` on `v*` tags): Linux
x86_64/aarch64 tarballs with compiled frontend payloads, a Windows zip and a
per-user WiX MSI, macOS per-arch tarballs and a signed, notarized universal DMG.
See [operations.md](operations.md).

## Sources

- `Cargo.toml`, `crates/tokengauge-core/Cargo.toml`, `crates/tokengauge-tui/Cargo.toml`, `crates/tokengauge-tray/Cargo.toml`, `crates/tokengauge-waybar/Cargo.toml`
- `package.json`, `tsconfig.gnome.json`, `.github/workflows/ci.yml`
