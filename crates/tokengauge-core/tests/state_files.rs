//! Every state file is derived from the snapshot's parent.
//!
//! That is the rule the whole of `--config` rests on: point `cache_file`
//! somewhere else and the daemon socket, the refresh sentinel, the revision
//! file, the selected provider, the notify state, the price table and the
//! fleet store all follow it there. It is how a developer runs a freshly built
//! binary beside an installed daemon without the two fighting over one
//! snapshot, and how the end-to-end tests get an isolated machine at all.
//!
//! Every one of these derivations was written the same way and none of them
//! were tested. A single one that reached for `$XDG_STATE_HOME` instead would
//! leak one file back into the real directory, which is exactly the kind of
//! thing nobody notices until a test suite starts editing a developer's own
//! config.

use std::path::{Path, PathBuf};

use tokengauge_core::cost::pricing;
use tokengauge_core::statefiles;
use tokengauge_core::sync;

/// Every file the core derives from a snapshot path, by the name it derives.
///
/// The list is spelled out rather than discovered, so adding a state file is a
/// line here as well - which is the point at which someone has to decide it
/// really does belong beside the snapshot.
fn derived(cache_file: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("revision", statefiles::revision_path(cache_file)),
        (
            "refresh sentinel",
            statefiles::refresh_sentinel_path(cache_file),
        ),
        ("waybar state", statefiles::waybar_state_path(cache_file)),
        ("update status", statefiles::update_status_path(cache_file)),
        ("notify state", statefiles::notify_state_path(cache_file)),
        (
            "backfill marker",
            statefiles::backfill_marker_path(cache_file),
        ),
        ("price table", pricing::price_cache_path(cache_file)),
        ("fleet store", sync::store::store_path(cache_file)),
        ("fleet key", sync::crypto::key_path(cache_file)),
    ]
}

#[test]
fn every_state_file_is_a_sibling_of_the_snapshot() {
    let cache = Path::new("/somewhere/else/entirely/tokengauge-usage.json");
    let parent = cache.parent().expect("a parent");

    for (what, path) in derived(cache) {
        assert_eq!(
            path.parent(),
            Some(parent),
            "the {what} file is not beside the snapshot: {}",
            path.display()
        );
        assert!(
            path.file_name().is_some(),
            "the {what} file has no name of its own"
        );
    }
}

/// A `--config` pointing somewhere else takes every state file with it. This
/// is the property a developer relies on to run a new binary beside the
/// installed daemon, and the one the end-to-end tests rely on for isolation.
#[test]
fn moving_the_snapshot_moves_all_of_them() {
    let here = derived(Path::new("/one/tokengauge-usage.json"));
    let there = derived(Path::new("/two/tokengauge-usage.json"));

    assert_eq!(here.len(), there.len());
    for ((what, a), (_, b)) in here.iter().zip(there.iter()) {
        assert_ne!(a, b, "the {what} file stayed put when the snapshot moved");
        assert_eq!(
            a.file_name(),
            b.file_name(),
            "the {what} file changed its name as well as its directory"
        );
        assert!(
            b.starts_with("/two"),
            "the {what} file did not follow the snapshot: {}",
            b.display()
        );
    }
}

/// No two of them are the same file. They are written independently and at
/// different rates - the snapshot wholesale on every fetch, the store never
/// wholesale - so a collision would silently truncate one of them.
#[test]
fn no_two_state_files_share_a_name() {
    let paths = derived(Path::new("/tmp/tokengauge-usage.json"));
    for (i, (what, a)) in paths.iter().enumerate() {
        for (other, b) in paths.iter().skip(i + 1) {
            assert_ne!(a, b, "the {what} and {other} files are the same path");
        }
    }
    // And none of them is the snapshot itself, which is rewritten wholesale.
    let cache = PathBuf::from("/tmp/tokengauge-usage.json");
    for (what, path) in &paths {
        assert_ne!(
            path, &cache,
            "the {what} file would be clobbered by a fetch"
        );
    }
}

/// A bare filename has no parent, and `Path::parent` answers `Some("")` for
/// one rather than `None`. Every derivation handles it the same way; a panic
/// here would take the whole panel down over a config typo.
#[test]
fn a_snapshot_with_no_directory_still_derives_its_neighbours() {
    for (what, path) in derived(Path::new("tokengauge-usage.json")) {
        assert!(
            !path.as_os_str().is_empty(),
            "the {what} file resolved to nothing"
        );
        assert!(
            path.file_name().is_some(),
            "the {what} file has no name: {}",
            path.display()
        );
    }
}

/// The names users already have on disk. Renaming one silently abandons
/// whatever it held - and two of these hold the only record there is: the
/// fleet store carries past days' tokens, and the key decrypts what peers
/// published under it.
#[test]
fn the_names_on_disk_are_the_names_users_already_have() {
    let names: Vec<String> = derived(Path::new("/x/tokengauge-usage.json"))
        .into_iter()
        .map(|(_, p)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        names,
        [
            "tokengauge-revision",
            "tokengauge-refreshing",
            // Deliberately still "waybar": it is the waybar scroll selection,
            // a state file users have, and it is not renamed with the binary.
            "tokengauge-waybar-state.json",
            "tokengauge-update.json",
            "tokengauge-notify-state.json",
            "tokengauge-backfilled",
            "tokengauge-prices.json",
            "tokengauge-fleet.json",
            "tokengauge-sync-key",
        ]
    );
}
