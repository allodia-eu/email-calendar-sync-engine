// SPDX-License-Identifier: MPL-2.0

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

use super::model::{TziRule, TziTimeZone};

// ============================================================================
// Timezone (TZI) blob decode — [MS-ASDTYPE] §2.7.6 (M8 Task 3)
// ============================================================================

/// Byte length of the [MS-ASDTYPE] §2.7.6 TimeZone structure:
/// Bias(4) + StandardName(64) + StandardDate(16) + StandardBias(4) +
/// DaylightName(64) + DaylightDate(16) + DaylightBias(4) = 172.
const TZI_BLOB_LEN: usize = 172;

/// Decode the base64 `Timezone` blob into a [`TziTimeZone`]. Malformed
/// base64 or a wrong-length payload warn and yield `None` — the caller
/// keeps the raw string either way. Never panics.
///
/// `pub(crate)` (M8 write direction): the `calendar_write` tests reuse this
/// to pin the write→parse round-trip of the synthesized TZI blob —
/// visibility only, no logic changed (the Task-4 seam precedent above).
pub(crate) fn parse_tzi_blob(raw: &str) -> Option<TziTimeZone> {
    let bytes = match BASE64_STANDARD.decode(raw) {
        Ok(b) => b,
        Err(e) => {
            log::warn!(
                "calendar Timezone: malformed base64 ({e}); keeping the raw blob, \
                 no TZI parse"
            );
            return None;
        }
    };
    if bytes.len() != TZI_BLOB_LEN {
        log::warn!(
            "calendar Timezone: TZI blob is {} bytes; expected {TZI_BLOB_LEN} per \
             [MS-ASDTYPE] §2.7.6 — keeping the raw blob, no TZI parse",
            bytes.len()
        );
        return None;
    }
    // Field offsets follow §2.7.6 exactly; the two 64-byte name fields are
    // not modeled and skipped.
    let bias = i32_le(&bytes[0..4]);
    let standard = tzi_rule(&bytes[68..84], i32_le(&bytes[84..88]), "Standard");
    let daylight = tzi_rule(&bytes[152..168], i32_le(&bytes[168..172]), "Daylight");
    Some(TziTimeZone {
        base_bias_minutes: bias,
        standard,
        daylight,
    })
}

/// Little-endian i32 from a 4-byte slice.
fn i32_le(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Little-endian u16 from a 2-byte slice.
fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// Decode one 16-byte SYSTEMTIME transition ([MS-DTYP] §2.3.13: 8×
/// little-endian u16 — wYear wMonth wDayOfWeek wDay wHour wMinute wSecond
/// wMilliseconds) into a [`TziRule`].
///
/// An all-zero SYSTEMTIME means "no transition" (flat zone, no DST) →
/// `None`. wYear is ignored with a debug note — recurring transitions carry
/// 0 and the v1 model keeps only the recurring fields. Out-of-range fields
/// warn + drop the rule.
fn tzi_rule(st: &[u8], bias_offset_minutes: i32, which: &'static str) -> Option<TziRule> {
    let year = u16_le(&st[0..2]);
    let month = u16_le(&st[2..4]);
    let day_of_week = u16_le(&st[4..6]);
    let day_occurrence = u16_le(&st[6..8]);
    let hour = u16_le(&st[8..10]);
    let minute = u16_le(&st[10..12]);
    let second = u16_le(&st[12..14]);
    let millis = u16_le(&st[14..16]);
    if (
        year,
        month,
        day_of_week,
        day_occurrence,
        hour,
        minute,
        second,
        millis,
    ) == (0, 0, 0, 0, 0, 0, 0, 0)
    {
        return None; // zeroed SYSTEMTIME = rule inactive (no DST)
    }
    if year != 0 {
        log::debug!(
            "calendar Timezone: {which}Date SYSTEMTIME carries absolute year {year}; \
             the v1 model only keeps the recurring-transition fields"
        );
    }
    let valid = (1..=12).contains(&month)
        && day_of_week <= 6
        && (1..=5).contains(&day_occurrence)
        && hour <= 23
        && minute <= 59;
    if !valid {
        log::warn!(
            "calendar Timezone: {which}Date SYSTEMTIME out of range (month={month} \
             dayOfWeek={day_of_week} day={day_occurrence} hour={hour} minute={minute}); \
             dropping the rule"
        );
        return None;
    }
    Some(TziRule {
        bias_offset_minutes,
        month,
        day_of_week,
        day_occurrence,
        hour,
        minute,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{
            CAL_TIMEZONE, PAGE_CALENDAR, TimeZoneBlob, parse_calendar_application_data,
            tests::TZI_FLAT_UTC8,
        },
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
        wbxml::WbxmlElement,
    };

    /// (b) DST zone UTC+1/UTC+2 (CET/CEST shape): Bias = -60 (`C4 FF FF
    /// FF`); StandardDate = last Sunday of October at 03:00 (wMonth=10,
    /// wDayOfWeek=0, wDay=5, wHour=3) with StandardBias 0; DaylightDate =
    /// last Sunday of March at 02:00 with DaylightBias = -60 (UTC+2 while
    /// DST is in effect).
    const TZI_DST_CET: &str = "xP///wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAoAAAAFAAMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMAAAAFAAIAAAAAAAAAxP///w==";

    /// Case (a): flat zone — bias -480 (UTC+8), zeroed SYSTEMTIMEs mean no
    /// DST, so both rules decode to None.
    #[test]
    fn parse_timezone_flat_zone_no_dst() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                PAGE_CALENDAR,
                CAL_TIMEZONE,
                TZI_FLAT_UTC8,
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.time_zone,
            Some(TimeZoneBlob {
                raw_base64: Some(TZI_FLAT_UTC8.to_string()),
                parsed: Some(TziTimeZone {
                    base_bias_minutes: -480,
                    standard: None,
                    daylight: None,
                }),
            })
        );
    }

    /// Case (b): DST zone — bias -60 (UTC+1); standard transition last
    /// Sunday of October at 03:00 (offset 0), daylight transition last
    /// Sunday of March at 02:00 (offset -60 ⇒ UTC+2 while in DST).
    #[test]
    fn parse_timezone_dst_zone_last_sunday_transitions() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(PAGE_CALENDAR, CAL_TIMEZONE, TZI_DST_CET)],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        let blob = props.time_zone.expect("Timezone present");
        assert_eq!(blob.raw_base64.as_deref(), Some(TZI_DST_CET));
        assert_eq!(
            blob.parsed,
            Some(TziTimeZone {
                base_bias_minutes: -60,
                standard: Some(TziRule {
                    bias_offset_minutes: 0,
                    month: 10,
                    day_of_week: 0,
                    day_occurrence: 5,
                    hour: 3,
                    minute: 0,
                }),
                daylight: Some(TziRule {
                    bias_offset_minutes: -60,
                    month: 3,
                    day_of_week: 0,
                    day_occurrence: 5,
                    hour: 2,
                    minute: 0,
                }),
            })
        );
    }

    /// Case (c): garbage and short blobs — `parsed` degrades to None with a
    /// warning, the raw string is kept, nothing panics.
    #[test]
    fn parse_timezone_malformed_blob_keeps_raw_none_parsed() {
        for bad in [
            "!!!not-base64!!!", // invalid base64 alphabet
            "3gclAC4AAAA=",     // valid base64, only 8 bytes
            "AQIDBAUGBwgJCg==", // valid base64, 10 bytes
        ] {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(PAGE_CALENDAR, CAL_TIMEZONE, bad)],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.time_zone,
                Some(TimeZoneBlob {
                    raw_base64: Some(bad.to_string()),
                    parsed: None,
                }),
                "malformed blob \"{bad}\" must keep raw + parsed None"
            );
        }
    }
}
