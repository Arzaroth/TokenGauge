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

/// One thing worth saying about the fleet. Worst first.
///
/// Four surfaces derived this list themselves and all four had drifted: the
/// TUI's had lost the quiet marker and the overlap's kept device, the doctor's
/// had its own wording for every entry, and the panel's had the
/// `[sync.providers]` advice the others carried inline where the others had it
/// as a separate sentence. There is one list now, and each surface picks the
/// fields it has room for.
#[derive(Debug, Clone)]
pub struct Finding {
    /// One word, for the panel's badge.
    pub headline: &'static str,
    /// A line naming the condition, for the surfaces with room for one.
    pub title: &'static str,
    pub tone: crate::panel::Tone,
    /// What happened, in the fleet's own nouns.
    pub detail: String,
    /// What to do about it, when there is anything to do.
    pub remedy: String,
}

impl Finding {
    /// Detail and remedy as one sentence, for the surfaces that print prose.
    pub fn sentence(&self) -> String {
        match (self.detail.is_empty(), self.remedy.is_empty()) {
            (true, _) => self.remedy.clone(),
            (_, true) => self.detail.clone(),
            _ => format!("{}. {}", self.detail, self.remedy),
        }
    }

    /// Something to act on, as opposed to a state worth naming.
    pub fn is_problem(&self) -> bool {
        matches!(
            self.tone,
            crate::panel::Tone::Critical | crate::panel::Tone::Warn
        )
    }
}

fn finding(
    headline: &'static str,
    title: &'static str,
    tone: crate::panel::Tone,
    detail: String,
    remedy: &str,
) -> Finding {
    Finding {
        headline,
        title,
        tone,
        detail,
        remedy: remedy.to_string(),
    }
}

/// Everything worth saying about the last cycle, worst first: a transport that
/// is down, then a transcript tree counted twice, then a fleet key change,
/// then objects that could not be used, then staleness, and only then the
/// quiet and healthy states. Empty when sync is off.
pub fn findings(status: &SyncStatus, refresh_secs: u64, now_ms: i64) -> Vec<Finding> {
    use crate::panel::{Tone, ago};

    if !status.enabled {
        return Vec::new();
    }
    let mut out = Vec::new();

    if let Some(error) = &status.error {
        out.push(finding(
            "error",
            "the last cycle failed",
            Tone::Critical,
            error.clone(),
            "",
        ));
    }
    for overlap in &status.overlaps {
        out.push(finding(
            "duplicate",
            "the same transcripts were read twice",
            Tone::Critical,
            format!(
                "{} read the same transcripts on {}; counted once, from {}",
                overlap.devices.join(" and "),
                overlap.date,
                overlap.kept
            ),
            "Turn that provider off under [sync.providers] on one of them.",
        ));
    }
    // A device id is derived from the machine, so one id under two hostnames is
    // a cloned image rather than two machines - and the two keep overwriting
    // each other's object, so one of them is always missing from the totals.
    let mut seen: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for device in &status.devices {
        if let Some(previous) = seen.insert(&device.device_id, &device.hostname)
            && previous != device.hostname
        {
            out.push(finding(
                "cloned",
                "two machines share one device id",
                Tone::Critical,
                format!("{previous} and {}", device.hostname),
                "A cloned image or a restored disk; they overwrite each other's object.",
            ));
        }
    }
    if !status.dropped.is_empty() {
        out.push(finding(
            "re-keyed",
            "the fleet key changed",
            Tone::Warn,
            format!(
                "new fleet key; dropped {} from the old fleet",
                status.dropped.join(", ")
            ),
            "",
        ));
    }
    if !status.unreadable_providers.is_empty() {
        out.push(finding(
            "incomplete",
            "a peer syncs a provider this build cannot rate",
            Tone::Warn,
            format!(
                "{} is synced by another machine but unknown to this build, so its tokens are missing here",
                status.unreadable_providers.join(", ")
            ),
            "Update TokenGauge.",
        ));
    }
    for skipped in &status.skipped {
        out.push(finding(
            "skipped",
            "an object could not be used",
            Tone::Warn,
            format!("{} - {}", skipped.name, skipped.reason),
            "",
        ));
    }
    match status.last_pull_ms {
        None => out.push(finding(
            "never",
            "no cycle has completed yet",
            Tone::Warn,
            "no sync has completed yet".to_string(),
            "Run: tokengauge --sync-test",
        )),
        Some(last) => {
            let age = now_ms - last;
            if age > 86_400_000 {
                out.push(finding(
                    "stale",
                    "the fleet has gone stale",
                    Tone::Critical,
                    format!("last synced {}; totals may be short", ago(last, now_ms)),
                    "",
                ));
            } else if age > 3 * (refresh_secs as i64) * 1000 {
                out.push(finding(
                    "stale",
                    "the fleet is behind",
                    Tone::Warn,
                    format!("last synced {}", ago(last, now_ms)),
                    "",
                ));
            }
        }
    }
    for device in status.devices.iter().filter(|d| d.stale) {
        out.push(finding(
            "quiet",
            "a machine has gone quiet",
            Tone::Dim,
            format!("{} has not published recently", device.label),
            "Its past days still count; --sync-forget drops it.",
        ));
    }
    if out.is_empty() {
        out.push(if status.devices.len() < 2 {
            finding(
                "waiting",
                "no other machine has published yet",
                Tone::Dim,
                "no other device has published yet".to_string(),
                "",
            )
        } else {
            finding("ok", "the fleet is healthy", Tone::Good, String::new(), "")
        });
    }
    out
}

/// The fleet's state as lines, resolved once here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub enabled: bool,
    pub transport: String,
    pub key_id: String,
    /// Relative, already formatted. `None` when no cycle has finished.
    pub last_pull: Option<String>,
    pub devices: Vec<DeviceLine>,
    /// Worst first, in the same order as [`findings`].
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceLine {
    pub label: String,
    /// "this machine, 3m ago", "quiet, 12d ago".
    pub detail: String,
}

pub fn describe(status: &SyncStatus, refresh_secs: u64, now_ms: i64) -> SyncReport {
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
        problems: findings(status, refresh_secs, now_ms)
            .iter()
            .filter(|f| f.is_problem())
            .map(Finding::sentence)
            .collect(),
    }
}

/// What the panel should say about sync, or nothing when it is off: the worst
/// finding, in the badge-sized wording.
pub fn note(status: &SyncStatus, refresh_secs: u64, now_ms: i64) -> Option<crate::panel::SyncNote> {
    let worst = findings(status, refresh_secs, now_ms).into_iter().next()?;
    Some(crate::panel::SyncNote {
        devices: status.devices.len(),
        tone: worst.tone,
        headline: worst.headline.to_string(),
        detail: worst.sentence(),
    })
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

        let report = describe(&status, 600, now);

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

        assert!(
            describe(&SyncStatus::default(), 600, now)
                .problems
                .is_empty()
        );
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

    /// The panel badge, the `--sync-status` problem list and the doctor's fleet
    /// section are three views of one list. Each used to build its own, so a
    /// condition could be worded three ways or reported by only two of them.
    #[test]
    fn every_surface_reads_the_same_findings() {
        let now = 10_000_000_000;
        let mut status = status_with(2, Some(now - 1000));
        status.skipped = vec![SkippedObject {
            name: "abc.tgsync".into(),
            reason: "sealed for another fleet key".into(),
        }];

        let all = findings(&status, 600, now);
        let worst = &all[0];
        assert_eq!(worst.headline, "skipped");

        // The panel takes the worst finding's badge and sentence.
        let panel = note(&status, 600, now).expect("enabled");
        assert_eq!(panel.headline, worst.headline);
        assert_eq!(panel.detail, worst.sentence());

        // --sync-status and the TUI take every finding a user must act on.
        let listed = describe(&status, 600, now).problems;
        let acted: Vec<String> = all
            .iter()
            .filter(|f| f.is_problem())
            .map(Finding::sentence)
            .collect();
        assert_eq!(listed, acted);

        // The doctor keys its pass/fail off the same predicate, so a condition
        // the panel paints critical cannot read as a healthy line there.
        assert!(worst.is_problem());
        assert!(
            !finding(
                "ok",
                "the fleet is healthy",
                crate::panel::Tone::Good,
                String::new(),
                ""
            )
            .is_problem()
        );
    }

    /// The doctor found this one and the other three surfaces did not, which is
    /// how it came to have its own wording for everything else too.
    #[test]
    fn a_cloned_disk_is_reported_everywhere_now() {
        let now = 10_000_000_000;
        let mut status = status_with(2, Some(now - 1000));
        status.devices[1].device_id = status.devices[0].device_id.clone();
        status.devices[1].hostname = "other-host".into();

        let note = note(&status, 600, now).expect("enabled");
        assert_eq!(note.headline, "cloned");
        assert_eq!(note.tone, crate::panel::Tone::Critical);
        assert!(note.detail.contains("other-host"), "{}", note.detail);
    }
}
