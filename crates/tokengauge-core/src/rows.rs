//! A fetcher's payload turned into the row every frontend renders.
//!
//! This is where the provider's own vocabulary stops. A payload speaks in
//! primary/secondary/tertiary windows and RFC3339 instants; a row speaks in the
//! labels that provider uses for its windows, percentages already clamped, and
//! reset times already relative. Past here, nothing needs to know which
//! provider it is looking at - which is what lets [`crate::panel`] build one
//! panel spec for all of them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::*;

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
    /// A prepaid balance in USD, for the providers that sell one instead of a
    /// window. [`crate::panel`] formats it; a row does not.
    pub credits: Option<f64>,
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

/// Burn pace for a usage window, if it has the percent, duration and reset
/// time pace needs - measured as of `anchor`, dropped once the window has
/// rolled over.
///
/// The two clocks are different on purpose: the projection is measured from
/// the instant the figures were true, and thrown away against the real one,
/// because a projection to a window that has already reset describes nothing
/// that is still the case however good the numbers behind it were.
fn window_pace(
    window: &UsageWindow,
    anchor: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<UsagePace> {
    let reset = DateTime::parse_from_rfc3339(window.resets_at.as_deref()?)
        .ok()?
        .with_timezone(&Utc);
    if reset <= now {
        return None;
    }
    UsagePace::for_window(
        window.used_percent?,
        window.window_minutes,
        window.resets_at.as_deref(),
        anchor,
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
///
/// Counted against the clock at render time, never against the fetch: the
/// instant is absolute, so a countdown drawn from a snapshot minutes old is
/// still the right countdown. The percentage beside it is the one that has to
/// wait for the next fetch.
///
/// A reset instant already past is a window that rolled over since the fetch,
/// which is a different thing from a window that never had a reset time - and
/// says "now" rather than counting up, because the panel calls the latter
/// "not started".
fn format_reset_time(resets_at: Option<&str>, description: Option<String>) -> String {
    if let Some(resets_at) = resets_at
        && let Ok(reset_time) = DateTime::parse_from_rfc3339(resets_at)
    {
        let now = Utc::now();
        let reset_utc = reset_time.with_timezone(&Utc);
        let duration = reset_utc.signed_duration_since(now);

        // Under a minute left rounds to "in 0m", which reads as broken on a
        // countdown that now ticks.
        return if duration.num_seconds() >= 60 {
            format!("in {}", fmt::format_duration(duration.num_minutes(), 3))
        } else {
            "now".to_string()
        };
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
        // `used` stopped moving when the fetch did, so a pace measured against
        // a clock that kept going decays on its own: the longer the outage
        // lasts, the more it reads as a slowdown that never happened. The
        // payload's own instant is what the rest of the panel is already
        // showing, so the projection is measured from there.
        // A stale payload with no `updated_at` gets none at all: last known
        // values with no instant to attach them to leave nothing honest to
        // measure against.
        let anchor = usage
            .updated_at
            .as_deref()
            .and_then(|iso| DateTime::parse_from_rfc3339(iso).ok())
            .map(|at| at.with_timezone(&Utc))
            .or_else(|| (!payload.stale).then_some(now));

        if let Some(anchor) = anchor {
            session_pace = usage
                .primary
                .as_ref()
                .and_then(|w| window_pace(w, anchor, now));
            weekly_pace = usage
                .secondary
                .as_ref()
                .and_then(|w| window_pace(w, anchor, now));
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
                let pace = anchor.and_then(|anchor| {
                    w.window
                        .as_ref()
                        .and_then(|window| window_pace(window, anchor, now))
                });
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

    let credits = payload.credits.and_then(|credits| credits.remaining);

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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn format_window_reset_already_passed_says_now() {
        // The window rolled over since the fetch, so its description is as old
        // as its instant and neither of them is the answer.
        let past = Utc::now() - chrono::Duration::minutes(3);
        let window = UsageWindow {
            used_percent: Some(69),
            reset_description: Some("Jan 20 at 12:59PM".to_string()),
            resets_at: Some(past.to_rfc3339()),
            window_minutes: Some(10080),
        };
        let (_, _, reset) = format_window(Some(window));
        assert_eq!(reset, "now");
    }

    #[test]
    fn format_window_under_a_minute_says_now() {
        let soon = Utc::now() + chrono::Duration::seconds(20);
        let window = UsageWindow {
            used_percent: Some(90),
            reset_description: None,
            resets_at: Some(soon.to_rfc3339()),
            window_minutes: Some(300),
        };
        let (_, _, reset) = format_window(Some(window));
        assert_eq!(reset, "now");
    }

    // ------------------------------------------------------------------------
    // payload_to_rows_with_costs tests
    // ------------------------------------------------------------------------

    fn rows_of(payloads: Vec<ProviderPayload>) -> Vec<ProviderRow> {
        payload_to_rows_with_costs(payloads, &HashMap::new())
    }

    /// A weekly window, `used` per cent of the way through, resetting in two
    /// hours, last fetched `fetched_minutes_ago` ago.
    fn paced(
        used: u8,
        fetched_minutes_ago: i64,
        reset_in_minutes: i64,
        stale: bool,
    ) -> ProviderPayload {
        let now = Utc::now();
        let window = |used: u8| UsageWindow {
            used_percent: Some(used),
            reset_description: None,
            resets_at: Some((now + chrono::Duration::minutes(reset_in_minutes)).to_rfc3339()),
            window_minutes: Some(10080),
        };
        ProviderPayload {
            provider: "claude".to_string(),
            version: None,
            source: None,
            usage: Some(UsageSnapshot {
                primary: None,
                secondary: Some(window(used)),
                tertiary: None,
                updated_at: Some(
                    (now - chrono::Duration::minutes(fetched_minutes_ago)).to_rfc3339(),
                ),
                login_method: None,
                extra_rate_windows: Vec::new(),
            }),
            credits: None,
            error: None,
            stale,
        }
    }

    /// Every other figure on a stale panel is the last known one. The pace used
    /// to be the exception, and dropping it is what left a window with a
    /// percentage, a reset time and no projection between them.
    #[test]
    fn a_stale_payload_keeps_the_pace_it_was_fetched_with() {
        let rows = rows_of(vec![paced(60, 90, 120, true)]);
        let pace = rows[0].weekly_pace.as_ref().expect("a stale pace");

        // Measured from the payload's own instant, not from now: an outage
        // must not walk the projection down on its own.
        let fresh = rows_of(vec![paced(60, 90, 120, false)]);
        let live = fresh[0].weekly_pace.as_ref().expect("a live pace");
        assert!(
            (pace.projected_percent - live.projected_percent).abs() < 0.001,
            "{} vs {}",
            pace.projected_percent,
            live.projected_percent
        );

        // And the anchor is what moves it: the same figures fetched ten hours
        // ago describe a window less far through, so they project higher. If
        // both were measured against `now` these would be equal.
        let older = rows_of(vec![paced(60, 600, 120, true)]);
        let older = older[0].weekly_pace.as_ref().expect("a stale pace");
        assert!(
            older.projected_percent > pace.projected_percent,
            "{} vs {}",
            older.projected_percent,
            pace.projected_percent
        );
    }

    /// However good the numbers behind it were, a projection to a window that
    /// has already rolled over describes nothing that is still the case.
    #[test]
    fn a_window_that_has_since_reset_carries_no_pace() {
        let rows = rows_of(vec![paced(60, 90, -30, true)]);
        assert!(rows[0].weekly_pace.is_none());
    }

    /// The one case that still withholds the pace: last known values with no
    /// instant to attach them to. Falling back to `now` here would quietly
    /// revive the decay this change removed, for exactly the payloads that
    /// cannot say how old they are.
    #[test]
    fn a_stale_payload_with_no_instant_carries_no_pace() {
        let mut payload = paced(60, 90, 120, true);
        payload.usage.as_mut().expect("usage").updated_at = None;
        let rows = rows_of(vec![payload]);
        assert!(rows[0].weekly_pace.is_none());
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
    fn payload_to_rows_carries_the_credit_balance() {
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
        assert_eq!(rows[0].credits, Some(42.567));
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
}
