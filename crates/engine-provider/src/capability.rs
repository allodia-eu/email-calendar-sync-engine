//! What an adapter can do.
//!
//! The engine queries capabilities from the adapter and routes work accordingly;
//! callers must not switch on provider kind (`providers.md`). This is the minimal
//! set the step-4 mail spine and calendar-read slice need — the data domains a
//! provider exposes. It maps directly onto the JMAP session's advertised
//! capability URNs (`urn:ietf:params:jmap:mail` → [`Capabilities::mail`], etc.)
//! and grows as protocol features are added.

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
    submission: bool,
    calendars: bool,
    calendar_writes: bool,
}

impl Capabilities {
    /// No capabilities (the starting point for the builder).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            mail: false,
            submission: false,
            calendars: false,
            calendar_writes: false,
        }
    }

    /// Marks mail read/sync as supported.
    #[must_use]
    pub const fn with_mail(mut self) -> Self {
        self.mail = true;
        self
    }

    /// Marks mail submission (`EmailSubmission`) as supported.
    #[must_use]
    pub const fn with_submission(mut self) -> Self {
        self.submission = true;
        self
    }

    /// Marks calendar read/sync as supported.
    #[must_use]
    pub const fn with_calendars(mut self) -> Self {
        self.calendars = true;
        self
    }

    /// Marks calendar **writes** (create/update/delete event resources) as
    /// supported. Distinct from [`with_calendars`](Self::with_calendars), the read
    /// capability — a calendar the account can read but not write (a shared
    /// read-only CalDAV collection, or a calendar-read-only adapter) advertises
    /// [`calendars`](Self::calendars) without this, exactly as a mail adapter with
    /// no SMTP advertises [`mail`](Self::mail) without [`submission`](Self::submission).
    #[must_use]
    pub const fn with_calendar_writes(mut self) -> Self {
        self.calendar_writes = true;
        self
    }

    /// Whether mail read/sync is supported.
    #[must_use]
    pub const fn mail(self) -> bool {
        self.mail
    }

    /// Whether mail submission is supported.
    #[must_use]
    pub const fn submission(self) -> bool {
        self.submission
    }

    /// Whether calendar read/sync is supported.
    #[must_use]
    pub const fn calendars(self) -> bool {
        self.calendars
    }

    /// Whether calendar writes (create/update/delete event resources) are
    /// supported.
    #[must_use]
    pub const fn calendar_writes(self) -> bool {
        self.calendar_writes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_sets_each_flag_independently() {
        assert_eq!(Capabilities::none(), Capabilities::default());
        let caps = Capabilities::none().with_mail().with_calendars();
        assert!(caps.mail());
        assert!(caps.calendars());
        assert!(!caps.submission());
        assert!(!caps.calendar_writes());
    }

    #[test]
    fn full_capability_set() {
        let caps = Capabilities::none()
            .with_mail()
            .with_submission()
            .with_calendars()
            .with_calendar_writes();
        assert!(caps.mail() && caps.submission() && caps.calendars() && caps.calendar_writes());
    }

    #[test]
    fn calendar_writes_is_independent_of_read() {
        // A read-only calendar advertises `calendars` without `calendar_writes`,
        // exactly as a no-SMTP mail adapter advertises `mail` without `submission`.
        let read_only = Capabilities::none().with_calendars();
        assert!(read_only.calendars() && !read_only.calendar_writes());
    }
}
