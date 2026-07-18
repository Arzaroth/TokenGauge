//! Native Kimi Code usage fetcher (kimi.com/code).
//!
//! Read-only: TokenGauge reads the Kimi Code CLI's own credential file
//! (`~/.kimi-code/credentials/kimi-code.json`, honoring `KIMI_CODE_HOME`) or a
//! `KIMI_CODE_API_KEY` env override, then calls the Kimi Code usage endpoint. It
//! never refreshes or rewrites that file - the `kimi-code` CLI owns refresh. On
//! an expired token we surface an error and let the stale-cache fallback keep
//! the last-good number visible until the CLI is next run.
//!
//! The macOS-only browser-cookie (`kimi-auth`) path CodexBar offers is skipped:
//! TokenGauge is Linux-first and the CLI token / API key cover kimi.com/code.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const DEFAULT_BASE_URL: &str = "https://api.kimi.com";
const API_KEY_ENV: &str = "KIMI_CODE_API_KEY";
const BASE_URL_ENV: &str = "KIMI_CODE_BASE_URL";
const HOME_ENV: &str = "KIMI_CODE_HOME";
const CLI_PLATFORM: &str = "kimi_code_cli";
/// A CLI access token must stay valid at least this long to be reused.
const CREDENTIAL_MIN_TTL_SECS: f64 = 60.0;

// ---------------------------------------------------------------------------
// Credentials (read-only)
// ---------------------------------------------------------------------------

struct Auth {
    token: String,
    /// `ProviderPayload.source` tag.
    source: &'static str,
    login_method: &'static str,
    identity_headers: Vec<(&'static str, String)>,
}

#[derive(Deserialize)]
struct CredentialFile {
    #[serde(default, alias = "accessToken")]
    access_token: String,
    #[serde(default, alias = "expiresAt", alias = "expires_at")]
    expires_at: Option<Value>,
}

fn cleaned(raw: Option<String>) -> Option<String> {
    let trimmed = raw?.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// Home of the Kimi Code CLI state (`~/.kimi-code`, or `KIMI_CODE_HOME`).
pub(crate) fn code_home() -> PathBuf {
    if let Some(dir) = cleaned(std::env::var(HOME_ENV).ok()) {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_default().join(".kimi-code")
}

/// Path to the credential file `--doctor` reports.
pub(crate) fn credentials_path() -> PathBuf {
    code_home().join("credentials").join("kimi-code.json")
}

fn read_credential(path: &Path) -> Option<CredentialFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn read_device_id(home: &Path) -> Option<String> {
    cleaned(std::fs::read_to_string(home.join("device_id")).ok())
}

/// Same device identity headers the official CLI sends. `device_id` is only
/// attached when the CLI file exists - never minted fresh per fetch.
fn identity_headers(home: &Path) -> Vec<(&'static str, String)> {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let os_name = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let mut headers = vec![
        ("User-Agent", format!("TokenGauge/{version}")),
        ("X-Msh-Platform", CLI_PLATFORM.to_string()),
        ("X-Msh-Version", version),
        ("X-Msh-Device-Name", "tokengauge".to_string()),
        ("X-Msh-Device-Model", format!("{os_name} {arch}")),
        ("X-Msh-Os-Version", os_name.to_string()),
    ];
    if let Some(device_id) = read_device_id(home) {
        headers.push(("X-Msh-Device-Id", device_id));
    }
    headers
}

fn is_fresh(expires_at: Option<&Value>, now_unix: f64) -> bool {
    let Some(expires) = expires_at.and_then(value_as_f64).filter(|e| e.is_finite()) else {
        return false;
    };
    // Accept both second and millisecond epochs.
    let expires_secs = if expires > 10_000_000_000.0 {
        expires / 1000.0
    } else {
        expires
    };
    expires_secs > now_unix + CREDENTIAL_MIN_TTL_SECS
}

/// Prefer an explicit API key, else fall back to the CLI's own fresh token.
fn resolve_auth(now_unix: f64) -> Result<Auth> {
    if let Some(key) = cleaned(std::env::var(API_KEY_ENV).ok()) {
        return Ok(Auth {
            token: key,
            source: "code-api",
            login_method: "API Key",
            identity_headers: Vec::new(),
        });
    }

    let home = code_home();
    let cred = read_credential(&credentials_path())
        .ok_or_else(|| anyhow!("Kimi not logged in - run `kimi-code`"))?;
    let token = cleaned(Some(cred.access_token))
        .ok_or_else(|| anyhow!("Kimi not logged in - run `kimi-code`"))?;
    if !is_fresh(cred.expires_at.as_ref(), now_unix) {
        return Err(anyhow!("Kimi token expired - run `kimi-code` to log in"));
    }
    Ok(Auth {
        token,
        source: "code-cli",
        login_method: "Kimi Code",
        identity_headers: identity_headers(&home),
    })
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UsageResponse {
    usage: Detail,
    #[serde(default)]
    limits: Option<Vec<RateLimit>>,
}

#[derive(Deserialize)]
struct RateLimit {
    #[serde(default)]
    window: Option<Window>,
    detail: Detail,
}

#[derive(Deserialize)]
struct Window {
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default, rename = "timeUnit", alias = "time_unit")]
    time_unit: Option<String>,
}

/// Kimi returns limit/used/remaining as either JSON numbers or strings.
#[derive(Deserialize)]
struct Detail {
    #[serde(default)]
    limit: Option<Value>,
    #[serde(default)]
    used: Option<Value>,
    #[serde(default)]
    remaining: Option<Value>,
    #[serde(
        default,
        rename = "resetTime",
        alias = "resetAt",
        alias = "reset_time",
        alias = "reset_at"
    )]
    reset_time: Option<String>,
}

fn value_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Convert a `{duration, timeUnit}` window to whole minutes.
fn window_minutes(w: &Window) -> Option<u32> {
    let duration = w.duration.filter(|d| d.is_finite() && *d > 0.0)?;
    let unit = w.time_unit.as_deref().unwrap_or("");
    let minutes = if unit.contains("SECOND") {
        duration / 60.0
    } else if unit.contains("MINUTE") {
        duration
    } else if unit.contains("HOUR") {
        duration * 60.0
    } else if unit.contains("DAY") {
        duration * 1440.0
    } else {
        return None;
    };
    (minutes.is_finite() && minutes > 0.0).then_some(minutes.round() as u32)
}

// ---------------------------------------------------------------------------
// Pure mapping
// ---------------------------------------------------------------------------

/// A window counts only when it carries a positive `limit` and a used value we
/// can derive (directly, or from `limit - remaining`).
fn to_window(detail: &Detail, window_minutes: Option<u32>) -> Option<UsageWindow> {
    let limit = detail.limit.as_ref().and_then(value_as_f64).filter(|l| *l > 0.0)?;
    let used = match (
        detail.used.as_ref().and_then(value_as_f64),
        detail.remaining.as_ref().and_then(value_as_f64),
    ) {
        (Some(used), _) => used,
        (None, Some(remaining)) => (limit - remaining).max(0.0),
        (None, None) => return None,
    };
    Some(UsageWindow {
        used_percent: Some(pct_u8(used / limit * 100.0)),
        reset_description: None,
        resets_at: detail.reset_time.clone(),
        window_minutes,
    })
}

fn to_payload(
    resp: UsageResponse,
    source: &str,
    login_method: &str,
    now: DateTime<Utc>,
) -> Result<ProviderPayload> {
    // Primary = the weekly coding quota; secondary = the first rolling rate limit.
    let primary = to_window(&resp.usage, Some(10080));
    let secondary = resp
        .limits
        .as_ref()
        .and_then(|limits| limits.first())
        .and_then(|limit| to_window(&limit.detail, limit.window.as_ref().and_then(window_minutes)));

    // An all-empty snapshot must be an error so the stale-cache fallback keeps
    // the last-good number instead of rendering a blank row.
    if primary.is_none() && secondary.is_none() {
        return Err(anyhow!("Kimi returned no usage windows"));
    }

    Ok(ProviderPayload {
        provider: "kimi".to_string(),
        version: None,
        source: Some(source.to_string()),
        usage: Some(UsageSnapshot {
            primary,
            secondary,
            tertiary: None,
            updated_at: Some(now.to_rfc3339()),
            login_method: Some(login_method.to_string()),
            extra_rate_windows: Vec::new(),
        }),
        credits: None,
        error: None,
        stale: false,
    })
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Build the usage endpoint, tolerating a `KIMI_CODE_BASE_URL` override that
/// already carries part of the `coding/v1` path.
fn usage_endpoint() -> String {
    let base = cleaned(std::env::var(BASE_URL_ENV).ok())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let base = base.trim_end_matches('/');
    if base.ends_with("/coding/v1") {
        format!("{base}/usages")
    } else if base.ends_with("/coding") {
        format!("{base}/v1/usages")
    } else {
        format!("{base}/coding/v1/usages")
    }
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let auth = resolve_auth(unix_now_secs())?;

    let client = http_client(timeout)?;
    let mut request = client
        .get(usage_endpoint())
        .header("authorization", format!("Bearer {}", auth.token))
        .header("accept", "application/json");
    for (name, value) in &auth.identity_headers {
        request = request.header(*name, value);
    }
    let resp = request.send().context("Kimi usage request failed")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!("Kimi unauthorized - run `kimi-code` to log in"));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(anyhow!("Kimi rate-limited - try again shortly"));
    }
    if !status.is_success() {
        return Err(anyhow!("Kimi usage HTTP {}", status.as_u16()));
    }

    let body: UsageResponse = resp.json().context("Kimi usage JSON was invalid")?;
    Ok(vec![to_payload(body, auth.source, auth.login_method, now)?])
}

fn unix_now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resp(json: &str) -> UsageResponse {
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn maps_string_and_number_fixture() {
        // Real wire shape: string quota + a 300-minute rate window with numbers.
        let body = resp(
            r#"{
                "usage": {"limit": "2048", "used": "214", "remaining": "1834",
                          "resetTime": "2026-07-16T09:59:59Z"},
                "limits": [{
                    "window": {"duration": 300, "timeUnit": "TIME_UNIT_MINUTE"},
                    "detail": {"limit": 200, "used": 139, "remaining": 61}
                }]
            }"#,
        );
        let payload = to_payload(body, "code-api", "API Key", Utc::now()).unwrap();
        let usage = payload.usage.unwrap();

        let primary = usage.primary.unwrap();
        assert_eq!(primary.used_percent, Some(10)); // 214 / 2048 ~ 10.4
        assert_eq!(primary.window_minutes, Some(10080));
        assert_eq!(primary.resets_at.as_deref(), Some("2026-07-16T09:59:59Z"));

        let secondary = usage.secondary.unwrap();
        assert_eq!(secondary.used_percent, Some(70)); // 139 / 200
        assert_eq!(secondary.window_minutes, Some(300));
        assert_eq!(usage.login_method.as_deref(), Some("API Key"));
    }

    #[test]
    fn derives_used_from_remaining() {
        let body = resp(r#"{"usage": {"limit": "1000", "remaining": "750"}}"#);
        let payload = to_payload(body, "code-cli", "Kimi Code", Utc::now()).unwrap();
        assert_eq!(payload.usage.unwrap().primary.unwrap().used_percent, Some(25));
    }

    #[test]
    fn empty_snapshot_is_error() {
        // Zero limit yields no primary and no secondary -> error so the stale
        // fallback keeps last-good rather than rendering a blank row.
        let body = resp(r#"{"usage": {"limit": "0", "used": "0"}}"#);
        assert!(to_payload(body, "code-api", "API Key", Utc::now()).is_err());
    }

    #[test]
    fn window_minutes_units() {
        let w = |json: &str| serde_json::from_str::<Window>(json).unwrap();
        assert_eq!(
            window_minutes(&w(r#"{"duration": 5, "timeUnit": "TIME_UNIT_HOUR"}"#)),
            Some(300)
        );
        assert_eq!(
            window_minutes(&w(r#"{"duration": 7, "timeUnit": "TIME_UNIT_DAY"}"#)),
            Some(10080)
        );
        assert_eq!(window_minutes(&w(r#"{"duration": 300}"#)), None);
    }

    #[test]
    fn freshness_epoch_forms() {
        let now = 1_800_000_000.0_f64;
        assert!(is_fresh(Some(&serde_json::json!(now + 3600.0)), now)); // seconds
        assert!(is_fresh(
            Some(&serde_json::json!((now + 3600.0) * 1000.0)),
            now
        )); // millis
        assert!(!is_fresh(Some(&serde_json::json!(now + 30.0)), now)); // inside TTL grace
        assert!(!is_fresh(Some(&serde_json::json!("not-a-time")), now));
        assert!(!is_fresh(None, now));
    }
}
