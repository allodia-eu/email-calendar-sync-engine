//! The two reads a host needs before it can write a contact: where a new card may go, and
//! which stored card a person's details actually live in.

use super::*;

#[tokio::test]
async fn address_books_and_person_sources_answer_where_a_write_would_land() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let account = AccountId::try_from("account-1").unwrap();
    engine.sync_contacts(&provider, &account).await.unwrap();

    let books = engine.address_books(&account).await.unwrap();
    assert_eq!(books.len(), 1, "{books:?}");
    assert_eq!(books[0].id.as_str(), "book");
    assert_eq!(books[0].name, "Contacts");
    assert!(books[0].is_writable);
    // An account that has never synced contacts has no books rather than an error, so a host
    // can ask before the first sync and simply offer nothing.
    let unsynced = AccountId::try_from("account-2").unwrap();
    assert!(engine.address_books(&unsynced).await.unwrap().is_empty());

    let ada = engine
        .people_page(&PeopleQuery {
            query: "ada".into(),
            limit: 10,
            ..PeopleQuery::default()
        })
        .await
        .unwrap()
        .people
        .remove(0);
    let sources = engine.person_sources(ada.id).await.unwrap();
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0].id.account, account);
    assert_eq!(sources[0].id.contact.as_str(), "c1");
    assert!(sources[0].writable);
    // The card itself comes back, not a reference to it: a patch needs the stored base, and a
    // second read to fetch it could observe a different generation.
    assert_eq!(
        sources[0].card.display_name().as_deref(),
        Some("Ada Lovelace")
    );

    // A person id that never existed resolves to nothing to edit rather than to an error.
    assert!(
        engine
            .person_sources(engine_api::PersonId::new(9999).unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}
