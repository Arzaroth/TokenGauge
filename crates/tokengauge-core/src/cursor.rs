//! Native Cursor usage fetcher (cursor.com).
//!
//! Cursor has no public usage API. What it has is the endpoint its own
//! dashboard calls, `GET /api/usage-summary`, which authenticates with a
//! session cookie rather than a key - and the raw material for that cookie is
//! already on disk, put there by Cursor itself.
//!
//! **Two sources, in this order:**
//!
//! 1. `~/.config/cursor/auth.json` - plain JSON, written by the `cursor-agent`
//!    CLI. Read first because it costs nothing: a file and a JSON parse.
//! 2. `.../Cursor/User/globalStorage/state.vscdb` - the IDE's own SQLite
//!    state, under the key `cursorAuth/accessToken`. **Not read here.** It
//!    holds the same token, but reaching it means a SQLite dependency, and
//!    every dependency in this crate is justified in a comment as adding no
//!    new crate. Worth adding only for someone who runs the IDE and never the
//!    CLI; until that is asked for, the cheap source is the whole story.
//!
//! Either way the token is a JWT whose `sub` claim is `<issuer>|<userId>`, and
//! the cookie the dashboard sends is `userId%3A%3Atoken` - a literal,
//! pre-encoded `::`. The endpoint also gates on browser-shaped headers, so
//! `Origin`, `Referer` and a `User-Agent` go with it.
//!
//! What comes back is unusually well-suited to the panel: three ready
//! percentages over one billing cycle, and an on-demand cap that is a
//! [`CreditLimit`] rather than a window.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::provider::{check_status, jwt_claims};
use crate::{
    CreditLimit, CreditLimitKind, Credits, ProviderPayload, UsageSnapshot, UsageWindow,
    http_client, pct_u8,
};

const DEFAULT_BASE_URL: &str = "https://cursor.com";
const BASE_URL_ENV: &str = "CURSOR_BASE_URL";
const TOKEN_ENVS: &[&str] = &["CURSOR_ACCESS_TOKEN", "CURSOR_SESSION_TOKEN"];

/// The dashboard rejects a request that does not look like a browser. Its own
/// value, not a disguise: this *is* the dashboard's call, made by the user's
/// own machine with the user's own session.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Where the `cursor-agent` CLI keeps its login. Cursor's own convention, not
/// ours - the same directory the CLI writes its config into.
pub(crate) fn agent_auth_path() -> PathBuf {
    if let Some(dir) = env_clean("CURSOR_CONFIG_DIR") {
        return PathBuf::from(dir).join("auth.json");
    }
    dirs::config_dir()
        .unwrap_or_default()
        .join("cursor")
        .join("auth.json")
}

#[derive(Debug, Deserialize)]
struct AgentAuth {
    #[serde(default, alias = "access_token")]
    access_token: Option<String>,
}

/// The session token: the environment first, then the file the CLI wrote.
///
/// A present file says nothing about whether it holds a token - the lesson
/// `claude.rs` records twice over - so an empty or tokenless file reads as
/// "not signed in" rather than being handed on as a credential.
pub(crate) fn access_token() -> Result<String> {
    if let Some(token) = TOKEN_ENVS.iter().find_map(|name| env_clean(name)) {
        return Ok(token);
    }
    let path = agent_auth_path();
    let raw = std::fs::read_to_string(&path).map_err(|_| {
        anyhow!("Cursor not signed in - run `cursor-agent` and log in, or set CURSOR_ACCESS_TOKEN")
    })?;
    let auth: AgentAuth = serde_json::from_str(&raw)
        .with_context(|| format!("Cursor auth file was invalid: {}", path.display()))?;
    auth.access_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Cursor auth file holds no token - run `cursor-agent` to log in"))
}

/// The cookie value the dashboard sends: the user id off the token's `sub`
/// claim, then a pre-encoded `::`, then the token itself.
fn session_cookie(token: &str) -> Result<String> {
    let claims = jwt_claims(token)
        .ok_or_else(|| anyhow!("Cursor token is not a readable JWT - log in again"))?;
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Cursor token carries no `sub` claim - log in again"))?;
    let user_id = sub
        .split('|')
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Cursor token `sub` is not `issuer|userId` - log in again"))?;
    Ok(format!("{user_id}%3A%3A{token}"))
}

fn request_base(override_base: Option<&str>) -> Result<&str> {
    let base = override_base
        .unwrap_or(DEFAULT_BASE_URL)
        .trim_end_matches('/');
    if !base.starts_with("https://") {
        return Err(anyhow!("Cursor base URL must use HTTPS"));
    }
    Ok(base)
}

fn summary_url(override_base: Option<&str>) -> Result<String> {
    Ok(format!(
        "{}/api/usage-summary",
        request_base(override_base)?
    ))
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct UsageSummary {
    #[serde(rename = "membershipType", default)]
    membership_type: Option<String>,
    #[serde(rename = "isUnlimited", default)]
    is_unlimited: bool,
    #[serde(rename = "billingCycleEnd", default)]
    billing_cycle_end: Option<String>,
    #[serde(rename = "individualUsage", default)]
    individual_usage: Option<Usage>,
    #[serde(rename = "teamUsage", default)]
    team_usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
struct Usage {
    #[serde(default)]
    plan: Option<Plan>,
    #[serde(rename = "onDemand", default)]
    on_demand: Option<OnDemand>,
}

#[derive(Debug, Clone, Deserialize)]
struct Plan {
    /// The dashboard headline: the whole included allowance.
    #[serde(rename = "totalPercentUsed", default)]
    total_percent_used: Option<f64>,
    /// The "Cursor Models" pool - Auto and Composer.
    #[serde(rename = "autoPercentUsed", default)]
    auto_percent_used: Option<f64>,
    /// The "Other Models" pool - named and third-party.
    #[serde(rename = "apiPercentUsed", default)]
    api_percent_used: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct OnDemand {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    used: Option<f64>,
    #[serde(default)]
    limit: Option<f64>,
}

/// A percentage that is one. Cursor's own dashboard treats these as
/// authoritative, so a value outside 0-100 is the endpoint having drifted
/// rather than a reading worth drawing.
fn percent(value: Option<f64>) -> Option<u8> {
    value
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        .map(pct_u8)
}

fn window(value: Option<f64>, resets_at: Option<&str>) -> Option<UsageWindow> {
    Some(UsageWindow {
        used_percent: Some(percent(value)?),
        reset_description: None,
        resets_at: resets_at.map(str::to_string),
        window_minutes: None,
    })
}

fn to_payload(body: UsageSummary, now: DateTime<Utc>) -> Result<ProviderPayload> {
    // A team seat reports its usage under `teamUsage` and an individual under
    // `individualUsage`; nobody has both, and which one answered is not
    // something the panel needs to say.
    let usage = body
        .individual_usage
        .or(body.team_usage)
        .ok_or_else(|| anyhow!("Cursor reported no usage for this account"))?;

    let resets = body
        .billing_cycle_end
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let plan = usage.plan.unwrap_or(Plan {
        total_percent_used: None,
        auto_percent_used: None,
        api_percent_used: None,
    });

    // The headline leads, then the two pools it is made of. All three share one
    // billing cycle, so they share one reset.
    let primary = window(plan.total_percent_used, resets);
    let secondary = window(plan.auto_percent_used, resets);
    let tertiary = window(plan.api_percent_used, resets);

    // An unlimited plan has no allowance to be a fraction of, so it reports no
    // percentage - saying so beats drawing an empty bar.
    if primary.is_none() && secondary.is_none() && tertiary.is_none() && !body.is_unlimited {
        return Err(anyhow!("Cursor reported no usable usage figure"));
    }

    let plan_label = match (&body.membership_type, body.is_unlimited) {
        (Some(m), true) if !m.trim().is_empty() => Some(format!("{} · unlimited", m.trim())),
        (Some(m), false) if !m.trim().is_empty() => Some(m.trim().to_string()),
        (_, true) => Some("unlimited".to_string()),
        _ => None,
    };

    let mut payload = ProviderPayload::live(
        "cursor",
        "session",
        UsageSnapshot {
            primary,
            secondary,
            tertiary,
            login_method: plan_label,
            ..UsageSnapshot::at(now)
        },
    );

    // On-demand spend past the included allowance: a cap, with money on both
    // sides of it. Only when the user has actually turned it on - a disabled
    // on-demand is not a cap of zero, it is no cap.
    if let Some(on_demand) = usage.on_demand.filter(|o| o.enabled) {
        let limit = on_demand.limit.filter(|v| v.is_finite() && *v >= 0.0);
        let used = on_demand.used.filter(|v| v.is_finite() && *v >= 0.0);
        if let (Some(limit), Some(used)) = (limit, used) {
            payload.credits = Some(Credits {
                // Cursor bills on-demand against a card rather than a prepaid
                // balance, so there is a cap and nothing behind it.
                remaining: None,
                limit: Some(CreditLimit {
                    kind: CreditLimitKind::OnDemand,
                    used,
                    limit,
                    resets: resets.map(str::to_string),
                }),
            });
        }
    }

    Ok(payload)
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let token = access_token()?;
    let cookie = session_cookie(&token)?;
    let base = env_clean(BASE_URL_ENV);
    let origin = request_base(base.as_deref())?;
    let url = summary_url(Some(origin))?;
    let client = http_client(timeout)?;

    let resp = client
        .get(&url)
        .header("cookie", format!("WorkosCursorSessionToken={cookie}"))
        .header("origin", origin)
        .header("referer", format!("{origin}/dashboard"))
        .header("user-agent", BROWSER_UA)
        .header("accept", "application/json")
        .send()
        .context("Cursor usage request failed")?;

    check_status(
        resp.status(),
        "Cursor",
        "sign in with `cursor-agent`, or set CURSOR_ACCESS_TOKEN",
    )?;

    let text = resp.text().context("Cursor usage read failed")?;
    if text.trim().is_empty() {
        return Err(anyhow!("Cursor returned an empty response"));
    }
    let body: UsageSummary =
        serde_json::from_str(&text).context("Cursor usage JSON was invalid")?;

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

    const FULL: &str = r#"{
        "membershipType": "pro",
        "isUnlimited": false,
        "billingCycleEnd": "2026-10-01T00:00:00Z",
        "individualUsage": {
            "plan": {"totalPercentUsed": 62.5, "autoPercentUsed": 40.0, "apiPercentUsed": 85.0},
            "onDemand": {"enabled": true, "used": 12.5, "limit": 50.0}
        }
    }"#;

    /// A base64url JWT payload carrying one `sub`, built by hand so the test
    /// does not need a signing library for a token nothing verifies.
    fn token_with_sub(sub: &str) -> String {
        fn b64(bytes: &[u8]) -> String {
            const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for i in 0..chunk.len() + 1 {
                    out.push(A[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
                }
            }
            out
        }
        let payload = serde_json::json!({"sub": sub}).to_string();
        format!("header.{}.signature", b64(payload.as_bytes()))
    }

    /// The headline leads and the two pools it is made of sit under it, all
    /// three sharing the one billing cycle they are measured over.
    #[test]
    fn the_three_pools_map_onto_the_three_windows() {
        let usage = parse(FULL).expect("payload").usage.expect("usage");

        assert_eq!(
            usage.primary.as_ref().expect("total").used_percent,
            Some(63)
        );
        assert_eq!(
            usage.secondary.as_ref().expect("auto").used_percent,
            Some(40)
        );
        assert_eq!(usage.tertiary.as_ref().expect("api").used_percent, Some(85));
        for w in [&usage.primary, &usage.secondary, &usage.tertiary] {
            assert_eq!(
                w.as_ref().unwrap().resets_at.as_deref(),
                Some("2026-10-01T00:00:00Z"),
                "every pool resets with the one billing cycle"
            );
        }
        assert_eq!(usage.login_method.as_deref(), Some("pro"));
    }

    /// On-demand is a cap with money on both sides, which is a `CreditLimit`
    /// rather than a window: it is spend past the allowance, not a share of it.
    #[test]
    fn on_demand_spending_is_a_cap_rather_than_a_window() {
        let credits = parse(FULL).expect("payload").credits.expect("credits");
        let cap = credits.limit.expect("cap");

        assert_eq!(cap.kind, CreditLimitKind::OnDemand);
        assert_eq!(cap.used, 12.5);
        assert_eq!(cap.limit, 50.0);
        assert_eq!(cap.remaining(), 37.5);
        assert_eq!(cap.used_percent(), Some(25));
        assert_eq!(
            credits.remaining, None,
            "Cursor bills on-demand to a card; there is no balance behind the cap"
        );
    }

    /// On-demand switched off is *no cap*, not a cap of zero. Drawing one
    /// would tell a user they had hit a limit they never enabled.
    #[test]
    fn on_demand_switched_off_draws_no_cap_at_all() {
        let raw = FULL.replace(r#""enabled": true"#, r#""enabled": false"#);
        assert!(parse(&raw).expect("payload").credits.is_none());
    }

    /// An unlimited plan has no allowance to be a fraction of. Saying so is
    /// better than an empty bar, and it must not read as a failed fetch.
    #[test]
    fn an_unlimited_plan_is_named_rather_than_gauged() {
        let payload = parse(
            r#"{"membershipType":"enterprise","isUnlimited":true,
                "individualUsage":{"plan":{}}}"#,
        )
        .expect("an unlimited plan is not an error");
        let usage = payload.usage.expect("usage");
        assert!(usage.primary.is_none());
        assert_eq!(
            usage.login_method.as_deref(),
            Some("enterprise · unlimited")
        );
    }

    /// A team seat reports under a different key and is otherwise identical.
    #[test]
    fn a_team_seat_reads_the_same_as_an_individual_one() {
        let raw = FULL.replace("individualUsage", "teamUsage");
        let usage = parse(&raw).expect("payload").usage.expect("usage");
        assert_eq!(usage.primary.expect("total").used_percent, Some(63));
    }

    /// A limited plan reporting nothing usable is a failure rather than an
    /// empty panel - serving it would blank yesterday's figures and call that
    /// a successful fetch.
    #[test]
    fn a_limited_plan_with_no_figures_is_an_error() {
        for raw in [
            r#"{"individualUsage":{"plan":{}}}"#,
            r#"{"individualUsage":{"plan":{"totalPercentUsed":null}}}"#,
            r#"{}"#,
        ] {
            assert!(parse(raw).is_err(), "{raw} was accepted");
        }
    }

    /// Cursor's dashboard treats these percentages as authoritative, so one
    /// outside 0-100 is drift rather than a reading worth drawing.
    #[test]
    fn a_percentage_outside_the_range_is_dropped() {
        assert_eq!(percent(Some(0.0)), Some(0));
        assert_eq!(percent(Some(62.5)), Some(63));
        assert_eq!(percent(Some(100.0)), Some(100));
        assert_eq!(percent(Some(-1.0)), None);
        assert_eq!(percent(Some(101.0)), None);
        assert_eq!(percent(Some(f64::NAN)), None);
        assert_eq!(percent(None), None);
    }

    /// The cookie is the user id off the `sub` claim, a pre-encoded `::`, then
    /// the token - which is what the dashboard's own JS sends.
    #[test]
    fn the_cookie_is_the_user_id_and_the_token() {
        let token = token_with_sub("auth0|user_01ABC");
        let cookie = session_cookie(&token).expect("cookie");
        assert!(cookie.starts_with("user_01ABC%3A%3A"), "{cookie}");
        assert!(cookie.ends_with(&token), "the token itself follows");
    }

    /// A token that is not a usable JWT is a credential problem, and each way
    /// it can fail says which.
    #[test]
    fn a_token_that_is_not_a_usable_jwt_says_so() {
        for (token, want) in [
            ("not-a-jwt", "readable JWT"),
            (token_with_sub("").as_str(), "issuer|userId"),
            (token_with_sub("noseparator").as_str(), "issuer|userId"),
            (token_with_sub("auth0|").as_str(), "issuer|userId"),
        ] {
            let err = session_cookie(token).unwrap_err().to_string();
            assert!(err.contains(want), "{token}: {err}");
        }
    }

    /// The token travels in a cookie, so a base that downgrades the transport
    /// would put a live session on the wire in the clear.
    #[test]
    fn a_base_url_that_is_not_https_is_refused() {
        assert_eq!(
            summary_url(None).unwrap(),
            "https://cursor.com/api/usage-summary"
        );
        assert_eq!(
            summary_url(Some("https://proxy.test/")).unwrap(),
            "https://proxy.test/api/usage-summary"
        );
        for bad in ["http://cursor.com", "cursor.com", "ftp://cursor.com"] {
            let err = summary_url(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("HTTPS"), "{bad} was accepted: {err}");
        }
    }

    /// `Origin` carries a serialized origin, which has no trailing slash, and
    /// the same string has to build the URL or the two describe different hosts.
    #[test]
    fn a_trailing_slash_on_the_base_reaches_neither_the_url_nor_the_headers() {
        let origin = request_base(Some("https://proxy.test/")).unwrap();
        assert_eq!(origin, "https://proxy.test");
        assert_eq!(
            format!("{origin}/dashboard"),
            "https://proxy.test/dashboard"
        );
        assert_eq!(
            summary_url(Some(origin)).unwrap(),
            "https://proxy.test/api/usage-summary"
        );
    }
}
