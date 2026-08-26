//! The colour palette, and the process-global one in force.
//!
//! `Tone` in [`crate::panel`] decides which tier a value falls in; this decides
//! what a tier looks like. Keeping the two apart is what stopped five copies of
//! the 50/80 boundaries from drifting - a frontend maps a tone, and never a
//! number, onto a colour.

use serde::{Deserialize, Serialize};

use crate::Tone;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Preset to start from: "catppuccin" (default), "nord", "gruvbox".
    /// Individual hex fields below override the preset's values.
    pub preset: String,
    pub dim: Option<String>,
    pub separator: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub neutral: Option<String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: "catppuccin".into(),
            dim: None,
            separator: None,
            green: None,
            yellow: None,
            red: None,
            neutral: None,
        }
    }
}

pub const DIM_HEX: &str = "#6c7086";

pub const SEPARATOR_HEX: &str = "#45475a";

pub const GREEN_HEX: &str = "#a6e3a1";

pub const YELLOW_HEX: &str = "#f9e2af";

pub const RED_HEX: &str = "#f38ba8";

pub const NEUTRAL_HEX: &str = "#cdd6f4";

/// Process-global active theme.
/// `install_theme` may be called more than once (e.g. on a daemon SIGHUP
/// reload); each installation `Box::leak`s a fresh `Theme` so existing
/// `&'static Theme` references stay valid. The leaked memory is a few
/// hundred bytes per reload and is never reclaimed; acceptable because
/// reloads are user-initiated and rare.
static ACTIVE_THEME: std::sync::RwLock<Option<&'static Theme>> = std::sync::RwLock::new(None);

pub fn theme() -> &'static Theme {
    if let Some(t) = *ACTIVE_THEME.read().expect("theme lock poisoned") {
        return t;
    }
    let mut w = ACTIVE_THEME.write().expect("theme lock poisoned");
    if let Some(t) = *w {
        return t;
    }
    let default: &'static Theme = Box::leak(Box::new(Theme::catppuccin()));
    *w = Some(default);
    default
}

pub fn install_theme(t: Theme) {
    let leaked: &'static Theme = Box::leak(Box::new(t));
    *ACTIVE_THEME.write().expect("theme lock poisoned") = Some(leaked);
}

/// Resolved color palette used by both waybar tooltip and TUI.
/// Fields are owned `String` so the values can come from a config override.
#[derive(Debug, Clone)]
pub struct Theme {
    pub dim: String,
    pub separator: String,
    pub green: String,
    pub yellow: String,
    pub red: String,
    pub neutral: String,
}

impl Theme {
    pub fn catppuccin() -> Self {
        Self {
            dim: DIM_HEX.into(),
            separator: SEPARATOR_HEX.into(),
            green: GREEN_HEX.into(),
            yellow: YELLOW_HEX.into(),
            red: RED_HEX.into(),
            neutral: NEUTRAL_HEX.into(),
        }
    }

    pub fn nord() -> Self {
        Self {
            dim: "#4c566a".into(),
            separator: "#3b4252".into(),
            green: "#a3be8c".into(),
            yellow: "#ebcb8b".into(),
            red: "#bf616a".into(),
            neutral: "#d8dee9".into(),
        }
    }

    pub fn gruvbox() -> Self {
        Self {
            dim: "#928374".into(),
            separator: "#504945".into(),
            green: "#b8bb26".into(),
            yellow: "#fabd2f".into(),
            red: "#fb4934".into(),
            neutral: "#ebdbb2".into(),
        }
    }

    /// This palette's colour for a semantic tier.
    pub fn color_for_tone(&self, tone: Tone) -> &str {
        match tone {
            Tone::Good => &self.green,
            Tone::Warn => &self.yellow,
            Tone::Critical => &self.red,
            Tone::Dim => &self.dim,
            Tone::Normal => &self.neutral,
        }
    }

    /// The gauge colour for a usage percentage. `Tone::for_percent` owns where
    /// the tiers fall; this owns only what they look like. Four copies of the
    /// 50/80 boundaries had accumulated across the Rust surfaces alone.
    pub fn color_for_percent(&self, percent: u8) -> &str {
        self.color_for_tone(Tone::for_percent(percent))
    }
}

/// Parse `#RRGGBB` into (r, g, b). Returns None on malformed input.
pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gauge_tiers_come_from_one_threshold_table() {
        let t = Theme::catppuccin();
        assert_eq!(t.color_for_percent(0), t.green);
        assert_eq!(t.color_for_percent(49), t.green);
        assert_eq!(t.color_for_percent(50), t.yellow);
        assert_eq!(t.color_for_percent(79), t.yellow);
        assert_eq!(t.color_for_percent(80), t.red);
    }

    #[test]
    fn parse_hex_rgb_works() {
        assert_eq!(parse_hex_rgb("#a6e3a1"), Some((0xa6, 0xe3, 0xa1)));
        assert_eq!(parse_hex_rgb("#DE7356"), Some((0xDE, 0x73, 0x56)));
        assert_eq!(parse_hex_rgb("not-hex"), None);
        assert_eq!(parse_hex_rgb("#abc"), None);
    }
}
