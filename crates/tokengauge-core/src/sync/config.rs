//! `[sync]`, and the writers the setup screen drives.
//!
//! Config types live with the feature that owns them rather than in `lib.rs`.
//! `lib.rs` re-exports them so `TokenGaugeConfig` reads unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::{NATIVELY_READ, edit_config_file, ensure_table, sync};

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
            NATIVELY_READ.join(", ")
        ));
    }
    let name = name.to_lowercase();
    edit_config_file(path, |doc| {
        let sync = ensure_table(doc, "sync");
        ensure_subtable(sync, "providers")[&name] = toml_edit::value(enabled);
    })
}
