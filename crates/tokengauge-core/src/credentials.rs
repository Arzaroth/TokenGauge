//! The credential store a switcher writes, read and never written.
//!
//! One provider can hold several credentials - a work plan and a personal one -
//! and the CLI is signed into one of them at a time. [remuda] keeps a copy of
//! each under `<store>/<provider>/<name>.json`, beside a `<name>.meta.json`
//! sidecar naming whose it is. This module reads that layout, refuses a store
//! anyone else could read or plant tokens in, and finds which stored credential
//! the live login is. ADR 0003 is the contract; remuda's side of it is its
//! `brain/architecture/store.md`.
//!
//! Nothing here refreshes, moves or writes a stored credential. A refresh
//! token rotates on use, so two processes refreshing one lock one of them out,
//! and the switcher is the one holding it.
//!
//! [remuda]: https://github.com/Arzaroth/remuda

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::doctor::DoctorCheck;
use crate::{CredentialState, TokenGaugeConfig};

/// remuda's own refresh period: asking an inactive credential more often than
/// its tokens can change buys nothing but rate limits.
pub const DEFAULT_INACTIVE_REFRESH_SECS: u64 = 1800;

/// Where remuda keeps its store when nothing moves it.
///
/// Deliberately not remuda's resolution. That reads `$REMUDA_STORE` and
/// `$XDG_DATA_HOME`, and the daemon (a systemd unit's environment) and a
/// frontend's in-process fetch (the compositor's) can see those differently,
/// which would have two processes read two stores into one snapshot.
pub fn default_store() -> PathBuf {
    #[cfg(windows)]
    {
        dirs::data_local_dir()
            .unwrap_or_default()
            .join("remuda")
            .join("credentials")
    }
    #[cfg(not(windows))]
    {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".local/share/remuda/credentials")
    }
}

/// A sidecar, minus what TokenGauge never reads: `email` stays on the machine
/// and out of every file TokenGauge writes, and `oauthAccount` is remuda's to
/// restore on a switch.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    account_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    creds_digest: Option<String>,
}

/// One stored credential, as read.
#[derive(Debug, Clone)]
pub struct StoredCredential {
    pub name: String,
    /// The credential file's text. The digest was taken over these bytes, and
    /// these are the bytes parsed: reading it twice could check one version
    /// and use another.
    pub(crate) text: String,
    /// The sidecar's identity. `None` when its digest names other tokens.
    pub account_id: Option<String>,
    /// The sidecar's label, trusted on the same terms as its identity.
    pub label: Option<String>,
}

impl StoredCredential {
    /// Whether the sidecar was written for these tokens.
    pub fn verified(&self) -> bool {
        self.account_id.is_some()
    }
}

/// One provider's directory, as read.
#[derive(Debug, Default)]
pub struct ProviderStore {
    /// Every credential that could be read, in name order.
    pub entries: Vec<StoredCredential>,
    /// `(name, why)` for every credential file that was not.
    pub skipped: Vec<(String, String)>,
    /// Why the whole directory was refused, when it was.
    pub refused: Option<String>,
}

/// The credential names under `<store>/<provider>/`, sorted.
///
/// A credential is a `.json` file directly under the provider directory that
/// neither starts with `.` nor ends in `.meta.json`: dot-files are the
/// switcher's own, and the other suffix is a sidecar. Only names, so it is
/// cheap enough for [`crate::cache_is_stale`] to call on every render.
pub fn credential_names(store: &Path, provider: &str) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(store.join(provider)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|f| !f.starts_with('.') && !f.ends_with(".meta.json"))
        .filter_map(|f| f.strip_suffix(".json").map(str::to_string))
        .collect();
    names.sort();
    names
}

/// When the switcher last changed a provider's active credential, in Unix
/// milliseconds. remuda rewrites `<store>/.last-switch.json` on every switch
/// as `{"<provider>": <ms>}`. A missing or unreadable file is no signal
/// rather than an error: the listing and the refresh period still apply.
pub fn last_switch_ms(store: &Path, provider: &str) -> Option<i64> {
    let text = std::fs::read_to_string(store.join(".last-switch.json")).ok()?;
    let all: BTreeMap<String, i64> = serde_json::from_str(&text).ok()?;
    all.get(provider).copied()
}

/// Read every credential a provider has in the store.
pub fn read_provider(store: &Path, provider: &str) -> ProviderStore {
    let dir = store.join(provider);
    if !dir.is_dir() {
        return ProviderStore::default();
    }
    for path in [store, dir.as_path()] {
        if let Err(why) = private(path, true) {
            return ProviderStore {
                refused: Some(why),
                ..ProviderStore::default()
            };
        }
    }
    let mut out = ProviderStore::default();
    for name in credential_names(store, provider) {
        match read_one(&dir, &name) {
            Ok(entry) => out.entries.push(entry),
            Err(why) => out.skipped.push((name, why)),
        }
    }
    out
}

fn read_one(dir: &Path, name: &str) -> std::result::Result<StoredCredential, String> {
    let path = dir.join(format!("{name}.json"));
    private(&path, false)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read it: {e}"))?;
    let digest = hex_sha256(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| "it is not UTF-8".to_string())?;
    if serde_json::from_str::<serde_json::Value>(&text).is_err() {
        return Err("it is not valid JSON".to_string());
    }
    let meta_path = dir.join(format!("{name}.meta.json"));
    let meta = match std::fs::read_to_string(&meta_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("it has no {name}.meta.json"));
        }
        Err(e) => return Err(format!("cannot read {name}.meta.json: {e}")),
    };
    let sidecar: Sidecar =
        serde_json::from_str(&meta).map_err(|e| format!("{name}.meta.json is malformed: {e}"))?;
    let verified = sidecar
        .creds_digest
        .as_deref()
        .is_none_or(|d| d.eq_ignore_ascii_case(&digest));
    Ok(StoredCredential {
        name: name.to_string(),
        text,
        account_id: verified.then_some(sidecar.account_id),
        label: sidecar
            .label
            .filter(|l| verified && !l.trim().is_empty())
            .map(|l| l.trim().to_string()),
    })
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A store directory must be the user's and not writable by anyone else, and
/// a credential file the user's and closed to everyone else.
///
/// "The user" is the owner of the home directory: this crate has no `unsafe`,
/// and that is the one uid it can read without asking the kernel for its own.
#[cfg(unix)]
fn private(path: &Path, is_dir: bool) -> std::result::Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let owner = dirs::home_dir()
        .and_then(|home| std::fs::metadata(home).ok())
        .map(|home| home.uid());
    if owner.is_some_and(|uid| uid != meta.uid()) {
        return Err(format!("{} does not belong to you", path.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if is_dir && mode & 0o022 != 0 {
        return Err(format!(
            "{} is writable by others (mode {mode:o})",
            path.display()
        ));
    }
    if !is_dir && mode & 0o077 != 0 {
        return Err(format!(
            "{} is open to others (mode {mode:o}); it should be 600",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private(_path: &Path, _is_dir: bool) -> std::result::Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// The live login, and which stored credential it is
// ---------------------------------------------------------------------------

/// The tokens a login holds, for telling one login from another. Tokens
/// rotate, so equal tokens are proof of the same login and different ones are
/// proof of nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LoginTokens {
    pub access: Option<String>,
    pub refresh: Option<String>,
}

impl LoginTokens {
    fn same_login(&self, other: &LoginTokens) -> bool {
        let equal = |a: &Option<String>, b: &Option<String>| matches!((a, b), (Some(a), Some(b)) if !a.is_empty() && a == b);
        equal(&self.refresh, &other.refresh) || equal(&self.access, &other.access)
    }
}

/// What the CLI is signed into right now.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct LiveLogin {
    pub tokens: LoginTokens,
    /// The identity the switcher files the login under. `None` when nothing
    /// that describes the token actually sent carries one: an override
    /// variable, a personal access token, an API key.
    pub account: Option<String>,
}

/// How one provider's credentials are read, for the providers a switcher
/// stores. Function pointers rather than a trait, like the rest of the
/// provider table.
pub(crate) struct StoreReader {
    /// The live login, read without a network call.
    pub live: fn() -> LiveLogin,
    /// A stored credential's tokens.
    pub tokens: fn(&str) -> LoginTokens,
    /// The fetch's checks without the request, for `--doctor`.
    pub check: fn(&str, DateTime<Utc>) -> Result<Option<CredentialState>>,
}

/// How a provider's store lines up with its live login.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// The entry the live login is. Its stored copy is never read: the live
    /// source can be hours ahead of it.
    pub live: Option<usize>,
    /// Every other entry worth reading, in store order.
    pub others: Vec<usize>,
    /// `(name, why)` for entries left out because another one already stands
    /// for the same account.
    pub duplicates: Vec<(String, String)>,
}

/// Find the live login in the store, the way the switcher does: by refresh
/// token, then by access token, then by the verified sidecar naming its
/// identity.
///
/// Two verified sidecars naming one account would draw the same plan twice
/// and count it twice in the combined figure, so one stands for it - the one
/// the live login is, else the first by name - and the rest are left out.
pub(crate) fn resolve(
    live: Option<&LiveLogin>,
    entries: &[StoredCredential],
    tokens: fn(&str) -> LoginTokens,
) -> Resolved {
    let by_token = live.and_then(|live| {
        entries
            .iter()
            .position(|e| tokens(&e.text).same_login(&live.tokens))
    });
    let by_account = || {
        let account = live?.account.as_deref()?;
        entries
            .iter()
            .position(|e| e.account_id.as_deref() == Some(account))
    };
    let matched = by_token.or_else(by_account);

    let mut out = Resolved {
        live: matched,
        ..Resolved::default()
    };
    for (i, entry) in entries.iter().enumerate() {
        if Some(i) == matched {
            continue;
        }
        let keeper = entry.account_id.as_deref().and_then(|account| {
            matched
                .filter(|m| entries[*m].account_id.as_deref() == Some(account))
                .or_else(|| {
                    entries
                        .iter()
                        .position(|e| e.account_id.as_deref() == Some(account))
                })
        });
        match keeper {
            Some(k) if k != i => out.duplicates.push((
                entry.name.clone(),
                format!("the same account as {}", entries[k].name),
            )),
            _ => out.others.push(i),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// --doctor
// ---------------------------------------------------------------------------

/// What `--doctor` says about the store: one line for a store that is off,
/// absent or refused, and one validated line per stored credential.
///
/// Validated, not statted, under the same rule as the Credentials section:
/// each line runs the parse, digest, expiry and scope checks the fetch makes,
/// offline, so a green line is a credential the next fetch will ask about.
pub fn doctor_checks(config: &TokenGaugeConfig, now: DateTime<Utc>) -> Vec<DoctorCheck> {
    let check = |label: String, ok: bool, detail: String| DoctorCheck { label, ok, detail };
    let Some(store) = config.credentials.store_root() else {
        return vec![check(
            "credential store".into(),
            true,
            "off ([credentials] store is empty)".into(),
        )];
    };
    if !store.is_dir() {
        return vec![check(
            format!("credential store: {}", store.display()),
            true,
            "none there - remuda is not in use, or keeps its store elsewhere".into(),
        )];
    }
    let mut out = vec![check(
        format!("credential store: {}", store.display()),
        true,
        String::new(),
    )];
    for provider in crate::stored_providers() {
        if !config.providers.is_enabled(provider) {
            continue;
        }
        let Some(reader) = crate::providers::store_reader(provider) else {
            continue;
        };
        let read = read_provider(store, provider);
        if let Some(why) = read.refused {
            out.push(check(
                format!("{provider} store"),
                false,
                format!("refused: {why}"),
            ));
            continue;
        }
        let live = (reader.live)();
        let resolved = resolve(Some(&live), &read.entries, reader.tokens);
        for (i, entry) in read.entries.iter().enumerate() {
            let label = format!("{provider}/{}", entry.name);
            if Some(i) == resolved.live {
                let detail = if entry.verified() {
                    "active - read from the CLI's own login"
                } else {
                    "active, but its sidecar names other tokens - remuda confirms whose on its next run"
                };
                out.push(check(label, true, detail.into()));
                continue;
            }
            if resolved
                .duplicates
                .iter()
                .any(|(name, _)| *name == entry.name)
            {
                continue;
            }
            if !entry.verified() {
                out.push(check(
                    label,
                    false,
                    "unverified - its sidecar names other tokens; remuda re-identifies it on its next run".into(),
                ));
                continue;
            }
            match (reader.check)(&entry.text, now) {
                Ok(None) => out.push(check(label, true, "inactive".into())),
                Ok(Some(CredentialState::Expired)) => out.push(check(
                    label,
                    false,
                    "access token expired - remuda's timer refreshes it (`remuda refresh`)".into(),
                )),
                Ok(Some(state)) => out.push(check(label, false, format!("{state:?}"))),
                Err(e) => out.push(check(label, false, format!("{e:#}"))),
            }
        }
        for (name, why) in read.skipped.iter().chain(resolved.duplicates.iter()) {
            out.push(check(
                format!("{provider}/{name}"),
                false,
                format!("skipped: {why}"),
            ));
        }
        if resolved.live.is_none() && !read.entries.is_empty() && live != LiveLogin::default() {
            out.push(check(
                format!("{provider} live login"),
                true,
                "matches no stored credential - drawn on its own".into(),
            ));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp_store(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-store-{tag}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        dir
    }

    #[cfg(unix)]
    pub(crate) fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(not(unix))]
    pub(crate) fn set_mode(_path: &Path, _mode: u32) {}

    /// Write a credential and its sidecar the way remuda does: the file at
    /// 0600, and a digest over its exact bytes.
    pub(crate) fn put(store: &Path, provider: &str, name: &str, creds: &str, account: &str) {
        let dir = store.join(provider);
        std::fs::create_dir_all(&dir).unwrap();
        set_mode(&dir, 0o700);
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, creds).unwrap();
        set_mode(&path, 0o600);
        let meta = serde_json::json!({
            "accountId": account,
            "email": format!("{account}@example.com"),
            "capturedAt": 1,
            "label": format!("{name} label"),
            "credsDigest": hex_sha256(creds.as_bytes()),
        });
        std::fs::write(
            dir.join(format!("{name}.meta.json")),
            serde_json::to_string(&meta).unwrap(),
        )
        .unwrap();
    }

    fn tokens_of(text: &str) -> LoginTokens {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        LoginTokens {
            access: v["access"].as_str().map(str::to_string),
            refresh: v["refresh"].as_str().map(str::to_string),
        }
    }

    fn creds(access: &str, refresh: &str) -> String {
        format!(r#"{{"access":"{access}","refresh":"{refresh}"}}"#)
    }

    fn entry(name: &str, account: Option<&str>, access: &str, refresh: &str) -> StoredCredential {
        StoredCredential {
            name: name.into(),
            text: creds(access, refresh),
            account_id: account.map(str::to_string),
            label: None,
        }
    }

    fn live(access: &str, refresh: &str, account: Option<&str>) -> LiveLogin {
        LiveLogin {
            tokens: LoginTokens {
                access: Some(access.into()),
                refresh: Some(refresh.into()),
            },
            account: account.map(str::to_string),
        }
    }

    #[test]
    fn the_store_is_listed_by_credential_file_and_nothing_else() {
        let store = temp_store("listing");
        put(&store, "claude", "work", "{}", "u-1");
        put(&store, "claude", "perso", "{}", "u-2");
        let dir = store.join("claude");
        std::fs::write(dir.join(".set-aside-u-1-5.json"), "{}").unwrap();
        std::fs::write(dir.join("orphan.meta.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        std::fs::write(store.join(".last-switch.json"), r#"{"claude": 42}"#).unwrap();

        assert_eq!(credential_names(&store, "claude"), ["perso", "work"]);
        assert!(credential_names(&store, "codex").is_empty());
        assert_eq!(last_switch_ms(&store, "claude"), Some(42));
        assert_eq!(last_switch_ms(&store, "codex"), None);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_sidecar_written_for_other_tokens_is_not_trusted() {
        let store = temp_store("digest");
        put(&store, "claude", "work", r#"{"a":1}"#, "u-1");
        // The credential moved on and the sidecar did not: the crash between
        // remuda's two renames.
        let path = store.join("claude/work.json");
        std::fs::write(&path, r#"{"a":2}"#).unwrap();
        set_mode(&path, 0o600);
        put(&store, "claude", "perso", r#"{"b":1}"#, "u-2");

        let read = read_provider(&store, "claude");
        let work = read.entries.iter().find(|e| e.name == "work").unwrap();
        assert!(!work.verified());
        assert_eq!(
            work.label, None,
            "a label from another credential's sidecar"
        );
        let perso = read.entries.iter().find(|e| e.name == "perso").unwrap();
        assert_eq!(perso.account_id.as_deref(), Some("u-2"));
        assert_eq!(perso.label.as_deref(), Some("perso label"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_sidecar_from_before_the_digest_is_trusted() {
        let store = temp_store("legacy");
        put(&store, "claude", "work", "{}", "u-1");
        std::fs::write(
            store.join("claude/work.meta.json"),
            r#"{"accountId":"u-1","email":"x","capturedAt":1}"#,
        )
        .unwrap();
        let read = read_provider(&store, "claude");
        assert_eq!(read.entries[0].account_id.as_deref(), Some("u-1"));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn an_entry_that_cannot_be_read_is_skipped_and_named() {
        let store = temp_store("skipped");
        put(&store, "claude", "work", "{}", "u-1");
        put(&store, "claude", "bad", "not json", "u-2");
        let lonely = store.join("claude/lonely.json");
        std::fs::write(&lonely, "{}").unwrap();
        set_mode(&lonely, 0o600);

        let read = read_provider(&store, "claude");
        assert_eq!(read.entries.len(), 1);
        let why: BTreeMap<_, _> = read.skipped.into_iter().collect();
        assert_eq!(why["lonely"], "it has no lonely.meta.json");
        assert_eq!(why["bad"], "it is not valid JSON");
        let _ = std::fs::remove_dir_all(&store);
    }

    #[cfg(unix)]
    #[test]
    fn a_store_others_can_reach_is_refused() {
        let store = temp_store("private");
        put(&store, "claude", "work", "{}", "u-1");
        put(&store, "claude", "open", "{}", "u-2");
        set_mode(&store.join("claude/open.json"), 0o644);

        let read = read_provider(&store, "claude");
        assert_eq!(read.entries.len(), 1);
        assert!(read.skipped[0].1.contains("open to others"), "{read:?}");

        set_mode(&store.join("claude"), 0o777);
        let read = read_provider(&store, "claude");
        assert!(read.entries.is_empty());
        assert!(
            read.refused
                .as_deref()
                .unwrap()
                .contains("writable by others"),
            "{read:?}"
        );
        set_mode(&store.join("claude"), 0o700);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_missing_store_is_no_store() {
        let read = read_provider(Path::new("/nonexistent/tg-store"), "claude");
        assert!(read.entries.is_empty() && read.skipped.is_empty() && read.refused.is_none());
    }

    #[test]
    fn the_live_login_is_found_by_token_before_identity() {
        let entries = [
            entry("perso", Some("u-2"), "a-2", "r-2"),
            entry("work", Some("u-1"), "a-1", "r-1"),
        ];
        // The sidecar says u-2, the tokens say work: tokens are proof.
        let found = resolve(Some(&live("a-1", "r-1", Some("u-2"))), &entries, tokens_of);
        assert_eq!(found.live, Some(1));
        assert_eq!(found.others, [0]);

        // Rotated since the switcher's last copy: identity decides.
        let found = resolve(Some(&live("a-9", "r-9", Some("u-1"))), &entries, tokens_of);
        assert_eq!(found.live, Some(1));

        // Nothing matches: every entry is read on its own.
        let found = resolve(Some(&live("a-9", "r-9", None)), &entries, tokens_of);
        assert_eq!(found.live, None);
        assert_eq!(found.others, [0, 1]);
    }

    #[test]
    fn an_unverified_entry_is_never_matched_by_identity() {
        let entries = [entry("work", None, "a-1", "r-1")];
        let found = resolve(Some(&live("a-9", "r-9", Some("u-1"))), &entries, tokens_of);
        assert_eq!(found.live, None);
        // Its own tokens are still proof of which login it is.
        let found = resolve(Some(&live("a-1", "r-9", None)), &entries, tokens_of);
        assert_eq!(found.live, Some(0));
    }

    #[test]
    fn two_entries_for_one_account_are_read_once() {
        let entries = [
            entry("a-copy", Some("u-1"), "a-1", "r-1"),
            entry("perso", Some("u-2"), "a-2", "r-2"),
            entry("work", Some("u-1"), "a-3", "r-3"),
        ];
        let found = resolve(None, &entries, tokens_of);
        assert_eq!(found.others, [0, 1]);
        assert_eq!(
            found.duplicates,
            [("work".to_string(), "the same account as a-copy".to_string())]
        );

        // The one the live login is stands for the account.
        let found = resolve(Some(&live("a-3", "r-3", None)), &entries, tokens_of);
        assert_eq!(found.live, Some(2));
        assert_eq!(found.others, [1]);
        assert_eq!(found.duplicates[0].0, "a-copy");
    }

    fn claude_creds(expires_ms: i64) -> String {
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"tg-test-{expires_ms}","refreshToken":"tg-test-r-{expires_ms}","expiresAt":{expires_ms},"scopes":["user:profile"]}}}}"#
        )
    }

    /// Validated, not statted: an expired or unverified credential is a red
    /// line, and one the next fetch would ask about is a green one.
    #[test]
    fn the_doctor_validates_every_stored_credential() {
        let store = temp_store("doctor");
        let now = Utc::now();
        let soon = now.timestamp_millis() + 3_600_000;
        let gone = now.timestamp_millis() - 1;
        put(&store, "claude", "fine", &claude_creds(soon), "tg-test-u-1");
        put(
            &store,
            "claude",
            "stale",
            &claude_creds(gone),
            "tg-test-u-2",
        );
        put(
            &store,
            "claude",
            "moved",
            &claude_creds(soon + 1),
            "tg-test-u-3",
        );
        let moved = store.join("claude/moved.json");
        std::fs::write(&moved, claude_creds(soon + 2)).unwrap();
        set_mode(&moved, 0o600);
        put(
            &store,
            "claude",
            "twin",
            &claude_creds(soon + 3),
            "tg-test-u-1",
        );

        let mut config = TokenGaugeConfig::default();
        config.credentials.store = store.clone();
        config.providers.codex = Some(false);
        let checks = doctor_checks(&config, now);
        let line = |name: &str| {
            checks
                .iter()
                .find(|c| c.label == format!("claude/{name}"))
                .unwrap_or_else(|| panic!("no line for {name}"))
        };
        assert!(line("fine").ok, "{}", line("fine").detail);
        assert!(!line("stale").ok && line("stale").detail.contains("expired"));
        assert!(!line("moved").ok && line("moved").detail.contains("unverified"));
        assert!(!line("twin").ok && line("twin").detail.contains("same account as fine"));
        assert!(
            checks.iter().all(|c| !c.detail.contains("tg-test-")),
            "a token reached the report"
        );

        config.credentials.store = PathBuf::new();
        assert!(doctor_checks(&config, now)[0].detail.starts_with("off"));
        config.credentials.store = store.join("absent");
        let absent = doctor_checks(&config, now);
        assert!(absent[0].ok && absent.len() == 1);
        let _ = std::fs::remove_dir_all(&store);
    }
}
