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
//! **Two credentials, and only one of them is required.** `/key` describes the
//! key that authenticated the request and answers to any key. `/credits` is
//! account-wide and answers only to a *management* key - anything else gets
//! `403 Only management keys can perform this operation`. So the account
//! balance is optional: a user with just an inference key still gets their
//! key's cap and their spend, and a 403 on `/credits` drops the balance
//! instead of failing the provider. Asking for a management key to show any
//! OpenRouter data at all would be a worse trade than showing less.
//!
//! Both are read from the environment. There is no CLI to read: an OpenRouter
//! key is pasted into whichever agent routes through it, so no file on disk
//! reliably holds one.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::provider::check_status;
use crate::{
    CreditLimit, CreditLimitKind, Credits, ProviderPayload, UsageSnapshot, UsageWindow,
    http_client, pct_u8,
};

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const BASE_URL_ENV: &str = "OPENROUTER_BASE_URL";
const API_KEY_ENVS: &[&str] = &["OPENROUTER_API_KEY", "OPENROUTER_KEY"];
/// Optional, and the only way to see the account balance. A management key is
/// minted separately at openrouter.ai/settings/keys.
pub(crate) const MANAGEMENT_KEY_ENVS: &[&str] =
    &["OPENROUTER_MANAGEMENT_KEY", "OPENROUTER_PROVISIONING_KEY"];

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

/// The management key, when there is one. Its absence is not an error: it
/// costs the account balance and nothing else.
fn management_key() -> Option<String> {
    MANAGEMENT_KEY_ENVS.iter().find_map(|name| env_clean(name))
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
#[derive(Debug, Clone, Default, Deserialize)]
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
    /// How the cap refills: `daily`, `weekly`, `monthly`, or null when it never
    /// does. A period name, not a timestamp.
    #[serde(default)]
    limit_reset: Option<String>,
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
    // A gauge needs a denominator, so a zero cap gets a row but no bar.
    let key_window = match (limit, remaining) {
        (Some(l), Some(rem)) if l > 0.0 => Some(UsageWindow {
            used_percent: Some(pct_u8((l - rem).max(0.0) / l * 100.0)),
            reset_description: key.limit_reset.clone(),
            resets_at: None,
            window_minutes: None,
        }),
        _ => None,
    };

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
    //
    // `limit` and `limit_remaining` move together: both null is an unlimited
    // key. A finite cap with no remainder is a shape OpenRouter does not
    // document, and there is no way to say how much of it is gone - so the cap
    // is dropped rather than drawn at a fabricated 0% used. Only the cap: the
    // balance and the windows in the same response are still good.
    let cap = match (limit, remaining) {
        (Some(l), Some(rem)) => Some(CreditLimit {
            kind: CreditLimitKind::Key,
            used: (l - rem).max(0.0),
            limit: l,
            resets: key
                .limit_reset
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        }),
        _ => None,
    };
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

    // `/key` first, and it is the one that must succeed: it describes the
    // credential the user actually has and carries the cap and the spend.
    let key_data: KeyData = get(&client, &key_url, &key)?;

    // `/credits` is account-wide and only a management key may ask. Without
    // one - which is the common case - the balance is simply absent, and a
    // provider with a cap and no balance is far better than no provider.
    let credits = management_key()
        .unwrap_or(key)
        .pipe(|k| get::<CreditsData>(&client, &credits_url, &k))
        .unwrap_or_default();

    Ok(vec![to_payload(credits, key_data, now)])
}

/// Small enough not to want a crate for it.
trait Pipe: Sized {
    fn pipe<T>(self, f: impl FnOnce(Self) -> T) -> T {
        f(self)
    }
}
impl<T> Pipe for T {}

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
            limit_reset: None,
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
    /// one - under the account gauge, because it is the narrower of the two -
    /// and it carries its refill period, which OpenRouter reports as a word
    /// rather than a date.
    #[test]
    fn a_key_with_a_cap_gets_a_window_under_the_account_one() {
        let mut k = key();
        k.limit = Some(50.0);
        k.limit_remaining = Some(12.5);
        k.limit_reset = Some("monthly".into());
        let payload = to_payload(credits(Some(100.0), Some(10.0)), k, at());
        let usage = payload.usage.expect("usage");
        assert_eq!(usage.primary.expect("credit window").used_percent, Some(10));
        assert_eq!(usage.secondary.expect("key window").used_percent, Some(75));

        let cap = payload.credits.expect("credits").limit.expect("cap");
        assert_eq!(cap.kind, CreditLimitKind::Key);
        assert_eq!(cap.remaining(), 12.5);
        assert_eq!(cap.used_percent(), Some(75));
        assert_eq!(cap.resets.as_deref(), Some("monthly"));
    }

    /// A cap of zero is a key that may spend nothing - a real state, and one
    /// worth saying, because otherwise a user whose requests all fail sees a
    /// panel with no explanation on it. It draws as a row with no bar, since
    /// zero cannot be a denominator.
    #[test]
    fn a_zero_cap_is_shown_rather_than_dropped() {
        let mut k = key();
        k.limit = Some(0.0);
        k.limit_remaining = Some(0.0);
        let payload = to_payload(credits(Some(100.0), Some(10.0)), k, at());
        let cap = payload
            .credits
            .expect("credits")
            .limit
            .expect("a zero cap is still a cap");
        assert_eq!(cap.limit, 0.0);
        assert_eq!(cap.used_percent(), None, "zero cannot be a denominator");
        assert!(
            payload.usage.expect("usage").secondary.is_none(),
            "and it draws no gauge"
        );
    }

    /// `limit` and `limit_remaining` move together - both null is an unlimited
    /// key. A finite cap with no remainder is undocumented, and there is no way
    /// to say how much of it is gone; drawing it at 0% used would invent the
    /// one number the response withheld.
    #[test]
    fn a_cap_with_no_remainder_is_dropped_rather_than_invented() {
        let mut k = key();
        k.limit = Some(50.0);
        k.limit_remaining = None;
        let payload = to_payload(credits(Some(100.0), Some(10.0)), k, at());

        assert!(
            payload.credits.as_ref().expect("credits").limit.is_none(),
            "a cap was drawn from a response that never said how much was used"
        );
        assert!(payload.usage.as_ref().expect("usage").secondary.is_none());
        // Only the cap is dropped: the rest of the response is still good.
        assert_eq!(
            payload.credits.expect("credits").remaining,
            Some(90.0),
            "the balance went with it"
        );
    }

    /// Most keys have no cap, and a missing cap must not become a 0% bar - an
    /// empty gauge reads as "nothing used" rather than "no such limit".
    #[test]
    fn a_key_with_no_cap_draws_no_second_window() {
        let payload = to_payload(credits(Some(100.0), Some(10.0)), key(), at());
        assert!(payload.usage.expect("usage").secondary.is_none());
        assert!(payload.credits.expect("credits").limit.is_none());
    }

    /// The common case: an inference key, so `/credits` answered 403 and the
    /// balance is absent. The provider still has everything else.
    #[test]
    fn a_key_with_no_account_balance_is_still_a_provider() {
        let mut k = key();
        k.limit = Some(20.0);
        k.limit_remaining = Some(5.0);
        let payload = to_payload(CreditsData::default(), k, at());

        let credits = payload
            .credits
            .expect("a cap with no balance is still credits");
        assert_eq!(credits.remaining, None, "no management key, no balance");
        assert_eq!(credits.limit.expect("cap").used_percent(), Some(75));
        assert!(
            payload.usage.expect("usage").primary.is_none(),
            "nothing to gauge the account against"
        );
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
