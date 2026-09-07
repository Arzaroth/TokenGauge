//! The `--doctor` checks that only the waybar binary can make.
//!
//! The report itself lives in [`tokengauge_core::doctor`], because the same
//! question is worth asking on every platform and this crate is Linux-only.
//! What stays here is what depends on this binary's own surface: whether the
//! bar module is wired up, whether the click action can launch, and the fleet
//! sync section `sync_cli` builds.

use std::path::{Path, PathBuf};

use tokengauge_core::TokenGaugeConfig;
pub use tokengauge_core::doctor::{DoctorCheck, DoctorLine};

use crate::*;

/// Print the report and return the exit code.
pub(crate) fn handle_doctor(config_path: &Path) -> i32 {
    tokengauge_core::doctor::handle_doctor(config_path, env!("CARGO_PKG_VERSION"), waybar_checks)
}

/// Slotted into the core's report where "Bar wiring" has always been.
fn waybar_checks(cfg: &TokenGaugeConfig) -> Vec<DoctorLine> {
    // Same shape as the core's report builder: a `RefCell` so the recording
    // closure and the headings can both reach the list.
    let out: std::cell::RefCell<Vec<DoctorLine>> = std::cell::RefCell::new(Vec::new());
    let record = |c: DoctorCheck| out.borrow_mut().push(DoctorLine::Check(c));

    // Bar wiring. Waybar is one surface of several now, so its module is only
    // missing-and-wrong when nothing else is drawing the gauge; on a desktop
    // running the Plasma applet, the GNOME extension or the Omarchy widget,
    // having no waybar config is the normal state and not a fault.
    out.borrow_mut().push(DoctorLine::Heading("Bar wiring"));
    let drawn_by: Vec<&str> = tokengauge_core::frontend::installed()
        .iter()
        .map(|f| f.label)
        .collect();
    let waybar_cfg = waybar_config_path();
    let contents = std::fs::read_to_string(&waybar_cfg).ok();
    record(bar_wiring_check(
        &waybar_cfg,
        contents.as_deref(),
        &drawn_by,
    ));
    record(click_action_check(cfg));

    let mut out = out.into_inner();
    out.extend(sync_cli::doctor_checks(cfg));
    out
}

fn waybar_config_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/waybar/config.jsonc"))
        .unwrap_or_else(|| PathBuf::from("~/.config/waybar/config.jsonc"))
}

/// `contents` is `None` when there is no waybar config to read.
///
/// Waybar is one surface of several now, so its module is only missing-and-
/// wrong when nothing else is drawing the gauge: on a desktop running the
/// Plasma applet, the GNOME extension or the Omarchy widget, having no waybar
/// config is the normal state and not a fault.
fn bar_wiring_check(waybar_cfg: &Path, contents: Option<&str>, drawn_by: &[&str]) -> DoctorCheck {
    let drawn_by_text = drawn_by.join(", ");
    match contents {
        Some(contents) => {
            let wired = contents.contains("custom/tokengauge");
            DoctorCheck {
                label: format!("module wired in {}", waybar_cfg.display()),
                ok: wired || !drawn_by.is_empty(),
                detail: match (wired, drawn_by.is_empty()) {
                    (true, _) => String::new(),
                    (false, true) => {
                        "run scripts/install.sh to add the custom/tokengauge module".into()
                    }
                    (false, false) => {
                        format!("not wired, and not needed: {drawn_by_text} draws it")
                    }
                },
            }
        }
        None if drawn_by.is_empty() => DoctorCheck {
            label: "no bar wired up".into(),
            ok: false,
            detail: format!(
                "no {} and no desktop frontend installed - run scripts/install.sh, or tokengauge --install-frontend <plasma|gnome|omarchy>",
                waybar_cfg.display()
            ),
        },
        None => DoctorCheck {
            label: format!("waybar not in use - {drawn_by_text} draws the gauge"),
            ok: true,
            detail: String::new(),
        },
    }
}

/// Click action prerequisites: the binary the user wants to spawn on
/// left-click must be on PATH.
fn click_action_check(cfg: &TokenGaugeConfig) -> DoctorCheck {
    let click_cmd = resolve_click_command(cfg);
    if click_cmd.is_empty() {
        return DoctorCheck {
            label: "click action launcher resolved".into(),
            ok: false,
            detail: "no TUI launcher found; set [waybar].tui_command or install a terminal".into(),
        };
    }
    let first = click_cmd.split_whitespace().next().unwrap_or("");
    let on_path = tokengauge_core::launch::which(first).is_some() || first.starts_with('/');
    DoctorCheck {
        label: format!(
            "click action: {:?} -> {}",
            cfg.waybar.click_action, click_cmd
        ),
        ok: on_path,
        detail: if on_path {
            String::new()
        } else {
            format!("'{first}' not found on $PATH")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TokenGaugeConfig {
        TokenGaugeConfig {
            cache_file: std::env::temp_dir().join(format!(
                "tg-waybar-doctor-{}-{:?}/usage.json",
                std::process::id(),
                std::thread::current().id()
            )),
            ..Default::default()
        }
    }

    /// A waybar config with no module in it is only a fault when nothing else
    /// draws the gauge. Reporting it as one on a GNOME or Plasma desktop was a
    /// red cross next to a machine that is working exactly as installed.
    #[test]
    fn an_unwired_bar_is_a_fault_only_when_nothing_else_draws_the_gauge() {
        let path = Path::new("/home/someone/.config/waybar/config.jsonc");

        let wired = bar_wiring_check(
            path,
            Some(r#"{"modules-right":["custom/tokengauge"]}"#),
            &[],
        );
        assert!(wired.ok);
        assert!(wired.detail.is_empty());

        let alone = bar_wiring_check(path, Some(r#"{"modules-right":["clock"]}"#), &[]);
        assert!(!alone.ok, "nothing draws the gauge at all");
        assert!(alone.detail.contains("scripts/install.sh"));

        let covered = bar_wiring_check(path, Some(r#"{"modules-right":["clock"]}"#), &["GNOME"]);
        assert!(covered.ok, "GNOME draws it; waybar not being wired is fine");
        assert!(covered.detail.contains("GNOME"));

        let no_config = bar_wiring_check(path, None, &[]);
        assert!(!no_config.ok);
        assert!(no_config.detail.contains("--install-frontend"));

        let no_config_but_covered = bar_wiring_check(path, None, &["Plasma", "GNOME"]);
        assert!(no_config_but_covered.ok);
        assert!(no_config_but_covered.label.contains("Plasma, GNOME"));
    }

    /// The check is about the launcher being on PATH, so it has to name the
    /// binary it could not find rather than the whole command line.
    #[test]
    fn the_click_action_check_names_the_binary_it_could_not_find() {
        let mut cfg = config();
        cfg.waybar.tui_command = "tokengauge-no-such-terminal -e tokengauge-tui".into();
        let missing = click_action_check(&cfg);
        assert!(!missing.ok);
        assert_eq!(
            missing.detail,
            "'tokengauge-no-such-terminal' not found on $PATH"
        );
        assert!(missing.label.contains("tokengauge-no-such-terminal -e"));

        cfg.waybar.tui_command = "sh -c tokengauge-tui".into();
        let present = click_action_check(&cfg);
        assert!(present.ok, "sh is on PATH on every unix");
        assert!(present.detail.is_empty());

        // An absolute path is taken at its word: it names a binary outside
        // PATH, which is the reason to have written one.
        cfg.waybar.tui_command = "/opt/term/bin/term -e tokengauge-tui".into();
        assert!(click_action_check(&cfg).ok);
    }

    /// The whole fleet section is gated on `[sync] enabled` - the history it
    /// feeds is not, but the cycle is, and a machine syncing with nobody has
    /// no fleet to report on.
    #[test]
    fn the_fleet_section_is_absent_until_sync_is_enabled() {
        let mut cfg = config();
        assert!(!cfg.sync.enabled);
        assert!(sync_cli::doctor_checks(&cfg).is_empty());

        cfg.sync.enabled = true;
        let checks = sync_cli::doctor_checks(&cfg);
        assert!(matches!(
            checks.first(),
            Some(DoctorLine::Heading("Fleet sync"))
        ));
        let labels: Vec<&str> = checks
            .iter()
            .filter_map(|line| match line {
                DoctorLine::Check(c) => Some(c.label.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"no fleet key"), "{labels:?}");
        assert!(labels.contains(&"no cycle has run yet"), "{labels:?}");
    }
}
