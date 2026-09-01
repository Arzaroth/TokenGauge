//! Every file TokenGauge writes, and where it puts them.
//!
//! One rule: the snapshot's path is the config's business, and every other
//! state file is derived from its **parent**. The daemon socket, the refresh
//! sentinel, the selected provider, the notify state, the price table and the
//! revision file all live beside it, so pointing `cache_file` somewhere else
//! moves the whole set and a test can have a directory of its own.
//!
//! Writes go through [`write_atomic`], which names its temporary per call - the
//! daemon can write the same path from its fetch loop and its signal thread at
//! once, and a shared temporary has them overwrite each other's half-written
//! file.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::{TokenGaugeConfig, now_ms};

/// Per-user directory for the snapshot and the small state files derived from
/// its parent (selection, update status, notify state, refresh sentinel, daemon
/// socket).
///
/// `XDG_STATE_HOME` rather than a cache directory: the snapshot is the only
/// record of past days' tokens and costs, so it has to survive a reboot even
/// when it is far too old to display without a refetch.
pub fn state_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(dir) = dirs::data_local_dir() {
            return dir.join("TokenGauge");
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(dir) = std::env::var("XDG_STATE_HOME")
            && !dir.is_empty()
        {
            return PathBuf::from(dir).join("tokengauge");
        }
        if let Some(home) = dirs::home_dir() {
            return home.join(".local").join("state").join("tokengauge");
        }
    }
    std::env::temp_dir().join("tokengauge")
}

/// Default snapshot location.
pub fn default_cache_file() -> PathBuf {
    state_dir().join("tokengauge-usage.json")
}

/// Where releases before 0.21 kept the snapshot: the platform temp dir, which
/// a reboot wipes on most distributions.
pub fn legacy_cache_file() -> PathBuf {
    std::env::temp_dir().join("tokengauge-usage.json")
}

/// Files that live beside the snapshot and are worth carrying over from the
/// temp directory. The sentinel, the socket and the update check are
/// regenerated on demand, so they stay behind.
const MIGRATED_STATE_FILES: [&str; 3] = [
    "tokengauge-usage.json",
    "tokengauge-waybar-state.json",
    "tokengauge-notify-state.json",
];

/// Move a pre-0.21 snapshot out of the temp directory, so an upgrade keeps its
/// history and its selected provider instead of cold-starting. Best effort:
/// anything that fails just means one refetch.
pub fn migrate_legacy_state(cache_file: &Path) {
    migrate_state_from(&std::env::temp_dir(), cache_file);
}

fn migrate_state_from(legacy_dir: &Path, cache_file: &Path) {
    let Some(dir) = cache_file.parent() else {
        return;
    };
    if dir == legacy_dir {
        return;
    }
    for name in MIGRATED_STATE_FILES {
        let from = legacy_dir.join(name);
        let to = dir.join(name);
        if to.exists() || !from.exists() {
            continue;
        }
        if fs::create_dir_all(dir).is_err() {
            return;
        }
        // Across filesystems (a tmpfs /tmp is the common case) rename fails
        // with EXDEV, so fall back to copying.
        if fs::rename(&from, &to).is_err() && fs::copy(&from, &to).is_ok() {
            let _ = fs::remove_file(&from);
        }
    }
}

pub fn default_config_path() -> PathBuf {
    // On Windows use the native config directory (`%APPDATA%`) so the path
    // matches what scripts/install.ps1 writes; on Unix keep the XDG convention
    // (`$XDG_CONFIG_HOME` or `~/.config`).
    #[cfg(windows)]
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")));

    #[cfg(not(windows))]
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            home.push(".config");
            home
        });

    config_dir.join("tokengauge").join("config.toml")
}

/// Replace a file in one step, so a reader watching it never sees a half
/// written snapshot.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    // Per call, not per process: the daemon can write the same path from its
    // fetch loop and its signal thread at once, and a shared temporary would
    // have them overwrite each other's half-written file.
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    fs::write(&tmp, contents)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// A few bytes that change on every snapshot write, and carry no provider data.
/// Frontends watch this instead of the snapshot: it is cheap to load, and it is
/// written after the snapshot lands, so a watcher that reacts to it always
/// reads complete data.
pub fn revision_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-revision")
}

/// Create the revision file if it is not there yet, so a frontend can put a
/// watch on it before anything has been fetched. A watch that has to wait for
/// the file to appear is one that never fires.
pub fn ensure_revision(cache_file: &Path) {
    let path = revision_path(cache_file);
    if !path.exists() {
        bump_revision(cache_file);
    }
}

pub fn read_revision(cache_file: &Path) -> String {
    fs::read_to_string(revision_path(cache_file))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Public because a config change that leaves the snapshot valid still changes
/// what every frontend renders, and a frontend that only watches has no other
/// way to hear about it.
pub fn bump_revision(cache_file: &Path) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Two writes inside the same millisecond have to differ, or a reader that
    // compares contents rather than watching for events misses the second one.
    let token = format!("{}-{}-{}", now_ms(), std::process::id(), seq);
    let path = revision_path(cache_file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    // Rewritten in place rather than replaced, unlike the snapshot: a watcher
    // bound to the inode (Qt's file watcher is) stops firing when the file it
    // watches is renamed away. A torn read here costs nothing - a reader either
    // sees the old token or a different one, and either way "different" is the
    // only thing it asks.
    let _ = fs::write(&path, token.as_bytes());
}

/// Path of the sentinel file held for the duration of a manual refresh.
/// Written by whoever kicks the fetch (daemon or the `--refresh` worker) and
/// removed when it lands, so any frontend can poll it to show a ⟳ indicator.
pub fn refresh_sentinel_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-refreshing")
}

/// Fallback TTL for a sentinel whose contents predate the deadline scheme (an
/// older build wrote a start timestamp, not a deadline): treat it as abandoned
/// once this much time has passed since it was last written.
const REFRESH_SENTINEL_TTL: Duration = Duration::from_secs(30);

/// Head-room added to the configured fetch budget so a refresh that runs to its
/// worst case still counts as in-flight.
const REFRESH_SENTINEL_MARGIN_MS: u64 = 10_000;

/// Wall-clock budget a manual refresh may legitimately take under the current
/// config: per-provider timeout (the slower of the fetch and ccusage limits)
/// plus the worst-case stagger delay, plus head-room. The sentinel stores
/// `now + this` as its deadline so a slow-but-live fetch keeps the ⟳ up instead
/// of expiring at a fixed TTL shorter than the fetch it is guarding.
pub fn refresh_budget_ms(config: &TokenGaugeConfig) -> u64 {
    let enabled = config.providers.enabled_providers().len() as u64;
    let stagger = config.stagger_ms.saturating_mul(enabled.saturating_sub(1));
    let timeout = config.timeout_secs.max(config.ccusage_timeout_secs) * 1000;
    stagger + timeout + REFRESH_SENTINEL_MARGIN_MS
}

/// Absolute deadline (epoch ms) to stamp into a fresh refresh sentinel.
pub fn refresh_sentinel_deadline_ms(config: &TokenGaugeConfig) -> i64 {
    now_ms().saturating_add(refresh_budget_ms(config) as i64)
}

/// True while a manual refresh is in flight. The sentinel holds an absolute
/// deadline (epoch ms) derived from the fetch budget, so a refresh counts as
/// live until that deadline rather than a fixed TTL that a configured-slow fetch
/// could outlast. Sentinels written by older builds (a start timestamp, already
/// in the past) fall back to the mtime TTL.
pub fn refresh_in_progress(sentinel: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(sentinel) else {
        return false;
    };
    if let Ok(deadline) = contents.trim().parse::<i64>()
        && deadline > now_ms()
    {
        return true;
    }
    let Ok(meta) = fs::metadata(sentinel) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        return false;
    };
    age < REFRESH_SENTINEL_TTL
}

/// Persistent waybar text selection (lives next to the cache file).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WaybarState {
    /// Provider key (lowercase, e.g. "claude") currently shown in the waybar text.
    /// None = follow config (config.waybar.primary, else show all).
    pub selected: Option<String>,
    /// Unix milliseconds of the last rotation. Used to throttle rapid scroll events.
    #[serde(default)]
    pub last_rotated_ms: i64,
}

/// Derive the waybar-state path from the cache file path.
pub fn waybar_state_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-waybar-state.json")
}

pub fn read_waybar_state(path: &Path) -> WaybarState {
    let Ok(contents) = fs::read_to_string(path) else {
        return WaybarState::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn write_waybar_state(path: &Path, state: &WaybarState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let contents = serde_json::to_string(state)?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write waybar state {}", path.display()))
}

/// State for one-shot threshold notifications: tracks which thresholds we
/// already fired notifications for, per `(provider, window)` key, so we
/// don't spam the user on every refresh while above the limit.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotifyState {
    #[serde(default)]
    pub entries: HashMap<String, NotifyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NotifyEntry {
    #[serde(default)]
    pub notified: Vec<u8>,
    /// The window's reset timestamp when we last fired. A change means the
    /// window rolled over, which clears the one-shot guard.
    #[serde(default)]
    pub resets_at: Option<String>,
}

/// Cached result of the last GitHub release check. Written by the waybar
/// binary (which owns the network code) and read by the GUIs so opening the
/// a panel frontend never triggers a network call.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateStatus {
    /// Currently-installed version (no leading `v`).
    #[serde(default)]
    pub current: String,
    /// Latest release version seen on GitHub (no leading `v` - self_update
    /// normalizes the tag, e.g. `0.9.0`), if a check succeeded. Display sites
    /// prepend their own `v`.
    #[serde(default)]
    pub latest: Option<String>,
    /// True when `latest` is newer than `current`.
    #[serde(default)]
    pub available: bool,
    /// Unix ms of the last successful check.
    #[serde(default)]
    pub checked_ms: i64,
    /// Version we last fired a desktop notification for (one-shot guard).
    #[serde(default)]
    pub notified: Option<String>,
}

pub fn update_status_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-update.json")
}

pub fn read_update_status(cache_file: &Path) -> Option<UpdateStatus> {
    let path = update_status_path(cache_file);
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

pub fn write_update_status(cache_file: &Path, status: &UpdateStatus) -> Result<()> {
    let path = update_status_path(cache_file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let contents = serde_json::to_string(status)?;
    fs::write(&path, contents)
        .with_context(|| format!("failed to write update status {}", path.display()))
}

pub fn notify_state_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-notify-state.json")
}

/// Marks that the one-time history backfill has been attempted.
///
/// Its presence is the whole state, and it is written whether the read found
/// anything or not: a machine with no transcripts to backfill must not re-walk
/// the whole tree on every fetch forever looking for them.
pub fn backfill_marker_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-backfilled")
}

pub fn backfill_done(cache_file: &Path) -> bool {
    backfill_marker_path(cache_file).exists()
}

pub fn mark_backfilled(cache_file: &Path) {
    let path = backfill_marker_path(cache_file);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&path, chrono::Utc::now().to_rfc3339()).ok();
}

pub fn read_notify_state(path: &Path) -> NotifyState {
    let Ok(contents) = fs::read_to_string(path) else {
        return NotifyState::default();
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn write_notify_state(path: &Path, state: &NotifyState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let contents = serde_json::to_string(state)?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write notify state {}", path.display()))
}

/// Anthropic recomputes `resets_at` per request, so the same window comes back
/// with a sub-second component a few hundred microseconds later every fetch. A
/// real roll-over moves the boundary by hours; anything under a minute is that
/// jitter, and treating it as a new window re-fires every threshold on every
/// poll.
const ROLLOVER_TOLERANCE: chrono::TimeDelta = chrono::TimeDelta::minutes(1);

/// Pure decision logic: given the current pct, the window's reset timestamp,
/// and the previously-notified thresholds, returns (thresholds_to_fire,
/// updated_notified_list).
///
/// Window roll-over clears the one-shot guard so the new window can alert
/// again. The reset timestamp is the reliable signal: when `resets_at` advances
/// by more than `ROLLOVER_TOLERANCE` the window rolled. Only when a provider
/// gives no timestamp do we fall back to the legacy heuristic (pct fell 10+
/// points below the highest fired threshold) - which mis-fires when a fresh
/// window briefly reports a stale-high percent, or when the value wobbles near
/// the top and clears + re-fires on every poll, spamming alerts.
pub fn thresholds_to_fire(
    pct: u8,
    resets_at: Option<&str>,
    prev_resets_at: Option<&str>,
    thresholds: &[u8],
    notified: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let mut current = notified.to_vec();
    let pct_drop = || {
        current
            .iter()
            .max()
            .is_some_and(|&max_notified| pct.saturating_add(10) < max_notified)
    };
    let rolled_over = match (resets_at, prev_resets_at) {
        // Only a forward move past the tolerance is a new window. A stale/older
        // payload must not clear the guard, else the real timestamp returns next
        // poll and notifications re-fire.
        (Some(now), Some(prev)) => match (
            DateTime::parse_from_rfc3339(now),
            DateTime::parse_from_rfc3339(prev),
        ) {
            (Ok(now), Ok(prev)) => now - prev > ROLLOVER_TOLERANCE,
            // Malformed timestamp: treat as unavailable, fall back to heuristic.
            _ => pct_drop(),
        },
        // First time we see a timestamp for this window: not a roll-over -
        // unless it is malformed, then treat it as unavailable.
        (Some(now), None) => DateTime::parse_from_rfc3339(now)
            .map(|_| false)
            .unwrap_or_else(|_| pct_drop()),
        // No timestamp available: legacy pct-drop heuristic.
        (None, _) => pct_drop(),
    };
    if rolled_over {
        current.clear();
    }
    let mut sorted = thresholds.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut to_fire = Vec::new();
    for &t in &sorted {
        if pct >= t && !current.contains(&t) {
            to_fire.push(t);
            current.push(t);
        }
    }
    current.sort_unstable();
    current.dedup();
    (to_fire, current)
}

pub fn ensure_config_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    Ok(())
}

pub fn ensure_cache_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create cache directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A directory of this test's own. Every state file hangs off the
    /// snapshot's parent, so giving a test its own parent gives it the whole
    /// set - which is the point of that rule.
    pub(crate) fn cache_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    pub(crate) fn tempdir_for_test(prefix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        path.push(format!("tokengauge-test-{prefix}-{pid}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn legacy_state_moves_out_of_the_temp_dir_once() {
        // Against a temp directory of the test's own: the real one may hold a
        // snapshot an older daemon is still writing, and moving that out from
        // under it is not something a test run gets to do.
        let root = cache_test_dir("migrate");
        let legacy_dir = root.join("tmp");
        let dir = root.join("state");
        fs::create_dir_all(&legacy_dir).expect("create legacy dir");
        let cache = dir.join("tokengauge-usage.json");
        let legacy = legacy_dir.join("tokengauge-usage.json");
        fs::write(&legacy, "snapshot").expect("seed legacy cache");
        fs::write(
            legacy_dir.join("tokengauge-waybar-state.json"),
            "{\"selected\":\"claude\"}",
        )
        .expect("seed legacy selection");

        migrate_state_from(&legacy_dir, &cache);
        assert_eq!(fs::read_to_string(&cache).expect("read cache"), "snapshot");
        assert!(!legacy.exists(), "the temp copy is moved, not duplicated");
        // The selected provider comes along; the sentinel and the socket do not.
        assert!(dir.join("tokengauge-waybar-state.json").exists());

        // A second run must not clobber a snapshot written since.
        fs::write(&cache, "current").expect("write current cache");
        fs::write(&legacy, "older").expect("reseed legacy cache");
        migrate_state_from(&legacy_dir, &cache);
        assert_eq!(fs::read_to_string(&cache).expect("read cache"), "current");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn waybar_state_path_lives_next_to_cache() {
        let cache = PathBuf::from("/tmp/foo/bar.json");
        let state = waybar_state_path(&cache);
        assert_eq!(
            state,
            PathBuf::from("/tmp/foo/tokengauge-waybar-state.json")
        );
    }

    #[test]
    fn waybar_state_round_trips() {
        let tmp = tempdir_for_test("waybar_state");
        let path = tmp.join("state.json");
        let state = WaybarState {
            selected: Some("claude".to_string()),
            last_rotated_ms: 12345,
        };
        write_waybar_state(&path, &state).expect("write state");
        let read = read_waybar_state(&path);
        assert_eq!(read.selected.as_deref(), Some("claude"));
        assert_eq!(read.last_rotated_ms, 12345);
    }

    #[test]
    fn waybar_state_legacy_without_last_rotated_parses() {
        let tmp = tempdir_for_test("waybar_state_legacy");
        let path = tmp.join("state.json");
        fs::write(&path, r#"{"selected":"codex"}"#).unwrap();
        let read = read_waybar_state(&path);
        assert_eq!(read.selected.as_deref(), Some("codex"));
        assert_eq!(read.last_rotated_ms, 0);
    }

    #[test]
    fn thresholds_to_fire_below_no_trigger() {
        let (fire, notified) = thresholds_to_fire(40, None, None, &[50, 80, 95], &[]);
        assert!(fire.is_empty());
        assert!(notified.is_empty());
    }

    #[test]
    fn thresholds_to_fire_crosses_50_once() {
        let (fire, notified) = thresholds_to_fire(55, None, None, &[50, 80, 95], &[]);
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_already_notified_50_now_at_60() {
        let (fire, notified) = thresholds_to_fire(60, None, None, &[50, 80, 95], &[50]);
        assert!(fire.is_empty());
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_jumps_past_two() {
        let (fire, notified) = thresholds_to_fire(82, None, None, &[50, 80, 95], &[]);
        assert_eq!(fire, vec![50, 80]);
        assert_eq!(notified, vec![50, 80]);
    }

    #[test]
    fn thresholds_to_fire_resets_on_pct_drop() {
        // No timestamp: legacy heuristic. notified up to 80, pct dropped to 5.
        let (fire, notified) = thresholds_to_fire(5, None, None, &[50, 80, 95], &[50, 80]);
        assert!(fire.is_empty());
        assert!(notified.is_empty(), "drop below 80-10=70 must clear");
    }

    #[test]
    fn thresholds_to_fire_resets_then_recrosses() {
        // No timestamp: dropped to 0, then climbed to 60.
        let (fire, notified) = thresholds_to_fire(60, None, None, &[50, 80, 95], &[50, 80]);
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_small_fluctuation_no_reset() {
        // No timestamp: notified 80, pct dipped to 75 (within 10) - no reset.
        let (fire, notified) = thresholds_to_fire(75, None, None, &[50, 80], &[50, 80]);
        assert!(fire.is_empty());
        assert_eq!(notified, vec![50, 80]);
    }

    #[test]
    fn thresholds_to_fire_same_reset_no_respam_on_wobble() {
        // Same window (same resets_at). A stale-high or wobbling percent must
        // NOT re-fire an already-notified threshold - this is the spam bug.
        let rat = Some("2026-07-20T00:00:00Z");
        let (fire, notified) = thresholds_to_fire(100, rat, rat, &[50, 80, 95], &[50, 80, 95]);
        assert!(fire.is_empty(), "same window must not re-fire");
        assert_eq!(notified, vec![50, 80, 95]);
    }

    #[test]
    fn thresholds_to_fire_new_reset_clears_and_refires() {
        // The window rolled over (resets_at advanced). The one-shot guard clears
        // so the fresh window can alert again from a genuinely high percent.
        let (fire, notified) = thresholds_to_fire(
            96,
            Some("2026-07-27T00:00:00Z"),
            Some("2026-07-20T00:00:00Z"),
            &[50, 80, 95],
            &[50, 80, 95],
        );
        assert_eq!(fire, vec![50, 80, 95]);
        assert_eq!(notified, vec![50, 80, 95]);
    }

    #[test]
    fn thresholds_to_fire_subsecond_jitter_no_respam() {
        // Anthropic recomputes resets_at per request: the same window comes
        // back a few hundred microseconds later on every fetch. A strict `>`
        // read that as a roll-over and re-fired every threshold every poll.
        let (fire, notified) = thresholds_to_fire(
            51,
            Some("2026-08-27T10:00:00.737390+00:00"),
            Some("2026-08-27T10:00:00.026022+00:00"),
            &[50, 80, 95],
            &[50],
        );
        assert!(fire.is_empty(), "sub-second jitter must not re-fire");
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_stale_older_timestamp_no_clear() {
        // A stale payload reports an OLDER resets_at than what we last saw. It
        // must not clear the guard - otherwise the real timestamp returns on
        // the next poll and every already-notified threshold re-fires.
        let (fire, notified) = thresholds_to_fire(
            100,
            Some("2026-07-13T00:00:00Z"),
            Some("2026-07-20T00:00:00Z"),
            &[50, 80, 95],
            &[50, 80, 95],
        );
        assert!(fire.is_empty(), "older timestamp must not re-fire");
        assert_eq!(notified, vec![50, 80, 95]);
    }

    #[test]
    fn thresholds_to_fire_malformed_timestamp_falls_back_to_heuristic() {
        // A malformed resets_at must be treated as unavailable: no clearing on
        // the raw inequality. With a still-high percent the guard holds.
        let (fire, notified) = thresholds_to_fire(
            100,
            Some("not-a-date"),
            Some("2026-07-20T00:00:00Z"),
            &[50, 80, 95],
            &[50, 80, 95],
        );
        assert!(fire.is_empty(), "malformed timestamp must not re-fire");
        assert_eq!(notified, vec![50, 80, 95]);

        // Same malformed input but a dropped percent: legacy heuristic rolls it.
        let (fire, notified) = thresholds_to_fire(
            60,
            Some("not-a-date"),
            Some("2026-07-20T00:00:00Z"),
            &[50, 80, 95],
            &[50, 80, 95],
        );
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);

        // Malformed with no prev (legacy state has no resets_at): a dropped
        // percent must still roll over via the heuristic, not stay guarded.
        let (fire, notified) =
            thresholds_to_fire(60, Some("not-a-date"), None, &[50, 80, 95], &[50, 80, 95]);
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_first_timestamp_sighting_no_clear() {
        // First time we see a timestamp (prev None) is not a roll-over.
        let (fire, notified) = thresholds_to_fire(
            96,
            Some("2026-07-20T00:00:00Z"),
            None,
            &[50, 80, 95],
            &[50, 80, 95],
        );
        assert!(fire.is_empty());
        assert_eq!(notified, vec![50, 80, 95]);
    }

    #[test]
    fn notify_state_path_lives_next_to_cache() {
        let cache = PathBuf::from("/tmp/foo/bar.json");
        let p = notify_state_path(&cache);
        assert_eq!(p, PathBuf::from("/tmp/foo/tokengauge-notify-state.json"));
    }

    #[test]
    fn waybar_state_missing_file_returns_default() {
        let path = PathBuf::from("/tmp/tokengauge-state-doesnt-exist-xyz.json");
        let _ = fs::remove_file(&path);
        let state = read_waybar_state(&path);
        assert_eq!(state.selected, None);
    }

    #[test]
    fn refresh_budget_scales_past_fixed_ttl() {
        let config = TokenGaugeConfig {
            timeout_secs: 45,
            ccusage_timeout_secs: 10,
            stagger_ms: 0,
            ..TokenGaugeConfig::default()
        };
        // Budget follows the larger configured timeout, so a 45s fetch is not
        // classed stale by a 30s TTL.
        assert_eq!(
            refresh_budget_ms(&config),
            45_000 + REFRESH_SENTINEL_MARGIN_MS
        );
        assert!(refresh_budget_ms(&config) > REFRESH_SENTINEL_TTL.as_millis() as u64);
    }

    #[test]
    fn refresh_in_progress_honors_future_deadline() {
        let dir = tempdir_for_test("sentinel");
        let sentinel = dir.join("tokengauge-refreshing");
        fs::write(&sentinel, (now_ms() + 3_600_000).to_string()).unwrap();
        assert!(refresh_in_progress(&sentinel));
        fs::remove_file(&sentinel).unwrap();
        assert!(!refresh_in_progress(&sentinel));
    }
}
