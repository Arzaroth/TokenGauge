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
use sha2::{Digest, Sha256};

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

/// The providers TokenGauge fetches natively, both OAuth.
pub const PROVIDERS: &[&str] = &["codex", "claude", "kimi", "grok", "glm"];

/// Get the display label for a provider.
pub fn provider_label(name: &str) -> &str {
    match name {
        "codex" => "Codex",
        "claude" => "Claude",
        "kimi" => "Kimi",
        "grok" => "Grok",
        "glm" => "GLM",
        other => other,
    }
}

// ============================================================================
// Native fetcher helpers (shared by the claude/codex modules)
// ============================================================================

mod claude;
mod codex;
pub mod cost;
mod glm;
mod grok;
mod kimi;
pub mod launch;
pub mod pace;
pub mod panel;
pub mod sync;

pub use cost::{CostSource, NativeCostReport};
pub use pace::{PaceStage, UsagePace};
pub use panel::{PanelRow, Section, SectionKind, Tone, panel_spec};

/// Round and clamp a float percentage into the `0..=100` byte range the render
/// layer expects. Mirrors the old `de_opt_percent` serde hook, now called from
/// the native fetchers instead of at deserialize time.
pub(crate) fn pct_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

/// Lowercase, collapse each run of non-alphanumeric characters to a single `-`,
/// and trim leading/trailing `-`. Used for stable extra-window ids.
pub(crate) fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// A blocking HTTP client with the per-request timeout wired to the config's
/// `timeout_secs` (the subprocess-kill timeout is gone with codexbar).
pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client")
}

/// Path to the Claude OAuth credentials file the native fetcher reads.
pub fn claude_credentials_path() -> PathBuf {
    claude::credentials_path()
}

/// Path to the Codex auth file the native fetcher reads (honors `CODEX_HOME`).
pub fn codex_auth_path() -> PathBuf {
    codex::auth_path()
}

/// Path to the Kimi Code CLI credential file the native fetcher reads (honors
/// `KIMI_CODE_HOME`).
pub fn kimi_credentials_path() -> PathBuf {
    kimi::credentials_path()
}

/// Path to the Grok CLI auth file the native fetcher reads (honors `GROK_HOME`).
pub fn grok_auth_path() -> PathBuf {
    grok::auth_path()
}

/// The CLI a provider's credentials come from, if any. `None` means the
/// provider authenticates with an API key / env var and needs no CLI.
pub fn provider_cli_name(provider: &str) -> Option<&'static str> {
    Some(match provider.to_lowercase().as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "kimi" => "kimi",
        "grok" => "grok",
        _ => return None,
    })
}

/// Whether a provider's credentials are currently available, and where from.
pub struct AuthStatus {
    /// At least one accepted auth source is present.
    pub ok: bool,
    /// What was found (or what is missing).
    pub detail: String,
    /// How to satisfy it when missing (empty when `ok`).
    pub hint: &'static str,
}

fn env_var_present(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
}

fn file_auth_status(path: PathBuf, hint: &'static str) -> AuthStatus {
    if path.exists() {
        AuthStatus {
            ok: true,
            detail: path.display().to_string(),
            hint: "",
        }
    } else {
        AuthStatus {
            ok: false,
            detail: format!("{} not found", path.display()),
            hint,
        }
    }
}

/// Report a provider's credential presence without doing a network fetch.
/// Mirrors the auth sources each native fetcher actually reads.
pub fn provider_auth_status(provider: &str) -> AuthStatus {
    match provider.to_lowercase().as_str() {
        "claude" => file_auth_status(claude_credentials_path(), "run `claude` to sign in"),
        "codex" => file_auth_status(codex_auth_path(), "run `codex` to sign in"),
        "grok" => match grok::credentials_valid(Utc::now()) {
            Ok(()) => AuthStatus {
                ok: true,
                detail: grok_auth_path().display().to_string(),
                hint: "",
            },
            Err(err) => AuthStatus {
                ok: false,
                detail: err.to_string(),
                hint: "run `grok login` to sign in",
            },
        },
        "kimi" => {
            let path = kimi_credentials_path();
            // Mirror kimi::resolve_auth, which prefers KIMI_CODE_API_KEY over the
            // CLI file and validates the file (parse + freshness) when used.
            if env_var_present("KIMI_CODE_API_KEY") {
                AuthStatus {
                    ok: true,
                    detail: "KIMI_CODE_API_KEY set".to_string(),
                    hint: "",
                }
            } else {
                match kimi::credentials_valid() {
                    Ok(()) => AuthStatus {
                        ok: true,
                        detail: format!("{} (kimi CLI)", path.display()),
                        hint: "",
                    },
                    Err(err) => AuthStatus {
                        ok: false,
                        detail: err.to_string(),
                        hint: "sign in with `kimi` or set KIMI_CODE_API_KEY",
                    },
                }
            }
        }
        "glm" => {
            if let Some(var) = ["Z_AI_API_KEY", "ZAI_API_TOKEN"]
                .into_iter()
                .find(|v| env_var_present(v))
            {
                AuthStatus {
                    ok: true,
                    detail: format!("{var} set"),
                    hint: "",
                }
            } else {
                AuthStatus {
                    ok: false,
                    detail: "Z_AI_API_KEY unset".to_string(),
                    hint: "set Z_AI_API_KEY (legacy ZAI_API_TOKEN also works)",
                }
            }
        }
        other => AuthStatus {
            ok: false,
            detail: format!("unknown provider {other}"),
            hint: "",
        },
    }
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

impl ProvidersConfig {
    /// Get list of all enabled provider names.
    pub fn enabled_providers(&self) -> Vec<&'static str> {
        let mut enabled = Vec::new();
        if self.codex.unwrap_or(false) {
            enabled.push("codex");
        }
        if self.claude.unwrap_or(false) {
            enabled.push("claude");
        }
        if self.kimi.unwrap_or(false) {
            enabled.push("kimi");
        }
        if self.grok.unwrap_or(false) {
            enabled.push("grok");
        }
        if self.glm.unwrap_or(false) {
            enabled.push("glm");
        }
        enabled
    }

    /// Check if a provider is enabled (used for filtering payloads).
    pub fn is_enabled(&self, provider: &str) -> bool {
        match provider {
            "codex" => self.codex.unwrap_or(false),
            "claude" => self.claude.unwrap_or(false),
            "kimi" => self.kimi.unwrap_or(false),
            "grok" => self.grok.unwrap_or(false),
            "glm" => self.glm.unwrap_or(false),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct WaybarConfig {
    pub window: WaybarWindow,
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

/// Which providers take part in fleet sync. The default is every enabled
/// provider that *can*: a provider read through ccusage has a `CostInfo` and no
/// usage events under it, so there is nothing to bucket. Turn one off when its
/// transcript tree is itself synced between machines, or both machines will
/// count it.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncProvidersConfig {
    pub claude: Option<bool>,
    pub codex: Option<bool>,
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl SyncProvidersConfig {
    pub fn resolve(&self, enabled: &[&str]) -> Vec<String> {
        enabled
            .iter()
            .filter(|name| sync::syncable(name))
            .filter(|name| match name.to_ascii_lowercase().as_str() {
                "claude" => self.claude.unwrap_or(true),
                "codex" => self.codex.unwrap_or(true),
                _ => false,
            })
            .map(|name| name.to_lowercase())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncTransportKind {
    /// A folder the user already syncs: Syncthing, Dropbox, Nextcloud, a NAS.
    #[default]
    Dir,
    /// Any S3-compatible bucket: S3, R2, B2, MinIO, Garage.
    S3,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncDirConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncS3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    /// Credentials belong in the environment; these exist for a machine where
    /// that is awkward. They are never written into the snapshot or logged.
    pub access_key_id: String,
    pub secret_access_key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SyncConfig {
    pub enabled: bool,
    pub transport: SyncTransportKind,
    /// This machine's name in the by-device rows. Empty falls back to the
    /// hostname.
    pub label: String,
    /// Days of buckets a contribution carries. The local store keeps far more,
    /// because it is the only record left once a CLI rotates a transcript away.
    pub retention_days: u32,
    /// A device silent this long is reported as quiet by `--sync-status` and
    /// `--doctor`. It does not stop counting: its past days really did happen,
    /// and a machine with no tokens in the period shown is already absent from
    /// the by-device rows without needing a rule.
    pub peer_max_age_days: u32,
    pub providers: SyncProvidersConfig,
    pub dir: SyncDirConfig,
    pub s3: SyncS3Config,
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: SyncTransportKind::default(),
            label: String::new(),
            retention_days: sync::WIRE_RETENTION_DAYS as u32,
            peer_max_age_days: 30,
            providers: SyncProvidersConfig::default(),
            dir: SyncDirConfig::default(),
            s3: SyncS3Config::default(),
            unknown: HashMap::new(),
        }
    }
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Preset to start from: "catppuccin" (default), "nord", "gruvbox".
    /// Individual hex fields below override the preset's values.
    pub preset: String,
    pub dim: Option<String>,
    pub separator: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub neutral: Option<String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: "catppuccin".into(),
            dim: None,
            separator: None,
            green: None,
            yellow: None,
            red: None,
            neutral: None,
        }
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
    if raw.contains("timeout") {
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

/// Which machine wrote a snapshot. Recorded next to the payloads so snapshots
/// collected from several machines can be told apart and reconciled later;
/// nothing merges them yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub machine_id: String,
    pub hostname: String,
}

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Fetch a single provider's usage natively over HTTP.
pub fn fetch_single_provider(provider: &str, timeout: Duration) -> Result<Vec<ProviderPayload>> {
    match provider {
        "claude" => claude::fetch(timeout),
        "codex" => codex::fetch(timeout),
        "kimi" => kimi::fetch(timeout),
        "grok" => grok::fetch(timeout),
        "glm" => glm::fetch(timeout),
        other => Err(anyhow!("unknown provider {other}")),
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

pub fn parse_payload(value: serde_json::Value) -> Result<Vec<ProviderPayload>> {
    if value.is_array() {
        serde_json::from_value(value).context("failed to parse provider payload list")
    } else {
        let payload: ProviderPayload =
            serde_json::from_value(value).context("failed to parse provider payload")?;
        Ok(vec![payload])
    }
}

pub fn parse_payload_bytes(bytes: &[u8]) -> Result<Vec<ProviderPayload>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("provider payload was not JSON")?;
    parse_payload(value)
}

pub fn payload_to_rows(payloads: Vec<ProviderPayload>) -> Vec<ProviderRow> {
    payload_to_rows_with_costs(payloads, &HashMap::new())
}

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
    costs
        .iter()
        .find(|(k, _)| key.starts_with(k.as_str()) || k.starts_with(&key))
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
            let total_minutes = duration.num_minutes();
            let days = total_minutes / (60 * 24);
            let hours = (total_minutes / 60) % 24;
            let mins = total_minutes % 60;

            return if days > 0 {
                format!("in {days}d {hours}h {mins}m")
            } else if hours > 0 {
                format!("in {hours}h {mins}m")
            } else {
                format!("in {mins}m")
            };
        }
    }
    // Fall back to description if we can't compute relative time
    description.unwrap_or_else(|| "—".to_string())
}

pub fn format_updated(value: Option<String>) -> String {
    let Some(value) = value else {
        return "—".to_string();
    };
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(&value) {
        let local = timestamp.with_timezone(&Local);
        return local.format("%H:%M").to_string();
    }
    if let Some((_, time_part)) = value.split_once('T') {
        let time = time_part.trim_end_matches('Z');
        let short = time.get(0..5).unwrap_or(time);
        return short.to_string();
    }
    value
}

/// Format an ISO8601 timestamp as a relative "Xm ago" string.
/// Returns None if parsing fails.
pub fn format_updated_relative(iso: &str) -> Option<String> {
    let ts = DateTime::parse_from_rfc3339(iso).ok()?;
    let delta = Utc::now().signed_duration_since(ts.with_timezone(&Utc));
    let secs = delta.num_seconds().max(0);
    Some(if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    })
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

/// Read cache, returning only successful payloads (for backwards compatibility).
pub fn read_cache(path: &Path) -> Result<Vec<ProviderPayload>> {
    let cached = read_cache_full(path)?;
    Ok(cached.payloads().to_vec())
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
            updated_at_ms: now_ms() as i64,
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

/// The machine this snapshot was written on. Resolved once per process.
pub fn device_identity(cache_file: &Path) -> DeviceIdentity {
    static IDENTITY: std::sync::OnceLock<DeviceIdentity> = std::sync::OnceLock::new();
    IDENTITY
        .get_or_init(|| DeviceIdentity {
            machine_id: machine_id(cache_file),
            hostname: hostname(),
        })
        .clone()
}

/// Domain separator for the device id. Bumping it re-keys every machine, which
/// a future snapshot merge would read as a fleet of new devices.
const DEVICE_ID_DOMAIN: &str = "tokengauge.device-id.v1";

/// `/etc/machine-id` is confidential - systemd's own documentation says an
/// application wanting a stable host identifier must derive one rather than
/// hand the id out, and this snapshot is written to be synced. The digest is
/// stable for a machine and the id it came from cannot be read back out of it.
/// Same shape as `sd_id128_get_machine_app_specific`.
fn derive_device_id(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_DOMAIN.as_bytes());
    hasher.update([0u8]);
    hasher.update(raw.as_bytes());
    hex16(&hasher.finalize())
}

fn hex16(digest: &[u8]) -> String {
    use std::fmt::Write;
    digest.iter().take(16).fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn machine_id(cache_file: &Path) -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let id = contents.trim();
            if !id.is_empty() {
                return derive_device_id(id);
            }
        }
    }
    // Windows, macOS, or a container without systemd: keep one beside the
    // snapshot instead, so it is at least stable for this user on this machine.
    let path = cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-device-id");
    if let Ok(contents) = fs::read_to_string(&path) {
        let id = contents.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let generated = generated_machine_id();
    let _ = write_atomic(&path, generated.as_bytes());
    generated
}

/// Only reached where there is no system id to derive from. Generated once and
/// kept, so it needs to be unique rather than unguessable.
fn generated_machine_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_DOMAIN.as_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(hostname().as_bytes());
    hex16(&hasher.finalize())
}

fn hostname() -> String {
    // The kernel first: a shell's exported HOSTNAME can outlive a rename.
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let name = contents.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(key) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "unknown".to_string()
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

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
pub fn refresh_sentinel_deadline_ms(config: &TokenGaugeConfig) -> u64 {
    now_ms().saturating_add(refresh_budget_ms(config))
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
    if let Ok(deadline) = contents.trim().parse::<u64>()
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

/// Write cache with only payloads (legacy, for backwards compatibility).
pub fn write_cache(
    path: &Path,
    payloads: &[ProviderPayload],
    providers: &ProvidersConfig,
) -> Result<()> {
    write_cache_full(path, payloads, &[], &HashMap::new(), providers, None)
}

// ============================================================================
// Display helpers (shared between waybar binary and TUI)
// ============================================================================

pub const DIM_HEX: &str = "#6c7086";
pub const SEPARATOR_HEX: &str = "#45475a";
pub const GREEN_HEX: &str = "#a6e3a1";
pub const YELLOW_HEX: &str = "#f9e2af";
pub const RED_HEX: &str = "#f38ba8";
pub const NEUTRAL_HEX: &str = "#cdd6f4";

/// Process-global active theme.
/// `install_theme` may be called more than once (e.g. on a daemon SIGHUP
/// reload); each installation `Box::leak`s a fresh `Theme` so existing
/// `&'static Theme` references stay valid. The leaked memory is a few
/// hundred bytes per reload and is never reclaimed; acceptable because
/// reloads are user-initiated and rare.
static ACTIVE_THEME: std::sync::RwLock<Option<&'static Theme>> = std::sync::RwLock::new(None);

pub fn theme() -> &'static Theme {
    if let Some(t) = *ACTIVE_THEME.read().expect("theme lock poisoned") {
        return t;
    }
    let mut w = ACTIVE_THEME.write().expect("theme lock poisoned");
    if let Some(t) = *w {
        return t;
    }
    let default: &'static Theme = Box::leak(Box::new(Theme::catppuccin()));
    *w = Some(default);
    default
}

pub fn install_theme(t: Theme) {
    let leaked: &'static Theme = Box::leak(Box::new(t));
    *ACTIVE_THEME.write().expect("theme lock poisoned") = Some(leaked);
}

/// Resolved color palette used by both waybar tooltip and TUI.
/// Fields are owned `String` so the values can come from a config override.
#[derive(Debug, Clone)]
pub struct Theme {
    pub dim: String,
    pub separator: String,
    pub green: String,
    pub yellow: String,
    pub red: String,
    pub neutral: String,
}

impl Theme {
    pub fn catppuccin() -> Self {
        Self {
            dim: DIM_HEX.into(),
            separator: SEPARATOR_HEX.into(),
            green: GREEN_HEX.into(),
            yellow: YELLOW_HEX.into(),
            red: RED_HEX.into(),
            neutral: NEUTRAL_HEX.into(),
        }
    }

    pub fn nord() -> Self {
        Self {
            dim: "#4c566a".into(),
            separator: "#3b4252".into(),
            green: "#a3be8c".into(),
            yellow: "#ebcb8b".into(),
            red: "#bf616a".into(),
            neutral: "#d8dee9".into(),
        }
    }

    pub fn gruvbox() -> Self {
        Self {
            dim: "#928374".into(),
            separator: "#504945".into(),
            green: "#b8bb26".into(),
            yellow: "#fabd2f".into(),
            red: "#fb4934".into(),
            neutral: "#ebdbb2".into(),
        }
    }

    /// Pick the color matching a usage percentage (green <50, yellow <80, red).
    pub fn color_for_percent(&self, percent: u8) -> &str {
        match percent {
            0..=49 => &self.green,
            50..=79 => &self.yellow,
            _ => &self.red,
        }
    }
}

pub struct ProviderIcon {
    pub glyph: &'static str,
    pub color_hex: &'static str,
}

pub fn provider_icon(label: &str) -> ProviderIcon {
    match label.to_lowercase().as_str() {
        "claude" => ProviderIcon {
            glyph: "\u{f0721}",
            color_hex: "#DE7356",
        },
        "codex" => ProviderIcon {
            glyph: "\u{f0b2b}",
            color_hex: "#74AA9C",
        },
        "kimi" => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: "#FE603C",
        },
        "grok" => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: "#000000",
        },
        "glm" => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: "#E85A6A",
        },
        _ => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: NEUTRAL_HEX,
        },
    }
}

/// Basename slug of the bundled brand SVG for a provider label, if one ships.
pub fn provider_icon_slug(label: &str) -> Option<&'static str> {
    Some(match label.to_lowercase().as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "kimi" => "kimi",
        "grok" => "grok",
        "glm" => "glm",
        _ => return None,
    })
}

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

/// Provider-specific labels for the three usage windows.
/// Defaults to generic "Session"/"Weekly"/"Tertiary" for unknown providers.
pub fn window_labels(provider: &str) -> (&'static str, &'static str, &'static str) {
    match provider.to_lowercase().as_str() {
        "claude" => ("Session", "Weekly (all)", "Weekly (Sonnet)"),
        "kimi" => ("Weekly", "Rate Limit", "Tertiary"),
        "grok" => ("Weekly", "On-demand", "Tertiary"),
        "glm" => ("Weekly", "30-day", "5-hour"),
        _ => ("Session", "Weekly", "Tertiary"),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderUrls {
    pub dashboard: Option<&'static str>,
    pub status: Option<&'static str>,
}

pub fn provider_urls(provider: &str) -> ProviderUrls {
    match provider.to_lowercase().as_str() {
        "claude" => ProviderUrls {
            dashboard: Some("https://claude.ai/settings/usage"),
            status: Some("https://status.anthropic.com"),
        },
        "codex" => ProviderUrls {
            dashboard: Some("https://platform.openai.com/usage"),
            status: Some("https://status.openai.com"),
        },
        "kimi" => ProviderUrls {
            dashboard: Some("https://www.kimi.com/code/console"),
            status: None,
        },
        "grok" => ProviderUrls {
            dashboard: Some("https://grok.com/?_s=usage"),
            status: Some("https://status.x.ai"),
        },
        "glm" => ProviderUrls {
            dashboard: Some("https://zcode.z.ai/en"),
            status: None,
        },
        _ => ProviderUrls {
            dashboard: None,
            status: None,
        },
    }
}

pub fn color_hex_for_percent(percent: u8) -> &'static str {
    match percent {
        0..=49 => GREEN_HEX,
        50..=79 => YELLOW_HEX,
        _ => RED_HEX,
    }
}

/// Render a 1-row sparkline from `values`, using the standard 8 block chars
/// scaled relative to the max value. Empty input or all-zero input returns
/// the lowest-block character repeated.
pub fn sparkline(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let chars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return chars[0].to_string().repeat(values.len());
    }
    values
        .iter()
        .map(|v| {
            let idx = ((v.max(0.0) / max) * 7.0).round() as usize;
            chars[idx.min(7)]
        })
        .collect()
}

pub fn format_tokens(t: u64) -> String {
    if t >= 1_000_000_000 {
        format!("{:.1}B", t as f64 / 1e9)
    } else if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1e6)
    } else if t >= 1_000 {
        format!("{:.1}K", t as f64 / 1e3)
    } else {
        format!("{t}")
    }
}

/// Parse `#RRGGBB` into (r, g, b). Returns None on malformed input.
pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

// ============================================================================
// Waybar State (rotation selection)
// ============================================================================

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

/// Pure decision logic: given the current pct, the window's reset timestamp,
/// and the previously-notified thresholds, returns (thresholds_to_fire,
/// updated_notified_list).
///
/// Window roll-over clears the one-shot guard so the new window can alert
/// again. The reset timestamp is the reliable signal: when `resets_at` advances
/// to a new value the window rolled. Only when a provider gives no timestamp do
/// we fall back to the legacy heuristic (pct fell 10+ points below the highest
/// fired threshold) - which mis-fires when a fresh window briefly reports a
/// stale-high percent, or when the value wobbles near the top and clears + re-
/// fires on every poll, spamming alerts.
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
        // Only a strictly forward move is a new window. A stale/older payload
        // must not clear the guard, else the real timestamp returns next poll
        // and notifications re-fire.
        (Some(now), Some(prev)) => match (
            DateTime::parse_from_rfc3339(now),
            DateTime::parse_from_rfc3339(prev),
        ) {
            (Ok(now), Ok(prev)) => now > prev,
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

    let month_start = today
        .format("%Y-%m-01")
        .to_string()
        .parse::<chrono::NaiveDate>()
        .unwrap_or(today);
    let offset = *Local::now().offset();
    let local_id = sync::local_device_id(config);
    let note = sync::note(&outcome.status, config.refresh_secs, now_ms() as i64);
    for (provider, cost) in report.costs.iter_mut() {
        cost.by_device =
            outcome
                .store
                .device_totals(provider, (month_start, today), offset, &prices, &local_id);
        cost.sync_note = note.clone();
    }
    report.sync = outcome.status;
    report
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
                    if NATIVELY_READ.contains(&provider.as_str())
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

/// The providers the native readers can produce on their own, being the
/// transcript trees they parse. Everything else reaches a cost row through the
/// `auto` fallback, so its absence from a native read says nothing.
/// Providers TokenGauge parses transcripts for. Only these produce usage
/// events, so only these can take part in fleet sync.
pub const NATIVELY_READ: &[&str] = &["claude", "codex"];

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

    CostDiagnostics {
        source,
        events: report.events,
        elapsed,
        roots: cost::transcript_roots(),
        prices: cost::pricing::load(cache_file, timeout, false).len(),
        native: month_totals(&report.costs),
        ccusage: compare.then(|| month_totals(&fetch_ccusage_costs(timeout))),
        unpriced: report.unpriced,
    }
}

// ============================================================================
// ccusage Integration
// ============================================================================

/// Map a ccusage model name to a TokenGauge provider key.
/// Returns None if the model doesn't belong to a tracked provider.
pub fn model_to_provider(model: &str) -> Option<&'static str> {
    let lower = model.to_lowercase();
    if lower.starts_with("claude") {
        Some("claude")
    } else if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("codex")
        || lower.starts_with("openai")
    {
        Some("codex")
    } else if lower.starts_with("kimi") || lower.starts_with("moonshot") {
        Some("kimi")
    } else if lower.starts_with("grok") {
        Some("grok")
    } else if lower.starts_with("glm") {
        Some("glm")
    } else {
        None
    }
}

/// Map a ccusage agent name (the `--by-agent` split) to a provider key.
///
/// Only consulted when the model name is not conclusive, because the agent is
/// the coarser signal: a GLM or Kimi model driven through Claude Code is
/// reported under the `claude` agent, and it is the model that says whose
/// money it was.
fn agent_to_provider(agent: &str) -> Option<&'static str> {
    match agent.to_lowercase().as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "kimi" => Some("kimi"),
        "grok" => Some("grok"),
        _ => None,
    }
}

/// Length of the rolling cost history, in days.
pub(crate) const WEEKLY_HISTORY_DAYS: usize = 7;

#[derive(Debug, Clone, Default, Deserialize)]
struct CcusageDailyResponse {
    #[serde(default)]
    daily: Vec<CcusageDay>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageDay {
    #[serde(default)]
    agents: Vec<CcusageAgent>,
    #[serde(default)]
    model_breakdowns: Vec<CcusageModelBreakdown>,
    #[serde(default)]
    period: String,
}

/// One agent's slice of a day, from `--by-agent`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageAgent {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    model_breakdowns: Vec<CcusageModelBreakdown>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageModelBreakdown {
    model_name: String,
    #[serde(default)]
    cost: f64,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
}

/// Every breakdown in a day, paired with the provider it belongs to.
///
/// `agents` and `model_breakdowns` carry the same spend - the flat list is the
/// per-agent one merged - so exactly one of them is read, never both. ccusage
/// builds too old for `--by-agent` only emit the flat list, and there a model
/// name that maps nowhere (a Kimi or Grok run) is simply dropped, as before.
fn day_breakdowns(day: &CcusageDay) -> Vec<(&'static str, &CcusageModelBreakdown)> {
    if day.agents.is_empty() {
        return day
            .model_breakdowns
            .iter()
            .filter_map(|b| Some((model_to_provider(&b.model_name)?, b)))
            .collect();
    }
    day.agents
        .iter()
        .flat_map(|a| {
            a.model_breakdowns.iter().filter_map(|b| {
                let provider =
                    model_to_provider(&b.model_name).or_else(|| agent_to_provider(&a.agent))?;
                Some((provider, b))
            })
        })
        .collect()
}

fn ccusage_total_tokens(b: &CcusageModelBreakdown) -> u64 {
    b.input_tokens + b.output_tokens + b.cache_creation_tokens + b.cache_read_tokens
}

/// Running per-model totals, minus the model name (it is the map key).
#[derive(Default)]
struct ModelTotals {
    usd: f64,
    tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
}

impl ModelTotals {
    fn add(&mut self, b: &CcusageModelBreakdown) {
        self.usd += b.cost;
        self.tokens += ccusage_total_tokens(b);
        self.input_tokens += b.input_tokens;
        self.output_tokens += b.output_tokens;
        self.cache_creation_tokens += b.cache_creation_tokens;
        self.cache_read_tokens += b.cache_read_tokens;
    }
}

struct AggregatedProvider {
    total_usd: f64,
    total_tokens: u64,
    models: HashMap<String, ModelTotals>,
}

impl AggregatedProvider {
    fn into_model_costs(self) -> (f64, u64, Vec<ModelCost>) {
        let mut models: Vec<ModelCost> = self
            .models
            .into_iter()
            .map(|(model, t)| ModelCost {
                model,
                usd: t.usd,
                tokens: t.tokens,
                input_tokens: t.input_tokens,
                output_tokens: t.output_tokens,
                cache_creation_tokens: t.cache_creation_tokens,
                cache_read_tokens: t.cache_read_tokens,
            })
            .collect();
        models.sort_by(|a, b| {
            b.usd
                .partial_cmp(&a.usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (self.total_usd, self.total_tokens, models)
    }
}

/// The last `n` calendar days ending at `today`, oldest first.
///
/// ccusage omits days with no usage entirely, so the window has to be built
/// from the calendar rather than from the response: an idle day is $0 spent,
/// not a day that did not happen, and dropping it silently shortens the series
/// and shifts every label in a chart drawn from it.
fn recent_periods(today: NaiveDate, n: usize) -> Vec<String> {
    (0..n)
        .rev()
        .filter_map(|offset| today.checked_sub_days(Days::new(offset as u64)))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .collect()
}

/// Last `n` calendar days of cost and tokens per provider, oldest first. Every
/// provider's series covers the same dates, zero-filled where it spent nothing.
fn last_n_days_by_provider(
    response: &CcusageDailyResponse,
    today: NaiveDate,
    n: usize,
) -> HashMap<String, Vec<DayCost>> {
    // (provider, period) -> (usd, tokens)
    let mut per_day: HashMap<String, HashMap<String, (f64, u64)>> = HashMap::new();
    for day in &response.daily {
        if day.period.is_empty() {
            continue;
        }
        for (provider, b) in day_breakdowns(day) {
            let entry = per_day
                .entry(provider.to_string())
                .or_default()
                .entry(day.period.clone())
                .or_insert((0.0, 0));
            entry.0 += b.cost;
            entry.1 += ccusage_total_tokens(b);
        }
    }
    let periods = recent_periods(today, n);
    per_day
        .into_iter()
        .map(|(provider, days)| {
            let series: Vec<DayCost> = periods
                .iter()
                .map(|p| {
                    let (usd, tokens) = days.get(p).copied().unwrap_or((0.0, 0));
                    DayCost {
                        date: p.clone(),
                        usd,
                        tokens,
                    }
                })
                .collect();
            (provider, series)
        })
        .collect()
}

/// Aggregate the days whose `period` falls within `since..=until` (inclusive,
/// `YYYY-MM-DD`, compared lexicographically - which is a date comparison in
/// that format). One ccusage call now covers today, the month and the rolling
/// week, and each figure slices the window it needs out of the same response.
fn aggregate_ccusage(
    response: &CcusageDailyResponse,
    since: &str,
    until: &str,
) -> HashMap<String, AggregatedProvider> {
    let mut totals: HashMap<String, AggregatedProvider> = HashMap::new();
    for day in &response.daily {
        if day.period.as_str() < since || day.period.as_str() > until {
            continue;
        }
        for (provider, b) in day_breakdowns(day) {
            let entry = totals
                .entry(provider.to_string())
                .or_insert_with(|| AggregatedProvider {
                    total_usd: 0.0,
                    total_tokens: 0,
                    models: HashMap::new(),
                });
            entry.total_usd += b.cost;
            entry.total_tokens += ccusage_total_tokens(b);
            entry.models.entry(b.model_name.clone()).or_default().add(b);
        }
    }
    totals
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBlocksResponse {
    #[serde(default)]
    blocks: Vec<CcusageBlock>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBlock {
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    burn_rate: Option<CcusageBurnRate>,
    #[serde(default)]
    projection: Option<CcusageProjection>,
    #[serde(default, rename = "costUSD")]
    cost_usd: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageBurnRate {
    cost_per_hour: f64,
    #[serde(default)]
    tokens_per_minute: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageProjection {
    #[serde(default)]
    remaining_minutes: u32,
    #[serde(default)]
    total_cost: f64,
}

/// Resolve which command launches ccusage on this host.
/// Order: direct `ccusage` (global npm/bun/AUR install) → `bunx ccusage` →
/// `npx --yes ccusage` (Node.js fallback). First one whose binary is on PATH
/// is used. Returns None if no runner is available.
fn resolve_ccusage_runner() -> Option<Vec<String>> {
    if binary_on_path("ccusage") {
        return Some(vec!["ccusage".into()]);
    }
    if binary_on_path("bunx") {
        return Some(vec!["bunx".into(), "ccusage".into()]);
    }
    if binary_on_path("npx") {
        return Some(vec!["npx".into(), "--yes".into(), "ccusage".into()]);
    }
    None
}

pub fn ccusage_runner_description() -> Option<String> {
    resolve_ccusage_runner().map(|parts| parts.join(" "))
}

fn binary_on_path(name: &str) -> bool {
    find_in_path(name).is_some()
}

/// Locate an executable named `name` on `PATH`, returning its full path.
///
/// On Windows the name is tried both as-is and with each extension from
/// `PATHEXT` (falling back to a sensible default set), so shims like
/// `npx.cmd`, `bunx.cmd` and `ccusage.exe` are found even when the caller
/// passes the bare stem.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let is_file = |p: &Path| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false);

    for dir in std::env::split_paths(&path) {
        let direct = dir.join(name);
        if is_file(&direct) {
            return Some(direct);
        }

        #[cfg(windows)]
        {
            // Only append extensions when the name has none of its own.
            if Path::new(name).extension().is_none() {
                let pathext =
                    std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
                for cand in pathext_candidates(name, &pathext) {
                    let candidate = dir.join(cand);
                    if is_file(&candidate) {
                        return Some(candidate);
                    }
                }
            }
        }
    }
    None
}

/// Expand an extensionless executable `name` into `name.<ext>` candidates from a
/// `PATHEXT` string (e.g. "npx" -> ["npx.EXE", "npx.CMD", ...]). Empty segments
/// are skipped. Extracted so the probing order can be unit-tested without
/// touching the process environment or filesystem.
#[cfg(windows)]
fn pathext_candidates(name: &str, pathext: &str) -> Vec<String> {
    pathext
        .split(';')
        .filter(|e| !e.is_empty())
        .map(|ext| format!("{name}.{}", ext.trim_start_matches('.')))
        .collect()
}

/// Build a `Command` for the resolved ccusage runner.
///
/// On Windows the runner is very often a batch shim (`npx.cmd`, `bunx.cmd`),
/// which `CreateProcess` cannot execute directly — Rust's `Command` only
/// appends `.exe`. Routing through `cmd /C` lets the shell resolve `.cmd`/`.bat`
/// (and plain `.exe`) via `PATHEXT`. On Unix we spawn the program directly.
fn ccusage_command(runner: &[String]) -> Command {
    #[cfg(windows)]
    {
        let mut command = Command::new("cmd");
        command.arg("/C");
        for part in runner {
            command.arg(part);
        }
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(&runner[0]);
        for part in &runner[1..] {
            command.arg(part);
        }
        command
    }
}

fn run_ccusage_blocks(args: &[&str], deadline: Instant) -> Result<CcusageBlocksResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.args(args).arg("--json");
    let output =
        run_with_timeout(command, budget_left(deadline)?).context("ccusage blocks failed")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ccusage blocks exit non-zero: {}", stderr.trim()));
    }
    serde_json::from_slice(&output.stdout).context("ccusage blocks output was not valid JSON")
}

struct ActiveBlockInfo {
    burn: Option<BurnRate>,
    session_usd: f64,
}

fn fetch_active_blocks(deadline: Instant) -> HashMap<String, ActiveBlockInfo> {
    let resp = match run_ccusage_blocks(&["blocks", "--active", "--offline"], deadline)
        .or_else(|_| run_ccusage_blocks(&["blocks", "--active"], deadline))
    {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    let mut by_provider: HashMap<String, ActiveBlockInfo> = HashMap::new();
    for block in resp.blocks.into_iter().filter(|b| b.is_active) {
        let provider = block
            .models
            .iter()
            .find_map(|m| model_to_provider(m))
            .unwrap_or("claude")
            .to_string();
        let burn = match (block.burn_rate, block.projection) {
            (Some(rate), Some(proj)) => Some(BurnRate {
                cost_per_hour: rate.cost_per_hour,
                tokens_per_minute: rate.tokens_per_minute as u64,
                remaining_minutes: proj.remaining_minutes,
                projected_cost: proj.total_cost,
            }),
            _ => None,
        };
        by_provider.insert(
            provider,
            ActiveBlockInfo {
                burn,
                session_usd: block.cost_usd,
            },
        );
    }
    by_provider
}

/// Run `ccusage daily` once for everything, with the two flags that make it
/// cheap and precise.
///
/// `--offline` skips the LiteLLM pricing fetch each invocation otherwise pays
/// for - measurably ~700ms, for figures identical to the cent. `--by-agent`
/// splits the merged breakdown back out per CLI, which is what lets a Kimi or
/// Grok row exist at all. A ccusage too old for either flag rejects it and
/// exits non-zero, so the bare form is retried before the caller gives up and
/// shows no cost at all.
fn run_ccusage_daily(since: &str, deadline: Instant) -> Result<CcusageDailyResponse> {
    run_ccusage(
        &["daily", "--since", since, "--offline", "--by-agent"],
        deadline,
    )
    .or_else(|_| run_ccusage(&["daily", "--since", since], deadline))
}

/// What is left of the cost fetch's budget.
///
/// Every ccusage call is a retry away from a second one, and both the daily and
/// the blocks call can retry, so handing each attempt the full timeout lets a
/// slow or too-old ccusage run four of them back to back. That outlives
/// `refresh_budget_ms`, which sizes the refresh sentinel from a single timeout -
/// and once the sentinel expires a second refresh starts on top of the first.
/// One deadline for the whole fetch keeps it inside the budget.
///
/// Past the deadline this fails instead of clamping to a token slice: another
/// ccusage process started there would overrun the budget it was meant to
/// respect, and a sliver of a timeout only buys a subprocess spawn it cannot
/// finish inside.
fn budget_left(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left < MIN_CCUSAGE_SLICE {
        return Err(anyhow!("cost fetch budget exhausted"));
    }
    Ok(left)
}

/// Less than this left on the clock is not worth spawning for.
const MIN_CCUSAGE_SLICE: Duration = Duration::from_millis(500);

fn run_ccusage(args: &[&str], deadline: Instant) -> Result<CcusageDailyResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.args(args).arg("--json");
    let output = run_with_timeout(command, budget_left(deadline)?).context("ccusage failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ccusage exited non-zero: {}", stderr.trim()));
    }

    serde_json::from_slice(&output.stdout).context("ccusage output was not valid JSON")
}

/// Fetch ccusage cost info. Returns a map from provider key to CostInfo.
/// Returns empty map on any failure (ccusage missing, no logs, parse error).
pub fn fetch_ccusage_costs(timeout: Duration) -> HashMap<String, CostInfo> {
    // One deadline for every call this makes, retries included.
    let deadline = Instant::now() + timeout;
    let today_date = Local::now().date_naive();
    let month_start_date = today_date
        .format("%Y-%m-01")
        .to_string()
        .parse::<NaiveDate>()
        .unwrap_or(today_date);
    // The rolling 7-day window reaches back past the 1st for the first six
    // days of a month, so the query has to start at whichever of the two is
    // earlier. Asking only from the 1st would zero-fill the days before it -
    // which reads as "spent nothing" rather than "not asked for", understating
    // `weekly_usd` and inflating the today-vs-average baseline.
    let week_start_date = today_date
        .checked_sub_days(Days::new(WEEKLY_HISTORY_DAYS as u64 - 1))
        .unwrap_or(today_date);
    let since = month_start_date.min(week_start_date);

    // One call, sliced three ways below. Three calls each re-read every
    // transcript on disk to answer a narrower question than the one before it.
    let daily = match run_ccusage_daily(&since.format("%Y%m%d").to_string(), deadline) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };

    let today = today_date.format("%Y-%m-%d").to_string();
    let month_start = month_start_date.format("%Y-%m-%d").to_string();
    let mut today_agg = aggregate_ccusage(&daily, &today, &today);
    let mut monthly_agg = aggregate_ccusage(&daily, &month_start, &today);
    let mut weekly_history = last_n_days_by_provider(&daily, today_date, WEEKLY_HISTORY_DAYS);
    let mut active_blocks = fetch_active_blocks(deadline);

    let mut result = HashMap::new();
    // `weekly_history` reaches back past the 1st, so early in a month a
    // provider can have spend in the rolling week and none in the month. Left
    // out of this set it loses its row entirely, chart and all.
    let providers: std::collections::HashSet<String> = today_agg
        .keys()
        .chain(monthly_agg.keys())
        .chain(active_blocks.keys())
        .chain(weekly_history.keys())
        .cloned()
        .collect();
    for provider in providers {
        let (today_usd, today_tokens, today_models) = today_agg
            .remove(&provider)
            .map(|a| a.into_model_costs())
            .unwrap_or((0.0, 0, Vec::new()));
        let (monthly_usd, monthly_tokens, monthly_models) = monthly_agg
            .remove(&provider)
            .map(|a| a.into_model_costs())
            .unwrap_or((0.0, 0, Vec::new()));
        let (burn_rate, session_usd) = active_blocks
            .remove(&provider)
            .map(|a| (a.burn, a.session_usd))
            .unwrap_or((None, 0.0));
        let history = weekly_history.remove(&provider).unwrap_or_default();
        let weekly_cost_history: Vec<f64> = history.iter().map(|d| d.usd).collect();
        let weekly_usd = weekly_cost_history.iter().sum();
        result.insert(
            provider,
            CostInfo {
                today_usd,
                today_tokens,
                monthly_usd,
                monthly_tokens,
                today_models,
                monthly_models,
                burn_rate,
                session_usd,
                weekly_usd,
                weekly_cost_history,
                weekly_history: history,
                by_device: Vec::new(),
                sync_note: None,
            },
        );
    }
    result
}

// ============================================================================
// Config File Operations
// ============================================================================

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

    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn ensure_table<'a>(doc: &'a mut toml_edit::DocumentMut, key: &str) -> &'a mut toml_edit::Table {
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

fn ensure_subtable<'a>(table: &'a mut toml_edit::Table, key: &str) -> &'a mut toml_edit::Table {
    if table.get(key).and_then(|i| i.as_table()).is_none() {
        let replacement = table
            .get(key)
            .and_then(|i| i.as_inline_table())
            .cloned()
            .map(|t| toml_edit::Item::Table(t.into_table()))
            .unwrap_or_else(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table.insert(key, replacement);
    }
    table[key].as_table_mut().expect("just ensured table")
}

/// Turn fleet sync on or off.
pub fn config_set_sync_enabled(path: &Path, enabled: bool) -> Result<()> {
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["enabled"] = toml_edit::value(enabled);
    })
}

pub fn config_set_sync_label(path: &Path, label: &str) -> Result<()> {
    let label = label.to_string();
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["label"] = toml_edit::value(label.as_str());
    })
}

pub fn config_set_sync_transport(path: &Path, kind: &str) -> Result<()> {
    let kind = match kind.to_ascii_lowercase().as_str() {
        "dir" => "dir",
        "s3" => "s3",
        other => {
            return Err(anyhow!(
                "unknown sync transport '{other}' (expected dir or s3)"
            ));
        }
    };
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["transport"] = toml_edit::value(kind);
    })
}

/// Point the folder transport at a directory the user's sync tool handles.
pub fn config_set_sync_dir(path: &Path, dir: &str) -> Result<()> {
    let dir = dir.trim().to_string();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "dir")["path"] = toml_edit::value(dir.as_str());
    })
}

/// Set one `[sync.s3]` field.
///
/// Credentials are deliberately not settable here: they belong in the
/// environment, not written into a config file by a setup screen.
pub fn config_set_sync_s3(path: &Path, field: &str, value: &str) -> Result<()> {
    const FIELDS: &[&str] = &["endpoint", "region", "bucket", "prefix"];
    if !FIELDS.contains(&field) {
        return Err(anyhow!(
            "'{field}' is not a settable S3 field ({}); credentials come from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
            FIELDS.join(", ")
        ));
    }
    let field = field.to_string();
    let value = value.trim().to_string();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "s3")[&field] = toml_edit::value(value.as_str());
    })
}

/// Take one provider in or out of sync. Only providers with a native reader can
/// take part: a ccusage-sourced provider has no events to bucket.
pub fn config_set_sync_provider(path: &Path, name: &str, enabled: bool) -> Result<()> {
    if !sync::syncable(name) {
        return Err(anyhow!(
            "'{name}' has no transcript reader, so it cannot sync (it can be one of: {})",
            NATIVELY_READ.join(", ")
        ));
    }
    let name = name.to_lowercase();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "providers")[&name] = toml_edit::value(enabled);
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
    use super::*;

    // ------------------------------------------------------------------------
    // Windows executable discovery / command construction
    // ------------------------------------------------------------------------

    #[cfg(windows)]
    #[test]
    fn pathext_candidates_appends_each_extension() {
        assert_eq!(
            pathext_candidates("npx", ".EXE;.CMD;.BAT"),
            vec![
                "npx.EXE".to_string(),
                "npx.CMD".to_string(),
                "npx.BAT".to_string()
            ]
        );
        // Empty PATHEXT segments are skipped.
        assert_eq!(
            pathext_candidates("foo", ".EXE;;"),
            vec!["foo.EXE".to_string()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn ccusage_command_routes_through_cmd_preserving_args() {
        let runner = vec![
            "npx".to_string(),
            "--yes".to_string(),
            "ccusage".to_string(),
        ];
        let command = ccusage_command(&runner);
        assert_eq!(command.get_program().to_string_lossy(), "cmd");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/C", "npx", "--yes", "ccusage"]);
    }

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

    fn cache_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        dir
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
    fn device_id_is_derived_not_the_system_one() {
        let raw = "0123456789abcdef0123456789abcdef";
        let derived = derive_device_id(raw);

        // Stable for a machine, and the system id cannot be read back out of it.
        assert_eq!(derived, derive_device_id(raw));
        assert_ne!(derived, raw);
        assert!(!derived.contains(raw));
        assert_eq!(derived.len(), 32);
        assert!(derived.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            derived,
            derive_device_id("0123456789abcdef0123456789abcdee")
        );
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

    #[test]
    fn provider_fetch_error_timeout() {
        let error = ProviderFetchError::new("codex".to_string(), "timeout after 2s");
        assert_eq!(error.message, "Request timed out");
        assert_eq!(error.raw, "timeout after 2s");
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
    fn parse_payload_single_object() {
        let json = r#"{"provider":"claude","version":"2.1.12","source":"oauth"}"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let payloads = parse_payload(value).unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].provider, "claude");
    }

    #[test]
    fn parse_payload_array() {
        let json = r#"[{"provider":"claude"},{"provider":"codex"}]"#;
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let payloads = parse_payload(value).unwrap();
        assert_eq!(payloads.len(), 2);
    }

    #[test]
    fn parse_payload_bytes_valid() {
        let json = br#"{"provider":"claude","version":"2.1.12"}"#;
        let payloads = parse_payload_bytes(json).unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].version, Some("2.1.12".to_string()));
    }

    #[test]
    fn parse_payload_bytes_invalid_json() {
        let json = b"not valid json";
        let result = parse_payload_bytes(json);
        assert!(result.is_err());
    }

    #[test]
    fn parse_payload_with_full_usage() {
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
        let payloads = parse_payload_bytes(json.as_bytes()).unwrap();
        assert_eq!(payloads.len(), 1);

        let payload = &payloads[0];
        assert_eq!(payload.provider, "claude");
        assert!(!payload.has_error());

        let usage = payload.usage.as_ref().unwrap();
        let primary = usage.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, Some(19));
        assert_eq!(primary.window_minutes, Some(300));
    }

    // ------------------------------------------------------------------------
    // payload_to_rows tests
    // ------------------------------------------------------------------------

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
        let rows = payload_to_rows(vec![good, bad]);
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
        let rows = payload_to_rows(vec![payload]);
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
        let rows = payload_to_rows(vec![payload1]);
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
        let rows = payload_to_rows(vec![payload2]);
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
        let rows = payload_to_rows(vec![payload3]);
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
        let rows = payload_to_rows(vec![payload4]);
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
    fn color_hex_for_percent_thresholds() {
        assert_eq!(color_hex_for_percent(0), GREEN_HEX);
        assert_eq!(color_hex_for_percent(49), GREEN_HEX);
        assert_eq!(color_hex_for_percent(50), YELLOW_HEX);
        assert_eq!(color_hex_for_percent(79), YELLOW_HEX);
        assert_eq!(color_hex_for_percent(80), RED_HEX);
    }

    #[test]
    fn parse_hex_rgb_works() {
        assert_eq!(parse_hex_rgb("#a6e3a1"), Some((0xa6, 0xe3, 0xa1)));
        assert_eq!(parse_hex_rgb("#DE7356"), Some((0xDE, 0x73, 0x56)));
        assert_eq!(parse_hex_rgb("not-hex"), None);
        assert_eq!(parse_hex_rgb("#abc"), None);
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

    fn ccusage_fixture() -> CcusageDailyResponse {
        serde_json::from_str(
            r#"{
              "daily": [
                {
                  "period": "2026-08-18",
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 1.5,
                      "inputTokens": 10, "outputTokens": 20,
                      "cacheCreationTokens": 30, "cacheReadTokens": 40 }
                  ]
                },
                {
                  "period": "2026-08-20",
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 2.5,
                      "inputTokens": 1, "outputTokens": 2,
                      "cacheCreationTokens": 3, "cacheReadTokens": 4 },
                    { "modelName": "gpt-5-codex", "cost": 9.0,
                      "inputTokens": 5, "outputTokens": 5,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                  ]
                }
              ]
            }"#,
        )
        .expect("parse ccusage fixture")
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn daily_history_carries_dates_and_tokens() {
        let history = last_n_days_by_provider(&ccusage_fixture(), day(2026, 8, 20), 4);
        let claude = history.get("claude").expect("claude history");

        // ccusage omits days with no usage, so the series is built from the
        // calendar: the 17th predates the fixture and the 19th is idle, and
        // both still get a zeroed entry rather than vanishing.
        assert_eq!(
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-17", "2026-08-18", "2026-08-19", "2026-08-20"]
        );
        assert_eq!(
            claude.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![0, 100, 0, 10]
        );
        assert_eq!(claude[2].usd, 0.0);

        // Every provider covers the same window: codex only spent on the 20th.
        let codex = history.get("codex").expect("codex history");
        assert_eq!(
            codex.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(
            codex.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![0, 0, 0, 10]
        );
    }

    #[test]
    fn daily_history_ends_on_today_and_ignores_later_entries() {
        // A day past the window - a machine whose clock ran ahead, or a stale
        // cache read after midnight - must not push the window forward.
        let history = last_n_days_by_provider(&ccusage_fixture(), day(2026, 8, 19), 2);
        let claude = history.get("claude").expect("claude history");
        assert_eq!(
            claude.iter().map(|d| d.date.as_str()).collect::<Vec<_>>(),
            vec!["2026-08-18", "2026-08-19"]
        );
        assert_eq!(
            claude.iter().map(|d| d.tokens).collect::<Vec<_>>(),
            vec![100, 0]
        );
    }

    /// `--by-agent` output: the same day, split per CLI. A `glm-4.6` run and a
    /// Kimi one both land under the `claude` agent because both are driven
    /// through Claude Code.
    fn by_agent_fixture() -> CcusageDailyResponse {
        serde_json::from_str(
            r#"{
              "daily": [
                {
                  "period": "2026-08-20",
                  "agents": [
                    {
                      "agent": "claude",
                      "modelBreakdowns": [
                        { "modelName": "claude-opus-5", "cost": 2.0,
                          "inputTokens": 1, "outputTokens": 1,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                        { "modelName": "glm-4.6", "cost": 0.5,
                          "inputTokens": 2, "outputTokens": 2,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                      ]
                    },
                    {
                      "agent": "grok",
                      "modelBreakdowns": [
                        { "modelName": "some-unlabelled-model", "cost": 1.0,
                          "inputTokens": 3, "outputTokens": 3,
                          "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                      ]
                    }
                  ],
                  "modelBreakdowns": [
                    { "modelName": "claude-opus-5", "cost": 2.0,
                      "inputTokens": 1, "outputTokens": 1,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                    { "modelName": "glm-4.6", "cost": 0.5,
                      "inputTokens": 2, "outputTokens": 2,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 },
                    { "modelName": "some-unlabelled-model", "cost": 1.0,
                      "inputTokens": 3, "outputTokens": 3,
                      "cacheCreationTokens": 0, "cacheReadTokens": 0 }
                  ]
                }
              ]
            }"#,
        )
        .expect("parse by-agent fixture")
    }

    #[test]
    fn by_agent_splits_providers_sharing_one_agent() {
        let agg = aggregate_ccusage(&by_agent_fixture(), "2026-08-20", "2026-08-20");

        // The model wins over the agent: GLM through Claude Code is GLM spend,
        // and before --by-agent it was dropped on the floor entirely.
        assert_eq!(agg.get("claude").expect("claude").total_usd, 2.0);
        assert_eq!(agg.get("glm").expect("glm").total_usd, 0.5);
        // The agent is the fallback when the model name maps nowhere.
        assert_eq!(agg.get("grok").expect("grok").total_usd, 1.0);
    }

    #[test]
    fn by_agent_and_flat_breakdowns_are_not_double_counted() {
        // ccusage emits both: `modelBreakdowns` is the per-agent data merged.
        let agg = aggregate_ccusage(&by_agent_fixture(), "2026-08-20", "2026-08-20");
        let total: f64 = agg.values().map(|a| a.total_usd).sum();
        assert_eq!(total, 3.5);

        let history = last_n_days_by_provider(&by_agent_fixture(), day(2026, 8, 20), 1);
        assert_eq!(history.get("claude").expect("claude")[0].usd, 2.0);
        assert_eq!(history.get("glm").expect("glm")[0].tokens, 4);
    }

    #[test]
    fn aggregate_window_slices_one_response() {
        // One ccusage call answers today, the month and the rolling week.
        let f = ccusage_fixture();
        let today = aggregate_ccusage(&f, "2026-08-20", "2026-08-20");
        assert_eq!(today.get("claude").expect("claude").total_usd, 2.5);
        assert_eq!(today.get("codex").expect("codex").total_usd, 9.0);

        let month = aggregate_ccusage(&f, "2026-08-01", "2026-08-20");
        assert_eq!(month.get("claude").expect("claude").total_usd, 4.0);

        // A day past `until` (a clock that ran ahead) stays out.
        let stale = aggregate_ccusage(&f, "2026-08-01", "2026-08-19");
        assert_eq!(stale.get("claude").expect("claude").total_usd, 1.5);
        assert!(!stale.contains_key("codex"));
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
    fn a_provider_with_only_last_months_spend_keeps_its_week() {
        // 2026-08-03: the rolling week reaches back to 2026-07-28, so spend on
        // the 31st is in the week and not in the month. The provider still has
        // a row, and it still has its chart.
        let response: CcusageDailyResponse = serde_json::from_str(
            r#"{"daily":[{"period":"2026-07-31","modelBreakdowns":[
                 {"modelName":"gpt-5-codex","cost":3.0,"inputTokens":5,
                  "outputTokens":5,"cacheCreationTokens":0,"cacheReadTokens":0}]}]}"#,
        )
        .expect("fixture parses");

        let today = day(2026, 8, 3);
        let history = last_n_days_by_provider(&response, today, WEEKLY_HISTORY_DAYS);
        let month = aggregate_ccusage(&response, "2026-08-01", "2026-08-03");

        assert!(month.is_empty(), "nothing was spent this month");
        let codex = history.get("codex").expect("codex has a week");
        assert_eq!(codex.iter().map(|d| d.usd).sum::<f64>(), 3.0);

        // The set the result is built from has to include it, or the row and
        // its history are dropped on the floor.
        let providers: std::collections::HashSet<&String> =
            month.keys().chain(history.keys()).collect();
        assert!(providers.contains(&"codex".to_string()));
    }

    #[test]
    fn recent_periods_spans_a_month_boundary() {
        assert_eq!(
            recent_periods(day(2026, 3, 2), 3),
            vec![
                "2026-02-28".to_string(),
                "2026-03-01".to_string(),
                "2026-03-02".to_string(),
            ]
        );
    }

    #[test]
    fn model_costs_carry_the_token_split() {
        let mut agg = aggregate_ccusage(&ccusage_fixture(), "2026-08-01", "2026-08-31");
        let (usd, tokens, models) = agg.remove("claude").expect("claude").into_model_costs();

        assert_eq!(usd, 4.0);
        assert_eq!(tokens, 110);
        assert_eq!(models.len(), 1);

        let opus = &models[0];
        assert_eq!(opus.model, "claude-opus-5");
        assert_eq!(opus.tokens, 110);
        assert_eq!(opus.input_tokens, 11);
        assert_eq!(opus.output_tokens, 22);
        assert_eq!(opus.cache_creation_tokens, 33);
        assert_eq!(opus.cache_read_tokens, 44);
        assert_eq!(
            opus.input_tokens
                + opus.output_tokens
                + opus.cache_creation_tokens
                + opus.cache_read_tokens,
            opus.tokens
        );
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

    fn tempdir_for_test(prefix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        path.push(format!("tokengauge-test-{prefix}-{pid}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
