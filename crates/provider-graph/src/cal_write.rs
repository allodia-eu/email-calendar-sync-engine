//! Calendar writes: `create_event` (POST), `patch_event` (PATCH), `delete_event`
//! (DELETE), all guarded by the `If-Match` ETag — so Graph advertises
//! [`WriteGuard::Enforced`](engine_provider::WriteGuard::Enforced), unlike JMAP.
//!
//! Like JMAP (and unlike CalDAV), **the server does the surgery**: a Graph `PATCH`
//! merges a partial event, so a [`patch_event`] translates the neutral
//! [`EventEdit`] *intent* into a partial event JSON and never re-serializes the lossy
//! projection (`calendar-semantics.md`). A create is the one write built from scratch.

use engine_core::{
    calendar::Event,
    ids::EventId,
    time::{CalendarDate, CalendarDateTime},
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventRsvp, EventWriteReceipt, PatchTarget,
    ProviderError, ProviderResult, RecurrenceEdit, RsvpResponse, TextEdit,
};
use serde_json::{Map, Value, json};

use crate::{
    cal_recur_render::render_recurrence, error::GraphError, json::opt_str, transport::GraphClient,
};

/// Creates `draft` in the bound `calendar_path` (`/me/calendars/{id}`) via `POST …/events`.
///
/// Graph assigns both the event id **and** the `iCalUId` (a client `UID` is not
/// accepted on create — see the `graph.md` limitations), so the receipt carries the
/// **server's** id and uid read from the `201` response.
pub(crate) async fn create_event(
    client: &GraphClient,
    calendar_path: &str,
    draft: &EventDraft,
) -> ProviderResult<EventWriteReceipt> {
    let body = build_create(draft)?;
    let created = client
        .post(
            &client.url(&format!("{calendar_path}/events")),
            "application/json",
            serde_json::to_vec(&body).map_err(GraphError::from)?,
        )
        .await?
        .ok_or_else(|| ProviderError::permanent("create event returned no body"))?;
    receipt(&created, draft.uid.clone())
}

/// Applies `edit` to `base` via `PATCH /me/events/{id}`, guarded by the ETag `base` was read
/// at.
///
/// An [`Instance`](PatchTarget::Instance) target patches the occurrence at the id Graph
/// derives for it ([`occurrence_id`]) — the same id a per-occurrence delete addresses, and the
/// reason the ETag cannot travel with it: that occurrence is a resource of its own with a
/// revision this base does not carry, so an instance edit is **unguarded**. Graph flips the
/// occurrence's `type` to `exception` as it applies the patch.
pub(crate) async fn patch_event(
    client: &GraphClient,
    base: &Event,
    edit: &EventEdit,
) -> ProviderResult<EventWriteReceipt> {
    if let PatchTarget::Instance(_) = &edit.target
        && edit.patch.recurrence_edit().is_some()
    {
        return Err(ProviderError::invalid_state(
            "a recurrence edit targets the series, never one occurrence; an occurrence has no \
             rule of its own",
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
    let (id, guard) = match &edit.target {
        PatchTarget::Series => (base.id.key().as_str().to_owned(), if_match(base)),
        PatchTarget::Instance(occurrence) => (
            occurrence_id(base.id.key().as_str(), &occurrence.start),
            None,
        ),
    };
    let body = build_patch(base, &edit.patch)?;
    let updated = client
        .patch(
            &client.url(&format!("/events/{id}")),
            "application/json",
            guard,
            serde_json::to_vec(&body).map_err(GraphError::from)?,
        )
        .await?;
    // Graph echoes the event it patched. On an instance that is the *occurrence*, whose id
    // is not the series' — so the echo is read only for a series edit; the receipt always
    // names the event the caller holds.
    match (updated, &edit.target) {
        (Some(event), PatchTarget::Series) => receipt(&event, base.uid.clone()),
        _ => Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            RevisionTokens::none(),
        )),
    }
}

/// Answers an invitation via `POST /me/events/{id}/{accept|tentativelyAccept|decline}`.
///
/// Graph's RSVP is a **action endpoint**, not a `PATCH` of the attendee array, and the
/// difference is the whole point: the action makes Exchange send the iTIP `REPLY` and (on a
/// decline) drop the event from the calendar, while patching `attendees` would change the
/// same field and tell nobody.
///
/// Both surrounding controls are native here — `comment` is the note the organizer reads,
/// `sendResponse` is Outlook's "Email organizer" tick — so this is the one transport where
/// [`EventRsvp::notify_organizer`] is honoured verbatim rather than refused.
///
/// **The write is unguarded**, and that is Graph's doing: the action endpoint accepts no
/// `If-Match` (unlike the `PATCH` beside it), so `rsvp.guard` cannot be sent and an answer
/// built on a stale copy lands anyway. The adapter advertises that as
/// [`WriteGuard::Absent`](engine_provider::WriteGuard::Absent) on
/// [`RsvpControls::guard`](engine_provider::RsvpControls::guard) rather than letting the
/// enforced guard it promises for edits imply one here.
///
/// The response is `202 Accepted` with **no body**, so the receipt carries the base's
/// identity and no revision; the post-write reconcile is what re-reads the event.
pub(crate) async fn rsvp_event(
    client: &GraphClient,
    base: &Event,
    rsvp: &EventRsvp,
) -> ProviderResult<EventWriteReceipt> {
    client
        .post(
            &client.url(&format!(
                "/events/{}/{}",
                base.id.key().as_str(),
                graph_rsvp_action(rsvp.response)
            )),
            "application/json",
            serde_json::to_vec(&build_rsvp(rsvp)).map_err(GraphError::from)?,
        )
        .await?;
    Ok(EventWriteReceipt::new(
        base.id.clone(),
        base.uid.clone(),
        RevisionTokens::none(),
    ))
}

/// The Graph action segment for an answer (`graph.md`; Outlook's own three buttons).
const fn graph_rsvp_action(response: RsvpResponse) -> &'static str {
    match response {
        RsvpResponse::Accepted => "accept",
        RsvpResponse::Tentative => "tentativelyAccept",
        RsvpResponse::Declined => "decline",
    }
}

/// Builds the RSVP action body: the note, and whether Exchange emails the organizer.
///
/// `sendResponse` is **always** sent rather than left to Graph's default, because the
/// default is `true` and a caller that asked for silence would be ignored — the one
/// outcome this verb must never produce.
fn build_rsvp(rsvp: &EventRsvp) -> Value {
    let mut body = Map::new();
    if let Some(comment) = &rsvp.comment {
        body.insert("comment".to_owned(), json!(comment));
    }
    body.insert("sendResponse".to_owned(), json!(rsvp.notify_organizer));
    Value::Object(body)
}

/// Deletes `deletion.event` via `DELETE /me/events/{id}`, guarded by the ETag it was read
/// at. An event that is **already gone** (`404`) is success — the delete is idempotent.
///
/// One **occurrence** is deleted the same way, at a *derived* id: Graph addresses an
/// occurrence as `OID.<seriesMasterId>.<local date>`, the shape it uses itself in the
/// series master's `cancelledOccurrences`. So no `/instances` round trip is needed to
/// find one — a `DELETE` on the derived id is enough, and the cancellation then appears in
/// that list.
///
/// **The guard cannot travel with it.** `deletion.guard` is the *series'* ETag, and the
/// occurrence is a different resource with a revision of its own; sending it would fail the
/// precondition on an occurrence nobody had touched. So an occurrence delete is
/// unconditional — a concurrent edit of that occurrence is overwritten rather than refused.
pub(crate) async fn delete_event(
    client: &GraphClient,
    deletion: &EventDeletion,
) -> ProviderResult<()> {
    let master = deletion.event.key().as_str();
    let (id, guard) = match deletion.occurrence_target() {
        None => (
            master.to_owned(),
            deletion
                .guard
                .as_ref()
                .and_then(|r| r.etag.as_ref())
                .map(ETag::as_str),
        ),
        Some(occurrence) => (occurrence_id(master, &occurrence.start), None),
    };
    match client
        .delete(&client.url(&format!("/events/{id}")), guard)
        .await
    {
        // A `404` — already deleted (or moved) — is idempotent success, like a clean delete.
        Ok(()) | Err(GraphError::Status { status: 404, .. }) => Ok(()),
        Err(other) => Err(other.into()),
    }
}

/// Graph's id for one occurrence of a series: `OID.<seriesMasterId>.<YYYY-MM-DD>`, where the
/// date is the occurrence's **local** one.
///
/// ⚠️ A date names an occurrence only while no rule can produce two in a day, which is true
/// of every rule the expander materializes (it has no sub-daily frequencies). A `HOURLY`
/// series would need Graph's real occurrence id, read from `/instances`.
fn occurrence_id(master: &str, start: &CalendarDateTime) -> String {
    let date = calendar_date_of(start);
    format!(
        "OID.{master}.{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    )
}

/// Builds the `POST …/events` create body from a draft.
fn build_create(draft: &EventDraft) -> ProviderResult<Value> {
    let (start, all_day) = graph_datetime(&draft.start)?;
    let (end, _) = graph_datetime(&draft.end)?;
    let mut body = Map::new();
    body.insert("subject".to_owned(), json!(draft.summary));
    body.insert("start".to_owned(), start);
    body.insert("end".to_owned(), end);
    if all_day {
        body.insert("isAllDay".to_owned(), json!(true));
    }
    if let Some(description) = &draft.description {
        body.insert("body".to_owned(), text_body(description));
    }
    if let Some(location) = &draft.location {
        body.insert("location".to_owned(), json!({ "displayName": location }));
    }
    if let Some(recurrence) = &draft.recurrence {
        // Graph takes a structured pattern rather than an RRULE, so the rule is rendered
        // by `cal_recur_render` — the inverse of the reader — and never as a string. The
        // series start date is a parameter because Graph's absolute patterns require the
        // `dayOfMonth`/`month` an RRULE leaves implicit in DTSTART.
        body.insert(
            "recurrence".to_owned(),
            render_recurrence(&recurrence.rule, calendar_date_of(&draft.start))?,
        );
    }
    Ok(Value::Object(body))
}

/// The calendar date a scheduled value falls on, in its own terms.
///
/// The anchor Graph's recurrence range and its absolute patterns are stated against, and the
/// date half of an occurrence id.
fn calendar_date_of(start: &CalendarDateTime) -> CalendarDate {
    match start {
        CalendarDateTime::Date(date) => *date,
        CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => {
            CalendarDate::new(local.year(), local.month(), local.day())
                .unwrap_or_else(|_| unreachable!("a LocalDateTime always holds a valid date"))
        }
    }
}

/// Builds the `PATCH /events/{id}` partial body from the neutral patch intent, checking
/// that a moved `start`/`end` keeps the event's existing time **form** (never a silent
/// zoned→UTC or all-day→timed conversion — `calendar-semantics.md`).
fn build_patch(base: &Event, patch: &EventPatch) -> ProviderResult<Value> {
    let mut body = Map::new();
    if let Some(summary) = patch.summary_edit() {
        body.insert("subject".to_owned(), json!(summary));
    }
    if let Some(edit) = patch.description_edit() {
        body.insert("body".to_owned(), description_body(edit));
    }
    if let Some(edit) = patch.location_edit() {
        let name = match edit {
            TextEdit::Set(name) => name.as_str(),
            TextEdit::Clear => "",
        };
        body.insert("location".to_owned(), json!({ "displayName": name }));
    }
    if let Some(start) = patch.start_edit() {
        guard_form(&base.start, start)?;
        body.insert("start".to_owned(), graph_datetime(start)?.0);
    }
    if let Some(end) = patch.end_edit() {
        // The end must keep the start's form (both endpoints are the same kind).
        guard_form(&base.start, end)?;
        body.insert("end".to_owned(), graph_datetime(end)?.0);
    }
    if let Some(edit) = patch.recurrence_edit() {
        // `null` is how Graph turns a series back into a single event; the structured
        // pattern is how it takes a new rule. Either way the *server* does the surgery.
        //
        // ⚠️ On Graph a rule change also discards every per-occurrence exception and
        // cancellation the user made — measured, and Outlook's own behaviour. That is a
        // property of this transport, not of the edit, so it belongs to the host's
        // confirmation copy rather than to a refusal here (`calendar-semantics.md`).
        let value = match edit {
            RecurrenceEdit::Set(recurrence) => {
                render_recurrence(&recurrence.rule, calendar_date_of(&base.start))?
            }
            RecurrenceEdit::Clear => Value::Null,
        };
        body.insert("recurrence".to_owned(), value);
    }
    Ok(Value::Object(body))
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

/// A [`CalendarDateTime`] as a Graph `{ dateTime, timeZone }` object plus whether it is
/// all-day. A zoned value carries its IANA zone (Graph accepts IANA names on write); an
/// all-day date is UTC-midnight with `isAllDay`. A floating value has no Graph form.
fn graph_datetime(value: &CalendarDateTime) -> ProviderResult<(Value, bool)> {
    match value {
        CalendarDateTime::Zoned { local, zone } => Ok((
            json!({ "dateTime": fmt_local(local), "timeZone": zone.as_str() }),
            false,
        )),
        CalendarDateTime::Date(date) => Ok((
            json!({ "dateTime": format!("{date}T00:00:00"), "timeZone": "UTC" }),
            true,
        )),
        CalendarDateTime::Floating(_) => Err(ProviderError::invalid_state(
            "Graph has no floating-time events; give the event a zone or make it all-day",
        )),
    }
}

/// Formats a wall clock as Graph's `YYYY-MM-DDThh:mm:ss` (no zone/offset).
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

/// A Graph `body` for a plain-text value.
fn text_body(content: &str) -> Value {
    json!({ "contentType": "text", "content": content })
}

/// A Graph `body` for a description edit (`Clear` writes an empty body).
fn description_body(edit: &TextEdit) -> Value {
    match edit {
        TextEdit::Set(text) => text_body(text),
        TextEdit::Clear => text_body(""),
    }
}

/// The `If-Match` ETag the write is guarded by (the one `base` was read at), if any.
fn if_match(base: &Event) -> Option<&str> {
    base.revisions.etag.as_ref().map(ETag::as_str)
}

/// Builds a receipt from a create/patch response: the event id it resolved to, the
/// server's `iCalUId` (falling back to `fallback_uid`), and the new ETag.
fn receipt(
    event: &Value,
    fallback_uid: engine_core::ids::Uid,
) -> ProviderResult<EventWriteReceipt> {
    let id = opt_str(event, "id")
        .ok_or_else(|| ProviderError::permanent("write response had no event id"))?;
    let event_id = EventId::try_from(id)
        .map_err(|e| ProviderError::permanent(format!("bad created event id: {e}")))?;
    let uid = opt_str(event, "iCalUId")
        .and_then(|u| engine_core::ids::Uid::new(u).ok())
        .unwrap_or(fallback_uid);
    let revisions = RevisionTokens {
        etag: opt_str(event, "@odata.etag").map(ETag::new),
        change_key: opt_str(event, "changeKey").map(engine_core::version::ChangeKey::new),
        ..RevisionTokens::none()
    };
    Ok(EventWriteReceipt::new(event_id, uid, revisions))
}

#[cfg(test)]
#[path = "cal_write_tests.rs"]
mod tests;
