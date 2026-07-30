//! Parsing iCalendar `DATE`, `DATE-TIME`, and `DURATION` values into the engine
//! time model (RFC 5545 §3.3.4–§3.3.6).
//!
//! The three `DATE-TIME` forms map to the engine's four-case
//! [`CalendarDateTime`]: a trailing `Z` is UTC, a `TZID` parameter is a zoned
//! value, and neither is floating. A `VALUE=DATE` (or a bare 8-digit value) is an
//! all-day date.
//!
//! A `TZID` is **not** assumed to be IANA. It very often is not: an Outlook- or
//! Exchange-authored event carries a **Windows** zone name
//! (`DTSTART;TZID=W. Europe Standard Time:20260730T163000`), and those arrive both over
//! CalDAV and — as invitations — over mail. So the parameter goes through the shared
//! [`resolve_zone_name`] policy, which maps a known Windows name through CLDR, passes an
//! IANA id through, and records anything else as
//! [`TimeZoneId::Custom`](engine_core::time::TimeZoneId) rather than as an IANA name that
//! no tzdb can resolve. Expanding a non-IANA embedded `VTIMEZONE` is still staged
//! (`calendar-semantics.md`); until then a custom zone is *honestly unplaceable* instead of
//! silently wrong.

use engine_core::time::{
    CalendarDate, CalendarDateTime, Duration, LocalDateTime, UtcDateTime, resolve_zone_name,
};

use super::unfold::ContentLine;
use crate::error::IcalError;

/// Parses a `DTSTART`/`DTEND`/`RECURRENCE-ID` value, honoring its `VALUE` and
/// `TZID` parameters.
///
/// # Errors
///
/// Returns [`IcalError`] on an unparseable date, time, or zone.
pub(crate) fn parse_calendar_date_time(line: &ContentLine) -> Result<CalendarDateTime, IcalError> {
    let value = line.value.trim();
    let is_date = line
        .param("VALUE")
        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"))
        || (value.len() == 8 && !value.contains(['T', 't']));
    if is_date {
        return Ok(CalendarDateTime::Date(parse_date(value)?));
    }
    if let Some(stripped) = value.strip_suffix(['Z', 'z']) {
        return Ok(CalendarDateTime::utc(parse_local(stripped)?));
    }
    let local = parse_local(value)?;
    match line.param("TZID") {
        Some(tzid) => {
            let zone = resolve_zone_name(tzid)
                .map_err(|e| IcalError::new(format!("bad TZID {tzid:?}: {e}")))?;
            Ok(CalendarDateTime::Zoned { local, zone })
        }
        None => Ok(CalendarDateTime::Floating(local)),
    }
}

/// Parses a comma-separated list value (`EXDATE`/`RDATE`), applying the line's
/// shared `VALUE`/`TZID` parameters to every entry.
///
/// Best-effort: a malformed or empty entry (e.g. a trailing comma) is **skipped**,
/// so one bad `EXDATE` element does not discard the others or the whole event.
pub(crate) fn parse_date_time_list(line: &ContentLine) -> Vec<CalendarDateTime> {
    line.value
        .split(',')
        .filter_map(|entry| {
            let item = ContentLine {
                name: line.name.clone(),
                params: line.params.clone(),
                value: entry.to_owned(),
            };
            parse_calendar_date_time(&item).ok()
        })
        .collect()
}

/// Parses an iCalendar UTC timestamp (`DTSTAMP`/`CREATED`/`LAST-MODIFIED`,
/// `YYYYMMDDThhmmssZ`) into a [`UtcDateTime`].
///
/// # Errors
///
/// Returns [`IcalError`] if the value is not a UTC date-time.
pub(crate) fn parse_utc(value: &str) -> Result<UtcDateTime, IcalError> {
    let body = value
        .trim()
        .strip_suffix(['Z', 'z'])
        .ok_or_else(|| IcalError::new(format!("timestamp not UTC: {value:?}")))?;
    let local = parse_local(body)?;
    UtcDateTime::new(
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second(),
    )
    .map_err(|e| IcalError::new(format!("bad UTC timestamp {value:?}: {e}")))
}

/// Parses an iCalendar `DURATION` value (ISO-8601, RFC 5545 §3.3.6) into a
/// non-negative [`Duration`].
///
/// # Errors
///
/// Returns [`IcalError`] on a malformed or signed duration.
pub(crate) fn parse_duration(value: &str) -> Result<Duration, IcalError> {
    value
        .trim()
        .parse()
        .map_err(|e| IcalError::new(format!("bad DURATION {value:?}: {e}")))
}

/// Parses an 8-digit `YYYYMMDD` basic-format date.
fn parse_date(value: &str) -> Result<CalendarDate, IcalError> {
    if value.len() != 8 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(IcalError::new(format!("bad DATE {value:?}")));
    }
    CalendarDate::new(
        field(value, 0, 4)?,
        field(value, 4, 6)?,
        field(value, 6, 8)?,
    )
    .map_err(|e| IcalError::new(format!("bad DATE {value:?}: {e}")))
}

/// Parses a 15-character `YYYYMMDDThhmmss` basic-format local date-time.
fn parse_local(value: &str) -> Result<LocalDateTime, IcalError> {
    let bytes = value.as_bytes();
    let well_formed = value.len() == 15
        && bytes[8].eq_ignore_ascii_case(&b'T')
        && value[..8].bytes().all(|b| b.is_ascii_digit())
        && value[9..].bytes().all(|b| b.is_ascii_digit());
    if !well_formed {
        return Err(IcalError::new(format!("bad DATE-TIME {value:?}")));
    }
    LocalDateTime::new(
        field(value, 0, 4)?,
        field(value, 4, 6)?,
        field(value, 6, 8)?,
        field(value, 9, 11)?,
        field(value, 11, 13)?,
        field(value, 13, 15)?,
    )
    .map_err(|e| IcalError::new(format!("bad DATE-TIME {value:?}: {e}")))
}

/// Parses the digits `value[start..end]` into any integer type.
fn field<T: core::str::FromStr>(value: &str, start: usize, end: usize) -> Result<T, IcalError> {
    value[start..end]
        .parse()
        .map_err(|_| IcalError::new(format!("bad numeric field in {value:?}")))
}

#[cfg(test)]
mod tests {
    use engine_core::time::TimeZoneId;

    use super::*;

    fn line(name: &str, params: &[(&str, &str)], value: &str) -> ContentLine {
        ContentLine {
            name: name.to_owned(),
            params: params
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn zoned_value_uses_its_tzid() {
        let dt = parse_calendar_date_time(&line(
            "DTSTART",
            &[("TZID", "Europe/Amsterdam")],
            "20260318T100000",
        ))
        .unwrap();
        match dt {
            CalendarDateTime::Zoned { local, zone } => {
                assert_eq!(local.to_string(), "2026-03-18T10:00:00");
                assert_eq!(zone, TimeZoneId::iana("Europe/Amsterdam").unwrap());
            }
            other => panic!("expected zoned, got {other:?}"),
        }
    }

    #[test]
    fn an_outlook_windows_tzid_maps_to_iana_not_a_dead_zone_name() {
        // Regression (G3). This is the literal DTSTART an Outlook invitation carries. The
        // parser used to call `TimeZoneId::iana` on it, which only rejects the empty
        // string — so the zone became `Iana("W. Europe Standard Time")`, no tzdb resolved
        // it, and the meeting had no instant: unplaceable on the grid, and impossible to
        // check for conflicts. It must come out as a real IANA zone.
        let dt = parse_calendar_date_time(&line(
            "DTSTART",
            &[("TZID", "W. Europe Standard Time")],
            "20260730T163000",
        ))
        .unwrap();
        match dt {
            CalendarDateTime::Zoned { local, zone } => {
                assert_eq!(local.to_string(), "2026-07-30T16:30:00");
                assert_eq!(zone, TimeZoneId::iana("Europe/Berlin").unwrap());
                assert!(zone.is_iana());
            }
            other => panic!("expected zoned, got {other:?}"),
        }
    }

    #[test]
    fn an_unplaceable_tzid_is_recorded_as_custom_rather_than_as_fake_iana() {
        // A zone we cannot map must not masquerade as IANA: `Custom` is what tells the rest
        // of the engine "we could not place this", instead of an IANA name that fails to
        // resolve for reasons no caller can distinguish from a tzdb gap.
        let dt = parse_calendar_date_time(&line(
            "DTSTART",
            &[("TZID", "Customized Time Zone")],
            "20260730T163000",
        ))
        .unwrap();
        match dt {
            CalendarDateTime::Zoned { zone, .. } => {
                assert!(!zone.is_iana());
                assert_eq!(zone.as_str(), "Customized Time Zone");
            }
            other => panic!("expected zoned, got {other:?}"),
        }
    }

    #[test]
    fn trailing_z_is_utc_and_bare_value_is_floating() {
        assert_eq!(
            parse_calendar_date_time(&line("DTSTART", &[], "20260119T070000Z")).unwrap(),
            CalendarDateTime::utc("2026-01-19T07:00:00".parse().unwrap())
        );
        assert_eq!(
            parse_calendar_date_time(&line("DTSTART", &[], "20260415T120000")).unwrap(),
            CalendarDateTime::Floating("2026-04-15T12:00:00".parse().unwrap())
        );
    }

    #[test]
    fn value_date_and_bare_eight_digits_are_all_day() {
        let by_param =
            parse_calendar_date_time(&line("DTSTART", &[("VALUE", "DATE")], "20260401")).unwrap();
        let by_shape = parse_calendar_date_time(&line("DTEND", &[], "20260402")).unwrap();
        assert_eq!(
            by_param,
            CalendarDateTime::Date(CalendarDate::new(2026, 4, 1).unwrap())
        );
        assert_eq!(
            by_shape,
            CalendarDateTime::Date(CalendarDate::new(2026, 4, 2).unwrap())
        );
    }

    #[test]
    fn exdate_list_applies_shared_params() {
        let list = parse_date_time_list(&line(
            "EXDATE",
            &[("TZID", "Europe/Amsterdam")],
            "20260119T093000,20260202T093000",
        ));
        assert_eq!(list.len(), 2);
        assert!(matches!(list[0], CalendarDateTime::Zoned { .. }));
    }

    #[test]
    fn exdate_list_skips_empty_and_malformed_entries() {
        // A trailing comma / empty entry must not discard the valid ones (and the
        // event must not be dropped over it).
        let list = parse_date_time_list(&line(
            "EXDATE",
            &[("TZID", "Europe/Amsterdam")],
            "20260119T093000,,garbage",
        ));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn utc_timestamp_and_duration_parse() {
        assert_eq!(
            parse_utc("20260101T000000Z").unwrap().to_string(),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(
            parse_duration("PT1H").unwrap(),
            "PT1H".parse::<Duration>().unwrap()
        );
    }

    #[test]
    fn malformed_values_are_rejected() {
        assert!(parse_calendar_date_time(&line("DTSTART", &[], "2026-03-18")).is_err());
        assert!(parse_calendar_date_time(&line("DTSTART", &[], "20261318T100000")).is_err()); // month 13
        assert!(parse_utc("20260101T000000").is_err()); // missing Z
        assert!(parse_duration("-PT1H").is_err()); // event durations are non-negative
    }
}
