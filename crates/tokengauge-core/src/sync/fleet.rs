//! The durable record: buckets keyed by device and hour, this machine included.
//!
//! A contribution cannot be rebuilt from transcripts alone, because
//! `cost::window_start` reaches back only to the start of the current month.
//! Regenerated every cycle it would forget its own history twelve times a year,
//! and asymmetrically, since peers keep what the writer lost.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::contribution::{
    Bucket, Contribution, DayDigest, DeviceRecord, SCHEMA_VERSION, bucketize, day_digests,
    intern_provider, sort_key, syncable,
};
use super::hour::Hour;
use crate::cost::UsageEvent;
use crate::cost::pricing::PriceTable;

/// Days of buckets the local store keeps. Once a CLI rotates a transcript away
/// the store holds the only record of that day, and a bucket is small.
pub const STORE_RETENTION_DAYS: i64 = 400;

/// What one device has contributed, as this machine holds it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSlice {
    pub device: DeviceRecord,
    pub updated_at_ms: i64,
    /// The oldest hour this device has ever been observed to cover. Derived
    /// from the read window rather than from the oldest bucket, so a quiet
    /// stretch does not read as missing data.
    pub covers_from: Option<Hour>,
    #[serde(default)]
    pub buckets: Vec<Bucket>,
    #[serde(default)]
    pub days: Vec<DayDigest>,
}

/// Two devices that read the same transcripts for the same day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    pub date: NaiveDate,
    pub devices: Vec<String>,
    /// The one whose buckets are counted. Both sides pick the same id without
    /// having to agree on anything.
    pub kept: String,
}

/// One device's share of a period, for the `tokens_by_device` section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCost {
    pub device_id: String,
    pub label: String,
    pub tokens: u64,
    pub usd: f64,
    pub updated_at_ms: i64,
    /// The device's coverage starts inside the period, so its share understates
    /// what that machine really spent.
    pub partial: bool,
    pub is_local: bool,
}

impl Default for FleetStore {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            devices: BTreeMap::new(),
            objects: BTreeMap::new(),
            last_pull_ms: None,
            last_publish_ms: None,
            published_hash: None,
            key_id: None,
            retired_key_ids: Vec::new(),
            published_name: None,
        }
    }
}

/// Buckets keyed by device and hour, covering every device including this one.
///
/// The durable record. A contribution cannot be rebuilt from transcripts alone,
/// because `cost::window_start` reaches back only to the start of the current
/// month: regenerated every cycle it would forget its own history twelve times
/// a year, and asymmetrically, since peers keep what the writer lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetStore {
    pub schema_version: u32,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceSlice>,
    /// What each peer object looked like when it was last read, so an unchanged
    /// object costs a listing rather than a transfer.
    #[serde(default)]
    pub objects: BTreeMap<String, ObjectState>,
    #[serde(default)]
    pub last_pull_ms: Option<i64>,
    #[serde(default)]
    pub last_publish_ms: Option<i64>,
    /// Content hash of the last contribution actually published, so a device
    /// that has done nothing since does not churn the storage.
    #[serde(default)]
    pub published_hash: Option<u64>,
    /// The fleet key this store was last built under. See [`FleetStore::adopt_key`].
    #[serde(default)]
    pub key_id: Option<String>,
    /// Keys this machine used to hold. An object sealed under one of these is
    /// our own past, so it is passed over in silence; an object under a key we
    /// have never held is a different fleet sharing the storage, which is worth
    /// saying out loud.
    #[serde(default)]
    pub retired_key_ids: Vec<String>,
    /// The object name this device last published under, so a re-key can take
    /// its own unreadable litter out of a shared folder.
    #[serde(default)]
    pub published_name: Option<String>,
}

/// What changing the fleet key cost.
#[derive(Debug, Clone, Default)]
pub struct KeyChange {
    /// Peers that belonged to the old fleet.
    pub dropped: Vec<String>,
    /// What this device published under the old key.
    pub previous_object: Option<String>,
}

/// One peer object as we last saw it. A `reason` means it was rejected, and is
/// kept so the rejection keeps being reported without re-downloading it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectState {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl FleetStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take on a fleet key, returning the peers dropped because the key changed.
    ///
    /// A different key is a different fleet. The old peers' objects are sealed
    /// under a key this machine no longer has, so they would be reported as
    /// foreign on every cycle from here to forever, and their rows would show
    /// machines this one no longer shares anything with. Rotating a key across
    /// the same machines costs only what predates the wire retention, because
    /// each of them republishes on its next cycle.
    ///
    /// This device's own history stays: it is ours, and the store is the only
    /// record of it.
    pub fn adopt_key(&mut self, key_id: &str, local_id: &str) -> Option<KeyChange> {
        if self.key_id.as_deref() == Some(key_id) {
            return None;
        }
        let first_time = self.key_id.is_none();
        let change = KeyChange {
            dropped: self
                .devices
                .iter()
                .filter(|(id, _)| id.as_str() != local_id)
                .map(|(_, slice)| slice.device.display().to_string())
                .collect(),
            previous_object: self.published_name.clone(),
        };

        if let Some(retired) = self.key_id.take() {
            self.retired_key_ids.retain(|id| *id != retired);
            self.retired_key_ids.push(retired);
            // A handful is plenty: this only exists to recognise our own litter.
            let excess = self.retired_key_ids.len().saturating_sub(8);
            self.retired_key_ids.drain(..excess);
        }
        self.devices.retain(|id, _| id == local_id);
        self.objects.clear();
        self.published_hash = None;
        self.published_name = None;
        self.key_id = Some(key_id.to_string());

        if first_time { None } else { Some(change) }
    }

    /// Replace this device's buckets inside `[from, ..]` and leave everything
    /// older untouched: a re-read only ever covers the window the transcripts
    /// were scanned for, and the months before it survive only here.
    pub fn upsert_local(
        &mut self,
        device: &DeviceRecord,
        from: Hour,
        events: &[UsageEvent],
        now_ms: i64,
    ) {
        let slice = self.devices.entry(device.id.clone()).or_default();
        slice.device = device.clone();
        slice.updated_at_ms = now_ms;
        slice.covers_from = Some(match slice.covers_from {
            Some(existing) => existing.min(from),
            None => from,
        });

        slice.buckets.retain(|bucket| bucket.hour < from);
        slice
            .buckets
            .extend(bucketize(events).into_iter().filter(|b| b.hour >= from));
        slice.buckets.sort_by(sort_key);

        let from_date = from.utc_date();
        slice.days.retain(|day| day.date < from_date);
        slice.days.extend(
            day_digests(events)
                .into_iter()
                .filter(|d| d.date >= from_date),
        );
        slice.days.sort_by_key(|day| day.date);
    }

    /// Take a peer's contribution. Its covered range replaces what we hold;
    /// anything older we already have survives, because the wire is capped
    /// tighter than the store.
    pub fn absorb(&mut self, contribution: &Contribution) {
        let from = contribution.covers_from;
        let slice = self
            .devices
            .entry(contribution.device.id.clone())
            .or_default();
        slice.device = contribution.device.clone();
        slice.updated_at_ms = contribution.written_at_ms;
        slice.covers_from = Some(match slice.covers_from {
            Some(existing) => existing.min(from),
            None => from,
        });

        slice.buckets.retain(|bucket| bucket.hour < from);
        slice.buckets.extend(contribution.buckets.iter().cloned());
        slice.buckets.sort_by(sort_key);

        let from_date = from.utc_date();
        slice.days.retain(|day| day.date < from_date);
        slice.days.extend(contribution.days.iter().cloned());
        slice.days.sort_by_key(|day| day.date);
    }

    pub fn prune(&mut self, now: DateTime<Utc>) {
        let floor = Hour::containing(now).minus_days(STORE_RETENTION_DAYS);
        let floor_date = floor.utc_date();
        for slice in self.devices.values_mut() {
            slice.buckets.retain(|bucket| bucket.hour >= floor);
            slice.days.retain(|day| day.date >= floor_date);
            slice.covers_from = Some(match slice.covers_from {
                Some(existing) => existing.max(floor),
                None => floor,
            });
        }
    }

    /// What this device should publish: its own slice, capped to the wire
    /// retention and to the providers taking part.
    pub fn contribution(
        &self,
        device_id: &str,
        now: DateTime<Utc>,
        providers: &[String],
        retention_days: i64,
    ) -> Option<Contribution> {
        let slice = self.devices.get(device_id)?;
        let floor = Hour::containing(now).minus_days(retention_days.max(1));
        let taking_part =
            |name: &str| syncable(name) && providers.iter().any(|p| p.eq_ignore_ascii_case(name));

        let buckets: Vec<Bucket> = slice
            .buckets
            .iter()
            .filter(|bucket| bucket.hour >= floor && taking_part(&bucket.provider))
            .cloned()
            .collect();
        let covers_from = slice.covers_from.unwrap_or(floor).max(floor);
        let floor_date = covers_from.utc_date();

        Some(Contribution {
            schema_version: SCHEMA_VERSION,
            device: slice.device.clone(),
            written_at_ms: now.timestamp_millis(),
            tz_offset_minutes: chrono::Local::now().offset().local_minus_utc() / 60,
            covers_from,
            providers: providers.iter().filter(|p| syncable(p)).cloned().collect(),
            buckets,
            days: slice
                .days
                .iter()
                .filter(|day| day.date >= floor_date)
                .cloned()
                .collect(),
        })
    }

    /// Two devices that read the same transcripts for the same day, which is
    /// what a synced `~/.claude/projects` looks like from here.
    pub fn overlaps(&self) -> Vec<Overlap> {
        let mut by_fingerprint: BTreeMap<(NaiveDate, u64, &str), Vec<&str>> = BTreeMap::new();
        for (id, slice) in &self.devices {
            for day in &slice.days {
                // An all-zero digest means no record that day carried an
                // identifier, which is not evidence of anything.
                if day.events == 0 || day.digest.chars().all(|c| c == '0') {
                    continue;
                }
                by_fingerprint
                    .entry((day.date, day.events, day.digest.as_str()))
                    .or_default()
                    .push(id);
            }
        }
        by_fingerprint
            .into_iter()
            .filter(|(_, devices)| devices.len() > 1)
            .map(|((date, _, _), devices)| {
                let kept = devices
                    .iter()
                    .min()
                    .copied()
                    .unwrap_or_default()
                    .to_string();
                Overlap {
                    date,
                    devices: devices.into_iter().map(str::to_string).collect(),
                    kept,
                }
            })
            .collect()
    }

    /// Events for `build_report`, minus what the caller already holds.
    ///
    /// The local device's buckets from `local_from` on are skipped: the caller
    /// passes its real events for that window, which carry exact timestamps
    /// rather than the start of an hour, and there is no reason to make the
    /// local session figure coarser than it already is.
    pub fn synthetic_events(&self, local_id: &str, local_from: Hour) -> Vec<UsageEvent> {
        let overlaps = self.overlaps();
        let dropped: Vec<(NaiveDate, &str)> = overlaps
            .iter()
            .flat_map(|overlap| {
                overlap
                    .devices
                    .iter()
                    .filter(|id| **id != overlap.kept)
                    .map(move |id| (overlap.date, id.as_str()))
            })
            .collect();

        let offset = *chrono::Local::now().offset();
        let mut events = Vec::new();
        for (id, slice) in &self.devices {
            for bucket in &slice.buckets {
                if id == local_id && bucket.hour >= local_from {
                    continue;
                }
                if dropped
                    .iter()
                    .any(|(date, dropped_id)| *date == bucket.hour.utc_date() && dropped_id == id)
                {
                    continue;
                }
                let Some(provider) = intern_provider(&bucket.provider) else {
                    continue;
                };
                let at = bucket.hour.start();
                events.push(UsageEvent {
                    provider,
                    model: bucket.model.clone(),
                    date: at.with_timezone(&offset).date_naive(),
                    at,
                    tokens: bucket.tokens,
                    key: None,
                });
            }
        }
        events
    }

    /// Providers held in the store that this build cannot rate.
    ///
    /// `synthetic_events` has to skip them: a `UsageEvent` carries a
    /// `&'static str`, so a name this binary does not know cannot become one. A
    /// peer on a newer TokenGauge syncing a provider added since would
    /// otherwise have its tokens vanish from the fleet total with nothing said,
    /// which is the failure this whole design treats as worse than breaking.
    pub fn unreadable_providers(&self, local_id: &str) -> Vec<String> {
        let mut unknown: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (id, slice) in &self.devices {
            if id == local_id {
                continue;
            }
            for bucket in &slice.buckets {
                if intern_provider(&bucket.provider).is_none() {
                    unknown.insert(bucket.provider.clone());
                }
            }
        }
        unknown.into_iter().collect()
    }

    /// Per-device share of one provider's spend over a local-calendar range,
    /// narrowed to a single model when `model` is set.
    ///
    /// The narrowing is what lets a day row and a model row answer "which
    /// machine did this come from" from the same buckets the section total is
    /// built out of, so the split cannot disagree with the row above it.
    pub fn device_totals(
        &self,
        provider: &str,
        range: (NaiveDate, NaiveDate),
        offset: FixedOffset,
        prices: &PriceTable,
        local_id: &str,
        model: Option<&str>,
    ) -> Vec<DeviceCost> {
        let (from, to) = range;
        let mut rows: Vec<DeviceCost> = self
            .devices
            .iter()
            .map(|(id, slice)| {
                let mut tokens = 0u64;
                let mut usd = 0.0;
                for bucket in &slice.buckets {
                    if !bucket.provider.eq_ignore_ascii_case(provider) {
                        continue;
                    }
                    if model.is_some_and(|want| !bucket.model.eq_ignore_ascii_case(want)) {
                        continue;
                    }
                    let date = bucket.hour.date_at(offset);
                    if date < from || date > to {
                        continue;
                    }
                    tokens += bucket.tokens.total();
                    if let Some(price) = prices.get(&bucket.model) {
                        usd += price.cost(&bucket.tokens);
                    }
                }
                let partial = slice
                    .covers_from
                    .is_some_and(|covers| covers.date_at(offset) > from);
                DeviceCost {
                    device_id: id.clone(),
                    label: slice.device.display().to_string(),
                    tokens,
                    usd,
                    updated_at_ms: slice.updated_at_ms,
                    partial,
                    is_local: id == local_id,
                }
            })
            .filter(|row| row.tokens > 0)
            .collect();
        rows.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.label.cmp(&b.label)));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::TokenCounts;
    use crate::cost::build_report;
    use crate::sync::contribution::{WIRE_RETENTION_DAYS, content_hash};
    use chrono::Local;

    fn hour(text: &str) -> Hour {
        Hour::parse(text).expect("hour stamp")
    }

    fn event(
        provider: &'static str,
        model: &str,
        at: Hour,
        out: u64,
        key: Option<u64>,
    ) -> UsageEvent {
        let at = at.start();
        UsageEvent {
            provider,
            model: model.into(),
            date: at.with_timezone(&Local).date_naive(),
            at,
            tokens: TokenCounts {
                output: out,
                ..Default::default()
            },
            key,
        }
    }

    fn device(id: &str) -> DeviceRecord {
        DeviceRecord {
            id: id.into(),
            hostname: format!("{id}-host"),
            label: String::new(),
            os: "linux".into(),
        }
    }

    fn tokens_for(store: &FleetStore, id: &str) -> u64 {
        store.devices[id]
            .buckets
            .iter()
            .map(|b| b.tokens.total())
            .sum()
    }

    #[test]
    fn events_inside_one_hour_fold_into_one_bucket() {
        let at = hour("2026-08-25T14");
        let mut late = event("claude", "opus", at, 5, None);
        late.at = at.start() + chrono::Duration::minutes(59);
        let buckets = bucketize(&[event("claude", "opus", at, 7, None), late]);

        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].tokens.output, 12);
        assert_eq!(buckets[0].hour, at);
    }

    #[test]
    fn one_bucket_set_reads_as_two_calendars() {
        // 02:00 UTC is still the 24th in Montreal and already the 25th in Paris.
        let at = hour("2026-08-25T02");
        let paris = FixedOffset::east_opt(2 * 3600).expect("offset");
        let montreal = FixedOffset::west_opt(4 * 3600).expect("offset");

        assert_eq!(at.date_at(paris).to_string(), "2026-08-25");
        assert_eq!(at.date_at(montreal).to_string(), "2026-08-24");
    }

    #[test]
    fn a_reread_replaces_its_window_and_keeps_the_months_before_it() {
        // The trap this store exists for: on the 1st, `cost::window_start`
        // reaches back seven days, so a contribution rebuilt from transcripts
        // alone drops everything older.
        let mut store = FleetStore::new();
        let july = hour("2026-07-14T09");
        let august = hour("2026-08-20T09");

        store.upsert_local(
            &device("a"),
            hour("2026-07-01T00"),
            &[
                event("claude", "opus", july, 100, Some(1)),
                event("claude", "opus", august, 40, Some(2)),
            ],
            1,
        );
        assert_eq!(tokens_for(&store, "a"), 140);

        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", august, 55, Some(2))],
            2,
        );

        assert_eq!(
            tokens_for(&store, "a"),
            155,
            "July survived, August replaced"
        );
        assert_eq!(store.devices["a"].covers_from, Some(hour("2026-07-01T00")));
    }

    #[test]
    fn a_peer_contribution_never_shortens_what_we_already_hold() {
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("b"),
            hour("2026-06-01T00"),
            &[event("claude", "opus", hour("2026-06-10T09"), 90, Some(3))],
            1,
        );

        store.absorb(&Contribution {
            schema_version: SCHEMA_VERSION,
            device: device("b"),
            written_at_ms: 5,
            tz_offset_minutes: 0,
            covers_from: hour("2026-08-01T00"),
            providers: vec!["claude".into()],
            buckets: bucketize(&[event("claude", "opus", hour("2026-08-09T09"), 10, Some(4))]),
            days: Vec::new(),
        });

        assert_eq!(
            tokens_for(&store, "b"),
            100,
            "June is older than the wire cap"
        );
    }

    #[test]
    fn the_same_tree_read_twice_is_reported_and_counted_once() {
        let at = hour("2026-08-25T14");
        let shared = [event("claude", "opus", at, 60, Some(0xabc))];
        let mut store = FleetStore::new();
        store.upsert_local(&device("a"), hour("2026-08-01T00"), &shared, 1);
        store.upsert_local(&device("z"), hour("2026-08-01T00"), &shared, 1);

        let overlaps = store.overlaps();
        assert_eq!(overlaps.len(), 1);
        assert_eq!(
            overlaps[0].kept, "a",
            "smallest id, agreed without coordinating"
        );

        let events = store.synthetic_events("nobody", hour("2099-01-01T00"));
        assert_eq!(events.iter().map(|e| e.tokens.output).sum::<u64>(), 60);
    }

    #[test]
    fn two_devices_that_merely_spent_the_same_are_not_an_overlap() {
        let at = hour("2026-08-25T14");
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", at, 60, Some(1))],
            1,
        );
        store.upsert_local(
            &device("z"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", at, 60, Some(2))],
            1,
        );

        assert!(store.overlaps().is_empty());
        let events = store.synthetic_events("nobody", hour("2099-01-01T00"));
        assert_eq!(events.iter().map(|e| e.tokens.output).sum::<u64>(), 120);
    }

    #[test]
    fn synthetic_events_leave_the_local_window_to_the_transcripts() {
        let at = hour("2026-08-25T14");
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", at, 60, Some(1))],
            1,
        );

        assert!(store.synthetic_events("a", at).is_empty());
        assert_eq!(store.synthetic_events("a", hour("2026-08-25T15")).len(), 1);
        assert_eq!(store.synthetic_events("other", at).len(), 1);
    }

    #[test]
    fn content_hash_ignores_the_write_time() {
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", hour("2026-08-25T14"), 60, Some(1))],
            1,
        );
        let now = hour("2026-08-25T15").start();
        let providers = vec!["claude".to_string()];

        let first = store
            .contribution("a", now, &providers, WIRE_RETENTION_DAYS)
            .expect("slice");
        let mut later = first.clone();
        later.written_at_ms += 60_000;
        assert_eq!(content_hash(&first), content_hash(&later));

        later.buckets[0].tokens.output += 1;
        assert_ne!(content_hash(&first), content_hash(&later));
    }

    /// The knob was declared, documented, defaulted and never read: a user
    /// setting 90 silently got 35.
    #[test]
    fn the_configured_retention_is_what_reaches_the_wire() {
        let now = hour("2026-08-25T12").start();
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-01-01T00"),
            &[
                event("claude", "opus", hour("2026-08-25T11"), 10, Some(1)),
                event("claude", "opus", hour("2026-08-24T09"), 20, Some(2)),
                event("claude", "opus", hour("2026-07-01T09"), 30, Some(3)),
            ],
            1,
        );
        let providers = vec!["claude".to_string()];
        let published = |days: i64| {
            store
                .contribution("a", now, &providers, days)
                .expect("slice")
                .buckets
                .len()
        };

        assert_eq!(published(1), 1, "an hour ago only");
        assert_eq!(published(2), 2, "yesterday too");
        assert_eq!(published(90), 3, "a long retention reaches July");
        assert_eq!(
            published(0),
            published(1),
            "a nonsense retention clamps to a day rather than publishing nothing"
        );
    }

    #[test]
    fn a_ccusage_only_provider_never_reaches_the_wire() {
        assert!(syncable("claude"));
        assert!(!syncable("kimi"));

        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[
                event("claude", "opus", hour("2026-08-25T14"), 60, Some(1)),
                event("kimi", "k2", hour("2026-08-25T14"), 99, Some(2)),
            ],
            1,
        );

        let published = store
            .contribution(
                "a",
                hour("2026-08-25T15").start(),
                &["claude".to_string(), "kimi".to_string()],
                WIRE_RETENTION_DAYS,
            )
            .expect("slice");

        assert_eq!(published.providers, vec!["claude".to_string()]);
        assert_eq!(published.buckets.len(), 1);
        assert_eq!(published.buckets[0].provider, "claude");
    }

    #[test]
    fn prune_keeps_the_retention_window() {
        let now = hour("2026-08-25T00").start();
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2020-01-01T00"),
            &[
                event(
                    "claude",
                    "opus",
                    Hour::containing(now).minus_days(500),
                    10,
                    Some(1),
                ),
                event(
                    "claude",
                    "opus",
                    Hour::containing(now).minus_days(10),
                    20,
                    Some(2),
                ),
            ],
            1,
        );

        store.prune(now);
        assert_eq!(tokens_for(&store, "a"), 20);
        assert!(store.devices["a"].covers_from.expect("floor") > hour("2020-01-01T00"));
    }

    #[test]
    fn device_totals_rank_and_flag_partial_coverage() {
        let utc = FixedOffset::east_opt(0).expect("offset");
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[event("claude", "opus", hour("2026-08-04T09"), 10, Some(1))],
            1,
        );
        store.upsert_local(
            &device("z"),
            hour("2026-08-20T00"),
            &[event("claude", "opus", hour("2026-08-21T09"), 50, Some(2))],
            2,
        );

        let rows = store.device_totals(
            "claude",
            (
                hour("2026-08-01T00").utc_date(),
                hour("2026-08-25T00").utc_date(),
            ),
            utc,
            &PriceTable::default(),
            "a",
            None,
        );

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].device_id, "z", "ranked by tokens");
        assert!(rows[0].partial, "joined mid-month");
        assert!(!rows[1].partial);
        assert!(rows[1].is_local);
    }

    /// The narrowed total is what a model row's tooltip attributes, so it has
    /// to come out of the same buckets the unnarrowed one adds up.
    #[test]
    fn device_totals_narrow_to_one_model() {
        let utc = FixedOffset::east_opt(0).expect("offset");
        let mut store = FleetStore::new();
        store.upsert_local(
            &device("a"),
            hour("2026-08-01T00"),
            &[
                event("claude", "opus", hour("2026-08-04T09"), 10, Some(1)),
                event("claude", "haiku", hour("2026-08-04T10"), 90, Some(2)),
            ],
            1,
        );

        let range = (
            hour("2026-08-01T00").utc_date(),
            hour("2026-08-25T00").utc_date(),
        );
        let prices = PriceTable::default();
        let all = store.device_totals("claude", range, utc, &prices, "a", None);
        let opus = store.device_totals("claude", range, utc, &prices, "a", Some("opus"));
        let haiku = store.device_totals("claude", range, utc, &prices, "a", Some("haiku"));

        assert_eq!(all[0].tokens, 100);
        assert_eq!(opus[0].tokens, 10);
        assert_eq!(haiku[0].tokens, 90);
        assert_eq!(
            opus[0].tokens + haiku[0].tokens,
            all[0].tokens,
            "a split that does not add up to its row is worse than none"
        );
    }

    #[test]
    fn a_fleet_of_one_matches_reading_the_transcripts_directly() {
        let now = Utc::now();
        let base = Hour::containing(now);
        let events: Vec<UsageEvent> = (1..=4)
            .map(|n| {
                event(
                    "claude",
                    "opus",
                    base.minus_hours(n * 5),
                    10 * n as u64,
                    Some(n as u64),
                )
            })
            .collect();
        let today = now.with_timezone(&Local).date_naive();
        let prices = PriceTable::default();

        let mut store = FleetStore::new();
        store.upsert_local(&device("a"), base.minus_days(30), &events, 1);
        let synthetic = store.synthetic_events("nobody", base);

        let direct = build_report(&events, &prices, today);
        let round_tripped = build_report(&synthetic, &prices, today);

        assert_eq!(
            direct.costs["claude"]
                .weekly_history
                .iter()
                .map(|d| d.tokens)
                .sum::<u64>(),
            round_tripped.costs["claude"]
                .weekly_history
                .iter()
                .map(|d| d.tokens)
                .sum::<u64>(),
        );
        assert_eq!(
            direct.costs["claude"].monthly_tokens,
            round_tripped.costs["claude"].monthly_tokens,
        );
    }
}
