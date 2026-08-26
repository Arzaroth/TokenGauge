use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, BorderType, Borders, Clear, List, ListItem, ListState,
    Paragraph, Wrap,
};
use tokengauge_core::panel::{PanelRow, Section, SectionKind, Tone, panel_spec};
use tokengauge_core::{ProviderRow, format_updated_relative, theme};

use crate::app::{AppState, Overlay};
use crate::theme::{dim, green, hex_to_color, provider_icon_color, tone_color};

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

    match &state.overlay {
        // Full screen: the panel underneath is drawn and then covered, which is
        // wasteful but keeps the layout code in one place.
        Overlay::Sync(sync) => sync.render(frame, area),
        Overlay::Help => render_help_popup(frame, area),
        Overlay::None => {}
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
            // Tone::for_percent is the tier's owner; the palette is this
            // frontend's. Reaching for a second threshold table here is how the
            // sidebar and the gauge under it come to disagree.
            let pct_color = tone_color(Tone::for_percent(used));
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

    // The core resolves the whole panel: which sections exist, in what order,
    // and every string in them. This frontend picks a shape per kind and loops,
    // so a section added in panel.rs reaches the terminal with no edit here.
    let spec = panel_spec(row);
    let credits_height = (row.credits != "—" && !row.credits.is_empty()) as u16 * 2;

    let mut constraints: Vec<Constraint> = spec
        .iter()
        .map(|section| Constraint::Length(section_height(section)))
        .collect();
    if credits_height > 0 {
        constraints.push(Constraint::Length(credits_height));
    }
    constraints.push(Constraint::Min(0));

    let chunks = Layout::vertical(constraints).split(inner);
    for (i, section) in spec.iter().enumerate() {
        render_section(frame, chunks[i], section);
    }
    if credits_height > 0 {
        render_credits(frame, chunks[spec.len()], row);
    }
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
// Panel sections
// ---------------------------------------------------------------------------

/// Rows past this are dropped from a `Bars` section. The detail pane is one
/// screen with no scroll, and a long tail of sub-1% models pushes the sections
/// under it off the bottom.
const BARS_ROW_CAP: usize = 6;

/// The section drawn as a chart instead of a row list. Same section, same
/// numbers, a shape worth the vertical space a terminal has - the layout
/// exemption this frontend holds is exactly this kind of swap, and the strings
/// still come from the spec.
const DAY_CHART_ID: &str = "tokens_by_day";
const DAY_CHART_HEIGHT: u16 = 5;

fn section_rows(section: &Section) -> &[PanelRow] {
    match section.kind {
        SectionKind::Bars => &section.rows[..section.rows.len().min(BARS_ROW_CAP)],
        _ => &section.rows,
    }
}

fn section_height(section: &Section) -> u16 {
    if section.id == DAY_CHART_ID {
        return DAY_CHART_HEIGHT;
    }
    let rows = section_rows(section).len() as u16;
    match section.kind {
        // Header line, one line per meter, then a spacer before what follows.
        SectionKind::Meters => rows + 2,
        // The title rides a top border rather than taking a line of its own.
        SectionKind::Bars | SectionKind::Rows => rows + 1,
    }
}

fn render_section(frame: &mut Frame, area: Rect, section: &Section) {
    if section.id == DAY_CHART_ID {
        render_day_chart(frame, area, section);
        return;
    }
    match section.kind {
        SectionKind::Meters => render_meters(frame, area, section),
        SectionKind::Bars => render_bars(frame, area, section),
        SectionKind::Rows => render_rows(frame, area, section),
    }
}

/// A dim heading on a line of its own, for the kinds that do not draw a table.
fn section_header(title: &str) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        format!(" {title}"),
        Style::default().fg(dim()).add_modifier(Modifier::BOLD),
    )))
}

/// A heading riding a top border, for the kinds that do.
fn section_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(dim()))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        ))
}

/// Label, gauge and value on one line, the footnote and the tinted pace badge
/// trailing. The gauge is hand-rolled: ratatui's Gauge rounds up to keep its
/// inline label visible, which exaggerated low percentages.
fn render_meters(frame: &mut Frame, area: Rect, section: &Section) {
    let rows = section_rows(section);
    if rows.is_empty() {
        return;
    }
    let constraints: Vec<Constraint> = (0..rows.len() + 2).map(|_| Constraint::Length(1)).collect();
    let chunks = Layout::vertical(constraints).split(area);
    frame.render_widget(section_header(section.title), chunks[0]);

    // Size the label column to the widest label, capped, so a full name like
    // "Weekly (Sonnet)" does not ellipsize when there is room for it.
    let widest = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0);
    let max_label_w = if area.width >= 100 {
        24
    } else if area.width >= 80 {
        20
    } else {
        16
    };
    let label_w = widest.clamp(8, max_label_w) as u16;
    let value_w = rows
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(0);
    // Below this width the trailing "Resets ..." column costs more than the
    // gauge it would squeeze.
    let trail_w: u16 = if area.width < 64 { 0 } else { 26 };
    // Two columns of gutter so the gauge does not butt up against the label.
    const GUTTER: u16 = 2;

    for (i, row) in rows.iter().enumerate() {
        let inner = chunks[i + 1].inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        let (label_slot, bar_slot, trail_slot) = if trail_w > 0 {
            let split = Layout::horizontal([
                Constraint::Length(label_w),
                Constraint::Length(GUTTER),
                Constraint::Min(20),
                Constraint::Length(trail_w),
            ])
            .split(inner);
            (split[0], split[2], Some(split[3]))
        } else {
            let split = Layout::horizontal([
                Constraint::Length(label_w),
                Constraint::Length(GUTTER),
                Constraint::Min(10),
            ])
            .split(inner);
            (split[0], split[2], None)
        };

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&row.label, label_w as usize),
                Style::default().fg(dim()).add_modifier(Modifier::BOLD),
            ))),
            label_slot,
        );

        let color = tone_color(row.tone);
        let bar_w = bar_slot.width.saturating_sub(value_w as u16 + 2) as usize;
        let filled = (row.fraction.unwrap_or(0.0).clamp(0.0, 1.0) * bar_w as f64).round() as usize;
        let filled = filled.min(bar_w);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("█".repeat(filled), Style::default().fg(color)),
                Span::styled("░".repeat(bar_w - filled), Style::default().fg(dim())),
                Span::styled(
                    format!(" {:>value_w$}", row.value),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])),
            bar_slot,
        );

        let Some(trail_area) = trail_slot else {
            continue;
        };
        // Reserve room for " · {badge}" so the pace projection never loses the
        // column to a long reset time.
        let footnote_budget = if row.badge.is_empty() {
            trail_w as usize - 1
        } else {
            (trail_w as usize).saturating_sub(row.badge.chars().count() + 4)
        };
        let mut spans = vec![Span::styled(
            truncate(&row.footnote, footnote_budget),
            Style::default().fg(dim()),
        )];
        if !row.badge.is_empty() {
            spans.push(Span::styled(
                format!(" · {}", row.badge),
                Style::default().fg(tone_color(row.badge_tone)),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), trail_area);
    }
}

/// One line per row with the share bar between the name and the value. Tokens
/// by model and tokens by device both land here.
fn render_bars(frame: &mut Frame, area: Rect, section: &Section) {
    let block = section_block(section.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = section_rows(section);
    if rows.is_empty() {
        return;
    }

    let name_cap = ((inner.width as usize).saturating_sub(20) / 2).clamp(8, 24);
    let name_w = rows
        .iter()
        .map(|r| truncate(&r.label, name_cap).chars().count())
        .max()
        .unwrap_or(8)
        .max(8);
    let value_w = rows
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(6);
    let suffix_w = rows
        .iter()
        .map(|r| r.suffix.chars().count())
        .max()
        .unwrap_or(0);

    let pad = "  ";
    let bar_room = (inner.width as usize)
        .saturating_sub(pad.len() + name_w + 2 + value_w + 2 + suffix_w)
        .max(4);

    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|row| {
            let filled =
                (row.fraction.unwrap_or(0.0).clamp(0.0, 1.0) * bar_room as f64).round() as usize;
            let filled = filled.min(bar_room);
            let name_style = if row.emphasized {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim())
            };
            let mut spans = vec![
                Span::raw(pad),
                Span::styled(
                    format!("{:<name_w$}", truncate(&row.label, name_w)),
                    name_style,
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{}{}", "█".repeat(filled), "░".repeat(bar_room - filled)),
                    Style::default().fg(green()),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("{:>value_w$}", row.value),
                    Style::default().fg(green()).add_modifier(Modifier::BOLD),
                ),
            ];
            if !row.suffix.is_empty() {
                spans.push(Span::styled(
                    format!("  {}", row.suffix),
                    Style::default().fg(dim()),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Label, value, tinted badge and dim suffix on one line, no bar. The cost
/// figures - and the sync health note that qualifies every one of them, which
/// is why it is drawn here and not only on the sync screen.
fn render_rows(frame: &mut Frame, area: Rect, section: &Section) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(area);
    frame.render_widget(section_header(section.title), chunks[0]);

    let rows = section_rows(section);
    let label_w = rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let value_w = rows
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(0);

    let lines: Vec<Line<'static>> = rows
        .iter()
        .map(|row| {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    format!("{:<label_w$}", row.label),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:>value_w$}", row.value),
                    Style::default().fg(green()),
                ),
            ];
            if !row.badge.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    row.badge.clone(),
                    Style::default().fg(tone_color(row.badge_tone)),
                ));
            }
            if !row.suffix.is_empty() {
                spans.push(Span::styled(
                    format!("  ·  {}", row.suffix),
                    Style::default().fg(dim()),
                ));
            }
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), chunks[1]);
}

/// The day rows as a bar chart. The day names and the amounts under them are
/// the spec's own strings, so nothing here derives a date from the wall clock -
/// the weekday letters used to be counted back from `now`, which relabelled the
/// whole week in a shell left open past midnight.
fn render_day_chart(frame: &mut Frame, area: Rect, section: &Section) {
    let block = section_block(section.title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = &section.rows;
    if rows.is_empty() || inner.width == 0 {
        return;
    }
    let n = rows.len() as u16;
    let gap: u16 = 1;
    let bar_width = (inner.width.saturating_sub(gap * (n - 1)) / n).max(3);
    let stride = bar_width + gap;

    // BarChart carries one label row; the amounts get a caption row below it.
    let split = Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).split(inner);
    let (bars_area, amounts_area) = (split[0], split[1]);

    // Charted on the share the spec resolved rather than the raw value, so the
    // tallest day fills the chart whatever unit is behind it.
    let bars: Vec<Bar> = rows
        .iter()
        .map(|row| {
            Bar::default()
                .value((row.fraction.unwrap_or(0.0).clamp(0.0, 1.0) * 1000.0).round() as u64)
                .label(Line::from(truncate(&row.label, bar_width as usize)))
                .text_value(String::new())
                .style(Style::default().fg(green()))
                .value_style(Style::default().fg(green()).bg(green()))
        })
        .collect();
    let group = BarGroup::default().bars(&bars);
    frame.render_widget(
        BarChart::default()
            .data(group)
            .max(1000)
            .bar_width(bar_width)
            .bar_gap(gap)
            .label_style(Style::default().fg(dim())),
        bars_area,
    );

    for (i, row) in rows.iter().enumerate() {
        let x = i as u16 * stride;
        if x >= amounts_area.width {
            break;
        }
        let cell = Rect {
            x: amounts_area.x + x,
            y: amounts_area.y,
            width: bar_width.min(amounts_area.width - x),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate(&row.suffix, cell.width as usize),
                Style::default().fg(green()).add_modifier(Modifier::BOLD),
            )))
            .alignment(Alignment::Center),
            cell,
        );
    }
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
        Span::styled("S", key),
        Span::styled(" sync", dim_s),
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
        binding_line("S", "fleet sync setup", key, desc),
        binding_line("?", "toggle this help", key, desc),
        binding_line("q / esc", "quit", key, desc),
        Line::from(""),
        Line::from(Span::styled(
            format!("  TokenGauge v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(dim()),
        )),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(draw: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 20)).expect("terminal");
        terminal.draw(draw).expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn provider_with_sync_note() -> ProviderRow {
        let cost: tokengauge_core::CostInfo = serde_json::from_value(serde_json::json!({
            "today_usd": 1.0,
            "today_tokens": 10,
            "monthly_usd": 9.0,
            "monthly_tokens": 90,
            "weekly_history": [
                {"date": "2026-08-24", "usd": 2.0, "tokens": 400},
                {"date": "2026-08-25", "usd": 5.0, "tokens": 900},
                {"date": "2026-08-26", "usd": 1.0, "tokens": 200},
            ],
            "monthly_models": [
                {"model": "claude-sonnet-4-5", "usd": 7.0, "tokens": 70},
                {"model": "claude-haiku-4-5", "usd": 2.0, "tokens": 20},
            ],
            "sync_note": {
                "devices": 2,
                "tone": "critical",
                "headline": "error",
                "detail": "bucket unreachable",
            },
            "by_device": [
                {"deviceId": "aaaa", "label": "desktop", "tokens": 1200000, "usd": 4.10,
                 "isLocal": true, "partial": false, "updatedAtMs": 0},
                {"deviceId": "bbbb", "label": "laptop", "tokens": 600000, "usd": 2.00,
                 "isLocal": false, "partial": false, "updatedAtMs": 0},
            ],
        }))
        .expect("cost");
        ProviderRow {
            provider: "claude".into(),
            session_used: Some(31),
            session_window_minutes: None,
            session_reset: "in 2h".into(),
            session_pace: None,
            weekly_used: None,
            weekly_window_minutes: None,
            weekly_reset: "—".into(),
            weekly_pace: None,
            tertiary_used: None,
            tertiary_reset: "—".into(),
            credits: "—".into(),
            source: "oauth".into(),
            updated: "now".into(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: Some(cost),
            stale: false,
        }
    }

    fn section(row: &ProviderRow, id: &str) -> Section {
        panel_spec(row)
            .into_iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("no `{id}` section"))
    }

    /// Sync configured but failing under-reports every figure in this section,
    /// so the provider view has to say so - not only the sync screen.
    #[test]
    fn the_cost_section_carries_the_sync_health_note() {
        let row = provider_with_sync_note();
        let cost = section(&row, "cost");
        let out = screen(|frame| render_rows(frame, frame.area(), &cost));
        assert!(out.contains("Sync"), "{out}");
        assert!(out.contains("2 devices"), "{out}");
        assert!(out.contains("error"), "{out}");
        assert!(out.contains("bucket unreachable"), "{out}");
    }

    #[test]
    fn a_merged_total_is_shown_broken_down_by_machine() {
        let row = provider_with_sync_note();
        let devices = section(&row, "tokens_by_device");
        let out = screen(|frame| render_bars(frame, frame.area(), &devices));
        assert!(out.contains("desktop"), "{out}");
        assert!(out.contains("laptop"), "{out}");
    }

    /// The drift this frontend used to carry: its own copies of the pace and
    /// trend thresholds, its own labels, and its own money formatter. Reading
    /// the spec's strings is what keeps it from happening again.
    #[test]
    fn the_panel_content_is_the_specs_and_not_this_files() {
        let row = provider_with_sync_note();
        let cost = section(&row, "cost");
        let out = screen(|frame| render_rows(frame, frame.area(), &cost));
        for label in ["Today", "This month"] {
            assert!(out.contains(label), "missing `{label}`:\n{out}");
        }
        assert!(!out.contains("Month "), "the old TUI-only heading survived");
    }

    /// Every kind the spec can hand over has a shape here. A new kind added to
    /// the core stops compiling this match rather than silently drawing blank.
    #[test]
    fn every_section_kind_has_a_height_and_a_renderer() {
        let row = provider_with_sync_note();
        for section in panel_spec(&row) {
            assert!(
                section_height(&section) > 0,
                "{} reserves no lines",
                section.id
            );
            let out = screen(|frame| render_section(frame, frame.area(), &section));
            assert!(
                out.trim()
                    .contains(section.title.split(' ').next().unwrap_or(section.title)),
                "{} drew no title:\n{out}",
                section.id
            );
        }
    }

    #[test]
    fn a_meter_paints_its_pace_badge_in_the_specs_tone() {
        let row = provider_with_sync_note();
        let limits = section(&row, "limits");
        assert_eq!(limits.kind, SectionKind::Meters);
        let out = screen(|frame| render_meters(frame, frame.area(), &limits));
        assert!(out.contains("31%"), "{out}");
        // Dropped rows stay dropped: a window the provider does not report has
        // no meter, rather than a permanently empty one.
        assert!(!out.contains("n/a"), "{out}");
    }
}
