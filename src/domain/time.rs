//! Timestamp helpers. The clock is read in one place so tests can inject time.

use chrono::{DateTime, SecondsFormat, Utc};

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

pub fn now_iso() -> String {
    to_iso(now())
}

pub fn to_iso(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn parse_iso(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|value| value.with_timezone(&Utc))
        .or_else(|_| {
            // SQLite may hand back a naive ISO timestamp for rows written without a zone.
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|value| value.and_utc())
        })
        .ok()
}

/// Hours between `published` and `now`, clamped at zero. Returns `None` when publication time is
/// unknown: a missing timestamp must never be turned into an invented age.
pub fn hours_since(published: Option<&str>, now: DateTime<Utc>) -> Option<f64> {
    let published = parse_iso(published?)?;
    let hours = (now - published).num_seconds() as f64 / 3600.0;
    Some(hours.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_publication_time_stays_unknown() {
        assert_eq!(hours_since(None, now()), None);
        assert_eq!(hours_since(Some("not a date"), now()), None);
    }

    #[test]
    fn future_timestamps_are_clamped() {
        let reference = parse_iso("2024-05-01T10:00:00Z").expect("date");
        let hours = hours_since(Some("2024-05-02T10:00:00Z"), reference).expect("hours");
        assert_eq!(hours, 0.0);
    }

    #[test]
    fn iso_round_trip() {
        let value = now();
        let iso = to_iso(value);
        let parsed = parse_iso(&iso).expect("parse");
        assert_eq!(value.timestamp(), parsed.timestamp());
    }
}
