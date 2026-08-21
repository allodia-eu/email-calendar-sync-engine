//! What a transport promises about a **calendar write**, beside performing it.
//!
//! Two post-connect facts a host reads off [`Capabilities`](crate::Capabilities)
//! *before* it writes: how strong a lost-update guard the transport can keep
//! ([`WriteGuard`]), and which of the controls around an RSVP it honours
//! ([`RsvpControls`]). They live beside the capability set rather than inside it so
//! that file carries the set alone.

/// What a transport can promise about the **lost-update guard** on a calendar write.
///
/// Every calendar write in this crate names the revision the caller read, so the
/// server can refuse an edit built on a copy that has since moved on. Whether the
/// server actually refuses is *not* universal, and a caller that assumes it is will
/// silently clobber a concurrent edit. So the promise is a post-connect fact a host
/// reads off [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard)
/// **before** it writes, not a property the write API implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGuard {
    /// A write whose guard names a superseded revision is **rejected**, so a stale
    /// edit can never overwrite a newer one.
    ///
    /// CalDAV: the event's `ETag` rides an `If-Match` and a stale one is a `412`
    /// (RFC 7232, RFC 4791 §5.3.2) — proven live against both harness servers.
    Enforced,
    /// The transport offers **no enforceable per-object precondition**: a stale edit
    /// silently wins, and last-writer-wins is the real semantics. A host that needs
    /// to detect a concurrent edit on such a transport must do so above the engine.
    ///
    /// JMAP: a `CalendarEvent` carries no revision token at all
    /// ([`RevisionTokens::is_empty`](engine_core::version::RevisionTokens::is_empty)),
    /// and the only precondition RFC 8620 §5.3 offers — `ifInState` — is scoped to
    /// "all objects of this type in the account" rather than to the object, so it
    /// rejects on *unrelated* concurrent changes instead of on a lost update.
    ///
    /// Note this is **not** a server shortcoming to be waited out. Stalwart enforces
    /// `ifInState` correctly from v0.16.14, and correct enforcement is exactly what
    /// makes it unusable here: an inbound iTIP invitation moves the attendee's
    /// `CalendarEvent` state while they sit idle, so guarding their next edit with it
    /// refuses a write nothing conflicted with. Demonstrated live in
    /// `provider-jmap/tests/live_calendar_precondition.rs`; the reasoning is in
    /// `jmap.md`.
    ///
    /// The lost update JMAP genuinely cannot detect is two writers patching the **same**
    /// property; disjoint properties merge, because `/set` takes a PatchObject.
    Absent,
}

/// What a transport lets the user control about an **RSVP**, beyond the answer itself.
///
/// Answering an invitation always changes the participation status. The two things around
/// it — a note for the organizer, and choosing not to tell them at all — are Outlook's
/// "optional message" and "Email organizer" toggle, and they are **not** universal:
///
/// - **Graph** and **Google** expose both as first-class request fields (`comment` +
///   `sendResponse`; attendee `comment` + `sendUpdates`), so the user's choice is honoured.
/// - **JMAP** carries the toggle but not the note: scheduling is opt-in per request
///   (`sendSchedulingMessages`, default `false`), so silence is honoured — while a
///   `participationComment` no server we run is known to relay is not a note the adapter will claim
///   to have sent.
/// - **CalDAV auto-schedule** (RFC 6638) is the one genuinely *server*-scheduled transport: the
///   server emits the iTIP `REPLY` the moment it sees the changed status and a client cannot
///   suppress it, and there is nowhere to put a per-attendee note in the stored resource either.
///
/// So a host reads this **before** it offers either control, and an adapter that cannot
/// honour one **refuses the write** rather than dropping it: a note that silently goes
/// nowhere, or an "Email organizer" tick that emails them anyway, is worse than a control
/// the user was never shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsvpControls {
    /// The transport has somewhere to put a note for the organizer.
    ///
    /// This is about *carriage*, not delivery: whether the note reaches a human is the
    /// organizer's client's business, so a host should not report it as delivered.
    pub comment: bool,
    /// The user can choose **not** to notify the organizer.
    ///
    /// `false` only where the *server* decides, as RFC 6638 auto-schedule does: the reply
    /// leaves the moment the status changes and no request can hold it back. Where the
    /// notification is a field of the request — Graph's `sendResponse`, Google's
    /// `sendUpdates`, JMAP's `sendSchedulingMessages` — this is `true`.
    ///
    /// An adapter that never sends its transport's field must not report `false` here: that
    /// reads as "the organizer is always told" while doing the exact opposite (#102).
    pub suppress_notification: bool,
    /// How strong a lost-update guard the **RSVP** carries — which is not always the same
    /// as [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard),
    /// because an RSVP is a different request.
    ///
    /// Graph is the case that forces this to be stated separately: its calendar `PATCH`
    /// enforces `If-Match`, but the RSVP is a *action* endpoint
    /// (`POST /events/{id}/accept`) that accepts no precondition at all. Answering "yes"
    /// to a meeting the organizer has since moved therefore lands, and the user has agreed
    /// to a time they never saw. Reporting [`WriteGuard::Enforced`] for the whole adapter
    /// would make that invisible.
    pub guard: WriteGuard,
}

impl RsvpControls {
    /// Refuses an RSVP that asks for a control this transport does not honour.
    ///
    /// Every adapter calls this **before** the write, so the rule that a control is refused
    /// rather than dropped has one implementation rather than four — and so an adapter
    /// cannot advertise a control it then ignores, or ignore one it advertises.
    ///
    /// # Errors
    ///
    /// Returns an
    /// [`InvalidState`](engine_core::error::FailureClass::InvalidState)
    /// [`ProviderError`](crate::ProviderError)
    /// naming the control. A host that read
    /// [`Capabilities::calendar_rsvp`](crate::Capabilities::calendar_rsvp) never reaches it.
    pub fn accept(self, rsvp: &crate::EventRsvp) -> Result<(), crate::ProviderError> {
        if rsvp.comment.is_some() && !self.comment {
            return Err(crate::ProviderError::invalid_state(
                "this transport has nowhere to carry a note to the organizer; read \
                 Capabilities::calendar_rsvp before offering one",
            ));
        }
        if !rsvp.notify_organizer && !self.suppress_notification {
            return Err(crate::ProviderError::invalid_state(
                "this transport's server sends the reply as soon as the participation status \
                 changes, so the organizer cannot be kept out of it; read \
                 Capabilities::calendar_rsvp before offering the toggle",
            ));
        }
        Ok(())
    }
}
