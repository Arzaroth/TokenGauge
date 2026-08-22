//! GitHub-release auto-updater. Gated behind the `self-update` feature so only
//! the binaries that expose an update command (waybar on Linux, tui on Windows)
//! pull in the network stack. The GUIs read the cached [`UpdateStatus`] and
//! shell out to the update command rather than linking this in.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use self_update::backends::github::ReleaseList;

use crate::frontend::{self, Frontend};
use crate::{UpdateStatus, read_update_status, write_update_status};

/// What an update did to a non-binary frontend, for the caller to report.
#[derive(Debug, Clone)]
pub struct FrontendOutcome {
    pub id: &'static str,
    pub label: &'static str,
    pub version: Option<String>,
    pub restart_hint: &'static str,
    pub needs_session_restart: bool,
    /// Set when the install failed; the binaries are already replaced by then,
    /// so this is reported rather than propagated.
    pub error: Option<String>,
}

fn install_frontends_from(
    source_root: &Path,
    targets: &[&'static Frontend],
) -> Vec<FrontendOutcome> {
    targets
        .iter()
        .map(|f| match f.install_from(source_root) {
            Ok(_) => FrontendOutcome {
                id: f.id,
                label: f.label,
                version: f.installed_version(),
                restart_hint: f.restart.hint(),
                needs_session_restart: f.restart.needs_session_restart(),
                error: None,
            },
            Err(e) => FrontendOutcome {
                id: f.id,
                label: f.label,
                version: None,
                restart_hint: f.restart.hint(),
                needs_session_restart: f.restart.needs_session_restart(),
                error: Some(format!("{e:#}")),
            },
        })
        .collect()
}

/// Binaries shipped in the release archive for this OS, in replace order.
#[cfg(target_os = "windows")]
const BINARIES: &[&str] = &["tokengauge-tui.exe", "tokengauge-tray.exe"];
#[cfg(not(target_os = "windows"))]
const BINARIES: &[&str] = &["tokengauge-waybar", "tokengauge-tui", "tokengauge-popover"];

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Exclusive lock so concurrent `--update` invocations (CLI, tray menu, Plasma
/// button) don't race on the shared staging dir. Atomic create-new; removed on
/// drop (normal return and unwind).
struct UpdateLock(PathBuf);

impl UpdateLock {
    fn acquire(install_dir: &Path) -> Result<Self> {
        let path = install_dir.join(".tg-update.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(UpdateLock(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "update already in progress ({} exists; remove it if stale)",
                path.display()
            ),
            Err(e) => Err(e).context("failed to acquire update lock"),
        }
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `owner/repo` to pull releases from. Mirrors the install scripts'
/// `TOKENGAUGE_REPO` override so a fork can self-update from its own releases.
fn repo() -> (String, String) {
    let slug = std::env::var("TOKENGAUGE_REPO").unwrap_or_else(|_| "Arzaroth/TokenGauge".into());
    match slug.split_once('/') {
        Some((o, r)) => (o.to_string(), r.to_string()),
        None => ("Arzaroth".into(), "TokenGauge".into()),
    }
}

/// Substring the release asset name must contain for the running platform.
/// Matches the release workflow's `tokengauge-<tag>-<target>.<ext>` naming.
fn arch_target() -> Result<&'static str> {
    #[cfg(target_os = "windows")]
    {
        match std::env::consts::ARCH {
            "x86_64" => Ok("windows-x86_64"),
            other => bail!("unsupported arch: {other}"),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        match std::env::consts::ARCH {
            "x86_64" => Ok("linux-x86_64"),
            "aarch64" | "arm64" => Ok("linux-aarch64"),
            other => bail!("unsupported arch: {other}"),
        }
    }
}

fn archive_kind() -> self_update::ArchiveKind {
    #[cfg(target_os = "windows")]
    {
        self_update::ArchiveKind::Zip
    }
    #[cfg(not(target_os = "windows"))]
    {
        self_update::ArchiveKind::Tar(Some(self_update::Compression::Gz))
    }
}

/// True if dotted version `a` (major.minor.patch) is greater than `b`. Leading
/// `v` and any pre-release suffix are ignored.
pub fn version_gt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> [u64; 3] {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let core = v.split(['-', '+']).next().unwrap_or(v);
        let mut out = [0u64; 3];
        for (i, seg) in core.split('.').take(3).enumerate() {
            out[i] = seg.parse().unwrap_or(0);
        }
        out
    }
    parts(a) > parts(b)
}

/// Fetch the newest release carrying an asset for the running platform.
fn latest_release() -> Result<self_update::update::Release> {
    let (owner, name) = repo();
    let target = arch_target()?;
    let releases = ReleaseList::configure()
        .repo_owner(&owner)
        .repo_name(&name)
        .build()?
        .fetch()
        .context("failed to fetch releases from GitHub")?;
    releases
        .into_iter()
        .find(|r| r.asset_for(target, None).is_some())
        .ok_or_else(|| anyhow!("no release with a {target} asset found"))
}

/// Query GitHub, recompute availability, and persist the cached status. The
/// `notified` guard is preserved across calls.
pub fn check(cache_file: &Path) -> Result<UpdateStatus> {
    let current = current_version().to_string();
    let mut status = read_update_status(cache_file).unwrap_or_default();
    status.current = current.clone();
    status.checked_ms = now_ms();

    let release = latest_release()?;
    let latest = release.version.clone();
    status.available = version_gt(&latest, &current);
    status.latest = Some(latest);

    write_update_status(cache_file, &status)?;
    Ok(status)
}

/// Download the platform archive and replace every installed binary next to the
/// running executable. Returns the version installed (unchanged when already
/// current, so it never clobbers on a same-version run).
/// Result of a successful [`apply`]: the version installed, plus what happened
/// to each non-binary frontend that was already present.
pub struct Applied {
    pub version: String,
    pub frontends: Vec<FrontendOutcome>,
}

pub fn apply(cache_file: &Path) -> Result<String> {
    apply_full(cache_file).map(|a| a.version)
}

pub fn apply_full(cache_file: &Path) -> Result<Applied> {
    let target = arch_target()?;
    let release = latest_release()?;
    let current = current_version();
    if !version_gt(&release.version, current) {
        return Ok(Applied {
            version: current.to_string(),
            frontends: Vec::new(),
        });
    }
    let asset = release
        .asset_for(target, None)
        .ok_or_else(|| anyhow!("release {} has no {target} asset", release.version))?;

    let exe = std::env::current_exe().context("cannot resolve current executable")?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot resolve install directory"))?
        .to_path_buf();

    // Held for the whole download/extract/replace so a second invocation fails
    // fast instead of corrupting the shared staging dir.
    let _lock = UpdateLock::acquire(&install_dir)?;

    // Stage inside the install dir so the final move is same-filesystem.
    let tmp = install_dir.join(".tg-update.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .with_context(|| format!("cannot create staging dir {}", tmp.display()))?;

    // Only what is already installed: this refreshes an existing frontend, it
    // does not decide that a machine should grow a GNOME extension.
    let present = frontend::installed();

    let result = (|| -> Result<(Vec<&'static str>, Vec<FrontendOutcome>)> {
        let archive = tmp.join(&asset.name);
        let f = std::fs::File::create(&archive)
            .with_context(|| format!("cannot create {}", archive.display()))?;
        // GitHub's asset `url` is the API endpoint, which streams the binary
        // only when `Accept: application/octet-stream` is set - otherwise it
        // returns the asset's JSON metadata (self_update's own updater sets
        // this, but this hand-rolled download path must do it too).
        self_update::Download::from_url(&asset.download_url)
            .set_header(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/octet-stream"),
            )
            .show_progress(true)
            .download_to(f)
            .context("download failed")?;

        self_update::Extract::from_source(&archive)
            .archive(archive_kind())
            .extract_into(&tmp)
            .context("extract failed")?;

        let mut replaced = Vec::new();
        for bin in BINARIES {
            let src = tmp.join(bin);
            if !src.exists() {
                continue;
            }
            let dest = install_dir.join(bin);
            // Move-with-temp so a running binary is replaced safely on both
            // Linux (old inode stays live) and Windows (the locked exe is
            // renamed aside rather than deleted in place).
            let backup = tmp.join(format!("{bin}.old"));
            self_update::Move::from_source(&src)
                .replace_using_temp(&backup)
                .to_dest(&dest)
                .with_context(|| format!("failed to replace {}", dest.display()))?;
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(&dest) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&dest, perms);
            }
            replaced.push(*bin);
        }

        // An archive predating the frontend payloads carries none, and every
        // install would fail with the same "payload not found". Say nothing
        // rather than reporting a failure per frontend for an old release.
        let frontends = if present.iter().any(|f| f.payload_in(&tmp).is_some()) {
            install_frontends_from(&tmp, &present)
        } else {
            Vec::new()
        };

        Ok((replaced, frontends))
    })();

    let outcome = result;
    let _ = std::fs::remove_dir_all(&tmp);
    let (replaced, frontends) = outcome?;
    if replaced.is_empty() {
        bail!("release archive contained no known binaries");
    }

    // Refresh the cached status so the GUI drops the update prompt.
    let mut status = read_update_status(cache_file).unwrap_or_default();
    status.current = release.version.clone();
    status.latest = Some(release.version.clone());
    status.available = false;
    status.notified = None;
    status.checked_ms = now_ms();
    let _ = write_update_status(cache_file, &status);

    Ok(Applied {
        version: release.version,
        frontends,
    })
}

/// Download the release matching `version` and install one frontend from it,
/// whether or not it is already present. This is the "switched desktops" path:
/// the payload always comes from the release the running binary belongs to, so
/// the frontend cannot land out of step with it.
pub fn install_frontends(
    targets: &[&'static Frontend],
    version: &str,
) -> Result<Vec<FrontendOutcome>> {
    let (owner, name) = repo();
    let releases = ReleaseList::configure()
        .repo_owner(&owner)
        .repo_name(&name)
        .build()?
        .fetch()
        .context("failed to fetch releases from GitHub")?;
    let wanted = version.trim_start_matches('v');
    let release = releases
        .iter()
        .find(|r| r.version.trim_start_matches('v') == wanted)
        .ok_or_else(|| anyhow!("no release v{wanted} to install frontends from"))?;
    let asset = release
        .asset_for(arch_target()?, None)
        .ok_or_else(|| anyhow!("release {} has no asset for this platform", release.version))?;

    let exe = std::env::current_exe().context("cannot resolve current executable")?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot resolve install directory"))?
        .to_path_buf();
    // One lock, one download, one extraction for the whole set: installing
    // three frontends used to fetch the archive three times.
    let _lock = UpdateLock::acquire(&install_dir)?;

    let tmp = install_dir.join(".tg-frontend.tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .with_context(|| format!("cannot create staging dir {}", tmp.display()))?;

    let result = (|| -> Result<Vec<FrontendOutcome>> {
        let archive = tmp.join(&asset.name);
        let f = std::fs::File::create(&archive)
            .with_context(|| format!("cannot create {}", archive.display()))?;
        self_update::Download::from_url(&asset.download_url)
            .set_header(
                http::header::ACCEPT,
                http::HeaderValue::from_static("application/octet-stream"),
            )
            .show_progress(true)
            .download_to(f)
            .context("download failed")?;
        self_update::Extract::from_source(&archive)
            .archive(archive_kind())
            .extract_into(&tmp)
            .context("extract failed")?;

        if !targets.iter().any(|t| t.payload_in(&tmp).is_some()) {
            bail!(
                "release v{wanted} ships no frontend payloads - they were added to the archive after it"
            );
        }
        // Per-frontend failures are collected rather than propagated, so one
        // unwritable destination does not skip the rest of the set.
        Ok(install_frontends_from(&tmp, targets))
    })();

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::version_gt;

    #[test]
    fn version_compare() {
        assert!(version_gt("0.9.0", "0.8.0"));
        assert!(version_gt("v0.8.1", "0.8.0"));
        assert!(version_gt("1.0.0", "0.9.9"));
        assert!(!version_gt("0.8.0", "0.8.0"));
        assert!(!version_gt("0.8.0", "0.9.0"));
        assert!(version_gt("0.9.0-rc1", "0.8.0"));
        assert!(!version_gt("0.8.0-rc1", "0.8.0"));
    }
}
