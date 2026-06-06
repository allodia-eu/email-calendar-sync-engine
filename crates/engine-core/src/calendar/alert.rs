//! Event alerts (reminders).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::RelativeTo;
use crate::time::{SignedDuration, UtcDateTime};

open_enum! {
    /// What an alert does when it fires (JSCalendar `Alert.action`,
    /// RFC 8984 §4.5.2). Defaults to `display`.
    AlertAction {
        /// Show the alert to the user.
        Display => "display",
        /// Send an email.
        Email => "email",
    }
}

/// When an alert fires (JSCalendar trigger types, RFC 8984 §4.5.2).
///
/// The "`relativeTo` is only valid on a relative trigger" rule is structural:
/// only [`Trigger::Offset`] carries it. An unrecognized trigger type is kept in
/// [`Trigger::Unknown`] and **must not fire** — it is preserved so it survives a
/// round-trip, nothing more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Trigger {
    /// Fires at an offset from the event's start or end. A negative offset is
    /// before the anchor.
    Offset {
        /// The signed offset from the anchor.
        offset: SignedDuration,
        /// The anchor the offset is relative to.
        relative_to: RelativeTo,
    },
    /// Fires at an absolute instant.
    Absolute {
        /// The instant the alert fires.
        when: UtcDateTime,
    },
    /// An unrecognized trigger, preserved verbatim and never fired.
    Unknown(Value),
}

impl Trigger {
    /// A trigger that fires `before` the event start.
    #[must_use]
    pub fn before_start(before: crate::time::Duration) -> Self {
        Self::Offset {
            offset: SignedDuration::before(before),
            relative_to: RelativeTo::Start,
        }
    }
}

/// An alert/reminder on an event (JSCalendar `Alert`, RFC 8984 §4.5.2).
///
/// Does not implement `Eq`: an unknown trigger holds arbitrary JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// When the alert fires.
    pub trigger: Trigger,
    /// What the alert does.
    pub action: AlertAction,
    /// When the user acknowledged the alert, if they have.
    pub acknowledged: Option<UtcDateTime>,
}

impl Alert {
    /// Creates a display alert with the given trigger.
    #[must_use]
    pub fn display(trigger: Trigger) -> Self {
        Self {
            trigger,
            action: AlertAction::Display,
            acknowledged: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{Duration, SignedDuration};

    #[test]
    fn offset_trigger_carries_anchor() {
        let fifteen = "PT15M".parse::<Duration>().unwrap();
        let alert = Alert::display(Trigger::before_start(fifteen));
        assert_eq!(
            alert.trigger,
            Trigger::Offset {
                offset: SignedDuration::before(fifteen),
                relative_to: RelativeTo::Start,
            }
        );
        assert_eq!(alert.action, AlertAction::Display);
    }

    #[test]
    fn unknown_trigger_is_preserved() {
        let raw = serde_json::json!({ "@type": "ProximityTrigger", "method": "leave" });
        let alert = Alert {
            trigger: Trigger::Unknown(raw.clone()),
            action: AlertAction::Other("audio".into()),
            acknowledged: None,
        };
        let json = serde_json::to_string(&alert).unwrap();
        let back: Alert = serde_json::from_str(&json).unwrap();
        assert_eq!(back, alert);
        assert!(matches!(back.trigger, Trigger::Unknown(_)));
    }

    #[test]
    fn absolute_trigger_roundtrips() {
        let alert = Alert::display(Trigger::Absolute {
            when: "2021-06-01T08:45:00Z".parse().unwrap(),
        });
        let json = serde_json::to_string(&alert).unwrap();
        assert_eq!(serde_json::from_str::<Alert>(&json).unwrap(), alert);
    }
}
