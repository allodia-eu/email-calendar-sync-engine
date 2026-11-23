//! The reply itself: [`VacationResponse`] and the parts it is made of.

use core::fmt;

use crate::{raw::RawVacation, time::UtcDateTime, version::VacationVersion};

/// An account's out-of-office reply, as read from its server or as a host wants it written.
///
/// It is the union of what the transports can say (`filters-and-out-of-office.md`, §5): a
/// transport that cannot hold a part says so through its
/// [`VacationVocabulary`](super::VacationVocabulary), and a write refuses a part it would lose
/// rather than drop it.
///
/// Built with [`Default`] (off, no window, no text, to everyone) and the `with_*` methods, so a
/// field added later breaks no caller. Its `Debug` never shows the reply's words.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacationResponse {
    /// Whether the reply is switched on. A reply that is on sends only inside its
    /// [`window`](Self::window) ([`replies_at`](Self::replies_at)).
    pub enabled: bool,
    /// When the reply runs; open at both ends, it runs for as long as it is on.
    pub window: VacationWindow,
    /// The reply sent to the [`audience`](Self::audience), or, where
    /// [`outside_reply`](Self::outside_reply) is set, to senders inside the organization.
    pub reply: AutoReply,
    /// Who gets a reply at all.
    pub audience: ReplyAudience,
    /// A different reply for senders outside the organization (Graph's external reply), or
    /// `None` for one reply to every sender the audience admits.
    pub outside_reply: Option<AutoReply>,
    /// The version this reply was read at, which a write built on it names so that it is
    /// refused if the reply moved since; `None` on a reply built rather than read, or read from
    /// a transport that keeps none.
    pub version: Option<VacationVersion>,
    /// The provider's own form, as read; `None` on a reply built rather than read.
    pub raw: Option<RawVacation>,
}

impl VacationResponse {
    /// Switches the reply on (builder-style).
    #[must_use]
    pub fn turned_on(mut self) -> Self {
        self.enabled = true;
        self
    }

    /// Switches the reply off (builder-style). What it would say and when stays as it is, as
    /// every transport keeps it.
    #[must_use]
    pub fn turned_off(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Runs the reply in `window` (builder-style).
    #[must_use]
    pub fn with_window(mut self, window: VacationWindow) -> Self {
        self.window = window;
        self
    }

    /// Sends `reply` (builder-style).
    #[must_use]
    pub fn with_reply(mut self, reply: AutoReply) -> Self {
        self.reply = reply;
        self
    }

    /// Answers `audience` (builder-style).
    #[must_use]
    pub fn with_audience(mut self, audience: ReplyAudience) -> Self {
        self.audience = audience;
        self
    }

    /// Sends `reply` to senders outside the organization instead (builder-style).
    #[must_use]
    pub fn with_outside_reply(mut self, reply: AutoReply) -> Self {
        self.outside_reply = Some(reply);
        self
    }

    /// Carries the version it was read at (builder-style).
    #[must_use]
    pub fn with_version(mut self, version: VacationVersion) -> Self {
        self.version = Some(version);
        self
    }

    /// Carries the provider's own form (builder-style).
    #[must_use]
    pub fn with_raw(mut self, raw: RawVacation) -> Self {
        self.raw = Some(raw);
        self
    }

    /// Whether a message arriving at `at` gets a reply: the reply is on and `at` is inside its
    /// window, the start included and the end not (RFC 8621 §8: "on or after" `fromDate`, "before"
    /// `toDate`). The one answer to "is the out-of-office running now", so every host shows the
    /// same thing.
    #[must_use]
    pub fn replies_at(&self, at: UtcDateTime) -> bool {
        self.enabled && self.window.contains(at)
    }
}

/// When a reply runs: from [`start`](Self::start), until before [`end`](Self::end). An end left
/// open runs on, a start left open has run all along, and both open is no window at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct VacationWindow {
    /// The first instant a reply is sent, or `None` for no start.
    pub start: Option<UtcDateTime>,
    /// The first instant a reply is no longer sent, or `None` for no end.
    pub end: Option<UtcDateTime>,
}

impl VacationWindow {
    /// Starts the window at `at` (builder-style).
    #[must_use]
    pub fn with_start(mut self, at: UtcDateTime) -> Self {
        self.start = Some(at);
        self
    }

    /// Ends the window at `at` (builder-style).
    #[must_use]
    pub fn with_end(mut self, at: UtcDateTime) -> Self {
        self.end = Some(at);
        self
    }

    /// Whether it is no window at all: neither end set.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.start.is_none() && self.end.is_none()
    }

    /// Whether `at` is inside: on or after the start, and before the end.
    fn contains(self, at: UtcDateTime) -> bool {
        self.start.is_none_or(|start| start <= at) && self.end.is_none_or(|end| at < end)
    }
}

/// What a reply says: a subject and a body in plain text, HTML, or both.
///
/// Content: its `Debug` shows which parts are present and their length, never their text.
#[derive(Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AutoReply {
    /// The reply's subject, or `None` to let the server choose one (RFC 8621 §8).
    pub subject: Option<String>,
    /// The plain-text body.
    pub text: Option<String>,
    /// The HTML body, unsanitized as the server holds it; a host renders it through the
    /// engine's sanitiser like any mail HTML.
    pub html: Option<String>,
}

impl AutoReply {
    /// Carries `subject` (builder-style).
    #[must_use]
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Carries `text` as the plain-text body (builder-style).
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Carries `html` as the HTML body (builder-style).
    #[must_use]
    pub fn with_html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }

    /// Whether it says nothing: no subject and no body.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subject.is_none() && self.text.is_none() && self.html.is_none()
    }
}

impl fmt::Debug for AutoReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutoReply")
            .field("subject", &Length(self.subject.as_deref()))
            .field("text", &Length(self.text.as_deref()))
            .field("html", &Length(self.html.as_deref()))
            .finish()
    }
}

/// A part of a reply as its `Debug` shows it: whether it is there, and how long.
struct Length<'a>(Option<&'a str>);

impl fmt::Debug for Length<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(text) => write!(f, "Some({} bytes)", text.len()),
            None => f.write_str("None"),
        }
    }
}

/// Who gets a reply.
///
/// Every transport can answer everyone; the narrower audiences are Graph's and Gmail's
/// (`filters-and-out-of-office.md`, §3): Graph's `externalAudience` `all` is
/// [`Everyone`](Self::Everyone), `none` is [`Organization`](Self::Organization), and
/// `contactsOnly` is [`OrganizationAndContacts`](Self::OrganizationAndContacts); Gmail's
/// `restrictToContacts` is [`Contacts`](Self::Contacts) and `restrictToDomain` is
/// [`Organization`](Self::Organization).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ReplyAudience {
    /// Every sender.
    #[default]
    Everyone,
    /// Only senders in the account's contacts.
    Contacts,
    /// Only senders inside the account's organization.
    Organization,
    /// Senders inside the organization, and senders outside it who are in the account's
    /// contacts.
    OrganizationAndContacts,
}
