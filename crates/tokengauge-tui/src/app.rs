use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
use ratatui::DefaultTerminal;
use tokengauge_core::{
    ProviderFetchError, ProviderRow, load_config, payload_to_rows_with_costs, provider_urls,
    read_cache_full, read_waybar_state, retain_enabled, waybar_state_path,
};

use crate::refresh::{RefreshResult, spawn_refresh};
use crate::ui;

#[derive(Debug)]
pub struct AppState {
    pub rows: Vec<ProviderRow>,
    pub errors: Vec<ProviderFetchError>,
    pub cache_file: PathBuf,
    pub last_refresh: Instant,
    pub last_error: Option<String>,
    pub status_message: Option<String>,
    pub spinner_index: usize,
    pub active_tab: usize,
    pub initial_provider: Option<String>,
    pub overlay: Overlay,
}

/// What is drawn over the panel, and what owns the keyboard while it is.
///
/// One state rather than a flag plus an option: each overlay used to need its
/// own carve-out in both the key handler and the renderer, so a third would
/// have cost four.
#[derive(Debug, Default)]
pub enum Overlay {
    #[default]
    None,
    Help,
    /// Boxed: a `SyncView` carries a whole config, and `AppState` is cloned
    /// around the draw loop.
    Sync(Box<crate::sync_view::SyncView>),
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
            active_tab: 0,
            initial_provider: None,
            overlay: Overlay::default(),
        }
    }

    fn next_tab(&mut self) {
        if !self.rows.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.rows.len();
        }
    }

    fn prev_tab(&mut self) {
        if !self.rows.is_empty() {
            self.active_tab = if self.active_tab == 0 {
                self.rows.len() - 1
            } else {
                self.active_tab - 1
            };
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

#[derive(Clone, Copy)]
enum OpenWhich {
    Dashboard,
    Status,
}

pub struct App {
    state: AppState,
    config_override: Option<PathBuf>,
    pending_refresh: Option<Receiver<Result<RefreshResult>>>,
    last_cache_poll: Instant,
    should_quit: bool,
}

impl App {
    pub fn new(config_override: Option<PathBuf>) -> Self {
        let config_path = config_override
            .clone()
            .unwrap_or_else(tokengauge_core::default_config_path);
        let loaded_config = config_path
            .exists()
            .then(|| load_config(Some(config_path)).ok())
            .flatten();
        if let Some(c) = &loaded_config {
            tokengauge_core::install_theme(c.theme.resolve());
        }
        let cache_file = loaded_config
            .as_ref()
            .map(|c| c.cache_file.clone())
            .unwrap_or_else(tokengauge_core::default_cache_file);
        let config_primary = loaded_config.and_then(|c| c.waybar.primary);

        let mut state = AppState::new(cache_file.clone());
        state.initial_provider = read_waybar_state(&waybar_state_path(&cache_file))
            .selected
            .or(config_primary);

        // Force on open: the user asked for the current picture, so don't serve
        // a cache that's merely young. The spinner covers the fetch.
        let pending_refresh = Some(spawn_refresh(config_override.clone(), true));
        Self {
            state,
            config_override,
            pending_refresh,
            last_cache_poll: Instant::now(),
            should_quit: false,
        }
    }

    pub fn open_sync(&mut self) {
        self.state.overlay = Overlay::Sync(Box::new(crate::sync_view::SyncView::open(
            self.config_override.clone(),
        )));
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            self.poll_refresh();
            self.maybe_repoll_cache();
            let is_refreshing = self.pending_refresh.is_some();
            terminal.draw(|frame| ui::draw(frame, &mut self.state, is_refreshing))?;
            self.handle_input()?;
            self.maybe_kick_refresh();
        }
        Ok(())
    }

    fn poll_refresh(&mut self) {
        let Some(receiver) = self.pending_refresh.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                self.apply_refresh_result(result);
                self.pending_refresh = None;
            }
            Err(TryRecvError::Empty) => {
                self.state.spinner_index = self.state.spinner_index.wrapping_add(1);
            }
            Err(TryRecvError::Disconnected) => {
                self.state.last_error = Some("refresh thread disconnected".into());
                self.state.status_message = None;
                self.pending_refresh = None;
            }
        }
    }

    /// Rebuild the rows from the snapshot on disk, without fetching.
    ///
    /// This is what picks up a fetch another frontend or the daemon made, and
    /// it is also what makes the reset countdowns tick: they are counted
    /// against the clock at render time, so they only move when a row is built
    /// again. Reading the snapshot costs a file read, so the cycle is short.
    fn maybe_repoll_cache(&mut self) {
        if self.pending_refresh.is_some() {
            return;
        }
        if self.last_cache_poll.elapsed() < Duration::from_secs(15) {
            return;
        }
        self.last_cache_poll = Instant::now();
        let Ok(config) = load_config(self.config_override.clone()) else {
            return;
        };
        let Ok(cached) = read_cache_full(&config.cache_file) else {
            return;
        };
        let (mut payloads, mut errors, costs) = cached.into_parts();
        retain_enabled(&mut payloads, &mut errors, &config.providers);
        self.state.rows = payload_to_rows_with_costs(payloads, &costs);
        self.state.errors = errors;
        self.state.last_error = None;
    }

    fn maybe_kick_refresh(&mut self) {
        if self.pending_refresh.is_some() {
            return;
        }
        let Ok(config) = load_config(self.config_override.clone()) else {
            return;
        };
        if self.state.last_refresh.elapsed() >= Duration::from_secs(config.refresh_secs) {
            self.pending_refresh = Some(spawn_refresh(self.config_override.clone(), false));
        }
    }

    fn handle_input(&mut self) -> Result<()> {
        if !event::poll(Duration::from_millis(120))? {
            return Ok(());
        }
        let Event::Key(key) = event::read()? else {
            return Ok(());
        };
        // An overlay owns the keyboard while it is up. The sync screen includes
        // `q`, because it has text fields and a typed `q` must not quit.
        match &mut self.state.overlay {
            Overlay::Sync(sync) => {
                if !sync.on_key(key) {
                    self.state.overlay = Overlay::None;
                }
                return Ok(());
            }
            Overlay::Help => {
                if should_exit(key) {
                    self.should_quit = true;
                } else {
                    self.state.overlay = Overlay::None;
                }
                return Ok(());
            }
            Overlay::None => {}
        }

        if should_exit(key) {
            self.should_quit = true;
            return Ok(());
        }
        if matches!(key.code, KeyCode::Char('r')) && self.pending_refresh.is_none() {
            self.state.status_message = Some("Refreshing…".into());
            self.pending_refresh = Some(spawn_refresh(self.config_override.clone(), true));
        }
        match key.code {
            KeyCode::Char('?') => self.state.overlay = Overlay::Help,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => self.state.next_tab(),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => self.state.prev_tab(),
            KeyCode::Char('l') | KeyCode::Right => self.state.next_tab(),
            KeyCode::Char('h') | KeyCode::Left => self.state.prev_tab(),
            KeyCode::Char('g') | KeyCode::Home if !self.state.rows.is_empty() => {
                self.state.active_tab = 0;
            }
            KeyCode::Char('G') | KeyCode::End if !self.state.rows.is_empty() => {
                self.state.active_tab = self.state.rows.len() - 1;
            }
            KeyCode::Char('u') => self.open_active_url(OpenWhich::Dashboard),
            KeyCode::Char('s') => self.open_active_url(OpenWhich::Status),
            KeyCode::Char('S') => self.open_sync(),
            _ => {}
        }
        Ok(())
    }

    fn apply_refresh_result(&mut self, result: Result<RefreshResult>) {
        match result {
            Ok(refresh) => {
                self.state.rows = refresh.rows;
                self.state.errors = refresh.errors;
                self.state.last_error = None;
            }
            Err(error) => {
                self.state.rows.clear();
                self.state.errors.clear();
                self.state.last_error = Some(error.to_string());
            }
        }
        // A refresh writes a newer sync status; an open sync screen read its
        // copy once and would otherwise sit on the old device list until the
        // user pressed `r`.
        if let Overlay::Sync(sync) = &mut self.state.overlay {
            sync.reload();
        }
        if let Some(provider) = self.state.initial_provider.take() {
            let lower = provider.to_lowercase();
            if let Some(idx) = self
                .state
                .rows
                .iter()
                .position(|r| r.provider.to_lowercase() == lower)
            {
                self.state.active_tab = idx;
            }
        }
        self.state.clamp_active_tab();
        self.state.last_refresh = Instant::now();
        self.state.status_message = None;
    }

    fn open_active_url(&self, which: OpenWhich) {
        let Some(row) = self.state.rows.get(self.state.active_tab) else {
            return;
        };
        let urls = provider_urls(&row.provider);
        let url = match which {
            OpenWhich::Dashboard => urls.dashboard,
            OpenWhich::Status => urls.status,
        };
        let Some(url) = url else { return };
        let _ = Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

fn should_exit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
}
