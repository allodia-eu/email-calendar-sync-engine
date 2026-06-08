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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    mail: bool,
    submission: bool,
    calendars: bool,
}

impl Capabilities {
    /// No capabilities (the starting point for the builder).
    #[must_use]
    pub const fn none() -> Self {
        Self {
            mail: false,
            submission: false,
            calendars: false,
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
    }

    #[test]
    fn full_capability_set() {
        let caps = Capabilities::none()
            .with_mail()
            .with_submission()
            .with_calendars();
        assert!(caps.mail() && caps.submission() && caps.calendars());
    }
}
