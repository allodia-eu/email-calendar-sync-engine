//! End-to-end body/full-text mail search: the on-demand-fetched body text is
//! searchable (including by prefix), survives a re-snapshot that re-derives the
//! message, and ignores tombstoned or other-account body rows.

use engine_core::mail::MessageBody;
use engine_store::MessageBodyStore;

use super::*;

#[tokio::test]
async fn search_matches_a_word_only_in_a_fetched_body_and_survives_re_snapshot() {
    let store = store();
    let scope = mail_scope();
    // The subject and addresses deliberately omit the word "quarterly".
    ingest_mail(
        &store,
        &scope,
        vec![message("m1", "Status update", "a@example.com", "inbox")],
    )
    .await;

    // Before the body is fetched, the word is not searchable.
    let before = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("quarterly"), 10)
        .await
        .unwrap();
    assert!(before.keys().is_empty(), "body word not yet indexed");

    // The on-demand body fetch caches the extracted text (lease-free).
    let body = MessageBody::new(Some("Here are the quarterly numbers.".to_owned()), None);
    store
        .put_message_body(&account(), &ProviderKey::new("m1").unwrap(), &body)
        .await
        .unwrap();

    let after = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("quarter"), 10)
        .await
        .unwrap();
    assert_eq!(
        after.keys().len(),
        1,
        "body text is searchable (prefix too)"
    );
    assert_eq!(after.keys()[0].as_str(), "m1");

    // A full re-snapshot (the IMAP reconcile path) re-projects the scope-derived
    // rows but must NOT wipe the lease-free body index.
    let claim = store
        .claim_sync_scope(account(), &scope, lease())
        .await
        .unwrap();
    let msg = message("m1", "Status update", "a@example.com", "inbox");
    let mut derived = DerivedWrite::empty();
    derived.removed.push(ProviderKey::new("m1").unwrap());
    derived.push_mail(project_message(&msg));
    let snapshot = SyncUpdate::snapshot(
        vec![msg],
        std::collections::BTreeSet::from([ProviderKey::new("m1").unwrap()]),
    );
    store
        .apply_sync_update(
            &claim.lease,
            ApplyBatch::new(&snapshot, &derived, &[], &SyncState::new("c2")),
        )
        .await
        .unwrap();
    store.release_sync_scope(claim.lease).await.unwrap();

    let survived = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("quarterly"), 10)
        .await
        .unwrap();
    assert_eq!(
        survived.keys().len(),
        1,
        "body search survives a re-snapshot that re-derived the message"
    );
}

#[tokio::test]
async fn body_search_ignores_a_tombstoned_message_and_other_accounts() {
    let store = store();
    let scope = mail_scope();
    ingest_mail(
        &store,
        &scope,
        vec![message("m1", "Status update", "a@example.com", "inbox")],
    )
    .await;
    let body = MessageBody::new(Some("the quarterly numbers".to_owned()), None);
    store
        .put_message_body(&account(), &ProviderKey::new("m1").unwrap(), &body)
        .await
        .unwrap();

    // A body row for another account's key (same string) must not surface in this
    // account's search.
    let other = engine_core::ids::AccountId::try_from("acct-2").unwrap();
    store
        .put_message_body(&other, &ProviderKey::new("m1").unwrap(), &body)
        .await
        .unwrap();
    // And one for a key with no live mail_index row (tombstoned/never-synced).
    store
        .put_message_body(&account(), &ProviderKey::new("ghost").unwrap(), &body)
        .await
        .unwrap();

    let hits = store
        .search_mail(std::slice::from_ref(&scope), &parse_mail("quarterly"), 10)
        .await
        .unwrap();
    assert_eq!(
        hits.keys().len(),
        1,
        "only the live, in-account, in-scope key"
    );
    assert_eq!(hits.keys()[0].as_str(), "m1");
}
