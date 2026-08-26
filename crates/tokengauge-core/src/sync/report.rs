//! What a cycle did, and the one place it becomes words.
//!
//! Separated from the cycle itself: three hand-rolled renderings of this had
//! already drifted apart, and the wording is what every surface shows.

use serde::{Deserialize, Serialize};

/// What the fleet looked like on the last cycle. Serialised into the snapshot
/// so `--json`, `--sync-status` and the panel all read one struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_id: String,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub devices: Vec<DeviceStatus>,
    #[serde(default)]
    pub last_pull_ms: Option<i64>,
    #[serde(default)]
    pub last_publish_ms: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Objects in the storage this machine could not use, each with the reason.
    #[serde(default)]
    pub skipped: Vec<SkippedObject>,
    /// Devices that read the same transcripts for the same day, which is what a
    /// synced `~/.claude/projects` looks like from here.
    #[serde(default)]
    pub overlaps: Vec<OverlapNote>,
    /// Peers dropped because the fleet key changed. Non-empty only on the cycle
    /// that noticed, so it reads as an event rather than a state.
    #[serde(default)]
    pub dropped: Vec<String>,
    /// Providers a peer syncs that this build cannot rate, so their tokens are
    /// missing from the totals. Usually a peer on a newer TokenGauge.
    #[serde(default)]
    pub unreadable_providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceStatus {
    pub device_id: String,
    pub label: String,
    pub hostname: String,
    pub updated_at_ms: i64,
    pub is_local: bool,
    /// Silent for longer than `peer_max_age_days`.
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkippedObject {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlapNote {
    pub date: String,
    pub devices: Vec<String>,
    pub kept: String,
}

/// The fleet's state as lines, resolved once here.
///
/// Three hand-rolled copies of this had already drifted apart: the TUI's had
/// lost the quiet marker, the overlap's kept device, the re-key notice and the
/// `[sync.providers]` advice the other two carried. The TUI's exemption is from
/// *layout* parity; everything a user reads still comes from the core.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub enabled: bool,
    pub transport: String,
    pub key_id: String,
    /// Relative, already formatted. `None` when no cycle has finished.
    pub last_pull: Option<String>,
    pub devices: Vec<DeviceLine>,
    /// Worst first, in the same order as [`note`].
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLine {
    pub label: String,
    /// "this machine, 3m ago", "quiet, 12d ago".
    pub detail: String,
}

pub fn describe(status: &SyncStatus, now_ms: i64) -> SyncReport {
    let mut problems = Vec::new();
    if let Some(error) = &status.error {
        problems.push(error.clone());
    }
    for overlap in &status.overlaps {
        problems.push(format!(
            "{} read the same transcripts on {}; counted once, from {}. Turn that provider off under [sync.providers] on one of them.",
            overlap.devices.join(" and "),
            overlap.date,
            overlap.kept
        ));
    }
    if !status.dropped.is_empty() {
        problems.push(format!(
            "new fleet key; dropped {} from the old fleet",
            status.dropped.join(", ")
        ));
    }
    if !status.unreadable_providers.is_empty() {
        problems.push(format!(
            "{} is synced by another machine but unknown to this build, so its tokens are missing here; update TokenGauge",
            status.unreadable_providers.join(", ")
        ));
    }
    for skipped in &status.skipped {
        problems.push(format!("{} - {}", skipped.name, skipped.reason));
    }

    SyncReport {
        enabled: status.enabled,
        transport: status.transport.clone(),
        key_id: status.key_id.clone(),
        last_pull: status
            .last_pull_ms
            .map(|last| crate::panel::ago(last, now_ms)),
        devices: status
            .devices
            .iter()
            .map(|device| {
                let mut notes = Vec::new();
                if device.is_local {
                    notes.push("this machine".to_string());
                }
                if device.stale {
                    notes.push("quiet".to_string());
                }
                notes.push(crate::panel::ago(device.updated_at_ms, now_ms));
                DeviceLine {
                    label: device.label.clone(),
                    detail: notes.join(", "),
                }
            })
            .collect(),
        problems,
    }
}

/// What the panel should say about sync, or nothing when it is off.
///
/// Ordered worst-first: a transport that is down, then a transcript tree read
/// twice, then objects that could not be used, then staleness, and only then
/// the healthy states.
pub fn note(status: &SyncStatus, refresh_secs: u64, now_ms: i64) -> Option<crate::panel::SyncNote> {
    use crate::panel::{SyncNote, Tone, ago};

    if !status.enabled {
        return None;
    }
    let devices = status.devices.len();
    let note = |tone, headline: &str, detail: String| {
        Some(SyncNote {
            devices,
            tone,
            headline: headline.to_string(),
            detail,
        })
    };

    if let Some(error) = &status.error {
        return note(Tone::Critical, "error", error.clone());
    }
    if let Some(overlap) = status.overlaps.first() {
        return note(
            Tone::Critical,
            "duplicate",
            format!(
                "{} read the same transcripts on {}; counted once. Turn that provider off in [sync.providers] on one of them.",
                overlap.devices.join(" and "),
                overlap.date
            ),
        );
    }
    if !status.dropped.is_empty() {
        return note(
            Tone::Warn,
            "re-keyed",
            format!(
                "new fleet key; dropped {} from the old fleet",
                status.dropped.join(", ")
            ),
        );
    }
    if !status.unreadable_providers.is_empty() {
        return note(
            Tone::Warn,
            "incomplete",
            format!(
                "{} is missing from these totals; this build cannot rate it",
                status.unreadable_providers.join(", ")
            ),
        );
    }
    if let Some(skipped) = status.skipped.first() {
        return note(
            Tone::Warn,
            "skipped",
            format!("{} unusable: {}", status.skipped.len(), skipped.reason),
        );
    }
    match status.last_pull_ms {
        None => note(Tone::Warn, "never", "no sync has completed yet".to_string()),
        Some(last) => {
            let age = now_ms - last;
            if age > 86_400_000 {
                note(
                    Tone::Critical,
                    "stale",
                    format!("last synced {}; totals may be short", ago(last, now_ms)),
                )
            } else if age > 3 * (refresh_secs as i64) * 1000 {
                note(
                    Tone::Warn,
                    "stale",
                    format!("last synced {}", ago(last, now_ms)),
                )
            } else if devices < 2 {
                note(
                    Tone::Dim,
                    "waiting",
                    "no other device has published yet".to_string(),
                )
            } else {
                note(Tone::Good, "ok", String::new())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with(devices: usize, last_pull_ms: Option<i64>) -> SyncStatus {
        SyncStatus {
            enabled: true,
            devices: (0..devices)
                .map(|n| DeviceStatus {
                    device_id: format!("d{n}"),
                    label: format!("machine {n}"),
                    hostname: "host".into(),
                    updated_at_ms: 0,
                    is_local: n == 0,
                    stale: false,
                })
                .collect(),
            last_pull_ms,
            ..Default::default()
        }
    }

    /// The wording every surface shows. It lived in three hand-rolled copies
    /// that had already drifted: one had lost the quiet marker, the overlap's
    /// kept device and the re-key notice.
    #[test]
    fn one_report_carries_everything_each_surface_used_to_re_derive() {
        let now = 10_000_000_000;
        let mut status = status_with(2, Some(now - 1000));
        status.transport = "dir:/tmp/fleet/v1".into();
        status.key_id = "024d4dba".into();
        status.devices[0].updated_at_ms = now - 1000;
        status.devices[1].stale = true;
        status.devices[1].updated_at_ms = now - 12 * 86_400_000;
        status.dropped = vec!["old-laptop".into()];
        status.overlaps = vec![OverlapNote {
            date: "2026-08-25".into(),
            devices: vec!["desktop".into(), "laptop".into()],
            kept: "desktop".into(),
        }];
        status.skipped = vec![SkippedObject {
            name: "abc.tgsync".into(),
            reason: "sealed for another fleet key (6712469d)".into(),
        }];
        status.error = Some("the folder is gone".into());

        let report = describe(&status, now);

        assert_eq!(report.last_pull.as_deref(), Some("just now"));
        assert_eq!(report.devices[0].detail, "this machine, just now");
        assert!(
            report.devices[1].detail.contains("quiet"),
            "a silent machine has to say so: {:?}",
            report.devices[1].detail
        );

        // Worst first, and nothing dropped on the floor.
        assert_eq!(report.problems.len(), 4);
        assert_eq!(report.problems[0], "the folder is gone");
        assert!(report.problems[1].contains("counted once, from desktop"));
        assert!(
            report.problems[1].contains("[sync.providers]"),
            "the advice that says what to actually do must survive"
        );
        assert!(report.problems[2].contains("old-laptop"));
        assert!(report.problems[3].contains("another fleet key"));

        assert!(describe(&SyncStatus::default(), now).problems.is_empty());
    }

    /// The wording every frontend shows, and its worst-first order.
    #[test]
    fn the_health_note_reports_the_worst_thing_first() {
        let now = 10_000_000_000;
        let fresh = Some(now - 1000);
        let refresh_secs = 600;
        let tone = |status: &SyncStatus| note(status, refresh_secs, now).expect("enabled").tone;

        assert_eq!(note(&SyncStatus::default(), refresh_secs, now), None);

        let healthy = status_with(2, fresh);
        assert_eq!(tone(&healthy), crate::panel::Tone::Good);
        assert_eq!(tone(&status_with(1, fresh)), crate::panel::Tone::Dim);
        assert_eq!(tone(&status_with(2, None)), crate::panel::Tone::Warn);

        // Three pull intervals is a warning; a day is not.
        let mut behind = status_with(2, Some(now - 4 * 600 * 1000));
        assert_eq!(tone(&behind), crate::panel::Tone::Warn);
        behind.last_pull_ms = Some(now - 25 * 3_600 * 1000);
        assert_eq!(tone(&behind), crate::panel::Tone::Critical);

        // Worst-first: each of these outranks the staleness above it.
        let mut skipped = behind.clone();
        skipped.skipped = vec![SkippedObject {
            name: "x".into(),
            reason: "sealed for another fleet key".into(),
        }];
        assert_eq!(
            note(&skipped, refresh_secs, now).unwrap().headline,
            "skipped"
        );

        let mut dropped = skipped.clone();
        dropped.dropped = vec!["laptop".into()];
        assert_eq!(
            note(&dropped, refresh_secs, now).unwrap().headline,
            "re-keyed"
        );

        // Wrong numbers outrank an informational re-key: a transcript tree
        // counted twice is the one thing here that makes the totals lie.
        let mut overlapping = dropped.clone();
        overlapping.overlaps = vec![OverlapNote {
            date: "2026-08-25".into(),
            devices: vec!["a".into(), "b".into()],
            kept: "a".into(),
        }];
        let duplicate = note(&overlapping, refresh_secs, now).unwrap();
        assert_eq!(duplicate.headline, "duplicate");
        assert_eq!(duplicate.tone, crate::panel::Tone::Critical);

        let mut failed = overlapping.clone();
        failed.error = Some("the folder is gone".into());
        let worst = note(&failed, refresh_secs, now).unwrap();
        assert_eq!(worst.headline, "error");
        assert_eq!(worst.tone, crate::panel::Tone::Critical);
    }
}
