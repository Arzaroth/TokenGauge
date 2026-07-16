//! Native Claude OAuth usage fetcher.
//!
//! Read-only: TokenGauge reads `~/.claude/.credentials.json` and calls the
//! Anthropic OAuth usage endpoint. It never refreshes or writes that file -
//! the `claude` CLI owns refresh (it rotates the token on its own schedule and
//! would race any lock we invent), and the file also holds unrelated `mcpOAuth`
//! secrets. On an expired token we surface an error and let the stale-cache
//! fallback keep the last-good number visible until `claude` is next run.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::{
    ExtraRateWindow, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8, slug,
};

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const USER_AGENT: &str = "claude-code/2.1.0";

/// Routine alias keys, highest priority first. A populated alias beats a null
/// one; a null (present) alias still emits the window at 0% so the bar stays
/// visible; no alias at all omits it.
const ROUTINE_ALIASES: &[&str] = &[
    "seven_day_routines",
    "seven_day_claude_routines",
    "claude_routines",
    "routines",
    "routine",
    "seven_day_cowork",
    "cowork",
];

// ---------------------------------------------------------------------------
// Credentials (read-only)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<Oauth>,
}

#[derive(Deserialize, Debug)]
struct Oauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// Milliseconds since epoch. Absent => treated as expired.
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(rename = "rateLimitTier")]
    rate_limit_tier: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

fn credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join(".credentials.json")
}

fn read_credentials(path: &Path, now: DateTime<Utc>) -> Result<Oauth> {
    let data = std::fs::read_to_string(path)
        .map_err(|_| anyhow!("Claude not logged in - run `claude`"))?;
    let creds: Credentials = serde_json::from_str(&data).context("credentials JSON was invalid")?;
    let oauth = creds
        .oauth
        .ok_or_else(|| anyhow!("Claude not logged in - run `claude`"))?;

    let expired = match oauth.expires_at {
        Some(ms) => now.timestamp_millis() >= ms,
        None => true,
    };
    if expired {
        return Err(anyhow!("Claude token expired - run `claude` to log in"));
    }
    if !oauth.scopes.iter().any(|s| s == "user:profile") {
        return Err(anyhow!("Claude OAuth missing user:profile scope"));
    }
    Ok(oauth)
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<Win>,
    seven_day: Option<Win>,
    seven_day_oauth_apps: Option<Win>,
    seven_day_opus: Option<Win>,
    seven_day_sonnet: Option<Win>,
    #[serde(default)]
    limits: Vec<Limit>,
    /// Routine aliases + unknown `seven_day_*` keys land here.
    #[serde(flatten)]
    rest: Map<String, Value>,
}

#[derive(Deserialize, Clone)]
struct Win {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Deserialize)]
struct Limit {
    group: Option<String>,
    kind: Option<String>,
    percent: Option<f64>,
    resets_at: Option<String>,
    scope: Option<Scope>,
}

#[derive(Deserialize)]
struct Scope {
    model: Option<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: Option<String>,
    display_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure mapping
// ---------------------------------------------------------------------------

/// A window counts only when it carries a non-nil `utilization`.
fn to_window(win: Option<&Win>, minutes: u32) -> Option<UsageWindow> {
    let w = win?;
    let util = w.utilization?;
    Some(UsageWindow {
        used_percent: Some(pct_u8(util)),
        reset_description: None,
        resets_at: w.resets_at.clone(),
        window_minutes: Some(minutes),
    })
}

/// Brand a plan label from the credential's subscription type and rate-limit
/// tier. The Max multiplier ("5x"/"20x") always comes from the tier.
fn plan_label(subscription_type: Option<&str>, tier: Option<&str>) -> Option<String> {
    #[derive(PartialEq)]
    enum Kind {
        Max,
        Pro,
        Team,
        Enterprise,
    }
    fn classify(s: &str) -> Option<Kind> {
        let s = s.to_lowercase();
        if s.contains("enterprise") {
            Some(Kind::Enterprise)
        } else if s.contains("team") {
            Some(Kind::Team)
        } else if s.contains("max") {
            Some(Kind::Max)
        } else if s.contains("pro") {
            Some(Kind::Pro)
        } else {
            None
        }
    }
    /// Word right after "max" in the tier, when it is a valid "<int>x" multiplier.
    fn max_multiplier(tier: &str) -> Option<String> {
        let words: Vec<&str> = tier
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let idx = words.iter().position(|w| w.eq_ignore_ascii_case("max"))?;
        let word = words.get(idx + 1)?;
        let digits = word.strip_suffix(['x', 'X'])?;
        (!digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())).then(|| word.to_string())
    }

    let kind = subscription_type
        .and_then(classify)
        .or_else(|| tier.and_then(classify))?;
    let label = match kind {
        Kind::Max => {
            let mult = tier.and_then(max_multiplier);
            match mult {
                Some(m) => format!("Claude Max {m}"),
                None => "Claude Max".to_string(),
            }
        }
        Kind::Pro => "Claude Pro".to_string(),
        Kind::Team => "Claude Team".to_string(),
        Kind::Enterprise => "Claude Enterprise".to_string(),
    };
    Some(label)
}

/// The routines extra window, if any alias key is present.
fn routines_window(rest: &Map<String, Value>) -> Option<ExtraRateWindow> {
    let mut placeholder = false;
    let mut populated: Option<Win> = None;
    for key in ROUTINE_ALIASES {
        if let Some(v) = rest.get(*key) {
            placeholder = true;
            if !v.is_null()
                && let Ok(w) = serde_json::from_value::<Win>(v.clone())
            {
                populated = Some(w);
                break;
            }
        }
    }
    let window = match (populated, placeholder) {
        (Some(w), _) => UsageWindow {
            used_percent: Some(pct_u8(w.utilization.unwrap_or(0.0))),
            reset_description: None,
            resets_at: w.resets_at,
            window_minutes: Some(10080),
        },
        (None, true) => UsageWindow {
            used_percent: Some(0),
            reset_description: None,
            resets_at: None,
            window_minutes: Some(10080),
        },
        (None, false) => return None,
    };
    Some(ExtraRateWindow {
        id: Some("claude-routines".to_string()),
        title: Some("Daily Routines".to_string()),
        window: Some(window),
    })
}

/// Scoped-weekly extra windows from `limits[]`, de-duplicated by id.
fn scoped_weekly_windows(limits: &[Limit]) -> Vec<ExtraRateWindow> {
    let mut out: Vec<ExtraRateWindow> = Vec::new();
    for limit in limits {
        if limit.group.as_deref() != Some("weekly") || limit.kind.as_deref() != Some("weekly_scoped")
        {
            continue;
        }
        let Some(percent) = limit.percent.filter(|p| p.is_finite()) else {
            continue;
        };
        let model = limit.scope.as_ref().and_then(|s| s.model.as_ref());
        let display = model
            .and_then(|m| m.display_name.as_deref())
            .filter(|d| !d.is_empty());
        let Some(display) = display else { continue };

        let id_source = model
            .and_then(|m| m.id.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or(display);
        let id_slug = slug(id_source);
        if slug(display) == "all-models"
            || id_slug == "all-models"
            || id_slug.ends_with("-all-models")
        {
            continue;
        }
        let id = format!("claude-weekly-scoped-{id_slug}");
        if out.iter().any(|w| w.id.as_deref() == Some(id.as_str())) {
            continue; // first id wins
        }
        out.push(ExtraRateWindow {
            id: Some(id),
            title: Some(format!("{display} only")),
            window: Some(UsageWindow {
                used_percent: Some(pct_u8(percent)),
                reset_description: None,
                resets_at: limit.resets_at.clone(),
                window_minutes: Some(10080),
            }),
        });
    }
    out
}

fn to_payload(resp: UsageResponse, plan: Option<String>, now: DateTime<Utc>) -> ProviderPayload {
    let primary = to_window(resp.five_hour.as_ref(), 300)
        .or_else(|| to_window(resp.seven_day.as_ref(), 10080))
        .or_else(|| to_window(resp.seven_day_oauth_apps.as_ref(), 10080))
        .or_else(|| to_window(resp.seven_day_sonnet.as_ref(), 10080))
        .or_else(|| to_window(resp.seven_day_opus.as_ref(), 10080));
    let secondary = to_window(resp.seven_day.as_ref(), 10080);
    let tertiary = to_window(resp.seven_day_sonnet.as_ref(), 10080)
        .or_else(|| to_window(resp.seven_day_opus.as_ref(), 10080));

    let mut extra_rate_windows = Vec::new();
    extra_rate_windows.extend(routines_window(&resp.rest));
    extra_rate_windows.extend(scoped_weekly_windows(&resp.limits));

    ProviderPayload {
        provider: "claude".to_string(),
        version: None,
        source: Some("oauth".to_string()),
        usage: Some(UsageSnapshot {
            primary,
            secondary,
            tertiary,
            updated_at: Some(now.to_rfc3339()),
            login_method: plan,
            extra_rate_windows,
        }),
        credits: None,
        error: None,
        stale: false,
    }
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let oauth = read_credentials(&credentials_path(), now)?;

    let client = http_client(timeout)?;
    let resp = client
        .get(USAGE_URL)
        .header("authorization", format!("Bearer {}", oauth.access_token))
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", USER_AGENT)
        .send()
        .context("Claude usage request failed")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(anyhow!("Claude unauthorized - run `claude` to log in"));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(anyhow!("Claude rate-limited - try again shortly"));
    }
    if !status.is_success() {
        return Err(anyhow!("Claude usage HTTP {}", status.as_u16()));
    }

    let body: UsageResponse = resp.json().context("Claude usage JSON was invalid")?;
    let plan = plan_label(
        oauth.subscription_type.as_deref(),
        oauth.rate_limit_tier.as_deref(),
    );
    Ok(vec![to_payload(body, plan, now)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(json: &str) -> UsageResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn maps_live_fixture() {
        // Derived from the real wire shape: five_hour + seven_day + one scoped
        // weekly limit + a routines window.
        let body = resp(
            r#"{
            "five_hour":  {"utilization": 5.0,  "resets_at": "2026-07-15T12:09:59Z"},
            "seven_day":  {"utilization": 89.0, "resets_at": "2026-07-16T09:59:59Z"},
            "seven_day_routines": null,
            "limits": [
              {"group":"weekly","kind":"weekly_scoped","percent":82.0,
               "resets_at":"2026-07-16T09:59:59Z",
               "scope":{"model":{"id":null,"display_name":"Fable"}}}
            ]
        }"#,
        );
        let plan = plan_label(Some("max"), Some("default_claude_max_20x"));
        let payload = to_payload(body, plan, Utc::now());
        let usage = payload.usage.unwrap();

        assert_eq!(usage.primary.as_ref().unwrap().used_percent, Some(5));
        assert_eq!(usage.primary.as_ref().unwrap().window_minutes, Some(300));
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, Some(89));
        assert!(usage.tertiary.is_none());
        assert_eq!(usage.login_method.as_deref(), Some("Claude Max 20x"));

        let titles: Vec<&str> = usage
            .extra_rate_windows
            .iter()
            .map(|w| w.title.as_deref().unwrap())
            .collect();
        assert_eq!(titles, vec!["Daily Routines", "Fable only"]);
        // resets_at flows through as RFC3339; reset_description stays None.
        let fable = &usage.extra_rate_windows[1];
        assert_eq!(fable.window.as_ref().unwrap().used_percent, Some(82));
        assert!(fable.window.as_ref().unwrap().reset_description.is_none());
    }

    #[test]
    fn null_routine_alias_emits_placeholder() {
        let body = resp(r#"{"five_hour":{"utilization":1.0},"seven_day_routines":null}"#);
        let payload = to_payload(body, None, Utc::now());
        let w = &payload.usage.unwrap().extra_rate_windows[0];
        assert_eq!(w.id.as_deref(), Some("claude-routines"));
        assert_eq!(w.window.as_ref().unwrap().used_percent, Some(0));
        assert!(w.window.as_ref().unwrap().resets_at.is_none());
    }

    #[test]
    fn absent_routine_alias_omits_window() {
        let body = resp(r#"{"five_hour":{"utilization":1.0}}"#);
        let payload = to_payload(body, None, Utc::now());
        assert!(payload.usage.unwrap().extra_rate_windows.is_empty());
    }

    #[test]
    fn populated_alias_beats_null_alias() {
        // routines (4th) null, cowork (7th) populated -> populated wins.
        let body = resp(
            r#"{"five_hour":{"utilization":1.0},"routines":null,
                "cowork":{"utilization":30.0}}"#,
        );
        let payload = to_payload(body, None, Utc::now());
        let w = &payload.usage.unwrap().extra_rate_windows[0];
        assert_eq!(w.window.as_ref().unwrap().used_percent, Some(30));
    }

    #[test]
    fn primary_falls_back_when_five_hour_absent() {
        let body = resp(r#"{"seven_day":{"utilization":42.0}}"#);
        let payload = to_payload(body, None, Utc::now());
        let usage = payload.usage.unwrap();
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, Some(42));
        assert_eq!(usage.primary.as_ref().unwrap().window_minutes, Some(10080));
    }

    #[test]
    fn scoped_weekly_filters_and_dedupes() {
        let body = resp(
            r#"{"five_hour":{"utilization":1.0},"limits":[
              {"group":"weekly","kind":"weekly_scoped","percent":10.0,
               "scope":{"model":{"display_name":"All models"}}},
              {"group":"weekly","kind":"weekly_scoped","percent":20.0,
               "scope":{"model":{"id":"claude/all_models","display_name":"X"}}},
              {"group":"weekly","kind":"weekly_scoped","percent":30.0,
               "scope":{"model":{"display_name":"Fable"}}},
              {"group":"weekly","kind":"weekly_scoped","percent":40.0,
               "scope":{"model":{"display_name":"Fable"}}},
              {"group":"weekly","kind":"weekly_scoped","percent":50.0,
               "scope":{"model":{"display_name":""}}}
            ]}"#,
        );
        let payload = to_payload(body, None, Utc::now());
        let windows = payload.usage.unwrap().extra_rate_windows;
        // "All models" and "claude/all_models" skipped; empty display skipped;
        // duplicate "Fable" collapses to the first (30%).
        let fable: Vec<_> = windows
            .iter()
            .filter(|w| w.title.as_deref() == Some("Fable only"))
            .collect();
        assert_eq!(fable.len(), 1);
        assert_eq!(fable[0].window.as_ref().unwrap().used_percent, Some(30));
        assert_eq!(windows.len(), 1);
    }

    #[test]
    fn plan_label_table() {
        assert_eq!(
            plan_label(None, Some("default_claude_max_5x")).as_deref(),
            Some("Claude Max 5x")
        );
        assert_eq!(
            plan_label(Some("max"), Some("v2_default_claude_max_20x")).as_deref(),
            Some("Claude Max 20x")
        );
        assert_eq!(
            plan_label(None, Some("claude_max")).as_deref(),
            Some("Claude Max")
        );
        assert_eq!(
            plan_label(None, Some("default_claude_team_5x")).as_deref(),
            Some("Claude Team")
        );
        assert_eq!(
            plan_label(Some("team"), Some("default_claude_max_5x")).as_deref(),
            Some("Claude Team")
        );
        assert_eq!(plan_label(None, None), None);
    }

    #[test]
    fn expired_or_missing_credentials_error() {
        let dir = std::env::temp_dir().join(format!("tg-claude-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.json");

        // Expired (expiresAt in the past).
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"x","expiresAt":1000,"scopes":["user:profile"]}}"#,
        )
        .unwrap();
        let err = read_credentials(&path, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");
        assert!(err.len() <= 60, "message too long for clean_error_message: {err}");

        // Missing scope.
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"x","expiresAt":32503680000000,"scopes":[]}}"#,
        )
        .unwrap();
        let err = read_credentials(&path, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("user:profile"), "{err}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
