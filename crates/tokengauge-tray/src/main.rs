//! TokenGauge system-tray GUI for Windows.
//!
//! A small always-available window drawing the same panel every other frontend
//! draws - limits, cost, tokens by day, tokens by model - from
//! [`tokengauge_core::panel_spec`], backed by a system-tray icon that renders
//! the current peak usage percentage. Windows-only; on other platforms this is
//! a stub (the Linux surfaces are the Waybar module, KDE applet, GNOME
//! extension and Quickshell widget).

// Build as a GUI (windowless) binary on Windows so launching it doesn't pop a
// console window - important when it runs at login / from the tray.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "tokengauge-tray is Windows-only; on Linux use the Waybar module, the KDE \
         applet, the GNOME extension or the Quickshell widget."
    );
}

#[cfg(windows)]
fn main() -> eframe::Result<()> {
    // A flyout, not a window: no title bar, no taskbar button, above whatever
    // it is opened over, and placed against the tray icon that opened it. It
    // used to be an ordinary decorated window sized 500x440, which is why it
    // read as an application someone had left running rather than as a panel.
    //
    // `--hidden` starts in the tray with nothing on screen, which is what the
    // run-at-login shortcut passes: a panel that opens itself on every login is
    // the one thing a tray app must not do.
    let hidden = std::env::args().any(|arg| arg == "--hidden");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([win::PANEL_WIDTH, win::PANEL_HEIGHT])
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_always_on_top()
            .with_visible(!hidden)
            .with_title("TokenGauge"),
        ..Default::default()
    };
    eframe::run_native(
        "TokenGauge",
        options,
        Box::new(|cc| Ok(Box::new(win::TrayApp::new(cc)?))),
    )
}

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use eframe::egui::{self, Color32, ProgressBar, RichText, ViewportCommand};
    use tokengauge_core::{
        HistoryPanel, PROVIDERS, ProviderRow, Section, SectionKind, TokenGaugeConfig, Tone,
        config_set_oauth_provider, config_set_primary, default_config_path, fetch_all_providers,
        load_config, panel_spec, payload_to_rows_with_costs, read_cache_full, retain_enabled,
        write_cache_full, write_default_config,
    };
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
    use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

    type DynErr = Box<dyn std::error::Error + Send + Sync + 'static>;

    // Catppuccin Mocha palette.
    const BG: Color32 = Color32::from_rgb(0x1e, 0x1e, 0x2e);
    const CARD: Color32 = Color32::from_rgb(0x31, 0x32, 0x44);
    const BORDER: Color32 = Color32::from_rgb(0x45, 0x47, 0x5a);
    const TEXT: Color32 = Color32::from_rgb(0xcd, 0xd6, 0xf4);
    const SUB: Color32 = Color32::from_rgb(0xa6, 0xad, 0xc8);
    const BLUE: Color32 = Color32::from_rgb(0x89, 0xb4, 0xfa);
    const MAUVE: Color32 = Color32::from_rgb(0xcb, 0xa6, 0xf7);
    const GREEN: Color32 = Color32::from_rgb(0xa6, 0xe3, 0xa1);
    const YELLOW: Color32 = Color32::from_rgb(0xf9, 0xe2, 0xaf);
    const PEACH: Color32 = Color32::from_rgb(0xfa, 0xb3, 0x87);
    const RED: Color32 = Color32::from_rgb(0xf3, 0x8b, 0xa8);
    const DARK: Color32 = Color32::from_rgb(0x11, 0x11, 0x1b);

    /// Flyout size in points. Fixed: it is anchored to the tray icon, so a
    /// user resizing it would only move it away from what it points at.
    pub(crate) const PANEL_WIDTH: f32 = 420.0;
    pub(crate) const PANEL_HEIGHT: f32 = 600.0;
    /// Gap between the flyout and both the tray icon and the screen edges.
    const PANEL_MARGIN: f32 = 8.0;
    /// A tray click that lands within this of the panel hiding itself is the
    /// same click: Windows blurs the panel before delivering it. Reopening
    /// then makes the icon impossible to close the panel with.
    const REOPEN_GRACE: Duration = Duration::from_millis(400);

    /// A rendered provider row. The panel body is resolved by the core, so this
    /// window draws the same sections in the same order as every other
    /// frontend; only the chrome around them is egui's own.
    #[derive(Clone, Default)]
    struct Row {
        provider: String,
        plan: Option<String>,
        stale: bool,
        updated: String,
        /// Kept out of `panel` because the tray icon needs the raw number.
        session_used: Option<u8>,
        weekly_used: Option<u8>,
        panel: Vec<Section>,
        /// Every range, resolved by the core. The second screen behind the
        /// History button draws one of these and formats none of it.
        history: HistoryPanel,
    }

    /// The two files a history panel is resolved from, read once per rebuild.
    ///
    /// Once, not once per provider: both are files, there are five providers,
    /// and this runs every fifteen seconds while the flyout is open.
    struct HistoryInputs {
        store: tokengauge_core::sync::FleetStore,
        prices: tokengauge_core::cost::pricing::PriceTable,
        /// A store that would not parse. Carried onto every panel rather than
        /// swallowed: an empty chart that should not be empty has to say why.
        note: Option<String>,
    }

    impl HistoryInputs {
        fn load(config: &TokenGaugeConfig) -> Self {
            let (store, note) = tokengauge_core::sync::store::load(&config.cache_file);
            // Never over the network: this is a render, not a fetch.
            let prices = tokengauge_core::cost::pricing::load(
                &config.cache_file,
                std::time::Duration::from_secs(config.ccusage_timeout_secs),
                false,
            );
            Self {
                store,
                prices,
                note,
            }
        }

        fn panel(&self, provider: &str) -> HistoryPanel {
            let mut panel = tokengauge_core::history_panel_now(
                &self.store,
                provider,
                &self.prices,
                tokengauge_core::cost::pricing::archive(),
            );
            if let Some(note) = &self.note {
                panel.notes.push(note.clone());
            }
            panel
        }
    }

    fn to_row(r: &ProviderRow, history: &HistoryInputs) -> Row {
        Row {
            provider: r.provider.clone(),
            plan: r.plan_label.clone(),
            stale: r.stale,
            updated: r.updated.clone(),
            session_used: r.session_used,
            weekly_used: r.weekly_used,
            panel: panel_spec(r),
            history: history.panel(&r.provider),
        }
    }

    #[derive(Default, Clone)]
    struct Snapshot {
        rows: Vec<Row>,
        errors: Vec<String>,
        fetching: bool,
        /// The providers currently enabled in the config. The settings pane
        /// draws a switch for every entry in `PROVIDERS` and tests membership
        /// here; the bar-pin chips list only these.
        enabled: Vec<String>,
        primary: String,
    }

    /// What the tray thread and the UI thread have to agree on to make the
    /// window behave like a flyout.
    #[derive(Default)]
    struct Flyout {
        /// The tray icon's rectangle in physical pixels, set by a click and
        /// consumed by the next frame, which is the only place that can move
        /// the window.
        anchor: Option<(f64, f64, f64, f64)>,
        /// The panel has held focus since it was last shown. Losing focus only
        /// means "the user clicked away" once it has been focused at all -
        /// otherwise the window would dismiss itself before it appeared.
        focused_once: bool,
        /// When it last dismissed itself, so the click that dismissed it does
        /// not immediately bring it back.
        hidden_at: Option<Instant>,
    }

    /// A config mutation from the settings pane, applied on the fetch thread so
    /// the UI never blocks on a file write.
    enum Action {
        Refresh,
        SetProvider(String, bool),
        SetPrimary(String),
    }

    pub struct TrayApp {
        shared: Arc<Mutex<Snapshot>>,
        action_tx: mpsc::Sender<Action>,
        selected: usize,
        settings_open: bool,
        /// The history screen, and which range it is showing. A second screen
        /// over the panel like the settings pane: exactly one of the three is
        /// up at a time.
        history_open: bool,
        history_range: usize,
        quit: Arc<AtomicBool>,
        tray: TrayIcon,
        _items: Vec<MenuItem>,
        last_tip: String,
        cfg_path: std::path::PathBuf,
        last_cache_poll: Instant,
        flyout: Arc<Mutex<Flyout>>,
    }

    /// The tray menu's ids, bundled so the event loop takes one of them rather
    /// than one parameter per item.
    struct MenuIds {
        show: MenuId,
        refresh: MenuId,
        sync: MenuId,
        update: MenuId,
        quit: MenuId,
    }

    impl TrayApp {
        pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, DynErr> {
            let ctx = cc.egui_ctx.clone();

            // egui keeps `↑` and `↓` - the arrows the cost trend badge is
            // written with - in Hack alone, and puts Hack in the monospace
            // family only. Every proportional string carrying one drew a tofu
            // box until Hack became the proportional family's last fallback.
            let mut fonts = egui::FontDefinitions::default();
            if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                family.push("Hack".to_owned());
            }
            ctx.set_fonts(fonts);

            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = BG;
            visuals.window_fill = BG;
            visuals.override_text_color = Some(TEXT);
            // `set_visuals` writes the style of the *current* theme only. On a
            // machine running Windows in light mode this window kept its
            // hardcoded dark fills and drew every string without an explicit
            // colour in the light theme's near-black, which is most of what
            // "the UI is broken" turned out to mean.
            ctx.set_theme(egui::ThemePreference::Dark);
            ctx.set_visuals_of(egui::Theme::Dark, visuals.clone());
            ctx.set_visuals_of(egui::Theme::Light, visuals);

            let shared = Arc::new(Mutex::new(Snapshot::default()));
            let (action_tx, action_rx) = mpsc::channel::<Action>();

            let cfg_path = default_config_path();
            if !cfg_path.exists() {
                let _ = write_default_config(&cfg_path);
            }

            // Seed from the shared cache for an instant first paint.
            load_from_cache(&shared, &cfg_path);

            // Background fetch loop.
            let cfg_path_for_app = cfg_path.clone();
            {
                let ctx = ctx.clone();
                let shared = shared.clone();
                thread::spawn(move || fetch_loop(ctx, shared, action_rx, cfg_path));
            }

            // Tray icon + menu. Left-click shows the window (not the menu).
            let menu = Menu::new();
            let show_i = MenuItem::new("Show TokenGauge", true, None);
            let refresh_i = MenuItem::new("Refresh now", true, None);
            let sync_i = MenuItem::new("Set up fleet sync", true, None);
            let update_i = MenuItem::new("Update TokenGauge", true, None);
            let quit_i = MenuItem::new("Quit", true, None);
            menu.append(&show_i)?;
            menu.append(&refresh_i)?;
            menu.append(&sync_i)?;
            menu.append(&update_i)?;
            menu.append(&quit_i)?;

            let tray = TrayIconBuilder::new()
                .with_tooltip("TokenGauge")
                .with_icon(render_icon(None, BLUE))
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()?;

            let quit = Arc::new(AtomicBool::new(false));

            let flyout = Arc::new(Mutex::new(Flyout::default()));

            // Handle tray/menu events on their own thread so they work even
            // while the window is hidden (the egui loop may not tick then).
            {
                let ctx = ctx.clone();
                let action_tx = action_tx.clone();
                let quit = quit.clone();
                let flyout = flyout.clone();
                let ids = MenuIds {
                    show: show_i.id().clone(),
                    refresh: refresh_i.id().clone(),
                    sync: sync_i.id().clone(),
                    update: update_i.id().clone(),
                    quit: quit_i.id().clone(),
                };
                thread::spawn(move || tray_event_loop(ctx, action_tx, quit, ids, flyout));
            }

            Ok(Self {
                shared,
                action_tx,
                selected: 0,
                settings_open: false,
                history_open: false,
                history_range: 0,
                quit,
                tray,
                _items: vec![show_i, refresh_i, sync_i, update_i, quit_i],
                last_tip: String::new(),
                cfg_path: cfg_path_for_app,
                last_cache_poll: Instant::now(),
                flyout,
            })
        }

        /// Re-read the snapshot on a short cycle, so the countdowns in the
        /// panel tick and a fetch by the daemon or another frontend lands here.
        /// Nothing is fetched: this is a file read, and the fetch loop keeps its
        /// own cadence. egui stops ticking while the window is hidden, so this
        /// runs only when there is something on screen to be wrong.
        fn repoll_cache(&mut self) {
            if self.last_cache_poll.elapsed() < Duration::from_secs(15) {
                return;
            }
            self.last_cache_poll = Instant::now();
            // A fetch in flight is about to write both the snapshot and the
            // rows; loading the one it is replacing would flash the old figures.
            if self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .fetching
            {
                return;
            }
            load_from_cache(&self.shared, &self.cfg_path);
        }

        /// Move the panel to the tray icon a click came from, and dismiss it
        /// when the user clicks away or presses Escape - the two things that
        /// close a tray flyout and that a decorated window got for free from
        /// its title bar.
        fn track_flyout(&mut self, ctx: &egui::Context) {
            let mut flyout = self.flyout.lock().unwrap_or_else(|e| e.into_inner());

            if let Some(rect) = flyout.anchor.take() {
                place_flyout(ctx, rect);
            }

            let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
            if focused {
                flyout.focused_once = true;
            }
            let dismissed = ctx.input(|i| i.key_pressed(egui::Key::Escape))
                || (!focused && flyout.focused_once);
            if dismissed {
                flyout.focused_once = false;
                flyout.hidden_at = Some(Instant::now());
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
        }

        /// Reflect the latest usage in the tray icon (peak %) and tooltip.
        fn sync_tray(&mut self, snap: &Snapshot) {
            let (tip, peak) = tray_summary(snap);
            if tip != self.last_tip {
                let color = peak.map(usage_color).unwrap_or(BLUE);
                let _ = self.tray.set_icon(Some(render_icon(peak, color)));
                let _ = self.tray.set_tooltip(Some(&tip));
                self.last_tip = tip;
            }
        }
    }

    impl eframe::App for TrayApp {
        fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
            BG.to_normalized_gamma_f32()
        }

        // Runs before each `ui`. Handles close-to-tray and keeps the tray icon
        // fresh; tray clicks are serviced by the dedicated thread.
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            self.repoll_cache();
            let snap = self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            self.sync_tray(&snap);
            self.track_flyout(ctx);

            // On real quit, let the close proceed so run_native returns and
            // TrayApp/TrayIcon drop cleanly (removing the tray icon). Otherwise
            // the window just hides to the tray.
            if ctx.input(|i| i.viewport().close_requested()) && !self.quit.load(Ordering::SeqCst) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            ctx.request_repaint_after(Duration::from_millis(750));
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let snap = self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if self.selected >= snap.rows.len() {
                self.selected = 0;
            }

            // Outer padding so content doesn't touch the window edges.
            egui::Frame::group(ui.style())
                .fill(BG)
                .stroke(egui::Stroke::new(0.0, Color32::TRANSPARENT))
                .inner_margin(egui::Margin::symmetric(18, 14))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("TokenGauge").size(22.0).strong().color(BLUE));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // The flyout has no title bar, so the only way back
                            // to the tray other than clicking away is here.
                            let close = egui::Button::new(
                                RichText::new("\u{d7}").size(16.0).strong().color(SUB),
                            )
                            .fill(CARD)
                            .corner_radius(6)
                            .min_size(egui::vec2(26.0, 26.0));
                            if ui.add(close).on_hover_text("Hide (Esc)").clicked() {
                                ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
                            }
                            let gear = egui::Button::new(
                                RichText::new("\u{2699} Settings")
                                    .strong()
                                    .color(if self.settings_open { DARK } else { SUB }),
                            )
                            .fill(if self.settings_open { BLUE } else { CARD })
                            .corner_radius(6)
                            .min_size(egui::vec2(0.0, 26.0));
                            if ui.add(gear).clicked() {
                                self.settings_open = !self.settings_open;
                                if self.settings_open {
                                    self.history_open = false;
                                }
                            }
                            // No glyph on this one. egui's bundled fonts carry
                            // the gear and the refresh arrow above, but not the
                            // chart emoji, and a tofu box in the header is the
                            // bug the cost trend badge already shipped once.
                            let history = egui::Button::new(
                                RichText::new("History")
                                    .strong()
                                    .color(if self.history_open { DARK } else { SUB }),
                            )
                            .fill(if self.history_open { BLUE } else { CARD })
                            .corner_radius(6)
                            .min_size(egui::vec2(0.0, 26.0));
                            if ui.add(history).clicked() {
                                self.history_open = !self.history_open;
                                if self.history_open {
                                    self.settings_open = false;
                                }
                            }
                            let btn = egui::Button::new(
                                RichText::new("\u{27f3} Refresh").strong().color(DARK),
                            )
                            .fill(BLUE)
                            .corner_radius(6)
                            .min_size(egui::vec2(0.0, 26.0));
                            if ui.add(btn).clicked() {
                                let _ = self.action_tx.send(Action::Refresh);
                            }
                            if snap.fetching {
                                ui.spinner();
                            }
                        });
                    });
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if snap.rows.is_empty() && !self.settings_open {
                                ui.add_space(24.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        RichText::new("No usage data yet").size(15.0).color(SUB),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Sign in with `codex` or `claude`, then refresh.",
                                        )
                                        .small()
                                        .color(SUB),
                                    );
                                });
                            }

                            if self.settings_open {
                                self.settings_pane(ui, &snap);
                            } else if self.history_open {
                                self.provider_tabs(ui, &snap);
                                if let Some(row) = snap.rows.get(self.selected) {
                                    self.history_pane(ui, row);
                                }
                            } else {
                                self.provider_tabs(ui, &snap);
                                if let Some(row) = snap.rows.get(self.selected) {
                                    self.provider_panel(ui, row);
                                }
                            }

                            if !snap.errors.is_empty() {
                                egui::Frame::group(ui.style())
                                    .fill(Color32::from_rgb(0x2a, 0x1e, 0x26))
                                    .stroke(egui::Stroke::new(1.0, RED))
                                    .corner_radius(10)
                                    .inner_margin(egui::Margin::same(12))
                                    .show(ui, |ui| {
                                        ui.label(RichText::new("Errors").strong().color(RED));
                                        ui.add_space(4.0);
                                        for e in &snap.errors {
                                            ui.label(RichText::new(e).small().color(SUB));
                                        }
                                    });
                            }
                        });
                });
        }
    }

    impl TrayApp {
        /// One chip per provider. Hidden with a single provider, where a tab
        /// strip is just a label repeating the card header below it.
        fn provider_tabs(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
            if snap.rows.len() < 2 {
                return;
            }
            ui.horizontal_wrapped(|ui| {
                for (i, row) in snap.rows.iter().enumerate() {
                    let active = i == self.selected;
                    let chip = egui::Button::new(
                        RichText::new(cap(&row.provider)).strong().color(if active {
                            DARK
                        } else {
                            SUB
                        }),
                    )
                    .fill(if active { BLUE } else { CARD })
                    .corner_radius(6);
                    if ui.add(chip).clicked() {
                        self.selected = i;
                    }
                }
            });
            ui.add_space(4.0);
        }

        /// The header card plus every section the core resolved, in order.
        fn provider_panel(&mut self, ui: &mut egui::Ui, row: &Row) {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(cap(&row.provider)).size(16.0).strong());
                    if row.stale {
                        ui.label(RichText::new("stale").small().color(PEACH));
                    }
                    if let Some(plan) = &row.plan {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(plan).small().color(MAUVE));
                        });
                    }
                });

                for section in &row.panel {
                    ui.add_space(10.0);
                    ui.label(RichText::new(section.title).small().strong().color(SUB));
                    ui.add_space(2.0);
                    for panel_row in &section.rows {
                        match section.kind {
                            SectionKind::Meters => meter_row(ui, panel_row),
                            SectionKind::Bars => bar_row(ui, panel_row),
                            SectionKind::Rows => key_row(ui, panel_row),
                        }
                    }
                }

                if !row.updated.is_empty() && row.updated != "\u{2014}" {
                    ui.add_space(6.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("Updated {}", row.updated))
                                .small()
                                .color(SUB),
                        );
                    });
                }
            });
            ui.add_space(8.0);
        }

        /// The history screen: a year of spend, as a chart.
        ///
        /// Every string is the core's; the chart is the only part egui decides.
        fn history_pane(&mut self, ui: &mut egui::Ui, row: &Row) {
            let history = row.history.clone();
            if self.history_range >= history.series.len() {
                self.history_range = 0;
            }
            card(ui, |ui| {
                ui.label(RichText::new("HISTORY").small().strong().color(SUB));
                ui.add_space(6.0);

                ui.horizontal_wrapped(|ui| {
                    for (at, series) in history.series.iter().enumerate() {
                        let selected = at == self.history_range;
                        let chip = egui::Button::new(
                            RichText::new(series.label).strong().color(if selected {
                                DARK
                            } else {
                                SUB
                            }),
                        )
                        .fill(if selected { BLUE } else { CARD })
                        .corner_radius(6);
                        if ui.add(chip).clicked() {
                            self.history_range = at;
                        }
                    }
                });

                let Some(series) = history.series.get(self.history_range) else {
                    ui.add_space(6.0);
                    ui.label(RichText::new("No history yet").color(SUB));
                    return;
                };

                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{}  \u{b7}  {} tokens  \u{b7}  avg {}",
                        series.total_usd, series.total_tokens, series.average_usd
                    ))
                    .color(TEXT),
                );

                if series.empty {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Nothing spent in this range")
                            .small()
                            .color(SUB),
                    );
                } else {
                    ui.add_space(8.0);
                    draw_history_chart(ui, series);
                    ui.horizontal(|ui| {
                        if let (Some(first), Some(last)) =
                            (series.points.first(), series.points.last())
                        {
                            ui.label(RichText::new(&first.full_label).small().color(SUB));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new(&last.full_label).small().color(SUB));
                                },
                            );
                        }
                    });
                }

                ui.add_space(6.0);
                let mut notes = vec![history.covers.clone()];
                notes.extend(history.notes.iter().cloned());
                ui.label(RichText::new(notes.join("  \u{b7}  ")).small().color(SUB));
            });
            ui.add_space(8.0);
        }

        /// Provider toggles and the bar pin - the same two controls the other
        /// frontends put behind their gear button.
        fn settings_pane(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
            card(ui, |ui| {
                ui.label(RichText::new("PROVIDERS").small().strong().color(SUB));
                ui.add_space(4.0);
                for provider in PROVIDERS {
                    let mut on = snap.enabled.iter().any(|p| p == provider);
                    if ui.checkbox(&mut on, cap(provider)).changed() {
                        // The fetch thread only rewrites `enabled` once a whole
                        // fetch has finished. Without this the switch snaps back
                        // for the length of that fetch, and a second click
                        // computes `on` from the stale value.
                        {
                            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                            s.enabled.retain(|p| p != provider);
                            if on {
                                s.enabled.push((*provider).to_string());
                            }
                        }
                        let _ = self
                            .action_tx
                            .send(Action::SetProvider((*provider).to_string(), on));
                    }
                }

                ui.add_space(12.0);
                ui.label(RichText::new("PIN TO BAR").small().strong().color(SUB));
                ui.add_space(4.0);
                let current = if snap.primary.is_empty() {
                    "highest"
                } else {
                    snap.primary.as_str()
                };
                ui.horizontal_wrapped(|ui| {
                    for choice in
                        std::iter::once("highest").chain(snap.enabled.iter().map(String::as_str))
                    {
                        let active = choice == current;
                        let chip = egui::Button::new(
                            RichText::new(pin_label(choice)).strong().color(if active {
                                DARK
                            } else {
                                SUB
                            }),
                        )
                        .fill(if active { BLUE } else { CARD })
                        .corner_radius(6);
                        if ui.add(chip).clicked() {
                            {
                                let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                                s.primary = if choice == "highest" {
                                    String::new()
                                } else {
                                    choice.to_string()
                                };
                            }
                            let _ = self.action_tx.send(Action::SetPrimary(choice.to_string()));
                        }
                    }
                });
            });
            ui.add_space(8.0);
        }
    }

    /// The bars, painted rather than laid out: egui has no bar chart, and a
    /// row of widgets would carry spacing rules this does not want.
    fn draw_history_chart(ui: &mut egui::Ui, series: &tokengauge_core::HistorySeries) {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 150.0),
            egui::Sense::hover(),
        );
        let n = series.points.len();
        if n == 0 || rect.width() <= 0.0 {
            return;
        }
        let painter = ui.painter_at(rect);
        // Wide steps get a gap; ninety days of bars have none to spare.
        let gap: f32 = if n <= 12 {
            2.0
        } else if n <= 31 {
            1.0
        } else {
            0.0
        };
        let width = ((rect.width() - gap * (n as f32 - 1.0)) / n as f32).max(1.0);
        for (i, point) in series.points.iter().enumerate() {
            let fraction = point.fraction.clamp(0.0, 1.0) as f32;
            // A floor of one pixel: a step that spent a little must never draw
            // as a step that spent nothing.
            let height = if fraction > 0.0 {
                (fraction * rect.height()).max(1.0)
            } else {
                0.0
            };
            if height <= 0.0 {
                continue;
            }
            let x = rect.left() + i as f32 * (width + gap);
            let bar = egui::Rect::from_min_size(
                egui::pos2(x, rect.bottom() - height),
                egui::vec2(width, height),
            );
            // The fill stays the series colour; `partial` below carries the
            // "in progress" signal on its own. Taking the dim tone as well
            // drew that step as a ghost rather than as this month so far.
            let colour = if point.tone == Tone::Critical {
                RED
            } else {
                BLUE
            };
            // The step in progress is short because it is not over, so it is
            // drawn as unfinished rather than as a fall.
            let colour = if point.partial {
                colour.gamma_multiply(0.45)
            } else {
                colour
            };
            painter.rect_filled(bar, 0.0, colour);
        }
    }

    fn tone_color(tone: Tone) -> Color32 {
        match tone {
            Tone::Good => GREEN,
            Tone::Warn => YELLOW,
            Tone::Critical => RED,
            Tone::Dim => SUB,
            Tone::Normal => TEXT,
        }
    }

    /// The spec pads its sub-tables with spaces, so a tooltip only lines up in
    /// a monospace face; egui's default hover text is proportional.
    fn hover_tooltip(response: &egui::Response, tooltip: &str) {
        if tooltip.is_empty() {
            return;
        }
        response.clone().on_hover_ui(|ui| {
            // on_hover_text caps the popup at this width on its own; a sync
            // error is long enough to need the cap that on_hover_ui drops.
            ui.set_max_width(ui.spacing().tooltip_width);
            ui.label(RichText::new(tooltip).monospace());
        });
    }

    /// Label and value on one line, a full-width bar under it, then the reset
    /// note and the pace badge.
    fn meter_row(ui: &mut egui::Ui, row: &tokengauge_core::PanelRow) {
        let fill = tone_color(row.tone);
        ui.horizontal(|ui| {
            ui.add_sized(
                [110.0, 18.0],
                egui::Label::new(RichText::new(&row.label).color(SUB)),
            );
            // Full width, like the bar rows below it. A bar pinned at 220pt
            // left the limits section ending halfway across a panel whose
            // other sections ran to the edge.
            let width = ui.available_width();
            ui.add(
                ProgressBar::new((row.fraction.unwrap_or(0.0) as f32).clamp(0.0, 1.0))
                    .desired_width(width)
                    .corner_radius(6)
                    .fill(fill)
                    .text(RichText::new(&row.value).small().strong().color(DARK)),
            );
        });
        if !row.footnote.is_empty() || !row.badge.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(110.0);
                if !row.footnote.is_empty() {
                    ui.label(RichText::new(&row.footnote).small().color(SUB));
                }
                if !row.badge.is_empty() {
                    ui.label(
                        RichText::new(format!("\u{b7} {}", row.badge))
                            .small()
                            .color(tone_color(row.badge_tone)),
                    );
                }
            });
        }
    }

    /// One line per row with the share bar filling the row behind the text.
    fn bar_row(ui: &mut egui::Ui, row: &tokengauge_core::PanelRow) {
        let fraction = (row.fraction.unwrap_or(0.0) as f32).clamp(0.0, 1.0);
        let value = if row.suffix.is_empty() {
            row.value.clone()
        } else {
            format!("{}  \u{b7}  {}", row.value, row.suffix)
        };
        let height = 20.0;
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height),
            egui::Sense::hover(),
        );
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, CARD);
        if fraction > 0.0 {
            let mut filled = rect;
            filled.set_width(rect.width() * fraction);
            painter.rect_filled(filled, 4.0, BORDER);
        }
        let color = if row.emphasized { TEXT } else { SUB };
        painter.text(
            rect.left_center() + egui::vec2(8.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &row.label,
            egui::FontId::proportional(12.0),
            color,
        );
        painter.text(
            rect.right_center() - egui::vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            &value,
            egui::FontId::monospace(12.0),
            color,
        );
        hover_tooltip(&response, &row.tooltip);
    }

    /// Label and value on one line; a badge and a suffix drop to a caption line
    /// under it, indented onto the value column the way a meter's footnote is.
    fn key_row(ui: &mut egui::Ui, row: &tokengauge_core::PanelRow) {
        let line = ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.add_sized(
                    [110.0, 18.0],
                    egui::Label::new(RichText::new(&row.label).color(SUB)),
                );
                ui.label(RichText::new(&row.value).monospace());
            });
            if !row.badge.is_empty() || !row.suffix.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(110.0);
                    if !row.badge.is_empty() {
                        ui.label(
                            RichText::new(&row.badge)
                                .small()
                                .color(tone_color(row.badge_tone)),
                        );
                    }
                    if !row.suffix.is_empty() {
                        // The separator divides a badge from a suffix, so a row
                        // with no badge must not open on one.
                        let text = if row.badge.is_empty() {
                            row.suffix.clone()
                        } else {
                            format!("\u{b7} {}", row.suffix)
                        };
                        ui.label(RichText::new(text).small().color(SUB));
                    }
                });
            }
        });
        // The suffix is the spec's ellipsized copy for surfaces that cannot
        // wrap; the tooltip carries the whole sentence, and the sync detail is
        // the row that actually needs it.
        hover_tooltip(&line.response, &row.tooltip);
    }

    fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
        egui::Frame::group(ui.style())
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, add);
    }

    /// Tray-icon palette for a usage percentage. `Tone::for_percent` owns where
    /// the tiers fall; the split inside critical is this icon's own, because a
    /// number on a 16px glyph needs the last stretch to read differently from
    /// the rest of the red band.
    fn usage_color(p: u8) -> Color32 {
        match Tone::for_percent(p) {
            Tone::Good => GREEN,
            Tone::Warn => YELLOW,
            Tone::Critical if p < 95 => PEACH,
            _ => RED,
        }
    }

    /// The pin picker's label for a choice. "highest" is the absence of a pin,
    /// and the four frontends called it four things - "Auto", "Highest",
    /// "Highest usage" and the raw id - for one setting.
    fn pin_label(choice: &str) -> String {
        if choice == "highest" {
            return "Highest usage".to_string();
        }
        PROVIDERS
            .iter()
            .find(|p| **p == choice)
            .map(|p| tokengauge_core::provider_label(p).to_string())
            .unwrap_or_else(|| cap(choice))
    }

    fn cap(s: &str) -> String {
        let mut chars = s.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    /// Tooltip text + the peak *session* usage percentage (what the icon shows).
    fn tray_summary(snap: &Snapshot) -> (String, Option<u8>) {
        if snap.rows.is_empty() {
            return ("TokenGauge — no data".to_string(), None);
        }
        let mut session_peak: Option<u8> = None;
        let mut lines = Vec::new();
        for r in &snap.rows {
            if let Some(p) = r.session_used {
                session_peak = Some(session_peak.map_or(p, |cur| cur.max(p)));
            }
            let s = r.session_used.map_or("—".to_string(), |p| format!("{p}%"));
            let w = r.weekly_used.map_or("—".to_string(), |p| format!("{p}%"));
            lines.push(format!("{}: session {s} · weekly {w}", cap(&r.provider)));
        }
        (lines.join("\n"), session_peak)
    }

    // --- Tray icon rendering (peak % drawn with a tiny 3x5 bitmap font) -------

    fn render_icon(number: Option<u8>, color: Color32) -> Icon {
        const W: usize = 32;
        const H: usize = 32;
        let mut px = vec![0u8; W * H * 4];
        // Rounded-ish filled square in `color`.
        for y in 0..H {
            for x in 0..W {
                let corner = !(3..W - 3).contains(&x) && !(3..H - 3).contains(&y);
                let i = (y * W + x) * 4;
                if !corner {
                    px[i] = color.r();
                    px[i + 1] = color.g();
                    px[i + 2] = color.b();
                    px[i + 3] = 255;
                }
            }
        }
        if let Some(n) = number {
            draw_number(&mut px, W, H, &n.to_string(), DARK);
        }
        Icon::from_rgba(px, W as u32, H as u32).expect("valid icon")
    }

    fn digit_rows(c: char) -> Option<[u8; 5]> {
        Some(match c {
            '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
            '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
            '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
            '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
            '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
            '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
            '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
            '7' => [0b111, 0b001, 0b010, 0b100, 0b100],
            '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
            '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
            _ => return None,
        })
    }

    fn draw_number(px: &mut [u8], w: usize, h: usize, s: &str, color: Color32) {
        let len = s.chars().count().max(1) as i32;
        // Fit width `scale*(4*len-1) <= 28` and height `5*scale <= 24`.
        let scale = ((28 / (4 * len - 1)).min(24 / 5)).clamp(1, 6) as usize;
        let dw = 3 * scale;
        let gap = scale;
        let total = (len as usize) * dw + (len as usize - 1) * gap;
        let mut x0 = w.saturating_sub(total) / 2;
        let y0 = h.saturating_sub(5 * scale) / 2;
        for c in s.chars() {
            if let Some(rows) = digit_rows(c) {
                for (r, row) in rows.iter().enumerate() {
                    for col in 0..3 {
                        if row & (0b100 >> col) != 0 {
                            for dy in 0..scale {
                                for dx in 0..scale {
                                    let x = x0 + col * scale + dx;
                                    let y = y0 + r * scale + dy;
                                    if x < w && y < h {
                                        let i = (y * w + x) * 4;
                                        px[i] = color.r();
                                        px[i + 1] = color.g();
                                        px[i + 2] = color.b();
                                        px[i + 3] = 255;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            x0 += dw + gap;
        }
    }

    fn show_window(ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        ctx.request_repaint();
    }

    /// Put the panel against the tray icon it was opened from: centred on the
    /// icon and above it, which is where the taskbar is on all but a minority
    /// of setups. When there is no room above (a taskbar at the top), it drops
    /// below the icon instead, and either way it is kept on the monitor.
    ///
    /// The icon rectangle arrives in physical pixels and every viewport command
    /// speaks points, so nothing here is right on a scaled display without the
    /// conversion.
    fn place_flyout(ctx: &egui::Context, (x, y, w, h): (f64, f64, f64, f64)) {
        let ppp = ctx.pixels_per_point().max(0.1);
        let size = ctx
            .input(|i| i.viewport().outer_rect.map(|r| r.size()))
            .unwrap_or(egui::vec2(PANEL_WIDTH, PANEL_HEIGHT));

        let icon_center_x = ((x + w / 2.0) as f32) / ppp;
        let icon_top = (y as f32) / ppp;
        let icon_bottom = ((y + h) as f32) / ppp;

        let mut pos = egui::pos2(
            icon_center_x - size.x / 2.0,
            icon_top - size.y - PANEL_MARGIN,
        );
        if pos.y < PANEL_MARGIN {
            pos.y = icon_bottom + PANEL_MARGIN;
        }
        if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
            pos.x = pos.x.clamp(
                PANEL_MARGIN,
                (monitor.x - size.x - PANEL_MARGIN).max(PANEL_MARGIN),
            );
            pos.y = pos.y.clamp(
                PANEL_MARGIN,
                (monitor.y - size.y - PANEL_MARGIN).max(PANEL_MARGIN),
            );
        } else {
            pos.x = pos.x.max(PANEL_MARGIN);
            pos.y = pos.y.max(PANEL_MARGIN);
        }
        ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos));
    }

    /// Open the TUI on its sync screen.
    ///
    /// The TUI is spawned directly rather than through `--sync-setup`, whose
    /// terminal discovery is Unix-shaped; on Windows the console the TUI opens
    /// in is the terminal.
    fn spawn_sync_setup() {
        let mut cmd = tui_command();
        let _ = cmd.arg("--sync").spawn();
    }

    /// The installed TUI beside this binary, falling back to `PATH`.
    fn tui_command() -> std::process::Command {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tokengauge-tui.exe")))
            .filter(|p| p.exists());
        match beside {
            Some(p) => std::process::Command::new(p),
            None => std::process::Command::new("tokengauge-tui"),
        }
    }

    /// Spawn `tokengauge-tui --update` (which owns the self-update code) to
    /// download the latest release and replace the installed binaries.
    fn spawn_update() {
        let mut cmd = tui_command();
        let _ = cmd.arg("--update").spawn();
    }

    fn tray_event_loop(
        ctx: egui::Context,
        action_tx: mpsc::Sender<Action>,
        quit: Arc<AtomicBool>,
        ids: MenuIds,
        flyout: Arc<Mutex<Flyout>>,
    ) {
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();
        loop {
            while let Ok(ev) = menu_rx.try_recv() {
                if ev.id == ids.show {
                    show_window(&ctx);
                } else if ev.id == ids.refresh {
                    let _ = action_tx.send(Action::Refresh);
                } else if ev.id == ids.sync {
                    spawn_sync_setup();
                } else if ev.id == ids.update {
                    // Quit as well: this binary is one of the two the update
                    // replaces. An MSI install goes through msiexec, which
                    // cannot replace a file this process holds open and would
                    // stop to ask about it; and even on the in-place path a
                    // tray left running keeps executing the old code until it
                    // is restarted anyway.
                    spawn_update();
                    quit.store(true, Ordering::SeqCst);
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    ctx.request_repaint();
                } else if ev.id == ids.quit {
                    // Ask the app to close so Drop runs (removes the tray icon)
                    // instead of exiting the process abruptly.
                    quit.store(true, Ordering::SeqCst);
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    ctx.request_repaint();
                }
            }
            while let Ok(ev) = tray_rx.try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    rect,
                    ..
                } = ev
                {
                    let mut flyout = flyout.lock().unwrap_or_else(|e| e.into_inner());
                    // Windows blurs the panel before it delivers the click that
                    // caused the blur, so the panel has already dismissed
                    // itself by now. Reopening it here would make the icon a
                    // button that can only ever open.
                    if flyout
                        .hidden_at
                        .is_some_and(|at| at.elapsed() < REOPEN_GRACE)
                    {
                        flyout.hidden_at = None;
                        continue;
                    }
                    flyout.hidden_at = None;
                    flyout.focused_once = false;
                    flyout.anchor = Some((
                        rect.position.x,
                        rect.position.y,
                        f64::from(rect.size.width),
                        f64::from(rect.size.height),
                    ));
                    drop(flyout);
                    show_window(&ctx);
                }
            }
            thread::sleep(Duration::from_millis(120));
        }
    }

    /// Rebuild the rows from the snapshot on disk, without fetching anything.
    ///
    /// Both the first paint and the tick below go through here: it is what
    /// picks up a fetch the daemon or another frontend made, and what makes the
    /// reset countdowns move. A countdown is measured against the clock at the
    /// moment a row is built, so it only advances when the rows are built
    /// again - the instant it counts down to is absolute, which is why a
    /// snapshot minutes old still yields the right one.
    fn load_from_cache(shared: &Mutex<Snapshot>, cfg_path: &std::path::Path) {
        let Ok(config) = load_config(Some(cfg_path.to_path_buf())) else {
            return;
        };
        let Ok(cached) = read_cache_full(&config.cache_file) else {
            return;
        };
        let (mut payloads, mut errors, costs) = cached.into_parts();
        retain_enabled(&mut payloads, &mut errors, &config.providers);
        let history = HistoryInputs::load(&config);
        let rows = payload_to_rows_with_costs(payloads, &costs)
            .iter()
            .map(|r| to_row(r, &history))
            .collect();
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        s.rows = rows;
        s.enabled = config
            .providers
            .enabled_providers()
            .into_iter()
            .map(str::to_string)
            .collect();
        s.primary = config.waybar.primary.clone().unwrap_or_default();
        s.errors = errors
            .iter()
            .map(|e| format!("{}: {}", e.provider, e.message))
            .collect();
    }

    fn fetch_loop(
        ctx: egui::Context,
        shared: Arc<Mutex<Snapshot>>,
        action_rx: mpsc::Receiver<Action>,
        cfg_path: std::path::PathBuf,
    ) {
        loop {
            {
                shared.lock().unwrap_or_else(|e| e.into_inner()).fetching = true;
            }
            ctx.request_repaint();

            let mut refresh_secs = 600u64;
            match load_config(Some(cfg_path.clone())) {
                Ok(config) => {
                    refresh_secs = config.refresh_secs.max(30);
                    let result = fetch_all_providers(&config);
                    let _ = write_cache_full(
                        &config.cache_file,
                        &result.payloads,
                        &result.errors,
                        &result.costs,
                        &config.providers,
                        Some(&result.sync),
                    );
                    let errors = result
                        .errors
                        .iter()
                        .map(|e| format!("{}: {}", e.provider, e.message))
                        .collect();
                    let history = HistoryInputs::load(&config);
                    let rows = payload_to_rows_with_costs(result.payloads, &result.costs)
                        .iter()
                        .map(|r| to_row(r, &history))
                        .collect();
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.rows = rows;
                    s.errors = errors;
                    s.enabled = config
                        .providers
                        .enabled_providers()
                        .into_iter()
                        .map(str::to_string)
                        .collect();
                    s.primary = config.waybar.primary.clone().unwrap_or_default();
                }
                // Surface a bad config instead of silently showing stale data -
                // there's no console to see the failure otherwise.
                Err(e) => {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.errors = vec![format!("config: {e}")];
                }
            }

            {
                shared.lock().unwrap_or_else(|e| e.into_inner()).fetching = false;
            }
            ctx.request_repaint();

            // A settings change rewrites the config and falls straight through
            // to the next fetch, so the pane never shows a toggle the config
            // does not yet carry.
            // Every iteration of this loop costs a full fetch of every
            // provider, staggered by `stagger_ms` to stay clear of 429s. Taking
            // one action per iteration would make a user flipping three
            // switches pay for three fetch cycles, so drain whatever else is
            // already queued before falling through to the fetch.
            let queued: Vec<Action> = action_rx
                .recv_timeout(Duration::from_secs(refresh_secs))
                .into_iter()
                .chain(action_rx.try_iter())
                .collect();
            for action in queued {
                let result = match action {
                    Action::SetProvider(name, enable) => {
                        config_set_oauth_provider(&cfg_path, &name, enable)
                    }
                    // "highest" is the absence of a pin, not a provider name.
                    Action::SetPrimary(name) => {
                        config_set_primary(&cfg_path, (name != "highest").then_some(name.as_str()))
                    }
                    Action::Refresh => Ok(()),
                };
                if let Err(e) = result {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.errors = vec![format!("config: {e}")];
                }
            }
        }
    }
}
