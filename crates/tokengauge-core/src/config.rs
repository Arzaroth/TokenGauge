//! The config file: its shape, its defaults, and the few edits TokenGauge
//! makes to it on a user's behalf.
//!
//! Every section captures its unrecognised keys with `#[serde(flatten)]` rather
//! than letting serde drop them, so `--doctor` can name a line that no longer
//! does anything - a removed provider, a renamed knob - instead of the user
//! believing a setting is in force.
//!
//! Edits go through [`edit_config_file`], which parses with `toml_edit` and
//! writes back through `write_atomic`: a user's comments and key order survive
//! a `--set-provider`, and two writers cannot leave a half-written file behind.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::*;
use anyhow::anyhow;
use std::process::Command;

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
        keys.extend(
            self.sync
                .dir
                .unknown
                .keys()
                .map(|k| format!("sync.dir.{k}")),
        );
        keys.extend(self.sync.s3.unknown.keys().map(|k| format!("sync.s3.{k}")));
        keys.sort();
        keys
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The transport sections were the gap: a typo in `[sync.s3]` means sync
    /// quietly does not work, which is the one failure the whole unknown-key
    /// mechanism exists to make loud. Both were dropped in silence.
    #[test]
    fn a_typo_in_a_transport_section_is_reported_too() {
        let config: TokenGaugeConfig = toml::from_str(
            "[sync.dir]\npath = \"/tmp/x\"\npth = \"typo\"\n\
             [sync.s3]\nbucket = \"b\"\nendpiont = \"typo\"\n",
        )
        .expect("parses");
        let keys = config.unknown_config_keys();
        assert!(keys.contains(&"sync.dir.pth".to_string()), "{keys:?}");
        assert!(keys.contains(&"sync.s3.endpiont".to_string()), "{keys:?}");
        // The real keys beside them are not mistaken for typos.
        assert_eq!(keys.len(), 2, "{keys:?}");
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
}
