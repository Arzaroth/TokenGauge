#!/usr/bin/env bash
set -euo pipefail

REPO="${TOKENGAUGE_REPO:-oorestisime/TokenGauge}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/tokengauge"
CONFIG_FILE="$CONFIG_DIR/config.toml"
WAYBAR_CONFIG="$HOME/.config/waybar/config.jsonc"
BACKUP_PATH=""
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

PLACEMENT_OVERRIDE="${TOKENGAUGE_PLACEMENT:-}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --placement=*) PLACEMENT_OVERRIDE="${1#*=}" ;;
    --placement)
      if [[ $# -lt 2 ]]; then
        fail "--placement requires an argument (left|right)"
        exit 1
      fi
      PLACEMENT_OVERRIDE="$2"
      shift
      ;;
    *) ;;
  esac
  shift
done
case "$PLACEMENT_OVERRIDE" in
  left|right|"") ;;
  *)
    warn "Invalid --placement '$PLACEMENT_OVERRIDE'; ignoring"
    PLACEMENT_OVERRIDE=""
    ;;
esac

mkdir -p "$INSTALL_DIR" "$CONFIG_DIR"

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

arch=$(uname -m)
case "$arch" in
  x86_64) asset_arch="x86_64" ;;
  aarch64|arm64) asset_arch="aarch64" ;;
  *) echo "Unsupported arch: $arch" >&2; exit 1 ;;
esac

latest=$(get_latest_tag "$REPO" | tail -n 1)
if [[ -z "$latest" ]]; then
  fail "Failed to find latest release for $REPO"
  exit 1
fi

asset="tokengauge-$latest-linux-$asset_arch.tar.gz"
url="https://github.com/$REPO/releases/download/$latest/$asset"

info "Downloading TokenGauge $latest"
curl -fL "$url" -o "$TMP_DIR/$asset"

tar -xzf "$TMP_DIR/$asset" -C "$TMP_DIR"

install -m 0755 "$TMP_DIR/tokengauge-waybar" "$INSTALL_DIR/tokengauge-waybar"
install -m 0755 "$TMP_DIR/tokengauge-tui" "$INSTALL_DIR/tokengauge-tui"

EXISTING_PLACEMENT=""
if [[ -f "$CONFIG_FILE" ]]; then
  EXISTING_PLACEMENT=$(grep -E '^\s*placement\s*=' "$CONFIG_FILE" 2>/dev/null \
    | sed -E 's/.*=\s*"?([a-z]+)"?.*/\1/' | head -n1 || true)
fi
PLACEMENT="${PLACEMENT_OVERRIDE:-${EXISTING_PLACEMENT:-right}}"
info "Placement: $PLACEMENT"

if [[ ! -f "$CONFIG_FILE" ]]; then
  cat <<TOML > "$CONFIG_FILE"
# TokenGauge configuration
codexbar_bin = "codexbar"
source = "oauth"
refresh_secs = 600
cache_file = "/tmp/tokengauge-usage.json"

[providers]
codex = true
claude = true

[waybar]
window = "daily" # daily | weekly
placement = "$PLACEMENT" # left | right
TOML
else
  if grep -qE '^\s*placement\s*=' "$CONFIG_FILE"; then
    sed -i -E "s@^(\s*placement\s*=).*@\1 \"$PLACEMENT\"@" "$CONFIG_FILE"
  elif grep -qE '^\s*\[waybar\]' "$CONFIG_FILE"; then
    sed -i -E "/^\s*\[waybar\]/a placement = \"$PLACEMENT\"" "$CONFIG_FILE"
  else
    printf '\n[waybar]\nplacement = "%s"\n' "$PLACEMENT" >> "$CONFIG_FILE"
  fi
fi

# Detect if omarchy is installed
HAS_OMARCHY=false
if command -v omarchy-launch-or-focus-tui >/dev/null 2>&1; then
  HAS_OMARCHY=true
fi

install_codexbar() {
  local codex_repo="steipete/CodexBar"
  local codex_latest
  codex_latest=$(get_latest_tag "$codex_repo")
  if [[ -z "$codex_latest" ]]; then
    echo "Failed to find latest release for $codex_repo" >&2
    return 1
  fi

  local codex_asset="CodexBarCLI-$codex_latest-linux-$asset_arch.tar.gz"
  local codex_url="https://github.com/$codex_repo/releases/download/$codex_latest/$codex_asset"
  local codex_tmp="$TMP_DIR/codexbar"

  mkdir -p "$codex_tmp"
  curl -fL "$codex_url" -o "$codex_tmp/$codex_asset"
  tar -xzf "$codex_tmp/$codex_asset" -C "$codex_tmp"

  if [[ -f "$codex_tmp/CodexBarCLI" ]]; then
    install -m 0755 "$codex_tmp/CodexBarCLI" "$INSTALL_DIR/CodexBarCLI"
  fi
  if [[ -f "$codex_tmp/codexbar" ]]; then
    install -m 0755 "$codex_tmp/codexbar" "$INSTALL_DIR/codexbar"
  else
    ln -sf "$INSTALL_DIR/CodexBarCLI" "$INSTALL_DIR/codexbar"
  fi

  success "Installed CodexBarCLI $codex_latest to $INSTALL_DIR"
}

if ! command -v codexbar >/dev/null 2>&1; then
  warn "CodexBar CLI not found. Installing..."
  if ! install_codexbar; then
    warn "Failed to install CodexBar CLI. Install manually from https://github.com/steipete/CodexBar/releases"
  fi
fi

if [[ -f "$WAYBAR_CONFIG" ]]; then
  backup="$WAYBAR_CONFIG.bak.tokengauge.$(date +%s)"
  BACKUP_PATH="$backup"
  cp "$WAYBAR_CONFIG" "$backup"
  tmp_config=$(mktemp)

  dedup_filter='
    def strip: if . == null then . else map(select(. != "custom/tokengauge")) end;
    ."modules-left" = (."modules-left" | strip)
    | ."modules-right" = (."modules-right" | strip)
  '

  if $HAS_OMARCHY; then
    module_filter='
      ."custom/tokengauge" = {
        "exec": "tokengauge-waybar",
        "return-type": "json",
        "interval": 60,
        "on-click": "omarchy-launch-or-focus-tui tokengauge-tui"
      }
    '
  else
    module_filter='
      ."custom/tokengauge" = {
        "exec": "tokengauge-waybar",
        "return-type": "json",
        "interval": 60
      }
    '
  fi

  if [[ "$PLACEMENT" == "left" ]]; then
    insert_filter='
      def ensure_array: if . == null then [] elif type == "array" then . else [] end;
      def insert_after($arr; $item; $after):
        (($arr | index($after)) as $idx
         | if $idx == null then ([$item] + $arr)
           else ($arr[:$idx+1] + [$item] + $arr[$idx+1:])
           end);
      ."modules-left" = (
        ."modules-left" | ensure_array | insert_after(.; "custom/tokengauge"; "hyprland/workspaces")
      )
    '
  elif $HAS_OMARCHY; then
    insert_filter='
      def ensure_array: if . == null then [] elif type == "array" then . else [] end;
      def add_before($arr; $item; $before):
        (($arr | index($before)) as $idx
         | if $idx == null then ($arr + [$item])
           else ($arr[:$idx] + [$item] + $arr[$idx:])
           end);
      ."modules-right" = (
        ."modules-right" | ensure_array | add_before(.; "custom/tokengauge"; "group/tray-expander")
      )
    '
  else
    insert_filter='
      def ensure_array: if . == null then [] elif type == "array" then . else [] end;
      ."modules-right" = (."modules-right" | ensure_array | . + ["custom/tokengauge"])
    '
  fi

  jq_filter="$dedup_filter | $module_filter | $insert_filter"

  if jq --indent 2 "$jq_filter" "$WAYBAR_CONFIG" > "$tmp_config"; then
    mv "$tmp_config" "$WAYBAR_CONFIG"
  else
    rm -f "$tmp_config"
    fail "Failed to patch Waybar config (invalid JSON)."
    warn "Restore with: cp '$BACKUP_PATH' '$WAYBAR_CONFIG'"
  fi
fi

if [[ -n "$BACKUP_PATH" ]]; then
  warn "Restore Waybar with: cp '$BACKUP_PATH' '$WAYBAR_CONFIG'"
fi
info "Installed tokengauge to $INSTALL_DIR"

if $HAS_OMARCHY; then
  success "Restart Waybar: omarchy-restart-waybar"
else
  success "Restart Waybar to see the module."
  echo ""
  info "To open the TUI on click, add \"on-click\" to your Waybar config:"
  echo "  ghostty -e tokengauge-tui"
  echo "  alacritty -e tokengauge-tui"
fi
