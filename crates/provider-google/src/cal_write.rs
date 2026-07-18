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
    calendar::Event,
    ids::EventId,
    time::CalendarDateTime,
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventWriteReceipt, PatchTarget,
    ProviderError, ProviderResult, TextEdit,
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
    Ok(Value::Object(body))
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
