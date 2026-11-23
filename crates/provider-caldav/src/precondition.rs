//! What a WebDAV error body names inside a precondition.

use quick_xml::{Reader, events::Event, name::LocalName};

/// The `DAV:href` inside the precondition element `local` of an error body, if it names one:
/// the resource that already holds a `UID`, in a `no-uid-conflict` (RFC 4791 §5.3.2.1, RFC 6352
/// §6.3.2.1). Whether the body names the precondition at all is
/// [`has_precondition`](crate::dav::has_precondition).
pub(crate) fn precondition_href<T: for<'a> TryFrom<&'a str>>(body: &str, local: &str) -> Option<T> {
    let mut reader = Reader::from_str(body);
    let mut inside = false;
    let mut in_href = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if named(element.local_name(), local) {
                    inside = true;
                } else if inside && named(element.local_name(), "href") {
                    in_href = true;
                }
            }
            Ok(Event::Text(text)) if in_href => return T::try_from(text.trim()).ok(),
            Ok(Event::End(element)) if named(element.local_name(), local) => inside = false,
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Whether an element's local name is `want`, in any case.
fn named(name: LocalName<'_>, want: &str) -> bool {
    name.into_inner().eq_ignore_ascii_case(want)
}

#[cfg(test)]
mod tests {
    use super::precondition_href;

    #[test]
    fn the_holder_of_a_uid_is_read_from_the_refusal() {
        // Stalwart's answer to a second object with a `UID` (observed live, v1.0.2).
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><D:error xmlns:D="DAV:" xmlns:A="urn:ietf:params:xml:ns:caldav"><A:no-uid-conflict><D:href>/dav/cal/bob%40test.local/work/h1.ics</D:href></A:no-uid-conflict></D:error>"#;
        assert_eq!(
            precondition_href::<String>(body, "no-uid-conflict").as_deref(),
            Some("/dav/cal/bob%40test.local/work/h1.ics")
        );
    }

    #[test]
    fn a_body_naming_no_holder_names_none() {
        assert_eq!(
            precondition_href::<String>(
                "<D:error xmlns:D=\"DAV:\"><D:href>/x</D:href></D:error>",
                "no-uid-conflict"
            ),
            None
        );
        assert_eq!(
            precondition_href::<String>("not xml <", "no-uid-conflict"),
            None
        );
        assert_eq!(precondition_href::<String>("", "no-uid-conflict"), None);
    }
}
