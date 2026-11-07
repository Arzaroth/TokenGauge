//! Google Antigravity's plan and limits, from the Cloud Code API
//! (`v1internal:loadCodeAssist`, then `v1internal:retrieveUserQuotaSummary`).
//!
//! The sign-in is `agy`'s: in the Secret Service keyring where there is one,
//! otherwise its `antigravity-oauth-token` file. It is only read. A lapsed access
//! token is reported, because `agy` owns renewing it.
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::provider::check_status;
use crate::{
    ExtraRateWindow, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8, slug,
};

const API_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
const SESSION_MINUTES: u32 = 300;
const WEEK_MINUTES: u32 = 10_080;

fn sign_in_file() -> Option<PathBuf> {
    let dir = std::env::var_os("AGY_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(dirs::home_dir()?.join(".gemini").join("antigravity-cli")))?;
    Some(dir.join("antigravity-oauth-token"))
}

#[cfg(target_os = "linux")]
fn keyring_sign_in() -> Option<String> {
    let mut lookup = std::process::Command::new("secret-tool");
    lookup.args(["lookup", "service", "gemini", "username", "antigravity"]);
    let output = crate::fetch::run_with_timeout(lookup, Duration::from_secs(3)).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

#[cfg(not(target_os = "linux"))]
fn keyring_sign_in() -> Option<String> {
    None
}

#[derive(Deserialize)]
struct SignIn {
    token: Option<TokenBlock>,
    access_token: Option<String>,
    expiry: Option<String>,
}

#[derive(Deserialize)]
struct TokenBlock {
    access_token: Option<String>,
    expiry: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Login {
    Fresh(String),
    Lapsed,
    Absent,
}

fn parse_login(raw: &str, now: DateTime<Utc>) -> Option<Login> {
    let sign_in: SignIn = serde_json::from_str(raw).ok()?;
    let (nested_token, nested_expiry) = sign_in
        .token
        .map(|block| (block.access_token, block.expiry))
        .unwrap_or_default();
    let token = nested_token
        .or(sign_in.access_token)
        .filter(|t| !t.is_empty())?;
    let lapsed = nested_expiry
        .or(sign_in.expiry)
        .and_then(|iso| DateTime::parse_from_rfc3339(&iso).ok())
        .is_some_and(|expiry| expiry <= now);
    Some(if lapsed {
        Login::Lapsed
    } else {
        Login::Fresh(token)
    })
}

pub(crate) fn login() -> Login {
    let now = Utc::now();
    keyring_sign_in()
        .and_then(|raw| parse_login(&raw, now))
        .or_else(|| {
            let raw = std::fs::read_to_string(sign_in_file()?).ok()?;
            parse_login(&raw, now)
        })
        .unwrap_or(Login::Absent)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssistResponse {
    paid_tier: Option<Tier>,
    current_tier: Option<Tier>,
    #[serde(default)]
    cloudaicompanion_project: Value,
}

#[derive(Deserialize)]
struct Tier {
    name: Option<String>,
    id: Option<String>,
}

impl AssistResponse {
    fn plan(&self) -> Option<String> {
        [&self.paid_tier, &self.current_tier]
            .into_iter()
            .flatten()
            .find_map(Tier::label)
    }

    /// The id arrives as a string or as `{id}`.
    fn project_body(&self) -> Value {
        let project = &self.cloudaicompanion_project;
        match project
            .as_str()
            .or_else(|| project.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => json!({ "project": id }),
            None => json!({}),
        }
    }
}

impl Tier {
    fn label(&self) -> Option<String> {
        let name = self.name.as_deref().unwrap_or("").trim();
        let id = self.id.as_deref().unwrap_or("").trim();
        if name.is_empty() && id.is_empty() {
            return None;
        }
        let haystack = format!("{name} {id}").to_lowercase();
        for (word, label) in [("ultra", "Ultra"), ("pro", "Pro"), ("free", "Free")] {
            if haystack.contains(word) {
                return Some(label.to_string());
            }
        }
        Some(if name.is_empty() { id } else { name }.to_string())
    }
}

#[derive(Deserialize)]
struct QuotaSummary {
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    buckets: Vec<Bucket>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Group {
    display_name: Option<String>,
    #[serde(default)]
    buckets: Vec<Bucket>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bucket {
    window: Option<String>,
    bucket_id: Option<String>,
    display_name: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(alias = "remaining_fraction")]
    remaining_fraction: Option<f64>,
    #[serde(default, alias = "remaining_amount")]
    remaining_amount: Value,
    #[serde(alias = "reset_time")]
    reset_time: Option<String>,
}

impl Bucket {
    /// How much is spent. Proto3 JSON drops zero values, so a bucket with a reset
    /// time and no remainder has none left. An amount gives no share of the whole
    /// unless it is zero.
    fn used_percent(&self) -> Option<u8> {
        if self.disabled {
            return None;
        }
        if let Some(fraction) = self.remaining_fraction.filter(|f| f.is_finite()) {
            return Some(pct_u8((1.0 - fraction.clamp(0.0, 1.0)) * 100.0));
        }
        let amount_is_zero = self.remaining_amount.as_f64() == Some(0.0)
            || self.remaining_amount.as_str() == Some("0");
        if self.remaining_amount.is_null() {
            return self.reset_time.is_some().then_some(100);
        }
        amount_is_zero.then_some(100)
    }

    fn names(&self, needles: &[&str]) -> bool {
        let names = [&self.window, &self.bucket_id, &self.display_name];
        names.into_iter().flatten().any(|name| {
            let name = name.to_lowercase();
            needles.iter().any(|needle| name.contains(needle))
        })
    }

    fn to_window(&self, minutes: u32) -> Option<UsageWindow> {
        Some(UsageWindow {
            used_percent: Some(self.used_percent()?),
            reset_description: None,
            resets_at: self.reset_time.clone(),
            window_minutes: Some(minutes),
            burn: None,
            opened_at: None,
        })
    }

    fn is_session(&self) -> bool {
        self.names(&["5h", "five"])
    }

    fn is_week(&self) -> bool {
        self.names(&["week"])
    }
}

fn is_gemini(group: &Group) -> bool {
    group
        .display_name
        .as_deref()
        .is_some_and(|name| name.to_lowercase().contains("gemini"))
}

fn to_payload(
    assist: &AssistResponse,
    summary: QuotaSummary,
    now: DateTime<Utc>,
) -> ProviderPayload {
    let mut groups = summary.groups;
    if groups.is_empty() && !summary.buckets.is_empty() {
        groups.push(Group {
            display_name: Some("Gemini".into()),
            buckets: summary.buckets,
        });
    }
    let main = groups.iter().position(is_gemini).unwrap_or(0);
    let (primary, secondary) = groups.get(main).map_or((None, None), |group| {
        (
            group
                .buckets
                .iter()
                .filter(|b| b.is_session())
                .find_map(|b| b.to_window(SESSION_MINUTES)),
            group
                .buckets
                .iter()
                .filter(|b| b.is_week())
                .find_map(|b| b.to_window(WEEK_MINUTES)),
        )
    });

    let mut extra_rate_windows = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index == main {
            continue;
        }
        let name = group.display_name.as_deref().unwrap_or("Other").trim();
        let short = if name.to_lowercase().contains("claude") {
            "Claude/GPT"
        } else {
            name
        };
        for bucket in &group.buckets {
            let week = bucket.is_week();
            let minutes = if week { WEEK_MINUTES } else { SESSION_MINUTES };
            let title = format!("{short} {}", if week { "Weekly" } else { "Session" });
            if let Some(window) = bucket.to_window(minutes) {
                extra_rate_windows.push(ExtraRateWindow {
                    id: Some(format!("antigravity-{}", slug(&title))),
                    title: Some(title),
                    window: Some(window),
                    placeholder: false,
                });
            }
        }
    }

    ProviderPayload::live(
        "antigravity",
        "api",
        UsageSnapshot {
            primary,
            secondary,
            login_method: assist.plan(),
            extra_rate_windows,
            ..UsageSnapshot::at(now)
        },
    )
}

fn post<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    token: &str,
    method: &str,
    body: &Value,
) -> Result<T> {
    let response = client
        .post(format!("{API_BASE}/v1internal:{method}"))
        .bearer_auth(token)
        .header("user-agent", "antigravity")
        .json(body)
        .send()
        .with_context(|| format!("Antigravity {method} request failed"))?;
    check_status(
        response.status(),
        "Antigravity",
        "run `agy` to sign in again",
    )?;
    response
        .json()
        .with_context(|| format!("Antigravity returned an unreadable {method}"))
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let token = match login() {
        Login::Fresh(token) => token,
        Login::Lapsed => return Err(anyhow!("Antigravity sign-in expired - run `agy`")),
        Login::Absent => return Err(anyhow!("Antigravity not signed in - run `agy`")),
    };
    let client = http_client(timeout)?;
    let assist: AssistResponse = post(&client, &token, "loadCodeAssist", &json!({}))?;
    let summary: QuotaSummary = post(
        &client,
        &token,
        "retrieveUserQuotaSummary",
        &assist.project_body(),
    )?;
    Ok(vec![to_payload(&assist, summary, Utc::now())])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSIST: &str = r#"{"currentTier":{"id":"free-tier"},"paidTier":{"name":"Google AI Ultra"},
            "cloudaicompanionProject":{"id":"p-1"}}"#;

    fn usage(summary: &str) -> UsageSnapshot {
        let assist: AssistResponse = serde_json::from_str(ASSIST).unwrap();
        to_payload(&assist, serde_json::from_str(summary).unwrap(), Utc::now())
            .usage
            .unwrap()
    }

    #[test]
    fn the_plan_is_the_paid_tier_and_the_project_is_asked_by_id() {
        let assist: AssistResponse = serde_json::from_str(ASSIST).unwrap();
        assert_eq!(assist.plan().as_deref(), Some("Ultra"));
        assert_eq!(assist.project_body(), json!({"project": "p-1"}));
        let plain: AssistResponse =
            serde_json::from_str(r#"{"cloudaicompanionProject":"p-2"}"#).unwrap();
        assert_eq!(plain.project_body(), json!({"project": "p-2"}));
        let none: AssistResponse = serde_json::from_str("{}").unwrap();
        assert_eq!(none.project_body(), json!({}));
    }

    #[test]
    fn gemini_fills_the_gauges_and_other_groups_become_extra_windows() {
        let usage = usage(
            r#"{"groups":[
                {"displayName":"Claude and GPT","buckets":[
                    {"window":"weekly","remainingFraction":0.5,"resetTime":"2026-12-01T00:00:00Z"}]},
                {"displayName":"Gemini","buckets":[
                    {"window":"5h","remainingFraction":0.75,"resetTime":"2026-12-01T00:00:00Z"},
                    {"bucketId":"gemini-weekly","remainingFraction":0.1}]}]}"#,
        );
        assert_eq!(usage.primary.unwrap().used_percent, Some(25));
        assert_eq!(usage.secondary.unwrap().used_percent, Some(90));
        let extra = &usage.extra_rate_windows[0];
        assert_eq!(extra.title.as_deref(), Some("Claude/GPT Weekly"));
        assert_eq!(extra.window.as_ref().unwrap().used_percent, Some(50));
    }

    #[test]
    fn a_dropped_zero_is_a_spent_bucket_and_a_disabled_one_counts_for_nothing() {
        let usage = usage(
            r#"{"buckets":[
                {"window":"5h","resetTime":"2026-12-01T00:00:00Z"},
                {"window":"weekly","disabled":true,"remainingFraction":0.2}]}"#,
        );
        assert_eq!(usage.primary.unwrap().used_percent, Some(100));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn a_disabled_bucket_does_not_hide_the_one_behind_it() {
        let usage = usage(
            r#"{"buckets":[
                {"window":"5h","disabled":true,"remainingFraction":0.9},
                {"window":"5h","remainingFraction":0.5}]}"#,
        );
        assert_eq!(usage.primary.unwrap().used_percent, Some(50));
    }

    #[test]
    fn a_lapsed_sign_in_is_never_sent() {
        let now = DateTime::parse_from_rfc3339("2026-11-06T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let raw =
            |expiry: &str| format!(r#"{{"token":{{"access_token":"a","expiry":"{expiry}"}}}}"#);
        assert_eq!(
            parse_login(&raw("2026-11-06T13:00:00Z"), now),
            Some(Login::Fresh("a".into()))
        );
        assert_eq!(
            parse_login(&raw("2026-11-06T11:00:00Z"), now),
            Some(Login::Lapsed)
        );
        assert_eq!(
            parse_login(r#"{"access_token":"b"}"#, now),
            Some(Login::Fresh("b".into()))
        );
        assert_eq!(parse_login(r#"{"token":{}}"#, now), None);
    }
}
