//! The parts every provider fetcher does the same way.
//!
//! Five fetchers, five transports - a JSON API each for Claude, Codex, z.ai and
//! Kimi, and gRPC-web for Grok - but the same shape underneath: check the HTTP
//! outcome, pull numbers out of a loosely-typed body, turn an epoch into
//! RFC3339, and build a `ProviderPayload` whose write-side fields are all the
//! same constant. That last part is why this exists: each fetcher spelling out
//! `version: None, credits: None, error: None, stale: false` is four lines that
//! can only ever be wrong.
//!
//! Deliberately not a trait. Five one-implementor methods would force the gRPC
//! fetcher through a JSON-shaped interface it does not have, to save nothing.

use anyhow::{Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::{ProviderPayload, UsageSnapshot};

/// The HTTP outcome ladder. Only the auth hint differs per provider, so only
/// that is a parameter.
///
/// Three of the five reported a 429 as a bare `HTTP 429`, which reads like a
/// bug rather than "wait a moment" - the friendly wording Claude and Kimi
/// already had is now everyone's.
pub(crate) fn check_status(
    status: reqwest::StatusCode,
    provider: &str,
    unauthorized_hint: &str,
) -> Result<()> {
    use reqwest::StatusCode;
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return Err(anyhow!("{provider} unauthorized - {unauthorized_hint}"));
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(anyhow!("{provider} rate-limited - try again shortly"));
    }
    if !status.is_success() {
        return Err(anyhow!("{provider} HTTP {}", status.as_u16()));
    }
    Ok(())
}

/// A number out of a loosely-typed body, whether it arrived as a number or as
/// a string.
///
/// NaN and infinity are rejected, including their string spellings: they parse
/// successfully, and a malformed value that masquerades as live data gets past
/// the stale-cache fallback that should have caught it.
pub(crate) fn json_num(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|v: &f64| v.is_finite())
}

/// [`json_num`] for whole numbers.
pub(crate) fn json_int(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

/// A string that is actually there. Whitespace-only is absent.
pub(crate) fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// An epoch to RFC3339, in seconds or milliseconds.
///
/// Which unit a provider means is not always documented and z.ai has sent
/// both, so the magnitude decides: anything past the year 2286 in seconds is
/// milliseconds.
pub(crate) fn epoch_to_rfc3339(value: f64) -> Option<String> {
    if !value.is_finite() {
        return None;
    }
    let secs = if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    Utc.timestamp_opt(secs as i64, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
}

impl UsageSnapshot {
    /// An empty snapshot stamped now. Fetchers fill in the windows they have
    /// and take the rest from here:
    ///
    /// ```ignore
    /// UsageSnapshot { primary, secondary, ..UsageSnapshot::at(now) }
    /// ```
    pub(crate) fn at(now: DateTime<Utc>) -> Self {
        Self {
            primary: None,
            secondary: None,
            tertiary: None,
            updated_at: Some(now.to_rfc3339()),
            login_method: None,
            extra_rate_windows: Vec::new(),
        }
    }
}

impl ProviderPayload {
    /// A payload from a live fetch.
    ///
    /// `version`, `credits`, `error` and `stale` are the write-side constants:
    /// `version` and `error` are read-side only, kept for snapshots and
    /// payloads written by the CodexBar-shaped tools this format came from, and
    /// `stale` belongs to `fetch_all_providers`, which is the only thing that
    /// knows a fetch failed. A fetcher with credits sets `credits` after.
    pub(crate) fn live(provider: &str, source: &str, usage: UsageSnapshot) -> Self {
        Self {
            stale_reason: None,
            provider: provider.to_string(),
            version: None,
            source: Some(source.to_string()),
            usage: Some(usage),
            credits: None,
            error: None,
            stale: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_is_read_whether_it_arrived_quoted_or_not() {
        assert_eq!(json_num(&Value::from(12.5)), Some(12.5));
        assert_eq!(json_num(&Value::from(" 12.5 ")), Some(12.5));
        assert_eq!(json_int(&Value::from("42")), Some(42));
        assert_eq!(json_num(&Value::Null), None);
    }

    /// "NaN" and "inf" parse. A percentage that is neither must not reach the
    /// panel as live data - the stale fallback exists for exactly that fetch.
    #[test]
    fn a_non_finite_number_is_absent_rather_than_present_and_wrong() {
        assert_eq!(json_num(&Value::from("NaN")), None);
        assert_eq!(json_num(&Value::from("inf")), None);
        assert_eq!(json_num(&Value::from("-inf")), None);
    }

    #[test]
    fn an_epoch_is_read_in_either_unit() {
        let secs = epoch_to_rfc3339(1_800_000_000.0).expect("seconds");
        let millis = epoch_to_rfc3339(1_800_000_000_000.0).expect("milliseconds");
        assert_eq!(secs, millis, "the same instant either way");
        assert!(secs.starts_with("2027-"), "{secs}");
        assert_eq!(epoch_to_rfc3339(f64::NAN), None);
    }

    #[test]
    fn whitespace_is_not_a_value() {
        assert_eq!(trimmed(Some("  x  ".into())), Some("x".into()));
        assert_eq!(trimmed(Some("   ".into())), None);
        assert_eq!(trimmed(None), None);
    }

    /// Every fetcher used to spell these four out. A 429 reported as a bare
    /// HTTP code reads like a bug rather than a wait, and three of them did.
    #[test]
    fn the_status_ladder_names_every_outcome() {
        use reqwest::StatusCode;
        let ladder = |code| check_status(code, "Kimi", "run `kimi` to log in");

        assert!(ladder(StatusCode::OK).is_ok());
        for code in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let message = ladder(code).unwrap_err().to_string();
            assert_eq!(message, "Kimi unauthorized - run `kimi` to log in");
        }
        assert_eq!(
            ladder(StatusCode::TOO_MANY_REQUESTS)
                .unwrap_err()
                .to_string(),
            "Kimi rate-limited - try again shortly"
        );
        assert_eq!(
            ladder(StatusCode::BAD_GATEWAY).unwrap_err().to_string(),
            "Kimi HTTP 502"
        );
    }

    #[test]
    fn a_live_payload_is_never_stale_and_never_carries_an_error() {
        let payload = ProviderPayload::live("glm", "z.ai", UsageSnapshot::at(Utc::now()));
        assert_eq!(payload.provider, "glm");
        assert_eq!(payload.source.as_deref(), Some("z.ai"));
        assert!(!payload.stale);
        assert!(!payload.has_error());
        assert!(payload.usage.as_ref().unwrap().updated_at.is_some());
    }
}
