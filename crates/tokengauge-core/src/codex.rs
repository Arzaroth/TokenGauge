//! Native Codex OAuth usage fetcher.
//!
//! Unlike Claude, Codex refreshes its own token and writes it back to
//! `$CODEX_HOME/auth.json`. The refresh token rotates, so two processes
//! refreshing at once would revoke one and log the user out - hence the
//! cross-process `try_lock` + double-check before refreshing.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Credits, ExtraRateWindow, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8,
    slug,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const REFRESH_AFTER: ChronoDuration = ChronoDuration::days(8);

// ---------------------------------------------------------------------------
// Credentials + refresh + write-back
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    api_key: Option<String>,
    tokens: Option<Tokens>,
    last_refresh: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
struct Tokens {
    #[serde(alias = "accessToken")]
    access_token: String,
    #[serde(
        alias = "refreshToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
}

fn codex_home() -> PathBuf {
    match std::env::var("CODEX_HOME") {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
        _ => dirs::home_dir().unwrap_or_default().join(".codex"),
    }
}

pub(crate) fn auth_path() -> PathBuf {
    codex_home().join("auth.json")
}

fn read_auth(path: &Path) -> Result<AuthFile> {
    let data =
        std::fs::read_to_string(path).map_err(|_| anyhow!("Codex not logged in - run `codex`"))?;
    serde_json::from_str(&data).context("auth.json was invalid")
}

/// Codex has no expiry field; upstream refreshes purely on `last_refresh` age
/// (the access token JWT lives 10 days, so the 8-day rule keeps a 2-day margin).
fn needs_refresh(last_refresh: Option<&str>, now: DateTime<Utc>) -> bool {
    match last_refresh.and_then(|s| DateTime::parse_from_rfc3339(s).ok()) {
        Some(ts) => now.signed_duration_since(ts.with_timezone(&Utc)) > REFRESH_AFTER,
        None => true,
    }
}

fn refresh(
    client: &reqwest::blocking::Client,
    tokens: &Tokens,
    refresh_token: &str,
) -> Result<Tokens> {
    let body = json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "scope": "openid profile email",
    });
    let resp = client
        .post(REFRESH_URL)
        .json(&body)
        .send()
        .context("Codex token refresh failed")?;
    let status = resp.status();
    if !status.is_success() {
        // Check status before decoding: an error body may be HTML/text, and the
        // specific code (invalid_grant / refresh_token_expired / reused /
        // invalidated) all mean the same thing to us anyway: re-auth.
        return Err(anyhow!("Codex token refresh failed - run `codex`"));
    }
    let val: Value = resp.json().context("Codex refresh response was invalid")?;
    map_refresh_response(&val, tokens)
}

/// Map a (successful) refresh response onto new tokens, carrying old values
/// forward when a field is missing or empty. Split out from `refresh` so the
/// token-validation rules are unit-testable without a live HTTP call.
fn map_refresh_response(val: &Value, old: &Tokens) -> Result<Tokens> {
    let pick = |key: &str, prev: &Option<String>| -> Option<String> {
        val.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty()) // an empty rotated token would brick the next refresh
            .map(str::to_string)
            .or_else(|| prev.clone())
    };
    Ok(Tokens {
        // A 200 without a fresh access_token must not silently reuse the old one
        // and advance last_refresh (which would block refresh for 8 more days).
        access_token: val
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Codex refresh response omitted access_token"))?,
        refresh_token: pick("refresh_token", &old.refresh_token),
        id_token: pick("id_token", &old.id_token),
        account_id: old.account_id.clone(),
    })
}

/// Write refreshed tokens back, preserving unknown fields, via an atomic
/// `create_new` + `rename` at mode 0600.
///
/// `root` is the JSON object read from `auth.json` **before** the network
/// refresh, so the only work left after the (irreversible) token rotation is a
/// serialize + write - there is no post-refresh reread that could fail and lose
/// the rotated token. If the final `rename` fails, the staged 0600 temp file is
/// deliberately kept: it holds the new token for recovery.
fn write_auth(path: &Path, mut root: Value, tokens: &Tokens, now: DateTime<Utc>) -> Result<()> {
    if !root.is_object() {
        return Err(anyhow!("auth.json is not a JSON object"));
    }
    // Merge the known token fields into any existing `tokens` object so opaque
    // keys codex may store there survive the rewrite.
    let new_tokens = serde_json::to_value(tokens)?;
    match root.get_mut("tokens").and_then(Value::as_object_mut) {
        Some(existing) => {
            if let Some(obj) = new_tokens.as_object() {
                for (k, v) in obj {
                    existing.insert(k.clone(), v.clone());
                }
            }
        }
        None => root["tokens"] = new_tokens,
    }
    root["last_refresh"] = json!(now.to_rfc3339());

    let buf = serde_json::to_string_pretty(&root)? + "\n";
    let tmp = path.with_file_name(format!("auth.json.tmp.{}", std::process::id()));

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // Stage the full new file first. `create_new` means an existing temp is a
    // kept recovery copy from a prior rename failure - do not clobber it.
    let mut f = match opts.open(&tmp) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(anyhow!(
                "a recovery temp already exists at {}; move it aside before retrying",
                tmp.display()
            ));
        }
        Err(e) => return Err(anyhow::Error::from(e).context("failed to stage auth.json")),
    };
    // We own the temp now; clean it up only if writing it fails.
    if let Err(e) = f
        .write_all(buf.as_bytes())
        .and_then(|_| f.sync_all())
        .context("failed to write auth.json")
    {
        drop(f);
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    drop(f);
    // Commit. If the rename fails, keep the staged copy (it holds the new token)
    // and point at it so the rotation isn't silently lost.
    std::fs::rename(&tmp, path).map_err(|e| {
        anyhow::Error::from(e).context(format!(
            "failed to replace auth.json; rotated token staged at {}",
            tmp.display()
        ))
    })?;
    // Best-effort: fsync the parent directory so the rename itself is durable
    // across a crash (the file's own fsync doesn't cover the directory entry).
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn api_key_tokens(key: String) -> Tokens {
    Tokens {
        access_token: key,
        refresh_token: None,
        id_token: None,
        account_id: None,
    }
}

/// Read the current token, refreshing (behind a cross-process lock) when the
/// 8-day age threshold is crossed.
fn ensure_access_token(timeout: Duration) -> Result<Tokens> {
    let home = codex_home();
    let path = home.join("auth.json");
    let auth = read_auth(&path)?;

    // Prefer OAuth tokens: they carry account_id and the auth shape wham/usage
    // expects. Only fall back to OPENAI_API_KEY when there are no tokens.
    let Some(tokens) = auth.tokens else {
        return auth
            .api_key
            .filter(|k| !k.is_empty())
            .map(api_key_tokens)
            .ok_or_else(|| anyhow!("Codex not logged in - run `codex`"));
    };
    if !needs_refresh(auth.last_refresh.as_deref(), Utc::now()) {
        return Ok(tokens);
    }

    // ponytail: try_lock, not lock. The 8d refresh rule leaves ~2d of JWT
    // margin, so the loser of the race just uses its current token. std
    // releases the lock on process death, so no TTL is needed.
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("auth.json.lock"))
        .context("failed to open codex refresh lock")?;
    match lock.try_lock() {
        Ok(()) => {}
        // Contention only: someone else is refreshing; ours is still valid.
        Err(std::fs::TryLockError::WouldBlock) => return Ok(tokens),
        // A real lock I/O error must surface, not silently serve the old token.
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow::Error::from(e).context("failed to lock auth.json"));
        }
    }

    // Double-check against the file now that we hold the lock, capturing the raw
    // JSON so the write-back merge base is fixed *before* the network refresh.
    let raw =
        std::fs::read_to_string(&path).map_err(|_| anyhow!("Codex not logged in - run `codex`"))?;
    let fresh: AuthFile = serde_json::from_str(&raw).context("auth.json was invalid")?;
    let Some(fresh_tokens) = fresh.tokens else {
        return fresh
            .api_key
            .filter(|k| !k.is_empty())
            .map(api_key_tokens)
            .ok_or_else(|| anyhow!("Codex not logged in - run `codex`"));
    };
    if !needs_refresh(fresh.last_refresh.as_deref(), Utc::now()) {
        return Ok(fresh_tokens); // the winner already refreshed
    }
    let Some(refresh_token) = fresh_tokens.refresh_token.clone().filter(|t| !t.is_empty()) else {
        return Ok(fresh_tokens); // nothing to refresh with
    };
    let root: Value = serde_json::from_str(&raw).context("auth.json was invalid")?;

    let client = http_client(timeout)?;
    let new = refresh(&client, &fresh_tokens, &refresh_token)?;
    write_auth(&path, root, &new, Utc::now())?;
    Ok(new)
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<RateLimit>,
    #[serde(default)]
    additional_rate_limits: Value,
    credits: Option<CreditsWire>,
    individual_limit: Option<IndividualLimit>,
}

#[derive(Deserialize)]
struct RateLimit {
    #[serde(default, deserialize_with = "lenient_win")]
    primary_window: Option<Win>,
    #[serde(default, deserialize_with = "lenient_win")]
    secondary_window: Option<Win>,
    individual_limit: Option<IndividualLimit>,
}

/// All three fields are required for a window to count; a window that is present
/// but has a malformed field is treated as absent (matches upstream) rather than
/// failing the whole response - see `lenient_win`.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq)]
struct Win {
    used_percent: i64,
    reset_at: i64,
    limit_window_seconds: i64,
}

/// Deserialize an optional window, swallowing a malformed object into `None` so
/// one bad field doesn't fail the entire usage response.
fn lenient_win<'de, D>(d: D) -> std::result::Result<Option<Win>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = Option::<Value>::deserialize(d)?;
    Ok(v.filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value(v).ok()))
}

#[derive(Deserialize)]
struct CreditsWire {
    balance: Option<Value>,
}

#[derive(Deserialize)]
struct IndividualLimit {
    limit: Option<Value>,
    used: Option<Value>,
    #[serde(alias = "remainingPercent")]
    remaining_percent: Option<Value>,
    #[serde(alias = "resetsAt")]
    resets_at: Option<Value>,
}

#[derive(Deserialize)]
struct AddLimit {
    limit_name: Option<String>,
    metered_feature: Option<String>,
    rate_limit: Option<AddRateLimit>,
}

#[derive(Deserialize)]
struct AddRateLimit {
    #[serde(default, deserialize_with = "lenient_win")]
    primary_window: Option<Win>,
    #[serde(default, deserialize_with = "lenient_win")]
    secondary_window: Option<Win>,
}

// ---------------------------------------------------------------------------
// Pure mapping
// ---------------------------------------------------------------------------

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        // Reject NaN/inf (string forms like "NaN"/"inf" parse successfully) so a
        // malformed value can't masquerade as live data past the stale fallback.
        .filter(|v| v.is_finite())
}

fn as_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn epoch_to_rfc3339(secs: i64) -> Option<String> {
    DateTime::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

#[derive(PartialEq)]
enum Role {
    Session,
    Weekly,
    Unknown,
}

fn role(w: &Win) -> Role {
    match w.limit_window_seconds / 60 {
        300 => Role::Session,
        10080 => Role::Weekly,
        _ => Role::Unknown,
    }
}

/// Assign windows to (primary, secondary) slots by their duration. A weekly
/// window in the primary slot is swapped down; a lone weekly window moves to
/// secondary.
fn normalize(primary: Option<Win>, secondary: Option<Win>) -> (Option<Win>, Option<Win>) {
    match (primary, secondary) {
        (Some(p), Some(s)) => {
            if role(&p) == Role::Weekly && matches!(role(&s), Role::Session | Role::Unknown) {
                (Some(s), Some(p))
            } else {
                (Some(p), Some(s))
            }
        }
        (Some(w), None) | (None, Some(w)) => {
            if role(&w) == Role::Weekly {
                (None, Some(w))
            } else {
                (Some(w), None)
            }
        }
        (None, None) => (None, None),
    }
}

/// Full-precision window, `resets_at` unconditional (matches upstream).
fn win_to_usage(w: Win) -> UsageWindow {
    UsageWindow {
        used_percent: Some(pct_u8(w.used_percent as f64)),
        reset_description: None,
        resets_at: epoch_to_rfc3339(w.reset_at),
        window_minutes: Some((w.limit_window_seconds / 60).max(0) as u32),
    }
}

/// Named extra window, with `resets_at`/`window_minutes` guarded to positive.
fn add_usage(w: Win) -> UsageWindow {
    UsageWindow {
        used_percent: Some(pct_u8(w.used_percent as f64)),
        reset_description: None,
        resets_at: (w.reset_at > 0)
            .then(|| epoch_to_rfc3339(w.reset_at))
            .flatten(),
        window_minutes: (w.limit_window_seconds > 0)
            .then_some((w.limit_window_seconds / 60) as u32),
    }
}

fn individual_to_window(il: &IndividualLimit) -> Option<UsageWindow> {
    let limit = il.limit.as_ref().and_then(as_f64).filter(|&l| l > 0.0)?;
    // Require an actual measurement (remaining_percent or used); don't fabricate
    // a live 0% from a limit alone, which would suppress the stale fallback. An
    // out-of-range remaining_percent is garbage - fall back to used/limit.
    let used_pct = match il
        .remaining_percent
        .as_ref()
        .and_then(as_f64)
        .filter(|rp| (0.0..=100.0).contains(rp))
    {
        Some(rp) => 100.0 - rp,
        None => il
            .used
            .as_ref()
            .and_then(as_f64)
            .map(|u| u / limit * 100.0)?,
    };
    let resets_at = il
        .resets_at
        .as_ref()
        .and_then(as_i64)
        .filter(|&s| s > 0)
        .and_then(epoch_to_rfc3339);
    Some(UsageWindow {
        used_percent: Some(pct_u8(used_pct)),
        reset_description: None,
        resets_at,
        window_minutes: None,
    })
}

/// (id, title) for a Spark window: prefer the window's own duration, else fall
/// back to its position (primary=5-hour, secondary=weekly).
fn spark_kind(minutes: i64, positional_weekly: bool) -> (&'static str, &'static str) {
    if minutes > 0 && minutes <= 360 {
        ("codex-spark", "Codex Spark 5-hour")
    } else if minutes >= 8640 || positional_weekly {
        ("codex-spark-weekly", "Codex Spark Weekly")
    } else {
        ("codex-spark", "Codex Spark 5-hour")
    }
}

fn push_unique(out: &mut Vec<ExtraRateWindow>, id: String, title: String, w: Win) {
    if out.iter().any(|e| e.id.as_deref() == Some(id.as_str())) {
        return;
    }
    out.push(ExtraRateWindow {
        id: Some(id),
        title: Some(title),
        window: Some(add_usage(w)),
    });
}

fn extra_windows(adds: &Value) -> Vec<ExtraRateWindow> {
    let Some(arr) = adds.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for el in arr {
        // Lossy: a malformed element is dropped, siblings survive.
        let Ok(a) = serde_json::from_value::<AddLimit>(el.clone()) else {
            continue;
        };
        let name = a.limit_name.as_deref().or(a.metered_feature.as_deref());
        let is_spark = name
            .map(|n| n.to_lowercase().contains("spark"))
            .unwrap_or(false);
        let Some(rl) = a.rate_limit else { continue };

        if is_spark {
            if let Some(w) = rl.primary_window {
                let (id, title) = spark_kind(w.limit_window_seconds / 60, false);
                push_unique(&mut out, id.to_string(), title.to_string(), w);
            }
            if let Some(w) = rl.secondary_window {
                let (id, title) = spark_kind(w.limit_window_seconds / 60, true);
                push_unique(&mut out, id.to_string(), title.to_string(), w);
            }
        } else {
            let Some(w) = rl.primary_window.or(rl.secondary_window) else {
                continue;
            };
            let slug_source = a.metered_feature.as_deref().or(a.limit_name.as_deref());
            let Some(slug_source) = slug_source else {
                continue;
            };
            let id = format!("codex-{}", slug(slug_source));
            let title = a
                .limit_name
                .or(a.metered_feature)
                .unwrap_or_else(|| "Codex extra limit".to_string());
            push_unique(&mut out, id, title, w);
        }
    }
    out
}

fn to_payload(resp: UsageResponse, now: DateTime<Utc>) -> Result<ProviderPayload> {
    let p_raw = resp.rate_limit.as_ref().and_then(|r| r.primary_window);
    let s_raw = resp.rate_limit.as_ref().and_then(|r| r.secondary_window);
    let (np, ns) = normalize(p_raw, s_raw);
    let mut primary = np.map(win_to_usage);
    let secondary = ns.map(win_to_usage);

    // Synthesize a primary window from individual_limit when there is no
    // rate-limit primary (enterprise/credit plans). The top-level limit is
    // preferred, but fall back to the nested one when it yields no usable window.
    if primary.is_none() {
        primary = resp
            .individual_limit
            .as_ref()
            .and_then(individual_to_window)
            .or_else(|| {
                resp.rate_limit
                    .as_ref()
                    .and_then(|r| r.individual_limit.as_ref())
                    .and_then(individual_to_window)
            });
    }

    let credits = resp
        .credits
        .as_ref()
        .and_then(|c| c.balance.as_ref())
        .and_then(as_f64)
        .map(|b| Credits { remaining: Some(b) });

    let extra_rate_windows = extra_windows(&resp.additional_rate_limits);

    if primary.is_none()
        && secondary.is_none()
        && credits.is_none()
        && extra_rate_windows.is_empty()
    {
        return Err(anyhow!("Codex returned no usage windows"));
    }

    Ok(ProviderPayload {
        provider: "codex".to_string(),
        version: None,
        source: Some("oauth".to_string()),
        usage: Some(UsageSnapshot {
            primary,
            secondary,
            tertiary: None,
            updated_at: Some(now.to_rfc3339()),
            login_method: resp.plan_type,
            extra_rate_windows,
        }),
        credits,
        error: None,
        stale: false,
    })
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let tokens = ensure_access_token(timeout)?;

    let client = http_client(timeout)?;
    let mut req = client
        .get(USAGE_URL)
        .header("authorization", format!("Bearer {}", tokens.access_token))
        .header("user-agent", "CodexBar")
        .header("accept", "application/json");
    if let Some(account) = tokens.account_id.as_deref().filter(|a| !a.is_empty()) {
        req = req.header("chatgpt-account-id", account);
    }
    let resp = req.send().context("Codex usage request failed")?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(anyhow!("Codex unauthorized - run `codex` to log in"));
    }
    if !status.is_success() {
        return Err(anyhow!("Codex usage HTTP {}", status.as_u16()));
    }

    let body: UsageResponse = resp.json().context("Codex usage JSON was invalid")?;
    Ok(vec![to_payload(body, now)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(pct: i64, secs: i64) -> Win {
        Win {
            used_percent: pct,
            reset_at: 1_800_000_000,
            limit_window_seconds: secs,
        }
    }

    #[test]
    fn normalize_truth_table() {
        let session = win(1, 300 * 60);
        let weekly = win(2, 10080 * 60);
        let monthly = win(3, 43200 * 60); // unknown role

        // Live sample: a lone monthly (unknown) window stays primary.
        assert_eq!(normalize(Some(monthly), None), (Some(monthly), None));
        // (weekly, session) swaps.
        assert_eq!(
            normalize(Some(weekly), Some(session)),
            (Some(session), Some(weekly))
        );
        // (weekly, unknown) swaps.
        assert_eq!(
            normalize(Some(weekly), Some(monthly)),
            (Some(monthly), Some(weekly))
        );
        // Lone weekly moves to secondary.
        assert_eq!(normalize(Some(weekly), None), (None, Some(weekly)));
        // Lone session stays primary.
        assert_eq!(normalize(Some(session), None), (Some(session), None));
        // Correctly ordered pair is untouched.
        assert_eq!(
            normalize(Some(session), Some(weekly)),
            (Some(session), Some(weekly))
        );
    }

    fn old_tokens() -> Tokens {
        Tokens {
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            id_token: Some("old-id".into()),
            account_id: Some("acc".into()),
        }
    }

    #[test]
    fn map_refresh_response_validates_tokens() {
        let old = old_tokens();

        // Full rotation: new values win.
        let t = map_refresh_response(
            &json!({"access_token":"a2","refresh_token":"r2","id_token":"i2"}),
            &old,
        )
        .unwrap();
        assert_eq!(t.access_token, "a2");
        assert_eq!(t.refresh_token.as_deref(), Some("r2"));
        assert_eq!(t.id_token.as_deref(), Some("i2"));
        assert_eq!(t.account_id.as_deref(), Some("acc")); // always carried over

        // Missing access_token -> error (must not reuse the old one).
        assert!(map_refresh_response(&json!({"refresh_token":"r2"}), &old).is_err());
        // Empty access_token -> error.
        assert!(map_refresh_response(&json!({"access_token":""}), &old).is_err());

        // Missing/empty refresh_token -> keep the old rotating token.
        let t = map_refresh_response(&json!({"access_token":"a2"}), &old).unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("old-refresh"));
        let t =
            map_refresh_response(&json!({"access_token":"a2","refresh_token":""}), &old).unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("old-refresh"));
    }

    #[test]
    fn needs_refresh_by_age() {
        let now = Utc::now();
        let ago = |d: i64| (now - ChronoDuration::days(d)).to_rfc3339();
        assert!(!needs_refresh(Some(&ago(7)), now));
        assert!(!needs_refresh(Some(&ago(8)), now)); // exactly 8d, not yet over
        assert!(needs_refresh(Some(&ago(9)), now));
        assert!(needs_refresh(None, now));
        assert!(needs_refresh(Some("not-a-date"), now));
    }

    #[test]
    fn as_f64_rejects_non_finite() {
        assert_eq!(as_f64(&json!("NaN")), None);
        assert_eq!(as_f64(&json!("inf")), None);
        assert_eq!(as_f64(&json!("-inf")), None);
        assert_eq!(as_f64(&json!("7.5")), Some(7.5));
        assert_eq!(as_f64(&json!(3)), Some(3.0));
    }

    #[test]
    fn malformed_window_is_treated_as_absent() {
        // primary_window has a bad field (string used_percent) -> dropped, not a
        // whole-response failure; the valid secondary still maps.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"pro","rate_limit":{
                "primary_window":{"used_percent":"oops","reset_at":1,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":50,"reset_at":1,"limit_window_seconds":604800}}}"#,
        )
        .expect("malformed window must not fail the whole response");
        let usage = to_payload(body, Utc::now()).unwrap().usage.unwrap();
        // The lone valid window is weekly-shaped, so it lands in secondary.
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, Some(50));
    }

    #[test]
    fn maps_live_codex_sample() {
        // primary is a 43200-minute (monthly) window -> unknown role, stays primary.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"free","rate_limit":{
                "primary_window":{"used_percent":6,"reset_at":1786646643,"limit_window_seconds":2592000},
                "secondary_window":null}}"#,
        )
        .unwrap();
        let payload = to_payload(body, Utc::now()).unwrap();
        let usage = payload.usage.unwrap();
        assert_eq!(usage.primary.as_ref().unwrap().used_percent, Some(6));
        assert_eq!(usage.primary.as_ref().unwrap().window_minutes, Some(43200));
        assert!(usage.secondary.is_none());
        assert_eq!(usage.login_method.as_deref(), Some("free"));
    }

    #[test]
    fn individual_limit_string_used_synthesizes_primary() {
        // Enterprise: both windows null, only individual_limit; `used` is a string.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"enterprise","rate_limit":{
                "primary_window":null,"secondary_window":null,
                "individual_limit":{"limit":100000,"used":"7761",
                    "remaining_percent":92.239,"resets_at":1782864000}}}"#,
        )
        .unwrap();
        let payload = to_payload(body, Utc::now()).unwrap();
        let primary = payload.usage.unwrap().primary.unwrap();
        // 100 - 92.239 = 7.761 -> rounds to 8.
        assert_eq!(primary.used_percent, Some(8));
        assert!(primary.resets_at.is_some());
    }

    #[test]
    fn individual_limit_falls_back_to_nested_when_top_level_unusable() {
        // Top-level individual_limit has no measurement (limit only); the nested
        // one does -> primary is synthesized from the nested limit.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"enterprise",
                "individual_limit":{"limit":100000},
                "rate_limit":{"primary_window":null,"secondary_window":null,
                    "individual_limit":{"limit":100000,"remaining_percent":40.0}}}"#,
        )
        .unwrap();
        let primary = to_payload(body, Utc::now())
            .unwrap()
            .usage
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(primary.used_percent, Some(60)); // 100 - 40
    }

    #[test]
    fn individual_limit_out_of_range_percent_falls_back_to_used() {
        // remaining_percent=200 is garbage -> fall back to used/limit (25%).
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"enterprise","rate_limit":{
                "primary_window":null,"secondary_window":null,
                "individual_limit":{"limit":1000,"used":250,"remaining_percent":200}}}"#,
        )
        .unwrap();
        let primary = to_payload(body, Utc::now())
            .unwrap()
            .usage
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(primary.used_percent, Some(25));
    }

    #[test]
    fn individual_limit_without_measurement_yields_no_window() {
        // limit alone (no used / remaining_percent) must not become a live 0%;
        // with no windows and no credits the whole response is then an error.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"enterprise","rate_limit":{
                "primary_window":null,"secondary_window":null,
                "individual_limit":{"limit":100000}}}"#,
        )
        .unwrap();
        assert!(to_payload(body, Utc::now()).is_err());
    }

    #[test]
    fn additional_rate_limits_lossy_and_spark() {
        let adds = json!([
            "garbage-not-an-object",
            {"limit_name":"GPT-5.3-Codex-Spark","metered_feature":"gpt_5_3_codex_spark",
             "rate_limit":{
                "primary_window":{"used_percent":30,"reset_at":1766948068,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":100,"reset_at":1767407914,"limit_window_seconds":604800}}},
            {"limit_name":"Some Feature","metered_feature":"some_feature",
             "rate_limit":{"primary_window":{"used_percent":5,"reset_at":0,"limit_window_seconds":0}}}
        ]);
        let out = extra_windows(&adds);
        let ids: Vec<&str> = out.iter().map(|w| w.id.as_deref().unwrap()).collect();
        // garbage dropped; spark 5h + weekly by duration; non-spark by slug.
        assert_eq!(
            ids,
            vec!["codex-spark", "codex-spark-weekly", "codex-some-feature"]
        );
        // reset_at 0 / secs 0 on the non-spark window is guarded to None.
        let non_spark = out.last().unwrap().window.as_ref().unwrap();
        assert!(non_spark.resets_at.is_none());
        assert!(non_spark.window_minutes.is_none());
    }

    #[test]
    fn no_windows_no_credits_errors() {
        let body: UsageResponse =
            serde_json::from_str(r#"{"plan_type":"free","rate_limit":{}}"#).unwrap();
        assert!(to_payload(body, Utc::now()).is_err());
    }

    #[test]
    fn write_auth_preserves_unknown_top_level() {
        let dir = std::env::temp_dir().join(format!("tg-codex-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"OPENAI_API_KEY":null,"custom_field":"keep-me",
                "tokens":{"access_token":"old","refresh_token":"oldr","opaque":"keep-me-too"},
                "last_refresh":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();

        let new = Tokens {
            access_token: "new".to_string(),
            refresh_token: Some("newr".to_string()),
            id_token: None,
            account_id: Some("acc-1".to_string()),
        };
        // Merge base is read before write_auth (as ensure_access_token does).
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        write_auth(&path, root, &new, Utc::now()).unwrap();

        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["custom_field"], "keep-me");
        assert_eq!(root["tokens"]["access_token"], "new");
        assert_eq!(root["tokens"]["account_id"], "acc-1");
        // Opaque keys inside `tokens` survive the rewrite.
        assert_eq!(root["tokens"]["opaque"], "keep-me-too");
        assert_ne!(root["last_refresh"], "2026-01-01T00:00:00Z");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_auth_failure_does_not_destroy_original() {
        // Failure injection: target dir is missing, so staging the temp fails.
        // write_auth must return Err without leaving a stray temp behind, and
        // (the caller not having replaced anything) the original is untouched.
        let dir = std::env::temp_dir().join(format!("tg-codex-fail-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("auth.json"); // parent dir does not exist
        let new = Tokens {
            access_token: "new".to_string(),
            refresh_token: Some("newr".to_string()),
            id_token: None,
            account_id: None,
        };
        let err = write_auth(&path, json!({"tokens": {}}), &new, Utc::now());
        assert!(err.is_err());
        assert!(!path.exists());
        let tmp = path.with_file_name(format!("auth.json.tmp.{}", std::process::id()));
        assert!(!tmp.exists(), "staging temp must be cleaned up on failure");
    }
}
