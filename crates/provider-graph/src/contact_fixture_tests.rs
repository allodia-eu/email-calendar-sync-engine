//! Contact normalization and sync driven against the **captured** Graph responses in
//! `tests/fixtures/contacts/`, rather than hand-written JSON.
//!
//! The hand-written cases in `contact_tests.rs` prove the orchestration branches; these
//! prove the adapter against object shapes a real personal `outlook.com` account
//! actually returned — the empty-object addresses, the `null`-vs-`""` name components,
//! the `@odata.type` on delta entries, the `changeKey` advance across a write, and the
//! `@removed` tombstone. See `tests/fixtures/README.md` for how they were captured.

use engine_core::{
    contact::{ContactSourceClass, PropertyId},
    ids::{AccountId, AddressBookId},
    sync::SyncUpdate,
};
use engine_provider::{ContactSourceSync, ContactsProvider};

use crate::{
    GraphContactProvider,
    test_support::{fake_client, fake_client_fallible, json},
};

const DELTA_SNAPSHOT: &str =
    include_str!("../tests/fixtures/contacts/contacts_delta_snapshot.json");
const DELTA_CHANGED: &str = include_str!("../tests/fixtures/contacts/contacts_delta_changed.json");
const DELTA_REMOVED: &str = include_str!("../tests/fixtures/contacts/contacts_delta_removed.json");
const DETAIL: &str = include_str!("../tests/fixtures/contacts/contact_detail.json");
const CREATED: &str = include_str!("../tests/fixtures/contacts/contact_created.json");
const PATCHED: &str = include_str!("../tests/fixtures/contacts/contact_patched.json");
const FOLDERS: &str = include_str!("../tests/fixtures/contacts/contact_folders.json");
const CHILD_FOLDERS: &str = include_str!("../tests/fixtures/contacts/child_folders.json");
const MSA_UNSUPPORTED: &str = include_str!("../tests/fixtures/error/contacts_msa_unsupported.json");
const DIRECTORY_UNAUTHORIZED: &str =
    include_str!("../tests/fixtures/error/contacts_directory_unauthorized.json");

fn account() -> AccountId {
    AccountId::try_from("account-1").unwrap()
}

fn property(id: &str) -> PropertyId {
    PropertyId::new(id).unwrap()
}

/// The field-complete captured contact normalizes every component the `$select` asks
/// for. Values are asserted against what Graph *returned*, not what was sent.
#[tokio::test]
async fn captured_contact_normalizes_every_selected_field() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_SNAPSHOT))]);
    let provider = GraphContactProvider::personal(client);
    let ContactSourceSync::Available { sync, .. } =
        provider.sync_contacts(&account(), None).await.unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    assert_eq!(objects.len(), 2, "both mailbox contacts");
    let card = &objects[0];

    // Name: Graph splits the components, and `displayName` is the assembled full name.
    let name = card.name.as_ref().expect("name");
    assert_eq!(name.full.as_deref(), Some("Ada Byron Lovelace"));
    let components: Vec<&str> = name.components.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(components, ["Dr.", "Ada", "Byron", "Lovelace"]);

    // Two `emailAddresses`, each keeping its Graph `name` as the property label.
    assert_eq!(card.emails.len(), 2);
    assert_eq!(
        card.emails[&property("email-0")].value.address,
        "ada.lovelace@example.test"
    );
    assert_eq!(
        card.emails[&property("email-0")].label.as_deref(),
        Some("Ada Lovelace")
    );

    // Phones flatten three Graph fields into one list; only the mobile is featured.
    assert_eq!(card.phones.len(), 3);
    assert!(
        card.phones[&property("phone-2")]
            .value
            .features
            .contains("mobile")
    );
    assert!(card.phones[&property("phone-0")].contexts.contains("work"));

    // `businessAddress` + `homeAddress` populate; the captured `otherAddress` is `{}`
    // and must not produce an empty address property.
    assert_eq!(card.addresses.len(), 2);
    let business = &card.addresses[&property("business")].value;
    assert_eq!(business.components["locality"], vec!["Amsterdam"]);
    assert_eq!(business.components["postcode"], vec!["1015 CJ"]);
    // `countryOrRegion` is a full country name, so no 2-letter country code is derived.
    assert_eq!(business.country_code, None);

    assert_eq!(
        card.organizations[&property("organization")].value.name,
        "Analytical Engines BV"
    );
    assert_eq!(
        card.organizations[&property("organization")].value.units[0].name,
        "Research"
    );
    assert_eq!(
        card.titles[&property("job-title")].value.name,
        "Chief Engineer"
    );
    assert_eq!(
        card.urls[&property("business-homepage")].value.uri,
        "https://example.test/ada"
    );
    assert!(card.notes.len() == 1);

    assert_eq!(
        card.revisions
            .change_key
            .as_ref()
            .map(engine_core::version::ChangeKey::as_str),
        Some("change-key-1")
    );
    assert!(card.is_writable);
    assert_eq!(card.source_class, ContactSourceClass::Personal);
    assert!(card.raw_provider_json.is_some());
}

/// The second captured contact was created by hand in the mailbox, so its unset string
/// fields come back as `""` while the API-created one returns `null`. Both mean absent.
#[tokio::test]
async fn empty_string_and_null_fields_are_both_treated_as_absent() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_SNAPSHOT))]);
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let hand_made = &objects[1];
    let name = hand_made.name.as_ref().expect("name");
    // `title: ""`, `middleName: ""`, `generation: ""` are all dropped; only the two
    // populated components survive.
    let components: Vec<&str> = name.components.iter().map(|c| c.value.as_str()).collect();
    assert_eq!(components, ["Test", "User"]);
    // `birthday: null`, `companyName: null`, `jobTitle: null` produce nothing.
    assert!(hand_made.anniversaries.is_empty());
    assert!(hand_made.organizations.is_empty());
    assert!(hand_made.titles.is_empty());
    // `businessAddress: {}` / `homeAddress: {}` / `otherAddress: {}` produce nothing.
    assert!(hand_made.addresses.is_empty());
    // `homePhones: []` with a `null` mobile leaves only the one business phone.
    assert_eq!(hand_made.phones.len(), 1);
}

/// Replaying the captured `deltaLink` after a `PATCH` yields a full object whose
/// `changeKey` has advanced — the version token a host compares.
#[tokio::test]
async fn captured_delta_carries_the_write_through_with_an_advanced_change_key() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_CHANGED))]);
    let cursor = engine_core::sync::SyncState::new(
        "https://graph.microsoft.com/v1.0/me/contacts/delta?$deltatoken=opaque-token-1",
    );
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_contacts(&account(), Some(&cursor))
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Delta { changed, .. } = sync.update else {
        panic!("expected delta");
    };
    assert_eq!(changed.len(), 1, "only the patched contact is returned");
    assert_eq!(
        changed[0].titles[&property("job-title")].value.name,
        "Director of Engineering"
    );
    assert_eq!(
        changed[0]
            .revisions
            .change_key
            .as_ref()
            .map(engine_core::version::ChangeKey::as_str),
        Some("change-key-2"),
        "the changeKey advanced from change-key-1 on the write"
    );
}

/// A deleted contact returns as `{ id, @removed:{reason}, @odata.type }` — no other
/// fields — and must become a removal key, never a card.
#[tokio::test]
async fn captured_tombstone_becomes_a_removal_not_a_card() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_REMOVED))]);
    let cursor = engine_core::sync::SyncState::new(
        "https://graph.microsoft.com/v1.0/me/contacts/delta?$deltatoken=opaque-token-2",
    );
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_contacts(&account(), Some(&cursor))
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Delta { changed, removed } = sync.update else {
        panic!("expected delta");
    };
    assert!(changed.is_empty());
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].as_str(), "contact-1");
    // The cursor advanced to the fixture's new deltaLink.
    assert!(sync.next_cursor.as_str().contains("opaque-token-3"));
}

/// The un-`$select`ed single-item `GET` returns a superset of the delta fields; the
/// same normalizer must handle it unchanged.
#[tokio::test]
async fn captured_detail_get_normalizes_like_a_delta_entry() {
    let client = fake_client(vec![("/contacts/contact-1", json(DETAIL))]);
    let card = GraphContactProvider::personal(client)
        .fetch_contact(
            &account(),
            &engine_core::ids::ContactId::try_from("contact-1").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        card.name.as_ref().and_then(|n| n.full.as_deref()),
        Some("Ada Byron Lovelace")
    );
    assert_eq!(card.emails.len(), 2);
    assert_eq!(card.uid.as_deref(), Some("urn:microsoft:graph:contact-1"));
}

/// `POST /me/contacts` echoes the created object; the id it carries is the receipt.
#[tokio::test]
async fn captured_create_echo_yields_the_new_contact_id() {
    let client = fake_client(vec![("/contacts", json(CREATED))]);
    let book = AddressBookId::try_from("graph-personal-root").unwrap();
    let draft = engine_core::contact::ContactDraft {
        address_book: book.clone(),
        card: engine_core::contact::ContactCard::new(
            engine_core::ids::ContactId::try_from("ignored").unwrap(),
            engine_core::membership::Memberships::of_one(book),
        ),
    };
    let receipt = GraphContactProvider::personal(client)
        .create_contact(&account(), &draft)
        .await
        .unwrap();
    assert_eq!(receipt.contact.as_str(), "contact-1");
}

/// The `PATCH` echo carries the advanced `changeKey` — pinned so a host that reads the
/// echo (instead of re-syncing) sees the same version the next delta will report.
#[tokio::test]
async fn captured_patch_echo_reports_the_advanced_change_key() {
    let patched = json(PATCHED);
    assert_eq!(patched["changeKey"], "change-key-2");
    assert_eq!(patched["jobTitle"], "Director of Engineering");
    // The etag is the changeKey in weak-validator form.
    assert_eq!(patched["@odata.etag"], "W/\"change-key-2\"");
}

/// Folder discovery walks the real parent chain: the synthetic root, the captured
/// folder, and its child — each keeping its `parentFolderId` as the owner link.
#[tokio::test]
async fn captured_folder_discovery_walks_the_real_parent_chain() {
    let client = fake_client(vec![
        (
            "/contactFolders/contact-folder-1/childFolders",
            json(CHILD_FOLDERS),
        ),
        (
            "/contactFolders/contact-folder-child-1/childFolders",
            json(r#"{"value":[]}"#),
        ),
        ("/contactFolders", json(FOLDERS)),
    ]);
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_address_books(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let ids: Vec<&str> = objects.iter().map(|b| b.id.as_str()).collect();
    assert!(ids.contains(&"graph-personal-root"), "synthetic root book");
    assert!(ids.contains(&"contact-folder-1"));
    assert!(ids.contains(&"contact-folder-child-1"));
    // The child's owner is the captured parent folder id, so the tree is reconstructable.
    let child = objects
        .iter()
        .find(|b| b.id.as_str() == "contact-folder-child-1")
        .expect("child folder");
    assert_eq!(child.owner.as_deref(), Some("contact-folder-1"));
    assert!(child.is_writable);
}

/// `categories` is part of `CONTACT_SELECT` and the write path maps
/// `ContactField::Keywords` onto it, so the read path must map it back — otherwise a
/// keyword survives a create and vanishes on the next sync.
#[tokio::test]
async fn captured_categories_become_card_keywords() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_SNAPSHOT))]);
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let keywords: Vec<&str> = objects[0].keywords.iter().map(String::as_str).collect();
    assert_eq!(keywords, ["Engineering", "Fixture"]);
    // The hand-created contact has `categories: []` and gains no keywords.
    assert!(objects[1].keywords.is_empty());
}

/// Graph anchors a birthday near local noon and returns a full timestamp; the neutral
/// `Anniversary.date` is JSContact *date* text (what the Google adapter emits), so the
/// time component must not leak into it.
#[tokio::test]
async fn captured_birthday_normalizes_to_a_jscontact_date() {
    let client = fake_client(vec![("/contacts/delta", json(DELTA_SNAPSHOT))]);
    let ContactSourceSync::Available { sync, .. } = GraphContactProvider::personal(client)
        .sync_contacts(&account(), None)
        .await
        .unwrap()
    else {
        panic!("expected available");
    };
    let SyncUpdate::Snapshot { objects, .. } = sync.update else {
        panic!("expected snapshot");
    };
    let birthday = &objects[0].anniversaries[&property("birthday")].value;
    // The captured value is "1815-12-10T11:59:00Z"; only the date is a JSContact date.
    assert_eq!(birthday.date, "1815-12-10");
    assert_eq!(birthday.kind.as_deref(), Some("birth"));
}

/// A personal Microsoft account refuses the tenant contact sources by *shape*, not by
/// permission — `400` for org contacts, `401` for directory users, neither a `403`. Both
/// must degrade to `Unavailable` so one unavailable source never fails the account's
/// whole contact sync (and never asks the host to re-authenticate a working token).
#[tokio::test]
async fn captured_tenant_refusals_degrade_instead_of_failing_the_sync() {
    for (source, status, body) in [
        ("org", 400u16, MSA_UNSUPPORTED),
        ("directory", 401u16, DIRECTORY_UNAUTHORIZED),
    ] {
        let routes = vec![("/delta", Err((status, json(body))))];
        let provider = if source == "org" {
            GraphContactProvider::organizational(fake_client_fallible(routes))
        } else {
            GraphContactProvider::directory(fake_client_fallible(routes))
        };
        let result = provider.sync_contacts(&account(), None).await.unwrap();
        assert!(
            matches!(result, ContactSourceSync::Unavailable(_)),
            "{source} source should degrade on a real {status}, got {result:?}"
        );
    }
}

/// The personal source is *not* optional: a failure there is a real error, never a
/// silent `Unavailable` that would look like an empty address book.
#[tokio::test]
async fn personal_source_never_degrades_to_unavailable() {
    let client = fake_client_fallible(vec![("/delta", Err((400, json(MSA_UNSUPPORTED))))]);
    let error = GraphContactProvider::personal(client)
        .sync_contacts(&account(), None)
        .await;
    assert!(error.is_err(), "personal contact failures must surface");
}
