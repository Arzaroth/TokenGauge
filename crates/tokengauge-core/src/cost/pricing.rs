//! Model prices, from LiteLLM's table.
//!
//! TokenGauge reads the token counts itself but does not maintain a price map:
//! that is the part which goes silently wrong every time a model ships. Prices
//! come from the same community table ccusage rates against
//! (`model_prices_and_context_window.json`), cached beside the snapshot and
//! backed by a vendored copy so a cold, offline machine still shows a figure.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::TokenCounts;
use crate::model_to_provider;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Prices for the models TokenGauge can attribute, sliced out of the full
/// LiteLLM table (3176 entries, 1.8MB) at vendor time.
const VENDORED_PRICES: &str = include_str!("prices.json");

/// How long a downloaded table is served before a refresh is attempted. Prices
/// change on the order of months; the fetch only exists so a new model gets a
/// number without waiting for a TokenGauge release.
const PRICE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Per-token prices in USD. Field names match LiteLLM's so the same struct
/// parses the upstream table and our slimmed cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct ModelPrice {
    #[serde(default, rename = "input_cost_per_token")]
    pub input: f64,
    #[serde(default, rename = "output_cost_per_token")]
    pub output: f64,
    #[serde(default, rename = "cache_creation_input_token_cost")]
    pub cache_write_5m: f64,
    /// A 1h cache write costs about 60% more than the 5m one, and on this
    /// machine 97.7% of cache-creation tokens are 1h writes: most of the cache
    /// bill rides on telling the two apart.
    #[serde(default, rename = "cache_creation_input_token_cost_above_1hr")]
    pub cache_write_1h: f64,
    #[serde(default, rename = "cache_read_input_token_cost")]
    pub cache_read: f64,
}

impl ModelPrice {
    pub fn cost(&self, t: &TokenCounts) -> f64 {
        // Models with no separate 1h entry bill both writes the same.
        let write_1h = if self.cache_write_1h > 0.0 {
            self.cache_write_1h
        } else {
            self.cache_write_5m
        };
        self.input * t.input as f64
            + self.output * t.output as f64
            + self.cache_write_5m * t.cache_write_5m as f64
            + write_1h * t.cache_write_1h as f64
            + self.cache_read * t.cache_read as f64
    }
}

#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    models: HashMap<String, ModelPrice>,
}

/// Strip a bracketed context-window suffix: `claude-opus-4-8[1m]` is the same
/// model on a longer context. LiteLLM carries no entry for the variant and no
/// `above_200k` tier for these models, so it is rated at the base price rather
/// than at a premium we would have to invent.
fn base_model_name(model: &str) -> &str {
    match model.split_once('[') {
        Some((base, _)) => base.trim_end_matches('-'),
        None => model,
    }
}

impl PriceTable {
    fn from_json(raw: &str) -> Result<Self> {
        let parsed: HashMap<String, serde_json::Value> =
            serde_json::from_str(raw).context("price table was not valid JSON")?;
        let mut models = HashMap::new();
        for (name, value) in parsed {
            // The upstream table also holds embeddings, rerankers and a
            // `sample_spec` doc entry; anything we cannot attribute to a
            // provider is not a model we will ever look up.
            if model_to_provider(&name).is_none() {
                continue;
            }
            if let Ok(price) = serde_json::from_value::<ModelPrice>(value)
                && (price.input > 0.0 || price.output > 0.0)
            {
                models.insert(name.to_lowercase(), price);
            }
        }
        Ok(Self { models })
    }

    pub fn vendored() -> Self {
        Self::from_json(VENDORED_PRICES).expect("vendored price table parses")
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn get(&self, model: &str) -> Option<&ModelPrice> {
        let lower = model.to_lowercase();
        if let Some(p) = self.models.get(&lower) {
            return Some(p);
        }
        self.models.get(&base_model_name(&lower).to_lowercase())
    }

    fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.models).unwrap_or_else(|_| "{}".into())
    }
}

/// Where the downloaded table is cached: beside the snapshot, like every other
/// derived state file.
pub fn price_cache_path(cache_file: &Path) -> PathBuf {
    cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-prices.json")
}

fn is_fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < PRICE_TTL)
        .unwrap_or(false)
}

fn download(timeout: Duration) -> Result<PriceTable> {
    let body = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()?
        .get(LITELLM_URL)
        .send()?
        .error_for_status()?
        .text()?;
    let table = PriceTable::from_json(&body)?;
    if table.is_empty() {
        anyhow::bail!("price table download held no usable models");
    }
    Ok(table)
}

/// Where a loaded table came from. An offline machine quietly rating everything
/// against the copy compiled in on release day is not a failure - it is the
/// deliberate fallback - but it is invisible from the cost row, which is why
/// `--doctor` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriceSource {
    /// Cached beside the snapshot, inside its freshness window.
    Fresh,
    /// Downloaded on this call.
    Downloaded,
    /// Cached but past its window; the network did not answer.
    Stale,
    /// The copy compiled into the binary. The floor every other case falls to,
    /// so it is also the sane default for a diagnosis that never ran.
    #[default]
    Vendored,
}

impl PriceSource {
    pub fn label(self) -> &'static str {
        match self {
            PriceSource::Fresh => "cached",
            PriceSource::Downloaded => "downloaded",
            PriceSource::Stale => "cached, past its refresh window",
            PriceSource::Vendored => "compiled in",
        }
    }

    /// A table that is not current. Not an error - it still prices everything
    /// it covers - but a model priced since this copy was made reads as
    /// unpriced, which looks like a reader bug from the cost row.
    pub fn is_current(self) -> bool {
        matches!(self, PriceSource::Fresh | PriceSource::Downloaded)
    }
}

/// Load the price table: fresh cache, else a download, else a stale cache, else
/// the vendored copy. Never fails - an outdated price is a better answer than
/// no cost at all, and an unpriced model is reported rather than shown as $0.
pub fn load(cache_file: &Path, timeout: Duration, allow_network: bool) -> PriceTable {
    load_with_source(cache_file, timeout, allow_network).0
}

/// [`load`], saying which of the four it fell through to.
pub fn load_with_source(
    cache_file: &Path,
    timeout: Duration,
    allow_network: bool,
) -> (PriceTable, PriceSource) {
    let path = price_cache_path(cache_file);
    if is_fresh(&path)
        && let Ok(raw) = fs::read_to_string(&path)
        && let Ok(table) = PriceTable::from_json(&raw)
        && !table.is_empty()
    {
        return (table, PriceSource::Fresh);
    }
    if allow_network && let Ok(table) = download(timeout) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = crate::write_atomic(&path, table.to_json().as_bytes());
        return (table, PriceSource::Downloaded);
    }
    if let Ok(raw) = fs::read_to_string(&path)
        && let Ok(table) = PriceTable::from_json(&raw)
        && !table.is_empty()
    {
        return (table, PriceSource::Stale);
    }
    (PriceTable::vendored(), PriceSource::Vendored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_table_covers_the_models_in_use() {
        let t = PriceTable::vendored();
        assert!(t.len() > 100, "vendored table looks truncated: {}", t.len());
        for model in [
            "claude-opus-5",
            "claude-fable-5",
            "claude-haiku-4-5-20251001",
            "gpt-5.5",
        ] {
            assert!(
                t.get(model).is_some(),
                "{model} missing from vendored table"
            );
        }
    }

    #[test]
    fn long_context_variant_falls_back_to_the_base_model() {
        let t = PriceTable::vendored();
        assert_eq!(t.get("claude-opus-4-8[1m]"), t.get("claude-opus-4-8"));
        assert!(t.get("claude-opus-4-8[1m]").is_some());
    }

    #[test]
    fn unknown_model_has_no_price_rather_than_a_zero_one() {
        assert!(PriceTable::vendored().get("claude-not-a-model-9").is_none());
    }

    #[test]
    fn an_hour_cache_write_costs_more_than_a_five_minute_one() {
        let t = PriceTable::vendored();
        let p = t.get("claude-opus-5").expect("opus priced");
        assert!(
            p.cache_write_1h > p.cache_write_5m,
            "1h write {} should exceed 5m write {}",
            p.cache_write_1h,
            p.cache_write_5m
        );

        let write_5m = ModelPrice::cost(
            p,
            &TokenCounts {
                cache_write_5m: 1_000_000,
                ..Default::default()
            },
        );
        let write_1h = ModelPrice::cost(
            p,
            &TokenCounts {
                cache_write_1h: 1_000_000,
                ..Default::default()
            },
        );
        assert!(write_1h > write_5m);
    }

    #[test]
    fn a_model_without_an_hour_price_bills_both_writes_alike() {
        let p = ModelPrice {
            cache_write_5m: 2.0,
            ..Default::default()
        };
        let counts = TokenCounts {
            cache_write_1h: 10,
            ..Default::default()
        };
        assert_eq!(p.cost(&counts), 20.0);
    }

    fn scratch(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-prices-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir.join("tokengauge-usage.json")
    }

    #[test]
    fn the_price_cache_sits_beside_the_snapshot() {
        let cache_file = Path::new("/var/lib/tokengauge/tokengauge-usage.json");
        assert_eq!(
            price_cache_path(cache_file),
            Path::new("/var/lib/tokengauge/tokengauge-prices.json")
        );
    }

    #[test]
    fn a_cached_table_is_served_without_the_network() {
        let cache_file = scratch("cached");
        fs::write(
            price_cache_path(&cache_file),
            r#"{"claude-opus-5":{"input_cost_per_token":1.0,"output_cost_per_token":2.0}}"#,
        )
        .expect("write cache");

        let table = load(&cache_file, Duration::from_secs(1), false);
        let price = table.get("claude-opus-5").expect("cached price");
        assert_eq!(price.input, 1.0);
        assert_eq!(price.output, 2.0);
        let _ = fs::remove_dir_all(cache_file.parent().expect("parent"));
    }

    /// A machine that has never reached LiteLLM rates everything against the
    /// table compiled in on release day. That is the deliberate fallback, and
    /// from the cost row it is indistinguishable from a current table - so the
    /// loader has to say which one it handed back.
    #[test]
    fn the_loader_says_which_of_the_four_it_fell_through_to() {
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-price-source-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let cache_file = dir.join("tokengauge-usage.json");

        // Nothing cached, no network allowed: the compiled-in copy.
        let (table, source) = load_with_source(&cache_file, Duration::from_secs(1), false);
        assert_eq!(source, PriceSource::Vendored);
        assert!(!source.is_current());
        assert_eq!(table.len(), PriceTable::vendored().len());

        // A cache written now is fresh, and reads as its own source.
        let path = price_cache_path(&cache_file);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("cache dir");
        std::fs::write(&path, PriceTable::vendored().to_json()).expect("write cache");
        let (_, source) = load_with_source(&cache_file, Duration::from_secs(1), false);
        assert_eq!(source, PriceSource::Fresh);
        assert!(source.is_current());

        // Backdated past the TTL, with the network refused: still served, but
        // named as stale rather than passed off as current.
        let old = SystemTime::now() - PRICE_TTL - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&path)
            .expect("reopen the cache")
            .set_modified(old)
            .expect("backdate the cache");
        let (table, source) = load_with_source(&cache_file, Duration::from_secs(1), false);
        assert_eq!(source, PriceSource::Stale);
        assert!(!source.is_current());
        assert!(!table.is_empty(), "a stale table is still served");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_falls_back_to_the_vendored_table() {
        let cache_file = scratch("corrupt");
        fs::write(price_cache_path(&cache_file), "{ not json").expect("write cache");

        // Offline, so the only way back is the compiled-in copy.
        let table = load(&cache_file, Duration::from_secs(1), false);
        assert_eq!(table.len(), PriceTable::vendored().len());
        assert!(table.get("claude-opus-5").is_some());
        let _ = fs::remove_dir_all(cache_file.parent().expect("parent"));
    }

    #[test]
    fn a_table_with_no_usable_models_is_not_served() {
        let cache_file = scratch("empty");
        // Valid JSON, but every entry is something we cannot attribute.
        fs::write(
            price_cache_path(&cache_file),
            r#"{"text-embedding-3-small":{"input_cost_per_token":1.0}}"#,
        )
        .expect("write cache");

        let table = load(&cache_file, Duration::from_secs(1), false);
        assert_eq!(table.len(), PriceTable::vendored().len());
        let _ = fs::remove_dir_all(cache_file.parent().expect("parent"));
    }

    #[test]
    fn cost_sums_every_token_class() {
        let p = ModelPrice {
            input: 1.0,
            output: 10.0,
            cache_write_5m: 2.0,
            cache_write_1h: 4.0,
            cache_read: 0.5,
        };
        let counts = TokenCounts {
            input: 1,
            output: 1,
            cache_write_5m: 1,
            cache_write_1h: 1,
            cache_read: 2,
        };
        assert_eq!(p.cost(&counts), 1.0 + 10.0 + 2.0 + 4.0 + 1.0);
    }
}
