//! Turning numbers into the strings a user reads, and the small time helpers
//! everything else needs.
//!
//! Pure leaves: no I/O, no config, no provider knowledge. Three copies of
//! `now_ms` and four of "the first of this month" had accumulated across the
//! crate before this file existed, and two duration formatters had grown the
//! same arithmetic side by side.

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};

/// Milliseconds since the epoch, signed.
///
/// Signed because every consumer subtracts two of these and cares about the
/// sign; the unsigned copy this replaces was cast at each of its call sites.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// The first of `day`'s month.
///
/// `with_day(1)` cannot fail - every month has one - which is what makes this
/// better than the `format("%Y-%m-01").parse()` round trip it replaces. That
/// version silently fell back to `day` itself on a parse failure, so a bug in
/// it would have read as "the month starts today".
pub fn month_start(day: NaiveDate) -> NaiveDate {
    day.with_day(1).unwrap_or(day)
}

/// A duration as its largest `units` components: `3d 16h 45m` at three,
/// `2h 30m` at two, `45m` at one.
///
/// Leading zeros are skipped, so ninety minutes is `1h 30m` and never
/// `0d 1h 30m`. Zeros *after* the first component are kept, because dropping
/// them turns `2d 0h` into a bare `2d` that reads as less precise than it is.
pub fn format_duration(minutes: i64, units: usize) -> String {
    let minutes = minutes.max(0);
    let parts = [
        (minutes / 1440, 'd'),
        ((minutes / 60) % 24, 'h'),
        (minutes % 60, 'm'),
    ];
    let first = parts
        .iter()
        .position(|(value, _)| *value > 0)
        .unwrap_or(parts.len() - 1);
    parts[first..]
        .iter()
        .take(units.max(1))
        .map(|(value, unit)| format!("{value}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How long ago, for a timestamp that is already RFC3339. `None` when it will
/// not parse, so a caller can fall back to whatever it had.
pub fn format_updated_relative(iso: &str) -> Option<String> {
    let ts = DateTime::parse_from_rfc3339(iso).ok()?;
    let delta = Utc::now().signed_duration_since(ts.with_timezone(&Utc));
    let secs = delta.num_seconds().max(0);
    Some(if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    })
}

/// A timestamp as a local wall-clock time. Falls back to slicing the string
/// when it will not parse, because a provider that sends a shape chrono does
/// not know still sent something more useful than an em dash.
pub fn format_updated(value: Option<String>) -> String {
    let Some(value) = value else {
        return "—".to_string();
    };
    if let Ok(timestamp) = DateTime::parse_from_rfc3339(&value) {
        let local = timestamp.with_timezone(&Local);
        return local.format("%H:%M").to_string();
    }
    if let Some((_, time_part)) = value.split_once('T') {
        let time = time_part.trim_end_matches('Z');
        let short = time.get(0..5).unwrap_or(time);
        return short.to_string();
    }
    value
}

/// Token counts at a glance: `384.0M`, `1.2K`, `999`.
pub fn format_tokens(t: u64) -> String {
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

/// A 1-row sparkline over `values`, scaled to the largest. Empty or all-zero
/// input returns the lowest block repeated rather than nothing, so a chart's
/// width stays honest about how many days it covers.
pub fn sparkline(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let chars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = values.iter().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return chars[0].to_string().repeat(values.len());
    }
    values
        .iter()
        .map(|v| {
            let idx = ((v.max(0.0) / max) * 7.0).round() as usize;
            chars[idx.min(7)]
        })
        .collect()
}

/// Round and clamp a float percentage into the `0..=100` byte range the render
/// layer expects. Mirrors the old `de_opt_percent` serde hook, now called from
/// the native fetchers instead of at deserialize time.
pub(crate) fn pct_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

/// Lowercase, collapse each run of non-alphanumeric characters to a single `-`,
/// and trim leading/trailing `-`. Used for stable extra-window ids.
pub(crate) fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_duration_skips_leading_zeros_but_not_inner_ones() {
        // Three components: what a reset time shows.
        assert_eq!(format_duration(5205, 3), "3d 14h 45m");
        assert_eq!(format_duration(150, 3), "2h 30m");
        assert_eq!(format_duration(44, 3), "44m");
        // Two: what a burn-rate projection shows.
        assert_eq!(format_duration(1500, 2), "1d 1h");
        assert_eq!(format_duration(90, 2), "1h 30m");
        assert_eq!(format_duration(30, 2), "30m");
        // An inner zero stays: "2d" alone reads as less precise than it is.
        assert_eq!(format_duration(2880, 2), "2d 0h");
        assert_eq!(format_duration(0, 3), "0m");
        assert_eq!(format_duration(-5, 2), "0m");
    }

    /// The four copies this replaces round-tripped through a string and fell
    /// back to the day itself when the parse failed - which would have read as
    /// "the month starts today" rather than as the bug it was.
    #[test]
    fn the_month_starts_on_the_first() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 26).expect("valid date");
        assert_eq!(
            month_start(day),
            NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid date")
        );
        let first = NaiveDate::from_ymd_opt(2026, 2, 1).expect("valid date");
        assert_eq!(month_start(first), first);
    }

    #[test]
    fn token_counts_shorten_at_each_thousand() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5K");
        assert_eq!(format_tokens(384_000_000), "384.0M");
        assert_eq!(format_tokens(2_000_000_000), "2.0B");
    }
}
