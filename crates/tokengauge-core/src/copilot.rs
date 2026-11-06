//! GitHub Copilot's monthly allowance, from `GET /copilot_internal/user`: the
//! endpoint the Copilot CLI and editors read. GitHub documents no personal-account
//! usage API.
//!
//! The token is `COPILOT_QUOTA_TOKEN`, `GH_TOKEN`, `GITHUB_TOKEN`, or what
//! `gh auth token` prints.
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use crate::fetch::run_with_timeout;
use crate::provider::{check_status, json_num};
use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const QUOTA_URL: &str = "https://api.github.com/copilot_internal/user";
pub(crate) const TOKEN_ENVS: &[&str] = &["COPILOT_QUOTA_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"];
const GH_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) fn token() -> Option<String> {
    TOKEN_ENVS
        .iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(gh_token)
}

fn gh_token() -> Option<String> {
    let mut gh = Command::new("gh");
    // An Enterprise GH_HOST holds a token that means nothing to github.com.
    gh.args(["auth", "token", "--hostname", "github.com"])
        .env_remove("GH_HOST");
    let output = run_with_timeout(gh, GH_TIMEOUT).ok()?;
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !token.is_empty()).then_some(token)
}

#[derive(Deserialize)]
struct UserResponse {
    copilot_plan: Option<String>,
    quota_reset_date_utc: Option<String>,
    #[serde(default)]
    quota_snapshots: QuotaSnapshots,
}

#[derive(Deserialize, Default)]
struct QuotaSnapshots {
    premium_interactions: Option<Snapshot>,
}

#[derive(Deserialize)]
struct Snapshot {
    #[serde(default)]
    unlimited: bool,
    #[serde(default)]
    entitlement: Value,
    #[serde(default)]
    remaining: Value,
    #[serde(default)]
    quota_remaining: Value,
    #[serde(default)]
    percent_remaining: Value,
    #[serde(default)]
    credits_used: Value,
}

impl Snapshot {
    /// A share of the allowance. The wire says what is left, in one of three
    /// fields, or what is spent where a `credits_used` is sent.
    fn used_percent(&self) -> Option<u8> {
        let entitlement = json_num(&self.entitlement).filter(|e| *e > 0.0);
        let percent = json_num(&self.credits_used)
            .zip(entitlement)
            .map(|(used, whole)| used / whole * 100.0)
            .or_else(|| {
                let left = json_num(&self.remaining).or_else(|| json_num(&self.quota_remaining))?;
                let whole = entitlement?;
                Some((whole - left) / whole * 100.0)
            })
            .or_else(|| Some(100.0 - json_num(&self.percent_remaining)?))?;
        Some(pct_u8(percent.clamp(0.0, 100.0)))
    }
}

fn plan_label(plan: &str) -> String {
    match plan.to_ascii_lowercase().as_str() {
        "individual" | "pro" => "Pro".into(),
        "individual_pro" | "pro_plus" | "pro+" => "Pro+".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "free" | "copilot_free" => "Free".into(),
        _ => plan.to_string(),
    }
}

fn to_payload(resp: UserResponse) -> Result<ProviderPayload> {
    let snapshot = resp
        .quota_snapshots
        .premium_interactions
        .ok_or_else(|| anyhow!("Copilot returned no allowance"))?;
    // A plan with no metered allowance reports zero of everything, which read
    // literally is an allowance spent in full.
    let metered = !snapshot.unlimited && json_num(&snapshot.entitlement) != Some(0.0);
    let primary = if !metered {
        None
    } else {
        Some(UsageWindow {
            used_percent: Some(
                snapshot
                    .used_percent()
                    .ok_or_else(|| anyhow!("Copilot returned an unreadable allowance"))?,
            ),
            reset_description: None,
            resets_at: resp.quota_reset_date_utc,
            window_minutes: None,
            burn: None,
            opened_at: None,
        })
    };
    Ok(ProviderPayload::live(
        "copilot",
        "api",
        UsageSnapshot {
            primary,
            login_method: resp.copilot_plan.as_deref().map(plan_label),
            ..UsageSnapshot::at(Utc::now())
        },
    ))
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let token = token().ok_or_else(|| {
        anyhow!("Copilot not signed in - run `gh auth login` or set COPILOT_QUOTA_TOKEN")
    })?;
    let response = http_client(timeout)?
        .get(QUOTA_URL)
        .bearer_auth(token)
        .header("accept", "application/json")
        // api.github.com answers a request with no agent with a bare 403.
        .header("user-agent", "tokengauge")
        .send()
        .context("Copilot quota request failed")?;
    check_status(response.status(), "Copilot", "run `gh auth login`")?;
    let body = response
        .json()
        .context("Copilot returned an unreadable quota")?;
    Ok(vec![to_payload(body)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(snapshot: &str) -> Result<UsageSnapshot> {
        let body = format!(
            r#"{{"copilot_plan":"individual","quota_reset_date_utc":"2026-12-01T00:00:00Z",
                 "quota_snapshots":{{"premium_interactions":{snapshot}}}}}"#
        );
        Ok(to_payload(serde_json::from_str(&body)?)?.usage.unwrap())
    }

    #[test]
    fn the_allowance_is_read_from_whichever_field_carries_it() {
        for snapshot in [
            r#"{"entitlement":300,"remaining":225}"#,
            r#"{"entitlement":300,"quota_remaining":225}"#,
            r#"{"entitlement":300,"percent_remaining":75}"#,
            r#"{"entitlement":300,"credits_used":75,"remaining":1}"#,
        ] {
            let usage = usage(snapshot).unwrap();
            assert_eq!(usage.primary.unwrap().used_percent, Some(25), "{snapshot}");
            assert_eq!(usage.login_method.as_deref(), Some("Pro"));
        }
    }

    #[test]
    fn an_unlimited_plan_draws_no_gauge() {
        assert!(usage(r#"{"unlimited":true}"#).unwrap().primary.is_none());
    }

    #[test]
    fn a_plan_with_no_metered_allowance_is_not_a_spent_one() {
        let usage = usage(
            r#"{"credits_used":0,"entitlement":0,"has_quota":false,"percent_remaining":0.0,
                "quota_remaining":0.0,"remaining":0,"unlimited":false}"#,
        );
        assert!(usage.unwrap().primary.is_none());
    }

    #[test]
    fn a_limited_allowance_with_no_figure_is_a_failed_check_not_an_untouched_one() {
        assert!(usage(r#"{"entitlement":300}"#).is_err());
    }

    #[test]
    fn no_allowance_at_all_is_an_error() {
        let body = serde_json::from_str(r#"{"copilot_plan":"free"}"#).unwrap();
        assert!(to_payload(body).is_err());
    }
}
