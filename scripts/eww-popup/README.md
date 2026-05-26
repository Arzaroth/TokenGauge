# TokenGauge eww popover

A starter eww popover so you can use a GUI window on left-click instead of
the terminal TUI.

## Install

1. Copy or symlink the contents of this directory into your eww config:

   ```sh
   ln -s "$(realpath eww.yuck)" ~/.config/eww/tokengauge.yuck
   ln -s "$(realpath eww.scss)" ~/.config/eww/tokengauge.scss
   ```

   Then `(include "./tokengauge.yuck")` and `@import "tokengauge";` from your
   main `eww.yuck` / `eww.scss`.

2. Tell TokenGauge to use it. In `~/.config/tokengauge/config.toml`:

   ```toml
   [waybar]
   click_action = "popover"
   popover_command = "eww open --toggle tokengauge-popup"
   ```

3. Reload eww (`eww reload`) and waybar (`pkill -SIGUSR2 waybar`).

## Verify

```sh
tokengauge-waybar --doctor
```

The doctor reports the resolved click action and warns if `eww` (or whichever
binary leads `popover_command`) isn't on `$PATH`.

## What it shows

The skeleton polls `tokengauge-waybar` every 10s and renders the same pango
markup the waybar tooltip uses. Extend the widget tree in `eww.yuck` to show
richer widgets (progress bars, charts, per-provider sub-cards) by parsing the
JSON further with `jq` filters.
