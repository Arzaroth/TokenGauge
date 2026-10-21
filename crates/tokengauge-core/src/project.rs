//! What selvedge cannot know: which binaries this is, where its releases live,
//! and what it ships beside them.

use selvedge::{Frontend, Project, Restart, SHORT_TARGETS, VersionSource};

/// Every executable the release archive carries, the primary one first. That
/// one also names the assets, so on Windows - where the archive is built
/// around the tray rather than the waybar binary - the order differs from the
/// Linux one rather than being a translation of it. macOS carries the Linux
/// pair for the daemon and the CLI, and the tray for the menu bar.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const BINARIES: &[&str] = &["tokengauge", "tokengauge-tui"];
#[cfg(target_os = "macos")]
const BINARIES: &[&str] = &["tokengauge", "tokengauge-tui", "tokengauge-tray"];
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
    targets: SHORT_TARGETS,
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
        std::fs::read_to_string(&path)
            .expect("the release workflow is what names the assets")
            .replace("\r\n", "\n")
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
        let target = arch_target(&TOKENGAUGE).expect("a supported arch builds this test");
        assert!(
            workflow
                .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
                .any(|token| token.contains(target) && token.ends_with(ARCHIVE_SUFFIX)),
            "no {target}{ARCHIVE_SUFFIX} asset in the release workflow"
        );
    }

    /// The test above sees only the platform running it, and CI runs it on
    /// three. A job dropped from the release workflow for a fourth would leave
    /// every machine on it with nothing to update to, and nothing here red.
    #[test]
    fn the_workflow_publishes_an_archive_for_every_platform() {
        let workflow = release_workflow();
        let tokens: Vec<_> = workflow
            .split(|c: char| c.is_whitespace() || c == '"' || c == ',')
            .collect();
        for (target, suffix) in [
            ("linux-x86_64", ".tar.gz"),
            ("linux-aarch64", ".tar.gz"),
            ("macos-x86_64", ".tar.gz"),
            ("macos-aarch64", ".tar.gz"),
            ("windows-x86_64", ".zip"),
        ] {
            assert!(
                tokens
                    .iter()
                    .any(|t| t.contains(target) && t.ends_with(suffix)),
                "no {target}{suffix} asset in the release workflow"
            );
        }
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

    /// One job of the release workflow, from its name to the next job's.
    fn job(workflow: &str, name: &str) -> String {
        let (_, rest) = workflow
            .split_once(&format!("\n  {name}:\n"))
            .unwrap_or_else(|| panic!("no {name} job"));
        rest.lines()
            .take_while(|line| {
                line.is_empty() || line.starts_with("   ") || line.starts_with("  #")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `install.sh` refuses an archive SHA256SUMS does not list, so the sums
    /// are taken once every build job has handed its assets over, and
    /// published with them.
    #[test]
    fn the_release_publishes_the_checksums_of_every_asset() {
        let workflow = release_workflow();
        let checksums = job(&workflow, "checksums");
        let sums = checksums
            .find("sha256sum -- * >SHA256SUMS")
            .expect("the release takes no checksums");
        let downloaded = checksums
            .find("actions/download-artifact")
            .expect("the checksums job downloads no assets");
        assert!(
            downloaded < sums,
            "the checksums are taken before the assets are in"
        );
        let uploaded = job(&workflow, "publish");
        let uploaded = uploaded.split("files: |").nth(1).expect("no files block");
        assert!(
            uploaded.contains("dist/SHA256SUMS"),
            "SHA256SUMS is not uploaded"
        );
    }

    fn repo_file(rel: &str) -> String {
        // A Windows checkout ends its lines in CRLF.
        std::fs::read_to_string(repo_root().join(rel))
            .unwrap_or_else(|e| panic!("{rel}: {e}"))
            .replace("\r\n", "\n")
    }

    const PKGBUILDS: [&str; 2] = [
        "packaging/arch/tokengauge/PKGBUILD",
        "packaging/arch/tokengauge-bin/PKGBUILD",
    ];

    /// A package puts each payload where its desktop loads system-wide
    /// extensions from; a payload anywhere else installs and is never drawn.
    /// The GNOME one also needs its schemas compiled, which the archive leaves
    /// to the machine that installs it.
    #[test]
    fn every_package_puts_the_desktop_payloads_where_the_desktop_looks() {
        let nfpm = repo_file("packaging/nfpm.yaml");
        for (id, dir) in [
            ("plasma", "/usr/share/plasma/plasmoids"),
            ("gnome", "/usr/share/gnome-shell/extensions"),
        ] {
            let f = TOKENGAUGE
                .frontends
                .iter()
                .find(|f| f.id == id)
                .expect("a frontend");
            // `tree`: a plain directory source flattens its subdirectories.
            assert!(
                nfpm.contains(&format!("    dst: {dir}/{}\n    type: tree\n", f.artifact)),
                "nfpm.yaml does not install the {id} payload as a tree under {dir}"
            );
            for rel in PKGBUILDS {
                assert!(
                    repo_file(rel).contains(&format!("/{}/{} \"$pkgdir{dir}/\"", f.id, f.artifact)),
                    "{rel} does not copy the {id} payload into {dir}"
                );
            }
        }
        assert!(
            repo_file("scripts/package.sh")
                .contains("glib-compile-schemas \"$stage/archive/frontends/gnome/"),
            "the .deb and .rpm ship the GNOME schemas uncompiled"
        );
        for rel in PKGBUILDS {
            assert!(
                repo_file(rel).contains("glib-compile-schemas \"$pkgdir/usr/share/gnome-shell/"),
                "{rel} ships the GNOME schemas uncompiled"
            );
        }
        assert!(
            repo_file("scripts/check-package.sh").contains("schemas/gschemas.compiled"),
            "the package check does not look for the compiled schemas"
        );
    }

    /// The unit runs `%h/.local/bin/tokengauge`, where install.sh puts it; a
    /// package puts it in /usr/bin, and each packager rewrites that one line.
    #[test]
    fn every_package_points_the_daemon_unit_at_usr_bin() {
        let unit = repo_file("scripts/tokengauge-daemon.service");
        assert!(unit.contains("\nExecStart=%h/.local/bin/tokengauge --daemon\n"));
        let rewrite = r"'s|^ExecStart=%h/\.local/bin/tokengauge |ExecStart=/usr/bin/tokengauge |'";
        for rel in ["scripts/package.sh", PKGBUILDS[0], PKGBUILDS[1]] {
            assert!(
                repo_file(rel).contains(rewrite),
                "{rel} does not rewrite ExecStart"
            );
        }
        assert!(
            repo_file("scripts/check-package.sh")
                .contains("'ExecStart=/usr/bin/tokengauge --daemon'"),
            "the package check does not look at the unit"
        );
    }

    /// Packages are made from the archive `--update` downloads, so the two
    /// cannot carry different bytes, and the archive carries the unit they
    /// install.
    #[test]
    fn the_packages_are_built_from_the_release_archive() {
        let workflow = release_workflow();
        assert!(workflow.contains("scripts/package.sh \"$archive\" dist"));
        assert!(workflow.contains("cp scripts/tokengauge-daemon.service dist/systemd/"));
        let bin = repo_file("packaging/arch/tokengauge-bin/PKGBUILD");
        for arch in ["x86_64", "aarch64"] {
            assert!(
                workflow.contains(&format!(
                    "-linux-{arch}.tar.gz\" -C dist tokengauge tokengauge-waybar tokengauge-tui frontends systemd"
                )),
                "the {arch} archive does not carry the unit"
            );
            assert!(
                bin.contains(&format!(
                    "/releases/download/v$pkgver/tokengauge-v$pkgver-linux-{arch}.tar.gz"
                )),
                "tokengauge-bin does not download the {arch} archive"
            );
        }
    }

    /// Nothing is published that the verify job has not installed, and every
    /// package and signature it checked is uploaded.
    #[test]
    fn the_release_uploads_every_package_once_it_has_been_installed() {
        let workflow = release_workflow();
        let publish = job(&workflow, "publish");
        assert!(
            publish.contains("needs: verify"),
            "the release publishes unverified packages"
        );
        let uploaded = publish.split("files: |").nth(1).expect("no files block");
        for glob in [
            "dist/*.deb",
            "dist/*.rpm",
            "dist/*.pkg.tar.zst",
            "dist/*.sig",
            "dist/*.asc",
        ] {
            assert!(uploaded.contains(glob), "{glob} is not uploaded");
        }
        assert!(job(&workflow, "verify").contains("sh /s/check-package.sh"));
    }

    /// A 0.22.x updater takes the first asset whose name contains
    /// `linux-<arch>`. The packaging tools name theirs `amd64`, `x86_64.rpm`
    /// and `-x86_64.pkg.tar.zst`, none of which it matches; the PKGBUILD
    /// bundle must not either.
    #[test]
    fn no_package_is_named_so_an_old_updater_could_mistake_it_for_the_archive() {
        let workflow = release_workflow();
        assert!(workflow.contains("\"out/tokengauge-$safe-pkgbuild.tar.gz\""));
        for rel in ["packaging/nfpm.yaml", PKGBUILDS[0], PKGBUILDS[1]] {
            for line in repo_file(rel).lines() {
                let line = line.trim();
                assert!(
                    !(line.starts_with("name:") || line.starts_with("pkgname="))
                        || !line.contains("linux-"),
                    "{rel}: `{line}` names a package an old updater would download"
                );
            }
        }
    }
}
