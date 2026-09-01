//! The long-lived process and the socket it serves.
//!
//! A daemon is optional: every command here has a no-daemon fallback, because a
//! user who never installed the unit still has to get a bar. What the daemon
//! buys is one fetch shared by every frontend, and a push when the numbers
//! change instead of each surface polling on its own clock.
//!
//! Both loops contain their own panics. The refresh sentinel raised before a
//! fetch has to come down whatever the fetch does, or every client renders the
//! spinner forever, and an escaping panic would take the thread with it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use tokengauge_core::update;
use tokengauge_core::{
    FetchResult, TokenGaugeConfig, cache_is_stale, load_config, payload_to_rows_with_costs,
    refresh_in_progress, refresh_sentinel_deadline_ms, refresh_sentinel_path,
};

use crate::*;

/// Restart the systemd user daemon so the freshly-installed binary is loaded.
/// Best effort: returns false when there's no active unit to restart (plain
/// polling mode) or systemctl is unavailable.
pub(crate) fn restart_daemon() -> bool {
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
pub(crate) fn notify_update_available(
    config: &TokenGaugeConfig,
    status: &tokengauge_core::UpdateStatus,
) {
    let Some(latest) = &status.latest else {
        return;
    };
    if !status.available || status.notified.as_deref() == Some(latest.as_str()) {
        return;
    }
    let title = "TokenGauge: update available";
    let body = format!(
        "v{latest} is available (you have v{}). Run tokengauge --update.",
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
pub(crate) fn daemon_update_loop(config: Arc<Mutex<TokenGaugeConfig>>) {
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

pub(crate) fn socket_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge.sock")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub(crate) enum SocketCommand {
    Snapshot,
    /// The `--json` snapshot, rendered by the daemon. A daemon that predates
    /// this variant fails to parse the line and hangs up without replying,
    /// which the caller reads as "no daemon" and falls back on.
    Json,
    Subscribe,
    Refresh,
    Rotate {
        direction: String,
    },
    Open {
        target: String,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SocketReply {
    Snapshot { output: WaybarOutput },
    Json { snapshot: serde_json::Value },
    Update { output: WaybarOutput },
    Ack,
    Error { message: String },
}

pub(crate) fn connect_socket(config: &TokenGaugeConfig) -> std::io::Result<UnixStream> {
    let path = socket_path(&config.cache_file);
    UnixStream::connect(&path)
}

pub(crate) fn try_send_command(config: &TokenGaugeConfig, cmd: &SocketCommand) -> Result<()> {
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

pub(crate) fn try_get_snapshot(config: &TokenGaugeConfig) -> Result<String> {
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

/// The daemon's answer to `--json`, for the same reason `try_get_snapshot`
/// exists: the process that renders must not be the process that fetches.
///
/// `--json` used to do its own `maybe_refresh`, so whichever desktop frontend
/// polled first after the snapshot went stale ran the fetch as a child of the
/// compositor. That child has the compositor's environment, and sync
/// credentials arriving through `environment.d` reach the systemd unit and
/// nothing else - the fetch then wrote "no S3 access key" into the snapshot
/// every frontend reads.
pub(crate) fn try_get_json(config: &TokenGaugeConfig) -> Result<String> {
    let mut stream = connect_socket(config).map_err(|e| anyhow::anyhow!(e))?;
    let line = serde_json::to_string(&SocketCommand::Json)?;
    writeln!(stream, "{line}")?;
    stream.flush()?;
    let mut reader = BufReader::new(&stream);
    let mut buf = String::new();
    if reader.read_line(&mut buf)? == 0 {
        anyhow::bail!("daemon closed the connection");
    }
    let reply: SocketReply = serde_json::from_str(buf.trim())?;
    match reply {
        SocketReply::Json { snapshot } => Ok(serde_json::to_string(&snapshot)?),
        SocketReply::Error { message } => Err(anyhow::anyhow!(message)),
        _ => Err(anyhow::anyhow!("unexpected reply from daemon")),
    }
}

/// The daemon's answer to a snapshot request - the standard waybar poll path -
/// rendered from the snapshot on disk rather than replayed from the last fetch.
///
/// Rendering again is what picks up the ⟳ while a manual refresh is in flight,
/// and it is what makes the reset countdowns in the tooltip move: a countdown
/// is measured against the clock at the moment the row is built, so replaying
/// the output of the last fetch replays the countdown it was rendered with. The
/// instant it counts down to is absolute, so a snapshot minutes old still
/// yields the right one - only the percentages have to wait for a fetch.
///
/// The stored output is the fallback for a cache that reads back as nothing:
/// a bar that has gone blank is worse than a bar a few minutes behind. Not
/// while a refresh is in flight, though - there the empty render carries the ⟳
/// that says so, and the first fetch on a cold machine has no output to replay
/// anyway.
pub(crate) fn current_snapshot(
    state: &Arc<Mutex<DaemonState>>,
    config: &TokenGaugeConfig,
) -> WaybarOutput {
    let refreshing = refresh_in_progress(&refresh_sentinel_path(&config.cache_file));
    let (rows, errors) = rows_from_cache(config);
    if !refreshing && rows.is_empty() && errors.is_empty() {
        return state
            .lock()
            .expect("daemon state mutex poisoned")
            .output
            .clone();
    }
    render_output(config, &rows, &errors, refreshing)
}

pub(crate) struct DaemonState {
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

/// Log a warning for each unrecognized config key (startup and after reloads).
pub(crate) fn warn_unknown_config_keys(config: &TokenGaugeConfig) {
    for key in config.unknown_config_keys() {
        dlog(
            "daemon",
            &format!("ignoring unrecognized config key `{key}`"),
        );
    }
}

pub(crate) fn run_daemon(config: TokenGaugeConfig, config_path: PathBuf) -> Result<()> {
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
    warn_unknown_config_keys(&config);

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
                            {
                                let mut guard = cfg.lock().expect("daemon config mutex poisoned");
                                *guard = new_cfg.clone();
                            }
                            dlog(
                                "reload",
                                &format!(
                                    "config reloaded from {} (refresh every {refresh_secs}s)",
                                    path.display()
                                ),
                            );
                            warn_unknown_config_keys(&new_cfg);
                            // Ask the cache, not the before/after config: a
                            // provider switched *off* leaves it a superset, which
                            // still answers and re-renders for free, and
                            // `--set-provider` has usually already fetched the
                            // one switched *on* before signalling us.
                            if cache_is_stale(&new_cfg) {
                                dlog(
                                    "reload",
                                    "cache cannot answer for the new config, refetching",
                                );
                                do_refresh_cycle(&state, &new_cfg);
                                continue;
                            }
                            // Otherwise re-render cached output with the new
                            // theme/config so colour changes show up before the
                            // next fetch, without paying for a fetch.
                            let (rows, errors) = rows_from_cache(&new_cfg);
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

pub(crate) fn dlog(tag: &str, msg: &str) {
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    eprintln!("[{ts}] [{tag}] {msg}");
}

/// How often the wait between fetches re-asks `cache_is_stale`.
///
/// Age is not the only way a snapshot goes stale: a window that resets
/// mid-cycle invalidates the percentages beside it at an instant no timer wakes
/// for. Sleeping `refresh_secs` in one go left the daemon blind to that, so the
/// first process to notice was a frontend polling `--json` every 30s - and it
/// then fetched in the frontend's own environment. The daemon has to be the one
/// that notices, or delegating `--json` to it just trades a wrong-environment
/// fetch for no fetch at all.
const STALE_TICK: Duration = Duration::from_secs(15);

pub(crate) fn daemon_fetch_loop(
    state: Arc<Mutex<DaemonState>>,
    config: Arc<Mutex<TokenGaugeConfig>>,
) {
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
        wait_for_next_fetch(&snapshot, STALE_TICK);
    }
}

/// Sleep out `refresh_secs`, waking early once the snapshot this fetch just
/// wrote has gone stale.
///
/// A snapshot that reads as stale immediately after a fetch is one the fetch
/// could not write - an unwritable state directory, a provider set nothing
/// covers. Waking early on that would fetch every tick forever, so the early
/// wake is armed only by a fetch that landed.
fn wait_for_next_fetch(config: &TokenGaugeConfig, tick: Duration) {
    let armed = !cache_is_stale(config);
    let full = Duration::from_secs(config.refresh_secs.max(10));
    let deadline = std::time::Instant::now() + full;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        thread::sleep(tick.min(remaining));
        if armed && cache_is_stale(config) {
            return;
        }
    }
}

pub(crate) fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

pub(crate) fn do_fetch_and_broadcast(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) {
    let started = std::time::Instant::now();
    let FetchResult {
        payloads,
        errors,
        costs,
        ..
    } = fetch_and_write(config, false);
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
pub(crate) fn raise_refresh_sentinel(config: &TokenGaugeConfig) {
    let _ = std::fs::write(
        refresh_sentinel_path(&config.cache_file),
        refresh_sentinel_deadline_ms(config).to_string(),
    );
    signal_waybar();
}

/// Full manual-refresh cycle: raise the sentinel so every frontend renders ⟳,
/// fetch, then drop it. waybar is signalled on both edges so the bar picks up
/// the indicator and the result without waiting for its poll interval.
/// Panics are contained here rather than at each caller: the sentinel raised
/// above must come down whatever the fetch does, or every client shows ⟳
/// forever, and an escaping panic would kill the caller's long-lived thread.
pub(crate) fn do_refresh_cycle(state: &Arc<Mutex<DaemonState>>, config: &TokenGaugeConfig) {
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

pub(crate) fn handle_client(
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
        SocketCommand::Json => {
            let (rows, errors) = rows_from_cache(&config);
            let snapshot = json_snapshot(&config, &rows, &errors);
            let reply = SocketReply::Json { snapshot };
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
            // that kicks a refresh and then polls for the ⟳ state (a panel
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
            let (rows, errors) = rows_from_cache(&config);
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

pub(crate) fn run_client_tail(config: &TokenGaugeConfig) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A snapshot request re-renders, so the countdowns in the tooltip are
    /// counted from now and not from whenever the last fetch happened. The
    /// stored output is a fallback, exercised by the test below it.
    #[test]
    fn socket_snapshot_renders_the_cache_rather_than_replaying_the_last_fetch() {
        use std::collections::HashMap;
        use tokengauge_core::{
            ProviderPayload, ProvidersConfig, UsageSnapshot, UsageWindow, write_cache_full,
        };

        let dir = unique_test_dir("snapshot-rerender");
        let cache = dir.join("cache.json");
        let mut config = test_config(cache.clone());
        config.providers = ProvidersConfig {
            claude: Some(true),
            ..Default::default()
        };
        let payload = ProviderPayload {
            stale_reason: None,
            provider: "claude".into(),
            version: None,
            source: None,
            usage: Some(UsageSnapshot {
                primary: Some(UsageWindow {
                    used_percent: Some(36),
                    reset_description: None,
                    resets_at: Some((chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339()),
                    window_minutes: Some(300),
                }),
                secondary: None,
                tertiary: None,
                updated_at: None,
                login_method: None,
                extra_rate_windows: Vec::new(),
            }),
            credits: None,
            error: None,
            stale: false,
        };
        write_cache_full(
            &cache,
            &[payload],
            &[],
            &HashMap::new(),
            &config.providers,
            None,
        )
        .unwrap();

        let state = test_state("BASELINE_TEXT");
        let (sock, server) = spawn_one_shot_server(&cache, state, config);
        let reply = send_recv(&sock, &SocketCommand::Snapshot);
        match reply {
            SocketReply::Snapshot { output } => {
                assert_ne!(output.text, "BASELINE_TEXT", "replayed the stored output");
                assert!(
                    output.tooltip.contains("Resets in 1h 59m"),
                    "expected a countdown measured now, got {}",
                    output.tooltip
                );
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn socket_snapshot_falls_back_to_the_last_output_when_the_cache_reads_as_nothing() {
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

    /// `--json` delegating to the daemon is only worth anything if the daemon
    /// answers with the same object the standalone path prints: every non-Rust
    /// frontend parses it.
    #[test]
    fn socket_json_renders_the_snapshot_from_the_cache() {
        use std::collections::HashMap;
        use tokengauge_core::{
            ProviderPayload, ProvidersConfig, UsageSnapshot, UsageWindow, write_cache_full,
        };

        let dir = unique_test_dir("json-socket");
        let cache = dir.join("cache.json");
        let mut config = test_config(cache.clone());
        config.providers = ProvidersConfig {
            claude: Some(true),
            ..Default::default()
        };
        let payload = ProviderPayload {
            stale_reason: None,
            provider: "claude".into(),
            version: None,
            source: None,
            usage: Some(UsageSnapshot {
                primary: Some(UsageWindow {
                    used_percent: Some(36),
                    reset_description: None,
                    resets_at: Some((chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339()),
                    window_minutes: Some(300),
                }),
                secondary: None,
                tertiary: None,
                updated_at: None,
                login_method: None,
                extra_rate_windows: Vec::new(),
            }),
            credits: None,
            error: None,
            stale: false,
        };
        write_cache_full(
            &cache,
            &[payload],
            &[],
            &HashMap::new(),
            &config.providers,
            None,
        )
        .unwrap();

        let state = test_state("BASELINE_TEXT");
        let (sock, server) = spawn_one_shot_server(&cache, state, config);
        let reply = send_recv(&sock, &SocketCommand::Json);
        match reply {
            SocketReply::Json { snapshot } => {
                for key in ["version", "rows", "errors", "enabled", "revision_file"] {
                    assert!(snapshot.get(key).is_some(), "missing {key}: {snapshot}");
                }
                let rows = snapshot["rows"].as_array().expect("rows");
                assert_eq!(rows.len(), 1, "{snapshot}");
                assert_eq!(rows[0]["provider"], "Claude");
                assert!(rows[0].get("panel").is_some(), "no panel spec on the row");
            }
            other => panic!("unexpected reply: {other:?}"),
        }
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(&sock);
    }

    /// The rollover case: the snapshot goes stale between fetches, at an
    /// instant `refresh_secs` does not line up with. The daemon has to notice
    /// it, because since `--json` is served from here no frontend will.
    #[test]
    fn the_fetch_wait_wakes_early_once_the_snapshot_goes_stale() {
        use std::collections::HashMap;
        use tokengauge_core::write_cache_full;

        let dir = unique_test_dir("fetch-wait-wake");
        let cache = dir.join("cache.json");
        let config = test_config(cache.clone());
        write_cache_full(&cache, &[], &[], &HashMap::new(), &config.providers, None).unwrap();

        let doomed = cache.clone();
        let killer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            std::fs::remove_file(&doomed).expect("remove the snapshot");
        });
        let started = std::time::Instant::now();
        wait_for_next_fetch(&config, Duration::from_millis(25));
        killer.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "waited out refresh_secs ({}s) instead of waking on staleness",
            started.elapsed().as_secs(),
        );
    }

    /// And the guard on that: a fetch that could not write leaves the snapshot
    /// stale on entry, and waking early on it would refetch every tick.
    #[test]
    fn the_fetch_wait_does_not_spin_when_the_fetch_wrote_nothing() {
        let dir = unique_test_dir("fetch-wait-spin");
        let config = test_config(dir.join("never-written.json"));

        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            wait_for_next_fetch(&config, Duration::from_millis(25));
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "woke early on a snapshot that was stale before the wait began",
        );
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
        let (sock, server) = spawn_one_shot_server(&cache, state.clone(), config);

        // The cycle the command spawns takes the sentinel back down the moment
        // its fetch returns, and with no network to wait on that beats the
        // assertion below. Holding the state lock parks the cycle where it
        // publishes its result - after the sentinel is up, before it comes
        // down - so what is asserted here is the ordering the handler promises
        // rather than whichever thread won. The ack path takes no lock.
        let parked = state.lock().unwrap();
        let reply = send_recv(&sock, &SocketCommand::Refresh);
        assert!(
            matches!(reply, SocketReply::Ack),
            "expected ack, got {reply:?}"
        );
        assert!(sentinel.exists(), "Refresh should create the sentinel file");
        drop(parked);

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
}
