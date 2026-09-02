//! Everything TokenGauge knows, so that no frontend has to.
//!
//! Six surfaces render this crate's output - the Waybar module and its tooltip,
//! the KDE applet, the GNOME extension, the Omarchy widget, the Windows tray and
//! the TUI - and a feature that lands on one has to land on all of them. What
//! makes that tractable is that a frontend never reads a credential, a cache
//! file or a provider endpoint: it renders [`panel::panel_spec`], which resolves
//! section order, labels, number formatting and colour tiers once, here.
//!
//! The shape of the crate, in the order data moves through it:
//!
//! | Module | What it owns |
//! | ------ | ------------ |
//! | [`providers`] | One table of everything a provider *is*. Adding one is a row. |
//! | [`fetch`] | Asking every enabled provider at once, and serving the last good answer when one fails. |
//! | [`payload`] | What a fetcher produces, and the on-disk shape it is stored in. |
//! | [`rows`] | A payload turned into the row every frontend renders. Past here, nothing knows which provider it is looking at. |
//! | [`cost`] | The transcripts the CLIs already write, rated against a price table. |
//! | [`sync`] | The same figures across every machine you code on, sealed and merged. |
//! | [`panel`] | The panel every frontend draws, resolved once. |
//! | [`snapshot`] | The one record of all of it, and the single fetch-or-serve decision. |
//! | [`config`], [`statefiles`], [`theme`], [`fmt`], [`device`] | The file, the paths, the palette, the strings, the machine. |
//!
//! Two rules the layout depends on. Every state file is derived from the
//! snapshot's **parent**, so pointing `cache_file` elsewhere moves the whole set
//! and a test gets a directory of its own. And a value's semantic tier
//! ([`panel::Tone`]) is decided here while its colour is decided per frontend,
//! which is what keeps five palettes from each growing their own thresholds.

// Path-and-copy only, so every crate gets it without the network stack that
// `self-update` pulls in.
pub mod frontend;

#[cfg(feature = "self-update")]
pub mod update;

mod ccusage;
mod claude;
mod codex;
pub mod config;
pub mod cost;
mod device;
pub mod doctor;
pub mod fetch;
pub mod fmt;
mod glm;
mod grok;
pub mod history;
mod kimi;
pub mod launch;
pub mod pace;
pub mod panel;
pub mod payload;
mod provider;
pub mod providers;
pub mod rows;
pub mod snapshot;
pub mod statefiles;
pub mod sync;
pub mod theme;

// Flat at the root, because that is how every caller already spells them: a
// split that renamed `tokengauge_core::ProviderRow` would be a split nobody
// asked for.
pub use ccusage::*;
pub use config::*;
pub use device::*;
pub use fetch::*;
pub use fmt::{
    format_tokens, format_updated, format_updated_relative, month_start, now_ms, sparkline,
};
pub(crate) use fmt::{pct_u8, slug};
pub use payload::*;
pub use providers::*;
pub use rows::*;
pub use snapshot::*;
pub use statefiles::*;
pub use theme::*;

pub use cost::{CostSource, NativeCostReport};
pub use history::{
    HISTORY_RANGES, HistoryPanel, HistoryPoint, HistoryRange, HistorySeries, history_panel,
    history_panel_now,
};
pub use pace::{PaceStage, UsagePace};
pub use panel::{PanelRow, Section, SectionKind, Tone, panel_spec, refresh_hint};
pub use sync::config::{
    SyncConfig, SyncDirConfig, SyncProvidersConfig, SyncS3Config, SyncTransportKind,
    config_set_sync_dir, config_set_sync_enabled, config_set_sync_label, config_set_sync_provider,
    config_set_sync_s3, config_set_sync_transport,
};
