mod daemon;
mod doctor;
mod export;
mod render;
mod snapshot;
mod sync_cli;

use daemon::*;
use doctor::*;
use render::*;
use snapshot::*;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Result;
use clap::Parser;
use tokengauge_core::now_ms;
use tokengauge_core::update;
use tokengauge_core::{
    TokenGaugeConfig, WaybarState, cache_is_stale, config_set_oauth_provider, config_set_primary,
    ensure_cache_dir, load_config, payload_to_rows_with_costs, read_waybar_state,
    refresh_in_progress, refresh_sentinel_path, signal_daemon_reload, waybar_state_path,
    write_default_config, write_waybar_state,
};

#[derive(Parser, Debug)]
// The package is still named for the Waybar module it grew out of, and clap
// takes its name from that unless told otherwise - so `--version` and every
// usage line would print the old binary name.
#[command(
    name = "tokengauge",
    version,
    about = "TokenGauge: usage, limits and costs for AI coding assistants"
)]
pub struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
    /// Rotate the provider shown in the waybar text and exit (no JSON output).
    #[arg(long, value_enum)]
    rotate: Option<RotateDir>,
    /// Wipe the cache file and exit. Next render will re-fetch usage and
    /// ccusage. Pair with a waybar signal so the bar repolls immediately.
    #[arg(long)]
    refresh: bool,
    /// Internal: run the actual fetch in a detached worker spawned by --refresh.
    /// Not for direct use.
    #[arg(long, hide = true)]
    internal_refresh_worker: bool,
    /// Open the selected provider's dashboard or status page in the browser.
    #[arg(long, value_enum)]
    open: Option<OpenTarget>,
    /// Print a diagnostic checklist (deps, config, cache, providers, waybar wiring).
    #[arg(long)]
    doctor: bool,
    /// Run as a long-lived daemon serving state over a Unix socket. The waybar
    /// custom module should use --client-tail to subscribe (push-based) instead
    /// of polling on an interval.
    #[arg(long)]
    daemon: bool,
    /// Experimental: connect to the daemon socket, subscribe, and stream JSON
    /// updates to stdout (one line per change). Most waybar versions don't
    /// pick up streaming exec output - use the standard polling config instead.
    #[arg(long, hide = true)]
    client_tail: bool,
    /// Handle a waybar on-click event by launching the terminal TUI. Override
    /// the launcher with `[waybar] tui_command`.
    #[arg(long)]
    click: bool,
    /// Emit the full snapshot as one JSON object (rows, errors, enabled,
    /// primary, theme, window) for non-waybar frontends such as the KDE
    /// Plasma applet. Does not affect the default waybar output line.
    #[arg(long)]
    json: bool,
    /// Block until the snapshot on disk changes, then exit 0. Lets a frontend
    /// whose toolkit cannot watch a file long-poll (`--wait-change && --json`)
    /// instead of re-reading on a timer.
    #[arg(long)]
    wait_change: bool,
    /// Give up waiting after this many seconds and exit 0 anyway, so a caller
    /// that long-polls still re-reads on a slow schedule when nothing writes.
    #[arg(long, value_name = "SECS", default_value_t = 300)]
    wait_timeout: u64,
    /// Enable/disable an OAuth provider in the config, then reload the daemon.
    /// Format: `--set-provider claude=true`.
    #[arg(long, value_name = "NAME=BOOL")]
    set_provider: Option<String>,
    /// Pin the bar to a provider, or `highest` to clear the pin, then reload
    /// the daemon. e.g. `--set-primary claude` or `--set-primary highest`.
    #[arg(long, value_name = "NAME")]
    set_primary: Option<String>,
    /// Download the latest matching release from GitHub and replace the
    /// installed binaries. Used by the GUI "Update" button too.
    #[arg(long)]
    update: bool,
    /// Query GitHub for the latest release, cache the result, and print it as
    /// JSON. Does not install anything.
    #[arg(long)]
    check_update: bool,
    /// Install a desktop frontend from the release this binary belongs to:
    /// `plasma`, `gnome`, `omarchy`, or `all`. Use it after switching desktops;
    /// `--update` already refreshes whichever are present.
    #[arg(long, value_name = "NAME")]
    install_frontend: Option<String>,
    /// Start a sync fleet: generate this machine's key and print it. Copy it to
    /// every other machine with `--sync-join`.
    #[arg(long)]
    sync_init: bool,
    /// Join the fleet a `--sync-init` key belongs to. Prefer `--sync-join -`,
    /// which reads the key from stdin: an argument lands in shell history and
    /// in `/proc/<pid>/cmdline`, and possession of the key is the only
    /// authentication there is.
    #[arg(long, value_name = "KEY")]
    sync_join: Option<String>,
    /// Replace an existing fleet key. Every machine has to be re-keyed
    /// together: devices holding the old key stop being readable.
    #[arg(long)]
    sync_force: bool,
    /// Print what the last sync cycle did. Add `--json` for the raw object.
    #[arg(long)]
    sync_status: bool,
    /// Write a probe object, read it back and remove it, to check the transport
    /// and the key before trusting the figures.
    #[arg(long)]
    sync_test: bool,
    /// Drop a device from the fleet by label or id: deletes its object and
    /// forgets its buckets.
    #[arg(long, value_name = "DEVICE")]
    sync_forget: Option<String>,
    /// Open the TUI on its sync screen, in a terminal.
    #[arg(long)]
    sync_setup: bool,
    /// Write the history store to stdout: one row per day, provider and model.
    /// `csv` when given no value, or `json`.
    #[arg(long, value_enum, value_name = "FORMAT", num_args = 0..=1, default_missing_value = "csv")]
    export: Option<ExportFormat>,
    /// Oldest day `--export` includes, as `YYYY-MM-DD`. Defaults to everything
    /// the store still holds.
    #[arg(long, value_name = "DATE")]
    since: Option<String>,
    /// Re-read every transcript still on disk into the history store. This runs
    /// once by itself, before the first fetch; the flag forces it again, which
    /// is what to reach for after restoring a machine's transcripts from a
    /// backup.
    #[arg(long)]
    backfill: bool,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExportFormat {
    #[default]
    Csv,
    Json,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum OpenTarget {
    Dashboard,
    Status,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RotateDir {
    Next,
    Prev,
}

/// What one invocation does.
///
/// The flags are mutually exclusive, and clap cannot say so across this many of
/// them. Resolving them here rather than in a chain of early returns makes the
/// precedence a list you can read, and turns a combination nobody meant into an
/// error: `--set-provider x=true --json` used to emit the JSON and drop the
/// toggle on the floor, because `--json` happened to be tested first.
///
/// `--json` is a *modifier* on `--sync-status`, not an action, which is why the
/// sync commands are resolved before it.
enum Action {
    Doctor,
    Sync(sync_cli::SyncCommand),
    InternalRefreshWorker,
    Daemon,
    ClientTail,
    Click,
    WaitChange,
    Json,
    SetProvider(String),
    SetPrimary(String),
    CheckUpdate,
    InstallFrontend(String),
    Export(ExportFormat),
    Backfill,
    Update,
    Refresh,
    Rotate(RotateDir),
    Open(OpenTarget),
    /// No action flag: emit one waybar payload and exit. The default because
    /// this is what waybar itself runs, on a timer, with no arguments.
    Bar,
}

impl Action {
    fn from_args(args: &Args) -> Result<Self> {
        // (what was asked for, how the user spelled it). Order is precedence,
        // and every entry is named so a second one can be reported rather than
        // silently losing to whichever came first.
        let mut asked: Vec<(&str, Action)> = Vec::new();
        macro_rules! add {
            ($flag:expr, $action:expr) => {
                asked.push(($flag, $action))
            };
        }

        if args.doctor {
            add!("--doctor", Action::Doctor);
        }
        let syncing = sync_cli::from_args(args);
        let has_sync = syncing.is_some();
        if let Some(command) = syncing {
            add!("--sync-*", Action::Sync(command));
        }
        if args.internal_refresh_worker {
            add!("--internal-refresh-worker", Action::InternalRefreshWorker);
        }
        if args.daemon {
            add!("--daemon", Action::Daemon);
        }
        if args.client_tail {
            add!("--client-tail", Action::ClientTail);
        }
        if args.click {
            add!("--click", Action::Click);
        }
        if args.wait_change {
            add!("--wait-change", Action::WaitChange);
        }
        // Only an action of its own when no sync command claimed it.
        if args.json && !has_sync {
            add!("--json", Action::Json);
        }
        if let Some(spec) = &args.set_provider {
            add!("--set-provider", Action::SetProvider(spec.clone()));
        }
        if let Some(name) = &args.set_primary {
            add!("--set-primary", Action::SetPrimary(name.clone()));
        }
        if args.check_update {
            add!("--check-update", Action::CheckUpdate);
        }
        if let Some(spec) = &args.install_frontend {
            add!("--install-frontend", Action::InstallFrontend(spec.clone()));
        }
        if let Some(format) = args.export {
            add!("--export", Action::Export(format));
        }
        if args.backfill {
            add!("--backfill", Action::Backfill);
        }
        if args.update {
            add!("--update", Action::Update);
        }
        if args.refresh {
            add!("--refresh", Action::Refresh);
        }
        if let Some(dir) = args.rotate {
            add!("--rotate", Action::Rotate(dir));
        }
        if let Some(target) = args.open {
            add!("--open", Action::Open(target));
        }

        if asked.len() > 1 {
            let flags: Vec<&str> = asked.iter().map(|(flag, _)| *flag).collect();
            anyhow::bail!(
                "{} each ask for a different thing; run them one at a time",
                flags.join(", ")
            );
        }
        Ok(asked.pop().map(|(_, action)| action).unwrap_or(Action::Bar))
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);
    let action = Action::from_args(&args)?;

    // --doctor runs before any of the setup below. It reports on a machine
    // whose config may be missing or unparseable, and writing a default one as
    // a side effect of diagnosing it would hide the very fault being reported.
    if matches!(action, Action::Doctor) {
        std::process::exit(handle_doctor(&config_path));
    }

    if !config_path.exists() {
        write_default_config(&config_path)?;
    }

    let config = load_config(Some(config_path.clone()))?;
    tokengauge_core::install_theme(config.theme.resolve());
    ensure_cache_dir(&config.cache_file)?;
    tokengauge_core::ensure_revision(&config.cache_file);

    match action {
        Action::Doctor => unreachable!("handled before the config is touched"),
        Action::Sync(command) => sync_cli::run(command, &config, &config_path),
        Action::InternalRefreshWorker => {
            worker_do_refresh(&config);
            Ok(())
        }
        Action::Daemon => run_daemon(config, config_path),
        Action::ClientTail => run_client_tail(&config),
        Action::Click => {
            handle_click(&config);
            Ok(())
        }
        Action::WaitChange => {
            wait_for_change(&config, args.wait_timeout);
            Ok(())
        }
        Action::Json => emit_json(&config),
        Action::SetProvider(spec) => handle_set_provider(&config_path, &spec),
        Action::SetPrimary(name) => handle_set_primary(&config, &config_path, &name),
        Action::CheckUpdate => handle_check_update(&config),
        Action::InstallFrontend(spec) => handle_install_frontend(&spec),
        Action::Export(format) => export::run(&config, format, args.since.as_deref()),
        Action::Backfill => handle_backfill(&config),
        Action::Update => handle_update(&config),
        Action::Refresh => {
            // The daemon owns the sentinel while it is up, so ask it first and
            // only fork a worker when nothing answers.
            if try_send_command(&config, &SocketCommand::Refresh).is_err() {
                handle_refresh_quick(&config);
            }
            Ok(())
        }
        Action::Rotate(dir) => {
            let cmd = SocketCommand::Rotate {
                direction: match dir {
                    RotateDir::Next => "next".into(),
                    RotateDir::Prev => "prev".into(),
                },
            };
            if try_send_command(&config, &cmd).is_err() {
                handle_rotate(&config, dir)?;
            }
            Ok(())
        }
        // Open in *this* process, never via the daemon socket. waybar invokes
        // us with the full graphical session env (DISPLAY/WAYLAND_DISPLAY/
        // DBUS/BROWSER); the daemon is started from a stripped systemd env, so
        // a browser it spawns can't reach the running instance and silently
        // opens nothing. handle_open reads the cache directly - no daemon
        // needed to resolve the selected provider's URL.
        Action::Open(target) => {
            handle_open(&config, target);
            Ok(())
        }
        Action::Bar => emit_bar(&config),
    }
}

/// One waybar payload on stdout. The daemon answers if it is up; otherwise this
/// process does the work itself, because a user who never installed the unit
/// still has to get a bar.
fn emit_bar(config: &TokenGaugeConfig) -> Result<()> {
    if let Ok(snapshot) = try_get_snapshot(config) {
        println!("{snapshot}");
        return Ok(());
    }

    if refresh_in_progress(&refresh_sentinel_path(&config.cache_file)) {
        let (rows, errors) = rows_from_cache(config);
        let output = render_output(config, &rows, &errors, true);
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let (payloads, errors, costs) = match maybe_refresh(config) {
        Ok(triple) => triple,
        Err(error) => {
            let output = WaybarOutput {
                text: "\u{27c2}".into(),
                tooltip: format!("<tt>TokenGauge: {}</tt>", pango_escape(&error.to_string())),
                class: "tokengauge-error".into(),
            };
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
    };

    let rows = payload_to_rows_with_costs(payloads, &costs);
    let output = render_output(config, &rows, &errors, false);
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

/// `--set-provider NAME=BOOL`: toggle an OAuth provider in the config, fetch
/// the new set if the cache cannot answer for it, then signal the daemon to
/// reload. Backs every settings pane.
///
/// The fetch belongs here, not in the daemon: frontends run
/// `--set-provider && --json` in one subprocess, and a `--json` that read the
/// cache before anything refetched would answer with no row for the provider
/// just switched on - the toggle would look like it failed until some later
/// poll. Fetching first also leaves the daemon a cache that already covers the
/// new set, so its reload re-renders instead of fetching the same thing again.
fn handle_set_provider(config_path: &Path, spec: &str) -> Result<()> {
    let (name, val) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected NAME=BOOL, got '{spec}'"))?;
    let enabled: bool = val
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid bool '{val}' (want true/false)"))?;
    config_set_oauth_provider(config_path, name.trim(), enabled)?;
    let updated = load_config(Some(config_path.to_path_buf()))?;
    if cache_is_stale(&updated) {
        refresh_inline(&updated);
    } else {
        // Switching one off leaves the snapshot a valid superset, so nothing
        // rewrites it - and a frontend that only watches would keep drawing the
        // provider until its fallback poll. Tell them the config moved.
        tokengauge_core::bump_revision(&updated.cache_file);
    }
    signal_daemon_reload();
    Ok(())
}

/// `--set-primary NAME|highest`: pin the bar to a provider (or clear the pin),
/// then signal the daemon to reload.
fn handle_set_primary(config: &TokenGaugeConfig, config_path: &Path, name: &str) -> Result<()> {
    let primary = match name.trim().to_lowercase().as_str() {
        "highest" | "none" | "" => None,
        other => Some(other.to_string()),
    };
    config_set_primary(config_path, primary.as_deref())?;
    // Nothing refetches for a pin, but every frontend renders it.
    tokengauge_core::bump_revision(&config.cache_file);
    signal_daemon_reload();
    Ok(())
}

/// `--check-update`: live GitHub check, cache result, print JSON status.
fn handle_check_update(config: &TokenGaugeConfig) -> Result<()> {
    let status = update::check(&config.cache_file)?;
    println!("{}", serde_json::to_string(&status)?);
    Ok(())
}

/// `--update`: download the latest release and swap the installed binaries.
fn handle_update(config: &TokenGaugeConfig) -> Result<()> {
    let current = update::current_version();
    println!("Current version: {current}");
    println!("Checking for updates...");
    let applied = update::apply_full(&config.cache_file)?;
    if !update::version_gt(&applied.version, current) {
        println!("Already up to date ({current}).");
        report_frontend_skew(current);
        return Ok(());
    }

    println!("Updated to {}.", applied.version);
    if restart_daemon() {
        println!("Restarted tokengauge-daemon.service.");
    } else {
        println!("Restart to load it: systemctl --user restart tokengauge-daemon.service");
    }
    report_frontends(&applied.frontends);
    Ok(())
}

/// The desktop frontends are QML and JavaScript installed outside the binary
/// directory, so an update that only swapped binaries used to leave them behind
/// - silently, since they report the binary's version rather than their own.
fn report_frontends(outcomes: &[update::FrontendOutcome]) {
    if outcomes.is_empty() {
        return;
    }
    println!();
    for f in outcomes {
        match &f.error {
            Some(e) => eprintln!("{}: NOT updated - {e}", f.label),
            None => match &f.version {
                Some(v) => println!("{} updated to {v}.", f.label),
                None => println!("{} updated.", f.label),
            },
        }
    }
    let hints: Vec<&update::FrontendOutcome> =
        outcomes.iter().filter(|f| f.error.is_none()).collect();
    if hints.is_empty() {
        return;
    }
    println!();
    for f in hints {
        let urgency = if f.needs_session_restart {
            "required"
        } else {
            "to load it"
        };
        println!("  {} ({urgency}): {}", f.label, f.restart_hint);
    }
}

/// An installed frontend that disagrees with the binary is the failure this all
/// exists to catch, so say so even on the path where nothing was updated.
fn report_frontend_skew(binary: &str) {
    use tokengauge_core::frontend;
    for f in frontend::installed() {
        match f.installed_version() {
            Some(v) if v == binary => {}
            Some(v) => println!(
                "{} is still v{v} - update it: tokengauge --install-frontend {}",
                f.label, f.id
            ),
            None => println!(
                "{} has no readable version - reinstall it: tokengauge --install-frontend {}",
                f.label, f.id
            ),
        }
    }
}

/// Re-read every transcript into the history store, and say what came back.
///
/// The first fetch does this by itself. The flag is for the second time it is
/// wanted: transcripts restored from a backup, or a store thrown away.
fn handle_backfill(config: &TokenGaugeConfig) -> Result<()> {
    let today = chrono::Local::now().date_naive();
    let outcome = tokengauge_core::sync::backfill(config, today);
    if let Some(error) = outcome.error {
        anyhow::bail!("{error}");
    }
    println!("read {} calls back to {}", outcome.events, outcome.since);
    Ok(())
}

fn handle_install_frontend(spec: &str) -> Result<()> {
    use tokengauge_core::frontend;

    // Same normalization `find` applies, so `ALL` and a stray space behave.
    let spec = spec.trim().to_lowercase();
    let wanted: Vec<&'static frontend::Frontend> = if spec == "all" {
        frontend::FRONTENDS.iter().collect()
    } else {
        vec![frontend::find(&spec).ok_or_else(|| {
            let ids: Vec<&str> = frontend::FRONTENDS.iter().map(|f| f.id).collect();
            anyhow::anyhow!("unknown frontend '{spec}' (known: {}, all)", ids.join(", "))
        })?]
    };

    let version = update::current_version();
    for target in &wanted {
        println!("Installing the {} from v{version}...", target.label);
    }

    let outcomes = update::install_frontends(&wanted, version)?;
    report_frontends(&outcomes);

    // The error is returned rather than printed here: main prints it once.
    let failed: Vec<&str> = outcomes
        .iter()
        .filter(|o| o.error.is_some())
        .map(|o| o.id)
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{} frontend(s) failed to install: {}",
            failed.len(),
            failed.join(", ")
        ))
    }
}

// ============================================================================
// Daemon + client (Unix socket)
// ============================================================================

fn handle_open(config: &TokenGaugeConfig, target: OpenTarget) {
    // Scoped to the enabled set like every other consumer of the snapshot: the
    // selection resolves by index, so an unfiltered read opens the dashboard of
    // whatever provider the user just switched off.
    let (rows, _) = rows_from_cache(config);
    let Some(idx) = selected_provider_for_tooltip(config, &rows) else {
        // No selection: use first row if any.
        if rows.is_empty() {
            return;
        }
        return open_url_for_provider(&rows[0].provider, target);
    };
    open_url_for_provider(&rows[idx].provider, target);
}

fn open_url_for_provider(provider: &str, target: OpenTarget) {
    let urls = tokengauge_core::provider_urls(provider);
    let url = match target {
        OpenTarget::Dashboard => urls.dashboard,
        OpenTarget::Status => urls.status,
    };
    if let Some(url) = url {
        let _ = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn handle_click(config: &TokenGaugeConfig) {
    let cmd = resolve_click_command(config);
    if cmd.is_empty() {
        return;
    }
    let _ = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Resolve the shell command that the waybar `on-click` should run, based
/// on the user's `[waybar].click_action` plus the matching override field.
/// Empty return = nothing to spawn.
///
/// Terminal discovery lives in the core so every frontend's "open" button can
/// be a spawn of the binary rather than toolkit-specific terminal knowledge.
fn resolve_click_command(config: &TokenGaugeConfig) -> String {
    // The GTK popover is gone, so both actions land on the TUI. A config still
    // set to "popover" resolves here rather than doing nothing on click.
    tokengauge_core::launch::tui_command(config)
}

fn handle_rotate(config: &TokenGaugeConfig, dir: RotateDir) -> Result<()> {
    // Scoped to the enabled set, or scroll would still stop on a provider the
    // user disabled and pin the selection to a row nothing else will render.
    let (rows, _) = rows_from_cache(config);
    if rows.is_empty() {
        return Ok(());
    }

    let state_path = waybar_state_path(&config.cache_file);
    let state = read_waybar_state(&state_path);

    let now = now_ms();
    if now - state.last_rotated_ms < config.waybar.scroll_throttle_ms as i64 {
        return Ok(());
    }

    let current_key = state
        .selected
        .clone()
        .or_else(|| config.waybar.primary.clone());
    let current_idx = current_key
        .as_deref()
        .and_then(|key| {
            let lower = key.to_lowercase();
            rows.iter().position(|r| r.provider.to_lowercase() == lower)
        })
        .unwrap_or(0);

    let len = rows.len();
    let next_idx = match dir {
        RotateDir::Next => (current_idx + 1) % len,
        RotateDir::Prev => (current_idx + len - 1) % len,
    };
    let new_state = WaybarState {
        selected: Some(rows[next_idx].provider.to_lowercase()),
        last_rotated_ms: now,
    };
    write_waybar_state(&state_path, &new_state)?;
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(cache_file: PathBuf) -> TokenGaugeConfig {
        TokenGaugeConfig {
            sync: Default::default(),
            refresh_secs: 600,
            timeout_secs: 10,
            stagger_ms: 0,
            ccusage_enabled: false,
            ccusage_timeout_secs: 15,
            cost_source: tokengauge_core::CostSource::Native,
            cache_file,
            providers: Default::default(),
            waybar: Default::default(),
            notifications: Default::default(),
            theme: Default::default(),
            update: Default::default(),
            unknown: Default::default(),
        }
    }

    fn action_for(argv: &[&str]) -> Result<Action> {
        let mut full = vec!["tokengauge"];
        full.extend_from_slice(argv);
        Action::from_args(&Args::parse_from(full))
    }

    /// The precedence used to be the order of an if-chain nobody had written
    /// down, and a flag that lost simply vanished.
    #[test]
    fn two_actions_at_once_are_refused_rather_than_one_silently_dropped() {
        let error = match action_for(&["--json", "--set-provider", "claude=true"]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("two actions were accepted"),
        };
        assert!(error.contains("--json"), "{error}");
        assert!(error.contains("--set-provider"), "{error}");
    }

    /// `--json` is `--sync-status`'s output format, not a second action - the
    /// pairing is what `--sync-status --json` means and is documented as such.
    #[test]
    fn json_is_a_modifier_on_sync_status_and_not_a_rival_action() {
        assert!(matches!(
            action_for(&["--sync-status", "--json"]).expect("one action"),
            Action::Sync(_)
        ));
    }

    /// No flags is what waybar itself runs, on a timer.
    #[test]
    fn no_flags_draws_the_bar() {
        assert!(matches!(action_for(&[]).expect("default"), Action::Bar));
    }

    #[test]
    fn resolve_click_command_popover_falls_back_to_tui() {
        // A config left on the removed popover action still opens something.
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.waybar.click_action = tokengauge_core::ClickAction::Popover;
        cfg.waybar.tui_command = "  my-term -e tokengauge-tui  ".into();
        assert_eq!(resolve_click_command(&cfg), "my-term -e tokengauge-tui");
    }

    #[test]
    fn resolve_click_command_tui_uses_explicit_override() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.waybar.click_action = tokengauge_core::ClickAction::Tui;
        cfg.waybar.tui_command = "alacritty -e tokengauge-tui".into();
        assert_eq!(resolve_click_command(&cfg), "alacritty -e tokengauge-tui");
    }

    /// A configured command is used verbatim. Auto-detect is deliberately not
    /// asserted on: it scans PATH, so what it picks is the runner's business,
    /// and the test this replaces called it and asserted nothing at all.
    #[test]
    fn a_configured_click_command_is_used_as_written() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.waybar.tui_command = "kitty -e tokengauge-tui".into();
        assert_eq!(resolve_click_command(&cfg), "kitty -e tokengauge-tui");
    }
}
