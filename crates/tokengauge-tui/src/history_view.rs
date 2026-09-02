//! The history screen: a year of spend, as a chart.
//!
//! A second screen rather than another panel section, because a year of bars
//! does not belong above the limit gauges. Waybar reaches this one too - its
//! tooltip has no second screen and cannot have one, so left-click has opened
//! the TUI since long before there was any history to look at.
//!
//! Every string here comes from [`tokengauge_core::history`]. What this file
//! owns is the chart, which is the part a toolkit decides.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, BorderType, Borders, Clear, Paragraph};
use tokengauge_core::history::{HistoryPanel, HistorySeries};
use tokengauge_core::{load_config, provider_label};

use crate::theme::{self, dim, tone_color};

pub struct HistoryView {
    provider: String,
    label: String,
    panel: HistoryPanel,
    /// Index into `panel.series`, which is in `HISTORY_RANGES` order.
    range: usize,
    error: Option<String>,
}

impl std::fmt::Debug for HistoryView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryView")
            .field("provider", &self.provider)
            .field("range", &self.range)
            .finish()
    }
}

impl HistoryView {
    /// Build the panel once, on open.
    ///
    /// Not on every draw: the store is a file, and the answer only changes when
    /// a fetch writes one. Re-opening the screen is the refresh.
    pub fn open(config_override: Option<PathBuf>, provider: &str) -> Self {
        let label = provider_label(provider).to_string();
        let (config, error) = match load_config(config_override.clone()) {
            Ok(config) => (config, None),
            Err(e) => (
                tokengauge_core::TokenGaugeConfig::default(),
                Some(format!("{e:#}")),
            ),
        };

        if let Some(error) = error {
            return Self {
                provider: provider.to_string(),
                label,
                panel: empty_panel(),
                range: 0,
                error: Some(error),
            };
        }

        let (store, store_error) = tokengauge_core::sync::store::load(&config.cache_file);
        let prices = tokengauge_core::cost::pricing::load(
            &config.cache_file,
            std::time::Duration::from_secs(config.ccusage_timeout_secs),
            false,
        );
        let now = chrono::Local::now();
        let mut panel = tokengauge_core::history_panel(
            &store,
            provider,
            now.date_naive(),
            *now.offset(),
            &prices,
            tokengauge_core::cost::pricing::archive(),
        );
        if let Some(error) = store_error {
            panel.notes.push(error);
        }

        Self {
            provider: provider.to_string(),
            label,
            panel,
            range: 0,
            error: None,
        }
    }

    /// `false` closes the screen.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('H') => false,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.cycle(1);
                true
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.cycle(-1);
                true
            }
            _ => true,
        }
    }

    fn cycle(&mut self, by: isize) {
        let count = self.panel.series.len();
        if count == 0 {
            return;
        }
        let at = self.range as isize + by;
        self.range = at.rem_euclid(count as isize) as usize;
    }

    fn series(&self) -> Option<&HistorySeries> {
        self.panel.series.get(self.range)
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme::green()))
            .title(format!(" History — {} ", self.label));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 7 || inner.width < 20 {
            return;
        }

        let layout = Layout::vertical([
            Constraint::Length(1), // range selector
            Constraint::Length(1), // totals
            Constraint::Min(4),    // chart
            // Two lines: coverage and a note can both be true at once, and a
            // store that would not parse is the one that must not be cut off.
            Constraint::Length(2), // notes
            Constraint::Length(1), // keys
        ])
        .split(inner);

        frame.render_widget(self.range_line(), layout[0]);
        frame.render_widget(self.totals_line(), layout[1]);
        self.render_chart(frame, layout[2]);
        frame.render_widget(self.notes_line(), layout[3]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "h/l or Tab  range    q/Esc  back",
                Style::default().fg(dim()),
            ))),
            layout[4],
        );
    }

    fn range_line(&self) -> Paragraph<'_> {
        let mut spans = Vec::new();
        for (at, series) in self.panel.series.iter().enumerate() {
            if at > 0 {
                spans.push(Span::styled("  ", Style::default().fg(dim())));
            }
            let style = if at == self.range {
                Style::default()
                    .fg(theme::green())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim())
            };
            spans.push(Span::styled(series.label, style));
        }
        Paragraph::new(Line::from(spans))
    }

    fn totals_line(&self) -> Paragraph<'_> {
        if let Some(error) = &self.error {
            return Paragraph::new(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(tone_color(tokengauge_core::Tone::Critical)),
            )));
        }
        let Some(series) = self.series() else {
            return Paragraph::new(Line::default());
        };
        Paragraph::new(Line::from(vec![
            Span::styled(series.total_usd.clone(), Style::default().fg(Color::White)),
            Span::styled(
                format!("  {} tokens", series.total_tokens),
                Style::default().fg(dim()),
            ),
            Span::styled(
                format!("    avg {}/step", series.average_usd),
                Style::default().fg(dim()),
            ),
        ]))
    }

    fn notes_line(&self) -> Paragraph<'_> {
        // Every note, not the first: a store that would not parse and a range
        // older than the price archive are both true at once, and the one that
        // got dropped was whichever came second.
        let mut parts = vec![self.panel.covers.clone()];
        parts.extend(self.panel.notes.iter().cloned());
        Paragraph::new(Line::from(Span::styled(
            parts.join(" — "),
            Style::default().fg(dim()),
        )))
        .wrap(ratatui::widgets::Wrap { trim: true })
    }

    fn render_chart(&self, frame: &mut Frame, area: Rect) {
        let Some(series) = self.series() else {
            return;
        };
        if series.empty {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "nothing spent in this range",
                    Style::default().fg(dim()),
                ))),
                area,
            );
            return;
        }

        let total = series.points.len().max(1) as u16;
        // Wide bars carry a label; narrow ones cannot, and 90 days of them at
        // three columns each would not fit any terminal worth drawing on.
        let gap: u16 = if total <= 12 { 1 } else { 0 };
        let bar_width = (area
            .width
            .saturating_sub(gap * total.saturating_sub(1))
            .checked_div(total)
            .unwrap_or(1))
        .max(1);
        let stride = bar_width + gap;

        // More steps than columns: show the most recent that fit rather than
        // squeezing a year into a scrollbar. The header still names the range.
        let capacity = (area.width / stride).max(1) as usize;
        let points = &series.points[series.points.len().saturating_sub(capacity)..];

        let labelled = bar_width >= 3;
        let bars: Vec<Bar> = points
            .iter()
            .map(|point| {
                let colour = tone_color(point.tone);
                Bar::default()
                    // Charted on the resolved share, so the tallest step fills
                    // the chart whatever the unit behind it is.
                    .value((point.fraction.clamp(0.0, 1.0) * 1000.0).round() as u64)
                    .label(Line::from(if labelled {
                        truncate(&point.label, bar_width as usize)
                    } else {
                        String::new()
                    }))
                    .text_value(String::new())
                    .style(Style::default().fg(colour))
                    .value_style(Style::default().fg(colour).bg(colour))
            })
            .collect();

        frame.render_widget(
            BarChart::default()
                .data(BarGroup::default().bars(&bars))
                .max(1000)
                .bar_width(bar_width)
                .bar_gap(gap)
                .label_style(Style::default().fg(dim())),
            area,
        );
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars().take(width).collect()
}

fn empty_panel() -> HistoryPanel {
    HistoryPanel {
        series: Vec::new(),
        covers: String::new(),
        notes: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokengauge_core::history::{HistoryPoint, HistorySeries};

    fn point(label: &str, usd: &str, fraction: f64, partial: bool) -> HistoryPoint {
        HistoryPoint {
            key: label.to_string(),
            label: label.to_string(),
            full_label: label.to_string(),
            usd: usd.to_string(),
            tokens: "1.0M".into(),
            fraction,
            partial,
            tone: if partial {
                tokengauge_core::Tone::Dim
            } else {
                tokengauge_core::Tone::Normal
            },
        }
    }

    fn view() -> HistoryView {
        HistoryView {
            provider: "claude".into(),
            label: "Claude".into(),
            panel: HistoryPanel {
                series: vec![
                    HistorySeries {
                        id: "30d",
                        label: "30 days",
                        points: vec![point("1 Aug", "$10.00", 1.0, false)],
                        total_usd: "$10.00".into(),
                        total_tokens: "1.0M".into(),
                        average_usd: "$10.00".into(),
                        empty: false,
                    },
                    HistorySeries {
                        id: "12m",
                        label: "12 months",
                        points: vec![
                            point("Jul", "$92.10", 1.0, false),
                            point("Aug", "$41.20", 0.45, true),
                        ],
                        total_usd: "$133.30".into(),
                        total_tokens: "12.0M".into(),
                        average_usd: "$92.10".into(),
                        empty: false,
                    },
                ],
                covers: "since 1 Jan 2026".into(),
                notes: Vec::new(),
            },
            range: 0,
            error: None,
        }
    }

    fn screen(view: &HistoryView) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, 16)).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn the_screen_names_the_provider_the_range_and_what_it_covers() {
        let out = screen(&view());
        assert!(out.contains("Claude"), "{out}");
        assert!(out.contains("30 days"), "{out}");
        assert!(out.contains("12 months"), "the other ranges are offered");
        assert!(out.contains("$10.00"), "the total is drawn");
        assert!(out.contains("since 1 Jan 2026"), "coverage is drawn");
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut view = view();
        assert_eq!(view.series().expect("a series").id, "30d");
        view.cycle(1);
        assert_eq!(view.series().expect("a series").id, "12m");
        view.cycle(1);
        assert_eq!(view.series().expect("a series").id, "30d", "forward wraps");
        view.cycle(-1);
        assert_eq!(view.series().expect("a series").id, "12m", "back wraps");
    }

    #[test]
    fn an_empty_range_says_so_rather_than_drawing_an_empty_chart() {
        let mut view = view();
        view.panel.series[0].empty = true;
        let out = screen(&view);
        assert!(out.contains("nothing spent in this range"), "{out}");
    }

    #[test]
    fn esc_closes_and_a_range_key_does_not() {
        let mut view = view();
        assert!(!view.on_key(KeyEvent::from(KeyCode::Esc)));
        assert!(view.on_key(KeyEvent::from(KeyCode::Tab)));
        assert!(!view.on_key(KeyEvent::from(KeyCode::Char('H'))));
    }
}
