//! One sync cycle: publish what changed, take what the peers published, merge.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::model::{Contribution, DeviceRecord, FleetStore, Hour, ObjectState, content_hash};
use super::{SCHEMA_VERSION, crypto, store, transport};
use crate::TokenGaugeConfig;
use crate::cost::UsageEvent;

/// What the fleet looked like on the last cycle. Serialised into the snapshot
/// so `--json`, `--sync-status` and the panel all read one struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_id: String,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub devices: Vec<DeviceStatus>,
    #[serde(default)]
    pub last_pull_ms: Option<i64>,
    #[serde(default)]
    pub last_publish_ms: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Objects in the storage this machine could not use, each with the reason.
    #[serde(default)]
    pub skipped: Vec<SkippedObject>,
    /// Devices that read the same transcripts for the same day, which is what a
    /// synced `~/.claude/projects` looks like from here.
    #[serde(default)]
    pub overlaps: Vec<OverlapNote>,
    /// Peers dropped because the fleet key changed. Non-empty only on the cycle
    /// that noticed, so it reads as an event rather than a state.
    #[serde(default)]
    pub dropped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub device_id: String,
    pub label: String,
    pub hostname: String,
    pub updated_at_ms: i64,
    pub is_local: bool,
    /// Silent for longer than `peer_max_age_days`.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedObject {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlapNote {
    pub date: String,
    pub devices: Vec<String>,
    pub kept: String,
}

/// What a cycle produced.
pub struct SyncOutcome {
    /// Peer events for `build_report`, alongside the caller's own.
    pub events: Vec<UsageEvent>,
    pub status: SyncStatus,
    /// The merged store, so per-device totals can be taken against the same
    /// price table the report was rated with.
    pub store: FleetStore,
}

/// Run a cycle and return the peer events `build_report` should see alongside
/// the caller's own.
///
/// Never fails: a transport that is down leaves the durable store in place and
/// the failure in the status, because blanking the totals would be a lie in the
/// other direction.
pub fn refresh(
    config: &TokenGaugeConfig,
    local_events: &[UsageEvent],
    since: NaiveDate,
) -> SyncOutcome {
    let mut status = SyncStatus {
        enabled: config.sync.enabled,
        ..Default::default()
    };
    if !config.sync.enabled {
        return SyncOutcome {
            events: Vec::new(),
            status,
            store: FleetStore::default(),
        };
    }

    let now = Utc::now();
    let from = window_hour(since);
    let identity = crate::device_identity(&config.cache_file);
    let device = DeviceRecord::new(&identity, &config.sync.label);

    let (mut store, store_error) = store::load(&config.cache_file);
    status.error = store_error;
    store.upsert_local(&device, from, local_events, now.timestamp_millis());

    if let Err(e) = cycle(config, &device, &mut store, now, &mut status) {
        // A store that would not parse is the more serious of the two, so it
        // keeps the slot if it already claimed it.
        status.error.get_or_insert(format!("{e:#}"));
    }

    store.prune(now);
    if let Err(e) = store::save(&config.cache_file, &store) {
        status.error.get_or_insert(format!("{e:#}"));
    }

    status.last_pull_ms = store.last_pull_ms;
    status.last_publish_ms = store.last_publish_ms;
    status.devices = device_statuses(&store, &device.id, config, now);
    status.overlaps = store
        .overlaps()
        .into_iter()
        .map(|overlap| OverlapNote {
            date: overlap.date.to_string(),
            devices: overlap
                .devices
                .iter()
                .map(|id| label_for(&store, id))
                .collect(),
            kept: label_for(&store, &overlap.kept),
        })
        .collect();

    SyncOutcome {
        events: store.synthetic_events(&device.id, from),
        status,
        store,
    }
}

/// What the panel should say about sync, or nothing when it is off.
///
/// Ordered worst-first: a transport that is down, then a transcript tree read
/// twice, then objects that could not be used, then staleness, and only then
/// the healthy states.
pub fn note(status: &SyncStatus, refresh_secs: u64, now_ms: i64) -> Option<crate::panel::SyncNote> {
    use crate::panel::{SyncNote, Tone, ago};

    if !status.enabled {
        return None;
    }
    let devices = status.devices.len();
    let note = |tone, headline: &str, detail: String| {
        Some(SyncNote {
            devices,
            tone,
            headline: headline.to_string(),
            detail,
        })
    };

    if let Some(error) = &status.error {
        return note(Tone::Critical, "error", error.clone());
    }
    if let Some(overlap) = status.overlaps.first() {
        return note(
            Tone::Critical,
            "duplicate",
            format!(
                "{} read the same transcripts on {}; counted once. Turn that provider off in [sync.providers] on one of them.",
                overlap.devices.join(" and "),
                overlap.date
            ),
        );
    }
    if !status.dropped.is_empty() {
        return note(
            Tone::Warn,
            "re-keyed",
            format!(
                "new fleet key; dropped {} from the old fleet",
                status.dropped.join(", ")
            ),
        );
    }
    if let Some(skipped) = status.skipped.first() {
        return note(
            Tone::Warn,
            "skipped",
            format!("{} unusable: {}", status.skipped.len(), skipped.reason),
        );
    }
    match status.last_pull_ms {
        None => note(Tone::Warn, "never", "no sync has completed yet".to_string()),
        Some(last) => {
            let age = now_ms - last;
            if age > 86_400_000 {
                note(
                    Tone::Critical,
                    "stale",
                    format!("last synced {}; totals may be short", ago(last, now_ms)),
                )
            } else if age > 3 * (refresh_secs as i64) * 1000 {
                note(
                    Tone::Warn,
                    "stale",
                    format!("last synced {}", ago(last, now_ms)),
                )
            } else if devices < 2 {
                note(
                    Tone::Dim,
                    "waiting",
                    "no other device has published yet".to_string(),
                )
            } else {
                note(Tone::Good, "ok", String::new())
            }
        }
    }
}

/// This machine's device id, for attributing the local row.
pub fn local_device_id(config: &TokenGaugeConfig) -> String {
    crate::device_identity(&config.cache_file).machine_id
}

fn cycle(
    config: &TokenGaugeConfig,
    device: &DeviceRecord,
    store: &mut FleetStore,
    now: DateTime<Utc>,
    status: &mut SyncStatus,
) -> Result<()> {
    let key = crypto::load_key(&config.cache_file)?.context(
        "no fleet key on this machine; run `--sync-init` here, or `--sync-join <key>` to join an existing fleet",
    )?;
    status.key_id = key.id_hex();
    let key_change = store.adopt_key(&key.id_hex(), &device.id);
    status.dropped = key_change
        .as_ref()
        .map(|change| change.dropped.clone())
        .unwrap_or_default();

    let transport = transport::open(&config.sync)?;
    status.transport = transport.describe();

    // Our own object under the old key is unreadable to everyone now, including
    // us. Leaving it in a shared folder is litter nothing would ever collect.
    if let Some(previous) = key_change.and_then(|change| change.previous_object) {
        let _ = transport.delete(&previous);
    }

    let providers = config
        .sync
        .providers
        .resolve(&config.providers.enabled_providers());
    let own_name = key.object_name(&device.id);

    if let Some(contribution) = store.contribution(
        &device.id,
        now,
        &providers,
        i64::from(config.sync.retention_days),
    ) {
        let hash = publish_stamp(&contribution, &transport.describe(), &own_name);
        if store.published_hash != Some(hash) {
            let body =
                serde_json::to_vec(&contribution).context("could not serialise a contribution")?;
            transport.put(&own_name, &key.seal(&own_name, &body)?)?;
            store.published_hash = Some(hash);
            store.published_name = Some(own_name.clone());
            store.last_publish_ms = Some(now.timestamp_millis());
            status.published = true;
        }
    }

    for entry in transport.list()? {
        if entry.name == own_name {
            continue;
        }
        let known = store.objects.get(&entry.name).cloned();

        // One object we cannot fetch must not end the pull. Propagating here
        // would drop every other peer's update for this cycle over a single
        // unreadable file, which is the opposite of what the durable store is
        // for. The version is deliberately not recorded, so it is retried.
        let sealed = match transport.get(&entry, known.as_ref().map(|o| o.version.as_str())) {
            Ok(sealed) => sealed,
            Err(e) => {
                status.skipped.push(SkippedObject {
                    name: entry.name,
                    reason: format!("{e:#}"),
                });
                continue;
            }
        };

        let Some(sealed) = sealed else {
            // Unchanged since we last looked. A standing rejection still gets
            // reported, or it would go quiet after the first cycle.
            if let Some(reason) = known.and_then(|o| o.reason) {
                status.skipped.push(SkippedObject {
                    name: entry.name,
                    reason,
                });
            }
            continue;
        };

        let reason = match read_peer(&key, &store.retired_key_ids, &entry.name, &sealed) {
            PeerOutcome::Absorbed(contribution) => {
                store.absorb(&contribution);
                None
            }
            PeerOutcome::Ignored => None,
            PeerOutcome::Rejected(reason) => {
                status.skipped.push(SkippedObject {
                    name: entry.name.clone(),
                    reason: reason.clone(),
                });
                Some(reason)
            }
        };
        store.objects.insert(
            entry.name,
            ObjectState {
                version: entry.version,
                reason,
            },
        );
    }

    store.last_pull_ms = Some(now.timestamp_millis());
    Ok(())
}

/// The contribution's content bound to **where** it goes.
///
/// Content alone is not enough: changing the folder, the bucket, the prefix or
/// the fleet key leaves the data identical but sends it to a different object
/// in a different place, and a device that skipped the write would never appear
/// there. The key is folded in through the object name, which is derived from
/// it.
fn publish_stamp(contribution: &Contribution, target: &str, name: &str) -> u64 {
    crate::cost::digest_u64(&[
        &content_hash(contribution).to_le_bytes(),
        target.as_bytes(),
        name.as_bytes(),
    ])
}

/// What one peer object turned out to be.
///
/// Split out of the pull loop so the decision is made once, from one decrypt,
/// and can be tested on bytes instead of only through a folder.
#[derive(Debug)]
enum PeerOutcome {
    Absorbed(Box<Contribution>),
    /// Ours, sealed under a key we have retired. Passed over in silence: it is
    /// our own past, not another fleet sharing the storage.
    Ignored,
    Rejected(String),
}

fn read_peer(key: &crypto::FleetKey, retired: &[String], name: &str, sealed: &[u8]) -> PeerOutcome {
    let plain = match key.open(name, sealed) {
        Ok(plain) => plain,
        Err(crypto::OpenError::ForeignKey { key_id }) if retired.contains(&key_id) => {
            return PeerOutcome::Ignored;
        }
        Err(e) => return PeerOutcome::Rejected(e.to_string()),
    };
    let contribution = match serde_json::from_slice::<Contribution>(&plain) {
        Ok(contribution) => contribution,
        Err(e) => return PeerOutcome::Rejected(format!("contribution did not parse: {e}")),
    };
    if contribution.schema_version > SCHEMA_VERSION {
        return PeerOutcome::Rejected(format!(
            "written by a newer TokenGauge (schema {})",
            contribution.schema_version
        ));
    }
    PeerOutcome::Absorbed(Box::new(contribution))
}

fn device_statuses(
    store: &FleetStore,
    local_id: &str,
    config: &TokenGaugeConfig,
    now: DateTime<Utc>,
) -> Vec<DeviceStatus> {
    let cutoff = i64::from(config.sync.peer_max_age_days) * 86_400_000;
    store
        .devices
        .iter()
        .map(|(id, slice)| DeviceStatus {
            device_id: id.clone(),
            label: slice.device.display().to_string(),
            hostname: slice.device.hostname.clone(),
            updated_at_ms: slice.updated_at_ms,
            is_local: id == local_id,
            stale: id != local_id && now.timestamp_millis() - slice.updated_at_ms > cutoff,
        })
        .collect()
}

fn label_for(store: &FleetStore, id: &str) -> String {
    store
        .devices
        .get(id)
        .map(|slice| slice.device.display().to_string())
        .unwrap_or_else(|| id.to_string())
}

/// The local midnight the transcript read started from, as a UTC hour.
fn window_hour(since: NaiveDate) -> Hour {
    let midnight = since.and_hms_opt(0, 0, 0).unwrap_or_default();
    let at = Local
        .from_local_datetime(&midnight)
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| midnight.and_utc());
    Hour::containing(at)
}

/// Write a probe object, read it back, and remove it.
///
/// Exercises what a cycle actually needs and nothing more: write access, read
/// access, the key, and delete. The probe's name is deliberately not a valid
/// object name, so a peer mid-test lists nothing new.
pub fn test_round_trip(config: &TokenGaugeConfig) -> Result<Vec<String>> {
    let mut steps = Vec::new();
    let key = crypto::load_key(&config.cache_file)?
        .context("no fleet key on this machine; run `--sync-init` or `--sync-join <key>` first")?;
    steps.push(format!("fleet key {}", key.id_hex()));

    let transport = transport::open(&config.sync)?;
    steps.push(transport.describe());

    let device = crate::device_identity(&config.cache_file);
    let name = format!("probe-{}.tgsync", device.machine_id);
    let body = b"tokengauge sync probe";
    transport.put(&name, &key.seal(&name, body)?)?;
    steps.push("wrote a probe".to_string());

    let entry = transport::PeerEntry {
        name: name.clone(),
        version: String::new(),
        size: 0,
    };
    // The probe is removed whatever the read does, or a failed test leaves
    // litter in the user's folder that no command ever cleans up.
    let checked = (|| -> Result<()> {
        let read = transport
            .get(&entry, None)?
            .context("the probe could not be read back")?;
        let opened = key
            .open(&name, &read)
            .map_err(|e| anyhow::anyhow!("the probe did not open: {e}"))?;
        anyhow::ensure!(opened == body, "the probe read back different bytes");
        Ok(())
    })();
    let removed = transport.delete(&name);
    checked?;
    steps.push("read it back and opened it".to_string());
    removed?;
    steps.push("removed it".to_string());
    Ok(steps)
}

/// Drop a device from the fleet: delete its object and forget its buckets.
///
/// Matched on device id or label, because nobody remembers a machine id.
pub fn forget(config: &TokenGaugeConfig, wanted: &str) -> Result<String> {
    let (mut store, _) = store::load(&config.cache_file);
    let local = crate::device_identity(&config.cache_file).machine_id;
    let matched: Vec<String> = store
        .devices
        .iter()
        .filter(|(id, slice)| {
            id.eq_ignore_ascii_case(wanted)
                || slice.device.display().eq_ignore_ascii_case(wanted)
                || slice.device.hostname.eq_ignore_ascii_case(wanted)
        })
        .map(|(id, _)| id.clone())
        .collect();

    let [id] = matched.as_slice() else {
        anyhow::bail!(
            "{} device matches '{wanted}'; known devices: {}",
            if matched.is_empty() {
                "no"
            } else {
                "more than one"
            },
            store
                .devices
                .values()
                .map(|slice| slice.device.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    };
    anyhow::ensure!(
        *id != local,
        "that is this machine; turn sync off in the config instead"
    );

    let label = store
        .devices
        .get(id)
        .map(|slice| slice.device.display().to_string())
        .unwrap_or_else(|| id.clone());

    // A delete that failed used to be swallowed, reporting success while the
    // object stayed in storage and the device rejoined on the next cycle.
    if let Some(key) = crypto::load_key(&config.cache_file)? {
        let transport = transport::open(&config.sync)?;
        let name = key.object_name(id);
        transport.delete(&name)?;
        store.objects.remove(&name);
    }
    store.devices.remove(id);
    store::save(&config.cache_file, &store)?;
    Ok(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::{Bucket, DeviceRecord, Granularity};

    fn contribution(schema: u32) -> Contribution {
        Contribution {
            schema_version: schema,
            device: DeviceRecord {
                id: "peer".into(),
                hostname: "laptop".into(),
                label: String::new(),
                os: "linux".into(),
            },
            written_at_ms: 1,
            tz_offset_minutes: 0,
            covers_from: Hour::containing(Utc::now()).minus_days(30),
            providers: vec!["claude".into()],
            buckets: vec![Bucket {
                hour: Hour::containing(Utc::now()),
                provider: "claude".into(),
                model: "opus".into(),
                granularity: Granularity::Hour,
                tokens: Default::default(),
            }],
            days: Vec::new(),
        }
    }

    fn sealed(key: &crypto::FleetKey, name: &str, schema: u32) -> Vec<u8> {
        let body = serde_json::to_vec(&contribution(schema)).expect("serialise");
        key.seal(name, &body).expect("seal")
    }

    #[test]
    fn every_way_a_peer_object_can_be_unusable_is_named() {
        let key = crypto::FleetKey::generate();
        let name = key.object_name("peer");
        let retired = Vec::new();

        assert!(matches!(
            read_peer(&key, &retired, &name, &sealed(&key, &name, SCHEMA_VERSION)),
            PeerOutcome::Absorbed(_)
        ));

        let mut tampered = sealed(&key, &name, SCHEMA_VERSION);
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        match read_peer(&key, &retired, &name, &tampered) {
            PeerOutcome::Rejected(why) => assert!(why.contains("authentication"), "{why}"),
            other => panic!("a tampered object must be rejected, got {other:?}"),
        }

        let not_json = key.seal(&name, b"{ not a contribution").expect("seal");
        match read_peer(&key, &retired, &name, &not_json) {
            PeerOutcome::Rejected(why) => assert!(why.contains("did not parse"), "{why}"),
            other => panic!("expected a parse rejection, got {other:?}"),
        }

        match read_peer(
            &key,
            &retired,
            &name,
            &sealed(&key, &name, SCHEMA_VERSION + 1),
        ) {
            PeerOutcome::Rejected(why) => assert!(why.contains("newer TokenGauge"), "{why}"),
            other => panic!("expected a schema rejection, got {other:?}"),
        }
    }

    #[test]
    fn our_own_retired_key_is_passed_over_but_a_strangers_is_reported() {
        let mine = crypto::FleetKey::generate();
        let old = crypto::FleetKey::generate();
        let stranger = crypto::FleetKey::generate();
        let name = mine.object_name("peer");

        let ours = sealed(&old, &name, SCHEMA_VERSION);
        assert!(matches!(
            read_peer(&mine, &[old.id_hex()], &name, &ours),
            PeerOutcome::Ignored
        ));

        let theirs = sealed(&stranger, &name, SCHEMA_VERSION);
        match read_peer(&mine, &[old.id_hex()], &name, &theirs) {
            PeerOutcome::Rejected(why) => assert!(why.contains("another fleet key"), "{why}"),
            other => panic!("a stranger's object must be reported, got {other:?}"),
        }
    }

    fn status_with(devices: usize, last_pull_ms: Option<i64>) -> SyncStatus {
        SyncStatus {
            enabled: true,
            devices: (0..devices)
                .map(|n| DeviceStatus {
                    device_id: format!("d{n}"),
                    label: format!("machine {n}"),
                    hostname: "host".into(),
                    updated_at_ms: 0,
                    is_local: n == 0,
                    stale: false,
                })
                .collect(),
            last_pull_ms,
            ..Default::default()
        }
    }

    /// The wording every frontend shows, and its worst-first order.
    #[test]
    fn the_health_note_reports_the_worst_thing_first() {
        let now = 10_000_000_000;
        let fresh = Some(now - 1000);
        let refresh_secs = 600;
        let tone = |status: &SyncStatus| note(status, refresh_secs, now).expect("enabled").tone;

        assert_eq!(note(&SyncStatus::default(), refresh_secs, now), None);

        let healthy = status_with(2, fresh);
        assert_eq!(tone(&healthy), crate::panel::Tone::Good);
        assert_eq!(tone(&status_with(1, fresh)), crate::panel::Tone::Dim);
        assert_eq!(tone(&status_with(2, None)), crate::panel::Tone::Warn);

        // Three pull intervals is a warning; a day is not.
        let mut behind = status_with(2, Some(now - 4 * 600 * 1000));
        assert_eq!(tone(&behind), crate::panel::Tone::Warn);
        behind.last_pull_ms = Some(now - 25 * 3_600 * 1000);
        assert_eq!(tone(&behind), crate::panel::Tone::Critical);

        // Worst-first: each of these outranks the staleness above it.
        let mut skipped = behind.clone();
        skipped.skipped = vec![SkippedObject {
            name: "x".into(),
            reason: "sealed for another fleet key".into(),
        }];
        assert_eq!(
            note(&skipped, refresh_secs, now).unwrap().headline,
            "skipped"
        );

        let mut dropped = skipped.clone();
        dropped.dropped = vec!["laptop".into()];
        assert_eq!(
            note(&dropped, refresh_secs, now).unwrap().headline,
            "re-keyed"
        );

        // Wrong numbers outrank an informational re-key: a transcript tree
        // counted twice is the one thing here that makes the totals lie.
        let mut overlapping = dropped.clone();
        overlapping.overlaps = vec![OverlapNote {
            date: "2026-08-25".into(),
            devices: vec!["a".into(), "b".into()],
            kept: "a".into(),
        }];
        let duplicate = note(&overlapping, refresh_secs, now).unwrap();
        assert_eq!(duplicate.headline, "duplicate");
        assert_eq!(duplicate.tone, crate::panel::Tone::Critical);

        let mut failed = overlapping.clone();
        failed.error = Some("the folder is gone".into());
        let worst = note(&failed, refresh_secs, now).unwrap();
        assert_eq!(worst.headline, "error");
        assert_eq!(worst.tone, crate::panel::Tone::Critical);
    }
}
