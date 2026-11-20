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
/// origin any place gives it; which meeting it joins is [`Conference::joined_at`]. Each
/// detected meeting is one video way in, keyed by its link, and takes the fullest link any
/// place writes for it ([`ConferenceUri::fuller_than`]), so a location's bare Zoom link gives
/// way to the description's one with its passcode. A meeting stored in a conference field or
/// made by the server keeps its own link.
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
        .map(|uri| (uri, DetectedIn::Description));
    for (uri, place) in vendor.chain(located).chain(described) {
        let service = ConferenceService::recognise(&uri);
        if place == DetectedIn::Description && service.is_none() {
            continue;
        }
        let held = conferences.iter_mut().any(|conference| {
            conference.claims_service(service.as_ref()) || hold_same_meeting(conference, &uri)
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

/// Whether `conference` already has a way in that joins the meeting at `uri`. A detected
/// conference whose way in `uri` is fuller than takes `uri` in its place.
fn hold_same_meeting(conference: &mut Conference, uri: &ConferenceUri) -> bool {
    let detected = matches!(conference.origin, ConferenceOrigin::Detected { .. });
    let Some(entry) = conference
        .entry_points
        .iter_mut()
        .find(|entry| entry.uri.same_meeting(uri))
    else {
        return false;
    };
    if detected && uri.fuller_than(&entry.uri) {
        if conference.key.as_str() == entry.uri.as_str() {
            conference.key = ConferenceKey::new(uri.as_str());
        }
        entry.uri = uri.clone();
    }
    true
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
/// (an HTML attribute), a pipe or HTML's `&nbsp;`. Punctuation that ends a sentence is not
/// part of it, and `&amp;`, as HTML writes `&` inside a link, is read as `&`. A link written
/// inside another is part of it, not a link of its own: Google Calendar wraps every link in an
/// invitation's description as `https://www.google.com/url?q=https://…`, and the copy inside
/// carries Google's own parameters.
pub(super) fn links(text: &str) -> impl Iterator<Item = ConferenceUri> + '_ {
    let lower = text.to_ascii_lowercase();
    let mut starts: Vec<usize> = lower
        .match_indices("https://")
        .chain(lower.match_indices("http://"))
        .map(|(at, _)| at)
        .collect();
    starts.sort_unstable();
    let mut covered = 0;
    starts.into_iter().filter_map(move |at| {
        if at < covered {
            return None;
        }
        let tail = &text[at..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '|'))
            .unwrap_or(tail.len());
        covered = at + end;
        let link = tail[..end].split("&nbsp;").next().unwrap_or_default();
        let link = link.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '*']);
        ConferenceUri::parse(&link.replace("&amp;", "&")).ok()
    })
}

#[cfg(test)]
#[path = "conference_detect_tests.rs"]
mod tests;
