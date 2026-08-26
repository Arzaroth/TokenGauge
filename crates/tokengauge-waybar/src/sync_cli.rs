//! The `--sync-*` commands and the doctor's fleet section.
//!
//! Split out of `main.rs`, which is the largest file in the crate and grew that
//! way one feature at a time. The six flags also became one typed command
//! instead of six early returns threaded through the startup path.

use std::path::Path;

use anyhow::{Context, Result};
use tokengauge_core::{TokenGaugeConfig, read_cache_full};

use crate::{Args, DoctorCheck};

/// A label no real check uses, so `handle_doctor` knows where to print the
/// heading without this module reaching into its closures.
pub const SECTION_MARKER: &str = "\u{0}fleet-sync-section";

pub enum SyncCommand {
    Init { force: bool },
    Join { key: String, force: bool },
    Status { json: bool },
    Test,
    Forget(String),
    Setup,
}

pub fn from_args(args: &Args) -> Option<SyncCommand> {
    if args.sync_init {
        return Some(SyncCommand::Init {
            force: args.sync_force,
        });
    }
    if let Some(key) = args.sync_join.as_deref() {
        return Some(SyncCommand::Join {
            key: key.to_string(),
            force: args.sync_force,
        });
    }
    if args.sync_status {
        return Some(SyncCommand::Status { json: args.json });
    }
    if args.sync_test {
        return Some(SyncCommand::Test);
    }
    if let Some(device) = args.sync_forget.as_deref() {
        return Some(SyncCommand::Forget(device.to_string()));
    }
    if args.sync_setup {
        return Some(SyncCommand::Setup);
    }
    None
}

pub fn run(command: SyncCommand, config: &TokenGaugeConfig, config_path: &Path) -> Result<()> {
    match command {
        SyncCommand::Init { force } => init(config, force),
        SyncCommand::Join { key, force } => join(config, &key, force),
        SyncCommand::Status { json } => status(config, json),
        SyncCommand::Test => test(config),
        SyncCommand::Forget(device) => forget(config, &device),
        SyncCommand::Setup => setup(config, config_path),
    }
}

fn init(config: &TokenGaugeConfig, force: bool) -> Result<()> {
    let key = tokengauge_core::sync::FleetKey::generate();
    let path = tokengauge_core::sync::store_key(&config.cache_file, &key, force)?;
    println!("{}", key.display());
    eprintln!("Fleet key written to {}", path.display());
    eprintln!(
        "On every other machine: `tokengauge --sync-join -` and paste it, which keeps the key out of shell history."
    );
    Ok(())
}

fn join(config: &TokenGaugeConfig, raw: &str, force: bool) -> Result<()> {
    let raw = if raw.trim() == "-" {
        read_key_from_stdin()?
    } else {
        raw.to_string()
    };
    let key = tokengauge_core::sync::FleetKey::parse(&raw)?;
    let path = tokengauge_core::sync::store_key(&config.cache_file, &key, force)?;
    eprintln!("Joined fleet {} (key at {})", key.id_hex(), path.display());
    Ok(())
}

/// Stdin, so the key stays out of shell history and `/proc/<pid>/cmdline`.
fn read_key_from_stdin() -> Result<String> {
    let mut typed = String::new();
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        eprint!("Fleet key: ");
    }
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut typed)
        .context("could not read the fleet key from stdin")?;
    Ok(typed)
}

fn test(config: &TokenGaugeConfig) -> Result<()> {
    for step in tokengauge_core::sync::test_round_trip(config)? {
        println!("ok  {step}");
    }
    Ok(())
}

fn forget(config: &TokenGaugeConfig, device: &str) -> Result<()> {
    let label = tokengauge_core::sync::forget(config, device)?;
    eprintln!("Dropped {label} from the fleet.");
    Ok(())
}

fn setup(config: &TokenGaugeConfig, config_path: &Path) -> Result<()> {
    let command = tokengauge_core::launch::tui_sync_command(config);
    if !tokengauge_core::launch::spawn_shell_with_config(&command, config_path) {
        anyhow::bail!(
            "no terminal found to open the TUI in; set [waybar] tui_command, or run `tokengauge-tui --sync` yourself"
        );
    }
    Ok(())
}

fn status(config: &TokenGaugeConfig, as_json: bool) -> Result<()> {
    let status = read_cache_full(&config.cache_file)
        .ok()
        .and_then(|cached| cached.sync().cloned());

    if as_json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    let Some(status) = status else {
        println!("Sync has not run yet.");
        if !config.sync.enabled {
            println!("It is off; set `enabled = true` under [sync] to turn it on.");
        }
        return Ok(());
    };

    let report = tokengauge_core::sync::describe(
        &status,
        config.refresh_secs,
        chrono::Utc::now().timestamp_millis(),
    );

    println!("Sync       {}", if report.enabled { "on" } else { "off" });
    if !report.transport.is_empty() {
        println!("Transport  {}", report.transport);
    }
    if !report.key_id.is_empty() {
        println!("Fleet key  {}", report.key_id);
    }
    if let Some(last) = &report.last_pull {
        println!("Last pull  {last}");
    }

    if !report.devices.is_empty() {
        println!("\nDevices");
        for device in &report.devices {
            println!("  {:<20} {}", device.label, device.detail);
        }
    }
    if !report.problems.is_empty() {
        println!("\nProblems");
        for problem in &report.problems {
            println!("  {problem}");
        }
    }
    Ok(())
}

/// The doctor's fleet section, or nothing when sync is off.
pub fn doctor_checks(cfg: &TokenGaugeConfig) -> Vec<DoctorCheck> {
    if !cfg.sync.enabled {
        return Vec::new();
    }
    let mut checks = vec![DoctorCheck {
        label: SECTION_MARKER.to_string(),
        ok: true,
        detail: String::new(),
    }];
    let mut record = |check: DoctorCheck| checks.push(check);
    let status = read_cache_full(&cfg.cache_file)
        .ok()
        .and_then(|cached| cached.sync().cloned());

    record(match tokengauge_core::sync::load_key(&cfg.cache_file) {
        Ok(Some(key)) => DoctorCheck {
            label: format!("fleet key {}", key.id_hex()),
            ok: true,
            detail: tokengauge_core::sync::key_path(&cfg.cache_file)
                .display()
                .to_string(),
        },
        _ => DoctorCheck {
            label: "no fleet key".into(),
            ok: false,
            detail: "run: tokengauge --sync-init, or --sync-join <key>".into(),
        },
    });

    let syncing = cfg
        .sync
        .providers
        .resolve(&cfg.providers.enabled_providers());
    record(DoctorCheck {
        label: format!("providers syncing: {}", syncing.join(", ")),
        ok: !syncing.is_empty(),
        detail: if syncing.is_empty() {
            "no enabled provider can sync; only claude and codex have transcript readers".into()
        } else {
            String::new()
        },
    });

    match &status {
        None => record(DoctorCheck {
            label: "no cycle has run yet".into(),
            ok: false,
            detail: "run: tokengauge --sync-test".into(),
        }),
        Some(status) => {
            record(DoctorCheck {
                label: format!("{} device(s) in the fleet", status.devices.len()),
                ok: status.error.is_none(),
                detail: status.transport.clone(),
            });
            // Every wording below is the core's. This section used to re-derive
            // the skipped objects, the overlaps and the quiet machines with its
            // own phrasing for each, which is three chances to say something
            // the panel does not.
            for finding in tokengauge_core::sync::findings(
                status,
                cfg.refresh_secs,
                chrono::Utc::now().timestamp_millis(),
            ) {
                record(DoctorCheck {
                    label: finding.title.to_string(),
                    ok: !finding.is_problem(),
                    detail: finding.sentence(),
                });
            }
        }
    }
    checks
}
