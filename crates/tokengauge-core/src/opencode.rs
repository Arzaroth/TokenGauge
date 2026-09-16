//! Native opencode Go usage fetcher (opencode.ai).
//!
//! opencode is two products and this reads one of them. **Go** is a $10/month
//! subscription with three spend caps, and it is the one with limits to draw.
//! **Zen** is a pay-as-you-go balance the same account can hold, which Go falls
//! back to once its caps are spent - that half needs a browser session to read
//! and is not here; see the note at the foot.
//!
//! The response is the cleanest of any provider TokenGauge reads:
//!
//! ```json
//! {"usage": {
//!   "rolling": {"status": "ok", "percent": 12.3, "resetsAt": "..."},
//!   "weekly":  {"status": "ok", "percent": 45.6, "resetsAt": "..."},
//!   "monthly": {"status": "ok", "percent": 78.9, "resetsAt": "..."}}}
//! ```
//!
//! Three windows, each already a percentage with the instant it resets. No
//! conversion and no arithmetic: opencode's caps are per-model dollar amounts
//! (5-hour is 20% of the monthly cap, weekly 50%), and the API does that
//! aggregation itself rather than making a reader do it per model.
//!
//! The credential is `OPENCODE_API_KEY`, minted at opencode.ai/auth and pasted
//! into the TUI with `/connect`. It is **not** read from a file: opencode keeps
//! its own credentials somewhere this has never seen, and a parser written
//! against a guessed shape is how the Claude reader broke twice. When the file
//! is known, it belongs here as a second source and the env var stays first.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::provider::check_status;
use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const DEFAULT_ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";
const ENDPOINT_ENV: &str = "OPENCODE_USAGE_URL";
const API_KEY_ENVS: &[&str] = &["OPENCODE_API_KEY", "OPENCODE_GO_API_KEY"];

/// Minutes each window covers. opencode states them as shares of the monthly
/// cap - 5-hour is 20%, weekly 50% - so the periods are fixed even though the
/// dollar amounts differ per model.
const ROLLING_MINUTES: u32 = 300;
const WEEKLY_MINUTES: u32 = 10_080;

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(crate) fn api_key() -> Result<String> {
    API_KEY_ENVS
        .iter()
        .find_map(|name| env_clean(name))
        .ok_or_else(|| anyhow!("opencode key missing - set OPENCODE_API_KEY"))
}

/// The usage endpoint, from an override a self-hosted gateway may set.
///
/// Split from the environment read so it can be tested: `std::env::set_var` is
/// unsafe from the 2024 edition, so a test going through the variable would
/// have to be the only test running.
fn endpoint_for(override_url: Option<&str>) -> Result<String> {
    let url = override_url.unwrap_or(DEFAULT_ENDPOINT);
    if !url.starts_with("https://") {
        return Err(anyhow!("opencode usage URL must use HTTPS"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    usage: Option<Windows>,
    /// An error envelope comes back with the same 200 some gateways give, so a
    /// response carrying one is a failure however it was framed.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Windows {
    #[serde(default)]
    rolling: Option<Window>,
    #[serde(default)]
    weekly: Option<Window>,
    #[serde(default)]
    monthly: Option<Window>,
}

#[derive(Debug, Clone, Deserialize)]
struct Window {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<String>,
}

impl Window {
    /// The window as the panel draws it, or `None` when the percentage is
    /// missing or not a percentage. A gauge is the whole point of this row, so
    /// a window without one is not worth a row.
    fn to_usage(&self) -> Option<UsageWindow> {
        let percent = self
            .percent
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))?;
        Some(UsageWindow {
            used_percent: Some(pct_u8(percent)),
            // opencode says outright when a window is spent, which is more
            // use than inferring it from 100%: a window can sit at 100 and
            // still serve, and one can be limited before it gets there.
            reset_description: self
                .status
                .as_deref()
                .filter(|s| s.eq_ignore_ascii_case("rate-limited"))
                .map(|_| "rate limited".to_string()),
            resets_at: self
                .resets_at
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            window_minutes: None,
        })
    }
}

fn to_payload(body: UsageResponse, now: DateTime<Utc>) -> Result<ProviderPayload> {
    if body.error.is_some() {
        return Err(anyhow!(
            "opencode returned an error - check OPENCODE_API_KEY"
        ));
    }
    let windows = body
        .usage
        .ok_or_else(|| anyhow!("opencode response carried no usage"))?;

    let mut rolling = windows.rolling.as_ref().and_then(Window::to_usage);
    let mut weekly = windows.weekly.as_ref().and_then(Window::to_usage);
    let monthly = windows.monthly.as_ref().and_then(Window::to_usage);

    // A response with nothing in any window is a failure, not an empty panel:
    // serving it would replace yesterday's figures with three blanks and call
    // that a successful fetch.
    if rolling.is_none() && weekly.is_none() && monthly.is_none() {
        return Err(anyhow!("opencode reported no usable usage window"));
    }

    // The periods are fixed even though each model's dollar cap is not, so the
    // pace projection has something to measure against.
    if let Some(w) = rolling.as_mut() {
        w.window_minutes = Some(ROLLING_MINUTES);
    }
    if let Some(w) = weekly.as_mut() {
        w.window_minutes = Some(WEEKLY_MINUTES);
    }

    Ok(ProviderPayload::live(
        "opencode",
        "api key",
        UsageSnapshot {
            primary: rolling,
            secondary: weekly,
            tertiary: monthly,
            // The label already says Go, so a plan line repeating it would
            // read as "opencode Go · Go".
            login_method: None,
            ..UsageSnapshot::at(now)
        },
    ))
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let key = api_key()?;
    let url = endpoint_for(env_clean(ENDPOINT_ENV).as_deref())?;
    let client = http_client(timeout)?;

    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .send()
        .context("opencode usage request failed")?;

    check_status(resp.status(), "opencode", "check OPENCODE_API_KEY")?;

    let text = resp.text().context("opencode usage read failed")?;
    if text.trim().is_empty() {
        return Err(anyhow!("opencode returned an empty response"));
    }
    let body: UsageResponse =
        serde_json::from_str(&text).context("opencode usage JSON was invalid")?;

    Ok(vec![to_payload(body, now)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn parse(raw: &str) -> Result<ProviderPayload> {
        to_payload(serde_json::from_str(raw).expect("fixture parses"), at())
    }

    const FULL: &str = r#"{"usage":{
        "rolling":{"status":"ok","percent":12.3,"resetsAt":"2026-09-16T05:00:00Z"},
        "weekly":{"status":"ok","percent":45.6,"resetsAt":"2026-09-21T00:00:00Z"},
        "monthly":{"status":"ok","percent":78.9,"resetsAt":"2026-10-01T00:00:00Z"}}}"#;

    /// The three windows land in the order the panel draws them, each keeping
    /// its own reset instant. No arithmetic: opencode aggregates its per-model
    /// dollar caps into a percentage before we see them.
    #[test]
    fn the_three_windows_map_straight_onto_the_three_slots() {
        let usage = parse(FULL).expect("payload").usage.expect("usage");

        let rolling = usage.primary.expect("rolling");
        assert_eq!(rolling.used_percent, Some(12));
        assert_eq!(rolling.window_minutes, Some(300));
        assert_eq!(rolling.resets_at.as_deref(), Some("2026-09-16T05:00:00Z"));

        assert_eq!(usage.secondary.expect("weekly").used_percent, Some(46));
        assert_eq!(usage.tertiary.expect("monthly").used_percent, Some(79));
        assert_eq!(
            usage.login_method, None,
            "the provider label already says which product this is"
        );
    }

    /// A window opencode has not reported is absent rather than zero. The two
    /// look identical on a bar and mean opposite things.
    #[test]
    fn a_window_that_was_not_reported_draws_nothing() {
        let payload =
            parse(r#"{"usage":{"monthly":{"status":"ok","percent":50.0}}}"#).expect("payload");
        let usage = payload.usage.expect("usage");
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert_eq!(usage.tertiary.expect("monthly").used_percent, Some(50));
    }

    /// opencode says outright when a window is spent, which is better than
    /// inferring it from 100%: a window can sit at 100 and still serve, and it
    /// can be limited before it gets there.
    #[test]
    fn a_rate_limited_window_says_so_rather_than_being_inferred() {
        let payload = parse(
            r#"{"usage":{"rolling":{"status":"rate-limited","percent":100.0,
                "resetsAt":"2026-09-16T05:00:00Z"}}}"#,
        )
        .expect("payload");
        let rolling = payload.usage.expect("usage").primary.expect("rolling");
        assert_eq!(rolling.used_percent, Some(100));
        assert_eq!(rolling.reset_description.as_deref(), Some("rate limited"));

        // And an ordinary window carries no such note.
        let ok = parse(FULL).expect("payload").usage.expect("usage");
        assert_eq!(ok.primary.expect("rolling").reset_description, None);
    }

    /// A status this build has not seen still draws its percentage. The number
    /// is the data; refusing the whole response over an unrecognised word is
    /// how a provider disappears on the day its vendor adds a state.
    #[test]
    fn an_unknown_status_still_draws_its_percentage() {
        let payload =
            parse(r#"{"usage":{"weekly":{"status":"degraded","percent":33.0}}}"#).expect("payload");
        assert_eq!(
            payload
                .usage
                .expect("usage")
                .secondary
                .expect("weekly")
                .used_percent,
            Some(33)
        );
    }

    /// A response with nothing usable is a failure, not an empty panel. Serving
    /// it would replace yesterday's figures with three blanks and call that a
    /// successful fetch - which is what the stale fallback exists to prevent.
    #[test]
    fn a_response_with_no_usable_window_is_an_error() {
        for raw in [
            r#"{"usage":{}}"#,
            r#"{"usage":{"rolling":{"status":"ok"}}}"#,
            r#"{"usage":{"rolling":{"status":"ok","percent":null}}}"#,
            r#"{}"#,
        ] {
            assert!(parse(raw).is_err(), "{raw} was accepted");
        }
    }

    /// A percentage that is not one is dropped rather than clamped into a bar
    /// nobody can defend. Out of range and out of `f64` are caught in
    /// different places, which is worth stating rather than conflating: serde
    /// refuses a number it cannot hold before this code sees it, so the
    /// range check here is about values that parse perfectly well and are
    /// still not percentages.
    #[test]
    fn a_percentage_that_is_not_a_percentage_is_dropped() {
        for bad in ["-1.0", "101.0", "1000"] {
            let raw = format!(r#"{{"usage":{{"weekly":{{"status":"ok","percent":{bad}}}}}}}"#);
            assert!(parse(&raw).is_err(), "{bad} was accepted");
        }

        let unrepresentable = serde_json::from_str::<UsageResponse>(
            r#"{"usage":{"weekly":{"status":"ok","percent":1e400}}}"#,
        );
        assert!(
            unrepresentable.is_err(),
            "serde used to reject this; if it now yields inf the is_finite guard is what catches it"
        );
    }

    /// An error envelope can arrive with a 200 through a gateway, so the body
    /// decides rather than the status line.
    #[test]
    fn an_error_envelope_is_a_failure_whatever_the_status_line_said() {
        let err = parse(r#"{"error":{"message":"invalid key"}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OPENCODE_API_KEY"), "{err}");
    }

    /// The key travels as a bearer header, so a URL that downgrades the
    /// transport would put it on the wire in the clear.
    #[test]
    fn an_endpoint_that_is_not_https_is_refused() {
        assert_eq!(endpoint_for(None).unwrap(), DEFAULT_ENDPOINT);
        assert_eq!(
            endpoint_for(Some("https://gw.example.test/usage/")).unwrap(),
            "https://gw.example.test/usage"
        );
        for bad in [
            "http://opencode.ai/zen/go/v1/usage",
            "example.test",
            "ftp://x.test",
        ] {
            let err = endpoint_for(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("HTTPS"), "{bad} was accepted: {err}");
        }
    }
}
