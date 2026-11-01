//! Naming the service a conference runs on.

use super::ConferenceUri;

open_enum! {
    /// The service a conference runs on.
    ///
    /// Open, as every enum a payload stores is: a value this build does not know reads back as
    /// `Other`, so adding a service later breaks no stored event.
    ConferenceService {
        /// Microsoft Teams.
        Teams => "teams",
        /// Skype for Business.
        SkypeForBusiness => "skype-for-business",
        /// Skype (consumer).
        Skype => "skype",
        /// Google Meet.
        GoogleMeet => "google-meet",
        /// Zoom.
        Zoom => "zoom",
        /// Webex.
        Webex => "webex",
        /// Jitsi Meet.
        Jitsi => "jitsi",
        /// Nextcloud Talk.
        NextcloudTalk => "nextcloud-talk",
        /// Whereby.
        Whereby => "whereby",
        /// Proton Meet.
        ProtonMeet => "proton-meet",
        /// GoTo Meeting.
        GoTo => "goto",
    }
}

impl ConferenceService {
    /// Names the service a web join URI belongs to, from the shape of its host and path.
    ///
    /// Only the shapes each service uses for a meeting are recognised, never merely its domain,
    /// so a link to a vendor's home or download page names no service. A service running on
    /// whatever host its owner chose (Nextcloud Talk, a self-hosted Jitsi) is recognised by
    /// path where its path is distinctive, and otherwise not at all.
    #[must_use]
    pub fn recognise(uri: &ConferenceUri) -> Option<Self> {
        let host = uri.host()?;
        let segments: Vec<&str> = uri.path()?.split('/').filter(|s| !s.is_empty()).collect();
        let host = host.as_str();
        let first = segments.first().copied();
        let nonempty = !segments.is_empty();
        let service = match host {
            "teams.microsoft.com" if matches!(first, Some("l" | "meet")) => Self::Teams,
            "teams.live.com" if first == Some("meet") => Self::Teams,
            "meet.google.com" if first.is_some_and(is_meet_code) => Self::GoogleMeet,
            "meet.jit.si" | "8x8.vc" if nonempty => Self::Jitsi,
            "whereby.com" if nonempty => Self::Whereby,
            "meet.proton.me" if nonempty => Self::ProtonMeet,
            "meet.goto.com" if nonempty => Self::GoTo,
            "global.gotomeeting.com" if first == Some("join") => Self::GoTo,
            "meet.lync.com" if nonempty => Self::SkypeForBusiness,
            "join.skype.com" if nonempty => Self::Skype,
            _ if under(host, "zoom.us") || under(host, "zoomgov.com") => {
                if matches!(first, Some("j" | "my" | "w")) {
                    Self::Zoom
                } else {
                    return None;
                }
            }
            _ if host.ends_with(".webex.com") => {
                let is_meeting = segments
                    .iter()
                    .any(|s| matches!(*s, "meet" | "join" | "j.php"));
                if is_meeting {
                    Self::Webex
                } else {
                    return None;
                }
            }
            _ if host.ends_with(".whereby.com") && nonempty => Self::Whereby,
            _ if is_talk_path(&segments) => Self::NextcloudTalk,
            _ => return None,
        };
        Some(service)
    }
}

/// Whether `host` is `domain` or one of its subdomains.
fn under(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// A Meet code: three, four and three lowercase letters joined by hyphens.
fn is_meet_code(segment: &str) -> bool {
    let parts: Vec<&str> = segment.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .zip([3, 4, 3])
            .all(|(part, len)| part.len() == len && part.bytes().all(|b| b.is_ascii_lowercase()))
}

/// A Nextcloud Talk call: a path ending in `call/<token>`, the token being the lowercase
/// letters and digits Talk generates.
fn is_talk_path(segments: &[&str]) -> bool {
    match segments {
        [.., "call", token] => {
            (4..=32).contains(&token.len())
                && token
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recognise(value: &str) -> Option<ConferenceService> {
        ConferenceService::recognise(&ConferenceUri::parse(value).unwrap())
    }

    #[test]
    fn recognises_each_service_by_its_meeting_shapes() {
        let cases = [
            (
                "https://teams.microsoft.com/l/meetup-join/19%3ameeting_x%40thread.v2/0?context=%7b%7d",
                ConferenceService::Teams,
            ),
            (
                "https://teams.microsoft.com/meet/12345678901?p=abc",
                ConferenceService::Teams,
            ),
            (
                "https://teams.live.com/meet/9990000000000?p=abc",
                ConferenceService::Teams,
            ),
            (
                "https://meet.google.com/abc-defg-hij",
                ConferenceService::GoogleMeet,
            ),
            (
                "https://meet.google.com/abc-defg-hij?authuser=0",
                ConferenceService::GoogleMeet,
            ),
            (
                "https://us02web.zoom.us/j/81234567890?pwd=abc",
                ConferenceService::Zoom,
            ),
            ("https://zoom.us/my/someone", ConferenceService::Zoom),
            (
                "https://example.zoomgov.com/j/1610000000",
                ConferenceService::Zoom,
            ),
            (
                "https://example.webex.com/meet/someone",
                ConferenceService::Webex,
            ),
            (
                "https://example.webex.com/example/j.php?MTID=m0",
                ConferenceService::Webex,
            ),
            ("https://meet.jit.si/SomeRoom", ConferenceService::Jitsi),
            ("https://8x8.vc/tenant/room", ConferenceService::Jitsi),
            (
                "https://cloud.example.com/call/abcd1234",
                ConferenceService::NextcloudTalk,
            ),
            (
                "https://cloud.example.com/index.php/call/abcd1234",
                ConferenceService::NextcloudTalk,
            ),
            (
                "https://example.com/nextcloud/index.php/call/abcd1234",
                ConferenceService::NextcloudTalk,
            ),
            ("https://whereby.com/room", ConferenceService::Whereby),
            ("https://team.whereby.com/room", ConferenceService::Whereby),
            (
                "https://meet.proton.me/join/abc",
                ConferenceService::ProtonMeet,
            ),
            ("https://meet.goto.com/123456789", ConferenceService::GoTo),
            (
                "https://global.gotomeeting.com/join/123456789",
                ConferenceService::GoTo,
            ),
            (
                "https://meet.lync.com/example/someone/ABC123",
                ConferenceService::SkypeForBusiness,
            ),
            ("https://join.skype.com/abcDEF", ConferenceService::Skype),
        ];
        for (value, service) in cases {
            assert_eq!(recognise(value), Some(service), "{value}");
        }
    }

    #[test]
    fn a_vendor_page_that_is_not_a_meeting_names_no_service() {
        for value in [
            "https://teams.microsoft.com/",
            "https://meet.google.com/",
            "https://meet.google.com/new",
            "https://zoom.us/download",
            "https://notzoom.us/j/123",
            "https://www.webex.com/downloads.html",
            "https://meet.jit.si/",
            "https://example.com/call/Not-A-Token",
            "https://meet.example.com/harness-room",
        ] {
            assert_eq!(recognise(value), None, "{value}");
        }
    }

    #[test]
    fn a_dial_in_or_sip_address_names_no_service() {
        assert_eq!(recognise("tel:+31201234567"), None);
        assert_eq!(recognise("sip:123@example.com"), None);
    }

    #[test]
    fn an_unknown_stored_service_reads_back_verbatim() {
        let stored: ConferenceService = serde_json::from_str("\"future-service\"").unwrap();
        assert_eq!(stored, ConferenceService::Other("future-service".into()));
        assert_eq!(
            serde_json::to_string(&ConferenceService::GoogleMeet).unwrap(),
            "\"google-meet\""
        );
    }
}
