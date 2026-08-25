// SPDX-License-Identifier: MPL-2.0
//! `email:GlobalObjId` → `calendar:UID` conversion ([MS-ASEMAIL] §3.1.4.7 /
//! §4.3) — the ≤14.1 half of the invite↔event correlation.
//!
//! At protocol 16.0/16.1 the meeting request carries `calendar:UID` directly
//! and "no conversion is necessary" ([MS-ASWBXML] §2.1.2.1.4 note 4). At
//! ≤14.1 it carries `email:GlobalObjId` — a base64 GLOBALOBJID structure —
//! which this module converts to the SAME string space the calendar item's
//! `calendar:UID` lives in, so the exact-key join works across protocol
//! versions.
//!
//! Algorithm (§3.1.4.7 steps 1-5, verbatim semantics):
//! 1. base64-decode the element value;
//! 2. classify OutlookID vs vCal ID — it is an OutlookID if ANY of: the decoded length < 53 bytes,
//!    bytes 41-48 ≠ `"vCal-Uid"`, or the little-endian u32 at bytes 37-40 is < 13 or exceeds the
//!    remaining length;
//! 3. OutlookID → zero bytes 17-20, hex-encode the whole value (uppercase — the spec's own §4.3
//!    example output);
//! 4. vCal ID (spec steps 4-5) → the UID length is the data length (bytes 37-40) minus 12 marker
//!    bytes minus one NUL terminator; the UID is that many bytes starting at byte 53 (1-based),
//!    decoded as UTF-8.
//!
//! Every malformed input (bad base64, truncated payload, non-UTF-8 vCal UID)
//! degrades to `None` with a `log::warn!` — never a panic, never an invented
//! join key (a fabricated UID would silently link the wrong invite).

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

/// The vCal-Uid marker at bytes 41-48 (§3.1.4.7 step 2, 1-based inclusive).
const VCAL_MARKER: &[u8] = b"vCal-Uid";

/// Convert a base64 `email:GlobalObjId` element value to the calendar UID
/// string ([MS-ASEMAIL] §3.1.4.7). `None` for absent/malformed input.
pub fn global_obj_id_to_uid(raw: Option<&str>) -> Option<String> {
    let raw = raw.filter(|s| !s.is_empty())?;
    let bytes = match BASE64_STANDARD.decode(raw) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("GlobalObjId: malformed base64 ({e}); no calendar-identity key");
            return None;
        }
    };
    if is_outlook_id(&bytes) {
        let hex = hex_uid_zeroing_instance_date(&bytes);
        // Defensive: an all-empty decode must not yield an empty join key.
        return if hex.is_empty() { None } else { Some(hex) };
    }
    vcal_uid(&bytes)
}

/// §3.1.4.7 step 2: the OutlookID classification. `true` when ANY of the
/// three disqualifiers of the vCal shape holds. Byte numbers below are
/// 1-BASED per the spec; the slices use the 0-based equivalents
/// (1-based bytes 37-40 → `b[36..40]`; 1-based bytes 41-48 → `b[40..48]`).
fn is_outlook_id(b: &[u8]) -> bool {
    if b.len() < 53 {
        return true;
    }
    if &b[40..48] != VCAL_MARKER {
        return true;
    }
    let data_len = data_length(b);
    // < 13, or greater than the remaining length (the bytes after 1-based
    // byte 40: len − 40).
    data_len < 13 || data_len > b.len() - 40
}

/// The little-endian u32 BYTECOUNT at 1-based bytes 37-40 (`b[36..40]`).
fn data_length(b: &[u8]) -> usize {
    u32::from_le_bytes([b[36], b[37], b[38], b[39]]) as usize
}

/// §3.1.4.7 step 3: zero bytes 17-20 (INSTDATE — the instance date must not
/// vary the identity), hex-encode everything (uppercase, the §4.3 example
/// output form).
fn hex_uid_zeroing_instance_date(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(b.len() * 2);
    for (i, byte) in b.iter().enumerate() {
        let byte = if (16..20).contains(&i) { 0 } else { *byte };
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// §3.1.4.7 steps 4-5: extract the embedded vCal UID. The length is the data
/// length (1-based bytes 37-40, `data_length`) minus 12 marker bytes
/// ("vCal-Uid" + VERSION) minus one trailing NUL; the UID starts at 1-based
/// byte 53 (index 52).
fn vcal_uid(b: &[u8]) -> Option<String> {
    let uid_len = data_length(b).checked_sub(12 + 1)?;
    let end = 52usize.checked_add(uid_len)?;
    if end > b.len() {
        log::warn!(
            "GlobalObjId: vCal UID length {uid_len} overruns the payload ({} bytes); \
             no calendar-identity key",
            b.len()
        );
        return None;
    }
    match std::str::from_utf8(&b[52..end]) {
        Ok(s) => Some(s.to_string()),
        Err(e) => {
            log::warn!("GlobalObjId: vCal UID is not valid UTF-8 ({e}); no key");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two §4.3 golden examples, input base64 → expected UID, verbatim
    // from the spec text (v20220429, "4.3 Converting a GlobalObjId to a UID").

    #[test]
    fn spec_example_1_outlook_id() {
        let uid = global_obj_id_to_uid(Some(
            "BAAAAIIA4AB0xbcQGoLgCAfUCRDgQMnBJoXEAQAAAAAAAAAAEAAAAAvw7UtuTulOnjnjhns3jvM=",
        ))
        .expect("OutlookID converts");
        assert_eq!(
            uid,
            "040000008200E00074C5B7101A82E00800000000E040C9C12685C4010000000000000000100000000BF0ED4B6E4EE94E9E39E3867B378EF3"
        );
    }

    #[test]
    fn spec_example_2_vcal_uid() {
        let uid = global_obj_id_to_uid(Some(
            "BAAAAIIA4AB0xbcQGoLgCAAAAAAAAAAAAAAAAAAAAAAAAAAAMwAAAHZDYWwtVWlkAQAAAHs4MTQxMkQzQy0yQTI0LTRFOUQtQjIwRS0xMUY3QkJFOTI3OTl9AA==",
        ))
        .expect("vCal-Uid converts");
        assert_eq!(uid, "{81412D3C-2A24-4E9D-B20E-11F7BBE92799}");
    }

    #[test]
    fn none_for_absent_and_malformed() {
        assert_eq!(global_obj_id_to_uid(None), None);
        assert_eq!(global_obj_id_to_uid(Some("")), None);
        assert_eq!(global_obj_id_to_uid(Some("!!!not-base64!!!")), None);
        // Valid base64 but truncated (12 bytes) — OutlookID by the <53 rule
        // still hex-encodes; a 4-byte payload likewise (a present, decodable
        // value is DATA per the spec — only the vCal shape can fail after
        // decode).
        assert!(global_obj_id_to_uid(Some("AQIDBAUGBwgJCg==")).is_some());
    }

    /// §3.1.4.7 step 2's third disqualifier: a vCal MARKER present but the
    /// declared data length overrunning the payload classifies as an
    /// OutlookID (hex form) — the spec funnels every inconsistent vCal
    /// shape into the OutlookID arm, so no partial UID can ever escape.
    #[test]
    fn vcal_shape_with_overrunning_length_classifies_as_outlook_id() {
        use base64::engine::general_purpose::STANDARD as B64;
        // 60 zero bytes with the marker spliced at 40..48 and a huge
        // little-endian length (0x0000FFFF = 65535) at 36..40.
        let mut b = vec![0u8; 60];
        b[40..48].copy_from_slice(b"vCal-Uid");
        b[36] = 0xFF;
        b[37] = 0xFF;
        let raw = B64.encode(b);
        let uid = global_obj_id_to_uid(Some(&raw)).expect("OutlookID arm converts");
        assert_eq!(uid.len(), 120, "hex of the 60-byte payload");
        assert!(
            uid.starts_with(&format!("{}FFFF", "0".repeat(72))),
            "the huge little-endian length at bytes 37-40 rides the hex form: {uid}"
        );
        assert!(
            uid.contains("7643616C2D556964"),
            "the vCal-Uid marker bytes ride the hex form: {uid}"
        );
    }
}
