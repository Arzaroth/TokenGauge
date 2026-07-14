#!/usr/bin/env bash
set -euo pipefail

REPO="${TOKENGAUGE_REPO:-Arzaroth/TokenGauge}"
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
INSTALL_DAEMON=true
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
    --no-daemon) INSTALL_DAEMON=false ;;
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
if [[ -f "$TMP_DIR/tokengauge-popover" ]]; then
  install -m 0755 "$TMP_DIR/tokengauge-popover" "$INSTALL_DIR/tokengauge-popover"
fi

# Provider brand SVG logos for the popover tab strip. Fetched from the repo
# (not bundled in the binary tarball) and recoloured so the monochrome
# (currentColor) marks are visible on the popover's dark background. Best
# effort - the popover falls back to glyph icons when a logo is missing.
ICON_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tokengauge/icons"
ICON_BASE="https://raw.githubusercontent.com/Arzaroth/TokenGauge/main/assets/providers"
ICON_FG="#cdd6f4"
mkdir -p "$ICON_DIR"
for icon in claude codex zai copilot minimax kimi; do
  if curl -fsSL "$ICON_BASE/ProviderIcon-$icon.svg" -o "$TMP_DIR/ProviderIcon-$icon.svg" 2>/dev/null; then
    sed "s/currentColor/$ICON_FG/g" "$TMP_DIR/ProviderIcon-$icon.svg" \
      > "$ICON_DIR/ProviderIcon-$icon.svg"
  fi
done

EXISTING_PLACEMENT=""
if [[ -f "$CONFIG_FILE" ]]; then
  EXISTING_PLACEMENT=$(awk '
    /^\s*\[/         { in_waybar = ($0 ~ /^\s*\[waybar\]\s*$/); next }
    in_waybar && /^\s*placement\s*=/ {
      sub(/.*=\s*"?/, ""); sub(/"?\s*(#.*)?$/, ""); print; exit
    }
  ' "$CONFIG_FILE" 2>/dev/null || true)
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
  tmp=$(mktemp)
  PLACEMENT="$PLACEMENT" awk '
    BEGIN { in_waybar = 0; updated = 0; have_waybar = 0 }
    /^\s*\[waybar\]\s*$/ { in_waybar = 1; have_waybar = 1; print; next }
    /^\s*\[/             { in_waybar = 0; print; next }
    in_waybar && /^\s*placement\s*=/ {
      print "placement = \"" ENVIRON["PLACEMENT"] "\""
      updated = 1
      next
    }
    { print }
    END {
      if (!have_waybar) {
        print ""
        print "[waybar]"
        print "placement = \"" ENVIRON["PLACEMENT"] "\""
      } else if (!updated) {
        # [waybar] section exists but no placement line - append at EOF as a fresh section is risky
        # Re-emit a placement line under a synthetic header is hard in single-pass; second pass handles it
      }
    }
  ' "$CONFIG_FILE" > "$tmp"
  mv "$tmp" "$CONFIG_FILE"
  # Second pass: if [waybar] existed but lacked placement, insert after the header
  if ! grep -qE '^\s*placement\s*=' "$CONFIG_FILE"; then
    tmp=$(mktemp)
    PLACEMENT="$PLACEMENT" awk '
      /^\s*\[waybar\]\s*$/ { print; print "placement = \"" ENVIRON["PLACEMENT"] "\""; next }
      { print }
    ' "$CONFIG_FILE" > "$tmp"
    mv "$tmp" "$CONFIG_FILE"
  fi
fi

# Detect if omarchy is installed
HAS_OMARCHY=false
if command -v omarchy-launch-or-focus-tui >/dev/null 2>&1; then
  HAS_OMARCHY=true
fi

# Detect systemd user session (for daemon mode)
HAS_SYSTEMD_USER=false
if $INSTALL_DAEMON && command -v systemctl >/dev/null 2>&1 && systemctl --user status >/dev/null 2>&1; then
  HAS_SYSTEMD_USER=true
fi

install_daemon_unit() {
  local unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  mkdir -p "$unit_dir"
  cat > "$unit_dir/tokengauge-daemon.service" <<UNIT
[Unit]
Description=TokenGauge daemon (long-lived fetcher + socket server)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart=$INSTALL_DIR/tokengauge-waybar --daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=graphical-session.target
UNIT
  systemctl --user daemon-reload
  # reenable (not enable) so an upgrade from the old WantedBy=default.target
  # drops the stale early-start symlink. Binding to graphical-session.target
  # makes the daemon start *after* the compositor imports WAYLAND/DBUS/BROWSER
  # into the systemd --user env, so notify-send + xdg-open reach the session.
  systemctl --user reenable tokengauge-daemon.service
  systemctl --user restart tokengauge-daemon.service
  success "tokengauge-daemon enabled via systemd --user"
}

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

if $HAS_SYSTEMD_USER; then
  install_daemon_unit
elif $INSTALL_DAEMON; then
  warn "systemd --user not available; skipping daemon setup. waybar will poll every 60s."
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

  # on-click goes through `tokengauge-waybar --click`, which dispatches
  # based on [waybar].click_action in the user's config (tui vs popover).
  module_filter='
    ."custom/tokengauge" = {
      "exec": "tokengauge-waybar",
      "return-type": "json",
      "interval": 60,
      "signal": 8,
      "on-click": "tokengauge-waybar --click",
      "on-click-right": "tokengauge-waybar --refresh",
      "on-click-middle": "tokengauge-waybar --open=dashboard",
      "on-click-backward": "tokengauge-waybar --open=status",
      "on-scroll-up": "tokengauge-waybar --rotate=next",
      "on-scroll-down": "tokengauge-waybar --rotate=prev"
    }
  '

  common_helpers='
      def ensure_array: if . == null then [] elif type == "array" then . else [] end;
  '

  if [[ "$PLACEMENT" == "left" ]]; then
    insert_filter="$common_helpers"'
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
    insert_filter="$common_helpers"'
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
    insert_filter="$common_helpers"'
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
