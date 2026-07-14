//! TokenGauge system-tray GUI for Windows.
//!
//! A small always-available window showing per-provider usage (session / weekly
//! bars, reset times), backed by a system-tray icon. Windows-only; on other
//! platforms this is a stub (the Linux surfaces are the Waybar module, GTK
//! popover, and KDE applet).

#[cfg(not(windows))]
fn main() {
    eprintln!("tokengauge-tray is Windows-only; use the Waybar / GTK / KDE surfaces on Linux.");
}

#[cfg(windows)]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([460.0, 380.0])
            .with_min_inner_size([340.0, 220.0])
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
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    use eframe::egui::{self, Color32, ProgressBar, ViewportCommand};
    use tokengauge_core::{
        ProviderRow, default_config_path, fetch_all_providers, load_config,
        payload_to_rows_with_costs, read_cache_full, write_cache_full, write_default_config,
    };
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
    use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

    type DynErr = Box<dyn std::error::Error + Send + Sync + 'static>;

    /// A rendered provider row (decoupled from core's `ProviderRow` so we don't
    /// depend on its `Clone`/internal shape).
    #[derive(Clone, Default)]
    struct Row {
        provider: String,
        plan: Option<String>,
        stale: bool,
        session_used: Option<u8>,
        session_reset: String,
        weekly_used: Option<u8>,
        weekly_reset: String,
    }

    fn to_row(r: &ProviderRow) -> Row {
        Row {
            provider: r.provider.clone(),
            plan: r.plan_label.clone(),
            stale: r.stale,
            session_used: r.session_used,
            session_reset: r.session_reset.clone(),
            weekly_used: r.weekly_used,
            weekly_reset: r.weekly_reset.clone(),
        }
    }

    #[derive(Default, Clone)]
    struct Snapshot {
        rows: Vec<Row>,
        errors: Vec<String>,
        fetching: bool,
    }

    pub struct TrayApp {
        shared: Arc<Mutex<Snapshot>>,
        refresh_tx: mpsc::Sender<()>,
        _tray: TrayIcon,
        _items: Vec<MenuItem>,
        show_id: MenuId,
        refresh_id: MenuId,
        quit_id: MenuId,
    }

    impl TrayApp {
        pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, DynErr> {
            let ctx = cc.egui_ctx.clone();
            let shared = Arc::new(Mutex::new(Snapshot::default()));
            let (refresh_tx, refresh_rx) = mpsc::channel::<()>();

            let cfg_path = default_config_path();
            if !cfg_path.exists() {
                let _ = write_default_config(&cfg_path);
            }

            // Seed from the shared cache for an instant first paint.
            if let Ok(config) = load_config(Some(cfg_path.clone()))
                && let Ok(cached) = read_cache_full(&config.cache_file)
            {
                let (payloads, errors, costs) = cached.into_parts();
                let rows = payload_to_rows_with_costs(payloads, &costs)
                    .iter()
                    .map(to_row)
                    .collect();
                let mut s = shared.lock().unwrap();
                s.rows = rows;
                s.errors = errors
                    .iter()
                    .map(|e| format!("{}: {}", e.provider, e.message))
                    .collect();
            }

            // Background fetch loop.
            {
                let ctx = ctx.clone();
                let shared = shared.clone();
                thread::spawn(move || fetch_loop(ctx, shared, refresh_rx, cfg_path));
            }

            // Tray icon + menu.
            let menu = Menu::new();
            let show_i = MenuItem::new("Show TokenGauge", true, None);
            let refresh_i = MenuItem::new("Refresh now", true, None);
            let quit_i = MenuItem::new("Quit", true, None);
            menu.append(&show_i)?;
            menu.append(&refresh_i)?;
            menu.append(&quit_i)?;
            let (show_id, refresh_id, quit_id) = (
                show_i.id().clone(),
                refresh_i.id().clone(),
                quit_i.id().clone(),
            );

            let tray = TrayIconBuilder::new()
                .with_tooltip("TokenGauge")
                .with_icon(make_icon())
                .with_menu(Box::new(menu))
                .build()?;

            Ok(Self {
                shared,
                refresh_tx,
                _tray: tray,
                _items: vec![show_i, refresh_i, quit_i],
                show_id,
                refresh_id,
                quit_id,
            })
        }

        fn show_window(ctx: &egui::Context) {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
    }

    impl eframe::App for TrayApp {
        // Runs before each `ui`, and (thanks to request_repaint) even while the
        // window is hidden - so tray clicks/menu still work when minimized.
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                if ev.id == self.show_id {
                    Self::show_window(ctx);
                } else if ev.id == self.refresh_id {
                    let _ = self.refresh_tx.send(());
                } else if ev.id == self.quit_id {
                    std::process::exit(0);
                }
            }
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } = ev
                {
                    Self::show_window(ctx);
                }
            }
            // Close button hides to tray instead of quitting.
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            // Keep polling tray events a few times per second, even when hidden.
            ctx.request_repaint_after(Duration::from_millis(300));
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let snap = self.shared.lock().unwrap().clone();

            ui.horizontal(|ui| {
                ui.heading("TokenGauge");
                if snap.fetching {
                    ui.spinner();
                    ui.weak("refreshing…");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh").clicked() {
                        let _ = self.refresh_tx.send(());
                    }
                });
            });
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                if snap.rows.is_empty() {
                    ui.weak("No usage data yet. Make sure codexbar_bin is set and signed in.");
                }
                for row in &snap.rows {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(&row.provider);
                            if let Some(plan) = &row.plan {
                                ui.weak(plan);
                            }
                            if row.stale {
                                ui.weak("(stale)");
                            }
                        });
                        usage_bar(ui, "Session", row.session_used, &row.session_reset);
                        usage_bar(ui, "Weekly", row.weekly_used, &row.weekly_reset);
                    });
                    ui.add_space(4.0);
                }

                if !snap.errors.is_empty() {
                    ui.separator();
                    ui.colored_label(Color32::from_rgb(0xf3, 0x8b, 0xa8), "Errors");
                    for e in &snap.errors {
                        ui.weak(e);
                    }
                }
            });
        }
    }

    fn usage_bar(ui: &mut egui::Ui, label: &str, used: Option<u8>, reset: &str) {
        ui.horizontal(|ui| {
            ui.add_sized([64.0, 16.0], egui::Label::new(label));
            match used {
                Some(p) => {
                    let frac = (p as f32 / 100.0).clamp(0.0, 1.0);
                    ui.add(
                        ProgressBar::new(frac)
                            .desired_width(230.0)
                            .text(format!("{p}%")),
                    );
                }
                None => {
                    ui.weak("no data");
                }
            }
            if !reset.is_empty() && reset != "—" {
                ui.weak(format!("· resets {reset}"));
            }
        });
    }

    /// A flat teal 32x32 icon (kept simple so we don't ship an asset).
    fn make_icon() -> Icon {
        let (w, h) = (32u32, 32u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[0x2e, 0xa0, 0x9b, 0xff]);
        }
        Icon::from_rgba(rgba, w, h).expect("valid icon")
    }

    fn fetch_loop(
        ctx: egui::Context,
        shared: Arc<Mutex<Snapshot>>,
        refresh_rx: mpsc::Receiver<()>,
        cfg_path: std::path::PathBuf,
    ) {
        loop {
            {
                shared.lock().unwrap().fetching = true;
            }
            ctx.request_repaint();

            let mut refresh_secs = 600u64;
            if let Ok(config) = load_config(Some(cfg_path.clone())) {
                refresh_secs = config.refresh_secs.max(30);
                let result = fetch_all_providers(&config);
                let _ = write_cache_full(
                    &config.cache_file,
                    &result.payloads,
                    &result.errors,
                    &result.costs,
                );
                let errors = result
                    .errors
                    .iter()
                    .map(|e| format!("{}: {}", e.provider, e.message))
                    .collect();
                let rows = payload_to_rows_with_costs(result.payloads, &result.costs)
                    .iter()
                    .map(to_row)
                    .collect();
                let mut s = shared.lock().unwrap();
                s.rows = rows;
                s.errors = errors;
            }

            {
                shared.lock().unwrap().fetching = false;
            }
            ctx.request_repaint();

            // Wait for the refresh interval, or wake early on a manual refresh.
            let _ = refresh_rx.recv_timeout(Duration::from_secs(refresh_secs));
        }
    }
}
