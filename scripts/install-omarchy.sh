#!/usr/bin/env bash
# Install the TokenGauge Omarchy shell plugin from a local checkout.
# Builds the release binaries, drops the provider logos where the core expects
# them, and copies the bar widget into ~/.config/omarchy/plugins/. The Waybar
# module, the Plasma applet, and the GNOME extension are untouched - this is an
# additive frontend that shares the same config, cache, and daemon.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}"
ICON_DIR="$DATA_DIR/tokengauge/icons"

PLUGIN_ID="arzaroth.tokengauge"
PLUGIN_SRC="$REPO_DIR/omarchy/$PLUGIN_ID"
PLUGIN_DIR="$HOME/.config/omarchy/plugins/$PLUGIN_ID"
PLACEMENT="${TOKENGAUGE_PLACEMENT:-right}"

if [[ -t 1 ]]; then B="\033[0;34m"; G="\033[0;32m"; Y="\033[0;33m"; R="\033[0;31m"; Z="\033[0m"; else B=""; G=""; Y=""; R=""; Z=""; fi
info()    { printf '%b\n' "${B}$*${Z}"; }
success() { printf '%b\n' "${G}$*${Z}"; }
warn()    { printf '%b\n' "${Y}$*${Z}"; }
fail()    { printf '%b\n' "${R}$*${Z}" >&2; }

while [[ $# -gt 0 ]]; do
  case "$1" in
  --placement=*) PLACEMENT="${1#*=}" ;;
  --placement)
    # set -u would abort on an unset $2 before the check below ever ran.
    [[ $# -ge 2 ]] || { fail "--placement needs a value (left, center, or right)"; exit 1; }
    PLACEMENT="$2"
    shift
    ;;
  -h | --help)
    echo "Usage: install-omarchy.sh [--placement=left|center|right]"
    exit 0
    ;;
  *) fail "unknown option: $1"; exit 1 ;;
  esac
  shift
done

case "$PLACEMENT" in
left | center | right) ;;
*) fail "placement must be left, center, or right (got '$PLACEMENT')"; exit 1 ;;
esac

command -v omarchy-shell >/dev/null 2>&1 || {
  fail "omarchy-shell not found - this needs Omarchy 4 or newer (the Quickshell bar)."
  exit 1
}
command -v omarchy-plugin-validate >/dev/null 2>&1 || {
  fail "omarchy-plugin-validate not found - this needs Omarchy 4 or newer."
  exit 1
}
command -v cargo >/dev/null 2>&1 || { fail "cargo not found - install Rust to build."; exit 1; }
[[ -d $PLUGIN_SRC ]] || { fail "plugin source not found: $PLUGIN_SRC"; exit 1; }

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
  fail "No provider icons found in $REPO_DIR/assets/providers"
  exit 1
fi
install -m 0644 "${icons[@]}" "$ICON_DIR/"

# The daemon keeps serving from the binary it started with, so a fresh build
# only reaches the widget once the unit restarts.
if systemctl --user is-active --quiet tokengauge-daemon 2>/dev/null; then
  info "Restarting the TokenGauge daemon..."
  systemctl --user restart tokengauge-daemon || warn "Could not restart tokengauge-daemon."
fi

info "Validating the plugin manifest..."
omarchy-plugin-validate "$PLUGIN_SRC"

# The shell's plugin registry refuses symlinks anywhere inside a plugin folder,
# so this is a copy rather than a link back into the checkout. Re-run the script
# to pick up local edits.
info "Installing the plugin to $PLUGIN_DIR"
mkdir -p "$(dirname "$PLUGIN_DIR")"
rm -rf "$PLUGIN_DIR"
cp -aL "$PLUGIN_SRC" "$PLUGIN_DIR"

omarchy-shell -q shell rescanPlugins >/dev/null 2>&1 || true

if omarchy-plugin-list 2>/dev/null | grep -q "^$PLUGIN_ID .*enabled"; then
  info "Plugin already enabled; leaving its bar placement alone."
else
  info "Enabling the widget in the $PLACEMENT section..."
  omarchy-plugin-enable "$PLUGIN_ID" "$PLACEMENT"
fi

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*) warn "Note: $INSTALL_DIR is not on your PATH. The widget prepends it anyway, but the CLI will not resolve." ;;
esac

# The registry reloads the manifest on change but keeps the already-instantiated
# widget, so QML edits only take effect on a restart.
info "Restarting the shell so the widget loads..."
omarchy-restart-shell >/dev/null 2>&1 || warn "Could not restart the shell - run 'omarchy-restart-shell' yourself."

success "Done."
echo
echo "Bar icon:  left = panel, right = refresh, middle = next provider."
echo "Panel:     h/l switch provider, j/k scroll, r or Enter refresh, Esc closes."
echo "IPC:       omarchy-shell $PLUGIN_ID <open|close|toggle|refresh|next>"
echo
echo "Settings live on the widget's entry in ~/.config/omarchy/shell.json:"
echo "  omarchy bar set $PLUGIN_ID refreshIntervalSec 300 --json"
echo
echo "Provider selection, thresholds, and everything else stay in"
echo "~/.config/tokengauge/config.toml, shared with the Waybar module."
