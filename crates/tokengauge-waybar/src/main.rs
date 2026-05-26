use std::collections::HashMap;
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
use tokengauge_core::{
    CostInfo, DIM_HEX, ExtraWindowRow, FetchResult, ProviderFetchError, ProviderPayload,
    ProviderRow, RED_HEX, SEPARATOR_HEX, TokenGaugeConfig, WaybarState, WaybarWindow, YELLOW_HEX,
    color_hex_for_percent, ensure_cache_dir, fetch_all_providers, format_tokens,
    format_updated_relative, load_config, notify_state_path, payload_to_rows_with_costs,
    provider_icon, read_cache_full, read_notify_state, read_waybar_state, thresholds_to_fire,
    waybar_state_path, window_labels, write_cache_full, write_default_config, write_notify_state,
    write_waybar_state,
};

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
    /// Connect to the daemon socket, subscribe, and stream JSON updates to
    /// stdout (one line per change). Designed for waybar's `exec` with no
    /// `interval` set.
    #[arg(long)]
    client_tail: bool,
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
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    match value {
        Some(percent) => {
            let bar_inner = bar_blocks(percent);
            let color = color_hex_for_percent(percent);
            format!(
                "{icon} {escaped_label} [<span foreground=\"{color}\">{bar_inner}</span>] <span foreground=\"{color}\">{percent}%</span>"
            )
        }
        None => format!(
            "{icon} {escaped_label} [<span foreground=\"{DIM_HEX}\">─────</span>] <span foreground=\"{DIM_HEX}\">—</span>"
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

    let config = load_config(Some(config_path))?;
    ensure_cache_dir(&config.cache_file)?;

    if args.internal_refresh_worker {
        worker_do_refresh(&config);
        return Ok(());
    }

    if args.daemon {
        return run_daemon(&config);
    }

    if args.client_tail {
        return run_client_tail(&config);
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
        let cmd = SocketCommand::Open {
            target: match target {
                OpenTarget::Dashboard => "dashboard".into(),
                OpenTarget::Status => "status".into(),
            },
        };
        if try_send_command(&config, &cmd).is_ok() {
            return Ok(());
        }
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
        let tooltip = format_tooltip_with_errors(&tooltip_refs, &errors, true);
        let text = if rows.is_empty() && errors.is_empty() {
            format!("   <span foreground=\"{YELLOW_HEX}\">⟳ Refreshing...</span>")
        } else {
            format!(
                "   <span foreground=\"{YELLOW_HEX}\">⟳</span> {}",
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

    let text = format!("   {}", build_text_for_rows_with_errors(&rows, &errors, &config));
    let selected = selected_provider_for_tooltip(&config, &rows);
    let tooltip_rows: Vec<&ProviderRow> = match selected {
        Some(idx) => vec![&rows[idx]],
        None => rows.iter().collect(),
    };
    let tooltip = format_tooltip_with_errors(&tooltip_rows, &errors, false);

    let class = if errors.is_empty() {
        "tokengauge".to_string()
    } else if rows.is_empty() {
        "tokengauge tokengauge-error".to_string()
    } else {
        "tokengauge tokengauge-partial-error".to_string()
    };

    let output = WaybarOutput {
        text,
        tooltip,
        class,
    };

    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn selected_provider_for_tooltip(config: &TokenGaugeConfig, rows: &[ProviderRow]) -> Option<usize> {
    let state = read_waybar_state(&waybar_state_path(&config.cache_file));
    let key = state
        .selected
        .as_deref()
        .or(config.waybar.primary.as_deref())?
        .to_lowercase();
    rows.iter()
        .position(|r| r.provider.to_lowercase() == key)
}

fn build_text_for_rows_with_errors(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    config: &TokenGaugeConfig,
) -> String {
    let state = read_waybar_state(&waybar_state_path(&config.cache_file));
    let selected_key = state
        .selected
        .as_deref()
        .or(config.waybar.primary.as_deref())
        .map(|s| s.to_lowercase());
    let show_all = selected_key.is_none();

    let mut parts: Vec<String> = Vec::new();
    for row in rows {
        let pick = match &selected_key {
            None => true,
            Some(k) => &row.provider.to_lowercase() == k,
        };
        if !pick && !show_all {
            continue;
        }
        let used = match config.waybar.window {
            WaybarWindow::Daily => row.session_used,
            WaybarWindow::Weekly => row.weekly_used,
        };
        parts.push(format_bar(&row.provider, used));
        if !show_all && pick {
            return parts.join("   ");
        }
    }
    for err in errors {
        let pick = match &selected_key {
            None => true,
            Some(k) => &err.provider.to_lowercase() == k,
        };
        if !pick && !show_all {
            continue;
        }
        parts.push(format_bar_error(&err.provider));
        if !show_all && pick {
            break;
        }
    }
    if parts.is_empty() {
        // Selected provider exists in neither success nor error sets - fall back to first row
        if let Some(row) = rows.first() {
            let used = match config.waybar.window {
                WaybarWindow::Daily => row.session_used,
                WaybarWindow::Weekly => row.weekly_used,
            };
            parts.push(format_bar(&row.provider, used));
        } else if let Some(err) = errors.first() {
            parts.push(format_bar_error(&err.provider));
        }
    }
    parts.join("   ")
}

fn format_bar_error(label: &str) -> String {
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    format!(
        "{icon} {escaped_label} <span foreground=\"{RED_HEX}\">⚠</span>"
    )
}


fn refresh_sentinel_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-refreshing")
}

const REFRESH_SENTINEL_TTL_MS: i64 = 30_000;

fn refresh_in_progress(sentinel: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(sentinel) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_millis() < REFRESH_SENTINEL_TTL_MS as u128
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
        let (s_label, w_label, t_label) =
            tokengauge_core::window_labels(&row.provider);
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
        .arg(format!("--hint=int:transient:{}", if threshold < 90 { 1 } else { 0 }))
        .arg(&title)
        .arg(&body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn signal_waybar() {
    let _ = Command::new("pkill")
        .arg("-RTMIN+8")
        .arg("waybar")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
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

    let mut failed = 0;
    let mut record = |c: DoctorCheck| {
        if !c.ok {
            failed += 1;
        }
        print_check(&c);
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

    println!();
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
        self.subscribers.retain_mut(|s| s.write_all(line.as_bytes()).is_ok());
    }
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
    let text_inner = build_text_for_rows_with_errors(rows, errors, config);
    let text = if refreshing {
        if rows.is_empty() && errors.is_empty() {
            format!("   <span foreground=\"{YELLOW_HEX}\">⟳ Refreshing...</span>")
        } else {
            format!("   <span foreground=\"{YELLOW_HEX}\">⟳</span> {text_inner}")
        }
    } else {
        format!("   {text_inner}")
    };
    let selected = selected_provider_for_tooltip(config, rows);
    let tooltip_rows: Vec<&ProviderRow> = match selected {
        Some(idx) => vec![&rows[idx]],
        None => rows.iter().collect(),
    };
    let tooltip = format_tooltip_with_errors(&tooltip_rows, errors, refreshing);
    let class = if refreshing {
        "tokengauge tokengauge-refreshing".to_string()
    } else if errors.is_empty() {
        "tokengauge".to_string()
    } else if rows.is_empty() {
        "tokengauge tokengauge-error".to_string()
    } else {
        "tokengauge tokengauge-partial-error".to_string()
    };
    WaybarOutput {
        text,
        tooltip,
        class,
    }
}

fn run_daemon(config: &TokenGaugeConfig) -> Result<()> {
    let sock_path = socket_path(&config.cache_file);
    let _ = std::fs::remove_file(&sock_path);
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind socket {}", sock_path.display()))?;

    let state = Arc::new(Mutex::new(DaemonState {
        output: WaybarOutput {
            text: "   <span foreground=\"#f9e2af\">⟳ Starting...</span>".into(),
            tooltip: "<tt>TokenGauge daemon starting...</tt>".into(),
            class: "tokengauge tokengauge-refreshing".into(),
        },
        subscribers: Vec::new(),
    }));

    // Initial fetch + periodic refresh loop
    {
        let state = Arc::clone(&state);
        let config = config.clone();
        thread::spawn(move || daemon_fetch_loop(state, config));
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
        let config = config.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(200));
                if signal_flag.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    do_fetch_and_broadcast(&state, &config);
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
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(200));
            if term.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = std::fs::remove_file(&sock_path_clone);
                std::process::exit(0);
            }
        });
    }

    // Accept connections
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let config = config.clone();
                thread::spawn(move || {
                    let _ = handle_client(stream, state, config);
                });
            }
            Err(e) => {
                eprintln!("accept failed: {e}");
            }
        }
    }
    let _ = std::fs::remove_file(&sock_path);
    Ok(())
}

fn daemon_fetch_loop(state: Arc<Mutex<DaemonState>>, config: TokenGaugeConfig) {
    loop {
        do_fetch_and_broadcast(&state, &config);
        thread::sleep(Duration::from_secs(config.refresh_secs.max(10)));
    }
}

fn do_fetch_and_broadcast(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) {
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
    let _ = write_cache_full(&config.cache_file, &payloads, &errors, &costs);
    check_and_notify(config, &payloads, &costs);
    let rows = payload_to_rows_with_costs(payloads, &costs);
    let output = render_output(config, &rows, &errors, false);
    let mut s = state.lock().unwrap();
    s.output = output;
    s.broadcast();
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
            let output = state.lock().unwrap().output.clone();
            let reply = SocketReply::Snapshot { output };
            writeln!(stream, "{}", serde_json::to_string(&reply)?)?;
            stream.flush()?;
        }
        SocketCommand::Subscribe => {
            // Send current state, register as subscriber, keep stream alive
            let output = state.lock().unwrap().output.clone();
            let reply = SocketReply::Update { output };
            writeln!(stream, "{}", serde_json::to_string(&reply)?)?;
            stream.flush()?;
            state.lock().unwrap().subscribers.push(stream);
            // Don't close - daemon broadcast will push updates
        }
        SocketCommand::Refresh => {
            do_fetch_and_broadcast(&state, &config);
            writeln!(stream, "{}", serde_json::to_string(&SocketReply::Ack)?)?;
            stream.flush()?;
        }
        SocketCommand::Rotate { direction } => {
            let dir = match direction.as_str() {
                "prev" => RotateDir::Prev,
                _ => RotateDir::Next,
            };
            let _ = handle_rotate(&config, dir);
            // Re-render from current cache + rotation
            let cached = read_cache_full(&config.cache_file).ok();
            let (rows, errors) = match cached {
                Some(c) => (
                    payload_to_rows_with_costs(c.payloads().to_vec(), &c.costs()),
                    c.errors().to_vec(),
                ),
                None => (Vec::new(), Vec::new()),
            };
            let output = render_output(&config, &rows, &errors, false);
            let mut s = state.lock().unwrap();
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
    writeln!(writer, "{}", serde_json::to_string(&SocketCommand::Subscribe)?)?;
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

fn handle_rotate(config: &TokenGaugeConfig, dir: RotateDir) -> Result<()> {
    let cached = match read_cache_full(&config.cache_file) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let rows = payload_to_rows_with_costs(cached.payloads().to_vec(), &cached.costs());
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
            rows.iter()
                .position(|r| r.provider.to_lowercase() == lower)
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

fn maybe_refresh(
    config: &TokenGaugeConfig,
) -> Result<(
    Vec<ProviderPayload>,
    Vec<ProviderFetchError>,
    HashMap<String, CostInfo>,
)> {
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
        let cached = read_cache_full(&config.cache_file)?;
        let costs = cached.costs();
        Ok((
            cached.payloads().to_vec(),
            cached.errors().to_vec(),
            costs,
        ))
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
    match used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = color_hex_for_percent(pct);
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
                "  {label:<16}  [<span foreground=\"{DIM_HEX}\">──────────</span>]          no data"
            )
        }
    }
}

fn format_credits_line(credits: &str) -> Option<String> {
    if credits == "—" || credits.is_empty() {
        return None;
    }
    Some(format!(
        "  Credits  <span foreground=\"{DIM_HEX}\">${}</span>",
        pango_escape(credits)
    ))
}

fn format_extra_window(extra: &ExtraWindowRow) -> String {
    let title = pango_escape(&extra.title);
    let title_padded = format!("{title:<14}");
    match extra.used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = color_hex_for_percent(pct);
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
            "  {title_padded}  <span foreground=\"{DIM_HEX}\">[──────────]</span>          no data"
        ),
    }
}

fn format_cost_lines(cost: &CostInfo) -> Vec<String> {
    let today_usd = format!("${:.2}", cost.today_usd);
    let monthly_usd = format!("${:.2}", cost.monthly_usd);
    let usd_width = today_usd.chars().count().max(monthly_usd.chars().count());
    let today_tokens = format_tokens(cost.today_tokens);
    let monthly_tokens = format_tokens(cost.monthly_tokens);
    let tokens_width = today_tokens
        .chars()
        .count()
        .max(monthly_tokens.chars().count());
    let mut lines = Vec::new();
    if let Some(br) = &cost.burn_rate {
        lines.push(format!(
            "  Rate      <span foreground=\"{DIM_HEX}\">${:.2}/hr</span>",
            br.cost_per_hour
        ));
    }
    lines.push(format!(
        "  Today     <span foreground=\"{DIM_HEX}\">{today_usd:>usd_width$}  ·  {today_tokens:>tokens_width$} tokens</span>"
    ));
    lines.push(format!(
        "  Month     <span foreground=\"{DIM_HEX}\">{monthly_usd:>usd_width$}  ·  {monthly_tokens:>tokens_width$} tokens</span>"
    ));
    lines
}

fn format_header(row: &ProviderRow) -> String {
    let icon = icon_markup(&row.provider);
    let name = pango_escape(&row.provider);
    let plan = row.plan_label.as_deref().filter(|s| !s.is_empty());
    let badge = match plan {
        Some(p) => format!(
            "  <span foreground=\"{DIM_HEX}\">·  {}</span>",
            pango_escape(p)
        ),
        None => String::new(),
    };
    format!("<b>{icon}  {name}</b>{badge}")
}

fn format_provider_card(row: &ProviderRow) -> String {
    let mut lines = vec![format_header(row)];

    if let Some(iso) = row.updated_iso.as_deref()
        && let Some(rel) = format_updated_relative(iso)
    {
        lines.push(format!(
            "  <span foreground=\"{DIM_HEX}\">Updated {}</span>",
            pango_escape(&rel)
        ));
    }

    let (session_label, weekly_label, tertiary_label) = window_labels(&row.provider);
    lines.push(format_provider_line(
        session_label,
        row.session_used,
        &row.session_reset,
    ));
    lines.push(format_provider_line(
        weekly_label,
        row.weekly_used,
        &row.weekly_reset,
    ));
    if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
        lines.push(format_provider_line(
            tertiary_label,
            row.tertiary_used,
            &row.tertiary_reset,
        ));
    }

    if !row.extra_windows.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "  <span foreground=\"{DIM_HEX}\">Extra usage</span>"
        ));
        for extra in &row.extra_windows {
            lines.push(format_extra_window(extra));
        }
    }

    if let Some(cost) = &row.cost {
        lines.push(String::new());
        lines.push(format!("  <span foreground=\"{DIM_HEX}\">Cost</span>"));
        lines.extend(format_cost_lines(cost));
    }

    if let Some(credits) = format_credits_line(&row.credits) {
        lines.push(credits);
    }

    format!("<tt>{}</tt>", lines.join("\n"))
}

fn format_error_card(err: &ProviderFetchError) -> String {
    let icon = icon_markup(&err.provider);
    let name = pango_escape(&err.provider);
    let msg = pango_escape(&err.message);
    format!(
        "<tt><b>{icon}  {name}</b>  <span foreground=\"{RED_HEX}\">⚠ {msg}</span></tt>"
    )
}

fn format_tooltip_with_errors(
    rows: &[&ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
) -> String {
    let mut cards: Vec<String> = rows.iter().map(|row| format_provider_card(row)).collect();
    for err in errors {
        cards.push(format_error_card(err));
    }
    let cards_refs: Vec<&str> = cards.iter().map(|s| s.as_str()).collect();
    format_tooltip_from_cards(&cards_refs, refreshing)
}

fn format_tooltip_from_cards(cards: &[&str], refreshing: bool) -> String {
    let separator = format!(
        "<tt><span foreground=\"{SEPARATOR_HEX}\">────────────────────────────────────</span></tt>"
    );
    let body = cards.join(&format!("\n{separator}\n"));
    let status_line = if refreshing {
        format!(
            "\n<tt><b><span foreground=\"{YELLOW_HEX}\">⟳ Refreshing...</span></b></tt>"
        )
    } else {
        String::new()
    };
    let pairs: &[(&str, &str)] = &[
        ("left", "open TUI"),
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
        "\n\n<tt><span foreground=\"{DIM_HEX}\">{}</span></tt>",
        hint_lines.join("\n")
    );
    format!("{body}{status_line}{hint}")
}

#[cfg(test)]
fn format_tooltip_cards(rows: &[&ProviderRow], refreshing: bool) -> String {
    format_tooltip_with_errors(rows, &[], refreshing)
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
        let rows = vec![sample_row("Claude"), sample_row("Codex")];
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
}
