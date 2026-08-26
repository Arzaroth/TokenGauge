//! Opening the TUI in a terminal, from a frontend that only knows how to run
//! the binary.
//!
//! Terminal discovery lives here rather than in the waybar crate so every
//! frontend's "open" button is a spawn of a command it already knows how to
//! run. No frontend needs to know what a terminal is.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::TokenGaugeConfig;

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The shell command that opens the TUI, honouring `[waybar] tui_command`.
pub fn tui_command(config: &TokenGaugeConfig) -> String {
    tui_command_with(config, &[])
}

/// The same, opened on the sync screen.
pub fn tui_sync_command(config: &TokenGaugeConfig) -> String {
    tui_command_with(config, &["--sync"])
}

/// One launcher, because the two differed only by arguments and the difference
/// had a trap in it.
///
/// The omarchy wrapper focuses an existing TUI rather than starting one, so it
/// silently ignores arguments: focusing a window that is not on the sync screen
/// is not what was asked for. It is used only when there are none - including
/// when the user's own `tui_command` *is* that wrapper, which the previous
/// version appended to regardless.
fn tui_command_with(config: &TokenGaugeConfig, args: &[&str]) -> String {
    let extra = if args.is_empty() {
        String::new()
    } else {
        format!(" {}", args.join(" "))
    };

    let explicit = config.waybar.tui_command.trim();
    if !explicit.is_empty() && !(args.is_empty() || is_focus_wrapper(explicit)) {
        return format!("{explicit}{extra}");
    }
    if !explicit.is_empty() && args.is_empty() {
        return explicit.to_string();
    }
    if args.is_empty() && which(FOCUS_WRAPPER).is_some() {
        return format!("{FOCUS_WRAPPER} tokengauge-tui");
    }
    match terminal() {
        Some(term) => format!("{term} -e tokengauge-tui{extra}"),
        None => String::new(),
    }
}

const FOCUS_WRAPPER: &str = "omarchy-launch-or-focus-tui";

fn is_focus_wrapper(command: &str) -> bool {
    command.split_whitespace().next() == Some(FOCUS_WRAPPER)
}

fn terminal() -> Option<String> {
    std::env::var("TERMINAL")
        .ok()
        .into_iter()
        .chain(
            ["ghostty", "alacritty", "kitty", "wezterm", "foot", "xterm"]
                .iter()
                .map(|s| s.to_string()),
        )
        .find(|term| which(term).is_some())
}

pub fn spawn_shell(command: &str) -> bool {
    spawn_shell_inner(command, None)
}

/// The same, with the TUI pointed at the config the caller resolved. The child
/// resolves its own config and would otherwise fall back to the default path,
/// so `--config other.toml --sync-setup` opened the sync screen of a different
/// fleet. It travels in the environment because the command is handed to
/// `sh -c`, where a path would have to be quoted.
pub fn spawn_shell_with_config(command: &str, config_path: &Path) -> bool {
    spawn_shell_inner(command, Some(config_path))
}

fn spawn_shell_inner(command: &str, config_path: Option<&Path>) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = config_path {
        cmd.env("TOKENGAUGE_CONFIG", path);
    }
    cmd.spawn().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(tui_command: &str) -> TokenGaugeConfig {
        let mut config = TokenGaugeConfig::default();
        config.waybar.tui_command = tui_command.to_string();
        config
    }

    #[test]
    fn an_explicit_launcher_is_used_verbatim_and_gains_the_flag() {
        let config = config_with("foot -e tokengauge-tui");
        assert_eq!(tui_command(&config), "foot -e tokengauge-tui");
        assert_eq!(tui_sync_command(&config), "foot -e tokengauge-tui --sync");
    }

    /// The wrapper focuses an existing window and drops arguments, so appending
    /// `--sync` to it opened the TUI on whatever screen it was already showing.
    #[test]
    fn the_focus_wrapper_is_never_handed_a_flag_it_will_ignore() {
        let config = config_with("omarchy-launch-or-focus-tui tokengauge-tui");
        assert_eq!(
            tui_command(&config),
            "omarchy-launch-or-focus-tui tokengauge-tui"
        );

        let sync = tui_sync_command(&config);
        assert!(
            !sync.starts_with("omarchy-launch-or-focus-tui"),
            "the wrapper would swallow --sync: {sync}"
        );
        assert!(
            sync.is_empty() || sync.ends_with("-e tokengauge-tui --sync"),
            "{sync}"
        );
    }
}
