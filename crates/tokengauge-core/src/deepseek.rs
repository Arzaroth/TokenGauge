//! DeepSeek's prepaid balance (`GET /user/balance`). There is no plan and so no
//! window: the balance is the whole row.
//!
//! The key is `DEEPSEEK_API_KEY`, or the one opencode or pi saved.
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::Deserialize;

use crate::provider::{check_status, saved_api_key};
use crate::{CreditUnit, Credits, ProviderPayload, UsageSnapshot, http_client};

const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";
const SAVED_AS: &[&str] = &["deepseek"];

pub(crate) fn api_key() -> Option<String> {
    std::env::var("DEEPSEEK_API_KEY")
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
        .or_else(|| saved_api_key(SAVED_AS).map(|(key, _)| key))
}

#[derive(Deserialize)]
struct BalanceResponse {
    #[serde(default)]
    balance_infos: Vec<BalanceInfo>,
}

#[derive(Deserialize)]
struct BalanceInfo {
    currency: Option<String>,
    /// A decimal string on the wire.
    total_balance: Option<String>,
}

impl BalanceInfo {
    fn total(&self) -> Option<f64> {
        self.total_balance
            .as_deref()?
            .trim()
            .parse()
            .ok()
            .filter(|total: &f64| total.is_finite())
    }
}

/// One entry per currency. The one holding credit is the balance, so an empty
/// CNY entry cannot hide a funded USD one.
fn to_payload(resp: BalanceResponse) -> Result<ProviderPayload> {
    let info = resp
        .balance_infos
        .iter()
        .find(|info| info.total().is_some_and(|total| total > 0.0))
        .or_else(|| {
            resp.balance_infos
                .iter()
                .find(|info| info.total().is_some())
        })
        .ok_or_else(|| anyhow!("DeepSeek returned no balance"))?;
    let unit = match info.currency.as_deref() {
        Some("USD") => CreditUnit::Usd,
        Some("CNY") | None => CreditUnit::Cny,
        Some(_) => CreditUnit::Other,
    };
    let mut payload = ProviderPayload::live("deepseek", "api", UsageSnapshot::at(Utc::now()));
    payload.credits = Some(Credits {
        remaining: info.total(),
        limit: None,
        unit,
    });
    Ok(payload)
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let key = api_key().ok_or_else(|| anyhow!("DeepSeek key missing - set DEEPSEEK_API_KEY"))?;
    let response = http_client(timeout)?
        .get(BALANCE_URL)
        .bearer_auth(key)
        .send()
        .context("DeepSeek balance request failed")?;
    check_status(response.status(), "DeepSeek", "check DEEPSEEK_API_KEY")?;
    let body = response
        .json()
        .context("DeepSeek returned an unreadable balance")?;
    Ok(vec![to_payload(body)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credits(body: &str) -> Credits {
        to_payload(serde_json::from_str(body).unwrap())
            .unwrap()
            .credits
            .unwrap()
    }

    #[test]
    fn an_empty_currency_does_not_hide_a_funded_one() {
        let balance = credits(
            r#"{"is_available":true,"balance_infos":[
                {"currency":"CNY","total_balance":"0.00"},
                {"currency":"USD","total_balance":"12.50"}]}"#,
        );
        assert_eq!(balance.remaining, Some(12.5));
        assert_eq!(balance.unit, CreditUnit::Usd);
    }

    #[test]
    fn a_yuan_balance_is_not_drawn_as_dollars() {
        let balance = credits(r#"{"balance_infos":[{"currency":"CNY","total_balance":"110.00"}]}"#);
        assert_eq!(balance.unit, CreditUnit::Cny);
    }

    #[test]
    fn an_empty_account_reads_as_zero_and_no_figure_is_an_error() {
        let zero = credits(r#"{"balance_infos":[{"currency":"USD","total_balance":"0.00"}]}"#);
        assert_eq!(zero.remaining, Some(0.0));
        let none: BalanceResponse = serde_json::from_str(r#"{"balance_infos":[]}"#).unwrap();
        assert!(to_payload(none).is_err());
    }
}
