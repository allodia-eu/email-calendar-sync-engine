//! Keeping a draft on the server through the facade: the key a host must hold on to, and
//! what a save made with no network leaves behind for a later drain.

use engine_api::{ApiError, DrainOutcome, Engine, PendingOpKind, PendingOpState};

use super::*;

/// A healthy save settles, and the **key moves on the re-save**. A host that kept the key
/// it passed in rather than the one it got back would store a second copy beside the
/// first on every autosave, which is the failure this return value exists to prevent.
#[tokio::test]
async fn a_save_returns_the_key_to_hold_and_the_re_save_moves_it() {
    let engine = Engine::open_in_memory().unwrap();
    let provider = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let draft = draft("compose-1@test.local", "Quarterly report");

    let first = engine
        .put_draft(&provider, &account(), &draft, None)
        .await
        .unwrap();
    assert_eq!(first.key, ProviderKey::new("draft-1").unwrap());
    assert_eq!(
        engine.pending_op_state(first.op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );

    // The autosave 30 seconds later names the copy it supersedes.
    let second = engine
        .put_draft(&provider, &account(), &draft, Some(&first.key))
        .await
        .unwrap();
    assert_eq!(second.key, ProviderKey::new("draft-2").unwrap());
    assert_ne!(
        second.key, first.key,
        "the key moved; the caller must keep it"
    );

    // And the draft can be discarded by that key.
    let op = engine
        .delete_draft(&provider, &account(), &second.key)
        .await
        .unwrap();
    assert_eq!(
        engine.pending_op_state(op).await.unwrap(),
        Some(PendingOpState::Succeeded)
    );
}

/// The case the outbox exists for, seen from the facade a host actually calls: a save
/// with no network is **kept**, and the drain that follows both stores it and reports the
/// key it was stored under.
///
/// That key has no other route back to the host. The op settled with nobody waiting on
/// it, and `outbox()` lists only what is still outstanding, so a host that could not read
/// the key off the drain report would save again under no key and store a second copy.
#[tokio::test]
async fn a_save_with_no_network_is_kept_and_the_drain_reports_its_key() {
    let engine = Engine::open_in_memory().unwrap();
    let offline = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
        unfiled: false,
    };
    let err = engine
        .put_draft(
            &offline,
            &account(),
            &draft("compose-offline@test.local", "Quarterly report"),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Sync(_)), "got {err:?}");

    // Kept, and legible as a draft save rather than as a send.
    let queued = engine.outbox(&account()).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, Some(PendingOpKind::MailDraftPut));

    // "Try now" clears the backoff, and this time the network is there.
    engine
        .retry_pending_op_now(&account(), queued[0].id)
        .await
        .unwrap();
    let healthy = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let report = engine.drain_outbox(&healthy, &account()).await.unwrap();

    assert_eq!(report.delivered(), 1);
    assert_eq!(report.attempted[0].outcome, DrainOutcome::Succeeded);
    assert_eq!(
        report.attempted[0].provider_key,
        Some(ProviderKey::new("draft-1").unwrap()),
        "the host learns what the queued draft was stored under"
    );
    assert!(engine.outbox(&account()).await.unwrap().is_empty());
}

/// A discard made with no network is kept too, so a draft the user deleted does not
/// reappear in Drafts the next time the account syncs.
#[tokio::test]
async fn a_discard_with_no_network_is_kept_and_drains() {
    let engine = Engine::open_in_memory().unwrap();
    let offline = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: true,
        unfiled: false,
    };
    let key = ProviderKey::new("draft-9").unwrap();
    engine
        .delete_draft(&offline, &account(), &key)
        .await
        .expect_err("the discard had no network");

    let queued = engine.outbox(&account()).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].kind, Some(PendingOpKind::MailDraftDelete));

    engine
        .retry_pending_op_now(&account(), queued[0].id)
        .await
        .unwrap();
    let healthy = SubmittingProvider {
        inner: FakeProvider::new(),
        fail: false,
        unfiled: false,
    };
    let report = engine.drain_outbox(&healthy, &account()).await.unwrap();
    assert_eq!(report.delivered(), 1);
    assert!(engine.outbox(&account()).await.unwrap().is_empty());
}
