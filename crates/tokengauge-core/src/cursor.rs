//! Native Cursor usage fetcher (cursor.com).
//!
//! Cursor has no public usage API. What it has is the endpoint its own
//! dashboard calls, `GET /api/usage-summary`, which authenticates with a
//! session cookie rather than a key - and the raw material for that cookie is
//! already on disk, put there by Cursor itself.
//!
//! **The sources, in this order:**
//!
//! 1. `~/.config/cursor/auth.json` (`~/.cursor/auth.json` on macOS) - plain
//!    JSON, written by the `cursor-agent` CLI. Read first because it costs
//!    nothing: a file and a JSON parse.
//! 2. On macOS, the Keychain items `cursor-access-token` /
//!    `cursor-refresh-token` under `cursor-user`, where the CLI normally keeps
//!    its login instead of the file.
//! 3. `.../Cursor/User/globalStorage/state.vscdb` - the IDE's own SQLite
//!    state, under the key `cursorAuth/accessToken`. **Not read here.** It
//!    holds the same token, but reaching it means a SQLite dependency, and
//!    every dependency in this crate is justified in a comment as adding no
//!    new crate. Worth adding only for someone who runs the IDE and never the
//!    CLI; until that is asked for, the cheap source is the whole story.
//!
//! Either way the token is a JWT whose `sub` claim is `<issuer>|<userId>`, and
//! the cookie the dashboard sends is `userId%3A%3Atoken` - a literal,
//! pre-encoded `::`. The endpoint also sits behind a firewall that gates on
//! browser-shaped headers, so `Origin`, `Referer`, a `User-Agent` and the
//! client hints a browser sends beside it go with it.
//!
//! What comes back is unusually well-suited to the panel: three ready
//! percentages over one billing cycle, and an on-demand cap that is a
//! [`CreditLimit`] rather than a window.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::credentials::{LiveLogin, LoginTokens};
use crate::provider::{check_status, jwt_claims};
use crate::{
    CredentialState, CreditLimit, CreditLimitKind, Credits, ProviderPayload, UsageSnapshot,
    UsageWindow, http_client, pct_u8,
};

const DEFAULT_BASE_URL: &str = "https://cursor.com";
const BASE_URL_ENV: &str = "CURSOR_BASE_URL";
const TOKEN_ENVS: &[&str] = &["CURSOR_ACCESS_TOKEN", "CURSOR_SESSION_TOKEN"];

/// The dashboard rejects a request that does not look like a browser. Its own
/// value, not a disguise: this *is* the dashboard's call, made by the user's
/// own machine with the user's own session.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";

/// The rest of what that browser sends. The endpoint sits behind Vercel's
/// firewall, which negotiates client hints and refuses a client that sends a
/// user agent and nothing else beside it, with a 403 and a valid cookie.
const BROWSER_HEADERS: &[(&str, &str)] = &[
    ("user-agent", BROWSER_UA),
    ("accept", "application/json, text/plain, */*"),
    ("accept-language", "en-US,en;q=0.9"),
    (
        "sec-ch-ua",
        "\"Chromium\";v=\"140\", \"Not=A?Brand\";v=\"24\", \"Google Chrome\";v=\"140\"",
    ),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"Linux\""),
    ("sec-fetch-dest", "empty"),
    ("sec-fetch-mode", "cors"),
    ("sec-fetch-site", "same-origin"),
];

fn dashboard_request(
    request: reqwest::blocking::RequestBuilder,
    origin: &str,
    cookie: &str,
) -> reqwest::blocking::RequestBuilder {
    BROWSER_HEADERS.iter().fold(
        request
            .header("cookie", format!("WorkosCursorSessionToken={cookie}"))
            .header("origin", origin)
            .header("referer", format!("{origin}/dashboard")),
        |request, (name, value)| request.header(*name, *value),
    )
}

/// The firewall's refusal and a dead session are both a 403 here, so the
/// shared "check your plan" reading would be wrong either way.
fn check_cursor_status(status: reqwest::StatusCode, unauthorized_hint: &str) -> Result<()> {
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!(
            "Cursor refused the request - the session may have ended, or cursor.com's edge blocked it; {unauthorized_hint} if it persists"
        ));
    }
    check_status(status, "Cursor", unauthorized_hint)
}

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Where the `cursor-agent` CLI keeps its login. Cursor's own convention, not
/// ours, and only Linux and Windows follow the config-directory one: on macOS
/// it is `~/.cursor`, not `~/Library/Application Support`.
pub(crate) fn agent_auth_path() -> PathBuf {
    if let Some(dir) = env_clean("CURSOR_CONFIG_DIR") {
        return PathBuf::from(dir).join("auth.json");
    }
    agent_auth_path_in(
        &dirs::home_dir().unwrap_or_default(),
        &dirs::config_dir().unwrap_or_default(),
        cfg!(target_os = "macos"),
    )
}

fn agent_auth_path_in(home: &Path, config: &Path, macos: bool) -> PathBuf {
    if macos {
        home.join(".cursor").join("auth.json")
    } else {
        config.join("cursor").join("auth.json")
    }
}

/// On macOS the CLI normally keeps its login in the Keychain and writes no
/// file at all.
#[cfg(target_os = "macos")]
fn keychain_login() -> Option<AgentAuth> {
    let read = |service: &str| {
        keyring::Entry::new(service, "cursor-user")
            .ok()?
            .get_password()
            .ok()
    };
    Some(AgentAuth {
        access_token: Some(read("cursor-access-token")?),
        refresh_token: read("cursor-refresh-token"),
    })
}

#[cfg(not(target_os = "macos"))]
fn keychain_login() -> Option<AgentAuth> {
    None
}

/// cursor-agent writes camelCase: `{"accessToken", "refreshToken", "apiKey"}`.
#[derive(Debug, Deserialize)]
struct AgentAuth {
    #[serde(default, rename = "accessToken", alias = "access_token")]
    access_token: Option<String>,
    #[serde(default, rename = "refreshToken", alias = "refresh_token")]
    refresh_token: Option<String>,
}

/// The session token and where it came from: the environment first, then the
/// file the CLI wrote, then the Keychain.
///
/// A present file says nothing about whether it holds a token - the lesson
/// `claude.rs` records twice over - so an empty or tokenless file reads as
/// "not signed in" rather than being handed on as a credential, and does not
/// shadow a Keychain login behind it.
pub(crate) fn access_token() -> Result<(String, String)> {
    if let Some((name, token)) = TOKEN_ENVS
        .iter()
        .find_map(|name| Some((name, env_clean(name)?)))
    {
        return Ok((token, format!("{name} set")));
    }
    let path = agent_auth_path();
    let from_file = std::fs::read_to_string(&path).ok().map(|raw| {
        serde_json::from_str::<AgentAuth>(&raw)
            .with_context(|| format!("Cursor auth file was invalid: {}", path.display()))
    });
    if let Some(token) = from_file
        .as_ref()
        .and_then(|auth| clean(auth.as_ref().ok()?.access_token.clone()))
    {
        return Ok((token, path.display().to_string()));
    }
    if let Some(token) = keychain_login().and_then(|auth| clean(auth.access_token)) {
        return Ok((token, "macOS Keychain".to_string()));
    }
    match from_file {
        None => Err(anyhow!(
            "Cursor not signed in - run `cursor-agent` and log in, or set CURSOR_ACCESS_TOKEN"
        )),
        Some(Err(e)) => Err(e),
        Some(Ok(_)) => Err(anyhow!(
            "Cursor auth file holds no token - run `cursor-agent` to log in"
        )),
    }
}

/// The user id in the token's `sub` claim, which reads `issuer|userId`.
/// remuda files a Cursor login under the same.
fn user_id(token: &str) -> Result<String> {
    let claims = jwt_claims(token)
        .ok_or_else(|| anyhow!("Cursor token is not a readable JWT - log in again"))?;
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Cursor token carries no `sub` claim - log in again"))?;
    sub.split('|')
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("Cursor token `sub` is not `issuer|userId` - log in again"))
}

/// The cookie value the dashboard sends: the user id off the token's `sub`
/// claim, then a pre-encoded `::`, then the token itself.
fn session_cookie(token: &str) -> Result<String> {
    Ok(format!("{}%3A%3A{token}", user_id(token)?))
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

/// What a `membershipType` is sold as. A type not listed reads as sent.
fn plan_label(membership: &str) -> String {
    let name = match membership.to_ascii_lowercase().as_str() {
        "free" => "Free",
        "hobby" => "Hobby",
        "free_trial" => "Pro Trial",
        "pro" => "Pro",
        "pro_student" => "Pro Student",
        "pro_plus" => "Pro+",
        "ultra" => "Ultra",
        "team" => "Teams",
        "enterprise" => "Enterprise",
        _ => return membership.to_string(),
    };
    format!("Cursor {name}")
}

/// A plan's nominal multiplier against Pro: Pro+ is sold as 3x its Agent
/// limits and Ultra as 20x.
///
/// `None` for a Teams seat: Standard is a Pro's allowance and Premium five
/// times that, but the seat type is not in `membershipType`, and guessing 1x
/// would understate a Premium seat without saying so.
fn plan_weight(membership: &str) -> Option<u32> {
    match membership.to_ascii_lowercase().as_str() {
        "pro" | "pro_student" => Some(1),
        "pro_plus" => Some(3),
        "ultra" => Some(20),
        _ => None,
    }
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

    let membership = body
        .membership_type
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let plan_label = match (membership.map(plan_label), body.is_unlimited) {
        (Some(m), true) => Some(format!("{m} · unlimited")),
        (Some(m), false) => Some(m),
        (None, true) => Some("unlimited".to_string()),
        (None, false) => None,
    };
    let weight = membership.and_then(plan_weight);

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
    payload.credential.plan_weight = weight.map(f64::from);

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
    let (token, _) = access_token()?;
    Ok(vec![usage_for(
        &token,
        timeout,
        now,
        "sign in with `cursor-agent`, or set CURSOR_ACCESS_TOKEN",
    )?])
}

fn usage_for(
    token: &str,
    timeout: Duration,
    now: DateTime<Utc>,
    unauthorized_hint: &str,
) -> Result<ProviderPayload> {
    let cookie = session_cookie(token)?;
    let base = env_clean(BASE_URL_ENV);
    let origin = request_base(base.as_deref())?;
    let url = summary_url(Some(origin))?;
    let client = http_client(timeout)?;

    let resp = dashboard_request(client.get(&url), origin, &cookie)
        .send()
        .context("Cursor usage request failed")?;

    check_cursor_status(resp.status(), unauthorized_hint)?;

    let text = resp.text().context("Cursor usage read failed")?;
    if text.trim().is_empty() {
        return Err(anyhow!("Cursor returned an empty response"));
    }
    let body: UsageSummary =
        serde_json::from_str(&text).context("Cursor usage JSON was invalid")?;

    to_payload(body, now)
}

// ---------------------------------------------------------------------------
// Stored credentials (remuda)
// ---------------------------------------------------------------------------

fn clean(token: Option<String>) -> Option<String> {
    token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn agent_tokens(auth: AgentAuth) -> LoginTokens {
    LoginTokens {
        access: clean(auth.access_token),
        refresh: clean(auth.refresh_token),
    }
}

pub(crate) fn live_login() -> LiveLogin {
    let tokens = match TOKEN_ENVS.iter().find_map(|name| env_clean(name)) {
        Some(token) => LoginTokens {
            access: Some(token),
            refresh: None,
        },
        None => std::fs::read_to_string(agent_auth_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<AgentAuth>(&raw).ok())
            .map(agent_tokens)
            .filter(|tokens| tokens.access.is_some())
            .or_else(|| keychain_login().map(agent_tokens))
            .unwrap_or_default(),
    };
    LiveLogin {
        account: tokens.access.as_deref().and_then(|t| user_id(t).ok()),
        tokens,
    }
}

fn stored_token(text: &str) -> Result<String> {
    // Never serde's message: it quotes the value it rejected.
    let auth: AgentAuth =
        serde_json::from_str(text).map_err(|_| anyhow!("the stored auth.json was invalid"))?;
    clean(auth.access_token).ok_or_else(|| anyhow!("the stored credential holds no token"))
}

pub(crate) fn stored_tokens(text: &str) -> LoginTokens {
    serde_json::from_str::<AgentAuth>(text)
        .map(agent_tokens)
        .unwrap_or_default()
}

/// Expiry is the token's own `exp`. The stored copy is never refreshed here:
/// it is remuda's.
fn stored_state(token: &str, now: DateTime<Utc>) -> Option<CredentialState> {
    jwt_claims(token)
        .and_then(|c| c.get("exp")?.as_i64())
        .and_then(|exp| DateTime::from_timestamp(exp, 0))
        .is_some_and(|exp| exp <= now)
        .then_some(CredentialState::Expired)
}

pub(crate) fn check_stored(text: &str, now: DateTime<Utc>) -> Result<Option<CredentialState>> {
    Ok(stored_state(&stored_token(text)?, now))
}

/// Ask about a stored credential, or say why it was not asked.
pub(crate) fn fetch_stored(
    text: &str,
    timeout: Duration,
    now: DateTime<Utc>,
) -> Result<ProviderPayload> {
    let token = stored_token(text)?;
    match stored_state(&token, now) {
        None => usage_for(
            &token,
            timeout,
            now,
            "remuda refreshes stored credentials (`remuda refresh`)",
        ),
        Some(state) => {
            let mut payload = ProviderPayload::live("cursor", "store", UsageSnapshot::at(now));
            payload.credential.state = Some(state);
            Ok(payload)
        }
    }
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
        assert_eq!(usage.login_method.as_deref(), Some("Cursor Pro"));
    }

    /// cursor-agent's own spelling, which the reader once missed.
    #[test]
    fn the_agent_file_is_read_as_cursor_agent_writes_it() {
        let auth: AgentAuth =
            serde_json::from_str(r#"{"accessToken": "t", "refreshToken": "r", "apiKey": null}"#)
                .unwrap();
        assert_eq!(auth.access_token.as_deref(), Some("t"));
        let legacy: AgentAuth = serde_json::from_str(r#"{"access_token": "t"}"#).unwrap();
        assert_eq!(legacy.access_token.as_deref(), Some("t"));
    }

    /// remuda files a Cursor login under the user id in its token, so a stored
    /// one is matched by that and by its tokens, and an expired one is shown
    /// as expired rather than asked about.
    #[test]
    fn a_stored_login_is_its_tokens_and_its_user() {
        use crate::provider::fake_jwt;
        let now = Utc::now();
        let alive = fake_jwt(r#"{"sub":"auth0|user_1","exp":4102444800}"#);
        let text = format!(r#"{{"accessToken": "{alive}", "refreshToken": "r1", "apiKey": null}}"#);
        assert_eq!(
            stored_tokens(&text),
            LoginTokens {
                access: Some(alive.clone()),
                refresh: Some("r1".into())
            }
        );
        assert_eq!(user_id(&alive).unwrap(), "user_1");
        assert_eq!(check_stored(&text, now).unwrap(), None);
        let dead = fake_jwt(r#"{"sub":"auth0|user_1","exp":1}"#);
        let text = format!(r#"{{"accessToken": "{dead}"}}"#);
        assert_eq!(
            check_stored(&text, now).unwrap(),
            Some(CredentialState::Expired)
        );
        let payload = fetch_stored(&text, Duration::from_secs(1), now).unwrap();
        assert_eq!(payload.credential.state, Some(CredentialState::Expired));
        assert!(check_stored(r#"{"accessToken": null, "apiKey": "k"}"#, now).is_err());
    }

    /// The combined header weighs an Ultra and a Pro as 20 to 1.
    #[test]
    fn a_plan_weighs_its_nominal_multiplier() {
        let plan = |membership: &str| {
            let raw = FULL.replace(
                r#""membershipType": "pro""#,
                &format!(r#""membershipType": "{membership}""#),
            );
            let payload = parse(&raw).expect("payload");
            (
                payload.usage.unwrap().login_method,
                payload.credential.plan_weight,
            )
        };
        let sold = |label: &str, weight| (Some(label.to_string()), weight);
        assert_eq!(plan("pro"), sold("Cursor Pro", Some(1.0)));
        assert_eq!(plan("pro_student"), sold("Cursor Pro Student", Some(1.0)));
        assert_eq!(plan("pro_plus"), sold("Cursor Pro+", Some(3.0)));
        assert_eq!(plan("Ultra"), sold("Cursor Ultra", Some(20.0)));
        assert_eq!(plan("team"), sold("Cursor Teams", None));
        assert_eq!(plan("free_trial"), sold("Cursor Pro Trial", None));
        assert_eq!(plan("hobby"), sold("Cursor Hobby", None));
        assert_eq!(plan("express"), sold("express", None));
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
            Some("Cursor Enterprise · unlimited")
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

    /// The CLI writes `~/.cursor/auth.json` on macOS; the config directory
    /// there is `~/Library/Application Support`, where nothing is.
    #[test]
    fn the_cli_login_is_where_each_os_puts_it() {
        let home = Path::new("/home/u");
        let config = Path::new("/cfg");
        assert_eq!(
            agent_auth_path_in(home, config, true),
            Path::new("/home/u/.cursor/auth.json")
        );
        assert_eq!(
            agent_auth_path_in(home, config, false),
            Path::new("/cfg/cursor/auth.json")
        );
    }

    /// A 403 from the edge is not a plan that lacks access, and the hint has
    /// to say so.
    #[test]
    fn a_refusal_is_not_read_as_a_plan_problem() {
        let err = check_cursor_status(reqwest::StatusCode::FORBIDDEN, "sign in again")
            .unwrap_err()
            .to_string();
        assert!(err.contains("edge"), "{err}");
        assert!(!err.contains("plan"), "{err}");
        assert!(check_cursor_status(reqwest::StatusCode::OK, "").is_ok());
    }

    /// Vercel's firewall reads the client hints as one set: a user agent and
    /// a platform hint that disagree is the fingerprint it refuses.
    #[test]
    fn the_client_hints_describe_the_user_agent_they_ride_with() {
        let header = |name: &str| {
            BROWSER_HEADERS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
                .unwrap()
        };
        assert!(header("user-agent").contains("Linux"));
        assert_eq!(header("sec-ch-ua-platform"), "\"Linux\"");
        assert!(header("user-agent").contains("Chrome/140"));
        assert!(header("sec-ch-ua").contains("v=\"140\""));
    }
}
