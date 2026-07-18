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
mod glm;
mod grok;
mod kimi;

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
        "grok" => file_auth_status(grok_auth_path(), "run `grok login` to sign in"),
        "kimi" => {
            let path = kimi_credentials_path();
            // Mirror kimi::resolve_auth, which prefers KIMI_CODE_API_KEY over the CLI file.
            if env_var_present("KIMI_CODE_API_KEY") {
                AuthStatus {
                    ok: true,
                    detail: "KIMI_CODE_API_KEY set".to_string(),
                    hint: "",
                }
            } else if path.exists() {
                AuthStatus {
                    ok: true,
                    detail: format!("{} (kimi CLI)", path.display()),
                    hint: "",
                }
            } else {
                AuthStatus {
                    ok: false,
                    detail: format!("no {} and KIMI_CODE_API_KEY unset", path.display()),
                    hint: "sign in with `kimi` or set KIMI_CODE_API_KEY",
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
    pub refresh_secs: u64,
    pub cache_file: PathBuf,
    /// Timeout in seconds for each provider request
    pub timeout_secs: u64,
    /// Delay in milliseconds between consecutive provider fetch starts. Spreads
    /// out fetches to avoid rate-limit (429) bursts. 0 disables staggering (all
    /// providers fetched at once).
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
            waybar: WaybarConfig::default(),
            notifications: NotificationsConfig::default(),
            theme: ThemeConfig::default(),
            update: UpdateConfig::default(),
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
        "kimi" => ("Weekly", "Rate Limit", "Tertiary"),
        "grok" => ("Weekly", "On-demand", "Tertiary"),
        "glm" => ("Primary", "Secondary", "Tertiary"),
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
        && pct.saturating_add(10) < max_notified
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
            "codexbar_bin = \"codexbar\"\n[providers]\nclaude = true\n\n[providers.zai]\napi_key = \"x\"\n",
        )
        .expect("legacy config still parses");
        assert_eq!(
            config.unknown_config_keys(),
            vec!["codexbar_bin".to_string(), "providers.zai".to_string()]
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
