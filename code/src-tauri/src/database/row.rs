//! Row-mapping helpers shared by the repositories.
//!
//! These convert SQLite column values into domain types and translate any
//! parse failure into a database error, so repositories stay free of
//! boilerplate and never surface a raw `rusqlite` error.

use chrono::{DateTime, Utc};
use rusqlite::Row;
use std::str::FromStr;

use crate::errors::{AppError, AppResult};
use crate::models::Timestamp;

/// Read a required TEXT column.
pub fn text(row: &Row<'_>, idx: usize) -> AppResult<String> {
    Ok(row.get::<_, String>(idx)?)
}

/// Read an optional TEXT column.
pub fn opt_text(row: &Row<'_>, idx: usize) -> AppResult<Option<String>> {
    Ok(row.get::<_, Option<String>>(idx)?)
}

/// Read a required INTEGER column as `i64`.
pub fn int(row: &Row<'_>, idx: usize) -> AppResult<i64> {
    Ok(row.get::<_, i64>(idx)?)
}

/// Read an optional INTEGER column as `i64`.
pub fn opt_int(row: &Row<'_>, idx: usize) -> AppResult<Option<i64>> {
    Ok(row.get::<_, Option<i64>>(idx)?)
}

/// Read a required INTEGER column as `u32`.
pub fn uint(row: &Row<'_>, idx: usize) -> AppResult<u32> {
    Ok(row.get::<_, i64>(idx)?.max(0) as u32)
}

/// Read a required INTEGER column as `u64`.
pub fn u64_val(row: &Row<'_>, idx: usize) -> AppResult<u64> {
    Ok(row.get::<_, i64>(idx)?.max(0) as u64)
}

/// Read an optional INTEGER column as `u32`.
pub fn opt_uint(row: &Row<'_>, idx: usize) -> AppResult<Option<u32>> {
    Ok(row.get::<_, Option<i64>>(idx)?.map(|v| v.max(0) as u32))
}

/// Read an optional INTEGER column as `u64`.
pub fn opt_u64(row: &Row<'_>, idx: usize) -> AppResult<Option<u64>> {
    Ok(row.get::<_, Option<i64>>(idx)?.map(|v| v.max(0) as u64))
}

/// Read an optional REAL column as `f64`.
pub fn opt_f64(row: &Row<'_>, idx: usize) -> AppResult<Option<f64>> {
    Ok(row.get::<_, Option<f64>>(idx)?)
}

/// Read a required INTEGER column as `bool` (0/1).
pub fn bool_val(row: &Row<'_>, idx: usize) -> AppResult<bool> {
    Ok(row.get::<_, i64>(idx)? != 0)
}

/// Read a TEXT column as a UTC timestamp, saying which field it came from.
///
/// `None` means the stored value could not be read as a time. That is not the
/// same as the column being null — see [`parse_timestamp`] — but it is reported
/// the same way, because a caller that cannot be told the time must not be told
/// a wrong one either.
pub fn timestamp(row: &Row<'_>, idx: usize, field: &str) -> AppResult<Option<Timestamp>> {
    let raw = text(row, idx)?;
    Ok(read_timestamp(&raw, field))
}

/// Read an optional TEXT column as a UTC timestamp, saying which field it came
/// from.
pub fn opt_timestamp(
    row: &Row<'_>,
    idx: usize,
    field: &str,
) -> AppResult<Option<Timestamp>> {
    match opt_text(row, idx)? {
        Some(raw) => Ok(read_timestamp(&raw, field)),
        None => Ok(None),
    }
}

/// Parse a stored timestamp, and record it when it cannot be read.
///
/// The warning names the field and the value and nothing else. A timestamp is
/// not a secret, and the value is what makes the offending row findable; a
/// logger that stayed silent here would leave "this task shows no date" with no
/// way to find out why.
fn read_timestamp(raw: &str, field: &str) -> Option<Timestamp> {
    let parsed = parse_timestamp(raw);
    if parsed.is_none() {
        tracing::warn!(
            target: "database",
            field,
            value = raw,
            "stored timestamp could not be read; the row is kept, with no time known for it"
        );
    }
    parsed
}

/// Parse a stored timestamp into UTC, or `None` when it cannot be read.
///
/// # What it accepts
///
/// * **RFC 3339 with an offset** — `2026-10-14T18:37:44+00:00`,
///   `2026-10-14T18:37:44Z`, and with fractional seconds
///   (`2026-10-14T18:37:44.569689700+00:00`). This is what the application
///   writes, and it is what every existing row therefore holds.
/// * **SQLite's own timestamp format** — `2026-10-14 21:37:34`, and
///   `2026-10-14 21:37:34.123` from `datetime('now', 'subsec')`. This is what
///   `datetime('now')` and `CURRENT_TIMESTAMP` produce, and SQLite documents
///   them as **UTC**, so reading them as UTC is the source's own definition
///   rather than a guess. It is the format a hand-edited database, a migration
///   written in SQL, or another tool is most likely to have left behind.
/// * **Those two combined** — a space separator with an offset, which is
///   unambiguous and costs nothing to accept.
///
/// # What it refuses, and why refusing is the point
///
/// A date on its own (`2026-10-14`) is refused rather than read as midnight: its
/// time of day is not something the source stated. Anything else unusual is
/// refused too. A value it cannot place is reported as *no time known*.
///
/// It never substitutes the current time. A row whose timestamp cannot be read
/// is a row with an unknown timestamp, and every other fact about it is still
/// true — which is exactly why this returns an `Option` instead of choosing
/// something plausible to put in its place.
pub fn parse_timestamp(raw: &str) -> Option<Timestamp> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    // The application's own format, and any offset-bearing near miss of it.
    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(parsed.with_timezone(&Utc));
    }

    // The same instant with a space where RFC 3339 wants a `T`. Only accepted
    // when it still carries an offset, so the instant is never in doubt.
    if let Some((date, rest)) = trimmed.split_once(' ') {
        if rest.contains('+') || rest.contains('-') || rest.ends_with('Z') {
            let normalised = format!("{date}T{rest}");
            if let Ok(parsed) = DateTime::parse_from_rfc3339(&normalised) {
                return Some(parsed.with_timezone(&Utc));
            }
        }
    }

    // SQLite's `datetime('now')`, and its fractional form. Documented as UTC.
    //
    // The *space* separator is what makes this the SQLite format rather than an
    // ISO-8601 time with the timezone left off. `2026-10-14T21:37:34` — the same
    // digits with a `T` — is deliberately not accepted: no producer this
    // application knows names that shape, so its timezone is genuinely unstated,
    // and reading it as UTC would be this code guessing rather than the source
    // saying.
    const SQLITE: &[&str] = &["%Y-%m-%d %H:%M:%S%.f"];
    for format in SQLITE {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, format) {
            return Some(naive.and_utc());
        }
    }

    None
}

/// Serialize a timestamp for storage.
pub fn timestamp_to_sql(ts: Timestamp) -> String {
    ts.to_rfc3339()
}

/// Serialize a timestamp the application is writing about itself.
///
/// A row being written is a row this application just built, and every writer
/// sets these fields as it saves. The fallback covers only the case where one
/// was somehow left unset — the moment of the write is then the honest answer,
/// and it keeps the NOT NULL contract of the columns these go into. It is
/// deliberately *not* used when reading: a stored value nobody can parse is not
/// a reason to claim the row is from now.
pub fn storing_timestamp(ts: Option<Timestamp>) -> String {
    timestamp_to_sql(ts.unwrap_or_else(crate::models::common::now))
}

/// Parse a `string_enum` column value, mapping failures to a database error.
///
/// Distinct from a user-facing validation error: a bad enum in the database
/// means the stored row is corrupt, not that the user typed something wrong.
pub fn parse_enum<T>(raw: &str, column: &str) -> AppResult<T>
where
    T: FromStr<Err = AppError>,
{
    raw.parse::<T>().map_err(|err| {
        AppError::database(format!("corrupt value in column `{column}`"))
            .with_technical_detail(format!("{raw}: {}", err.message()))
    })
}

/// Parse an optional enum column.
pub fn opt_enum<T>(raw: Option<String>, column: &str) -> AppResult<Option<T>>
where
    T: FromStr<Err = AppError>,
{
    match raw {
        Some(value) => Ok(Some(parse_enum(&value, column)?)),
        None => Ok(None),
    }
}

/// Parse a JSON metadata column into free-form metadata.
pub fn metadata(row: &Row<'_>, idx: usize) -> AppResult<crate::models::Metadata> {
    let raw = text(row, idx)?;
    if raw.trim().is_empty() {
        return Ok(crate::models::Metadata::new());
    }
    serde_json::from_str(&raw).map_err(|err| {
        AppError::database("stored metadata is not valid JSON")
            .with_technical_detail(err.to_string())
    })
}

/// Serialize metadata for storage.
pub fn metadata_to_sql(value: &crate::models::Metadata) -> AppResult<String> {
    serde_json::to_string(value).map_err(AppError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The instant every accepted spelling of it must mean.
    fn expected() -> Timestamp {
        "2026-10-14T21:37:34Z".parse().unwrap()
    }

    #[test]
    fn reads_the_format_the_application_writes() {
        // What `timestamp_to_sql` produces, and therefore what every row the
        // application has ever written holds — offsets in both spellings.
        for raw in [
            "2026-10-14T21:37:34+00:00",
            "2026-10-14T21:37:34Z",
            // The same instant, stated elsewhere and converted.
            "2026-10-14T22:37:34+01:00",
            "2026-10-14T14:37:34-07:00",
        ] {
            assert_eq!(parse_timestamp(raw), Some(expected()), "for {raw}");
        }
    }

    #[test]
    fn reads_fractional_seconds_to_the_precision_it_was_given() {
        // `to_rfc3339` on a timestamp with sub-second precision writes this, so
        // it is in the database already.
        assert_eq!(
            parse_timestamp("2026-10-14T21:37:34.569689700+00:00"),
            Some("2026-10-14T21:37:34.569689700Z".parse().unwrap())
        );
        // And it still compares as the instant it names.
        let with_fraction = parse_timestamp("2026-10-14T21:37:34.5Z").unwrap();
        let next_second: Timestamp = "2026-10-14T21:37:35Z".parse().unwrap();
        assert!(with_fraction > expected());
        assert!(with_fraction < next_second);
    }

    #[test]
    fn reads_sqlite_timestamps_as_the_utc_they_are() {
        // `datetime('now')` and `CURRENT_TIMESTAMP` produce exactly this, and
        // SQLite documents both as UTC — so this is the source's own meaning,
        // not a timezone guessed on its behalf.
        assert_eq!(parse_timestamp("2026-10-14 21:37:34"), Some(expected()));
        // `datetime('now', 'subsec')`.
        assert_eq!(
            parse_timestamp("2026-10-14 21:37:34.000"),
            Some(expected())
        );
        // The same text with an offset, which the application's own RFC 3339
        // writer would never produce but a hand-written row might.
        assert_eq!(
            parse_timestamp("2026-10-14 21:37:34+00:00"),
            Some(expected())
        );
    }

    #[test]
    fn refuses_a_value_whose_instant_it_cannot_know() {
        // Every one of these names *something*, and none of them names a moment
        // this application can place on a timeline. Reading any of them as the
        // current time — or as midnight, or as anything else plausible — would
        // put a fiction where the source put a fact.
        for raw in [
            "",
            "   ",
            // A date with no time of day. Which instant that is, is not stated.
            "2026-10-14",
            "14/10/2026 21:37:34",
            "2026-10-14 21:37",
            "yesterday",
            "now",
            "2026-10-14T21:37:34",
            "0000-00-00 00:00:00",
            "not a timestamp at all",
        ] {
            assert_eq!(parse_timestamp(raw), None, "must refuse {raw:?}");
        }
    }

    #[test]
    fn a_refusal_is_never_a_time_of_day() {
        // The specific inversion this guards against: a caller that treats `None`
        // as "use now" turns an unreadable row into the most recent thing that
        // ever happened to it.
        assert_ne!(parse_timestamp("2026-10-14"), Some(crate::models::common::now()));
    }

    #[test]
    fn writes_and_reads_round_trip() {
        let original = "2026-10-14T21:37:34.123456789Z".parse::<Timestamp>().unwrap();
        let stored = timestamp_to_sql(original);
        assert_eq!(parse_timestamp(&stored), Some(original));
    }

    #[test]
    fn a_row_being_written_is_stamped_with_the_moment_it_is_written() {
        // The one place the current time is the honest answer: this application
        // is writing a row about itself right now, and the column is NOT NULL.
        let stamped = storing_timestamp(None);
        let parsed = parse_timestamp(&stamped).expect("a written value must read back");
        assert!((parsed - crate::models::common::now()).num_seconds().abs() < 5);
    }
}
