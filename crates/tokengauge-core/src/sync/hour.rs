//! One UTC hour: the unit a bucket is keyed by.
//!
//! Hourly and in UTC so each reader converts into its *own* local calendar. A
//! daily unit would have forced one machine's midnight onto another, and would
//! have destroyed the session window and burn rate that peers still contribute
//! to.

use std::fmt;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const SECONDS_PER_HOUR: i64 = 3600;

/// One UTC hour, as hours since the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hour(i64);

impl Hour {
    pub fn containing(at: DateTime<Utc>) -> Self {
        Self(at.timestamp().div_euclid(SECONDS_PER_HOUR))
    }

    pub fn start(self) -> DateTime<Utc> {
        Utc.timestamp_opt(self.0 * SECONDS_PER_HOUR, 0)
            .single()
            .unwrap_or_default()
    }

    /// The calendar day this hour falls in for a reader at `offset`. Each
    /// device converts with its own, which is what lets one bucket set read
    /// correctly in two timezones.
    pub fn date_at(self, offset: FixedOffset) -> NaiveDate {
        self.start().with_timezone(&offset).date_naive()
    }

    pub fn utc_date(self) -> NaiveDate {
        self.start().date_naive()
    }

    pub fn minus_hours(self, hours: i64) -> Self {
        Self(self.0 - hours)
    }

    pub fn minus_days(self, days: i64) -> Self {
        Self(self.0 - days * 24)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let naive =
            NaiveDateTime::parse_from_str(&format!("{text}:00:00"), "%Y-%m-%dT%H:%M:%S").ok()?;
        Some(Self(
            naive.and_utc().timestamp().div_euclid(SECONDS_PER_HOUR),
        ))
    }
}

impl fmt::Display for Hour {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.start().format("%Y-%m-%dT%H"))
    }
}

impl Serialize for Hour {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Hour {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Hour::parse(&raw).ok_or_else(|| D::Error::custom(format!("not an hour stamp: {raw}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_hour_reads_as_two_calendars() {
        // 02:00 UTC is still the 24th in Montreal and already the 25th in Paris.
        let at = Hour::parse("2026-08-25T02").expect("hour");
        let paris = FixedOffset::east_opt(2 * 3600).expect("offset");
        let montreal = FixedOffset::west_opt(4 * 3600).expect("offset");

        assert_eq!(at.date_at(paris).to_string(), "2026-08-25");
        assert_eq!(at.date_at(montreal).to_string(), "2026-08-24");
        assert_eq!(at.utc_date().to_string(), "2026-08-25");
    }

    #[test]
    fn an_hour_survives_the_form_it_is_written_in() {
        let at = Hour::parse("2026-08-25T14").expect("hour");
        assert_eq!(at.to_string(), "2026-08-25T14");
        assert_eq!(Hour::containing(at.start()), at);
        assert_eq!(at.minus_days(1).to_string(), "2026-08-24T14");
        assert!(Hour::parse("nonsense").is_none());
        assert!(Hour::parse("2026-08-25").is_none());
    }
}
