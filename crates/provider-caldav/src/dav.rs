//! Parsing a WebDAV `multistatus` response (RFC 4918 §14.16) into a structured,
//! prefix-agnostic form.
//!
//! Servers choose their own namespace prefixes (`D:`/`d:`, `A:`/`C:`/`cal:`), and
//! a property can be requested but absent — returned in a separate `propstat`
//! with a `404` status. So this parser matches on **local element names** (the
//! part after the prefix) and keeps only the properties from `2xx` `propstat`s.
//! A response carrying a response-level `404` status is a `sync-collection`
//! removal (RFC 6578). CDATA (the `calendar-data` payload) and entity-escaped
//! text are both handled by `quick-xml`.

use std::collections::{BTreeMap, BTreeSet};

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::CalDavError;

/// A parsed `multistatus`: its member responses and the top-level `sync-token`
/// (present on a `sync-collection` REPORT, RFC 6578).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MultiStatus {
    /// Each `<response>`, in document order.
    pub responses: Vec<DavResponse>,
    /// The `<sync-token>` reported for the whole collection, if any.
    pub sync_token: Option<String>,
}

/// One `<response>`: its href(s), an optional response-level status (a `404`
/// marks a `sync-collection` removal), and the properties from its `2xx`
/// `propstat`s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DavResponse {
    /// The resource href(s), URL-encoded as the server returned them. Usually one;
    /// RFC 4918 §14.16 allows several in a status-only response (e.g. a multi-href
    /// removal), so all are kept.
    pub hrefs: Vec<String>,
    /// The response-level HTTP status code, if the response carried one directly.
    pub status: Option<u16>,
    /// The successfully-read properties.
    pub props: Props,
}

impl DavResponse {
    /// The primary (first) href, or `""` when the response carried none. Used by
    /// single-resource consumers (a calendar collection, a changed object); the
    /// removal path iterates [`hrefs`](Self::hrefs) directly.
    pub(crate) fn href(&self) -> &str {
        self.hrefs.first().map_or("", String::as_str)
    }

    /// Whether this response reports the resource(s) as removed (a `sync-collection`
    /// `404`, RFC 6578 §3.4).
    pub(crate) fn is_removed(&self) -> bool {
        self.status.is_some_and(|status| status == 404)
    }
}

/// The properties read from a response's successful `propstat`s.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Props {
    /// Leaf/text (or inner-href) properties, keyed by lowercased local name.
    text: BTreeMap<String, String>,
    /// The local names of `<resourcetype>`'s child elements (e.g. `collection`,
    /// `calendar`).
    resourcetype: BTreeSet<String>,
}

impl Props {
    /// The value of the text (or inner-href) property `name`, if present.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.text.get(name).map(String::as_str)
    }

    /// Whether `<resourcetype>` marked this collection a CalDAV calendar.
    pub(crate) fn is_calendar(&self) -> bool {
        self.resourcetype.contains("calendar")
    }
}

/// Parses a `multistatus` XML document.
///
/// Whether a WebDAV `DAV:error` body contains a precondition **element** with the
/// given (lowercase) local name — e.g. `valid-sync-token` (RFC 6578 §3.2).
///
/// Matched as an XML element, not a raw substring, so a genuine `403` whose body
/// merely *mentions* the phrase in prose is not misclassified. Malformed XML
/// yields `false` (no precondition recognized).
pub(crate) fn has_precondition(body: &str, local: &str) -> bool {
    let mut reader = Reader::from_str(body);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == local {
                    return true;
                }
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// # Errors
///
/// Returns [`CalDavError::Xml`] on malformed XML.
pub(crate) fn parse_multistatus(xml: &str) -> Result<MultiStatus, CalDavError> {
    let mut reader = Reader::from_str(xml);
    let mut result = MultiStatus::default();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut response: Option<DavResponse> = None;
    let mut propstat: Option<(Option<u16>, Props)> = None;

    loop {
        match reader
            .read_event()
            .map_err(|e| CalDavError::xml(e.to_string()))?
        {
            Event::Eof => {
                // A truncated document (elements still open at EOF) must be an
                // error, never a partial result: a short snapshot would tombstone
                // resources the server never meant to remove.
                if !path.is_empty() {
                    return Err(CalDavError::xml("unexpected end of multistatus document"));
                }
                break;
            }
            Event::Start(start) => {
                let name = local_name(start.name().as_ref());
                if name == "response" {
                    response = Some(DavResponse::default());
                } else if name == "propstat" {
                    propstat = Some((None, Props::default()));
                }
                record_resourcetype_child(&path, &name, &mut propstat);
                path.push(name);
                text.clear();
            }
            Event::Empty(empty) => {
                // Self-closing elements (e.g. `<D:collection/>`) never push state;
                // only `<resourcetype>`'s children carry meaning here.
                let name = local_name(empty.name().as_ref());
                record_resourcetype_child(&path, &name, &mut propstat);
            }
            Event::Text(bytes) => text.push_str(
                &bytes
                    .unescape()
                    .map_err(|e| CalDavError::xml(e.to_string()))?,
            ),
            Event::CData(bytes) => {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
            Event::End(_) => {
                route_closed_element(
                    &path,
                    text.trim(),
                    &mut result,
                    &mut response,
                    &mut propstat,
                );
                if let Some(name) = path.pop() {
                    if name == "propstat" {
                        commit_propstat(&mut propstat, response.as_mut());
                    } else if name == "response"
                        && let Some(done) = response.take()
                    {
                        result.responses.push(done);
                    }
                }
                text.clear();
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Routes the trimmed text content of the element being closed to the right field
/// based on the element path (all lowercased local names).
fn route_closed_element(
    path: &[String],
    text: &str,
    result: &mut MultiStatus,
    response: &mut Option<DavResponse>,
    propstat: &mut Option<(Option<u16>, Props)>,
) {
    let Some(closing) = path.last() else { return };
    let parent = path.len().checked_sub(2).map(|i| path[i].as_str());

    match (closing.as_str(), parent) {
        ("href", Some("response")) => {
            if let Some(response) = response.as_mut()
                && !text.is_empty()
            {
                // Keep every response-level href (RFC 4918 §14.16 allows several).
                response.hrefs.push(text.to_owned());
            }
        }
        ("status", Some("response")) => {
            if let Some(response) = response.as_mut() {
                response.status = parse_http_status(text);
            }
        }
        ("status", Some("propstat")) => {
            if let Some((status, _)) = propstat.as_mut() {
                *status = parse_http_status(text);
            }
        }
        ("sync-token", Some("multistatus")) => result.sync_token = Some(text.to_owned()),
        _ => store_prop_text(path, text, propstat),
    }
}

/// Stores a property's text (or its inner `<href>`) inside the current propstat,
/// keyed by the property's local name.
fn store_prop_text(path: &[String], text: &str, propstat: &mut Option<(Option<u16>, Props)>) {
    let Some((_, props)) = propstat.as_mut() else {
        return;
    };
    let Some(prop_idx) = path.iter().position(|name| name == "prop") else {
        return;
    };
    let after = &path[prop_idx + 1..];
    let key = match after {
        // A direct leaf property: `<getetag>`, `<getctag>`, `<calendar-data>`, …
        [prop] => prop,
        // A property whose value is a nested `<href>`, at any depth — e.g.
        // `<current-user-principal><href>…` or a server that wraps it deeper like
        // `<current-user-principal><authenticated-as><href>…`.
        [prop, .., last] if last == "href" => prop,
        _ => return,
    };
    if !text.is_empty() {
        props.text.insert(key.clone(), text.to_owned());
    }
}

/// Records a `<resourcetype>` child (e.g. `calendar`) into the current propstat.
fn record_resourcetype_child(
    path: &[String],
    name: &str,
    propstat: &mut Option<(Option<u16>, Props)>,
) {
    if path.last().map(String::as_str) == Some("resourcetype")
        && let Some((_, props)) = propstat.as_mut()
    {
        props.resourcetype.insert(name.to_owned());
    }
}

/// Merges a finished propstat's properties into the response, but only when its
/// status was a success (RFC 4918 §14.22: a `404` propstat lists absent props).
fn commit_propstat(
    propstat: &mut Option<(Option<u16>, Props)>,
    response: Option<&mut DavResponse>,
) {
    let Some((status, props)) = propstat.take() else {
        return;
    };
    let succeeded = status.is_none_or(|code| (200..300).contains(&code));
    if let (true, Some(response)) = (succeeded, response) {
        response.props.text.extend(props.text);
        response.props.resourcetype.extend(props.resourcetype);
    }
}

/// The local part of a possibly-prefixed XML name, lowercased
/// (`D:calendar-home-set` → `calendar-home-set`).
fn local_name(qualified: &[u8]) -> String {
    let local = qualified
        .iter()
        .rposition(|&b| b == b':')
        .map_or(qualified, |i| &qualified[i + 1..]);
    String::from_utf8_lossy(local).to_ascii_lowercase()
}

/// Extracts the numeric code from an HTTP status line (`HTTP/1.1 200 OK` → 200).
fn parse_http_status(line: &str) -> Option<u16> {
    line.split_whitespace()
        .find_map(|token| token.parse::<u16>().ok())
        .filter(|code| (100..600).contains(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calendar_home_listing_skipping_404_props() {
        let xml = include_str!("../tests/fixtures/calendar-home.xml");
        let parsed = parse_multistatus(xml).unwrap();
        // The home itself (a plain collection) plus the default calendar.
        assert_eq!(parsed.responses.len(), 2);
        let calendar = parsed
            .responses
            .iter()
            .find(|r| r.props.is_calendar())
            .expect("the default calendar is a CalDAV calendar");
        assert_eq!(calendar.href(), "/dav/cal/alice%40test.local/default/");
        assert_eq!(
            calendar.props.get("displayname"),
            Some("Stalwart Calendar (alice@test.local)")
        );
        // The CTag came back; the unsupported calendar-color was a 404 propstat
        // and must not leak into the props.
        assert_eq!(calendar.props.get("getctag"), Some("\"22\""));
        assert_eq!(calendar.props.get("calendar-color"), None);
        // The home href is a collection but not a calendar.
        let home = parsed
            .responses
            .iter()
            .find(|r| !r.props.is_calendar())
            .unwrap();
        assert!(!home.props.is_calendar());
    }

    #[test]
    fn parses_principal_and_home_hrefs() {
        let xml = include_str!("../tests/fixtures/principal.xml");
        let parsed = parse_multistatus(xml).unwrap();
        let response = &parsed.responses[0];
        assert_eq!(
            response.props.get("current-user-principal"),
            Some("/dav/pal/alice%40test.local/")
        );
        assert_eq!(
            response.props.get("calendar-home-set"),
            Some("/dav/cal/alice%40test.local/")
        );
    }

    #[test]
    fn parses_sync_collection_with_etags_and_cdata_calendar_data() {
        let xml = include_str!("../tests/fixtures/sync-initial.xml");
        let parsed = parse_multistatus(xml).unwrap();
        assert_eq!(
            parsed.sync_token.as_deref(),
            Some("urn:stalwart:davsync:16")
        );
        // The collection self-response (no calendar-data) plus six resources.
        let resources: Vec<_> = parsed
            .responses
            .iter()
            .filter(|r| r.props.get("calendar-data").is_some())
            .collect();
        assert_eq!(resources.len(), 6);
        let oneoff = resources
            .iter()
            .find(|r| r.href().ends_with("oneoff-2001.ics"))
            .unwrap();
        assert!(oneoff.props.get("getetag").is_some());
        // The CDATA iCalendar survived intact.
        let data = oneoff.props.get("calendar-data").unwrap();
        assert!(data.contains("UID:oneoff-2001@test.local"));
        assert!(data.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn parses_noop_delta_token_with_no_responses() {
        let xml = include_str!("../tests/fixtures/sync-noop.xml");
        let parsed = parse_multistatus(xml).unwrap();
        assert!(parsed.responses.is_empty());
        assert_eq!(
            parsed.sync_token.as_deref(),
            Some("urn:stalwart:davsync:16")
        );
    }

    #[test]
    fn recognizes_a_removal_response() {
        // A sync-collection delta reports a deleted resource as a 404 response.
        let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/cal/gone.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>t2</D:sync-token></D:multistatus>";
        let parsed = parse_multistatus(xml).unwrap();
        assert_eq!(parsed.responses.len(), 1);
        assert!(parsed.responses[0].is_removed());
        assert_eq!(parsed.responses[0].href(), "/cal/gone.ics");
    }

    #[test]
    fn captures_a_property_value_nested_below_a_single_href() {
        // A server that wraps current-user-principal deeper than a direct <href>
        // must still yield the href (else discovery fails to find the principal).
        let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/</D:href><D:propstat><D:prop><D:current-user-principal><D:authenticated-as><D:href>/principals/u/</D:href></D:authenticated-as></D:current-user-principal></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>";
        let parsed = parse_multistatus(xml).unwrap();
        assert_eq!(
            parsed.responses[0].props.get("current-user-principal"),
            Some("/principals/u/")
        );
    }

    #[test]
    fn keeps_every_href_in_a_multi_href_response() {
        // RFC 4918 §14.16: a status-only response may cover several hrefs; a
        // multi-href removal must tombstone all of them, not just the first.
        let xml = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/a.ics</D:href><D:href>/b.ics</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response><D:sync-token>t2</D:sync-token></D:multistatus>";
        let parsed = parse_multistatus(xml).unwrap();
        assert!(parsed.responses[0].is_removed());
        assert_eq!(parsed.responses[0].hrefs, vec!["/a.ics", "/b.ics"]);
        assert_eq!(parsed.responses[0].href(), "/a.ics");
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(parse_multistatus("<D:multistatus><unclosed>").is_err());
    }
}
