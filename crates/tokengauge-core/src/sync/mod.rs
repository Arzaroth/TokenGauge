//! Fleet sync: one panel covering every machine the user codes on.
//!
//! Limits are account-scoped and already read the same everywhere, so only cost
//! crosses machines. The unit that crosses is a [`model::Bucket`] - tokens for
//! one provider and model within one UTC hour - and never a dollar figure,
//! because money is tokens times the reader's own price table.
//!
//! See `docs/sync.md` for the design and `docs/adr/0001-fleet-sync-shape.md`
//! for why it is shaped this way.

pub mod crypto;
pub mod model;
pub mod run;
pub mod s3;
pub mod store;
pub mod transport;

pub use crypto::{FleetKey, OpenError, key_path, load_key, store_key};
pub use model::{
    Bucket, BucketKey, Contribution, DayDigest, DeviceCost, DeviceRecord, DeviceSlice, FleetStore,
    Granularity, Hour, Overlap, SCHEMA_VERSION, STORE_RETENTION_DAYS, WIRE_RETENTION_DAYS,
    content_hash, syncable,
};
pub use run::{
    DeviceStatus, OverlapNote, SkippedObject, SyncOutcome, SyncStatus, forget, local_device_id,
    note, refresh, test_round_trip,
};
pub use transport::{PeerEntry, Transport};
