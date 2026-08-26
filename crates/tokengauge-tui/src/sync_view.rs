//! The sync screen: the one place fleet sync is set up.
//!
//! Setup lives here rather than in each desktop's settings pane. Every toolkit
//! disagrees about input widgets, and this pane handles a fleet key and, later,
//! S3 credentials - five implementations of a secret input is five chances to
//! log one. The other frontends reach it with `tokengauge --sync-setup`.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tokengauge_core::sync::SyncStatus;
use tokengauge_core::{SyncTransportKind, TokenGaugeConfig, load_config, read_cache_full};

use crate::theme;

pub struct SyncView {
    config_path: PathBuf,
    config: TokenGaugeConfig,
    /// Set when the config on disk would not load. The struct above is then a
    /// default, whose `cache_file` points somewhere else entirely, so anything
    /// touching the fleet key or the store has to refuse rather than act on the
    /// wrong path.
    config_error: Option<String>,
    status: Option<SyncStatus>,
    /// Shown only after the user asks for it: this is the secret they copy to
    /// the next machine.
    revealed_key: Option<String>,
    message: Option<Message>,
    input: Option<Input>,
}

#[derive(Debug)]
struct Message {
    text: String,
    failed: bool,
}

struct Input {
    field: Field,
    buffer: String,
}

impl std::fmt::Debug for Input {
    /// The buffer holds the pasted fleet key while `Field::Join` is active, so
    /// redacting `revealed_key` alone would still leak the same secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Input")
            .field("field", &self.field)
            .field("buffer", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
enum Field {
    Join,
    Folder,
    Label,
    Endpoint,
    Bucket,
    Region,
    Prefix,
}

impl std::fmt::Debug for SyncView {
    /// The revealed key is redacted: a debug dump of the app state is not a
    /// place for the fleet secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncView")
            .field("config_path", &self.config_path)
            .field("status", &self.status)
            .field(
                "revealed_key",
                &self.revealed_key.as_ref().map(|_| "<redacted>"),
            )
            .field("message", &self.message)
            .field("input", &self.input)
            .finish()
    }
}

impl Field {
    fn prompt(self) -> &'static str {
        match self {
            Field::Join => "Fleet key from the other machine",
            Field::Folder => "Folder your sync tool handles",
            Field::Label => "Name for this machine",
            Field::Endpoint => "S3 endpoint URL",
            Field::Bucket => "Bucket",
            Field::Region => "Region (auto for R2)",
            Field::Prefix => "Key prefix (optional)",
        }
    }

    fn s3_key(self) -> Option<&'static str> {
        match self {
            Field::Endpoint => Some("endpoint"),
            Field::Bucket => Some("bucket"),
            Field::Region => Some("region"),
            Field::Prefix => Some("prefix"),
            _ => None,
        }
    }
}

impl SyncView {
    pub fn open(config_override: Option<PathBuf>) -> Self {
        let config_path = config_override.unwrap_or_else(tokengauge_core::default_config_path);
        let mut view = Self {
            config: TokenGaugeConfig::default(),
            config_error: None,
            config_path,
            status: None,
            revealed_key: None,
            message: None,
            input: None,
        };
        view.reload();
        view
    }

    pub fn reload(&mut self) {
        match load_config(Some(self.config_path.clone())) {
            Ok(config) => {
                self.config = config;
                self.config_error = None;
            }
            Err(e) => {
                self.config_error = Some(format!("{e:#}"));
                self.status = None;
                return;
            }
        }
        self.status = read_cache_full(&self.config.cache_file)
            .ok()
            .and_then(|cached| cached.sync().cloned());
    }

    fn report(&mut self, result: anyhow::Result<String>) {
        self.message = Some(match result {
            Ok(text) => Message {
                text,
                failed: false,
            },
            Err(e) => Message {
                text: format!("{e:#}"),
                failed: true,
            },
        });
        self.reload();
    }

    /// `false` closes the screen.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+C reaches here as `Char('c')`. Without this it lands on the
        // reveal binding and prints the fleet key on screen, which is the one
        // thing this module exists to avoid.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return !matches!(key.code, KeyCode::Char('c'));
        }

        if let Some(input) = self.input.as_mut() {
            match key.code {
                KeyCode::Esc => self.input = None,
                KeyCode::Backspace => {
                    input.buffer.pop();
                }
                KeyCode::Char(c) => input.buffer.push(c),
                KeyCode::Enter => {
                    let Input { field, buffer } = self.input.take().expect("just matched");
                    self.commit(field, buffer.trim());
                }
                _ => {}
            }
            return true;
        }

        // One gate, not a guard at each call site. `self.config` is a default
        // when the real one would not load, so *every* action reads or writes
        // another machine's paths - including `e`, which would compute
        // `!default.enabled` and write it into the user's actual config.
        if let Some(error) = self.config_error.clone() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => return false,
                KeyCode::Char('r') => {
                    self.reload();
                    self.message = None;
                }
                _ => {
                    self.message = Some(Message {
                        text: format!("Fix the config first: {error}"),
                        failed: true,
                    });
                }
            }
            return true;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Char('e') => {
                let next = !self.config.sync.enabled;
                let result =
                    tokengauge_core::config_set_sync_enabled(&self.config_path, next).map(|()| {
                        if next {
                            "Sync is on. It takes effect on the next refresh.".to_string()
                        } else {
                            "Sync is off. Figures cover this machine again.".to_string()
                        }
                    });
                self.report(result);
            }
            KeyCode::Char('x') => self.cycle_transport(),
            KeyCode::Char('g') => self.generate_key(),
            KeyCode::Char('c') => self.reveal_key(),
            KeyCode::Char('j') => self.prompt(Field::Join),
            KeyCode::Char('d') if self.is_dir() => self.prompt(Field::Folder),
            KeyCode::Char('1') if !self.is_dir() => self.prompt(Field::Endpoint),
            KeyCode::Char('2') if !self.is_dir() => self.prompt(Field::Bucket),
            KeyCode::Char('3') if !self.is_dir() => self.prompt(Field::Region),
            KeyCode::Char('4') if !self.is_dir() => self.prompt(Field::Prefix),
            KeyCode::Char('n') => self.prompt(Field::Label),
            KeyCode::Char('t') => {
                let result = tokengauge_core::sync::test_round_trip(&self.config)
                    .map(|steps| format!("Round trip ok: {}", steps.join(", ")));
                self.report(result);
            }
            KeyCode::Char('r') => {
                self.reload();
                self.message = None;
            }
            _ => {}
        }
        true
    }

    fn is_dir(&self) -> bool {
        matches!(self.config.sync.transport, SyncTransportKind::Dir)
    }

    fn cycle_transport(&mut self) {
        let next = if self.is_dir() { "s3" } else { "dir" };
        let result = tokengauge_core::config_set_sync_transport(&self.config_path, next)
            .map(|()| format!("Transport is now {next}."));
        self.report(result);
    }

    fn prompt(&mut self, field: Field) {
        let s3 = &self.config.sync.s3;
        let buffer = match field {
            Field::Folder => self.config.sync.dir.path.to_string_lossy().to_string(),
            Field::Label => self.config.sync.label.clone(),
            Field::Join => String::new(),
            Field::Endpoint => s3.endpoint.clone(),
            Field::Bucket => s3.bucket.clone(),
            Field::Region => s3.region.clone(),
            Field::Prefix => s3.prefix.clone(),
        };
        self.input = Some(Input { field, buffer });
    }

    fn commit(&mut self, field: Field, value: &str) {
        let result = match field {
            Field::Join => tokengauge_core::sync::FleetKey::parse(value).and_then(|key| {
                tokengauge_core::sync::store_key(&self.config.cache_file, &key, false)
                    .map(|_| format!("Joined fleet {}.", key.id_hex()))
            }),
            Field::Folder => tokengauge_core::config_set_sync_dir(&self.config_path, value)
                .map(|()| format!("Folder set to {value}.")),
            Field::Label => tokengauge_core::config_set_sync_label(&self.config_path, value)
                .map(|()| format!("This machine is now called {value}.")),
            other => {
                let key = other.s3_key().expect("every remaining field is an S3 one");
                tokengauge_core::config_set_sync_s3(&self.config_path, key, value)
                    .map(|()| format!("{key} set to {value}."))
            }
        };
        self.report(result);
    }

    /// Never replaces a key that is already here. Re-keying a fleet has to
    /// happen on every machine at once, so it belongs on the CLI where
    /// `--sync-force` makes the intent explicit - including after `c`, which
    /// used to clear the guard and let a second `g` replace the key silently.
    fn generate_key(&mut self) {
        let key = tokengauge_core::sync::FleetKey::generate();
        match tokengauge_core::sync::store_key(&self.config.cache_file, &key, false) {
            Ok(_) => {
                self.revealed_key = Some(key.display());
                self.report(Ok(
                    "New fleet key. Copy it to every other machine.".to_string()
                ));
            }
            Err(e) => self.report(Err(e)),
        }
    }

    fn reveal_key(&mut self) {
        match tokengauge_core::sync::load_key(&self.config.cache_file) {
            Ok(Some(key)) => {
                self.revealed_key = Some(key.display());
                self.message = None;
            }
            Ok(None) => {
                self.message = Some(Message {
                    text: "No fleet key yet. Press g to start a fleet.".into(),
                    failed: true,
                })
            }
            Err(e) => self.report(Err(e)),
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Fleet sync ");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines = self.status_lines();
        lines.push(Line::from(""));
        lines.extend(self.action_lines());

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn status_lines(&self) -> Vec<Line<'static>> {
        let dim = Style::default().fg(theme::dim());
        let mut lines = Vec::new();
        if let Some(error) = self.config_error.as_ref() {
            lines.push(Line::from(Span::styled(
                format!("{} could not be read:", self.config_path.display()),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(Span::styled(error.clone(), dim)));
            lines.push(Line::from(""));
            return lines;
        }
        let field = |label: &str, value: String, style: Style| {
            Line::from(vec![
                Span::styled(format!("{label:<12}"), dim),
                Span::styled(value, style),
            ])
        };

        lines.push(field(
            "State",
            if self.config.sync.enabled {
                "on".into()
            } else {
                "off".into()
            },
            Style::default().fg(if self.config.sync.enabled {
                theme::green()
            } else {
                theme::dim()
            }),
        ));
        lines.push(field(
            "Transport",
            if self.is_dir() {
                "shared folder".into()
            } else {
                "S3-compatible bucket".into()
            },
            Style::default(),
        ));
        if self.is_dir() {
            lines.push(field(
                "Folder",
                if self.config.sync.dir.path.as_os_str().is_empty() {
                    "not set - press d".into()
                } else {
                    self.config.sync.dir.path.to_string_lossy().to_string()
                },
                Style::default(),
            ));
        } else {
            let s3 = &self.config.sync.s3;
            let or_unset = |value: &str, hint: &str| {
                if value.trim().is_empty() {
                    format!("not set - press {hint}")
                } else {
                    value.to_string()
                }
            };
            lines.push(field(
                "Endpoint",
                or_unset(&s3.endpoint, "1"),
                Style::default(),
            ));
            lines.push(field("Bucket", or_unset(&s3.bucket, "2"), Style::default()));
            lines.push(field("Region", or_unset(&s3.region, "3"), Style::default()));
            lines.push(field("Prefix", or_unset(&s3.prefix, "4"), Style::default()));
            lines.push(Line::from(Span::styled(
                "            Credentials come from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY.",
                dim,
            )));
        }
        lines.push(field(
            "This machine",
            if self.config.sync.label.is_empty() {
                "hostname - press n to name it".into()
            } else {
                self.config.sync.label.clone()
            },
            Style::default(),
        ));

        if let Some(key) = self.revealed_key.as_ref() {
            lines.push(field(
                "Fleet key",
                key.clone(),
                Style::default().fg(theme::green()),
            ));
            lines.push(Line::from(Span::styled(
                "            Run `tokengauge --sync-join <key>` on the other machine.",
                dim,
            )));
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let Some(report) = self
            .status
            .as_ref()
            .map(|status| tokengauge_core::sync::describe(status, now_ms))
        else {
            lines.push(field(
                "Last sync",
                "has not run yet".into(),
                Style::default().fg(theme::dim()),
            ));
            return lines;
        };

        if let Some(last) = &report.last_pull {
            lines.push(field("Last sync", last.clone(), Style::default()));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Devices", dim)));
        for device in &report.devices {
            lines.push(Line::from(vec![
                Span::raw(format!("  {:<20}", device.label)),
                Span::styled(device.detail.clone(), dim),
            ]));
        }
        if report.devices.len() < 2 {
            lines.push(Line::from(Span::styled(
                "  no other machine has published yet",
                dim,
            )));
        }

        if !report.problems.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Problems", dim)));
            for problem in &report.problems {
                lines.push(Line::from(Span::styled(
                    format!("  {problem}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        lines
    }

    fn action_lines(&self) -> Vec<Line<'static>> {
        let dim = Style::default().fg(theme::dim());
        let mut lines = Vec::new();

        if let Some(input) = self.input.as_ref() {
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", input.field.prompt()), dim),
                Span::raw(input.buffer.clone()),
                Span::styled("_", dim),
            ]));
            lines.push(Line::from(Span::styled(
                "Enter to save, Esc to cancel",
                dim,
            )));
            return lines;
        }

        if let Some(message) = self.message.as_ref() {
            lines.push(Line::from(Span::styled(
                message.text.clone(),
                Style::default().fg(if message.failed {
                    Color::Red
                } else {
                    theme::green()
                }),
            )));
            lines.push(Line::from(""));
        }

        let where_to = if self.is_dir() {
            "d folder"
        } else {
            "1-4 bucket"
        };
        lines.push(Line::from(Span::styled(
            format!(
                "e on/off   x transport   {where_to}   n name   g new key   c show key   j join   t test   r reload   Esc back"
            ),
            dim,
        )));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tokengauge-syncview-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        let config = dir.join("config.toml");
        // A TOML *literal* string. In a basic string a Windows path turns
        // `\Users` into an escape, the config silently fails to parse, and the
        // view falls back to defaults pointing at the real state directory.
        std::fs::write(
            &config,
            format!("cache_file = '{}'\n", dir.join("usage.json").display()),
        )
        .expect("config");
        config
    }

    /// Every test opens through here: a view whose config did not parse would
    /// operate on the developer's own state directory, and the tests would
    /// collide with each other through it.
    fn open_scratch(name: &str) -> SyncView {
        let path = scratch(name);
        let view = SyncView::open(Some(path.clone()));
        assert_eq!(view.config_error, None, "the scratch config must parse");
        assert!(
            view.config
                .cache_file
                .starts_with(path.parent().expect("scratch dir")),
            "a test must never touch the real state directory: {:?}",
            view.config.cache_file
        );
        view
    }

    fn screen(view: &SyncView) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).expect("terminal");
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

    fn press(view: &mut SyncView, c: char) -> bool {
        view.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn key(view: &mut SyncView, code: KeyCode) -> bool {
        view.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn an_unconfigured_fleet_says_what_to_press() {
        let view = open_scratch("empty");
        let screen = screen(&view);

        assert!(screen.contains("not set - press d"), "{screen}");
        assert!(screen.contains("has not run yet"), "{screen}");
        assert!(screen.contains("d folder"), "{screen}");
    }

    #[test]
    fn setting_the_folder_writes_the_config() {
        let mut view = open_scratch("folder");
        let config_path = view.config_path.clone();

        assert!(press(&mut view, 'd'));
        assert!(screen(&view).contains("Folder your sync tool handles"));
        for c in "/tmp/fleet".chars() {
            press(&mut view, c);
        }
        key(&mut view, KeyCode::Enter);

        let written = std::fs::read_to_string(&config_path).expect("config");
        assert!(written.contains("/tmp/fleet"), "{written}");
        assert!(screen(&view).contains("Folder set to /tmp/fleet"));
    }

    #[test]
    fn a_config_that_will_not_parse_stops_the_screen_from_acting() {
        // Exactly what CI hit on Windows: a path in a TOML *basic* string, so
        // `\U` reads as an invalid escape. Falling back to a default config
        // would put this fleet's key in the real state directory.
        let dir = std::env::temp_dir().join(format!(
            "tokengauge-syncview-{}-badtoml",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("scratch");
        let config = dir.join("config.toml");
        std::fs::write(&config, "cache_file = \"C:\\Users\\me\\usage.json\"\n").expect("config");

        let mut view = SyncView::open(Some(config));
        assert!(view.config_error.is_some(), "the config does not parse");
        assert!(
            screen(&view).contains("could not be read"),
            "{}",
            screen(&view)
        );

        press(&mut view, 'g');
        assert!(view.revealed_key.is_none(), "no key may be written");
        assert!(
            screen(&view).contains("Fix the config first"),
            "{}",
            screen(&view)
        );

        press(&mut view, 't');
        assert!(screen(&view).contains("Fix the config first"));

        // Every action, not just the ones that were remembered individually:
        // `e` would compute `!default.enabled` and write it to the real config.
        for gated in ['e', 'x', 'n', 'd', 'j', 'c'] {
            press(&mut view, gated);
            assert!(
                screen(&view).contains("Fix the config first"),
                "`{gated}` acted on a default config"
            );
            assert!(view.input.is_none(), "`{gated}` opened a field anyway");
        }

        // Reload and leave still work, or the screen is a trap.
        assert!(press(&mut view, 'r'));
        assert!(!press(&mut view, 'q'));
    }

    #[test]
    fn ctrl_c_leaves_instead_of_revealing_the_fleet_key() {
        let mut view = open_scratch("ctrl-c");
        press(&mut view, 'g');
        let shown = view.revealed_key.clone().expect("generated");

        let mut fresh = open_scratch("ctrl-c-2");
        std::fs::copy(
            tokengauge_core::sync::key_path(&view.config.cache_file),
            tokengauge_core::sync::key_path(&fresh.config.cache_file),
        )
        .expect("copy key");

        // Crossterm delivers Ctrl+C as Char('c') with a modifier, which used to
        // land on the reveal binding.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!fresh.on_key(ctrl_c), "Ctrl+C should leave the screen");
        assert!(
            fresh.revealed_key.is_none(),
            "Ctrl+C revealed the fleet key"
        );
        assert!(!screen(&fresh).contains(&shown));

        // And it must not type itself into a field either.
        fresh.input = Some(Input {
            field: Field::Label,
            buffer: String::new(),
        });
        fresh.on_key(ctrl_c);
        assert_eq!(
            fresh.input.as_ref().map(|i| i.buffer.as_str()),
            Some(""),
            "Ctrl+C typed a `c` into the field"
        );
    }

    #[test]
    fn a_typed_q_lands_in_the_field_instead_of_quitting() {
        let mut view = open_scratch("typed-q");
        press(&mut view, 'n');
        assert!(
            press(&mut view, 'q'),
            "the screen must stay open while typing"
        );
        assert!(screen(&view).contains("Name for this machine"));

        key(&mut view, KeyCode::Esc);
        assert!(
            !press(&mut view, 'q'),
            "q closes the screen outside a field"
        );
    }

    #[test]
    fn switching_to_a_bucket_asks_for_bucket_things() {
        let mut view = open_scratch("s3");
        let config_path = view.config_path.clone();
        assert!(screen(&view).contains("shared folder"));

        press(&mut view, 'x');
        let shown = screen(&view);
        assert!(shown.contains("S3-compatible bucket"), "{shown}");
        assert!(shown.contains("Endpoint"), "{shown}");
        assert!(
            shown.contains("AWS_ACCESS_KEY_ID"),
            "credentials belong in the environment, and the screen should say so"
        );
        assert!(shown.contains("1-4 bucket"), "{shown}");

        assert!(press(&mut view, '2'));
        for c in "tokengauge".chars() {
            press(&mut view, c);
        }
        key(&mut view, KeyCode::Enter);

        let written = std::fs::read_to_string(&config_path).expect("config");
        assert!(written.contains("bucket = \"tokengauge\""), "{written}");
    }

    #[test]
    fn a_key_is_only_shown_when_asked_for() {
        let mut view = open_scratch("key");
        assert!(!screen(&view).contains("tgsync1"));

        press(&mut view, 'c');
        assert!(screen(&view).contains("No fleet key yet"));

        press(&mut view, 'g');
        let shown = screen(&view);
        assert!(shown.contains("tgsync1"), "{shown}");
        assert!(shown.contains("--sync-join"), "{shown}");
    }

    #[test]
    fn showing_the_key_does_not_open_the_door_to_replacing_it() {
        let mut view = open_scratch("no-clobber");
        press(&mut view, 'g');
        let first = view.revealed_key.clone().expect("a key was generated");

        // `c` used to clear the guard, so a following `g` replaced the fleet's
        // key with no confirmation and no way back.
        press(&mut view, 'c');
        press(&mut view, 'g');

        assert_eq!(view.revealed_key.as_ref(), Some(&first), "the key changed");
        assert!(screen(&view).contains("--sync-force"), "{}", screen(&view));

        // Joining a different fleet is the same destructive act.
        press(&mut view, 'j');
        for c in tokengauge_core::sync::FleetKey::generate()
            .display()
            .chars()
        {
            press(&mut view, c);
        }
        key(&mut view, KeyCode::Enter);
        assert!(screen(&view).contains("--sync-force"));

        let on_disk = tokengauge_core::sync::load_key(&view.config.cache_file)
            .expect("read")
            .expect("still there");
        assert_eq!(on_disk.display(), first);
    }
}
