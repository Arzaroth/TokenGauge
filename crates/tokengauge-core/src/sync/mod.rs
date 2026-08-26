//! Fleet sync: one panel covering every machine the user codes on.
//!
//! Limits are account-scoped and already read the same everywhere, so only cost
//! crosses machines. The unit that crosses is a [`model::Bucket`] - tokens for
//! one provider and model within one UTC hour - and never a dollar figure,
//! because money is tokens times the reader's own price table.
//!
//! See `docs/sync.md` for the design and `docs/adr/0001-fleet-sync-shape.md`
//! for why it is shaped this way.

pub mod config;
pub mod contribution;
pub mod crypto;
pub mod fleet;
pub mod hour;
pub mod report;
pub mod run;
pub mod s3;
pub mod store;
pub mod transport;

pub use contribution::{
    Bucket, BucketKey, Contribution, DayDigest, DeviceRecord, Granularity, SCHEMA_VERSION,
    WIRE_RETENTION_DAYS, bucketize, content_hash, day_digests, syncable,
};
pub use crypto::{FleetKey, OpenError, key_path, load_key, store_key};
pub use fleet::{
    DeviceCost, DeviceSlice, FleetStore, KeyChange, ObjectState, Overlap, STORE_RETENTION_DAYS,
};
pub use hour::Hour;
pub use report::{
    DeviceLine, DeviceStatus, OverlapNote, SkippedObject, SyncReport, SyncStatus, describe, note,
};
pub use run::{SyncOutcome, forget, local_device_id, refresh, test_round_trip};
pub use transport::{PeerEntry, Transport};
