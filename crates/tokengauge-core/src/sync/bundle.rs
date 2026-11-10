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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tg-bundle-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn machine(dir: &Path, sync: &str) -> (TokenGaugeConfig, PathBuf) {
        let path = dir.join("config.toml");
        let text = format!(
            "cache_file = '{}'\n{sync}",
            dir.join("cache.json").display()
        );
        std::fs::write(&path, &text).expect("write config");
        (toml::from_str(&text).expect("config parses"), path)
    }

    fn reload(path: &Path) -> TokenGaugeConfig {
        toml::from_str(&std::fs::read_to_string(path).expect("read")).expect("config parses")
    }

    const SOURCE: &str = r#"
[sync]
enabled = true
transport = "s3"
label = "laptop"
retention_days = 20
[sync.providers]
codex = false
[sync.s3]
endpoint = "https://s3.example.com"
bucket = "fleet"
prefix = "tg"
"#;

    fn exported(tag: &str) -> (SyncBundle, FleetKey) {
        let dir = scratch(tag);
        let (config, _) = machine(&dir, SOURCE);
        let key = FleetKey::generate();
        crypto::store_key(&config.cache_file, &key, false).expect("key");
        s3::store_credentials(
            &config.cache_file,
            &S3Credentials {
                access_key_id: "AKID".into(),
                secret_access_key: "SECRET".into(),
            },
        )
        .expect("credentials");
        (export(&config).expect("export"), key)
    }

    #[test]
    fn a_bundle_carries_the_fleet_and_its_keys_but_never_the_device() {
        let (bundle, key) = exported("export");
        let json = serde_json::to_value(&bundle).expect("json");

        assert_eq!(json["key"], key.display());
        assert_eq!(json["transport"], "s3");
        assert_eq!(json["retention_days"], 20);
        assert_eq!(json["providers"], serde_json::json!({"codex": false}));
        assert_eq!(json["s3"]["bucket"], "fleet");
        assert_eq!(json["s3_credentials"]["access_key_id"], "AKID");
        let text = json.to_string();
        assert!(!text.contains("laptop"), "{text}");
        assert!(!text.contains("device"), "{text}");
    }

    #[test]
    fn a_folder_under_home_travels_as_a_tilde_path() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        assert_eq!(home_relative(&home.join("Sync/tg")), "~/Sync/tg");
        assert_eq!(home_relative(&home), "~");
        assert_eq!(home_relative(Path::new("/srv/fleet")), "/srv/fleet");
    }

    #[test]
    fn joining_writes_the_key_the_transport_and_the_keys_and_keeps_the_label() {
        let (bundle, key) = exported("join-src");
        let dir = scratch("join-dst");
        let (config, path) = machine(&dir, "[sync]\nlabel = \"desktop\"\n");

        let joined = join(&config, &path, &bundle, false).expect("join");

        assert_eq!(joined.key_id, key.id_hex());
        let stored = crypto::load_key(&config.cache_file)
            .expect("read")
            .expect("key");
        assert_eq!(stored.id_hex(), key.id_hex());
        let sync = reload(&path).sync;
        assert!(sync.enabled);
        assert_eq!(sync.transport, SyncTransportKind::S3);
        assert_eq!(sync.label, "desktop");
        assert_eq!(sync.retention_days, 20);
        assert_eq!(sync.providers.codex, Some(false));
        assert_eq!(sync.s3.bucket, "fleet");
        assert_eq!(sync.s3.prefix, "tg");
        assert!(sync.s3.access_key_id.is_empty());
        let credentials = s3::load_credentials(&config.cache_file)
            .expect("read")
            .expect("written");
        assert_eq!(credentials.secret_access_key, "SECRET");
        assert_eq!(
            joined.credentials_path,
            Some(s3::credentials_path(&config.cache_file))
        );
        assert!(!dir.join("tokengauge-device-id").exists());
    }

    #[test]
    fn a_machine_in_another_fleet_is_refused_before_anything_is_written() {
        let (bundle, _) = exported("other-src");
        let dir = scratch("other-dst");
        let before = "[sync]\nlabel = \"desktop\"\n";
        let (config, path) = machine(&dir, before);
        let theirs = FleetKey::generate();
        crypto::store_key(&config.cache_file, &theirs, false).expect("key");

        let error = join(&config, &path, &bundle, false).err().expect("refused");
        assert!(error.to_string().contains("--sync-force"), "{error}");
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .ends_with(before)
        );
        assert!(
            s3::load_credentials(&config.cache_file)
                .expect("read")
                .is_none()
        );

        join(&config, &path, &bundle, true).expect("forced");
        assert_eq!(reload(&path).sync.transport, SyncTransportKind::S3);
    }

    #[test]
    fn a_bare_key_is_not_a_bundle_and_another_format_is_refused() {
        assert!(parse(&FleetKey::generate().display()).is_none());
        let (bundle, _) = exported("format");
        let mut json = serde_json::to_value(&bundle).expect("json");
        assert!(parse(&json.to_string()).expect("a bundle").is_ok());
        json["tokengauge_sync"] = 2.into();
        let error = parse(&json.to_string())
            .expect("a bundle")
            .err()
            .expect("refused");
        assert!(error.to_string().contains("format 2"), "{error}");
        assert!(parse("{nonsense").expect("looks like one").is_err());
    }

    #[test]
    fn a_folder_fleet_carries_no_s3_keys() {
        let dir = scratch("folder");
        let (config, _) = machine(
            &dir,
            "[sync]\nenabled = true\ntransport = \"dir\"\n[sync.dir]\npath = \"/srv/fleet\"\n",
        );
        crypto::store_key(&config.cache_file, &FleetKey::generate(), false).expect("key");
        let bundle = export(&config).expect("export");
        assert_eq!(bundle.dir.as_deref(), Some("/srv/fleet"));
        assert!(bundle.s3.is_none());
        assert!(bundle.s3_credentials.is_none());
    }

    #[test]
    fn there_is_nothing_to_export_without_a_fleet_key() {
        let dir = scratch("nokey");
        let (config, _) = machine(&dir, SOURCE);
        let error = export(&config).err().expect("no key");
        assert!(error.to_string().contains("--sync-init"), "{error}");
    }
}
