// SPDX-License-Identifier: MPL-2.0

use super::fields::{tag_label, text_value_opt};
use crate::wbxml::{
    WbxmlElement,
    tags::{base, pages},
};

/// `airsyncbase:Location` (page 17, 0x20; 16.0/16.1 only) is a CONTAINER
/// element — [MS-ASAIRS] §2.2.2.28: "The Location element is container data
/// type" — whose human-readable value lives in its `DisplayName` child
/// ([MS-ASAIRS] §2.2.2.22.3, string, "MUST have a maximum of one", 14.1+).
/// All the other §2.2.2.28 children (LocationUri, Accuracy, Latitude,
/// Longitude, Altitude, AltitudeAccuracy, Annotation, Street, City, State,
/// Country, PostalCode) are optional structured-location data the v1 model
/// does not carry — debug-skipped.
///
/// M8-L1 (2026-08-17 live seed drill): the original parser read the
/// container's own (always empty) text via `text_value_opt`, silently
/// dropping LOCATION for every real-Exchange-16.x event — hence the
/// DisplayName-first shape here. M8-L1 variant: the email `MeetingRequest`
/// parse (`commands/sync.rs`) reads the same page-17 container through
/// this helper with `ctx = "email MeetingRequest"` — calendar and email
/// share one Location parse policy.
///
/// `ctx` is the log-context prefix (which parser emitted the line) —
/// e.g. `"calendar ApplicationData"` or `"email MeetingRequest"`.
///
/// Degrades loudly, never panics, never invents a value:
/// * duplicate DisplayName children (spec violation) → warn, last wins;
/// * a container without a DisplayName (legal: every child is optional; an empty container is the
///   "no/cleared location" shape) → `None` with a debug note;
/// * defensive fallback: a leaf Location carrying text directly (a gateway serializing the ≤14.1
///   shape on page 17) still parses — pinned by `parse_location_accepts_airsyncbase_16_1_form`.
pub(crate) fn parse_location_16x(ctx: &'static str, elem: &WbxmlElement) -> Option<String> {
    if let Some(text) = text_value_opt(elem) {
        return Some(text);
    }
    let mut display: Option<String> = None;
    let mut display_seen = false;
    let mut other_children = 0usize;
    for child in &elem.children {
        if let (pages::BASE, base::DISPLAY_NAME) = (child.page, child.token) {
            if display_seen {
                log::warn!(
                    "{ctx}: Location carries more than one \
                     DisplayName child — [MS-ASAIRS] §2.2.2.22.3 allows at most one; \
                     keeping the last"
                );
            }
            display_seen = true;
            display = text_value_opt(child);
        } else {
            other_children += 1;
            log::debug!(
                "{ctx}: Location: skipping unmodeled child {} \
                 (page {} token 0x{:02X}) — v1 models only the DisplayName",
                tag_label(child),
                child.page,
                child.token
            );
        }
    }
    if !display_seen {
        log::debug!(
            "{ctx}: Location container without a DisplayName child \
             ({other_children} other child(ren)); location stays unset"
        );
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        calendar::{
            BASE_LOCATION, CAL_EXCEPTION, CAL_EXCEPTION_START_TIME, CAL_EXCEPTIONS, PAGE_CALENDAR,
            parse_calendar_application_data,
        },
        commands::{AS_APPLICATION_DATA, PAGE_AIRSYNC},
        wbxml::tags::{base, pages},
    };

    /// 16.x wire form, LEAF shape (defensive): `airsyncbase:Location`
    /// (page 17, 0x20) per [MS-ASWBXML] §2.1.2.1.5 note 2 carrying its
    /// text directly. The spec form is a container — see
    /// [`parse_location_16x_container_reads_display_name_child`]; this
    /// test pins the tolerant fallback for gateways that serialize the
    /// ≤14.1 shape on page 17.
    #[test]
    fn parse_location_accepts_airsyncbase_16_1_form() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::text(
                pages::BASE,
                BASE_LOCATION,
                "Teams Meeting",
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.location.as_deref(), Some("Teams Meeting"));
    }

    // ====================================================================
    // M8-L1 (2026-08-17 live seed drill) — the 16.x Location CONTAINER
    // shape. [MS-ASAIRS] §2.2.2.28: "The Location element is container
    // data type" whose children are "all ... optional" — the
    // human-readable value is the DisplayName child (§2.2.2.22.3, max
    // one, 14.1+). All 27 drilled events (real Exchange 16.x) lost
    // LOCATION because the parser read the container's own (always
    // empty) text instead of the DisplayName child.
    // ====================================================================

    /// RED: a page-17 Location CONTAINER with a DisplayName child must
    /// yield the DisplayName text.
    #[test]
    fn parse_location_16x_container_reads_display_name_child() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                BASE_LOCATION,
                vec![
                    WbxmlElement::text(
                        pages::BASE,
                        base::DISPLAY_NAME,
                        "Teams Room 4A, Building 2",
                    ),
                    // Structured siblings (§2.2.2.28: all optional) —
                    // unmodeled in v1, must be skipped without error.
                    // LocationUri = page 17, token 0x2C.
                    WbxmlElement::text(pages::BASE, 0x2C, "https://maps.example.com/4a"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.location.as_deref(),
            Some("Teams Room 4A, Building 2"),
            "airsyncbase:Location container must yield its DisplayName child text"
        );
    }

    /// RED: the same container form inside an Exception (§2.2.2.28 lists
    /// `calendar:Exception` among Location's parents) must fill the
    /// exception's location.
    #[test]
    fn exception_location_16x_container_reads_display_name_child() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                PAGE_CALENDAR,
                CAL_EXCEPTIONS,
                vec![WbxmlElement::container(
                    PAGE_CALENDAR,
                    CAL_EXCEPTION,
                    vec![
                        WbxmlElement::text(
                            PAGE_CALENDAR,
                            CAL_EXCEPTION_START_TIME,
                            "20260901T090000Z",
                        ),
                        WbxmlElement::container(
                            pages::BASE,
                            BASE_LOCATION,
                            vec![WbxmlElement::text(
                                pages::BASE,
                                base::DISPLAY_NAME,
                                "Overflow Room B",
                            )],
                        ),
                    ],
                )],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(props.exceptions.len(), 1);
        assert_eq!(
            props.exceptions[0].location.as_deref(),
            Some("Overflow Room B"),
            "Exception-level airsyncbase:Location container must yield its DisplayName"
        );
    }

    /// RED: more than one DisplayName child violates [MS-ASAIRS]
    /// §2.2.2.22.3 ("MUST have a maximum of one") — warn and keep the
    /// last (the file-wide later-element-wins convention).
    #[test]
    fn location_16x_duplicate_display_name_warns_and_keeps_last() {
        let app_data = WbxmlElement::container(
            PAGE_AIRSYNC,
            AS_APPLICATION_DATA,
            vec![WbxmlElement::container(
                pages::BASE,
                BASE_LOCATION,
                vec![
                    WbxmlElement::text(pages::BASE, base::DISPLAY_NAME, "First Room"),
                    WbxmlElement::text(pages::BASE, base::DISPLAY_NAME, "Second Room"),
                ],
            )],
        );
        let props = parse_calendar_application_data(&app_data).expect("parse ok");
        assert_eq!(
            props.location.as_deref(),
            Some("Second Room"),
            "duplicate DisplayName children must keep the last value (with a warn)"
        );
    }

    /// PIN (legal wire shapes, not errors — §2.2.2.28: every child is
    /// optional): a Location container WITHOUT a DisplayName yields
    /// `None` — never an invented value.
    #[test]
    fn location_16x_container_without_display_name_is_none() {
        for children in [
            // Structured-geo-only container (Latitude 0x27 / Longitude 0x28).
            vec![
                WbxmlElement::text(pages::BASE, 0x27, "47.641944"),
                WbxmlElement::text(pages::BASE, 0x28, "-122.127222"),
            ],
            // Empty container — the "no location / cleared" shape.
            vec![],
        ] {
            let app_data = WbxmlElement::container(
                PAGE_AIRSYNC,
                AS_APPLICATION_DATA,
                vec![WbxmlElement::container(
                    pages::BASE,
                    BASE_LOCATION,
                    children,
                )],
            );
            let props = parse_calendar_application_data(&app_data).expect("parse ok");
            assert_eq!(
                props.location, None,
                "a DisplayName-less Location container must not invent a location"
            );
        }
    }
}
