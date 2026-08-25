//! Moving one sealed object per device, over storage the user already has.
//!
//! Every device writes exactly one object and never anyone else's, which is
//! what removes conflict resolution from the design: there is no shared writer
//! to race.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{SyncConfig, SyncTransportKind};

/// One object as the storage lists it. `version` is opaque and only ever
/// compared to itself, so each transport picks whatever it can report cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEntry {
    pub name: String,
    pub version: String,
    pub size: u64,
}

pub trait Transport: Send {
    fn describe(&self) -> String;
    fn put(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn list(&self) -> Result<Vec<PeerEntry>>;
    /// `None` when `known_version` still matches, so nothing was transferred.
    fn get(&self, entry: &PeerEntry, known_version: Option<&str>) -> Result<Option<Vec<u8>>>;
    fn delete(&self, name: &str) -> Result<()>;
}

pub fn open(config: &SyncConfig) -> Result<Box<dyn Transport>> {
    match config.transport {
        SyncTransportKind::Dir => {
            if config.dir.path.as_os_str().is_empty() {
                bail!("[sync.dir] path is not set");
            }
            Ok(Box::new(DirTransport::new(&config.dir.path)))
        }
        SyncTransportKind::S3 => Ok(Box::new(super::s3::S3Transport::new(
            &config.s3,
            std::time::Duration::from_secs(30),
        )?)),
    }
}

/// An object name is 32 hex characters and nothing else, which also filters out
/// the conflict copies a sync tool leaves behind (`…sync-conflict-….tgsync`).
pub fn is_object_name(name: &str) -> bool {
    match name.strip_suffix(".tgsync") {
        Some(stem) => stem.len() == 32 && stem.bytes().all(|b| b.is_ascii_hexdigit()),
        None => false,
    }
}

pub struct DirTransport {
    root: PathBuf,
}

impl DirTransport {
    pub fn new(path: &Path) -> Self {
        Self {
            root: expand_home(path).join("v1"),
        }
    }
}

impl Transport for DirTransport {
    fn describe(&self) -> String {
        format!("dir:{}", self.root.display())
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("could not create {}", self.root.display()))?;
        crate::write_atomic(&self.root.join(name), bytes)
            .with_context(|| format!("could not write {name}"))
    }

    fn list(&self) -> Result<Vec<PeerEntry>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // Nothing published yet is not a failure; it is a fleet of one.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("could not read {}", self.root.display()));
            }
        };

        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_object_name(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let stamp = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis())
                .unwrap_or(0);
            found.push(PeerEntry {
                name,
                version: format!("{stamp}:{}", meta.len()),
                size: meta.len(),
            });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    fn get(&self, entry: &PeerEntry, known_version: Option<&str>) -> Result<Option<Vec<u8>>> {
        if known_version == Some(entry.version.as_str()) {
            return Ok(None);
        }
        std::fs::read(self.root.join(&entry.name))
            .map(Some)
            .with_context(|| format!("could not read {}", entry.name))
    }

    fn delete(&self, name: &str) -> Result<()> {
        match std::fs::remove_file(self.root.join(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("could not delete {name}")),
        }
    }
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}
