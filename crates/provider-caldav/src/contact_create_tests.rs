//! The extended `MKCOL` and its read-back, over the answers Stalwart gave to the same requests.

use std::sync::Arc;

use engine_core::{error::FailureClass, ids::AccountId};
use engine_provider::{AddressBookCollections, IgnoreConnectSteps, NewAddressBook};

use crate::{
    CardDavProvider,
    carddav_tests::{BOOKS, PRINCIPAL},
    response::HttpResponse,
    test_support::{Replay, ok, options, status, wrote},
    transport::DavMethod,
};

const CREATED: &str = include_str!("../tests/fixtures/import/stalwart-card-created-listing.xml");

async fn create(
    after: Vec<HttpResponse>,
) -> (
    engine_provider::ProviderResult<engine_core::contact::AddressBook>,
    Arc<Replay>,
) {
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
    let new = NewAddressBook::new("Imported & Kept").with_description("From the source");
    let made = provider
        .create_address_book(&AccountId::try_from("acct").unwrap(), &new)
        .await;
    (made, replay)
}

#[tokio::test]
async fn an_address_book_is_made_under_the_home_and_answered_as_held() {
    let (made, replay) = create(vec![wrote(201, None), ok(CREATED)]).await;

    let book = made.expect("created");
    assert_eq!(book.name, "Imported & Kept");
    assert_eq!(book.description.as_deref(), Some("From the source"));
    assert!(book.is_writable);
    let writes = replay.writes();
    let mkcol = &writes[0];
    assert_eq!(mkcol.method, DavMethod::Mkcol);
    assert!(mkcol.href.starts_with("/dav/addressbooks/alice/") && mkcol.href.ends_with('/'));
    assert!(mkcol.body.contains("<a:addressbook/>"));
    assert!(
        mkcol
            .body
            .contains("<d:displayname>Imported &amp; Kept</d:displayname>")
    );
    assert!(
        mkcol
            .body
            .contains("<a:addressbook-description>From the source<")
    );
}

#[tokio::test]
async fn a_refused_or_unreadable_create_says_what_is_known() {
    let (made, _) = create(vec![status(403, "private")]).await;
    let refused = made.unwrap_err();
    assert_eq!(refused.class(), FailureClass::Permanent);
    assert!(!refused.detail().contains("private"));

    let (made, _) = create(vec![wrote(201, None), status(500, "")]).await;
    assert!(made.unwrap_err().requires_confirmation());

    let calendar = include_str!("../tests/fixtures/import/stalwart-created-listing.xml");
    let (made, _) = create(vec![wrote(201, None), ok(calendar)]).await;
    assert!(
        made.unwrap_err().requires_confirmation(),
        "not an address book"
    );
}

#[test]
fn a_bare_address_book_sends_only_its_name() {
    let body = crate::request::mkcol_address_book("Plain", None);
    assert!(!body.contains("addressbook-description"));
}
