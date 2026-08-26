//! Two machines meeting through a shared folder.
//!
//! The peer is written into the folder directly rather than by a second
//! in-process device: `device_identity` memoises per process, so one test
//! binary only ever has one machine id.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use tokengauge_core::cost::{TokenCounts, UsageEvent};
use tokengauge_core::sync::{self, FleetKey, SCHEMA_VERSION};
use tokengauge_core::sync::{Contribution, DeviceRecord, Hour, bucketize};
use tokengauge_core::{SyncConfig, SyncDirConfig, TokenGaugeConfig};

const PEER_ID: &str = "0000feedfacecafe";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tokengauge-fleet-{}-{name}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn config(root: &Path) -> TokenGaugeConfig {
    TokenGaugeConfig {
        cache_file: root.join("tokengauge-usage.json"),
        sync: SyncConfig {
            enabled: true,
            dir: SyncDirConfig {
                path: root.join("shared"),
                ..SyncDirConfig::default()
            },
            ..SyncConfig::default()
        },
        ..TokenGaugeConfig::default()
    }
}

fn event(hours_ago: i64, out: u64, key: u64) -> UsageEvent {
    let at = Utc::now() - Duration::hours(hours_ago);
    UsageEvent {
        provider: "claude",
        model: "claude-opus-5".into(),
        date: at.with_timezone(&chrono::Local).date_naive(),
        at,
        tokens: TokenCounts {
            output: out,
            ..Default::default()
        },
        key: Some(key),
    }
}

fn objects(root: &Path) -> Vec<String> {
    objects_in(&root.join("shared"))
}

fn objects_in(sync_dir: &Path) -> Vec<String> {
    let dir = sync_dir.join("v1");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// Stand in for the other machine: a contribution written straight into the
/// folder, sealed with the same fleet key.
fn publish_peer(root: &Path, key: &FleetKey, tokens: u64) {
    let device = DeviceRecord {
        id: PEER_ID.into(),
        hostname: "laptop".into(),
        label: "laptop".into(),
        os: "linux".into(),
    };
    let contribution = Contribution {
        schema_version: SCHEMA_VERSION,
        device,
        written_at_ms: Utc::now().timestamp_millis(),
        tz_offset_minutes: 0,
        covers_from: Hour::containing(Utc::now()).minus_days(30),
        providers: vec!["claude".into()],
        buckets: bucketize(&[event(50, tokens, 0xfeed)]),
        days: Vec::new(),
    };

    let name = key.object_name(PEER_ID);
    let body = serde_json::to_vec(&contribution).expect("serialise");
    let sealed = key.seal(&name, &body).expect("seal");
    let dir = root.join("shared").join("v1");
    std::fs::create_dir_all(&dir).expect("shared dir");
    std::fs::write(dir.join(&name), sealed).expect("write peer object");
}

#[test]
fn a_fleet_of_two_meets_through_a_folder() {
    let root = scratch("two");
    let config = config(&root);
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");

    let local = vec![event(2, 100, 1), event(30, 200, 2)];

    let outcome = sync::refresh(&config, &local, since);
    let (peers, status) = (outcome.events, outcome.status);
    assert_eq!(status.error, None, "first cycle should not error");
    assert!(status.published, "a first contribution is always new");
    assert!(peers.is_empty(), "nothing to take from an empty folder");
    assert_eq!(objects(&root).len(), 1, "this machine published one object");
    assert_eq!(status.devices.len(), 1);

    let status = sync::refresh(&config, &local, since).status;
    assert!(
        !status.published,
        "unchanged data must not churn the storage"
    );

    publish_peer(&root, &key, 4242);
    let outcome = sync::refresh(&config, &local, since);
    let (peers, status) = (outcome.events, outcome.status);
    assert_eq!(status.error, None);
    assert_eq!(status.devices.len(), 2, "the peer joined the fleet");
    assert!(status.skipped.is_empty(), "{:?}", status.skipped);
    assert_eq!(
        peers.iter().map(|e| e.tokens.output).sum::<u64>(),
        4242,
        "the peer's tokens arrived, and this machine's own were left to its transcripts"
    );
    assert_eq!(objects(&root).len(), 2);
}

/// Content alone decided whether to publish, so pointing sync at a new folder
/// or re-keying the fleet left the new target empty until the data happened to
/// change.
#[test]
fn a_new_target_gets_this_machine_even_when_the_data_has_not_changed() {
    let root = scratch("retarget");
    let mut config = config(&root);
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = vec![event(2, 100, 1)];

    assert!(sync::refresh(&config, &local, since).status.published);
    assert!(
        !sync::refresh(&config, &local, since).status.published,
        "unchanged data to the same place must not churn"
    );

    let moved = root.join("moved");
    config.sync.dir.path = moved.clone();
    let status = sync::refresh(&config, &local, since).status;
    assert!(status.published, "a new folder has to receive this machine");
    assert_eq!(objects_in(&moved).len(), 1);

    // Re-keying changes the object name, so the fleet's new object is a
    // different one and has to be written even though the tokens are identical.
    let rekeyed = FleetKey::generate();
    sync::store_key(&config.cache_file, &rekeyed, true).expect("re-key");
    let status = sync::refresh(&config, &local, since).status;
    assert!(status.published, "a re-keyed fleet writes a new object");
    assert_eq!(
        objects_in(&moved),
        vec![rekeyed.object_name(&sync::local_device_id(&config))],
        "the object under the old key is unreadable to everyone now, including \
         us, so it is not left behind in a shared folder"
    );
}

/// A different key is a different fleet, so the machines from the old one stop
/// counting. Their objects are sealed under a key this machine no longer holds,
/// and their rows would claim a fleet it is no longer part of.
#[test]
fn re_keying_leaves_the_old_fleet_behind_but_keeps_this_machine() {
    let root = scratch("rekey");
    let config = config(&root);
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = vec![event(2, 100, 1)];

    publish_peer(&root, &key, 4242);
    let outcome = sync::refresh(&config, &local, since);
    assert_eq!(outcome.status.devices.len(), 2, "the peer joined");
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        4242
    );

    sync::store_key(&config.cache_file, &FleetKey::generate(), true).expect("re-key");
    let outcome = sync::refresh(&config, &local, since);

    assert_eq!(outcome.status.dropped, vec!["laptop".to_string()]);
    assert_eq!(
        outcome.status.devices.len(),
        1,
        "only this machine is in the new fleet"
    );
    assert!(
        outcome.events.is_empty(),
        "the old fleet's tokens must stop counting"
    );
    assert!(
        outcome.status.skipped.is_empty(),
        "the old object must not be reported as foreign every cycle: {:?}",
        outcome.status.skipped
    );
    assert!(
        sync::store::load(&config.cache_file)
            .0
            .devices
            .contains_key(&sync::local_device_id(&config)),
        "this machine's own history is ours and stays"
    );
}

#[test]
fn an_object_for_another_fleet_is_named_and_not_retried() {
    let root = scratch("foreign");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let mine = FleetKey::generate();
    sync::store_key(&config.cache_file, &mine, true).expect("key");
    publish_peer(&root, &FleetKey::generate(), 999);

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since);
    let (peers, status) = (outcome.events, outcome.status);
    assert!(
        peers.is_empty(),
        "another fleet's tokens must not be counted"
    );
    assert_eq!(status.skipped.len(), 1);
    assert!(
        status.skipped[0].reason.contains("another fleet key"),
        "expected a named reason, got {:?}",
        status.skipped[0].reason
    );

    // Unchanged on the next cycle: no re-download, but the warning stands.
    let status = sync::refresh(&config, &[event(1, 10, 1)], since).status;
    assert_eq!(
        status.skipped.len(),
        1,
        "a standing rejection keeps reporting"
    );
}

/// `refresh` promises it never fails and keeps the totals. One unusable object
/// among healthy peers is the cheapest way that promise breaks.
#[test]
fn one_unusable_object_does_not_cost_the_other_peers_their_cycle() {
    let root = scratch("bad-apple");
    let config = config(&root);
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");

    publish_peer(&root, &key, 4242);
    let dir = root.join("shared").join("v1");
    // A name that lists like ours but is not one of our envelopes at all.
    std::fs::write(
        dir.join("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tgsync"),
        b"garbage",
    )
    .expect("write junk");

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since);

    assert_eq!(outcome.status.error, None, "the cycle must not abort");
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        4242,
        "the healthy peer still landed"
    );
    assert_eq!(
        outcome.status.skipped.len(),
        1,
        "{:?}",
        outcome.status.skipped
    );
    assert!(
        outcome.status.skipped[0]
            .reason
            .contains("not a TokenGauge sync object")
    );
}

#[test]
fn forget_matches_a_machine_the_way_a_person_names_it() {
    let root = scratch("forget");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    publish_peer(&root, &key, 500);
    sync::refresh(&config, &[event(1, 10, 1)], since);

    assert!(sync::forget(&config, "nobody").is_err(), "unknown name");
    let local = sync::local_device_id(&config);
    let refused = sync::forget(&config, &local).expect_err("this machine");
    assert!(format!("{refused}").contains("this machine"), "{refused}");

    // label, id and hostname all name the same device to a person.
    assert_eq!(sync::forget(&config, "laptop").expect("by label"), "laptop");
    assert!(
        !objects(&root).contains(&key.object_name(PEER_ID)),
        "the peer's object must go, or it rejoins next cycle"
    );

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since);
    assert_eq!(outcome.status.devices.len(), 1);
    assert!(outcome.events.is_empty());
}

#[test]
fn absorbing_the_same_contribution_twice_does_not_double_it() {
    let root = scratch("idempotent");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    publish_peer(&root, &key, 700);

    let first = sync::refresh(&config, &[], since);
    assert_eq!(
        first.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        700
    );

    // Republished with the same tokens: a peer that rewrites its object must
    // replace what we hold, never add to it. This is the failure class the
    // whole feature exists to prevent.
    publish_peer(&root, &key, 700);
    let again = sync::refresh(&config, &[], since);
    assert_eq!(
        again.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        700,
        "absorb must replace its covered range, not accumulate"
    );
}

#[test]
fn sync_that_is_off_reads_nothing_and_writes_nothing() {
    let root = scratch("off");
    let mut config = config(&root);
    config.sync.enabled = false;
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since);
    let (peers, status) = (outcome.events, outcome.status);
    assert!(peers.is_empty());
    assert!(!status.enabled);
    assert!(objects(&root).is_empty());
    assert!(!sync::store::store_path(&config.cache_file).exists());
}

#[test]
fn a_missing_fleet_key_is_an_error_the_user_can_act_on() {
    let root = scratch("nokey");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();

    let status = sync::refresh(&config, &[event(1, 10, 1)], since).status;
    let error = status.error.expect("a fleet with no key cannot sync");
    assert!(error.contains("--sync-init"), "{error}");
}

#[test]
fn a_sync_tools_conflict_copy_is_not_mistaken_for_a_contribution() {
    use tokengauge_core::sync::transport::is_object_name;

    let name = "0123456789abcdef0123456789abcdef";
    assert!(is_object_name(&format!("{name}.tgsync")));
    assert!(!is_object_name(&format!(
        "{name}.sync-conflict-20260825.tgsync"
    )));
    assert!(!is_object_name(&format!("{name}.tgsync.tmp1234")));
    assert!(!is_object_name("README.md"));
}

/// The seam where peer buckets actually reach the panel. It used to be reachable
/// only through a real home directory, so nothing proved a peer's tokens land in
/// `by_device` or that the sync note reaches every provider row.
#[test]
fn a_peers_buckets_reach_the_panel_rows() {
    use tokengauge_core::cost::build_report;
    use tokengauge_core::cost::pricing::PriceTable;

    let root = scratch("panel-seam");
    let config = config(&root);
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    publish_peer(&root, &key, 4242);

    let local = vec![event(2, 1000, 1)];
    let outcome = sync::refresh(&config, &local, since);

    let mut events = local.clone();
    events.extend(outcome.events);
    let prices = PriceTable::default();
    let today = Utc::now().with_timezone(&chrono::Local).date_naive();
    let mut report = build_report(&events, &prices, today);

    tokengauge_core::attach_fleet(
        &mut report,
        &outcome.store,
        &prices,
        today,
        &sync::local_device_id(&config),
        sync::note(&outcome.status, 600, Utc::now().timestamp_millis()),
    );

    let claude = report.costs.get("claude").expect("a claude row");
    assert_eq!(
        claude.by_device.len(),
        2,
        "both machines appear: {:?}",
        claude.by_device
    );
    assert!(
        claude
            .by_device
            .iter()
            .any(|d| d.is_local && d.tokens == 1000),
        "{:?}",
        claude.by_device
    );
    assert!(
        claude
            .by_device
            .iter()
            .any(|d| !d.is_local && d.tokens == 4242),
        "the peer's tokens have to reach the rows, not just the total"
    );
    assert!(
        claude.sync_note.is_some(),
        "every provider row carries the sync state"
    );
}

/// A peer on a newer TokenGauge syncing a provider this build cannot rate used
/// to have its tokens vanish from the total with nothing said.
#[test]
fn a_provider_this_build_cannot_rate_is_reported_not_swallowed() {
    let root = scratch("unknown-provider");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");

    let device = DeviceRecord {
        id: PEER_ID.into(),
        hostname: "laptop".into(),
        label: "laptop".into(),
        os: "linux".into(),
    };
    let mut bucket = bucketize(&[event(3, 999, 7)]);
    bucket[0].provider = "quasar".into();
    let contribution = Contribution {
        schema_version: SCHEMA_VERSION,
        device,
        written_at_ms: Utc::now().timestamp_millis(),
        tz_offset_minutes: 0,
        covers_from: Hour::containing(Utc::now()).minus_days(30),
        providers: vec!["quasar".into()],
        buckets: bucket,
        days: Vec::new(),
    };
    let name = key.object_name(PEER_ID);
    let dir = root.join("shared").join("v1");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(
        dir.join(&name),
        key.seal(&name, &serde_json::to_vec(&contribution).expect("json"))
            .expect("seal"),
    )
    .expect("write");

    let outcome = sync::refresh(&config, &[], since);

    assert!(
        outcome.events.is_empty(),
        "it cannot be rated, so it is not counted"
    );
    assert_eq!(
        outcome.status.unreadable_providers,
        vec!["quasar".to_string()]
    );
    let report = sync::describe(&outcome.status, 600, Utc::now().timestamp_millis());
    assert!(
        report
            .problems
            .iter()
            .any(|p| p.contains("quasar") && p.contains("Update TokenGauge")),
        "the gap has to be visible: {:?}",
        report.problems
    );
}
