//! Rendering the neutral [`EventEdit`] as the JSCalendar PatchObject a
//! `CalendarEvent/set` `update` carries.
//!
//! Its own file rather than part of [`calendar_write`](super::calendar_write), which holds
//! the three round trips (create, update, destroy); this holds the one thing that decides
//! whether an update means what the caller asked.
//!
//! # A pointer may only address what already exists
//!
//! RFC 8620 §5.3 requires every part of a pointer *before* the last to exist on the server
//! already; a pointer into an absent map is an `invalidPatch`, and the **whole** update is
//! rejected with it. That is not a corner case here — it decides the shape of the two edits
//! this transport expresses through a map:
//!
//! - **A location** an event does not have yet goes in as a whole `locations/<id>` object, not as a
//!   `locations/<id>/name` pointer.
//! - **The first edit of an occurrence** assigns the whole `recurrenceOverrides` map, with that
//!   occurrence as its sole entry. Only once the series is overridden *somewhere* can a later edit
//!   address one entry through `recurrenceOverrides/<start>/…`.
//!
//! Measured on Stalwart: with no `recurrenceOverrides` on the event, every pointer into it —
//! `…/<start>`, `…/<start>/title`, `…/<start>/excluded` — comes back `invalidProperties`,
//! while assigning the map succeeds. With the map present, all three are accepted.

use engine_core::{calendar::Event, scheduling::addresses_match, time::CalendarDateTime};
use engine_provider::{EventEdit, InviteeRole, PatchTarget, RecurrenceEdit, TextEdit};
use serde_json::{Map, Value, json};

use crate::{
    calendar::participant_address,
    calendar_rule::render_rule,
    calendar_write::{NEW_LOCATION_ID, duration, escape_pointer, local_date_time},
    error::JmapError,
};

/// Renders an [`EventEdit`] as a JSCalendar PatchObject (RFC 8620 §5.3): a flat map of
/// JSON-pointer → new value, where `null` **removes** the property.
///
/// An [`Instance`](PatchTarget::Instance) target names the occurrence by its **original**
/// start under `recurrenceOverrides` (RFC 8984 §4.3.3), so overriding one occurrence is the
/// same set of edits addressed through that map, and the **server** materializes the
/// override from the series itself. That is CalDAV's whole `RECURRENCE-ID`-splitting chore,
/// done server-side.
///
/// Which of the two override shapes it takes is [`is_overridden`]'s call — see the module
/// docs for why there are two.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the recurrence id is not in the series' own time form,
/// if a recurrence edit rides an occurrence target, if a new start would change the event's
/// time form, or if a new end precedes the start.
pub(crate) fn patch_to_json(
    base: &Event,
    edit: &EventEdit,
    max_participants: Option<usize>,
) -> Result<Map<String, Value>, JmapError> {
    // The recurrence id is the occurrence's identity within the series, so check it before
    // anything else: an id in the wrong form names no occurrence at all, and saying so beats
    // reporting whatever a property downstream trips over first.
    if let PatchTarget::Instance(occurrence) = &edit.target
        && !base.start.has_same_form(&occurrence.start)
    {
        return Err(JmapError::protocol(format!(
            "the occurrence's recurrence id must be in the series' own time form ({}); \
             naming it in another form overrides no instance",
            base.start.form_name(),
        )));
    }

    let patch = &edit.patch;
    let mut out = Map::new();

    if let Some(invitees) = patch.invitee_edit() {
        if !matches!(edit.target, PatchTarget::Series) {
            return Err(JmapError::protocol(
                "invitees belong to the whole meeting, not one occurrence",
            ));
        }
        out.insert(
            "participants".to_owned(),
            Value::Object(merge_participants(base, invitees, max_participants)?),
        );
    }

    if let Some(recurrence) = patch.recurrence_edit() {
        // A recurrence edit is series-level by definition, so it cannot go under the
        // `recurrenceOverrides` entry everything else on an Instance target does — writing
        // a rule *inside* one occurrence's override would mean nothing.
        if !matches!(edit.target, PatchTarget::Series) {
            return Err(JmapError::protocol(
                "a recurrence edit targets the series, never one occurrence; an occurrence \
                 has no rule of its own",
            ));
        }
        // `null` removes the property (RFC 8620 §5.3), which is how a series becomes a
        // single event. The singular name is Stalwart's and the only one it takes on a
        // write — see `jmap.md` → "Recurrence property naming".
        out.insert(
            "recurrenceRule".to_owned(),
            match recurrence {
                RecurrenceEdit::Set(r) => render_rule(&r.rule)?,
                RecurrenceEdit::Clear => Value::Null,
            },
        );
    }

    // The properties the edit changes, named **relative to what they patch** — the event
    // itself on a Series target, one occurrence's override on an Instance one. Which
    // prefix they end up under is the assembly step below.
    let mut properties = Map::new();

    if let Some(summary) = patch.summary_edit() {
        properties.insert("title".to_owned(), json!(summary));
    }
    if let Some(description) = patch.description_edit() {
        properties.insert("description".to_owned(), text_edit(description));
    }
    if let Some(location) = patch.location_edit() {
        for (pointer, value) in location_edit(base, location) {
            properties.insert(pointer, value);
        }
    }

    // A move keeps the event's form: JSCalendar states the wall clock in `start` and the
    // zone separately in `timeZone`, so writing the instant here would move the event for
    // every reader in another zone. `timeZone` is deliberately never patched — this is a
    // move, not a conversion.
    if let Some(start) = patch.start_edit() {
        if !base.start.has_same_form(start) {
            return Err(JmapError::protocol(format!(
                "the new start would change the event's time form ({}), which a move must \
                 never do silently; supply it in the event's own form",
                base.start.form_name(),
            )));
        }
        properties.insert("start".to_owned(), json!(local_date_time(start)?));
    }
    // JSCalendar has no end: it states a `duration` from the start. So an end edit is
    // resolved against the start the event will *have* — the new one if this patch moves it,
    // else the one it already has — which is also what catches a caller who drags the start
    // past the existing end without resizing.
    if let Some(end) = patch.end_edit() {
        let effective_start = patch.start_edit().unwrap_or(&base.start);
        properties.insert("duration".to_owned(), duration(effective_start, end)?);
    }

    match &edit.target {
        PatchTarget::Series => out.append(&mut properties),
        PatchTarget::Instance(occurrence) if !properties.is_empty() => {
            let start = local_date_time(&occurrence.start)?;
            if is_overridden(base) {
                let key = escape_pointer(&start);
                for (name, value) in properties {
                    out.insert(format!("recurrenceOverrides/{key}/{name}"), value);
                }
            } else {
                // The map itself has to be assigned, and its entry keeps the pointer-keyed
                // names: a `recurrenceOverrides` value *is* a PatchObject (RFC 8984 §1.4.11),
                // so `locations/<id>/name` inside it addresses the master's location exactly
                // as it does at the top level.
                out.insert(
                    "recurrenceOverrides".to_owned(),
                    json!({ start: Value::Object(properties) }),
                );
            }
        }
        PatchTarget::Instance(_) => {}
    }
    Ok(out)
}

fn merge_participants(
    base: &Event,
    edit: &engine_provider::InviteePatch,
    max_participants: Option<usize>,
) -> Result<Map<String, Value>, JmapError> {
    let raw = base.raw_jscalendar.as_ref().ok_or_else(|| {
        JmapError::protocol("event has no preserved JSCalendar; re-sync it before editing invitees")
    })?;
    let value: Value = serde_json::from_str(raw.as_str()).map_err(|error| {
        JmapError::protocol(format!("preserved JSCalendar is not JSON: {error}"))
    })?;
    let mut participants = value
        .get("participants")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let organiser = value
        .get("organizerCalendarAddress")
        .and_then(Value::as_str);

    for address in edit.removals() {
        let owner = organiser.is_some_and(|value| addresses_match(value, address.as_str()))
            || participants.iter().any(|(_, participant)| {
                participant_address(participant)
                    .is_some_and(|value| addresses_match(value, address.as_str()))
                    && has_role(participant, "owner")
            });
        if owner {
            return Err(JmapError::protocol(
                "the organiser cannot be removed from their own meeting",
            ));
        }
        participants.retain(|_, participant| {
            !participant_address(participant)
                .is_some_and(|value| addresses_match(value, address.as_str()))
        });
    }

    for invitee in edit.upserts() {
        let address = invitee.identity().address().as_str();
        if organiser.is_some_and(|value| addresses_match(value, address)) {
            return Err(JmapError::protocol(
                "the organiser cannot be edited as an invitee",
            ));
        }
        let existing = participants.iter().find_map(|(id, participant)| {
            participant_address(participant)
                .is_some_and(|value| addresses_match(value, address))
                .then(|| id.clone())
        });
        if existing
            .as_ref()
            .and_then(|id| participants.get(id))
            .is_some_and(|participant| has_role(participant, "owner"))
        {
            return Err(JmapError::protocol(
                "the organiser cannot be edited as an invitee",
            ));
        }
        let id = existing.unwrap_or_else(|| next_participant_id(&participants));
        let participant = participants.entry(id).or_insert_with(|| {
            json!({
                "@type": "Participant",
                "calendarAddress": format!("mailto:{address}"),
                "participationStatus": "needs-action",
                "expectReply": true,
                "roles": {}
            })
        });
        let object = participant
            .as_object_mut()
            .ok_or_else(|| JmapError::protocol("JSCalendar participant is not an object"))?;
        object.insert(
            "calendarAddress".to_owned(),
            json!(format!("mailto:{address}")),
        );
        object.remove("sendTo");
        if let Some(name) = invitee.identity().name() {
            object.insert("name".to_owned(), json!(name));
        }
        let roles = object
            .entry("roles")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| JmapError::protocol("JSCalendar participant roles are not an object"))?;
        roles.remove("attendee");
        roles.remove("required");
        roles.remove("optional");
        match invitee.role() {
            InviteeRole::Required => {
                roles.insert("required".to_owned(), json!(true));
            }
            InviteeRole::Optional => {
                roles.insert("optional".to_owned(), json!(true));
            }
        }
    }
    if let Some(limit) = max_participants
        && participants.len() > limit
    {
        return Err(JmapError::protocol(format!(
            "provider accepts at most {limit} participants, got {}",
            participants.len()
        )));
    }
    Ok(participants)
}

fn has_role(participant: &Value, role: &str) -> bool {
    participant
        .get("roles")
        .and_then(Value::as_object)
        .and_then(|roles| roles.get(role))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn next_participant_id(participants: &Map<String, Value>) -> String {
    (1..=participants.len() + 1)
        .map(|index| format!("invitee-{index}"))
        .find(|id| !participants.contains_key(id))
        .expect("one of n + 1 numeric ids is absent from a map of n participants")
}

/// The `update` that removes one occurrence from a series.
///
/// JSCalendar has no verb for it, because an occurrence is not an object: the occurrence is
/// marked `excluded` in the series' `recurrenceOverrides` (RFC 8984 §4.3.3) — the same map an
/// edit of one occurrence writes into, taking the same two shapes for the same reason (see
/// the module docs).
///
/// An `excluded` override may carry nothing else, so an entry the user had edited is
/// **replaced** rather than merged: the occurrence is gone, and what they had changed about
/// it has nothing left to describe.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the occurrence is not named in the series' own time
/// form, which would exclude no instance of it.
pub(crate) fn exclusion(
    base: &Event,
    occurrence: &CalendarDateTime,
) -> Result<Map<String, Value>, JmapError> {
    if !base.start.has_same_form(occurrence) {
        return Err(JmapError::protocol(format!(
            "the occurrence must be named in the series' own time form ({}); naming it in \
             another form excludes no instance",
            base.start.form_name(),
        )));
    }
    let start = local_date_time(occurrence)?;
    let mut out = Map::new();
    if is_overridden(base) {
        out.insert(
            format!("recurrenceOverrides/{}", escape_pointer(&start)),
            json!({ "excluded": true }),
        );
    } else {
        out.insert(
            "recurrenceOverrides".to_owned(),
            json!({ start: { "excluded": true } }),
        );
    }
    Ok(out)
}

/// Whether the series already carries a `recurrenceOverrides` map — which decides whether a
/// pointer into it addresses anything (see the module docs).
///
/// Read off the base rather than fetched: it is what the caller read, and a fetch would only
/// narrow the window rather than close it. A base stale in exactly this respect — the series
/// gained its *first* override elsewhere between the read and this write — makes the
/// map-assigning shape overwrite that one. JMAP carries no per-object revision to refuse it
/// on ([`WriteGuard::Absent`](engine_provider::WriteGuard::Absent)), so this is stated, not
/// guarded.
fn is_overridden(base: &Event) -> bool {
    base.recurrence
        .as_ref()
        .is_some_and(|recurrence| !recurrence.overrides.is_empty())
}

/// A text property's new value, or `null` to remove it (RFC 8620 §5.3).
fn text_edit(edit: &TextEdit) -> Value {
    match edit {
        TextEdit::Set(text) => json!(text),
        TextEdit::Clear => Value::Null,
    }
}

/// The pointers a location edit writes.
///
/// JSCalendar has no scalar location: `locations` is a **map** of id → `Location` object
/// (RFC 8984 §4.2.5). So renaming "the location" means renaming the one already on the
/// event, and its id lives only in the preserved `raw_jscalendar` — which is why the read
/// path keeps it. Patching `locations/<id>/name` leaves that location's coordinates, its
/// `locationTypes` and any other location the event has exactly as they were; replacing the
/// whole map would discard them.
///
/// An event with no location yet gets one at a fresh id; clearing removes the whole map,
/// which is the only honest reading of "this event has no location".
fn location_edit(base: &Event, edit: &TextEdit) -> Vec<(String, Value)> {
    match edit {
        TextEdit::Clear => vec![("locations".to_owned(), Value::Null)],
        TextEdit::Set(name) => {
            match existing_location_id(base) {
                Some(id) => vec![(
                    format!("locations/{}/name", escape_pointer(&id)),
                    json!(name),
                )],
                // No location to rename: add one. The whole object goes in at once, because
                // a pointer into a map entry the server does not have would be an
                // `invalidPatch`.
                None => vec![(
                    format!("locations/{NEW_LOCATION_ID}"),
                    json!({ "@type": "Location", "name": name }),
                )],
            }
        }
    }
}

/// The id of the first location on the event, read out of the preserved JSCalendar payload.
///
/// The projection ([`Location`](engine_core::calendar::Location)) does not carry the
/// JSCalendar map id — it has no use for it on the read path — so the raw is the only place
/// it survives. An event with no raw (never synced from this transport) reports none, and a
/// location edit then adds one rather than failing.
fn existing_location_id(base: &Event) -> Option<String> {
    let raw = base.raw_jscalendar.as_ref()?;
    let value: Value = serde_json::from_str(raw.as_str()).ok()?;
    let locations = value.get("locations")?.as_object()?;
    locations.keys().next().cloned()
}
