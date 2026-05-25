# TokenGauge

[![GitHub release](https://img.shields.io/github/v/release/oorestisime/TokenGauge)](https://github.com/oorestisime/TokenGauge/releases)

Monitor token usage from your Waybar. Powered by [CodexBar](https://github.com/steipete/CodexBar). Built for [Omarchy](https://omarchy.org) ([GitHub](https://github.com/basecamp/omarchy)) but works with any Waybar setup on Linux.

| Waybar | TUI |
|--------|-----|
| ![Waybar module](waybar.png) | ![TUI dashboard](tui.png) |

## Features

- Per-provider usage bars in Waybar
- TUI dashboard with colored progress bars and reset times
- Show daily or weekly usage (configurable)
- Smart caching to minimize API calls
- Click waybar module to open TUI

## Supported Providers

| Provider | Type | Config |
|----------|------|--------|
| Codex | OAuth | `codex = true` |
| Claude | OAuth | `claude = true` |
| Kimi K2 | API | `[providers.kimik2]` with `api_key` |
| z.ai | API | `[providers.zai]` with `api_key` |
| Copilot | API | `[providers.copilot]` with `api_key` |
| MiniMax | API | `[providers.minimax]` with `api_key` |
| Kimi | API | `[providers.kimi]` with `api_key` |

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/oorestisime/TokenGauge/main/scripts/install.sh | bash
omarchy-restart-waybar
```

Click the waybar module to open the TUI dashboard.

### Placement

By default the module is added to `modules-right` (before the tray on Omarchy). To put it on the left instead (right after `hyprland/workspaces`), run:

```bash
curl -fsSL https://raw.githubusercontent.com/oorestisime/TokenGauge/main/scripts/install.sh | bash -s -- --placement=left
```

`TOKENGAUGE_PLACEMENT=left` works too. The choice is persisted in `~/.config/tokengauge/config.toml` under `[waybar] placement`; re-running the installer with a different `--placement` migrates the module to the other side.

## Configuration

Edit `~/.config/tokengauge/config.toml`:

| Field | Description | Default |
|-------|-------------|---------|
| `codexbar_bin` | Path to CodexBar CLI | `codexbar` |
| `refresh_secs` | Cache refresh interval (seconds) | `600` |
| `cache_file` | Cache file location | `/tmp/tokengauge-usage.json` |
| `providers.codex` | Enable Codex (OAuth) | `true` |
| `providers.claude` | Enable Claude (OAuth) | `true` |
| `providers.<name>.api_key` | API key for API providers | — |
| `waybar.window` | Show `daily` or `weekly` usage | `daily` |
| `waybar.placement` | `left` or `right` in the waybar | `right` |
| `waybar.primary` | Provider key shown in the bar text (unset = show all) | unset |

> **Note:** Waybar's `interval` controls how often the UI refreshes. Keep it shorter than `refresh_secs` so the UI updates from cache without extra API calls.

## Usage

### Waybar

The module displays per-provider usage bars. Hover for detailed tooltip with reset times.

Set `waybar.primary` (e.g. `primary = "claude"`) to show only one provider in the bar text and tooltip. Unset = show all providers stacked. Scrolling on the module rotates the visible provider; that choice is persisted in `<cache_dir>/tokengauge-waybar-state.json` and overrides `primary` until you scroll again. Click opens the TUI on the same provider.

### TUI

Run `tokengauge-tui` or click the waybar module.

| Key | Action |
|-----|--------|
| `r` | Refresh |
| `q` / `Esc` | Quit |

## Updates

```bash
# Update TokenGauge
curl -fsSL https://raw.githubusercontent.com/oorestisime/TokenGauge/main/scripts/update.sh | bash

# Update CodexBar CLI
curl -fsSL https://raw.githubusercontent.com/oorestisime/TokenGauge/main/scripts/update-codexbar.sh | bash
```

## Without Omarchy

The install script detects Omarchy automatically. Without it, you'll need to configure the TUI click handler manually.

Edit `~/.config/waybar/config.jsonc` and add `on-click` to the tokengauge module:

```jsonc
"custom/tokengauge": {
  "exec": "tokengauge-waybar",
  "return-type": "json",
  "interval": 60,
  "on-click": "ghostty -e tokengauge-tui"
}
```

Other terminals: `alacritty -e tokengauge-tui`, `kitty -e tokengauge-tui`, `foot tokengauge-tui`

## Manual Installation

1. Download the latest release from [GitHub Releases](https://github.com/oorestisime/TokenGauge/releases)

2. Extract and install:
   ```bash
   tar -xzf tokengauge-<version>-linux-<arch>.tar.gz
   install -m 0755 tokengauge-waybar ~/.local/bin/
   install -m 0755 tokengauge-tui ~/.local/bin/
   ```

3. Create config:
   ```bash
   mkdir -p ~/.config/tokengauge
   cat > ~/.config/tokengauge/config.toml << 'EOF'
   codexbar_bin = "codexbar"
   refresh_secs = 600
   cache_file = "/tmp/tokengauge-usage.json"

   [providers]
   codex = true
   claude = true

   # API providers (uncomment and add your API key)
   # [providers.kimik2]
   # api_key = "your-api-key"

   [waybar]
   window = "daily"
   EOF
   ```

4. Add to waybar config (`~/.config/waybar/config.jsonc`). Add `"custom/tokengauge"` to either `modules-left` (after `"hyprland/workspaces"`) or `modules-right`:
   ```jsonc
   "modules-right": ["custom/tokengauge", ...],
   "custom/tokengauge": {
     "exec": "tokengauge-waybar",
     "return-type": "json",
     "interval": 60,
     "on-click": "ghostty -e tokengauge-tui"
   }
   ```

5. Install [CodexBar CLI](https://github.com/steipete/CodexBar) if not already installed

6. Restart Waybar
