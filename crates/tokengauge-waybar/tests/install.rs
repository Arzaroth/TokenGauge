//! `scripts/install.sh` the way `curl ... | bash` runs it: read from stdin,
//! with no checkout around it, against a release laid out in a directory.
//! Linux only: on macOS the script registers LaunchAgents.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn installer() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => panic!("no release for {other}"),
    }
}

struct Release {
    root: PathBuf,
}

impl Release {
    /// A release directory holding this build's binary in the archive layout,
    /// and the SHA256SUMS the release workflow writes beside it. The TUI is
    /// another crate's binary, so a script stands in for it.
    fn new() -> Release {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("tg-install-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let stage = root.join("stage");
        let dist = root.join("dist");
        std::fs::create_dir_all(&stage).expect("a stage");
        std::fs::create_dir_all(&dist).expect("a dist");
        std::fs::copy(env!("CARGO_BIN_EXE_tokengauge"), stage.join("tokengauge"))
            .expect("the binary");
        std::fs::write(stage.join("tokengauge-tui"), "#!/bin/sh\n").expect("the TUI");

        let archive = format!("tokengauge-v{VERSION}-linux-{}.tar.gz", arch());
        run(Command::new("tar")
            .arg("-czf")
            .arg(dist.join(&archive))
            .arg("-C")
            .arg(&stage)
            .arg("."));
        let sums = run(Command::new("sha256sum")
            .arg("--")
            .arg(&archive)
            .current_dir(&dist));
        std::fs::write(dist.join("SHA256SUMS"), sums.stdout).expect("SHA256SUMS");
        std::fs::create_dir_all(root.join("home")).expect("a home");
        Release { root }
    }

    fn archive(&self) -> PathBuf {
        self.root
            .join("dist")
            .join(format!("tokengauge-v{VERSION}-linux-{}.tar.gz", arch()))
    }

    fn bindir(&self) -> PathBuf {
        self.root.join("home/.local/bin")
    }

    fn install(&self, args: &[&str]) -> Output {
        let script = std::fs::File::open(installer()).expect("the installer");
        Command::new("bash")
            .arg("-s")
            .arg("--")
            .args(args)
            .stdin(Stdio::from(script))
            .current_dir(&self.root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.root.join("home"))
            .env("TOKENGAUGE_DOWNLOAD_BASE", self.root.join("dist"))
            .output()
            .expect("bash runs")
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn run(command: &mut Command) -> Output {
    let out = command.output().expect("the command runs");
    assert!(
        out.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn a_piped_install_puts_the_release_binaries_and_the_old_name_on_the_machine() {
    let release = Release::new();
    let out = release.install(&["--no-daemon"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let binary = release.bindir().join("tokengauge");
    let printed = run(Command::new(&binary).arg("--version")).stdout;
    assert_eq!(
        String::from_utf8_lossy(&printed).trim(),
        format!("tokengauge {VERSION}")
    );
    assert!(release.bindir().join("tokengauge-tui").is_file());
    assert_eq!(
        std::fs::read_link(release.bindir().join("tokengauge-waybar")).expect("the old name"),
        Path::new("tokengauge")
    );
    assert!(
        release
            .root
            .join("home/.config/tokengauge/config.toml")
            .is_file()
    );
}

#[test]
fn an_archive_that_does_not_match_its_checksum_installs_nothing() {
    let release = Release::new();
    let mut tampered = std::fs::read(release.archive()).expect("the archive");
    tampered.extend_from_slice(b"junk");
    std::fs::write(release.archive(), tampered).expect("tampered");

    let out = release.install(&["--no-daemon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not match its SHA256SUMS entry"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!release.bindir().join("tokengauge").exists());
}

#[test]
fn a_release_without_checksums_installs_nothing() {
    let release = Release::new();
    std::fs::remove_file(release.root.join("dist/SHA256SUMS")).expect("SHA256SUMS");

    let out = release.install(&["--no-daemon"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No SHA256SUMS"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!release.bindir().join("tokengauge").exists());
}
