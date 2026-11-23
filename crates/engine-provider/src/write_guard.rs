//! [`WriteGuard`]: what a transport promises about refusing a write built on a stale read, for
//! every kind of guarded write.

/// What a transport can promise about the **lost-update guard** on a write: of a calendar event
/// ([`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard)), a contact
/// ([`Capabilities::contact_write_guard`](crate::Capabilities::contact_write_guard)), or an
/// account setting ([`Vacation::vacation_write_guard`](crate::Vacation::vacation_write_guard)).
///
/// Every calendar write in this crate names the revision the caller read, so the
/// server can refuse an edit built on a copy that has since moved on. Whether the
/// server actually refuses is *not* universal, and a caller that assumes it is will
/// silently clobber a concurrent edit. So the promise is a post-connect fact a host
/// reads off [`Capabilities::calendar_write_guard`](crate::Capabilities::calendar_write_guard)
/// **before** it writes, not a property the write API implies.
///
/// The grain decides the answer, not the protocol. The JMAP notes on the variants are about
/// calendar events, where `ifInState` guards every event in the account at once. A singleton
/// setting such as the out-of-office reply is the only object of its type, so there the type's
/// state is the object's own version, and a JMAP `VacationResponse` write is `Enforced`.
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
