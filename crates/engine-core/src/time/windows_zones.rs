//! Resolving a **provider-supplied** time-zone name to the engine's [`TimeZoneId`].
//!
//! The engine time model is **IANA-only** (`calendar-semantics.md`), but two inbound
//! surfaces hand us **Windows** zone names instead:
//!
//! - **Microsoft Graph** reports event zones as Windows names (`"W. Europe Standard Time"`,
//!   `"Pacific Standard Time"`, `"UTC"`) unless the sync asked for IANA.
//! - **iCalendar `TZID` parameters**, whenever the event was authored by Outlook/Exchange — which
//!   is most invitations. `DTSTART;TZID=W. Europe Standard Time:20260730T163000` is a perfectly
//!   ordinary iMIP payload, and it reaches us over CalDAV *and* over mail.
//!
//! Both call [`resolve_zone_name`], so one policy decides all of it. That matters
//! concretely: while this lived inside the Graph adapter, the iCalendar path called
//! `TimeZoneId::iana` directly — which validates nothing but emptiness — so a Windows name
//! became `Iana("W. Europe Standard Time")`, `is_supported_zone` returned `false`, and the
//! instant never resolved. The same Outlook meeting therefore rendered correctly when it
//! arrived over Graph and not at all when it arrived as an invitation.
//!
//! The table is the CLDR `windowsZones` **default** mapping (each Windows zone's
//! `territory="001"` IANA zone) — CLDR release **49**, vendored from
//! `common/supplemental/windowsZones.xml`. It is the same mapping the .NET/ICU stacks use.
//! An unknown name is preserved as [`TimeZoneId::custom`] rather than guessed (the engine's
//! expander then rejects the custom zone until embedded-`VTIMEZONE` support lands —
//! `calendar-semantics.md`, "custom/embedded-VTIMEZONE zones"): being honest that we cannot
//! place the event beats silently placing it in the wrong hour.

use super::{TimeError, TimeZoneId};

/// CLDR `windowsZones` default (`territory="001"`) mappings, **sorted by the Windows
/// name** so [`windows_to_iana`] can binary-search. CLDR release 49.
const WINDOWS_TO_IANA: &[(&str, &str)] = &[
    ("AUS Central Standard Time", "Australia/Darwin"),
    ("AUS Eastern Standard Time", "Australia/Sydney"),
    ("Afghanistan Standard Time", "Asia/Kabul"),
    ("Alaskan Standard Time", "America/Anchorage"),
    ("Aleutian Standard Time", "America/Adak"),
    ("Altai Standard Time", "Asia/Barnaul"),
    ("Arab Standard Time", "Asia/Riyadh"),
    ("Arabian Standard Time", "Asia/Dubai"),
    ("Arabic Standard Time", "Asia/Baghdad"),
    ("Argentina Standard Time", "America/Buenos_Aires"),
    ("Astrakhan Standard Time", "Europe/Astrakhan"),
    ("Atlantic Standard Time", "America/Halifax"),
    ("Aus Central W. Standard Time", "Australia/Eucla"),
    ("Azerbaijan Standard Time", "Asia/Baku"),
    ("Azores Standard Time", "Atlantic/Azores"),
    ("Bahia Standard Time", "America/Bahia"),
    ("Bangladesh Standard Time", "Asia/Dhaka"),
    ("Belarus Standard Time", "Europe/Minsk"),
    ("Bougainville Standard Time", "Pacific/Bougainville"),
    ("Canada Central Standard Time", "America/Regina"),
    ("Cape Verde Standard Time", "Atlantic/Cape_Verde"),
    ("Caucasus Standard Time", "Asia/Yerevan"),
    ("Cen. Australia Standard Time", "Australia/Adelaide"),
    ("Central America Standard Time", "America/Guatemala"),
    ("Central Asia Standard Time", "Asia/Bishkek"),
    ("Central Brazilian Standard Time", "America/Cuiaba"),
    ("Central Europe Standard Time", "Europe/Budapest"),
    ("Central European Standard Time", "Europe/Warsaw"),
    ("Central Pacific Standard Time", "Pacific/Guadalcanal"),
    ("Central Standard Time", "America/Chicago"),
    ("Central Standard Time (Mexico)", "America/Mexico_City"),
    ("Chatham Islands Standard Time", "Pacific/Chatham"),
    ("China Standard Time", "Asia/Shanghai"),
    ("Cuba Standard Time", "America/Havana"),
    ("Dateline Standard Time", "Etc/GMT+12"),
    ("E. Africa Standard Time", "Africa/Nairobi"),
    ("E. Australia Standard Time", "Australia/Brisbane"),
    ("E. Europe Standard Time", "Europe/Chisinau"),
    ("E. South America Standard Time", "America/Sao_Paulo"),
    ("Easter Island Standard Time", "Pacific/Easter"),
    ("Eastern Standard Time", "America/New_York"),
    ("Eastern Standard Time (Mexico)", "America/Cancun"),
    ("Egypt Standard Time", "Africa/Cairo"),
    ("Ekaterinburg Standard Time", "Asia/Yekaterinburg"),
    ("FLE Standard Time", "Europe/Kiev"),
    ("Fiji Standard Time", "Pacific/Fiji"),
    ("GMT Standard Time", "Europe/London"),
    ("GTB Standard Time", "Europe/Bucharest"),
    ("Georgian Standard Time", "Asia/Tbilisi"),
    ("Greenland Standard Time", "America/Godthab"),
    ("Greenwich Standard Time", "Atlantic/Reykjavik"),
    ("Haiti Standard Time", "America/Port-au-Prince"),
    ("Hawaiian Standard Time", "Pacific/Honolulu"),
    ("India Standard Time", "Asia/Calcutta"),
    ("Iran Standard Time", "Asia/Tehran"),
    ("Israel Standard Time", "Asia/Jerusalem"),
    ("Jordan Standard Time", "Asia/Amman"),
    ("Kaliningrad Standard Time", "Europe/Kaliningrad"),
    ("Korea Standard Time", "Asia/Seoul"),
    ("Libya Standard Time", "Africa/Tripoli"),
    ("Line Islands Standard Time", "Pacific/Kiritimati"),
    ("Lord Howe Standard Time", "Australia/Lord_Howe"),
    ("Magadan Standard Time", "Asia/Magadan"),
    ("Magallanes Standard Time", "America/Punta_Arenas"),
    ("Marquesas Standard Time", "Pacific/Marquesas"),
    ("Mauritius Standard Time", "Indian/Mauritius"),
    ("Middle East Standard Time", "Asia/Beirut"),
    ("Montevideo Standard Time", "America/Montevideo"),
    ("Morocco Standard Time", "Africa/Casablanca"),
    ("Mountain Standard Time", "America/Denver"),
    ("Mountain Standard Time (Mexico)", "America/Mazatlan"),
    ("Myanmar Standard Time", "Asia/Rangoon"),
    ("N. Central Asia Standard Time", "Asia/Novosibirsk"),
    ("Namibia Standard Time", "Africa/Windhoek"),
    ("Nepal Standard Time", "Asia/Katmandu"),
    ("New Zealand Standard Time", "Pacific/Auckland"),
    ("Newfoundland Standard Time", "America/St_Johns"),
    ("Norfolk Standard Time", "Pacific/Norfolk"),
    ("North Asia East Standard Time", "Asia/Irkutsk"),
    ("North Asia Standard Time", "Asia/Krasnoyarsk"),
    ("North Korea Standard Time", "Asia/Pyongyang"),
    ("Omsk Standard Time", "Asia/Omsk"),
    ("Pacific SA Standard Time", "America/Santiago"),
    ("Pacific Standard Time", "America/Los_Angeles"),
    ("Pacific Standard Time (Mexico)", "America/Tijuana"),
    ("Pakistan Standard Time", "Asia/Karachi"),
    ("Paraguay Standard Time", "America/Asuncion"),
    ("Qyzylorda Standard Time", "Asia/Qyzylorda"),
    ("Romance Standard Time", "Europe/Paris"),
    ("Russia Time Zone 10", "Asia/Srednekolymsk"),
    ("Russia Time Zone 11", "Asia/Kamchatka"),
    ("Russia Time Zone 3", "Europe/Samara"),
    ("Russian Standard Time", "Europe/Moscow"),
    ("SA Eastern Standard Time", "America/Cayenne"),
    ("SA Pacific Standard Time", "America/Bogota"),
    ("SA Western Standard Time", "America/La_Paz"),
    ("SE Asia Standard Time", "Asia/Bangkok"),
    ("Saint Pierre Standard Time", "America/Miquelon"),
    ("Sakhalin Standard Time", "Asia/Sakhalin"),
    ("Samoa Standard Time", "Pacific/Apia"),
    ("Sao Tome Standard Time", "Africa/Sao_Tome"),
    ("Saratov Standard Time", "Europe/Saratov"),
    ("Singapore Standard Time", "Asia/Singapore"),
    ("South Africa Standard Time", "Africa/Johannesburg"),
    ("South Sudan Standard Time", "Africa/Juba"),
    ("Sri Lanka Standard Time", "Asia/Colombo"),
    ("Sudan Standard Time", "Africa/Khartoum"),
    ("Syria Standard Time", "Asia/Damascus"),
    ("Taipei Standard Time", "Asia/Taipei"),
    ("Tasmania Standard Time", "Australia/Hobart"),
    ("Tocantins Standard Time", "America/Araguaina"),
    ("Tokyo Standard Time", "Asia/Tokyo"),
    ("Tomsk Standard Time", "Asia/Tomsk"),
    ("Tonga Standard Time", "Pacific/Tongatapu"),
    ("Transbaikal Standard Time", "Asia/Chita"),
    ("Turkey Standard Time", "Europe/Istanbul"),
    ("Turks And Caicos Standard Time", "America/Grand_Turk"),
    ("US Eastern Standard Time", "America/Indianapolis"),
    ("US Mountain Standard Time", "America/Phoenix"),
    ("UTC", "Etc/UTC"),
    ("UTC+12", "Etc/GMT-12"),
    ("UTC+13", "Etc/GMT-13"),
    ("UTC-02", "Etc/GMT+2"),
    ("UTC-08", "Etc/GMT+8"),
    ("UTC-09", "Etc/GMT+9"),
    ("UTC-11", "Etc/GMT+11"),
    ("Ulaanbaatar Standard Time", "Asia/Ulaanbaatar"),
    ("Venezuela Standard Time", "America/Caracas"),
    ("Vladivostok Standard Time", "Asia/Vladivostok"),
    ("Volgograd Standard Time", "Europe/Volgograd"),
    ("W. Australia Standard Time", "Australia/Perth"),
    ("W. Central Africa Standard Time", "Africa/Lagos"),
    ("W. Europe Standard Time", "Europe/Berlin"),
    ("W. Mongolia Standard Time", "Asia/Hovd"),
    ("West Asia Standard Time", "Asia/Tashkent"),
    ("West Bank Standard Time", "Asia/Hebron"),
    ("West Pacific Standard Time", "Pacific/Port_Moresby"),
    ("Yakutsk Standard Time", "Asia/Yakutsk"),
    ("Yukon Standard Time", "America/Whitehorse"),
];

/// The IANA zone id for a Windows zone name, or `None` when the name is not a known
/// Windows zone (a legacy `tzone://Microsoft/Custom`, or one absent from CLDR).
#[must_use]
pub fn windows_to_iana(name: &str) -> Option<&'static str> {
    WINDOWS_TO_IANA
        .binary_search_by(|(win, _)| (*win).cmp(name))
        .ok()
        .map(|i| WINDOWS_TO_IANA[i].1)
}

/// Resolves a provider- or iCalendar-supplied zone name to a [`TimeZoneId`].
///
/// Three steps, in order:
///
/// 1. A known **Windows** name maps through the CLDR table to its IANA zone.
/// 2. A name that looks like an **IANA** id (`Region/City`, and not a `tzone:` URI) is taken as
///    one, unvalidated — the bundled tzdb lives in `engine-recurrence`, whose `is_supported_zone`
///    is where a name that does not resolve is caught.
/// 3. Anything else — a legacy `tzone://Microsoft/Custom`, a made-up `"Foo Standard Time"`, an
///    embedded-`VTIMEZONE`-only id — becomes a [`TimeZoneId::custom`], recording that we could
///    **not** place it rather than pretending it is an IANA zone that happens not to resolve.
///
/// # Errors
///
/// Returns [`TimeError::Empty`] if `name` is empty.
pub fn resolve_zone_name(name: &str) -> Result<TimeZoneId, TimeError> {
    if let Some(iana) = windows_to_iana(name) {
        return TimeZoneId::iana(iana);
    }
    if name.contains('/') && !name.starts_with("tzone:") {
        return TimeZoneId::iana(name);
    }
    TimeZoneId::custom(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_windows_zones_to_iana() {
        assert_eq!(
            windows_to_iana("W. Europe Standard Time"),
            Some("Europe/Berlin")
        );
        assert_eq!(
            windows_to_iana("Pacific Standard Time"),
            Some("America/Los_Angeles")
        );
        assert_eq!(
            windows_to_iana("Eastern Standard Time"),
            Some("America/New_York")
        );
        assert_eq!(windows_to_iana("Tokyo Standard Time"), Some("Asia/Tokyo"));
        // Graph reports UTC as a Windows name too.
        assert_eq!(windows_to_iana("UTC"), Some("Etc/UTC"));
    }

    #[test]
    fn an_unknown_name_is_none_so_the_caller_can_preserve_it() {
        assert_eq!(windows_to_iana("tzone://Microsoft/Custom"), None);
        assert_eq!(windows_to_iana("Not A Zone"), None);
        assert_eq!(windows_to_iana(""), None);
    }

    #[test]
    fn the_table_is_sorted_for_binary_search() {
        // The binary search is only correct if the table stays sorted by the Windows
        // name; guard the invariant so a hand-edit that breaks ordering fails here.
        assert!(WINDOWS_TO_IANA.windows(2).all(|w| w[0].0 < w[1].0));
        // Every mapped IANA value is non-empty (so `TimeZoneId::iana` never errors).
        assert!(WINDOWS_TO_IANA.iter().all(|(_, iana)| !iana.is_empty()));
    }

    // --- resolve_zone_name: the one policy both boundaries share ------------------

    #[test]
    fn an_outlook_invitations_windows_tzid_resolves_to_a_real_iana_zone() {
        // The exact `TZID` an Outlook-authored invitation carries. Before this policy was
        // shared, the iCalendar path called `TimeZoneId::iana` directly and produced
        // `Iana("W. Europe Standard Time")` — a name no tzdb resolves, so the meeting had
        // no instant and could be neither placed on the grid nor checked for conflicts.
        let zone = resolve_zone_name("W. Europe Standard Time").unwrap();
        assert_eq!(zone, TimeZoneId::iana("Europe/Berlin").unwrap());
        assert!(zone.is_iana(), "it must be resolvable, not merely recorded");
    }

    #[test]
    fn an_iana_name_passes_through_unchanged() {
        // The overwhelmingly common iCalendar case must be untouched by the new lookup.
        for name in ["Europe/Amsterdam", "America/New_York", "Etc/UTC"] {
            assert_eq!(
                resolve_zone_name(name).unwrap(),
                TimeZoneId::iana(name).unwrap()
            );
        }
    }

    #[test]
    fn a_name_we_cannot_place_becomes_custom_rather_than_a_fake_iana_zone() {
        // `Custom` is the honest answer: it records that we could not place the event.
        // Calling it `Iana` would claim a resolvable zone and then fail to resolve.
        for name in [
            "tzone://Microsoft/Custom",
            "Foo Standard Time",
            "Customized Time Zone",
        ] {
            let zone = resolve_zone_name(name).unwrap();
            assert_eq!(zone, TimeZoneId::custom(name).unwrap());
            assert!(!zone.is_iana(), "{name} must not masquerade as IANA");
            assert_eq!(zone.as_str(), name, "the original id is preserved verbatim");
        }
    }

    #[test]
    fn an_empty_zone_name_is_rejected() {
        assert_eq!(resolve_zone_name(""), Err(TimeError::Empty));
    }
}
