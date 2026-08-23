//! Locating the `VEVENT`s of a calendar object resource within its raw text.
//!
//! A patch has to know *which lines it may touch*. Two traps make that less obvious
//! than a scan for `SUMMARY:`:
//!
//! - A `VEVENT` nests sub-components, and a `VALARM` has its own `SUMMARY`, `DESCRIPTION` and
//!   `DURATION`. Patching the first `SUMMARY` inside the `VEVENT`'s line range would retitle the
//!   *alarm*. So the scan tracks component depth and records only the properties directly inside
//!   each `VEVENT`.
//! - One resource holds several `VEVENT`s sharing a `UID` (RFC 4791 §4.1): the series **master**
//!   plus its `RECURRENCE-ID` overrides. Which one an edit lands on is the difference between
//!   moving one occurrence and rewriting a weekly standup for all time.

use core::ops::Range;

use engine_core::time::CalendarDateTime;

use super::super::{
    lines::Document,
    unfold::{content_lines, split_once_unquoted},
    value::parse_calendar_date_time,
};
use crate::error::IcalError;

/// One `VEVENT` component, located by logical-line index.
#[derive(Debug)]
pub(super) struct Vevent {
    /// The `BEGIN:VEVENT` line.
    begin: usize,
    /// The `END:VEVENT` line.
    end: usize,
    /// The properties **directly** inside this `VEVENT` — never a nested `VALARM`'s.
    pub(super) own: Vec<usize>,
    /// Where a property this `VEVENT` lacks gets inserted: before its first nested
    /// sub-component, else before `END:VEVENT`. RFC 5545 §3.6.1 puts properties ahead
    /// of the alarms, so appending blindly before `END:VEVENT` would emit them after a
    /// `VALARM`.
    pub(super) anchor: usize,
}

impl Vevent {
    /// The logical lines this component spans, `BEGIN` through `END` inclusive.
    pub(super) fn groups(&self) -> Range<usize> {
        self.begin..self.end + 1
    }

    /// The logical-line index of this `VEVENT`'s own `name` property, if it has one.
    pub(super) fn property(&self, doc: &Document, name: &str) -> Option<usize> {
        self.own
            .iter()
            .copied()
            .find(|&group| property_name(&doc.logical(group)).eq_ignore_ascii_case(name))
    }

    /// This `VEVENT`'s own `name` property parsed as a date-time (`DTSTART`,
    /// `RECURRENCE-ID`), honoring its `TZID`/`VALUE` parameters.
    pub(super) fn date_time(
        &self,
        doc: &Document,
        name: &str,
    ) -> Option<Result<CalendarDateTime, IcalError>> {
        let group = self.property(doc, name)?;
        let logical = doc.logical(group);
        let line = content_lines(&logical).into_iter().next()?;
        Some(parse_calendar_date_time(&line))
    }

    /// Whether this `VEVENT` carries a recurrence rule or an explicit recurrence date —
    /// i.e. whether it is a series that *has* instances to override.
    pub(super) fn is_recurring(&self, doc: &Document) -> bool {
        self.property(doc, "RRULE").is_some() || self.property(doc, "RDATE").is_some()
    }
}

/// The `VEVENT`s of one calendar object resource, plus where a new one is spliced in.
#[derive(Debug)]
pub(super) struct Resource {
    /// Every `VEVENT`, in document order.
    vevents: Vec<Vevent>,
    /// The `END:VCALENDAR` line — a new override `VEVENT` goes before it.
    end_vcalendar: Option<usize>,
}

impl Resource {
    /// The series master: the `VEVENT` with no `RECURRENCE-ID` (RFC 5545 §3.8.4.4).
    ///
    /// # Errors
    ///
    /// Returns [`IcalError`] when the resource carries only override
    /// instances, so there is no master to patch.
    pub(super) fn master(&self, doc: &Document) -> Result<&Vevent, IcalError> {
        self.vevents
            .iter()
            .find(|vevent| vevent.property(doc, "RECURRENCE-ID").is_none())
            .ok_or_else(|| {
                IcalError::new(
                    "resource has no master VEVENT (it carries only RECURRENCE-ID overrides)",
                )
            })
    }

    /// The existing override for the occurrence originally starting at `recurrence_id`,
    /// if the resource already carries one. A sibling whose `RECURRENCE-ID` will not
    /// parse is skipped rather than fatal — the same tolerance the read path applies.
    pub(super) fn override_for(
        &self,
        doc: &Document,
        recurrence_id: &CalendarDateTime,
    ) -> Option<&Vevent> {
        self.vevents.iter().find(|vevent| {
            vevent
                .date_time(doc, "RECURRENCE-ID")
                .and_then(Result::ok)
                .is_some_and(|existing| existing == *recurrence_id)
        })
    }

    /// Where to splice a newly-split override `VEVENT`: immediately before
    /// `END:VCALENDAR`.
    ///
    /// # Errors
    ///
    /// Returns [`IcalError`] if the resource has no `END:VCALENDAR` — a
    /// truncated document we must not "repair" by guessing.
    /// Every `RECURRENCE-ID` override in the resource.
    ///
    /// Removing the series' rule has to take these with it: an override whose master no
    /// longer recurs is not inert, it is an **extra instance** — the reader folds it into
    /// the event's override map either way, and the expander materializes an override on a
    /// non-rule instant as an added occurrence (RDATE-like). Left behind, "does not repeat"
    /// would still draw every occurrence the user had ever edited.
    pub(super) fn overrides<'a>(
        &'a self,
        doc: &'a Document,
    ) -> impl Iterator<Item = &'a Vevent> + 'a {
        self.vevents
            .iter()
            .filter(move |vevent| vevent.property(doc, "RECURRENCE-ID").is_some())
    }

    pub(super) fn splice_point(&self) -> Result<usize, IcalError> {
        self.end_vcalendar
            .ok_or_else(|| IcalError::new("resource has no END:VCALENDAR to splice into"))
    }
}

/// Scans `doc` for its `VEVENT`s, tracking component nesting so a `VALARM`'s
/// properties are never mistaken for the event's own.
///
/// # Errors
///
/// Returns [`IcalError`] if the document has no `VEVENT` at all.
pub(super) fn scan(doc: &Document) -> Result<Resource, IcalError> {
    let mut stack: Vec<String> = Vec::new();
    let mut vevents = Vec::new();
    let mut end_vcalendar = None;
    let mut current: Option<Vevent> = None;

    for index in 0..doc.len() {
        let logical = doc.logical(index);
        let name = property_name(&logical).to_ascii_uppercase();
        match name.as_str() {
            "BEGIN" => {
                let component = component_name(&logical);
                if component == "VEVENT" && current.is_none() {
                    current = Some(Vevent {
                        begin: index,
                        end: index,
                        own: Vec::new(),
                        anchor: index,
                    });
                } else if let Some(vevent) = current.as_mut()
                    && vevent.anchor == vevent.begin
                {
                    // The first sub-component inside this VEVENT — new properties go
                    // before it, not after the alarms.
                    vevent.anchor = index;
                }
                stack.push(component);
            }
            "END" => {
                let component = component_name(&logical);
                stack.pop();
                if component == "VCALENDAR" {
                    end_vcalendar = Some(index);
                }
                if component == "VEVENT"
                    && let Some(mut vevent) = current.take()
                {
                    vevent.end = index;
                    if vevent.anchor == vevent.begin {
                        vevent.anchor = index; // no sub-components: insert before END
                    }
                    vevents.push(vevent);
                }
            }
            _ => {
                // A property line, but only this VEVENT's *own* if the innermost open
                // component is the VEVENT itself.
                if let Some(vevent) = current.as_mut()
                    && stack.last().is_some_and(|component| component == "VEVENT")
                {
                    vevent.own.push(index);
                }
            }
        }
    }

    if vevents.is_empty() {
        return Err(IcalError::new("resource has no VEVENT to patch"));
    }
    Ok(Resource {
        vevents,
        end_vcalendar,
    })
}

/// The property name of a logical content line — everything before its first `;` or
/// `:` (RFC 5545 §3.1). Not uppercased; compare case-insensitively.
pub(super) fn property_name(logical: &str) -> &str {
    let end = logical.find([';', ':']).unwrap_or(logical.len());
    &logical[..end]
}

/// The value of a `BEGIN`/`END` line — the component it opens or closes, uppercased.
fn component_name(logical: &str) -> String {
    split_once_unquoted(logical, ':')
        .map(|(_, value)| value.trim().to_ascii_uppercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A master with a VALARM (whose SUMMARY must never be mistaken for the event's)
    /// plus a RECURRENCE-ID override.
    const RESOURCE: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:w@x\r\nDTSTART;TZID=Europe/Amsterdam:20260105T093000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO\r\nSUMMARY:Standup\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nSUMMARY:Alarm title\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:w@x\r\nRECURRENCE-ID;TZID=Europe/Amsterdam:20260126T093000\r\nDTSTART;TZID=Europe/Amsterdam:20260126T140000\r\nSUMMARY:Moved\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    fn zoned(local: &str) -> CalendarDateTime {
        CalendarDateTime::Zoned {
            local: local.parse().unwrap(),
            zone: engine_core::time::TimeZoneId::iana("Europe/Amsterdam").unwrap(),
        }
    }

    #[test]
    fn the_master_is_the_vevent_without_a_recurrence_id() {
        let doc = Document::parse(RESOURCE);
        let resource = scan(&doc).unwrap();
        let master = resource.master(&doc).unwrap();
        assert!(master.is_recurring(&doc));
        assert_eq!(
            doc.logical(master.property(&doc, "SUMMARY").unwrap()),
            "SUMMARY:Standup"
        );
    }

    #[test]
    fn a_valarms_properties_are_not_the_events_own() {
        // The trap: the VALARM's SUMMARY sits inside the master's line range. If `own`
        // included it, `property("SUMMARY")` could retitle the alarm instead.
        let doc = Document::parse(RESOURCE);
        let resource = scan(&doc).unwrap();
        let master = resource.master(&doc).unwrap();
        let summaries: Vec<String> = master
            .own
            .iter()
            .map(|&group| doc.logical(group))
            .filter(|line| property_name(line) == "SUMMARY")
            .collect();
        assert_eq!(summaries, vec!["SUMMARY:Standup".to_owned()]);
        // And a new property lands before the VALARM, not after it (RFC 5545 §3.6.1).
        assert_eq!(doc.logical(master.anchor), "BEGIN:VALARM");
    }

    #[test]
    fn an_existing_override_is_found_by_its_recurrence_id() {
        let doc = Document::parse(RESOURCE);
        let resource = scan(&doc).unwrap();
        let found = resource
            .override_for(&doc, &zoned("2026-01-26T09:30:00"))
            .unwrap();
        assert_eq!(
            doc.logical(found.property(&doc, "SUMMARY").unwrap()),
            "SUMMARY:Moved"
        );
        // A different occurrence has no override yet.
        assert!(
            resource
                .override_for(&doc, &zoned("2026-02-02T09:30:00"))
                .is_none()
        );
    }

    #[test]
    fn a_vevent_without_sub_components_anchors_on_its_end() {
        let doc = Document::parse(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nSUMMARY:S\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let resource = scan(&doc).unwrap();
        let master = resource.master(&doc).unwrap();
        assert_eq!(doc.logical(master.anchor), "END:VEVENT");
        assert_eq!(resource.splice_point().unwrap(), 5);
        assert!(!master.is_recurring(&doc));
    }

    #[test]
    fn a_resource_with_only_overrides_has_no_master() {
        let doc = Document::parse(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x@y\r\nRECURRENCE-ID:20260126T093000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let resource = scan(&doc).unwrap();
        assert!(resource.master(&doc).is_err());
    }

    #[test]
    fn a_document_without_a_vevent_is_an_error() {
        let doc = Document::parse("BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n");
        assert!(scan(&doc).is_err());
    }
}
