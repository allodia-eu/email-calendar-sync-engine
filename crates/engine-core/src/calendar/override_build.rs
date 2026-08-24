//! Assembling the per-occurrence override patch every transport folds into.
//!
//! An occurrence the user changed arrives four different ways — a `RECURRENCE-ID` `VEVENT`,
//! a JSCalendar `recurrenceOverrides` entry, a Graph `exception`, a Google entry carrying
//! `recurringEventId` — and reaches the expander as one
//! [`RecurrenceOverride`](super::RecurrenceOverride) either way. That only holds if the
//! adapters agree on *which* fields they carry and on how each is spelled, and three of them
//! were assembling the same JSCalendar keys by hand. This is that field set, stated once.
//!
//! JMAP is deliberately not a caller: its server hands over a JSCalendar patch already, and
//! passing it through carries strictly more than this builder knows how to name. What the
//! builder is for is the three transports that state a whole instance and have to be
//! projected down to a patch.
//!
//! Keys are **JSCalendar** (RFC 8984), because that is what the expander reads and what
//! JMAP's pass-through produces — so an override reads the same whichever door it came in.

use serde_json::{Value, json};

use super::{Location, RecurrenceOverride};
use crate::{
    patch::{PatchError, PatchObject},
    time::{CalendarDateTime, Duration, LocalDateTime},
};

/// The id a synthesized single-location map is keyed by.
///
/// JSCalendar location ids are arbitrary strings the event owns, and an instance projected
/// from a transport with one scalar location has no id of its own to preserve. A fixed one
/// keeps the projection deterministic — two syncs of an unchanged occurrence produce equal
/// patches, which a store comparing them depends on.
const PROJECTED_LOCATION_ID: &str = "loc";

/// Builds the JSCalendar patch describing what one occurrence changed about itself.
///
/// Every setter is optional and order does not matter; a field never set is a field the
/// occurrence did not change, which is what lets it keep following the master.
#[derive(Debug, Default, Clone)]
pub struct OverrideBuilder {
    fields: Vec<(String, Value)>,
}

impl OverrideBuilder {
    /// An empty patch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the occurrence moved to, as its wall clock plus the zone that reads it.
    ///
    /// An **all-day** date contributes its midnight, which is the wall clock the expander
    /// keys and resolves one at. Reading it through
    /// [`CalendarDateTime::local`] instead would answer `None` and write no `start` at all,
    /// so an all-day occurrence dragged to another date would come back on its original one
    /// — the expander falls back to the recurrence id when a patch names no start.
    #[must_use]
    pub fn start(mut self, start: &CalendarDateTime) -> Self {
        let local = match start {
            CalendarDateTime::Floating(local) | CalendarDateTime::Zoned { local, .. } => *local,
            CalendarDateTime::Date(date) => {
                LocalDateTime::new(date.year(), date.month(), date.day(), 0, 0, 0)
                    .unwrap_or_else(|_| unreachable!("a CalendarDate always holds a valid date"))
            }
        };
        self.fields
            .push(("start".to_owned(), json!(local.to_string())));
        if let Some(zone) = start.zone() {
            self.fields
                .push(("timeZone".to_owned(), json!(zone.as_str())));
        }
        self
    }

    /// How long the occurrence runs, when it differs from the series.
    #[must_use]
    pub fn duration(mut self, duration: Duration) -> Self {
        self.fields
            .push(("duration".to_owned(), json!(duration.to_string())));
        self
    }

    /// What the occurrence is called.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.fields.push(("title".to_owned(), json!(title.into())));
        self
    }

    /// The occurrence's own notes.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.fields
            .push(("description".to_owned(), json!(description.into())));
        self
    }

    /// The occurrence's own location, by name.
    ///
    /// JSCalendar has no scalar location — `locations` is a map of id → `Location` (RFC 8984
    /// §4.2.5) — so a transport carrying one piece of text still projects a map, and reads
    /// the same as the JMAP pass-through beside it.
    #[must_use]
    pub fn location_named(self, name: impl Into<String>) -> Self {
        self.location(&Location::named(name))
    }

    /// The occurrence's own location, from a projected [`Location`].
    ///
    /// Only the name and coordinates travel: they are what every transport in this workspace
    /// can state about an instance, and a field invented here would differ per transport,
    /// which is the one thing this builder exists to prevent.
    #[must_use]
    pub fn location(mut self, location: &Location) -> Self {
        let mut entry = serde_json::Map::new();
        entry.insert("@type".to_owned(), json!("Location"));
        if let Some(name) = &location.name {
            entry.insert("name".to_owned(), json!(name));
        }
        if let Some(coordinates) = &location.coordinates {
            entry.insert("coordinates".to_owned(), json!(coordinates));
        }
        self.fields.push((
            "locations".to_owned(),
            json!({ PROJECTED_LOCATION_ID: Value::Object(entry) }),
        ));
        self
    }

    /// The organizer called this occurrence off.
    ///
    /// Distinct from [`RecurrenceOverride::Excluded`], which is the occurrence being removed
    /// from the set altogether: a cancelled instance is still *in* the series, and a host
    /// that shows cancelled meetings can still draw it.
    #[must_use]
    pub fn cancelled(mut self) -> Self {
        self.fields.push(("status".to_owned(), json!("cancelled")));
        self
    }

    /// Finishes the patch.
    ///
    /// # Errors
    ///
    /// Returns [`PatchError`] if the assembled fields are not a well-formed patch.
    pub fn build(self) -> Result<RecurrenceOverride, PatchError> {
        PatchObject::new(self.fields).map(RecurrenceOverride::Patch)
    }
}

#[cfg(test)]
#[path = "override_build_tests.rs"]
mod tests;
