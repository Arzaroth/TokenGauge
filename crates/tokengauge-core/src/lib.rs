use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

#[cfg(feature = "self-update")]
pub mod update;

// ============================================================================
// Codexbar Payload Types
// ============================================================================

/// Deserialize a percentage that may arrive as an integer (upstream codexbar)
/// or a float (e.g. Win-CodexBar sends `100.0`), rounding/clamping to `0..=100`.
fn de_opt_percent<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<f64>::deserialize(deserializer)?;
    Ok(opt.map(|v| v.round().clamp(0.0, 100.0) as u8))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub primary: Option<UsageWindow>,
    pub secondary: Option<UsageWindow>,
    #[serde(default)]
    pub tertiary: Option<UsageWindow>,
    // Accept snake_case too, so third-party codexbar ports (e.g. Win-CodexBar,
    // which serializes snake_case) parse alongside the upstream camelCase CLI.
    #[serde(default, alias = "updated_at")]
    pub updated_at: Option<String>,
    #[serde(default, alias = "login_method")]
    pub login_method: Option<String>,
    #[serde(default, alias = "extra_rate_windows")]
    pub extra_rate_windows: Vec<ExtraRateWindow>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtraRateWindow {
    pub id: Option<String>,
    pub title: Option<String>,
    pub window: Option<UsageWindow>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    // `alias` accepts snake_case (Win-CodexBar); `de_opt_percent` also tolerates
    // a float percent like `100.0` in addition to an integer.
    #[serde(default, alias = "used_percent", deserialize_with = "de_opt_percent")]
    pub used_percent: Option<u8>,
    #[serde(default, alias = "reset_description")]
    pub reset_description: Option<String>,
    #[serde(default, alias = "resets_at")]
    pub resets_at: Option<String>,
    #[serde(default, alias = "window_minutes")]
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
    /// live fetch failed. Set by `fetch_all_providers`, not by codexbar.
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

/// The type of authentication a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    /// OAuth-based providers (codex, claude) - use `--source oauth`
    OAuth,
    /// API key providers (zai, kimik2, etc.) - use `--source api` with env var
    Api,
}

/// Information about a supported provider.
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub name: &'static str,
    pub provider_type: ProviderType,
    /// Environment variable name for API key (only for Api type)
    pub env_var: Option<&'static str>,
    pub label: &'static str,
}

/// Registry of all supported providers.
pub const PROVIDERS: &[ProviderInfo] = &[
    // OAuth providers
    ProviderInfo {
        name: "codex",
        provider_type: ProviderType::OAuth,
        env_var: None,
        label: "Codex",
    },
    ProviderInfo {
        name: "claude",
        provider_type: ProviderType::OAuth,
        env_var: None,
        label: "Claude",
    },
    // API providers
    ProviderInfo {
        name: "zai",
        provider_type: ProviderType::Api,
        env_var: Some("ZAI_API_TOKEN"),
        label: "z.ai",
    },
    ProviderInfo {
        name: "kimik2",
        provider_type: ProviderType::Api,
        env_var: Some("KIMI_K2_API_KEY"),
        label: "Kimi K2",
    },
    ProviderInfo {
        name: "copilot",
        provider_type: ProviderType::Api,
        env_var: Some("COPILOT_API_TOKEN"),
        label: "Copilot",
    },
    ProviderInfo {
        name: "minimax",
        provider_type: ProviderType::Api,
        env_var: Some("MINIMAX_API_TOKEN"),
        label: "MiniMax",
    },
    ProviderInfo {
        name: "kimi",
        provider_type: ProviderType::Api,
        env_var: Some("KIMI_AUTH_TOKEN"),
        label: "Kimi",
    },
];

/// Get provider info by name.
pub fn get_provider_info(name: &str) -> Option<&'static ProviderInfo> {
    PROVIDERS.iter().find(|p| p.name == name)
}

/// Get the display label for a provider.
pub fn provider_label(name: &str) -> &str {
    get_provider_info(name).map(|p| p.label).unwrap_or(name)
}

// ============================================================================
// Configuration Types
// ============================================================================

/// Configuration for an API provider (requires api_key).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApiProviderConfig {
    pub api_key: String,
}

/// Provider configuration section.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ProvidersConfig {
    // OAuth providers - just true/false
    pub codex: Option<bool>,
    pub claude: Option<bool>,
    // API providers - struct with api_key
    pub zai: Option<ApiProviderConfig>,
    pub kimik2: Option<ApiProviderConfig>,
    pub copilot: Option<ApiProviderConfig>,
    pub minimax: Option<ApiProviderConfig>,
    pub kimi: Option<ApiProviderConfig>,
}

/// An enabled provider with its configuration.
#[derive(Debug, Clone)]
pub struct EnabledProvider {
    pub name: String,
    pub provider_type: ProviderType,
    pub api_key: Option<String>,
    pub env_var: Option<&'static str>,
}

impl ProvidersConfig {
    /// Get list of all enabled providers with their configuration.
    pub fn enabled_providers(&self) -> Vec<EnabledProvider> {
        let mut enabled = Vec::new();

        // OAuth providers
        if self.codex.unwrap_or(false) {
            enabled.push(EnabledProvider {
                name: "codex".to_string(),
                provider_type: ProviderType::OAuth,
                api_key: None,
                env_var: None,
            });
        }
        if self.claude.unwrap_or(false) {
            enabled.push(EnabledProvider {
                name: "claude".to_string(),
                provider_type: ProviderType::OAuth,
                api_key: None,
                env_var: None,
            });
        }

        // API providers - enabled if api_key is present
        if let Some(ref config) = self.zai {
            enabled.push(EnabledProvider {
                name: "zai".to_string(),
                provider_type: ProviderType::Api,
                api_key: Some(config.api_key.clone()),
                env_var: Some("ZAI_API_TOKEN"),
            });
        }
        if let Some(ref config) = self.kimik2 {
            enabled.push(EnabledProvider {
                name: "kimik2".to_string(),
                provider_type: ProviderType::Api,
                api_key: Some(config.api_key.clone()),
                env_var: Some("KIMI_K2_API_KEY"),
            });
        }
        if let Some(ref config) = self.copilot {
            enabled.push(EnabledProvider {
                name: "copilot".to_string(),
                provider_type: ProviderType::Api,
                api_key: Some(config.api_key.clone()),
                env_var: Some("COPILOT_API_TOKEN"),
            });
        }
        if let Some(ref config) = self.minimax {
            enabled.push(EnabledProvider {
                name: "minimax".to_string(),
                provider_type: ProviderType::Api,
                api_key: Some(config.api_key.clone()),
                env_var: Some("MINIMAX_API_TOKEN"),
            });
        }
        if let Some(ref config) = self.kimi {
            enabled.push(EnabledProvider {
                name: "kimi".to_string(),
                provider_type: ProviderType::Api,
                api_key: Some(config.api_key.clone()),
                env_var: Some("KIMI_AUTH_TOKEN"),
            });
        }

        enabled
    }

    /// Check if a provider is enabled (used for filtering payloads).
    pub fn is_enabled(&self, provider: &str) -> bool {
        match provider {
            "codex" => self.codex.unwrap_or(false),
            "claude" => self.claude.unwrap_or(false),
            "zai" => self.zai.is_some(),
            "kimik2" => self.kimik2.is_some(),
            "copilot" => self.copilot.is_some(),
            "minimax" => self.minimax.is_some(),
            "kimi" => self.kimi.is_some(),
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
    /// What happens on left-click on the waybar module:
    /// "tui" launches the terminal TUI, "popover" runs `popover_command`
    /// (defaults to the bundled GTK4 panel).
    pub click_action: ClickAction,
    /// Shell command used when `click_action = "tui"`. Empty = auto-detect
    /// (omarchy-launch-or-focus-tui if available, else $TERMINAL -e tokengauge-tui).
    pub tui_command: String,
    /// Shell command used when `click_action = "popover"`. Defaults to the
    /// bundled `tokengauge-popover --toggle`.
    pub popover_command: String,
    /// Top-edge offset in pixels for the bundled `tokengauge-popover` window.
    pub popover_margin_top: i32,
    /// Side-edge (left/right matching `placement`) offset in pixels.
    pub popover_margin_side: i32,
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
            popover_command: "tokengauge-popover --toggle".to_string(),
            // Top edge: 0 sits flush under waybar when waybar reserves its
            // own exclusive zone; bump up if you want a gap.
            popover_margin_top: 4,
            popover_margin_side: 8,
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
    Popover,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TokenGaugeConfig {
    pub codexbar_bin: String,
    pub refresh_secs: u64,
    pub cache_file: PathBuf,
    /// Timeout in seconds for each provider request
    pub timeout_secs: u64,
    /// Delay in milliseconds between consecutive provider fetch starts. Spreads
    /// out codexbar calls to avoid rate-limit (429) bursts when many providers
    /// are enabled. 0 disables staggering (all providers fetched at once).
    pub stagger_ms: u64,
    /// Enable ccusage cost fetching (requires `npx ccusage`)
    pub ccusage_enabled: bool,
    /// Timeout in seconds for each ccusage call
    pub ccusage_timeout_secs: u64,
    pub providers: ProvidersConfig,
    pub waybar: WaybarConfig,
    pub notifications: NotificationsConfig,
    pub theme: ThemeConfig,
    pub update: UpdateConfig,
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
    /// automatic - the user triggers `tokengauge-waybar --update`.
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
            codexbar_bin: "codexbar".to_string(),
            refresh_secs: 600,
            cache_file: default_cache_file(),
            timeout_secs: 20,
            stagger_ms: 0,
            ccusage_enabled: true,
            ccusage_timeout_secs: 15,
            providers: ProvidersConfig {
                codex: Some(true),
                claude: Some(true),
                ..Default::default()
            },
            waybar: WaybarConfig::default(),
            notifications: NotificationsConfig::default(),
            theme: ThemeConfig::default(),
            update: UpdateConfig::default(),
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

/// Clean up error messages to extract the meaningful part.
/// Removes JSON log prefixes and extracts key error info.
fn clean_error_message(raw: &str) -> String {
    // If it's a codexbar failure with JSON in stderr, try to extract the actual error
    if raw.contains("codexbar failed") {
        // Try to find API error messages like "401: {\"error\":\"Unauthorized\"}"
        if let Some(api_error) = extract_api_error(raw) {
            return api_error;
        }
        // Try to find "No available fetch strategy" errors
        if raw.contains("No available fetch strategy") {
            return "No available fetch strategy".to_string();
        }
        // Try to extract message from JSON payload error
        if let Some(msg) = extract_json_message(raw) {
            return msg;
        }
        // Default: just say it failed
        return "API request failed".to_string();
    }

    // If it's a timeout
    if raw.contains("timeout") {
        return "Request timed out".to_string();
    }

    // Clean up codexbar API error messages like "Kimi K2 API returned 401: {\"error\":..."
    if raw.contains("API returned") || raw.contains("API error") {
        if let Some(api_error) = extract_api_error(raw) {
            return api_error;
        }
        // Extract just the status part
        if let Some(status) = extract_http_status(raw) {
            return format!("API error ({})", status);
        }
    }

    // If message is reasonably short, use it as-is
    if raw.len() <= 60 {
        return raw.to_string();
    }

    // Truncate long messages
    format!("{}...", &raw[..57])
}

/// Try to extract API error like "Unauthorized" or "Invalid API key"
fn extract_api_error(raw: &str) -> Option<String> {
    // Look for patterns like: API returned 401: {"error":"Unauthorized"}
    // Or: Kimi K2 API error: {"error":"Unauthorized"}
    if let Some(idx) = raw.find("\"error\":\"") {
        let start = idx + 9;
        if let Some(end) = raw[start..].find('"') {
            let error = &raw[start..start + end];
            // Look for HTTP status code
            if let Some(status) = extract_http_status(raw) {
                return Some(format!("{} (HTTP {})", error, status));
            }
            return Some(error.to_string());
        }
    }
    None
}

/// Extract HTTP status code from error message
fn extract_http_status(raw: &str) -> Option<&'static str> {
    // Look for patterns like "returned 401:" or "status: 401)"
    ["401", "403", "404", "500", "502", "503"]
        .iter()
        .find(|&pattern| raw.contains(pattern))
        .copied()
}

/// Try to extract "message" field from JSON in error
fn extract_json_message(raw: &str) -> Option<String> {
    // Look for "message":"..." pattern
    if let Some(idx) = raw.find("\"message\":\"") {
        let start = idx + 11;
        if let Some(end) = raw[start..].find('"') {
            let msg = &raw[start..start + end];
            if !msg.is_empty() && msg.len() <= 80 {
                return Some(msg.to_string());
            }
        }
    }
    None
}

/// Result of fetching all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResult {
    pub payloads: Vec<ProviderPayload>,
    pub errors: Vec<ProviderFetchError>,
    #[serde(default)]
    pub costs: HashMap<String, CostInfo>,
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
}

impl CostInfo {
    /// Average hourly cost over the available days of history.
    /// Returns None if history is empty or sum is zero.
    pub fn avg_hourly_cost(&self) -> Option<f64> {
        if self.weekly_cost_history.is_empty() {
            return None;
        }
        let sum: f64 = self.weekly_cost_history.iter().sum();
        if sum <= 0.0 {
            return None;
        }
        let hours = self.weekly_cost_history.len() as f64 * 24.0;
        Some(sum / hours)
    }
}

/// Per-model cost slice (ccusage modelBreakdowns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub model: String,
    pub usd: f64,
    pub tokens: u64,
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
    pub weekly_used: Option<u8>,
    pub weekly_window_minutes: Option<u32>,
    pub weekly_reset: String,
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
    if config.codexbar_bin.is_empty() {
        config.codexbar_bin = "codexbar".to_string();
    }
    if config.cache_file.as_os_str().is_empty() {
        config.cache_file = default_cache_file();
    }
    if config.refresh_secs == 0 {
        config.refresh_secs = 600;
    }

    Ok(config)
}

/// Default cache file location. Uses the platform temp dir so it resolves to
/// `%TEMP%` on Windows and `/tmp` on Unix (preserving the previous behaviour on
/// Linux, since `std::env::temp_dir()` is `/tmp` there).
pub fn default_cache_file() -> PathBuf {
    std::env::temp_dir().join("tokengauge-usage.json")
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

/// Secondary codexbar `--source` to retry with when the primary source fails.
/// Anthropic's OAuth endpoint rate-limits often; the local Claude CLI source
/// keeps the bar populated. Only providers codexbar exposes a `cli` source for
/// qualify (currently just Claude).
fn cli_fallback_source(provider: &EnabledProvider) -> Option<&'static str> {
    match provider.name.as_str() {
        "claude" => Some("cli"),
        _ => None,
    }
}

/// Fetch a single provider using codexbar, retrying via a fallback `--source`
/// when the primary source errors (e.g. Claude OAuth 429 → local CLI).
pub fn fetch_single_provider(
    codexbar_bin: &str,
    provider: &EnabledProvider,
    timeout: Duration,
) -> Result<Vec<ProviderPayload>> {
    let primary = match provider.provider_type {
        ProviderType::OAuth => "oauth",
        ProviderType::Api => "api",
    };

    let result = fetch_single_provider_source(codexbar_bin, provider, primary, timeout);
    if payloads_usable(&result) {
        return result;
    }

    if let Some(fallback) = cli_fallback_source(provider) {
        let retried = fetch_single_provider_source(codexbar_bin, provider, fallback, timeout);
        if payloads_usable(&retried) {
            return retried;
        }
    }

    result
}

/// True when a fetch result carries at least one usable (error-free) payload.
fn payloads_usable(result: &Result<Vec<ProviderPayload>>) -> bool {
    matches!(result, Ok(payloads) if payloads.iter().any(|p| !p.has_error()))
}

/// Build the base `Command` for the codexbar binary.
///
/// A plain executable (`.exe`, or a name CreateProcess resolves) is spawned
/// directly. On Windows a `.cmd`/`.bat` shim can't be exec'd directly, so those
/// are routed through `cmd /C` - mirroring the ccusage handling and letting a
/// batch wrapper work as `codexbar_bin`.
fn codexbar_command(codexbar_bin: &str) -> Command {
    #[cfg(windows)]
    {
        let is_batch = |p: &Path| {
            matches!(
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("cmd") | Some("bat")
            )
        };
        let resolved = find_in_path(codexbar_bin);
        if is_batch(Path::new(codexbar_bin)) || resolved.as_deref().map(is_batch).unwrap_or(false) {
            let mut command = Command::new("cmd");
            command.arg("/C").arg(codexbar_bin);
            return command;
        }
    }
    Command::new(codexbar_bin)
}

/// Fetch a single provider from a specific codexbar `--source`.
fn fetch_single_provider_source(
    codexbar_bin: &str,
    provider: &EnabledProvider,
    source: &str,
    timeout: Duration,
) -> Result<Vec<ProviderPayload>> {
    let mut command = codexbar_command(codexbar_bin);
    command
        .arg("usage")
        .arg("--provider")
        .arg(&provider.name)
        .arg("--source")
        .arg(source)
        .arg("--format")
        .arg("json");

    // The upstream (macOS/Linux) codexbar CLI takes `--json-only` to suppress
    // any non-JSON preamble. On Windows there is no upstream binary; the only
    // codexbar-compatible options are third-party ports (e.g. Win-CodexBar)
    // whose `usage` command doesn't define that flag - and clap rejects unknown
    // flags - while their `--format json` already emits pure JSON. So only pass
    // `--json-only` off Windows, which keeps Win-CodexBar usable as a drop-in.
    #[cfg(not(windows))]
    command.arg("--json-only");

    // Set API key environment variable if needed
    if let (Some(api_key), Some(env_var)) = (&provider.api_key, provider.env_var) {
        command.env(env_var, api_key);
    }

    let provider_name = provider.name.clone();
    let output = run_with_timeout(command, timeout)
        .with_context(|| format!("failed to run codexbar for {provider_name}"))?;

    if !output.status.success() {
        // Try to parse JSON error from stdout first
        if let Ok(payloads) = parse_payload_bytes(&output.stdout) {
            // Codexbar returns non-zero but still outputs JSON with error info
            return Ok(payloads);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "no error output".to_string()
        };
        return Err(anyhow!("codexbar failed ({}) - {}", output.status, detail));
    }

    parse_payload_bytes(&output.stdout)
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
        };
    }

    let ccusage_enabled = config.ccusage_enabled;
    let ccusage_timeout = Duration::from_secs(config.ccusage_timeout_secs.max(1));
    let ccusage_handle = thread::spawn(move || {
        if ccusage_enabled {
            fetch_ccusage_costs(ccusage_timeout)
        } else {
            HashMap::new()
        }
    });

    // Spawn threads for each provider. Each thread self-delays by its index
    // times `stagger_ms` so codexbar calls are spread out (rate-limit relief)
    // without blocking the main spawn loop or the ccusage thread.
    let stagger = Duration::from_millis(config.stagger_ms);
    let handles: Vec<_> = enabled
        .into_iter()
        .enumerate()
        .map(|(i, provider)| {
            let bin = config.codexbar_bin.clone();
            let provider_name = provider.name.clone();
            thread::spawn(move || {
                if !stagger.is_zero() && i > 0 {
                    thread::sleep(stagger.saturating_mul(i as u32));
                }
                let result = fetch_single_provider(&bin, &provider, timeout);
                (provider_name, result)
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

    let costs = ccusage_handle.join().unwrap_or_default();
    FetchResult {
        payloads,
        errors,
        costs,
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
        serde_json::from_slice(bytes).context("codexbar output was not JSON")?;
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

    if let Some(usage) = payload.usage {
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
                let (used, _, reset) = format_window(w.window);
                Some(ExtraWindowRow { title, used, reset })
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
        weekly_used,
        weekly_window_minutes: weekly_window,
        weekly_reset,
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
pub fn write_cache_full(
    path: &Path,
    payloads: &[ProviderPayload],
    errors: &[ProviderFetchError],
    costs: &HashMap<String, CostInfo>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let data = CachedData::Full {
        payloads: payloads.to_vec(),
        errors: errors.to_vec(),
        costs: costs.clone(),
    };
    let contents = serde_json::to_string(&data)?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write cache {}", path.display()))?;
    Ok(())
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
    let is_enabled = |name: &str| {
        enabled
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(name.trim()))
    };
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

/// TTL guard so a worker killed mid-fetch can't strand the ⟳ state forever.
const REFRESH_SENTINEL_TTL: Duration = Duration::from_secs(30);

/// True while a manual refresh is in flight (sentinel present and not stale).
pub fn refresh_in_progress(sentinel: &Path) -> bool {
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
pub fn write_cache(path: &Path, payloads: &[ProviderPayload]) -> Result<()> {
    write_cache_full(path, payloads, &[], &HashMap::new())
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
        "copilot" => ProviderIcon {
            glyph: "\u{f4b8}",
            color_hex: "#8b5cf6",
        },
        "z.ai" | "zai" => ProviderIcon {
            glyph: "Z",
            color_hex: "#126EF4",
        },
        _ => ProviderIcon {
            glyph: "\u{f06a9}",
            color_hex: NEUTRAL_HEX,
        },
    }
}

/// Basename slug of the bundled brand SVG for a provider label, if one ships.
/// Kimi K2 reuses the Kimi (Moonshot) mark.
pub fn provider_icon_slug(label: &str) -> Option<&'static str> {
    Some(match label.to_lowercase().as_str() {
        "claude" => "claude",
        "codex" => "codex",
        "copilot" => "copilot",
        "z.ai" | "zai" => "zai",
        "minimax" => "minimax",
        "kimi" | "kimi k2" | "kimik2" => "kimi",
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
/// (the popover then falls back to the glyph icon).
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
        "copilot" => ProviderUrls {
            dashboard: Some("https://github.com/settings/copilot"),
            status: Some("https://www.githubstatus.com"),
        },
        "z.ai" | "zai" => ProviderUrls {
            dashboard: Some("https://z.ai/manage-apikey"),
            status: Some("https://status.z.ai"),
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
}

/// Cached result of the last GitHub release check. Written by the waybar
/// binary (which owns the network code) and read by the GUIs so opening the
/// popover/plasmoid never triggers a network call.
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

/// Pure decision logic: given current pct and previously-notified thresholds,
/// returns (thresholds_to_fire, updated_notified_list).
///
/// Reset: if pct dropped 10+ points below the highest previously-notified
/// threshold, treat as window roll-over and clear the notified list before
/// considering thresholds. Avoids needing to track raw reset timestamps.
pub fn thresholds_to_fire(pct: u8, thresholds: &[u8], notified: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut current = notified.to_vec();
    if let Some(&max_notified) = current.iter().max()
        && (pct + 10) < max_notified
    {
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
    } else {
        None
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CcusageDailyResponse {
    #[serde(default)]
    daily: Vec<CcusageDay>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CcusageDay {
    #[serde(default)]
    model_breakdowns: Vec<CcusageModelBreakdown>,
    #[serde(default)]
    period: String,
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

fn ccusage_total_tokens(b: &CcusageModelBreakdown) -> u64 {
    b.input_tokens + b.output_tokens + b.cache_creation_tokens + b.cache_read_tokens
}

struct AggregatedProvider {
    total_usd: f64,
    total_tokens: u64,
    /// per-model: model_name -> (usd, tokens)
    models: HashMap<String, (f64, u64)>,
}

impl AggregatedProvider {
    fn into_model_costs(self) -> (f64, u64, Vec<ModelCost>) {
        let mut models: Vec<ModelCost> = self
            .models
            .into_iter()
            .map(|(model, (usd, tokens))| ModelCost { model, usd, tokens })
            .collect();
        models.sort_by(|a, b| {
            b.usd
                .partial_cmp(&a.usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        (self.total_usd, self.total_tokens, models)
    }
}

/// Last `n` days of cost per provider, oldest first. Pads with 0.0 for any
/// days missing from the response so the sparkline has consistent length.
fn last_n_days_by_provider(response: &CcusageDailyResponse, n: usize) -> HashMap<String, Vec<f64>> {
    // (provider, period) -> usd
    let mut per_day: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut periods: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for day in &response.daily {
        if day.period.is_empty() {
            continue;
        }
        periods.insert(day.period.clone());
        for b in &day.model_breakdowns {
            if let Some(provider) = model_to_provider(&b.model_name) {
                *per_day
                    .entry(provider.to_string())
                    .or_default()
                    .entry(day.period.clone())
                    .or_insert(0.0) += b.cost;
            }
        }
    }
    let periods: Vec<String> = periods.into_iter().rev().take(n).collect();
    let periods: Vec<String> = periods.into_iter().rev().collect();
    per_day
        .into_iter()
        .map(|(provider, days)| {
            let series: Vec<f64> = periods
                .iter()
                .map(|p| days.get(p).copied().unwrap_or(0.0))
                .collect();
            (provider, series)
        })
        .collect()
}

fn aggregate_ccusage(response: &CcusageDailyResponse) -> HashMap<String, AggregatedProvider> {
    let mut totals: HashMap<String, AggregatedProvider> = HashMap::new();
    for day in &response.daily {
        for b in &day.model_breakdowns {
            if let Some(provider) = model_to_provider(&b.model_name) {
                let entry =
                    totals
                        .entry(provider.to_string())
                        .or_insert_with(|| AggregatedProvider {
                            total_usd: 0.0,
                            total_tokens: 0,
                            models: HashMap::new(),
                        });
                let tokens = ccusage_total_tokens(b);
                entry.total_usd += b.cost;
                entry.total_tokens += tokens;
                let model_entry = entry.models.entry(b.model_name.clone()).or_insert((0.0, 0));
                model_entry.0 += b.cost;
                model_entry.1 += tokens;
            }
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

fn run_ccusage_blocks(timeout: Duration) -> Result<CcusageBlocksResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.arg("blocks").arg("--active").arg("--json");
    let output = run_with_timeout(command, timeout).context("ccusage blocks failed")?;
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

fn fetch_active_blocks(timeout: Duration) -> HashMap<String, ActiveBlockInfo> {
    let resp = match run_ccusage_blocks(timeout) {
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

fn run_ccusage(args: &[&str], timeout: Duration) -> Result<CcusageDailyResponse> {
    let runner = resolve_ccusage_runner().ok_or_else(|| anyhow!("no ccusage runner on PATH"))?;
    let mut command = ccusage_command(&runner);
    command.args(args).arg("--json");
    let output = run_with_timeout(command, timeout).context("ccusage failed")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("ccusage exited non-zero: {}", stderr.trim()));
    }

    serde_json::from_slice(&output.stdout).context("ccusage output was not valid JSON")
}

/// Fetch ccusage cost info. Returns a map from provider key (claude/codex) to CostInfo.
/// Returns empty map on any failure (ccusage missing, no logs, parse error).
pub fn fetch_ccusage_costs(timeout: Duration) -> HashMap<String, CostInfo> {
    let today = Local::now().format("%Y%m%d").to_string();
    let month_start = Local::now().format("%Y%m01").to_string();

    let daily = match run_ccusage(&["daily", "--since", &today], timeout) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    let monthly = match run_ccusage(&["daily", "--since", &month_start], timeout) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };

    let mut today_agg = aggregate_ccusage(&daily);
    let mut monthly_agg = aggregate_ccusage(&monthly);
    let mut active_blocks = fetch_active_blocks(timeout);
    let mut weekly_history = last_n_days_by_provider(&monthly, 7);

    let mut result = HashMap::new();
    let providers: std::collections::HashSet<String> = today_agg
        .keys()
        .chain(monthly_agg.keys())
        .chain(active_blocks.keys())
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
        let weekly_cost_history = weekly_history.remove(&provider).unwrap_or_default();
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

# Path to codexbar binary
codexbar_bin = "codexbar"

# Refresh interval in seconds
refresh_secs = 600

# Cache file location
cache_file = "/tmp/tokengauge-usage.json"

# Delay in milliseconds between provider fetch starts. Spreads out codexbar
# calls to avoid rate-limit (429) bursts when several providers are enabled.
# 0 = fetch all at once (fastest, default).
stagger_ms = 0

# Enable ccusage cost fetching (requires `npx ccusage` to be available)
ccusage_enabled = true
# Timeout in seconds for each ccusage call (cold starts can be slow)
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
# Left-click action: "tui" opens the terminal TUI, "popover" runs
# popover_command (defaults to the bundled GTK4 panel).
click_action = "tui"
# Optional explicit launcher for click_action = "tui". Empty = auto-detect
# (omarchy-launch-or-focus-tui if present, else $TERMINAL -e tokengauge-tui).
# tui_command = "ghostty -e tokengauge-tui"
# Shell command used when click_action = "popover".
popover_command = "tokengauge-popover --toggle"

[providers]
# OAuth providers - set to true/false to enable/disable
codex = true
claude = true

# API providers - uncomment and add your API key to enable
# [providers.zai]
# api_key = "your-zai-api-key"

# [providers.kimik2]
# api_key = "your-kimi-k2-api-key"

# [providers.copilot]
# api_key = "your-copilot-api-key"

# [providers.minimax]
# api_key = "your-minimax-api-key"

# [providers.kimi]
# api_key = "your-kimi-api-key"
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

/// Ask a running TokenGauge daemon (`tokengauge-waybar --daemon`) to reload its
/// config from disk without a restart. No-op when no daemon is running.
///
/// Matches the full command line: the 17-char binary name exceeds procps'
/// 15-char comm cap, so a bare `pkill tokengauge-waybar` matches nothing. The
/// `--daemon` fragment also keeps us from signalling the short-lived one-shot
/// invocation that triggered the edit (it has no SIGHUP handler).
pub fn signal_daemon_reload() {
    let _ = Command::new("pkill")
        .arg("-HUP")
        .arg("-f")
        .arg("tokengauge-waybar --daemon")
        .status();
}

/// Enable/disable an OAuth provider (codex, claude) in the config file.
pub fn config_set_oauth_provider(path: &Path, name: &str, enabled: bool) -> Result<()> {
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
    use super::*;

    // ------------------------------------------------------------------------
    // Third-party codexbar (Win-CodexBar) snake_case + float-percent parsing
    // ------------------------------------------------------------------------

    #[test]
    fn parses_win_codexbar_snake_case_usage() {
        // Real output from Win-CodexBar's codexbar-cli: snake_case field names
        // and a float `used_percent`, unlike upstream codexbar's camelCase/int.
        let json = br#"[{"cost":null,"provider":"claude","source":"oauth","usage":{"extra_rate_windows":[{"id":"claude-weekly-scoped-fable","title":"Fable only","window":{"used_percent":0.0,"window_minutes":10080}}],"login_method":"Claude Max 5x","primary":{"reset_description":"Jul 14 at 8:49PM","resets_at":"2026-07-14T20:49:59.922430Z","used_percent":100.0,"window_minutes":300},"secondary":{"reset_description":"Jul 18 at 9:59AM","resets_at":"2026-07-18T09:59:59.922452Z","used_percent":21.0,"window_minutes":10080},"updated_at":"2026-07-14T15:57:20.912196200Z"}}]"#;

        let payloads = parse_payload_bytes(json).expect("Win-CodexBar JSON should parse");
        assert_eq!(payloads.len(), 1);
        let usage = payloads[0].usage.as_ref().expect("usage present");
        let primary = usage.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, Some(100));
        // Every snake_case alias must map, not just the percentages.
        assert_eq!(
            primary.reset_description.as_deref(),
            Some("Jul 14 at 8:49PM")
        );
        assert_eq!(
            primary.resets_at.as_deref(),
            Some("2026-07-14T20:49:59.922430Z")
        );
        assert_eq!(primary.window_minutes, Some(300));
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, Some(21));
        assert_eq!(
            usage.updated_at.as_deref(),
            Some("2026-07-14T15:57:20.912196200Z")
        );
        assert_eq!(usage.login_method.as_deref(), Some("Claude Max 5x"));
        assert_eq!(usage.extra_rate_windows.len(), 1);

        let rows = payload_to_rows(payloads);
        assert_eq!(rows[0].session_used, Some(100));
        assert_eq!(rows[0].weekly_used, Some(21));
    }

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

    #[cfg(windows)]
    #[test]
    fn codexbar_command_routes_batch_shim_through_cmd() {
        // A .cmd/.bat shim must run via `cmd /C`...
        let command = codexbar_command("codexbar.cmd");
        assert_eq!(command.get_program().to_string_lossy(), "cmd");
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["/C", "codexbar.cmd"]);

        // ...while a plain .exe is spawned directly.
        let direct = codexbar_command("codexbar-cli.exe");
        assert_eq!(direct.get_program().to_string_lossy(), "codexbar-cli.exe");
        assert_eq!(direct.get_args().count(), 0);
    }

    // ------------------------------------------------------------------------
    // fallback source tests
    // ------------------------------------------------------------------------

    fn enabled(name: &str, ty: ProviderType) -> EnabledProvider {
        EnabledProvider {
            name: name.to_string(),
            provider_type: ty,
            api_key: None,
            env_var: None,
        }
    }

    #[test]
    fn cli_fallback_only_for_claude() {
        assert_eq!(
            cli_fallback_source(&enabled("claude", ProviderType::OAuth)),
            Some("cli")
        );
        assert_eq!(
            cli_fallback_source(&enabled("codex", ProviderType::OAuth)),
            None
        );
        assert_eq!(
            cli_fallback_source(&enabled("zai", ProviderType::Api)),
            None
        );
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

    #[test]
    fn payloads_usable_detects_error_only_results() {
        let ok = ProviderPayload {
            provider: "claude".into(),
            version: None,
            source: None,
            usage: None,
            credits: None,
            error: None,
            stale: false,
        };
        let err = ProviderPayload {
            error: Some(ProviderError {
                message: Some("429".into()),
                code: None,
                kind: None,
            }),
            ..ok.clone()
        };
        assert!(payloads_usable(&Ok(vec![ok.clone()])));
        assert!(payloads_usable(&Ok(vec![err.clone(), ok])));
        assert!(!payloads_usable(&Ok(vec![err])));
        assert!(!payloads_usable(&Ok(vec![])));
        assert!(!payloads_usable(&Err(anyhow!("boom"))));
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
        assert_eq!(provider_label("zai"), "z.ai");
        assert_eq!(provider_label("kimik2"), "Kimi K2");
    }

    #[test]
    fn provider_label_unknown_returns_input() {
        assert_eq!(provider_label("unknown_provider"), "unknown_provider");
    }

    // ------------------------------------------------------------------------
    // get_provider_info tests
    // ------------------------------------------------------------------------

    #[test]
    fn get_provider_info_oauth_provider() {
        let info = get_provider_info("claude").unwrap();
        assert_eq!(info.name, "claude");
        assert_eq!(info.provider_type, ProviderType::OAuth);
        assert!(info.env_var.is_none());
    }

    #[test]
    fn get_provider_info_api_provider() {
        let info = get_provider_info("zai").unwrap();
        assert_eq!(info.name, "zai");
        assert_eq!(info.provider_type, ProviderType::Api);
        assert_eq!(info.env_var, Some("ZAI_API_TOKEN"));
    }

    #[test]
    fn get_provider_info_unknown() {
        assert!(get_provider_info("nonexistent").is_none());
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
        assert!(enabled.iter().any(|p| p.name == "codex"));
        assert!(enabled.iter().any(|p| p.name == "claude"));
    }

    #[test]
    fn providers_config_enabled_with_api_provider() {
        let config = ProvidersConfig {
            claude: Some(true),
            zai: Some(ApiProviderConfig {
                api_key: "test-key".to_string(),
            }),
            ..Default::default()
        };
        let enabled = config.enabled_providers();
        assert_eq!(enabled.len(), 2);

        let zai = enabled.iter().find(|p| p.name == "zai").unwrap();
        assert_eq!(zai.api_key, Some("test-key".to_string()));
        assert_eq!(zai.env_var, Some("ZAI_API_TOKEN"));
    }

    #[test]
    fn providers_config_disabled_oauth() {
        let config = ProvidersConfig {
            codex: Some(false),
            claude: Some(true),
            ..Default::default()
        };
        let enabled = config.enabled_providers();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].name, "claude");
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
            zai: Some(ApiProviderConfig {
                api_key: "key".to_string(),
            }),
            ..Default::default()
        };
        assert!(config.is_enabled("codex"));
        assert!(!config.is_enabled("claude"));
        assert!(config.is_enabled("zai"));
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
            payloads: vec![payload.clone()],
            errors: vec![error.clone()],
            costs: HashMap::new(),
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
    fn provider_fetch_error_api_401() {
        let raw = r#"codexbar failed (exit status: 1) - {"error":"Unauthorized"}"#;
        let error = ProviderFetchError::new("kimik2".to_string(), raw);
        assert!(error.message.contains("Unauthorized"));
    }

    #[test]
    fn provider_fetch_error_no_fetch_strategy() {
        let raw = "codexbar failed - No available fetch strategy for provider";
        let error = ProviderFetchError::new("test".to_string(), raw);
        assert_eq!(error.message, "No available fetch strategy");
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
        assert!(error.message.len() <= 60);
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
        assert_eq!(config.codexbar_bin, "codexbar");
        assert_eq!(config.refresh_secs, 600);
        assert!(config.providers.codex.unwrap_or(false));
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
            },
        );
        assert!(lookup_cost("Claude", &costs).is_some());
        assert!(lookup_cost("claude-code", &costs).is_some());
        assert!(lookup_cost("CLAUDE", &costs).is_some());
        assert!(lookup_cost("zai", &costs).is_none());
    }

    #[test]
    fn thresholds_to_fire_below_no_trigger() {
        let (fire, notified) = thresholds_to_fire(40, &[50, 80, 95], &[]);
        assert!(fire.is_empty());
        assert!(notified.is_empty());
    }

    #[test]
    fn thresholds_to_fire_crosses_50_once() {
        let (fire, notified) = thresholds_to_fire(55, &[50, 80, 95], &[]);
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_already_notified_50_now_at_60() {
        let (fire, notified) = thresholds_to_fire(60, &[50, 80, 95], &[50]);
        assert!(fire.is_empty());
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_jumps_past_two() {
        let (fire, notified) = thresholds_to_fire(82, &[50, 80, 95], &[]);
        assert_eq!(fire, vec![50, 80]);
        assert_eq!(notified, vec![50, 80]);
    }

    #[test]
    fn thresholds_to_fire_resets_on_pct_drop() {
        // notified up to 80, but pct dropped to 5 (window rolled over)
        let (fire, notified) = thresholds_to_fire(5, &[50, 80, 95], &[50, 80]);
        assert!(fire.is_empty());
        assert!(notified.is_empty(), "drop below 80-10=70 must clear");
    }

    #[test]
    fn thresholds_to_fire_resets_then_recrosses() {
        // dropped to 0, then climbed to 60
        let (fire, notified) = thresholds_to_fire(60, &[50, 80, 95], &[50, 80]);
        assert_eq!(fire, vec![50]);
        assert_eq!(notified, vec![50]);
    }

    #[test]
    fn thresholds_to_fire_small_fluctuation_no_reset() {
        // notified 80, pct dipped to 75 (within 10) - shouldn't reset
        let (fire, notified) = thresholds_to_fire(75, &[50, 80], &[50, 80]);
        assert!(fire.is_empty());
        assert_eq!(notified, vec![50, 80]);
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

    fn tempdir_for_test(prefix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        let pid = std::process::id();
        path.push(format!("tokengauge-test-{prefix}-{pid}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
