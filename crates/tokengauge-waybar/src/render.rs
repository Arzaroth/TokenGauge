//! Everything the bar and the tooltip look like.
//!
//! Pure functions over a `ProviderRow` and the panel spec: no I/O, no config
//! writes, no daemon. `render_output` is the single entry point for the
//! daemon's broadcast, the one-shot invocation and the refreshing state alike:
//! when the one-shot path carried its own copy the two drifted, and the click
//! hint it printed disagreed with every other surface.

use serde::Serialize;
use tokengauge_core::{
    PanelRow, ProviderFetchError, ProviderRow, Section, SectionKind, Theme, TokenGaugeConfig, Tone,
    WaybarWindow, format_updated_relative, provider_icon, read_waybar_state, theme,
    waybar_state_path,
};

pub(crate) fn theme_palette() -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    let t: &Theme = theme();
    (
        t.dim.as_str(),
        t.separator.as_str(),
        t.green.as_str(),
        t.yellow.as_str(),
        t.red.as_str(),
        t.neutral.as_str(),
    )
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub(crate) struct WaybarOutput {
    pub(crate) text: String,
    pub(crate) tooltip: String,
    pub(crate) class: String,
}

pub(crate) fn format_bar(label: &str, value: Option<u8>) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    match value {
        Some(percent) => {
            let bar_inner = bar_blocks(percent);
            let color = theme().color_for_percent(percent);
            format!(
                "{icon} {escaped_label} [<span foreground=\"{color}\">{bar_inner}</span>] <span foreground=\"{color}\">{percent}%</span>"
            )
        }
        None => format!(
            "{icon} {escaped_label} [<span foreground=\"{dim}\">─────</span>] <span foreground=\"{dim}\">—</span>"
        ),
    }
}

pub(crate) const MINI_BAR_WIDTH: usize = 5;

pub(crate) fn bar_blocks(percent: u8) -> String {
    let pct = percent.min(100) as usize;
    let filled = (pct * MINI_BAR_WIDTH).div_ceil(100);
    let empty = MINI_BAR_WIDTH.saturating_sub(filled);
    format!("{}{}", "━".repeat(filled), "─".repeat(empty))
}

/// Resolve which provider key the waybar text + tooltip should show.
/// Priority: persisted scroll selection > config primary > first row's
/// provider > first error's provider. Always returns Some unless both
/// rows and errors are empty - so the bar is single-provider by default
/// instead of stacking everything on first boot.
pub(crate) fn resolved_selection_key(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
) -> Option<String> {
    let state = read_waybar_state(&waybar_state_path(&config.cache_file));
    state
        .selected
        .clone()
        .or_else(|| config.waybar.primary.clone())
        .or_else(|| rows.first().map(|r| r.provider.clone()))
        .or_else(|| errors.first().map(|e| e.provider.clone()))
        .map(|s| s.to_lowercase())
}

pub(crate) fn selected_provider_for_tooltip(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
) -> Option<usize> {
    let key = resolved_selection_key(config, rows, &[])?;
    rows.iter().position(|r| r.provider.to_lowercase() == key)
}

pub(crate) fn build_text_for_rows_with_errors(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    config: &TokenGaugeConfig,
) -> String {
    let selected_key = resolved_selection_key(config, rows, errors);

    let used_for = |row: &ProviderRow| match config.waybar.window {
        WaybarWindow::Daily => row.session_used,
        WaybarWindow::Weekly => row.weekly_used,
    };
    let matches_key = |provider: &str| {
        selected_key
            .as_deref()
            .is_none_or(|k| provider.to_lowercase() == k)
    };

    let success_parts = rows
        .iter()
        .filter(|r| matches_key(&r.provider))
        .map(|r| format_bar(&r.provider, used_for(r)));
    let error_parts = errors
        .iter()
        .filter(|e| matches_key(&e.provider))
        .map(|e| format_bar_error(&e.provider));
    let parts: Vec<String> = success_parts.chain(error_parts).collect();

    if !parts.is_empty() {
        // Always one provider in the bar text now that selected_key
        // defaults to the first row / error.
        return parts.into_iter().next().unwrap_or_default();
    }

    // Selected provider exists in neither set; fall back to the first row,
    // or the first error if there are no successes.
    rows.first()
        .map(|r| format_bar(&r.provider, used_for(r)))
        .or_else(|| errors.first().map(|e| format_bar_error(&e.provider)))
        .unwrap_or_default()
}

pub(crate) fn format_bar_error(label: &str) -> String {
    let (_dim, _separator, _green, _yellow, red, _neutral) = theme_palette();
    let icon = icon_markup(label);
    let escaped_label = pango_escape(label);
    format!("{icon} {escaped_label} <span foreground=\"{red}\">⚠</span>")
}

/// The percentage the bar shows for a row, under the configured window. Every
/// frontend resolved this itself; it is exported on the row now so they cannot
/// disagree about which window the headline number came from.
pub(crate) fn window_percent(row: &ProviderRow, window: &WaybarWindow) -> Option<u8> {
    match window {
        WaybarWindow::Daily => row.session_used,
        WaybarWindow::Weekly => row.weekly_used,
    }
}

/// Pick the strongest CSS class tier based on current state.
/// Order of precedence (strongest first): refreshing > error > partial-error >
/// crit (>=80%) > warn (>=50%) > base. `tokengauge-stale` is additive: it is
/// appended whenever any row was served from last-good cache, on top of the
/// tier so usage colouring still shows.
pub(crate) fn compute_class(
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
    window: WaybarWindow,
) -> String {
    let stale_suffix = if rows.iter().any(|r| r.stale) {
        " tokengauge-stale"
    } else {
        ""
    };
    if refreshing {
        return "tokengauge tokengauge-refreshing".to_string();
    }
    if !errors.is_empty() {
        return if rows.is_empty() {
            "tokengauge tokengauge-error".to_string()
        } else {
            format!("tokengauge tokengauge-partial-error{stale_suffix}")
        };
    }
    let max_pct = rows
        .iter()
        .filter_map(|r| window_percent(r, &window))
        .max()
        .unwrap_or(0);
    // The CSS tier names are this frontend's; where the tiers fall is not.
    let tier = match Tone::for_percent(max_pct) {
        Tone::Critical => "tokengauge tokengauge-crit",
        Tone::Warn => "tokengauge tokengauge-warn",
        _ => "tokengauge",
    };
    format!("{tier}{stale_suffix}")
}

pub(crate) fn render_output(
    config: &TokenGaugeConfig,
    rows: &[ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
) -> WaybarOutput {
    if rows.is_empty() && errors.is_empty() && !refreshing {
        return WaybarOutput {
            text: "—".into(),
            tooltip: "<tt>TokenGauge: no providers</tt>".into(),
            class: "tokengauge-empty".into(),
        };
    }
    let yellow = theme().yellow.as_str();
    let text_inner = build_text_for_rows_with_errors(rows, errors, config);
    let text = if refreshing {
        if rows.is_empty() && errors.is_empty() {
            format!("   <span foreground=\"{yellow}\">⟳ Refreshing...</span>")
        } else {
            format!("   <span foreground=\"{yellow}\">⟳</span> {text_inner}")
        }
    } else {
        format!("   {text_inner}")
    };
    let selected = selected_provider_for_tooltip(config, rows);
    let tooltip_rows: Vec<&ProviderRow> = match selected {
        Some(idx) => vec![&rows[idx]],
        None => rows.iter().collect(),
    };
    let tooltip = format_tooltip_with_errors(&tooltip_rows, errors, refreshing, LEFT_CLICK_LABEL);
    let class = compute_class(rows, errors, refreshing, config.waybar.window.clone());
    WaybarOutput {
        text,
        tooltip,
        class,
    }
}

pub(crate) fn pango_escape(s: &str) -> String {
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

pub(crate) fn tooltip_bar(percent: u8) -> String {
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

pub(crate) const NERD_FONT_FACE: &str = "JetBrainsMono Nerd Font";

pub(crate) fn icon_markup(label: &str) -> String {
    let icon = provider_icon(label);
    format!(
        "<span face=\"{NERD_FONT_FACE}\" foreground=\"{}\">{}</span>",
        icon.color_hex, icon.glyph
    )
}

/// The `· ends ~26%` / `· empty in 2h 15m` trailer, coloured by how far the
/// window is off an even burn. Shared so every tooltip gauge - the session and
/// weekly ones and the extra windows - renders its projection identically.
/// Map a core [`Tone`] onto the tooltip palette.
pub(crate) fn tone_color(tone: Tone) -> &'static str {
    let (dim, _separator, green, yellow, red, neutral) = theme_palette();
    match tone {
        Tone::Good => green,
        Tone::Warn => yellow,
        Tone::Critical => red,
        Tone::Dim => dim,
        Tone::Normal => neutral,
    }
}

/// The tinted `· ends ~26%` / `· ↑161% vs prior avg` trailer.
pub(crate) fn format_badge(row: &PanelRow) -> String {
    if row.badge.is_empty() {
        return String::new();
    }
    let color = tone_color(row.badge_tone);
    format!(
        "  <span foreground=\"{color}\">· {}</span>",
        pango_escape(&row.badge)
    )
}

/// A ten-cell bar from a 0.0-1.0 fill, for the sections carrying a fraction
/// rather than a percentage.
pub(crate) fn fraction_bar(fraction: f64) -> String {
    tooltip_bar((fraction.clamp(0.0, 1.0) * 100.0).round() as u8)
}

/// Render one core panel section as tooltip lines: a blank spacer, a dim
/// heading, then one line per row shaped by the section kind.
pub(crate) fn format_panel_section(section: &Section) -> Vec<String> {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    // Each column is padded to the widest entry in its own section, so the
    // token counts line up under each other and so do the amounts.
    let value_width = section
        .rows
        .iter()
        .map(|r| r.value.chars().count())
        .max()
        .unwrap_or(0);
    let suffix_width = section
        .rows
        .iter()
        .map(|r| r.suffix.chars().count())
        .max()
        .unwrap_or(0);

    let lines = section.rows.iter().map(|row| {
        let label = format!("{:<16}", pango_escape(&row.label));
        let value = format!("{:>value_width$}", pango_escape(&row.value));
        match section.kind {
            SectionKind::Meters => {
                let color = tone_color(row.tone);
                let bar = fraction_bar(row.fraction.unwrap_or(0.0));
                let trailing = pango_escape(&row.footnote);
                let badge = format_badge(row);
                format!(
                    "  {label}  [<span foreground=\"{color}\">{bar}</span>]  <span foreground=\"{color}\">{value}</span>   {trailing}{badge}"
                )
            }
            SectionKind::Bars => {
                let bar = fraction_bar(row.fraction.unwrap_or(0.0));
                let (open, close) = if row.emphasized {
                    ("<b>", "</b>")
                } else {
                    ("", "")
                };
                let suffix = if row.suffix.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {:>suffix_width$}", pango_escape(&row.suffix))
                };
                format!(
                    "  {open}{label}{close}  <span foreground=\"{dim}\">[{bar}]</span>  {value}{suffix}"
                )
            }
            SectionKind::Rows => {
                let suffix = if row.suffix.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {}", pango_escape(&row.suffix))
                };
                let badge = format_badge(row);
                format!(
                    "  {label}  <span foreground=\"{dim}\">{value}</span>{badge}<span foreground=\"{dim}\">{suffix}</span>"
                )
            }
        }
    });

    std::iter::once(String::new())
        .chain(std::iter::once(format!(
            "  <span foreground=\"{dim}\">{}</span>",
            pango_escape(section.title)
        )))
        .chain(lines)
        .collect()
}

pub(crate) fn format_credits_line(credits: &str) -> Option<String> {
    if credits == "—" || credits.is_empty() {
        return None;
    }
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    Some(format!(
        "  Credits  <span foreground=\"{dim}\">${}</span>",
        pango_escape(credits)
    ))
}

pub(crate) fn format_header(row: &ProviderRow) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();
    let icon = icon_markup(&row.provider);
    let name = pango_escape(&row.provider);
    let plan = row.plan_label.as_deref().filter(|s| !s.is_empty());
    let badge = match plan {
        Some(p) => format!("  <span foreground=\"{dim}\">·  {}</span>", pango_escape(p)),
        None => String::new(),
    };
    format!("<b>{icon}  {name}</b>{badge}")
}

pub(crate) fn format_provider_card(row: &ProviderRow) -> String {
    let (dim, _separator, _green, _yellow, _red, _neutral) = theme_palette();

    let updated_line = row
        .updated_iso
        .as_deref()
        .and_then(format_updated_relative)
        .map(|rel| {
            format!(
                "  <span foreground=\"{dim}\">Updated {}</span>",
                pango_escape(&rel)
            )
        });

    // The tooltip is waybar's panel, so it draws the same sections in the same
    // order as every other panel. The header, the credits and the input hints
    // below are the only waybar-specific chrome left here.
    let sections: Vec<String> = tokengauge_core::panel_spec(row)
        .iter()
        .flat_map(format_panel_section)
        .collect();

    let lines: Vec<String> = std::iter::once(format_header(row))
        .chain(updated_line)
        .chain(sections)
        .chain(format_credits_line(&row.credits))
        .collect();

    format!("<tt>{}</tt>", lines.join("\n"))
}

pub(crate) fn format_error_card(err: &ProviderFetchError) -> String {
    let (_dim, _separator, _green, _yellow, red, _neutral) = theme_palette();
    let icon = icon_markup(&err.provider);
    let name = pango_escape(&err.provider);
    let msg = pango_escape(&err.message);
    format!("<tt><b>{icon}  {name}</b>  <span foreground=\"{red}\">⚠ {msg}</span></tt>")
}

pub(crate) fn format_tooltip_with_errors(
    rows: &[&ProviderRow],
    errors: &[ProviderFetchError],
    refreshing: bool,
    left_verb: &str,
) -> String {
    let cards: Vec<String> = rows
        .iter()
        .map(|row| format_provider_card(row))
        .chain(errors.iter().map(format_error_card))
        .collect();
    let cards_refs: Vec<&str> = cards.iter().map(String::as_str).collect();
    format_tooltip_from_cards(&cards_refs, refreshing, left_verb)
}

pub(crate) fn format_tooltip_from_cards(
    cards: &[&str],
    refreshing: bool,
    left_verb: &str,
) -> String {
    let (dim, separator, _green, yellow, _red, _neutral) = theme_palette();
    let separator = format!(
        "<tt><span foreground=\"{separator}\">────────────────────────────────────</span></tt>"
    );
    let body = cards.join(&format!("\n{separator}\n"));
    let status_line = if refreshing {
        format!("\n<tt><b><span foreground=\"{yellow}\">⟳ Refreshing...</span></b></tt>")
    } else {
        String::new()
    };
    let left_pair = ("left", left_verb);
    let pairs: [(&str, &str); 5] = [
        left_pair,
        ("middle", "dashboard"),
        ("right", "refresh"),
        ("scroll", "rotate"),
        ("back", "status"),
    ];
    let cell = |k: &str, v: &str| format!("{k:<6} {v:<10}");
    let hint_lines: Vec<String> = pairs
        .chunks(3)
        .map(|chunk| {
            let cells: Vec<String> = chunk.iter().map(|(k, v)| cell(k, v)).collect();
            format!("  {}", cells.join("  ·  "))
        })
        .collect();
    let hint = format!(
        "\n\n<tt><span foreground=\"{dim}\">{}</span></tt>",
        hint_lines.join("\n")
    );
    format!("{body}{status_line}{hint}")
}

/// Short verb shown in the tooltip's left-click hint. Both click actions land
/// on the TUI now that the popover is gone, so there is nothing to branch on -
/// it was a function taking a config it ignored.
pub(crate) const LEFT_CLICK_LABEL: &str = "open TUI";

#[cfg(test)]
pub(crate) fn format_tooltip_cards(rows: &[&ProviderRow], refreshing: bool) -> String {
    format_tooltip_with_errors(rows, &[], refreshing, "open")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn bar_blocks_boundaries() {
        assert_eq!(bar_blocks(0), "─────");
        assert_eq!(bar_blocks(20), "━────");
        assert_eq!(bar_blocks(40), "━━───");
        assert_eq!(bar_blocks(60), "━━━──");
        assert_eq!(bar_blocks(80), "━━━━─");
        assert_eq!(bar_blocks(100), "━━━━━");
    }

    #[test]
    fn bar_blocks_clamps_over_100() {
        assert_eq!(bar_blocks(150), "━━━━━");
    }

    // ------------------------------------------------------------------------
    // format_bar tests
    // ------------------------------------------------------------------------

    #[test]
    fn format_bar_with_value() {
        let result = format_bar("Claude", Some(42));
        assert!(result.contains("Claude"));
        assert!(result.contains("42%"));
        assert!(result.contains("━━━──")); // 42% -> ceil(2.1) = 3 filled
        assert!(result.contains("[<span"));
        assert!(result.contains("</span>]"));
        assert!(result.contains("\u{f0721}"));
        assert!(result.contains("face=\"JetBrainsMono Nerd Font\""));
        assert!(result.contains("foreground=\"#DE7356\""));
        // percent + bar wrapped in status color span (42% -> green)
        assert!(result.contains("foreground=\"#a6e3a1\""));
    }

    #[test]
    fn format_bar_with_high_percent_uses_red() {
        let result = format_bar("Claude", Some(85));
        assert!(result.contains("foreground=\"#f38ba8\""));
    }

    #[test]
    fn format_bar_none() {
        let result = format_bar("Codex", None);
        assert!(result.contains("Codex"));
        assert!(result.contains("─────"));
        assert!(result.contains("—"));
        assert!(result.contains("\u{f0b2b}"));
        assert!(result.contains("foreground=\"#74AA9C\""));
        // dim color for missing data
        assert!(result.contains("foreground=\"#6c7086\""));
    }

    #[test]
    fn format_bar_escapes_label() {
        let result = format_bar("ev<il>", Some(50));
        assert!(result.contains("ev&lt;il&gt;"));
        assert!(!result.contains(" ev<il> "));
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

    // color_hex_for_percent + format_tokens + provider_icon tested in core

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

    pub(crate) fn sample_row(provider: &str) -> ProviderRow {
        ProviderRow {
            provider: provider.to_string(),
            session_used: Some(67),
            session_window_minutes: Some(300),
            session_reset: "in 2h 34m".to_string(),
            session_pace: None,
            weekly_used: Some(19),
            weekly_window_minutes: Some(10080),
            weekly_reset: "in 4d 11h".to_string(),
            weekly_pace: None,
            tertiary_used: None,
            tertiary_reset: "—".to_string(),
            credits: "—".to_string(),
            source: "oauth".to_string(),
            updated: "07:37".to_string(),
            updated_iso: None,
            plan_label: None,
            extra_windows: Vec::new(),
            cost: None,
            stale: false,
        }
    }

    /// The contract five frontends parse, none of which the compiler sees. A
    /// renamed or dropped key reaches them as a blank panel, a missing tab
    /// strip or a settings pane that cannot toggle anything, and no Rust test
    /// would have failed on the way there. Adding a key is fine; taking one
    /// away or renaming it means editing all five and this list.
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
        assert!(card.contains("<span foreground=\"#f9e2af\">67%</span>"));
        assert!(card.contains("<span foreground=\"#a6e3a1\">19%</span>"));
        assert!(card.contains("Resets in 2h 34m"));
        assert!(card.contains("Resets in 4d 11h"));
        assert!(card.contains("LIMITS"));
    }

    #[test]
    fn format_provider_card_missing_session() {
        // A window the provider does not report is dropped rather than drawn as
        // a permanently empty meter - the same rule every other panel follows.
        let mut row = sample_row("Codex");
        row.session_used = None;
        row.session_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("Codex</b>"));
        assert!(!card.contains("Session"));
        assert!(card.contains("━─────────"));
        assert!(card.contains("Resets in 4d 11h"));
    }

    #[test]
    fn format_provider_card_missing_reset_renders_not_started() {
        let mut row = sample_row("Codex");
        row.weekly_reset = "—".to_string();
        let card = format_provider_card(&row);
        assert!(card.contains("not started"));
        assert!(!card.contains("Resets —"));
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
        assert!(card.contains("Resets a &amp; b"));
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
        let rows = [sample_row("Claude"), sample_row("Codex")];
        let refs: Vec<&ProviderRow> = rows.iter().collect();
        let tooltip = format_tooltip_cards(&refs, false);
        assert!(tooltip.contains("</tt>\n<tt>"));
        assert!(tooltip.contains("────────────────────────────────────"));
    }

    #[test]
    fn format_tooltip_cards_single_card_no_separator() {
        let row = sample_row("Claude");
        let tooltip = format_tooltip_cards(&[&row], false);
        assert!(!tooltip.contains("────────────────────────────────────"));
    }

    #[test]
    fn format_tooltip_cards_refreshing_shows_indicator() {
        let row = sample_row("Claude");
        let tooltip = format_tooltip_cards(&[&row], true);
        assert!(tooltip.contains("Refreshing"));
        assert!(tooltip.contains("⟳"));
    }

    // ------------------------------------------------------------------------
    // Socket protocol tests
    //
    // Each test binds its own UnixListener at a unique path under /tmp,
    // spawns a one-shot server that drives `handle_client`, and exchanges
    // one command/reply over a connected stream. Configs disable providers
    // and ccusage so the Refresh path doesn't shell out to external bins.
    // ------------------------------------------------------------------------
}
