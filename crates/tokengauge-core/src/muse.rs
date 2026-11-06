//! Meta Muse's subscription windows, from the key endpoint the `muse` CLI's own
//! `/usage` overlay reads (`POST https://api.meta.ai/muse-code/key`).
//!
//! The sign-in is `muse login`'s, read from `MUSE_AUTH_PATH` or
//! `~/.config/muse/auth.json` and never refreshed. The answer also carries the
//! account's Model API key, which is why the wire struct names only what is drawn:
//! the key is dropped at parsing and never reaches a payload or the snapshot.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::provider::{check_status, json_num};
use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const KEY_URL: &str = "https://api.meta.ai/muse-code/key";
const SESSION_MINUTES: u32 = 300;
const WEEK_MINUTES: u32 = 10_080;

fn auth_path() -> Option<PathBuf> {
    std::env::var_os("MUSE_AUTH_PATH")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .or_else(|| Some(dirs::home_dir()?.join(".config")))?;
            Some(config.join("muse").join("auth.json"))
        })
}

fn token_in(raw: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    parsed
        .get("providers")?
        .get("meta")?
        .get("access_token")?
        .as_str()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

pub(crate) fn access_token() -> Option<String> {
    token_in(&std::fs::read_to_string(auth_path()?).ok()?)
}

#[derive(Deserialize)]
struct KeyResponse {
    is_subs_active: Option<bool>,
    subs_tier_name: Option<String>,
    subs_usage: Option<SubsUsage>,
}

#[derive(Deserialize)]
struct SubsUsage {
    window: Option<WireWindow>,
    weekly: Option<WireWindow>,
}

#[derive(Deserialize)]
struct WireWindow {
    #[serde(default)]
    used_percent: Value,
    #[serde(default)]
    resets_at: Value,
    #[serde(default)]
    window_duration_mins: Value,
}

/// Epoch seconds or milliseconds, or an ISO time.
fn reset_iso(value: &Value) -> Option<String> {
    if let Some(iso) = value.as_str().filter(|s| s.parse::<f64>().is_err()) {
        return DateTime::parse_from_rfc3339(iso.trim())
            .ok()
            .map(|at| at.to_rfc3339());
    }
    let at = json_num(value).filter(|at| *at > 0.0)?;
    let seconds = if at >= 1e12 { at / 1000.0 } else { at };
    DateTime::from_timestamp(seconds as i64, 0).map(|at: DateTime<Utc>| at.to_rfc3339())
}

impl WireWindow {
    fn to_window(&self, fallback_minutes: u32) -> Option<UsageWindow> {
        let used = json_num(&self.used_percent).filter(|used| used.is_finite())?;
        Some(UsageWindow {
            used_percent: Some(pct_u8(used.clamp(0.0, 100.0))),
            reset_description: None,
            resets_at: reset_iso(&self.resets_at),
            window_minutes: Some(
                json_num(&self.window_duration_mins)
                    .filter(|minutes| *minutes > 0.0)
                    .map_or(fallback_minutes, |minutes| minutes as u32),
            ),
            burn: None,
            opened_at: None,
        })
    }
}

fn to_payload(resp: KeyResponse) -> Result<ProviderPayload> {
    let (primary, secondary) = resp.subs_usage.as_ref().map_or((None, None), |usage| {
        (
            usage
                .window
                .as_ref()
                .and_then(|w| w.to_window(SESSION_MINUTES)),
            usage
                .weekly
                .as_ref()
                .and_then(|w| w.to_window(WEEK_MINUTES)),
        )
    });
    // Pay-as-you-go and some active plans steadily report no windows, and say so
    // through `is_subs_active`. A response that does not is a shape this misread.
    if primary.is_none() && secondary.is_none() && resp.is_subs_active.is_none() {
        return Err(anyhow!("Muse returned no usage"));
    }
    Ok(ProviderPayload::live(
        "muse",
        "api",
        UsageSnapshot {
            primary,
            secondary,
            login_method: resp
                .subs_tier_name
                .map(|tier| tier.trim().to_string())
                .filter(|tier| !tier.is_empty()),
            ..UsageSnapshot::at(Utc::now())
        },
    ))
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let token = access_token().ok_or_else(|| anyhow!("Muse not signed in - run `muse login`"))?;
    let response = http_client(timeout)?
        .post(KEY_URL)
        .bearer_auth(token)
        .header("accept", "application/json")
        .json(&serde_json::json!({}))
        .send()
        .context("Muse usage request failed")?;
    check_status(response.status(), "Muse", "run `muse login`")?;
    let body = response
        .json()
        .context("Muse returned an unreadable usage")?;
    Ok(vec![to_payload(body)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(body: &str) -> Result<ProviderPayload> {
        to_payload(serde_json::from_str(body).unwrap())
    }

    #[test]
    fn a_subscription_draws_both_windows_and_its_tier() {
        let usage = payload(
            r#"{"is_subs_active":true,"subs_tier_name":"Muse Code Max","api_key":"sk-secret",
                "subs_usage":{
                  "window":{"used_percent":25,"resets_at":1800000000,"window_duration_mins":300},
                  "weekly":{"used_percent":"60","resets_at":"2027-01-15T08:00:00Z"}}}"#,
        )
        .unwrap()
        .usage
        .unwrap();
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, Some(25));
        assert_eq!(session.window_minutes, Some(300));
        assert!(session.resets_at.unwrap().starts_with("2027-01-15"));
        let weekly = usage.secondary.unwrap();
        assert_eq!(weekly.used_percent, Some(60));
        assert_eq!(weekly.window_minutes, Some(WEEK_MINUTES));
        assert_eq!(usage.login_method.as_deref(), Some("Muse Code Max"));
    }

    #[test]
    fn a_reset_in_milliseconds_is_the_same_instant() {
        assert_eq!(
            reset_iso(&serde_json::json!(1_800_000_000_000_i64)),
            reset_iso(&serde_json::json!(1_800_000_000_i64))
        );
    }

    #[test]
    fn the_minted_key_never_reaches_the_snapshot() {
        let payload = payload(
            r#"{"is_subs_active":true,"api_key":"sk-secret","key":"sk-secret",
                "subs_usage":{"window":{"used_percent":1}}}"#,
        )
        .unwrap();
        assert!(
            !serde_json::to_string(&payload)
                .unwrap()
                .contains("sk-secret")
        );
    }

    #[test]
    fn pay_as_you_go_and_a_plan_without_windows_are_answers_and_silence_is_not() {
        assert!(payload(r#"{"is_subs_active":false,"subs_tier_name":null}"#).is_ok());
        let power = payload(r#"{"is_subs_active":true,"subs_tier_name":"Power"}"#).unwrap();
        assert_eq!(power.usage.unwrap().login_method.as_deref(), Some("Power"));
        assert!(payload("{}").is_err());
    }

    #[test]
    fn the_token_is_only_read_where_muse_files_it() {
        let filed = r#"{"providers":{"meta":{"access_token":" t "}}}"#;
        assert_eq!(token_in(filed), Some("t".into()));
        assert_eq!(token_in(r#"{"providers":[]}"#), None);
        assert_eq!(
            token_in(r#"{"providers":{"meta":{"access_token":""}}}"#),
            None
        );
        assert_eq!(token_in("[]"), None);
    }
}
