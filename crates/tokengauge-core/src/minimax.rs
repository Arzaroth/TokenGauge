//! MiniMax's Token Plan limits (`GET /v1/token_plan/remains`).
//!
//! The key is `MINIMAX_API_KEY`, or the one opencode or pi saved. A key answers
//! only on its own region's host, so the host follows where the key came from.
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;

use crate::credentials::{LiveLogin, key_login, stored_key};
use crate::provider::{check_status, saved_api_key};
use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const GLOBAL_HOST: &str = "https://api.minimax.io";
const CN_HOST: &str = "https://api.minimaxi.com";
const SAVED_AS: &[&str] = &[
    "minimax",
    "minimax-cn",
    "minimax-coding-plan",
    "minimax-cn-coding-plan",
];
const MEDIA_WORDS: &[&str] = &["video", "image", "audio", "vision"];
/// What MiniMax answers a bad key with, under a 200.
const AUTH_STATUS_CODES: &[i64] = &[1004, 2049];
const STATUS_EXHAUSTED: i64 = 2;
const STATUS_UNLIMITED: i64 = 3;
const SESSION_MINUTES: u32 = 300;
const WEEK_MINUTES: u32 = 10_080;

fn env_clean(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) fn credential() -> Result<Option<(String, &'static str)>> {
    if let Some(key) = env_clean("MINIMAX_API_KEY") {
        let host = if env_clean("MINIMAX_REGION").as_deref() == Some("cn") {
            CN_HOST
        } else {
            GLOBAL_HOST
        };
        return Ok(Some((key, host)));
    }
    Ok(saved_api_key(SAVED_AS)?.map(|(key, saved_as)| {
        let host = if saved_as.starts_with("minimax-cn") {
            CN_HOST
        } else {
            GLOBAL_HOST
        };
        (key, host)
    }))
}

#[derive(Deserialize)]
struct RemainsResponse {
    #[serde(default)]
    model_remains: Vec<ModelRemains>,
    base_resp: Option<BaseResp>,
}

#[derive(Deserialize)]
struct BaseResp {
    status_code: Option<i64>,
    status_msg: Option<String>,
}

#[derive(Deserialize)]
struct ModelRemains {
    model_name: Option<String>,
    current_interval_remaining_percent: Option<f64>,
    current_interval_status: Option<i64>,
    current_interval_total_count: Option<f64>,
    end_time: Option<i64>,
    remains_time: Option<i64>,
    current_weekly_remaining_percent: Option<f64>,
    current_weekly_status: Option<i64>,
    current_weekly_total_count: Option<f64>,
    weekly_end_time: Option<i64>,
    weekly_remains_time: Option<i64>,
}

impl ModelRemains {
    /// Both windows unlimited with nothing allotted: a model the plan does not
    /// cover, rather than one with room left.
    fn outside_the_plan(&self) -> bool {
        self.current_interval_total_count == Some(0.0)
            && self.current_weekly_total_count == Some(0.0)
            && self.current_interval_status == Some(STATUS_UNLIMITED)
            && self.current_weekly_status == Some(STATUS_UNLIMITED)
    }

    fn is_media(&self) -> bool {
        let name = self.model_name.as_deref().unwrap_or("").to_lowercase();
        MEDIA_WORDS.iter().any(|word| name.contains(word))
    }
}

fn window(
    remaining_percent: Option<f64>,
    status: Option<i64>,
    end_ms: Option<i64>,
    remains_ms: Option<i64>,
    minutes: u32,
    now: DateTime<Utc>,
) -> Option<UsageWindow> {
    // The status outranks the percentage: an exhausted window may omit it or
    // keep an old one, and an unlimited one has nothing to fill.
    let used = match status {
        Some(STATUS_UNLIMITED) => return None,
        Some(STATUS_EXHAUSTED) => 100,
        _ => pct_u8(100.0 - remaining_percent?.clamp(0.0, 100.0)),
    };
    let resets_at = end_ms
        .filter(|ms| *ms > 0)
        .and_then(DateTime::from_timestamp_millis)
        .or_else(|| {
            let remains = remains_ms.filter(|ms| *ms > 0)?;
            now.checked_add_signed(TimeDelta::milliseconds(remains))
        });
    Some(UsageWindow {
        used_percent: Some(used),
        reset_description: None,
        resets_at: resets_at.map(|at| at.to_rfc3339()),
        window_minutes: Some(minutes),
        burn: None,
        opened_at: None,
    })
}

fn to_payload(
    resp: RemainsResponse,
    now: DateTime<Utc>,
    auth_hint: &str,
) -> Result<ProviderPayload> {
    if let Some(base) = resp.base_resp.as_ref()
        && let Some(code) = base.status_code.filter(|code| *code != 0)
    {
        if AUTH_STATUS_CODES.contains(&code) {
            return Err(anyhow!("MiniMax unauthorized - {auth_hint}"));
        }
        return Err(anyhow!(
            "MiniMax: {}",
            base.status_msg
                .as_deref()
                .unwrap_or("quota request refused")
        ));
    }
    let rows: Vec<&ModelRemains> = resp
        .model_remains
        .iter()
        .filter(|row| !row.outside_the_plan())
        .collect();
    let general = rows
        .iter()
        .find(|row| row.model_name.as_deref() == Some("general"))
        .or_else(|| rows.iter().find(|row| !row.is_media()))
        .ok_or_else(|| anyhow!("MiniMax returned no Token Plan quota"))?;
    let primary = window(
        general.current_interval_remaining_percent,
        general.current_interval_status,
        general.end_time,
        general.remains_time,
        SESSION_MINUTES,
        now,
    );
    let secondary = window(
        general.current_weekly_remaining_percent,
        general.current_weekly_status,
        general.weekly_end_time,
        general.weekly_remains_time,
        WEEK_MINUTES,
        now,
    );
    if primary.is_none() && secondary.is_none() {
        return Err(anyhow!("MiniMax returned no percentage-based quota"));
    }
    Ok(ProviderPayload::live(
        "minimax",
        "api",
        UsageSnapshot {
            primary,
            secondary,
            ..UsageSnapshot::at(now)
        },
    ))
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let (key, host) =
        credential()?.ok_or_else(|| anyhow!("MiniMax key missing - set MINIMAX_API_KEY"))?;
    Ok(vec![remains_for(
        &key,
        host,
        timeout,
        "check MINIMAX_API_KEY",
    )?])
}

pub(crate) fn live_login() -> LiveLogin {
    key_login(credential().ok().flatten().map(|(key, _)| key))
}

/// A stored key's host, from the region remuda filed it under. A key stored
/// before regions were is global, as an unset `MINIMAX_REGION` is.
fn stored_host(text: &str) -> &'static str {
    let region = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| value.get("region")?.as_str().map(str::to_string));
    if region.as_deref() == Some("cn") {
        CN_HOST
    } else {
        GLOBAL_HOST
    }
}

pub(crate) fn fetch_stored(
    text: &str,
    timeout: Duration,
    _now: DateTime<Utc>,
) -> Result<ProviderPayload> {
    remains_for(
        &stored_key(text)?,
        stored_host(text),
        timeout,
        "`remuda login -p minimax` replaces the stored key",
    )
}

fn remains_for(
    key: &str,
    host: &str,
    timeout: Duration,
    auth_hint: &str,
) -> Result<ProviderPayload> {
    let response = http_client(timeout)?
        .get(format!("{host}/v1/token_plan/remains"))
        .bearer_auth(key)
        .send()
        .context("MiniMax quota request failed")?;
    check_status(response.status(), "MiniMax", auth_hint)?;
    let body = response
        .json()
        .context("MiniMax returned an unreadable quota")?;
    to_payload(body, Utc::now(), auth_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: &str = "check MINIMAX_API_KEY";

    #[test]
    fn a_stored_key_answers_on_the_host_of_the_region_it_was_filed_under() {
        assert_eq!(stored_host(r#"{"key": "k", "region": "cn"}"#), CN_HOST);
        assert_eq!(
            stored_host(r#"{"key": "k", "region": "global"}"#),
            GLOBAL_HOST
        );
        assert_eq!(stored_host(r#"{"key": "k"}"#), GLOBAL_HOST);
    }

    fn usage(body: &str) -> UsageSnapshot {
        to_payload(serde_json::from_str(body).unwrap(), Utc::now(), HINT)
            .unwrap()
            .usage
            .unwrap()
    }

    #[test]
    fn the_general_row_stands_for_the_plan_and_percentages_are_what_is_left() {
        let usage = usage(
            r#"{"model_remains":[
                {"model_name":"image-01","current_interval_remaining_percent":1},
                {"model_name":"general","current_interval_remaining_percent":75,
                 "end_time":1800000000000,"current_weekly_remaining_percent":40,
                 "weekly_remains_time":3600000}]}"#,
        );
        let session = usage.primary.unwrap();
        assert_eq!(session.used_percent, Some(25));
        assert!(session.resets_at.unwrap().starts_with("2027-01-15"));
        assert_eq!(usage.secondary.unwrap().used_percent, Some(60));
    }

    #[test]
    fn a_reset_with_no_end_time_is_placed_from_the_time_remaining() {
        let now = Utc::now();
        let body = serde_json::from_str(
            r#"{"model_remains":[{"model_name":"general",
                "current_weekly_remaining_percent":40,"weekly_remains_time":3600000}]}"#,
        )
        .unwrap();
        let weekly = to_payload(body, now, HINT)
            .unwrap()
            .usage
            .unwrap()
            .secondary
            .unwrap();
        assert_eq!(
            weekly.resets_at.unwrap(),
            (now + TimeDelta::hours(1)).to_rfc3339()
        );
    }

    #[test]
    fn the_status_outranks_the_percentage() {
        let usage = usage(
            r#"{"model_remains":[{"model_name":"general",
                "current_interval_status":2,"current_interval_remaining_percent":90,
                "current_weekly_status":3,"current_weekly_remaining_percent":90}]}"#,
        );
        assert_eq!(usage.primary.unwrap().used_percent, Some(100));
        assert!(usage.secondary.is_none());
    }

    #[test]
    fn a_model_outside_the_plan_never_stands_for_it() {
        let body: RemainsResponse = serde_json::from_str(
            r#"{"model_remains":[{"model_name":"m",
                "current_interval_total_count":0,"current_weekly_total_count":0,
                "current_interval_status":3,"current_weekly_status":3}]}"#,
        )
        .unwrap();
        assert!(to_payload(body, Utc::now(), HINT).is_err());
    }

    #[test]
    fn a_bad_key_under_a_200_is_a_login_problem() {
        let body: RemainsResponse =
            serde_json::from_str(r#"{"base_resp":{"status_code":2049,"status_msg":"x"}}"#).unwrap();
        let err = to_payload(body, Utc::now(), HINT).unwrap_err().to_string();
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[test]
    fn a_stored_key_refused_under_a_200_names_the_stored_key() {
        let body: RemainsResponse =
            serde_json::from_str(r#"{"base_resp":{"status_code":1004,"status_msg":"x"}}"#).unwrap();
        let err = to_payload(body, Utc::now(), "replace the stored key")
            .unwrap_err()
            .to_string();
        assert_eq!(err, "MiniMax unauthorized - replace the stored key");
    }
}
