//! How full each window has been since it opened, one trail per provider,
//! credential and window, kept beside the snapshot.
//!
//! The snapshot holds a window's figure as of the last fetch and nothing
//! before it, so a pace drawn from it alone can only assume the window filled
//! evenly since it opened. The trail is what a pace needs to tell a morning's
//! burst from a steady afternoon. It holds the current instance of each window
//! only: a trail ends when its window resets, and a window a day or longer
//! leaves a closing record behind, kept a year, of how full it got, with the
//! trail itself for the first eight weeks.

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
const CLOSED_SAMPLES_KEEP_DAYS: i64 = 56;

pub fn history_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-window-history.json")
}

/// This machine's history with its fleet's folded in, for readers only:
/// nothing in it is published, so a key change forgets the old fleet here as
/// it does in the fleet store.
pub fn fleet_view_path(cache_file: &Path) -> PathBuf {
    let parent = cache_file.parent().unwrap_or_else(|| Path::new("."));
    parent.join("tokengauge-window-fleet.json")
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
    /// The trail it closed with, kept for `CLOSED_SAMPLES_KEEP_DAYS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<(i64, u8)>,
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
#[derive(Clone, Copy)]
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

    /// The index of the trail `reading` belongs to, started afresh when its
    /// window reset.
    pub(crate) fn observe(
        &mut self,
        provider: &str,
        credential: (Option<&str>, Option<&str>),
        window: &str,
        reading: &Reading,
    ) -> usize {
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
        index
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
        let horizon = now - chrono::Duration::days(CLOSED_SAMPLES_KEEP_DAYS);
        for record in self.closed.iter_mut().filter(|c| c.resets_at <= horizon) {
            record.samples.clear();
        }
    }
}

impl Trail {
    fn closed(&self) -> Option<ClosedWindow> {
        self.closed_at(self.opened_at, &self.samples)
    }

    fn closed_at(
        &self,
        opened_at: Option<DateTime<Utc>>,
        samples: &[(i64, u8)],
    ) -> Option<ClosedWindow> {
        let &(ms, used) = samples.last()?;
        (self.window_minutes >= CLOSED_MIN_MINUTES).then(|| ClosedWindow {
            provider: self.provider.clone(),
            credential: self.credential.clone(),
            account_digest: self.account_digest.clone(),
            window: self.window.clone(),
            window_minutes: self.window_minutes,
            opened_at: opened_at.unwrap_or(
                self.resets_at - chrono::Duration::minutes(i64::from(self.window_minutes)),
            ),
            resets_at: self.resets_at,
            used,
            seen_at: DateTime::from_timestamp_millis(ms).unwrap_or(self.resets_at),
            samples: squeeze(samples.iter().copied()),
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
/// window's burn and opening from its trail with the fleet's samples of it
/// folded in. A payload served stale saw nothing new, so it adds nothing. The
/// time of a reading is the payload's own, not the write's: a stored
/// credential's figures are carried between its fetches, and a carried figure
/// is not a new sample. Only what this machine saw is kept in its history;
/// the fleet's lands in the view beside it.
pub fn record(
    cache_file: &Path,
    payloads: &mut [ProviderPayload],
    peers: &[&SharedWindows],
    now: DateTime<Utc>,
) {
    let path = history_path(cache_file);
    let read = WindowHistory::read(&path);
    let readable = read.is_some();
    let mut history = read.unwrap_or_default();
    let before = history.clone();
    let mut seen = Vec::new();
    for_each_reading(payloads, now, |provider, credential, name, _, reading| {
        seen.push(history.observe(provider, credential, name, &reading));
    });
    let view = history.with_fleet(peers, now);
    let mut seen = seen.into_iter();
    for_each_reading(payloads, now, |_, (_, account), _, window, reading| {
        let Some(index) = seen.next() else {
            return;
        };
        let own = &history.trails[index];
        let trail = view
            .trails
            .get(index)
            .filter(|t| near(t.resets_at, own.resets_at, own.window_minutes))
            .unwrap_or(own);
        let ours = own.same_account(account);
        window.burn = ours.then(|| burn(trail, &reading)).flatten();
        let opened = trail.opened_at.filter(|at| ours && *at <= reading.at);
        window.opened_at = opened.map(|at| at.to_rfc3339()).or(window.opened_at.take());
    });
    history.prune(now);
    if !readable {
        drop_view(cache_file);
        return;
    }
    if history != before
        && let Ok(text) = serde_json::to_string(&history)
    {
        let _ = write_atomic(&path, text.as_bytes());
    }
    write_view(cache_file, &history, peers, now);
}

fn for_each_reading(
    payloads: &mut [ProviderPayload],
    now: DateTime<Utc>,
    mut each: impl FnMut(&str, (Option<&str>, Option<&str>), &str, &mut UsageWindow, Reading),
) {
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
                each(
                    &provider,
                    (credential.as_deref(), account.as_deref()),
                    &name,
                    window,
                    reading,
                );
            }
        }
    }
}

/// What a device shares with its fleet: the trails and closed windows of the
/// logins whose account it knows, filed by account rather than by store name,
/// since each machine names its logins its own way.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SharedWindows {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trails: Vec<Trail>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed: Vec<ClosedWindow>,
}

impl SharedWindows {
    pub fn is_empty(&self) -> bool {
        self.trails.is_empty() && self.closed.is_empty()
    }

    /// The trails, and the windows closed since `closed_since`, of the
    /// providers in `providers`.
    pub fn of(
        history: &WindowHistory,
        providers: &[String],
        closed_since: DateTime<Utc>,
    ) -> SharedWindows {
        let shared = |provider: &str, account: &Option<String>| {
            account.is_some() && providers.iter().any(|p| p.eq_ignore_ascii_case(provider))
        };
        SharedWindows {
            trails: history
                .trails
                .iter()
                .filter(|t| shared(&t.provider, &t.account_digest))
                .map(|t| Trail {
                    credential: None,
                    ..t.clone()
                })
                .collect(),
            closed: history
                .closed
                .iter()
                .filter(|c| shared(&c.provider, &c.account_digest) && c.resets_at >= closed_since)
                .map(|c| ClosedWindow {
                    credential: None,
                    ..c.clone()
                })
                .collect(),
        }
    }
}

impl Trail {
    fn same_window(&self, other: &Trail) -> bool {
        self.account_digest.is_some()
            && self.account_digest == other.account_digest
            && self.provider == other.provider
            && self.window == other.window
    }

    /// Takes in another machine's samples of the same instance. Whichever
    /// side saw the later refill opened it; a drop between the two sides'
    /// samples is a refill neither saw. What came before the opening ended,
    /// and is handed back as closed.
    fn merge(&mut self, peer: &Trail) -> Vec<ClosedWindow> {
        let mut ended = Vec::new();
        let mut opened = self.opened_at.max(peer.opened_at);
        if let Some(at) = opened.filter(|at| Some(*at) != self.opened_at) {
            let before = self
                .samples
                .partition_point(|(t, _)| *t <= at.timestamp_millis());
            ended.extend(self.closed_at(self.opened_at, &self.samples[..before]));
        }
        let floor = opened.map_or(i64::MIN, |at| at.timestamp_millis());
        let mut points: Vec<(i64, u8)> = self
            .samples
            .iter()
            .chain(&peer.samples)
            .copied()
            .filter(|(t, _)| *t > floor)
            .collect();
        points.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        points.dedup_by_key(|p| p.0);
        let drops: Vec<usize> = (1..points.len())
            .filter(|&i| points[i].1 < points[i - 1].1)
            .collect();
        let mut start = 0;
        for drop in drops {
            ended.extend(self.closed_at(opened, &points[start..drop]));
            opened = DateTime::from_timestamp_millis(points[drop - 1].0);
            start = drop;
        }
        points.drain(..start);
        let mut samples = squeeze(points);
        if samples.len() > MAX_SAMPLES {
            samples.drain(..samples.len() - MAX_SAMPLES);
        }
        self.samples = samples;
        self.opened_at = opened;
        ended
    }
}

fn squeeze(points: impl IntoIterator<Item = (i64, u8)>) -> Vec<(i64, u8)> {
    let mut samples: Vec<(i64, u8)> = Vec::new();
    for point in points {
        match samples.as_mut_slice() {
            [.., (_, before), last] if *before == point.1 && last.1 == point.1 => {
                last.0 = point.0;
            }
            _ => samples.push(point),
        }
    }
    samples
}

impl WindowHistory {
    /// A copy of this history with the fleet's trails and closed windows
    /// folded in, every trail at the index it has here.
    fn with_fleet(&self, peers: &[&SharedWindows], now: DateTime<Utc>) -> WindowHistory {
        let mut view = self.clone();
        view.fold(peers, now);
        view
    }

    /// Folds the fleet's trails and closed windows into this history.
    /// A trail only ever joins one this machine keeps for the same account:
    /// the samples of the same instance merge, a later instance takes the
    /// trail's place, and an earlier one is history already.
    pub fn fold(&mut self, peers: &[&SharedWindows], now: DateTime<Utc>) {
        let mut ended = Vec::new();
        for peer in peers.iter().flat_map(|p| &p.trails) {
            if peer.account_digest.is_none() || peer.resets_at <= now {
                continue;
            }
            for local in self.trails.iter_mut().filter(|t| t.same_window(peer)) {
                if near(local.resets_at, peer.resets_at, local.window_minutes) {
                    ended.extend(local.merge(peer));
                } else if peer.resets_at > local.resets_at {
                    ended.extend(local.closed());
                    *local = Trail {
                        credential: local.credential.take(),
                        ..peer.clone()
                    };
                }
            }
        }
        let closed = peers.iter().flat_map(|p| &p.closed).cloned();
        for record in ended.into_iter().chain(closed) {
            self.close(record);
        }
    }

    /// Files a closed window once: the same instance seen from another
    /// machine keeps whichever saw it last.
    fn close(&mut self, record: ClosedWindow) {
        if record.account_digest.is_none() {
            return;
        }
        let same = |c: &ClosedWindow| {
            c.account_digest == record.account_digest
                && c.provider == record.provider
                && c.window == record.window
                && near(c.resets_at, record.resets_at, record.window_minutes)
                && near(c.opened_at, record.opened_at, record.window_minutes)
        };
        if let Some(kept) = self.closed.iter_mut().find(|c| same(c)) {
            if record.seen_at > kept.seen_at {
                kept.used = record.used;
                kept.seen_at = record.seen_at;
            }
            let mut points: Vec<(i64, u8)> = kept
                .samples
                .iter()
                .chain(&record.samples)
                .copied()
                .collect();
            points.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
            points.dedup_by_key(|p| p.0);
            kept.samples = squeeze(points);
            return;
        }
        let credential = record.credential.clone().or_else(|| {
            let ours = |provider: &str, account: &Option<String>| {
                provider == record.provider && *account == record.account_digest
            };
            self.trails
                .iter()
                .filter(|t| ours(&t.provider, &t.account_digest))
                .map(|t| t.credential.clone())
                .chain(
                    self.closed
                        .iter()
                        .filter(|c| ours(&c.provider, &c.account_digest))
                        .map(|c| c.credential.clone()),
                )
                .flatten()
                .next()
        });
        self.closed.push(ClosedWindow {
            credential,
            ..record
        });
    }
}

/// Rewrites the fleet view beside `cache_file` from this machine's history
/// and what the fleet store holds now. Run on every sync cycle, so peers the
/// store dropped, a key change's among them, leave the view with it.
pub fn refresh_fleet_view(cache_file: &Path, peers: &[&SharedWindows], now: DateTime<Utc>) {
    match WindowHistory::read(&history_path(cache_file)) {
        Some(history) => write_view(cache_file, &history, peers, now),
        None => drop_view(cache_file),
    }
}

/// A view that cannot be rebuilt is gone rather than left to stand for a
/// fleet the store may no longer hold.
fn drop_view(cache_file: &Path) {
    let _ = fs::remove_file(fleet_view_path(cache_file));
}

/// With no fleet the view is gone, and readers fall back to the history.
fn write_view(
    cache_file: &Path,
    history: &WindowHistory,
    peers: &[&SharedWindows],
    now: DateTime<Utc>,
) {
    if peers.is_empty() {
        drop_view(cache_file);
        return;
    }
    let path = fleet_view_path(cache_file);
    let mut view = history.with_fleet(peers, now);
    view.prune(now);
    if WindowHistory::read(&path).as_ref() != Some(&view)
        && let Ok(text) = serde_json::to_string(&view)
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
            record(&self.cache, &mut payloads, &[], now);
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
                "samples",
                "seenAt",
                "used",
                "window",
                "windowMinutes"
            ]
        );
        assert!(
            record["samples"][0]
                .as_array()
                .is_some_and(|s| s.len() == 2)
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

    #[test]
    fn a_reading_from_before_the_refill_is_not_opened_by_it() {
        let m = Machine::new("before_refill");
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 3 * 86_400, 0).unwrap();
        let start = reset - Duration::days(7);
        let fetch = |hours: i64, used: u8| {
            let at = start + Duration::hours(hours);
            m.fetch(vec![payload(None, at, window(used, reset, 10080))], at)
                .remove(0)
                .usage
                .unwrap()
                .primary
                .unwrap()
        };
        fetch(72, 70);
        fetch(74, 0);
        assert_eq!(fetch(71, 70).opened_at, None);
    }

    #[test]
    fn a_carried_opening_survives_an_unreadable_history() {
        let m = Machine::new("carried_opening");
        fs::create_dir_all(history_path(&m.cache)).unwrap();
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 3 * 86_400, 0).unwrap();
        let at = reset - Duration::days(2);
        let opened = (reset - Duration::days(3)).to_rfc3339();
        let mut carried = window(12, reset, 10080);
        carried.opened_at = Some(opened.clone());
        let window = m
            .fetch(vec![payload(None, at, carried)], at)
            .remove(0)
            .usage
            .unwrap()
            .primary
            .unwrap();
        assert_eq!(window.opened_at, Some(opened));
    }

    fn trail_of(
        name: Option<&str>,
        account: &str,
        reset: DateTime<Utc>,
        samples: &[(i64, u8)],
    ) -> Trail {
        Trail {
            provider: "claude".into(),
            credential: name.map(str::to_owned),
            account_digest: Some(account.to_owned()),
            window: "secondary".into(),
            window_minutes: 10080,
            resets_at: reset,
            samples: samples.to_vec(),
            opened_at: None,
        }
    }

    fn week() -> (DateTime<Utc>, impl Fn(i64) -> i64) {
        let reset = DateTime::from_timestamp(Utc::now().timestamp() + 2 * 86_400, 0).unwrap();
        let start = (reset - Duration::days(7)).timestamp_millis();
        (reset, move |hours: i64| start + hours * 3_600_000)
    }

    fn peer(trails: Vec<Trail>, closed: Vec<ClosedWindow>) -> SharedWindows {
        SharedWindows { trails, closed }
    }

    #[test]
    fn the_fleets_samples_shape_the_burn_and_stay_out_of_this_machines_history() {
        let (reset, h) = week();
        let at = |hours: i64| DateTime::from_timestamp_millis(h(hours)).unwrap();
        let fetch = |m: &Machine, peers: &[&SharedWindows], hours: i64, used: u8| {
            let mut payloads = vec![payload(Some("work"), at(hours), window(used, reset, 10080))];
            payloads[0].credential.account_digest = Some("a".into());
            record(&m.cache, &mut payloads, peers, at(hours));
            payloads[0].usage.as_ref().unwrap().primary.clone().unwrap()
        };
        let theirs = peer(
            vec![Trail {
                window: "primary".into(),
                ..trail_of(None, "a", reset, &[(h(100), 10), (h(110), 40)])
            }],
            Vec::new(),
        );

        let alone = Machine::new("fleet_alone");
        fetch(&alone, &[], 1, 0);
        let alone = fetch(&alone, &[], 119, 50).burn.unwrap().per_hour;

        let m = Machine::new("fleet_view");
        fetch(&m, &[], 1, 0);
        let burn = fetch(&m, &[&theirs], 119, 50).burn.unwrap().per_hour;
        assert!(burn > 1.5 && alone < 0.5, "{burn} against {alone}");
        assert_eq!(samples(&m.history()), [vec![0, 50]]);
        let view = WindowHistory::read(&fleet_view_path(&m.cache)).unwrap();
        assert_eq!(samples(&view), [vec![0, 10, 40, 50]]);

        fetch(&m, &[], 119 + 1, 50);
        assert!(!fleet_view_path(&m.cache).exists(), "no fleet, no view");
    }

    #[test]
    fn a_view_that_cannot_be_rebuilt_is_gone() {
        let m = Machine::new("view_unreadable");
        let view = fleet_view_path(&m.cache);
        fs::write(&view, "{}").unwrap();
        fs::create_dir_all(history_path(&m.cache)).unwrap();
        let theirs = peer(Vec::new(), Vec::new());
        refresh_fleet_view(&m.cache, &[&theirs], Utc::now());
        assert!(!view.exists(), "a sync cycle drops it");

        fs::write(&view, "{}").unwrap();
        m.fetch(Vec::new(), Utc::now());
        assert!(!view.exists(), "a write drops it");
    }

    #[test]
    fn what_is_shared_is_filed_by_account_and_never_by_name() {
        let (reset, h) = week();
        let now = Utc::now();
        let mut unknown = trail_of(Some("old"), "x", reset, &[(h(1), 5)]);
        unknown.account_digest = None;
        let mut codex = trail_of(Some("work"), "c", reset, &[(h(1), 5)]);
        codex.provider = "codex".into();
        let recent = ClosedWindow {
            provider: "claude".into(),
            credential: Some("work".into()),
            account_digest: Some("a".into()),
            window: "secondary".into(),
            window_minutes: 10080,
            opened_at: now - Duration::days(9),
            resets_at: now - Duration::days(2),
            used: 80,
            seen_at: now - Duration::days(2),
            samples: Vec::new(),
        };
        let old = ClosedWindow {
            resets_at: now - Duration::days(60),
            ..recent.clone()
        };
        let history = WindowHistory {
            trails: vec![
                trail_of(Some("work"), "a", reset, &[(h(1), 5)]),
                unknown,
                codex,
            ],
            closed: vec![recent, old],
        };
        let shared = SharedWindows::of(&history, &["Claude".into()], now - Duration::days(35));
        assert_eq!(shared.trails.len(), 1);
        assert_eq!(shared.trails[0].credential, None);
        assert_eq!(shared.trails[0].account_digest.as_deref(), Some("a"));
        assert_eq!(shared.closed.len(), 1);
        assert_eq!(shared.closed[0].credential, None);
        assert_eq!(shared.closed[0].used, 80);
    }

    #[test]
    fn a_peers_samples_fill_the_gap_this_machine_was_off_for() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(1), 0), (h(2), 0), (h(100), 40)],
            )],
            closed: Vec::new(),
        };
        let theirs = peer(
            vec![trail_of(
                None,
                "a",
                reset + Duration::seconds(30),
                &[(h(50), 10), (h(60), 10), (h(70), 10), (h(90), 30)],
            )],
            Vec::new(),
        );
        history.fold(&[&theirs], Utc::now());
        let ours = &history.trails[0];
        assert_eq!(
            ours.samples,
            [
                (h(1), 0),
                (h(2), 0),
                (h(50), 10),
                (h(70), 10),
                (h(90), 30),
                (h(100), 40)
            ]
        );
        assert_eq!(ours.credential.as_deref(), Some("work"));
        assert_eq!(
            ours.resets_at, reset,
            "this machine's own reset instant stands"
        );

        let again = history.clone();
        history.fold(&[&theirs], Utc::now());
        assert_eq!(history, again, "a fold is idempotent");
    }

    #[test]
    fn another_account_or_window_is_left_alone() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(Some("work"), "a", reset, &[(h(1), 5)])],
            closed: Vec::new(),
        };
        let mut session = trail_of(None, "a", reset, &[(h(2), 50)]);
        session.window = "primary".into();
        let theirs = peer(
            vec![trail_of(None, "b", reset, &[(h(2), 50)]), session],
            Vec::new(),
        );
        let before = history.clone();
        history.fold(&[&theirs], Utc::now());
        assert_eq!(history, before);
    }

    #[test]
    fn a_refill_the_peer_saw_opens_this_machines_trail_and_closes_what_came_before() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(10), 20), (h(20), 50)],
            )],
            closed: Vec::new(),
        };
        let mut refilled = trail_of(None, "a", reset, &[(h(80), 2), (h(90), 9)]);
        refilled.opened_at = DateTime::from_timestamp_millis(h(75));
        let start = reset - Duration::days(7);
        let seen = |hours: i64| DateTime::from_timestamp_millis(h(hours)).unwrap();
        let before_refill = ClosedWindow {
            provider: "claude".into(),
            credential: None,
            account_digest: Some("a".into()),
            window: "secondary".into(),
            window_minutes: 10080,
            opened_at: start,
            resets_at: reset,
            used: 70,
            seen_at: seen(75),
            samples: Vec::new(),
        };
        history.fold(&[&peer(vec![refilled], vec![before_refill])], Utc::now());
        let ours = &history.trails[0];
        assert_eq!(ours.samples, [(h(80), 2), (h(90), 9)]);
        assert_eq!(ours.opened_at, Some(seen(75)));
        let records: Vec<_> = history
            .closed
            .iter()
            .map(|c| (c.opened_at, c.used, c.seen_at, c.credential.as_deref()))
            .collect();
        assert_eq!(
            records,
            [(start, 70, seen(75), Some("work"))],
            "the same week seen by both is filed once, as whoever saw it last"
        );
    }

    #[test]
    fn a_drop_between_the_two_sides_is_a_refill_neither_saw() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(10), 40), (h(30), 60)],
            )],
            closed: Vec::new(),
        };
        let theirs = peer(
            vec![trail_of(None, "a", reset, &[(h(90), 5), (h(95), 8)])],
            Vec::new(),
        );
        history.fold(&[&theirs], Utc::now());
        let ours = &history.trails[0];
        assert_eq!(ours.samples, [(h(90), 5), (h(95), 8)]);
        assert_eq!(ours.opened_at, DateTime::from_timestamp_millis(h(30)));
        assert_eq!(history.closed.len(), 1);
        assert_eq!(history.closed[0].used, 60);
        assert_eq!(history.closed[0].opened_at, reset - Duration::days(7));
    }

    #[test]
    fn a_drop_after_a_refill_closes_the_instance_from_that_refill() {
        let (reset, h) = week();
        let refilled = DateTime::from_timestamp_millis(h(75));
        let mut ours = trail_of(Some("work"), "a", reset, &[(h(80), 20), (h(85), 30)]);
        ours.opened_at = refilled;
        let mut history = WindowHistory {
            trails: vec![ours],
            closed: Vec::new(),
        };
        let mut theirs = trail_of(None, "a", reset, &[(h(60), 10), (h(90), 5)]);
        theirs.opened_at = DateTime::from_timestamp_millis(h(50));
        history.fold(&[&peer(vec![theirs], Vec::new())], Utc::now());
        assert_eq!(history.closed.len(), 1);
        assert_eq!(Some(history.closed[0].opened_at), refilled);
        assert_eq!(history.closed[0].used, 30);
    }

    #[test]
    fn a_later_instance_takes_the_trails_place_under_this_machines_name() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(10), 40), (h(30), 60)],
            )],
            closed: Vec::new(),
        };
        let next = reset + Duration::days(7);
        let theirs = peer(vec![trail_of(None, "a", next, &[(h(200), 5)])], Vec::new());
        history.fold(&[&theirs], Utc::now());
        assert_eq!(history.trails[0].resets_at, next);
        assert_eq!(history.trails[0].samples, [(h(200), 5)]);
        assert_eq!(history.trails[0].credential.as_deref(), Some("work"));
        assert_eq!(history.closed[0].used, 60);
        assert_eq!(history.closed[0].resets_at, reset);

        let earlier = peer(vec![trail_of(None, "a", reset, &[(h(40), 70)])], Vec::new());
        let before = history.clone();
        history.fold(&[&earlier], Utc::now());
        assert_eq!(history, before, "an earlier instance is history already");
    }

    #[test]
    fn a_closed_week_this_machine_never_saw_is_filed_under_its_name_for_the_account() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(Some("work"), "a", reset, &[(h(1), 5)])],
            closed: Vec::new(),
        };
        let last_week = ClosedWindow {
            provider: "claude".into(),
            credential: None,
            account_digest: Some("a".into()),
            window: "secondary".into(),
            window_minutes: 10080,
            opened_at: reset - Duration::days(14),
            resets_at: reset - Duration::days(7),
            used: 91,
            seen_at: reset - Duration::days(7) - Duration::hours(1),
            samples: Vec::new(),
        };
        let stranger = ClosedWindow {
            account_digest: Some("z".into()),
            ..last_week.clone()
        };
        history.fold(&[&peer(Vec::new(), vec![last_week, stranger])], Utc::now());
        let names: Vec<_> = history
            .closed
            .iter()
            .map(|c| (c.account_digest.as_deref(), c.credential.as_deref(), c.used))
            .collect();
        assert_eq!(
            names,
            [(Some("a"), Some("work"), 91), (Some("z"), None, 91)]
        );
    }

    #[test]
    fn a_closed_week_keeps_the_trail_it_closed_with() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(1), 5), (h(20), 40), (h(30), 40), (h(40), 40)],
            )],
            closed: Vec::new(),
        };
        history.prune(reset + Duration::minutes(1));
        assert!(history.trails.is_empty());
        assert_eq!(history.closed[0].used, 40);
        assert_eq!(
            history.closed[0].samples,
            [(h(1), 5), (h(20), 40), (h(40), 40)]
        );
    }

    #[test]
    fn the_same_closed_week_from_two_machines_keeps_both_their_samples() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(10), 20), (h(20), 50)],
            )],
            closed: Vec::new(),
        };
        let mut refilled = trail_of(None, "a", reset, &[(h(80), 2)]);
        refilled.opened_at = DateTime::from_timestamp_millis(h(75));
        let theirs = ClosedWindow {
            provider: "claude".into(),
            credential: None,
            account_digest: Some("a".into()),
            window: "secondary".into(),
            window_minutes: 10080,
            opened_at: reset - Duration::days(7),
            resets_at: reset,
            used: 70,
            seen_at: DateTime::from_timestamp_millis(h(60)).unwrap(),
            samples: vec![(h(10), 20), (h(60), 70)],
        };
        history.fold(&[&peer(vec![refilled], vec![theirs])], Utc::now());
        assert_eq!(history.closed.len(), 1);
        assert_eq!(history.closed[0].used, 70);
        assert_eq!(
            history.closed[0].samples,
            [(h(10), 20), (h(20), 50), (h(60), 70)]
        );
    }

    #[test]
    fn a_closed_week_loses_its_samples_after_eight_weeks_and_keeps_its_figure() {
        let now = Utc::now();
        let record = |days: i64| ClosedWindow {
            provider: "claude".into(),
            credential: Some("work".into()),
            account_digest: Some("a".into()),
            window: "secondary".into(),
            window_minutes: 10080,
            opened_at: now - Duration::days(days + 7),
            resets_at: now - Duration::days(days),
            used: 90,
            seen_at: now - Duration::days(days),
            samples: vec![(0, 10), (1, 90)],
        };
        let mut history = WindowHistory {
            trails: Vec::new(),
            closed: vec![record(55), record(57)],
        };
        history.prune(now);
        let kept: Vec<_> = history
            .closed
            .iter()
            .map(|c| (c.used, c.samples.len()))
            .collect();
        assert_eq!(kept, [(90, 2), (90, 0)]);
    }

    #[test]
    fn interleaved_refills_never_put_a_drop_inside_a_closed_week() {
        let (reset, h) = week();
        let mut history = WindowHistory {
            trails: vec![trail_of(
                Some("work"),
                "a",
                reset,
                &[(h(10), 40), (h(30), 60)],
            )],
            closed: Vec::new(),
        };
        let theirs = trail_of(None, "a", reset, &[(h(20), 5), (h(40), 10)]);
        history.fold(&[&peer(vec![theirs], Vec::new())], Utc::now());
        let records: Vec<_> = history
            .closed
            .iter()
            .map(|c| (c.opened_at.timestamp_millis(), c.samples.clone()))
            .collect();
        assert_eq!(
            records,
            [
                (
                    (reset - Duration::days(7)).timestamp_millis(),
                    vec![(h(10), 40)]
                ),
                (h(10), vec![(h(20), 5), (h(30), 60)]),
            ]
        );
        assert_eq!(history.trails[0].samples, [(h(40), 10)]);
    }
}
