//! Finding the meetings an event names outside the fields built for them.
//!
//! An invitation from another calendar often carries its meeting only as text: Google's and
//! Microsoft's invitations put the link in a vendor property and the description, and a
//! meeting add-on writes it into the location. [`add_detected`] reads those after the
//! adapter has read the structured fields, so each meeting is one conference however many
//! places name it.

use super::{
    Conference, ConferenceKey, ConferenceOrigin, ConferenceService, ConferenceUri, DetectedIn,
    EntryPoint, Location,
};

/// The places besides its conference fields where an event names its meetings.
#[derive(Debug, Clone, Copy, Default)]
pub struct DetectionSources<'a> {
    /// The URIs of the event's vendor properties (`X-GOOGLE-CONFERENCE`,
    /// `X-MICROSOFT-SKYPETEAMSMEETINGURL`), in the order the event holds them.
    pub vendor: &'a [ConferenceUri],
    /// The event's locations.
    pub locations: &'a [Location],
    /// The description as the server holds it, plain text or HTML.
    pub description: Option<&'a str>,
}

/// Adds to `conferences` every meeting `sources` name that none of them is already.
///
/// The places are read strongest first, each after the ones before it: a vendor property, a
/// location (one that is only a web link, whatever it links to, or a meeting link inside
/// one), then a meeting link in the description. In a description only a link
/// [`ConferenceService::recognise`] names counts, since a description links to anything. A
/// link that joins a meeting already held adds nothing, so a meeting keeps the strongest
/// origin any place gives it. It joins one when it is the same meeting by
/// [`ConferenceUri::same_meeting`], or when it runs on the service of a meeting the calendar
/// server made: that is the event's one meeting there, and the server may write it into the
/// text under another link (Teams puts a short `meet/<id>` link in the body of a meeting whose
/// `joinUrl` is its long `meetup-join` one). Each detected meeting is one video way in, keyed
/// by its link.
pub fn add_detected(conferences: &mut Vec<Conference>, sources: &DetectionSources<'_>) {
    let vendor = sources
        .vendor
        .iter()
        .cloned()
        .map(|uri| (uri, DetectedIn::VendorProperty));
    let located = sources
        .locations
        .iter()
        .filter_map(|location| location.name.as_deref())
        .flat_map(location_links)
        .map(|uri| (uri, DetectedIn::Location));
    let described = sources
        .description
        .into_iter()
        .flat_map(links)
        .filter(|uri| ConferenceService::recognise(uri).is_some())
        .map(|uri| (uri, DetectedIn::Description));
    for (uri, place) in vendor.chain(located).chain(described) {
        let service = ConferenceService::recognise(&uri);
        let held = conferences.iter().any(|conference| {
            let servers_own = service.is_some()
                && conference.service == service
                && matches!(conference.origin, ConferenceOrigin::Native { .. });
            servers_own
                || conference
                    .entry_points
                    .iter()
                    .any(|entry| entry.uri.same_meeting(&uri))
        });
        if !held {
            conferences.push(Conference {
                key: ConferenceKey::new(uri.as_str()),
                service,
                origin: ConferenceOrigin::Detected { place },
                entry_points: vec![EntryPoint::at(uri)],
                label: None,
                meeting_id: None,
                notes: None,
            });
        }
    }
}

/// The meetings a location names: the whole text when it is one web link, else each
/// meeting link in it.
fn location_links(name: &str) -> Vec<ConferenceUri> {
    match ConferenceUri::parse(name) {
        Ok(uri) if uri.scheme().is_web() => vec![uri],
        _ => links(name)
            .filter(|uri| ConferenceService::recognise(uri).is_some())
            .collect(),
    }
}

/// Every web link in `text`, which may be plain text or HTML.
///
/// A link starts at `https://` or `http://` and ends at whitespace or a character no link
/// holds unescaped: an angle bracket (Exchange's plain text writes `text<https://…>`), a quote
/// (an HTML attribute) or a pipe. Punctuation that ends a sentence is not part of it, and
/// `&amp;`, as HTML writes `&` inside a link, is read as `&`.
fn links(text: &str) -> impl Iterator<Item = ConferenceUri> + '_ {
    let lower = text.to_ascii_lowercase();
    let mut starts: Vec<usize> = lower
        .match_indices("https://")
        .chain(lower.match_indices("http://"))
        .map(|(at, _)| at)
        .collect();
    starts.sort_unstable();
    starts.into_iter().filter_map(move |at| {
        let tail = &text[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '|'))
            .unwrap_or(tail.len());
        let link = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '*']);
        ConferenceUri::parse(&link.replace("&amp;", "&")).ok()
    })
}

#[cfg(test)]
#[path = "conference_detect_tests.rs"]
mod tests;
