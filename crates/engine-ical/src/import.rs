//! Preparing a calendar object to be stored as it was, telling nobody: the iCalendar half of
//! calendar import.
//!
//! Importing an event stores the object the source held. On a server that schedules (RFC 6638),
//! storing a meeting is also what makes the server send its invitations, so the stored object
//! has to say that nobody is to be told: every `ORGANIZER` and `ATTENDEE` gets
//! `SCHEDULE-AGENT=CLIENT` (RFC 6638 §7.1), which tells the server the client does the
//! scheduling for that party, and the client does none. Overrides are components of their own,
//! so each one's parties get it too.
//!
//! The parameter stays in the stored object, so the server does not schedule that meeting
//! later either, when it is edited in a client that keeps the parameter. That is the price of an
//! import that tells nobody, and a host says so to its users.
//!
//! Every other byte is kept: the edit is line surgery on the source text ([`Document`]), never a
//! re-serialization of the projection.

use engine_core::{ids::Uid, raw::RawIcal};

use crate::{
    Document, Edit, Edits, IcalError, LineEdit, component::parse_components, resource_components,
    split_once_unquoted, split_unquoted, vevent_uid,
};

/// A calendar object ready to be stored: its `UID`, and its text with every scheduling party
/// left to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImportObject {
    /// The `UID` every event in the object shares (RFC 4791 §4.1).
    pub uid: Uid,
    /// The text to store.
    pub ical: RawIcal,
}

/// Prepares one calendar object resource for import.
///
/// - Every `ORGANIZER` and `ATTENDEE` of every event, overrides included, carries
///   `SCHEDULE-AGENT=CLIENT`; a `SCHEDULE-AGENT` it had is replaced, and a `SCHEDULE-FORCE-SEND` (a
///   request to the server, not data) is dropped. An alarm's `ATTENDEE` names who the reminder
///   mails and is not a scheduling party, so it is left alone.
/// - A `METHOD` is dropped: a stored object carries none (RFC 4791 §4.1), though an exported `.ics`
///   file often does.
/// - Every other line is kept byte for byte, folding and terminators included.
///
/// # Errors
///
/// [`IcalError`] when the text holds no event, an event without a `UID`, or events with more
/// than one `UID`: a resource is one event with its overrides, so a file of several events is
/// split before it is imported.
pub fn import_object(text: &str) -> Result<ImportObject, IcalError> {
    let roots = parse_components(text);
    let (components, _) = resource_components(&roots)?;
    let uid = vevent_uid(components[0])?;
    let events = crate::collect_vevents(&roots);
    if events.len() != components.len() {
        return Err(IcalError::new(
            "the object holds events with more than one UID",
        ));
    }
    let document = Document::parse(text);
    let mut edits = Edits::new();
    let mut open: Vec<String> = Vec::new();
    for index in 0..document.len() {
        let line = document.logical(index);
        let name = property_name(&line);
        match name.as_str() {
            "BEGIN" => open.push(value_of(&line)),
            "END" => {
                open.pop();
            }
            "METHOD" if open.len() == 1 => {
                edits.insert(
                    index,
                    Edit {
                        before: String::new(),
                        line: LineEdit::Remove,
                    },
                );
            }
            "ORGANIZER" | "ATTENDEE"
                if open.last().is_some_and(|component| component != "VALARM") =>
            {
                if let Some(rewritten) = left_to_the_client(&line) {
                    edits.insert(index, Edit::replace(rewritten));
                }
            }
            _ => {}
        }
    }
    Ok(ImportObject {
        uid,
        ical: RawIcal::new(document.render(&edits)),
    })
}

/// `line` with `SCHEDULE-AGENT=CLIENT` as its only scheduling-agent parameter and no
/// `SCHEDULE-FORCE-SEND`, or `None` when it already reads that way.
fn left_to_the_client(line: &str) -> Option<String> {
    let (head, value) = split_once_unquoted(line, ':')?;
    let mut parameters = split_unquoted(head, ';').into_iter();
    let property = parameters.next()?;
    let mut kept = Vec::new();
    let mut client = false;
    let mut changed = false;
    for parameter in parameters {
        let name = parameter
            .split_once('=')
            .map_or(parameter, |(name, _)| name);
        if name.eq_ignore_ascii_case("SCHEDULE-AGENT") {
            if parameter[name.len()..].eq_ignore_ascii_case("=CLIENT") && !client {
                client = true;
                kept.push(parameter);
            } else {
                changed = true;
            }
        } else if name.eq_ignore_ascii_case("SCHEDULE-FORCE-SEND") {
            changed = true;
        } else {
            kept.push(parameter);
        }
    }
    if client && !changed {
        return None;
    }
    let mut rewritten = property.to_owned();
    for parameter in kept {
        rewritten.push(';');
        rewritten.push_str(parameter);
    }
    if !client {
        rewritten.push_str(";SCHEDULE-AGENT=CLIENT");
    }
    rewritten.push(':');
    rewritten.push_str(value);
    Some(rewritten)
}

/// The property name of a logical line, upper-cased.
fn property_name(line: &str) -> String {
    let end = line.find([';', ':']).unwrap_or(line.len());
    line[..end].trim().to_ascii_uppercase()
}

/// The value of a logical line (`BEGIN:VEVENT` → `VEVENT`), upper-cased.
fn value_of(line: &str) -> String {
    split_once_unquoted(line, ':')
        .map_or("", |(_, value)| value)
        .trim()
        .to_ascii_uppercase()
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
