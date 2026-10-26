//! The components a calendar collection holds (RFC 4791 §5.2.3), read off its listing: what
//! decides whether a collection is a calendar of events or a list of tasks.

use std::collections::BTreeSet;

use quick_xml::events::BytesStart;

use super::Props;

impl Props {
    /// Whether the collection can hold events: its `supported-calendar-component-set` names
    /// `VEVENT`, or it reported none, which RFC 4791 §5.2.3 reads as every component type. A
    /// collection of tasks alone (iCloud's reminders) answers `false`.
    pub(crate) fn holds_events(&self) -> bool {
        self.components
            .as_ref()
            .is_none_or(|components| components.contains("VEVENT"))
    }
}

/// Records `supported-calendar-component-set` into the current propstat: its presence when the
/// element opens, and each `<comp name="…"/>` inside it.
pub(super) fn record_component(
    path: &[String],
    name: &str,
    element: &BytesStart<'_>,
    propstat: &mut Option<(Option<u16>, Props)>,
) {
    let Some((_, props)) = propstat.as_mut() else {
        return;
    };
    match path.last().map(String::as_str) {
        Some("prop") if name == "supported-calendar-component-set" => {
            props.components.get_or_insert_with(BTreeSet::new);
        }
        Some("supported-calendar-component-set") if name == "comp" => {
            if let Ok(Some(attribute)) = element.try_get_attribute("name") {
                props
                    .components
                    .get_or_insert_with(BTreeSet::new)
                    .insert(attribute.value.to_ascii_uppercase());
            }
        }
        _ => {}
    }
}
