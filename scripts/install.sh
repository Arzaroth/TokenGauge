#!/usr/bin/env bash
# Usage: curl -fsSL https://raw.githubusercontent.com/Arzaroth/TokenGauge/master/scripts/install.sh | bash
#        ... | bash -s -- [--placement=left|right] [--no-daemon] [--no-tray] [--version vX.Y.Z]
#
# Installs a GitHub release (the latest unless --version names one), checked
# against the SHA256SUMS published beside it.
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

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Downloads the release archive for this machine, checked against the
# release's SHA256SUMS, and unpacks it into $TMP_DIR. TOKENGAUGE_DOWNLOAD_BASE
# points it at a directory of assets instead, which is how the release
# workflow runs this script before anything is published.
fetch_release() {
  local base="${TOKENGAUGE_DOWNLOAD_BASE:-}" tag="$VERSION" latest pattern archive expected actual

  if [[ -z "$tag" && -z "$base" ]]; then
    # The redirect names the tag, without the API's 60 requests an hour.
    latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
    tag="${latest##*/}"
    if [[ "$tag" != v* ]]; then
      fail "Failed to find latest release for $REPO"
      exit 1
    fi
  fi
  case "$base" in
    "") base="https://github.com/$REPO/releases/download/$tag" ;;
    /*) base="file://$base" ;;
  esac

  if ! curl -fsSL "$base/SHA256SUMS" -o "$TMP_DIR/SHA256SUMS"; then
    fail "No SHA256SUMS in $base: releases before 0.47.0 do not publish one"
    exit 1
  fi
  pattern="tokengauge-${tag:-v[0-9][^ ]*}-$asset_os-$asset_arch\.tar\.gz"
  archive=$(sed -n "s/^[0-9a-f]*  \($pattern\)$/\1/p" "$TMP_DIR/SHA256SUMS")
  if [[ -z "$archive" ]]; then
    fail "No $asset_os-$asset_arch archive in $base"
    exit 1
  fi
  if [[ $(wc -l <<<"$archive") -ne 1 ]]; then
    fail "Several $asset_os-$asset_arch archives in $base: pick one with --version"
    exit 1
  fi

  info "Downloading $archive"
  curl -fsSL "$base/$archive" -o "$TMP_DIR/$archive"
  expected=$(grep "  $archive\$" "$TMP_DIR/SHA256SUMS" | cut -d' ' -f1)
  actual=$(sha256 "$TMP_DIR/$archive")
  if [[ -z "$expected" || "$expected" != "$actual" ]]; then
    fail "$archive does not match its SHA256SUMS entry"
    exit 1
  fi

  tar -xzf "$TMP_DIR/$archive" -C "$TMP_DIR"
  if ! "$TMP_DIR/tokengauge" --version >/dev/null 2>&1; then
    fail "The downloaded tokengauge does not run on this system"
    exit 1
  fi
}

# Everything runs from here, called on the last line, so a download cut short
# by the pipe runs nothing at all. Not indented: the heredocs below are
# written out as they stand.
main() {

PLACEMENT_OVERRIDE="${TOKENGAUGE_PLACEMENT:-}"
INSTALL_DAEMON=true
INSTALL_TRAY=true
VERSION=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version=*) VERSION="${1#*=}"; VERSION="v${VERSION#v}" ;;
    --version)
      if [[ $# -lt 2 ]]; then
        fail "--version requires a tag (vX.Y.Z)"
        exit 1
      fi
      VERSION="v${2#v}"
      shift
      ;;
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
    --no-tray) INSTALL_TRAY=false ;;
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

case "$(uname -s)" in
  Linux) asset_os="linux"; IS_MACOS=false ;;
  Darwin) asset_os="macos"; IS_MACOS=true ;;
  *) echo "Unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

arch=$(uname -m)
case "$arch" in
  x86_64) asset_arch="x86_64" ;;
  aarch64|arm64) asset_arch="aarch64" ;;
  *) echo "Unsupported arch: $arch" >&2; exit 1 ;;
esac

fetch_release

install -m 0755 "$TMP_DIR/tokengauge" "$INSTALL_DIR/tokengauge"
# The binary was called tokengauge-waybar before 0.23.0. Keep the old name
# working for an existing waybar config, systemd unit or frontend setting.
rm -f "$INSTALL_DIR/tokengauge-waybar"
ln -s tokengauge "$INSTALL_DIR/tokengauge-waybar"
install -m 0755 "$TMP_DIR/tokengauge-tui" "$INSTALL_DIR/tokengauge-tui"

# A path with & or < in it is otherwise invalid XML, and launchctl refuses the
# whole plist after the old agent has already been booted out.
xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g'
}

plist_env() {
  printf '    <key>%s</key>\n    <string>%s</string>\n' "$1" "$(xml_escape "$2")"
}

# launchd starts what a LaunchAgent names with a bare PATH, so the agent sets
# its own - with Homebrew's prefixes, where bunx and npx live for ccusage. An
# XDG directory the installer ran under is carried too, or the agents would
# read a different config and snapshot from the one just written.
install_launch_agent() {
  local label="$1" plist="$HOME/Library/LaunchAgents/$1.plist"
  shift
  mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
  {
    cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$(xml_escape "$label")</string>
  <key>ProgramArguments</key>
  <array>
PLIST
    for arg in "$@"; do
      printf '    <string>%s</string>\n' "$(xml_escape "$arg")"
    done
    cat <<PLIST
  </array>
  <key>EnvironmentVariables</key>
  <dict>
PLIST
    plist_env PATH "$INSTALL_DIR:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
    [[ -n "${XDG_CONFIG_HOME:-}" ]] && plist_env XDG_CONFIG_HOME "$XDG_CONFIG_HOME"
    [[ -n "${XDG_STATE_HOME:-}" ]] && plist_env XDG_STATE_HOME "$XDG_STATE_HOME"
    cat <<PLIST
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>StandardErrorPath</key>
  <string>$(xml_escape "$HOME/Library/Logs/$label.log")</string>
PLIST
    [[ "$label" == *.daemon ]] && printf '  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n'
    printf '</dict>\n</plist>\n'
  } > "$plist"
  # bootout first so a reinstall picks up the new binary and the new plist.
  # It returns before the old job is gone, and bootstrapping over a job still
  # being torn down fails, so give it a moment.
  launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
  local attempt
  for attempt in 1 2 3 4 5; do
    launchctl bootstrap "gui/$(id -u)" "$plist" 2>/dev/null && return 0
    sleep 1
  done
  warn "Could not load $label; it will start at next login ($plist)"
  return 1
}

if $IS_MACOS; then
  install -m 0755 "$TMP_DIR/tokengauge-tray" "$INSTALL_DIR/tokengauge-tray"
  if [[ ! -f "$CONFIG_FILE" ]]; then
    cat <<TOML > "$CONFIG_FILE"
# TokenGauge configuration
refresh_secs = 600

[providers]
codex = true
claude = true
TOML
  fi
  if $INSTALL_DAEMON && install_launch_agent org.tokengauge.daemon "$INSTALL_DIR/tokengauge" --daemon; then
    success "tokengauge-daemon enabled via launchd"
  fi
  if $INSTALL_TRAY && install_launch_agent org.tokengauge.tray "$INSTALL_DIR/tokengauge-tray" --hidden; then
    success "TokenGauge is in the menu bar, and starts at login"
  fi
  info "Installed tokengauge to $INSTALL_DIR"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) warn "$INSTALL_DIR is not on your PATH. Add it to run tokengauge and tokengauge-tui from a shell." ;;
  esac
  exit 0
fi

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
refresh_secs = 600

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
# The systemd user manager's PATH omits ~/.local/bin, where ccusage and the
# tokengauge binaries usually live; without this the daemon's ccusage cost
# fetches fail and rows show usage without cost enrichment (usage limits are
# fetched natively and are unaffected).
Environment=PATH=$INSTALL_DIR:/usr/local/bin:/usr/bin:/bin
ExecStart=$INSTALL_DIR/tokengauge --daemon
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

  # on-click goes through `tokengauge --click`, which dispatches
  # based on [waybar].click_action in the user's config.
  module_filter='
    ."custom/tokengauge" = {
      "exec": "tokengauge",
      "return-type": "json",
      "interval": 60,
      "signal": 8,
      "on-click": "tokengauge --click",
      "on-click-right": "tokengauge --refresh",
      "on-click-middle": "tokengauge --open=dashboard",
      "on-click-backward": "tokengauge --open=status",
      "on-scroll-up": "tokengauge --rotate=next",
      "on-scroll-down": "tokengauge --rotate=prev"
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

if [[ "${XDG_CURRENT_DESKTOP:-}" == *KDE* ]] || command -v kpackagetool6 >/dev/null 2>&1; then
  echo ""
  info "KDE Plasma detected. For a native panel widget instead of Waybar, run"
  info "scripts/install-plasma.sh from a checkout, then add the TokenGauge widget."
fi

if $HAS_OMARCHY; then
  success "Restart Waybar: omarchy-restart-waybar"
else
  success "Restart Waybar to see the module."
  echo ""
  info "To open the TUI on click, add \"on-click\" to your Waybar config:"
  echo "  ghostty -e tokengauge-tui"
  echo "  alacritty -e tokengauge-tui"
fi
}

main "$@"
