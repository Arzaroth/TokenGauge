//! Where the fleet store lives on disk.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::model::FleetStore;
use crate::write_atomic;

/// Derived from the snapshot's parent, like every other state file, but a
/// separate file: the snapshot is rewritten wholesale on every fetch and this
/// must not be.
pub fn store_path(cache_file: &Path) -> PathBuf {
    cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-fleet.json")
}

/// A store that cannot be read is replaced rather than fatal: losing fleet
/// history is bad, but refusing to draw the panel over it is worse, and the
/// local slice rebuilds from transcripts on the next fetch.
///
/// A missing store and an unreadable one are not the same event, though. The
/// second discards the only record of past days, so it is reported rather than
/// waved through, and the rejected file is kept for diagnosis.
pub fn load(cache_file: &Path) -> (FleetStore, Option<String>) {
    let path = store_path(cache_file);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (FleetStore::default(), None),
        Err(e) => {
            return (
                FleetStore::default(),
                Some(format!("could not read {}: {e}", path.display())),
            );
        }
    };

    match serde_json::from_slice::<FleetStore>(&bytes) {
        Ok(store) => (store, None),
        Err(e) => {
            let kept = path.with_extension("json.rejected");
            let saved = std::fs::write(&kept, &bytes).is_ok();
            (
                FleetStore::default(),
                Some(format!(
                    "the fleet store did not parse ({e}); starting a fresh one{}",
                    if saved {
                        format!(" and keeping the old at {}", kept.display())
                    } else {
                        String::new()
                    }
                )),
            )
        }
    }
}

pub fn save(cache_file: &Path, store: &FleetStore) -> Result<()> {
    let path = store_path(cache_file);
    let bytes = serde_json::to_vec(store).context("could not serialise the fleet store")?;
    write_atomic(&path, &bytes).with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tokengauge-store-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        dir.join("tokengauge-usage.json")
    }

    #[test]
    fn a_missing_store_is_not_an_incident() {
        let (store, error) = load(&scratch("missing"));
        assert!(store.devices.is_empty());
        assert_eq!(
            error, None,
            "a fleet that has never synced is not a failure"
        );
    }

    #[test]
    fn an_unreadable_store_is_reported_and_kept() {
        let cache_file = scratch("corrupt");
        let path = store_path(&cache_file);
        std::fs::write(&path, b"{not json at all").expect("write");

        let (store, error) = load(&cache_file);
        assert!(store.devices.is_empty());
        let error = error.expect("discarding the only record of past days must be reported");
        assert!(error.contains("did not parse"), "{error}");

        let kept = path.with_extension("json.rejected");
        assert!(kept.exists(), "the rejected file is kept for diagnosis");
        assert_eq!(std::fs::read(&kept).expect("read"), b"{not json at all");
    }
}
