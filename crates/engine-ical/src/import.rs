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
//! **It fails closed.** A party this module cannot read for certain (a line whose quotes do not
//! balance, components that do not nest, a party outside every component) is a refused object,
//! never one stored as it was: a server that reads such a line more leniently than this module
//! would schedule it. What the rewrite produced is parsed again and checked before it is
//! answered.
//!
//! Which component a line belongs to is read off the one component tree the read path builds
//! (`component::parse_tree`), by each line's index. Every other byte is kept: the edit is line
//! surgery on the source text ([`Document`]), never a re-serialization of the projection.

use std::collections::BTreeSet;

use engine_core::{ids::Uid, raw::RawIcal};

use crate::{
    Document, Edit, Edits, IcalError, LineEdit,
    component::{Component, parse_tree},
    resource_components, split_once_unquoted, split_unquoted,
    unfold::{ContentLine, parse_param},
    vevent_uid,
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
/// - Every `ORGANIZER` and `ATTENDEE` of every event, overrides included (and a group-prefixed
///   `x.ATTENDEE` too), carries exactly one `SCHEDULE-AGENT`, `CLIENT`; a `SCHEDULE-FORCE-SEND` (a
///   request to the server, not data) is dropped. An alarm's `ATTENDEE` names who the reminder
///   mails and is not a scheduling party, so it is left alone.
/// - A `METHOD` is dropped: a stored object carries none (RFC 4791 §4.1), though an exported `.ics`
///   file often does.
/// - Every other line is kept byte for byte, folding and terminators included.
///
/// The text is UTF-8, as [`RawIcal`] is: a source in another charset is decoded first.
///
/// # Errors
///
/// [`IcalError`] when the object is not one event with its overrides (no event, an event without
/// a `UID`, several `UID`s, a task or journal beside it, an event nested below the calendar), when
/// its components do not nest, or when a party cannot be read and rewritten for certain.
pub fn import_object(text: &str) -> Result<ImportObject, IcalError> {
    let tree = parse_tree(text);
    if !tree.nests {
        return Err(IcalError::new("the object's components do not nest"));
    }
    let [calendar] = tree.roots.as_slice() else {
        return Err(IcalError::new("the object is not one VCALENDAR"));
    };
    let uid = one_event_uid(calendar)?;
    let document = Document::parse(text);
    let mut edits = Edits::new();
    for line in &calendar.properties {
        if line.name == "METHOD" {
            edits.insert(
                line.index,
                Edit {
                    before: String::new(),
                    line: LineEdit::Remove,
                },
            );
        }
    }
    let mut parties = BTreeSet::new();
    let mut alarm_parties = BTreeSet::new();
    for (component, line) in properties(calendar) {
        if !is_party(&line.name) {
            continue;
        }
        if component.name == "VALARM" {
            alarm_parties.insert(line.index);
            continue;
        }
        let logical = (line.index < document.len())
            .then(|| document.logical(line.index))
            .filter(|logical| is_party(&head_name(logical)))
            .ok_or_else(|| IcalError::new("a party line could not be located"))?;
        if let Some(rewritten) = left_to_the_client(&logical)? {
            edits.insert(line.index, Edit::replace(rewritten));
        }
        parties.insert(line.index);
    }
    // A party line the tree does not hold is one its reader could not parse: refuse it rather
    // than store it untouched for a server that reads it more leniently.
    for index in 0..document.len() {
        if is_party(&head_name(&document.logical(index)))
            && !parties.contains(&index)
            && !alarm_parties.contains(&index)
        {
            return Err(IcalError::new("a party line cannot be read"));
        }
    }
    let ical = document.render(&edits);
    verify(&ical, parties.len())?;
    Ok(ImportObject {
        uid,
        ical: RawIcal::new(ical),
    })
}

/// The `UID` of the event a stored calendar object holds, read without rewriting anything: what
/// a lookup compares.
///
/// # Errors
///
/// [`IcalError`] when the object holds no event, or its first event has no `UID`.
pub fn object_uid(text: &str) -> Result<Uid, IcalError> {
    let roots = parse_tree(text).roots;
    let (components, _) = resource_components(&roots)?;
    vevent_uid(components[0])
}

/// The one `UID` of the calendar's events, refusing anything that is not one event with its
/// overrides: every component carrying a `UID` (an alarm's aside, RFC 9074) must be an event
/// directly in the calendar, and all must share it.
fn one_event_uid(calendar: &Component) -> Result<Uid, IcalError> {
    let not_one = || IcalError::new("the object is not one event with its overrides");
    let mut uids = BTreeSet::new();
    for (depth, component) in components(calendar, 0) {
        let event = component.name == "VEVENT";
        let foreign = matches!(component.name.as_str(), "VTODO" | "VJOURNAL" | "VFREEBUSY");
        if foreign || (event && depth != 1) {
            return Err(not_one());
        }
        if event {
            uids.insert(vevent_uid(component)?);
        } else if component.name != "VALARM" && component.property("UID").is_some() {
            return Err(not_one());
        }
    }
    let mut uids = uids.into_iter();
    match (uids.next(), uids.next()) {
        (Some(uid), None) => Ok(uid),
        _ => Err(not_one()),
    }
}

/// Every component below `root` with its depth (the root's children are at depth 1).
fn components(root: &Component, depth: usize) -> Vec<(usize, &Component)> {
    let mut found = Vec::new();
    for child in &root.children {
        found.push((depth + 1, child));
        found.extend(components(child, depth + 1));
    }
    found
}

/// Every property below `root`, with the component that holds it.
fn properties(root: &Component) -> Vec<(&Component, &ContentLine)> {
    components(root, 0)
        .into_iter()
        .flat_map(|(_, component)| {
            component
                .properties
                .iter()
                .map(move |line| (component, line))
        })
        .collect()
}

/// Whether a property name is a scheduling party, a group prefix (`x.ATTENDEE`) included.
fn is_party(name: &str) -> bool {
    let bare = name.rsplit('.').next().unwrap_or(name);
    bare.eq_ignore_ascii_case("ATTENDEE") || bare.eq_ignore_ascii_case("ORGANIZER")
}

/// The name a logical line starts with, before any parameter or value, whatever its quoting.
fn head_name(line: &str) -> String {
    let end = line.find([';', ':']).unwrap_or(line.len());
    line[..end].trim().to_ascii_uppercase()
}

/// `line` with one `SCHEDULE-AGENT=CLIENT` and no `SCHEDULE-FORCE-SEND`, `None` when it already
/// reads that way. Parameter names are compared as the reader compares them, trimmed and
/// case-folded (`unfold::parse_param`), so no spelling survives beside the one added.
///
/// # Errors
///
/// [`IcalError`] when the line does not split into name, parameters and value.
fn left_to_the_client(line: &str) -> Result<Option<String>, IcalError> {
    let unreadable = || IcalError::new("a party line cannot be read");
    let (head, value) = split_once_unquoted(line, ':').ok_or_else(unreadable)?;
    let mut parameters = split_unquoted(head, ';').into_iter();
    let property = parameters.next().ok_or_else(unreadable)?;
    let mut kept = Vec::new();
    let mut agents = Vec::new();
    let mut forced = false;
    for parameter in parameters {
        let (name, parsed) = parse_param(parameter).ok_or_else(unreadable)?;
        match name.as_str() {
            "SCHEDULE-AGENT" => agents.push(parsed),
            "SCHEDULE-FORCE-SEND" => forced = true,
            _ => kept.push(parameter),
        }
    }
    if !forced && matches!(agents.as_slice(), [only] if only.eq_ignore_ascii_case("CLIENT")) {
        return Ok(None);
    }
    let mut rewritten = property.to_owned();
    for parameter in kept {
        rewritten.push(';');
        rewritten.push_str(parameter);
    }
    rewritten.push_str(";SCHEDULE-AGENT=CLIENT:");
    rewritten.push_str(value);
    Ok(Some(rewritten))
}

/// Parses the rewritten text again and checks what is to be stored: it nests, and each of its
/// `parties` outside an alarm carries one `SCHEDULE-AGENT`, `CLIENT`, and no forced send.
fn verify(ical: &str, parties: usize) -> Result<(), IcalError> {
    let tree = parse_tree(ical);
    let held: Vec<(&Component, &ContentLine)> = tree
        .roots
        .iter()
        .flat_map(properties)
        .filter(|(component, line)| component.name != "VALARM" && is_party(&line.name))
        .collect();
    let left = held
        .iter()
        .filter(|(_, line)| {
            let agents: Vec<&str> = line
                .params
                .iter()
                .filter(|(name, _)| name == "SCHEDULE-AGENT")
                .map(|(_, value)| value.as_str())
                .collect();
            matches!(agents.as_slice(), [only] if only.eq_ignore_ascii_case("CLIENT"))
                && line.param("SCHEDULE-FORCE-SEND").is_none()
        })
        .count();
    if tree.nests && left == parties && held.len() == parties {
        Ok(())
    } else {
        Err(IcalError::new(
            "the rewritten object does not leave every party to the client",
        ))
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
