//! Calendar writes: `create_event` (`events.insert`), `patch_event` (`events.patch`),
//! `delete_event` (`events.delete`), all guarded by the `If-Match` ETag — so Google
//! advertises [`WriteGuard::Enforced`](engine_provider::WriteGuard::Enforced), unlike
//! JMAP.
//!
//! Like JMAP/Graph (and unlike CalDAV), **the server does the surgery**: `events.patch`
//! merges a partial event, so [`patch_event`] translates the neutral [`EventEdit`]
//! *intent* into a partial event JSON and never re-serializes the lossy projection
//! (`calendar-semantics.md`). A create is the one write built from scratch.

use engine_core::{
    calendar::{Event, RecurrenceBound, UntilForm, format_rrule},
    ids::EventId,
    time::CalendarDateTime,
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    DraftRecurrence, EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp,
    EventWriteReceipt, PatchTarget, ProviderError, ProviderResult, RecurrenceEdit, RsvpResponse,
    TextEdit,
};
use serde_json::{Map, Value, json};

use crate::{error::GoogleError, json::opt_str, transport::GoogleClient};

/// The Google Calendar v3 events collection for a calendar.
fn events_path(calendar: &str) -> String {
    format!("/calendar/v3/calendars/{calendar}/events")
}

/// Creates `draft` in `calendar` (the bound calendar id) via `POST …/events`.
///
/// Google assigns the event id and (on insert) the `iCalUID`, echoed in the created
/// event; the receipt carries them (falling back to the draft's uid).
pub(crate) async fn create_event(
    client: &GoogleClient,
    calendar: &str,
    draft: &EventDraft,
) -> ProviderResult<EventWriteReceipt> {
    let body = build_create(draft)?;
    let created = client
        .post(
            &client.url(&events_path(calendar)),
            "application/json",
            serde_json::to_vec(&body).map_err(GoogleError::from)?,
        )
        .await?
        .ok_or_else(|| ProviderError::permanent("create event returned no body"))?;
    receipt(&created, draft.uid.clone())
}

/// Applies `edit` to `base` via `PATCH …/events/{id}`, guarded by the ETag `base` was read
/// at. Only [`PatchTarget::Series`] is supported; a single-occurrence edit is deferred
/// (per-instance overrides are staged — `calendar-semantics.md`).
pub(crate) async fn patch_event(
    client: &GoogleClient,
    calendar: &str,
    base: &Event,
    edit: &EventEdit,
) -> ProviderResult<EventWriteReceipt> {
    if !matches!(edit.target, PatchTarget::Series) {
        return Err(ProviderError::invalid_state(
            "Google calendar patch supports only whole-series edits; per-occurrence edits are not \
             yet supported",
        ));
    }
    // An empty patch changes nothing; skip the round trip and report the current revision.
    if edit.patch.is_empty() {
        return Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            base.revisions.clone(),
        ));
    }
    let body = build_patch(base, &edit.patch)?;
    let updated = client
        .patch(
            &client.url(&format!(
                "{}/{}",
                events_path(calendar),
                base.id.key().as_str()
            )),
            "application/json",
            if_match(base),
            serde_json::to_vec(&body).map_err(GoogleError::from)?,
        )
        .await?;
    match updated {
        Some(event) => receipt(&event, base.uid.clone()),
        None => Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            RevisionTokens::none(),
        )),
    }
}

/// Answers an invitation via `events.patch` on the answering attendee's `responseStatus`,
/// with `sendUpdates` deciding whether the organizer hears about it.
///
/// Google has no RSVP endpoint — answering *is* a patch of the attendee array — but it is
/// still a distinct request, because `sendUpdates` is what turns a stored status into an
/// email to the organizer. Omitting it (the API default is `none` for `patch`) would change
/// the status and tell nobody, which is the exact failure the neutral verb exists to prevent.
///
/// **Only the answering attendee is sent.** Google applies just the caller's own
/// `responseStatus` and `comment` when the caller is not the organizer, and ignores every
/// other attendee change — so a one-element array cannot truncate the invitee list. Both
/// halves of that are live-proven (`tests/live_calendar_rsvp.rs`), including the one place it
/// is imprecise: the leniency is keyed on the **caller's role**, so an *organizer answering
/// their own invitation* really does replace the array, and the other guests are dropped.
/// That is a known gap — a host should answer as an attendee — rather than something worked
/// around by rebuilding the array from the lossy projection, which would drop the
/// per-attendee fields the engine does not model (`additionalGuests`) for *everybody*
/// rather than nobody.
///
/// The write keeps the enforced `If-Match` guard the rest of this adapter promises — unlike
/// Graph, whose RSVP action endpoint takes no precondition. The precondition is
/// [`rsvp.guard`](EventRsvp::guard), the revision the *caller* read, not whatever `base`
/// carries at drain time: the outbox recorded the intent when the user answered, and a
/// `guard` of `None` has to mean "answer unconditionally" for the field to mean anything.
pub(crate) async fn rsvp_event(
    client: &GoogleClient,
    calendar: &str,
    base: &Event,
    rsvp: &EventRsvp,
) -> ProviderResult<EventWriteReceipt> {
    // `sendUpdates` is a *query* parameter, not a body field (Calendar API v3). `all` also
    // reaches guests outside the domain, which is what an external organizer is.
    let notify = if rsvp.notify_organizer { "all" } else { "none" };
    let updated = client
        .patch(
            &client.url(&format!(
                "{}/{}?sendUpdates={notify}",
                events_path(calendar),
                base.id.key().as_str()
            )),
            "application/json",
            rsvp.guard
                .as_ref()
                .and_then(|tokens| tokens.etag.as_ref())
                .map(ETag::as_str),
            serde_json::to_vec(&build_rsvp(rsvp)).map_err(GoogleError::from)?,
        )
        .await?;
    match updated {
        Some(event) => receipt(&event, base.uid.clone()),
        None => Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            RevisionTokens::none(),
        )),
    }
}

/// The Google `responseStatus` for an answer (Calendar API v3 `Event.attendees[]`).
const fn google_response_status(response: RsvpResponse) -> &'static str {
    match response {
        RsvpResponse::Accepted => "accepted",
        RsvpResponse::Tentative => "tentative",
        RsvpResponse::Declined => "declined",
    }
}

/// Builds the RSVP patch body: a one-element `attendees` array naming the **matched**
/// address, its new `responseStatus`, and the note if there is one.
///
/// The address is the one that travelled with the intent, never the account's primary
/// identity — an alias invitation must answer as the alias or Google matches no attendee
/// and the answer goes nowhere.
fn build_rsvp(rsvp: &EventRsvp) -> Value {
    let mut attendee = Map::new();
    attendee.insert("email".to_owned(), json!(rsvp.attendee));
    attendee.insert(
        "responseStatus".to_owned(),
        json!(google_response_status(rsvp.response)),
    );
    if let Some(comment) = &rsvp.comment {
        attendee.insert("comment".to_owned(), json!(comment));
    }
    json!({ "attendees": [Value::Object(attendee)] })
}

/// Deletes `deletion.event` via `DELETE …/events/{id}`, guarded by the ETag it was read
/// at. An event that is **already gone** is success — the delete is idempotent — and
/// Google signals gone as either `404` **or** `410 Gone` (a re-delete of an
/// already-deleted event). A stale `If-Match` on an event that still exists (modified
/// out from under the caller) is a `412` [`Conflict`](engine_core::error::FailureClass::Conflict),
/// surfaced so the caller refetches — not swallowed as idempotent.
pub(crate) async fn delete_event(
    client: &GoogleClient,
    calendar: &str,
    deletion: &EventDeletion,
) -> ProviderResult<()> {
    let guard = deletion
        .guard
        .as_ref()
        .and_then(|r| r.etag.as_ref())
        .map(ETag::as_str);
    match client
        .delete(
            &client.url(&format!(
                "{}/{}",
                events_path(calendar),
                deletion.event.key().as_str()
            )),
            guard,
        )
        .await
    {
        Ok(())
        | Err(GoogleError::Status {
            status: 404 | 410, ..
        }) => Ok(()),
        Err(other) => Err(other.into()),
    }
}

/// Builds the `events.insert` body from a draft.
fn build_create(draft: &EventDraft) -> ProviderResult<Value> {
    let mut body = Map::new();
    body.insert("summary".to_owned(), json!(draft.summary));
    body.insert("start".to_owned(), google_datetime(&draft.start)?);
    body.insert("end".to_owned(), google_datetime(&draft.end)?);
    if let Some(description) = &draft.description {
        body.insert("description".to_owned(), json!(description));
    }
    if let Some(location) = &draft.location {
        body.insert("location".to_owned(), json!(location));
    }
    if let Some(recurrence) = &draft.recurrence {
        // Google's `recurrence` is an array of raw iCalendar lines, so the rule goes
        // through the shared `format_rrule` — the same bytes CalDAV writes.
        body.insert(
            "recurrence".to_owned(),
            json!([format!("RRULE:{}", rrule_value(recurrence, &draft.start)?)]),
        );
    }
    Ok(Value::Object(body))
}

/// The `RRULE` value for a draft's recurrence, in the `UNTIL` form its own start requires.
///
/// Google stores iCalendar lines verbatim, so RFC 5545 §3.3.10 binds here exactly as it
/// does on CalDAV: a zoned start obliges `UNTIL` in UTC, and the instant that takes is the
/// caller's to resolve because no adapter carries tzdata (`DraftRecurrence`).
fn rrule_value(recurrence: &DraftRecurrence, start: &CalendarDateTime) -> ProviderResult<String> {
    let until = match (start, &recurrence.rule.bound) {
        // No UNTIL to render at all; the form is irrelevant.
        (_, RecurrenceBound::Unbounded | RecurrenceBound::Count(_))
        | (CalendarDateTime::Floating(_), RecurrenceBound::Until(_)) => UntilForm::Floating,
        (CalendarDateTime::Date(_), RecurrenceBound::Until(_)) => UntilForm::Date,
        (CalendarDateTime::Zoned { .. }, RecurrenceBound::Until(_)) => {
            UntilForm::Utc(recurrence.until.ok_or_else(|| {
                ProviderError::invalid_state(
                    "a recurrence ending at a wall clock on a zoned event needs that clock \
                     resolved to an instant: RFC 5545 requires UNTIL in UTC when the start \
                     carries a zone. Build the draft with DraftRecurrence::ending_at",
                )
            })?)
        }
    };
    format_rrule(&recurrence.rule, until).map_err(|e| ProviderError::invalid_state(e.to_string()))
}

/// Builds the `events.patch` partial body from the neutral patch intent, checking that a
/// moved `start`/`end` keeps the event's existing time **form** (never a silent
/// zoned→UTC or all-day→timed conversion — `calendar-semantics.md`).
fn build_patch(base: &Event, patch: &EventPatch) -> ProviderResult<Value> {
    let mut body = Map::new();
    if let Some(summary) = patch.summary_edit() {
        body.insert("summary".to_owned(), json!(summary));
    }
    if let Some(edit) = patch.description_edit() {
        body.insert("description".to_owned(), json!(text_value(edit)));
    }
    if let Some(edit) = patch.location_edit() {
        body.insert("location".to_owned(), json!(text_value(edit)));
    }
    if let Some(start) = patch.start_edit() {
        guard_form(&base.start, start)?;
        body.insert("start".to_owned(), google_datetime(start)?);
    }
    if let Some(end) = patch.end_edit() {
        guard_form(&base.start, end)?;
        body.insert("end".to_owned(), google_datetime(end)?);
    }
    if let Some(edit) = patch.recurrence_edit() {
        // An empty array clears the rule. Measured: Google also accepts `null` here and
        // clears it just the same — the array is chosen because it says "this event has
        // no recurrence rules" rather than relying on how a patch reads a null.
        let value = match edit {
            RecurrenceEdit::Set(recurrence) => {
                json!([format!("RRULE:{}", rrule_value(recurrence, &base.start)?)])
            }
            RecurrenceEdit::Clear => json!([]),
        };
        body.insert("recurrence".to_owned(), value);
    }
    Ok(Value::Object(body))
}

/// The string a text edit writes (`Clear` writes an empty string).
fn text_value(edit: &TextEdit) -> &str {
    match edit {
        TextEdit::Set(text) => text.as_str(),
        TextEdit::Clear => "",
    }
}

/// Rejects a new value whose time **form** differs from the event's current one (a
/// zoned→UTC or all-day→timed conversion is silent corruption — `calendar-semantics.md`).
fn guard_form(current: &CalendarDateTime, new: &CalendarDateTime) -> ProviderResult<()> {
    if current.has_same_form(new) {
        Ok(())
    } else {
        Err(ProviderError::invalid_state(format!(
            "a move must keep the event's form ({}), got {}",
            current.form_name(),
            new.form_name()
        )))
    }
}

/// A [`CalendarDateTime`] as a Google `start`/`end` object: a zoned value carries its
/// IANA zone in `timeZone` (Google accepts a zoneless `dateTime` alongside it); an all-day
/// date is `{ date }`. A floating value has no Google form (give it a zone or make it
/// all-day).
fn google_datetime(value: &CalendarDateTime) -> ProviderResult<Value> {
    match value {
        CalendarDateTime::Zoned { local, zone } => {
            Ok(json!({ "dateTime": fmt_local(local), "timeZone": zone.as_str() }))
        }
        CalendarDateTime::Date(date) => Ok(json!({ "date": date.to_string() })),
        CalendarDateTime::Floating(_) => Err(ProviderError::invalid_state(
            "Google events need a zone or all-day date; a floating time has no form",
        )),
    }
}

/// Formats a wall clock as `YYYY-MM-DDThh:mm:ss` (no zone/offset — the `timeZone` field
/// carries the zone).
fn fmt_local(local: &engine_core::time::LocalDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second()
    )
}

/// The `If-Match` ETag the write is guarded by (the one `base` was read at), if any.
fn if_match(base: &Event) -> Option<&str> {
    base.revisions.etag.as_ref().map(ETag::as_str)
}

/// Builds a receipt from a create/patch response: the event id it resolved to, the
/// server's `iCalUID` (falling back to `fallback_uid`), and the new ETag.
fn receipt(
    event: &Value,
    fallback_uid: engine_core::ids::Uid,
) -> ProviderResult<EventWriteReceipt> {
    let id = opt_str(event, "id")
        .ok_or_else(|| ProviderError::permanent("write response had no event id"))?;
    let event_id = EventId::try_from(id)
        .map_err(|e| ProviderError::permanent(format!("bad created event id: {e}")))?;
    let uid = opt_str(event, "iCalUID")
        .and_then(|u| engine_core::ids::Uid::new(u).ok())
        .unwrap_or(fallback_uid);
    let revisions = RevisionTokens {
        etag: opt_str(event, "etag").map(ETag::new),
        ..RevisionTokens::none()
    };
    Ok(EventWriteReceipt::new(event_id, uid, revisions))
}

#[cfg(test)]
#[path = "cal_write_tests.rs"]
mod tests;
