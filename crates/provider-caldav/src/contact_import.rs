//! Importing cards into CardDAV address books (the `import` feature, `providers.md` → "Import").
//!
//! The calendar importer's shape (`calendar_import`), with what a card changes:
//!
//! - **The card is stored as it is.** A vCard schedules nothing, so there is nothing to suppress,
//!   and its version is the source's: a book that does not name it in `supported-address-data`
//!   refuses it before anything is sent ([`ContactImportLimits::check`]); converting is the job of
//!   a vCard writer that converts between versions (not yet in the engine), not an import.
//! - **The `UID` is looked up with `addressbook-query`**, `match-type="equals"` (RFC 6352 §10.5.4),
//!   and the answer still checked, before the `PUT`. Stalwart and Nextcloud refuse a second card
//!   with a `UID` and name the first (`403` and `409` `no-uid-conflict`), which reads the same as
//!   for events (`dav_import`); the lookup is for a server that does not, as SabreDAV's stock
//!   backend does not for events (SABREDAV-1). It runs live on both (a re-import and another
//!   client's card are found by it), but the harness has no CardDAV server that would otherwise
//!   store the duplicate.
//! - **A card without a `UID`** (vCard 3.0 makes it optional) is named by the hash of its bytes, so
//!   a retry meets its own earlier write; it cannot be looked up.
//!
//! The name is [`resource_name`]'s, as for events: both servers list `uid@example.com.vcf` for a
//! `PUT` to `uid%40example.com.vcf` (observed live).

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use engine_core::{
    contact::VcardVersion,
    ids::{AccountId, AddressBookId, ContactId},
    sync::SyncState,
    version::{ETag, RevisionTokens},
};
use engine_provider::{
    ProviderError, ProviderResult,
    import::{ContactImport, ContactImportLimits, ContactImporter, ImportedContact, not_sent},
};
use quick_xml::{Reader, events::Event};

use crate::{
    CardDavProvider,
    dav::MultiStatus,
    dav_import::{PutAnswer, cursor_of, ending, holder, max_size, name_held_by, read_put},
    error::CalDavError,
    href::{collection_href, hashed_name, resource_name},
    request::{CONTACT_IMPORT_PROPFIND, card_uid_query},
    transport::{DavExecutor, DavMethod, Precondition, WriteRequest},
    vcard::unfold,
};

#[async_trait]
impl ContactImporter for CardDavProvider {
    async fn contact_import_limits(
        &self,
        _account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<ContactImportLimits> {
        let (props, body) = properties(self.executor.as_ref(), address_book)
            .await
            .map_err(|err| err.content_free("reading the address book's limits"))?;
        Ok(limits_of(&props, &body))
    }

    async fn import_cursor(
        &self,
        _account: &AccountId,
        address_book: &AddressBookId,
    ) -> ProviderResult<SyncState> {
        let (props, _) = properties(self.executor.as_ref(), address_book)
            .await
            .map_err(|err| err.content_free("reading the address book's cursor"))?;
        cursor_of(&props).ok_or_else(|| {
            ProviderError::permanent(
                "reading the address book's cursor: it offers neither a sync-token nor a CTag",
            )
        })
    }

    async fn import_contacts(
        &self,
        _account: &AccountId,
        batch: &[ContactImport],
    ) -> Vec<ProviderResult<ImportedContact>> {
        let mut limits: BTreeMap<&AddressBookId, ContactImportLimits> = BTreeMap::new();
        let mut answers = Vec::with_capacity(batch.len());
        for card in batch {
            if !limits.contains_key(&card.address_book) {
                // A book whose limits cannot be read states none; the server's refusal remains.
                let stated = properties(self.executor.as_ref(), &card.address_book)
                    .await
                    .map_or_else(
                        |_| ContactImportLimits::new(),
                        |(props, body)| limits_of(&props, &body),
                    );
                limits.insert(&card.address_book, stated);
            }
            let answer = match limits[&card.address_book].check(card) {
                Ok(()) => self.import_card(card).await,
                Err(refused) => Err(refused),
            };
            // A throttle or a refused credential reaches every card after it.
            if let Some(ended) = ending(&answer).map(not_sent) {
                answers.push(answer);
                answers.resize_with(batch.len(), || Err(not_sent(&ended)));
                break;
            }
            answers.push(answer);
        }
        answers
    }
}

impl CardDavProvider {
    /// Imports one card: look its `UID` up, then `PUT` it where nothing is.
    async fn import_card(&self, card: &ContactImport) -> ProviderResult<ImportedContact> {
        let text = card.vcard.as_str();
        let uid = match one_card(text) {
            Some(OneCard::Uid(uid)) => Some(uid),
            Some(OneCard::NoUid) => None,
            None => {
                return Err(ProviderError::item_unavailable(
                    "importing a card: the text is not one vCard",
                ));
            }
        };
        let collection = collection_href(card.address_book.as_str())
            .map_err(|err| err.content_free("importing a card"))?;
        let name = uid
            .as_deref()
            .map_or_else(|| hashed_name(text.as_bytes()), resource_name);
        let href = format!("{collection}{name}.vcf");
        let key = |href: &str| ContactId::try_from(href).ok();
        // A card is the one asked for when it has the same `UID`. A card without one is named by
        // the SHA-256 of its bytes, and the server may rewrite what it stores (Stalwart writes 3.0
        // as 4.0, Nextcloud gives the card a `UID` of its own: observed), so any one card at that
        // name is taken as ours: only those bytes lead to it.
        let holds = |data: &str| match (&uid, one_card(data)) {
            (Some(uid), Some(OneCard::Uid(held))) => held == *uid,
            (None, Some(_)) => true,
            _ => false,
        };
        let held = async || match &uid {
            Some(uid) => holder(
                self.executor.as_ref(),
                &collection,
                card_uid_query(uid),
                "address-data",
                holds,
            )
            .await
            .map(|found| found.as_deref().and_then(key)),
            None => Ok(None),
        };
        if let Some(existing) = held().await? {
            return Ok(ImportedContact::existing(Some(existing)));
        }
        let response = self
            .executor
            .send_write(WriteRequest {
                method: DavMethod::Put,
                href: href.clone(),
                content_type: Some("text/vcard; charset=utf-8"),
                precondition: Precondition::IfNoneMatch,
                body: text.to_owned(),
            })
            .await
            .map_err(|err| err.content_free("importing a card"))?;
        match read_put(response) {
            PutAnswer::Stored(etag) => Ok(ImportedContact::created(
                key(&href).ok_or_else(|| {
                    ProviderError::needs_confirmation(
                        "importing a card: it was stored under a name the engine cannot hold",
                    )
                })?,
                etag.map_or_else(RevisionTokens::none, |etag| {
                    RevisionTokens::from_etag(ETag::new(etag))
                }),
            )),
            PutAnswer::NameTaken => {
                name_held_by(self.executor.as_ref(), &href, holds).await?;
                Ok(ImportedContact::existing(key(&href)))
            }
            PutAnswer::HeldBy(Some(named)) => Ok(ImportedContact::existing(key(&named))),
            PutAnswer::HeldBy(None) => Ok(ImportedContact::existing(held().await.ok().flatten())),
            PutAnswer::Failed(err) => Err(err.refused_write("importing a card")),
        }
    }
}

/// What the one vCard a text holds is named by.
enum OneCard {
    /// Its `UID`.
    Uid(String),
    /// Nothing: it has no `UID` (optional in vCard 3.0).
    NoUid,
}

/// The one vCard `text` holds, or `None` when the text is not exactly one vCard.
fn one_card(text: &str) -> Option<OneCard> {
    let lines = unfold(text);
    let count = |marker: &str| {
        lines
            .iter()
            .filter(|line| line.trim().eq_ignore_ascii_case(marker))
            .count()
    };
    if count("BEGIN:VCARD") != 1 || count("END:VCARD") != 1 {
        return None;
    }
    // The value starts after the first `:` outside a quoted parameter (RFC 6350 §3.3).
    let uid = lines.iter().find_map(|line| {
        let mut quoted = false;
        let colon = line.char_indices().find_map(|(at, c)| {
            if c == '"' {
                quoted = !quoted;
            }
            (c == ':' && !quoted).then_some(at)
        })?;
        let (head, value) = (&line[..colon], line[colon + 1..].trim());
        let name = head.split(';').next()?.rsplit('.').next()?;
        (name.trim().eq_ignore_ascii_case("UID") && !value.is_empty()).then(|| value.to_owned())
    });
    Some(uid.map_or(OneCard::NoUid, OneCard::Uid))
}

/// The address book's import properties, parsed, and the body they came in, which holds the
/// versions as attributes the multistatus parser does not keep.
async fn properties(
    exec: &dyn DavExecutor,
    book: &AddressBookId,
) -> Result<(MultiStatus, String), CalDavError> {
    let response = exec
        .send(
            DavMethod::Propfind,
            book.as_str(),
            "0",
            CONTACT_IMPORT_PROPFIND.to_owned(),
        )
        .await?;
    let body = response.body.clone();
    Ok((response.into_multistatus()?, body))
}

/// What the address book states of the cards it takes.
fn limits_of(props: &MultiStatus, body: &str) -> ContactImportLimits {
    let limits = match max_size(props) {
        Some(max) => ContactImportLimits::new().with_max_card_size(max),
        None => ContactImportLimits::new(),
    };
    match vcard_versions(body) {
        Some(versions) => limits.with_versions(versions),
        None => limits,
    }
}

/// The vCard versions a `supported-address-data` names (RFC 6352 §6.2.2): each
/// `address-data-type` whose media type, its parameters aside, is `text/vcard`. The DTD's
/// defaults apply (§10.3): a type naming no `content-type` is `text/vcard`, and one naming no
/// `version` is 3.0. `None` where the body names no `address-data-type`, so a server that states
/// nothing is not read as taking nothing; a book naming only other media types takes no vCard.
fn vcard_versions(body: &str) -> Option<BTreeSet<VcardVersion>> {
    let mut reader = Reader::from_str(body);
    let mut versions = BTreeSet::new();
    let mut stated = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element) | Event::Empty(element))
                if element
                    .local_name()
                    .into_inner()
                    .eq_ignore_ascii_case("address-data-type") =>
            {
                stated = true;
                let attribute = |name: &str| {
                    element
                        .try_get_attribute(name)
                        .ok()
                        .flatten()
                        .map(|value| value.value.into_owned())
                };
                let media = attribute("content-type").unwrap_or_else(|| "text/vcard".to_owned());
                let bare = media.split(';').next().unwrap_or_default().trim();
                if bare.eq_ignore_ascii_case("text/vcard")
                    && let Some(version) =
                        VcardVersion::from_token(attribute("version").as_deref().unwrap_or("3.0"))
                {
                    versions.insert(version);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    stated.then_some(versions)
}

#[cfg(test)]
#[path = "contact_import_tests.rs"]
mod tests;
