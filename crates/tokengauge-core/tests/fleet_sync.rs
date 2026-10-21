//! Two machines meeting through a shared folder.
//!
//! The peer is written into the folder directly rather than by a second
//! in-process device: `device_identity` memoises per process, so one test
//! binary only ever has one machine id.

use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use tokengauge_core::cost::{TokenCounts, UsageEvent};
use tokengauge_core::sync::{self, FleetKey, SCHEMA_VERSION};
use tokengauge_core::sync::{Adopted, Contribution, DeviceRecord, Hour, bucketize};
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
        // Now, not 50 hours ago: the tokens_by_device split is month-to-date
        // (cost::window_start reaches back only to the 1st), so a fixed
        // hours-ago offset drops out of the window on the first days of a
        // month - which is what made this fixture flake overnight into Sep 1.
        buckets: bucketize(&[event(0, tokens, 0xfeed)]),
        days: Vec::new(),
        adopted: Vec::new(),
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

    let outcome = sync::refresh(&config, &local, since, &[]);
    let (peers, status) = (outcome.events, outcome.status);
    assert_eq!(status.error, None, "first cycle should not error");
    assert!(status.published, "a first contribution is always new");
    assert!(peers.is_empty(), "nothing to take from an empty folder");
    assert_eq!(objects(&root).len(), 1, "this machine published one object");
    assert_eq!(status.devices.len(), 1);

    let status = sync::refresh(&config, &local, since, &[]).status;
    assert!(
        !status.published,
        "unchanged data must not churn the storage"
    );

    publish_peer(&root, &key, 4242);
    let outcome = sync::refresh(&config, &local, since, &[]);
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

    assert!(sync::refresh(&config, &local, since, &[]).status.published);
    assert!(
        !sync::refresh(&config, &local, since, &[]).status.published,
        "unchanged data to the same place must not churn"
    );

    let moved = root.join("moved");
    config.sync.dir.path = moved.clone();
    let status = sync::refresh(&config, &local, since, &[]).status;
    assert!(status.published, "a new folder has to receive this machine");
    assert_eq!(objects_in(&moved).len(), 1);

    // Re-keying changes the object name, so the fleet's new object is a
    // different one and has to be written even though the tokens are identical.
    let rekeyed = FleetKey::generate();
    sync::store_key(&config.cache_file, &rekeyed, true).expect("re-key");
    let status = sync::refresh(&config, &local, since, &[]).status;
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
    let outcome = sync::refresh(&config, &local, since, &[]);
    assert_eq!(outcome.status.devices.len(), 2, "the peer joined");
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        4242
    );

    sync::store_key(&config.cache_file, &FleetKey::generate(), true).expect("re-key");
    let outcome = sync::refresh(&config, &local, since, &[]);

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

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since, &[]);
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
    let status = sync::refresh(&config, &[event(1, 10, 1)], since, &[]).status;
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

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since, &[]);

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
    sync::refresh(&config, &[event(1, 10, 1)], since, &[]);

    assert!(
        sync::forget(&config, "nobody", || {}).is_err(),
        "unknown name"
    );
    let local = sync::local_device_id(&config);
    let refused = sync::forget(&config, &local, || {}).expect_err("this machine");
    assert!(format!("{refused}").contains("this machine"), "{refused}");

    // label, id and hostname all name the same device to a person.
    assert_eq!(
        sync::forget(&config, "laptop", || {}).expect("by label"),
        "laptop"
    );
    assert!(
        !objects(&root).contains(&key.object_name(PEER_ID)),
        "the peer's object must go, or it rejoins next cycle"
    );

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since, &[]);
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

    let first = sync::refresh(&config, &[], since, &[]);
    assert_eq!(
        first.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        700
    );

    // Republished with the same tokens: a peer that rewrites its object must
    // replace what we hold, never add to it. This is the failure class the
    // whole feature exists to prevent.
    publish_peer(&root, &key, 700);
    let again = sync::refresh(&config, &[], since, &[]);
    assert_eq!(
        again.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        700,
        "absorb must replace its covered range, not accumulate"
    );
}

#[test]
fn sync_that_is_off_keeps_the_store_and_touches_no_transport() {
    let root = scratch("off");
    let mut config = config(&root);
    config.sync.enabled = false;
    // A week, not today: the event below is an hour old, so for the first hour
    // after local midnight `today` is a window that starts after it and the
    // store legitimately comes out empty. Every other test here that expects
    // its own event to land already reaches back.
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();

    let outcome = sync::refresh(&config, &[event(1, 10, 1)], since, &[]);
    assert!(
        outcome.events.is_empty(),
        "there are no peers to take events from"
    );
    assert!(!outcome.status.enabled);
    assert!(
        objects(&root).is_empty(),
        "sync that is off publishes nothing and reads nobody"
    );

    // The store is not sync's own bookkeeping, it is the durable record that
    // history is read from. Gating it on `[sync] enabled` left every machine
    // that never turned sync on with no past to look at.
    assert!(
        sync::store::store_path(&config.cache_file).exists(),
        "the store is kept whether or not sync is on"
    );
    let (store, error) = sync::store::load(&config.cache_file);
    assert_eq!(error, None);
    let own: u64 = store
        .devices
        .values()
        .flat_map(|slice| &slice.buckets)
        .map(|bucket| bucket.tokens.output)
        .sum();
    assert_eq!(own, 10, "this machine's own tokens are in it");
}

#[test]
fn a_missing_fleet_key_is_an_error_the_user_can_act_on() {
    let root = scratch("nokey");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();

    let status = sync::refresh(&config, &[event(1, 10, 1)], since, &[]).status;
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

    // Also now, not 2 hours ago: same month-window reason as the peer above -
    // in the first hours of a month a 2-hours-ago local event falls into last
    // month and drops out of the by_device split, and then neither machine
    // appears.
    let local = vec![event(0, 1000, 1)];
    let outcome = sync::refresh(&config, &local, since, &[]);

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
        adopted: Vec::new(),
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

    let outcome = sync::refresh(&config, &[], since, &[]);

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

const OLD_ID: &str = "01d1e00000000000";
const NEW_ID: &str = "000000000000000e";

/// A contribution from any device, written straight into the folder.
fn publish_device(
    root: &Path,
    key: &FleetKey,
    device: DeviceRecord,
    covers_days: i64,
    events: &[UsageEvent],
    adopted: &[&str],
) {
    let name = key.object_name(&device.id);
    let contribution = Contribution {
        schema_version: SCHEMA_VERSION,
        device,
        written_at_ms: Utc::now().timestamp_millis(),
        tz_offset_minutes: 0,
        covers_from: Hour::containing(Utc::now()).minus_days(covers_days),
        providers: vec!["claude".into()],
        buckets: bucketize(events),
        days: Vec::new(),
        adopted: adopted
            .iter()
            .map(|id| Adopted {
                id: id.to_string(),
                at_ms: Utc::now().timestamp_millis(),
            })
            .collect(),
    };
    let body = serde_json::to_vec(&contribution).expect("serialise");
    let dir = root.join("shared").join("v1");
    std::fs::create_dir_all(&dir).expect("shared dir");
    std::fs::write(dir.join(&name), key.seal(&name, &body).expect("seal")).expect("write");
}

fn record(id: &str, hostname: &str) -> DeviceRecord {
    DeviceRecord {
        id: id.into(),
        hostname: hostname.into(),
        label: String::new(),
        os: "linux".into(),
    }
}

fn published(root: &Path, key: &FleetKey, id: &str) -> Contribution {
    let name = key.object_name(id);
    let sealed = std::fs::read(root.join("shared").join("v1").join(&name)).expect("object");
    serde_json::from_slice(&key.open(&name, &sealed).expect("open")).expect("parse")
}

fn adopted_ids(contribution: &Contribution) -> Vec<&str> {
    contribution
        .adopted
        .iter()
        .map(|adopted| adopted.id.as_str())
        .collect()
}

fn remove_object(root: &Path, key: &FleetKey, id: &str) {
    std::fs::remove_file(root.join("shared").join("v1").join(key.object_name(id)))
        .expect("remove object");
}

#[test]
fn adopting_a_reinstalled_machine_keeps_its_history_and_leaves_the_fleet() {
    let root = scratch("adopt");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = sync::local_device_id(&config);

    let outcome = sync::refresh(&config, &[event(0, 10, 1)], since, &[]);
    let hostname = outcome.status.devices[0].hostname.clone();
    publish_device(
        &root,
        &key,
        record(OLD_ID, &hostname),
        60,
        &[event(40 * 24, 500, 9), event(0, 7, 10)],
        &[],
    );
    assert_eq!(
        sync::refresh(&config, &[event(0, 10, 1)], since, &[])
            .status
            .devices
            .len(),
        2
    );

    let ambiguous = sync::adopt(&config, &hostname, || {}).expect_err("both share the hostname");
    assert!(
        format!("{ambiguous}").contains("more than one"),
        "{ambiguous}"
    );
    assert!(
        format!("{ambiguous}").contains(OLD_ID),
        "the ids are what tells the two apart: {ambiguous}"
    );
    let refused = sync::adopt(&config, &local, || {}).expect_err("this machine");
    assert!(format!("{refused}").contains("old entry"), "{refused}");

    let adoption = sync::adopt(&config, &OLD_ID[..8], || {}).expect("adopted by an id prefix");
    assert_eq!(adoption.device.id, OLD_ID);
    assert_eq!(
        adoption.buckets, 1,
        "the hour inside this machine's window stays its own"
    );
    assert!(adoption.range.is_some());
    assert!(!objects(&root).contains(&key.object_name(OLD_ID)));

    let wide = published(&root, &key, &local);
    assert_eq!(adopted_ids(&wide), [OLD_ID]);
    assert_eq!(
        wide.buckets.iter().map(|b| b.tokens.output).sum::<u64>(),
        510,
        "peers receive the history past the wire retention, once"
    );

    let outcome = sync::refresh(&config, &[event(0, 10, 1)], since, &[]);
    assert_eq!(outcome.status.error, None);
    assert_eq!(outcome.status.devices.len(), 1);
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        500,
        "the adopted day survives a refresh and still counts"
    );
    let narrow = published(&root, &key, &local);
    assert_eq!(
        adopted_ids(&narrow),
        [OLD_ID],
        "the marker stays, for peers that missed the wide publish"
    );
    assert_eq!(
        narrow.buckets.iter().map(|b| b.tokens.output).sum::<u64>(),
        10,
        "the regular contribution is back to the wire retention"
    );
}

#[test]
fn a_peer_folds_a_device_another_machine_adopted() {
    let root = scratch("adopted-elsewhere");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");

    publish_device(
        &root,
        &key,
        record(OLD_ID, "box"),
        60,
        &[event(40 * 24, 500, 9)],
        &[],
    );
    publish_device(
        &root,
        &key,
        record(NEW_ID, "box"),
        2,
        &[event(0, 10, 11)],
        &[],
    );
    assert_eq!(
        sync::refresh(&config, &[], since, &[]).status.devices.len(),
        3
    );

    // The other machine adopted while this one was away: its wide publish has
    // already been replaced by a regular one, and only the marker is left.
    publish_device(
        &root,
        &key,
        record(NEW_ID, "box"),
        2,
        &[event(0, 10, 11)],
        &[OLD_ID],
    );
    remove_object(&root, &key, OLD_ID);

    let outcome = sync::refresh(&config, &[], since, &[]);
    assert_eq!(outcome.status.error, None);
    let ids: Vec<&str> = outcome
        .status
        .devices
        .iter()
        .map(|d| d.device_id.as_str())
        .collect();
    assert!(!ids.contains(&OLD_ID), "{ids:?}");
    let adopter: u64 = outcome.store.devices[NEW_ID]
        .buckets
        .iter()
        .map(|b| b.tokens.total())
        .sum();
    assert_eq!(adopter, 510, "our own copy of the old history moved over");
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        510,
        "and is counted once"
    );
}

#[test]
fn a_peer_whose_object_is_gone_leaves_this_machine_too() {
    let root = scratch("vanished");
    let mut config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = vec![event(0, 10, 1)];
    let devices = |config: &TokenGaugeConfig| {
        sync::refresh(config, &local, since, &[])
            .status
            .devices
            .len()
    };

    publish_peer(&root, &key, 500);
    publish_device(
        &root,
        &key,
        record(NEW_ID, "desk"),
        2,
        &[event(0, 5, 3)],
        &[],
    );
    assert_eq!(devices(&config), 3);

    let shared = config.sync.dir.path.clone();
    config.sync.dir.path = root.join("moved");
    for _ in 0..2 {
        assert_eq!(
            devices(&config),
            3,
            "a new folder without the peers means they have not moved yet"
        );
    }

    config.sync.dir.path = shared;
    assert_eq!(devices(&config), 3);
    remove_object(&root, &key, PEER_ID);
    assert_eq!(devices(&config), 2, "forgotten elsewhere, so dropped here");
}

#[test]
fn a_listing_without_our_own_object_drops_nobody() {
    let root = scratch("vanished-guard");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = vec![event(1, 10, 1)];

    publish_peer(&root, &key, 500);
    assert_eq!(
        sync::refresh(&config, &local, since, &[])
            .status
            .devices
            .len(),
        2
    );

    remove_object(&root, &key, PEER_ID);
    remove_object(&root, &key, &sync::local_device_id(&config));
    let status = sync::refresh(&config, &local, since, &[]).status;
    assert!(!status.published, "nothing changed, so nothing republished");
    assert_eq!(status.devices.len(), 2);

    let status = sync::refresh(&config, &local, since, &[]).status;
    assert!(
        status.published,
        "our missing object is put back, or peers would drop this machine"
    );
    assert_eq!(status.devices.len(), 2);
}

#[test]
fn a_folder_that_lost_every_peer_at_once_drops_nobody() {
    let root = scratch("unmounted");
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");

    publish_peer(&root, &key, 500);
    assert_eq!(
        sync::refresh(&config, &[event(0, 10, 1)], since, &[])
            .status
            .devices
            .len(),
        2
    );

    // The share is unmounted: the mount point is empty, and this cycle's
    // publish writes into it, so the listing holds our object and nothing else.
    std::fs::remove_dir_all(root.join("shared")).expect("unmount");
    let status = sync::refresh(&config, &[event(0, 20, 2)], since, &[]).status;
    assert!(status.published);
    assert_eq!(status.devices.len(), 2, "{:?}", status.devices);
}

/// Two machines, one of them the old entry, with the old one absorbed.
fn adopt_fixture(name: &str) -> (PathBuf, TokenGaugeConfig, FleetKey) {
    let root = scratch(name);
    let config = config(&root);
    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    sync::refresh(&config, &[event(0, 10, 1)], since, &[]);
    publish_device(
        &root,
        &key,
        record(OLD_ID, "box"),
        60,
        &[event(40 * 24, 500, 9)],
        &[],
    );
    sync::refresh(&config, &[event(0, 10, 1)], since, &[]);
    (root, config, key)
}

fn store_devices(config: &TokenGaugeConfig) -> Vec<String> {
    let (store, _) = sync::store::load(&config.cache_file);
    store.devices.keys().cloned().collect()
}

#[test]
fn with_sync_off_an_adoption_stays_on_this_machine() {
    let (root, mut config, key) = adopt_fixture("adopt-offline");
    config.sync.enabled = false;
    let before = std::fs::read(
        root.join("shared/v1")
            .join(key.object_name(&sync::local_device_id(&config))),
    )
    .expect("own object");

    let adoption = sync::adopt(&config, OLD_ID, || {}).expect("adopted locally");
    assert_eq!(adoption.buckets, 1);
    assert!(!store_devices(&config).contains(&OLD_ID.to_string()));
    assert!(
        objects(&root).contains(&key.object_name(OLD_ID)),
        "a disabled transport is not touched"
    );
    assert_eq!(
        std::fs::read(
            root.join("shared/v1")
                .join(key.object_name(&sync::local_device_id(&config)))
        )
        .expect("own object"),
        before,
        "nothing uploaded"
    );
}

#[test]
fn an_adoption_that_cannot_publish_deletes_nothing() {
    let (root, config, _) = adopt_fixture("adopt-unpublished");
    let shared = root.join("shared");
    std::fs::remove_dir_all(&shared).expect("clear");
    std::fs::write(&shared, b"not a folder").expect("block the transport");

    let failed = sync::adopt(&config, OLD_ID, || {}).expect_err("the publish fails");
    assert!(
        format!("{failed:#}").contains("nothing was deleted"),
        "{failed:#}"
    );
    assert!(
        store_devices(&config).contains(&OLD_ID.to_string()),
        "the store is not saved over an adoption that did not land"
    );
}

/// A reinstall after the 1st leaves the start of the month to the old entry:
/// the new machine reads the whole month and finds nothing there.
#[test]
fn an_adopted_day_inside_the_read_window_survives_a_refresh() {
    let root = scratch("adopt-in-window");
    let config = config(&root);
    let since = (Utc::now() - Duration::days(10))
        .with_timezone(&chrono::Local)
        .date_naive();
    let key = FleetKey::generate();
    sync::store_key(&config.cache_file, &key, true).expect("key");
    let local = vec![event(0, 10, 1)];

    sync::refresh(&config, &local, since, &[]);
    publish_device(
        &root,
        &key,
        record(OLD_ID, "box"),
        60,
        &[event(5 * 24, 300, 9)],
        &[],
    );
    sync::refresh(&config, &local, since, &[]);

    let adoption = sync::adopt(&config, OLD_ID, || {}).expect("adopted");
    assert_eq!(adoption.buckets, 1);

    for _ in 0..2 {
        let outcome = sync::refresh(&config, &local, since, &[]);
        assert_eq!(
            outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
            300,
            "the adopted day is counted, and kept, though the read window covers it"
        );
    }
}

/// An adoption whose delete failed leaves the retired object behind; the next
/// cycle has to finish the job rather than count the device again.
#[test]
fn a_retired_object_left_behind_is_deleted_by_the_next_cycle() {
    let (root, config, key) = adopt_fixture("adopt-leftover");
    let leftover = root.join("shared/v1").join(key.object_name(OLD_ID));
    let bytes = std::fs::read(&leftover).expect("retired object");

    sync::adopt(&config, OLD_ID, || {}).expect("adopted");
    std::fs::write(&leftover, bytes).expect("the delete never happened");

    let since = Utc::now().with_timezone(&chrono::Local).date_naive();
    let outcome = sync::refresh(&config, &[event(0, 10, 1)], since, &[]);
    assert!(
        !leftover.exists(),
        "the cycle deletes what the adoption could not"
    );
    assert_eq!(outcome.status.devices.len(), 1);
    assert_eq!(
        outcome.events.iter().map(|e| e.tokens.output).sum::<u64>(),
        500,
        "and the retired history is counted once"
    );
}

/// CodexBar #4617: a transcript root that is there and cannot be read reads
/// as no events. With another root of the same provider still readable, the
/// read starts at that root's oldest event and the store replaced everything
/// after it with what the readable root alone held.
#[test]
fn an_unreadable_transcript_root_keeps_the_history_it_had() {
    let root = scratch("unreadable");
    let config = TokenGaugeConfig {
        cache_file: root.join("tokengauge-usage.json"),
        ..TokenGaugeConfig::default()
    };
    let since = (Utc::now() - Duration::days(7))
        .with_timezone(&chrono::Local)
        .date_naive();
    let output = |store: &sync::FleetStore| -> u64 {
        store
            .devices
            .values()
            .flat_map(|slice| &slice.buckets)
            .map(|bucket| bucket.tokens.output)
            .sum()
    };

    // Two roots: the readable one holds the oldest event, the other the rest.
    let readable = event(40, 1, 1);
    let both = [readable.clone(), event(30, 100, 2), event(2, 100, 3)];
    assert_eq!(
        output(&sync::refresh(&config, &both, since, &[]).store),
        201
    );

    let unreadable = vec![root.join("projects")];
    let blind = sync::refresh(&config, &[readable], since, &unreadable);
    assert_eq!(output(&blind.store), 201, "the month must survive the gap");
    assert!(
        blind
            .status
            .error
            .as_deref()
            .is_some_and(|e| e.contains("could not be read")),
        "{:?}",
        blind.status.error
    );
}
