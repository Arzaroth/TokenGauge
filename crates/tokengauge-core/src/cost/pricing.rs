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

/// The vendor paths LiteLLM files a provider's models under.
///
/// Only the makers whose models are sold through someone else's namespace need
/// one. Anthropic and OpenAI models are keyed bare, which is why this went
/// unnoticed for as long as those were the only two providers with a reader.
fn vendor_prefixes(model: &str) -> &'static [&'static str] {
    match model_to_provider(model) {
        Some("glm") => &["zai/", "z-ai/", "openrouter/z-ai/"],
        Some("grok") => &["xai/", "x-ai/", "openrouter/x-ai/"],
        Some("kimi") => &["moonshot/", "moonshotai/", "openrouter/moonshotai/"],
        _ => &[],
    }
}

/// Plan names a CLI reports in place of a model.
///
/// The Kimi Code subscription writes `kimi-for-coding` whatever the plan is
/// routing to that month, and LiteLLM prices models, not plans. Rated at the
/// model the plan currently serves: ccusage splits this on a timestamp because
/// the plan served k2.5 until April 2026, which this does not reproduce - a
/// transcript that old is outside every window the panel draws.
const PLAN_ALIASES: &[(&str, &str)] = &[("kimi-for-coding", "moonshot/kimi-k2.6")];

fn push_unique(out: &mut Vec<String>, candidate: String) {
    if !candidate.is_empty() && !out.contains(&candidate) {
        out.push(candidate);
    }
}

/// The price-table keys to try for one transcript model name, in order.
///
/// A transcript carries the id the CLI was configured with - `glm-4.6`,
/// `kimi-k2-thinking`, `grok-4.5-build` - and LiteLLM keys the same model under
/// the vendor selling it. Without the walk, every GLM and Kimi call read out of
/// a Claude Code transcript was attributed to the right provider, counted into
/// the right day, and then rated at zero.
fn price_candidates(lower: &str) -> Vec<String> {
    let mut out = Vec::new();
    push_unique(&mut out, lower.to_string());

    // Grok names the plan in the model id, and the bracket has to come off
    // before the context-window suffix is read: a leading `[` would otherwise
    // take the whole name with it.
    let unbracketed = lower.strip_prefix("[grok] ").unwrap_or(lower).trim();
    let base = base_model_name(unbracketed);
    push_unique(&mut out, base.to_string());
    let unbuilt = base.strip_suffix("-build").unwrap_or(base);
    push_unique(&mut out, unbuilt.to_string());

    for name in [base, unbuilt] {
        if let Some((_, alias)) = PLAN_ALIASES.iter().find(|(plan, _)| *plan == name) {
            push_unique(&mut out, (*alias).to_string());
        }
        for prefix in vendor_prefixes(name) {
            push_unique(&mut out, format!("{prefix}{name}"));
        }
    }
    out
}

/// Which provider a *price-table key* belongs to, and `None` when no name a
/// transcript can carry would ever resolve to it.
///
/// LiteLLM keys a model by where you buy it rather than by who made it, so
/// asking [`model_to_provider`] about the whole key kept `moonshot/...` - which
/// happens to lead with a maker's name - and dropped all 75 Grok and all 98 GLM
/// entries in the table on the floor.
///
/// The mirror of [`price_candidates`]: a bare name, or one under a vendor path
/// that function builds. LiteLLM also files the same models under bedrock,
/// azure and half a dozen resellers, and nothing here can ask for those - they
/// would be 700 entries of dead weight compiled into every binary.
fn attribute_price_key(key: &str) -> Option<&'static str> {
    if let Some(provider) = model_to_provider(key) {
        return Some(provider);
    }
    let bare = key.rsplit('/').next()?;
    let provider = model_to_provider(bare)?;
    vendor_prefixes(bare)
        .iter()
        .any(|prefix| key.len() == prefix.len() + bare.len() && key.starts_with(prefix))
        .then_some(provider)
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
            if attribute_price_key(&name.to_lowercase()).is_none() {
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
        price_candidates(&lower)
            .iter()
            .find_map(|candidate| self.models.get(candidate))
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

/// Where the models already asked about are remembered, so one is asked once.
fn missed_path(cache_file: &Path) -> PathBuf {
    cache_file
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tokengauge-prices-missed.json")
}

/// Ask upstream again when a model was counted but not priced.
///
/// A table inside its freshness window is served without asking, so a model
/// released after the last download reads as $0 for up to [`PRICE_TTL`] -
/// counted, rated at nothing, and indistinguishable in the panel from a day
/// that cost nothing. Tokens with no price are proof the table is behind, the
/// way a window that has reset is proof the snapshot is, so they buy one
/// download outside the window.
///
/// One, and once per set: a model upstream will never carry - a local model, or
/// a provider sold under a namespace [`vendor_prefixes`] misses - is unpriced
/// on every fetch, and asking each time would re-download the table every
/// `refresh_secs` forever. The set that came back unpriced is recorded, and an
/// identical one is not asked about twice. A download that failed records
/// nothing, so an offline machine retries rather than burning its one ask.
pub fn refetch_for_unpriced(
    cache_file: &Path,
    unpriced: &[String],
    timeout: Duration,
) -> Option<PriceTable> {
    if unpriced.is_empty() {
        return None;
    }
    let path = missed_path(cache_file);
    let asked: Vec<String> = fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if asked == unpriced {
        return None;
    }
    let table = download(timeout).ok()?;
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = crate::write_atomic(&price_cache_path(cache_file), table.to_json().as_bytes());
    let _ = crate::write_atomic(
        &path,
        serde_json::to_string(unpriced)
            .unwrap_or_default()
            .as_bytes(),
    );
    Some(table)
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

    /// The bug this walk exists for: a GLM or Kimi plan driven through Claude
    /// Code lands in `~/.claude/projects` under its own model name, is
    /// attributed to the right provider by `model_to_provider`, and was then
    /// rated at zero because LiteLLM files those models under `zai/` and
    /// `moonshot/`. Tokens counted, money silently missing.
    #[test]
    fn a_model_sold_under_a_vendor_path_is_priced_by_its_bare_name() {
        let t = PriceTable::vendored();
        for model in [
            "glm-4.6",
            "glm-4.7",
            "glm-5",
            "kimi-k2-thinking",
            "kimi-k2.5",
            "kimi-k2.6",
            "grok-4",
            "grok-code-fast-1",
        ] {
            assert!(t.get(model).is_some(), "{model} priced at nothing");
        }
        assert_eq!(t.get("glm-4.6"), t.get("zai/glm-4.6"));
        assert_eq!(t.get("kimi-k2.6"), t.get("moonshot/kimi-k2.6"));
    }

    /// Grok reports the plan in the model id. Both spellings have to reach the
    /// same price, and the bracket has to come off before the context-window
    /// suffix is read - `base_model_name` would otherwise take the whole name.
    #[test]
    fn grok_plan_decoration_does_not_hide_the_model() {
        let t = PriceTable::vendored();
        let plain = t.get("grok-4.5");
        assert!(plain.is_some(), "grok-4.5 priced at nothing");
        assert_eq!(t.get("grok-4.5-build"), plain);
        assert_eq!(t.get("[grok] grok-4.5-build"), plain);
    }

    /// `kimi-for-coding` is the subscription, not a model, and LiteLLM prices
    /// models.
    #[test]
    fn a_plan_alias_is_rated_at_the_model_the_plan_serves() {
        let t = PriceTable::vendored();
        assert_eq!(t.get("kimi-for-coding"), t.get("moonshot/kimi-k2.6"));
        assert!(t.get("kimi-for-coding").is_some());
    }

    /// The table carries only what `price_candidates` can ask for. LiteLLM
    /// lists these same models under bedrock, azure and several resellers;
    /// carrying those would be several hundred entries in every binary that
    /// no lookup can reach.
    #[test]
    fn the_table_holds_only_keys_a_lookup_can_reach() {
        assert_eq!(attribute_price_key("zai/glm-4.6"), Some("glm"));
        assert_eq!(attribute_price_key("xai/grok-4"), Some("grok"));
        assert_eq!(attribute_price_key("openrouter/z-ai/glm-4.6"), Some("glm"));
        assert_eq!(attribute_price_key("claude-opus-5"), Some("claude"));
        assert_eq!(attribute_price_key("bedrock/us-east-1/zai.glm-5"), None);
        assert_eq!(attribute_price_key("azure_ai/grok-4"), None);
        assert_eq!(attribute_price_key("baseten/zai-org/glm-4.6"), None);
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

    /// The bound that keeps the retry from becoming a download loop: a model
    /// upstream will never carry comes back unpriced on every fetch, and only
    /// the first one asks.
    #[test]
    fn an_unpriced_set_is_only_asked_about_once() {
        let cache = scratch("asked-once");
        let missed = missed_path(&cache);
        fs::write(&missed, r#"["claude-not-a-model-9"]"#).expect("marker");
        let unpriced = vec!["claude-not-a-model-9".to_string()];
        assert!(refetch_for_unpriced(&cache, &unpriced, Duration::from_millis(1)).is_none());
    }

    #[test]
    fn a_priced_read_asks_nothing() {
        let cache = scratch("asks-nothing");
        assert!(refetch_for_unpriced(&cache, &[], Duration::from_millis(1)).is_none());
        assert!(!missed_path(&cache).exists());
    }

    /// A download that did not answer must not burn the one ask, or an offline
    /// machine never picks the price up at all.
    #[test]
    fn a_failed_ask_is_not_recorded() {
        let cache = scratch("failed-ask");
        let missed = missed_path(&cache);
        fs::write(&missed, r#"["claude-old-gap"]"#).expect("marker");
        let unpriced = vec!["claude-new-gap".to_string()];
        // A 1ms timeout cannot complete, online or off.
        assert!(refetch_for_unpriced(&cache, &unpriced, Duration::from_millis(1)).is_none());
        assert_eq!(
            fs::read_to_string(&missed).expect("marker survives"),
            r#"["claude-old-gap"]"#
        );
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
