//! Opening the TUI in a terminal, from a frontend that only knows how to run
//! the binary.
//!
//! Terminal discovery lives here rather than in the waybar crate so every
//! frontend's "open" button is a spawn of a command it already knows how to
//! run. No frontend needs to know what a terminal is.

use std::path::PathBuf;
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
    let explicit = config.waybar.tui_command.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if which("omarchy-launch-or-focus-tui").is_some() {
        return "omarchy-launch-or-focus-tui tokengauge-tui".to_string();
    }
    match terminal() {
        Some(term) => format!("{term} -e tokengauge-tui"),
        None => String::new(),
    }
}

/// The same, opened on the sync screen.
///
/// The omarchy wrapper is skipped deliberately: it focuses an existing TUI, and
/// focusing a window that is not on the sync screen is not what was asked for.
pub fn tui_sync_command(config: &TokenGaugeConfig) -> String {
    let explicit = config.waybar.tui_command.trim();
    if !explicit.is_empty() {
        return format!("{explicit} --sync");
    }
    match terminal() {
        Some(term) => format!("{term} -e tokengauge-tui --sync"),
        None => String::new(),
    }
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
    if command.trim().is_empty() {
        return false;
    }
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}
