//! Calendar writes via `CalendarEvent/set` (RFC 8620 §5.3, JSCalendar RFC 8984): create,
//! patch, destroy. The fourth verb, the RSVP, is [`calendar_rsvp`](crate::calendar_rsvp) —
//! it alone has to resolve a participant id out of the preserved JSCalendar.
//!
//! This is how JMAP renders the neutral write verbs (`engine-provider`), and it is the
//! mirror image of CalDAV's. CalDAV's write verb is `PUT` — replace the whole resource — so
//! the *client* has to do the surgery (`provider_caldav`'s structural iCalendar patcher).
//! JMAP's `update` **is already a patch**: a PatchObject keyed by JSON pointer, which the
//! **server** merges into the stored object. We probed this against the harness rather than
//! assuming it: an `update` of `title` alone left `timeZone`, `duration`, `uid` and `start`
//! untouched. So this module never rewrites a document — it translates intent into pointers,
//! and there is no JSCalendar serializer to keep in step with the parser.
//!
//! The `raw_jscalendar` preserved on read ([`calendar`](crate::calendar)) is therefore not
//! the *target* of the surgery here, as `raw_ical` is on CalDAV, but it is still consulted:
//! a location edit needs the id of the location already on the event, and that id lives
//! only in the provider-native payload.
//!
//! # There is no lost-update guard on this transport
//!
//! [`WriteGuard::Absent`](engine_provider::WriteGuard::Absent), and deliberately so. Two
//! independent reasons, both established rather than assumed:
//!
//! 1. **A `CalendarEvent` carries no per-object revision.** No `ETag`, no `changeKey` —
//!    `RevisionTokens` is empty for every JMAP object by construction. There is simply nothing to
//!    guard *this event* with.
//! 2. **`ifInState` is the wrong instrument, not merely a broken one.** RFC 8620 §5.3 scopes it to
//!    the account's whole `CalendarEvent` **type state**, not to the object: on a spec-compliant
//!    server, guarding an edit of *my* event with it means the write is rejected because somebody
//!    added an *unrelated* meeting since my last sync. That is not lost-update protection, it is a
//!    spurious failure — and the value would have to be the sync cursor, which is a property of the
//!    sync, not of the event being written.
//!
//! Stalwart's own handling of it changed under us, and neither state changes the above.
//! v0.16.11 through v0.16.13 parsed `ifInState` and never compared it (a stale-state `/set`
//! was applied and returned a fresh `newState`, where RFC 8620 §5.3 requires a
//! `stateMismatch`); v0.16.14 fixed that, and the harness pins v0.16.15 — so a
//! stale-but-well-formed token is refused with `stateMismatch` and the write does not land on
//! the server our live tests meet. Which only sharpens reason 2: the probe's state had moved
//! because of an edit to a *different* property of a *different* event, exactly the spurious
//! rejection a per-event guard must not have.
//!
//! So sending `ifInState` would buy nothing on the server we run against and would cause
//! spurious rejections on one that behaved. We send none, and say so through the capability
//! — the one thing that must not happen is a write API that *looks* like it gives optimistic
//! concurrency everywhere when here it gives none (`jmap.md`).

use engine_core::{calendar::Event, ids::EventId, time::CalendarDateTime, version::RevisionTokens};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventWriteReceipt, PatchTarget, TextEdit,
};
use serde_json::{Map, Value, json};

use crate::{
    error::JmapError,
    executor::Executor,
    request::{Request, capability},
};

/// The creation id for the single object a create posts. RFC 8620 §5.3 scopes it to the
/// call, so a fixed one is unambiguous — the server echoes it back in `created`.
const CREATION_ID: &str = "new";

/// The JSCalendar id given to a location an event does not have yet. Only ever used when
/// the event's `locations` map is empty, so it cannot collide with one the server assigned.
const NEW_LOCATION_ID: &str = "1";

/// Whether a `CalendarEvent/set` asks the server to send the iTIP messages the change
/// implies (`sendSchedulingMessages`). Always `true` on these three verbs, and the constant
/// exists so that is one decision rather than three.
///
/// **The argument defaults to `false`**, so omitting it is not "leave it to the server" — it
/// is "tell nobody". Cancelling a meeting you organize would store the deletion and leave
/// every attendee holding a meeting that is not happening; moving one would leave them at
/// the old time. The neutral write verbs carry no notify control for a caller to state
/// instead ([`EventDraft`] cannot even name a participant), and the transports that do
/// schedule — CalDAV's RFC 6638 auto-schedule, and Graph — do it unconditionally. So the
/// engine's answer to "does a calendar write reach its participants?" stays the same
/// whichever transport is under it.
///
/// The RSVP is the exception and takes the caller's choice
/// ([`calendar_rsvp`](crate::calendar_rsvp)), because there the neutral verb *does* carry
/// one.
const SCHEDULE: bool = true;

/// Creates an event: one `CalendarEvent/set` `create` of a JSCalendar object.
///
/// The **server** assigns the id, so the receipt is the only place the caller learns it. The
/// caller-minted `UID` travels with the object, which is what lets a retried create be
/// recognized as the same event.
///
/// # Errors
///
/// Returns [`JmapError`] on a transport/method failure, or [`JmapError::Set`] when the
/// server rejects the object with a `SetError` (or silently drops it — treated as a
/// `notFound` conflict, never a false success).
pub(crate) async fn create_event(
    executor: &dyn Executor,
    calendar_account: &str,
    draft: &EventDraft,
) -> Result<EventWriteReceipt, JmapError> {
    let object = draft_to_json(draft)?;
    let args = json!({
        "accountId": calendar_account,
        "create": { CREATION_ID: object },
        "sendSchedulingMessages": SCHEDULE,
    });

    let mut req = Request::new([capability::CORE, capability::CALENDARS]);
    let call = req.invoke("CalendarEvent/set", args);
    let resp = executor.execute(&req).await?;
    let result = resp.result(&call)?;

    if let Some(error_type) = set_error(result, "notCreated", CREATION_ID) {
        return Err(JmapError::set(CREATION_ID, error_type));
    }
    // The server-assigned id is the whole point of the `created` map; a create the server
    // neither confirmed nor rejected is a conflict, never a silent success.
    let id = result
        .get("created")
        .and_then(|c| c.get(CREATION_ID))
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| JmapError::set(CREATION_ID, "notFound"))?;
    let event = EventId::try_from(id)
        .map_err(|e| JmapError::protocol(format!("bad created event id: {e}")))?;

    // A JMAP object has no revision token, so the receipt carries none — the caller learns
    // nothing about "the version I just wrote" here, because there is no such thing.
    Ok(EventWriteReceipt::new(
        event,
        draft.uid.clone(),
        RevisionTokens::none(),
    ))
}

/// Applies an edit: one `CalendarEvent/set` `update` whose PatchObject the server merges.
///
/// `base` is the event as the caller read it. Unlike CalDAV, it is **not** the document the
/// surgery runs over — the server holds that — but it still decides two things: the event's
/// time *form*, which a move must not change, and the id of an existing location an edit
/// would rename.
///
/// # Errors
///
/// Returns [`JmapError::Protocol`] if the patch would change the event's time form (a zoned
/// event silently resolved to a UTC instant, an all-day event to a timed one) or if a new
/// end precedes the start. Returns [`JmapError::Set`] when the server rejects the object.
pub(crate) async fn patch_event(
    executor: &dyn Executor,
    calendar_account: &str,
    base: &Event,
    edit: &EventEdit,
) -> Result<EventWriteReceipt, JmapError> {
    let target = edit.event.as_str();
    let patch = patch_to_json(base, edit)?;

    // An empty patch would be a no-op `update` the server may still bump state for. The
    // caller asked for nothing; give the network nothing.
    if patch.is_empty() {
        return Ok(EventWriteReceipt::new(
            edit.event.clone(),
            edit.uid.clone(),
            RevisionTokens::none(),
        ));
    }

    let mut update = Map::new();
    update.insert(target.to_owned(), Value::Object(patch));
    let args = json!({
        "accountId": calendar_account,
        "update": update,
        "sendSchedulingMessages": SCHEDULE,
    });

    let mut req = Request::new([capability::CORE, capability::CALENDARS]);
    let call = req.invoke("CalendarEvent/set", args);
    let resp = executor.execute(&req).await?;
    let result = resp.result(&call)?;

    if let Some(error_type) = set_error(result, "notUpdated", target) {
        return Err(JmapError::set(target, error_type));
    }
    // `updated` is an object keyed by id; its value may be `null` (the server made no extra
    // server-set changes) — still an acknowledgement. A target mentioned in neither map was
    // silently dropped: a conflict, never a false success.
    if result.get("updated").and_then(|u| u.get(target)).is_none() {
        return Err(JmapError::set(target, "notFound"));
    }
    Ok(EventWriteReceipt::new(
        edit.event.clone(),
        edit.uid.clone(),
        RevisionTokens::none(),
    ))
}

/// Deletes an event: one `CalendarEvent/set` `destroy`.
///
/// An event that is **already gone** is a success, not an error. A `notFound` on a destroy
/// means the desired end state already holds, so a retry of a delete whose response was lost
/// resolves cleanly — the same idempotent-delete contract CalDAV gets from treating
/// `404`/`410` as success, and what makes the outbox's "a recovery retry is safe" promise
/// true here.
///
/// # Errors
///
/// Returns [`JmapError`] on a transport/method failure, or [`JmapError::Set`] for any other
/// `SetError` (a `forbidden` is `Permanent`, not something to retry).
pub(crate) async fn delete_event(
    executor: &dyn Executor,
    calendar_account: &str,
    deletion: &EventDeletion,
) -> Result<(), JmapError> {
    let target = deletion.event.as_str();
    let args = json!({
        "accountId": calendar_account,
        "destroy": [target],
        "sendSchedulingMessages": SCHEDULE,
    });

    let mut req = Request::new([capability::CORE, capability::CALENDARS]);
    let call = req.invoke("CalendarEvent/set", args);
    let resp = executor.execute(&req).await?;
    let result = resp.result(&call)?;

    if let Some(error_type) = set_error(result, "notDestroyed", target) {
        // Already gone is the end state we asked for.
        if error_type == "notFound" {
            return Ok(());
        }
        return Err(JmapError::set(target, error_type));
    }
    // `destroyed` is an array of ids.
    let destroyed = result
        .get("destroyed")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(target)));
    if destroyed {
        Ok(())
    } else {
        Err(JmapError::set(target, "notFound"))
    }
}

/// The `SetError` type the server reported for `target` under `map`, if any (RFC 8620 §5.3).
pub(crate) fn set_error<'a>(result: &'a Value, map: &str, target: &str) -> Option<&'a str> {
    result
        .get(map)
        .and_then(|f| f.get(target))
        .and_then(|e| e.get("type"))
        .and_then(Value::as_str)
}

/// Renders an [`EventDraft`] as a JSCalendar `Event` object (RFC 8984 §5).
fn draft_to_json(draft: &EventDraft) -> Result<Value, JmapError> {
    let mut object = Map::new();
    object.insert("@type".to_owned(), json!("Event"));
    object.insert("uid".to_owned(), json!(draft.uid.as_str()));
    object.insert(
        "calendarIds".to_owned(),
        json!({ draft.calendar.as_str(): true }),
    );
    object.insert("title".to_owned(), json!(draft.summary));
    if let Some(description) = &draft.description {
        object.insert("description".to_owned(), json!(description));
    }
    if let Some(location) = &draft.location {
        // JSCalendar has no scalar location: it is a map of id -> Location (RFC 8984
        // §4.2.5). A create mints the sole entry at a fixed id, the same one a later
        // location edit reuses when it finds the event has one.
        object.insert(
            "locations".to_owned(),
            json!({ NEW_LOCATION_ID: { "@type": "Location", "name": location } }),
        );
    }
    for (key, value) in start_fields(&draft.start)? {
        object.insert(key, value);
    }
    object.insert("duration".to_owned(), duration(&draft.start, &draft.end)?);
    Ok(Value::Object(object))
}

/// Renders an [`EventEdit`] as a JSCalendar PatchObject (RFC 8620 §5.3): a flat map of
/// JSON-pointer → new value, where `null` **removes** the property.
///
/// An [`Instance`](PatchTarget::Instance) target prefixes every pointer with
/// `recurrenceOverrides/<original start>/`, which is how JSCalendar names one occurrence
/// (RFC 8984 §4.3.3) — so overriding a single occurrence is the same set of edits under a
/// different prefix, and the server materializes the override from the series itself. That
/// is CalDAV's whole `RECURRENCE-ID`-splitting chore, done server-side.
fn patch_to_json(base: &Event, edit: &EventEdit) -> Result<Map<String, Value>, JmapError> {
    let prefix = match &edit.target {
        PatchTarget::Series => String::new(),
        PatchTarget::Instance(recurrence_id) => {
            // The override key is the occurrence's original start in the series' own form.
            // A zoned series is not overridden by "the same moment" expressed in UTC.
            if !base.start.has_same_form(recurrence_id) {
                return Err(JmapError::protocol(format!(
                    "the occurrence's recurrence id must be in the series' own time form \
                     ({}); naming it in another form overrides no instance",
                    base.start.form_name(),
                )));
            }
            format!(
                "recurrenceOverrides/{}/",
                escape_pointer(&local_date_time(recurrence_id)?)
            )
        }
    };
    let patch = &edit.patch;
    let mut out = Map::new();

    if let Some(summary) = patch.summary_edit() {
        out.insert(format!("{prefix}title"), json!(summary));
    }
    if let Some(description) = patch.description_edit() {
        out.insert(format!("{prefix}description"), text_edit(description));
    }
    if let Some(location) = patch.location_edit() {
        for (pointer, value) in location_edit(base, location) {
            out.insert(format!("{prefix}{pointer}"), value);
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
        out.insert(format!("{prefix}start"), json!(local_date_time(start)?));
    }
    // JSCalendar has no end: it states a `duration` from the start. So an end edit is
    // resolved against the start the event will *have* — the new one if this patch moves it,
    // else the one it already has — which is also what catches a caller who drags the start
    // past the existing end without resizing.
    if let Some(end) = patch.end_edit() {
        let effective_start = patch.start_edit().unwrap_or(&base.start);
        out.insert(format!("{prefix}duration"), duration(effective_start, end)?);
    }
    Ok(out)
}

/// `start` + the fields that state its form: `timeZone` for a zoned value, `showWithoutTime`
/// for an all-day one (RFC 8984 §4.1.2). A floating value carries a null zone and no flag.
fn start_fields(start: &CalendarDateTime) -> Result<Vec<(String, Value)>, JmapError> {
    let mut fields = vec![("start".to_owned(), json!(local_date_time(start)?))];
    match start {
        CalendarDateTime::Zoned { zone, .. } => {
            fields.push(("timeZone".to_owned(), json!(zone.as_str())));
        }
        CalendarDateTime::Date(_) => {
            fields.push(("timeZone".to_owned(), Value::Null));
            fields.push(("showWithoutTime".to_owned(), json!(true)));
        }
        CalendarDateTime::Floating(_) => {
            fields.push(("timeZone".to_owned(), Value::Null));
        }
    }
    Ok(fields)
}

/// A JSCalendar `LocalDateTime` (`YYYY-MM-DDThh:mm:ss`) — the wall clock, with no zone and
/// no offset, whatever form the value has.
fn local_date_time(value: &CalendarDateTime) -> Result<String, JmapError> {
    match value {
        CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => {
            // The same serde rendering the read path parses, so a write round-trips.
            serde_json::to_value(local)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or_else(|| JmapError::protocol("cannot render LocalDateTime"))
        }
        // An all-day value has no wall clock. JSCalendar still states one — midnight — and
        // lets `showWithoutTime` reinterpret it as the whole day (RFC 8984 §4.1.2).
        CalendarDateTime::Date(date) => Ok(format!(
            "{:04}-{:02}-{:02}T00:00:00",
            date.year(),
            date.month(),
            date.day()
        )),
    }
}

/// The JSCalendar `duration` (ISO 8601) from `start` to `end`.
fn duration(start: &CalendarDateTime, end: &CalendarDateTime) -> Result<Value, JmapError> {
    let duration = start.duration_until(end).map_err(|_| {
        JmapError::protocol(
            "the edit would leave the event ending before it begins, or would mix an all-day \
             value with a timed one",
        )
    })?;
    serde_json::to_value(duration)
        .map_err(|e| JmapError::protocol(format!("cannot render duration: {e}")))
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

/// Escapes a JSON Pointer reference token (RFC 6901 §3): `~` → `~0`, `/` → `~1`.
///
/// A JSCalendar id is server-assigned and opaque, so it may contain either; an unescaped
/// pointer would then address the wrong thing.
pub(crate) fn escape_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}
