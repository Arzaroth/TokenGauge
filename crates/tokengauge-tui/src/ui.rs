use chrono::{Datelike, Duration as ChronoDuration, Local, Weekday};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Borders, Clear, List, ListItem, ListState,
    Paragraph, Wrap,
};
use tokengauge_core::{
    CostInfo, ModelCost, ProviderRow, format_tokens, format_updated_relative, theme, window_labels,
};

use crate::app::AppState;
use crate::theme::{color_for, dim, green, hex_to_color, provider_icon_color};

// Width breakpoints: hide sidebar on narrow terminals.
const NARROW_BREAKPOINT: u16 = 80;
const SIDEBAR_WIDTH: u16 = 24;

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

pub fn draw(frame: &mut Frame, state: &mut AppState, is_refreshing: bool) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(3), // header
        Constraint::Min(0),    // body
        Constraint::Length(1), // footer
    ])
    .split(area);

    render_header(frame, layout[0], state, is_refreshing);
    render_body(frame, layout[1], state);
    render_footer(frame, layout[2], state, is_refreshing);

    if state.show_help {
        render_help_popup(frame, area);
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn render_header(frame: &mut Frame, area: Rect, state: &AppState, is_refreshing: bool) {
    // Half-circle rotation: vertically center-aligned in most fonts, unlike
    // Braille spinners which sit a row low.
    let spinner_frames = ["◐", "◓", "◑", "◒"];
    let spinner = spinner_frames[state.spinner_index % spinner_frames.len()];

    let title = Span::styled(
        "  TokenGauge",
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
    );
    let separator = Span::styled("  ·  ", Style::default().fg(dim()));
    let status = if is_refreshing {
        Span::styled(
            format!("{spinner} refreshing"),
            Style::default()
                .fg(hex_to_color(&theme().yellow))
                .add_modifier(Modifier::BOLD),
        )
    } else {
        let secs = state.last_refresh.elapsed().as_secs();
        let label = match secs {
            0..=5 => "just now".to_string(),
            6..=59 => format!("{secs}s ago"),
            60..=3599 => format!("{}m ago", secs / 60),
            _ => format!("{}h ago", secs / 3600),
        };
        Span::styled(format!("updated {label}"), Style::default().fg(dim()))
    };

    let provider_count = state.rows.len();
    let count_span = Span::styled(
        format!(
            "{provider_count} provider{}",
            if provider_count == 1 { "" } else { "s" }
        ),
        Style::default().fg(dim()),
    );

    let line = Line::from(vec![
        title,
        separator.clone(),
        status,
        separator,
        count_span,
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(dim()));
    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);
}

// ---------------------------------------------------------------------------
// Body: Sidebar + Detail
// ---------------------------------------------------------------------------

fn render_body(frame: &mut Frame, area: Rect, state: &mut AppState) {
    if state.rows.is_empty() && state.errors.is_empty() {
        let message = state
            .status_message
            .as_deref()
            .or(state.last_error.as_deref())
            .unwrap_or("No providers returned");
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Usage ")
            .border_style(Style::default().fg(dim()));
        let paragraph = Paragraph::new(message)
            .style(Style::default().fg(Color::Red))
            .block(block);
        frame.render_widget(paragraph, area);
        state.content_height = 0;
        state.viewport_height = area.height;
        return;
    }

    let has_errors = !state.errors.is_empty();
    let with_errors = if has_errors {
        let err_h = ((state.errors.len() as u16) + 2 + 1).min(8);
        Layout::vertical([Constraint::Min(0), Constraint::Length(err_h)]).split(area)
    } else {
        Layout::vertical([Constraint::Min(0)]).split(area)
    };
    let usage_area = with_errors[0];

    if state.rows.is_empty() {
        // Errors only
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Usage ")
            .border_style(Style::default().fg(dim()));
        let paragraph = Paragraph::new("Errors only - no usable provider data")
            .style(Style::default().fg(Color::Red))
            .block(block);
        frame.render_widget(paragraph, usage_area);
    } else if usage_area.width < NARROW_BREAKPOINT {
        // Narrow mode: hide sidebar, show only the active provider's detail.
        render_detail(frame, usage_area, state);
    } else {
        let cols = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(0)])
            .split(usage_area);
        render_sidebar(frame, cols[0], state);
        render_detail(frame, cols[1], state);
    }

    if has_errors {
        render_errors(frame, with_errors[1], state);
    }
}

fn render_sidebar(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let items: Vec<ListItem> = state
        .rows
        .iter()
        .map(|row| {
            let (icon, color) = provider_icon_color(&row.provider);
            let used = row.session_used.or(row.weekly_used).unwrap_or(0);
            let pct_color = color_for(used);
            let name = truncate(&row.provider, 12);
            let pct_str = if row.session_used.is_some() || row.weekly_used.is_some() {
                format!("{used}%")
            } else {
                "—".to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(icon.to_string(), Style::default().fg(color)),
                Span::raw("  "),
                Span::raw(name),
                Span::raw("  "),
                Span::styled(
                    pct_str,
                    Style::default().fg(pct_color).add_modifier(Modifier::BOLD),
                ),
            ]))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Providers ")
        .border_style(Style::default().fg(dim()));

    let list = List::new(items)
        .block(block)
        .highlight_style(
            // Keep per-span colors (icon brand color, tier-tinted percent)
            // visible by NOT setting fg on the highlight style.
            Style::default()
                .bg(Color::Rgb(0x31, 0x32, 0x44))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");

    let mut list_state = ListState::default();
    list_state.select(Some(state.active_tab));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn render_detail(frame: &mut Frame, area: Rect, state: &mut AppState) {
    let row = &state.rows[state.active_tab];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(detail_title_line(row))
        .border_style(Style::default().fg(dim()));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Vertical split: usage gauges, cost section, credits
    let mut sections: Vec<Constraint> = Vec::new();
    let usage_lines = count_usage_lines(row);
    let cost_block_height = row.cost.as_ref().map(estimate_cost_height).unwrap_or(0);
    let credits_height = (row.credits != "—" && !row.credits.is_empty()) as u16 * 2;

    sections.push(Constraint::Length(usage_lines));
    if cost_block_height > 0 {
        sections.push(Constraint::Length(cost_block_height));
    }
    if credits_height > 0 {
        sections.push(Constraint::Length(credits_height));
    }
    sections.push(Constraint::Min(0));

    let chunks = Layout::vertical(sections).split(inner);
    let mut idx = 0;
    render_usage(frame, chunks[idx], row);
    idx += 1;
    if cost_block_height > 0 {
        render_cost(frame, chunks[idx], row.cost.as_ref().unwrap());
        idx += 1;
    }
    if credits_height > 0 {
        render_credits(frame, chunks[idx], row);
    }

    // Track viewport height for any future scroll
    state.content_height = usage_lines + cost_block_height + credits_height;
    state.viewport_height = inner.height;
}

fn detail_title_line(row: &ProviderRow) -> Line<'static> {
    let (icon, icon_color) = provider_icon_color(&row.provider);
    let plan = row
        .plan_label
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|p| format!(" · {p}"))
        .unwrap_or_default();
    let updated = row
        .updated_iso
        .as_deref()
        .and_then(format_updated_relative)
        .map(|r| format!(" · updated {r}"))
        .unwrap_or_default();
    Line::from(vec![
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::default().fg(icon_color)),
        Span::raw("  "),
        Span::styled(
            row.provider.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{plan}{updated} "), Style::default().fg(dim())),
    ])
}

// ---------------------------------------------------------------------------
// Usage section: Gauge widgets per window
// ---------------------------------------------------------------------------

struct UsageRow {
    label: String,
    used: Option<u8>,
    reset: String,
}

fn collect_usage_rows(row: &ProviderRow) -> Vec<UsageRow> {
    let (session_label, weekly_label, tertiary_label) = window_labels(&row.provider);
    let mut rows = vec![
        UsageRow {
            label: session_label.to_string(),
            used: row.session_used,
            reset: row.session_reset.clone(),
        },
        UsageRow {
            label: weekly_label.to_string(),
            used: row.weekly_used,
            reset: row.weekly_reset.clone(),
        },
    ];
    if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
        rows.push(UsageRow {
            label: tertiary_label.to_string(),
            used: row.tertiary_used,
            reset: row.tertiary_reset.clone(),
        });
    }
    for extra in &row.extra_windows {
        rows.push(UsageRow {
            label: extra.title.clone(),
            used: extra.used,
            reset: extra.reset.clone(),
        });
    }
    rows
}

fn count_usage_lines(row: &ProviderRow) -> u16 {
    // 1 header row + 1 row per gauge + 1 trailing spacer
    collect_usage_rows(row).len() as u16 + 2
}

fn render_usage(frame: &mut Frame, area: Rect, row: &ProviderRow) {
    let usage_rows = collect_usage_rows(row);
    if usage_rows.is_empty() {
        return;
    }

    let header_line = Line::from(Span::styled(
        " Usage windows",
        Style::default().fg(dim()).add_modifier(Modifier::BOLD),
    ));
    let header_para = Paragraph::new(header_line);
    let constraints: Vec<Constraint> = std::iter::once(Constraint::Length(1))
        .chain(usage_rows.iter().map(|_| Constraint::Length(1)))
        .chain(std::iter::once(Constraint::Length(1)))
        .collect();
    let chunks = Layout::vertical(constraints).split(area);
    frame.render_widget(header_para, chunks[0]);

    // Tight mode: drop the trailing "resets ..." column when there isn't
    // room (~28 chars min for a readable gauge + label).
    // Size label column to the widest label (capped) so full names like
    // "Weekly (Sonnet)" / "Daily Routine" don't ellipsize in wide mode.
    let widest_label = usage_rows
        .iter()
        .map(|u| u.label.chars().count())
        .max()
        .unwrap_or(0);
    let max_label_w = if area.width >= 100 {
        24
    } else if area.width >= 80 {
        20
    } else {
        16
    };
    let label_w: u16 = widest_label.clamp(8, max_label_w) as u16;
    let tight = area.width < 64;
    let trail_w: u16 = if tight { 0 } else { 26 };
    // truncate to label_w (not label_w - 1) so widest label fits exactly
    // - the layout split already reserves label_w columns.

    for (i, urow) in usage_rows.iter().enumerate() {
        let slot = chunks[i + 1];
        let inner = slot.inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        // Reserve 2 cols of gutter between the label and the bar so the
        // gauge doesn't butt up against the label text.
        let gutter: u16 = 2;
        let (label_slot, bar_slot, trail_slot) = if trail_w > 0 {
            let split = Layout::horizontal([
                Constraint::Length(label_w),
                Constraint::Length(gutter),
                Constraint::Min(20),
                Constraint::Length(trail_w),
            ])
            .split(inner);
            (split[0], split[2], Some(split[3]))
        } else {
            let split = Layout::horizontal([
                Constraint::Length(label_w),
                Constraint::Length(gutter),
                Constraint::Min(10),
            ])
            .split(inner);
            (split[0], split[2], None)
        };

        let label_text = truncate(&urow.label, label_w as usize);
        let label = Paragraph::new(Line::from(Span::styled(
            label_text,
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        )));
        frame.render_widget(label, label_slot);

        // Hand-rolled bar gives us exact width semantics (Gauge widget
        // rounds up to keep its inline label visible, which exaggerated
        // low percentages).
        let bar_inner = bar_slot;
        match urow.used {
            Some(pct) => {
                let pct_clamped = pct.min(100);
                let pct_label = format!(" {pct_clamped:>3}%");
                let label_w = pct_label.chars().count() as u16;
                let bar_w = bar_inner.width.saturating_sub(label_w + 1);
                let filled = ((pct_clamped as usize * bar_w as usize) + 50) / 100; // round to nearest
                let filled = filled.min(bar_w as usize);
                let empty = bar_w as usize - filled;
                let bar_color = color_for(pct_clamped);
                let line = Line::from(vec![
                    Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
                    Span::styled("░".repeat(empty), Style::default().fg(dim())),
                    Span::styled(
                        pct_label,
                        Style::default().fg(bar_color).add_modifier(Modifier::BOLD),
                    ),
                ]);
                frame.render_widget(Paragraph::new(line), bar_inner);
            }
            None => {
                let placeholder = Paragraph::new(Line::from(vec![
                    Span::styled(
                        "─".repeat(bar_inner.width.saturating_sub(8) as usize),
                        Style::default().fg(dim()),
                    ),
                    Span::styled("    n/a", Style::default().fg(dim())),
                ]));
                frame.render_widget(placeholder, bar_inner);
            }
        }

        if let Some(trail_area) = trail_slot {
            let trailing = match (urow.used, urow.reset.as_str()) {
                (None, _) => "no data".to_string(),
                (Some(0), "—") => String::new(),
                (Some(_), "—") => "not started".to_string(),
                (Some(_), reset) => truncate(&format!("resets {reset}"), trail_w as usize - 1),
            };
            let trail = Paragraph::new(Line::from(Span::styled(
                trailing,
                Style::default().fg(dim()),
            )));
            frame.render_widget(trail, trail_area);
        }
    }
}

// ---------------------------------------------------------------------------
// Cost section: text + Sparkline + BarChart
// ---------------------------------------------------------------------------

fn estimate_cost_height(cost: &CostInfo) -> u16 {
    // header + (rate? + session? + weekly?) totals + today + month + spark + barchart
    let mut h = 1; // section header line
    if cost.burn_rate.is_some() {
        h += 1;
    }
    if cost.session_usd > 0.0 {
        h += 1;
    }
    if cost.weekly_usd > 0.0 {
        h += 1;
    }
    h += 1; // Today
    h += 1; // Month
    if !cost.weekly_cost_history.is_empty() {
        // 1 title border + 2 bar body + 1 weekday label + 1 dollar caption
        h += 5;
    }
    if !cost.today_models.is_empty() {
        // 1 title row + 1 row per model (capped at 6)
        h += 1 + cost.today_models.len().min(6) as u16;
    }
    h.min(40)
}

fn render_cost(frame: &mut Frame, area: Rect, cost: &CostInfo) {
    let header = Paragraph::new(Line::from(Span::styled(
        " Cost",
        Style::default().fg(dim()).add_modifier(Modifier::BOLD),
    )));

    // Vertical split: header, summary text, sparkline, barchart
    let has_spark = !cost.weekly_cost_history.is_empty();
    let has_bars = !cost.today_models.is_empty();

    let summary_h = {
        let mut h = 2; // today + month
        if cost.burn_rate.is_some() {
            h += 1;
        }
        if cost.session_usd > 0.0 {
            h += 1;
        }
        if cost.weekly_usd > 0.0 {
            h += 1;
        }
        h
    };

    let mut constraints = vec![Constraint::Length(1), Constraint::Length(summary_h)];
    if has_spark {
        constraints.push(Constraint::Length(5));
    }
    if has_bars {
        let rows = cost.today_models.len().min(6) as u16;
        constraints.push(Constraint::Length(1 + rows));
    }
    let chunks = Layout::vertical(constraints).split(area);

    frame.render_widget(header, chunks[0]);
    frame.render_widget(cost_summary(cost), chunks[1]);

    let mut idx = 2;
    if has_spark {
        render_sparkline(frame, chunks[idx], cost);
        idx += 1;
    }
    if has_bars {
        render_today_models(frame, chunks[idx], cost);
    }
}

fn cost_summary(cost: &CostInfo) -> Paragraph<'static> {
    let pad = "  ";
    let label_w = 8;
    let value_w = 9;
    let mut lines: Vec<Line> = Vec::new();

    if let Some(br) = cost.burn_rate.as_ref() {
        let mut spans = vec![
            Span::raw(pad),
            Span::styled(
                format!("{:<label_w$}", "Rate"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>value_w$}/hr", format!("${:.2}", br.cost_per_hour)),
                Style::default().fg(green()),
            ),
        ];
        if let Some(avg) = cost.avg_hourly_cost().filter(|a| *a > 0.0) {
            let pct = ((br.cost_per_hour - avg) / avg) * 100.0;
            let arrow = if pct >= 0.0 { "↑" } else { "↓" };
            let trend_color = if pct >= 25.0 {
                hex_to_color(&theme().red)
            } else if pct >= -10.0 {
                hex_to_color(&theme().yellow)
            } else {
                green()
            };
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{arrow}{:.0}%", pct.abs()),
                Style::default().fg(trend_color),
            ));
            spans.push(Span::styled(" vs 7d avg", Style::default().fg(dim())));
        }
        lines.push(Line::from(spans));
    }

    let money_row = |label: &str, usd: f64| {
        Line::from(vec![
            Span::raw(pad),
            Span::styled(
                format!("{label:<label_w$}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:>value_w$}", format!("${usd:.2}")),
                Style::default().fg(green()),
            ),
        ])
    };
    if cost.session_usd > 0.0 {
        lines.push(money_row("Session", cost.session_usd));
    }
    if cost.weekly_usd > 0.0 {
        lines.push(money_row("Weekly", cost.weekly_usd));
    }
    let today_tokens = format_tokens(cost.today_tokens);
    let month_tokens = format_tokens(cost.monthly_tokens);
    lines.push(Line::from(vec![
        Span::raw(pad),
        Span::styled(
            format!("{:<label_w$}", "Today"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>value_w$}", format!("${:.2}", cost.today_usd)),
            Style::default().fg(green()),
        ),
        Span::styled(
            format!("  ·  {today_tokens} tokens"),
            Style::default().fg(dim()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw(pad),
        Span::styled(
            format!("{:<label_w$}", "Month"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>value_w$}", format!("${:.2}", cost.monthly_usd)),
            Style::default().fg(green()),
        ),
        Span::styled(
            format!("  ·  {month_tokens} tokens"),
            Style::default().fg(dim()),
        ),
    ]));
    Paragraph::new(lines)
}

fn weekday_initial(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "M",
        Weekday::Tue => "T",
        Weekday::Wed => "W",
        Weekday::Thu => "T",
        Weekday::Fri => "F",
        Weekday::Sat => "S",
        Weekday::Sun => "S",
    }
}

fn render_sparkline(frame: &mut Frame, area: Rect, cost: &CostInfo) {
    let raw = &cost.weekly_cost_history;
    if raw.is_empty() {
        return;
    }
    let n = raw.len();
    let max = raw.iter().copied().fold(0.0_f64, f64::max);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(dim()))
        .title(Span::styled(
            " 7-day cost ",
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let peak_str = format!("peak ${max:.2}");
    let peak_w = (peak_str.chars().count() as u16) + 2;
    let split = Layout::horizontal([Constraint::Min(10), Constraint::Length(peak_w)]).split(inner);
    let chart_area = split[0];
    let peak_area = split[1];

    // Per-day weekday initials (oldest first, today last).
    let today = Local::now().date_naive();
    let day_labels: Vec<String> = (0..n)
        .map(|i| {
            let offset = (n - 1 - i) as i64;
            let day = today - ChronoDuration::days(offset);
            weekday_initial(day.weekday()).to_string()
        })
        .collect();
    let dollar_labels: Vec<String> = raw.iter().map(|usd| format!("${usd:.0}")).collect();

    // BarChart only carries one label row; build bars with the day letter
    // there, then layout a second caption row below for the $ amounts.
    let gap: u16 = 1;
    let bar_width =
        ((chart_area.width.saturating_sub(gap * (n as u16 - 1)) / n as u16) as u16).max(3);
    let stride = bar_width + gap;

    // Reserve bottom row of chart_area for dollar amounts.
    let split_v = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).split(chart_area);
    let bars_area = split_v[0];
    let dollars_area = split_v[1];

    let bars: Vec<Bar> = raw
        .iter()
        .zip(day_labels.iter())
        .map(|(usd, label)| {
            let cents = (*usd * 100.0).round().max(0.0) as u64;
            Bar::default()
                .value(cents)
                .label(Line::from(label.clone()))
                .text_value(String::new()) // hide inline value
                .style(Style::default().fg(green()))
                .value_style(Style::default().fg(green()).bg(green()))
        })
        .collect();
    let group = BarGroup::default().bars(&bars);
    let chart = BarChart::default()
        .data(group)
        .bar_width(bar_width)
        .bar_gap(gap)
        .label_style(Style::default().fg(dim()));
    frame.render_widget(chart, bars_area);

    // Render dollar amounts centered under each day column.
    for (i, dollar) in dollar_labels.iter().enumerate() {
        let x_offset = i as u16 * stride;
        if x_offset >= dollars_area.width {
            break;
        }
        let cell_w = bar_width.min(dollars_area.width.saturating_sub(x_offset));
        let cell = Rect {
            x: dollars_area.x + x_offset,
            y: dollars_area.y,
            width: cell_w,
            height: 1,
        };
        let para = Paragraph::new(Line::from(Span::styled(
            dollar.clone(),
            Style::default().fg(green()).add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(para, cell);
    }

    let peak_para = Paragraph::new(Line::from(Span::styled(
        peak_str,
        Style::default().fg(dim()),
    )));
    frame.render_widget(peak_para, peak_area);
}

fn render_today_models(frame: &mut Frame, area: Rect, cost: &CostInfo) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(dim()))
        .title(Span::styled(
            " Today by model · spend in $ ",
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let models: Vec<&ModelCost> = cost.today_models.iter().take(6).collect();
    if models.is_empty() {
        return;
    }

    // Auto-fit name column to the widest model name (capped) and dollar
    // column to the widest formatted amount; rest of the row is the bar.
    let max_total = models.iter().map(|m| m.usd).fold(0.0_f64, f64::max);
    let name_cap = ((inner.width as usize).saturating_sub(20) / 2).clamp(8, 24);
    let name_w = models
        .iter()
        .map(|m| truncate(&m.model, name_cap).chars().count())
        .max()
        .unwrap_or(8)
        .max(8);
    let usd_strs: Vec<String> = models.iter().map(|m| format!("${:.2}", m.usd)).collect();
    let usd_w = usd_strs
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(6);

    let pad = "  ";
    let bar_room = (inner.width as usize).saturating_sub(pad.len() + name_w + 2 + usd_w + 1);
    let bar_room = bar_room.max(4);

    let lines: Vec<Line<'static>> = models
        .iter()
        .zip(usd_strs.iter())
        .map(|(m, usd)| {
            let frac = if max_total > 0.0 {
                m.usd / max_total
            } else {
                0.0
            };
            let filled = (frac * bar_room as f64).round() as usize;
            let filled = filled.min(bar_room);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_room - filled));
            let name = truncate(&m.model, name_w);
            Line::from(vec![
                Span::raw(pad),
                Span::styled(format!("{name:<name_w$}"), Style::default().fg(dim())),
                Span::raw(" "),
                Span::styled(bar, Style::default().fg(green())),
                Span::raw(" "),
                Span::styled(
                    format!("{usd:>usd_w$}"),
                    Style::default().fg(green()).add_modifier(Modifier::BOLD),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_credits(frame: &mut Frame, area: Rect, row: &ProviderRow) {
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled("Credits", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("    "),
        Span::styled(format!("${}", row.credits), Style::default().fg(green())),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn render_errors(frame: &mut Frame, area: Rect, state: &AppState) {
    let error_lines: Vec<Line> = state
        .errors
        .iter()
        .map(|err| {
            Line::from(vec![
                Span::styled(
                    format!(" {}: ", err.provider),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate(&err.message, 80),
                    Style::default().fg(Color::LightRed),
                ),
            ])
        })
        .chain(std::iter::once(Line::from(Span::styled(
            format!(" Full details: {}", state.cache_file.display()),
            Style::default().fg(Color::DarkGray),
        ))))
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Errors ")
        .border_style(Style::default().fg(Color::Red));
    let widget = Paragraph::new(error_lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

// ---------------------------------------------------------------------------
// Footer + Help popup
// ---------------------------------------------------------------------------

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState, is_refreshing: bool) {
    let status_text = if is_refreshing {
        "refreshing"
    } else {
        state.status_message.as_deref().unwrap_or("idle")
    };
    let status_color = if is_refreshing {
        hex_to_color(&theme().yellow)
    } else if state.status_message.is_some() {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let key = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let dim_s = Style::default().fg(Color::Gray);
    let sep = Style::default().fg(Color::DarkGray);
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled("j/k", key),
        Span::styled(" select", dim_s),
        Span::styled("  ", sep),
        Span::styled("r", key),
        Span::styled(" refresh", dim_s),
        Span::styled("  ", sep),
        Span::styled("u", key),
        Span::styled(" dashboard", dim_s),
        Span::styled("  ", sep),
        Span::styled("s", key),
        Span::styled(" status", dim_s),
        Span::styled("  ", sep),
        Span::styled("?", key),
        Span::styled(" help", dim_s),
        Span::styled("  ", sep),
        Span::styled("q", key),
        Span::styled(" quit", dim_s),
        Span::styled("  ·  ", sep),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_help_popup(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(hex_to_color(&theme().yellow)))
        .title(Span::styled(
            " Keybindings ",
            Style::default()
                .fg(hex_to_color(&theme().yellow))
                .add_modifier(Modifier::BOLD),
        ));

    let key = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let desc = Style::default().fg(Color::Gray);
    let lines = vec![
        Line::from(""),
        binding_line("j / ↓", "select next provider", key, desc),
        binding_line("k / ↑", "select previous provider", key, desc),
        binding_line("h / l", "select prev / next provider", key, desc),
        binding_line("g / G", "first / last provider", key, desc),
        binding_line("r", "refresh now", key, desc),
        binding_line("u", "open provider dashboard", key, desc),
        binding_line("s", "open provider status page", key, desc),
        binding_line("?", "toggle this help", key, desc),
        binding_line("q / esc", "quit", key, desc),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(dim()),
        )),
    ];
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, popup);
}

fn binding_line(
    key_str: &str,
    desc_str: &str,
    key_style: Style,
    desc_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{key_str:<10}"), key_style),
        Span::styled(desc_str.to_string(), desc_style),
    ])
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(popup_layout[1])[1]
}
