//! Asking every enabled provider at once, and what to do when one does not
//! answer.
//!
//! Each provider is fetched on its own thread behind [`run_with_timeout`],
//! because a hung TLS handshake in one must not hold the bar for the rest, and
//! a panic in one fetcher must not take the process with it.
//!
//! A provider that fails does not go blank. [`apply_stale_fallback`] serves its
//! last good payload with `stale` set, so the bar shows numbers it can say are
//! old rather than an em dash that says nothing. The error rides alongside, so
//! the tooltip can explain the staleness.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::*;
use anyhow::{Context, anyhow};
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;

/// A blocking HTTP client with the per-request timeout wired to the config's
/// `timeout_secs` (the subprocess-kill timeout is gone with codexbar).
pub(crate) fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build HTTP client")
}

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

/// Run a subprocess with a hard timeout. On timeout, kills the child so it
/// does not leak. Captures stdout/stderr in background threads to avoid
/// deadlocking on full pipes.
pub(crate) fn run_with_timeout(mut command: Command, timeout: Duration) -> Result<Output> {
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
                // The error is about to be dropped, and it is the only record
                // of why these figures stopped moving. A cached payload can
                // already carry an older reason; this round's is the true one.
                payload.stale_reason = Some(err.message.clone());
                payload
            }));
            false // drop the error, we have last-good data
        }
    });
}

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
        cost.by_device = store.device_totals(
            provider,
            (month_start, today),
            offset,
            prices,
            local_id,
            None,
        );
        // On a lone machine the split restates the row it hangs off, so the day
        // and model rows only carry one once there is something to attribute.
        if cost.by_device.len() > 1 {
            for day in cost.weekly_history.iter_mut() {
                let Ok(date) = day.date.parse::<chrono::NaiveDate>() else {
                    continue;
                };
                day.by_device =
                    store.device_totals(provider, (date, date), offset, prices, local_id, None);
            }
            for model in cost.monthly_models.iter_mut() {
                let want = model.model.clone();
                model.by_device = store.device_totals(
                    provider,
                    (month_start, today),
                    offset,
                    prices,
                    local_id,
                    Some(&want),
                );
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_stale_fallback_serves_last_good_and_keeps_uncovered_errors() {
        let good_claude = ProviderPayload {
            stale_reason: None,
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
            stale_reason: None,
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
            stale_reason: None,
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
                stale_reason: None,
                provider: "claude".into(),
                version: None,
                source: Some("oauth".into()),
                usage: None,
                credits: None,
                error: None,
                stale: false,
            },
            ProviderPayload {
                stale_reason: None,
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
        // GLM has no reader of its own - it is read only when the plan is
        // driven through Claude Code - so the `auto` fallback is what covers a
        // GLM CLI, and its absence from a native read is not a fault. Kimi and
        // Grok have their own readers now, so for those the same shape *is*
        // drift: it says the reader missed a session ccusage found.
        let d = diagnostics(&[("claude", 100)], &[("claude", 100), ("glm", 900)]);
        let (provider, drift) = d.worst_token_drift().expect("claude is comparable");
        assert_eq!(provider, "claude");
        assert_eq!(drift, 0.0, "glm being ccusage-only must not read as drift");

        // And with nothing comparable at all, there is no verdict to give.
        let empty = diagnostics(&[], &[("glm", 900)]);
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

        // Kimi enabled with nothing any reader found: the machine may be on a
        // wire format this build does not parse, so ccusage is still asked.
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
}
