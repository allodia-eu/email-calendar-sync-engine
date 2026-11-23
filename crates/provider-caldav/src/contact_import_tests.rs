//! The CardDAV importer over replayed answers Stalwart and Nextcloud gave to the exact requests it
//! sends (`tests/fixtures/import/*-card-*`). Proven live in `tests/contact_import.rs`.

use std::sync::Arc;

use engine_core::{
    contact::VcardVersion,
    error::FailureClass,
    ids::{AccountId, AddressBookId, ContactId},
    raw::RawVcard,
    sync::SyncState,
};
use engine_provider::{
    IgnoreConnectSteps, Provider, ProviderResult,
    import::{ContactImport, ContactImporter, ImportedContact},
};

use crate::{
    CardDavProvider,
    carddav_tests::{BOOKS, PRINCIPAL},
    response::HttpResponse,
    test_support::{Replay, ok, options, status, wrote},
    transport::{DavMethod, Precondition},
};

const PROPS: &str = include_str!("../tests/fixtures/import/stalwart-card-import-props.xml");
const NC_PROPS: &str = include_str!("../tests/fixtures/import/nextcloud-card-import-props.xml");
const NONE_HELD: &str = include_str!("../tests/fixtures/import/stalwart-card-uid-query-empty.xml");
const HELD: &str = include_str!("../tests/fixtures/import/stalwart-card-uid-query.xml");
const NC_HELD: &str = include_str!("../tests/fixtures/import/nextcloud-card-uid-query.xml");
const CONFLICT: &str = include_str!("../tests/fixtures/import/stalwart-card-put-second-uid.xml");
const NC_CONFLICT: &str =
    include_str!("../tests/fixtures/import/nextcloud-card-put-second-uid.xml");
const INVALID: &str = include_str!("../tests/fixtures/import/stalwart-card-put-invalid.xml");
const NC_INVALID: &str = include_str!("../tests/fixtures/import/nextcloud-card-put-invalid.xml");

const CARD: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:capture-1@example.com\r\n\
                    FN:Captured Person\r\nEMAIL:captured@example.com\r\nEND:VCARD\r\n";
const HELD_HREF: &str = "/dav/card/bob%40test.local/laneb-capture/capture-1@example.com.vcf";

fn account() -> AccountId {
    AccountId::try_from("acct").unwrap()
}

fn book() -> AddressBookId {
    AddressBookId::try_from("/dav/addressbooks/alice/friends/").unwrap()
}

fn card(text: &str) -> ContactImport {
    ContactImport::new(RawVcard::new(text), book())
}

fn key(answer: &ImportedContact) -> Option<&str> {
    answer.key.as_ref().map(ContactId::as_str)
}

async fn target(after: Vec<HttpResponse>) -> (CardDavProvider, Arc<Replay>) {
    let mut responses = vec![ok(PRINCIPAL), options(Some("1, 3, addressbook")), ok(BOOKS)];
    responses.extend(after);
    let replay = Arc::new(Replay::new(responses));
    let provider = CardDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    (provider, replay)
}

async fn import_one(
    text: &str,
    after: Vec<HttpResponse>,
) -> (ProviderResult<ImportedContact>, Arc<Replay>) {
    let (provider, replay) = target(after).await;
    let mut answers = provider.import_contacts(&account(), &[card(text)]).await;
    assert_eq!(answers.len(), 1);
    (answers.remove(0), replay)
}

#[tokio::test]
async fn a_card_nobody_holds_is_stored_as_it_is_where_its_uid_names_it() {
    let (answer, replay) = import_one(
        CARD,
        vec![ok(PROPS), ok(NONE_HELD), wrote(201, Some("\"c1\""))],
    )
    .await;

    let stored = answer.expect("stored");
    assert_eq!(
        key(&stored),
        Some("/dav/addressbooks/alice/friends/capture-1@example.com.vcf")
    );
    assert!(!stored.already_there);
    let reads = replay.reads();
    let query = reads
        .iter()
        .find(|read| read.0 == DavMethod::Report)
        .expect("a lookup");
    assert_eq!(query.1, "/dav/addressbooks/alice/friends/");
    assert!(
        query
            .3
            .contains("match-type=\"equals\">capture-1@example.com<"),
        "{}",
        query.3
    );
    drop(reads);
    let put = &replay.writes()[0];
    assert_eq!(put.precondition, Precondition::IfNoneMatch);
    assert_eq!(put.content_type, Some("text/vcard; charset=utf-8"));
    assert_eq!(put.body, CARD, "a card is stored byte for byte");
}

#[tokio::test]
async fn a_uid_the_book_holds_is_already_there() {
    for held in [HELD, NC_HELD] {
        let (answer, replay) = import_one(CARD, vec![ok(PROPS), ok(held)]).await;
        let linked = answer.expect("linked");
        assert!(linked.already_there);
        assert!(key(&linked).is_some_and(|href| href.ends_with("capture-1@example.com.vcf")));
        assert!(replay.writes().is_empty());
    }
}

#[tokio::test]
async fn a_refusal_of_a_second_uid_links_to_the_card_it_names() {
    let (answer, _) = import_one(CARD, vec![ok(PROPS), ok(NONE_HELD), status(403, CONFLICT)]).await;
    assert_eq!(key(&answer.expect("linked")), Some(HELD_HREF));

    let (answer, _) = import_one(
        CARD,
        vec![ok(PROPS), ok(NONE_HELD), status(409, NC_CONFLICT)],
    )
    .await;
    assert_eq!(
        key(&answer.expect("linked")),
        Some("/remote.php/dav/addressbooks/users/alice/laneb-capture/capture-1@example.com.vcf")
    );

    let bare = "<d:error xmlns:d=\"DAV:\" xmlns:a=\"urn:ietf:params:xml:ns:carddav\">\
                <a:no-uid-conflict/></d:error>";
    let (answer, _) = import_one(
        CARD,
        vec![ok(PROPS), ok(NONE_HELD), status(409, bare), ok(HELD)],
    )
    .await;
    assert_eq!(key(&answer.expect("linked")), Some(HELD_HREF));
}

#[tokio::test]
async fn a_taken_name_is_an_earlier_attempt_and_a_refused_card_is_never_taken() {
    let (answer, _) = import_one(
        CARD,
        vec![ok(PROPS), ok(NONE_HELD), status(412, ""), status(200, CARD)],
    )
    .await;
    assert!(answer.expect("linked").already_there);

    for refusal in [
        status(412, INVALID),
        status(415, NC_INVALID),
        status(400, ""),
    ] {
        let (answer, _) = import_one(CARD, vec![ok(PROPS), ok(NONE_HELD), refusal]).await;
        let refused = answer.unwrap_err();
        assert!(refused.is_item_unavailable(), "{refused:?}");
        assert!(!refused.detail().contains("vCard object"));
    }
    let (answer, _) = import_one(CARD, vec![ok(PROPS), ok(NONE_HELD), status(504, "")]).await;
    assert!(answer.unwrap_err().requires_confirmation());
}

#[tokio::test]
async fn a_card_without_a_uid_is_named_by_its_bytes_and_not_looked_up() {
    let anonymous = CARD.replace("UID:capture-1@example.com\r\n", "");
    let (answer, replay) = import_one(&anonymous, vec![ok(PROPS), wrote(201, None)]).await;
    assert!(
        key(&answer.expect("stored"))
            .is_some_and(|href| href.starts_with("/dav/addressbooks/alice/friends/sha256-"))
    );
    assert!(
        replay
            .seen()
            .iter()
            .all(|(method, _)| *method != DavMethod::Report)
    );
}

#[tokio::test]
async fn what_is_not_one_card_or_not_a_version_the_book_takes_is_refused_unsent() {
    let two = format!("{CARD}{}", CARD.replace("capture-1", "capture-2"));
    let old = CARD.replace("VERSION:4.0", "VERSION:2.1");
    for text in [two.as_str(), "not a card", old.as_str()] {
        let (answer, replay) = import_one(text, vec![ok(NC_PROPS)]).await;
        assert!(answer.unwrap_err().is_item_unavailable(), "{text}");
        assert!(replay.writes().is_empty());
    }
}

#[tokio::test]
async fn the_limits_are_the_books_size_and_versions() {
    let (provider, _) = target(vec![ok(PROPS), ok(NC_PROPS), status(404, "")]).await;

    let stalwart = provider
        .contact_import_limits(&account(), &book())
        .await
        .unwrap();
    let nextcloud = provider
        .contact_import_limits(&account(), &book())
        .await
        .unwrap();

    assert_eq!(stalwart.max_card_size().map(u64::from), Some(524_288));
    assert_eq!(
        stalwart
            .versions()
            .map(|v| v.iter().copied().collect::<Vec<_>>()),
        Some(vec![VcardVersion::V2_1, VcardVersion::V3, VcardVersion::V4])
    );
    assert_eq!(
        nextcloud
            .versions()
            .map(|v| v.iter().copied().collect::<Vec<_>>()),
        Some(vec![VcardVersion::V3, VcardVersion::V4]),
        "jCard is not a vCard version"
    );
    assert!(
        provider
            .contact_import_limits(&account(), &book())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_book_stating_nothing_takes_any_version_and_unreadable_limits_are_none() {
    let bare = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/b/</D:href><D:propstat>\
                <D:prop><D:sync-token>t</D:sync-token></D:prop><D:status>HTTP/1.1 200 OK\
                </D:status></D:propstat></D:response></D:multistatus>";
    let (provider, replay) = target(vec![
        ok(bare),
        status(500, ""),
        ok(NONE_HELD),
        wrote(201, None),
    ])
    .await;
    let limits = provider
        .contact_import_limits(&account(), &book())
        .await
        .unwrap();
    assert_eq!(limits.versions(), None);
    assert_eq!(limits.max_card_size(), None);

    let answers = provider.import_contacts(&account(), &[card(CARD)]).await;
    assert!(answers[0].is_ok());
    assert_eq!(replay.writes().len(), 1);
}

#[tokio::test]
async fn the_cursor_is_the_books_sync_token_or_its_ctag() {
    let ctag_only = "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"http://calendarserver.org/ns/\">\
                     <D:response><D:href>/b/</D:href><D:propstat><D:prop><C:getctag>9</C:getctag>\
                     </D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>\
                     </D:multistatus>";
    let neither = "<D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>/b/</D:href></D:response>\
                   </D:multistatus>";
    let (provider, _) = target(vec![ok(PROPS), ok(ctag_only), ok(neither), status(503, "")]).await;

    assert_eq!(
        provider.import_cursor(&account(), &book()).await.unwrap(),
        SyncState::new("urn:stalwart:davsync:6")
    );
    assert_eq!(
        provider.import_cursor(&account(), &book()).await.unwrap(),
        SyncState::new("ctag:9")
    );
    assert_eq!(
        provider
            .import_cursor(&account(), &book())
            .await
            .unwrap_err()
            .class(),
        FailureClass::Permanent
    );
    assert_eq!(
        provider
            .import_cursor(&account(), &book())
            .await
            .unwrap_err()
            .class(),
        FailureClass::Retryable
    );
}

#[tokio::test]
async fn a_server_without_address_books_offers_neither_verb() {
    let replay = Arc::new(Replay::new(vec![ok(PRINCIPAL), options(Some("1, 3"))]));
    let plain = CardDavProvider::with_executor(
        Box::new(Arc::clone(&replay)),
        "/.well-known/carddav",
        "default",
        &IgnoreConnectSteps,
    )
    .await
    .expect("discovery");
    assert!(plain.contact_importer().is_none());
    assert!(plain.address_book_collections().is_none());
    let (with_books, _) = target(Vec::new()).await;
    assert!(with_books.contact_importer().is_some());
    assert!(with_books.address_book_collections().is_some());
}

/// From an independent review (R4) and its nits: the DTD defaults of `address-data-type`, a
/// quoted `:` before the `UID`, a book named by anything but its href.
mod review {
    use super::*;

    fn supported(inner: &str) -> String {
        format!(
            "<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"urn:ietf:params:xml:ns:carddav\">\
             <D:response><D:href>/b/</D:href><D:propstat><D:prop>\
             <C:supported-address-data>{inner}</C:supported-address-data></D:prop>\
             <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"
        )
    }

    #[tokio::test]
    async fn an_address_data_type_reads_with_rfc_6352s_defaults() {
        let v3 = Some(vec![VcardVersion::V3]);
        for (inner, expected) in [
            (
                "<C:address-data-type content-type=\"text/vcard\"/>",
                v3.clone(),
            ),
            ("<C:address-data-type/>", v3.clone()),
            (
                "<C:address-data-type content-type=\"text/vcard; charset=utf-8\" version=\"4.0\"/>",
                Some(vec![VcardVersion::V4]),
            ),
            (
                "<C:address-data-type content-type=\"TEXT/VCARD\" version=\"4.0\"/>",
                Some(vec![VcardVersion::V4]),
            ),
            (
                "<C:address-data-type content-type=\"application/vcard+json\" version=\"4.0\"/>",
                Some(Vec::new()),
            ),
        ] {
            let (provider, _) = target(vec![ok(&supported(inner))]).await;
            let limits = provider
                .contact_import_limits(&account(), &book())
                .await
                .unwrap();
            assert_eq!(
                limits
                    .versions()
                    .map(|v| v.iter().copied().collect::<Vec<_>>()),
                expected,
                "{inner}"
            );
        }
    }

    #[tokio::test]
    async fn a_quoted_colon_before_the_uid_is_not_its_end() {
        let quoted = CARD.replace(
            "UID:capture-1@example.com",
            "UID;X-NOTE=\"a:b\":capture-1@example.com",
        );
        let (answer, replay) = import_one(&quoted, vec![ok(PROPS), ok(HELD)]).await;
        assert!(answer.expect("linked").already_there);
        let reads = replay.reads();
        let query = reads
            .iter()
            .find(|read| read.0 == DavMethod::Report)
            .expect("looked up");
        assert!(query.3.contains(">capture-1@example.com<"), "{}", query.3);
    }

    #[tokio::test]
    async fn a_book_that_is_not_an_href_is_refused_before_anything_is_sent() {
        let (provider, replay) = target(vec![ok(PROPS)]).await;
        let nameless = ContactImport::new(
            RawVcard::new(CARD),
            AddressBookId::try_from("friends").unwrap(),
        );
        let answers = provider.import_contacts(&account(), &[nameless]).await;
        assert_eq!(
            answers[0].as_ref().unwrap_err().class(),
            FailureClass::Permanent
        );
        assert!(replay.writes().is_empty());
    }

    #[tokio::test]
    async fn a_name_taken_by_another_card_is_a_conflict() {
        let other = CARD.replace("capture-1@example.com", "someone-else@example.com");
        let (answer, _) = import_one(
            CARD,
            vec![
                ok(PROPS),
                ok(NONE_HELD),
                status(412, ""),
                status(200, &other),
            ],
        )
        .await;
        assert_eq!(answer.unwrap_err().class(), FailureClass::Conflict);
    }

    #[tokio::test]
    async fn a_book_that_refuses_every_card_says_so() {
        let (answer, _) = import_one(CARD, vec![ok(PROPS), ok(NONE_HELD), status(403, "")]).await;
        assert_eq!(
            answer.unwrap_err().target_refusal(),
            Some(engine_provider::TargetRefusal::ReadOnlyContainer)
        );
    }
}

/// A card without a `UID` taken at its own name is ours however the server rewrote it, a `UID` of
/// the server's own included; what is there must still be one card (found live: Stalwart stores a
/// 3.0 card as 4.0, Nextcloud adds a `UID`).
#[tokio::test]
async fn a_card_without_a_uid_meets_its_own_earlier_write() {
    let anonymous = CARD.replace("UID:capture-1@example.com\r\n", "");
    let stored = anonymous.replace("VERSION:4.0", "VERSION:4.0\r\nPRODID:-//Server//EN");
    let (answer, _) = import_one(
        &anonymous,
        vec![ok(PROPS), status(412, ""), status(200, &stored)],
    )
    .await;
    assert!(answer.expect("linked").already_there);

    let (answer, _) = import_one(
        &anonymous,
        vec![ok(PROPS), status(412, ""), status(200, CARD)],
    )
    .await;
    assert!(
        answer.expect("linked").already_there,
        "a UID the server gave it"
    );

    let (answer, _) = import_one(
        &anonymous,
        vec![ok(PROPS), status(412, ""), status(200, "not a card")],
    )
    .await;
    assert_eq!(answer.unwrap_err().class(), FailureClass::Conflict);
}

/// A throttle or a refused credential ends a card batch as it ends an event batch.
#[tokio::test]
async fn a_throttle_or_a_refused_credential_ends_the_batch() {
    let throttled = include_str!("../tests/fixtures/concurrency-limit-stalwart.json");
    for (ending, class) in [
        (status(400, throttled), FailureClass::RateLimited),
        (status(401, ""), FailureClass::Authentication),
    ] {
        let (provider, replay) = target(vec![ok(PROPS), ok(NONE_HELD), ending]).await;
        let second = CARD.replace("capture-1@", "capture-2@");

        let answers = provider
            .import_contacts(&account(), &[card(CARD), card(&second)])
            .await;

        assert_eq!(answers.len(), 2);
        for answer in &answers {
            assert_eq!(answer.as_ref().unwrap_err().class(), class);
        }
        assert!(replay.writes().len() == 1, "one PUT, then nothing");
    }
}
