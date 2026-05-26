use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, anyhow};
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs, Wrap};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokengauge_core::{
    CostInfo, ExtraWindowRow, FetchResult, ModelCost, ProviderFetchError, ProviderRow,
    fetch_all_providers, format_tokens, format_updated_relative, load_config, parse_hex_rgb,
    payload_to_rows_with_costs, provider_icon as core_provider_icon, provider_urls, read_cache_full,
    read_waybar_state, sparkline, theme, waybar_state_path, window_labels, write_cache_full,
    write_default_config,
};

const MIN_BAR_WIDTH: usize = 12;
const MAX_BAR_WIDTH: usize = 200;
const LEFT_PAD: usize = 3;

fn hex_to_color(hex: &str) -> Color {
    match parse_hex_rgb(hex) {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::White,
    }
}

fn dim() -> Color {
    hex_to_color(&theme().dim)
}

fn green() -> Color {
    hex_to_color(&theme().green)
}

#[derive(Parser, Debug)]
#[command(version, about = "TokenGauge TUI")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Debug)]
struct AppState {
    rows: Vec<ProviderRow>,
    errors: Vec<ProviderFetchError>,
    cache_file: PathBuf,
    last_refresh: Instant,
    last_error: Option<String>,
    status_message: Option<String>,
    spinner_index: usize,
    scroll: u16,
    content_height: u16,
    viewport_height: u16,
    active_tab: usize,
    initial_provider: Option<String>,
}

impl AppState {
    fn new(cache_file: PathBuf) -> Self {
        Self {
            rows: Vec::new(),
            errors: Vec::new(),
            cache_file,
            last_refresh: Instant::now(),
            last_error: None,
            status_message: None,
            spinner_index: 0,
            scroll: 0,
            content_height: 0,
            viewport_height: 0,
            active_tab: 0,
            initial_provider: None,
        }
    }

    fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport_height)
    }

    fn scroll_by(&mut self, delta: i32) {
        let new = (self.scroll as i32 + delta).max(0) as u16;
        self.scroll = new.min(self.max_scroll());
    }

    fn next_tab(&mut self) {
        if !self.rows.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.rows.len();
            self.scroll = 0;
        }
    }

    fn prev_tab(&mut self) {
        if !self.rows.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.rows.len() - 1
            } else {
                self.active_tab - 1
            };
            self.scroll = 0;
        }
    }

    fn clamp_active_tab(&mut self) {
        if self.rows.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.rows.len() {
            self.active_tab = self.rows.len() - 1;
        }
    }
}

/// Result of a refresh operation.
struct RefreshResult {
    rows: Vec<ProviderRow>,
    errors: Vec<ProviderFetchError>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let stdout = io::stdout();
    if !crossterm::tty::IsTty::is_tty(&stdout) {
        return Err(anyhow!("tokengauge-tui must run in a TTY"));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_app(&mut terminal, &args);

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, args: &Args) -> Result<()> {
    // Load config for cache file path and primary provider
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);
    let loaded_config = if config_path.exists() {
        let cfg = load_config(Some(config_path)).ok();
        if let Some(c) = &cfg {
            tokengauge_core::install_theme(c.theme.resolve());
        }
        cfg
    } else {
        None
    };
    let cache_file = loaded_config
        .as_ref()
        .map(|c| c.cache_file.clone())
        .unwrap_or_else(|| PathBuf::from("/tmp/tokengauge-usage.json"));
    let config_primary = loaded_config.and_then(|c| c.waybar.primary);

    let mut state = AppState::new(cache_file.clone());
    state.initial_provider = read_waybar_state(&waybar_state_path(&cache_file))
        .selected
        .or(config_primary);
    let mut pending_refresh = Some(spawn_refresh(args, false));
    let mut last_cache_poll = Instant::now();

    loop {
        if let Some(receiver) = pending_refresh.as_ref() {
            match receiver.try_recv() {
                Ok(result) => {
                    apply_refresh_result(&mut state, result);
                    pending_refresh = None;
                }
                Err(TryRecvError::Empty) => {
                    state.spinner_index = state.spinner_index.wrapping_add(1);
                }
                Err(TryRecvError::Disconnected) => {
                    state.last_error = Some("refresh thread disconnected".to_string());
                    state.status_message = None;
                    pending_refresh = None;
                }
            }
        }

        if pending_refresh.is_none() && last_cache_poll.elapsed() >= Duration::from_secs(60) {
            last_cache_poll = Instant::now();
            if let Ok(config) = load_config(args.config.clone())
                && let Ok(cached) = read_cache_full(&config.cache_file)
            {
                let (payloads, errors, costs) = cached.into_parts();
                state.rows = payload_to_rows_with_costs(payloads, &costs);
                state.errors = errors;
                state.last_error = None;
            }
        }

        terminal.draw(|frame| draw_ui(frame, &mut state, pending_refresh.is_some()))?;

        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
        {
            if should_exit(key) {
                break;
            }
            if matches!(key.code, KeyCode::Char('r')) && pending_refresh.is_none() {
                state.status_message = Some("Refreshing…".to_string());
                pending_refresh = Some(spawn_refresh(args, true));
            }
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => state.scroll_by(1),
                KeyCode::Char('k') | KeyCode::Up => state.scroll_by(-1),
                KeyCode::PageDown => state.scroll_by(state.viewport_height as i32),
                KeyCode::PageUp => state.scroll_by(-(state.viewport_height as i32)),
                KeyCode::Char('g') | KeyCode::Home => state.scroll = 0,
                KeyCode::Char('G') | KeyCode::End => state.scroll = state.max_scroll(),
                KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => state.next_tab(),
                KeyCode::Char('h') | KeyCode::Left | KeyCode::BackTab => state.prev_tab(),
                KeyCode::Char('u') => open_active_url(&state, OpenWhich::Dashboard),
                KeyCode::Char('s') => open_active_url(&state, OpenWhich::Status),
                _ => {}
            }
        }

        if pending_refresh.is_none()
            && let Ok(config) = load_config(args.config.clone())
            && state.last_refresh.elapsed() >= Duration::from_secs(config.refresh_secs)
        {
            pending_refresh = Some(spawn_refresh(args, false));
        }
    }

    Ok(())
}

fn apply_refresh_result(state: &mut AppState, result: Result<RefreshResult>) {
    match result {
        Ok(refresh) => {
            state.rows = refresh.rows;
            state.errors = refresh.errors;
            state.last_error = None;
        }
        Err(error) => {
            state.rows.clear();
            state.errors.clear();
            state.last_error = Some(error.to_string());
        }
    }
    if let Some(provider) = state.initial_provider.take() {
        let lower = provider.to_lowercase();
        if let Some(idx) = state
            .rows
            .iter()
            .position(|r| r.provider.to_lowercase() == lower)
        {
            state.active_tab = idx;
        }
    }
    state.clamp_active_tab();
    state.last_refresh = Instant::now();
    state.status_message = None;
}

fn spawn_refresh(args: &Args, force: bool) -> Receiver<Result<RefreshResult>> {
    let config_override = args.config.clone();
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        let result = fetch_rows_with_config(config_override, force);
        let _ = sender.send(result);
    });

    receiver
}

#[derive(Clone, Copy)]
enum OpenWhich {
    Dashboard,
    Status,
}

fn open_active_url(state: &AppState, which: OpenWhich) {
    let Some(row) = state.rows.get(state.active_tab) else {
        return;
    };
    let urls = provider_urls(&row.provider);
    let url = match which {
        OpenWhich::Dashboard => urls.dashboard,
        OpenWhich::Status => urls.status,
    };
    let Some(url) = url else { return };
    use std::process::{Command, Stdio};
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn should_exit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
}

fn fetch_rows_with_config(config_override: Option<PathBuf>, force: bool) -> Result<RefreshResult> {
    let config_path = config_override.unwrap_or_else(tokengauge_core::default_config_path);
    if !config_path.exists() {
        write_default_config(&config_path)?;
    }

    let config = load_config(Some(config_path))?;

    // Try to read from cache first
    let cached = read_cache_full(&config.cache_file).ok();

    // Determine if we need to refresh
    let stale = match fs::metadata(&config.cache_file) {
        Ok(metadata) => metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map(|age| age >= Duration::from_secs(config.refresh_secs))
            .unwrap_or(true),
        Err(_) => true,
    };

    let (payloads, errors, costs) = match cached {
        Some(cached) if !force && !stale => cached.into_parts(),
        _ => {
            let FetchResult {
                payloads,
                errors,
                costs,
            } = fetch_all_providers(&config);
            write_cache_full(&config.cache_file, &payloads, &errors, &costs).ok();
            (payloads, errors, costs)
        }
    };

    let rows = payload_to_rows_with_costs(payloads, &costs);
    Ok(RefreshResult { rows, errors })
}

fn color_for(percent: u8) -> Color {
    hex_to_color(theme().color_for_percent(percent))
}

fn provider_icon_color(label: &str) -> (&'static str, Color) {
    let icon = core_provider_icon(label);
    (icon.glyph, hex_to_color(icon.color_hex))
}

fn render_bar(percent: u8, width: usize) -> (String, Color) {
    let pct = percent.min(100);
    let width = width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH);
    let filled = (pct as usize * width).div_ceil(100);
    let empty = width.saturating_sub(filled);
    let bar = format!("{}{}", "━".repeat(filled), "─".repeat(empty));
    (bar, color_for(pct))
}

fn window_section(label: &str, used: Option<u8>, reset: &str, bar_width: usize) -> Vec<Line<'static>> {
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


fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
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

    let today_block = std::iter::once(totals_line(
        "Today",
        &today_usd_str,
        &today_tokens_str,
    ))
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

fn draw_ui(frame: &mut ratatui::Frame, state: &mut AppState, is_refreshing: bool) {
    let size = frame.area();

    // Calculate layout based on whether we have errors
    let has_errors = !state.errors.is_empty();
    let error_height = if has_errors {
        // 1 line per error + 1 for hint + 2 for borders, max 8 lines
        (state.errors.len() as u16 + 1 + 2).min(8)
    } else {
        0
    };

    let layout = if has_errors {
        Layout::vertical([
            Constraint::Length(3),            // Header
            Constraint::Min(0),               // Usage table
            Constraint::Length(error_height), // Errors section
            Constraint::Length(3),            // Footer
        ])
        .split(size)
    } else {
        Layout::vertical([
            Constraint::Length(3), // Header
            Constraint::Min(0),    // Usage table
            Constraint::Length(3), // Footer
        ])
        .split(size)
    };

    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = spinner_frames[state.spinner_index % spinner_frames.len()];
    let header_label = if is_refreshing {
        "Refreshing"
    } else {
        "TokenGauge Usage"
    };
    let header_text = if is_refreshing {
        format!("{} {}", spinner, header_label)
    } else {
        header_label.to_string()
    };

    let header = Paragraph::new(header_text)
        .style(
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title("TokenGauge"));
    frame.render_widget(header, layout[0]);

    let body_area = layout[1];
    if state.rows.is_empty() && state.errors.is_empty() {
        let message = state
            .status_message
            .as_deref()
            .or(state.last_error.as_deref())
            .unwrap_or("No providers returned");
        let empty = Paragraph::new(message)
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title("Usage"));
        frame.render_widget(empty, body_area);
        state.content_height = 0;
        state.viewport_height = body_area.height.saturating_sub(2);
    } else {
        let outer = Block::default().borders(Borders::ALL).title("Usage");
        let inner_area = outer.inner(body_area);
        frame.render_widget(outer, body_area);

        let panes = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(inner_area);

        let tabs = Tabs::new(tab_titles(&state.rows, state.active_tab))
            .select(state.active_tab)
            .block(Block::default().borders(Borders::BOTTOM))
            .highlight_style(Style::default())
            .divider("  ");
        frame.render_widget(tabs, panes[0]);

        let card_area = panes[1];
        let inner_width = card_area.width;
        let inner_height = card_area.height;
        let row = &state.rows[state.active_tab];
        let lines = provider_card_lines(row, inner_width);
        state.content_height = lines.len() as u16;
        state.viewport_height = inner_height;
        if state.scroll > state.max_scroll() {
            state.scroll = state.max_scroll();
        }
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((state.scroll, 0));
        frame.render_widget(paragraph, card_area);
    }

    // Render errors section if there are errors
    if has_errors {
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

        let errors_widget = Paragraph::new(error_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Errors")
                .border_style(Style::default().fg(Color::Red)),
        );
        frame.render_widget(errors_widget, layout[2]);
    }

    let footer_index = if has_errors { 3 } else { 2 };
    let status_text = state.status_message.as_deref().unwrap_or("Idle");
    let status_color = if state.status_message.is_some() {
        Color::Yellow
    } else {
        Color::DarkGray
    };

    let key_style = Style::default()
        .fg(Color::LightCyan)
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(Color::Gray);
    let sep_style = Style::default().fg(Color::DarkGray);
    let footer_line = Line::from(vec![
        Span::styled("r", key_style),
        Span::styled(" refresh", dim_style),
        Span::styled(" | ", sep_style),
        Span::styled("h/l", key_style),
        Span::styled(" tabs", dim_style),
        Span::styled(" | ", sep_style),
        Span::styled("u", key_style),
        Span::styled(" dashboard", dim_style),
        Span::styled(" | ", sep_style),
        Span::styled("s", key_style),
        Span::styled(" status", dim_style),
        Span::styled(" | ", sep_style),
        Span::styled("q/esc", key_style),
        Span::styled(" quit", dim_style),
        Span::styled(" | ", sep_style),
        Span::styled(
            status_text,
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let footer = Paragraph::new(footer_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[footer_index]);
}

