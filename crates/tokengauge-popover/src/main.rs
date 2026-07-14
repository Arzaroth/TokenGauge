use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, Local, Weekday};
use clap::Parser;
use gtk4::glib::{ControlFlow, Propagation, source};
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GBox, Button, CheckButton, CssProvider,
    EventControllerKey, Expander, Grid, Image, Label, Orientation, ProgressBar, ScrolledWindow,
    Stack, Switch, ToggleButton, Widget,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use tokengauge_core::{
    ClickAction, CostInfo, ProviderRow, TokenGaugeConfig, WaybarPlacement,
    config_set_oauth_provider, config_set_primary, format_tokens, format_updated_relative,
    load_config, payload_to_rows_with_costs, provider_icon, provider_icon_svg_path, provider_label,
    read_cache_full, read_update_status, read_waybar_state, signal_daemon_reload, theme,
    waybar_state_path, window_labels,
};

const APP_ID: &str = "io.arzaroth.tokengauge.popover";

#[derive(Parser, Debug)]
#[command(version, about = "GTK4 popover for TokenGauge")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
    /// If a popover is already running, send SIGTERM to it and exit.
    /// Otherwise open the popover normally.
    #[arg(long)]
    toggle: bool,
}

fn pid_file_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/tokengauge-popover.pid"))
}

/// Returns true if the popover was already running and we asked it to close.
fn maybe_close_existing() -> bool {
    let path = pid_file_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<i32>() else {
        return false;
    };
    // kill(0) probes for process existence.
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    if !alive {
        let _ = std::fs::remove_file(&path);
        return false;
    }
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = std::fs::remove_file(&path);
    true
}

fn write_pid_file() {
    let pid = std::process::id();
    let _ = std::fs::write(pid_file_path(), pid.to_string());
}

fn clear_pid_file() {
    let _ = std::fs::remove_file(pid_file_path());
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);
    let config = if config_path.exists() {
        load_config(Some(config_path.clone())).context("loading config")?
    } else {
        TokenGaugeConfig::default()
    };
    tokengauge_core::install_theme(config.theme.resolve());

    if args.toggle && maybe_close_existing() {
        return Ok(());
    }

    let app = Application::builder().application_id(APP_ID).build();
    let config_rc = Rc::new(config);
    let path_rc = Rc::new(config_path);

    app.connect_activate(move |app| {
        build_window(app, Rc::clone(&config_rc), Rc::clone(&path_rc));
    });

    // GTK's Application uses argv for command-line handling; pass an empty
    // arg list so clap-handled flags don't leak through.
    let exit = app.run_with_args::<String>(&[]);
    clear_pid_file();
    if exit.value() != 0 {
        std::process::exit(exit.value());
    }
    Ok(())
}

fn build_window(app: &Application, config: Rc<TokenGaugeConfig>, config_path: Rc<PathBuf>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("TokenGauge")
        .default_width(440)
        .default_height(520)
        .resizable(false)
        .build();

    // Layer-shell setup: anchor to the top edge and the side that matches
    // the user's waybar placement.
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    window.set_anchor(Edge::Top, true);
    match config.waybar.placement {
        WaybarPlacement::Left => {
            window.set_anchor(Edge::Left, true);
            window.set_margin(Edge::Left, config.waybar.popover_margin_side);
        }
        WaybarPlacement::Right => {
            window.set_anchor(Edge::Right, true);
            window.set_margin(Edge::Right, config.waybar.popover_margin_side);
        }
    }
    window.set_margin(Edge::Top, config.waybar.popover_margin_top);

    install_css();

    let outer = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(0)
        .css_classes(vec!["tg-root".to_string()])
        .build();
    outer.append(&header_bar());

    let scroller = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vscrollbar_policy(gtk4::PolicyType::Automatic)
        .build();
    let body = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .css_classes(vec!["tg-body".to_string()])
        .build();
    scroller.set_child(Some(&body));
    outer.append(&scroller);

    // Footer with action buttons.
    let footer = GBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .css_classes(vec!["tg-footer".to_string()])
        .halign(Align::End)
        .build();
    let btn_settings = Button::builder().label("⚙  Settings").build();
    let btn_refresh = Button::builder().label("↻  Refresh").build();
    let btn_tui = Button::builder().label("  Open TUI").build();
    let btn_close = Button::builder().label("✕  Close").build();
    footer.append(&btn_settings);
    footer.append(&btn_refresh);
    footer.append(&btn_tui);
    footer.append(&btn_close);

    // Update button - only shown when the daemon's cached release check found a
    // newer version. Clicking shells out to `tokengauge-waybar --update`.
    if let Some(status) = read_update_status(&config.cache_file) {
        if status.available {
            let label = match &status.latest {
                Some(v) => format!("⬆  Update to v{v}"),
                None => "⬆  Update".to_string(),
            };
            let btn_update = Button::builder()
                .label(label)
                .css_classes(vec!["tg-update".to_string()])
                .build();
            let btn = btn_update.clone();
            btn_update.connect_clicked(move |_| {
                spawn_update();
                btn.set_label("⬆  Updating...");
                btn.set_sensitive(false);
            });
            footer.prepend(&btn_update);
        }
    }

    outer.append(&footer);

    window.set_child(Some(&outer));

    // Initial content render. Active tab + scroll position are stashed so
    // refreshes (manual or auto) preserve the user's selection and don't
    // jump back to the top of the page.
    let body_rc = body.clone();
    let scroller_rc = scroller.clone();
    let cfg_for_refresh = Rc::clone(&config);
    let path_for_render = Rc::clone(&config_path);
    let active_tab: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let active_tab_for_render = Rc::clone(&active_tab);
    let settings_mode: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let settings_mode_for_render = Rc::clone(&settings_mode);
    let do_render = Rc::new(RefCell::new(move || {
        if *settings_mode_for_render.borrow() {
            render_settings(&scroller_rc, &body_rc, &path_for_render);
        } else {
            render_body(
                &scroller_rc,
                &body_rc,
                &cfg_for_refresh,
                &active_tab_for_render,
            );
        }
    }));
    (do_render.borrow_mut())();

    // Settings button: flip the body between the provider view and the inline
    // settings pane.
    {
        let render = Rc::clone(&do_render);
        let mode = Rc::clone(&settings_mode);
        let btn = btn_settings.clone();
        btn_settings.connect_clicked(move |_| {
            let now_settings = !*mode.borrow();
            *mode.borrow_mut() = now_settings;
            btn.set_label(if now_settings {
                "‹  Back"
            } else {
                "⚙  Settings"
            });
            (render.borrow_mut())();
        });
    }

    // Refresh button: ask daemon to fetch, then re-render after a beat.
    {
        let cfg = Rc::clone(&config);
        let render = Rc::clone(&do_render);
        btn_refresh.connect_clicked(move |_| {
            send_refresh(&cfg);
            let render = Rc::clone(&render);
            source::timeout_add_local(Duration::from_millis(400), move || {
                (render.borrow_mut())();
                ControlFlow::Break
            });
        });
    }

    // Open TUI: spawn launcher, close popover.
    {
        let cfg = Rc::clone(&config);
        let win = window.clone();
        btn_tui.connect_clicked(move |_| {
            spawn_tui(&cfg);
            win.close();
        });
    }

    // Close button.
    {
        let win = window.clone();
        btn_close.connect_clicked(move |_| {
            win.close();
        });
    }

    // Escape closes too.
    let keys = EventControllerKey::new();
    let win_for_keys = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk4::gdk::Key::Escape {
            win_for_keys.close();
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    window.add_controller(keys);

    // No auto-refresh: it would wipe the body every tick, which loses both
    // scroll position and animation state. The user clicks the Refresh
    // button if they want fresh data mid-view.

    write_pid_file();
    window.connect_close_request(move |_| {
        clear_pid_file();
        Propagation::Proceed
    });
    window.present();
}

fn header_bar() -> GBox {
    let bar = GBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .css_classes(vec!["tg-header".to_string()])
        .build();
    let title = Label::builder()
        .label("TokenGauge")
        .css_classes(vec!["tg-title".to_string()])
        .halign(Align::Start)
        .hexpand(true)
        .build();
    let stamp = Label::builder()
        .label(format!("updated {}", Local::now().format("%H:%M")))
        .css_classes(vec!["tg-dim".to_string()])
        .halign(Align::End)
        .build();
    bar.append(&title);
    bar.append(&stamp);
    bar
}

fn settings_section(text: &str) -> Label {
    Label::builder()
        .label(text)
        .css_classes(vec!["tg-settings-section".to_string()])
        .halign(Align::Start)
        .build()
}

/// Inline settings pane: toggle OAuth providers and pick the bar-pinned
/// provider. Changes are written to the config file live (comments preserved)
/// and the daemon is signalled to reload.
fn render_settings(scroller: &ScrolledWindow, body: &GBox, config_path: &Rc<PathBuf>) {
    // Load fresh from disk each render so the pane reflects edits made by the
    // toggles below (the in-memory snapshot from startup goes stale otherwise).
    let config = load_config(Some((**config_path).clone())).unwrap_or_default();

    // Rebuild the pane after a provider edit so it drops from the bar-pin list
    // immediately. Deferred to idle so the switch that fired the signal isn't
    // destroyed mid-callback.
    let rerender: Rc<dyn Fn()> = Rc::new({
        let scroller = scroller.clone();
        let body = body.clone();
        let config_path = Rc::clone(config_path);
        move || {
            let (s, b, p) = (scroller.clone(), body.clone(), Rc::clone(&config_path));
            gtk4::glib::idle_add_local_once(move || render_settings(&s, &b, &p));
        }
    });

    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    scroller.vadjustment().set_value(0.0);

    // --- Providers ---
    body.append(&settings_section("Providers"));
    for (key, label, enabled) in [
        ("codex", "Codex", config.providers.codex.unwrap_or(false)),
        ("claude", "Claude", config.providers.claude.unwrap_or(false)),
    ] {
        let row = GBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let name = Label::builder()
            .label(label)
            .halign(Align::Start)
            .hexpand(true)
            .build();
        let sw = Switch::builder()
            .active(enabled)
            .valign(Align::Center)
            .build();
        let path = Rc::clone(config_path);
        let k = key.to_string();
        let rerender = Rc::clone(&rerender);
        sw.connect_state_set(move |_, state| {
            if let Err(e) = config_set_oauth_provider(&path, &k, state) {
                eprintln!("tokengauge-popover: failed to update config: {e:#}");
            } else {
                signal_daemon_reload();
                rerender();
            }
            Propagation::Proceed
        });
        row.append(&name);
        row.append(&sw);
        body.append(&row);
    }
    // API providers are enabled by adding an api_key to the config; show their
    // state read-only here.
    for (label, on) in [
        ("z.ai", config.providers.zai.is_some()),
        ("Kimi K2", config.providers.kimik2.is_some()),
        ("Copilot", config.providers.copilot.is_some()),
        ("MiniMax", config.providers.minimax.is_some()),
        ("Kimi", config.providers.kimi.is_some()),
    ] {
        let row = GBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(8)
            .build();
        let name = Label::builder()
            .label(label)
            .halign(Align::Start)
            .hexpand(true)
            .css_classes(vec!["tg-dim".to_string()])
            .build();
        let state = Label::builder()
            .label(if on { "on (api_key)" } else { "needs api_key" })
            .css_classes(vec!["tg-dim".to_string()])
            .halign(Align::End)
            .build();
        row.append(&name);
        row.append(&state);
        body.append(&row);
    }

    // --- Bar pin ---
    body.append(&settings_section("Bar pin"));
    let current = config.waybar.primary.clone();
    let highest = CheckButton::builder()
        .label("Highest (most constrained)")
        .active(current.is_none())
        .build();
    body.append(&highest);

    let mut prev = highest.clone();
    for provider in config.providers.enabled_providers() {
        let name = provider.name.clone();
        let rb = CheckButton::builder().label(provider_label(&name)).build();
        rb.set_group(Some(&prev));
        rb.set_active(current.as_deref() == Some(name.as_str()));
        let path = Rc::clone(config_path);
        rb.connect_toggled(move |b| {
            if b.is_active() {
                if let Err(e) = config_set_primary(&path, Some(&name)) {
                    eprintln!("tokengauge-popover: failed to update config: {e:#}");
                } else {
                    signal_daemon_reload();
                }
            }
        });
        body.append(&rb);
        prev = rb;
    }
    // Connect "Highest" last so the set_active calls above don't fire it.
    let path = Rc::clone(config_path);
    highest.connect_toggled(move |b| {
        if b.is_active() {
            if let Err(e) = config_set_primary(&path, None) {
                eprintln!("tokengauge-popover: failed to update config: {e:#}");
            } else {
                signal_daemon_reload();
            }
        }
    });
}

fn render_body(
    scroller: &ScrolledWindow,
    body: &GBox,
    config: &TokenGaugeConfig,
    active_tab: &Rc<RefCell<Option<String>>>,
) {
    // Stash scroll position; the layout-settled restore lives at the end.
    let saved_scroll = scroller.vadjustment().value();
    // Clear existing children.
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }

    let cached = match read_cache_full(&config.cache_file) {
        Ok(c) => c,
        Err(e) => {
            let err = Label::builder()
                .label(format!("No cached data: {e}"))
                .css_classes(vec!["tg-err".to_string()])
                .halign(Align::Start)
                .build();
            body.append(&err);
            return;
        }
    };
    let (payloads, errors, costs) = cached.into_parts();
    let rows = payload_to_rows_with_costs(payloads, &costs);

    if rows.is_empty() && errors.is_empty() {
        body.append(
            &Label::builder()
                .label("No providers returned data yet.")
                .css_classes(vec!["tg-dim".to_string()])
                .halign(Align::Start)
                .build(),
        );
        return;
    }

    if !rows.is_empty() {
        // Stack of one card per provider. Disable homogeneous sizing so
        // the popover sizes to the currently-visible page instead of
        // reserving the max width/height across every provider.
        let stack = Stack::builder()
            .hhomogeneous(false)
            .vhomogeneous(false)
            .interpolate_size(false)
            .build();
        for (i, row) in rows.iter().enumerate() {
            let page_name = format!("p{i}");
            stack.add_named(&provider_card_contents(row), Some(&page_name));
        }

        // Custom tab strip: per-provider ToggleButton with icon + name +
        // session usage subtext (matching codexbar's popup style).
        let tabs_row = GBox::builder()
            .orientation(Orientation::Horizontal)
            .spacing(2)
            .css_classes(vec!["tg-tabs".to_string()])
            .halign(Align::Fill)
            .hexpand(true)
            .build();
        let mut buttons: Vec<ToggleButton> = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let btn = build_tab_button(row);
            btn.set_hexpand(true);
            if i > 0 {
                btn.set_group(Some(&buttons[0]));
            }
            tabs_row.append(&btn);
            buttons.push(btn);
        }
        // Wire each button: when toggled on, swap visible stack child and
        // remember the selection.
        for (i, btn) in buttons.iter().enumerate() {
            let stack = stack.clone();
            let page = format!("p{i}");
            let active = Rc::clone(active_tab);
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    stack.set_visible_child_name(&page);
                    *active.borrow_mut() = Some(page.clone());
                }
            });
        }

        // Pick initial active tab in priority order:
        //   1. Prior selection (carried across refresh)
        //   2. Currently-selected provider in waybar (set by mouse scroll)
        //   3. config.primary
        //   4. First provider
        let scroll_selected = read_waybar_state(&waybar_state_path(&config.cache_file)).selected;
        let preferred = active_tab
            .borrow()
            .clone()
            .or_else(|| {
                scroll_selected.as_ref().and_then(|p| {
                    rows.iter()
                        .position(|r| r.provider.to_lowercase() == p.to_lowercase())
                        .map(|idx| format!("p{idx}"))
                })
            })
            .or_else(|| {
                config.waybar.primary.as_ref().and_then(|p| {
                    rows.iter()
                        .position(|r| r.provider.to_lowercase() == p.to_lowercase())
                        .map(|idx| format!("p{idx}"))
                })
            })
            .unwrap_or_else(|| "p0".to_string());
        let initial = preferred
            .strip_prefix('p')
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|i| *i < buttons.len())
            .unwrap_or(0);
        buttons[initial].set_active(true);
        stack.set_visible_child_name(&format!("p{initial}"));
        *active_tab.borrow_mut() = Some(format!("p{initial}"));

        body.append(&tabs_row);
        body.append(&stack);
    }

    if !errors.is_empty() {
        let err_box = GBox::builder()
            .orientation(Orientation::Vertical)
            .css_classes(vec!["tg-errors".to_string()])
            .spacing(4)
            .build();
        let head = Label::builder()
            .label("Errors")
            .css_classes(vec!["tg-section-title".to_string()])
            .halign(Align::Start)
            .build();
        err_box.append(&head);
        for e in &errors {
            err_box.append(
                &Label::builder()
                    .label(format!("{}: {}", e.provider, e.message))
                    .css_classes(vec!["tg-err".to_string()])
                    .halign(Align::Start)
                    .wrap(true)
                    .build(),
            );
        }
        body.append(&err_box);
    }

    // After GTK settles the rebuilt layout, restore the scroll offset so
    // periodic refreshes don't jump the view back to the top. We hook the
    // vadjustment's "changed" signal (fires once the new content size is
    // known) and restore exactly once.
    if saved_scroll > 0.0 {
        let adj = scroller.vadjustment();
        let handler: Rc<RefCell<Option<gtk4::glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
        let handler_for_cb = Rc::clone(&handler);
        let adj_for_cb = adj.clone();
        let id = adj.connect_changed(move |adj| {
            if adj.upper() - adj.page_size() < 1.0 {
                return;
            }
            let clamped = saved_scroll.min(adj.upper() - adj.page_size()).max(0.0);
            adj.set_value(clamped);
            if let Some(id) = handler_for_cb.borrow_mut().take() {
                adj_for_cb.disconnect(id);
            }
        });
        *handler.borrow_mut() = Some(id);
    }
}

/// Build one tab toggle button: icon glyph on top, provider name middle,
/// thin session-usage bar at the bottom (codexbar-style, compact).
fn build_tab_button(row: &ProviderRow) -> ToggleButton {
    let v = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(1)
        .halign(Align::Center)
        .build();
    let icon_widget = build_tab_icon(row);

    let name_label = Label::builder()
        .label(row.provider.clone())
        .css_classes(vec!["tg-tab-name".to_string()])
        .build();

    // Session usage as a thin bar underneath the name (replaces "8% used"
    // text). Tier-color tinted so the colour itself communicates severity.
    let used = row.session_used.or(row.weekly_used);
    let bar = ProgressBar::builder()
        .css_classes(vec!["tg-tab-usage-bar".to_string()])
        .hexpand(true)
        .show_text(false)
        .build();
    match used {
        Some(pct) => {
            let p = pct.min(100);
            bar.set_fraction(p as f64 / 100.0);
            bar.add_css_class(tier_class(p));
        }
        None => {
            bar.set_fraction(0.0);
            bar.add_css_class("tg-tier-none");
        }
    }
    v.append(&icon_widget);
    v.append(&name_label);
    v.append(&bar);

    ToggleButton::builder()
        .child(&v)
        .css_classes(vec!["tg-tab".to_string()])
        .hexpand(true)
        .build()
}

/// Tab icon: the bundled brand SVG logo when installed, otherwise the
/// brand-coloured glyph fallback.
fn build_tab_icon(row: &ProviderRow) -> Widget {
    if let Some(path) = provider_icon_svg_path(&row.provider) {
        let image = Image::from_file(&path);
        image.set_pixel_size(20);
        image.add_css_class("tg-tab-icon");
        return image.upcast();
    }

    let icon = provider_icon(&row.provider);
    let icon_label = Label::builder()
        .label(icon.glyph)
        .css_classes(vec!["tg-tab-icon".to_string()])
        .build();
    let inline_css = format!("label.tg-tab-icon {{ color: {}; }}", icon.color_hex);
    let provider_css = CssProvider::new();
    provider_css.load_from_data(&inline_css);
    icon_label
        .style_context()
        .add_provider(&provider_css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    icon_label.upcast()
}

/// Page body for a provider (no outer Frame; StackSwitcher provides the tab
/// header so the card chrome is redundant inside the popover).
fn provider_card_contents(row: &ProviderRow) -> GBox {
    let inner = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(6)
        .css_classes(vec!["tg-card".to_string()])
        .build();

    // Header: provider name + plan + updated.
    let header = GBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .build();
    let name = Label::builder()
        .label(row.provider.clone())
        .css_classes(vec!["tg-provider".to_string()])
        .halign(Align::Start)
        .hexpand(true)
        .build();
    header.append(&name);
    let mut meta_parts: Vec<String> = Vec::new();
    if let Some(plan) = row.plan_label.as_ref().filter(|s| !s.is_empty()) {
        meta_parts.push(plan.clone());
    }
    if let Some(rel) = row.updated_iso.as_deref().and_then(format_updated_relative) {
        meta_parts.push(format!("updated {rel}"));
    }
    if !meta_parts.is_empty() {
        let meta = Label::builder()
            .label(meta_parts.join(" · "))
            .css_classes(vec!["tg-dim".to_string()])
            .halign(Align::End)
            .build();
        header.append(&meta);
    }
    inner.append(&header);

    // Usage gauges grid.
    let grid = Grid::builder()
        .column_spacing(10)
        .row_spacing(3)
        .hexpand(true)
        .build();
    let (s_label, w_label, t_label) = window_labels(&row.provider);
    let mut idx: i32 = 0;
    push_gauge_row(&grid, idx, s_label, row.session_used, &row.session_reset);
    idx += 1;
    push_gauge_row(&grid, idx, w_label, row.weekly_used, &row.weekly_reset);
    idx += 1;
    if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
        push_gauge_row(&grid, idx, t_label, row.tertiary_used, &row.tertiary_reset);
        idx += 1;
    }
    for extra in &row.extra_windows {
        push_gauge_row(&grid, idx, &extra.title, extra.used, &extra.reset);
        idx += 1;
    }
    inner.append(&grid);

    if let Some(cost) = row.cost.as_ref() {
        inner.append(&cost_section(cost));
    }
    if row.credits != "—" && !row.credits.is_empty() {
        let credits = Label::builder()
            .label(format!("Credits: ${}", row.credits))
            .css_classes(vec!["tg-credits".to_string()])
            .halign(Align::Start)
            .build();
        inner.append(&credits);
    }
    inner
}

fn push_gauge_row(grid: &Grid, row: i32, label: &str, used: Option<u8>, reset: &str) {
    let l = Label::builder()
        .label(label.to_string())
        .css_classes(vec!["tg-row-label".to_string()])
        .halign(Align::Start)
        .build();
    grid.attach(&l, 0, row, 1, 1);
    let bar = ProgressBar::builder()
        .css_classes(vec!["tg-gauge".to_string()])
        .hexpand(true)
        .show_text(true)
        .build();
    match used {
        Some(pct) => {
            let p = pct.min(100);
            bar.set_fraction(p as f64 / 100.0);
            bar.set_text(Some(&format!("{p}%")));
            bar.add_css_class(tier_class(p));
        }
        None => {
            bar.set_fraction(0.0);
            bar.set_text(Some("no data"));
            bar.add_css_class("tg-tier-none");
        }
    }
    grid.attach(&bar, 1, row, 1, 1);
    let trailing = match (used, reset) {
        (None, _) => "no data".to_string(),
        (Some(0), "—") => String::new(),
        (Some(_), "—") => "not started".to_string(),
        (Some(_), r) => format!("resets {r}"),
    };
    let t = Label::builder()
        .label(trailing)
        .css_classes(vec!["tg-dim".to_string()])
        .halign(Align::End)
        .wrap(true)
        .build();
    grid.attach(&t, 2, row, 1, 1);
}

fn tier_class(pct: u8) -> &'static str {
    match pct {
        0..=49 => "tg-tier-ok",
        50..=79 => "tg-tier-warn",
        _ => "tg-tier-crit",
    }
}

fn weekday_initial(w: Weekday) -> &'static str {
    match w {
        Weekday::Mon => "M",
        Weekday::Tue | Weekday::Thu => "T",
        Weekday::Wed => "W",
        Weekday::Fri => "F",
        Weekday::Sat | Weekday::Sun => "S",
    }
}

fn cost_section(cost: &CostInfo) -> GBox {
    let cont = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(4)
        .css_classes(vec!["tg-cost".to_string()])
        .build();
    let title = Label::builder()
        .label("Cost")
        .css_classes(vec!["tg-section-title".to_string()])
        .halign(Align::Start)
        .build();
    cont.append(&title);

    let grid = Grid::builder()
        .column_spacing(14)
        .row_spacing(1)
        .hexpand(true)
        .build();
    let mut row: i32 = 0;
    // Pad value to a width that fits the widest amount in this card, so
    // decimal points line up under a monospace font.
    let widest = [
        cost.burn_rate
            .as_ref()
            .map(|b| b.cost_per_hour)
            .unwrap_or(0.0),
        cost.session_usd,
        cost.weekly_usd,
        cost.today_usd,
        cost.monthly_usd,
    ]
    .iter()
    .copied()
    .fold(0.0_f64, f64::max);
    // Pad the WHOLE "$X.XX" string right-aligned so the $ sits flush
    // against the digits (with leading spaces before the $), matching the
    // tooltip / TUI alignment.
    let widest_money = format!("${widest:.2}").chars().count();
    let fmt_money = |v: f64| -> String { format!("{:>w$}", format!("${v:.2}"), w = widest_money) };

    if let Some(br) = cost.burn_rate.as_ref() {
        let trend = cost
            .avg_hourly_cost()
            .filter(|a| *a > 0.0)
            .map(|avg| {
                let pct = ((br.cost_per_hour - avg) / avg) * 100.0;
                let arrow = if pct >= 0.0 { "↑" } else { "↓" };
                format!("/hr   {arrow}{:.0}% vs 7d avg", pct.abs())
            })
            .unwrap_or_else(|| "/hr".to_string());
        push_cost_row(&grid, row, "Rate", &fmt_money(br.cost_per_hour), &trend);
        row += 1;
    }
    if cost.session_usd > 0.0 {
        push_cost_row(&grid, row, "Session", &fmt_money(cost.session_usd), "");
        row += 1;
    }
    if cost.weekly_usd > 0.0 {
        push_cost_row(&grid, row, "Weekly", &fmt_money(cost.weekly_usd), "");
        row += 1;
    }
    push_cost_row(
        &grid,
        row,
        "Today",
        &fmt_money(cost.today_usd),
        &format!("·  {} tokens", format_tokens(cost.today_tokens)),
    );
    row += 1;
    push_cost_row(
        &grid,
        row,
        "Month",
        &fmt_money(cost.monthly_usd),
        &format!("·  {} tokens", format_tokens(cost.monthly_tokens)),
    );
    cont.append(&grid);

    if !cost.weekly_cost_history.is_empty() {
        // Tuck the 7-day chart behind a collapsible Expander so the
        // default cost view stays compact.
        let expander = Expander::builder()
            .label("7-day cost")
            .css_classes(vec!["tg-expander".to_string()])
            .expanded(false)
            .build();
        expander.set_child(Some(&seven_day_chart(cost)));
        cont.append(&expander);
    }

    cont
}

fn push_cost_row(grid: &Grid, row: i32, label: &str, value: &str, suffix: &str) {
    let l = Label::builder()
        .label(label.to_string())
        .css_classes(vec!["tg-row-label".to_string()])
        .halign(Align::Start)
        .build();
    // Right-align the dollar amount so decimal points line up across rows
    // (matches the TUI / tooltip alignment).
    let v = Label::builder()
        .label(value.to_string())
        .css_classes(vec!["tg-money".to_string()])
        .halign(Align::End)
        .build();
    grid.attach(&l, 0, row, 1, 1);
    grid.attach(&v, 1, row, 1, 1);
    if !suffix.is_empty() {
        let s = Label::builder()
            .label(suffix.to_string())
            .css_classes(vec!["tg-dim".to_string()])
            .halign(Align::Start)
            .hexpand(true)
            .wrap(true)
            .build();
        grid.attach(&s, 2, row, 1, 1);
    }
}

fn seven_day_chart(cost: &CostInfo) -> GBox {
    let cont = GBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .build();
    let max = cost
        .weekly_cost_history
        .iter()
        .copied()
        .fold(0.0_f64, f64::max);
    let today = Local::now().date_naive();
    let n = cost.weekly_cost_history.len();

    // 7 columns, each fixed-width so the row never exceeds the popover.
    let bar_row = GBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .halign(Align::Fill)
        .hexpand(true)
        .build();
    for (i, usd) in cost.weekly_cost_history.iter().enumerate() {
        let offset = (n - 1 - i) as i64;
        let day = today - ChronoDuration::days(offset);
        let pct = if max > 0.0 { (usd / max).max(0.0) } else { 0.0 };
        let col = GBox::builder()
            .orientation(Orientation::Vertical)
            .spacing(1)
            .halign(Align::Center)
            .hexpand(true)
            .build();
        let bar = ProgressBar::builder()
            .css_classes(vec!["tg-7d-bar".to_string()])
            .show_text(false)
            .fraction(pct)
            .build();
        bar.set_orientation(gtk4::Orientation::Vertical);
        bar.set_inverted(true);
        bar.set_size_request(8, 36);
        bar.set_halign(Align::Center);
        col.append(&bar);
        col.append(
            &Label::builder()
                .label(weekday_initial(day.weekday()))
                .css_classes(vec!["tg-dim".to_string()])
                .halign(Align::Center)
                .build(),
        );
        col.append(
            &Label::builder()
                .label(format!("${usd:.0}"))
                .css_classes(vec!["tg-7d-amount".to_string()])
                .halign(Align::Center)
                .build(),
        );
        bar_row.append(&col);
    }
    cont.append(&bar_row);
    cont
}

fn install_css() {
    let provider = CssProvider::new();
    provider.load_from_data(include_str!("style.css"));
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn socket_path_from_config(config: &TokenGaugeConfig) -> PathBuf {
    let parent = config.cache_file.parent().unwrap_or(Path::new("."));
    parent.join("tokengauge.sock")
}

fn send_refresh(config: &TokenGaugeConfig) {
    let sock = socket_path_from_config(config);
    let Ok(mut stream) = UnixStream::connect(&sock) else {
        return;
    };
    let cmd = r#"{"cmd":"refresh"}"#;
    let _ = writeln!(stream, "{cmd}");
    let _ = stream.flush();
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    let _ = reader.read_line(&mut buf);
}

fn spawn_tui(config: &TokenGaugeConfig) {
    // Reuse the launcher logic by shelling to tokengauge-waybar --click
    // when click_action is currently set to Tui, otherwise build a sane
    // default ourselves.
    let cmd_str = if matches!(config.waybar.click_action, ClickAction::Tui) {
        if !config.waybar.tui_command.trim().is_empty() {
            config.waybar.tui_command.clone()
        } else {
            default_terminal_launcher()
        }
    } else {
        default_terminal_launcher()
    };
    if cmd_str.is_empty() {
        return;
    }
    let _ = Command::new("sh")
        .arg("-c")
        .arg(cmd_str)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn spawn_update() {
    let mut cmd = Command::new("tokengauge-waybar");
    cmd.arg("--update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = std::env::var_os("TOKENGAUGE_CONFIG") {
        cmd.env("TOKENGAUGE_CONFIG", path);
    }
    let _ = cmd.spawn();
}

fn default_terminal_launcher() -> String {
    if which("omarchy-launch-or-focus-tui").is_some() {
        return "omarchy-launch-or-focus-tui tokengauge-tui".to_string();
    }
    let candidates: Vec<String> = std::env::var("TERMINAL")
        .ok()
        .into_iter()
        .chain(
            ["ghostty", "alacritty", "kitty", "wezterm", "foot", "xterm"]
                .iter()
                .map(|s| s.to_string()),
        )
        .collect();
    for term in candidates {
        if which(&term).is_some() {
            return format!("{term} -e tokengauge-tui");
        }
    }
    String::new()
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

// theme() is called for side-effect of installing the global theme; the
// import is also used by future styling tweaks.
#[allow(dead_code)]
fn _touch_theme() {
    let _ = theme();
}
