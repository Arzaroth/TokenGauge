//! What another machine needs to join this fleet on its own, as one document:
//! `--sync-export` prints it and `--sync-join -` takes it in place of a bare
//! key. It never carries the device id, which would make two machines write one
//! object, nor `label`, which names a device.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::crypto::{self, FleetKey};
use super::s3::{self, S3Credentials};
use crate::{SyncTransportKind, TokenGaugeConfig};

pub const BUNDLE_FORMAT: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BundleS3 {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
}

/// Holds the fleet key, so it has no `Debug` that could reach a log.
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncBundle {
    pub tokengauge_sync: u32,
    pub key: String,
    pub enabled: bool,
    pub transport: SyncTransportKind,
    pub retention_days: u32,
    pub peer_max_age_days: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, bool>,
    /// `~/` stands for the home directory, which is rarely the same path on
    /// the machine joining.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<BundleS3>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3_credentials: Option<S3Credentials>,
}

pub struct Joined {
    pub key_id: String,
    pub key_path: PathBuf,
    pub credentials_path: Option<PathBuf>,
}

fn home_relative(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

pub fn export(config: &TokenGaugeConfig) -> Result<SyncBundle> {
    let key = crypto::load_key(&config.cache_file)?
        .context("no fleet key on this machine; run `--sync-init` first")?;
    let sync = &config.sync;
    let mut providers = BTreeMap::new();
    for (name, set) in [
        ("claude", sync.providers.claude),
        ("codex", sync.providers.codex),
    ] {
        if let Some(on) = set {
            providers.insert(name.to_string(), on);
        }
    }
    for (name, value) in &sync.providers.unknown {
        if let Some(on) = value.as_bool() {
            providers.insert(name.to_lowercase(), on);
        }
    }
    let dir = (!sync.dir.path.as_os_str().is_empty()).then(|| home_relative(&sync.dir.path));
    let place = BundleS3 {
        endpoint: sync.s3.endpoint.trim().to_string(),
        region: sync.s3.region.trim().to_string(),
        bucket: sync.s3.bucket.trim().to_string(),
        prefix: sync.s3.prefix.trim().to_string(),
    };
    let s3_credentials = match s3::resolve_credentials(&sync.s3, &config.cache_file)? {
        (Some(access_key_id), Some(secret_access_key))
            if sync.transport == SyncTransportKind::S3 =>
        {
            Some(S3Credentials {
                access_key_id,
                secret_access_key,
            })
        }
        _ => None,
    };
    Ok(SyncBundle {
        tokengauge_sync: BUNDLE_FORMAT,
        key: key.display(),
        enabled: sync.enabled,
        transport: sync.transport,
        retention_days: sync.retention_days,
        peer_max_age_days: sync.peer_max_age_days,
        providers,
        dir,
        s3: (place != BundleS3::default()).then_some(place),
        s3_credentials,
    })
}

/// A bundle when `text` is one, `None` when it reads as a bare key.
pub fn parse(text: &str) -> Option<Result<SyncBundle>> {
    let text = text.trim();
    if !text.starts_with('{') {
        return None;
    }
    Some(
        serde_json::from_str::<SyncBundle>(text)
            .context("not a fleet bundle `--sync-export` printed")
            .and_then(|bundle| {
                if bundle.tokengauge_sync != BUNDLE_FORMAT {
                    bail!(
                        "this bundle is format {}, and this TokenGauge reads format {BUNDLE_FORMAT}; update the older side",
                        bundle.tokengauge_sync
                    );
                }
                Ok(bundle)
            }),
    )
}

/// The key goes first, refused when this machine already belongs to another
/// fleet unless `force`, so a refusal writes nothing else.
pub fn join(
    config: &TokenGaugeConfig,
    config_path: &Path,
    bundle: &SyncBundle,
    force: bool,
) -> Result<Joined> {
    let key = FleetKey::parse(&bundle.key)?;
    let key_path = crypto::store_key(&config.cache_file, &key, force)?;
    super::config::config_join_sync(config_path, bundle)?;
    let credentials_path = bundle
        .s3_credentials
        .as_ref()
        .map(|credentials| s3::store_credentials(&config.cache_file, credentials))
        .transpose()?;
    Ok(Joined {
        key_id: key.id_hex(),
        key_path,
        credentials_path,
    })
}
