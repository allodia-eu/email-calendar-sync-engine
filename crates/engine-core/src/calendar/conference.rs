//! Conferences: the meetings an event can be joined at.
//!
//! Every calendar format stores a conference differently. iCalendar's `CONFERENCE`
//! (RFC 7986 §5.11) and JSCalendar's `virtualLocations` (RFC 8984 §4.2.6) are flat lists of
//! addresses; Google's `conferenceData` and Graph's `onlineMeeting` describe one meeting with
//! several ways in. The projection is the second shape, because it is what a person sees: one
//! meeting, a join link, perhaps dial-in numbers and a meeting ID. A flat list is grouped into
//! that shape by [`group_flat`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{ConferenceService, ConferenceUri, UriScheme};

/// Names a conference within its event, so a later edit can say which one it means.
///
/// The value is the format's own where it has one (a JSCalendar `virtualLocations` key), the
/// provider's field name where it allows only one conference (`conferenceData`,
/// `onlineMeeting`), and otherwise the conference's first URI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConferenceKey(String);

impl ConferenceKey {
    /// Wraps a key.
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

open_enum! {
    /// What a way into a conference is for (Google's `entryPointType`).
    EntryKind {
        /// A web link that opens the meeting.
        Video => "video",
        /// A number to dial.
        Phone => "phone",
        /// A SIP address.
        Sip => "sip",
        /// A page listing further ways in, such as more dial-in numbers.
        More => "more",
    }
}

open_enum! {
    /// A capability a way in offers (RFC 7986 `FEATURE`, JSCalendar `features`).
    ConferenceFeature {
        /// Audio.
        Audio => "audio",
        /// Chat or instant messaging.
        Chat => "chat",
        /// A blog or atom feed.
        Feed => "feed",
        /// Moderator dial-in.
        Moderator => "moderator",
        /// Phone conference.
        Phone => "phone",
        /// Screen sharing.
        Screen => "screen",
        /// Video.
        Video => "video",
    }
}

/// The codes a way in asks for, under Google's names for them. Each is shown to the person
/// as given; none is a secret the engine guards.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessCodes {
    /// A PIN.
    pub pin: Option<String>,
    /// An access code.
    pub access_code: Option<String>,
    /// A meeting code.
    pub meeting_code: Option<String>,
    /// A passcode.
    pub passcode: Option<String>,
    /// A password.
    pub password: Option<String>,
}

/// One way into a conference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPoint {
    /// What this way in is for.
    pub kind: EntryKind,
    /// Where it is.
    pub uri: ConferenceUri,
    /// The server's label for it, such as a formatted phone number.
    pub label: Option<String>,
    /// What the server says about it beyond the label, such as how to enter a PIN.
    pub description: Option<String>,
    /// The capabilities the server says it offers; empty when the server says nothing.
    pub features: BTreeSet<ConferenceFeature>,
    /// The codes it asks for.
    pub access: AccessCodes,
}

impl EntryPoint {
    /// A way in at `uri`, its kind read from the scheme: a web link joins the meeting, a
    /// `tel:` dials in and a `sip:` is a SIP address.
    #[must_use]
    pub fn at(uri: ConferenceUri) -> Self {
        let kind = match uri.scheme() {
            UriScheme::Https | UriScheme::Http => EntryKind::Video,
            UriScheme::Tel => EntryKind::Phone,
            UriScheme::Sip | UriScheme::Sips => EntryKind::Sip,
        };
        Self {
            kind,
            uri,
            label: None,
            description: None,
            features: BTreeSet::new(),
            access: AccessCodes::default(),
        }
    }

    /// The capabilities to write for this way in: the ones the server stated, else what its
    /// kind always offers (a web link audio and video, a number the phone).
    #[must_use]
    pub fn offered_features(&self) -> BTreeSet<ConferenceFeature> {
        if !self.features.is_empty() {
            return self.features.clone();
        }
        match self.kind {
            EntryKind::Video => [ConferenceFeature::Audio, ConferenceFeature::Video].into(),
            EntryKind::Phone => [ConferenceFeature::Phone].into(),
            _ => BTreeSet::new(),
        }
    }

    /// The number a person reads for a `tel:` way in: the server's label when it is written
    /// as a phone number, else the URI's own ([`ConferenceUri::phone_number`]). `None` for any
    /// other scheme. A label such as "Dial in" names the way in rather than the number, so it
    /// is not one.
    #[must_use]
    pub fn phone_number(&self) -> Option<&str> {
        let number = self.uri.phone_number()?;
        Some(
            self.label
                .as_deref()
                .map(str::trim)
                .filter(|label| reads_as_phone_number(label))
                .unwrap_or(number),
        )
    }
}

/// Whether `text` is a phone number as people write one: digits, an optional leading `+`, and
/// spaces or the separators `-`, `.`, `(` and `)` between them.
fn reads_as_phone_number(text: &str) -> bool {
    let digits = text.strip_prefix('+').unwrap_or(text);
    digits.bytes().any(|b| b.is_ascii_digit())
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b' ' | b'-' | b'.' | b'(' | b')'))
}

/// Where a calendar server's own meeting stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NativeState {
    /// The meeting exists.
    Ready,
    /// The server was asked for a meeting and has not made it yet.
    Pending,
    /// The server was asked for a meeting and could not make it.
    Failed,
}

/// How a conference came to be on the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ConferenceOrigin {
    /// The calendar server made the meeting and keeps it: a Google Meet conference, a Graph
    /// online meeting.
    Native {
        /// Whether the server lets it be taken off the event again.
        removable: bool,
        /// Where the meeting stands.
        state: NativeState,
    },
    /// The server stores a meeting it did not make, in a field built for it: iCalendar
    /// `CONFERENCE`, JSCalendar `virtualLocations`, a Google add-on conference.
    Attached,
    /// Found where no field built for a meeting held it (see
    /// [`add_detected`](super::add_detected)).
    Detected {
        /// Where it was found.
        place: DetectedIn,
    },
}

/// Where a detected conference was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DetectedIn {
    /// A vendor's own iCalendar property: `X-GOOGLE-CONFERENCE`,
    /// `X-MICROSOFT-SKYPETEAMSMEETINGURL`.
    VendorProperty,
    /// A location: one whose whole text is a web link, or a meeting link inside one.
    Location,
    /// A meeting link in the description.
    Description,
}

/// A meeting a person can join, with every way in it offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conference {
    /// Names it within the event.
    pub key: ConferenceKey,
    /// The service it runs on, when the server says or its join link shows; `None` when
    /// neither does.
    pub service: Option<ConferenceService>,
    /// How it came to be on the event.
    pub origin: ConferenceOrigin,
    /// The ways in, the one a person would normally use first. Empty only while a server's
    /// own meeting is pending or after it failed.
    pub entry_points: Vec<EntryPoint>,
    /// The server's or service's name for the meeting, such as `Google Meet` or an iCalendar
    /// `LABEL`.
    pub label: Option<String>,
    /// The ID a person types to join from a phone or an app (Google's and Graph's
    /// `conferenceId`).
    pub meeting_id: Option<String>,
    /// Instructions shown with the meeting, as plain text.
    pub notes: Option<String>,
}

impl Conference {
    /// The web link a person joins at: the first [`EntryKind::Video`] way in.
    #[must_use]
    pub fn join(&self) -> Option<&ConferenceUri> {
        self.entry_points
            .iter()
            .find(|entry| entry.kind == EntryKind::Video)
            .map(|entry| &entry.uri)
    }

    /// Each way in, the join link first, with the label a flat list stores for it: the
    /// meeting's name on the join link, which is where [`group_flat`] reads the name from, and
    /// each other way in's own.
    pub fn labelled_ways_in(&self) -> impl Iterator<Item = (&EntryPoint, Option<&str>)> {
        let join = self.join();
        let mut points: Vec<&EntryPoint> = self.entry_points.iter().collect();
        points.sort_by_key(|point| Some(&point.uri) != join);
        points.into_iter().map(move |point| {
            let label = if Some(&point.uri) == join {
                self.label.as_deref().or(point.label.as_deref())
            } else {
                point.label.as_deref()
            };
            (point, label)
        })
    }

    /// Whether following `uri` joins this meeting: one of its ways in is the same meeting
    /// ([`ConferenceUri::same_meeting`]), or `uri` runs on its service and this is the event's
    /// one meeting there, which the calendar server made or a vendor property names. The
    /// server may write such a meeting into the text under another link than its own (Teams
    /// puts a short `meet/<id>` link in the body of a meeting whose `joinUrl`, or
    /// `X-MICROSOFT-SKYPETEAMSMEETINGURL`, is its long `meetup-join` one).
    #[must_use]
    pub fn joined_at(&self, uri: &ConferenceUri) -> bool {
        self.claims_service(ConferenceService::recognise(uri).as_ref())
            || self
                .entry_points
                .iter()
                .any(|entry| entry.uri.same_meeting(uri))
    }

    /// Whether this is the event's one meeting on `service`, so that any link to `service`
    /// joins it (see [`Self::joined_at`]).
    pub(super) fn claims_service(&self, service: Option<&ConferenceService>) -> bool {
        service.is_some()
            && self.service.as_ref() == service
            && matches!(
                self.origin,
                ConferenceOrigin::Native { .. }
                    | ConferenceOrigin::Detected {
                        place: DetectedIn::VendorProperty
                    }
            )
    }
}

/// One address of a flat conference list (iCalendar `CONFERENCE`, JSCalendar
/// `virtualLocations`), before [`group_flat`] gathers the addresses into meetings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatEntry {
    /// The format's key for it, where it has one (a `virtualLocations` key).
    pub key: Option<String>,
    /// The address.
    pub uri: ConferenceUri,
    /// The label (`LABEL`, `name`).
    pub label: Option<String>,
    /// A description (JSCalendar `description`).
    pub description: Option<String>,
    /// The features (`FEATURE`, `features`).
    pub features: BTreeSet<ConferenceFeature>,
}

impl FlatEntry {
    /// An address with nothing else said about it.
    #[must_use]
    pub fn new(uri: ConferenceUri) -> Self {
        Self {
            key: None,
            uri,
            label: None,
            description: None,
            features: BTreeSet::new(),
        }
    }
}

/// Gathers a flat list of conference addresses into meetings.
///
/// Each web address starts a meeting. A dial-in or SIP address belongs to the meeting when the
/// list holds exactly one web address, and is a meeting of its own otherwise, since nothing in
/// the format says which of several meetings a number dials into. A repeated address is read
/// once. Every meeting is [`ConferenceOrigin::Attached`]; its key, label and notes are its
/// web address's, and an address gathered into it keeps its own label and description but
/// not its format key.
#[must_use]
pub fn group_flat(entries: Vec<FlatEntry>) -> Vec<Conference> {
    let mut seen = BTreeSet::new();
    let entries: Vec<FlatEntry> = entries
        .into_iter()
        .filter(|entry| seen.insert(entry.uri.clone()))
        .collect();
    let (web, other): (Vec<FlatEntry>, Vec<FlatEntry>) = entries
        .into_iter()
        .partition(|entry| entry.uri.scheme().is_web());
    let mut conferences: Vec<Conference> = web.into_iter().map(conference_from).collect();
    if let [only] = conferences.as_mut_slice() {
        only.entry_points
            .extend(other.into_iter().map(entry_point_from));
    } else {
        conferences.extend(other.into_iter().map(conference_from));
    }
    conferences
}

/// A meeting started by `entry`. A web address's description is the meeting's notes; a
/// number standing alone keeps it on its way in, as a gathered one does.
fn conference_from(mut entry: FlatEntry) -> Conference {
    let key = ConferenceKey::new(entry.key.take().unwrap_or_else(|| entry.uri.to_string()));
    let service = ConferenceService::recognise(&entry.uri);
    let label = entry.label.clone();
    let notes = if entry.uri.scheme().is_web() {
        entry.description.take()
    } else {
        None
    };
    Conference {
        key,
        service,
        origin: ConferenceOrigin::Attached,
        entry_points: vec![entry_point_from(entry)],
        label,
        meeting_id: None,
        notes,
    }
}

fn entry_point_from(entry: FlatEntry) -> EntryPoint {
    let mut point = EntryPoint::at(entry.uri);
    point.label = entry.label;
    point.description = entry.description;
    point.features = entry.features;
    point
}

#[cfg(test)]
#[path = "conference_tests.rs"]
mod tests;
