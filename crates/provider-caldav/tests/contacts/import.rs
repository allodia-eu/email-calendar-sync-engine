//! The live contact import scenario, run against both harness servers with CardDAV (Stalwart,
//! Nextcloud): create an address book, import into it, import again, meet a `UID` someone else
//! stored, and read it all back through the book's own delta from the import cursor.
//!
//! What only a real server answers: the extended `MKCOL` and what it keeps, the vCard versions a
//! book takes, whether the `addressbook-query` with `match-type="equals"` is a shape it takes, how
//! it refuses a second `UID`, and whether a rich card comes back with every property.

use engine_core::{
    contact::{ContactCard, VcardVersion},
    ids::{AccountId, AddressBookId, ContactId},
    raw::RawVcard,
    sync::SyncUpdate,
};
use engine_provider::{
    ContactSourceSync, ContactsProvider, NewAddressBook, Provider,
    import::{ContactImport, ImportedContact},
};
use futures_util::FutureExt;
use provider_caldav::{CardDavConfig, CardDavProvider, Credentials};

/// Where a harness server is and who signs in.
pub(crate) struct DavServer {
    pub(crate) base: String,
    pub(crate) user: String,
    pub(crate) pass: String,
}

/// The name every scratch book this scenario makes is given, so an interrupted run is swept.
const SCRATCH: &str = "Import scratch (live suite)";

/// The survey's rich vCard 4.0: 26 properties, a `data:` photo, a group-less `RELATED`, an `X-`.
fn rich(uid: &str) -> String {
    [
        "BEGIN:VCARD",
        "VERSION:4.0",
        &format!("UID:{uid}"),
        "KIND:individual",
        "FN:Rich Person",
        "N:Person;Rich;Middle;Dr.;Jr.",
        "NICKNAME:Richie",
        "EMAIL;TYPE=work;PREF=1:rich.work@example.com",
        "EMAIL;TYPE=home:rich.home@example.org",
        "TEL;VALUE=uri;TYPE=cell:tel:+31-6-0000-0004",
        "ADR;TYPE=home:;;1 Example Street;Exampletown;;1234 AB;Netherlands",
        "ORG:Example Org;Unit",
        "TITLE:Engineer",
        "ROLE:Lead",
        "BDAY:19800102",
        "ANNIVERSARY:20100605",
        "NOTE:A note\\, with comma",
        "URL:https://www.example.com/rich",
        "PHOTO:data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4\
         2mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        "CATEGORIES:friends,work",
        "IMPP:xmpp:rich@example.com",
        "GENDER:M",
        "LANG:nl",
        "TZ:Europe/Amsterdam",
        "GEO:geo:52.37,4.89",
        "RELATED;TYPE=spouse:urn:uuid:live-import-spouse",
        "X-CUSTOM-THING:kept?",
        "END:VCARD",
        "",
    ]
    .join("\r\n")
}

/// A vCard 3.0 card.
fn three(uid: &str) -> String {
    format!(
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:Three Person\r\nN:Person;Three;;;\r\n\
         EMAIL;TYPE=INTERNET,WORK:three@example.com\r\nEND:VCARD\r\n"
    )
}

/// The whole scenario.
pub(crate) async fn import_round_trip(
    provider: &CardDavProvider,
    account: &AccountId,
    server: &DavServer,
) {
    sweep(provider, account, server).await;
    let book = provider
        .address_book_collections()
        .expect("CardDAV creates address books")
        .create_address_book(
            account,
            &NewAddressBook::new(SCRATCH).with_description("Imported by the live suite"),
        )
        .await
        .expect("extended MKCOL");
    assert_eq!(book.name, SCRATCH);
    assert_eq!(
        book.description.as_deref(),
        Some("Imported by the live suite")
    );
    assert!(book.is_writable);

    let outcome = std::panic::AssertUnwindSafe(steps(provider, account, &book.id, server))
        .catch_unwind()
        .await;
    remove(server, book.id.as_str()).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn steps(
    provider: &CardDavProvider,
    account: &AccountId,
    book: &AddressBookId,
    server: &DavServer,
) {
    let importer = provider.contact_importer().expect("CardDAV imports cards");
    let limits = importer
        .contact_import_limits(account, book)
        .await
        .expect("limits");
    let versions = limits.versions().expect("both servers name their versions");
    assert!(versions.contains(&VcardVersion::V3) && versions.contains(&VcardVersion::V4));
    assert!(limits.max_card_size().is_some());
    let cursor = importer.import_cursor(account, book).await.expect("cursor");

    let (person, legacy) = (
        "live-import-rich@example.com",
        "live-import-three@example.com",
    );
    let anonymous = three("x").replace("UID:x\r\n", "");
    let batch = [
        ContactImport::new(RawVcard::new(rich(person)), book.clone()),
        ContactImport::new(RawVcard::new(three(legacy)), book.clone()),
        ContactImport::new(RawVcard::new(anonymous), book.clone()),
        ContactImport::new(RawVcard::new("not a card"), book.clone()),
        ContactImport::new(RawVcard::new(rich(person)), book.clone()),
    ];
    let answers = importer.import_contacts(account, &batch).await;
    assert_eq!(answers.len(), 5);
    for answer in &answers[..3] {
        assert!(
            !answer.as_ref().expect("stored").already_there,
            "{answers:?}"
        );
    }
    assert!(answers[3].as_ref().unwrap_err().is_item_unavailable());
    assert_eq!(
        answers[4].as_ref().expect("linked"),
        &ImportedContact::existing(answers[0].as_ref().unwrap().key.clone()),
        "a UID the batch just stored is not stored twice"
    );

    let again = importer.import_contacts(account, &batch[..3]).await;
    for (answer, earlier) in again.iter().zip(&answers) {
        let answer = answer.as_ref().expect("a re-import is answered");
        assert!(
            answer.already_there,
            "a re-import stores nothing: {answer:?}"
        );
        assert_eq!(answer.key, earlier.as_ref().unwrap().key);
    }

    let foreign = "live-import-foreign@example.com";
    let elsewhere = format!("{}someone-else.vcf", book.as_str());
    put(server, &elsewhere, &three(foreign)).await;
    let linked = importer
        .import_contacts(
            account,
            &[ContactImport::new(
                RawVcard::new(three(foreign)),
                book.clone(),
            )],
        )
        .await;
    let linked = linked[0].as_ref().expect("answered");
    assert!(linked.already_there, "{linked:?}");
    assert_eq!(
        linked.key.as_ref().map(ContactId::as_str),
        Some(elsewhere.as_str())
    );

    let bound = CardDavProvider::connect(
        CardDavConfig::new(
            server.base.clone(),
            Credentials::Basic {
                username: server.user.clone(),
                password: server.pass.clone(),
            },
            engine_http::test_server::user_agent(),
        )
        .with_address_book(book.as_str()),
    )
    .await
    .expect("connect bound to the scratch book");
    let ContactSourceSync::Available { sync, .. } = bound
        .sync_contacts(account, Some(&cursor))
        .await
        .expect("the delta from the import cursor")
    else {
        panic!("the book is available");
    };
    let cards: Vec<ContactCard> = match sync.update {
        SyncUpdate::Snapshot { objects, .. } => objects,
        SyncUpdate::Delta { changed, .. } => changed,
    };
    assert_eq!(
        cards.len(),
        4,
        "the rich, the 3.0, the anonymous and the foreign card"
    );
    let stored = cards
        .iter()
        .find(|card| card.uid.as_deref() == Some(person))
        .expect("the rich card");
    every_property_kept(stored, &rich(person));
}

/// Every property the rich card was sent with is in the stored card, whatever the server did to
/// its spelling (Stalwart upper-cases `TYPE` values; Nextcloud stores as sent).
fn every_property_kept(stored: &ContactCard, sent: &str) {
    let names = |text: &str| -> std::collections::BTreeSet<String> {
        text.replace("\r\n ", "")
            .lines()
            .filter_map(|line| line.split([';', ':']).next())
            .map(|name| name.trim().to_ascii_uppercase())
            .filter(|name| !name.is_empty())
            .collect()
    };
    let held = names(stored.raw_vcard.as_ref().expect("raw").as_str());
    let missing: Vec<String> = names(sent).difference(&held).cloned().collect();
    assert!(missing.is_empty(), "the server dropped {missing:?}");
}

/// Removes every scratch book an interrupted run left behind.
async fn sweep(provider: &CardDavProvider, account: &AccountId, server: &DavServer) {
    let ContactSourceSync::Available { sync, .. } = provider
        .sync_address_books(account, None)
        .await
        .expect("list address books")
    else {
        return;
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        return;
    };
    for book in objects.iter().filter(|book| book.name == SCRATCH) {
        remove(server, book.id.as_str()).await;
    }
}

fn client(server: &DavServer, method: reqwest::Method, href: &str) -> reqwest::RequestBuilder {
    reqwest::Client::new()
        .request(method, format!("{}{href}", server.base))
        .basic_auth(&server.user, Some(&server.pass))
}

/// Another client storing a card under a name of its own.
async fn put(server: &DavServer, href: &str, card: &str) {
    let answer = client(server, reqwest::Method::PUT, href)
        .header("Content-Type", "text/vcard; charset=utf-8")
        .body(card.to_owned())
        .send()
        .await
        .expect("PUT");
    assert!(answer.status().is_success(), "{}", answer.status());
}

/// `DELETE` of an address book, which no engine verb does.
async fn remove(server: &DavServer, href: &str) {
    let answer = client(server, reqwest::Method::DELETE, href)
        .send()
        .await
        .expect("DELETE the scratch book");
    assert!(answer.status().is_success(), "{}", answer.status());
}
