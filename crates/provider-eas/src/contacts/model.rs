// SPDX-License-Identifier: MPL-2.0
// Contacts item model ([MS-ASCNTC] §2.2).

use serde::{Deserialize, Serialize};

// ============================================================================
// Model types ([MS-ASCNTC] §2.2)
// ============================================================================

/// One address set of a contact — the Business / Home / Other triples of
/// [MS-ASCNTC]. The wire carries each component as its own flat element
/// (BusinessAddressStreet, BusinessAddressCity, … — every component "can
/// be ghosted", §2.2.2.10 et al.); grouping the five components per set
/// mirrors the Outlook object model and hands the sync/store conversion
/// layer one natural unit per address. A set that appeared on the wire
/// with only some components is `Some(ContactsAddress { ..partial })`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsAddress {
    /// `...AddressStreet` ([MS-ASCNTC] §2.2.2.14 / §2.2.2.37 / §2.2.2.56).
    pub street: Option<String>,
    /// `...AddressCity` ([MS-ASCNTC] §2.2.2.10 / §2.2.2.33 / §2.2.2.52).
    pub city: Option<String>,
    /// `...AddressState` ([MS-ASCNTC] §2.2.2.13 / §2.2.2.36 / §2.2.2.55).
    pub state: Option<String>,
    /// `...AddressPostalCode` ([MS-ASCNTC] §2.2.2.12 / §2.2.2.35 /
    /// §2.2.2.54).
    pub postal_code: Option<String>,
    /// `...AddressCountry` ([MS-ASCNTC] §2.2.2.11 / §2.2.2.34 /
    /// §2.2.2.53).
    pub country: Option<String>,
}

/// One EAS Contacts item's application data (downsync model; v1 never builds
/// these for upload). Raw wire values; conversion to the app's
/// `ParsedContact` happens in the backend (sync/store layer), not here.
///
/// C1 core set: name parts + filing, the three e-mail slots, company/job,
/// the plain-text body (contact notes), and the three primary phone slots.
/// M8-C task 2 added: the three address sets, the remaining practical
/// phones, anniversary/birthday (raw wire strings), assistant/manager,
/// web page, and picture PRESENCE (the Picture payload itself is dropped
/// at parse time — see `parse_picture_present`).
///
/// Serde derives: the type rides inside `SyncResult`
/// (`ContactsItemWithId`), which itself derives `Serialize`/`Deserialize`
/// — mirrors the `CalendarEventProps` precedent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContactsContactProps {
    /// `FileAs` — how the contact is filed ([MS-ASCNTC] §2.2.2.30).
    pub file_as: Option<String>,
    /// `FirstName` ([MS-ASCNTC] §2.2.2.31).
    pub first_name: Option<String>,
    /// `MiddleName` ([MS-ASCNTC] §2.2.2.47).
    pub middle_name: Option<String>,
    /// `LastName` ([MS-ASCNTC] §2.2.2.45).
    pub last_name: Option<String>,
    /// `Suffix` — name suffix, e.g. "Jr." ([MS-ASCNTC] §2.2.2.61).
    pub name_suffix: Option<String>,
    /// `Title` — name prefix, e.g. "Mr." ([MS-ASCNTC] §2.2.2.62).
    pub name_prefix: Option<String>,
    /// `Email1Address` — the primary e-mail address
    /// ([MS-ASCNTC] §2.2.2.27).
    pub email_1: Option<String>,
    /// `Email2Address` ([MS-ASCNTC] §2.2.2.28).
    pub email_2: Option<String>,
    /// `Email3Address` ([MS-ASCNTC] §2.2.2.29).
    pub email_3: Option<String>,
    /// `CompanyName` ([MS-ASCNTC] §2.2.2.24).
    pub company: Option<String>,
    /// `JobTitle` ([MS-ASCNTC] §2.2.2.44).
    pub job_title: Option<String>,
    /// Plain-text body: `airsyncbase:Body` with `Type = 1` (PlainText) —
    /// the contact's notes ([MS-ASCNTC] §2.2.2.7.1; the 2.5-only
    /// contacts-page Body is not modeled). HTML/MIME bodies are not
    /// modeled on contact items in v1.
    pub body_plain: Option<String>,
    /// `BusinessPhoneNumber` — the primary business line
    /// ([MS-ASCNTC] §2.2.2.16).
    pub business_phone: Option<String>,
    /// `HomePhoneNumber` ([MS-ASCNTC] §2.2.2.39).
    pub home_phone: Option<String>,
    /// `MobilePhoneNumber` ([MS-ASCNTC] §2.2.2.49).
    pub mobile_phone: Option<String>,

    // ---- M8-C task 2 ----
    /// `Anniversary` — wedding anniversary ([MS-ASCNTC] §2.2.2.3). The
    /// wire value verbatim: dateTime per [MS-ASDTYPE] §2.3
    /// (`YYYY-MM-DDTHH:MM:SS.MSSZ`, UTC; the time part "might be 11:59
    /// and SHOULD be ignored") — RAW string, no date parsing in the
    /// parser; interpretation belongs to the conversion layer.
    pub anniversary: Option<String>,
    /// `Birthday` — birth date ([MS-ASCNTC] §2.2.2.6). Raw wire string,
    /// same contract as [`Self::anniversary`].
    pub birthday: Option<String>,
    /// `AssistantName` ([MS-ASCNTC] §2.2.2.4).
    pub assistant_name: Option<String>,
    /// `contacts2:ManagerName` (page 12) — the DN of the contact's
    /// manager ([MS-ASCNTC] §2.2.2.46).
    pub manager_name: Option<String>,
    /// `AssistantPhoneNumber` ([MS-ASCNTC] §2.2.2.5).
    pub assistant_phone: Option<String>,
    /// `Business2PhoneNumber` — the second business line
    /// ([MS-ASCNTC] §2.2.2.17).
    pub business_2_phone: Option<String>,
    /// `BusinessFaxNumber` ([MS-ASCNTC] §2.2.2.15).
    pub business_fax: Option<String>,
    /// `CarPhoneNumber` ([MS-ASCNTC] §2.2.2.18).
    pub car_phone: Option<String>,
    /// `contacts2:CompanyMainPhone` (page 12) — the company's main line
    /// ([MS-ASCNTC] §2.2.2.23).
    pub company_main_phone: Option<String>,
    /// `Home2PhoneNumber` — the second home line
    /// ([MS-ASCNTC] §2.2.2.40).
    pub home_2_phone: Option<String>,
    /// `HomeFaxNumber` ([MS-ASCNTC] §2.2.2.38).
    pub home_fax: Option<String>,
    /// `PagerNumber` ([MS-ASCNTC] §2.2.2.57).
    pub pager: Option<String>,
    /// `RadioPhoneNumber` ([MS-ASCNTC] §2.2.2.59).
    pub radio_phone: Option<String>,
    /// Business address set — the five `BusinessAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.10-.14); `None` when none appeared on the wire.
    pub business_address: Option<ContactsAddress>,
    /// Home address set — the five `HomeAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.33-.37).
    pub home_address: Option<ContactsAddress>,
    /// Other (alternate) address set — the five `OtherAddress*` elements
    /// ([MS-ASCNTC] §2.2.2.52-.56).
    pub other_address: Option<ContactsAddress>,
    /// `WebPage` — the contact's web site / personal page
    /// ([MS-ASCNTC] §2.2.2.63).
    pub web_page: Option<String>,
    /// `Picture` PRESENCE only ([MS-ASCNTC] §2.2.2.58). v1 never retains
    /// the base64 payload — it is dropped at parse time with a
    /// `log::debug!` (pinned by the picture-drop tests); `true` iff a
    /// Picture element with a non-empty value appeared on the wire.
    pub picture_present: bool,
}
