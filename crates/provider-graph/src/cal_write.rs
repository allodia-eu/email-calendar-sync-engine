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
    time::CalendarDateTime,
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventPatch, EventWriteReceipt, PatchTarget,
    ProviderError, ProviderResult, TextEdit,
};
use serde_json::{Map, Value, json};

use crate::{error::GraphError, json::opt_str, transport::GraphClient};

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

/// Applies `edit` to `base` via `PATCH /me/events/{id}`, guarded by the ETag `base` was
/// read at. Only [`PatchTarget::Series`] is supported; a single-occurrence edit is
/// deferred (Graph needs the occurrence resolved from its recurrence-id, which v1.0 does
/// not expose — see the `graph.md` limitations).
pub(crate) async fn patch_event(
    client: &GraphClient,
    base: &Event,
    edit: &EventEdit,
) -> ProviderResult<EventWriteReceipt> {
    if !matches!(edit.target, PatchTarget::Series) {
        return Err(ProviderError::invalid_state(
            "Graph calendar patch supports only whole-series edits; per-occurrence edits are not \
             yet supported (v1.0 exposes no occurrence recurrence-id)",
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
            &client.url(&format!("/events/{}", base.id.key().as_str())),
            "application/json",
            if_match(base),
            serde_json::to_vec(&body).map_err(GraphError::from)?,
        )
        .await?;
    // Graph echoes the updated event; fall back to the base's identity if it did not.
    match updated {
        Some(event) => receipt(&event, base.uid.clone()),
        None => Ok(EventWriteReceipt::new(
            base.id.clone(),
            base.uid.clone(),
            RevisionTokens::none(),
        )),
    }
}

/// Deletes `deletion.event` via `DELETE /me/events/{id}`, guarded by the ETag it was read
/// at. An event that is **already gone** (`404`) is success — the delete is idempotent.
pub(crate) async fn delete_event(
    client: &GraphClient,
    deletion: &EventDeletion,
) -> ProviderResult<()> {
    let guard = deletion
        .guard
        .as_ref()
        .and_then(|r| r.etag.as_ref())
        .map(ETag::as_str);
    match client
        .delete(
            &client.url(&format!("/events/{}", deletion.event.key().as_str())),
            guard,
        )
        .await
    {
        // A `404` — already deleted (or moved) — is idempotent success, like a clean delete.
        Ok(()) | Err(GraphError::Status { status: 404, .. }) => Ok(()),
        Err(other) => Err(other.into()),
    }
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
    Ok(Value::Object(body))
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
