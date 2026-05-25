use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use tokengauge_core::{
    CostInfo, ExtraWindowRow, FetchResult, ProviderPayload, ProviderRow, TokenGaugeConfig,
    WaybarState, WaybarWindow, ensure_cache_dir, fetch_all_providers, format_updated_relative,
    load_config, payload_to_rows_with_costs, read_cache_full, read_waybar_state, waybar_state_path,
    write_cache_full, write_default_config, write_waybar_state,
};

#[derive(Parser, Debug)]
#[command(version, about = "Waybar module for TokenGauge")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
    /// Rotate the provider shown in the waybar text and exit (no JSON output).
    #[arg(long, value_enum)]
    rotate: Option<RotateDir>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum RotateDir {
    Next,
    Prev,
}

#[derive(Debug, Serialize)]
struct WaybarOutput {
    text: String,
    tooltip: String,
    class: String,
}

fn format_bar(label: &str, value: Option<u8>) -> String {
    let icon = icon_markup(label);
    let (bars, percent) = match value {
        Some(percent) => (bar_blocks(percent), format!("{percent}%")),
        None => ("—".to_string(), "—".to_string()),
    };
    format!("{icon} {label} {bars} {percent}")
}

fn bar_blocks(percent: u8) -> String {
    match percent.min(100) {
        0..=20 => "▁".to_string(),
        21..=40 => "▁▂".to_string(),
        41..=60 => "▁▂▃".to_string(),
        61..=80 => "▁▂▃▅".to_string(),
        _ => "▁▂▃▅▇".to_string(),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(tokengauge_core::default_config_path);
    if !config_path.exists() {
        write_default_config(&config_path)?;
    }

    let config = load_config(Some(config_path))?;
    ensure_cache_dir(&config.cache_file)?;

    if let Some(dir) = args.rotate {
        handle_rotate(&config, dir)?;
        return Ok(());
    }

    let (payloads, costs) = match maybe_refresh(&config) {
        Ok(pair) => pair,
        Err(error) => {
            let output = WaybarOutput {
                text: "⟂".into(),
                tooltip: format!("<tt>TokenGauge: {}</tt>", pango_escape(&error.to_string())),
                class: "tokengauge-error".into(),
            };
            println!("{}", serde_json::to_string(&output)?);
            return Ok(());
        }
    };

    let rows = payload_to_rows_with_costs(payloads, &costs);
    if rows.is_empty() {
        let output = WaybarOutput {
            text: "—".into(),
            tooltip: "<tt>TokenGauge: no providers</tt>".into(),
            class: "tokengauge-empty".into(),
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let state = read_waybar_state(&waybar_state_path(&config.cache_file));
    let selected_key = state
        .selected
        .as_deref()
        .or(config.waybar.primary.as_deref());
    let visible_rows: Vec<ProviderRow> = match selected_key {
        Some(key) => {
            let lower = key.to_lowercase();
            let matched: Vec<ProviderRow> = rows
                .iter()
                .filter(|r| r.provider.to_lowercase() == lower)
                .cloned()
                .collect();
            if matched.is_empty() {
                rows.clone()
            } else {
                matched
            }
        }
        None => rows.clone(),
    };

    let text = visible_rows
        .iter()
        .map(|row| {
            let used = match config.waybar.window {
                WaybarWindow::Daily => row.session_used,
                WaybarWindow::Weekly => row.weekly_used,
            };
            format_bar(&row.provider, used)
        })
        .collect::<Vec<_>>()
        .join("   ");
    let text = format!("   {text}");

    let tooltip = format_tooltip_cards(&visible_rows);

    let output = WaybarOutput {
        text,
        tooltip,
        class: "tokengauge".into(),
    };

    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

const SCROLL_THROTTLE_MS: i64 = 250;

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn handle_rotate(config: &TokenGaugeConfig, dir: RotateDir) -> Result<()> {
    let cached = match read_cache_full(&config.cache_file) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let rows = payload_to_rows_with_costs(cached.payloads().to_vec(), &cached.costs());
    if rows.is_empty() {
        return Ok(());
    }

    let state_path = waybar_state_path(&config.cache_file);
    let state = read_waybar_state(&state_path);

    let now = now_ms();
    if now - state.last_rotated_ms < SCROLL_THROTTLE_MS {
        return Ok(());
    }

    let current_key = state
        .selected
        .clone()
        .or_else(|| config.waybar.primary.clone());
    let current_idx = current_key
        .as_deref()
        .and_then(|key| {
            let lower = key.to_lowercase();
            rows.iter()
                .position(|r| r.provider.to_lowercase() == lower)
        })
        .unwrap_or(0);

    let len = rows.len();
    let next_idx = match dir {
        RotateDir::Next => (current_idx + 1) % len,
        RotateDir::Prev => (current_idx + len - 1) % len,
    };
    let new_state = WaybarState {
        selected: Some(rows[next_idx].provider.to_lowercase()),
        last_rotated_ms: now,
    };
    write_waybar_state(&state_path, &new_state)?;
    Ok(())
}

fn maybe_refresh(
    config: &TokenGaugeConfig,
) -> Result<(Vec<ProviderPayload>, HashMap<String, CostInfo>)> {
    let now = SystemTime::now();
    let stale = match std::fs::metadata(&config.cache_file) {
        Ok(metadata) => metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age >= Duration::from_secs(config.refresh_secs))
            .unwrap_or(true),
        Err(_) => true,
    };

    if stale {
        let FetchResult {
            payloads,
            errors,
            costs,
        } = fetch_all_providers(config);
        write_cache_full(&config.cache_file, &payloads, &errors, &costs)?;
        Ok((payloads, costs))
    } else {
        let cached = read_cache_full(&config.cache_file)?;
        let costs = cached.costs();
        Ok((cached.payloads().to_vec(), costs))
    }
}

fn pango_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

fn tooltip_bar(percent: u8) -> String {
    let filled = (percent.min(100) / 10) as usize;
    let mut bar = String::with_capacity(30);
    for _ in 0..filled {
        bar.push('━');
    }
    for _ in filled..10 {
        bar.push('─');
    }
    bar
}

fn color_for(percent: u8) -> &'static str {
    match percent {
        0..=49 => "#a6e3a1",
        50..=79 => "#f9e2af",
        _ => "#f38ba8",
    }
}

const DIM_COLOR: &str = "#6c7086";
const SEPARATOR_COLOR: &str = "#45475a";

const NERD_FONT_FACE: &str = "JetBrainsMono Nerd Font";

fn provider_icon(label: &str) -> (&'static str, &'static str) {
    match label.to_lowercase().as_str() {
        "claude" => ("\u{f0721}", "#DE7356"),
        "codex" => ("\u{f0b2b}", "#74AA9C"),
        "copilot" => ("\u{f4b8}", "#8b5cf6"),
        "z.ai" | "zai" => ("Z", "#126EF4"),
        "kimi" | "kimi k2" => ("\u{f06a9}", "#cdd6f4"),
        "minimax" => ("\u{f06a9}", "#cdd6f4"),
        _ => ("\u{f06a9}", "#cdd6f4"),
    }
}

fn icon_markup(label: &str) -> String {
    let (glyph, color) = provider_icon(label);
    format!(
        "<span face=\"{NERD_FONT_FACE}\" foreground=\"{color}\">{glyph}</span>"
    )
}

fn format_provider_line(label: &str, used: Option<u8>, reset: &str) -> String {
    match used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = color_for(pct);
            let pct_cell = format!("{pct:>3}%");
            let reset_part = if reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {}", pango_escape(reset))
            };
            format!(
                "  {label:<8}  [<span foreground=\"{color}\">{bar}</span>]  <span foreground=\"{color}\">{pct_cell}</span>   {reset_part}"
            )
        }
        None => {
            format!(
                "  {label:<8}  [<span foreground=\"{DIM_COLOR}\">──────────</span>]          no data"
            )
        }
    }
}

fn format_credits_line(credits: &str) -> Option<String> {
    if credits == "—" || credits.is_empty() {
        return None;
    }
    Some(format!(
        "  Credits  <span foreground=\"{DIM_COLOR}\">${}</span>",
        pango_escape(credits)
    ))
}

fn format_tokens(t: u64) -> String {
    if t >= 1_000_000_000 {
        format!("{:.1}B", t as f64 / 1e9)
    } else if t >= 1_000_000 {
        format!("{:.1}M", t as f64 / 1e6)
    } else if t >= 1_000 {
        format!("{:.1}K", t as f64 / 1e3)
    } else {
        format!("{t}")
    }
}

fn format_extra_window(extra: &ExtraWindowRow) -> String {
    let title = pango_escape(&extra.title);
    let title_padded = format!("{title:<14}");
    match extra.used {
        Some(pct) => {
            let bar = tooltip_bar(pct);
            let color = color_for(pct);
            let pct_cell = format!("{pct:>3}%");
            let reset_part = if extra.reset == "—" {
                "not started".to_string()
            } else {
                format!("resets {}", pango_escape(&extra.reset))
            };
            format!(
                "  {title_padded}  [<span foreground=\"{color}\">{bar}</span>]  <span foreground=\"{color}\">{pct_cell}</span>   {reset_part}"
            )
        }
        None => format!(
            "  {title_padded}  <span foreground=\"{DIM_COLOR}\">[──────────]</span>          no data"
        ),
    }
}

fn format_cost_lines(cost: &CostInfo) -> Vec<String> {
    let today_usd = format!("${:.2}", cost.today_usd);
    let monthly_usd = format!("${:.2}", cost.monthly_usd);
    let today_tokens = format_tokens(cost.today_tokens);
    let monthly_tokens = format_tokens(cost.monthly_tokens);
    vec![
        format!(
            "  Today     <span foreground=\"{DIM_COLOR}\">{today_usd}  ·  {today_tokens} tokens</span>"
        ),
        format!(
            "  Month     <span foreground=\"{DIM_COLOR}\">{monthly_usd}  ·  {monthly_tokens} tokens</span>"
        ),
    ]
}

fn format_header(row: &ProviderRow) -> String {
    let icon = icon_markup(&row.provider);
    let name = pango_escape(&row.provider);
    let plan = row.plan_label.as_deref().filter(|s| !s.is_empty());
    let badge = match plan {
        Some(p) => format!(
            "  <span foreground=\"{DIM_COLOR}\">·  {}</span>",
            pango_escape(p)
        ),
        None => String::new(),
    };
    format!("<b>{icon}  {name}</b>{badge}")
}

fn format_provider_card(row: &ProviderRow) -> String {
    let mut lines = vec![format_header(row)];

    if let Some(iso) = row.updated_iso.as_deref()
        && let Some(rel) = format_updated_relative(iso)
    {
        lines.push(format!(
            "  <span foreground=\"{DIM_COLOR}\">Updated {}</span>",
            pango_escape(&rel)
        ));
    }

    lines.push(format_provider_line(
        "Session",
        row.session_used,
        &row.session_reset,
    ));
    lines.push(format_provider_line(
        "Weekly",
        row.weekly_used,
        &row.weekly_reset,
    ));
    if row.tertiary_used.is_some() || row.tertiary_reset != "—" {
        lines.push(format_provider_line(
            "Tertiary",
            row.tertiary_used,
            &row.tertiary_reset,
        ));
    }

    if !row.extra_windows.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "  <span foreground=\"{DIM_COLOR}\">Extra usage</span>"
        ));
        for extra in &row.extra_windows {
            lines.push(format_extra_window(extra));
        }
    }

    if let Some(cost) = &row.cost {
        lines.push(String::new());
        lines.push(format!("  <span foreground=\"{DIM_COLOR}\">Cost</span>"));
        lines.extend(format_cost_lines(cost));
    }

    if let Some(credits) = format_credits_line(&row.credits) {
        lines.push(credits);
    }

    format!("<tt>{}</tt>", lines.join("\n"))
}

fn format_tooltip_cards(rows: &[ProviderRow]) -> String {
    let cards: Vec<String> = rows.iter().map(format_provider_card).collect();
    let separator = format!(
        "<tt><span foreground=\"{SEPARATOR_COLOR}\">────────────────────────────────────</span></tt>"
    );
    cards.join(&format!("\n{separator}\n"))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // bar_blocks tests
    // ------------------------------------------------------------------------

    #[test]
    fn bar_blocks_boundaries() {
        // 0-20%
        assert_eq!(bar_blocks(0), "▁");
        assert_eq!(bar_blocks(20), "▁");

        // 21-40%
        assert_eq!(bar_blocks(21), "▁▂");
        assert_eq!(bar_blocks(40), "▁▂");

        // 41-60%
        assert_eq!(bar_blocks(41), "▁▂▃");
        assert_eq!(bar_blocks(60), "▁▂▃");

        // 61-80%
        assert_eq!(bar_blocks(61), "▁▂▃▅");
        assert_eq!(bar_blocks(80), "▁▂▃▅");

        // 81-100%
        assert_eq!(bar_blocks(81), "▁▂▃▅▇");
        assert_eq!(bar_blocks(100), "▁▂▃▅▇");
    }

    #[test]
    fn bar_blocks_clamps_over_100() {
        assert_eq!(bar_blocks(150), "▁▂▃▅▇");
    }

    // ------------------------------------------------------------------------
    // format_bar tests
    // ------------------------------------------------------------------------

    #[test]
    fn format_bar_with_value() {
        let result = format_bar("Claude", Some(42));
        assert!(result.contains("Claude"));
        assert!(result.contains("42%"));
        assert!(result.contains("▁▂▃")); // 41-60% range
        assert!(result.contains("\u{f0721}"));
        assert!(result.contains("face=\"JetBrainsMono Nerd Font\""));
        assert!(result.contains("foreground=\"#DE7356\""));
    }

    #[test]
    fn format_bar_none() {
        let result = format_bar("Codex", None);
        assert!(result.contains("Codex — —"));
        assert!(result.contains("\u{f0b2b}"));
        assert!(result.contains("foreground=\"#74AA9C\""));
    }

    // ------------------------------------------------------------------------
    // tooltip_bar tests
    // ------------------------------------------------------------------------

    #[test]
    fn tooltip_bar_lengths() {
        assert_eq!(tooltip_bar(0).chars().count(), 10);
        assert_eq!(tooltip_bar(100).chars().count(), 10);
        assert_eq!(tooltip_bar(67).chars().count(), 10);
        assert_eq!(tooltip_bar(0), "──────────");
        assert_eq!(tooltip_bar(100), "━━━━━━━━━━");
        assert_eq!(tooltip_bar(67), "━━━━━━────");
    }

    #[test]
    fn tooltip_bar_clamps_over_100() {
        assert_eq!(tooltip_bar(200).chars().count(), 10);
        assert_eq!(tooltip_bar(200), "━━━━━━━━━━");
    }

    // ------------------------------------------------------------------------
    // color_for tests
    // ------------------------------------------------------------------------

    #[test]
    fn color_for_thresholds() {
        assert_eq!(color_for(0), "#a6e3a1");
        assert_eq!(color_for(49), "#a6e3a1");
        assert_eq!(color_for(50), "#f9e2af");
        assert_eq!(color_for(79), "#f9e2af");
        assert_eq!(color_for(80), "#f38ba8");
        assert_eq!(color_for(100), "#f38ba8");
    }

    // ------------------------------------------------------------------------
    // pango_escape tests
    // ------------------------------------------------------------------------

    #[test]
    fn pango_escape_specials() {
        assert_eq!(pango_escape("a & b"), "a &amp; b");
        assert_eq!(pango_escape("<tag>"), "&lt;tag&gt;");
        assert_eq!(pango_escape("\"quote\""), "&quot;quote&quot;");
        assert_eq!(pango_escape("it's"), "it&apos;s");
        assert_eq!(pango_escape("plain text 123"), "plain text 123");
    }

    // ------------------------------------------------------------------------
    // format_provider_card tests
    // ------------------------------------------------------------------------

    fn sample_row(provider: &str) -> ProviderRow {
        ProviderRow {
            provider: provider.to_string(),
            session_used: Some(67),
            session_window_minutes: Some(300),
            session_reset: "in 2h 34m".to_string(),
            weekly_used: Some(19),
            weekly_window_minutes: Some(10080),
            weekly_reset: "in 4d 11h".to_string(),
            tertiary_used: None,
            tertiary_reset: "—".to_string(),
            credits: "—".to_string(),
            source: "oauth".to_string(),
            updated: "07:37".to_string(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: None,
        }
    }

    #[test]
    fn format_provider_card_full_data() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(card.starts_with("<tt><b>"));
        assert!(card.contains("Claude</b>"));
        assert!(card.ends_with("</tt>"));
        assert!(card.contains("Session"));
        assert!(card.contains("Weekly"));
        assert!(card.contains("━━━━━━────"));
        assert!(card.contains("━─────────"));
        assert!(card.contains("<span foreground=\"#f9e2af\"> 67%</span>"));
        assert!(card.contains("<span foreground=\"#a6e3a1\"> 19%</span>"));
        assert!(card.contains("resets in 2h 34m"));
        assert!(card.contains("resets in 4d 11h"));
    }

    #[test]
    fn format_provider_card_missing_session() {
        let mut row = sample_row("Codex");
        row.session_used = None;
        row.session_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("Codex</b>"));
        assert!(card.contains("──────────"));
        assert!(card.contains("no data"));
        assert!(card.contains("━─────────"));
        assert!(card.contains("resets in 4d 11h"));
    }

    #[test]
    fn format_provider_card_missing_reset_renders_not_started() {
        let mut row = sample_row("Codex");
        row.weekly_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("not started"));
        assert!(!card.contains("resets —"));
    }

    #[test]
    fn format_provider_card_escapes_provider_name() {
        let row = sample_row("ev<il>");
        let card = format_provider_card(&row);
        assert!(card.contains("ev&lt;il&gt;</b>"));
        assert!(!card.contains("ev<il></b>"));
    }

    #[test]
    fn format_provider_card_escapes_reset_string() {
        let mut row = sample_row("Claude");
        row.session_reset = "a & b".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("resets a &amp; b"));
    }

    #[test]
    fn format_provider_card_includes_icon() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(card.contains("\u{f0721}"));
        assert!(card.contains("face=\"JetBrainsMono Nerd Font\""));
        assert!(card.contains("foreground=\"#DE7356\""));
        let codex_card = format_provider_card(&sample_row("Codex"));
        assert!(codex_card.contains("\u{f0b2b}"));
        let mut other = sample_row("Mystery");
        other.provider = "Mystery".to_string();
        let card = format_provider_card(&other);
        assert!(card.contains("\u{f06a9}"));
    }

    #[test]
    fn format_provider_card_omits_credits_when_dash() {
        let card = format_provider_card(&sample_row("Claude"));
        assert!(!card.contains("Credits"));
    }

    #[test]
    fn format_provider_card_includes_credits_when_present() {
        let mut row = sample_row("Kimi");
        row.credits = "42.57".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("Credits"));
        assert!(card.contains("$42.57"));
    }

    #[test]
    fn format_tooltip_cards_joins_with_separator() {
        let rows = vec![sample_row("Claude"), sample_row("Codex")];
        let tooltip = format_tooltip_cards(&rows);
        assert!(tooltip.contains("</tt>\n<tt>"));
        assert!(tooltip.contains("────────────────────────────────────"));
    }

    #[test]
    fn format_tooltip_cards_single_card_no_separator() {
        let tooltip = format_tooltip_cards(&[sample_row("Claude")]);
        assert!(!tooltip.contains("────────────────────────────────────"));
    }
}
