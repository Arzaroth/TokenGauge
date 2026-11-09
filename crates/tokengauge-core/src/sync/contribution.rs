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
use crate::cost::{Source, TokenCounts, UsageEvent, digest_u64};
use crate::window_history::SharedWindows;
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
    pub login: Option<String>,
    pub source: Source,
}

impl BucketKey {
    pub fn for_event(event: &UsageEvent) -> Self {
        BucketKey {
            hour: Hour::containing(event.at),
            provider: event.provider.to_string(),
            model: event.model.clone(),
            login: event.login.clone(),
            source: event.source,
        }
    }

    pub fn of(bucket: &Bucket) -> Self {
        BucketKey {
            hour: bucket.hour,
            provider: bucket.provider.clone(),
            model: bucket.model.clone(),
            login: bucket.login.clone(),
            source: bucket.source,
        }
    }
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
    /// The login that spent it, as an account digest; absent when nothing
    /// attributes it, and on everything a peer older than attribution wrote.
    #[serde(rename = "l", default, skip_serializing_if = "Option::is_none")]
    pub login: Option<String>,
    /// Sessions whose first request falls in this bucket.
    #[serde(rename = "s", default, skip_serializing_if = "is_zero")]
    pub sessions: u64,
    /// Spend only remuda's proxy saw; transcripts' when absent.
    #[serde(rename = "src", default = "cli", skip_serializing_if = "is_cli")]
    pub source: Source,
    #[serde(flatten)]
    pub tokens: TokenCounts,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn cli() -> Source {
    Source::Cli
}

fn is_cli(source: &Source) -> bool {
    *source == Source::Cli
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
    /// Devices this one took over with `--sync-adopt`. Carried on every
    /// contribution, so a peer that was offline when the adoption happened
    /// still folds its own copy of them in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adopted: Vec<Adopted>,
    /// How full each limit window has been, so a machine that was off sees
    /// what the others saw meanwhile.
    #[serde(default, skip_serializing_if = "SharedWindows::is_empty")]
    pub windows: SharedWindows,
    /// The calls each session made in each hour, by provider, so a device
    /// that reads a session another one already counted leaves it out.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_prints: Prints,
}

/// Session prints per provider and hour.
pub type Prints = BTreeMap<String, BTreeMap<Hour, PrintSet>>;

/// The calls one session made within one provider-hour: how many, and the XOR
/// of their dedup keys folded to 32 bits. Two devices holding the same print
/// for the same session and hour read the same calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Print {
    pub calls: u32,
    pub digest: u32,
}

impl Print {
    fn add(&mut self, key: u64) {
        self.calls = self.calls.saturating_add(1);
        self.digest ^= fold_key(key);
    }
}

/// A dedup key folded to 32 bits, as the store and the wire keep it.
pub fn fold_key(key: u64) -> u32 {
    (key ^ (key >> 32)) as u32
}

/// A session id folded to 32 bits. A clash needs two sessions in one hour
/// with the same count and digest as well, so the width is not what keeps
/// two sessions apart.
pub fn session_id(session: &str) -> u32 {
    fold_key(digest_u64(&[session.as_bytes()]))
}

/// One provider-hour's prints, by session, written as one base64 blob of
/// 12-byte records like [`super::fleet::KeySet`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrintSet(pub BTreeMap<u32, Print>);

impl PrintSet {
    pub fn get(&self, session: u32) -> Option<Print> {
        self.0.get(&session).copied()
    }

    pub fn insert(&mut self, session: u32, print: Print) {
        self.0.insert(session, print);
    }

    /// One more call of `session`, with `key`.
    pub fn add(&mut self, session: u32, key: u64) {
        self.0.entry(session).or_default().add(key);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for PrintSet {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes: Vec<u8> = self
            .0
            .iter()
            .flat_map(|(session, print)| {
                [session, &print.calls, &print.digest]
                    .into_iter()
                    .flat_map(|field| field.to_le_bytes())
            })
            .collect();
        serializer.serialize_str(&crate::provider::base64_encode(&bytes))
    }
}

impl<'de> Deserialize<'de> for PrintSet {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let bytes = crate::provider::base64_decode(&raw)
            .ok_or_else(|| serde::de::Error::custom("session prints are not base64"))?;
        Ok(Self(
            bytes
                .as_chunks::<12>()
                .0
                .iter()
                .map(|record| {
                    let field = |at: usize| {
                        u32::from_le_bytes([
                            record[at],
                            record[at + 1],
                            record[at + 2],
                            record[at + 3],
                        ])
                    };
                    (
                        field(0),
                        Print {
                            calls: field(4),
                            digest: field(8),
                        },
                    )
                })
                .collect(),
        ))
    }
}

/// Where a call's print is filed: provider, hour, session.
pub type PrintKey = (&'static str, Hour, u32);

/// The print of every session-hour a read holds, over the calls that carry
/// both a dedup key and a session. A call missing either has no print and is
/// never left out.
pub fn session_prints<'a>(
    events: impl IntoIterator<Item = &'a UsageEvent>,
) -> BTreeMap<PrintKey, Print> {
    let mut prints: BTreeMap<PrintKey, Print> = BTreeMap::new();
    for event in events {
        if let Some(group) = print_key(event) {
            prints
                .entry(group)
                .or_default()
                .add(event.key.unwrap_or_default());
        }
    }
    prints
}

pub fn print_key(event: &UsageEvent) -> Option<PrintKey> {
    event.key?;
    let session = event.session.as_deref()?;
    Some((
        event.provider,
        Hour::containing(event.at),
        session_id(session),
    ))
}

/// One device another took over, and when.
///
/// The instant is what keeps a mistake recoverable: a device that publishes
/// again after it was adopted - the wrong id picked, or the old id restored
/// from a backup - is live, and folding it away on every cycle would erase
/// its usage on every peer for good.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adopted {
    pub id: String,
    pub at_ms: i64,
}

/// One entry per id, the latest adoption winning.
pub(super) fn merge_adopted(into: &mut Vec<Adopted>, more: impl IntoIterator<Item = Adopted>) {
    into.extend(more);
    into.sort_by(|a, b| a.id.cmp(&b.id).then(b.at_ms.cmp(&a.at_ms)));
    into.dedup_by(|later, kept| later.id == kept.id);
}

/// Fold events into `(hour, provider, model, login, source)` buckets, sorted so
/// the serialised form is stable and [`content_hash`] can tell a real change
/// from a rewrite.
pub fn bucketize(events: &[UsageEvent]) -> Vec<Bucket> {
    let mut folded: BTreeMap<BucketKey, (TokenCounts, u64)> = BTreeMap::new();
    for event in events {
        let entry = folded.entry(BucketKey::for_event(event)).or_default();
        entry.0.add(&event.tokens);
        entry.1 += u64::from(event.starts_session);
    }
    folded
        .into_iter()
        .map(|(key, (tokens, sessions))| Bucket {
            hour: key.hour,
            provider: key.provider,
            model: key.model,
            granularity: Granularity::Hour,
            login: key.login,
            sessions,
            source: key.source,
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
        // Each only when present, so a bucket without them hashes as before
        // and a store that predates them does not republish on upgrade.
        if let Some(login) = &bucket.login {
            feed(&mut buf, b"l");
            feed(&mut buf, login.as_bytes());
        }
        if bucket.sessions > 0 {
            feed(&mut buf, b"s");
            feed(&mut buf, &bucket.sessions.to_le_bytes());
        }
        if bucket.source == Source::Proxy {
            feed(&mut buf, b"src");
        }
    }
    for day in &contribution.days {
        feed(&mut buf, day.date.to_string().as_bytes());
        feed(&mut buf, &day.events.to_le_bytes());
        feed(&mut buf, day.digest.as_bytes());
    }
    // Only when present, so a device that adopted nothing hashes as before
    // and does not republish on upgrade.
    for adopted in &contribution.adopted {
        feed(&mut buf, adopted.id.as_bytes());
        feed(&mut buf, &adopted.at_ms.to_le_bytes());
    }
    if !contribution.windows.is_empty() {
        feed(&mut buf, b"w");
        feed(
            &mut buf,
            &serde_json::to_vec(&contribution.windows).unwrap_or_default(),
        );
    }
    if !contribution.session_prints.is_empty() {
        feed(&mut buf, b"sp");
        feed(
            &mut buf,
            &serde_json::to_vec(&contribution.session_prints).unwrap_or_default(),
        );
    }
    digest_u64(&[&buf])
}

pub(super) fn sort_key(a: &Bucket, b: &Bucket) -> std::cmp::Ordering {
    a.hour
        .cmp(&b.hour)
        .then_with(|| a.provider.cmp(&b.provider))
        .then_with(|| a.model.cmp(&b.model))
        .then_with(|| a.login.cmp(&b.login))
        .then_with(|| a.source.cmp(&b.source))
}
