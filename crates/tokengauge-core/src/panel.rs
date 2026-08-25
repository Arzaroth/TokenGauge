//! The panel layout, built once here and rendered by every frontend.
//!
//! Five surfaces draw the same panel in four toolkits - the waybar tooltip
//! (pango markup), the Plasma applet and the Quickshell widget (QML), the GNOME
//! extension (GJS), and the tray window (egui). Each one used to decide its own
//! section order, labels, number formatting, sort order and thresholds, so a
//! feature landed on whichever surface the change happened to touch.
//!
//! [`panel_spec`] resolves all of that once. A frontend receives an ordered list
//! of [`Section`]s already carrying display strings, and implements exactly
//! three primitives - [`SectionKind::Meters`], [`SectionKind::Bars`] and
//! [`SectionKind::Rows`]. A new section added here appears everywhere without a
//! per-frontend edit.
//!
//! What stays per-frontend is *chrome*, not content: the header, the update
//! banner, the provider selector and the settings pane are interactive and
//! toolkit-shaped. Everything a user reads lives in this file.

use serde::Serialize;

use crate::sync::DeviceCost;
use crate::{CostInfo, ModelCost, ProviderRow, format_tokens};

/// Colour tier for a row, resolved from the value rather than from a palette -
/// each frontend maps these onto its own theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Normal,
    Dim,
    Good,
    Warn,
    Critical,
}

impl Tone {
    /// The usual 0-49 / 50-79 / 80+ gauge tiers.
    pub fn for_percent(percent: u8) -> Self {
        match percent {
            0..=49 => Self::Good,
            50..=79 => Self::Warn,
            _ => Self::Critical,
        }
    }

    /// Burning ahead of an even rate is the warning direction; behind it is
    /// headroom. On-track says nothing worth tinting.
    pub fn for_pace(pace: &crate::UsagePace) -> Self {
        if pace.stage.is_ahead() {
            if pace.delta_percent.abs() > 6.0 {
                Self::Critical
            } else {
                Self::Warn
            }
        } else if pace.stage.is_behind() {
            Self::Good
        } else {
            Self::Dim
        }
    }

    /// Spending well above the prior daily average is the warning direction.
    fn for_trend(percent: f64) -> Self {
        if percent >= 25.0 {
            Self::Critical
        } else if percent >= -10.0 {
            Self::Warn
        } else {
            Self::Good
        }
    }
}

/// One line of a section. Which fields a frontend reads depends on the
/// section's [`SectionKind`]; the rest are empty rather than absent, so a
/// renderer never has to branch on presence.
#[derive(Debug, Clone, Serialize)]
pub struct PanelRow {
    pub label: String,
    /// Right-aligned headline value: `31%`, `384.0M · $312.21`.
    pub value: String,
    /// Secondary value trailing `value` on the same line, joined with `  ·  `:
    /// the token count next to a cost, the dollars next to a token count. A
    /// monospace frontend aligns it as its own column; the rest concatenate.
    pub suffix: String,
    /// Short tinted trailer: the pace projection `ends ~33%` on a meter, the
    /// `↑161% vs prior avg` trend on a cost row. Empty when there is none.
    pub badge: String,
    /// Colour for `badge` alone - the rest of the line stays dim.
    pub badge_tone: Tone,
    /// Dim line under the bar: `Resets in 15m`. Meters only.
    pub footnote: String,
    /// Bar fill, 0.0-1.0. `None` draws no bar.
    pub fraction: Option<f64>,
    pub tone: Tone,
    /// Today's row, the pinned row - drawn brighter and bold.
    pub emphasized: bool,
    /// Multi-line hover text, empty when the row has nothing more to say.
    pub tooltip: String,
}

impl PanelRow {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            suffix: String::new(),
            badge: String::new(),
            badge_tone: Tone::Dim,
            footnote: String::new(),
            fraction: None,
            tone: Tone::Normal,
            emphasized: false,
            tooltip: String::new(),
        }
    }
}

/// How a frontend draws a section's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    /// Label and value on one line, a full-width bar under it, then the
    /// footnote and badge. The limit gauges.
    Meters,
    /// One line per row with the bar filling the row behind the text, so a long
    /// list stays on one screen. Tokens by day and by model.
    Bars,
    /// Label, value and suffix on one line, no bar. The cost figures.
    Rows,
}

#[derive(Debug, Clone, Serialize)]
pub struct Section {
    /// Stable identifier - frontends key off this, never off the title.
    pub id: &'static str,
    pub title: &'static str,
    pub kind: SectionKind,
    pub rows: Vec<PanelRow>,
}

/// Section ids in canonical order. A frontend that renders the panel renders
/// these, in this order, skipping the ones the spec omits for lack of data.
pub const SECTION_IDS: &[&str] = &[
    "limits",
    "cost",
    "tokens_by_day",
    "tokens_by_model",
    "tokens_by_device",
];

/// What the panel has to say about fleet sync, resolved in the core so the
/// wording is the same on every frontend.
///
/// Error-first by construction: configured-but-not-working is the dangerous
/// state, because it under-reports silently instead of breaking, and a total
/// that is quietly too low is worse than one that is visibly missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncNote {
    pub devices: usize,
    pub tone: Tone,
    /// One word for the badge.
    pub headline: String,
    pub detail: String,
}

/// Build the panel for one provider. Sections with nothing to show are omitted
/// rather than emitted empty, so a frontend can render the list blindly.
pub fn panel_spec(row: &ProviderRow) -> Vec<Section> {
    let mut out = Vec::new();

    let limits = limit_rows(row);
    if !limits.is_empty() {
        out.push(Section {
            id: "limits",
            title: "LIMITS",
            kind: SectionKind::Meters,
            rows: limits,
        });
    }

    if let Some(cost) = row.cost.as_ref() {
        out.push(Section {
            id: "cost",
            title: "COST",
            kind: SectionKind::Rows,
            rows: cost_rows(cost),
        });

        let days = day_rows(cost);
        if !days.is_empty() {
            out.push(Section {
                id: "tokens_by_day",
                title: "TOKENS BY DAY",
                kind: SectionKind::Bars,
                rows: days,
            });
        }

        let models = model_rows(cost);
        if !models.is_empty() {
            out.push(Section {
                id: "tokens_by_model",
                // The cost layer is scoped to the calendar month. A bare
                // "Tokens by model" next to a panel counting all-time is worse
                // than a longer heading.
                title: "TOKENS BY MODEL · THIS MONTH",
                kind: SectionKind::Bars,
                rows: models,
            });
        }

        let devices = device_rows(cost);
        if !devices.is_empty() {
            out.push(Section {
                id: "tokens_by_device",
                title: "TOKENS BY DEVICE · THIS MONTH",
                kind: SectionKind::Bars,
                rows: devices,
            });
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Tokens by device
// ---------------------------------------------------------------------------

/// Present exactly when this provider is fleet-merged, which is what makes a
/// mixed per-provider setup readable without inventing a marker for it.
fn device_rows(cost: &CostInfo) -> Vec<PanelRow> {
    let max = cost.by_device.first().map(|d| d.tokens).unwrap_or(0);
    let now_ms = crate::now_ms() as i64;
    cost.by_device
        .iter()
        .map(|device| {
            let mut r = PanelRow::new(device.label.clone(), format_tokens(device.tokens));
            r.suffix = money(device.usd);
            r.fraction = Some(if max > 0 {
                device.tokens as f64 / max as f64
            } else {
                0.0
            });
            r.emphasized = device.is_local;
            if device.partial {
                r.badge = "partial".to_string();
                r.badge_tone = Tone::Dim;
            } else if !device.is_local {
                r.badge = ago(device.updated_at_ms, now_ms);
                r.badge_tone = Tone::Dim;
            }
            r.tooltip = device_tooltip(device, now_ms);
            r
        })
        .collect()
}

fn device_tooltip(device: &DeviceCost, now_ms: i64) -> String {
    let mut lines = vec![device.label.clone()];
    if device.is_local {
        lines.push("This machine".to_string());
    }
    lines.push(format!(
        "Last published  {}",
        ago(device.updated_at_ms, now_ms)
    ));
    lines.push(format!("Tokens  {}", exact_tokens(device.tokens)));
    if device.partial {
        lines.push(
            "Joined the fleet part-way through the month, so its share is only what it has covered"
                .to_string(),
        );
    }
    lines.join("\n")
}

/// Relative time for a device row, and for `--sync-status`.
pub fn ago_public(then_ms: i64, now_ms: i64) -> String {
    ago(then_ms, now_ms)
}

pub(crate) fn ago(then_ms: i64, now_ms: i64) -> String {
    let seconds = ((now_ms - then_ms) / 1000).max(0);
    match seconds {
        0..=89 => "just now".to_string(),
        90..=5399 => format!("{}m ago", seconds / 60),
        5400..=172_799 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

fn limit_rows(row: &ProviderRow) -> Vec<PanelRow> {
    let (session, weekly, tertiary) = crate::window_labels(&row.provider);
    let mut out = Vec::new();

    let mut push = |label: &str, used: Option<u8>, reset: &str, pace: Option<&crate::UsagePace>| {
        let Some(used) = used else { return };
        let mut r = PanelRow::new(label, format!("{used}%"));
        r.fraction = Some(f64::from(used) / 100.0);
        r.tone = Tone::for_percent(used);
        // A window with a percentage but no reset time has started counting
        // and has nowhere to reset to yet. Say so here rather than in one
        // frontend, or the other four render the line blank.
        r.footnote = if reset == "—" || reset.is_empty() {
            "not started".to_string()
        } else {
            format!("Resets {reset}")
        };
        if let Some(pace) = pace {
            r.badge = pace.badge();
            r.badge_tone = Tone::for_pace(pace);
        }
        out.push(r);
    };

    push(
        session,
        row.session_used,
        &row.session_reset,
        row.session_pace.as_ref(),
    );
    push(
        weekly,
        row.weekly_used,
        &row.weekly_reset,
        row.weekly_pace.as_ref(),
    );
    push(tertiary, row.tertiary_used, &row.tertiary_reset, None);

    for extra in &row.extra_windows {
        // A slot the provider exposes but reports nothing in is a permanently
        // empty meter. Only the waybar tooltip used to keep them, to hold its
        // line count steady; it now shares this list, so they go everywhere.
        if extra.placeholder {
            continue;
        }
        push(&extra.title, extra.used, &extra.reset, extra.pace.as_ref());
    }

    out
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

fn cost_rows(cost: &CostInfo) -> Vec<PanelRow> {
    let mut out = Vec::new();

    let mut today = PanelRow::new("Today", money(cost.today_usd));
    today.suffix = format!("{} tokens", format_tokens(cost.today_tokens));

    if let Some(pct) = cost.today_vs_avg_percent() {
        today.badge = format!(
            "{}{:.0}% vs prior avg",
            if pct >= 0.0 { "↑" } else { "↓" },
            pct.abs()
        );
        today.badge_tone = Tone::for_trend(pct);
    }
    out.push(today);

    if cost.session_usd > 0.0 {
        out.push(PanelRow::new("Session", money(cost.session_usd)));
    }
    if cost.weekly_usd > 0.0 {
        out.push(PanelRow::new("7-day", money(cost.weekly_usd)));
    }

    let mut month = PanelRow::new("This month", money(cost.monthly_usd));
    month.suffix = format!("{} tokens", format_tokens(cost.monthly_tokens));
    out.push(month);

    if let Some(burn) = cost.burn_rate.as_ref()
        && burn.cost_per_hour > 0.0
    {
        out.push(PanelRow::new(
            "Burn rate",
            format!("{}/hr", money(burn.cost_per_hour)),
        ));
    }

    if let Some(note) = cost.sync_note.as_ref() {
        let mut r = PanelRow::new(
            "Sync",
            match note.devices {
                1 => "1 device".to_string(),
                n => format!("{n} devices"),
            },
        );
        r.badge = note.headline.clone();
        r.badge_tone = note.tone;
        r.suffix = note.detail.clone();
        r.tooltip = if note.detail.is_empty() {
            "Cost and token figures cover every machine in the fleet".to_string()
        } else {
            note.detail.clone()
        };
        out.push(r);
    }

    out
}

// ---------------------------------------------------------------------------
// Tokens by day
// ---------------------------------------------------------------------------

fn day_rows(cost: &CostInfo) -> Vec<PanelRow> {
    // "Today" is the newest entry rather than the wall clock: the core always
    // ends the window on the current date, and a long-running shell that
    // compared against `now` would keep the marker on yesterday past midnight.
    let Some(today) = cost.weekly_history.last().map(|d| d.date.clone()) else {
        return Vec::new();
    };
    let max = cost
        .weekly_history
        .iter()
        .map(|d| d.tokens)
        .max()
        .unwrap_or(0);

    cost.weekly_history
        .iter()
        .map(|day| {
            let is_today = day.date == today;
            let mut r = PanelRow::new(
                if is_today {
                    "Today".to_string()
                } else {
                    weekday_label(&day.date)
                },
                format_tokens(day.tokens),
            );
            r.suffix = money(day.usd);
            r.fraction = Some(if max > 0 {
                day.tokens as f64 / max as f64
            } else {
                0.0
            });
            r.emphasized = is_today;
            r.tooltip = format!(
                "{}\n{} tokens\n${:.2}",
                long_date_label(&day.date),
                exact_tokens(day.tokens),
                day.usd
            );
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tokens by model
// ---------------------------------------------------------------------------

fn model_rows(cost: &CostInfo) -> Vec<PanelRow> {
    let mut models: Vec<&ModelCost> = cost.monthly_models.iter().collect();
    models.sort_by_key(|m| std::cmp::Reverse(m.tokens));
    let max = models.first().map(|m| m.tokens).unwrap_or(0);

    models
        .into_iter()
        .map(|m| {
            let mut r = PanelRow::new(model_label(&m.model), format_tokens(m.tokens));
            r.suffix = money(m.usd);
            r.fraction = Some(if max > 0 {
                m.tokens as f64 / max as f64
            } else {
                0.0
            });
            r.tooltip = model_tooltip(m);
            r
        })
        .collect()
}

fn model_tooltip(m: &ModelCost) -> String {
    let mut lines = vec![m.model.clone()];
    // The split only reaches the snapshot from ccusage 16+; older caches carry
    // zeroes, and a breakdown adding up to nothing is worse than none.
    let split = m.input_tokens + m.output_tokens + m.cache_creation_tokens + m.cache_read_tokens;
    if split > 0 {
        lines.push(format!("Input   {}", exact_tokens(m.input_tokens)));
        lines.push(format!("Output  {}", exact_tokens(m.output_tokens)));
        lines.push(format!(
            "Cache write  {}",
            exact_tokens(m.cache_creation_tokens)
        ));
        lines.push(format!(
            "Cache read   {}",
            exact_tokens(m.cache_read_tokens)
        ));
    } else {
        lines.push(format!("{} tokens", exact_tokens(m.tokens)));
    }
    lines.push(format!("${:.2} this month", m.usd));
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// `$1.23` under a hundred, `$312` above it - cents stop carrying information
/// once the figure is that large, and the extra digits push the value column
/// wide on a narrow panel.
pub fn money(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    if value.abs() >= 100.0 {
        format!("${}", value.round() as i64)
    } else {
        format!("${value:.2}")
    }
}

/// Thousands-separated, for tooltips where the rounded `384.0M` is not enough.
pub fn exact_tokens(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `claude-haiku-4-5-20251001` -> `Haiku 4.5`. Model ids separate version parts
/// with the same dash they use between words, so a plain dash-to-space pass
/// turns every point release into two numbers.
pub fn model_label(id: &str) -> String {
    let trimmed = id.rsplit_once('-').map_or(id, |(head, tail)| {
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            head
        } else {
            id
        }
    });
    let trimmed = ["claude-", "anthropic-", "openai-"]
        .iter()
        .find_map(|p| trimmed.strip_prefix(p))
        .unwrap_or(trimmed);

    let mut words: Vec<String> = Vec::new();
    for part in trimmed.split('-') {
        let numeric = !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
        let prev_ends_digit = words
            .last()
            .and_then(|w| w.chars().last())
            .is_some_and(|c| c.is_ascii_digit());
        if numeric && prev_ends_digit {
            let last = words.last_mut().expect("prev_ends_digit implies a last");
            last.push('.');
            last.push_str(part);
        } else {
            words.push(part.to_string());
        }
    }

    words
        .iter()
        .map(|word| match word.to_lowercase().as_str() {
            "gpt" | "glm" | "zai" | "ai" => word.to_uppercase(),
            _ => {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `2026-08-22` -> `Sat`. Falls back to the raw date when it will not parse.
fn weekday_label(date: &str) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(d) => d.format("%a").to_string(),
        Err(_) => date.to_string(),
    }
}

/// `2026-08-22` -> `Saturday 22 August`.
fn long_date_label(date: &str) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(d) => d.format("%A %-d %B").to_string(),
        Err(_) => date.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BurnRate, DayCost, ExtraWindowRow};

    fn row() -> ProviderRow {
        ProviderRow {
            provider: "Claude".into(),
            session_used: Some(31),
            session_window_minutes: Some(300),
            session_reset: "in 15m".into(),
            session_pace: None,
            weekly_used: Some(16),
            weekly_window_minutes: Some(10080),
            weekly_reset: "in 4d".into(),
            weekly_pace: None,
            tertiary_used: None,
            tertiary_reset: "—".into(),
            credits: "—".into(),
            source: "—".into(),
            updated: "just now".into(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: None,
            stale: false,
        }
    }

    fn cost() -> CostInfo {
        CostInfo {
            today_usd: 312.21,
            today_tokens: 384_000_000,
            monthly_usd: 1050.91,
            monthly_tokens: 1_400_000_000,
            today_models: Vec::new(),
            monthly_models: vec![
                ModelCost {
                    model: "claude-haiku-4-5-20251001".into(),
                    usd: 10.0,
                    tokens: 200,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
                ModelCost {
                    model: "claude-opus-5".into(),
                    usd: 90.0,
                    tokens: 800,
                    input_tokens: 100,
                    output_tokens: 200,
                    cache_creation_tokens: 300,
                    cache_read_tokens: 200,
                },
            ],
            burn_rate: Some(BurnRate {
                cost_per_hour: 56.09,
                tokens_per_minute: 10,
                remaining_minutes: 30,
                projected_cost: 1.0,
            }),
            session_usd: 172.92,
            weekly_usd: 1050.91,
            by_device: vec![
                DeviceCost {
                    device_id: "aaaa".into(),
                    label: "desktop".into(),
                    tokens: 900_000_000,
                    usd: 700.0,
                    updated_at_ms: crate::now_ms() as i64,
                    partial: false,
                    is_local: true,
                },
                DeviceCost {
                    device_id: "bbbb".into(),
                    label: "laptop".into(),
                    tokens: 500_000_000,
                    usd: 350.91,
                    updated_at_ms: crate::now_ms() as i64 - 7_200_000,
                    partial: true,
                    is_local: false,
                },
            ],
            sync_note: Some(SyncNote {
                devices: 2,
                tone: Tone::Good,
                headline: "ok".into(),
                detail: String::new(),
            }),
            weekly_cost_history: vec![1.0, 2.0],
            weekly_history: vec![
                DayCost {
                    date: "2026-08-21".into(),
                    usd: 1.0,
                    tokens: 500,
                },
                DayCost {
                    date: "2026-08-22".into(),
                    usd: 2.0,
                    tokens: 1000,
                },
            ],
        }
    }

    #[test]
    fn sections_follow_canonical_order() {
        let mut r = row();
        r.cost = Some(cost());
        let ids: Vec<&str> = panel_spec(&r).iter().map(|s| s.id).collect();
        assert_eq!(ids, SECTION_IDS);
    }

    #[test]
    fn the_device_section_reports_shares_and_flags_a_partial_machine() {
        let mut r = row();
        r.cost = Some(cost());
        let spec = panel_spec(&r);
        let devices = &spec
            .iter()
            .find(|s| s.id == "tokens_by_device")
            .unwrap()
            .rows;

        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].label, "desktop");
        assert!(devices[0].emphasized, "this machine is emphasized");
        assert_eq!(devices[0].fraction, Some(1.0));
        assert_eq!(devices[1].badge, "partial");
        assert!(devices[1].tooltip.contains("part-way through the month"));

        let sync = spec
            .iter()
            .find(|s| s.id == "cost")
            .unwrap()
            .rows
            .iter()
            .find(|r| r.label == "Sync")
            .expect("the cost section carries the sync state");
        assert_eq!(sync.value, "2 devices");
        assert_eq!(sync.badge, "ok");
    }

    #[test]
    fn a_provider_that_does_not_sync_looks_exactly_as_it_did() {
        let mut r = row();
        let mut cost = cost();
        cost.by_device.clear();
        cost.sync_note = None;
        r.cost = Some(cost);
        let spec = panel_spec(&r);

        let ids: Vec<&str> = spec.iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["limits", "cost", "tokens_by_day", "tokens_by_model"]
        );
        assert!(
            !spec
                .iter()
                .flat_map(|s| s.rows.iter())
                .any(|r| r.label == "Sync")
        );
    }

    #[test]
    fn cost_sections_are_omitted_without_cost() {
        let ids: Vec<&str> = panel_spec(&row()).iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["limits"]);
    }

    #[test]
    fn limits_carry_percent_fraction_and_tone() {
        let spec = panel_spec(&row());
        let limits = &spec[0].rows;
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[0].label, "Session");
        assert_eq!(limits[0].value, "31%");
        assert_eq!(limits[0].fraction, Some(0.31));
        assert_eq!(limits[0].tone, Tone::Good);
        assert_eq!(limits[0].footnote, "Resets in 15m");
        assert_eq!(limits[0].badge, "");
        // A window with no reset time reads the same on every surface.
        let mut no_reset = row();
        no_reset.session_reset = "—".into();
        assert_eq!(panel_spec(&no_reset)[0].rows[0].footnote, "not started");
    }

    #[test]
    fn tertiary_and_placeholder_extras_are_dropped() {
        let mut r = row();
        r.extra_windows = vec![
            ExtraWindowRow {
                title: "Daily Routines".into(),
                used: Some(0),
                reset: "—".into(),
                pace: None,
                placeholder: true,
            },
            ExtraWindowRow {
                title: "Fable only".into(),
                used: Some(9),
                reset: "in 4d".into(),
                pace: None,
                placeholder: false,
            },
        ];
        let spec = panel_spec(&r);
        let labels: Vec<&str> = spec[0].rows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["Session", "Weekly (all)", "Fable only"]);
    }

    #[test]
    fn days_mark_the_newest_entry_as_today() {
        let mut r = row();
        r.cost = Some(cost());
        let spec = panel_spec(&r);
        let days = &spec.iter().find(|s| s.id == "tokens_by_day").unwrap().rows;
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].label, "Fri");
        assert!(!days[0].emphasized);
        assert_eq!(days[1].value, "1.0K");
        assert_eq!(days[1].suffix, "$2.00");
        assert_eq!(days[1].label, "Today");
        assert!(days[1].emphasized);
        // Scaled against the biggest day, which is today's 1000.
        assert_eq!(days[0].fraction, Some(0.5));
        assert_eq!(days[1].fraction, Some(1.0));
    }

    #[test]
    fn models_sort_by_tokens_desc_and_scale_to_the_largest() {
        let mut r = row();
        r.cost = Some(cost());
        let spec = panel_spec(&r);
        let models = &spec
            .iter()
            .find(|s| s.id == "tokens_by_model")
            .unwrap()
            .rows;
        assert_eq!(models[0].label, "Opus 5");
        assert_eq!(models[0].fraction, Some(1.0));
        assert_eq!(models[1].label, "Haiku 4.5");
        assert_eq!(models[1].fraction, Some(0.25));
        // ccusage 16+ split present -> per-kind breakdown rather than a total.
        assert!(models[0].tooltip.contains("Cache read   200"));
        assert!(models[1].tooltip.contains("200 tokens"));
    }

    #[test]
    fn model_label_keeps_point_releases_together() {
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_label("claude-opus-5"), "Opus 5");
        assert_eq!(model_label("gpt-5-codex"), "GPT 5 Codex");
        assert_eq!(model_label("glm-4-6"), "GLM 4.6");
    }

    #[test]
    fn pace_and_trend_badges_carry_their_own_tone() {
        let now = chrono::Utc::now();
        let reset = (now + chrono::Duration::minutes(150)).to_rfc3339();
        let mut r = row();
        // Half a 5h window elapsed at 80% used -> far ahead of an even burn.
        r.session_pace = crate::UsagePace::for_window(80, Some(300), Some(&reset), now);
        r.cost = Some(cost());
        let spec = panel_spec(&r);

        let session = &spec[0].rows[0];
        assert!(session.badge.starts_with("empty in "));
        assert_eq!(session.badge_tone, Tone::Critical);
        assert_eq!(session.footnote, "Resets in 15m");

        let today = &spec.iter().find(|s| s.id == "cost").unwrap().rows[0];
        assert_eq!(today.suffix, "384.0M tokens");
        assert!(today.badge.ends_with("vs prior avg"));
    }

    /// Every frontend that draws the panel has to handle every section kind.
    /// The Rust frontends get that from an exhaustive `match`; the QML and JS
    /// ones do not, and a kind they quietly skip renders as a heading with
    /// nothing under it. Pin it here rather than discovering it on a desktop
    /// nobody in CI is running.
    #[test]
    fn every_panel_frontend_handles_every_section_kind() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root");
        let frontends = [
            (
                "waybar",
                "crates/tokengauge-waybar/src/main.rs",
                "SectionKind::",
            ),
            (
                "tray",
                "crates/tokengauge-tray/src/main.rs",
                "SectionKind::",
            ),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui/FullRep.qml",
                "\"",
            ),
            (
                "gnome",
                "gnome/tokengauge@arzaroth.github.io/extension.js",
                "'",
            ),
            ("quickshell", "omarchy/arzaroth.tokengauge/Panel.qml", "\""),
        ];

        for (id, path, prefix) in frontends {
            let src = std::fs::read_to_string(repo.join(path))
                .unwrap_or_else(|e| panic!("{id}: cannot read {path}: {e}"));
            for kind in ["meters", "bars", "rows"] {
                let needle = if prefix == "SectionKind::" {
                    let mut c = kind.chars();
                    format!(
                        "SectionKind::{}{}",
                        c.next().unwrap().to_uppercase(),
                        c.as_str()
                    )
                } else {
                    format!("{prefix}{kind}{prefix}")
                };
                assert!(
                    src.contains(&needle),
                    "{id} ({path}) never handles the `{kind}` section kind - \
                     looked for {needle}"
                );
            }
        }
    }

    #[test]
    fn money_drops_cents_over_a_hundred() {
        assert_eq!(money(1.5), "$1.50");
        assert_eq!(money(99.994), "$99.99");
        assert_eq!(money(312.21), "$312");
        assert_eq!(money(f64::NAN), "-");
    }

    #[test]
    fn exact_tokens_groups_thousands() {
        assert_eq!(exact_tokens(0), "0");
        assert_eq!(exact_tokens(999), "999");
        assert_eq!(exact_tokens(1_000), "1,000");
        assert_eq!(exact_tokens(384_000_000), "384,000,000");
    }
}
