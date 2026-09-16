//! Native OpenRouter usage fetcher (openrouter.ai).
//!
//! OpenRouter is the first provider TokenGauge reads that sells **credit**
//! rather than a plan, and that changes what the panel can say about it. The
//! other five report a percentage of a window they define; OpenRouter reports
//! money, in two independent forms:
//!
//! - an **account balance** - credit bought, credit spent. Every key on the
//!   account draws on it, and it is the figure a user actually watches.
//! - an optional **per-key limit**, which is a spend cap on the key being used.
//!   Most keys have none, and `limit` is then `null`.
//!
//! Neither is a percentage, so the mapping is deliberate rather than obvious:
//!
//! | Wire field | Where it lands | Why |
//! | --- | --- | --- |
//! | `total_credits` - `total_usage` | [`Credits::remaining`] | the balance, the headline |
//! | `limit`, `limit_remaining` | [`CreditLimit`], and a window | a cap is both money and exhaustible |
//! | `usage_daily` / `weekly` / `monthly` | nowhere, yet | spend over a period is cost, not a limit |
//!
//! That last row is unfinished on purpose. The three figures are OpenRouter's
//! own billing numbers and are better than anything a transcript reader could
//! produce, but `CostInfo` is assembled from readers and ccusage, and there is
//! no channel for a provider that reports its own cost. They are parsed and
//! dropped until there is one; inventing a side door for one provider is how
//! the cost pipeline stops having one shape.
//!
//! The third row is the one worth stating out loud: those three look like
//! TokenGauge's three windows and are not. A window is a quota you can exhaust;
//! `usage_daily` is money already spent with nothing to be a fraction *of*.
//! `CostInfo` already carries exactly `today_usd`, `weekly_usd` and
//! `monthly_usd`, so they land there and the cost section draws them with no
//! new vocabulary at all.
//!
//! Credentials are an API key in the environment. There is no CLI to read: the
//! OpenRouter key is pasted into whichever agent routes through it, so no file
//! on disk reliably holds one.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::provider::check_status;
use crate::{
    CreditLimit, Credits, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8,
};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const BASE_URL_ENV: &str = "OPENROUTER_BASE_URL";
const API_KEY_ENVS: &[&str] = &["OPENROUTER_API_KEY", "OPENROUTER_KEY"];

// ---------------------------------------------------------------------------
// Auth + endpoints (env only - no CLI file to read)
// ---------------------------------------------------------------------------

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

pub(crate) fn api_key() -> Result<String> {
    API_KEY_ENVS
        .iter()
        .find_map(|name| env_clean(name))
        .ok_or_else(|| anyhow!("OpenRouter key missing - set OPENROUTER_API_KEY"))
}

/// The two endpoints, from a base URL that a self-hosted gateway may override.
///
/// Split from the environment read so the URL building can be tested:
/// `std::env::set_var` is unsafe from the 2024 edition, so a test going through
/// the variable would have to be the only test running.
fn endpoints_for(override_base: Option<&str>) -> Result<(String, String)> {
    let base = override_base.unwrap_or(DEFAULT_BASE_URL);
    let base = base.trim_end_matches('/');
    if !base.starts_with("https://") {
        return Err(anyhow!("OpenRouter base URL must use HTTPS"));
    }
    Ok((format!("{base}/credits"), format!("{base}/key")))
}

fn endpoints() -> Result<(String, String)> {
    endpoints_for(env_clean(BASE_URL_ENV).as_deref())
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

/// Every OpenRouter v1 endpoint wraps its payload in `{"data": {...}}`.
#[derive(Debug, Clone, Deserialize)]
struct Envelope<T> {
    data: T,
}

/// `GET /credits`. Both figures are USD and cumulative over the account's life,
/// so the balance is the difference rather than either one.
#[derive(Debug, Clone, Deserialize)]
struct CreditsData {
    #[serde(default)]
    total_credits: Option<f64>,
    #[serde(default)]
    total_usage: Option<f64>,
}

/// `GET /key`. Describes the key that authenticated the request, not the
/// account: `usage_*` is this key's spend and `limit` is this key's cap.
#[derive(Debug, Clone, Deserialize)]
struct KeyData {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    limit: Option<f64>,
    #[serde(default)]
    limit_remaining: Option<f64>,
    // Read off the wire and routed nowhere yet - see the module note. Kept
    // rather than dropped because they are the contract, and a test pins that
    // they parse; deleting them means rediscovering the API shape when the
    // channel for a self-reported cost arrives.
    #[serde(default)]
    #[allow(dead_code)]
    usage_daily: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    usage_weekly: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    usage_monthly: Option<f64>,
    #[serde(default)]
    is_free_tier: Option<bool>,
}

/// A money figure that is usable: present, finite, and not negative.
///
/// OpenRouter has been seen to send `null` for a key with no cap, and a JSON
/// number can always be `NaN`-adjacent through a bad gateway. A percentage
/// derived from either is worse than no percentage: it draws a filled bar off
/// a value nobody can defend.
fn money(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v >= 0.0)
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

fn to_payload(credits: CreditsData, key: KeyData, now: DateTime<Utc>) -> ProviderPayload {
    // A cap on the key is the only thing here that behaves like a window: it
    // has a ceiling, a consumed part, and it is exhaustible.
    let limit = money(key.limit);
    let remaining = money(key.limit_remaining);
    let key_window = limit.filter(|l| *l > 0.0).map(|l| {
        // Prefer what OpenRouter says is left over a subtraction of our own:
        // the two can disagree while a request is in flight, and the server's
        // answer is the one the next request will be judged against.
        let used = match remaining {
            Some(rem) => (l - rem).max(0.0),
            None => 0.0,
        };
        UsageWindow {
            used_percent: Some(pct_u8(used / l * 100.0)),
            reset_description: None,
            resets_at: None,
            window_minutes: None,
        }
    });

    // The account balance. Both halves are cumulative-since-forever, so the
    // balance is the difference and the fraction spent is the gauge.
    let total = money(credits.total_credits);
    let spent = money(credits.total_usage);
    let balance = match (total, spent) {
        (Some(t), Some(s)) => Some((t - s).max(0.0)),
        _ => None,
    };
    let credit_window = total
        .filter(|t| *t > 0.0)
        .zip(spent)
        .map(|(t, s)| UsageWindow {
            used_percent: Some(pct_u8(s / t * 100.0)),
            reset_description: None,
            resets_at: None,
            window_minutes: None,
        });

    // A free-tier key has no purchased credit, so a balance gauge would read
    // 0% forever. Naming the tier is the honest thing to show instead.
    let plan = match key.is_free_tier {
        Some(true) => Some("free tier".to_string()),
        _ => key
            .label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    };

    let mut payload = ProviderPayload::live(
        "openrouter",
        "api key",
        UsageSnapshot {
            // The credit gauge leads: it is the account-wide figure, and the
            // key cap is the narrower one sitting under it.
            primary: credit_window,
            secondary: key_window,
            tertiary: None,
            login_method: plan,
            ..UsageSnapshot::at(now)
        },
    );
    // The account balance, and under it the key's own cap when the key has
    // one. Both draw on the same money, which is the whole reason they are one
    // value rather than two unrelated figures.
    let cap = limit.filter(|l| *l > 0.0).map(|l| CreditLimit {
        title: "Key limit".to_string(),
        used: match remaining {
            Some(rem) => (l - rem).max(0.0),
            None => 0.0,
        },
        limit: l,
        // A spend cap on a key does not refill; it is raised by hand.
        resets_at: None,
    });
    if balance.is_some() || cap.is_some() {
        payload.credits = Some(Credits {
            remaining: balance,
            limit: cap,
        });
    }

    payload
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let key = api_key()?;
    let (credits_url, key_url) = endpoints()?;
    let client = http_client(timeout)?;

    let credits: CreditsData = get(&client, &credits_url, &key)?;
    let key_data: KeyData = get(&client, &key_url, &key)?;

    Ok(vec![to_payload(credits, key_data, now)])
}

fn get<T: serde::de::DeserializeOwned>(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
) -> Result<T> {
    let resp = client
        .get(url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("accept", "application/json")
        .send()
        .with_context(|| format!("OpenRouter request failed: {url}"))?;

    check_status(resp.status(), "OpenRouter", "check OPENROUTER_API_KEY")?;

    let text = resp
        .text()
        .with_context(|| format!("OpenRouter read failed: {url}"))?;
    if text.trim().is_empty() {
        return Err(anyhow!("OpenRouter empty response from {url}"));
    }
    let env: Envelope<T> = serde_json::from_str(&text)
        .with_context(|| format!("OpenRouter JSON was invalid: {url}"))?;
    Ok(env.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn credits(total: Option<f64>, usage: Option<f64>) -> CreditsData {
        CreditsData {
            total_credits: total,
            total_usage: usage,
        }
    }

    fn key() -> KeyData {
        KeyData {
            label: Some("laptop".into()),
            limit: None,
            limit_remaining: None,
            usage_daily: Some(1.25),
            usage_weekly: Some(8.0),
            usage_monthly: Some(31.5),
            is_free_tier: Some(false),
        }
    }

    /// Both endpoints wrap their payload, and the wrapper is the same shape for
    /// each - which is the only reason one `get` serves both.
    #[test]
    fn both_endpoints_come_back_wrapped_in_data() {
        let c: Envelope<CreditsData> =
            serde_json::from_str(r#"{"data":{"total_credits":100.0,"total_usage":25.5}}"#).unwrap();
        assert_eq!(c.data.total_credits, Some(100.0));

        let k: Envelope<KeyData> = serde_json::from_str(
            r#"{"data":{"label":"work","limit":null,"limit_remaining":null,
                "usage_daily":1.0,"usage_weekly":2.0,"usage_monthly":3.0,
                "is_free_tier":false}}"#,
        )
        .unwrap();
        assert_eq!(k.data.label.as_deref(), Some("work"));
        assert_eq!(k.data.limit, None, "a key with no cap sends null, not 0");
    }

    /// The balance is what the user watches, and it is a difference: both wire
    /// figures are cumulative over the account's life, so `total_usage` alone
    /// says nothing about what is left.
    #[test]
    fn the_balance_is_credit_bought_minus_credit_spent() {
        let payload = to_payload(credits(Some(100.0), Some(25.5)), key(), at());
        assert_eq!(payload.credits.and_then(|c| c.remaining), Some(74.5));
    }

    /// Spending past the purchased credit must not report a negative balance:
    /// OpenRouter settles usage asynchronously, so usage can briefly exceed
    /// credits, and "-$0.30 left" reads as a bug rather than as an overdraft.
    #[test]
    fn an_overspend_floors_at_zero_rather_than_going_negative() {
        let payload = to_payload(credits(Some(10.0), Some(10.3)), key(), at());
        assert_eq!(payload.credits.and_then(|c| c.remaining), Some(0.0));
    }

    /// The headline gauge is the share of purchased credit already spent.
    #[test]
    fn the_leading_window_is_the_share_of_credit_spent() {
        let payload = to_payload(credits(Some(200.0), Some(50.0)), key(), at());
        let usage = payload.usage.expect("usage");
        assert_eq!(usage.primary.expect("credit window").used_percent, Some(25));
    }

    /// A key cap is the one figure here that behaves like a window, so it gets
    /// one - under the account gauge, because it is the narrower of the two.
    #[test]
    fn a_key_with_a_cap_gets_a_window_under_the_account_one() {
        let mut k = key();
        k.limit = Some(50.0);
        k.limit_remaining = Some(12.5);
        let payload = to_payload(credits(Some(100.0), Some(10.0)), k, at());
        let usage = payload.usage.expect("usage");
        assert_eq!(usage.primary.expect("credit window").used_percent, Some(10));
        assert_eq!(usage.secondary.expect("key window").used_percent, Some(75));
    }

    /// Most keys have no cap, and a missing cap must not become a 0% bar - an
    /// empty gauge reads as "nothing used" rather than "no such limit".
    #[test]
    fn a_key_with_no_cap_draws_no_second_window() {
        let payload = to_payload(credits(Some(100.0), Some(10.0)), key(), at());
        assert!(payload.usage.expect("usage").secondary.is_none());
    }

    /// The three `usage_*` fields look like TokenGauge's three windows and are
    /// not: they are money already spent, with nothing to be a fraction of.
    /// They are read off the wire and deliberately go nowhere yet - routing
    /// them needs a channel for a provider that reports its own cost, which
    /// does not exist. What this holds is that they never become a window.
    #[test]
    fn period_spend_is_read_but_never_becomes_a_window() {
        let parsed: Envelope<KeyData> = serde_json::from_str(
            r#"{"data":{"usage_daily":1.25,"usage_weekly":8.0,"usage_monthly":31.5}}"#,
        )
        .unwrap();
        assert_eq!(parsed.data.usage_daily, Some(1.25));
        assert_eq!(parsed.data.usage_monthly, Some(31.5));

        let payload = to_payload(credits(Some(100.0), Some(10.0)), key(), at());
        let usage = payload.usage.expect("usage");
        assert!(usage.tertiary.is_none(), "monthly spend became a window");
        assert!(
            usage.secondary.is_none(),
            "this key has no cap, so nothing but the credit gauge is drawn"
        );
    }

    /// A free-tier key has no purchased credit, so the balance gauge would read
    /// 0% forever. Say what the account is instead.
    #[test]
    fn a_free_tier_key_is_named_rather_than_gauged() {
        let mut k = key();
        k.is_free_tier = Some(true);
        let payload = to_payload(credits(Some(0.0), Some(0.0)), k, at());
        let usage = payload.usage.expect("usage");
        assert!(
            usage.primary.is_none(),
            "nothing purchased, nothing to gauge"
        );
        assert_eq!(usage.login_method.as_deref(), Some("free tier"));
    }

    /// Anything not finite or below zero is dropped rather than drawn. A bar
    /// filled off a NaN is worse than a bar that is absent.
    #[test]
    fn a_figure_that_is_not_money_is_dropped_rather_than_drawn() {
        assert_eq!(money(Some(1.5)), Some(1.5));
        assert_eq!(money(Some(0.0)), Some(0.0));
        assert_eq!(money(None), None);
        assert_eq!(money(Some(-1.0)), None);
        assert_eq!(money(Some(f64::NAN)), None);
        assert_eq!(money(Some(f64::INFINITY)), None);

        let payload = to_payload(credits(Some(f64::NAN), Some(10.0)), key(), at());
        assert!(payload.usage.expect("usage").primary.is_none());
        assert!(payload.credits.is_none());
    }

    /// A self-hosted gateway can move the base, and whatever is pasted in has
    /// to land on the same two endpoints.
    #[test]
    fn a_base_url_lands_on_both_endpoints_however_it_was_pasted() {
        let (c, k) = endpoints_for(None).unwrap();
        assert_eq!(c, "https://openrouter.ai/api/v1/credits");
        assert_eq!(k, "https://openrouter.ai/api/v1/key");

        let (c, k) = endpoints_for(Some("https://gw.example.test/v1/")).unwrap();
        assert_eq!(c, "https://gw.example.test/v1/credits");
        assert_eq!(k, "https://gw.example.test/v1/key");
    }

    /// The key travels as a bearer header, so a base that downgrades the
    /// transport would put it on the wire in the clear.
    #[test]
    fn a_base_url_that_is_not_https_is_refused() {
        for bad in [
            "http://openrouter.ai/api/v1",
            "ftp://example.test",
            "example.test",
            "httpsx://example.test",
        ] {
            let err = endpoints_for(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("HTTPS"), "{bad} was accepted: {err}");
        }
    }
}
