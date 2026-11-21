//! Recipient-history cases shared by every backend, beyond replay and suppression.

use engine_core::{
    ids::{AccountId, MailboxId, MessageId, ProviderKey},
    mail::Message,
    membership::Memberships,
    people::CanonicalEmail,
    recipient::RecipientObservation,
    sync::{JmapDataType, SyncScope, SyncState, SyncUpdate},
};

use super::lease_request;
use crate::{ApplyBatch, ContactStore, DerivedWrite, Store, lease::SyncLease};

fn account() -> AccountId {
    AccountId::try_from("acct-1").unwrap()
}

/// Applies one sync of the Sent message `m1` that observed `observation`.
async fn observe<S: Store>(store: &S, lease: &SyncLease, observation: &RecipientObservation) {
    let message = Message::new(
        MessageId::try_from("m1").unwrap(),
        Memberships::of_one(MailboxId::try_from("sent").unwrap()),
    );
    let update = SyncUpdate::delta(vec![message], Vec::<ProviderKey>::new());
    let state = SyncState::new("mail-1");
    let derived = DerivedWrite::empty();
    let batch = ApplyBatch::new(&update, &derived, &[], &state)
        .with_recipient_observations(core::slice::from_ref(observation));
    store.apply_sync_update(lease, batch).await.unwrap();
}

/// A replay of an observation fills in the sent instant the stored one lacks, and changes nothing
/// else: the instant is the source message's, which never changes, so a stored observation without
/// one was recorded from a read that could not see it. A re-sync that reads the message again is
/// how its recipients come to say when they were last written to.
pub(super) async fn a_replay_fills_a_missing_sent_instant<S>(store: &S)
where
    S: Store + ContactStore,
{
    let scope = SyncScope::JmapType {
        account: account(),
        data_type: JmapDataType::Email,
    };
    let claim = store
        .claim_sync_scope(account(), &scope, lease_request("recipients", 60))
        .await
        .unwrap();
    let lease = &claim.lease;
    let unseen = RecipientObservation {
        account: account(),
        source_message: MessageId::try_from("m1").unwrap(),
        email: CanonicalEmail::parse("friend@example.test").unwrap(),
        name: Some("Friend".into()),
        sent_at: None,
    };

    observe(store, lease, &unseen).await;
    assert_eq!(
        store.recipient_interactions(None).await.unwrap()[0].last_sent,
        None
    );

    let seen = RecipientObservation {
        name: Some("Another name".into()),
        sent_at: Some("2026-10-29T09:00:00Z".parse().unwrap()),
        ..unseen.clone()
    };
    observe(store, lease, &seen).await;
    let interactions = store.recipient_interactions(None).await.unwrap();
    assert_eq!(interactions.len(), 1);
    assert_eq!(interactions[0].sent_count, 1);
    assert_eq!(interactions[0].last_sent, seen.sent_at);
    assert_eq!(interactions[0].name.as_deref(), Some("Friend"));

    // A known instant is never replaced, nor cleared by a read that could not see it.
    let later = RecipientObservation {
        sent_at: Some("2026-11-20T20:15:44Z".parse().unwrap()),
        ..unseen.clone()
    };
    observe(store, lease, &later).await;
    observe(store, lease, &unseen).await;
    assert_eq!(
        store.recipient_interactions(None).await.unwrap()[0].last_sent,
        seen.sent_at
    );

    // A backfill replays the same way.
    let other = RecipientObservation {
        source_message: MessageId::try_from("m2").unwrap(),
        ..unseen.clone()
    };
    observe(store, lease, &other).await;
    let backfilled = RecipientObservation {
        sent_at: later.sent_at,
        ..other
    };
    assert!(
        store
            .apply_recipient_backfill(account(), 1, &[backfilled])
            .await
            .unwrap()
    );
    let interactions = store.recipient_interactions(None).await.unwrap();
    assert_eq!(interactions[0].sent_count, 2);
    assert_eq!(interactions[0].last_sent, later.sent_at);
}
