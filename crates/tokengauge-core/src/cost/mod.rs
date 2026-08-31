//! Native cost and token detail, read from the transcripts the coding CLIs
//! already write.
//!
//! TokenGauge reads the token counts itself and rates them against LiteLLM's
//! price table (see [`pricing`]). ccusage remains available as a second opinion
//! - `--doctor` diffs the two - but is no longer required for a cost figure.
//!
//! The unit every reader produces is a [`UsageEvent`]; everything downstream is
//! aggregation, so a new CLI means one more reader and nothing else.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Days, Duration as ChronoDuration, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BurnRate, CostInfo, DayCost, ModelCost, ProviderPayload, WEEKLY_HISTORY_DAYS, recent_periods,
};

pub mod claude_code;
pub mod codex_cli;
pub mod grok_cli;
pub mod kimi_cli;
pub mod pricing;

/// Where cost figures come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostSource {
    /// Native readers, falling back to ccusage when they find no transcripts at
    /// all - a machine driving a CLI TokenGauge does not parse yet.
    #[default]
    Auto,
    /// Native readers only.
    Native,
    /// The ccusage subprocess only. The escape hatch when a figure looks wrong.
    Ccusage,
}

/// Tokens billed at each rate. Every reader normalises onto this, which is why
/// `input` here means *fresh* input: Codex reports cached tokens inside its
/// input count and Anthropic reports them beside it.
/// Field names are the sync wire format: a contribution is re-uploaded on every
/// change, and these five keys repeat once per bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    #[serde(rename = "i", default, skip_serializing_if = "is_zero")]
    pub input: u64,
    #[serde(rename = "o", default, skip_serializing_if = "is_zero")]
    pub output: u64,
    #[serde(rename = "cw5", default, skip_serializing_if = "is_zero")]
    pub cache_write_5m: u64,
    #[serde(rename = "cw1h", default, skip_serializing_if = "is_zero")]
    pub cache_write_1h: u64,
    #[serde(rename = "cr", default, skip_serializing_if = "is_zero")]
    pub cache_read: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl TokenCounts {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write_5m + self.cache_write_1h + self.cache_read
    }

    pub fn cache_creation(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    pub(crate) fn add(&mut self, other: &TokenCounts) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_read += other.cache_read;
    }
}

/// One billed call.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub provider: &'static str,
    pub model: String,
    /// Local calendar day, which is what "today" means in the panel.
    pub date: NaiveDate,
    /// When the call was billed. The day above is this in local time; the
    /// instant itself is what the session window is measured against.
    pub at: DateTime<Utc>,
    pub tokens: TokenCounts,
    /// The reader's dedup key, kept so a day can be fingerprinted without
    /// re-reading the transcripts. `None` where a record carried no identifier
    /// to build one from.
    pub key: Option<u64>,
}

/// A rated event, kept only long enough to measure the current session window.
#[derive(Debug, Clone, Copy)]
pub struct RecentEvent {
    pub at: DateTime<Utc>,
    pub usd: f64,
    pub tokens: u64,
}

/// A 64-bit digest with a **specified** algorithm.
///
/// `DefaultHasher` is explicitly not stable across Rust releases, and both of
/// this crate's 64-bit keys outlive the run that produced them: a day
/// fingerprint that two machines have to agree on, and the hash of the last
/// published contribution. Two machines on different toolchains would never
/// agree, which would silently disable the double-counting check.
///
/// Each part is length-prefixed so `("ab", "c")` and `("a", "bc")` differ.
pub(crate) fn digest_u64(parts: &[&[u8]]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 is 32 bytes"))
}

/// Stable 64-bit key for a pair of identifiers, used to drop transcript records
/// a resumed session copied forward. Hashed rather than stored whole so the
/// set stays small enough to persist between runs.
pub fn dedup_key(a: &str, b: &str) -> u64 {
    digest_u64(&[a.as_bytes(), b.as_bytes()])
}

/// Every `.jsonl` under `root` that could hold an event dated `since` or later.
///
/// Transcripts are append-only, so a file untouched since before the window
/// cannot contain an event inside it. A day of slack absorbs clock skew and
/// filesystems with coarse timestamps.
pub(crate) fn jsonl_files(root: &Path, since: NaiveDate) -> Vec<PathBuf> {
    let cutoff = since
        .checked_sub_days(Days::new(1))
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|dt| Local.from_local_datetime(&dt).single())
        .map(|dt| UNIX_EPOCH + Duration::from_secs(dt.timestamp().max(0) as u64));

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            if let Some(cutoff) = cutoff
                && let Ok(modified) = entry.metadata().and_then(|m| m.modified())
                && modified < cutoff
            {
                continue;
            }
            files.push(path);
        }
    }
    files.sort();
    files
}

/// What a native read produced, plus what it could not price.
#[derive(Debug, Default)]
pub struct NativeCostReport {
    pub costs: HashMap<String, CostInfo>,
    /// Rated events from the last week, per provider, oldest first. Session
    /// spend and burn rate are measured from these once the provider's real
    /// window is known - see [`anchor_burn_rates`].
    pub recent: HashMap<String, Vec<RecentEvent>>,
    /// Models with tokens and no entry in the price table, sorted. Surfaced by
    /// `--doctor`: an unpriced model must read as a gap, never as $0 spent.
    pub unpriced: Vec<String>,
    pub events: usize,
    /// What the last fleet-sync cycle did, when sync is on.
    pub sync: crate::sync::SyncStatus,
}

impl NativeCostReport {
    pub fn is_empty(&self) -> bool {
        self.events == 0
    }
}

/// The window one read has to cover: month-to-date and the rolling week, which
/// reaches back past the 1st for the first six days of a month.
fn window_start(today: NaiveDate) -> NaiveDate {
    let month_start = crate::month_start(today);
    let week_start = today
        .checked_sub_days(Days::new(WEEKLY_HISTORY_DAYS as u64 - 1))
        .unwrap_or(today);
    month_start.min(week_start)
}

/// Read every transcript in the window and rate it.
pub fn fetch_native(cache_file: &Path, timeout: Duration, today: NaiveDate) -> NativeCostReport {
    let (events, _) = read_window(today);
    rate(&events, cache_file, timeout, today)
}

/// Read every transcript in the window, returning the window with them.
///
/// The bound is part of the answer: it is the slice of history this read is
/// authoritative for, and the fleet store replaces exactly that much of its own
/// device's data and no more.
pub fn read_window(today: NaiveDate) -> (Vec<UsageEvent>, NaiveDate) {
    let since = window_start(today);
    (read_events_from(&Roots::discover(), since), since)
}

/// Rate a set of events, wherever they were read.
pub fn rate(
    events: &[UsageEvent],
    cache_file: &Path,
    timeout: Duration,
    today: NaiveDate,
) -> NativeCostReport {
    if events.is_empty() {
        return NativeCostReport::default();
    }
    let prices = pricing::load(cache_file, timeout, true);
    build_report(events, &prices, today)
}

/// Where each reader looks.
///
/// One field per reader rather than one argument per reader: adding a CLI is
/// supposed to be a reader and nothing else, and a growing argument list makes
/// it an edit to every caller as well. [`Default`] gives a test the two trees it
/// cares about and empty roots for the rest.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub claude: Vec<PathBuf>,
    pub codex: Vec<PathBuf>,
    pub kimi: Vec<PathBuf>,
    pub grok: Vec<PathBuf>,
}

impl Roots {
    /// Every reader's own default locations.
    pub fn discover() -> Self {
        Self {
            claude: claude_code::roots(),
            codex: codex_cli::roots(),
            kimi: kimi_cli::roots(),
            grok: grok_cli::roots(),
        }
    }

    pub fn all(&self) -> Vec<PathBuf> {
        let mut out = self.claude.clone();
        out.extend(self.codex.iter().cloned());
        out.extend(self.kimi.iter().cloned());
        out.extend(self.grok.iter().cloned());
        out
    }
}

/// Read every transcript shape from explicit roots, oldest window bound first.
///
/// Split out from [`fetch_native`] so a test can point at a fixture tree
/// without touching process-global environment variables.
pub fn read_events_from(roots: &Roots, since: NaiveDate) -> Vec<UsageEvent> {
    // A `seen` set per reader: the keys are only unique within one transcript
    // format, and a shared set would let one reader's hash collide another's
    // and silently drop a call.
    let mut claude_seen = HashMap::new();
    let mut codex_seen = HashSet::new();
    let mut kimi_seen = HashSet::new();
    let mut grok_seen = HashSet::new();
    let mut events = claude_code::read_events(&roots.claude, since, &mut claude_seen);
    events.extend(codex_cli::read_events(&roots.codex, since, &mut codex_seen));
    events.extend(kimi_cli::read_events(&roots.kimi, since, &mut kimi_seen));
    events.extend(grok_cli::read_events(&roots.grok, since, &mut grok_seen));
    events
}

#[derive(Default)]
struct Totals {
    usd: f64,
    tokens: TokenCounts,
}

impl Totals {
    fn add(&mut self, usd: f64, tokens: &TokenCounts) {
        self.usd += usd;
        self.tokens.add(tokens);
    }
}

/// Rate every event and fold it into one `CostInfo` per provider.
///
/// Split out from the reading so it can be tested on fixtures without a
/// filesystem, and so a second source of events (a new CLI) needs no changes
/// here.
pub fn build_report(
    events: &[UsageEvent],
    prices: &pricing::PriceTable,
    today: NaiveDate,
) -> NativeCostReport {
    // provider -> day -> model -> totals
    let mut buckets: HashMap<&str, HashMap<NaiveDate, HashMap<&str, Totals>>> = HashMap::new();
    let mut unpriced: HashSet<String> = HashSet::new();
    let mut recent: HashMap<String, Vec<RecentEvent>> = HashMap::new();
    // A week back covers the longest window any provider reports, so a session
    // figure never has to re-read the transcripts to find its own start.
    //
    // Anchored on the caller's `today`, not the wall clock. The two disagree
    // whenever a date is injected - a fixture, or a fleet replay of another
    // machine's history - and a cutoff measured from `now` silently drops the
    // whole replay. UTC midnight of that date is up to half a day more
    // generous than the local one, which this window can afford.
    let recent_cutoff = today
        .checked_sub_days(chrono::Days::new(WEEKLY_HISTORY_DAYS as u64))
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
        .unwrap_or_else(|| Utc::now() - ChronoDuration::days(WEEKLY_HISTORY_DAYS as i64));

    for event in events {
        let usd = match prices.get(&event.model) {
            Some(price) => price.cost(&event.tokens),
            None => {
                if event.tokens.total() > 0 {
                    unpriced.insert(event.model.clone());
                }
                0.0
            }
        };
        if event.at >= recent_cutoff {
            recent
                .entry(event.provider.to_string())
                .or_default()
                .push(RecentEvent {
                    at: event.at,
                    usd,
                    tokens: event.tokens.total(),
                });
        }
        buckets
            .entry(event.provider)
            .or_default()
            .entry(event.date)
            .or_default()
            .entry(event.model.as_str())
            .or_default()
            .add(usd, &event.tokens);
    }

    let periods = recent_periods(today, WEEKLY_HISTORY_DAYS);
    let month_start = crate::month_start(today);

    let mut costs = HashMap::new();
    for (provider, days) in buckets {
        let mut today_models: HashMap<&str, Totals> = HashMap::new();
        let mut monthly_models: HashMap<&str, Totals> = HashMap::new();
        let mut per_day: HashMap<String, Totals> = HashMap::new();

        for (date, models) in &days {
            let key = date.format("%Y-%m-%d").to_string();
            for (model, totals) in models {
                if *date == today {
                    today_models
                        .entry(model)
                        .or_default()
                        .add(totals.usd, &totals.tokens);
                }
                if *date >= month_start && *date <= today {
                    monthly_models
                        .entry(model)
                        .or_default()
                        .add(totals.usd, &totals.tokens);
                }
                per_day
                    .entry(key.clone())
                    .or_default()
                    .add(totals.usd, &totals.tokens);
            }
        }

        // Every provider covers the same dates, zero-filled where it spent
        // nothing: an idle day is $0, not a day that did not happen.
        let weekly_history: Vec<DayCost> = periods
            .iter()
            .map(|period| {
                let totals = per_day.get(period);
                DayCost {
                    date: period.clone(),
                    usd: totals.map(|t| t.usd).unwrap_or(0.0),
                    tokens: totals.map(|t| t.tokens.total()).unwrap_or(0),
                    by_device: Vec::new(),
                }
            })
            .collect();

        let (today_usd, today_tokens, today_models) = into_model_costs(today_models);
        let (monthly_usd, monthly_tokens, monthly_models) = into_model_costs(monthly_models);

        costs.insert(
            provider.to_string(),
            CostInfo {
                today_usd,
                today_tokens,
                monthly_usd,
                monthly_tokens,
                today_models,
                monthly_models,
                burn_rate: None,
                session_usd: 0.0,
                weekly_usd: weekly_history.iter().map(|d| d.usd).sum(),
                weekly_cost_history: weekly_history.iter().map(|d| d.usd).collect(),
                weekly_history,
                by_device: Vec::new(),
                sync_note: None,
            },
        );
    }

    let mut unpriced: Vec<String> = unpriced.into_iter().collect();
    unpriced.sort();
    for series in recent.values_mut() {
        series.sort_by_key(|e| e.at);
    }
    NativeCostReport {
        costs,
        recent,
        unpriced,
        events: events.len(),
        sync: crate::sync::SyncStatus::default(),
    }
}

/// The window a provider says it is currently inside.
fn session_window(payload: &ProviderPayload) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let window = payload.usage.as_ref()?.primary.as_ref()?;
    let end = window.resets_at.as_deref()?.parse::<DateTime<Utc>>().ok()?;
    let minutes = window.window_minutes.filter(|m| *m > 0)?;
    Some((end - ChronoDuration::minutes(minutes as i64), end))
}

/// Fill in session spend, burn rate and projection from the provider's **real**
/// window.
///
/// ccusage infers a 5h block by flooring the hour of the first activity and
/// cutting five hours later, because from outside it has nothing better. The
/// provider tells TokenGauge exactly when the window resets and how long it is,
/// and the gauge directly above this row in the panel is already drawn from
/// that. Anchoring here is what makes the two agree.
pub fn anchor_burn_rates(report: &mut NativeCostReport, payloads: &[ProviderPayload]) {
    let now = Utc::now();
    let retention = ChronoDuration::days(WEEKLY_HISTORY_DAYS as i64);
    for payload in payloads {
        // A payload restored from cache carries the window it had when it was
        // written, which may have reset since. Measuring session spend against
        // an expired window invents a figure.
        if payload.stale {
            continue;
        }
        let Some((start, end)) = session_window(payload) else {
            continue;
        };
        // `recent` only keeps a week, so a longer window - Codex's lone
        // unknown-duration one, or a GLM quota measured in months - would be
        // measured from a fraction of itself and read as a lull.
        if end - start > retention {
            continue;
        }
        // A window that has already reset is not the current session. Its spend
        // belongs to a period that is over, and projecting from it would report
        // a burn rate for time that is no longer being billed.
        if end <= now {
            continue;
        }
        let key = payload.provider.to_lowercase();
        let Some(cost) = report.costs.get_mut(&key) else {
            continue;
        };
        let Some(events) = report.recent.get(&key) else {
            continue;
        };

        let mut usd = 0.0;
        let mut tokens = 0u64;
        for event in events.iter().filter(|e| e.at >= start && e.at <= end) {
            usd += event.usd;
            tokens += event.tokens;
        }
        cost.session_usd = usd;

        // Elapsed is measured from the window's start, not from its first
        // event: a window that opened an hour ago and was idle for most of it
        // is burning slowly, and saying otherwise overstates the projection.
        let elapsed_minutes = (now - start).num_seconds() as f64 / 60.0;
        let remaining_minutes = (end - now).num_seconds() as f64 / 60.0;
        if elapsed_minutes <= 0.0 || tokens == 0 {
            continue;
        }
        let cost_per_hour = usd / (elapsed_minutes / 60.0);
        let remaining = remaining_minutes.max(0.0);
        cost.burn_rate = Some(BurnRate {
            cost_per_hour,
            tokens_per_minute: (tokens as f64 / elapsed_minutes) as u64,
            remaining_minutes: remaining as u32,
            projected_cost: usd + cost_per_hour * (remaining / 60.0),
        });
    }
}

fn into_model_costs(models: HashMap<&str, Totals>) -> (f64, u64, Vec<ModelCost>) {
    let mut total_usd = 0.0;
    let mut total_tokens = 0;
    let mut rows: Vec<ModelCost> = models
        .into_iter()
        .map(|(model, t)| {
            total_usd += t.usd;
            total_tokens += t.tokens.total();
            ModelCost {
                model: model.to_string(),
                usd: t.usd,
                tokens: t.tokens.total(),
                input_tokens: t.tokens.input,
                output_tokens: t.tokens.output,
                cache_creation_tokens: t.tokens.cache_creation(),
                cache_read_tokens: t.tokens.cache_read,
                by_device: Vec::new(),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.usd
            .partial_cmp(&a.usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });
    (total_usd, total_tokens, rows)
}

/// Freshness of the last native read, for `--doctor`.
pub fn transcript_roots() -> Vec<PathBuf> {
    Roots::discover().all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two machines on different builds have to produce the same day
    /// fingerprint, or the double-counting check goes quietly dead. A literal
    /// pins that: refactoring the length-prefixing would otherwise pass every
    /// other test while breaking mixed-version fleets.
    #[test]
    fn the_dedup_key_is_a_fixed_value_not_whatever_this_build_hashes_to() {
        assert_eq!(dedup_key("msg_01ABC", "req_99"), 5_941_904_215_720_101_304);
        assert_eq!(digest_u64(&[b"", b""]), 3_983_162_290_893_594_069);
        assert_ne!(
            dedup_key("ab", "c"),
            dedup_key("a", "bc"),
            "length prefixing must keep the parts distinct"
        );
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn event(provider: &'static str, model: &str, date: NaiveDate, out: u64) -> UsageEvent {
        UsageEvent {
            provider,
            model: model.into(),
            date,
            at: Local
                .from_local_datetime(&date.and_hms_opt(12, 0, 0).expect("noon"))
                .single()
                .expect("unambiguous")
                .with_timezone(&Utc),
            tokens: TokenCounts {
                output: out,
                ..Default::default()
            },
            key: None,
        }
    }

    fn payload_with_window(provider: &str, resets_at: &str, minutes: u32) -> ProviderPayload {
        ProviderPayload {
            provider: provider.into(),
            version: None,
            source: None,
            usage: Some(crate::UsageSnapshot {
                primary: Some(crate::UsageWindow {
                    used_percent: Some(10),
                    reset_description: None,
                    resets_at: Some(resets_at.into()),
                    window_minutes: Some(minutes),
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
    fn history_is_zero_filled_across_the_whole_window() {
        let prices = pricing::PriceTable::vendored();
        let events = vec![
            event("claude", "claude-opus-5", day(2026, 8, 18), 1000),
            event("claude", "claude-opus-5", day(2026, 8, 24), 2000),
        ];
        let report = build_report(&events, &prices, day(2026, 8, 24));
        let claude = report.costs.get("claude").expect("claude");

        assert_eq!(claude.weekly_history.len(), WEEKLY_HISTORY_DAYS);
        assert_eq!(claude.weekly_history[0].date, "2026-08-18");
        assert_eq!(claude.weekly_history[6].date, "2026-08-24");
        assert_eq!(claude.weekly_history[1].usd, 0.0);
        assert_eq!(claude.weekly_history[1].tokens, 0);
        assert_eq!(claude.today_tokens, 2000);
    }

    #[test]
    fn today_month_and_week_come_from_one_read() {
        let prices = pricing::PriceTable::vendored();
        let events = vec![
            // Previous month: inside the rolling week, outside the month.
            event("claude", "claude-opus-5", day(2026, 7, 31), 100),
            event("claude", "claude-opus-5", day(2026, 8, 1), 200),
            event("claude", "claude-opus-5", day(2026, 8, 3), 400),
        ];
        let report = build_report(&events, &prices, day(2026, 8, 3));
        let claude = report.costs.get("claude").expect("claude");

        assert_eq!(claude.today_tokens, 400);
        assert_eq!(claude.monthly_tokens, 600);
        assert_eq!(
            claude.weekly_history.iter().map(|d| d.tokens).sum::<u64>(),
            700
        );
        assert!(claude.weekly_usd > claude.monthly_usd);
    }

    #[test]
    fn an_unpriced_model_is_reported_rather_than_billed_at_zero() {
        let prices = pricing::PriceTable::vendored();
        let events = vec![event(
            "claude",
            "claude-from-the-future-9",
            day(2026, 8, 24),
            5,
        )];
        let report = build_report(&events, &prices, day(2026, 8, 24));

        assert_eq!(
            report.unpriced,
            vec!["claude-from-the-future-9".to_string()]
        );
        // The tokens are still counted; only the money is unknown.
        assert_eq!(report.costs["claude"].today_tokens, 5);
        assert_eq!(report.costs["claude"].today_usd, 0.0);
    }

    #[test]
    fn providers_are_kept_apart() {
        let prices = pricing::PriceTable::vendored();
        let events = vec![
            event("claude", "claude-opus-5", day(2026, 8, 24), 100),
            event("codex", "gpt-5.5", day(2026, 8, 24), 100),
            event("glm", "glm-4.6", day(2026, 8, 24), 100),
        ];
        let report = build_report(&events, &prices, day(2026, 8, 24));
        assert_eq!(report.costs.len(), 3);
        assert!(report.costs.contains_key("glm"));
    }

    #[test]
    fn models_are_ranked_by_spend() {
        let prices = pricing::PriceTable::vendored();
        let events = vec![
            event(
                "claude",
                "claude-haiku-4-5-20251001",
                day(2026, 8, 24),
                1000,
            ),
            event("claude", "claude-opus-5", day(2026, 8, 24), 1000),
        ];
        let report = build_report(&events, &prices, day(2026, 8, 24));
        let models = &report.costs["claude"].today_models;
        assert_eq!(models[0].model, "claude-opus-5");
        assert!(models[0].usd > models[1].usd);
    }

    #[test]
    fn the_session_figure_uses_the_providers_own_window() {
        let now = Utc::now();
        let prices = pricing::PriceTable::vendored();
        let today = now.with_timezone(&Local).date_naive();

        // Two calls an hour apart, one of them before the window opened.
        let mut inside = event("claude", "claude-opus-5", today, 1_000_000);
        inside.at = now - ChronoDuration::minutes(30);
        let mut outside = event("claude", "claude-opus-5", today, 1_000_000);
        outside.at = now - ChronoDuration::hours(6);

        let mut report = build_report(&[inside, outside], &prices, today);
        let resets_at = (now + ChronoDuration::hours(2)).to_rfc3339();
        anchor_burn_rates(
            &mut report,
            &[payload_with_window("claude", &resets_at, 300)],
        );

        let claude = &report.costs["claude"];
        // The call from six hours ago is outside a five-hour window.
        let one_call = prices
            .get("claude-opus-5")
            .expect("priced")
            .cost(&TokenCounts {
                output: 1_000_000,
                ..Default::default()
            });
        assert!((claude.session_usd - one_call).abs() < 1e-9);

        let burn = claude.burn_rate.as_ref().expect("burn rate");
        // Three hours elapsed of a five-hour window, two remaining.
        assert!((118..=121).contains(&burn.remaining_minutes));
        assert!(burn.cost_per_hour > 0.0);
        assert!(burn.projected_cost > claude.session_usd);
    }

    #[test]
    fn a_window_longer_than_the_retained_history_gets_no_burn_rate() {
        let now = Utc::now();
        let prices = pricing::PriceTable::vendored();
        let today = now.with_timezone(&Local).date_naive();
        let mut report = build_report(
            &[event("claude", "claude-opus-5", today, 100)],
            &prices,
            today,
        );
        // 30 days: only the last seven are retained, so any figure would be
        // measured from a fraction of the window.
        let resets_at = (now + ChronoDuration::hours(1)).to_rfc3339();
        anchor_burn_rates(
            &mut report,
            &[payload_with_window("claude", &resets_at, 30 * 24 * 60)],
        );
        assert!(report.costs["claude"].burn_rate.is_none());
        assert_eq!(report.costs["claude"].session_usd, 0.0);
    }

    #[test]
    fn an_already_reset_window_gets_no_burn_rate() {
        let now = Utc::now();
        let prices = pricing::PriceTable::vendored();
        let today = now.with_timezone(&Local).date_naive();
        let mut report = build_report(
            &[event("claude", "claude-opus-5", today, 100)],
            &prices,
            today,
        );
        // Reset an hour ago: that session is over.
        let resets_at = (now - ChronoDuration::hours(1)).to_rfc3339();
        anchor_burn_rates(
            &mut report,
            &[payload_with_window("claude", &resets_at, 300)],
        );
        assert!(report.costs["claude"].burn_rate.is_none());
        assert_eq!(report.costs["claude"].session_usd, 0.0);
    }

    #[test]
    fn a_stale_payload_gets_no_burn_rate() {
        let now = Utc::now();
        let prices = pricing::PriceTable::vendored();
        let today = now.with_timezone(&Local).date_naive();
        let mut report = build_report(
            &[event("claude", "claude-opus-5", today, 100)],
            &prices,
            today,
        );
        let resets_at = (now + ChronoDuration::hours(2)).to_rfc3339();
        let mut payload = payload_with_window("claude", &resets_at, 300);
        payload.stale = true;
        anchor_burn_rates(&mut report, &[payload]);
        assert!(report.costs["claude"].burn_rate.is_none());
    }

    #[test]
    fn a_provider_that_reports_no_window_keeps_no_burn_rate() {
        let prices = pricing::PriceTable::vendored();
        let today = Utc::now().with_timezone(&Local).date_naive();
        let mut report = build_report(
            &[event("claude", "claude-opus-5", today, 100)],
            &prices,
            today,
        );
        let mut payload = payload_with_window("claude", "not-a-timestamp", 300);
        anchor_burn_rates(&mut report, std::slice::from_ref(&payload));
        assert!(report.costs["claude"].burn_rate.is_none());

        payload.usage = None;
        anchor_burn_rates(&mut report, &[payload]);
        assert!(report.costs["claude"].burn_rate.is_none());
    }

    /// Checks the readers against ccusage on this machine's real transcripts,
    /// over a window wide enough to include Codex history. Ignored by default:
    /// it needs both a populated home directory and a ccusage runner.
    ///
    /// `cargo test -p tokengauge-core -- --ignored --nocapture agrees_with_ccusage`
    #[test]
    #[ignore = "reads the developer's own transcripts and shells out to ccusage"]
    fn agrees_with_ccusage_on_real_transcripts() {
        let since = day(2026, 1, 1);
        let events = read_events_from(&Roots::discover(), since);
        assert!(!events.is_empty(), "no transcripts to check against");

        let mut ours: HashMap<&str, u64> = HashMap::new();
        for event in &events {
            *ours.entry(event.provider).or_default() += event.tokens.total();
        }

        for (agent, provider) in [("claude", "claude"), ("codex", "codex")] {
            let output = std::process::Command::new("bunx")
                .args([
                    "ccusage",
                    agent,
                    "daily",
                    "--since",
                    "20260101",
                    "--offline",
                    "--json",
                ])
                .output()
                .expect("run ccusage");
            let parsed: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("ccusage json");
            let theirs = parsed["totals"]["totalTokens"].as_u64().unwrap_or(0);
            let mine = ours.get(provider).copied().unwrap_or(0);
            let drift = (mine as f64 - theirs as f64).abs() / theirs.max(1) as f64;
            println!(
                "{provider}: native {mine} vs ccusage {theirs} ({:.3}%)",
                drift * 100.0
            );
            assert!(drift < 0.01, "{provider} drifted {:.3}%", drift * 100.0);
        }
    }

    #[test]
    fn the_window_covers_the_month_and_the_rolling_week() {
        // Early in a month the week reaches back further than the 1st.
        assert_eq!(window_start(day(2026, 8, 3)), day(2026, 7, 28));
        // Late in a month the month reaches back further than the week.
        assert_eq!(window_start(day(2026, 8, 24)), day(2026, 8, 1));
    }
}
