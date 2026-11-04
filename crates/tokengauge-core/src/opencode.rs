//! Native opencode Go usage fetcher (opencode.ai).
//!
//! opencode is two products and this reads one of them. **Go** is a $10/month
//! subscription with three spend caps, and it is the one with limits to draw.
//! **Zen** is a pay-as-you-go balance the same account can hold, which Go falls
//! back to once its caps are spent - that half needs a browser session to read
//! and is not here; see the note at the foot.
//!
//! The response is the cleanest of any provider TokenGauge reads:
//!
//! ```json
//! {"usage": {
//!   "rolling": {"status": "ok", "percent": 12.3, "resetsAt": "..."},
//!   "weekly":  {"status": "ok", "percent": 45.6, "resetsAt": "..."},
//!   "monthly": {"status": "ok", "percent": 78.9, "resetsAt": "..."}}}
//! ```
//!
//! Three windows, each already a percentage with the instant it resets. No
//! conversion and no arithmetic: opencode's caps are per-model dollar amounts
//! (5-hour is 20% of the monthly cap, weekly 50%), and the API does that
//! aggregation itself rather than making a reader do it per model.
//!
//! The credential is `OPENCODE_API_KEY`, minted at opencode.ai/auth and pasted
//! into the TUI with `/connect`. opencode 2 files it in its own
//! `$XDG_DATA_HOME/opencode/opencode.db` (or `OPENCODE_DB`), table
//! `credential`, as an `opencode-go` row holding `{"type": "key", "key": ...}`
//! or an OAuth login `{"type": "oauth", "access": ..., "expires": <ms>}`;
//! opencode 1 filed it in `auth.json` beside it as
//! `"opencode-go": {"type": "api", "key": ...}`, which opencode 2 imports once
//! and never reads again. Both versions may have filed the account under the
//! older `opencode` id. The variable wins, as it does for opencode, then the
//! database once it has imported the file, else the file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::credentials::{LiveLogin, LoginTokens, key_login, stored_key};
use crate::provider::check_status;
use crate::{ProviderPayload, UsageSnapshot, UsageWindow, http_client, pct_u8};

const DEFAULT_ENDPOINT: &str = "https://opencode.ai/zen/go/v1/usage";
const ENDPOINT_ENV: &str = "OPENCODE_USAGE_URL";
const API_KEY_ENVS: &[&str] = &["OPENCODE_API_KEY", "OPENCODE_GO_API_KEY"];

/// Minutes each window covers. opencode states them as shares of the monthly
/// cap - 5-hour is 20%, weekly 50% - so the periods are fixed even though the
/// dollar amounts differ per model.
const ROLLING_MINUTES: u32 = 300;
const WEEKLY_MINUTES: u32 = 10_080;

fn env_clean(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim().to_string();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The ids opencode files the Go account under, the Go integration first.
const ACCOUNT_IDS: &[&str] = &["opencode-go", "opencode"];

/// opencode keeps XDG paths on every platform, macOS included, so this is
/// not `dirs::data_dir()`, which would be `~/Library/Application Support`.
fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/share"))
        .join("opencode")
}

pub(crate) fn auth_path() -> PathBuf {
    data_dir().join("auth.json")
}

/// opencode's database, or the one `OPENCODE_DB` names.
pub(crate) fn database_path() -> PathBuf {
    let data = data_dir();
    std::env::var_os("OPENCODE_DB")
        .filter(|v| !v.is_empty())
        .map_or_else(|| data.join("opencode.db"), |db| data.join(db))
}

/// What opencode holds for the Go account. An OAuth access token is accepted
/// by the usage endpoint as a key is, but it rotates, so it names no account.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Login {
    Key(String),
    Access(String),
}

impl Login {
    fn token(&self) -> &str {
        match self {
            Self::Key(token) | Self::Access(token) => token,
        }
    }

    fn source(&self) -> &'static str {
        match self {
            Self::Key(_) => "api key",
            Self::Access(_) => "oauth",
        }
    }
}

/// The Go key in opencode's `auth.json`, which holds every provider's login.
/// A file that is not JSON is an error of its own, never quoted: it holds
/// every provider's login. Only an `api` entry carries a key: a `wellknown`
/// one's `key` names an environment variable.
fn file_key(text: &str) -> Result<Option<String>> {
    let auth: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| anyhow!("opencode's auth.json is not valid JSON"))?;
    Ok(ACCOUNT_IDS.iter().find_map(|id| {
        let entry = auth.get(*id)?;
        let kind = entry.get("type").and_then(serde_json::Value::as_str);
        if kind != Some("api") {
            return None;
        }
        entry
            .get("key")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(str::to_string)
    }))
}

/// A `credential` row's value: a key, or an OAuth login still inside its
/// expiry.
fn row_login(value: &str, now_ms: i64) -> Option<Login> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    let text = |name: &str| {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    match value.get("type")?.as_str()? {
        "key" | "api" => text("key").map(Login::Key),
        "oauth" if value.get("expires")?.as_i64()? > now_ms => text("access").map(Login::Access),
        _ => None,
    }
}

/// The Go login in opencode 2's database, opened read-only: the database is
/// opencode's, and a refresh token in it rotates on use. Once opencode has
/// imported `auth.json` it reads only the database, so `None` - no database,
/// or one before that import - is the only case `auth.json` is asked, and a
/// database that cannot be read is an error rather than a reason to send the
/// key opencode stopped using. Each integration's login is the row opencode
/// selects, the active one, else the newest.
fn database_login(path: &Path, now_ms: i64) -> Result<Option<Option<Login>>> {
    use rusqlite::{Connection, OpenFlags, OptionalExtension};
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(anyhow!("opencode's opencode.db could not be read")),
        Ok(_) => {}
    }
    let read = || -> rusqlite::Result<Option<Option<Login>>> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(500))?;
        let migrated: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'migration')",
            [],
            |row| row.get(0),
        )?;
        let imported = migrated
            && conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM migration WHERE id LIKE '%_import_legacy_credentials')",
                [],
                |row| row.get(0),
            )?;
        if !imported {
            return Ok(None);
        }
        let mut selected = conn.prepare(
            "SELECT value FROM credential WHERE integration_id = ?1 \
             ORDER BY active DESC, time_created DESC, id DESC LIMIT 1",
        )?;
        for id in ACCOUNT_IDS {
            let value = selected
                .query_row([id], |row| row.get::<_, String>(0))
                .optional()?;
            if let Some(login) = value.and_then(|value| row_login(&value, now_ms)) {
                return Ok(Some(Some(login)));
            }
        }
        Ok(Some(None))
    };
    read().map_err(|_| anyhow!("opencode's opencode.db could not be read"))
}

/// No file is no key; a file that cannot be read says why.
fn stored_file_key() -> Result<Option<String>> {
    match std::fs::read_to_string(auth_path()) {
        Ok(text) => file_key(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!(
            "opencode's auth.json could not be read: {}",
            e.kind()
        )),
    }
}

/// The Go login and where it was found.
pub(crate) fn login() -> Result<(Login, &'static str)> {
    if let Some(key) = API_KEY_ENVS.iter().find_map(|name| env_clean(name)) {
        return Ok((Login::Key(key), "environment"));
    }
    let key_missing =
        || anyhow!("opencode key missing - `/connect` it in opencode, or set OPENCODE_API_KEY");
    if let Some(login) = database_login(&database_path(), Utc::now().timestamp_millis())? {
        return login
            .map(|login| (login, "opencode.db"))
            .ok_or_else(key_missing);
    }
    stored_file_key()?
        .map(|key| (Login::Key(key), "auth.json"))
        .ok_or_else(key_missing)
}

/// The usage endpoint, from an override a self-hosted gateway may set.
///
/// Split from the environment read so it can be tested: `std::env::set_var` is
/// unsafe from the 2024 edition, so a test going through the variable would
/// have to be the only test running.
fn endpoint_for(override_url: Option<&str>) -> Result<String> {
    let url = override_url.unwrap_or(DEFAULT_ENDPOINT);
    if !url.starts_with("https://") {
        return Err(anyhow!("opencode usage URL must use HTTPS"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

// ---------------------------------------------------------------------------
// Wire response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct UsageResponse {
    #[serde(default)]
    usage: Option<Windows>,
    /// An error envelope comes back with the same 200 some gateways give, so a
    /// response carrying one is a failure however it was framed.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Windows {
    #[serde(default)]
    rolling: Option<Window>,
    #[serde(default)]
    weekly: Option<Window>,
    #[serde(default)]
    monthly: Option<Window>,
}

#[derive(Debug, Clone, Deserialize)]
struct Window {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<String>,
}

impl Window {
    /// The window as the panel draws it, or `None` when the percentage is
    /// missing or not a percentage. A gauge is the whole point of this row, so
    /// a window without one is not worth a row.
    fn to_usage(&self) -> Option<UsageWindow> {
        let percent = self
            .percent
            .filter(|p| p.is_finite() && (0.0..=100.0).contains(p))?;
        Some(UsageWindow {
            used_percent: Some(pct_u8(percent)),
            // opencode says outright when a window is spent, which is more
            // use than inferring it from 100%: a window can sit at 100 and
            // still serve, and one can be limited before it gets there.
            reset_description: self
                .status
                .as_deref()
                .filter(|s| s.eq_ignore_ascii_case("rate-limited"))
                .map(|_| "rate limited".to_string()),
            resets_at: self
                .resets_at
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            window_minutes: None,
            burn: None,
        })
    }
}

fn to_payload(
    body: UsageResponse,
    now: DateTime<Utc>,
    source: &str,
    unauthorized_hint: &str,
) -> Result<ProviderPayload> {
    if body.error.is_some() {
        return Err(anyhow!("opencode returned an error - {unauthorized_hint}"));
    }
    let windows = body
        .usage
        .ok_or_else(|| anyhow!("opencode response carried no usage"))?;

    let mut rolling = windows.rolling.as_ref().and_then(Window::to_usage);
    let mut weekly = windows.weekly.as_ref().and_then(Window::to_usage);
    let monthly = windows.monthly.as_ref().and_then(Window::to_usage);

    // A response with nothing in any window is a failure, not an empty panel:
    // serving it would replace yesterday's figures with three blanks and call
    // that a successful fetch.
    if rolling.is_none() && weekly.is_none() && monthly.is_none() {
        return Err(anyhow!("opencode reported no usable usage window"));
    }

    // The periods are fixed even though each model's dollar cap is not, so the
    // pace projection has something to measure against.
    if let Some(w) = rolling.as_mut() {
        w.window_minutes = Some(ROLLING_MINUTES);
    }
    if let Some(w) = weekly.as_mut() {
        w.window_minutes = Some(WEEKLY_MINUTES);
    }

    Ok(ProviderPayload::live(
        "opencode",
        source,
        UsageSnapshot {
            primary: rolling,
            secondary: weekly,
            tertiary: monthly,
            // The label already says Go, so a plan line repeating it would
            // read as "opencode Go · Go".
            login_method: None,
            ..UsageSnapshot::at(now)
        },
    ))
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

pub(crate) fn fetch(timeout: Duration) -> Result<Vec<ProviderPayload>> {
    let now = Utc::now();
    let (login, _) = login()?;
    Ok(vec![usage_for(
        login.token(),
        timeout,
        now,
        login.source(),
        "check OPENCODE_API_KEY, or `/connect` opencode Go again",
    )?])
}

pub(crate) fn live_login() -> LiveLogin {
    match login() {
        Ok((Login::Key(key), _)) => key_login(Some(key)),
        Ok((Login::Access(token), _)) => LiveLogin {
            account: None,
            tokens: LoginTokens {
                access: Some(token),
                refresh: None,
            },
        },
        Err(_) => key_login(None),
    }
}

pub(crate) fn fetch_stored(
    text: &str,
    timeout: Duration,
    now: DateTime<Utc>,
) -> Result<ProviderPayload> {
    usage_for(
        &stored_key(text)?,
        timeout,
        now,
        "api key",
        "opencode refused the stored key (`remuda login -p opencode` replaces it)",
    )
}

fn usage_for(
    key: &str,
    timeout: Duration,
    now: DateTime<Utc>,
    source: &str,
    unauthorized_hint: &str,
) -> Result<ProviderPayload> {
    let url = endpoint_for(env_clean(ENDPOINT_ENV).as_deref())?;
    let client = http_client(timeout)?;

    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {key}"))
        .header("accept", "application/json")
        .send()
        .context("opencode usage request failed")?;

    check_status(resp.status(), "opencode", unauthorized_hint)?;

    let text = resp.text().context("opencode usage read failed")?;
    if text.trim().is_empty() {
        return Err(anyhow!("opencode returned an empty response"));
    }
    let body: UsageResponse =
        serde_json::from_str(&text).context("opencode usage JSON was invalid")?;

    to_payload(body, now, source, unauthorized_hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-16T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    /// `/connect` files the key beside every other provider's login.
    #[test]
    fn the_go_key_is_read_from_its_entry_in_opencodes_file() {
        let text = r#"{"openrouter": {"type": "api", "key": "or"},
                       "opencode-go": {"type": "api", "key": " go-1 "}}"#;
        assert_eq!(file_key(text).unwrap().as_deref(), Some("go-1"));
        assert_eq!(
            file_key(r#"{"openrouter": {"type": "api", "key": "or"}}"#).unwrap(),
            None
        );
        assert_eq!(
            file_key(r#"{"opencode-go": {"type": "api", "key": ""}}"#).unwrap(),
            None
        );
        let err = file_key(r#"{"opencode-go": {"key": "go-secret""#).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        assert!(!err.to_string().contains("go-secret"), "{err}");
    }

    #[test]
    fn opencode_1s_own_id_and_only_an_api_entry_carry_a_key() {
        assert_eq!(
            file_key(r#"{"opencode": {"type": "api", "key": "legacy"}}"#)
                .unwrap()
                .as_deref(),
            Some("legacy")
        );
        assert_eq!(
            file_key(
                r#"{"opencode": {"type": "api", "key": "legacy"},
                    "opencode-go": {"type": "api", "key": "go"}}"#
            )
            .unwrap()
            .as_deref(),
            Some("go")
        );
        assert_eq!(
            file_key(r#"{"opencode": {"type": "wellknown", "key": "OPENCODE_TOKEN"}}"#).unwrap(),
            None
        );
        assert_eq!(
            file_key(r#"{"opencode-go": {"key": "untyped"}}"#).unwrap(),
            None
        );
    }

    const NOW_MS: i64 = 1_800_000_000_000;

    #[test]
    fn a_credential_row_is_a_key_or_an_unexpired_oauth_login() {
        assert_eq!(
            row_login(r#"{"type": "key", "key": " k "}"#, NOW_MS),
            Some(Login::Key("k".into()))
        );
        let oauth = |expires: i64| {
            format!(r#"{{"type": "oauth", "access": "tok", "refresh": "r", "expires": {expires}}}"#)
        };
        assert_eq!(
            row_login(&oauth(NOW_MS + 1), NOW_MS),
            Some(Login::Access("tok".into()))
        );
        assert_eq!(row_login(&oauth(NOW_MS - 1), NOW_MS), None);
        assert_eq!(row_login(r#"{"type": "key", "key": ""}"#, NOW_MS), None);
        assert_eq!(row_login("not json", NOW_MS), None);
    }

    type Row<'a> = (&'a str, Option<bool>, i64, &'a str);

    fn database(name: &str, imported: bool, rows: &[Row]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-opencode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL); \
             INSERT INTO migration VALUES ('20260611192811_lush_chimera', 0); \
             CREATE TABLE credential (id text PRIMARY KEY, integration_id text, \
             label text NOT NULL, value text NOT NULL, connector_id text, method_id text, \
             active integer, time_created integer NOT NULL, time_updated integer NOT NULL);",
        )
        .unwrap();
        if imported {
            conn.execute(
                "INSERT INTO migration VALUES ('20260805200742_import_legacy_credentials', 0)",
                [],
            )
            .unwrap();
        }
        for (i, (integration, active, created, value)) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO credential VALUES (?1, ?2, 'l', ?3, NULL, NULL, ?4, ?5, ?5)",
                rusqlite::params![i.to_string(), integration, value, active, created],
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn the_database_gives_the_login_opencode_selects() {
        let key = |k: &str| format!(r#"{{"type": "key", "key": "{k}"}}"#);
        let expired = r#"{"type": "oauth", "access": "old", "expires": 1}"#;
        let (legacy, imported_old, imported_new, active, other) = (
            key("legacy"),
            key("go-old"),
            key("go"),
            key("go-active"),
            key("other"),
        );

        let mut rows: Vec<Row> = vec![
            ("opencode", Some(true), 3, &legacy),
            ("opencode-go", None, 2, &imported_old),
            ("opencode-go", None, 5, &imported_new),
            ("anthropic", Some(true), 9, &other),
        ];
        let path = database("newest", true, &rows);
        assert_eq!(
            database_login(&path, NOW_MS).unwrap(),
            Some(Some(Login::Key("go".into())))
        );
        rows.push(("opencode-go", Some(true), 1, &active));
        let path = database("active", true, &rows);
        assert_eq!(
            database_login(&path, NOW_MS).unwrap(),
            Some(Some(Login::Key("go-active".into())))
        );

        let inactive = key("inactive");
        let path = database(
            "expired",
            true,
            &[
                ("opencode-go", Some(true), 3, expired),
                ("opencode-go", Some(false), 9, &inactive),
                ("opencode", Some(true), 2, &legacy),
            ],
        );
        assert_eq!(
            database_login(&path, NOW_MS).unwrap(),
            Some(Some(Login::Key("legacy".into())))
        );

        let path = database("empty", true, &[("anthropic", Some(true), 1, &other)]);
        assert_eq!(database_login(&path, NOW_MS).unwrap(), Some(None));
    }

    #[test]
    fn auth_json_is_asked_only_without_a_database_that_imported_it() {
        let go = r#"{"type": "key", "key": "go"}"#;
        let path = database("pending", false, &[("opencode-go", Some(true), 1, go)]);
        assert_eq!(database_login(&path, NOW_MS).unwrap(), None);

        let dir = std::env::temp_dir().join(format!("tg-opencode-none-{}", std::process::id()));
        assert_eq!(
            database_login(&dir.join("opencode.db"), NOW_MS).unwrap(),
            None
        );
        assert!(!dir.join("opencode.db").exists());

        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.db");
        std::fs::write(&junk, "not a database").unwrap();
        assert!(database_login(&junk, NOW_MS).is_err());
    }

    #[test]
    fn an_imported_database_that_cannot_be_read_is_an_error_not_auth_json() {
        let path = database("broken", true, &[]);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE credential")
            .unwrap();
        let err = database_login(&path, NOW_MS).unwrap_err();
        assert_eq!(err.to_string(), "opencode's opencode.db could not be read");

        let dir = path.parent().unwrap();
        let unmigrated = dir.join("unmigrated.db");
        rusqlite::Connection::open(&unmigrated)
            .unwrap()
            .execute_batch("CREATE TABLE other (id TEXT)")
            .unwrap();
        assert_eq!(database_login(&unmigrated, NOW_MS).unwrap(), None);
    }

    #[test]
    fn a_row_says_whether_it_was_read_with_a_key_or_an_oauth_login() {
        assert_eq!(Login::Key("k".into()).source(), "api key");
        assert_eq!(Login::Access("t".into()).source(), "oauth");
    }

    fn parse(raw: &str) -> Result<ProviderPayload> {
        to_payload(
            serde_json::from_str(raw).expect("fixture parses"),
            at(),
            "api key",
            "check OPENCODE_API_KEY",
        )
    }

    const FULL: &str = r#"{"usage":{
        "rolling":{"status":"ok","percent":12.3,"resetsAt":"2026-09-16T05:00:00Z"},
        "weekly":{"status":"ok","percent":45.6,"resetsAt":"2026-09-21T00:00:00Z"},
        "monthly":{"status":"ok","percent":78.9,"resetsAt":"2026-10-01T00:00:00Z"}}}"#;

    /// The three windows land in the order the panel draws them, each keeping
    /// its own reset instant. No arithmetic: opencode aggregates its per-model
    /// dollar caps into a percentage before we see them.
    #[test]
    fn the_three_windows_map_straight_onto_the_three_slots() {
        let usage = parse(FULL).expect("payload").usage.expect("usage");

        let rolling = usage.primary.expect("rolling");
        assert_eq!(rolling.used_percent, Some(12));
        assert_eq!(rolling.window_minutes, Some(300));
        assert_eq!(rolling.resets_at.as_deref(), Some("2026-09-16T05:00:00Z"));

        assert_eq!(usage.secondary.expect("weekly").used_percent, Some(46));
        assert_eq!(usage.tertiary.expect("monthly").used_percent, Some(79));
        assert_eq!(
            usage.login_method, None,
            "the provider label already says which product this is"
        );
    }

    /// A window opencode has not reported is absent rather than zero. The two
    /// look identical on a bar and mean opposite things.
    #[test]
    fn a_window_that_was_not_reported_draws_nothing() {
        let payload =
            parse(r#"{"usage":{"monthly":{"status":"ok","percent":50.0}}}"#).expect("payload");
        let usage = payload.usage.expect("usage");
        assert!(usage.primary.is_none());
        assert!(usage.secondary.is_none());
        assert_eq!(usage.tertiary.expect("monthly").used_percent, Some(50));
    }

    /// opencode says outright when a window is spent, which is better than
    /// inferring it from 100%: a window can sit at 100 and still serve, and it
    /// can be limited before it gets there.
    #[test]
    fn a_rate_limited_window_says_so_rather_than_being_inferred() {
        let payload = parse(
            r#"{"usage":{"rolling":{"status":"rate-limited","percent":100.0,
                "resetsAt":"2026-09-16T05:00:00Z"}}}"#,
        )
        .expect("payload");
        let rolling = payload.usage.expect("usage").primary.expect("rolling");
        assert_eq!(rolling.used_percent, Some(100));
        assert_eq!(rolling.reset_description.as_deref(), Some("rate limited"));

        // And an ordinary window carries no such note.
        let ok = parse(FULL).expect("payload").usage.expect("usage");
        assert_eq!(ok.primary.expect("rolling").reset_description, None);
    }

    /// A status this build has not seen still draws its percentage. The number
    /// is the data; refusing the whole response over an unrecognised word is
    /// how a provider disappears on the day its vendor adds a state.
    #[test]
    fn an_unknown_status_still_draws_its_percentage() {
        let payload =
            parse(r#"{"usage":{"weekly":{"status":"degraded","percent":33.0}}}"#).expect("payload");
        assert_eq!(
            payload
                .usage
                .expect("usage")
                .secondary
                .expect("weekly")
                .used_percent,
            Some(33)
        );
    }

    /// A response with nothing usable is a failure, not an empty panel. Serving
    /// it would replace yesterday's figures with three blanks and call that a
    /// successful fetch - which is what the stale fallback exists to prevent.
    #[test]
    fn a_response_with_no_usable_window_is_an_error() {
        for raw in [
            r#"{"usage":{}}"#,
            r#"{"usage":{"rolling":{"status":"ok"}}}"#,
            r#"{"usage":{"rolling":{"status":"ok","percent":null}}}"#,
            r#"{}"#,
        ] {
            assert!(parse(raw).is_err(), "{raw} was accepted");
        }
    }

    /// A percentage that is not one is dropped rather than clamped into a bar
    /// nobody can defend. Out of range and out of `f64` are caught in
    /// different places, which is worth stating rather than conflating: serde
    /// refuses a number it cannot hold before this code sees it, so the
    /// range check here is about values that parse perfectly well and are
    /// still not percentages.
    #[test]
    fn a_percentage_that_is_not_a_percentage_is_dropped() {
        for bad in ["-1.0", "101.0", "1000"] {
            let raw = format!(r#"{{"usage":{{"weekly":{{"status":"ok","percent":{bad}}}}}}}"#);
            assert!(parse(&raw).is_err(), "{bad} was accepted");
        }

        let unrepresentable = serde_json::from_str::<UsageResponse>(
            r#"{"usage":{"weekly":{"status":"ok","percent":1e400}}}"#,
        );
        assert!(
            unrepresentable.is_err(),
            "serde used to reject this; if it now yields inf the is_finite guard is what catches it"
        );
    }

    /// An error envelope can arrive with a 200 through a gateway, so the body
    /// decides rather than the status line.
    #[test]
    fn an_error_envelope_is_a_failure_whatever_the_status_line_said() {
        let err = parse(r#"{"error":{"message":"invalid key"}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OPENCODE_API_KEY"), "{err}");
    }

    /// The key travels as a bearer header, so a URL that downgrades the
    /// transport would put it on the wire in the clear.
    #[test]
    fn an_endpoint_that_is_not_https_is_refused() {
        assert_eq!(endpoint_for(None).unwrap(), DEFAULT_ENDPOINT);
        assert_eq!(
            endpoint_for(Some("https://gw.example.test/usage/")).unwrap(),
            "https://gw.example.test/usage"
        );
        for bad in [
            "http://opencode.ai/zen/go/v1/usage",
            "example.test",
            "ftp://x.test",
        ] {
            let err = endpoint_for(Some(bad)).unwrap_err().to_string();
            assert!(err.contains("HTTPS"), "{bad} was accepted: {err}");
        }
    }
}
