//! What one transport can hold of a reply, and the one rule that says what it would lose.

use core::fmt;
use std::collections::BTreeSet;

use super::{AutoReply, ReplyAudience, VacationResponse};

/// What a transport can hold of a [`VacationResponse`].
///
/// An adapter states it; a host reads it to offer only what the account keeps, and every write
/// checks the reply against it with [`lost_parts`](Self::lost_parts) first. The table it comes
/// from is `filters-and-out-of-office.md`, §5: JMAP holds a window and a subject and answers
/// everyone with one reply; Graph has no subject but an outside reply and narrower audiences;
/// Gmail has a subject and audiences but no outside reply.
///
/// Every transport can switch a reply on and off and carry a plain-text and an HTML body, so
/// neither is a field. Built with [`everyone`](Self::everyone) and the `with_*` methods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacationVocabulary {
    /// Whether a reply can run in a [`VacationWindow`](super::VacationWindow).
    pub window: bool,
    /// Whether a reply can carry its own subject.
    pub subject: bool,
    /// The audiences a reply can be limited to; [`ReplyAudience::Everyone`] always.
    pub audiences: BTreeSet<ReplyAudience>,
    /// Whether senders outside the organization can get a reply of their own.
    pub outside_reply: bool,
}

impl VacationVocabulary {
    /// The least a transport holds: one reply, on or off, to every sender, with no window and no
    /// subject of its own.
    #[must_use]
    pub fn everyone() -> Self {
        Self {
            window: false,
            subject: false,
            audiences: BTreeSet::from([ReplyAudience::Everyone]),
            outside_reply: false,
        }
    }

    /// Holds a window (builder-style).
    #[must_use]
    pub fn with_window(mut self) -> Self {
        self.window = true;
        self
    }

    /// Holds a subject (builder-style).
    #[must_use]
    pub fn with_subject(mut self) -> Self {
        self.subject = true;
        self
    }

    /// Holds `audience` too (builder-style).
    #[must_use]
    pub fn with_audience(mut self, audience: ReplyAudience) -> Self {
        self.audiences.insert(audience);
        self
    }

    /// Holds a reply of its own for senders outside the organization (builder-style).
    #[must_use]
    pub fn with_outside_reply(mut self) -> Self {
        self.outside_reply = true;
        self
    }

    /// The parts of `reply` this transport cannot hold, each once, in [`VacationPart`] order;
    /// empty when it holds all of it.
    ///
    /// A part counts only when the reply uses it: a window with either end, a subject on either
    /// reply, an audience other than everyone, an outside reply at all. Dropping any of them would
    /// change who gets what, and an audience dropped would reply to senders the user chose not to
    /// answer, so an edit refuses a reply with a lost part instead of writing the rest.
    #[must_use]
    pub fn lost_parts(&self, reply: &VacationResponse) -> Vec<VacationPart> {
        let has_subject = |part: &AutoReply| part.subject.is_some();
        [
            (
                VacationPart::Window,
                !self.window && !reply.window.is_open(),
            ),
            (
                VacationPart::Subject,
                !self.subject
                    && (has_subject(&reply.reply)
                        || reply.outside_reply.as_ref().is_some_and(has_subject)),
            ),
            (
                VacationPart::Audience,
                !self.audiences.contains(&reply.audience),
            ),
            (
                VacationPart::OutsideReply,
                !self.outside_reply && reply.outside_reply.is_some(),
            ),
        ]
        .into_iter()
        .filter_map(|(part, lost)| lost.then_some(part))
        .collect()
    }
}

/// A part of a reply a transport may be unable to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum VacationPart {
    /// The window it runs in.
    Window,
    /// A subject of its own.
    Subject,
    /// An audience narrower than everyone.
    Audience,
    /// A second reply for senders outside the organization.
    OutsideReply,
}

impl fmt::Display for VacationPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Window => "window",
            Self::Subject => "subject",
            Self::Audience => "audience",
            Self::OutsideReply => "outside reply",
        })
    }
}
