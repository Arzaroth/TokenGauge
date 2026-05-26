use ratatui::style::Color;
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

pub fn color_for(percent: u8) -> Color {
    hex_to_color(theme().color_for_percent(percent))
}

pub fn provider_icon_color(label: &str) -> (&'static str, Color) {
    let icon = tokengauge_core::provider_icon(label);
    (icon.glyph, hex_to_color(icon.color_hex))
}
