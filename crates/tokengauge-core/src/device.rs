//! Which machine this is, for the fleet.
//!
//! The id is derived from the system's own machine id rather than being it: the
//! raw value is used as a secret by other software, and a fleet object named
//! with it would leak that to whoever holds the storage. A generated id in the
//! state directory covers a machine with no stable one - a container, mostly -
//! at the cost of a rebuild looking like a new device.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::write_atomic;

/// Which machine wrote a snapshot. Recorded next to the payloads so snapshots
/// collected from several machines can be told apart and reconciled later;
/// nothing merges them yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub machine_id: String,
    pub hostname: String,
}

/// The machine this snapshot was written on. Resolved once per process.
pub fn device_identity(cache_file: &Path) -> DeviceIdentity {
    static IDENTITY: std::sync::OnceLock<DeviceIdentity> = std::sync::OnceLock::new();
    IDENTITY
        .get_or_init(|| DeviceIdentity {
            machine_id: machine_id(cache_file),
            hostname: hostname(),
        })
        .clone()
}

/// Domain separator for the device id. Bumping it re-keys every machine, which
/// a future snapshot merge would read as a fleet of new devices.
const DEVICE_ID_DOMAIN: &str = "tokengauge.device-id.v1";

/// `/etc/machine-id` is confidential - systemd's own documentation says an
/// application wanting a stable host identifier must derive one rather than
/// hand the id out, and this snapshot is written to be synced. The digest is
/// stable for a machine and the id it came from cannot be read back out of it.
/// Same shape as `sd_id128_get_machine_app_specific`.
fn derive_device_id(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_DOMAIN.as_bytes());
    hasher.update([0u8]);
    hasher.update(raw.as_bytes());
    hex16(&hasher.finalize())
}

fn hex16(digest: &[u8]) -> String {
    use std::fmt::Write;
    digest.iter().take(16).fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn machine_id(cache_file: &Path) -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let id = contents.trim();
            if !id.is_empty() {
                return derive_device_id(id);
            }
        }
    }
    // Windows, macOS, or a container without systemd: keep one beside the
    // snapshot instead, so it is at least stable for this user on this machine.
    let path = cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-device-id");
    if let Ok(contents) = fs::read_to_string(&path) {
        let id = contents.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let generated = generated_machine_id();
    let _ = write_atomic(&path, generated.as_bytes());
    generated
}

/// Only reached where there is no system id to derive from. Generated once and
/// kept, so it needs to be unique rather than unguessable.
fn generated_machine_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_ID_DOMAIN.as_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(hostname().as_bytes());
    hex16(&hasher.finalize())
}

fn hostname() -> String {
    // The kernel first: a shell's exported HOSTNAME can outlive a rename.
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let name = contents.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(name) = std::env::var(key) {
            let name = name.trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_derived_not_the_system_one() {
        let raw = "0123456789abcdef0123456789abcdef";
        let derived = derive_device_id(raw);

        // Stable for a machine, and the system id cannot be read back out of it.
        assert_eq!(derived, derive_device_id(raw));
        assert_ne!(derived, raw);
        assert!(!derived.contains(raw));
        assert_eq!(derived.len(), 32);
        assert!(derived.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            derived,
            derive_device_id("0123456789abcdef0123456789abcdee")
        );
    }
}
