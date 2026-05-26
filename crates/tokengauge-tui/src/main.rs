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
    CostInfo, DIM_HEX, ExtraWindowRow, FetchResult, GREEN_HEX, ProviderFetchError,
    ProviderRow, color_hex_for_percent, fetch_all_providers, format_tokens,
    format_updated_relative, load_config, parse_hex_rgb, payload_to_rows_with_costs,
    provider_icon as core_provider_icon, provider_urls, read_cache_full, read_waybar_state,
    waybar_state_path, window_labels, write_cache_full, write_default_config,
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
    hex_to_color(DIM_HEX)
}

fn green() -> Color {
    hex_to_color(GREEN_HEX)
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
        load_config(Some(config_path)).ok()
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
    hex_to_color(color_hex_for_percent(percent))
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
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("{pad}{label}"),
        Style::default().fg(dim()).add_modifier(Modifier::BOLD),
    )));
    match used {
        Some(pct) => {
            let (bar, color) = render_bar(pct, bar_width);
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(bar, Style::default().fg(color)),
            ]));
            let reset_text = if reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {reset}")
            };
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    format!("{pct}% used"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(reset_text, Style::default().fg(dim())),
            ]));
        }
        None => {
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    "─".repeat(bar_width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)),
                    Style::default().fg(dim()),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled("no data", Style::default().fg(dim())),
            ]));
        }
    }
    lines
}

fn extra_window_lines(extra: &ExtraWindowRow, bar_width: usize) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let title = truncate(&extra.title, 32);
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::raw(pad.clone()),
        Span::styled(
            title,
            Style::default().fg(dim()).add_modifier(Modifier::BOLD),
        ),
    ]));
    match extra.used {
        Some(pct) => {
            let (bar, color) = render_bar(pct, bar_width);
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(bar, Style::default().fg(color)),
            ]));
            let trailing = if extra.reset == "—" {
                if pct == 0 {
                    String::new()
                } else {
                    "not started".to_string()
                }
            } else {
                format!("resets {}", extra.reset)
            };
            let mut spans = vec![
                Span::raw(pad.clone()),
                Span::styled(
                    format!("{pct}% used"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ];
            if !trailing.is_empty() {
                spans.push(Span::raw("   "));
                spans.push(Span::styled(trailing, Style::default().fg(dim())));
            }
            lines.push(Line::from(spans));
        }
        None => {
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled(
                    "─".repeat(bar_width.clamp(MIN_BAR_WIDTH, MAX_BAR_WIDTH)),
                    Style::default().fg(dim()),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw(pad.clone()),
                Span::styled("no data", Style::default().fg(dim())),
            ]));
        }
    }
    lines
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
    vec![
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("Today", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("      "),
            Span::styled(
                format!("${:.2}", cost.today_usd),
                Style::default().fg(green()),
            ),
            Span::styled(
                format!("  ·  {} tokens", format_tokens(cost.today_tokens)),
                Style::default().fg(dim()),
            ),
        ]),
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("Month", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("      "),
            Span::styled(
                format!("${:.2}", cost.monthly_usd),
                Style::default().fg(green()),
            ),
            Span::styled(
                format!("  ·  {} tokens", format_tokens(cost.monthly_tokens)),
                Style::default().fg(dim()),
            ),
        ]),
    ]
}

fn provider_card_lines(row: &ProviderRow, inner_width: u16) -> Vec<Line<'static>> {
    let pad = " ".repeat(LEFT_PAD);
    let bar_width = (inner_width as usize).saturating_sub(LEFT_PAD * 2 + 2);
    let mut lines = Vec::new();

    let (icon, icon_color) = provider_icon_color(&row.provider);
    let mut header = vec![
        Span::raw(pad.clone()),
        Span::styled(format!("{icon}  "), Style::default().fg(icon_color)),
        Span::styled(
            row.provider.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(plan) = row.plan_label.as_deref().filter(|s| !s.is_empty()) {
        header.push(Span::styled(
            format!("  ·  {plan}"),
            Style::default().fg(dim()),
        ));
    }
    lines.push(Line::from(header));

    if let Some(iso) = row.updated_iso.as_deref()
        && let Some(rel) = format_updated_relative(iso)
    {
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::raw("   "),
            Span::styled(format!("Updated {rel}"), Style::default().fg(dim())),
        ]));
    }

    let (session_label, weekly_label, tertiary_label) = window_labels(&row.provider);

    lines.push(Line::from(""));
    lines.extend(window_section(
        session_label,
        row.session_used,
        &row.session_reset,
        bar_width,
    ));
    lines.push(Line::from(""));
    lines.extend(window_section(
        weekly_label,
        row.weekly_used,
        &row.weekly_reset,
        bar_width,
    ));
    if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
        lines.push(Line::from(""));
        lines.extend(window_section(
            tertiary_label,
            row.tertiary_used,
            &row.tertiary_reset,
            bar_width,
        ));
    }

    if !row.extra_windows.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                "Extra usage",
                Style::default().fg(dim()).add_modifier(Modifier::BOLD),
            ),
        ]));
        for extra in &row.extra_windows {
            lines.push(Line::from(""));
            lines.extend(extra_window_lines(extra, bar_width));
        }
    }

    if let Some(cost) = &row.cost {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled(
                "Cost",
                Style::default().fg(dim()).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.extend(cost_lines(cost));
    }

    if row.credits != "—" && !row.credits.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("Credits", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("    "),
            Span::styled(format!("${}", row.credits), Style::default().fg(green())),
        ]));
    }

    lines
}

fn tab_titles(rows: &[ProviderRow]) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| {
            let (icon, color) = provider_icon_color(&row.provider);
            Line::from(vec![
                Span::styled(icon.to_string(), Style::default().fg(color)),
                Span::raw("  "),
                Span::raw(row.provider.clone()),
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

        let tabs = Tabs::new(tab_titles(&state.rows))
            .select(state.active_tab)
            .block(Block::default().borders(Borders::BOTTOM))
            .style(Style::default().fg(dim()))
            .highlight_style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
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
        let mut error_lines: Vec<Line> = state
            .errors
            .iter()
            .map(|err| {
                Line::from(vec![
                    Span::styled(
                        format!("{}: ", err.provider),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        truncate_string(&err.message, 60),
                        Style::default().fg(Color::LightRed),
                    ),
                ])
            })
            .collect();

        // Add hint about where to find full error details
        error_lines.push(Line::from(Span::styled(
            format!("Full details: {}", state.cache_file.display()),
            Style::default().fg(Color::DarkGray),
        )));

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

fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}…", &s[..max_len - 1])
    }
}
