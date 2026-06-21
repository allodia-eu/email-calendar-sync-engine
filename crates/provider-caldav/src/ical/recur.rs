//! Parsing an iCalendar `RRULE` into the structural [`RecurrenceRule`]
//! (RFC 5545 §3.3.10).
//!
//! The rule is stored in full (every `BY*` part), even where the
//! `engine-recurrence` expander does not yet expand it — an unsupported rule is
//! preserved and simply materializes no occurrences (`calendar-semantics.md`), so
//! the parser never drops structure it understood. `FREQ` is required; the rest
//! default. `COUNT` and `UNTIL` are mutually exclusive (RFC 5545); `COUNT` wins if
//! both somehow appear.

use core::num::{NonZeroI32, NonZeroU32};

use engine_core::calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, Weekday};

use super::value::parse_until;
use crate::error::CalDavError;

/// Parses an `RRULE` value into a [`RecurrenceRule`].
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if `FREQ` is missing/unknown or `UNTIL` is
/// malformed.
pub(crate) fn parse_rrule(value: &str) -> Result<RecurrenceRule, CalDavError> {
    let parts = parse_parts(value);
    let frequency = parts
        .iter()
        .find(|(k, _)| k == "FREQ")
        .and_then(|(_, v)| frequency(v))
        .ok_or_else(|| CalDavError::ical(format!("RRULE missing or invalid FREQ: {value:?}")))?;
    let mut rule = RecurrenceRule::new(frequency);
    let get = |key: &str| {
        parts
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    if let Some(interval) = get("INTERVAL")
        .and_then(|v| v.parse::<u32>().ok())
        .and_then(NonZeroU32::new)
    {
        rule.interval = interval;
    }
    // COUNT and UNTIL are mutually exclusive; COUNT wins if both appear. A COUNT
    // present but zero/unparseable is rejected (RFC 5545: COUNT "MUST be a positive
    // integer") rather than silently falling through to UNTIL/unbounded — the
    // caller degrades a rejected rule to no-recurrence (one occurrence).
    if let Some(count) = get("COUNT") {
        let n = count
            .parse::<u32>()
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| {
                CalDavError::ical(format!("RRULE COUNT not a positive integer: {count:?}"))
            })?;
        rule.bound = RecurrenceBound::Count(n);
    } else if let Some(until) = get("UNTIL") {
        rule.bound = RecurrenceBound::Until(parse_until(until)?);
    }
    if let Some(by_day) = get("BYDAY") {
        rule.by_day = by_day.split(',').filter_map(parse_nday).collect();
    }
    rule.by_month_day = int_list(get("BYMONTHDAY"));
    rule.by_month = str_list(get("BYMONTH"));
    rule.by_year_day = int_list(get("BYYEARDAY"));
    rule.by_week_no = int_list(get("BYWEEKNO"));
    rule.by_set_position = int_list(get("BYSETPOS"));
    rule.by_hour = uint_list(get("BYHOUR"));
    rule.by_minute = uint_list(get("BYMINUTE"));
    rule.by_second = uint_list(get("BYSECOND"));
    if let Some(wkst) = get("WKST").and_then(weekday) {
        rule.first_day_of_week = wkst;
    }
    Ok(rule)
}

/// Splits the `;`-separated `KEY=value` parts, uppercasing keys.
fn parse_parts(value: &str) -> Vec<(String, String)> {
    value
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(key, val)| (key.trim().to_ascii_uppercase(), val.trim().to_owned()))
        .collect()
}

/// Maps an iCalendar `FREQ` token to a [`Frequency`].
fn frequency(value: &str) -> Option<Frequency> {
    match value.to_ascii_uppercase().as_str() {
        "SECONDLY" => Some(Frequency::Secondly),
        "MINUTELY" => Some(Frequency::Minutely),
        "HOURLY" => Some(Frequency::Hourly),
        "DAILY" => Some(Frequency::Daily),
        "WEEKLY" => Some(Frequency::Weekly),
        "MONTHLY" => Some(Frequency::Monthly),
        "YEARLY" => Some(Frequency::Yearly),
        _ => None,
    }
}

/// Maps a two-letter weekday token to a [`Weekday`].
fn weekday(value: &str) -> Option<Weekday> {
    match value.trim().to_ascii_uppercase().as_str() {
        "MO" => Some(Weekday::Mo),
        "TU" => Some(Weekday::Tu),
        "WE" => Some(Weekday::We),
        "TH" => Some(Weekday::Th),
        "FR" => Some(Weekday::Fr),
        "SA" => Some(Weekday::Sa),
        "SU" => Some(Weekday::Su),
        _ => None,
    }
}

/// Parses one `BYDAY` token: an optional signed ordinal followed by a weekday
/// (`MO`, `-1SU`, `2TH`).
fn parse_nday(token: &str) -> Option<NDay> {
    let token = token.trim();
    // The weekday is the final two bytes (two ASCII letters); the optional signed
    // ordinal precedes it. Split only at a char boundary — a token ending in a
    // multibyte char is invalid and rejected, never sliced mid-codepoint (which
    // would panic on hostile input like `BYDAY=Ωa`).
    let split = token.len().checked_sub(2)?;
    if !token.is_char_boundary(split) {
        return None;
    }
    let day = weekday(&token[split..])?;
    let ordinal = &token[..split];
    let nth_of_period = if ordinal.is_empty() {
        None
    } else {
        Some(ordinal.parse::<i32>().ok().and_then(NonZeroI32::new)?)
    };
    Some(NDay { day, nth_of_period })
}

/// A comma-separated signed-integer list, dropping unparseable entries.
fn int_list(value: Option<&str>) -> Vec<i32> {
    list(value, |entry| entry.parse().ok())
}

/// A comma-separated unsigned-byte list (`BYHOUR`/`BYMINUTE`/`BYSECOND`).
fn uint_list(value: Option<&str>) -> Vec<u8> {
    list(value, |entry| entry.parse().ok())
}

/// A comma-separated string list (`BYMONTH`), kept verbatim.
fn str_list(value: Option<&str>) -> Vec<String> {
    list(value, |entry| Some(entry.to_owned()))
}

/// Splits `value` on commas, mapping each trimmed entry through `parse`.
fn list<T>(value: Option<&str>, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter_map(|entry| parse(entry.trim()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_seed_weekly_rule() {
        // The recurring-weekly fixture: FREQ=WEEKLY;COUNT=8;BYDAY=MO.
        let rule = parse_rrule("FREQ=WEEKLY;COUNT=8;BYDAY=MO").unwrap();
        assert_eq!(rule.frequency, Frequency::Weekly);
        assert_eq!(rule.interval, NonZeroU32::new(1).unwrap());
        assert_eq!(
            rule.bound,
            RecurrenceBound::Count(NonZeroU32::new(8).unwrap())
        );
        assert_eq!(
            rule.by_day,
            vec![NDay {
                day: Weekday::Mo,
                nth_of_period: None
            }]
        );
    }

    #[test]
    fn parses_nth_weekday_and_bymonth() {
        // A VTIMEZONE-style rule: last Sunday of March.
        let rule = parse_rrule("FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU").unwrap();
        assert_eq!(rule.frequency, Frequency::Yearly);
        assert_eq!(rule.by_month, vec!["3".to_owned()]);
        assert_eq!(
            rule.by_day,
            vec![NDay {
                day: Weekday::Su,
                nth_of_period: Some(NonZeroI32::new(-1).unwrap())
            }]
        );
    }

    #[test]
    fn interval_until_and_negatives() {
        let rule =
            parse_rrule("FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=-1;UNTIL=20261231T000000Z").unwrap();
        assert_eq!(rule.interval, NonZeroU32::new(2).unwrap());
        assert_eq!(rule.by_month_day, vec![-1]);
        assert!(matches!(rule.bound, RecurrenceBound::Until(_)));
    }

    #[test]
    fn wkst_and_staged_parts_are_preserved() {
        let rule = parse_rrule("FREQ=WEEKLY;WKST=SU;BYSETPOS=1,-1;BYHOUR=9").unwrap();
        assert_eq!(rule.first_day_of_week, Weekday::Su);
        assert_eq!(rule.by_set_position, vec![1, -1]);
        assert_eq!(rule.by_hour, vec![9]);
    }

    #[test]
    fn missing_or_unknown_frequency_is_rejected() {
        assert!(parse_rrule("COUNT=3").is_err());
        assert!(parse_rrule("FREQ=FORTNIGHTLY").is_err());
    }

    #[test]
    fn count_zero_is_rejected_not_treated_as_unbounded() {
        // COUNT=0 must NOT silently fall through to an unbounded series (which
        // would expand to the whole horizon); RFC 5545 requires a positive COUNT.
        assert!(parse_rrule("FREQ=DAILY;COUNT=0").is_err());
        // A non-numeric COUNT is likewise rejected, not ignored.
        assert!(parse_rrule("FREQ=DAILY;COUNT=lots").is_err());
    }

    #[test]
    fn multibyte_byday_token_is_rejected_without_panicking() {
        // A hostile BYDAY whose final bytes are part of a multibyte char must not
        // be sliced mid-codepoint (that would panic). 'Ω' is 2 bytes, so the
        // 3-byte token "Ωa" would split at byte 1, inside Ω.
        let rule = parse_rrule("FREQ=WEEKLY;BYDAY=\u{3a9}a").unwrap();
        assert!(
            rule.by_day.is_empty(),
            "the bad token is dropped, not panicked on"
        );
        // A 2-byte all-multibyte token is also safe.
        assert!(parse_nday("\u{e9}").is_none());
        assert!(parse_nday("\u{e9}\u{e9}").is_none());
    }
}
