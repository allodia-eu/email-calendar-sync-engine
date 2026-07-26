//! CardDAV discovery, multistatus normalization, and small wire helpers.

use std::collections::BTreeSet;

use engine_core::{
    contact::{AddressBook, ContactCard, ContactField, ContactFieldSet, ContactSourceClass},
    ids::{AddressBookId, ContactId, ProviderKey},
    sync::SyncUpdate,
    version::{ETag, RevisionTokens},
};
use engine_provider::ProviderResult;

use crate::{
    dav::{DavResponse, MultiStatus},
    error::CalDavError,
    request::{
        ADDRESS_BOOK_CTAG_PROPFIND, ADDRESS_BOOK_LIST_PROPFIND, ADDRESS_BOOK_PRINCIPAL_PROPFIND,
        ADDRESS_BOOK_QUERY_REPORT, address_book_sync_report,
    },
    transport::{DavExecutor, DavMethod},
    vcard,
};

pub(crate) async fn discover_home(
    executor: &dyn DavExecutor,
    start: &str,
) -> Result<String, CalDavError> {
    let first = principal_props(executor, start).await?;
    if let Some(home) = property(&first, "addressbook-home-set") {
        return Ok(home);
    }
    let principal = property(&first, "current-user-principal")
        .ok_or_else(|| CalDavError::protocol("CardDAV principal missing"))?;
    property(
        &principal_props(executor, &principal).await?,
        "addressbook-home-set",
    )
    .ok_or_else(|| CalDavError::protocol("CardDAV address-book home missing"))
}

async fn principal_props(
    executor: &dyn DavExecutor,
    href: &str,
) -> Result<MultiStatus, CalDavError> {
    let mut href = href.to_owned();
    for _ in 0..4 {
        let response = executor
            .send(
                DavMethod::Propfind,
                &href,
                "0",
                ADDRESS_BOOK_PRINCIPAL_PROPFIND.into(),
            )
            .await?;
        if response.is_redirect()
            && let Some(location) = response.location
        {
            href = location;
            continue;
        }
        return response.into_multistatus();
    }
    Err(CalDavError::protocol(
        "too many CardDAV discovery redirects",
    ))
}

fn property(status: &MultiStatus, name: &str) -> Option<String> {
    status
        .responses
        .iter()
        .find_map(|response| response.props.get(name).map(str::to_owned))
}

pub(crate) async fn list_address_books(
    executor: &dyn DavExecutor,
    home: &str,
) -> Result<Vec<AddressBook>, CalDavError> {
    let status = executor
        .send(
            DavMethod::Propfind,
            home,
            "1",
            ADDRESS_BOOK_LIST_PROPFIND.into(),
        )
        .await?
        .into_multistatus()?;
    status
        .responses
        .iter()
        .filter(|response| response.props.is_address_book())
        .map(address_book)
        .collect()
}

fn address_book(response: &DavResponse) -> Result<AddressBook, CalDavError> {
    let id = AddressBookId::try_from(response.href())
        .map_err(|error| CalDavError::protocol(error.to_string()))?;
    let mut book = AddressBook::new(
        id,
        response.props.get("displayname").unwrap_or("Address book"),
        ContactSourceClass::Personal,
    );
    book.description = response
        .props
        .get("addressbook-description")
        .map(str::to_owned);
    book.is_writable = response.props.grants_member_writes();
    if let Some(rights) = response.props.privileges() {
        book.rights.extend(rights.iter().cloned());
    }
    Ok(book)
}

pub(crate) async fn contact_report(
    executor: &dyn DavExecutor,
    collection: &str,
    token: &str,
) -> Result<MultiStatus, CalDavError> {
    executor
        .send(
            DavMethod::Report,
            collection,
            "1",
            address_book_sync_report(token),
        )
        .await?
        .into_multistatus()
}

pub(crate) async fn fallback_contact_sync(
    executor: &dyn DavExecutor,
    collection: &str,
    book: &AddressBookId,
    writable: bool,
    cursor: Option<&engine_core::sync::SyncState>,
) -> Result<engine_provider::ScopeSync<ContactCard>, CalDavError> {
    let properties = executor
        .send(
            DavMethod::Propfind,
            collection,
            "0",
            ADDRESS_BOOK_CTAG_PROPFIND.into(),
        )
        .await?
        .into_multistatus()?;
    let ctag = property(&properties, "getctag")
        .ok_or_else(|| CalDavError::protocol("CardDAV fallback response had no CTag"))?;
    let next = engine_core::sync::SyncState::new(format!("ctag:{ctag}"));
    if cursor.is_some_and(|cursor| cursor.as_str() == next.as_str()) {
        return Ok(engine_provider::ScopeSync::new(
            SyncUpdate::delta(Vec::new(), Vec::new()),
            next,
        ));
    }
    let report = executor
        .send(
            DavMethod::Report,
            collection,
            "1",
            ADDRESS_BOOK_QUERY_REPORT.into(),
        )
        .await?
        .into_multistatus()?;
    Ok(engine_provider::ScopeSync::new(
        contact_update(&report, book, writable, true),
        next,
    ))
}

pub(crate) fn contact_update(
    report: &MultiStatus,
    book: &AddressBookId,
    writable: bool,
    snapshot: bool,
) -> SyncUpdate<ContactCard> {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut present = BTreeSet::new();
    for response in &report.responses {
        if response.is_removed() {
            removed.extend(
                response
                    .hrefs
                    .iter()
                    .filter_map(|href| ProviderKey::new(href).ok()),
            );
            continue;
        }
        // The `present` set is derived from what the server *listed*, not from what
        // parsed. A snapshot's `present` is the store's "everything that exists
        // server-side", so anything missing from it is deleted locally — deriving it
        // from successfully-parsed cards would turn one unreadable vCard (or one
        // response the server sent without `address-data`) into silent data loss.
        present.extend(
            response
                .hrefs
                .iter()
                .filter_map(|href| ProviderKey::new(href).ok()),
        );
        if let Some(card) = normalize_response(response, book, writable) {
            changed.push(card);
        }
    }
    if snapshot {
        SyncUpdate::snapshot(changed, present)
    } else {
        SyncUpdate::delta(changed, removed)
    }
}

pub(crate) fn normalize_response(
    response: &DavResponse,
    book: &AddressBookId,
    writable: bool,
) -> Option<ContactCard> {
    let raw = response.props.get("address-data")?;
    let id = ContactId::try_from(response.href()).ok()?;
    let mut card = vcard::parse_vcard(raw, id, book.clone(), writable).ok()?;
    if let Some(etag) = response.props.get("getetag") {
        card.revisions = RevisionTokens::from_etag(ETag::new(etag));
    }
    Some(card)
}

pub(crate) fn multiget_report(href: &str) -> String {
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="utf-8"?>"#,
            r#"<a:addressbook-multiget xmlns:d="DAV:" xmlns:a="urn:ietf:params:xml:ns:carddav">"#,
            r#"<d:prop><d:getetag/><a:address-data/></d:prop><d:href>{}</d:href>"#,
            r#"</a:addressbook-multiget>"#
        ),
        xml_escape(href)
    )
}

pub(crate) fn bind_collection(home: &str, value: &str) -> Result<AddressBookId, CalDavError> {
    let href = if value.starts_with('/') || value.contains("://") {
        value.to_owned()
    } else {
        format!("{}/{}", home.trim_end_matches('/'), value.trim_matches('/'))
    };
    let href = format!("{}/", href.trim_end_matches('/'));
    AddressBookId::try_from(href.as_str()).map_err(|error| CalDavError::protocol(error.to_string()))
}

pub(crate) fn stable_suffix(card: &ContactCard) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    card.display_name().hash(&mut hasher);
    for email in card.emails.values() {
        email.value.address.hash(&mut hasher);
    }
    format!("contact-{:016x}", hasher.finish())
}

pub(crate) fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub(crate) fn decode_data_uri(value: &str) -> Result<Vec<u8>, CalDavError> {
    let (_, data) = value
        .split_once(',')
        .ok_or_else(|| CalDavError::protocol("malformed vCard data URI"))?;
    decode_base64(data)
}

fn decode_base64(value: &str) -> Result<Vec<u8>, CalDavError> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut bits = 0_u32;
    let mut count = 0_u8;
    for byte in value.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        if byte == b'=' {
            break;
        }
        let digit = alphabet
            .iter()
            .position(|candidate| *candidate == byte)
            .ok_or_else(|| CalDavError::protocol("invalid vCard photo base64"))?;
        bits = (bits << 6) | u32::try_from(digit).unwrap_or_default();
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(
                u8::try_from(bits >> count)
                    .map_err(|_| CalDavError::protocol("vCard photo base64 overflow"))?,
            );
            bits &= (1_u32 << count).saturating_sub(1);
        }
    }
    Ok(out)
}

pub(crate) fn supported_fields() -> ContactFieldSet {
    ContactFieldSet::from_fields([
        ContactField::Kind,
        ContactField::Name,
        ContactField::Emails,
        ContactField::Phones,
        ContactField::Notes,
        ContactField::Urls,
        ContactField::Keywords,
    ])
}

pub(crate) fn require_writable(writable: bool) -> ProviderResult<()> {
    if writable {
        Ok(())
    } else {
        Err(engine_provider::ProviderError::invalid_state(
            "CardDAV address book is read-only",
        ))
    }
}

pub(crate) fn contact_id(value: &str) -> Result<ContactId, CalDavError> {
    ContactId::try_from(value).map_err(|error| CalDavError::protocol(error.to_string()))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
