//! TokenGauge system-tray GUI for Windows.
//!
//! A small always-available window showing per-provider usage (session / weekly
//! bars, reset times), backed by a system-tray icon that renders the current
//! peak usage percentage. Windows-only; on other platforms this is a stub (the
//! Linux surfaces are the Waybar module, GTK popover, and KDE applet).

// Build as a GUI (windowless) binary on Windows so launching it doesn't pop a
// console window - important when it runs at login / from the tray.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    eprintln!("tokengauge-tray is Windows-only; use the Waybar / GTK / KDE surfaces on Linux.");
}

#[cfg(windows)]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([500.0, 440.0])
            .with_min_inner_size([380.0, 260.0])
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
    use std::time::Duration;

    use eframe::egui::{self, Color32, ProgressBar, RichText, ViewportCommand};
    use tokengauge_core::{
        ProviderRow, default_config_path, fetch_all_providers, load_config,
        payload_to_rows_with_costs, read_cache_full, retain_enabled, write_cache_full,
        write_default_config,
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

    /// A rendered provider row (decoupled from core's `ProviderRow`).
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
        quit: Arc<AtomicBool>,
        tray: TrayIcon,
        _items: Vec<MenuItem>,
        last_tip: String,
    }

    impl TrayApp {
        pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, DynErr> {
            let ctx = cc.egui_ctx.clone();

            let mut visuals = egui::Visuals::dark();
            visuals.panel_fill = BG;
            visuals.window_fill = BG;
            visuals.override_text_color = Some(TEXT);
            ctx.set_visuals(visuals);

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
                let (mut payloads, mut errors, costs) = cached.into_parts();
                retain_enabled(&mut payloads, &mut errors, &config.providers);
                let rows = payload_to_rows_with_costs(payloads, &costs)
                    .iter()
                    .map(to_row)
                    .collect();
                let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
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

            // Tray icon + menu. Left-click shows the window (not the menu).
            let menu = Menu::new();
            let show_i = MenuItem::new("Show TokenGauge", true, None);
            let refresh_i = MenuItem::new("Refresh now", true, None);
            let update_i = MenuItem::new("Update TokenGauge", true, None);
            let quit_i = MenuItem::new("Quit", true, None);
            menu.append(&show_i)?;
            menu.append(&refresh_i)?;
            menu.append(&update_i)?;
            menu.append(&quit_i)?;

            let tray = TrayIconBuilder::new()
                .with_tooltip("TokenGauge")
                .with_icon(render_icon(None, BLUE))
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()?;

            let quit = Arc::new(AtomicBool::new(false));

            // Handle tray/menu events on their own thread so they work even
            // while the window is hidden (the egui loop may not tick then).
            {
                let ctx = ctx.clone();
                let refresh_tx = refresh_tx.clone();
                let quit = quit.clone();
                let (show_id, refresh_id, update_id, quit_id) = (
                    show_i.id().clone(),
                    refresh_i.id().clone(),
                    update_i.id().clone(),
                    quit_i.id().clone(),
                );
                thread::spawn(move || {
                    tray_event_loop(
                        ctx, refresh_tx, quit, show_id, refresh_id, update_id, quit_id,
                    )
                });
            }

            Ok(Self {
                shared,
                refresh_tx,
                quit,
                tray,
                _items: vec![show_i, refresh_i, update_i, quit_i],
                last_tip: String::new(),
            })
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
            let snap = self
                .shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            self.sync_tray(&snap);

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
                            let btn =
                                egui::Button::new(RichText::new("⟳ Refresh").strong().color(DARK))
                                    .fill(BLUE)
                                    .corner_radius(6)
                                    .min_size(egui::vec2(0.0, 26.0));
                            if ui.add(btn).clicked() {
                                let _ = self.refresh_tx.send(());
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
                            if snap.rows.is_empty() {
                                ui.add_space(24.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(
                                        RichText::new("No usage data yet").size(15.0).color(SUB),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Set codexbar_bin and sign in to Win-CodexBar.",
                                        )
                                        .small()
                                        .color(SUB),
                                    );
                                });
                            }

                            for row in &snap.rows {
                                card(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(cap(&row.provider)).size(16.0).strong(),
                                        );
                                        if row.stale {
                                            ui.label(RichText::new("stale").small().color(PEACH));
                                        }
                                        if let Some(plan) = &row.plan {
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.label(
                                                        RichText::new(plan).small().color(MAUVE),
                                                    );
                                                },
                                            );
                                        }
                                    });
                                    ui.add_space(8.0);
                                    usage_row(ui, "Session", row.session_used, &row.session_reset);
                                    usage_row(ui, "Weekly", row.weekly_used, &row.weekly_reset);
                                });
                                ui.add_space(8.0);
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

    fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
        egui::Frame::group(ui.style())
            .fill(CARD)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(10)
            .inner_margin(egui::Margin::same(14))
            .show(ui, add);
    }

    fn usage_row(ui: &mut egui::Ui, label: &str, used: Option<u8>, reset: &str) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [62.0, 18.0],
                egui::Label::new(RichText::new(label).color(SUB)),
            );
            match used {
                Some(p) => {
                    ui.add(
                        ProgressBar::new((p as f32 / 100.0).clamp(0.0, 1.0))
                            .desired_width(190.0)
                            .corner_radius(6)
                            .fill(usage_color(p))
                            .text(RichText::new(format!("{p}%")).small().strong().color(DARK)),
                    );
                }
                None => {
                    ui.add_sized(
                        [190.0, 18.0],
                        egui::Label::new(RichText::new("no data").weak()),
                    );
                }
            }
            if !reset.is_empty() && reset != "—" {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("resets {reset}")).small().color(SUB));
                });
            }
        });
    }

    fn usage_color(p: u8) -> Color32 {
        match p {
            0..=49 => GREEN,
            50..=79 => YELLOW,
            80..=94 => PEACH,
            _ => RED,
        }
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

    /// Spawn `tokengauge-tui --update` (which owns the self-update code) to
    /// download the latest release and replace the installed binaries.
    fn spawn_update() {
        let tui = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("tokengauge-tui.exe")))
            .filter(|p| p.exists());
        let mut cmd = match tui {
            Some(p) => std::process::Command::new(p),
            None => std::process::Command::new("tokengauge-tui"),
        };
        let _ = cmd.arg("--update").spawn();
    }

    fn tray_event_loop(
        ctx: egui::Context,
        refresh_tx: mpsc::Sender<()>,
        quit: Arc<AtomicBool>,
        show_id: MenuId,
        refresh_id: MenuId,
        update_id: MenuId,
        quit_id: MenuId,
    ) {
        let menu_rx = MenuEvent::receiver();
        let tray_rx = TrayIconEvent::receiver();
        loop {
            while let Ok(ev) = menu_rx.try_recv() {
                if ev.id == show_id {
                    show_window(&ctx);
                } else if ev.id == refresh_id {
                    let _ = refresh_tx.send(());
                } else if ev.id == update_id {
                    spawn_update();
                } else if ev.id == quit_id {
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
                    ..
                } = ev
                {
                    show_window(&ctx);
                }
            }
            thread::sleep(Duration::from_millis(120));
        }
    }

    fn fetch_loop(
        ctx: egui::Context,
        shared: Arc<Mutex<Snapshot>>,
        refresh_rx: mpsc::Receiver<()>,
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
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.rows = rows;
                    s.errors = errors;
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

            let _ = refresh_rx.recv_timeout(Duration::from_secs(refresh_secs));
        }
    }
}
