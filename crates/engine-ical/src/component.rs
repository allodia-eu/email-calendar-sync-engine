//! Building the iCalendar component tree from content lines (RFC 5545 §3.6).
//!
//! `BEGIN:X` … `END:X` blocks nest into a tree of [`Component`]s. Parsing is
//! tolerant: properties outside any component are ignored, an unmatched `END` is
//! ignored, and components left open at end-of-input are still emitted, so a
//! truncated or malformed resource yields as much structure as it can.

use super::unfold::{ContentLine, content_lines};

/// A parsed iCalendar component (a `VCALENDAR`, `VEVENT`, `VTIMEZONE`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Component {
    /// The component name, uppercased.
    pub name: String,
    /// The direct properties of this component, in source order.
    pub properties: Vec<ContentLine>,
    /// The nested sub-components, in source order.
    pub children: Vec<Component>,
}

impl Component {
    /// The first property named `name` (case-insensitive).
    pub(crate) fn property(&self, name: &str) -> Option<&ContentLine> {
        self.properties
            .iter()
            .find(|line| line.name.eq_ignore_ascii_case(name))
    }

    /// The raw value of the first property named `name`.
    pub(crate) fn value(&self, name: &str) -> Option<&str> {
        self.property(name).map(|line| line.value.as_str())
    }

    /// Every property named `name` (case-insensitive), in source order.
    pub(crate) fn all_properties<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a ContentLine> {
        self.properties
            .iter()
            .filter(move |line| line.name.eq_ignore_ascii_case(name))
    }

    /// Every direct child component named `name` (case-insensitive).
    pub(crate) fn children_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Component> {
        self.children
            .iter()
            .filter(move |child| child.name.eq_ignore_ascii_case(name))
    }
}

/// Parses `text` into its top-level components (normally a single `VCALENDAR`).
pub(crate) fn parse_components(text: &str) -> Vec<Component> {
    let mut roots = Vec::new();
    let mut stack: Vec<Component> = Vec::new();
    for line in content_lines(text) {
        match line.name.as_str() {
            "BEGIN" => stack.push(Component {
                name: line.value.to_ascii_uppercase(),
                properties: Vec::new(),
                children: Vec::new(),
            }),
            "END" => close_top(&mut stack, &mut roots),
            _ => {
                if let Some(top) = stack.last_mut() {
                    top.properties.push(line);
                }
            }
        }
    }
    // Flush components left unclosed by a truncated resource.
    while !stack.is_empty() {
        close_top(&mut stack, &mut roots);
    }
    roots
}

/// Pops the innermost open component and attaches it to its parent, or to the
/// roots when the stack empties.
fn close_top(stack: &mut Vec<Component>, roots: &mut Vec<Component>) {
    if let Some(done) = stack.pop() {
        match stack.last_mut() {
            Some(parent) => parent.children.push(done),
            None => roots.push(done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Amsterdam\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:a@x\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:b@x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn nests_components_and_collects_properties() {
        let roots = parse_components(SAMPLE);
        assert_eq!(roots.len(), 1);
        let vcal = &roots[0];
        assert_eq!(vcal.name, "VCALENDAR");
        assert_eq!(vcal.value("VERSION"), Some("2.0"));
        assert_eq!(vcal.children_named("VTIMEZONE").count(), 1);
        let events: Vec<_> = vcal.children_named("VEVENT").collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].value("UID"), Some("a@x"));
        assert_eq!(events[1].value("UID"), Some("b@x"));
    }

    #[test]
    fn unclosed_component_is_still_emitted() {
        // A VEVENT with no END (truncated) still yields its structure.
        let roots = parse_components("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x@y\n");
        let vcal = &roots[0];
        let event = vcal.children_named("VEVENT").next().unwrap();
        assert_eq!(event.value("UID"), Some("x@y"));
    }

    #[test]
    fn stray_end_and_loose_properties_are_ignored() {
        let roots = parse_components("END:VEVENT\nLOOSE:prop\nBEGIN:VCALENDAR\nEND:VCALENDAR\n");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "VCALENDAR");
        assert!(roots[0].properties.is_empty());
    }
}
