//! What a fetcher produces and what a cost read produces: the two shapes
//! everything downstream is written against.
//!
//! A `ProviderPayload` is deliberately the same JSON the CodexBar-shaped tools
//! this format came from wrote, which is why it carries fields no fetcher here
//! ever sets - `version` and `error` are read-side only, and `stale` belongs to
//! [`crate::fetch::apply_stale_fallback`], the only thing that knows a fetch
//! failed.
//!
//! Both shapes are serialised into the snapshot, so a field added here has to
//! be `#[serde(default)]` or an existing snapshot stops parsing and a user
//! loses their history.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::*;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
