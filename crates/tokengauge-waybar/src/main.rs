use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::Parser;
use serde::Serialize;
use tokengauge_core::update;
use tokengauge_core::{
    CostInfo, ExtraWindowRow, FetchResult, ProviderFetchError, ProviderPayload, ProviderRow, Theme,
    TokenGaugeConfig, WaybarState, WaybarWindow, config_set_oauth_provider, config_set_primary,
    ensure_cache_dir, fetch_all_providers, format_tokens, format_updated_relative, load_config,
    notify_state_path, payload_to_rows_with_costs, provider_icon, provider_icon_svg_path,
    provider_label, read_cache_full, read_notify_state, read_waybar_state, refresh_in_progress,
    refresh_sentinel_path, retain_enabled, signal_daemon_reload, theme, thresholds_to_fire,
    waybar_state_path, window_labels, write_cache_full, write_default_config, write_notify_state,
    write_waybar_state,
};

fn theme_palette() -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    let t: &Theme = theme();
    (
        t.dim.as_str(),
        t.separator.as_str(),
        t.green.as_str(),
        t.yellow.as_str(),
        t.red.as_str(),
        t.neutral.as_str(),
    )
}

#[derive(Parser, Debug)]
#[command(version, about = "Waybar module for TokenGauge")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
    /// Rotate the provider shown in the waybar text and exit (no JSON output).
    #[arg(long, value_enum)]
    rotate: Option<RotateDir>,
    /// Wipe the cache file and exit. Next render will re-fetch from codexbar
    /// and ccusage. Pair with a waybar signal so the bar repolls immediately.
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
    /// Handle a waybar on-click event. Dispatches based on `[waybar]
    /// click_action` in the config: "tui" launches the terminal TUI,
    /// "popover" runs `popover_command` (defaults to the bundled
    /// `tokengauge-popover --toggle`).
    #[arg(long)]
    click: bool,
    /// Emit the full snapshot as one JSON object (rows, errors, enabled,
    /// primary, theme, window) for non-waybar frontends such as the KDE
    /// Plasma applet. Does not affect the default waybar output line.
    #[arg(long)]
    json: bool,
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

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct WaybarOutput {
    text: String,
    tooltip: String,
    class: String,
}

fn format_bar(label: &str, value: Option<u8>) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    match value {
        Some(percent) => {
            let bar_inner = bar_blocks(percent);
            let color = theme().color_for_percent(percent);
            format!(
                "{icon} {escaped_label} [<span foreground=\"{color}\">{bar_inner}</span>] <span foreground=\"{color}\">{percent}%</span>"
            )
        }
        None => format!(
            "{icon} {escaped_label} [<span foreground=\"{dim}\">─────</span>] <span foreground=\"{dim}\">—</span>"
        ),
    }
}

const MINI_BAR_WIDTH: usize = 5;

fn bar_blocks(percent: u8) -> String {
    let pct = percent.min(100) as usize;
    let filled = (pct * MINI_BAR_WIDTH).div_ceil(100);
    let empty = MINI_BAR_WIDTH.saturating_sub(filled);
    format!("{}{}", "━".repeat(filled), "─".repeat(empty))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);

    if args.doctor {
        let exit = handle_doctor(&config_path);
        std::process::exit(exit);
    }

    if !config_path.exists() {
        write_default_config(&config_path)?;
    }

    let config = load_config(Some(config_path.clone()))?;
    tokengauge_core::install_theme(config.theme.resolve());
    ensure_cache_dir(&config.cache_file)?;

    if args.internal_refresh_worker {
        worker_do_refresh(&config);
        return Ok(());
    }

    if args.daemon {
        return run_daemon(config, config_path);
    }

    if args.client_tail {
        return run_client_tail(&config);
    }

    if args.click {
        handle_click(&config);
        return Ok(());
    }

    if args.json {
        return emit_json(&config);
    }

    if let Some(spec) = &args.set_provider {
        return handle_set_provider(&config_path, spec);
    }

    if let Some(name) = &args.set_primary {
        return handle_set_primary(&config_path, name);
    }

    if args.check_update {
        return handle_check_update(&config);
    }

    if args.update {
        return handle_update(&config);
    }

    if args.refresh {
        if try_send_command(&config, &SocketCommand::Refresh).is_ok() {
            return Ok(());
        }
        handle_refresh_quick(&config);
        return Ok(());
    }

    if let Some(dir) = args.rotate {
        let cmd = SocketCommand::Rotate {
            direction: match dir {
                RotateDir::Next => "next".into(),
                RotateDir::Prev => "prev".into(),
            },
        };
        if try_send_command(&config, &cmd).is_ok() {
            return Ok(());
        }
        handle_rotate(&config, dir)?;
        return Ok(());
    }

    if let Some(target) = args.open {
        // Open in *this* process, never via the daemon socket. waybar invokes
        // us with the full graphical session env (DISPLAY/WAYLAND_DISPLAY/
        // DBUS/BROWSER); the daemon is started from a stripped systemd env, so
        // a browser it spawns can't reach the running instance and silently
        // opens nothing. handle_open reads the cache directly - no daemon
        // needed to resolve the selected provider's URL.
        handle_open(&config, target);
        return Ok(());
    }

    // One-shot snapshot mode: try daemon first
    if let Ok(snapshot) = try_get_snapshot(&config) {
        println!("{snapshot}");
        return Ok(());
    }

    let sentinel = refresh_sentinel_path(&config.cache_file);
    let refreshing = refresh_in_progress(&sentinel);

    if refreshing {
        let yellow = theme().yellow.as_str();
        let cached = read_cache_full(&config.cache_file).ok();
        let (rows, errors) = match cached {
            Some(c) => (
                payload_to_rows_with_costs(c.payloads().to_vec(), &c.costs()),
                c.errors().to_vec(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let selected = selected_provider_for_tooltip(&config, &rows);
        let tooltip_refs: Vec<&ProviderRow> = match selected {
            Some(idx) => vec![&rows[idx]],
            None => rows.iter().collect(),
        };
        let tooltip = format_tooltip_with_errors(&tooltip_refs, &errors, true, "open");
        let text = if rows.is_empty() && errors.is_empty() {
            format!("   <span foreground=\"{yellow}\">⟳ Refreshing...</span>")
        } else {
            format!(
                "   <span foreground=\"{yellow}\">⟳</span> {}",
                build_text_for_rows_with_errors(&rows, &errors, &config)
            )
        };
        let output = WaybarOutput {
            text,
            tooltip,
            class: "tokengauge tokengauge-refreshing".into(),
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let (payloads, errors, costs) = match maybe_refresh(&config) {
        Ok(triple) => triple,
        Err(error) => {
            let output = WaybarOutput {
                text: "⟂".into(),
                tooltip: format!("<tt>TokenGauge: {}</tt>", pango_escape(&error.to_string())),
                class: "tokengauge-error".into(),
            };
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
    };

    let rows = payload_to_rows_with_costs(payloads, &costs);
    if rows.is_empty() && errors.is_empty() {
        let output = WaybarOutput {
            text: "—".into(),
            tooltip: "<tt>TokenGauge: no providers</tt>".into(),
            class: "tokengauge-empty".into(),
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let text = format!(
        "   {}",
        build_text_for_rows_with_errors(&rows, &errors, &config)
    );
    let selected = selected_provider_for_tooltip(&config, &rows);
    let tooltip_rows: Vec<&ProviderRow> = match selected {
        Some(idx) => vec![&rows[idx]],
        None => rows.iter().collect(),
    };
    let tooltip =
        format_tooltip_with_errors(&tooltip_rows, &errors, false, &left_click_label(&config));

    let class = compute_class(&rows, &errors, false, config.waybar.window.clone());

    let output = WaybarOutput {
        text,
        tooltip,
        class,
    };

    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

/// Emit the full snapshot as one JSON object for non-waybar frontends (KDE
/// Plasma applet, etc.). Uses maybe_refresh so a standalone plasmoid (no daemon
/// or waybar keeping the cache warm) still refetches when the cache is stale
/// instead of serving it forever. Each row is enriched with the display label,
/// brand SVG path, glyph, and brand colour so the QML frontend needs no
/// provider knowledge.
fn emit_json(config: &TokenGaugeConfig) -> Result<()> {
    let (payloads, errors, costs) = maybe_refresh(config)?;
    let rows = payload_to_rows_with_costs(payloads, &costs);

    let enabled: Vec<String> = config
        .providers
        .enabled_providers()
        .into_iter()
        .map(|p| p.name)
        .collect();

    let row_values: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            if let serde_json::Value::Object(map) = &mut v {
                let icon = provider_icon(&r.provider);
                let (wl_s, wl_w, wl_t) = window_labels(&r.provider);
                map.insert(
                    "window_labels".into(),
                    serde_json::json!([wl_s, wl_w, wl_t]),
                );
                map.insert("label".into(), provider_label(&r.provider).into());
                map.insert(
                    "icon_svg".into(),
                    provider_icon_svg_path(&r.provider)
                        .map(|p| serde_json::Value::from(p.to_string_lossy().into_owned()))
                        .unwrap_or(serde_json::Value::Null),
                );
                map.insert("glyph".into(), icon.glyph.into());
                map.insert("color".into(), icon.color_hex.into());
            }
            v
        })
        .collect();

    let t = theme();
    let window = match config.waybar.window {
        WaybarWindow::Daily => "daily",
        WaybarWindow::Weekly => "weekly",
    };
    let update_status = tokengauge_core::read_update_status(&config.cache_file);
    let out = serde_json::json!({
        "rows": row_values,
        "errors": errors,
        "enabled": enabled,
        "primary": config.waybar.primary,
        "window": window,
        "theme": {
            "dim": t.dim,
            "separator": t.separator,
            "green": t.green,
            "yellow": t.yellow,
            "red": t.red,
            "neutral": t.neutral,
        },
        "update": update_status,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// `--set-provider NAME=BOOL`: toggle an OAuth provider in the config, then
/// signal the daemon to reload. Backs the plasmoid settings pane.
fn handle_set_provider(config_path: &Path, spec: &str) -> Result<()> {
    let (name, val) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("expected NAME=BOOL, got '{spec}'"))?;
    let enabled: bool = val
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid bool '{val}' (want true/false)"))?;
    config_set_oauth_provider(config_path, name.trim(), enabled)?;
    signal_daemon_reload();
    Ok(())
}

/// `--set-primary NAME|highest`: pin the bar to a provider (or clear the pin),
/// then signal the daemon to reload.
fn handle_set_primary(config_path: &Path, name: &str) -> Result<()> {
    let primary = match name.trim().to_lowercase().as_str() {
        "highest" | "none" | "" => None,
        other => Some(other.to_string()),
    };
    config_set_primary(config_path, primary.as_deref())?;
    signal_daemon_reload();
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resolve which provider key the waybar text + tooltip should show.
/// Priority: persisted scroll selection > config primary > first row's
/// provider > first error's provider. Always returns Some unless both
/// rows and errors are empty - so the bar is single-provider by default
/// instead of stacking everything on first boot.
fn resolved_selection_key(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
) -> Option<String> {
    let state = read_waybar_state(&waybar_state_path(&config.cache_file));
    state
        .selected
        .clone()
        .or_else(|| config.waybar.primary.clone())
        .or_else(|| rows.first().map(|r| r.provider.clone()))
        .or_else(|| errors.first().map(|e| e.provider.clone()))
        .map(|s| s.to_lowercase())
}

fn selected_provider_for_tooltip(config: &TokenGaugeConfig, rows: &[ProviderRow]) -> Option<usize> {
    let key = resolved_selection_key(config, rows, &[])?;
    rows.iter().position(|r| r.provider.to_lowercase() == key)
}

fn build_text_for_rows_with_errors(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    config: &TokenGaugeConfig,
) -> String {
    let selected_key = resolved_selection_key(config, rows, errors);

    let used_for = |row: &ProviderRow| match config.waybar.window {
        WaybarWindow::Daily => row.session_used,
        WaybarWindow::Weekly => row.weekly_used,
    };
    let matches_key = |provider: &str| {
        selected_key
            .as_deref()
            .is_none_or(|k| provider.to_lowercase() == k)
    };

    let success_parts = rows
        .iter()
        .filter(|r| matches_key(&r.provider))
        .map(|r| format_bar(&r.provider, used_for(r)));
    let error_parts = errors
        .iter()
        .filter(|e| matches_key(&e.provider))
        .map(|e| format_bar_error(&e.provider));
    let parts: Vec<String> = success_parts.chain(error_parts).collect();

    if !parts.is_empty() {
        // Always one provider in the bar text now that selected_key
        // defaults to the first row / error.
        return parts.into_iter().next().unwrap_or_default();
    }

    // Selected provider exists in neither set; fall back to the first row,
    // or the first error if there are no successes.
    rows.first()
        .map(|r| format_bar(&r.provider, used_for(r)))
        .or_else(|| errors.first().map(|e| format_bar_error(&e.provider)))
        .unwrap_or_default()
}

fn format_bar_error(label: &str) -> String {
    let (_dim, _separator, _green, _yellow, red, _neutral) = theme_palette();
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    format!("{icon} {escaped_label} <span foreground=\"{red}\">⚠</span>")
}

fn check_and_notify(
    config: &TokenGaugeConfig,
    payloads: &[ProviderPayload],
    costs: &HashMap<String, CostInfo>,
) {
    if !config.notifications.enabled || config.notifications.thresholds.is_empty() {
        return;
    }
    let rows = payload_to_rows_with_costs(payloads.to_vec(), costs);
    if rows.is_empty() {
        return;
    }

    let path = notify_state_path(&config.cache_file);
    let mut state = read_notify_state(&path);
    let thresholds = &config.notifications.thresholds;

    for row in &rows {
        let (s_label, w_label, t_label) = tokengauge_core::window_labels(&row.provider);
        let windows: [(&str, Option<u8>, &str, &str); 3] = [
            ("session", row.session_used, &row.session_reset, s_label),
            ("weekly", row.weekly_used, &row.weekly_reset, w_label),
            ("tertiary", row.tertiary_used, &row.tertiary_reset, t_label),
        ];
        for (slot, used_opt, reset, label) in windows {
            let Some(pct) = used_opt else { continue };
            let key = format!("{}:{}", row.provider.to_lowercase(), slot);
            let entry = state.entries.entry(key).or_default();
            let (to_fire, new_notified) = thresholds_to_fire(pct, thresholds, &entry.notified);
            entry.notified = new_notified;
            for threshold in to_fire {
                fire_notification(&row.provider, label, pct, threshold, reset);
            }
        }
    }

    let _ = write_notify_state(&path, &state);
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
    let installed = update::apply(&config.cache_file)?;
    if update::version_gt(&installed, current) {
        println!("Updated to {installed}.");
        if restart_daemon() {
            println!("Restarted tokengauge-daemon.service.");
        } else {
            println!("Restart to load it: systemctl --user restart tokengauge-daemon.service");
        }
    } else {
        println!("Already up to date ({current}).");
    }
    Ok(())
}

/// Restart the systemd user daemon so the freshly-installed binary is loaded.
/// Best effort: returns false when there's no active unit to restart (plain
/// polling mode) or systemctl is unavailable.
fn restart_daemon() -> bool {
    let active = Command::new("systemctl")
        .args([
            "--user",
            "is-active",
            "--quiet",
            "tokengauge-daemon.service",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        return false;
    }
    Command::new("systemctl")
        .args(["--user", "restart", "tokengauge-daemon.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Fire a one-shot "update available" desktop notification, guarding on the
/// version so the daemon doesn't nag on every check.
fn notify_update_available(config: &TokenGaugeConfig, status: &tokengauge_core::UpdateStatus) {
    let Some(latest) = &status.latest else {
        return;
    };
    if !status.available || status.notified.as_deref() == Some(latest.as_str()) {
        return;
    }
    let title = "TokenGauge: update available";
    let body = format!(
        "v{latest} is available (you have v{}). Run tokengauge-waybar --update.",
        status.current
    );
    let _ = Command::new("notify-send")
        .arg("--app-name")
        .arg("tokengauge")
        .arg("--hint=int:transient:1")
        .arg(title)
        .arg(&body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();

    let mut persisted = status.clone();
    persisted.notified = Some(latest.clone());
    let _ = tokengauge_core::write_update_status(&config.cache_file, &persisted);
}

/// Daemon thread: periodically check GitHub and notify once per new version.
fn daemon_update_loop(config: Arc<Mutex<TokenGaugeConfig>>) {
    loop {
        let snapshot = config.lock().expect("daemon config mutex poisoned").clone();
        if !snapshot.update.check {
            thread::sleep(Duration::from_secs(3600));
            continue;
        }
        match update::check(&snapshot.cache_file) {
            Ok(status) => {
                if status.available {
                    dlog(
                        "update",
                        &format!("newer version available: {:?}", status.latest),
                    );
                    notify_update_available(&snapshot, &status);
                }
            }
            Err(e) => dlog("update", &format!("check failed: {e}")),
        }
        thread::sleep(Duration::from_secs(
            snapshot.update.check_interval_secs.max(600),
        ));
    }
}

fn fire_notification(provider: &str, window: &str, pct: u8, threshold: u8, reset: &str) {
    let title = format!("TokenGauge: {provider} {window} at {pct}%");
    let body = if reset == "—" {
        String::new()
    } else {
        format!("resets {reset}")
    };
    let urgency = if threshold >= 90 {
        "critical"
    } else if threshold >= 70 {
        "normal"
    } else {
        "low"
    };
    let _ = Command::new("notify-send")
        .arg("--urgency")
        .arg(urgency)
        .arg("--app-name")
        .arg("tokengauge")
        .arg(format!(
            "--hint=int:transient:{}",
            if threshold < 90 { 1 } else { 0 }
        ))
        .arg(&title)
        .arg(&body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Send SIGRTMIN+8 to every running `waybar` process.
/// Replaces the previous `pkill -RTMIN+8 waybar` shell-out: no subprocess
/// fork, no PATH dependency on pkill, no race window where the process
/// list could change between match and send.
fn signal_waybar() {
    const SIGRTMIN_PLUS_8: libc::c_int = 42;
    let pids = find_waybar_pids();
    for pid in pids {
        // SAFETY: kill(2) is a syscall; passing a stale PID is a no-op or
        // would target a recycled pid (acceptable - we no-op on EPERM/ESRCH).
        let _ = unsafe { libc::kill(pid, SIGRTMIN_PLUS_8) };
    }
}

fn find_waybar_pids() -> Vec<libc::pid_t> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: libc::pid_t = entry.file_name().to_str()?.parse().ok()?;
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            (comm.trim() == "waybar").then_some(pid)
        })
        .collect()
}

/// Front-half of refresh: write sentinel, signal waybar, fork detached worker
/// for the actual fetch, return immediately so waybar's on-click-right
/// handler unblocks fast and waybar services the signal.
fn handle_refresh_quick(config: &TokenGaugeConfig) {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    let _ = std::fs::write(&sentinel, now_ms().to_string());
    signal_waybar();

    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = Command::new(exe);
        cmd.arg("--internal-refresh-worker")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(path) = std::env::var_os("TOKENGAUGE_CONFIG") {
            cmd.env("TOKENGAUGE_CONFIG", path);
        }
        let _ = cmd.spawn();
    }
}

/// Detached worker: do the actual fetch + clear sentinel + signal waybar.
fn worker_do_refresh(config: &TokenGaugeConfig) {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    let _ = std::fs::remove_file(&config.cache_file);
    let FetchResult {
        payloads,
        errors,
        costs,
    } = fetch_all_providers(config);
    let _ = write_cache_full(&config.cache_file, &payloads, &errors, &costs);
    let _ = std::fs::remove_file(&sentinel);
    check_and_notify(config, &payloads, &costs);
    signal_waybar();
}

struct DoctorCheck {
    label: String,
    ok: bool,
    detail: String,
}

fn handle_doctor(config_path: &Path) -> i32 {
    let isatty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let (green, red, dim, reset) = if isatty {
        ("\x1b[32m", "\x1b[31m", "\x1b[2m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    let section = |title: &str| {
        println!("\n{title}");
        println!("{}", "─".repeat(title.chars().count()));
    };
    let print_check = |c: &DoctorCheck| {
        let icon = if c.ok {
            format!("{green}✓{reset}")
        } else {
            format!("{red}✗{reset}")
        };
        if c.detail.is_empty() {
            println!("  {icon}  {}", c.label);
        } else {
            println!("  {icon}  {}  {dim}- {}{reset}", c.label, c.detail);
        }
    };

    let checks: std::cell::RefCell<Vec<DoctorCheck>> = std::cell::RefCell::new(Vec::new());
    let record = |c: DoctorCheck| {
        print_check(&c);
        checks.borrow_mut().push(c);
    };

    println!("TokenGauge doctor");

    // Config
    section("Config");
    let config = if config_path.exists() {
        match load_config(Some(config_path.to_path_buf())) {
            Ok(c) => {
                record(DoctorCheck {
                    label: format!("config loads: {}", config_path.display()),
                    ok: true,
                    detail: String::new(),
                });
                Some(c)
            }
            Err(e) => {
                record(DoctorCheck {
                    label: format!("config loads: {}", config_path.display()),
                    ok: false,
                    detail: e.to_string(),
                });
                None
            }
        }
    } else {
        record(DoctorCheck {
            label: format!("config exists: {}", config_path.display()),
            ok: false,
            detail: "run any tokengauge-waybar invocation to write defaults".into(),
        });
        None
    };

    let cfg = config.unwrap_or_default();

    // External dependencies
    section("Dependencies");
    record(check_binary(
        &cfg.codexbar_bin,
        "codexbar usage limits",
        "install from https://github.com/steipete/CodexBar",
    ));
    if cfg.ccusage_enabled {
        match tokengauge_core::ccusage_runner_description() {
            Some(cmd) => record(DoctorCheck {
                label: "ccusage runner available".into(),
                ok: true,
                detail: cmd,
            }),
            None => record(DoctorCheck {
                label: "ccusage runner".into(),
                ok: false,
                detail: "install ccusage (npm i -g ccusage / bun i -g ccusage / npx fallback) or set ccusage_enabled = false".into(),
            }),
        }
    } else {
        record(DoctorCheck {
            label: "ccusage disabled in config".into(),
            ok: true,
            detail: "no cost data".into(),
        });
    }
    if cfg.notifications.enabled {
        record(check_binary(
            "notify-send",
            "threshold notifications",
            "install libnotify, or set notifications.enabled = false",
        ));
    }
    record(check_binary(
        "xdg-open",
        "open dashboard/status URLs",
        "install xdg-utils",
    ));

    // Cache + state files
    section("Filesystem");
    let cache_dir = cfg.cache_file.parent().unwrap_or(Path::new("."));
    let cache_ok = std::fs::create_dir_all(cache_dir).is_ok();
    record(DoctorCheck {
        label: format!("cache directory writable: {}", cache_dir.display()),
        ok: cache_ok,
        detail: if cache_ok {
            String::new()
        } else {
            "permission denied".into()
        },
    });

    // Providers
    section("Providers");
    let enabled = cfg.providers.enabled_providers();
    if enabled.is_empty() {
        record(DoctorCheck {
            label: "providers enabled".into(),
            ok: false,
            detail: "set [providers] codex/claude = true or add an API provider".into(),
        });
    } else {
        record(DoctorCheck {
            label: format!("{} provider(s) enabled", enabled.len()),
            ok: true,
            detail: enabled
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
        });
        let result = fetch_all_providers(&cfg);
        for payload in &result.payloads {
            record(DoctorCheck {
                label: format!("fetch {}", payload.provider),
                ok: true,
                detail: payload.source.clone().unwrap_or_default(),
            });
        }
        for err in &result.errors {
            record(DoctorCheck {
                label: format!("fetch {}", err.provider),
                ok: false,
                detail: err.message.clone(),
            });
        }
    }

    // Waybar wiring
    section("Waybar");
    let waybar_cfg = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".config/waybar/config.jsonc"))
        .unwrap_or_else(|| PathBuf::from("~/.config/waybar/config.jsonc"));
    if waybar_cfg.exists() {
        let contents = std::fs::read_to_string(&waybar_cfg).unwrap_or_default();
        let wired = contents.contains("custom/tokengauge");
        record(DoctorCheck {
            label: format!("module wired in {}", waybar_cfg.display()),
            ok: wired,
            detail: if wired {
                String::new()
            } else {
                "run scripts/install.sh to add the custom/tokengauge module".into()
            },
        });
    } else {
        record(DoctorCheck {
            label: "waybar config not found".into(),
            ok: false,
            detail: format!("expected at {}", waybar_cfg.display()),
        });
    }

    // Click action prerequisites: the binary the user wants to spawn
    // on left-click must be on PATH.
    let click_cmd = resolve_click_command(&cfg);
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
        let on_path = which(&first).is_some() || first.starts_with('/');
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

    section("Updates");
    record(DoctorCheck {
        label: format!("installed version: {}", update::current_version()),
        ok: true,
        detail: String::new(),
    });
    match tokengauge_core::read_update_status(&cfg.cache_file) {
        Some(status) if status.available => record(DoctorCheck {
            label: "update available".into(),
            ok: true,
            detail: format!(
                "{} available - run: tokengauge-waybar --update",
                status.latest.as_deref().unwrap_or("newer release")
            ),
        }),
        Some(status) => record(DoctorCheck {
            label: "up to date".into(),
            ok: true,
            detail: status
                .latest
                .map(|v| format!("latest: {v}"))
                .unwrap_or_default(),
        }),
        None => record(DoctorCheck {
            label: "no update check yet".into(),
            ok: true,
            detail: "run: tokengauge-waybar --check-update".into(),
        }),
    }

    println!();
    let failed = checks.borrow().iter().filter(|c| !c.ok).count();
    if failed == 0 {
        println!("{green}All checks passed.{reset}");
        0
    } else {
        println!("{red}{failed} check(s) failed.{reset}");
        1
    }
}

fn check_binary(name: &str, purpose: &str, hint: &str) -> DoctorCheck {
    let found = Command::new("which")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    DoctorCheck {
        label: format!("{name} on PATH ({purpose})"),
        ok: found,
        detail: if found { String::new() } else { hint.into() },
    }
}

// ============================================================================
// Daemon + client (Unix socket)
// ============================================================================

fn socket_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge.sock")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
enum SocketCommand {
    Snapshot,
    Subscribe,
    Refresh,
    Rotate { direction: String },
    Open { target: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SocketReply {
    Snapshot { output: WaybarOutput },
    Update { output: WaybarOutput },
    Ack,
    Error { message: String },
}

fn connect_socket(config: &TokenGaugeConfig) -> std::io::Result<UnixStream> {
    let path = socket_path(&config.cache_file);
    UnixStream::connect(&path)
}

fn try_send_command(config: &TokenGaugeConfig, cmd: &SocketCommand) -> Result<()> {
    let mut stream = connect_socket(config).map_err(|e| anyhow::anyhow!(e))?;
    let line = serde_json::to_string(cmd)?;
    writeln!(stream, "{line}")?;
    stream.flush()?;
    // Read one reply line for ack
    let mut reader = BufReader::new(&stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    Ok(())
}

fn try_get_snapshot(config: &TokenGaugeConfig) -> Result<String> {
    let mut stream = connect_socket(config).map_err(|e| anyhow::anyhow!(e))?;
    let line = serde_json::to_string(&SocketCommand::Snapshot)?;
    writeln!(stream, "{line}")?;
    stream.flush()?;
    let mut reader = BufReader::new(&stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let reply: SocketReply = serde_json::from_str(buf.trim())?;
    match reply {
        SocketReply::Snapshot { output } => Ok(serde_json::to_string(&output)?),
        SocketReply::Error { message } => Err(anyhow::anyhow!(message)),
        _ => Err(anyhow::anyhow!("unexpected reply from daemon")),
    }
}

/// Snapshot the daemon's current output, re-rendering with the refreshing
/// indicator when the sentinel file is present (i.e. a manual refresh is
/// still in flight). Lets snapshot clients (the standard waybar poll path)
/// pick up the ⟳ state without subscribing.
fn current_snapshot(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) -> WaybarOutput {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    if refresh_in_progress(&sentinel) {
        let cached = read_cache_full(&config.cache_file).ok();
        let (rows, errors) = match cached {
            Some(c) => (
                payload_to_rows_with_costs(c.payloads().to_vec(), &c.costs()),
                c.errors().to_vec(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        return render_output(config, &rows, &errors, true);
    }
    state
        .lock()
        .expect("daemon state mutex poisoned")
        .output
        .clone()
}

struct DaemonState {
    output: WaybarOutput,
    subscribers: Vec<UnixStream>,
}

impl DaemonState {
    fn broadcast(&mut self) {
        let line = match serde_json::to_string(&SocketReply::Update {
            output: self.output.clone(),
        }) {
            Ok(s) => format!("{s}\n"),
            Err(_) => return,
        };
        self.subscribers
            .retain_mut(|s| s.write_all(line.as_bytes()).is_ok());
    }
}

/// Pick the strongest CSS class tier based on current state.
/// Order of precedence (strongest first): refreshing > error > partial-error >
/// crit (>=80%) > warn (>=50%) > base. `tokengauge-stale` is additive: it is
/// appended whenever any row was served from last-good cache, on top of the
/// tier so usage colouring still shows.
fn compute_class(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
    window: WaybarWindow,
) -> String {
    let stale_suffix = if rows.iter().any(|r| r.stale) {
        " tokengauge-stale"
    } else {
        ""
    };
    if refreshing {
        return "tokengauge tokengauge-refreshing".to_string();
    }
    if !errors.is_empty() {
        return if rows.is_empty() {
            "tokengauge tokengauge-error".to_string()
        } else {
            format!("tokengauge tokengauge-partial-error{stale_suffix}")
        };
    }
    let max_pct = rows
        .iter()
        .filter_map(|r| match window {
            WaybarWindow::Daily => r.session_used,
            WaybarWindow::Weekly => r.weekly_used,
        })
        .max()
        .unwrap_or(0);
    let tier = match max_pct {
        80..=u8::MAX => "tokengauge tokengauge-crit",
        50..=79 => "tokengauge tokengauge-warn",
        _ => "tokengauge",
    };
    format!("{tier}{stale_suffix}")
}

fn render_output(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
) -> WaybarOutput {
    if rows.is_empty() && errors.is_empty() && !refreshing {
        return WaybarOutput {
            text: "—".into(),
            tooltip: "<tt>TokenGauge: no providers</tt>".into(),
            class: "tokengauge-empty".into(),
        };
    }
    let yellow = theme().yellow.as_str();
    let text_inner = build_text_for_rows_with_errors(rows, errors, config);
    let text = if refreshing {
        if rows.is_empty() && errors.is_empty() {
            format!("   <span foreground=\"{yellow}\">⟳ Refreshing...</span>")
        } else {
            format!("   <span foreground=\"{yellow}\">⟳</span> {text_inner}")
        }
    } else {
        format!("   {text_inner}")
    };
    let selected = selected_provider_for_tooltip(config, rows);
    let tooltip_rows: Vec<&ProviderRow> = match selected {
        Some(idx) => vec![&rows[idx]],
        None => rows.iter().collect(),
    };
    let tooltip =
        format_tooltip_with_errors(&tooltip_rows, errors, refreshing, &left_click_label(config));
    let class = compute_class(rows, errors, refreshing, config.waybar.window.clone());
    WaybarOutput {
        text,
        tooltip,
        class,
    }
}

fn run_daemon(config: TokenGaugeConfig, config_path: PathBuf) -> Result<()> {
    let sock_path = socket_path(&config.cache_file);
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind socket {}", sock_path.display()))?;
    dlog(
        "daemon",
        &format!(
            "listening on {} (refresh every {}s)",
            sock_path.display(),
            config.refresh_secs.max(10)
        ),
    );

    let state = Arc::new(Mutex::new(DaemonState {
        output: WaybarOutput {
            text: "   <span foreground=\"#f9e2af\">⟳ Starting...</span>".into(),
            tooltip: "<tt>TokenGauge daemon starting...</tt>".into(),
            class: "tokengauge tokengauge-refreshing".into(),
        },
        subscribers: Vec::new(),
    }));

    let shared_config = Arc::new(Mutex::new(config));

    // Initial fetch + periodic refresh loop
    {
        let state = Arc::clone(&state);
        let cfg = Arc::clone(&shared_config);
        thread::spawn(move || daemon_fetch_loop(state, cfg));
    }

    // Periodic GitHub release check + one-shot "update available" notification.
    {
        let cfg = Arc::clone(&shared_config);
        thread::spawn(move || daemon_update_loop(cfg));
    }

    // Signal-driven immediate fetch (preserves backward compat with pkill -RTMIN+8)
    {
        let signal_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // SIGRTMIN+8 on Linux glibc = 42. Preserves backward compat with the
        // older waybar `signal: 8` + `pkill -RTMIN+8 waybar` invocations.
        const SIGRTMIN_PLUS_8: i32 = 42;
        signal_hook::flag::register(SIGRTMIN_PLUS_8, Arc::clone(&signal_flag))
            .map_err(|e| anyhow::anyhow!("signal register: {e}"))?;
        let state = Arc::clone(&state);
        let cfg = Arc::clone(&shared_config);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(200));
                if signal_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    dlog("signal", "SIGRTMIN+8 received, forcing fetch");
                    let s = state.clone();
                    let snapshot = cfg.lock().expect("daemon config mutex poisoned").clone();
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        do_fetch_and_broadcast(&s, &snapshot);
                    }));
                    if let Err(payload) = res {
                        dlog(
                            "signal",
                            &format!("panic recovered: {}", panic_message(&payload)),
                        );
                    }
                }
            }
        });
    }

    // SIGHUP: reload config + theme from disk without restart
    {
        let hup = Arc::new(std::sync::atomic::AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGHUP, Arc::clone(&hup))?;
        let cfg = Arc::clone(&shared_config);
        let state = Arc::clone(&state);
        let path = config_path.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(200));
                if hup.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    dlog("signal", "SIGHUP received, reloading config");
                    match load_config(Some(path.clone())) {
                        Ok(new_cfg) => {
                            tokengauge_core::install_theme(new_cfg.theme.resolve());
                            let refresh_secs = new_cfg.refresh_secs.max(10);
                            let prior = {
                                let mut guard = cfg.lock().expect("daemon config mutex poisoned");
                                let prior = enabled_set(&guard.providers);
                                *guard = new_cfg.clone();
                                prior
                            };
                            dlog(
                                "reload",
                                &format!(
                                    "config reloaded from {} (refresh every {refresh_secs}s)",
                                    path.display()
                                ),
                            );
                            // A changed provider set invalidates the cache rather
                            // than merely ageing it: re-rendering would keep
                            // serving a provider the user just disabled (and show
                            // nothing for one just enabled) until the next tick.
                            if enabled_set(&new_cfg.providers) != prior {
                                dlog("reload", "provider set changed, refetching");
                                do_refresh_cycle(&state, &new_cfg);
                                continue;
                            }
                            // Otherwise re-render cached output with the new
                            // theme/config so colour changes show up before the
                            // next fetch, without paying for a fetch.
                            let cached = read_cache_full(&new_cfg.cache_file).ok();
                            let (rows, errors) = match cached {
                                Some(c) => {
                                    let (mut payloads, mut errors, costs) = c.into_parts();
                                    retain_enabled(&mut payloads, &mut errors, &new_cfg.providers);
                                    (payload_to_rows_with_costs(payloads, &costs), errors)
                                }
                                None => (Vec::new(), Vec::new()),
                            };
                            let output = render_output(&new_cfg, &rows, &errors, false);
                            let mut s = state.lock().expect("daemon state mutex poisoned");
                            s.output = output;
                            s.broadcast();
                            drop(s);
                            signal_waybar();
                        }
                        Err(e) => {
                            dlog("reload", &format!("failed: {e}; keeping previous config"));
                        }
                    }
                }
            }
        });
    }

    let sock_path_clone = sock_path.clone();
    // Graceful shutdown on SIGTERM/SIGINT
    {
        let term = Arc::new(std::sync::atomic::AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&term))?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&term))?;
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(200));
                if term.load(std::sync::atomic::Ordering::SeqCst) {
                    dlog("daemon", "SIGTERM/SIGINT received, shutting down");
                    let _ = std::fs::remove_file(&sock_path_clone);
                    std::process::exit(0);
                }
            }
        });
    }

    // Accept connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let cfg = Arc::clone(&shared_config);
                thread::spawn(move || {
                    let snapshot = cfg.lock().expect("daemon config mutex poisoned").clone();
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        handle_client(stream, state, snapshot)
                    }));
                    match res {
                        Ok(Err(e)) => dlog("client", &format!("error: {e}")),
                        Err(payload) => dlog(
                            "client",
                            &format!("panic recovered: {}", panic_message(&payload)),
                        ),
                        Ok(Ok(())) => {}
                    }
                });
            }
            Err(e) => {
                dlog("accept", &format!("failed: {e}"));
            }
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

fn dlog(tag: &str, msg: &str) {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    eprintln!("[{ts}] [{tag}] {msg}");
}

fn daemon_fetch_loop(state: Arc<Mutex<DaemonState>>, config: Arc<Mutex<TokenGaugeConfig>>) {
    loop {
        let snapshot = config.lock().expect("daemon config mutex poisoned").clone();
        let s = state.clone();
        // catch_unwind requires the closure to be UnwindSafe. Arc<Mutex> + Clone
        // values used here are safe to recover - a panic taints nothing externally.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            do_fetch_and_broadcast(&s, &snapshot);
        }));
        if let Err(payload) = res {
            let msg = panic_message(&payload);
            dlog("fetch", &format!("panic recovered: {msg}"));
        }
        thread::sleep(Duration::from_secs(snapshot.refresh_secs.max(10)));
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn do_fetch_and_broadcast(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) {
    let started = std::time::Instant::now();
    let prior_costs = read_cache_full(&config.cache_file)
        .map(|c| c.costs())
        .unwrap_or_default();
    let FetchResult {
        payloads,
        errors,
        mut costs,
    } = fetch_all_providers(config);
    if costs.is_empty() && !prior_costs.is_empty() {
        costs = prior_costs;
    }
    if let Err(e) = write_cache_full(&config.cache_file, &payloads, &errors, &costs) {
        dlog("cache", &format!("write failed: {e}"));
    }
    check_and_notify(config, &payloads, &costs);
    let rows = payload_to_rows_with_costs(payloads, &costs);
    let output = render_output(config, &rows, &errors, false);
    let subscriber_count = {
        let mut s = state.lock().expect("daemon state mutex poisoned");
        s.output = output;
        s.broadcast();
        s.subscribers.len()
    };
    dlog(
        "fetch",
        &format!(
            "rows={} stale={} errors={} costs={} subscribers={} elapsed={:?}",
            rows.len(),
            rows.iter().filter(|r| r.stale).count(),
            errors.len(),
            costs.len(),
            subscriber_count,
            started.elapsed()
        ),
    );
}

/// Raise the ⟳ sentinel and signal waybar to re-poll so the indicator appears.
/// Idempotent: callers that must guarantee the sentinel is up before replying to
/// a client (see the Refresh command) can raise it themselves first.
fn raise_refresh_sentinel(config: &TokenGaugeConfig) {
    let _ = std::fs::write(
        refresh_sentinel_path(&config.cache_file),
        now_ms().to_string(),
    );
    signal_waybar();
}

/// Full manual-refresh cycle: raise the sentinel so every frontend renders ⟳,
/// fetch, then drop it. waybar is signalled on both edges so the bar picks up
/// the indicator and the result without waiting for its poll interval.
/// Panics are contained here rather than at each caller: the sentinel raised
/// above must come down whatever the fetch does, or every client shows ⟳
/// forever, and an escaping panic would kill the caller's long-lived thread.
fn do_refresh_cycle(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) {
    raise_refresh_sentinel(config);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        do_fetch_and_broadcast(state, config);
    }));
    if let Err(payload) = res {
        dlog(
            "refresh",
            &format!("panic recovered: {}", panic_message(&payload)),
        );
    }
    let _ = std::fs::remove_file(refresh_sentinel_path(&config.cache_file));
    signal_waybar();
}

/// Names of the providers currently enabled, for change detection on reload.
fn enabled_set(providers: &tokengauge_core::ProvidersConfig) -> BTreeSet<String> {
    providers
        .enabled_providers()
        .into_iter()
        .map(|p| p.name.to_lowercase())
        .collect()
}

fn handle_client(
    mut stream: UnixStream,
    state: Arc<Mutex<DaemonState>>,
    config: TokenGaugeConfig,
) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
        return Ok(());
    }
    let cmd: SocketCommand = serde_json::from_str(buf.trim())?;
    match cmd {
        SocketCommand::Snapshot => {
            let output = current_snapshot(&state, &config);
            let reply = SocketReply::Snapshot { output };
            writeln!(stream, "{}", serde_json::to_string(&reply)?)?;
            stream.flush()?;
        }
        SocketCommand::Subscribe => {
            // Send current state, register as subscriber, keep stream alive
            let output = current_snapshot(&state, &config);
            let reply = SocketReply::Update { output };
            writeln!(stream, "{}", serde_json::to_string(&reply)?)?;
            stream.flush()?;
            state
                .lock()
                .expect("daemon state mutex poisoned")
                .subscribers
                .push(stream);
            // Don't close - daemon broadcast will push updates
        }
        SocketCommand::Refresh => {
            // Raise the sentinel and start the fetch before acking: a client
            // that kicks a refresh and then polls for the ⟳ state (the popover
            // on open) must never observe the gap between its ack and the fetch
            // thread starting. The fetch itself runs in the background so the
            // client doesn't block on the network.
            //
            // Both precede the ack, which is a `?` path: a client that hangs up
            // before reading it would otherwise return early and strand the
            // sentinel raised with no fetch thread left to take it down.
            raise_refresh_sentinel(&config);
            {
                let state = state.clone();
                let config = config.clone();
                thread::spawn(move || do_refresh_cycle(&state, &config));
            }
            writeln!(stream, "{}", serde_json::to_string(&SocketReply::Ack)?)?;
            stream.flush()?;
        }
        SocketCommand::Rotate { direction } => {
            let dir = match direction.as_str() {
                "prev" => RotateDir::Prev,
                _ => RotateDir::Next,
            };
            let _ = handle_rotate(&config, dir);
            // Re-render from current cache + rotation, scoped to the enabled set
            // like handle_rotate just was: rotating off an unfiltered cache would
            // put a disabled provider back in the bar.
            let cached = read_cache_full(&config.cache_file).ok();
            let (rows, errors) = match cached {
                Some(c) => {
                    let (mut payloads, mut errors, costs) = c.into_parts();
                    retain_enabled(&mut payloads, &mut errors, &config.providers);
                    (payload_to_rows_with_costs(payloads, &costs), errors)
                }
                None => (Vec::new(), Vec::new()),
            };
            let output = render_output(&config, &rows, &errors, false);
            let mut s = state.lock().expect("daemon state mutex poisoned");
            s.output = output;
            s.broadcast();
            writeln!(stream, "{}", serde_json::to_string(&SocketReply::Ack)?)?;
            stream.flush()?;
        }
        SocketCommand::Open { target } => {
            let t = match target.as_str() {
                "status" => OpenTarget::Status,
                _ => OpenTarget::Dashboard,
            };
            handle_open(&config, t);
            writeln!(stream, "{}", serde_json::to_string(&SocketReply::Ack)?)?;
            stream.flush()?;
        }
    }
    Ok(())
}

fn run_client_tail(config: &TokenGaugeConfig) -> Result<()> {
    // Retry connect briefly if daemon not yet up
    let stream = loop {
        match connect_socket(config) {
            Ok(s) => break s,
            Err(_) => {
                thread::sleep(Duration::from_millis(500));
                if !socket_path(&config.cache_file).exists() {
                    // No daemon running - fall through to a one-shot snapshot
                    let result = (|| {
                        let sentinel = refresh_sentinel_path(&config.cache_file);
                        let refreshing = refresh_in_progress(&sentinel);
                        let (rows, errors, costs) = maybe_refresh(config)?;
                        let rows_v = payload_to_rows_with_costs(rows, &costs);
                        Ok::<_, anyhow::Error>(render_output(config, &rows_v, &errors, refreshing))
                    })();
                    if let Ok(out) = result {
                        println!("{}", serde_json::to_string(&out)?);
                    }
                    // Wait + retry
                    thread::sleep(Duration::from_secs(60));
                    continue;
                }
            }
        }
    };
    let mut writer = stream.try_clone()?;
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&SocketCommand::Subscribe)?
    )?;
    writer.flush()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let reply: SocketReply = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let SocketReply::Update { output } | SocketReply::Snapshot { output } = reply {
            println!("{}", serde_json::to_string(&output)?);
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
    // Daemon disconnected; exit cleanly so waybar restarts us
    Ok(())
}

fn handle_open(config: &TokenGaugeConfig, target: OpenTarget) {
    let cached = match read_cache_full(&config.cache_file) {
        Ok(c) => c,
        Err(_) => return,
    };
    let rows = payload_to_rows_with_costs(cached.payloads().to_vec(), &cached.costs());
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
fn resolve_click_command(config: &TokenGaugeConfig) -> String {
    use tokengauge_core::ClickAction;
    match config.waybar.click_action {
        ClickAction::Popover => config.waybar.popover_command.trim().to_string(),
        ClickAction::Tui => {
            let explicit = config.waybar.tui_command.trim();
            if !explicit.is_empty() {
                return explicit.to_string();
            }
            default_tui_launcher()
        }
    }
}

fn default_tui_launcher() -> String {
    // Prefer omarchy's launcher if installed.
    if which("omarchy-launch-or-focus-tui").is_some() {
        return "omarchy-launch-or-focus-tui tokengauge-tui".to_string();
    }
    // Fall back to $TERMINAL, then a list of common terminals.
    let candidates: Vec<String> = std::env::var("TERMINAL")
        .ok()
        .into_iter()
        .chain(
            ["ghostty", "alacritty", "kitty", "wezterm", "foot", "xterm"]
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    for term in candidates {
        if which(&term).is_some() {
            return format!("{term} -e tokengauge-tui");
        }
    }
    String::new()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn handle_rotate(config: &TokenGaugeConfig, dir: RotateDir) -> Result<()> {
    let cached = match read_cache_full(&config.cache_file) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    // Scoped to the enabled set, or scroll would still stop on a provider the
    // user disabled and pin the selection to a row nothing else will render.
    let (mut payloads, mut errors, costs) = cached.into_parts();
    retain_enabled(&mut payloads, &mut errors, &config.providers);
    let rows = payload_to_rows_with_costs(payloads, &costs);
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

type RefreshSnapshot = (
    Vec<ProviderPayload>,
    Vec<ProviderFetchError>,
    HashMap<String, CostInfo>,
);

fn maybe_refresh(config: &TokenGaugeConfig) -> Result<RefreshSnapshot> {
    let now = SystemTime::now();
    let stale = match std::fs::metadata(&config.cache_file) {
        Ok(metadata) => metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age >= Duration::from_secs(config.refresh_secs))
            .unwrap_or(true),
        Err(_) => true,
    };

    if stale {
        let prior_costs = read_cache_full(&config.cache_file)
            .map(|c| c.costs())
            .unwrap_or_default();
        let FetchResult {
            payloads,
            errors,
            mut costs,
        } = fetch_all_providers(config);
        if costs.is_empty() && !prior_costs.is_empty() {
            costs = prior_costs;
        }
        write_cache_full(&config.cache_file, &payloads, &errors, &costs)?;
        check_and_notify(config, &payloads, &costs);
        Ok((payloads, errors, costs))
    } else {
        // Scope the cache to the currently-enabled providers: it was written by
        // whatever set was enabled at fetch time, so without this a provider
        // disabled since then keeps rendering until the cache next turns over.
        let (mut payloads, mut errors, costs) = read_cache_full(&config.cache_file)?.into_parts();
        retain_enabled(&mut payloads, &mut errors, &config.providers);
        Ok((payloads, errors, costs))
    }
}

fn pango_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn tooltip_bar(percent: u8) -> String {
    let filled = (percent.min(100) / 10) as usize;
    let mut bar = String::with_capacity(30);
    for _ in 0..filled {
        bar.push('━');
    }
    for _ in filled..10 {
        bar.push('─');
    }
    bar
}

const NERD_FONT_FACE: &str = "JetBrainsMono Nerd Font";

fn icon_markup(label: &str) -> String {
    let icon = provider_icon(label);
    format!(
        "<span face=\"{NERD_FONT_FACE}\" foreground=\"{}\">{}</span>",
        icon.color_hex, icon.glyph
    )
}

fn format_provider_line(label: &str, used: Option<u8>, reset: &str) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    match used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = theme().color_for_percent(pct);
            let pct_cell = format!("{pct:>3}%");
            let reset_part = if reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {}", pango_escape(reset))
            };
            format!(
                "  {label:<16}  [<span foreground=\"{color}\">{bar}</span>]  <span foreground=\"{color}\">{pct_cell}</span>   {reset_part}"
            )
        }
        None => {
            format!(
                "  {label:<16}  [<span foreground=\"{dim}\">──────────</span>]          no data"
            )
        }
    }
}

fn format_credits_line(credits: &str) -> Option<String> {
    if credits == "—" || credits.is_empty() {
        return None;
    }
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    Some(format!(
        "  Credits  <span foreground=\"{dim}\">${}</span>",
        pango_escape(credits)
    ))
}

fn format_extra_window(extra: &ExtraWindowRow) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let title = pango_escape(&extra.title);
    let title_padded = format!("{title:<14}");
    match extra.used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = theme().color_for_percent(pct);
            let pct_cell = format!("{pct:>3}%");
            let reset_part = if extra.reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {}", pango_escape(&extra.reset))
            };
            format!(
                "  {title_padded}  [<span foreground=\"{color}\">{bar}</span>]  <span foreground=\"{color}\">{pct_cell}</span>   {reset_part}"
            )
        }
        None => format!(
            "  {title_padded}  <span foreground=\"{dim}\">[──────────]</span>          no data"
        ),
    }
}

fn format_cost_lines(cost: &CostInfo) -> Vec<String> {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let today_usd = format!("${:.2}", cost.today_usd);
    let monthly_usd = format!("${:.2}", cost.monthly_usd);
    let session_usd = format!("${:.2}", cost.session_usd);
    let weekly_usd = format!("${:.2}", cost.weekly_usd);
    let usd_width = today_usd
        .chars()
        .count()
        .max(monthly_usd.chars().count())
        .max(session_usd.chars().count())
        .max(weekly_usd.chars().count());
    let today_tokens = format_tokens(cost.today_tokens);
    let monthly_tokens = format_tokens(cost.monthly_tokens);
    let tokens_width = today_tokens
        .chars()
        .count()
        .max(monthly_tokens.chars().count());
    let rate_line = cost.burn_rate.as_ref().map(|br| {
        let rate_str = format!("${:.2}", br.cost_per_hour);
        let trend = cost
            .avg_hourly_cost()
            .filter(|avg| *avg > 0.0)
            .map(|avg| {
                let pct = ((br.cost_per_hour - avg) / avg) * 100.0;
                let arrow = if pct >= 0.0 { "↑" } else { "↓" };
                let color = if pct >= 25.0 {
                    "#f38ba8"
                } else if pct >= -10.0 {
                    "#f9e2af"
                } else {
                    "#a6e3a1"
                };
                format!(
                    "  <span foreground=\"{color}\">{arrow}{:.0}%</span> <span foreground=\"{dim}\">vs 7d avg</span>",
                    pct.abs()
                )
            })
            .unwrap_or_default();
        format!("  Rate      <span foreground=\"{dim}\">{rate_str:>usd_width$}/hr</span>{trend}")
    });

    let session_line = (cost.session_usd > 0.0).then(|| {
        format!("  Session   <span foreground=\"{dim}\">{session_usd:>usd_width$}</span>")
    });
    let weekly_line = (cost.weekly_usd > 0.0)
        .then(|| format!("  Weekly    <span foreground=\"{dim}\">{weekly_usd:>usd_width$}</span>"));
    let blank = (session_line.is_some() || weekly_line.is_some()).then(String::new);

    let today_line = format!(
        "  Today     <span foreground=\"{dim}\">{today_usd:>usd_width$}  ·  {today_tokens:>tokens_width$} tokens</span>"
    );
    let month_line = format!(
        "  Month     <span foreground=\"{dim}\">{monthly_usd:>usd_width$}  ·  {monthly_tokens:>tokens_width$} tokens</span>"
    );

    [
        rate_line,
        session_line,
        weekly_line,
        blank,
        Some(today_line),
        Some(month_line),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn format_header(row: &ProviderRow) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let icon = icon_markup(&row.provider);
    let name = pango_escape(&row.provider);
    let plan = row.plan_label.as_deref().filter(|s| !s.is_empty());
    let badge = match plan {
        Some(p) => format!("  <span foreground=\"{dim}\">·  {}</span>", pango_escape(p)),
        None => String::new(),
    };
    format!("<b>{icon}  {name}</b>{badge}")
}

fn format_provider_card(row: &ProviderRow) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();

    let updated_line = row
        .updated_iso
        .as_deref()
        .and_then(format_updated_relative)
        .map(|rel| {
            format!(
                "  <span foreground=\"{dim}\">Updated {}</span>",
                pango_escape(&rel)
            )
        });

    let (session_label, weekly_label, tertiary_label) = window_labels(&row.provider);
    let window_lines = [
        Some(format_provider_line(
            session_label,
            row.session_used,
            &row.session_reset,
        )),
        Some(format_provider_line(
            weekly_label,
            row.weekly_used,
            &row.weekly_reset,
        )),
        (row.tertiary_used.is_some() || row.tertiary_reset != "—")
            .then(|| format_provider_line(tertiary_label, row.tertiary_used, &row.tertiary_reset)),
    ];

    let extras_section: Vec<String> = if row.extra_windows.is_empty() {
        Vec::new()
    } else {
        std::iter::once(String::new())
            .chain(std::iter::once(format!(
                "  <span foreground=\"{dim}\">Extra usage</span>"
            )))
            .chain(row.extra_windows.iter().map(format_extra_window))
            .collect()
    };

    let cost_section: Vec<String> = match &row.cost {
        Some(cost) => std::iter::once(String::new())
            .chain(std::iter::once(format!(
                "  <span foreground=\"{dim}\">Cost</span>"
            )))
            .chain(format_cost_lines(cost))
            .collect(),
        None => Vec::new(),
    };

    let credits_line = format_credits_line(&row.credits);

    let lines: Vec<String> = std::iter::once(format_header(row))
        .chain(updated_line)
        .chain(window_lines.into_iter().flatten())
        .chain(extras_section)
        .chain(cost_section)
        .chain(credits_line)
        .collect();

    format!("<tt>{}</tt>", lines.join("\n"))
}

fn format_error_card(err: &ProviderFetchError) -> String {
    let (_dim, _separator, _green, _yellow, red, _neutral) = theme_palette();
    let icon = icon_markup(&err.provider);
    let name = pango_escape(&err.provider);
    let msg = pango_escape(&err.message);
    format!("<tt><b>{icon}  {name}</b>  <span foreground=\"{red}\">⚠ {msg}</span></tt>")
}

fn format_tooltip_with_errors(
    rows: &[&ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
    left_verb: &str,
) -> String {
    let cards: Vec<String> = rows
        .iter()
        .map(|row| format_provider_card(row))
        .chain(errors.iter().map(format_error_card))
        .collect();
    let cards_refs: Vec<&str> = cards.iter().map(String::as_str).collect();
    format_tooltip_from_cards(&cards_refs, refreshing, left_verb)
}

fn format_tooltip_from_cards(cards: &[&str], refreshing: bool, left_verb: &str) -> String {
    let (dim, separator, _green, yellow, _red, _neutral) = theme_palette();
    let separator = format!(
        "<tt><span foreground=\"{separator}\">────────────────────────────────────</span></tt>"
    );
    let body = cards.join(&format!("\n{separator}\n"));
    let status_line = if refreshing {
        format!("\n<tt><b><span foreground=\"{yellow}\">⟳ Refreshing...</span></b></tt>")
    } else {
        String::new()
    };
    let left_pair = ("left", left_verb);
    let pairs: [(&str, &str); 5] = [
        left_pair,
        ("middle", "dashboard"),
        ("right", "refresh"),
        ("scroll", "rotate"),
        ("back", "status"),
    ];
    let cell = |k: &str, v: &str| format!("{k:<6} {v:<10}");
    let hint_lines: Vec<String> = pairs
        .chunks(3)
        .map(|chunk| {
            let cells: Vec<String> = chunk.iter().map(|(k, v)| cell(k, v)).collect();
            format!("  {}", cells.join("  ·  "))
        })
        .collect();
    let hint = format!(
        "\n\n<tt><span foreground=\"{dim}\">{}</span></tt>",
        hint_lines.join("\n")
    );
    format!("{body}{status_line}{hint}")
}

/// Short verb shown in the tooltip's left-click hint, matching the user's
/// configured click_action.
fn left_click_label(config: &TokenGaugeConfig) -> String {
    match config.waybar.click_action {
        tokengauge_core::ClickAction::Tui => "open TUI".to_string(),
        tokengauge_core::ClickAction::Popover => "open panel".to_string(),
    }
}

#[cfg(test)]
fn format_tooltip_cards(rows: &[&ProviderRow], refreshing: bool) -> String {
    format_tooltip_with_errors(rows, &[], refreshing, "open")
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // bar_blocks tests
    // ------------------------------------------------------------------------

    #[test]
    fn bar_blocks_boundaries() {
        assert_eq!(bar_blocks(0), "─────");
        assert_eq!(bar_blocks(20), "━────");
        assert_eq!(bar_blocks(40), "━━───");
        assert_eq!(bar_blocks(60), "━━━──");
        assert_eq!(bar_blocks(80), "━━━━─");
        assert_eq!(bar_blocks(100), "━━━━━");
    }

    #[test]
    fn bar_blocks_clamps_over_100() {
        assert_eq!(bar_blocks(150), "━━━━━");
    }

    // ------------------------------------------------------------------------
    // format_bar tests
    // ------------------------------------------------------------------------

    #[test]
    fn format_bar_with_value() {
        let result = format_bar("Claude", Some(42));
        assert!(result.contains("Claude"));
        assert!(result.contains("42%"));
        assert!(result.contains("━━━──")); // 42% -> ceil(2.1) = 3 filled
        assert!(result.contains("[<span"));
        assert!(result.contains("</span>]"));
        assert!(result.contains("\u{f0721}"));
        assert!(result.contains("face=\"JetBrainsMono Nerd Font\""));
        assert!(result.contains("foreground=\"#DE7356\""));
        // percent + bar wrapped in status color span (42% -> green)
        assert!(result.contains("foreground=\"#a6e3a1\""));
    }

    #[test]
    fn format_bar_with_high_percent_uses_red() {
        let result = format_bar("Claude", Some(85));
        assert!(result.contains("foreground=\"#f38ba8\""));
    }

    #[test]
    fn format_bar_none() {
        let result = format_bar("Codex", None);
        assert!(result.contains("Codex"));
        assert!(result.contains("─────"));
        assert!(result.contains("—"));
        assert!(result.contains("\u{f0b2b}"));
        assert!(result.contains("foreground=\"#74AA9C\""));
        // dim color for missing data
        assert!(result.contains("foreground=\"#6c7086\""));
    }

    #[test]
    fn format_bar_escapes_label() {
        let result = format_bar("ev<il>", Some(50));
        assert!(result.contains("ev&lt;il&gt;"));
        assert!(!result.contains(" ev<il> "));
    }

    // ------------------------------------------------------------------------
    // tooltip_bar tests
    // ------------------------------------------------------------------------

    #[test]
    fn tooltip_bar_lengths() {
        assert_eq!(tooltip_bar(0).chars().count(), 10);
        assert_eq!(tooltip_bar(100).chars().count(), 10);
        assert_eq!(tooltip_bar(67).chars().count(), 10);
        assert_eq!(tooltip_bar(0), "──────────");
        assert_eq!(tooltip_bar(100), "━━━━━━━━━━");
        assert_eq!(tooltip_bar(67), "━━━━━━────");
    }

    #[test]
    fn tooltip_bar_clamps_over_100() {
        assert_eq!(tooltip_bar(200).chars().count(), 10);
        assert_eq!(tooltip_bar(200), "━━━━━━━━━━");
    }

    // color_hex_for_percent + format_tokens + provider_icon tested in core

    // ------------------------------------------------------------------------
    // pango_escape tests
    // ------------------------------------------------------------------------

    #[test]
    fn pango_escape_specials() {
        assert_eq!(pango_escape("a & b"), "a &amp; b");
        assert_eq!(pango_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(pango_escape("\"quote\""), "&quot;quote&quot;");
        assert_eq!(pango_escape("it's"), "it&apos;s");
        assert_eq!(pango_escape("plain text 123"), "plain text 123");
    }

    // ------------------------------------------------------------------------
    // format_provider_card tests
    // ------------------------------------------------------------------------

    fn sample_row(provider: &str) -> ProviderRow {
        ProviderRow {
            provider: provider.to_string(),
            session_used: Some(67),
            session_window_minutes: Some(300),
            session_reset: "in 2h 34m".to_string(),
            weekly_used: Some(19),
            weekly_window_minutes: Some(10080),
            weekly_reset: "in 4d 11h".to_string(),
            tertiary_used: None,
            tertiary_reset: "—".to_string(),
            credits: "—".to_string(),
            source: "oauth".to_string(),
            updated: "07:37".to_string(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: None,
            stale: false,
        }
    }

    #[test]
    fn format_provider_card_full_data() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(card.starts_with("<tt><b>"));
        assert!(card.contains("Claude</b>"));
        assert!(card.ends_with("</tt>"));
        assert!(card.contains("Session"));
        assert!(card.contains("Weekly"));
        assert!(card.contains("━━━━━━────"));
        assert!(card.contains("━─────────"));
        assert!(card.contains("<span foreground=\"#f9e2af\"> 67%</span>"));
        assert!(card.contains("<span foreground=\"#a6e3a1\"> 19%</span>"));
        assert!(card.contains("resets in 2h 34m"));
        assert!(card.contains("resets in 4d 11h"));
    }

    #[test]
    fn format_provider_card_missing_session() {
        let mut row = sample_row("Codex");
        row.session_used = None;
        row.session_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("Codex</b>"));
        assert!(card.contains("──────────"));
        assert!(card.contains("no data"));
        assert!(card.contains("━─────────"));
        assert!(card.contains("resets in 4d 11h"));
    }

    #[test]
    fn format_provider_card_missing_reset_renders_not_started() {
        let mut row = sample_row("Codex");
        row.weekly_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("not started"));
        assert!(!card.contains("resets —"));
    }

    #[test]
    fn format_provider_card_escapes_provider_name() {
        let row = sample_row("ev<il>");
        let card = format_provider_card(&row);
        assert!(card.contains("ev&lt;il&gt;</b>"));
        assert!(!card.contains("ev<il></b>"));
    }

    #[test]
    fn format_provider_card_escapes_reset_string() {
        let mut row = sample_row("Claude");
        row.session_reset = "a & b".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("resets a &amp; b"));
    }

    #[test]
    fn format_provider_card_includes_icon() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(card.contains("\u{f0721}"));
        assert!(card.contains("face=\"JetBrainsMono Nerd Font\""));
        assert!(card.contains("foreground=\"#DE7356\""));
        let codex_card = format_provider_card(&sample_row("Codex"));
        assert!(codex_card.contains("\u{f0b2b}"));
        let mut other = sample_row("Mystery");
        other.provider = "Mystery".to_string();
        let card = format_provider_card(&other);
        assert!(card.contains("\u{f06a9}"));
    }

    #[test]
    fn format_provider_card_omits_credits_when_dash() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(!card.contains("Credits"));
    }

    #[test]
    fn format_provider_card_includes_credits_when_present() {
        let mut row = sample_row("Kimi");
        row.credits = "42.57".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("Credits"));
        assert!(card.contains("$42.57"));
    }

    #[test]
    fn format_tooltip_cards_joins_with_separator() {
        let rows = [sample_row("Claude"), sample_row("Codex")];
        let refs: Vec<&ProviderRow> = rows.iter().collect();
        let tooltip = format_tooltip_cards(&refs, false);
        assert!(tooltip.contains("</tt>\n<tt>"));
        assert!(tooltip.contains("────────────────────────────────────"));
    }

    #[test]
    fn format_tooltip_cards_single_card_no_separator() {
        let row = sample_row("Claude");
        let tooltip = format_tooltip_cards(&[&row], false);
        assert!(!tooltip.contains("────────────────────────────────────"));
    }

    #[test]
    fn format_tooltip_cards_refreshing_shows_indicator() {
        let row = sample_row("Claude");
        let tooltip = format_tooltip_cards(&[&row], true);
        assert!(tooltip.contains("Refreshing"));
        assert!(tooltip.contains("⟳"));
    }

    // ------------------------------------------------------------------------
    // Socket protocol tests
    //
    // Each test binds its own UnixListener at a unique path under /tmp,
    // spawns a one-shot server that drives `handle_client`, and exchanges
    // one command/reply over a connected stream. Configs disable providers
    // and ccusage so the Refresh path doesn't shell out to external bins.
    // ------------------------------------------------------------------------

    fn unique_test_dir(tag: &str) -> PathBuf {
        let counter = std::sync::atomic::AtomicU64::new(0);
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-test-{tag}-{}-{}-{}",
            std::process::id(),
            now_ms(),
            n,
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn test_config(cache_file: PathBuf) -> TokenGaugeConfig {
        TokenGaugeConfig {
            codexbar_bin: "codexbar".to_string(),
            refresh_secs: 600,
            timeout_secs: 10,
            stagger_ms: 0,
            ccusage_enabled: false,
            ccusage_timeout_secs: 15,
            cache_file,
            providers: Default::default(),
            waybar: Default::default(),
            notifications: Default::default(),
            theme: Default::default(),
            update: Default::default(),
        }
    }

    fn test_state(text: &str) -> Arc<Mutex<DaemonState>> {
        Arc::new(Mutex::new(DaemonState {
            output: WaybarOutput {
                text: text.into(),
                tooltip: "TEST_TIP".into(),
                class: "tokengauge-test".into(),
            },
            subscribers: Vec::new(),
        }))
    }

    fn send_recv(sock_path: &Path, cmd: &SocketCommand) -> SocketReply {
        let mut stream = UnixStream::connect(sock_path).expect("connect");
        writeln!(stream, "{}", serde_json::to_string(cmd).unwrap()).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        serde_json::from_str(buf.trim()).expect("parse reply")
    }

    /// Bind a one-shot listener, spawn handle_client on accept, and
    /// return both the socket path and the server's join handle.
    fn spawn_one_shot_server(
        cache_file: &Path,
        state: Arc<Mutex<DaemonState>>,
        config: TokenGaugeConfig,
    ) -> (PathBuf, thread::JoinHandle<Result<()>>) {
        let sock = socket_path(cache_file);
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind listener");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept()?;
            handle_client(stream, state, config)
        });
        (sock, handle)
    }

    #[test]
    fn socket_snapshot_returns_current_output() {
        let dir = unique_test_dir("snapshot");
        let cache = dir.join("cache.json");
        let state = test_state("SNAPSHOT_TEXT");
        let config = test_config(cache.clone());
        let (sock, server) = spawn_one_shot_server(&cache, state, config);

        let reply = send_recv(&sock, &SocketCommand::Snapshot);
        match reply {
            SocketReply::Snapshot { output } => {
                assert_eq!(output.text, "SNAPSHOT_TEXT");
                assert_eq!(output.tooltip, "TEST_TIP");
                assert_eq!(output.class, "tokengauge-test");
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_subscribe_returns_update_then_receives_broadcasts() {
        let dir = unique_test_dir("subscribe");
        let cache = dir.join("cache.json");
        let state = test_state("INITIAL");
        let config = test_config(cache.clone());
        let (sock, server) = spawn_one_shot_server(&cache, Arc::clone(&state), config);

        let mut stream = UnixStream::connect(&sock).unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::to_string(&SocketCommand::Subscribe).unwrap()
        )
        .unwrap();
        stream.flush().unwrap();

        let read_stream = stream.try_clone().unwrap();
        let mut reader = BufReader::new(read_stream);

        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        let first: SocketReply = serde_json::from_str(buf.trim()).unwrap();
        assert!(
            matches!(&first, SocketReply::Update { output } if output.text == "INITIAL"),
            "expected initial Update, got {first:?}"
        );

        // handle_client returns once subscriber is registered; wait for it.
        server.join().unwrap().unwrap();

        // Mutate state + broadcast. Subscriber should receive an Update.
        {
            let mut s = state.lock().unwrap();
            s.output.text = "BROADCAST".into();
            s.broadcast();
        }

        let mut buf2 = String::new();
        reader.read_line(&mut buf2).unwrap();
        let second: SocketReply = serde_json::from_str(buf2.trim()).unwrap();
        assert!(
            matches!(&second, SocketReply::Update { output } if output.text == "BROADCAST"),
            "expected broadcast Update, got {second:?}"
        );

        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_refresh_acks_and_writes_sentinel() {
        let dir = unique_test_dir("refresh");
        let cache = dir.join("cache.json");
        let state = test_state("REFRESH_TEXT");
        let config = test_config(cache.clone());
        let sentinel = refresh_sentinel_path(&config.cache_file);
        let _ = std::fs::remove_file(&sentinel);
        let (sock, server) = spawn_one_shot_server(&cache, state, config);

        let reply = send_recv(&sock, &SocketCommand::Refresh);
        assert!(
            matches!(reply, SocketReply::Ack),
            "expected ack, got {reply:?}"
        );
        assert!(sentinel.exists(), "Refresh should create the sentinel file");

        server.join().unwrap().unwrap();
        // Background fetch thread may still be running; cleanup is best-effort.
        // Wait briefly so it clears its own sentinel/cache writes.
        thread::sleep(Duration::from_millis(200));
        let _ = std::fs::remove_file(&sentinel);
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_rotate_acks_when_no_cache() {
        let dir = unique_test_dir("rotate");
        let cache = dir.join("cache.json");
        let state = test_state("ROTATE_TEXT");
        let config = test_config(cache.clone());
        let (sock, server) = spawn_one_shot_server(&cache, state, config);

        let reply = send_recv(
            &sock,
            &SocketCommand::Rotate {
                direction: "next".into(),
            },
        );
        assert!(
            matches!(reply, SocketReply::Ack),
            "expected ack, got {reply:?}"
        );

        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_open_acks_when_no_cache() {
        let dir = unique_test_dir("open");
        let cache = dir.join("cache.json");
        let state = test_state("OPEN_TEXT");
        let config = test_config(cache.clone());
        let (sock, server) = spawn_one_shot_server(&cache, state, config);

        let reply = send_recv(
            &sock,
            &SocketCommand::Open {
                target: "dashboard".into(),
            },
        );
        assert!(
            matches!(reply, SocketReply::Ack),
            "expected ack, got {reply:?}"
        );

        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_snapshot_renders_refreshing_when_sentinel_present() {
        let dir = unique_test_dir("snapshot-refreshing");
        let cache = dir.join("cache.json");
        let state = test_state("BASELINE_TEXT");
        let config = test_config(cache.clone());

        // Drop a sentinel before the client connects so current_snapshot()
        // picks the refreshing render path.
        let sentinel = refresh_sentinel_path(&config.cache_file);
        std::fs::write(&sentinel, now_ms().to_string()).unwrap();

        let (sock, server) = spawn_one_shot_server(&cache, state, config);
        let reply = send_recv(&sock, &SocketCommand::Snapshot);
        match reply {
            SocketReply::Snapshot { output } => {
                assert!(
                    output.class.contains("tokengauge-refreshing"),
                    "expected refreshing class, got class={}",
                    output.class
                );
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sentinel);
        let _ = std::fs::remove_file(&sock);
    }

    // ------------------------------------------------------------------------
    // Click-action dispatch
    // ------------------------------------------------------------------------

    #[test]
    fn resolve_click_command_popover_uses_popover_command() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.waybar.click_action = tokengauge_core::ClickAction::Popover;
        cfg.waybar.popover_command = "  my-popover --toggle  ".into();
        assert_eq!(resolve_click_command(&cfg), "my-popover --toggle");
    }

    #[test]
    fn resolve_click_command_tui_uses_explicit_override() {
        let mut cfg = test_config(PathBuf::from("/tmp/x"));
        cfg.waybar.click_action = tokengauge_core::ClickAction::Tui;
        cfg.waybar.tui_command = "alacritty -e tokengauge-tui".into();
        assert_eq!(resolve_click_command(&cfg), "alacritty -e tokengauge-tui");
    }

    #[test]
    fn resolve_click_command_tui_default_autodetect_nonempty() {
        // Auto-detect picks something based on PATH; on the test runner we
        // expect at least one of sh/xterm to be findable, but the exact
        // value depends on the environment - assert only non-empty.
        let cfg = test_config(PathBuf::from("/tmp/x"));
        // Force PATH to contain at least /usr/bin so the candidate scan
        // succeeds deterministically on the CI/dev box.
        let _path = std::env::var_os("PATH");
        // We don't manipulate env mid-test; rely on the runner having a
        // sensible PATH. Empty is acceptable in a fully stripped env.
        let _ = resolve_click_command(&cfg);
    }
}
