//! Desktop frontends that are not binaries.
//!
//! The Plasma applet, the GNOME extension, and the Omarchy bar widget are QML
//! and JavaScript installed outside `~/.local/bin`, so replacing the binaries
//! leaves them untouched. A 0.18.0 binary feeding 0.17.0 QML looks like a
//! missing feature rather than a stale install, which is exactly how it was
//! first reported: the snapshot carried the flag, the applet did not read it.
//!
//! Everything here is path-and-copy work with no network. [`crate::update`]
//! owns fetching a release; this owns knowing where each artifact belongs, what
//! version is currently sitting there, and how to replace it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Directory inside the release archive holding the frontend payloads.
pub const ARCHIVE_ROOT: &str = "frontends";

/// What has to happen before a freshly installed frontend is actually running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// Picks itself up, or needs a cheap restart the user already knows about.
    Cheap(&'static str),
    /// Needs the whole session restarted - GNOME Shell cannot reload an
    /// extension on Wayland without logging out.
    Session(&'static str),
}

impl Restart {
    pub fn hint(&self) -> &'static str {
        match self {
            Restart::Cheap(h) | Restart::Session(h) => h,
        }
    }

    pub fn needs_session_restart(&self) -> bool {
        matches!(self, Restart::Session(_))
    }
}

/// How a frontend records its own version, so skew against the binary is
/// visible rather than silent.
#[derive(Debug, Clone, Copy)]
enum VersionSource {
    /// `metadata.json` -> `KPlugin.Version` (Plasma).
    PlasmaMetadata,
    /// `metadata.json` -> `version-name` (GNOME).
    GnomeMetadata,
    /// `manifest.json` -> `version` (Omarchy plugin schema).
    ManifestVersion,
}

#[derive(Debug, Clone, Copy)]
pub struct Frontend {
    /// Value accepted by `--install-frontend`.
    pub id: &'static str,
    pub label: &'static str,
    /// Path of the payload inside the archive, below [`ARCHIVE_ROOT`], and the
    /// same path below the repository root in a checkout.
    pub payload: &'static str,
    /// Directory name the payload lands in; the id its desktop expects.
    pub artifact: &'static str,
    version_source: VersionSource,
    pub restart: Restart,
}

pub const FRONTENDS: &[Frontend] = &[
    Frontend {
        id: "plasma",
        label: "KDE Plasma applet",
        payload: "plasma/org.tokengauge.plasmoid",
        artifact: "org.tokengauge.plasmoid",
        version_source: VersionSource::PlasmaMetadata,
        restart: Restart::Cheap("kquitapp6 plasmashell && kstart plasmashell"),
    },
    Frontend {
        id: "gnome",
        label: "GNOME Shell extension",
        payload: "gnome/tokengauge@arzaroth.github.io",
        artifact: "tokengauge@arzaroth.github.io",
        version_source: VersionSource::GnomeMetadata,
        restart: Restart::Session(
            "log out and back in, then: gnome-extensions enable tokengauge@arzaroth.github.io",
        ),
    },
    Frontend {
        id: "omarchy",
        label: "Omarchy bar widget",
        payload: "omarchy/arzaroth.tokengauge",
        artifact: "arzaroth.tokengauge",
        version_source: VersionSource::ManifestVersion,
        restart: Restart::Cheap("omarchy-restart-shell"),
    },
];

pub fn find(id: &str) -> Option<&'static Frontend> {
    let id = id.trim().to_lowercase();
    FRONTENDS.iter().find(|f| f.id == id)
}

fn home() -> Option<PathBuf> {
    // Windows spells it differently, and the core crate builds there even
    // though none of these desktops exist on it - without the fallback
    // `dest_dir` is None for every frontend and the registry looks broken.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => home().map(|h| h.join(".local/share")),
    }
}

fn config_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => home().map(|h| h.join(".config")),
    }
}

impl Frontend {
    /// Where this frontend's desktop expects to find it.
    pub fn dest_dir(&self) -> Option<PathBuf> {
        match self.id {
            "plasma" => data_home().map(|d| d.join("plasma/plasmoids").join(self.artifact)),
            "gnome" => data_home().map(|d| d.join("gnome-shell/extensions").join(self.artifact)),
            "omarchy" => config_home().map(|c| c.join("omarchy/plugins").join(self.artifact)),
            _ => None,
        }
    }

    pub fn is_installed(&self) -> bool {
        self.dest_dir().is_some_and(|d| d.is_dir())
    }

    /// The version recorded in the installed copy, which is the one actually
    /// running - not the version of the binary asking the question.
    pub fn installed_version(&self) -> Option<String> {
        self.version_in(&self.dest_dir()?)
    }

    fn version_in(&self, dir: &Path) -> Option<String> {
        let (file, pointer) = match self.version_source {
            VersionSource::PlasmaMetadata => ("metadata.json", "/KPlugin/Version"),
            VersionSource::GnomeMetadata => ("metadata.json", "/version-name"),
            VersionSource::ManifestVersion => ("manifest.json", "/version"),
        };
        let raw = std::fs::read_to_string(dir.join(file)).ok()?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let value = parsed.pointer(pointer)?;
        // Plasma writes it as a string; be forgiving about a number.
        let text = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other
                .as_str()
                .map(str::to_string)
                .unwrap_or(other.to_string()),
        };
        let text = text.trim().trim_start_matches('v').to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Copy the payload out of an extracted archive (or a checkout) into place,
    /// replacing whatever is there.
    ///
    /// `source_root` is the directory holding [`ARCHIVE_ROOT`], or a repository
    /// checkout root, so the same call serves both `--update` and a dev install.
    pub fn install_from(&self, source_root: &Path) -> Result<PathBuf> {
        let dest = self.dest_dir().ok_or_else(|| {
            anyhow::anyhow!("cannot resolve an install directory for {}", self.id)
        })?;
        self.install_into(source_root, &dest)
    }

    /// The body of [`install_from`] with the destination supplied, so a test can
    /// exercise the real staging and replacement without mutating the process
    /// environment to move `$XDG_DATA_HOME`.
    pub fn install_into(&self, source_root: &Path, dest: &Path) -> Result<PathBuf> {
        let src = self.payload_in(source_root).ok_or_else(|| {
            anyhow::anyhow!(
                "{} payload not found under {}",
                self.label,
                source_root.display()
            )
        })?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }

        // Staged beside the destination so the final move is a rename on the
        // same filesystem, and hidden so a half-written copy is not picked up by
        // the scanners that walk these directories - Omarchy's plugin catalog
        // skips dotted entries, and neither plasmashell nor GNOME Shell lists
        // them. The name is built from the whole directory name: every artifact
        // id contains dots, so `with_extension` would have truncated
        // `org.tokengauge.plasmoid` to `org.tokengauge.tg-new` and could shadow
        // an unrelated directory.
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("cannot name a staging dir for {}", dest.display()))?;
        let staged = dest.with_file_name(format!(".{name}.tg-new"));
        let _ = std::fs::remove_dir_all(&staged);

        // Replace rather than merge: a file dropped upstream has to disappear
        // here too, or a stale QML file keeps being loaded alongside the new one.
        if let Err(e) = copy_dir(&src, &staged) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e)
                .with_context(|| format!("cannot stage {} into {}", self.label, staged.display()));
        }

        let _ = std::fs::remove_dir_all(dest);
        if let Err(e) = std::fs::rename(&staged, dest) {
            let _ = std::fs::remove_dir_all(&staged);
            return Err(e).with_context(|| format!("cannot move {} into place", self.label));
        }
        Ok(dest.to_path_buf())
    }

    /// Locate this frontend's payload under an archive root or a checkout.
    pub fn payload_in(&self, source_root: &Path) -> Option<PathBuf> {
        let archived = source_root.join(ARCHIVE_ROOT).join(self.payload);
        if archived.is_dir() {
            return Some(archived);
        }
        let in_checkout = source_root.join(self.payload);
        in_checkout.is_dir().then_some(in_checkout)
    }
}

/// Every frontend already present on this machine.
pub fn installed() -> Vec<&'static Frontend> {
    FRONTENDS.iter().filter(|f| f.is_installed()).collect()
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dest.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &target)?;
        } else if file_type.is_symlink() {
            // The Omarchy plugin registry refuses a plugin folder containing
            // one, and none of the payloads ship any; refusing beats copying
            // something that resolves differently on the target machine.
            bail!("refusing to copy symlink {}", entry.path().display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_resolve_case_insensitively() {
        assert_eq!(find("plasma").unwrap().id, "plasma");
        assert_eq!(find("  GNOME ").unwrap().id, "gnome");
        assert!(find("aqua").is_none());
    }

    #[test]
    fn every_frontend_resolves_a_destination() {
        // A frontend whose id is not handled in dest_dir() would silently be
        // uninstallable, and `--update` would skip it without a word.
        for f in FRONTENDS {
            assert!(f.dest_dir().is_some(), "{} has no destination", f.id);
        }
    }

    #[test]
    fn payload_is_found_in_an_archive_or_a_checkout() {
        let tmp = std::env::temp_dir().join(format!("tg-frontend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let plasma = find("plasma").unwrap();

        std::fs::create_dir_all(tmp.join(ARCHIVE_ROOT).join(plasma.payload)).unwrap();
        assert_eq!(
            plasma.payload_in(&tmp).unwrap(),
            tmp.join(ARCHIVE_ROOT).join(plasma.payload)
        );

        let checkout = tmp.join("checkout");
        std::fs::create_dir_all(checkout.join(plasma.payload)).unwrap();
        assert_eq!(
            plasma.payload_in(&checkout).unwrap(),
            checkout.join(plasma.payload)
        );

        assert!(plasma.payload_in(&tmp.join("empty")).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn every_payload_exists_in_the_repository() {
        // The release workflow copies these directories into the archive by
        // name. If a payload is renamed here without the workflow following,
        // `--update` silently stops refreshing that frontend; if it is renamed
        // in the repository, the archive ships an empty directory. Either way
        // the failure is invisible at build time, so pin it here.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf();
        for f in FRONTENDS {
            let dir = repo.join(f.payload);
            assert!(dir.is_dir(), "{} payload missing: {}", f.id, dir.display());
            assert_eq!(
                dir.file_name().and_then(|n| n.to_str()),
                Some(f.artifact),
                "{} payload does not end in the artifact directory its desktop expects",
                f.id
            );
            assert!(
                f.version_in(&dir).is_some(),
                "{} payload carries no readable version; skew would be undetectable",
                f.id
            );
        }
    }

    #[test]
    fn archive_layout_resolves_every_frontend() {
        let tmp = std::env::temp_dir().join(format!("tg-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for f in FRONTENDS {
            std::fs::create_dir_all(tmp.join(ARCHIVE_ROOT).join(f.payload)).unwrap();
        }
        for f in FRONTENDS {
            assert!(
                f.payload_in(&tmp).is_some(),
                "{} does not resolve in the archive layout release.yml builds",
                f.id
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_is_read_from_each_metadata_shape() {
        let tmp = std::env::temp_dir().join(format!("tg-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        std::fs::write(
            tmp.join("metadata.json"),
            r#"{"KPlugin":{"Version":"0.18.0"}}"#,
        )
        .unwrap();
        assert_eq!(
            find("plasma").unwrap().version_in(&tmp).as_deref(),
            Some("0.18.0")
        );

        std::fs::write(tmp.join("metadata.json"), r#"{"version-name":"v0.18.0"}"#).unwrap();
        assert_eq!(
            find("gnome").unwrap().version_in(&tmp).as_deref(),
            Some("0.18.0"),
            "a leading v must not read as a different version"
        );

        std::fs::write(tmp.join("manifest.json"), r#"{"version":"0.18.0"}"#).unwrap();
        assert_eq!(
            find("omarchy").unwrap().version_in(&tmp).as_deref(),
            Some("0.18.0")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn install_replaces_rather_than_merges() {
        let plasma = find("plasma").unwrap();
        let tmp = std::env::temp_dir().join(format!("tg-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);

        // A real archive layout, so payload_in, the staging path, and the
        // replacement all run exactly as they do under `--update`.
        let archive = tmp.join("archive");
        let payload = archive.join(ARCHIVE_ROOT).join(plasma.payload);
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("kept.qml"), "new").unwrap();
        std::fs::write(
            payload.join("metadata.json"),
            r#"{"KPlugin":{"Version":"9.9.9"}}"#,
        )
        .unwrap();

        let dest = tmp.join("plasmoids").join(plasma.artifact);
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("gone.qml"), "old").unwrap();

        let installed = plasma.install_into(&archive, &dest).unwrap();
        assert_eq!(installed, dest);
        assert!(dest.join("kept.qml").exists());
        assert!(
            !dest.join("gone.qml").exists(),
            "a file dropped upstream must not survive the install"
        );
        assert_eq!(plasma.version_in(&dest).as_deref(), Some("9.9.9"));

        // The staging directory is a dotted sibling built from the whole
        // artifact name, and nothing is left behind once the move lands.
        let staged = dest.with_file_name(format!(".{}.tg-new", plasma.artifact));
        assert!(
            !staged.exists(),
            "staging dir survived a successful install"
        );
        assert!(
            !dest.with_extension("tg-new").exists(),
            "staging must not truncate the dotted artifact name"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_failed_install_leaves_nothing_staged() {
        let plasma = find("plasma").unwrap();
        let tmp = std::env::temp_dir().join(format!("tg-install-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dest = tmp.join("plasmoids").join(plasma.artifact);
        std::fs::create_dir_all(&dest).unwrap();

        // No payload in the source, so the install fails before staging; the
        // scanners that walk this directory must not find a partial copy.
        let err = plasma.install_into(&tmp.join("empty"), &dest);
        assert!(err.is_err());
        assert!(
            !dest
                .with_file_name(format!(".{}.tg-new", plasma.artifact))
                .exists(),
            "a failed install must not leave a staged directory behind"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
