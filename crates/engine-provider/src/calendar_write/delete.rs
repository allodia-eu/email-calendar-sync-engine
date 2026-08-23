//! The **intent** of a delete: the whole event, or one occurrence of a series.
//!
//! "Delete this" is two different requests on a recurring event, and — exactly as with an
//! edit ([`PatchTarget`](super::PatchTarget)) — only the user knows which, so [`DeleteTarget`]
//! has no `Default`. Deleting Tuesday's standup is either cancelling that Tuesday or
//! cancelling the standup.
//!
//! # Removing one occurrence is not a delete on every transport
//!
//! Only two of the four express it as one. Graph and Google address the occurrence by a
//! **derived id** and `DELETE` it; CalDAV and JMAP have nothing to delete, because an
//! occurrence is not a stored object — the series is — so they *edit the series* to say the
//! occurrence is gone (an `EXDATE`, an `excluded` override). That is why the intent is
//! stated here and rendered per adapter, and why this verb carries a
//! [`stamp`](DeleteTarget::Occurrence::stamp): on the transports that rewrite a document,
//! this delete is a revision of it.

use engine_core::{
    calendar::Event,
    ids::{EventId, Uid},
    time::{CalendarDateTime, UtcDateTime},
    version::RevisionTokens,
};
use serde::{Deserialize, Serialize};

/// One occurrence of a series, named the way the transports address it.
///
/// The [`start`](Self::start) is the occurrence's **original** start — the start it had
/// before any edit, which is its identity within the series and not where it may since have
/// moved to. It must be in the series' own time form (a zoned series names its occurrences
/// with zoned wall clocks, not with "the same moment" in UTC), and it must name a real
/// occurrence of the rule: no adapter expands recurrence to check, so a caller that invents
/// one addresses no instance.
///
/// That start is all iCalendar (`RECURRENCE-ID`), JSCalendar (a `recurrenceOverrides` key)
/// and Graph (whose occurrence id ends in the occurrence's local *date*) need.
/// [`instant`](Self::instant) exists for Google alone, which addresses an occurrence by an
/// id built from that start **in UTC** — and resolving a wall clock to an instant needs
/// tzdata no adapter carries, for the same reason a recurrence ending at a wall clock
/// carries its own resolved bound ([`DraftRecurrence`](super::DraftRecurrence)). So a caller
/// that may be talking to Google states it; one that cannot, or whose occurrence is all-day,
/// needs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    /// The occurrence's original start, in the series' own time form.
    pub start: CalendarDateTime,
    /// That same moment as an instant, when the caller could resolve it.
    pub instant: Option<UtcDateTime>,
}

impl Occurrence {
    /// The occurrence that originally started at `start`, with no instant resolved.
    ///
    /// Enough for every transport but Google — see the type docs.
    #[must_use]
    pub fn starting(start: CalendarDateTime) -> Self {
        Self {
            start,
            instant: None,
        }
    }

    /// The occurrence that originally started at `start`, which is the instant `instant`.
    #[must_use]
    pub fn at(start: CalendarDateTime, instant: UtcDateTime) -> Self {
        Self {
            start,
            instant: Some(instant),
        }
    }
}

/// What a delete removes: the whole event, or one occurrence of it.
///
/// No `Default` — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteTarget {
    /// The event itself, and with it every occurrence. The stored object goes.
    Series,
    /// One occurrence. The series stays, minus that instance.
    Occurrence {
        /// Which occurrence.
        occurrence: Occurrence,
        /// The revision stamp for the rewritten series, on a transport that expresses this
        /// as an edit of the stored document (CalDAV's `DTSTAMP`/`LAST-MODIFIED`). The
        /// caller's, because engine time types deliberately cannot read the system clock; a
        /// server that stamps its own ignores it.
        stamp: UtcDateTime,
    },
}

/// A request to delete an event, or one of its occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDeletion {
    /// The event to delete. On an [`Occurrence`](DeleteTarget::Occurrence) target this is
    /// still the **series**, never a synthetic id for the instance: deriving that id is the
    /// adapter's job, and the two transports that have one derive it differently.
    pub event: EventId,
    /// Its cross-system `UID`. Carried here rather than passed alongside because the outbox
    /// serializes every calendar op on it, and a caller that supplied it separately could
    /// supply the *wrong* one — pairing a delete of event A with the lock on event B.
    pub uid: Uid,
    /// The revision the delete is guarded by — the one the caller read. `None` deletes
    /// unconditionally.
    ///
    /// It guards the **series**, which is what this names, so an adapter that removes one
    /// occurrence by deleting a separate instance resource cannot apply it — the instance
    /// has a revision of its own that this is not. Each says what it does.
    pub guard: Option<RevisionTokens>,
    /// What the delete removes.
    pub target: DeleteTarget,
}

impl EventDeletion {
    /// Deletes `base` — the event as the caller read it — guarded by the revision it was
    /// read at, so the delete cannot silently discard someone else's newer edit.
    #[must_use]
    pub fn of(base: &Event) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            guard: Some(base.revisions.clone()),
            target: DeleteTarget::Series,
        }
    }

    /// Removes **one occurrence** of `base`, leaving the rest of the series.
    ///
    /// `stamp` revises the series on a transport that rewrites its document; see
    /// [`DeleteTarget::Occurrence`].
    #[must_use]
    pub fn occurrence(base: &Event, occurrence: Occurrence, stamp: UtcDateTime) -> Self {
        Self {
            event: base.id.clone(),
            uid: base.uid.clone(),
            guard: Some(base.revisions.clone()),
            target: DeleteTarget::Occurrence { occurrence, stamp },
        }
    }

    /// Deletes with **no** guard: the event goes, whatever the server holds.
    #[must_use]
    pub fn unconditional(event: EventId, uid: Uid) -> Self {
        Self {
            event,
            uid,
            guard: None,
            target: DeleteTarget::Series,
        }
    }

    /// The occurrence this delete removes, or `None` when it removes the whole event.
    #[must_use]
    pub fn occurrence_target(&self) -> Option<&Occurrence> {
        match &self.target {
            DeleteTarget::Series => None,
            DeleteTarget::Occurrence { occurrence, .. } => Some(occurrence),
        }
    }
}
