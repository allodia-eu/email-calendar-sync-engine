//! CalDAV calendar writes: conditional `PUT` (create/patch/replace) and `DELETE`.
//!
//! This is how CalDAV renders the neutral write verbs (`engine-provider`). A calendar
//! object resource is created or replaced with a single `PUT` of its iCalendar body (RFC
//! 4791 §5.3.2) and removed with `DELETE` — CalDAV has **no partial write**, so a patch is
//! still a whole-document `PUT`, of the stored bytes with the edit applied
//! ([`patch_event_ical`](engine_ical::patch_event_ical)), never of a re-serialized
//! projection (`calendar-semantics.md`).
//!
//! Optimistic concurrency rides on the resource `ETag`, and CalDAV **enforces** it
//! ([`WriteGuard::Enforced`](engine_provider::WriteGuard::Enforced)): a create sends
//! `If-None-Match: *` (never overwrite an existing resource), while a patch, a replace or a
//! guarded delete sends `If-Match: <etag>` (apply only while the server copy is unchanged).
//! A failed precondition is `412` → [`FailureClass::Conflict`], recovered by refetching and
//! re-applying the edit, never by a blind retry (`error.rs`).
//!
//! [`FailureClass::Conflict`]: engine_core::error::FailureClass::Conflict

use engine_core::{
    calendar::Event,
    ids::EventId,
    raw::RawIcal,
    version::{ETag, RevisionTokens},
};
use engine_ical::{build_event_ical, patch_event_ical};
use engine_provider::{
    EventDeletion, EventDraft, EventEdit, EventRsvp, EventWrite, EventWriteReceipt, ReplyDelivery,
    WritePrecondition,
};

use crate::{
    error::CalDavError,
    transport::{DavExecutor, DavMethod, Precondition, WriteRequest},
};

/// The iCalendar media type sent on a `PUT` (RFC 5545 §3.1; RFC 4791 §5.3.2).
const ICALENDAR_CONTENT_TYPE: &str = "text/calendar; charset=utf-8";

/// Creates a new event: build its iCalendar document and `PUT` it to a freshly minted
/// href under `If-None-Match: *`.
///
/// The href is the client's to choose (RFC 4791 §5.3.2) and is minted from the draft's
/// `UID`, so a retried create targets the same resource — and `If-None-Match` then makes
/// the retry fail loudly (`412`) rather than silently overwrite whatever a concurrent
/// writer put there.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure. A resource already existing at the
/// href is a `412`, classified [`Conflict`](engine_core::error::FailureClass::Conflict).
pub(crate) async fn create_event(
    exec: &dyn DavExecutor,
    href: EventId,
    draft: &EventDraft,
) -> Result<EventWriteReceipt, CalDavError> {
    let ical = build_event_ical(draft)?;
    put(exec, &href, &ical, Precondition::IfNoneMatch)
        .await
        .map(|revisions| EventWriteReceipt::new(href.clone(), draft.uid.clone(), revisions))
}

/// Applies an edit to a stored event: patch the stored `RawIcal` in place, then `PUT` the
/// result under `If-Match` on the revision the caller read.
///
/// The `base` is load-bearing: its `raw_ical` is the document the surgery runs over (so
/// every property the engine does not model survives), and its `ETag` is the guard.
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the base carries no stored `raw_ical` to patch (it was
/// never synced from this transport), or if the patch is rejected by the patcher's rules (a
/// time-form change, an inverted end, an `Instance` target on a non-recurring event).
/// Returns a `412`-classified [`Conflict`](engine_core::error::FailureClass::Conflict) if
/// the server copy moved on, and [`CalDavError`] on any transport/HTTP failure.
pub(crate) async fn patch_event(
    exec: &dyn DavExecutor,
    base: &Event,
    edit: &EventEdit,
) -> Result<EventWriteReceipt, CalDavError> {
    let stored = base.raw_ical.as_ref().ok_or_else(|| {
        CalDavError::ical(
            "event has no stored iCalendar to patch; it was not synced from this provider. \
             Re-sync the calendar before editing — patching a document we do not hold would \
             mean rebuilding it from the lossy projection, which silently drops every \
             property the engine does not model",
        )
    })?;
    let ical = patch_event_ical(stored, &edit.target, &edit.patch)?;
    put(
        exec,
        &edit.event,
        &ical,
        guard(base.revisions.etag.as_ref()),
    )
    .await
    .map(|revisions| EventWriteReceipt::new(edit.event.clone(), edit.uid.clone(), revisions))
}

/// Answers an invitation: rewrite *my* `PARTSTAT` in the stored iCalendar
/// ([`imip::set_my_partstat`](crate::imip::set_my_partstat)) and `PUT` it back under
/// `If-Match`.
///
/// **The `PUT` is the whole RSVP.** CalDAV has no reply verb: on an RFC 6638 auto-schedule
/// server the *server* notices the changed participation status and emits the iTIP `REPLY`
/// to the organizer itself (§3.2). That is why neither of the two surrounding controls is
/// available here, and why the adapter refuses rather than ignores them:
///
/// - **`notify_organizer: false`** cannot be honoured — the reply leaves the moment the `PUT`
///   lands. Suppressing it would mean `SCHEDULE-AGENT=CLIENT` on the `ORGANIZER`, and then *nobody*
///   sends the reply, because client-side iMIP delivery is not wired (`imip.rs`). A tick that
///   emails them anyway is worse than one never shown.
/// - **A `comment`** has nowhere to go: iCalendar has no per-attendee comment parameter, and a
///   `COMMENT` property on the stored `VEVENT` is the organizer's copy of the event, not a note
///   from an attendee.
///
/// Both are advertised as absent on
/// [`Capabilities::calendar_rsvp`](engine_provider::Capabilities::calendar_rsvp), and
/// enforced by [`RsvpControls::accept`](engine_provider::RsvpControls::accept) in the
/// adapter — so a host that reads capabilities never reaches those errors, and one that does
/// not gets a refusal rather than a silent drop.
///
/// # The `PUT` is the whole *write*, and not the whole story
///
/// It stores the answer, and on an auto-scheduling server it is also what makes the reply
/// leave — but it says nothing about whether the reply *arrived*. That verdict is written
/// into the stored object as `SCHEDULE-STATUS` (RFC 6638 §3.2.9), so where the server
/// schedules, the object is read back once and the result rides home on
/// [`EventWriteReceipt::reply_delivery`]. Most servers report nothing; one real deployment
/// reports a permanent failure on every reply it ever sends. Neither may be guessed at, which
/// is why [`ReplyDelivery`] has a third state — see [`reply_delivery`].
///
/// # Errors
///
/// Returns [`CalDavError::Ical`] if the base carries no stored `raw_ical` (never synced from
/// this transport) or has no `ATTENDEE` matching the answering address — you cannot answer
/// an invitation you are not on. Returns a `412`-classified
/// [`Conflict`](engine_core::error::FailureClass::Conflict) if the server copy moved on, and
/// [`CalDavError`] on any transport/HTTP failure.
pub(crate) async fn rsvp_event(
    exec: &dyn DavExecutor,
    base: &Event,
    rsvp: &EventRsvp,
    scheduling: bool,
) -> Result<EventWriteReceipt, CalDavError> {
    let stored = base.raw_ical.as_ref().ok_or_else(|| {
        CalDavError::ical(
            "event has no stored iCalendar to answer; it was not synced from this provider. \
             Re-sync the calendar before answering — rewriting a document we do not hold would \
             mean rebuilding it from the lossy projection, which silently drops every property \
             the engine does not model",
        )
    })?;
    let ical = crate::imip::set_my_partstat(stored, &rsvp.attendee, &rsvp.response.status())?;
    let revisions = put(
        exec,
        &rsvp.event,
        &ical,
        rsvp.guard
            .as_ref()
            .map_or(Precondition::None, |tokens| guard(tokens.etag.as_ref())),
    )
    .await?;
    let receipt = EventWriteReceipt::new(rsvp.event.clone(), rsvp.uid.clone(), revisions);
    Ok(receipt.with_reply_delivery(reply_delivery(exec, rsvp, scheduling).await))
}

/// Reads back what the server recorded about delivering the reply we just stored.
///
/// # Why a second request, and why it is not optional
///
/// RFC 6638 §3.2.9 puts the outcome in the *stored object*, not in the `PUT` response — there
/// is no header and no body to read it from, so the only way to learn it is to fetch the
/// resource again. Measured against a real Sabre deployment the status is written **during**
/// the `PUT`: it was present on the very first `GET`, ~140 ms after the write returned, at
/// 10 ms polling resolution. So one request is enough and no retry loop is warranted.
///
/// # Why it never fails the write
///
/// The answer is already stored by the time this runs. A read that fails would turn a
/// successful RSVP into an error the caller might retry — answering twice — so every failure
/// path here degrades to [`ReplyDelivery::NotReported`], which is the truth: we did not learn
/// anything. The cost of that silence is a prompt the user does not see, against the cost of
/// a duplicate reply; the first is recoverable and the second is not.
///
/// # Why it is gated
///
/// A server that does not advertise `calendar-auto-schedule` performs no scheduling, so it
/// has nothing to report and the request would be pure latency on every answer.
async fn reply_delivery(
    exec: &dyn DavExecutor,
    rsvp: &EventRsvp,
    scheduling: bool,
) -> ReplyDelivery {
    if !scheduling {
        return ReplyDelivery::NotReported;
    }
    let Ok(response) = exec
        .send(DavMethod::Get, rsvp.event.as_str(), "0", String::new())
        .await
    else {
        return ReplyDelivery::NotReported;
    };
    if !(200..300).contains(&response.status) {
        return ReplyDelivery::NotReported;
    }
    crate::schedule_status::reply_delivery(&response.body)
}

/// Stores an event's whole document — replacing what is there, or creating where nothing
/// is (`engine-provider`'s [`EventWrite`] docs explain why this verb exists beside the
/// neutral patch).
///
/// Each [`WritePrecondition`] has an exact HTTP rendering here, which is why CalDAV can
/// promise [`WriteGuard::Enforced`](engine_provider::WriteGuard::Enforced) on all three:
/// [`IfUnchanged`](WritePrecondition::IfUnchanged) → `If-Match: <etag>`,
/// [`IfAbsent`](WritePrecondition::IfAbsent) → `If-None-Match: *` (RFC 7232 §3.2), and
/// [`Unconditional`](WritePrecondition::Unconditional) → no conditional header at all.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure; a failed precondition is a `412`
/// classified [`Conflict`](engine_core::error::FailureClass::Conflict) — for a create that
/// means a resource is already there, which the caller resolves by re-reading, never by
/// re-writing unconditionally.
pub(crate) async fn put_event(
    exec: &dyn DavExecutor,
    write: &EventWrite,
) -> Result<EventWriteReceipt, CalDavError> {
    let precondition = match &write.guard {
        WritePrecondition::Unconditional => Precondition::None,
        WritePrecondition::IfUnchanged(tokens) => guard(tokens.etag.as_ref()),
        WritePrecondition::IfAbsent => Precondition::IfNoneMatch,
    };
    put(exec, &write.event, &write.ical, precondition)
        .await
        .map(|revisions| EventWriteReceipt::new(write.event.clone(), write.uid.clone(), revisions))
}

/// `DELETE`s an event, guarded by `If-Match` on the revision the caller read.
///
/// `DELETE` is idempotent (RFC 7231 §4.3.5): a resource that is **already absent**
/// (`404`/`410`) means the desired end state already holds, so it resolves as success — not
/// the `Permanent` error a generic non-`2xx` check would yield. This is what makes the
/// outbox's "a recovery retry is safe" promise true for deletes: re-running a delete whose
/// response was lost (the first one landed) sees `404` and succeeds. A `412` (the resource
/// still exists but its ETag moved) is a genuine
/// [`Conflict`](engine_core::error::FailureClass::Conflict), surfaced for refetch.
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure; a failed `If-Match` is a `412`
/// classified [`Conflict`](engine_core::error::FailureClass::Conflict).
pub(crate) async fn delete_event(
    exec: &dyn DavExecutor,
    deletion: &EventDeletion,
) -> Result<(), CalDavError> {
    let precondition = deletion
        .guard
        .as_ref()
        .map_or(Precondition::None, |tokens| guard(tokens.etag.as_ref()));
    let request = WriteRequest {
        method: DavMethod::Delete,
        href: deletion.event.as_str().to_owned(),
        content_type: None,
        precondition,
        body: String::new(),
    };
    let response = exec.send_write(request).await?;
    // Already-gone is success for an idempotent delete; anything else non-`2xx`
    // (incl. a `412` If-Match conflict) flows through the classified error path.
    if matches!(response.status, 404 | 410) {
        return Ok(());
    }
    response.into_write_etag()?;
    Ok(())
}

/// The one `PUT`, shared by every write that stores a document. Returns the revision the
/// server reported, empty when it returned no `ETag` (RFC 4791 §5.3.4 only *recommends*
/// one — the caller then learns the revision from the next sync).
async fn put(
    exec: &dyn DavExecutor,
    href: &EventId,
    ical: &RawIcal,
    precondition: Precondition,
) -> Result<RevisionTokens, CalDavError> {
    let request = WriteRequest {
        method: DavMethod::Put,
        href: href.as_str().to_owned(),
        content_type: Some(ICALENDAR_CONTENT_TYPE),
        precondition,
        body: ical.as_str().to_owned(),
    };
    let etag = exec.send_write(request).await?.into_write_etag()?;
    Ok(etag.map_or_else(RevisionTokens::none, |etag| {
        RevisionTokens::from_etag(ETag::new(etag))
    }))
}

/// The `If-Match` precondition for a guard, or none when the caller read an event that
/// carried no `ETag`.
///
/// An event synced from CalDAV always has one (`getetag` is fetched with every resource),
/// so the `None` arm is for an event the caller assembled by hand. It degrades to an
/// unconditional write rather than failing — the alternative would reject a legitimate
/// write on a technicality — but it is also why the *neutral* guard is a
/// [`RevisionTokens`], not an `ETag`: an adapter can only enforce what its transport
/// actually carries, and
/// [`Capabilities::calendar_write_guard`](engine_provider::Capabilities::calendar_write_guard)
/// is what tells a host which it got.
fn guard(etag: Option<&ETag>) -> Precondition {
    etag.map_or(Precondition::None, |etag| {
        Precondition::IfMatch(etag.as_str().to_owned())
    })
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
