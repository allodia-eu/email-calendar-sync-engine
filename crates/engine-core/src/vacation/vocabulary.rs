//! What one transport can hold of a reply, and the one rule that says what it would lose.

use core::fmt;
use std::collections::BTreeSet;

use super::{AutoReply, ReplyAudience, VacationResponse};

/// What a transport can hold of a [`VacationResponse`].
///
/// An adapter states it; a host reads it to offer only what the account keeps, and every write
/// checks the reply against it with [`lost_parts`](Self::lost_parts) first. The table it comes
/// from is `filters-and-out-of-office.md`, §5:
///
/// | | window | subject | bodies | audiences | outside reply |
/// |---|---|---|---|---|---|
/// | JMAP | either end | yes | text and HTML | everyone | no |
/// | Graph | both ends | no | one (HTML) | everyone, organization, organization and contacts | yes |
/// | Gmail | either end | yes | one (text or HTML) | everyone, contacts; organization and contacts in organization on Workspace | no |
///
/// Every transport can switch a reply on and off and carry at least one body. Built with
/// [`everyone`](Self::everyone) and the `with_*` methods, and read through its accessors, so a
/// capability added later breaks no caller.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacationVocabulary {
    window: WindowSupport,
    subject: bool,
    bodies: ReplyBodies,
    audiences: BTreeSet<ReplyAudience>,
    outside_reply: bool,
}

/// Which windows a transport can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowSupport {
    /// None: a reply runs for as long as it is on.
    None,
    /// A start, an end, or both, each optional (JMAP `fromDate` and `toDate`).
    EitherEnd,
    /// Both ends or neither: a scheduled reply needs a start and an end (Graph's `scheduled`
    /// status takes both `scheduledStartDateTime` and `scheduledEndDateTime`).
    BothEnds,
}

/// Which bodies a transport can hold for one reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReplyBodies {
    /// One body per reply: Graph keeps each message as one HTML string, and Gmail drops the text
    /// body where an HTML one is set (observed), so a reply carrying a separate text body and HTML
    /// body cannot keep both as they are.
    One,
    /// A plain-text body and an HTML body side by side (JMAP `textBody` and `htmlBody`).
    TextAndHtml,
}

impl VacationVocabulary {
    /// The least a transport holds: one reply, on or off, with one body, to every sender, with no
    /// window and no subject of its own.
    #[must_use]
    pub fn everyone() -> Self {
        Self {
            window: WindowSupport::None,
            subject: false,
            bodies: ReplyBodies::One,
            audiences: BTreeSet::from([ReplyAudience::Everyone]),
            outside_reply: false,
        }
    }

    /// Holds windows of this kind (builder-style).
    #[must_use]
    pub fn with_window(mut self, window: WindowSupport) -> Self {
        self.window = window;
        self
    }

    /// Holds a subject (builder-style).
    #[must_use]
    pub fn with_subject(mut self) -> Self {
        self.subject = true;
        self
    }

    /// Holds a plain-text and an HTML body side by side (builder-style).
    #[must_use]
    pub fn with_text_and_html(mut self) -> Self {
        self.bodies = ReplyBodies::TextAndHtml;
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

    /// Which windows a reply can run in.
    #[must_use]
    pub fn window(&self) -> WindowSupport {
        self.window
    }

    /// Whether a reply can carry its own subject.
    #[must_use]
    pub fn has_subject(&self) -> bool {
        self.subject
    }

    /// Which bodies one reply can carry.
    #[must_use]
    pub fn bodies(&self) -> ReplyBodies {
        self.bodies
    }

    /// The audiences a reply can be limited to; [`ReplyAudience::Everyone`] always.
    #[must_use]
    pub fn audiences(&self) -> &BTreeSet<ReplyAudience> {
        &self.audiences
    }

    /// Whether senders outside the organization can get a reply of their own.
    #[must_use]
    pub fn has_outside_reply(&self) -> bool {
        self.outside_reply
    }

    /// The parts of `reply` this transport cannot hold, each once, in [`VacationPart`] order;
    /// empty when it holds all of it.
    ///
    /// A part counts only when the reply uses it: a window the transport cannot express, a
    /// subject on either reply, separate text and HTML bodies on either reply where one body is
    /// kept, an audience the transport lacks, an outside reply at all. Dropping any of them would
    /// change who gets what, and an audience dropped would reply to senders the user chose not to
    /// answer, so an edit refuses a reply with a lost part instead of writing the rest.
    #[must_use]
    pub fn lost_parts(&self, reply: &VacationResponse) -> Vec<VacationPart> {
        let replies = || core::iter::once(&reply.reply).chain(reply.outside_reply.as_ref());
        let both_bodies = |part: &AutoReply| part.text.is_some() && part.html.is_some();
        let window = &reply.window;
        let window_lost = match self.window {
            WindowSupport::None => !window.is_open(),
            WindowSupport::EitherEnd => false,
            WindowSupport::BothEnds => window.start.is_some() != window.end.is_some(),
        };
        [
            (VacationPart::Window, window_lost),
            (
                VacationPart::Subject,
                !self.subject && replies().any(|part| part.subject.is_some()),
            ),
            (
                VacationPart::Bodies,
                self.bodies == ReplyBodies::One && replies().any(both_bodies),
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
    /// A plain-text body and an HTML body kept apart.
    Bodies,
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
            Self::Bodies => "separate text and HTML bodies",
            Self::Audience => "audience",
            Self::OutsideReply => "outside reply",
        })
    }
}
