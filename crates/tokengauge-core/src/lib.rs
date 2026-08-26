use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

// Path-and-copy only, so every crate gets it without the network stack that
// `self-update` pulls in.
pub mod frontend;

#[cfg(feature = "self-update")]
pub mod update;

// ============================================================================
// Provider payload types
//
// These are the internal model the native fetchers produce and the frontends
// render; they are also the on-disk cache format (`CachedData`). The camelCase
// serde naming is preserved so caches written by earlier versions still read.
// ============================================================================

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub primary: Option<UsageWindow>,
    pub secondary: Option<UsageWindow>,
    #[serde(default)]
    pub tertiary: Option<UsageWindow>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub login_method: Option<String>,
    #[serde(default)]
    pub extra_rate_windows: Vec<ExtraRateWindow>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraRateWindow {
    pub id: Option<String>,
    pub title: Option<String>,
    pub window: Option<UsageWindow>,
    /// True when the provider exposes a slot for this window but reports
    /// nothing in it - a feature the account does not have, rather than one it
    /// has and has not used. Frontends with room for only real windows drop
    /// these; the waybar module keeps them so its shape does not shift.
    #[serde(default)]
    pub placeholder: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    #[serde(default)]
    pub used_percent: Option<u8>,
    #[serde(default)]
    pub reset_description: Option<String>,
    #[serde(default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub window_minutes: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Credits {
    pub remaining: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderError {
    pub message: Option<String>,
    pub code: Option<i32>,
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPayload {
    pub provider: String,
    pub version: Option<String>,
    pub source: Option<String>,
    pub usage: Option<UsageSnapshot>,
    pub credits: Option<Credits>,
    pub error: Option<ProviderError>,
    /// True when this payload was served from a previous cache because the
    /// live fetch failed. Set by `fetch_all_providers`, not by the fetchers.
    #[serde(default)]
    pub stale: bool,
}

impl ProviderPayload {
    /// Returns true if this payload represents an error (no usage data).
    pub fn has_error(&self) -> bool {
        self.error.is_some()
    }
}

// ============================================================================
// Provider Registry
// ============================================================================

// ============================================================================
// Native fetcher helpers (shared by the claude/codex modules)
// ============================================================================

mod ccusage;
mod claude;
mod codex;
pub mod cost;
mod device;
pub mod fmt;
mod glm;
mod grok;
mod kimi;
pub mod launch;
pub mod pace;
pub mod panel;
mod provider;
pub mod providers;
pub mod statefiles;
pub mod sync;
pub mod theme;

pub use ccusage::*;
pub use device::*;
pub use fmt::{
    format_tokens, format_updated, format_updated_relative, month_start, now_ms, sparkline,
};
pub(crate) use fmt::{pct_u8, slug};
pub use providers::*;
pub use statefiles::*;
pub use theme::*;

pub use cost::{CostSource, NativeCostReport};
pub use pace::{PaceStage, UsagePace};
pub use panel::{PanelRow, Section, SectionKind, Tone, panel_spec};
pub use sync::config::{
    SyncConfig, SyncDirConfig, SyncProvidersConfig, SyncS3Config, SyncTransportKind,
    config_set_sync_dir, config_set_sync_enabled, config_set_sync_label, config_set_sync_provider,
    config_set_sync_s3, config_set_sync_transport,
};

/// A blocking HTTP client with the per-request timeout wired to the config's
/// `timeout_secs` (the subprocess-kill timeout is gone with codexbar).
pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client")
}

// ============================================================================
// Configuration Types
// ============================================================================

/// Provider configuration section.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ProvidersConfig {
    pub codex: Option<bool>,
    pub claude: Option<bool>,
    pub kimi: Option<bool>,
    pub grok: Option<bool>,
    pub glm: Option<bool>,
    /// Removed-provider keys (e.g. `[providers.zai]`) left over from older
    /// configs. Captured so `--doctor` can warn instead of silently ignoring.
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WaybarConfig {
    pub window: WaybarWindow,
    /// Which side of the bar the module is installed on. No Rust code reads
    /// this - `scripts/install.sh` does, to decide where in waybar's own
    /// `modules-left`/`modules-right` to insert us, and it reads the value back
    /// out of here so a re-install keeps the side you chose. It is declared
    /// here so the field is not reported as an unknown key.
    pub placement: WaybarPlacement,
    pub primary: Option<String>,
    pub scroll_throttle_ms: u64,
    /// What happens on left-click on the waybar module. Only the TUI is left;
    /// `"popover"` still parses so an existing config keeps loading, and
    /// resolves to the TUI.
    pub click_action: ClickAction,
    /// Shell command used when `click_action = "tui"`. Empty = auto-detect
    /// (omarchy-launch-or-focus-tui if available, else $TERMINAL -e tokengauge-tui).
    pub tui_command: String,
    /// Keys serde would otherwise drop in silence. The popover options lived
    /// here until 0.20.0 removed it, so a config carrying them still loads and
    /// the doctor can say which lines are dead.
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl Default for WaybarConfig {
    fn default() -> Self {
        Self {
            window: WaybarWindow::Daily,
            placement: WaybarPlacement::default(),
            primary: None,
            scroll_throttle_ms: 250,
            click_action: ClickAction::default(),
            tui_command: String::new(),
            unknown: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WaybarWindow {
    #[default]
    Daily,
    Weekly,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum WaybarPlacement {
    Left,
    #[default]
    Right,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClickAction {
    #[default]
    Tui,
    /// Removed in 0.20.0 - the waybar tooltip is the panel now. Kept so a
    /// config still carrying `click_action = "popover"` loads instead of
    /// failing to parse; it resolves to the TUI.
    Popover,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TokenGaugeConfig {
    pub refresh_secs: u64,
    pub cache_file: PathBuf,
    /// Timeout in seconds for each provider request
    pub timeout_secs: u64,
    /// Delay in milliseconds between consecutive provider fetch starts. Spreads
    /// out fetches to avoid rate-limit (429) bursts. 0 disables staggering (all
    /// providers fetched at once).
    pub stagger_ms: u64,
    /// Master switch for cost figures. Off means no cost rows at all, whatever
    /// `cost_source` says. Named for the days when ccusage was the only way to
    /// get them.
    pub ccusage_enabled: bool,
    /// Timeout in seconds for each ccusage call, and for the price-table fetch.
    pub ccusage_timeout_secs: u64,
    /// Which mechanism produces cost figures: the native transcript readers,
    /// the ccusage subprocess, or native with ccusage as the fallback.
    pub cost_source: CostSource,
    pub providers: ProvidersConfig,
    pub waybar: WaybarConfig,
    pub notifications: NotificationsConfig,
    pub theme: ThemeConfig,
    pub update: UpdateConfig,
    pub sync: SyncConfig,
    /// Unknown top-level keys (e.g. the removed `codexbar_bin`) left over from
    /// older configs. Captured so `--doctor` can warn instead of ignoring.
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl TokenGaugeConfig {
    /// Config keys that are no longer recognized (own top-level keys plus any
    /// `providers.<name>` left from a removed provider), sorted for stable output.
    pub fn unknown_config_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.unknown.keys().cloned().collect();
        keys.extend(
            self.providers
                .unknown
                .keys()
                .map(|k| format!("providers.{k}")),
        );
        keys.extend(self.waybar.unknown.keys().map(|k| format!("waybar.{k}")));
        keys.extend(self.sync.unknown.keys().map(|k| format!("sync.{k}")));
        keys.extend(
            self.sync
                .providers
                .unknown
                .keys()
                .map(|k| format!("sync.providers.{k}")),
        );
        keys.sort();
        keys
    }
}

impl ThemeConfig {
    /// Build a concrete Theme by resolving the preset and applying any
    /// per-field overrides on top.
    pub fn resolve(&self) -> Theme {
        let base = match self.preset.to_lowercase().as_str() {
            "nord" => Theme::nord(),
            "gruvbox" => Theme::gruvbox(),
            _ => Theme::catppuccin(),
        };
        Theme {
            dim: self.dim.clone().unwrap_or(base.dim),
            separator: self.separator.clone().unwrap_or(base.separator),
            green: self.green.clone().unwrap_or(base.green),
            yellow: self.yellow.clone().unwrap_or(base.yellow),
            red: self.red.clone().unwrap_or(base.red),
            neutral: self.neutral.clone().unwrap_or(base.neutral),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Enable desktop notifications (via `notify-send`) when usage crosses thresholds.
    pub enabled: bool,
    /// Percentage thresholds at which to notify. Applied per (provider, window).
    pub thresholds: Vec<u8>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thresholds: vec![50, 80, 95],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Have the daemon periodically check GitHub releases and notify (via
    /// `notify-send`) when a newer version is available. Applying is never
    /// automatic - the user triggers `tokengauge --update`.
    pub check: bool,
    /// Seconds between daemon update checks. Default 6h.
    pub check_interval_secs: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            check_interval_secs: 21600,
        }
    }
}

impl Default for TokenGaugeConfig {
    fn default() -> Self {
        Self {
            refresh_secs: 600,
            cache_file: default_cache_file(),
            timeout_secs: 20,
            stagger_ms: 0,
            ccusage_enabled: true,
            ccusage_timeout_secs: 15,
            providers: ProvidersConfig {
                codex: Some(true),
                claude: Some(true),
                kimi: None,
                grok: None,
                glm: None,
                unknown: HashMap::new(),
            },
            cost_source: CostSource::default(),
            waybar: WaybarConfig::default(),
            notifications: NotificationsConfig::default(),
            theme: ThemeConfig::default(),
            update: UpdateConfig::default(),
            sync: SyncConfig::default(),
            unknown: HashMap::new(),
        }
    }
}

// ============================================================================
// Fetch Results
// ============================================================================

/// Error from fetching a single provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderFetchError {
    pub provider: String,
    /// Short, cleaned-up error message for display
    pub message: String,
    /// Full raw error message for debugging
    pub raw: String,
}

impl ProviderFetchError {
    /// Create a new error with both cleaned and raw messages.
    pub fn new(provider: String, raw_message: &str) -> Self {
        Self {
            provider,
            message: clean_error_message(raw_message),
            raw: raw_message.to_string(),
        }
    }
}

/// Shorten a fetch error for display. The native fetchers already produce
/// concise, purpose-written messages, so this only guards against runaway
/// length (e.g. a raw provider error body) and normalizes timeouts.
fn clean_error_message(raw: &str) -> String {
    // reqwest phrases its own timeout "operation timed out". Matching a bare
    // "timeout" rewrote any provider error body that merely mentioned the word -
    // including one telling the user to raise their own timeout setting, which
    // then read as the request having timed out.
    if raw.contains("timed out") {
        return "Request timed out".to_string();
    }
    // Char-boundary-safe truncation: `raw` may be an HTTP body with multi-byte
    // characters, so a byte slice at 57 could split a codepoint and panic.
    if raw.chars().count() <= 60 {
        return raw.to_string();
    }
    let mut s: String = raw.chars().take(57).collect();
    s.push_str("...");
    s
}

/// Result of fetching all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    pub payloads: Vec<ProviderPayload>,
    pub errors: Vec<ProviderFetchError>,
    #[serde(default)]
    pub costs: HashMap<String, CostInfo>,
    pub sync: sync::SyncStatus,
}

/// Bumped when the on-disk snapshot grows a field a reader has to know about.
pub const CACHE_SCHEMA_VERSION: u32 = 1;

/// Provenance of one snapshot write.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheMeta {
    pub schema_version: u32,
    pub device: DeviceIdentity,
    /// Unix milliseconds of the write. A merge across machines needs it to
    /// decide which snapshot of the same day is the later one.
    pub updated_at_ms: i64,
    /// Providers enabled at fetch time. See `CachedData::covers`.
    pub providers: Vec<String>,
}

/// Cached data format - stores both payloads and errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CachedData {
    /// New format with payloads and errors
    Full {
        payloads: Vec<ProviderPayload>,
        errors: Vec<ProviderFetchError>,
        #[serde(default)]
        costs: HashMap<String, CostInfo>,
        /// Absent in snapshots written before 0.21.
        #[serde(default)]
        meta: Option<CacheMeta>,
        /// Absent unless fleet sync is on. Boxed: a status carries several
        /// vectors, and the legacy variant is a bare list.
        #[serde(default)]
        sync: Option<Box<sync::SyncStatus>>,
    },
    /// Legacy format - just an array of payloads (for backwards compatibility)
    Legacy(Vec<ProviderPayload>),
}

impl CachedData {
    pub fn payloads(&self) -> &[ProviderPayload] {
        match self {
            CachedData::Full { payloads, .. } => payloads,
            CachedData::Legacy(payloads) => payloads,
        }
    }

    pub fn errors(&self) -> &[ProviderFetchError] {
        match self {
            CachedData::Full { errors, .. } => errors,
            CachedData::Legacy(_) => &[],
        }
    }

    pub fn costs(&self) -> HashMap<String, CostInfo> {
        match self {
            CachedData::Full { costs, .. } => costs.clone(),
            CachedData::Legacy(_) => HashMap::new(),
        }
    }

    pub fn meta(&self) -> Option<&CacheMeta> {
        match self {
            CachedData::Full { meta, .. } => meta.as_ref(),
            CachedData::Legacy(_) => None,
        }
    }

    pub fn sync(&self) -> Option<&sync::SyncStatus> {
        match self {
            CachedData::Full { sync, .. } => sync.as_deref(),
            CachedData::Legacy(_) => None,
        }
    }

    /// True when the snapshot was fetched with every currently-enabled
    /// provider in the set. A snapshot written before a provider was switched
    /// on holds no row for it and never will, so serving it leaves the panel
    /// without the provider the user just enabled - which is why enabling one
    /// has to invalidate the cache, not merely age it.
    pub fn covers(&self, providers: &ProvidersConfig) -> bool {
        let Some(meta) = self.meta() else {
            return false;
        };
        providers.enabled_providers().iter().all(|wanted| {
            meta.providers
                .iter()
                .any(|have| have.eq_ignore_ascii_case(wanted))
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<ProviderPayload>,
        Vec<ProviderFetchError>,
        HashMap<String, CostInfo>,
    ) {
        match self {
            CachedData::Full {
                payloads,
                errors,
                costs,
                ..
            } => (payloads, errors, costs),
            CachedData::Legacy(payloads) => (payloads, Vec::new(), HashMap::new()),
        }
    }
}

/// Cost info for a provider (sourced from ccusage).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostInfo {
    pub today_usd: f64,
    pub today_tokens: u64,
    pub monthly_usd: f64,
    pub monthly_tokens: u64,
    #[serde(default)]
    pub today_models: Vec<ModelCost>,
    #[serde(default)]
    pub monthly_models: Vec<ModelCost>,
    #[serde(default)]
    pub burn_rate: Option<BurnRate>,
    /// Cost accrued in the current ccusage 5h session block (matches the
    /// Session usage row anchored to claude.ai's reset, approximately).
    #[serde(default)]
    pub session_usd: f64,
    /// Sum of the last 7 days of cost (rolling weekly cost).
    #[serde(default)]
    pub weekly_usd: f64,
    /// Last N days of total cost per day (oldest -> newest). N = up to 7.
    #[serde(default)]
    pub weekly_cost_history: Vec<f64>,
    /// Same window as `weekly_cost_history`, carrying the date and the token
    /// count each day's cost was rated from.
    #[serde(default)]
    pub weekly_history: Vec<DayCost>,
    /// Per-device share of the month, present only when this provider is
    /// fleet-merged. Its presence is what tells a reader the figures above
    /// cover more than this machine.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_device: Vec<sync::DeviceCost>,
    /// What the panel says about sync state. Error-first: see [`panel::SyncNote`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_note: Option<panel::SyncNote>,
}

impl CostInfo {
    /// Average daily cost over the previous days of history, excluding today
    /// (the newest entry) so a partial day doesn't dilute its own baseline.
    /// Returns None with fewer than two days of history, or a zero sum.
    pub fn avg_daily_cost(&self) -> Option<f64> {
        let prior = self.weekly_cost_history.split_last()?.1;
        if prior.is_empty() {
            return None;
        }
        let sum: f64 = prior.iter().sum();
        if sum <= 0.0 {
            return None;
        }
        Some(sum / prior.len() as f64)
    }

    /// Today's spend as a percentage change against `avg_daily_cost`.
    pub fn today_vs_avg_percent(&self) -> Option<f64> {
        let avg = self.avg_daily_cost().filter(|a| *a > 0.0)?;
        Some((self.today_usd - avg) / avg * 100.0)
    }
}

/// One day of spend, as ccusage rated it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayCost {
    /// ccusage `period`, `YYYY-MM-DD`.
    pub date: String,
    pub usd: f64,
    pub tokens: u64,
}

/// Per-model cost slice (ccusage modelBreakdowns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub model: String,
    pub usd: f64,
    pub tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
}

/// Current burn rate + 5h-block projection from ccusage `blocks --active`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurnRate {
    pub cost_per_hour: f64,
    pub tokens_per_minute: u64,
    pub remaining_minutes: u32,
    pub projected_cost: f64,
}

// ============================================================================
// Provider Row (for display)
// ============================================================================

#[derive(Debug, Clone, Serialize)]
pub struct ProviderRow {
    pub provider: String,
    pub session_used: Option<u8>,
    pub session_window_minutes: Option<u32>,
    pub session_reset: String,
    /// Burn pace for the session window, when it has a duration + reset time.
    pub session_pace: Option<UsagePace>,
    pub weekly_used: Option<u8>,
    pub weekly_window_minutes: Option<u32>,
    pub weekly_reset: String,
    /// Burn pace for the weekly window.
    pub weekly_pace: Option<UsagePace>,
    pub tertiary_used: Option<u8>,
    pub tertiary_reset: String,
    pub credits: String,
    pub source: String,
    pub updated: String,
    pub updated_iso: Option<String>,
    pub plan_label: Option<String>,
    pub extra_windows: Vec<ExtraWindowRow>,
    pub cost: Option<CostInfo>,
    /// True when this row came from a cached last-good payload after a failed
    /// live fetch.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtraWindowRow {
    pub title: String,
    pub used: Option<u8>,
    pub reset: String,
    /// Burn pace for this window, on the same terms as the session/weekly ones.
    pub pace: Option<UsagePace>,
    /// See [`ExtraRateWindow::placeholder`].
    pub placeholder: bool,
}

// ============================================================================
// Config Loading
// ============================================================================

pub fn load_config(path: Option<PathBuf>) -> Result<TokenGaugeConfig> {
    let path = path.unwrap_or_else(default_config_path);

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut config: TokenGaugeConfig = toml::from_str(&contents)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;

    // Apply defaults for empty values
    if config.cache_file.as_os_str().is_empty() {
        config.cache_file = default_cache_file();
    }
    // Every config written before 0.21 carries the old temp-dir path
    // explicitly, so treating it as an opt-out would strand those users in
    // /tmp forever. Read it as "never chose one" and move them.
    if config.cache_file == legacy_cache_file() {
        config.cache_file = default_cache_file();
    }
    migrate_legacy_state(&config.cache_file);
    if config.refresh_secs == 0 {
        config.refresh_secs = 600;
    }

    Ok(config)
}

// ============================================================================
// Fetching Logic
// ============================================================================

/// Run a subprocess with a hard timeout. On timeout, kills the child so it
/// does not leak. Captures stdout/stderr in background threads to avoid
/// deadlocking on full pipes.
fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to spawn subprocess")?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stdout_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut s) = stderr_pipe {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().context("subprocess wait failed")? {
            Some(status) => {
                let stdout = stdout_handle.join().unwrap_or_default();
                let stderr = stderr_handle.join().unwrap_or_default();
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!("timeout after {:?}", timeout));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Fetch all enabled providers in parallel.
pub fn fetch_all_providers(config: &TokenGaugeConfig) -> FetchResult {
    let enabled = config.providers.enabled_providers();
    let timeout = Duration::from_secs(config.timeout_secs);

    if enabled.is_empty() {
        return FetchResult {
            payloads: Vec::new(),
            errors: Vec::new(),
            costs: HashMap::new(),
            sync: sync::SyncStatus::default(),
        };
    }

    let ccusage_enabled = config.ccusage_enabled;
    let cost_config = config.clone();
    let cost_providers: Vec<&'static str> = enabled.clone();
    let ccusage_handle = thread::spawn(move || {
        if ccusage_enabled {
            fetch_costs(&cost_config, &cost_providers)
        } else {
            NativeCostReport::default()
        }
    });

    // Spawn threads for each provider. Each thread self-delays by its index
    // times `stagger_ms` so provider fetches are spread out (rate-limit relief)
    // without blocking the main spawn loop or the ccusage thread.
    let stagger = Duration::from_millis(config.stagger_ms);
    let handles: Vec<_> = enabled
        .into_iter()
        .enumerate()
        .map(|(i, provider)| {
            thread::spawn(move || {
                if !stagger.is_zero() && i > 0 {
                    thread::sleep(stagger.saturating_mul(i as u32));
                }
                let result = fetch_single_provider(provider, timeout);
                (provider.to_string(), result)
            })
        })
        .collect();

    // Collect results
    let mut payloads = Vec::new();
    let mut errors = Vec::new();

    for handle in handles {
        match handle.join() {
            Ok((provider_name, Ok(provider_payloads))) => {
                // Filter out payloads with errors and add successful ones
                for payload in provider_payloads {
                    if payload.has_error() {
                        let msg = payload
                            .error
                            .as_ref()
                            .and_then(|e| e.message.clone())
                            .unwrap_or_else(|| "Unknown error".to_string());
                        errors.push(ProviderFetchError::new(provider_name.clone(), &msg));
                    } else {
                        payloads.push(payload);
                    }
                }
            }
            Ok((provider_name, Err(e))) => {
                // {:#} prints the full anyhow cause chain ("ctx: cause1: cause2");
                // {} alone drops everything after the topmost context wrap.
                errors.push(ProviderFetchError::new(provider_name, &format!("{e:#}")));
            }
            Err(_) => {
                // Thread panicked - shouldn't happen normally
                errors.push(ProviderFetchError {
                    provider: "unknown".to_string(),
                    message: "thread panicked".to_string(),
                    raw: "thread panicked".to_string(),
                });
            }
        }
    }

    // Serve last-good cached data for providers that failed this round, so a
    // transient 429 / network blip surfaces as `stale` instead of a blank bar.
    if !errors.is_empty()
        && let Ok(previous) = read_cache_full(&config.cache_file)
    {
        apply_stale_fallback(&mut payloads, &mut errors, previous.payloads());
    }

    let mut report = ccusage_handle.join().unwrap_or_default();
    // Only now are the provider windows known, and the session figures are
    // measured against the real one rather than an inferred block.
    cost::anchor_burn_rates(&mut report, &payloads);
    FetchResult {
        payloads,
        errors,
        costs: report.costs,
        sync: report.sync,
    }
}

/// Replace each failed provider's error with its previous good payload (marked
/// stale) when the cache still holds one. Providers with no cached fallback
/// keep their error.
fn apply_stale_fallback(
    payloads: &mut Vec<ProviderPayload>,
    errors: &mut Vec<ProviderFetchError>,
    previous: &[ProviderPayload],
) {
    errors.retain(|err| {
        // A provider can return several payloads (one per account/window); if
        // one succeeded and another errored, the provider name is in both lists.
        // A per-name stale clone would then duplicate the live row - and a
        // second error for the same provider would clone it again. Skip once the
        // provider already has any payload (live or an earlier stale restore).
        if payloads
            .iter()
            .any(|p| p.provider.eq_ignore_ascii_case(&err.provider))
        {
            return false;
        }
        // Restore every cached payload for the provider (accounts/windows), not
        // just the first, so a full outage doesn't drop all but one row.
        let cached: Vec<ProviderPayload> = previous
            .iter()
            .filter(|p| !p.has_error() && p.provider.eq_ignore_ascii_case(&err.provider))
            .cloned()
            .collect();
        if cached.is_empty() {
            true // no fallback, keep the error
        } else {
            payloads.extend(cached.into_iter().map(|mut payload| {
                payload.stale = true;
                payload
            }));
            false // drop the error, we have last-good data
        }
    });
}

// ============================================================================
// Payload Processing
// ============================================================================

pub fn payload_to_rows_with_costs(
    payloads: Vec<ProviderPayload>,
    costs: &HashMap<String, CostInfo>,
) -> Vec<ProviderRow> {
    payloads
        .into_iter()
        .filter(|payload| !payload.has_error())
        .map(|payload| {
            let cost = lookup_cost(&payload.provider, costs);
            let mut row = provider_to_row(payload);
            row.cost = cost;
            row
        })
        .collect()
}

fn lookup_cost(provider: &str, costs: &HashMap<String, CostInfo>) -> Option<CostInfo> {
    let key = provider.to_lowercase();
    if let Some(cost) = costs.get(&key) {
        return Some(cost.clone());
    }
    // A row's provider can be a longer spelling of the cost key ("claude-code"
    // against "claude") or the other way round. Only at a separator, and the
    // longest wins: a bare `starts_with` would let a future "claude-max"
    // answer for "claude", and since this walks a HashMap the money would land
    // on a different row from one run to the next.
    let extends = |long: &str, short: &str| {
        long.len() > short.len()
            && long.starts_with(short)
            && !long.as_bytes()[short.len()].is_ascii_alphanumeric()
    };
    costs
        .iter()
        .filter(|(k, _)| extends(&key, k) || extends(k, &key))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, v)| v.clone())
}

/// Compute burn pace for a usage window, if it has the percent, duration and
/// reset time pace needs.
fn window_pace(window: &UsageWindow, now: DateTime<Utc>) -> Option<UsagePace> {
    UsagePace::for_window(
        window.used_percent?,
        window.window_minutes,
        window.resets_at.as_deref(),
        now,
    )
}

pub fn format_window(window: Option<UsageWindow>) -> (Option<u8>, Option<u32>, String) {
    if let Some(window) = window {
        let used = window.used_percent.map(|used| used.min(100));
        let minutes = window.window_minutes;
        let reset = format_reset_time(window.resets_at.as_deref(), window.reset_description);
        (used, minutes, reset)
    } else {
        (None, None, "—".into())
    }
}

/// Format reset time as relative duration (e.g., "in 2h 30m") if possible,
/// otherwise fall back to the description (e.g., "Jan 22 at 5:59PM").
fn format_reset_time(resets_at: Option<&str>, description: Option<String>) -> String {
    if let Some(resets_at) = resets_at
        && let Ok(reset_time) = DateTime::parse_from_rfc3339(resets_at)
    {
        let now = Utc::now();
        let reset_utc = reset_time.with_timezone(&Utc);
        let duration = reset_utc.signed_duration_since(now);

        if duration.num_seconds() > 0 {
            return format!("in {}", fmt::format_duration(duration.num_minutes(), 3));
        }
    }
    // Fall back to description if we can't compute relative time
    description.unwrap_or_else(|| "—".to_string())
}

fn provider_to_row(payload: ProviderPayload) -> ProviderRow {
    let mut session_used = None;
    let mut session_window = None;
    let mut session_reset = "—".to_string();
    let mut weekly_used = None;
    let mut weekly_window = None;
    let mut weekly_reset = "—".to_string();
    let mut tertiary_used = None;
    let mut tertiary_reset = "—".to_string();
    let mut updated = "—".to_string();
    let mut updated_iso = None;
    let mut plan_label = None;
    let mut extra_windows = Vec::new();

    let mut session_pace = None;
    let mut weekly_pace = None;

    if let Some(usage) = payload.usage {
        let now = Utc::now();
        let live = !payload.stale;
        if live {
            session_pace = usage.primary.as_ref().and_then(|w| window_pace(w, now));
            weekly_pace = usage.secondary.as_ref().and_then(|w| window_pace(w, now));
        }

        let (s_used, s_win, s_reset) = format_window(usage.primary);
        session_used = s_used;
        session_window = s_win;
        session_reset = s_reset;

        let (w_used, w_win, w_reset) = format_window(usage.secondary);
        weekly_used = w_used;
        weekly_window = w_win;
        weekly_reset = w_reset;

        let (t_used, _, t_reset) = format_window(usage.tertiary);
        tertiary_used = t_used;
        tertiary_reset = t_reset;

        updated_iso = usage.updated_at.clone();
        updated = format_updated(usage.updated_at);
        plan_label = usage.login_method;

        extra_windows = usage
            .extra_rate_windows
            .into_iter()
            .filter_map(|w| {
                let title = w.title?;
                let placeholder = w.placeholder;
                let pace = if live {
                    w.window
                        .as_ref()
                        .and_then(|window| window_pace(window, now))
                } else {
                    None
                };
                let (used, _, reset) = format_window(w.window);
                Some(ExtraWindowRow {
                    title,
                    used,
                    reset,
                    pace,
                    placeholder,
                })
            })
            .collect();
    }

    let credits = payload
        .credits
        .and_then(|credits| credits.remaining)
        .map(|remaining| format!("{remaining:.2}"))
        .unwrap_or_else(|| "—".to_string());

    let source = match (payload.version, payload.source) {
        (Some(version), Some(source)) => format!("{version} ({source})"),
        (Some(version), None) => version,
        (None, Some(source)) => source,
        (None, None) => "—".to_string(),
    };

    ProviderRow {
        provider: provider_label(&payload.provider).to_string(),
        session_used,
        session_window_minutes: session_window,
        session_reset,
        session_pace,
        weekly_used,
        weekly_window_minutes: weekly_window,
        weekly_reset,
        weekly_pace,
        tertiary_used,
        tertiary_reset,
        credits,
        source,
        updated,
        updated_iso,
        plan_label,
        extra_windows,
        cost: None,
        stale: payload.stale,
    }
}

// ============================================================================
// Cache Operations
// ============================================================================

/// Read cache, returning both payloads and errors.
pub fn read_cache_full(path: &Path) -> Result<CachedData> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read cache file {}", path.display()))?;
    let cached: CachedData = serde_json::from_str(&contents).context("cached JSON was invalid")?;
    Ok(cached)
}

/// Write cache with payloads, errors and optional costs.
///
/// `providers` is the set the fetch ran with, not the set that answered: a
/// provider that errored still counts as covered, or a failing provider would
/// put every reader into a refetch loop.
pub fn write_cache_full(
    path: &Path,
    payloads: &[ProviderPayload],
    errors: &[ProviderFetchError],
    costs: &HashMap<String, CostInfo>,
    providers: &ProvidersConfig,
    sync: Option<&sync::SyncStatus>,
) -> Result<()> {
    let data = CachedData::Full {
        payloads: payloads.to_vec(),
        errors: errors.to_vec(),
        costs: costs.clone(),
        sync: sync.cloned().map(Box::new),
        meta: Some(CacheMeta {
            schema_version: CACHE_SCHEMA_VERSION,
            device: device_identity(path),
            updated_at_ms: now_ms(),
            providers: providers
                .enabled_providers()
                .into_iter()
                .map(str::to_string)
                .collect(),
        }),
    };
    let contents = serde_json::to_string(&data)?;
    write_atomic(path, contents.as_bytes())
        .with_context(|| format!("failed to write cache {}", path.display()))?;
    bump_revision(path);
    Ok(())
}

/// True when the on-disk snapshot cannot answer for this config: it is
/// missing, it has aged past `refresh_secs`, or it predates a provider that is
/// enabled now. Every reader routes its fetch-or-serve decision through here so
/// none of them can disagree about what stale means.
pub fn cache_is_stale(config: &TokenGaugeConfig) -> bool {
    let expired = fs::metadata(&config.cache_file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .map(|age| age >= Duration::from_secs(config.refresh_secs))
        .unwrap_or(true);
    if expired {
        return true;
    }
    match read_cache_full(&config.cache_file) {
        Ok(cached) => !cached.covers(&config.providers),
        Err(_) => true,
    }
}

/// Drop cached payloads and errors for providers that are no longer enabled.
/// The cache is written by whichever provider set was enabled at fetch time, so
/// a later toggle leaves it holding rows the user just disabled. Every read of
/// the cache is config-scoped through here; the cache file itself only catches
/// up on the next fetch.
pub fn retain_enabled(
    payloads: &mut Vec<ProviderPayload>,
    errors: &mut Vec<ProviderFetchError>,
    providers: &ProvidersConfig,
) {
    let enabled = providers.enabled_providers();
    let is_enabled = |name: &str| enabled.iter().any(|p| p.eq_ignore_ascii_case(name.trim()));
    payloads.retain(|p| is_enabled(&p.provider));
    errors.retain(|e| is_enabled(&e.provider));
}

// ============================================================================
// Display helpers (shared between waybar binary and TUI)
// ============================================================================

/// Directory the installer drops provider SVG logos into. Overridable with
/// `TOKENGAUGE_ICON_DIR` (e.g. point it at the repo `assets/providers` when
/// running a dev build).
pub fn provider_icon_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("TOKENGAUGE_ICON_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/share"));
    base.join("tokengauge").join("icons")
}

/// Path to a provider's bundled brand SVG, or None when no logo is installed
/// (the frontend then falls back to the glyph icon).
pub fn provider_icon_svg_path(label: &str) -> Option<PathBuf> {
    let slug = provider_icon_slug(label)?;
    let path = provider_icon_dir().join(format!("ProviderIcon-{slug}.svg"));
    path.exists().then_some(path)
}

// ============================================================================
// Waybar State (rotation selection)
// ============================================================================

// ============================================================================
// Cost Fetching
// ============================================================================

/// Produce cost figures through whichever mechanism `source` selects.
///
/// `Auto` prefers the native readers and falls back to ccusage only when they
/// found no transcripts at all, which is what a machine driving a CLI
/// TokenGauge cannot parse yet looks like. A machine that simply has not used
/// its assistants this month reads as zero spend, not as a missing source, so
/// the fallback keys on events read rather than on dollars.
pub fn fetch_costs(config: &TokenGaugeConfig, enabled: &[&str]) -> NativeCostReport {
    let today = Local::now().date_naive();
    let timeout = Duration::from_secs(config.ccusage_timeout_secs.max(1));
    match config.cost_source {
        CostSource::Ccusage => NativeCostReport {
            costs: fetch_ccusage_costs(timeout),
            sync: sync::SyncStatus {
                enabled: config.sync.enabled,
                error: config.sync.enabled.then(|| {
                    "cost_source = ccusage produces no usage events, so there is nothing to sync"
                        .to_string()
                }),
                ..Default::default()
            },
            ..Default::default()
        },
        CostSource::Native => native_costs(config, today, timeout),
        CostSource::Auto => {
            let mut report = native_costs(config, today, timeout);
            let missing = missing_providers(&report, enabled);
            if missing.is_empty() {
                return report;
            }
            // ccusage reads 22 agent formats; TokenGauge parses two trees. A
            // Kimi or Grok plan driven from its own CLI leaves nothing in
            // either, and dropping the cost row it had yesterday would be a
            // regression dressed up as an optimisation. Only the providers the
            // native read came up empty on are taken from ccusage, so the
            // common Claude/Codex machine never spawns it at all.
            for (provider, cost) in fetch_ccusage_costs(timeout) {
                if missing.contains(&provider) {
                    report.costs.insert(provider, cost);
                }
            }
            report
        }
    }
}

/// The native read, with the fleet folded in when sync is on.
///
/// Peer buckets arrive as synthetic events, so `build_report` sees one kind of
/// input and needs to know nothing about any of this.
fn native_costs(
    config: &TokenGaugeConfig,
    today: chrono::NaiveDate,
    timeout: Duration,
) -> NativeCostReport {
    if !config.sync.enabled {
        return cost::fetch_native(&config.cache_file, timeout, today);
    }
    let (mut events, since) = cost::read_window(today);
    let outcome = sync::refresh(config, &events, since);
    events.extend(outcome.events);

    let prices = cost::pricing::load(&config.cache_file, timeout, true);
    let mut report = cost::build_report(&events, &prices, today);
    attach_fleet(
        &mut report,
        &outcome.store,
        &prices,
        today,
        &sync::local_device_id(config),
        sync::note(&outcome.status, config.refresh_secs, now_ms()),
    );
    report.sync = outcome.status;
    report
}

/// Attach the fleet view to a rated report: who spent what this month, and what
/// the panel should say about sync.
///
/// Split from the reading because that is the seam where peer buckets actually
/// reach the panel, and reading needs a home directory while this needs
/// nothing.
pub fn attach_fleet(
    report: &mut NativeCostReport,
    store: &sync::FleetStore,
    prices: &cost::pricing::PriceTable,
    today: chrono::NaiveDate,
    local_id: &str,
    note: Option<panel::SyncNote>,
) {
    let month_start = fmt::month_start(today);
    let offset = *Local::now().offset();
    for (provider, cost) in report.costs.iter_mut() {
        cost.by_device =
            store.device_totals(provider, (month_start, today), offset, prices, local_id);
        cost.sync_note = note.clone();
    }
}

/// Enabled providers the native readers produced nothing for.
///
/// Keyed on events rather than on dollars: a provider that is enabled and
/// simply unused this month has a real answer, and it is zero.
fn missing_providers(report: &NativeCostReport, enabled: &[&str]) -> HashSet<String> {
    enabled
        .iter()
        .map(|p| p.to_lowercase())
        .filter(|p| !report.costs.contains_key(p))
        .collect()
}

/// What `--doctor` needs to say about where cost figures come from, and
/// whether the native readers agree with ccusage.
#[derive(Debug, Default)]
pub struct CostDiagnostics {
    pub source: CostSource,
    pub events: usize,
    pub elapsed: Duration,
    pub roots: Vec<PathBuf>,
    pub prices: usize,
    /// Which of the four fallbacks the price table came from. A cold or
    /// offline machine rates everything against the compiled-in copy, which
    /// is correct and completely invisible from the cost row.
    pub price_source: cost::pricing::PriceSource,
    pub unpriced: Vec<String>,
    /// Month-to-date tokens and spend per provider, from each source. The
    /// token counts are the parser check: they come from the transcripts and
    /// must agree. Cost can differ legitimately if the two rate against
    /// different price data.
    pub native: HashMap<String, (u64, f64)>,
    pub ccusage: Option<HashMap<String, (u64, f64)>>,
}

impl CostDiagnostics {
    /// Largest month-to-date token disagreement between the two sources, as a
    /// fraction. `None` when there is nothing to compare.
    ///
    /// A provider that one side reports and the other does not is full drift,
    /// not a skip - a Claude reader that breaks after a format change reports
    /// no `claude` key at all, and comparing only the keys it did produce would
    /// call that agreement. Restricted to the trees the readers actually parse:
    /// Kimi or Grok driven from its own CLI is legitimately ccusage-only, and
    /// the `auto` fallback exists precisely to cover it.
    pub fn worst_token_drift(&self) -> Option<(String, f64)> {
        let ccusage = self.ccusage.as_ref()?;
        let mut worst: Option<(String, f64)> = None;
        let mut consider = |provider: &String, drift: f64| {
            if worst.as_ref().is_none_or(|(_, w)| drift > *w) {
                worst = Some((provider.clone(), drift));
            }
        };

        let providers: std::collections::BTreeSet<&String> =
            self.native.keys().chain(ccusage.keys()).collect();
        for provider in providers {
            let mine = self.native.get(provider).map(|(t, _)| *t);
            let theirs = ccusage.get(provider).map(|(t, _)| *t);
            match (mine, theirs) {
                (Some(mine), Some(theirs)) => {
                    let denominator = theirs.max(mine) as f64;
                    if denominator > 0.0 {
                        consider(provider, (mine as f64 - theirs as f64).abs() / denominator);
                    }
                }
                // Present on one side only. Meaningful for the trees we parse,
                // expected for anything the fallback covers.
                (mine, theirs)
                    if natively_read().contains(&provider.as_str())
                        && (mine.unwrap_or(0) > 0 || theirs.unwrap_or(0) > 0) =>
                {
                    consider(provider, 1.0);
                }
                _ => {}
            }
        }
        worst
    }
}

/// Run the native readers, and ccusage alongside them when asked, so the two
/// can be compared. This is what keeps the dependency earning its keep: a
/// transcript format change shows up here as drift rather than as a number
/// that quietly stopped growing.
pub fn diagnose_costs(
    source: CostSource,
    cache_file: &Path,
    timeout: Duration,
    compare: bool,
) -> CostDiagnostics {
    let today = Local::now().date_naive();
    let started = Instant::now();
    let report = cost::fetch_native(cache_file, timeout, today);
    let elapsed = started.elapsed();

    let month_totals = |costs: &HashMap<String, CostInfo>| -> HashMap<String, (u64, f64)> {
        costs
            .iter()
            .map(|(provider, c)| (provider.clone(), (c.monthly_tokens, c.monthly_usd)))
            .collect()
    };

    // No network here: --doctor reports the table the cost path would use, and
    // a download from inside the diagnosis would report a freshness the real
    // fetch did not have.
    let (prices, price_source) = cost::pricing::load_with_source(cache_file, timeout, false);

    CostDiagnostics {
        source,
        events: report.events,
        elapsed,
        roots: cost::transcript_roots(),
        prices: prices.len(),
        price_source,
        native: month_totals(&report.costs),
        ccusage: compare.then(|| month_totals(&fetch_ccusage_costs(timeout))),
        unpriced: report.unpriced,
    }
}

// ============================================================================
// ccusage Integration
// ============================================================================

// ============================================================================
// Config File Operations
// ============================================================================

pub fn write_default_config(path: &Path) -> Result<()> {
    ensure_config_dir(path)?;
    let contents = r#"# TokenGauge Configuration

# Refresh interval in seconds
refresh_secs = 600

# Snapshot location. Defaults to $XDG_STATE_HOME/tokengauge/tokengauge-usage.json
# (%LOCALAPPDATA%\TokenGauge on Windows); the state files beside it - selected
# provider, daemon socket, refresh sentinel - follow its directory.
# cache_file = ""

# Delay in milliseconds between provider fetch starts. Spreads out codexbar
# calls to avoid rate-limit (429) bursts when several providers are enabled.
# 0 = fetch all at once (fastest, default).
stagger_ms = 0

# Master switch for cost figures. false = no cost rows at all.
ccusage_enabled = true
# Where cost figures come from:
#   "auto"    - read the transcripts natively, and ask ccusage only about
#               enabled providers the readers found nothing for, such as a
#               Kimi or Grok plan driven from its own CLI (default)
#   "native"  - native readers only, no subprocess and no Node/Bun needed
#   "ccusage" - the ccusage subprocess only
# cost_source = "auto"
# Timeout in seconds for each ccusage call, and for the price-table refresh.
ccusage_timeout_secs = 15

[notifications]
# Fire desktop notifications (notify-send) when usage crosses thresholds.
enabled = true
# Percentages to alert on (one notification per threshold per window).
thresholds = [50, 80, 95]

[waybar]
# Which window to show in waybar: "daily" or "weekly"
window = "daily"
# Where to place the module: "left" or "right"
placement = "right"
# Provider key shown in the waybar text. Unset = show all providers stacked.
# Mouse scroll over the module rotates the selection (overrides this until restart).
# primary = "claude"
# Left-click action: "tui" opens the terminal TUI.
click_action = "tui"
# Optional explicit launcher for click_action = "tui". Empty = auto-detect
# (omarchy-launch-or-focus-tui if present, else $TERMINAL -e tokengauge-tui).
# tui_command = "ghostty -e tokengauge-tui"

[providers]
# OAuth providers - set to true/false to enable/disable
codex = true
claude = true
# Kimi Code (kimi.com/code). Reads the kimi CLI token
# (~/.kimi-code/credentials/kimi-code.json) or the KIMI_CODE_API_KEY env var.
# Disabled by default; set to true after signing in with kimi.
# kimi = true
# Grok build (x.ai). Reads the grok CLI token (~/.grok/auth.json).
# Disabled by default; set to true after signing in with `grok login`.
# grok = true
# GLM Coding Plan (z.ai / zcode.z.ai). Reads the Z_AI_API_KEY env var
# (legacy ZAI_API_TOKEN). Set Z_AI_API_HOST for the China BigModel region.
# Disabled by default.
# glm = true
"#;
    fs::write(path, contents)
        .with_context(|| format!("failed to write config {}", path.display()))?;
    Ok(())
}

/// Apply an in-place edit to the config file, preserving comments/formatting
/// and writing atomically (temp file + rename) so a crash mid-write can't
/// truncate the user's config. Creates a default config first if none exists.
pub fn edit_config_file<F>(path: &Path, edit: F) -> Result<()>
where
    F: FnOnce(&mut toml_edit::DocumentMut),
{
    if !path.exists() {
        write_default_config(path)?;
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("config at {} is not valid TOML", path.display()))?;

    edit(&mut doc);

    // Through write_atomic, which names its temp per call. A fixed `.toml.tmp`
    // had two writers - `--set-provider` from a frontend and the settings pane
    // of another - clobbering each other's half-written file and renaming the
    // result over the config.
    write_atomic(path, doc.to_string().as_bytes())
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn ensure_table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    key: &str,
) -> &'a mut toml_edit::Table {
    if doc.get(key).and_then(|i| i.as_table()).is_none() {
        // An existing inline table (`providers = { codex = true }`) reads as None
        // via as_table(); convert it in place so its keys survive instead of
        // silently overwriting the user's settings with an empty table.
        let replacement = doc
            .get(key)
            .and_then(|i| i.as_inline_table())
            .cloned()
            .map(|t| toml_edit::Item::Table(t.into_table()))
            .unwrap_or_else(|| toml_edit::Item::Table(toml_edit::Table::new()));
        doc.insert(key, replacement);
    }
    doc[key].as_table_mut().expect("just ensured table")
}

/// Ask a running TokenGauge daemon (`tokengauge --daemon`) to reload its config
/// from disk without a restart. No-op when no daemon is running.
///
/// Matches the full command line: the old 17-char binary name exceeded procps'
/// 15-char comm cap, so a bare `pkill` on it matched nothing. The `--daemon`
/// fragment also keeps us from signalling the short-lived one-shot invocation
/// that triggered the edit (it has no SIGHUP handler).
///
/// Both names are matched: a daemon started before the rename, or from a
/// systemd unit still naming the old path, is the same process to reload.
pub fn signal_daemon_reload() {
    let _ = Command::new("pkill")
        .arg("-HUP")
        .arg("-f")
        .arg(r"tokengauge(-waybar)? --daemon")
        .status();
}

/// Enable/disable an OAuth provider (codex, claude) in the config file.
pub fn config_set_oauth_provider(path: &Path, name: &str, enabled: bool) -> Result<()> {
    if !PROVIDERS.contains(&name) {
        return Err(anyhow!(
            "unknown provider '{name}' (expected one of: {})",
            PROVIDERS.join(", ")
        ));
    }
    let name = name.to_string();
    edit_config_file(path, |doc| {
        let providers = ensure_table(doc, "providers");
        providers[&name] = toml_edit::value(enabled);
    })
}

/// Set (or clear, when `None`) the pinned `[waybar].primary` provider.
pub fn config_set_primary(path: &Path, primary: Option<&str>) -> Result<()> {
    let primary = primary.map(|s| s.to_string());
    edit_config_file(path, |doc| {
        let waybar = ensure_table(doc, "waybar");
        match &primary {
            Some(p) => waybar["primary"] = toml_edit::value(p.as_str()),
            None => {
                waybar.remove("primary");
            }
        }
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use crate::statefiles::tests::cache_test_dir;

    use super::*;

    #[test]
    fn retain_enabled_drops_disabled_providers_from_cache() {
        let payload = |name: &str| ProviderPayload {
            provider: name.into(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        // Cache written while codex was still enabled; config since toggled it off.
        let mut payloads = vec![payload("codex"), payload("Claude")];
        let mut errors = vec![
            ProviderFetchError::new("codex".into(), "boom"),
            ProviderFetchError::new("claude".into(), "429"),
        ];
        let providers = ProvidersConfig {
            codex: Some(false),
            claude: Some(true),
            ..Default::default()
        };

        retain_enabled(&mut payloads, &mut errors, &providers);

        // Disabled provider is gone from both lists; the enabled one survives
        // regardless of the case the cache happened to store it in.
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].provider, "Claude");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].provider, "claude");
    }

    fn write_test_cache(path: &Path, providers: &ProvidersConfig) {
        write_cache_full(path, &[], &[], &HashMap::new(), providers, None).expect("write cache");
    }

    #[test]
    fn cache_written_before_a_provider_was_enabled_does_not_cover_it() {
        let dir = cache_test_dir("cover");
        let cache = dir.join("tokengauge-usage.json");

        let claude_only = ProvidersConfig {
            codex: Some(false),
            claude: Some(true),
            ..Default::default()
        };
        write_test_cache(&cache, &claude_only);
        let cached = read_cache_full(&cache).expect("read cache");

        // Same set, and the subset left after switching one off: both answer.
        assert!(cached.covers(&claude_only));
        assert!(cached.covers(&ProvidersConfig {
            codex: Some(false),
            claude: Some(false),
            ..Default::default()
        }));
        // A provider switched on since the fetch has no row here and never
        // will, so the cache cannot answer for it.
        assert!(!cached.covers(&ProvidersConfig {
            codex: Some(true),
            claude: Some(true),
            ..Default::default()
        }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_without_meta_never_covers() {
        // Every snapshot written before 0.21, and the legacy array format.
        let unknown = CachedData::Full {
            sync: None,
            payloads: Vec::new(),
            errors: Vec::new(),
            costs: HashMap::new(),
            meta: None,
        };
        assert!(!unknown.covers(&ProvidersConfig::default()));
        assert!(!CachedData::Legacy(Vec::new()).covers(&ProvidersConfig::default()));
    }

    #[test]
    fn cache_records_the_writing_device_and_provider_set() {
        let dir = cache_test_dir("meta");
        let cache = dir.join("tokengauge-usage.json");

        write_test_cache(
            &cache,
            &ProvidersConfig {
                claude: Some(true),
                codex: Some(false),
                ..Default::default()
            },
        );
        let cached = read_cache_full(&cache).expect("read cache");
        let meta = cached.meta().expect("meta");

        assert_eq!(meta.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(meta.providers, vec!["claude".to_string()]);
        assert!(!meta.device.machine_id.is_empty());
        assert!(!meta.device.hostname.is_empty());
        assert!(meta.updated_at_ms > 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_cache_write_changes_the_revision() {
        let dir = cache_test_dir("revision");
        let cache = dir.join("tokengauge-usage.json");
        let providers = ProvidersConfig::default();

        assert_eq!(read_revision(&cache), "");
        write_test_cache(&cache, &providers);
        let first = read_revision(&cache);
        assert!(!first.is_empty());

        // Back to back, so the millisecond is likely to be the same one: a
        // frontend comparing contents still has to see a change.
        write_test_cache(&cache, &providers);
        assert_ne!(read_revision(&cache), first);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_stale_fallback_serves_last_good_and_keeps_uncovered_errors() {
        let good_claude = ProviderPayload {
            provider: "claude".into(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let previous = vec![good_claude];

        let mut payloads: Vec<ProviderPayload> = Vec::new();
        let mut errors = vec![
            ProviderFetchError::new("claude".into(), "429"),
            ProviderFetchError::new("codex".into(), "boom"),
        ];

        apply_stale_fallback(&mut payloads, &mut errors, &previous);

        // claude had a cached good payload -> served stale, error dropped.
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].provider, "claude");
        assert!(payloads[0].stale);
        // codex had no fallback -> error retained.
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].provider, "codex");
    }

    #[test]
    fn apply_stale_fallback_skips_providers_with_a_live_payload() {
        let cached = ProviderPayload {
            provider: "claude".into(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let previous = vec![cached];

        // claude already has a live payload this round plus an error for a
        // sibling sub-payload; a stale clone must not be added (no dup row).
        let mut payloads = vec![ProviderPayload {
            provider: "claude".into(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        }];
        let mut errors = vec![
            ProviderFetchError::new("claude".into(), "429"),
            ProviderFetchError::new("claude".into(), "429 again"),
        ];

        apply_stale_fallback(&mut payloads, &mut errors, &previous);

        assert_eq!(payloads.len(), 1, "no duplicate stale row: {payloads:?}");
        assert!(!payloads[0].stale);
        assert!(
            errors.is_empty(),
            "errors covered by live payload: {errors:?}"
        );
    }

    #[test]
    fn apply_stale_fallback_restores_all_cached_payloads_for_a_failed_provider() {
        // Provider with two cached payloads (e.g. two accounts/windows).
        let previous = vec![
            ProviderPayload {
                provider: "claude".into(),
                version: None,
                source: Some("oauth".into()),
                usage: None,
                credits: None,
                error: None,
                stale: false,
            },
            ProviderPayload {
                provider: "claude".into(),
                version: None,
                source: Some("cli".into()),
                usage: None,
                credits: None,
                error: None,
                stale: false,
            },
        ];

        // Full outage this round: no live payloads, one error for the provider.
        let mut payloads: Vec<ProviderPayload> = Vec::new();
        let mut errors = vec![ProviderFetchError::new("claude".into(), "timeout")];

        apply_stale_fallback(&mut payloads, &mut errors, &previous);

        assert_eq!(payloads.len(), 2, "both cached rows restored: {payloads:?}");
        assert!(payloads.iter().all(|p| p.stale));
        assert!(errors.is_empty());
    }

    #[test]
    fn config_edits_preserve_comments_and_toggle_values() {
        let dir = std::env::temp_dir().join(format!("tg-cfgtest-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        fs::write(
            &path,
            "# my config\n[providers]\n# oauth\ncodex = true\nclaude = true\n\n[waybar]\nwindow = \"daily\"\n",
        )
        .unwrap();

        config_set_oauth_provider(&path, "claude", false).unwrap();
        config_set_primary(&path, Some("codex")).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("# my config"), "top comment lost: {out}");
        assert!(out.contains("# oauth"), "section comment lost: {out}");
        assert!(out.contains("claude = false"), "toggle not applied: {out}");
        assert!(
            out.contains("codex = true"),
            "other provider changed: {out}"
        );
        assert!(
            out.contains("primary = \"codex\""),
            "primary not set: {out}"
        );

        // Clearing primary removes the key, keeps the rest.
        config_set_primary(&path, None).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert!(!out.contains("primary ="), "primary not cleared: {out}");
        assert!(out.contains("window = \"daily\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_edit_preserves_inline_provider_table() {
        let dir = std::env::temp_dir().join(format!("tg-cfginline-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        fs::write(&path, "providers = { codex = true, claude = true }\n").unwrap();

        config_set_oauth_provider(&path, "claude", false).unwrap();

        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("codex = true"), "codex wiped: {out}");
        assert!(out.contains("claude = false"), "claude not toggled: {out}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_edit_creates_default_when_missing() {
        let dir = std::env::temp_dir().join(format!("tg-cfgtest2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        assert!(!path.exists());

        config_set_oauth_provider(&path, "codex", false).unwrap();
        assert!(path.exists());
        let out = fs::read_to_string(&path).unwrap();
        assert!(out.contains("codex = false"), "{out}");

        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------------
    // format_window tests
    // ------------------------------------------------------------------------

    #[test]
    fn format_window_with_resets_at() {
        // Use a time 2 hours and 30 minutes in the future
        let future = Utc::now() + chrono::Duration::hours(2) + chrono::Duration::minutes(30);
        let window = UsageWindow {
            used_percent: Some(42),
            reset_description: Some("Jan 20 at 12:59PM".to_string()),
            resets_at: Some(future.to_rfc3339()),
            window_minutes: Some(300),
        };
        let (used, minutes, reset) = format_window(Some(window));
        assert_eq!(used, Some(42));
        assert_eq!(minutes, Some(300));
        // Allow for slight timing variations (29-30m)
        assert!(
            reset.starts_with("in 2h 2") || reset.starts_with("in 2h 30"),
            "unexpected reset: {}",
            reset
        );
    }

    #[test]
    fn format_window_with_days() {
        let future = Utc::now()
            + chrono::Duration::days(3)
            + chrono::Duration::hours(16)
            + chrono::Duration::minutes(41);
        let window = UsageWindow {
            used_percent: Some(5),
            reset_description: Some("ignored".to_string()),
            resets_at: Some(future.to_rfc3339()),
            window_minutes: Some(10080),
        };
        let (_, _, reset) = format_window(Some(window));
        assert!(
            reset.starts_with("in 3d 16h 4"),
            "unexpected reset: {reset}"
        );
    }

    #[test]
    fn format_window_falls_back_to_description() {
        // When resets_at is missing, fall back to description
        let window = UsageWindow {
            used_percent: Some(42),
            reset_description: Some("Jan 20 at 12:59PM".to_string()),
            resets_at: None,
            window_minutes: Some(300),
        };
        let (used, minutes, reset) = format_window(Some(window));
        assert_eq!(used, Some(42));
        assert_eq!(minutes, Some(300));
        assert_eq!(reset, "Jan 20 at 12:59PM");
    }

    #[test]
    fn format_window_clamps_over_100() {
        let window = UsageWindow {
            used_percent: Some(150),
            reset_description: None,
            resets_at: None,
            window_minutes: None,
        };
        let (used, _, _) = format_window(Some(window));
        assert_eq!(used, Some(100)); // clamped to 100
    }

    #[test]
    fn format_window_none() {
        let (used, minutes, reset) = format_window(None);
        assert_eq!(used, None);
        assert_eq!(minutes, None);
        assert_eq!(reset, "—");
    }

    #[test]
    fn format_window_missing_both_resets_at_and_description() {
        let window = UsageWindow {
            used_percent: Some(50),
            reset_description: None,
            resets_at: None,
            window_minutes: Some(60),
        };
        let (_, _, reset) = format_window(Some(window));
        assert_eq!(reset, "—");
    }

    #[test]
    fn format_window_minutes_only() {
        // Use a time 45 minutes in the future
        let future = Utc::now() + chrono::Duration::minutes(45);
        let window = UsageWindow {
            used_percent: Some(10),
            reset_description: None,
            resets_at: Some(future.to_rfc3339()),
            window_minutes: Some(60),
        };
        let (_, _, reset) = format_window(Some(window));
        // Allow for slight timing variations (44-45m)
        assert!(
            reset == "in 44m" || reset == "in 45m",
            "unexpected reset: {}",
            reset
        );
    }

    // ------------------------------------------------------------------------
    // format_updated tests
    // ------------------------------------------------------------------------

    #[test]
    fn format_updated_rfc3339() {
        // Full RFC3339 timestamp should be formatted to local time HH:MM
        let result = format_updated(Some("2026-01-20T07:37:16Z".to_string()));
        // We can't assert exact time due to timezone, but it should be HH:MM format
        assert!(result.len() == 5 || result.len() <= 8); // "HH:MM" or with timezone offset
        assert!(result.contains(':'));
    }

    #[test]
    fn format_updated_iso_with_t() {
        // ISO format with T separator, extracts time part
        let result = format_updated(Some("2026-01-20T14:30:00Z".to_string()));
        assert!(result.contains(':'));
    }

    #[test]
    fn format_updated_none() {
        assert_eq!(format_updated(None), "—");
    }

    #[test]
    fn format_updated_fallback() {
        // Unknown format returns as-is
        let result = format_updated(Some("unknown format".to_string()));
        assert_eq!(result, "unknown format");
    }

    // ------------------------------------------------------------------------
    // provider_label tests
    // ------------------------------------------------------------------------

    #[test]
    fn provider_label_known_providers() {
        assert_eq!(provider_label("claude"), "Claude");
        assert_eq!(provider_label("codex"), "Codex");
    }

    #[test]
    fn provider_label_unknown_returns_input() {
        assert_eq!(provider_label("unknown_provider"), "unknown_provider");
    }

    // ------------------------------------------------------------------------
    // ProvidersConfig tests
    // ------------------------------------------------------------------------

    #[test]
    fn providers_config_enabled_oauth_only() {
        let config = ProvidersConfig {
            codex: Some(true),
            claude: Some(true),
            ..Default::default()
        };
        let enabled = config.enabled_providers();
        assert_eq!(enabled.len(), 2);
        assert!(enabled.contains(&"codex"));
        assert!(enabled.contains(&"claude"));
    }

    #[test]
    fn providers_config_disabled_oauth() {
        let config = ProvidersConfig {
            codex: Some(false),
            claude: Some(true),
            ..Default::default()
        };
        let enabled = config.enabled_providers();
        assert_eq!(enabled, vec!["claude"]);
    }

    #[test]
    fn providers_config_none_means_disabled() {
        let config = ProvidersConfig::default();
        let enabled = config.enabled_providers();
        assert!(enabled.is_empty());
    }

    #[test]
    fn providers_config_is_enabled() {
        let config = ProvidersConfig {
            codex: Some(true),
            claude: Some(false),
            ..Default::default()
        };
        assert!(config.is_enabled("codex"));
        assert!(!config.is_enabled("claude"));
        assert!(!config.is_enabled("kimik2"));
        assert!(!config.is_enabled("unknown"));
    }

    // ------------------------------------------------------------------------
    // ProviderPayload tests
    // ------------------------------------------------------------------------

    #[test]
    fn provider_payload_has_error_true() {
        let payload = ProviderPayload {
            provider: "test".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: Some(ProviderError {
                message: Some("error".to_string()),
                code: None,
                kind: None,
            }),
            stale: false,
        };
        assert!(payload.has_error());
    }

    #[test]
    fn provider_payload_has_error_false() {
        let payload = ProviderPayload {
            provider: "test".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        assert!(!payload.has_error());
    }

    // ------------------------------------------------------------------------
    // CachedData tests
    // ------------------------------------------------------------------------

    #[test]
    fn cached_data_full_format() {
        let payload = ProviderPayload {
            provider: "claude".to_string(),
            version: Some("2.0".to_string()),
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let error = ProviderFetchError {
            provider: "codex".to_string(),
            message: "timeout".to_string(),
            raw: "raw error".to_string(),
        };
        let cached = CachedData::Full {
            sync: None,
            payloads: vec![payload.clone()],
            errors: vec![error.clone()],
            costs: HashMap::new(),
            meta: None,
        };

        assert_eq!(cached.payloads().len(), 1);
        assert_eq!(cached.errors().len(), 1);

        let (payloads, errors, costs) = cached.into_parts();
        assert_eq!(payloads.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(costs.is_empty());
    }

    #[test]
    fn cached_data_legacy_format() {
        let payload = ProviderPayload {
            provider: "claude".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let cached = CachedData::Legacy(vec![payload]);

        assert_eq!(cached.payloads().len(), 1);
        assert_eq!(cached.errors().len(), 0); // legacy has no errors

        let (payloads, errors, costs) = cached.into_parts();
        assert_eq!(payloads.len(), 1);
        assert!(errors.is_empty());
        assert!(costs.is_empty());
    }

    // ------------------------------------------------------------------------
    // Error message cleaning tests
    // ------------------------------------------------------------------------

    /// The shape `{e:#}` actually produces for a reqwest timeout - the whole
    /// anyhow chain, ending in the transport's own words.
    #[test]
    fn a_transport_timeout_is_said_in_one_line() {
        let raw = "Codex usage request failed: error sending request: operation timed out";
        let error = ProviderFetchError::new("codex".to_string(), raw);
        assert_eq!(error.message, "Request timed out");
        assert_eq!(error.raw, raw);
    }

    /// A provider error body that mentions the word is not a timeout. Matching
    /// a bare "timeout" turned advice about raising one into a report that the
    /// request had timed out.
    #[test]
    fn an_error_body_about_timeouts_is_not_rewritten_as_one() {
        let error =
            ProviderFetchError::new("glm".to_string(), "z.ai error: raise your request timeout");
        assert_eq!(error.message, "z.ai error: raise your request timeout");
    }

    #[test]
    fn provider_fetch_error_short_message_unchanged() {
        let error = ProviderFetchError::new("test".to_string(), "Short error");
        assert_eq!(error.message, "Short error");
    }

    #[test]
    fn provider_fetch_error_long_message_truncated() {
        let long_msg = "a".repeat(100);
        let error = ProviderFetchError::new("test".to_string(), &long_msg);
        assert!(error.message.chars().count() <= 60);
        assert!(error.message.ends_with("..."));
    }

    #[test]
    fn provider_fetch_error_multibyte_truncation_does_not_panic() {
        // A long body with a multi-byte char straddling byte 57 must not panic.
        let raw = "é".repeat(100);
        let error = ProviderFetchError::new("claude".to_string(), &raw);
        assert!(error.message.ends_with("..."));
    }

    // ------------------------------------------------------------------------
    // JSON parsing tests
    // ------------------------------------------------------------------------

    #[test]
    fn a_fetchers_payload_deserialises_every_window_field() {
        let json = r#"{
            "provider": "claude",
            "version": "2.1.12",
            "source": "oauth",
            "usage": {
                "primary": {
                    "usedPercent": 19,
                    "resetDescription": "Jan 20 at 12:59PM",
                    "resetsAt": "2026-01-20T12:59:00Z",
                    "windowMinutes": 300
                },
                "secondary": {
                    "usedPercent": 12,
                    "resetDescription": "Jan 26 at 8:59AM",
                    "resetsAt": "2026-01-26T08:59:00Z",
                    "windowMinutes": 10080
                },
                "updatedAt": "2026-01-20T07:37:16Z"
            },
            "credits": null,
            "error": null
        }"#;
        let payload: ProviderPayload = serde_json::from_slice(json.as_bytes()).unwrap();
        assert_eq!(payload.provider, "claude");
        assert!(!payload.has_error());

        let usage = payload.usage.as_ref().unwrap();
        let primary = usage.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, Some(19));
        assert_eq!(primary.window_minutes, Some(300));
    }

    // ------------------------------------------------------------------------
    // payload_to_rows_with_costs tests
    // ------------------------------------------------------------------------

    fn rows_of(payloads: Vec<ProviderPayload>) -> Vec<ProviderRow> {
        payload_to_rows_with_costs(payloads, &HashMap::new())
    }

    #[test]
    fn payload_to_rows_filters_errors() {
        let good = ProviderPayload {
            provider: "claude".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let bad = ProviderPayload {
            provider: "codex".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: Some(ProviderError {
                message: Some("error".to_string()),
                code: None,
                kind: None,
            }),
            stale: false,
        };
        let rows = rows_of(vec![good, bad]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "Claude");
    }

    #[test]
    fn payload_to_rows_formats_credits() {
        let payload = ProviderPayload {
            provider: "zai".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: Some(Credits {
                remaining: Some(42.567),
            }),
            error: None,
            stale: false,
        };
        let rows = rows_of(vec![payload]);
        assert_eq!(rows[0].credits, "42.57"); // 2 decimal places
    }

    #[test]
    fn payload_to_rows_formats_source() {
        // Both version and source
        let payload1 = ProviderPayload {
            provider: "claude".to_string(),
            version: Some("2.1.12".to_string()),
            source: Some("oauth".to_string()),
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let rows = rows_of(vec![payload1]);
        assert_eq!(rows[0].source, "2.1.12 (oauth)");

        // Only version
        let payload2 = ProviderPayload {
            provider: "claude".to_string(),
            version: Some("2.1.12".to_string()),
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let rows = rows_of(vec![payload2]);
        assert_eq!(rows[0].source, "2.1.12");

        // Only source
        let payload3 = ProviderPayload {
            provider: "claude".to_string(),
            version: None,
            source: Some("oauth".to_string()),
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let rows = rows_of(vec![payload3]);
        assert_eq!(rows[0].source, "oauth");

        // Neither
        let payload4 = ProviderPayload {
            provider: "claude".to_string(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let rows = rows_of(vec![payload4]);
        assert_eq!(rows[0].source, "—");
    }

    // ------------------------------------------------------------------------
    // WaybarConfig tests
    // ------------------------------------------------------------------------

    #[test]
    fn waybar_config_default() {
        let config = WaybarConfig::default();
        assert_eq!(config.window, WaybarWindow::Daily);
        assert_eq!(config.placement, WaybarPlacement::Right);
    }

    #[test]
    fn tokengauge_config_default() {
        let config = TokenGaugeConfig::default();
        assert_eq!(config.refresh_secs, 600);
        assert!(config.providers.codex.unwrap_or(false));
        assert!(config.providers.claude.unwrap_or(false));
    }

    #[test]
    fn unknown_config_keys_flags_removed_providers_and_keys() {
        let config: TokenGaugeConfig = toml::from_str(
            "codexbar_bin = \"codexbar\"\n[providers]\nclaude = true\n\n[providers.zai]\napi_key = \"x\"\n\n[waybar]\npopover_command = \"tokengauge-popover --toggle\"\n",
        )
        .expect("legacy config still parses");
        // Including the options of the popover 0.20.0 removed: serde would drop
        // a stale `[waybar]` key in silence, leaving the line in the file with
        // nothing to say it does nothing.
        assert_eq!(
            config.unknown_config_keys(),
            vec![
                "codexbar_bin".to_string(),
                "providers.zai".to_string(),
                "waybar.popover_command".to_string()
            ]
        );
        // Parsing does not fail - the daemon keeps running on an old config.
        assert!(config.providers.claude.unwrap_or(false));
    }

    #[test]
    fn waybar_config_default_placement_is_right() {
        assert_eq!(WaybarPlacement::default(), WaybarPlacement::Right);
    }

    #[test]
    fn waybar_placement_deserializes_lowercase() {
        let left: WaybarConfig =
            toml::from_str(r#"placement = "left""#).expect("parse left placement");
        assert_eq!(left.placement, WaybarPlacement::Left);

        let right: WaybarConfig =
            toml::from_str(r#"placement = "right""#).expect("parse right placement");
        assert_eq!(right.placement, WaybarPlacement::Right);
    }

    #[test]
    fn waybar_config_missing_placement_field_defaults_to_right() {
        let config: WaybarConfig =
            toml::from_str(r#"window = "daily""#).expect("parse partial waybar config");
        assert_eq!(config.window, WaybarWindow::Daily);
        assert_eq!(config.placement, WaybarPlacement::Right);
        assert_eq!(config.primary, None);
    }

    #[test]
    fn waybar_config_primary_round_trips() {
        let config: WaybarConfig = toml::from_str(r#"primary = "claude""#).expect("parse primary");
        assert_eq!(config.primary.as_deref(), Some("claude"));
    }

    #[test]
    fn format_tokens_units() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(2_300_000), "2.3M");
        assert_eq!(format_tokens(4_500_000_000), "4.5B");
    }

    #[test]
    fn provider_icon_known_and_default() {
        assert_eq!(provider_icon("Claude").glyph, "\u{f0721}");
        assert_eq!(provider_icon("claude").color_hex, "#DE7356");
        assert_eq!(provider_icon("Codex").glyph, "\u{f0b2b}");
        assert_eq!(provider_icon("Unknown").glyph, "\u{f06a9}");
    }

    #[test]
    fn sparkline_basic_ramp() {
        assert_eq!(
            sparkline(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0])
                .chars()
                .count(),
            8
        );
        assert_eq!(sparkline(&[0.0, 7.0]), "▁█");
        assert_eq!(sparkline(&[3.5, 7.0]), "▅█");
    }

    #[test]
    fn sparkline_all_zero() {
        assert_eq!(sparkline(&[0.0, 0.0, 0.0]), "▁▁▁");
    }

    #[test]
    fn sparkline_empty() {
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn lookup_cost_exact_lowercase() {
        let mut costs = HashMap::new();
        costs.insert(
            "claude".to_string(),
            CostInfo {
                today_usd: 1.0,
                today_tokens: 100,
                monthly_usd: 10.0,
                monthly_tokens: 1000,
                today_models: Vec::new(),
                monthly_models: Vec::new(),
                burn_rate: None,
                session_usd: 0.0,
                weekly_usd: 0.0,
                weekly_cost_history: Vec::new(),
                weekly_history: Vec::new(),
                by_device: Vec::new(),
                sync_note: None,
            },
        );
        assert!(lookup_cost("Claude", &costs).is_some());
        assert!(lookup_cost("claude-code", &costs).is_some());
        assert!(lookup_cost("CLAUDE", &costs).is_some());
        assert!(lookup_cost("zai", &costs).is_none());
    }

    /// Two providers sharing a prefix used to answer for each other, and which
    /// one won depended on HashMap order - so the same snapshot could put the
    /// money on a different row from one run to the next.
    #[test]
    fn a_provider_never_answers_for_one_whose_name_merely_starts_the_same() {
        let cost = |usd: f64| CostInfo {
            today_usd: usd,
            ..CostInfo::default()
        };
        let mut costs = HashMap::new();
        costs.insert("claude".to_string(), cost(1.0));
        costs.insert("claudex".to_string(), cost(2.0));

        // Exact wins outright, either way round.
        assert_eq!(lookup_cost("claude", &costs).unwrap().today_usd, 1.0);
        assert_eq!(lookup_cost("claudex", &costs).unwrap().today_usd, 2.0);
        // And a longer spelling only matches across a separator.
        assert_eq!(lookup_cost("claude-code", &costs).unwrap().today_usd, 1.0);
        assert!(lookup_cost("claudexyz", &costs).is_none());
    }

    #[test]
    fn today_vs_avg_excludes_today_from_the_baseline() {
        let cost = CostInfo {
            today_usd: 20.0,
            today_tokens: 0,
            monthly_usd: 0.0,
            monthly_tokens: 0,
            today_models: Vec::new(),
            monthly_models: Vec::new(),
            burn_rate: None,
            session_usd: 0.0,
            weekly_usd: 0.0,
            // Three prior days at $10 plus today's partial entry.
            weekly_cost_history: vec![10.0, 10.0, 10.0, 20.0],
            weekly_history: Vec::new(),
            by_device: Vec::new(),
            sync_note: None,
        };
        assert_eq!(cost.avg_daily_cost(), Some(10.0));
        assert_eq!(cost.today_vs_avg_percent(), Some(100.0));

        let single_day = CostInfo {
            weekly_cost_history: vec![20.0],
            ..cost
        };
        assert_eq!(single_day.today_vs_avg_percent(), None);
    }

    #[test]
    fn cost_source_parses_every_spelling() {
        for (text, expected) in [
            ("auto", CostSource::Auto),
            ("native", CostSource::Native),
            ("ccusage", CostSource::Ccusage),
        ] {
            let cfg: TokenGaugeConfig =
                toml::from_str(&format!("cost_source = \"{text}\"\n")).expect("parses");
            assert_eq!(cfg.cost_source, expected, "{text}");
        }
        // Absent means auto, which is what an existing config has.
        let cfg: TokenGaugeConfig = toml::from_str("refresh_secs = 600\n").expect("parses");
        assert_eq!(cfg.cost_source, CostSource::Auto);
        assert!(toml::from_str::<TokenGaugeConfig>("cost_source = \"nope\"\n").is_err());
    }

    fn diagnostics(native: &[(&str, u64)], ccusage: &[(&str, u64)]) -> CostDiagnostics {
        CostDiagnostics {
            native: native
                .iter()
                .map(|(p, t)| (p.to_string(), (*t, 0.0)))
                .collect(),
            ccusage: Some(
                ccusage
                    .iter()
                    .map(|(p, t)| (p.to_string(), (*t, 0.0)))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn drift_is_measured_where_both_sources_report() {
        let d = diagnostics(
            &[("claude", 100), ("codex", 50)],
            &[("claude", 100), ("codex", 55)],
        );
        let (provider, drift) = d.worst_token_drift().expect("drift");
        assert_eq!(provider, "codex");
        assert!((drift - 5.0 / 55.0).abs() < 1e-9);
    }

    #[test]
    fn a_reader_that_stopped_producing_anything_is_full_drift() {
        // The failure the check exists for: a format change and the Claude
        // reader returns nothing. Comparing only the keys it produced would
        // call that agreement, because there is no `claude` key left to compare.
        let d = diagnostics(&[("codex", 50)], &[("claude", 1_000_000), ("codex", 50)]);
        let (provider, drift) = d.worst_token_drift().expect("drift");
        assert_eq!(provider, "claude");
        assert_eq!(drift, 1.0);
    }

    #[test]
    fn a_provider_only_ccusage_can_see_is_not_drift() {
        // Kimi driven from its own CLI writes into neither tree we parse; the
        // `auto` fallback is what covers it, so its absence is not a fault.
        let d = diagnostics(&[("claude", 100)], &[("claude", 100), ("kimi", 900)]);
        let (provider, drift) = d.worst_token_drift().expect("claude is comparable");
        assert_eq!(provider, "claude");
        assert_eq!(drift, 0.0, "kimi being ccusage-only must not read as drift");

        // And with nothing comparable at all, there is no verdict to give.
        let empty = diagnostics(&[], &[("kimi", 900)]);
        assert!(empty.worst_token_drift().is_none());
    }

    #[test]
    fn auto_asks_ccusage_only_about_providers_the_readers_missed() {
        let mut report = NativeCostReport {
            events: 100,
            ..Default::default()
        };
        report.costs.insert("claude".into(), zero_cost());
        report.costs.insert("codex".into(), zero_cost());

        // A Claude/Codex machine never spawns the subprocess.
        assert!(missing_providers(&report, &["claude", "codex"]).is_empty());

        // Kimi enabled with nothing in either transcript tree: its own CLI
        // writes elsewhere, so ccusage still has to be asked about it.
        let missing = missing_providers(&report, &["claude", "codex", "kimi"]);
        assert_eq!(missing.len(), 1);
        assert!(missing.contains("kimi"));
    }

    #[test]
    fn a_provider_with_native_spend_is_never_taken_from_ccusage() {
        let mut report = NativeCostReport {
            events: 1,
            ..Default::default()
        };
        report.costs.insert("glm".into(), zero_cost());
        // GLM driven through Claude Code lands in the native read; asking
        // ccusage as well would overwrite it with a coarser answer.
        assert!(missing_providers(&report, &["GLM"]).is_empty());
    }

    fn zero_cost() -> CostInfo {
        CostInfo {
            today_usd: 0.0,
            today_tokens: 0,
            monthly_usd: 0.0,
            monthly_tokens: 0,
            today_models: Vec::new(),
            monthly_models: Vec::new(),
            burn_rate: None,
            session_usd: 0.0,
            weekly_usd: 0.0,
            weekly_cost_history: Vec::new(),
            weekly_history: Vec::new(),
            by_device: Vec::new(),
            sync_note: None,
        }
    }

    #[test]
    fn provider_cli_names() {
        assert_eq!(provider_cli_name("kimi"), Some("kimi"));
        assert_eq!(provider_cli_name("grok"), Some("grok"));
        assert_eq!(provider_cli_name("claude"), Some("claude"));
        // GLM authenticates with an API key - no CLI.
        assert_eq!(provider_cli_name("glm"), None);
        assert_eq!(provider_cli_name("nope"), None);
    }

    #[test]
    fn provider_auth_status_covers_all_providers() {
        // Never panics and always yields a hint when not satisfied.
        for provider in PROVIDERS {
            let status = provider_auth_status(provider);
            if !status.ok {
                assert!(!status.hint.is_empty(), "{provider} missing hint");
            }
        }
    }
}
