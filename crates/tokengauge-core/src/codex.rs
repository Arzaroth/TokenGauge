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

use crate::provider::{check_status, epoch_to_rfc3339, json_int, json_num, trimmed};
use crate::{
    Credits, ExtraRateWindow, ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8,
    slug,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const WHOAMI_URL: &str = "https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami";
const REFRESH_AFTER: ChronoDuration = ChronoDuration::days(8);

// ---------------------------------------------------------------------------
// Credentials + refresh + write-back
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    api_key: Option<String>,
    /// A personal access token, written by `codex login --with-pat` and by the
    /// managed-workspace flows. It never rotates, so none of the refresh
    /// machinery below applies to it.
    #[serde(default, alias = "personalAccessToken")]
    personal_access_token: Option<String>,
    tokens: Option<Tokens>,
    last_refresh: Option<String>,
}

/// What the fetch ended up authenticating with. `source` reaches the snapshot,
/// so a frontend can say which of the three shapes answered.
struct Credential {
    tokens: Tokens,
    source: &'static str,
    /// A PAT's plan, from whoami. `wham/usage`'s own `plan_type` wins when it
    /// reports one.
    plan_hint: Option<String>,
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

#[derive(Deserialize, Default)]
struct Whoami {
    #[serde(default, alias = "chatgptAccountId")]
    chatgpt_account_id: Option<String>,
    #[serde(default, alias = "chatgptPlanType")]
    chatgpt_plan_type: Option<String>,
}

/// Resolve which account a personal access token speaks for. Best-effort:
/// `wham/usage` answers without the account header as well, and it is the call
/// whose 401 tells the user their token is no good.
fn whoami(client: &reqwest::blocking::Client, token: &str) -> Option<Whoami> {
    let resp = client
        .get(WHOAMI_URL)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .send()
        .ok()?;
    resp.status().is_success().then(|| resp.json().ok())?
}

/// `auth.json` with no OAuth tokens: a personal access token, else the API key.
/// A PAT is preferred because it is what `wham/usage` accepts; `OPENAI_API_KEY`
/// is a platform key that usually cannot read subscription usage at all, and is
/// only kept as the last resort it always was.
fn non_oauth_credential(auth: AuthFile, timeout: Duration) -> Result<Credential> {
    pat_or_api_key(auth, |token| {
        http_client(timeout)
            .ok()
            .and_then(|client| whoami(&client, token))
            .unwrap_or_default()
    })
}

/// The credential rules on their own, with identity resolution injected so they
/// are testable without a network call.
fn pat_or_api_key(auth: AuthFile, resolve: impl FnOnce(&str) -> Whoami) -> Result<Credential> {
    if let Some(pat) = trimmed(auth.personal_access_token) {
        let who = resolve(&pat);
        return Ok(Credential {
            tokens: Tokens {
                access_token: pat,
                refresh_token: None,
                id_token: None,
                account_id: trimmed(who.chatgpt_account_id),
            },
            source: "pat",
            plan_hint: trimmed(who.chatgpt_plan_type),
        });
    }
    trimmed(auth.api_key)
        .map(|key| Credential {
            tokens: api_key_tokens(key),
            source: "api-key",
            plan_hint: None,
        })
        .ok_or_else(|| anyhow!("Codex not logged in - run `codex`"))
}

fn oauth(tokens: Tokens) -> Credential {
    Credential {
        tokens,
        source: "oauth",
        plan_hint: None,
    }
}

/// Read the current token, refreshing (behind a cross-process lock) when the
/// 8-day age threshold is crossed.
fn ensure_access_token(timeout: Duration) -> Result<Credential> {
    let home = codex_home();
    let path = home.join("auth.json");
    let mut auth = read_auth(&path)?;

    // Prefer OAuth tokens: they carry account_id and the auth shape wham/usage
    // expects. Only fall back to a PAT or OPENAI_API_KEY when there are none.
    let Some(tokens) = auth.tokens.take() else {
        return non_oauth_credential(auth, timeout);
    };
    if !needs_refresh(auth.last_refresh.as_deref(), Utc::now()) {
        return Ok(oauth(tokens));
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
        Err(std::fs::TryLockError::WouldBlock) => return Ok(oauth(tokens)),
        // A real lock I/O error must surface, not silently serve the old token.
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(anyhow::Error::from(e).context("failed to lock auth.json"));
        }
    }

    // Double-check against the file now that we hold the lock, capturing the raw
    // JSON so the write-back merge base is fixed *before* the network refresh.
    let raw =
        std::fs::read_to_string(&path).map_err(|_| anyhow!("Codex not logged in - run `codex`"))?;
    let mut fresh: AuthFile = serde_json::from_str(&raw).context("auth.json was invalid")?;
    let Some(fresh_tokens) = fresh.tokens.take() else {
        return non_oauth_credential(fresh, timeout);
    };
    if !needs_refresh(fresh.last_refresh.as_deref(), Utc::now()) {
        return Ok(oauth(fresh_tokens)); // the winner already refreshed
    }
    let Some(refresh_token) = fresh_tokens.refresh_token.clone().filter(|t| !t.is_empty()) else {
        return Ok(oauth(fresh_tokens)); // nothing to refresh with
    };
    let root: Value = serde_json::from_str(&raw).context("auth.json was invalid")?;

    let client = http_client(timeout)?;
    let new = refresh(&client, &fresh_tokens, &refresh_token)?;
    write_auth(&path, root, &new, Utc::now())?;
    Ok(oauth(new))
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
    /// Team, enterprise and EDU workspaces report the administrator-defined
    /// monthly credit pool here instead of at the root.
    #[serde(default, alias = "spendControl")]
    spend_control: Option<SpendControl>,
}

#[derive(Deserialize)]
struct SpendControl {
    #[serde(default, alias = "individualLimit")]
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

#[derive(PartialEq)]
enum Role {
    Session,
    Weekly,
    Monthly,
    Unknown,
}

/// Classify a window by its duration. Anything from four weeks up is monthly:
/// a 30-day window in the primary slot is not the 5-hour session gauge, and
/// labelling it "Session" made a month of headroom read as an exhausted day.
const MONTHLY_MIN_MINUTES: i64 = 40320;

fn role(w: &Win) -> Role {
    match w.limit_window_seconds / 60 {
        300 => Role::Session,
        10080 => Role::Weekly,
        m if m >= MONTHLY_MIN_MINUTES => Role::Monthly,
        _ => Role::Unknown,
    }
}

/// Pull monthly windows out of the two rate-limit slots. They keep their own
/// labelled row instead of taking the Session or Weekly one.
fn split_monthly(
    primary: Option<Win>,
    secondary: Option<Win>,
) -> (Vec<Win>, Option<Win>, Option<Win>) {
    let mut monthly = Vec::new();
    let mut keep = |w: Option<Win>| -> Option<Win> {
        match w {
            Some(w) if role(&w) == Role::Monthly => {
                monthly.push(w);
                None
            }
            other => other,
        }
    };
    let primary = keep(primary);
    let secondary = keep(secondary);
    (monthly, primary, secondary)
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
        resets_at: epoch_to_rfc3339(w.reset_at as f64),
        window_minutes: Some((w.limit_window_seconds / 60).max(0) as u32),
    }
}

/// Named extra window, with `resets_at`/`window_minutes` guarded to positive.
fn add_usage(w: Win) -> UsageWindow {
    UsageWindow {
        used_percent: Some(pct_u8(w.used_percent as f64)),
        reset_description: None,
        resets_at: (w.reset_at > 0)
            .then(|| epoch_to_rfc3339(w.reset_at as f64))
            .flatten(),
        window_minutes: (w.limit_window_seconds > 0)
            .then_some((w.limit_window_seconds / 60) as u32),
    }
}

fn individual_to_window(il: &IndividualLimit) -> Option<UsageWindow> {
    let limit = il.limit.as_ref().and_then(json_num).filter(|&l| l > 0.0)?;
    // Require an actual measurement (remaining_percent or used); don't fabricate
    // a live 0% from a limit alone, which would suppress the stale fallback. An
    // out-of-range remaining_percent is garbage - fall back to used/limit.
    let used_pct = match il
        .remaining_percent
        .as_ref()
        .and_then(json_num)
        .filter(|rp| (0.0..=100.0).contains(rp))
    {
        Some(rp) => 100.0 - rp,
        None => il
            .used
            .as_ref()
            .and_then(json_num)
            .map(|u| u / limit * 100.0)?,
    };
    let resets_at = il
        .resets_at
        .as_ref()
        .and_then(json_int)
        .filter(|&s| s > 0)
        .and_then(|s| epoch_to_rfc3339(s as f64));
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
        placeholder: false,
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

fn to_payload(
    resp: UsageResponse,
    now: DateTime<Utc>,
    source: &str,
    plan_hint: Option<String>,
) -> Result<ProviderPayload> {
    let p_raw = resp.rate_limit.as_ref().and_then(|r| r.primary_window);
    let s_raw = resp.rate_limit.as_ref().and_then(|r| r.secondary_window);
    let (monthly, p_raw, s_raw) = split_monthly(p_raw, s_raw);
    let (np, ns) = normalize(p_raw, s_raw);
    let mut primary = np.map(win_to_usage);
    let secondary = ns.map(win_to_usage);

    // Synthesize a primary window from individual_limit when there is no
    // rate-limit primary (enterprise/credit plans). The top-level limit is
    // preferred, then the nested one, then the workspace-wide pool a team, EDU
    // or enterprise account reports under spend_control.
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
            })
            .or_else(|| {
                resp.spend_control
                    .as_ref()
                    .and_then(|s| s.individual_limit.as_ref())
                    .and_then(individual_to_window)
            });
    }

    let credits = resp
        .credits
        .as_ref()
        .and_then(|c| c.balance.as_ref())
        .and_then(json_num)
        .map(|b| Credits { remaining: Some(b) });

    let mut extra_rate_windows = Vec::new();
    for w in monthly {
        push_unique(
            &mut extra_rate_windows,
            "codex-monthly".to_string(),
            "Monthly".to_string(),
            w,
        );
    }
    for extra in extra_windows(&resp.additional_rate_limits) {
        if extra_rate_windows
            .iter()
            .any(|e: &ExtraRateWindow| e.id == extra.id)
        {
            continue;
        }
        extra_rate_windows.push(extra);
    }

    if primary.is_none()
        && secondary.is_none()
        && credits.is_none()
        && extra_rate_windows.is_empty()
    {
        return Err(anyhow!("Codex returned no usage windows"));
    }

    let mut payload = ProviderPayload::live(
        "codex",
        source,
        UsageSnapshot {
            primary,
            secondary,
            login_method: resp.plan_type.or(plan_hint),
            extra_rate_windows,
            ..UsageSnapshot::at(now)
        },
    );
    payload.credits = credits;
    Ok(payload)
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let cred = ensure_access_token(timeout)?;
    let tokens = &cred.tokens;

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

    check_status(resp.status(), "Codex", "run `codex` to log in")?;

    let body: UsageResponse = resp.json().context("Codex usage JSON was invalid")?;
    Ok(vec![to_payload(body, now, cred.source, cred.plan_hint)?])
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
        let unknown = win(3, 60 * 60); // a duration with no semantic slot

        // A lone window of unknown duration stays primary.
        assert_eq!(normalize(Some(unknown), None), (Some(unknown), None));
        // (weekly, session) swaps.
        assert_eq!(
            normalize(Some(weekly), Some(session)),
            (Some(session), Some(weekly))
        );
        // (weekly, unknown) swaps.
        assert_eq!(
            normalize(Some(weekly), Some(unknown)),
            (Some(unknown), Some(weekly))
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
        assert_eq!(json_num(&json!("NaN")), None);
        assert_eq!(json_num(&json!("inf")), None);
        assert_eq!(json_num(&json!("-inf")), None);
        assert_eq!(json_num(&json!("7.5")), Some(7.5));
        assert_eq!(json_num(&json!(3)), Some(3.0));
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
        let usage = to_payload(body, Utc::now(), "oauth", None)
            .unwrap()
            .usage
            .unwrap();
        // The lone valid window is weekly-shaped, so it lands in secondary.
        assert!(usage.primary.is_none());
        assert_eq!(usage.secondary.as_ref().unwrap().used_percent, Some(50));
    }

    #[test]
    fn maps_live_codex_sample() {
        // Live free-plan sample: the only window is 43200 minutes long. Held in
        // the primary slot it read as "Session 6%" resetting a month out; it now
        // gets its own Monthly row and leaves the session gauge empty.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"free","rate_limit":{
                "primary_window":{"used_percent":6,"reset_at":1786646643,"limit_window_seconds":2592000},
                "secondary_window":null}}"#,
        )
        .unwrap();
        let payload = to_payload(body, Utc::now(), "oauth", None).unwrap();
        let usage = payload.usage.unwrap();
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        let monthly = &usage.extra_rate_windows[0];
        assert_eq!(monthly.id.as_deref(), Some("codex-monthly"));
        assert_eq!(monthly.title.as_deref(), Some("Monthly"));
        let window = monthly.window.as_ref().unwrap();
        assert_eq!(window.used_percent, Some(6));
        assert_eq!(window.window_minutes, Some(43200));
        assert_eq!(usage.login_method.as_deref(), Some("free"));
    }

    #[test]
    fn monthly_window_does_not_take_the_weekly_slot() {
        // Session plus a 30-day window: the session gauge keeps its slot, the
        // monthly one is labelled rather than shown as "Weekly".
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"plus","rate_limit":{
                "primary_window":{"used_percent":12,"reset_at":1786646643,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":40,"reset_at":1786646643,"limit_window_seconds":2592000}}}"#,
        )
        .unwrap();
        let usage = to_payload(body, Utc::now(), "oauth", None)
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(12));
        assert!(usage.secondary.is_none());
        assert_eq!(
            usage.extra_rate_windows[0].title.as_deref(),
            Some("Monthly")
        );
        assert_eq!(
            usage.extra_rate_windows[0]
                .window
                .as_ref()
                .unwrap()
                .used_percent,
            Some(40)
        );
    }

    #[test]
    fn spend_control_limit_is_the_last_primary_fallback() {
        // Team / EDU workspaces report the administrator-defined pool under
        // spend_control; without it those accounts had no gauge at all.
        let body: UsageResponse = serde_json::from_str(
            r#"{"plan_type":"business","rate_limit":{"primary_window":null,"secondary_window":null},
                "spend_control":{"individual_limit":{"limit":"100","used":"25","resets_at":1786646643}}}"#,
        )
        .unwrap();
        let usage = to_payload(body, Utc::now(), "oauth", None)
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(25));
    }

    #[test]
    fn root_individual_limit_still_wins_over_spend_control() {
        let body: UsageResponse = serde_json::from_str(
            r#"{"individual_limit":{"limit":"100","used":"10"},
                "spend_control":{"individual_limit":{"limit":"100","used":"90"}}}"#,
        )
        .unwrap();
        let usage = to_payload(body, Utc::now(), "oauth", None)
            .unwrap()
            .usage
            .unwrap();
        assert_eq!(usage.primary.unwrap().used_percent, Some(10));
    }

    #[test]
    fn pat_identity_comes_from_whoami() {
        let auth: AuthFile = serde_json::from_str(
            r#"{"personal_access_token":" pat-abc ","OPENAI_API_KEY":"sk-unused"}"#,
        )
        .unwrap();
        let cred = pat_or_api_key(auth, |token| {
            assert_eq!(token, "pat-abc"); // trimmed before it reaches the wire
            Whoami {
                chatgpt_account_id: Some("acct_1".into()),
                chatgpt_plan_type: Some("pro".into()),
            }
        })
        .unwrap();
        assert_eq!(cred.tokens.access_token, "pat-abc");
        assert_eq!(cred.tokens.account_id.as_deref(), Some("acct_1"));
        assert_eq!(cred.source, "pat");
        assert_eq!(cred.plan_hint.as_deref(), Some("pro"));
    }

    #[test]
    fn pat_survives_an_unanswered_whoami() {
        let auth: AuthFile = serde_json::from_str(r#"{"personalAccessToken":"pat-abc"}"#).unwrap();
        let cred = pat_or_api_key(auth, |_| Whoami::default()).unwrap();
        assert_eq!(cred.tokens.access_token, "pat-abc");
        assert!(cred.tokens.account_id.is_none());
        assert_eq!(cred.source, "pat");
    }

    #[test]
    fn api_key_is_the_last_resort() {
        let auth: AuthFile = serde_json::from_str(r#"{"OPENAI_API_KEY":"sk-1"}"#).unwrap();
        let cred = pat_or_api_key(auth, |_| unreachable!("no PAT to resolve")).unwrap();
        assert_eq!(cred.tokens.access_token, "sk-1");
        assert_eq!(cred.source, "api-key");

        let empty: AuthFile = serde_json::from_str(r#"{"OPENAI_API_KEY":"  "}"#).unwrap();
        assert!(pat_or_api_key(empty, |_| Whoami::default()).is_err());
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
        let payload = to_payload(body, Utc::now(), "oauth", None).unwrap();
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
        let primary = to_payload(body, Utc::now(), "oauth", None)
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
        let primary = to_payload(body, Utc::now(), "oauth", None)
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
        assert!(to_payload(body, Utc::now(), "oauth", None).is_err());
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
        assert!(to_payload(body, Utc::now(), "oauth", None).is_err());
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
