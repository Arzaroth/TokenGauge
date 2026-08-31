mod app;
mod refresh;
mod sync_view;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;
use tokengauge_core::default_config_path;

use crate::app::App;

#[derive(Parser, Debug)]
#[command(version, about = "TokenGauge TUI")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
    /// Download the latest matching release from GitHub and replace the
    /// installed binaries.
    #[arg(long)]
    update: bool,
    /// Query GitHub for the latest release, cache the result, and print it as
    /// JSON. Does not install anything.
    #[arg(long)]
    check_update: bool,
    /// Open straight on the sync screen. This is what `tokengauge --sync-setup`
    /// runs, so every frontend's "set up sync" button is one spawn.
    #[arg(long)]
    sync: bool,
    /// Print what this machine looks like to TokenGauge and exit non-zero if
    /// anything is wrong.
    #[arg(long)]
    doctor: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.update || args.check_update {
        return run_update(args.config, args.check_update);
    }

    // Before the TTY check: the doctor is the thing you run when the dashboard
    // will not tell you anything, including from a pipe.
    if args.doctor {
        let config_path = args.config.unwrap_or_else(default_config_path);
        // No extras: the bar wiring and fleet-sync checks belong to the waybar
        // binary, which is Linux-only and not installed beside this one.
        std::process::exit(tokengauge_core::doctor::handle_doctor(
            &config_path,
            env!("CARGO_PKG_VERSION"),
            |_| Vec::new(),
        ));
    }

    if !crossterm::tty::IsTty::is_tty(&io::stdout()) {
        return Err(anyhow!("tokengauge-tui must run in a TTY"));
    }

    let mut terminal = ratatui::init();
    let mut app = App::new(args.config);
    if args.sync {
        app.open_sync();
    }
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

/// `--update` / `--check-update`: run outside the TTY UI so the Windows build
/// (which has no waybar binary) can self-update too.
fn run_update(config: Option<PathBuf>, check_only: bool) -> Result<()> {
    use tokengauge_core::{load_config, update};
    let config = load_config(config)?;
    if check_only {
        let status = update::check(&config.cache_file)?;
        println!("{}", serde_json::to_string(&status)?);
        return Ok(());
    }
    let current = update::current_version();
    println!("Current version: {current}");
    println!("Checking for updates...");
    let installed = update::apply(&config.cache_file)?;
    if update::version_gt(&installed, current) {
        println!("Updated to {installed}. Restart TokenGauge to load it.");
    } else {
        println!("Already up to date ({current}).");
    }
    Ok(())
}
