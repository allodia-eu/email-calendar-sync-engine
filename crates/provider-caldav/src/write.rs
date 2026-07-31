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
    EventDeletion, EventDraft, EventEdit, EventRsvp, EventWrite, EventWriteReceipt,
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
    let ical = build_event_ical(draft);
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
    put(
        exec,
        &rsvp.event,
        &ical,
        rsvp.guard
            .as_ref()
            .map_or(Precondition::None, |tokens| guard(tokens.etag.as_ref())),
    )
    .await
    .map(|revisions| EventWriteReceipt::new(rsvp.event.clone(), rsvp.uid.clone(), revisions))
}

/// Replaces an event's whole stored document (the iMIP RSVP path — `engine-provider`'s
/// [`EventWrite`] docs explain why this verb exists beside the neutral patch).
///
/// # Errors
///
/// Returns [`CalDavError`] on a transport/HTTP failure; a failed guard is a `412`
/// classified [`Conflict`](engine_core::error::FailureClass::Conflict).
pub(crate) async fn put_event(
    exec: &dyn DavExecutor,
    write: &EventWrite,
) -> Result<EventWriteReceipt, CalDavError> {
    let precondition = write
        .guard
        .as_ref()
        .map_or(Precondition::None, |tokens| guard(tokens.etag.as_ref()));
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
mod tests {
    use engine_core::{
        error::FailureClass,
        ids::{CalendarId, EventId, ProviderKey, Uid},
        membership::Memberships,
        raw::RawIcal,
        time::{CalendarDateTime, UtcDateTime},
    };
    use engine_provider::{EventPatch, PatchTarget};

    use super::*;
    use crate::test_support::{Replay, wrote};

    fn href() -> EventId {
        EventId::try_from("/dav/cal/alice%40test.local/default/evt-1.ics").unwrap()
    }

    fn uid() -> Uid {
        Uid::new("evt-1@test.local").unwrap()
    }

    fn calendar() -> CalendarId {
        CalendarId::new(ProviderKey::new("/dav/cal/alice%40test.local/default/").unwrap())
    }

    fn stamp() -> UtcDateTime {
        UtcDateTime::new(2026, 6, 20, 8, 0, 0).unwrap()
    }

    fn at(hour: u8) -> CalendarDateTime {
        CalendarDateTime::utc(format!("2026-06-25T{hour:02}:00:00").parse().unwrap())
    }

    const BODY: &str = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:evt-1@test.local\r\n\
                        DTSTAMP:20260601T000000Z\r\nDTSTART:20260625T140000Z\r\n\
                        DTEND:20260625T150000Z\r\nSUMMARY:Old\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    /// An event as `sync_events` hands it back: the stored raw plus the ETag it was read at.
    fn stored(etag: Option<&str>) -> Event {
        let mut event = Event::new(href(), uid(), Memberships::of_one(calendar()), at(14));
        event.raw_ical = Some(RawIcal::new(BODY));
        event.revisions = etag.map_or_else(RevisionTokens::none, |e| {
            RevisionTokens::from_etag(ETag::new(e))
        });
        event
    }

    fn draft() -> EventDraft {
        EventDraft::new(
            calendar(),
            uid(),
            "Sprint planning",
            at(14),
            at(15),
            stamp(),
        )
    }

    #[tokio::test]
    async fn create_puts_the_built_document_with_if_none_match() {
        let exec = Replay::new(vec![wrote(201, Some("\"v1\""))]);
        let receipt = create_event(&exec, href(), &draft()).await.unwrap();

        // The receipt names the href the create resolved to, the uid, and the new ETag.
        assert_eq!(receipt.event, href());
        assert_eq!(receipt.uid, uid());
        assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v1\"")));

        // The executor saw a PUT of a *built* iCalendar document under the create
        // precondition — the caller never handed us bytes.
        let writes = exec.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].method, DavMethod::Put);
        assert_eq!(writes[0].href, href().as_str());
        assert_eq!(writes[0].content_type, Some(ICALENDAR_CONTENT_TYPE));
        assert_eq!(writes[0].precondition, Precondition::IfNoneMatch);
        assert!(writes[0].body.contains("UID:evt-1@test.local"));
        assert!(writes[0].body.contains("SUMMARY:Sprint planning"));
    }

    #[tokio::test]
    async fn patch_puts_the_stored_document_edited_in_place_under_if_match() {
        let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
        let base = stored(Some("\"v1\""));
        let edit = EventEdit::new(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).summary("Renamed"),
        );
        let receipt = patch_event(&exec, &base, &edit).await.unwrap();
        assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v2\"")));

        let writes = exec.writes();
        // The guard is the revision the caller read, taken off the base — never
        // hand-assembled, so it cannot be stale by construction.
        assert_eq!(
            writes[0].precondition,
            Precondition::IfMatch("\"v1\"".to_owned())
        );
        // The body is the *stored* document with only the summary rewritten: the
        // untouched DTSTART is still there byte-for-byte.
        assert!(writes[0].body.contains("SUMMARY:Renamed\r\n"));
        assert!(!writes[0].body.contains("SUMMARY:Old"));
        assert!(writes[0].body.contains("DTSTART:20260625T140000Z\r\n"));
    }

    #[tokio::test]
    async fn patching_an_event_we_never_synced_is_refused_not_rebuilt() {
        // No stored raw means we do not hold the document. Rebuilding it from the lossy
        // projection would silently drop the VALARMs, the VTIMEZONE, the X- properties —
        // a save that looks like it worked. Refuse instead, and say why.
        let exec = Replay::new(vec![]);
        let mut base = stored(Some("\"v1\""));
        base.raw_ical = None;
        let edit = EventEdit::new(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).summary("Renamed"),
        );
        let err = patch_event(&exec, &base, &edit).await.unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Permanent);
        assert!(exec.writes().is_empty(), "nothing may reach the network");
    }

    #[tokio::test]
    async fn a_put_without_a_response_etag_yields_no_revision() {
        // Some servers omit the ETag on the PUT; the receipt then carries no revision and
        // the caller learns it from the next sync.
        let exec = Replay::new(vec![wrote(201, None)]);
        let receipt = create_event(&exec, href(), &draft()).await.unwrap();
        assert!(receipt.revisions.is_empty());
    }

    #[tokio::test]
    async fn a_stale_guard_is_a_conflict() {
        let exec = Replay::new(vec![wrote(412, None)]);
        let base = stored(Some("\"stale\""));
        let edit = EventEdit::new(
            &base,
            PatchTarget::Series,
            EventPatch::new(stamp()).summary("Renamed"),
        );
        let err = patch_event(&exec, &base, &edit).await.unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }

    #[tokio::test]
    async fn a_document_write_replaces_the_bytes_under_the_bases_guard() {
        // The iMIP RSVP path: the caller assembled the document itself.
        let exec = Replay::new(vec![wrote(204, Some("\"v2\""))]);
        let base = stored(Some("\"v1\""));
        let write = EventWrite::replacing(&base, RawIcal::new("BEGIN:VCALENDAR\r\nEND:VCALENDAR"));
        let receipt = put_event(&exec, &write).await.unwrap();
        assert_eq!(receipt.revisions.etag, Some(ETag::new("\"v2\"")));

        let writes = exec.writes();
        assert_eq!(
            writes[0].precondition,
            Precondition::IfMatch("\"v1\"".to_owned())
        );
        assert_eq!(writes[0].body, "BEGIN:VCALENDAR\r\nEND:VCALENDAR");
    }

    #[tokio::test]
    async fn delete_sends_if_match_when_guarded() {
        let exec = Replay::new(vec![wrote(204, None)]);
        let base = stored(Some("\"v2\""));
        delete_event(&exec, &EventDeletion::of(&base))
            .await
            .unwrap();
        let writes = exec.writes();
        assert_eq!(writes[0].method, DavMethod::Delete);
        assert_eq!(writes[0].href, href().as_str());
        assert!(writes[0].body.is_empty());
        assert_eq!(
            writes[0].precondition,
            Precondition::IfMatch("\"v2\"".to_owned())
        );
    }

    #[tokio::test]
    async fn unconditional_delete_sends_no_precondition() {
        let exec = Replay::new(vec![wrote(204, None)]);
        delete_event(&exec, &EventDeletion::unconditional(href(), uid()))
            .await
            .unwrap();
        assert_eq!(exec.writes()[0].precondition, Precondition::None);
    }

    #[tokio::test]
    async fn a_guard_naming_no_revision_degrades_to_unconditional() {
        // An event assembled by hand (never synced) carries no ETag, so there is nothing to
        // put in an `If-Match`. Rejecting the write on that technicality would be worse than
        // sending it unguarded — and `calendar_write_guard` is what tells the host what a
        // guard is worth on this transport.
        let exec = Replay::new(vec![wrote(204, None)]);
        let base = stored(None);
        delete_event(&exec, &EventDeletion::of(&base))
            .await
            .unwrap();
        assert_eq!(exec.writes()[0].precondition, Precondition::None);
    }

    #[tokio::test]
    async fn deleting_an_already_gone_resource_is_idempotent_success() {
        // DELETE is idempotent (RFC 7231 §4.3.5): an already-absent resource
        // (404/410) resolves as success, so a lost-ack retry of a landed delete
        // does not report a hard failure.
        for status in [404, 410] {
            let exec = Replay::new(vec![wrote(status, None)]);
            delete_event(&exec, &EventDeletion::unconditional(href(), uid()))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_delete_if_match_conflict_is_surfaced() {
        // A 412 (the resource still exists but its ETag moved) is a real conflict,
        // distinct from the already-gone case — the caller refetches and merges.
        let exec = Replay::new(vec![wrote(412, None)]);
        let base = stored(Some("\"stale\""));
        let err = delete_event(&exec, &EventDeletion::of(&base))
            .await
            .unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Conflict);
    }

    #[tokio::test]
    async fn a_delete_server_error_still_surfaces() {
        // A genuine failure (e.g. 503) is not swallowed by the idempotent-gone path.
        let exec = Replay::new(vec![wrote(503, None)]);
        let err = delete_event(&exec, &EventDeletion::unconditional(href(), uid()))
            .await
            .unwrap_err();
        assert_eq!(err.failure_class(), FailureClass::Retryable);
    }
}
