//! Google People **request/response shape** contracts.
//!
//! Split from `contact_tests.rs` by responsibility — and to keep both files under the
//! line limit. What these pin is the shape of what goes on the wire and what comes
//! back off it, the class of defect an offline fake cannot catch on its own: a fixture
//! that omits a field the real API always sends, or a request the fake would route
//! either way.

use engine_core::{
    ids::AccountId,
    sync::{SyncState, SyncUpdate},
};
use engine_provider::{ContactSourceSync, ContactsProvider};
use serde_json::json;

use crate::{GoogleContactProvider, test_support::fake_client};

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

/// The People API stamps every field of a person with the **same**
/// `metadata.source.id` — it identifies the source *record*, not the field. Keying
/// property ids on it collapsed each multi-valued field to its last entry. The
/// snapshot fixtures in `contact_tests.rs` omit `metadata.source` entirely and so
/// could never catch it; this one carries the shape a real account returns.
#[tokio::test]
async fn multi_valued_fields_survive_a_shared_source_id() {
    let source = json!({ "source": { "type": "CONTACT", "id": "1a2b3c" }, "primary": true });
    let client = fake_client(vec![(
        "/v1/people/me/connections",
        json!({
            "connections": [{
                "resourceName": "people/c1",
                "emailAddresses": [
                    { "value": "ada@example.test", "type": "work", "metadata": source },
                    { "value": "ada@analytical.example", "type": "home", "metadata": source },
                ],
                "phoneNumbers": [
                    { "value": "+44 100", "type": "mobile", "metadata": source },
                    { "value": "+44 200", "type": "work", "metadata": source },
                ],
                "urls": [
                    { "value": "https://one.example", "metadata": source },
                    { "value": "https://two.example", "metadata": source },
                ],
                "addresses": [
                    { "streetAddress": "1 First St", "metadata": source },
                    { "streetAddress": "2 Second St", "metadata": source },
                ],
                "organizations": [
                    { "name": "Acme", "title": "Engineer", "metadata": source },
                    { "name": "Analytical", "title": "Analyst", "metadata": source },
                ],
                "birthdays": [
                    { "date": { "year": 1815, "month": 12, "day": 10 }, "metadata": source },
                    { "date": { "year": 1852, "month": 11, "day": 27 }, "metadata": source },
                ],
            }],
            "nextSyncToken": "token-1"
        }),
    )]);
    let result = GoogleContactProvider::connections(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap();
    let engine_provider::ContactSourceSync::Available { sync, .. } = result else {
        panic!("expected source");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let card = &objects[0];
    assert_eq!(card.emails.len(), 2, "{:?}", card.emails);
    assert_eq!(card.phones.len(), 2, "{:?}", card.phones);
    assert_eq!(card.urls.len(), 2);
    assert_eq!(card.addresses.len(), 2);
    assert_eq!(card.organizations.len(), 2);
    assert_eq!(card.titles.len(), 2);
    assert_eq!(card.anniversaries.len(), 2);
}

/// Sync and page tokens are opaque server strings spliced into a query. Splicing one
/// raw lets a token containing `&` or `=` add parameters of its own — the client would
/// then fetch a page the server never named. The route below only matches the encoded
/// spelling, so a regression fails as a routing miss rather than a silent wrong page.
#[tokio::test]
async fn opaque_tokens_are_percent_encoded_into_the_query() {
    let hostile = "tok&pageSize=1";
    let result = GoogleContactProvider::other_contacts(fake_client(vec![(
        "syncToken=tok%26pageSize%3D1",
        json!({ "otherContacts": [], "nextSyncToken": "next" }),
    )]))
    .sync_contacts(&account(), Some(&SyncState::new(hostile)))
    .await
    .unwrap();
    assert!(matches!(result, ContactSourceSync::Available { .. }));
}
