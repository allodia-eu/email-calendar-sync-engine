//! A message's attachment flag as read from its own source outlasts the provider's guess.

use engine_store::MessageSourceCache;

use super::{account, ingest_mail, mail_scope, message, parse_mail, pk, store};

async fn flagged(store: &store_sqlite::SqliteStore<engine_store::ManualClock>) -> Vec<String> {
    let results = store
        .search_mail(&[mail_scope()], &parse_mail("has_attachment:true"), 10)
        .await
        .unwrap();
    let mut keys: Vec<String> = results
        .keys()
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect();
    keys.sort();
    keys
}

#[tokio::test]
async fn the_answer_read_from_the_source_survives_the_next_sync_of_the_message() {
    let store = store();
    let scope = mail_scope();
    // The provider guessed wrong both ways: Gmail's metadata read knows no parts (false for a
    // PDF), and a server's own rule may count what the opened message does not list.
    let pdf_guessed_bare = message("pdf", "report", "a@example.com", "inbox");
    let mut photo_guessed_flagged = message("inline", "photo", "a@example.com", "inbox");
    photo_guessed_flagged.has_attachment = true;
    ingest_mail(
        &store,
        &scope,
        vec![pdf_guessed_bare.clone(), photo_guessed_flagged.clone()],
    )
    .await;
    assert_eq!(flagged(&store).await, ["inline"]);

    store
        .set_mail_has_attachment(&account(), &pk("pdf"), true)
        .await
        .unwrap();
    store
        .set_mail_has_attachment(&account(), &pk("inline"), false)
        .await
        .unwrap();
    assert_eq!(flagged(&store).await, ["pdf"]);

    // A later sync sends the same guesses again, as a re-snapshot or a server re-fetch does.
    ingest_mail(
        &store,
        &scope,
        vec![pdf_guessed_bare, photo_guessed_flagged],
    )
    .await;
    assert_eq!(
        flagged(&store).await,
        ["pdf"],
        "the provider's guess must not undo what the source said"
    );
}

#[tokio::test]
async fn a_message_settled_before_its_row_arrives_lands_settled() {
    let store = store();
    // A body opened from a search hit can be read before the sync that files it lands.
    store
        .set_mail_has_attachment(&account(), &pk("early"), true)
        .await
        .unwrap();
    ingest_mail(
        &store,
        &mail_scope(),
        vec![message("early", "x", "a@example.com", "inbox")],
    )
    .await;
    assert_eq!(flagged(&store).await, ["early"]);
}
