#!/usr/bin/env bash
# Install the TokenGauge KDE Plasma 6 applet from a local checkout.
# Builds the release binaries, drops the provider logos where the core expects
# them, and registers the plasmoid with kpackagetool6. The Waybar module is
# untouched - this is an additive frontend.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_DIR/tokengauge/icons"
PLASMOID_DIR="$REPO_DIR/plasma/org.tokengauge.plasmoid"

if [[ -t 1 ]]; then B="\033[0;34m"; G="\033[0;32m"; Y="\033[0;33m"; R="\033[0;31m"; Z="\033[0m"; else B=""; G=""; Y=""; R=""; Z=""; fi
info()    { printf '%b\n' "${B}$*${Z}"; }
success() { printf '%b\n' "${G}$*${Z}"; }
warn()    { printf '%b\n' "${Y}$*${Z}"; }
fail()    { printf '%b\n' "${R}$*${Z}" >&2; }

command -v kpackagetool6 >/dev/null 2>&1 || {
  fail "kpackagetool6 not found - this needs KDE Plasma 6."
  exit 1
}
command -v cargo >/dev/null 2>&1 || { fail "cargo not found - install Rust to build."; exit 1; }

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

info "Registering the Plasma applet..."
if kpackagetool6 --type Plasma/Applet --show org.tokengauge.plasmoid >/dev/null 2>&1; then
  kpackagetool6 --type Plasma/Applet --upgrade "$PLASMOID_DIR"
else
  kpackagetool6 --type Plasma/Applet --install "$PLASMOID_DIR"
fi

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) warn "Note: $INSTALL_DIR is not on your PATH. Add it or set the applet's binary path in its settings." ;;
esac

success "Done."
echo
echo "Add the widget: right-click a panel or the desktop -> Add Widgets -> search \"TokenGauge\"."
echo "If it does not appear yet, restart Plasma: kquitapp6 plasmashell && kstart plasmashell"
echo
echo "The applet reads config from ~/.config/tokengauge/config.toml (shared with the Waybar module)."
