//! The document one device publishes, and the unit inside it.
//!
//! Token counts and never dollars: money is tokens times the *reader's* price
//! table, so a figure on the wire would let one machine's stale prices skew the
//! fleet total.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use super::hour::Hour;
use crate::DeviceIdentity;
use crate::cost::{TokenCounts, UsageEvent, digest_u64};
use crate::{PROVIDERS, natively_read};

pub const SCHEMA_VERSION: u32 = 1;

/// Default days of buckets a contribution carries, when `[sync] retention_days`
/// says nothing. Tighter than the store's, because a contribution is
/// re-uploaded whenever it changes.
pub const WIRE_RETENTION_DAYS: i64 = 35;

/// Whether a provider can take part in sync at all. A provider read through
/// ccusage has a `CostInfo` and no usage events under it, so it has nothing to
/// bucket.
pub fn syncable(provider: &str) -> bool {
    natively_read()
        .iter()
        .any(|p| p.eq_ignore_ascii_case(provider))
}

pub(super) fn intern_provider(name: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .find(|known| known.eq_ignore_ascii_case(name))
        .copied()
}

/// How wide a bucket's span is. `Day` is reserved for the degraded contribution
/// a ccusage-sourced provider would need, and is never produced yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Granularity {
    #[default]
    Hour,
    Day,
}

impl Granularity {
    fn is_hour(&self) -> bool {
        matches!(self, Granularity::Hour)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BucketKey {
    pub hour: Hour,
    pub provider: String,
    pub model: String,
}

/// Tokens billed for one provider and model within one UTC hour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    #[serde(rename = "h")]
    pub hour: Hour,
    #[serde(rename = "p")]
    pub provider: String,
    #[serde(rename = "m")]
    pub model: String,
    #[serde(rename = "g", default, skip_serializing_if = "Granularity::is_hour")]
    pub granularity: Granularity,
    #[serde(flatten)]
    pub tokens: TokenCounts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub id: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
}

impl DeviceRecord {
    pub fn new(identity: &DeviceIdentity, label: &str) -> Self {
        Self {
            id: identity.machine_id.clone(),
            hostname: identity.hostname.clone(),
            label: label.trim().to_string(),
            os: std::env::consts::OS.to_string(),
        }
    }

    pub fn display(&self) -> &str {
        if self.label.is_empty() {
            &self.hostname
        } else {
            &self.label
        }
    }
}

/// A day's fingerprint, in **UTC** so two devices in different timezones still
/// compare. Its only job is catching a transcript tree that is itself synced,
/// which would otherwise double the fleet total in silence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayDigest {
    #[serde(rename = "d")]
    pub date: NaiveDate,
    #[serde(rename = "n")]
    pub events: u64,
    #[serde(rename = "x")]
    pub digest: String,
}

/// The document one device publishes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contribution {
    pub schema_version: u32,
    pub device: DeviceRecord,
    pub written_at_ms: i64,
    pub tz_offset_minutes: i32,
    pub covers_from: Hour,
    pub providers: Vec<String>,
    pub buckets: Vec<Bucket>,
    #[serde(default)]
    pub days: Vec<DayDigest>,
}

/// Fold events into `(hour, provider, model)` buckets, sorted so the serialised
/// form is stable and [`content_hash`] can tell a real change from a rewrite.
pub fn bucketize(events: &[UsageEvent]) -> Vec<Bucket> {
    let mut folded: BTreeMap<BucketKey, TokenCounts> = BTreeMap::new();
    for event in events {
        let key = BucketKey {
            hour: Hour::containing(event.at),
            provider: event.provider.to_string(),
            model: event.model.clone(),
        };
        folded.entry(key).or_default().add(&event.tokens);
    }
    folded
        .into_iter()
        .map(|(key, tokens)| Bucket {
            hour: key.hour,
            provider: key.provider,
            model: key.model,
            granularity: Granularity::Hour,
            tokens,
        })
        .collect()
}

/// Fingerprint every UTC day a read touched.
pub fn day_digests(events: &[UsageEvent]) -> Vec<DayDigest> {
    let mut folded: BTreeMap<NaiveDate, (u64, u64)> = BTreeMap::new();
    for event in events {
        let entry = folded.entry(event.at.date_naive()).or_default();
        entry.0 += 1;
        if let Some(key) = event.key {
            entry.1 ^= key;
        }
    }
    folded
        .into_iter()
        .map(|(date, (events, digest))| DayDigest {
            date,
            events,
            digest: format!("{digest:016x}"),
        })
        .collect()
}

/// Stable over `written_at_ms`, so a device republishes only when its data
/// actually moved. Persisted and compared on the next cycle, which is why the
/// algorithm has to be a specified one - see [`digest_u64`].
pub fn content_hash(contribution: &Contribution) -> u64 {
    fn feed(buf: &mut Vec<u8>, bytes: &[u8]) {
        buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(bytes);
    }

    let mut buf = Vec::new();
    feed(&mut buf, contribution.device.id.as_bytes());
    feed(&mut buf, contribution.covers_from.to_string().as_bytes());
    for provider in &contribution.providers {
        feed(&mut buf, provider.as_bytes());
    }
    for bucket in &contribution.buckets {
        feed(&mut buf, bucket.hour.to_string().as_bytes());
        feed(&mut buf, bucket.provider.as_bytes());
        feed(&mut buf, bucket.model.as_bytes());
        feed(
            &mut buf,
            &[match bucket.granularity {
                Granularity::Hour => 0u8,
                Granularity::Day => 1u8,
            }],
        );
        let t = &bucket.tokens;
        for field in [
            t.input,
            t.output,
            t.cache_write_5m,
            t.cache_write_1h,
            t.cache_read,
        ] {
            feed(&mut buf, &field.to_le_bytes());
        }
    }
    for day in &contribution.days {
        feed(&mut buf, day.date.to_string().as_bytes());
        feed(&mut buf, &day.events.to_le_bytes());
        feed(&mut buf, day.digest.as_bytes());
    }
    digest_u64(&[&buf])
}

pub(super) fn sort_key(a: &Bucket, b: &Bucket) -> std::cmp::Ordering {
    a.hour
        .cmp(&b.hour)
        .then_with(|| a.provider.cmp(&b.provider))
        .then_with(|| a.model.cmp(&b.model))
}
