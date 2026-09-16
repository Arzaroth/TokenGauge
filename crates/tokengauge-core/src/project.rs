//! What selvedge cannot know: which binaries this is, where its releases live,
//! and what it ships beside them.

use selvedge::{Frontend, Project, Restart, VersionSource};

/// Every executable the release archive carries, the primary one first. That
/// one also names the assets, so on Windows - where the archive is built
/// around the tray rather than the waybar binary - the order differs from the
/// Linux one rather than being a translation of it.
#[cfg(not(target_os = "windows"))]
const BINARIES: &[&str] = &["tokengauge", "tokengauge-tui"];
#[cfg(target_os = "windows")]
const BINARIES: &[&str] = &["tokengauge-tui.exe", "tokengauge-tray.exe"];

pub const TOKENGAUGE: Project = Project {
    binaries: BINARIES,
    repo: "Arzaroth/TokenGauge",
    // So a fork can self-update from its own releases.
    repo_env: "TOKENGAUGE_REPO",
    // This crate's version, not selvedge's: `CARGO_PKG_VERSION` evaluated
    // inside selvedge would report the library.
    version: env!("CARGO_PKG_VERSION"),
    frontends: FRONTENDS,
    // The updater performing an upgrade is the *old* binary, and it only knows
    // to look for the old name.
    aliases: &["tokengauge-waybar"],
    legacy: &[],
    msi_marker_key: Some(r"HKCU\Software\TokenGauge"),
};

pub const FRONTENDS: &[Frontend] = &[
    Frontend {
        id: "plasma",
        label: "KDE Plasma applet",
        payload: "plasma/org.tokengauge.plasmoid",
        artifact: "org.tokengauge.plasmoid",
        version_source: VersionSource::PlasmaMetadata,
        gsettings_schemas: false,
        compiled: false,
        restart: Restart::Cheap("kquitapp6 plasmashell && kstart plasmashell"),
    },
    Frontend {
        id: "gnome",
        label: "GNOME Shell extension",
        payload: "gnome/tokengauge@arzaroth.github.io",
        artifact: "tokengauge@arzaroth.github.io",
        version_source: VersionSource::GnomeMetadata,
        gsettings_schemas: true,
        // TypeScript: the directory beside this one holds the sources, and
        // installing those lands an extension the shell refuses to load.
        compiled: true,
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
        gsettings_schemas: false,
        compiled: false,
        restart: Restart::Cheap("omarchy-restart-shell"),
    },
];

/// The contract between this repository and the release it publishes.
///
/// selvedge knows how to fetch an archive and install what is in it. What it
/// cannot know is whether *this* repository builds the assets it will go
/// looking for, so these read the workflow and the payload directories. They
/// were inner tests of the modules that moved out; the machinery went with
/// them and this stayed.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .to_path_buf()
    }

    fn release_workflow() -> String {
        let path = repo_root().join(".github/workflows/release.yml");
        std::fs::read_to_string(&path).expect("the release workflow is what names the assets")
    }

    #[test]
    fn every_payload_exists_in_the_repository() {
        // The release workflow copies these directories into the archive by
        // name. If a payload is renamed here without the workflow following,
        // `--update` silently stops refreshing that frontend; if it is renamed
        // in the repository, the archive ships an empty directory. Either way
        // the failure is invisible at build time, so pin it here.
        let repo = repo_root();
        for f in TOKENGAUGE.frontends {
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

    /// Asset names, not the workflow, are the compatibility surface: every
    /// updater already shipped matches by substring. So the archive this build
    /// asks for has to be one the workflow actually publishes.
    #[cfg(feature = "self-update")]
    #[test]
    fn the_workflow_publishes_the_asset_this_build_asks_for() {
        use selvedge::update::{ARCHIVE_SUFFIX, arch_target};
        let workflow = release_workflow();
        let target = arch_target().expect("a supported arch builds this test");
        assert!(
            workflow
                .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
                .any(|token| token.contains(target) && token.ends_with(ARCHIVE_SUFFIX)),
            "no {target}{ARCHIVE_SUFFIX} asset in the release workflow"
        );
    }

    /// The MSI is named `win64` rather than `windows-x86_64` on purpose: a
    /// 0.22.x updater asks for no suffix at all and takes whichever asset
    /// matches the platform first, so an MSI carrying the platform string
    /// would be handed to the zip extractor on machines whose binaries can no
    /// longer be changed.
    #[test]
    fn no_msi_is_named_so_an_old_updater_could_mistake_it_for_the_archive() {
        for token in release_workflow().split(|c: char| c.is_whitespace() || c == '"' || c == ',') {
            let token = token.trim_end_matches('`');
            if token.ends_with(".msi") {
                assert!(
                    !token.contains("windows-x86_64"),
                    "`{token}` is reachable by an updater asking for the platform alone"
                );
            }
        }
    }

    /// The alias is a symlink an old updater looks for by name. Nothing else
    /// declares it, so a rename here is the whole change.
    #[test]
    fn the_legacy_binary_name_is_still_carried() {
        assert!(
            TOKENGAUGE.aliases.contains(&"tokengauge-waybar"),
            "a 0.22.x updater upgrades by the old name and would find nothing"
        );
    }
}
