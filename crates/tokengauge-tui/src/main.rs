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
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokengauge_core::{
    FetchResult, ProviderFetchError, ProviderRow, fetch_all_providers, load_config,
    payload_to_rows_with_costs, read_cache_full, write_cache_full, write_default_config,
};

const BAR_WIDTH: usize = 10;

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
    // Load config to get cache file path
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);
    let cache_file = if config_path.exists() {
        load_config(Some(config_path))
            .map(|c| c.cache_file)
            .unwrap_or_else(|_| PathBuf::from("/tmp/tokengauge-usage.json"))
    } else {
        PathBuf::from("/tmp/tokengauge-usage.json")
    };

    let mut state = AppState::new(cache_file);
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

        terminal.draw(|frame| draw_ui(frame, &state, pending_refresh.is_some()))?;

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

fn percent_color(percent_left: u8) -> Color {
    match percent_left {
        70..=100 => Color::Green,
        40..=69 => Color::Yellow,
        20..=39 => Color::LightRed,
        _ => Color::Red,
    }
}

fn bar_line(percent_used: Option<u8>) -> Line<'static> {
    match percent_used {
        Some(percent) => {
            let percent = percent.min(100);
            let filled = (percent as usize * BAR_WIDTH).div_ceil(100);
            let empty = BAR_WIDTH.saturating_sub(filled);
            let color = percent_color(100 - percent);
            let filled_bar = "█".repeat(filled);
            let empty_bar = "░".repeat(empty);
            Line::from(vec![
                Span::styled(filled_bar, Style::default().fg(color)),
                Span::styled(empty_bar, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(" {:>3}%", percent),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
            ])
        }
        None => Line::from(Span::styled("—", Style::default().fg(Color::DarkGray))),
    }
}

fn draw_ui(frame: &mut ratatui::Frame, state: &AppState, is_refreshing: bool) {
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

    if state.rows.is_empty() && state.errors.is_empty() {
        let message = state
            .status_message
            .as_deref()
            .or(state.last_error.as_deref())
            .unwrap_or("No providers returned");
        let empty = Paragraph::new(message)
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title("Usage"));
        frame.render_widget(empty, layout[1]);
    } else {
        let table_rows = state.rows.iter().flat_map(|row| {
            let primary = Row::new(vec![
                Cell::from(Span::styled(
                    row.provider.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Cell::from(bar_line(row.session_used)),
                Cell::from(Span::styled(
                    row.session_reset.clone(),
                    Style::default().fg(Color::Gray),
                )),
                Cell::from(bar_line(row.weekly_used)),
                Cell::from(Span::styled(
                    row.weekly_reset.clone(),
                    Style::default().fg(Color::Gray),
                )),
                Cell::from(Span::styled(
                    row.credits.clone(),
                    Style::default().fg(Color::LightGreen),
                )),
                Cell::from(Span::styled(
                    row.source.clone(),
                    Style::default().fg(Color::LightBlue),
                )),
                Cell::from(Span::styled(
                    row.updated.clone(),
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            let spacer = Row::new(vec![Cell::from(" "); 8]);
            [primary, spacer]
        });

        let table = Table::new(
            table_rows,
            [
                Constraint::Length(12),
                Constraint::Length(18),
                Constraint::Length(20),
                Constraint::Length(18),
                Constraint::Length(20),
                Constraint::Length(10),
                Constraint::Length(18),
                Constraint::Min(8),
            ],
        )
        .header(
            Row::new([
                Cell::from("Provider"),
                Cell::from("Session Used"),
                Cell::from("Session Reset"),
                Cell::from("Weekly Used"),
                Cell::from("Weekly Reset"),
                Cell::from("Credits"),
                Cell::from("Source"),
                Cell::from("Updated"),
            ])
            .style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(Block::default().borders(Borders::ALL).title("Usage"));

        frame.render_widget(table, layout[1]);
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

    let footer_line = Line::from(vec![
        Span::styled(
            "r",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" refresh", Style::default().fg(Color::Gray)),
        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "q/esc",
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit", Style::default().fg(Color::Gray)),
        Span::styled(" | ", Style::default().fg(Color::DarkGray)),
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
