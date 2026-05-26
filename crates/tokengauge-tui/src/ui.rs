use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use tokengauge_core::{
    CostInfo, ExtraWindowRow, ModelCost, ProviderRow, format_tokens, format_updated_relative,
    sparkline, theme, window_labels,
};

use crate::app::AppState;
use crate::theme::{color_for, dim, green, hex_to_color, provider_icon_color};

const MIN_BAR_WIDTH: usize = 12;
const MAX_BAR_WIDTH: usize = 200;
const LEFT_PAD: usize = 3;

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn render_bar(percent: u8, width: usize) -> (String, Color) {
    let pct = percent.min(100);
    let width = width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH);
    let filled = (pct as usize * width).div_ceil(100);
    let empty = width.saturating_sub(filled);
    (
        format!("{}{}", "━".repeat(filled), "─".repeat(empty)),
        color_for(pct),
    )
}

fn window_section(
    label: &str,
    used: Option<u8>,
    reset: &str,
    bar_width: usize,
) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let title = Line::from(Span::styled(
        format!("{pad}{label}"),
        Style::default().fg(dim()).add_modifier(Modifier::BOLD),
    ));
    let body: Vec<Line<'static>> = match used {
        Some(pct) => {
            let (bar, color) = render_bar(pct, bar_width);
            let reset_text = if reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {reset}")
            };
            vec![
                Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(bar, Style::default().fg(color)),
                ]),
                Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(
                        format!("{pct}% used"),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("   "),
                    Span::styled(reset_text, Style::default().fg(dim())),
                ]),
            ]
        }
        None => vec![
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    "─".repeat(bar_width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)),
                    Style::default().fg(dim()),
                ),
            ]),
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled("no data", Style::default().fg(dim())),
            ]),
        ],
    };
    std::iter::once(title).chain(body).collect()
}

fn extra_window_lines(extra: &ExtraWindowRow, bar_width: usize) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let title_line = Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            truncate(&extra.title, 32),
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        ),
    ]);
    let body: Vec<Line<'static>> = match extra.used {
        Some(pct) => {
            let (bar, color) = render_bar(pct, bar_width);
            let trailing = match (extra.reset.as_str(), pct) {
                ("—", 0) => None,
                ("—", _) => Some("not started".to_string()),
                (other, _) => Some(format!("resets {other}")),
            };
            let trailing_spans = trailing
                .map(|text| {
                    vec![
                        Span::raw("   "),
                        Span::styled(text, Style::default().fg(dim())),
                    ]
                })
                .unwrap_or_default();
            let percent_spans = [
                Span::raw(pad.clone()),
                Span::styled(
                    format!("{pct}% used"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ];
            vec![
                Line::from(vec![
                    Span::raw(pad.clone()),
                    Span::styled(bar, Style::default().fg(color)),
                ]),
                Line::from(
                    percent_spans
                        .into_iter()
                        .chain(trailing_spans)
                        .collect::<Vec<_>>(),
                ),
            ]
        }
        None => vec![
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    "─".repeat(bar_width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)),
                    Style::default().fg(dim()),
                ),
            ]),
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled("no data", Style::default().fg(dim())),
            ]),
        ],
    };
    std::iter::once(title_line).chain(body).collect()
}

fn cost_lines(cost: &CostInfo) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let sub_pad = " ".repeat(LEFT_PAD + 2);

    let all_models: Vec<&ModelCost> = cost
        .today_models
        .iter()
        .chain(cost.monthly_models.iter())
        .collect();
    let model_w = all_models
        .iter()
        .map(|m| truncate(&m.model, 28).chars().count())
        .max()
        .unwrap_or(0);
    let model_usd_w = all_models
        .iter()
        .map(|m| format!("${:.2}", m.usd).chars().count())
        .max()
        .unwrap_or(0);
    let model_tokens_w = all_models
        .iter()
        .map(|m| format_tokens(m.tokens).chars().count())
        .max()
        .unwrap_or(0);

    let today_usd_str = format!("${:.2}", cost.today_usd);
    let monthly_usd_str = format!("${:.2}", cost.monthly_usd);
    let session_usd_str = format!("${:.2}", cost.session_usd);
    let weekly_usd_str = format!("${:.2}", cost.weekly_usd);
    let total_usd_w = today_usd_str
        .chars()
        .count()
        .max(monthly_usd_str.chars().count())
        .max(session_usd_str.chars().count())
        .max(weekly_usd_str.chars().count());
    let today_tokens_str = format_tokens(cost.today_tokens);
    let monthly_tokens_str = format_tokens(cost.monthly_tokens);
    let total_tokens_w = today_tokens_str
        .chars()
        .count()
        .max(monthly_tokens_str.chars().count());

    let label_w = 7usize;

    let totals_line = |label: &str, usd_str: &str, tokens_str: &str| {
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                format!("{label:<label_w$}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("{usd_str:>total_usd_w$}"),
                Style::default().fg(green()),
            ),
            Span::styled(
                format!("  ·  {tokens_str:>total_tokens_w$} tokens"),
                Style::default().fg(dim()),
            ),
        ])
    };
    let model_line = |m: &ModelCost| {
        let name = truncate(&m.model, 28);
        let usd = format!("${:.2}", m.usd);
        let tokens = format_tokens(m.tokens);
        Line::from(vec![
            Span::raw(sub_pad.clone()),
            Span::styled(format!("{name:<model_w$}"), Style::default().fg(dim())),
            Span::raw("  "),
            Span::styled(
                format!("{usd:>model_usd_w$}"),
                Style::default().fg(green()),
            ),
            Span::styled(
                format!("  ·  {tokens:>model_tokens_w$}"),
                Style::default().fg(dim()),
            ),
        ])
    };
    let window_line = |label: &str, usd_str: &str| {
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                format!("{label:<label_w$}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("{usd_str:>total_usd_w$}"),
                Style::default().fg(green()),
            ),
        ])
    };

    let rate_line = cost.burn_rate.as_ref().map(|br| {
        let rate_str = format!("${:.2}", br.cost_per_hour);
        let base = vec![
            Span::raw(pad.clone()),
            Span::styled(
                format!("{:<label_w$}", "Rate"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled(
                format!("{rate_str:>total_usd_w$}/hr"),
                Style::default().fg(green()),
            ),
        ];
        let trend_spans = cost
            .avg_hourly_cost()
            .filter(|avg| *avg > 0.0)
            .map(|avg| {
                let pct = ((br.cost_per_hour - avg) / avg) * 100.0;
                let arrow = if pct >= 0.0 { "↑" } else { "↓" };
                let trend_color = if pct >= 25.0 {
                    Color::Rgb(0xf3, 0x8b, 0xa8)
                } else if pct >= -10.0 {
                    hex_to_color(&theme().yellow)
                } else {
                    green()
                };
                vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{arrow}{:.0}%", pct.abs()),
                        Style::default().fg(trend_color),
                    ),
                    Span::styled(" vs 7d avg".to_string(), Style::default().fg(dim())),
                ]
            })
            .unwrap_or_default();
        Line::from(base.into_iter().chain(trend_spans).collect::<Vec<_>>())
    });

    let session_line =
        (cost.session_usd > 0.0).then(|| window_line("Session", &session_usd_str));
    let weekly_line = (cost.weekly_usd > 0.0).then(|| window_line("Weekly", &weekly_usd_str));
    let blank = (session_line.is_some() || weekly_line.is_some()).then(|| Line::from(""));

    let today_block = std::iter::once(totals_line("Today", &today_usd_str, &today_tokens_str))
        .chain(cost.today_models.iter().map(&model_line));
    let month_block = std::iter::once(totals_line(
        "Month",
        &monthly_usd_str,
        &monthly_tokens_str,
    ))
    .chain(cost.monthly_models.iter().map(&model_line));

    let spark_line = (!cost.weekly_cost_history.is_empty()).then(|| {
        let spark = sparkline(&cost.weekly_cost_history);
        let max = cost
            .weekly_cost_history
            .iter()
            .copied()
            .fold(0.0_f64, f64::max);
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("7d", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("         "),
            Span::styled(spark, Style::default().fg(green())),
            Span::styled(format!("  peak ${max:.2}"), Style::default().fg(dim())),
        ])
    });

    rate_line
        .into_iter()
        .chain(session_line)
        .chain(weekly_line)
        .chain(blank)
        .chain(today_block)
        .chain(month_block)
        .chain(spark_line)
        .collect()
}

fn provider_card_lines(row: &ProviderRow, inner_width: u16) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let bar_width = (inner_width as usize).saturating_sub(LEFT_PAD * 2 + 2);

    let (icon, icon_color) = provider_icon_color(&row.provider);
    let plan_span = row
        .plan_label
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|plan| Span::styled(format!("  ·  {plan}"), Style::default().fg(dim())));
    let header_line = Line::from(
        [
            Span::raw(pad.clone()),
            Span::styled(format!("{icon}  "), Style::default().fg(icon_color)),
            Span::styled(
                row.provider.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]
        .into_iter()
        .chain(plan_span)
        .collect::<Vec<_>>(),
    );

    let updated_line = row
        .updated_iso
        .as_deref()
        .and_then(format_updated_relative)
        .map(|rel| {
            Line::from(vec![
                Span::raw(pad.clone()),
                Span::raw("   "),
                Span::styled(format!("Updated {rel}"), Style::default().fg(dim())),
            ])
        });

    let (session_label, weekly_label, tertiary_label) = window_labels(&row.provider);
    let window_block = |label: &'static str, used: Option<u8>, reset: &str| {
        std::iter::once(Line::from("")).chain(window_section(label, used, reset, bar_width))
    };
    let session_block = window_block(session_label, row.session_used, &row.session_reset);
    let weekly_block = window_block(weekly_label, row.weekly_used, &row.weekly_reset);
    let tertiary_block: Box<dyn Iterator<Item = Line<'static>>> =
        if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
            Box::new(window_block(
                tertiary_label,
                row.tertiary_used,
                &row.tertiary_reset,
            ))
        } else {
            Box::new(std::iter::empty())
        };

    let extras_block: Box<dyn Iterator<Item = Line<'static>>> = if row.extra_windows.is_empty() {
        Box::new(std::iter::empty())
    } else {
        let pad_for_extras = pad.clone();
        let header = std::iter::once(Line::from("")).chain(std::iter::once(Line::from(vec![
            Span::raw(pad_for_extras),
            Span::styled(
                "Extra usage",
                Style::default().fg(dim()).add_modifier(Modifier::BOLD),
            ),
        ])));
        let entries: Vec<Line<'static>> = row
            .extra_windows
            .iter()
            .flat_map(|extra| {
                std::iter::once(Line::from("")).chain(extra_window_lines(extra, bar_width))
            })
            .collect();
        Box::new(header.chain(entries))
    };

    let cost_block: Box<dyn Iterator<Item = Line<'static>>> = match &row.cost {
        Some(cost) => {
            let pad_for_cost = pad.clone();
            let head = std::iter::once(Line::from("")).chain(std::iter::once(Line::from(vec![
                Span::raw(pad_for_cost),
                Span::styled(
                    "Cost",
                    Style::default().fg(dim()).add_modifier(Modifier::BOLD),
                ),
            ])));
            Box::new(head.chain(cost_lines(cost)))
        }
        None => Box::new(std::iter::empty()),
    };

    let credits_block: Box<dyn Iterator<Item = Line<'static>>> =
        if row.credits != "—" && !row.credits.is_empty() {
            let pad_for_credits = pad.clone();
            let credits = row.credits.clone();
            Box::new(
                std::iter::once(Line::from("")).chain(std::iter::once(Line::from(vec![
                    Span::raw(pad_for_credits),
                    Span::styled("Credits", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw("    "),
                    Span::styled(format!("${credits}"), Style::default().fg(green())),
                ]))),
            )
        } else {
            Box::new(std::iter::empty())
        };

    std::iter::once(header_line)
        .chain(updated_line)
        .chain(session_block)
        .chain(weekly_block)
        .chain(tertiary_block)
        .chain(extras_block)
        .chain(cost_block)
        .chain(credits_block)
        .collect()
}

fn tab_titles(rows: &[ProviderRow], active: usize) -> Vec<Line<'static>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let (icon, color) = provider_icon_color(&row.provider);
            let is_active = i == active;
            let icon_style = if is_active {
                Style::default().fg(color)
            } else {
                Style::default().fg(dim())
            };
            let name_style = if is_active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(dim())
            };
            Line::from(vec![
                Span::styled(icon.to_string(), icon_style),
                Span::raw("  "),
                Span::styled(row.provider.clone(), name_style),
            ])
        })
        .collect()
}

pub fn draw(frame: &mut Frame, state: &mut AppState, is_refreshing: bool) {
    let size = frame.area();

    let has_errors = !state.errors.is_empty();
    let error_height = if has_errors {
        (state.errors.len() as u16 + 1 + 2).min(8)
    } else {
        0
    };

    let layout = if has_errors {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(error_height),
            Constraint::Length(3),
        ])
        .split(size)
    } else {
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(size)
    };

    render_header(frame, layout[0], state, is_refreshing);
    render_body(frame, layout[1], state);
    if has_errors {
        render_errors(frame, layout[2], state);
    }
    let footer_index = if has_errors { 3 } else { 2 };
    render_footer(frame, layout[footer_index], state);
}

fn render_header(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &AppState,
    is_refreshing: bool,
) {
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[state.spinner_index % spinner_frames.len()];
    let label = if is_refreshing {
        "Refreshing"
    } else {
        "TokenGauge Usage"
    };
    let text = if is_refreshing {
        format!("{spinner} {label}")
    } else {
        label.to_string()
    };
    let header = Paragraph::new(text)
        .style(
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title("TokenGauge"));
    frame.render_widget(header, area);
}

fn render_body(frame: &mut Frame, area: ratatui::layout::Rect, state: &mut AppState) {
    if state.rows.is_empty() && state.errors.is_empty() {
        let message = state
            .status_message
            .as_deref()
            .or(state.last_error.as_deref())
            .unwrap_or("No providers returned");
        let empty = Paragraph::new(message)
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title("Usage"));
        frame.render_widget(empty, area);
        state.content_height = 0;
        state.viewport_height = area.height.saturating_sub(2);
        return;
    }

    let outer = Block::default().borders(Borders::ALL).title("Usage");
    let inner_area = outer.inner(area);
    frame.render_widget(outer, area);

    let panes =
        Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(inner_area);

    let tabs = Tabs::new(tab_titles(&state.rows, state.active_tab))
        .select(state.active_tab)
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(Style::default())
        .divider("  ");
    frame.render_widget(tabs, panes[0]);

    let card_area = panes[1];
    let row = &state.rows[state.active_tab];
    let lines = provider_card_lines(row, card_area.width);
    state.content_height = lines.len() as u16;
    state.viewport_height = card_area.height;
    if state.scroll > state.max_scroll() {
        state.scroll = state.max_scroll();
    }
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0));
    frame.render_widget(paragraph, card_area);
}

fn render_errors(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let error_lines: Vec<Line> = state
        .errors
        .iter()
        .map(|err| {
            Line::from(vec![
                Span::styled(
                    format!("{}: ", err.provider),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate(&err.message, 60),
                    Style::default().fg(Color::LightRed),
                ),
            ])
        })
        .chain(std::iter::once(Line::from(Span::styled(
            format!("Full details: {}", state.cache_file.display()),
            Style::default().fg(Color::DarkGray),
        ))))
        .collect();

    let widget = Paragraph::new(error_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Errors")
            .border_style(Style::default().fg(Color::Red)),
    );
    frame.render_widget(widget, area);
}

fn render_footer(frame: &mut Frame, area: ratatui::layout::Rect, state: &AppState) {
    let status_text = state.status_message.as_deref().unwrap_or("Idle");
    let status_color = if state.status_message.is_some() {
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
        Span::styled("r", key),
        Span::styled(" refresh", dim_s),
        Span::styled(" | ", sep),
        Span::styled("h/l", key),
        Span::styled(" tabs", dim_s),
        Span::styled(" | ", sep),
        Span::styled("u", key),
        Span::styled(" dashboard", dim_s),
        Span::styled(" | ", sep),
        Span::styled("s", key),
        Span::styled(" status", dim_s),
        Span::styled(" | ", sep),
        Span::styled("q/esc", key),
        Span::styled(" quit", dim_s),
        Span::styled(" | ", sep),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let footer = Paragraph::new(line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}
