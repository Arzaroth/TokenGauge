//! Reading, refetching and writing the snapshot, and the JSON every non-Rust
//! frontend renders from.
//!
//! One rule holds this together: `cache_is_stale` decides fetch-or-serve, and
//! `fetch_and_write` is the only thing that refetches. Four paths used to fetch
//! and write on their own terms, and two of them dropped the cost history the
//! snapshot is the sole record of.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use tokengauge_core::{
    CostInfo, FetchResult, ProviderFetchError, ProviderPayload, ProviderRow, TokenGaugeConfig,
    Tone, UsagePace, WaybarWindow, cache_is_stale, fetch_all_providers, notify_state_path,
    payload_to_rows_with_costs, provider_icon, provider_icon_svg_path, provider_label,
    read_cache_full, read_notify_state, refresh_sentinel_deadline_ms, refresh_sentinel_path,
    retain_enabled, theme, thresholds_to_fire, window_labels, write_cache_full, write_notify_state,
};

use crate::*;

/// Emit the full snapshot as one JSON object for non-waybar frontends (KDE
/// Plasma applet, etc.). Each row is enriched with the display label, brand SVG
/// path, glyph, and brand colour so the QML frontend needs no provider
/// knowledge.
///
/// The daemon answers if it is up, exactly as `emit_bar` does, and for a
/// sharper reason than sparing a duplicate fetch: the fallback below refetches
/// when the snapshot is stale, and this process is a child of whatever spawned
/// the frontend. The daemon's environment is the systemd unit's - which is
/// where `environment.d` puts sync credentials - while a panel's is the
/// compositor's, and the fetch that ran there wrote its missing-credential
/// error into the snapshot every other frontend reads.
///
/// The fallback stays because a standalone plasmoid on a machine with no
/// daemon still has to get numbers, and there the environment is the only one
/// there is.
pub(crate) fn emit_json(config: &TokenGaugeConfig) -> Result<()> {
    if let Ok(snapshot) = try_get_json(config) {
        println!("{snapshot}");
        return Ok(());
    }

    let (payloads, errors, costs) = maybe_refresh(config)?;
    let rows = payload_to_rows_with_costs(payloads, &costs);
    println!(
        "{}",
        serde_json::to_string(&json_snapshot(config, &rows, &errors))?
    );
    Ok(())
}

/// The snapshot object itself. Split out from the printing so the key set five
/// frontends parse can be pinned by a test - none of them are compiled here,
/// and a renamed field reaches them as a blank panel.
pub(crate) fn json_snapshot(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
) -> serde_json::Value {
    let enabled: Vec<String> = config
        .providers
        .enabled_providers()
        .into_iter()
        .map(|p| p.to_string())
        .collect();

    // History is resolved here, on every render, rather than stored in the
    // snapshot. Its steps are relative to today, so a panel that rebuilt them
    // only when it refetched would draw yesterday's calendar for the same
    // reason a reset countdown stopped counting down. Reading the store and the
    // cached price table is local work; `allow_network: false` is what keeps a
    // render from ever turning into a fetch.
    let (store, store_error) = tokengauge_core::sync::store::load(&config.cache_file);
    let prices = tokengauge_core::cost::pricing::load(
        &config.cache_file,
        std::time::Duration::from_secs(config.ccusage_timeout_secs),
        false,
    );
    let archive = tokengauge_core::cost::pricing::archive();
    let now = chrono::Local::now();
    let (today, offset) = (now.date_naive(), *now.offset());
    let now_ms = now.timestamp_millis();

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
                // What the refresh control says on hover. Resolved on every
                // render like the reset countdowns, for the same reason: the
                // age it states keeps moving after the fetch that produced it.
                map.insert(
                    "refresh_hint".into(),
                    tokengauge_core::refresh_hint(r.updated_iso.as_deref(), now_ms).into(),
                );
                map.insert("color".into(), icon.color_hex.into());
                let pace_badge = |pace: Option<UsagePace>| {
                    pace.map(|p| serde_json::Value::from(p.badge()))
                        .unwrap_or(serde_json::Value::Null)
                };
                map.insert("session_pace".into(), pace_badge(r.session_pace));
                map.insert("weekly_pace".into(), pace_badge(r.weekly_pace));
                // The panel layout, resolved once in the core. Every frontend
                // that draws a panel walks this list rather than deciding its
                // own section order, labels and number formatting.
                map.insert(
                    "panel".into(),
                    serde_json::to_value(tokengauge_core::panel_spec(r)).unwrap_or_default(),
                );
                // The second screen: every range resolved, so switching one is
                // a click in an open pane rather than another `--json`.
                let mut history = tokengauge_core::history_panel(
                    &store,
                    &r.provider,
                    today,
                    offset,
                    &prices,
                    archive,
                );
                if let Some(error) = &store_error {
                    history.notes.push(error.clone());
                }
                map.insert(
                    "history".into(),
                    serde_json::to_value(&history).unwrap_or_default(),
                );
                // Extra windows get the same badge-string treatment, so a
                // frontend renders every gauge's projection the same way.
                if let Some(serde_json::Value::Array(extras)) = map.get_mut("extra_windows") {
                    for (value, extra) in extras.iter_mut().zip(r.extra_windows.iter()) {
                        if let serde_json::Value::Object(entry) = value {
                            entry.insert("pace".into(), pace_badge(extra.pace));
                        }
                    }
                }
                // Frontends that keep their own provider selection cannot use
                // `--open`, which resolves the provider from the config rather
                // than from the caller. Carrying the URLs on the row lets them
                // open exactly the provider they are displaying.
                // The headline number and its tier, resolved here. Three
                // frontends each picked the window themselves and each carried
                // their own copy of the 50/80 boundaries to tint it with.
                let percent = window_percent(r, &config.waybar.window);
                map.insert(
                    "bar".into(),
                    serde_json::json!({
                        "percent": percent,
                        "tone": percent.map(Tone::for_percent).unwrap_or(Tone::Dim),
                    }),
                );
                let urls = tokengauge_core::provider_urls(&r.provider);
                map.insert(
                    "dashboard_url".into(),
                    urls.dashboard
                        .map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );
                map.insert(
                    "status_url".into(),
                    urls.status
                        .map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );
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
    serde_json::json!({
        // Frontends show this in their settings pane; `update` only carries a
        // version once a release check has run, and is null until then.
        "version": env!("CARGO_PKG_VERSION"),
        "rows": row_values,
        "errors": errors,
        "enabled": enabled,
        "providers": tokengauge_core::PROVIDERS,
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
        // Frontends watch this file and re-read the snapshot when it changes,
        // so a fetch by the daemon or by another frontend lands immediately
        // instead of on the next poll. It holds no provider data.
        "revision_file": tokengauge_core::revision_path(&config.cache_file),
    })
}

/// Fetch in this process and wait for it, with the sentinel raised so every
/// bar renders the refreshing state meanwhile. Unlike `--refresh` this does not
/// fork: the caller is a frontend that chains `--json` behind it and needs the
/// snapshot to be current by the time this returns.
pub(crate) fn refresh_inline(config: &TokenGaugeConfig) {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    let _ = std::fs::write(&sentinel, refresh_sentinel_deadline_ms(config).to_string());
    signal_waybar();
    let FetchResult {
        payloads, costs, ..
    } = fetch_and_write(config, false);
    let _ = std::fs::remove_file(&sentinel);
    check_and_notify(config, &payloads, &costs);
    signal_waybar();
}

/// Block until the snapshot is rewritten, or the timeout runs out. Polls the
/// revision file rather than the snapshot: it is a few bytes, and it is written
/// last, so a change to it means the snapshot beside it is already complete.
pub(crate) fn wait_for_change(config: &TokenGaugeConfig, timeout_secs: u64) {
    const POLL: Duration = Duration::from_secs(1);
    let initial = tokengauge_core::read_revision(&config.cache_file);
    let deadline = timeout_secs.clamp(5, 3600);
    for _ in 0..deadline {
        thread::sleep(POLL);
        if tokengauge_core::read_revision(&config.cache_file) != initial {
            return;
        }
    }
}

pub(crate) fn check_and_notify(
    config: &TokenGaugeConfig,
    payloads: &[ProviderPayload],
    costs: &HashMap<String, CostInfo>,
) {
    if !config.notifications.enabled || config.notifications.thresholds.is_empty() {
        return;
    }
    let _ = costs; // costs no longer needed here; kept for a stable signature.

    let path = notify_state_path(&config.cache_file);
    let mut state = read_notify_state(&path);
    let thresholds = &config.notifications.thresholds;

    for payload in payloads {
        if payload.has_error() {
            continue;
        }
        let Some(usage) = &payload.usage else {
            continue;
        };
        let name = provider_label(&payload.provider);
        let (s_label, w_label, t_label) = tokengauge_core::window_labels(&payload.provider);
        // Notify off the raw windows so we can key roll-over on the actual
        // reset timestamp instead of the formatted "in 2h" countdown string.
        let windows = [
            ("session", usage.primary.as_ref(), s_label),
            ("weekly", usage.secondary.as_ref(), w_label),
            ("tertiary", usage.tertiary.as_ref(), t_label),
        ];
        for (slot, window, label) in windows {
            let Some(window) = window else { continue };
            let Some(pct) = window.used_percent.map(|pct| pct.min(100)) else {
                continue;
            };
            let source = payload.source.as_deref().unwrap_or_default();
            let key = format!("{}:{}:{}", payload.provider.to_lowercase(), source, slot);
            let entry = state.entries.entry(key).or_default();
            let resets_at = window.resets_at.as_deref();
            let (to_fire, new_notified) = thresholds_to_fire(
                pct,
                resets_at,
                entry.resets_at.as_deref(),
                thresholds,
                &entry.notified,
            );
            entry.notified = new_notified;
            entry.resets_at = window.resets_at.clone();
            if !to_fire.is_empty() {
                let (_, _, reset_str) = tokengauge_core::format_window(Some(window.clone()));
                for threshold in to_fire {
                    fire_notification(name, label, pct, threshold, &reset_str);
                }
            }
        }
    }

    let _ = write_notify_state(&path, &state);
}

pub(crate) fn fire_notification(provider: &str, window: &str, pct: u8, threshold: u8, reset: &str) {
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
pub(crate) fn signal_waybar() {
    const SIGRTMIN_PLUS_8: libc::c_int = 42;
    let pids = find_waybar_pids();
    for pid in pids {
        // SAFETY: kill(2) is a syscall; passing a stale PID is a no-op or
        // would target a recycled pid (acceptable - we no-op on EPERM/ESRCH).
        let _ = unsafe { libc::kill(pid, SIGRTMIN_PLUS_8) };
    }
}

pub(crate) fn find_waybar_pids() -> Vec<libc::pid_t> {
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
pub(crate) fn handle_refresh_quick(config: &TokenGaugeConfig) {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    let _ = std::fs::write(&sentinel, refresh_sentinel_deadline_ms(config).to_string());
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
pub(crate) fn worker_do_refresh(config: &TokenGaugeConfig) {
    let sentinel = refresh_sentinel_path(&config.cache_file);
    let FetchResult {
        payloads, costs, ..
    } = fetch_and_write(config, true);
    let _ = std::fs::remove_file(&sentinel);
    check_and_notify(config, &payloads, &costs);
    signal_waybar();
}

pub(crate) type RefreshSnapshot = (
    Vec<ProviderPayload>,
    Vec<ProviderFetchError>,
    HashMap<String, CostInfo>,
);

/// Read the snapshot and scope it to the enabled providers. Both steps or
/// neither: the snapshot was written by whatever set was enabled at fetch time,
/// so a caller that reads it raw still sees a provider disabled since then.
pub(crate) fn cached_parts(config: &TokenGaugeConfig) -> Result<RefreshSnapshot> {
    let (mut payloads, mut errors, costs) = read_cache_full(&config.cache_file)?.into_parts();
    retain_enabled(&mut payloads, &mut errors, &config.providers);
    Ok((payloads, errors, costs))
}

/// Enabled-provider rows, empty when there is no readable snapshot. Every path
/// that renders or resolves a provider from cache goes through here.
pub(crate) fn rows_from_cache(
    config: &TokenGaugeConfig,
) -> (Vec<ProviderRow>, Vec<ProviderFetchError>) {
    match cached_parts(config) {
        Ok((payloads, errors, costs)) => (payload_to_rows_with_costs(payloads, &costs), errors),
        Err(_) => (Vec::new(), Vec::new()),
    }
}

/// One fetch, one snapshot write, for every path that refetches. The cost
/// fallback below has to be here rather than at each caller: it existed at two
/// of the four sites, and the two without it discarded cost history on any
/// transient reader failure.
///
/// `wipe_snapshot` is `--refresh`'s deliberate cold start. Prior costs are read
/// before it fires, because the snapshot holds the only record of past days.
pub(crate) fn fetch_and_write(config: &TokenGaugeConfig, wipe_snapshot: bool) -> FetchResult {
    let prior_costs = read_cache_full(&config.cache_file)
        .map(|c| c.costs())
        .unwrap_or_default();
    if wipe_snapshot {
        let _ = std::fs::remove_file(&config.cache_file);
    }
    let mut result = fetch_all_providers(config);
    // An empty cost map means the readers found nothing at all, which is a
    // failure and not a history that ended. Keeping the prior figures shows
    // stale money; dropping them loses every past day permanently.
    if result.costs.is_empty() && !prior_costs.is_empty() {
        result.costs = prior_costs;
    }
    if let Err(e) = write_cache_full(
        &config.cache_file,
        &result.payloads,
        &result.errors,
        &result.costs,
        &config.providers,
        Some(&result.sync),
    ) {
        dlog("cache", &format!("write failed: {e}"));
    }
    result
}

pub(crate) fn maybe_refresh(config: &TokenGaugeConfig) -> Result<RefreshSnapshot> {
    if cache_is_stale(config) {
        let FetchResult {
            payloads,
            errors,
            costs,
            ..
        } = fetch_and_write(config, false);
        check_and_notify(config, &payloads, &costs);
        Ok((payloads, errors, costs))
    } else {
        cached_parts(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::tests::sample_row;

    #[test]
    fn the_json_snapshot_keeps_the_keys_every_frontend_reads() {
        let config = TokenGaugeConfig::default();
        let row = sample_row("claude");
        let snapshot = json_snapshot(&config, std::slice::from_ref(&row), &[]);

        let top: Vec<&str> = snapshot
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in [
            "version",
            "rows",
            "errors",
            "enabled",
            "providers",
            "primary",
            "window",
            "theme",
            "update",
            "revision_file",
        ] {
            assert!(top.contains(&key), "top-level `{key}` is gone: {top:?}");
        }

        let theme: Vec<&str> = snapshot["theme"]
            .as_object()
            .expect("a theme object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in ["dim", "separator", "green", "yellow", "red", "neutral"] {
            assert!(theme.contains(&key), "theme.{key} is gone: {theme:?}");
        }

        let row_value = &snapshot["rows"][0];
        let keys: Vec<&str> = row_value
            .as_object()
            .expect("a row object")
            .keys()
            .map(String::as_str)
            .collect();
        for key in [
            // Serialised straight off ProviderRow.
            "provider",
            "session_used",
            "session_reset",
            "weekly_used",
            "weekly_reset",
            "credits",
            "source",
            "updated",
            "plan_label",
            "extra_windows",
            "stale",
            // Added here, and invisible to ProviderRow's own derive.
            "label",
            "glyph",
            "color",
            "refresh_hint",
            "icon_svg",
            "window_labels",
            "session_pace",
            "weekly_pace",
            "panel",
            "history",
            "bar",
            "dashboard_url",
            "status_url",
        ] {
            assert!(keys.contains(&key), "row key `{key}` is gone: {keys:?}");
        }

        // Every range is carried, so a frontend switches one without another
        // `--json`. A missing id is a selector with a dead button on it.
        let ranges: Vec<&str> = snapshot["rows"][0]["history"]["series"]
            .as_array()
            .map(|series| {
                series
                    .iter()
                    .filter_map(|s| s["id"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            ranges,
            tokengauge_core::HISTORY_RANGES
                .iter()
                .map(|r| r.id())
                .collect::<Vec<_>>(),
            "the history pane's ranges are not all on the row"
        );

        // The panel is the whole content contract; a row that carries an empty
        // one draws nothing anywhere.
        assert!(
            snapshot["rows"][0]["panel"]
                .as_array()
                .is_some_and(|sections| !sections.is_empty()),
            "the row carries no panel sections"
        );
    }
}
