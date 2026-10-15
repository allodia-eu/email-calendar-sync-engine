//! Forgetting one account's contacts: the people index is rebuilt from what remains.

use engine_api::SearchDomain;

use super::*;

/// The accounts the stored people index says Ada's cards come from. Read through `person`,
/// which answers from the index as stored, so a missed rebuild shows here.
async fn ada_sources(engine: &Engine) -> Vec<AccountId> {
    let people = engine
        .people_page(&PeopleQuery {
            query: "ada".into(),
            limit: 10,
            ..PeopleQuery::default()
        })
        .await
        .unwrap()
        .people;
    let Some(ada) = people.first() else {
        return Vec::new();
    };
    engine
        .person(ada.id)
        .await
        .unwrap()
        .unwrap()
        .sources
        .into_iter()
        .map(|source| source.account)
        .collect()
}

#[tokio::test]
async fn forgetting_one_accounts_contacts_leaves_the_person_the_other_account_holds() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = FakeContacts::default();
    let first = AccountId::try_from("account-1").unwrap();
    let second = AccountId::try_from("account-2").unwrap();
    engine.sync_contacts(&provider, &first).await.unwrap();
    engine.sync_contacts(&provider, &second).await.unwrap();
    // The same address on both accounts is one person with two source cards.
    assert_eq!(ada_sources(&engine).await.len(), 2);

    engine
        .forget_account_domain(&first, SearchDomain::Contacts)
        .await
        .unwrap();

    // No person still lists a card the forgotten account held, without a sync having run.
    assert_eq!(ada_sources(&engine).await, core::slice::from_ref(&second));
    assert!(engine.address_books(&first).await.unwrap().is_empty());
    assert_eq!(engine.address_books(&second).await.unwrap().len(), 1);

    // Syncing the account's contacts again brings its card back.
    engine.sync_contacts(&provider, &first).await.unwrap();
    assert_eq!(ada_sources(&engine).await.len(), 2);
}
