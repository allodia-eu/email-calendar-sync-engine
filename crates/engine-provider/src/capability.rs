//! What an adapter can do.
//!
//! The engine queries capabilities from the adapter and routes work accordingly;
//! callers must not switch on provider kind (`providers.md`). This is the minimal
//! set the step-4 mail spine and calendar-read slice need — the data domains a
//! provider exposes. It maps directly onto the JMAP session's advertised
//! capability URNs (`urn:ietf:params:jmap:mail` → [`Capabilities::mail`], etc.)
//! and grows as protocol features are added.

/// What a transport can promise about the **lost-update guard** on a calendar write.
///
/// Every calendar write in this crate names the revision the caller read, so the
/// server can refuse an edit built on a copy that has since moved on. Whether the
/// server actually refuses is *not* universal, and a caller that assumes it is will
/// silently clobber a concurrent edit. So the promise is a post-connect fact a host
/// reads off [`Capabilities::calendar_write_guard`] **before** it writes, not a
/// property the write API implies.
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
    /// as [`Capabilities::calendar_write_guard`], because an RSVP is a different request.
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

/// The data domains a provider supports.
///
/// Built with a `with_*` chain from [`Capabilities::none`] so each flag is set by
/// name, never by a positional boolean:
///
/// ```
/// use engine_provider::Capabilities;
/// let caps = Capabilities::none().with_mail().with_submission();
/// assert!(caps.mail() && caps.submission() && !caps.calendars());
/// ```
///
/// Calendar writes are the one capability that is not a plain flag: an adapter states
/// *how strong* its lost-update guard is ([`WriteGuard`]), because "can write" and
/// "can refuse a stale write" are different promises and only one of them is
/// universal.
// These are independent capability flags (a small fixed bitset), not the state of
// a state machine, so the excessive-bools heuristic's "use a state machine"
// suggestion does not apply; each flag is queried by name on its own.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent capability flags, not state-machine state"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    mail: bool,
    mail_writes: bool,
    message_source: bool,
    submission: bool,
    scheduling_submission: bool,
    idle: bool,
    calendars: bool,
    /// `None` when the adapter cannot write calendars at all; otherwise the strength
    /// of the guard it can promise. One field rather than two, so "guarded but not
    /// writable" is unrepresentable.
    calendar_writes: Option<WriteGuard>,
    /// `None` when the adapter cannot answer an invitation at all; otherwise which of the
    /// two surrounding controls it honours. One field rather than two, so "carries a
    /// comment but cannot RSVP" is unrepresentable.
    calendar_rsvp: Option<RsvpControls>,
    calendar_scheduling: bool,
    contacts: bool,
    contact_writes: Option<WriteGuard>,
    contact_groups: bool,
    contact_photos: bool,
}

impl Capabilities {
    /// No capabilities (the starting point for the builder).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            mail: false,
            mail_writes: false,
            message_source: false,
            submission: false,
            scheduling_submission: false,
            idle: false,
            calendars: false,
            calendar_writes: None,
            calendar_rsvp: None,
            calendar_scheduling: false,
            contacts: false,
            contact_writes: None,
            contact_groups: false,
            contact_photos: false,
        }
    }

    /// Marks mail read/sync as supported.
    #[must_use]
    pub const fn with_mail(mut self) -> Self {
        self.mail = true;
        self
    }

    /// Marks mail **writes** (mark-read/flag, move, delete via
    /// [`Provider::edit_mail`](crate::Provider::edit_mail)) as supported. Distinct
    /// from [`with_mail`](Self::with_mail), the read capability — a mailbox the
    /// account can read but not mutate (a shared read-only IMAP folder) advertises
    /// [`mail`](Self::mail) without this, exactly as a no-SMTP adapter advertises
    /// [`mail`](Self::mail) without [`submission`](Self::submission).
    #[must_use]
    pub const fn with_mail_writes(mut self) -> Self {
        self.mail_writes = true;
        self
    }

    /// Marks fetching a message's raw RFC 5322 source on demand (Tier-3 bodies via
    /// [`Provider::fetch_message_source`](crate::Provider::fetch_message_source)) as
    /// supported. Distinct from [`with_mail`](Self::with_mail), the metadata
    /// read/sync capability — an adapter can sync envelopes without being able to
    /// download full bodies, exactly as a no-SMTP adapter advertises
    /// [`mail`](Self::mail) without [`submission`](Self::submission).
    #[must_use]
    pub const fn with_message_source(mut self) -> Self {
        self.message_source = true;
        self
    }

    /// Marks mail submission (`EmailSubmission`) as supported.
    #[must_use]
    pub const fn with_submission(mut self) -> Self {
        self.submission = true;
        self
    }

    /// Marks this transport as able to send an **iMIP scheduling message** — a
    /// [`Draft`](crate::Draft) carrying a [`DraftCalendar`](crate::DraftCalendar)
    /// ([`Capabilities::scheduling_submission`]).
    ///
    /// Distinct from [`with_submission`](Self::with_submission): a transport can send
    /// ordinary mail perfectly and still be unable to put the `method=` parameter on a body
    /// part, which is what makes a message a scheduling message rather than a file.
    #[must_use]
    pub const fn with_scheduling_submission(mut self) -> Self {
        self.scheduling_submission = true;
        self
    }

    /// Marks **push / change notification** as supported — the adapter can hand a
    /// host a [`Watch`](crate::Watch) session that signals when a scope changes (the
    /// IMAP `IDLE` keep-alive, RFC 2177; a JMAP push channel or Graph webhook later).
    /// Distinct from [`with_mail`](Self::with_mail), the read capability — a host
    /// reads this to decide whether to offer an "as it comes in" sync strategy versus
    /// periodic polling, exactly as a no-SMTP adapter advertises [`mail`](Self::mail)
    /// without [`submission`](Self::submission). Push is a **latency optimization**:
    /// the authoritative reconciliation is always the scope's normal sync, so a
    /// provider without this is fully functional on a poll.
    #[must_use]
    pub const fn with_idle(mut self) -> Self {
        self.idle = true;
        self
    }

    /// Marks calendar read/sync as supported.
    #[must_use]
    pub const fn with_calendars(mut self) -> Self {
        self.calendars = true;
        self
    }

    /// Marks calendar **writes** (create/patch/delete events) as supported, stating
    /// how strong a lost-update [`WriteGuard`] the transport can promise.
    ///
    /// Distinct from [`with_calendars`](Self::with_calendars), the read capability — a
    /// calendar the account can read but not write (a shared read-only CalDAV
    /// collection, or a calendar-read-only adapter) advertises
    /// [`calendars`](Self::calendars) without this, exactly as a mail adapter with no
    /// SMTP advertises [`mail`](Self::mail) without [`submission`](Self::submission).
    #[must_use]
    pub const fn with_calendar_writes(mut self, guard: WriteGuard) -> Self {
        self.calendar_writes = Some(guard);
        self
    }

    /// Marks **RSVP** (answering an invitation) as supported, stating which of the two
    /// surrounding controls the transport honours ([`RsvpControls`]).
    ///
    /// Distinct from [`with_calendar_writes`](Self::with_calendar_writes): an RSVP is a
    /// separate verb on every transport because it makes the server tell the organizer,
    /// which no edit does. An adapter that can create and patch events but cannot schedule
    /// advertises the writes without this.
    #[must_use]
    pub const fn with_calendar_rsvp(mut self, controls: RsvpControls) -> Self {
        self.calendar_rsvp = Some(controls);
        self
    }

    /// Marks the server as performing **scheduling itself** — it delivers the iTIP
    /// messages a calendar write implies, so the caller never has to
    /// ([`Capabilities::calendar_scheduling`]).
    ///
    /// Distinct from [`with_calendar_rsvp`](Self::with_calendar_rsvp), which says only that
    /// the transport can *express* an answer. On CalDAV those come apart, which is why this
    /// is a flag rather than an implication — see the getter.
    #[must_use]
    pub const fn with_calendar_scheduling(mut self) -> Self {
        self.calendar_scheduling = true;
        self
    }

    /// Marks address-book/contact read and sync as supported.
    #[must_use]
    pub const fn with_contacts(mut self) -> Self {
        self.contacts = true;
        self
    }

    /// Marks source-targeted contact writes and their guard strength.
    #[must_use]
    pub const fn with_contact_writes(mut self, guard: WriteGuard) -> Self {
        self.contact_writes = Some(guard);
        self
    }

    /// Marks contact-group reads as supported.
    #[must_use]
    pub const fn with_contact_groups(mut self) -> Self {
        self.contact_groups = true;
        self
    }

    /// Marks authenticated, on-demand contact-photo fetch as supported.
    #[must_use]
    pub const fn with_contact_photos(mut self) -> Self {
        self.contact_photos = true;
        self
    }

    /// Whether mail read/sync is supported.
    #[must_use]
    pub const fn mail(self) -> bool {
        self.mail
    }

    /// Whether mail writes (mark-read/flag, move, delete) are supported.
    #[must_use]
    pub const fn mail_writes(self) -> bool {
        self.mail_writes
    }

    /// Whether on-demand raw-message-source fetch (Tier-3 bodies) is supported.
    #[must_use]
    pub const fn message_source(self) -> bool {
        self.message_source
    }

    /// Whether mail submission is supported.
    #[must_use]
    pub const fn submission(self) -> bool {
        self.submission
    }

    /// Whether this transport can send an **iMIP scheduling message** — a
    /// [`Draft`](crate::Draft) carrying a [`DraftCalendar`](crate::DraftCalendar).
    ///
    /// Read this together with
    /// [`calendar_scheduling`](Self::calendar_scheduling): between them they answer the
    /// only question that matters to a host holding an invitation — *can this account
    /// answer it at all?* If the calendar server schedules, the answer travels by itself;
    /// if not, it travels only if the mail transport can send one. **Neither means the
    /// invitation cannot be answered**, and a host is better off saying so than storing a
    /// `PARTSTAT` nobody will ever see.
    ///
    /// `false` on **JMAP**, and this is a server-verified fact rather than a spec reading.
    /// The `method=` parameter that makes a part a scheduling message (RFC 6047 §2.4)
    /// cannot go in an `EmailBodyPart`'s `type`, which is a media type without parameters.
    /// RFC 8621 §4.1.3 appears to offer a way round — a raw `header:Content-Type` on the
    /// part — but driven against Stalwart all three shapes fail: with the raw header alone
    /// the server emits **two** `Content-Type` fields (ours, then a generated
    /// `text/plain`), with `type` *and* the raw header it emits two again, and with `type`
    /// alone the parameter is simply absent. Every one of those sends successfully and
    /// arrives as something the organizer's client will not process. So the adapter
    /// **refuses** the draft instead (`jmap.md`).
    ///
    /// `true` on **IMAP/SMTP**, **Graph** and **Google**, which submit assembled RFC 5322
    /// bytes through `engine-rfc5322` and therefore own the parameter outright.
    #[must_use]
    pub const fn scheduling_submission(self) -> bool {
        self.scheduling_submission
    }

    /// Whether push / change notification ([`Watch`](crate::Watch), e.g. IMAP
    /// `IDLE`) is supported.
    #[must_use]
    pub const fn idle(self) -> bool {
        self.idle
    }

    /// Whether calendar read/sync is supported.
    #[must_use]
    pub const fn calendars(self) -> bool {
        self.calendars
    }

    /// Whether calendar writes (create/patch/delete events) are supported at all.
    #[must_use]
    pub const fn calendar_writes(self) -> bool {
        self.calendar_writes.is_some()
    }

    /// How strong a lost-update guard this transport can promise on a calendar write,
    /// or `None` if it cannot write calendars.
    ///
    /// Read this **before** writing. [`WriteGuard::Absent`] means a stale edit silently
    /// wins, so "the write succeeded" does not imply "nobody else's edit was lost" — a
    /// host that must not lose a concurrent edit has to detect it itself.
    #[must_use]
    pub const fn calendar_write_guard(self) -> Option<WriteGuard> {
        self.calendar_writes
    }

    /// Which RSVP controls this transport honours, or `None` if it cannot answer an
    /// invitation at all.
    ///
    /// Read this **before** offering a note field or an "Email organizer" toggle: an
    /// adapter refuses a write asking for a control it does not have, rather than dropping
    /// it silently ([`RsvpControls`]).
    #[must_use]
    pub const fn calendar_rsvp(self) -> Option<RsvpControls> {
        self.calendar_rsvp
    }

    /// Whether the **server** performs the scheduling a calendar write implies — sending the
    /// iTIP `REQUEST` to the invitees, the `REPLY` to the organizer, the `CANCEL` on a delete.
    ///
    /// [`calendar_rsvp`](Self::calendar_rsvp) answers *"can this transport express an
    /// answer?"*. This answers *"will anyone be told?"*, and **they are different questions**.
    /// Read this before writing, because `false` means the caller owns the delivery: it must
    /// build the iTIP object and send it as an iMIP message itself (RFC 6047 — see
    /// [`DraftCalendar`](crate::DraftCalendar)), or the organizer never learns the answer
    /// while every local copy says it was sent.
    ///
    /// Per transport:
    ///
    /// - **CalDAV** — **discovered**, not assumed. RFC 4791 is calendar *access*; RFC 6638 adds
    ///   scheduling on top and §2 makes a conforming server advertise the `calendar-auto-schedule`
    ///   token in the `DAV:` header of an `OPTIONS` response. So the adapter asks at connect. Both
    ///   are real: the Stalwart harness advertises it, the SabreDAV one does not (`caldav.md`).
    /// - **Graph** and **Google** — constant `true`; both schedule server-side, with no opt-out a
    ///   client could reach.
    /// - **JMAP** — `true`, but for a different reason: `sendSchedulingMessages` makes it the
    ///   *request's* choice, and the adapter asks on every write. This says the caller does not
    ///   send the iMIP itself, which is what a caller needs to know. Note there is nothing here to
    ///   *detect* — JMAP Calendars leaves scheduling to the implementation and offers no capability
    ///   to probe, so a server that quietly ignores the argument would look identical (`jmap.md`).
    #[must_use]
    pub const fn calendar_scheduling(self) -> bool {
        self.calendar_scheduling
    }

    /// Whether address-book/contact read and sync is supported.
    #[must_use]
    pub const fn contacts(self) -> bool {
        self.contacts
    }

    /// Whether contact writes are supported.
    #[must_use]
    pub const fn contact_writes(self) -> bool {
        self.contact_writes.is_some()
    }

    /// Contact-write lost-update guard strength.
    #[must_use]
    pub const fn contact_write_guard(self) -> Option<WriteGuard> {
        self.contact_writes
    }

    /// Whether contact-group reads are supported.
    #[must_use]
    pub const fn contact_groups(self) -> bool {
        self.contact_groups
    }

    /// Whether authenticated contact-photo fetch is supported.
    #[must_use]
    pub const fn contact_photos(self) -> bool {
        self.contact_photos
    }
}

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
