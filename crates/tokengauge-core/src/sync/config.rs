//! `[sync]`, and the writers the setup screen drives.
//!
//! Config types live with the feature that owns them rather than in `lib.rs`.
//! `lib.rs` re-exports them so `TokenGaugeConfig` reads unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::{edit_config_file, ensure_table, natively_read, sync};

/// Which providers take part in fleet sync. The default is every enabled
/// provider that *can*: a provider read through ccusage has a `CostInfo` and no
/// usage events under it, so there is nothing to bucket. Turn one off when its
/// transcript tree is itself synced between machines, or both machines will
/// count it.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncProvidersConfig {
    pub claude: Option<bool>,
    pub codex: Option<bool>,
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl SyncProvidersConfig {
    pub fn resolve(&self, enabled: &[&str]) -> Vec<String> {
        enabled
            .iter()
            .filter(|name| sync::syncable(name))
            // Anything else that gains a transcript reader syncs by default
            // rather than waiting for someone to remember this struct. The
            // named fields are overrides, not an allow-list.
            .filter(|name| match name.to_ascii_lowercase().as_str() {
                "claude" => self.claude.unwrap_or(true),
                "codex" => self.codex.unwrap_or(true),
                _ => true,
            })
            .map(|name| name.to_lowercase())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncTransportKind {
    /// A folder the user already syncs: Syncthing, Dropbox, Nextcloud, a NAS.
    #[default]
    Dir,
    /// Any S3-compatible bucket: S3, R2, B2, MinIO, Garage.
    S3,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncDirConfig {
    pub path: PathBuf,
    /// See [`SyncConfig::unknown`]. A typo here means sync quietly does not
    /// work, so it has to be reportable rather than dropped.
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct SyncS3Config {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    /// Credentials belong in the environment; these exist for a machine where
    /// that is awkward. They are never written into the snapshot or logged.
    pub access_key_id: String,
    pub secret_access_key: String,
    /// See [`SyncConfig::unknown`].
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SyncConfig {
    pub enabled: bool,
    pub transport: SyncTransportKind,
    /// This machine's name in the by-device rows. Empty falls back to the
    /// hostname.
    pub label: String,
    /// Days of buckets a contribution carries. The local store keeps far more,
    /// because it is the only record left once a CLI rotates a transcript away.
    pub retention_days: u32,
    /// A device silent this long is reported as quiet by `--sync-status` and
    /// `--doctor`. It does not stop counting: its past days really did happen,
    /// and a machine with no tokens in the period shown is already absent from
    /// the by-device rows without needing a rule.
    pub peer_max_age_days: u32,
    pub providers: SyncProvidersConfig,
    pub dir: SyncDirConfig,
    pub s3: SyncS3Config,
    #[serde(flatten)]
    pub unknown: HashMap<String, toml::Value>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: SyncTransportKind::default(),
            label: String::new(),
            retention_days: sync::WIRE_RETENTION_DAYS as u32,
            peer_max_age_days: 30,
            providers: SyncProvidersConfig::default(),
            dir: SyncDirConfig::default(),
            s3: SyncS3Config::default(),
            unknown: HashMap::new(),
        }
    }
}

fn ensure_subtable<'a>(table: &'a mut toml_edit::Table, key: &str) -> &'a mut toml_edit::Table {
    if table.get(key).and_then(|i| i.as_table()).is_none() {
        let replacement = table
            .get(key)
            .and_then(|i| i.as_inline_table())
            .cloned()
            .map(|t| toml_edit::Item::Table(t.into_table()))
            .unwrap_or_else(|| toml_edit::Item::Table(toml_edit::Table::new()));
        table.insert(key, replacement);
    }
    table[key].as_table_mut().expect("just ensured table")
}

/// Turn fleet sync on or off.
pub fn config_set_sync_enabled(path: &Path, enabled: bool) -> Result<()> {
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["enabled"] = toml_edit::value(enabled);
    })
}

pub fn config_set_sync_label(path: &Path, label: &str) -> Result<()> {
    let label = label.to_string();
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["label"] = toml_edit::value(label.as_str());
    })
}

pub fn config_set_sync_transport(path: &Path, kind: &str) -> Result<()> {
    let kind = match kind.to_ascii_lowercase().as_str() {
        "dir" => "dir",
        "s3" => "s3",
        other => {
            return Err(anyhow!(
                "unknown sync transport '{other}' (expected dir or s3)"
            ));
        }
    };
    edit_config_file(path, |doc| {
        ensure_table(doc, "sync")["transport"] = toml_edit::value(kind);
    })
}

/// Point the folder transport at a directory the user's sync tool handles.
pub fn config_set_sync_dir(path: &Path, dir: &str) -> Result<()> {
    let dir = dir.trim().to_string();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "dir")["path"] = toml_edit::value(dir.as_str());
    })
}

/// Set one `[sync.s3]` field.
///
/// Credentials are deliberately not settable here: they belong in the
/// environment, not written into a config file by a setup screen.
pub fn config_set_sync_s3(path: &Path, field: &str, value: &str) -> Result<()> {
    const FIELDS: &[&str] = &["endpoint", "region", "bucket", "prefix"];
    if !FIELDS.contains(&field) {
        return Err(anyhow!(
            "'{field}' is not a settable S3 field ({}); credentials come from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
            FIELDS.join(", ")
        ));
    }
    let field = field.to_string();
    let value = value.trim().to_string();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "s3")[&field] = toml_edit::value(value.as_str());
    })
}

/// Take one provider in or out of sync. Only providers with a native reader can
/// take part: a ccusage-sourced provider has no events to bucket.
pub fn config_set_sync_provider(path: &Path, name: &str, enabled: bool) -> Result<()> {
    if !sync::syncable(name) {
        return Err(anyhow!(
            "'{name}' has no transcript reader, so it cannot sync (it can be one of: {})",
            natively_read().join(", ")
        ));
    }
    let name = name.to_lowercase();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "providers")[&name] = toml_edit::value(enabled);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(tag: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-sync-config-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("config.toml");
        std::fs::write(&path, contents).expect("write");
        path
    }

    /// Parsed rather than loaded: `load_config` runs `migrate_legacy_state`,
    /// which moves state files out of the system temp directory - where these
    /// fixtures live - and into whatever `cache_file` resolves to.
    fn reload(path: &Path) -> SyncConfig {
        let text = std::fs::read_to_string(path).expect("read");
        toml::from_str::<crate::TokenGaugeConfig>(&text)
            .expect("the setup screen must leave a config that still parses")
            .sync
    }

    /// Everything the setup screen can change goes through these, and each one
    /// has to survive being read back - a writer that produces a config the
    /// loader then rejects breaks the whole file, not just its own field.
    #[test]
    fn every_field_the_setup_screen_writes_reads_back() {
        let path = config_with("writers", "refresh_secs = 600\n");

        config_set_sync_enabled(&path, true).expect("enabled");
        config_set_sync_label(&path, "laptop").expect("label");
        config_set_sync_transport(&path, "S3").expect("transport is case-folded");
        config_set_sync_dir(&path, "  /srv/fleet  ").expect("dir");
        config_set_sync_s3(&path, "bucket", " tokens ").expect("bucket");
        config_set_sync_s3(&path, "endpoint", "https://s3.example").expect("endpoint");
        config_set_sync_provider(&path, "Claude", false).expect("provider");

        let sync = reload(&path);
        assert!(sync.enabled);
        assert_eq!(sync.label, "laptop");
        assert_eq!(sync.transport, SyncTransportKind::S3);
        assert_eq!(sync.dir.path, PathBuf::from("/srv/fleet"));
        assert_eq!(sync.s3.bucket, "tokens");
        assert_eq!(sync.s3.endpoint, "https://s3.example");
        assert_eq!(sync.providers.claude, Some(false));
        assert!(
            sync.unknown.is_empty() && sync.s3.unknown.is_empty(),
            "a writer that lands a key the loader does not know is a typo the doctor will report"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// A hand-written config may spell a section as an inline table. Replacing
    /// it with an empty one would silently drop whatever else the user had put
    /// in it.
    #[test]
    fn an_inline_section_is_promoted_rather_than_replaced() {
        let path = config_with(
            "inline",
            "[sync]\nenabled = true\nproviders = { claude = false }\n",
        );

        config_set_sync_provider(&path, "codex", false).expect("provider");

        let sync = reload(&path);
        assert_eq!(sync.providers.codex, Some(false));
        assert_eq!(
            sync.providers.claude,
            Some(false),
            "the field that was already in the inline table was dropped"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// The setup screen passes user input straight through, so a value it does
    /// not know has to come back as an error rather than land in the file and
    /// take sync down quietly.
    #[test]
    fn a_value_the_writers_do_not_know_never_reaches_the_file() {
        let path = config_with("refused", "refresh_secs = 600\n");
        let before = std::fs::read_to_string(&path).expect("read");

        let transport = config_set_sync_transport(&path, "ftp").expect_err("unknown transport");
        assert!(transport.to_string().contains("expected dir or s3"));

        let secret = config_set_sync_s3(&path, "secret_access_key", "hunter2")
            .expect_err("credentials are not settable");
        assert!(
            secret.to_string().contains("AWS_SECRET_ACCESS_KEY"),
            "the error has to say where a credential does belong: {secret}"
        );

        let provider =
            config_set_sync_provider(&path, "glm", true).expect_err("glm has no transcript reader");
        assert!(provider.to_string().contains("no transcript reader"));

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            before,
            "a refused write must not have touched the config"
        );

        let _ = std::fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// The named fields are overrides, not an allow-list: a provider that
    /// gains a transcript reader syncs without anyone remembering this struct.
    #[test]
    fn a_provider_with_a_reader_syncs_unless_it_is_turned_off() {
        let all = SyncProvidersConfig::default();
        assert_eq!(
            all.resolve(&["claude", "codex", "kimi", "glm"]),
            vec!["claude", "codex", "kimi"],
            "glm has no reader, so it has nothing to bucket"
        );

        let off = SyncProvidersConfig {
            claude: Some(false),
            ..SyncProvidersConfig::default()
        };
        assert_eq!(off.resolve(&["Claude", "Codex"]), vec!["codex"]);
    }
}
