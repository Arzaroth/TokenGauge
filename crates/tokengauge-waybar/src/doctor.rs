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
    tokengauge_core::doctor::handle_doctor(config_path, waybar_checks)
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
    let drawn_by_text = drawn_by.join(", ");
    let waybar_cfg = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/waybar/config.jsonc"))
        .unwrap_or_else(|| PathBuf::from("~/.config/waybar/config.jsonc"));
    if waybar_cfg.exists() {
        let contents = std::fs::read_to_string(&waybar_cfg).unwrap_or_default();
        let wired = contents.contains("custom/tokengauge");
        record(DoctorCheck {
            label: format!("module wired in {}", waybar_cfg.display()),
            ok: wired || !drawn_by.is_empty(),
            detail: match (wired, drawn_by.is_empty()) {
                (true, _) => String::new(),
                (false, true) => {
                    "run scripts/install.sh to add the custom/tokengauge module".into()
                }
                (false, false) => format!("not wired, and not needed: {drawn_by_text} draws it"),
            },
        });
    } else if drawn_by.is_empty() {
        record(DoctorCheck {
            label: "no bar wired up".into(),
            ok: false,
            detail: format!(
                "no {} and no desktop frontend installed - run scripts/install.sh, or tokengauge --install-frontend <plasma|gnome|omarchy>",
                waybar_cfg.display()
            ),
        });
    } else {
        record(DoctorCheck {
            label: format!("waybar not in use - {drawn_by_text} draws the gauge"),
            ok: true,
            detail: String::new(),
        });
    }

    // Click action prerequisites: the binary the user wants to spawn
    // on left-click must be on PATH.
    let click_cmd = resolve_click_command(cfg);
    let (label, ok, detail) = if click_cmd.is_empty() {
        (
            "click action launcher resolved".into(),
            false,
            "no TUI launcher found; set [waybar].tui_command or install a terminal".into(),
        )
    } else {
        let first = click_cmd
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        let on_path = tokengauge_core::launch::which(&first).is_some() || first.starts_with('/');
        (
            format!(
                "click action: {:?} -> {}",
                cfg.waybar.click_action, click_cmd
            ),
            on_path,
            if on_path {
                String::new()
            } else {
                format!("'{first}' not found on $PATH")
            },
        )
    };
    record(DoctorCheck { label, ok, detail });

    let mut out = out.into_inner();
    out.extend(sync_cli::doctor_checks(cfg));
    out
}
