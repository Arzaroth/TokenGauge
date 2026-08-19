#!/usr/bin/env bash
# Install the TokenGauge GNOME Shell extension from a local checkout.
# Builds the release binaries, drops the provider logos where the core expects
# them, and installs the extension into the user extensions dir. The Waybar
# module is untouched - this is an additive frontend.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_DIR/tokengauge/icons"
UUID="tokengauge@arzaroth.github.io"
SRC_DIR="$REPO_DIR/gnome/$UUID"
EXT_DIR="$DATA_DIR/gnome-shell/extensions/$UUID"

if [[ -t 1 ]]; then B="\033[0;34m"; G="\033[0;32m"; Y="\033[0;33m"; R="\033[0;31m"; Z="\033[0m"; else B=""; G=""; Y=""; R=""; Z=""; fi
info()    { printf '%b\n' "${B}$*${Z}"; }
success() { printf '%b\n' "${G}$*${Z}"; }
warn()    { printf '%b\n' "${Y}$*${Z}"; }
fail()    { printf '%b\n' "${R}$*${Z}" >&2; }

command -v glib-compile-schemas >/dev/null 2>&1 || {
  fail "glib-compile-schemas not found - install glib2 development tools."
  exit 1
}
command -v cargo >/dev/null 2>&1 || { fail "cargo not found - install Rust to build."; exit 1; }

if ! command -v gnome-shell >/dev/null 2>&1; then
  warn "gnome-shell not found on PATH - installing anyway."
fi

info "Building release binaries..."
cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" \
  -p tokengauge-waybar -p tokengauge-tui -p tokengauge-popover

info "Installing binaries to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
for bin in tokengauge-waybar tokengauge-tui tokengauge-popover; do
  install -m 0755 "$REPO_DIR/target/release/$bin" "$INSTALL_DIR/$bin"
done

info "Installing provider logos to $ICON_DIR"
mkdir -p "$ICON_DIR"
shopt -s nullglob
icons=("$REPO_DIR"/assets/providers/ProviderIcon-*.svg)
shopt -u nullglob
if [[ ${#icons[@]} -eq 0 ]]; then
  fail "No provider icons found in $REPO_DIR/assets/providers"; exit 1
fi
install -m 0644 "${icons[@]}" "$ICON_DIR/"

info "Installing the extension to $EXT_DIR"
rm -rf "$EXT_DIR"
mkdir -p "$EXT_DIR"
cp -r "$SRC_DIR"/. "$EXT_DIR/"
glib-compile-schemas "$EXT_DIR/schemas"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "Note: $INSTALL_DIR is not on your PATH. The extension only prepends ~/.local/bin, ~/bin and /usr/local/bin, so set its waybar-binary preference to $INSTALL_DIR/tokengauge-waybar." ;;
esac

if command -v gnome-extensions >/dev/null 2>&1; then
  if gnome-extensions enable "$UUID" 2>/dev/null; then
    success "Extension enabled."
  else
    warn "Could not enable the extension yet - restart the shell first, then: gnome-extensions enable $UUID"
  fi
else
  warn "gnome-extensions not found - enable it after restarting the shell: gnome-extensions enable $UUID"
fi

success "Done."
echo
if [[ "${XDG_SESSION_TYPE:-}" == "wayland" ]]; then
  echo "Wayland cannot restart the shell in place - log out and back in to load the extension."
else
  echo "Restart the shell to load it: Alt+F2, type 'r', Enter."
fi
echo
echo "The extension reads config from ~/.config/tokengauge/config.toml (shared with the Waybar module)."
