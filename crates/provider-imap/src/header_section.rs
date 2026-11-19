//! Reading fields out of the raw header section a metadata fetch returns
//! (`BODY[HEADER.FIELDS (…)]`).

use engine_core::mail::{Importance, ImportanceHeaders};

/// The unfolded value of every occurrence of the field `name` in a raw header section, in the
/// order the message carries them. A line that starts with whitespace continues the field before
/// it (RFC 5322 §2.2.3).
pub(crate) fn header_fields(section: &str, name: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current: Option<String> = None;
    for line in section.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(value) = current.as_mut() {
                // A value that starts on the continuation line has nothing before it to separate.
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        values.extend(current.take());
        let Some((field, rest)) = line.split_once(':') else {
            continue;
        };
        if field.trim().eq_ignore_ascii_case(name) {
            current = Some(rest.trim().to_owned());
        }
    }
    values.extend(current);
    values
}

/// The first occurrence of the field `name`, unfolded, or `None` when the section has none.
pub(crate) fn header_field(section: &str, name: &str) -> Option<String> {
    header_fields(section, name).into_iter().next()
}

/// The importance the section's headers state, read by the engine's one rule from the last
/// occurrence of each ([`ImportanceHeaders`]).
pub(crate) fn importance(section: &str) -> Importance {
    let last: Vec<(&str, Option<String>)> = Importance::HEADERS
        .iter()
        .map(|name| (*name, header_fields(section, name).pop()))
        .collect();
    Importance::from_headers(ImportanceHeaders::from_lookup(|name| {
        last.iter()
            .find(|(header, _)| *header == name)
            .and_then(|(_, value)| value.as_deref())
    }))
}

#[cfg(test)]
#[path = "header_section_tests.rs"]
mod tests;
