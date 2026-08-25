//! Parsing an RFC 5545 `RRULE` **value** string into a [`RecurrenceRule`].
//!
//! This is the one shared RRULE-string parser: any provider whose wire format carries
//! a raw `RRULE` (CalDAV/iCalendar's `RRULE:` property, Google Calendar's `recurrence`
//! array) parses it here, so the RFC 5545 grammar lives in exactly one place. Providers
//! that instead receive a *structured* recurrence (Microsoft Graph's
//! `patternedRecurrence`) build the [`RecurrenceRule`] directly and do not use this.
//!
//! The rule is stored in full — every `BY*` part, plus RFC 7529's `RSCALE` and `SKIP` —
//! even where the `engine-recurrence` expander does not expand it. That is the point
//! rather than an oversight: an unsupported rule is preserved and materializes no
//! occurrences (`calendar-semantics.md`), and the expander's refusal only fires on a part
//! that survived parsing. A part dropped here is not a rule missing a detail, it is a
//! *different rule* — one the expander will happily expand, onto dates the series was
//! never on. `FREQ` is required; the rest default. `COUNT` and `UNTIL` are mutually
//! exclusive (RFC 5545); `COUNT` wins if both somehow appear.

use core::num::{NonZeroI32, NonZeroU32};

use crate::{
    calendar::{Frequency, NDay, RecurrenceBound, RecurrenceRule, RecurrenceSkip, Weekday},
    time::LocalDateTime,
};

/// A failure parsing an `RRULE` value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RruleParseError {
    /// `FREQ` was missing or not a known frequency.
    #[error("RRULE missing or invalid FREQ: {0:?}")]
    Frequency(String),
    /// `COUNT` was present but not a positive integer (RFC 5545 requires one).
    #[error("RRULE COUNT not a positive integer: {0:?}")]
    Count(String),
    /// `UNTIL` was present but not a valid date or date-time.
    #[error("bad RRULE UNTIL: {0:?}")]
    Until(String),
}

/// Parses an `RRULE` value (the text after `RRULE:`, e.g. `FREQ=WEEKLY;BYDAY=MO`) into a
/// [`RecurrenceRule`].
///
/// # Errors
///
/// Returns [`RruleParseError`] if `FREQ` is missing/unknown, `COUNT` is not positive, or
/// `UNTIL` is malformed.
pub fn parse_rrule(value: &str) -> Result<RecurrenceRule, RruleParseError> {
    let parts = parse_parts(value);
    let frequency = parts
        .iter()
        .find(|(k, _)| k == "FREQ")
        .and_then(|(_, v)| frequency(v))
        .ok_or_else(|| RruleParseError::Frequency(value.to_owned()))?;
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
    // COUNT and UNTIL are mutually exclusive; COUNT wins if both appear. A COUNT present
    // but zero/unparseable is rejected (RFC 5545: COUNT "MUST be a positive integer")
    // rather than silently falling through to UNTIL/unbounded — the caller degrades a
    // rejected rule to no-recurrence (one occurrence).
    if let Some(count) = get("COUNT") {
        let n = count
            .parse::<u32>()
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| RruleParseError::Count(count.to_owned()))?;
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
    // RFC 7529, and the parts with the sharpest teeth here. The expander refuses a
    // non-Gregorian rule outright, so carrying `RSCALE` is what turns a Hebrew-calendar
    // series into an event reported as unexpandable rather than one quietly expanded onto
    // Gregorian dates. Lowercased because CLDR's identifier is the lowercase one and
    // iCalendar conventionally shouts it — a rule must mean the same thing whether it
    // arrived as `RSCALE=HEBREW` or as JSCalendar's `"rscale": "hebrew"`.
    rule.rscale = get("RSCALE").map(str::to_ascii_lowercase);
    if let Some(skip) = get("SKIP").and_then(skip) {
        rule.skip = skip;
    }
    Ok(rule)
}

/// The `SKIP` token (RFC 7529), or `None` for anything else — leaving the `OMIT` default.
fn skip(value: &str) -> Option<RecurrenceSkip> {
    match value.to_ascii_uppercase().as_str() {
        "OMIT" => Some(RecurrenceSkip::Omit),
        "BACKWARD" => Some(RecurrenceSkip::Backward),
        "FORWARD" => Some(RecurrenceSkip::Forward),
        _ => None,
    }
}

/// Splits the `;`-separated `KEY=value` parts, uppercasing keys.
fn parse_parts(value: &str) -> Vec<(String, String)> {
    value
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(key, val)| (key.trim().to_ascii_uppercase(), val.trim().to_owned()))
        .collect()
}

/// Parses an `UNTIL` value — an RFC 5545 basic-format date (`YYYYMMDD`) or date-time
/// (`YYYYMMDDTHHMMSS`, with an optional trailing `Z`) — into a [`LocalDateTime`]. A
/// date-only `UNTIL` bounds through the end of that day (`23:59:59`).
fn parse_until(value: &str) -> Result<LocalDateTime, RruleParseError> {
    let body = value.trim().trim_end_matches(['Z', 'z']);
    let bad = || RruleParseError::Until(value.to_owned());
    let (y, mo, d): (i32, u8, u8) = (
        field(body, 0..4).ok_or_else(bad)?,
        field(body, 4..6).ok_or_else(bad)?,
        field(body, 6..8).ok_or_else(bad)?,
    );
    if body.len() == 8 {
        return LocalDateTime::new(y, mo, d, 23, 59, 59).map_err(|_| bad());
    }
    // A basic-format date-time: `YYYYMMDDTHHMMSS`.
    if body.len() != 15 || body.as_bytes().get(8) != Some(&b'T') {
        return Err(bad());
    }
    let (h, mi, s): (u8, u8, u8) = (
        field(body, 9..11).ok_or_else(bad)?,
        field(body, 11..13).ok_or_else(bad)?,
        field(body, 13..15).ok_or_else(bad)?,
    );
    LocalDateTime::new(y, mo, d, h, mi, s).map_err(|_| bad())
}

/// Parses the ASCII-digit slice `range` of `s` directly into `T` (no casts), or `None` if
/// out of range, empty, or unparseable.
fn field<T: core::str::FromStr>(s: &str, range: core::ops::Range<usize>) -> Option<T> {
    s.get(range).filter(|t| !t.is_empty())?.parse().ok()
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

/// Parses one `BYDAY` token: an optional signed ordinal followed by a weekday (`MO`,
/// `-1SU`, `2TH`).
fn parse_nday(token: &str) -> Option<NDay> {
    let token = token.trim();
    // The weekday is the final two bytes (two ASCII letters); the optional signed ordinal
    // precedes it. Split only at a char boundary — a token ending in a multibyte char is
    // invalid and rejected, never sliced mid-codepoint (which would panic on hostile
    // input like `BYDAY=Ωa`).
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
    fn parses_a_weekly_rule_with_count_and_byday() {
        let rule = parse_rrule("FREQ=WEEKLY;COUNT=6;BYDAY=MO").unwrap();
        assert_eq!(rule.frequency, Frequency::Weekly);
        assert_eq!(
            rule.bound,
            RecurrenceBound::Count(NonZeroU32::new(6).unwrap())
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
    fn parses_interval_nth_weekday_bymonth_and_wkst() {
        let rule =
            parse_rrule("FREQ=MONTHLY;INTERVAL=2;BYDAY=-1SU,2TH;BYMONTH=3,6;WKST=SU").unwrap();
        assert_eq!(rule.interval.get(), 2);
        assert_eq!(rule.first_day_of_week, Weekday::Su);
        assert_eq!(rule.by_month, vec!["3".to_owned(), "6".to_owned()]);
        assert_eq!(rule.by_day.len(), 2);
        assert_eq!(rule.by_day[0].nth_of_period, NonZeroI32::new(-1));
    }

    #[test]
    fn parses_until_as_a_date_or_datetime() {
        let d = parse_rrule("FREQ=DAILY;UNTIL=20261231").unwrap();
        assert_eq!(
            d.bound,
            RecurrenceBound::Until(LocalDateTime::new(2026, 12, 31, 23, 59, 59).unwrap())
        );
        let dt = parse_rrule("FREQ=DAILY;UNTIL=20261231T235900Z").unwrap();
        assert_eq!(
            dt.bound,
            RecurrenceBound::Until(LocalDateTime::new(2026, 12, 31, 23, 59, 0).unwrap())
        );
    }

    #[test]
    fn rejects_missing_freq_bad_count_and_bad_until() {
        assert!(matches!(
            parse_rrule("BYDAY=MO"),
            Err(RruleParseError::Frequency(_))
        ));
        assert!(matches!(
            parse_rrule("FREQ=DAILY;COUNT=0"),
            Err(RruleParseError::Count(_))
        ));
        assert!(matches!(
            parse_rrule("FREQ=DAILY;UNTIL=nonsense"),
            Err(RruleParseError::Until(_))
        ));
    }

    #[test]
    fn carries_rscale_and_skip_so_a_non_gregorian_rule_is_not_read_as_gregorian() {
        // RFC 7529. Dropping these does not produce a rule missing a detail — it produces a
        // Gregorian rule, which the expander is perfectly willing to expand, onto dates the
        // series was never on. Carried, `RSCALE` reaches the expander's refusal and the event
        // is reported as unexpandable instead of drawn wrong.
        //
        // These are the bytes a Stalwart harness actually returned for such an event, part
        // order and all — it reserializes what it stores, so `RSCALE` arrives after the `BY*`
        // parts rather than first, where RFC 7529's examples put it.
        let rule =
            parse_rrule("FREQ=YEARLY;BYMONTHDAY=1;BYMONTH=1;RSCALE=HEBREW;SKIP=FORWARD").unwrap();
        assert_eq!(rule.rscale.as_deref(), Some("hebrew"));
        assert_eq!(rule.skip, RecurrenceSkip::Forward);
        assert_eq!(rule.by_month_day, vec![1]);
    }

    #[test]
    fn an_rscale_is_stored_as_its_lowercase_cldr_name_whatever_the_wire_said() {
        // Not hypothetical: one server, one event, two transports. Read over CalDAV it is
        // `RSCALE=HEBREW`; read over JMAP the same event is `"rscale": "hebrew"`. CLDR's own
        // identifier is the lowercase one, so that is what both are stored as — keeping the
        // wire's casing would make one calendar system two values that never compare equal.
        for spelling in ["HEBREW", "hebrew", "Hebrew"] {
            let rule = parse_rrule(&format!("RSCALE={spelling};FREQ=YEARLY")).unwrap();
            assert_eq!(rule.rscale.as_deref(), Some("hebrew"), "{spelling}");
        }
    }

    #[test]
    fn skip_defaults_to_omit_and_an_unknown_value_does_not_change_it() {
        // `SKIP` has meaning only under an `RSCALE`, and RFC 7529 makes `OMIT` the default. An
        // unparseable value leaves the default rather than failing the rule: the parser rejects
        // only what RFC 5545 says must be well-formed (FREQ, COUNT, UNTIL).
        assert_eq!(
            parse_rrule("FREQ=DAILY").unwrap().skip,
            RecurrenceSkip::Omit
        );
        assert_eq!(
            parse_rrule("FREQ=DAILY;SKIP=sideways").unwrap().skip,
            RecurrenceSkip::Omit
        );
        assert_eq!(
            parse_rrule("FREQ=DAILY;SKIP=backward").unwrap().skip,
            RecurrenceSkip::Backward
        );
    }
}
