//! The snapshot: the one record of what every provider last reported, and the
//! single decision about whether to serve it or refetch.
//!
//! It is state, not cache. It holds the only copy of past days' tokens and
//! costs, which is why every path that rewrites it keeps the prior figures when
//! a fetch reads none.
//!
//! [`cache_is_stale`] is that decision, in one place. A snapshot is stale when
//! it is missing, older than `refresh_secs`, **or** was written before a
//! provider that is enabled now was switched on - [`CacheMeta::providers`]
//! records the set each fetch ran with. Age alone was the old rule, and it is
//! why enabling a provider used to do nothing for ten minutes.
//! [`retain_enabled`] handles the other direction, filtering a provider
//! switched off out of a snapshot that is otherwise fine.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::*;
use std::time::Duration;

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
    let Some(written_at) = fs::metadata(&config.cache_file)
        .and_then(|meta| meta.modified())
        .ok()
    else {
        return true;
    };
    let expired = std::time::SystemTime::now()
        .duration_since(written_at)
        .map(|age| age >= Duration::from_secs(config.refresh_secs))
        .unwrap_or(true);
    if expired {
        return true;
    }
    match read_cache_full(&config.cache_file) {
        Ok(cached) => {
            !cached.covers(&config.providers)
                || rolled_over(cached.payloads(), written_at.into(), Utc::now())
        }
        Err(_) => true,
    }
}

/// True when a window this snapshot reported has reset since it was written.
///
/// The percentages beside such a window describe a window that no longer
/// exists, so serving them is wrong however young the snapshot is - and the
/// reset is exactly the moment a user looks. The comparison is against the
/// write, not against now alone: a provider that reports an instant already in
/// the past reports the same one on the next fetch, and asking again on every
/// render would never stop.
fn rolled_over(
    payloads: &[ProviderPayload],
    written_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    payloads
        .iter()
        .filter_map(|payload| payload.usage.as_ref())
        .flat_map(|usage| {
            [&usage.primary, &usage.secondary, &usage.tertiary]
                .into_iter()
                .flatten()
                .chain(
                    usage
                        .extra_rate_windows
                        .iter()
                        .filter_map(|e| e.window.as_ref()),
                )
        })
        .filter_map(|window| window.resets_at.as_deref())
        .filter_map(|iso| DateTime::parse_from_rfc3339(iso).ok())
        .any(|reset| {
            let reset = reset.with_timezone(&Utc);
            reset <= now && reset > written_at
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statefiles::tests::cache_test_dir;

    fn payload_resetting_at(resets_at: Option<&str>) -> ProviderPayload {
        ProviderPayload {
            provider: "claude".into(),
            version: None,
            source: None,
            usage: Some(UsageSnapshot {
                primary: Some(UsageWindow {
                    used_percent: Some(69),
                    reset_description: None,
                    resets_at: resets_at.map(str::to_string),
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
        }
    }

    #[test]
    fn a_window_that_reset_since_the_write_is_stale_however_young_the_snapshot() {
        let now = Utc::now();
        let written_at = now - chrono::Duration::minutes(5);
        let rolled = payload_resetting_at(Some(&(now - chrono::Duration::minutes(2)).to_rfc3339()));
        assert!(rolled_over(&[rolled], written_at, now));

        let pending = payload_resetting_at(Some(&(now + chrono::Duration::hours(2)).to_rfc3339()));
        assert!(!rolled_over(&[pending], written_at, now));
    }

    #[test]
    fn a_reset_time_already_past_at_the_write_never_asks_again() {
        // Otherwise a provider reporting a stale instant would have every
        // render refetch, and every fetch report the same instant.
        let now = Utc::now();
        let written_at = now - chrono::Duration::minutes(5);
        let stuck = payload_resetting_at(Some(&(now - chrono::Duration::hours(1)).to_rfc3339()));
        assert!(!rolled_over(&[stuck], written_at, now));
    }

    #[test]
    fn a_window_with_no_reset_time_never_rolls_over() {
        let now = Utc::now();
        assert!(!rolled_over(
            &[payload_resetting_at(None)],
            now - chrono::Duration::minutes(5),
            now
        ));
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
}
