use ratatui::style::Color;
use tokengauge_core::panel::Tone;
use tokengauge_core::{parse_hex_rgb, theme};

pub fn hex_to_color(hex: &str) -> Color {
    match parse_hex_rgb(hex) {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::White,
    }
}

pub fn dim() -> Color {
    hex_to_color(&theme().dim)
}

pub fn green() -> Color {
    hex_to_color(&theme().green)
}

/// The panel spec hands out semantic tiers; the palette is per-frontend.
pub fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Good => green(),
        Tone::Warn => hex_to_color(&theme().yellow),
        Tone::Critical => hex_to_color(&theme().red),
        Tone::Dim => dim(),
        Tone::Normal => Color::White,
    }
}

pub fn color_for(percent: u8) -> Color {
    hex_to_color(theme().color_for_percent(percent))
}

pub fn provider_icon_color(label: &str) -> (&'static str, Color) {
    let icon = tokengauge_core::provider_icon(label);
    (icon.glyph, hex_to_color(icon.color_hex))
}
