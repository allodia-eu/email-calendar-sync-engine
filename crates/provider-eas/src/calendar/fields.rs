// SPDX-License-Identifier: MPL-2.0

use super::datetime::is_valid_eas_datetime;
use crate::wbxml::{WbxmlElement, WbxmlValue};

// ============================================================================
// Field-parse helpers (permissive: warn + default, never panic — the
// ApplicationData precedent from `commands/sync/parse_item.rs`)
// ============================================================================

/// Permissive text extraction — the `commands::text_value_opt` twin:
/// missing or non-text values map to `None` rather than aborting the item
/// parse. (Local copy because the commands-module helper is private.)
pub(super) fn text_value_opt(elem: &WbxmlElement) -> Option<String> {
    match &elem.value {
        WbxmlValue::Text(s) => Some(s.clone()),
        WbxmlValue::Opaque(b) => std::str::from_utf8(b)
            .ok()
            .map(std::string::ToString::to_string),
        WbxmlValue::Empty => None,
    }
}

/// Boolean per [MS-ASDTYPE] §2.1: `"1"` = true, `"0"` = false. Absent →
/// default `false`; any other text → warn + `false` (loud, never silent).
pub(super) fn parse_bool_field(name: &'static str, elem: &WbxmlElement) -> bool {
    match text_value_opt(elem).as_deref() {
        Some("1") => true,
        Some("0") | None => false,
        Some(other) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{other}\"; \
                 expected \"0\" or \"1\", defaulting to false"
            );
            false
        }
    }
}

/// Tri-state boolean for OPTIONAL wire elements whose ABSENCE is
/// semantically different from `"0"` (the Exception-level AllDayEvent,
/// [MS-ASCAL] §2.2.2.21): `"1"` → `Some(true)`, `"0"` → `Some(false)`.
/// Present-but-unreadable (other text, or an element without text) →
/// warn + `None` — an unreadable value carries no information, so the
/// caller falls back exactly like absence (loud, never silent, never a
/// forced default). Element-level ABSENCE is the caller's detection:
/// the field simply stays at its `None` default.
pub(super) fn parse_tri_bool_field(name: &'static str, elem: &WbxmlElement) -> Option<bool> {
    match text_value_opt(elem).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        Some(other) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{other}\"; expected \
                 \"0\" or \"1\", treating the element as absent"
            );
            None
        }
        None => {
            log::warn!(
                "calendar ApplicationData: {name} element without a text value; \
                 treating it as absent"
            );
            None
        }
    }
}

/// DateTime per [MS-ASCAL] §2.2.2.42/§2.2.2.20/§2.2.2.18 — the raw wire
/// string when it validates, else warn + `None`.
pub(super) fn parse_datetime_field(name: &'static str, elem: &WbxmlElement) -> Option<String> {
    let raw = text_value_opt(elem)?;
    if is_valid_eas_datetime(&raw) {
        Some(raw)
    } else {
        log::warn!(
            "calendar ApplicationData: malformed {name} \"{raw}\"; expected Compact \
             DateTime per [MS-ASDTYPE] §2.7.2, ignoring"
        );
        None
    }
}

/// Numeric enum: parse to u8, check against the spec value set, otherwise
/// warn + `None`. Absent → `None`.
pub(super) fn parse_enum_field(
    name: &'static str,
    elem: &WbxmlElement,
    valid: impl Fn(u8) -> bool,
) -> Option<u8> {
    let raw = text_value_opt(elem)?;
    match raw.parse::<u8>() {
        Ok(n) if valid(n) => Some(n),
        Ok(n) => {
            log::warn!(
                "calendar ApplicationData: {name} value {n} outside the [MS-ASCAL] \
                 range; ignoring"
            );
            None
        }
        Err(_) => {
            log::warn!(
                "calendar ApplicationData: malformed {name} \"{raw}\"; expected a \
                 number, ignoring"
            );
            None
        }
    }
}

/// `airsyncbase:Body` on a calendar item → the plain-text payload, if any.
/// Type 1 (PlainText) fills `body_plain`; Type 2 (HTML) / Type 4 (MIME) are
/// valid wire data but not modeled on calendar items in v1 (debug-logged);
/// a Body without a parseable Type warns and keeps the data as plain
/// (graceful degradation, the Email `parse_body` precedent).
pub(super) fn parse_calendar_body(elem: &WbxmlElement) -> Option<String> {
    let mut body_type: Option<u8> = None;
    let mut data: Option<String> = None;
    for child in &elem.children {
        match child.tag_name() {
            "Type" => body_type = text_value_opt(child).and_then(|s| s.parse().ok()),
            "Data" => data = text_value_opt(child),
            "EstimatedDataSize" | "Truncated" => {} // not surfaced on calendar items
            _ => {
                log::debug!(
                    "calendar ApplicationData: skipping unexpected Body child {} \
                     (page {} token 0x{:02X})",
                    tag_label(child),
                    child.page,
                    child.token
                );
            }
        }
    }
    match body_type {
        Some(1) => data,
        Some(other) => {
            log::debug!(
                "calendar ApplicationData: Body Type {other} (not PlainText) — \
                 calendar bodies are plain-only in v1; skipping payload"
            );
            None
        }
        None => {
            if data.is_some() {
                log::warn!(
                    "calendar ApplicationData: Body without a parseable Type; \
                     keeping payload as plain text"
                );
            }
            data
        }
    }
}

/// Human-readable tag name for log lines — unlike `WbxmlElement::tag_name()`
/// this never warns on unregistered tokens (a skip line must not spawn a
/// second warn for the same element).
pub(super) fn tag_label(elem: &WbxmlElement) -> String {
    match crate::wbxml::code_page(elem.page).and_then(|p| p.tag_name(elem.token)) {
        Some(name) => name.to_string(),
        None => format!("unknown-0x{:02X}", elem.token),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{CAL_START_TIME, PAGE_CALENDAR, parse_calendar_application_data},
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
        wbxml::tags::{base, pages},
    };

    /// DateTime acceptance: the spec form is Compact DateTime
    /// (`20130722T090000Z`, [MS-ASDTYPE] §2.7.2/§3.6.2); the separated
    /// RFC 3339 form is accepted defensively. Range violations reject.
    #[test]
    fn parse_datetime_accepts_compact_and_rfc3339_forms() {
        let cases: &[(&str, bool)] = &[
            ("20130722T090000Z", true),          // spec §3.6.2 example
            ("20260818T235959Z", true),          // edge of day
            ("2026-08-18T09:00:00Z", true),      // separated UTC
            ("2026-08-18T09:00:00.123Z", true),  // fractional seconds
            ("2026-08-18T09:00:00+08:00", true), // numeric offset
            ("20260818T250000Z", false),         // hour 25
            ("20260818T096100Z", false),         // minute 61
            ("2026-13-18T09:00:00Z", false),     // month 13
            ("2026-08-18T09:00:00", false),      // missing zone designator
            ("2026-08-18 09:00:00Z", false),     // space instead of T
        ];
        for &(value, ok) in cases {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::text(PAGE_CALENDAR, CAL_START_TIME, value)],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.start_time.as_deref() == Some(value),
                ok,
                "StartTime \"{value}\" acceptance mismatch"
            );
        }
    }

    /// Only a PlainText (Type 1) body fills `body_plain`; an HTML body is
    /// valid wire data but not modeled on calendar items in v1.
    #[test]
    fn parse_body_plain_only_for_type_1() {
        let html = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                base::BODY,
                vec![
                    WbxmlElement::text(pages::BASE, base::TYPE, "2"),
                    WbxmlElement::text(pages::BASE, base::DATA, "<p>html body</p>"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&html).expect("parse ok");
        assert_eq!(props.body_plain, None, "HTML calendar body is not plain");
    }
}
