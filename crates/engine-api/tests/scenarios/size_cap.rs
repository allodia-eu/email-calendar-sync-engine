//! The per-account message-size cap, end to end through the facade: what lowering it frees, and
//! what raising it fetches back.
//!
//! Split from `cases.rs` to keep both inside the 500-line limit; the shared fixtures and the
//! blob helpers stay there.

use engine_api::Engine;

use super::{
    cases::{age_blobs, no_observer, source_blobs, warm},
    *,
};

#[tokio::test]
async fn lowering_a_size_cap_frees_the_bytes_and_re_queues_the_body() {
    // The whole round trip a message-size cap needs: drop the heaviest sources, get the disk
    // back, keep the mail readable — and come back as work when the cap goes up again.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();
    let provider = SimProvider::new(messages(2), 2);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;
    let rows = engine.mail_missing_body(&[account()], 50).await.unwrap();
    warm(&engine, &provider, &rows).await;
    assert_eq!(source_blobs(&db), 2);
    assert!(
        engine
            .mail_missing_body(&[account()], 50)
            .await
            .unwrap()
            .is_empty(),
        "both are warm to begin with"
    );

    // A cap nothing exceeds is a no-op, not an accidental wipe.
    let untouched = engine
        .drop_message_sources_over(&account(), 10 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(untouched.sources_removed, 0);
    assert_eq!(untouched.octets_freed, 0);
    assert_eq!(source_blobs(&db), 2);

    // Lower it under every message and the cached sources go.
    let dropped = engine
        .drop_message_sources_over(&account(), 0)
        .await
        .unwrap();
    assert_eq!(dropped.sources_removed, 2);
    assert!(dropped.octets_freed > 0, "freed no bytes");

    // The mail itself is untouched: still listed, and still readable offline from its text.
    assert_eq!(engine.messages(&account()).await.unwrap().len(), 2);

    // And it is work again — the half that makes raising the cap fetch anything back.
    assert_eq!(
        engine
            .mail_missing_body(&[account()], 50)
            .await
            .unwrap()
            .len(),
        2,
        "a dropped source must re-queue the body, or a raised cap downloads nothing",
    );

    // Only now do the files actually go.
    age_blobs(&db);
    let swept = engine.sweep_unreferenced_blobs().await.unwrap();
    assert_eq!(swept.blobs_removed, 2);
    assert_eq!(source_blobs(&db), 0);
}

#[tokio::test]
async fn raising_the_cap_puts_the_bytes_back() {
    // The other half of the round trip. A body read is text-first, so once the text survives a
    // drop it reports success and fetches nothing — `ensure_message_source` is what a warm adds
    // to make a raised cap mean something.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("engine.sqlite");
    let engine = Engine::open(&db).unwrap();
    let provider = SimProvider::new(messages(1), 1);
    engine
        .sync_mail(
            core::slice::from_ref(&provider),
            &account(),
            responsive(),
            &no_observer(),
        )
        .await;
    let rows = engine.mail_missing_body(&[account()], 50).await.unwrap();
    warm(&engine, &provider, &rows).await;
    engine
        .drop_message_sources_over(&account(), 0)
        .await
        .unwrap();
    age_blobs(&db);
    engine.sweep_unreferenced_blobs().await.unwrap();
    assert_eq!(source_blobs(&db), 0, "the bytes are gone");

    // A body read alone cannot notice: the text is still cached.
    let message = engine.messages(&account()).await.unwrap();
    engine
        .message_body(&provider, &account(), &message[0])
        .await
        .unwrap();
    assert_eq!(source_blobs(&db), 0, "text-first, so it fetched nothing");

    engine
        .ensure_message_source(&provider, &account(), &message[0])
        .await
        .unwrap();
    assert_eq!(source_blobs(&db), 1, "and this is what fetches them back");
    assert!(
        engine
            .mail_missing_body(&[account()], 50)
            .await
            .unwrap()
            .is_empty(),
        "with both halves cached it is off the work list again",
    );
}
