// SPDX-License-Identifier: MPL-2.0

/// Validate an EAS Calendar datetime value.
///
/// Spec form: Compact DateTime ([MS-ASDTYPE] §2.7.2 ABNF) —
/// `yyyyMMdd'T'HHmmss'Z'`, e.g. `20130722T090000Z` (§3.6.2 example).
/// The RFC 3339 / ISO 8601 separated form (`2026-08-18T09:00:00Z`, optional
/// fractional seconds, `Z` or ±HH:MM offset) is accepted too: gateways and
/// captures serialize it, and the raw string is kept verbatim for golden
/// fidelity (conversion to unix-secs is downstream, M8 Task 5/6).
///
/// `pub(crate)` (M8 write direction): `calendar_write::validate` reuses this
/// as the ONE datetime-validation policy file-wide — visibility only.
pub(crate) fn is_valid_eas_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() == 16 {
        is_compact_datetime(b)
    } else {
        is_rfc3339_datetime(b)
    }
}

/// Compact form per [MS-ASDTYPE] §2.7.2. Ranges follow the ABNF literally
/// (month ≤ 12, day ≤ 31, hour ≤ 23, minute/seconds ≤ 59); no calendar-day
/// sanity (Feb 30 passes) — the wire value is kept verbatim, strictness
/// beyond the ABNF is the converter's job.
fn is_compact_datetime(b: &[u8]) -> bool {
    b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && two_digits(&b[4..6]) <= 12
        && two_digits(&b[6..8]) <= 31
        && two_digits(&b[9..11]) <= 23
        && two_digits(&b[11..13]) <= 59
        && two_digits(&b[13..15]) <= 59
}

/// Separated form: `yyyy-MM-ddTHH:mm:ss` followed by optional fractional
/// seconds and `Z` or a ±HH:MM offset.
fn is_rfc3339_datetime(b: &[u8]) -> bool {
    if b.len() < 20 {
        return false;
    }
    let shape_ok = b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'T'
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[13] == b':'
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[16] == b':'
        && b[17..19].iter().all(u8::is_ascii_digit);
    if !shape_ok {
        return false;
    }
    if two_digits(&b[5..7]) > 12
        || two_digits(&b[8..10]) > 31
        || two_digits(&b[11..13]) > 23
        || two_digits(&b[14..16]) > 59
        || two_digits(&b[17..19]) > 59
    {
        return false;
    }
    // Fractional seconds, then zone designator.
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false; // '.' must be followed by digits
        }
    }
    match b.get(i) {
        Some(b'Z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            let off = &b[i + 1..];
            off.len() == 5
                && off[..2].iter().all(u8::is_ascii_digit)
                && off[2] == b':'
                && off[3..].iter().all(u8::is_ascii_digit)
                && two_digits(&off[..2]) <= 23
                && two_digits(&off[3..]) <= 59
        }
        _ => false,
    }
}

/// Decode two ASCII digit bytes as a number (callers check digit-ness first).
fn two_digits(b: &[u8]) -> u32 {
    u32::from((b[0] - b'0') * 10 + (b[1] - b'0'))
}
