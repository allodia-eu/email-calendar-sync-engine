//! Event kinds.

open_enum! {
    /// The kind of an event, normalizing provider event types (Google
    /// `eventType`, Microsoft Graph focus/OOF events) onto one discriminator.
    ///
    /// The kind is recorded even when the JSCalendar projection cannot express
    /// its behavior; the kind-specific payload (working-location details,
    /// out-of-office auto-decline settings, …) is preserved in the event's
    /// extended properties (`modeling.md`).
    EventKind {
        /// A regular event.
        Default => "default",
        /// A birthday or anniversary (typically an annual all-day event).
        Birthday => "birthday",
        /// Focus time.
        FocusTime => "focus-time",
        /// Out of office.
        OutOfOffice => "out-of-office",
        /// A working-location event.
        WorkingLocation => "working-location",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kinds_have_canonical_names() {
        assert_eq!(EventKind::OutOfOffice.as_str(), "out-of-office");
        assert_eq!(EventKind::WorkingLocation.as_str(), "working-location");
    }

    #[test]
    fn unknown_kind_preserved() {
        let kind = EventKind::from_wire("fromGmail");
        assert_eq!(kind, EventKind::Other("fromGmail".into()));
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(serde_json::from_str::<EventKind>(&json).unwrap(), kind);
    }
}
