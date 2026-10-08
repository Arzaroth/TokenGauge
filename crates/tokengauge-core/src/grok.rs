//! Native Grok (xAI "grok build") usage fetcher.
//!
//! Read-only: reads the `grok` CLI's own `~/.grok/auth.json` (honoring
//! `GROK_HOME`) and calls the grok.com build-billing endpoint. That endpoint
//! speaks gRPC-web (protobuf over HTTP POST), not JSON, so the response is
//! scanned field-by-field rather than deserialized. TokenGauge never refreshes
//! the token - `grok login` owns that; on an expired/rejected token we surface
//! an error and let the stale-cache fallback keep the last-good number.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::credentials::{LiveLogin, LoginTokens};
use crate::provider::{check_status, jwt_claims};
use crate::{CredentialState, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const BILLING_ENDPOINT: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const SETTINGS_ENDPOINT: &str = "https://cli-chat-proxy.grok.com/v1/settings";
/// The tier is a label, not a figure: it may not double a slow fetch.
const SETTINGS_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Credentials (read-only)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Credentials {
    access_token: String,
    login_method: Option<String>,
}

pub(crate) fn auth_path() -> PathBuf {
    if let Ok(home) = std::env::var("GROK_HOME") {
        let home = home.trim();
        if !home.is_empty() {
            return PathBuf::from(home).join("auth.json");
        }
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".grok")
        .join("auth.json")
}

fn read_credentials(path: &Path, now: DateTime<Utc>) -> Result<Credentials> {
    let text = std::fs::read_to_string(path)
        .map_err(|_| anyhow!("Grok not logged in - run `grok login`"))?;
    parse_credentials(&text, now)
}

/// Validate the on-disk credentials the fetcher would use, so `--doctor` matches
/// fetch-time behavior instead of only checking that the file exists.
pub(crate) fn credentials_valid(now: DateTime<Utc>) -> Result<()> {
    read_credentials(&auth_path(), now).map(drop)
}

/// True when the entry carries an `expires_at` that is at or before `now`.
fn is_expired(entry: &Value, now: DateTime<Utc>) -> bool {
    entry
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|expires| expires.with_timezone(&Utc) <= now)
}

fn has_key(entry: &Value) -> bool {
    entry
        .get("key")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

/// The entry a login is filed under, expired or not: the OIDC one, else the
/// first carrying a `key`. What a fetch sends skips expired entries
/// ([`parse_credentials`]); which login a file holds does not.
fn preferred(map: &serde_json::Map<String, Value>) -> Option<&Value> {
    map.iter()
        .find(|(scope, entry)| scope.starts_with("https://auth.x.ai::") && has_key(entry))
        .or_else(|| map.iter().find(|(_, entry)| has_key(entry)))
        .map(|(_, entry)| entry)
}

fn text_of(entry: &Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Who an entry's tokens belong to: the access token's own `sub`, else the
/// `user_id` written beside it. remuda files a Grok login under the same.
fn account_of(entry: &Value) -> Option<String> {
    text_of(entry, "key")
        .as_deref()
        .and_then(jwt_claims)
        .and_then(|c| c.get("sub")?.as_str().map(str::to_string))
        .filter(|s| !s.is_empty())
        .or_else(|| text_of(entry, "user_id"))
}

fn tokens_of(entry: &Value) -> LoginTokens {
    LoginTokens {
        access: text_of(entry, "key"),
        refresh: text_of(entry, "refresh_token"),
    }
}

/// `auth.json` is an object keyed by OIDC scope URL. Prefer the SuperGrok/OIDC
/// entry (`https://auth.x.ai::`), else the first entry carrying a `key`.
/// Expired entries are skipped so a stale preferred token never wins.
fn parse_credentials(text: &str, now: DateTime<Utc>) -> Result<Credentials> {
    let root: Value = serde_json::from_str(text).context("Grok auth.json was invalid")?;
    let map = root
        .as_object()
        .ok_or_else(|| anyhow!("Grok auth.json was invalid"))?;

    // Skip expired entries during selection, so an expired preferred (OIDC)
    // token never masks a later still-valid keyed fallback.
    let mut selected: Option<&Value> = None;
    let mut saw_expired = false;
    for (scope, entry) in map {
        let has_key = entry
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_key {
            continue;
        }
        if is_expired(entry, now) {
            saw_expired = true;
            continue;
        }
        if scope.starts_with("https://auth.x.ai::") {
            selected = Some(entry);
            break;
        }
        if selected.is_none() {
            selected = Some(entry);
        }
    }

    let entry = match selected {
        Some(entry) => entry,
        None if saw_expired => return Err(anyhow!("Grok token expired - run `grok login`")),
        None => return Err(anyhow!("Grok not logged in - run `grok login`")),
    };
    let access_token = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Grok not logged in - run `grok login`"))?
        .to_string();

    Ok(Credentials {
        access_token,
        login_method: login_method(entry),
    })
}

fn login_method(entry: &Value) -> Option<String> {
    match entry
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("oidc") => Some("SuperGrok".to_string()),
        Some("session") => Some("Session".to_string()),
        Some(other) if !other.is_empty() => Some(other.to_string()),
        _ => Some("Grok".to_string()),
    }
}

// ---------------------------------------------------------------------------
// gRPC-web / protobuf scanning
//
// The billing response is a protobuf message with no schema we can rely on, so
// we walk every field: the used-percent is a float (fixed32) at field path
// ending in `1` within `[0, 100]`; the reset time is a unix-seconds varint in a
// plausible range. This mirrors CodexBar / Win-CodexBar's byte scanner.
// ---------------------------------------------------------------------------

struct Billing {
    used_percent: u8,
    resets_at: Option<String>,
}

fn parse_grpc_web_response(
    data: &[u8],
    header_status: Option<u16>,
    now: DateTime<Utc>,
) -> Result<Billing> {
    // A gRPC status (trailer frame or HTTP header) is authoritative whenever
    // present: reject a non-zero status before trusting any body.
    let status = grpc_web_trailer_status(data).or(header_status);
    match status {
        Some(16) => return Err(anyhow!("Grok unauthorized - run `grok login`")),
        Some(code) if code != 0 => return Err(anyhow!("Grok billing gRPC status {code}")),
        _ => {}
    }

    let frames = grpc_web_data_frames(data);
    let messages: Vec<&[u8]> = match (&frames, status) {
        // Cleanly framed with a success status: use the data frames.
        (Some(frames), Some(_)) if !frames.is_empty() => frames.iter().map(Vec::as_slice).collect(),
        // Cleanly framed but no status is incomplete - never trust its frames.
        (Some(frames), None) if !frames.is_empty() => {
            return Err(anyhow!("Grok billing missing gRPC status"));
        }
        // Truncated/malformed framing while a status marks a framed response:
        // reject rather than accept the partially parsed prefix.
        (None, Some(_)) => return Err(anyhow!("Grok billing response was truncated")),
        // No framing and no status: treat the whole body as a raw protobuf message.
        (None, None) if !data.is_empty() => vec![data],
        // Empty body, empty frames, or a status with no data frames: nothing usable.
        _ => return Err(anyhow!("Grok billing returned no payload")),
    };

    let mut scan = ProtoScan::default();
    for message in &messages {
        // The top-level frame must parse in full; a valid prefix followed by a
        // malformed/truncated suffix is not accepted as complete billing data.
        if !scan.scan_message(message, &mut Vec::new(), 0) {
            return Err(anyhow!("Grok billing response was malformed"));
        }
    }

    let used_percent = scan
        .fixed32
        .iter()
        .filter(|f| {
            f.path.last() == Some(&1) && f.value.is_finite() && f.value >= 0.0 && f.value <= 100.0
        })
        .min_by(|a, b| a.path.len().cmp(&b.path.len()).then(a.order.cmp(&b.order)))
        .map(|f| f.value as f64);

    // The reset timestamp is the varint at protobuf path [1, 5, 1]; prefer it so
    // an unrelated future-epoch varint elsewhere can't be mistaken for the reset.
    // Fall back to the earliest plausible future epoch when that field is absent.
    let as_future_epoch = |value: u64| {
        (1_700_000_000..=2_100_000_000)
            .contains(&value)
            .then(|| Utc.timestamp_opt(value as i64, 0).single())
            .flatten()
            .filter(|dt| *dt > now)
    };
    let resets_at = scan
        .varints
        .iter()
        .find(|f| f.path == [1, 5, 1])
        .and_then(|f| as_future_epoch(f.value))
        .or_else(|| {
            scan.varints
                .iter()
                .filter_map(|f| as_future_epoch(f.value))
                .min()
        })
        .map(|dt| dt.to_rfc3339());

    // proto3 omits a zeroed percent field, so a percent-less snapshot is only a
    // genuine 0% when another billing signal (a valid reset) corroborates it;
    // otherwise there is nothing usable and the stale-cache fallback should win.
    let used = match used_percent {
        Some(value) => value,
        None if resets_at.is_some() => 0.0,
        None => return Err(anyhow!("Grok billing had no usable data")),
    };

    Ok(Billing {
        used_percent: pct_u8(used),
        resets_at,
    })
}

/// Split a gRPC-web body into its data frames, skipping trailer frames (the
/// high bit of the flags byte marks a trailer). Returns `None` when the body is
/// truncated or malformed (a frame runs past the end, or trailing bytes are too
/// short for a frame header) so a partial prefix is never treated as complete.
fn grpc_web_data_frames(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    let mut index = 0;
    while index < data.len() {
        if index + 5 > data.len() {
            return None;
        }
        let flags = data[index];
        let len = ((data[index + 1] as usize) << 24)
            | ((data[index + 2] as usize) << 16)
            | ((data[index + 3] as usize) << 8)
            | (data[index + 4] as usize);
        let start = index + 5;
        let end = start.checked_add(len).filter(|end| *end <= data.len())?;
        if flags & 0x80 == 0 {
            frames.push(data[start..end].to_vec());
        }
        index = end;
    }
    Some(frames)
}

/// Extract `grpc-status` from the gRPC-web trailer frame (high flags bit set),
/// whose body carries HTTP/1-style `grpc-status: N` header lines.
fn grpc_web_trailer_status(data: &[u8]) -> Option<u16> {
    let mut index = 0;
    while index + 5 <= data.len() {
        let flags = data[index];
        let len = ((data[index + 1] as usize) << 24)
            | ((data[index + 2] as usize) << 16)
            | ((data[index + 3] as usize) << 8)
            | (data[index + 4] as usize);
        let start = index + 5;
        let end = start.checked_add(len).filter(|end| *end <= data.len())?;
        if flags & 0x80 != 0 {
            let text = String::from_utf8_lossy(&data[start..end]);
            for line in text.split(['\r', '\n']) {
                if let Some(rest) = line.trim().strip_prefix("grpc-status:") {
                    return rest.trim().parse::<u16>().ok();
                }
            }
        }
        index = end;
    }
    None
}

#[derive(Default)]
struct ProtoScan {
    fixed32: Vec<Fixed32Field>,
    varints: Vec<VarintField>,
    order: usize,
}

struct Fixed32Field {
    path: Vec<u64>,
    value: f32,
    order: usize,
}

struct VarintField {
    path: Vec<u64>,
    value: u64,
}

impl ProtoScan {
    /// Returns true when the whole message parsed cleanly. Callers that require
    /// a well-formed message (the top-level frame) reject a `false` result;
    /// nested length-delimited fields are best-effort (a bytes/string field
    /// won't parse as a sub-message, which is expected) and ignore it.
    fn scan_message(&mut self, data: &[u8], path: &mut Vec<u64>, depth: usize) -> bool {
        if depth > 8 {
            return false;
        }
        let mut i = 0;
        while i < data.len() {
            let Some((field, wire, next)) = read_key(data, i) else {
                return false;
            };
            i = next;
            path.push(field);
            let advanced = self.scan_field(data, i, path, depth, wire);
            path.pop();
            let Some(next) = advanced else { return false };
            i = next;
        }
        true
    }

    fn scan_field(
        &mut self,
        data: &[u8],
        i: usize,
        path: &mut Vec<u64>,
        depth: usize,
        wire: u64,
    ) -> Option<usize> {
        match wire {
            0 => self.scan_varint(data, i, path),
            2 => self.scan_length_delimited(data, i, path, depth),
            5 => self.scan_fixed32(data, i, path),
            1 => i.checked_add(8).filter(|end| *end <= data.len()),
            _ => None,
        }
    }

    fn scan_varint(&mut self, data: &[u8], i: usize, path: &[u64]) -> Option<usize> {
        let (value, next) = read_varint(data, i)?;
        self.varints.push(VarintField {
            path: path.to_vec(),
            value,
        });
        Some(next)
    }

    fn scan_length_delimited(
        &mut self,
        data: &[u8],
        i: usize,
        path: &mut Vec<u64>,
        depth: usize,
    ) -> Option<usize> {
        let (len, start) = read_varint(data, i)?;
        let end = start
            .checked_add(len as usize)
            .filter(|end| *end <= data.len())?;
        // Best-effort: a bytes/string field won't parse as a sub-message, and
        // that's fine - only the top-level frame must be fully consumed.
        let _ = self.scan_message(&data[start..end], path, depth + 1);
        Some(end)
    }

    fn scan_fixed32(&mut self, data: &[u8], i: usize, path: &[u64]) -> Option<usize> {
        let bytes: [u8; 4] = data.get(i..i + 4)?.try_into().ok()?;
        self.fixed32.push(Fixed32Field {
            path: path.to_vec(),
            value: f32::from_le_bytes(bytes),
            order: self.order,
        });
        self.order += 1;
        Some(i + 4)
    }
}

fn read_key(data: &[u8], i: usize) -> Option<(u64, u64, usize)> {
    let (key, next) = read_varint(data, i)?;
    let field = key >> 3;
    // Field numbers start at 1; a zero field means this isn't a valid protobuf
    // tag, so fail closed rather than misreading arbitrary bytes as fields.
    if field == 0 {
        return None;
    }
    Some((field, key & 0x07, next))
}

fn read_varint(data: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    while i < data.len() && shift < 64 {
        let b = data[i];
        i += 1;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some((value, i));
        }
        shift += 7;
    }
    None
}

// ---------------------------------------------------------------------------
// Mapping + network
// ---------------------------------------------------------------------------

fn to_payload(
    billing: Billing,
    login_method: Option<String>,
    now: DateTime<Utc>,
) -> ProviderPayload {
    // Grok exposes a single shared weekly usage pool (no rolling sub-windows).
    let primary = UsageWindow {
        used_percent: Some(billing.used_percent),
        reset_description: None,
        resets_at: billing.resets_at,
        window_minutes: Some(10080),
    };
    ProviderPayload::live(
        "grok",
        "grok-web",
        UsageSnapshot {
            primary: Some(primary),
            login_method,
            ..UsageSnapshot::at(now)
        },
    )
}

/// What a `subscription_tier_display` is sold as. A tier not listed reads as
/// sent.
fn tier_label(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let key: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '_' | '-'))
        .collect::<String>()
        .to_lowercase();
    let name = match key.as_str() {
        "" => return None,
        "supergrok" => "SuperGrok",
        "supergrokplus" | "plus" => "SuperGrok Plus",
        "supergrokheavy" | "heavy" => "SuperGrok Heavy",
        _ => return Some(raw.to_string()),
    };
    Some(name.to_string())
}

#[derive(serde::Deserialize)]
struct Settings {
    #[serde(default)]
    subscription_tier_display: Option<String>,
}

/// A plan's weight by its price per $10, since xAI publishes no multiplier
/// between tiers: SuperGrok $30, Plus $100, Heavy $300.
fn tier_weight(label: &str) -> Option<u32> {
    match label {
        "SuperGrok" => Some(3),
        "SuperGrok Plus" => Some(10),
        "SuperGrok Heavy" => Some(30),
        _ => None,
    }
}

/// The plan as the CLI's settings name it. The billing response carries no
/// tier, and `auth_mode` says only how the user signed in, so without this
/// every paid plan reads as SuperGrok. Best effort: a failure keeps the
/// `auth_mode` label rather than failing the fetch.
fn subscription_tier(client: &reqwest::blocking::Client, token: &str) -> Option<String> {
    let resp = client
        .get(SETTINGS_ENDPOINT)
        .timeout(SETTINGS_TIMEOUT)
        .header("authorization", format!("Bearer {token}"))
        .header("x-xai-token-auth", "xai-grok-cli")
        .header("accept", "application/json")
        .header("user-agent", "TokenGauge")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let settings: Settings = resp.json().ok()?;
    settings
        .subscription_tier_display
        .as_deref()
        .and_then(tier_label)
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let creds = read_credentials(&auth_path(), now)?;
    Ok(vec![usage_for(creds, timeout, now, "run `grok login`")?])
}

fn usage_for(
    creds: Credentials,
    timeout: Duration,
    now: DateTime<Utc>,
    unauthorized_hint: &str,
) -> Result<ProviderPayload> {
    let client = http_client(timeout)?;
    let resp = client
        .post(BILLING_ENDPOINT)
        .body(vec![0u8, 0, 0, 0, 0]) // empty gRPC-web frame
        .header("authorization", format!("Bearer {}", creds.access_token))
        .header("origin", "https://grok.com")
        .header("referer", "https://grok.com/?_s=usage")
        .header("accept", "*/*")
        .header("content-type", "application/grpc-web+proto")
        .header("x-grpc-web", "1")
        .header("x-user-agent", "connect-es/2.1.1")
        .header("user-agent", "TokenGauge")
        .send()
        .context("Grok billing request failed")?;

    check_status(resp.status(), "Grok", unauthorized_hint)?;

    // gRPC carries its own status (HTTP header for unary, else the trailer frame).
    let header_status = resp
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u16>().ok());

    let bytes = resp.bytes().context("Grok billing read failed")?;
    let billing = parse_grpc_web_response(&bytes, header_status, now)?;
    let tier = subscription_tier(&client, &creds.access_token);
    let weight = tier.as_deref().and_then(tier_weight);
    let mut payload = to_payload(billing, tier.or(creds.login_method), now);
    payload.credential.plan_weight = weight;
    Ok(payload)
}

// ---------------------------------------------------------------------------
// Stored credentials (remuda)
// ---------------------------------------------------------------------------

pub(crate) fn live_login() -> LiveLogin {
    let Ok(text) = std::fs::read_to_string(auth_path()) else {
        return LiveLogin::default();
    };
    match stored_entry(&text) {
        Ok(entry) => LiveLogin {
            tokens: tokens_of(&entry),
            account: account_of(&entry),
        },
        Err(_) => LiveLogin::default(),
    }
}

/// A stored `auth.json`'s login, read the way the live file is.
fn stored_entry(text: &str) -> Result<Value> {
    // Never serde's message: it quotes the value it rejected.
    let root: Value =
        serde_json::from_str(text).map_err(|_| anyhow!("the stored auth.json was invalid"))?;
    let map = root
        .as_object()
        .ok_or_else(|| anyhow!("the stored auth.json was invalid"))?;
    preferred(map)
        .cloned()
        .ok_or_else(|| anyhow!("the stored credential holds no token"))
}

pub(crate) fn stored_tokens(text: &str) -> LoginTokens {
    stored_entry(text)
        .map(|e| tokens_of(&e))
        .unwrap_or_default()
}

/// Why a stored credential is not worth a request, if it is not. Expiry is
/// the entry's `expires_at`, else the token's `exp`. The stored copy is never
/// refreshed here: its refresh token rotates on use, and it is remuda's.
fn stored_state(entry: &Value, now: DateTime<Utc>) -> Option<CredentialState> {
    let by_claim = || {
        text_of(entry, "key")
            .as_deref()
            .and_then(jwt_claims)
            .and_then(|c| c.get("exp")?.as_i64())
            .and_then(|exp| DateTime::from_timestamp(exp, 0))
            .is_some_and(|exp| exp <= now)
    };
    (is_expired(entry, now) || by_claim()).then_some(CredentialState::Expired)
}

pub(crate) fn check_stored(text: &str, now: DateTime<Utc>) -> Result<Option<CredentialState>> {
    Ok(stored_state(&stored_entry(text)?, now))
}

/// Ask about a stored credential, or say why it was not asked.
pub(crate) fn fetch_stored(
    text: &str,
    timeout: Duration,
    now: DateTime<Utc>,
) -> Result<ProviderPayload> {
    let entry = stored_entry(text)?;
    let login_method = login_method(&entry);
    match stored_state(&entry, now) {
        None => {
            let creds = Credentials {
                access_token: text_of(&entry, "key")
                    .ok_or_else(|| anyhow!("the stored credential holds no token"))?,
                login_method,
            };
            usage_for(
                creds,
                timeout,
                now,
                "remuda refreshes stored credentials (`remuda refresh`)",
            )
        }
        Some(state) => {
            let mut payload = ProviderPayload::live(
                "grok",
                "store",
                UsageSnapshot {
                    login_method,
                    ..UsageSnapshot::at(now)
                },
            );
            payload.credential.state = Some(state);
            Ok(payload)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_oidc_entry() {
        let auth = r#"{
            "https://accounts.x.ai/sign-in": {"key": "legacy"},
            "https://auth.x.ai::abc": {"key": "oidc", "auth_mode": "oidc"}
        }"#;
        let creds = parse_credentials(auth, Utc::now()).unwrap();
        assert_eq!(creds.access_token, "oidc");
        assert_eq!(creds.login_method.as_deref(), Some("SuperGrok"));
    }

    #[test]
    fn keeps_first_fallback_over_later_sign_in() {
        // No OIDC entry: the first keyed fallback must not be displaced by a
        // later sign-in entry.
        let auth = r#"{
            "https://a.example::x": {"key": "first"},
            "https://z.example/sign-in": {"key": "later"}
        }"#;
        let creds = parse_credentials(auth, Utc::now()).unwrap();
        assert_eq!(creds.access_token, "first");
    }

    #[test]
    fn expired_oidc_falls_back_to_valid_key() {
        // Expired OIDC entry must not mask a later still-valid keyed fallback.
        let auth = r#"{
            "https://auth.x.ai::a": {"key": "oidc", "auth_mode": "oidc", "expires_at": "2000-01-01T00:00:00Z"},
            "https://accounts.x.ai/sign-in": {"key": "valid"}
        }"#;
        let creds = parse_credentials(auth, Utc::now()).unwrap();
        assert_eq!(creds.access_token, "valid");
    }

    /// Expired and never-logged-in are different states and are not fixed by
    /// the same thing. `expired_token_errors` below covers the first; this is
    /// the other side of it, and the boundary between them.
    #[test]
    fn an_absence_is_reported_as_an_absence_and_a_spent_token_as_spent() {
        for empty in ["{}", r#"{"scope": {}}"#, r#"{"scope": {"key": ""}}"#] {
            let err = parse_credentials(empty, Utc::now())
                .unwrap_err()
                .to_string();
            assert!(err.contains("not logged in"), "{empty} -> {err}");
        }

        // A token expiring exactly now is spent, not good for one more call.
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let err = parse_credentials(
            r#"{"s": {"key": "t", "expires_at": "2026-01-01T00:00:00Z"}}"#,
            now,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("expired"), "{err}");
    }

    /// A file that is not an object at all, which is what a truncated or
    /// half-written `auth.json` looks like. None of these may panic: this runs
    /// inside a fetch, and a panic there takes the bar down rather than the
    /// provider.
    #[test]
    fn a_file_that_is_not_an_object_is_rejected_rather_than_panicking() {
        for bad in ["[]", "\"token\"", "null", "not json at all", "", "{"] {
            assert!(parse_credentials(bad, Utc::now()).is_err(), "{bad:?}");
        }
    }

    /// The login method is what the panel shows beside the provider. An
    /// unknown mode is passed through rather than dropped, because whatever
    /// x.ai calls a new tier is more use than a blank.
    #[test]
    fn an_unknown_login_mode_is_shown_rather_than_dropped() {
        let named = |json: &str| {
            parse_credentials(json, Utc::now())
                .expect("credentials")
                .login_method
        };
        assert_eq!(
            named(r#"{"s": {"key": "t", "auth_mode": "OIDC"}}"#).as_deref(),
            Some("SuperGrok"),
            "the mode is matched case-insensitively"
        );
        assert_eq!(
            named(r#"{"s": {"key": "t", "auth_mode": "enterprise"}}"#).as_deref(),
            Some("enterprise")
        );
        assert_eq!(named(r#"{"s": {"key": "t"}}"#).as_deref(), Some("Grok"));
        assert_eq!(
            named(r#"{"s": {"key": "t", "auth_mode": ""}}"#).as_deref(),
            Some("Grok")
        );
    }

    /// Heavy reads as Heavy, however the settings spell it.
    #[test]
    fn a_tier_reads_as_it_is_sold() {
        let sold = |raw: &str| tier_label(raw);
        assert_eq!(sold("SuperGrok").as_deref(), Some("SuperGrok"));
        assert_eq!(sold("supergrok").as_deref(), Some("SuperGrok"));
        assert_eq!(sold("SuperGrok Heavy").as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(sold("SUPERGROK_HEAVY").as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(sold("heavy").as_deref(), Some("SuperGrok Heavy"));
        assert_eq!(sold("SuperGrok Plus").as_deref(), Some("SuperGrok Plus"));
        assert_eq!(sold(" Enterprise ").as_deref(), Some("Enterprise"));
        assert_eq!(sold("  "), None);
        assert_eq!(sold("SuperGrok++").as_deref(), Some("SuperGrok++"));
        assert_eq!(sold("超级").as_deref(), Some("超级"));
    }

    /// With no published multiplier, Heavy weighs ten SuperGroks, as it costs.
    #[test]
    fn a_tier_weighs_its_price() {
        let weight = |raw: &str| tier_label(raw).as_deref().and_then(tier_weight);
        assert_eq!(weight("SuperGrok"), Some(3));
        assert_eq!(weight("SuperGrok Plus"), Some(10));
        assert_eq!(weight("SUPERGROK_HEAVY"), Some(30));
        assert_eq!(weight("Enterprise"), None);
    }

    use crate::provider::fake_jwt as jwt;

    /// remuda files a Grok login under the token's `sub`, so the live login is
    /// matched to its stored copy by that, and by its tokens.
    #[test]
    fn a_login_is_its_tokens_and_the_tokens_sub() {
        let key = jwt(r#"{"sub":"u-1","exp":4102444800}"#);
        let text = format!(
            r#"{{"https://accounts.x.ai/sign-in": {{"key": "other"}},
                "https://auth.x.ai::c": {{"key": "{key}", "refresh_token": "r1",
                                         "user_id": "ignored", "auth_mode": "oidc"}}}}"#
        );
        let entry = stored_entry(&text).unwrap();
        assert_eq!(account_of(&entry).as_deref(), Some("u-1"));
        assert_eq!(
            stored_tokens(&text),
            LoginTokens {
                access: Some(key),
                refresh: Some("r1".into())
            }
        );
        let opaque = stored_entry(r#"{"s": {"key": "opaque", "user_id": "u-2"}}"#).unwrap();
        assert_eq!(account_of(&opaque).as_deref(), Some("u-2"));
        assert!(stored_entry(r#"{"s": {"key": ""}}"#).is_err());
        assert!(stored_entry("[]").is_err());
    }

    /// A stored login past its expiry is shown as expired, not asked about:
    /// its refresh token is remuda's to spend.
    #[test]
    fn an_expired_stored_login_is_not_asked_about() {
        let now = Utc::now();
        let alive = jwt(r#"{"sub":"u","exp":4102444800}"#);
        let dead = jwt(r#"{"sub":"u","exp":1}"#);
        let state = |entry: &str| {
            check_stored(&format!(r#"{{"https://auth.x.ai::c": {entry}}}"#), now).unwrap()
        };
        assert_eq!(state(&format!(r#"{{"key": "{alive}"}}"#)), None);
        assert_eq!(
            state(&format!(r#"{{"key": "{dead}"}}"#)),
            Some(CredentialState::Expired)
        );
        assert_eq!(
            state(&format!(
                r#"{{"key": "{alive}", "expires_at": "2020-01-01T00:00:00Z"}}"#
            )),
            Some(CredentialState::Expired)
        );
        let payload = fetch_stored(
            &format!(r#"{{"https://auth.x.ai::c": {{"key": "{dead}", "auth_mode": "oidc"}}}}"#),
            Duration::from_secs(1),
            now,
        )
        .unwrap();
        assert_eq!(payload.credential.state, Some(CredentialState::Expired));
        assert_eq!(
            payload.usage.unwrap().login_method.as_deref(),
            Some("SuperGrok")
        );
    }

    #[test]
    fn expired_token_errors() {
        let auth =
            r#"{"https://auth.x.ai::a": {"key": "t", "expires_at": "2000-01-01T00:00:00Z"}}"#;
        let err = parse_credentials(auth, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");
    }

    #[test]
    fn splits_data_frames_skipping_trailers() {
        // One data frame [1,2], then a trailer frame (flags 0x80) that is dropped.
        let data = [0, 0, 0, 0, 2, 1, 2, 0x80, 0, 0, 0, 1, b'x'];
        assert_eq!(grpc_web_data_frames(&data), Some(vec![vec![1, 2]]));
    }

    #[test]
    fn truncated_frame_is_rejected() {
        // A valid data frame followed by a second frame whose declared length
        // runs past the body: with grpc-status: 0 this must fail, not use the
        // valid prefix as complete billing data.
        let mut data = vec![0, 0, 0, 0, 1, 42]; // frame 1: len 1, payload [42]
        data.extend_from_slice(&[0, 0, 0, 0, 8, 1, 2]); // frame 2: len 8, only 2 bytes follow
        let err = match parse_grpc_web_response(&data, Some(0), Utc::now()) {
            Ok(_) => panic!("expected truncated framing to error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("truncated"), "{err}");
    }

    #[test]
    fn trailer_nonzero_status_is_rejected() {
        let trailer = b"grpc-status:16\r\ngrpc-message:unauthenticated";
        let len = trailer.len();
        let mut data = vec![0u8, 0, 0, 0, 1, 0]; // one empty-ish data frame
        data.push(0x80);
        data.extend_from_slice(&[
            (len >> 24) as u8,
            (len >> 16) as u8,
            (len >> 8) as u8,
            len as u8,
        ]);
        data.extend_from_slice(trailer);
        let err = match parse_grpc_web_response(&data, None, Utc::now()) {
            Ok(_) => panic!("expected non-zero trailer status to error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unauthorized"), "{err}");
    }

    #[test]
    fn missing_grpc_status_is_rejected() {
        // A lone data frame with no trailer and no header status is incomplete.
        let data = [0u8, 0, 0, 0, 1, 42];
        let err = match parse_grpc_web_response(&data, None, Utc::now()) {
            Ok(_) => panic!("expected missing gRPC status to error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("missing gRPC status"), "{err}");
    }

    #[test]
    fn scans_percent_and_reset() {
        fn varint(mut v: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    out.push(b | 0x80);
                } else {
                    out.push(b);
                    break;
                }
            }
            out
        }
        fn frame(payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0u8];
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }

        let mut payload = vec![0x0D]; // field 1, wire type 5 (fixed32)
        payload.extend_from_slice(&42.0f32.to_le_bytes());
        payload.push(0x28); // field 5, wire type 0 (varint)
        payload.extend(varint(2_000_000_000)); // future unix seconds

        let billing = parse_grpc_web_response(&frame(&payload), Some(0), Utc::now()).unwrap();
        assert_eq!(billing.used_percent, 42);
        assert!(billing.resets_at.is_some());
    }

    #[test]
    fn zero_use_snapshot_maps_to_zero_percent() {
        // Fresh account: framed, status 0, only a reset varint, no percent field.
        fn frame(payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0u8];
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        let mut payload = vec![0x28]; // field 5, varint
        let mut v = 2_000_000_000u64;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                payload.push(b | 0x80);
            } else {
                payload.push(b);
                break;
            }
        }
        let billing = parse_grpc_web_response(&frame(&payload), Some(0), Utc::now()).unwrap();
        assert_eq!(billing.used_percent, 0);
        assert!(billing.resets_at.is_some());
    }

    #[test]
    fn accepts_raw_protobuf_body() {
        // No gRPC-Web framing at all: a bare protobuf message carrying a percent.
        let mut data = vec![0x0D]; // field 1, wire type 5 (fixed32)
        data.extend_from_slice(&37.0f32.to_le_bytes());
        let billing = parse_grpc_web_response(&data, None, Utc::now()).unwrap();
        assert_eq!(billing.used_percent, 37);
    }

    #[test]
    fn no_billing_signal_errors() {
        // Framed status-0 reply with an empty payload: no percent and no reset,
        // so there is nothing usable and the stale fallback should apply.
        let data = [0u8, 0, 0, 0, 0]; // one zero-length data frame
        let err = match parse_grpc_web_response(&data, Some(0), Utc::now()) {
            Ok(_) => panic!("expected missing billing signal to error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no usable data"), "{err}");
    }

    #[test]
    fn prefers_reset_field_path() {
        fn varint(mut v: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    out.push(b | 0x80);
                } else {
                    out.push(b);
                    break;
                }
            }
            out
        }
        fn frame(payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0u8];
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        // Reset epoch (later) nested at path [1, 5, 1].
        let mut inner = vec![0x08]; // field 1, wire 0 (varint)
        inner.extend(varint(2_000_000_000));
        let mut mid = vec![0x2A]; // field 5, wire 2 (message)
        mid.extend(varint(inner.len() as u64));
        mid.extend_from_slice(&inner);
        let mut payload = vec![0x0A]; // field 1, wire 2 (message)
        payload.extend(varint(mid.len() as u64));
        payload.extend_from_slice(&mid);
        // Unrelated earlier future epoch at top-level field 9.
        payload.push(0x48); // field 9, wire 0 (varint)
        payload.extend(varint(1_800_000_000));

        let billing = parse_grpc_web_response(&frame(&payload), Some(0), Utc::now()).unwrap();
        assert!(
            billing.resets_at.unwrap().starts_with("2033"),
            "should prefer the [1, 5, 1] reset field over an unrelated epoch"
        );
    }

    #[test]
    fn dangling_top_level_field_is_rejected() {
        fn frame(payload: &[u8]) -> Vec<u8> {
            let mut out = vec![0u8];
            out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            out.extend_from_slice(payload);
            out
        }
        // A valid percent fixed32 followed by a truncated varint (continuation
        // bit set, no terminating byte) must fail, not be accepted as billing.
        let mut payload = vec![0x0D]; // field 1, wire 5 (fixed32)
        payload.extend_from_slice(&42.0f32.to_le_bytes());
        payload.push(0x28); // field 5, wire 0 (varint)
        payload.push(0x80); // continuation bit set, no more bytes -> truncated
        let err = match parse_grpc_web_response(&frame(&payload), Some(0), Utc::now()) {
            Ok(_) => panic!("expected a malformed message to error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("malformed"), "{err}");
    }

    #[test]
    fn raw_body_with_error_status_is_rejected() {
        // A bare body carrying grpc-status: 16 must not be accepted as 0% usage.
        let mut data = vec![0x0D];
        data.extend_from_slice(&37.0f32.to_le_bytes());
        let err = match parse_grpc_web_response(&data, Some(16), Utc::now()) {
            Ok(_) => panic!("expected error status to be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unauthorized"), "{err}");
    }
}
