//! The parser's error type.

/// An iCalendar text that could not be parsed, or a value the engine model cannot
/// represent.
///
/// One flat variant on purpose: every caller's recovery is the same — skip this
/// resource and keep syncing the rest — so a taxonomy of parse failures would carry
/// no decision. The message names the offending property and value so the failure is
/// diagnosable from a log line. Transports map it into their own error (CalDAV maps
/// it to `CalDavError::Ical`, classified `Permanent`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("icalendar error: {0}")]
pub struct IcalError(String);

impl IcalError {
    /// Creates an error describing why the text or value was rejected.
    #[must_use]
    pub fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }

    /// Returns the failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.0
    }
}
