//! Spend over the months and days the store still holds.
//!
//! The panel answers "what have I spent today, this week, this month". This
//! answers "and before that". It reads the same buckets `tokens_by_device`
//! splits - kept for [`fleet::STORE_RETENTION_DAYS`] and rolled up to a day
//! past [`fleet::HOURLY_RETENTION_DAYS`] - so nothing here opens a transcript
//! or asks a provider anything.
//!
//! Like [`crate::panel`], every string a user reads is resolved here, and a
//! frontend draws the chart its toolkit can draw without formatting any of it.
//! What differs is where it goes: the panel spec is the popup's main scroll,
//! and this is a second screen behind a toggle, because a year of bars is not
//! something to put above the limit gauges. Waybar has no second screen and
//! renders none of this - its second screen is the TUI, which left-click has
//! opened since long before there was any history to look at.

use chrono::{Datelike, Days, FixedOffset, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::cost::pricing::{PriceArchive, PriceTable};
use crate::fmt::format_tokens;
use crate::panel::{Tone, money};
use crate::sync::fleet::{self, FleetStore, Step};

/// How far back a history view reaches, and what it steps by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum HistoryRange {
    #[default]
    #[serde(rename = "30d")]
    Days30,
    #[serde(rename = "90d")]
    Days90,
    #[serde(rename = "12m")]
    Months12,
}

/// The ranges a frontend offers, in the order its selector shows them.
pub const HISTORY_RANGES: &[HistoryRange] = &[
    HistoryRange::Days30,
    HistoryRange::Days90,
    HistoryRange::Months12,
];

impl HistoryRange {
    /// Stable identifier. Frontends key off this, never off the label.
    pub fn id(self) -> &'static str {
        match self {
            Self::Days30 => "30d",
            Self::Days90 => "90d",
            Self::Months12 => "12m",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Days30 => "30 days",
            Self::Days90 => "90 days",
            Self::Months12 => "12 months",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        HISTORY_RANGES.iter().copied().find(|r| r.id() == id)
    }

    /// The next range in the selector, wrapping. What a key or a click cycles.
    pub fn next(self) -> Self {
        let at = HISTORY_RANGES.iter().position(|r| *r == self).unwrap_or(0);
        HISTORY_RANGES[(at + 1) % HISTORY_RANGES.len()]
    }

    fn step(self) -> Step {
        match self {
            Self::Days30 | Self::Days90 => Step::Day,
            Self::Months12 => Step::Month,
        }
    }

    fn count(self) -> u32 {
        match self {
            Self::Days30 => 30,
            Self::Days90 => 90,
            Self::Months12 => 12,
        }
    }
}

/// One step of a series, with everything a bar needs already decided.
///
/// Serialize only, like [`crate::panel::Section`]: it is resolved from the
/// store on every render rather than stored, so nothing ever reads one back.
#[derive(Debug, Clone, Serialize)]
pub struct HistoryPoint {
    /// `2026-07-14` for a day, `2026-07` for a month. Matches the store's key.
    pub key: String,
    /// Short enough for an axis tick.
    pub label: String,
    /// Unambiguous, for a tooltip.
    pub full_label: String,
    pub usd: String,
    pub tokens: String,
    /// Height against the tallest point in this series, 0 to 1.
    pub fraction: f64,
    /// The step still in progress. Its bar is short because it is not over
    /// yet, not because spend collapsed, and every chart that does not say so
    /// ends on a cliff the user reads as a drop.
    pub partial: bool,
    pub tone: Tone,
}

/// A provider's spend over one range, oldest first, with no gaps: a step with
/// no buckets is a zero rather than a missing bar, or a quiet week would draw
/// as a narrower chart instead of as a quiet week.
#[derive(Debug, Clone, Serialize)]
pub struct HistorySeries {
    pub id: &'static str,
    pub label: &'static str,
    pub points: Vec<HistoryPoint>,
    pub total_usd: String,
    pub total_tokens: String,
    /// Mean over the *completed* steps. The partial one is left out for the
    /// same reason [`crate::payload::CostInfo::avg_daily_cost`] leaves today
    /// out: a half-finished step would dilute its own baseline.
    pub average_usd: String,
    /// Nothing was spent anywhere in the range.
    pub empty: bool,
}

/// Every range, resolved once, plus what qualifies the figures.
///
/// `Default` is an empty panel rather than a missing one, so a frontend can
/// hold one in a row it builds before it has read the store.
#[derive(Debug, Clone, Default, Serialize)]
pub struct HistoryPanel {
    pub series: Vec<HistorySeries>,
    /// How far back the store actually holds anything, in words.
    pub covers: String,
    /// Anything that qualifies the numbers: a store that would not load, or
    /// months older than the vendored price archive, which are rated at
    /// today's prices rather than at the ones that were in effect.
    pub notes: Vec<String>,
}

impl HistoryPanel {
    pub fn is_empty(&self) -> bool {
        self.series.iter().all(|series| series.empty)
    }
}

/// Every range for one provider.
///
/// All of them at once because switching range is a click in a pane that is
/// already open, and a frontend that had to re-run `--json` for it would
/// redraw the whole popup to move one chart.
pub fn history_panel(
    store: &FleetStore,
    provider: &str,
    today: NaiveDate,
    offset: FixedOffset,
    prices: &PriceTable,
    archive: &PriceArchive,
) -> HistoryPanel {
    let mut unpriced = std::collections::BTreeSet::new();
    let series = HISTORY_RANGES
        .iter()
        .map(|range| {
            let (series, missing) =
                build_series(store, provider, *range, today, offset, prices, archive);
            unpriced.extend(missing);
            series
        })
        .collect();

    let mut notes = Vec::new();
    // A bar is drawn from the money, so a model with no price makes its step
    // shorter than it was - and a step carrying nothing else draws as flat
    // beside a real token count, which reads as a month that cost nothing
    // rather than one nobody can price. `--doctor` already names these; the
    // chart has to say it too, because the chart is where the gap is visible.
    if !unpriced.is_empty() {
        notes.push(unpriced_note(&unpriced));
    }
    if let (Some(earliest), Some(oldest)) = (archive.earliest(), month_starts(today, 12).first()) {
        let oldest_key = crate::cost::pricing::month_key(*oldest);
        if oldest_key.as_str() < earliest {
            notes.push(format!(
                "months before {earliest} are rated at today's prices; this build's price history does not reach them"
            ));
        }
    }

    HistoryPanel {
        series,
        covers: covers(store, offset),
        notes,
    }
}

/// [`history_panel`] against the local clock.
///
/// For a frontend with no reason to hold a chrono type of its own - the tray
/// would have taken the whole crate as a dependency to name a `NaiveDate`. A
/// caller resolving several providers at once should still use
/// [`history_panel`] with one clock reading, so they cannot straddle midnight.
pub fn history_panel_now(
    store: &FleetStore,
    provider: &str,
    prices: &PriceTable,
    archive: &PriceArchive,
) -> HistoryPanel {
    let now = chrono::Local::now();
    history_panel(
        store,
        provider,
        now.date_naive(),
        *now.offset(),
        prices,
        archive,
    )
}

/// The models with no price, named, and capped so one bad price table cannot
/// turn the note into the panel.
fn unpriced_note(unpriced: &std::collections::BTreeSet<String>) -> String {
    const SHOWN: usize = 3;
    let names: Vec<&str> = unpriced.iter().take(SHOWN).map(String::as_str).collect();
    let listed = match unpriced.len().saturating_sub(SHOWN) {
        0 => names.join(", "),
        rest => format!("{} and {rest} more", names.join(", ")),
    };
    format!("no price for {listed}: their tokens are counted, their cost is not")
}

/// The series, and the models it could not rate.
fn build_series(
    store: &FleetStore,
    provider: &str,
    range: HistoryRange,
    today: NaiveDate,
    offset: FixedOffset,
    prices: &PriceTable,
    archive: &PriceArchive,
) -> (HistorySeries, std::collections::BTreeSet<String>) {
    let step = range.step();
    let starts: Vec<NaiveDate> = match step {
        Step::Day => day_starts(today, range.count()),
        Step::Month => month_starts(today, range.count()),
    };
    let from = starts.first().copied().unwrap_or(today);
    let walk = store.totals_by_step(provider, (from, today), offset, step, prices, archive);
    let totals = walk.steps;

    let peak = totals.values().map(|(_, usd)| *usd).fold(0.0_f64, f64::max);
    let last = starts.len().saturating_sub(1);

    let points: Vec<HistoryPoint> = starts
        .iter()
        .enumerate()
        .map(|(at, date)| {
            let key = match step {
                Step::Day => date.to_string(),
                Step::Month => crate::cost::pricing::month_key(*date),
            };
            let (tokens, usd) = totals.get(&key).copied().unwrap_or((0, 0.0));
            let partial = at == last;
            HistoryPoint {
                label: match step {
                    Step::Day => date.format("%-d %b").to_string(),
                    Step::Month => date.format("%b").to_string(),
                },
                full_label: match step {
                    Step::Day => date.format("%-d %b %Y").to_string(),
                    Step::Month => date.format("%B %Y").to_string(),
                },
                key,
                usd: money(usd),
                tokens: format_tokens(tokens),
                fraction: if peak > 0.0 { usd / peak } else { 0.0 },
                partial,
                tone: if partial { Tone::Dim } else { Tone::Normal },
            }
        })
        .collect();

    let total_usd: f64 = totals.values().map(|(_, usd)| *usd).sum();
    let total_tokens: u64 = totals.values().map(|(tokens, _)| *tokens).sum();
    let completed = points.len().saturating_sub(1);
    let average = if completed > 0 {
        let sum: f64 = points[..completed]
            .iter()
            .enumerate()
            .map(|(at, _)| {
                let key = &points[at].key;
                totals.get(key).map(|(_, usd)| *usd).unwrap_or(0.0)
            })
            .sum();
        sum / completed as f64
    } else {
        0.0
    };

    let series = HistorySeries {
        id: range.id(),
        label: range.label(),
        points,
        total_usd: money(total_usd),
        total_tokens: format_tokens(total_tokens),
        average_usd: money(average),
        empty: total_tokens == 0,
    };
    (series, walk.unpriced)
}

/// The `count` days ending today, oldest first.
fn day_starts(today: NaiveDate, count: u32) -> Vec<NaiveDate> {
    (0..count)
        .rev()
        .filter_map(|back| today.checked_sub_days(Days::new(u64::from(back))))
        .collect()
}

/// The first of each of the `count` months ending this one, oldest first.
fn month_starts(today: NaiveDate, count: u32) -> Vec<NaiveDate> {
    let (mut year, mut month) = (today.year(), today.month());
    let mut out = Vec::with_capacity(count as usize);
    for _ in 0..count {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, 1) {
            out.push(date);
        }
        if month == 1 {
            year -= 1;
            month = 12;
        } else {
            month -= 1;
        }
    }
    out.reverse();
    out
}

/// How far back the store holds anything, from the read windows rather than
/// from the oldest bucket: a quiet fortnight is not missing data.
fn covers(store: &FleetStore, offset: FixedOffset) -> String {
    let earliest = store
        .devices
        .values()
        .filter_map(|slice| slice.covers_from)
        .min();
    match earliest {
        Some(hour) => format!("since {}", hour.date_at(offset).format("%-d %b %Y")),
        None => "no history yet".to_string(),
    }
}

/// The days of history the store is able to hold at all, for `--doctor` and
/// the docs.
pub const RETENTION_DAYS: i64 = fleet::STORE_RETENTION_DAYS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::contribution::{Bucket, DeviceRecord, Granularity};
    use crate::sync::hour::Hour;

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).expect("utc")
    }

    fn store_with(buckets: Vec<Bucket>) -> FleetStore {
        let mut store = FleetStore::default();
        let slice = store.devices.entry("dev".to_string()).or_default();
        slice.device = DeviceRecord {
            id: "dev".into(),
            hostname: "host".into(),
            label: String::new(),
            os: "linux".into(),
        };
        slice.covers_from = buckets.iter().map(|b| b.hour).min();
        slice.buckets = buckets;
        store
    }

    fn bucket(day: &str, tokens: u64) -> Bucket {
        Bucket {
            hour: Hour::parse(&format!("{day}T12")).expect("hour"),
            provider: "claude".into(),
            model: "claude-sonnet-4-5".into(),
            granularity: Granularity::Hour,
            tokens: crate::cost::TokenCounts {
                input: tokens,
                ..Default::default()
            },
        }
    }

    fn day(text: &str) -> NaiveDate {
        text.parse().expect("date")
    }

    #[test]
    fn a_quiet_step_is_a_zero_and_not_a_missing_bar() {
        let store = store_with(vec![bucket("2026-08-20", 1000)]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        let series = &panel.series[0];
        assert_eq!(series.points.len(), 30, "every step is present");
        assert_eq!(series.points.last().expect("today").key, "2026-08-25");
        assert!(!series.empty);
        let spent: Vec<&HistoryPoint> = series.points.iter().filter(|p| p.fraction > 0.0).collect();
        assert_eq!(spent.len(), 1, "one day carried tokens");
        assert_eq!(spent[0].key, "2026-08-20");
    }

    #[test]
    fn the_step_in_progress_is_marked_so_a_chart_does_not_end_on_a_cliff() {
        let store = store_with(vec![bucket("2026-08-25", 1000)]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        for series in &panel.series {
            let last = series.points.last().expect("a step");
            assert!(last.partial, "{} ends on the step in progress", series.id);
            assert_eq!(last.tone, Tone::Dim);
            assert!(
                series.points[..series.points.len() - 1]
                    .iter()
                    .all(|p| !p.partial),
                "only the last step is partial"
            );
        }
    }

    #[test]
    fn a_month_series_steps_by_month_and_ends_on_this_one() {
        let store = store_with(vec![bucket("2026-06-14", 1000)]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        let months = panel
            .series
            .iter()
            .find(|s| s.id == "12m")
            .expect("a 12 month series");
        assert_eq!(months.points.len(), 12);
        assert_eq!(months.points.last().expect("this month").key, "2026-08");
        assert_eq!(months.points[0].key, "2025-09");
        let june = months
            .points
            .iter()
            .find(|p| p.key == "2026-06")
            .expect("june");
        assert!(june.fraction > 0.0, "june carried the tokens");
    }

    #[test]
    fn the_average_leaves_out_the_step_still_running() {
        // Two complete days at the same spend, plus a partial one at zero. The
        // average is the completed pair's, not a third of it.
        let store = store_with(vec![
            bucket("2026-08-23", 1_000_000),
            bucket("2026-08-24", 1_000_000),
        ]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        let series = &panel.series[0];
        let total: f64 = series
            .total_usd
            .trim_start_matches('$')
            .parse()
            .unwrap_or(0.0);
        let average: f64 = series
            .average_usd
            .trim_start_matches('$')
            .parse()
            .unwrap_or(0.0);
        assert!(total > 0.0, "the pair spent something");
        assert!(
            (average - total / 29.0).abs() < total / 1000.0,
            "averaged over 29 completed days, not 30: total {total}, average {average}"
        );
    }

    #[test]
    fn an_empty_store_says_so_rather_than_drawing_nothing() {
        let panel = history_panel(
            &FleetStore::default(),
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        assert!(panel.is_empty());
        assert_eq!(panel.covers, "no history yet");
        assert!(
            panel.series.iter().all(|s| !s.points.is_empty()),
            "the steps still exist, they are just all zero"
        );
    }

    #[test]
    fn a_model_with_no_price_is_named_rather_than_left_to_a_flat_bar() {
        // The bar is drawn from the money, so an unpriced model draws as a
        // month that cost nothing beside a real token count. The tokens still
        // count; the panel says why the money does not match them.
        let store = store_with(vec![Bucket {
            hour: Hour::parse("2026-08-20T12").expect("hour"),
            provider: "claude".into(),
            model: "a-model-nobody-prices".into(),
            granularity: Granularity::Hour,
            tokens: crate::cost::TokenCounts {
                input: 1_000_000,
                ..Default::default()
            },
        }]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );

        let note = panel
            .notes
            .iter()
            .find(|n| n.contains("no price"))
            .unwrap_or_else(|| panic!("nothing said so: {:?}", panel.notes));
        assert!(note.contains("a-model-nobody-prices"), "{note}");
        assert!(note.contains("tokens are counted"), "{note}");

        let series = &panel.series[0];
        assert!(
            !series.empty,
            "the tokens are real even if the money is not"
        );
        assert_eq!(series.total_usd, "$0.00");
    }

    #[test]
    fn a_priced_range_says_nothing_about_prices() {
        let store = store_with(vec![bucket("2026-08-20", 1000)]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        assert!(
            !panel.notes.iter().any(|n| n.contains("no price")),
            "a fully priced range must not carry the caveat: {:?}",
            panel.notes
        );
    }

    #[test]
    fn an_empty_range_reads_as_zero_and_not_as_minus_zero() {
        // A range whose buckets all fall outside it sums to nothing, and
        // "$-0.00" on a chart reads as a number someone got wrong.
        let store = store_with(vec![bucket("2026-01-05", 1000)]);
        let panel = history_panel(
            &store,
            "claude",
            day("2026-08-25"),
            utc(),
            &PriceTable::vendored(),
            &PriceArchive::vendored(),
        );
        let series = &panel.series[0];
        assert!(series.empty, "nothing was spent in the last 30 days");
        assert_eq!(series.total_usd, "$0.00");
        assert_eq!(series.average_usd, "$0.00");
        for point in &series.points {
            assert_eq!(point.usd, "$0.00", "{} printed a signed zero", point.key);
        }
    }

    #[test]
    fn a_range_cycles_through_every_option() {
        let mut seen = vec![HistoryRange::default()];
        for _ in 0..HISTORY_RANGES.len() {
            let next = seen.last().expect("a range").next();
            if !seen.contains(&next) {
                seen.push(next);
            }
        }
        assert_eq!(seen.len(), HISTORY_RANGES.len(), "cycling reaches them all");
        assert_eq!(
            HistoryRange::from_id("90d"),
            Some(HistoryRange::Days90),
            "an id round-trips"
        );
    }
}
