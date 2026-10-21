//! Usage "pace": are you burning a window's quota faster or slower than a
//! steady even-consumption rate would allow before it resets?
//!
//! Computed from a single snapshot - used %, window duration, reset time, now -
//! so it needs no historical series. Ported from CodexBar's `UsagePace`.

use chrono::{DateTime, Utc};
use serde::Serialize;

/// Hide pace until this fraction of the window has elapsed - early-window
/// numbers are noise.
const MIN_ELAPSED_FRACTION: f64 = 0.03;

/// See [`UsagePace::early`].
const EARLY_FRACTION: f64 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaceStage {
    OnTrack,
    SlightlyAhead,
    Ahead,
    FarAhead,
    SlightlyBehind,
    Behind,
    FarBehind,
}

impl PaceStage {
    /// Burning faster than the even rate (will run out early).
    pub fn is_ahead(self) -> bool {
        matches!(self, Self::SlightlyAhead | Self::Ahead | Self::FarAhead)
    }

    /// Burning slower than the even rate (headroom to spare).
    pub fn is_behind(self) -> bool {
        matches!(self, Self::SlightlyBehind | Self::Behind | Self::FarBehind)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct UsagePace {
    pub stage: PaceStage,
    /// actual% - expected%. Positive = ahead/deficit, negative = behind/reserve.
    pub delta_percent: f64,
    /// Where the window lands at reset if the current rate holds.
    pub projected_percent: f64,
    /// Seconds until the window is projected to hit 100%, if that happens before
    /// the reset.
    pub eta_seconds: Option<i64>,
    /// True when the current burn rate lasts to the reset without running out.
    pub will_last_to_reset: bool,
    /// The opening stretch of a window with most of it still unspent, where a
    /// burst of work projects to an early run-out it is far from.
    #[serde(skip)]
    pub early: bool,
}

fn stage_for_delta(delta: f64) -> PaceStage {
    let abs = delta.abs();
    if abs <= 2.0 {
        PaceStage::OnTrack
    } else if abs <= 6.0 {
        if delta >= 0.0 {
            PaceStage::SlightlyAhead
        } else {
            PaceStage::SlightlyBehind
        }
    } else if abs <= 12.0 {
        if delta >= 0.0 {
            PaceStage::Ahead
        } else {
            PaceStage::Behind
        }
    } else if delta >= 0.0 {
        PaceStage::FarAhead
    } else {
        PaceStage::FarBehind
    }
}

impl UsagePace {
    /// Pace for one usage window, or None when it can't be computed: no reset
    /// time, no window duration, past reset, before the window began, or less
    /// than 3% of the window elapsed.
    pub fn for_window(
        used_percent: u8,
        window_minutes: Option<u32>,
        resets_at: Option<&str>,
        now: DateTime<Utc>,
    ) -> Option<Self> {
        let minutes = window_minutes.filter(|m| *m > 0)?;
        let reset = DateTime::parse_from_rfc3339(resets_at?)
            .ok()?
            .with_timezone(&Utc);

        let duration_secs = f64::from(minutes) * 60.0;
        let time_until_reset = (reset - now).num_seconds() as f64;
        if time_until_reset <= 0.0 || time_until_reset > duration_secs {
            return None;
        }

        let elapsed = (duration_secs - time_until_reset).clamp(0.0, duration_secs);
        if elapsed < duration_secs * MIN_ELAPSED_FRACTION {
            return None;
        }

        let expected = (elapsed / duration_secs * 100.0).clamp(0.0, 100.0);
        let actual = f64::from(used_percent).clamp(0.0, 100.0);
        let delta = actual - expected;

        let (eta_seconds, will_last_to_reset) = if actual > 0.0 {
            let rate = actual / elapsed; // percent per second
            let remaining = (100.0 - actual).max(0.0);
            let candidate = remaining / rate;
            if candidate >= time_until_reset {
                (None, true)
            } else {
                (Some(candidate.round() as i64), false)
            }
        } else {
            // No usage yet: it lasts to reset by definition.
            (None, true)
        };

        Some(Self {
            stage: stage_for_delta(delta),
            delta_percent: delta,
            projected_percent: (actual / elapsed * duration_secs).clamp(0.0, 100.0),
            eta_seconds,
            will_last_to_reset,
            early: elapsed < duration_secs * EARLY_FRACTION && actual <= 50.0,
        })
    }

    /// Compact projection badge for the bar/menu: where the window lands at
    /// reset (`ends ~16%`), or when it runs out first (`empty in 2h 15m`).
    pub fn badge(&self) -> String {
        match self.eta_seconds {
            Some(seconds) => format!("empty in {}", format_eta(seconds)),
            None => format!("ends ~{}%", self.projected_percent.round() as i64),
        }
    }
}

/// A projection is a guess; two components is as much precision as it earns.
/// The reset time beside it shows three, because that one is the provider's
/// own number.
fn format_eta(seconds: i64) -> String {
    crate::fmt::format_duration(seconds.max(0) / 60, 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn at(now: DateTime<Utc>, mins_until_reset: i64) -> String {
        (now + Duration::minutes(mins_until_reset)).to_rfc3339()
    }

    /// ai-usagebar #390: a burst in the first stretch of a week projects to
    /// a run-out that is days away, with most of the week unspent.
    #[test]
    fn the_opening_stretch_with_most_left_is_early() {
        let now = Utc::now();
        let week = 10080;
        // 10% into the week, 30% spent: far ahead, but early.
        let early = UsagePace::for_window(30, Some(week), Some(&at(now, 9072)), now).unwrap();
        assert!(early.stage.is_ahead());
        assert!(early.early);
        assert_eq!(crate::Tone::for_pace(&early), crate::Tone::Warn);
        // The same burst past half the allowance is not early any more.
        let spent = UsagePace::for_window(60, Some(week), Some(&at(now, 9072)), now).unwrap();
        assert!(!spent.early);
        assert_eq!(crate::Tone::for_pace(&spent), crate::Tone::Critical);
        // Nor is it a fifth of the way in.
        let later = UsagePace::for_window(40, Some(week), Some(&at(now, 8064)), now).unwrap();
        assert!(!later.early);
        assert_eq!(crate::Tone::for_pace(&later), crate::Tone::Critical);
    }

    #[test]
    fn ahead_of_pace_flags_deficit() {
        let now = Utc::now();
        // 5h window, half elapsed (expected ~50%), but 80% used -> far ahead.
        let pace = UsagePace::for_window(80, Some(300), Some(&at(now, 150)), now).unwrap();
        assert!(pace.stage.is_ahead());
        assert!(pace.delta_percent > 12.0);
        // Burning at 80%/150min -> runs out before the 150 min remaining.
        assert!(!pace.will_last_to_reset);
        assert!(pace.eta_seconds.is_some());
        assert!(pace.badge().starts_with("empty in "));
    }

    #[test]
    fn behind_pace_flags_reserve_and_lasts() {
        let now = Utc::now();
        // Half elapsed, only 10% used -> far behind, lasts to reset.
        let pace = UsagePace::for_window(10, Some(300), Some(&at(now, 150)), now).unwrap();
        assert!(pace.stage.is_behind());
        assert!(pace.will_last_to_reset);
        // 10% at half the window -> lands near 20% at reset.
        assert_eq!(pace.projected_percent.round() as i64, 20);
        assert_eq!(pace.badge(), "ends ~20%");
    }

    #[test]
    fn none_before_3pct_elapsed() {
        let now = Utc::now();
        // 5h window, only 1 min elapsed (< 3%).
        assert!(UsagePace::for_window(1, Some(300), Some(&at(now, 299)), now).is_none());
    }

    #[test]
    fn none_without_window_or_reset() {
        let now = Utc::now();
        assert!(UsagePace::for_window(50, None, Some(&at(now, 100)), now).is_none());
        assert!(UsagePace::for_window(50, Some(300), None, now).is_none());
    }

    #[test]
    fn none_when_past_reset() {
        let now = Utc::now();
        assert!(UsagePace::for_window(50, Some(300), Some(&at(now, -5)), now).is_none());
    }
}
