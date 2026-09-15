#!/usr/bin/env bash
# Assembles the installable frontend payloads under build/.
#
# Every frontend renders the panel the binary resolves, so nothing shared is
# compiled here: this copies three payloads out of the sources, and the GNOME
# extension's TypeScript is the only thing that needs a compiler.
#
# The layout is the release archive's - build/frontends/<payload> - because
# `frontend::payload_in` resolves a checkout's payloads there. Installing the
# GNOME extension's source directory would land TypeScript in
# ~/.local/share/gnome-shell/extensions and the shell would refuse it.
#
# The GSettings schemas are deliberately left as XML: the compiled blob is
# built on the machine that runs the extension, by `install_into` or by
# scripts/install-gnome.sh, and a blob built here would ship in the archive.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$REPO_DIR/build"
FRONTENDS="$BUILD_DIR/frontends"
TSC="$REPO_DIR/node_modules/.bin/tsc"

PLASMOID_ID="org.tokengauge.plasmoid"
EXTENSION_UUID="tokengauge@arzaroth.github.io"
PLUGIN_ID="arzaroth.tokengauge"

# scripts/install-gnome.sh runs this from a fresh clone, so the toolchain is
# fetched here rather than left as a step to remember. pnpm owns the lockfile;
# npm is the fallback because it is the one package manager an end user is
# guaranteed to have. That path is best-effort: npm cannot read pnpm-lock.yaml,
# so it re-resolves the ranges in package.json and may pick a different patch
# than CI built with. --no-package-lock keeps it from leaving a second lockfile
# behind.
if [[ ! -x $TSC ]]; then
  echo "build: installing the TypeScript toolchain" >&2
  if command -v pnpm >/dev/null 2>&1; then
    (cd "$REPO_DIR" && pnpm install --frozen-lockfile)
  elif command -v npm >/dev/null 2>&1; then
    echo "build: pnpm not found - resolving with npm, which cannot read pnpm-lock.yaml" >&2
    (cd "$REPO_DIR" && npm install --no-package-lock)
  else
    echo "build: pnpm or npm is required to compile the TypeScript sources" >&2
    exit 1
  fi
fi

if [[ ! -x $TSC ]]; then
  echo "build: $TSC is still missing after installing the toolchain" >&2
  exit 1
fi

rm -rf "$BUILD_DIR"
mkdir -p "$FRONTENDS/plasma" "$FRONTENDS/gnome" "$FRONTENDS/omarchy"

# ---- TypeScript -----------------------------------------------------------
"$TSC" -p "$REPO_DIR/tsconfig.gnome.json"

# ---- Plasma ---------------------------------------------------------------
cp -r "$REPO_DIR/plasma/$PLASMOID_ID" "$FRONTENDS/plasma/$PLASMOID_ID"

# ---- GNOME ----------------------------------------------------------------
cp -r "$REPO_DIR/gnome/$EXTENSION_UUID" "$FRONTENDS/gnome/$EXTENSION_UUID"
# The .ts sources are the compiler's input, not part of the package.
rm -f "$FRONTENDS/gnome/$EXTENSION_UUID"/*.ts
cp "$BUILD_DIR/.ts/gnome/gnome/$EXTENSION_UUID"/*.js "$FRONTENDS/gnome/$EXTENSION_UUID/"

# ---- Omarchy --------------------------------------------------------------
cp -r "$REPO_DIR/omarchy/$PLUGIN_ID" "$FRONTENDS/omarchy/$PLUGIN_ID"

echo "Built:"
echo "  $FRONTENDS/plasma/$PLASMOID_ID"
echo "  $FRONTENDS/gnome/$EXTENSION_UUID"
echo "  $FRONTENDS/omarchy/$PLUGIN_ID"
