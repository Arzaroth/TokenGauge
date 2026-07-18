//! Native z.ai / GLM Coding Plan usage fetcher (zcode.z.ai).
//!
//! Unlike Claude/Codex, z.ai has no local CLI credential file: the GLM Coding
//! Plan is consumed through an Anthropic-compatible base URL, and usage lives at
//! a separate monitor endpoint. So TokenGauge reads the API key from the
//! `Z_AI_API_KEY` env var (legacy `ZAI_API_TOKEN`) and queries the quota
//! endpoint directly. Set `Z_AI_API_HOST` (or the full `Z_AI_QUOTA_URL`) to
//! target the China BigModel region (`open.bigmodel.cn`).

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const DEFAULT_QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const API_KEY_ENVS: &[&str] = &["Z_AI_API_KEY", "ZAI_API_TOKEN"];

// ---------------------------------------------------------------------------
// Auth + endpoint (env only - no CLI file to read)
// ---------------------------------------------------------------------------

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn api_key() -> Result<String> {
    API_KEY_ENVS
        .iter()
        .find_map(|name| env_clean(name))
        .ok_or_else(|| anyhow!("z.ai key missing - set Z_AI_API_KEY"))
}

fn quota_url() -> String {
    if let Some(url) = env_clean("Z_AI_QUOTA_URL") {
        return url;
    }
    if let Some(host) = env_clean("Z_AI_API_HOST") {
        let host = host.trim_end_matches('/');
        let host = host.strip_prefix("https://").unwrap_or(host);
        return format!("https://{host}/api/monitor/usage/quota/limit");
    }
    DEFAULT_QUOTA_URL.to_string()
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct QuotaResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default, alias = "msg")]
    message: Option<String>,
    #[serde(default)]
    data: Option<QuotaData>,
    /// Legacy shape put the limits at the top level.
    #[serde(default)]
    limits: Option<Vec<Limit>>,
}

#[derive(Deserialize)]
struct QuotaData {
    #[serde(
        default,
        alias = "planName",
        alias = "plan",
        alias = "plan_type",
        alias = "packageName"
    )]
    plan_name: Option<String>,
    #[serde(default)]
    limits: Option<Vec<Limit>>,
}

/// Numeric fields arrive as JSON numbers or strings depending on API version.
#[derive(Deserialize)]
struct Limit {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    usage: Option<Value>,
    #[serde(default)]
    used: Option<Value>,
    #[serde(default, alias = "currentValue")]
    current_value: Option<Value>,
    #[serde(default)]
    limit: Option<Value>,
    #[serde(default)]
    remaining: Option<Value>,
    #[serde(default)]
    percentage: Option<Value>,
    #[serde(default)]
    unit: Option<Value>,
    #[serde(default)]
    number: Option<Value>,
    #[serde(default, alias = "resetAt")]
    reset_at: Option<String>,
    #[serde(default, rename = "nextResetTime", alias = "next_reset_time")]
    next_reset_time: Option<Value>,
}

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn is_token_limit(l: &Limit) -> bool {
    l.kind
        .as_deref()
        .map(|k| k.to_uppercase().contains("TOKEN"))
        .unwrap_or(false)
}

/// Window length in minutes from the `unit`/`number` pair (1=days, 3=hours,
/// 5=minutes, 6=weeks).
fn window_minutes(l: &Limit) -> Option<u32> {
    let unit = l.unit.as_ref().and_then(value_as_f64)? as i64;
    let per = match unit {
        1 => 1440.0,
        3 => 60.0,
        5 => 1.0,
        6 => 10080.0,
        _ => return None,
    };
    let number = l
        .number
        .as_ref()
        .and_then(value_as_f64)
        .filter(|n| *n > 0.0)
        .unwrap_or(1.0);
    let minutes = per * number;
    (minutes > 0.0).then_some(minutes.round() as u32)
}

/// Percent used: prefer an explicit `percentage`; else derive from limit and
/// used/remaining/currentValue. z.ai omits fields rather than sending zeros, so
/// a missing basis yields None (never a false 100%).
fn used_percent(l: &Limit) -> Option<u8> {
    if let Some(percent) = l.percentage.as_ref().and_then(value_as_f64) {
        return Some(pct_u8(percent));
    }
    let total = l
        .limit
        .as_ref()
        .and_then(value_as_f64)
        .or_else(|| l.usage.as_ref().and_then(value_as_f64))
        .filter(|t| *t > 0.0)?;
    let used = match (
        l.used.as_ref().and_then(value_as_f64),
        l.remaining.as_ref().and_then(value_as_f64),
        l.current_value.as_ref().and_then(value_as_f64),
    ) {
        (Some(used), _, _) => used,
        (None, Some(remaining), current) => (total - remaining).max(current.unwrap_or(0.0)),
        (None, None, Some(current)) => current,
        (None, None, None) => return None,
    };
    Some(pct_u8(used / total * 100.0))
}

fn epoch_to_rfc3339(ms: f64) -> Option<String> {
    // Accept both millisecond and second epochs.
    let secs = if ms > 10_000_000_000.0 {
        ms / 1000.0
    } else {
        ms
    };
    Utc.timestamp_opt(secs as i64, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
}

fn to_window(l: &Limit) -> Option<UsageWindow> {
    let used_percent = used_percent(l)?;
    let resets_at = l.reset_at.clone().or_else(|| {
        l.next_reset_time
            .as_ref()
            .and_then(value_as_f64)
            .and_then(epoch_to_rfc3339)
    });
    Some(UsageWindow {
        used_percent: Some(used_percent),
        reset_description: None,
        resets_at,
        window_minutes: window_minutes(l),
    })
}

fn to_payload(resp: QuotaResponse, now: DateTime<Utc>) -> Result<ProviderPayload> {
    if let Some(code) = resp.code
        && code != 0
        && code != 200
    {
        let message = resp
            .message
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| format!("code {code}"));
        return Err(anyhow!("z.ai error: {message}"));
    }

    let plan = resp.data.as_ref().and_then(|d| d.plan_name.clone());
    let limits = resp
        .data
        .and_then(|d| d.limits)
        .or(resp.limits)
        .unwrap_or_default();

    // Priority order: token quotas longest-window first (weekly), shortest last
    // (5-hour), then any remaining limit (time-based fallback) in wire order.
    // Assign slots from limits that actually map to a usable window, so an
    // unusable first limit can't occupy a slot and hide later valid windows.
    let mut ordered: Vec<&Limit> = limits.iter().filter(|l| is_token_limit(l)).collect();
    ordered.sort_by_key(|l| std::cmp::Reverse(window_minutes(l).unwrap_or(0)));
    for l in &limits {
        if !ordered.iter().any(|o| std::ptr::eq(*o, l)) {
            ordered.push(l);
        }
    }

    let mut windows = ordered.into_iter().filter_map(to_window);
    let primary = windows.next();
    let secondary = windows.next();

    if primary.is_none() && secondary.is_none() {
        return Err(anyhow!("z.ai returned no usage - check region/token"));
    }

    Ok(ProviderPayload {
        provider: "glm".to_string(),
        version: None,
        source: Some("z.ai".to_string()),
        usage: Some(UsageSnapshot {
            primary,
            secondary,
            tertiary: None,
            updated_at: Some(now.to_rfc3339()),
            login_method: plan,
            extra_rate_windows: Vec::new(),
        }),
        credits: None,
        error: None,
        stale: false,
    })
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let key = api_key()?;
    let url = quota_url();
    if !url.starts_with("https://") {
        return Err(anyhow!("z.ai quota URL must use HTTPS"));
    }

    let client = http_client(timeout)?;
    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .send()
        .context("z.ai usage request failed")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!(
            "z.ai unauthorized - check Z_AI_API_KEY (legacy ZAI_API_TOKEN)"
        ));
    }
    if !status.is_success() {
        return Err(anyhow!("z.ai usage HTTP {}", status.as_u16()));
    }

    // A wrong region often answers 200 with an empty body.
    let text = resp.text().context("z.ai usage read failed")?;
    if text.trim().is_empty() {
        return Err(anyhow!("z.ai empty response - check region/token"));
    }
    let body: QuotaResponse = serde_json::from_str(&text).context("z.ai usage JSON was invalid")?;
    Ok(vec![to_payload(body, now)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(json: &str) -> QuotaResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn maps_two_token_windows() {
        let body = resp(
            r#"{
                "code": 0,
                "data": {
                    "planName": "GLM Coding Plan",
                    "limits": [
                        {"type": "TOKENS_LIMIT", "percentage": 30,
                         "unit": 3, "number": 5, "nextResetTime": 1893456000000},
                        {"type": "TOKENS_LIMIT", "percentage": 80, "unit": 6, "number": 1},
                        {"type": "TIME_LIMIT", "limit": 100, "used": 10, "unit": 1, "number": 30}
                    ]
                }
            }"#,
        );
        let payload = to_payload(body, Utc::now()).unwrap();
        let usage = payload.usage.unwrap();
        // Longest token window (weekly, 10080m) is primary; shorter (5h) secondary.
        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, Some(80));
        assert_eq!(primary.window_minutes, Some(10080));
        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, Some(30));
        assert_eq!(secondary.window_minutes, Some(300));
        assert_eq!(usage.login_method.as_deref(), Some("GLM Coding Plan"));
    }

    #[test]
    fn derives_percent_from_limit_and_remaining() {
        let body = resp(
            r#"{"data": {"limits": [{"type": "TOKENS_LIMIT", "limit": 1000, "remaining": 250}]}}"#,
        );
        let usage = to_payload(body, Utc::now()).unwrap().usage.unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(75));
    }

    #[test]
    fn error_code_surfaces_message() {
        let body = resp(r#"{"code": 401, "message": "invalid token"}"#);
        let err = to_payload(body, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("invalid token"), "{err}");
    }

    #[test]
    fn error_code_surfaces_msg_alias() {
        let body = resp(r#"{"code": 401, "msg": "bad key"}"#);
        let err = to_payload(body, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("bad key"), "{err}");
    }

    #[test]
    fn skips_unusable_first_limit() {
        // Longest limit has no derivable percent; the valid shorter one takes primary.
        let body = resp(
            r#"{"code": 0, "data": {"limits": [
                {"type": "TOKENS_LIMIT", "unit": 6, "number": 1},
                {"type": "TOKENS_LIMIT", "percentage": 55, "unit": 3, "number": 5}
            ]}}"#,
        );
        let usage = to_payload(body, Utc::now()).unwrap().usage.unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(55));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn time_only_does_not_duplicate_into_both_windows() {
        let body = resp(
            r#"{"code": 0, "data": {"limits": [
                {"type": "TIME_LIMIT", "percentage": 42, "unit": 1, "number": 30}
            ]}}"#,
        );
        let usage = to_payload(body, Utc::now()).unwrap().usage.unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(42));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn empty_limits_is_error() {
        let body = resp(r#"{"code": 0, "data": {"limits": []}}"#);
        assert!(to_payload(body, Utc::now()).is_err());
    }
}
