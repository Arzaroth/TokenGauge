//! How full each window has been since it opened, one trail per provider,
//! credential and window, kept beside the snapshot.
//!
//! The snapshot holds a window's figure as of the last fetch and nothing
//! before it, so a pace drawn from it alone can only assume the window filled
//! evenly since it opened. The trail is what a pace needs to tell a morning's
//! burst from a steady afternoon. It holds the current instance of each window
//! only: a trail ends when its window resets, and a window a day or longer
//! leaves a closing record behind, kept a year, of how full it got.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::payload::{Burn, ProviderPayload, UsageSnapshot, UsageWindow};
use crate::statefiles::write_atomic;

/// A weekly window fetched every ten minutes changes at most about a thousand
/// times; past this the oldest samples go first.
const MAX_SAMPLES: usize = 2000;

/// A five-hour window closes five times a day: only those of a day or longer
/// are kept once closed.
const CLOSED_MIN_MINUTES: u32 = 1440;
const CLOSED_KEEP_DAYS: i64 = 366;

pub fn history_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-window-history.json")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowHistory {
    #[serde(default)]
    pub trails: Vec<Trail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<ClosedWindow>,
}

/// How full a window instance got before it reset, early or when due.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosedWindow {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_digest: Option<String>,
    pub window: String,
    pub window_minutes: u32,
    pub opened_at: DateTime<Utc>,
    /// When it was due to reset. A banked reset ends it sooner; the next
    /// instance's `openedAt` says when.
    pub resets_at: DateTime<Utc>,
    /// The last figure seen, and when.
    pub used: u8,
    pub seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trail {
    pub provider: String,
    /// The store name, as on the payload; absent for a live login no stored
    /// credential matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The credential's `accountDigest`, so a name re-captured for another
    /// account starts its own trail. TokenGauge's own: no reader needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_digest: Option<String>,
    /// `primary`, `secondary`, `tertiary`, or an extra window's id (its
    /// title when it has none).
    pub window: String,
    pub window_minutes: u32,
    /// The latest reset instant the provider gave for this instance.
    pub resets_at: DateTime<Utc>,
    /// `[ms since the epoch, percent used]`, oldest first: when each figure
    /// was first and last seen. A trail only rises, so the figure held between
    /// two equal samples; between two others it moved at some point nobody saw.
    pub samples: Vec<(i64, u8)>,
    /// When a reset that left `resetsAt` where it was opened this instance:
    /// the last moment the instance before it was seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened_at: Option<DateTime<Utc>>,
}

/// One window's figures as a fetch reported them.
pub(crate) struct Reading {
    pub used: u8,
    pub minutes: u32,
    pub resets_at: DateTime<Utc>,
    pub at: DateTime<Utc>,
}

impl Reading {
    fn start(&self) -> i64 {
        self.resets_at.timestamp_millis() - i64::from(self.minutes) * 60_000
    }

    pub(crate) fn of(window: &UsageWindow, at: DateTime<Utc>) -> Option<Reading> {
        let resets_at = DateTime::parse_from_rfc3339(window.resets_at.as_deref()?)
            .ok()?
            .with_timezone(&Utc);
        let minutes = window.window_minutes.filter(|m| *m > 0)?;
        let reading = Reading {
            used: window.used_percent?.min(100),
            minutes,
            resets_at,
            at,
        };
        (resets_at > at && at.timestamp_millis() >= reading.start()).then_some(reading)
    }
}

/// How far apart two sightings can place one instance's reset or opening:
/// a twentieth of the window, an hour at most.
fn tolerance_ms(minutes: u32) -> i64 {
    (i64::from(minutes) * 3_000).min(3_600_000)
}

fn near(a: DateTime<Utc>, b: DateTime<Utc>, minutes: u32) -> bool {
    (a - b).num_milliseconds().abs() <= tolerance_ms(minutes)
}

impl Trail {
    /// The same instance until its reset has passed, its figure went down
    /// (which only a reset does, a banked one included), or the reading's
    /// window opened after the trail's last sample, or it belongs to another
    /// account. A reading older than that sample came from an overlapping
    /// fetch and ends nothing.
    fn continues(&self, reading: &Reading, account: Option<&str>) -> bool {
        let Some(&(last, used)) = self.samples.last() else {
            return self.same_account(account) && self.resets_at > reading.at;
        };
        reading.at.timestamp_millis() < last
            || (self.same_account(account)
                && self.resets_at > reading.at
                && reading.used >= used
                && last >= reading.start())
    }

    /// When the instance `reading` belongs to opened, if a reset refilled
    /// this one and left its reset instant in place.
    fn refilled(&self, reading: &Reading, account: Option<&str>) -> Option<DateTime<Utc>> {
        let &(last, used) = self.samples.last()?;
        let refilled = self.same_account(account)
            && self.window_minutes == reading.minutes
            && near(self.resets_at, reading.resets_at, reading.minutes)
            && self.resets_at > reading.at
            && reading.used < used
            && last >= reading.start()
            && reading.at.timestamp_millis() > last;
        refilled
            .then(|| DateTime::from_timestamp_millis(last))
            .flatten()
    }

    fn opened_ms(&self, reading: &Reading) -> i64 {
        let start = reading.start();
        self.opened_at
            .map_or(start, |at| at.timestamp_millis().max(start))
    }

    fn same_account(&self, account: Option<&str>) -> bool {
        match (self.account_digest.as_deref(), account) {
            (Some(was), Some(is)) => was == is,
            _ => true,
        }
    }

    fn observe(&mut self, reading: &Reading) {
        let ms = reading.at.timestamp_millis();
        if self.samples.last().is_some_and(|(at, _)| *at >= ms) {
            return;
        }
        self.resets_at = reading.resets_at;
        self.window_minutes = reading.minutes;
        match self.samples.as_mut_slice() {
            [.., (_, before), last] if *before == reading.used && last.1 == reading.used => {
                last.0 = ms;
            }
            _ => self.samples.push((ms, reading.used)),
        }
        if self.samples.len() > MAX_SAMPLES {
            self.samples.drain(..self.samples.len() - MAX_SAMPLES);
        }
    }
}

impl WindowHistory {
    /// `None` when the file is there but could not be read: it may hold
    /// trails, and writing over it would lose them.
    pub fn read(path: &Path) -> Option<WindowHistory> {
        match fs::read_to_string(path) {
            Ok(text) => Some(serde_json::from_str(&text).unwrap_or_default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(WindowHistory::default()),
            Err(_) => None,
        }
    }

    /// The trail `reading` belongs to, started afresh when its window reset.
    pub(crate) fn observe(
        &mut self,
        provider: &str,
        credential: (Option<&str>, Option<&str>),
        window: &str,
        reading: &Reading,
    ) -> &Trail {
        let (name, account) = credential;
        let found = self.trails.iter().position(|t| {
            t.provider == provider && t.credential.as_deref() == name && t.window == window
        });
        let index = match found {
            Some(i) if self.trails[i].continues(reading, account) => i,
            Some(i) => {
                let opened_at = self.trails[i].refilled(reading, account);
                let ended = std::mem::replace(
                    &mut self.trails[i],
                    Trail::new(provider, credential, window, reading),
                );
                self.trails[i].opened_at = opened_at;
                if ended.same_account(account) {
                    self.closed.extend(ended.closed());
                }
                i
            }
            None => {
                self.trails
                    .push(Trail::new(provider, credential, window, reading));
                self.trails.len() - 1
            }
        };
        self.trails[index].observe(reading);
        &self.trails[index]
    }

    /// Drops the trails whose window has reset: nothing reports on them any
    /// more, and a credential removed from the store leaves its trails behind.
    fn prune(&mut self, now: DateTime<Utc>) {
        let (open, ended): (Vec<Trail>, Vec<Trail>) = std::mem::take(&mut self.trails)
            .into_iter()
            .partition(|t| t.resets_at > now);
        self.trails = open;
        self.closed.extend(ended.iter().filter_map(Trail::closed));
        let horizon = now - chrono::Duration::days(CLOSED_KEEP_DAYS);
        self.closed.retain(|c| c.resets_at > horizon);
    }
}

impl Trail {
    fn closed(&self) -> Option<ClosedWindow> {
        let &(ms, used) = self.samples.last()?;
        (self.window_minutes >= CLOSED_MIN_MINUTES).then(|| ClosedWindow {
            provider: self.provider.clone(),
            credential: self.credential.clone(),
            account_digest: self.account_digest.clone(),
            window: self.window.clone(),
            window_minutes: self.window_minutes,
            opened_at: self.opened_at.unwrap_or(
                self.resets_at - chrono::Duration::minutes(i64::from(self.window_minutes)),
            ),
            resets_at: self.resets_at,
            used,
            seen_at: DateTime::from_timestamp_millis(ms).unwrap_or(self.resets_at),
        })
    }

    fn new(
        provider: &str,
        (name, account): (Option<&str>, Option<&str>),
        window: &str,
        reading: &Reading,
    ) -> Trail {
        Trail {
            provider: provider.to_owned(),
            credential: name.map(str::to_owned),
            account_digest: account.map(str::to_owned),
            window: window.to_owned(),
            window_minutes: reading.minutes,
            resets_at: reading.resets_at,
            samples: Vec::new(),
            opened_at: None,
        }
    }
}

const HOUR_MS: f64 = 3_600_000.0;

/// How fast `trail` has been filling over the last seventh of its window (an
/// hour at least): the last day of a week, the last hour of five, or since the
/// window opened when that is sooner. A window is empty when it opens, so its
/// start is a sample every trail has.
fn burn(trail: &Trail, reading: &Reading) -> Option<Burn> {
    let minutes = i64::from(reading.minutes);
    let start = trail.opened_ms(reading);
    let at = reading.at.timestamp_millis();
    let span = (minutes / 7).max(60) * 60_000;
    let from = (at - span).max(start);
    let hours = (at - from) as f64 / HOUR_MS;
    if hours <= 0.0 {
        return None;
    }
    let used = used_at(&trail.samples, (start, 0), from);
    let per_hour = (f64::from(reading.used) - used).max(0.0) / hours;
    Some(Burn {
        per_hour,
        over_minutes: u32::try_from((at - from) / 60_000).unwrap_or(u32::MAX),
    })
}

/// The figure at `at`: held between two equal samples, and on a straight line
/// between two that differ, the move falling somewhere nobody saw.
fn used_at(samples: &[(i64, u8)], origin: (i64, u8), at: i64) -> f64 {
    let points = std::iter::once(&origin).chain(samples.iter().filter(|(t, _)| *t > origin.0));
    let mut before = origin;
    for &(t, used) in points {
        if t > at {
            let (t0, u0) = before;
            let share = (at - t0) as f64 / (t - t0) as f64;
            return f64::from(u0) + (f64::from(used) - f64::from(u0)) * share;
        }
        before = (t, used);
    }
    f64::from(before.1)
}

/// Every window a usage snapshot reports, by the name its trail goes by.
pub(crate) fn windows_mut(usage: &mut UsageSnapshot) -> Vec<(String, &mut UsageWindow)> {
    let mut windows = Vec::new();
    for (name, window) in [
        ("primary", usage.primary.as_mut()),
        ("secondary", usage.secondary.as_mut()),
        ("tertiary", usage.tertiary.as_mut()),
    ] {
        if let Some(window) = window {
            windows.push((name.to_owned(), window));
        }
    }
    for extra in usage
        .extra_rate_windows
        .iter_mut()
        .filter(|w| !w.placeholder)
    {
        let Some(name) = extra.id.clone().or_else(|| extra.title.clone()) else {
            continue;
        };
        if let Some(window) = extra.window.as_mut() {
            windows.push((name, window));
        }
    }
    windows
}

/// Adds what this fetch saw to the history beside `cache_file`, and sets each
/// window's burn from its trail. A payload
/// served stale saw nothing new, so it adds nothing. The time of a reading is
/// the payload's own, not the write's: a stored credential's figures are
/// carried between its fetches, and a carried figure is not a new sample.
pub fn record(cache_file: &Path, payloads: &mut [ProviderPayload], now: DateTime<Utc>) {
    let path = history_path(cache_file);
    let read = WindowHistory::read(&path);
    let readable = read.is_some();
    let mut history = read.unwrap_or_default();
    let before = history.clone();
    for payload in payloads.iter_mut().filter(|p| !p.stale) {
        let provider = payload.provider.clone();
        let credential = payload.credential.name.clone();
        let account = payload.credential.account_digest.clone();
        let Some(usage) = payload.usage.as_mut() else {
            continue;
        };
        let at = usage
            .updated_at
            .as_deref()
            .and_then(|iso| DateTime::parse_from_rfc3339(iso).ok())
            .map_or(now, |at| at.with_timezone(&Utc).min(now));
        for (name, window) in windows_mut(usage) {
            if let Some(reading) = Reading::of(window, at) {
                let trail = history.observe(
                    &provider,
                    (credential.as_deref(), account.as_deref()),
                    &name,
                    &reading,
                );
                let ours = trail.same_account(account.as_deref());
                window.burn = ours.then(|| burn(trail, &reading)).flatten();
                window.opened_at = trail.opened_at.filter(|_| ours).map(|at| at.to_rfc3339());
            }
        }
    }
    history.prune(now);
    if readable
        && history != before
        && let Ok(text) = serde_json::to_string(&history)
    {
        let _ = write_atomic(&path, text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{CredentialInfo, ExtraRateWindow};
    use chrono::Duration;

    fn window(used: u8, resets_at: DateTime<Utc>, minutes: u32) -> UsageWindow {
        UsageWindow {
            used_percent: Some(used),
            reset_description: None,
            resets_at: Some(resets_at.to_rfc3339()),
            window_minutes: Some(minutes),
            burn: None,
            opened_at: None,
        }
    }

    fn payload(
        credential: Option<&str>,
        at: DateTime<Utc>,
        primary: UsageWindow,
    ) -> ProviderPayload {
        ProviderPayload {
            stale_reason: None,
            reported_cost: None,
            provider: "claude".into(),
            version: None,
            source: None,
            usage: Some(UsageSnapshot {
                primary: Some(primary),
                secondary: None,
                tertiary: None,
                updated_at: Some(at.to_rfc3339()),
                login_method: None,
                extra_rate_windows: Vec::new(),
                banked_resets: None,
            }),
            credits: None,
            error: None,
            stale: false,
            credential: CredentialInfo {
                name: credential.map(str::to_owned),
                ..Default::default()
            },
        }
    }

    struct Machine {
        cache: PathBuf,
    }

    impl Machine {
        fn new(name: &str) -> Machine {
            let dir = crate::statefiles::tests::cache_test_dir(&format!("window-history-{name}"));
            Machine {
                cache: dir.join("tokengauge-usage.json"),
            }
        }

        fn fetch(
            &self,
            mut payloads: Vec<ProviderPayload>,
            now: DateTime<Utc>,
        ) -> Vec<ProviderPayload> {
            record(&self.cache, &mut payloads, now);
            payloads
        }

        fn history(&self) -> WindowHistory {
            WindowHistory::read(&history_path(&self.cache)).unwrap()
        }
    }

    fn samples(history: &WindowHistory) -> Vec<Vec<u8>> {
        history
            .trails
            .iter()
            .map(|t| t.samples.iter().map(|(_, used)| *used).collect())
            .collect()
    }

    #[test]
    fn a_trail_keeps_when_each_figure_was_first_and_last_seen_and_not_a_carried_one() {
        let m = Machine::new("a_trail_takes");
        let start = Utc::now();
        let reset = start + Duration::hours(4);
        for (minutes, used) in [(0, 10), (10, 10), (20, 14), (30, 14), (40, 20)] {
            let at = start + Duration::minutes(minutes);
            m.fetch(
                vec![payload(Some("work"), at, window(used, reset, 300))],
                at,
            );
        }
        // A stored credential's figures carried to a later write, same instant.
        let carried = start + Duration::minutes(40);
        m.fetch(
            vec![payload(Some("work"), carried, window(20, reset, 300))],
            start + Duration::minutes(50),
        );
        let history = m.history();
        assert_eq!(samples(&history), [vec![10, 10, 14, 14, 20]]);
        let trail = &history.trails[0];
        assert_eq!(trail.credential.as_deref(), Some("work"));
        assert_eq!(trail.window, "primary");
        assert_eq!(trail.window_minutes, 300);
    }

    #[test]
    fn each_credential_and_window_has_its_own_trail() {
        let m = Machine::new("each_credential");
        let now = Utc::now();
        let reset = now + Duration::hours(4);
        let mut both = payload(None, now, window(5, reset, 300));
        if let Some(usage) = both.usage.as_mut() {
            usage.secondary = Some(window(30, now + Duration::days(3), 10080));
            usage.extra_rate_windows = vec![
                ExtraRateWindow {
                    id: None,
                    title: Some("Fable only".into()),
                    window: Some(window(2, now + Duration::days(3), 10080)),
                    placeholder: false,
                },
                ExtraRateWindow {
                    id: Some("unused".into()),
                    title: None,
                    window: Some(window(0, now + Duration::days(3), 10080)),
                    placeholder: true,
                },
            ];
        }
        m.fetch(
            vec![both, payload(Some("perso"), now, window(50, reset, 300))],
            now,
        );
        let names: Vec<(Option<String>, String)> = m
            .history()
            .trails
            .into_iter()
            .map(|t| (t.credential, t.window))
            .collect();
        assert_eq!(
            names,
            [
                (None, "primary".to_owned()),
                (None, "secondary".to_owned()),
                (None, "Fable only".to_owned()),
                (Some("perso".to_owned()), "primary".to_owned()),
            ]
        );
    }

    #[test]
    fn a_reset_starts_a_new_trail_whether_it_came_due_or_was_banked() {
        let m = Machine::new("a_reset_starts");
        let start = Utc::now();
        let first = start + Duration::hours(1);
        m.fetch(vec![payload(None, start, window(80, first, 300))], start);

        let later = first + Duration::minutes(10);
        let second = later + Duration::hours(4);
        m.fetch(vec![payload(None, later, window(3, second, 300))], later);
        assert_eq!(samples(&m.history()), [vec![3]]);

        let banked = later + Duration::minutes(20);
        m.fetch(vec![payload(None, banked, window(0, second, 300))], banked);
        assert_eq!(samples(&m.history()), [vec![0]]);
    }

    #[test]
    fn a_window_that_reset_early_starts_a_new_trail_even_when_it_reads_higher() {
        let m = Machine::new("early_reset");
        let start = Utc::now();
        m.fetch(
            vec![payload(
                None,
                start,
                window(20, start + Duration::hours(4), 300),
            )],
            start,
        );
        let later = start + Duration::minutes(30);
        let reopened = later - Duration::minutes(10) + Duration::hours(5);
        m.fetch(vec![payload(None, later, window(25, reopened, 300))], later);
        assert_eq!(samples(&m.history()), [vec![25]]);
    }

    #[test]
    fn a_name_captured_for_another_account_starts_a_new_trail() {
        let m = Machine::new("another_account");
        let start = Utc::now();
        let reset = start + Duration::hours(4);
        let on = |account: &str, minutes: i64, used: u8| {
            let at = start + Duration::minutes(minutes);
            let mut p = payload(Some("work"), at, window(used, reset, 300));
            p.credential.account_digest = Some(account.to_owned());
            m.fetch(vec![p], at);
        };
        on("a", 0, 10);
        on("a", 10, 20);
        on("b", 20, 30);
        assert_eq!(samples(&m.history()), [vec![30]]);
    }

    #[test]
    fn a_late_reading_from_the_previous_account_leaves_the_new_trail_alone() {
        let m = Machine::new("late_account");
        let start = Utc::now();
        let reset = start + Duration::hours(4);
        let on = |account: &str, minutes: i64, used: u8| {
            let at = start + Duration::minutes(minutes);
            let mut p = payload(Some("work"), at, window(used, reset, 300));
            p.credential.account_digest = Some(account.to_owned());
            m.fetch(vec![p], start + Duration::minutes(30))
        };
        on("b", 10, 5);
        on("b", 20, 8);
        let late = on("a", 15, 40);
        assert_eq!(samples(&m.history()), [vec![5, 8]]);
        let window = late[0].usage.as_ref().unwrap().primary.as_ref().unwrap();
        assert!(window.burn.is_none(), "b's trail measured a's reading");
    }

    #[test]
    fn a_reading_older_than_the_trail_leaves_it_alone() {
        let m = Machine::new("older_reading");
        let start = Utc::now();
        let reset = start + Duration::hours(4);
        for (minutes, used) in [(0, 10), (20, 30)] {
            let at = start + Duration::minutes(minutes);
            m.fetch(vec![payload(None, at, window(used, reset, 300))], at);
        }
        let older = start + Duration::minutes(10);
        m.fetch(
            vec![payload(None, older, window(20, reset, 300))],
            start + Duration::minutes(25),
        );
        assert_eq!(samples(&m.history()), [vec![10, 30]]);
    }

    #[test]
    fn a_stale_payload_adds_nothing_and_a_reset_window_is_dropped() {
        let m = Machine::new("a_stale_payload");
        let now = Utc::now();
        let mut stale = payload(None, now, window(40, now + Duration::hours(2), 300));
        stale.stale = true;
        m.fetch(vec![stale], now);
        assert!(!history_path(&m.cache).exists());

        m.fetch(
            vec![payload(
                Some("work"),
                now,
                window(40, now + Duration::hours(2), 300),
            )],
            now,
        );
        assert_eq!(m.history().trails.len(), 1);
        m.fetch(Vec::new(), now + Duration::hours(3));
        assert!(m.history().trails.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_history_that_could_not_be_read_is_not_written_over() {
        use std::os::unix::fs::PermissionsExt;
        let m = Machine::new("unreadable");
        let now = Utc::now();
        let reset = now + Duration::hours(4);
        m.fetch(vec![payload(None, now, window(10, reset, 300))], now);
        let path = history_path(&m.cache);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let later = now + Duration::minutes(10);
        let fetched = m.fetch(vec![payload(None, later, window(20, reset, 300))], later);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(samples(&m.history())[0][0], 10);
        assert!(
            fetched[0]
                .usage
                .as_ref()
                .unwrap()
                .primary
                .as_ref()
                .unwrap()
                .burn
                .is_some()
        );
    }

    #[test]
    fn a_reading_before_its_window_opened_or_after_the_write_adds_nothing_odd() {
        let m = Machine::new("outside_window");
        let now = Utc::now();
        let reset = now + Duration::hours(6);
        m.fetch(vec![payload(None, now, window(10, reset, 300))], now);
        assert!(!history_path(&m.cache).exists());

        let reset = now + Duration::hours(4);
        let ahead = now + Duration::minutes(30);
        m.fetch(vec![payload(None, ahead, window(10, reset, 300))], now);
        let trail = &m.history().trails[0];
        assert_eq!(trail.samples, [(now.timestamp_millis(), 10)]);
    }

    #[test]
    fn a_window_with_no_reset_or_duration_has_no_trail() {
        let m = Machine::new("a_window_with_no");
        let now = Utc::now();
        let mut open = window(40, now, 300);
        open.resets_at = None;
        let mut endless = window(40, now + Duration::hours(2), 300);
        endless.window_minutes = None;
        m.fetch(
            vec![payload(None, now, open), payload(Some("x"), now, endless)],
            now,
        );
        assert!(!history_path(&m.cache).exists());
    }

    fn burn_after(m: &Machine, fetches: &[(i64, u8)], reset: DateTime<Utc>, minutes: u32) -> Burn {
        let mut last = None;
        for (at_minutes, used) in fetches {
            let at = reset - Duration::minutes(i64::from(minutes)) + Duration::minutes(*at_minutes);
            last = Some(m.fetch(vec![payload(None, at, window(*used, reset, minutes))], at));
        }
        let usage = last.unwrap().remove(0).usage.unwrap();
        usage.primary.unwrap().burn.unwrap()
    }

    #[test]
    fn with_no_history_the_burn_is_the_average_since_the_window_opened() {
        let m = Machine::new("burn-average");
        let reset = Utc::now() + Duration::hours(3);
        let burn = burn_after(&m, &[(120, 30)], reset, 300);
        assert_eq!(burn.per_hour, 15.0);
        assert_eq!(burn.over_minutes, 60);
    }

    #[test]
    fn a_burst_then_a_quiet_spell_reads_as_quiet() {
        let m = Machine::new("burn-quiet");
        let reset = Utc::now() + Duration::hours(2);
        let burn = burn_after(&m, &[(10, 10), (50, 40), (110, 40), (180, 40)], reset, 300);
        assert_eq!(burn.per_hour, 0.0);
        assert_eq!(
            burn.over_minutes, 60,
            "the figure held at 40 through the last hour"
        );

        let m = Machine::new("burn-steady");
        let burn = burn_after(&m, &[(10, 2), (120, 20), (150, 25), (180, 30)], reset, 300);
        assert_eq!(burn.per_hour, 10.0, "{burn:?}");
        assert_eq!(burn.over_minutes, 60);
    }

    #[test]
    fn a_slow_burn_is_kept_whole() {
        let m = Machine::new("burn-slow");
        let reset = Utc::now() + Duration::days(6);
        let burn = burn_after(&m, &[(10 * 60, 1)], reset, 10080);
        assert_eq!(burn.per_hour, 0.1);

        let m = Machine::new("burn-slower");
        let reset = Utc::now() + Duration::hours(1);
        let burn = burn_after(&m, &[(167 * 60, 1)], reset, 10080);
        assert!((burn.per_hour - 1.0 / 167.0).abs() < 1e-12, "{burn:?}");
    }

    #[test]
    fn a_week_is_measured_over_its_last_day() {
        let m = Machine::new("burn-week");
        let reset = Utc::now() + Duration::days(2);
        let day = 24 * 60;
        let burn = burn_after(&m, &[(day, 10), (3 * day, 20), (5 * day, 44)], reset, 10080);
        assert_eq!(burn.over_minutes, u32::try_from(day).unwrap());
        assert_eq!(burn.per_hour, 0.5);
    }

    fn closed(history: &WindowHistory) -> Vec<(String, u8)> {
        history
            .closed
            .iter()
            .map(|c| (c.window.clone(), c.used))
            .collect()
    }

    #[test]
    fn a_week_that_resets_is_recorded_and_a_five_hour_window_is_not() {
        let m = Machine::new("closed_week");
        let start = Utc::now();
        let week = start + Duration::days(2);
        let session = start + Duration::hours(2);
        let mut both = payload(Some("work"), start, window(10, session, 300));
        both.usage.as_mut().unwrap().secondary = Some(window(40, week, 10080));
        m.fetch(vec![both], start);
        let later = start + Duration::hours(1);
        let mut both = payload(Some("work"), later, window(30, session, 300));
        both.usage.as_mut().unwrap().secondary = Some(window(55, week, 10080));
        m.fetch(vec![both], later);
        assert!(m.history().closed.is_empty());

        m.fetch(Vec::new(), week + Duration::minutes(1));
        let history = m.history();
        assert!(history.trails.is_empty());
        assert_eq!(closed(&history), [("secondary".to_owned(), 55)]);
        let record = &history.closed[0];
        assert_eq!(record.credential.as_deref(), Some("work"));
        assert_eq!(record.window_minutes, 10080);
        assert_eq!(record.resets_at, week);
        assert_eq!(record.opened_at, week - Duration::days(7));
        assert_eq!(record.seen_at.timestamp_millis(), later.timestamp_millis());
    }

    #[test]
    fn a_week_reset_early_is_recorded_when_the_next_one_opens() {
        let m = Machine::new("closed_banked");
        let start = Utc::now();
        let week = start + Duration::days(3);
        m.fetch(vec![payload(None, start, window(70, week, 10080))], start);
        let after = start + Duration::hours(1);
        m.fetch(
            vec![payload(
                None,
                after,
                window(0, after + Duration::days(7), 10080),
            )],
            after,
        );
        let history = m.history();
        assert_eq!(closed(&history), [("primary".to_owned(), 70)]);
        assert_eq!(history.closed[0].resets_at, week);
        assert_eq!(samples(&history), [vec![0]]);
        assert!(history.trails[0].opened_at.is_none(), "its reset moved");
    }

    #[test]
    fn a_week_whose_name_now_holds_another_account_is_not_recorded() {
        let m = Machine::new("closed_other_account");
        let start = Utc::now();
        let week = start + Duration::days(3);
        let on = |account: &str, hours: i64, used: u8| {
            let at = start + Duration::hours(hours);
            let mut p = payload(Some("work"), at, window(used, week, 10080));
            p.credential.account_digest = Some(account.to_owned());
            m.fetch(vec![p], at);
        };
        on("a", 0, 40);
        on("b", 1, 5);
        assert!(m.history().closed.is_empty());
        assert_eq!(samples(&m.history()), [vec![5]]);
    }

    #[test]
    fn a_record_is_kept_a_year() {
        let m = Machine::new("closed_year");
        let start = Utc::now();
        let week = start + Duration::days(1);
        m.fetch(vec![payload(None, start, window(20, week, 10080))], start);
        m.fetch(Vec::new(), week + Duration::days(365));
        assert_eq!(m.history().closed.len(), 1);
        m.fetch(Vec::new(), week + Duration::days(367));
        assert!(m.history().closed.is_empty());
    }

    #[test]
    fn the_keys_remuda_reads_are_spelled_as_decision_0003_says() {
        let m = Machine::new("keys");
        let now = Utc::now();
        let fetched = m.fetch(
            vec![payload(
                Some("work"),
                now,
                window(40, now + Duration::hours(2), 300),
            )],
            now,
        );
        let window = serde_json::to_value(&fetched[0].usage.as_ref().unwrap().primary).unwrap();
        let burn = window["burn"].as_object().unwrap();
        assert!(burn.contains_key("perHour") && burn.contains_key("overMinutes"));

        let text = std::fs::read_to_string(history_path(&m.cache)).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        let trail = file["trails"][0].as_object().unwrap();
        let mut keys: Vec<&str> = trail.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "credential",
                "provider",
                "resetsAt",
                "samples",
                "window",
                "windowMinutes"
            ]
        );
        assert!(trail["samples"][0].as_array().is_some_and(|s| s.len() == 2));

        let week = now + Duration::days(1);
        m.fetch(
            vec![payload(Some("perso"), now, self::window(40, week, 10080))],
            now,
        );
        m.fetch(Vec::new(), week + Duration::minutes(1));
        let text = std::fs::read_to_string(history_path(&m.cache)).unwrap();
        let file: serde_json::Value = serde_json::from_str(&text).unwrap();
        let record = file["closed"][0].as_object().unwrap();
        let mut keys: Vec<&str> = record.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "credential",
                "openedAt",
                "provider",
                "resetsAt",
                "seenAt",
                "used",
                "window",
                "windowMinutes"
            ]
        );
    }

    #[test]
    fn a_week_idle_until_its_last_hours_is_measured_over_its_last_day() {
        let m = Machine::new("burn-idle");
        let reset = Utc::now() + Duration::days(2);
        let day = 24 * 60;
        let burn = burn_after(
            &m,
            &[
                (60, 0),
                (4 * day + 20 * 60, 0),
                (4 * day + 21 * 60, 12),
                (5 * day, 18),
            ],
            reset,
            10080,
        );
        assert_eq!(burn.over_minutes, u32::try_from(day).unwrap());
        assert_eq!(burn.per_hour, 0.75, "the 0% held until the climb began");
    }

    #[test]
    fn a_rise_across_a_gap_is_spread_over_the_gap() {
        let m = Machine::new("burn-gap");
        let reset = Utc::now() + Duration::days(2);
        let day = 24 * 60;
        let burn = burn_after(
            &m,
            &[(60, 0), (3 * day + 60, 0), (4 * day + 60, 20)],
            reset,
            10080,
        );
        assert!((burn.per_hour - 20.0 / 24.0).abs() < 1e-9, "{burn:?}");
    }

    #[test]
    fn a_week_refilled_in_place_opens_where_it_was_refilled() {
        let m = Machine::new("refilled");
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 3 * 86_400, 0).unwrap();
        let start = reset - Duration::days(7);
        let fetch = |hours: i64, used: u8| {
            let at = start + Duration::hours(hours);
            m.fetch(vec![payload(None, at, window(used, reset, 10080))], at)
                .remove(0)
        };
        fetch(1, 10);
        fetch(72, 70);
        fetch(74, 0);
        let window = fetch(77, 12).usage.unwrap().primary.unwrap();
        let refilled = start + Duration::hours(72);
        assert_eq!(window.opened_at, Some(refilled.to_rfc3339()));
        let burn = window.burn.unwrap();
        assert_eq!(burn.over_minutes, 300);
        assert!((burn.per_hour - 2.4).abs() < 1e-9, "{burn:?}");

        m.fetch(Vec::new(), reset + Duration::minutes(1));
        let records: Vec<_> = m
            .history()
            .closed
            .iter()
            .map(|c| (c.opened_at, c.used))
            .collect();
        assert_eq!(records, [(start, 70), (refilled, 12)]);
    }

    #[test]
    fn a_drop_that_moved_the_reset_opens_the_window_where_it_begins() {
        let m = Machine::new("moved_reset");
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 3 * 86_400, 0).unwrap();
        let start = reset - Duration::days(7);
        let at = start + Duration::hours(72);
        m.fetch(vec![payload(None, at, window(70, reset, 10080))], at);
        let at = at + Duration::hours(2);
        let moved = reset + Duration::days(1);
        let window = m
            .fetch(vec![payload(None, at, window(0, moved, 10080))], at)
            .remove(0)
            .usage
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(window.opened_at, None);
    }
}
