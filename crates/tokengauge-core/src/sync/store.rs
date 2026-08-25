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
pub fn load(cache_file: &Path) -> FleetStore {
    let path = store_path(cache_file);
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FleetStore>(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(cache_file: &Path, store: &FleetStore) -> Result<()> {
    let path = store_path(cache_file);
    let bytes = serde_json::to_vec(store).context("could not serialise the fleet store")?;
    write_atomic(&path, &bytes).with_context(|| format!("could not write {}", path.display()))
}
