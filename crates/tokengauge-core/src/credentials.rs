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
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::doctor::DoctorCheck;
use crate::{CredentialState, ProviderFetchError, ProviderPayload, TokenGaugeConfig};

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
    /// A short digest of the sidecar's identity, for telling whether a name
    /// still holds the account it did without writing the account down.
    pub fn account_digest(&self) -> Option<String> {
        self.account_id
            .as_deref()
            .map(|id| hex_sha256(id.as_bytes())[..16].to_string())
    }

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
        if let Err(why) = private(path, Guard::Writes) {
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
    private(&path, Guard::Reads)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read it: {e}"))?;
    let digest = hex_sha256(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| "it is not UTF-8".to_string())?;
    if serde_json::from_str::<serde_json::Value>(&text).is_err() {
        return Err("it is not valid JSON".to_string());
    }
    let meta_path = dir.join(format!("{name}.meta.json"));
    // The sidecar is where identity and label come from: anyone who can write
    // it can make an entry answer for another account.
    if meta_path.exists() {
        private(&meta_path, Guard::Writes)?;
    }
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

/// What a path must keep others from doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// Nobody else may write it: the store's directories and the sidecars.
    Writes,
    /// Nobody else may touch it at all: a credential file.
    Reads,
}

/// A store path must be the user's, and closed to others as far as `guard`
/// says.
///
/// "The user" is the owner of the home directory: this crate has no `unsafe`,
/// and that is the one uid it can read without asking the kernel for its own.
/// With no home directory to ask, nothing is the user's.
#[cfg(unix)]
fn private(path: &Path, guard: Guard) -> std::result::Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let owner = dirs::home_dir()
        .and_then(|home| std::fs::metadata(home).ok())
        .map(|home| home.uid());
    if owner != Some(meta.uid()) {
        return Err(format!("{} does not belong to you", path.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if guard == Guard::Writes && mode & 0o022 != 0 {
        return Err(format!(
            "{} is writable by others (mode {mode:o})",
            path.display()
        ));
    }
    if guard == Guard::Reads && mode & 0o077 != 0 {
        return Err(format!(
            "{} is open to others (mode {mode:o}); it should be 600",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private(_path: &Path, _guard: Guard) -> std::result::Result<(), String> {
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
    /// Ask about a stored credential: a payload with usage, or one in a
    /// [`CredentialState`] when it is not worth asking.
    pub fetch: fn(&str, Duration, DateTime<Utc>) -> Result<ProviderPayload>,
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
// Fetching
// ---------------------------------------------------------------------------

/// Everything a provider's credentials say: the live login as it always was,
/// then every stored credential it is not.
///
/// The live login is asked on every refresh. An inactive credential is asked
/// only once its last payload is older than `inactive_refresh_secs` or a
/// window it reported has reset since; otherwise that payload is carried
/// unchanged, its own `updatedAt` and all. Each one that is asked gets its own
/// thread, spaced by `stagger_ms`, so a store of several credentials costs one
/// request's time rather than their sum.
pub(crate) fn fetch_provider(
    provider: &'static str,
    reader: &StoreReader,
    config: &TokenGaugeConfig,
    previous: &[ProviderPayload],
    live_fetch: impl FnOnce() -> Result<Vec<ProviderPayload>>,
) -> (Vec<ProviderPayload>, Vec<ProviderFetchError>) {
    let now = Utc::now();
    let timeout = Duration::from_secs(config.timeout_secs);
    let read = config
        .credentials
        .store_root()
        .map(|store| read_provider(&store, provider))
        .unwrap_or_default();
    // No store, no reason to read the live login twice: on macOS each read can
    // be a keychain prompt.
    let live = if read.entries.is_empty() {
        LiveLogin::default()
    } else {
        (reader.live)()
    };
    let resolved = resolve(
        (live != LiveLogin::default()).then_some(&live),
        &read.entries,
        reader.tokens,
    );
    let matched = resolved.live.map(|i| &read.entries[i]);

    // The stored credentials are asked while the live login is, so a store of
    // several costs one request's time rather than two.
    let max_age = i64::try_from(config.credentials.inactive_refresh_secs)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .unwrap_or(chrono::Duration::MAX);
    let mut slots: Vec<Option<ProviderPayload>> = Vec::new();
    let mut asks = Vec::new();
    for &i in &resolved.others {
        let entry = &read.entries[i];
        if !entry.verified() {
            let mut payload =
                ProviderPayload::live(provider, "store", crate::UsageSnapshot::at(now));
            payload.credential.state = Some(CredentialState::Unverified);
            slots.push(Some(stored(payload, entry)));
            continue;
        }
        if let Some(payload) = carried(provider, entry, previous, now, max_age) {
            slots.push(Some(stored(payload, entry)));
            continue;
        }
        let stagger = Duration::from_millis(config.stagger_ms).saturating_mul(asks.len() as u32);
        let (text, fetch) = (entry.text.clone(), reader.fetch);
        asks.push((
            slots.len(),
            i,
            std::thread::spawn(move || {
                if !stagger.is_zero() {
                    std::thread::sleep(stagger);
                }
                fetch(&text, timeout, now)
            }),
        ));
        slots.push(None);
    }

    let (mut payloads, mut errors) = crate::fetch::settle(provider, live_fetch());
    // A live login that matches nothing may still be one of the stored
    // credentials read beside it. With no identity to rule that out, or an
    // unverified entry it could be, it stays out of the combined figure.
    let may_be_stored = matched.is_none()
        && !read.entries.is_empty()
        && (live.account.is_none() || read.entries.iter().any(|e| !e.verified()));
    for payload in &mut payloads {
        payload.credential.active = Some(true);
        payload.credential.name = matched.map(|e| e.name.clone());
        payload.credential.label = matched.and_then(|e| e.label.clone());
        payload.credential.account_digest = matched.and_then(StoredCredential::account_digest);
        if may_be_stored {
            payload.credential.plan_weight = None;
        }
    }
    for error in &mut errors {
        error.active = true;
        error.credential = matched.map(|e| e.name.clone());
    }

    for (slot, i, handle) in asks {
        let entry = &read.entries[i];
        match handle
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("thread panicked")))
        {
            Ok(payload) => slots[slot] = Some(stored(payload, entry)),
            Err(e) => {
                // Named in the message itself: every frontend already draws an
                // error as provider and message, and "Claude: 401" beside a
                // healthy panel reads as the live login failing.
                let mut error = ProviderFetchError::new(
                    provider.to_string(),
                    &format!("{}: {e:#}", entry.name),
                );
                error.credential = Some(entry.name.clone());
                errors.push(error);
            }
        }
    }
    payloads.extend(slots.into_iter().flatten());
    (payloads, errors)
}

/// A payload made a stored credential's.
fn stored(mut payload: ProviderPayload, entry: &StoredCredential) -> ProviderPayload {
    payload.credential.name = Some(entry.name.clone());
    payload.credential.active = Some(false);
    payload.credential.label = entry.label.clone();
    payload.credential.account_digest = entry.account_digest();
    payload
}

/// The last payload a stored credential produced, when it is still worth
/// serving: asked within `max_age`, a live answer rather than a fallback, and
/// with no window that has reset since it was asked.
///
/// And only while the name still holds the same account: a name re-captured
/// for another account would otherwise carry the old one's figures.
fn carried(
    provider: &str,
    entry: &StoredCredential,
    previous: &[ProviderPayload],
    now: DateTime<Utc>,
    max_age: chrono::Duration,
) -> Option<ProviderPayload> {
    let payload = previous.iter().find(|p| {
        p.provider.eq_ignore_ascii_case(provider)
            && p.credential.name.as_deref() == Some(entry.name.as_str())
            && p.credential.account_digest.is_some()
            && p.credential.account_digest == entry.account_digest()
            && !p.stale
            && !p.has_error()
            && p.credential.state.is_none()
    })?;
    let usage = payload.usage.as_ref()?;
    let asked = DateTime::parse_from_rfc3339(usage.updated_at.as_deref()?)
        .ok()?
        .with_timezone(&Utc);
    if now - asked >= max_age || asked > now {
        return None;
    }
    let reset_since = crate::snapshot::rolled_over(std::slice::from_ref(payload), asked, now);
    (!reset_since).then(|| payload.clone())
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
        let read = read_provider(&store, provider);
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
        let meta_path = dir.join(format!("{name}.meta.json"));
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
        set_mode(&meta_path, 0o600);
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
        let _ = std::fs::remove_file(store.join("claude/open.json"));

        put(&store, "claude", "forged", "{}", "u-3");
        set_mode(&store.join("claude/forged.meta.json"), 0o666);
        let read = read_provider(&store, "claude");
        assert!(
            read.skipped
                .iter()
                .any(|(n, why)| n == "forged" && why.contains("writable by others")),
            "{read:?}"
        );

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

    // -----------------------------------------------------------------------
    // fetch_provider
    // -----------------------------------------------------------------------

    fn fake_live() -> LiveLogin {
        live("a-work", "r-work", Some("u-work"))
    }

    /// Answers from the credential's own text: `"fail"` errors, `"expired"`
    /// comes back in that state, anything else is asked and answers `fetched`.
    fn fake_fetch(text: &str, _: Duration, now: DateTime<Utc>) -> Result<ProviderPayload> {
        let access = tokens_of(text).access.unwrap_or_default();
        if access == "fail" {
            anyhow::bail!("Claude rate-limited - try again shortly");
        }
        let mut payload = ProviderPayload::live("claude", "fetched", crate::UsageSnapshot::at(now));
        if access == "expired" {
            payload.credential.state = Some(CredentialState::Expired);
        }
        payload.credential.plan_weight = Some(5);
        Ok(payload)
    }

    fn fake_check(_: &str, _: DateTime<Utc>) -> Result<Option<CredentialState>> {
        Ok(None)
    }

    const FAKE: StoreReader = StoreReader {
        live: fake_live,
        tokens: tokens_of,
        fetch: fake_fetch,
        check: fake_check,
    };

    fn live_payload() -> Result<Vec<ProviderPayload>> {
        let mut payload =
            ProviderPayload::live("claude", "oauth", crate::UsageSnapshot::at(Utc::now()));
        payload.credential.plan_weight = Some(20);
        Ok(vec![payload])
    }

    fn store_with_every_kind(tag: &str) -> (PathBuf, TokenGaugeConfig) {
        let store = temp_store(tag);
        put(
            &store,
            "claude",
            "work",
            &creds("a-work", "r-work"),
            "u-work",
        );
        put(
            &store,
            "claude",
            "perso",
            &creds("a-perso", "r-perso"),
            "u-perso",
        );
        put(
            &store,
            "claude",
            "broken",
            &creds("fail", "r-fail"),
            "u-broken",
        );
        put(&store, "claude", "old", &creds("expired", "r-old"), "u-old");
        put(
            &store,
            "claude",
            "moved",
            &creds("a-moved", "r-moved"),
            "u-moved",
        );
        let moved = store.join("claude/moved.json");
        std::fs::write(&moved, creds("a-moved-2", "r-moved-2")).unwrap();
        set_mode(&moved, 0o600);
        let mut config = TokenGaugeConfig::default();
        config.credentials.store = store.clone();
        config.timeout_secs = 1;
        (store, config)
    }

    fn named<'a>(payloads: &'a [ProviderPayload], name: &str) -> &'a ProviderPayload {
        payloads
            .iter()
            .find(|p| p.credential.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no payload for {name}"))
    }

    #[test]
    fn every_stored_credential_comes_through_named_and_in_its_state() {
        let (store, config) = store_with_every_kind("fetch");
        let (payloads, errors) = fetch_provider("claude", &FAKE, &config, &[], live_payload);

        // The live login is work's, and work's stored copy is never asked.
        assert_eq!(payloads[0].credential.name.as_deref(), Some("work"));
        assert_eq!(payloads[0].credential.active, Some(true));
        assert_eq!(payloads[0].credential.label.as_deref(), Some("work label"));
        assert_eq!(payloads[0].source.as_deref(), Some("oauth"));
        assert_eq!(
            payloads
                .iter()
                .filter(|p| p.credential.name.as_deref() == Some("work"))
                .count(),
            1
        );

        let perso = named(&payloads, "perso");
        assert_eq!(perso.credential.active, Some(false));
        assert_eq!(perso.source.as_deref(), Some("fetched"));
        assert_eq!(
            named(&payloads, "old").credential.state,
            Some(CredentialState::Expired)
        );
        let moved = named(&payloads, "moved");
        assert_eq!(moved.credential.state, Some(CredentialState::Unverified));
        assert_eq!(moved.credential.label, None);

        // A failure is an error attributed to its credential, not a payload.
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].credential.as_deref(), Some("broken"));
        assert!(
            errors[0].message.starts_with("broken: "),
            "{}",
            errors[0].message
        );
        assert!(!errors[0].active);
        // Store order after the live login, whatever order the threads end in.
        let order: Vec<_> = payloads
            .iter()
            .filter_map(|p| p.credential.name.as_deref())
            .collect();
        assert_eq!(order, ["work", "moved", "old", "perso"]);
        let _ = std::fs::remove_dir_all(&store);
    }

    /// Asked recently and nothing has reset since: carried, not asked again.
    /// A window that has reset since makes the carried figures wrong however
    /// young they are.
    #[test]
    fn an_inactive_credential_is_carried_until_it_is_due() {
        let (store, config) = store_with_every_kind("carry");
        let now = Utc::now();
        let mut recent = ProviderPayload::live(
            "claude",
            "carried",
            crate::UsageSnapshot {
                primary: Some(crate::UsageWindow {
                    used_percent: Some(40),
                    reset_description: None,
                    resets_at: Some((now + chrono::Duration::hours(2)).to_rfc3339()),
                    window_minutes: Some(300),
                }),
                ..crate::UsageSnapshot::at(now - chrono::Duration::minutes(5))
            },
        );
        recent.credential.name = Some("perso".into());
        recent.credential.active = Some(false);
        recent.credential.account_digest = Some(hex_sha256(b"u-perso")[..16].to_string());

        let (payloads, _) =
            fetch_provider("claude", &FAKE, &config, &[recent.clone()], live_payload);
        assert_eq!(named(&payloads, "perso").source.as_deref(), Some("carried"));

        // The same name, re-captured for another account, is asked afresh.
        let mut other = recent.clone();
        other.credential.account_digest = Some(hex_sha256(b"u-someone-else")[..16].to_string());
        let (payloads, _) = fetch_provider("claude", &FAKE, &config, &[other], live_payload);
        assert_eq!(named(&payloads, "perso").source.as_deref(), Some("fetched"));

        let mut rolled = recent.clone();
        rolled
            .usage
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap()
            .resets_at = Some((now - chrono::Duration::minutes(1)).to_rfc3339());
        let (payloads, _) = fetch_provider("claude", &FAKE, &config, &[rolled], live_payload);
        assert_eq!(named(&payloads, "perso").source.as_deref(), Some("fetched"));

        let mut due = recent;
        due.usage.as_mut().unwrap().updated_at =
            Some((now - chrono::Duration::hours(1)).to_rfc3339());
        let (payloads, _) = fetch_provider("claude", &FAKE, &config, &[due], live_payload);
        assert_eq!(named(&payloads, "perso").source.as_deref(), Some("fetched"));
        let _ = std::fs::remove_dir_all(&store);
    }

    /// A live login that matches nothing, with no identity to tell it from the
    /// stored ones being read beside it, may be one of them: it stays out of
    /// the combined figure rather than counting a plan twice.
    #[test]
    fn a_live_login_that_matches_nothing_is_not_counted_twice() {
        fn anonymous() -> LiveLogin {
            live("a-env", "r-env", None)
        }
        let reader = StoreReader {
            live: anonymous,
            ..FAKE
        };
        let (store, config) = store_with_every_kind("unmatched");
        let (payloads, _) = fetch_provider("claude", &reader, &config, &[], live_payload);
        let live = &payloads[0];
        assert_eq!(live.credential.active, Some(true));
        assert_eq!(live.credential.name, None);
        assert_eq!(live.credential.plan_weight, None);
        // Every stored credential is read, work included.
        assert_eq!(named(&payloads, "work").credential.active, Some(false));
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn with_no_store_the_live_login_is_still_marked_active() {
        let config = TokenGaugeConfig {
            credentials: crate::CredentialsConfig::off(),
            ..TokenGaugeConfig::default()
        };
        let (payloads, errors) = fetch_provider("claude", &FAKE, &config, &[], live_payload);
        assert_eq!(payloads.len(), 1);
        assert!(errors.is_empty());
        assert_eq!(payloads[0].credential.active, Some(true));
        assert_eq!(payloads[0].credential.name, None);
        assert_eq!(payloads[0].credential.plan_weight, Some(20));
    }
}
