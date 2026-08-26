// SPDX-License-Identifier: MPL-2.0
// TZI synthesis (design D6: fixed offset, no DST rules).

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

// ============================================================================
// TZI synthesis (design D6: fixed offset, no DST rules)
// ============================================================================

/// Byte length of the [MS-ASDTYPE] §2.7.6 TimeZone structure — mirrors
/// `calendar::TZI_BLOB_LEN` (kept private there):
/// Bias(4) + StandardName(64) + StandardDate(16) + StandardBias(4) +
/// DaylightName(64) + DaylightDate(16) + DaylightBias(4) = 172.
const TZI_BLOB_LEN: usize = 172;

/// Synthesize a flat fixed-offset TZI blob ([MS-ASDTYPE] §2.7.6), base64
/// STANDARD: `Bias = -(local_minus_utc_minutes)` (§2.7.6 sign convention —
/// UTC−local, so UTC+8 ⇒ Bias −480) with both name fields, both SYSTEMTIME
/// transition rules and both rule biases zeroed = no DST (design D6).
///
/// Round-trips through `calendar::parse_tzi_blob` into a `TziTimeZone`
/// with `standard: None, daylight: None`.
pub fn build_fixed_offset_tzi_base64(local_minus_utc_minutes: i32) -> String {
    // All-zero 172-byte structure, Bias at offset 0 ([MS-ASDTYPE] §2.7.6):
    // zeroed SYSTEMTIMEs = no transitions, zeroed rule biases = no DST.
    // `saturating_neg` so a nonsensical i32::MIN input degrades to i32::MAX
    // instead of panicking on negation overflow (debug builds).
    let mut blob = [0u8; TZI_BLOB_LEN];
    let bias = local_minus_utc_minutes.saturating_neg();
    blob[0..4].copy_from_slice(&bias.to_le_bytes());
    BASE64_STANDARD.encode(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::{TziTimeZone, parse_tzi_blob, tests::TZI_FLAT_UTC8};

    /// UTC+8 (local_minus_utc = 480 ⇒ Bias −480) must byte-for-byte equal
    /// the golden flat fixture from `calendar/tests.rs` — one source of truth for
    /// the 172-byte layout — and decode to a rule-less TziTimeZone.
    #[test]
    fn tzi_utc8_matches_golden_flat_fixture() {
        let b64 = build_fixed_offset_tzi_base64(480);
        assert_eq!(
            b64, TZI_FLAT_UTC8,
            "synthesized blob must equal the golden flat UTC+8 fixture"
        );
        assert_eq!(
            BASE64_STANDARD.decode(&b64).expect("valid base64").len(),
            172
        );
        assert_eq!(
            parse_tzi_blob(&b64),
            Some(TziTimeZone {
                base_bias_minutes: -480,
                standard: None,
                daylight: None,
            })
        );
    }

    /// Bias sign convention at the extremes of the ruling: UTC+0 ⇒ Bias 0;
    /// UTC−5 (local_minus_utc = −300) ⇒ Bias +300. All rules stay inactive.
    #[test]
    fn tzi_zero_and_negative_offsets() {
        for (local_minus_utc, expected_bias) in [(0, 0), (-300, 300)] {
            let b64 = build_fixed_offset_tzi_base64(local_minus_utc);
            assert_eq!(
                BASE64_STANDARD.decode(&b64).expect("valid base64").len(),
                172,
                "local_minus_utc {local_minus_utc}"
            );
            assert_eq!(
                parse_tzi_blob(&b64),
                Some(TziTimeZone {
                    base_bias_minutes: expected_bias,
                    standard: None,
                    daylight: None,
                }),
                "local_minus_utc {local_minus_utc} ⇒ Bias {expected_bias}"
            );
        }
    }

    // ====================================================================
    // Structural serialization
    // ====================================================================
}
