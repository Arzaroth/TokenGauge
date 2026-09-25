//! Opening the TUI in a terminal, a URL in a browser, and a notification on
//! the desktop, from a frontend that only knows how to run the binary.
//!
//! Terminal discovery lives here rather than in the waybar crate so every
//! frontend's "open" button is a spawn of a command it already knows how to
//! run. No frontend needs to know what a terminal is.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::TokenGaugeConfig;

/// The PATH walk is selvedge's, which answers only for a file that is
/// actually executable. This copy stopped at `is_file`, so a regular file
/// named `kitty` and never chmod'd answered for the terminal.
pub use selvedge::proc::which;

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
pub fn tui_command_with(config: &TokenGaugeConfig, args: &[&str]) -> String {
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
    #[cfg(target_os = "macos")]
    if std::env::var("TERMINAL").is_err() {
        return terminal_app_command(&format!("{}{extra}", shell_quote(&tui_path())));
    }
    match terminal() {
        Some(term) => format!("{term} -e tokengauge-tui{extra}"),
        None => String::new(),
    }
}

/// The TUI installed beside this binary. A GUI launched from Finder or launchd
/// does not have the shell's `PATH`, and neither does the Terminal window it
/// opens until the login shell has run, so a bare name finds nothing there.
#[cfg(target_os = "macos")]
fn tui_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("tokengauge-tui")))
        .filter(|tui| tui.is_file())
        .map(|tui| tui.display().to_string())
        .unwrap_or_else(|| "tokengauge-tui".to_string())
}

/// Terminal.app takes no command on its command line, only through AppleScript.
/// The command travels as an argument to the script rather than inside its
/// source, so nothing in it has to be escaped for AppleScript.
#[cfg(any(target_os = "macos", all(test, unix)))]
fn terminal_app_command(command: &str) -> String {
    format!(
        "osascript -e 'on run argv' \
         -e 'tell application \"Terminal\" to do script (item 1 of argv)' \
         -e 'tell application \"Terminal\" to activate' \
         -e 'end run' {}",
        shell_quote(command)
    )
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
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

/// Open a URL in the default browser.
pub fn open_url(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// How loudly a notification asks for attention, in libnotify's terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// Post a desktop notification. `transient` ones skip the notification
/// history where the desktop keeps one.
pub fn notify(title: &str, body: &str, urgency: Urgency, transient: bool) -> bool {
    notify_command(title, body, urgency, transient)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

#[cfg(target_os = "macos")]
fn notify_command(title: &str, body: &str, _urgency: Urgency, _transient: bool) -> Command {
    let mut cmd = Command::new("osascript");
    cmd.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        title,
        body,
    ]);
    cmd
}

#[cfg(not(target_os = "macos"))]
fn notify_command(title: &str, body: &str, urgency: Urgency, transient: bool) -> Command {
    let urgency = match urgency {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };
    let mut cmd = Command::new("notify-send");
    cmd.arg("--urgency")
        .arg(urgency)
        .arg("--app-name")
        .arg("tokengauge")
        .arg(format!("--hint=int:transient:{}", u8::from(transient)))
        .arg(title)
        .arg(body);
    cmd
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
        // Terminal.app on macOS receives the command quoted as one word.
        assert!(
            sync.is_empty()
                || sync.ends_with("-e tokengauge-tui --sync")
                || (cfg!(target_os = "macos") && sync.ends_with(" --sync'")),
            "{sync}"
        );
    }

    /// `which` is the doctor's PATH walk as well as this module's, so it has to
    /// answer for a name that is not there rather than for the directory entry
    /// that happens to share it.
    #[test]
    fn which_finds_a_binary_on_path_and_nothing_else() {
        // `which` joins the name onto each PATH entry verbatim, so only a unix
        // has a name it can be asked about; Windows would need the extension.
        #[cfg(unix)]
        assert!(which("sh").is_some(), "sh is on PATH on every unix");
        assert!(which("tokengauge-no-such-binary").is_none());
    }

    /// A path with a space or a quote in it has to reach Terminal.app as one
    /// word. Running the shell half of the command shows what it hands over.
    #[cfg(unix)]
    #[test]
    fn terminal_app_is_handed_the_command_as_one_argument() {
        let tui = shell_quote("/Users/o'brien/my bin/tokengauge-tui");
        let command = terminal_app_command(&format!("{tui} --sync"));
        let probe = command.replacen("osascript", "printf '%s\\n'", 1);
        let out = Command::new("sh").arg("-c").arg(&probe).output().unwrap();
        let args: Vec<_> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            args.last().map(String::as_str),
            Some(r"'/Users/o'\''brien/my bin/tokengauge-tui' --sync"),
            "{args:?}"
        );
        let words = Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n' {}", args.last().unwrap()))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(words.stdout).unwrap(),
            "/Users/o'brien/my bin/tokengauge-tui\n--sync\n"
        );
    }

    /// An empty command is what `tui_command` returns when it found no
    /// terminal at all. Handing that to `sh -c` runs a shell that does nothing
    /// and reports success, which is a click that silently did nothing.
    #[test]
    fn a_command_that_resolved_to_nothing_is_not_spawned() {
        assert!(!spawn_shell(""));
        assert!(!spawn_shell("   "));
        assert!(!spawn_shell_with_config(
            "  ",
            Path::new("/tmp/tokengauge.toml")
        ));
        #[cfg(unix)]
        assert!(spawn_shell("exit 0"), "a real command still spawns");
    }
}
