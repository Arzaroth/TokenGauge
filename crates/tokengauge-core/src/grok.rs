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

use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const BILLING_ENDPOINT: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";

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

/// `auth.json` is an object keyed by OIDC scope URL. Prefer the SuperGrok/OIDC
/// entry (`https://auth.x.ai::`), else the first entry carrying a `key`.
fn parse_credentials(text: &str, now: DateTime<Utc>) -> Result<Credentials> {
    let root: Value = serde_json::from_str(text).context("Grok auth.json was invalid")?;
    let map = root
        .as_object()
        .ok_or_else(|| anyhow!("Grok auth.json was invalid"))?;

    let mut selected: Option<&Value> = None;
    for (scope, entry) in map {
        let has_key = entry
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_key {
            continue;
        }
        if scope.starts_with("https://auth.x.ai::") {
            selected = Some(entry);
            break;
        }
        if selected.is_none() || scope.contains("/sign-in") {
            selected = Some(entry);
        }
    }

    let entry = selected.ok_or_else(|| anyhow!("Grok not logged in - run `grok login`"))?;
    let access_token = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Grok not logged in - run `grok login`"))?
        .to_string();

    if let Some(expires) = entry
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        && expires.with_timezone(&Utc) <= now
    {
        return Err(anyhow!("Grok token expired - run `grok login`"));
    }

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

fn parse_grpc_web_response(data: &[u8], now: DateTime<Utc>) -> Result<Billing> {
    let frames = grpc_web_data_frames(data);
    if frames.is_empty() {
        return Err(anyhow!("Grok billing returned no payload"));
    }

    let mut scan = ProtoScan::default();
    for frame in &frames {
        scan.scan_message(frame, &mut Vec::new(), 0);
    }

    let used = scan
        .fixed32
        .iter()
        .filter(|f| {
            f.path.last() == Some(&1) && f.value.is_finite() && f.value >= 0.0 && f.value <= 100.0
        })
        .min_by(|a, b| a.path.len().cmp(&b.path.len()).then(a.order.cmp(&b.order)))
        .map(|f| f.value as f64)
        .ok_or_else(|| anyhow!("Grok billing percent missing"))?;

    let resets_at = scan
        .varints
        .iter()
        .filter_map(|v| {
            (1_700_000_000..=2_100_000_000)
                .contains(v)
                .then(|| Utc.timestamp_opt(*v as i64, 0).single())
                .flatten()
        })
        .filter(|dt| *dt > now)
        .min()
        .map(|dt| dt.to_rfc3339());

    Ok(Billing {
        used_percent: pct_u8(used),
        resets_at,
    })
}

/// Split a gRPC-web body into its data frames, skipping trailer frames (the
/// high bit of the flags byte marks a trailer).
fn grpc_web_data_frames(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut index = 0;
    while index + 5 <= data.len() {
        let flags = data[index];
        let len = ((data[index + 1] as usize) << 24)
            | ((data[index + 2] as usize) << 16)
            | ((data[index + 3] as usize) << 8)
            | (data[index + 4] as usize);
        let start = index + 5;
        let Some(end) = start.checked_add(len).filter(|end| *end <= data.len()) else {
            break;
        };
        if flags & 0x80 == 0 {
            frames.push(data[start..end].to_vec());
        }
        index = end;
    }
    frames
}

#[derive(Default)]
struct ProtoScan {
    fixed32: Vec<Fixed32Field>,
    varints: Vec<u64>,
    order: usize,
}

struct Fixed32Field {
    path: Vec<u64>,
    value: f32,
    order: usize,
}

impl ProtoScan {
    fn scan_message(&mut self, data: &[u8], path: &mut Vec<u64>, depth: usize) {
        if depth > 8 {
            return;
        }
        let mut i = 0;
        while i < data.len() {
            let Some((field, wire, next)) = read_key(data, i) else {
                break;
            };
            i = next;
            path.push(field);
            let advanced = self.scan_field(data, i, path, depth, wire);
            path.pop();
            let Some(next) = advanced else { break };
            i = next;
        }
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
            0 => self.scan_varint(data, i),
            2 => self.scan_length_delimited(data, i, path, depth),
            5 => self.scan_fixed32(data, i, path),
            1 => i.checked_add(8),
            _ => None,
        }
    }

    fn scan_varint(&mut self, data: &[u8], i: usize) -> Option<usize> {
        let (value, next) = read_varint(data, i)?;
        self.varints.push(value);
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
        let end = start.checked_add(len as usize).filter(|end| *end <= data.len())?;
        self.scan_message(&data[start..end], path, depth + 1);
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
    Some((key >> 3, key & 0x07, next))
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

fn to_payload(billing: Billing, login_method: Option<String>, now: DateTime<Utc>) -> ProviderPayload {
    // Grok exposes a single monthly billing cycle (no rolling sub-windows).
    let primary = UsageWindow {
        used_percent: Some(billing.used_percent),
        reset_description: None,
        resets_at: billing.resets_at,
        window_minutes: None,
    };
    ProviderPayload {
        provider: "grok".to_string(),
        version: None,
        source: Some("grok-web".to_string()),
        usage: Some(UsageSnapshot {
            primary: Some(primary),
            secondary: None,
            tertiary: None,
            updated_at: Some(now.to_rfc3339()),
            login_method,
            extra_rate_windows: Vec::new(),
        }),
        credits: None,
        error: None,
        stale: false,
    }
}

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let creds = read_credentials(&auth_path(), now)?;

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

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!("Grok unauthorized - run `grok login`"));
    }
    if !status.is_success() {
        return Err(anyhow!("Grok billing HTTP {}", status.as_u16()));
    }

    // gRPC carries its own status; 16 = unauthenticated.
    let grpc_status = resp
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u16>().ok());
    if let Some(code) = grpc_status {
        if code == 16 {
            return Err(anyhow!("Grok unauthorized - run `grok login`"));
        }
        if code != 0 {
            return Err(anyhow!("Grok billing gRPC status {code}"));
        }
    }

    let bytes = resp.bytes().context("Grok billing read failed")?;
    let billing = parse_grpc_web_response(&bytes, now)?;
    Ok(vec![to_payload(billing, creds.login_method, now)])
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
    fn expired_token_errors() {
        let auth = r#"{"https://auth.x.ai::a": {"key": "t", "expires_at": "2000-01-01T00:00:00Z"}}"#;
        let err = parse_credentials(auth, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("expired"), "{err}");
    }

    #[test]
    fn splits_data_frames_skipping_trailers() {
        // One data frame [1,2], then a trailer frame (flags 0x80) that is dropped.
        let data = [0, 0, 0, 0, 2, 1, 2, 0x80, 0, 0, 0, 1, b'x'];
        assert_eq!(grpc_web_data_frames(&data), vec![vec![1, 2]]);
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

        let billing = parse_grpc_web_response(&frame(&payload), Utc::now()).unwrap();
        assert_eq!(billing.used_percent, 42);
        assert!(billing.resets_at.is_some());
    }
}
