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
use crate::{
    CostInfo, CreditLimit, CreditLimitKind, DayModelCost, ModelCost, PanelConfig, PlansTotal,
    ProviderRow, format_tokens,
};

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
    /// The bar split into one segment per credential, left to right. Empty
    /// draws `fraction` as one bar. Meters only.
    pub segments: Vec<Segment>,
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
            segments: Vec::new(),
            tone: Tone::Normal,
            emphasized: false,
            tooltip: String::new(),
        }
    }
}

/// One credential's stretch of a split bar.
#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    /// Share of the bar's width, 0.0-1.0. A row's segments add up to 1.
    pub width: f64,
    /// How much of its own stretch the segment fills, 0.0-1.0.
    pub fraction: f64,
    pub tone: Tone,
}

/// Whether a split bar of `count` segments has room for a separator between
/// each pair and a cell for each segment in `width` cells.
pub fn segment_separators(count: usize, width: usize) -> bool {
    count > 0 && width >= 2 * count - 1
}

/// A split bar in a character grid: how many cells each segment gets and how
/// many of those are filled, when the bar is `width` cells including the
/// separators [`segment_separators`] says fit. The cells add up to exactly
/// that, so a split bar lines up with the plain bars beside it and never runs
/// past its slot; each segment gets one cell at least while there are cells
/// enough to go round. Waybar and the TUI draw in cells; the pixel frontends
/// use [`Segment::width`] directly.
pub fn segment_cells(segments: &[Segment], width: usize) -> Vec<(usize, usize)> {
    let n = segments.len();
    if n == 0 {
        return Vec::new();
    }
    let content = if segment_separators(n, width) {
        width - (n - 1)
    } else {
        width
    };
    let least = usize::from(content >= n);
    let ideal: Vec<f64> = segments.iter().map(|s| s.width * content as f64).collect();
    let mut cells: Vec<usize> = ideal.iter().map(|i| (*i as usize).max(least)).collect();
    while cells.iter().sum::<usize>() > content {
        let Some(widest) = (0..n)
            .filter(|i| cells[*i] > least)
            .max_by_key(|i| cells[*i])
        else {
            break;
        };
        cells[widest] -= 1;
    }
    while cells.iter().sum::<usize>() < content {
        let behind = (0..n)
            .max_by(|a, b| {
                (ideal[*a] - cells[*a] as f64).total_cmp(&(ideal[*b] - cells[*b] as f64))
            })
            .expect("at least one segment");
        cells[behind] += 1;
    }
    segments
        .iter()
        .zip(cells)
        .map(|(s, cells)| {
            let filled = (s.fraction.clamp(0.0, 1.0) * cells as f64).round() as usize;
            (cells, filled.min(cells))
        })
        .collect()
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
    /// Resolved here like every other string: a credential's group names the
    /// credential in its title.
    pub title: String,
    pub kind: SectionKind,
    pub rows: Vec<PanelRow>,
    /// The credential a `limits` section belongs to, when a provider has
    /// several and each gets its own. They share the id; this tells them
    /// apart. No frontend has to read it to draw the panel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

/// Section ids in canonical order. A frontend that renders the panel renders
/// these, in this order, skipping the ones the spec omits for lack of data.
/// `limits` repeats once per credential when a provider has several, each
/// carrying its [`Section::group`].
pub const SECTION_IDS: &[&str] = &[
    "status",
    "plans",
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
pub fn panel_spec(row: &ProviderRow, options: &PanelConfig) -> Vec<Section> {
    let mut out = Vec::new();

    if !row.credentials.is_empty() {
        out.extend(credential_sections(&row.credentials, options));
    } else {
        // First, because it says whether to believe anything under it.
        let status = status_rows(row, None);
        if !status.is_empty() {
            out.push(Section {
                id: "status",
                title: "STATUS".into(),
                kind: SectionKind::Rows,
                rows: status,
                group: None,
            });
        }

        let limits = limit_rows(row);
        if !limits.is_empty() {
            out.push(Section {
                id: "limits",
                title: "LIMITS".into(),
                kind: SectionKind::Meters,
                rows: limits,
                group: None,
            });
        }
    }

    // A provider can have one without the other: a plan sells a window and a
    // reader prices its transcripts, while a prepaid provider sells a balance
    // and writes nothing to read.
    let cost_rows = cost_rows(row.cost.as_ref(), row.credits, row.credit_limit.as_ref());
    if !cost_rows.is_empty() {
        out.push(Section {
            id: "cost",
            title: "COST".into(),
            kind: SectionKind::Rows,
            rows: cost_rows,
            group: None,
        });
    }

    if let Some(cost) = row.cost.as_ref() {
        let days = day_rows(cost);
        if !days.is_empty() {
            out.push(Section {
                id: "tokens_by_day",
                title: "TOKENS BY DAY".into(),
                kind: SectionKind::Bars,
                rows: days,
                group: None,
            });
        }

        let models = model_rows(cost);
        if !models.is_empty() {
            out.push(Section {
                id: "tokens_by_model",
                // The cost layer is scoped to the calendar month. A bare
                // "Tokens by model" next to a panel counting all-time is worse
                // than a longer heading.
                title: "TOKENS BY MODEL · THIS MONTH".into(),
                kind: SectionKind::Bars,
                rows: models,
                group: None,
            });
        }

        let devices = device_rows(cost);
        if !devices.is_empty() {
            out.push(Section {
                id: "tokens_by_device",
                title: "TOKENS BY DEVICE · THIS MONTH".into(),
                kind: SectionKind::Bars,
                rows: devices,
                group: None,
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
    let now_ms = crate::now_ms();
    cost.by_device
        .iter()
        .map(|device| {
            let mut r = PanelRow::new(device.label.clone(), format_tokens(device.tokens));
            // A `Bars` row draws label, bar, value and suffix - never a badge,
            // and on waybar and GNOME never a tooltip either. A marker put
            // anywhere else is invisible on every surface, so it rides here.
            r.suffix = match () {
                _ if device.partial => format!("{} · partial", money(device.usd)),
                _ if !device.is_local => {
                    format!(
                        "{} · {}",
                        money(device.usd),
                        ago(device.updated_at_ms, now_ms)
                    )
                }
                _ => money(device.usd),
            };
            r.fraction = Some(if max > 0 {
                device.tokens as f64 / max as f64
            } else {
                0.0
            });
            r.emphasized = device.is_local;
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

/// One line of a bar icon's hover summary: a label, the figure beside it, and
/// the tier that figure sits in.
///
/// A frontend maps [`Tone`] onto its own palette and does its own escaping; it
/// never picks which lines there are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BarTooltipLine {
    pub label: String,
    pub value: String,
    pub tone: Tone,
}

/// What a bar icon says on hover.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BarTooltip {
    /// The provider, for the frontends whose tooltip has a heading slot.
    /// The ones that do not put it on the first line themselves.
    pub title: String,
    pub lines: Vec<BarTooltipLine>,
}

/// The hover summary behind a bar icon: every limit window with its tier, then
/// today's spend.
///
/// The icon itself is chrome - a plasmoid's compact representation, a St label,
/// a `WidgetButton`, a tray icon - but what it says on hover is content, and it
/// was the last piece of content no frontend agreed on. Plasma built this in
/// QML, the tray re-derived `session_used` / `weekly_used` in Rust and phrased
/// it differently, and GNOME and the Quickshell widget said nothing at all.
///
/// Read off [`panel_spec`] rather than off the row, so the summary can never
/// name a window the panel under it does not draw.
pub fn bar_tooltip(row: &ProviderRow, options: &PanelConfig) -> BarTooltip {
    let sections = panel_spec(row, options);
    let mut lines = Vec::new();

    let line = |r: &PanelRow, tone: Tone| BarTooltipLine {
        label: r.label.clone(),
        value: r.value.clone(),
        tone,
    };

    // With several credentials, the first group is the active one: the plan
    // being spent from is what a glance is for, and the combined figures
    // follow it.
    let live =
        row.credentials.is_empty() || row.credential.as_ref().and_then(|c| c.active) == Some(true);
    let first = sections.iter().find(|s| s.id == "limits").filter(|_| live);
    if let Some(limits) = first {
        lines.extend(limits.rows.iter().map(|r| line(r, r.tone)));
    }
    if let Some(plans) = sections.iter().find(|s| s.id == "plans") {
        lines.extend(plans.rows.iter().map(|r| BarTooltipLine {
            label: format!("{} · all plans", r.label),
            ..line(r, r.tone)
        }));
    }
    // One money line, not the whole cost section: a hover is a glance, and the
    // rest of it is one click away in the panel. The first row is the section's
    // headline figure either way - today's spend for a provider whose
    // transcripts are read, and the balance for a prepaid one, which has no
    // spend to report and would otherwise hover with no money on it at all.
    // Untinted: a spend figure has no threshold to tint against.
    if let Some(money) = sections
        .iter()
        .find(|s| s.id == "cost")
        .and_then(|s| s.rows.first())
    {
        lines.push(line(money, Tone::Normal));
    }

    let provider = crate::provider_label(&row.provider);
    BarTooltip {
        title: match first.and_then(|s| s.group.as_deref()) {
            Some(group) => format!("{provider} · {group}"),
            None => provider.to_string(),
        },
        lines,
    }
}

/// What a refresh control says on hover: when the figures it offers to replace
/// arrived.
///
/// A refresh button is worth pressing only in proportion to how old the panel
/// under it already is, and every frontend used to answer that on its own terms
/// or not at all - the TUI header measured the *process's* last fetch, which
/// reads "just now" over a snapshot ten minutes old, because serving the cache
/// is still a refresh as far as the process is concerned. The instant here is
/// the payload's own, so it says when the numbers arrived rather than when
/// something last asked for them.
pub fn refresh_hint(updated_iso: Option<&str>, now_ms: i64) -> String {
    let Some(at) = updated_iso.and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
    else {
        return "Last refresh unknown".to_string();
    };
    let at = at.with_timezone(&chrono::Local);
    // The clock alone answers "when" for anything today; past midnight it needs
    // the date, or "14:32" is a time on an unnamed day.
    let same_day = chrono::DateTime::from_timestamp_millis(now_ms)
        .is_some_and(|now| now.with_timezone(&chrono::Local).date_naive() == at.date_naive());
    let stamp = if same_day {
        at.format("%H:%M")
    } else {
        at.format("%-d %b %H:%M")
    };
    format!(
        "Last refreshed {} · {stamp}",
        ago(at.timestamp_millis(), now_ms)
    )
}

/// Relative time for a device row, for `--sync-status` and for the TUI.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let seconds = ((now_ms - then_ms) / 1000).max(0);
    match seconds {
        0..=89 => "just now".to_string(),
        90..=5399 => format!("{}m ago", seconds / 60),
        5400..=172_799 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Why the figures below are frozen, when they are.
///
/// A stale row is last-good cache served after a failed fetch, and the failure
/// that caused it is dropped from the error list so the bar does not also show
/// it. Without this section a panel says `stale` and nothing else, which is the
/// same thing whether the network blipped once or a credential expired weeks
/// ago and no fetch has succeeded since.
fn status_rows(row: &ProviderRow, credential: Option<&str>) -> Vec<PanelRow> {
    if !row.stale {
        return Vec::new();
    }
    let age = row
        .updated_iso
        .as_deref()
        .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
        .map(|at| ago(at.timestamp_millis(), crate::now_ms()))
        .unwrap_or_default();

    let reason = row
        .stale_reason
        .as_deref()
        .filter(|r| !r.is_empty())
        .unwrap_or("the last live fetch failed");

    let label = match credential {
        Some(name) => format!("Stale · {name}"),
        None => "Stale".to_string(),
    };
    let mut r = PanelRow::new(label, age);
    r.badge = ellipsize(reason, 72);
    r.badge_tone = Tone::Warn;
    r.tooltip = format!("Showing the last figures that arrived.\n{reason}");
    vec![r]
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// One window a row reports, before it is drawn.
struct Window<'a> {
    label: &'a str,
    used: u8,
    reset: &'a str,
    resets_at: Option<&'a str>,
    pace: Option<&'a crate::UsagePace>,
}

/// Every window a row has a figure for, in panel order. A window the provider
/// does not report has no meter, rather than a permanently empty one.
fn windows(row: &ProviderRow) -> Vec<Window<'_>> {
    let (session, weekly, tertiary) = crate::window_labels(&row.provider);
    let fixed = [
        (
            session,
            row.session_used,
            row.session_reset.as_str(),
            row.session_resets_at.as_deref(),
            row.session_pace.as_ref(),
        ),
        (
            weekly,
            row.weekly_used,
            row.weekly_reset.as_str(),
            row.weekly_resets_at.as_deref(),
            row.weekly_pace.as_ref(),
        ),
        (
            tertiary,
            row.tertiary_used,
            row.tertiary_reset.as_str(),
            row.tertiary_resets_at.as_deref(),
            None,
        ),
    ];
    // A slot the provider exposes but reports nothing in is a permanently
    // empty meter. Only the waybar tooltip used to keep them, to hold its
    // line count steady; it now shares this list, so they go everywhere.
    let extra = row
        .extra_windows
        .iter()
        .filter(|e| !e.placeholder)
        .map(|e| {
            (
                e.title.as_str(),
                e.used,
                e.reset.as_str(),
                e.resets_at.as_deref(),
                e.pace.as_ref(),
            )
        });
    fixed
        .into_iter()
        .chain(extra)
        .filter_map(|(label, used, reset, resets_at, pace)| {
            Some(Window {
                label,
                used: used?,
                reset,
                resets_at,
                pace,
            })
        })
        .collect()
}

/// `Resets in 2h`, or what to say when there is no reset to count to.
///
/// A window at 0% with no reset time has nowhere to reset to because nothing
/// has started it. Say so here rather than in one frontend, or the other four
/// render the line blank. Above 0% the same missing reset means the opposite -
/// the window is counting and the provider is not saying when it ends - and
/// "not started" beside a full bar is the sentence that reads as broken.
fn reset_footnote(used: u8, reset: &str) -> String {
    if reset == "—" || reset.is_empty() {
        if used == 0 {
            "not started".to_string()
        } else {
            String::new()
        }
    } else {
        format!("Resets {reset}")
    }
}

fn limit_rows(row: &ProviderRow) -> Vec<PanelRow> {
    windows(row)
        .into_iter()
        .map(|w| {
            let mut r = PanelRow::new(w.label, format!("{}%", w.used));
            r.fraction = Some(f64::from(w.used) / 100.0);
            r.tone = Tone::for_percent(w.used);
            r.footnote = reset_footnote(w.used, w.reset);
            if let Some(pace) = w.pace {
                r.badge = pace.badge();
                r.badge_tone = Tone::for_pace(pace);
            }
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// What a credential is called in its group's title: the store name, or what
/// a live login no stored credential matches is.
fn credential_name(row: &ProviderRow) -> String {
    row.credential
        .as_ref()
        .and_then(|c| c.name.clone())
        .unwrap_or_else(|| "live login".to_string())
}

/// A credential's weight in the combined figure, or `None` when it is left
/// out. Only a credential that was asked counts; weighted, its plan also needs
/// a known weight, and absolute, every plan weighs the same.
fn counted(row: &ProviderRow, total: PlansTotal) -> Option<f64> {
    let credential = row.credential.as_ref()?;
    credential.state.is_none().then_some(())?;
    match total {
        PlansTotal::Weighted => credential.plan_weight.filter(|w| w.is_finite() && *w > 0.0),
        PlansTotal::Absolute => Some(1.0),
    }
}

/// The sections a provider with several credentials draws in place of its
/// one `status` and `limits`: every stale group's reason, the combined
/// header, then one group per credential, active first.
///
/// With `active_credential_only`, the groups shrink to the active one while
/// the header still adds every credential up, so the header has to say how
/// many it covers - or the active group does, when there is no header. Only a
/// healthy group is ever dropped: one that is stale, was not asked or is out
/// of the total has something to say that its numbers alone do not, and the
/// status section keeps every group's reason.
fn credential_sections(groups: &[ProviderRow], options: &PanelConfig) -> Vec<Section> {
    let mut out = Vec::new();

    let plans = plan_rows(groups, options);
    let has_total = !plans.is_empty();
    let has_active = groups
        .iter()
        .any(|g| g.credential.as_ref().and_then(|c| c.active) == Some(true));
    let droppable = |g: &ProviderRow| {
        let credential = g.credential.clone().unwrap_or_default();
        credential.active != Some(true)
            && credential.state.is_none()
            && !g.stale
            && (!has_total || counted(g, options.plans_total).is_some())
    };
    let shown: Vec<&ProviderRow> = groups
        .iter()
        .filter(|g| !(options.active_credential_only && has_active && droppable(g)))
        .collect();
    let hidden = groups.len() - shown.len();

    let status: Vec<PanelRow> = groups
        .iter()
        .flat_map(|g| status_rows(g, Some(&credential_name(g))))
        .collect();
    if !status.is_empty() {
        out.push(Section {
            id: "status",
            title: "STATUS".into(),
            kind: SectionKind::Rows,
            rows: status,
            group: None,
        });
    }

    if has_total {
        out.push(Section {
            id: "plans",
            title: if hidden > 0 {
                format!("ALL PLANS · {} credentials", groups.len())
            } else {
                "ALL PLANS".into()
            },
            kind: SectionKind::Meters,
            rows: plans,
            group: None,
        });
    }

    for g in &shown {
        let credential = g.credential.clone().unwrap_or_default();
        let name = credential_name(g);
        let mut title = vec![name.clone()];
        title.extend(credential.label.clone());
        title.extend(g.plan_label.clone().filter(|p| !p.is_empty()));
        if credential.active == Some(true) {
            title.push("active".to_string());
        }
        if has_total && credential.state.is_none() && counted(g, options.plans_total).is_none() {
            title.push("not in total".to_string());
        }
        if hidden > 0 && !has_total && credential.active == Some(true) {
            title.push(format!("{} of {} shown", shown.len(), groups.len()));
        }
        let (kind, rows) = match credential.state {
            Some(state) => (SectionKind::Rows, vec![state_row(state)]),
            None => {
                let rows = limit_rows(g);
                if rows.is_empty() {
                    (
                        SectionKind::Rows,
                        vec![PanelRow::new("Limits", "none reported")],
                    )
                } else {
                    (SectionKind::Meters, rows)
                }
            }
        };
        out.push(Section {
            id: "limits",
            title: title.join(" · "),
            kind,
            rows,
            group: Some(name),
        });
    }
    out
}

/// The one line a credential that was not asked about draws.
fn state_row(state: crate::CredentialState) -> PanelRow {
    use crate::CredentialState;
    let (label, badge, tooltip) = match state {
        CredentialState::Expired => (
            "Expired",
            "remuda refreshes it",
            "Its access token has expired, so it was not asked about. remuda's timer refreshes stored credentials every 30 minutes; `remuda refresh` does it now.",
        ),
        CredentialState::Unverified => (
            "Unverified",
            "remuda re-identifies it",
            "Its sidecar was written for other tokens, so whose they are is unknown. remuda re-identifies it on its next run.",
        ),
        CredentialState::Other => (
            "Unknown state",
            "",
            "A newer build wrote a state this one cannot draw.",
        ),
    };
    let mut r = PanelRow::new(label, "not asked");
    r.badge = badge.to_string();
    r.badge_tone = Tone::Warn;
    r.tooltip = tooltip.to_string();
    r
}

/// The combined header: one meter per window at least two counted
/// credentials report.
///
/// Weighted, it counts in units of the largest plan, so a Max 20x and a Pro
/// both at 100% read "105% of 105%": each contributes `used × weight ÷
/// largest`, and the capacity is the same sum at 100%. Absolute, every plan
/// weighs 1 and the largest is 1 too, so five plans read "409% of 500%". The
/// bar fills to the pooled fraction, `Σ used × weight ÷ Σ weight`, so an idle
/// Pro cannot make two busy plans look free; split, each credential gets the
/// stretch of it its weight buys. The weights are the nominal multipliers the
/// plans are sold with, so the weighted figure is an estimate and says so.
fn plan_rows(groups: &[ProviderRow], options: &PanelConfig) -> Vec<PanelRow> {
    struct Share<'a> {
        name: String,
        weight: f64,
        window: Window<'a>,
    }
    let total_mode = options.plans_total;
    let mut by_label: Vec<(&str, Vec<Share<'_>>)> = Vec::new();
    for g in groups {
        let Some(weight) = counted(g, total_mode) else {
            continue;
        };
        for window in windows(g) {
            let share = Share {
                name: credential_name(g),
                weight,
                window,
            };
            match by_label.iter_mut().find(|(l, _)| *l == share.window.label) {
                Some((_, shares)) => shares.push(share),
                None => by_label.push((share.window.label, vec![share])),
            }
        }
    }
    let left_out: Vec<String> = groups
        .iter()
        .filter(|g| {
            g.credential.as_ref().is_some_and(|c| c.state.is_none())
                && counted(g, total_mode).is_none()
        })
        .map(credential_name)
        .collect();

    by_label
        .into_iter()
        .filter(|(_, shares)| shares.len() >= 2)
        .map(|(label, shares)| {
            let largest = shares.iter().map(|s| s.weight).fold(0.0_f64, f64::max);
            let total: f64 = shares.iter().map(|s| s.weight).sum();
            let weighted: f64 = shares
                .iter()
                .map(|s| f64::from(s.window.used) * s.weight)
                .sum();
            let pooled = weighted / total;
            let mut r = PanelRow::new(
                label,
                format!(
                    "{:.0}% of {:.0}%",
                    weighted / largest,
                    total / largest * 100.0
                ),
            );
            r.fraction = Some((pooled / 100.0).clamp(0.0, 1.0));
            r.tone = Tone::for_percent(crate::pct_u8(pooled));
            if options.split_bars {
                let weights: Vec<f64> = shares.iter().map(|s| s.weight).collect();
                r.segments = shares
                    .iter()
                    .zip(segment_widths(&weights))
                    .map(|(s, width)| Segment {
                        width,
                        fraction: (f64::from(s.window.used) / 100.0).clamp(0.0, 1.0),
                        tone: Tone::for_percent(s.window.used),
                    })
                    .collect();
            }
            let earliest = shares
                .iter()
                .filter_map(|s| {
                    let at = chrono::DateTime::parse_from_rfc3339(s.window.resets_at?).ok()?;
                    Some((at, s))
                })
                .min_by_key(|(at, _)| *at)
                .map(|(_, s)| s);
            r.footnote = earliest
                .map(|s| reset_footnote(s.window.used, s.window.reset))
                .unwrap_or_default();
            let mut lines: Vec<String> = shares
                .iter()
                .map(|s| {
                    let reset = match reset_footnote(s.window.used, s.window.reset) {
                        f if f.is_empty() => String::new(),
                        f => format!(" · {}", f.to_lowercase()),
                    };
                    match total_mode {
                        PlansTotal::Weighted => {
                            format!("{}  {}% × {}{reset}", s.name, s.window.used, s.weight)
                        }
                        PlansTotal::Absolute => format!("{}  {}%{reset}", s.name, s.window.used),
                    }
                })
                .collect();
            match total_mode {
                PlansTotal::Weighted => {
                    r.badge = "estimate".to_string();
                    r.badge_tone = Tone::Dim;
                    lines.push(
                        "Weighted by each plan's nominal multiplier. The real limits are not published, so this is an estimate."
                            .to_string(),
                    );
                }
                PlansTotal::Absolute => {
                    lines.push("Every plan counts as 100%, whatever its size.".to_string());
                }
            }
            if !left_out.is_empty() {
                lines.push(format!("Not in the total: {}", left_out.join(", ")));
            }
            r.tooltip = lines.join("\n");
            r
        })
        .collect()
}

/// A split bar's narrowest segment, as a share of the bar.
const MIN_SEGMENT_WIDTH: f64 = 0.1;

/// Each weight's share of a split bar, none under [`MIN_SEGMENT_WIDTH`]. A
/// Max 20x beside two Pro seats would otherwise draw them as slivers next to
/// one bar that reads as pooled. Segments held at the minimum are taken out
/// and the rest is shared again by weight, until none falls under it.
fn segment_widths(weights: &[f64]) -> Vec<f64> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    if MIN_SEGMENT_WIDTH * n as f64 >= 1.0 {
        return vec![1.0 / n as f64; n];
    }
    let mut pinned = vec![false; n];
    loop {
        let free = 1.0 - MIN_SEGMENT_WIDTH * pinned.iter().filter(|p| **p).count() as f64;
        let weight: f64 = weights
            .iter()
            .zip(&pinned)
            .filter(|(_, p)| !**p)
            .map(|(w, _)| w)
            .sum();
        let widths: Vec<f64> = weights
            .iter()
            .zip(&pinned)
            .map(|(w, p)| {
                if *p {
                    MIN_SEGMENT_WIDTH
                } else {
                    w / weight * free
                }
            })
            .collect();
        let mut moved = false;
        for (i, width) in widths.iter().enumerate() {
            if !pinned[i] && *width < MIN_SEGMENT_WIDTH {
                pinned[i] = true;
                moved = true;
            }
        }
        if !moved {
            return widths;
        }
    }
}

// ---------------------------------------------------------------------------
// Cost
// ---------------------------------------------------------------------------

fn cost_rows(
    cost: Option<&CostInfo>,
    credits: Option<f64>,
    credit_limit: Option<&CreditLimit>,
) -> Vec<PanelRow> {
    let mut out = Vec::new();
    if let Some(cost) = cost {
        out.extend(spend_rows(cost));
    }

    // Under the spend it is being drawn down by. A credit balance is what a
    // provider selling credits has instead of a window, so for such a provider
    // this is the only row in the section; for Codex it sits below a month's spend.
    if let Some(remaining) = credits {
        out.push(PanelRow::new("Credits", balance(remaining)));
    }

    // A cap drawing on that balance goes under it, because it is the narrower
    // of the two and reading it first would suggest the account has only that
    // much. It is the one figure in this section with a threshold to tint
    // against - a cap is exhaustible where a month's spend is not - so it is
    // also the only one that carries a tone.
    if let Some(cap) = credit_limit {
        let title = credit_limit_title(cap.kind);
        let mut r = PanelRow::new(title, balance(cap.remaining()));
        r.suffix = format!("of {}", balance(cap.limit));
        if let Some(percent) = cap.used_percent() {
            r.badge = format!("{percent}% used");
            r.badge_tone = Tone::for_percent(percent);
        }
        if let Some(resets) = cap.resets.as_deref() {
            r.tooltip = format!("{title} resets {resets}");
        }
        out.push(r);
    }

    // Sync stays last: it is the section's status line, not one of its figures.
    if let Some(note) = cost.and_then(|c| c.sync_note.as_ref()) {
        out.push(sync_row(note));
    }

    out
}

/// What a cap is called. The kind is the provider's; the words are ours.
fn credit_limit_title(kind: CreditLimitKind) -> &'static str {
    match kind {
        CreditLimitKind::Key => "Key limit",
        CreditLimitKind::Subscription => "Plan limit",
        CreditLimitKind::OnDemand => "On-demand cap",
        // A kind this build does not know still draws, under a word that is
        // true of every cap there is.
        CreditLimitKind::Other => "Spend limit",
    }
}

fn sync_row(note: &SyncNote) -> PanelRow {
    let mut r = PanelRow::new(
        "Sync",
        match note.devices {
            1 => "1 device".to_string(),
            n => format!("{n} devices"),
        },
    );
    r.badge = note.headline.clone();
    r.badge_tone = note.tone;
    // A transport error can be a paragraph. Rows renderers print the suffix
    // inline on surfaces with no wrapping, so the line is capped and the whole
    // sentence kept for the tooltip.
    r.suffix = ellipsize(&note.detail, 72);
    r.tooltip = if note.detail.is_empty() {
        "Cost and token figures cover every machine in the fleet".to_string()
    } else {
        note.detail.clone()
    };
    r
}

fn spend_rows(cost: &CostInfo) -> Vec<PanelRow> {
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
            if let Some(split) = model_breakdown(&day.by_model) {
                r.tooltip.push_str(&split);
            }
            if let Some(split) = device_breakdown(&day.by_device) {
                r.tooltip.push_str(&split);
            }
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

/// One row of a tooltip sub-table: a label, its tokens, its spend, and a note
/// hung off the end (empty for most of them).
type BreakdownRow = (String, String, String, &'static str);

/// A titled sub-table under the figure it splits, or None when there is nothing
/// to split it by.
fn breakdown(title: &str, rows: &[BreakdownRow]) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    // Columns, not a sentence: the tooltips render in a monospace face, and
    // unpadded fields only line up when the label and token widths happen to
    // cancel out, which reads as broken on the day they stop.
    let width = |f: fn(&BreakdownRow) -> usize| rows.iter().map(f).max().unwrap_or(0);
    let (lw, tw, mw) = (
        width(|r| r.0.chars().count()),
        width(|r| r.1.chars().count()),
        width(|r| r.2.chars().count()),
    );

    let mut out = format!("\n\n{title}");
    for (label, tokens, usd, note) in rows {
        out.push_str(&format!(
            "\n{label:<lw$}  {tokens:>tw$}  ·  {usd:>mw$}{note}"
        ));
    }
    Some(out)
}

/// Which machines a day or a model came from.
///
/// `by_device` is empty unless the fleet has more than one machine in it, and
/// that decision lives in `fetch::attach_fleet` rather than here: a split of one
/// restates the row it hangs off.
fn device_breakdown(devices: &[DeviceCost]) -> Option<String> {
    let rows: Vec<BreakdownRow> = devices
        .iter()
        .map(|d| {
            (
                d.label.clone(),
                format_tokens(d.tokens),
                money(d.usd),
                if d.partial { "  · partial" } else { "" },
            )
        })
        .collect();
    breakdown("By device", &rows)
}

/// Which models spent a day, under the day it splits. Capped and ordered by
/// `DayModelCost::top` before it ever reaches here.
fn model_breakdown(models: &[DayModelCost]) -> Option<String> {
    let rows: Vec<BreakdownRow> = models
        .iter()
        .map(|m| {
            (
                model_label(&m.model),
                format_tokens(m.tokens),
                money(m.usd),
                "",
            )
        })
        .collect();
    breakdown("By model", &rows)
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
    let mut out = lines.join("\n");
    if let Some(split) = device_breakdown(&m.by_device) {
        out.push_str(&split);
    }
    out
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// `$1.23` under a hundred, `$312` above it - cents stop carrying information
/// once the figure is that large, and the extra digits push the value column
/// wide on a narrow panel.
/// Cut to `max` characters on a word boundary where there is one.
fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max - 1).collect();
    let trimmed = match cut.rsplit_once(' ') {
        Some((head, _)) if head.chars().count() >= max / 2 => head,
        _ => cut.trim_end(),
    };
    format!("{trimmed}…")
}

/// A prepaid balance, which keeps the cents [`money`] drops past a hundred
/// dollars. A month's spend is a magnitude, where the cents are noise; a
/// balance is what is left to spend it from, counting down to zero, and $100.49
/// rounded to $100 misstates it.
fn balance(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    format!("${value:.2}")
}

pub fn money(value: f64) -> String {
    if !value.is_finite() {
        return "-".to_string();
    }
    // Summing an empty iterator of `f64` gives **-0.0**: the standard library
    // folds from the IEEE additive identity, which has to be negative zero so
    // that adding it preserves the sign of everything else. A period with
    // nothing in it therefore formats as "$-0.00", which reads as a number
    // somebody got wrong. `-0.0 == 0.0` is true, so this catches both.
    let value = if value == 0.0 { 0.0 } else { value };
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

    fn panel_spec(row: &ProviderRow) -> Vec<Section> {
        super::panel_spec(row, &PanelConfig::default())
    }

    fn bar_tooltip(row: &ProviderRow) -> BarTooltip {
        super::bar_tooltip(row, &PanelConfig::default())
    }

    fn row() -> ProviderRow {
        ProviderRow {
            stale_reason: None,
            credit_limit: None,
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
            credits: None,
            source: "—".into(),
            updated: "just now".into(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: None,
            stale: false,
            credential: None,
            credentials: Vec::new(),
            session_resets_at: None,
            weekly_resets_at: None,
            tertiary_resets_at: None,
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
                    by_device: Vec::new(),
                },
                ModelCost {
                    model: "claude-opus-5".into(),
                    usd: 90.0,
                    tokens: 800,
                    input_tokens: 100,
                    output_tokens: 200,
                    cache_creation_tokens: 300,
                    cache_read_tokens: 200,
                    by_device: Vec::new(),
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
                    updated_at_ms: crate::now_ms(),
                    partial: false,
                    is_local: true,
                },
                DeviceCost {
                    device_id: "bbbb".into(),
                    label: "laptop".into(),
                    tokens: 500_000_000,
                    usd: 350.91,
                    updated_at_ms: crate::now_ms() - 7_200_000,
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
                    by_device: Vec::new(),
                    by_model: Vec::new(),
                },
                DayCost {
                    date: "2026-08-22".into(),
                    usd: 2.0,
                    tokens: 1000,
                    by_device: Vec::new(),
                    by_model: Vec::new(),
                },
            ],
        }
    }

    #[test]
    fn sections_follow_canonical_order() {
        let mut r = row();
        r.cost = Some(cost());
        r.stale = true;
        r.stale_reason = Some("Claude token expired".into());
        let ids: Vec<&str> = panel_spec(&r).iter().map(|s| s.id).collect();
        // `plans` is the one section a provider with one credential never has.
        let single: Vec<&str> = SECTION_IDS
            .iter()
            .copied()
            .filter(|id| *id != "plans")
            .collect();
        assert_eq!(ids, single);
    }

    // ------------------------------------------------------------------------
    // Several credentials
    // ------------------------------------------------------------------------

    fn credential(
        name: &str,
        active: bool,
        weight: Option<u32>,
        session: u8,
        weekly: u8,
    ) -> ProviderRow {
        let mut r = row();
        r.session_used = Some(session);
        r.weekly_used = Some(weekly);
        r.plan_label = Some(format!("{name} plan"));
        r.credential = Some(crate::CredentialInfo {
            name: Some(name.into()),
            active: Some(active),
            plan_weight: weight.map(f64::from),
            ..Default::default()
        });
        r
    }

    fn grouped(groups: Vec<ProviderRow>) -> ProviderRow {
        let mut top = groups[0].clone();
        top.cost = Some(cost());
        top.credentials = groups;
        top
    }

    fn section<'a>(spec: &'a [Section], id: &str, group: Option<&str>) -> &'a Section {
        spec.iter()
            .find(|s| s.id == id && s.group.as_deref() == group)
            .unwrap_or_else(|| panic!("no {id} section for {group:?}"))
    }

    /// The ADR's two worked examples, in units of the largest plan.
    #[test]
    fn the_combined_header_weighs_each_plan_by_its_multiplier() {
        let spec = panel_spec(&grouped(vec![
            credential("work", true, Some(20), 100, 50),
            credential("perso", false, Some(1), 100, 100),
        ]));
        let plans = section(&spec, "plans", None);
        let session = &plans.rows[0];
        assert_eq!(session.value, "105% of 105%");
        assert_eq!(session.fraction, Some(1.0));
        assert_eq!(session.badge, "estimate");

        let spec = panel_spec(&grouped(vec![
            credential("big", true, Some(20), 50, 0),
            credential("small", false, Some(5), 100, 0),
        ]));
        let session = &section(&spec, "plans", None).rows[0];
        assert_eq!(session.value, "75% of 125%");
        // Pooled: an idle small plan cannot make a busy big one look free.
        assert!((session.fraction.unwrap() - 0.6).abs() < 1e-9);
        assert_eq!(session.tone, Tone::Warn);
    }

    #[test]
    fn several_credentials_draw_a_header_then_a_group_each_active_first() {
        let mut expired = credential("old", false, Some(5), 0, 0);
        expired.credential.as_mut().unwrap().state = Some(crate::CredentialState::Expired);
        let mut stale = credential("perso", false, Some(1), 40, 40);
        stale.stale = true;
        stale.stale_reason = Some("Claude rate-limited - try again shortly".into());
        let spec = panel_spec(&grouped(vec![
            credential("work", true, Some(20), 30, 10),
            stale,
            credential("corp", false, None, 90, 90),
            expired,
        ]));

        let ids: Vec<(&str, Option<&str>)> =
            spec.iter().map(|s| (s.id, s.group.as_deref())).collect();
        assert_eq!(
            &ids[..6],
            [
                ("status", None),
                ("plans", None),
                ("limits", Some("work")),
                ("limits", Some("perso")),
                ("limits", Some("corp")),
                ("limits", Some("old")),
            ]
        );
        assert_eq!(
            section(&spec, "status", None).rows[0].label,
            "Stale · perso"
        );
        assert_eq!(
            section(&spec, "limits", Some("work")).title,
            "work · work plan · active"
        );
        // Unweighted, so out of the total, and the title says so.
        assert_eq!(
            section(&spec, "limits", Some("corp")).title,
            "corp · corp plan · not in total"
        );
        let plans = section(&spec, "plans", None);
        assert!(
            plans.rows[0].tooltip.contains("Not in the total: corp"),
            "{}",
            plans.rows[0].tooltip
        );
        // Not asked, so a line saying why rather than meters.
        let old = section(&spec, "limits", Some("old"));
        assert_eq!(old.kind, SectionKind::Rows);
        assert_eq!(old.rows[0].label, "Expired");
        assert!(!old.title.contains("not in total"));
        // Cost stays provider-scoped, once.
        assert_eq!(spec.iter().filter(|s| s.id == "cost").count(), 1);
    }

    #[test]
    fn the_header_resets_when_capacity_first_comes_back() {
        let mut work = credential("work", true, Some(20), 50, 0);
        work.session_reset = "in 3h".into();
        work.session_resets_at = Some("2099-01-01T15:00:00Z".into());
        let mut perso = credential("perso", false, Some(1), 50, 0);
        perso.session_reset = "in 1h".into();
        perso.session_resets_at = Some("2099-01-01T13:00:00Z".into());
        let spec = panel_spec(&grouped(vec![work, perso]));
        assert_eq!(
            section(&spec, "plans", None).rows[0].footnote,
            "Resets in 1h"
        );
    }

    /// No weights, no header - and no "not in total" beside a total that is
    /// not there.
    #[test]
    fn credentials_without_weights_draw_groups_and_no_header() {
        let spec = panel_spec(&grouped(vec![
            credential("work", true, None, 30, 10),
            credential("perso", false, None, 40, 40),
        ]));
        assert!(spec.iter().all(|s| s.id != "plans"));
        assert!(spec.iter().all(|s| !s.title.contains("not in total")));
        assert_eq!(spec.iter().filter(|s| s.id == "limits").count(), 2);
    }

    fn five_plans() -> ProviderRow {
        grouped(vec![
            credential("work", true, Some(20), 90, 10),
            credential("perso", false, Some(5), 100, 40),
            credential("corp", false, None, 50, 80),
        ])
    }

    #[test]
    fn absolute_counts_every_plan_as_100_percent_and_leaves_none_out() {
        let options = PanelConfig {
            plans_total: PlansTotal::Absolute,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&five_plans(), &options);
        let session = &section(&spec, "plans", None).rows[0];
        assert_eq!(session.value, "240% of 300%");
        assert!((session.fraction.unwrap() - 0.8).abs() < 1e-9);
        assert_eq!(session.badge, "");
        assert!(!session.tooltip.contains("Not in the total"));
        assert!(spec.iter().all(|s| !s.title.contains("not in total")));
    }

    /// A Team seat weighs 1.25 Pros: beside a Max 20x, two spent Standard
    /// seats add 12.5% to the total, and each says so in the tooltip.
    #[test]
    fn a_team_seat_weighs_a_fraction_of_a_pro() {
        let seat = |name: &str| {
            let mut g = credential(name, false, Some(1), 100, 100);
            g.credential.as_mut().unwrap().plan_weight = Some(1.25);
            g
        };
        let groups = grouped(vec![
            credential("perso", true, Some(20), 0, 0),
            seat("axeo"),
            seat("idc"),
        ]);
        let spec = super::panel_spec(&groups, &PanelConfig::default());
        let session = &section(&spec, "plans", None).rows[0];
        assert_eq!(session.value, "12% of 112%");
        assert!(
            session.tooltip.contains("axeo  100% × 1.25"),
            "{}",
            session.tooltip
        );
    }

    /// Plans with no known weight (Enterprise, ChatGPT Free) draw a header
    /// only under the absolute total.
    #[test]
    fn absolute_draws_a_header_for_plans_with_no_known_weight() {
        let groups = grouped(vec![
            credential("work", true, None, 30, 10),
            credential("perso", false, None, 40, 40),
        ]);
        let options = PanelConfig {
            plans_total: PlansTotal::Absolute,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&groups, &options);
        assert_eq!(section(&spec, "plans", None).rows[0].value, "70% of 200%");
    }

    #[test]
    fn a_split_bar_gives_each_plan_the_stretch_its_weight_buys() {
        let spec = panel_spec(&five_plans());
        let segments = &section(&spec, "plans", None).rows[0].segments;
        let widths: Vec<f64> = segments.iter().map(|s| s.width).collect();
        assert_eq!(widths, [0.8, 0.2]);
        let fills: Vec<f64> = segments.iter().map(|s| s.fraction).collect();
        assert_eq!(fills, [0.9, 1.0]);
        assert_eq!(segments[0].tone, Tone::Critical);

        let options = PanelConfig {
            plans_total: PlansTotal::Absolute,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&five_plans(), &options);
        let segments = &section(&spec, "plans", None).rows[0].segments;
        assert_eq!(segments.len(), 3);
        assert!(segments.iter().all(|s| (s.width - 1.0 / 3.0).abs() < 1e-9));

        let options = PanelConfig {
            split_bars: false,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&five_plans(), &options);
        assert!(section(&spec, "plans", None).rows[0].segments.is_empty());
    }

    #[test]
    fn a_split_bar_keeps_a_small_plan_wide_enough_to_read() {
        let close = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9);
        let widths = segment_widths(&[20.0, 1.0, 1.0]);
        assert!(close(&widths, &[0.8, 0.1, 0.1]), "{widths:?}");
        let widths = segment_widths(&[20.0, 5.0, 1.0]);
        assert!(close(&widths, &[0.72, 0.18, 0.1]), "{widths:?}");
        assert!(close(&segment_widths(&[1.0; 12]), &[1.0 / 12.0; 12]));
        assert!(segment_widths(&[]).is_empty());

        let spec = panel_spec(&grouped(vec![
            credential("work", true, Some(20), 30, 10),
            credential("a", false, Some(1), 40, 40),
            credential("b", false, Some(1), 40, 40),
        ]));
        let segments = &section(&spec, "plans", None).rows[0].segments;
        let total: f64 = segments.iter().map(|s| s.width).sum();
        assert!((total - 1.0).abs() < 1e-9);
        assert!(segments.iter().all(|s| s.width >= MIN_SEGMENT_WIDTH - 1e-9));
    }

    #[test]
    fn segment_cells_fill_the_bar_exactly_with_one_cell_at_least_each() {
        let seg = |width, fraction| Segment {
            width,
            fraction,
            tone: Tone::Good,
        };
        let total = |cells: &[(usize, usize)]| cells.iter().map(|c| c.0).sum::<usize>();
        let cells = segment_cells(&[seg(0.8, 0.5), seg(0.1, 1.0), seg(0.1, 0.0)], 10);
        assert_eq!(total(&cells) + 2, 10, "{cells:?}");
        assert_eq!(cells, [(6, 3), (1, 1), (1, 0)]);
        let thirds = vec![seg(1.0 / 3.0, 0.0); 3];
        assert_eq!(total(&segment_cells(&thirds, 40)) + 2, 40);
        assert_eq!(
            segment_cells(&[seg(0.75, 0.5), seg(0.25, 1.0)], 41),
            [(30, 15), (10, 10)]
        );
        // Too many to separate, and then too many for a cell each: the bar
        // still never runs past its slot.
        let six = vec![seg(1.0 / 6.0, 1.0); 6];
        assert!(!segment_separators(6, 10));
        assert_eq!(total(&segment_cells(&six, 10)), 10);
        assert!(segment_cells(&six, 10).iter().all(|c| c.0 >= 1));
        let many = vec![seg(1.0 / 12.0, 1.0); 12];
        assert_eq!(total(&segment_cells(&many, 10)), 10);
        assert!(segment_separators(3, 10));
        assert!(segment_cells(&[], 10).is_empty());
    }

    #[test]
    fn active_only_keeps_the_total_and_says_how_many_it_covers() {
        let mut stale = credential("perso", false, Some(5), 100, 40);
        stale.stale = true;
        stale.stale_reason = Some("Claude rate-limited - try again shortly".into());
        let groups = grouped(vec![
            credential("work", true, Some(20), 90, 10),
            stale,
            credential("corp", false, Some(1), 50, 80),
        ]);
        let options = PanelConfig {
            active_credential_only: true,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&groups, &options);
        let ids: Vec<(&str, Option<&str>)> =
            spec.iter().map(|s| (s.id, s.group.as_deref())).collect();
        // The stale one keeps its group and its reason: hiding it would leave
        // its old figures in the total with nothing saying why.
        assert_eq!(
            ids[..4],
            [
                ("status", None),
                ("plans", None),
                ("limits", Some("work")),
                ("limits", Some("perso")),
            ]
        );
        assert_eq!(spec.iter().filter(|s| s.id == "limits").count(), 2);
        assert_eq!(
            section(&spec, "status", None).rows[0].label,
            "Stale · perso"
        );
        let plans = section(&spec, "plans", None);
        assert_eq!(plans.title, "ALL PLANS · 3 credentials");
        assert_eq!(plans.rows[0].segments.len(), 3);

        let tip = super::bar_tooltip(&groups, &options);
        assert_eq!(tip.title, "Claude · work");
        let labels: Vec<&str> = tip.lines.iter().map(|l| l.label.as_str()).collect();
        assert!(labels.contains(&"Session · all plans"), "{labels:?}");
    }

    #[test]
    fn active_only_keeps_a_credential_that_was_not_asked_or_is_out_of_the_total() {
        let mut expired = credential("old", false, Some(5), 0, 0);
        expired.credential.as_mut().unwrap().state = Some(crate::CredentialState::Expired);
        let groups = grouped(vec![
            credential("work", true, Some(20), 30, 10),
            credential("perso", false, Some(1), 40, 40),
            credential("corp", false, None, 50, 50),
            expired,
        ]);
        let options = PanelConfig {
            active_credential_only: true,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&groups, &options);
        let groups: Vec<&str> = spec
            .iter()
            .filter(|s| s.id == "limits")
            .filter_map(|s| s.group.as_deref())
            .collect();
        assert_eq!(groups, ["work", "corp", "old"]);
        assert_eq!(
            section(&spec, "limits", Some("corp")).title,
            "corp · corp plan · not in total"
        );
    }

    #[test]
    fn active_only_with_no_header_puts_the_count_on_the_group() {
        let groups = grouped(vec![
            credential("work", true, None, 30, 10),
            credential("perso", false, None, 40, 40),
        ]);
        let options = PanelConfig {
            active_credential_only: true,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&groups, &options);
        let limits: Vec<&Section> = spec.iter().filter(|s| s.id == "limits").collect();
        assert_eq!(limits.len(), 1);
        assert_eq!(limits[0].title, "work · work plan · active · 1 of 2 shown");
    }

    /// With no credential marked active there is no one to keep, so all draw.
    #[test]
    fn active_only_with_no_active_credential_draws_them_all() {
        let groups = grouped(vec![
            credential("work", false, Some(20), 30, 10),
            credential("perso", false, Some(1), 40, 40),
        ]);
        let options = PanelConfig {
            active_credential_only: true,
            ..PanelConfig::default()
        };
        let spec = super::panel_spec(&groups, &options);
        assert_eq!(spec.iter().filter(|s| s.id == "limits").count(), 2);
        assert_eq!(section(&spec, "plans", None).title, "ALL PLANS");
    }

    #[test]
    fn the_bar_icon_names_the_active_credential_and_the_total() {
        let tip = bar_tooltip(&grouped(vec![
            credential("work", true, Some(20), 30, 10),
            credential("perso", false, Some(1), 40, 40),
        ]));
        assert_eq!(tip.title, "Claude · work");
        let labels: Vec<&str> = tip.lines.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels[0], "Session");
        assert!(labels.contains(&"Session · all plans"), "{labels:?}");
        assert!(!labels.iter().any(|l| l.contains("perso")));
    }

    #[test]
    fn a_stale_row_leads_with_why_and_a_fresh_one_has_no_status_section() {
        let mut r = row();
        r.stale = true;
        r.stale_reason = Some("Claude token expired - run `claude` to log in".into());
        let spec = panel_spec(&r);
        assert_eq!(spec[0].id, "status");
        assert_eq!(spec[0].kind, SectionKind::Rows);
        assert_eq!(spec[0].rows[0].label, "Stale");
        assert_eq!(
            spec[0].rows[0].badge,
            "Claude token expired - run `claude` to log in"
        );
        assert_eq!(spec[0].rows[0].badge_tone, Tone::Warn);

        // A restore written before the reason was recorded still says what it
        // knows rather than dropping the section.
        r.stale_reason = None;
        assert_eq!(
            panel_spec(&r)[0].rows[0].badge,
            "the last live fetch failed"
        );

        assert!(panel_spec(&row()).iter().all(|s| s.id != "status"));
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
        assert!(
            devices[1].suffix.ends_with("· partial"),
            "the marker has to be somewhere Bars draws: {:?}",
            devices[1].suffix
        );
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

    /// `every_panel_frontend_handles_every_section_kind` checks *kinds*, so it
    /// cannot see a row putting content in a field its kind never draws. Bars
    /// renderers read label, bar, value and suffix; a badge set here is
    /// invisible on all five surfaces.
    #[test]
    fn a_row_never_hides_content_in_a_field_its_kind_does_not_draw() {
        let mut r = row();
        r.cost = Some(cost());
        for section in panel_spec(&r).iter() {
            for row in &section.rows {
                // Per kind: the fields no frontend's delegate for that kind
                // reads. Filling one is content the user never sees.
                let undrawn: &[(&str, bool)] = match section.kind {
                    SectionKind::Meters => &[("suffix", !row.suffix.is_empty())],
                    SectionKind::Bars => &[
                        ("badge", !row.badge.is_empty()),
                        ("footnote", !row.footnote.is_empty()),
                    ],
                    SectionKind::Rows => &[
                        ("fraction", row.fraction.is_some()),
                        ("footnote", !row.footnote.is_empty()),
                    ],
                };
                for (field, filled) in undrawn {
                    assert!(
                        !filled,
                        "{}: `{}` fills `{field}`, which no {:?} renderer draws",
                        section.id, row.label, section.kind
                    );
                }
            }
        }
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

    /// A provider selling prepaid credits has a balance and no transcripts to
    /// read, so COST is the balance and nothing else. The waybar tooltip and
    /// the TUI each used to draw this themselves, below the panel, and the four
    /// desktop panels drew it nowhere.
    #[test]
    fn a_balance_alone_is_enough_for_a_cost_section() {
        let mut row = row();
        row.credits = Some(18.44);
        let spec = panel_spec(&row);
        let cost = spec.iter().find(|s| s.id == "cost").expect("cost section");
        assert_eq!(cost.kind, SectionKind::Rows);
        assert_eq!(cost.rows.len(), 1);
        assert_eq!(cost.rows[0].label, "Credits");
        assert_eq!(cost.rows[0].value, "$18.44");
    }

    /// `money` rounds past a hundred dollars, which is right for a month's
    /// spend and wrong for the money left on a prepaid plan.
    #[test]
    fn a_balance_keeps_the_cents_a_spend_figure_drops() {
        assert_eq!(balance(100.49), "$100.49");
        assert_eq!(money(100.49), "$100");
        assert_eq!(balance(4.5), "$4.50");
        assert_eq!(balance(f64::NAN), "-");
    }

    /// Under the spend it is being drawn down by, and above the sync status
    /// line, which is not one of the section's figures.
    #[test]
    fn a_balance_sits_below_the_spend_it_is_drawn_down_by() {
        let mut row = row();
        row.cost = Some(cost());
        row.credits = Some(18.44);
        let spec = panel_spec(&row);
        let labels: Vec<&str> = spec
            .iter()
            .find(|s| s.id == "cost")
            .expect("cost section")
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        let credits = labels
            .iter()
            .position(|l| *l == "Credits")
            .expect("credits");
        let month = labels
            .iter()
            .position(|l| *l == "This month")
            .expect("month");
        assert!(month < credits, "{labels:?}");
        if let Some(sync) = labels.iter().position(|l| *l == "Sync") {
            assert!(credits < sync, "{labels:?}");
        }
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
        no_reset.session_used = Some(0);
        assert_eq!(panel_spec(&no_reset)[0].rows[0].footnote, "not started");

        // Counting, but the provider is not saying when it ends. "not started"
        // under a full bar is the sentence that reads as broken.
        no_reset.session_used = Some(100);
        assert_eq!(panel_spec(&no_reset)[0].rows[0].footnote, "");
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
                resets_at: None,
            },
            ExtraWindowRow {
                title: "Fable only".into(),
                used: Some(9),
                reset: "in 4d".into(),
                pace: None,
                placeholder: false,
                resets_at: None,
            },
        ];
        let spec = panel_spec(&r);
        let labels: Vec<&str> = spec[0].rows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, vec!["Session", "Weekly (all)", "Fable only"]);
    }

    fn split() -> Vec<DeviceCost> {
        vec![
            DeviceCost {
                device_id: "aaaa".into(),
                label: "desktop".into(),
                tokens: 600,
                usd: 1.2,
                updated_at_ms: crate::now_ms(),
                partial: false,
                is_local: true,
            },
            DeviceCost {
                device_id: "bbbb".into(),
                label: "laptop".into(),
                tokens: 400,
                usd: 0.8,
                updated_at_ms: crate::now_ms(),
                partial: true,
                is_local: false,
            },
        ]
    }

    #[test]
    fn a_day_and_a_model_tooltip_carry_the_fleet_split() {
        let mut c = cost();
        c.weekly_history.last_mut().expect("a day").by_device = split();
        for model in c.monthly_models.iter_mut() {
            model.by_device = split();
        }
        let mut r = row();
        r.cost = Some(c);
        let spec = panel_spec(&r);

        let today = spec
            .iter()
            .find(|s| s.id == "tokens_by_day")
            .expect("days")
            .rows
            .last()
            .expect("today")
            .clone();
        assert!(today.tooltip.contains("By device"), "{}", today.tooltip);
        // Padded into columns: the shorter label, token and money strings are
        // widened to the longest of each, so the bullets and figures align in
        // the monospace faces the tooltips render in.
        assert!(
            today.tooltip.contains("desktop  600  ·  $1.20"),
            "{}",
            today.tooltip
        );
        // The marker the by-device rows already use, rather than a second
        // wording for the same thing.
        assert!(
            today.tooltip.contains("laptop   400  ·  $0.80  · partial"),
            "{}",
            today.tooltip
        );

        let model = spec
            .iter()
            .find(|s| s.id == "tokens_by_model")
            .expect("models")
            .rows[0]
            .clone();
        assert!(model.tooltip.contains("By device"), "{}", model.tooltip);
        assert!(model.tooltip.contains("desktop"), "{}", model.tooltip);
    }

    #[test]
    fn a_day_tooltip_names_the_models_that_spent_it() {
        let mut c = cost();
        let today = c.weekly_history.last_mut().expect("a day");
        today.by_device = split();
        today.by_model = DayModelCost::top(vec![
            DayModelCost {
                model: "claude-opus-5".into(),
                usd: 1.2,
                tokens: 600,
            },
            DayModelCost {
                model: "claude-haiku-4-5-20251001".into(),
                usd: 0.05,
                tokens: 400,
            },
        ]);
        let mut r = row();
        r.cost = Some(c);
        let spec = panel_spec(&r);

        let tooltip = spec
            .iter()
            .find(|s| s.id == "tokens_by_day")
            .expect("days")
            .rows
            .last()
            .expect("today")
            .tooltip
            .clone();

        // The panel's own model labels, in the columns the fleet split already
        // uses, and above it: what spent the day before where it was spent.
        assert!(
            tooltip.contains("By model\nOpus 5     600  ·  $1.20\nHaiku 4.5  400  ·  $0.05"),
            "{tooltip}"
        );
        assert!(
            tooltip.find("By model") < tooltip.find("By device"),
            "{tooltip}"
        );
    }

    #[test]
    fn a_lone_machine_gets_no_split_under_its_rows() {
        let mut r = row();
        r.cost = Some(cost());
        let spec = panel_spec(&r);
        for section in spec.iter().filter(|s| s.id.starts_with("tokens_by_")) {
            for row in &section.rows {
                assert!(
                    !row.tooltip.contains("By device"),
                    "{} {}",
                    section.id,
                    row.tooltip
                );
            }
        }
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
    /// Every source file of one frontend, by directory and extension.
    ///
    /// By directory rather than by file: a hardcoded path stops proving
    /// anything the moment the code moves to a sibling module, which it did
    /// once already.
    fn frontend_sources(id: &str, dir: &str, extension: &str) -> Vec<String> {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root");
        let root = repo.join(dir);
        let mut sources = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(next) = stack.pop() {
            let entries = std::fs::read_dir(&next)
                .unwrap_or_else(|e| panic!("{id}: cannot read {}: {e}", next.display()));
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == extension) {
                    sources.push(
                        std::fs::read_to_string(&path).unwrap_or_else(|e| {
                            panic!("{id}: cannot read {}: {e}", path.display())
                        }),
                    );
                }
            }
        }
        assert!(
            !sources.is_empty(),
            "{id}: no .{extension} sources under {} - the path has gone stale",
            root.display()
        );
        sources
    }

    /// The second screen's backstop, and the record of why waybar is not in it.
    ///
    /// The test above covers the panel; this covers the history screen, which
    /// no compiler checks the drawing of on any of these five. Waybar is
    /// deliberately absent: its tooltip is a hover surface with no second
    /// screen and no way to gain one, so a waybar user's history is the TUI,
    /// which left-click has opened since long before there was any history to
    /// open it for. That is a decision, not a gap - if it is ever revisited,
    /// this is the list to add it to.
    #[test]
    fn every_frontend_with_a_second_screen_draws_the_history_series() {
        let frontends = [
            ("tray", "crates/tokengauge-tray/src", "rs"),
            ("tui", "crates/tokengauge-tui/src", "rs"),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
            ),
            ("gnome", "gnome/tokengauge@arzaroth.github.io", "ts"),
            ("quickshell", "omarchy/arzaroth.tokengauge", "qml"),
        ];

        for (id, dir, extension) in frontends {
            let sources = frontend_sources(id, dir, extension);
            // The three fields a screen cannot draw the chart without: the
            // ranges it offers, the steps it plots, and their heights.
            for needle in ["history", "series", "points", "fraction"] {
                assert!(
                    sources.iter().any(|src| src.contains(needle)),
                    "{id} ({dir}) never reads `{needle}` - it is not drawing the \
                     history series the core resolved"
                );
            }
        }
    }

    /// A split bar is a `segments` list on a meter row, which no frontend's
    /// compiler knows about: one that never draws it shows the pooled bar and
    /// looks finished. The needle is the call that draws a meter's segments,
    /// not the word, which a type declaration alone would carry.
    #[test]
    fn every_panel_frontend_draws_split_bars() {
        let frontends = [
            (
                "waybar",
                "crates/tokengauge-waybar/src",
                "rs",
                "segmented_bar(&row.segments)",
            ),
            (
                "tray",
                "crates/tokengauge-tray/src",
                "rs",
                "split_bar(ui, &row.segments",
            ),
            (
                "tui",
                "crates/tokengauge-tui/src",
                "rs",
                "split_bar_spans(&row.segments",
            ),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
                "model: segmented.segments",
            ),
            (
                "gnome",
                "gnome/tokengauge@arzaroth.github.io",
                "ts",
                "add_child(splitBar(",
            ),
            (
                "quickshell",
                "omarchy/arzaroth.tokengauge",
                "qml",
                "model: meterRow.segments",
            ),
        ];
        for (id, dir, extension, needle) in frontends {
            let sources = frontend_sources(id, dir, extension);
            assert!(
                sources.iter().any(|src| src.contains(needle)),
                "{id} ({dir}) never draws a meter's `segments` (`{needle}`)"
            );
        }
    }

    /// A list a QML delegate reads off `modelData` is a sequence Qt has
    /// converted, not a JavaScript array: `Array.isArray` says false over a
    /// full list, which is how both QML panels drew a split bar pooled from
    /// 0.40.0 to 0.42.0. Test a list by its length.
    #[test]
    fn no_qml_frontend_asks_whether_a_delegates_list_is_an_array() {
        for (id, dir) in [
            ("plasma", "plasma/org.tokengauge.plasmoid/contents/ui"),
            ("quickshell", "omarchy/arzaroth.tokengauge"),
        ] {
            for src in frontend_sources(id, dir, "qml") {
                assert!(
                    !src.contains("Array.isArray(modelData."),
                    "{id} ({dir}) tests a delegate's list with Array.isArray, which is false for it"
                );
            }
        }
    }

    /// The `[panel]` options are flipped from every settings pane. Each
    /// needle is the pane's own call with that key, not the key alone, which
    /// the data layer and the type declarations carry whether or not a pane
    /// draws a control for it. Waybar and the TUI have no pane, so theirs is
    /// config.toml.
    #[test]
    fn every_settings_pane_offers_the_panel_options() {
        let frontends: [(&str, &str, &str, [&str; 3]); 4] = [
            (
                "tray",
                "crates/tokengauge-tray/src",
                "rs",
                [
                    "\"active_credential_only\",",
                    "Some((\"split_bars\"",
                    "Some((\"plans_total\"",
                ],
            ),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
                [
                    "root.setPanel(\"active_credential_only\"",
                    "root.setPanel(\"split_bars\"",
                    "root.setPanel(\"plans_total\"",
                ],
            ),
            (
                "gnome",
                "gnome/tokengauge@arzaroth.github.io",
                "ts",
                [
                    "toggle(_('Active credential only'), 'active_credential_only')",
                    "toggle(_('Split ALL PLANS bars'), 'split_bars')",
                    "write(total, 'plans_total'",
                ],
            ),
            (
                "quickshell",
                "omarchy/arzaroth.tokengauge",
                "qml",
                [
                    "{ key: \"active_credential_only\"",
                    "{ key: \"split_bars\"",
                    "usage.setPanel(\"plans_total\"",
                ],
            ),
        ];
        for (id, dir, extension, needles) in frontends {
            let sources = frontend_sources(id, dir, extension);
            for needle in needles {
                assert!(
                    sources.iter().any(|src| src.contains(needle)),
                    "{id} ({dir}) never draws a control for `{needle}` - a panel \
                     option is missing from its settings pane"
                );
            }
        }
    }

    /// remuda's button, on every surface that can draw one (ADR 0004).
    ///
    /// Each must read `serving` off the status core resolves, and open the page
    /// only through what the binary runs, never through remuda or its files.
    /// Waybar is absent because it has no button to draw (`--open=remuda` is
    /// for a click binding in its config), and the tray because it builds only
    /// where remuda does not run.
    #[test]
    fn every_frontend_with_a_header_offers_remuda_while_it_serves() {
        let frontends = [
            (
                "tui",
                "crates/tokengauge-tui/src",
                "rs",
                "state.remuda_serving",
                "launch::open_remuda",
            ),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
                "root.remudaServing",
                "--open=remuda",
            ),
            (
                "gnome",
                "gnome/tokengauge@arzaroth.github.io",
                "ts",
                "remuda?.serving",
                "--open=remuda",
            ),
            (
                "quickshell",
                "omarchy/arzaroth.tokengauge",
                "qml",
                "usage.remudaServing",
                "--open=remuda",
            ),
        ];

        for (id, dir, extension, shown, opened) in frontends {
            let sources = frontend_sources(id, dir, extension);
            for needle in [shown, opened] {
                assert!(
                    sources.iter().any(|src| src.contains(needle)),
                    "{id} ({dir}) never mentions `{needle}` - its remuda button \
                     is missing or no longer follows the binary"
                );
            }
            assert!(
                !sources.iter().any(|src| src.contains("serve.url")),
                "{id} ({dir}) names serve.url, which carries remuda's token"
            );
        }
    }

    /// Every frontend says when it last refreshed.
    ///
    /// Five of the six put the sentence behind their refresh control: hovering
    /// the button that would replace the figures is where the age of those
    /// figures belongs. Waybar has no button to hover - its tooltip *is* the
    /// hover surface - so it carries the same sentence as a line, and a right
    /// click is still the refresh.
    #[test]
    fn every_frontend_says_when_it_last_refreshed() {
        let frontends = [
            ("waybar", "crates/tokengauge-waybar/src", "rs"),
            ("tray", "crates/tokengauge-tray/src", "rs"),
            ("tui", "crates/tokengauge-tui/src", "rs"),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
            ),
            ("gnome", "gnome/tokengauge@arzaroth.github.io", "ts"),
            ("quickshell", "omarchy/arzaroth.tokengauge", "qml"),
        ];

        for (id, dir, extension) in frontends {
            let sources = frontend_sources(id, dir, extension);
            assert!(
                sources.iter().any(|src| src.contains("refresh_hint")),
                "{id} ({dir}) never reads `refresh_hint` - it is formatting the \
                 last refresh itself, or not saying it at all"
            );
        }
    }

    #[test]
    fn the_bar_tooltip_is_every_limit_and_one_money_line() {
        let mut r = row();
        r.cost = Some(cost());
        let tip = bar_tooltip(&r);

        assert_eq!(tip.title, "Claude");
        let labels: Vec<&str> = tip.lines.iter().map(|l| l.label.as_str()).collect();
        let spec = panel_spec(&r);
        let limits: Vec<&str> = spec
            .iter()
            .find(|s| s.id == "limits")
            .expect("a limits section")
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect();
        // Every limit the panel draws, in the panel's order, and then one cost
        // line - not the whole cost section.
        assert_eq!(labels[..limits.len()], limits[..]);
        assert_eq!(labels.len(), limits.len() + 1);

        // The tier rides along, so a frontend never re-derives the boundaries.
        assert_eq!(tip.lines[0].value, "31%");
        assert_eq!(tip.lines[0].tone, Tone::Good);
        // A spend figure has no threshold to tint against.
        assert_eq!(tip.lines.last().unwrap().tone, Tone::Normal);
    }

    /// A prepaid provider has a balance and no transcripts to read, so the
    /// cost section is the balance alone. That is the money line it hovers
    /// with - taking today's spend by name instead would leave the one
    /// provider whose money matters most with no money on it.
    #[test]
    fn a_prepaid_provider_hovers_with_its_balance() {
        let mut r = row();
        r.cost = None;
        r.credits = Some(18.44);
        let tip = bar_tooltip(&r);
        let money = tip.lines.last().expect("a money line");
        assert_eq!(money.label, "Credits");
        assert_eq!(money.value, "$18.44");
        assert_eq!(money.tone, Tone::Normal);
        // Still one line, not the section.
        assert_eq!(tip.lines.iter().filter(|l| l.label == "Credits").count(), 1);
    }

    #[test]
    fn a_provider_with_no_windows_still_names_itself() {
        let mut r = row();
        r.session_used = None;
        r.weekly_used = None;
        let tip = bar_tooltip(&r);
        assert_eq!(tip.title, "Claude");
        assert!(tip.lines.is_empty(), "{:?}", tip.lines);
    }

    /// Every frontend with a bar icon says the same thing when it is hovered.
    ///
    /// Four surfaces have an icon sitting in a bar or a tray, and hovering one
    /// is the cheapest read of the panel there is. They each used to answer it
    /// alone: Plasma picked the lines out of `panel` in QML, the tray
    /// re-derived the two percentages in Rust and named only those, and GNOME
    /// and the Quickshell widget said nothing at all - a hover that looked
    /// broken rather than deliberate.
    ///
    /// Waybar and the TUI are absent because neither has an icon to hover.
    /// Waybar's tooltip *is* the panel, so a summary of it would be the same
    /// figures twice on one surface; a TUI has no pointer surface at all.
    #[test]
    fn every_frontend_with_a_bar_icon_says_the_same_thing_on_hover() {
        let frontends = [
            ("tray", "crates/tokengauge-tray/src", "rs"),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
            ),
            ("gnome", "gnome/tokengauge@arzaroth.github.io", "ts"),
            ("quickshell", "omarchy/arzaroth.tokengauge", "qml"),
        ];

        for (id, dir, extension) in frontends {
            let sources = frontend_sources(id, dir, extension);
            assert!(
                sources.iter().any(|src| src.contains("bar_tooltip")),
                "{id} ({dir}) never reads `bar_tooltip` - its icon is summarising \
                 the panel itself, or saying nothing when it is hovered"
            );
        }
    }

    /// Every frontend that draws the panel draws every kind of section in it.
    ///
    /// The backstop for a rule no compiler enforces: `SectionKind` is a Rust
    /// enum, a QML string and a TypeScript union, and none of the three makes
    /// a frontend that never mentions a kind fail to build.
    #[test]
    fn every_panel_frontend_handles_every_section_kind() {
        // Each frontend is named by its *directory* and the extensions its
        // sources use, not by one file. A hardcoded path stops proving
        // anything the moment the code moves to a sibling module - which it
        // did, and this test went red rather than silently green only because
        // the file it named stopped existing at that path.
        let frontends = [
            (
                "waybar",
                "crates/tokengauge-waybar/src",
                "rs",
                "SectionKind::",
            ),
            ("tray", "crates/tokengauge-tray/src", "rs", "SectionKind::"),
            // The TUI is exempt from *layout* parity, not content parity: it
            // draws the day section as a chart and keeps its own chrome, but
            // every string in a section comes from here.
            ("tui", "crates/tokengauge-tui/src", "rs", "SectionKind::"),
            (
                "plasma",
                "plasma/org.tokengauge.plasmoid/contents/ui",
                "qml",
                "\"",
            ),
            ("gnome", "gnome/tokengauge@arzaroth.github.io", "ts", "'"),
            ("quickshell", "omarchy/arzaroth.tokengauge", "qml", "\""),
        ];

        for (id, dir, extension, prefix) in frontends {
            let sources = frontend_sources(id, dir, extension);

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
                    sources.iter().any(|src| src.contains(&needle)),
                    "{id} ({dir}) never handles the `{kind}` section kind - \
                     looked for {needle}"
                );
            }
        }
    }

    #[test]
    fn a_refresh_control_says_how_old_the_figures_under_it_are() {
        let now = crate::now_ms();
        let iso = chrono::DateTime::from_timestamp_millis(now - 180_000)
            .expect("a timestamp")
            .to_rfc3339();
        let hint = refresh_hint(Some(&iso), now);
        assert!(hint.starts_with("Last refreshed 3m ago \u{b7} "), "{hint}");

        // A payload with no instant of its own - a snapshot written before
        // 0.21 - says so rather than claiming a time it does not have.
        assert_eq!(refresh_hint(None, now), "Last refresh unknown");
        assert_eq!(refresh_hint(Some("yesterday"), now), "Last refresh unknown");
    }

    #[test]
    fn a_refresh_from_another_day_carries_its_date() {
        let now = crate::now_ms();
        let iso = chrono::DateTime::from_timestamp_millis(now - 2 * 86_400_000)
            .expect("a timestamp")
            .to_rfc3339();
        let hint = refresh_hint(Some(&iso), now);
        assert!(hint.starts_with("Last refreshed 2d ago \u{b7} "), "{hint}");
        // "14:32" alone is a time on an unnamed day, so the stamp gains a date
        // once the refresh is not today's.
        let stamp = hint.split(" \u{b7} ").nth(1).expect("a stamp");
        assert!(stamp.contains(' '), "no date in `{stamp}`");
    }

    #[test]
    fn a_long_problem_does_not_run_off_the_row() {
        let long = "could not list: AccessDenied (403) the request signature we \
                    calculated does not match the signature you provided";
        let cut = ellipsize(long, 72);
        assert!(cut.chars().count() <= 72, "{cut}");
        assert!(cut.ends_with('…'));
        assert!(!cut.contains("  "), "cut on a word boundary: {cut}");
        assert_eq!(ellipsize("short enough", 72), "short enough");
    }

    #[test]
    fn money_drops_cents_over_a_hundred() {
        assert_eq!(money(1.5), "$1.50");
        assert_eq!(money(99.994), "$99.99");
        assert_eq!(money(312.21), "$312");
        assert_eq!(money(f64::NAN), "-");
    }

    #[test]
    fn money_never_prints_a_signed_zero() {
        // What an empty period actually produces: `[].sum::<f64>()` is -0.0,
        // and "$-0.00" on a chart reads as a bug rather than as nothing spent.
        let nothing: f64 = [].iter().sum();
        assert!(nothing.is_sign_negative(), "the identity is -0.0");
        assert_eq!(money(nothing), "$0.00");
        assert_eq!(money(-0.0), "$0.00");
        assert_eq!(money(0.0), "$0.00");
    }

    #[test]
    fn exact_tokens_groups_thousands() {
        assert_eq!(exact_tokens(0), "0");
        assert_eq!(exact_tokens(999), "999");
        assert_eq!(exact_tokens(1_000), "1,000");
        assert_eq!(exact_tokens(384_000_000), "384,000,000");
    }
}
