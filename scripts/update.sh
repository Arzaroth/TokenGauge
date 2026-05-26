#!/usr/bin/env bash
set -euo pipefail

REPO="${TOKENGAUGE_REPO:-Arzaroth/TokenGauge}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
TMP_DIR=$(mktemp -d)

if [[ -t 1 ]]; then
  COLOR_RESET="\033[0m"
  COLOR_GREEN="\033[0;32m"
  COLOR_YELLOW="\033[0;33m"
  COLOR_BLUE="\033[0;34m"
  COLOR_RED="\033[0;31m"
else
  COLOR_RESET=""
  COLOR_GREEN=""
  COLOR_YELLOW=""
  COLOR_BLUE=""
  COLOR_RED=""
fi

info() {
  printf '%b\n' "${COLOR_BLUE}$*${COLOR_RESET}"
}

success() {
  printf '%b\n' "${COLOR_GREEN}$*${COLOR_RESET}"
}

warn() {
  printf '%b\n' "${COLOR_YELLOW}$*${COLOR_RESET}"
}

fail() {
  printf '%b\n' "${COLOR_RED}$*${COLOR_RESET}" >&2
}

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

get_latest_tag() {
  local repo="$1"
  local api_json
  info "Fetching latest release for $repo"
  api_json=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest")

  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$api_json" | jq -r '.tag_name // empty'
  else
    fail "Missing jq for JSON parsing"
    return 1
  fi
}

latest=$(get_latest_tag "$REPO" | tail -n 1)
if [[ -z "$latest" ]]; then
  fail "Failed to find latest release for $REPO"
  exit 1
fi

arch=$(uname -m)
case "$arch" in
  x86_64) asset_arch="x86_64" ;;
  aarch64|arm64) asset_arch="aarch64" ;;
  *) fail "Unsupported arch: $arch"; exit 1 ;;
 esac

asset="tokengauge-$latest-linux-$asset_arch.tar.gz"
url="https://github.com/$REPO/releases/download/$latest/$asset"

info "Downloading TokenGauge $latest"
curl -fL "$url" -o "$TMP_DIR/$asset"

tar -xzf "$TMP_DIR/$asset" -C "$TMP_DIR"

install -m 0755 "$TMP_DIR/tokengauge-waybar" "$INSTALL_DIR/tokengauge-waybar"
install -m 0755 "$TMP_DIR/tokengauge-tui" "$INSTALL_DIR/tokengauge-tui"

success "Updated tokengauge to $latest in $INSTALL_DIR"

if command -v omarchy-restart-waybar >/dev/null 2>&1; then
  info "Restart Waybar: omarchy-restart-waybar"
else
  info "Restart Waybar to see the changes."
fi
